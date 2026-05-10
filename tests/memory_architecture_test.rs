use std::sync::Arc;

use flayout_wgpu::{
    layout::{
        LayoutBundle, LayoutBundleError, LayoutCell, LayoutCellId, LayoutInstance,
        LayoutRepetition, LayoutShape, LayoutTransform, LayoutView, LayoutViewBuildOptions,
        LayoutViewMetadata, build_layout_view_scene,
    },
    scene::{Bounds, LayerId},
};
use glam::Vec2;

fn sample_layer(layer: u32, datatype: u32) -> LayerId {
    LayerId { layer, datatype }
}

fn sample_polygon(layer: LayerId) -> LayoutShape {
    LayoutShape::polygon(
        layer,
        vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(4.0, 0.0),
            Vec2::new(4.0, 2.0),
            Vec2::new(0.0, 2.0),
        ],
    )
}

#[test]
fn hierarchical_layout_cell_preserves_local_geometry_details() {
    let polygon = sample_polygon(sample_layer(1, 0));
    let path = LayoutShape::path(
        sample_layer(2, 1),
        vec![Vec2::new(10.0, 1.0), Vec2::new(14.0, 1.0)],
        2.0,
    );
    let cell = LayoutCell::new(
        LayoutCellId::new(7),
        "leaf",
        vec![polygon.clone(), path.clone()],
        Vec::new(),
    );

    assert_eq!(cell.id(), LayoutCellId::new(7));
    assert_eq!(cell.name(), "leaf");
    assert_eq!(cell.local_shapes().len(), 2);
    assert_eq!(cell.instances().len(), 0);
    assert_eq!(cell.local_shape_count(), 2);
    assert_eq!(cell.local_instance_count(), 0);
    assert_eq!(cell.local_shapes()[0].points(), polygon.points());
    assert!(cell.local_shapes()[0].closed());
    assert_eq!(cell.local_shapes()[0].stroke_width(), None);
    assert_eq!(cell.local_shapes()[1].points(), path.points());
    assert!(!cell.local_shapes()[1].closed());
    assert_eq!(cell.local_shapes()[1].stroke_width(), Some(2.0));
    assert_eq!(
        cell.local_shapes()[1].bounds(),
        Bounds::new(9.0, 0.0, 15.0, 2.0)
    );
    assert_eq!(cell.local_bounds(), Some(Bounds::new(0.0, 0.0, 15.0, 2.0)));
    assert_eq!(
        cell.local_layers(),
        &[sample_layer(1, 0), sample_layer(2, 1)]
    );
}

#[test]
fn hierarchical_layout_instance_preserves_array_repetition_without_eager_expansion() {
    let child_id = LayoutCellId::new(11);
    let child = Arc::new(LayoutCell::new(
        child_id,
        "child",
        vec![sample_polygon(sample_layer(2, 1))],
        Vec::new(),
    ));
    let instance = LayoutInstance::with_transform(
        child_id,
        Bounds::new(20.0, 30.0, 240.0, 140.0),
        LayoutTransform {
            translation: Vec2::new(20.0, 30.0),
            rotation_degrees: 90.0,
            magnification: 2.0,
            mirrored_x: true,
        },
    )
    .with_repetition(LayoutRepetition::regular_grid(
        3,
        2,
        Vec2::new(100.0, 0.0),
        Vec2::new(0.0, 80.0),
    ));
    let parent = LayoutCell::new(LayoutCellId::new(12), "parent", Vec::new(), vec![instance]);

    let bundle = LayoutBundle::new(
        vec![Arc::new(parent), Arc::clone(&child)],
        vec![LayoutView::new(LayoutViewMetadata::new(
            "top",
            LayoutCellId::new(12),
        ))],
    )
    .expect("bundle should build");

    let parent = bundle.cell(LayoutCellId::new(12)).expect("parent cell");
    let bundled_child = bundle.cell(child_id).expect("child cell");
    let instance = &parent.instances()[0];

    assert_eq!(instance.target_cell_id(), child_id);
    assert_eq!(
        instance.local_bounds(),
        Bounds::new(20.0, 30.0, 240.0, 140.0)
    );
    assert_eq!(instance.transform().translation, Vec2::new(20.0, 30.0));
    assert_eq!(instance.transform().rotation_degrees, 90.0);
    assert_eq!(instance.transform().magnification, 2.0);
    assert!(instance.transform().mirrored_x);
    assert_eq!(
        instance.repetition(),
        Some(&LayoutRepetition::regular_grid(
            3,
            2,
            Vec2::new(100.0, 0.0),
            Vec2::new(0.0, 80.0),
        ))
    );
    assert_eq!(parent.local_shapes().len(), 0);
    assert_eq!(parent.local_instance_count(), 1);
    assert_eq!(bundled_child.local_shapes().len(), 1);
}

