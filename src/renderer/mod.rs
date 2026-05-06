//! 主渲染器。
//!
//! 这个模块负责把 scene 真正画到屏幕上，并串起几何查询、tile cache、GPU pass。
//! 当前结构大致可以分成三层：
//! - `geometry`：准备可见 tiles / 顶点数据 / 缓存 key
//! - `pipeline`：描述 GPU 如何画这些顶点
//! - `Renderer`：持有 wgpu 资源并执行一帧渲染
//!
//! 对学习来说，最关键的是理解这里的缓存层次：
//! 1. `cached_scene_key`：控制“这一帧是否要重新做场景查询”
//! 2. `tile_cache_domain`：控制“tile buffer 是否还能复用”
//! 3. `tile_vertex_cache`：真正缓存每个 tile 的 GPU vertex buffer

pub mod geometry;
pub mod pipeline;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    sync::Arc,
};

use egui_wgpu::{Renderer as EguiRenderer, RendererOptions, ScreenDescriptor};
use glam::Vec2;
use wgpu::util::DeviceExt;
use winit::{dpi::PhysicalSize, window::Window};

use crate::{
    camera::Camera2D,
    error::AppError,
    scene::{LayerId, Scene},
};

use self::{
    geometry::{
        ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_STYLE_PRESET,
        DEFAULT_HATCH_WIDTH, DEFAULT_TILE_GRID_DIVISIONS, HatchParams, HatchStylePreset,
        LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD, LineVertex, PreparedTileFragments,
        RenderCacheKey, ShapeSpatialIndex, TileGridIndex, TileId, build_hatch_signature,
        build_hatch_style_signature, build_render_cache_key_with_hatch_styles,
        build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles,
        build_scaled_scene_vertices_for_tile, camera_visible_world_bounds,
        layer_hatch_style_hash_value, logical_viewport_size, prepare_large_shape_tile_fragments,
        query_visible_shapes, query_visible_tiles,
    },
    pipeline::{ScenePipeline, SceneUniform},
};

/// 暴露给 UI 的渲染调试统计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RenderDebugStats {
    pub total_shapes: usize,
    pub candidate_shapes: usize,
    pub visible_shapes: usize,
    pub bucket_hits: usize,
    pub vertex_count: usize,
    pub draw_calls: usize,
    pub visible_tiles: usize,
    pub tile_cache_hits: usize,
    pub tile_cache_misses: usize,
    pub layer_cache_hits: usize,
    pub layer_cache_misses: usize,
    pub cache_entries: usize,
    pub cache_capacity: usize,
    pub cache_bytes: usize,
    pub cache_evictions: usize,
    pub prepared_shapes: usize,
    pub prepared_tiles: usize,
    pub prepared_fragments: usize,
    pub pending_entries: usize,
    pub build_budget: usize,
    pub dropped_stale_entries: usize,
    pub active_layer: Option<LayerId>,
    pub active_layer_pending: usize,
    pub active_layer_estimated_work: usize,
    pub active_layer_progress_mode: Option<ActiveLayerProgressMode>,
    pub progressive_bypassed: bool,
    pub layer_bypass_entry_threshold: usize,
    pub layer_bypass_work_threshold: usize,
    pub cache_hit: bool,
    pub hatch_spacing: f32,
    pub hatch_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveLayerProgressMode {
    /// 当前活动 layer 满足轻量条件，直接一帧补完。
    Bypassed,
    /// 当前活动 layer 仍然按预算渐进补全。
    Progressive,
}

/// 当前活动 layer 的待处理工作统计。
///
/// 这份统计不是“精确成本模型”，而是一份足够稳定的启发式：
/// - `pending_entries`：还差多少个 `tile + layer` 条目没建好
/// - `prepared_fragment_count`：超大 shape 预碎片化带来的片段量
/// - `regular_shape_count`：普通 shape 数量
/// - `estimated_work_units`：把上面几项压成一个粗粒度工作量分数
///
/// 它的职责不是替代 profiler，而是帮助 renderer 做：
/// - 当前 layer 是否该 bypass
/// - UI 里该怎么解释“为什么这层一帧补完/为什么这层还在渐进”
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerPendingStats {
    pub pending_entries: usize,
    pub estimated_work_units: usize,
    pub prepared_fragment_count: usize,
    pub regular_shape_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerAdaptiveBypassConfig {
    /// 允许当前 layer 直接一帧补完的 entry 数上限。
    pub entry_threshold: usize,
    /// 允许当前 layer 直接一帧补完的估算工作量上限。
    pub work_threshold: usize,
}

impl RenderDebugStats {
    pub fn new(
        total_shapes: usize,
        candidate_shapes: usize,
        visible_shapes: usize,
        bucket_hits: usize,
        vertex_count: usize,
        draw_calls: usize,
        visible_tiles: usize,
        tile_cache_hits: usize,
        tile_cache_misses: usize,
        layer_cache_hits: usize,
        layer_cache_misses: usize,
        cache_entries: usize,
        cache_capacity: usize,
        cache_bytes: usize,
        cache_evictions: usize,
        prepared_shapes: usize,
        prepared_tiles: usize,
        prepared_fragments: usize,
        pending_entries: usize,
        build_budget: usize,
        dropped_stale_entries: usize,
        active_layer: Option<LayerId>,
        active_layer_pending: usize,
        active_layer_estimated_work: usize,
        active_layer_progress_mode: Option<ActiveLayerProgressMode>,
        progressive_bypassed: bool,
        layer_bypass_entry_threshold: usize,
        layer_bypass_work_threshold: usize,
        cache_hit: bool,
        hatch_spacing: f32,
        hatch_width: f32,
    ) -> Self {
        Self {
            total_shapes,
            candidate_shapes,
            visible_shapes,
            bucket_hits,
            vertex_count,
            draw_calls,
            visible_tiles,
            tile_cache_hits,
            tile_cache_misses,
            layer_cache_hits,
            layer_cache_misses,
            cache_entries,
            cache_capacity,
            cache_bytes,
            cache_evictions,
            prepared_shapes,
            prepared_tiles,
            prepared_fragments,
            pending_entries,
            build_budget,
            dropped_stale_entries,
            active_layer,
            active_layer_pending,
            active_layer_estimated_work,
            active_layer_progress_mode,
            progressive_bypassed,
            layer_bypass_entry_threshold,
            layer_bypass_work_threshold,
            cache_hit,
            hatch_spacing,
            hatch_width,
        }
    }
}

/// 单个 `tile + layer` 缓存条目的 key。
///
/// 这一层比原来的"整 tile 一个 key"更细，
/// 这样切换单个 layer 的显隐或显示模式时，其他 layer 更有机会继续复用旧 buffer。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileCacheKey {
    scene_revision: u64,
    zoom_bits: u32,
    tile_id: TileId,
    layer: LayerId,
    effective_mode_tag: u8,
    hatch_signature: u64,
    effective_hatch_style_tag: u8,
}

/// 一组 tile cache 共享的“域”。
///
/// 现在缓存已经细化到 `tile + layer`，
/// 所以 domain 可以收敛成只描述那些"会影响所有 layer 顶点坐标或图案"的全局条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TileCacheDomainKey {
    scene_revision: u64,
    zoom_bits: u32,
    hatch_signature: u64,
    hatch_style_signature: u64,
}


/// 一条等待构建的 `tile + layer` 缓存请求。
///
/// 渐进式渲染的关键点不是“把所有东西都算完”，
/// 而是只承认最新视图状态；旧视图遗留的工作要么跳过，要么被丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingTileBuild {
    view_revision: u64,
    tile_key: TileCacheKey,
}

/// 一个 tile 对应的一段 GPU 顶点缓冲。
///
/// 这里除了 GPU buffer 本身，还刻意保留了 CPU 顶点副本：
/// - 显示阶段可以再按 tile 重新合批
/// - 不必每次都反向从 GPU 取数据
struct TileCacheEntry {
    cpu_vertices: Arc<[LineVertex]>,
    vertex_count: u32,
    byte_size: usize,
    last_used_tick: u64,
}

/// 一个 tile 当前可见 layer 组合对应的显示 batch。
///
/// 底层几何缓存仍然保持 `tile + layer` 粒度，
/// 这里只是在显示阶段把同一 tile 下当前需要画的 layer 顶点拼成一个临时批次，
/// 以减少单帧 draw call。
///
/// 可以把它理解成“第二级显示缓存”：
/// - 第一级：`tile + layer` 几何缓存
/// - 第二级：当前这一帧真正要画的 per-tile 合批结果
struct TileDisplayBatchEntry {
    signature: u64,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,
    byte_size: usize,
}

/// 默认 tile cache 条目上限。
pub const DEFAULT_TILE_CACHE_CAPACITY: usize = 512;

/// 每一帧最多新构建多少个 `tile + layer` 缓存条目。
///
/// 这个预算值决定了 viewer 在交互时更偏向“快速给反馈”，还是“尽快把空白补满”。
pub const DEFAULT_PROGRESSIVE_BUILD_BUDGET: usize = 16;

/// 当缺失条目数不多时，直接在同一帧补完，避免用户感知到“没必要的渐进式补全”。
pub const DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD: usize = 16;

/// UI 允许的最小 tile cache 容量。
pub const MIN_TILE_CACHE_CAPACITY: usize = 32;

