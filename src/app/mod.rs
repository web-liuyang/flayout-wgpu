//! 顶层应用编排层。
//!
//! 这里是整个 viewer 的“总控中心”，负责把这些子系统接在一起：
//! - `winit` 窗口与事件循环
//! - `egui` 输入与 UI 绘制
//! - `io` 文件加载
//! - `scene` 当前场景/视图状态
//! - `camera` 平移缩放
//! - `renderer` GPU 绘制
//!
//! 这个模块的价值在于：
//! 它让“状态流”和“数据流”都能在一个地方看明白。

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use egui_winit::State as EguiWinitState;
use glam::Vec2;
use rfd::FileDialog;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

use crate::{
    camera::Camera2D,
    config::{DEFAULT_LAYOUT_PATH, INITIAL_HEIGHT, INITIAL_WIDTH, WINDOW_TITLE},
    io::{load_layout_bundle, load_layout_hierarchy_bundle},
    layout::{LayoutBundle, LayoutCellId, LayoutViewBuildOptions, build_layout_view_scene},
    perf::{FrameStats, RenderStatsHistory},
    persistence::{
        PersistedCamera, PersistedClosedShapeDrawMode, PersistedHatchStylePreset,
        PersistedLayerDrawMode, PersistedLayerHatchStyle, PersistedLayerId, ViewerConfig,
        filter_hidden_layers_for_scene, filter_layer_draw_modes_for_scene,
        filter_layer_hatch_styles_for_scene, load_viewer_config, resolve_saved_view_index,
        save_viewer_config,
    },
    renderer::{
        DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD, DEFAULT_LAYER_BYPASS_WORK_THRESHOLD,
        DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD, DEFAULT_TILE_CACHE_CAPACITY, RenderDebugStats,
        Renderer,
        geometry::{
            ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_WIDTH,
            DEFAULT_TILE_GRID_DIVISIONS, HatchParams, HatchStylePreset,
            camera_visible_world_bounds,
        },
    },
    scene::{Bounds, LayerId, Scene, SceneBundle, SceneView},
    ui::draw_ui,
};

fn filter_hidden_layers_for_layer_ids(
    config: &ViewerConfig,
    layers: &[LayerId],
) -> BTreeSet<LayerId> {
    let existing: BTreeSet<_> = layers.iter().copied().collect();
    config
        .hidden_layers
        .iter()
        .map(|layer| layer.to_runtime())
        .filter(|layer| existing.contains(layer))
        .collect()
}

fn filter_layer_draw_modes_for_layer_ids(
    config: &ViewerConfig,
    layers: &[LayerId],
) -> BTreeMap<LayerId, ClosedShapeDrawMode> {
    let existing: BTreeSet<_> = layers.iter().copied().collect();
    config
        .layer_draw_modes
        .iter()
        .filter_map(|entry| {
            let layer = entry.layer.to_runtime();
            existing
                .contains(&layer)
                .then_some((layer, entry.mode.to_runtime()))
        })
        .collect()
}

fn filter_layer_hatch_styles_for_layer_ids(
    config: &ViewerConfig,
    layers: &[LayerId],
) -> BTreeMap<LayerId, HatchStylePreset> {
    let existing: BTreeSet<_> = layers.iter().copied().collect();
    config
        .layer_hatch_styles
        .iter()
        .filter_map(|entry| {
            let layer = entry.layer.to_runtime();
            existing
                .contains(&layer)
                .then_some((layer, entry.style.to_runtime()))
        })
        .collect()
}

const INITIAL_LAYOUT_WORKSET_SHAPE_BUDGET: u64 = 3_000_000;
const INITIAL_LAYOUT_WORKSET_POINT_BUDGET: u64 = 20_000_000;
/// 当层级范围预估成本超过这个阈值时，app 不再先构完整 workset，
/// 而是切到 renderer 侧 direct hierarchy tile 路径。
///
/// 这两个阈值和上面的 initial workset budget 看起来接近，是有意的：
/// - 小场景：直接构临时 Scene，逻辑简单，交互更直接
/// - 大场景：避免 app 层先常驻一份巨大 flat Scene
const DIRECT_LAYOUT_TILE_SOURCE_SHAPE_THRESHOLD: u64 = 3_000_000;
const DIRECT_LAYOUT_TILE_SOURCE_POINT_THRESHOLD: u64 = 20_000_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LayoutHierarchyLevelCost {
    shape_count: u64,
    point_count: u64,
}

/// 当前加载状态，用于左侧 UI 展示。
#[derive(Debug, Clone)]
pub enum LoadState {
    Idle,
    Loaded,
    Failed(String),
}

impl LoadState {
    /// 将内部状态转成人类可读文本。
    pub fn summary(&self) -> String {
        match self {
            Self::Idle => "Waiting for layout file".to_string(),
            Self::Loaded => "Layout loaded".to_string(),
            Self::Failed(message) => format!("Load failed: {message}"),
        }
    }
}

/// 一个很小的纯函数，方便测试 pan 叠加行为。
pub fn apply_pan_delta(current: Vec2, delta: Vec2) -> Vec2 {
    current + delta
}

/// 只有在 UI 明确还想 repaint，或者 renderer 还有渐进式待补工作时，
/// 才继续主动请求下一帧重绘。
///
/// 之前 viewer 在 `about_to_wait` 里无条件 `request_redraw()`，
/// 这会让重图层场景在"其实没有新的构建结果"时也持续整帧重画。
/// 这里把策略收紧以后，交互仍然流畅，但空转重绘会明显减少。
pub fn should_request_continuous_redraw(pending_entries: usize, egui_wants_repaint: bool) -> bool {
    egui_wants_repaint || pending_entries > 0
}

/// 窗口事件是否应该立刻唤醒一帧重绘。
///
/// 关键点是区分两类情况：
/// - `about_to_wait`：负责"持续"重绘（动画/渐进补全）
/// - `window_event`：负责"交互唤醒"（鼠标、滚轮、键盘等输入）
///
/// 上一轮如果只保留前者，就会让普通输入没有机会触发下一帧，
/// 表现出来就像窗口直接卡住。
pub fn should_request_redraw_after_window_event(
    egui_wants_repaint: bool,
    event: &WindowEvent,
) -> bool {
    egui_wants_repaint
        || matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::CursorEntered { .. }
                | WindowEvent::CursorLeft { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::KeyboardInput { .. }
                | WindowEvent::TouchpadPressure { .. }
                | WindowEvent::Touch { .. }
                | WindowEvent::ModifiersChanged(..)
                | WindowEvent::Ime(_)
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::PanGesture { .. }
                | WindowEvent::DoubleTapGesture { .. }
                | WindowEvent::RotationGesture { .. }
        )
}

const INTERACTION_RENDER_DEGRADE_HOLD: Duration = Duration::from_millis(120);
/// 初始化层级范围时，判定“小版图”的 shape 数阈值。
const SMALL_SCENE_SHAPE_THRESHOLD: usize = 50_000;
/// 初始化层级范围时，判定“小版图”的点数阈值。
const SMALL_SCENE_POINT_THRESHOLD: usize = 300_000;
/// 分层 workset 的子树 screen-space LOD 阈值。
///
/// 当一个子树在屏幕上的最大包围尺寸已经小于这个阈值时，
/// 继续向下展开往往只会显著放大内存和索引成本，但肉眼几乎看不出差别。
const HIERARCHICAL_SUBTREE_MIN_SCREEN_EXTENT: f32 = 2.0;
/// 只有足够深的 hierarchy 子树才允许做 screen-space 折叠。
///
/// 这样可以保住浅层阵列/骨架的整体形态，避免在全局 fit 时过早被压成 marker。
const HIERARCHICAL_SUBTREE_MIN_COLLAPSE_LEVEL: u32 = 4;
/// workset 视口裁剪会比当前可见视口多预取一圈，减少连续交互中的频繁重建。
const LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO: f32 = 0.25;
/// 当前 zoom 相对上次 workset 构建 zoom 的允许漂移比例。
///
/// 在这个阈值内，小幅滚轮缩放直接复用当前 workset；
/// 只有 zoom 变化足够明显时，才重新按新的 screen-space LOD 重建。
const LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD: f32 = 1.2;

/// 最近发生过平移/缩放时，是否应临时把闭合图形降级成更轻的显示模式。
///
/// 这里故意只看一个很短的时间窗，而不是持久切模式：
/// - 交互中：优先保证手感
/// - 停下来：尽快恢复完整 hatch 结果
pub fn should_degrade_interaction_render(
    elapsed_since_interaction: Option<Duration>,
    hold: Duration,
) -> bool {
    elapsed_since_interaction
        .map(|elapsed| elapsed <= hold)
        .unwrap_or(false)
}

fn expand_layout_workset_visible_bounds(bounds: Bounds, margin_ratio: f32) -> Bounds {
    Bounds::new(
        bounds.min_x - bounds.width().max(0.0) * margin_ratio,
        bounds.min_y - bounds.height().max(0.0) * margin_ratio,
        bounds.max_x + bounds.width().max(0.0) * margin_ratio,
        bounds.max_y + bounds.height().max(0.0) * margin_ratio,
    )
}

fn bounds_contains_bounds(container: Bounds, candidate: Bounds) -> bool {
    container.min_x <= candidate.min_x
        && container.min_y <= candidate.min_y
        && container.max_x >= candidate.max_x
        && container.max_y >= candidate.max_y
}

