// SPDX-License-Identifier: MPL-2.0
//! The corner visibility of the baked GI lookup (ADR-1105): the per-tap Chebyshev filter, the
//! light-bleeding reduction, and the legacy path (moments filtered first, no reduction) kept
//! bit for bit.

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer, VolumeLayout,
    oct_decode, oct_encode,
};

use super::{
    BakedGiSettings, BakedGiUniformGpu, DEFAULT_LEAK_REDUCTION, GiVisibilityFilter, LOOKUP_PER_TAP,
    MAX_LEAK_REDUCTION, MOMENT_TEXEL_BYTES, SunRelight, V3, VolumeWords, oct_texel, screen_source,
    visibility,
};

const FAR: f32 = 16.0;
const RES: usize = 4;

/// A volume of one probe (one column, one level) whose 4 × 4 moment texels (metres) come from
/// `texel(x, y)`.
fn one_probe(texel: impl Fn(usize, usize) -> [f32; 2]) -> BakedGiVolume {
    let layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [4.0, 4.0],
            counts: [1, 1],
            levels: vec![1.0],
            ground: vec![0.0],
        }),
        bricks: Vec::new(),
        moment_resolution: RES as u32,
        max_distance: FAR,
    };
    let probe = ProbeData {
        sh: [[[0.0; 4]; 3]; 4],
        moments: (0..RES * RES).map(|t| texel(t % RES, t / RES)).collect(),
        meta: ProbeMeta {
            offset: [0.0; 3],
            flags: ProbeFlags::VALID,
        },
    };
    BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &[probe]).expect("volume")
}

/// The corner visibility as it was before ADR-1105 (bilinear moments with the taps read across
/// the octahedral folds, ADR-861; then one Chebyshev bound), frozen here so the legacy settings
/// can be compared with it bit for bit.
fn frozen_reference(volume: &BakedGiVolume, dir: V3, r: f32, s: f32) -> f32 {
    let res = volume.moment_resolution.max(1) as usize;
    let bytes = volume.moment_bytes();
    let ri = res as i32;
    let at = |x: i32, y: i32| -> [f32; 2] {
        let (tx, ty) = oct_texel(x, y, ri);
        let k = (ty * res + tx) * MOMENT_TEXEL_BYTES;
        match bytes.get(k..k + 4) {
            Some(b) => [
                f32::from(u16::from_le_bytes([b[0], b[1]])) / 65535.0,
                f32::from(u16::from_le_bytes([b[2], b[3]])) / 65535.0,
            ],
            None => [0.0; 2],
        }
    };
    let uv = oct_encode(dir);
    let rf = res as f32;
    let fx = uv[0].clamp(0.0, 1.0) * rf - 0.5;
    let fy = uv[1].clamp(0.0, 1.0) * rf - 0.5;
    let (flx, fly) = (fx.floor(), fy.floor());
    let (x0, y0) = (flx as i32, fly as i32);
    let (tx, ty) = (fx - flx, fy - fly);
    let lerp = |a: [f32; 2], b: [f32; 2], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let m = lerp(
        lerp(at(x0, y0), at(x0 + 1, y0), tx),
        lerp(at(x0, y0 + 1), at(x0 + 1, y0 + 1), tx),
        ty,
    );
    let (mean, sq) = (
        m[0] * volume.max_distance,
        m[1] * volume.max_distance * volume.max_distance,
    );
    if r > mean {
        let variance = (sq - mean * mean).abs().max(1e-4 * s * s);
        let d = r - mean;
        variance / (variance + d * d)
    } else {
        1.0
    }
}

/// `n` directions spread over the sphere (a golden-angle spiral).
fn directions(n: usize) -> Vec<V3> {
    let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    (0..n)
        .map(|k| {
            let i = k as f32 + 0.5;
            let y = 1.0 - 2.0 * i / n as f32;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let phi = (i * golden) % std::f32::consts::TAU;
            [r * phi.cos(), y, r * phi.sin()]
        })
        .collect()
}

fn with(filter: GiVisibilityFilter, leak_reduction: f32) -> BakedGiSettings {
    BakedGiSettings {
        visibility_filter: filter,
        leak_reduction,
        ..BakedGiSettings::default()
    }
}