/// UI 允许的最大 tile cache 容量。
pub const MAX_TILE_CACHE_CAPACITY: usize = 4096;

pub const MIN_PROGRESSIVE_BYPASS_THRESHOLD: usize = 0;
pub const MAX_PROGRESSIVE_BYPASS_THRESHOLD: usize = 256;
pub const DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD: usize = 8;
pub const DEFAULT_LAYER_BYPASS_WORK_THRESHOLD: usize = 128;
pub const MIN_LAYER_BYPASS_ENTRY_THRESHOLD: usize = 0;
pub const MAX_LAYER_BYPASS_ENTRY_THRESHOLD: usize = 64;
pub const MIN_LAYER_BYPASS_WORK_THRESHOLD: usize = 0;
pub const MAX_LAYER_BYPASS_WORK_THRESHOLD: usize = 1024;
const LAYER_WORK_ENTRY_WEIGHT: usize = 1;
const LAYER_WORK_PREPARED_WEIGHT: usize = 4;
const LAYER_WORK_REGULAR_WEIGHT: usize = 2;

#[derive(Debug, Clone, Copy, Default)]
struct CachedShapeQueryStats {
    candidate_shapes: usize,
    visible_shapes: usize,
    bucket_hits: usize,
    visible_tiles: usize,
}

/// GPU 渲染器。
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    egui_renderer: EguiRenderer,
    scene_pipeline: ScenePipeline,
    scene: Scene,
    spatial_index: ShapeSpatialIndex,
    tile_grid: TileGridIndex,
    prepared_tile_fragments: PreparedTileFragments,
    tile_grid_divisions: u32,
    draw_mode: ClosedShapeDrawMode,
    layer_draw_modes: BTreeMap<LayerId, ClosedShapeDrawMode>,
    hatch_params: HatchParams,
    hatch_style: HatchStylePreset,
    layer_hatch_styles: BTreeMap<LayerId, HatchStylePreset>,
    tile_cache_capacity: usize,
    scene_revision: u64,
    cache_access_tick: u64,
    total_cache_evictions: usize,
    cached_scene_key: Option<RenderCacheKey>,
    tile_cache_domain: Option<TileCacheDomainKey>,
    tile_vertex_cache: HashMap<TileCacheKey, TileCacheEntry>,
    tile_display_batch_cache: HashMap<TileId, TileDisplayBatchEntry>,
    visible_tile_keys: Vec<TileCacheKey>,
    visible_tiles_to_draw: Vec<TileId>,
    requested_tile_keys: Vec<TileCacheKey>,
    pending_tile_builds: VecDeque<PendingTileBuild>,
    /// 当前处在“构建渐进流程”中的活动 layer。
    ///
    /// 这决定了：
    /// - 当前 build budget 主要花在哪一层
    /// - 后续 layer 是否需要继续等待
    active_progressive_layer: Option<LayerId>,
    /// 当前处在“显示渐进流程”中的活动 layer。
    ///
    /// 之所以和 `active_progressive_layer` 分开，是因为：
    /// - 某层的缓存可能已经建好了
    /// - 但为了减少单帧 draw 峰值，我们仍然可能选择分批把它显示出来
    active_display_layer: Option<LayerId>,
    /// 当前活动显示层已经放开的条目预算。
    active_display_budget: usize,
    /// 冻结交互中间视图时，当前稳定视图的 zoom 基准。
    ///
    /// 拖拽/缩放过程中我们会暂时跳过中间状态重算，
    /// 这时用 `position_scale = camera.zoom / display_zoom_basis` 在 shader 里做视觉跟随。
    display_zoom_basis: f32,
    /// 最新视图状态的修订号。
    ///
    /// 每次影响可见结果的条件变化后，这个数字都会递增；
    /// 旧 revision 留下的 pending 构建请求应该被直接丢弃。
    view_revision: u64,
    progressive_build_budget: usize,
    progressive_bypass_threshold: usize,
    layer_bypass_entry_threshold: usize,
    layer_bypass_work_threshold: usize,
    /// 累计丢掉了多少条“已经过期的旧视图构建请求”。
    total_stale_drops: usize,
    shape_query_stats: CachedShapeQueryStats,
    debug_stats: RenderDebugStats,
}

