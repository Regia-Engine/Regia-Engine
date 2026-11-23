// SPDX-License-Identifier: MIT-0
//! `dyn_light`: the dynamic-light test plate (see [`lookdev::dyn_light`]).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> dyn_light --out <dir> [--maps europa,mars]
//!     [--objects grey,charcoal,hull] [--distances 1,3,5] [--spp 256] [--bounces 6]
//!     [--size 960x540] [--frames 14] [--tag <label>]
//! ```
//!
//! Per map, object and distance: the real-time frame and the path-traced reference at the same
//! pose, the per-side irradiance table (`<dir>/dyn_light.csv`, `<dir>/summary.md`), previews
//! (`<map>_<object>_<d>m_{ldr,rt,pt}.png`, the HDR pair at one exposure) and a sheet per map
//! (`<map>_sheet.png`: rows = objects, columns = distances, each cell real-time | traced).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::dyn_light::{self, DynObject, MeasureInput, Side, SideStats, lum};
use lookdev::look_r2::{
    CameraOverride, HeroInsert, R2Options, R2Planets, R2Shot, ShotHook, render_shot_with,
};
use lookdev::pt_plates::{PlateTrace, trace_capture};
use lookdev::{LookdevError, Planet};
use regia_core::glam::Vec3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dyn_light: {err}");
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

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// What the capture hook keeps: the real-time HDR (full, direct-only) and the traced image.
type Captured = (Vec<[f32; 3]>, Vec<[f32; 3]>, regia_pt::PtImage);

/// One measured frame.
struct Row {
    map: &'static str,
    object: DynObject,
    distance: f32,
    stats: [SideStats; 6],
    /// Mean display luma (0–255) of the object's pixels in the final frame.
    display_luma: f32,
    /// Display luma of the sunlit ground next to it.
    ground_luma: f32,
}

/// Half-size box downsample of an RGBA8 image.
fn half(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (hw, hh) = (w / 2, h / 2);
    let mut out = vec![0u8; (hw * hh * 4) as usize];
    for y in 0..hh {
        for x in 0..hw {
            for c in 0..4 {
                let mut s = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    s += u32::from(rgba[(((y * 2 + dy) * w + x * 2 + dx) * 4 + c) as usize]);
                }
                out[((y * hw + x) * 4 + c) as usize] = (s / 4) as u8;
            }
        }
    }
    out
}

/// Pastes `tile` (`tw`×`th`) into `sheet` (`sw` wide) at (`x`, `y`).
fn paste(sheet: &mut [u8], sw: u32, tile: &[u8], tw: u32, th: u32, x: u32, y: u32) {
    for row in 0..th {
        let src = (row * tw * 4) as usize;
        let dst = (((y + row) * sw + x) * 4) as usize;
        sheet[dst..dst + (tw * 4) as usize].copy_from_slice(&tile[src..src + (tw * 4) as usize]);
    }
}

/// `DYN_LIGHT_DUMP=1`: the measured buffers as little-endian f32 (width, height, then per pixel
/// rt full rgb, rt direct rgb, pt beauty rgb, pt direct rgb, pt albedo rgb, pt normal xyz,
/// pt depth, pt sun visibility) for offline per-pixel analysis.
fn dump_buffers(path: &Path, w: u32, h: u32, input: &MeasureInput<'_>) -> Result<(), LookdevError> {
    let n = input.pt_beauty.len();
    let mut data: Vec<f32> = Vec::with_capacity(2 + n * 20);
    data.extend([w as f32, h as f32]);
    for i in 0..n {
        for rgb in [
            input.rt_full[i],
            input.rt_direct[i],
            input.pt_beauty[i],
            input.pt_direct[i],
            input.pt_albedo[i],
            input.pt_normal[i],
        ] {
            data.extend(rgb);
        }
        data.extend([input.pt_depth[i], input.pt_shadow[i]]);
    }
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(path, bytes).map_err(io(path))
}

