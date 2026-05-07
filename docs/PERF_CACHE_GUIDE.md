# 缓存与性能优化讲解

这份文档专门解释当前 viewer 里最容易“看代码能跑，但不太明白为什么这样设计”的部分：

- 为什么要做缓存
- 缓存分成几层
- 每一层缓存各自挡住了什么成本
- `ShapeSpatialIndex`、`TileGridIndex`、`tile cache` 之间是什么关系

如果你后面准备继续做性能优化，这份文档建议在看 `renderer/mod.rs` 和 `renderer/geometry.rs` 之前先读一遍。

## 一句话总览

当前项目的性能优化主线可以概括成：

`减少无意义扫描 -> 减少无意义顶点生成 -> 减少无意义 GPU buffer 重建 -> 尽量复用 tile 结果`

它不是“终极高性能方案”，但已经把一个学习型 viewer 最该建立的几层意识搭起来了。

## 为什么一开始就要关心性能

版图查看器和普通 UI 最大的不同在于：

- 视口里可能同时有很多图形
- 用户会频繁平移和缩放
- 同一份数据会被反复重画

如果没有缓存，最朴素的做法往往是每一帧都做：

1. 扫描全部 shape
2. 重新判断哪些可见
3. 重新生成全部顶点
4. 重新创建 GPU buffer

这种方式在很小的数据上也许还能跑，但它几乎没有扩展余地。

所以这个项目虽然还是 demo，但从比较早就开始按“查看器”的思路搭缓存，而不是只做一个一次性的样例。

## 当前项目里一共有几层性能优化

可以把现在的优化分成 6 层：

1. `ShapeSpatialIndex`：减少每帧扫描的 shape 数
2. 离屏裁剪：减少生成顶点的 shape 数
3. `cached_scene_key`：连查询都不必重做的帧级缓存
4. `tile_vertex_cache`：复用 GPU vertex buffer
5. `hierarchy level range`：从 scene 语义上直接减少要画的层级范围
6. `screen-space LOD`：图形已经很小时，不再保留全部原始细节

下面我们按顺序讲。

## 第 1 层：`ShapeSpatialIndex` 在解决什么问题

位置：
- `src/renderer/geometry.rs`

`ShapeSpatialIndex` 是一个均匀网格索引。

它的目标不是缓存 GPU 数据，而是先回答一个更基础的问题：

**当前视口附近，哪些 shape 才值得看一眼？**

### 如果没有它

每一帧都要遍历：
- `scene.shapes()` 里的全部图元

即使你当前只放大到一个局部区域，也要从头扫到尾。

### 有了它之后

流程变成：

1. 根据当前 camera 算出 `visible_world_bounds`
2. 用 bounds 找出命中的网格 bucket
3. 只看这些 bucket 里的候选 shape
4. 再做精确 bounds 相交判断

所以它带来的收益是：

- 减少候选数量
- 降低每帧 CPU 扫描量
- 让放大到局部时的成本更接近“局部复杂度”而不是“全局复杂度”

### 为什么这里不用更复杂的数据结构

因为这个项目当前阶段的目标是：

- 先把索引思想建立起来
- 保证代码容易读
- 保证调试直观

均匀网格并不是最万能的，但它足够简单，特别适合你现在这种学习阶段。

## 第 2 层：离屏裁剪在解决什么问题

即使候选 shape 数减少了，仍然不代表这些候选都一定要生成顶点。

所以第二层优化是：

- 再次判断 shape 的 `bounds` 是否和 `visible_world_bounds` 真正相交
- 如果完全离屏，就不生成任何顶点

这一步的作用是：

**把“候选查询”和“几何生成”彻底分开。**

这是一个很重要的性能思维：

- “可能相关”不等于“真的要画”

当前 UI 里的这些数字就是为了帮助你看清这个差异：

- `Candidates`
- `Visible shapes`

如果这两个差很多，说明空间索引确实在帮你缩小范围。

## 第 3 层：`cached_scene_key` 在解决什么问题

位置：
- `src/renderer/mod.rs`

这是“整帧级别”的缓存。

它的核心问题是：