#[test]
fn hierarchical_layout_bundle_rejects_missing_root_cell() {
    let root_id = LayoutCellId::new(21);
    let child = LayoutCell::new(
        LayoutCellId::new(22),
        "child",
        vec![sample_polygon(sample_layer(3, 0))],
        Vec::new(),
    );

    let error = LayoutBundle::new(
        vec![Arc::new(child)],
        vec![LayoutView::new(LayoutViewMetadata::new("top", root_id))],
    )
    .expect_err("missing root should fail");

    assert_eq!(error, LayoutBundleError::MissingRootCell(root_id));
}

#[test]
fn hierarchical_layout_bundle_rejects_dangling_instance_target() {
    let root_id = LayoutCellId::new(31);
    let missing_child_id = LayoutCellId::new(32);
    let root = LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![LayoutInstance::new(
            missing_child_id,
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        )],
    );

    let error = LayoutBundle::new(
        vec![Arc::new(root)],
        vec![LayoutView::new(LayoutViewMetadata::new("top", root_id))],
    )
    .expect_err("dangling instance target should fail");

    assert_eq!(
        error,
        LayoutBundleError::DanglingInstanceTarget {
            owner_cell_id: root_id,
            target_cell_id: missing_child_id,
        }
    );
}

#[test]
fn hierarchical_layout_bundle_exposes_selected_root_metadata_without_flat_scene() {
    let root_id = LayoutCellId::new(41);
    let child_id = LayoutCellId::new(42);

    let root = LayoutCell::new(
        root_id,
        "root",
        vec![sample_polygon(sample_layer(4, 0))],
        vec![LayoutInstance::new(
            child_id,
            Bounds::new(10.0, 10.0, 20.0, 20.0),
        )],
    );
    let child = LayoutCell::new(
        child_id,
        "child",
        vec![sample_polygon(sample_layer(5, 0))],
        Vec::new(),
    );

    let mut bundle = LayoutBundle::new(
        vec![Arc::new(root), Arc::new(child)],
        vec![
            LayoutView::new(LayoutViewMetadata::new("top", root_id)),
            LayoutView::new(LayoutViewMetadata::new("child", child_id)),
        ],
    )
    .expect("bundle should build");

    assert_eq!(
        bundle
            .selected_root_metadata()
            .map(LayoutViewMetadata::name),
        Some("top")
    );
    assert_eq!(
        bundle.selected_root_cell().map(LayoutCell::name),
        Some("root")
    );

    assert!(bundle.select(1));
    assert_eq!(
        bundle
            .selected_root_metadata()
            .map(LayoutViewMetadata::name),
        Some("child")
    );
    assert_eq!(
        bundle.selected_root_cell().map(LayoutCell::name),
        Some("child")
    );
}

#[test]
fn view_builder_can_filter_single_layer() {
    let root_id = LayoutCellId::new(1);
    let layer_a = sample_layer(10, 0);
    let layer_b = sample_layer(11, 0);
    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        vec![
            LayoutShape::rectangle(layer_a, Bounds::new(0.0, 0.0, 10.0, 10.0)),
            LayoutShape::rectangle(layer_b, Bounds::new(20.0, 0.0, 30.0, 10.0)),
        ],
        vec![],
    ));
    let bundle = LayoutBundle::new(
        vec![root],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle");

    let scene = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 0).with_layer_filter(Some(layer_b)),
    )
    .expect("scene");

    assert_eq!(scene.stats().shape_count, 1);
    assert_eq!(scene.shapes()[0].layer, layer_b);
}

