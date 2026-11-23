// SPDX-License-Identifier: MIT-0
//! `hull_terms`: a DO27 vehicle parked on its map, seen from the side away from the sun, read
//! back as linear HDR and averaged over the hull's pixels (lane v017-black-hull: the black
//! Remora hull on Bore Twelve).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> hull_terms --out <dir> [--asset remora_europa]
//!     [--cam <name>:<azimuth° from the sun>:<distance m>:<eye height m>]...
//!     [--point <dx>,<dy>,<dz>:<lumens>:<radius m>]... [--size 960x540] [--frames 14]
//!     [--tex 2048] [--tag base]
//! ```
//!
//! Cameras stand around the hull's bounds centre: azimuth 0 looks at the sunlit side, 180 at
//! the side away from the sun; they aim at the centre. `--point` adds a shadowless point light
//! at an offset from the bounds centre (a host's hull light). The default cameras are DO27's
//! `cam ship 16 2 <az>` framings at 180 (shadow side), 90 and 0, plus a low one under the belly.
//!
//! Per camera: `<dir>/<tag>_<asset>_<cam>.png` and one line on stdout and in `<dir>/terms.csv`
//! with the mean luminance (cd/m²) over the hull pixels facing away from the sun and over all
//! hull pixels, in the full frame and in the direct-only frame (sun, planet-shine fill and local
//! lights: no sky, probes, ground bounce or AO), and the sunlit ground for scale. The frame's
//! knobs come from the environment (`LOOK_R2_FILL`, `LOOK_R2_NO_SSAO`, `LOOK_R2_NO_PROBES`, …,
//! see [`lookdev::look_r2`]), so one run per variant gives the terms by difference.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::hero;
use lookdev::look_r2::{
    CameraOverride, HeroInsert, R2Options, R2Planets, R2Shot, ShotHook, render_shot_with,
};
use regia_core::glam::Vec3;
use regia_render::{LabLocalLight, LabMesh, LabPointLight};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hull_terms: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| *a == flag)
        .filter_map(|(i, _)| args.get(i + 1).map(String::as_str))
        .collect()
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

/// One `--cam`: name, azimuth from the sun (radians), distance and eye height (metres).
fn cameras(args: &[String]) -> Result<Vec<(String, f32, f32, f32)>, LookdevError> {
    let given = values(args, "--cam");
    if given.is_empty() {
        return Ok(vec![
            ("shadow".into(), 180f32.to_radians(), 16.0, 2.0),
            ("side".into(), 90f32.to_radians(), 16.0, 2.0),
            ("sun".into(), 0.0, 16.0, 2.0),
            ("belly".into(), 160f32.to_radians(), 9.0, 0.5),
        ]);
    }
    given
        .into_iter()
        .map(|v| {
            let parts: Vec<&str> = v.split(':').collect();
            let num = |s: &str| s.parse::<f32>().ok();
            match parts.as_slice() {
                [name, az, dist, h] => match (num(az), num(dist), num(h)) {
                    (Some(az), Some(dist), Some(h)) => Ok(((*name).to_string(), az.to_radians(), dist, h)),
                    _ => Err(LookdevError::Config(format!("--cam {v}: not numbers"))),
                },
                _ => Err(LookdevError::Config(format!(
                    "--cam {v}: want <name>:<azimuth°>:<distance m>:<height m>"
                ))),
            }
        })
        .collect()
}

/// `--point <dx>,<dy>,<dz>:<lumens>:<radius>` as shadowless point lights around `centre`.
fn points(args: &[String], centre: Vec3) -> Result<Vec<LabLocalLight>, LookdevError> {
    values(args, "--point")
        .into_iter()
        .map(|v| {
            let parts: Vec<&str> = v.split(':').collect();
            let bad = || LookdevError::Config(format!("--point {v}: want <dx>,<dy>,<dz>:<lumens>:<radius>"));
            let [offset, lumens, radius] = parts.as_slice() else {
                return Err(bad());
            };
            let o = floats(offset);
            let (Ok(lumens), Ok(radius)) = (lumens.parse::<f32>(), radius.parse::<f32>()) else {
                return Err(bad());
            };
            if o.len() != 3 {
                return Err(bad());
            }
            Ok(LabLocalLight::Point(LabPointLight {
                position: (centre + Vec3::new(o[0], o[1], o[2])).to_array(),
                color: [1.0, 1.0, 1.0],
                intensity: lumens,
                radius,
                cast_shadows: false,
                source_radius: None,
            }))
        })
        .collect()
}

