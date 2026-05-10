# Performance Tuning

这份文档的目标很直接：

> 当你看到某个性能症状时，先去看哪些指标，再优先调哪些参数。

它不是一份“理论最优”指南，而是一份结合当前项目实际架构的**调参地图**。

## 1. 先分清你在优化哪一层

当前 viewer 大致有四个容易互相混淆的性能层次：

1. `app` workset / hierarchy range / 视口裁剪
2. `layout` 子树展开
3. `renderer::geometry` 顶点量 / clipping / hatch / LOD
4. `renderer` tile cache / pending build / draw call / GPU 提交

所以第一步不是直接改常量，而是先判断症状更像哪一层。

## 2. 先看哪些指标

调优前，建议先盯左侧面板里的这些数字：

### 交互期

- `FPS`
- `Frame`
- `Pending entries`
- `Tile misses`
- `Visible tiles`
- `Cache hit/miss`

### 静态期

- `FPS`
- `Vertices`
- `Draw calls`
- `Cache bytes`
- `Pending entries`

## 3. 症状 -> 优先排查方向

### 症状 A：缩放或拖动时卡，但停下来后还行

更像：
- workset 重建太频繁
- pending tile build 太重
- zoom 造成 tile cache 抖动

优先看：
- `Pending entries`
- `Tile misses`
- `Cache hit/miss`

先看这些常量：

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`
  - `LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO`
  - `LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`
  - `DIRECT_HIERARCHY_TILE_CACHE_ZOOM_BUCKET_RATIO`
  - `DEFAULT_PROGRESSIVE_BUILD_BUDGET`
  - `DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD`
  - `DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD`
  - `DEFAULT_LAYER_BYPASS_WORK_THRESHOLD`

### 症状 B：静态不动也慢，`Pending entries` 已经接近 0

更像：
- GPU / draw-path / present 限速
- draw calls 太多
- 顶点量太大

优先看：
- `Vertices`
- `Draw calls`
- `Pending entries`

先看这些常量：

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`
  - `DIRECT_HIERARCHY_MAX_TILE_GRID_DIVISIONS`
  - `DIRECT_HIERARCHY_OUTLINE_ONLY_MAX_ZOOM`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`
  - `SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT`
  - `TINY_SHAPE_MARKER_MAX_SCREEN_EXTENT`
  - `CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT`
  - `POLYLINE_LOD_MAX_SCREEN_EXTENT`

### 症状 C：内存占用高，但还不一定卡

更像：
- workset 太大
- tile cache 太大
- prepared fragments 太多

优先看：
- `Cache bytes`
- `Cache entries`
- `Prepared fragments`
- `Prepared shapes`

先看这些常量：

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`
  - `INITIAL_LAYOUT_WORKSET_SHAPE_BUDGET`
  - `INITIAL_LAYOUT_WORKSET_POINT_BUDGET`
  - `DIRECT_LAYOUT_TILE_SOURCE_SHAPE_THRESHOLD`
  - `DIRECT_LAYOUT_TILE_SOURCE_POINT_THRESHOLD`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`
  - `DEFAULT_TILE_CACHE_CAPACITY`
  - `TILE_CACHE_BYTES_PER_ENTRY_BUDGET`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`
  - `DEFAULT_PREPARED_FRAGMENT_BUDGET`
  - `DEFAULT_PREPARED_POINT_BUDGET`
  - `LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD`

### 症状 D：远景时图形“看起来不完整”或者细节消失过多

更像：
- LOD 过早
- subtree collapse 太激进
- tiny marker / suppress fill 阈值太早触发

优先看：
- `Vertices`
- 当前 `Zoom`
- 画面上是否只剩 outline / marker

先看这些常量：

- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`
  - `HIERARCHICAL_SUBTREE_MIN_SCREEN_EXTENT`
  - `HIERARCHICAL_SUBTREE_MIN_COLLAPSE_LEVEL`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`
  - `SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT`
  - `TINY_SHAPE_MARKER_MAX_SCREEN_EXTENT`
  - `TINY_SHAPE_MARKER_SCREEN_SIZE`
  - `CLOSED_SHAPE_LOD_MIN_POINTS`
  - `POLYLINE_LOD_MIN_POINTS`

