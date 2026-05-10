//! 分层 `LayoutBundle -> 临时扁平 Scene` 的按需展开器。
//!
//! 这一层的职责非常明确：
//! - 输入：root cell、层级范围、可选视口 bounds
//! - 输出：当前这一帧真正需要给 renderer 的临时 `Scene`
//!
//! 这样 app/renderer 后续就不需要长期持有“全量扁平展开场景”，
//! 内存会更多地跟“当前 root / 当前 level / 当前视口”绑定。

use glam::Vec2;

use crate::{
    layout::{
        LayoutBundle, LayoutCell, LayoutCellId, LayoutInstance, LayoutRepetition, LayoutShape,
        LayoutTransform,
    },
    scene::{Bounds, RectShape, Scene},
};

/// 构建临时扁平 `Scene` 时需要的筛选条件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutViewBuildOptions {
    /// 当前想展开的 root cell。
    pub root_cell_id: LayoutCellId,
    /// 只保留这个层级范围内的 shape。
    ///
    /// 这里的语义是“shape 所属的 hierarchy level”，
    /// 而不是“递归还要不要继续往下走”；后者还会受 max level / LOD 影响。
    pub min_hierarchy_level: u32,
    pub max_hierarchy_level: u32,
    /// 只展开和当前视口相交的世界坐标范围。
    ///
    /// 这是 app 侧 workset 裁剪、renderer 侧 tile 局部化的基础输入。
    pub visible_world_bounds: Option<Bounds>,
    /// 可选的 screen-space 子树折叠。
    ///
    /// 这层优化的目标不是“完全正确地做 LOD”，
    /// 而是尽量在肉眼看不出差异时停止深层递归，避免生成巨大临时 scene。
    pub subtree_screen_lod: Option<SubtreeScreenLod>,
    /// 只展开某一个 layer。
    ///
    /// direct hierarchy tile 渲染路径会大量依赖这个字段，
    /// 这样 renderer 请求某个 `tile + layer` 时不必先把整 tile 的全部 layer 都展开。
    pub layer_filter: Option<crate::scene::LayerId>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubtreeScreenLod {
    /// 世界坐标到当前屏幕坐标的缩放比。
    pub world_to_screen_scale: f32,
    /// 当子树最大屏幕尺寸小于这个阈值时，允许折叠。
    pub min_subtree_screen_extent: f32,
    /// 为了保住浅层骨架，只有达到这个 hierarchy level 之后才允许折叠。
    pub min_collapse_hierarchy_level: u32,
}

/// view builder 的最小错误集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutViewBuildError {
    /// 指定的 root cell 在当前 bundle 里不存在。
    MissingRootCell(LayoutCellId),
    /// 最小层级大于最大层级，属于调用方输入错误。
    InvalidHierarchyRange { min: u32, max: u32 },
}

/// 把一个分层 `LayoutBundle` 按需展开成当前视图的临时 `Scene`。
pub fn build_layout_view_scene(
    bundle: &LayoutBundle,
    options: LayoutViewBuildOptions,
) -> Result<Scene, LayoutViewBuildError> {
    if options.min_hierarchy_level > options.max_hierarchy_level {
        return Err(LayoutViewBuildError::InvalidHierarchyRange {
            min: options.min_hierarchy_level,
            max: options.max_hierarchy_level,
        });
    }

    let root_cell = bundle
        .cell(options.root_cell_id)
        .ok_or(LayoutViewBuildError::MissingRootCell(options.root_cell_id))?;

    let mut shapes = Vec::new();
    let root_transform = AffineTransform::identity();
    let root_bounds_world = root_cell
        .local_bounds()
        .map(|bounds| transform_bounds(bounds, root_transform));

    if intersects_visible_world(root_bounds_world, options.visible_world_bounds) {
        expand_cell_into_scene(bundle, root_cell, 0, root_transform, &options, &mut shapes);
    }

    Ok(Scene::from_shapes(shapes))
}