#[test]
fn the_legacy_settings_keep_the_old_visibility_bit_for_bit() {
    // Texels at assorted distances and spreads: every direction, distance and cell scale reads
    // exactly what the lookup read before ADR-1105.
    let vol = one_probe(|x, y| {
        let mean = 0.5 + ((x * 7 + y * 3) % 11) as f32 * 1.3;
        let var = ((x + 2 * y) % 5) as f32 * 0.2;
        [mean, mean * mean + var]
    });
    let legacy = BakedGiSettings::legacy_visibility();
    assert_eq!(legacy.visibility_filter, GiVisibilityFilter::Moments);
    assert_eq!(legacy.leak_reduction, 0.0);
    let mut hidden = 0;
    for dir in directions(300) {
        for r in [0.3, 1.0, 2.5, 6.0, 12.0, 15.5] {
            for s in [0.5, 1.0, 4.0] {
                let want = frozen_reference(&vol, dir, r, s);
                let got = visibility(&vol, &legacy, 0, dir, r, s);
                assert_eq!(got.to_bits(), want.to_bits(), "{dir:?} r {r} s {s}");
                hidden += usize::from(want < 0.5);
            }
        }
    }
    assert!(hidden > 500, "the sweep reaches hidden receivers ({hidden})");
}

#[test]
fn per_tap_matches_the_moments_where_the_taps_agree() {
    // One distance distribution in every texel: filtering the bounds or the moments is the same.
    let vol = one_probe(|_, _| [3.0, 9.5]);
    for dir in directions(64) {
        for r in [1.0, 3.2, 4.0, 7.0] {
            let a = visibility(&vol, &with(GiVisibilityFilter::PerTap, 0.0), 0, dir, r, 1.0);
            let b = visibility(&vol, &with(GiVisibilityFilter::Moments, 0.0), 0, dir, r, 1.0);
            assert_eq!(a.to_bits(), b.to_bits(), "{dir:?} r {r}");
        }
    }
}

#[test]
fn per_tap_keeps_a_wall_tap_hidden_where_the_moments_average_it_away() {
    // Texel columns 0-1 see a wall 0.5 m out, columns 2-3 see nothing (the far distance). A
    // direction halfway between columns 1 and 2, a receiver 1 m out: behind the wall for half
    // the taps.
    let vol = one_probe(|x, _| if x <= 1 { [0.5, 0.25] } else { [FAR, FAR * FAR] });
    let dir = oct_decode([0.5, 0.375]);
    let uv = oct_encode(dir);
    assert!((uv[0] - 0.5).abs() < 1e-4, "{uv:?}");
    // The filtered moments put the mean past the receiver (about 8 m): fully visible.
    let moments = visibility(&vol, &with(GiVisibilityFilter::Moments, 0.0), 0, dir, 1.0, 1.0);
    assert_eq!(moments, 1.0);
    // Per tap: the wall taps are hidden, the open taps visible, half and half.
    let per_tap = visibility(&vol, &with(GiVisibilityFilter::PerTap, 0.0), 0, dir, 1.0, 1.0);
    assert!((per_tap - 0.5).abs() < 0.01, "{per_tap}");
    // The reduction cuts the tail: (0.5 − 0.25) / 0.75 = 1/3, and the corner weight (cubed)
    // drops to about 4 % of an unhidden corner's.
    let reduced = visibility(&vol, &BakedGiSettings::default(), 0, dir, 1.0, 1.0);
    assert!((reduced - 1.0 / 3.0).abs() < 0.01, "{reduced}");
    assert!(reduced.powi(3) < 0.04);
    // The reduction alone does not change a receiver the filtered moments call visible.
    let moments_reduced = visibility(&vol, &with(GiVisibilityFilter::Moments, 0.25), 0, dir, 1.0, 1.0);
    assert_eq!(moments_reduced, 1.0);
}