/// `--bench <hulls>`: the cost of the large-mesh proxies. Per map, the 3 m hull frame plus
/// `hulls − 1` more hulls on a ring around it (6–40 m out), rendered with GPU timing with the
/// proxies off and on (no trace); prints the per-pass medians of both.
fn bench(
    device: &regia_render::RenderDevice,
    planets: &mut R2Planets,
    maps: &[Planet],
    mut options: R2Options,
    hulls: usize,
) -> Result<(), LookdevError> {
    options.timing = true;
    options.frames = options.frames.max(48);
    for &planet in maps {
        let shot = match planet {
            Planet::Mars => R2Shot::SwitchbacksMid,
            Planet::Europa => R2Shot::BoreTwelveMid,
        };
        let (eye, target, meshes, occluders) = {
            let terrain = planets.get(planet, options.terrain_resolution)?;
            let (anchor, toward_sun) = match terrain.do27.as_deref() {
                Some(c) => (
                    c.cameras
                        .get(shot.name())
                        .map_or(Vec3::ZERO, |cam| Vec3::from_array(cam.target)),
                    Vec3::from_array(c.map.sun.toward),
                ),
                None => (Vec3::ZERO, Vec3::new(0.4, 0.6, 0.3).normalize()),
            };
            let spot = dyn_light::find_spot(terrain, anchor, toward_sun, 9.0);
            let (eye, target, base, yaw) =
                dyn_light::frame_layout(terrain, spot, toward_sun, DynObject::Hull, 3.0);
            let mut meshes = Vec::new();
            let mut occluders = Vec::new();
            for k in 0..hulls {
                let (b, y) = if k == 0 {
                    (base, yaw)
                } else {
                    let a = k as f32 * 2.399_963; // golden angle
                    let r = 6.0 + 34.0 * (k as f32 / hulls as f32).sqrt();
                    let p = base + Vec3::new(a.cos() * r, 0.0, a.sin() * r);
                    (Vec3::new(p.x, terrain.height_at(p.x, p.z) - 0.03, p.z), a * 1.7)
                };
                let (m, _) = DynObject::Hull.build(b, y);
                meshes.extend(m);
                occluders.extend(DynObject::Hull.occluder(b, y));
            }
            (eye, target, meshes, occluders)
        };
        let material = DynObject::Hull.build(Vec3::ZERO, 0.0).1;
        for on in [false, true] {
            let hero = HeroInsert {
                meshes: meshes.clone(),
                materials: vec![material.clone()],
                eye: eye.to_array(),
                target: target.to_array(),
                fov_y: 55f32.to_radians(),
                glass: Vec::new(),
                // DYN_LIGHT_BENCH_FAR=1: the same proxies 5 km away (the term's fixed cost).
                occluders: if !on {
                    Vec::new()
                } else if std::env::var_os("DYN_LIGHT_BENCH_FAR").is_some() {
                    occluders
                        .iter()
                        .map(|o| regia_render::MeshOccluder {
                            center: [o.center[0] + 5000.0, o.center[1], o.center[2]],
                            ..*o
                        })
                        .collect()
                } else {
                    occluders.clone()
                },
                lights: Vec::new(),
            };
            println!(
                "{} {}×{} {hulls} hulls, proxies {}:",
                lookdev::do27::map_name(planet),
                options.width,
                options.height,
                if on { "on" } else { "off" }
            );
            render_shot_with(
                device,
                planets,
                shot,
                options,
                CameraOverride::default(),
                Some(&hero),
                None,
            )?;
        }
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let out = PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(io(&out))?;
    let maps: Vec<Planet> = value(args, "--maps")
        .unwrap_or("europa,mars")
        .split(',')
        .map(|m| match m.trim() {
            "europa" | "bore_twelve" => Ok(Planet::Europa),
            "mars" | "switchbacks" => Ok(Planet::Mars),
            other => Err(LookdevError::Config(format!("unknown map {other}"))),
        })
        .collect::<Result<_, _>>()?;
    let objects: Vec<DynObject> = value(args, "--objects")
        .unwrap_or("gray,charcoal,hull")
        .split(',')
        .map(|o| match o.trim() {
            "grey" => Ok(DynObject::Grey),
            "charcoal" => Ok(DynObject::Charcoal),
            "hull" => Ok(DynObject::Hull),
            other => Err(LookdevError::Config(format!("unknown object {other}"))),
        })
        .collect::<Result<_, _>>()?;
    let distances: Vec<f32> = value(args, "--distances")
        .unwrap_or("1,3,5")
        .split(',')
        .map(|d| {
            d.trim()
                .parse()
                .map_err(|_| LookdevError::Config(format!("bad distance {d}")))
        })
        .collect::<Result<_, _>>()?;
    let mut options = R2Options::evidence();
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.width &= !1;
    options.height &= !1;
    if let Some(f) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        options.frames = f;
    }
    let trace = PlateTrace {
        spp: value(args, "--spp").and_then(|v| v.parse().ok()).unwrap_or(256),
        max_bounces: value(args, "--bounces").and_then(|v| v.parse().ok()).unwrap_or(6),
        seed: value(args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(1),
    };
    let tag = value(args, "--tag").unwrap_or("run").to_string();
    let content = lookdev::do27::content_dir().is_some();
    let device = if content {
        regia_terrain::gpu::headless_bcn_device()?
    } else {
        regia_render::RenderDevice::headless()?
    };
    let info = device.adapter.get_info();
    println!(
        "dyn_light [{tag}]: device {} ({:?}), DO27 content: {content}, {}×{}, {} spp",
        info.name, info.backend, options.width, options.height, trace.spp
    );
    let tracer = regia_pt::PathTracer::new(&device).map_err(|e| LookdevError::Config(e.to_string()))?;
    let mut planets = R2Planets::default();
    if let Some(hulls) = value(args, "--bench").and_then(|v| v.parse::<usize>().ok()) {
        return bench(&device, &mut planets, &maps, options, hulls.max(1));
    }
    let mut rows: Vec<Row> = Vec::new();
    let (w, h) = (options.width, options.height);
    for planet in maps {
        let shot = match planet {
            Planet::Mars => R2Shot::SwitchbacksMid,
            Planet::Europa => R2Shot::BoreTwelveMid,
        };
        let map_name = lookdev::do27::map_name(planet);
        let (spot, toward_sun) = {
            let terrain = planets.get(planet, options.terrain_resolution)?;
            let (anchor, toward_sun) = match terrain.do27.as_deref() {
                Some(c) => (
                    c.cameras
                        .get(shot.name())
                        .map_or(Vec3::ZERO, |cam| Vec3::from_array(cam.target)),
                    Vec3::from_array(c.map.sun.toward),
                ),
                None => (Vec3::ZERO, Vec3::new(0.4, 0.6, 0.3).normalize()),
            };
            (dyn_light::find_spot(terrain, anchor, toward_sun, 9.0), toward_sun)
        };
        println!(
            "{map_name}: spot {:?}, sun toward {:?} (elevation {:.1}°)",
            spot.to_array(),
            toward_sun.to_array(),
            toward_sun.y.asin().to_degrees()
        );
        {
            // The sky irradiance map's integral per face direction (what meshes get as IBL).
            let terrain = planets.get(planet, options.terrain_resolution)?;
            let level = terrain.do27.as_deref().map(|c| c.map.level.atmosphere.clone());
            let mut params = regia_render::AtmosphereParameters::from_preset(
                level
                    .as_ref()
                    .map_or(regia_scene::level::AtmospherePreset::Mars, |l| l.preset),
            );
            if let Some(lux) = level.as_ref().and_then(|l| l.sun_illuminance_lux) {
                params.sun_illuminance = lux;
            }
            let sun_h = Vec3::new(toward_sun.x, 0.0, toward_sun.z).normalize_or_zero();
            // The horizon the bounce grid hands the forward pass at the spot, and the forward
            // pass's sky visibility for the probe normals (compare with the probe's occluded sky).
            let mips = regia_terrain::HeightMips::build(&terrain.map);
            let (factor, tan_h) = regia_terrain::bounce::bounce_and_horizon(
                &terrain.map,
                &mips,
                spot.to_array(),
                toward_sun.to_array(),
                0.0,
            );
            println!("  bounce grid at the spot: factor {factor:.3}, horizon tan {tan_h:?}");
            for (name, n) in [("up", Vec3::Y), ("toward sun", sun_h), ("away from sun", -sun_h)] {
                let [removed, added] =
                    dyn_light::shader_sky_occlusion(&params, toward_sun, spot.y.max(1.0), tan_h, n);
                println!(
                    "  forward-pass horizon {name:13}: removes {removed:.0} lx sky, adds {added:.0} lx ground"
                );
            }
            for (name, n) in [
                ("up", Vec3::Y),
                ("toward sun", sun_h),
                ("away from sun", -sun_h),
                ("down", -Vec3::Y),
            ] {
                let [sky, ground] =
                    dyn_light::sky_irradiance_probe(&params, toward_sun, spot.y.max(1.0), n, None);
                let at = spot + Vec3::Y * 1.0;
                let hides = |d: Vec3| dyn_light::terrain_hides(terrain, at, d);
                let [sky_o, ground_o] =
                    dyn_light::sky_irradiance_probe(&params, toward_sun, spot.y.max(1.0), n, Some(&hides));
                println!(
                    "  IBL probe {name:13}: sky {sky:8.0} lx, ground {ground:8.0} lx | terrain-occluded sky {sky_o:8.0}, ground {ground_o:8.0}"
                );
            }
        }
        let tw = w / 2;
        let th = h / 2;
        let cols = distances.len() as u32 * 2;
        let mut sheet = vec![0u8; (tw * cols * th * objects.len() as u32 * 4) as usize];
        for (oi, &object) in objects.iter().enumerate() {
            for (di, &d) in distances.iter().enumerate() {
                let started = std::time::Instant::now();
                let (eye, target, base, yaw) = {
                    let terrain = planets.get(planet, options.terrain_resolution)?;
                    dyn_light::frame_layout(terrain, spot, toward_sun, object, d)
                };
                let (meshes, material) = object.build(base, yaw);
                let hero = HeroInsert {
                    meshes,
                    materials: vec![material],
                    eye: eye.to_array(),
                    target: target.to_array(),
                    fov_y: 55f32.to_radians(),
                    glass: Vec::new(),
                    occluders: object.occluder(base, yaw).into_iter().collect(),
                    lights: Vec::new(),
                };
                let label = format!("{map_name}_{}_{}m", object.name(), d);
                let mut captured: Option<Captured> = None;
                let mut hook = |capture: lookdev::look_r2::ShotCapture<'_>| -> Result<(), LookdevError> {
                    let img = trace_capture(&tracer, &capture, &label, trace)?;
                    captured = Some((capture.hdr_full.rgb.clone(), capture.hdr_direct.rgb.clone(), img));
                    Ok(())
                };
                let (mut ldr, _) = render_shot_with(
                    &device,
                    &mut planets,
                    shot,
                    options,
                    CameraOverride::default(),
                    Some(&hero),
                    Some(ShotHook {
                        albedo: regia_pt::terrain_bake::AlbedoBake::default(),
                        run: &mut hook,
                    }),
                )?;
                let (rt_full, rt_direct, img) =
                    captured.ok_or_else(|| LookdevError::Config("the capture hook did not run".into()))?;
                if rt_full.len() != img.beauty.len() {
                    return Err(LookdevError::Config("real-time HDR and PT sizes differ".into()));
                }
                let input = MeasureInput {
                    rt_full: &rt_full,
                    rt_direct: &rt_direct,
                    pt_beauty: &img.beauty,
                    pt_direct: &img.direct,
                    pt_albedo: &img.albedo,
                    pt_normal: &img.normal,
                    pt_depth: &img.depth,
                    pt_shadow: &img.shadow,
                };
                if std::env::var_os("DYN_LIGHT_DUMP").is_some() {
                    dump_buffers(&out.join(format!("{label}.f32")), w, h, &input)?;
                }
                let window = object.depth_window(d);
                let stats = dyn_light::measure(&input, object.albedo(), window, toward_sun);
                // The final frame is mirrored for DO27 content (handedness): measure it unmirrored.
                if content {
                    lookdev::do27::mirror_columns(&mut ldr, w);
                }
                let mut obj_luma = (0.0f64, 0usize);
                let mut ground_luma = (0.0f64, 0usize);
                for i in 0..img.beauty.len() {
                    let dpt = img.depth[i];
                    let a = img.albedo[i];
                    let y = f64::from(lookdev::look_r2::luma(&ldr[i * 4..i * 4 + 4]));
                    if dpt >= window[0]
                        && dpt <= window[1]
                        && a.iter().all(|c| (c - object.albedo()).abs() < 0.006)
                    {
                        obj_luma.0 += y;
                        obj_luma.1 += 1;
                    } else if img.normal[i][1] > 0.85
                        && img.shadow[i] > 0.98
                        && dpt >= window[0] - 1.0
                        && dpt <= window[1] + 3.0
                    {
                        ground_luma.0 += y;
                        ground_luma.1 += 1;
                    }
                }
                // Previews: the final frame, and the HDR pair at the traced frame's metered exposure.
                // Metered on the ground and the object (the black airless sky would overexpose it).
                let log_avg = {
                    let (mut s, mut n) = (0.0f64, 0usize);
                    for (c, d) in img.beauty.iter().zip(&img.depth) {
                        if *d > 0.0 {
                            s += f64::from((0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]).max(1e-4)).ln();
                            n += 1;
                        }
                    }
                    (s / n.max(1) as f64).exp() as f32
                };
                let ev = regia_render::ev100_from_luminance(log_avg);
                let mut rt_prev = regia_pt::output::preview_rgba8(&rt_full, ev);
                let mut pt_prev = regia_pt::output::preview_rgba8(&img.beauty, ev);
                if content {
                    lookdev::do27::mirror_columns(&mut ldr, w);
                    lookdev::do27::mirror_columns(&mut rt_prev, w);
                    lookdev::do27::mirror_columns(&mut pt_prev, w);
                }
                for (suffix, px) in [("ldr", &ldr), ("rt", &rt_prev), ("pt", &pt_prev)] {
                    let path = out.join(format!("{label}_{suffix}.png"));
                    regia_pt::output::write_png(&path, w, h, px)
                        .map_err(|e| LookdevError::Config(e.to_string()))?;
                }
                paste(
                    &mut sheet,
                    tw * cols,
                    &half(&rt_prev, w, h),
                    tw,
                    th,
                    (di as u32 * 2) * tw,
                    oi as u32 * th,
                );
                paste(
                    &mut sheet,
                    tw * cols,
                    &half(&pt_prev, w, h),
                    tw,
                    th,
                    (di as u32 * 2 + 1) * tw,
                    oi as u32 * th,
                );
                let row = Row {
                    map: map_name,
                    object,
                    distance: d,
                    stats,
                    display_luma: (obj_luma.0 / obj_luma.1.max(1) as f64) as f32,
                    ground_luma: (ground_luma.0 / ground_luma.1.max(1) as f64) as f32,
                };
                print_row(&row);
                println!(
                    "  ({:.0} s, preview EV100 {ev:.2})",
                    started.elapsed().as_secs_f32()
                );
                rows.push(row);
            }
        }
        let path = out.join(format!("{map_name}_sheet.png"));
        regia_pt::output::write_png(&path, tw * cols, th * objects.len() as u32, &sheet)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
    }
    let csv = out.join("dyn_light.csv");
    std::fs::write(&csv, to_csv(&rows, &tag)).map_err(io(&csv))?;
    let md = out.join("summary.md");
    std::fs::write(&md, to_markdown(&rows, &tag)).map_err(io(&md))?;
    println!("wrote {} and {}", csv.display(), md.display());
    Ok(())
}

