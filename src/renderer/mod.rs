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
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use egui_wgpu::{Renderer as EguiRenderer, RendererOptions, ScreenDescriptor};
use glam::Vec2;
use wgpu::util::DeviceExt;
use winit::{dpi::PhysicalSize, window::Window};

use crate::{
    camera::Camera2D,
    error::AppError,
    layout::{
        LayoutBundle, LayoutViewBuildOptions, visit_layout_shape_bounds_in_view,
        visit_layout_shapes_in_view,
    },
    scene::{LayerId, Scene},
};

use self::{
    geometry::{
        ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_STYLE_PRESET,
        DEFAULT_HATCH_WIDTH, DEFAULT_TILE_GRID_DIVISIONS, HatchParams, HatchStylePreset,
        LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD, LineVertex, PreparedTileFragments, RenderCacheKey,
        ShapeSpatialIndex, TileGridIndex, TileId, build_hatch_signature,
        build_hatch_style_signature, build_render_cache_key_with_hatch_styles,
        build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles,
        build_scaled_scene_vertices_for_tile, camera_visible_world_bounds,
        emit_scaled_shape_vertices, layer_hatch_style_hash_value, logical_viewport_size,
        prepare_large_shape_tile_fragments, query_visible_shapes, query_visible_tiles,
    },
    pipeline::{ScenePipeline, SceneUniform},
};

/// 暴露给 UI 的渲染调试统计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RenderDebugStats {
    /// 当前数据源总共有多少 shape。
    pub total_shapes: usize,
    /// 经过空间索引初筛后的候选 shape 数。
    pub candidate_shapes: usize,
    /// 最终落进当前可见查询结果的 shape 数。
    pub visible_shapes: usize,
    /// 可见查询过程中命中的空间桶数量。
    pub bucket_hits: usize,
    /// 当前帧准备提交给 GPU 的顶点数。
    pub vertex_count: usize,
    /// 当前帧真正发出的 draw 次数。
    pub draw_calls: usize,
    /// 当前视图相交的 tile 数量。
    pub visible_tiles: usize,
    /// tile 级 GPU cache 命中次数。
    pub tile_cache_hits: usize,
    /// tile 级 GPU cache 未命中次数。
    pub tile_cache_misses: usize,
    /// layer 级别的请求命中次数。
    pub layer_cache_hits: usize,
    /// layer 级别的请求未命中次数。
    pub layer_cache_misses: usize,
    /// 当前 cache 里还活着的 `tile + layer` 条目数。
    pub cache_entries: usize,
    /// cache 允许保留的条目容量上限。
    pub cache_capacity: usize,
    /// 当前 cache 粗略估计的总字节占用。
    pub cache_bytes: usize,
    /// 当前场景生命周期里累计驱逐的 cache 条目数。
    pub cache_evictions: usize,
    /// 被预碎片化路径接管的 shape 数。
    pub prepared_shapes: usize,
    /// 预碎片化后触达过的 tile 数。
    pub prepared_tiles: usize,
    /// 预碎片化后生成的局部 fragment 数。
    pub prepared_fragments: usize,
    /// 当前仍在等待构建的 `tile + layer` 条目数。
    pub pending_entries: usize,
    /// 本帧允许消化的渐进式构建预算。
    pub build_budget: usize,
    /// 视图切换后被丢弃的过期 pending 条目数。
    pub dropped_stale_entries: usize,
    /// 当前正在优先补建的活动 layer。
    pub active_layer: Option<LayerId>,
    /// 活动 layer 尚未完成的条目数。
    pub active_layer_pending: usize,
    /// 活动 layer 的估算工作量。
    pub active_layer_estimated_work: usize,
    /// 活动 layer 当前采用的推进模式。
    pub active_layer_progress_mode: Option<ActiveLayerProgressMode>,
    /// 当前帧是否绕过了渐进式模式。
    pub progressive_bypassed: bool,
    /// 当前 per-layer bypass entry 阈值。
    pub layer_bypass_entry_threshold: usize,
    /// 当前 per-layer bypass work 阈值。
    pub layer_bypass_work_threshold: usize,
    /// 这一帧 scene/tile 调度是否命中了稳定 cache 路径。
    pub cache_hit: bool,
    /// 当前使用的 hatch 间距。
    pub hatch_spacing: f32,
    /// 当前使用的 hatch 线宽。
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
    /// 这一层还差多少个 `tile + layer` 条目没完成。
    pub pending_entries: usize,
    /// 把 pending 条目、普通 shape、prepared fragment 压成的粗略工作量。
    pub estimated_work_units: usize,
    /// 这一层里预碎片化 fragment 的数量。
    pub prepared_fragment_count: usize,
    /// 这一层里普通 shape 的数量。
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
    /// 当前几何数据所属的 scene 版本。
    scene_revision: u64,
    /// 当前缓存归属的量化 zoom。
    zoom_bits: u32,
    /// 命中的 tile 编号。
    tile_id: TileId,
    /// 命中的 layer。
    layer: LayerId,
    /// 当前实际闭合图元画法的紧凑标签。
    effective_mode_tag: u8,
    /// 当前 hatch 参数签名。
    hatch_signature: u64,
    /// 当前 hatch 预设签名。
    effective_hatch_style_tag: u8,
}

/// 一组 tile cache 共享的“域”。
///
/// 现在缓存已经细化到 `tile + layer`，
/// 所以 domain 可以收敛成只描述那些"会影响所有 layer 顶点坐标或图案"的全局条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TileCacheDomainKey {
    /// 当前 scene 版本。
    scene_revision: u64,
    /// 当前 domain 对应的量化 zoom。
    zoom_bits: u32,
    /// 全局 hatch 参数签名。
    hatch_signature: u64,
    /// 全局 hatch 风格签名。
    hatch_style_signature: u64,
}

/// 一条等待构建的 `tile + layer` 缓存请求。
///
/// 渐进式渲染的关键点不是“把所有东西都算完”，
/// 而是只承认最新视图状态；旧视图遗留的工作要么跳过，要么被丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingTileBuild {
    /// 这条构建请求属于哪个视图修订号。
    view_revision: u64,
    /// 需要补建的 `tile + layer` key。
    tile_key: TileCacheKey,
}

/// 一个 `tile + layer` 对应的一段 GPU 顶点缓冲。
///
/// 这里现在只保留真正用于绘制的 GPU buffer。
/// 之前为了做 per-tile display batch，我们还额外持有了一份 CPU 顶点副本，
/// 以及第二层合批后的 GPU buffer；那样会显著放大内存占用，
/// 但对实际卡顿改善并不明显。
struct TileCacheEntry {
    /// 同一个 `tile + layer` 可能因为设备单 buffer 上限被拆成多段。
    vertex_buffers: Vec<TileCacheBufferSegment>,
    /// 估算缓存占用时使用的字节数。
    byte_size: usize,
    /// 近似 LRU 的访问时钟。
    last_used_tick: u64,
}

/// 针对“已经完全稳定的 direct hierarchy 可见集”的第二层显示合批。
///
/// 它和 `tile_vertex_cache` 的关系是：
/// - `tile_vertex_cache`：偏构建复用，粒度是 `tile + layer`
/// - `VisibleDisplayBatch`：偏静态显示复用，粒度是“当前整屏可见集”
///
/// 之所以保留这第二层，是因为 sample 显示静态帧主要卡在 GPU/present，
/// 而不是 tile worker；这时候把很多小 draw 压成少量大 draw 更值钱。
struct VisibleDisplayBatch {
    /// 当前可见集 key 列表的稳定哈希。
    source_hash: u64,
    /// 当前可见集 key 的条目数。
    source_len: usize,
    /// 合批后的 GPU buffer 段列表。
    vertex_buffers: Vec<TileCacheBufferSegment>,
    /// 这批合并显示 buffer 的粗略字节占用。
    byte_size: usize,
}

#[derive(Debug, Clone)]
struct HierarchyTileSource {
    /// 原始 hierarchy 数据源。
    bundle: LayoutBundle,
    /// 这一组 direct hierarchy 构建共享的基础选项。
    ///
    /// renderer 后续会在它上面继续叠：
    /// - 当前 tile bounds
    /// - 当前 layer filter
    base_options: LayoutViewBuildOptions,
    /// 当前层级范围内真实存在过的 layer 列表。
    layer_ids: Vec<LayerId>,
    /// 当前 range 的粗略总 shape 数，用于 UI/debug 展示。
    total_shape_estimate: usize,
}

#[derive(Debug, Clone)]
struct HierarchyTileBuildTask {
    /// 任务派发时所属的视图修订号。
    view_revision: u64,
    /// 这次要构建的 `tile + layer` key。
    tile_key: TileCacheKey,
    /// 共享的 hierarchy 数据源与基础配置。
    source: Arc<HierarchyTileSource>,
    /// 这次构建采用的 zoom 基准。
    zoom: f32,
    /// 当前 tile 实际生效的闭合图元画法。
    effective_mode: ClosedShapeDrawMode,
    /// 当前 tile 实际生效的 hatch 预设。
    effective_hatch_style: HatchStylePreset,
    /// 当前 tile 的世界坐标 bounds。
    tile_bounds: crate::scene::Bounds,
}

#[derive(Debug)]
struct HierarchyTileBuildResult {
    /// 结果所属的视图修订号。
    view_revision: u64,
    /// 已完成构建的 `tile + layer` key。
    tile_key: TileCacheKey,
    /// worker 线程生成好的 CPU 顶点。
    vertices: Vec<LineVertex>,
}

struct HierarchyTileBuildWorkers {
    /// 主线程向 worker 派发构建任务的发送端。
    task_sender: mpsc::Sender<HierarchyTileBuildTask>,
    /// worker 把构建结果送回主线程的接收端。
    result_receiver: mpsc::Receiver<HierarchyTileBuildResult>,
    /// 后台工作线程句柄，保留它们是为了生命周期跟随 renderer。
    _threads: Vec<thread::JoinHandle<()>>,
}

