# flayout-wgpu

一个使用 Rust 构建的学习型芯片版图查看器 demo。

当前目标不是立刻做成完整 EDA 工具，而是先把一条**结构清晰、方便继续演化**的主链搭起来：

- 能读取版图文件
- 能选择 root cell
- 能查看图层
- 能平移、缩放、fit
- 能在渲染层逐步引入真实的性能优化思路

当前技术栈以：

- `winit`：窗口和事件循环
- `wgpu`：主画布渲染
- `egui`：左侧面板和交互外壳
- `laykit`：GDS / OASIS 解析

为核心。

---

## 这个项目现在适合谁

它特别适合下面这类场景：

- 你想学 Rust 图形/桌面应用怎么搭结构
- 你想学版图查看器这类“大画布 + 海量图元”应用的基本思路
- 你想从一个真实可运行的小项目里理解：
  - 导入层怎么设计
  - Scene 中间层为什么重要
  - 相机和交互怎么组织
  - 渲染优化为什么要分层做

如果你的目标是“今天就拿到一个工业级 GDS viewer”，那这个项目还不是。
如果你的目标是“把一个 viewer 的骨架真正学懂，然后自己继续扩展”，它会比较合适。

---

## 当前已经具备的能力

- 读取 `GDS` / `OASIS`
- 自动找出 root cells，并通过 UI 选择当前 cell
- `GDS` 已切到分层内存模型，不再默认把整棵 hierarchy 先扁平展开
- 按实例层级深度过滤显示范围（`Min level / Max level`）
- 显示基础轮廓/路径
- 鼠标拖拽平移
- 鼠标滚轮缩放
- `Fit to window`
- 图层显隐
- Viewer 配置自动保存与恢复
- FPS / frame time 显示
- renderer 调试统计显示
- tile grid 参数可调
- 基础空间索引
- tile 级缓存

---

## 当前还没有做完的部分

下面这些能力目前还没有完整实现，或者还只是简化版：

- 更完整的 GDS/OASIS 图元支持
- 更完整的 `STRANS` / 旋转 / 镜像 / 复杂实例变换
- 多边形真实填充
- 真正面向超大规模版图的数据流和 GPU 常驻缓存
- tile cache 容量上限和淘汰策略
- picking / hover / 选择框
- 更完整的项目化工作流

也就是说，当前项目已经进入“结构正确、可继续扩展”的阶段，但还没有进入“功能全面”的阶段。

---

## 项目结构

```text
src/
  app/                 顶层应用编排
  camera/              2D 相机
  config/              默认路径、窗口尺寸等配置
  io/                  文件导入与 laykit 适配层
  perf/                FPS / 帧时间统计
  persistence/         viewer 配置持久化
  renderer/            wgpu 渲染、几何生成、缓存和优化
  scene/               内部统一场景模型
  ui/                  egui 左侧面板和画布交互
  error.rs             统一错误类型
  lib.rs               crate 入口
  main.rs              可执行入口

tests/
  camera_test.rs       相机和交互手感相关测试
  io_test.rs           IO / Scene / 投影基础行为测试
  perf_test.rs         FPS 统计测试
  persistence_test.rs  viewer 配置持久化测试
  renderer_test.rs     渲染正确性与缓存/索引回归测试

docs/
  READING_GUIDE.md         阅读顺序指南
  LAYOUT_IMPORT_GUIDE.md   版图导入链讲解
  RENDER_FRAME_GUIDE.md    一帧渲染主链讲解
  PERF_CACHE_GUIDE.md      缓存与性能优化讲解
  SHAPE_TO_PIXEL_GUIDE.md  单个图形到屏幕像素的推导
  SOURCE_INDEX.md          源码索引手册
  TEST_WALKTHROUGH.md      测试讲解手册
  CHANGE_ENTRY_POINTS.md   常见修改入口指南
```

---

## 如何运行

### 1. 配置默认文件路径

先修改：

- `src/config/mod.rs`

里面的：

- `DEFAULT_LAYOUT_PATH`

把它改成你本地真实存在的 `.gds` 或 `.oas` 文件。

### 2. 运行项目

```bash
cargo run
```

### 3. 常用交互

- 左侧 `Cell view`：切换 root cell
- 左侧 `Fit to window`：重新适配当前 cell
- 左侧 `Layers`：按 layer/datatype 显隐
- 中央画布拖拽：平移
- 中央画布滚轮：缩放
- 左侧 `Tile grid`：调节 tile 切分密度，观察缓存行为

### 4. Viewer 配置保存

应用现在会自动保存一份全局 viewer 配置，并在下次启动时恢复。

当前会保存：
- 最近打开的版图文件路径
- 当前 cell view
- 当前 hierarchy level range
- layer 显隐与每层显示模式
- hatch 参数
- 每层 hatch 预设（左斜、右斜、交叉、点阵）
- adaptive per-layer progressive bypass for lightweight layers
- tile grid / tile cache / bypass threshold
- camera pan / zoom

配置文件会放在系统配置目录下的 `flayout-wgpu/viewer-config.json`。
在 macOS 上通常类似：

- `~/Library/Application Support/io.openai.flayout-wgpu/viewer-config.json`

