# 渲染主链讲解

这份文档专门回答一个问题：

**当前 viewer 里，一帧画面到底是怎么从状态变成屏幕图像的？**

如果你把这个问题真正看明白，后面再去理解缓存、裁剪、性能优化，会轻松很多。

## 一句话总览

当前项目里，一帧渲染可以概括成：

`app 收集输入 -> ui 生成动作 -> camera / renderer 状态更新 -> renderer 生成可见 tile -> shader 做最终平移和 NDC 变换 -> egui 叠加 UI`

## 参与的核心文件

1. `src/app/mod.rs`
2. `src/ui/mod.rs`
3. `src/camera/mod.rs`
4. `src/renderer/mod.rs`
5. `src/renderer/geometry.rs`
6. `src/renderer/pipeline.rs`
7. `src/renderer/scene.wgsl`

建议你边看本文档，边把这几个文件打开。

## 第 1 步：事件循环驱动重绘

起点在 `ViewerApp::redraw()`。

这个函数每次被 `WindowEvent::RedrawRequested` 触发时执行。
它做的第一件事不是画图，而是先记录时间：

- `last_frame_at`
- `frame_stats.record_frame(...)`

这么做的目的是让 UI 能实时显示：
- `FPS`
- `Frame ms`

也就是说，性能统计是嵌在每一帧主循环里的，而不是独立线程里额外算的。

## 第 2 步：egui 先跑一遍，生成动作

接下来 `app` 会调用：

- `egui_state.take_egui_input(...)`
- `egui_ctx.run(raw_input, |ctx| { ... })`

在这个闭包里，真正执行的是 `ui::draw_ui(...)`。

`draw_ui` 做两类事：

1. 画左侧面板
2. 收集当前帧用户动作

它不会直接修改 renderer，也不会直接修改 scene。
它只返回一个 `UiAction`，里面可能包含：

- 切换 cell
- 请求 fit to window
- 平移 delta
- 缩放 factor
- 隐藏图层集合
- tile grid 参数变化
- 闭合图形显示模式变化（Outline / Hatch / Hatch + Outline）
- tile cache 容量变化

这是这个项目里一个非常值得学习的设计点：

**UI 不直接改核心状态，而是把“意图”交给 app 统一处理。**

这样做的好处：
- 状态流清楚
- 副作用集中
- 不会出现 UI 到处偷偷改 renderer 的情况

## 第 3 步：app 把动作应用到状态

`redraw()` 在拿到 `UiAction` 之后，会按顺序处理：

1. 更新 `canvas_size`
2. 如果切换了 cell，调用 `select_scene_view()`
3. 如果图层显隐变化，更新 `hidden_layers`
4. 如果 tile grid slider 变了，调用 `renderer.set_tile_grid_divisions(...)`
5. 如果需要 fit，调用 `fit_scene()`
6. 如果有拖拽，调用 `camera.translate_screen(...)`
7. 如果有滚轮缩放，调用 `camera.zoom_by(...)`

这个顺序不是随便排的。

例如：
- 先切 scene，再 fit
- 先更新 canvas size，再 fit
- 先拿到 zoom，再进入 render

如果顺序乱了，就很容易出现：
- fit 对旧场景生效
- 缩放中心不对
- renderer 用旧参数画新画面

## 第 4 步：renderer 开始准备一帧 GPU 绘制

`app` 最后会调用：

`renderer.render(...)`

传进去的核心输入有：
- `camera`
- `hidden_layers`
- `canvas_origin`
- `canvas_size`
- `pixels_per_point`
- `egui full_output`

注意这里同时传了：
- 逻辑坐标信息
- DPI 缩放信息

这是因为这个项目已经踩过 Retina / 高 DPI 坐标不一致的问题。

## 第 5 步：先准备 egui 自己要画的内容

在 `renderer.render()` 里，前半段先做的是：

- 更新 egui texture
- 把 egui shapes tessellate 成 clipped primitives
- 调 `egui_renderer.update_buffers(...)`

这一步和主版图渲染是并行存在的两套绘制数据：

- 一套是 `wgpu` 主画布几何
- 一套是 `egui` 自己的 UI primitives

可以理解成：

**scene pass 负责“版图内容”，egui pass 负责“界面层”。**

## 第 6 步：根据当前视口更新 scene cache

接下来 renderer 会计算：

- `viewport_size = logical_viewport_size(...)`
- `update_scene_cache(...)`

这里是当前性能优化的核心入口。

### 6.1 先看 `cached_scene_key`

`update_scene_cache()` 第一件事，是构造 `RenderCacheKey`。

