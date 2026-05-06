//! 启动配置模块。
//!
//! 目前这里只放最简单、最稳定的常量：
//! - 默认加载的版图文件路径
//! - 初始窗口标题
//! - 初始窗口尺寸
//!
//! 为什么把这些值单独抽出来？
//! 因为它们经常会被你手动改，但又不应该散落在多个模块里。
//! 对学习阶段来说，这也是最方便你快速试不同文件、不同窗口大小的位置。

// pub const DEFAULT_LAYOUT_PATH: &str = "/Users/liuyang/Desktop/xiaoyao/gdsii/bend_euler_1.gds";
/// 默认打开的版图文件路径。
///
/// 这里用常量而不是文件对话框，是为了保持 demo 的结构最小化。
/// 后面如果你要加“打开文件”按钮，这里依然可以保留成回退路径。
pub const DEFAULT_LAYOUT_PATH: &str = "/Users/liuyang/Desktop/xiaoyao/gdsii/FF_SAR_ADC.gds"; // Pref Test Cell: SAR_ADC

/// 窗口标题。
pub const WINDOW_TITLE: &str = "flayout-wgpu";

/// 初始窗口宽度（逻辑像素）。
pub const INITIAL_WIDTH: u32 = 1440;

/// 初始窗口高度（逻辑像素）。
pub const INITIAL_HEIGHT: u32 = 960;
// 现在 UI 已经提供“Open layout...”按钮，这个默认路径更多是回退入口和启动时的初始文件。