**如果这一帧和上一帧看到的条件完全一样，为什么还要重新做可见查询和统计？**

所以这里构造了一个 `RenderCacheKey`，包含：

- `scene_revision`
- `pan`
- `zoom`
- `canvas_origin`
- `canvas_size`
- `viewport_size`
- `hidden_layers_hash`
- `draw_mode`（闭合图形的显示模式）

只要这些值都没变：

- 说明视图没变
- scene 没变
- layer 状态没变

这时就可以直接：
- 复用上一帧算过的可见结果
- 让 `debug_stats.cache_hit = true`

### 它挡住的成本是什么

不是 GPU 绘制本身。

它挡住的是：
- 每帧重新做查询
- 每帧重新做 visible tiles 分析
- 每帧重新统计调试信息

也就是说，它是“上游计算缓存”。

## 第 4 层：`tile_vertex_cache` 在解决什么问题

这是当前项目里最有代表性的一层。

位置：
- `src/renderer/mod.rs`

核心问题是：

**即使可见区域变了，能不能只重建变化的那一部分几何，而不是整屏全部重建？**

所以当前策略是：

1. 先把 scene 切成 tile
2. 每个 tile 单独生成 vertex buffer
3. 当前帧只绘制可见 tiles
4. 对没有变化的 tile 直接复用旧 buffer

这一步比“整帧一个大 buffer”更像真正的版图查看器思路。

## 为什么是 tile，而不是整屏一个 buffer

如果整屏只有一个总 buffer：

- 平移一点点，哪怕只多出一小块区域
- 通常也得把整屏的顶点重新组织一遍

而 tile 的好处是：

- 视口平移时，只会进入/离开少量 tile
- 原来还在视口里的 tile 可以继续复用
- 局部变化不会导致整场景全部重建

所以 tile 是一个非常典型的“空间局部性”优化手段。

## 第 5 层：`hierarchy level range` 在解决什么问题

这层优化和前面几层有一个很大的不同：

- 前面几层更像“渲染器内部怎么少做无意义工作”
- `hierarchy level range` 更像“从业务语义上直接减小 scene”

当前新的 `GDS` 路径实现是：
- `LayoutBundle`：当前 root cell 的分层 source
- `view_builder`：按 `min/max level` 只展开需要的层级
- `scene`：真正送给 renderer 的临时 workset

### 为什么它对性能有帮助

如果你只是想调试：
- root 自己画了什么
- 第 1 层子实例带来了什么
- 到第 3 层开始为什么画面突然复杂

那没必要一上来就把完整层级全部展开成常驻扁平场景。

这时直接把 `max level` 收小，收益是立刻发生的：
- shape 数减少
- 点数减少
- 后面的索引、LOD、tile cache 都一起少做很多事

### 为什么不把这个过滤放在 renderer 里

因为 hierarchy range 更像 scene 语义，不是底层渲染语义。

如果硬塞进 renderer：
- 可见查询要额外感知层级范围
- cache key / 统计链会多一套专用逻辑
- UI 还得想办法额外拿完整 scene 的最大深度

现在放在 app 层的好处是：
- renderer 仍然只消费一个普通 `Scene`
- 现有索引、tile cache、统计都直接复用
- 复杂度下降更早发生
- 更深层的 hierarchy 如果当前没请求到，就不会整份常驻内存

### 这一层对大文件内存的直接影响

这轮重构后，项目已经能把：
- “分层 source”
- “当前 workset”

这两个概念拆开。

对 `example_mzi_perf.gds / MziArray`，当前 `memory_probe` 的一个真实例子是：
- root 最大层级：`5`
- 当只展开 `0..2` 时：
  - `shape_count = 230016`
  - `total_points = 1150080`

而旧的全量扁平探针曾经是：
- `shape_count = 2274428`
- `total_points = 55362608`

这组数字最能说明：
- 这层优化不是“让 renderer 稍微少画一点”
- 而是直接改变了常驻数据本体的规模

### 初始化为什么不是总显示全部层级

因为大版图完整展开后，很多时候一开始就太重。

所以现在默认策略是：
- 小版图：直接显示全部层级
- 大版图：先显示前一半层级

