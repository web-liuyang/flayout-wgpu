# 版图导入链讲解

这份文档专门讲一件事：

**一个 GDS / OASIS 文件，是怎么被当前项目读进来并变成内部 `SceneBundle` 的？**

如果你想继续扩展：
- 更多图元类型
- 更多层级特性
- 更完整的版图语义

那这一层就是最应该先弄懂的地方。

## 当前导入层涉及的文件

1. `src/config/mod.rs`
2. `src/io/mod.rs`
3. `src/io/laykit_loader.rs`
4. `src/scene/mod.rs`
5. `tests/io_test.rs`

## 一句话总览

导入链现在可以概括成：

`layout path -> 根据扩展名选择解析入口 -> laykit 读文件 -> 找 root cells -> 递归展开层级 -> 转成内部 RectShape -> 组装 SceneBundle`

## 第 1 步：路径从哪里来

当前默认路径来自：

`src/config/mod.rs`

这里有一个 `DEFAULT_LAYOUT_PATH`。

`app` 在窗口创建完成后，会调用 `load_layout()`，
而 `load_layout()` 最终就会把这个路径交给 `io::load_layout_bundle(...)`。

所以目前整个导入链的起点不是文件选择器，而是一个静态配置常量。

这虽然简单，但对学习型 demo 非常有利，因为它减少了 UI 和平台文件对话框的噪音。

## 第 2 步：`io/mod.rs` 做统一入口分发

`src/io/mod.rs` 的职责很克制：

1. 判断路径是不是空
2. 判断文件是不是存在
3. 读取扩展名
4. 根据扩展名分发到：
- `load_gds(...)`
- `load_oasis(...)`

这里最重要的设计思想是：

**上层不需要知道 `laykit` 细节，也不需要知道 GDS 和 OASIS 各自的入口类型。**

也就是说，`io/mod.rs` 负责向外暴露一个干净的“统一导入接口”。

## 第 3 步：为什么导入结果不是 `Scene`，而是 `SceneBundle`

这是导入层最关键的设计之一。

一个版图文件，尤其是 GDS，经常不止一个你真正想直接看的顶层单元。
比如：
- 一个总装单元
- 一个测试单元
- 一些独立 root blocks

如果导入层只返回单个 `Scene`，就意味着你必须在某个地方“偷偷替用户选一个 cell”。
这往往会让用户迷惑：
- 为什么显示的是这个？
- 另一个 cell 去哪了？

所以当前设计是：

- `SceneView { name, scene }`：一个可选视图
- `SceneBundle { views, selected }`：一组视图和当前选择项

UI 再把它展示成 `Cell view` 下拉框。

## 第 4 步：为什么要先找 root cells

在 `laykit_loader.rs` 里，GDS 和 OASIS 都先做了一件事：

- 扫描所有引用关系
- 找出“没有被别人引用”的单元

这类单元通常就是 root cells。

### GDS

通过：
- `StructRef`
- `ArrayRef`

来统计谁被引用了。

### OASIS

通过：
- `Placement`

来统计谁被引用了。

最后得到：
- root structures
- root cells

为什么这么做？

因为如果你把所有 cell 都直接丢给 UI：
- 会混入大量中间子单元
- 用户很难知道哪些是“入口单元”
- 切换体验会很乱

所以当前 viewer 只把 root cells 暴露给 UI。

## 第 5 步：如果没有明显 root，为什么回退为全部 cells

有些实际数据并不总是“教科书式地拥有清晰 root”。

可能出现：
- 数据不完整
- 某些引用关系没有形成你预期的树
- 文件本身就是某种中间产物

如果这时我们硬要求必须有 root，就会让查看器什么也显示不出来。

所以代码里有一个务实的回退：

- 如果找不到 root，就把全部 structure / cell 都当成可选视图

这体现的是学习项目里一个很重要的原则：

**优先保证“用户能看到东西”，再逐步收紧规则。**

## 第 6 步：为什么要递归展开层级

