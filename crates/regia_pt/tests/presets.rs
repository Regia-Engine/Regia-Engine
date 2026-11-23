// SPDX-License-Identifier: MPL-2.0
//! ADR-2433: quality presets, the built-in a-trous denoiser, adaptive sampling and the caustics
//! modes, on the real kernels (lavapipe in CI). No wall-clock assertions.

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::denoise_atrous::{self, AtrousSettings};
use regia_pt::scenes;
use regia_pt::{
    AdaptiveSettings, Aov, CausticsMode, DenoiserKind, PathTracer, PtCamera, PtImage, PtMaterial, PtPreset,
    PtQuality, PtScene, PtSettings, PtSky, PtSun, render_preset,
};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings(spp: u32, bounces: u32) -> PtSettings {
    PtSettings {
        spp,
        max_bounces: bounces,
        seed: 5,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 8,
    }
}

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

fn all_planes_bits(img: &PtImage) -> Vec<u32> {
    let mut bits = Vec::new();
    for aov in Aov::ALL {
        for c in img.rgb(aov) {
            bits.extend(c.iter().map(|v| v.to_bits()));
        }
    }
    bits
}

fn rmse(a: &[[f32; 3]], b: &[[f32; 3]]) -> f64 {
    let mut e = 0.0f64;
    for (p, q) in a.iter().zip(b) {
        for k in 0..3 {
            e += f64::from(p[k] - q[k]).powi(2);
        }
    }
    (e / (a.len() * 3) as f64).sqrt()
}

fn mean_lum(v: &[[f32; 3]]) -> f64 {
    v.iter().map(|c| lum(*c)).sum::<f64>() / v.len() as f64
}

#[test]
fn reference_preset_is_the_base_byte_for_byte() {
    let scenes: Vec<(&str, PtScene, PtSettings)> = vec![
        (
            "furnace",
            scenes::furnace_sphere(PtMaterial::lambert([0.8; 3]), 16),
            settings(24, 4),
        ),
        ("cornell", scenes::cornell_box(24, 24, true), settings(16, 5)),
    ];
    for (name, scene, base) in scenes {
        let plain = tracer().render(&scene, &base).expect("base render");
        let table = PtPreset::Reference.settings_for(&base);
        assert!(table.is_base());
        let via = render_preset(tracer(), &scene, &base, &table).expect("preset render");
        assert!(via.raw.is_none(), "{name}: nothing was post-processed");
        assert!(via.report.base);
        assert_eq!(
            all_planes_bits(&plain),
            all_planes_bits(&via.image),
            "{name} differs from the base"
        );
        assert!(via.image.metadata.denoiser.is_empty());
        // The default quality is the base too.
        let mut s = tracer()
            .begin_with_quality(&scene, &base, &PtQuality::default())
            .expect("session");
        s.run(base.spp).expect("run");
        assert_eq!(
            all_planes_bits(&plain),
            all_planes_bits(&s.image().expect("image")),
            "{name}: default quality differs"
        );
    }
}

