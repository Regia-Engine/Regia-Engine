// SPDX-License-Identifier: MPL-2.0
//! Baked GI leak control by visibility (ADR-1105), CPU only: thin-wall repro scenes.
//!
//! The probes' distance moments are gathered here the way the bake gathers them (`regia_pt`
//! `pt.wgsl` `probe_accumulate`: every ray of a fixed set weighted into an octahedral texel by
//! `max(0, cos)^50` around the texel direction, distances clamped to the far distance), against
//! boxes traced analytically. The light is set per probe by which side of the wall it sits on,
//! and the lookup is the CPU mirror of the shader (`sample_static_gi`), so each case isolates
//! the visibility weighting:
//!
//! - A 10 cm wall 0.25 m past a probe plane (the lit side) over a dark room (8 × 8 moments): the
//!   legacy lookup (moments filtered, then one Chebyshev bound) lets most of the lit probes'
//!   light onto the dark floor at the wall; the per-tap filter with the light-bleeding reduction
//!   cuts it, and the lit side reads its own light either way.
//! - The same wall with 4 × 4 moments (the terrain layer's resolution): the new lookup never leaks
//!   more than the legacy one.
//! - Open ground under a light gradient: the new lookup stays within a few percent of the legacy
//!   one (it only re-weights corners the moments find partly hidden), and a uniform light reads
//!   the same in both.
//! - A thin wall halfway between two probe planes of a 1 m brick: both lookups hold.

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, ShRgb,
    TerrainLayer, VolumeLayout, texel_direction,
};
use regia_render::baked_gi::GiVisibilityFilter;
use regia_render::{BakedGiSettings, sample_static_gi};

/// Moment distance clamp, metres.
const FAR: f32 = 16.0;
/// The bake's moment lobe exponent (`ProbeTraceSettings::moment_exponent`).
const LOBE: f32 = 50.0;
/// Rays per probe.
const RAYS: usize = 2048;

/// An axis-aligned box: min, max corner.
type Aabb = ([f32; 3], [f32; 3]);

/// The ground: a slab under y = 0.
const GROUND: Aabb = ([-40.0, -1.0, -40.0], [40.0, 0.0, 40.0]);

/// Radiance SH of a uniform sky radiance `l` (no L1), in the sky component.
fn sky(l: f32) -> [ShRgb; 4] {
    let k = 4.0 * std::f32::consts::PI * SH_Y00 * l;
    let zero = [[0.0; 4]; 3];
    [zero, [[k, 0.0, 0.0, 0.0]; 3], zero, zero]
}

/// Sky component only, at the baked level.
const SCALES: [[f32; 3]; 4] = [[0.0; 3], [1.0; 3], [0.0; 3], [0.0; 3]];

