use std::{collections::BTreeMap, env};

use flayout_wgpu::{
    io::{load_layout_bundle, load_layout_hierarchy_bundle},
    layout::{build_layout_view_scene, LayoutBundle, LayoutCellId, LayoutViewBuildOptions},
    renderer::geometry::{
        DEFAULT_TILE_GRID_DIVISIONS, LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD, TileGridIndex,
        prepare_large_shape_tile_fragments,
    },
    scene::{LayerId, Scene},
};

fn main() {
    let path = env::args().nth(1).expect("usage: cargo run --example memory_probe -- <layout> [view-name] [min-level] [max-level]");
    let view_name = env::args().nth(2);
    let min_level = env::args().nth(3).and_then(|value| value.parse::<u32>().ok());
    let max_level = env::args().nth(4).and_then(|value| value.parse::<u32>().ok());

    if path.to_ascii_lowercase().ends_with(".gds") {
        let bundle = load_layout_hierarchy_bundle(&path).expect("load hierarchical bundle");
        let selected = select_layout_root(&bundle, view_name.as_deref()).expect("layout root");
        let selected_name = selected.metadata().name().to_string();
        let root_cell_id = selected.metadata().root_cell_id();
        let max_depth = compute_layout_root_max_hierarchy_level(&bundle, root_cell_id);
        let min_level = min_level.unwrap_or(0);
        let max_level = max_level.unwrap_or(max_depth);
        let scene = build_layout_view_scene(
            &bundle,
            LayoutViewBuildOptions::new(root_cell_id, min_level, max_level),
        )
        .expect("build workset scene");

        println!("mode=hierarchical-gds");
        println!("view={selected_name}");
        println!("root_max_hierarchy_level={max_depth}");
        println!("requested_min_level={min_level}");
        println!("requested_max_level={max_level}");
        print_scene_stats(&scene);
        return;
    }

    let bundle = load_layout_bundle(&path).expect("load bundle");
    let scene = select_scene(&bundle, view_name.as_deref()).expect("scene");

    println!("mode=flat-legacy");
    println!(
        "view={}",
        view_name.unwrap_or_else(|| bundle.current_view().map(|v| v.name.clone()).unwrap_or_default())
    );
    print_scene_stats(scene);
}

fn print_scene_stats(scene: &Scene) {
    let shape_count = scene.shapes().len();
    let total_points = scene.total_point_count();
    let max_level = scene.max_hierarchy_level();
    let layers = scene.layer_ids().len();

    let grid = TileGridIndex::build_with_divisions(scene, DEFAULT_TILE_GRID_DIVISIONS);
    let prepared = prepare_large_shape_tile_fragments(
        scene,
        &grid,
        LARGE_SHAPE_PRE_FRAGMENT_TILE_THRESHOLD,
    );

    let prepared_points: usize = prepared
        .per_tile_layer
        .values()
        .flat_map(|fragments| fragments.iter())
        .map(|fragment| fragment.points.len())
        .sum();
    let prepared_outline_segments: usize = prepared
        .per_tile_layer
        .values()
        .flat_map(|fragments| fragments.iter())
        .map(|fragment| fragment.outline_segments.len())
        .sum();

    let mut layer_counts: BTreeMap<LayerId, usize> = BTreeMap::new();
    let mut level_counts: BTreeMap<u32, usize> = BTreeMap::new();
    for shape in scene.shapes() {
        *layer_counts.entry(shape.layer).or_default() += 1;
        *level_counts.entry(shape.hierarchy_level).or_default() += 1;
    }

    println!("shape_count={shape_count}");
    println!("total_points={total_points}");
    println!("layer_count={layers}");
    println!("max_hierarchy_level={max_level}");
    println!("prepared_shapes={}", prepared.prepared_shape_count());
    println!("prepared_tiles={}", prepared.prepared_tile_count());
    println!("prepared_fragments={}", prepared.prepared_fragment_count());
    println!("prepared_points={prepared_points}");
    println!("prepared_outline_segments={prepared_outline_segments}");
    println!("hierarchy_levels:");
    for (level, count) in level_counts {
        println!("  level {} -> {}", level, count);
    }
    println!("top_layers_by_shape_count:");
    for (layer, count) in layer_counts.into_iter().rev().take(16) {
        println!("  L{}/D{} -> {}", layer.layer, layer.datatype, count);
    }
}

fn select_layout_root<'a>(
    bundle: &'a LayoutBundle,
    wanted_name: Option<&str>,
) -> Option<&'a flayout_wgpu::layout::LayoutView> {
    if let Some(name) = wanted_name {
        bundle
            .views()
            .iter()
            .find(|view| view.metadata().name() == name)
    } else {
        bundle.selected_view()
    }
}

fn select_scene<'a>(
    bundle: &'a flayout_wgpu::scene::SceneBundle,
    wanted_name: Option<&str>,
) -> Option<&'a Scene> {
    if let Some(name) = wanted_name {
        bundle
            .views()
            .iter()
            .find(|view| view.name == name)
            .map(|view| view.scene.as_ref())
    } else {
        bundle.current_scene()
    }
}

fn compute_layout_root_max_hierarchy_level(bundle: &LayoutBundle, root_cell_id: LayoutCellId) -> u32 {
    fn visit(
        bundle: &LayoutBundle,
        cell_id: LayoutCellId,
        cache: &mut std::collections::HashMap<LayoutCellId, u32>,
    ) -> u32 {
        if let Some(depth) = cache.get(&cell_id) {
            return *depth;
        }

        let depth = bundle
            .cell(cell_id)
            .map(|cell| {
                cell.instances()
                    .iter()
                    .map(|instance| 1 + visit(bundle, instance.target_cell_id(), cache))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        cache.insert(cell_id, depth);
        depth
    }

    visit(bundle, root_cell_id, &mut std::collections::HashMap::new())
}
