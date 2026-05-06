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

use std::{collections::{BTreeMap, BTreeSet}, sync::Arc, time::{Duration, Instant}};

use egui_winit::State as EguiWinitState;
use rfd::FileDialog;
use glam::Vec2;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

use crate::{
    camera::Camera2D,
    config::{DEFAULT_LAYOUT_PATH, INITIAL_HEIGHT, INITIAL_WIDTH, WINDOW_TITLE},
    io::load_layout_bundle,
    perf::{FrameStats, RenderStatsHistory},
    persistence::{
        PersistedCamera, PersistedClosedShapeDrawMode, PersistedLayerDrawMode,
        PersistedHatchStylePreset, PersistedLayerHatchStyle, PersistedLayerId, ViewerConfig,
        filter_hidden_layers_for_scene, filter_layer_draw_modes_for_scene,
        filter_layer_hatch_styles_for_scene, load_viewer_config, resolve_saved_view_index,
        save_viewer_config,
    },
    renderer::{
        RenderDebugStats, Renderer, DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD,
        DEFAULT_LAYER_BYPASS_WORK_THRESHOLD, DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD,
        DEFAULT_TILE_CACHE_CAPACITY,
        geometry::{
            ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_WIDTH,
            DEFAULT_TILE_GRID_DIVISIONS, HatchParams, HatchStylePreset,
        },
    },
    scene::{LayerId, Scene, SceneBundle},
    ui::draw_ui,
};

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
pub fn should_request_continuous_redraw(
    pending_entries: usize,
    egui_wants_repaint: bool,
) -> bool {
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
    egui_wants_repaint || matches!(
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

/// 把外部传入的层级范围收紧到当前 scene 的真实范围内。
///
/// 这个小 helper 的价值在于把两种边界情况收进一个地方：
/// - 用户滑杆可能拉到超过当前 scene 最大层级
/// - `min > max` 时需要自动纠正
fn clamp_hierarchy_level_range(scene: &Scene, min_level: u32, max_level: u32) -> (u32, u32) {
    let scene_max_level = scene.max_hierarchy_level();
    let clamped_min = min_level.min(scene_max_level);
    let clamped_max = max_level.min(scene_max_level).max(clamped_min);
    (clamped_min, clamped_max)
}

/// Viewer 应用状态。
pub struct ViewerApp {
    window: Option<Arc<Window>>,
    window_id: Option<WindowId>,
    renderer: Option<Renderer>,
    egui_ctx: egui::Context,
    egui_state: Option<EguiWinitState>,
    scene_bundle: SceneBundle,
    /// 当前选中 view 对应的完整场景，不受 hierarchy level range 过滤。
    full_scene: Scene,
    /// 当前真正送入 renderer 的场景。
    ///
    /// 这里通常等于 `full_scene` 经过 level range 过滤后的结果。
    scene: Scene,
    camera: Camera2D,
    load_state: LoadState,
    layout_path: String,
    initialized_camera: bool,
    canvas_size: Vec2,
    hidden_layers: BTreeSet<LayerId>,
    layer_draw_modes: BTreeMap<LayerId, ClosedShapeDrawMode>,
    layer_hatch_styles: BTreeMap<LayerId, HatchStylePreset>,
    frame_stats: FrameStats,
    last_frame_at: Option<Instant>,
    render_debug_stats: RenderDebugStats,
    render_stats_history: RenderStatsHistory,
    pending_restore_config: Option<ViewerConfig>,
    tile_grid_divisions: u32,
    draw_mode: ClosedShapeDrawMode,
    hatch_params: HatchParams,
    /// 当前显示层级范围的下界。
    min_hierarchy_level: u32,
    /// 当前显示层级范围的上界。
    max_hierarchy_level: u32,
    tile_cache_capacity: usize,
    progressive_bypass_threshold: usize,
    layer_bypass_entry_threshold: usize,
    layer_bypass_work_threshold: usize,
    last_camera_interaction_at: Option<Instant>,
    interaction_view_dirty: bool,
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
            full_scene: Scene::empty(),
            scene: Scene::empty(),
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
            tile_cache_capacity: DEFAULT_TILE_CACHE_CAPACITY,
            progressive_bypass_threshold: DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD,
            layer_bypass_entry_threshold: DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD,
            layer_bypass_work_threshold: DEFAULT_LAYER_BYPASS_WORK_THRESHOLD,
            last_camera_interaction_at: None,
            interaction_view_dirty: false,
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

    /// 让 per-layer hatch preset 和当前 scene 保持一致。
    ///
    /// 这里分两步做，而不是简单 `clear + refill`：
    /// 1. 先删掉当前 scene 里已经不存在的 layer，避免把旧 view 的垃圾状态写回配置
    /// 2. 再给缺失 layer 补默认值，这样同 layer 的显式选择能跨 view 继续保留
    fn sync_layer_hatch_styles_with_scene(&mut self) {
        let existing_layers: BTreeSet<LayerId> = self.full_scene.layer_ids().into_iter().collect();
        self.layer_hatch_styles
            .retain(|layer, _| existing_layers.contains(layer));
        fill_missing_layer_hatch_styles(&self.full_scene, &mut self.layer_hatch_styles);
    }

    /// 用当前 `SceneBundle` 里选中的 view 刷新 `full_scene`。
    ///
    /// 这个函数只负责切到“完整数据”，还不做 level 过滤。
    fn set_full_scene_from_bundle(&mut self) {
        self.full_scene = self
            .scene_bundle
            .current_scene()
            .cloned()
            .unwrap_or_else(Scene::empty);
    }

    /// 根据当前 `min/max level` 重新生成过滤后的运行场景。
    fn apply_current_hierarchy_level_filter(&mut self) {
        let (min_level, max_level) = clamp_hierarchy_level_range(
            &self.full_scene,
            self.min_hierarchy_level,
            self.max_hierarchy_level,
        );
        self.min_hierarchy_level = min_level;
        self.max_hierarchy_level = max_level;
        self.scene = self
            .full_scene
            .filtered_by_hierarchy_range(self.min_hierarchy_level, self.max_hierarchy_level);
    }

    /// 初始化或收紧当前的层级范围。
    ///
    /// - `recompute_defaults = true`：按 scene 复杂度重新给一组默认范围
    /// - `recompute_defaults = false`：尽量保留用户当前选择，只做合法范围收紧
    fn initialize_or_clamp_hierarchy_level_range(&mut self, recompute_defaults: bool) {
        if recompute_defaults || !self.hierarchy_level_range_initialized {
            let (min_level, max_level) =
                recommended_initial_hierarchy_level_range(&self.full_scene);
            self.min_hierarchy_level = min_level;
            self.max_hierarchy_level = max_level;
            self.hierarchy_level_range_initialized = true;
        } else {
            let (min_level, max_level) = clamp_hierarchy_level_range(
                &self.full_scene,
                self.min_hierarchy_level,
                self.max_hierarchy_level,
            );
            self.min_hierarchy_level = min_level;
            self.max_hierarchy_level = max_level;
        }
    }

    /// 把当前 hierarchy level range 的结果同步到 renderer。
    ///
    /// 这里集中处理有两个好处：
    /// - scene 过滤逻辑不会散落在多个事件分支里
    /// - renderer 相关的 scene / per-layer 配置刷新顺序更稳定
    fn refresh_filtered_scene_and_renderer(&mut self) {
        self.apply_current_hierarchy_level_filter();
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.update_scene(&self.scene);
            renderer.set_layer_draw_modes(self.layer_draw_modes.clone());
            renderer.set_layer_hatch_styles(self.layer_hatch_styles.clone());
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
        match load_layout_bundle(&self.layout_path) {
            Ok(bundle) => {
                self.scene_bundle = bundle;
                self.load_state = LoadState::Loaded;
                self.set_full_scene_from_bundle();

                if let Some(config) = self.take_matching_restore_config() {
                    self.apply_persisted_scene_config(&config);
                } else {
                    self.hidden_layers.clear();
                    self.layer_draw_modes.clear();
                    self.layer_hatch_styles.clear();
                    self.sync_layer_hatch_styles_with_scene();
                    self.initialize_or_clamp_hierarchy_level_range(
                        !self.hierarchy_level_range_initialized,
                    );
                    self.apply_current_hierarchy_level_filter();
                    // 新场景加载后，需要重新 fit。
                    self.initialized_camera = false;
                }

                self.refresh_filtered_scene_and_renderer();
            }
            Err(err) => {
                self.scene_bundle = SceneBundle::empty();
                self.full_scene = Scene::empty();
                self.scene = Scene::empty();
                self.load_state = LoadState::Failed(err.to_string());
                self.hidden_layers.clear();
                self.layer_draw_modes.clear();
                self.layer_hatch_styles.clear();
                self.min_hierarchy_level = 0;
                self.max_hierarchy_level = 0;
                self.hierarchy_level_range_initialized = false;
                self.initialized_camera = false;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.update_scene(&self.scene);
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
        if let Some(index) = resolve_saved_view_index(&self.scene_bundle, config.selected_view_name.as_deref()) {
            let _ = self.scene_bundle.select(index);
        }
        self.set_full_scene_from_bundle();
        self.hidden_layers = filter_hidden_layers_for_scene(config, &self.full_scene);
        self.layer_draw_modes = filter_layer_draw_modes_for_scene(config, &self.full_scene);
        self.layer_hatch_styles = filter_layer_hatch_styles_for_scene(config, &self.full_scene);
        self.sync_layer_hatch_styles_with_scene();
        if let (Some(saved_min_level), Some(saved_max_level)) =
            (config.min_hierarchy_level, config.max_hierarchy_level)
        {
            let (min_level, max_level) = clamp_hierarchy_level_range(
                &self.full_scene,
                saved_min_level,
                saved_max_level,
            );
            self.min_hierarchy_level = min_level;
            self.max_hierarchy_level = max_level;
            self.hierarchy_level_range_initialized = true;
        } else {
            self.initialize_or_clamp_hierarchy_level_range(true);
        }
        self.apply_current_hierarchy_level_filter();
        self.camera
            .set_state(Vec2::new(config.camera.pan_x, config.camera.pan_y), config.camera.zoom);
        self.initialized_camera = true;
    }

    /// 将当前 viewer 配置保存到用户配置文件。
    fn persist_viewer_config(&self) {
        if let Err(error) = save_viewer_config(&self.collect_viewer_config()) {
            eprintln!("failed to save viewer config: {error}");
        }
    }

    /// 切换当前 scene view。
    fn select_scene_view(&mut self, index: usize) {
        if !self.scene_bundle.select(index) {
            return;
        }

        self.set_full_scene_from_bundle();
        self.hidden_layers.clear();
        self.sync_layer_hatch_styles_with_scene();
        self.initialize_or_clamp_hierarchy_level_range(false);
        self.apply_current_hierarchy_level_filter();
        self.initialized_camera = false;
        self.refresh_filtered_scene_and_renderer();
    }

    /// 将当前场景 fit 到画布。
    fn fit_scene(&mut self) {
        if let Some(bounds) = self.scene.bounds() {
            if self.canvas_size.x > 0.0 && self.canvas_size.y > 0.0 {
                self.camera.fit_bounds(bounds, self.canvas_size);
                self.initialized_camera = true;
            }
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
                    self.full_scene.max_hierarchy_level(),
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

            self.canvas_size = action.canvas_size.max(Vec2::new(1.0, 1.0));
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
            }
            if action.pan_delta != Vec2::ZERO {
                self.camera.translate_screen(action.pan_delta);
                self.initialized_camera = true;
                self.last_camera_interaction_at = Some(now);
                self.interaction_view_dirty = true;
                self.interaction_view_dirty = true;
            }
            if let Some((factor, cursor)) = action.zoom {
                self.camera.zoom_by(factor, cursor);
                self.initialized_camera = true;
                self.last_camera_interaction_at = Some(now);
            }

            (action.canvas_origin, full_output)
        };

        let interaction_degraded = should_degrade_interaction_render(
            self.last_camera_interaction_at.map(|last| now.duration_since(last)),
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
        ) || interaction_degraded || self.interaction_view_dirty {
            self.request_redraw();
        }
    }
}
