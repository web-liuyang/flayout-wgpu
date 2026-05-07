# flayout-wgpu 阅读指南

这份指南的目标不是重复源码注释，而是告诉你：

1. 先看哪些文件
2. 每个文件最值得抓住什么问题
3. 遇到不懂时，应该顺着哪条数据流继续看

## 推荐阅读顺序

1. `src/main.rs`
2. `src/lib.rs`
3. `src/app/mod.rs`
4. `src/ui/mod.rs`
5. `src/camera/mod.rs`
6. `src/scene/mod.rs`
7. `src/layout/mod.rs`
8. `src/layout/view_builder.rs`
9. `src/persistence/mod.rs`
10. `src/io/mod.rs`
11. `src/io/laykit_loader.rs`
12. `src/renderer/mod.rs`
13. `src/renderer/geometry.rs`
14. `src/renderer/pipeline.rs`
15. `src/renderer/scene.wgsl`
16. `tests/` 目录
17. `docs/LAYOUT_IMPORT_GUIDE.md`
18. `docs/RENDER_FRAME_GUIDE.md`
19. `docs/PERF_CACHE_GUIDE.md`
20. `docs/SHAPE_TO_PIXEL_GUIDE.md`
21. `docs/SOURCE_INDEX.md`
22. `docs/TEST_WALKTHROUGH.md`

## 为什么这样看

### 1. 先看入口和总控

先读 `src/main.rs` 和 `src/app/mod.rs`，因为这两处能帮你先建立“系统地图”。

重点看：
- 程序从哪里启动
- 窗口什么时候创建
- 默认版图文件什么时候加载
- `egui` 动作是怎么流到 `camera` 和 `renderer` 的

如果你一开始就直接扎进 `renderer`，很容易只看到局部实现，但不知道是谁在驱动它。

### 2. 再看 UI 和相机

接着读 `src/ui/mod.rs` 和 `src/camera/mod.rs`。

重点看：
- 左侧面板如何收集交互
- 中央画布如何收集拖拽与滚轮
- 为什么 `UiAction` 不直接改 renderer
- `fit_bounds`、`zoom_by`、`translate_screen` 分别负责什么

这里能帮你理解“交互为什么跟手”和“fit to window 为什么能工作”。

### 3. 再看 scene 层和新的 layout 层

现在有两层数据结构要分开理解：

- `scene/mod.rs`：renderer 真正消费的扁平 workset
- `layout/mod.rs`：GDS 的分层 source

先读 `src/scene/mod.rs`，再读 `src/layout/mod.rs` 和 `src/layout/view_builder.rs`。

重点看：
- 为什么需要 `Scene`
- 为什么需要 `SceneBundle`
- 为什么现在还需要额外一层 `LayoutBundle`
- `RectShape` 现在其实已经承担了哪些图元职责
- `hierarchy_level` 为什么直接挂在 shape 上
- `Bounds` 为什么会被这么多地方复用

这一组文件一起构成当前项目的数据中台。

### 4. 再看 view builder 和 persistence 层

`src/layout/view_builder.rs` 是这轮内存重构里最值得跟的一层。

重点看：
- 为什么输入是 `LayoutBundle + root + level range + visible bounds`
- 为什么输出仍然是 renderer 熟悉的 `Scene`
- 为什么这一步能显著减少常驻内存

接着再读 `src/persistence/mod.rs`。

读 `src/persistence/mod.rs` 时，重点看：
- 为什么要把持久化结构和 runtime 结构分开
- 为什么按 scene 恢复的过滤逻辑也放在 persistence 层
- 为什么 hierarchy level range 在配置里用 `Option<u32>`
- 为什么只保存 viewer 状态，而不保存 renderer cache

这层很适合理解“哪些状态值得长期记住，哪些状态只属于运行时”。

### 5. 再看 IO / laykit 适配层

读 `src/io/mod.rs` 和 `src/io/laykit_loader.rs`。

