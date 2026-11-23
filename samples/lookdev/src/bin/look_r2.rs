// SPDX-License-Identifier: MIT-0
//! `look_r2`: the look-director parity shots (`lookdev::look_r2`) at 960×540.
//!
//! ```text
//! look_r2 --out <dir> [--shot <name>] [--width 960] [--height 540] [--frames 14] [--res 1025]
//!         [--eye x,z] [--above m] [--yaw deg-from-sun] [--pitch deg] [--set parity] [--perf]
//!
//! With `REGIA_DO27_CONTENT=<do27-content dir>` the shots render DO27's own maps, set
//! pieces and plate cameras (see `lookdev::do27`); without it, the stand-ins.
//! look_r2 --side-by-side <before_dir> <after_dir> <out_dir>
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::default_look::{read_png, side_by_side, write_png};
use lookdev::look_r2::{CameraOverride, R2Options, R2Planets, R2Shot, render_shot};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("look_r2: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, LookdevError> {
    args.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| LookdevError::Config(format!("{flag} needs a value")))
}

fn number<T: std::str::FromStr>(args: &[String], i: usize, flag: &str) -> Result<T, LookdevError> {
    value(args, i, flag)?
        .parse()
        .map_err(|_| LookdevError::Config(format!("{flag}: not a number")))
}

fn pair(args: &[String], i: usize, flag: &str) -> Result<[f32; 2], LookdevError> {
    let text = value(args, i, flag)?;
    let mut it = text.split(',').map(str::trim).map(str::parse::<f32>);
    match (it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b))) => Ok([a, b]),
        _ => Err(LookdevError::Config(format!("{flag}: expected x,z"))),
    }
}

