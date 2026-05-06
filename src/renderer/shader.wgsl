// 这是项目早期的简单 shader：
// 输入顶点已经是 NDC，shader 只负责原样输出颜色。
//
// 当前主渲染路径已经迁移到 `scene.wgsl`，
// 这个文件暂时保留，方便你对比“CPU 全投影”与“CPU + shader 分工”的差异。
struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(input: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.position = vec4<f32>(input.position, 0.0, 1.0);
    out.color = input.color;
    return out;
}

@fragment
fn fs_main(input: VertexOut) -> @location(0) vec4<f32> {
    return input.color;
}
