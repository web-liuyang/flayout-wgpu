# 源码索引手册

这份文档的目标不是讲原理，而是帮助你在代码里快速定位：

- 某个功能在哪个文件
- 某个关键行为主要由哪个函数负责
- 当你带着问题读源码时，应该先跳到哪里

你可以把它当成“项目地图 + 函数索引”。

## 项目总览

当前项目按职责大致分成 5 层：

1. 入口层
2. 应用编排层
3. 数据与导入层
4. 交互与 UI 层
5. 渲染与性能层

---

## 1. 入口层

### `src/main.rs`

#### `main`
负责：
- 初始化日志
- 启动 `ViewerApp`
- 打印启动级错误

什么时候看它：
- 你想知道程序最开始从哪进入
- 你想加命令行参数或多种启动模式

### `src/lib.rs`

负责：
- 暴露整个 crate 的模块结构

什么时候看它：
- 你想先建立模块全图
- 你想知道有哪些一级模块

---

## 2. 应用编排层

### `src/app/mod.rs`

这是整个项目的总控文件。

#### `ViewerApp::run`
负责：
- 创建事件循环
- 启动应用实例

#### `ViewerApp::new`
负责：
- 初始化所有运行时状态
- 设置默认路径、默认画布大小、默认 tile grid 参数

#### `ViewerApp::create_window`
负责：
- 创建窗口
- 创建 renderer
- 创建 egui 状态
- 启动后立即加载默认版图

什么时候看它：
- 你想知道窗口和 renderer 是谁先创建的
- 你想知道默认文件何时加载

#### `ViewerApp::load_layout`
负责：
- 调用 `io::load_layout_bundle`
- 更新 `scene_bundle`、`scene`、`load_state`
- 场景变化后通知 renderer

什么时候看它：
- 你想知道文件加载成功后，场景是怎么进入 app 的
- 你想改成“重新加载当前文件”按钮

#### `ViewerApp::select_scene_view`
负责：
- 切换 root cell 视图
- 重置隐藏图层
- 通知 renderer 切换到新 scene

什么时候看它：
- 你想知道左侧 `Cell view` 下拉框选中后发生了什么

#### `ViewerApp::fit_scene`
负责：
- 用当前 `Scene.bounds()` 和画布大小调用 `camera.fit_bounds`

什么时候看它：
- 你想理解 fit to window 为什么能工作

#### `ViewerApp::redraw`
负责：
- 记录帧时间
- 跑一轮 egui UI
- 接收 `UiAction`
- 应用动作到 scene/camera/renderer
- 最后调用 `renderer.render`

这是整个项目最重要的控制流函数之一。

什么时候看它：
- 你想理解“一帧是怎么跑起来的”
- 你想知道 UI 动作怎么进入 renderer

#### `ViewerApp::request_redraw`
负责：
- 请求下一帧重绘

#### `ApplicationHandler::window_event`
负责：
- 分发 winit 事件
- 处理 resize、scale factor 变化、关闭窗口、触发 redraw

#### `ApplicationHandler::about_to_wait`
负责：
- 当前采用持续请求重绘的策略

什么时候看它：
- 你想知道为什么 FPS 会持续刷新

---

## 3. 数据与导入层

### `src/config/mod.rs`

负责：
- 默认文件路径
- 初始窗口尺寸
- 标题

什么时候看它：
- 你想切换默认版图文件
- 你想调启动窗口尺寸

### `src/error.rs`

#### `AppError`
负责：
- 统一表达路径错误、解析错误、渲染初始化错误

什么时候看它：
- 你想新增错误类型
- 你想改 UI 错误展示语义

### `src/io/mod.rs`

#### `load_layout_bundle`
负责：
- 统一导入入口
- 判空、判文件存在、按扩展名分发到 GDS/OASIS 加载器

#### `load_layout_scene`
负责：
- 在测试或极简调用场景中直接拿当前 scene