## 4. 常量分组说明

## app 层：决定是否先构 workset

文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`

### `INITIAL_LAYOUT_WORKSET_SHAPE_BUDGET`
### `INITIAL_LAYOUT_WORKSET_POINT_BUDGET`

作用：
- 决定初始化 hierarchy range 时，什么量级还算“可以直接构 workset”

调大：
- 更容易直接展开更多层级
- 初始显示更完整
- 但更容易把内存打高

调小：
- 更早保守收浅
- 更安全
- 但初始看起来会更“粗”

### `DIRECT_LAYOUT_TILE_SOURCE_SHAPE_THRESHOLD`
### `DIRECT_LAYOUT_TILE_SOURCE_POINT_THRESHOLD`

作用：
- 决定什么时候切到 direct hierarchy tile render

调大：
- 更多场景继续走传统 workset + scene 路径

调小：
- 更早切到 direct hierarchy

### `LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO`

作用：
- workset 构建时比当前视口多预取多少范围

调大：
- 连续拖动时更少重建
- 但单次 workset 更大

调小：
- 单次 workset 更轻
- 但 pan 更容易频繁重建

### `LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD`

作用：
- 允许当前 zoom 相对旧 workset zoom 漂移多少

调大：
- 更容忍小幅缩放
- 更少重建
- 但 screen-space LOD 更可能滞后

调小：
- 画面更快追上当前 zoom
- 但缩放时更容易频繁 rebuild

## layout 层：控制子树展开

文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/app/mod.rs`
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/layout/view_builder.rs`

### `HIERARCHICAL_SUBTREE_MIN_SCREEN_EXTENT`

作用：
- 子树在屏幕上小到什么程度开始允许折叠

调大：
- 更早折叠深层子树
- 内存和展开成本更低
- 更容易丢远景细节

调小：
- 更保守
- 远景结构更完整
- 成本更高

### `HIERARCHICAL_SUBTREE_MIN_COLLAPSE_LEVEL`

作用：
- 至少多深的 hierarchy level 才允许折叠

调大：
- 浅层骨架更稳定
- 不容易“整体看起来缺块”
- 但远景仍会保留更多真实展开

调小：
- 更早从浅层就开始压缩

## geometry 层：控制顶点量

文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/geometry.rs`

### `SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT`

作用：
- 小闭合图元从 `Hatch/HatchOutline` 退到 `Outline` 的阈值

调大：
- 更早关闭 fill
- 顶点量更低
- 远景图面感更弱

调小：
- 更保留 hatch
- 成本更高

### `TINY_SHAPE_MARKER_MAX_SCREEN_EXTENT`

作用：
- 闭合图元什么时候彻底压成 marker

调大：
- 更激进省顶点
- 远景会更抽象

调小：
- 更保留真实几何

### `CLOSED_SHAPE_LOD_MAX_SCREEN_EXTENT`
### `POLYLINE_LOD_MAX_SCREEN_EXTENT`

作用：
- shape 已经很小时，是否允许点列降采样

调大：
- 更多图元会触发 coarse LOD
- 顶点数更容易下降

调小：
- 更保守
- 细节保留更多

### `LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD`

作用：
- 一个 shape 跨多少 tile 之后，才值得预碎片化

调大：
- 更少 shape 会被预切块
- 内存副本更少
- 运行时可能多做一些现场裁剪

调小：
- 更早预切块
- 大 shape 重复裁剪更少
- 但内存副本更大

## renderer 层：控制 cache、progressive build 和 draw

文件：
- `/Users/liuyang/Desktop/store/flayout-wgpu/src/renderer/mod.rs`

### `DEFAULT_TILE_CACHE_CAPACITY`

作用：
- 允许缓存多少个 `tile + layer` 条目

调大：
- 更容易命中
- 交互更稳
- 内存更高

调小：
- 内存更低
- 但更容易 miss / 重建

