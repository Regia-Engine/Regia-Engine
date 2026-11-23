// SPDX-License-Identifier: MPL-2.0
//! The baked GI lookup on the GPU (`baked_gi.wgsl` `static_gi`, run at given receivers by
//! `gpu_sample_static_gi`) against its CPU reference (`sample_static_gi`), ADR-506.
//!
//! The volume is synthetic and deterministic (a fixed-seed generator): a terrain layer over
//! uneven ground, a detail brick that blends with it over its last cell, an interior brick,
//! invalid and relocated probes, every light component, and distance moments that vary per
//! texel (so the octahedral seam filter and the Chebyshev visibility decide real weights). The
//! receivers are spread over and around the bricks with random normals and views; roughly half
//! of all probe directions put a bilinear tap across a fold of the 8 × 8 moment maps.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI) and skips without
//! one.

use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, BrickFlags, DetailBrick, GI_COMPONENTS, ProbeData, ProbeFlags,
    ProbeMeta, SH_Y00, ShRgb, TerrainLayer, VolumeLayout,
};
use regia_render::RenderDevice;
use regia_render::baked_gi::{
    BakedGiSettings, StaticGiQuery, StaticGiSample, gpu_sample_static_gi, sample_static_gi,
};

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438).
static GPU: Mutex<()> = Mutex::new(());

/// Takes the process-wide GPU lock, then opens a headless device. Bind the guard before the
/// device (`let Some((_gpu, device)) = device()`) so the device drops while the lock is held.
fn device() -> Option<(MutexGuard<'static, ()>, RenderDevice)> {
    let guard = GPU.lock().unwrap_or_else(PoisonError::into_inner);
    match RenderDevice::headless() {
        Ok(device) => Some((guard, device)),
        Err(err) => {
            eprintln!("skip baked_gi_sampling: {err}");
            None
        }
    }
}

/// xorshift64*: the same stream on every machine.
struct Rng(u64);