什么时候看它：
- 你想知道 app 调用解析层的统一入口是什么

### `src/io/laykit_loader.rs`

这是版图导入核心文件。

#### `load_gds`
负责：
- 用 laykit 读取 GDS 文件
- 调 `build_gds_scene_bundle`

#### `load_oasis`
负责：
- 用 laykit 读取 OASIS 文件
- 调 `build_oasis_scene_bundle`

#### `build_gds_scene_bundle`
负责：
- 找 root structures
- 把每个 root structure 转成一个 `SceneView`

#### `build_oasis_scene_bundle`
负责：
- 找 root cells
- 把每个 root cell 转成一个 `SceneView`

#### `collect_gds_shapes`
负责：
- 扫描一个 structure 的元素
- 将边界/盒子/路径转成内部 shape
- 递归展开 `StructRef` / `ArrayRef`

#### `collect_oasis_shapes`
负责：
- 扫描一个 OASIS cell 的元素
- 递归展开 `Placement`

#### `expand_gds_struct_ref`
负责：
- 展开一个 GDS cell 引用
- 做循环引用保护
- 累加坐标偏移

#### `expand_gds_array_ref`
负责：
- 展开阵列引用
- 对每个阵列实例递归进入子 cell

#### `expand_oasis_placement`
负责：
- 展开 OASIS placement

#### `push_boundary`
负责：
- 把 GDS boundary 转成内部轮廓 shape

#### `push_gds_box`
负责：
- 把 GDS box 转成内部轮廓 shape

#### `push_gds_path`
负责：
- 把 GDS path 转成内部折线 shape
- 保留 `stroke_width_world`

#### `push_rectangle`
负责：
- 把 OASIS rectangle 转成内部矩形 shape

#### `array_ref_offsets`
负责：
- 把 GDS `ArrayRef` 的阵列描述还原成每个实例的 offset

#### `bounds_from_i32_points`
负责：
- 从整数点集推导 bounds

什么时候看这个文件：
- 你想扩展新的图元类型
- 你想加更完整的层级语义
- 你想理解 root cell 为什么能被正确列出来

### `src/scene/mod.rs`

这是内部统一模型层。

#### `Bounds`
负责：
- 表达包围盒
- 提供 `width/height/center/translate/pad/intersects/union`

#### `LayerId`
负责：
- 统一表达 layer + datatype

#### `RectShape`
负责：
- 表达当前 viewer 的基础几何图元

#### `RectShape::rectangle`
负责：
- 从 bounds 构造闭合矩形轮廓

#### `RectShape::polyline`
负责：
- 构造带世界线宽的折线图元

#### `Scene`
负责：
- 承载一个可渲染场景里的所有 shape

#### `Scene::bounds`
负责：
- 求整个 scene 的整体包围盒

#### `Scene::layer_ids`
负责：
- 返回去重且排序后的 layer 列表

#### `SceneBundle`
负责：
- 承载一个文件里的多个可选视图

#### `SceneBundle::select`
负责：
- 切换当前选中的 scene view

什么时候看它：
- 你想理解 renderer 消费的数据长什么样
- 你想知道 UI 的 layer 列表和 bounds 从哪里来

---

## 4. UI 与交互层

### `src/camera/mod.rs`

#### `Camera2D::new`
负责：
- 创建默认相机

#### `Camera2D::translate_screen`
负责：
- 根据屏幕拖拽 delta 做平移

#### `Camera2D::zoom_by`
负责：
- 以光标为中心缩放

#### `Camera2D::fit_bounds`
负责：
- 让一个 bounds 自动适配当前 viewport

什么时候看它：
- 你想调交互手感
- 你想理解 zoom / pan / fit 的数学关系

### `src/ui/mod.rs`

#### `UiAction`
负责：
- 承载当前帧 UI 收集到的所有动作意图

#### `draw_ui`
负责：
- 画左侧面板
- 画中央画布边框
- 收集：cell 切换、fit 请求、拖拽、缩放、layer 显隐、tile grid slider 变化