当前“小版图”的判断是一个保守启发式：
- `shape_count`
- `total_point_count`

它不是严格最优，但很适合 viewer 的启动体验。

## 第 6 层：`screen-space LOD` 在解决什么问题

这一层优化专门回答一个现实问题：

**图形已经缩得很小了，为什么还要继续保留几百个点的原始轮廓？**

### 为什么这个问题很关键

像 `L600/D10` 这类层，慢的主因不是：
- 变换错了
- tile 跨得特别夸张

而是：
- 高点数闭合图形很多
- 总点数非常高
- 图形在屏幕上已经很小，但仍然保留了全部细节

这时继续为全部点列逐点买单，性价比很低。

### 当前做了哪两类 LOD

1. `closed shape`
- 屏幕上已经很小时，进入 coarse LOD
- hatch 继续保留
- coarse 目标点数会随屏幕尺寸动态变化

2. `polyline / path`
- 屏幕上已经很小时，也会做折线点列抽稀
- 但仍然保持开放折线语义

### 为什么按屏幕尺寸，而不是世界坐标大小

因为真正影响用户感知的是：
- 它在屏幕上占多少像素

不是：
- 它在版图坐标里本来多大

一个世界坐标很大的图形，如果当前缩得很小，它在屏幕上依然可能只占十几像素。
这时保留全部点列通常是不划算的。

## `TileCacheDomainKey` 为什么存在

这个地方很关键。

很多人第一次看会问：

“既然已经有 `TileCacheKey` 了，为什么还要有 `TileCacheDomainKey`？”

它们的职责不同：

### `TileCacheKey`
描述“某一个 tile 的某一个缓存条目”

### `TileCacheDomainKey`
描述“整批 tile buffer 是否还在同一个世界里”

它当前包含：
- `scene_revision`
- `zoom_bits`
- `hidden_layers_hash`
- `draw_mode`（闭合图形的显示模式）

意思是：

只要这些条件之一变了，旧 tile buffer 就整体不再可信，必须清空整批。

### 为什么这里故意不包含 `pan`

因为当前优化的关键目标之一就是：

**平移时尽量继续复用 tile buffer。**

如果把 `pan` 也放进 domain，用户每拖一下，所有 tile 都会整体失效，那这个缓存价值就大大下降了。

## 为什么缩放会导致 tile domain 失效

这是因为当前 tile 顶点保存的是：

- 已经乘过 `zoom` 的逻辑屏幕坐标

也就是说，一旦 `zoom` 变化：
- 顶点本身就变了
- 旧 tile buffer 不能直接再用

所以当前项目的策略是：

- 平移复用 tile
- 缩放失效 tile

这是一个非常典型、也非常合理的折中。

## 为什么隐藏图层会导致 tile domain 失效

因为 layer 显隐会直接改变：

- 一个 tile 里到底有哪些图元要参与绘制
- 顶点总数是多少
- buffer 内容是什么

所以它不能只影响调试 UI，必须进入缓存域。

如果不这么做，就会出现：
- UI 里图层开关变了
- 但屏幕上继续复用旧图层顶点

这是非常典型的错误复用。

## `ShapeSpatialIndex` 和 `TileGridIndex` 的关系

它们看起来都像“网格”，但服务目标不同：

### `ShapeSpatialIndex`
服务“可见查询”

目标：
- 降低扫描量
- 快速拿候选 shape

### `TileGridIndex`
服务“几何分块与缓存复用”

目标：
- 把场景拆成 tile
- 支持 per-tile buffer 复用

可以理解成：

- 前者更像搜索索引
- 后者更像缓存分片目录

## 当前 UI 里的性能数字怎么理解

### `Total shapes`
当前 scene 总 shape 数

### `Candidates`
空间索引查询返回的候选数

### `Visible shapes`
候选里真正与可见区域相交的 shape 数

### `Bucket hits`
本次 shape 可见查询命中了多少 bucket

### `Draw calls`

