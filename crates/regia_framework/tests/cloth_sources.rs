// SPDX-License-Identifier: MPL-2.0
//! Cooked cloth and soft assets at runtime: `ClothSimSource` / `SoftSimSource` resolve through
//! the `AssetServer` into `ClothSim` / `SoftSim` with running solvers, and a hot reload rebuilds
//! the solver. Cooked files are written here; the path from `.cloth.ron` through `cook_project`
//! to these files is tested in `regia_asset_pipeline::cloth_cook`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_animation::skeleton::LocalBind;
use regia_animation::{Skeleton, SkinnedMesh};
use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetManifest, AssetServer, AssetServerSettings, COOKED_FORMAT_VERSION, CookedHeader,
    ManifestEntry, cooked_cache_dir, cooked_file_name, encode_cooked_file,
};
use regia_core::id::TypePathHash;
use regia_core::math::Vec3;
use regia_framework::cloth::{
    ClothGluePlugin, ClothOutput, ClothSim, ClothSimSource, ClothSourceState, SoftSim, SoftSimSource,
    load_cloth_asset, load_soft_asset,
};
use regia_physics::cloth::asset::{
    COOKED_CLOTH_TYPE_PATH, COOKED_SOFT_TYPE_PATH, ClothSource, SimMeshInput, SoftSource,
};
use regia_physics::cloth::{ClothAsset, ClothSet, SoftAsset, SoftSet};
use regia_scene::Transform;

const CLOTH_PATH: &str = "assets/flag.cloth.ron";
const SOFT_PATH: &str = "assets/ball.soft.ron";

fn sheet() -> SimMeshInput {
    let mut mesh = SimMeshInput::default();
    for r in 0..5u32 {
        for c in 0..5u32 {
            mesh.positions
                .push(Vec3::new(c as f32 * 0.1, -(r as f32) * 0.1, 0.0));
        }
    }
    for r in 0..4u32 {
        for c in 0..4u32 {
            let i = r * 5 + c;
            mesh.triangles.push([i, i + 5, i + 1]);
            mesh.triangles.push([i + 1, i + 5, i + 6]);
        }
    }
    mesh
}

fn octahedron() -> SimMeshInput {
    SimMeshInput {
        positions: vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z],
        triangles: vec![
            [0, 2, 4],
            [2, 1, 4],
            [1, 3, 4],
            [3, 0, 4],
            [2, 0, 5],
            [1, 2, 5],
            [3, 1, 5],
            [0, 3, 5],
        ],
        ..SimMeshInput::default()
    }
}

fn cloth_payload(default_reach: f32) -> Vec<u8> {
    let source = ClothSource::from_ron_str(&format!(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "mesh.glb", node: "Sim"),
            maps: {{ "reach": Sparse(default: {default_reach}, entries: [(0, 0.0), (4, 0.0)]) }})"#
    ))
    .expect("parses");
    ClothAsset::cook(&source, &sheet(), None, &[])
        .expect("cooks")
        .to_bytes()
        .expect("payload")
}

fn soft_payload() -> Vec<u8> {
    let source = SoftSource::from_ron_str(
        r#"(format_version: 1, name: "ball", model: Pressure(pressure: 1.0, volume_compliance: 0.0),
            surface: (source: "mesh.glb", node: "Ball"), params: (gravity_scale: 0.0))"#,
    )
    .expect("parses");
    SoftAsset::cook(&source, &octahedron(), None)
        .expect("cooks")
        .to_bytes()
        .expect("payload")
}

fn write_cooked(cache: &Path, id: AssetId, type_path: &str, payload: &[u8]) {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(type_path),
        asset_id: id,
        importer_version: 1_000_000,
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, payload).expect("cooked file");
    std::fs::write(cache.join(cooked_file_name(id)), file).expect("write cooked");
}

/// A project with a cooked cloth and soft body, as `cook_project` leaves them.
fn project(tag: &str) -> (PathBuf, AssetId) {
    let root = regia_test_utils::unique_temp_dir(tag);
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let cloth = AssetId::new_random();
    let soft = AssetId::new_random();
    write_cooked(&cache, cloth, COOKED_CLOTH_TYPE_PATH, &cloth_payload(2.0));
    write_cooked(&cache, soft, COOKED_SOFT_TYPE_PATH, &soft_payload());
    let mut map = BTreeMap::new();
    for (path, id, type_path) in [
        (CLOTH_PATH, cloth, COOKED_CLOTH_TYPE_PATH),
        (SOFT_PATH, soft, COOKED_SOFT_TYPE_PATH),
    ] {
        map.insert(
            path.to_string(),
            ManifestEntry {
                path: path.to_string(),
                id,
                type_path: type_path.to_string(),
            },
        );
    }
    AssetManifest::from_map(map)
        .write_to_cache(&cache)
        .expect("manifest");
    (root, cloth)
}

fn server(root: &Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("open")
}

fn app(server: AssetServer) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ))
        .insert_resource(server)
        .add_plugins(ClothGluePlugin);
    app
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