这个 key 包括：
- `scene_revision`
- `pan`
- `zoom`
- `canvas_origin`
- `canvas_size`
- `viewport_size`
- `hidden_layers_hash`

如果这个 key 和上一帧一样，就直接：
- 标记 `cache_hit = true`
- 返回

意思是：

**这一帧连“可见范围”和“绘制条件”都没变，连查询都不必重做。**

### 6.2 再看 `tile_cache_domain`

如果帧级 key 变了，还会继续构造一个更粗一点的 domain：

- `scene_revision`
- `zoom`
- `hidden_layers_hash`

为什么这里故意不包含 pan？

因为我们希望：
- 平移时能继续复用已有 tile vertex buffer
- 只有 scene / zoom / hidden layers 变化时，才整批失效 tile cache

这是这个项目里最关键的一层缓存设计。

## 第 7 步：先算可见世界范围

renderer 接着会调用：

`camera_visible_world_bounds(camera, viewport_size)`

这个函数做的是反推：

- 已知当前屏幕大小
- 已知当前 pan 和 zoom
- 求当前能看到的世界坐标矩形范围

后面所有的可见查询，都是围绕这个范围展开的。

## 第 8 步：两套查询同时工作

### 8.1 `ShapeSpatialIndex`

先通过：

`query_visible_shapes(...)`

拿到：
- `candidate_shapes`
- `visible_shapes`
- `bucket_hits`

这组数据主要服务于：
- 调试统计
- 验证当前索引策略有没有生效

### 8.2 `TileGridIndex`

再通过：

`query_visible_tiles(...)`

拿到当前视口命中的 tile 列表。

后续真正参与渲染的是 tile，不是完整场景。

这是因为当前主渲染优化方向是：

**按 tile 复用 GPU vertex buffer，而不是每次重新打整屏一个大 buffer。**

## 第 9 步：为每个可见 tile 找缓存或重建缓存

循环可见 tiles 时，renderer 会做：

1. 构造 `TileCacheKey`
2. 查 `tile_vertex_cache`
3. 如果命中：
- 直接累计 `tile_cache_hits`
- 记录这个 tile 需要参与本帧绘制
4. 如果没命中：
- 先取这个 tile 内的 shape indices
- 过滤掉已经被"预碎片化"接管的超大 shape
- 对剩余普通 shape 继续走运行时 tile 裁剪
- 再把这个 tile 预先准备好的 world-space fragments 也一起转成顶点
- 最后统一创建新的 GPU vertex buffer
- 放进 `tile_vertex_cache`
- 累计 `tile_cache_misses`

这里其实已经有两条几何生成路径同时存在：

1. 小 shape 的动态路径
- 每次 tile miss 时，现场按 tile bounds 裁剪

2. 超大 shape 的预碎片路径
- 在 `scene` 或 `tile grid` 变化时，先切成 `PreparedTileFragments`
- tile miss 时只需要把这些世界坐标碎片按当前 zoom 转成顶点

这样分开的好处是：
- 小 shape 不需要过早复杂化
- 大 shape 不需要为每个 tile 反复做完整裁剪

这里有一个非常重要的细节：

**tile 顶点当前只包含“缩放后的逻辑屏幕坐标”，不包含最终平移。**

这样一来：
- 平移只改 uniform
- tile buffer 不必重建

这就是为什么当前 viewer 在平移时缓存命中率会更高。

## 第 10 步：为什么顶点只做缩放，不做平移

这是很多人第一次看会疑惑的点。

如果 CPU 在生成 tile 顶点时，把下面这些都一起算进去：
- world -> zoom
- pan
- canvas origin
- viewport -> NDC

那平移一下就会导致：
- 所有 tile 顶点都失效
- 所有 buffer 都要重建

现在拆成两段：

### CPU 阶段
- 只做 `world * zoom`
- 得到缩放后的逻辑屏幕坐标

### Shader 阶段
- 再加 `translation = pan + canvas_origin`
- 再根据 `viewport_size` 转成 NDC

这样做的收益是：
- tile buffer 的复用粒度变好了
- 平移成本下降了
- shader 逻辑仍然很简单

## 第 10.5 步：交互冻结到底冻结了什么

最近这条链容易让人第一次看时误会成：

- “冻结交互视图 = 停止渲染”

其实不是。

它真正做的是：
- 拖拽/缩放中，不为每个中间状态都重建 scene cache
- 先复用上一帧已经稳定的视图
- 用 shader 里的 `position_scale` 和 `translation` 让画面继续跟手
- 交互停下来后，再补当前真实视图