重点看：
- 为什么不能让 renderer 直接依赖 `laykit`
- root cell 是怎么找出来的
- `StructRef` / `ArrayRef` / `Placement` 为什么要递归展开
- 为什么 path 只保留中心线和世界坐标线宽

这一层最适合帮助你理解“版图文件格式”和“查看器内部场景”之间的边界。

### 6. 最后看 renderer

读 `src/renderer/mod.rs`、`src/renderer/geometry.rs`、`src/renderer/pipeline.rs`、`src/renderer/scene.wgsl`。

建议顺序：
- 先看 `renderer/mod.rs`，理解一帧渲染流程
- 再看 `geometry.rs`，理解缓存、裁剪、tile 和顶点生成
- 再看 `pipeline.rs` 和 `scene.wgsl`，理解 GPU 如何接收这些数据

重点看：
- 为什么有 `ShapeSpatialIndex`
- 为什么又有 `TileGridIndex`
- 为什么 tile 顶点只做缩放，不做平移
- 为什么平移要放到 shader uniform 里
- `cached_scene_key` 和 `tile_cache_domain` 分别是在保护什么

## 这个 demo 当前的数据流

可以把整个 viewer 理解成这条主链：

`layout file -> laykit -> LayoutBundle/SceneBundle -> view builder or flat scene -> visible query -> tile cache -> GPU draw -> egui overlay`

更细一点：

1. `app` 从默认路径读取版图文件
2. `io/laykit_loader`
   - `GDS` 转成 `LayoutBundle`
   - `OASIS` 目前仍然转成扁平 `SceneBundle`
3. `app`
   - 选择当前 root/view
   - 决定当前 `min/max level`
4. `layout/view_builder`
   - 只在 `GDS` 路径上，把当前 root 的必要子树展开成临时 `Scene`
5. `camera` 决定当前可见世界范围
6. `renderer/geometry` 先通过空间索引找 visible shapes / visible tiles
7. `renderer` 用 tile cache 复用已有 GPU buffer
8. `scene.wgsl` 再把平移和 viewport 变换补齐
9. `egui` 最后把左侧 UI 叠加到画面上

## 你最值得重点理解的 5 个点

### 1. 为什么有 `SceneBundle`

因为一个 GDS 文件不一定只有一个“真正可看的入口单元”。
如果没有 `SceneBundle`，你只能默认读一个 cell，很多时候会让用户不知道当前看到的是哪个层级。

### 2. 为什么 path 不直接在解析层变成粗线三角形

因为 path 的屏幕线宽跟 zoom 有关系。
如果在解析层就把它烘焙成固定几何，缩放后会不自然。
所以现在保留的是：
- 中心线点列
- 世界坐标线宽

把最终屏幕线宽换算放到渲染阶段做。

### 3. 为什么有两个“网格”

- `ShapeSpatialIndex`：偏查询，用来减少每帧扫描的 shape 数
- `TileGridIndex`：偏缓存，用来把几何切成可复用的 tile buffer

它们表面都像网格，但目的不一样。

### 4. 为什么 tile 顶点不直接生成最终屏幕位置

因为平移很频繁。
如果顶点里直接写死平移后的坐标，那么每拖一下鼠标，所有 tile buffer 都得重建。
现在的做法是：
- CPU 先生成“缩放后的逻辑屏幕坐标”
- shader 再加 `translation`

这样平移时更容易复用 tile buffer。

### 5. 为什么 UI 不直接调用 renderer 改状态

因为会让状态流变得很乱。
现在 `ui` 只产出 `UiAction`，`app` 再统一处理：
- 先切 scene
- 再改 hidden layers
- 再改 tile grid
- 再改 camera
- 最后进入 render

这种结构对学习和后续扩展都更友好。

### 6. 为什么 hierarchy level range 现在放在 app / view builder 层，而不是 renderer 层

