# 测试讲解手册

这份文档的目标是帮助你把 `tests/` 目录从“回归检查脚本”读成“设计说明书”。

当前这个项目里，测试最大的价值不只是防回归，而是：

- 告诉你系统希望保持什么行为
- 记录我们曾经踩过什么坑
- 帮助你从断言反推出设计意图

如果你在读源码时感觉“函数能看懂，但不知道为什么要这么写”，
那就去对照这份文档和对应测试。

## 怎么读这些测试

推荐顺序：

1. `tests/camera_test.rs`
2. `tests/io_test.rs`
3. `tests/perf_test.rs`
4. `tests/renderer_test.rs`

原因是：
- 先理解交互和基础数据行为
- 再理解导入和投影
- 最后再进入最复杂的 renderer 测试

---

## 1. `tests/camera_test.rs`

### `zoom_increases_scale`

它在保护什么：
- 最基础的滚轮放大行为

为什么重要：
- 这类测试看起来简单，但它能保证相机最基本的倍率方向不会写反

### `app_pan_helper_moves_camera`

它在保护什么：
- `app` 侧对 pan delta 的叠加逻辑是线性的

为什么重要：
- 当交互变复杂时，这种很小的辅助函数也可能被改坏
- 用测试锁住它，可以避免把“拖不跟手”的问题怀疑到更深层

### `fit_bounds_selects_positive_zoom`

它在保护什么：
- fit 后 zoom 必须合理且可预测

为什么重要：
- fit to window 是查看器最常用的入口动作之一
- 如果它不稳定，很多后续问题都会被放大

### `zoom_can_return_to_fit_scale_after_zooming_in`

它在保护什么：
- 放大后还能缩回 fit 比例

为什么重要：
- 这是之前真实踩过的 bug
- 根因是最小缩放限制过大

这个测试的意义不是数学多复杂，而是它直接记录了：

**我们已经发现过这个交互坑，而且不想再回去。**

---

## 2. `tests/io_test.rs`

### `default_layout_path_constant_is_available`

它在保护什么：
- demo 启动时必须有默认文件路径可用

为什么重要：
- 当前项目的使用方式本来就依赖静态路径
- 如果路径常量被删空，程序虽然能编译，但启动体验会立刻退化

### `empty_scene_reports_zero_shapes`

它在保护什么：
- 空场景的统计和 bounds 语义要干净

为什么重要：
- 空场景是很多错误路径和初始状态的基础
- 如果这里行为不干净，UI 和 renderer 都会被迫写更多防御逻辑

### `invalid_path_returns_missing_file_error`

它在保护什么：
- 错误路径要走 `MissingFile` 语义，而不是 panic

为什么重要：
- 这是把“程序崩掉”和“程序优雅报错”区分开的关键测试

### `geometry_projection_keeps_points_in_canvas`

它在保护什么：
- fit 后，一个简单矩形的投影点应该留在画布范围内

为什么重要：
- 它相当于一个非常基础的“坐标链路 sanity check”
- 当相机或投影逻辑改动时，这类测试能很早报警

### `failed_load_state_exposes_error_text`

它在保护什么：
- `LoadState::Failed` 里的错误信息要能传到 UI 层

为什么重要：
- 如果错误文本被吃掉，调试体验会明显变差

### `scene_exposes_unique_sorted_layers`

它在保护什么：
- scene 输出的 layer 列表必须：
  - 去重
  - 排序

为什么重要：
- 左侧 layer 面板就依赖这个结果
- 如果不稳定，UI 顺序会飘，用户体验会差

### `path_shape_can_preserve_world_stroke_width`

它在保护什么：
- path 图元的世界线宽不会在 scene 层丢失

为什么重要：
- renderer 正是靠这个字段才能根据 zoom 算屏幕线宽

---

## 3. `tests/perf_test.rs`

### `frame_stats_reports_positive_fps_after_ticks`

它在保护什么：
- `FrameStats` 至少能在记录几帧后给出正数 FPS 和 frame time

为什么重要：
- `perf.rs` 很小，但 UI 一直在展示它
- 这个测试能防止最基本的统计逻辑被写坏

---