---

## 当前最重要的设计思路

### 1. 导入层和渲染层隔离

`laykit` 只存在于 `io` 层。
对 renderer 来说，它只认识内部 `Scene` / `SceneBundle`。

这样以后即使换解析库，也不会让整个项目都被牵着改。

### 2. `GDS` 先保留层次化模型，再按需构建 workset

当前 `GDS` 路径已经不再走“加载时先把整棵 hierarchy 完全扁平展开”的模式。
现在更接近：

- `laykit_loader` 先构造 `LayoutBundle`
- `app` 持有当前 root cell 和当前 level range
- `view_builder` 再按需生成当前 workset `Scene`

这一步非常关键，因为它让常驻内存更多地跟：
- 当前 root cell
- 当前 level range
- 当前视图工作集

绑定，而不是和整棵深层 hierarchy 绑定。

### 3. UI 不直接改 renderer

`ui` 只产生 `UiAction`，`app` 再统一调度状态变化。
这样状态流更清楚，也更容易扩功能。

### 3.5. hierarchy level 过滤放在 app / workset builder 层，而不是 renderer 层

现在的实现是：
- `GDS`：`LayoutBundle` 保留分层 source
- `app`：记录当前 `min/max level`
- `view_builder`：按这个范围构建当前 workset `Scene`
- renderer：只消费已经裁好的 workset

这样做的好处是：
- renderer 不需要知道“层级范围”这个业务概念
- 现有索引、tile cache、统计链都可以直接复用
- UI 仍然可以拿到当前 root cell 的真实 `max level`

### 4. 平移和缩放的职责拆开

当前 tile 顶点缓存保存的是：
- 已乘过 `zoom` 的逻辑屏幕坐标

而平移则通过 shader uniform 补上。

这样：
- 平移时更容易复用 tile buffer
- 缩放时再重新生成 tile 顶点

这是当前缓存设计里最关键的折中。

### 5. 先用可理解的优化，再追极致性能

当前项目没有一开始就追求最复杂的 GPU 常驻策略，
而是先按下面的顺序建立性能思维：

1. 空间索引减少扫描
2. 离屏裁剪减少顶点生成
3. 帧级缓存减少重复查询
4. tile cache 复用 GPU buffer

这个顺序更适合学习，也更利于持续验证。

### 6. 当前内存架构和 probe

如果你现在重点在学“大版图为什么容易爆内存”，最值得看的新链路是：

- `src/layout/mod.rs`
- `src/layout/view_builder.rs`
- `src/io/laykit_loader.rs`
- `examples/memory_probe.rs`

当前 `memory_probe` 已经能区分：
- `flat-legacy`
- `hierarchical-gds`

例如对 `example_mzi_perf.gds / MziArray`，在只展开 `0..2` 层时，当前 probe 实测：
- `shape_count = 230016`
- `total_points = 1150080`

而旧的全量扁平路径曾经探测到：
- `shape_count = 2274428`
- `total_points = 55362608`

这条对比能很直观地说明：
- 这轮重构真正减少的不是一点点 cache
- 而是“整棵 hierarchy 默认常驻”的数据本体

---

## 推荐阅读顺序

如果你是第一次看这个项目，建议先读：

1. `docs/READING_GUIDE.md`
2. `docs/SOURCE_INDEX.md`
3. `docs/LAYOUT_IMPORT_GUIDE.md`
4. `docs/RENDER_FRAME_GUIDE.md`
5. `docs/PERF_CACHE_GUIDE.md`
6. `docs/SHAPE_TO_PIXEL_GUIDE.md`
7. `docs/TEST_WALKTHROUGH.md`
8. `docs/CHANGE_ENTRY_POINTS.md`

这套顺序会比直接从 `renderer/mod.rs` 一头扎进去更容易真正建立全局理解。

---

## 如果你接下来想自己动手改

比较好的练习顺序是：

1. 改 `layer_color(...)`，熟悉 renderer 表现层
2. 改 `Tile grid` slider 范围，熟悉 UI -> app -> renderer 传参链
3. 新增一个 renderer 调试字段，熟悉 stats 流
4. 给导入层多支持一种简单图元，熟悉 `laykit -> Scene` 转换
5. 做一个小的缓存策略实验，例如 tile cache 限制大小

如果你不知道某种改动该从哪里下手，可以直接看：

- `docs/CHANGE_ENTRY_POINTS.md`

---

## 当前项目的学习资料状态

目前这个仓库已经不只是“一个能跑的 demo”，而是：

- 有源码中文注释
- 有测试中文注释
- 有按主题拆开的讲解文档
- 有源码索引
- 有修改入口指南

也就是说，它已经比较适合作为一个“带教学属性的小型 viewer 项目”继续演化。

---

## 后续演进方向

当你准备继续开发时，下一阶段比较自然的方向包括：

- 更完整的版图图元支持
- 更完整的实例变换支持
- tile cache 容量上限和淘汰策略
- 更大规模数据下的缓存与上传策略
- 选择、hover、拾取
- 文件打开与项目化交互

当前最合适的节奏是：

**先把结构和学习材料吃透，再继续扩功能和做更深的性能优化。**
EOF
