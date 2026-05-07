use std::{collections::{BTreeMap, BTreeSet}, sync::Arc};

use flayout_wgpu::{
    persistence::{
        filter_hidden_layers_for_scene, filter_layer_draw_modes_for_scene,
        filter_layer_hatch_styles_for_scene,
        load_viewer_config_from_path, resolve_saved_view_index, save_viewer_config_to_path,
        PersistedCamera, PersistedClosedShapeDrawMode, PersistedHatchStylePreset,
        PersistedLayerDrawMode, PersistedLayerHatchStyle, PersistedLayerId, ViewerConfig,
    },
    renderer::geometry::{ClosedShapeDrawMode, HatchStylePreset},
    scene::{Bounds, LayerId, RectShape, Scene, SceneBundle, SceneView},
};

fn sample_scene() -> Scene {
    Scene::from_shapes(vec![
        RectShape::rectangle(
            LayerId { layer: 1, datatype: 1 },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId { layer: 70, datatype: 30 },
            Bounds::new(20.0, 20.0, 40.0, 40.0),
        ),
    ])
}

fn sample_config() -> ViewerConfig {
    ViewerConfig {
        layout_path: "/tmp/example.gds".to_string(),
        selected_view_name: Some("top".to_string()),
        camera: PersistedCamera {
            pan_x: 120.0,
            pan_y: 64.0,
            zoom: 2.5,
        },
        min_hierarchy_level: Some(1),
        max_hierarchy_level: Some(3),
        hidden_layers: vec![PersistedLayerId { layer: 70, datatype: 30 }],
        layer_draw_modes: vec![PersistedLayerDrawMode {
            layer: PersistedLayerId { layer: 1, datatype: 1 },
            mode: PersistedClosedShapeDrawMode::Outline,
        }],
        layer_hatch_styles: vec![PersistedLayerHatchStyle {
            layer: PersistedLayerId { layer: 70, datatype: 30 },
            style: PersistedHatchStylePreset::Cross,
        }],
        draw_mode: PersistedClosedShapeDrawMode::HatchOutline,
        hatch_spacing: 12.0,
        hatch_width: 2.0,
        tile_grid_divisions: 9,
        tile_cache_capacity: 256,
        progressive_bypass_threshold: 24,
        layer_bypass_entry_threshold: 8,
        layer_bypass_work_threshold: 128,
    }
}

#[test]
fn viewer_config_round_trip_preserves_core_fields() {
    let temp_path = std::env::temp_dir().join(format!(
        "flayout-viewer-config-{}-roundtrip.json",
        std::process::id()
    ));
    let config = sample_config();

    save_viewer_config_to_path(&temp_path, &config).expect("save config");
    let loaded = load_viewer_config_from_path(&temp_path).expect("load config");

    assert_eq!(loaded.layout_path, config.layout_path);
    assert_eq!(loaded.selected_view_name, config.selected_view_name);
    assert_eq!(loaded.camera.zoom, 2.5);
    assert_eq!(loaded.min_hierarchy_level, Some(1));
    assert_eq!(loaded.max_hierarchy_level, Some(3));
    assert_eq!(loaded.hidden_layers.len(), 1);
    assert_eq!(loaded.layer_draw_modes.len(), 1);
    assert_eq!(loaded.layer_hatch_styles.len(), 1);
    assert_eq!(
        loaded.layer_hatch_styles[0].style,
        PersistedHatchStylePreset::Cross
    );
    assert_eq!(loaded.tile_grid_divisions, 9);

    let _ = std::fs::remove_file(temp_path);
}

#[test]
fn resolve_saved_view_index_matches_saved_name() {
    let bundle = SceneBundle::new(vec![
        SceneView {
            name: "leaf".to_string(),
            scene: Arc::new(Scene::empty()),
        },
        SceneView {
            name: "top".to_string(),
            scene: Arc::new(Scene::empty()),
        },
    ]);

    assert_eq!(resolve_saved_view_index(&bundle, Some("top")), Some(1));
    assert_eq!(resolve_saved_view_index(&bundle, Some("missing")), None);
    assert_eq!(resolve_saved_view_index(&bundle, None), None);
}

