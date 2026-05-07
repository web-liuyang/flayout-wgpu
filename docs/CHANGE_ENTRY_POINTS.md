# 常见修改入口指南

这份文档专门回答一个很实际的问题：

**如果你准备自己改这个项目，某类需求通常应该从哪里下手？**

很多时候源码不是看不懂，而是不知道“第一刀该切哪里”。
这份指南的目标就是帮你减少这种犹豫。

---

## 先记住一个总原则

改这个项目时，优先按职责分层去找入口：

- 改文件读取与格式支持：先看 `io`
- 改内部数据结构：先看 `scene`
- 改交互：先看 `ui` 和 `camera`
- 改一帧控制流：先看 `app`
- 改 GPU 绘制与性能：先看 `renderer`
- 改行为时，最后一定回头补对应 `tests`

如果你一上来就直接在 `app/mod.rs` 或 `renderer/mod.rs` 里到处塞逻辑，
代码很快就会重新变乱。

---

## 场景 1：想支持新的版图图元类型

例如：
- GDS `Text`
- 更完整的 polygon
- 更复杂的 OASIS 元素

### 第一入口
- `src/io/laykit_loader.rs`

### 你通常要改什么
1. 找到对应格式里的元素枚举分支
2. 新增一个 `push_xxx(...)` 或 `expand_xxx(...)`
3. 把外部格式类型转成内部 `RectShape` 或未来更通用的 `Shape`
4. 补 bounds、points、closed、stroke_width_world 等信息

### 第二入口
- `src/scene/mod.rs`

如果新图元已经无法被当前 `RectShape` 模型舒服表达，
你可能需要先升级内部模型。

### 第三入口
- `tests/io_test.rs`
- `src/io/laykit_loader.rs` 内部测试

### 一句话判断
如果你的需求是“文件里多一种内容被正确读出来”，第一站一定是 `laykit_loader.rs`。

---

## 场景 2：想支持新的层级语义

例如：
- 更完整的 `STRANS`
- 旋转/镜像引用
- 更复杂的阵列变换

### 第一入口
- `src/io/laykit_loader.rs`

### 重点函数
- `expand_gds_struct_ref`
- `expand_gds_array_ref`
- `expand_oasis_placement`
- `array_ref_offsets`

### 为什么从这里开始
因为这些能力属于“导入时如何把层级变成内部场景”的问题，
不是 renderer 的职责。

renderer 应该尽量接收已经语义清晰的 scene，
而不是自己去理解 GDS/OASIS 的层级规则。

---

## 场景 3：想增加一个新的左侧 UI 控件

例如：
- 新按钮
- 新 slider
- 新图层过滤开关
- 新统计显示项

### 第一入口
- `src/ui/mod.rs`

### 常见改法
1. 在 `UiAction` 里新增一个字段
2. 在 `draw_ui(...)` 里加控件并写入动作
3. 在 `src/app/mod.rs` 的 `redraw()` 里消费这个动作
4. 如果需要，进一步传给 renderer / camera / scene

### 为什么不要让 UI 直接改 renderer
因为这个项目现在有一个很好的边界：
- UI 只收集意图
- app 统一调度副作用

尽量保持这个模式，对你后续加功能会很有帮助。

---

## 场景 4：想改拖拽、缩放、fit 的手感

例如：
- 缩放更灵敏
- 平移更跟手
- fit 边距更大/更小
- 缩放上下限调整

### 第一入口
- `src/camera/mod.rs`

### 常见改法
- 改 `MIN_ZOOM` / `MAX_ZOOM`
- 改 `zoom_by(...)`
- 改 `fit_bounds(...)`

### 第二入口
- `src/ui/mod.rs`

如果是滚轮映射的手感，例如：
- `factor = (scroll / 240.0).exp()`

这类更偏输入到动作的转换，就先看 UI。

### 测试配套
- `tests/camera_test.rs`

---

## 场景 5：想改默认文件、默认窗口大小、启动行为

### 第一入口
- `src/config/mod.rs`

### 第二入口
- `src/app/mod.rs`

如果只是：
- 改默认文件路径
- 改窗口标题
- 改初始尺寸

通常 `config` 就够了。

如果是：
- 启动时不自动加载
- 启动后弹文件对话框
- 增加多种启动模式

那就继续看 `app/mod.rs`。

---

## 场景 6：想增加新的 Scene 级元信息

例如：
- layer 统计数量
- cell 名附加信息
- shape 分类统计
- scene revision 之外的附加状态

### 第一入口
- `src/scene/mod.rs`

### 第二入口
- `src/io/laykit_loader.rs`

