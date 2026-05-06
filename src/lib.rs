//! `flayout-wgpu` 的库入口。
//!
//! 这个项目目前是一个“可学习、可继续扩展”的版图查看器 demo，模块划分大致如下：
//! - `app`：顶层应用编排，连接 winit / egui / renderer / io
//! - `camera`：2D 相机，负责平移、缩放和 fit 行为
//! - `config`：默认路径、窗口尺寸等启动配置
//! - `error`：统一错误类型
//! - `io`：版图文件加载与解析适配层
//! - `perf`：帧时间与 FPS 统计
//! - `renderer`：GPU 渲染、几何生成、缓存与可视化优化
//! - `scene`：内部统一场景模型
//! - `ui`：左侧面板和主画布交互
//!
//! 这个层次的核心目标是“隔离变化”：
//! - 将来换解析库，尽量只动 `io`
//! - 将来换渲染策略，尽量只动 `renderer`
//! - 将来扩 UI，尽量只动 `ui` 和 `app`

pub mod app;
pub mod camera;
pub mod config;
pub mod error;
pub mod io;
pub mod perf;
pub mod persistence;
pub mod renderer;
pub mod scene;
pub mod ui;