impl Renderer {
    /// 创建渲染器并初始化所有 wgpu 资源。
    pub async fn new(window: Arc<Window>) -> Result<Self, AppError> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window)
            .map_err(|err| AppError::Render(err.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|err| AppError::Render(err.to_string()))?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("flayout-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|err| AppError::Render(err.to_string()))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|candidate| candidate.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::AutoVsync) {
            wgpu::PresentMode::AutoVsync
        } else {
            caps.present_modes[0]
        };
        let alpha_mode = caps.alpha_modes[0];
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let egui_renderer = EguiRenderer::new(&device, format, RendererOptions::default());
        let scene_pipeline = ScenePipeline::new(&device, format);
        let empty_scene = Scene::empty();

        Ok(Self {
            surface,
            device,
            queue,
            config,
            size,
            egui_renderer,
            scene_pipeline,
            scene: empty_scene.clone(),
            spatial_index: ShapeSpatialIndex::build(&empty_scene),
            tile_grid: TileGridIndex::build_with_divisions(
                &empty_scene,
                DEFAULT_TILE_GRID_DIVISIONS,
            ),
            prepared_tile_fragments: PreparedTileFragments::default(),
            tile_grid_divisions: DEFAULT_TILE_GRID_DIVISIONS,
            draw_mode: ClosedShapeDrawMode::HatchOutline,
            layer_draw_modes: BTreeMap::new(),
            hatch_params: HatchParams {
                spacing: DEFAULT_HATCH_SPACING,
                width: DEFAULT_HATCH_WIDTH,
            },
            hatch_style: DEFAULT_HATCH_STYLE_PRESET,
            layer_hatch_styles: BTreeMap::new(),
            tile_cache_capacity: DEFAULT_TILE_CACHE_CAPACITY,
            scene_revision: 0,
            cache_access_tick: 0,
            total_cache_evictions: 0,
            cached_scene_key: None,
            tile_cache_domain: None,
            tile_vertex_cache: HashMap::new(),
            tile_display_batch_cache: HashMap::new(),
            visible_tile_keys: Vec::new(),
            visible_tiles_to_draw: Vec::new(),
            requested_tile_keys: Vec::new(),
            pending_tile_builds: VecDeque::new(),
            active_progressive_layer: None,
            active_display_layer: None,
            active_display_budget: 0,
            display_zoom_basis: 1.0,
            view_revision: 0,
            progressive_build_budget: DEFAULT_PROGRESSIVE_BUILD_BUDGET,
            progressive_bypass_threshold: DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD,
            layer_bypass_entry_threshold: DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD,
            layer_bypass_work_threshold: DEFAULT_LAYER_BYPASS_WORK_THRESHOLD,
            total_stale_drops: 0,
            shape_query_stats: CachedShapeQueryStats::default(),
            debug_stats: RenderDebugStats::default(),
        })
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }

    /// 获取调试统计信息。
    pub fn debug_stats(&self) -> RenderDebugStats {
        self.debug_stats
    }

    /// 当前 tile grid 密度。
    pub fn tile_grid_divisions(&self) -> u32 {
        self.tile_grid_divisions
    }

    /// 当前闭合图形显示模式。
    pub fn draw_mode(&self) -> ClosedShapeDrawMode {
        self.draw_mode
    }

    /// 当前 hatch 参数。
    pub fn hatch_params(&self) -> HatchParams {
        self.hatch_params
    }

    /// 当前全局 hatch preset。
    pub fn hatch_style(&self) -> HatchStylePreset {
        self.hatch_style
    }

    /// 当前每层显示模式覆盖。
    pub fn layer_draw_modes(&self) -> &BTreeMap<LayerId, ClosedShapeDrawMode> {
        &self.layer_draw_modes
    }

    /// 当前每层 hatch preset 覆盖。
    pub fn layer_hatch_styles(&self) -> &BTreeMap<LayerId, HatchStylePreset> {
        &self.layer_hatch_styles
    }

    /// 当前 tile cache 容量上限。
    pub fn tile_cache_capacity(&self) -> usize {
        self.tile_cache_capacity
    }

    /// 当前渐进式构建预算。
    pub fn progressive_build_budget(&self) -> usize {
        self.progressive_build_budget
    }

    /// 当前“轻场景直接补完”的阈值。
    pub fn progressive_bypass_threshold(&self) -> usize {
        self.progressive_bypass_threshold
    }

    /// 当前“轻 layer 直接整层补完”的条目阈值。
    pub fn layer_bypass_entry_threshold(&self) -> usize {
        self.layer_bypass_entry_threshold
    }

    /// 当前“轻 layer 直接整层补完”的工作量阈值。
    pub fn layer_bypass_work_threshold(&self) -> usize {
        self.layer_bypass_work_threshold
    }

    /// 调整 tile grid 密度，并正确失效已有缓存。
    pub fn set_tile_grid_divisions(&mut self, divisions: u32) {
        if self.tile_grid_divisions == divisions {
            return;
        }

        self.tile_grid_divisions = divisions;
        self.tile_grid = TileGridIndex::build_with_divisions(&self.scene, self.tile_grid_divisions);
        self.rebuild_prepared_tile_fragments();
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }


    /// 切换闭合图形显示模式。
    ///
    /// 这个模式会直接影响 tile buffer 内容，
    /// 所以切换后必须让 tile cache 整体失效。
    pub fn set_draw_mode(&mut self, draw_mode: ClosedShapeDrawMode) {
        if self.draw_mode == draw_mode {
            return;
        }

        self.draw_mode = draw_mode;
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }

    /// 调整每层的显示模式覆盖。
    ///
    /// 这是当前查看器里非常实用的一步：
    /// 允许我们把大包层收成 `Outline`，而保留真正关心层的 hatch。
    pub fn set_layer_draw_modes(
        &mut self,
        layer_draw_modes: BTreeMap<LayerId, ClosedShapeDrawMode>,
    ) {
        if self.layer_draw_modes == layer_draw_modes {
            return;
        }

        self.layer_draw_modes = layer_draw_modes;
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }

    /// 调整 hatch 全局参数。
    ///
    /// hatch 图案是在 fragment shader 中按屏幕坐标生成的，
    /// 但 spacing / width 仍然会影响最终视觉结果，所以也必须纳入缓存域。
    pub fn set_hatch_params(&mut self, params: HatchParams) {
        let params = params.normalized();
        if self.hatch_params == params {
            return;
        }

        self.hatch_params = params;
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }

    /// 切换全局 hatch preset。
    ///
    /// 虽然几何三角形拓扑不变，但每个 fill 顶点里编码的 shader 语义会变，
    /// 所以这里同样必须让 tile cache 失效，避免旧 buffer 带着过期 preset 继续被画出来。
    pub fn set_hatch_style(&mut self, hatch_style: HatchStylePreset) {
        if self.hatch_style == hatch_style {
            return;
        }

        self.hatch_style = hatch_style;
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }

    /// 调整每层的 hatch preset 覆盖。
    ///
    /// 这和 `layer_draw_modes` 的角色类似：
    /// 全局给一个默认风格，但允许某些关注层用更醒目的交叉线或点阵。
    pub fn set_layer_hatch_styles(
        &mut self,
        layer_hatch_styles: BTreeMap<LayerId, HatchStylePreset>,
    ) {
        if self.layer_hatch_styles == layer_hatch_styles {
            return;
        }

        self.layer_hatch_styles = layer_hatch_styles;
        self.invalidate_progressive_state(true);
        self.debug_stats.cache_hit = false;
    }

    /// 调整 tile cache 容量上限。
    pub fn set_tile_cache_capacity(&mut self, capacity: usize) {
        let capacity = capacity.clamp(MIN_TILE_CACHE_CAPACITY, MAX_TILE_CACHE_CAPACITY);
        if self.tile_cache_capacity == capacity {
            return;
        }

        self.tile_cache_capacity = capacity;
        let evicted = prune_tile_cache(&mut self.tile_vertex_cache, capacity, &std::collections::HashSet::new());
        self.total_cache_evictions += evicted;
        self.cached_scene_key = None;
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();
        self.visible_tiles_to_draw.clear();
        self.pending_tile_builds.clear();
        self.debug_stats.cache_hit = false;
    }

    /// 调整“轻场景直接补完”的阈值。
    pub fn set_progressive_bypass_threshold(&mut self, threshold: usize) {
        let threshold = threshold.clamp(MIN_PROGRESSIVE_BYPASS_THRESHOLD, MAX_PROGRESSIVE_BYPASS_THRESHOLD);
        if self.progressive_bypass_threshold == threshold {
            return;
        }

        self.progressive_bypass_threshold = threshold;
        self.cached_scene_key = None;
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();
        self.visible_tiles_to_draw.clear();
        self.pending_tile_builds.clear();
        self.active_progressive_layer = None;
        self.debug_stats.cache_hit = false;
    }

    /// 同时调整按 layer 的双阈值 bypass 条件。
    pub fn set_layer_bypass_thresholds(&mut self, entry_threshold: usize, work_threshold: usize) {
        let entry_threshold = entry_threshold.clamp(MIN_LAYER_BYPASS_ENTRY_THRESHOLD, MAX_LAYER_BYPASS_ENTRY_THRESHOLD);
        let work_threshold = work_threshold.clamp(MIN_LAYER_BYPASS_WORK_THRESHOLD, MAX_LAYER_BYPASS_WORK_THRESHOLD);
        if self.layer_bypass_entry_threshold == entry_threshold
            && self.layer_bypass_work_threshold == work_threshold
        {
            return;
        }

        self.layer_bypass_entry_threshold = entry_threshold;
        self.layer_bypass_work_threshold = work_threshold;
        self.cached_scene_key = None;
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();
        self.visible_tiles_to_draw.clear();
        self.pending_tile_builds.clear();
        self.active_progressive_layer = None;
        self.debug_stats.cache_hit = false;
    }

    /// 处理 surface resize。
    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            self.size = size;
            return;
        }
        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.cached_scene_key = None;
    }

    /// 用新场景替换旧场景，并重建相关索引。
    pub fn update_scene(&mut self, scene: &Scene) {
        self.scene = scene.clone();
        self.spatial_index = ShapeSpatialIndex::build(&self.scene);
        self.tile_grid = TileGridIndex::build_with_divisions(&self.scene, self.tile_grid_divisions);
        self.rebuild_prepared_tile_fragments();
        self.scene_revision = self.scene_revision.wrapping_add(1);
        self.layer_draw_modes
            .retain(|layer, _| self.scene.layer_ids().contains(layer));
        self.layer_hatch_styles
            .retain(|layer, _| self.scene.layer_ids().contains(layer));
        self.shape_query_stats = CachedShapeQueryStats::default();
        self.invalidate_progressive_state(true);
        self.debug_stats = RenderDebugStats::new(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, self.tile_cache_capacity, 0, self.total_cache_evictions, 0, 0, 0, 0, self.progressive_build_budget, self.total_stale_drops, None, 0, 0, None, false, self.layer_bypass_entry_threshold, self.layer_bypass_work_threshold, false, self.hatch_params.spacing, self.hatch_params.width);
    }

    /// 当 scene 或 tile grid 变化时，预先把超大 shape 切成按 tile 组织的世界坐标碎片。
    ///
    /// 这一步只做"几何准备"，不掺杂当前 zoom / hidden layers / draw mode。
    /// 这样后续缓存域变化时，可以在更小的范围内复用这些准备结果。
    fn rebuild_prepared_tile_fragments(&mut self) {
        self.prepared_tile_fragments = prepare_large_shape_tile_fragments(
            &self.scene,
            &self.tile_grid,
            LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD,
        );
    }

    /// 执行一帧完整渲染。
    ///
    /// 顺序是：
    /// 1. 更新 egui 纹理和 buffers
    /// 2. 计算当前 viewport 的可见场景缓存
    /// 3. 画 wgpu scene pass
    /// 4. 叠加 egui pass
    pub fn render(
        &mut self,
        camera: &Camera2D,
        hidden_layers: &BTreeSet<LayerId>,
        canvas_origin: Vec2,
        canvas_size: Vec2,
        pixels_per_point: f32,
        interaction_degraded: bool,
        egui_ctx: &egui::Context,
        full_output: egui::FullOutput,
        window: &Window,
    ) -> Result<(), wgpu::SurfaceError> {
        if self.config.width == 0 || self.config.height == 0 {
            return Ok(());
        }

        let surface_texture = self.surface.get_current_texture()?;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        let clipped_primitives =
            egui_ctx.tessellate(full_output.shapes, egui_ctx.pixels_per_point());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("main-encoder"),
            });
        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: window.scale_factor() as f32,
        };
        let callback_cmds = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        // 这里先把 surface 的物理像素尺寸转换回逻辑 viewport。
        // camera / ui / 几何投影链都在逻辑坐标里工作，最后一步再换回物理像素做 scissor。
        let viewport_size = logical_viewport_size(
            Vec2::new(self.config.width as f32, self.config.height as f32),
            pixels_per_point,
        );
        // 交互冻结的目标不是“停止渲染”，而是：
        // - 拖拽/缩放过程中不要为每个中间状态都重建 scene cache
        // - 先复用上一帧已经稳定的视图
        // - 用 shader 里的位置缩放让画面跟手
        let freeze_interaction_view = should_freeze_interaction_view(
            interaction_degraded,
            self.cached_scene_key.is_some() && !self.visible_tiles_to_draw.is_empty(),
        );
        if !freeze_interaction_view {
            self.update_scene_cache(
                camera,
                hidden_layers,
                canvas_origin,
                canvas_size,
                viewport_size,
                self.draw_mode,
                self.hatch_params,
                self.hatch_style,
                interaction_degraded,
            );
            self.display_zoom_basis = camera.zoom();
        }

        // 这里把“画布位置 + 相机平移”放进 uniform，
        // 这样 tile 顶点缓存就不用因为平移而重建。
        //
        // 交互冻结时还会额外设置 `position_scale`：
        // - cache 仍然复用旧 zoom 下的稳定视图
        // - shader 临时乘一个缩放比，让用户先看到平滑跟手的中间状态
        // - 停下来后再真正重算当前视图
        let scene_uniform = SceneUniform {
            translation: (camera.pan() + canvas_origin).to_array(),
            viewport_size: viewport_size.to_array(),
            hatch_spacing: self.hatch_params.spacing,
            hatch_width: self.hatch_params.width,
            suppress_fill: if interaction_degraded { 1.0 } else { 0.0 },
            position_scale: if freeze_interaction_view {
                camera.zoom() / self.display_zoom_basis.max(f32::EPSILON)
            } else {
                1.0
            },
            _padding: [0.0; 4],
        };
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("scene-uniform-buffer"),
                contents: bytemuck::bytes_of(&scene_uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let scene_bind_group = self
            .scene_pipeline
            .create_bind_group(&self.device, &uniform_buffer);

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.03,
                            g: 0.04,
                            b: 0.05,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // 这里的 scissor 只允许 scene 渲染到中央画布，
            // 避免 GPU 图元盖到左侧 egui 面板上。
            let canvas_scissor = ScissorRect::from_logical_rect(
                canvas_origin,
                canvas_size,
                pixels_per_point,
                self.config.width,
                self.config.height,
            )
            .expect("canvas scissor");
            render_pass.set_pipeline(self.scene_pipeline.render_pipeline());
            render_pass.set_bind_group(0, &scene_bind_group, &[]);
            let translation = camera.pan() + canvas_origin;
            for tile_id in &self.visible_tiles_to_draw {
                if let Some(entry) = self.tile_display_batch_cache.get(tile_id) {
                    let tile_bounds = self.tile_grid.tile_bounds(*tile_id);
                    if let Some(tile_scissor) = tile_scissor_rect(
                        tile_bounds,
                        camera.zoom(),
                        translation,
                        pixels_per_point,
                        self.config.width,
                        self.config.height,
                    )
                    .and_then(|tile_scissor| tile_scissor.intersect(canvas_scissor))
                    {
                        render_pass.set_scissor_rect(
                            tile_scissor.x,
                            tile_scissor.y,
                            tile_scissor.width,
                            tile_scissor.height,
                        );
                        render_pass.set_vertex_buffer(0, entry.vertex_buffer.slice(..));
                        render_pass.draw(0..entry.vertex_count, 0..1);
                    }
                }
            }
        }

        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            let mut ui_pass = render_pass.forget_lifetime();
            self.egui_renderer
                .render(&mut ui_pass, &clipped_primitives, &screen_descriptor);
        }

        self.queue.submit(callback_cmds);
        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        Ok(())
    }


