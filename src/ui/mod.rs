//! UI 模块。
//!
//! 这个项目的 UI 分成两块：
//! - 左侧面板：状态、统计、layer 控制、调试参数
//! - 中间画布：只负责接收交互和显示 GPU 主画布边界
//!
//! 一个很重要的设计点是：
//! `ui` 并不直接修改 `scene` 或 `renderer`，
//! 它只产出 `UiAction`，再交给 `app` 统一处理。
//! 这样做可以让交互流更清楚，也更容易测试和扩展。

use std::collections::{BTreeMap, BTreeSet};

use egui::{Color32, ScrollArea, Sense, Stroke, Vec2 as EguiVec2};
use glam::Vec2;

use crate::{
    app::LoadState,
    camera::Camera2D,
    perf::{FrameStats, MetricHistory, RenderStatsHistory},
    renderer::{
        RenderDebugStats,
        geometry::{
            ClosedShapeDrawMode, HatchParams, HatchStylePreset, MAX_HATCH_SPACING,
            MAX_HATCH_WIDTH, MAX_TILE_GRID_DIVISIONS, MIN_HATCH_SPACING,
            MIN_HATCH_WIDTH, MIN_TILE_GRID_DIVISIONS,
        },
        MAX_LAYER_BYPASS_ENTRY_THRESHOLD, MAX_LAYER_BYPASS_WORK_THRESHOLD,
        MAX_PROGRESSIVE_BYPASS_THRESHOLD, MAX_TILE_CACHE_CAPACITY,
        MIN_LAYER_BYPASS_ENTRY_THRESHOLD, MIN_LAYER_BYPASS_WORK_THRESHOLD,
        MIN_PROGRESSIVE_BYPASS_THRESHOLD, MIN_TILE_CACHE_CAPACITY,
    },
    scene::{LayerId, Scene, SceneBundle},
};

/// UI 一帧内收集到的用户意图。
///
/// 这里不用回调直接改外部状态，而是把“发生了什么”打包成一个动作对象。
/// 这样 `app` 作为顶层调度者，就能按固定顺序处理：
/// 先切 view，再改 layer，再改 camera，再触发 render。
#[derive(Debug, Clone, Default)]
pub struct UiAction {
    /// 选择新的 root cell / scene view。
    pub selected_view: Option<usize>,
    /// 请求重新 fit 当前场景。
    pub request_fit: bool,
    /// 主画布左上角在 egui 逻辑坐标中的位置。
    pub canvas_origin: Vec2,
    /// 主画布尺寸。
    pub canvas_size: Vec2,
    /// 本帧累积的平移增量。
    pub pan_delta: Vec2,
    /// 缩放因子与缩放中心点。
    pub zoom: Option<(f32, Vec2)>,
    /// 更新后的隐藏 layer 集合。
    pub hidden_layers: Option<BTreeSet<LayerId>>,
    /// 更新后的 per-layer draw mode 覆盖。
    pub layer_draw_modes: Option<BTreeMap<LayerId, ClosedShapeDrawMode>>,
    /// 更新后的 per-layer hatch preset 覆盖。
    pub layer_hatch_styles: Option<BTreeMap<LayerId, HatchStylePreset>>,
    /// 更新后的 tile grid 粒度。
    pub tile_grid_divisions: Option<u32>,
    /// 更新后的全局 draw mode。
    pub draw_mode: Option<ClosedShapeDrawMode>,
    /// 更新后的 hatch 参数。
    pub hatch_params: Option<HatchParams>,
    /// 更新后的最小层级深度。
    pub min_hierarchy_level: Option<u32>,
    /// 更新后的最大层级深度。
    pub max_hierarchy_level: Option<u32>,
    /// 更新后的 tile cache 容量。
    pub tile_cache_capacity: Option<usize>,
    /// 更新后的全局 progressive bypass 阈值。
    pub progressive_bypass_threshold: Option<usize>,
    /// 更新后的 layer entry bypass 阈值。
    pub layer_bypass_entry_threshold: Option<usize>,
    /// 更新后的 layer work bypass 阈值。
    pub layer_bypass_work_threshold: Option<usize>,
    /// 请求打开新文件。
    pub request_open_file: bool,
    /// 请求重新加载当前文件。
    pub request_reload_layout: bool,
}

