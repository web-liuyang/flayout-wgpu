//! 渲染器关键行为回归测试。
//!
//! 这组测试是整个 demo 里最值得认真读的一组，
//! 因为它基本覆盖了我们一路修过和优化过的核心行为：
//! - 顶点是否正确进入 NDC
//! - Retina / 高 DPI 下 fit 是否一致
//! - cache key 是否对关键状态敏感
//! - 离屏裁剪是否生效
//! - 空间索引 / tile grid 是否按预期工作
//! - 调试统计是否能真实反映渲染状态

use std::collections::{BTreeMap, BTreeSet};

use flayout_wgpu::{
    camera::Camera2D,
    renderer::{
        RenderDebugStats,
        geometry::{
            ClosedShapeDrawMode, DEFAULT_HATCH_SPACING, DEFAULT_HATCH_WIDTH, HatchParams,
            HatchStylePreset,
            ShapeSpatialIndex, TileGridIndex, TileId, build_hatch_signature, build_line_vertices,
            build_hatch_style_signature, build_render_cache_key,
            build_render_cache_key_with_hatch_styles, build_scaled_scene_vertices_for_indices,
            build_scaled_scene_vertices_for_indices_with_hatch_styles,
            build_scaled_scene_vertices_for_prepared_fragments, build_scaled_scene_vertices_for_tile,
            build_scene_vertices, layer_draw_mode_hash_value, layer_hatch_style_hash_value,
            logical_viewport_size, ndc_bounds, prepare_large_shape_tile_fragments,
            query_visible_shape_indices, query_visible_shapes, query_visible_tiles,
        },
    },
    scene::{Bounds, LayerId, RectShape, Scene},
};
use glam::Vec2;

/// 一个最小矩形场景应该被展开成可见的 clip-space 线段顶点。
#[test]
fn rectangle_scene_builds_clip_space_line_vertices() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut camera = Camera2D::new();
    let canvas_size = Vec2::new(100.0, 100.0);
    camera.fit_bounds(scene.bounds().expect("scene bounds"), canvas_size);

    let vertices = build_line_vertices(&scene, &camera, Vec2::ZERO, canvas_size, &BTreeSet::new());

    // 一个闭合矩形有 4 条边，每条边被膨胀为 2 个三角形，共 24 个顶点。
    assert_eq!(vertices.len(), 24);
    assert!(
        vertices
            .iter()
            .all(|vertex| vertex.position[0].abs() <= 1.0)
    );
    assert!(
        vertices
            .iter()
            .all(|vertex| vertex.position[1].abs() <= 1.0)
    );
}


/// 闭合图形在 `Hatch` 模式下应该真正生成 hatch 填充三角形，
/// 而不是继续只画轮廓。
#[test]
fn rectangle_scene_builds_hatch_fill_vertices_in_hatch_mode() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut camera = Camera2D::new();
    let canvas_size = Vec2::new(100.0, 100.0);
    camera.fit_bounds(scene.bounds().expect("scene bounds"), canvas_size);

    let vertices = build_scene_vertices(
        &scene,
        &camera,
        Vec2::ZERO,
        canvas_size,
        &BTreeSet::new(),
        &BTreeMap::new(),
        ClosedShapeDrawMode::Hatch,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    );

    // 一个矩形面被三角扇拆成 2 个三角形，共 6 个顶点。
    assert_eq!(vertices.len(), 6);
    assert!(vertices.iter().all(|vertex| vertex.position[0].abs() <= 1.0));
    assert!(vertices.iter().all(|vertex| vertex.position[1].abs() <= 1.0));
}

