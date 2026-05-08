//! IO / scene / 基础投影相关测试。
//!
//! 这些测试覆盖的不是“完整文件解析”，
//! 而是一些非常适合在单元测试层锁住的基础契约：
//! - 默认配置是否存在
//! - 空场景行为是否合理
//! - 错误路径是否会给出清晰报错
//! - 基础投影是否落在画布内
//! - scene 是否能正确暴露 layer 与 shape 元信息

use flayout_wgpu::{
    app::{LoadState, fill_missing_layer_hatch_styles, recommended_initial_hierarchy_level_range},
    camera::Camera2D,
    config::DEFAULT_LAYOUT_PATH,
    io::{load_layout_hierarchy_bundle, load_layout_scene},
    layout::{LayoutRepetition, LayoutViewBuildOptions, build_layout_view_scene},
    renderer::geometry::{HatchStylePreset, project_points},
    scene::{Bounds, LayerId, RectShape, Scene},
};
use glam::Vec2;
use laykit::{
    ArrayRef, Boundary, GDSBox, GDSElement, GDSIIFile, GDSStructure, GDSTime, GPath, StructRef,
};
use std::collections::BTreeMap;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// 默认版图路径常量必须存在，否则应用启动时没有任何加载目标。
#[test]
fn default_layout_path_constant_is_available() {
    assert!(!DEFAULT_LAYOUT_PATH.is_empty());
}

/// 空场景的统计和 bounds 应该是干净的。
#[test]
fn empty_scene_reports_zero_shapes() {
    let scene = Scene::empty();
    assert_eq!(scene.stats().shape_count, 0);
    assert!(scene.bounds().is_none());
}

/// 明显错误的路径应该走到缺失文件错误，而不是 panic。
#[test]
fn invalid_path_returns_missing_file_error() {
    let err = load_layout_scene("/definitely/not/here/demo.gds").unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

/// 经过 fit 后，一个简单矩形的四个角点都应该落在画布范围内。
#[test]
fn geometry_projection_keeps_points_in_canvas() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 0,
        },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape.clone()]);

    let mut camera = Camera2D::new();
    camera.fit_bounds(
        scene.bounds().expect("scene bounds"),
        Vec2::new(100.0, 100.0),
    );
    let points = project_points(&shape, &camera, Vec2::ZERO);
    assert_eq!(points.len(), 4);
    assert!(
        points
            .iter()
            .all(|point| point.x >= 0.0 && point.x <= 100.0)
    );
    assert!(
        points
            .iter()
            .all(|point| point.y >= 0.0 && point.y <= 100.0)
    );
}

/// 加载失败状态应该能把错误信息透传给 UI。
#[test]
fn failed_load_state_exposes_error_text() {
    let state = LoadState::Failed("parse failed".to_string());
    assert!(state.summary().contains("parse failed"));
}

/// scene 输出的 layer 列表应该：
/// - 去重
/// - 排序
#[test]
fn scene_exposes_unique_sorted_layers() {
    let scene = Scene::from_shapes(vec![
        RectShape::rectangle(
            LayerId {
                layer: 70,
                datatype: 31,
            },
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(2.0, 2.0, 3.0, 3.0),
        ),
    ]);

    assert_eq!(
        scene.layer_ids(),
        vec![
            LayerId {
                layer: 1,
                datatype: 2
            },
            LayerId {
                layer: 70,
                datatype: 31
            },
        ]
    );
}

/// path 类型图元要能保留世界坐标线宽，供 renderer 在不同 zoom 下换算屏幕线宽。
#[test]
fn path_shape_can_preserve_world_stroke_width() {
    let shape = RectShape::polyline(
        LayerId {
            layer: 1,
            datatype: 1,
        },
        vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)],
        2.5,
    );

    assert_eq!(shape.stroke_width_world, Some(2.5));
    assert!(!shape.closed);
}