impl HierarchyTileBuildWorkers {
    /// 创建 direct hierarchy tile 构建用的后台 worker 池。
    ///
    /// 这里故意只把“CPU 顶点生成”挪到后台：
    /// - hierarchy 遍历
    /// - tile 局部裁剪
    /// - 顶点发射
    ///
    /// GPU buffer 创建仍然留在主线程，避免和 wgpu 资源模型缠在一起。
    fn new() -> Self {
        let (task_sender, task_receiver) = mpsc::channel::<HierarchyTileBuildTask>();
        let (result_sender, result_receiver) = mpsc::channel::<HierarchyTileBuildResult>();
        let receiver = Arc::new(Mutex::new(task_receiver));
        let worker_count = thread::available_parallelism()
            .map(|count| count.get().clamp(1, 8))
            .unwrap_or(4);
        let mut threads = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let result_sender = result_sender.clone();
            threads.push(
                thread::Builder::new()
                    .name(format!("hierarchy-tile-worker-{index}"))
                    .spawn(move || {
                        loop {
                            let task = {
                                let Ok(receiver) = receiver.lock() else {
                                    break;
                                };
                                receiver.recv()
                            };
                            let Ok(task) = task else {
                                break;
                            };
                            let vertices = build_hierarchy_tile_vertices(
                                &task.source,
                                task.tile_key,
                                task.zoom,
                                task.effective_mode,
                                task.effective_hatch_style,
                                task.tile_bounds,
                            );
                            if result_sender
                                .send(HierarchyTileBuildResult {
                                    view_revision: task.view_revision,
                                    tile_key: task.tile_key,
                                    vertices,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .expect("spawn hierarchy tile worker"),
            );
        }

        Self {
            task_sender,
            result_receiver,
            _threads: threads,
        }
    }
}

struct TileCacheBufferSegment {
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,
}

/// 默认 tile cache 条目上限。
pub const DEFAULT_TILE_CACHE_CAPACITY: usize = 512;
/// 估算每个 tile cache 条目默认可占用的字节预算。
///
/// 这里先给一个保守上限，避免“条目数不多，但每个条目都特别大”时，
/// 仅靠 entry 数量上限仍然把内存一路顶高。
const TILE_CACHE_BYTES_PER_ENTRY_BUDGET: usize = 512 * 1024;

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
/// direct hierarchy 超远景下，内部允许把 hatch/hatch-outline 临时退成 outline。
///
/// 注意这不是“修改用户配置”，而是 renderer 的内部 far-zoom 退化：
/// 放大到用户能看清时，会自动回到原来的显示模式。
const DIRECT_HIERARCHY_OUTLINE_ONLY_MAX_ZOOM: f32 = 0.02;
/// direct hierarchy tile cache 的 zoom 桶比例。
///
/// 如果直接用精确 zoom 做 cache key，滚轮缩放时会几乎每一帧都失效；
/// 这里改成邻近 zoom 共用一个桶，优先复用已有 tile 几何。
const DIRECT_HIERARCHY_TILE_CACHE_ZOOM_BUCKET_RATIO: f32 = 1.08;
/// direct hierarchy 内部允许使用的最粗 tile grid。
///
/// 现在把它收到了 `1x1`，原因是当前瓶颈已经更偏 GPU/present：
/// 对超远景全局视图来说，继续切很多 tile 容易重复生成和重复 draw。
const DIRECT_HIERARCHY_MAX_TILE_GRID_DIVISIONS: u32 = 1;

#[derive(Debug, Clone, Copy, Default)]
struct CachedShapeQueryStats {
    candidate_shapes: usize,
    visible_shapes: usize,
    bucket_hits: usize,
    visible_tiles: usize,
}

fn effective_draw_mode_for_current_view(
    base_mode: ClosedShapeDrawMode,
    hierarchy_tile_source_active: bool,
    zoom: f32,
) -> ClosedShapeDrawMode {
    if hierarchy_tile_source_active && zoom <= DIRECT_HIERARCHY_OUTLINE_ONLY_MAX_ZOOM {
        ClosedShapeDrawMode::Outline
    } else {
        base_mode
    }
}

fn quantize_zoom_for_tile_cache(zoom: f32, hierarchy_tile_source_active: bool) -> f32 {
    // 旧实现里 zoom 只要有微小变化，就会让 tile cache 整锅失效。
    // 对大版图来说，这会把 worker 吞吐浪费在“每次缩放都从头重建”上。
    if !hierarchy_tile_source_active || zoom <= f32::EPSILON {
        return zoom;
    }

    let ratio = DIRECT_HIERARCHY_TILE_CACHE_ZOOM_BUCKET_RATIO;
    let bucket_index = (zoom.ln() / ratio.ln()).round();
    ratio.powf(bucket_index)
}

fn effective_tile_grid_divisions(
    requested_divisions: u32,
    hierarchy_tile_source_active: bool,
) -> u32 {
    // direct hierarchy 路径下，这里不是把 UI 配置“改掉”，
    // 而是内部按更保守的策略重解释它，优先减少远景全局视图的重复工作。
    if hierarchy_tile_source_active {
        requested_divisions.min(DIRECT_HIERARCHY_MAX_TILE_GRID_DIVISIONS)
    } else {
        requested_divisions
    }
}

fn should_use_per_tile_scissor(hierarchy_tile_source_active: bool) -> bool {
    !hierarchy_tile_source_active
}

fn should_skip_static_cache_refresh(
    cached_scene_key: Option<RenderCacheKey>,
    next_scene_key: RenderCacheKey,
    tile_cache_domain_matches: bool,
    pending_tile_builds: usize,
    in_flight_tile_builds: usize,
) -> bool {
    // 这是一个非常保守的 fast path：
    // 只有当 scene key、tile cache domain、pending/in-flight 全都稳定时，
    // 才允许整帧跳过 update_scene_cache 的 bookkeeping。
    tile_cache_domain_matches
        && cached_scene_key == Some(next_scene_key)
        && pending_tile_builds == 0
        && in_flight_tile_builds == 0
}

fn should_use_static_visible_display_batch(
    hierarchy_tile_source_active: bool,
    freeze_interaction_view: bool,
    visible_tile_key_count: usize,
    pending_tile_builds: usize,
    in_flight_tile_builds: usize,
) -> bool {
    // 只有 direct hierarchy 的完全静态帧才值得做第二层显示合批。
    // 一旦还在交互、还有 pending、或者 tile 正在后台构建，
    // 再去做这层 copy 反而会增加额外开销。
    hierarchy_tile_source_active
        && !freeze_interaction_view
        && visible_tile_key_count > 0
        && pending_tile_builds == 0
        && in_flight_tile_builds == 0
}

fn batch_visible_vertex_segments(
    segment_vertex_counts: &[usize],
    max_vertices_per_buffer: usize,
) -> Vec<usize> {
    // 输入是一串已有 tile cache segment 的顶点数，
    // 输出是“如果把这些 segment 尽量拼成少量大 buffer”，每段该放多少顶点。
    if segment_vertex_counts.is_empty() || max_vertices_per_buffer == 0 {
        return Vec::new();
    }

    let mut merged = Vec::new();
    let mut current = 0usize;
    for count in segment_vertex_counts
        .iter()
        .copied()
        .filter(|count| *count > 0)
    {
        let mut remaining = count;
        while remaining > 0 {
            let available = max_vertices_per_buffer.saturating_sub(current);
            if available == 0 {
                if current > 0 {
                    merged.push(current);
                }
                current = 0;
                continue;
            }
            let take = remaining.min(available);
            current += take;
            remaining -= take;
            if current == max_vertices_per_buffer {
                merged.push(current);
                current = 0;
            }
        }
    }

    if current > 0 {
        merged.push(current);
    }
    merged
}

fn hash_visible_tile_keys(visible_tile_keys: &[TileCacheKey]) -> u64 {
    // 这不是安全校验，只是一个快速“可见集有没有变”的签名。
    let mut hasher = DefaultHasher::new();
    visible_tile_keys.hash(&mut hasher);
    hasher.finish()
}

/// GPU 渲染器。
pub struct Renderer {
    /// 交换链输出 surface。
    surface: wgpu::Surface<'static>,
    /// 主设备对象，负责创建 buffer / pipeline / bind group 等 GPU 资源。
    device: wgpu::Device,
    /// 提交命令和上传数据到 GPU 的队列。
    queue: wgpu::Queue,
    /// 当前 surface 配置。
    config: wgpu::SurfaceConfiguration,
    /// 当前窗口物理尺寸。
    size: PhysicalSize<u32>,
    /// egui 的 wgpu 渲染器。
    egui_renderer: EguiRenderer,
    /// 主场景绘制 pipeline。
    scene_pipeline: ScenePipeline,
    /// direct hierarchy tile 构建用的后台 worker 池。
    hierarchy_tile_workers: HierarchyTileBuildWorkers,
    /// 当前 renderer 消费的扁平场景。
    ///
    /// 在 direct hierarchy 模式下，这里通常是空 Scene，
    /// 真正的几何来源会转移到 `hierarchy_tile_source`。
    scene: Arc<Scene>,
    /// 大场景 direct hierarchy 渲染时的分层数据源摘要。
    hierarchy_tile_source: Option<Arc<HierarchyTileSource>>,
    /// `tile -> layers` 的惰性摘要。
    ///
    /// 只有 direct hierarchy 模式下才会按需填充。
    hierarchy_tile_layer_hints: HashMap<TileId, Vec<LayerId>>,
    /// 普通 Scene 路径使用的 shape 可见查询索引。
    spatial_index: ShapeSpatialIndex,
    /// 当前渲染路径统一使用的 tile 网格。
    tile_grid: TileGridIndex,
    /// 对超大 shape 的 tile 预碎片化缓存。
    prepared_tile_fragments: PreparedTileFragments,
    /// UI 侧配置的 tile grid 密度。
    tile_grid_divisions: u32,
    /// 全局默认闭合图形显示模式。
    draw_mode: ClosedShapeDrawMode,
    /// 每层闭合图形显示模式覆盖。
    layer_draw_modes: BTreeMap<LayerId, ClosedShapeDrawMode>,
    /// 全局 hatch 参数。
    hatch_params: HatchParams,
    /// 全局默认 hatch 风格。
    hatch_style: HatchStylePreset,
    /// 每层 hatch 风格覆盖。
    layer_hatch_styles: BTreeMap<LayerId, HatchStylePreset>,
    /// tile cache 容量上限。
    tile_cache_capacity: usize,
    /// 场景/数据源发生语义变化时递增的版本号。
    scene_revision: u64,
    /// 近似 LRU 使用的访问时钟。
    cache_access_tick: u64,
    /// 从启动到现在累计驱逐掉的 cache 条目数。
    total_cache_evictions: usize,
    /// 当前帧级 scene cache key。
    cached_scene_key: Option<RenderCacheKey>,
    /// 当前 tile 顶点缓存所在的“域”。
    tile_cache_domain: Option<TileCacheDomainKey>,
    /// 第一层 GPU cache：`tile + layer -> vertex buffers`。
    tile_vertex_cache: HashMap<TileCacheKey, TileCacheEntry>,
    /// 第二层显示 cache：只服务于稳定静态视图的批处理结果。
    visible_display_batch: Option<VisibleDisplayBatch>,
    /// 已知为空的 `tile + layer` key。
    ///
    /// 这样后续看到同一个 key 时可以直接跳过，不必重复构建。
    empty_tile_keys: HashSet<TileCacheKey>,
    /// 当前仍在 worker 线程里构建中的 tile keys。
    in_flight_hierarchy_tile_builds: HashSet<TileCacheKey>,
    /// 当前这一帧最终允许显示的 tile keys。
    visible_tile_keys: Vec<TileCacheKey>,
    /// 当前视图理论上需要的全部 tile keys。
    requested_tile_keys: Vec<TileCacheKey>,
    /// 尚未命中 cache、等待构建的 tile keys。
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
    /// 每帧最多允许消化多少个 pending 构建任务。
    progressive_build_budget: usize,
    /// 当待补条目很少时，允许直接 bypass 渐进补全。
    progressive_bypass_threshold: usize,
    /// 当前活动 layer 触发 bypass 的 entry 阈值。
    layer_bypass_entry_threshold: usize,
    /// 当前活动 layer 触发 bypass 的估算工作量阈值。
    layer_bypass_work_threshold: usize,
    /// 累计丢掉了多少条“已经过期的旧视图构建请求”。
    total_stale_drops: usize,
    /// 最近一次 visible query 的统计摘要。
    shape_query_stats: CachedShapeQueryStats,
    /// 暴露给 UI 的调试统计。
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
        let hierarchy_tile_workers = HierarchyTileBuildWorkers::new();
        let empty_scene = Arc::new(Scene::empty());

        Ok(Self {
            surface,
            device,
            queue,
            config,
            size,
            egui_renderer,
            scene_pipeline,
            hierarchy_tile_workers,
            scene: Arc::clone(&empty_scene),
            hierarchy_tile_source: None,
            hierarchy_tile_layer_hints: HashMap::new(),
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
            visible_display_batch: None,
            empty_tile_keys: HashSet::new(),
            in_flight_hierarchy_tile_builds: HashSet::new(),
            visible_tile_keys: Vec::new(),
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

    /// 当前 surface 的物理尺寸。
    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }

    /// 获取调试统计信息。
    pub fn debug_stats(&self) -> RenderDebugStats {
        self.debug_stats
    }

    /// 当前 tile grid 密度。
    ///
    /// 注意在 direct hierarchy 模式下，内部实际使用的密度可能更粗，
    /// 这里返回的是 UI 配置值本身。
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
        self.tile_grid = if self.hierarchy_tile_source.is_some() {
            TileGridIndex::build_for_bounds(
                self.tile_grid.scene_bounds(),
                effective_tile_grid_divisions(self.tile_grid_divisions, true),
            )
        } else {
            TileGridIndex::build_with_divisions(&self.scene, self.tile_grid_divisions)
        };
        if self.hierarchy_tile_source.is_some() {
            self.hierarchy_tile_layer_hints.clear();
        }
        if self.hierarchy_tile_source.is_none() {
            self.rebuild_prepared_tile_fragments();
        }
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
        let evicted = prune_tile_cache(
            &mut self.tile_vertex_cache,
            capacity,
            tile_cache_byte_budget(capacity),
            &std::collections::HashSet::new(),
        );
        self.total_cache_evictions += evicted;
        self.cached_scene_key = None;
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();
        self.visible_display_batch = None;
        self.pending_tile_builds.clear();
        self.empty_tile_keys.clear();
        self.in_flight_hierarchy_tile_builds.clear();
        self.debug_stats.cache_hit = false;
    }

    /// 调整“轻场景直接补完”的阈值。
    pub fn set_progressive_bypass_threshold(&mut self, threshold: usize) {
        let threshold = threshold.clamp(
            MIN_PROGRESSIVE_BYPASS_THRESHOLD,
            MAX_PROGRESSIVE_BYPASS_THRESHOLD,
        );
        if self.progressive_bypass_threshold == threshold {
            return;
        }

        self.progressive_bypass_threshold = threshold;
        self.cached_scene_key = None;
        self.requested_tile_keys.clear();
        self.visible_tile_keys.clear();
        self.visible_display_batch = None;
        self.pending_tile_builds.clear();
        self.active_progressive_layer = None;
        self.empty_tile_keys.clear();
        self.in_flight_hierarchy_tile_builds.clear();
        self.debug_stats.cache_hit = false;
    }

    /// 同时调整按 layer 的双阈值 bypass 条件。
    pub fn set_layer_bypass_thresholds(&mut self, entry_threshold: usize, work_threshold: usize) {
        let entry_threshold = entry_threshold.clamp(
            MIN_LAYER_BYPASS_ENTRY_THRESHOLD,
            MAX_LAYER_BYPASS_ENTRY_THRESHOLD,
        );
        let work_threshold = work_threshold.clamp(
            MIN_LAYER_BYPASS_WORK_THRESHOLD,
            MAX_LAYER_BYPASS_WORK_THRESHOLD,
        );
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
        self.visible_display_batch = None;
        self.pending_tile_builds.clear();
        self.active_progressive_layer = None;
        self.empty_tile_keys.clear();
        self.in_flight_hierarchy_tile_builds.clear();
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
        self.visible_display_batch = None;
    }

    /// 用新场景替换旧场景，并重建相关索引。
    pub fn update_scene(&mut self, scene: Arc<Scene>) {
        self.hierarchy_tile_source = None;
        self.hierarchy_tile_layer_hints.clear();
        self.scene = scene;
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
        self.debug_stats = RenderDebugStats::new(
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            self.tile_cache_capacity,
            0,
            self.total_cache_evictions,
            0,
            0,
            0,
            0,
            self.progressive_build_budget,
            self.total_stale_drops,
            None,
            0,
            0,
            None,
            false,
            self.layer_bypass_entry_threshold,
            self.layer_bypass_work_threshold,
            false,
            self.hatch_params.spacing,
            self.hatch_params.width,
        );
    }

    /// 切换到 direct hierarchy tile 渲染模式。
    ///
    /// 这里不会立刻把整份 hierarchy 扁平化成 Scene，
    /// 而是只建立 renderer 后续按 tile 请求几何所需的最小摘要。
    pub fn update_hierarchy_tile_source(
        &mut self,
        bundle: LayoutBundle,
        base_options: LayoutViewBuildOptions,
        layer_ids: Vec<LayerId>,
        root_bounds: crate::scene::Bounds,
        total_shape_estimate: usize,
    ) {
        let tile_grid = TileGridIndex::build_for_bounds(
            root_bounds,
            effective_tile_grid_divisions(self.tile_grid_divisions, true),
        );
        let base_options = base_options
            .with_visible_world_bounds(Some(root_bounds))
            .with_layer_filter(None);
        let base_options = LayoutViewBuildOptions {
            subtree_screen_lod: None,
            ..base_options
        };
        self.hierarchy_tile_layer_hints.clear();
        self.hierarchy_tile_source = Some(Arc::new(HierarchyTileSource {
            bundle,
            base_options,
            layer_ids,
            total_shape_estimate,
        }));
        self.scene = Arc::new(Scene::empty());
        self.spatial_index = ShapeSpatialIndex::build(&self.scene);
        self.tile_grid = tile_grid;
        self.prepared_tile_fragments = PreparedTileFragments::default();
        self.scene_revision = self.scene_revision.wrapping_add(1);
        self.layer_draw_modes.clear();
        self.layer_hatch_styles.clear();
        self.shape_query_stats = CachedShapeQueryStats::default();
        self.invalidate_progressive_state(true);
        self.debug_stats = RenderDebugStats::default();
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
        let hierarchy_tile_source_active = self.hierarchy_tile_source.is_some();
        let tile_cache_zoom_basis =
            quantize_zoom_for_tile_cache(camera.zoom(), hierarchy_tile_source_active);
        // 交互冻结的目标不是“停止渲染”，而是：
        // - 拖拽/缩放过程中不要为每个中间状态都重建 scene cache
        // - 先复用上一帧已经稳定的视图
        // - 用 shader 里的位置缩放让画面跟手
        let freeze_interaction_view = should_freeze_interaction_view(
            interaction_degraded,
            self.cached_scene_key.is_some() && !self.visible_tile_keys.is_empty(),
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
        let use_static_display_batch = should_use_static_visible_display_batch(
            hierarchy_tile_source_active,
            freeze_interaction_view,
            self.visible_tile_keys.len(),
            self.pending_tile_builds.len(),
            self.in_flight_hierarchy_tile_builds.len(),
        );
        self.ensure_visible_display_batch(&mut encoder, use_static_display_batch);

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
                camera.zoom() / tile_cache_zoom_basis.max(f32::EPSILON)
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
            render_pass.set_scissor_rect(
                canvas_scissor.x,
                canvas_scissor.y,
                canvas_scissor.width,
                canvas_scissor.height,
            );
            if let Some(batch) = &self.visible_display_batch {
                self.debug_stats.draw_calls = batch.vertex_buffers.len();
                self.debug_stats.cache_bytes = self
                    .tile_vertex_cache
                    .values()
                    .map(|entry| entry.byte_size)
                    .sum::<usize>()
                    + batch.byte_size;
                for segment in &batch.vertex_buffers {
                    render_pass.set_vertex_buffer(0, segment.vertex_buffer.slice(..));
                    render_pass.draw(0..segment.vertex_count, 0..1);
                }
            } else {
                self.debug_stats.draw_calls = self.visible_draw_segment_count_for_current_view();
                let translation = camera.pan() + canvas_origin;
                let use_per_tile_scissor =
                    should_use_per_tile_scissor(self.hierarchy_tile_source.is_some());
                for tile_key in &self.visible_tile_keys {
                    if let Some(entry) = self.tile_vertex_cache.get(tile_key) {
                        if use_per_tile_scissor {
                            let tile_bounds = self.tile_grid.tile_bounds(tile_key.tile_id);
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
                            } else {
                                continue;
                            }
                        }
                        for segment in &entry.vertex_buffers {
                            render_pass.set_vertex_buffer(0, segment.vertex_buffer.slice(..));
                            render_pass.draw(0..segment.vertex_count, 0..1);
                        }
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
    ///
    /// 这是 renderer 里最重要的“调度层”：
    /// - 先决定当前视图有没有变
    /// - 再决定 requested/visible tile keys
    /// - 再决定 pending build、显示渐进、cache prune
    /// - 最后产出这一帧的 debug 统计
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
        self.collect_completed_hierarchy_tile_builds();
        let effective_layer_draw_modes = self.layer_draw_modes.clone();
        let hierarchy_tile_source_active = self.hierarchy_tile_source.is_some();
        let tile_cache_zoom_basis =
            quantize_zoom_for_tile_cache(camera.zoom(), hierarchy_tile_source_active);
        let effective_draw_mode = effective_draw_mode_for_current_view(
            draw_mode,
            hierarchy_tile_source_active,
            camera.zoom(),
        );
        let previous_visible_tile_keys = self.visible_tile_keys.clone();
        let previous_requested_tile_keys = self.requested_tile_keys.clone();

        // `cached_scene_key` 回答“这一帧要不要重新组织可见结果”；
        // `tile_cache_domain` 回答“已经建好的 tile buffer 还能不能继续用”。
        //
        // 两者看起来接近，但作用不同：
        // - 前者更偏视图 / 隐藏层 / 模式切换
        // - 后者更偏顶点几何本身是否失效
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
            zoom_bits: tile_cache_zoom_basis.to_bits(),
            hatch_signature: build_hatch_signature(hatch_params),
            hatch_style_signature: build_hatch_style_signature(hatch_style)
                ^ layer_hatch_style_hash_value(&self.layer_hatch_styles),
        };
        if self.tile_cache_domain != Some(domain) {
            self.tile_vertex_cache.clear();
            self.empty_tile_keys.clear();
            self.in_flight_hierarchy_tile_builds.clear();
            self.tile_cache_domain = Some(domain);
        }
        let tile_cache_domain_matches = self.tile_cache_domain == Some(domain);
        if should_skip_static_cache_refresh(
            self.cached_scene_key,
            key,
            tile_cache_domain_matches,
            self.pending_tile_builds.len(),
            self.in_flight_hierarchy_tile_builds.len(),
        ) {
            self.debug_stats.cache_hit = true;
            return;
        }

        let mut dropped_this_frame = 0usize;
        let mut stable_overlap_keys = HashSet::new();
        if self.cached_scene_key != Some(key) {
            let can_incrementally_refresh_tiles = self
                .cached_scene_key
                .map(|previous| previous.differs_only_by_pan(key))
                .unwrap_or(false);

            // 当前视图变化时，先重新推导 requested tile keys。
            // direct hierarchy 与普通 Scene 路径在这里共用同一套调度壳，
            // 只是 tile/layer 来源不同。
            let visible_world = camera_visible_world_bounds(camera, viewport_size);
            let visible_world_center = Vec2::new(
                (visible_world.min_x + visible_world.max_x) * 0.5,
                (visible_world.min_y + visible_world.max_y) * 0.5,
            );
            let visible_tiles = query_visible_tiles(&self.tile_grid, visible_world);
            if self.hierarchy_tile_source.is_some() {
                self.ensure_hierarchy_tile_layer_hints(&visible_tiles);
            }
            let layer_order = self
                .hierarchy_tile_source
                .as_ref()
                .map(|source| source.layer_ids.clone())
                .unwrap_or_else(|| self.scene.layer_ids());
            if self.hierarchy_tile_source.is_some() {
                self.shape_query_stats = CachedShapeQueryStats {
                    candidate_shapes: 0,
                    visible_shapes: 0,
                    bucket_hits: 0,
                    visible_tiles: visible_tiles.len(),
                };
            } else {
                let shape_query =
                    query_visible_shapes(&self.scene, &self.spatial_index, visible_world);
                self.shape_query_stats = CachedShapeQueryStats {
                    candidate_shapes: shape_query.stats.candidate_shapes,
                    visible_shapes: shape_query.stats.visible_shapes,
                    bucket_hits: shape_query.stats.bucket_hits,
                    visible_tiles: visible_tiles.len(),
                };
            }

            let mut keys_by_layer: BTreeMap<LayerId, Vec<TileCacheKey>> = BTreeMap::new();
            for tile_id in visible_tiles.iter().copied() {
                let tile_layers: BTreeSet<LayerId> = if self.hierarchy_tile_source.is_some() {
                    self.hierarchy_tile_layer_hints
                        .get(&tile_id)
                        .into_iter()
                        .flat_map(|layers| layers.iter())
                        .copied()
                        .filter(|layer| !hidden_layers.contains(layer))
                        .collect()
                } else {
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
                                .any(|shape_index| {
                                    !self
                                        .prepared_tile_fragments
                                        .shape_indices
                                        .contains(shape_index)
                                })
                        })
                        .collect();
                    tile_layers.extend(
                        self.prepared_tile_fragments
                            .layers_for_tile(tile_id)
                            .iter()
                            .copied()
                            .filter(|layer| !hidden_layers.contains(layer)),
                    );
                    tile_layers
                };

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
                        zoom_bits: tile_cache_zoom_basis.to_bits(),
                        tile_id,
                        layer,
                        effective_mode_tag: effective_mode.as_tag(),
                        hatch_signature: build_hatch_signature(hatch_params),
                        effective_hatch_style_tag: effective_hatch_style.as_tag(),
                    });
                }
            }
            let next_requested_tile_keys = flatten_layer_major_requested_tile_keys(
                keys_by_layer,
                &layer_order,
                &self.tile_grid,
                visible_world_center,
            );
            if can_incrementally_refresh_tiles {
                let previous_requested_set: HashSet<_> =
                    previous_requested_tile_keys.iter().copied().collect();
                stable_overlap_keys.extend(
                    next_requested_tile_keys
                        .iter()
                        .copied()
                        .filter(|tile_key| previous_requested_set.contains(tile_key)),
                );
            }
            self.requested_tile_keys = if can_incrementally_refresh_tiles {
                merge_requested_tile_keys_for_incremental_pan(
                    &previous_requested_tile_keys,
                    next_requested_tile_keys,
                )
            } else {
                next_requested_tile_keys
            };
            let cached_tile_keys: HashSet<_> = self.tile_vertex_cache.keys().copied().collect();
            let (pending, dropped) = if can_incrementally_refresh_tiles {
                refresh_progressive_queue_incremental(
                    self.view_revision,
                    std::mem::take(&mut self.pending_tile_builds),
                    &self.requested_tile_keys,
                    &cached_tile_keys,
                )
            } else {
                self.view_revision = self.view_revision.wrapping_add(1);
                self.active_progressive_layer = None;
                self.active_display_layer = None;
                self.active_display_budget = 0;
                refresh_progressive_queue(
                    self.view_revision,
                    std::mem::take(&mut self.pending_tile_builds),
                    &self.requested_tile_keys,
                    &cached_tile_keys,
                )
            };
            dropped_this_frame = dropped;
            self.total_stale_drops += dropped;
            self.pending_tile_builds = pending;
            self.cached_scene_key = Some(key);
        }

        let active_layer =
            next_active_progressive_layer(self.active_progressive_layer, &self.pending_tile_builds);
        let active_layer_stats = active_layer
            .map(|layer| {
                estimate_active_layer_pending_stats(
                    layer,
                    &self.pending_tile_builds,
                    &self.prepared_tile_fragments,
                    &self.tile_grid,
                )
            })
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
            tile_cache_zoom_basis,
            effective_draw_mode,
            &effective_layer_draw_modes,
            hatch_style,
            effective_build_budget,
        );

        let displayed_active_layer =
            next_active_progressive_layer(self.active_progressive_layer, &self.pending_tile_builds);
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
                let counts_towards_active_budget = Some(tile_key.layer) == displayed_active_layer
                    && !progressive_bypassed
                    && !stable_overlap_keys.contains(&tile_key);
                if counts_towards_active_budget {
                    active_layer_cached_count += 1;
                }
                let allow_layer = should_display_cached_tile_key(
                    tile_key,
                    &stable_overlap_keys,
                    progressive_bypassed,
                    displayed_active_layer,
                    self.active_display_budget,
                    active_layer_visible_count,
                    &revealed_layers,
                );
                if allow_layer {
                    if counts_towards_active_budget {
                        active_layer_visible_count += 1;
                    }
                    total_vertices += entry
                        .vertex_buffers
                        .iter()
                        .map(|segment| segment.vertex_count as usize)
                        .sum::<usize>();
                    visible_tile_keys.push(tile_key);
                }
            } else if self.empty_tile_keys.contains(&tile_key) {
                state.0 = true;
                layer_cache_hits += 1;
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
            tile_cache_byte_budget(self.tile_cache_capacity),
            &protected_tiles,
        );
        self.total_cache_evictions += evicted;