/// `n` directions spread over the sphere (a golden-angle spiral).
fn fibonacci(n: usize) -> Vec<[f32; 3]> {
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

/// Distance along `d` from `o` (outside the box) to the box, if the ray enters it.
fn hit(o: [f32; 3], d: [f32; 3], b: &Aabb) -> Option<f32> {
    let (mut t0, mut t1) = (f32::NEG_INFINITY, f32::INFINITY);
    for (((&ok, &dk), &lo), &hi) in o.iter().zip(&d).zip(&b.0).zip(&b.1) {
        if dk.abs() < 1e-12 {
            if ok < lo || ok > hi {
                return None;
            }
        } else {
            let (a, c) = ((lo - ok) / dk, (hi - ok) / dk);
            t0 = t0.max(a.min(c));
            t1 = t1.min(a.max(c));
        }
    }
    (t0 > 0.0 && t1 >= t0).then_some(t0)
}

fn inside(p: [f32; 3], boxes: &[Aabb]) -> bool {
    boxes
        .iter()
        .any(|(lo, hi)| (0..3).all(|k| p[k] > lo[k] && p[k] < hi[k]))
}

/// The bake's moments of a probe at `o`: per texel the lobe-weighted mean and mean square of the
/// first-hit distance (clamped to [`FAR`]; a texel no ray reaches reads the far distance).
fn moments(o: [f32; 3], boxes: &[Aabb], rays: &[[f32; 3]], res: u32) -> Vec<[f32; 2]> {
    let dist: Vec<f32> = rays
        .iter()
        .map(|d| boxes.iter().filter_map(|b| hit(o, *d, b)).fold(FAR, f32::min))
        .collect();
    (0..(res * res) as usize)
        .map(|t| {
            let td = texel_direction(t, res);
            let (mut m, mut sq, mut w) = (0.0f32, 0.0f32, 0.0f32);
            for (d, &r) in rays.iter().zip(&dist) {
                let c = (d[0] * td[0] + d[1] * td[1] + d[2] * td[2]).max(0.0);
                let k = c.powf(LOBE);
                if k > 0.0 {
                    m += k * r;
                    sq += k * r * r;
                    w += k;
                }
            }
            if w > 0.0 {
                [m / w, sq / w]
            } else {
                [FAR, FAR * FAR]
            }
        })
        .collect()
}

/// A volume over `boxes` with `layout`'s probes: valid unless inside a box, lit `light(p)`.
fn bake(layout: &VolumeLayout, boxes: &[Aabb], light: impl Fn([f32; 3]) -> f32) -> BakedGiVolume {
    let rays = fibonacci(RAYS);
    let positions: Vec<[f32; 3]> = (0..layout.probe_count())
        .map(|i| {
            if let Some(t) = layout.terrain.as_ref().filter(|t| i < t.probe_count()) {
                let (ci, cj, l) = t.coords(i);
                return t.nominal_position(ci, cj, l);
            }
            let b = layout
                .bricks
                .iter()
                .find(|b| {
                    let first = b.first_probe as usize;
                    i >= first && i < first + b.probe_count()
                })
                .expect("every probe is in the terrain layer or a brick");
            let (x, y, z) = b.coords(i - b.first_probe as usize);
            b.nominal_position(x, y, z)
        })
        .collect();
    let probes: Vec<ProbeData> = positions
        .iter()
        .map(|&p| {
            let valid = !inside(p, boxes);
            ProbeData {
                sh: sky(light(p)),
                moments: if valid {
                    moments(p, boxes, &rays, layout.moment_resolution)
                } else {
                    vec![[FAR, FAR * FAR]; (layout.moment_resolution * layout.moment_resolution) as usize]
                },
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: if valid {
                        ProbeFlags::VALID
                    } else {
                        ProbeFlags::INSIDE_GEOMETRY
                    },
                },
            }
        })
        .collect();
    BakedGiVolume::from_probes(
        layout,
        BakeLights {
            sky_scale: 1.0,
            ..BakeLights::default()
        },
        BakeInfo::default(),
        &probes,
    )
    .expect("volume")
}

/// A terrain layer over flat ground: 6 × 6 columns 4 m apart from (−8, −8), levels 1, 2.5, 5 m.
fn terrain_layout(res: u32) -> VolumeLayout {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-8.0, -8.0],
            spacing: [4.0, 4.0],
            counts: [6, 6],
            levels: vec![1.0, 2.5, 5.0],
            ground: vec![0.0; 36],
        }),
        bricks: Vec::new(),
        moment_resolution: res,
        max_distance: FAR,
    };
    layout.assign_brick_offsets();
    layout
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn unit(a: [f32; 3]) -> [f32; 3] {
    let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    [a[0] / l, a[1] / l, a[2] / l]
}

/// The sky light at `p` (normal `n`) seen from `eye`.
fn light_at(vol: &BakedGiVolume, s: &BakedGiSettings, p: [f32; 3], n: [f32; 3], eye: [f32; 3]) -> f32 {
    let r = sample_static_gi(vol, s, &SCALES, p, n, unit(sub(eye, p)));
    assert!(r.coverage > 0.0, "{p:?} is inside the volume: {r:?}");
    r.irradiance_over_pi[1]
}

const LIT: f32 = 10.0;
const DARK: f32 = 1.0;

/// The share of the lit side's light a dark-side receiver reads: 0 = none, 1 = all of it.
fn leak_share(light: f32) -> f32 {
    (light - DARK) / (LIT - DARK)
}

/// A 10 cm wall (x 0.25..0.35, 4 m high, z ±20 m) just past the terrain layer's x = 0 probe
/// plane: probes at x < 0.3 read [`LIT`], the others [`DARK`].
fn wall_near_a_probe_plane(res: u32) -> BakedGiVolume {
    let wall: Aabb = ([0.25, 0.0, -20.0], [0.35, 4.0, 20.0]);
    bake(&terrain_layout(res), &[GROUND, wall], |p| {
        if p[0] < 0.3 { LIT } else { DARK }
    })
}