所以它冻结的是：
- `update_scene_cache(...)` 这条重路径

它没有冻结的是：
- 当前帧 scene pass
- 当前帧 egui pass
- 画面本身对用户输入的视觉响应

这是最近一条非常重要的体验优化，因为它直接绕开了：
- 连续缩放时的中间状态白算
- 交互期大量 cache / tile 反复重建

## 第 11 步：scene pass 真正开始画

renderer 会创建一个 `SceneUniform`：

- `translation`
- `viewport_size`

然后建立 bind group，开始 `scene-render-pass`。

这个 render pass 里会做三件重要的事：

### 11.1 清屏

先用深色背景清掉整个 surface。

### 11.2 设置 scissor

这里会按 `canvas_origin` 和 `canvas_size` 设置 scissor rect。

目的很明确：

**版图只能画在中央画布区域，不能盖住左侧面板。**

### 11.3 逐 tile 绘制

遍历 `visible_tile_keys`：
- 取缓存中的 vertex buffer
- `set_vertex_buffer(...)`
- `draw(...)`

所以当前真实绘制粒度已经不是“整场景一次 draw”，而是“每个可见 tile 一次 draw”。

## 第 11.5 步：为什么“按 layer 渐进”后来还要再区分构建和显示

这是这套 renderer 里最容易混淆的一点之一。

### 先有的只是“构建渐进”

最开始我们做的是：
- `pending queue` 决定这一帧去 build 哪些 `tile + layer`

这会让缓存构建变得渐进，但并不自动等于：
- 用户看到的显示也会一层一层地出来

因为如果后续 layer 的缓存已经存在，而显示层不做额外门控，
它们仍然可能被一起画出来。

### 后来补的是“显示渐进”

所以现在 renderer 里其实有两套相关但不同的概念：

1. `active_progressive_layer`
- 控制“这一帧主要构建哪一层”

2. `active_display_layer`
- 控制“这一帧当前层已经允许放出来多少显示条目”

也就是说，当前实现已经不只是：
- build by layer

而是：
- build by layer
- reveal by layer

这样观感才更接近版图工具，而不是地图瓦片。

## 第 12 步：shader 做最后的平移和 NDC 变换

在 `scene.wgsl` 的 `vs_main` 里，顶点会走这两步：

1. `screen = input.position + scene_uniform.translation`
2. 把 `screen` 根据 `viewport_size` 转成 NDC

这意味着当前主渲染链的职责分工是：

### CPU / geometry
负责：
- 过滤哪些 shape / tile 可见
- 生成粗线三角形顶点
- 做 world * zoom

### GPU / shader
负责：
- 加平移
- 转 NDC
- 输出颜色

这是一个很适合当前 demo 的中间态。

它不是最极致的 GPU 方案，但已经比最早“每帧全量 egui painter 画 outline”的版本更接近真实 viewer 了。

## 第 13 步：egui pass 覆盖到最上层

scene pass 画完后，renderer 会再开一个 `egui-render-pass`。

这个 pass 的 `load` 不是清屏，而是 `Load`：

也就是保留前面 scene pass 已经画好的内容，
然后把左侧 UI、文本、边框等叠加上去。

最终结果就是你看到的：
- 背后是版图
- 前面是可交互的 UI

## 一帧结束后，调试信息怎么回到 UI

在 `update_scene_cache()` 末尾，renderer 会更新：

- `total_shapes`
- `candidate_shapes`
- `visible_shapes`
- `bucket_hits`
- `vertex_count`
- `visible_tiles`
- `tile_cache_hits`
- `tile_cache_misses`
- `cache_hit`

然后 `app` 在 render 成功后拿到：

`self.render_debug_stats = renderer.debug_stats()`

再在下一帧 `draw_ui()` 时显示出来。

所以左侧 UI 看到的这些数字，本质上是上一帧 scene cache 更新后的结果。

## 你最该记住的 4 个关键点

### 1. UI 不直接操作 renderer

它只产出动作，`app` 统一调度。

### 2. renderer 有两层缓存

- 一层是“这一帧需不需要重算可见结果”
- 一层是“tile vertex buffer 能不能继续复用”

### 3. 可见查询和 tile 缓存是两回事

- `ShapeSpatialIndex` 主要减少扫描量
- `TileGridIndex` 主要服务缓存复用

### 4. 平移被刻意留给 shader 去做

这是当前 cache 命中率能提高的关键原因。

## 建议你对照源码时重点看这些函数

在 `src/app/mod.rs`：
- `ViewerApp::redraw`
- `ViewerApp::fit_scene`

