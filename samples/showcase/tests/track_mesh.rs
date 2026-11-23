// SPDX-License-Identifier: MIT-0
//! A track that names a glTF mesh (and a swap mesh): the files load with their codes, the director swaps the
//! entity at its frame and not before, a slow frame runs every 60 Hz step of its time. No GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::write_box_gltf;
use regia::prelude::*;
use regia_app::enable_fixed_step;
use regia_render::MeshRenderer;
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;
use showcase::app::{Options, ShowcaseClock, ShowcasePlugin, ShowcaseStats, Tracked, pin_fixed_step};
use showcase::example::write_example_scene;
use showcase::scene::{SceneData, load_scene};
use showcase::{ErrorCode, STEP_HZ, ShowcaseError};

/// A fresh example scene whose track holds still on the ground and names `extra` fields.
fn scene_dir(tag: &str, extra: &str) -> PathBuf {
    let dir = unique_temp_dir(tag);
    write_example_scene(&dir).expect("write the example scene");
    let track = format!(
        r#"(version: 1, entity: "hardware", capsule: (radius: 2.5, half_length: 4.5), {extra}
        height_mode: Absolute,
        keys: [(frame: 0, position: (0.0, 40.0, -100.0)), (frame: 60, position: (10.0, 40.0, -100.0))])"#
    );
    std::fs::write(dir.join("track.ron"), track).expect("track");
    dir
}

fn err(dir: &Path) -> ShowcaseError {
    load_scene(dir).expect_err("the scene must be refused")
}

