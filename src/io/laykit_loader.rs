//! `laykit` 解析适配层。
//!
//! 这个文件是“外部版图格式”进入“内部 Scene”的桥梁。
//! 它的职责不是把所有 GDS / OASIS 细节完整暴露出去，
//! 而是尽量抽取出当前 viewer 需要的最小几何信息。
//!
//! 当前重点支持：
//! - GDS: `Boundary` / `Box` / `Path` / `StructRef` / `ArrayRef`
//! - OASIS: `Rectangle` / `Placement`
//!
//! 这里最值得学习的点有三个：
//! 1. 为什么要先找 root cell，再展开层级
//! 2. 为什么要把子 cell 的局部坐标递归累加成“扁平场景”
//! 3. 为什么层级展开参数必须是“完整 2D 变换”，而不是单纯 offset
//!
//! 对一个最小查看器来说，先做“扁平化渲染”是非常划算的：
//! 结构清晰，容易看对不对，也更利于后面加索引和缓存。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use glam::Vec2;
use laykit::{
    ArrayRef, Boundary, ExtensionScheme, GDSBox, GDSElement, GDSIIFile, GDSStructure, GPath,
    OASISCell, OASISElement, OASISFile, OPath, Placement, Polygon, Rectangle, Repetition,
    STrans, StructRef,
};

use crate::error::AppError;
use crate::scene::{Bounds, LayerId, RectShape, Scene, SceneBundle, SceneView};

/// 一个非常轻量的 2D 仿射变换表示。
///
/// 为什么这里不直接引入完整矩阵库？
/// 因为当前导入层真正需要的能力其实很明确：
/// - 点变换
/// - 向量变换
/// - 变换组合
/// - 从 GDS / OASIS 的实例参数构造局部变换
///
/// 用“两根基向量 + 一个平移向量”的表示已经足够，
/// 而且它非常适合在学习时直观看懂：
///
/// `global_point = basis_x * local.x + basis_y * local.y + translation`
#[derive(Debug, Clone, Copy, PartialEq)]
struct Transform2D {
    basis_x: Vec2,
    basis_y: Vec2,
    translation: Vec2,
}

impl Transform2D {
    /// 单位变换。
    fn identity() -> Self {
        Self {
            basis_x: Vec2::X,
            basis_y: Vec2::Y,
            translation: Vec2::ZERO,
        }
    }


    /// 只构造“线性部分”，不包含平移。
    ///
    /// 这里统一处理：
    /// - mirror / reflection
    /// - magnification
    /// - angle
    ///
    /// 对 GDS 来说，常见语义可以理解为：
    /// - 先在局部坐标里做镜像
    /// - 再做缩放 / 旋转
    /// - 最后再把实例平移到目标位置
    ///
    /// 由于当前只支持等比缩放，缩放和旋转的先后顺序对结果等价，
    /// 所以这里用“镜像 -> 缩放 -> 旋转”的实现会比较直观。
    fn from_linear_parts(mirror: bool, angle_deg: Option<f64>, magnification: Option<f64>) -> Self {
        let mut basis_x = Vec2::X;
        let mut basis_y = Vec2::Y;

        if mirror {
            // GDS 的 reflection 可以先近似理解成“关于局部 X 轴翻转”，
            // 也就是把 y 方向反过来。
            basis_y = -basis_y;
        }

        let scale = magnification.unwrap_or(1.0).abs() as f32;
        basis_x *= scale;
        basis_y *= scale;

        let angle = angle_deg.unwrap_or(0.0).to_radians() as f32;
        if angle != 0.0 {
            basis_x = rotate_vector(basis_x, angle);
            basis_y = rotate_vector(basis_y, angle);
        }

        Self {
            basis_x,
            basis_y,
            translation: Vec2::ZERO,
        }
    }

    /// 从一个 GDS `STrans` 构造实例局部变换。
    fn from_gds_reference(origin: Vec2, strans: Option<&STrans>) -> Self {
        let linear = match strans {
            Some(strans) => Self::from_linear_parts(
                strans.reflection,
                strans.angle,
                strans.magnification,
            ),
            None => Self::identity(),
        };

        Self {
            translation: origin,
            ..linear
        }
    }

    /// 从一个 OASIS `Placement` 构造实例局部变换。
    fn from_oasis_placement(placement: &Placement) -> Self {
        let linear = Self::from_linear_parts(
            placement.mirror,
            placement.angle,
            placement.magnification,
        );

        Self {
            translation: Vec2::new(placement.x as f32, placement.y as f32),
            ..linear
        }
    }

    /// 变换一个“位置点”。
    fn apply_point(self, point: Vec2) -> Vec2 {
        self.basis_x * point.x + self.basis_y * point.y + self.translation
    }

    /// 变换一个“纯向量”。
    ///
    /// 和 `apply_point` 的区别在于：
    /// 这里不会加平移，只保留线性部分。
    fn apply_vector(self, vector: Vec2) -> Vec2 {
        self.basis_x * vector.x + self.basis_y * vector.y
    }

    /// 组合两个变换。
    ///
    /// 返回值语义是：先应用 `local`，再应用 `self`。
    ///
    /// 为什么要这样设计？
    /// 因为层级展开正好符合“父实例包住子实例”的嵌套关系：
    /// - 子 cell 里的点，先经过自己的局部实例变换
    /// - 再经过父层级已经累积好的全局变换
    fn combine(self, local: Self) -> Self {
        Self {
            basis_x: self.apply_vector(local.basis_x),
            basis_y: self.apply_vector(local.basis_y),
            translation: self.apply_point(local.translation),
        }
    }