### `TILE_CACHE_BYTES_PER_ENTRY_BUDGET`

作用：
- 每个 cache entry 粗略允许占用多少字节

调大：
- 大 tile/layer 几何更容易留在 cache

调小：
- 更快驱逐超大条目

### `DEFAULT_PROGRESSIVE_BUILD_BUDGET`

作用：
- 每帧最多补多少个 pending `tile + layer`

调大：
- 补图更快
- 但更容易拖慢交互帧

调小：
- 交互更稳
- 但补图更慢

### `DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD`

作用：
- 当缺失条目很少时，是否直接一帧补完

调大：
- 小缺口更快消失

调小：
- 更坚持渐进式

### `DEFAULT_LAYER_BYPASS_ENTRY_THRESHOLD`
### `DEFAULT_LAYER_BYPASS_WORK_THRESHOLD`

作用：
- 当前 active layer 是否允许临时 bypass，直接补完

调大：
- 小/中层更容易一帧完成

调小：
- 更保守地按预算推进

### `DIRECT_HIERARCHY_TILE_CACHE_ZOOM_BUCKET_RATIO`

作用：
- 相近 zoom 复用同一套 tile 几何的程度

调大：
- zoom 变化更不容易 miss
- 缩放手感更稳
- 但缩放过程中几何可能更“滞后于真实 zoom”

调小：
- zoom 更精确
- 但 cache 更容易抖动

### `DIRECT_HIERARCHY_MAX_TILE_GRID_DIVISIONS`

作用：
- direct hierarchy 路径内部允许的最大 tile grid 密度

调大：
- tile 更细
- 可见集更局部
- 但 draw / cache / 重复几何通常会上升

调小：
- tile 更粗
- 更有利于压静态 draw 和远景重复顶点
- 但局部更新粒度更粗

### `DIRECT_HIERARCHY_OUTLINE_ONLY_MAX_ZOOM`

作用：
- direct hierarchy 超远景下，何时内部把 hatch 退到 outline

调大：
- 更早降低 fill 成本

调小：
- 更保留真实 hatch

## 5. 建议的实际调优顺序

如果你之后要自己做一轮新的性能实验，我建议按这个顺序：

1. 先固定一个真实文件和固定视角
2. 先记录四组数字
   - FPS
   - Vertices
   - Draw calls
   - Pending entries
3. 只改一组相关常量
4. 每次只改一个方向
   - 先改 tile / draw 层
   - 再改 geometry LOD
   - 最后再动 app/workset 阈值
5. 每次改完重新看那四组数字

## 6. 一些经验性判断

### 如果静态很慢

优先动：
- `DIRECT_HIERARCHY_MAX_TILE_GRID_DIVISIONS`
- `SMALL_CLOSED_SHAPE_OUTLINE_ONLY_MAX_SCREEN_EXTENT`
- `DIRECT_HIERARCHY_OUTLINE_ONLY_MAX_ZOOM`

### 如果缩放时卡

优先动：
- `DIRECT_HIERARCHY_TILE_CACHE_ZOOM_BUCKET_RATIO`
- `LAYOUT_WORKSET_REBUILD_ZOOM_RATIO_THRESHOLD`
- `DEFAULT_PROGRESSIVE_BUILD_BUDGET`

### 如果拖动时总在补图

优先动：
- `LAYOUT_WORKSET_PREFETCH_MARGIN_RATIO`
- `DEFAULT_TILE_CACHE_CAPACITY`
- `DEFAULT_PROGRESSIVE_BYPASS_THRESHOLD`

### 如果内存太高

优先动：
- `DIRECT_LAYOUT_TILE_SOURCE_*`
- `DEFAULT_TILE_CACHE_CAPACITY`
- `DEFAULT_PREPARED_*_BUDGET`
- `HIERARCHICAL_SUBTREE_MIN_SCREEN_EXTENT`

## 7. 最后一个建议

不要一开始就同时改：

- tile grid
- hatch
- subtree LOD
- progressive budget

这些参数互相之间是会耦合的。  
如果同时改太多，你会很难判断到底是哪一层真的带来了收益。
