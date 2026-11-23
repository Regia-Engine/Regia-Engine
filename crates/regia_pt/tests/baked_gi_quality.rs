// SPDX-License-Identifier: MPL-2.0
//! Baked GI quality pass (ADR-861): the probe-volume lookup against analytic irradiance, on the
//! CPU only (the CPU probe tracer bakes, the CPU mirror of the shader looks up), so it runs on
//! every host without a GPU.
//!
//! Each scene is baked once, then looked up twice: the **baseline** (the ADR-861 switches off:
//! `deringing`, `leak_guard`, `sun_relight`) and the **quality** settings (the defaults). The
//! test prints the error table (and writes it to `$REGIA_GI_TABLE/baked_gi_quality.md` when
//! that is set), asserts that no metric of any scene gets worse, and that each change fixes what
//! it targets:
//!
//! - `open_ground`: an infinite Lambertian ground under a uniform sky and a sun. Exact: a
//!   receiver with normal `n` (c = n·up) reads `L (1 + c)/2 + ρ L (1 − c)/2` of sky and
//!   `ρ E sin h / π · (1 − c)/2` of sun bounce. Baked at h = 60°, looked up at 60° and, for the
//!   sun relight, at 25° and 80°.
//! - `emissive_panel`: a 2 × 2 m one-sided emitter facing a black floor, nothing else. Exact by
//!   Lambert's polygon formula on the panel clipped to the receiver's hemisphere. The probes see
//!   a narrow bright source, where the L1 evaluation rings (de-ringing).
//! - `sealed_room`: a closed room with 10 cm walls on the open ground, probes 0.78 m apart (some
//!   under the floor). Exact inside: 0. The leak is the light read inside over the open ground's.

use std::f32::consts::PI;
use std::fmt::Write as _;

use regia_asset::baked_gi::{BakedGiVolume, BrickFlags, DetailBrick, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::{PtCamera, PtMaterial, PtMesh, PtScene, PtSky, PtSun};
use regia_render::{
    BakedGiFrameLights, BakedGiSettings, SunRelight, component_scales, sample_static_gi_relit, sun_relight,
};

const SKY: f32 = 1.0;
const GROUND_ALBEDO: f32 = 0.5;
const SUN_LUX: f32 = 10.0;
const SUN_AZIMUTH_DEG: f32 = 20.0;
const BAKED_SUN_DEG: f32 = 60.0;

/// A deterministic stream of uniform numbers in `[0, 1)` (SplitMix64).
struct Stream(u64);

impl Stream {
    fn uniform(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.uniform()
    }

    /// A unit vector, uniform over the sphere.
    fn direction(&mut self) -> Vec3 {
        let y = 1.0 - 2.0 * self.uniform();
        let r = (1.0 - y * y).max(0.0).sqrt();
        let phi = 2.0 * PI * self.uniform();
        Vec3::new(r * phi.cos(), y, r * phi.sin())
    }
}

/// An axis-aligned box `min..max` facing outward, 12 triangles.
fn box_mesh(min: [f32; 3], max: [f32; 3]) -> PtMesh {
    let corner = |i: usize| {
        [
            if i & 1 == 0 { min[0] } else { max[0] },
            if i & 2 == 0 { min[1] } else { max[1] },
            if i & 4 == 0 { min[2] } else { max[2] },
        ]
    };
    let positions: Vec<[f32; 3]> = (0..8).map(corner).collect();
    // Counter-clockwise seen from outside.
    let faces: [[u32; 4]; 6] = [
        [0, 4, 6, 2],
        [1, 3, 7, 5],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 2, 3, 1],
        [4, 5, 7, 6],
    ];
    let indices = faces
        .iter()
        .flat_map(|f| [f[0], f[1], f[2], f[0], f[2], f[3]])
        .collect();
    PtMesh {
        normals: vec![[0.0; 3]; positions.len()],
        positions,
        indices,
        ..PtMesh::default()
    }
}