/// 绘制整套 UI，并收集这一帧的交互动作。
pub fn draw_ui(
    ctx: &egui::Context,
    layout_path: &str,
    load_state: &LoadState,
    scene_bundle: &SceneBundle,
    scene: &Scene,
    full_scene_max_hierarchy_level: u32,
    camera: &Camera2D,
    hidden_layers: &BTreeSet<LayerId>,
    layer_draw_modes: &BTreeMap<LayerId, ClosedShapeDrawMode>,
    layer_hatch_styles: &BTreeMap<LayerId, HatchStylePreset>,
    frame_stats: &FrameStats,
    render_debug_stats: &RenderDebugStats,
    render_stats_history: &RenderStatsHistory,
    tile_grid_divisions: u32,
    draw_mode: ClosedShapeDrawMode,
    hatch_params: HatchParams,
    min_hierarchy_level: u32,
    max_hierarchy_level: u32,
    tile_cache_capacity: usize,
    progressive_bypass_threshold: usize,
    layer_bypass_entry_threshold: usize,
    layer_bypass_work_threshold: usize,
) -> UiAction {
    // 这里先把外部状态拷贝成“可编辑副本”，
    // 整个 UI 绘制过程中都只改这些副本。
    // 等到函数末尾，再把真正变化过的字段回填到 `UiAction`，
    // 这样 app 就能按统一顺序处理状态变更。
    let mut action = UiAction::default();
    let mut selected_view = scene_bundle.selected_index();
    let mut next_hidden_layers = hidden_layers.clone();
    let mut next_layer_draw_modes = layer_draw_modes.clone();
    let mut next_layer_hatch_styles = layer_hatch_styles.clone();
    let mut next_tile_grid_divisions = tile_grid_divisions;
    let mut next_draw_mode = draw_mode;
    let mut next_hatch_spacing = hatch_params.spacing.clamp(MIN_HATCH_SPACING, MAX_HATCH_SPACING);
    let mut next_hatch_width = hatch_params.width.clamp(MIN_HATCH_WIDTH, MAX_HATCH_WIDTH);
    let scene_max_hierarchy_level = full_scene_max_hierarchy_level;
    let mut next_min_hierarchy_level = min_hierarchy_level.min(scene_max_hierarchy_level);
    let mut next_max_hierarchy_level = max_hierarchy_level.min(scene_max_hierarchy_level);
    if next_min_hierarchy_level > next_max_hierarchy_level {
        next_min_hierarchy_level = next_max_hierarchy_level;
    }
    let mut next_tile_cache_capacity = tile_cache_capacity.clamp(MIN_TILE_CACHE_CAPACITY, MAX_TILE_CACHE_CAPACITY) as u32;
    let mut next_progressive_bypass_threshold = progressive_bypass_threshold
        .clamp(MIN_PROGRESSIVE_BYPASS_THRESHOLD, MAX_PROGRESSIVE_BYPASS_THRESHOLD) as u32;
    let mut next_layer_bypass_entry_threshold = layer_bypass_entry_threshold
        .clamp(MIN_LAYER_BYPASS_ENTRY_THRESHOLD, MAX_LAYER_BYPASS_ENTRY_THRESHOLD) as u32;
    let mut next_layer_bypass_work_threshold = layer_bypass_work_threshold
        .clamp(MIN_LAYER_BYPASS_WORK_THRESHOLD, MAX_LAYER_BYPASS_WORK_THRESHOLD) as u32;
    let mut layers_changed = false;
    let mut layer_modes_changed = false;
    let mut layer_hatch_styles_changed = false;

    egui::SidePanel::left("left-panel")
        .resizable(true)
        .default_width(280.0)
        .show(ctx, |ui| {
            // 整个左侧面板已经逐渐变成一个完整工具栏：
            // 顶部有文件/视图信息，中段有 layer 控制，下面还有性能与调试区。
            // 所以不能只让 `Layers` 自己滚动，还需要让整栏在高度不足时整体可滚。
            ScrollArea::vertical()
                .id_salt("left-panel-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.heading("flayout-wgpu");
                    ui.separator();
                    ui.label("Layout file");
                    ui.monospace(layout_path);
                    ui.horizontal(|ui| {
                        if ui.button("Open layout...").clicked() {
                            action.request_open_file = true;
                        }
                        if ui.button("Reload").clicked() {
                            action.request_reload_layout = true;
                        }
                    });
                    ui.separator();
                    ui.label("Load status");
                    ui.label(load_state.summary());

                    // 一个文件可能包含多个 root cell，
                    // 所以这里给用户一个视图切换入口。
                    if !scene_bundle.views().is_empty() {
                        ui.separator();
                        ui.label("Cell view");
                        egui::ComboBox::from_id_salt("cell-view-combo")
                            .selected_text(
                                scene_bundle
                                    .current_view()
                                    .map(|view| view.name.as_str())
                                    .unwrap_or("<none>"),
                            )
                            .show_ui(ui, |ui| {
                                for (index, view) in scene_bundle.views().iter().enumerate() {
                                    ui.selectable_value(&mut selected_view, index, &view.name);
                                }
                            });
                    }

                    ui.separator();
                    if ui.button("Fit to window").clicked() {
                        action.request_fit = true;
                    }

                    // 只有 scene 真有多层级时才显示 level range 控件。
                    // 对单层级版图来说，这块 UI 只会增加干扰，不会带来实际价值。
                    if scene_max_hierarchy_level > 0 {
                        ui.separator();
                        ui.label("Hierarchy levels");
                        if ui
                            .add(
                                egui::Slider::new(
                                    &mut next_min_hierarchy_level,
                                    0..=scene_max_hierarchy_level,
                                )
                                .text("Min level"),
                            )
                            .changed()
                        {
                            next_max_hierarchy_level =
                                next_max_hierarchy_level.max(next_min_hierarchy_level);
                        }
                        if ui
                            .add(
                                egui::Slider::new(
                                    &mut next_max_hierarchy_level,
                                    0..=scene_max_hierarchy_level,
                                )
                                .text("Max level"),
                            )
                            .changed()
                        {
                            next_min_hierarchy_level =
                                next_min_hierarchy_level.min(next_max_hierarchy_level);
                        }
                        ui.label(format!(
                            "Showing levels {}..={} / max {}",
                            next_min_hierarchy_level, next_max_hierarchy_level, scene_max_hierarchy_level
                        ));
                    }

                    // 这里保留两层滚动：
                    // 1. 整个左栏可以整体滚动，解决面板内容总高度过高的问题
                    // 2. `Layers` 自己仍然有局部滚动，避免 layer 特别多时独占整栏高度
                    let layers = scene.layer_ids();
                    if !layers.is_empty() {
                        ui.separator();
                        ui.label("Layers");
                        ScrollArea::vertical()
                            .id_salt("layer-scroll")
                            .max_height(180.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for layer in layers {
                                    // 这里改成“两行一组”而不是把所有控件硬塞进一行：
                                    // layer 名称长度不可控，如果继续横向平铺，
                                    // 下拉框很容易在窄侧栏里互相挤压，交互体验会明显变差。
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            let mut visible = !next_hidden_layers.contains(&layer);
                                            if ui
                                                .checkbox(
                                                    &mut visible,
                                                    format!("L{} / D{}", layer.layer, layer.datatype),
                                                )
                                                .changed()
                                            {
                                                layers_changed = true;
                                                if visible {
                                                    next_hidden_layers.remove(&layer);
                                                } else {
                                                    next_hidden_layers.insert(layer);
                                                }
                                            }
                                        });

                                        ui.horizontal(|ui| {
                                            let mut layer_mode = next_layer_draw_modes
                                                .get(&layer)
                                                .copied()
                                                .unwrap_or(draw_mode);
                                            egui::ComboBox::from_id_salt(("layer-mode", layer.layer, layer.datatype))
                                                .selected_text(closed_shape_draw_mode_label(layer_mode))
                                                .width(100.0)
                                                .show_ui(ui, |ui| {
                                                    ui.selectable_value(&mut layer_mode, ClosedShapeDrawMode::Outline, "Outline");
                                                    ui.selectable_value(&mut layer_mode, ClosedShapeDrawMode::Hatch, "Hatch");
                                                    ui.selectable_value(&mut layer_mode, ClosedShapeDrawMode::HatchOutline, "Hatch+Outline");
                                                });
                                            if layer_mode
                                                != next_layer_draw_modes
                                                    .get(&layer)
                                                    .copied()
                                                    .unwrap_or(draw_mode)
                                            {
                                                layer_modes_changed = true;
                                                if layer_mode == draw_mode {
                                                    next_layer_draw_modes.remove(&layer);
                                                } else {
                                                    next_layer_draw_modes.insert(layer, layer_mode);
                                                }
                                            }

                                            let mut hatch_style = next_layer_hatch_styles
                                                .get(&layer)
                                                .copied()
                                                .unwrap_or(HatchStylePreset::LeftDiagonal);
                                            // hatch preset 是“每层的记忆配置”，而不是始终正在生效的画法。
                                            // 当当前层被切到 Outline 时，我们把 preset 下拉框置灰，
                                            // 既保留这层未来切回 Hatch 时的图案选择，也避免让用户误以为 preset 仍在当前画面里起作用。
                                            ui.add_enabled_ui(layer_mode != ClosedShapeDrawMode::Outline, |ui| {
                                                egui::ComboBox::from_id_salt(("layer-hatch-style", layer.layer, layer.datatype))
                                                    .selected_text(hatch_style_label(hatch_style))
                                                    .width(118.0)
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(&mut hatch_style, HatchStylePreset::LeftDiagonal, hatch_style_label(HatchStylePreset::LeftDiagonal));
                                                        ui.selectable_value(&mut hatch_style, HatchStylePreset::RightDiagonal, hatch_style_label(HatchStylePreset::RightDiagonal));
                                                        ui.selectable_value(&mut hatch_style, HatchStylePreset::Cross, hatch_style_label(HatchStylePreset::Cross));
                                                        ui.selectable_value(&mut hatch_style, HatchStylePreset::Dots, hatch_style_label(HatchStylePreset::Dots));
                                                    });
                                            });
                                            if hatch_style
                                                != next_layer_hatch_styles
                                                    .get(&layer)
                                                    .copied()
                                                    .unwrap_or(HatchStylePreset::LeftDiagonal)
                                            {
                                                layer_hatch_styles_changed = true;
                                                next_layer_hatch_styles.insert(layer, hatch_style);
                                            }
                                        });
                                    });
                                    ui.add_space(4.0);
                                }
                            });
                    }

                    ui.separator();
                    ui.label("Performance");
                    ui.label(format!("FPS: {:.1}", frame_stats.fps()));
                    ui.label(format!("Frame: {:.2} ms", frame_stats.frame_time_ms()));

                    // 这一组调试指标主要是为了让性能优化“有数据可看”，
                    // 而不是只凭感觉猜快慢。
                    ui.separator();
                    ui.label("Renderer");
                    ui.label(format!("Total shapes: {}", render_debug_stats.total_shapes));
                    ui.label(format!(
                        "Candidates: {}",
                        render_debug_stats.candidate_shapes
                    ));
                    ui.label(format!(
                        "Visible shapes: {}",
                        render_debug_stats.visible_shapes
                    ));
                    ui.label(format!("Bucket hits: {}", render_debug_stats.bucket_hits));
                    ui.label(format!("Vertices: {}", render_debug_stats.vertex_count));
                    ui.label(format!("Draw calls: {}", render_debug_stats.draw_calls));
                    egui::ComboBox::from_id_salt("draw-mode-combo")
                        .selected_text(match next_draw_mode {
                            ClosedShapeDrawMode::Outline => "Closed shapes: Outline",
                            ClosedShapeDrawMode::Hatch => "Closed shapes: Hatch",
                            ClosedShapeDrawMode::HatchOutline => "Closed shapes: Hatch + Outline",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut next_draw_mode, ClosedShapeDrawMode::Outline, "Closed shapes: Outline");
                            ui.selectable_value(&mut next_draw_mode, ClosedShapeDrawMode::Hatch, "Closed shapes: Hatch");
                            ui.selectable_value(&mut next_draw_mode, ClosedShapeDrawMode::HatchOutline, "Closed shapes: Hatch + Outline");
                        });
                    if next_draw_mode != draw_mode {
                        action.draw_mode = Some(next_draw_mode);
                    }
                    let mut hatch_changed = false;
                    if ui
                        .add(
                            egui::Slider::new(&mut next_hatch_spacing, MIN_HATCH_SPACING..=MAX_HATCH_SPACING)
                                .text("Hatch spacing"),
                        )
                        .changed()
                    {
                        hatch_changed = true;
                    }
                    if ui
                        .add(
                            egui::Slider::new(&mut next_hatch_width, MIN_HATCH_WIDTH..=MAX_HATCH_WIDTH)
                                .text("Hatch width"),
                        )
                        .changed()
                    {
                        hatch_changed = true;
                    }
                    if hatch_changed {
                        action.hatch_params = Some(HatchParams {
                            spacing: next_hatch_spacing,
                            width: next_hatch_width,
                        });
                    }
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut next_tile_grid_divisions,
                                MIN_TILE_GRID_DIVISIONS..=MAX_TILE_GRID_DIVISIONS,
                            )
                            .text("Tile grid"),
                        )
                        .changed()
                    {
                        action.tile_grid_divisions = Some(next_tile_grid_divisions);
                    }
                    ui.label(format!(
                        "Visible tiles: {}",
                        render_debug_stats.visible_tiles
                    ));
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut next_tile_cache_capacity,
                                MIN_TILE_CACHE_CAPACITY as u32..=MAX_TILE_CACHE_CAPACITY as u32,
                            )
                            .text("Tile cache"),
                        )
                        .changed()
                    {
                        action.tile_cache_capacity = Some(next_tile_cache_capacity as usize);
                    }
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut next_progressive_bypass_threshold,
                                MIN_PROGRESSIVE_BYPASS_THRESHOLD as u32..=MAX_PROGRESSIVE_BYPASS_THRESHOLD as u32,
                            )
                            .text("Bypass threshold"),
                        )
                        .changed()
                    {
                        action.progressive_bypass_threshold = Some(next_progressive_bypass_threshold as usize);
                    }
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut next_layer_bypass_entry_threshold,
                                MIN_LAYER_BYPASS_ENTRY_THRESHOLD as u32..=MAX_LAYER_BYPASS_ENTRY_THRESHOLD as u32,
                            )
                            .text("Layer bypass entries"),
                        )
                        .changed()
                    {
                        action.layer_bypass_entry_threshold = Some(next_layer_bypass_entry_threshold as usize);
                    }
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut next_layer_bypass_work_threshold,
                                MIN_LAYER_BYPASS_WORK_THRESHOLD as u32..=MAX_LAYER_BYPASS_WORK_THRESHOLD as u32,
                            )
                            .text("Layer bypass work"),
                        )
                        .changed()
                    {
                        action.layer_bypass_work_threshold = Some(next_layer_bypass_work_threshold as usize);
                    }
                    ui.label(format!("Tile hits: {}", render_debug_stats.tile_cache_hits));
                    ui.label(format!(
                        "Tile misses: {}",
                        render_debug_stats.tile_cache_misses
                    ));
                    ui.label(format!("Layer hits: {}", render_debug_stats.layer_cache_hits));
                    ui.label(format!(
                        "Layer misses: {}",
                        render_debug_stats.layer_cache_misses
                    ));
                    ui.label(format!(
                        "Cache: {}",
                        if render_debug_stats.cache_hit {
                            "hit"
                        } else {
                            "miss"
                        }
                    ));
                    ui.label(format!("Cache entries: {} / {}", render_debug_stats.cache_entries, render_debug_stats.cache_capacity));
                    ui.label(format!("Cache bytes: {}", render_debug_stats.cache_bytes));
                    ui.label(format!("Cache evictions: {}", render_debug_stats.cache_evictions));
                    ui.label(format!("Prepared shapes: {}", render_debug_stats.prepared_shapes));
                    ui.label(format!("Prepared tiles: {}", render_debug_stats.prepared_tiles));
                    ui.label(format!("Prepared fragments: {}", render_debug_stats.prepared_fragments));
                    ui.label(format!("Pending entries: {}", render_debug_stats.pending_entries));
                    ui.label(format!("Build budget: {}", render_debug_stats.build_budget));
                    ui.label(format!(
                        "Progressive mode: {}",
                        if render_debug_stats.progressive_bypassed {
                            "Bypassed"
                        } else {
                            "Progressive"
                        }
                    ));
                    ui.label(format!("Dropped stale entries: {}", render_debug_stats.dropped_stale_entries));
                    ui.label(format!(
                        "Active layer: {}",
                        render_debug_stats
                            .active_layer
                            .map(|layer| format!("L{} / D{}", layer.layer, layer.datatype))
                            .unwrap_or_else(|| "<none>".to_string())
                    ));
                    ui.label(format!("Active layer pending: {}", render_debug_stats.active_layer_pending));
                    ui.label(format!("Active layer estimated work: {}", render_debug_stats.active_layer_estimated_work));
                    ui.label(format!(
                        "Active layer mode: {}",
                        match render_debug_stats.active_layer_progress_mode {
                            Some(crate::renderer::ActiveLayerProgressMode::Bypassed) => "Bypassed",
                            Some(crate::renderer::ActiveLayerProgressMode::Progressive) => "Progressive",
                            None => "<none>",
                        }
                    ));
                    ui.label(format!(
                        "Layer bypass thresholds: entries <= {}, work <= {}",
                        render_debug_stats.layer_bypass_entry_threshold,
                        render_debug_stats.layer_bypass_work_threshold,
                    ));

                    ui.separator();
                    ui.label("Trends");
                    draw_metric_history(
                        ui,
                        "Vertices",
                        render_stats_history.vertices(),
                        Color32::from_rgb(120, 170, 255),
                        |value| format!("{value:.0}"),
                    );
                    draw_metric_history(
                        ui,
                        "Tile misses",
                        render_stats_history.tile_misses(),
                        Color32::from_rgb(120, 220, 160),
                        |value| format!("{value:.0}"),
                    );
                    draw_metric_history(
                        ui,
                        "Cache bytes",
                        render_stats_history.cache_bytes(),
                        Color32::from_rgb(255, 190, 110),
                        format_bytes_compact,
                    );
                    draw_metric_history(
                        ui,
                        "Pending entries",
                        render_stats_history.pending_entries(),
                        Color32::from_rgb(240, 140, 140),
                        |value| format!("{value:.0}"),
                    );

                    ui.label(format!("Hatch spacing: {:.1}", render_debug_stats.hatch_spacing));
                    ui.label(format!("Hatch width: {:.1}", render_debug_stats.hatch_width));

                    ui.separator();
                    let stats = scene.stats();
                    ui.label(format!("Shape count: {}", stats.shape_count));
                    if let Some(bounds) = scene.bounds() {
                        ui.label(format!(
                            "Bounds: [{:.1}, {:.1}] - [{:.1}, {:.1}]",
                            bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y
                        ));
                        ui.label(format!(
                            "Scene size: {:.1} x {:.1}",
                            bounds.width(),
                            bounds.height()
                        ));
                    } else {
                        ui.label("Bounds: <none>");
                    }

                    ui.separator();
                    ui.label(format!("Zoom: {:.4}x", camera.zoom()));
                    let pan = camera.pan();
                    ui.label(format!("Pan: ({:.1}, {:.1})", pan.x, pan.y));
                    ui.separator();
                    ui.label("Controls");
                    ui.label("Drag in canvas to pan");
                    ui.label("Scroll in canvas to zoom");
                });
        });

    if selected_view != scene_bundle.selected_index() {
        action.selected_view = Some(selected_view);
    }
    if layers_changed {
        action.hidden_layers = Some(next_hidden_layers);
    }
    if layer_modes_changed {
        action.layer_draw_modes = Some(next_layer_draw_modes);
    }
    if layer_hatch_styles_changed {
        action.layer_hatch_styles = Some(next_layer_hatch_styles);
    }
    if next_min_hierarchy_level != min_hierarchy_level {
        action.min_hierarchy_level = Some(next_min_hierarchy_level);
    }
    if next_max_hierarchy_level != max_hierarchy_level {
        action.max_hierarchy_level = Some(next_max_hierarchy_level);
    }

    // 中央 panel 本身不直接画版图内容，
    // 真正的版图由 wgpu pass 负责；这里主要提供交互区域和边框提示。
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(Color32::TRANSPARENT))
        .show(ctx, |ui| {
            let desired = ui.available_size();
            let (response, painter) = ui.allocate_painter(desired, Sense::drag());
            action.canvas_origin = Vec2::new(response.rect.min.x, response.rect.min.y);
            action.canvas_size = Vec2::new(response.rect.width(), response.rect.height());

            painter.rect_stroke(
                response.rect,
                0.0,
                Stroke::new(1.0, Color32::from_gray(70)),
                egui::StrokeKind::Inside,
            );

            if response.dragged() {
                let delta = ctx.input(|i| i.pointer.delta());
                action.pan_delta = Vec2::new(delta.x, delta.y);
            }

            if response.hovered() {
                let scroll = ctx.input(|i| i.raw_scroll_delta.y);
                if scroll.abs() > f32::EPSILON {
                    // 用 exp 映射滚轮值，会比线性倍率更自然，尤其在连续滚动时手感更顺。
                    let factor = (scroll / 240.0).exp();
                    let cursor = response
                        .hover_pos()
                        .map(|pos| pos - response.rect.min)
                        .unwrap_or(EguiVec2::ZERO);
                    action.zoom = Some((factor, Vec2::new(cursor.x, cursor.y)));
                }
            }
        });

    action
}