/// 当高点数闭合图形在屏幕上已经缩得很小时，
/// viewer 应该切到更粗的 LOD，而不是继续保留所有原始轮廓细节。
#[test]
fn coarse_lod_keeps_non_axis_aligned_shape_character() {
    let layer = LayerId { layer: 600, datatype: 10 };
    let base = [
        Vec2::new(0.0, -80.0),
        Vec2::new(55.0, -25.0),
        Vec2::new(110.0, 0.0),
        Vec2::new(55.0, 25.0),
        Vec2::new(0.0, 80.0),
        Vec2::new(-55.0, 25.0),
        Vec2::new(-110.0, 0.0),
        Vec2::new(-55.0, -25.0),
    ];
    let mut points = Vec::new();
    for index in 0..720usize {
        let t = index as f32 / 720.0;
        let seg = (t * base.len() as f32).floor() as usize % base.len();
        let local_t = (t * base.len() as f32) - seg as f32;
        let a = base[seg];
        let b = base[(seg + 1) % base.len()];
        points.push(a.lerp(b, local_t));
    }
    let scene = Scene::from_shapes(vec![RectShape {
        layer,
        hierarchy_level: 0,
        bounds: Bounds::new(-110.0, -80.0, 110.0, 80.0),
        points,
        closed: true,
        stroke_width_world: None,
    }]);
    let hidden = BTreeSet::new();
    let vertices = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        0.08,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Outline,
        HatchStylePreset::LeftDiagonal,
    );
    let unique_positions: std::collections::BTreeSet<(i32, i32)> = vertices
        .iter()
        .map(|vertex| ((vertex.position[0] * 100.0).round() as i32, (vertex.position[1] * 100.0).round() as i32))
        .collect();
    assert!(unique_positions.len() > 8);
}

#[test]
fn smaller_screen_extent_uses_stronger_closed_shape_lod() {
    let layer = LayerId { layer: 600, datatype: 10 };
    let point_count = 720usize;
    let radius = 50.0f32;
    let points: Vec<Vec2> = (0..point_count)
        .map(|idx| {
            let angle = (idx as f32 / point_count as f32) * std::f32::consts::TAU;
            Vec2::new(angle.cos() * radius, angle.sin() * radius)
        })
        .collect();
    let scene = Scene::from_shapes(vec![RectShape {
        layer,
        hierarchy_level: 0,
        bounds: Bounds::new(-radius, -radius, radius, radius),
        points,
        closed: true,
        stroke_width_world: None,
    }]);
    let hidden = BTreeSet::new();

    let medium = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        0.18,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Outline,
        HatchStylePreset::LeftDiagonal,
    );
    let tiny = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        0.08,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Outline,
        HatchStylePreset::LeftDiagonal,
    );

    assert!(tiny.len() < medium.len());
}

#[test]
fn small_high_detail_closed_shapes_switch_to_coarse_lod() {
    let layer = LayerId { layer: 600, datatype: 10 };
    let point_count = 720usize;
    let radius = 50.0f32;
    let points: Vec<Vec2> = (0..point_count)
        .map(|idx| {
            let angle = (idx as f32 / point_count as f32) * std::f32::consts::TAU;
            Vec2::new(angle.cos() * radius, angle.sin() * radius)
        })
        .collect();
    let scene = Scene::from_shapes(vec![RectShape {
        layer,
        hierarchy_level: 0,
        bounds: Bounds::new(-radius, -radius, radius, radius),
        points,
        closed: true,
        stroke_width_world: None,
    }]);

    let hidden = BTreeSet::new();
    let large_zoom_vertices = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
        HatchStylePreset::LeftDiagonal,
    );
    let small_zoom_vertices = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        0.1,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
        HatchStylePreset::LeftDiagonal,
    );

    assert!(large_zoom_vertices.len() > 4000);
    assert!(small_zoom_vertices.len() < large_zoom_vertices.len() / 4);
}

/// 不同 hatch preset 虽然共享同一套 CPU 填充三角形，
/// 但必须向 shader 传出不同的语义编码，否则片元阶段无法切换图案族。
#[test]
fn hatch_presets_encode_different_fill_vertex_styles_for_shader_path() {
    let shape = RectShape::rectangle(
        LayerId { layer: 1, datatype: 2 },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let hidden = BTreeSet::new();
    let no_overrides = BTreeMap::new();

    let left = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &no_overrides,
        &[0],
        ClosedShapeDrawMode::Hatch,
        HatchStylePreset::LeftDiagonal,
    );
    let right = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &no_overrides,
        &[0],
        ClosedShapeDrawMode::Hatch,
        HatchStylePreset::RightDiagonal,
    );
    let cross = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &no_overrides,
        &[0],
        ClosedShapeDrawMode::Hatch,
        HatchStylePreset::Cross,
    );
    let dots = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &no_overrides,
        &[0],
        ClosedShapeDrawMode::Hatch,
        HatchStylePreset::Dots,
    );

    let left_style = first_fill_hatch_style(&left);
    let right_style = first_fill_hatch_style(&right);
    let cross_style = first_fill_hatch_style(&cross);
    let dots_style = first_fill_hatch_style(&dots);

    assert_ne!(left_style, right_style);
    assert_ne!(left_style, cross_style);
    assert_ne!(left_style, dots_style);
    assert_ne!(right_style, cross_style);
    assert_ne!(right_style, dots_style);
    assert_ne!(cross_style, dots_style);
}

