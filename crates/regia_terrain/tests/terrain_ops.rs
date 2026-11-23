// SPDX-License-Identifier: MPL-2.0
//! Heightfield operators (DO27 absorb row C-18): fBm and ridged noise, domain warp, terraces,
//! impact craters and flattened pads. DO27's tool tests (`tools/terrain/test_terrain_features.py`,
//! `test_terrain_noise.py` at d6ed37677) are ported case by case and named after them; the
//! acceptance test is `flatten_pad_is_flat_within_1cm`.

use regia_terrain::TerrainMap;
use regia_terrain::ops::{
    CraterParams, GridFrame, NoiseKind, NoiseParams, OpError, PadParams, PadShape, TerraceParams, WarpParams,
    changed_rect, crater, domain_warp, fbm, flatten_pad, noise_field, ridged, terrace,
};

const CELL: f64 = 5.0;

fn frame(rows: u32, cols: u32) -> GridFrame {
    GridFrame::square(cols, rows, CELL).expect("frame")
}

fn flat(rows: u32, cols: u32, value: f32) -> Vec<f32> {
    vec![value; (rows * cols) as usize]
}

/// Rises `slope` meters per meter along +X.
fn ramp(rows: u32, cols: u32, slope: f64) -> Vec<f32> {
    (0..rows * cols)
        .map(|i| (f64::from(i % cols) * CELL * slope) as f32)
        .collect()
}

fn at(g: &[f32], cols: u32, r: u32, c: u32) -> f32 {
    g[(r * cols + c) as usize]
}

fn noise(rows: u32, cols: u32, seed: u32, amp: f64) -> Vec<f32> {
    fbm(&frame(rows, cols), &NoiseParams::new(seed, amp, 400.0, 5)).expect("fbm")
}

fn std_dev(v: &[f32]) -> f64 {
    let n = v.len() as f64;
    let mean = v.iter().map(|x| f64::from(*x)).sum::<f64>() / n;
    (v.iter().map(|x| (f64::from(*x) - mean).powi(2)).sum::<f64>() / n).sqrt()
}

/// Mean absolute difference between neighbors along X (`axis` 1) or Z (`axis` 0).
fn mean_diff(g: &[f32], rows: u32, cols: u32, axis: u8) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for r in 0..rows {
        for c in 0..cols {
            let (r2, c2) = if axis == 1 { (r, c + 1) } else { (r + 1, c) };
            if r2 < rows && c2 < cols {
                sum += f64::from((at(g, cols, r2, c2) - at(g, cols, r, c)).abs());
                n += 1.0;
            }
        }
    }
    sum / n
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from((x - y).abs()))
        .fold(0.0, f64::max)
}

fn mean_abs_diff(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from((x - y).abs()))
        .sum::<f64>()
        / a.len() as f64
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// Central-difference slope in degrees at an interior sample.
fn slope_deg(g: &[f32], cols: u32, r: u32, c: u32, cell: f64) -> f64 {
    let gx = f64::from(at(g, cols, r, c + 1) - at(g, cols, r, c - 1)) / (2.0 * cell);
    let gz = f64::from(at(g, cols, r + 1, c) - at(g, cols, r - 1, c)) / (2.0 * cell);
    (gx * gx + gz * gz).sqrt().atan().to_degrees()
}

// ---- acceptance -----------------------------------------------------------------------------