fn should_rebuild_layout_workset_for_camera(
    current_visible_world_bounds: Option<Bounds>,
    current_zoom: f32,
    covered_visible_world_bounds: Option<Bounds>,
    covered_zoom: Option<f32>,
) -> bool {
    // 这层判定本质上是在回答一个问题：
    // “当前 camera 变化，是否已经足以让旧 workset 的视口裁剪 / screen-space LOD 失真？”
    //
    // 如果答案还是否，我们宁愿先复用旧 workset，
    // 把交互流畅度让给 pan/zoom，而不是每一下都同步重建。
    let Some(current_visible_world_bounds) = current_visible_world_bounds else {
        return false;
    };
    let (Some(covered_visible_world_bounds), Some(covered_zoom)) =
        (covered_visible_world_bounds, covered_zoom)
    else {
        return true;
    };

    if !bounds_contains_bounds(covered_visible_world_bounds, current_visible_world_bounds) {
        return true;
    }

    let zoom_ratio = if covered_zoom.abs() <= f32::EPSILON {
        f32::INFINITY
    } else {
        current_zoom / covered_zoom
    };
    zoom_ratio < (1.0 / LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD)
        || zoom_ratio > LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD
}

/// 按 scene 的排序后 layer 索引给出默认 hatch preset。
///
/// 这里故意不看“当前缺了几个”，而是看“这个 layer 在完整有序列表里的绝对位置”：
/// 这样当用户只给中间某一层手动指定了样式时，其余层的默认结果仍然稳定，
/// 不会因为某个显式覆盖的存在与否而把后续层整体错位。
fn alternating_default_hatch_style(layer_index: usize) -> HatchStylePreset {
    if layer_index % 2 == 0 {
        HatchStylePreset::LeftDiagonal
    } else {
        HatchStylePreset::RightDiagonal
    }
}

/// 给当前 scene 中还没有显式 hatch preset 的 layer 补上默认值。
pub fn fill_missing_layer_hatch_styles(
    scene: &Scene,
    layer_hatch_styles: &mut BTreeMap<LayerId, HatchStylePreset>,
) {
    for (index, layer) in scene.layer_ids().into_iter().enumerate() {
        layer_hatch_styles
            .entry(layer)
            .or_insert_with(|| alternating_default_hatch_style(index));
    }
}

pub fn fill_missing_layer_hatch_styles_for_layers(
    layers: &[LayerId],
    layer_hatch_styles: &mut BTreeMap<LayerId, HatchStylePreset>,
) {
    for (index, layer) in layers.iter().copied().enumerate() {
        layer_hatch_styles
            .entry(layer)
            .or_insert_with(|| alternating_default_hatch_style(index));
    }
}

/// 用一个保守的启发式判断“当前 scene 是否算小版图”。
///
/// 这里故意同时看：
/// - shape 数量
/// - 总点数
///
/// 因为只看 shape 数会漏掉“shape 不多但每个都非常复杂”的情况；
/// 只看点数又不够直观。两者一起判断，能得到一个足够稳定的初始化策略。
pub fn is_small_scene_for_initial_hierarchy_range(scene: &Scene) -> bool {
    scene.stats().shape_count <= SMALL_SCENE_SHAPE_THRESHOLD
        && scene.total_point_count() <= SMALL_SCENE_POINT_THRESHOLD
}

/// 根据 scene 复杂度给出初始层级显示范围。
///
/// 规则：
/// - 小版图：默认显示全部层级
/// - 大版图：默认先显示前一半层级，帮助性能和调试
pub fn recommended_initial_hierarchy_level_range(scene: &Scene) -> (u32, u32) {
    let max_level = scene.max_hierarchy_level();
    if is_small_scene_for_initial_hierarchy_range(scene) {
        return (0, max_level);
    }

    let total_levels = max_level as usize + 1;
    let visible_level_count = total_levels.div_ceil(2).max(1);
    (0, (visible_level_count - 1) as u32)
}

/// Viewer 应用状态。
pub struct ViewerApp {
    /// winit 创建的主窗口句柄。
    window: Option<Arc<Window>>,
    /// 主窗口 id，用来过滤窗口事件。
    window_id: Option<WindowId>,
    /// GPU renderer；窗口创建前保持为空。
    renderer: Option<Renderer>,
    /// egui 全局上下文。
    egui_ctx: egui::Context,
    /// egui 和 winit 之间的事件桥接状态。
    egui_state: Option<EguiWinitState>,
    /// 当前文件可切换的 scene 视图集合。
    scene_bundle: SceneBundle,
    /// GDS 新内存架构下的分层数据源。
    ///
    /// - `Some`：当前文件走分层 source + workset builder 路径
    /// - `None`：当前文件仍走旧的扁平 `SceneBundle` 路径
    layout_bundle: Option<LayoutBundle>,
    /// 当前真正送入 renderer 的场景。
    ///
    /// 在旧路径下，它通常等于选中 scene 经过 level range 过滤后的结果；
    /// 在新路径下，它是从 `LayoutBundle` 按需构建出来的临时 workset。
    scene: Arc<Scene>,
    /// 当前场景真实可见层集合的轻量摘要。
    ///
    /// direct hierarchy tile 路径下，app 可能不会长期持有完整 Scene，
    /// 这时 UI 仍然需要靠它来驱动 layer 列表和持久化过滤。
    scene_layer_ids: Vec<LayerId>,
    /// 当前场景的 bounds 提示。
    ///
    /// 对 direct hierarchy 路径尤其重要，因为 fit / UI 不能再假设一定有完整 Scene 可读。
    scene_bounds_hint: Option<Bounds>,
    /// 是否切到了 renderer 侧 direct hierarchy tile 渲染。
    ///
    /// `false`：app 先构一个临时 Scene/workset，再交给 renderer。
    /// `true`：app 只维护 hierarchy source 摘要，renderer 按 tile 向 hierarchy 要几何。
    hierarchy_tile_render_active: bool,
    /// 当前交互相机。
    camera: Camera2D,
    /// 文件加载状态。
    load_state: LoadState,
    /// 当前打开或待打开的版图路径。
    layout_path: String,
    /// 是否已经针对当前 scene 初始化过相机。
    initialized_camera: bool,
    /// 中央画布的逻辑尺寸。
    canvas_size: Vec2,
    /// 当前被隐藏的 layer 集合。
    hidden_layers: BTreeSet<LayerId>,
    /// per-layer 闭合图元显示模式覆盖。
    layer_draw_modes: BTreeMap<LayerId, ClosedShapeDrawMode>,
    /// per-layer hatch 风格覆盖。
    layer_hatch_styles: BTreeMap<LayerId, HatchStylePreset>,
    /// 滑动窗口帧时间统计。
    frame_stats: FrameStats,
    /// 上一帧绘制完成时刻。
    last_frame_at: Option<Instant>,
    /// 当前帧 renderer 调试统计。
    render_debug_stats: RenderDebugStats,
    /// 多帧 renderer 调试趋势历史。
    render_stats_history: RenderStatsHistory,
    /// 启动后待恢复的一份持久化配置快照。
    pending_restore_config: Option<ViewerConfig>,
    /// 当前 tile grid 粒度设置。
    tile_grid_divisions: u32,
    /// 当前全局闭合图元画法。
    draw_mode: ClosedShapeDrawMode,
    /// 当前全局 hatch 参数。
    hatch_params: HatchParams,
    /// 当前显示层级范围的下界。
    min_hierarchy_level: u32,
    /// 当前显示层级范围的上界。
    max_hierarchy_level: u32,
    /// 当前 root cell 实际拥有的最大层级深度。
    ///
    /// 对旧扁平路径，它来自当前选中 scene 的 `max_hierarchy_level()`；
    /// 对新分层路径，它来自 root cell 子树的递归深度，而不是当前 workset。
    available_max_hierarchy_level: u32,
    /// tile cache 条目容量上限。
    tile_cache_capacity: usize,
    /// 全局 progressive bypass 阈值。
    progressive_bypass_threshold: usize,
    /// per-layer entry bypass 阈值。
    layer_bypass_entry_threshold: usize,
    /// per-layer work bypass 阈值。
    layer_bypass_work_threshold: usize,
    /// 最近一次相机交互时间戳。
    last_camera_interaction_at: Option<Instant>,
    /// 交互冻结视图是否已经落后于真实相机。
    interaction_view_dirty: bool,
    /// 上一次构建 workset 时实际覆盖的可见世界范围（已经带预取 margin）。
    layout_workset_visible_bounds: Option<Bounds>,
    /// 上一次构建 workset 时的 zoom。
    ///
    /// 视口裁剪本身只看 bounds，但 subtree screen-space LOD 还依赖 zoom，
    /// 所以相机缩放偏离太多时也需要重建。
    layout_workset_zoom: Option<f32>,
    /// 交互期间暂缓的 workset 重建请求。
    ///
    /// 连续 pan/zoom 时我们先让画面跟手；
    /// 等交互窗口过去后，再补一次真正的重建。
    layout_workset_rebuild_pending: bool,
    /// 是否已经为当前会话初始化过 hierarchy level range。
    ///
    /// 如果没有旧配置可恢复，我们只在第一次加载 scene 时按复杂度生成默认值；
    /// 后续切换 view 时更倾向于保留用户已经调过的范围语义。
    hierarchy_level_range_initialized: bool,
}

impl ViewerApp {
    /// 应用启动入口。
    pub fn run() -> Result<(), winit::error::EventLoopError> {
        let event_loop = EventLoop::new()?;
        let mut app = Self::new();
        event_loop.run_app(&mut app)
    }