impl Rng {
    fn uniform(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (v >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.uniform()
    }

    fn unit(&mut self) -> [f32; 3] {
        loop {
            let d = [
                self.range(-1.0, 1.0),
                self.range(-1.0, 1.0),
                self.range(-1.0, 1.0),
            ];
            let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            if l > 0.1 && l <= 1.0 {
                return [d[0] / l, d[1] / l, d[2] / l];
            }
        }
    }
}

const FAR: f32 = 12.0;
const RES: u32 = 8;

fn regular_brick() -> DetailBrick {
    DetailBrick {
        origin: [-3.0, 0.25, -2.5],
        spacing: [1.0, 0.75, 1.25],
        counts: [5, 4, 4],
        first_probe: 0,
        flags: 0,
    }
}

fn interior_brick() -> DetailBrick {
    DetailBrick {
        origin: [6.0, 0.3, 1.0],
        spacing: [0.8; 3],
        counts: [4, 3, 4],
        first_probe: 0,
        flags: BrickFlags::INTERIOR,
    }
}

/// A probe's radiance SH for one component: L00 from `level`, L1 a random direction within the
/// encodable range (|L1| ≤ √3·L00).
fn sh(rng: &mut Rng, level: f32) -> ShRgb {
    let k = 4.0 * std::f32::consts::PI * SH_Y00;
    std::array::from_fn(|_| {
        let l00 = level * rng.range(0.2, 1.0) * k;
        let d = rng.unit();
        let a = rng.range(0.0, 1.5) * l00;
        [l00, d[0] * a, d[1] * a, d[2] * a]
    })
}

/// The test volume (see the module docs).
fn volume(seed: u64) -> BakedGiVolume {
    volume_with_walls(seed, 0.0)
}

/// [`volume`] with a wall code (ADR-366) toward a random +axis neighbor, at a random position,
/// on a `walls` share of the probes, so the wall split and the ADR-861 leak guard (rejected
/// corners beside wall codes) run. `walls` 0 is [`volume`] exactly (no extra draws).
fn volume_with_walls(seed: u64, walls: f32) -> BakedGiVolume {
    let mut rng = Rng(seed);
    let counts = [6u32, 5u32];
    let ground = (0..counts[1])
        .flat_map(|j| (0..counts[0]).map(move |i| (i, j)))
        .map(|(i, j)| {
            let x = -8.0 + 4.0 * i as f32;
            let z = -6.0 + 3.0 * j as f32;
            0.4 * (0.5 * x).sin() + 0.3 * (0.7 * z).cos()
        })
        .collect();
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-8.0, -6.0],
            spacing: [4.0, 3.0],
            counts,
            levels: vec![0.5, 1.5, 3.0, 6.0],
            ground,
        }),
        bricks: vec![regular_brick(), interior_brick()],
        moment_resolution: RES,
        max_distance: FAR,
    };
    layout.assign_brick_offsets();
    let texels = (RES * RES) as usize;
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| {
            let flags = if rng.uniform() < 0.1 {
                ProbeFlags::UNDER_TERRAIN
            } else if rng.uniform() < 0.3 {
                ProbeFlags::VALID | ProbeFlags::RELOCATED
            } else {
                ProbeFlags::VALID
            };
            let offset = if flags & ProbeFlags::RELOCATED != 0 {
                [
                    rng.range(-0.25, 0.25),
                    rng.range(-0.2, 0.2),
                    rng.range(-0.25, 0.25),
                ]
            } else {
                [0.0; 3]
            };
            // Half the probes see open space around them (the far clamp), half a surface at
            // 0.3..8 m that differs per texel.
            let open = rng.uniform() < 0.5;
            let moments = (0..texels)
                .map(|_| {
                    if open && rng.uniform() < 0.7 {
                        [FAR, FAR * FAR]
                    } else {
                        let mean = rng.range(0.3, 8.0);
                        [mean, mean * mean + rng.range(0.0, 0.5)]
                    }
                })
                .collect();
            let sh = [
                sh(&mut rng, 3.0),
                sh(&mut rng, 1.0),
                sh(&mut rng, 0.2),
                sh(&mut rng, 0.5),
            ];
            let mut flags = flags;
            if walls > 0.0 && rng.uniform() < walls {
                let axis = ((rng.uniform() * 3.0) as usize).min(2);
                flags |= ProbeFlags::wall(axis, rng.range(0.05, 0.95));
            }
            ProbeData {
                sh,
                moments,
                meta: ProbeMeta { offset, flags },
            }
        })
        .collect();
    BakedGiVolume::from_probes(
        &layout,
        BakeLights {
            sun_illuminance: 100_000.0,
            sun_color: [1.0; 3],
            sun_direction: [0.0, 1.0, 0.0],
            sky_scale: 1.0,
            fill_lux: 1.0,
            emissive_scale: 1.0,
        },
        BakeInfo::default(),
        &probes,
    )
    .expect("the test layout is valid")
}

/// Inset (cells) of `q` in `b`: < 0 outside.
fn inset(b: &DetailBrick, q: [f32; 3]) -> f32 {
    (0..3)
        .map(|a| {
            let g = (q[a] - b.origin[a]) / b.spacing[a];
            g.min(b.counts[a] as f32 - 1.0 - g)
        })
        .fold(f32::INFINITY, f32::min)
}

/// Receivers over and around the bricks (two thirds) and over the whole layer, with random
/// normals and views on the normal's side. Receivers whose interior test point (pushed half a
/// cell along the normal) lies within 10⁻³ cell of the interior brick's faces are skipped: the
/// interior brick wins or loses there on the last bit, on either side.
fn queries(seed: u64, count: usize) -> Vec<StaticGiQuery> {
    let mut rng = Rng(seed);
    let interior = interior_brick();
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let box_pick = rng.uniform();
        let p = if box_pick < 0.33 {
            [rng.range(-4.0, 2.0), rng.range(-0.2, 3.5), rng.range(-3.5, 2.5)]
        } else if box_pick < 0.66 {
            [rng.range(5.0, 9.5), rng.range(-0.2, 3.0), rng.range(0.0, 4.5)]
        } else {
            [
                rng.range(-10.0, 14.0),
                rng.range(-1.0, 13.0),
                rng.range(-8.0, 8.0),
            ]
        };
        let n = rng.unit();
        let mut v = rng.unit();
        let nv = n[0] * v[0] + n[1] * v[1] + n[2] * v[2];
        if nv < 0.0 {
            v = [-v[0], -v[1], -v[2]];
        }
        let half = 0.5 * interior.spacing[0];
        let q = [p[0] + n[0] * half, p[1] + n[1] * half, p[2] + n[2] * half];
        if inset(&interior, q).abs() < 1e-3 {
            continue;
        }
        out.push(StaticGiQuery { p, n, v });
    }
    out
}