/// 闭合图形在 `Hatch + Outline` 模式下应该同时生成 hatch 面和轮廓线。
#[test]
fn rectangle_scene_builds_hatch_and_outline_vertices_in_hatch_outline_mode() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut camera = Camera2D::new();
    let canvas_size = Vec2::new(100.0, 100.0);
    camera.fit_bounds(scene.bounds().expect("scene bounds"), canvas_size);

    let vertices = build_scene_vertices(
        &scene,
        &camera,
        Vec2::ZERO,
        canvas_size,
        &BTreeSet::new(),
        &BTreeMap::new(),
        ClosedShapeDrawMode::HatchOutline,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    );

    // 一个矩形面是 6 个顶点，外轮廓是 24 个顶点，总共 30 个顶点。
    assert_eq!(vertices.len(), 30);
}

#[test]
fn small_high_detail_polyline_switches_to_coarse_lod() {
    let layer = LayerId { layer: 600, datatype: 10 };
    let point_count = 720usize;
    let points: Vec<Vec2> = (0..point_count)
        .map(|idx| {
            let x = idx as f32 * 0.5;
            let y = (idx as f32 * 0.2).sin() * 40.0;
            Vec2::new(x, y)
        })
        .collect();
    let scene = Scene::from_shapes(vec![RectShape::polyline(layer, points, 2.0)]);
    let hidden = BTreeSet::new();

    let large_zoom_vertices = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        1.0,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Outline,
        HatchStylePreset::LeftDiagonal,
    );
    let small_zoom_vertices = build_scaled_scene_vertices_for_indices_with_hatch_styles(
        &scene,
        0.05,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Outline,
        HatchStylePreset::LeftDiagonal,
    );

    assert!(large_zoom_vertices.len() > 4000);
    assert!(small_zoom_vertices.len() < large_zoom_vertices.len() / 4);
}

/// 开放折线即使在 `Hatch` 模式下也不应该被当成面填充，
/// 仍然应按线段膨胀逻辑绘制。
#[test]
fn open_polyline_stays_outline_only_in_hatch_mode() {
    let shape = RectShape::polyline(
        LayerId {
            layer: 1,
            datatype: 1,
        },
        vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)],
        4.0,
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut camera = Camera2D::new();
    let canvas_size = Vec2::new(100.0, 100.0);
    camera.fit_bounds(Bounds::new(0.0, -2.0, 10.0, 2.0), canvas_size);

    let vertices = build_scene_vertices(
        &scene,
        &camera,
        Vec2::ZERO,
        canvas_size,
        &BTreeSet::new(),
        &BTreeMap::new(),
        ClosedShapeDrawMode::Hatch,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    );

    assert_eq!(vertices.len(), 6);
}

/// 凹多边形在 hatch 填充模式下不应该被错误三角扇覆盖到轮廓外部。
///
/// 这个测试不直接比对像素，而是检查一个更稳定的几何性质：
/// 填充三角形总面积应该接近原始多边形面积。
/// 如果还在用简单三角扇，凹多边形通常会出现明显面积膨胀。
#[test]
fn concave_polygon_hatch_fill_preserves_polygon_area() {
    let layer = LayerId { layer: 7, datatype: 0 };
    let points = vec![
        Vec2::new(0.0, 0.0),
        Vec2::new(6.0, 0.0),
        Vec2::new(6.0, 2.0),
        Vec2::new(2.0, 2.0),
        Vec2::new(2.0, 6.0),
        Vec2::new(0.0, 6.0),
    ];
    let shape = RectShape {
        layer,
        hierarchy_level: 0,
        bounds: Bounds::new(0.0, 0.0, 6.0, 6.0),
        points: points.clone(),
        closed: true,
        stroke_width_world: None,
    };
    let scene = Scene::from_shapes(vec![shape]);

    let vertices = build_scaled_scene_vertices_for_indices(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Hatch,
    );

    let polygon_area = polygon_area(&points);
    let fill_area = triangles_area(&vertices);

    assert!(
        (fill_area - polygon_area).abs() < 0.01,
        "expected fill area {fill_area} to match polygon area {polygon_area}"
    );
}