什么时候看它：
- 你想新增一个按钮、开关、面板指标
- 你想理解为什么 UI 不直接操作 renderer

---

## 5. 渲染与性能层

### `src/perf.rs`

#### `FrameStats::record_frame`
负责：
- 记录帧耗时到滑动窗口

#### `FrameStats::fps`
负责：
- 用平均帧时间换算 FPS

什么时候看它：
- 你想改 FPS 统计窗口长度

### `src/renderer/pipeline.rs`

#### `SceneUniform`
负责：
- 把 `translation` 和 `viewport_size` 传给 shader

#### `ScenePipeline::new`
负责：
- 创建 shader module
- 创建 bind group layout
- 创建 render pipeline

#### `ScenePipeline::create_bind_group`
负责：
- 为某一帧的 uniform buffer 建立 bind group

什么时候看它：
- 你想改 shader 输入结构
- 你想改 blend / 顶点布局 / 渲染状态

### `src/renderer/scene.wgsl`

#### `vs_main`
负责：
- 给缩放后的顶点加 translation
- 再转成 NDC

#### `fs_main`
负责：
- 输出颜色

什么时候看它：
- 你想看最终顶点怎么进屏幕
- 你想理解平移为什么能不重建 tile 顶点

### `src/renderer/geometry.rs`

这是当前所有“几何准备”和“查询辅助”的核心文件。

#### `LineVertex`
负责：
- 定义送进 GPU 的顶点格式

#### `RenderCacheKey`
负责：
- 帧级别缓存 key

#### `TileId`
负责：
- 标识 tile 编号

#### `ShapeSpatialIndex`
负责：
- 减少每帧扫描 shape 的数量

#### `ShapeSpatialIndex::build`
负责：
- 根据 scene 构建均匀网格索引

#### `TileGridIndex`
负责：
- 管理 tile 切分与 tile -> shape 映射

#### `TileGridIndex::build_with_divisions`
负责：
- 按指定网格密度构建 tile grid

#### `query_visible_shapes`
负责：
- 返回与当前可见范围相交的 shape
- 同时给出 bucket / candidate / visible 统计

#### `query_visible_tiles`
负责：
- 返回当前可见范围命中的 tile 列表

#### `camera_visible_world_bounds`
负责：
- 根据 camera 反推出当前可见 world bounds

#### `build_render_cache_key`
负责：
- 生成一帧级别的 scene cache key

#### `project_points`
负责：
- 把一个 shape 的点完整投影到屏幕
- 常用于测试和理解坐标链路

#### `build_line_vertices`
负责：
- 为测试场景直接生成最终 NDC 顶点

#### `build_scaled_line_vertices_for_indices`
负责：
- 为给定 shape 集合生成“只乘了 zoom”的逻辑屏幕顶点

#### `transform_vertices_to_ndc`
负责：
- 给逻辑屏幕顶点加 translation 并转 NDC

#### `emit_segment`
负责：
- 把一段线膨胀成两个三角形

#### `layer_color`
负责：
- 根据 layer 决定颜色

#### `stroke_width`
负责：
- 根据图元和 zoom 决定最终屏幕线宽

什么时候看它：
- 你想理解可见查询、tile 切分、线段膨胀、坐标变换
- 你想继续做性能优化

### `src/renderer/mod.rs`

这是 renderer 总控文件。

#### `RenderDebugStats`
负责：
- 向 UI 暴露 renderer 的调试与性能统计

#### `TileCacheKey`
负责：
- 单个 tile buffer 的缓存键

#### `TileCacheDomainKey`
负责：
- 判断整批 tile buffer 是否还能继续复用

#### `Renderer::new`
负责：
- 初始化 surface / device / queue / egui renderer / scene pipeline

#### `Renderer::update_scene`
负责：
- 替换 scene
- 重建索引
- 失效相关缓存