/// 更新当前视口对应的 scene cache / tile cache / 调试统计。
fn update_scene_cache(
    &mut self,
    camera: &Camera2D,
    hidden_layers: &BTreeSet<LayerId>,
    canvas_origin: Vec2,
    canvas_size: Vec2,
    viewport_size: Vec2,
    draw_mode: ClosedShapeDrawMode,
    hatch_params: HatchParams,
    hatch_style: HatchStylePreset,
    interaction_degraded: bool,
) {
    let _ = interaction_degraded;
    let effective_layer_draw_modes = self.layer_draw_modes.clone();
    let effective_draw_mode = draw_mode;

    let key = build_render_cache_key_with_hatch_styles(
        self.scene_revision,
        camera,
        hidden_layers,
        &effective_layer_draw_modes,
        &self.layer_hatch_styles,
        canvas_origin,
        canvas_size,
        viewport_size,
        effective_draw_mode,
        hatch_params,
        hatch_style,
    );

    let domain = TileCacheDomainKey {
        scene_revision: self.scene_revision,
        zoom_bits: camera.zoom().to_bits(),
        hatch_signature: build_hatch_signature(hatch_params),
        hatch_style_signature: build_hatch_style_signature(hatch_style)
            ^ layer_hatch_style_hash_value(&self.layer_hatch_styles),
    };
    if self.tile_cache_domain != Some(domain) {
        self.tile_vertex_cache.clear();
        self.tile_display_batch_cache.clear();
        self.tile_cache_domain = Some(domain);
    }

    let mut dropped_this_frame = 0usize;
    if self.cached_scene_key != Some(key) {
        self.view_revision = self.view_revision.wrapping_add(1);
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();

        let visible_world = camera_visible_world_bounds(camera, viewport_size);
        let visible_world_center = Vec2::new(
            (visible_world.min_x + visible_world.max_x) * 0.5,
            (visible_world.min_y + visible_world.max_y) * 0.5,
        );
        let shape_query = query_visible_shapes(&self.scene, &self.spatial_index, visible_world);
        let visible_tiles = query_visible_tiles(&self.tile_grid, visible_world);
        self.shape_query_stats = CachedShapeQueryStats {
            candidate_shapes: shape_query.stats.candidate_shapes,
            visible_shapes: shape_query.stats.visible_shapes,
            bucket_hits: shape_query.stats.bucket_hits,
            visible_tiles: visible_tiles.len(),
        };

        let mut keys_by_layer: BTreeMap<LayerId, Vec<TileCacheKey>> = BTreeMap::new();
        for tile_id in visible_tiles.iter().copied() {
            let mut tile_layers: BTreeSet<LayerId> = self
                .tile_grid
                .layers_for_tile(tile_id)
                .iter()
                .copied()
                .filter(|layer| !hidden_layers.contains(layer))
                .filter(|layer| {
                    self.tile_grid
                        .shape_indices_for_tile_layer(tile_id, *layer)
                        .iter()
                        .any(|shape_index| !self.prepared_tile_fragments.shape_indices.contains(shape_index))
                })
                .collect();
            tile_layers.extend(
                self.prepared_tile_fragments
                    .layers_for_tile(tile_id)
                    .iter()
                    .copied()
                    .filter(|layer| !hidden_layers.contains(layer)),
            );

            for layer in tile_layers {
                let effective_mode = effective_layer_draw_modes
                    .get(&layer)
                    .copied()
                    .unwrap_or(effective_draw_mode);
                let effective_hatch_style = self
                    .layer_hatch_styles
                    .get(&layer)
                    .copied()
                    .unwrap_or(hatch_style);
                keys_by_layer.entry(layer).or_default().push(TileCacheKey {
                    scene_revision: self.scene_revision,
                    zoom_bits: camera.zoom().to_bits(),
                    tile_id,
                    layer,
                    effective_mode_tag: effective_mode.as_tag(),
                    hatch_signature: build_hatch_signature(hatch_params),
                    effective_hatch_style_tag: effective_hatch_style.as_tag(),
                });
            }
        }
        self.requested_tile_keys = flatten_layer_major_requested_tile_keys(
            keys_by_layer,
            &self.scene.layer_ids(),
            &self.tile_grid,
            visible_world_center,
        );
        let (pending, dropped) = refresh_progressive_queue(
            self.view_revision,
            std::mem::take(&mut self.pending_tile_builds),
            &self.requested_tile_keys,
        );
        dropped_this_frame = dropped;
        self.total_stale_drops += dropped;
        self.pending_tile_builds = pending;
        self.active_progressive_layer = None;
        self.active_display_layer = None;
        self.active_display_budget = 0;
        self.cached_scene_key = Some(key);
    }

    let active_layer = next_active_progressive_layer(
        self.active_progressive_layer,
        &self.pending_tile_builds,
    );
    let active_layer_stats = active_layer
        .map(|layer| estimate_active_layer_pending_stats(
            layer,
            &self.pending_tile_builds,
            &self.prepared_tile_fragments,
            &self.tile_grid,
        ))
        .unwrap_or_default();
    let layer_bypass = LayerAdaptiveBypassConfig {
        entry_threshold: self.layer_bypass_entry_threshold,
        work_threshold: self.layer_bypass_work_threshold,
    };
    let (global_build_budget, progressive_bypassed) = compute_effective_build_budget(
        self.pending_tile_builds.len(),
        self.progressive_build_budget,
        self.progressive_bypass_threshold,
    );
    let effective_build_budget = if progressive_bypassed {
        global_build_budget
    } else {
        effective_build_budget_for_active_layer(
            self.pending_tile_builds.len(),
            active_layer,
            active_layer_stats,
            self.progressive_build_budget,
            layer_bypass,
        )
    };
    self.process_pending_tile_builds(
        camera.zoom(),
        effective_draw_mode,
        &effective_layer_draw_modes,
        hatch_style,
        effective_build_budget,
    );

    let displayed_active_layer = next_active_progressive_layer(
        self.active_progressive_layer,
        &self.pending_tile_builds,
    );
    let displayed_active_layer_stats = estimate_active_layer_pending_stats_for_option(
        displayed_active_layer,
        &self.pending_tile_builds,
        &self.prepared_tile_fragments,
        &self.tile_grid,
    );
    let displayed_active_layer_progress_mode = displayed_active_layer.map(|_| {
        if progressive_bypassed
            || should_bypass_progressive_for_layer(displayed_active_layer_stats, layer_bypass)
        {
            ActiveLayerProgressMode::Bypassed
        } else {
            ActiveLayerProgressMode::Progressive
        }
    });
    advance_active_display_budget(
        &mut self.active_display_layer,
        &mut self.active_display_budget,
        displayed_active_layer,
        progressive_bypassed,
        displayed_active_layer_progress_mode,
        effective_build_budget,
    );
    let revealed_layers = if progressive_bypassed {
        requested_layers_in_order(&self.requested_tile_keys)
            .into_iter()
            .collect()
    } else {
        compute_revealed_layers_for_display(
            &self.requested_tile_keys,
            &self.pending_tile_builds,
            displayed_active_layer,
        )
    };

    let mut tile_cache_hits = 0usize;
    let mut tile_cache_misses = 0usize;
    let mut layer_cache_hits = 0usize;
    let mut layer_cache_misses = 0usize;
    let mut total_vertices = 0usize;
    let mut visible_tile_keys = Vec::new();
    let mut tile_state: HashMap<TileId, (bool, bool)> = HashMap::new();
    let mut active_layer_visible_count = 0usize;
    let mut active_layer_cached_count = 0usize;

    for tile_key in self.requested_tile_keys.clone() {
        let state = tile_state.entry(tile_key.tile_id).or_insert((false, false));
        if let Some(entry) = self.tile_vertex_cache.get(&tile_key) {
            state.0 = true;
            layer_cache_hits += 1;
            let allow_layer = if progressive_bypassed {
                true
            } else if Some(tile_key.layer) == displayed_active_layer {
                active_layer_cached_count += 1;
                if self.active_display_budget == usize::MAX {
                    true
                } else {
                    active_layer_visible_count < self.active_display_budget
                }
            } else {
                revealed_layers.contains(&tile_key.layer)
            };
            if allow_layer {
                if Some(tile_key.layer) == displayed_active_layer {
                    active_layer_visible_count += 1;
                }
                total_vertices += entry.vertex_count as usize;
                visible_tile_keys.push(tile_key);
            }
        } else {
            state.1 = true;
            layer_cache_misses += 1;
            self.enqueue_pending_tile_build(tile_key);
        }
    }

    for (_, (had_hit, had_miss)) in tile_state {
        if had_hit || had_miss {
            if had_miss {
                tile_cache_misses += 1;
            } else {
                tile_cache_hits += 1;
            }
        }
    }

    let protected_tiles: HashSet<_> = visible_tile_keys.iter().copied().collect();
    let evicted = prune_tile_cache(
        &mut self.tile_vertex_cache,
        self.tile_cache_capacity,
        &protected_tiles,
    );
    self.total_cache_evictions += evicted;

    let mut visible_tile_groups: BTreeMap<TileId, Vec<TileCacheKey>> = BTreeMap::new();
    for tile_key in visible_tile_keys.iter().copied() {
        visible_tile_groups.entry(tile_key.tile_id).or_default().push(tile_key);
    }

    let mut visible_tiles_to_draw = Vec::new();
    let visible_tile_ids: HashSet<_> = visible_tile_groups.keys().copied().collect();
    self.tile_display_batch_cache
        .retain(|tile_id, _| visible_tile_ids.contains(tile_id));

    for (tile_id, tile_keys) in &visible_tile_groups {
        let signature = build_tile_display_batch_signature(tile_keys);
        let needs_rebuild = self
            .tile_display_batch_cache
            .get(tile_id)
            .map(|entry| entry.signature != signature)
            .unwrap_or(true);
        if needs_rebuild {
            if let Some(entry) = create_tile_display_batch_entry(
                &self.device,
                tile_keys,
                &self.tile_vertex_cache,
            ) {
                self.tile_display_batch_cache.insert(*tile_id, entry);
            } else {
                self.tile_display_batch_cache.remove(tile_id);
            }
        }
        if self.tile_display_batch_cache.contains_key(tile_id) {
            visible_tiles_to_draw.push(*tile_id);
        }
    }

    let cache_bytes: usize = self.tile_vertex_cache.values().map(|entry| entry.byte_size).sum::<usize>()
        + self.tile_display_batch_cache.values().map(|entry| entry.byte_size).sum::<usize>();

    self.visible_tile_keys = visible_tile_keys;
    self.visible_tiles_to_draw = visible_tiles_to_draw;
    self.debug_stats = RenderDebugStats::new(
        self.scene.shapes().len(),
        self.shape_query_stats.candidate_shapes,
        self.shape_query_stats.visible_shapes,
        self.shape_query_stats.bucket_hits,
        total_vertices,
        self.visible_tiles_to_draw.len(),
        self.shape_query_stats.visible_tiles,
        tile_cache_hits,
        tile_cache_misses,
        layer_cache_hits,
        layer_cache_misses,
        self.tile_vertex_cache.len(),
        self.tile_cache_capacity,
        cache_bytes,
        self.total_cache_evictions,
        self.prepared_tile_fragments.prepared_shape_count(),
        self.prepared_tile_fragments.prepared_tile_count(),
        self.prepared_tile_fragments.prepared_fragment_count(),
        self.pending_tile_builds.len()
            + active_layer_cached_count.saturating_sub(active_layer_visible_count),
        effective_build_budget,
        self.total_stale_drops,
        displayed_active_layer,
        active_layer_pending_count(displayed_active_layer, &self.pending_tile_builds),
        displayed_active_layer_stats.estimated_work_units,
        displayed_active_layer_progress_mode,
        progressive_bypassed,
        self.layer_bypass_entry_threshold,
        self.layer_bypass_work_threshold,
        false,
        self.hatch_params.spacing,
        self.hatch_params.width,
    );
    // 视图 key 没变但还有待构建队列时，说明这一帧命中了“渐进补全”路径。
    // 这里故意把 `cache_hit` 定义成“这一帧完全没有额外准备工作”，
    // 这样 UI 上更容易一眼看出 viewer 是否已经稳定下来。
    self.debug_stats.cache_hit = self.cached_scene_key == Some(key)
        && self.pending_tile_builds.is_empty()
        && layer_cache_misses == 0
        && dropped_this_frame == 0;
}