/// 用真实 GDS 的弯曲边界做回归，避免只靠人造简单多边形误判。
#[test]
fn bend_euler_outer_polygon_fill_matches_polygon_area() {
    use flayout_wgpu::io::load_layout_bundle;

    let bundle = load_layout_bundle("/Users/liuyang/Desktop/xiaoyao/gdsii/bend_euler_1.gds")
        .expect("load bend euler gds");
    let shape = bundle
        .views()
        .iter()
        .flat_map(|view| view.scene.shapes().iter())
        .filter(|shape| shape.closed && shape.layer.layer == 1 && shape.layer.datatype == 2)
        .max_by(|a, b| {
            let area_a = polygon_area(&a.points);
            let area_b = polygon_area(&b.points);
            area_a.partial_cmp(&area_b).unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("outer polygon");

    let scene = Scene::from_shapes(vec![shape.clone()]);
    let vertices = build_scaled_scene_vertices_for_indices(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::Hatch,
    );

    let polygon_area = polygon_area(&shape.points);
    let fill_area = triangles_area(&vertices);

    assert!(
        (fill_area - polygon_area).abs() / polygon_area.max(1.0) < 0.001,
        "expected fill area {fill_area} to match polygon area {polygon_area}"
    );
}

/// 即使全局模式是 `Hatch + Outline`，单个 layer 也应该能强制降级成 `Outline`。
/// `vias_45.gds` 的大菱形在 tile 裁剪后，顶部/底部 fragment 不应因为相邻重复点而丢失填充。
#[test]
fn vias_45_top_and_bottom_fragments_still_build_hatch_fill_vertices() {
    use flayout_wgpu::io::load_layout_bundle;

    let bundle = load_layout_bundle("/Users/liuyang/Desktop/xiaoyao/gdsii/vias_45.gds")
        .expect("load vias_45 test layout");
    let scene = bundle
        .views()
        .iter()
        .find(|view| view.name == "Vias_s")
        .map(|view| &view.scene)
        .expect("Vias_s view");
    let grid = TileGridIndex::build_with_divisions(scene, 8);
    let prepared = prepare_large_shape_tile_fragments(scene, &grid, 4);
    let target_layer = LayerId { layer: 70, datatype: 30 };

    let zero_fill_fragments = prepared
        .per_tile_layer
        .iter()
        .filter(|((_, layer), _)| *layer == target_layer)
        .flat_map(|(_, fragments)| fragments.iter())
        .filter(|fragment| {
            let vertices = build_scaled_scene_vertices_for_prepared_fragments(
                std::slice::from_ref(*fragment),
                1.0,
                &BTreeSet::new(),
                &BTreeMap::new(),
                ClosedShapeDrawMode::Hatch,
            );
            !vertices.iter().any(|vertex| (vertex.kind - 1.0).abs() < 0.1)
        })
        .count();

    assert_eq!(zero_fill_fragments, 0);
}

#[test]
fn layer_override_can_force_outline_when_global_mode_is_hatch_outline() {
    let shape = RectShape::rectangle(
        LayerId { layer: 1, datatype: 2 },
        Bounds::new(0.0, 0.0, 10.0, 20.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut overrides = BTreeMap::new();
    overrides.insert(
        LayerId { layer: 1, datatype: 2 },
        ClosedShapeDrawMode::Outline,
    );

    let vertices = build_scaled_scene_vertices_for_indices(
        &scene,
        1.0,
        &BTreeSet::new(),
        &overrides,
        &[0],
        ClosedShapeDrawMode::HatchOutline,
    );

    assert_eq!(vertices.len(), 24);
}

/// 对跨很多 tile 的大多边形，tile 内局部裁剪应显著减少单个 tile 里的顶点量。
#[test]
fn tile_local_clipping_reduces_large_polygon_vertices() {
    let mut points = Vec::new();
    for x in 0..100 {
        points.push(Vec2::new(x as f32, 0.0));
    }
    for x in (0..100).rev() {
        points.push(Vec2::new(x as f32, 40.0 + (x as f32 * 0.1)));
    }

    let shape = RectShape {
        layer: LayerId { layer: 1, datatype: 2 },
        hierarchy_level: 0,
        bounds: Bounds::new(0.0, 0.0, 99.0, 49.9),
        points,
        closed: true,
        stroke_width_world: None,
    };
    let scene = Scene::from_shapes(vec![shape]);

    let unclipped = build_scaled_scene_vertices_for_indices(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
    );
    let clipped = build_scaled_scene_vertices_for_tile(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
        Some(Bounds::new(0.0, 0.0, 20.0, 20.0)),
        &BTreeMap::new(),
        HatchStylePreset::LeftDiagonal,
    );

    assert!(clipped.len() < unclipped.len());
}

/// 对横跨很多 tile 的超大多边形，预碎片化后的单 tile 顶点应与运行时 tile 裁剪一致。
#[test]
fn prepared_tile_fragments_match_runtime_tile_clipping_for_large_polygon() {
    let mut points = Vec::new();
    for x in 0..100 {
        points.push(Vec2::new(x as f32, 0.0));
    }
    for x in (0..100).rev() {
        points.push(Vec2::new(x as f32, 40.0 + (x as f32 * 0.1)));
    }

    let shape = RectShape {
        layer: LayerId { layer: 1, datatype: 2 },
        hierarchy_level: 0,
        bounds: Bounds::new(0.0, 0.0, 99.0, 49.9),
        points,
        closed: true,
        stroke_width_world: None,
    };
    let scene = Scene::from_shapes(vec![shape]);
    let grid = TileGridIndex::build_with_divisions(&scene, 4);
    let prepared = prepare_large_shape_tile_fragments(&scene, &grid, 4);

    assert!(prepared.shape_indices.contains(&0));

    let (&tile_id, layers) = prepared.tile_layers.iter().next().expect("prepared tile");
    let layer = *layers.first().expect("prepared layer");
    let fragments = prepared.fragments_for_tile_layer(tile_id, layer).expect("prepared fragments");
    let tile_bounds = grid.tile_bounds(tile_id);
    for fragment in fragments {
        assert!(fragment.points.iter().all(|point| {
            point.x >= tile_bounds.min_x - 1e-4
                && point.x <= tile_bounds.max_x + 1e-4
                && point.y >= tile_bounds.min_y - 1e-4
                && point.y <= tile_bounds.max_y + 1e-4
        }));
    }

    let prepared_vertices = build_scaled_scene_vertices_for_prepared_fragments(
        fragments,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        ClosedShapeDrawMode::HatchOutline,
    );
    let runtime_vertices = build_scaled_scene_vertices_for_tile(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
        Some(tile_bounds),
        &BTreeMap::new(),
        HatchStylePreset::LeftDiagonal,
    );

    assert_eq!(prepared_vertices.len(), runtime_vertices.len());
}

/// 对横跨很多 tile 的长 path，预碎片化后的局部线段也应该与运行时 tile 裁剪一致。
#[test]
fn prepared_tile_fragments_match_runtime_tile_clipping_for_large_polyline() {
    let shape = RectShape::polyline(
        LayerId { layer: 70, datatype: 31 },
        vec![Vec2::new(0.0, 0.0), Vec2::new(100.0, 100.0)],
        6.0,
    );
    let scene = Scene::from_shapes(vec![shape]);
    let grid = TileGridIndex::build_with_divisions(&scene, 4);
    let prepared = prepare_large_shape_tile_fragments(&scene, &grid, 4);

    assert!(prepared.shape_indices.contains(&0));

    let (&tile_id, layers) = prepared.tile_layers.iter().next().expect("prepared tile");
    let layer = *layers.first().expect("prepared layer");
    let fragments = prepared.fragments_for_tile_layer(tile_id, layer).expect("prepared fragments");
    let tile_bounds = grid.tile_bounds(tile_id);
    let prepared_vertices = build_scaled_scene_vertices_for_prepared_fragments(
        fragments,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        ClosedShapeDrawMode::HatchOutline,
    );
    let runtime_vertices = build_scaled_scene_vertices_for_tile(
        &scene,
        1.0,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &[0],
        ClosedShapeDrawMode::HatchOutline,
        Some(tile_bounds),
        &BTreeMap::new(),
        HatchStylePreset::LeftDiagonal,
    );

    assert_eq!(prepared_vertices.len(), runtime_vertices.len());
}

/// 高 DPI / Retina 下，逻辑画布 fit 的结果应该和普通屏幕保持一致。
#[test]
fn retina_viewport_uses_logical_canvas_scale_for_fit() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 100.0, 100.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let mut camera = Camera2D::new();
    let logical_canvas = Vec2::new(100.0, 100.0);
    camera.fit_bounds(scene.bounds().expect("scene bounds"), logical_canvas);

    let logical_vertices = build_line_vertices(
        &scene,
        &camera,
        Vec2::ZERO,
        logical_canvas,
        &BTreeSet::new(),
    );
    let retina_vertices = build_line_vertices(
        &scene,
        &camera,
        Vec2::ZERO,
        logical_viewport_size(Vec2::new(200.0, 200.0), 2.0),
        &BTreeSet::new(),
    );

    let logical_bounds = ndc_bounds(&logical_vertices).expect("logical ndc bounds");
    let retina_bounds = ndc_bounds(&retina_vertices).expect("retina ndc bounds");

    assert!((logical_bounds.width() - retina_bounds.width()).abs() < 0.01);
    assert!((logical_bounds.height() - retina_bounds.height()).abs() < 0.01);
}

/// 隐藏图层变化必须让 render cache key 变化，否则会错误复用旧缓存。
#[test]
fn render_cache_key_changes_when_hidden_layers_change() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0), Vec2::new(800.0, 600.0));

    let no_hidden = BTreeSet::new();
    let mut one_hidden = BTreeSet::new();
    one_hidden.insert(LayerId {
        layer: 1,
        datatype: 2,
    });

    let a = build_render_cache_key(
        7,
        &camera,
        &no_hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Outline,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    );
    let b = build_render_cache_key(
        7,
        &camera,
        &one_hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Outline,
        HatchParams {
            spacing: DEFAULT_HATCH_SPACING,
            width: DEFAULT_HATCH_WIDTH,
        },
    );

    assert_ne!(a, b);
}