/// Cornell box, an 8 spp trace against a converged one.
#[test]
fn the_builtin_denoiser_beats_the_noise_on_the_cornell_box() {
    let scene = scenes::cornell_box(64, 64, true);
    let converged = tracer().render(&scene, &settings(768, 6)).expect("reference");
    let table = {
        let mut t = PtPreset::Preview.settings();
        t.spp = 8;
        t.max_bounces = 6;
        t.quality = PtQuality::default();
        t.firefly_threshold = 0.0;
        t.adaptive.enabled = false;
        t
    };
    let run = render_preset(tracer(), &scene, &settings(8, 6), &table).expect("preview");
    let raw = run.raw.as_ref().expect("raw kept");
    // The lit part: the emitter's own pixels are passed through unfiltered by design (a bright
    // source is never blurred), so their edge noise is not the denoiser's to remove.
    let lit = |img: &PtImage| -> Vec<[f32; 3]> {
        img.beauty
            .iter()
            .zip(&img.emissive)
            .map(|(b, e)| [0, 1, 2].map(|k| (b[k] - e[k]).max(0.0)))
            .collect()
    };
    let truth = lit(&converged);
    let before = rmse(&lit(raw), &truth);
    let after = rmse(&lit(&run.image), &truth);
    let (before_all, after_all) = (
        rmse(&raw.beauty, &converged.beauty),
        rmse(&run.image.beauty, &converged.beauty),
    );
    assert!(
        after <= 0.6 * before,
        "lit RMSE against the converged trace: {before:.4} -> {after:.4} (needs a 40 % drop; whole image {before_all:.4} -> {after_all:.4})"
    );
    assert!(
        after_all <= before_all * 1.05,
        "the whole image must not get worse: {before_all:.4} -> {after_all:.4}"
    );
    // The preview is marked and refused as a reference.
    assert!(denoise_atrous::is_builtin_mark(&run.image.metadata.denoiser));
    assert_eq!(run.report.denoiser, "builtin");
    let dir = regia_test_utils::unique_temp_dir("pt_presets");
    let exr = dir.join("preview.exr");
    run.image.write_exr(&exr, &[Aov::Beauty]).expect("write exr");
    let raw_exr = dir.join("raw.exr");
    raw.write_exr(&raw_exr, &[Aov::Beauty]).expect("write raw");
    let loaded = regia_pt::compare::load(&exr, "beauty").expect("load");
    assert!(loaded.denoiser.is_some(), "DENOISER_KEY survives the EXR");
    let refused = regia_pt::compare::compare_files(&exr, &raw_exr, "beauty", 0.0, 60.0, None);
    assert!(
        matches!(refused, Err(regia_pt::PtError::DenoisedReference(_))),
        "{refused:?}"
    );
    // A denoised test image against a raw reference is fine.
    regia_pt::compare::compare_files(&raw_exr, &exr, "beauty", 0.0, 60.0, None).expect("preview as test");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gpu_and_cpu_filters_agree() {
    let scene = scenes::cornell_box(32, 32, true);
    let a = tracer().render(&scene, &settings(4, 4)).expect("a");
    let mut b_settings = settings(4, 4);
    b_settings.seed = 99;
    let b = tracer().render(&scene, &b_settings).expect("b");
    let variance = denoise_atrous::variance_from_halves(&a.beauty, &b.beauty).expect("variance");
    let mut image = a.clone();
    for (m, (x, y)) in image.beauty.iter_mut().zip(a.beauty.iter().zip(&b.beauty)) {
        *m = [0, 1, 2].map(|k| 0.5 * (x[k] + y[k]));
    }
    let s = AtrousSettings::default();
    let cpu = denoise_atrous::filter_cpu(&image, &variance, &s).expect("cpu");
    let gpu = denoise_atrous::filter_gpu(tracer().device(), &image, &variance, &s).expect("gpu");
    let peak = cpu
        .beauty
        .iter()
        .flatten()
        .fold(0.0f32, |m, v| m.max(*v))
        .max(1e-6);
    let worst = cpu
        .beauty
        .iter()
        .zip(&gpu.beauty)
        .flat_map(|(p, q)| (0..3).map(move |k| (p[k] - q[k]).abs() / peak))
        .fold(0.0f32, f32::max);
    assert!(
        worst < 2e-3,
        "GPU differs from the CPU twin by {worst} of the peak"
    );
    assert_eq!(cpu.metadata.denoiser, gpu.metadata.denoiser);
}

#[test]
fn a_flat_furnace_survives_the_denoiser() {
    // An unlit sphere filling the frame: every pixel exactly the same.
    let flat = PtMaterial {
        unlit: true,
        base_color: [0.5, 0.5, 0.5, 1.0],
        ..PtMaterial::default()
    };
    let scene = scenes::furnace_sphere(flat, 24);
    let mut table = PtPreset::Draft.settings();
    table.spp = 4;
    table.quality = PtQuality::default();
    table.firefly_threshold = 0.0;
    table.adaptive.enabled = false;
    let run = render_preset(tracer(), &scene, &settings(4, 2), &table).expect("draft");
    let raw = run.raw.as_ref().expect("raw");
    for (d, r) in run.image.beauty.iter().zip(&raw.beauty) {
        for k in 0..3 {
            assert!((d[k] - r[k]).abs() <= 0.01 * r[k], "{d:?} vs {r:?}");
        }
    }
    // A scattering furnace at low spp: the mean does not move by 1 %.
    let scene = scenes::furnace_sphere(PtMaterial::lambert([0.7; 3]), 24);
    let run = render_preset(tracer(), &scene, &settings(8, 4), &{
        let mut t = table;
        t.spp = 8;
        t.max_bounces = 4;
        t
    })
    .expect("furnace preview");
    let raw = run.raw.as_ref().expect("raw");
    let (m_raw, m_den) = (mean_lum(&raw.beauty), mean_lum(&run.image.beauty));
    assert!(
        (m_den / m_raw - 1.0).abs() < 0.01,
        "mean {m_raw:.4} -> {m_den:.4}"
    );
}

/// A mirror ball over a diffuse floor under a uniform sky: floor light that came by the mirror is
/// the "diffuse bounce, then a specular vertex, then the source" path of the caustics modes. (The
/// tracer has thin transmission only, so the specular object is a polished metal ball.)
fn mirror_over_floor(mirror: bool) -> PtScene {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 6.0, 7.0],
        target: [0.0, 0.3, 0.0],
        fov_y: 0.7,
        width: 32,
        height: 32,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let floor = scene.add_mesh(scenes::quad([
        [-30.0, 0.0, -30.0],
        [-30.0, 0.0, 30.0],
        [30.0, 0.0, 30.0],
        [30.0, 0.0, -30.0],
    ]));
    let fm = scene.add_material(PtMaterial::diffuse([0.6; 3]));
    scene.add_instance(floor, fm, Mat4::IDENTITY);
    let ball = scene.add_mesh(scenes::uv_sphere(1.0, 48, 24, false));
    let bm = scene.add_material(if mirror {
        PtMaterial {
            base_color: [1.0; 4],
            metallic: 1.0,
            roughness: 0.02,
            ..PtMaterial::default()
        }
    } else {
        PtMaterial::diffuse([0.9; 3])
    });
    scene.add_instance(ball, bm, Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0)));
    scene.sky = PtSky::Uniform([1.0; 3]);
    scene.sun = Some(PtSun {
        toward: [0.3, 0.8, 0.2],
        color: [1.0; 3],
        illuminance: 3.0,
        angular_radius: 0.08,
    });
    scene
}