#### `Renderer::set_tile_grid_divisions`
负责：
- 更新 tile grid 密度
- 失效 tile 相关缓存

#### `Renderer::render`
负责：
- 整帧 scene pass + egui pass
- 设置 scissor
- 调用 `update_scene_cache`
- 绘制所有可见 tile

#### `Renderer::update_scene_cache`
负责：
- 用 `RenderCacheKey` 做帧级缓存
- 用 `TileCacheDomainKey` 管理 tile cache 生命周期
- 查询 visible shapes / visible tiles
- 为 misses 的 tile 生成并上传 vertex buffer
- 更新 debug stats

#### `create_tile_cache_entry`
负责：
- 为一个 tile 的顶点数据创建 GPU vertex buffer

什么时候看它：
- 你想理解真正的一帧渲染流程
- 你想改缓存策略或 draw 粒度

---

## 6. 测试层

### `tests/camera_test.rs`
负责：
- 锁住缩放、平移、fit 的手感与边界行为

### `tests/io_test.rs`
负责：
- 锁住 scene / IO / 基础投影契约

### `tests/perf_test.rs`
负责：
- 锁住 FPS 统计最小行为

### `tests/renderer_test.rs`
负责：
- 锁住渲染正确性与主要性能优化约束

---

## 7. 文档层

### `docs/READING_GUIDE.md`
适合：
- 第一次进项目时建立阅读顺序

### `docs/LAYOUT_IMPORT_GUIDE.md`
适合：
- 专门理解版图文件如何转成 SceneBundle

### `docs/RENDER_FRAME_GUIDE.md`
适合：
- 专门理解一帧如何被画出来

### `docs/PERF_CACHE_GUIDE.md`
适合：
- 专门理解缓存和性能优化层次

### `docs/SHAPE_TO_PIXEL_GUIDE.md`
适合：
- 手推一个 shape 是怎么变成屏幕像素的

---

## 如果你带着具体问题来查

### 想知道“为什么 fit to window 正常”
先看：
1. `src/app/mod.rs` -> `fit_scene`
2. `src/camera/mod.rs` -> `fit_bounds`

### 想知道“为什么平移跟手”
先看：
1. `src/ui/mod.rs` -> `draw_ui` 里的拖拽采集
2. `src/app/mod.rs` -> `redraw`
3. `src/camera/mod.rs` -> `translate_screen`

### 想知道“为什么平移不会让 tile cache 全部失效”
先看：
1. `docs/PERF_CACHE_GUIDE.md`
2. `src/renderer/mod.rs` -> `TileCacheDomainKey`
3. `src/renderer/scene.wgsl` -> `vs_main`

### 想知道“为什么能切换 Cell view”
先看：
1. `src/io/laykit_loader.rs` -> `build_gds_scene_bundle` / `build_oasis_scene_bundle`
2. `src/scene/mod.rs` -> `SceneBundle`
3. `src/ui/mod.rs` -> `draw_ui`
4. `src/app/mod.rs` -> `select_scene_view`

### 想知道“一个 GDS boundary 最后是怎么画到屏幕上的”
先看：
1. `docs/LAYOUT_IMPORT_GUIDE.md`
2. `docs/RENDER_FRAME_GUIDE.md`
3. `docs/SHAPE_TO_PIXEL_GUIDE.md`
4. `src/io/laykit_loader.rs` -> `push_boundary`
5. `src/renderer/geometry.rs` -> `build_scaled_line_vertices_for_indices`
6. `src/renderer/geometry.rs` -> `emit_segment`
7. `src/renderer/scene.wgsl` -> `vs_main`

---

## 最后一个建议

如果你准备开始真正深入源码，不要试图从头到尾线性读完整个项目。

更好的方式是：
- 先带一个具体问题
- 用这份索引跳到相关文件和函数
- 读完后再回到讲解文档补背景

这样你会更容易把“模块职责”和“设计意图”真正记住。