GDS / OASIS 常常不是“所有图形都直接躺在顶层 cell 里”，
而是：

- 顶层 cell 引用子 cell
- 子 cell 又引用更小的 leaf cell
- 每层都有自己的局部坐标系

如果你只画当前 cell 自己直接包含的元素，而不展开引用，
那你看到的往往只是：
- 空空的顶层
- 或者一堆局部坐标重叠的错乱图形

这也是我们之前真实踩过的坑之一。

所以当前导入层一定会递归做：

- `StructRef` 展开
- `ArrayRef` 展开
- `Placement` 展开

最后把层级结构“压平”为一个可直接渲染的 shape 列表。

## 第 7 步：为什么要把 offset 一层层累加

每个子 cell 里的点，最初都处在它自己的局部坐标系中。

例如：
- leaf cell 里的 boundary 在 `(0, 0) ~ (10, 20)`
- 顶层通过 `StructRef` 把它放到 `(100, 200)`

那真正应该画出来的位置就是：

`(局部点坐标) + (父引用 offset)`

如果有多层引用，就要一直累加：

`leaf local -> parent local -> root local`

当前实现里，这个累加是通过递归参数 `offset: Vec2` 完成的。

## 第 8 步：为什么还要维护 `stack`

递归展开引用时，代码里还维护了一条 `stack`。

它的作用是防止循环引用。

虽然很多正常版图数据不会出现这种情况，
但如果某个文件真的形成：

- A 引用 B
- B 又引用 A

那没有保护的话，递归会无限下去。

所以每次进入一个子 cell 之前，都会检查：

- 这个名字是否已经在当前递归栈里

如果已经在，就报：
- `cyclic cell reference detected`

这是一种很典型的“层级展开防炸保护”。

## 第 9 步：为什么当前支持的图元类型比较少

这是有意为之，不是遗漏。

当前导入层的目标不是成为完整版图数据库，
而是先支撑这个学习型 viewer 跑通关键主链。

### GDS 当前重点支持

- `Boundary`
- `Box`
- `Path`
- `StructRef`
- `ArrayRef`

### OASIS 当前重点支持

- `Rectangle`
- `Polygon`
- `OPath`
- `Placement`
- 上面几者中已接入部分的 `repetition` 展开

这些类型已经足够让很多 demo 和测试版图显示出主要结构。

## 第 10 步：为什么 `Boundary` / `Box` / `Path` 要转成统一内部 shape

导入层不会把这些格式专有类型直接交给 renderer。
而是统一转成：

- `LayerId`
- `Bounds`
- `points`
- `closed`
- `stroke_width_world`

也就是 `RectShape`。

这一步的意义非常大：

### 对 renderer 来说

它只要关心：
- 点列是什么
- 是闭合轮廓还是折线
- 属于哪一层
- 线宽是多少

而不需要关心：
- 原始数据来自 GDS 还是 OASIS
- 原始图元名字叫 Boundary 还是 Rectangle

### 对后续扩展来说

如果你以后换解析库，只要还能产出这套内部结构，
渲染层就几乎不用动。

## 第 7.5 步：为什么现在不能只传 `offset`，而要传完整变换

这是这段时间里导入层最重要的一次升级。

一开始，层级展开只传一个 `offset: Vec2`，
也就是默认所有实例都只是“平移放置”。

这种写法在最小 demo 阶段很常见，因为它简单、直观，
而且很多小测试数据确实也只用到了平移。

但只要文件里出现：
- `StructRef.strans`
- `ArrayRef.strans`
- `Placement.mirror / angle / magnification`

单纯累加 offset 就不够了。

因为子 cell 里的一个点，不再只是：

`global = local + offset`

而是更一般的：

`global = parent_transform(child_transform(local_point))`

也就是说，真正需要往下递归传递的是：
- 局部 X 轴现在指向哪里
- 局部 Y 轴现在指向哪里
- 当前原点被放到了哪里

这就是为什么现在代码里引入了 `Transform2D`。

它本质上是在保存：
- `basis_x`
- `basis_y`
- `translation`

