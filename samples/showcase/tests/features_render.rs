// SPDX-License-Identifier: MIT-0
//! The feature scene through the renderer: eight frames at 640x360 on lavapipe (or whatever adapter the
//! machine has), twice from fresh apps, with identical frame hashes, with the grass layer, the water ribbon,
//! the valley mist, the entry streak and the contact map all bound. Without any GPU adapter the test prints
//! a note and passes, like the other render tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{JobSpec, run};
use regia_render::RenderDevice;
use regia_test_utils::unique_temp_dir;
use serde_json::json;
use showcase::example::write_feature_scene;

/// The two tests each open devices (the probe and the job's app): they run one at a time, so the NVIDIA
/// 615.71 driver never sees concurrent device creation in one process (ADR-1438, ADR-1439).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_feature_scene_renders_twice_with_identical_hashes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase feature render test: {err}");
        return;
    }
    let dir = unique_temp_dir("showcase-features-render");
    write_feature_scene(&dir).expect("write the feature scene");
    let spec = JobSpec {
        width: 640,
        height: 360,
        frames: 8,
        scene: Some(dir.clone()),
        contact_ao: true,
        after: vec![
            ("terrain.grass".into(), json!({})),
            ("water.state".into(), json!({})),
            ("render.get_valley_mist".into(), json!({})),
        ],
        ..JobSpec::default()
    };
    let a = run(1, &spec);
    let b = run(2, &spec);
    assert_eq!(
        a.hashes, b.hashes,
        "two runs of the same request give the same frames"
    );
    assert_eq!(a.sequence, b.sequence);

    let stats = a.stats.as_ref().expect("stats");
    assert!(
        stats.grass_installed,
        "grass layer bound: {:?}",
        stats.grass_error
    );
    assert!(stats.contact_installed, "contact map bound");
    assert_eq!(
        (stats.water_bodies, stats.entry_streaks, stats.mist),
        (1, 1, true)
    );

    let [grass, water, mist] = &a.reports[..] else {
        panic!("three reports, got {}", a.reports.len());
    };
    assert_eq!(grass["report"]["installed"], json!(true), "{grass}");
    assert_eq!(water["plugin"], json!(true), "{water}");
    assert_eq!(water["bodies"].as_array().map(Vec::len), Some(1), "{water}");
    assert_eq!(mist["enabled"], json!(true), "{mist}");
    assert!(!mist["grid"].is_null(), "the mist has a height grid: {mist}");

    // The features draw something: with them all switched off over the agent protocol the frames change.
    let off = run(
        3,
        &JobSpec {
            calls: vec![
                ("terrain.set_grass".into(), json!({"enabled": false})),
                ("render.set_valley_mist".into(), json!({"enabled": false})),
                ("water.set".into(), json!({"enabled": false})),
            ],
            after: Vec::new(),
            ..spec.clone()
        },
    );
    assert_ne!(a.hashes, off.hashes, "grass, mist and water change the picture");
    let _ = std::fs::remove_dir_all(dir);
}

/// Mean absolute difference per channel between two RGBA8 frames of the same size.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "frames of one size");
    let total: u64 = a.iter().zip(b).map(|(x, y)| u64::from(x.abs_diff(*y))).sum();
    total as f64 / a.len() as f64
}

/// The valley mist reaches a headless frame (ADR-1453). The headless app draws only through the agent's
/// offscreen capture, which once never handed the mist to the renderer: the fog pass never ran and a
/// mist of any density left every pixel as it was. Two fresh apps over the feature scene, one with a strong mist
/// and one with the resource removed, must differ, and a thicker mist must differ from the thin one.
#[test]
fn the_valley_mist_changes_a_headless_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase mist render test: {err}");
        return;
    }
    let dir = unique_temp_dir("showcase-mist-render");
    write_feature_scene(&dir).expect("write the feature scene");
    let frame = |id: u32, call: serde_json::Value| {
        run(
            id,
            &JobSpec {
                width: 320,
                height: 180,
                frames: 2,
                scene: Some(dir.clone()),
                calls: vec![("render.set_valley_mist".into(), call)],
                ..JobSpec::default()
            },
        )
    };
    let removed = frame(11, json!({"enabled": false}));
    let thin = frame(12, json!({"density": 0.01, "open_ground_fraction": 0.5}));
    let thick = frame(13, json!({"density": 0.08, "open_ground_fraction": 0.5}));
    assert_ne!(removed.hashes, thin.hashes, "a thin mist changes the frame");
    assert_ne!(thin.hashes, thick.hashes, "a thicker mist changes it again");
    let (_, _, removed_px) = &removed.first_frame;
    let (_, _, thin_px) = &thin.first_frame;
    let (_, _, thick_px) = &thick.first_frame;
    let thin_diff = mean_abs_diff(removed_px, thin_px);
    let thick_diff = mean_abs_diff(removed_px, thick_px);
    eprintln!("mist vs none: thin {thin_diff:.3}, thick {thick_diff:.3} levels");
    assert!(thin_diff > 0.05, "a thin mist is visible: {thin_diff}");
    assert!(
        thick_diff > 2.0 * thin_diff,
        "mist grows with density: {thin_diff} then {thick_diff}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