    /// 创建初始应用状态。
    pub fn new() -> Self {
        let persisted = load_viewer_config().ok().flatten();
        let mut app = Self {
            window: None,
            window_id: None,
            renderer: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            scene_bundle: SceneBundle::empty(),
            layout_bundle: None,
            scene: Arc::new(Scene::empty()),
            scene_layer_ids: Vec::new(),
            scene_bounds_hint: None,
            hierarchy_tile_render_active: false,
            camera: Camera2D::new(),
            load_state: LoadState::Idle,
            layout_path: DEFAULT_LAYOUT_PATH.to_string(),
            initialized_camera: false,
            canvas_size: Vec2::new((INITIAL_WIDTH - 280) as f32, INITIAL_HEIGHT as f32),
            hidden_layers: BTreeSet::new(),
            layer_draw_modes: BTreeMap::new(),
            layer_hatch_styles: BTreeMap::new(),
            frame_stats: FrameStats::new(),
            last_frame_at: None,
            render_debug_stats: RenderDebugStats::default(),
            render_stats_history: RenderStatsHistory::new(),
            pending_restore_config: persisted.clone(),
            tile_grid_divisions: DEFAULT_TILE_GRID_DIVISIONS,
            draw_mode: ClosedShapeDrawMode::HatchOutline,
            hatch_params: HatchParams {
                spacing: DEFAULT_HATCH_SPACING,
                width: DEFAULT_HATCH_WIDTH,
            },
            min_hierarchy_level: 0,
            max_hierarchy_level: 0,
            available_max_hierarchy_level: 0,
            tile_cache_capacity: DEFAULT_TILE_CACHE_CAPACITY,
            progressive_bypass_threshold: DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD,
            layer_bypass_entry_threshold: DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD,
            layer_bypass_work_threshold: DEFAULT_LAYER_BYPASS_WORK_THRESHOLD,
            last_camera_interaction_at: None,
            interaction_view_dirty: false,
            layout_workset_visible_bounds: None,
            layout_workset_zoom: None,
            layout_workset_rebuild_pending: false,
            hierarchy_level_range_initialized: false,
        };

        if let Some(config) = persisted {
            if !config.layout_path.trim().is_empty() {
                app.layout_path = config.layout_path.clone();
            }
            app.draw_mode = config.draw_mode.to_runtime();
            app.hatch_params = HatchParams {
                spacing: config.hatch_spacing,
                width: config.hatch_width,
            };
            app.tile_grid_divisions = config.tile_grid_divisions;
            app.tile_cache_capacity = config.tile_cache_capacity;
            app.progressive_bypass_threshold = config.progressive_bypass_threshold;
            app.layer_bypass_entry_threshold = config.layer_bypass_entry_threshold;
            app.layer_bypass_work_threshold = config.layer_bypass_work_threshold;
        }

        app
    }

    /// 给当前运行场景缺失的 layer hatch preset 补默认值。
    ///
    /// 这里不主动删除“当前 workset 里暂时没出现的 layer”：
    /// - 对旧扁平路径，用户切 level 时仍然可能希望保留更深层的样式选择
    /// - 对新分层路径，深层 layer 在当前 workset 不出现，不代表它永远不存在
    fn sync_layer_hatch_styles_with_scene(&mut self) {
        if self.layout_bundle.is_some() {
            fill_missing_layer_hatch_styles_for_layers(
                &self.scene_layer_ids,
                &mut self.layer_hatch_styles,
            );
        } else {
            fill_missing_layer_hatch_styles(&self.scene, &mut self.layer_hatch_styles);
        }
    }

    /// 用一个只保存 view 名称的轻量 `SceneBundle` 承载层次化 source 的 UI 选择状态。
    ///
    /// 这样我们可以继续复用现有 UI / persistence 代码，
    /// 但不再为了 view 列表而常驻真实扁平场景。
    fn placeholder_scene_bundle_from_layout_bundle(layout_bundle: &LayoutBundle) -> SceneBundle {
        let mut bundle = SceneBundle::new(
            layout_bundle
                .views()
                .iter()
                .map(|view| SceneView {
                    name: view.metadata().name().to_string(),
                    scene: Arc::new(Scene::empty()),
                })
                .collect(),
        );
        let _ = bundle.select(layout_bundle.selected_index());
        bundle
    }