当 viewer 已经把构建阶段拆得比较细以后，剩下的卡顿很可能来自"这一帧到底要发出多少次 draw"。
如果 `Vertices` 并没有特别夸张，但仍然觉得某一层出现时会卡一下，`Draw calls` 就是很值得看的指标。

当前 renderer 现在会在显示阶段把同一 tile 下已经可见的多个 layer 条目拼成一个 tile display batch，
所以理想情况下：

- 过去：一个 tile 里每个 layer 都要一次 draw
- 现在：一个 tile 一次 draw

这条优化不会减少几何量本身，但能明显降低单帧 draw call 峰值。


### `Vertices`
最终参与绘制的顶点数

### `Visible tiles`
当前视口命中的 tile 数

### `Tile hits`
这些 tile 中，直接复用了旧 buffer 的数量

### `Tile misses`
这些 tile 中，本帧新建了 buffer 的数量

### `Cache: hit/miss`
这是帧级缓存命中，不是 tile 级命中。

所以：
- `Cache: hit` 说明连查询层都没重做
- `Tile hits` 说明即使查询重做了，tile buffer 里仍有复用

这两个层级不要混淆。

## 为什么平移时缓存收益会更明显

当前顶点生成策略是：

- CPU 负责 `world * zoom`
- shader 再加 `translation = pan + canvas_origin`

所以平移不会改变 tile 顶点本身。

这意味着当你拖动画布时：
- 可能会有少量 tile 进入或离开视口
- 但大多数原本就在视口里的 tile buffer 可以复用

这正是你在 UI 里看到 `Tile hits` 上升的原因。

## 为什么当前还不算“终极高性能方案”

虽然已经有了几层优化，但它仍然是一个学习型、过渡型方案。

当前还没有做的包括：

- tile cache 的容量上限与淘汰策略
- world-space 顶点长期驻留 GPU
- 更复杂的 layer / tile 增量更新
- 更强的空间索引（例如 R-tree）
- 真正的大规模并行准备与上传

所以你可以把现在的版本理解成：

**已经走在正确方向上，但还没有走到头。**

## 你最该记住的 6 个点

1. `ShapeSpatialIndex` 是为了少扫 shape
2. 离屏裁剪是为了少生顶点
3. `cached_scene_key` 是帧级缓存
4. `tile_cache_domain` 是整批 tile 的有效域
5. `tile_vertex_cache` 是真正的 GPU buffer 复用层
6. 当前最关键的复用收益来自“平移不重建 tile 顶点”

## 预碎片化统计怎么看

现在左侧调试区还会直接显示 3 个和"超大 shape 预碎片化"相关的数字：

- `Prepared shapes`：有多少原始 shape 被预碎片化路径接管
- `Prepared tiles`：这些 shape 一共覆盖了多少个 tile 条目
- `Prepared fragments`：最后实际准备出了多少个局部世界坐标碎片

这组统计的价值在于：

- 它能告诉你当前这条优化是不是根本没有命中场景
- 也能告诉你某个 cell 里是不是有少量超大 shape 正在主导成本

如果你看到：
- `Prepared shapes` 很小
- 但 `Prepared fragments` 很大

通常就说明：

**场景里虽然只有少数几个超大 shape，但它们横跨了很多 tile，正是这条优化最该接管的对象。**

## 推荐你自己做的观察实验

你可以运行 viewer，然后专门观察这些变化：

### 实验 1：静止不动
预期：
- `Cache` 更容易是 `hit`
- `Tile hits` 稳定

### 实验 2：只平移
预期：
- `Cache` 多半会重新算，因为 pan 变了
- 但 `Tile hits` 应该仍然不少

### 实验 3：只缩放
预期：
- `Tile misses` 会明显增加
- 因为 zoom 变化导致 tile domain 失效

### 实验 4：切换 layer 显隐
预期：
- `Tile misses` 增加
- 因为 hidden layers 进入了 domain key

### 实验 5：调整 `Tile grid` slider
预期：
- `Visible tiles` 会变化
- 命中率模式也会变化
- 更细的 tile 更利于局部复用，但管理成本更高

这几个实验做完，你对当前性能结构的理解会非常扎实。

## 第 5 层：为什么现在还要给 tile cache 加容量上限

