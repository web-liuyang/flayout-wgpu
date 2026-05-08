//! 渲染几何与空间查询辅助。
//!
//! 这是当前 viewer 最值得反复读的一层，因为它连接了：
//! - `scene`：内部几何数据
//! - `camera`：当前视图状态
//! - `renderer`：GPU 需要的顶点和缓存 key
//!
//! 你可以把这个模块理解成“渲染前的数据准备层”。
//! 它主要做四类事：
//! 1. 空间索引与 tile grid
//! 2. 可见区域查询
//! 3. 顶点生成
//! 4. 坐标系转换
//!
//! 当前策略是一个很适合学习的折中：
//! - 先用 CPU 做 shape 裁剪和 tile 拆分
//! - 再把结果送给 GPU
//! - 同时把平移保留给 shader uniform 去做，提升 tile 复用率

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use bytemuck::{Pod, Zeroable};
use glam::Vec2;

use crate::{
    camera::Camera2D,
    scene::{Bounds, LayerId, RectShape, Scene},
};

type ShapeIndex = u32;

fn encode_shape_index(index: usize) -> ShapeIndex {
    u32::try_from(index).expect("shape index must fit into u32")
}

/// 最终写入 GPU vertex buffer 的顶点格式。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq)]
pub struct LineVertex {
    /// 顶点位置。
    ///
    /// 在当前主渲染路径下，这里保存的是“缩放后的逻辑屏幕坐标”，
    /// 但还没有加相机平移和画布 origin。
    pub position: [f32; 2],

    /// RGBA 颜色。
    pub color: [f32; 4],

    /// 顶点所属的 primitive 类型。
    ///
    /// 我们把这层语义直接塞进顶点，是为了让 shader 能区分：
    /// - 这是 outline 线段顶点
    /// - 还是 hatch 填充三角形顶点
    ///
    /// 这样后面想继续扩展交叉斜线、点阵、按 layer 不同 hatch，
    /// 都不需要把 CPU 侧几何生成路径推翻重来。
    pub kind: f32,

    /// hatch 预设编码。
    ///
    /// 这里专门给 fill 顶点留出一个独立语义，而不是把 preset 烧进几何路径里，
    /// 原因有两个：
    /// 1. CPU 仍然只负责“这个闭合区域要不要被填充成三角形”
    /// 2. 真正画成左斜线 / 右斜线 / 交叉线 / 点阵，交给 shader 片元阶段决定
    ///
    /// 这样 style 变化时，我们复用的是统一的面三角语义，
    /// 而不是为每一种 hatch 单独生成一套不同 CPU 几何。
    pub hatch_style: f32,
}

/// 普通轮廓线段顶点。
const VERTEX_KIND_OUTLINE: f32 = 0.0;

/// 闭合图形的 hatch 填充顶点。
const VERTEX_KIND_HATCH_FILL: f32 = 1.0;

/// outline 顶点不会真正用到 hatch preset，
/// 但为了保证整个 vertex buffer 布局稳定，我们仍然填一个固定值。
const DEFAULT_VERTEX_HATCH_STYLE: f32 = 0.0;

/// 当高点数闭合图形在屏幕上已经缩得很小时，
/// 没必要继续保留全部原始轮廓点。
const CLOSED_SHAPE_LOD_MIN_POINTS: usize = 64;
const CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT: f32 = 24.0;
const CLOSED_SHAPE_LOD_MIN_TARGET_POINTS: usize = 8;
const CLOSED_SHAPE_LOD_MAX_TARGET_POINTS: usize = 24;
const TINY_SHAPE_MARKER_MAX_SCREEN_EXTENT: f32 = 2.0;
const TINY_SHAPE_MARKER_SCREEN_SIZE: f32 = 1.5;
const SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT: f32 = 12.0;
const POLYLINE_LOD_MIN_POINTS: usize = 64;
const POLYLINE_LOD_MAX_SCREEN_EXTENT: f32 = 24.0;
const POLYLINE_LOD_MIN_TARGET_POINTS: usize = 6;
const POLYLINE_LOD_MAX_TARGET_POINTS: usize = 20;

/// 一帧级别的场景缓存 key。
///
/// 只要这里任意一个因素变化，说明当前屏幕上的可见结果可能变化了，
/// 我们就需要重新计算 visible tiles / 统计信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCacheKey {
    scene_revision: u64,
    pan_x_bits: u32,
    pan_y_bits: u32,
    zoom_bits: u32,
    canvas_origin_x_bits: u32,
    canvas_origin_y_bits: u32,
    canvas_width_bits: u32,
    canvas_height_bits: u32,
    viewport_width_bits: u32,
    viewport_height_bits: u32,
    hidden_layers_hash: u64,
}

impl RenderCacheKey {
    /// 判断两个缓存 key 是否只在平移上不同。
    ///
    /// 这类变化仍然需要重新查询可见 tile，
    /// 但不应该把整套渐进式构建状态都当成“全新视图”重置掉。
    pub fn differs_only_by_pan(self, other: Self) -> bool {
        self.scene_revision == other.scene_revision
            && self.zoom_bits == other.zoom_bits
            && self.canvas_origin_x_bits == other.canvas_origin_x_bits
            && self.canvas_origin_y_bits == other.canvas_origin_y_bits
            && self.canvas_width_bits == other.canvas_width_bits
            && self.canvas_height_bits == other.canvas_height_bits
            && self.viewport_width_bits == other.viewport_width_bits
            && self.viewport_height_bits == other.viewport_height_bits
            && self.hidden_layers_hash == other.hidden_layers_hash
            && (self.pan_x_bits != other.pan_x_bits || self.pan_y_bits != other.pan_y_bits)
    }
}

/// tile 网格中的逻辑编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub col: i32,
    pub row: i32,
}

/// 默认 tile grid 密度。
///
/// 这里的含义是：把场景 bounds 平均切成 `N x N` 的网格。
/// 数值越大，tile 越细；越小，tile 越粗。
pub const DEFAULT_TILE_GRID_DIVISIONS: u32 = 8;

/// UI 允许的最小 tile grid 密度。
pub const MIN_TILE_GRID_DIVISIONS: u32 = 2;

/// UI 允许的最大 tile grid 密度。
pub const MAX_TILE_GRID_DIVISIONS: u32 = 32;

/// 触发"超大 shape 预碎片化"的最小 tile 覆盖数。
///
/// 只有当一个图元横跨足够多的 tile 时，
/// 预先把它裁成每个 tile 的局部世界坐标碎片才真正划算。
pub const LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD: usize = 4;
/// 预碎片化允许保留的最大 fragment 数。
///
/// 超过这个量级后，预碎片化带来的内存副本通常已经不划算，
/// 这时宁可回退到运行时 tile 裁剪，也不要把整份场景先复制一遍。
pub const DEFAULT_PREPARED_FRAGMENT_BUDGET: usize = 200_000;
/// 预碎片化允许保留的最大点复制预算。
///
/// 这里统计的是“碎片里会额外复制多少个点”的粗略上限，
/// 用来防止少量超大 polygon 横跨很多 tile 时提前把内存打满。
pub const DEFAULT_PREPARED_POINT_BUDGET: usize = 4_000_000;

/// 闭合图形的显示模式。
///
/// 这里特意只把“填充语义”作用在闭合图形上：
/// - `Boundary / Box / Rectangle / Polygon` 这类图形可以填充
/// - `Path / Polyline` 这类开放折线仍然按线段绘制
///
/// 这样做能在提升可读性的同时，避免把本来就是“线”的图元误画成面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClosedShapeDrawMode {
    /// 只画闭合图形外轮廓。
    Outline,
    /// 只画 hatch 填充，不额外叠加轮廓线。
    Hatch,
    /// 既画 hatch，又额外叠加外轮廓，阅读体验最接近常见版图工具。
    HatchOutline,
}

impl ClosedShapeDrawMode {
    /// 把枚举压成一个稳定的小整数，便于放进缓存 key。
    pub fn as_tag(self) -> u8 {
        match self {
            Self::Outline => 0,
            Self::Hatch => 1,
            Self::HatchOutline => 2,
        }
    }
}

/// 一个很轻量的 shape 空间索引。
///
/// 它不是通用 R-tree，而是均匀网格（uniform grid）。
/// 这么做的原因是：
/// - 实现简单，适合学习
/// - 对 demo 规模足够有效
/// - 容易和 tile cache 的思路形成对照
#[derive(Debug, Clone)]
pub struct ShapeSpatialIndex {
    scene_bounds: Bounds,
    cell_width: f32,
    cell_height: f32,
    cols: i32,
    rows: i32,
    buckets: HashMap<(i32, i32), Vec<ShapeIndex>>,
}

/// tile 级缓存使用的网格索引。
///
/// 它和 `ShapeSpatialIndex` 的目的不同：
/// - `ShapeSpatialIndex` 更偏“快速找可能可见的 shape”
/// - `TileGridIndex` 更偏“把几何按 tile 分组，便于缓存复用”
#[derive(Debug, Clone)]
pub struct TileGridIndex {
    scene_bounds: Bounds,
    tile_width: f32,
    tile_height: f32,
    cols: i32,
    rows: i32,
    tile_layers: HashMap<TileId, Vec<LayerId>>,
    buckets_by_layer: HashMap<(TileId, LayerId), Vec<ShapeIndex>>,
}

