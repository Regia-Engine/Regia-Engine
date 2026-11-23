// SPDX-License-Identifier: MIT-0
//! `hero_ref`: a DO27 hero asset in a reference studio (sun, sky, 0.18 grey ground, camera),
//! written as linear EXR and a Standard-view PNG for comparison with DO27's Cycles references
//! (see [`lookdev::hero_ref`]).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> hero_ref --out <dir> --asset <name>
//!     [--sun <elevation°>,<azimuth°>,<lux>] [--sky flat:<lux>|atmosphere] [--ground 0.18]
//!     [--cam <ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<vertical fov°>] [--size 1920x1080] [--frames 14]
//!     [--tex 4096]
//! ```
//!
//! Writes `<dir>/<asset>.exr` (layer `full`, cd/m²), `<dir>/<asset>_standard.png` (sunlit
//! ground at linear 0.5) and `<dir>/<asset>.json` (the setup and the ground luminance).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> hero_ref --out <dir> --ref <asset>_ref.json
//!     [--units 930] [--emissive ref|cycles|engine|<factor>] [--ids] [--frames 32] [--tex 4096]
//!     [--cams front34,side] [--as-authored] [--focal <mm>] [--pt <spp>] [--ev <EV100>]
//!     [--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>]...
//! ```
//!
//! `--ref` reproduces one of DO27's Cycles hero references from its setup JSON (see
//! [`lookdev::hero_ref::CyclesRef`]): the glTF files it lists (found by name under the content
//! drop, unmirrored, stood in the studio as the references stand it: horizontal bounds centre
//! at the origin, lowest point on the ground), its sun, flat world, ground, resolution and every
//! camera. Per camera it
//! writes `<asset>_<camera>.exr` (cd/m²) and `<asset>_<camera>.png` in the reference PNG's
//! own mapping (Standard view, the reference's display exposure, Blender units = cd/m² ÷
//! `--units`); `--emissive ref` (default) converts emission as the reference did (its
//! `emission_conversion.factor` × `--units`, or × `--units` alone for the first set, which fed
//! the nits to Cycles unconverted: `cycles`), `engine` keeps the glTF strength as nits; `--ids` adds the auxiliary passes
//! `<asset>_<camera>_{ids,albedo,orm,sun}.exr` (see [`lookdev::hero_ref::AuxPass`]) and
//! `<asset>_materials.txt` naming each material index; with `--pt` as well, the sun pass is
//! also path-traced, direct light only (`<asset>_<camera>_sun_pt.exr`).

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::hero;
use lookdev::hero_ref::{
    AuxPass, CyclesRef, StudioSetup, StudioSky, aux_model, aux_setup, render_studio, standard_view,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hero_ref: {err}");
            ExitCode::from(1)
        }
    }
}