/// A horizontal quad at height `y` over `-half..half` in x and z, facing up or down.
fn quad(y: f32, half: f32, up: bool) -> PtMesh {
    let indices = if up {
        vec![0, 2, 1, 1, 2, 3]
    } else {
        vec![0, 1, 2, 1, 3, 2]
    };
    PtMesh {
        positions: vec![
            [-half, y, -half],
            [half, y, -half],
            [-half, y, half],
            [half, y, half],
        ],
        normals: vec![[0.0, if up { 1.0 } else { -1.0 }, 0.0]; 4],
        indices,
        ..PtMesh::default()
    }
}

/// The unit vector toward a sun at elevation `deg` (azimuth `SUN_AZIMUTH_DEG`).
fn to_sun(deg: f32) -> Vec3 {
    let (h, a) = (deg.to_radians(), SUN_AZIMUTH_DEG.to_radians());
    Vec3::new(h.cos() * a.cos(), h.sin(), h.cos() * a.sin())
}

fn sun(deg: f32) -> PtSun {
    PtSun {
        toward: to_sun(deg).to_array(),
        color: [1.0; 3],
        illuminance: SUN_LUX,
        angular_radius: 0.0,
    }
}

fn frame_lights(deg: f32) -> BakedGiFrameLights {
    BakedGiFrameLights {
        sun: Some([SUN_LUX; 3]),
        to_sun: Some(to_sun(deg).to_array()),
        fill: None,
    }
}

/// The open ground under the sky and the baked sun.
fn ground_scene() -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 8.0], [0.0; 3], 0.8, 4, 4));
    let floor = scene.add_material(PtMaterial::lambert([GROUND_ALBEDO; 3]));
    let plane = scene.add_mesh(quad(0.0, 200.0, true));
    scene.add_instance(plane, floor, Mat4::IDENTITY);
    scene.sun = Some(sun(BAKED_SUN_DEG));
    scene.sky = PtSky::Uniform([SKY; 3]);
    scene
}

/// Inner half extent (x, z) and height of the sealed room, and its wall thickness.
const ROOM: (f32, f32, f32) = (1.5, 2.5, 1.5);
const WALL: f32 = 0.1;

fn room_scene() -> PtScene {
    let mut scene = ground_scene();
    let wall = scene.add_material(PtMaterial::lambert([GROUND_ALBEDO; 3]));
    let (x, h, z) = ROOM;
    let w = WALL;
    for (min, max) in [
        ([-x - w, h, -z - w], [x + w, h + w, z + w]),
        ([-x - w, 0.0, -z - w], [-x, h, z + w]),
        ([x, 0.0, -z - w], [x + w, h, z + w]),
        ([-x, 0.0, -z - w], [x, h, -z]),
        ([-x, 0.0, z], [x, h, z + w]),
    ] {
        let mesh = scene.add_mesh(box_mesh(min, max));
        scene.add_instance(mesh, wall, Mat4::IDENTITY);
    }
    scene
}

/// The emitter of `panel_scene`: radiance, height, half side.
const PANEL: (f32, f32, f32) = (10.0, 3.0, 1.0);

fn panel_scene() -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 8.0], [0.0; 3], 0.8, 4, 4));
    let black = scene.add_material(PtMaterial::lambert([0.0; 3]));
    let light = scene.add_material(PtMaterial::emitter([PANEL.0; 3]));
    let plane = scene.add_mesh(quad(0.0, 200.0, true));
    scene.add_instance(plane, black, Mat4::IDENTITY);
    let panel = scene.add_mesh(quad(PANEL.1, PANEL.2, false));
    scene.add_instance(panel, light, Mat4::IDENTITY);
    scene.sky = PtSky::None;
    scene
}

fn bake(scene: &PtScene, min: [f32; 3], max: [f32; 3], spacing: f32) -> BakedGiVolume {
    bake_bricks(scene, vec![detail_brick(min, max, spacing)])
}

fn bake_bricks(scene: &PtScene, bricks: Vec<DetailBrick>) -> BakedGiVolume {
    let layout = VolumeLayout {
        terrain: None,
        bricks,
        moment_resolution: 4,
        max_distance: 16.0,
    };
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 2,
            max_bounces: 2,
            seed: 7,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    };
    bake_gi_volume(&CpuProbeTracer::new(), scene, &layout, &settings, |_, _, _| true)
        .expect("CPU bake")
        .volume
}

