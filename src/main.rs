//! 可执行入口文件。
//!
//! 这个文件故意保持很薄：
//! - 日志初始化放在这里
//! - 应用生命周期启动放在这里
//! - 真正的业务逻辑都在 `app` 模块里
//!
//! 这样做的好处是：当你以后要改窗口创建、增加命令行参数、切换启动模式时，
//! 不会把渲染、UI、文件加载逻辑和入口混在一起。

use flayout_wgpu::app::ViewerApp;

fn main() {
    // 初始化 env_logger，方便后续在调试渲染、IO、交互时直接打日志。
    env_logger::init();

    // 桌面应用的主循环由 ViewerApp 统一管理。
    // 这里不直接 panic，而是把错误打印出来，便于定位启动阶段问题。
    if let Err(err) = ViewerApp::run() {
        eprintln!("application error: {err}");
    }
}
