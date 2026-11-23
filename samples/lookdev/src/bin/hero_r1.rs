// SPDX-License-Identifier: MIT-0
//! `hero_r1`: DO27's hero buildings and the X12 on DO27's own ground, three cameras each
//! (lane hero-lookdev). See [`lookdev::hero`].
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> hero_r1 --out <dir> [--asset <name>|all] [--list]
//!     [--size 1920x1080] [--tex 2048] [--frames 14] [--occluder]
//! ```
//!
//! `--occluder` registers the asset's large-mesh proxy (`regia_render::MeshOccluder`).
//!
//! Writes `<dir>/<asset>_{wide,mid,close}.png` (Desktop High, native resolution).

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::hero::{self, HeroAsset};
use lookdev::look_r2::{HeroInsert, R2Options, R2Planets, R2Shot, render_hero};
use regia_core::glam::Vec3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hero_r1: {err}");
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

/// The `--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>` cameras, offsets from `centre`.
fn custom_cams(args: &[String], centre: Vec3) -> Result<Vec<(String, Vec3, Vec3, f32)>, LookdevError> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a != "--cam" {
            continue;
        }
        let bad = || LookdevError::Config("--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>".into());
        let v = args.get(i + 1).ok_or_else(bad)?;
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() != 4 {
            return Err(bad());
        }
        let vec3 = |s: &str| -> Result<Vec3, LookdevError> {
            let f: Vec<f32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if f.len() == 3 {
                Ok(Vec3::new(f[0], f[1], f[2]))
            } else {
                Err(bad())
            }
        };
        let fov: f32 = parts[3].parse().map_err(|_| bad())?;
        out.push((
            parts[0].to_string(),
            centre + vec3(parts[1])?,
            centre + vec3(parts[2])?,
            fov,
        ));
    }
    Ok(out)
}