如果这些信息来自文件本身，通常需要导入层一起改。

### 第三入口
- `src/ui/mod.rs`

如果你希望把这些信息显示出来，再接 UI。

### hierarchy level range 属于这类需求的一个典型例子

现在这条链已经升级成：
1. `layout/mod.rs` 保留分层 source
2. `io/laykit_loader.rs` 把 `GDSStructure -> LayoutCell`
3. `layout/view_builder.rs` 按 `min/max level` 构建临时 workset `Scene`
4. `app/mod.rs` 只保留当前 source 和当前 workset，不再常驻 `full_scene`
5. `ui/mod.rs` 只负责暴露 level range slider
6. `persistence/mod.rs` 再决定这些范围如何保存/恢复

---

## 场景 7：想改颜色、线宽、视觉表现

例如：
- 改 layer 默认颜色
- 改 path 的线宽策略
- 改不同 layer 的粗细映射

### 第一入口
- `src/renderer/geometry.rs`

### 重点函数
- `layer_color(...)`
- `stroke_width(...)`
- `emit_segment(...)`

### 为什么不是 UI
因为这些都属于渲染语义，不是界面语义。

---

## 场景 8：想增加新的 renderer 调试信息

例如：
- tile cache 大小
- 当前 tile 数
- 某一帧重建的 buffer 数量
- 更细的 query stats

### 第一入口
- `src/renderer/mod.rs`

### 第二入口
- `src/ui/mod.rs`

### 常见改法
1. 在 `RenderDebugStats` 里加字段
2. 在 `update_scene_cache(...)` 里更新字段
3. 在 `ui::draw_ui(...)` 左侧面板显示出来
4. 在 `tests/renderer_test.rs` 增加对应断言

---

## 场景 9：想改 tile grid 和缓存策略

例如：
- tile grid 更细或更粗
- tile cache key 增加字段
- 加缓存上限
- 加淘汰策略

### 第一入口
- `src/renderer/mod.rs`
- `src/renderer/geometry.rs`

### 如何分工理解
- `geometry.rs`：更偏“tile 如何切、如何查、如何生成几何”
- `mod.rs`：更偏“tile cache 生命周期和一帧调度”

### 如果你想加缓存容量上限
通常入口会在：
- `tile_vertex_cache`
- `update_scene_cache(...)`

### 如果你想改 tile slider 范围
入口在：
- `src/renderer/geometry.rs` 的 `MIN/MAX_TILE_GRID_DIVISIONS`
- `src/ui/mod.rs` 的 slider

---

## 场景 10：想改“哪一帧需要重新计算”

例如：
- 某个新参数变化时必须失效缓存
- 某个状态变化不应该触发整帧查询

### 第一入口
- `src/renderer/geometry.rs` -> `build_render_cache_key(...)`
- `src/renderer/mod.rs` -> `update_scene_cache(...)`

### 你应该先问自己两个问题
1. 这个变化会不会影响“当前看见什么”？
2. 这个变化会不会影响“tile 顶点本身”？

如果：
- 只影响可见判断，通常进 `RenderCacheKey`
- 会影响 tile 顶点内容，通常还要进 `TileCacheDomainKey`

这是当前缓存设计里最重要的判断方法。

---

## 场景 11：想改 shader 或 GPU 顶点格式

例如：
- 顶点里再加一列属性
- shader 里做更复杂的颜色逻辑
- 把更多变换留给 GPU

### 第一入口
- `src/renderer/pipeline.rs`
- `src/renderer/scene.wgsl`

### 第二入口
- `src/renderer/geometry.rs`

因为顶点格式一变，CPU 生成的 `LineVertex` 也要一起变。

### 第三入口
- `tests/renderer_test.rs`

这类改动很容易影响坐标链和顶点数量，测试最好一起补。

---

## 场景 12：想把更多逻辑移到 GPU

例如：
- world-space 顶点长期驻留 GPU
- shader 里做更多相机变换
- 减少 CPU 每次缩放的几何重建

### 第一入口
- `src/renderer/geometry.rs`
- `src/renderer/pipeline.rs`
- `src/renderer/scene.wgsl`

### 当前你要特别留意的事实
现在 tile 顶点保存的是：
- 已经乘过 `zoom` 的逻辑屏幕坐标

如果你想把更多逻辑移到 GPU，
很可能要改成：
- 顶点保存 world-space 坐标
- shader 接收更完整的 camera uniform

这会影响整个 tile cache 设计，所以这类改动通常不只是改 shader 一处。

---

## 场景 13：想加文件打开、重新加载之类的功能

### 第一入口
- `src/ui/mod.rs`
- `src/app/mod.rs`