/// Waits for the GPU so wgpu releases what was dropped (it frees resources only once the
/// submissions using them are known complete) before the next renderer allocates.
fn free_dropped(device: &regia_render::RenderDevice) {
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// Angular radius of Blender's default sun disc (0.526° across), radians.
const BLENDER_SUN_RADIUS: f32 = 0.0046;

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn floats(text: &str, sep: char) -> Result<Vec<f32>, LookdevError> {
    text.split(sep)
        .map(|v| {
            v.trim()
                .parse::<f32>()
                .map_err(|_| LookdevError::Config(format!("not a number: {v}")))
        })
        .collect()
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let out = PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    if let Some(r) = value(args, "--ref") {
        return run_ref(args, &content, &out, &PathBuf::from(r));
    }
    let name =
        value(args, "--asset").ok_or_else(|| LookdevError::Config("--asset <name> is required".into()))?;
    let asset = hero::catalog(&content)
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| LookdevError::Config(format!("no hero asset {name}")))?;
    let max_tex: u32 = value(args, "--tex").and_then(|v| v.parse().ok()).unwrap_or(4096);
    let model = hero::load(&asset, max_tex)?;
    let size = model.max - model.min;
    let centre = (model.min + model.max) * 0.5;
    let mut setup = StudioSetup {
        sun_elevation_deg: 35.0,
        sun_azimuth_deg: 30.0,
        sun_lux: 100_000.0,
        sky: StudioSky::Flat { lux: 20_000.0 },
        ground_albedo: 0.18,
        // Front three-quarter at 2.2 × the asset's size.
        eye: (centre + regia_core::glam::Vec3::new(0.7, 0.35, 0.7).normalize() * size.length() * 1.1)
            .to_array(),
        target: centre.to_array(),
        fov_y: 35f32.to_radians(),
        width: 1920,
        height: 1080,
        frames: 14,
        emissive_scale: 1.0,
        taa: false,
        exposure_ev: None,
    };
    if let Some(v) = value(args, "--sun") {
        let f = floats(v, ',')?;
        if f.len() != 3 {
            return Err(LookdevError::Config("--sun <elevation>,<azimuth>,<lux>".into()));
        }
        (setup.sun_elevation_deg, setup.sun_azimuth_deg, setup.sun_lux) = (f[0], f[1], f[2]);
    }
    match value(args, "--sky") {
        Some("atmosphere") => setup.sky = StudioSky::Atmosphere,
        Some(v) if v.starts_with("flat:") => {
            let lux = v[5..]
                .parse()
                .map_err(|_| LookdevError::Config(format!("--sky {v}")))?;
            setup.sky = StudioSky::Flat { lux };
        }
        Some(v) => {
            return Err(LookdevError::Config(format!(
                "--sky {v}: flat:<lux> or atmosphere"
            )));
        }
        None => {}
    }
    if let Some(g) = value(args, "--ground").and_then(|v| v.parse().ok()) {
        setup.ground_albedo = g;
    }
    if let Some(v) = value(args, "--cam") {
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() != 3 {
            return Err(LookdevError::Config(
                "--cam <ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>".into(),
            ));
        }
        let (e, t) = (floats(parts[0], ',')?, floats(parts[1], ',')?);
        if e.len() != 3 || t.len() != 3 {
            return Err(LookdevError::Config("--cam needs three coordinates each".into()));
        }
        setup.eye = [e[0], e[1], e[2]];
        setup.target = [t[0], t[1], t[2]];
        setup.fov_y = floats(parts[2], ',')?[0].to_radians();
    }
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        setup.width = w.parse().unwrap_or(setup.width);
        setup.height = h.parse().unwrap_or(setup.height);
    }
    if let Some(f) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        setup.frames = f;
    }
    let device = regia_terrain::gpu::headless_bcn_device()?;
    let mut renderer = regia_render::FrameRenderer::new();
    let frame = render_studio(&device, &mut renderer, &model, &setup)?;
    let (w, h) = (frame.hdr.width, frame.hdr.height);
    lookdev::pt_plates::write_linear_exr(
        &out.join(format!("{name}.exr")),
        w,
        h,
        &[("full", &frame.hdr.rgb)],
    )?;
    let png = standard_view(&frame.hdr.rgb, 0.5 / frame.ground_luminance);
    lookdev::default_look::write_png(&out.join(format!("{name}_standard.png")), w, h, &png)?;
    let json = format!(
        "{{\"asset\":\"{name}\",\"sun_elevation_deg\":{},\"sun_azimuth_deg\":{},\"sun_lux\":{},\"sky\":\"{:?}\",\"ground_albedo\":{},\"eye\":{:?},\"target\":{:?},\"fov_y_deg\":{},\"ground_luminance_cd_m2\":{}}}",
        setup.sun_elevation_deg,
        setup.sun_azimuth_deg,
        setup.sun_lux,
        setup.sky,
        setup.ground_albedo,
        setup.eye,
        setup.target,
        setup.fov_y.to_degrees(),
        frame.ground_luminance
    );
    std::fs::write(out.join(format!("{name}.json")), json).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    println!(
        "hero_ref {name}: {w}x{h}, ground {:.1} cd/m²",
        frame.ground_luminance
    );
    Ok(())
}

