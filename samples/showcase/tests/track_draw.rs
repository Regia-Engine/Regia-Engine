// SPDX-License-Identifier: MIT-0
//! The tracked entity is drawn: with the track in the scene, a frame differs from the same frame without it
//! inside the entity's projected box, on the GPU-driven path (the film host's default) and on the old path;
//! a slow render-out (`fps 10`) moves the track at game speed (ADR-1426); a track with a glTF mesh draws the
//! mesh and swaps it at its frame. Without any GPU adapter each test prints a note and passes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use common::{JobSpec, Outcome, run, write_box_gltf};
use regia_render::RenderDevice;
use regia_test_utils::unique_temp_dir;
use showcase::example::write_example_scene;

const EYE: [f32; 3] = [60.0, 46.0, -170.0];
const TARGET: [f32; 3] = [0.0, 36.0, -100.0];

/// The track of a 14 m capsule that holds still at `TARGET`, with `extra` fields (mesh names).
fn still_track(extra: &str) -> String {
    format!(
        r#"(version: 1, entity: "hardware", capsule: (radius: 2.5, half_length: 4.5),
        color: (0.9, 0.1, 0.1), height_mode: Absolute, {extra}
        keys: [(frame: 0, position: (0.0, 36.0, -100.0), euler_deg: (0.0, 0.0, 0.0))])"#
    )
}

/// A track that starts 600 m above `TARGET`, falls onto it by frame 3 and holds.
const FALLING_TRACK: &str = r#"(version: 1, entity: "hardware", capsule: (radius: 2.5, half_length: 4.5),
        color: (0.9, 0.1, 0.1), height_mode: Absolute,
        keys: [(frame: 0, position: (0.0, 600.0, -100.0), euler_deg: (0.0, 0.0, 0.0)),
               (frame: 3, position: (0.0, 36.0, -100.0), euler_deg: (0.0, 0.0, 0.0))])"#;