fn io_err(path: &std::path::Path, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    if args.first().map(String::as_str) == Some("--side-by-side") {
        let [_, before, after, out] = args else {
            return Err(LookdevError::Config(
                "--side-by-side <before> <after> <out>".into(),
            ));
        };
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
        for shot in R2Shot::ALL {
            let name = format!("{}.png", shot.name());
            let (Ok(left), Ok(right)) = (
                read_png(&PathBuf::from(before).join(&name)),
                read_png(&PathBuf::from(after).join(&name)),
            ) else {
                continue;
            };
            if (left.0, left.1) != (right.0, right.1) {
                return Err(LookdevError::Config(format!("{name}: sizes differ")));
            }
            let (ow, pixels) = side_by_side(left.0, left.1, &left.2, &right.2);
            let path = out.join(&name);
            write_png(&path, ow, left.1, &pixels)?;
            println!("wrote {}", path.display());
        }
        return Ok(());
    }
    let mut options = R2Options::evidence();
    let mut camera = CameraOverride::default();
    let mut out: Option<PathBuf> = None;
    let mut only: Option<String> = None;
    let mut parity = false;
    let mut cameras: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out")?)),
            "--width" => options.width = number(args, i, "--width")?,
            "--height" => options.height = number(args, i, "--height")?,
            "--frames" => options.frames = number(args, i, "--frames")?,
            "--res" => options.terrain_resolution = number(args, i, "--res")?,
            "--shot" => only = Some(value(args, i, "--shot")?.to_string()),
            "--set" => parity = value(args, i, "--set")? == "parity",
            "--perf" => {
                // GPU pass timings: 50 frames of a drifting eye, median after warm-up.
                options.timing = true;
                options.frames = 50;
                i += 1;
                continue;
            }
            "--eye" => camera.eye = Some(pair(args, i, "--eye")?),
            "--above" => camera.above = Some(number(args, i, "--above")?),
            "--yaw" => camera.yaw_from_sun = Some(number(args, i, "--yaw")?),
            "--pitch" => camera.pitch = Some(number(args, i, "--pitch")?),
            // --cameras <file>: one `name ex ey ez tx ty tz fov°` per line, each rendered with
            // the shot's world (loaded once) to <out>/<name>.png.
            "--cameras" => cameras = Some(PathBuf::from(value(args, i, "--cameras")?)),
            "--map" => {
                let planet = if value(args, i, "--map")? == "europa" {
                    lookdev::Planet::Europa
                } else {
                    lookdev::Planet::Mars
                };
                let mut planets = R2Planets::default();
                let scene = planets.get(planet, options.terrain_resolution)?;
                let n = 512u32;
                let mut px = Vec::with_capacity((n * n * 4) as usize);
                let (lo, hi) = scene
                    .map
                    .heights()
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(a, b), h| (a.min(*h), b.max(*h)));
                for y in 0..n {
                    for x in 0..n {
                        let wx = (x as f32 / n as f32 - 0.5) * 4096.0;
                        let wz = (0.5 - y as f32 / n as f32) * 4096.0;
                        let h = scene.height_at(wx, wz);
                        let shade = scene.height_at(wx + 8.0, wz) - h;
                        let v = ((h - lo) / (hi - lo).max(1.0) * 200.0 + shade * 4.0 + 30.0).clamp(0.0, 255.0)
                            as u8;
                        let grid = (x % 64 == 0) || (y % 64 == 0);
                        px.extend_from_slice(&if grid { [255, 0, 0, 255] } else { [v, v, v, 255] });
                    }
                }
                let path = PathBuf::from(value(args, i, "--map")?).with_extension("map.png");
                write_png(&path, n, n, &px)?;
                println!(
                    "wrote {} (grid every 512 m, center 0,0, +x right, +z up; heights {lo:.0}..{hi:.0})",
                    path.display()
                );
                return Ok(());
            }
            other => return Err(LookdevError::Config(format!("unknown flag {other}"))),
        }
        i += 2;
    }
    let out = out.ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?;
    std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
    if let Some(name) = only.as_deref()
        && R2Shot::from_name(name).is_none()
    {
        return Err(LookdevError::Config(format!("unknown shot {name}")));
    }
    // DO27's cooked terrain is BC7/BC5/BC4: ask for block compression when rendering it.
    let device = if lookdev::do27::content_dir().is_some() {
        regia_terrain::gpu::headless_bcn_device()?
    } else {
        regia_render::RenderDevice::headless()?
    };
    let mut planets = R2Planets::default();
    if let Some(list) = cameras {
        let shot = only
            .as_deref()
            .and_then(R2Shot::from_name)
            .ok_or_else(|| LookdevError::Config("--cameras needs --shot".into()))?;
        let text = std::fs::read_to_string(&list).map_err(|e| io_err(&list, e))?;
        for line in text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            let mut it = line.split_whitespace();
            let name = it.next().unwrap_or("cam").to_string();
            let v: Vec<f32> = it.filter_map(|x| x.parse().ok()).collect();
            let [ex, ey, ez, tx, ty, tz, fov] = v[..] else {
                return Err(LookdevError::Config(format!("{name}: expected 7 numbers")));
            };
            camera.look_at = Some([ex, ey, ez, tx, ty, tz, fov]);
            let started = Instant::now();
            // LOOK_R2_PROBE_PX="x,y;x,y" (final-image pixels): print the frame's light inputs and
            // the linear HDR radiance (full and direct-only, cd/m²) around those pixels.
            let probes: Vec<(f32, f32)> = std::env::var("LOOK_R2_PROBE_PX")
                .unwrap_or_default()
                .split(';')
                .filter_map(|p| {
                    p.split_once(',')
                        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
                })
                .collect();
            let (width, height) = (options.width as f32, options.height as f32);
            let mut run = |cap: lookdev::look_r2::ShotCapture<'_>| -> Result<(), LookdevError> {
                let f = cap.frame;
                println!(
                    "{name}: sun dir {:?} colour {:?} illuminance {} lux; exposure_ev {} ambient_lux {} ibl {} directional_shadows {}",
                    f.directional.direction,
                    f.directional.color,
                    f.directional.illuminance,
                    f.exposure_ev,
                    f.ambient_lux,
                    f.ibl,
                    f.directional_shadows
                );
                if let Some(post) = f.post.as_ref() {
                    println!(
                        "{name}: post auto_exposure {} band {}..{} compensation {}",
                        post.auto_exposure,
                        post.auto_exposure_min_ev100,
                        post.auto_exposure_max_ev100,
                        post.exposure_compensation
                    );
                }
                for &(px, py) in &probes {
                    let at = |h: &regia_render::HdrCapture| {
                        // The image is mirrored after rendering (DO27's left-handed frame).
                        let cx = ((width - 1.0 - px) / width * h.width as f32) as i32;
                        let cy = (py / height * h.height as f32) as i32;
                        let mut sum = [0.0f32; 3];
                        let mut n = 0.0f32;
                        for dy in -2..=2 {
                            for dx in -2..=2 {
                                let (x, y) = (cx + dx, cy + dy);
                                if x >= 0 && y >= 0 && (x as u32) < h.width && (y as u32) < h.height {
                                    let c = h.rgb[(y as u32 * h.width + x as u32) as usize];
                                    for k in 0..3 {
                                        sum[k] += c[k];
                                    }
                                    n += 1.0;
                                }
                            }
                        }
                        sum.map(|v| v / n.max(1.0))
                    };
                    println!(
                        "{name}: px ({px},{py}) radiance full {:?} direct {:?} cd/m2",
                        at(&cap.hdr_full),
                        at(&cap.hdr_direct)
                    );
                }
                Ok(())
            };
            let hook = (!probes.is_empty()).then(|| lookdev::look_r2::ShotHook {
                albedo: regia_pt::terrain_bake::AlbedoBake::default(),
                run: &mut run,
            });
            let (pixels, _) =
                lookdev::look_r2::render_shot_with(&device, &mut planets, shot, options, camera, None, hook)?;
            let path = out.join(format!("{name}.png"));
            write_png(&path, options.width, options.height, &pixels)?;
            println!(
                "{name} -> {} ({:.1} s)",
                path.display(),
                started.elapsed().as_secs_f32()
            );
        }
        return Ok(());
    }
    let shots: &[R2Shot] = if parity || only.is_some() {
        &R2Shot::PARITY
    } else {
        &R2Shot::ALL
    };
    for &shot in shots {
        if only.as_deref().is_some_and(|name| name != shot.name()) {
            continue;
        }
        let started = Instant::now();
        let (pixels, frame) = render_shot(&device, &mut planets, shot, options, camera)?;
        let path = out.join(format!("{}.png", shot.name()));
        write_png(&path, options.width, options.height, &pixels)?;
        println!(
            "{} -> {} ({:.1} s) eye {:?} target {:?} sun {:?}",
            shot.name(),
            path.display(),
            started.elapsed().as_secs_f32(),
            frame.eye.map(|v| v.round()),
            frame.target.map(|v| v.round()),
            frame.toward_sun.map(|v| (v * 100.0).round() / 100.0),
        );
    }
    Ok(())
}