/// Receivers on the dark side (point, normal), seen from a camera in the dark room.
const DARK_EYE: [f32; 3] = [3.0, 1.6, 1.0];
const DARK_SIDE: [([f32; 3], [f32; 3]); 5] = [
    ([0.4, 0.0, 1.0], [0.0, 1.0, 0.0]),
    ([1.0, 0.0, -1.3], [0.0, 1.0, 0.0]),
    ([0.6, 0.0, 2.1], [0.0, 1.0, 0.0]),
    ([0.351, 1.5, 0.2], [1.0, 0.0, 0.0]),
    ([0.351, 0.6, -0.7], [1.0, 0.0, 0.0]),
];

/// Receivers on the lit side, seen from a camera in the lit room.
const LIT_EYE: [f32; 3] = [-3.0, 1.6, 1.0];
const LIT_SIDE: [([f32; 3], [f32; 3]); 3] = [
    ([0.1, 0.0, 1.0], [0.0, 1.0, 0.0]),
    ([0.249, 1.5, 0.2], [-1.0, 0.0, 0.0]),
    ([-1.5, 0.0, -0.4], [0.0, 1.0, 0.0]),
];

#[test]
fn a_thin_wall_beside_a_probe_plane_leaks_less_with_per_tap_visibility() {
    let vol = wall_near_a_probe_plane(8);
    let legacy = BakedGiSettings::legacy_visibility();
    let now = BakedGiSettings::default();
    assert_eq!(now.visibility_filter, GiVisibilityFilter::PerTap);
    let (mut sum_legacy, mut sum_now) = (0.0, 0.0);
    let mut shares = Vec::new();
    for (p, n) in DARK_SIDE {
        let before = leak_share(light_at(&vol, &legacy, p, n, DARK_EYE));
        let after = leak_share(light_at(&vol, &now, p, n, DARK_EYE));
        eprintln!("dark side {p:?}: leak share legacy {before:.3}, per tap + reduction {after:.3}");
        assert!(
            after <= before + 0.005,
            "{p:?}: the new lookup leaks more ({after:.3} vs {before:.3})"
        );
        sum_legacy += before;
        sum_now += after;
        shares.push((before, after));
    }
    // The repro: the legacy lookup lets most of the lit light onto the dark floor at the wall.
    assert!(
        shares[0].0 > 0.5 && shares[2].0 > 0.5,
        "the scene no longer reproduces the leak: {shares:?}"
    );
    // The cut: under half at the wall foot, gone a little farther out, and under 40 % overall.
    assert!(shares[0].1 < 0.6 * shares[0].0, "{shares:?}");
    assert!(shares[2].1 < 0.1, "{shares:?}");
    assert!(
        sum_now < 0.4 * sum_legacy,
        "leak share summed over the dark side: legacy {sum_legacy:.3}, now {sum_now:.3}"
    );
    // The lit side reads its own light either way.
    for (p, n) in LIT_SIDE {
        for s in [&legacy, &now] {
            let e = light_at(&vol, s, p, n, LIT_EYE);
            assert!((e - LIT).abs() < 0.01 * LIT, "lit side {p:?}: {e}");
        }
    }
}

#[test]
fn with_terrain_moments_the_new_lookup_never_leaks_more() {
    let vol = wall_near_a_probe_plane(4);
    let legacy = BakedGiSettings::legacy_visibility();
    let now = BakedGiSettings::default();
    for (p, n) in DARK_SIDE {
        let before = leak_share(light_at(&vol, &legacy, p, n, DARK_EYE));
        let after = leak_share(light_at(&vol, &now, p, n, DARK_EYE));
        eprintln!("4 × 4 moments, dark side {p:?}: leak share legacy {before:.3}, now {after:.3}");
        assert!(after <= before + 0.005, "{p:?}: {after:.3} vs {before:.3}");
        assert!(after < 0.1, "{p:?}: {after:.3}");
    }
}