/// Row C-18's acceptance: a pad flattened into rough ground and written into a terrain map reads
/// flat within 1 cm wherever the bilinear surface sits wholly inside the footprint, for a circle
/// and for a turned rectangle, at a fixed height and at the default (mean) height.
#[test]
fn flatten_pad_is_flat_within_1cm() {
    // A 512 m map, 2 m per sample, rough to ±15 m.
    let n = 257u32;
    let map0 = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 512.0, 1.0, 0.0).expect("map");
    let fr = GridFrame::of_map(&map0);
    let mut np = NoiseParams::new(4, 15.0, 120.0, 5);
    np.grain_angle_deg = 25.0;
    let rough = fbm(&fr, &np).expect("rough ground");
    assert!(std_dev(&rough) > 2.0, "the ground is rough");

    let circle = PadParams::circle([37.5, -61.0], 30.0);
    let mut rect = PadParams::rect([-80.0, 44.0], [60.0, 24.0], 30.0);
    rect.height_m = Some(7.25);
    rect.feather_m = 12.0;
    for pad in [circle, rect] {
        let out = flatten_pad(&fr, &rough, &pad).expect("pad");
        let map = TerrainMap::from_heights(n, n, out.clone(), 512.0, 1.0, 0.0).expect("map");
        // Points whose bilinear cell lies wholly inside the footprint: one sample diagonal in.
        let margin = 2.0 * 2f64.sqrt() + 1e-6;
        let (sin, cos) = match pad.shape {
            PadShape::Rect { rotation_deg, .. } => rotation_deg.to_radians().sin_cos(),
            _ => (0.0, 1.0),
        };
        let mut seen = Vec::new();
        for i in -60..=60 {
            for j in -60..=60 {
                let (u, v) = (f64::from(i) * 0.5, f64::from(j) * 0.5);
                let inside = match pad.shape {
                    PadShape::Circle { radius_m } => (u * u + v * v).sqrt() <= radius_m - margin,
                    PadShape::Rect { size_m, .. } => {
                        u.abs() <= size_m[0] * 0.5 - margin && v.abs() <= size_m[1] * 0.5 - margin
                    }
                    _ => false,
                };
                if !inside {
                    continue;
                }
                let x = pad.center[0] + u * cos - v * sin;
                let z = pad.center[1] + u * sin + v * cos;
                seen.push(f64::from(map.sample_bilinear(x as f32, z as f32)));
            }
        }
        assert!(seen.len() > 400, "{} interior points", seen.len());
        let lo = seen.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = seen.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(hi - lo <= 0.01, "pad {pad:?} spans {lo} .. {hi}");
        if let Some(h) = pad.height_m {
            assert!(
                (lo - h).abs() <= 0.01 && (hi - h).abs() <= 0.01,
                "pad at {h}, read {lo} .. {hi}"
            );
        }
        // Far ground keeps its bits; the input is untouched.
        let rect = changed_rect(&fr, &rough, &out).expect("the pad changed the ground");
        let reach = pad.reach_m() / 2.0 + 2.0;
        assert!(f64::from(rect[2] - rect[0]) <= 2.0 * reach, "{rect:?}");
    }
}

// ---- noise (test_terrain_noise.py, test_terrain_features.py) --------------------------------

#[test]
fn fbm_deterministic_bounded_and_seeded() {
    let a = noise(48, 80, 3, 10.0);
    let b = noise(48, 80, 3, 10.0);
    let c = noise(48, 80, 4, 10.0);
    assert_eq!(a.len(), 48 * 80);
    assert_eq!(bits(&a), bits(&b));
    assert_ne!(bits(&a), bits(&c));
    assert!(a.iter().all(|v| v.abs() <= 10.0 + 1e-4));
    assert!(std_dev(&a) > 0.5);
}

#[test]
fn fbm_grain_runs_along_angle() {
    let grained = |angle: f64| {
        let mut p = NoiseParams::new(1, 20.0, 400.0, 2);
        p.grain_angle_deg = angle;
        p.anisotropy = 6.0;
        fbm(&frame(96, 96), &p).expect("fbm")
    };
    // Grain along +X: heights vary slowly along columns, quickly along rows.
    let g = grained(0.0);
    assert!(mean_diff(&g, 96, 96, 1) < 0.5 * mean_diff(&g, 96, 96, 0));
    // Turning the grain 90 degrees swaps the roles.
    let g2 = grained(90.0);
    assert!(mean_diff(&g2, 96, 96, 0) < 0.5 * mean_diff(&g2, 96, 96, 1));
}

#[test]
fn fbm_tiles_with_origin() {
    let p = NoiseParams::new(3, 1.0, 400.0, 5);
    let full = fbm(&frame(20, 40), &p).expect("full");
    let right_frame = GridFrame::new(20, 20, [CELL, CELL], [100.0, 0.0]).expect("frame");
    let right = fbm(&right_frame, &p).expect("right");
    for r in 0..20 {
        for c in 0..20 {
            assert_eq!(at(&full, 40, r, c + 20).to_bits(), at(&right, 20, r, c).to_bits());
        }
    }
}