#[test]
fn view_builder_expands_only_requested_hierarchy_levels() {
    let root_id = LayoutCellId::new(100);
    let child_id = LayoutCellId::new(101);
    let grandchild_id = LayoutCellId::new(102);
    let layer_root = sample_layer(10, 0);
    let layer_child = sample_layer(11, 0);
    let layer_grandchild = sample_layer(12, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        vec![LayoutShape::rectangle(
            layer_root,
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        )],
        vec![LayoutInstance::with_transform(
            child_id,
            Bounds::new(20.0, 0.0, 30.0, 10.0),
            LayoutTransform {
                translation: Vec2::new(20.0, 0.0),
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "child",
        vec![LayoutShape::rectangle(
            layer_child,
            Bounds::new(0.0, 0.0, 8.0, 8.0),
        )],
        vec![LayoutInstance::with_transform(
            grandchild_id,
            Bounds::new(5.0, 5.0, 9.0, 9.0),
            LayoutTransform {
                translation: Vec2::new(5.0, 5.0),
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let grandchild = Arc::new(LayoutCell::new(
        grandchild_id,
        "grandchild",
        vec![LayoutShape::rectangle(
            layer_grandchild,
            Bounds::new(0.0, 0.0, 4.0, 4.0),
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child, grandchild],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let root_only = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 0).with_visible_world_bounds(None),
    )
    .expect("root-only scene");
    assert_eq!(root_only.stats().shape_count, 1);
    assert!(
        root_only
            .shapes()
            .iter()
            .all(|shape| shape.hierarchy_level == 0)
    );
    assert_eq!(root_only.shapes()[0].layer, layer_root);

    let child_only = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 1, 1).with_visible_world_bounds(None),
    )
    .expect("child-only scene");
    assert_eq!(child_only.stats().shape_count, 1);
    assert!(
        child_only
            .shapes()
            .iter()
            .all(|shape| shape.hierarchy_level == 1)
    );
    assert_eq!(child_only.shapes()[0].layer, layer_child);

    let grandchild_only = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 2, 2).with_visible_world_bounds(None),
    )
    .expect("grandchild-only scene");
    assert_eq!(grandchild_only.stats().shape_count, 1);
    assert!(
        grandchild_only
            .shapes()
            .iter()
            .all(|shape| shape.hierarchy_level == 2)
    );
    assert_eq!(grandchild_only.shapes()[0].layer, layer_grandchild);
}

#[test]
fn view_builder_skips_offscreen_subtrees_when_visible_world_is_provided() {
    let root_id = LayoutCellId::new(200);
    let child_id = LayoutCellId::new(201);
    let target_layer = sample_layer(20, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![
            LayoutInstance::with_transform(
                child_id,
                Bounds::new(0.0, 0.0, 10.0, 10.0),
                LayoutTransform {
                    translation: Vec2::new(0.0, 0.0),
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            ),
            LayoutInstance::with_transform(
                child_id,
                Bounds::new(1000.0, 1000.0, 1010.0, 1010.0),
                LayoutTransform {
                    translation: Vec2::new(1000.0, 1000.0),
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            ),
        ],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "leaf",
        vec![LayoutShape::rectangle(
            target_layer,
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let scene = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 4)
            .with_visible_world_bounds(Some(Bounds::new(-50.0, -50.0, 50.0, 50.0))),
    )
    .expect("pruned scene");

    assert_eq!(scene.stats().shape_count, 1);
    assert_eq!(scene.shapes()[0].layer, target_layer);
    assert_eq!(scene.shapes()[0].bounds, Bounds::new(0.0, 0.0, 10.0, 10.0));
}

#[test]
fn view_builder_expands_regular_grid_instances_into_flat_workset_shapes() {
    let root_id = LayoutCellId::new(300);
    let child_id = LayoutCellId::new(301);
    let layer = sample_layer(30, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![
            LayoutInstance::with_transform(
                child_id,
                Bounds::new(10.0, 20.0, 32.0, 42.0),
                LayoutTransform {
                    translation: Vec2::new(10.0, 20.0),
                    rotation_degrees: 0.0,
                    magnification: 1.0,
                    mirrored_x: false,
                },
            )
            .with_repetition(LayoutRepetition::regular_grid(
                2,
                2,
                Vec2::new(20.0, 0.0),
                Vec2::new(0.0, 20.0),
            )),
        ],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "via",
        vec![LayoutShape::rectangle(
            layer,
            Bounds::new(0.0, 0.0, 2.0, 2.0),
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let scene = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 3).with_visible_world_bounds(None),
    )
    .expect("expanded scene");

    let mut mins: Vec<(i32, i32)> = scene
        .shapes()
        .iter()
        .map(|shape| {
            (
                shape.bounds.min_x.round() as i32,
                shape.bounds.min_y.round() as i32,
            )
        })
        .collect();
    mins.sort_unstable();

    assert_eq!(scene.stats().shape_count, 4);
    assert_eq!(mins, vec![(10, 20), (10, 40), (30, 20), (30, 40)]);
}

#[test]
fn view_builder_preserves_path_stroke_width_through_expansion() {
    let root_id = LayoutCellId::new(400);
    let child_id = LayoutCellId::new(401);
    let layer = sample_layer(40, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![LayoutInstance::with_transform(
            child_id,
            Bounds::new(50.0, 60.0, 70.0, 80.0),
            LayoutTransform {
                translation: Vec2::new(50.0, 60.0),
                rotation_degrees: 0.0,
                magnification: 2.0,
                mirrored_x: false,
            },
        )],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "path-cell",
        vec![LayoutShape::path(
            layer,
            vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)],
            3.0,
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let scene = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 3).with_visible_world_bounds(None),
    )
    .expect("expanded scene");

    assert_eq!(scene.stats().shape_count, 1);
    assert_eq!(scene.shapes()[0].stroke_width_world, Some(6.0));
    assert_eq!(scene.shapes()[0].points[0], Vec2::new(50.0, 60.0));
    assert_eq!(scene.shapes()[0].points[1], Vec2::new(70.0, 60.0));
}

#[test]
fn view_builder_can_skip_tiny_subtrees_by_screen_extent() {
    let root_id = LayoutCellId::new(500);
    let child_id = LayoutCellId::new(501);
    let grandchild_id = LayoutCellId::new(502);
    let layer = sample_layer(50, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![LayoutInstance::with_transform(
            child_id,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            LayoutTransform {
                translation: Vec2::ZERO,
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "child",
        Vec::new(),
        vec![LayoutInstance::with_transform(
            grandchild_id,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            LayoutTransform {
                translation: Vec2::ZERO,
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let grandchild = Arc::new(LayoutCell::new(
        grandchild_id,
        "leaf",
        vec![LayoutShape::rectangle(
            layer,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child, grandchild],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let skipped = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 3)
            .with_visible_world_bounds(None)
            .with_subtree_screen_lod(1.0, 2.0, 2),
    )
    .expect("skipped scene");
    assert_eq!(skipped.stats().shape_count, 1);
    assert_eq!(skipped.shapes()[0].layer, layer);
    assert!(!skipped.shapes()[0].closed);
    assert_eq!(skipped.shapes()[0].points.len(), 2);

    let expanded = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 3)
            .with_visible_world_bounds(None)
            .with_subtree_screen_lod(4.0, 2.0, 2),
    )
    .expect("expanded scene");
    assert_eq!(expanded.stats().shape_count, 1);
    assert_eq!(expanded.shapes()[0].layer, layer);
}

#[test]
fn view_builder_does_not_collapse_shallow_subtrees_before_min_level() {
    let root_id = LayoutCellId::new(510);
    let child_id = LayoutCellId::new(511);
    let grandchild_id = LayoutCellId::new(512);
    let layer = sample_layer(51, 0);

    let root = Arc::new(LayoutCell::new(
        root_id,
        "root",
        Vec::new(),
        vec![LayoutInstance::with_transform(
            child_id,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            LayoutTransform {
                translation: Vec2::ZERO,
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let child = Arc::new(LayoutCell::new(
        child_id,
        "child",
        Vec::new(),
        vec![LayoutInstance::with_transform(
            grandchild_id,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            LayoutTransform {
                translation: Vec2::ZERO,
                rotation_degrees: 0.0,
                magnification: 1.0,
                mirrored_x: false,
            },
        )],
    ));
    let grandchild = Arc::new(LayoutCell::new(
        grandchild_id,
        "leaf",
        vec![LayoutShape::rectangle(
            layer,
            Bounds::new(0.0, 0.0, 1.0, 1.0),
        )],
        Vec::new(),
    ));
    let bundle = LayoutBundle::new(
        vec![root, child, grandchild],
        vec![LayoutView::new(LayoutViewMetadata::new("root", root_id))],
    )
    .expect("bundle should build");

    let expanded = build_layout_view_scene(
        &bundle,
        LayoutViewBuildOptions::new(root_id, 0, 3)
            .with_visible_world_bounds(None)
            .with_subtree_screen_lod(1.0, 2.0, 4),
    )
    .expect("expanded scene");

    assert_eq!(expanded.stats().shape_count, 1);
    assert!(expanded.shapes()[0].closed);
    assert!(expanded.shapes()[0].points.len() > 2);
}
