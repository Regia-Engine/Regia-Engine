// SPDX-License-Identifier: MPL-2.0
//! VFX and decal live reload (`docs/specs/12-assets.md` § 8): editing a `.vfx.ron` replaces the
//! system in place (same index, running instances continue), editing a `.decal.ron` rewrites the
//! pool slot of every static decal placed from it — each observed within one second of the write.
//! A file that does not parse keeps the previous definition.

// The source watcher API is desktop-only, including when dev-assets is enabled.
#![cfg(not(target_os = "android"))]

use std::path::Path;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, SourceKind};
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_vfx::decal::DecalPool;
use regia_vfx::decal_format::StaticDecal;
use regia_vfx::{VfxPlugin, VfxScene, VfxSettings};

const VFX: &str = include_str!("fixtures/muzzle_flash.vfx.ron");
const DECAL: &str = include_str!("../../../docs/formats/decal.example.ron");

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_file_name(format!(
        ".regia-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

fn update_until(app: &mut App, mut done: impl FnMut(&App) -> bool) -> Duration {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        app.update();
        if done(app) {
            return start.elapsed();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no reload within 10 s");
}

fn drag(app: &App) -> f32 {
    let scene = app.world().resource::<VfxScene>();
    let index = scene.library().index("muzzle_flash").expect("loaded");
    scene.library().def(index).emitters[0].update.drag
}

#[test]
fn vfx_and_decal_edits_reload_within_one_second() {
    let root = std::env::temp_dir().join(format!("regia_vfx_hot_{}", std::process::id()));
    let dir = root.join("assets").join("vfx");
    std::fs::create_dir_all(&dir).expect("dirs");
    let vfx_path = dir.join("muzzle_flash.vfx.ron");
    let decal_path = dir.join("scorch.decal.ron");
    std::fs::write(&vfx_path, VFX).expect("vfx");
    std::fs::write(&decal_path, DECAL).expect("decal");

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
    app.insert_resource(VfxSettings {
        library_dir: Some(dir.clone()),
        ..VfxSettings::default()
    });
    app.add_plugins(VfxPlugin);
    app.world_mut().spawn((
        StaticDecal {
            prefab: "scorch".into(),
            yaw: 0.0,
            receiver_mask: 0,
        },
        Transform::from_translation(Vec3::new(1.0, 0.0, 2.0)),
    ));
    let _watch = regia_asset::hot_reload::spawn_source_forwarder(
        std::slice::from_ref(&dir),
        Duration::from_millis(80),
        |kind| matches!(kind, SourceKind::Vfx | SourceKind::Decal),
        tx,
    )
    .expect("watch");
    app.update();
    assert!((drag(&app) - 6.0).abs() < 1e-6);
    let index = app.world().resource::<VfxScene>().library().index("muzzle_flash");
    // The library also holds the engine's built-in effects.
    let defs_before = app.world().resource::<VfxScene>().library().defs().len();
    let pool = app.world().resource::<DecalPool>();
    assert_eq!(pool.len(), 1, "the static decal is placed");
    let half_before = pool.decals()[0].half_size;

    // VFX: same index, new parameters.
    let start = Instant::now();
    write_atomically(&vfx_path, &VFX.replace("drag: 6.0", "drag: 2.0"));
    let took = update_until(&mut app, |app| (drag(app) - 2.0).abs() < 1e-6).max(start.elapsed());
    assert_reload_budget(took, "vfx reload");
    assert_eq!(
        app.world().resource::<VfxScene>().library().index("muzzle_flash"),
        index
    );
    assert_eq!(
        app.world().resource::<VfxScene>().library().defs().len(),
        defs_before,
        "replaced, not appended"
    );

    // Decal: the placed decal's slot is rewritten.
    let start = Instant::now();
    write_atomically(
        &decal_path,
        &DECAL.replace("half_size: (1.4, 1.4, 0.35)", "half_size: (2.8, 2.8, 0.35)"),
    );
    let took = update_until(&mut app, |app| {
        app.world().resource::<DecalPool>().decals()[0].half_size != half_before
    })
    .max(start.elapsed());
    assert_reload_budget(took, "decal reload");
    let pool = app.world().resource::<DecalPool>();
    assert_eq!(pool.len(), 1, "rewritten in place");
    assert!((pool.decals()[0].half_size.x - half_before.x * 2.0).abs() < 1e-4);

    // A broken VFX file: reported, the previous definition stays.
    let errors = app.world().resource::<AssetWatchStatus>().errors;
    write_atomically(&vfx_path, "( version: 1, name: ");
    update_until(&mut app, |app| {
        app.world().resource::<AssetWatchStatus>().errors > errors
    });
    assert!((drag(&app) - 2.0).abs() < 1e-6, "old definition kept");

    let _ = std::fs::remove_dir_all(&root);
}

/// The spec's one-second reload budget, asserted in release builds only: a debug test binary on a
/// loaded machine measures the scheduler, not the reload (audit-v035-flaky H1). Debug runs print it.
fn assert_reload_budget(took: Duration, what: &str) {
    eprintln!("{what} took {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < Duration::from_secs(1), "{what} took {took:?}");
    }
}
