# Rendering Architecture

这份文档的目标不是替代源码，而是帮你先建立一张“从文件加载到最终上屏”的心智地图。

## 总览

当前 viewer 大致分成四层：

1. `app`
2. `layout`
3. `renderer::geometry`
4. `renderer`

可以把它们理解成下面这条链路：

```text
Layout file / SceneBundle
  -> app 选择当前 view / hierarchy range / camera 策略
  -> layout 按需展开成 workset，或者提供 hierarchy source
  -> geometry 做 tile 查询 / 顶点生成 / LOD / clipping
  -> renderer 做 cache、progressive build、GPU draw
```

## 1. app 层在做什么

入口文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`

这一层最重要的职责不是“渲染”，而是**决定走哪条渲染路径**。

### 小中型场景

当当前 root + hierarchy range 的预估成本还比较低时，app 会：

1. 用 `LayoutViewBuildOptions`
2. 调 `build_layout_view_scene()`
3. 构一个临时 `Scene`
4. 交给 renderer 走普通 scene 路径

这条路径的优点是：
- 逻辑简单
- 调试方便
- 和旧 flat scene 模型更一致

缺点是：
- 大场景下会同时持有完整 flat scene、spatial index、tile grid、tile cache

### 大场景

当当前 range 预估成本太高时，app 会切到 `direct hierarchy tile render`：

1. 不再先构完整 flat `Scene`
2. 只给 renderer 一份 `HierarchyTileSource`
3. renderer 按 tile 按需向 hierarchy 请求几何

这条路径的目标是：
- 避免 app 层先常驻巨大 workset
- 把内存和 CPU 成本尽量收敛到“当前可见 tile”

### workset 重建策略

app 里还有一层很重要的优化：

- 视口会带 `prefetch margin`
- 小幅 pan/zoom 先复用旧 workset
- 只有当视口超出 coverage，或者 zoom 偏离太多时，才真正重建

这样做的原因是：
- workset 重建本身也很贵
- 如果每次滚轮或拖拽都同步重建，交互会明显变钝

## 2. layout 层在做什么

关键文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/layout/mod.rs`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/layout/view_builder.rs`

这一层的核心思想是：

> 保留 hierarchy 语义，按需展开，而不是在 loader 阶段立刻 eager flatten。

### LayoutBundle

`LayoutBundle` 保存的是：
- cell
- instance
- transform
- repetition
- root views

也就是说，它更像一个“分层版图数据库”，不是 renderer 可直接绘制的数据。

### view_builder

`view_builder` 提供三条能力：

1. `build_layout_view_scene()`
   - 真正构一个临时 flat `Scene`
2. `visit_layout_shape_bounds_in_view()`
   - 只遍历 `layer + bounds`
   - 用来做轻量摘要，比如 `tile -> layers`
3. `visit_layout_shapes_in_view()`
   - 逐个流式吐出展开后的 `RectShape`
   - 不先落成完整 `Scene`
   - direct hierarchy tile worker 会用这条路径直接产顶点

### 两层 LOD

layout 这里处理的是**子树级 LOD**：

- 一个 hierarchy 子树在屏幕上已经非常小
- 而且它已经足够深
- 就不再继续往下递归展开

这和 renderer 里的 shape LOD 不一样：

- layout LOD：少展开一些 shape
- geometry LOD：shape 已经要画了，但少发一些点

## 3. geometry 层在做什么

关键文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`

这是“渲染前数据准备层”，主要做四类事：

1. spatial index / tile grid
2. 可见查询
3. shape -> vertex
4. 坐标变换与 clipping

### ShapeSpatialIndex

它是一个均匀网格索引，用来快速回答：

> 当前视口里，哪些 shape 值得进一步检查？

它不是通用 R-tree，选择 uniform grid 的原因是：
- 实现简单
- 对这个项目当前规模足够有效
- 便于学习和调试

### TileGridIndex

它回答的是另一个问题：

> 如果我们要按 tile 缓存几何，每个 tile 里有哪些 layer、哪些 shape？

它和 `ShapeSpatialIndex` 的职责不同：
- `ShapeSpatialIndex` 偏“找到可能可见的 shape”
- `TileGridIndex` 偏“按 tile 为 cache 和 draw 组织几何”