当前 viewer 已经能在平移时复用 tile buffer，
这很好，但也带来了一个新的真实问题：

**如果用户持续浏览一个很大的版图区域，cache 可能会一直长。**

所以这轮又补了一层控制：

- `tile_cache_capacity`：最多允许保留多少个 tile buffer
- 近似 `LRU` 淘汰：优先淘汰最久没用、且当前不在可见区域里的 tile

### 为什么不是“缓存越多越好”

因为 cache 不是免费的。
它会占：
- GPU vertex buffer 数量
- CPU 侧 cache 元数据
- 长时间浏览后的驻留内存

如果没有上限，viewer 可能在短时间里看起来很顺，
但随着浏览范围越来越大，内存和缓存管理成本会持续膨胀。

### 当前的淘汰策略为什么叫“近似 LRU”

因为我们并没有维护一个非常重的双向链表结构去做教科书式 LRU，
而是用了一个更适合当前 demo 的折中：

- 每个 tile 记录 `last_used_tick`
- 需要裁剪 cache 时，按最久未使用排序
- 当前视口里仍可见的 tile 优先保护不淘汰

这套策略的优点是：

1. 容易读
2. 容易测
3. 对当前 viewer 已经足够有效

### 左侧 UI 现在能看什么

你现在可以直接在左侧看到：

- `Tile hits / misses`
- `Layer hits / misses`
- `Cache entries / capacity`
- `Cache bytes`
- `Cache evictions`
- `Vertices / Tile misses / Cache bytes` 的小型趋势图

这会让“缓存到底有没有工作”变成一件能直接观察的事情，而不是只能靠感觉判断。

趋势图这一步很有价值，因为单帧数字容易受当前视口影响而抖动；
而把最近一小段时间的样本并排看出来，你会更容易判断：

- 这轮优化是不是让顶点量整体下降了
- tile miss 是不是在平移后又很快回落
- cache bytes 是在稳定，还是在持续膨胀


## Hatch 参数为什么也要进缓存 Key

虽然 hatch 图案主要在 shader 里生成，但当前 renderer 仍然把 `draw_mode / hatch spacing / hatch width` 放进缓存域。原因是：

- UI 切换模式时，我们希望调试统计能准确反映这是一次新配置
- 后续如果按模式拆不同 draw pass，缓存边界已经提前对齐
- 学习上也更容易看清：哪些参数只是 uniform，哪些参数会改变可视语义

当前实现里，`hatch spacing / hatch width` 会进入：
- frame 级 render cache key
- tile cache domain key
- 调试面板 stats

这样当你调节 hatch 参数时，左侧缓存统计会和实际可视状态保持一致。


## Tile 内局部裁剪

当前最关键的一步结构性性能优化，是把"命中 tile 就把整 shape 顶点塞进去"，推进成"先按 tile bounds 做局部裁剪，再生成这个 tile 真正需要的几何"。

这会直接减少：
- 大 boundary 在每个 tile 里的重复顶点
- tile cache 条目体积
- `Vertices` 调试统计里的无意义膨胀

当前实现里：
- 闭合图形走矩形裁剪后的多边形再三角化
- 折线走线段对矩形的裁剪

## 超大 Shape 预碎片化

在 tile 内局部裁剪之后，当前性能链又往前推进了一步：

- 运行时 tile 裁剪：当某个 tile cache miss 时，现场把 shape 裁到 tile 内
- 预碎片化：当 `scene` 或 `tile grid` 变化时，先把横跨很多 tile 的超大 shape 切成按 tile 组织的世界坐标碎片

这两步的关系不是替代，而是分工：

- 小 shape：继续走运行时 tile 裁剪，简单直接
- 超大 shape：提前切块，避免每次 miss 都重新做完整裁剪

### 再往前一步：为什么要细化成 `tile + layer` 缓存

在把超大 shape 预碎片化之后，下一层自然的问题是：

**如果我只切换了一个 layer，为什么还要让同一个 tile 里别的 layer 也跟着一起重建？**

所以当前缓存又从"整 tile 一个 buffer"推进成了：

- `tile + layer -> buffer`