/// 清理与当前视图相关的一切渐进式状态。
fn invalidate_progressive_state(&mut self, clear_gpu_cache: bool) {
    self.cached_scene_key = None;
    self.requested_tile_keys.clear();
    self.visible_tile_keys.clear();
    self.pending_tile_builds.clear();
    self.active_progressive_layer = None;
    self.shape_query_stats = CachedShapeQueryStats::default();
    self.view_revision = self.view_revision.wrapping_add(1);
    if clear_gpu_cache {
        self.tile_cache_domain = None;
        self.tile_vertex_cache.clear();
        self.tile_display_batch_cache.clear();
    }
}

/// 把当前视图仍然缺失的 `tile + layer` 键放进待构建队列。
fn enqueue_pending_tile_build(&mut self, tile_key: TileCacheKey) {
    if self.tile_vertex_cache.contains_key(&tile_key)
        || self
            .pending_tile_builds
            .iter()
            .any(|pending| pending.tile_key == tile_key)
    {
        return;
    }
    self.pending_tile_builds = enqueue_unique_pending(
        std::mem::take(&mut self.pending_tile_builds),
        PendingTileBuild {
            view_revision: self.view_revision,
            tile_key,
        },
    );
}

/// 每帧只消化固定预算的 pending 条目。
///
/// 这里采用“layer-complete-first”策略：
/// 当前活动 layer 没补满之前，不切到下一 layer。
fn process_pending_tile_builds(
    &mut self,
    zoom: f32,
    draw_mode: ClosedShapeDrawMode,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    hatch_style: HatchStylePreset,
    build_budget: usize,
) {
    self.cache_access_tick = self.cache_access_tick.wrapping_add(1);
    let current_tick = self.cache_access_tick;
    let mut built = 0usize;

    while built < build_budget {
        let Some(active_layer) = next_active_progressive_layer(
            self.active_progressive_layer,
            &self.pending_tile_builds,
        ) else {
            self.active_progressive_layer = None;
            break;
        };
        self.active_progressive_layer = Some(active_layer);

        let Some(pending) = pop_next_pending_for_layer(&mut self.pending_tile_builds, active_layer) else {
            self.active_progressive_layer = None;
            break;
        };
        if pending.view_revision != self.view_revision {
            self.total_stale_drops += 1;
            continue;
        }
        if self.tile_vertex_cache.contains_key(&pending.tile_key) {
            continue;
        }
        if let Some(entry) = self.build_tile_cache_entry_for_key(
            pending.tile_key,
            zoom,
            draw_mode,
            layer_draw_modes,
            hatch_style,
            current_tick,
        ) {
            self.tile_vertex_cache.insert(pending.tile_key, entry);
        }
        built += 1;

        if !self.pending_tile_builds.iter().any(|item| item.tile_key.layer == active_layer) {
            self.active_progressive_layer = None;
        }
    }
}

