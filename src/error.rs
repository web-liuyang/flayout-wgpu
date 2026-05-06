//! 应用统一错误类型。
//!
//! 这个项目目前既有 IO 错误，也有解析错误、渲染初始化错误。
//! 如果每层都随手返回字符串，后面会很难判断错误来源。
//! 因此这里统一收口成 `AppError`，让上层 UI 可以用一致方式展示错误。

use thiserror::Error;

/// 应用级错误。
///
/// 这里不追求“错误类型非常细”，而是优先保证：
/// - UI 能显示足够明确的信息
/// - 调用链能把错误来源传递回来
#[derive(Debug, Error)]
pub enum AppError {
    /// 路径为空，说明用户还没有配置任何版图文件。
    #[error("layout path is not configured")]
    MissingPath,

    /// 路径存在问题，通常是文件被移动、重命名，或者写错了。
    #[error("layout file does not exist: {0}")]
    MissingFile(String),

    /// 当前 demo 只支持我们已经接好的版图格式。
    #[error("unsupported layout format: {0}")]
    UnsupportedFormat(String),

    /// 解析库成功打开文件，但内容本身无法被正确解析。
    #[error("layout parse failed: {0}")]
    Parse(String),

    /// GPU / surface / adapter 等渲染初始化阶段错误。
    #[error("render setup failed: {0}")]
    Render(String),
}
