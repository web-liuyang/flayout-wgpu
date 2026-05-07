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
    pub root_cell_id: LayoutCellId,
    pub min_hierarchy_level: u32,
    pub max_hierarchy_level: u32,
    pub visible_world_bounds: Option<Bounds>,
}

/// view builder 的最小错误集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutViewBuildError {
    MissingRootCell(LayoutCellId),
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
        }
    }

    /// 给构建选项补一个可选视口范围。
    pub fn with_visible_world_bounds(mut self, visible_world_bounds: Option<Bounds>) -> Self {
        self.visible_world_bounds = visible_world_bounds;
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
    if hierarchy_level >= options.min_hierarchy_level
        && hierarchy_level <= options.max_hierarchy_level
    {
        for shape in cell.local_shapes() {
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
                    let repeated_transform = base_transform.with_extra_translation(delta);
                    if !subtree_might_be_visible(
                        target_cell,
                        repeated_transform,
                        options.visible_world_bounds,
                    ) {
                        continue;
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
            if !subtree_might_be_visible(target_cell, base_transform, options.visible_world_bounds)
            {
                return;
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
fn subtree_might_be_visible(
    cell: &LayoutCell,
    world_transform: AffineTransform,
    visible_world_bounds: Option<Bounds>,
) -> bool {
    let subtree_bounds = cell
        .local_bounds()
        .map(|bounds| transform_bounds(bounds, world_transform));
    intersects_visible_world(subtree_bounds, visible_world_bounds)
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