    /// 计算当前层次化 root cell 的最大层级深度。
    ///
    /// 这里只按“实例深度”计数，不关心一个实例重复多少次，
    /// 因为 repetition 会放大图形数量，但不会改变层级深度本身。
    fn compute_layout_root_max_hierarchy_level(
        bundle: &LayoutBundle,
        root_cell_id: LayoutCellId,
    ) -> u32 {
        fn visit(
            bundle: &LayoutBundle,
            cell_id: LayoutCellId,
            cache: &mut HashMap<LayoutCellId, u32>,
            stack: &mut BTreeSet<LayoutCellId>,
        ) -> u32 {
            if let Some(depth) = cache.get(&cell_id) {
                return *depth;
            }
            if !stack.insert(cell_id) {
                return 0;
            }

            let depth = bundle
                .cell(cell_id)
                .map(|cell| {
                    cell.instances()
                        .iter()
                        .map(|instance| 1 + visit(bundle, instance.target_cell_id(), cache, stack))
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);

            stack.remove(&cell_id);
            cache.insert(cell_id, depth);
            depth
        }

        visit(
            bundle,
            root_cell_id,
            &mut HashMap::new(),
            &mut BTreeSet::new(),
        )
    }

    fn estimate_layout_root_level_costs(
        bundle: &LayoutBundle,
        root_cell_id: LayoutCellId,
    ) -> Vec<LayoutHierarchyLevelCost> {
        fn repetition_factor(instance: &crate::layout::LayoutInstance) -> u64 {
            match instance.repetition() {
                Some(crate::layout::LayoutRepetition::RegularGrid { columns, rows, .. }) => {
                    (*columns as u64).saturating_mul(*rows as u64)
                }
                None => 1,
            }
        }

        fn visit(
            bundle: &LayoutBundle,
            cell_id: LayoutCellId,
            cache: &mut HashMap<LayoutCellId, Vec<LayoutHierarchyLevelCost>>,
            stack: &mut BTreeSet<LayoutCellId>,
        ) -> Vec<LayoutHierarchyLevelCost> {
            if let Some(cached) = cache.get(&cell_id) {
                return cached.clone();
            }
            if !stack.insert(cell_id) {
                return Vec::new();
            }

            let mut levels = vec![LayoutHierarchyLevelCost::default()];
            if let Some(cell) = bundle.cell(cell_id) {
                levels[0].shape_count = cell.local_shape_count() as u64;
                levels[0].point_count = cell
                    .local_shapes()
                    .iter()
                    .map(|shape| shape.points().len() as u64)
                    .sum();

                for instance in cell.instances() {
                    let child_levels = visit(bundle, instance.target_cell_id(), cache, stack);
                    let multiplier = repetition_factor(instance);
                    if child_levels.is_empty() || multiplier == 0 {
                        continue;
                    }
                    if levels.len() < child_levels.len() + 1 {
                        levels.resize(child_levels.len() + 1, LayoutHierarchyLevelCost::default());
                    }
                    for (index, child_cost) in child_levels.iter().enumerate() {
                        levels[index + 1].shape_count = levels[index + 1]
                            .shape_count
                            .saturating_add(child_cost.shape_count.saturating_mul(multiplier));
                        levels[index + 1].point_count = levels[index + 1]
                            .point_count
                            .saturating_add(child_cost.point_count.saturating_mul(multiplier));
                    }
                }
            }

            stack.remove(&cell_id);
            cache.insert(cell_id, levels.clone());
            levels
        }

        visit(
            bundle,
            root_cell_id,
            &mut HashMap::new(),
            &mut BTreeSet::new(),
        )
    }

    /// 层次化路径下的初始层级范围。
    ///
    /// 这里不直接展开整棵树，而是先递归估算每一层累计的 shape/point 量，
    /// 再选一个能把首帧 workset 控制在安全预算内的 `max_level`。
    fn recommended_initial_hierarchy_level_range_for_layout(
        bundle: &LayoutBundle,
        root_cell_id: LayoutCellId,
        max_level: u32,
    ) -> (u32, u32) {
        let level_costs = Self::estimate_layout_root_level_costs(bundle, root_cell_id);
        if level_costs.is_empty() {
            return (0, max_level);
        }

        let mut cumulative_shapes = 0u64;
        let mut cumulative_points = 0u64;
        let mut selected_max_level = 0u32;
        for (index, cost) in level_costs.iter().enumerate() {
            let next_shapes = cumulative_shapes.saturating_add(cost.shape_count);
            let next_points = cumulative_points.saturating_add(cost.point_count);
            if index > 0
                && (next_shapes > INITIAL_LAYOUT_WORKSET_SHAPE_BUDGET
                    || next_points > INITIAL_LAYOUT_WORKSET_POINT_BUDGET)
            {
                break;
            }
            cumulative_shapes = next_shapes;
            cumulative_points = next_points;
            selected_max_level = index as u32;
        }

        (0, selected_max_level.min(max_level))
    }

    /// 根据当前 source 重新计算真实可用的最大层级深度。
    fn refresh_available_hierarchy_level_limit(&mut self) {
        self.available_max_hierarchy_level = if let Some(layout_bundle) = &self.layout_bundle {
            layout_bundle
                .selected_root_metadata()
                .map(|metadata| {
                    Self::compute_layout_root_max_hierarchy_level(
                        layout_bundle,
                        metadata.root_cell_id(),
                    )
                })
                .unwrap_or(0)
        } else {
            self.scene_bundle
                .current_scene()
                .map(Scene::max_hierarchy_level)
                .unwrap_or(0)
        };
    }

    fn estimated_layout_range_cost(
        bundle: &LayoutBundle,
        root_cell_id: LayoutCellId,
        min_level: u32,
        max_level: u32,
    ) -> LayoutHierarchyLevelCost {
        let level_costs = Self::estimate_layout_root_level_costs(bundle, root_cell_id);
        let mut total = LayoutHierarchyLevelCost::default();
        for (index, cost) in level_costs.into_iter().enumerate() {
            let level = index as u32;
            if level < min_level || level > max_level {
                continue;
            }
            total.shape_count = total.shape_count.saturating_add(cost.shape_count);
            total.point_count = total.point_count.saturating_add(cost.point_count);
        }
        total
    }

    fn should_use_direct_hierarchy_tile_render(range_cost: LayoutHierarchyLevelCost) -> bool {
        range_cost.shape_count > DIRECT_LAYOUT_TILE_SOURCE_SHAPE_THRESHOLD
            || range_cost.point_count > DIRECT_LAYOUT_TILE_SOURCE_POINT_THRESHOLD
    }

    fn collect_layout_layers_for_range(
        bundle: &LayoutBundle,
        root_cell_id: LayoutCellId,
        min_level: u32,
        max_level: u32,
    ) -> Vec<LayerId> {
        fn visit(
            bundle: &LayoutBundle,
            cell_id: LayoutCellId,
            level: u32,
            min_level: u32,
            max_level: u32,
            layers: &mut BTreeSet<LayerId>,
        ) {
            let Some(cell) = bundle.cell(cell_id) else {
                return;
            };
            if level >= min_level && level <= max_level {
                layers.extend(cell.local_layers().iter().copied());
            }
            if level >= max_level {
                return;
            }
            for instance in cell.instances() {
                visit(
                    bundle,
                    instance.target_cell_id(),
                    level + 1,
                    min_level,
                    max_level,
                    layers,
                );
            }
        }

        let mut layers = BTreeSet::new();
        visit(bundle, root_cell_id, 0, min_level, max_level, &mut layers);
        layers.into_iter().collect()
    }

    /// 根据当前 source 和当前 level range 重新构建运行场景。
    fn rebuild_scene_from_source(&mut self) {
        // 这一步是 app 层“决定走哪条渲染架构”的核心分叉：
        // - 小 / 中型场景：直接构一个临时 Scene workset，renderer 继续走老路径
        // - 大场景：不在 app 层落完整 workset，改让 renderer 按 tile 直接访问 hierarchy
        //
        // 这样做的原因很现实：
        // 继续坚持“所有场景都先扁平化成 Scene”，在大 GDS 上会同时把
        // Scene / spatial index / tile grid / tile cache 都堆起来，内存很快失控。
        let max_available = self.available_max_hierarchy_level;
        self.min_hierarchy_level = self.min_hierarchy_level.min(max_available);
        self.max_hierarchy_level = self
            .max_hierarchy_level
            .min(max_available)
            .max(self.min_hierarchy_level);

        self.hierarchy_tile_render_active = false;
        self.scene = if let Some(layout_bundle) = &self.layout_bundle {
            let visible_world_bounds = self.visible_world_bounds_for_layout_workset();
            let workset_visible_world_bounds = visible_world_bounds.map(|bounds| {
                expand_layout_workset_visible_bounds(bounds, LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO)
            });
            let subtree_screen_lod = self.subtree_screen_lod_for_layout_workset();
            if let Some(metadata) = layout_bundle.selected_root_metadata() {
                let root_cell_id = metadata.root_cell_id();
                let root_bounds = layout_bundle
                    .selected_root_cell()
                    .and_then(|cell| cell.local_bounds());
                let range_cost = Self::estimated_layout_range_cost(
                    layout_bundle,
                    root_cell_id,
                    self.min_hierarchy_level,
                    self.max_hierarchy_level,
                );
                self.scene_layer_ids = Self::collect_layout_layers_for_range(
                    layout_bundle,
                    root_cell_id,
                    self.min_hierarchy_level,
                    self.max_hierarchy_level,
                );
                self.scene_bounds_hint = root_bounds;
                self.hierarchy_tile_render_active =
                    Self::should_use_direct_hierarchy_tile_render(range_cost);

                if self.hierarchy_tile_render_active {
                    Arc::new(Scene::empty())
                } else {
                    let mut options = LayoutViewBuildOptions::new(
                        root_cell_id,
                        self.min_hierarchy_level,
                        self.max_hierarchy_level,
                    )
                    .with_visible_world_bounds(workset_visible_world_bounds);
                    if let Some((
                        world_to_screen_scale,
                        min_subtree_screen_extent,
                        min_collapse_hierarchy_level,
                    )) = subtree_screen_lod
                    {
                        options = options.with_subtree_screen_lod(
                            world_to_screen_scale,
                            min_subtree_screen_extent,
                            min_collapse_hierarchy_level,
                        );
                    }
                    build_layout_view_scene(layout_bundle, options)
                        .map(Arc::new)
                        .unwrap_or_else(|_| Arc::new(Scene::empty()))
                }
            } else {
                self.scene_layer_ids = Vec::new();
                self.scene_bounds_hint = None;
                Arc::new(Scene::empty())
            }
        } else {
            self.scene_layer_ids = self
                .scene_bundle
                .current_scene()
                .map(Scene::layer_ids)
                .unwrap_or_default();
            self.scene_bounds_hint = self.scene_bundle.current_scene().and_then(Scene::bounds);
            let base_scene = self
                .scene_bundle
                .current_scene_handle()
                .unwrap_or_else(|| Arc::new(Scene::empty()));
            if self.min_hierarchy_level == 0
                && self.max_hierarchy_level >= base_scene.max_hierarchy_level()
            {
                base_scene
            } else {
                Arc::new(base_scene.filtered_by_hierarchy_range(
                    self.min_hierarchy_level,
                    self.max_hierarchy_level,
                ))
            }
        };
        if self.layout_bundle.is_some() {
            self.layout_workset_visible_bounds = self
                .visible_world_bounds_for_layout_workset()
                .map(|bounds| {
                    expand_layout_workset_visible_bounds(
                        bounds,
                        LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO,
                    )
                });
            self.layout_workset_zoom = self
                .subtree_screen_lod_for_layout_workset()
                .map(|(zoom, _, _)| zoom);
        } else {
            self.layout_workset_visible_bounds = None;
            self.layout_workset_zoom = None;
        }
        if !self.hierarchy_tile_render_active {
            self.scene_layer_ids = self.scene.layer_ids();
            self.scene_bounds_hint = self.scene.bounds();
        }
    }

    /// 给分层 workset builder 计算当前可用的视口 world bounds。
    ///
    /// 这里故意在首次 fit 之前返回 `None`：
    /// - 初始加载时我们还不知道“用户真正想看的视口”
    /// - 如果过早裁剪，第一次 fit 可能只会对着左上角那一小块内容做适配
    fn visible_world_bounds_for_layout_workset(&self) -> Option<Bounds> {
        if self.layout_bundle.is_none() || !self.initialized_camera {
            return None;
        }
        if self.canvas_size.x <= 0.0 || self.canvas_size.y <= 0.0 {
            return None;
        }

        Some(camera_visible_world_bounds(&self.camera, self.canvas_size))
    }

    /// 给分层 workset 提供一层轻量的 screen-space 子树 LOD。
    ///
    /// 这里和 renderer 的点列 coarse LOD 互补：
    /// - renderer LOD：图形已经展开后，少画一些点
    /// - 这里：在递归展开前，就别把肉眼几乎看不出的深层子树整棵拉进来
    fn subtree_screen_lod_for_layout_workset(&self) -> Option<(f32, f32, u32)> {
        if self.layout_bundle.is_none() || !self.initialized_camera {
            return None;
        }

        Some((
            self.camera.zoom(),
            HIERARCHICAL_SUBTREE_MIN_SCREEN_EXTENT,
            HIERARCHICAL_SUBTREE_MIN_COLLAPSE_LEVEL,
        ))
    }

    /// 初始化或收紧当前的层级范围。
    ///
    /// - `recompute_defaults = true`：按 scene 复杂度重新给一组默认范围
    /// - `recompute_defaults = false`：尽量保留用户当前选择，只做合法范围收紧
    fn initialize_or_clamp_hierarchy_level_range(&mut self, recompute_defaults: bool) {
        if recompute_defaults || !self.hierarchy_level_range_initialized {
            let (min_level, max_level) = if self.layout_bundle.is_some() {
                self.layout_bundle
                    .as_ref()
                    .and_then(|bundle| {
                        bundle.selected_root_metadata().map(|metadata| {
                            Self::recommended_initial_hierarchy_level_range_for_layout(
                                bundle,
                                metadata.root_cell_id(),
                                self.available_max_hierarchy_level,
                            )
                        })
                    })
                    .unwrap_or((0, self.available_max_hierarchy_level))
            } else {
                recommended_initial_hierarchy_level_range(
                    &self
                        .scene_bundle
                        .current_scene()
                        .cloned()
                        .unwrap_or_else(Scene::empty),
                )
            };
            self.min_hierarchy_level = min_level;
            self.max_hierarchy_level = max_level;
            self.hierarchy_level_range_initialized = true;
        } else {
            let max_available = self.available_max_hierarchy_level;
            self.min_hierarchy_level = self.min_hierarchy_level.min(max_available);
            self.max_hierarchy_level = self
                .max_hierarchy_level
                .min(max_available)
                .max(self.min_hierarchy_level);
        }
    }

    /// 把当前 hierarchy level range 的结果同步到 renderer。
    ///
    /// 这里集中处理有两个好处：
    /// - scene 过滤逻辑不会散落在多个事件分支里
    /// - renderer 相关的 scene / per-layer 配置刷新顺序更稳定
    fn refresh_filtered_scene_and_renderer(&mut self) {
        // 这里尽量把“app 的数据准备”和“renderer 的状态同步”绑在一起，
        // 避免出现：
        // - app 里的 scene/range 已经变了
        // - renderer 还保留着旧 tile source / 旧 layer 配置
        //
        // 对 direct hierarchy 路径尤其重要，因为这时 renderer 自己持有一份 hierarchy source。
        self.rebuild_scene_from_source();
        self.sync_layer_hatch_styles_with_scene();
        let subtree_screen_lod = self.subtree_screen_lod_for_layout_workset();
        if let Some(renderer) = self.renderer.as_mut() {
            if let Some(layout_bundle) = &self.layout_bundle {
                if self.hierarchy_tile_render_active {
                    if let (Some(metadata), Some(root_bounds)) = (
                        layout_bundle.selected_root_metadata(),
                        self.scene_bounds_hint,
                    ) {
                        let range_cost = Self::estimated_layout_range_cost(
                            layout_bundle,
                            metadata.root_cell_id(),
                            self.min_hierarchy_level,
                            self.max_hierarchy_level,
                        );
                        let mut options = LayoutViewBuildOptions::new(
                            metadata.root_cell_id(),
                            self.min_hierarchy_level,
                            self.max_hierarchy_level,
                        );
                        if let Some((
                            world_to_screen_scale,
                            min_subtree_screen_extent,
                            min_collapse_hierarchy_level,
                        )) = subtree_screen_lod
                        {
                            options = options.with_subtree_screen_lod(
                                world_to_screen_scale,
                                min_subtree_screen_extent,
                                min_collapse_hierarchy_level,
                            );
                        }
                        renderer.update_hierarchy_tile_source(
                            layout_bundle.clone(),
                            options,
                            self.scene_layer_ids.clone(),
                            root_bounds,
                            range_cost.shape_count as usize,
                        );
                    } else {
                        renderer.update_scene(Arc::clone(&self.scene));
                    }
                } else {
                    renderer.update_scene(Arc::clone(&self.scene));
                }
            } else {
                renderer.update_scene(Arc::clone(&self.scene));
            }
            renderer.set_layer_draw_modes(self.layer_draw_modes.clone());
            renderer.set_layer_hatch_styles(self.layer_hatch_styles.clone());
        }
    }

    fn clear_layout_workset_tracking(&mut self) {
        self.layout_workset_visible_bounds = None;
        self.layout_workset_zoom = None;
        self.layout_workset_rebuild_pending = false;
    }

    fn layout_scene_rebuild_needed_for_current_camera(&self) -> bool {
        // direct hierarchy 模式下，这里只影响“app 是否要重建 workset”；
        // renderer 自己的 tile cache / visible tile 调度仍然会继续工作。
        //
        // 也就是说，这个判定主要保护的是：
        // - 临时 Scene workset 的构建成本
        // - subtree screen-space LOD 的更新频率
        if self.layout_bundle.is_none() || !self.initialized_camera {
            return false;
        }

        let current_visible_world_bounds = self.visible_world_bounds_for_layout_workset();
        should_rebuild_layout_workset_for_camera(
            current_visible_world_bounds,
            self.camera.zoom(),
            self.layout_workset_visible_bounds,
            self.layout_workset_zoom,
        )
    }

    fn refresh_layout_scene_if_camera_requires_rebuild(&mut self) {
        // 这里是“正确性 vs 交互手感”的折中点：
        // - 交互中：尽量不打断 pan/zoom，先把 rebuild 标记成 pending
        // - 停下来：尽快补做真正重建，让可见范围和 LOD 追上相机状态
        let interaction_degraded = should_degrade_interaction_render(
            self.last_camera_interaction_at.map(|last| last.elapsed()),
            INTERACTION_RENDER_DEGRADE_HOLD,
        );

        if !self.layout_scene_rebuild_needed_for_current_camera() {
            self.layout_workset_rebuild_pending = false;
            return;
        }

        if interaction_degraded {
            self.layout_workset_rebuild_pending = true;
        } else {
            self.refresh_filtered_scene_and_renderer();
            self.layout_workset_rebuild_pending = false;
        }
    }

    /// 当用户把 `max level` 调到当前已加载深度之外时，重新加载当前 view。
    fn ensure_loaded_hierarchy_capacity(&mut self, requested_max_level: u32) {
        if self.layout_bundle.is_some()
            || requested_max_level <= self.scene.max_hierarchy_level()
            || requested_max_level > self.available_max_hierarchy_level
        {
            return;
        }

        let selected_index = self.scene_bundle.selected_index();
        if let Ok(mut bundle) = load_layout_bundle(&self.layout_path) {
            let _ = bundle.select(selected_index);
            self.scene_bundle = bundle;
            self.layout_bundle = None;
            self.refresh_available_hierarchy_level_limit();
            let empty_scene = Scene::empty();
            let reference_scene = self.scene_bundle.current_scene().unwrap_or(&empty_scene);
            self.hidden_layers =
                filter_hidden_layers_for_scene(&self.collect_viewer_config(), reference_scene);
            self.layer_draw_modes =
                filter_layer_draw_modes_for_scene(&self.collect_viewer_config(), reference_scene);
            self.layer_hatch_styles =
                filter_layer_hatch_styles_for_scene(&self.collect_viewer_config(), reference_scene);
        }
    }

    /// 创建窗口、renderer 和 egui 状态。
    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size(winit::dpi::LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let mut renderer =
            pollster::block_on(Renderer::new(window.clone())).expect("create renderer");

        // UI 和 renderer 共享同一份 tile grid 参数，
        // 这里让 app 成为“单一真实来源”。
        self.tile_grid_divisions = renderer.tile_grid_divisions();
        self.draw_mode = renderer.draw_mode();
        self.layer_draw_modes = renderer.layer_draw_modes().clone();
        self.hatch_params = renderer.hatch_params();
        self.tile_cache_capacity = renderer.tile_cache_capacity();
        self.progressive_bypass_threshold = renderer.progressive_bypass_threshold();
        self.layer_bypass_entry_threshold = renderer.layer_bypass_entry_threshold();
        self.layer_bypass_work_threshold = renderer.layer_bypass_work_threshold();
        renderer.set_tile_grid_divisions(self.tile_grid_divisions);
        renderer.set_draw_mode(self.draw_mode);
        renderer.set_layer_draw_modes(self.layer_draw_modes.clone());
        renderer.set_layer_hatch_styles(self.layer_hatch_styles.clone());
        renderer.set_hatch_params(self.hatch_params);
        renderer.set_tile_cache_capacity(self.tile_cache_capacity);
        renderer.set_progressive_bypass_threshold(self.progressive_bypass_threshold);
        renderer.set_layer_bypass_thresholds(
            self.layer_bypass_entry_threshold,
            self.layer_bypass_work_threshold,
        );

        let egui_state = EguiWinitState::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            Some(window.theme().unwrap_or(winit::window::Theme::Dark)),
            None,
        );

        self.window_id = Some(window.id());
        self.renderer = Some(renderer);
        self.egui_state = Some(egui_state);
        self.window = Some(window);

        // 窗口创建完成后立刻尝试加载默认版图。
        self.load_layout();
    }

