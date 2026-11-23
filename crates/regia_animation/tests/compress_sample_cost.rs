// SPDX-License-Identifier: MPL-2.0
//! Measurement, not a check: the per-call cost of `CompressedClip::sample` on the cooked demo
//! locomotion set (every gait of `demo_rig::demo_locomotion` on the demo humanoid, compressed with
//! the default options), next to `AnimationClip::sample_pose` on the same clips. Ignored: run it
//! by hand in a release build and read the printed numbers (no timing is asserted).
//!
//! ```text
//! cargo test --release -p regia_animation --test compress_sample_cost -- --ignored --nocapture
//! ```

use std::hint::black_box;
use std::time::Instant;

use regia_animation::clip_compress::{CompressOptions, compress};
use regia_animation::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};

/// Samples per clip per measured pass.
const SAMPLES: usize = 20_000;

#[test]
#[ignore = "measurement: run by hand with --release --nocapture"]
fn compressed_sample_cost_on_the_cooked_locomotion_set() {
    let Some(skeleton) = demo_humanoid(&DemoRig::default()) else {
        panic!("the demo rig builds");
    };
    let clips = demo_locomotion(&skeleton, &DemoGait::standard());
    assert!(!clips.is_empty());
    let options = CompressOptions::default();
    let mut pose = skeleton.bind_pose_pose();
    let mut total_compressed = 0.0f64;
    let mut total_raw = 0.0f64;
    println!(
        "skeleton: {} joints; {} clips; {SAMPLES} samples per clip per pass",
        skeleton.len(),
        clips.len()
    );
    for clip in &clips {
        let compressed = match compress(clip, &skeleton, &options) {
            Ok(compressed) => compressed,
            Err(error) => panic!("clip {:?} compresses: {error:?}", clip.name),
        };
        // A fixed, deterministic sweep of times over the clip (with a stride that is not a
        // divisor of the key spacing, so the searches land everywhere).
        let times: Vec<f32> = (0..SAMPLES)
            .map(|k| (k as f32 * 0.618_034 * clip.duration / 7.0).rem_euclid(clip.duration))
            .collect();
        // Warm both paths once.
        for &t in times.iter().take(256) {
            compressed.sample(t, &mut pose);
            clip.sample_pose(t, &mut pose);
        }
        let start = Instant::now();
        for &t in &times {
            compressed.sample(black_box(t), &mut pose);
            black_box(&pose);
        }
        let compressed_ns = start.elapsed().as_nanos() as f64 / SAMPLES as f64;
        let start = Instant::now();
        for &t in &times {
            clip.sample_pose(black_box(t), &mut pose);
            black_box(&pose);
        }
        let raw_ns = start.elapsed().as_nanos() as f64 / SAMPLES as f64;
        total_compressed += compressed_ns;
        total_raw += raw_ns;
        println!(
            "{:>12}: {} tracks, {:.2} s, ratio {:.3}: CompressedClip::sample {:.0} ns/call ({:.1} ns/track), AnimationClip::sample_pose {:.0} ns/call",
            clip.name,
            compressed.tracks.len(),
            clip.duration,
            compressed.report().ratio(),
            compressed_ns,
            compressed_ns / compressed.tracks.len().max(1) as f64,
            raw_ns
        );
    }
    let n = clips.len() as f64;
    println!(
        "mean: CompressedClip::sample {:.0} ns/call, AnimationClip::sample_pose {:.0} ns/call",
        total_compressed / n,
        total_raw / n
    );
}