在 `src/ui/mod.rs`：
- `draw_ui`

在 `src/renderer/mod.rs`：
- `Renderer::render`
- `Renderer::update_scene_cache`
- `Renderer::set_tile_grid_divisions`

在 `src/renderer/geometry.rs`：
- `camera_visible_world_bounds`
- `query_visible_shapes`
- `query_visible_tiles`
- `build_scaled_line_vertices_for_indices`
- `emit_segment`

在 `src/renderer/scene.wgsl`：
- `vs_main`

## 推荐的学习方式

你可以用这个顺序自己手画一遍：

1. 画一个矩形 shape
2. 假设一个 zoom
3. 先算 `world * zoom`
4. 再加 `pan + canvas_origin`
5. 再转成 NDC
6. 再想象这个 shape 落在哪个 tile
7. 再想象平移时，为什么 tile buffer 可以继续复用

如果这套你能手推出来，当前 viewer 的渲染主链你就真的掌握了。

## 第 9.5 步：为什么现在闭合图形可以填充了

当前 renderer 不再只会画轮廓。
它现在把“闭合图形怎么显示”抽成了一个独立模式：

- `Outline`
- `Hatch`
- `Hatch + Outline`

这个模式只影响闭合图形：
- `Rectangle / Boundary / Box / Polygon`

不会影响开放折线：
- `Path / Polyline`

原因很简单：

开放折线本质上描述的是“线”，不是“面”。
如果把它们也强行走填充分支，就会把路径语义画错。

当前实现里，闭合图形填充采用的是非常适合学习的方案：

- CPU 侧把点列拆成三角扇
- 和原来的线段三角形一起走同一条 GPU pipeline

这样做的好处是：

1. 不需要额外再起一条复杂的 fill pipeline
2. `Hatch + Outline` 可以很自然地叠加
3. 调试时你可以直接从顶点数量看出 hatch 面是否真的参与了绘制

所以现在你看到某些 layer 不再只是一个空框，
并不是 renderer “偷偷换了画法”，而是因为闭合图形终于拥有了明确的填充语义。


## Hatch 预设填充

当前闭合图形已经不再使用纯色整块覆盖，而是走一条更接近版图工具的路径：

- CPU 仍然先为闭合图形生成填充三角形
- 这些顶点会带上 `kind = hatch fill` 语义
- WGSL 在 `fragment shader` 里按屏幕坐标生成 45 度单斜线
- `Path / Polyline` 仍然按线段绘制，不参与面填充

这样做的好处是：
- 不会像纯色填充那样把内部结构整块盖住
- hatch 疏密是屏幕空间稳定的，缩放时阅读体验更一致
- 后续扩成交叉斜线、点阵时，不需要推翻 CPU 侧 tile cache 和几何生成


## 按 layer 单独设置显示模式

当前 renderer 已经从"只有一个全局闭合图形模式"推进到"全局模式 + 每层可覆盖"。

这意味着：
- 你仍然可以用全局 `Outline / Hatch / Hatch + Outline` 快速切换整体风格
- 也可以对少数特殊 layer 单独指定模式
- 没有显式覆盖的 layer，会自动回退到当前全局模式

这套设计的价值在于：
- UI 使用成本低：默认不用每层都配
- 缓存边界清楚：layer 覆盖也会进入 cache key
- 很适合版图查看：把大包层收成 outline，把核心工艺层保留 hatch


## Tile 局部几何生成

现在 renderer 在为某个 tile 生成顶点时，不再简单地把整 shape 复制进去。
而是会：
1. 先拿到该 tile 的世界坐标 bounds
2. 对闭合多边形做矩形裁剪
3. 对折线做线段裁剪
4. 再把裁剪后的局部几何转成顶点

这一步的意义不是显示正确性，而是减少大 shape 在多个 tile 中的重复顶点。

## 渲染统计为什么还要单独留历史

当前 `Renderer` 每帧都会给出一份瞬时的 `RenderDebugStats`，
但瞬时值有一个天然问题：

- 你刚刚平移了一下，`tile misses` 会突然抬高
- 你刚切到一个复杂 cell，`vertex_count` 会瞬时增大

这些都是真的，但如果只看单帧数字，很难判断它是“短暂波动”还是“长期趋势”。

所以 `app` 现在会在每次 render 成功后，把几项关键字段压进一个固定窗口历史：

- `vertex_count`
- `tile_cache_misses`
- `cache_bytes`

UI 再把这三个窗口画成轻量 sparkline。

这样做的目的不是把 viewer 变成 profiler，
而是让我们在继续做性能优化时，能快速回答一个很实际的问题：