#[test]
fn cooked_assets_load_by_either_path_form_and_report_bad_ones() {
    let (root, _) = project("cloth_sources_load");
    let mut server = server(&root);
    for path in ["assets/flag.cloth.ron", "flag.cloth.ron"] {
        let asset = load_cloth_asset(&mut server, path).expect("loads");
        assert_eq!(asset.name, "flag");
        assert_eq!(asset.lod0().rest_positions.len(), 25);
    }
    let soft = load_soft_asset(&mut server, "ball.soft.ron").expect("loads");
    assert_eq!(soft.name, "ball");
    assert_eq!(
        load_cloth_asset(&mut server, "missing.cloth.ron")
            .expect_err("absent")
            .code(),
        "cloth_asset_invalid"
    );
    assert_eq!(
        load_cloth_asset(&mut server, "ball.soft.ron")
            .expect_err("wrong type")
            .code(),
        "cloth_asset_invalid",
        "a soft body is not a cloth"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn saved_sources_resolve_into_running_instances() {
    let (root, _) = project("cloth_sources_resolve");
    let mut app = app(server(&root));
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton"),
    );
    let owner = app
        .world_mut()
        .spawn((
            SkinnedMesh {
                skeleton,
                joints: Vec::new(),
            },
            Transform::IDENTITY,
        ))
        .id();
    let cloth = app
        .world_mut()
        .spawn((
            ClothSimSource {
                asset: "assets/flag.cloth.ron".into(),
                solver: "flag".into(),
                blend: 0.5,
                ..ClothSimSource::default()
            },
            Transform::IDENTITY,
            ChildOf(owner),
        ))
        .id();
    let ball = app
        .world_mut()
        .spawn((
            SoftSimSource {
                asset: "ball.soft.ron".into(),
                solver: "ball".into(),
                ..SoftSimSource::default()
            },
            Transform::IDENTITY,
        ))
        .id();
    let missing = app
        .world_mut()
        .spawn((
            ClothSimSource {
                asset: "nope.cloth.ron".into(),
                ..ClothSimSource::default()
            },
            Transform::IDENTITY,
        ))
        .id();
    run(&mut app, 10);
    let sim = app.world().get::<ClothSim>(cloth).expect("sim inserted");
    assert_eq!(
        sim.skinned_mesh,
        Some(owner),
        "the owner comes from the ancestors"
    );
    assert_eq!(sim.section, Some(cloth), "the source entity shows the section");
    assert!((sim.blend - 0.5).abs() < 1e-6);
    let set = app.world().resource::<ClothSet>();
    assert!(set.get("flag").expect("cloth solver").stats().steps > 0);
    let softs = app.world().resource::<SoftSet>();
    assert!(softs.get("ball").expect("soft solver").cloth().stats().steps > 0);
    assert!(app.world().get::<SoftSim>(ball).is_some());
    assert!(
        !app.world()
            .get::<ClothOutput>(cloth)
            .expect("output")
            .cur
            .is_empty()
    );
    // A failed load is recorded once and leaves the entity without an instance.
    let state = app.world().get::<ClothSourceState>(missing).expect("state");
    assert_eq!(state.error.as_deref(), Some("cloth_asset_invalid"));
    assert!(app.world().get::<ClothSim>(missing).is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_hot_reload_rebuilds_the_solver_from_the_new_cook() {
    let (root, id) = project("cloth_sources_reload");
    let mut app = app(server(&root));
    let e = app
        .world_mut()
        .spawn((
            ClothSimSource {
                asset: "flag.cloth.ron".into(),
                solver: "flag".into(),
                ..ClothSimSource::default()
            },
            Transform::IDENTITY,
        ))
        .id();
    run(&mut app, 20);
    let before = Arc::clone(&app.world().get::<ClothSim>(e).expect("sim").asset);
    let steps = app
        .world()
        .resource::<ClothSet>()
        .get("flag")
        .expect("solver")
        .stats()
        .steps;
    assert!(steps > 5);
    // No reload: the same `Arc` stays and the solver keeps going.
    run(&mut app, 5);
    assert!(Arc::ptr_eq(
        &before,
        &app.world().get::<ClothSim>(e).expect("sim").asset
    ));

    // The cook wrote a new file and the watcher asks for the reload.
    write_cooked(
        &cooked_cache_dir(&root, "linux"),
        id,
        COOKED_CLOTH_TYPE_PATH,
        &cloth_payload(0.05),
    );
    app.world_mut()
        .resource_mut::<AssetServer>()
        .reload_id(id)
        .expect("reload");
    run(&mut app, 3);
    let after = Arc::clone(&app.world().get::<ClothSim>(e).expect("sim").asset);
    assert!(!Arc::ptr_eq(&before, &after), "the new cook replaced the asset");
    let reach = after.lod0().maps.get("reach").expect("reach")[12];
    assert!(
        (reach - 0.05).abs() < 1e-6,
        "the edit is in the new asset: {reach}"
    );
    let rebuilt = app
        .world()
        .resource::<ClothSet>()
        .get("flag")
        .expect("solver")
        .stats()
        .steps;
    assert!(rebuilt < steps, "the solver restarted ({rebuilt} < {steps})");
    let _ = std::fs::remove_dir_all(root);
}