/// Unit horizontal vector at `azimuth` radians (x = sin, z = cos).
fn horiz(azimuth: f32) -> Vec3 {
    Vec3::new(azimuth.sin(), 0.0, azimuth.cos())
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let assets = hero::catalog(&content);
    if args.iter().any(|a| a == "--list") {
        for a in &assets {
            println!("{} ({:?}): {} files", a.name, a.planet, a.files.len());
        }
        return Ok(());
    }
    let out = PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let which = value(args, "--asset").unwrap_or("all");
    let mut options = R2Options::evidence();
    options.width = 1920;
    options.height = 1080;
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
    let picked: Vec<&HeroAsset> = assets
        .iter()
        .filter(|a| which == "all" || a.name == which)
        .collect();
    if picked.is_empty() {
        return Err(LookdevError::Config(format!(
            "no hero asset {which} (try --list)"
        )));
    }
    for asset in picked {
        let started = std::time::Instant::now();
        let mut model = hero::load(asset, max_tex)?;
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
        // The flattest sunlit spot within 240 m of the plate camera's target.
        let half = (model.max - model.min) * 0.5;
        let radius = half.x.hypot(half.z).max(1.0);
        let spot = hero::sunlit_spot(|x, z| terrain.height_at(x, z), anchor, toward_sun, radius, half.y);
        let centre_xz = (model.min + model.max) * 0.5;
        // Seated 5 cm into the highest ground under the footprint.
        let offset = Vec3::new(
            spot.x - centre_xz.x,
            spot.y - model.min.y - 0.05,
            spot.z - centre_xz.z,
        );
        hero::place(&mut model, 0.0, offset);
        let centre = (model.min + model.max) * 0.5;
        let height = model.max.y - model.min.y;
        let sun_az = toward_sun.x.atan2(toward_sun.z);
        let ground = |p: Vec3| terrain.height_at(p.x, p.z);
        let lift = |e: Vec3, min: f32| Vec3::new(e.x, e.y.max(ground(e) + min), e.z);
        let size = radius.max(height * 0.6);
        // Wide: the sun 40° off the camera's back, a little above the asset.
        let wide_dir = horiz(sun_az + 40f32.to_radians());
        let wide_eye = lift(
            centre + wide_dir * (2.4 * size + 8.0) + Vec3::Y * (0.35 * size + 3.0),
            2.0,
        );
        // Mid: three-quarters, the sun 60° to the other side.
        let mid_dir = horiz(sun_az - 60f32.to_radians());
        let mid_eye = lift(
            centre + mid_dir * (1.25 * size + 3.0) + Vec3::Y * (0.1 * size + 0.5),
            1.6,
        );
        // Close: 1.4 m from what the mid camera sees at 40 % of the height.
        let aim = Vec3::new(centre.x, model.min.y + height * 0.4, centre.z);
        let (hit, normal) =
            hero::raycast(&model.meshes, mid_eye, (aim - mid_eye).normalize()).unwrap_or((aim, mid_dir));
        let side = Vec3::Y.cross(normal).normalize_or_zero();
        let close_eye = lift(hit + normal * 1.4 + side * 0.5 + Vec3::Y * 0.3, 1.2);
        let mut cams = vec![
            (
                "wide".to_string(),
                wide_eye,
                centre + Vec3::Y * (height * 0.15),
                38f32,
            ),
            ("mid".to_string(), mid_eye, centre, 45.0),
            ("close".to_string(), close_eye, hit, 55.0),
        ];
        // `--cam <name>:<ex>,<ey>,<ez>:<tx>,<ty>,<tz>:<fov°>` (repeatable) replaces the three
        // framings with cameras placed relative to the asset's bounds centre (the asset is not
        // rotated: its own axes, X mirrored into the content frame), e.g. a 1 m canopy close-up.
        eprintln!(
            "{}: center {:?} min {:?} max {:?} toward sun {:?}",
            asset.name,
            centre.to_array(),
            model.min.to_array(),
            model.max.to_array(),
            toward_sun.to_array()
        );
        let custom = custom_cams(args, centre)?;
        if !custom.is_empty() {
            cams = custom;
        }
        eprintln!(
            "{}: {} meshes, {} materials, {:.1}×{:.1}×{:.1} m at {:?} (load {:.1} s)",
            asset.name,
            model.meshes.len(),
            model.materials.len(),
            model.max.x - model.min.x,
            height,
            model.max.z - model.min.z,
            spot.to_array(),
            started.elapsed().as_secs_f32()
        );
        // `--occluder`: the asset's large-mesh proxy (its bounds, the materials' mean albedo,
        // standing on the spot), as a host adds `OcclusionProxy` to a parked vehicle.
        let occluders = if args.iter().any(|a| a == "--occluder") {
            let n = model.materials.len().max(1) as f32;
            let albedo = model.materials.iter().fold([0.0f32; 3], |acc, m| {
                [
                    acc[0] + m.base_color[0] / n,
                    acc[1] + m.base_color[1] / n,
                    acc[2] + m.base_color[2] / n,
                ]
            });
            vec![
                regia_render::MeshOccluder::from_local_bounds(
                    regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    model.min.to_array(),
                    model.max.to_array(),
                    albedo,
                )
                .with_ground(spot.y),
            ]
        } else {
            Vec::new()
        };
        let mut insert = HeroInsert {
            meshes: std::mem::take(&mut model.meshes),
            materials: std::mem::take(&mut model.materials),
            eye: [0.0; 3],
            target: [0.0; 3],
            fov_y: 0.7,
            glass: Vec::new(),
            occluders,
            lights: Vec::new(),
        };
        for (cam, eye, target, fov_deg) in cams {
            let t = std::time::Instant::now();
            insert.eye = eye.to_array();
            insert.target = target.to_array();
            insert.fov_y = fov_deg.to_radians();
            let (pixels, _) = render_hero(&device, &mut planets, shot, options, &insert)?;
            let path = out.join(format!("{}_{cam}.png", asset.name));
            write_png(&path, options.width, options.height, &pixels)?;
            println!(
                "{} -> {} ({:.1} s)",
                asset.name,
                path.display(),
                t.elapsed().as_secs_f32()
            );
        }
    }
    Ok(())
}