/// A small deterministic generator (PCG multiplier), so the receivers are the same everywhere.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// 96 receivers over open ground: half on the ground facing up, half in the air facing anywhere.
fn open_receivers() -> Vec<([f32; 3], [f32; 3], [f32; 3])> {
    let mut rng = Lcg(7);
    (0..96)
        .map(|k| {
            let x = -6.0 + 16.0 * rng.next();
            let z = -6.0 + 16.0 * rng.next();
            if k % 2 == 0 {
                let p = [x, 0.0, z];
                let eye = [x - 4.0 + 8.0 * rng.next(), 1.6, z - 4.0 + 8.0 * rng.next()];
                (p, [0.0, 1.0, 0.0], eye)
            } else {
                let p = [x, 0.2 + 4.3 * rng.next(), z];
                let raw = [
                    -1.0 + 2.0 * rng.next(),
                    -1.0 + 2.0 * rng.next(),
                    -1.0 + 2.0 * rng.next(),
                ];
                let l = (raw[0] * raw[0] + raw[1] * raw[1] + raw[2] * raw[2])
                    .sqrt()
                    .max(1e-3);
                let n = [raw[0] / l, raw[1] / l, raw[2] / l];
                let eye = [p[0] + 3.0 * n[0], p[1] + 3.0 * n[1] + 0.5, p[2] + 3.0 * n[2]];
                (p, n, eye)
            }
        })
        .collect()
}

#[test]
fn open_ground_reads_as_before_within_a_few_percent() {
    let legacy = BakedGiSettings::legacy_visibility();
    let now = BakedGiSettings::default();
    for res in [4, 8] {
        // A light gradient: a corner the new filter re-weights shows as a change.
        let vol = bake(&terrain_layout(res), &[GROUND], |p| 20.0 + p[0] + 0.5 * p[2]);
        let (mut sum, mut worst) = (0.0f32, 0.0f32);
        let receivers = open_receivers();
        for &(p, n, eye) in &receivers {
            let a = light_at(&vol, &legacy, p, n, eye);
            let b = light_at(&vol, &now, p, n, eye);
            let dev = (b - a).abs() / a;
            sum += dev;
            worst = worst.max(dev);
        }
        let mean = sum / receivers.len() as f32;
        eprintln!("open ground, {res} × {res} moments: mean change {mean:.4}, worst {worst:.4}");
        assert!(mean < 0.02, "{res}: mean change {mean}");
        assert!(worst < 0.12, "{res}: worst change {worst}");
        // A uniform light: the weights renormalize, so no lookup darkens open ground.
        let flat = bake(&terrain_layout(res), &[GROUND], |_| 5.0);
        for &(p, n, eye) in receivers.iter().take(24) {
            let a = light_at(&flat, &legacy, p, n, eye);
            let b = light_at(&flat, &now, p, n, eye);
            assert!((a - b).abs() <= 1e-4 * a, "{p:?}: {a} vs {b}");
            assert!((b - 5.0).abs() < 0.05, "{p:?}: {b}");
        }
    }
}

#[test]
fn a_thin_wall_between_two_probe_planes_holds_either_way() {
    // A 1 m brick (8 × 8 moments) with probes at x = ±0.5, ±1.5, ±2.5 and a 10 cm wall at x = 0;
    // its y = −0.5 probes are in the ground (invalid).
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin: [-2.5, -0.5, -2.5],
            spacing: [1.0; 3],
            counts: [6, 4, 6],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 8,
        max_distance: FAR,
    };
    layout.assign_brick_offsets();
    let wall: Aabb = ([-0.05, 0.0, -6.0], [0.05, 3.0, 6.0]);
    let vol = bake(&layout, &[GROUND, wall], |p| if p[0] < 0.0 { LIT } else { DARK });
    let eye = [2.0, 1.6, 0.3];
    for (p, n) in [
        ([0.06, 0.0, 0.2], [0.0, 1.0, 0.0]),
        ([0.2, 0.0, -0.7], [0.0, 1.0, 0.0]),
        ([0.051, 0.3, 0.2], [1.0, 0.0, 0.0]),
        ([0.051, 1.2, -0.4], [1.0, 0.0, 0.0]),
        ([0.5, 1.0, 0.0], [0.0, 1.0, 0.0]),
    ] {
        let before = leak_share(light_at(&vol, &BakedGiSettings::legacy_visibility(), p, n, eye));
        let after = leak_share(light_at(&vol, &BakedGiSettings::default(), p, n, eye));
        eprintln!("brick, dark side {p:?}: leak share legacy {before:.3}, now {after:.3}");
        assert!(before < 0.05 && after < 0.05, "{p:?}: {before:.3} / {after:.3}");
        assert!(after <= before + 0.01, "{p:?}: {after:.3} vs {before:.3}");
    }
}
