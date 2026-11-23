// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the hashed alpha thresholds (ADR-1489).

use super::*;

const N: usize = 96;

fn image(f: impl Fn(usize, usize) -> f32) -> Vec<f32> {
    (0..N * N).map(|i| f(i % N, i / N)).collect()
}

/// `threshold` is the noise phase. A magnified texel (`minified = 0`) compares `cutoff`,
/// so an alpha between the phase and the cutoff is kept by `alpha < phase` and rejected
/// by the shader's `alpha < mix(cutoff, phase, 0)` ([`rejects`], its CPU twin, pinned to the
/// shader text below). A fully minified texel compares the phase.
#[test]
fn threshold_is_the_phase_and_a_magnified_texel_compares_cutoff() {
    let mut saw_split = false;
    let frag = [3.5, 7.5];
    for frame in 0..16 {
        let noise = threshold(frag, frame as f32, true);
        let phase = ign_phase(frag);
        let expect = (phase + GOLDEN_STEP * frame as f32).fract();
        assert_eq!(noise.to_bits(), expect.to_bits());
        let (cutoff, alpha) = (0.5, 0.3);
        // A fully minified texel compares the phase.
        assert_eq!(
            rejects(alpha, cutoff, 1.0, frag, frame as f32, true, true),
            alpha < noise,
            "frame {frame}"
        );
        // The hash off compares the cutoff whatever the minification.
        assert!(rejects(alpha, cutoff, 1.0, frag, frame as f32, true, false));
        if alpha < noise {
            continue;
        }
        // The phase keeps this alpha; the magnified texel still rejects it.
        assert!(
            rejects(alpha, cutoff, 0.0, frag, frame as f32, true, true),
            "frame {frame}"
        );
        saw_split = true;
    }
    assert!(saw_split, "a frame whose phase is at most 0.3");
}

/// [`rejects`] is the shader's test: `alpha_rejects` in `lab_forward.wgsl` compares the cutoff
/// with the hash off and `mix(cutoff, noise, minified)` with it on.
#[test]
fn rejects_mirrors_the_shader_comparisons() {
    let source = include_str!("../../shaders/lab_forward.wgsl");
    let start = source
        .find("fn alpha_rejects(alpha: f32, cutoff: f32, minified: f32, frag: vec2<f32>) -> bool {")
        .expect("alpha_rejects in lab_forward.wgsl");
    let body = &source[start..];
    let body = &body[..body.find("\n}\n").expect("end of alpha_rejects")];
    assert!(body.contains("return alpha < cutoff;"), "{body}");
    assert!(
        body.contains("return alpha < mix(cutoff, noise, minified);"),
        "{body}"
    );
    assert!(
        body.contains("fract(phase + 0.618034 * frame.quality.w)"),
        "{body}"
    );
    // `mix(a, b, t)` = `a + (b - a) * t`, as `rejects` spells it.
    let frag = [10.5, 4.5];
    for frame in 0..8 {
        let noise = threshold(frag, frame as f32, false);
        for minified in [0.0, 0.25, 0.5, 1.0] {
            let limit = 0.5 + (noise - 0.5) * minified;
            for alpha in [0.0, 0.2, 0.45, 0.55, 0.8, 1.0] {
                assert_eq!(
                    rejects(alpha, 0.5, minified, frag, frame as f32, false, true),
                    alpha < limit,
                    "alpha {alpha} minified {minified} frame {frame}"
                );
            }
        }
    }
}

#[test]
fn pcg_matches_the_reference_values() {
    // The shader's `alpha_hash_pcg` takes the same constants; these pin them.
    assert_eq!(pcg(0), 129_708_002);
    assert_eq!(pcg(1), 2_831_084_092);
    assert_eq!(pcg(12_345), 4_099_845_390);
    assert_eq!(pcg(0xdead_beef), 1_730_779_506);
}

#[test]
fn every_permutation_of_s4_is_drawn_once() {
    let mut seen = std::collections::HashSet::new();
    for p in 0..24 {
        let digits: Vec<u32> = (0..4).map(|pos| permuted_digit(pos, p)).collect();
        let mut sorted = digits.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2, 3], "permutation {p} gives {digits:?}");
        assert!(seen.insert(digits), "permutation {p} repeats");
    }
}

#[test]
fn every_aligned_2x2_block_holds_one_phase_per_quarter() {
    for by in (0..256u32).step_by(2) {
        for bx in (0..256u32).step_by(2) {
            let mut quarters: Vec<u32> = (0..4)
                .map(|i| (hierarchy_phase([bx + (i & 1), by + (i >> 1)]) * 4.0) as u32)
                .collect();
            quarters.sort_unstable();
            assert_eq!(quarters, [0, 1, 2, 3], "block at ({bx}, {by})");
        }
    }
}

#[test]
fn the_hierarchy_phase_is_uniform() {
    let mut bins = [0u32; 16];
    for y in 0..64 {
        for x in 0..64 {
            let p = hierarchy_phase([x, y]);
            assert!((0.0..1.0).contains(&p), "phase {p} at ({x}, {y})");
            bins[(p * 16.0) as usize] += 1;
        }
    }
    for (i, &n) in bins.iter().enumerate() {
        assert!(
            (224..=288).contains(&n),
            "bin {i} holds {n} of 4096 phases (256 expected)"
        );
    }
}

