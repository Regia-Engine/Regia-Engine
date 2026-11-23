// SPDX-License-Identifier: MIT-0
//! Emitters lighting their surroundings: Regia Engine (with and without the fitted emissive area
//! lights) against the path tracer on one of DO27's Cycles hero references (lane
//! emissive-local).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> emissive_bounce --ref <asset>_ref.json --out <dir>
//!     [--cam side] [--pt 512] [--frames 32] [--tex 4096] [--units 930] [--lights 96]
//!     [--perf <frames>] [--size 2560x1440] [--night <factor>] [--synthetic]
//! ```
//!
//! `--perf` measures the cost instead: Desktop High as shipped (its render scale and TAAU) at
//! `--size`, one renderer, frames with the fitted lights on, off and on again; prints the GPU
//! pass medians (forward, cluster build, SSGI) of each phase and the pipelines created after the
//! first phase's warm-up (must be 0).
//!
//! Renders, at the reference's size and fixed exposure: Regia Engine with emission and the fitted
//! lights (`after`), Regia Engine with emission and no fitted lights (`before`, the GI alone),
//! Regia Engine without emission, and the path tracer with and without emission. Each image is in
//! ground units (the sunlit ground's luminance). The "emissive part" of a renderer is its
//! emission-on minus emission-off luminance; the path tracer's is the yardstick (it reads
//! 0.94–0.97 of Cycles in the shaded rings, look-r2 report).
//!
//! Regions: pixels the sun does not reach (path tracer sun visibility < 0.05), split into the
//! ground plane (up-facing, albedo 0.18) and the asset (the hull), in rings 0–8, 8–24, 24–60
//! and 60–150 px from the emitters (the path tracer's directly seen emission). Writes
//! `<asset>_<cam>_emissive_bounce.md` (medians) and `..._sheet.png` (beauty row: path tracer,
//! before, after; emissive-part row in the same order, ×4).

use std::path::Path;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::hero;
use lookdev::hero_ref::{CyclesRef, render_studio_with, standard_view, trace_studio_image};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("emissive_bounce: {e}");
            ExitCode::FAILURE
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Chamfer (3-4) distance in pixels from the set pixels of `mask`.
fn distance_px(mask: &[bool], w: usize, h: usize) -> Vec<f32> {
    let big = f32::MAX / 4.0;
    let mut d: Vec<f32> = mask.iter().map(|&m| if m { 0.0 } else { big }).collect();
    let (a, b) = (1.0f32, std::f32::consts::SQRT_2);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let mut v = d[i];
            if x > 0 {
                v = v.min(d[i - 1] + a);
            }
            if y > 0 {
                v = v.min(d[i - w] + a);
                if x > 0 {
                    v = v.min(d[i - w - 1] + b);
                }
                if x + 1 < w {
                    v = v.min(d[i - w + 1] + b);
                }
            }
            d[i] = v;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            let mut v = d[i];
            if x + 1 < w {
                v = v.min(d[i + 1] + a);
            }
            if y + 1 < h {
                v = v.min(d[i + w] + a);
                if x + 1 < w {
                    v = v.min(d[i + w + 1] + b);
                }
                if x > 0 {
                    v = v.min(d[i + w - 1] + b);
                }
            }
            d[i] = v;
        }
    }
    d
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

/// A lone emissive bar over the studio ground (see `--synthetic`); `scale` is unused (the setup
/// applies the emission scale to every material).
fn synthetic_model(_scale: f32) -> hero::HeroModel {
    use regia_core::glam::{Mat4, Vec3};
    let (vertices, indices) = regia_render::box_mesh([0.6, 0.02, 0.02]);
    let material = regia_render::LabMaterial {
        base_color: [0.2, 0.2, 0.2, 1.0],
        emissive: [0.15, 1.0, 0.3],
        emissive_intensity: 8000.0,
        double_sided: true,
        ..regia_render::LabMaterial::EMPTY
    };
    let mesh = regia_render::LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(Vec3::new(0.0, 0.6, 0.0)).to_cols_array_2d(),
        joints: Vec::new(),
    };
    hero::HeroModel {
        meshes: vec![mesh],
        materials: vec![material],
        min: Vec3::new(-0.6, 0.58, -0.02),
        max: Vec3::new(0.6, 0.62, 0.02),
    }
}