## 4. `tests/renderer_test.rs`

这是最关键的一组测试。

### `rectangle_scene_builds_clip_space_line_vertices`

它在保护什么：
- 一个最小矩形场景能被正确展开成 NDC 顶点
- 顶点数量符合“粗线三角形”逻辑

为什么重要：
- 它验证的是最核心的“能不能画出来”
- 24 个顶点这个数字，实际上也在提醒你当前粗线是如何生成的

### `retina_viewport_uses_logical_canvas_scale_for_fit`

它在保护什么：
- 高 DPI 下逻辑 fit 效果要和普通 DPI 一致

为什么重要：
- 这是之前真实踩过的 Retina 坐标坑
- 如果没有这条测试，后来再改 viewport 逻辑时很容易回归

### `render_cache_key_changes_when_hidden_layers_change`

它在保护什么：
- 图层显隐变化必须进入帧级缓存 key

为什么重要：
- 否则会出现 UI 切 layer 但画面复用旧结果的错误

### `offscreen_shapes_do_not_generate_vertices`

它在保护什么：
- 完全离屏的 shape 不应该继续生成顶点

为什么重要：
- 这是离屏裁剪优化最直观的一条回归测试

### `spatial_index_returns_only_shapes_overlapping_visible_world_bounds`

它在保护什么：
- `ShapeSpatialIndex` 至少在功能上要能把可见 shape 过滤出来

为什么重要：
- 它不是在测“索引快不快”，而是在测“索引对不对”

### `visible_shape_query_reports_bucket_and_candidate_stats`

它在保护什么：
- 可见查询除了给结果，还要能返回统计指标

为什么重要：
- 左侧 `Renderer` 面板里很多数字就依赖这些统计
- 这类测试帮助你把“调试可观测性”也当成系统契约的一部分

### `render_debug_stats_capture_query_and_cache_state`

它在保护什么：
- `RenderDebugStats` 的字段不会在重构里漏掉或错位

为什么重要：
- 调试结构经常容易在重构时被顺手改坏
- 这条测试是在锁定观测层语义

### `tile_grid_returns_only_tiles_overlapping_visible_world_bounds`

它在保护什么：
- tile 查询至少在功能上正确

为什么重要：
- 当前 tile cache 全都建立在“tile 查询靠谱”的前提上

### `denser_tile_grid_splits_visible_region_into_more_tiles`

它在保护什么：
- 调大 tile grid 密度后，可见区域会被切成更多 tile

为什么重要：
- 这是我们后来加 `Tile grid` slider 时补上的行为测试
- 它记录了 UI 参数和底层 tile 切分之间的真实约束

---

## 如何把测试和源码一起读

这里给你一个很实用的方法：

### 场景 1：你在看 `camera/mod.rs`
先配套看：
- `tests/camera_test.rs`

顺序建议：
1. 先读函数
2. 再看测试名
3. 再想“这个测试是不是在防某个真实体验问题”

### 场景 2：你在看 `laykit_loader.rs`
先配套看：
- `tests/io_test.rs`
- `src/io/laykit_loader.rs` 自带测试

重点去想：
- 哪些行为是“文件格式契约”
- 哪些行为是“viewer 设计选择”

### 场景 3：你在看 `renderer`
先配套看：
- `tests/renderer_test.rs`
- `docs/RENDER_FRAME_GUIDE.md`
- `docs/PERF_CACHE_GUIDE.md`

这样你就不会只看到一堆缓存结构和几何函数名，而能把它们放回设计上下文里。

---

## 你最该记住的测试阅读原则

1. 测试不是“额外内容”，而是设计意图的一部分
2. 失败过一次的 bug，最好都能在测试里找到影子
3. 名字简单的测试，往往在保护很关键的用户体验
4. renderer 测试很多时候不是在测视觉细节，而是在测“渲染约束”

---

## 一个很适合你的练习

你可以自己选 3 条测试，尝试回答这三个问题：

1. 它在保护什么行为？
2. 如果删掉它，未来最可能回归什么问题？
3. 它更像“功能测试”还是“设计契约测试”？

如果你能把这些问题答顺，后面你自己加功能时，测试会写得越来越有质量。