        let cache_bytes: usize = self
            .tile_vertex_cache
            .values()
            .map(|entry| entry.byte_size)
            .sum::<usize>();

        self.visible_tile_keys = stabilize_visible_tile_keys_during_transition(
            visible_tile_keys,
            &previous_visible_tile_keys,
            self.pending_tile_builds.len(),
        );
        self.debug_stats = RenderDebugStats::new(
            self.hierarchy_tile_source
                .as_ref()
                .map(|source| source.total_shape_estimate)
                .unwrap_or_else(|| self.scene.shapes().len()),
            self.shape_query_stats.candidate_shapes,
            self.shape_query_stats.visible_shapes,
            self.shape_query_stats.bucket_hits,
            total_vertices,
            self.visible_tile_keys.len(),
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
        self.visible_display_batch = None;
        self.view_revision = self.view_revision.wrapping_add(1);
        if clear_gpu_cache {
            self.tile_cache_domain = None;
            self.tile_vertex_cache.clear();
            self.empty_tile_keys.clear();
            self.in_flight_hierarchy_tile_builds.clear();
        }
    }

    /// 回收后台 worker 已完成的 direct hierarchy tile 构建结果。
    fn collect_completed_hierarchy_tile_builds(&mut self) {
        self.cache_access_tick = self.cache_access_tick.wrapping_add(1);
        let current_tick = self.cache_access_tick;
        let mut changed = false;

        while let Ok(result) = self.hierarchy_tile_workers.result_receiver.try_recv() {
            self.in_flight_hierarchy_tile_builds
                .remove(&result.tile_key);
            if result.view_revision != self.view_revision {
                self.total_stale_drops += 1;
                continue;
            }
            if let Some(entry) =
                create_tile_cache_entry(&self.device, &result.vertices, current_tick)
            {
                self.tile_vertex_cache.insert(result.tile_key, entry);
            } else {
                self.empty_tile_keys.insert(result.tile_key);
            }
            changed = true;
        }
        if changed {
            self.visible_display_batch = None;
        }
    }