pub fn visit_layout_shape_bounds_in_view(
    bundle: &LayoutBundle,
    options: LayoutViewBuildOptions,
    mut visitor: impl FnMut(crate::scene::LayerId, u32, Bounds),
) -> Result<(), LayoutViewBuildError> {
    // 这条路径是“轻量索引遍历”：
    // 只给调用方 layer + bounds，不构造 RectShape / Scene，
    // 适合 renderer 先做 tile->layer hints 这种便宜摘要。
    if options.min_hierarchy_level > options.max_hierarchy_level {
        return Err(LayoutViewBuildError::InvalidHierarchyRange {
            min: options.min_hierarchy_level,
            max: options.max_hierarchy_level,
        });
    }

    let root_cell = bundle
        .cell(options.root_cell_id)
        .ok_or(LayoutViewBuildError::MissingRootCell(options.root_cell_id))?;
    let root_transform = AffineTransform::identity();
    let root_bounds_world = root_cell
        .local_bounds()
        .map(|bounds| transform_bounds(bounds, root_transform));

    if intersects_visible_world(root_bounds_world, options.visible_world_bounds) {
        visit_cell_shape_bounds(bundle, root_cell, 0, root_transform, &options, &mut visitor);
    }

    Ok(())
}

pub fn visit_layout_shapes_in_view(
    bundle: &LayoutBundle,
    options: LayoutViewBuildOptions,
    mut visitor: impl FnMut(crate::scene::RectShape),
) -> Result<(), LayoutViewBuildError> {
    // 这条路径和 build_layout_view_scene 的差别在于：
    // 它仍然逐个展开 shape，但不把结果先收集成 Scene。
    // direct hierarchy tile worker 会直接用它流式产顶点，减少临时分配。
    if options.min_hierarchy_level > options.max_hierarchy_level {
        return Err(LayoutViewBuildError::InvalidHierarchyRange {
            min: options.min_hierarchy_level,
            max: options.max_hierarchy_level,
        });
    }

    let root_cell = bundle
        .cell(options.root_cell_id)
        .ok_or(LayoutViewBuildError::MissingRootCell(options.root_cell_id))?;
    let root_transform = AffineTransform::identity();
    let root_bounds_world = root_cell
        .local_bounds()
        .map(|bounds| transform_bounds(bounds, root_transform));

    if intersects_visible_world(root_bounds_world, options.visible_world_bounds) {
        visit_cell_shapes(bundle, root_cell, 0, root_transform, &options, &mut visitor);
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SubtreeExpansionDecision {
    /// 继续正常向下递归。
    Expand,
    /// 不再展开深层内容，改成一个 proxy shape 占位。
    CollapseToProxy(Bounds, SubtreeScreenLod),
    /// 整个子树都可以跳过。
    Skip,
}

impl LayoutViewBuildOptions {
    /// 构造一组最小 build 选项。
    pub fn new(
        root_cell_id: LayoutCellId,
        min_hierarchy_level: u32,
        max_hierarchy_level: u32,
    ) -> Self {
        Self {
            root_cell_id,
            min_hierarchy_level,
            max_hierarchy_level,
            visible_world_bounds: None,
            subtree_screen_lod: None,
            layer_filter: None,
        }
    }

    /// 给构建选项补一个可选视口范围。
    pub fn with_visible_world_bounds(mut self, visible_world_bounds: Option<Bounds>) -> Self {
        self.visible_world_bounds = visible_world_bounds;
        self
    }

    /// 给子树递归补一层基于屏幕尺寸的 LOD 裁剪。
    pub fn with_subtree_screen_lod(
        mut self,
        world_to_screen_scale: f32,
        min_subtree_screen_extent: f32,
        min_collapse_hierarchy_level: u32,
    ) -> Self {
        self.subtree_screen_lod = Some(SubtreeScreenLod {
            world_to_screen_scale,
            min_subtree_screen_extent,
            min_collapse_hierarchy_level,
        });
        self
    }

    /// 只展开指定 layer。
    pub fn with_layer_filter(mut self, layer: Option<crate::scene::LayerId>) -> Self {
        self.layer_filter = layer;
        self
    }
}

/// 递归展开一个 cell。
fn expand_cell_into_scene(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
    hierarchy_level: u32,
    world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    output: &mut Vec<RectShape>,
) {
    // 先处理“这个 level 的本地 shape”，再递归 instance。
    // 这样生成出来的 Scene hierarchy level 语义更直观，也更接近老 flat 路径。
    if hierarchy_level >= options.min_hierarchy_level
        && hierarchy_level <= options.max_hierarchy_level
    {
        for shape in cell.local_shapes() {
            if let Some(layer_filter) = options.layer_filter {
                if shape.layer() != layer_filter {
                    continue;
                }
            }
            if let Some(expanded_shape) = expand_shape(
                shape,
                hierarchy_level,
                world_transform,
                options.visible_world_bounds,
            ) {
                output.push(expanded_shape);
            }
        }
    }

    if hierarchy_level >= options.max_hierarchy_level {
        return;
    }

    for instance in cell.instances() {
        expand_instance_into_scene(
            bundle,
            instance,
            hierarchy_level + 1,
            world_transform,
            options,
            output,
        );
    }
}

fn visit_cell_shape_bounds(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
    hierarchy_level: u32,
    world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    visitor: &mut impl FnMut(crate::scene::LayerId, u32, Bounds),
) {
    // 和 expand_cell_into_scene 类似，但这里只上报 bounds，
    // 这样上层可以在不分配 shape 点列的情况下完成 tile/layer 摘要。
    if hierarchy_level >= options.min_hierarchy_level
        && hierarchy_level <= options.max_hierarchy_level
    {
        for shape in cell.local_shapes() {
            if let Some(layer_filter) = options.layer_filter {
                if shape.layer() != layer_filter {
                    continue;
                }
            }
            let world_bounds = transform_bounds(shape.bounds(), world_transform);
            if intersects_visible_world(Some(world_bounds), options.visible_world_bounds) {
                visitor(shape.layer(), hierarchy_level, world_bounds);
            }
        }
    }

    if hierarchy_level >= options.max_hierarchy_level {
        return;
    }

    for instance in cell.instances() {
        visit_instance_shape_bounds(
            bundle,
            instance,
            hierarchy_level + 1,
            world_transform,
            options,
            visitor,
        );
    }
}

fn visit_instance_shape_bounds(
    bundle: &LayoutBundle,
    instance: &LayoutInstance,
    child_hierarchy_level: u32,
    parent_world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    visitor: &mut impl FnMut(crate::scene::LayerId, u32, Bounds),
) {
    let Some(target_cell) = bundle.cell(instance.target_cell_id()) else {
        return;
    };

    let base_transform = parent_world_transform
        .combine(AffineTransform::from_layout_transform(instance.transform()));

    match instance.repetition() {
        Some(LayoutRepetition::RegularGrid {
            columns,
            rows,
            column_step,
            row_step,
        }) => {
            for row in 0..*rows {
                for col in 0..*columns {
                    let delta = *column_step * col as f32 + *row_step * row as f32;
                    let repeated_transform = base_transform
                        .with_extra_translation(parent_world_transform.apply_vector(delta));
                    let subtree_bounds = target_cell
                        .local_bounds()
                        .map(|bounds| transform_bounds(bounds, repeated_transform));
                    if !intersects_visible_world(subtree_bounds, options.visible_world_bounds) {
                        continue;
                    }
                    visit_cell_shape_bounds(
                        bundle,
                        target_cell,
                        child_hierarchy_level,
                        repeated_transform,
                        options,
                        visitor,
                    );
                }
            }
        }
        None => {
            let subtree_bounds = target_cell
                .local_bounds()
                .map(|bounds| transform_bounds(bounds, base_transform));
            if !intersects_visible_world(subtree_bounds, options.visible_world_bounds) {
                return;
            }
            visit_cell_shape_bounds(
                bundle,
                target_cell,
                child_hierarchy_level,
                base_transform,
                options,
                visitor,
            );
        }
    }
}

fn visit_cell_shapes(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
    hierarchy_level: u32,
    world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    visitor: &mut impl FnMut(RectShape),
) {
    if hierarchy_level >= options.min_hierarchy_level
        && hierarchy_level <= options.max_hierarchy_level
    {
        for shape in cell.local_shapes() {
            if let Some(layer_filter) = options.layer_filter {
                if shape.layer() != layer_filter {
                    continue;
                }
            }
            if let Some(expanded_shape) = expand_shape(
                shape,
                hierarchy_level,
                world_transform,
                options.visible_world_bounds,
            ) {
                visitor(expanded_shape);
            }
        }
    }

    if hierarchy_level >= options.max_hierarchy_level {
        return;
    }

    for instance in cell.instances() {
        visit_instance_shapes(
            bundle,
            instance,
            hierarchy_level + 1,
            world_transform,
            options,
            visitor,
        );
    }
}

fn visit_instance_shapes(
    bundle: &LayoutBundle,
    instance: &LayoutInstance,
    child_hierarchy_level: u32,
    parent_world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    visitor: &mut impl FnMut(RectShape),
) {
    let Some(target_cell) = bundle.cell(instance.target_cell_id()) else {
        return;
    };

    let base_transform = parent_world_transform
        .combine(AffineTransform::from_layout_transform(instance.transform()));

    match instance.repetition() {
        Some(LayoutRepetition::RegularGrid {
            columns,
            rows,
            column_step,
            row_step,
        }) => {
            for row in 0..*rows {
                for col in 0..*columns {
                    let delta = *column_step * col as f32 + *row_step * row as f32;
                    let repeated_transform = base_transform
                        .with_extra_translation(parent_world_transform.apply_vector(delta));
                    match classify_subtree_expansion(
                        target_cell,
                        child_hierarchy_level,
                        repeated_transform,
                        options,
                    ) {
                        SubtreeExpansionDecision::Expand => {}
                        SubtreeExpansionDecision::CollapseToProxy(bounds, lod) => {
                            if let Some(proxy_shape) = collapsed_subtree_proxy_shape(
                                bundle,
                                target_cell,
                                child_hierarchy_level,
                                bounds,
                                lod,
                            ) {
                                visitor(proxy_shape);
                            }
                            continue;
                        }
                        SubtreeExpansionDecision::Skip => continue,
                    }
                    visit_cell_shapes(
                        bundle,
                        target_cell,
                        child_hierarchy_level,
                        repeated_transform,
                        options,
                        visitor,
                    );
                }
            }
        }
        None => {
            match classify_subtree_expansion(
                target_cell,
                child_hierarchy_level,
                base_transform,
                options,
            ) {
                SubtreeExpansionDecision::Expand => {}
                SubtreeExpansionDecision::CollapseToProxy(bounds, lod) => {
                    if let Some(proxy_shape) = collapsed_subtree_proxy_shape(
                        bundle,
                        target_cell,
                        child_hierarchy_level,
                        bounds,
                        lod,
                    ) {
                        visitor(proxy_shape);
                    }
                    return;
                }
                SubtreeExpansionDecision::Skip => return,
            }
            visit_cell_shapes(
                bundle,
                target_cell,
                child_hierarchy_level,
                base_transform,
                options,
                visitor,
            );
        }
    }
}

/// 展开一个实例引用。
fn expand_instance_into_scene(
    bundle: &LayoutBundle,
    instance: &LayoutInstance,
    child_hierarchy_level: u32,
    parent_world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
    output: &mut Vec<RectShape>,
) {
    let Some(target_cell) = bundle.cell(instance.target_cell_id()) else {
        return;
    };

    let base_transform = parent_world_transform
        .combine(AffineTransform::from_layout_transform(instance.transform()));

    match instance.repetition() {
        Some(LayoutRepetition::RegularGrid {
            columns,
            rows,
            column_step,
            row_step,
        }) => {
            for row in 0..*rows {
                for col in 0..*columns {
                    let delta = *column_step * col as f32 + *row_step * row as f32;
                    let repeated_transform = base_transform
                        .with_extra_translation(parent_world_transform.apply_vector(delta));
                    match classify_subtree_expansion(
                        target_cell,
                        child_hierarchy_level,
                        repeated_transform,
                        options,
                    ) {
                        SubtreeExpansionDecision::Expand => {}
                        SubtreeExpansionDecision::CollapseToProxy(bounds, lod) => {
                            if let Some(proxy_shape) = collapsed_subtree_proxy_shape(
                                bundle,
                                target_cell,
                                child_hierarchy_level,
                                bounds,
                                lod,
                            ) {
                                output.push(proxy_shape);
                            }
                            continue;
                        }
                        SubtreeExpansionDecision::Skip => continue,
                    }
                    expand_cell_into_scene(
                        bundle,
                        target_cell,
                        child_hierarchy_level,
                        repeated_transform,
                        options,
                        output,
                    );
                }
            }
        }
        None => {
            match classify_subtree_expansion(
                target_cell,
                child_hierarchy_level,
                base_transform,
                options,
            ) {
                SubtreeExpansionDecision::Expand => {}
                SubtreeExpansionDecision::CollapseToProxy(bounds, lod) => {
                    if let Some(proxy_shape) = collapsed_subtree_proxy_shape(
                        bundle,
                        target_cell,
                        child_hierarchy_level,
                        bounds,
                        lod,
                    ) {
                        output.push(proxy_shape);
                    }
                    return;
                }
                SubtreeExpansionDecision::Skip => return,
            }
            expand_cell_into_scene(
                bundle,
                target_cell,
                child_hierarchy_level,
                base_transform,
                options,
                output,
            );
        }
    }
}

/// 把局部 `LayoutShape` 展开成 renderer 可消费的 `RectShape`。
fn expand_shape(
    shape: &LayoutShape,
    hierarchy_level: u32,
    world_transform: AffineTransform,
    visible_world_bounds: Option<Bounds>,
) -> Option<RectShape> {
    let transformed_points: Vec<Vec2> = shape
        .points()
        .iter()
        .copied()
        .map(|point| world_transform.apply_point(point))
        .collect();
    if transformed_points.is_empty() {
        return None;
    }

    let stroke_width_world = shape
        .stroke_width()
        .map(|width| width * world_transform.uniform_scale());
    let expanded = RectShape::from_points(
        shape.layer(),
        transformed_points,
        shape.closed(),
        hierarchy_level,
        stroke_width_world,
    );

    if intersects_visible_world(Some(expanded.bounds), visible_world_bounds) {
        Some(expanded)
    } else {
        None
    }
}

/// 通过 cell 的局部 bounds 做视口裁剪，避免不必要地递归进入整个子树。
fn classify_subtree_expansion(
    cell: &LayoutCell,
    hierarchy_level: u32,
    world_transform: AffineTransform,
    options: &LayoutViewBuildOptions,
) -> SubtreeExpansionDecision {
    let subtree_bounds = cell
        .local_bounds()
        .map(|bounds| transform_bounds(bounds, world_transform));
    if !intersects_visible_world(subtree_bounds, options.visible_world_bounds) {
        return SubtreeExpansionDecision::Skip;
    }

    match (subtree_bounds, options.subtree_screen_lod) {
        (Some(subtree_bounds), Some(lod))
            if hierarchy_level >= lod.min_collapse_hierarchy_level
                && hierarchy_level > options.min_hierarchy_level
                && subtree_screen_extent(subtree_bounds, lod.world_to_screen_scale)
                    < lod.min_subtree_screen_extent =>
        {
            SubtreeExpansionDecision::CollapseToProxy(subtree_bounds, lod)
        }
        _ => SubtreeExpansionDecision::Expand,
    }
}

fn collapsed_subtree_proxy_shape(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
    hierarchy_level: u32,
    bounds: Bounds,
    lod: SubtreeScreenLod,
) -> Option<RectShape> {
    let layer = representative_layer_for_cell(bundle, cell)?;
    let center = bounds.center();
    let marker_world_extent =
        (lod.min_subtree_screen_extent / lod.world_to_screen_scale.max(f32::EPSILON)).max(1.0);
    let half = marker_world_extent * 0.5;
    Some(RectShape::from_points(
        layer,
        vec![
            Vec2::new(center.x - half, center.y),
            Vec2::new(center.x + half, center.y),
        ],
        false,
        hierarchy_level,
        Some(marker_world_extent * 0.5),
    ))
}

fn representative_layer_for_cell(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
) -> Option<crate::scene::LayerId> {
    representative_layer_for_cell_inner(bundle, cell, &mut std::collections::BTreeSet::new())
}

fn representative_layer_for_cell_inner(
    bundle: &LayoutBundle,
    cell: &LayoutCell,
    visiting: &mut std::collections::BTreeSet<LayoutCellId>,
) -> Option<crate::scene::LayerId> {
    if let Some(layer) = cell.local_layers().first().copied() {
        return Some(layer);
    }
    if !visiting.insert(cell.id()) {
        return None;
    }
    for instance in cell.instances() {
        let Some(target_cell) = bundle.cell(instance.target_cell_id()) else {
            continue;
        };
        if let Some(layer) = representative_layer_for_cell_inner(bundle, target_cell, visiting) {
            visiting.remove(&cell.id());
            return Some(layer);
        }
    }
    visiting.remove(&cell.id());
    None
}

fn subtree_screen_extent(bounds: Bounds, world_to_screen_scale: f32) -> f32 {
    bounds.width().max(bounds.height()).max(0.0) * world_to_screen_scale.max(0.0)
}

fn intersects_visible_world(
    candidate: Option<Bounds>,
    visible_world_bounds: Option<Bounds>,
) -> bool {
    match (candidate, visible_world_bounds) {
        (Some(candidate), Some(visible_world_bounds)) => candidate.intersects(visible_world_bounds),
        (None, Some(_)) => false,
        (_, None) => true,
    }
}

/// 一个简单的 2D 仿射变换。
///
/// 这里和 loader 里的临时变换结构做了相同的数学表示，
/// 但保持这个模块自给自足，避免为了 Task 3 去扩大别的模块公开面。
#[derive(Debug, Clone, Copy, PartialEq)]
struct AffineTransform {
    basis_x: Vec2,
    basis_y: Vec2,
    translation: Vec2,
}

impl AffineTransform {
    fn identity() -> Self {
        Self {
            basis_x: Vec2::X,
            basis_y: Vec2::Y,
            translation: Vec2::ZERO,
        }
    }

    fn from_layout_transform(transform: LayoutTransform) -> Self {
        let mut basis_x = Vec2::X;
        let mut basis_y = Vec2::Y;

        if transform.mirrored_x {
            basis_y = -basis_y;
        }

        let scale = transform.magnification.abs();
        basis_x *= scale;
        basis_y *= scale;

        let angle_radians = transform.rotation_degrees.to_radians();
        if angle_radians != 0.0 {
            basis_x = rotate_vector(basis_x, angle_radians);
            basis_y = rotate_vector(basis_y, angle_radians);
        }

        Self {
            basis_x,
            basis_y,
            translation: transform.translation,
        }
    }

    fn combine(self, local: Self) -> Self {
        Self {
            basis_x: self.apply_vector(local.basis_x),
            basis_y: self.apply_vector(local.basis_y),
            translation: self.apply_point(local.translation),
        }
    }

    fn with_extra_translation(self, delta: Vec2) -> Self {
        Self {
            translation: self.translation + delta,
            ..self
        }
    }

    fn apply_point(self, point: Vec2) -> Vec2 {
        self.basis_x * point.x + self.basis_y * point.y + self.translation
    }

    fn apply_vector(self, vector: Vec2) -> Vec2 {
        self.basis_x * vector.x + self.basis_y * vector.y
    }

    fn uniform_scale(self) -> f32 {
        self.basis_x.length().max(self.basis_y.length()).max(1.0)
    }
}

fn transform_bounds(bounds: Bounds, transform: AffineTransform) -> Bounds {
    let corners = [
        Vec2::new(bounds.min_x, bounds.min_y),
        Vec2::new(bounds.max_x, bounds.min_y),
        Vec2::new(bounds.max_x, bounds.max_y),
        Vec2::new(bounds.min_x, bounds.max_y),
    ];
    let mut transformed = corners
        .into_iter()
        .map(|corner| transform.apply_point(corner));
    let first = transformed.next().expect("bounds corners");
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for point in transformed {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Bounds::new(min_x, min_y, max_x, max_y)
}

fn rotate_vector(vector: Vec2, angle_radians: f32) -> Vec2 {
    let (sin, cos) = angle_radians.sin_cos();
    Vec2::new(
        vector.x * cos - vector.y * sin,
        vector.x * sin + vector.y * cos,
    )
}