    /// 估算当前变换的等比缩放系数。
    ///
    /// 当前 viewer 只支持等比缩放，所以理论上两根基向量长度相同。
    /// 这里取它们的最大值做稳妥回退，避免未来数据稍有偏差时线宽退化成 0。
    fn uniform_scale(self) -> f32 {
        self.basis_x.length().max(self.basis_y.length()).max(1.0)
    }
}

/// 从 GDS 文件加载场景集合。
pub fn load_gds(path: &Path) -> Result<SceneBundle, AppError> {
    let gds = GDSIIFile::read_from_file(path).map_err(|err| AppError::Parse(err.to_string()))?;
    build_gds_scene_bundle(&gds)
}

/// 从 OASIS 文件加载场景集合。
pub fn load_oasis(path: &Path) -> Result<SceneBundle, AppError> {
    let oasis = OASISFile::read_from_file(path).map_err(|err| AppError::Parse(err.to_string()))?;
    build_oasis_scene_bundle(&oasis)
}

/// 从 GDS 内容构造 `SceneBundle`。
///
/// 这里会优先选择“没有被别人引用的 structure”作为 root cells。
/// 原因是：
/// - 这些 structure 更像用户真正想看的入口单元
/// - 如果把所有子 cell 也直接列出来，UI 会混入很多局部构件，阅读体验会很差
fn build_gds_scene_bundle(gds: &GDSIIFile) -> Result<SceneBundle, AppError> {
    let structures_by_name: HashMap<&str, &GDSStructure> = gds
        .structures
        .iter()
        .map(|structure| (structure.name.as_str(), structure))
        .collect();

    let mut referenced = HashSet::new();
    for structure in &gds.structures {
        for element in &structure.elements {
            match element {
                GDSElement::StructRef(sref) => {
                    referenced.insert(sref.sname.as_str());
                }
                GDSElement::ArrayRef(aref) => {
                    referenced.insert(aref.sname.as_str());
                }
                _ => {}
            }
        }
    }

    let root_structures: Vec<&GDSStructure> = gds
        .structures
        .iter()
        .filter(|structure| !referenced.contains(structure.name.as_str()))
        .collect();
    let source_structures = if root_structures.is_empty() {
        // 某些数据可能没有明显 root，这里回退为“全部结构都可选”。
        gds.structures.iter().collect()
    } else {
        root_structures
    };

    let mut views = Vec::new();
    for structure in source_structures {
        let mut shapes = Vec::new();
        let mut stack = vec![structure.name.clone()];
        collect_gds_shapes(
            structure,
            &structures_by_name,
            Transform2D::identity(),
            0,
            &mut stack,
            &mut shapes,
        )?;
        views.push(SceneView {
            name: structure.name.clone(),
            scene: Scene::from_shapes(shapes),
        });
    }

    Ok(SceneBundle::new(views))
}

/// 从 OASIS 内容构造 `SceneBundle`。
fn build_oasis_scene_bundle(oasis: &OASISFile) -> Result<SceneBundle, AppError> {
    let cells_by_name: HashMap<&str, &OASISCell> = oasis
        .cells
        .iter()
        .map(|cell| (cell.name.as_str(), cell))
        .collect();

    let mut referenced = HashSet::new();
    for cell in &oasis.cells {
        for element in &cell.elements {
            if let OASISElement::Placement(placement) = element {
                referenced.insert(placement.cell_name.as_str());
            }
        }
    }

    let root_cells: Vec<&OASISCell> = oasis
        .cells
        .iter()
        .filter(|cell| !referenced.contains(cell.name.as_str()))
        .collect();
    let source_cells = if root_cells.is_empty() {
        oasis.cells.iter().collect()
    } else {
        root_cells
    };

    let mut views = Vec::new();
    for cell in source_cells {
        let mut shapes = Vec::new();
        let mut stack = vec![cell.name.clone()];
        collect_oasis_shapes(
            cell,
            &cells_by_name,
            Transform2D::identity(),
            0,
            &mut stack,
            &mut shapes,
        )?;
        views.push(SceneView {
            name: cell.name.clone(),
            scene: Scene::from_shapes(shapes),
        });
    }

    Ok(SceneBundle::new(views))
}

/// 收集一个 GDS structure 中的几何，并递归展开层级引用。
///
/// `hierarchy_level` 的定义是“当前 structure 自己处在第几层”：
/// - root structure 里直接包含的图形是 `0`
/// - 它引用的子 structure 里的图形是 `1`
/// - 再往下继续递增
fn collect_gds_shapes(
    structure: &GDSStructure,
    structures_by_name: &HashMap<&str, &GDSStructure>,
    transform: Transform2D,
    hierarchy_level: u32,
    stack: &mut Vec<String>,
    shapes: &mut Vec<RectShape>,
) -> Result<(), AppError> {
    for element in &structure.elements {
        match element {
            GDSElement::Boundary(boundary) => {
                push_boundary(shapes, boundary, transform, hierarchy_level)
            }
            GDSElement::Box(gds_box) => {
                push_gds_box(shapes, gds_box, transform, hierarchy_level)
            }
            GDSElement::Path(path) => push_gds_path(shapes, path, transform, hierarchy_level),
            GDSElement::StructRef(sref) => {
                expand_gds_struct_ref(
                    sref,
                    structures_by_name,
                    transform,
                    hierarchy_level,
                    stack,
                    shapes,
                )?
            }
            GDSElement::ArrayRef(aref) => {
                expand_gds_array_ref(
                    aref,
                    structures_by_name,
                    transform,
                    hierarchy_level,
                    stack,
                    shapes,
                )?
            }
            _ => {}
        }
    }

    Ok(())
}

