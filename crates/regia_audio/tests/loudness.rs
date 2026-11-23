// SPDX-License-Identifier: MPL-2.0
//! End-to-end loudness normalization (EBU R128 / ITU-R BS.1770-4): measure an asset, normalize
//! it to a target, re-measure and confirm it lands on the reference level. The DSP itself is
//! exercised in `regia_audio/src/loudness.rs`; this file checks the public re-exports and the
//! whole measure → gain → apply → re-measure loop.

use regia_audio::{REFERENCE_LUFS, measure_loudness, normalize_gain};

fn sine(hz: f32, seconds: f64, amp: f32) -> Vec<f32> {
    let len = (seconds * 48_000.0) as usize;
    (0..len)
        .map(|i| (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * amp)
        .collect()
}

#[test]
fn normalizing_two_assets_lands_both_on_the_reference_level() {
    for amp in [0.05f32, 0.4f32] {
        let samples = sine(1000.0, 3.0, amp);
        let measured = measure_loudness(&samples, 1, 48_000).expect("valid input");
        let gain = normalize_gain(measured.integrated_lufs, REFERENCE_LUFS);
        let normalized: Vec<f32> = samples.iter().map(|s| s * gain).collect();
        let again = measure_loudness(&normalized, 1, 48_000).expect("valid input");
        assert!(
            (again.integrated_lufs - REFERENCE_LUFS).abs() < 0.05,
            "amp {amp}: measured {}, gain {}, re-measured {}",
            measured.integrated_lufs,
            gain,
            again.integrated_lufs
        );
    }
}

#[test]
fn stereo_buffers_are_supported() {
    let mono = sine(440.0, 1.0, 0.1);
    let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
    let loud = measure_loudness(&stereo, 2, 48_000).expect("valid input");
    assert!(loud.integrated_lufs.is_finite());
    assert!(loud.true_peak_db.is_finite());
    assert!(loud.momentary_lufs.is_finite());
}

#[test]
fn normalization_never_returns_a_non_finite_gain() {
    assert_eq!(normalize_gain(f32::NAN, REFERENCE_LUFS), 1.0);
    assert_eq!(normalize_gain(f32::INFINITY, REFERENCE_LUFS), 1.0);
    // Clamped, not infinite, even for a pathological measurement.
    assert!(normalize_gain(-1000.0, REFERENCE_LUFS).is_finite());
}