/// Medians of the GPU passes over `frames` frames of `scene` with the emissive budget `budget`
/// (rendered into a texture: no readback in the loop).
fn phase(
    device: &regia_render::RenderDevice,
    renderer: &mut regia_render::FrameRenderer,
    scene: &lookdev::hero_ref::StudioScene,
    budget: u32,
    frames: u32,
) -> Result<[f32; 6], LookdevError> {
    let shipped = regia_render::RenderSettings::default().quality();
    let mut quality = scene.quality;
    quality.emissive_lights = budget;
    quality.render_scale_permille = shipped.render_scale_permille;
    let frame = scene.lab.as_frame_scene_with_quality(quality);
    let (mut fwd, mut clu, mut gi, mut total) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut probes, mut blend) = (Vec::new(), Vec::new());
    for _ in 0..frames {
        renderer.render_into_texture(device, 1, &frame)?;
        let t = renderer.gpu_timings();
        fwd.push(t.forward_ms);
        clu.push(t.clusters_ms);
        gi.push(t.ssgi_ms);
        total.push(t.scene_ms);
        probes.push(t.gi_ms);
        blend.push(t.gi_stages_ms[2]);
    }
    let m = |v: Vec<f32>| median(v).unwrap_or(0.0);
    Ok([m(fwd), m(clu), m(gi), m(total), m(probes), m(blend)])
}

