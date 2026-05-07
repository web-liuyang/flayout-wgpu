//! 内部统一场景模型。
//!
//! 这个模块是整个项目非常关键的“中间层”：
//! - `io` 把 GDS / OASIS 转成这里的结构
//! - `renderer` 只消费这里的结构
//! - `ui` 也通过这里拿统计信息、layer 列表、bounds 等
//!
//! 为什么一定要有这层？
//! 因为版图解析库的原始数据结构往往会带很多格式细节，
//! 如果渲染层直接依赖它们，后面很容易牵一发动全身。

use std::{collections::BTreeSet, sync::Arc};

use glam::Vec2;

/// 一个轴对齐包围盒。
///
/// 目前它既用于：
/// - 整个 scene 的可见范围
/// - 单个 shape 的快速裁剪
/// - 相机 fit
/// - 空间索引
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

/// 工艺层标识。
///
/// 在版图里，通常要同时看 `layer` 和 `datatype`，
/// 所以这里把它们绑定成一个排序友好的键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerId {
    pub layer: u32,
    pub datatype: u32,
}

/// 每个 layer 的可视化覆盖配置。
///
/// 当前我们只先覆盖"闭合图形怎么画"这一项：
/// - 有些大包层更适合只画轮廓
/// - 有些真正关心的工艺层适合 hatch 或 hatch + outline
///
/// 这里先保持结构最小化，后面如果你想继续扩成
/// 每层单独颜色、单独 hatch 样式，也可以继续往这个方向长。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerDisplayOverride<T> {
    pub layer: LayerId,
    pub value: T,
}

/// 查看器当前的基础图元表示。
///
/// 这里名字虽然叫 `RectShape`，但现在其实承载的是更泛化的“折线/闭合轮廓”。
/// 之所以暂时没有改成更宽泛的名字，是因为这个 demo 是从矩形/包围盒版本逐步演进过来的。
/// 后面如果你继续扩展到多边形填充，可以考虑重命名为 `Shape`。
#[derive(Debug, Clone)]
pub struct RectShape {
    /// 所属 layer/datatype。
    pub layer: LayerId,

    /// 这个图形来自实例层级树中的哪一层。
    ///
    /// 定义约定：
    /// - `0`：当前 root cell 自己直接包含的图形
    /// - `1`：root 直接引用的子 cell 内的图形
    /// - `2`：再下一层
    ///
    /// 这个字段的主要用途有两个：
    /// 1. 调试层级展开是否正确
    /// 2. 做 level range 过滤，帮助性能与可视化排查
    pub hierarchy_level: u32,

    /// 用于裁剪、fit、索引的快速 bounds。
    pub bounds: Bounds,

    /// 图形的点序列。
    ///
    /// - 对闭合 shape，这是轮廓点
    /// - 对 path，这是中心线点序列
    pub points: Vec<Vec2>,

    /// 是否闭合。
    pub closed: bool,

    /// path 类型在世界坐标中的线宽。
    ///
    /// 对 boundary / box 一类轮廓，这里一般为 `None`，
    /// 渲染层会按默认轮廓线宽处理。
    pub stroke_width_world: Option<f32>,
}

/// 场景统计信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneStats {
    pub shape_count: usize,
}

/// 一个具体可渲染的场景。
#[derive(Debug, Clone, Default)]
pub struct Scene {
    shapes: Vec<RectShape>,
}

/// 一个可切换的视图项，通常对应一个 root cell。
#[derive(Debug, Clone)]
pub struct SceneView {
    pub name: String,
    /// 这里用 `Arc<Scene>`，避免 bundle / app / renderer 之间为同一个 root cell
    /// 重复拷贝整份几何数据。
    pub scene: Arc<Scene>,
}

/// 一个版图文件展开后的“可选场景集合”。
///
/// 多个 root cell 时，UI 用它来切换当前显示的 cell。
#[derive(Debug, Clone, Default)]
pub struct SceneBundle {
    views: Vec<SceneView>,
    selected: usize,
}