/// A fresh example scene whose track is `track` (`None`: no track and no spawns that need one).
fn scene(tag: &str, track: Option<&str>) -> PathBuf {
    let dir = unique_temp_dir(tag);
    write_example_scene(&dir).expect("write the example scene");
    match track {
        Some(text) => std::fs::write(dir.join("track.ron"), text).expect("track"),
        None => {
            let text = std::fs::read_to_string(dir.join("scene.ron")).expect("scene.ron");
            let text = text.replace(r#"track: Some("track.ron")"#, "track: None");
            let text = text.replace(r#"spawns: Some("spawns.ron")"#, "spawns: None");
            std::fs::write(dir.join("scene.ron"), text).expect("scene.ron");
        }
    }
    dir
}

fn spec(dir: &Path) -> JobSpec {
    JobSpec {
        scene: Some(dir.to_path_buf()),
        camera: Some((EYE, TARGET)),
        ..JobSpec::default()
    }
}

fn differing_pixels(a: &Outcome, b: &Outcome) -> usize {
    a.first_frame
        .2
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.first_frame.2.as_chunks::<4>().0)
        .filter(|(p, q)| p != q)
        .count()
}

fn has_gpu(what: &str) -> bool {
    match RenderDevice::headless() {
        Ok(_) => true,
        Err(err) => {
            eprintln!("skip showcase {what} test: {err}");
            false
        }
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438, ADR-1440). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_tracked_entity_is_drawn_on_both_paths() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !has_gpu("track draw") {
        return;
    }
    let with = scene("showcase-track-with", Some(&still_track("")));
    let without = scene("showcase-track-without", None);
    for (gpu_driven, hiz) in [(false, false), (true, false), (true, true)] {
        let job = |dir: &Path| JobSpec {
            gpu_driven,
            hiz,
            ..spec(dir)
        };
        let drawn = differing_pixels(&run(1, &job(&with)), &run(2, &job(&without)));
        assert!(
            drawn > 200,
            "gpu_driven {gpu_driven} hiz {hiz}: only {drawn} pixels differ from the frame without the track"
        );
    }
    let _ = std::fs::remove_dir_all(with);
    let _ = std::fs::remove_dir_all(without);
}

#[test]
fn two_renders_of_a_scene_with_a_track_are_hash_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !has_gpu("track hash") {
        return;
    }
    let dir = scene("showcase-track-hash", Some(FALLING_TRACK));
    let job = JobSpec {
        frames: 4,
        gpu_driven: true,
        hiz: true,
        ..spec(&dir)
    };
    let (a, b) = (run(1, &job), run(2, &job));
    assert_eq!(
        a.hashes, b.hashes,
        "two runs of the same request give the same frames"
    );
    assert!(
        a.hashes.windows(2).any(|w| w[0] != w[1]),
        "the entity moves between frames"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_slow_render_out_moves_the_track_at_game_speed() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !has_gpu("slow render-out") {
        return;
    }
    let with = scene("showcase-track-slow-with", Some(FALLING_TRACK));
    let without = scene("showcase-track-slow-without", None);
    // `fps 10` gives each frame six 60 Hz steps: one frame of warm-up is six director steps, past the track's
    // frame 3, so the capsule rests at `TARGET`. With one step per frame (the old clock) it would still be
    // 400 m above the view.
    let job = |dir: &Path| JobSpec {
        fps: 10,
        warmup_frames: 1,
        gpu_driven: true,
        hiz: true,
        ..spec(dir)
    };
    let drawn = differing_pixels(&run(1, &job(&with)), &run(2, &job(&without)));
    assert!(
        drawn > 200,
        "only {drawn} pixels differ from the frame without the track"
    );
    let _ = std::fs::remove_dir_all(with);
    let _ = std::fs::remove_dir_all(without);
}

#[test]
fn a_track_with_a_mesh_draws_the_mesh_instead_of_the_capsule() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !has_gpu("track mesh") {
        return;
    }
    let capsule = scene("showcase-track-mesh-capsule", Some(&still_track("")));
    let boxed = scene(
        "showcase-track-mesh-box",
        Some(&still_track(r#"mesh: Some("meshes/hardware.gltf"),"#)),
    );
    write_box_gltf(&boxed, "meshes/hardware.gltf", [3.0, 3.0, 7.0], [0.1, 0.2, 0.9]);
    let without = scene("showcase-track-mesh-none", None);
    let job = |dir: &Path| JobSpec {
        gpu_driven: true,
        hiz: true,
        ..spec(dir)
    };
    let boxed_frame = run(1, &job(&boxed));
    let stats = boxed_frame.stats.as_ref().expect("stats");
    assert_eq!((stats.track_parts, stats.track_swapped), (1, false));
    assert!(
        differing_pixels(&boxed_frame, &run(2, &job(&without))) > 200,
        "the mesh is drawn"
    );
    let capsule_frame = run(3, &job(&capsule));
    assert!(
        differing_pixels(&boxed_frame, &capsule_frame) > 200,
        "the mesh replaces the capsule"
    );
    for dir in [capsule, boxed, without] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_entity_swaps_to_the_damaged_mesh_at_its_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !has_gpu("track swap") {
        return;
    }
    let swapping = scene(
        "showcase-track-swap",
        Some(&still_track(
            r#"mesh: Some("meshes/hardware.gltf"), swap_mesh: Some("meshes/hardware_damaged.gltf"), swap_frame: Some(8),"#,
        )),
    );
    let plain = scene(
        "showcase-track-noswap",
        Some(&still_track(r#"mesh: Some("meshes/hardware.gltf"),"#)),
    );
    for dir in [&swapping, &plain] {
        write_box_gltf(dir, "meshes/hardware.gltf", [3.0, 3.0, 7.0], [0.1, 0.2, 0.9]);
        write_box_gltf(
            dir,
            "meshes/hardware_damaged.gltf",
            [4.0, 1.5, 4.0],
            [0.9, 0.5, 0.1],
        );
    }
    let at = |dir: &Path, warmup: u64| {
        run(
            1,
            &JobSpec {
                warmup_frames: warmup,
                gpu_driven: true,
                hiz: true,
                ..spec(dir)
            },
        )
    };
    // Before the swap frame the two scenes draw the same frame; after it the damaged variant shows.
    let (swap_before, plain_before) = (at(&swapping, 2), at(&plain, 2));
    assert_eq!(
        swap_before.hashes, plain_before.hashes,
        "the intact mesh shows before the swap"
    );
    assert!(!swap_before.stats.as_ref().expect("stats").track_swapped);
    let (swap_after, plain_after) = (at(&swapping, 12), at(&plain, 12));
    assert!(swap_after.stats.as_ref().expect("stats").track_swapped);
    assert!(
        differing_pixels(&swap_after, &plain_after) > 200,
        "the damaged mesh shows after the swap"
    );
    assert!(
        differing_pixels(&swap_after, &swap_before) > 200,
        "the frame changes at the swap"
    );
    let _ = std::fs::remove_dir_all(swapping);
    let _ = std::fs::remove_dir_all(plain);
}
