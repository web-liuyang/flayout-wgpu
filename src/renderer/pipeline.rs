//! 渲染 pipeline 定义。
//!
//! 这里负责的事情很单纯：
//! - 加载 shader
//! - 声明顶点格式
//! - 创建 uniform bind group layout
//! - 产出可供 renderer 直接使用的 render pipeline
//!
//! 为什么单独拆这个模块？
//! 因为 pipeline 属于“GPU 状态描述”，和场景数据、缓存策略不是一层概念。
//! 把它拆开以后，后面想改 shader、blend、顶点布局时会更容易定位。

use std::mem;

use bytemuck::{Pod, Zeroable};

use super::geometry::LineVertex;

/// 传给 shader 的 uniform。
///
/// 当前我们让 tile 顶点缓存保存的是“缩放后的逻辑屏幕坐标”，
/// 但还没有叠加相机平移和画布 origin。
/// 这两个量放进 uniform 后，平移时可以复用更多 tile buffer。
///
/// 这也是当前缓存体系一个非常重要的取舍：
/// - CPU 负责把 shape 变成“缩放后的局部屏幕坐标”
/// - shader 再负责补最后一层平移 / 视口归一化
///
/// 这样能显著提升 pan 场景下的 tile 复用率，
/// 因为平移不再要求 CPU 重新生成顶点。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SceneUniform {
    /// 需要在 shader 中叠加到顶点上的平移量。
    pub translation: [f32; 2],

    /// 当前视口大小（逻辑像素）。
    /// shader 依赖它把屏幕坐标换算成 NDC。
    pub viewport_size: [f32; 2],

    /// hatch 线间距。
    pub hatch_spacing: f32,

    /// hatch 线宽。
    pub hatch_width: f32,

    /// 交互期是否临时隐藏 fill，只保留 outline。
    ///
    /// 这里不再通过重建一套 Outline cache 来降级，
    /// 而是让 shader 直接在片元阶段跳过 fill。
    pub suppress_fill: f32,

    /// 当交互期复用上一帧稳定视图时，顶点位置需要补一个缩放比。
    pub position_scale: f32,

    /// 把整个 uniform 结构补到 48 字节，匹配 WGSL 侧期望的绑定大小。
    pub _padding: [f32; 4],
}

/// 场景渲染 pipeline 封装。
pub struct ScenePipeline {
    /// 当前 scene 使用的基础 render pipeline。
    render_pipeline: wgpu::RenderPipeline,
    /// scene uniform 对应的 bind group layout。
    bind_group_layout: wgpu::BindGroupLayout,
}

impl ScenePipeline {
    /// 创建场景渲染 pipeline。
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });

        // 只有一个 uniform 绑定，先保持最小模型，方便理解渲染数据是怎么进 shader 的。
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene-bind-group-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // 当前 pipeline 故意保持“只有一条基础 scene pipeline”的简单结构：
        // - 不按 layer 建多套 pipeline
        // - 不按 hatch style 建多套 pipeline
        // - 不引入额外的 depth / stencil 复杂度
        //
        // 这些语义尽量通过：
        // - 顶点属性
        // - uniform
        // - shader 分支
        // 来表达，便于缓存和 draw 调度保持统一。
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scene-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: mem::size_of::<LineVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x4,
                        2 => Float32,
                        3 => Float32
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // 目前 scene 与 egui 叠加的方式比较直接：
                    // scene 先画，egui 后画，所以 scene 这里启用普通 alpha blending 即可。
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Self {
            render_pipeline,
            bind_group_layout,
        }
    }

    /// 暴露底层 render pipeline 给 renderer 使用。
    pub fn render_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.render_pipeline
    }

    /// 为一份 uniform buffer 创建 bind group。
    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        uniform_buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        })
    }
}