    /// 按需为当前可见 tiles 补 `tile -> layers` 摘要。
    ///
    /// 这里故意是 lazy 的：
    /// 打开大文件时不再先扫描整个 root 去建立全局摘要，
    /// 而是等用户真正看到某个 tile 时再补这块的信息。
    fn ensure_hierarchy_tile_layer_hints(&mut self, visible_tiles: &[TileId]) {
        let Some(source) = self.hierarchy_tile_source.as_ref().map(Arc::clone) else {
            return;
        };
        let missing_tiles: HashSet<_> = visible_tiles
            .iter()
            .copied()
            .filter(|tile_id| !self.hierarchy_tile_layer_hints.contains_key(tile_id))
            .collect();
        if missing_tiles.is_empty() {
            return;
        }

        let hints = build_hierarchy_tile_layer_hints_for_tiles(
            &source.bundle,
            source.base_options,
            &self.tile_grid,
            &missing_tiles,
        );
        for tile_id in missing_tiles {
            self.hierarchy_tile_layer_hints
                .insert(tile_id, hints.get(&tile_id).cloned().unwrap_or_default());
        }
    }

    /// 把当前视图仍然缺失的 `tile + layer` 键放进待构建队列。
    fn enqueue_pending_tile_build(&mut self, tile_key: TileCacheKey) {
        if self.tile_vertex_cache.contains_key(&tile_key)
            || self.in_flight_hierarchy_tile_builds.contains(&tile_key)
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
        if self.hierarchy_tile_source.is_some() {
            self.dispatch_pending_hierarchy_tile_builds(
                zoom,
                draw_mode,
                layer_draw_modes,
                hatch_style,
                build_budget,
            );
            return;
        }

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

            let Some(pending) =
                pop_next_pending_for_layer(&mut self.pending_tile_builds, active_layer)
            else {
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
            } else {
                self.empty_tile_keys.insert(pending.tile_key);
            }
            built += 1;

            if !self
                .pending_tile_builds
                .iter()
                .any(|item| item.tile_key.layer == active_layer)
            {
                self.active_progressive_layer = None;
            }
        }
    }

    /// direct hierarchy 模式下，把 pending `tile + layer` 任务发给后台 worker。
    fn dispatch_pending_hierarchy_tile_builds(
        &mut self,
        zoom: f32,
        draw_mode: ClosedShapeDrawMode,
        layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
        hatch_style: HatchStylePreset,
        build_budget: usize,
    ) {
        let Some(source) = self.hierarchy_tile_source.as_ref().map(Arc::clone) else {
            return;
        };
        let mut dispatched = 0usize;

        while dispatched < build_budget {
            let Some(active_layer) = next_active_progressive_layer(
                self.active_progressive_layer,
                &self.pending_tile_builds,
            ) else {
                self.active_progressive_layer = None;
                break;
            };
            self.active_progressive_layer = Some(active_layer);
            let Some(pending) =
                pop_next_pending_for_layer(&mut self.pending_tile_builds, active_layer)
            else {
                self.active_progressive_layer = None;
                break;
            };
            if pending.view_revision != self.view_revision {
                self.total_stale_drops += 1;
                continue;
            }
            if self.tile_vertex_cache.contains_key(&pending.tile_key)
                || self.empty_tile_keys.contains(&pending.tile_key)
                || self
                    .in_flight_hierarchy_tile_builds
                    .contains(&pending.tile_key)
            {
                continue;
            }

            let effective_mode = layer_draw_modes
                .get(&pending.tile_key.layer)
                .copied()
                .unwrap_or(draw_mode);
            let effective_hatch_style = self
                .layer_hatch_styles
                .get(&pending.tile_key.layer)
                .copied()
                .unwrap_or(hatch_style);
            let task = HierarchyTileBuildTask {
                view_revision: self.view_revision,
                tile_key: pending.tile_key,
                source: Arc::clone(&source),
                zoom,
                effective_mode,
                effective_hatch_style,
                tile_bounds: self.tile_grid.tile_bounds(pending.tile_key.tile_id),
            };

            if self.hierarchy_tile_workers.task_sender.send(task).is_err() {
                break;
            }
            self.in_flight_hierarchy_tile_builds
                .insert(pending.tile_key);
            dispatched += 1;

            if !self
                .pending_tile_builds
                .iter()
                .any(|item| item.tile_key.layer == active_layer)
            {
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
        if let Some(source) = &self.hierarchy_tile_source {
            let tile_bounds = self.tile_grid.tile_bounds(tile_key.tile_id);
            let effective_mode = layer_draw_modes
                .get(&tile_key.layer)
                .copied()
                .unwrap_or(draw_mode);
            let effective_hatch_style = self
                .layer_hatch_styles
                .get(&tile_key.layer)
                .copied()
                .unwrap_or(hatch_style);
            let vertices = build_hierarchy_tile_vertices(
                source,
                tile_key,
                zoom,
                effective_mode,
                effective_hatch_style,
                tile_bounds,
            );
            return create_tile_cache_entry(&self.device, &vertices, current_tick);
        }

        let shape_indices: Vec<_> = self
            .tile_grid
            .shape_indices_for_tile_layer(tile_key.tile_id, tile_key.layer)
            .iter()
            .map(|shape_index| *shape_index as usize)
            .filter(|shape_index| {
                !self
                    .prepared_tile_fragments
                    .shape_indices
                    .contains(&(*shape_index as u32))
            })
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
            vertices.extend(
                build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles(
                    prepared_fragments,
                    zoom,
                    &empty_hidden_layers,
                    layer_draw_modes,
                    &self.layer_hatch_styles,
                    draw_mode,
                    hatch_style,
                ),
            );
        }
        create_tile_cache_entry(&self.device, &vertices, current_tick)
    }

    /// 统计当前视图如果“不做静态合批”，实际需要多少段 draw。
    ///
    /// 这个值比 `visible_tile_keys.len()` 更接近 GPU 真实负担，
    /// 因为单个 cache entry 也可能因为 buffer 上限被拆成多段。
    fn visible_draw_segment_count_for_current_view(&self) -> usize {
        self.visible_tile_keys
            .iter()
            .filter_map(|tile_key| self.tile_vertex_cache.get(tile_key))
            .map(|entry| entry.vertex_buffers.len())
            .sum()
    }

    /// 在静态 direct hierarchy 视图下，建立一份“整屏显示批处理”。
    ///
    /// 这里依然复用第一层 tile cache 里的 GPU buffer，
    /// 只是通过 `copy_buffer_to_buffer` 把当前可见集重新打包成少量大 buffer，
    /// 以减少静态帧的 draw call 和 vertex buffer 绑定次数。
    fn ensure_visible_display_batch(&mut self, encoder: &mut wgpu::CommandEncoder, enabled: bool) {
        if !enabled {
            self.visible_display_batch = None;
            return;
        }

        let source_hash = hash_visible_tile_keys(&self.visible_tile_keys);
        if self.visible_display_batch.as_ref().is_some_and(|batch| {
            batch.source_hash == source_hash && batch.source_len == self.visible_tile_keys.len()
        }) {
            return;
        }

        let max_buffer_size = self.device.limits().max_buffer_size as usize;
        let vertex_size = std::mem::size_of::<LineVertex>();
        let max_vertices_per_buffer =
            max_vertices_per_buffer_for_limit(max_buffer_size, vertex_size);
        let segment_vertex_counts: Vec<usize> = self
            .visible_tile_keys
            .iter()
            .filter_map(|tile_key| self.tile_vertex_cache.get(tile_key))
            .flat_map(|entry| {
                entry
                    .vertex_buffers
                    .iter()
                    .map(|segment| segment.vertex_count as usize)
            })
            .collect();
        let batch_vertex_counts =
            batch_visible_vertex_segments(&segment_vertex_counts, max_vertices_per_buffer);
        if batch_vertex_counts.is_empty() {
            self.visible_display_batch = None;
            return;
        }

        let mut vertex_buffers = Vec::with_capacity(batch_vertex_counts.len());
        let byte_size = batch_vertex_counts
            .iter()
            .copied()
            .map(|count| count * vertex_size)
            .sum::<usize>();
        for vertex_count in batch_vertex_counts {
            vertex_buffers.push(TileCacheBufferSegment {
                vertex_buffer: self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("visible-display-batch-buffer"),
                    size: (vertex_count * vertex_size) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                vertex_count: vertex_count as u32,
            });
        }

        let mut dst_index = 0usize;
        let mut dst_vertex_offset = 0usize;
        for tile_key in &self.visible_tile_keys {
            let Some(entry) = self.tile_vertex_cache.get(tile_key) else {
                self.visible_display_batch = None;
                return;
            };
            for source_segment in &entry.vertex_buffers {
                let mut remaining_vertices = source_segment.vertex_count as usize;
                let mut src_vertex_offset = 0usize;
                while remaining_vertices > 0 {
                    let Some(dst_segment) = vertex_buffers.get(dst_index) else {
                        self.visible_display_batch = None;
                        return;
                    };
                    let dst_capacity = dst_segment.vertex_count as usize;
                    let dst_remaining = dst_capacity.saturating_sub(dst_vertex_offset);
                    if dst_remaining == 0 {
                        dst_index += 1;
                        dst_vertex_offset = 0;
                        continue;
                    }
                    let take_vertices = remaining_vertices.min(dst_remaining);
                    encoder.copy_buffer_to_buffer(
                        &source_segment.vertex_buffer,
                        (src_vertex_offset * vertex_size) as u64,
                        &dst_segment.vertex_buffer,
                        (dst_vertex_offset * vertex_size) as u64,
                        (take_vertices * vertex_size) as u64,
                    );
                    remaining_vertices -= take_vertices;
                    src_vertex_offset += take_vertices;
                    dst_vertex_offset += take_vertices;
                    if dst_vertex_offset == dst_capacity {
                        dst_index += 1;
                        dst_vertex_offset = 0;
                    }
                }
            }
        }

        self.visible_display_batch = Some(VisibleDisplayBatch {
            source_hash,
            source_len: self.visible_tile_keys.len(),
            vertex_buffers,
            byte_size,
        });
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
        let x1 = self
            .x
            .saturating_add(self.width)
            .min(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .min(other.y.saturating_add(other.height));
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
    ScissorRect::from_logical_rect(
        min,
        max - min,
        pixels_per_point,
        surface_width,
        surface_height,
    )
}

/// 为一个 tile 创建 GPU vertex buffer。
fn create_tile_cache_entry(
    device: &wgpu::Device,
    vertices: &[LineVertex],
    last_used_tick: u64,
) -> Option<TileCacheEntry> {
    if vertices.is_empty() {
        return None;
    }

    let max_buffer_size = device.limits().max_buffer_size as usize;
    let max_vertices_per_buffer =
        max_vertices_per_buffer_for_limit(max_buffer_size, std::mem::size_of::<LineVertex>());
    let mut vertex_buffers = Vec::new();

    for chunk in vertices.chunks(max_vertices_per_buffer) {
        vertex_buffers.push(TileCacheBufferSegment {
            vertex_buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tile-layer-vertex-buffer"),
                contents: bytemuck::cast_slice(chunk),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_SRC,
            }),
            vertex_count: chunk.len() as u32,
        });
    }

    Some(TileCacheEntry {
        vertex_buffers,
        byte_size: std::mem::size_of_val(vertices),
        last_used_tick,
    })
}