现在新的 `GDS` 路径是：
- `LayoutBundle` 保留分层 source
- `app` 保留当前 root / level range
- `view_builder` 只展开当前要求的层级
- renderer 只消费这个 workset `Scene`

这样做的好处是：
- renderer 不需要知道“层级范围”这个业务概念
- 现有索引、tile cache、统计链可以直接复用
- 更深层的数据如果当前不需要，就不会整份常驻内存

### 7. 为什么现在值得先读 `memory_probe`

如果你在学这轮“从根上减内存”的重构，我会建议你顺手看：

- `examples/memory_probe.rs`

它现在能直接展示：
- 当前是 `flat-legacy` 还是 `hierarchical-gds`
- 当前请求的 `min/max level`
- workset 展开后的 `shape_count / total_points`

这个 probe 很适合帮你把“结构变化”变成直观数字。 

## 如果你想带着问题去读

建议你用这几个问题来对照源码：

1. 当前画面上的一个 shape，是怎么从 GDS 里的 boundary 走到 GPU 顶点的？
2. 为什么平移不会让 tile cache 全部失效？
3. 为什么缩放会让 tile cache domain 变化？
4. 为什么 high DPI 下要区分 logical point 和 physical pixel？
5. 为什么 layer 显隐变化必须进入 cache key？

## 建议的文档阅读顺序

如果你更喜欢先看讲解再回源码，我建议这样读：

1. `docs/READING_GUIDE.md`
2. `docs/LAYOUT_IMPORT_GUIDE.md`
3. `docs/RENDER_FRAME_GUIDE.md`
4. `docs/PERF_CACHE_GUIDE.md`
5. `docs/SHAPE_TO_PIXEL_GUIDE.md`
6. `docs/SOURCE_INDEX.md`
7. `docs/TEST_WALKTHROUGH.md`

它们分别回答的是：
- 整体地图是什么
- 文件是怎么进来的
- 一帧是怎么画出来的
- 缓存为什么这样设计
- 一个具体图形是怎么变成像素的
- 某个功能和函数具体在代码里的哪里
- 测试各自在保护什么系统行为

## 测试怎么配合阅读

当你读完一个模块后，去看对应测试：

- `tests/camera_test.rs`：看交互手感相关约束
- `tests/io_test.rs`：看 scene / IO / 基础投影契约
- `tests/perf_test.rs`：看 FPS 统计最小保证
- `tests/renderer_test.rs`：看渲染正确性与性能优化约束

比较好的阅读方式是：
- 先看实现注释
- 再看测试名字
- 再猜这个测试为什么存在
- 最后看断言细节

这样会比只从实现代码往下读更容易建立“设计意图”。

## 新近值得关注的一条链

如果你现在重新阅读这套代码，最值得跟一遍的新能力是“每层 hatch preset”。
建议按这个顺序看：

1. `src/ui/mod.rs`：用户如何给某一层选择图案
2. `src/app/mod.rs`：默认交替分配和状态同步
3. `src/persistence/mod.rs`：为什么这些 preset 关掉重开还能恢复
4. `src/renderer/geometry.rs`：fill 顶点如何携带 `hatch_style`
5. `src/renderer/scene.wgsl`：shader 如何把同一批 fill 三角形解释成不同图案


## 新近值得关注的另一条性能链

如果你现在重点在学性能而不是图案渲染，那么最值得补读的一条链是“按 layer 双阈值 bypass”。
建议按这个顺序看：

1. `src/renderer/mod.rs`：`LayerPendingStats`、`should_bypass_progressive_for_layer`、`effective_build_budget_for_active_layer`
2. `src/ui/mod.rs`：为什么现在有 `Layer bypass entries / Layer bypass work` 两个 slider
3. `src/persistence/mod.rs`：为什么这些阈值重启后还能保留
4. `docs/PERF_CACHE_GUIDE.md`：全局 bypass 和 layer bypass 的职责分工