/// 收集一个 OASIS cell 中的几何，并递归展开 placement。
fn collect_oasis_shapes(
    cell: &OASISCell,
    cells_by_name: &HashMap<&str, &OASISCell>,
    transform: Transform2D,
    hierarchy_level: u32,
    stack: &mut Vec<String>,
    shapes: &mut Vec<RectShape>,
) -> Result<(), AppError> {
    for element in &cell.elements {
        match element {
            OASISElement::Rectangle(rectangle) => {
                push_rectangle(shapes, rectangle, transform, hierarchy_level)?
            }
            OASISElement::Polygon(polygon) => {
                push_polygon(shapes, polygon, transform, hierarchy_level)?
            }
            OASISElement::Path(path) => {
                push_oasis_path(shapes, path, transform, hierarchy_level)?
            }
            OASISElement::Placement(placement) => {
                expand_oasis_placement(
                    placement,
                    cells_by_name,
                    transform,
                    hierarchy_level,
                    stack,
                    shapes,
                )?
            }
            _ => {}
        }
    }

    Ok(())
}

/// 展开一次 GDS 的 `StructRef`。
///
/// 这里最关键的是三点：
/// - 递归前先检查循环引用
/// - 不能只累加平移，而是要把 `STrans` 也组合进来
/// - 父实例和子实例的变换顺序必须正确
///
/// 同时，子 cell 里的图形层级深度要 `+1`，
/// 这样 app 才能用 `min/max level` 去做 hierarchy range 过滤。
fn expand_gds_struct_ref(
    sref: &StructRef,
    structures_by_name: &HashMap<&str, &GDSStructure>,
    transform: Transform2D,
    hierarchy_level: u32,
    stack: &mut Vec<String>,
    shapes: &mut Vec<RectShape>,
) -> Result<(), AppError> {
    let child = structures_by_name
        .get(sref.sname.as_str())
        .ok_or_else(|| AppError::Parse(format!("missing referenced cell: {}", sref.sname)))?;

    if stack.iter().any(|name| name == &sref.sname) {
        return Err(AppError::Parse(format!(
            "cyclic cell reference detected at {}",
            sref.sname
        )));
    }

    let local = Transform2D::from_gds_reference(
        Vec2::new(sref.xy.0 as f32, sref.xy.1 as f32),
        sref.strans.as_ref(),
    );
    let child_transform = transform.combine(local);
    stack.push(sref.sname.clone());
    let result = collect_gds_shapes(
        child,
        structures_by_name,
        child_transform,
        hierarchy_level + 1,
        stack,
        shapes,
    );
    stack.pop();
    result
}

/// 展开一次 GDS 的 `ArrayRef`。
///
/// `ArrayRef` 可以看成“规则阵列复制”，
/// 所以这里先把每个实例的位移算出来，再叠加 `STrans` 后逐个递归进去。
fn expand_gds_array_ref(
    aref: &ArrayRef,
    structures_by_name: &HashMap<&str, &GDSStructure>,
    transform: Transform2D,
    hierarchy_level: u32,
    stack: &mut Vec<String>,
    shapes: &mut Vec<RectShape>,
) -> Result<(), AppError> {
    let child = structures_by_name
        .get(aref.sname.as_str())
        .ok_or_else(|| AppError::Parse(format!("missing referenced cell: {}", aref.sname)))?;

    if stack.iter().any(|name| name == &aref.sname) {
        return Err(AppError::Parse(format!(
            "cyclic cell reference detected at {}",
            aref.sname
        )));
    }

    for delta in array_ref_offsets(aref) {
        let local = Transform2D::from_gds_reference(delta, aref.strans.as_ref());
        let child_transform = transform.combine(local);
        stack.push(aref.sname.clone());
        let result = collect_gds_shapes(
            child,
            structures_by_name,
            child_transform,
            hierarchy_level + 1,
            stack,
            shapes,
        );
        stack.pop();
        result?;
    }

    Ok(())
}

/// 展开一次 OASIS 的 `Placement`。
///
/// 这里和 GDS `StructRef` 的层级语义保持一致：
/// placement 引入的子 cell 里的图形深度统一 `+1`。
fn expand_oasis_placement(
    placement: &Placement,
    cells_by_name: &HashMap<&str, &OASISCell>,
    transform: Transform2D,
    hierarchy_level: u32,
    stack: &mut Vec<String>,
    shapes: &mut Vec<RectShape>,
) -> Result<(), AppError> {
    let child = cells_by_name
        .get(placement.cell_name.as_str())
        .ok_or_else(|| {
            AppError::Parse(format!("missing referenced cell: {}", placement.cell_name))
        })?;

    if stack.iter().any(|name| name == &placement.cell_name) {
        return Err(AppError::Parse(format!(
            "cyclic cell reference detected at {}",
            placement.cell_name
        )));
    }

    for delta in repetition_offsets(placement.repetition.as_ref())? {
        let mut local = Transform2D::from_oasis_placement(placement);
        // OASIS placement 自身的位置是实例原点，
        // repetition 额外提供的是“同一实例模板的附加位移”。
        local.translation += delta;
        let child_transform = transform.combine(local);
        stack.push(placement.cell_name.clone());
        let result = collect_oasis_shapes(
            child,
            cells_by_name,
            child_transform,
            hierarchy_level + 1,
            stack,
            shapes,
        );
        stack.pop();
        result?;
    }

    Ok(())
}