fn trace_with(scene: &PtScene, quality: PtQuality, spp: u32) -> PtImage {
    let mut s = tracer()
        .begin_with_quality(scene, &settings(spp, 5), &quality)
        .expect("session");
    s.run(spp).expect("run");
    s.image().expect("image")
}

#[test]
fn suppress_removes_the_specular_chain_light_and_spares_diffuse_scenes() {
    let scene = mirror_over_floor(true);
    let suppress = PtQuality {
        caustics: CausticsMode::Suppress,
        ..PtQuality::default()
    };
    let full = trace_with(&scene, PtQuality::default(), 96);
    let cut = trace_with(&scene, suppress, 96);
    let (mf, mc) = (mean_lum(&full.beauty), mean_lum(&cut.beauty));
    assert!(
        mc < mf,
        "suppress must remove energy: full {mf:.5} suppress {mc:.5}"
    );
    assert!(
        mf - mc > 0.002 * mf,
        "the cut is visible: full {mf:.5} suppress {mc:.5}"
    );
    // The indirect planes lose it, the direct plane does not change at all.
    assert_eq!(
        full.direct
            .iter()
            .flatten()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        cut.direct
            .iter()
            .flatten()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        "first-vertex light is untouched"
    );
    // Clamped sits between: a generous cap is Full, a tiny cap approaches Suppress.
    let clamped = |cap: f32| {
        let q = PtQuality {
            caustics: CausticsMode::Clamped,
            clamp_caustics: cap,
            ..PtQuality::default()
        };
        mean_lum(&trace_with(&scene, q, 96).beauty)
    };
    let (loose, tight) = (clamped(1.0e9), clamped(1.0e-3));
    assert!(
        ((loose - mf) / mf).abs() < 1e-6,
        "a huge cap changes nothing: {loose} vs {mf}"
    );
    assert!(
        tight < mf && tight >= mc - 1e-9,
        "tight cap {tight:.5} between suppress {mc:.5} and full {mf:.5}"
    );

    // A diffuse-only scene has no such path: identical bits in every mode.
    let diffuse = mirror_over_floor(false);
    let a = trace_with(&diffuse, PtQuality::default(), 48);
    let b = trace_with(&diffuse, suppress, 48);
    assert_eq!(all_planes_bits(&a), all_planes_bits(&b));
}