fn chroma(c: [f64; 3]) -> [f64; 3] {
    let m = (c[0] + c[1] + c[2]) / 3.0;
    if m > 1e-9 { c.map(|v| v / m) } else { [0.0; 3] }
}

fn print_row(row: &Row) {
    println!(
        "{} {} {} m: display luma object {:.1}, sunlit ground {:.1}",
        row.map,
        row.object.name(),
        row.distance,
        row.display_luma,
        row.ground_luma
    );
    for (side, s) in Side::ALL.iter().zip(&row.stats) {
        if s.pixels == 0 {
            continue;
        }
        let (rt, pt) = (lum(s.rt_mean()), lum(s.pt_mean()));
        let (rti, pti) = (lum(s.rt_indirect_mean()), lum(s.pt_indirect_mean()));
        println!(
            "  {:13} {:6} px  E rt {:9.0} pt {:9.0} lx (rt/pt {:.2})  indirect rt {:8.0} pt {:8.0} (rt/pt {:.2})",
            side.name(),
            s.pixels,
            rt,
            pt,
            rt / pt.max(1e-9),
            rti,
            pti,
            rti / pti.max(1e-9)
        );
    }
}

fn to_csv(rows: &[Row], tag: &str) -> String {
    let mut s = String::from(
        "tag,map,object,distance_m,side,pixels,rt_lux,pt_lux,rt_over_pt,rt_indirect_lux,pt_indirect_lux,indirect_rt_over_pt,rt_r,rt_g,rt_b,pt_r,pt_g,pt_b,display_luma_object,display_luma_ground\n",
    );
    for row in rows {
        for (side, st) in Side::ALL.iter().zip(&row.stats) {
            if st.pixels == 0 {
                continue;
            }
            let (rt, pt) = (lum(st.rt_mean()), lum(st.pt_mean()));
            let (rti, pti) = (lum(st.rt_indirect_mean()), lum(st.pt_indirect_mean()));
            let rc = chroma(st.rt_mean());
            let pc = chroma(st.pt_mean());
            let _ = writeln!(
                s,
                "{tag},{},{},{},{},{},{rt:.1},{pt:.1},{:.3},{rti:.1},{pti:.1},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.1},{:.1}",
                row.map,
                row.object.name(),
                row.distance,
                side.name(),
                st.pixels,
                rt / pt.max(1e-9),
                rti / pti.max(1e-9),
                rc[0],
                rc[1],
                rc[2],
                pc[0],
                pc[1],
                pc[2],
                row.display_luma,
                row.ground_luma
            );
        }
    }
    s
}