#[test]
fn scene_can_filter_shapes_by_hierarchy_level_range() {
    let mut root = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 0,
        },
        Bounds::new(0.0, 0.0, 10.0, 10.0),
    );
    root.hierarchy_level = 0;
    let mut child = RectShape::rectangle(
        LayerId {
            layer: 2,
            datatype: 0,
        },
        Bounds::new(20.0, 20.0, 30.0, 30.0),
    );
    child.hierarchy_level = 2;
    let scene = Scene::from_shapes(vec![root, child]);

    let filtered = scene.filtered_by_hierarchy_range(0, 1);
    assert_eq!(filtered.stats().shape_count, 1);
    assert_eq!(
        filtered.shapes()[0].layer,
        LayerId {
            layer: 1,
            datatype: 0
        }
    );
    assert_eq!(scene.max_hierarchy_level(), 2);
}

#[test]
fn large_scene_defaults_to_half_hierarchy_depth() {
    let mut shapes = Vec::new();
    for index in 0..60_000usize {
        let mut shape = RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 0,
            },
            Bounds::new(index as f32, 0.0, index as f32 + 1.0, 1.0),
        );
        shape.hierarchy_level = if index % 3 == 0 { 5 } else { 0 };
        shapes.push(shape);
    }
    let scene = Scene::from_shapes(shapes);

    assert_eq!(recommended_initial_hierarchy_level_range(&scene), (0, 2));
}

/// 图层 hatch preset 的默认值不是“按缺失项轮流分配”，
/// 而是严格按 scene 排序后的绝对位置决定。
/// 这样即使中间某层已经有显式样式，其他层的默认结果也仍然稳定可预测。
#[test]
fn missing_layer_hatch_styles_receive_predictable_alternating_defaults() {
    let scene = Scene::from_shapes(vec![
        RectShape::rectangle(
            LayerId {
                layer: 70,
                datatype: 31,
            },
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 5,
                datatype: 0,
            },
            Bounds::new(2.0, 2.0, 3.0, 3.0),
        ),
    ]);
    let mut hatch_styles = BTreeMap::from([(
        LayerId {
            layer: 5,
            datatype: 0,
        },
        HatchStylePreset::Cross,
    )]);

    fill_missing_layer_hatch_styles(&scene, &mut hatch_styles);

    assert_eq!(
        hatch_styles.get(&LayerId {
            layer: 1,
            datatype: 2,
        }),
        Some(&HatchStylePreset::LeftDiagonal)
    );
    assert_eq!(
        hatch_styles.get(&LayerId {
            layer: 5,
            datatype: 0,
        }),
        Some(&HatchStylePreset::Cross)
    );
    assert_eq!(
        hatch_styles.get(&LayerId {
            layer: 70,
            datatype: 31,
        }),
        Some(&HatchStylePreset::LeftDiagonal)
    );
}