fn perf(
    device: &regia_render::RenderDevice,
    model: &hero::HeroModel,
    setup: &lookdev::hero_ref::StudioSetup,
    lights: u32,
    frames: u32,
    asset: &str,
    cam: &str,
) -> Result<(), LookdevError> {
    // One scene for every frame: stable mesh keys, so uploads, fits and probes happen once.
    let scene = lookdev::hero_ref::studio_scene(model, setup)?;
    let mut renderer = regia_render::FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.set_studio_sky(device, scene.studio);
    // Warm-up: pipelines, uploads, the fit cache, the probes and TAAU history (and the GPU's
    // clocks: a cold card reads every pass slow).
    phase(device, &mut renderer, &scene, lights, 120)?;
    let created = regia_render::pipeline_cache::pipelines_created();
    let on = phase(device, &mut renderer, &scene, lights, frames)?;
    let report = renderer.emissive_lights();
    let off = phase(device, &mut renderer, &scene, 0, frames)?;
    let on2 = phase(device, &mut renderer, &scene, lights, frames)?;
    let after_warm = regia_render::pipeline_cache::pipelines_created() - created;
    println!(
        "emissive_bounce perf {asset} {cam} {}x{} Desktop High: {} lights uploaded ({} fitted, {} refits, {:.3} ms CPU)",
        setup.width, setup.height, report.uploaded, report.fitted, report.refits, report.cpu_ms
    );
    println!("phase   forward  clusters  ssgi   scene (GPU ms, medians of {frames})");
    for (name, t) in [("on", on), ("off", off), ("on", on2)] {
        println!(
            "{name:<6} {:>8.3} {:>8.3} {:>6.3} {:>7.3}",
            t[0], t[1], t[2], t[3]
        );
    }
    let delta = |k: usize| 0.5 * (on[k] + on2[k]) - off[k];
    println!(
        "cost (on − off): forward {:+.3} ms, clusters {:+.3} ms, ssgi {:+.3} ms, scene {:+.3} ms; pipelines created after warm-up: {after_warm}",
        delta(0),
        delta(1),
        delta(2),
        delta(3)
    );
    // `EMISSIVE_BOUNCE_PROBE_EMITTERS=1` (with `HERO_GI=depth`): the GPU irradiance probes'
    // emitter list (`DdgiSettings::emitters`) on, off and on again, the fitted lights off,
    // with the probe update's own timers (the list is added in the irradiance blend).
    if std::env::var_os("EMISSIVE_BOUNCE_PROBE_EMITTERS").is_some() {
        let base = renderer.ddgi_settings().clone();
        let mut timed = |emitters: bool| -> Result<[f32; 6], LookdevError> {
            renderer.set_ddgi_settings(regia_render::DdgiSettings {
                emitters,
                ..base.clone()
            });
            phase(device, &mut renderer, &scene, 0, 60)?;
            phase(device, &mut renderer, &scene, 0, frames)
        };
        let on = timed(true)?;
        let off = timed(false)?;
        let on2 = timed(true)?;
        println!("probe emitter list   gi probes  blend irr  scene (GPU ms, medians of {frames})");
        for (name, t) in [("on", on), ("off", off), ("on", on2)] {
            println!("{name:<20} {:>9.3} {:>10.3} {:>6.3}", t[4], t[5], t[3]);
        }
        let delta = |k: usize| 0.5 * (on[k] + on2[k]) - off[k];
        println!(
            "probe emitter list cost (on − off): gi probes {:+.3} ms, blend {:+.3} ms, scene {:+.3} ms",
            delta(4),
            delta(5),
            delta(3)
        );
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let out = std::path::PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(io_err(&out))?;
    let ref_path = std::path::PathBuf::from(
        value(args, "--ref").ok_or_else(|| LookdevError::Config("--ref <json> is required".into()))?,
    );
    let content = std::path::PathBuf::from(
        std::env::var("REGIA_DO27_CONTENT")
            .map_err(|_| LookdevError::Config("REGIA_DO27_CONTENT is not set".into()))?,
    );
    let text = std::fs::read_to_string(&ref_path).map_err(io_err(&ref_path))?;
    let mut r = CyclesRef::parse(&text)?;
    if let Some(u) = value(args, "--units").and_then(|v| v.parse().ok()) {
        r.set_units(u);
    }
    let emissive_scale = r.emission_factor.map_or(r.units, |f| f * r.units);
    let (files, missing) = r.resolve_files(&content);
    for m in &missing {
        eprintln!(
            "emissive_bounce {}: {m} is not in the content drop (not drawn)",
            r.asset
        );
    }
    let max_tex: u32 = value(args, "--tex").and_then(|v| v.parse().ok()).unwrap_or(4096);
    // `--synthetic`: a lone emissive bar (closed box 1.2 × 0.04 × 0.04 m, 0.6 m up, the
    // reference's emitter material) over the ground: nothing occludes it, so forward and path
    // tracer must agree on the magnitude and the falloff.
    let synthetic = args.iter().any(|a| a == "--synthetic");
    let model = if synthetic {
        synthetic_model(emissive_scale)
    } else {
        let mut model = hero::load_files(&r.asset, &files, max_tex, false)?;
        let c = (model.min + model.max) * 0.5;
        let offset = regia_core::glam::Vec3::new(-c.x, -model.min.y, -c.z);
        hero::place(&mut model, 0.0, offset);
        model
    };
    let cam = value(args, "--cam").unwrap_or("side");
    let (_, eye, target, fov_y) = r
        .cameras
        .iter()
        .find(|(name, ..)| name == cam)
        .cloned()
        .ok_or_else(|| LookdevError::Config(format!("camera {cam} is not in {}", ref_path.display())))?;
    let mut setup = r.setup(eye, target, fov_y);
    if let Some(f) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        setup.frames = f;
    }
    // `--night <f>`: the sun and the sky at `f` times the reference (a dusk-like studio where the
    // emitters dominate); results stay in the reference's ground units.
    if let Some(f) = value(args, "--night").and_then(|v| v.parse::<f32>().ok()) {
        setup.sun_lux *= f;
        if let lookdev::hero_ref::StudioSky::Uniform { radiance } = &mut setup.sky {
            *radiance = radiance.map(|r| r * f);
        }
    }
    let lights: u32 = value(args, "--lights").and_then(|v| v.parse().ok()).unwrap_or(96);
    let spp: u32 = value(args, "--pt").and_then(|v| v.parse().ok()).unwrap_or(512);
    let device = regia_terrain::gpu::headless_bcn_device()?;
    if let Some(frames) = value(args, "--perf").and_then(|v| v.parse::<u32>().ok()) {
        if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
            setup.width = w.parse().unwrap_or(2560);
            setup.height = h.parse().unwrap_or(1440);
        } else {
            (setup.width, setup.height) = (2560, 1440);
        }
        setup.emissive_scale = emissive_scale;
        setup.frames = 1;
        return perf(&device, &model, &setup, lights, frames.max(10), &r.asset, cam);
    }

    let forward = |emission: f32,
                   budget: u32|
     -> Result<
        (
            Vec<[f32; 3]>,
            f32,
            regia_render::emissive_lights::EmissiveLightReport,
        ),
        LookdevError,
    > {
        let mut s = setup.clone();
        s.emissive_scale = emission;
        // A fresh renderer per image: no temporal history carries one image into the next.
        let mut renderer = regia_render::FrameRenderer::new();
        let f = render_studio_with(&device, &mut renderer, &model, &s, |q| q.emissive_lights = budget)?;
        Ok((f.hdr.rgb, f.ground_luminance, renderer.emissive_lights()))
    };
    let (after, unit, report) = forward(emissive_scale, lights)?;
    let (before, _, _) = forward(emissive_scale, 0)?;
    let (off, _, _) = forward(0.0, lights)?;
    println!(
        "emissive_bounce {}: fitted {} lights on {} meshes, {} in view, {} uploaded (budget {}), {:.3} ms CPU",
        r.asset, report.fitted, report.meshes, report.in_view, report.uploaded, report.cap, report.cpu_ms
    );
    // Option (b), a screen-space emissive gather, can only gather emitters the camera sees: the
    // share of the fitted flux on faces turned toward the camera (an upper bound: occlusion
    // ignored).
    let (mut seen, mut all) = (0.0f32, 0.0f32);
    {
        use regia_core::glam::{Mat4, Vec3};
        use regia_render::emissive_lights::{EmitterShape, FitParams, fit_mesh};
        let eye_v = Vec3::from_array(eye);
        for mesh in &model.meshes {
            let Some(material) = model.materials.get(mesh.material) else {
                continue;
            };
            let m = Mat4::from_cols_array_2d(&mesh.model);
            for l in fit_mesh(&mesh.vertices, &mesh.indices, material, &FitParams::default()) {
                let l = l.transformed(&m);
                let flux = l.flux();
                all += flux;
                let c = Vec3::from_array(l.centre);
                let to_eye = (eye_v - c).normalize_or_zero();
                let ax = Vec3::from_array(l.axis_x);
                let facing = match l.shape {
                    EmitterShape::Quad { two_sided: true, .. } => ax
                        .cross(Vec3::from_array(l.axis_y))
                        .normalize_or_zero()
                        .dot(to_eye)
                        .abs(),
                    EmitterShape::Quad { .. } => ax
                        .cross(Vec3::from_array(l.axis_y))
                        .normalize_or_zero()
                        .dot(to_eye)
                        .max(0.0),
                    // A tube's silhouette shrinks toward its axis.
                    EmitterShape::Tube { .. } => {
                        (1.0 - ax.normalize_or_zero().dot(to_eye).powi(2)).max(0.0).sqrt()
                    }
                };
                seen += flux * facing;
            }
        }
    }
    println!(
        "emissive_bounce {}: projected share of the emitters' flux toward the camera {:.0} % (screen-space upper bound)",
        r.asset,
        100.0 * seen / all.max(1e-9)
    );
    let tracer = regia_pt::PathTracer::new(&device).map_err(|e| LookdevError::Config(e.to_string()))?;
    let mut pt_setup = setup.clone();
    pt_setup.emissive_scale = emissive_scale;
    let pt_on = trace_studio_image(&tracer, &model, &pt_setup, spp, 0.0046)?;
    pt_setup.emissive_scale = 0.0;
    let pt_off = trace_studio_image(&tracer, &model, &pt_setup, spp, 0.0046)?;
    let (w, h) = (setup.width as usize, setup.height as usize);
    if pt_on.beauty.len() != w * h || after.len() != w * h {
        return Err(LookdevError::Config(format!(
            "image sizes differ: path tracer {} px, forward {} px, expected {w}x{h}",
            pt_on.beauty.len(),
            after.len()
        )));
    }
    let gu = 1.0 / unit.max(1e-9);
    // Emitters: emission the path tracer's camera sees directly (≥ 1 ground unit, about 5 % of
    // a strip pixel's).
    let strip: Vec<bool> = pt_on.emissive.iter().map(|&e| lum(e) * gu >= 1.0).collect();
    let dist = distance_px(&strip, w, h);
    let rings: [(f32, f32, &str); 4] = [
        (0.0, 8.0, "0–8 px"),
        (8.0, 24.0, "8–24 px"),
        (24.0, 60.0, "24–60 px"),
        (60.0, 150.0, "60–150 px"),
    ];
    let mut table = String::new();
    table.push_str(&format!(
        "# {} {cam}: emitters lighting their surroundings\n\nPath tracer {spp} spp; Regia Engine Desktop High (Balanced GI), budget {lights} lights: {} fitted on {} meshes, {} uploaded, {:.3} ms CPU. Emitter flux projected toward the camera (what a screen-space gather could see, occlusion ignored): {:.0} %. Emitter pixels: {}.\nValues: median luminance in ground units over sun-shadowed pixels in each ring. Emissive part = emission on − off.\n\n",
        r.asset,
        report.fitted,
        report.meshes,
        report.uploaded,
        report.cpu_ms,
        100.0 * seen / all.max(1e-9),
        strip.iter().filter(|&&s| s).count()
    ));
    table.push_str("| Region | Ring | Pixels | PT total | Before total | After total | PT emissive part | Before part | After part | After / PT | PT part mean | Before mean | After mean | After / PT (mean) |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (region, is_region) in [
        (
            "ground",
            Box::new(|i: usize| {
                pt_on.normal[i][1] > 0.99
                    && (pt_on.albedo[i][0] - 0.18).abs() < 0.01
                    && (pt_on.albedo[i][2] - 0.18).abs() < 0.01
            }) as Box<dyn Fn(usize) -> bool>,
        ),
        (
            "hull",
            Box::new(|i: usize| {
                !(pt_on.normal[i][1] > 0.99
                    && (pt_on.albedo[i][0] - 0.18).abs() < 0.01
                    && (pt_on.albedo[i][2] - 0.18).abs() < 0.01)
            }),
        ),
    ] {
        for &(lo, hi, name) in &rings {
            let px: Vec<usize> = (0..w * h)
                .filter(|&i| !strip[i] && dist[i] > lo && dist[i] <= hi.max(lo + 0.5))
                .filter(|&i| {
                    (synthetic || pt_on.shadow[i] < 0.05) && lum(pt_on.sky[i]) <= 0.0 && pt_on.depth[i] > 0.0
                })
                .filter(|&i| is_region(i))
                .collect();
            let m = |img: &dyn Fn(usize) -> f32| median(px.iter().map(|&i| img(i)).collect());
            let pt_total = m(&|i| lum(pt_on.beauty[i]) * gu);
            let before_total = m(&|i| lum(before[i]) * gu);
            let after_total = m(&|i| lum(after[i]) * gu);
            let pt_part = m(&|i| (lum(pt_on.beauty[i]) - lum(pt_off.beauty[i])) * gu);
            let before_part = m(&|i| (lum(before[i]) - lum(off[i])) * gu);
            let after_part = m(&|i| (lum(after[i]) - lum(off[i])) * gu);
            let mean = |img: &dyn Fn(usize) -> f32| {
                (!px.is_empty()).then(|| px.iter().map(|&i| img(i)).sum::<f32>() / px.len() as f32)
            };
            let pt_mean = mean(&|i| (lum(pt_on.beauty[i]) - lum(pt_off.beauty[i])) * gu);
            let before_mean = mean(&|i| (lum(before[i]) - lum(off[i])) * gu);
            let after_mean = mean(&|i| (lum(after[i]) - lum(off[i])) * gu);
            let ratio_mean = match (after_mean, pt_mean) {
                (Some(a), Some(p)) if p.abs() > 1e-4 => format!("{:.0} %", 100.0 * a / p),
                _ => "–".into(),
            };
            let f = |v: Option<f32>| v.map_or("–".to_string(), |v| format!("{v:.3}"));
            let ratio = match (after_part, pt_part) {
                (Some(a), Some(p)) if p.abs() > 1e-4 => format!("{:.0} %", 100.0 * a / p),
                _ => "–".into(),
            };
            table.push_str(&format!(
                "| {region} | {name} | {} | {} | {} | {} | {} | {} | {} | {ratio} | {} | {} | {} | {ratio_mean} |\n",
                px.len(),
                f(pt_total),
                f(before_total),
                f(after_total),
                f(pt_part),
                f(before_part),
                f(after_part),
                f(pt_mean),
                f(before_mean),
                f(after_mean)
            ));
        }
    }
    print!("{table}");
    let stem = format!("{}_{cam}_emissive_bounce", r.asset);
    let md = out.join(format!("{stem}.md"));
    std::fs::write(&md, &table).map_err(io_err(&md))?;
    for (name, img) in [
        ("pt_on", &pt_on.beauty),
        ("pt_off", &pt_off.beauty),
        ("before", &before),
        ("after", &after),
        ("off", &off),
    ] {
        lookdev::pt_plates::write_linear_exr(
            &out.join(format!("{stem}_{name}.exr")),
            w as u32,
            h as u32,
            &[("full", img)],
        )?;
    }
    // Sheet: beauty row (path tracer, before, after), emissive-part row ×4, half size.
    let scale = r.png_scale();
    let part = |on: &[[f32; 3]], off: &[[f32; 3]]| -> Vec<[f32; 3]> {
        on.iter()
            .zip(off)
            .map(|(a, b)| {
                [
                    (a[0] - b[0]).max(0.0) * 4.0,
                    (a[1] - b[1]).max(0.0) * 4.0,
                    (a[2] - b[2]).max(0.0) * 4.0,
                ]
            })
            .collect()
    };
    let tiles = [
        pt_on.beauty.clone(),
        before.clone(),
        after.clone(),
        part(&pt_on.beauty, &pt_off.beauty),
        part(&before, &off),
        part(&after, &off),
    ];
    let (tw, th) = (w / 2, h / 2);
    let (sw, sh) = (tw * 3, th * 2);
    let mut sheet = vec![0u8; sw * sh * 4];
    for (k, tile) in tiles.iter().enumerate() {
        let rgba = standard_view(tile, scale);
        let (ox, oy) = ((k % 3) * tw, (k / 3) * th);
        for y in 0..th {
            for x in 0..tw {
                let s = ((y * 2) * w + x * 2) * 4;
                let d = ((oy + y) * sw + ox + x) * 4;
                sheet[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
    }
    lookdev::default_look::write_png(
        &out.join(format!("{stem}_sheet.png")),
        sw as u32,
        sh as u32,
        &sheet,
    )?;
    println!("emissive_bounce: wrote {}", md.display());
    Ok(())
}
