//! 分层版图内存模型。
//!
//! 这个模块先只解决一件事：
//! 把“一个版图由哪些 cell 组成、cell 之间怎样实例引用”表示清楚，
//! 暂时不负责把整棵层级树展开成 renderer 直接消费的扁平 `Scene`。

pub mod view_builder;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use glam::Vec2;

use crate::scene::{Bounds, LayerId};
pub use view_builder::{build_layout_view_scene, LayoutViewBuildError, LayoutViewBuildOptions};

/// 稳定的 cell 标识。
///
/// 这里不用字符串当主键，是为了后续 loader / workset builder
/// 可以更稳定地做引用、索引和缓存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayoutCellId(u64);

/// 版图里的局部图形。
///
/// 这里直接保留局部几何点列，而不是只存一个矩形标签。
/// 这样后续 workset builder 在处理 Boundary / Box / Path 时，
/// 可以继续复用这些原始局部几何，而不需要重新猜测。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutShape {
    layer: LayerId,
    bounds: Bounds,
    points: Vec<Vec2>,
    closed: bool,
    stroke_width: Option<f32>,
}

/// 一个 cell 对另一个 cell 的实例引用。
///
/// `local_bounds` 是实例在父 cell 局部坐标中的快速包围盒，
/// 后续可用于裁剪、统计和 workset 选择。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutInstance {
    target_cell_id: LayoutCellId,
    local_bounds: Bounds,
    transform: LayoutTransform,
    repetition: Option<LayoutRepetition>,
}

/// 实例在父 cell 局部坐标系中的放置变换。
///
/// 这里先保留学习友好的显式字段，而不是过早引入矩阵。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutTransform {
    pub translation: Vec2,
    pub rotation_degrees: f32,
    pub magnification: f32,
    pub mirrored_x: bool,
}

/// 实例重复放置的最小表达。
///
/// 先保留 ArrayRef 需要的规则网格语义，
/// 避免为了后续展开而在这里提前做 eager expansion。
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRepetition {
    RegularGrid {
        columns: u32,
        rows: u32,
        column_step: Vec2,
        row_step: Vec2,
    },
}

/// 一个 cell 的局部内容。
///
/// 注意这里的 shapes 只保存“本 cell 直接拥有的几何”，
/// 不包含任何子实例展开结果。
#[derive(Debug, Clone)]
pub struct LayoutCell {
    id: LayoutCellId,
    name: String,
    local_shapes: Arc<[LayoutShape]>,
    instances: Vec<LayoutInstance>,
    local_bounds: Option<Bounds>,
    local_layers: Vec<LayerId>,
}

/// root 视图的元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutViewMetadata {
    name: String,
    root_cell_id: LayoutCellId,
}

/// 一个可切换的 root 视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutView {
    metadata: LayoutViewMetadata,
}

/// 一份版图文件的分层 bundle。
#[derive(Debug, Clone, Default)]
pub struct LayoutBundle {
    cells: BTreeMap<LayoutCellId, Arc<LayoutCell>>,
    views: Vec<LayoutView>,
    selected: usize,
}

/// bundle 构造期的最小错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutBundleError {
    DuplicateCellId(LayoutCellId),
    MissingRootCell(LayoutCellId),
    DanglingInstanceTarget {
        owner_cell_id: LayoutCellId,
        target_cell_id: LayoutCellId,
    },
}