/// 为一个指定的 `tile + layer` 生成 GPU 顶点缓存条目。
fn build_tile_cache_entry_for_key(
    &self,
    tile_key: TileCacheKey,
    zoom: f32,
    draw_mode: ClosedShapeDrawMode,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    hatch_style: HatchStylePreset,
    current_tick: u64,
) -> Option<TileCacheEntry> {
    let shape_indices: Vec<_> = self
        .tile_grid
        .shape_indices_for_tile_layer(tile_key.tile_id, tile_key.layer)
        .iter()
        .copied()
        .filter(|shape_index| !self.prepared_tile_fragments.shape_indices.contains(shape_index))
        .collect();
    let empty_hidden_layers = BTreeSet::new();
    let mut vertices = build_scaled_scene_vertices_for_tile(
        &self.scene,
        zoom,
        &empty_hidden_layers,
        layer_draw_modes,
        &shape_indices,
        draw_mode,
        Some(self.tile_grid.tile_bounds(tile_key.tile_id)),
        &self.layer_hatch_styles,
        hatch_style,
    );
    if let Some(prepared_fragments) = self
        .prepared_tile_fragments
        .fragments_for_tile_layer(tile_key.tile_id, tile_key.layer)
    {
        vertices.extend(build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles(
            prepared_fragments,
            zoom,
            &empty_hidden_layers,
            layer_draw_modes,
            &self.layer_hatch_styles,
            draw_mode,
            hatch_style,
        ));
    }
    create_tile_cache_entry(&self.device, &vertices, current_tick)
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScissorRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl ScissorRect {
    fn from_logical_rect(
        origin: Vec2,
        size: Vec2,
        pixels_per_point: f32,
        surface_width: u32,
        surface_height: u32,
    ) -> Option<Self> {
        let x = (origin.x * pixels_per_point).floor();
        let y = (origin.y * pixels_per_point).floor();
        let max_x = ((origin.x + size.x) * pixels_per_point).ceil();
        let max_y = ((origin.y + size.y) * pixels_per_point).ceil();
        scissor_from_edges(x, y, max_x, max_y, surface_width, surface_height)
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.x.saturating_add(self.width).min(other.x.saturating_add(other.width));
        let y1 = self.y.saturating_add(self.height).min(other.y.saturating_add(other.height));
        if x1 > x0 && y1 > y0 {
            Some(Self {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            })
        } else {
            None
        }
    }
}

fn scissor_from_edges(
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    surface_width: u32,
    surface_height: u32,
) -> Option<ScissorRect> {
    let clamped_min_x = min_x.max(0.0).min(surface_width as f32);
    let clamped_min_y = min_y.max(0.0).min(surface_height as f32);
    let clamped_max_x = max_x.max(0.0).min(surface_width as f32);
    let clamped_max_y = max_y.max(0.0).min(surface_height as f32);
    let width = (clamped_max_x - clamped_min_x).max(0.0).round() as u32;
    let height = (clamped_max_y - clamped_min_y).max(0.0).round() as u32;
    (width > 0 && height > 0).then_some(ScissorRect {
        x: clamped_min_x.round() as u32,
        y: clamped_min_y.round() as u32,
        width,
        height,
    })
}

/// 根据 tile 的世界坐标范围，计算它在当前帧应使用的 scissor 矩形。
///
/// 这样即使同一个 shape 同时命中多个 tile buffer，
/// 也只会在各自负责的屏幕小块里被画一次，不会整块重复叠画。
fn tile_scissor_rect(
    tile_bounds: crate::scene::Bounds,
    zoom: f32,
    translation: Vec2,
    pixels_per_point: f32,
    surface_width: u32,
    surface_height: u32,
) -> Option<ScissorRect> {
    let min = Vec2::new(tile_bounds.min_x, tile_bounds.min_y) * zoom + translation;
    let max = Vec2::new(tile_bounds.max_x, tile_bounds.max_y) * zoom + translation;
    ScissorRect::from_logical_rect(min, max - min, pixels_per_point, surface_width, surface_height)
}

/// 为一个 tile 创建 GPU vertex buffer。
fn create_tile_cache_entry(
    device: &wgpu::Device,
    vertices: &[LineVertex],
    last_used_tick: u64,
) -> Option<TileCacheEntry> {
    let _ = device;
    (!vertices.is_empty()).then(|| TileCacheEntry {
        cpu_vertices: Arc::from(vertices.to_vec().into_boxed_slice()),
        vertex_count: vertices.len() as u32,
        byte_size: std::mem::size_of_val(vertices),
        last_used_tick,
    })
}

fn should_freeze_interaction_view(interaction_degraded: bool, has_stable_view: bool) -> bool {
    interaction_degraded && has_stable_view
}

fn build_tile_display_batch_signature(tile_keys: &[TileCacheKey]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tile_keys.hash(&mut hasher);
    hasher.finish()
}

fn create_tile_display_batch_entry(
    device: &wgpu::Device,
    tile_keys: &[TileCacheKey],
    tile_vertex_cache: &HashMap<TileCacheKey, TileCacheEntry>,
) -> Option<TileDisplayBatchEntry> {
    let mut vertices = Vec::new();
    for tile_key in tile_keys {
        if let Some(entry) = tile_vertex_cache.get(tile_key) {
            vertices.extend_from_slice(&entry.cpu_vertices);
        }
    }
    if vertices.is_empty() {
        return None;
    }

    Some(TileDisplayBatchEntry {
        signature: build_tile_display_batch_signature(tile_keys),
        vertex_buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tile-display-batch-buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        }),
        vertex_count: vertices.len() as u32,
        byte_size: std::mem::size_of_val(vertices.as_slice()),
    })
}



fn enqueue_unique_pending(
    mut pending: VecDeque<PendingTileBuild>,
    new_item: PendingTileBuild,
) -> VecDeque<PendingTileBuild> {
    if pending.iter().any(|item| item.tile_key == new_item.tile_key) {
        return pending;
    }
    pending.push_back(new_item);
    pending
}

fn sort_tile_keys_by_world_center(
    mut keys: Vec<TileCacheKey>,
    tile_grid: &TileGridIndex,
    center: Vec2,
) -> Vec<TileCacheKey> {
    keys.sort_by(|a, b| {
        let a_bounds = tile_grid.tile_bounds(a.tile_id);
        let b_bounds = tile_grid.tile_bounds(b.tile_id);
        let a_center = Vec2::new((a_bounds.min_x + a_bounds.max_x) * 0.5, (a_bounds.min_y + a_bounds.max_y) * 0.5);
        let b_center = Vec2::new((b_bounds.min_x + b_bounds.max_x) * 0.5, (b_bounds.min_y + b_bounds.max_y) * 0.5);
        let a_dist = a_center.distance_squared(center);
        let b_dist = b_center.distance_squared(center);
        a_dist.partial_cmp(&b_dist).unwrap_or(std::cmp::Ordering::Equal)
    });
    keys
}

fn flatten_layer_major_requested_tile_keys(
    mut keys_by_layer: BTreeMap<LayerId, Vec<TileCacheKey>>,
    scene_layers: &[LayerId],
    tile_grid: &TileGridIndex,
    visible_world_center: Vec2,
) -> Vec<TileCacheKey> {
    let mut ordered = Vec::new();

    // 这里刻意按 scene 的 layer 顺序来展开请求队列。
    // 同一层内部再按“离当前视口中心有多近”排序，
    // 这样用户最先看到的会是正在关注区域先稳定下来。
    for layer in scene_layers {
        if let Some(keys) = keys_by_layer.remove(layer) {
            ordered.extend(sort_tile_keys_by_world_center(keys, tile_grid, visible_world_center));
        }
    }

    for (_, keys) in keys_by_layer {
        ordered.extend(sort_tile_keys_by_world_center(keys, tile_grid, visible_world_center));
    }

    ordered
}

fn compute_effective_build_budget(
    pending_entries: usize,
    progressive_build_budget: usize,
    progressive_bypass_threshold: usize,
) -> (usize, bool) {
    if pending_entries <= progressive_bypass_threshold {
        return (pending_entries, pending_entries > 0);
    }
    (progressive_build_budget.min(pending_entries), false)
}

/// 计算当前活动 layer 在这一帧实际能拿到的构建预算。
///
/// 这里有一个很重要的分层思路：
/// - 全局 bypass：针对“整个当前视图都很轻”的情况
/// - layer bypass：针对“当前活动 layer 很轻，但整个视图未必轻”的情况
///
/// 所以这个函数只在“没有命中全局 bypass”时出场，
/// 它负责把预算进一步细化到当前活动 layer 这一层。
fn effective_build_budget_for_active_layer(
    pending_entries_total: usize,
    active_layer: Option<LayerId>,
    active_layer_stats: LayerPendingStats,
    progressive_build_budget: usize,
    bypass: LayerAdaptiveBypassConfig,
) -> usize {
    if pending_entries_total == 0 {
        return 0;
    }

    if active_layer.is_some() && should_bypass_progressive_for_layer(active_layer_stats, bypass) {
        return active_layer_stats.pending_entries;
    }

    progressive_build_budget.min(pending_entries_total)
}

/// 双阈值判断：只有“条目数小”且“估计工作量小”同时成立，
/// 当前 layer 才允许一帧补完。
///
/// 这样做是为了避免两类误判：
/// - 条目数不多，但每个 tile 都很重
/// - 工作量不大，但需要跨很多 tile 逐个建立条目
pub fn should_bypass_progressive_for_layer(
    stats: LayerPendingStats,
    config: LayerAdaptiveBypassConfig,
) -> bool {
    stats.pending_entries > 0
        && stats.pending_entries <= config.entry_threshold
        && stats.estimated_work_units <= config.work_threshold
}