/// Mean luminance over hull pixels, split by which way their surface faces.
#[derive(Default, Clone)]
struct Means {
    shade_full: f64,
    shade_direct: f64,
    shade_n: u32,
    all_full: f64,
    all_direct: f64,
    all_n: u32,
    ground: Vec<f64>,
}

fn lum(c: [f32; 3]) -> f64 {
    f64::from(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
}

/// Classifies a grid of the capture's pixels by casting their camera rays at the hull
/// meshes: hull pixels facing away from the sun, every hull pixel, and the open ground.
#[allow(clippy::too_many_arguments)] // one capture and the scene it was taken of
fn measure(
    full: &regia_render::HdrCapture,
    direct: &regia_render::HdrCapture,
    eye: Vec3,
    target: Vec3,
    fov_y: f32,
    meshes: &[LabMesh],
    toward_sun: Vec3,
    ground_y: f32,
) -> Means {
    let mut m = Means::default();
    let (w, h) = (full.width, full.height);
    let forward = (target - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let t = (fov_y * 0.5).tan();
    let aspect = w as f32 / h.max(1) as f32;
    // Per-mesh bounds, so a ray skips the meshes it cannot hit.
    let bounds: Vec<(Vec3, Vec3)> = meshes
        .iter()
        .map(|mesh| {
            mesh.vertices
                .iter()
                .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), v| {
                    let p = Vec3::from_array(v.position);
                    (lo.min(p), hi.max(p))
                })
        })
        .collect();
    let step = (w / 160).max(1);
    for py in (0..h).step_by(step as usize) {
        for px in (0..w).step_by(step as usize) {
            let x = (2.0 * (px as f32 + 0.5) / w as f32 - 1.0) * t * aspect;
            let y = (1.0 - 2.0 * (py as f32 + 0.5) / h as f32) * t;
            let d = (forward + right * x + up * y).normalize();
            let candidates: Vec<LabMesh> = meshes
                .iter()
                .zip(&bounds)
                .filter(|(_, (lo, hi))| ray_hits_box(eye, d, *lo, *hi))
                .map(|(mesh, _)| mesh.clone())
                .collect();
            let i = (py * w + px) as usize;
            let (Some(f), Some(dr)) = (full.rgb.get(i), direct.rgb.get(i)) else {
                continue;
            };
            if let Some((_, n)) = hero::raycast(&candidates, eye, d) {
                m.all_full += lum(*f);
                m.all_direct += lum(*dr);
                m.all_n += 1;
                if n.dot(toward_sun) < 0.0 {
                    m.shade_full += lum(*f);
                    m.shade_direct += lum(*dr);
                    m.shade_n += 1;
                }
            } else if d.y < -0.02 {
                // Open ground within 60 m: its 90th percentile stands for the sunlit ground.
                let hit = eye + d * ((ground_y - eye.y) / d.y);
                if (hit - eye).length() < 60.0 {
                    m.ground.push(lum(*f));
                }
            }
        }
    }
    m
}

/// The 90th percentile of the ground pixels' luminance (NaN without any).
fn sunlit_ground(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[(values.len() * 9 / 10).min(values.len() - 1)]
}

fn ray_hits_box(o: Vec3, d: Vec3, lo: Vec3, hi: Vec3) -> bool {
    let inv = d.recip();
    let a = (lo - o) * inv;
    let b = (hi - o) * inv;
    let near = a.min(b).max_element();
    let far = a.max(b).min_element();
    far >= near.max(0.0)
}