impl LayoutCellId {
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl LayoutShape {
    /// 用通用几何描述构造一个局部图形。
    pub fn from_points(
        layer: LayerId,
        points: Vec<Vec2>,
        closed: bool,
        stroke_width: Option<f32>,
    ) -> Self {
        assert!(
            !points.is_empty(),
            "LayoutShape 至少需要一个点，不能用空点列构造。"
        );

        Self {
            layer,
            bounds: collect_points_bounds(&points, stroke_width),
            points,
            closed,
            stroke_width,
        }
    }

    /// 构造一个闭合轮廓。
    pub fn polygon(layer: LayerId, points: Vec<Vec2>) -> Self {
        Self::from_points(layer, points, true, None)
    }

    /// 构造一个带线宽的 path 中心线。
    pub fn path(layer: LayerId, points: Vec<Vec2>, stroke_width: f32) -> Self {
        Self::from_points(layer, points, false, Some(stroke_width))
    }

    /// 构造一个矩形局部几何。
    pub fn rectangle(layer: LayerId, bounds: Bounds) -> Self {
        Self::polygon(
            layer,
            vec![
                Vec2::new(bounds.min_x, bounds.min_y),
                Vec2::new(bounds.max_x, bounds.min_y),
                Vec2::new(bounds.max_x, bounds.max_y),
                Vec2::new(bounds.min_x, bounds.max_y),
            ],
        )
    }

    pub fn layer(&self) -> LayerId {
        self.layer
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    pub fn points(&self) -> &[Vec2] {
        &self.points
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn stroke_width(&self) -> Option<f32> {
        self.stroke_width
    }
}

impl LayoutInstance {
    /// 构造一个默认 identity 变换的实例引用。
    pub fn new(target_cell_id: LayoutCellId, local_bounds: Bounds) -> Self {
        Self {
            target_cell_id,
            local_bounds,
            transform: LayoutTransform::identity(),
            repetition: None,
        }
    }

    pub fn with_transform(
        target_cell_id: LayoutCellId,
        local_bounds: Bounds,
        transform: LayoutTransform,
    ) -> Self {
        Self {
            target_cell_id,
            local_bounds,
            transform,
            repetition: None,
        }
    }

    /// 给实例补充重复放置语义。
    pub fn with_repetition(mut self, repetition: LayoutRepetition) -> Self {
        self.repetition = Some(repetition);
        self
    }

    pub fn target_cell_id(&self) -> LayoutCellId {
        self.target_cell_id
    }

    pub fn local_bounds(&self) -> Bounds {
        self.local_bounds
    }

    pub fn transform(&self) -> LayoutTransform {
        self.transform
    }

    pub fn repetition(&self) -> Option<&LayoutRepetition> {
        self.repetition.as_ref()
    }
}

impl LayoutTransform {
    pub fn identity() -> Self {
        Self {
            translation: Vec2::ZERO,
            rotation_degrees: 0.0,
            magnification: 1.0,
            mirrored_x: false,
        }
    }
}

impl LayoutRepetition {
    pub fn regular_grid(columns: u32, rows: u32, column_step: Vec2, row_step: Vec2) -> Self {
        assert!(columns > 0, "columns 必须大于 0。");
        assert!(rows > 0, "rows 必须大于 0。");

        Self::RegularGrid {
            columns,
            rows,
            column_step,
            row_step,
        }
    }
}

impl LayoutCell {
    pub fn new(
        id: LayoutCellId,
        name: impl Into<String>,
        local_shapes: Vec<LayoutShape>,
        instances: Vec<LayoutInstance>,
    ) -> Self {
        let local_bounds = collect_cell_bounds(&local_shapes, &instances);
        let local_layers = collect_local_layers(&local_shapes);

        Self {
            id,
            name: name.into(),
            local_shapes: Arc::from(local_shapes),
            instances,
            local_bounds,
            local_layers,
        }
    }

    pub fn id(&self) -> LayoutCellId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn local_shapes(&self) -> &[LayoutShape] {
        &self.local_shapes
    }

    pub fn local_shapes_handle(&self) -> Arc<[LayoutShape]> {
        Arc::clone(&self.local_shapes)
    }

    pub fn instances(&self) -> &[LayoutInstance] {
        &self.instances
    }

    pub fn local_shape_count(&self) -> usize {
        self.local_shapes.len()
    }

    pub fn local_instance_count(&self) -> usize {
        self.instances.len()
    }

    pub fn local_bounds(&self) -> Option<Bounds> {
        self.local_bounds
    }

    pub fn local_layers(&self) -> &[LayerId] {
        &self.local_layers
    }
}

impl LayoutViewMetadata {
    pub fn new(name: impl Into<String>, root_cell_id: LayoutCellId) -> Self {
        Self {
            name: name.into(),
            root_cell_id,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root_cell_id(&self) -> LayoutCellId {
        self.root_cell_id
    }
}

impl LayoutView {
    pub fn new(metadata: LayoutViewMetadata) -> Self {
        Self { metadata }
    }

    pub fn metadata(&self) -> &LayoutViewMetadata {
        &self.metadata
    }
}

impl LayoutBundle {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(
        cells: Vec<Arc<LayoutCell>>,
        views: Vec<LayoutView>,
    ) -> Result<Self, LayoutBundleError> {
        let mut cells_by_id = BTreeMap::new();
        for cell in cells {
            let cell_id = cell.id();
            if cells_by_id.insert(cell_id, cell).is_some() {
                return Err(LayoutBundleError::DuplicateCellId(cell_id));
            }
        }

        for view in &views {
            let root_cell_id = view.metadata().root_cell_id();
            if !cells_by_id.contains_key(&root_cell_id) {
                return Err(LayoutBundleError::MissingRootCell(root_cell_id));
            }
        }

        for (owner_cell_id, cell) in &cells_by_id {
            for instance in cell.instances() {
                let target_cell_id = instance.target_cell_id();
                if !cells_by_id.contains_key(&target_cell_id) {
                    return Err(LayoutBundleError::DanglingInstanceTarget {
                        owner_cell_id: *owner_cell_id,
                        target_cell_id,
                    });
                }
            }
        }

        Ok(Self {
            cells: cells_by_id,
            views,
            selected: 0,
        })
    }

    pub fn cells(&self) -> &BTreeMap<LayoutCellId, Arc<LayoutCell>> {
        &self.cells
    }

    pub fn cell(&self, id: LayoutCellId) -> Option<&LayoutCell> {
        self.cells.get(&id).map(Arc::as_ref)
    }

    pub fn views(&self) -> &[LayoutView] {
        &self.views
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_view(&self) -> Option<&LayoutView> {
        self.views.get(self.selected)
    }

    pub fn selected_root_metadata(&self) -> Option<&LayoutViewMetadata> {
        self.selected_view().map(LayoutView::metadata)
    }

    pub fn selected_root_cell(&self) -> Option<&LayoutCell> {
        let root_id = self.selected_root_metadata()?.root_cell_id();
        self.cell(root_id)
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.views.len() || index == self.selected {
            return false;
        }
        self.selected = index;
        true
    }
}

fn collect_local_layers(local_shapes: &[LayoutShape]) -> Vec<LayerId> {
    let mut layers = BTreeSet::new();
    for shape in local_shapes {
        layers.insert(shape.layer());
    }
    layers.into_iter().collect()
}

fn collect_cell_bounds(
    local_shapes: &[LayoutShape],
    instances: &[LayoutInstance],
) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;

    for shape in local_shapes {
        bounds = Some(match bounds {
            Some(current) => current.union(shape.bounds()),
            None => shape.bounds(),
        });
    }

    for instance in instances {
        bounds = Some(match bounds {
            Some(current) => current.union(instance.local_bounds()),
            None => instance.local_bounds(),
        });
    }

    bounds
}

fn collect_points_bounds(points: &[Vec2], stroke_width: Option<f32>) -> Bounds {
    let mut min_x = points[0].x;
    let mut min_y = points[0].y;
    let mut max_x = points[0].x;
    let mut max_y = points[0].y;

    for point in &points[1..] {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }

    let bounds = Bounds::new(min_x, min_y, max_x, max_y);
    match stroke_width {
        Some(width) => bounds.pad(width * 0.5),
        None => bounds,
    }
}