**这次改动，到底是在改善长期趋势，还是只是在某一帧看起来比较好？**


## 渐进式补全

当前 renderer 不再要求“同一帧把所有可见 tile 都补齐”。它现在会：

1. 为当前视图生成一个新的 `view_revision`
2. 先画已经存在的缓存
3. 把缺失的 `tile + layer` 放进 `pending queue`
4. 每帧只按 `build budget` 消化少量条目
5. 如果用户已经移动到新视图，旧 revision 的工作直接丢弃

这就是 viewer 开始呈现出类似 KLayout 那种“逐步补全”体验的基础。

这里建议你把它拆成三层来理解：

1. `pending queue`
- 控制“缺失缓存条目怎么分帧构建”

2. `active_progressive_layer`
- 控制“当前优先补哪一层”

3. `active_display_layer + active_display_budget`
- 控制“这一层已经构建好的部分，如何分批显示出来”

如果只理解了第一层，很容易以为“已经按 layer 分帧了，为什么还会卡”。
后面之所以又补显示门控和显示预算，就是因为构建渐进不等于显示渐进。


### Layer-first progressive fill

现在 renderer 在生成 `requested_tile_keys` 时，不再直接按可见 tile 逐个推入请求，而是先把这些请求按 `LayerId` 分组，再按 scene 的 layer 顺序展开。

这意味着：

- 内部仍然使用 tile cache
- 但用户看到的补全过程会更偏向“先补完整层，再补下一层”

这一步主要解决的是观感问题，而不是底层缓存结构问题。


### Active progressive layer

renderer 现在维护一个 `active_progressive_layer`：

- 如果当前活动层还有待补条目，就继续只补这一层
- 如果这一层补完了，再切到下一层

这一步没有改变 tile cache 结构，但明显改变了用户看到的补全过程。


### View-center-first ordering

当前活动层内部的 pending 条目不再按原始 tile 顺序消费，而是先按 tile 中心到当前可见世界中心的距离排序。这样可以把同一层里“最值得先补的区域”提前。


### Open layout and adaptive bypass

viewer 现在提供了 `Open layout...` 按钮来切换 GDS/OASIS 文件，不再只能修改配置常量。与此同时，renderer 会根据当前缺失条目数量自动决定：是直接一帧补完，还是继续走渐进式补全。

## 每层 Hatch Preset

现在 viewer 不只支持一个全局 hatch 风格。
每个 layer 都可以单独记住自己的 hatch preset：

- `LeftDiagonal`
- `RightDiagonal`
- `Cross`
- `Dots`

实现上，CPU 仍然只负责给闭合图形生成 fill 三角形，
不会因为图案不同去生成不同的 CPU 线段几何。
真正的差异只体现在两处：

- 顶点里的 `hatch_style` 编码
- shader 片元阶段对屏幕坐标的解释方式

这样做的好处是：

- 新增 hatch 样式时，不用推翻几何路径
- `tile + layer` 缓存仍然可以复用现有结构
- 每层 preset 可以自然进入持久化配置


### Layer-gated display

这里要特别注意一个容易混淆的点：

- `pending queue` 控制的是"这一帧去构建哪些 tile + layer 条目"
- `visible_tile_keys` 控制的是"这一帧最终允许哪些条目被真正画出来"

现在 renderer 已经不再把所有命中的缓存条目都直接塞进 `visible_tile_keys`。
它会先按 layer 顺序算出一个"可显示前缀"：

- 完整旧 layer：显示
- 当前活动 layer：显示已完成部分
- 后续 layer：暂时隐藏

所以这一步是把"构建顺序"和"显示顺序"真正对齐。


### Layer-adaptive active-layer bypass

在现有 `active_progressive_layer` 机制上，renderer 现在还会额外估算“这一层到底轻不轻”。

估算时会看三类量：

- 当前活动 layer 还剩多少 `pending entries`
- 这一层命中的 `prepared fragments` 数量
- 这一层普通 `regular shapes` 的数量

然后把它们折算成一个 `estimated_work_units`。
如果当前活动 layer 同时满足：

- 条目数不大
- 估计工作量不大

那么这一层就会直接一帧补完，而不是继续沿用普通的 `build budget`。

这一步的目标很明确：

- 轻 layer 直接出来
- 重 layer 继续渐进

所以现在 viewer 的调度已经是三层判断：

1. 这个视图整体是不是轻到可以全局 bypass
2. 如果不是，当前活动 layer 是不是轻到可以按 layer bypass
3. 如果还不是，才走普通渐进式构建
