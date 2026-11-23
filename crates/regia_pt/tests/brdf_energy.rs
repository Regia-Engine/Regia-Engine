// SPDX-License-Identifier: MPL-2.0
//! ADR-2632: the path tracer's white furnace under `BrdfEnergy::Compensated`, on whatever
//! adapter `RenderDevice::headless` picks (lavapipe in CI). The tracer runs the shared
//! `pbr_brdf.wgsl::with_compensation` the forward pass's `BRDF_ENERGY_COMPENSATED` variant runs,
//! so this is the GPU side of the CPU sweep in `cpu_brdf.rs`.
//!
//! `REGIA_BRDF_ENERGY_PLATES=<dir>` also writes the before/after plates of
//! `docs/evidence/brdf-energy/` there (the test never writes into the source tree on its own).

use std::sync::OnceLock;

use regia_core::glam::Vec3;
use regia_pt::cpu_brdf::{DfgTable, directional_albedo_energy};
use regia_pt::{Aov, PathTracer, PtCamera, PtImage, PtMaterial, PtScene, PtSettings, scenes};
use regia_render::{BrdfEnergy, RenderDevice};

/// The CPU sweep's stated tolerance (`cpu_brdf.rs`, `COMPENSATED_FURNACE_TOLERANCE`) plus the
/// Monte Carlo noise of a 16×16 furnace at 1024 samples per pixel.
const GPU_FURNACE_TOLERANCE: f64 = 0.015;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn render(scene: &PtScene, spp: u32) -> PtImage {
    let settings = PtSettings {
        spp,
        max_bounces: 4,
        seed: 7,
        rr_start: 3,
        jitter: false,
        samples_per_submit: 64,
    };
    tracer().render(scene, &settings).expect("render")
}

fn pixel_ray(cam: &PtCamera, x: u32, y: u32) -> (Vec3, Vec3) {
    let (fwd, right, up) = cam.basis();
    let th = (0.5 * cam.fov_y).tan();
    let aspect = cam.width as f32 / cam.height as f32;
    let sx = ((x as f32 + 0.5) / cam.width as f32) * 2.0 - 1.0;
    let sy = 1.0 - ((y as f32 + 0.5) / cam.height as f32) * 2.0;
    let d = (fwd + right * (sx * th * aspect) + up * (sy * th)).normalize();
    (Vec3::from_array(cam.eye), d)
}

/// n·v of the unit sphere at pixel (x, y), `None` off the sphere.
fn sphere_no_v(cam: &PtCamera, x: u32, y: u32) -> Option<f32> {
    let (o, d) = pixel_ray(cam, x, y);
    let b = o.dot(d);
    let c = o.dot(o) - 1.0;
    let disc = b * b - c;
    if disc <= 0.0 {
        return None;
    }
    let t = -b - disc.sqrt();
    Some((o + d * t).normalize().dot(-d))
}

struct Furnace {
    /// Mean over the sphere: the tracer, and the CPU model's directional albedo.
    pt: f64,
    cpu: f64,
    /// Mean of the rim pixels (n·v < 0.35), where the legacy excess lives.
    rim_pt: f64,
}

fn furnace(m: &PtMaterial, energy: BrdfEnergy, size: u32, spp: u32) -> (Furnace, PtImage) {
    let dfg = DfgTable::default();
    let mut scene = scenes::furnace_sphere(m.clone(), size);
    // The whole sphere in frame (angular radius asin(1/3) = 19.5°, half the field 20.6°), so the
    // rim's grazing views, where the legacy excess lives, are on screen.
    scene.camera.fov_y = 0.72;
    scene.brdf_energy = energy;
    let img = render(&scene, spp);
    assert_eq!(img.metadata.rejected_samples, 0);
    let table: Vec<f64> = (0..=32)
        .map(|i| directional_albedo_energy(m, &dfg, energy, (i as f32 / 32.0).max(0.02), 0, 256, 128))
        .collect();
    let albedo_at = |no_v: f32| {
        let f = no_v.clamp(0.0, 1.0) * 32.0;
        let i = (f.floor() as usize).min(31);
        let t = f64::from(f - i as f32);
        table[i] * (1.0 - t) + table[i + 1] * t
    };
    let (mut pt, mut cpu, mut n) = (0.0f64, 0.0f64, 0usize);
    let (mut rim, mut rim_n) = (0.0f64, 0usize);
    for y in 0..size {
        for x in 0..size {
            let Some(no_v) = sphere_no_v(&scene.camera, x, y) else {
                continue;
            };
            if no_v < 0.03 {
                // The tessellated silhouette: the mesh's facets and the analytic sphere disagree.
                continue;
            }
            let v = f64::from(img.at(Aov::Beauty, x, y)[0]);
            pt += v;
            cpu += albedo_at(no_v);
            n += 1;
            if no_v < 0.35 {
                rim += v;
                rim_n += 1;
            }
        }
    }
    let rim_pt = if rim_n > 0 { rim / rim_n as f64 } else { 0.0 };
    (
        Furnace {
            pt: pt / n as f64,
            cpu: cpu / n as f64,
            rim_pt,
        },
        img,
    )
}

fn white(metallic: f32, roughness: f32, multi_scatter: bool) -> PtMaterial {
    PtMaterial {
        base_color: [1.0; 4],
        metallic,
        roughness,
        multi_scatter,
        ..PtMaterial::default()
    }
}