#[test]
fn the_indirect_clamp_removes_energy_and_off_changes_nothing() {
    let scene = scenes::cornell_box(24, 24, true);
    let base = trace_with(&scene, PtQuality::default(), 32);
    let clamped = trace_with(
        &scene,
        PtQuality {
            clamp_indirect: 0.5,
            ..PtQuality::default()
        },
        32,
    );
    assert!(mean_lum(&clamped.beauty) < mean_lum(&base.beauty));
    // Direct light at the first vertex is not "indirect": its plane is unchanged.
    assert_eq!(
        base.emissive
            .iter()
            .flatten()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        clamped
            .emissive
            .iter()
            .flatten()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>()
    );
}

/// A sky band over a noisy floor: the sky rows have no variance and stop first.
#[test]
fn adaptive_sampling_stops_the_flat_sky_before_the_noisy_floor() {
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 1.5, 6.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.2,
        width: 32,
        height: 32,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let floor = scene.add_mesh(scenes::quad([
        [-40.0, 0.0, -40.0],
        [-40.0, 0.0, 40.0],
        [40.0, 0.0, 40.0],
        [40.0, 0.0, -40.0],
    ]));
    let fm = scene.add_material(PtMaterial::diffuse([0.7; 3]));
    scene.add_instance(floor, fm, Mat4::IDENTITY);
    let ball = scene.add_mesh(scenes::uv_sphere(1.0, 32, 16, false));
    let bm = scene.add_material(PtMaterial::diffuse([0.9, 0.3, 0.3]));
    scene.add_instance(ball, bm, Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0)));
    scene.sky = PtSky::Uniform([2.0; 3]);
    let mut table = PtPreset::Final.settings();
    table.spp = 96;
    table.max_bounces = 3;
    table.adaptive = AdaptiveSettings {
        enabled: true,
        tile_rows: 8,
        threshold: 0.02,
        min_spp: 8,
        round_spp: 8,
    };
    let run = render_preset(tracer(), &scene, &settings(96, 3), &table).expect("adaptive");
    let tiles = &run.report.tiles;
    assert_eq!(tiles.len(), 4);
    let sky = tiles[0];
    let floor = tiles[3];
    assert!(
        sky.spp < floor.spp,
        "sky band took {} spp, floor band {} (tiles {tiles:?})",
        sky.spp,
        floor.spp
    );
    assert!(sky.spp <= 16.0, "the sky stops at its minimum: {}", sky.spp);
    assert!(run.report.pixel_samples < run.report.pixel_samples_uniform);
    // The AOV carries it: the spp plane differs between the top and the bottom row.
    let plane = run.image.rgb(Aov::Spp);
    assert!(
        plane[0][0] < plane[31 * 32][0],
        "{} vs {}",
        plane[0][0],
        plane[31 * 32][0]
    );
    // An exact-uniform trace reports none.
    let uniform = tracer().render(&scene, &settings(4, 3)).expect("uniform");
    assert!(uniform.spp_map.is_empty());
}

#[test]
fn hostile_settings_are_errors_not_panics() {
    let scene = scenes::cornell_box(8, 8, false);
    let base = settings(2, 2);
    let mut t = PtPreset::Draft.settings();
    for bad_spp in [0, u32::MAX, regia_pt::MAX_SPP + 1] {
        t.spp = bad_spp;
        assert!(
            render_preset(tracer(), &scene, &base, &t).is_err(),
            "spp {bad_spp}"
        );
    }
    t = PtPreset::Draft.settings();
    for v in [f32::NAN, f32::INFINITY, -1.0] {
        t.quality.clamp_indirect = v;
        assert!(render_preset(tracer(), &scene, &base, &t).is_err());
        let q = PtQuality {
            clamp_indirect: v,
            ..PtQuality::default()
        };
        assert!(tracer().begin_with_quality(&scene, &base, &q).is_err());
    }
    t = PtPreset::Draft.settings();
    t.firefly_threshold = f32::NAN;
    assert!(render_preset(tracer(), &scene, &base, &t).is_err());
    t = PtPreset::Draft.settings();
    t.adaptive.threshold = f32::NAN;
    assert!(render_preset(tracer(), &scene, &base, &t).is_err());
    t = PtPreset::Draft.settings();
    t.max_bounces = 0;
    assert!(render_preset(tracer(), &scene, &base, &t).is_err());
    t = PtPreset::Draft.settings();
    t.denoiser = DenoiserKind::Oidn;
    if !cfg!(feature = "denoise") {
        assert!(
            render_preset(tracer(), &scene, &base, &t).is_err(),
            "no OIDN in this build"
        );
    }
}