/// 将 GDS boundary 转成内部 shape。
fn push_boundary(
    shapes: &mut Vec<RectShape>,
    boundary: &Boundary,
    transform: Transform2D,
    hierarchy_level: u32,
) {
    let points = transform_i32_points(&boundary.xy, transform);
    push_outline_shape(
        shapes,
        LayerId {
            layer: boundary.layer as u32,
            datatype: boundary.datatype as u32,
        },
        points,
        true,
        None,
        hierarchy_level,
    );
}

/// 将 GDS box 转成内部 shape。
fn push_gds_box(
    shapes: &mut Vec<RectShape>,
    gds_box: &GDSBox,
    transform: Transform2D,
    hierarchy_level: u32,
) {
    let points = transform_i32_points(&gds_box.xy, transform);
    push_outline_shape(
        shapes,
        LayerId {
            layer: gds_box.layer as u32,
            datatype: gds_box.boxtype as u32,
        },
        points,
        true,
        None,
        hierarchy_level,
    );
}

/// 将 GDS path 转成内部折线。
///
/// 这里不会直接生成“厚线填充几何”，而是保留中心线和世界坐标线宽，
/// 把真正的粗线三角形生成推迟到 renderer 阶段。
/// 这样可以让渲染层根据当前 zoom 动态调整屏幕线宽。
fn push_gds_path(
    shapes: &mut Vec<RectShape>,
    path: &GPath,
    transform: Transform2D,
    hierarchy_level: u32,
) {
    let points = transform_i32_points(&path.xy, transform);
    let base_width = path.width.unwrap_or_default().max(0) as f32;
    let scaled_width = (base_width * transform.uniform_scale()).max(1.0);
    let half_width = scaled_width * 0.5;

    push_outline_shape(
        shapes,
        LayerId {
            layer: path.layer as u32,
            datatype: path.datatype as u32,
        },
        points,
        false,
        Some((scaled_width, half_width)),
        hierarchy_level,
    );
}

/// 将 OASIS rectangle 转成内部矩形轮廓。
fn push_rectangle(
    shapes: &mut Vec<RectShape>,
    rectangle: &Rectangle,
    transform: Transform2D,
    hierarchy_level: u32,
) -> Result<(), AppError> {
    let x0 = rectangle.x as f32;
    let y0 = rectangle.y as f32;
    let x1 = x0 + rectangle.width as f32;
    let y1 = y0 + rectangle.height as f32;

    for delta in repetition_offsets(rectangle.repetition.as_ref())? {
        let points = vec![
            transform.apply_point(Vec2::new(x0, y0) + delta),
            transform.apply_point(Vec2::new(x1, y0) + delta),
            transform.apply_point(Vec2::new(x1, y1) + delta),
            transform.apply_point(Vec2::new(x0, y1) + delta),
        ];

        push_outline_shape(
            shapes,
            LayerId {
                layer: rectangle.layer as u32,
                datatype: rectangle.datatype as u32,
            },
            points,
            true,
            None,
            hierarchy_level,
        );
    }

    Ok(())
}


/// 将 OASIS polygon 转成内部闭合轮廓。
///
/// `laykit` 的 OASIS polygon 使用的是：
/// - `x/y` 作为基点
/// - `points` 作为相对点列
///
/// 所以这里要先把相对点还原到局部坐标，再进入统一变换链。
fn push_polygon(
    shapes: &mut Vec<RectShape>,
    polygon: &Polygon,
    transform: Transform2D,
    hierarchy_level: u32,
) -> Result<(), AppError> {
    let local_points: Vec<Vec2> = polygon
        .points
        .iter()
        .map(|&(px, py)| Vec2::new(polygon.x as f32 + px as f32, polygon.y as f32 + py as f32))
        .collect();

    for delta in repetition_offsets(polygon.repetition.as_ref())? {
        let points = local_points
            .iter()
            .map(|point| transform.apply_point(*point + delta))
            .collect();

        push_outline_shape(
            shapes,
            LayerId {
                layer: polygon.layer as u32,
                datatype: polygon.datatype as u32,
            },
            points,
            true,
            None,
            hierarchy_level,
        );
    }

    Ok(())
}

/// 将 OASIS path 转成内部折线。
///
/// 当前这版先支持：
/// - 还原 `x/y + relative points`
/// - 保留世界坐标线宽
/// - 在实例 magnification 下正确缩放线宽
///
/// `extension_scheme` 暂时只记录在注释层面，
/// 还没有把端点延伸几何单独实现到 viewer 里。
fn push_oasis_path(
    shapes: &mut Vec<RectShape>,
    path: &OPath,
    transform: Transform2D,
    hierarchy_level: u32,
) -> Result<(), AppError> {
    let local_points = oasis_path_points(path);
    let base_width = path.half_width as f32 * 2.0;
    let scaled_width = (base_width * transform.uniform_scale()).max(1.0);
    let half_width = scaled_width * 0.5;

    match path.extension_scheme {
        ExtensionScheme::Flush | ExtensionScheme::HalfWidth | ExtensionScheme::Custom { .. } => {
            for delta in repetition_offsets(path.repetition.as_ref())? {
                let points = local_points
                    .iter()
                    .map(|point| transform.apply_point(*point + delta))
                    .collect();

                push_outline_shape(
                    shapes,
                    LayerId {
                        layer: path.layer as u32,
                        datatype: path.datatype as u32,
                    },
                    points,
                    false,
                    Some((scaled_width, half_width)),
                    hierarchy_level,
                );
            }
        }
    }

    Ok(())
}