### 你大概率会改什么
- UI 加按钮
- app 收到动作后更新 `layout_path`
- 再调用 `load_layout()`

### 如果引入系统文件对话框
你还可能会改：
- `main.rs` 或 `app/mod.rs` 的平台集成

---

## 场景 14：想改测试，而不是改功能

### 先做什么
先判断这个改动属于哪一层：
- 相机行为 -> `tests/camera_test.rs`
- scene / IO -> `tests/io_test.rs`
- 性能统计 -> `tests/perf_test.rs`
- 渲染正确性 / 缓存 / 索引 -> `tests/renderer_test.rs`

### 一个很实用的原则
改功能时尽量遵循：
1. 先加或改测试
2. 让测试先失败
3. 再改实现
4. 再看是不是需要补文档

---

## 如果你拿不准应该从哪开始

你可以先问自己这 3 个问题：

1. 这个需求更像“文件语义”还是“显示语义”？
2. 这个需求更像“状态变化”还是“绘制变化”？
3. 这个需求会不会影响测试里已经锁住的行为？

大多数情况下：
- 文件语义 -> `io`
- 显示语义 -> `renderer`
- 状态变化 -> `app`
- 交互入口 -> `ui`
- 几何和 bounds -> `scene`

---

## 推荐你接下来真的动手做的练习

如果你想开始练，不妨按难度从低到高试这几个：

1. 改 `layer_color(...)`，观察颜色变化
2. 改 `Tile grid` slider 范围
3. 在左侧 UI 新增一个自定义统计项
4. 给 path 换一种线宽映射策略
5. 给 renderer 再加一个 cache 调试字段
6. 给导入层多支持一种简单图元

这几个练习覆盖了：
- UI
- app
- scene
- renderer
- io

基本能把项目结构都摸一遍。


## 想加每层显示模式

当前这条能力已经接进来了，后续如果你想继续扩展：

- `src/ui/mod.rs`：layer 列表里的下拉框和交互收集
- `src/app/mod.rs`：把 UI 动作变成 app 状态，再同步给 renderer
- `src/renderer/mod.rs`：缓存失效、cache key、tile cache domain
- `src/renderer/geometry.rs`：真正决定某个 layer 最终按 `Outline / Hatch / Hatch + Outline` 哪种路径发顶点

如果你以后想做"每层不同 hatch 样式"，最自然的落点也是这四处。

---

## 场景 13：想改 viewer 配置保存 / 恢复行为

例如：
- 想增加新的持久化字段
- 想把“全局一份”升级成“按文件分别记忆”
- 想改配置文件落盘路径
- 想把保存时机从“退出时”改成“实时保存”

### 第一入口
- `src/persistence/mod.rs`

### 第二入口
- `src/app/mod.rs`

### 你通常要改什么
1. 在 `ViewerConfig` 里增加或调整持久化字段
2. 增加 persisted 类型和 runtime 类型之间的转换
3. 在 `app` 里调整：
   - 启动时如何读取配置
   - 布局加载后如何应用 scene 相关状态
   - 退出时如何收集并保存当前状态

### 测试配套
- `tests/persistence_test.rs`

### 一句话判断
如果你的需求是“关掉再打开以后 viewer 应该记住什么”，第一站一定是 `persistence/mod.rs`。

## 想扩 hatch 预设时先改哪里

如果你想再加新的 hatch 图案，例如方格、三角点阵或其他规则样式，优先看这几处：

- `src/renderer/geometry.rs`：新增 `HatchStylePreset` 枚举值，并决定 fill 顶点怎么携带该 preset
- `src/renderer/scene.wgsl`：真正把 preset 解释成屏幕图案
- `src/ui/mod.rs`：把新 preset 暴露到每层下拉框
- `src/persistence/mod.rs`：让它能进配置保存与恢复

这条能力目前是刻意按“可扩展 preset”来建模的，
所以加新图案时，尽量保持“几何路径不变，shader 语义扩展”这个方向。


## 想改按 layer bypass 规则时先看哪里

如果你以后想调整“某一层要不要直接一帧补完”的判断，优先看这几处：

- `src/renderer/mod.rs`：`LayerPendingStats`、`estimate_active_layer_pending_stats`、`should_bypass_progressive_for_layer`
- `src/ui/mod.rs`：阈值 slider 和调试文案
- `src/persistence/mod.rs`：阈值持久化

当前这套实现是刻意按“双阈值 + 轻量估算”来建模的。
如果后面你想引入更复杂的代价模型，最好先保留这层结构，再逐步替换估算公式，而不是把调度逻辑和估算逻辑重新搅在一起。
