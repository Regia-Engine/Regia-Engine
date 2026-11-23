// SPDX-License-Identifier: MIT-0
//! `render_lab post_volumes`: a camera walking into a convex-hull post volume with a warm LUT.
//! The volume's weight at the eye rises from 0 outside the band to 1 inside, and the frames get
//! warmer with it (more red, less blue). Frames are compared with each other, so no golden image
//! is involved (lavapipe or any adapter).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// `(weight, mean red, mean blue)` per step, from the scene's stdout lines.
fn steps(stdout: &str) -> Vec<(f32, f32, f32)> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("post_volumes: "))
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let value = |key: &str| -> f32 {
                let i = words.iter().position(|w| *w == key).expect(key);
                words[i + 1].parse().expect(key)
            };
            (value("weight"), value("mean_red"), value("mean_blue"))
        })
        .collect()
}

#[test]
fn walking_into_a_hull_volume_warms_the_frame_with_its_weight() {
    let dir = regia_test_utils::unique_temp_dir("render-lab-post-volumes");
    let out = dir.join("frames");
    let output = Command::new(env!("CARGO_BIN_EXE_render_lab"))
        .current_dir(&dir)
        .args(["post_volumes", "--out"])
        .arg(&out)
        .output()
        .expect("spawn render_lab");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "render_lab failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    let steps = steps(&stdout);
    assert_eq!(steps.len(), 5, "one line per step\n{stdout}");
    for step in 0..5 {
        let len = std::fs::metadata(out.join(format!("post_volumes_{step}.png")))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(len > 0, "frame {step} written\n{stdout}");
    }

    let weights: Vec<f32> = steps.iter().map(|s| s.0).collect();
    assert_eq!(weights[0], 0.0, "outside the band\n{stdout}");
    assert_eq!(weights[4], 1.0, "inside the wedge\n{stdout}");
    assert!(
        weights.windows(2).all(|w| w[0] <= w[1]),
        "the weight rises along the walk: {weights:?}"
    );
    assert!(
        weights[1..4].iter().all(|w| *w > 0.0 && *w < 1.0),
        "the middle steps are in the band: {weights:?}"
    );
    let (_, red_out, blue_out) = steps[0];
    let (_, red_in, blue_in) = steps[4];
    assert!(
        red_in > red_out + 3.0 && blue_in + 3.0 < blue_out,
        "inside is warmer: red {red_out} -> {red_in}, blue {blue_out} -> {blue_in}"
    );
    // A half-weighted step lands between the outside and inside looks.
    let mid = steps
        .iter()
        .find(|s| s.0 > 0.25 && s.0 < 0.75)
        .copied()
        .expect("a step near half weight");
    assert!(
        mid.1 > red_out && mid.1 < red_in,
        "the band blends: red {red_out} < {} < {red_in}",
        mid.1
    );
    let _ = std::fs::remove_dir_all(&dir);
}