/// Exact irradiance / π over the open ground for a receiver facing `n`, sun at `deg`.
fn ground_truth(n: Vec3, deg: f32) -> f32 {
    let c = n.y;
    let sky = SKY * (1.0 + c) / 2.0 + GROUND_ALBEDO * SKY * (1.0 - c) / 2.0;
    let sun = GROUND_ALBEDO * SUN_LUX * to_sun(deg).y / PI * (1.0 - c) / 2.0;
    sky + sun
}

/// Irradiance / π at `p` (unit normal `n`) from a one-sided Lambertian emitter `polygon` of
/// radiance `le` lit toward `facing`: Lambert's polygon formula, `E = le/2 · |Σ θ_i (γ_i·n)|`
/// over the polygon clipped to the receiver's hemisphere (Sutherland–Hodgman), θ_i the angle
/// each edge subtends and γ_i the unit normal of its plane through `p` (Arvo 1995).
fn polygon_irradiance_over_pi(p: Vec3, n: Vec3, polygon: &[Vec3], facing: Vec3, le: f32) -> f32 {
    if (p - polygon[0]).dot(facing) <= 0.0 {
        return 0.0;
    }
    let rel: Vec<Vec3> = polygon.iter().map(|q| *q - p).collect();
    let mut clipped = Vec::new();
    for (a, b) in rel.iter().zip(rel.iter().cycle().skip(1)) {
        let (da, db) = (a.dot(n), b.dot(n));
        if da >= 0.0 {
            clipped.push(*a);
        }
        if (da >= 0.0) != (db >= 0.0) {
            clipped.push(*a + (*b - *a) * (da / (da - db)));
        }
    }
    if clipped.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for (a, b) in clipped.iter().zip(clipped.iter().cycle().skip(1)) {
        let (a, b) = (a.normalize_or_zero(), b.normalize_or_zero());
        let g = a.cross(b);
        let len = g.length();
        if len > 1e-9 {
            // atan2 keeps small angles accurate where acos of a dot near 1 does not.
            let theta = len.atan2(a.dot(b));
            sum += theta * g.dot(n) / len;
        }
    }
    le * 0.5 * sum.abs() / PI
}

fn panel_polygon() -> [Vec3; 4] {
    let (_, y, h) = PANEL;
    [
        Vec3::new(-h, y, -h),
        Vec3::new(h, y, -h),
        Vec3::new(h, y, h),
        Vec3::new(-h, y, h),
    ]
}

/// A receiver: position, unit normal, unit vector toward the viewer.
type Receiver = (Vec3, Vec3, Vec3);

fn toward(from: Vec3, to: Vec3) -> Vec3 {
    (to - from).normalize_or(Vec3::Y)
}

fn ground_receivers() -> Vec<Receiver> {
    let mut s = Stream(0x5eed_0861);
    let eye = Vec3::new(0.0, 2.0, 8.0);
    (0..80)
        .map(|i| {
            let (x, z) = (s.range(-2.5, 2.5), s.range(-2.5, 2.5));
            if i < 40 {
                let p = Vec3::new(x, 0.0, z);
                (p, Vec3::Y, toward(p, eye))
            } else {
                let p = Vec3::new(x, s.range(0.2, 2.0), z);
                (p, s.direction(), toward(p, eye))
            }
        })
        .collect()
}

fn panel_receivers() -> Vec<Receiver> {
    let mut s = Stream(0x5eed_0862);
    let eye = Vec3::new(1.0, 1.5, 6.0);
    (0..160)
        .map(|_| {
            let p = Vec3::new(s.range(-2.5, 2.5), s.range(0.3, 2.3), s.range(-2.5, 2.5));
            (p, s.direction(), toward(p, eye))
        })
        .collect()
}

/// Inside the sealed room: floor, ceiling and two inner walls, in turn.
fn room_receivers() -> Vec<Receiver> {
    let mut s = Stream(0x5eed_0863);
    let (x, h, z) = ROOM;
    let eye = Vec3::new(0.0, 1.2, 0.0);
    (0..80)
        .map(|i| {
            let (p, n) = match i % 4 {
                0 => (Vec3::new(s.range(-x, x), 0.0, s.range(-z, z)), Vec3::Y),
                1 => (Vec3::new(s.range(-x, x), h, s.range(-z, z)), Vec3::NEG_Y),
                2 => (Vec3::new(x, s.range(0.0, h), s.range(-z, z)), Vec3::NEG_X),
                _ => (Vec3::new(s.range(-x, x), s.range(0.0, h), -z), Vec3::Z),
            };
            (p, n, toward(p, eye))
        })
        .collect()
}