/// 估算当前活动 layer 剩余工作的轻量成本。
///
/// 这不是精确 profiling，而是一个“稳定、可解释、足够便宜”的近似值：
/// - pending entries 代表还要建多少个 `tile + layer` 条目
/// - prepared fragments 代表超大 shape 预碎片化路径的工作量
/// - regular shapes 代表普通 shape 路径的工作量
///
/// 第一版先用固定权重加权，后面如果我们发现真实版图上还需要更细，
/// 再迭代这套估算公式会更安全。
fn estimate_active_layer_pending_stats(
    active_layer: LayerId,
    pending: &VecDeque<PendingTileBuild>,
    prepared: &PreparedTileFragments,
    tile_grid: &TileGridIndex,
) -> LayerPendingStats {
    let mut tile_ids = Vec::new();
    for item in pending.iter().filter(|item| item.tile_key.layer == active_layer) {
        tile_ids.push(item.tile_key.tile_id);
    }
    tile_ids.sort();
    tile_ids.dedup();

    let prepared_fragment_count: usize = tile_ids
        .iter()
        .map(|tile_id| prepared.fragments_for_tile_layer(*tile_id, active_layer).map(|f| f.len()).unwrap_or(0))
        .sum();

    let regular_shape_count: usize = tile_ids
        .iter()
        .map(|tile_id| {
            tile_grid
                .shape_indices_for_tile_layer(*tile_id, active_layer)
                .iter()
                .filter(|shape_index| !prepared.shape_indices.contains(shape_index))
                .count()
        })
        .sum();

    let pending_entries = pending.iter().filter(|item| item.tile_key.layer == active_layer).count();
    let estimated_work_units = pending_entries * LAYER_WORK_ENTRY_WEIGHT
        + prepared_fragment_count * LAYER_WORK_PREPARED_WEIGHT
        + regular_shape_count * LAYER_WORK_REGULAR_WEIGHT;

    LayerPendingStats {
        pending_entries,
        estimated_work_units,
        prepared_fragment_count,
        regular_shape_count,
    }
}

fn active_layer_pending_count(
    active_layer: Option<LayerId>,
    pending: &VecDeque<PendingTileBuild>,
) -> usize {
    let Some(layer) = active_layer else {
        return 0;
    };
    pending.iter().filter(|item| item.tile_key.layer == layer).count()
}

fn requested_layers_in_order(requested_keys: &[TileCacheKey]) -> Vec<LayerId> {
    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    for tile_key in requested_keys {
        if seen.insert(tile_key.layer) {
            ordered.push(tile_key.layer);
        }
    }
    ordered
}

fn compute_revealed_layers_for_display(
    requested_keys: &[TileCacheKey],
    pending: &VecDeque<PendingTileBuild>,
    active_layer: Option<LayerId>,
) -> BTreeSet<LayerId> {
    let pending_layers: HashSet<_> = pending.iter().map(|item| item.tile_key.layer).collect();
    let mut revealed = BTreeSet::new();

    // 这里的关键不是再改 cache，而是收紧“这一帧允许显示哪些 layer”。
    // 我们只放开一个连续前缀：
    // - 已经没有 pending 的旧 layer：完整显示
    // - 当前活动 layer：允许显示已完成部分
    // - 后续 layer：即使 cache 已经命中，也暂时先不显示
    for layer in requested_layers_in_order(requested_keys) {
        if Some(layer) == active_layer {
            revealed.insert(layer);
            break;
        }

        if pending_layers.contains(&layer) {
            break;
        }

        revealed.insert(layer);
    }

    revealed
}

fn advance_active_display_budget(
    active_display_layer: &mut Option<LayerId>,
    active_display_budget: &mut usize,
    active_layer: Option<LayerId>,
    progressive_bypassed: bool,
    active_layer_progress_mode: Option<ActiveLayerProgressMode>,
    effective_build_budget: usize,
) {
    if *active_display_layer != active_layer {
        *active_display_layer = active_layer;
        *active_display_budget = 0;
    }

    match active_layer_progress_mode {
        Some(ActiveLayerProgressMode::Bypassed) if progressive_bypassed => {
            *active_display_budget = usize::MAX;
        }
        Some(ActiveLayerProgressMode::Bypassed) => {
            *active_display_budget = usize::MAX;
        }
        Some(ActiveLayerProgressMode::Progressive) => {
            let step = effective_build_budget.max(1);
            *active_display_budget = active_display_budget.saturating_add(step);
        }
        None => {
            *active_display_budget = if progressive_bypassed { usize::MAX } else { 0 };
        }
    }
}

fn estimate_active_layer_pending_stats_for_option(
    active_layer: Option<LayerId>,
    pending: &VecDeque<PendingTileBuild>,
    prepared: &PreparedTileFragments,
    tile_grid: &TileGridIndex,
) -> LayerPendingStats {
    active_layer
        .map(|layer| estimate_active_layer_pending_stats(layer, pending, prepared, tile_grid))
        .unwrap_or_default()
}

fn next_active_progressive_layer(
    current: Option<LayerId>,
    pending: &VecDeque<PendingTileBuild>,
) -> Option<LayerId> {
    if let Some(layer) = current {
        if pending.iter().any(|item| item.tile_key.layer == layer) {
            return Some(layer);
        }
    }
    pending.front().map(|item| item.tile_key.layer)
}

fn pop_next_pending_for_layer(
    pending: &mut VecDeque<PendingTileBuild>,
    layer: LayerId,
) -> Option<PendingTileBuild> {
    let index = pending.iter().position(|item| item.tile_key.layer == layer)?;
    pending.remove(index)
}

fn refresh_progressive_queue(
    view_revision: u64,
    old_pending: VecDeque<PendingTileBuild>,
    requested_keys: &[TileCacheKey],
) -> (VecDeque<PendingTileBuild>, usize) {
    let dropped = old_pending.len();
    let pending = requested_keys
        .iter()
        .copied()
        .map(|tile_key| PendingTileBuild {
            view_revision,
            tile_key,
        })
        .collect();
    (pending, dropped)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use glam::Vec2;

    use crate::scene::LayerId;

    use std::collections::VecDeque;

    use super::{
        active_layer_pending_count, advance_active_display_budget,
        compute_effective_build_budget, compute_revealed_layers_for_display,
        effective_build_budget_for_active_layer, enqueue_unique_pending,
        next_active_progressive_layer, pop_next_pending_for_layer, refresh_progressive_queue,
        requested_layers_in_order, select_tile_eviction_victims,
        should_bypass_progressive_for_layer, should_freeze_interaction_view,
        tile_scissor_rect, ActiveLayerProgressMode,
        LayerAdaptiveBypassConfig, LayerPendingStats, PendingTileBuild, RenderDebugStats,
        TileCacheKey,
    };

    #[test]
    fn eviction_prefers_oldest_non_visible_tiles() {
        let mut usage = HashMap::new();
        let a = key(0, 1);
        let b = key(1, 5);
        let c = key(2, 3);
        usage.insert(a, 1);
        usage.insert(b, 5);
        usage.insert(c, 3);

        let protected = HashSet::from([b]);
        let victims = select_tile_eviction_victims(&usage, 2, &protected);

        assert_eq!(victims, vec![a]);
    }

    #[test]
    fn disjoint_scissor_intersection_returns_none_without_overflow() {
        let a = super::ScissorRect { x: 100, y: 100, width: 20, height: 20 };
        let b = super::ScissorRect { x: 10, y: 10, width: 5, height: 5 };

        assert_eq!(a.intersect(b), None);
        assert_eq!(b.intersect(a), None);
    }

    #[test]
    fn tile_scissor_rect_limits_draw_to_tile_region() {
        let tile = crate::scene::Bounds::new(0.0, 0.0, 50.0, 25.0);
        let rect = tile_scissor_rect(tile, 2.0, Vec2::new(10.0, 20.0), 1.0, 400, 300)
            .expect("tile scissor");

        assert_eq!(rect.x, 10);
        assert_eq!(rect.y, 20);
        assert_eq!(rect.width, 100);
        assert_eq!(rect.height, 50);
    }

    #[test]
    fn tile_cache_key_now_distinguishes_layers_and_modes_within_same_tile() {
        let tile = super::TileId { col: 0, row: 0 };
        let a = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: tile,
            layer: LayerId { layer: 1, datatype: 1 },
            effective_mode_tag: 0,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };
        let b = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: tile,
            layer: LayerId { layer: 1, datatype: 2 },
            effective_mode_tag: 0,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };
        let c = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: tile,
            layer: LayerId { layer: 1, datatype: 1 },
            effective_mode_tag: 2,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };

        assert_ne!(a, b);
        assert_ne!(a, c);
    }


#[test]
fn enqueue_skips_duplicate_tile_keys() {
    let pending = enqueue_unique_pending(
        VecDeque::from([
            PendingTileBuild {
                view_revision: 3,
                tile_key: key(0, 0),
            },
        ]),
        PendingTileBuild {
            view_revision: 3,
            tile_key: key(0, 0),
        },
    );

    assert_eq!(pending.len(), 1);
}