/// 分层 GDS 加载路径应该保留：
/// - root cell 发现
/// - `StructRef` 实例关系
/// - `ArrayRef` 的规则阵列语义
/// - path 的局部点列和线宽
#[test]
fn gds_loader_builds_hierarchical_layout() {
    let path = write_temp_gds_file(sample_hierarchical_gds(), "hierarchical-layout");
    let bundle = load_layout_hierarchy_bundle(path.to_str().expect("utf8 path"))
        .expect("hierarchical GDS bundle");

    let root_names: Vec<_> = bundle
        .views()
        .iter()
        .map(|view| view.metadata().name().to_string())
        .collect();
    assert_eq!(root_names, vec!["top".to_string(), "grid".to_string()]);

    let leaf = bundle
        .cells()
        .values()
        .find(|cell| cell.name() == "leaf")
        .expect("leaf cell");
    assert_eq!(leaf.local_shapes().len(), 2);
    assert_eq!(leaf.local_instance_count(), 0);
    assert_eq!(
        leaf.local_shapes()[0].points(),
        &[
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 20.0),
            Vec2::new(0.0, 20.0),
            Vec2::new(0.0, 0.0),
        ]
    );
    assert_eq!(leaf.local_shapes()[1].stroke_width(), Some(6.0));
    assert_eq!(
        leaf.local_shapes()[1].points(),
        &[
            Vec2::new(5.0, 5.0),
            Vec2::new(35.0, 5.0),
            Vec2::new(35.0, 15.0)
        ]
    );

    let top = bundle
        .cells()
        .values()
        .find(|cell| cell.name() == "top")
        .expect("top cell");
    assert_eq!(top.local_shapes().len(), 0);
    assert_eq!(top.local_instance_count(), 1);
    let top_instance = &top.instances()[0];
    assert_eq!(
        top_instance.transform().translation,
        Vec2::new(100.0, 200.0)
    );
    assert_eq!(top_instance.transform().rotation_degrees, 0.0);
    assert_eq!(top_instance.transform().magnification, 1.0);
    assert!(!top_instance.transform().mirrored_x);
    assert!(top_instance.repetition().is_none());

    let grid = bundle
        .cells()
        .values()
        .find(|cell| cell.name() == "grid")
        .expect("grid cell");
    assert_eq!(grid.local_shapes().len(), 1);
    assert_eq!(grid.local_instance_count(), 1);
    let array_instance = &grid.instances()[0];
    assert_eq!(
        array_instance.transform().translation,
        Vec2::new(20.0, 30.0)
    );
    assert_eq!(
        array_instance.repetition(),
        Some(&LayoutRepetition::regular_grid(
            3,
            2,
            Vec2::new(40.0, 0.0),
            Vec2::new(0.0, 50.0),
        ))
    );

    fs::remove_file(path).ok();
}

