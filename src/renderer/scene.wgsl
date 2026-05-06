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
    translation: vec2<f32>,
    viewport_size: vec2<f32>,
    hatch_spacing: f32,
    hatch_width: f32,
    suppress_fill: f32,
    position_scale: f32,
    _padding: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> scene_uniform: SceneUniform;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) kind: f32,
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
    let distance_to_line = abs(fract(diagonal / spacing) - 0.5) * spacing;
    return distance_to_line <= width * 0.5;
}

fn dot_mask(screen: vec2<f32>, spacing: f32, width: f32) -> bool {
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

    if (scene_uniform.suppress_fill > 0.5) {
        discard;
    }

    let spacing = max(scene_uniform.hatch_spacing, 2.0);
    let width = clamp(scene_uniform.hatch_width, 0.5, spacing);
    let left_diagonal = input.screen.x + input.screen.y;
    let right_diagonal = input.screen.x - input.screen.y;
    let style = i32(round(input.hatch_style));

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

    discard;
}