/// hatch 参数变化也必须让 render cache key 变化，否则会错误复用旧图案。
#[test]
fn render_cache_key_changes_when_hatch_params_change() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0), Vec2::new(800.0, 600.0));

    let hidden = BTreeSet::new();
    let a = build_render_cache_key(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Hatch,
        HatchParams { spacing: 8.0, width: 1.0 },
    );
    let b = build_render_cache_key(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Hatch,
        HatchParams { spacing: 12.0, width: 1.0 },
    );
    let c = build_render_cache_key(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::HatchOutline,
        HatchParams { spacing: 8.0, width: 2.0 },
    );

    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(
        build_hatch_signature(HatchParams { spacing: 8.0, width: 1.0 }),
        build_hatch_signature(HatchParams { spacing: 12.0, width: 1.0 }),
    );
}

/// 每层显示模式覆盖变化也必须让 render cache key 变化。
#[test]
fn render_cache_key_changes_when_layer_draw_modes_change() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0), Vec2::new(800.0, 600.0));

    let hidden = BTreeSet::new();
    let mut overrides = BTreeMap::new();
    overrides.insert(
        LayerId { layer: 1, datatype: 2 },
        ClosedShapeDrawMode::Outline,
    );

    let a = build_render_cache_key(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::HatchOutline,
        HatchParams { spacing: 8.0, width: 1.0 },
    );
    let b = build_render_cache_key(
        7,
        &camera,
        &hidden,
        &overrides,
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::HatchOutline,
        HatchParams { spacing: 8.0, width: 1.0 },
    );

    assert_ne!(a, b);
    assert_ne!(layer_draw_mode_hash_value(&BTreeMap::new()), layer_draw_mode_hash_value(&overrides));
}