这样做有几个直接好处：

1. `Boundary / Box / Path / Rectangle` 都能统一做点变换
2. `StructRef / ArrayRef / Placement` 都能统一做层级组合
3. `Path` 的世界线宽也能跟着 magnification 一起缩放
4. 后面如果你要继续支持更多实例语义，扩展点会非常清楚

这也是一个很典型的工程演进过程：

- 第一版先用 offset 跑通最小闭环
- 真遇到旋转/镜像/缩放数据后，再把递归参数升级成完整变换

这种演进是健康的，不是返工。
因为前一版帮我们先把：
- UI
- 场景
- 渲染
- 调试手段

都立住了，后面升级导入层时问题范围就会非常可控。

## 第 11 步：为什么 path 不在导入层直接变成粗线三角形

这是当前实现里最值得记住的一点。

path 在版图里是“有世界坐标线宽”的几何。

如果导入层直接把它烘焙成固定三角形：
- 缩放后线宽会不自然
- renderer 失去根据 zoom 调整线宽的机会
- 后面更难做不同显示模式

所以当前导入层只保留：
- 中心线 `points`
- `stroke_width_world`

真正的“粗线三角形”是在 renderer 里，通过 `emit_segment(...)` 生成的。

这是把“文件语义”和“显示语义”分开的一个典型例子。

## 第 12 步：为什么还要为 path 扩 bounds

虽然 path 只是保留中心线，
但它的可见范围不能只按中心线算。

因为一条宽线真正占据的区域应该是：
- 中心线左右各扩半个线宽

所以导入层这里会做：

- `half_width`
- `bounds.pad(half_width)`

这一步很重要，因为后面的：
- fit to window
- 空间索引
- 可见裁剪

全都依赖 `bounds`。

如果 bounds 太小，path 可能会在边缘被误裁掉。

## 第 13 步：`ArrayRef` 为什么要单独算 offset 列表

`ArrayRef` 不是简单的“复制 N 次到固定偏移”。
它通常给的是：
- 原点
- 最后一列方向参考点
- 最后一行方向参考点
- rows / cols

所以代码里会先通过 `array_ref_offsets(...)` 把它还原成：

- 每个实例的实际位移向量

然后再逐个递归展开。

这一步很适合作为你理解 GDS 阵列语义的切入口。

## 第 14 步：导入完成后，最后返回什么

最后返回的是：

`SceneBundle::new(Vec<SceneView>)`

每个 `SceneView` 包含：
- `name`：cell 名
- `scene`：已经扁平化后的可渲染场景

也就是说：

**从 `app` 和 `ui` 的角度看，导入层的工作已经全部完成了。**

后面：
- UI 只负责切 view
- renderer 只负责看 `Scene`

## 你最该记住的 5 个点

### 1. 导入层的目标不是暴露 laykit，而是隔离 laykit

这层存在的最大价值就是：
- 把文件格式细节挡在外面
- 给 renderer 一个稳定的内部表示

### 2. 先找 root cells 再显示

这样 UI 才像一个真正的查看器，而不是把所有中间子单元都暴露出来。

### 3. 层级必须展开，局部坐标必须累加

否则顶层要么空，要么错。

### 4. path 保留中心线和世界线宽，是为了把显示策略留给 renderer

这是一种很典型的分层设计。

### 5. 当前支持少量关键图元，是有意控制复杂度

先把结构搭对，再扩图元类型，后面会顺很多。

## 建议你对照源码重点看这些函数

在 `src/io/mod.rs`：
- `load_layout_bundle`

在 `src/io/laykit_loader.rs`：
- `build_gds_scene_bundle`
- `build_oasis_scene_bundle`
- `collect_gds_shapes`
- `collect_oasis_shapes`
- `expand_gds_struct_ref`
- `expand_gds_array_ref`
- `expand_oasis_placement`
- `push_gds_path`
- `array_ref_offsets`

在 `src/scene/mod.rs`：
- `RectShape`
- `SceneBundle`