fn sample_hierarchical_gds() -> GDSIIFile {
    GDSIIFile {
        version: 600,
        modification_time: sample_time(),
        access_time: sample_time(),
        library_name: "hier".to_string(),
        units: (1e-6, 1e-9),
        reflibs: Vec::new(),
        fonts: Vec::new(),
        generations: None,
        attrtable: None,
        structures: vec![
            GDSStructure {
                name: "leaf".to_string(),
                creation_time: sample_time(),
                modification_time: sample_time(),
                strclass: None,
                elements: vec![
                    GDSElement::Boundary(Boundary {
                        layer: 1,
                        datatype: 0,
                        xy: vec![(0, 0), (10, 0), (10, 20), (0, 20), (0, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    }),
                    GDSElement::Path(GPath {
                        layer: 2,
                        datatype: 1,
                        pathtype: 0,
                        width: Some(6),
                        bgnextn: None,
                        endextn: None,
                        xy: vec![(5, 5), (35, 5), (35, 15)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    }),
                ],
            },
            GDSStructure {
                name: "top".to_string(),
                creation_time: sample_time(),
                modification_time: sample_time(),
                strclass: None,
                elements: vec![GDSElement::StructRef(StructRef {
                    sname: "leaf".to_string(),
                    xy: (100, 200),
                    strans: None,
                    elflags: None,
                    plex: None,
                    properties: Vec::new(),
                })],
            },
            GDSStructure {
                name: "grid".to_string(),
                creation_time: sample_time(),
                modification_time: sample_time(),
                strclass: None,
                elements: vec![
                    GDSElement::Box(GDSBox {
                        layer: 9,
                        boxtype: 0,
                        xy: vec![(0, 0), (20, 0), (20, 20), (0, 20), (0, 0)],
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    }),
                    GDSElement::ArrayRef(ArrayRef {
                        sname: "leaf".to_string(),
                        columns: 3,
                        rows: 2,
                        xy: vec![(20, 30), (140, 30), (20, 130)],
                        strans: None,
                        elflags: None,
                        plex: None,
                        properties: Vec::new(),
                    }),
                ],
            },
        ],
    }
}

fn sample_time() -> GDSTime {
    GDSTime {
        year: 2026,
        month: 5,
        day: 7,
        hour: 12,
        minute: 0,
        second: 0,
    }
}

fn write_temp_gds_file(file: GDSIIFile, prefix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "flayout-wgpu-{prefix}-{}-{stamp}.gds",
        std::process::id()
    ));
    file.write_to_file(&path).expect("write temp gds");
    path
}

#[test]
fn vias_45_hierarchical_builder_matches_flat_scene_signature_for_visible_levels() {
    let flat_bundle =
        flayout_wgpu::io::load_layout_bundle("/Users/liuyang/Desktop/xiaoyao/gdsii/vias_45.gds")
            .expect("load flat vias_45 bundle");
    let flat_scene = flat_bundle
        .views()
        .iter()
        .find(|view| view.name == "Vias_s")
        .map(|view| view.scene.filtered_by_hierarchy_range(0, 2))
        .expect("flat Vias_s view");

    let hierarchical_bundle =
        load_layout_hierarchy_bundle("/Users/liuyang/Desktop/xiaoyao/gdsii/vias_45.gds")
            .expect("load hierarchical vias_45 bundle");
    let root = hierarchical_bundle
        .views()
        .iter()
        .find(|view| view.metadata().name() == "Vias_s")
        .expect("hierarchical Vias_s root");
    let hierarchical_scene = build_layout_view_scene(
        &hierarchical_bundle,
        LayoutViewBuildOptions::new(root.metadata().root_cell_id(), 0, 2),
    )
    .expect("hierarchical workset");

    fn signature(scene: &Scene) -> Vec<(u32, u32, i32, i32, i32, i32, usize)> {
        let mut items: Vec<_> = scene
            .shapes()
            .iter()
            .map(|shape| {
                (
                    shape.layer.layer,
                    shape.layer.datatype,
                    shape.bounds.min_x.round() as i32,
                    shape.bounds.min_y.round() as i32,
                    shape.bounds.max_x.round() as i32,
                    shape.bounds.max_y.round() as i32,
                    shape.points.len(),
                )
            })
            .collect();
        items.sort_unstable();
        items
    }

    assert_eq!(
        flat_scene.stats().shape_count,
        hierarchical_scene.stats().shape_count
    );
    assert_eq!(signature(&flat_scene), signature(&hierarchical_scene));
}

#[test]
fn example_mzi_perf_fit_view_keeps_shallow_structure_while_collapsing_deep_subtrees() {
    let hierarchical_bundle =
        load_layout_hierarchy_bundle("/Users/liuyang/Desktop/xiaoyao/gdsii/example_mzi_perf.gds")
            .expect("load hierarchical example_mzi_perf bundle");
    let root = hierarchical_bundle
        .views()
        .iter()
        .find(|view| view.metadata().name() == "MziArray")
        .expect("hierarchical MziArray root");
    let root_cell = hierarchical_bundle
        .cell(root.metadata().root_cell_id())
        .expect("MziArray root cell");

    let viewport = Vec2::new(1600.0, 1200.0);
    let mut camera = Camera2D::new();
    camera.fit_bounds(root_cell.local_bounds().expect("MziArray bounds"), viewport);
    let visible_world =
        flayout_wgpu::renderer::geometry::camera_visible_world_bounds(&camera, viewport);

    let full_scene = build_layout_view_scene(
        &hierarchical_bundle,
        LayoutViewBuildOptions::new(root.metadata().root_cell_id(), 0, 5)
            .with_visible_world_bounds(Some(visible_world)),
    )
    .expect("full fit-view workset");
    let lod_scene = build_layout_view_scene(
        &hierarchical_bundle,
        LayoutViewBuildOptions::new(root.metadata().root_cell_id(), 0, 5)
            .with_visible_world_bounds(Some(visible_world))
            .with_subtree_screen_lod(camera.zoom(), 2.0, 4),
    )
    .expect("lod fit-view workset");

    let proxy_like_shape_count = lod_scene
        .shapes()
        .iter()
        .filter(|shape| !shape.closed && shape.points.len() == 2)
        .count();

    assert_eq!(full_scene.bounds(), root_cell.local_bounds());
    assert!(lod_scene.stats().shape_count > 1_000_000);
    assert!(lod_scene.stats().shape_count < full_scene.stats().shape_count);
    assert!(proxy_like_shape_count > 100_000);
    assert!(proxy_like_shape_count < lod_scene.stats().shape_count);
}
