// SPDX-License-Identifier: MPL-2.0
//! Terrain layer-stack and scatter-rule live reload (`docs/specs/12-assets.md` § 8): editing a
//! `.terrain.layers.ron` updates the [`Terrain`] that uses it, editing a loaded `.scatter.ron`
//! replaces its [`ScatterSets`] entry — each within one second; an invalid file keeps the old
//! data.

use std::path::Path;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{
    AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, Heightfield, SourceKind,
};
use regia_terrain::cdlod::CdlodConfig;
use regia_terrain::{ScatterSets, Terrain, TerrainLayers, TerrainLayersSource, TerrainPlugin};

const LAYERS: &str = include_str!("fixtures/ground.terrain.layers.ron");
const SCATTER: &str = include_str!("fixtures/rocks.scatter.ron");

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_file_name(format!(
        ".regia-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

/// Polls until `done` holds; the 3 s bound only turns a hang into a failure, it is not the contract.
fn wait(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        app.update();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no reload within 3 s");
}

fn roughness(app: &mut App) -> f32 {
    let mut q = app.world_mut().query::<&Terrain>();
    q.single(app.world()).expect("terrain").layers.layers[0].roughness
}

#[test]
fn layer_and_scatter_edits_reload() {
    let root = std::env::temp_dir().join(format!("regia_terrain_hot_{}", std::process::id()));
    let dir = root.join("assets").join("maps");
    std::fs::create_dir_all(&dir).expect("dirs");
    let layers_path = dir.join("ground.terrain.layers.ron");
    let scatter_path = dir.join("rocks.scatter.ron");
    std::fs::write(&layers_path, LAYERS).expect("layers");
    std::fs::write(&scatter_path, SCATTER).expect("scatter");

    let (tx, inbox) = AssetReloadInbox::channel();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(inbox);
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    app.add_plugins(TerrainPlugin);
    let layers = TerrainLayers::from_ron(LAYERS).expect("layers");
    let n = 33;
    let terrain = Terrain::new(
        &Heightfield {
            width: n,
            height: n,
            samples: vec![32_768; (n * n) as usize],
        },
        64.0,
        10.0,
        0.0,
        CdlodConfig::default(),
        2,
        layers,
    )
    .expect("terrain");
    app.world_mut()
        .spawn((terrain, TerrainLayersSource(layers_path.clone())));
    let seed_before = app
        .world_mut()
        .resource_mut::<ScatterSets>()
        .load(&scatter_path)
        .expect("scatter")
        .seed;
    let _watch = regia_asset::hot_reload::spawn_source_forwarder(
        std::slice::from_ref(&dir),
        Duration::from_millis(80),
        |kind| matches!(kind, SourceKind::TerrainLayers | SourceKind::Scatter),
        tx,
    )
    .expect("watch");
    app.update();
    assert!((roughness(&mut app) - 0.95).abs() < 1e-6);

    write_atomically(
        &layers_path,
        &LAYERS.replace("roughness: 0.95", "roughness: 0.40"),
    );
    wait(&mut app, |app| (roughness(app) - 0.40).abs() < 1e-6);

    write_atomically(&scatter_path, &SCATTER.replace("seed: 2747", "seed: 9001"));
    wait(&mut app, |app| {
        app.world()
            .resource::<ScatterSets>()
            .get(&scatter_path)
            .is_some_and(|s| s.seed == 9001)
    });
    assert_ne!(seed_before, 9001);
    assert_eq!(
        app.world().resource::<ScatterSets>().revision(&scatter_path),
        Some(1)
    );

    // An invalid layer stack (roughness out of range): reported, the old stack stays.
    let errors = app.world().resource::<AssetWatchStatus>().errors;
    write_atomically(&layers_path, &LAYERS.replace("roughness: 0.95", "roughness: 7.0"));
    wait(&mut app, |app| {
        app.world().resource::<AssetWatchStatus>().errors > errors
    });
    assert!((roughness(&mut app) - 0.40).abs() < 1e-6, "old layers kept");

    let _ = std::fs::remove_dir_all(&root);
}