#[test]
fn a_track_mesh_loads_beside_the_scene() {
    let dir = scene_dir("showcase-track-mesh-load", r#"mesh: Some("meshes/hw.gltf"),"#);
    write_box_gltf(&dir, "meshes/hw.gltf", [3.0, 3.0, 7.0], [0.2, 0.2, 0.25]);
    let scene = load_scene(&dir).expect("loads");
    let base = scene.track_meshes.base.expect("the track's mesh");
    assert_eq!(base.parts.len(), 1);
    assert_eq!(base.triangles_lod0, 12);
    assert!(scene.track_meshes.swap.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_track_without_a_mesh_keeps_the_capsule() {
    let dir = scene_dir("showcase-track-capsule", "");
    let scene = load_scene(&dir).expect("loads");
    assert!(scene.track_meshes.base.is_none() && scene.track_meshes.swap.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bad_track_meshes_have_their_codes() {
    // A named file that is not there.
    let dir = scene_dir("showcase-track-mesh-bad", r#"mesh: Some("meshes/hw.gltf"),"#);
    assert_eq!(err(&dir).code, ErrorCode::TrackMeshMissing);
    // A file that is not a glTF.
    std::fs::create_dir_all(dir.join("meshes")).expect("mkdir");
    std::fs::write(dir.join("meshes/hw.gltf"), "not a gltf").expect("write");
    assert_eq!(err(&dir).code, ErrorCode::TrackMeshMalformed);
    // The swap mesh is checked the same way.
    write_box_gltf(&dir, "meshes/hw.gltf", [1.0, 1.0, 1.0], [0.5, 0.5, 0.5]);
    std::fs::write(
        dir.join("track.ron"),
        r#"(version: 1, entity: "a", mesh: Some("meshes/hw.gltf"), swap_mesh: Some("meshes/gone.glb"), swap_frame: Some(5),
            keys: [(frame: 0, position: (0.0, 0.0, 0.0))])"#,
    )
    .expect("track");
    let missing = err(&dir);
    assert_eq!(missing.code, ErrorCode::TrackMeshMissing);
    assert!(
        missing.path.ends_with("meshes/gone.glb"),
        "{}",
        missing.path.display()
    );
    // Names the track file refuses before any file is read: wrong extension, a swap without its frame, paths
    // that leave the scene directory.
    for fields in [
        r#"mesh: Some("meshes/hw.png"),"#,
        r#"swap_mesh: Some("meshes/hw.gltf"),"#,
        r#"swap_frame: Some(5),"#,
        r#"mesh: Some("../hw.gltf"),"#,
        r#"mesh: Some("/etc/hw.gltf"),"#,
    ] {
        std::fs::write(
            dir.join("track.ron"),
            format!(r#"(version: 1, entity: "a", {fields} keys: [(frame: 0, position: (0.0, 0.0, 0.0))])"#),
        )
        .expect("track");
        assert_eq!(err(&dir).code, ErrorCode::TrackMalformed, "{fields}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn app(scene: Arc<SceneData>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    pin_fixed_step(&mut app);
    app.add_plugins(ShowcasePlugin {
        scene,
        options: Options {
            plain: true,
            ..Options::default()
        },
    });
    app
}

/// The meshes the tracked entity wears now.
fn worn(app: &mut App) -> Vec<Arc<regia_render::CpuMesh>> {
    let mut query = app.world_mut().query::<(&Tracked, &MeshRenderer)>();
    query
        .iter(app.world())
        .map(|(_, r)| Arc::clone(&r.mesh))
        .collect()
}

#[test]
fn the_director_swaps_the_entity_at_its_frame() {
    let dir = scene_dir(
        "showcase-track-swap-cpu",
        r#"mesh: Some("meshes/hw.gltf"), swap_mesh: Some("meshes/hw_damaged.gltf"), swap_frame: Some(10),"#,
    );
    write_box_gltf(&dir, "meshes/hw.gltf", [3.0, 3.0, 7.0], [0.2, 0.2, 0.25]);
    write_box_gltf(&dir, "meshes/hw_damaged.gltf", [4.0, 1.0, 4.0], [0.3, 0.1, 0.05]);
    let scene = Arc::new(load_scene(&dir).expect("loads"));
    let base = Arc::clone(&scene.track_meshes.base.as_ref().expect("base").parts[0].mesh);
    let swap = Arc::clone(&scene.track_meshes.swap.as_ref().expect("swap").parts[0].mesh);
    assert!(!Arc::ptr_eq(&base, &swap));

    let mut app = app(Arc::clone(&scene));
    let mut seen = Vec::new();
    for _ in 0..16 {
        app.update();
        // The director ran frame `clock - 1` in this update.
        let frame = app.world().resource::<ShowcaseClock>().frame - 1;
        let meshes = worn(&mut app);
        assert_eq!(meshes.len(), 1, "one entity per material part at frame {frame}");
        let swapped = Arc::ptr_eq(&meshes[0], &swap);
        assert!(
            swapped || Arc::ptr_eq(&meshes[0], &base),
            "frame {frame} wears neither mesh"
        );
        assert_eq!(swapped, frame >= 10, "frame {frame}");
        let stats = app.world().resource::<ShowcaseStats>().clone();
        assert_eq!(
            (stats.track_swapped, stats.track_parts),
            (swapped, 1),
            "frame {frame}"
        );
        // The swapped entity keeps following the track.
        let want = scene.track.as_ref().expect("track").sample(frame).position;
        let mut query = app.world_mut().query::<(&Tracked, &Transform)>();
        let at = query.iter(app.world()).next().expect("entity").1.translation;
        assert!((at - want).length() < 1e-3, "frame {frame}: {at:?} vs {want:?}");
        seen.push(swapped);
    }
    assert_eq!(
        seen.iter().filter(|s| **s).count(),
        6,
        "swapped on frames 10 to 15 only"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Director steps run by `updates` updates of `step` game time each (after the start-up update), with the fixed step
/// pinned the way the host does or only by `enable_fixed_step`.
fn steps_run(pinned: bool, step: Duration, updates: usize) -> i64 {
    let dir = scene_dir("showcase-track-clock", "");
    let scene = Arc::new(load_scene(&dir).expect("loads"));
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    if pinned {
        pin_fixed_step(&mut app);
    } else {
        enable_fixed_step(&mut app, STEP_HZ);
    }
    app.add_plugins(ShowcasePlugin {
        scene,
        options: Options {
            plain: true,
            ..Options::default()
        },
    });
    // The first update runs Startup; a render-out then sets its own step.
    app.update();
    let before = app.world().resource::<ShowcaseClock>().frame;
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    for _ in 0..updates {
        app.update();
    }
    let after = app.world().resource::<ShowcaseClock>().frame;
    let _ = std::fs::remove_dir_all(dir);
    after - before
}

#[test]
fn a_slow_frame_runs_every_director_step_of_its_time() {
    // `render.out_start` at 10 fps steps the clock by 100 ms a frame: six 60 Hz director steps per frame, so 60
    // for ten frames (the step is not a whole number of nanoseconds, so one frame may run five).
    let slow = steps_run(true, Duration::from_millis(100), 10);
    assert!((59..=60).contains(&slow), "ten frames at 10 fps ran {slow} steps");
    // At 60 fps one step per frame, as before.
    assert_eq!(steps_run(true, Duration::from_secs_f64(1.0 / 60.0), 10), 10);
    // Without the cap raised the director ran once per frame whatever the frame length: the bug.
    assert_eq!(steps_run(false, Duration::from_millis(100), 10), 10);
}