#[test]
fn every_named_preset_renders_a_finite_image() {
    let scene = scenes::cornell_box(16, 16, true);
    for p in [PtPreset::Draft, PtPreset::Preview] {
        let mut t = p.settings();
        t.spp = t.spp.min(8);
        t.adaptive.min_spp = t.adaptive.min_spp.min(t.spp);
        let run = render_preset(tracer(), &scene, &settings(8, 4), &t)
            .unwrap_or_else(|e| panic!("{}: {e}", p.name()));
        assert!(
            run.image
                .beauty
                .iter()
                .flatten()
                .all(|v| v.is_finite() && *v >= 0.0)
        );
        assert_eq!(run.report.preset, p.name());
        assert!(!run.report.base);
    }
}

/// One submission per sample batch, including when the live bands leave a gap, and no device
/// idle between batches. The readback is what waits.
#[test]
fn a_gapped_batch_is_one_submit() {
    let scene = scenes::cornell_box(64, 8, false);
    let mut base = settings(6, 2);
    base.samples_per_submit = 2;
    let mut session = tracer().begin(&scene, &base).expect("session");
    // 64×8 pixels, 64-wide groups: 8 groups. Two live bands with a gap, two groups each.
    let ranges = [(0u32, 2u32), (6, 2)];
    session.run_ranges(6, &ranges, |_| true).expect("ranges");
    let batches = 6u64.div_ceil(u64::from(base.samples_per_submit));
    assert_eq!(
        session.submit_count(),
        batches,
        "submits {} for {batches} batches over {} ranges (one submit per range would be {})",
        session.submit_count(),
        ranges.len(),
        batches * ranges.len() as u64
    );
    assert_eq!(
        session.device_polls(),
        0,
        "the batch loop polled the device {} times",
        session.device_polls()
    );
    let image = session.image().expect("readback");
    assert!(image.beauty.iter().flatten().all(|v| v.is_finite()));
    assert_eq!(session.device_polls(), 1, "the readback waits once");
}

/// Draft's adaptive loop reads beauty and variance once per round after `min_spp`, and the
/// frame it returns is still a full decode.
#[test]
fn draft_stopping_reads_only_the_stopping_aovs() {
    let scene = scenes::cornell_box(32, 16, false);
    let mut base = settings(4, 2);
    base.samples_per_submit = 1;
    let mut table = PtPreset::Draft.settings();
    table.adaptive.tile_rows = 8;
    // Above zero (the validator rejects 0) and small enough that a 2 spp band does not have to
    // converge for the loop to keep running. Bands may still stop; the byte check does not
    // depend on how many rounds that takes.
    table.adaptive.threshold = 1.0e-6;
    let trace = table.trace_settings(&base);
    let mut session = tracer()
        .begin_with_quality(&scene, &trace, &table.quality)
        .expect("session");
    let run = regia_pt::run_preset(&mut session, &table, &mut |_| true).expect("draft");
    let pixels = u64::from(scene.camera.width) * u64::from(scene.camera.height);
    let stop_bytes = pixels * 2 * 16;
    let full_bytes = pixels * 11 * 16;
    assert!(
        session.stopping_readbacks() >= 1,
        "the adaptive loop did not read the stopping AOVs ({})",
        session.stopping_readbacks()
    );
    assert_eq!(
        session.full_readbacks(),
        1,
        "full readbacks {} (the loop should not copy the whole accum)",
        session.full_readbacks()
    );
    assert_eq!(
        session.readback_bytes(),
        session.stopping_readbacks() * stop_bytes + full_bytes,
        "readback bytes {} (stopping {} × {stop_bytes}, one full accum {full_bytes})",
        session.readback_bytes(),
        session.stopping_readbacks()
    );
    assert_eq!(
        session.readback_staging_allocs(),
        2,
        "staging allocations {} (one stopping buffer and one full-frame buffer)",
        session.readback_staging_allocs()
    );
    let raw = run.raw.expect("the built-in denoiser keeps the raw frame");
    assert_eq!(raw.albedo.len(), raw.beauty.len());
    assert_eq!(raw.normal.len(), raw.beauty.len());
    assert_eq!(raw.depth.len(), raw.beauty.len());
    assert_eq!(raw.variance.len(), raw.beauty.len());
    assert!(raw.beauty.iter().flatten().all(|v| v.is_finite()));
    assert_eq!(run.image.beauty.len(), raw.beauty.len());
}