#[test]
fn ridged_range_and_grain() {
    let p = NoiseParams::new(2, 8.0, 400.0, 5);
    let r = ridged(&frame(64, 64), &p, 2.0).expect("ridged");
    assert!(r.iter().all(|v| (0.0..=8.0 + 1e-4).contains(v)));
    assert!(std_dev(&r) > 0.3);
    assert_eq!(bits(&r), bits(&ridged(&frame(64, 64), &p, 2.0).expect("again")));
    let mut q = NoiseParams::new(2, 20.0, 400.0, 2);
    q.grain_angle_deg = 90.0;
    q.anisotropy = 6.0;
    let g = ridged(&frame(96, 96), &q, 2.0).expect("grained");
    assert!(mean_diff(&g, 96, 96, 0) < 0.5 * mean_diff(&g, 96, 96, 1));
    // Another sharpness goes through the deterministic power.
    let s3 = noise_field(&frame(16, 16), &p, NoiseKind::Ridged { sharpness: 3.0 }).expect("sharp");
    assert!(s3.iter().all(|v| (0.0..=8.0 + 1e-4).contains(v)));
}

#[test]
fn bad_settings_and_grids_are_refused() {
    let mut p = NoiseParams::new(0, 1.0, 400.0, 5);
    p.anisotropy = 0.5;
    assert_eq!(fbm(&frame(8, 8), &p).expect_err("aniso").code(), "op_bad_param");
    assert!(matches!(
        GridFrame::square(1, 8, CELL),
        Err(OpError::GridTooSmall { .. })
    ));
    assert_eq!(
        GridFrame::square(9000, 9000, CELL).expect_err("big").code(),
        "op_bad_grid"
    );
    assert_eq!(
        GridFrame::square(8, 8, 0.0).expect_err("cell").code(),
        "op_bad_grid"
    );
    let fr = frame(8, 8);
    let mut h = flat(8, 8, 1.0);
    assert_eq!(
        terrace(&fr, &h[..10], &TerraceParams::default())
            .expect_err("count")
            .code(),
        "op_bad_grid"
    );
    h[5] = f32::NAN;
    assert_eq!(
        crater(&fr, &h, &CraterParams::new([0.0, 0.0], 10.0, 1.0, 1.0)).expect_err("nan"),
        OpError::NonFinite { index: 5 }
    );
    let mut c = CraterParams::new([0.0, 0.0], 10.0, 1.0, 1.0);
    c.ejecta_extent = 1.0;
    assert_eq!(
        crater(&fr, &flat(8, 8, 0.0), &c).expect_err("extent").code(),
        "op_bad_param"
    );
}

// ---- domain warp ----------------------------------------------------------------------------

#[test]
fn domain_warp_changes_but_keeps_range() {
    let base = noise(64, 64, 1, 10.0);
    let fr = frame(64, 64);
    let w = domain_warp(&fr, &base, &WarpParams::new(5, 40.0, 300.0)).expect("warp");
    assert_eq!(w.len(), base.len());
    assert_ne!(bits(&w), bits(&base));
    let (lo, hi) = base.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
        (a.min(*v), b.max(*v))
    });
    assert!(w.iter().all(|v| *v >= lo - 1e-4 && *v <= hi + 1e-4));
    assert_eq!(
        bits(&w),
        bits(&domain_warp(&fr, &base, &WarpParams::new(5, 40.0, 300.0)).expect("again"))
    );
    assert_eq!(
        bits(&domain_warp(&fr, &base, &WarpParams::new(5, 0.0, 300.0)).expect("zero")),
        bits(&base)
    );
}

/// A window of a map warps like the map wherever its reads stay inside the window.
#[test]
fn domain_warp_window_matches_the_full_map() {
    let full_frame = frame(64, 64);
    let base = noise(64, 64, 9, 10.0);
    let p = WarpParams::new(2, 20.0, 200.0);
    let full = domain_warp(&full_frame, &base, &p).expect("full");
    // The window covers columns and rows 16..48; margin 20 m = 4 samples, plus one for bilinear.
    let win = GridFrame::square(32, 32, CELL)
        .and_then(|f| f.with_first([16, 16]))
        .expect("window");
    // Noise positions come from the full-map index: the window's own noise matches bit for bit.
    let p_noise = NoiseParams::new(9, 10.0, 400.0, 5);
    let sub_noise = fbm(&win, &p_noise).expect("window noise");
    for r in 0..32 {
        for c in 0..32 {
            assert_eq!(
                at(&sub_noise, 32, r, c).to_bits(),
                at(&base, 64, r + 16, c + 16).to_bits()
            );
        }
    }
    let sub: Vec<f32> = (16..48)
        .flat_map(|r| (16..48).map(move |c| (r, c)))
        .map(|(r, c)| at(&base, 64, r, c))
        .collect();
    let part = domain_warp(&win, &sub, &p).expect("window warp");
    // The same displacement; only the bilinear weights' last bits may differ (c + d vs c + 16 + d).
    for r in 5..27 {
        for c in 5..27 {
            let (a, b) = (at(&part, 32, r, c), at(&full, 64, r + 16, c + 16));
            assert!((a - b).abs() < 1e-4, "({r}, {c}): {a} vs {b}");
        }
    }
}