#[test]
fn the_reduction_cuts_the_tail_and_leaves_visible_receivers_alone() {
    // Mean 2 m, variance 1 m²: the bound falls from 1 at 2 m to near 0 far out.
    let vol = one_probe(|_, _| [2.0, 5.0]);
    let dir = [0.0, 0.0, 1.0];
    let s = 1.0;
    let mut last = f32::INFINITY;
    for r in [1.0, 1.9, 2.3, 2.8, 3.5, 5.0, 8.0, 14.0] {
        let plain = visibility(&vol, &with(GiVisibilityFilter::PerTap, 0.0), 0, dir, r, s);
        let cut = visibility(&vol, &with(GiVisibilityFilter::PerTap, 0.4), 0, dir, r, s);
        if plain >= 1.0 {
            // Before the mean: visible, untouched.
            assert_eq!(cut, 1.0, "r {r}");
        } else if plain <= 0.4 {
            assert_eq!(cut, 0.0, "r {r}: {plain} is in the cut tail");
        } else {
            assert!(cut < plain && cut > 0.0, "r {r}: {plain} → {cut}");
            assert!((cut - (plain - 0.4) / 0.6).abs() < 1e-5);
        }
        assert!(cut <= last, "monotonic in distance");
        last = cut;
    }
    assert_eq!(last, 0.0, "far behind the mean: hidden");
}

#[test]
fn the_reduction_amount_is_clamped_and_a_bad_value_turns_it_off() {
    let amount = |k: f32| {
        BakedGiSettings {
            leak_reduction: k,
            ..BakedGiSettings::default()
        }
        .effective_leak_reduction()
    };
    assert_eq!(amount(0.25), 0.25);
    assert_eq!(amount(-1.0), 0.0);
    assert_eq!(amount(3.0), MAX_LEAK_REDUCTION);
    assert_eq!(amount(f32::NAN), 0.0);
    assert_eq!(amount(f32::INFINITY), 0.0);
    // A bad value looks up as "off".
    let vol = one_probe(|_, _| [2.0, 5.0]);
    let dir = [0.0, 1.0, 0.0];
    let off = visibility(&vol, &with(GiVisibilityFilter::PerTap, 0.0), 0, dir, 3.0, 1.0);
    let nan = visibility(
        &vol,
        &with(GiVisibilityFilter::PerTap, f32::NAN),
        0,
        dir,
        3.0,
        1.0,
    );
    assert_eq!(off.to_bits(), nan.to_bits());
    assert!(off > 0.0 && off < 1.0);
}

#[test]
fn the_settings_reach_the_shader_uniform() {
    let d = BakedGiSettings::default();
    assert_eq!(d.visibility_filter, GiVisibilityFilter::PerTap);
    assert_eq!(d.leak_reduction, DEFAULT_LEAK_REDUCTION);
    // The filter is an `extra.z` flag beside the ADR-861 switches, the amount `relight.y`.
    let vol = one_probe(|_, _| [2.0, 5.0]);
    let words = VolumeWords::of(&vol);
    let scales = [[1.0; 3]; 4];
    let uniform = |s: &BakedGiSettings| BakedGiUniformGpu::of(&vol, &words, s, &scales, &SunRelight::NONE);
    let u = uniform(&d);
    assert_ne!(u.extra[2] & LOOKUP_PER_TAP, 0);
    assert_eq!(u.relight, [1.0, DEFAULT_LEAK_REDUCTION, 0.0, 0.0]);
    let legacy = uniform(&BakedGiSettings::legacy_visibility());
    assert_eq!(legacy.extra[2] & LOOKUP_PER_TAP, 0);
    assert_eq!(legacy.relight, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        legacy.extra[2] | LOOKUP_PER_TAP,
        u.extra[2],
        "the other switches are untouched"
    );
    assert_eq!(
        uniform(&with(GiVisibilityFilter::Moments, 9.0)).relight[1],
        MAX_LEAK_REDUCTION
    );
    for f in [GiVisibilityFilter::Moments, GiVisibilityFilter::PerTap] {
        assert_eq!(GiVisibilityFilter::from_name(f.name()), Some(f));
    }
    assert_eq!(GiVisibilityFilter::from_name("pcf"), None);
    // The shader reads the same bit and clamps the amount where the CPU does.
    let src = screen_source();
    assert!(src.contains(&format!("const BGI_LOOKUP_PER_TAP: u32 = {LOOKUP_PER_TAP}u;")));
    assert!(src.contains(&format!("clamp(bgi.relight.y, 0.0, {MAX_LEAK_REDUCTION})")));
    assert!(src.contains("if ((bgi.extra.z & BGI_LOOKUP_PER_TAP) != 0u) {"));
}