fn build_hierarchy_tile_layer_hints_for_tiles(
    bundle: &LayoutBundle,
    options: LayoutViewBuildOptions,
    tile_grid: &TileGridIndex,
    requested_tiles: &HashSet<TileId>,
) -> HashMap<TileId, Vec<LayerId>> {
    if requested_tiles.is_empty() {
        return HashMap::new();
    }

    let query_bounds = requested_tiles
        .iter()
        .copied()
        .map(|tile_id| tile_grid.tile_bounds(tile_id))
        .reduce(|acc, bounds| acc.union(bounds))
        .expect("requested tiles are non-empty");
    let mut tile_layers_seen: HashMap<TileId, BTreeSet<LayerId>> = HashMap::new();
    let _ = visit_layout_shape_bounds_in_view(
        bundle,
        options.with_visible_world_bounds(Some(query_bounds)),
        |layer, _hierarchy_level, bounds| {
            for tile_id in tile_grid.tile_ids_for_bounds(bounds) {
                if !requested_tiles.contains(&tile_id) {
                    continue;
                }
                tile_layers_seen.entry(tile_id).or_default().insert(layer);
            }
        },
    );
    tile_layers_seen
        .into_iter()
        .map(|(tile_id, layers)| (tile_id, layers.into_iter().collect()))
        .collect()
}