### 顶点生成

`emit_scaled_shape_vertices()` 是 geometry 里最关键的入口。

它同时做了这些事：

1. 世界坐标 -> 当前 zoom 下的逻辑屏幕坐标
2. 必要时按 tile bounds 裁剪
3. 决定是否使用：
   - tiny marker
   - coarse LOD
   - suppress fill
4. 最终发出：
   - outline 顶点
   - hatch fill 顶点

### 远景退化的三档策略

当前闭合图形在远景下大致有三档：

1. 正常 `Hatch / HatchOutline`
2. suppress fill，只保留 outline
3. tiny marker

这么做的动机很现实：
- 大场景远景下，hatch 细节通常没有信息密度
- 但完整 fill 三角形成本非常高

### 预碎片化

`PreparedTileFragments` 是为超大 shape 做的世界坐标预切块：

- 普通路径：tile miss 时现场裁剪
- 预碎片化路径：scene / tile grid 变化时先切好

这样做适合“横跨很多 tile 的大 polygon”，
但也会增加一份内存副本，所以它还有 fragment / point budget 保护。

## 4. renderer 层在做什么

关键文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`

这一层真正负责编排：

- 当前帧是否要重算可见结果
- 哪些 tile key 该显示
- 哪些 tile key 该后台构建
- 哪些 tile buffer 还能复用
- 最终怎么提交 draw

### 三层缓存

当前最值得记住的是这三层：

1. `cached_scene_key`
   - 这一帧是否要重新组织可见结果
2. `tile_cache_domain`
   - 已有 tile buffer 是否仍然有效
3. `tile_vertex_cache`
   - 真正的 `tile + layer -> GPU vertex buffers`

### progressive build

renderer 不会尝试一帧把所有缺失 tile 都建完。

它会：

1. 维护 `pending_tile_builds`
2. 每帧只消化一个预算
3. 优先完成当前 active layer

这样做的收益是：
- 交互时能更快给出画面反馈
- 不会因为一次全量补建直接拖跨整帧

### direct hierarchy worker

当 direct hierarchy 路径开启后：

- 主线程决定哪些 `tile + layer` 需要构建
- worker 线程做 hierarchy 遍历和 CPU 顶点生成
- 主线程回收结果并创建 GPU buffer

这样线程边界更干净：
- worker 不碰 wgpu 资源
- 主线程不做重的 hierarchy 顶点生成

### 静态显示合批

这是后来为了静态帧 GPU/present 瓶颈加的一层：

- 第一层 cache：`tile + layer -> vertex buffers`
- 第二层 cache：`VisibleDisplayBatch`

第二层只在这些条件下启用：
- direct hierarchy 路径
- 完全静态
- 没有 pending / in-flight build

它会把当前可见集的多个小 buffer 重新打包成更少的大 buffer，
目标是减少：

- draw call
- vertex buffer 绑定次数

## 5. shader 在这套架构里的角色

关键文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/pipeline.rs`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/scene.wgsl`

这层最重要的设计思想是：

> CPU 不把“最终屏幕语义”一次做死，而是把最后一层平移、缩放补偿、hatch 图案解释留给 shader。

### 顶点阶段

CPU 传给 shader 的顶点位置并不是最终 NDC，而是：

- 已经乘过当前 tile cache zoom 基准
- 但还没叠加 pan / canvas origin
- 也还没转换成 NDC

在 `vs_main` 里，shader 会再补：

1. `position_scale`
   - 用来支持 zoom bucket 复用
   - 以及交互冻结时的“旧缓存跟手”
2. `translation`
   - 用来支持 pan 和画布偏移
3. `viewport_size`
   - 把逻辑屏幕坐标转成 NDC

### 片元阶段

fill 顶点并不直接代表“这个像素一定要着色”，而是代表：

> 这个像素处在一个允许被 hatch 图案填充的闭合区域里

真正要不要着色，要由 `fs_main` 决定：

- `kind`
  - 0 = outline
  - 1 = hatch fill
- `hatch_style`
  - 左斜线 / 右斜线 / 交叉线 / 点阵
- `suppress_fill`
  - 交互期是否临时直接丢弃 fill

这套分工的好处是：

1. CPU 不需要为每种 hatch preset 各造一套不同几何
2. 交互降级时不需要切一整套新的 CPU tile cache
3. zoom bucket 复用时也不需要为每个细小 zoom 变化重建顶点

## 6. 调试面板里的数字该怎么看

左侧 `Renderer` 面板里的数字大致可以按三层去理解。

### 几何层

- `Total shapes`
  - 当前场景或当前 hierarchy range 的总 shape 量级
- `Candidates`
  - 进入可见查询二次判断前的候选 shape 数
- `Visible shapes`
  - 最终和当前可见范围相交的 shape 数
- `Vertices`
  - 当前帧真正准备提交给 GPU 的顶点数

如果你优化后：
- `Vertices` 明显下降
- 但 `Draw calls` 没怎么变

那通常说明你更像是在优化：
- hatch fill
- shape LOD
- clipping
- 几何重复

### tile / cache 层

- `Visible tiles`
  - 当前视口理论上命中的 tile 数
- `Tile hits / Tile misses`
  - 当前视图请求 tile 时，命中还是需要补建
- `Cache entries`
  - 当前 tile cache 里实际有多少 `tile + layer` 条目
- `Cache bytes`
  - 当前 tile cache 的粗略体积
- `Pending entries`
  - 当前还有多少 `tile + layer` 在排队补建

这组数字更适合判断：
- cache 是否稳定
- tile 粒度是否太细
- progressive build 是否还在追当前视图

### draw / display 层

- `Draw calls`
  - 当前帧实际提交的绘制段数
- `Cache hit / miss`
  - 这一帧 scene/tile 调度是否整体稳定
- `Progressive mode`
  - 当前是 bypass 还是按预算渐进补图

如果静态帧很低、而 `Pending entries` 已经是 0，
通常更值得看：

- `Vertices`
- `Draw calls`

而不是继续盯 worker 线程。

## 7. 为什么静态帧会卡

前面真实 sample 过之后，当前最重要的结论是：

- 大版图静态 `24 FPS` 的主瓶颈不是“没开多线程”
- 而是 GPU / present / drawable wait

也就是主线程很多时间卡在：

- `wgpu::Surface::get_current_texture`
- `CAMetalLayer nextDrawable`

这说明当你继续优化时，优先级应该更偏向：

1. 减少真实 draw call
2. 减少重复顶点
3. 减少每帧 GPU 状态切换

而不是先继续堆更多 worker 线程

## 8. 你后面最值得继续看的入口

如果你想继续自己优化，建议按这个顺序往下看：

### 先看整体分叉

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`
  - `rebuild_scene_from_source()`
  - `refresh_filtered_scene_and_renderer()`
  - `refresh_layout_scene_if_camera_requires_rebuild()`

### 再看 hierarchy 如何按需展开

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/layout/view_builder.rs`
  - `LayoutViewBuildOptions`
  - `build_layout_view_scene()`
  - `visit_layout_shape_bounds_in_view()`
  - `visit_layout_shapes_in_view()`

### 再看 geometry 怎么把 shape 变成顶点

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`
  - `TileGridIndex`
  - `query_visible_tiles()`
  - `emit_scaled_shape_vertices()`
  - `prepare_large_shape_tile_fragments()`

### 最后看 renderer 调度

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`
  - `update_scene_cache()`
  - `dispatch_pending_hierarchy_tile_builds()`
  - `collect_completed_hierarchy_tile_builds()`
  - `ensure_visible_display_batch()`

## 9. 现在最可能继续有效的优化方向

如果后面继续围着大版图性能做，我建议优先考虑：

1. 进一步减少 direct hierarchy 超远景下的重复几何
2. 补充更精确的运行时统计
   - 真实 draw 段数
   - static batch 是否启用
   - pending / in-flight tile build
   - tile cache bytes
3. 继续把“用户看不出来的细节”更早地在 hierarchy 或 geometry 层裁掉

相比之下，继续单纯增加线程数的收益通常会更有限。