fn baseline() -> BakedGiSettings {
    BakedGiSettings {
        deringing: false,
        leak_guard: false,
        sun_relight: false,
        ..BakedGiSettings::default()
    }
}

/// Irradiance / π (luminance) of every receiver under `settings` and the frame's sun at `deg`.
fn lookup(volume: &BakedGiVolume, settings: &BakedGiSettings, deg: f32, receivers: &[Receiver]) -> Vec<f32> {
    let lights = frame_lights(deg);
    let scales = component_scales(volume, settings, &lights);
    let relight: SunRelight = sun_relight(volume, settings, &lights);
    receivers
        .iter()
        .map(|(p, n, v)| {
            let s = sample_static_gi_relit(
                volume,
                settings,
                &scales,
                &relight,
                p.to_array(),
                n.to_array(),
                v.to_array(),
            );
            let e = s.irradiance_over_pi;
            0.2126 * e[0] + 0.7152 * e[1] + 0.0722 * e[2]
        })
        .collect()
}

/// One row of the error table.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    /// RMSE over the mean truth (or over `scale` for a black truth).
    rel_rmse: f64,
    /// Mean estimate over mean truth (1 = right energy); `None` for a black truth.
    energy: Option<f64>,
    /// Largest error over the mean truth (or `scale`).
    max_err: f64,
    /// Mean estimate where the truth is (near) black, over the mean truth (or `scale`): light
    /// behind a source or leaked into a sealed room.
    dark_light: f64,
}

fn measure(estimate: &[f32], truth: &[f32], scale: Option<f64>) -> Metrics {
    assert_eq!(estimate.len(), truth.len());
    let n = truth.len() as f64;
    let mean_truth = truth.iter().map(|&t| f64::from(t)).sum::<f64>() / n;
    let norm = scale.unwrap_or(mean_truth).max(1e-9);
    let mut sq = 0.0f64;
    let mut max_err = 0.0f64;
    let (mut dark_sum, mut dark_n) = (0.0f64, 0usize);
    let dark = 1e-3 * norm;
    for (&e, &t) in estimate.iter().zip(truth) {
        let err = f64::from(e) - f64::from(t);
        sq += err * err;
        max_err = max_err.max(err.abs());
        if f64::from(t) <= dark {
            dark_sum += f64::from(e);
            dark_n += 1;
        }
    }
    let mean_est = estimate.iter().map(|&e| f64::from(e)).sum::<f64>() / n;
    Metrics {
        rel_rmse: (sq / n).sqrt() / norm,
        energy: (scale.is_none() && mean_truth > 1e-9).then_some(mean_est / mean_truth),
        max_err: max_err / norm,
        dark_light: if dark_n > 0 {
            dark_sum / dark_n as f64 / norm
        } else {
            0.0
        },
    }
}

/// `quality` is no worse than `base` on any metric (a small allowance for float noise); the
/// energy ratio only when `energy` (see the panel below).
fn never_worse(scene: &str, base: &Metrics, quality: &Metrics, energy: bool) {
    let tol = |b: f64| b * 1e-3 + 1e-5;
    assert!(
        quality.rel_rmse <= base.rel_rmse + tol(base.rel_rmse),
        "{scene}: rel RMSE {base:?} → {quality:?}"
    );
    assert!(
        quality.max_err <= base.max_err + tol(base.max_err),
        "{scene}: max error {base:?} → {quality:?}"
    );
    assert!(
        quality.dark_light <= base.dark_light + tol(base.dark_light),
        "{scene}: light in the dark {base:?} → {quality:?}"
    );
    if let (true, Some(b), Some(q)) = (energy, base.energy, quality.energy) {
        assert!(
            (q - 1.0).abs() <= (b - 1.0).abs() + 0.01,
            "{scene}: energy {b} → {q}"
        );
    }
}

