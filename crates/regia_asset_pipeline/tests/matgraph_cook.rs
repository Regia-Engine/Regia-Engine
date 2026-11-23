// SPDX-License-Identifier: MPL-2.0
//! Material graphs through the cook: a graph, an instance (switch + parameter override) and a
//! `.matfunc.ron` function cook into the cache and a pak; a game loads the instance from the
//! pak without compiling a graph and draws the frame the compiler's material draws; editing
//! the function file recooks the instance and a hot reload swaps the entity's material; a scene
//! file names the graph through the reflected `MaterialGraphRef`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, AssetSource, cooked_cache_dir};
use regia_asset_pipeline::{CookOptions, cook_project, pack_cooked_directory};
use regia_matgraph::{
    GraphMaterials, MaterialGraph, MaterialGraphPlugin, MaterialGraphRef, MaterialInstance, compile_with,
    instance_material, thread_compile_count,
};
use regia_render::{CpuMaterial, CpuMesh, LabTexture, MeshRenderer};

const FUNCTION: &str = r#"(
    format_version: 1,
    name: "wear",
    inputs: [(name: "c", ty: Vec3)],
    nodes: { "c": Input("c"), "k": Vec3((0.6, 0.55, 0.5)), "o": Mul("c", "k") },
    outputs: { "o": "o" },
)"#;

const GRAPH: &str = r#"(
    format_version: 2,
    name: "panel",
    params: [(name: "tint", value: Color((0.9, 0.9, 0.9)))],
    textures: [(name: "albedo", path: "assets/tex/checker.png", color_space: Srgb)],
    switches: [(name: "worn", default: false)],
    functions: { "wear": File("assets/materials/wear.matfunc.ron") },
    nodes: {
        "uv": TexCoord,
        "s": Sample(texture: "albedo", uv: "uv"),
        "rgb": Swizzle(input: "s", channels: "rgb"),
        "tint": Param("tint"),
        "lit": Mul("rgb", "tint"),
        "worn_c": Call(function: "wear", args: { "c": "lit" }),
        "base": Switch(switch: "worn", on: "worn_c", off: "lit"),
    },
    output: (base_color: Some("base")),
)"#;

const INSTANCE: &str = r#"(
    format_version: 2,
    parent: "assets/materials/panel.matgraph.ron",
    params: { "tint": Color((1.0, 0.2, 0.1)) },
    switches: { "worn": true },
)"#;

const INSTANCE_PATH: &str = "assets/materials/panel_red.matinst.ron";

fn checker() -> Vec<u8> {
    let mut rgba = Vec::new();
    for y in 0..8u32 {
        for x in 0..8u32 {
            rgba.extend_from_slice(if (x + y) % 2 == 0 {
                &[220, 210, 190, 255]
            } else {
                &[40, 70, 120, 255]
            });
        }
    }
    rgba
}