#[test]
fn scene_layer_filters_ignore_unknown_saved_layers() {
    let scene = sample_scene();
    let config = ViewerConfig {
        hidden_layers: vec![
            PersistedLayerId { layer: 70, datatype: 30 },
            PersistedLayerId { layer: 999, datatype: 0 },
        ],
        layer_draw_modes: vec![
            PersistedLayerDrawMode {
                layer: PersistedLayerId { layer: 1, datatype: 1 },
                mode: PersistedClosedShapeDrawMode::Outline,
            },
            PersistedLayerDrawMode {
                layer: PersistedLayerId { layer: 999, datatype: 0 },
                mode: PersistedClosedShapeDrawMode::Hatch,
            },
        ],
        layer_hatch_styles: vec![
            PersistedLayerHatchStyle {
                layer: PersistedLayerId { layer: 70, datatype: 30 },
                style: PersistedHatchStylePreset::Dots,
            },
            PersistedLayerHatchStyle {
                layer: PersistedLayerId { layer: 999, datatype: 0 },
                style: PersistedHatchStylePreset::RightDiagonal,
            },
        ],
        ..sample_config()
    };

    let hidden = filter_hidden_layers_for_scene(&config, &scene);
    let draw_modes = filter_layer_draw_modes_for_scene(&config, &scene);
    let hatch_styles = filter_layer_hatch_styles_for_scene(&config, &scene);

    assert_eq!(hidden, BTreeSet::from([LayerId { layer: 70, datatype: 30 }]));
    assert_eq!(
        draw_modes,
        BTreeMap::from([(
            LayerId { layer: 1, datatype: 1 },
            ClosedShapeDrawMode::Outline,
        )])
    );
    assert_eq!(
        hatch_styles,
        BTreeMap::from([(
            LayerId { layer: 70, datatype: 30 },
            HatchStylePreset::Dots,
        )])
    );
}


#[test]
fn layer_bypass_thresholds_round_trip_through_viewer_config() {
    let temp_path = std::env::temp_dir().join(format!(
        "flayout-viewer-config-{}-layer-bypass.json",
        std::process::id()
    ));
    let config = sample_config();

    save_viewer_config_to_path(&temp_path, &config).expect("save config");
    let loaded = load_viewer_config_from_path(&temp_path).expect("load config");

    assert_eq!(loaded.layer_bypass_entry_threshold, 8);
    assert_eq!(loaded.layer_bypass_work_threshold, 128);

    let _ = std::fs::remove_file(temp_path);
}

#[test]
fn old_config_without_hierarchy_range_fields_still_loads() {
    let temp_path = std::env::temp_dir().join(format!(
        "flayout-viewer-config-{}-old-layout.json",
        std::process::id()
    ));
    let json = r#"{
  "layout_path": "/tmp/example.gds",
  "selected_view_name": "top",
  "camera": { "pan_x": 0.0, "pan_y": 0.0, "zoom": 1.0 },
  "hidden_layers": [],
  "layer_draw_modes": [],
  "layer_hatch_styles": [],
  "draw_mode": "hatch_outline",
  "hatch_spacing": 12.0,
  "hatch_width": 2.0,
  "tile_grid_divisions": 9,
  "tile_cache_capacity": 256,
  "progressive_bypass_threshold": 24
}"#;

    std::fs::write(&temp_path, json).expect("write old config");
    let loaded = load_viewer_config_from_path(&temp_path).expect("load old config");

    assert_eq!(loaded.min_hierarchy_level, None);
    assert_eq!(loaded.max_hierarchy_level, None);
    assert_eq!(loaded.layer_bypass_entry_threshold, 8);
    assert_eq!(loaded.layer_bypass_work_threshold, 128);

    let _ = std::fs::remove_file(temp_path);
}