#[allow(clippy::too_many_lines)] // argument parsing, placement, then one render per camera
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
    let name = value(args, "--asset").unwrap_or("remora_europa");
    let tag = value(args, "--tag").unwrap_or("base");
    let asset = hero::catalog(&content)
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| LookdevError::Config(format!("no hero asset {name} in the drop")))?;
    let mut options = R2Options::evidence();
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    if let Some(f) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        options.frames = f;
    }
    let max_tex: u32 = value(args, "--tex").and_then(|v| v.parse().ok()).unwrap_or(2048);
    let device = regia_terrain::gpu::headless_bcn_device()?;
    let mut planets = R2Planets::default();
    let mut model = hero::load(&asset, max_tex)?;
    let shot = match asset.planet {
        lookdev::Planet::Mars => R2Shot::SwitchbacksMid,
        lookdev::Planet::Europa => R2Shot::BoreTwelveMid,
    };
    let terrain = planets.get(asset.planet, options.terrain_resolution)?;
    let content_map = terrain
        .do27
        .as_deref()
        .ok_or_else(|| LookdevError::Config("the map has no DO27 content".into()))?;
    let toward_sun = Vec3::from_array(content_map.map.sun.toward);
    let anchor = content_map
        .cameras
        .get(shot.name())
        .map_or(Vec3::ZERO, |c| Vec3::from_array(c.target));
    let half = (model.max - model.min) * 0.5;
    let radius = half.x.hypot(half.z).max(1.0);
    let spot = hero::sunlit_spot(|x, z| terrain.height_at(x, z), anchor, toward_sun, radius, half.y);
    let centre_xz = (model.min + model.max) * 0.5;
    let offset = Vec3::new(
        spot.x - centre_xz.x,
        spot.y - model.min.y - 0.05,
        spot.z - centre_xz.z,
    );
    hero::place(&mut model, 0.0, offset);
    let centre = (model.min + model.max) * 0.5;
    let ground_y = model.min.y + 0.05;
    let sun_az = toward_sun.x.atan2(toward_sun.z);
    eprintln!(
        "{name}: centre {:?} size {:?} toward sun {:?}",
        centre.to_array(),
        (model.max - model.min).to_array(),
        toward_sun.to_array()
    );
    // Poses first: the terrain borrow ends before the renders take the planets again.
    let poses: Vec<(String, Vec3, Vec3)> = cameras(args)?
        .into_iter()
        .map(|(cam, az, dist, height)| {
            let a = sun_az + az;
            let (x, z) = (centre.x + a.sin() * dist, centre.z + a.cos() * dist);
            let eye = Vec3::new(x, (ground_y + height).max(terrain.height_at(x, z) + 0.3), z);
            let target = Vec3::new(centre.x, centre.y.min(eye.y + dist * 0.3), centre.z);
            (cam, eye, target)
        })
        .collect();
    let lights = points(args, centre)?;
    let hull_meshes = model.meshes.clone();
    let mut insert = HeroInsert {
        meshes: std::mem::take(&mut model.meshes),
        materials: std::mem::take(&mut model.materials),
        eye: [0.0; 3],
        target: [0.0; 3],
        fov_y: 50f32.to_radians(),
        glass: Vec::new(),
        occluders: Vec::new(),
        lights,
    };
    let csv_path = out.join("terms.csv");
    let new_csv = !csv_path.exists();
    let mut csv = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&csv_path)
        .map_err(|source| LookdevError::Io {
            path: csv_path.display().to_string(),
            source,
        })?;
    let io = |source| LookdevError::Io {
        path: csv_path.display().to_string(),
        source,
    };
    if new_csv {
        writeln!(
            csv,
            "tag,asset,cam,shade_full,shade_direct,shade_indirect,shade_px,all_full,all_direct,all_px,sunlit_ground"
        )
        .map_err(io)?;
    }
    for (cam, eye, target) in poses {
        insert.eye = eye.to_array();
        insert.target = target.to_array();
        let mut means = None;
        let mut hook = |capture: lookdev::look_r2::ShotCapture<'_>| -> Result<(), LookdevError> {
            means = Some(measure(
                &capture.hdr_full,
                &capture.hdr_direct,
                Vec3::from_array(capture.frame.eye),
                Vec3::from_array(capture.frame.target),
                capture.frame.fov_y,
                &hull_meshes,
                toward_sun,
                ground_y,
            ));
            Ok(())
        };
        let (pixels, _) = render_shot_with(
            &device,
            &mut planets,
            shot,
            options,
            CameraOverride::default(),
            Some(&insert),
            Some(ShotHook {
                albedo: regia_pt::terrain_bake::AlbedoBake::default(),
                run: &mut hook,
            }),
        )?;
        let path = out.join(format!("{tag}_{name}_{cam}.png"));
        write_png(&path, options.width, options.height, &pixels)?;
        let mut m = means.ok_or_else(|| LookdevError::Config("the capture hook did not run".into()))?;
        let avg = |s: f64, n: u32| if n == 0 { f64::NAN } else { s / f64::from(n) };
        let shade_full = avg(m.shade_full, m.shade_n);
        let shade_direct = avg(m.shade_direct, m.shade_n);
        let line = format!(
            "{tag},{name},{cam},{shade_full:.3},{shade_direct:.3},{:.3},{},{:.3},{:.3},{},{:.1}",
            shade_full - shade_direct,
            m.shade_n,
            avg(m.all_full, m.all_n),
            avg(m.all_direct, m.all_n),
            m.all_n,
            sunlit_ground(&mut m.ground),
        );
        println!("{line}  -> {}", path.display());
        writeln!(csv, "{line}").map_err(io)?;
    }
    Ok(())
}