impl RectShape {
    /// 用一组点直接构造内部图元。
    ///
    /// 这个构造函数主要给层次化 `view_builder` 用：
    /// - loader 端先保留 cell 本地点列
    /// - 真正进入临时扁平 `Scene` 时，再在这里补齐 bounds 和层级深度
    pub fn from_points(
        layer: LayerId,
        points: Vec<Vec2>,
        closed: bool,
        hierarchy_level: u32,
        stroke_width_world: Option<f32>,
    ) -> Self {
        let mut bounds = bounds_from_points(&points).unwrap_or(Bounds::new(0.0, 0.0, 0.0, 0.0));
        if let Some(width) = stroke_width_world {
            bounds = bounds.pad(width * 0.5);
        }
        Self {
            layer,
            hierarchy_level,
            bounds,
            points,
            closed,
            stroke_width_world,
        }
    }

    /// 构造一个矩形轮廓。
    ///
    /// 这里直接按四个角点生成顺时针点列，
    /// 渲染层会根据 `closed = true` 自动补最后一条边。
    pub fn rectangle(layer: LayerId, bounds: Bounds) -> Self {
        Self {
            layer,
            hierarchy_level: 0,
            bounds,
            points: vec![
                Vec2::new(bounds.min_x, bounds.min_y),
                Vec2::new(bounds.max_x, bounds.min_y),
                Vec2::new(bounds.max_x, bounds.max_y),
                Vec2::new(bounds.min_x, bounds.max_y),
            ],
            closed: true,
            stroke_width_world: None,
        }
    }

    /// 构造一条折线。
    pub fn polyline(layer: LayerId, points: Vec<Vec2>, stroke_width_world: f32) -> Self {
        Self::from_points(layer, points, false, 0, Some(stroke_width_world))
    }
}

impl Scene {
    /// 空场景。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从一组 shape 创建场景。
    pub fn from_shapes(shapes: Vec<RectShape>) -> Self {
        Self { shapes }
    }

    /// 只读访问底层 shape 列表。
    pub fn shapes(&self) -> &[RectShape] {
        &self.shapes
    }

    /// 返回场景的基础统计信息。
    pub fn stats(&self) -> SceneStats {
        SceneStats {
            shape_count: self.shapes.len(),
        }
    }

    /// 计算整个场景的包围盒。
    ///
    /// 这里每次现算而不是缓存，是因为当前 demo 的场景对象比较小、修改频率也低。
    /// 将来如果要做增量编辑器，再考虑专门缓存 bounds。
    pub fn bounds(&self) -> Option<Bounds> {
        let mut iter = self.shapes.iter();
        let first = iter.next()?;
        let mut bounds = first.bounds;
        for shape in iter {
            bounds = bounds.union(shape.bounds);
        }
        Some(bounds)
    }

    /// 返回当前场景里出现过的 layer 列表，并按自然顺序排序。
    pub fn layer_ids(&self) -> Vec<LayerId> {
        let mut layers = BTreeSet::new();
        for shape in &self.shapes {
            layers.insert(shape.layer);
        }
        layers.into_iter().collect()
    }

    /// 返回当前 scene 中出现过的最大层级深度。
    pub fn max_hierarchy_level(&self) -> u32 {
        self.shapes
            .iter()
            .map(|shape| shape.hierarchy_level)
            .max()
            .unwrap_or(0)
    }

    /// 统计场景中所有点的总数。
    ///
    /// 这里不区分闭合轮廓还是开放折线，因为对“初始化时判断版图是否偏大”来说，
    /// 点总量本身就是一个足够有价值的粗略复杂度指标。
    pub fn total_point_count(&self) -> usize {
        self.shapes.iter().map(|shape| shape.points.len()).sum()
    }