/// hatch style 变化也必须进入 render cache key，
/// 否则 shader 会错误复用仍然编码着旧 preset 的顶点缓存。
#[test]
fn render_cache_key_changes_when_hatch_style_changes() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0), Vec2::new(800.0, 600.0));

    let hidden = BTreeSet::new();
    let a = build_render_cache_key_with_hatch_styles(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Hatch,
        HatchParams { spacing: 8.0, width: 1.0 },
        HatchStylePreset::LeftDiagonal,
    );
    let b = build_render_cache_key_with_hatch_styles(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        &BTreeMap::new(),
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Hatch,
        HatchParams { spacing: 8.0, width: 1.0 },
        HatchStylePreset::Cross,
    );

    let mut style_overrides = BTreeMap::new();
    style_overrides.insert(LayerId { layer: 1, datatype: 2 }, HatchStylePreset::Dots);
    let c = build_render_cache_key_with_hatch_styles(
        7,
        &camera,
        &hidden,
        &BTreeMap::new(),
        &style_overrides,
        Vec2::new(280.0, 0.0),
        Vec2::new(1160.0, 960.0),
        Vec2::new(1440.0, 960.0),
        ClosedShapeDrawMode::Hatch,
        HatchParams { spacing: 8.0, width: 1.0 },
        HatchStylePreset::LeftDiagonal,
    );

    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(
        build_hatch_style_signature(HatchStylePreset::LeftDiagonal),
        build_hatch_style_signature(HatchStylePreset::Cross),
    );
    assert_ne!(
        layer_hatch_style_hash_value(&BTreeMap::new()),
        layer_hatch_style_hash_value(&style_overrides),
    );
}