fn to_markdown(rows: &[Row], tag: &str) -> String {
    let mut s = format!(
        "# dyn_light [{tag}]\n\nIrradiance per side, E = π·L/ρ in lux (Rec.709 luminance), real-time (rt) vs path-traced (pt) at the same pose. \
         `ind` = indirect only (rt: full − direct-only frame; pt: beauty − direct). Display luma: mean 0–255 of the object in the final frame.\n\n\
         | map | object | d | display obj / ground | side | px | rt E | pt E | rt/pt | rt ind | pt ind | ind rt/pt | rt rgb | pt rgb |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for row in rows {
        for (side, st) in Side::ALL.iter().zip(&row.stats) {
            if st.pixels == 0 {
                continue;
            }
            let (rt, pt) = (lum(st.rt_mean()), lum(st.pt_mean()));
            let (rti, pti) = (lum(st.rt_indirect_mean()), lum(st.pt_indirect_mean()));
            let rc = chroma(st.rt_mean());
            let pc = chroma(st.pt_mean());
            let _ = writeln!(
                s,
                "| {} | {} | {} m | {:.0} / {:.0} | {} | {} | {rt:.0} | {pt:.0} | {:.2} | {rti:.0} | {pti:.0} | {:.2} | {:.2} {:.2} {:.2} | {:.2} {:.2} {:.2} |",
                row.map,
                row.object.name(),
                row.distance,
                row.display_luma,
                row.ground_luma,
                side.name(),
                st.pixels,
                rt / pt.max(1e-9),
                rti / pti.max(1e-9),
                rc[0],
                rc[1],
                rc[2],
                pc[0],
                pc[1],
                pc[2],
            );
        }
    }
    s
}