fn scales() -> [[f32; 3]; GI_COMPONENTS] {
    [[1.2, 1.0, 0.8], [1.0; 3], [0.5; 3], [2.0; 3]]
}

/// Largest |a − b| over the irradiance, per component and in total.
fn light_error(a: &StaticGiSample, b: &StaticGiSample) -> f32 {
    std::iter::once((&a.irradiance_over_pi, &b.irradiance_over_pi))
        .chain(a.components.iter().zip(&b.components))
        .flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| (p - q).abs()))
        .fold(0.0f32, f32::max)
}

fn peak(samples: &[StaticGiSample]) -> f32 {
    samples
        .iter()
        .flat_map(|s| s.irradiance_over_pi)
        .fold(0.0f32, f32::max)
}

/// Runs both lookups and checks every receiver: coverage, visibility and the layer shares to
/// 10⁻³, light to 0.2 % of the brightest receiver plus 0.2 % of its own value.
fn check(device: &RenderDevice, vol: &BakedGiVolume, settings: &BakedGiSettings, qs: &[StaticGiQuery]) {
    let k = scales();
    let gpu = gpu_sample_static_gi(device, vol, settings, &k, qs).expect("the GPU lookup ran");
    let cpu: Vec<StaticGiSample> = qs
        .iter()
        .map(|q| sample_static_gi(vol, settings, &k, q.p, q.n, q.v))
        .collect();
    assert_eq!(gpu.len(), cpu.len());
    let scale = peak(&cpu);
    assert!(scale > 0.0, "the receivers saw no baked light");
    let (mut covered, mut blended, mut interior, mut hidden) = (0, 0, 0, 0);
    let mut worst = (0.0f32, 0usize);
    for (i, (g, c)) in gpu.iter().zip(&cpu).enumerate() {
        let q = qs[i];
        assert!(
            (g.coverage - c.coverage).abs() <= 1e-3,
            "receiver {i} {q:?}: coverage gpu {} cpu {}",
            g.coverage,
            c.coverage
        );
        assert!(
            (g.visibility - c.visibility).abs() <= 1e-3,
            "receiver {i} {q:?}: visibility gpu {} cpu {}",
            g.visibility,
            c.visibility
        );
        // The probe-relative AO split (ADR-1104): the layers' shares of the coverage.
        assert!(
            (g.brick_share - c.brick_share).abs() <= 1e-3
                && (g.terrain_share - c.terrain_share).abs() <= 1e-3,
            "receiver {i} {q:?}: shares gpu ({}, {}) cpu ({}, {})",
            g.brick_share,
            g.terrain_share,
            c.brick_share,
            c.terrain_share
        );
        let own = c.irradiance_over_pi.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let e = light_error(g, c);
        let allowed = 2e-3 * scale + 2e-3 * own;
        assert!(
            e <= allowed,
            "receiver {i} {q:?}: the GPU lookup departs from the CPU reference by {e} (allowed {allowed})\n gpu {g:?}\n cpu {c:?}"
        );
        if e / scale > worst.0 {
            worst = (e / scale, i);
        }
        if c.coverage > 0.0 {
            covered += 1;
        }
        if c.coverage > 0.0 && c.visibility < 0.5 {
            hidden += 1;
        }
        let rb = inset(&regular_brick(), q.p);
        if rb > 0.0 && rb < 1.0 {
            blended += 1;
        }
        if inset(&interior_brick(), q.p) > 0.0 {
            interior += 1;
        }
    }
    eprintln!(
        "gpu vs cpu: {} receivers, {covered} covered, {blended} in the brick's blend band, {interior} in the \
         interior brick, {hidden} mostly hidden from their probes; worst {:.2e} of the peak at receiver {}",
        qs.len(),
        worst.0,
        worst.1
    );
    // The receivers exercise every path the test is about.
    assert!(covered > qs.len() / 2, "{covered} covered");
    assert!(blended >= 50, "{blended} in the blend band");
    assert!(interior >= 50, "{interior} in the interior brick");
    assert!(hidden >= 20, "{hidden} hidden");
}