/// 完全离屏的图元不应该继续生成顶点。
#[test]
fn offscreen_shapes_do_not_generate_vertices() {
    let visible = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 10.0, 10.0),
    );
    let offscreen = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(1_000.0, 1_000.0, 1_010.0, 1_010.0),
    );
    let scene = Scene::from_shapes(vec![visible, offscreen]);
    let mut camera = Camera2D::new();
    let canvas_size = Vec2::new(100.0, 100.0);
    camera.fit_bounds(Bounds::new(0.0, 0.0, 10.0, 10.0), canvas_size);

    let vertices = build_line_vertices(&scene, &camera, Vec2::ZERO, canvas_size, &BTreeSet::new());

    assert_eq!(vertices.len(), 24);
}

/// 空间索引查询应只返回与可见范围相交的 shape。
#[test]
fn spatial_index_returns_only_shapes_overlapping_visible_world_bounds() {
    let shapes = vec![
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(200.0, 200.0, 210.0, 210.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(400.0, 400.0, 410.0, 410.0),
        ),
    ];
    let scene = Scene::from_shapes(shapes);
    let index = ShapeSpatialIndex::build(&scene);

    let visible = query_visible_shape_indices(&scene, &index, Bounds::new(-5.0, -5.0, 20.0, 20.0));

    assert_eq!(visible, vec![0]);
}

/// 空间索引除了返回结果，还应该报告这次查询扫描了多少候选与 bucket。
#[test]
fn visible_shape_query_reports_bucket_and_candidate_stats() {
    let shapes = vec![
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(8.0, 8.0, 20.0, 20.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(400.0, 400.0, 410.0, 410.0),
        ),
    ];
    let scene = Scene::from_shapes(shapes);
    let index = ShapeSpatialIndex::build(&scene);

    let query = query_visible_shapes(&scene, &index, Bounds::new(-5.0, -5.0, 25.0, 25.0));

    assert_eq!(query.indices, vec![0, 1]);
    assert!(query.stats.bucket_hits >= 1);
    assert!(query.stats.candidate_shapes >= 2);
    assert_eq!(query.stats.visible_shapes, 2);
}

/// 渲染调试统计结构本身要能正确保存所有字段。
#[test]
fn render_debug_stats_capture_query_and_cache_state() {
    let stats = RenderDebugStats::new(9, 3, 2, 1, 24, 2, 2, 1, 1, 4, 2, 12, 64, 768, 3, 2, 5, 9, 6, 16, 2, None, 0, 0, None, false, 8, 128, true, 10.0, 1.5);

    assert_eq!(stats.total_shapes, 9);
    assert_eq!(stats.candidate_shapes, 3);
    assert_eq!(stats.visible_shapes, 2);
    assert_eq!(stats.bucket_hits, 1);
    assert_eq!(stats.vertex_count, 24);
    assert_eq!(stats.visible_tiles, 2);
    assert_eq!(stats.tile_cache_hits, 1);
    assert_eq!(stats.tile_cache_misses, 1);
    assert_eq!(stats.layer_cache_hits, 4);
    assert_eq!(stats.layer_cache_misses, 2);
    assert_eq!(stats.cache_entries, 12);
    assert_eq!(stats.cache_capacity, 64);
    assert_eq!(stats.hatch_spacing, 10.0);
    assert_eq!(stats.hatch_width, 1.5);
    assert_eq!(stats.cache_bytes, 768);
    assert_eq!(stats.cache_evictions, 3);
    assert_eq!(stats.prepared_shapes, 2);
    assert_eq!(stats.prepared_tiles, 5);
    assert_eq!(stats.prepared_fragments, 9);
    assert!(stats.cache_hit);
}