    /// 按层级深度过滤 scene。
    ///
    /// 这里返回一个新的 `Scene`，而不是在 renderer 内部再做一次运行时过滤，
    /// 是因为：
    /// - 这样 renderer 可以继续把“当前 scene”当成完整输入来处理
    /// - level range 切换会自然进入现有的索引、缓存、统计链
    pub fn filtered_by_hierarchy_range(&self, min_level: u32, max_level: u32) -> Self {
        let shapes = self
            .shapes
            .iter()
            .filter(|shape| {
                shape.hierarchy_level >= min_level && shape.hierarchy_level <= max_level
            })
            .cloned()
            .collect();
        Self { shapes }
    }
}

impl SceneBundle {
    /// 空 bundle。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从多个视图构造 bundle。
    pub fn new(views: Vec<SceneView>) -> Self {
        Self { views, selected: 0 }
    }

    /// 只有一个场景时的便捷构造函数。
    pub fn single(name: impl Into<String>, scene: Scene) -> Self {
        Self::new(vec![SceneView {
            name: name.into(),
            scene: Arc::new(scene),
        }])
    }

    /// 全部可选视图。
    pub fn views(&self) -> &[SceneView] {
        &self.views
    }

    /// 当前选中的视图索引。
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// 当前选中的视图。
    pub fn current_view(&self) -> Option<&SceneView> {
        self.views.get(self.selected)
    }

    /// 当前选中的场景。
    pub fn current_scene(&self) -> Option<&Scene> {
        self.current_view().map(|view| view.scene.as_ref())
    }

    /// 当前选中场景的共享句柄。
    ///
    /// app 层需要拿到这个句柄再往 renderer 传递，
    /// 这样可以避免 bundle -> app -> renderer 之间继续复制完整场景。
    pub fn current_scene_handle(&self) -> Option<Arc<Scene>> {
        self.current_view().map(|view| Arc::clone(&view.scene))
    }

    /// 只保留当前选中 view 的真实 scene，其余 view 退化成空 scene 占位。
    ///
    /// 这样做的目的是在大版图下尽快释放未选中 root cell 的展开结果，
    /// 避免 `SceneBundle` 为了保留 cell 列表而常驻整份扁平几何。
    pub fn retain_only_selected_scene(&mut self, selected_scene: Arc<Scene>) {
        for (index, view) in self.views.iter_mut().enumerate() {
            if index == self.selected {
                view.scene = Arc::clone(&selected_scene);
            } else {
                view.scene = Arc::new(Scene::empty());
            }
        }
    }

    /// 切换视图。
    ///
    /// 返回值表示“是否真的发生了切换”，
    /// 这样上层就能避免重复刷新和重复重建缓存。
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.views.len() || index == self.selected {
            return false;
        }
        self.selected = index;
        true
    }
}

impl Bounds {
    /// 创建一个 bounds。
    pub fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// 宽度。
    pub fn width(&self) -> f32 {
        self.max_x - self.min_x
    }

    /// 高度。
    pub fn height(&self) -> f32 {
        self.max_y - self.min_y
    }

    /// 中心点。
    pub fn center(&self) -> Vec2 {
        Vec2::new(
            (self.min_x + self.max_x) * 0.5,
            (self.min_y + self.max_y) * 0.5,
        )
    }

    /// 平移一个 bounds。
    pub fn translate(self, delta: Vec2) -> Bounds {
        Bounds {
            min_x: self.min_x + delta.x,
            min_y: self.min_y + delta.y,
            max_x: self.max_x + delta.x,
            max_y: self.max_y + delta.y,
        }
    }

    /// 在四周扩一圈。
    ///
    /// path 线宽会影响实际可见范围，所以这里常用于把中心线扩成带宽度的包围盒。
    pub fn pad(self, amount: f32) -> Bounds {
        Bounds {
            min_x: self.min_x - amount,
            min_y: self.min_y - amount,
            max_x: self.max_x + amount,
            max_y: self.max_y + amount,
        }
    }

    /// 判断两个 bounds 是否相交。
    pub fn intersects(self, other: Bounds) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }

    /// 合并两个 bounds。
    pub fn union(self, other: Bounds) -> Bounds {
        Bounds {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }
}

/// 从点集计算最小包围盒。
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
