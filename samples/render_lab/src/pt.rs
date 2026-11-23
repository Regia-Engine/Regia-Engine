// SPDX-License-Identifier: MIT-0
//! `render_lab pt <scene>`: the reference path tracer on a lab scene (`specs/27` § 1).
//!
//! The lab scenes (`material_grid`, `clustered_lights`, `light_shadows`, `surface_maps`,
//! `alpha_modes`, `ibl_spheres`) are extracted exactly as the real-time frame draws them
//! (`regia_pt::extract::from_frame`); `cornell`, `cornell_room` and `furnace` are the tracer's
//! validation scenes. Writes `<scene>_pt.exr` (every AOV), `<scene>_pt.png` (AgX preview at the
//! scene's exposure), `<scene>_pt.json` (metadata) and, for lab scenes, `<scene>_rt.png` (the
//! real-time frame) into `--out` (default `target/render-lab/pt`).

use std::path::PathBuf;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{Aov, PathTracer, PtScene, PtSettings};
use regia_render::{LabScene, RenderDevice, render_lab_scene};
use regia_test_utils::scenes::{
    alpha_modes_scene, clustered_lights_scene, ibl_spheres_scene, light_shadows_scene,
    material_sphere_grid_scene, surface_maps_scene,
};

/// Scenes `render_lab pt` knows.
pub const SCENES: &[&str] = &[
    "material_grid",
    "clustered_lights",
    "light_shadows",
    "surface_maps",
    "alpha_modes",
    "ibl_spheres",
    "cornell",
    "cornell_room",
    "furnace",
];

struct Options {
    scene: String,
    spp: u32,
    bounces: u32,
    size: Option<(u32, u32)>,
    out: PathBuf,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let scene = args
        .first()
        .filter(|s| !s.starts_with("--"))
        .ok_or_else(|| format!("which scene? one of {}", SCENES.join(", ")))?
        .clone();
    let mut o = Options {
        scene,
        spp: 256,
        bounces: 12,
        size: None,
        out: PathBuf::from("target/render-lab/pt"),
    };
    let mut width = None;
    let mut height = None;
    let mut i = 1;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{} needs a value", args[i]))?;
        let num = || {
            value
                .parse::<u32>()
                .map_err(|_| format!("{}: not a number", args[i]))
        };
        match args[i].as_str() {
            "--spp" => o.spp = num()?.max(1),
            "--bounces" => o.bounces = num()?.max(1),
            "--width" => width = Some(num()?.max(1)),
            "--height" => height = Some(num()?.max(1)),
            "--out" => o.out = PathBuf::from(value),
            other => return Err(format!("unknown flag {other}")),
        }
        i += 2;
    }
    if let (Some(w), Some(h)) = (width, height) {
        o.size = Some((w, h));
    } else if width.is_some() || height.is_some() {
        return Err("give both --width and --height".into());
    }
    Ok(o)
}

fn lab_scene(name: &str) -> Option<LabScene> {
    Some(match name {
        "material_grid" => material_sphere_grid_scene(),
        "clustered_lights" => clustered_lights_scene(),
        "light_shadows" => light_shadows_scene(),
        "surface_maps" => surface_maps_scene(),
        "alpha_modes" => alpha_modes_scene(),
        "ibl_spheres" => ibl_spheres_scene(),
        _ => return None,
    })
}

/// Runs `render_lab pt` with the arguments after `pt`.
///
/// # Errors
///
/// Bad arguments, an unknown scene, no GPU, a failed trace or a failed write.
pub fn run(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let device = RenderDevice::headless().map_err(|e| format!("no GPU: {e}"))?;
    let mut lab = lab_scene(&o.scene);
    if let (Some(lab), Some((w, h))) = (lab.as_mut(), o.size) {
        lab.width = w;
        lab.height = h;
    }
    let (w, h) = o.size.unwrap_or((512, 512));
    let (scene, ev100): (PtScene, f32) = match (&lab, o.scene.as_str()) {
        (Some(lab), _) => {
            let frame = lab.as_frame_scene();
            (from_frame(&frame, &ExtractOptions::default()), lab.exposure_ev)
        }
        (None, "cornell") => (regia_pt::scenes::cornell_box(w, h, true), 0.0),
        (None, "cornell_room") => (regia_pt::scenes::cornell_box(w, h, false), 0.0),
        (None, "furnace") => (
            regia_pt::scenes::furnace_sphere(regia_pt::PtMaterial::lambert([1.0; 3]), w.min(h)),
            0.0,
        ),
        (None, other) => return Err(format!("unknown scene {other}; one of {}", SCENES.join(", "))),
    };
    std::fs::create_dir_all(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    let settings = PtSettings {
        spp: o.spp,
        max_bounces: o.bounces,
        ..PtSettings::default()
    };
    let started = std::time::Instant::now();
    let mut session = tracer.begin(&scene, &settings).map_err(|e| e.to_string())?;
    session
        .run_with(o.spp, |done| {
            if done % 64 == 0 || done == o.spp {
                println!("  {}: {done}/{} spp", o.scene, o.spp);
            }
            true
        })
        .map_err(|e| e.to_string())?;
    let img = session.image().map_err(|e| e.to_string())?;
    let path = |suffix: &str| o.out.join(format!("{}_{suffix}", o.scene));
    img.write_exr(&path("pt.exr"), &[]).map_err(|e| e.to_string())?;
    img.write_metadata_json(&path("pt.json"))
        .map_err(|e| e.to_string())?;
    regia_pt::output::write_png(
        &path("pt.png"),
        img.width,
        img.height,
        &img.preview_rgba8(Aov::Beauty, ev100),
    )
    .map_err(|e| e.to_string())?;
    if let Some(lab) = &lab {
        let rt = render_lab_scene(&device, lab).map_err(|e| e.to_string())?;
        regia_pt::output::write_png(&path("rt.png"), rt.width, rt.height, &rt.pixels)
            .map_err(|e| e.to_string())?;
    }
    println!(
        "{}: {} spp in {:.1} s ({} triangles) → {}",
        o.scene,
        img.spp,
        started.elapsed().as_secs_f32(),
        img.metadata.triangles,
        path("pt.exr").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_rejects_half_sizes() {
        let args: Vec<String> = ["cornell", "--spp", "8", "--width", "32", "--height", "16"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse(&args).expect("parse");
        assert_eq!((o.scene.as_str(), o.spp, o.size), ("cornell", 8, Some((32, 16))));
        let half: Vec<String> = ["cornell", "--width", "32"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse(&half).is_err());
        assert!(parse(&[]).is_err());
        assert!(
            SCENES
                .iter()
                .all(|s| lab_scene(s).is_some() || s.starts_with("cornell") || *s == "furnace")
        );
    }
}