/// The compensated tracer matches the CPU model (the same BRDF in WGSL and Rust) and its white
/// furnace stays at or under 1 everywhere on the sphere, rim included; the legacy tracer's rim
/// of a white dielectric reflects more than it receives (the finding this mode fixes).
#[test]
fn compensated_furnace_matches_the_cpu_model_and_stays_under_one() {
    let mut report = Vec::new();
    let mut configs = Vec::new();
    for r in [0.05f32, 0.25, 0.5, 1.0] {
        configs.push(("dielectric ms", white(0.0, r, true)));
        configs.push(("dielectric ss", white(0.0, r, false)));
    }
    for r in [0.5f32, 1.0] {
        configs.push(("metal ms", white(1.0, r, true)));
    }
    for (name, m) in &configs {
        let (f, img) = furnace(m, BrdfEnergy::Compensated, 16, 1024);
        let rel = (f.pt / f.cpu - 1.0).abs();
        let max = img.beauty.iter().map(|c| f64::from(c[0])).fold(0.0f64, f64::max);
        report.push(format!(
            "{name} r={} pt={:.4} cpu={:.4} rel={rel:.4} rim={:.4} max={max:.4}",
            m.roughness, f.pt, f.cpu, f.rim_pt
        ));
        assert!(
            rel < 0.005,
            "{name} r {}: pt {} vs cpu {}",
            m.roughness,
            f.pt,
            f.cpu
        );
        assert!(
            f.pt <= 1.0 + GPU_FURNACE_TOLERANCE && f.rim_pt <= 1.0 + GPU_FURNACE_TOLERANCE,
            "{name} r {}: mean {} rim {}",
            m.roughness,
            f.pt,
            f.rim_pt
        );
    }
    eprintln!("compensated furnace:\n{}", report.join("\n"));
    // The legacy rim of the same white dielectric (single-scatter fallback) is far over 1.
    let (legacy, _) = furnace(&white(0.0, 0.5, false), BrdfEnergy::Legacy, 16, 1024);
    eprintln!(
        "legacy dielectric ss r=0.5: mean {:.4} rim {:.4}",
        legacy.pt, legacy.rim_pt
    );
    assert!(legacy.rim_pt > 1.05, "legacy rim {}", legacy.rim_pt);
}

/// A legacy scene packs the energy code 0 and renders exactly as before: the compensated flag is
/// the only difference between two renders of one scene.
#[test]
fn the_default_scene_is_legacy_and_the_flag_changes_the_image() {
    let m = white(0.0, 0.5, true);
    let mut scene = scenes::furnace_sphere(m.clone(), 8);
    // `PtScene::new` takes the process-wide model: legacy unless REGIA_BRDF_ENERGY says otherwise.
    assert_eq!(scene.brdf_energy, regia_render::process_brdf_energy());
    scene.brdf_energy = BrdfEnergy::Legacy;
    let a = render(&scene, 64);
    let b = render(&scene, 64);
    assert_eq!(a.beauty, b.beauty, "the tracer is deterministic for one seed");
    let mut compensated = scene.clone();
    compensated.brdf_energy = BrdfEnergy::Compensated;
    let c = render(&compensated, 64);
    assert_ne!(a.beauty, c.beauty);
}

/// `REGIA_BRDF_ENERGY_PLATES=<dir>`: the before/after plates. One row per model, white spheres
/// from roughness 0.05 to 1 (dielectric with and without multi-scatter, then metal) in a uniform
/// furnace of radiance 1; `furnace-<model>.png` is the linear value as grey (1.0 = white) and
/// `excess-<model>.png` paints every pixel over 1.005 red in proportion to the excess (full red
/// at 1.25).
#[test]
fn write_plates_when_asked() {
    let Some(dir) = std::env::var_os("REGIA_BRDF_ENERGY_PLATES") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("plates dir");
    let size = 32u32;
    let rows: [(&str, f32, bool); 3] = [
        ("dielectric ms", 0.0, true),
        ("dielectric ss", 0.0, false),
        ("metal ms", 1.0, true),
    ];
    let roughness = [0.05f32, 0.25, 0.5, 0.75, 1.0];
    let mut lines = Vec::new();
    for energy in BrdfEnergy::ALL {
        let width = size * roughness.len() as u32;
        let height = size * rows.len() as u32;
        let mut grey = vec![0u8; (width * height * 4) as usize];
        let mut excess = vec![0u8; (width * height * 4) as usize];
        for (ri, (name, metallic, ms)) in rows.iter().enumerate() {
            for (ci, r) in roughness.iter().enumerate() {
                let (f, img) = furnace(&white(*metallic, *r, *ms), energy, size, 256);
                lines.push(format!(
                    "{} {name} r={r}: mean {:.4} rim {:.4} cpu {:.4}",
                    energy.name(),
                    f.pt,
                    f.rim_pt,
                    f.cpu
                ));
                for y in 0..size {
                    for x in 0..size {
                        let v = img.at(Aov::Beauty, x, y)[0];
                        let gx = ci as u32 * size + x;
                        let gy = ri as u32 * size + y;
                        let i = ((gy * width + gx) * 4) as usize;
                        let g = (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
                        grey[i..i + 4].copy_from_slice(&[g, g, g, 255]);
                        let e = ((v - 1.005) / 0.245).clamp(0.0, 1.0);
                        let base = (v.clamp(0.0, 1.0) * 96.0) as u8;
                        let red = if e > 0.0 { 128 + (e * 127.0) as u8 } else { base };
                        excess[i..i + 4].copy_from_slice(&[red, base, base, 255]);
                    }
                }
            }
        }
        regia_pt::output::write_png(
            &dir.join(format!("furnace-{}.png", energy.name())),
            width,
            height,
            &grey,
        )
        .expect("write plate");
        regia_pt::output::write_png(
            &dir.join(format!("excess-{}.png", energy.name())),
            width,
            height,
            &excess,
        )
        .expect("write plate");
    }
    std::fs::write(dir.join("furnace-values.txt"), lines.join("\n") + "\n").expect("write values");
}