/// 统一把一组点和元信息落成内部 shape。
///
/// 这样做的好处是：
/// - `Boundary` / `Box` / `Rectangle` / `Path` 共享同一套 bounds 生成逻辑
/// - 将来如果要支持更多图元，不用把“算 bounds / 建 shape”逻辑复制很多份
/// - hierarchy depth 也只需要在这一处稳定落盘
fn push_outline_shape(
    shapes: &mut Vec<RectShape>,
    layer: LayerId,
    points: Vec<Vec2>,
    closed: bool,
    stroke: Option<(f32, f32)>,
    hierarchy_level: u32,
) {
    let Some(mut bounds) = bounds_from_points(&points) else {
        return;
    };

    let stroke_width_world = if let Some((stroke_width_world, half_width)) = stroke {
        bounds = bounds.pad(half_width);
        Some(stroke_width_world)
    } else {
        None
    };

    shapes.push(RectShape {
        layer,
        hierarchy_level,
        bounds,
        points,
        closed,
        stroke_width_world,
    });
}

/// 把 GDS 的整数点列转换成经过实例变换后的点列。
fn transform_i32_points(points: &[(i32, i32)], transform: Transform2D) -> Vec<Vec2> {
    points
        .iter()
        .map(|&(x, y)| transform.apply_point(Vec2::new(x as f32, y as f32)))
        .collect()
}

/// 还原 OASIS path 的局部点列。
///
/// OASIS path 的 `points` 也是相对 `x/y` 基点存储的，
/// 所以这里先拼出完整局部坐标。
fn oasis_path_points(path: &OPath) -> Vec<Vec2> {
    path.points
        .iter()
        .map(|&(px, py)| Vec2::new(path.x as f32 + px as f32, path.y as f32 + py as f32))
        .collect()
}

/// 把 OASIS repetition 还原成一组附加位移。
///
/// 这里故意返回“包含原始实例本身”的位移列表，
/// 也就是第一个元素通常是 `Vec2::ZERO`。
/// 这样调用方就可以统一写成：
/// - 枚举每个 delta
/// - 把 delta 加到当前实例模板上
/// - 生成一个真实 shape / placement
///
/// 当前对 `ReusePrevious` 的策略是显式报错。
/// 原因是它需要引用前一个 repetition 记录，
/// 而当前最小导入层还没有保存这类跨元素状态。
fn repetition_offsets(repetition: Option<&Repetition>) -> Result<Vec<Vec2>, AppError> {
    let Some(repetition) = repetition else {
        return Ok(vec![Vec2::ZERO]);
    };

    match repetition {
        Repetition::ReusePrevious => Err(AppError::Parse(
            "OASIS ReusePrevious repetition is not supported yet".to_string(),
        )),
        Repetition::Matrix {
            x_count,
            y_count,
            x_space,
            y_space,
        } => {
            let cols = (*x_count).max(1) as usize;
            let rows = (*y_count).max(1) as usize;
            let x_step = *x_space as f32;
            let y_step = *y_space as f32;
            let mut offsets = Vec::with_capacity(cols * rows);
            for row in 0..rows {
                for col in 0..cols {
                    offsets.push(Vec2::new(col as f32 * x_step, row as f32 * y_step));
                }
            }
            Ok(offsets)
        }
        Repetition::Arbitrary {
            x_displacements,
            y_displacements,
        } => {
            let len = x_displacements.len().max(y_displacements.len()).max(1);
            let mut offsets = Vec::with_capacity(len);
            for index in 0..len {
                let x = x_displacements.get(index).copied().unwrap_or_default() as f32;
                let y = y_displacements.get(index).copied().unwrap_or_default() as f32;
                offsets.push(Vec2::new(x, y));
            }
            Ok(offsets)
        }
        Repetition::Grid { count, grid_space } => {
            let count = (*count).max(1) as usize;
            let step = *grid_space as f32;
            // `laykit` 当前暴露的 Grid 只有一个标量步长，没有方向字段。
            // 这里先按 X 方向阵列解释，保证行为稳定、可验证。
            Ok((0..count)
                .map(|index| Vec2::new(index as f32 * step, 0.0))
                .collect())
        }
    }
}