    /// 读取当前配置路径对应的版图文件。
    fn load_layout(&mut self) {
        let path_ext = Path::new(&self.layout_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());

        let load_result = match path_ext.as_deref() {
            Some("gds") => load_layout_hierarchy_bundle(&self.layout_path)
                .map(|layout_bundle| (Some(layout_bundle), None)),
            _ => load_layout_bundle(&self.layout_path).map(|bundle| (None, Some(bundle))),
        };

        match load_result {
            Ok((Some(layout_bundle), None)) => {
                self.layout_bundle = Some(layout_bundle);
                if let Some(layout_bundle) = &self.layout_bundle {
                    self.scene_bundle =
                        Self::placeholder_scene_bundle_from_layout_bundle(layout_bundle);
                }
                self.load_state = LoadState::Loaded;
                self.refresh_available_hierarchy_level_limit();

                if let Some(config) = self.take_matching_restore_config() {
                    self.apply_persisted_scene_config(&config);
                } else {
                    self.hidden_layers.clear();
                    self.layer_draw_modes.clear();
                    self.layer_hatch_styles.clear();
                    self.clear_layout_workset_tracking();
                    self.initialize_or_clamp_hierarchy_level_range(
                        !self.hierarchy_level_range_initialized,
                    );
                    self.refresh_filtered_scene_and_renderer();
                    self.initialized_camera = false;
                }
            }
            Ok((None, Some(bundle))) => {
                self.layout_bundle = None;
                self.scene_bundle = bundle;
                self.load_state = LoadState::Loaded;
                self.refresh_available_hierarchy_level_limit();

                if let Some(config) = self.take_matching_restore_config() {
                    self.apply_persisted_scene_config(&config);
                } else {
                    self.hidden_layers.clear();
                    self.layer_draw_modes.clear();
                    self.layer_hatch_styles.clear();
                    self.clear_layout_workset_tracking();
                    self.initialize_or_clamp_hierarchy_level_range(
                        !self.hierarchy_level_range_initialized,
                    );
                    self.refresh_filtered_scene_and_renderer();
                    self.initialized_camera = false;
                }
            }
            Ok(_) => unreachable!("loader bridge should choose exactly one source"),
            Err(err) => {
                self.layout_bundle = None;
                self.scene_bundle = SceneBundle::empty();
                self.scene = Arc::new(Scene::empty());
                self.scene_layer_ids.clear();
                self.scene_bounds_hint = None;
                self.hierarchy_tile_render_active = false;
                self.load_state = LoadState::Failed(err.to_string());
                self.hidden_layers.clear();
                self.layer_draw_modes.clear();
                self.layer_hatch_styles.clear();
                self.min_hierarchy_level = 0;
                self.max_hierarchy_level = 0;
                self.available_max_hierarchy_level = 0;
                self.hierarchy_level_range_initialized = false;
                self.initialized_camera = false;
                self.clear_layout_workset_tracking();
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.update_scene(Arc::clone(&self.scene));
                }
            }
        }
    }

    /// 从当前运行态收集一份可持久化的 viewer 配置。
    fn collect_viewer_config(&self) -> ViewerConfig {
        ViewerConfig {
            layout_path: self.layout_path.clone(),
            selected_view_name: self
                .scene_bundle
                .current_view()
                .map(|view| view.name.clone()),
            camera: PersistedCamera {
                pan_x: self.camera.pan().x,
                pan_y: self.camera.pan().y,
                zoom: self.camera.zoom(),
            },
            min_hierarchy_level: Some(self.min_hierarchy_level),
            max_hierarchy_level: Some(self.max_hierarchy_level),
            hidden_layers: self
                .hidden_layers
                .iter()
                .copied()
                .map(PersistedLayerId::from_runtime)
                .collect(),
            layer_draw_modes: self
                .layer_draw_modes
                .iter()
                .map(|(layer, mode)| PersistedLayerDrawMode {
                    layer: PersistedLayerId::from_runtime(*layer),
                    mode: PersistedClosedShapeDrawMode::from_runtime(*mode),
                })
                .collect(),
            layer_hatch_styles: self
                .layer_hatch_styles
                .iter()
                .map(|(layer, style)| PersistedLayerHatchStyle {
                    layer: PersistedLayerId::from_runtime(*layer),
                    style: PersistedHatchStylePreset::from_runtime(*style),
                })
                .collect(),
            draw_mode: PersistedClosedShapeDrawMode::from_runtime(self.draw_mode),
            hatch_spacing: self.hatch_params.spacing,
            hatch_width: self.hatch_params.width,
            tile_grid_divisions: self.tile_grid_divisions,
            tile_cache_capacity: self.tile_cache_capacity,
            progressive_bypass_threshold: self.progressive_bypass_threshold,
            layer_bypass_entry_threshold: self.layer_bypass_entry_threshold,
            layer_bypass_work_threshold: self.layer_bypass_work_threshold,
        }
    }

    /// 仅在启动阶段恢复一次“和具体 scene 绑定”的 viewer 状态。
    ///
    /// 这样可以避免用户在当前会话里已经切换过 view / layer 后，
    /// 再次 `Reload` 时被旧配置反向覆盖。
    fn take_matching_restore_config(&mut self) -> Option<ViewerConfig> {
        let should_apply = self
            .pending_restore_config
            .as_ref()
            .map(|config| config.layout_path == self.layout_path)
            .unwrap_or(false);
        if should_apply {
            self.pending_restore_config.take()
        } else {
            None
        }
    }

    /// 在当前 scene 已经加载完成后，应用持久化配置里的 scene 相关状态。
    fn apply_persisted_scene_config(&mut self, config: &ViewerConfig) {
        if let Some(index) =
            resolve_saved_view_index(&self.scene_bundle, config.selected_view_name.as_deref())
        {
            let _ = self.scene_bundle.select(index);
            if let Some(layout_bundle) = self.layout_bundle.as_mut() {
                let _ = layout_bundle.select(index);
            }
        }
        self.refresh_available_hierarchy_level_limit();
        if let (Some(saved_min_level), Some(saved_max_level)) =
            (config.min_hierarchy_level, config.max_hierarchy_level)
        {
            self.min_hierarchy_level = saved_min_level.min(self.available_max_hierarchy_level);
            self.max_hierarchy_level = saved_max_level
                .min(self.available_max_hierarchy_level)
                .max(self.min_hierarchy_level);
            self.hierarchy_level_range_initialized = true;
        } else {
            self.initialize_or_clamp_hierarchy_level_range(true);
        }
        self.refresh_filtered_scene_and_renderer();
        if self.layout_bundle.is_some() {
            self.hidden_layers = filter_hidden_layers_for_layer_ids(config, &self.scene_layer_ids);
            self.layer_draw_modes =
                filter_layer_draw_modes_for_layer_ids(config, &self.scene_layer_ids);
            self.layer_hatch_styles =
                filter_layer_hatch_styles_for_layer_ids(config, &self.scene_layer_ids);
        } else {
            self.hidden_layers = filter_hidden_layers_for_scene(config, &self.scene);
            self.layer_draw_modes = filter_layer_draw_modes_for_scene(config, &self.scene);
            self.layer_hatch_styles = filter_layer_hatch_styles_for_scene(config, &self.scene);
        }
        self.sync_layer_hatch_styles_with_scene();
        self.refresh_filtered_scene_and_renderer();
        self.camera.set_state(
            Vec2::new(config.camera.pan_x, config.camera.pan_y),
            config.camera.zoom,
        );
        self.initialized_camera = true;
        self.refresh_filtered_scene_and_renderer();
    }

    /// 将当前 viewer 配置保存到用户配置文件。
    fn persist_viewer_config(&self) {
        if let Err(error) = save_viewer_config(&self.collect_viewer_config()) {
            eprintln!("failed to save viewer config: {error}");
        }
    }

    /// 切换当前 scene view。
    fn select_scene_view(&mut self, index: usize) {
        if index == self.scene_bundle.selected_index() {
            return;
        }
        if !self.scene_bundle.select(index) {
            return;
        }
        if let Some(layout_bundle) = self.layout_bundle.as_mut() {
            let _ = layout_bundle.select(index);
            self.refresh_available_hierarchy_level_limit();
            self.hidden_layers.clear();
            self.initialize_or_clamp_hierarchy_level_range(false);
            self.initialized_camera = false;
            self.clear_layout_workset_tracking();
            self.refresh_filtered_scene_and_renderer();
            return;
        }

        let Ok(mut bundle) = load_layout_bundle(&self.layout_path) else {
            return;
        };
        if !bundle.select(index) {
            return;
        }

        self.scene_bundle = bundle;
        self.refresh_available_hierarchy_level_limit();
        self.hidden_layers.clear();
        self.initialize_or_clamp_hierarchy_level_range(false);
        self.initialized_camera = false;
        self.clear_layout_workset_tracking();
        self.refresh_filtered_scene_and_renderer();
    }

    /// 将当前场景 fit 到画布。
    fn fit_scene(&mut self) {
        if self.canvas_size.x <= 0.0 || self.canvas_size.y <= 0.0 {
            return;
        }
        let bounds = self
            .layout_bundle
            .as_ref()
            .and_then(LayoutBundle::selected_root_cell)
            .and_then(|cell| cell.local_bounds())
            .or_else(|| self.scene.bounds());
        if let Some(bounds) = bounds {
            self.camera.fit_bounds(bounds, self.canvas_size);
            self.initialized_camera = true;
        }
    }

    /// 请求下一帧重绘。
    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// 一帧完整的 UI + 渲染流程。
    ///
    /// 这部分建议你按顺序读：
    /// 1. 记录帧时间
    /// 2. 让 egui 产出 `UiAction`
    /// 3. 应用用户动作到 app 状态
    /// 4. 调 renderer 画场景
    /// 5. 把 renderer 统计写回 UI
    fn redraw(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_frame_at.replace(now) {
            self.frame_stats.record_frame(now.duration_since(last));
        }

        let window = match &self.window {
            Some(window) => window.clone(),
            None => return,
        };

        let (canvas_origin, full_output) = {
            let egui_state = match &mut self.egui_state {
                Some(egui_state) => egui_state,
                None => return,
            };
            let raw_input = egui_state.take_egui_input(&window);
            let mut action = crate::ui::UiAction::default();
            let full_output = self.egui_ctx.run(raw_input, |ctx| {
                action = draw_ui(
                    ctx,
                    &self.layout_path,
                    &self.load_state,
                    &self.scene_bundle,
                    &self.scene,
                    &self.scene_layer_ids,
                    self.scene_bounds_hint,
                    self.available_max_hierarchy_level,
                    &self.camera,
                    &self.hidden_layers,
                    &self.layer_draw_modes,
                    &self.layer_hatch_styles,
                    &self.frame_stats,
                    &self.render_debug_stats,
                    &self.render_stats_history,
                    self.tile_grid_divisions,
                    self.draw_mode,
                    self.hatch_params,
                    self.min_hierarchy_level,
                    self.max_hierarchy_level,
                    self.tile_cache_capacity,
                    self.progressive_bypass_threshold,
                    self.layer_bypass_entry_threshold,
                    self.layer_bypass_work_threshold,
                );
            });
            egui_state.handle_platform_output(&window, full_output.platform_output.clone());

            let previous_canvas_size = self.canvas_size;
            self.canvas_size = action.canvas_size.max(Vec2::new(1.0, 1.0));
            if self.layout_bundle.is_some()
                && self.initialized_camera
                && self.canvas_size != previous_canvas_size
            {
                self.refresh_layout_scene_if_camera_requires_rebuild();
            }
            if let Some(index) = action.selected_view {
                self.select_scene_view(index);
            }
            if let Some(hidden_layers) = action.hidden_layers {
                self.hidden_layers = hidden_layers;
            }
            if let Some(tile_grid_divisions) = action.tile_grid_divisions {
                self.tile_grid_divisions = tile_grid_divisions;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_tile_grid_divisions(tile_grid_divisions);
                }
            }
            if let Some(draw_mode) = action.draw_mode {
                self.draw_mode = draw_mode;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_draw_mode(draw_mode);
                }
            }
            if let Some(layer_draw_modes) = action.layer_draw_modes {
                self.layer_draw_modes = layer_draw_modes.clone();
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_layer_draw_modes(layer_draw_modes);
                }
            }
            if let Some(layer_hatch_styles) = action.layer_hatch_styles {
                self.layer_hatch_styles = layer_hatch_styles.clone();
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_layer_hatch_styles(layer_hatch_styles);
                }
            }
            if let Some(min_hierarchy_level) = action.min_hierarchy_level {
                self.min_hierarchy_level = min_hierarchy_level;
                self.hierarchy_level_range_initialized = true;
                self.refresh_filtered_scene_and_renderer();
            }
            if let Some(max_hierarchy_level) = action.max_hierarchy_level {
                self.ensure_loaded_hierarchy_capacity(max_hierarchy_level);
                self.max_hierarchy_level = max_hierarchy_level;
                self.hierarchy_level_range_initialized = true;
                self.refresh_filtered_scene_and_renderer();
            }
            if let Some(tile_cache_capacity) = action.tile_cache_capacity {
                self.tile_cache_capacity = tile_cache_capacity;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_tile_cache_capacity(tile_cache_capacity);
                }
            }
            if let Some(progressive_bypass_threshold) = action.progressive_bypass_threshold {
                self.progressive_bypass_threshold = progressive_bypass_threshold;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_progressive_bypass_threshold(progressive_bypass_threshold);
                }
            }
            if let Some(layer_bypass_entry_threshold) = action.layer_bypass_entry_threshold {
                self.layer_bypass_entry_threshold = layer_bypass_entry_threshold;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_layer_bypass_thresholds(
                        self.layer_bypass_entry_threshold,
                        self.layer_bypass_work_threshold,
                    );
                }
            }
            if let Some(layer_bypass_work_threshold) = action.layer_bypass_work_threshold {
                self.layer_bypass_work_threshold = layer_bypass_work_threshold;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_layer_bypass_thresholds(
                        self.layer_bypass_entry_threshold,
                        self.layer_bypass_work_threshold,
                    );
                }
            }
            if action.request_open_file {
                if let Some(path) = FileDialog::new()
                    .add_filter("Layout", &["gds", "oas"])
                    .pick_file()
                {
                    self.layout_path = path.display().to_string();
                    self.load_layout();
                }
            }
            if action.request_reload_layout {
                self.load_layout();
            }
            if !self.initialized_camera || action.request_fit {
                self.fit_scene();
                if self.layout_bundle.is_some() && self.initialized_camera {
                    self.refresh_filtered_scene_and_renderer();
                }
            }
            if action.pan_delta != Vec2::ZERO {
                self.camera.translate_screen(action.pan_delta);
                self.initialized_camera = true;
                self.last_camera_interaction_at = Some(now);
                self.interaction_view_dirty = true;
                self.refresh_layout_scene_if_camera_requires_rebuild();
            }
            if let Some((factor, cursor)) = action.zoom {
                self.camera.zoom_by(factor, cursor);
                self.initialized_camera = true;
                self.last_camera_interaction_at = Some(now);
                self.interaction_view_dirty = true;
                self.refresh_layout_scene_if_camera_requires_rebuild();
            }

            (action.canvas_origin, full_output)
        };

        let interaction_degraded = should_degrade_interaction_render(
            self.last_camera_interaction_at
                .map(|last| now.duration_since(last)),
            INTERACTION_RENDER_DEGRADE_HOLD,
        );

        let renderer = match &mut self.renderer {
            Some(renderer) => renderer,
            None => return,
        };

        let pixels_per_point = window.scale_factor() as f32;

        match renderer.render(
            &self.camera,
            &self.hidden_layers,
            canvas_origin,
            self.canvas_size,
            pixels_per_point,
            interaction_degraded,
            &self.egui_ctx,
            full_output,
            window.as_ref(),
        ) {
            Ok(()) => {
                if !interaction_degraded {
                    self.interaction_view_dirty = false;
                }
                self.render_debug_stats = renderer.debug_stats();
                self.render_stats_history.record(&self.render_debug_stats);
            }
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                renderer.resize(window.inner_size());
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                panic!("wgpu surface out of memory");
            }
            Err(wgpu::SurfaceError::Timeout) => {}
            Err(wgpu::SurfaceError::Other) => {}
        }

        if !interaction_degraded && self.layout_workset_rebuild_pending {
            self.refresh_layout_scene_if_camera_requires_rebuild();
            self.request_redraw();
        }
    }
}

