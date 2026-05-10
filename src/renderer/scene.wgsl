// 当前真正用于主画布渲染的 shader。
//
// 设计思路：
// 1. CPU / tile cache 先生成“缩放后的逻辑屏幕坐标”顶点
// 2. shader 再叠加 translation（相机平移 + 画布偏移）
// 3. 对 hatch 填充三角形，片元阶段按屏幕坐标生成图案
// 4. 最后根据 viewport size 转成 NDC
//
// 这样做的好处是：
// - 平移时不需要重建 tile 顶点
// - hatch 的疏密由屏幕像素控制，缩放时观感更稳定
// - 后面扩更多 preset 时不需要改 CPU 几何路径
struct SceneUniform {
    // 由 app/renderer 在 CPU 侧计算好的平移量：
    // 相机 pan + 画布 origin。
    translation: vec2<f32>,
    // 当前逻辑视口尺寸，用来把屏幕坐标换算成 NDC。
    viewport_size: vec2<f32>,
    // hatch 图案的屏幕空间间距。
    hatch_spacing: f32,
    // hatch 图案的屏幕空间线宽。
    hatch_width: f32,
    // 交互降级期间，fill 顶点在片元阶段直接丢弃。
    //
    // 这样能临时把 Hatch/HatchOutline 退化成更轻的结果，
    // 又不必在 CPU 侧重建一整套新的 Outline tile cache。
    suppress_fill: f32,
    // 当前顶点相对 tile cache zoom 基准需要补乘的缩放比。
    //
    // 这是 zoom bucket 复用的关键：
    // CPU 不必为每个细小 zoom 变化重建顶点，
    // shader 只需要再乘一个比例，就能让画面先跟着当前 zoom 走。
    position_scale: f32,
    _padding: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> scene_uniform: SceneUniform;

struct VertexInput {
    // CPU 传进来的“缩放后逻辑屏幕坐标”，但还没加 translation。
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    // 0 = outline，1 = hatch fill
    @location(2) kind: f32,
    // hatch preset 编码，只有 fill 顶点真正会用到。
    @location(3) hatch_style: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) kind: f32,
    @location(2) screen: vec2<f32>,
    @location(3) hatch_style: f32,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;

    // 顶点在这里才补最后一层“当前视图语义”：
    // - position_scale：解决 zoom bucket 复用 / 交互冻结跟手
    // - translation：解决 pan + canvas origin
    let screen = input.position * scene_uniform.position_scale + scene_uniform.translation;
    let ndc = vec2<f32>(
        (screen.x / scene_uniform.viewport_size.x) * 2.0 - 1.0,
        1.0 - (screen.y / scene_uniform.viewport_size.y) * 2.0,
    );

    output.position = vec4<f32>(ndc, 0.0, 1.0);
    output.color = input.color;
    output.kind = input.kind;
    output.screen = screen;
    output.hatch_style = input.hatch_style;
    return output;
}

fn diagonal_mask(diagonal: f32, spacing: f32, width: f32) -> bool {
    // 这里不去“显式画一条条 hatch 线段”，
    // 而是对 fill 三角形覆盖到的每个像素，在片元阶段判断：
    // 这个像素离最近一条理论 hatch 线够不够近。
    let distance_to_line = abs(fract(diagonal / spacing) - 0.5) * spacing;
    return distance_to_line <= width * 0.5;
}

fn dot_mask(screen: vec2<f32>, spacing: f32, width: f32) -> bool {
    // 点阵模式同理：fill 三角形只负责给出“这个区域可以被填”，
    // 真正要不要着色，由片元阶段按屏幕网格判定。
    let cell = floor(screen / spacing);
    let center = (cell + vec2<f32>(0.5, 0.5)) * spacing;
    let radius = max(width * 0.5, 0.75);
    return distance(screen, center) <= radius;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // kind < 0.5 代表普通 outline，直接输出。
    if (input.kind < 0.5) {
        return input.color;
    }

    // 交互降级时，fill 顶点统一在这里丢弃。
    // 这比切 CPU 模式更省，因为 tile cache 仍然可以复用。
    if (scene_uniform.suppress_fill > 0.5) {
        discard;
    }

    let spacing = max(scene_uniform.hatch_spacing, 2.0);
    let width = clamp(scene_uniform.hatch_width, 0.5, spacing);
    let left_diagonal = input.screen.x + input.screen.y;
    let right_diagonal = input.screen.x - input.screen.y;
    let style = i32(round(input.hatch_style));

    // 同一批 fill 三角形，最终呈现成什么图案，完全由 hatch_style 决定。
    // 这也是为什么 CPU 侧不需要为不同 hatch preset 分别生成不同几何。
    var visible = false;
    if (style == 1) {
        visible = diagonal_mask(right_diagonal, spacing, width);
    } else if (style == 2) {
        visible =
            diagonal_mask(left_diagonal, spacing, width) ||
            diagonal_mask(right_diagonal, spacing, width);
    } else if (style == 3) {
        visible = dot_mask(input.screen, spacing, width);
    } else {
        visible = diagonal_mask(left_diagonal, spacing, width);
    }

    if (visible) {
        return input.color;
    }

    // 不在 hatch 线/点上的像素直接丢弃，保留背景。
    discard;
}