/// 计算 `ArrayRef` 的每个实例偏移量。
///
/// GDS 的阵列引用给出的不是“每个点的完整坐标表”，
/// 而是 origin、最后一列方向、最后一行方向等信息。
/// 这里把它还原成每个实例真正的位移向量。
fn array_ref_offsets(aref: &ArrayRef) -> Vec<Vec2> {
    let origin = aref
        .xy
        .first()
        .map(|&(x, y)| Vec2::new(x as f32, y as f32))
        .unwrap_or(Vec2::ZERO);
    let cols = aref.columns.max(1) as usize;
    let rows = aref.rows.max(1) as usize;

    if cols == 1 && rows == 1 {
        return vec![origin];
    }

    let col_step = if aref.xy.len() >= 2 && cols > 1 {
        let array_col_vector = Vec2::new(aref.xy[1].0 as f32, aref.xy[1].1 as f32) - origin;
        // GDS AREF 的第二个点表示“参考点 + 列间距 * 列数”，
        // 不是“最后一列实例中心”。
        // 所以这里要除以 `columns`，而不是 `columns - 1`。
        array_col_vector / cols as f32
    } else {
        Vec2::ZERO
    };
    let row_step = if aref.xy.len() >= 3 && rows > 1 {
        let array_row_vector = Vec2::new(aref.xy[2].0 as f32, aref.xy[2].1 as f32) - origin;
        // 同理，第三个点表示“参考点 + 行间距 * 行数”。
        array_row_vector / rows as f32
    } else {
        Vec2::ZERO
    };

    let mut offsets = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        for col in 0..cols {
            offsets.push(origin + col_step * col as f32 + row_step * row as f32);
        }
    }
    offsets
}