impl ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.create_window(event_loop);
        self.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if Some(window_id) != self.window_id {
            return;
        }

        let window = match &self.window {
            Some(window) => window.clone(),
            None => return,
        };

        let egui_wants_repaint = if let Some(egui_state) = &mut self.egui_state {
            egui_state.on_window_event(window.as_ref(), &event).repaint
        } else {
            false
        };

        if should_request_redraw_after_window_event(egui_wants_repaint, &event) {
            self.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => {
                self.persist_viewer_config();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // 交互刚结束的一小段时间里，我们还要继续请求几帧，
        // 这样才能把交互期临时降级的 outline 视图自然恢复回 hatch。
        let interaction_degraded = should_degrade_interaction_render(
            self.last_camera_interaction_at.map(|last| last.elapsed()),
            INTERACTION_RENDER_DEGRADE_HOLD,
        );
        // 只有当 UI 还想继续动画/交互，renderer 仍在渐进式补全，
        // 或者交互降级窗口还没结束时，才持续请求下一帧。
        if should_request_continuous_redraw(
            self.render_debug_stats.pending_entries,
            self.egui_ctx.has_requested_repaint(),
        ) || interaction_degraded
            || self.interaction_view_dirty
            || self.layout_workset_rebuild_pending
        {
            self.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        layout::{LayoutCell, LayoutShape, LayoutView, LayoutViewMetadata},
        persistence::{
            PersistedCamera, PersistedClosedShapeDrawMode, PersistedLayerId, ViewerConfig,
        },
        scene::{Bounds, LayerId},
    };

    fn sample_layer(layer: u32) -> LayerId {
        LayerId { layer, datatype: 0 }
    }

    fn sample_hierarchical_bundle() -> LayoutBundle {
        let root_id = LayoutCellId::new(1);
        let child_id = LayoutCellId::new(2);
        let grandchild_id = LayoutCellId::new(3);

        let root = Arc::new(LayoutCell::new(
            root_id,
            "root",
            vec![LayoutShape::rectangle(
                sample_layer(10),
                Bounds::new(0.0, 0.0, 10.0, 10.0),
            )],
            vec![crate::layout::LayoutInstance::with_transform(
                child_id,
                Bounds::new(20.0, 0.0, 30.0, 10.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::new(20.0, 0.0),
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let child = Arc::new(LayoutCell::new(
            child_id,
            "child",
            vec![LayoutShape::rectangle(
                sample_layer(11),
                Bounds::new(0.0, 0.0, 8.0, 8.0),
            )],
            vec![crate::layout::LayoutInstance::with_transform(
                grandchild_id,
                Bounds::new(5.0, 5.0, 9.0, 9.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::new(5.0, 5.0),
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let grandchild = Arc::new(LayoutCell::new(
            grandchild_id,
            "grandchild",
            vec![LayoutShape::rectangle(
                sample_layer(12),
                Bounds::new(0.0, 0.0, 4.0, 4.0),
            )],
            Vec::new(),
        ));

        LayoutBundle::new(
            vec![root, child, grandchild],
            vec![
                LayoutView::new(LayoutViewMetadata::new("root", root_id)),
                LayoutView::new(LayoutViewMetadata::new("child", child_id)),
            ],
        )
        .expect("hierarchical bundle")
    }

    fn sample_tiny_subtree_bundle() -> LayoutBundle {
        let root_id = LayoutCellId::new(10);
        let child_id = LayoutCellId::new(11);
        let grandchild_id = LayoutCellId::new(12);
        let great_grandchild_id = LayoutCellId::new(13);
        let leaf_id = LayoutCellId::new(14);

        let root = Arc::new(LayoutCell::new(
            root_id,
            "root",
            Vec::new(),
            vec![crate::layout::LayoutInstance::with_transform(
                child_id,
                Bounds::new(0.0, 0.0, 1.0, 1.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::ZERO,
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let child = Arc::new(LayoutCell::new(
            child_id,
            "child",
            Vec::new(),
            vec![crate::layout::LayoutInstance::with_transform(
                grandchild_id,
                Bounds::new(0.0, 0.0, 1.0, 1.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::ZERO,
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let grandchild = Arc::new(LayoutCell::new(
            grandchild_id,
            "grandchild",
            Vec::new(),
            vec![crate::layout::LayoutInstance::with_transform(
                great_grandchild_id,
                Bounds::new(0.0, 0.0, 1.0, 1.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::ZERO,
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let great_grandchild = Arc::new(LayoutCell::new(
            great_grandchild_id,
            "great-grandchild",
            Vec::new(),
            vec![crate::layout::LayoutInstance::with_transform(
                leaf_id,
                Bounds::new(0.0, 0.0, 1.0, 1.0),
                crate::layout::LayoutTransform {
                    translation: Vec2::ZERO,
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )],
        ));
        let leaf = Arc::new(LayoutCell::new(
            leaf_id,
            "leaf",
            vec![LayoutShape::rectangle(
                sample_layer(20),
                Bounds::new(0.0, 0.0, 1.0, 1.0),
            )],
            Vec::new(),
        ));

        LayoutBundle::new(
            vec![root, child, grandchild, great_grandchild, leaf],
            vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
        )
        .expect("tiny subtree bundle")
    }

    fn sample_large_repetition_bundle() -> LayoutBundle {
        let root_id = LayoutCellId::new(30);
        let child_id = LayoutCellId::new(31);
        let leaf_id = LayoutCellId::new(32);

        let root = Arc::new(LayoutCell::new(
            root_id,
            "root",
            vec![LayoutShape::rectangle(
                sample_layer(30),
                Bounds::new(0.0, 0.0, 1.0, 1.0),
            )],
            vec![
                crate::layout::LayoutInstance::with_transform(
                    child_id,
                    Bounds::new(0.0, 0.0, 10.0, 10.0),
                    crate::layout::LayoutTransform::identity(),
                )
                .with_repetition(crate::layout::LayoutRepetition::regular_grid(
                    2_000,
                    1,
                    Vec2::new(20.0, 0.0),
                    Vec2::new(0.0, 20.0),
                )),
            ],
        ));
        let child = Arc::new(LayoutCell::new(
            child_id,
            "child",
            vec![LayoutShape::rectangle(
                sample_layer(31),
                Bounds::new(0.0, 0.0, 2.0, 2.0),
            )],
            vec![
                crate::layout::LayoutInstance::with_transform(
                    leaf_id,
                    Bounds::new(0.0, 0.0, 5.0, 5.0),
                    crate::layout::LayoutTransform::identity(),
                )
                .with_repetition(crate::layout::LayoutRepetition::regular_grid(
                    2_000,
                    1,
                    Vec2::new(10.0, 0.0),
                    Vec2::new(0.0, 10.0),
                )),
            ],
        ));
        let leaf = Arc::new(LayoutCell::new(
            leaf_id,
            "leaf",
            vec![LayoutShape::rectangle(
                sample_layer(32),
                Bounds::new(0.0, 0.0, 3.0, 3.0),
            )],
            Vec::new(),
        ));

        LayoutBundle::new(
            vec![root, child, leaf],
            vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
        )
        .expect("large repetition bundle")
    }

    #[test]
    fn hierarchical_source_rebuilds_scene_when_level_range_changes() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();

        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 0;
        app.hierarchy_level_range_initialized = true;
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.available_max_hierarchy_level, 2);
        assert_eq!(app.scene.stats().shape_count, 1);
        assert!(
            app.scene
                .shapes()
                .iter()
                .all(|shape| shape.hierarchy_level == 0)
        );

        app.max_hierarchy_level = 1;
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 2);
        assert_eq!(app.scene.max_hierarchy_level(), 1);

        app.max_hierarchy_level = 2;
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 3);
        assert_eq!(app.scene.max_hierarchy_level(), 2);
    }

    #[test]
    fn hierarchical_source_clips_workset_by_visible_world_bounds() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();

        app.canvas_size = Vec2::new(15.0, 15.0);
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 1;
        app.hierarchy_level_range_initialized = true;
        app.initialized_camera = true;

        app.camera.set_state(Vec2::ZERO, 1.0);
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(10));

        app.camera.set_state(Vec2::new(-20.0, 0.0), 1.0);
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(11));
    }

    #[test]
    fn hierarchical_source_skips_tiny_subtrees_when_zoomed_out() {
        let layout_bundle = sample_tiny_subtree_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();

        app.canvas_size = Vec2::new(100.0, 100.0);
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 5;
        app.hierarchy_level_range_initialized = true;
        app.initialized_camera = true;

        app.camera.set_state(Vec2::ZERO, 1.0);
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(20));

        app.camera.set_state(Vec2::ZERO, 4.0);
        app.refresh_filtered_scene_and_renderer();
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(20));
    }

    #[test]
    fn hierarchical_fit_scene_uses_full_source_bounds_not_current_workset_bounds() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();

        app.canvas_size = Vec2::new(100.0, 100.0);
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 2;
        app.hierarchy_level_range_initialized = true;
        app.initialized_camera = true;

        app.camera.set_state(Vec2::new(-50.0, 0.0), 1.0);
        app.refresh_filtered_scene_and_renderer();
        assert!(app.scene.bounds().map(|bounds| bounds.min_x).unwrap_or(0.0) > 0.0);

        app.fit_scene();

        assert!(app.camera.zoom() < 4.0);
    }

    #[test]
    fn hierarchical_initial_range_for_large_layout_stops_before_budget_explodes() {
        let layout_bundle = sample_large_repetition_bundle();
        let root_id = layout_bundle
            .selected_root_metadata()
            .expect("root metadata")
            .root_cell_id();
        let max_level = ViewerApp::compute_layout_root_max_hierarchy_level(&layout_bundle, root_id);

        let (min_level, max_level_selected) =
            ViewerApp::recommended_initial_hierarchy_level_range_for_layout(
                &layout_bundle,
                root_id,
                max_level,
            );

        assert_eq!(min_level, 0);
        assert_eq!(max_level, 2);
        assert_eq!(max_level_selected, 1);
    }

    #[test]
    fn large_layout_range_switches_to_direct_hierarchy_tile_render_mode() {
        let layout_bundle = sample_large_repetition_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 2;
        app.hierarchy_level_range_initialized = true;

        app.refresh_filtered_scene_and_renderer();

        assert!(app.hierarchy_tile_render_active);
        assert!(app.scene.shapes().is_empty());
        assert_eq!(
            app.scene_layer_ids,
            vec![sample_layer(30), sample_layer(31), sample_layer(32)]
        );
        assert_eq!(
            app.scene_bounds_hint,
            layout_bundle
                .selected_root_cell()
                .and_then(|cell| cell.local_bounds())
        );
    }

    #[test]
    fn layout_workset_camera_rebuild_is_skipped_inside_prefetch_margin() {
        let coverage = Bounds::new(-10.0, -10.0, 110.0, 110.0);
        let visible = Bounds::new(0.0, 0.0, 100.0, 100.0);

        assert!(!should_rebuild_layout_workset_for_camera(
            Some(visible),
            1.05,
            Some(coverage),
            Some(1.0),
        ));
    }

    #[test]
    fn layout_workset_camera_rebuild_triggers_when_view_leaves_prefetch_margin() {
        let coverage = Bounds::new(-10.0, -10.0, 110.0, 110.0);
        let visible = Bounds::new(20.0, 0.0, 120.0, 100.0);

        assert!(should_rebuild_layout_workset_for_camera(
            Some(visible),
            1.0,
            Some(coverage),
            Some(1.0),
        ));
    }

    #[test]
    fn layout_workset_camera_rebuild_triggers_when_zoom_changes_too_much() {
        let coverage = Bounds::new(-10.0, -10.0, 110.0, 110.0);
        let visible = Bounds::new(0.0, 0.0, 100.0, 100.0);

        assert!(should_rebuild_layout_workset_for_camera(
            Some(visible),
            1.3,
            Some(coverage),
            Some(1.0),
        ));
    }

    #[test]
    fn layout_workset_camera_rebuild_is_deferred_while_interaction_is_active() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();
        app.canvas_size = Vec2::new(15.0, 15.0);
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 1;
        app.hierarchy_level_range_initialized = true;
        app.initialized_camera = true;

        app.camera.set_state(Vec2::ZERO, 1.0);
        app.refresh_filtered_scene_and_renderer();
        let previous_bounds = app.scene.bounds();

        app.camera.set_state(Vec2::new(-20.0, 0.0), 1.0);
        app.last_camera_interaction_at = Some(Instant::now());
        app.refresh_layout_scene_if_camera_requires_rebuild();

        assert!(app.layout_workset_rebuild_pending);
        assert_eq!(app.scene.bounds(), previous_bounds);
    }

    #[test]
    fn layout_workset_camera_rebuild_executes_after_interaction_settles() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();
        app.canvas_size = Vec2::new(15.0, 15.0);
        app.min_hierarchy_level = 0;
        app.max_hierarchy_level = 1;
        app.hierarchy_level_range_initialized = true;
        app.initialized_camera = true;

        app.camera.set_state(Vec2::ZERO, 1.0);
        app.refresh_filtered_scene_and_renderer();

        app.camera.set_state(Vec2::new(-20.0, 0.0), 1.0);
        app.last_camera_interaction_at =
            Some(Instant::now() - INTERACTION_RENDER_DEGRADE_HOLD - Duration::from_millis(1));
        app.layout_workset_rebuild_pending = true;
        app.refresh_layout_scene_if_camera_requires_rebuild();

        assert!(!app.layout_workset_rebuild_pending);
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(11));
    }

    #[test]
    fn persisted_hierarchical_scene_config_restores_selected_view_and_range() {
        let layout_bundle = sample_hierarchical_bundle();
        let mut app = ViewerApp::new();
        app.layout_bundle = Some(layout_bundle.clone());
        app.scene_bundle = ViewerApp::placeholder_scene_bundle_from_layout_bundle(&layout_bundle);
        app.refresh_available_hierarchy_level_limit();

        let config = ViewerConfig {
            layout_path: "/tmp/example.gds".to_string(),
            selected_view_name: Some("child".to_string()),
            camera: PersistedCamera {
                pan_x: 10.0,
                pan_y: 20.0,
                zoom: 1.5,
            },
            min_hierarchy_level: Some(0),
            max_hierarchy_level: Some(0),
            hidden_layers: vec![PersistedLayerId {
                layer: 11,
                datatype: 0,
            }],
            layer_draw_modes: Vec::new(),
            layer_hatch_styles: Vec::new(),
            draw_mode: PersistedClosedShapeDrawMode::HatchOutline,
            hatch_spacing: DEFAULT_HATCH_SPACING,
            hatch_width: DEFAULT_HATCH_WIDTH,
            tile_grid_divisions: DEFAULT_TILE_GRID_DIVISIONS,
            tile_cache_capacity: DEFAULT_TILE_CACHE_CAPACITY,
            progressive_bypass_threshold: DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD,
            layer_bypass_entry_threshold: DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD,
            layer_bypass_work_threshold: DEFAULT_LAYER_BYPASS_WORK_THRESHOLD,
        };

        app.apply_persisted_scene_config(&config);

        assert_eq!(app.scene_bundle.selected_index(), 1);
        assert_eq!(
            app.layout_bundle.as_ref().map(LayoutBundle::selected_index),
            Some(1)
        );
        assert_eq!(app.min_hierarchy_level, 0);
        assert_eq!(app.max_hierarchy_level, 0);
        assert_eq!(app.scene.stats().shape_count, 1);
        assert_eq!(app.scene.shapes()[0].layer, sample_layer(11));
        assert_eq!(app.hidden_layers, BTreeSet::from([sample_layer(11)]));
        assert_eq!(app.camera.pan(), Vec2::new(10.0, 20.0));
        assert_eq!(app.camera.zoom(), 1.5);
    }
}