/// 为超大 shape 预先切出来的 tile 局部世界坐标碎片。
///
/// 注意这里保存的仍然是"世界坐标"，而不是已经乘了 zoom 的屏幕坐标。
/// 这样做的好处是：
/// - 平移时完全可以复用
/// - 缩放变化时只需要重新做一次世界->缩放坐标转换
/// - 数据结构更贴近"几何准备层"的职责
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedTileFragment {
    pub layer: LayerId,
    pub points: Vec<Vec2>,
    pub closed: bool,
    pub stroke_width_world: Option<f32>,
    /// 对闭合图形，这里保存“原始真实边裁到 tile 内之后”的线段集合。
    ///
    /// 这样 fill 仍然可以用裁剪后的局部多边形，
    /// 但 outline 不会错误地把 tile 裁剪产生的内部边画出来。
    pub outline_segments: Vec<[Vec2; 2]>,
}

/// 一组按 tile 组织好的预碎片化结果。
///
/// `shape_indices` 记录哪些原始 shape 已经由这套结构接管，
/// renderer 后续就不需要再把它们放进"运行时 tile 裁剪"那条路径里。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreparedTileFragments {
    pub per_tile_layer: HashMap<(TileId, LayerId), Vec<PreparedTileFragment>>,
    pub tile_layers: HashMap<TileId, Vec<LayerId>>,
    pub shape_indices: HashSet<ShapeIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedFragmentBudget {
    pub max_fragments: usize,
    pub max_point_copies: usize,
}

impl PreparedFragmentBudget {
    pub const fn new(max_fragments: usize, max_point_copies: usize) -> Self {
        Self {
            max_fragments,
            max_point_copies,
        }
    }

    pub const fn generous_default() -> Self {
        Self::new(
            DEFAULT_PREPARED_FRAGMENT_BUDGET,
            DEFAULT_PREPARED_POINT_BUDGET,
        )
    }
}

impl PreparedTileFragments {
    /// 被预碎片化路径接管的原始 shape 数量。
    pub fn prepared_shape_count(&self) -> usize {
        self.shape_indices.len()
    }

    /// 当前一共预备了多少个 tile 条目。
    pub fn prepared_tile_count(&self) -> usize {
        self.tile_layers.len()
    }

    /// 当前一共准备了多少个局部 fragment。
    ///
    /// 这个数字比 `prepared_shape_count` 更细，
    /// 因为一个超大 shape 可能会被拆成很多 tile 局部碎片。
    pub fn prepared_fragment_count(&self) -> usize {
        self.per_tile_layer.values().map(Vec::len).sum()
    }

    /// 当前 tile 下哪些 layer 拥有预碎片结果。
    pub fn layers_for_tile(&self, tile_id: TileId) -> &[LayerId] {
        self.tile_layers
            .get(&tile_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 取出某个 `tile + layer` 对应的预碎片列表。
    pub fn fragments_for_tile_layer(
        &self,
        tile_id: TileId,
        layer: LayerId,
    ) -> Option<&[PreparedTileFragment]> {
        self.per_tile_layer
            .get(&(tile_id, layer))
            .map(Vec::as_slice)
    }
}

/// 一次可见 shape 查询的统计信息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShapeQueryStats {
    pub bucket_hits: usize,
    pub candidate_shapes: usize,
    pub visible_shapes: usize,
}

/// 可见 shape 查询结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisibleShapeQuery {
    pub indices: Vec<usize>,
    pub stats: ShapeQueryStats,
}

/// Hatch 图案的全局参数。
///
/// 这里故意把参数定义在 geometry 层，而不是直接塞到 shader 字符串里，
/// 是为了让 UI、缓存 key、测试都能复用同一套语义。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HatchParams {
    /// 相邻两条斜线的逻辑像素间距。
    pub spacing: f32,
    /// 每条斜线本身的逻辑像素线宽。
    pub width: f32,
}

/// 运行时可编辑的 hatch 风格预设。
///
/// 这个枚举是“渲染语义层”的概念，不和 UI 绑定。
/// 只要 renderer、shader、缓存 key 都认这一套 tag，
/// 上层无论未来从 preset 下拉框、快捷键还是配置文件切换，都能落到同一条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HatchStylePreset {
    LeftDiagonal,
    RightDiagonal,
    Cross,
    Dots,
}

impl HatchStylePreset {
    /// 压成稳定小整数，便于塞进顶点属性、uniform 派生逻辑和缓存 key。
    pub fn as_tag(self) -> u8 {
        match self {
            Self::LeftDiagonal => 0,
            Self::RightDiagonal => 1,
            Self::Cross => 2,
            Self::Dots => 3,
        }
    }

    /// shader 顶点属性是 `f32`，所以这里提供一个对应的无歧义编码。
    pub fn as_vertex_code(self) -> f32 {
        self.as_tag() as f32
    }
}

pub const DEFAULT_HATCH_STYLE_PRESET: HatchStylePreset = HatchStylePreset::LeftDiagonal;

impl HatchParams {
    pub fn normalized(self) -> Self {
        Self {
            spacing: self.spacing.max(2.0),
            width: self.width.clamp(0.5, self.spacing.max(2.0)),
        }
    }
}

pub const DEFAULT_HATCH_SPACING: f32 = 10.0;
pub const MIN_HATCH_SPACING: f32 = 4.0;
pub const MAX_HATCH_SPACING: f32 = 32.0;
pub const DEFAULT_HATCH_WIDTH: f32 = 1.5;
pub const MIN_HATCH_WIDTH: f32 = 0.5;
pub const MAX_HATCH_WIDTH: f32 = 8.0;

/// 把 hatch 参数压成稳定整数，便于放进缓存 key。
pub fn build_hatch_signature(params: HatchParams) -> u64 {
    let params = params.normalized();
    ((params.spacing.to_bits() as u64) << 32) | params.width.to_bits() as u64
}

/// 把 hatch preset 压成稳定整数，便于参与缓存 key。
pub fn build_hatch_style_signature(style: HatchStylePreset) -> u64 {
    style.as_tag() as u64
}

impl ShapeSpatialIndex {
    /// 基于当前场景构建均匀网格索引。
    pub fn build(scene: &Scene) -> Self {
        let scene_bounds = scene.bounds().unwrap_or(Bounds::new(0.0, 0.0, 1.0, 1.0));
        let shape_count = scene.shapes().len().max(1) as f32;

        // shape 越多，网格越细，但这里做了上限，避免网格本身过度膨胀。
        let grid_side = shape_count.sqrt().ceil().clamp(1.0, 32.0) as i32;
        let cols = grid_side;
        let rows = grid_side;
        let cell_width = (scene_bounds.width().max(1.0) / cols as f32).max(1.0);
        let cell_height = (scene_bounds.height().max(1.0) / rows as f32).max(1.0);
        let mut buckets: HashMap<(i32, i32), Vec<ShapeIndex>> = HashMap::new();

        for (index, shape) in scene.shapes().iter().enumerate() {
            let shape_index = encode_shape_index(index);
            let min_col = cell_for(shape.bounds.min_x, scene_bounds.min_x, cell_width, cols);
            let max_col = cell_for(shape.bounds.max_x, scene_bounds.min_x, cell_width, cols);
            let min_row = cell_for(shape.bounds.min_y, scene_bounds.min_y, cell_height, rows);
            let max_row = cell_for(shape.bounds.max_y, scene_bounds.min_y, cell_height, rows);

            for col in min_col..=max_col {
                for row in min_row..=max_row {
                    buckets.entry((col, row)).or_default().push(shape_index);
                }
            }
        }

        Self {
            scene_bounds,
            cell_width,
            cell_height,
            cols,
            rows,
            buckets,
        }
    }
}

impl TileGridIndex {
    /// 用默认密度构建 tile grid。
    pub fn build(scene: &Scene) -> Self {
        Self::build_with_divisions(scene, DEFAULT_TILE_GRID_DIVISIONS)
    }

    /// 用指定密度构建 tile grid。
    ///
    /// 这里的 `divisions = N` 表示把场景 bounds 分成 `N x N` 块。
    pub fn build_with_divisions(scene: &Scene, divisions: u32) -> Self {
        let scene_bounds = scene.bounds().unwrap_or(Bounds::new(0.0, 0.0, 1.0, 1.0));
        let cols = clamp_tile_grid_divisions(divisions);
        let rows = cols;
        let tile_width = (scene_bounds.width().max(1.0) / cols as f32).max(1.0);
        let tile_height = (scene_bounds.height().max(1.0) / rows as f32).max(1.0);
        let mut tile_layers_seen: HashMap<TileId, BTreeSet<LayerId>> = HashMap::new();
        let mut buckets_by_layer: HashMap<(TileId, LayerId), Vec<ShapeIndex>> = HashMap::new();

        for (index, shape) in scene.shapes().iter().enumerate() {
            let shape_index = encode_shape_index(index);
            let min_col = cell_for(shape.bounds.min_x, scene_bounds.min_x, tile_width, cols);
            let max_col = cell_for(shape.bounds.max_x, scene_bounds.min_x, tile_width, cols);
            let min_row = cell_for(shape.bounds.min_y, scene_bounds.min_y, tile_height, rows);
            let max_row = cell_for(shape.bounds.max_y, scene_bounds.min_y, tile_height, rows);

            for col in min_col..=max_col {
                for row in min_row..=max_row {
                    let tile_id = TileId { col, row };
                    buckets_by_layer
                        .entry((tile_id, shape.layer))
                        .or_default()
                        .push(shape_index);
                    tile_layers_seen
                        .entry(tile_id)
                        .or_default()
                        .insert(shape.layer);
                }
            }
        }

        Self {
            scene_bounds,
            tile_width,
            tile_height,
            cols,
            rows,
            tile_layers: tile_layers_seen
                .into_iter()
                .map(|(tile_id, layers)| (tile_id, layers.into_iter().collect()))
                .collect(),
            buckets_by_layer,
        }
    }