fn row(table: &mut String, scene: &str, variant: &str, m: &Metrics) {
    let energy = m.energy.map_or("–".to_owned(), |e| format!("{e:.3}"));
    let _ = writeln!(
        table,
        "| {scene} | {variant} | {:.4} | {energy} | {:.4} | {:.4} |",
        m.rel_rmse, m.max_err, m.dark_light
    );
}

/// Writes `text` to `$REGIA_GI_TABLE/baked_gi_quality.md` when the variable is set (tmp +
/// rename, so a reader never sees half a file).
fn publish(text: &str) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_GI_TABLE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("table dir");
    let tmp = dir.join(format!("baked_gi_quality.md.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).expect("write table");
    std::fs::rename(&tmp, dir.join("baked_gi_quality.md")).expect("publish table");
}

#[test]
fn lamberts_formula_matches_the_limits() {
    // A huge square 1 m above an up-facing receiver fills its hemisphere: E/π = L.
    let big = [
        Vec3::new(-1000.0, 1.0, -1000.0),
        Vec3::new(1000.0, 1.0, -1000.0),
        Vec3::new(1000.0, 1.0, 1000.0),
        Vec3::new(-1000.0, 1.0, 1000.0),
    ];
    let full = polygon_irradiance_over_pi(Vec3::ZERO, Vec3::Y, &big, Vec3::NEG_Y, 2.0);
    assert!((full - 2.0).abs() < 0.01, "{full}");
    // Facing away, or behind the one-sided emitter: nothing.
    assert_eq!(
        polygon_irradiance_over_pi(Vec3::ZERO, Vec3::NEG_Y, &big, Vec3::NEG_Y, 2.0),
        0.0
    );
    assert_eq!(
        polygon_irradiance_over_pi(Vec3::new(0.0, 2.0, 0.0), Vec3::NEG_Y, &big, Vec3::NEG_Y, 2.0),
        0.0
    );
    // A small 0.1 m square 10 m away on the axis: E ≈ L·A/d², so E/π ≈ L·A/(π d²).
    let h = 0.05;
    let small = [
        Vec3::new(-h, 10.0, -h),
        Vec3::new(h, 10.0, -h),
        Vec3::new(h, 10.0, h),
        Vec3::new(-h, 10.0, h),
    ];
    let e = polygon_irradiance_over_pi(Vec3::ZERO, Vec3::Y, &small, Vec3::NEG_Y, 1.0);
    let expected = 0.01 / (PI * 100.0);
    assert!((e - expected).abs() < 1e-3 * expected, "{e} vs {expected}");
    // Edge-on, half the hemisphere sees the huge square: E/π = L/2.
    let side = polygon_irradiance_over_pi(Vec3::ZERO, Vec3::X, &big, Vec3::NEG_Y, 2.0);
    assert!((side - 1.0).abs() < 0.01, "{side}");
}

#[test]
fn the_quality_pass_beats_the_baseline_on_the_analytic_scenes() {
    let mut table = String::from(
        "| scene | lookup | rel RMSE | energy | max error | light in the dark |\n|---|---|---|---|---|---|\n",
    );
    let quality = BakedGiSettings::default();

    // Open ground: the same sun, then a moved sun (relight).
    let ground = bake(&ground_scene(), [-4.07, -0.63, -4.07], [4.07, 3.37, 4.07], 1.5);
    let receivers = ground_receivers();
    let mut relit = Vec::new();
    for deg in [BAKED_SUN_DEG, 25.0, 80.0] {
        let truth: Vec<f32> = receivers.iter().map(|(_, n, _)| ground_truth(*n, deg)).collect();
        let b = measure(&lookup(&ground, &baseline(), deg, &receivers), &truth, None);
        let q = measure(&lookup(&ground, &quality, deg, &receivers), &truth, None);
        let name = format!("open_ground sun {deg}°");
        row(&mut table, &name, "baseline", &b);
        row(&mut table, &name, "quality", &q);
        never_worse(&name, &b, &q, true);
        relit.push((deg, b, q));
    }

    // The emitter panel (de-ringing).
    // Probe layers at −0.63 (under the floor, rejected), 0.37, 1.37 and 2.37 m, all under the
    // panel; the receivers (0.3..2.3 m) lie inside the brick.
    let panel = bake(&panel_scene(), [-3.03, -0.63, -3.03], [3.03, 2.37, 3.03], 1.0);
    let receivers = panel_receivers();
    let polygon = panel_polygon();
    let truth: Vec<f32> = receivers
        .iter()
        .map(|(p, n, _)| polygon_irradiance_over_pi(*p, *n, &polygon, Vec3::NEG_Y, PANEL.0))
        .collect();
    let panel_base = measure(
        &lookup(&panel, &baseline(), BAKED_SUN_DEG, &receivers),
        &truth,
        None,
    );
    let panel_quality = measure(&lookup(&panel, &quality, BAKED_SUN_DEG, &receivers), &truth, None);
    row(&mut table, "emissive_panel", "baseline", &panel_base);
    row(&mut table, "emissive_panel", "quality", &panel_quality);
    // Energy is not held here: de-ringing gives every probe its exact mean over the sphere
    // (`deringing_keeps_the_mean_and_first_moment_and_never_goes_negative` in regia_render),
    // while the clamped linear form adds about 9 % at |r1| = 1.8 r0, which more than makes up
    // for the interpolation's shortfall between probes near a small source (the prototype
    // measured 1.013 baseline, 0.981 de-ringed; ADR-861). The error metrics are held.
    never_worse("emissive_panel", &panel_base, &panel_quality, false);

    // The sealed room (leak guard): black inside, the open ground's up-facing light as scale.
    let room = bake(&room_scene(), [-3.13, -0.71, -3.13], [3.13, 4.5, 3.13], 0.8);
    let receivers = room_receivers();
    let truth = vec![0.0f32; receivers.len()];
    let outdoor = f64::from(ground_truth(Vec3::Y, BAKED_SUN_DEG));
    let room_base = measure(
        &lookup(&room, &baseline(), BAKED_SUN_DEG, &receivers),
        &truth,
        Some(outdoor),
    );
    let room_quality = measure(
        &lookup(&room, &quality, BAKED_SUN_DEG, &receivers),
        &truth,
        Some(outdoor),
    );
    row(&mut table, "sealed_room", "baseline", &room_base);
    row(&mut table, "sealed_room", "quality", &room_quality);
    never_worse("sealed_room", &room_base, &room_quality, true);

    // Measured, not targeted: the room with an INTERIOR brick over its inside (and a coarse
    // outdoor brick around it), the brick's faces on the inner walls or inset 0.12 m from them
    // (ADR-861 follow-up: probes on wall surfaces).
    let (x, h, z) = ROOM;
    for (name, inset) in [
        ("interior_room on the walls", 0.0f32),
        ("interior_room inset 0.12 m", 0.12),
    ] {
        let mut interior = detail_brick(
            [-x + inset, inset, -z + inset],
            [x - inset, h - inset, z - inset],
            0.75,
        );
        interior.flags = BrickFlags::INTERIOR;
        let outdoor_brick = detail_brick([-5.03, -0.71, -5.03], [5.03, 4.5, 5.03], 2.0);
        let volume = bake_bricks(&room_scene(), vec![outdoor_brick, interior]);
        let b = measure(
            &lookup(&volume, &baseline(), BAKED_SUN_DEG, &receivers),
            &truth,
            Some(outdoor),
        );
        let q = measure(
            &lookup(&volume, &quality, BAKED_SUN_DEG, &receivers),
            &truth,
            Some(outdoor),
        );
        row(&mut table, name, "baseline", &b);
        row(&mut table, name, "quality", &q);
        never_worse(name, &b, &q, true);
    }

    println!("{table}");
    publish(&table);

    // What each change targets.
    for (deg, b, q) in &relit {
        if *deg == BAKED_SUN_DEG {
            continue;
        }
        assert!(q.rel_rmse < 0.5 * b.rel_rmse, "sun {deg}°: relight {b:?} → {q:?}");
        assert!(
            q.rel_rmse < 0.05,
            "sun {deg}°: the relit ground is close to exact: {q:?}"
        );
    }
    assert!(
        panel_quality.rel_rmse < panel_base.rel_rmse,
        "de-ringing lowers the panel's error: {panel_base:?} → {panel_quality:?}"
    );
    assert!(
        room_quality.dark_light < 0.05,
        "the sealed room stays dark with the leak guard: {room_quality:?}"
    );
}
