// SPDX-License-Identifier: MIT-0
//! Default-look goldens: the four DO27-like shots of `lookdev::default_look` on the
//! runtime defaults (Desktop High profile, default post stack, default fog), lavapipe,
//! 320×180 at a fixed sunny-day EV100 (auto exposure reads the histogram back
//! asynchronously, so it is not frame-exact). Bless with `REGIA_BLESS=1`.
//!
//! Each shot is captured at the settled frame `SETTLED_GOLDEN_FRAME` (64) of a fresh
//! renderer, not frame 1: the probe GI and the anti-aliasing history settle at frame 40,
//! and a first frame is shown for one frame only (ADR-5990). The same run draws
//! `SETTLE_CHECK_FRAMES` more frames and requires the later frame to equal the capture,
//! so a change that moves the settle point past the capture fails here instead of
//! quietly pinning a frame that is still moving.
//!
//! Each shot also has a guard on the properties the default look exists for: not
//! clipped, not crushed, not flat.

use std::path::PathBuf;

use lookdev::default_look::{
    LookOptions, LookShot, SETTLE_CHECK_FRAMES, SETTLED_GOLDEN_FRAME, render_shot_captures,
};
use regia_render::{FrameRenderer, RenderDevice};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, guard_golden};

fn golden(shot: LookShot) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{}.png", shot.name()))
}

/// Standard deviation of Rec.709 luma (0–255): a flat, washed-out frame scores low.
fn luma_spread(pixels: &[u8]) -> f64 {
    let lumas: Vec<f64> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let n = lumas.len().max(1) as f64;
    let mean = lumas.iter().sum::<f64>() / n;
    (lumas.iter().map(|l| (l - mean) * (l - mean)).sum::<f64>() / n).sqrt()
}

/// (pixels that differ, largest channel difference) between two RGBA8 frames.
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    let mut count = 0;
    let mut worst = 0u8;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        count += usize::from(d > 0);
        worst = worst.max(d);
    }
    (count, worst)
}

#[test]
fn default_look_shots_match_lavapipe_goldens() {
    let device =
        RenderDevice::headless().expect("default-look goldens need a GPU; lavapipe is the reference");
    let options = LookOptions::settled_golden();
    let capture = options.frames;
    let later = capture + SETTLE_CHECK_FRAMES;
    assert_eq!(capture, SETTLED_GOLDEN_FRAME);
    let mut failed = Vec::new();
    for shot in LookShot::ALL {
        let mut renderer = FrameRenderer::new();
        let mut frames = render_shot_captures(&device, &mut renderer, shot, options, &[capture, later])
            .expect("render shot");
        let after = frames.pop().expect("later frame");
        let pixels = frames.pop().expect("captured frame");
        // The capture must be settled: the frame SETTLE_CHECK_FRAMES later is the same bytes.
        let (differ, worst) = diff(&pixels, &after);
        eprintln!(
            "default look {}: frame {capture} vs frame {later}: {differ} pixels differ (worst {worst})",
            shot.name()
        );
        if differ > 0 {
            failed.push(format!(
                "{}: frame {capture} is not settled: {differ} pixels differ from frame {later} (worst {worst})",
                shot.name()
            ));
        }
        guard_golden(
            &pixels,
            options.width,
            options.height,
            shot.name(),
            GoldenGuard {
                min_lum: 40.0,
                max_lum: 200.0,
                max_white_pct: 4.0,
                max_black_pct: 25.0,
                min_object_pct: 50.0,
            },
        );
        let spread = luma_spread(&pixels);
        eprintln!("default look {}: luma spread {spread:.1}", shot.name());
        assert!(
            spread > 20.0,
            "{}: luma spread {spread:.1}: the frame is flat (no light/shadow separation)",
            shot.name()
        );
        // Every shot is compared before the test fails: stopping at the first one hid the
        // cockpit's change behind props_on_snow at the v0.1.29 gate (ADR-1410).
        if let Err(err) = compare_rgba8_to_png_on(
            &device.adapter.get_info(),
            &pixels,
            options.width,
            options.height,
            &golden(shot),
            DEFAULT_SSIM_THRESHOLD,
        ) {
            eprintln!("{} golden: {err:#}", shot.name());
            failed.push(format!("{} golden: {err:#}", shot.name()));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("; "));
}