// ---- terrace --------------------------------------------------------------------------------

fn terrace_p(step: f64, jitter: f64, sharp: f64) -> TerraceParams {
    let mut p = TerraceParams::new(step);
    p.width_jitter = jitter;
    p.edge_sharpness = sharp;
    p
}

#[test]
fn terrace_creates_flat_treads() {
    // test_terrace_creates_flat_treads: a 0..100 m ramp over 100 columns, 10 m steps.
    let cols = 100u32;
    let row: Vec<f32> = (0..cols).map(|c| (f64::from(c) * 100.0 / 99.0) as f32).collect();
    let h: Vec<f32> = (0..20).flat_map(|_| row.iter().copied()).collect();
    let fr = frame(20, cols);
    let out = terrace(&fr, &h, &terrace_p(10.0, 0.0, 1.0)).expect("terrace");
    assert!(max_abs_diff(&out, &h) <= 10.0 + 1e-3);
    let flat_steps = (0..cols - 1)
        .filter(|c| (out[*c as usize + 1] - out[*c as usize]).abs() < 0.05)
        .count();
    assert!(
        flat_steps as f64 / f64::from(cols - 1) > 0.7,
        "{flat_steps} flat steps"
    );
    assert_eq!(
        bits(&out),
        bits(&terrace(&fr, &h, &terrace_p(10.0, 0.0, 1.0)).expect("again"))
    );
}

#[test]
fn terrace_sharpness_zero_is_identity() {
    let h = ramp(20, 100, 0.2);
    let out = terrace(&frame(20, 100), &h, &terrace_p(10.0, 0.0, 0.0)).expect("terrace");
    assert_eq!(bits(&out), bits(&h));
}

#[test]
fn terrace_snaps_to_treads_when_sharp() {
    let h = ramp(32, 64, 0.25);
    let out = terrace(&frame(32, 64), &h, &terrace_p(4.0, 0.0, 1.0)).expect("terrace");
    let snapped = out
        .iter()
        .filter(|v| {
            let q = f64::from(**v) / 4.0;
            (q - q.round()).abs() < 0.02
        })
        .count();
    assert!(
        snapped as f64 / out.len() as f64 > 0.9,
        "{snapped} of {}",
        out.len()
    );
    assert_ne!(bits(&out), bits(&h));
    // A base offset moves the treads with it.
    let mut based = terrace_p(4.0, 0.0, 1.0);
    based.base_m = 1.5;
    let off = terrace(&frame(32, 64), &h, &based).expect("based");
    let on_base = off
        .iter()
        .filter(|v| {
            let q = (f64::from(**v) - 1.5) / 4.0;
            (q - q.round()).abs() < 0.02
        })
        .count();
    assert!(on_base as f64 / off.len() as f64 > 0.9);
}

#[test]
fn terrace_softness_jitter_and_collapse() {
    let fr = frame(32, 64);
    let h = ramp(32, 64, 0.25);
    let hard = terrace(&fr, &h, &terrace_p(4.0, 0.0, 1.0)).expect("hard");
    let soft = terrace(&fr, &h, &terrace_p(4.0, 0.0, 0.1)).expect("soft");
    let max_step = |g: &[f32]| {
        (0..g.len() - 1)
            .map(|i| (g[i + 1] - g[i]).abs())
            .fold(0.0, f32::max)
    };
    assert!(max_step(&soft) < max_step(&hard));
    let mut gone = terrace_p(4.0, 0.0, 1.0);
    gone.collapse = 1.0;
    assert_eq!(bits(&terrace(&fr, &h, &gone).expect("collapsed")), bits(&h));
    let mut ja = TerraceParams::new(6.0);
    ja.width_jitter = 0.5;
    let mut jb = ja;
    ja.seed = 1;
    jb.seed = 2;
    assert_ne!(
        bits(&terrace(&fr, &h, &ja).expect("a")),
        bits(&terrace(&fr, &h, &jb).expect("b"))
    );
}