这样做的直接收益是：

- 单个 layer 的显隐变化，不会天然拖累同 tile 里的所有其他 layer
- 单个 layer 的显示模式变化，也只会影响该 layer 对应的缓存键
- prepared fragments 这条路径也能按 layer 继续复用

这一步的本质是把缓存粒度继续收细。
它不会减少 draw call 数，甚至可能略微增加；
但它能显著改善"局部 UI 变动导致整块 tile 全失效"这个问题。

### 为什么这里保存的是世界坐标碎片

预碎片化结果没有直接存成 GPU 顶点，而是保存成：

- `tile -> fragment list`
- 每个 fragment 仍然是世界坐标点列

这样做的好处是：

1. 平移时完全可以复用
2. 缩放变化时只需要重新做世界坐标到缩放坐标的转换
3. 数据结构仍然留在 geometry 层，而不是直接耦合到某个 GPU buffer 生命周期里

### 现在 renderer 会怎么用它

当前 renderer 在生成某个 tile 的缓存时，会把几何分成两部分：

1. 普通 shape
- 继续从 `tile_grid.shape_indices_for_tile(tile)` 里取
- 但会过滤掉已经被预碎片化接管的那些大 shape

2. 预碎片化 shape
- 直接从 `PreparedTileFragments.per_tile[tile]` 里取世界坐标碎片
- 再按当前 zoom 和 layer draw mode 生成顶点

这样做的直接收益是：

- 大 shape 不再为每个 tile 反复走完整裁剪流程
- tile cache miss 的成本更稳定
- 代码层次也更清楚：
  - `TileGridIndex` 负责谁命中哪个 tile
  - `PreparedTileFragments` 负责超大 shape 的预拆分
  - `tile_vertex_cache` 负责最终 GPU buffer 复用

### 这一步还不是什么

它还不是最终的"离线分块数据库"，也不是"世界坐标顶点长期常驻 GPU"。

更准确地说，它是：

**在当前 tile cache 结构不推翻的前提下，把超大 shape 的重复 CPU 裁剪成本提前搬走。**

这一步特别适合当前这个学习项目，因为你可以非常清楚地看到：

- 先有 tile query
- 再有 tile local clipping
- 再有 large-shape pre-fragmentation

它们是一层一层叠上来的，而不是一下子跳到一个特别黑盒的最终方案。

### `Tile hits` 和 `Layer hits` 现在的区别

在缓存细化成 `tile + layer` 之后，命中统计也要分两层看：

- `Tile hits / misses`
  - 看当前可见 tile 这一层整体是否稳定
  - 只要某个 tile 里有任意 layer 需要重建，这个 tile 就会记一次 miss

- `Layer hits / misses`
  - 看更细粒度的缓存条目是否真的复用了
  - 这组数字更适合评估 `tile + layer` 缓存本身有没有带来收益

一个很常见的现象是：

- `Tile misses` 仍然不低
- 但 `Layer hits` 已经明显上升

这通常说明：

**同一个 tile 里只有少数 layer 在变化，而其他 layer 已经开始从细粒度缓存里获益了。**

### 预碎片结果现在为什么也改成 `tile + layer` 组织

在缓存已经细化成 `tile + layer` 之后，如果 `prepared fragments` 还只是"按 tile 一大包存"，
renderer 每帧就还得再做一遍：

- 按 tile 取出全部 prepared fragments
- 再现场按 layer 重新分桶
- 甚至为了借用关系方便而做 clone

所以现在预碎片结果本身也提前组织成：

- `tile + layer -> fragments`
- 外加 `tile -> layers` 的快速索引

这样 renderer 在热点路径里就只需要：

1. 先知道当前 tile 有哪些 prepared layers
2. 再按 `tile + layer` 直接取对应 fragment 切片

这一步的意义不在于改变最终绘制结果，
而在于把"细粒度缓存"和"细粒度几何准备"真正对齐。

### 普通 shape 为什么也要预先做 `tile + layer` 索引

在 `prepared fragments` 已经改成 `tile + layer` 组织之后，
普通 shape 如果还停留在"先拿 tile 的全量 shape，再在 renderer 里现场按 layer 分桶"，
热点路径里仍然会留下一段重复工作。

