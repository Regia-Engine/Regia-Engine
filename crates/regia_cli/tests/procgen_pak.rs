// SPDX-License-Identifier: MPL-2.0
//! Procedural bakes ship in the pak (spec 62 § 8, ADR-7214): the cook stores each `.pgout` byte for
//! byte under `regia_procgen::pgout::PGOUT_COOKED_TYPE_PATH` (an `editor_only` generator's bake
//! stays out), the pak's manifest lists it, and a game with no loose files (Android reads nothing
//! loose from its APK) loads the bake through the asset server. Android cook, pak reader only: no
//! device needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, AssetSource, PakReader};
use regia_asset_pipeline::{CookOptions, cook_project, pack_cooked_directory};
use regia_core::id::Guid;
use regia_procgen::exec::{Collision, Instance, InstanceSet, RunOutput};
use regia_procgen::pgout::{PgoutHeader, encode};
use regia_procgen::world::{
    ProcGenMode, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource, ProcOutput,
    level_bake_dir,
};
use regia_scene::components::EntityGuid;
use regia_scene::format::{SceneEntity, SceneFile};

const KEPT: Guid = Guid(0xa1);
const DROPPED: Guid = Guid(0xb2);

fn bake() -> Vec<u8> {
    let inst = |x: f32| Instance {
        translation: [x, 0.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        color: [1.0; 4],
        custom: [0.0; 4],
    };
    let output = RunOutput {
        instance_sets: vec![InstanceSet {
            mesh: "rock.glb".into(),
            material: None,
            cast_shadows: true,
            collision: Collision::None,
            draw_distance_m: 100.0,
            fade_m: 0.0,
            cull_below_px: 2.0,
            instances: vec![inst(1.0), inst(2.0), inst(3.0)],
            layer: Some("quest".into()),
        }],
        ..RunOutput::default()
    };
    let header = PgoutHeader {
        graph_hash: [1; 32],
        param_hash: [2; 32],
        seed: 3,
        input_hash: [4; 32],
        output_hash: output.hash(),
        quality: 5,
    };
    encode(&header, &output)
}

fn generator_record(guid: Guid, editor_only: bool) -> SceneEntity {
    use regia_ecs::reflect::TypePath;
    let mut components = BTreeMap::new();
    components.insert(
        ProcGenerator::type_path().to_string(),
        ron::value::RawValue::from_boxed_ron(
            format!("(graph: \"g.pgraph.ron\", editor_only: {editor_only})").into_boxed_str(),
        )
        .expect("raw ron"),
    );
    SceneEntity {
        guid,
        name: None,
        parent: None,
        components,
        prefab_instance: None,
    }
}

/// A project with one level, a shipping generator and an `editor_only` one, and a bake for each.
fn project() -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir("procgen-pak");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let mut scene = SceneFile::new();
    scene.entities = vec![generator_record(KEPT, false), generator_record(DROPPED, true)];
    std::fs::write(
        assets.join("grove.level.ron"),
        scene.to_ron_string().expect("level text"),
    )
    .expect("level");
    let bakes = assets.join(level_bake_dir(Path::new("grove.level.ron")));
    std::fs::create_dir_all(&bakes).expect("bake dir");
    std::fs::write(bakes.join(format!("{KEPT}.pgout")), bake()).expect("kept bake");
    std::fs::write(bakes.join(format!("{DROPPED}.pgout")), bake()).expect("dropped bake");
    (root, bakes)
}

#[test]
fn the_pak_lists_shipping_bakes_and_the_game_loads_them_from_it() {
    let (root, bakes) = project();
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: "android".to_string(),
        incremental: false,
    })
    .expect("cook");
    assert!(
        !report.errors.iter().any(|(p, _)| p.ends_with(".pgout")),
        "{:?}",
        report.errors
    );
    let cache = regia_asset::cooked_cache_dir(&root, "android");
    let pak_dir = root.join("build/paks/android");
    std::fs::create_dir_all(&pak_dir).expect("pak dir");
    let paks = pack_cooked_directory(&cache, &pak_dir, &BTreeMap::new(), true).expect("pack");
    assert_eq!(paks.len(), 1);

    let kept_path = format!("assets/grove.procgen/{KEPT}.pgout");
    let dropped_path = format!("assets/grove.procgen/{DROPPED}.pgout");
    let pak = PakReader::open(&paks[0]).expect("open pak");
    let listed: Vec<&str> = pak.manifest().iter().map(|e| e.path.as_str()).collect();
    assert!(listed.contains(&kept_path.as_str()), "{listed:?}");
    assert!(
        !listed.contains(&dropped_path.as_str()),
        "an editor_only generator's bake stays out: {listed:?}"
    );
    drop(pak);

    // The game: no loose bake and no cooked cache, only the pak.
    std::fs::remove_dir_all(&bakes).expect("remove loose bakes");
    std::fs::remove_dir_all(&cache).expect("remove the cooked cache");
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "android".to_string(),
        content_dir: Some(pak_dir.clone()),
    })
    .expect("asset server on the pak");
    let id = server
        .id_for_path(&kept_path)
        .expect("the manifest knows the bake");

    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        bake_dir: Some(PathBuf::from("assets").join(level_bake_dir(Path::new("grove.level.ron")))),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(server);
    let generator = app
        .world_mut()
        .spawn((
            ProcGenerator {
                mode: ProcGenMode::Baked,
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(KEPT),
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }
    let output = app
        .world()
        .get::<ProcOutput>(generator)
        .expect("the bake loaded from the pak");
    assert_eq!(output.0.instance_count(), 3);
    assert_eq!(output.0.instance_sets[0].layer.as_deref(), Some("quest"));
    assert_eq!(
        app.world().get::<ProcGenState>(generator).map(|s| s.evaluations),
        Some(0),
        "loaded, not run"
    );
    assert_eq!(
        app.world().resource::<AssetServer>().source_of(id),
        AssetSource::Pak("base".to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}