/// 从浮点点集生成 bounds。
fn bounds_from_points(points: &[Vec2]) -> Option<Bounds> {
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

/// 旋转一个向量。
fn rotate_vector(vector: Vec2, angle_radians: f32) -> Vec2 {
    let (sin, cos) = angle_radians.sin_cos();
    Vec2::new(
        vector.x * cos - vector.y * sin,
        vector.x * sin + vector.y * cos,
    )
}

#[cfg(test)]
mod tests {
    use glam::Vec2;
    use laykit::{
        ArrayRef, Boundary, ExtensionScheme, GDSElement, GDSIIFile, GDSStructure, GDSTime, GPath,
        OASISCell, OASISElement, OASISFile, OPath, Placement, Polygon, Rectangle, Repetition,
        STrans, StructRef,
    };

    use super::{build_gds_scene_bundle, build_oasis_scene_bundle};
    use crate::scene::Bounds;

    /// 回归测试：
    /// - 只把 root cells 暴露给 UI
    /// - `StructRef` 会被正确展开成最终几何
    #[test]
    fn gds_bundle_uses_only_root_cells_and_flattens_struct_refs() {
        let file = GDSIIFile {
            version: 600,
            modification_time: sample_time(),
            access_time: sample_time(),
            library_name: "demo".to_string(),
            units: (1e-6, 1e-9),
            reflibs: Vec::new(),
            fonts: Vec::new(),
            generations: None,
            attrtable: None,
            structures: vec![
                GDSStructure {
                    name: "leaf".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::Boundary(Boundary {
                        layer: 1,
                        datatype: 0,
                        xy: vec![(0, 0), (10, 0), (10, 20), (0, 20), (0, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
                GDSStructure {
                    name: "top".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::StructRef(StructRef {
                        sname: "leaf".to_string(),
                        xy: (100, 200),
                        strans: None,
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
                GDSStructure {
                    name: "standalone".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::Boundary(Boundary {
                        layer: 2,
                        datatype: 0,
                        xy: vec![(50, 50), (80, 50), (80, 90), (50, 90), (50, 50)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
            ],
        };

        let bundle = build_gds_scene_bundle(&file).expect("bundle");
        let names: Vec<_> = bundle
            .views()
            .iter()
            .map(|view| view.name.as_str())
            .collect();
        assert_eq!(names, vec!["top", "standalone"]);

        let top_scene = &bundle.views()[0].scene;
        assert_eq!(top_scene.stats().shape_count, 1);
        assert_eq!(top_scene.shapes()[0].points.len(), 5);
        assert_eq!(top_scene.shapes()[0].hierarchy_level, 1);
        let bounds = top_scene.bounds().expect("bounds");
        assert_eq!(bounds.min_x, 100.0);
        assert_eq!(bounds.min_y, 200.0);
        assert_eq!(bounds.max_x, 110.0);
        assert_eq!(bounds.max_y, 220.0);

        let standalone_scene = &bundle.views()[1].scene;
        assert_eq!(standalone_scene.shapes()[0].hierarchy_level, 0);
    }

    /// 回归测试：
    /// GDS `AREF` 的第二、第三个坐标表示的是“阵列跨度向量”，
    /// 也就是 `reference + step * columns/rows`，而不是“最后一个实例中心”。
    ///
    /// 所以步长要除以 `columns/rows`，不能除以 `columns - 1 / rows - 1`。
    #[test]
    fn gds_array_ref_uses_full_array_span_for_column_and_row_steps() {
        let file = GDSIIFile {
            version: 600,
            modification_time: sample_time(),
            access_time: sample_time(),
            library_name: "demo".to_string(),
            units: (1e-6, 1e-9),
            reflibs: Vec::new(),
            fonts: Vec::new(),
            generations: None,
            attrtable: None,
            structures: vec![
                GDSStructure {
                    name: "leaf".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::Boundary(Boundary {
                        layer: 1,
                        datatype: 0,
                        xy: vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
                GDSStructure {
                    name: "top".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::ArrayRef(ArrayRef {
                        sname: "leaf".to_string(),
                        columns: 4,
                        rows: 4,
                        xy: vec![(0, 0), (400, 0), (0, 400)],
                        strans: None,
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
            ],
        };

        let bundle = build_gds_scene_bundle(&file).expect("bundle");
        let scene = &bundle.views()[0].scene;
        let centers: Vec<Vec2> = scene
            .shapes()
            .iter()
            .map(|shape| Vec2::new(
                (shape.bounds.min_x + shape.bounds.max_x) * 0.5,
                (shape.bounds.min_y + shape.bounds.max_y) * 0.5,
            ))
            .collect();

        assert_eq!(centers.len(), 16);
        assert!(centers.contains(&Vec2::new(5.0, 5.0)));
        assert!(centers.contains(&Vec2::new(105.0, 5.0)));
        assert!(centers.contains(&Vec2::new(305.0, 305.0)));
        assert!(!centers.contains(&Vec2::new(405.0, 405.0)));
    }

    #[test]
    fn gds_struct_ref_applies_strans_to_child_geometry() {
        let file = GDSIIFile {
            version: 600,
            modification_time: sample_time(),
            access_time: sample_time(),
            library_name: "demo".to_string(),
            units: (1e-6, 1e-9),
            reflibs: Vec::new(),
            fonts: Vec::new(),
            generations: None,
            attrtable: None,
            structures: vec![
                GDSStructure {
                    name: "leaf".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::Boundary(Boundary {
                        layer: 1,
                        datatype: 0,
                        xy: vec![(0, 0), (10, 0), (10, 20), (0, 20), (0, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
                GDSStructure {
                    name: "top".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::StructRef(StructRef {
                        sname: "leaf".to_string(),
                        xy: (100, 200),
                        strans: Some(STrans {
                            reflection: true,
                            absolute_magnification: false,
                            absolute_angle: false,
                            magnification: Some(2.0),
                            angle: Some(90.0),
                        }),
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
            ],
        };

        let bundle = build_gds_scene_bundle(&file).expect("bundle");
        let shape = &bundle.views()[0].scene.shapes()[0];
        let bounds = shape.bounds;

        assert_bounds_close(bounds, (100.0, 200.0, 140.0, 220.0));
        assert_eq!(shape.points.len(), 5);
        assert_vec2_close(shape.points[0], Vec2::new(100.0, 200.0));
        assert_vec2_close(shape.points[1], Vec2::new(100.0, 220.0));
        assert_vec2_close(shape.points[2], Vec2::new(140.0, 220.0));
        assert_vec2_close(shape.points[3], Vec2::new(140.0, 200.0));
    }

    /// 回归测试：
    /// path 的世界线宽也必须跟着 magnification 一起变化，
    /// 否则缩放过的实例会出现“位置对了但线宽不对”的问题。
    #[test]
    fn gds_path_width_scales_with_strans_magnification() {
        let file = GDSIIFile {
            version: 600,
            modification_time: sample_time(),
            access_time: sample_time(),
            library_name: "demo".to_string(),
            units: (1e-6, 1e-9),
            reflibs: Vec::new(),
            fonts: Vec::new(),
            generations: None,
            attrtable: None,
            structures: vec![
                GDSStructure {
                    name: "leaf".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::Path(GPath {
                        layer: 3,
                        datatype: 1,
                        pathtype: 0,
                        width: Some(4),
                        bgnextn: None,
                        endextn: None,
                        xy: vec![(0, 0), (10, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
                GDSStructure {
                    name: "top".to_string(),
                    creation_time: sample_time(),
                    modification_time: sample_time(),
                    strclass: None,
                    elements: vec![GDSElement::StructRef(StructRef {
                        sname: "leaf".to_string(),
                        xy: (0, 0),
                        strans: Some(STrans {
                            reflection: false,
                            absolute_magnification: false,
                            absolute_angle: false,
                            magnification: Some(3.0),
                            angle: None,
                        }),
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    })],
                },
            ],
        };

        let bundle = build_gds_scene_bundle(&file).expect("bundle");
        let shape = &bundle.views()[0].scene.shapes()[0];
        assert_eq!(shape.stroke_width_world, Some(12.0));
        assert_bounds_close(shape.bounds, (-6.0, -6.0, 36.0, 6.0));
    }


    /// 回归测试：
    /// OASIS 的图元 repetition 不是元数据装饰，而是真的会复制出多个实例。
    /// 这里先锁住最常见的 `Matrix` 阵列展开。
    #[test]
    fn oasis_rectangle_matrix_repetition_expands_into_multiple_shapes() {
        let file = OASISFile {
            version: "1.0".to_string(),
            unit: 1.0,
            offset_flag: false,
            names: sample_name_table(),
            cells: vec![OASISCell {
                name: "top".to_string(),
                elements: vec![OASISElement::Rectangle(Rectangle {
                    layer: 10,
                    datatype: 0,
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 20,
                    repetition: Some(Repetition::Matrix {
                        x_count: 2,
                        y_count: 2,
                        x_space: 100,
                        y_space: 200,
                    }),
                    properties: Vec::new(),
                })],
            }],
            layers: Vec::new(),
            properties: Vec::new(),
        };

        let bundle = build_oasis_scene_bundle(&file).expect("bundle");
        let scene = &bundle.views()[0].scene;
        assert_eq!(scene.stats().shape_count, 4);
        assert_bounds_close(scene.bounds().expect("bounds"), (0.0, 0.0, 110.0, 220.0));
    }

    /// 回归测试：
    /// `Placement` 自己也可以带 repetition，
    /// 也就是“同一个子 cell 用同一组局部变换复制多次”。
    #[test]
    fn oasis_placement_grid_repetition_expands_child_instances() {
        let file = OASISFile {
            version: "1.0".to_string(),
            unit: 1.0,
            offset_flag: false,
            names: sample_name_table(),
            cells: vec![
                OASISCell {
                    name: "leaf".to_string(),
                    elements: vec![OASISElement::Rectangle(Rectangle {
                        layer: 2,
                        datatype: 0,
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 10,
                        repetition: None,
                        properties: Vec::new(),
                    })],
                },
                OASISCell {
                    name: "top".to_string(),
                    elements: vec![OASISElement::Placement(Placement {
                        cell_name: "leaf".to_string(),
                        x: 50,
                        y: 80,
                        magnification: None,
                        angle: None,
                        mirror: false,
                        repetition: Some(Repetition::Grid {
                            count: 3,
                            grid_space: 40,
                        }),
                        properties: Vec::new(),
                    })],
                },
            ],
            layers: Vec::new(),
            properties: Vec::new(),
        };

        let bundle = build_oasis_scene_bundle(&file).expect("bundle");
        let scene = &bundle.views()[0].scene;
        assert_eq!(scene.stats().shape_count, 3);
        assert_bounds_close(scene.bounds().expect("bounds"), (50.0, 80.0, 140.0, 90.0));
    }


    /// 回归测试：
    /// OASIS Polygon 需要把 `x/y` 基点和相对点列拼成真实轮廓，
    /// 并且 repetition 也要继续生效。
    #[test]
    fn oasis_polygon_arbitrary_repetition_expands_and_offsets_points() {
        let file = OASISFile {
            version: "1.0".to_string(),
            unit: 1.0,
            offset_flag: false,
            names: sample_name_table(),
            cells: vec![OASISCell {
                name: "top".to_string(),
                elements: vec![OASISElement::Polygon(Polygon {
                    layer: 6,
                    datatype: 1,
                    x: 10,
                    y: 20,
                    points: vec![(0, 0), (30, 0), (30, 10), (0, 10), (0, 0)],
                    repetition: Some(Repetition::Arbitrary {
                        x_displacements: vec![0, 100],
                        y_displacements: vec![0, 50],
                    }),
                    properties: Vec::new(),
                })],
            }],
            layers: Vec::new(),
            properties: Vec::new(),
        };

        let bundle = build_oasis_scene_bundle(&file).expect("bundle");
        let scene = &bundle.views()[0].scene;
        assert_eq!(scene.stats().shape_count, 2);
        assert_bounds_close(scene.bounds().expect("bounds"), (10.0, 20.0, 140.0, 80.0));
        assert_vec2_close(scene.shapes()[1].points[0], Vec2::new(110.0, 70.0));
    }

    /// 回归测试：
    /// OASIS Path 需要保留世界线宽，
    /// 并且在实例 magnification 下跟着一起缩放。
    #[test]
    fn oasis_path_preserves_world_stroke_width_under_placement_transform() {
        let file = OASISFile {
            version: "1.0".to_string(),
            unit: 1.0,
            offset_flag: false,
            names: sample_name_table(),
            cells: vec![
                OASISCell {
                    name: "leaf".to_string(),
                    elements: vec![OASISElement::Path(OPath {
                        layer: 8,
                        datatype: 0,
                        x: 5,
                        y: 10,
                        half_width: 2,
                        extension_scheme: ExtensionScheme::Flush,
                        points: vec![(0, 0), (20, 0), (20, 10)],
                        repetition: None,
                        properties: Vec::new(),
                    })],
                },
                OASISCell {
                    name: "top".to_string(),
                    elements: vec![OASISElement::Placement(Placement {
                        cell_name: "leaf".to_string(),
                        x: 100,
                        y: 200,
                        magnification: Some(2.0),
                        angle: None,
                        mirror: false,
                        repetition: None,
                        properties: Vec::new(),
                    })],
                },
            ],
            layers: Vec::new(),
            properties: Vec::new(),
        };

        let bundle = build_oasis_scene_bundle(&file).expect("bundle");
        let shape = &bundle.views()[0].scene.shapes()[0];
        assert_eq!(shape.stroke_width_world, Some(8.0));
        assert_bounds_close(shape.bounds, (106.0, 216.0, 154.0, 244.0));
        assert_vec2_close(shape.points[0], Vec2::new(110.0, 220.0));
        assert_vec2_close(shape.points[2], Vec2::new(150.0, 240.0));
    }

    fn assert_bounds_close(bounds: Bounds, expected: (f32, f32, f32, f32)) {
        assert!((bounds.min_x - expected.0).abs() < 0.01);
        assert!((bounds.min_y - expected.1).abs() < 0.01);
        assert!((bounds.max_x - expected.2).abs() < 0.01);
        assert!((bounds.max_y - expected.3).abs() < 0.01);
    }

    fn assert_vec2_close(actual: Vec2, expected: Vec2) {
        assert!((actual.x - expected.x).abs() < 0.01, "x mismatch: {actual:?} vs {expected:?}");
        assert!((actual.y - expected.y).abs() < 0.01, "y mismatch: {actual:?} vs {expected:?}");
    }

    fn sample_name_table() -> laykit::NameTable {
        laykit::NameTable {
            cell_names: std::collections::HashMap::new(),
            text_strings: std::collections::HashMap::new(),
            prop_names: std::collections::HashMap::new(),
            prop_strings: std::collections::HashMap::new(),
            layer_names: std::collections::HashMap::new(),
        }
    }

    fn sample_time() -> GDSTime {
        GDSTime {
            year: 2026,
            month: 4,
            day: 30,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }
}