所以 `TileGridIndex` 现在也会在构建时直接准备：

- `tile -> layers`
- `tile + layer -> shape indices`

这样 `update_scene_cache()` 每帧就只需要：

1. 知道当前 tile 有哪些普通 layer
2. 直接拿对应的 `shape_indices` 切片
3. 把它和同一个 `tile + layer` 下的 prepared fragments 合并

这一步的价值在于：

- 普通 shape 路径和 prepared 路径终于对齐了
- renderer 更像一个"消费索引和缓存"的层，而不是现场做数据整理的层
- 后面继续做更细粒度的缓存统计时，入口会清楚很多


## 渐进式渲染统计

现在左侧会多出 3 个和渐进式补全直接相关的指标：

- `Pending entries`：当前视图还有多少 `tile + layer` 条目尚未补完
- `Build budget`：每一帧最多新构建多少个条目
- `Dropped stale entries`：因为用户连续缩放/平移而被丢弃的过期工作累计值

这组数字的意义是：

- 如果 `Pending entries` 很快回到 `0`，说明当前视图已经稳定
- 如果你连续滚轮缩放时 `Dropped stale entries` 上升，说明 viewer 的确跳过了中间过期状态
- `Build budget` 越大，补满速度越快，但单帧 CPU 压力也更高


### Layer-major scheduling

渐进式渲染现在仍然以内存和缓存友好的 `tile + layer` 结构作为底层实现，但 pending queue 的调度顺序已经改成 `layer-major`：

- 先按 layer 分组可见请求
- 再在同一层内遍历相关 tile

这样做不会推翻现有 cache 结构，却能让用户看到的补全过程更接近“整层逐步出现”，而不是空间块状补全。


### Layer-complete-first scheduling

现在的渐进式调度比单纯的 `layer-major` 更严格：

- 当前活动 layer 没补满之前，不切到下一 layer
- 所以用户看到的会更像“这一层先完整起来”

左侧的 `Active layer` 和 `Active layer pending` 就是用来观察这条规则是否在生效。


### Center priority inside active layer

在当前活动 layer 内部，pending 条目现在会优先选择更靠近当前视口中心的 tile。这样做不会改变 cache 结构，但会让用户正在看的区域更快稳定下来。


### Adaptive progressive bypass

当缺失的 `tile + layer` 条目数很少时，renderer 现在会直接在同一帧把它们补完，而不是刻意走渐进式补全。这样能避免轻场景下出现“其实很快能画完，却还故意慢慢补”的违和感。


### Layer display gating

前面这些优化大多只是在控制"构建顺序"。
但如果显示层仍然把所有已经缓存好的 layer 一起画出来，用户主观上还是会觉得很多层在同时冒出来。

所以 renderer 现在又加了一层"显示门控"：

- 已经没有 pending 的旧 layer：允许完整显示
- 当前活动 layer：允许显示已经构建完成的那部分
- 后续 layer：即使 cache 已经命中，也先不显示

这样一来，

- 渐进式补全的底层仍然是 `tile + layer` cache
- 但用户看到的顺序会更像"一层一层出来"

这一步解决的是显示语义，而不是再去推翻 cache 结构。


### Layer-adaptive bypass

全局 `Adaptive progressive bypass` 解决的是“整个当前视图都很轻”的情况。
但真实版图里更常见的是：

- 整个视图不轻
- 当前活动 layer 却很轻

所以现在 renderer 又加了一层按 layer 的自适应判断。

只有当下面两条同时成立时，当前活动 layer 才会在这一帧直接补完：

- `pending_entries <= layer_bypass_entry_threshold`
- `estimated_work_units <= layer_bypass_work_threshold`

这就是为什么 UI 里现在除了全局 `Bypass threshold`，又多了：

- `Layer bypass entries`
- `Layer bypass work`

这样做的好处是：

- 轻 layer 不会被不必要地慢慢补
- 重 layer 仍然保留渐进式补全
- 判断逻辑比只看条目数或只看工作量都更稳
