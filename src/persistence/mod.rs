//! Viewer 配置持久化层。
//!
//! 这层只负责两件事：
//! - 定义一份可序列化的 viewer 配置模型
//! - 负责 JSON 文件的读写和恢复辅助逻辑
//!
//! 它刻意不直接依赖 `renderer` 的内部缓存结构，
//! 也不直接执行 UI 或窗口操作。这样 `app` 仍然是状态编排中心，
//! 而持久化层只是一个干净的“状态快照与恢复工具”。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    renderer::geometry::{ClosedShapeDrawMode, HatchStylePreset},
    scene::{LayerId, Scene, SceneBundle},
};

/// 持久化层自己的错误类型。
#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("viewer config directory is not available on this platform")]
    ConfigDirUnavailable,

    #[error("viewer config I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("viewer config JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedLayerId {
    pub layer: u32,
    pub datatype: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedClosedShapeDrawMode {
    Outline,
    Hatch,
    HatchOutline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedHatchStylePreset {
    LeftDiagonal,
    RightDiagonal,
    Cross,
    Dots,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedLayerDrawMode {
    pub layer: PersistedLayerId,
    pub mode: PersistedClosedShapeDrawMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedLayerHatchStyle {
    pub layer: PersistedLayerId,
    pub style: PersistedHatchStylePreset,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PersistedCamera {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewerConfig {
    /// 最近一次成功打开的版图文件路径。
    pub layout_path: String,
    /// 当前选中的 root cell / 视图名称。
    pub selected_view_name: Option<String>,
    /// 当前相机状态。
    pub camera: PersistedCamera,
    /// 当前层级过滤范围的下界。
    ///
    /// 这里做成 `Option` 而不是裸 `u32`，是为了兼容旧配置文件：
    /// - 老文件没有这个字段时，不要误恢复成 `0`
    /// - 而是让 app 按当前 scene 复杂度重新计算默认范围
    #[serde(default)]
    pub min_hierarchy_level: Option<u32>,
    /// 当前层级过滤范围的上界。
    #[serde(default)]
    pub max_hierarchy_level: Option<u32>,
    /// 当前被隐藏的 layer 列表。
    pub hidden_layers: Vec<PersistedLayerId>,
    /// 每层对闭合图形显示模式的覆盖。
    pub layer_draw_modes: Vec<PersistedLayerDrawMode>,
    /// 每层 hatch 预设。
    #[serde(default)]
    pub layer_hatch_styles: Vec<PersistedLayerHatchStyle>,
    /// 全局默认闭合图形显示模式。
    pub draw_mode: PersistedClosedShapeDrawMode,
    /// 全局 hatch 间距。
    pub hatch_spacing: f32,
    /// 全局 hatch 线宽。
    pub hatch_width: f32,
    /// tile grid 粒度。
    pub tile_grid_divisions: u32,
    /// tile cache 容量上限。
    pub tile_cache_capacity: usize,
    /// 全局 bypass 渐进式渲染的阈值。
    pub progressive_bypass_threshold: usize,
    /// 当前活动 layer 走“一帧补完”的 entry 数阈值。
    #[serde(default = "default_layer_bypass_entry_threshold")]
    pub layer_bypass_entry_threshold: usize,
    /// 当前活动 layer 走“一帧补完”的工作量阈值。
    #[serde(default = "default_layer_bypass_work_threshold")]
    pub layer_bypass_work_threshold: usize,
}

impl PersistedLayerId {
    pub fn from_runtime(layer: LayerId) -> Self {
        Self {
            layer: layer.layer,
            datatype: layer.datatype,
        }
    }

    pub fn to_runtime(self) -> LayerId {
        LayerId {
            layer: self.layer,
            datatype: self.datatype,
        }
    }
}

impl PersistedClosedShapeDrawMode {
    pub fn from_runtime(mode: ClosedShapeDrawMode) -> Self {
        match mode {
            ClosedShapeDrawMode::Outline => Self::Outline,
            ClosedShapeDrawMode::Hatch => Self::Hatch,
            ClosedShapeDrawMode::HatchOutline => Self::HatchOutline,
        }
    }

    pub fn to_runtime(self) -> ClosedShapeDrawMode {
        match self {
            Self::Outline => ClosedShapeDrawMode::Outline,
            Self::Hatch => ClosedShapeDrawMode::Hatch,
            Self::HatchOutline => ClosedShapeDrawMode::HatchOutline,
        }
    }
}

fn default_layer_bypass_entry_threshold() -> usize {
    8
}

fn default_layer_bypass_work_threshold() -> usize {
    128
}

impl PersistedHatchStylePreset {
    pub fn from_runtime(style: HatchStylePreset) -> Self {
        match style {
            HatchStylePreset::LeftDiagonal => Self::LeftDiagonal,
            HatchStylePreset::RightDiagonal => Self::RightDiagonal,
            HatchStylePreset::Cross => Self::Cross,
            HatchStylePreset::Dots => Self::Dots,
        }
    }

    pub fn to_runtime(self) -> HatchStylePreset {
        match self {
            Self::LeftDiagonal => HatchStylePreset::LeftDiagonal,
            Self::RightDiagonal => HatchStylePreset::RightDiagonal,
            Self::Cross => HatchStylePreset::Cross,
            Self::Dots => HatchStylePreset::Dots,
        }
    }
}

pub fn viewer_config_path() -> Result<PathBuf, PersistenceError> {
    let dirs = ProjectDirs::from("com", "webliuyang", "flayout-wgpu")
        .ok_or(PersistenceError::ConfigDirUnavailable)?;
    Ok(dirs.config_dir().join("viewer-config.json"))
}

pub fn load_viewer_config() -> Result<Option<ViewerConfig>, PersistenceError> {
    let path = viewer_config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(load_viewer_config_from_path(&path)?))
}

/// 从指定路径读取 viewer 配置。
///
/// 单元测试会更常用这个入口，因为它允许把 JSON 写到临时目录里再回读。
pub fn load_viewer_config_from_path(path: &Path) -> Result<ViewerConfig, PersistenceError> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// 保存 viewer 配置到系统默认配置路径。
pub fn save_viewer_config(config: &ViewerConfig) -> Result<(), PersistenceError> {
    let path = viewer_config_path()?;
    save_viewer_config_to_path(&path, config)
}

/// 保存 viewer 配置到指定路径。
///
/// app 正常运行时用默认路径；测试时用这个入口避免污染真实用户配置。
pub fn save_viewer_config_to_path(
    path: &Path,
    config: &ViewerConfig,
) -> Result<(), PersistenceError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    fs::write(path, text)?;
    Ok(())
}

/// 根据保存的 view 名称恢复当前 bundle 的选中索引。
pub fn resolve_saved_view_index(bundle: &SceneBundle, saved_name: Option<&str>) -> Option<usize> {
    let saved_name = saved_name?;
    bundle
        .views()
        .iter()
        .position(|view| view.name == saved_name)
}

/// 只恢复“当前 scene 里仍然存在”的隐藏 layer。
pub fn filter_hidden_layers_for_scene(config: &ViewerConfig, scene: &Scene) -> BTreeSet<LayerId> {
    let existing: BTreeSet<LayerId> = scene.layer_ids().into_iter().collect();
    config
        .hidden_layers
        .iter()
        .map(|layer| layer.to_runtime())
        .filter(|layer| existing.contains(layer))
        .collect()
}

/// 只恢复“当前 scene 里仍然存在”的 per-layer draw mode 覆盖。
pub fn filter_layer_draw_modes_for_scene(
    config: &ViewerConfig,
    scene: &Scene,
) -> BTreeMap<LayerId, ClosedShapeDrawMode> {
    let existing: BTreeSet<LayerId> = scene.layer_ids().into_iter().collect();
    config
        .layer_draw_modes
        .iter()
        .filter_map(|entry| {
            let layer = entry.layer.to_runtime();
            existing
                .contains(&layer)
                .then_some((layer, entry.mode.to_runtime()))
        })
        .collect()
}

/// 只恢复“当前 scene 里仍然存在”的 per-layer hatch preset。
pub fn filter_layer_hatch_styles_for_scene(
    config: &ViewerConfig,
    scene: &Scene,
) -> BTreeMap<LayerId, HatchStylePreset> {
    let existing: BTreeSet<LayerId> = scene.layer_ids().into_iter().collect();
    config
        .layer_hatch_styles
        .iter()
        .filter_map(|entry| {
            let layer = entry.layer.to_runtime();
            existing
                .contains(&layer)
                .then_some((layer, entry.style.to_runtime()))
        })
        .collect()
}