    /// 当前 grid 的密度。
    pub fn divisions(&self) -> u32 {
        self.cols.max(self.rows) as u32
    }

    /// 取出一个 tile 下出现过哪些 layer。
    pub fn layers_for_tile(&self, tile_id: TileId) -> &[LayerId] {
        self.tile_layers
            .get(&tile_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 直接取出某个 `tile + layer` 对应的 shape 索引集合。
    ///
    /// 这是把"按 tile 扫 shape 再现场分层"这一步提前搬到了索引构建阶段。
    pub fn shape_indices_for_tile_layer(&self, tile_id: TileId, layer: LayerId) -> &[ShapeIndex] {
        self.buckets_by_layer
            .get(&(tile_id, layer))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 列出一个 bounds 会横跨哪些 tile。
    ///
    /// 这个辅助函数是"预碎片化"的基础：
    /// 我们会先看一个 shape 会覆盖多少 tile，再决定要不要提前切块。
    pub fn tile_ids_for_bounds(&self, bounds: Bounds) -> Vec<TileId> {
        let min_col = cell_for(
            bounds.min_x,
            self.scene_bounds.min_x,
            self.tile_width,
            self.cols,
        );
        let max_col = cell_for(
            bounds.max_x,
            self.scene_bounds.min_x,
            self.tile_width,
            self.cols,
        );
        let min_row = cell_for(
            bounds.min_y,
            self.scene_bounds.min_y,
            self.tile_height,
            self.rows,
        );
        let max_row = cell_for(
            bounds.max_y,
            self.scene_bounds.min_y,
            self.tile_height,
            self.rows,
        );

        let mut tiles = Vec::new();
        for col in min_col..=max_col {
            for row in min_row..=max_row {
                let tile_id = TileId { col, row };
                if self.tile_layers.contains_key(&tile_id) {
                    tiles.push(tile_id);
                }
            }
        }
        tiles.sort();
        tiles
    }

    /// 计算一个 tile 在世界坐标中的包围盒。
    ///
    /// 这一步很关键，因为当前 tile cache 里保存的是"可能出现在这个 tile 里的 shape 顶点"，
    /// 而不是已经裁剪到 tile 内部的几何。
    /// 所以后续渲染阶段必须拿到 tile 的世界范围，再把 draw call 用 scissor 裁回这一小块区域。
    pub fn tile_bounds(&self, tile_id: TileId) -> Bounds {
        let min_x = self.scene_bounds.min_x + tile_id.col as f32 * self.tile_width;
        let min_y = self.scene_bounds.min_y + tile_id.row as f32 * self.tile_height;
        let max_x = if tile_id.col == self.cols - 1 {
            self.scene_bounds.max_x
        } else {
            min_x + self.tile_width
        };
        let max_y = if tile_id.row == self.rows - 1 {
            self.scene_bounds.max_y
        } else {
            min_y + self.tile_height
        };

        Bounds::new(min_x, min_y, max_x, max_y)
    }
}

/// 查询与可见 world bounds 相交的 shape。
pub fn query_visible_shapes(
    scene: &Scene,
    index: &ShapeSpatialIndex,
    visible_world_bounds: Bounds,
) -> VisibleShapeQuery {
    if scene.shapes().is_empty() {
        return VisibleShapeQuery::default();
    }

    let min_col = cell_for(
        visible_world_bounds.min_x,
        index.scene_bounds.min_x,
        index.cell_width,
        index.cols,
    );
    let max_col = cell_for(
        visible_world_bounds.max_x,
        index.scene_bounds.min_x,
        index.cell_width,
        index.cols,
    );
    let min_row = cell_for(
        visible_world_bounds.min_y,
        index.scene_bounds.min_y,
        index.cell_height,
        index.rows,
    );
    let max_row = cell_for(
        visible_world_bounds.max_y,
        index.scene_bounds.min_y,
        index.cell_height,
        index.rows,
    );

    let mut seen = HashSet::new();
    let mut indices = Vec::new();
    let mut bucket_hits = 0usize;
    let mut candidate_shapes = 0usize;
    for col in min_col..=max_col {
        for row in min_row..=max_row {
            if let Some(bucket) = index.buckets.get(&(col, row)) {
                bucket_hits += 1;
                for &shape_index in bucket {
                    // 同一个 shape 可能跨多个 bucket，所以要去重。
                    if seen.insert(shape_index) {
                        candidate_shapes += 1;
                        if scene.shapes()[shape_index as usize]
                            .bounds
                            .intersects(visible_world_bounds)
                        {
                            indices.push(shape_index as usize);
                        }
                    }
                }
            }
        }
    }
    indices.sort_unstable();

    VisibleShapeQuery {
        stats: ShapeQueryStats {
            bucket_hits,
            candidate_shapes,
            visible_shapes: indices.len(),
        },
        indices,
    }
}

/// 只取可见 shape 索引，适合不关心统计细节的调用方。
pub fn query_visible_shape_indices(
    scene: &Scene,
    index: &ShapeSpatialIndex,
    visible_world_bounds: Bounds,
) -> Vec<usize> {
    query_visible_shapes(scene, index, visible_world_bounds).indices
}

/// 查询与可见 world bounds 相交的 tile 列表。
pub fn query_visible_tiles(index: &TileGridIndex, visible_world_bounds: Bounds) -> Vec<TileId> {
    let min_col = cell_for(
        visible_world_bounds.min_x,
        index.scene_bounds.min_x,
        index.tile_width,
        index.cols,
    );
    let max_col = cell_for(
        visible_world_bounds.max_x,
        index.scene_bounds.min_x,
        index.tile_width,
        index.cols,
    );
    let min_row = cell_for(
        visible_world_bounds.min_y,
        index.scene_bounds.min_y,
        index.tile_height,
        index.rows,
    );
    let max_row = cell_for(
        visible_world_bounds.max_y,
        index.scene_bounds.min_y,
        index.tile_height,
        index.rows,
    );

    let mut tiles = Vec::new();
    for col in min_col..=max_col {
        for row in min_row..=max_row {
            let tile_id = TileId { col, row };
            if index.tile_layers.contains_key(&tile_id) {
                tiles.push(tile_id);
            }
        }
    }
    tiles.sort();
    tiles
}

/// 为超大 shape 提前做一轮按 tile 的世界坐标预碎片化。
///
/// 和当前已有的"运行时 tile 裁剪"相比，这一步更进一步：
/// - 运行时裁剪：每次 tile miss 时，现场把大 shape 裁到 tile 里
/// - 预碎片化：在 scene / tile grid 变化时，先把超大 shape 切好并缓存为世界坐标碎片
///
/// 这样做的收益主要体现在两个地方：
/// 1. 超大 shape 不再为每个 tile 反复做完整裁剪流程
/// 2. renderer 可以把"小 shape 的动态路径"和"大 shape 的预碎片路径"明确分开
pub fn prepare_large_shape_tile_fragments(
    scene: &Scene,
    tile_grid: &TileGridIndex,
    min_tile_span: usize,
) -> PreparedTileFragments {
    prepare_large_shape_tile_fragments_with_budget(
        scene,
        tile_grid,
        min_tile_span,
        PreparedFragmentBudget::generous_default(),
    )
}

pub fn prepare_large_shape_tile_fragments_with_budget(
    scene: &Scene,
    tile_grid: &TileGridIndex,
    min_tile_span: usize,
    budget: PreparedFragmentBudget,
) -> PreparedTileFragments {
    let min_tile_span = min_tile_span.max(2);
    let mut prepared = PreparedTileFragments::default();
    let mut tile_layers_seen: HashMap<TileId, BTreeSet<LayerId>> = HashMap::new();
    let mut prepared_fragment_budget_used = 0usize;
    let mut prepared_point_budget_used = 0usize;

    for (shape_index_raw, shape) in scene.shapes().iter().enumerate() {
        let shape_index = encode_shape_index(shape_index_raw);
        let tile_ids = tile_grid.tile_ids_for_bounds(shape.bounds);
        if tile_ids.len() < min_tile_span {
            continue;
        }

        let estimated_fragment_count =
            estimated_fragment_count_for_shape(shape.closed, &shape.points, tile_ids.len());
        let estimated_point_copies =
            estimated_point_copies_for_shape(shape.closed, &shape.points, tile_ids.len());
        if prepared_fragment_budget_used.saturating_add(estimated_fragment_count)
            > budget.max_fragments
            || prepared_point_budget_used.saturating_add(estimated_point_copies)
                > budget.max_point_copies
        {
            continue;
        }

        let mut produced_any_fragment = false;
        let mut produced_fragment_count = 0usize;
        let mut produced_point_count = 0usize;
        for tile_id in tile_ids {
            let tile_bounds = tile_grid.tile_bounds(tile_id);
            if shape.closed {
                let clipped = normalized_closed_points(&clip_closed_polygon_to_bounds(
                    &shape.points,
                    tile_bounds,
                ));
                if clipped.len() >= 3 {
                    prepared
                        .per_tile_layer
                        .entry((tile_id, shape.layer))
                        .or_default()
                        .push(PreparedTileFragment {
                            layer: shape.layer,
                            points: clipped,
                            closed: true,
                            stroke_width_world: shape.stroke_width_world,
                            outline_segments: clipped_closed_outline_segments(
                                &shape.points,
                                tile_bounds,
                            ),
                        });
                    tile_layers_seen
                        .entry(tile_id)
                        .or_default()
                        .insert(shape.layer);
                    produced_any_fragment = true;
                    produced_fragment_count += 1;
                    produced_point_count += prepared
                        .per_tile_layer
                        .get(&(tile_id, shape.layer))
                        .and_then(|fragments| fragments.last())
                        .map(|fragment| fragment.points.len())
                        .unwrap_or(0);
                }
            } else {
                for segment in shape.points.windows(2) {
                    if let Some((start, end)) =
                        clip_segment_to_bounds(segment[0], segment[1], tile_bounds)
                    {
                        prepared
                            .per_tile_layer
                            .entry((tile_id, shape.layer))
                            .or_default()
                            .push(PreparedTileFragment {
                                layer: shape.layer,
                                points: vec![start, end],
                                closed: false,
                                stroke_width_world: shape.stroke_width_world,
                                outline_segments: Vec::new(),
                            });
                        tile_layers_seen
                            .entry(tile_id)
                            .or_default()
                            .insert(shape.layer);
                        produced_any_fragment = true;
                        produced_fragment_count += 1;
                        produced_point_count += 2;
                    }
                }
            }
        }

        if produced_any_fragment {
            prepared.shape_indices.insert(shape_index);
            prepared_fragment_budget_used =
                prepared_fragment_budget_used.saturating_add(produced_fragment_count);
            prepared_point_budget_used =
                prepared_point_budget_used.saturating_add(produced_point_count);
        }
    }

    prepared.tile_layers = tile_layers_seen
        .into_iter()
        .map(|(tile_id, layers)| (tile_id, layers.into_iter().collect()))
        .collect();

    prepared
}

fn estimated_fragment_count_for_shape(closed: bool, points: &[Vec2], tile_span: usize) -> usize {
    if closed {
        tile_span
    } else {
        tile_span.saturating_mul(points.len().saturating_sub(1))
    }
}

fn estimated_point_copies_for_shape(closed: bool, points: &[Vec2], tile_span: usize) -> usize {
    if closed {
        tile_span.saturating_mul(points.len())
    } else {
        estimated_fragment_count_for_shape(closed, points, tile_span).saturating_mul(2)
    }
}

/// 为隐藏图层集合计算一个稳定 hash，用于缓存 key。
pub fn layer_draw_mode_hash_value(
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (layer, mode) in layer_draw_modes {
        hash ^= layer.layer as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        hash ^= layer.datatype as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        hash ^= mode.as_tag() as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// 按 layer 的 hatch preset 覆盖同样要进入缓存语义。
///
/// 这里故意单独做一个 hash，而不是和 draw mode 混在一起，
/// 这样测试可以更明确地区分：
/// - 是显示模式在变
/// - 还是 hatch 图案族在变
pub fn layer_hatch_style_hash_value(
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (layer, style) in layer_hatch_styles {
        hash ^= layer.layer as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        hash ^= layer.datatype as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        hash ^= build_hatch_style_signature(*style);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn effective_draw_mode(
    layer: LayerId,
    default: ClosedShapeDrawMode,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
) -> ClosedShapeDrawMode {
    layer_draw_modes.get(&layer).copied().unwrap_or(default)
}

fn effective_hatch_style(
    layer: LayerId,
    default: HatchStylePreset,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
) -> HatchStylePreset {
    layer_hatch_styles.get(&layer).copied().unwrap_or(default)
}

pub fn hidden_layers_hash_value(hidden_layers: &BTreeSet<LayerId>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for layer in hidden_layers {
        hash ^= layer.layer as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        hash ^= layer.datatype as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// 世界坐标转屏幕坐标。
///
/// 这是一个“完整版本”的变换：包含 zoom、pan 和 canvas origin。
pub fn world_to_screen(world: Vec2, camera: &Camera2D, canvas_origin: Vec2) -> Vec2 {
    canvas_origin + camera.pan() + world * camera.zoom()
}

/// 世界坐标只乘 zoom，不加平移。
///
/// 这样做是为了让 tile cache 在平移时更容易复用。
pub fn scaled_world_to_screen(world: Vec2, zoom: f32) -> Vec2 {
    world * zoom
}

/// 将物理像素 viewport 转成逻辑像素 viewport。
///
/// 这是为了解决 Retina / 高 DPI 下逻辑坐标和物理坐标不一致的问题。
pub fn logical_viewport_size(surface_pixels: Vec2, pixels_per_point: f32) -> Vec2 {
    Vec2::new(
        surface_pixels.x / pixels_per_point.max(1.0),
        surface_pixels.y / pixels_per_point.max(1.0),
    )
}

/// 根据当前 camera 反推出可见的 world bounds。
pub fn camera_visible_world_bounds(camera: &Camera2D, viewport_size: Vec2) -> Bounds {
    let inv_zoom = 1.0 / camera.zoom().max(f32::EPSILON);
    Bounds::new(
        -camera.pan().x * inv_zoom,
        -camera.pan().y * inv_zoom,
        (viewport_size.x - camera.pan().x) * inv_zoom,
        (viewport_size.y - camera.pan().y) * inv_zoom,
    )
}

/// 构建一帧级别的缓存 key。
pub fn build_render_cache_key(
    scene_revision: u64,
    camera: &Camera2D,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    canvas_origin: Vec2,
    canvas_size: Vec2,
    viewport_size: Vec2,
    draw_mode: ClosedShapeDrawMode,
    hatch_params: HatchParams,
) -> RenderCacheKey {
    build_render_cache_key_with_hatch_styles(
        scene_revision,
        camera,
        hidden_layers,
        layer_draw_modes,
        &BTreeMap::new(),
        canvas_origin,
        canvas_size,
        viewport_size,
        draw_mode,
        hatch_params,
        DEFAULT_HATCH_STYLE_PRESET,
    )
}

/// 构建一帧级别缓存 key，并把 hatch preset 语义也纳入其中。
pub fn build_render_cache_key_with_hatch_styles(
    scene_revision: u64,
    camera: &Camera2D,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
    canvas_origin: Vec2,
    canvas_size: Vec2,
    viewport_size: Vec2,
    draw_mode: ClosedShapeDrawMode,
    hatch_params: HatchParams,
    hatch_style: HatchStylePreset,
) -> RenderCacheKey {
    RenderCacheKey {
        scene_revision,
        pan_x_bits: camera.pan().x.to_bits(),
        pan_y_bits: camera.pan().y.to_bits(),
        zoom_bits: camera.zoom().to_bits(),
        canvas_origin_x_bits: canvas_origin.x.to_bits(),
        canvas_origin_y_bits: canvas_origin.y.to_bits(),
        canvas_width_bits: canvas_size.x.to_bits(),
        canvas_height_bits: canvas_size.y.to_bits(),
        viewport_width_bits: viewport_size.x.to_bits(),
        viewport_height_bits: viewport_size.y.to_bits(),
        hidden_layers_hash: hidden_layers_hash_value(hidden_layers)
            ^ layer_draw_mode_hash_value(layer_draw_modes)
            ^ draw_mode.as_tag() as u64
            ^ build_hatch_signature(hatch_params)
            ^ build_hatch_style_signature(hatch_style)
            ^ layer_hatch_style_hash_value(layer_hatch_styles),
    }
}

/// 将一个 shape 的点集完整投影到屏幕。
pub fn project_points(shape: &RectShape, camera: &Camera2D, canvas_origin: Vec2) -> Vec<Vec2> {
    shape
        .points
        .iter()
        .copied()
        .map(|point| world_to_screen(point, camera, canvas_origin))
        .collect()
}

/// 为测试和回归验证构建最终 NDC 顶点。
///
/// 这个函数更偏“测试辅助入口”，
/// 因为真实主渲染路径现在会先生成 scaled 顶点，再在 shader 里平移。
pub fn build_line_vertices(
    scene: &Scene,
    camera: &Camera2D,
    canvas_origin: Vec2,
    viewport_size: Vec2,
    hidden_layers: &BTreeSet<LayerId>,
) -> Vec<LineVertex> {
    build_scene_vertices(
        scene,
        camera,
        canvas_origin,
        viewport_size,
        hidden_layers,
        &BTreeMap::new(),
        ClosedShapeDrawMode::Outline,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    )
}

/// 为测试和回归验证构建最终 NDC 顶点，并允许指定闭合图形的显示模式。
pub fn build_scene_vertices(
    scene: &Scene,
    camera: &Camera2D,
    canvas_origin: Vec2,
    viewport_size: Vec2,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    draw_mode: ClosedShapeDrawMode,
    _hatch_params: HatchParams,
) -> Vec<LineVertex> {
    let spatial_index = ShapeSpatialIndex::build(scene);
    let visible_world = camera_visible_world_bounds(camera, viewport_size);
    let scaled_vertices = build_scaled_scene_vertices_for_indices(
        scene,
        camera.zoom(),
        hidden_layers,
        layer_draw_modes,
        &query_visible_shape_indices(scene, &spatial_index, visible_world),
        draw_mode,
    );
    transform_vertices_to_ndc(
        &scaled_vertices,
        camera.pan() + canvas_origin,
        viewport_size,
    )
}

/// 针对一组 shape 索引生成“缩放后的逻辑屏幕坐标”顶点。
pub fn build_scaled_line_vertices_for_indices(
    scene: &Scene,
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    shape_indices: &[usize],
) -> Vec<LineVertex> {
    build_scaled_scene_vertices_for_indices(
        scene,
        zoom,
        hidden_layers,
        &BTreeMap::new(),
        shape_indices,
        ClosedShapeDrawMode::Outline,
    )
}

/// 针对一组 shape 索引生成“缩放后的逻辑屏幕坐标”顶点，并允许指定闭合图形显示模式。
pub fn build_scaled_scene_vertices_for_indices(
    scene: &Scene,
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    shape_indices: &[usize],
    draw_mode: ClosedShapeDrawMode,
) -> Vec<LineVertex> {
    build_scaled_scene_vertices_for_indices_with_hatch_styles(
        scene,
        zoom,
        hidden_layers,
        layer_draw_modes,
        &BTreeMap::new(),
        shape_indices,
        draw_mode,
        DEFAULT_HATCH_STYLE_PRESET,
    )
}

/// 针对一组 shape 索引生成“缩放后的逻辑屏幕坐标”顶点，并显式编码 hatch preset。
pub fn build_scaled_scene_vertices_for_indices_with_hatch_styles(
    scene: &Scene,
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
    shape_indices: &[usize],
    draw_mode: ClosedShapeDrawMode,
    hatch_style: HatchStylePreset,
) -> Vec<LineVertex> {
    build_scaled_scene_vertices_for_tile(
        scene,
        zoom,
        hidden_layers,
        layer_draw_modes,
        shape_indices,
        draw_mode,
        None,
        layer_hatch_styles,
        hatch_style,
    )
}

/// 针对一组 shape 索引生成某个 tile 真正需要的局部几何。
///
/// 这是当前结构性性能优化的关键一步：
/// 以前只要某个 shape 命中一个 tile，就会把整块 shape 顶点塞进该 tile cache；
/// 现在我们会先按 tile bounds 做局部裁剪，再为这个 tile 生成真正需要画的几何。
pub fn build_scaled_scene_vertices_for_tile(
    scene: &Scene,
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    shape_indices: &[usize],
    draw_mode: ClosedShapeDrawMode,
    tile_bounds: Option<Bounds>,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
    hatch_style: HatchStylePreset,
) -> Vec<LineVertex> {
    let mut vertices = Vec::new();
    let scaled_tile_bounds = tile_bounds.map(|bounds| scale_bounds(bounds, zoom));
    for &shape_index in shape_indices {
        let shape = &scene.shapes()[shape_index];
        if hidden_layers.contains(&shape.layer) {
            continue;
        }

        let effective_mode = effective_draw_mode(shape.layer, draw_mode, layer_draw_modes);
        let effective_hatch_style =
            effective_hatch_style(shape.layer, hatch_style, layer_hatch_styles);
        emit_scaled_shape_vertices(
            &mut vertices,
            shape.layer,
            &shape.points,
            shape.closed,
            shape.stroke_width_world,
            zoom,
            effective_mode,
            scaled_tile_bounds,
            effective_hatch_style,
        );
    }
    vertices
}

/// 为已经完成世界坐标预碎片化的几何生成缩放后的顶点。
///
/// 这条路径不再需要按 tile 现场裁剪，
/// 因为碎片本身已经保证只覆盖某一个 tile 的局部区域。
pub fn build_scaled_scene_vertices_for_prepared_fragments(
    fragments: &[PreparedTileFragment],
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    draw_mode: ClosedShapeDrawMode,
) -> Vec<LineVertex> {
    build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles(
        fragments,
        zoom,
        hidden_layers,
        layer_draw_modes,
        &BTreeMap::new(),
        draw_mode,
        DEFAULT_HATCH_STYLE_PRESET,
    )
}

/// 为预碎片化结果生成顶点，并把每个 layer 的 hatch preset 一起编码进 fill 顶点。
pub fn build_scaled_scene_vertices_for_prepared_fragments_with_hatch_styles(
    fragments: &[PreparedTileFragment],
    zoom: f32,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
    draw_mode: ClosedShapeDrawMode,
    hatch_style: HatchStylePreset,
) -> Vec<LineVertex> {
    let mut vertices = Vec::new();
    for fragment in fragments {
        if hidden_layers.contains(&fragment.layer) {
            continue;
        }
        let effective_mode = effective_draw_mode(fragment.layer, draw_mode, layer_draw_modes);
        let effective_hatch_style =
            effective_hatch_style(fragment.layer, hatch_style, layer_hatch_styles);
        if fragment.closed {
            emit_scaled_prepared_closed_fragment_vertices(
                &mut vertices,
                fragment,
                zoom,
                effective_mode,
                effective_hatch_style,
            );
        } else {
            emit_scaled_shape_vertices(
                &mut vertices,
                fragment.layer,
                &fragment.points,
                fragment.closed,
                fragment.stroke_width_world,
                zoom,
                effective_mode,
                None,
                effective_hatch_style,
            );
        }
    }
    vertices
}

fn emit_scaled_shape_vertices(
    vertices: &mut Vec<LineVertex>,
    layer: LayerId,
    source_points: &[Vec2],
    closed: bool,
    stroke_width_world: Option<f32>,
    zoom: f32,
    effective_mode: ClosedShapeDrawMode,
    scaled_tile_bounds: Option<Bounds>,
    hatch_style: HatchStylePreset,
) {
    let outline_color = layer_color(layer);
    let width = stroke_width_for_layer(layer, stroke_width_world, zoom);

    if closed {
        let outline_points: Vec<Vec2> = source_points
            .iter()
            .copied()
            .map(|point| scaled_world_to_screen(point, zoom))
            .collect();
        let mut fill_points = outline_points.clone();
        if let Some(tile_bounds) = scaled_tile_bounds {
            fill_points = clip_closed_polygon_to_bounds(&fill_points, tile_bounds);
        }
        let outline_points = normalized_closed_points(&outline_points);
        let fill_points = normalized_closed_points(&fill_points);
        if let Some(marker_center) = tiny_shape_marker_center(&outline_points) {
            emit_tiny_closed_shape_marker(
                vertices,
                marker_center,
                outline_color,
                fill_color(layer),
                effective_mode,
                hatch_style,
            );
            return;
        }
        let suppress_fill = matches!(
            effective_mode,
            ClosedShapeDrawMode::Hatch | ClosedShapeDrawMode::HatchOutline
        ) && should_suppress_fill_for_small_closed_shape(&outline_points);
        let coarse_lod_points = if should_use_coarse_closed_shape_lod(&fill_points) {
            coarse_closed_shape_points(&fill_points)
        } else if should_use_coarse_closed_shape_lod(&outline_points) {
            coarse_closed_shape_points(&outline_points)
        } else {
            None
        };

        match effective_mode {
            ClosedShapeDrawMode::Outline => {
                if let Some(coarse_points) = coarse_lod_points.as_ref() {
                    emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                } else if let Some(tile_bounds) = scaled_tile_bounds {
                    emit_clipped_closed_outline_segments(
                        vertices,
                        &outline_points,
                        tile_bounds,
                        width,
                        outline_color,
                    );
                } else if outline_points.len() >= 3 {
                    emit_outline_segments(vertices, &outline_points, true, width, outline_color);
                }
            }
            ClosedShapeDrawMode::Hatch => {
                if suppress_fill {
                    if let Some(coarse_points) = coarse_lod_points.as_ref() {
                        emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                    } else if let Some(tile_bounds) = scaled_tile_bounds {
                        emit_clipped_closed_outline_segments(
                            vertices,
                            &outline_points,
                            tile_bounds,
                            width,
                            outline_color,
                        );
                    } else if outline_points.len() >= 3 {
                        emit_outline_segments(
                            vertices,
                            &outline_points,
                            true,
                            width,
                            outline_color,
                        );
                    }
                } else if let Some(coarse_points) = coarse_lod_points.as_ref() {
                    emit_polygon_fill(vertices, coarse_points, fill_color(layer), hatch_style);
                } else if fill_points.len() >= 3 {
                    emit_polygon_fill(vertices, &fill_points, fill_color(layer), hatch_style);
                }
            }
            ClosedShapeDrawMode::HatchOutline => {
                if suppress_fill {
                    if let Some(coarse_points) = coarse_lod_points.as_ref() {
                        emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                    } else if let Some(tile_bounds) = scaled_tile_bounds {
                        emit_clipped_closed_outline_segments(
                            vertices,
                            &outline_points,
                            tile_bounds,
                            width,
                            outline_color,
                        );
                    } else if outline_points.len() >= 3 {
                        emit_outline_segments(
                            vertices,
                            &outline_points,
                            true,
                            width,
                            outline_color,
                        );
                    }
                } else if let Some(coarse_points) = coarse_lod_points.as_ref() {
                    emit_polygon_fill(vertices, coarse_points, fill_color(layer), hatch_style);
                    emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                } else {
                    if fill_points.len() >= 3 {
                        emit_polygon_fill(vertices, &fill_points, fill_color(layer), hatch_style);
                    }
                    if let Some(tile_bounds) = scaled_tile_bounds {
                        emit_clipped_closed_outline_segments(
                            vertices,
                            &outline_points,
                            tile_bounds,
                            width,
                            outline_color,
                        );
                    } else if outline_points.len() >= 3 {
                        emit_outline_segments(
                            vertices,
                            &outline_points,
                            true,
                            width,
                            outline_color,
                        );
                    }
                }
            }
        }
    } else {
        let points: Vec<Vec2> = source_points
            .iter()
            .copied()
            .map(|point| scaled_world_to_screen(point, zoom))
            .collect();
        if points.len() < 2 {
            return;
        }
        if let Some(marker_segment) = tiny_open_shape_marker_segment(&points) {
            emit_segment(
                vertices,
                marker_segment[0],
                marker_segment[1],
                width,
                outline_color,
            );
            return;
        }
        let coarse_points = coarse_open_polyline_points(&points);
        if let Some(coarse_points) = coarse_points.as_ref() {
            emit_outline_segments(vertices, coarse_points, false, width, outline_color);
        } else if let Some(tile_bounds) = scaled_tile_bounds {
            emit_clipped_polyline_segments(vertices, &points, tile_bounds, width, outline_color);
        } else {
            emit_outline_segments(vertices, &points, false, width, outline_color);
        }
    }
}

fn screen_bounds_from_points(points: &[Vec2]) -> Option<Bounds> {
    let first = *points.first()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for point in points.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Some(Bounds::new(min_x, min_y, max_x, max_y))
}

fn should_use_coarse_closed_shape_lod(points: &[Vec2]) -> bool {
    if points.len() < CLOSED_SHAPE_LOD_MIN_POINTS {
        return false;
    }
    let Some(bounds) = screen_bounds_from_points(points) else {
        return false;
    };
    bounds.width().max(bounds.height()) <= CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT
}

fn tiny_shape_marker_center(points: &[Vec2]) -> Option<Vec2> {
    let bounds = screen_bounds_from_points(points)?;
    if bounds.width().max(bounds.height()) > TINY_SHAPE_MARKER_MAX_SCREEN_EXTENT {
        return None;
    }
    Some(Vec2::new(
        (bounds.min_x + bounds.max_x) * 0.5,
        (bounds.min_y + bounds.max_y) * 0.5,
    ))
}

fn tiny_open_shape_marker_segment(points: &[Vec2]) -> Option<[Vec2; 2]> {
    let center = tiny_shape_marker_center(points)?;
    let half_extent = TINY_SHAPE_MARKER_SCREEN_SIZE * 0.5;
    Some([
        center + Vec2::new(-half_extent, 0.0),
        center + Vec2::new(half_extent, 0.0),
    ])
}

fn should_suppress_fill_for_small_closed_shape(points: &[Vec2]) -> bool {
    let Some(bounds) = screen_bounds_from_points(points) else {
        return false;
    };
    bounds.width().max(bounds.height()) <= SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT
}

fn coarse_closed_shape_target_points(points: &[Vec2]) -> Option<usize> {
    let bounds = screen_bounds_from_points(points)?;
    let extent = bounds
        .width()
        .max(bounds.height())
        .clamp(0.0, CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT);
    let ratio = if CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT <= f32::EPSILON {
        1.0
    } else {
        extent / CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT
    };
    let target = CLOSED_SHAPE_LOD_MIN_TARGET_POINTS as f32
        + (CLOSED_SHAPE_LOD_MAX_TARGET_POINTS - CLOSED_SHAPE_LOD_MIN_TARGET_POINTS) as f32 * ratio;
    Some(target.round() as usize)
}

fn coarse_closed_shape_points(points: &[Vec2]) -> Option<Vec<Vec2>> {
    let normalized = normalized_closed_points(points);
    if normalized.len() < 3 {
        return None;
    }
    let target = coarse_closed_shape_target_points(&normalized)?
        .max(4)
        .min(normalized.len());
    if normalized.len() <= target {
        return Some(normalized);
    }

    let step = normalized.len() as f32 / target as f32;
    let mut simplified = Vec::with_capacity(target);
    let mut seen = std::collections::HashSet::new();
    for index in 0..target {
        let sample = ((index as f32 * step).round() as usize).min(normalized.len() - 1);
        let point = normalized[sample];
        let key = (point.x.to_bits(), point.y.to_bits());
        if seen.insert(key) {
            simplified.push(point);
        }
    }
    if simplified.len() < 3 {
        return Some(normalized);
    }
    Some(simplified)
}

fn coarse_open_polyline_target_points(points: &[Vec2]) -> Option<usize> {
    let bounds = screen_bounds_from_points(points)?;
    let extent = bounds
        .width()
        .max(bounds.height())
        .clamp(0.0, POLYLINE_LOD_MAX_SCREEN_EXTENT);
    let ratio = if POLYLINE_LOD_MAX_SCREEN_EXTENT <= f32::EPSILON {
        1.0
    } else {
        extent / POLYLINE_LOD_MAX_SCREEN_EXTENT
    };
    let target = POLYLINE_LOD_MIN_TARGET_POINTS as f32
        + (POLYLINE_LOD_MAX_TARGET_POINTS - POLYLINE_LOD_MIN_TARGET_POINTS) as f32 * ratio;
    Some(target.round() as usize)
}

fn coarse_open_polyline_points(points: &[Vec2]) -> Option<Vec<Vec2>> {
    if points.len() < POLYLINE_LOD_MIN_POINTS {
        return None;
    }
    let bounds = screen_bounds_from_points(points)?;
    if bounds.width().max(bounds.height()) > POLYLINE_LOD_MAX_SCREEN_EXTENT {
        return None;
    }
    let target = coarse_open_polyline_target_points(points)?
        .max(2)
        .min(points.len());
    if points.len() <= target {
        return Some(points.to_vec());
    }

    let step = (points.len() - 1) as f32 / (target - 1) as f32;
    let mut simplified = Vec::with_capacity(target);
    simplified.push(points[0]);
    let mut seen = std::collections::HashSet::new();
    seen.insert((points[0].x.to_bits(), points[0].y.to_bits()));
    for index in 1..(target - 1) {
        let sample = ((index as f32 * step).round() as usize).min(points.len() - 2);
        let point = points[sample];
        let key = (point.x.to_bits(), point.y.to_bits());
        if seen.insert(key) {
            simplified.push(point);
        }
    }
    if simplified.last().copied() != points.last().copied() {
        simplified.push(*points.last().expect("polyline last point"));
    }
    if simplified.len() < 2 {
        return Some(points.to_vec());
    }
    Some(simplified)
}

fn scale_bounds(bounds: Bounds, zoom: f32) -> Bounds {
    Bounds::new(
        bounds.min_x * zoom,
        bounds.min_y * zoom,
        bounds.max_x * zoom,
        bounds.max_y * zoom,
    )
}

fn clip_closed_polygon_to_bounds(points: &[Vec2], bounds: Bounds) -> Vec<Vec2> {
    let mut output = normalized_closed_points(points);
    if output.len() < 3 {
        return Vec::new();
    }

    output = clip_polygon_against_edge(
        output,
        |p| p.x >= bounds.min_x,
        |a, b| intersect_vertical(a, b, bounds.min_x),
    );
    output = clip_polygon_against_edge(
        output,
        |p| p.x <= bounds.max_x,
        |a, b| intersect_vertical(a, b, bounds.max_x),
    );
    output = clip_polygon_against_edge(
        output,
        |p| p.y >= bounds.min_y,
        |a, b| intersect_horizontal(a, b, bounds.min_y),
    );
    output = clip_polygon_against_edge(
        output,
        |p| p.y <= bounds.max_y,
        |a, b| intersect_horizontal(a, b, bounds.max_y),
    );
    normalized_closed_points(&output)
}

fn clip_polygon_against_edge<FInside, FIntersect>(
    input: Vec<Vec2>,
    inside: FInside,
    intersect: FIntersect,
) -> Vec<Vec2>
where
    FInside: Fn(Vec2) -> bool,
    FIntersect: Fn(Vec2, Vec2) -> Vec2,
{
    if input.is_empty() {
        return input;
    }

    let mut output = Vec::new();
    let mut previous = *input.last().expect("non-empty polygon");
    let mut previous_inside = inside(previous);
    for current in input {
        let current_inside = inside(current);
        match (previous_inside, current_inside) {
            (true, true) => output.push(current),
            (true, false) => output.push(intersect(previous, current)),
            (false, true) => {
                output.push(intersect(previous, current));
                output.push(current);
            }
            (false, false) => {}
        }
        previous = current;
        previous_inside = current_inside;
    }
    output
}

fn intersect_vertical(a: Vec2, b: Vec2, x: f32) -> Vec2 {
    let dx = b.x - a.x;
    if dx.abs() <= f32::EPSILON {
        return Vec2::new(x, a.y);
    }
    let t = (x - a.x) / dx;
    Vec2::new(x, a.y + (b.y - a.y) * t)
}

fn intersect_horizontal(a: Vec2, b: Vec2, y: f32) -> Vec2 {
    let dy = b.y - a.y;
    if dy.abs() <= f32::EPSILON {
        return Vec2::new(a.x, y);
    }
    let t = (y - a.y) / dy;
    Vec2::new(a.x + (b.x - a.x) * t, y)
}

fn emit_scaled_prepared_closed_fragment_vertices(
    vertices: &mut Vec<LineVertex>,
    fragment: &PreparedTileFragment,
    zoom: f32,
    effective_mode: ClosedShapeDrawMode,
    hatch_style: HatchStylePreset,
) {
    let outline_color = layer_color(fragment.layer);
    let width = stroke_width_for_layer(fragment.layer, fragment.stroke_width_world, zoom);
    let fill_points: Vec<Vec2> = fragment
        .points
        .iter()
        .copied()
        .map(|point| scaled_world_to_screen(point, zoom))
        .collect();
    let fill_points = normalized_closed_points(&fill_points);
    if let Some(marker_center) = tiny_shape_marker_center(&fill_points) {
        emit_tiny_closed_shape_marker(
            vertices,
            marker_center,
            outline_color,
            fill_color(fragment.layer),
            effective_mode,
            hatch_style,
        );
        return;
    }
    let suppress_fill = matches!(
        effective_mode,
        ClosedShapeDrawMode::Hatch | ClosedShapeDrawMode::HatchOutline
    ) && should_suppress_fill_for_small_closed_shape(&fill_points);
    let coarse_lod_points = should_use_coarse_closed_shape_lod(&fill_points)
        .then(|| coarse_closed_shape_points(&fill_points))
        .flatten();

    match effective_mode {
        ClosedShapeDrawMode::Outline => {
            if let Some(coarse_points) = coarse_lod_points.as_ref() {
                emit_outline_segments(vertices, coarse_points, true, width, outline_color);
            } else {
                emit_scaled_outline_segment_pairs(
                    vertices,
                    &fragment.outline_segments,
                    zoom,
                    width,
                    outline_color,
                );
            }
        }
        ClosedShapeDrawMode::Hatch => {
            if suppress_fill {
                if let Some(coarse_points) = coarse_lod_points.as_ref() {
                    emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                } else {
                    emit_scaled_outline_segment_pairs(
                        vertices,
                        &fragment.outline_segments,
                        zoom,
                        width,
                        outline_color,
                    );
                }
            } else if let Some(coarse_points) = coarse_lod_points.as_ref() {
                emit_polygon_fill(
                    vertices,
                    coarse_points,
                    fill_color(fragment.layer),
                    hatch_style,
                );
            } else if fill_points.len() >= 3 {
                emit_polygon_fill(
                    vertices,
                    &fill_points,
                    fill_color(fragment.layer),
                    hatch_style,
                );
            }
        }
        ClosedShapeDrawMode::HatchOutline => {
            if suppress_fill {
                if let Some(coarse_points) = coarse_lod_points.as_ref() {
                    emit_outline_segments(vertices, coarse_points, true, width, outline_color);
                } else {
                    emit_scaled_outline_segment_pairs(
                        vertices,
                        &fragment.outline_segments,
                        zoom,
                        width,
                        outline_color,
                    );
                }
            } else if let Some(coarse_points) = coarse_lod_points.as_ref() {
                emit_polygon_fill(
                    vertices,
                    coarse_points,
                    fill_color(fragment.layer),
                    hatch_style,
                );
                emit_outline_segments(vertices, coarse_points, true, width, outline_color);
            } else {
                if fill_points.len() >= 3 {
                    emit_polygon_fill(
                        vertices,
                        &fill_points,
                        fill_color(fragment.layer),
                        hatch_style,
                    );
                }
                emit_scaled_outline_segment_pairs(
                    vertices,
                    &fragment.outline_segments,
                    zoom,
                    width,
                    outline_color,
                );
            }
        }
    }
}

fn emit_tiny_closed_shape_marker(
    vertices: &mut Vec<LineVertex>,
    center: Vec2,
    outline_color: [f32; 4],
    fill_color_value: [f32; 4],
    effective_mode: ClosedShapeDrawMode,
    hatch_style: HatchStylePreset,
) {
    let half_extent = TINY_SHAPE_MARKER_SCREEN_SIZE * 0.5;
    let corners = [
        center + Vec2::new(-half_extent, -half_extent),
        center + Vec2::new(half_extent, -half_extent),
        center + Vec2::new(half_extent, half_extent),
        center + Vec2::new(-half_extent, half_extent),
    ];

    match effective_mode {
        ClosedShapeDrawMode::Outline => {
            emit_outline_segments(vertices, &corners, true, 1.0, outline_color);
        }
        ClosedShapeDrawMode::Hatch | ClosedShapeDrawMode::HatchOutline => {
            emit_polygon_fill(vertices, &corners, fill_color_value, hatch_style);
        }
    }
}

fn emit_scaled_outline_segment_pairs(
    vertices: &mut Vec<LineVertex>,
    segments: &[[Vec2; 2]],
    zoom: f32,
    width: f32,
    color: [f32; 4],
) {
    for segment in segments {
        emit_segment(
            vertices,
            scaled_world_to_screen(segment[0], zoom),
            scaled_world_to_screen(segment[1], zoom),
            width,
            color,
        );
    }
}

fn emit_clipped_closed_outline_segments(
    vertices: &mut Vec<LineVertex>,
    points: &[Vec2],
    bounds: Bounds,
    width: f32,
    color: [f32; 4],
) {
    if points.len() < 2 {
        return;
    }
    for (start, end) in closed_segments(points) {
        if let Some((start, end)) = clip_segment_to_bounds(start, end, bounds) {
            emit_segment(vertices, start, end, width, color);
        }
    }
}

fn emit_clipped_polyline_segments(
    vertices: &mut Vec<LineVertex>,
    points: &[Vec2],
    bounds: Bounds,
    width: f32,
    color: [f32; 4],
) {
    for segment in points.windows(2) {
        if let Some((start, end)) = clip_segment_to_bounds(segment[0], segment[1], bounds) {
            emit_segment(vertices, start, end, width, color);
        }
    }
}

fn closed_segments(points: &[Vec2]) -> Vec<(Vec2, Vec2)> {
    let points = normalized_closed_points(points);
    if points.len() < 2 {
        return Vec::new();
    }
    let mut segments = Vec::with_capacity(points.len());
    for i in 0..points.len() {
        segments.push((points[i], points[(i + 1) % points.len()]));
    }
    segments
}

fn clipped_closed_outline_segments(points: &[Vec2], bounds: Bounds) -> Vec<[Vec2; 2]> {
    closed_segments(points)
        .into_iter()
        .filter_map(|(start, end)| clip_segment_to_bounds(start, end, bounds).map(|(s, e)| [s, e]))
        .collect()
}

fn clip_segment_to_bounds(start: Vec2, end: Vec2, bounds: Bounds) -> Option<(Vec2, Vec2)> {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let p = [-dx, dx, -dy, dy];
    let q = [
        start.x - bounds.min_x,
        bounds.max_x - start.x,
        start.y - bounds.min_y,
        bounds.max_y - start.y,
    ];

    let mut t0 = 0.0f32;
    let mut t1 = 1.0f32;
    for i in 0..4 {
        if p[i].abs() <= f32::EPSILON {
            if q[i] < 0.0 {
                return None;
            }
            continue;
        }
        let r = q[i] / p[i];
        if p[i] < 0.0 {
            t0 = t0.max(r);
        } else {
            t1 = t1.min(r);
        }
        if t0 > t1 {
            return None;
        }
    }

    Some((
        start + Vec2::new(dx, dy) * t0,
        start + Vec2::new(dx, dy) * t1,
    ))
}

/// 将逻辑屏幕坐标顶点转成 NDC 顶点。
pub fn transform_vertices_to_ndc(
    vertices: &[LineVertex],
    translation: Vec2,
    viewport_size: Vec2,
) -> Vec<LineVertex> {
    vertices
        .iter()
        .map(|vertex| {
            let screen = Vec2::from_array(vertex.position) + translation;
            LineVertex {
                position: screen_to_ndc(screen, viewport_size),
                color: vertex.color,
                kind: vertex.kind,
                hatch_style: vertex.hatch_style,
            }
        })
        .collect()
}

/// 计算一组 NDC 顶点的 bounds，主要用于测试。
pub fn ndc_bounds(vertices: &[LineVertex]) -> Option<Bounds> {
    let first = vertices.first()?;
    let mut min_x = first.position[0];
    let mut min_y = first.position[1];
    let mut max_x = first.position[0];
    let mut max_y = first.position[1];

    for vertex in vertices.iter().skip(1) {
        min_x = min_x.min(vertex.position[0]);
        min_y = min_y.min(vertex.position[1]);
        max_x = max_x.max(vertex.position[0]);
        max_y = max_y.max(vertex.position[1]);
    }

    Some(Bounds::new(min_x, min_y, max_x, max_y))
}

/// 发射一组轮廓边。
fn emit_outline_segments(
    vertices: &mut Vec<LineVertex>,
    points: &[Vec2],
    closed: bool,
    width: f32,
    color: [f32; 4],
) {
    for segment in points.windows(2) {
        emit_segment(vertices, segment[0], segment[1], width, color);
    }
    if closed && points.len() >= 3 {
        emit_segment(
            vertices,
            *points.last().expect("closed shape end"),
            points[0],
            width,
            color,
        );
    }
}

/// 给闭合图形生成填充三角形。
///
/// 这里不再使用最简单的三角扇。
/// 原因是版图里常见的 boundary 往往是长而弯的凹多边形，
/// 三角扇会把轮廓外的大片区域也错误盖进去。
///
/// 当前改成耳切（ear clipping）三角化：
/// - 对学习项目足够直观
/// - 能处理简单凹多边形
/// - 对我们现在这种几百个点的边界也完全够用
fn emit_polygon_fill(
    vertices: &mut Vec<LineVertex>,
    points: &[Vec2],
    color: [f32; 4],
    hatch_style: HatchStylePreset,
) {
    let local_points = points
        .first()
        .copied()
        .map(|origin| {
            points
                .iter()
                .copied()
                .map(|point| point - origin)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let Some(indices) = triangulate_polygon(&local_points) else {
        return;
    };

    for [a, b, c] in indices {
        for point in [points[a], points[b], points[c]] {
            vertices.push(LineVertex {
                position: point.to_array(),
                color,
                kind: VERTEX_KIND_HATCH_FILL,
                hatch_style: hatch_style.as_vertex_code(),
            });
        }
    }
}

/// 对闭合点列做轻量归一化。
///
/// 有些来源格式会把首点再重复一遍放到末尾，
/// 对 outline 来说问题不大，但对 fill 三角扇来说会产生退化三角形。
/// 所以这里会把“末尾重复首点”的情况裁掉。
fn normalized_closed_points(points: &[Vec2]) -> Vec<Vec2> {
    let mut normalized: Vec<Vec2> = Vec::with_capacity(points.len());
    for point in points.iter().copied() {
        let duplicated_with_previous = normalized
            .last()
            .map(|previous| (point - *previous).length_squared() <= f32::EPSILON)
            .unwrap_or(false);
        if !duplicated_with_previous {
            normalized.push(point);
        }
    }

    if normalized.len() >= 2
        && normalized
            .first()
            .zip(normalized.last())
            .map(|(first, last)| (*first - *last).length_squared() <= f32::EPSILON)
            .unwrap_or(false)
    {
        normalized.pop();
    }
    normalized
}

/// 用耳切法把一个简单多边形拆成三角形索引。
fn triangulate_polygon(points: &[Vec2]) -> Option<Vec<[usize; 3]>> {
    if points.len() < 3 {
        return None;
    }

    let signed_area = polygon_signed_area(points);
    if signed_area.abs() <= f32::EPSILON {
        return None;
    }

    // 耳切通常按逆时针点序更直观；如果输入是顺时针，就先翻转索引顺序。
    let mut remaining: Vec<usize> = if signed_area > 0.0 {
        (0..points.len()).collect()
    } else {
        (0..points.len()).rev().collect()
    };

    let mut triangles = Vec::with_capacity(points.len().saturating_sub(2));
    let mut guard = 0usize;
    while remaining.len() > 3 && guard < points.len() * points.len() {
        guard += 1;
        let mut ear_found = false;

        for i in 0..remaining.len() {
            let prev = remaining[(i + remaining.len() - 1) % remaining.len()];
            let curr = remaining[i];
            let next = remaining[(i + 1) % remaining.len()];
            let a = points[prev];
            let b = points[curr];
            let c = points[next];

            if !is_convex_corner(a, b, c) {
                continue;
            }

            if remaining.iter().copied().any(|idx| {
                idx != prev && idx != curr && idx != next && point_in_triangle(points[idx], a, b, c)
            }) {
                continue;
            }

            triangles.push([prev, curr, next]);
            remaining.remove(i);
            ear_found = true;
            break;
        }

        if !ear_found {
            return None;
        }
    }

    if remaining.len() == 3 {
        triangles.push([remaining[0], remaining[1], remaining[2]]);
    }

    Some(triangles)
}

fn polygon_signed_area(points: &[Vec2]) -> f32 {
    let mut area = 0.0;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        area += a.x * b.y - b.x * a.y;
    }
    area * 0.5
}

fn is_convex_corner(a: Vec2, b: Vec2, c: Vec2) -> bool {
    (b - a).perp_dot(c - b) > 1e-5
}

fn point_in_triangle(point: Vec2, a: Vec2, b: Vec2, c: Vec2) -> bool {
    let ab = (b - a).perp_dot(point - a);
    let bc = (c - b).perp_dot(point - b);
    let ca = (a - c).perp_dot(point - c);
    (ab >= -1e-5 && bc >= -1e-5 && ca >= -1e-5) || (ab <= 1e-5 && bc <= 1e-5 && ca <= 1e-5)
}

/// 把一条线段膨胀成两个三角形。
///
/// 这是当前 demo 渲染粗线最直接的方法：
/// 不依赖图形 API 的“原生线宽”，而是自己生成带宽度的三角形。
/// 这样跨平台表现更稳定，也更接近以后做真实几何批渲染的思路。
fn emit_segment(
    vertices: &mut Vec<LineVertex>,
    start: Vec2,
    end: Vec2,
    width: f32,
    color: [f32; 4],
) {
    let delta = end - start;
    if delta.length_squared() <= f32::EPSILON {
        return;
    }

    let normal = Vec2::new(-delta.y, delta.x).normalize() * (width.max(1.0) * 0.5);
    let corners = [start - normal, start + normal, end + normal, end - normal];
    let indices = [[0usize, 1usize, 2usize], [0usize, 2usize, 3usize]];

    for triangle in indices {
        for index in triangle {
            vertices.push(LineVertex {
                position: corners[index].to_array(),
                color,
                kind: VERTEX_KIND_OUTLINE,
                hatch_style: DEFAULT_VERTEX_HATCH_STYLE,
            });
        }
    }
}

/// 将 UI 中的 tile grid 值钳制到安全范围。
fn clamp_tile_grid_divisions(divisions: u32) -> i32 {
    divisions.clamp(MIN_TILE_GRID_DIVISIONS, MAX_TILE_GRID_DIVISIONS) as i32
}

/// 根据连续坐标值找到它所属的网格单元。
fn cell_for(value: f32, origin: f32, cell_extent: f32, cell_count: i32) -> i32 {
    (((value - origin) / cell_extent).floor() as i32).clamp(0, cell_count - 1)
}

/// 屏幕坐标转 NDC。
fn screen_to_ndc(position: Vec2, viewport_size: Vec2) -> [f32; 2] {
    let safe_width = viewport_size.x.max(1.0);
    let safe_height = viewport_size.y.max(1.0);
    [
        (position.x / safe_width) * 2.0 - 1.0,
        1.0 - (position.y / safe_height) * 2.0,
    ]
}

/// 根据 layer 生成颜色。
///
/// 这里混合了两种策略：
/// - 对少数常见测试层给固定颜色，便于你建立视觉记忆
/// - 对其他层做伪随机稳定着色，避免所有层都长得一样
fn layer_color(layer: LayerId) -> [f32; 4] {
    let rgb = base_layer_rgb(layer);

    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        1.0,
    ]
}

/// 填充色和轮廓色共用同一套基色，但填充会带透明度，
/// 这样既能看见 layer 面积，又不至于把内部结构完全盖住。
fn fill_color(layer: LayerId) -> [f32; 4] {
    let rgb = base_layer_rgb(layer);
    // hatch 线条本身需要足够清楚，但又不能像实心填充那样整块盖住下层，
    // 所以这里保留较高 alpha，真正的稀疏感交给 shader 图案来控制。
    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        0.72,
    ]
}

fn base_layer_rgb(layer: LayerId) -> [u8; 3] {
    match (layer.layer, layer.datatype) {
        (1, 1) => [80, 220, 120],
        (1, 2) => [120, 150, 255],
        (70, 31) => [250, 180, 70],
        _ => {
            let seed =
                layer.layer.wrapping_mul(1_103_515_245) ^ layer.datatype.wrapping_mul(12_345);
            [
                ((seed & 0xFF) as u8).saturating_add(40),
                (((seed >> 8) & 0xFF) as u8).saturating_add(40),
                (((seed >> 16) & 0xFF) as u8).saturating_add(40),
            ]
        }
    }
}

/// 根据 layer、世界坐标线宽和当前 zoom 推导屏幕线宽。
///
/// 之所以拆成这个辅助函数，是因为现在既有"原始 scene shape"，
/// 也有"预碎片化后的 tile 局部 fragment"，两者都需要同一套线宽规则。
fn stroke_width_for_layer(layer: LayerId, stroke_width_world: Option<f32>, zoom: f32) -> f32 {
    if let Some(world_width) = stroke_width_world {
        return (world_width * zoom).clamp(1.0, 12.0);
    }

    match (layer.layer, layer.datatype) {
        (1, 1) => 2.0,
        (1, 2) => 1.5,
        _ => 1.0,
    }
}