fn build_hierarchy_tile_vertices(
    source: &HierarchyTileSource,
    tile_key: TileCacheKey,
    zoom: f32,
    effective_mode: ClosedShapeDrawMode,
    effective_hatch_style: HatchStylePreset,
    tile_bounds: crate::scene::Bounds,
) -> Vec<LineVertex> {
    let mut vertices = Vec::new();
    let scaled_tile_bounds = crate::scene::Bounds::new(
        tile_bounds.min_x * zoom,
        tile_bounds.min_y * zoom,
        tile_bounds.max_x * zoom,
        tile_bounds.max_y * zoom,
    );
    let _ = visit_layout_shapes_in_view(
        &source.bundle,
        source
            .base_options
            .with_visible_world_bounds(Some(tile_bounds))
            .with_layer_filter(Some(tile_key.layer)),
        |shape| {
            emit_scaled_shape_vertices(
                &mut vertices,
                shape.layer,
                &shape.points,
                shape.closed,
                shape.stroke_width_world,
                zoom,
                effective_mode,
                Some(scaled_tile_bounds),
                effective_hatch_style,
            );
        },
    );
    vertices
}

fn max_vertices_per_buffer_for_limit(max_buffer_size: usize, vertex_size: usize) -> usize {
    let max_vertices = max_buffer_size / vertex_size;
    let triangle_aligned = max_vertices - (max_vertices % 3);
    triangle_aligned.max(3)
}

fn should_freeze_interaction_view(interaction_degraded: bool, has_stable_view: bool) -> bool {
    interaction_degraded && has_stable_view
}

fn enqueue_unique_pending(
    mut pending: VecDeque<PendingTileBuild>,
    new_item: PendingTileBuild,
) -> VecDeque<PendingTileBuild> {
    if pending
        .iter()
        .any(|item| item.tile_key == new_item.tile_key)
    {
        return pending;
    }
    pending.push_back(new_item);
    pending
}

fn stabilize_visible_tile_keys_during_transition(
    mut current: Vec<TileCacheKey>,
    previous: &[TileCacheKey],
    pending_entries: usize,
) -> Vec<TileCacheKey> {
    if pending_entries == 0 {
        return current;
    }

    let mut seen: HashSet<TileCacheKey> = current.iter().copied().collect();
    for key in previous {
        if seen.insert(*key) {
            current.push(*key);
        }
    }
    current
}

fn sort_tile_keys_by_world_center(
    mut keys: Vec<TileCacheKey>,
    tile_grid: &TileGridIndex,
    center: Vec2,
) -> Vec<TileCacheKey> {
    keys.sort_by(|a, b| {
        let a_bounds = tile_grid.tile_bounds(a.tile_id);
        let b_bounds = tile_grid.tile_bounds(b.tile_id);
        let a_center = Vec2::new(
            (a_bounds.min_x + a_bounds.max_x) * 0.5,
            (a_bounds.min_y + a_bounds.max_y) * 0.5,
        );
        let b_center = Vec2::new(
            (b_bounds.min_x + b_bounds.max_x) * 0.5,
            (b_bounds.min_y + b_bounds.max_y) * 0.5,
        );
        let a_dist = a_center.distance_squared(center);
        let b_dist = b_center.distance_squared(center);
        a_dist
            .partial_cmp(&b_dist)
            .unwrap_or(std::cmp::Ordering::Equal)
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
            ordered.extend(sort_tile_keys_by_world_center(
                keys,
                tile_grid,
                visible_world_center,
            ));
        }
    }

    for (_, keys) in keys_by_layer {
        ordered.extend(sort_tile_keys_by_world_center(
            keys,
            tile_grid,
            visible_world_center,
        ));
    }

    ordered
}