## 推荐你自己做的一个练习

你可以手推这个例子：

1. 定义一个 `leaf` cell，里面有一个 `(0, 0) ~ (10, 20)` 的 boundary
2. 定义一个 `top` cell，通过 `StructRef` 把 `leaf` 放到 `(100, 200)`
3. 问自己：
- root cell 是谁？
- `leaf` 会不会出现在 UI 下拉框？
- 最终 scene 的 bounds 应该是多少？
- shape 的 points 应该是多少？

如果这个你能自己推出来，当前导入链你就已经掌握得很扎实了。

## 当前关于 `STRANS / Placement` 的导入行为

当前 viewer 在导入层对实例变换的处理可以概括成：

- 先处理局部线性变换：镜像 / 旋转 / 等比缩放
- 再处理实例平移
- 最后把结果与父层级已经累计好的全局变换组合起来

对于学习来说，最值得记住的是：

**层级展开传下去的不是“一个数字偏移”，而是一整个局部坐标系。**

这也是为什么 `StructRef` 一旦带上 `STrans`，
导入层就必须升级到完整 2D 变换模型。

当前这版已经覆盖：
- GDS `StructRef.strans`
- GDS `ArrayRef.strans`
- OASIS `Placement.mirror / angle / magnification`

但也要注意，当前仍然是“最小查看器导入层”，
还没有把所有格式细枝末节都完整实现。
例如更复杂的 repetition / 更丰富的图元类型，后面还可以继续补。

## 当前关于 `OASIS repetition` 的导入行为

`repetition` 可以把一个图元或一个 placement 复制成多个实例。
当前导入层采用的策略是：

- 在导入阶段直接展开
- 每个 repetition 实例都变成一份真实 shape
- 继续沿用当前 viewer 的“扁平 Scene”模型

这样做的原因很实际：

1. renderer 现在只认识扁平 shape 列表
2. 我们已经有 tile cache / 空间索引 / bounds 裁剪，这些都更适合消费展开后的结果
3. 对学习来说，“先看见最终展开后的几何”比“保留压缩语义到后面再解释”更直观

### 当前已覆盖的 repetition

- `Matrix`：按二维阵列展开
- `Arbitrary`：按位移列表展开
- `Grid`：按单轴等距阵列展开

### 当前暂不支持的 repetition

- `ReusePrevious`

这里没有选择静默忽略，而是显式报错。
原因是它需要引用“前一个 repetition 记录”的状态，
而当前最小导入层还没有维护这类跨元素上下文。

对学习项目来说，**明确报错通常比悄悄画错更好**。

## 当前关于 `Polygon / OPath` 的导入行为

这一轮导入层新增了两个很值得学习的 OASIS 图元：

- `Polygon`
- `OPath`

它们和 `Rectangle` 最大的区别不是“形状更复杂”，
而是都采用了：

- `x/y` 作为基点
- `points` 作为相对点列

所以导入层不能直接把 `points` 当全局坐标使用，
而是要先做一步“局部点列还原”：

`local_point = base_xy + relative_point`

然后再进入我们前面已经建立好的统一链路：

- repetition 展开
- 实例变换组合
- 转内部 `RectShape`
- 交给 renderer

### `Polygon` 当前策略

- 还原出完整局部点列
- 按闭合轮廓处理
- repetition 会继续展开成多份 shape

### `OPath` 当前策略

- 还原出完整局部折线点列
- 保留世界坐标线宽
- magnification 会同步缩放线宽
- repetition 也会继续展开

### 这一轮暂时没有做什么

`OPath.extension_scheme` 目前还没有单独生成端点延伸几何。
也就是说：

- `Flush` / `HalfWidth` / `Custom` 现在不会改变折线端点的额外延伸形状
- viewer 当前仍然是按中心线 + 线宽 bounds 去近似显示

这是一个有意控制范围的取舍。
先把：
- 坐标还原
- 线宽传递
- repetition
- placement 变换

全部打通，比一开始就做很细的端帽语义更重要。