#[test]
fn terrace_half_collapse_restores_part_of_the_slope() {
    // test_terrace_collapse_restores_original.
    let fr = frame(30, 100);
    let h = ramp(30, 100, 0.2);
    let with = |collapse: f64| {
        let mut p = terrace_p(10.0, 0.3, 1.0);
        p.seed = 2;
        p.collapse = collapse;
        terrace(&fr, &h, &p).expect("terrace")
    };
    let full = mean_abs_diff(&with(0.0), &h);
    let half = mean_abs_diff(&with(0.5), &h);
    assert!(0.2 * full < half && half < full, "full {full}, half {half}");
}

// ---- crater ---------------------------------------------------------------------------------

#[test]
fn crater_profile() {
    let fr = frame(101, 101);
    let out = crater(
        &fr,
        &flat(101, 101, 0.0),
        &CraterParams::new([250.0, 250.0], 100.0, 12.0, 4.0),
    )
    .expect("crater");
    let a = |r, c| f64::from(at(&out, 101, r, c));
    assert!((a(50, 50) + 12.0).abs() < 1e-4, "center {}", a(50, 50));
    assert!((a(50, 70) - 4.0).abs() < 1e-3, "rim {}", a(50, 70));
    assert!(a(50, 60) < 0.0 && 0.0 < a(50, 75));
    assert!(a(50, 75) > a(50, 90), "the blanket decays");
    assert_eq!(at(&out, 101, 50, 100), 0.0, "beyond the extent: untouched");
    let max = out.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    assert!((f64::from(max) - 4.0).abs() < 1e-3);
}

#[test]
fn crater_central_peak_and_additive() {
    let fr = frame(101, 101);
    let base = ramp(101, 101, 0.05);
    let mut p = CraterParams::new([250.0, 250.0], 100.0, 12.0, 3.0);
    let plain = crater(&fr, &base, &p).expect("plain");
    p.central_peak_m = 9.0;
    let peaked = crater(&fr, &base, &p).expect("peaked");
    let d = |g: &[f32], r, c| f64::from(at(g, 101, r, c));
    assert!((d(&peaked, 50, 50) - d(&plain, 50, 50) - 9.0).abs() < 1e-3);
    assert!((d(&peaked, 50, 80) - d(&plain, 50, 80)).abs() < 0.5);
    assert!((d(&plain, 50, 50) - (d(&base, 50, 50) - 12.0)).abs() < 1e-3);
    // Beyond the reach every sample keeps its bits.
    let rect = changed_rect(&fr, &base, &plain).expect("changed");
    assert!(rect[2] <= 101, "{rect:?}");
    assert_eq!(at(&plain, 101, 0, 0).to_bits(), at(&base, 101, 0, 0).to_bits());
}

// ---- flatten pad ----------------------------------------------------------------------------

#[test]
fn flatten_pad_circle_and_rect() {
    let fr = frame(64, 64);
    let h = noise(64, 64, 4, 15.0);
    let mut p = PadParams::circle([160.0, 160.0], 30.0);
    p.feather_m = 25.0;
    let out = flatten_pad(&fr, &h, &p).expect("pad");
    let dist = |r: u32, c: u32| {
        let (x, z) = (f64::from(c) * CELL - 160.0, f64::from(r) * CELL - 160.0);
        (x * x + z * z).sqrt()
    };
    let mut inside = Vec::new();
    for r in 0..64 {
        for c in 0..64 {
            let i = (r * 64 + c) as usize;
            if dist(r, c) <= 30.0 {
                inside.push(out[i]);
            }
            if dist(r, c) > 60.0 {
                assert_eq!(out[i].to_bits(), h[i].to_bits(), "far sample ({r}, {c})");
            }
            if dist(r, c) <= 20.0 {
                assert!(slope_deg(&out, 64, r, c, CELL) < 0.1);
            }
        }
    }
    assert!(inside.iter().all(|v| v.to_bits() == inside[0].to_bits()));
    assert_eq!(bits(&h), bits(&noise(64, 64, 4, 15.0)), "the input is untouched");

    let mut rect = PadParams::rect([160.0, 160.0], [80.0, 40.0], 0.0);
    rect.height_m = Some(7.0);
    rect.feather_m = 10.0;
    let out = flatten_pad(&fr, &h, &rect).expect("rect");
    assert_eq!(at(&out, 64, 32, 32), 7.0);
    assert_eq!(
        at(&out, 64, 32, 38),
        7.0,
        "30 m east: inside the 40 m half-length"
    );
    assert!(
        (at(&out, 64, 40, 32) - 7.0).abs() > 1e-3,
        "40 m north: outside the 20 m half-width"
    );
}