fn project() -> PathBuf {
    let root = std::env::temp_dir().join(format!("regia_matgraph_cook_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("assets/tex")).expect("tex dir");
    std::fs::create_dir_all(root.join("assets/materials")).expect("materials dir");
    image::RgbaImage::from_raw(8, 8, checker())
        .expect("checker")
        .save(root.join("assets/tex/checker.png"))
        .expect("png");
    std::fs::write(root.join("assets/materials/wear.matfunc.ron"), FUNCTION).expect("function");
    std::fs::write(root.join("assets/materials/panel.matgraph.ron"), GRAPH).expect("graph");
    std::fs::write(root.join(INSTANCE_PATH), INSTANCE).expect("instance");
    root
}

fn cook(root: &Path) -> regia_asset_pipeline::CookReport {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

/// What the compiler builds from the sources directly.
fn direct_material(root: &Path) -> CpuMaterial {
    let mut graph = MaterialGraph::from_ron(GRAPH).expect("graph");
    graph
        .resolve_function_files(&mut |path| {
            std::fs::read_to_string(root.join(path)).map_err(|e| e.to_string())
        })
        .expect("functions");
    let instance = MaterialInstance::from_ron(INSTANCE).expect("instance");
    let compiled = compile_with(&graph, &instance.switches).expect("compile");
    let mut images = BTreeMap::new();
    images.insert(
        "albedo".to_string(),
        LabTexture {
            width: 8,
            height: 8,
            rgba: checker(),
        },
    );
    instance_material(&graph, &compiled, &instance, &images, 0.0).expect("material")
}

#[test]
fn graphs_cook_into_a_pak_and_load_without_a_compile() {
    let root = project();
    let report = cook(&root);
    assert!(report.cooked >= 3, "{report:?}");

    let content = root.join("content");
    std::fs::create_dir_all(&content).expect("content dir");
    let cache = cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    let paks = pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pak");
    assert!(!paks.is_empty());

    // A game: an empty project root (no cooked cache) with the pak mounted.
    let game_root = root.join("game");
    std::fs::create_dir_all(&game_root).expect("game root");
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: game_root,
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: Some(content),
    })
    .expect("server");
    let mut graphs = GraphMaterials::default();
    let compiles = thread_compile_count();
    let material = graphs
        .load(&mut server, INSTANCE_PATH)
        .expect("load from the pak");
    assert_eq!(
        thread_compile_count(),
        compiles,
        "a cooked graph loads without a compile"
    );
    let id = graphs.asset_id(INSTANCE_PATH).expect("id");
    assert!(
        matches!(server.source_of(id), AssetSource::Pak(_)),
        "{:?}",
        server.source_of(id)
    );

    let direct = direct_material(&root);
    let (cooked, compiled) = (
        material.surface_graph.as_ref().expect("cooked graph"),
        direct.surface_graph.as_ref().expect("compiled graph"),
    );
    assert_eq!(
        cooked.key, compiled.key,
        "the cook compiled the instance's permutation"
    );
    assert!(cooked.wgsl.contains("// Mul `worn_c/o`"), "{}", cooked.wgsl);
    assert_eq!(cooked.params, compiled.params);
    let texture = cooked.textures[0].as_ref().expect("albedo");
    assert!(texture.srgb);
    assert_eq!(texture.image.rgba, checker());

    // Same frame on the GPU.
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|e| panic!("no GPU: {e}"));
    let render = |material: &CpuMaterial| {
        let mut scene = regia_render::hard_surface::hull_closeup_scene();
        scene.width = 128;
        scene.height = 96;
        for mesh in &mut scene.meshes {
            mesh.material = 0;
        }
        scene.materials = vec![material.clone()];
        regia_render::FrameRenderer::new()
            .render_offscreen(&device, &scene.as_frame_scene())
            .expect("frame")
            .pixels
    };
    assert_eq!(
        render(&material),
        render(&direct),
        "cooked and compiled draw the same frame"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_cook_report_counts_the_forward_permutations_of_the_graphs() {
    let root = project();
    let first = cook(&root);
    assert_eq!(first.permutation_budget, regia_render::FORWARD_PERMUTATION_BUDGET);
    assert!(!first.graph_permutations.is_empty(), "{first:?}");
    assert!(first.graph_permutations.len() <= 4);
    for set in &first.graph_permutations {
        assert_eq!(set[0], "MATERIAL_GRAPH", "{set:?}");
    }
    // A cache hit counts the same graphs.
    let second = cook(&root);
    assert_eq!(second.cooked, 0, "{second:?}");
    assert_eq!(second.graph_permutations, first.graph_permutations);
}

#[test]
fn a_function_edit_recooks_and_hot_reloads_the_entity() {
    let root = project();
    cook(&root);
    let unchanged = cook(&root);
    assert_eq!(
        unchanged.cooked, 0,
        "an unchanged project is a cache hit: {unchanged:?}"
    );

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin)
        .add_plugins(MaterialGraphPlugin);
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    app.insert_resource(server);
    let mesh = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let entity = app
        .world_mut()
        .spawn((
            MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
            MaterialGraphRef::new(INSTANCE_PATH),
        ))
        .id();
    app.update();
    let first = app
        .world()
        .get::<MeshRenderer>(entity)
        .expect("renderer")
        .material
        .clone();
    let first_key = first.surface_graph.as_ref().expect("graph material").key;

    // Edit the function: the instance (which calls it through its parent) recooks.
    std::fs::write(
        root.join("assets/materials/wear.matfunc.ron"),
        FUNCTION.replace("(0.6, 0.55, 0.5)", "(0.3, 0.3, 0.3)"),
    )
    .expect("edit");
    let recook = cook(&root);
    assert!(recook.cooked >= 2, "graph and instance recook: {recook:?}");
    // What the dev cooked watcher does for the rewritten file.
    let id = app
        .world()
        .resource::<GraphMaterials>()
        .asset_id(INSTANCE_PATH)
        .expect("id");
    app.world_mut()
        .resource_mut::<AssetServer>()
        .reload_id(id)
        .expect("reload");
    app.update();
    let second = app
        .world()
        .get::<MeshRenderer>(entity)
        .expect("renderer")
        .material
        .clone();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the hot reload swapped the material"
    );
    let second_key = second.surface_graph.as_ref().expect("graph material").key;
    assert_ne!(first_key, second_key);
    assert!(
        second
            .surface_graph
            .as_ref()
            .is_some_and(|g| g.wgsl.contains("vec3<f32>(0.3, 0.3, 0.3)")),
        "the new function body is in the code"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_scene_file_names_a_graph_through_the_reflected_component() {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin)
        .add_plugins(MaterialGraphPlugin);
    let guid = regia_core::id::Guid::new_random();
    let file = regia_scene::format::SceneFile::from_ron_str(&format!(
        r#"(format_version: 1, entities: [
            (guid: "{guid}", name: "Hull", components: {{
                "regia_matgraph::runtime::MaterialGraphRef": (path: "{INSTANCE_PATH}"),
            }}),
        ])"#
    ))
    .expect("scene RON");
    let spawned = regia_scene::spawn::spawn_scene(
        app.world_mut(),
        &file,
        &regia_scene::spawn::SpawnOptions::default(),
    )
    .expect("spawn");
    let entity = spawned.roots[0];
    assert_eq!(
        app.world().get::<MaterialGraphRef>(entity),
        Some(&MaterialGraphRef::new(INSTANCE_PATH))
    );
}