#[test]
fn the_gpu_lookup_matches_its_cpu_reference_at_random_receivers() {
    let Some((_gpu, device)) = device() else {
        return;
    };
    let vol = volume(0x5eed_0001);
    let qs = queries(0x5eed_0002, 4096);
    check(&device, &vol, &BakedGiSettings::default(), &qs);
}

#[test]
fn the_gpu_lookup_matches_with_other_biases_and_intensity() {
    let Some((_gpu, device)) = device() else {
        return;
    };
    let vol = volume(0x5eed_0003);
    let qs = queries(0x5eed_0004, 2048);
    let settings = BakedGiSettings {
        normal_bias: 0.25,
        view_bias: 0.05,
        intensity: 1.7,
        ..BakedGiSettings::default()
    };
    check(&device, &vol, &settings, &qs);
}

#[test]
fn a_uniform_volume_reads_its_radiance_on_the_gpu() {
    let Some((_gpu, device)) = device() else {
        return;
    };
    // Every probe: the sky component only, uniform radiance (2, 1, 0.5), open moments.
    let layout = {
        let mut l = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [4, 4],
                levels: vec![1.0, 2.0, 4.0],
                ground: vec![10.0; 16],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        l.assign_brick_offsets();
        l
    };
    let k = 4.0 * std::f32::consts::PI * SH_Y00;
    let sky: ShRgb = [
        [2.0 * k, 0.0, 0.0, 0.0],
        [k, 0.0, 0.0, 0.0],
        [0.5 * k, 0.0, 0.0, 0.0],
    ];
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [[[0.0; 4]; 3], sky, [[0.0; 4]; 3], [[0.0; 4]; 3]],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let vol = BakedGiVolume::from_probes(
        &layout,
        BakeLights {
            sky_scale: 1.0,
            ..BakeLights::default()
        },
        BakeInfo::default(),
        &probes,
    )
    .expect("volume");
    let up = [0.0, 1.0, 0.0];
    let qs = [
        StaticGiQuery {
            p: [6.0, 11.5, 6.0],
            n: up,
            v: up,
        },
        // A column spacing outside the layer: nothing.
        StaticGiQuery {
            p: [-5.0, 11.5, 6.0],
            n: up,
            v: up,
        },
    ];
    let k = [[0.0; 3], [1.0; 3], [0.0; 3], [0.0; 3]];
    let s = gpu_sample_static_gi(&device, &vol, &BakedGiSettings::default(), &k, &qs).expect("lookup");
    assert!((s[0].coverage - 1.0).abs() < 1e-5, "{:?}", s[0]);
    assert!((s[0].visibility - 1.0).abs() < 1e-5, "{:?}", s[0]);
    for (got, want) in s[0].irradiance_over_pi.iter().zip([2.0, 1.0, 0.5]) {
        assert!((got - want).abs() < 0.02 * want, "{:?}", s[0]);
    }
    assert!((s[0].components[1][0] - s[0].irradiance_over_pi[0]).abs() < 1e-5);
    assert_eq!(s[1].coverage, 0.0);
    assert_eq!(s[1].irradiance_over_pi, [0.0; 3]);
    // No receivers: no GPU work, no error.
    assert!(
        gpu_sample_static_gi(&device, &vol, &BakedGiSettings::default(), &k, &[])
            .expect("empty")
            .is_empty()
    );
}

/// ADR-861: the shader's de-ringing lobe and leak guard against the CPU mirror, on a volume where
/// a third of the probes carry a wall code (beside the tenth that is rejected), with the quality
/// switches on (the defaults) and off.
#[test]
fn the_gpu_lookup_matches_with_walls_and_the_quality_switches() {
    let Some((_gpu, device)) = device() else {
        return;
    };
    let vol = volume_with_walls(0x5eed_0861, 0.33);
    assert!(
        vol.wall_probe_count() > vol.probe_count() / 5,
        "{} walled",
        vol.wall_probe_count()
    );
    let qs = queries(0x5eed_0862, 4096);
    check(&device, &vol, &BakedGiSettings::default(), &qs);
    let off = BakedGiSettings {
        deringing: false,
        leak_guard: false,
        sun_relight: false,
        ..BakedGiSettings::default()
    };
    check(&device, &vol, &off, &qs);
}