fn merge_requested_tile_keys_for_incremental_pan(
    previous_requested: &[TileCacheKey],
    next_requested: Vec<TileCacheKey>,
) -> Vec<TileCacheKey> {
    let next_requested_set: HashSet<_> = next_requested.iter().copied().collect();
    let mut merged = Vec::with_capacity(next_requested.len());
    let mut seen = HashSet::new();

    for key in previous_requested {
        if next_requested_set.contains(key) && seen.insert(*key) {
            merged.push(*key);
        }
    }

    for key in next_requested {
        if seen.insert(key) {
            merged.push(key);
        }
    }

    merged
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
    for item in pending
        .iter()
        .filter(|item| item.tile_key.layer == active_layer)
    {
        tile_ids.push(item.tile_key.tile_id);
    }
    tile_ids.sort();
    tile_ids.dedup();

    let prepared_fragment_count: usize = tile_ids
        .iter()
        .map(|tile_id| {
            prepared
                .fragments_for_tile_layer(*tile_id, active_layer)
                .map(|f| f.len())
                .unwrap_or(0)
        })
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

    let pending_entries = pending
        .iter()
        .filter(|item| item.tile_key.layer == active_layer)
        .count();
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
    pending
        .iter()
        .filter(|item| item.tile_key.layer == layer)
        .count()
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

fn should_display_cached_tile_key(
    tile_key: TileCacheKey,
    stable_overlap_keys: &HashSet<TileCacheKey>,
    progressive_bypassed: bool,
    displayed_active_layer: Option<LayerId>,
    active_display_budget: usize,
    active_layer_visible_count: usize,
    revealed_layers: &BTreeSet<LayerId>,
) -> bool {
    if progressive_bypassed || stable_overlap_keys.contains(&tile_key) {
        return true;
    }

    if Some(tile_key.layer) == displayed_active_layer {
        return active_display_budget == usize::MAX
            || active_layer_visible_count < active_display_budget;
    }

    revealed_layers.contains(&tile_key.layer)
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
    let index = pending
        .iter()
        .position(|item| item.tile_key.layer == layer)?;
    pending.remove(index)
}

fn refresh_progressive_queue(
    view_revision: u64,
    old_pending: VecDeque<PendingTileBuild>,
    requested_keys: &[TileCacheKey],
    cached_keys: &HashSet<TileCacheKey>,
) -> (VecDeque<PendingTileBuild>, usize) {
    let previous_keys: HashSet<_> = old_pending.iter().map(|item| item.tile_key).collect();
    let requested_key_set: HashSet<_> = requested_keys.iter().copied().collect();
    let dropped = previous_keys
        .iter()
        .filter(|tile_key| !requested_key_set.contains(tile_key))
        .count();
    let pending = requested_keys
        .iter()
        .copied()
        .filter(|tile_key| !cached_keys.contains(tile_key))
        .map(|tile_key| PendingTileBuild {
            view_revision,
            tile_key,
        })
        .collect();
    (pending, dropped)
}

fn refresh_progressive_queue_incremental(
    view_revision: u64,
    old_pending: VecDeque<PendingTileBuild>,
    requested_keys: &[TileCacheKey],
    cached_keys: &HashSet<TileCacheKey>,
) -> (VecDeque<PendingTileBuild>, usize) {
    let requested_key_set: HashSet<_> = requested_keys.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut dropped = 0usize;
    let mut pending = VecDeque::new();

    for previous in old_pending {
        if !requested_key_set.contains(&previous.tile_key) {
            dropped += 1;
            continue;
        }
        if cached_keys.contains(&previous.tile_key) || !seen.insert(previous.tile_key) {
            continue;
        }
        pending.push_back(PendingTileBuild {
            view_revision,
            tile_key: previous.tile_key,
        });
    }

    for tile_key in requested_keys.iter().copied() {
        if cached_keys.contains(&tile_key) || !seen.insert(tile_key) {
            continue;
        }
        pending.push_back(PendingTileBuild {
            view_revision,
            tile_key,
        });
    }

    (pending, dropped)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    use glam::Vec2;

    use crate::{
        camera::Camera2D,
        layout::{
            LayoutBundle, LayoutCell, LayoutCellId, LayoutShape, LayoutView,
            LayoutViewBuildOptions, LayoutViewMetadata,
        },
        renderer::geometry::{
            ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_STYLE_PRESET,
            DEFAULT_HATCH_WIDTH, HatchParams, build_render_cache_key_with_hatch_styles,
        },
        scene::{Bounds, LayerId},
    };

    use std::collections::VecDeque;

    use super::{
        ActiveLayerProgressMode, HierarchyTileSource, LayerAdaptiveBypassConfig, LayerPendingStats,
        PendingTileBuild, RenderDebugStats, TileCacheKey, active_layer_pending_count,
        advance_active_display_budget, batch_visible_vertex_segments,
        build_hierarchy_tile_layer_hints_for_tiles, build_hierarchy_tile_vertices,
        compute_effective_build_budget, compute_revealed_layers_for_display,
        effective_build_budget_for_active_layer, effective_draw_mode_for_current_view,
        effective_tile_grid_divisions, enqueue_unique_pending, max_vertices_per_buffer_for_limit,
        merge_requested_tile_keys_for_incremental_pan, next_active_progressive_layer,
        pop_next_pending_for_layer, quantize_zoom_for_tile_cache, refresh_progressive_queue,
        refresh_progressive_queue_incremental, requested_layers_in_order,
        select_tile_eviction_victims, should_bypass_progressive_for_layer,
        should_display_cached_tile_key, should_freeze_interaction_view,
        should_skip_static_cache_refresh, should_use_per_tile_scissor,
        should_use_static_visible_display_batch, stabilize_visible_tile_keys_during_transition,
        tile_scissor_rect,
    };

    #[test]
    fn eviction_prefers_oldest_non_visible_tiles() {
        let mut usage = HashMap::new();
        let a = key(0, 1);
        let b = key(1, 5);
        let c = key(2, 3);
        usage.insert(a, (1, 10));
        usage.insert(b, (5, 10));
        usage.insert(c, (3, 10));

        let protected = HashSet::from([b]);
        let victims = select_tile_eviction_victims(&usage, 2, usize::MAX, &protected);

        assert_eq!(victims, vec![a]);
    }

    #[test]
    fn disjoint_scissor_intersection_returns_none_without_overflow() {
        let a = super::ScissorRect {
            x: 100,
            y: 100,
            width: 20,
            height: 20,
        };
        let b = super::ScissorRect {
            x: 10,
            y: 10,
            width: 5,
            height: 5,
        };

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
            layer: LayerId {
                layer: 1,
                datatype: 1,
            },
            effective_mode_tag: 0,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };
        let b = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: tile,
            layer: LayerId {
                layer: 1,
                datatype: 2,
            },
            effective_mode_tag: 0,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };
        let c = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: tile,
            layer: LayerId {
                layer: 1,
                datatype: 1,
            },
            effective_mode_tag: 2,
            hatch_signature: 7,
            effective_hatch_style_tag: 0,
        };

        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn hierarchy_tile_hints_only_keep_layers_present_in_requested_tiles() {
        let root_id = LayoutCellId::new(1);
        let layer_a = LayerId {
            layer: 1,
            datatype: 0,
        };
        let layer_b = LayerId {
            layer: 2,
            datatype: 0,
        };
        let root = std::sync::Arc::new(LayoutCell::new(
            root_id,
            "root",
            vec![
                LayoutShape::rectangle(layer_a, Bounds::new(0.0, 0.0, 10.0, 10.0)),
                LayoutShape::rectangle(layer_b, Bounds::new(80.0, 80.0, 90.0, 90.0)),
            ],
            vec![],
        ));
        let bundle = LayoutBundle::new(
            vec![root],
            vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
        )
        .expect("bundle");
        let grid = super::TileGridIndex::build_for_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0), 4);

        let left_tile = super::TileId { col: 0, row: 0 };
        let hints = build_hierarchy_tile_layer_hints_for_tiles(
            &bundle,
            LayoutViewBuildOptions::new(root_id, 0, 0)
                .with_visible_world_bounds(Some(Bounds::new(0.0, 0.0, 100.0, 100.0))),
            &grid,
            &HashSet::from([left_tile]),
        );

        let right_tile = super::TileId { col: 3, row: 3 };
        assert_eq!(hints.get(&left_tile), Some(&vec![layer_a]));
        assert!(!hints.contains_key(&right_tile));
    }

    #[test]
    fn hierarchy_tile_vertices_match_scene_tile_builder_for_same_tile() {
        let root_id = LayoutCellId::new(11);
        let layer = LayerId {
            layer: 7,
            datatype: 0,
        };
        let root = std::sync::Arc::new(LayoutCell::new(
            root_id,
            "root",
            vec![LayoutShape::rectangle(
                layer,
                Bounds::new(20.0, 20.0, 80.0, 80.0),
            )],
            vec![],
        ));
        let bundle = LayoutBundle::new(
            vec![root],
            vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
        )
        .expect("bundle");
        let options = LayoutViewBuildOptions::new(root_id, 0, 0);
        let scene = crate::layout::build_layout_view_scene(&bundle, options).expect("scene");
        let tile_grid = crate::renderer::geometry::TileGridIndex::build_for_bounds(
            Bounds::new(0.0, 0.0, 100.0, 100.0),
            4,
        );
        let tile_id = crate::renderer::geometry::TileId { col: 1, row: 1 };
        let tile_bounds = tile_grid.tile_bounds(tile_id);
        let source = HierarchyTileSource {
            bundle,
            base_options: options,
            layer_ids: vec![layer],
            total_shape_estimate: 1,
        };
        let tile_key = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 0.5f32.to_bits(),
            tile_id,
            layer,
            effective_mode_tag: ClosedShapeDrawMode::HatchOutline.as_tag(),
            hatch_signature: crate::renderer::geometry::build_hatch_signature(
                crate::renderer::geometry::HatchParams {
                    spacing: crate::renderer::geometry::DEFAULT_HATCH_SPACING,
                    width: crate::renderer::geometry::DEFAULT_HATCH_WIDTH,
                },
            ),
            effective_hatch_style_tag: crate::renderer::geometry::HatchStylePreset::LeftDiagonal
                .as_tag(),
        };

        let expected = crate::renderer::geometry::build_scaled_scene_vertices_for_tile(
            &scene,
            0.5,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &[0],
            ClosedShapeDrawMode::HatchOutline,
            Some(tile_bounds),
            &BTreeMap::new(),
            crate::renderer::geometry::HatchStylePreset::LeftDiagonal,
        );
        let actual = build_hierarchy_tile_vertices(
            &source,
            tile_key,
            0.5,
            ClosedShapeDrawMode::HatchOutline,
            crate::renderer::geometry::HatchStylePreset::LeftDiagonal,
            tile_bounds,
        );

        assert_eq!(actual.len(), expected.len());
    }

    #[test]
    fn direct_hierarchy_far_zoom_suppresses_fill_to_outline() {
        assert_eq!(
            effective_draw_mode_for_current_view(ClosedShapeDrawMode::HatchOutline, true, 0.01),
            ClosedShapeDrawMode::Outline
        );
        assert_eq!(
            effective_draw_mode_for_current_view(ClosedShapeDrawMode::Hatch, true, 0.01),
            ClosedShapeDrawMode::Outline
        );
        assert_eq!(
            effective_draw_mode_for_current_view(ClosedShapeDrawMode::HatchOutline, true, 0.2),
            ClosedShapeDrawMode::HatchOutline
        );
    }

    #[test]
    fn direct_hierarchy_quantizes_nearby_zooms_into_same_tile_cache_bucket() {
        let base = quantize_zoom_for_tile_cache(1.0, true);
        let nearby = quantize_zoom_for_tile_cache(1.03, true);
        let farther = quantize_zoom_for_tile_cache(1.2, true);

        assert_eq!(base.to_bits(), nearby.to_bits());
        assert_ne!(base.to_bits(), farther.to_bits());
        assert_eq!(quantize_zoom_for_tile_cache(1.03, false), 1.03);
    }

    #[test]
    fn direct_hierarchy_uses_coarser_internal_tile_grid() {
        assert_eq!(effective_tile_grid_divisions(8, true), 1);
        assert_eq!(effective_tile_grid_divisions(4, true), 1);
        assert_eq!(effective_tile_grid_divisions(2, true), 1);
        assert_eq!(effective_tile_grid_divisions(8, false), 8);
    }

    #[test]
    fn direct_hierarchy_skips_per_tile_scissor_state_churn() {
        assert!(!should_use_per_tile_scissor(true));
        assert!(should_use_per_tile_scissor(false));
    }

    #[test]
    fn static_cache_hit_can_skip_scene_cache_refresh_work() {
        let key = build_render_cache_key_with_hatch_styles(
            1,
            &Camera2D::default(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            Vec2::ZERO,
            Vec2::new(100.0, 100.0),
            Vec2::new(100.0, 100.0),
            ClosedShapeDrawMode::Outline,
            HatchParams {
                spacing: DEFAULT_HATCH_SPACING,
                width: DEFAULT_HATCH_WIDTH,
            },
            DEFAULT_HATCH_STYLE_PRESET,
        );
        assert!(should_skip_static_cache_refresh(Some(key), key, true, 0, 0));
        assert!(!should_skip_static_cache_refresh(None, key, true, 0, 0));
        assert!(!should_skip_static_cache_refresh(
            Some(key),
            key,
            false,
            0,
            0
        ));
        assert!(!should_skip_static_cache_refresh(
            Some(key),
            key,
            true,
            1,
            0
        ));
        assert!(!should_skip_static_cache_refresh(
            Some(key),
            key,
            true,
            0,
            1
        ));
    }

    #[test]
    fn static_display_batch_only_enables_for_fully_stable_direct_hierarchy_views() {
        assert!(should_use_static_visible_display_batch(
            true, false, 4, 0, 0
        ));
        assert!(!should_use_static_visible_display_batch(
            false, false, 4, 0, 0
        ));
        assert!(!should_use_static_visible_display_batch(
            true, true, 4, 0, 0
        ));
        assert!(!should_use_static_visible_display_batch(
            true, false, 0, 0, 0
        ));
        assert!(!should_use_static_visible_display_batch(
            true, false, 4, 1, 0
        ));
        assert!(!should_use_static_visible_display_batch(
            true, false, 4, 0, 1
        ));
    }

    #[test]
    fn static_display_batch_merges_visible_segments_until_buffer_limit() {
        let merged = batch_visible_vertex_segments(&[3, 3, 3, 3, 3], 6);
        assert_eq!(merged, vec![6, 6, 3]);

        let merged = batch_visible_vertex_segments(&[4, 4, 1, 2], 8);
        assert_eq!(merged, vec![8, 3]);
    }

    #[test]
    fn enqueue_skips_duplicate_tile_keys() {
        let pending = enqueue_unique_pending(
            VecDeque::from([PendingTileBuild {
                view_revision: 3,
                tile_key: key(0, 0),
            }]),
            PendingTileBuild {
                view_revision: 3,
                tile_key: key(0, 0),
            },
        );

        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn transition_keeps_previous_visible_tiles_while_new_view_is_still_pending() {
        let previous = vec![key(0, 0), key(1, 0)];
        let current = vec![key(2, 0)];

        let stabilized = stabilize_visible_tile_keys_during_transition(current, &previous, 3);

        assert_eq!(stabilized, vec![key(2, 0), key(0, 0), key(1, 0)]);
    }

    #[test]
    fn transition_does_not_keep_previous_tiles_once_view_is_stable() {
        let previous = vec![key(0, 0), key(1, 0)];
        let current = vec![key(2, 0)];

        let stabilized = stabilize_visible_tile_keys_during_transition(current, &previous, 0);

        assert_eq!(stabilized, vec![key(2, 0)]);
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
        let cached = HashSet::new();
        let (pending, dropped) = refresh_progressive_queue(7, old, &requested, &cached);

        assert_eq!(dropped, 2);
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|item| item.view_revision == 7));
        let cols: Vec<_> = pending
            .iter()
            .map(|item| item.tile_key.tile_id.col)
            .collect();
        assert_eq!(cols, vec![2, 3]);
    }

    #[test]
    fn refresh_progressive_queue_preserves_overlap_without_counting_it_as_stale() {
        let old = VecDeque::from([
            PendingTileBuild {
                view_revision: 1,
                tile_key: key(0, 0),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key(1, 0),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key(2, 0),
            },
        ]);
        let requested = vec![key(1, 0), key(2, 0), key(3, 0)];
        let cached = HashSet::new();
        let (pending, dropped) = refresh_progressive_queue(7, old, &requested, &cached);

        assert_eq!(dropped, 1);
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().all(|item| item.view_revision == 7));
        let cols: Vec<_> = pending
            .iter()
            .map(|item| item.tile_key.tile_id.col)
            .collect();
        assert_eq!(cols, vec![1, 2, 3]);
    }

    #[test]
    fn refresh_progressive_queue_skips_already_cached_tiles() {
        let old = VecDeque::new();
        let requested = vec![key(1, 0), key(2, 0), key(3, 0)];
        let cached = HashSet::from([key(2, 0)]);

        let (pending, dropped) = refresh_progressive_queue(7, old, &requested, &cached);

        assert_eq!(dropped, 0);
        let cols: Vec<_> = pending
            .iter()
            .map(|item| item.tile_key.tile_id.col)
            .collect();
        assert_eq!(cols, vec![1, 3]);
    }

    #[test]
    fn incremental_refresh_keeps_only_uncached_newly_visible_tiles() {
        let old = VecDeque::from([
            PendingTileBuild {
                view_revision: 3,
                tile_key: key(0, 0),
            },
            PendingTileBuild {
                view_revision: 3,
                tile_key: key(1, 0),
            },
        ]);
        let requested = vec![key(1, 0), key(2, 0), key(3, 0)];
        let cached = HashSet::from([key(1, 0), key(2, 0)]);

        let (pending, dropped) = refresh_progressive_queue_incremental(3, old, &requested, &cached);

        assert_eq!(dropped, 1);
        let cols: Vec<_> = pending
            .iter()
            .map(|item| item.tile_key.tile_id.col)
            .collect();
        assert_eq!(cols, vec![3]);
    }

    #[test]
    fn incremental_pan_keeps_overlap_tile_order_stable() {
        let previous = vec![key(1, 0), key(2, 0), key(3, 0), key(4, 0)];
        let next = vec![key(4, 0), key(3, 0), key(2, 0), key(5, 0)];

        let merged = merge_requested_tile_keys_for_incremental_pan(&previous, next);
        let cols: Vec<_> = merged.iter().map(|item| item.tile_id.col).collect();

        assert_eq!(cols, vec![2, 3, 4, 5]);
    }

    #[test]
    fn stable_overlap_tile_stays_visible_even_when_later_layers_are_not_revealed() {
        let active_layer = LayerId {
            layer: 1,
            datatype: 0,
        };
        let later_layer = LayerId {
            layer: 2,
            datatype: 0,
        };
        let overlap_key = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: super::TileId { col: 4, row: 0 },
            layer: later_layer,
            effective_mode_tag: 0,
            hatch_signature: 0,
            effective_hatch_style_tag: 0,
        };
        let revealed_layers = BTreeSet::from([active_layer]);
        let stable_overlap = HashSet::from([overlap_key]);

        assert!(should_display_cached_tile_key(
            overlap_key,
            &stable_overlap,
            false,
            Some(active_layer),
            0,
            0,
            &revealed_layers,
        ));
    }

    #[test]
    fn non_overlap_later_layer_tile_stays_hidden_until_revealed() {
        let active_layer = LayerId {
            layer: 1,
            datatype: 0,
        };
        let later_layer = LayerId {
            layer: 2,
            datatype: 0,
        };
        let later_key = TileCacheKey {
            scene_revision: 1,
            zoom_bits: 1.0f32.to_bits(),
            tile_id: super::TileId { col: 5, row: 0 },
            layer: later_layer,
            effective_mode_tag: 0,
            hatch_signature: 0,
            effective_hatch_style_tag: 0,
        };
        let revealed_layers = BTreeSet::from([active_layer]);

        assert!(!should_display_cached_tile_key(
            later_key,
            &HashSet::new(),
            false,
            Some(active_layer),
            0,
            0,
            &revealed_layers,
        ));
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
    fn max_vertices_per_buffer_respects_limit_and_triangle_alignment() {
        let vertex_size = std::mem::size_of::<super::LineVertex>();
        let max_vertices = max_vertices_per_buffer_for_limit(vertex_size * 10 + 1, vertex_size);

        assert_eq!(max_vertices, 9);
        assert!(max_vertices * vertex_size <= vertex_size * 10 + 1);
        assert_eq!(max_vertices % 3, 0);
    }

    #[test]
    fn active_display_budget_grows_progressively_for_same_layer() {
        let layer = LayerId {
            layer: 5,
            datatype: 0,
        };
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
        let layer = LayerId {
            layer: 5,
            datatype: 0,
        };
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
        let layer_a = LayerId {
            layer: 1,
            datatype: 0,
        };
        let layer_b = LayerId {
            layer: 2,
            datatype: 0,
        };
        let layer_c = LayerId {
            layer: 3,
            datatype: 0,
        };
        let requested = vec![
            key_for_layer(0, layer_a),
            key_for_layer(1, layer_a),
            key_for_layer(2, layer_b),
            key_for_layer(3, layer_b),
            key_for_layer(4, layer_c),
        ];
        let pending = VecDeque::from([
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(2, layer_b),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(4, layer_c),
            },
        ]);

        let revealed = compute_revealed_layers_for_display(&requested, &pending, Some(layer_b));

        assert!(revealed.contains(&layer_a));
        assert!(revealed.contains(&layer_b));
        assert!(!revealed.contains(&layer_c));
    }

    #[test]
    fn requested_layers_keep_layer_major_order_without_duplicates() {
        let layer_a = LayerId {
            layer: 1,
            datatype: 0,
        };
        let layer_b = LayerId {
            layer: 2,
            datatype: 0,
        };
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
        let layer_a = LayerId {
            layer: 1,
            datatype: 1,
        };
        let layer_b = LayerId {
            layer: 2,
            datatype: 0,
        };
        let pending = VecDeque::from([
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(0, layer_a),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(1, layer_a),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(2, layer_b),
            },
        ]);

        assert_eq!(next_active_progressive_layer(None, &pending), Some(layer_a));
        assert_eq!(
            next_active_progressive_layer(Some(layer_a), &pending),
            Some(layer_a)
        );
    }

    #[test]
    fn active_layer_switches_after_previous_layer_finishes() {
        let layer_a = LayerId {
            layer: 1,
            datatype: 1,
        };
        let layer_b = LayerId {
            layer: 2,
            datatype: 0,
        };
        let mut pending = VecDeque::from([
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(0, layer_a),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(1, layer_b),
            },
        ]);

        let first = pop_next_pending_for_layer(&mut pending, layer_a).expect("first layer item");
        assert_eq!(first.tile_key.layer, layer_a);
        assert_eq!(
            next_active_progressive_layer(Some(layer_a), &pending),
            Some(layer_b)
        );
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
        let layer = LayerId {
            layer: 10,
            datatype: 0,
        };
        let pending = VecDeque::from([
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(0, layer),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(1, layer),
            },
            PendingTileBuild {
                view_revision: 1,
                tile_key: key_for_layer(2, layer),
            },
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
    fn tile_cache_eviction_also_respects_byte_budget() {
        let protected = std::collections::HashSet::new();
        let usage = HashMap::from([
            (key(0, 0), (1_u64, 70_usize)),
            (key(1, 0), (2_u64, 60_usize)),
            (key(2, 0), (3_u64, 10_usize)),
        ]);

        let victims = select_tile_eviction_victims(&usage, 8, 100, &protected);

        assert_eq!(victims, vec![key(0, 0)]);
    }

    #[test]
    fn render_debug_stats_include_active_layer_bypass_details() {
        let stats = RenderDebugStats::new(
            9,
            3,
            2,
            1,
            24,
            2,
            2,
            1,
            1,
            4,
            2,
            12,
            64,
            768,
            3,
            2,
            5,
            9,
            6,
            16,
            2,
            Some(LayerId {
                layer: 1,
                datatype: 0,
            }),
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

        assert_eq!(
            stats.active_layer,
            Some(LayerId {
                layer: 1,
                datatype: 0
            })
        );
        assert_eq!(stats.active_layer_pending, 5);
        assert_eq!(stats.active_layer_estimated_work, 32);
        assert_eq!(
            stats.active_layer_progress_mode,
            Some(ActiveLayerProgressMode::Bypassed)
        );
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
            layer: LayerId {
                layer: 1,
                datatype: 2,
            },
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
    usage: &HashMap<TileCacheKey, (u64, usize)>,
    capacity: usize,
    byte_budget: usize,
    protected: &std::collections::HashSet<TileCacheKey>,
) -> Vec<TileCacheKey> {
    let total_bytes: usize = usage.values().map(|(_, bytes)| *bytes).sum();
    if usage.len() <= capacity && total_bytes <= byte_budget {
        return Vec::new();
    }

    let mut candidates: Vec<_> = usage
        .iter()
        .filter(|(key, _)| !protected.contains(key))
        .map(|(key, (tick, bytes))| (*key, *tick, *bytes))
        .collect();
    candidates.sort_by_key(|(_, tick, _)| *tick);

    let mut remaining_entries = usage.len();
    let mut remaining_bytes = total_bytes;
    let mut victims = Vec::new();
    for (key, _, bytes) in candidates {
        if remaining_entries <= capacity && remaining_bytes <= byte_budget {
            break;
        }
        victims.push(key);
        remaining_entries = remaining_entries.saturating_sub(1);
        remaining_bytes = remaining_bytes.saturating_sub(bytes);
    }
    victims
}

/// 按近似 LRU 规则裁剪 tile cache。
fn prune_tile_cache(
    cache: &mut HashMap<TileCacheKey, TileCacheEntry>,
    capacity: usize,
    byte_budget: usize,
    protected: &std::collections::HashSet<TileCacheKey>,
) -> usize {
    let usage: HashMap<_, _> = cache
        .iter()
        .map(|(key, entry)| (*key, (entry.last_used_tick, entry.byte_size)))
        .collect();
    let victims = select_tile_eviction_victims(&usage, capacity, byte_budget, protected);
    for victim in &victims {
        cache.remove(victim);
    }
    victims.len()
}

fn tile_cache_byte_budget(capacity: usize) -> usize {
    capacity.saturating_mul(TILE_CACHE_BYTES_PER_ENTRY_BUDGET)
}
