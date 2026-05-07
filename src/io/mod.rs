//! 版图文件加载入口。
//!
//! 这个模块的目标不是直接暴露 `laykit` 的类型，
//! 而是对上层提供统一接口：给我一个路径，我返回我们的内部 `SceneBundle`。
//!
//! 这样做的好处：
//! - 将来如果换解析库，尽量只改 `io`
//! - 上层 `app / renderer / ui` 不需要关心 GDS/OASIS 细节

mod laykit_loader;

use std::path::Path;

use crate::error::AppError;
use crate::layout::LayoutBundle;
use crate::scene::{Scene, SceneBundle};

/// 按路径加载版图，并返回可供 UI 切换的 `SceneBundle`。
///
/// `SceneBundle` 的存在，是因为一个版图文件可能包含多个 root cell，
/// 查看器需要给用户一个 cell 视图列表来切换。
pub fn load_layout_bundle(path: &str) -> Result<SceneBundle, AppError> {
    if path.trim().is_empty() {
        return Err(AppError::MissingPath);
    }

    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(AppError::MissingFile(path.to_string()));
    }

    let ext = path_ref
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    match ext.as_deref() {
        Some("gds") => laykit_loader::load_gds(path_ref),
        Some("oas") => laykit_loader::load_oasis(path_ref),
        _ => Err(AppError::UnsupportedFormat(path.to_string())),
    }
}

/// 按路径加载“分层 GDS bundle”。
///
/// 这是 Task 2 期间保留的一个临时桥接入口：
/// - 旧 viewer 仍然继续消费扁平 `SceneBundle`
/// - 新的层次化内存架构先从 GDS loader 这端开始落地
///
/// 后续 Task 3 / 4 会把 app 和 renderer 逐步接到这个分层模型上。
pub fn load_layout_hierarchy_bundle(path: &str) -> Result<LayoutBundle, AppError> {
    if path.trim().is_empty() {
        return Err(AppError::MissingPath);
    }

    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(AppError::MissingFile(path.to_string()));
    }

    let ext = path_ref
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    match ext.as_deref() {
        Some("gds") => laykit_loader::load_gds_layout_bundle(path_ref),
        Some("oas") => Err(AppError::UnsupportedFormat(format!(
            "hierarchical OASIS loading is not implemented yet: {path}"
        ))),
        _ => Err(AppError::UnsupportedFormat(path.to_string())),
    }
}

/// 加载当前 bundle 中默认选中的场景。
///
/// 这个函数适合测试或非常简单的调用场景，
/// 但真实 viewer 里更常用的是 `load_layout_bundle`，因为它保留了 cell 选择能力。
pub fn load_layout_scene(path: &str) -> Result<Scene, AppError> {
    Ok(load_layout_bundle(path)?
        .current_scene()
        .cloned()
        .unwrap_or_else(Scene::empty))
}