fn io_err(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn run_ref(
    args: &[String],
    content: &std::path::Path,
    out: &std::path::Path,
    ref_path: &std::path::Path,
) -> Result<(), LookdevError> {
    let text = std::fs::read_to_string(ref_path).map_err(io_err(ref_path))?;
    let mut r = CyclesRef::parse(&text)?;
    if let Some(u) = value(args, "--units").and_then(|v| v.parse().ok()) {
        r.set_units(u);
    }
    // `--sun <elevation°>,<azimuth°>` and `--sky flat:<lux>` override the reference's light
    // (diagnostics: a grazing sun on the hull flanks, a black airless sky over a canopy). The
    // sun keeps the reference's illuminance; a flat sky's lux is in the studio's units.
    if let Some(v) = value(args, "--sun") {
        let f = floats(v, ',')?;
        if f.len() < 2 {
            return Err(LookdevError::Config("--sun <elevation>,<azimuth>".into()));
        }
        (r.sun_elevation_deg, r.sun_azimuth_deg) = (f[0], f[1]);
    }
    if let Some(v) = value(args, "--sky") {
        let lux = v
            .strip_prefix("flat:")
            .and_then(|l| l.parse().ok())
            .ok_or_else(|| LookdevError::Config(format!("--sky {v}: flat:<lux>")))?;
        r.sky = StudioSky::Flat { lux };
    }
    // glTF's emissive strength is nits to the engine. The refreshed references convert it to
    // Blender radiance (`emission_conversion.factor`, nits / 93); the first set fed the nits to
    // Cycles unconverted. `ref` (default) reproduces whichever the JSON says: the factor × the
    // studio's units, or `cycles` (× units) when the JSON has no conversion; `engine` keeps
    // the glTF strength as nits.
    let emissive_scale = match value(args, "--emissive").unwrap_or("ref") {
        "ref" => r.emission_factor.map_or(r.units, |f| f * r.units),
        "cycles" => r.units,
        "engine" => 1.0,
        v => v
            .parse()
            .map_err(|_| LookdevError::Config(format!("--emissive {v}: ref, cycles, engine or a factor")))?,
    };
    let (files, missing) = r.resolve_files(content);
    for m in &missing {
        eprintln!("hero_ref {}: {m} is not in the content drop (not drawn)", r.asset);
    }
    let max_tex: u32 = value(args, "--tex").and_then(|v| v.parse().ok()).unwrap_or(4096);
    let mut model = hero::load_files(&r.asset, &files, max_tex, false)?;
    // The references stand the asset in the studio: horizontal bounds centre at the origin,
    // lowest point on the ground (their JSON bounds and cameras are in the glTF's own frame;
    // registration against the images confirms the move to the pixel). `--as-authored` skips it.
    if !args.iter().any(|a| a == "--as-authored") {
        let c = (model.min + model.max) * 0.5;
        let offset = regia_core::glam::Vec3::new(-c.x, -model.min.y, -c.z);
        hero::place(&mut model, 0.0, offset);
    }
    let ids = args.iter().any(|a| a == "--ids");
    let passes = [
        (AuxPass::Ids, "ids"),
        (AuxPass::Albedo, "albedo"),
        (AuxPass::Orm, "orm"),
        (AuxPass::Sun, "sun"),
    ];
    let aux: Vec<_> = if ids {
        passes
            .iter()
            .map(|&(p, n)| (p, n, aux_model(&model, p)))
            .collect()
    } else {
        Vec::new()
    };
    if ids {
        // index, name, ORM texture packed (0/1), roughness factor, metallic factor
        let names: String = r
            .material_names(&files)
            .iter()
            .zip(&model.materials)
            .enumerate()
            .map(|(i, (n, m))| {
                format!(
                    "{i}\t{n}\t{}\t{}\t{}\n",
                    u8::from(m.orm_packed && m.occlusion_map.is_some()),
                    m.roughness,
                    m.metallic
                )
            })
            .collect();
        let p = out.join(format!("{}_materials.txt", r.asset));
        std::fs::write(&p, names).map_err(io_err(&p))?;
    }
    let meta = out.join(format!("{}_regia.json", r.asset));
    std::fs::write(
        &meta,
        format!("{{\"units\":{},\"emissive_scale\":{emissive_scale}}}", r.units),
    )
    .map_err(io_err(&meta))?;
    // `--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>` (repeatable) adds a camera in the
    // studio's frame (the asset stood at the origin), e.g. a close-up the reference lacks.
    for (i, a) in args.iter().enumerate() {
        if a != "--cam" {
            continue;
        }
        let v = args
            .get(i + 1)
            .ok_or_else(|| LookdevError::Config("--cam needs a value".into()))?;
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() != 4 {
            return Err(LookdevError::Config(
                "--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>".into(),
            ));
        }
        let (e, t, fov) = (
            floats(parts[1], ',')?,
            floats(parts[2], ',')?,
            floats(parts[3], ',')?,
        );
        if e.len() != 3 || t.len() != 3 || fov.len() != 1 {
            return Err(LookdevError::Config(
                "--cam needs three coordinates each and a fov".into(),
            ));
        }
        r.cameras.push((
            parts[0].to_string(),
            [e[0], e[1], e[2]],
            [t[0], t[1], t[2]],
            fov[0].to_radians(),
        ));
    }
    let only: Option<Vec<&str>> = value(args, "--cams").map(|v| v.split(',').collect());
    let device = regia_terrain::gpu::headless_bcn_device()?;
    // `--pt <spp>`: also path-trace each camera with the engine's BRDF (`<stem>_pt.exr`).
    let pt_spp: Option<u32> = value(args, "--pt").and_then(|v| v.parse().ok());
    let tracer = match pt_spp {
        Some(_) => Some(regia_pt::PathTracer::new(&device).map_err(|e| LookdevError::Config(e.to_string()))?),
        None => None,
    };
    for (cam, eye, target, fov_y) in &r.cameras {
        if only.as_ref().is_some_and(|o| !o.contains(&cam.as_str())) {
            continue;
        }
        // Diagnostic: `--focal <mm>` replaces every camera's focal length.
        let fov_y = match value(args, "--focal").and_then(|v| v.parse::<f32>().ok()) {
            Some(mm) => lookdev::hero_ref::blender_fov_y(mm, 36.0, r.width, r.height),
            None => *fov_y,
        };
        let mut setup = r.setup(*eye, *target, fov_y);
        if let Some(f) = value(args, "--frames").and_then(|v| v.parse().ok()) {
            setup.frames = f;
        }
        setup.emissive_scale = emissive_scale;
        // `--ev <EV100>` fixes the exposure (a studio without a lit ground, such as a BRDF
        // sphere sheet on a black ground, would otherwise meter on nothing and clip the
        // pre-exposed HDR target).
        if let Some(ev) = value(args, "--ev").and_then(|v| v.parse().ok()) {
            setup.exposure_ev = Some(ev);
        }
        // One renderer per camera, dropped before the auxiliary passes (each of which has its
        // own), so a camera peaks at one renderer's VRAM: two renderers' texture sets plus the
        // tracer's scene on a 16 GB card wedged the driver (Xid 62) on DO27's heavier heroes.
        let mut renderer = regia_render::FrameRenderer::new();
        let frame = render_studio(&device, &mut renderer, &model, &setup)?;
        let (w, h) = (frame.hdr.width, frame.hdr.height);
        let stem = format!("{}_{cam}", r.asset);
        lookdev::pt_plates::write_linear_exr(
            &out.join(format!("{stem}.exr")),
            w,
            h,
            &[("full", &frame.hdr.rgb)],
        )?;
        let png = standard_view(&frame.hdr.rgb, r.png_scale());
        lookdev::default_look::write_png(&out.join(format!("{stem}.png")), w, h, &png)?;
        if let (Some(spp), Some(tracer)) = (pt_spp, tracer.as_ref()) {
            let beauty = lookdev::hero_ref::trace_studio(tracer, &model, &setup, spp, BLENDER_SUN_RADIUS)?;
            lookdev::pt_plates::write_linear_exr(
                &out.join(format!("{stem}_pt.exr")),
                w,
                h,
                &[("full", &beauty)],
            )?;
        }
        drop(renderer);
        free_dropped(&device);
        for (pass, suffix, aux_model) in &aux {
            // A fresh renderer per pass: temporal state (history, held haze) must not carry the
            // shaded frame's light into an index or class pass.
            let mut aux_renderer = regia_render::FrameRenderer::new();
            let f = render_studio(&device, &mut aux_renderer, aux_model, &aux_setup(&setup, *pass))?;
            lookdev::pt_plates::write_linear_exr(
                &out.join(format!("{stem}_{suffix}.exr")),
                f.hdr.width,
                f.hdr.height,
                &[("full", &f.hdr.rgb)],
            )?;
            // With `--pt`, the sun pass is traced too (direct light only): the raster's sun
            // visibility against the tracer's, shadow outline for shadow outline.
            if let (AuxPass::Sun, Some(spp), Some(tracer)) = (pass, pt_spp, tracer.as_ref()) {
                let sun = lookdev::hero_ref::trace_studio_bounces(
                    tracer,
                    aux_model,
                    &aux_setup(&setup, *pass),
                    spp,
                    BLENDER_SUN_RADIUS,
                    1,
                )?;
                lookdev::pt_plates::write_linear_exr(
                    &out.join(format!("{stem}_{suffix}_pt.exr")),
                    f.hdr.width,
                    f.hdr.height,
                    &[("full", &sun)],
                )?;
            }
            drop(aux_renderer);
            free_dropped(&device);
        }
        println!(
            "hero_ref {stem}: {w}x{h}, analytic sunlit ground {:.1} cd/m², streaming settle frames {}",
            frame.ground_luminance, frame.settle_frames
        );
    }
    Ok(())
}