#[test]
fn flatten_pad_default_height_is_the_mean_with_a_hard_edge() {
    let fr = frame(10, 10);
    let h: Vec<f32> = (0..100).map(|i| i as f32).collect();
    let mut p = PadParams::rect([22.5, 22.5], [20.0, 20.0], 0.0);
    p.feather_m = 0.0;
    let out = flatten_pad(&fr, &h, &p).expect("pad");
    let mut sum = 0.0;
    let mut n = 0.0;
    for r in 0..10u32 {
        for c in 0..10u32 {
            let (x, z) = (f64::from(c) * CELL, f64::from(r) * CELL);
            if (x - 22.5).abs() <= 10.0 && (z - 22.5).abs() <= 10.0 {
                sum += f64::from(h[(r * 10 + c) as usize]);
                n += 1.0;
            }
        }
    }
    let changed: Vec<f32> = out
        .iter()
        .zip(&h)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| *a)
        .collect();
    assert!(!changed.is_empty());
    assert!(changed.iter().all(|v| *v == changed[0]));
    assert!(
        (f64::from(changed[0]) - sum / n).abs() < 1e-4,
        "{} vs {}",
        changed[0],
        sum / n
    );
}

#[test]
fn flatten_pad_rect_rotation() {
    let fr = frame(40, 40);
    let h = flat(40, 40, 5.0);
    let mut a = PadParams::rect([102.5, 102.5], [120.0, 20.0], 0.0);
    a.height_m = Some(0.0);
    a.feather_m = 0.0;
    let mut b = a;
    b.shape = PadShape::Rect {
        size_m: [120.0, 20.0],
        rotation_deg: 90.0,
    };
    let oa = flatten_pad(&fr, &h, &a).expect("a");
    let ob = flatten_pad(&fr, &h, &b).expect("b");
    let zeros = |g: &[f32]| g.iter().filter(|v| **v == 0.0).count();
    assert_eq!(zeros(&oa), zeros(&ob));
    assert_eq!(at(&oa, 40, 20, 10), 0.0);
    assert_eq!(at(&oa, 40, 10, 20), 5.0);
    assert_eq!(at(&ob, 40, 10, 20), 0.0);
    assert_eq!(at(&ob, 40, 20, 10), 5.0);
}

#[test]
fn flatten_pad_without_height_needs_a_covered_sample() {
    let fr = frame(8, 8);
    let p = PadParams::circle([-500.0, -500.0], 1.0);
    assert_eq!(flatten_pad(&fr, &flat(8, 8, 0.0), &p), Err(OpError::PadEmpty));
    assert_eq!(OpError::PadEmpty.code(), "op_pad_empty");
    let mut fixed = p;
    fixed.height_m = Some(3.0);
    assert_eq!(
        flatten_pad(&fr, &flat(8, 8, 0.0), &fixed).expect("nothing to do"),
        flat(8, 8, 0.0)
    );
}

/// The standard resource-pad radius of the DO27 tool is a setting here: any radius works, and a
/// hard-edged pad changes only the samples inside it.
#[test]
fn flatten_pad_hard_edge_changes_only_the_inside() {
    let fr = frame(64, 64);
    let h = ramp(64, 64, 0.2);
    let mut p = PadParams::circle([160.0, 160.0], 18.7);
    p.feather_m = 0.0;
    let out = flatten_pad(&fr, &h, &p).expect("pad");
    let mut inside = Vec::new();
    for r in 0..64u32 {
        for c in 0..64u32 {
            let (x, z) = (f64::from(c) * CELL - 160.0, f64::from(r) * CELL - 160.0);
            let i = (r * 64 + c) as usize;
            if (x * x + z * z).sqrt() <= 18.7 {
                inside.push(out[i]);
            } else {
                assert_eq!(out[i].to_bits(), h[i].to_bits());
            }
        }
    }
    assert!(!inside.is_empty());
    assert!(inside.iter().all(|v| v.to_bits() == inside[0].to_bits()));
}