#[test]
fn a_held_view_keeps_the_interleaved_gradient_threshold() {
    // Held views draw exactly the threshold of before ADR-1489 (`hair::hashed_threshold`), so
    // their pixels do not change.
    for i in 0..4096u32 {
        let frag = [(i % 64) as f32 + 0.5, (i / 64) as f32 + 0.5];
        let frame = (i % 16) as f32;
        assert_eq!(
            threshold(frag, frame, true).to_bits(),
            crate::hair::hashed_threshold(frag, frame).to_bits()
        );
    }
}

#[test]
fn stipple_energy_finds_the_checker_and_the_lattice() {
    let checker = stipple_energy(&image(|x, y| ((x + y) % 2) as f32), N, N);
    assert!(checker.checker > 0.9, "{checker:?}");
    let ign = stipple_energy(&image(|x, y| ign_phase([x as f32 + 0.5, y as f32 + 0.5])), N, N);
    assert!(ign.lattice > 0.4 && ign.peak_ratio > 200.0, "{ign:?}");
    let hier = stipple_energy(&image(|x, y| hierarchy_phase([x as u32, y as u32])), N, N);
    assert!(
        hier.lattice < 0.05 && hier.checker < 0.05 && hier.peak_ratio < 60.0,
        "{hier:?}"
    );
    let flat = stipple_energy(&image(|_, _| 0.25), N, N);
    assert_eq!(flat.lattice, 0.0);
    assert_eq!(
        stipple_energy(&[0.0; 16], 4, 4).peak_ratio,
        0.0,
        "too small to measure"
    );
}

/// A CPU model of the temporal resolve of `taau.wgsl` over a hashed mask under a camera moving
/// `speed` pixels per frame to the right: the coverage under pixel `x` on frame `t` is the
/// world's at `x + speed·t` (a smooth 0.2-0.8 ramp), the history is fetched `speed` pixels on,
/// bilinearly, clipped to the current 3 × 3 mean ± 1.25 σ box (with the pixel's own sample), and
/// blended at 0.9 less the fractional-fetch loss. Returns the final error (resolved − true).
fn resolved_error(held_phase: bool, speed: f32, frames: u32) -> Vec<f32> {
    let truth = |x: usize, t: u32| 0.5 + 0.3 * (TAU * (x as f32 + speed * t as f32) / 37.0).sin();
    let mut history: Vec<f32> = Vec::new();
    for t in 0..frames {
        let current = image(|x, y| {
            let frag = [x as f32 + 0.5, y as f32 + 0.5];
            f32::from(u8::from(
                truth(x, t) > threshold(frag, (t % 16) as f32, held_phase),
            ))
        });
        if history.is_empty() {
            history = current;
            continue;
        }
        let fetch = |x: usize, y: usize| {
            let hx = (x as f32 + speed).min((N - 1) as f32);
            let (x0, f) = (hx.floor() as usize, hx.fract());
            let x1 = (x0 + 1).min(N - 1);
            history[y * N + x0] * (1.0 - f) + history[y * N + x1] * f
        };
        let next = image(|x, y| {
            let c = current[y * N + x];
            let (mut lo, mut hi, mut m1, mut m2) = (f32::MAX, f32::MIN, 0.0f32, 0.0f32);
            for dy in [-1i64, 0, 1] {
                for dx in [-1i64, 0, 1] {
                    let sx = (x as i64 + dx).clamp(0, N as i64 - 1) as usize;
                    let sy = (y as i64 + dy).clamp(0, N as i64 - 1) as usize;
                    let s = current[sy * N + sx];
                    lo = lo.min(s);
                    hi = hi.max(s);
                    m1 += s / 9.0;
                    m2 += s * s / 9.0;
                }
            }
            let sigma = (m2 - m1 * m1).max(0.0).sqrt();
            let box_lo = lo.max((m1 - 1.25 * sigma).min(c));
            let box_hi = hi.min((m1 + 1.25 * sigma).max(c));
            let f = (x as f32 + speed).fract();
            let weight = 0.9 * (1.0 - 0.35 * 4.0 * f * (1.0 - f));
            weight * fetch(x, y).clamp(box_lo, box_hi) + (1.0 - weight) * c
        });
        history = next;
    }
    image(|x, y| history[y * N + x] - truth(x, frames - 1))
}

fn rms(e: &[f32]) -> f32 {
    (e.iter().map(|v| v * v).sum::<f32>() / e.len() as f32).sqrt()
}

#[test]
fn a_moving_resolve_keeps_the_ign_lattice_and_drops_the_hierarchy() {
    // The look-round-4 stipple, reproduced on the CPU: under motion the IGN threshold's error
    // concentrates on its lattice; the hierarchy phase spreads it, at no more error.
    for speed in [0.37f32, 1.3] {
        let ign = resolved_error(true, speed, 64);
        let hier = resolved_error(false, speed, 64);
        let (e_ign, e_hier) = (stipple_energy(&ign, N, N), stipple_energy(&hier, N, N));
        let (r_ign, r_hier) = (rms(&ign), rms(&hier));
        eprintln!("speed {speed}: IGN {e_ign:?} rms {r_ign:.4}; hierarchy {e_hier:?} rms {r_hier:.4}");
        assert!(
            e_ign.lattice > 0.25,
            "IGN residual on the lattice at {speed} px/frame: {e_ign:?}"
        );
        assert!(
            e_hier.lattice < 0.06 && e_hier.peak_ratio < 60.0,
            "hierarchy residual at {speed} px/frame: {e_hier:?}"
        );
        assert!(r_hier <= r_ign * 1.05, "hierarchy rms {r_hier} vs IGN {r_ign}");
    }
}