/// tile grid 现在也应该直接暴露 `tile + layer` 级别的 shape 索引，
/// 避免 renderer 每帧再临时从 tile 内二次分桶。
#[test]
fn tile_grid_provides_tile_layer_shape_indices() {
    let scene = Scene::from_shapes(vec![
        RectShape::rectangle(
            LayerId { layer: 1, datatype: 1 },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId { layer: 1, datatype: 2 },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId { layer: 1, datatype: 2 },
            Bounds::new(0.0, 0.0, 8.0, 8.0),
        ),
    ]);
    let grid = TileGridIndex::build_with_divisions(&scene, 2);
    let tile_id = TileId { col: 0, row: 0 };

    assert!(grid.layers_for_tile(tile_id).contains(&LayerId { layer: 1, datatype: 1 }));
    assert!(grid.layers_for_tile(tile_id).contains(&LayerId { layer: 1, datatype: 2 }));
    assert_eq!(grid.shape_indices_for_tile_layer(tile_id, LayerId { layer: 1, datatype: 1 }), &[0]);
    assert_eq!(grid.shape_indices_for_tile_layer(tile_id, LayerId { layer: 1, datatype: 2 }), &[1, 2]);
}

/// 预碎片化统计应该能反映一个大图元被切到多少 tile 和 fragment。
#[test]
fn prepared_tile_fragments_report_shape_tile_and_fragment_counts() {
    let shape = RectShape::rectangle(
        LayerId { layer: 1, datatype: 2 },
        Bounds::new(0.0, 0.0, 100.0, 100.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let grid = TileGridIndex::build_with_divisions(&scene, 4);
    let prepared = prepare_large_shape_tile_fragments(&scene, &grid, 4);

    assert_eq!(prepared.prepared_shape_count(), 1);
    assert_eq!(prepared.prepared_tile_count(), 16);
    assert_eq!(prepared.prepared_fragment_count(), 16);
    assert!(prepared.layers_for_tile(TileId { col: 0, row: 0 }).len() >= 1);
}

/// tile 查询只应返回和可见范围相交的 tile。
#[test]
fn tile_grid_returns_only_tiles_overlapping_visible_world_bounds() {
    let shapes = vec![
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(0.0, 0.0, 10.0, 10.0),
        ),
        RectShape::rectangle(
            LayerId {
                layer: 1,
                datatype: 2,
            },
            Bounds::new(200.0, 200.0, 210.0, 210.0),
        ),
    ];
    let scene = Scene::from_shapes(shapes);
    let grid = TileGridIndex::build(&scene);

    let tiles = query_visible_tiles(&grid, Bounds::new(-5.0, -5.0, 20.0, 20.0));

    assert_eq!(tiles.len(), 1);
}

/// 提高 tile grid 密度后，同一片可见区域应该被切成更多 tile。
#[test]
fn denser_tile_grid_splits_visible_region_into_more_tiles() {
    let shape = RectShape::rectangle(
        LayerId {
            layer: 1,
            datatype: 2,
        },
        Bounds::new(0.0, 0.0, 100.0, 100.0),
    );
    let scene = Scene::from_shapes(vec![shape]);
    let coarse_grid = TileGridIndex::build_with_divisions(&scene, 2);
    let dense_grid = TileGridIndex::build_with_divisions(&scene, 10);
    let visible = Bounds::new(0.0, 0.0, 24.0, 24.0);

    let coarse_tiles = query_visible_tiles(&coarse_grid, visible);
    let dense_tiles = query_visible_tiles(&dense_grid, visible);

    assert_eq!(coarse_tiles.len(), 1);
    assert!(dense_tiles.len() > coarse_tiles.len());
}

fn polygon_area(points: &[Vec2]) -> f32 {
    let mut area = 0.0;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        area += a.x * b.y - b.x * a.y;
    }
    area.abs() * 0.5
}

fn triangles_area(vertices: &[flayout_wgpu::renderer::geometry::LineVertex]) -> f32 {
    let mut area = 0.0;
    for tri in vertices.chunks_exact(3) {
        let a = Vec2::new(tri[0].position[0], tri[0].position[1]);
        let b = Vec2::new(tri[1].position[0], tri[1].position[1]);
        let c = Vec2::new(tri[2].position[0], tri[2].position[1]);
        area += ((b - a).perp_dot(c - a)).abs() * 0.5;
    }
    area
}

fn first_fill_hatch_style(vertices: &[flayout_wgpu::renderer::geometry::LineVertex]) -> u32 {
    vertices
        .iter()
        .find(|vertex| (vertex.kind - 1.0).abs() < 0.1)
        .map(|vertex| vertex.hatch_style.to_bits())
        .expect("fill vertex hatch style")
}