fn closed_shape_draw_mode_label(mode: ClosedShapeDrawMode) -> &'static str {
    match mode {
        ClosedShapeDrawMode::Outline => "Outline",
        ClosedShapeDrawMode::Hatch => "Hatch",
        ClosedShapeDrawMode::HatchOutline => "Hatch+Outline",
    }
}

fn hatch_style_label(style: HatchStylePreset) -> &'static str {
    match style {
        HatchStylePreset::LeftDiagonal => r"LeftDiag (\)",
        HatchStylePreset::RightDiagonal => "RightDiag (//)",
        HatchStylePreset::Cross => "Cross (XX)",
        HatchStylePreset::Dots => "Dots (..)",
    }
}


/// 画一个很轻量的 sparkline。
///
/// 这里没有额外引图表依赖，而是直接用 `egui::Painter` 自己画。
/// 好处是：
/// - 改动面小
/// - 更容易把绘制逻辑读明白
/// - 足够满足当前“看趋势而不是做专业图表”的需求
fn draw_metric_history(
    ui: &mut egui::Ui,
    label: &str,
    history: &MetricHistory,
    color: Color32,
    formatter: impl Fn(f32) -> String,
) {
    ui.label(format!(
        "{label}: {} (max {})",
        formatter(history.latest()),
        formatter(history.max_value())
    ));

    let desired_size = egui::vec2(ui.available_width().max(80.0), 44.0);
    let (response, painter) = ui.allocate_painter(desired_size, Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 4.0, Color32::from_gray(24));
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(52)),
        egui::StrokeKind::Inside,
    );

    let samples = history.samples();
    if samples.len() < 2 {
        return;
    }

    let max_value = history.max_value().max(1.0);
    let width = rect.width().max(1.0);
    let height = rect.height().max(1.0);
    let left = rect.left() + 6.0;
    let right = rect.right() - 6.0;
    let top = rect.top() + 6.0;
    let bottom = rect.bottom() - 6.0;
    let plot_width = (right - left).max(1.0);
    let plot_height = (bottom - top).max(1.0);

    let points: Vec<_> = samples
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = if samples.len() == 1 {
                left
            } else {
                left + plot_width * index as f32 / (samples.len() - 1) as f32
            };
            let y = bottom - (value / max_value) * plot_height;
            egui::pos2(x, y.clamp(top, bottom))
        })
        .collect();

    painter.line_segment(
        [egui::pos2(left, bottom), egui::pos2(right, bottom)],
        Stroke::new(1.0, Color32::from_gray(46)),
    );
    painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));

    let text = formatter(history.latest());
    painter.text(
        egui::pos2(rect.right() - 8.0, rect.top() + 6.0),
        egui::Align2::RIGHT_TOP,
        text,
        egui::TextStyle::Small.resolve(ui.style()),
        color,
    );

    let _ = (width, height);
}

fn format_bytes_compact(value: f32) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.2} MB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1} KB", value / 1024.0)
    } else {
        format!("{value:.0} B")
    }
}