#[test]
fn refresh_progressive_queue_drops_stale_entries_on_new_view() {
    let old = VecDeque::from([
        PendingTileBuild {
            view_revision: 1,
            tile_key: key(0, 0),
        },
        PendingTileBuild {
            view_revision: 1,
            tile_key: key(1, 0),
        },
    ]);
    let requested = vec![key(2, 0), key(3, 0)];
    let (pending, dropped) = refresh_progressive_queue(7, old, &requested);

    assert_eq!(dropped, 2);
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|item| item.view_revision == 7));
    let cols: Vec<_> = pending.iter().map(|item| item.tile_key.tile_id.col).collect();
    assert_eq!(cols, vec![2, 3]);
}

    #[test]
    fn small_pending_sets_bypass_progressive_mode() {
        assert_eq!(compute_effective_build_budget(4, 16, 8), (4, true));
        assert_eq!(compute_effective_build_budget(0, 16, 8), (0, false));
        assert_eq!(compute_effective_build_budget(20, 16, 8), (16, false));
    }

    #[test]
    fn interaction_view_freezes_only_when_we_have_a_stable_view() {
        assert!(should_freeze_interaction_view(true, true));
        assert!(!should_freeze_interaction_view(true, false));
        assert!(!should_freeze_interaction_view(false, true));
    }

    #[test]
    fn active_display_budget_grows_progressively_for_same_layer() {
        let layer = LayerId { layer: 5, datatype: 0 };
        let mut active_display_layer = None;
        let mut active_display_budget = 0usize;

        advance_active_display_budget(
            &mut active_display_layer,
            &mut active_display_budget,
            Some(layer),
            false,
            Some(ActiveLayerProgressMode::Progressive),
            3,
        );
        advance_active_display_budget(
            &mut active_display_layer,
            &mut active_display_budget,
            Some(layer),
            false,
            Some(ActiveLayerProgressMode::Progressive),
            3,
        );

        assert_eq!(active_display_layer, Some(layer));
        assert_eq!(active_display_budget, 6);
    }

    #[test]
    fn bypassed_active_display_layer_reveals_all_entries_immediately() {
        let layer = LayerId { layer: 5, datatype: 0 };
        let mut active_display_layer = None;
        let mut active_display_budget = 0usize;

        advance_active_display_budget(
            &mut active_display_layer,
            &mut active_display_budget,
            Some(layer),
            false,
            Some(ActiveLayerProgressMode::Bypassed),
            2,
        );

        assert_eq!(active_display_layer, Some(layer));
        assert_eq!(active_display_budget, usize::MAX);
    }

    #[test]
    fn display_gate_reveals_only_completed_prefix_and_active_layer() {
        let layer_a = LayerId { layer: 1, datatype: 0 };
        let layer_b = LayerId { layer: 2, datatype: 0 };
        let layer_c = LayerId { layer: 3, datatype: 0 };
        let requested = vec![
            key_for_layer(0, layer_a),
            key_for_layer(1, layer_a),
            key_for_layer(2, layer_b),
            key_for_layer(3, layer_b),
            key_for_layer(4, layer_c),
        ];
        let pending = VecDeque::from([
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(2, layer_b) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(4, layer_c) },
        ]);

        let revealed = compute_revealed_layers_for_display(&requested, &pending, Some(layer_b));

        assert!(revealed.contains(&layer_a));
        assert!(revealed.contains(&layer_b));
        assert!(!revealed.contains(&layer_c));
    }

    #[test]
    fn requested_layers_keep_layer_major_order_without_duplicates() {
        let layer_a = LayerId { layer: 1, datatype: 0 };
        let layer_b = LayerId { layer: 2, datatype: 0 };
        let ordered = requested_layers_in_order(&[
            key_for_layer(0, layer_a),
            key_for_layer(1, layer_a),
            key_for_layer(2, layer_b),
            key_for_layer(3, layer_b),
        ]);

        assert_eq!(ordered, vec![layer_a, layer_b]);
    }

    #[test]
    fn next_active_layer_prefers_existing_layer_until_it_is_empty() {
        let layer_a = LayerId { layer: 1, datatype: 1 };
        let layer_b = LayerId { layer: 2, datatype: 0 };
        let pending = VecDeque::from([
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(0, layer_a) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(1, layer_a) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(2, layer_b) },
        ]);

        assert_eq!(next_active_progressive_layer(None, &pending), Some(layer_a));
        assert_eq!(next_active_progressive_layer(Some(layer_a), &pending), Some(layer_a));
    }

    #[test]
    fn active_layer_switches_after_previous_layer_finishes() {
        let layer_a = LayerId { layer: 1, datatype: 1 };
        let layer_b = LayerId { layer: 2, datatype: 0 };
        let mut pending = VecDeque::from([
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(0, layer_a) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(1, layer_b) },
        ]);

        let first = pop_next_pending_for_layer(&mut pending, layer_a).expect("first layer item");
        assert_eq!(first.tile_key.layer, layer_a);
        assert_eq!(next_active_progressive_layer(Some(layer_a), &pending), Some(layer_b));
        assert_eq!(active_layer_pending_count(Some(layer_b), &pending), 1);
    }


    #[test]
    fn small_active_layer_with_small_work_is_bypassed() {
        let stats = LayerPendingStats {
            pending_entries: 4,
            estimated_work_units: 32,
            prepared_fragment_count: 2,
            regular_shape_count: 6,
        };

        assert!(should_bypass_progressive_for_layer(
            stats,
            LayerAdaptiveBypassConfig {
                entry_threshold: 8,
                work_threshold: 64,
            },
        ));
    }

    #[test]
    fn small_active_layer_with_large_work_stays_progressive() {
        let stats = LayerPendingStats {
            pending_entries: 4,
            estimated_work_units: 160,
            prepared_fragment_count: 20,
            regular_shape_count: 10,
        };

        assert!(!should_bypass_progressive_for_layer(
            stats,
            LayerAdaptiveBypassConfig {
                entry_threshold: 8,
                work_threshold: 64,
            },
        ));
    }

    #[test]
    fn large_active_layer_with_small_work_stays_progressive() {
        let stats = LayerPendingStats {
            pending_entries: 12,
            estimated_work_units: 24,
            prepared_fragment_count: 0,
            regular_shape_count: 12,
        };

        assert!(!should_bypass_progressive_for_layer(
            stats,
            LayerAdaptiveBypassConfig {
                entry_threshold: 8,
                work_threshold: 64,
            },
        ));
    }

    #[test]
    fn active_layer_can_bypass_with_temporary_full_layer_budget() {
        let layer = LayerId { layer: 10, datatype: 0 };
        let pending = VecDeque::from([
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(0, layer) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(1, layer) },
            PendingTileBuild { view_revision: 1, tile_key: key_for_layer(2, layer) },
        ]);

        let stats = LayerPendingStats {
            pending_entries: 3,
            estimated_work_units: 20,
            prepared_fragment_count: 1,
            regular_shape_count: 3,
        };

        let budget = effective_build_budget_for_active_layer(
            pending.len(),
            Some(layer),
            stats,
            2,
            LayerAdaptiveBypassConfig {
                entry_threshold: 4,
                work_threshold: 32,
            },
        );

        assert_eq!(budget, 3);
    }

    #[test]
    fn render_debug_stats_include_active_layer_bypass_details() {
        let stats = RenderDebugStats::new(
            9, 3, 2, 1, 24, 2, 2, 1, 1, 4, 2, 12, 64, 768, 3, 2, 5, 9, 6, 16, 2,
            Some(LayerId { layer: 1, datatype: 0 }),
            5,
            32,
            Some(ActiveLayerProgressMode::Bypassed),
            false,
            8,
            128,
            true,
            10.0,
            1.5,
        );

        assert_eq!(stats.active_layer, Some(LayerId { layer: 1, datatype: 0 }));
        assert_eq!(stats.active_layer_pending, 5);
        assert_eq!(stats.active_layer_estimated_work, 32);
        assert_eq!(stats.active_layer_progress_mode, Some(ActiveLayerProgressMode::Bypassed));
        assert_eq!(stats.layer_bypass_entry_threshold, 8);
        assert_eq!(stats.layer_bypass_work_threshold, 128);
    }

    fn key_for_layer(col: i32, layer: LayerId) -> TileCacheKey {
        TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: super::TileId { col, row: 0 },
            layer,
            effective_mode_tag: 0,
            hatch_signature: 0,
            effective_hatch_style_tag: 0,
        }
    }

    fn key(col: i32, last_used_tick: u64) -> TileCacheKey {
        let _ = last_used_tick;
        TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: super::TileId { col, row: 0 },
            layer: LayerId { layer: 1, datatype: 2 },
            effective_mode_tag: 0,
            hatch_signature: 0,
            effective_hatch_style_tag: 0,
        }
    }
}


/// 从 tile cache 里选出应该淘汰的条目。
///
/// 当前策略是一个很直接的近似 LRU：
/// - 优先保住当前可见 tile
/// - 其余条目按 `last_used_tick` 从旧到新淘汰
fn select_tile_eviction_victims(
    usage: &HashMap<TileCacheKey, u64>,
    capacity: usize,
    protected: &std::collections::HashSet<TileCacheKey>,
) -> Vec<TileCacheKey> {
    if usage.len() <= capacity {
        return Vec::new();
    }

    let mut candidates: Vec<_> = usage
        .iter()
        .filter(|(key, _)| !protected.contains(key))
        .map(|(key, tick)| (*key, *tick))
        .collect();
    candidates.sort_by_key(|(_, tick)| *tick);

    let required = usage.len().saturating_sub(capacity);
    candidates
        .into_iter()
        .take(required)
        .map(|(key, _)| key)
        .collect()
}

/// 按近似 LRU 规则裁剪 tile cache。
fn prune_tile_cache(
    cache: &mut HashMap<TileCacheKey, TileCacheEntry>,
    capacity: usize,
    protected: &std::collections::HashSet<TileCacheKey>,
) -> usize {
    let usage: HashMap<_, _> = cache
        .iter()
        .map(|(key, entry)| (*key, entry.last_used_tick))
        .collect();
    let victims = select_tile_eviction_victims(&usage, capacity, protected);
    for victim in &victims {
        cache.remove(victim);
    }
    victims.len()
}
