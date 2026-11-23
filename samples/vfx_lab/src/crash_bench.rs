// SPDX-License-Identifier: MIT-0
//! `vfx_lab --crash-bench [file] [quick]` and `vfx_lab --crash-sheets <dir> [width height]`: the
//! crash effects' GPU cost and evidence frames (`regia_vfx::crash`, ADR-1398, ADR-1414).
//!
//! One crash (fireball, snow blast front and surge, ejecta, mushroom column, ground fires) at the
//! origin in a 3 m/s wind, on the harness ground (a 240 m patch: beyond it the layers draw over
//! the sky), at the showcase's 0.25 kt and at the top of the scale range, 1 kt. The bench times the VFX compute and draw passes with
//! GPU timestamps in four windows: the fireball (0.8–1.6 s), the surge and ejecta (4–5 s), the
//! column at 60 s (59–60 s) and the column at 3 minutes (179–180 s; full runs only), for both
//! tiers, at 2560×1440 and 3840×2160, from shot 4's camera (3.3 km, the showcase's distance
//! test) and from 1.2 km. Between windows the simulation advances at 64×36 so a software adapter
//! finishes. Without timestamps (some software adapters) it reports wall time per frame
//! instead, marked `wall`.

use std::path::Path;
use std::time::Instant;

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::VfxCamera;
use regia_vfx::crash::{CrashBurst, CrashScale, CrashTier, GroundClass, ensure_crash_effects, spawn_crash};
use regia_vfx::gpu::{VfxError, VfxProfile};
use regia_vfx::lights::VfxFrameLights;
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::timing::GpuTimer;

/// Simulation step, seconds.
const DT: f32 = 1.0 / 60.0;

/// Shot 4's camera relative to the impact site (PLAN.md test site: camera (-1752, 353.2,
/// -2203), site (700, 105.7, 5), +Z north): 3.3 km to the south-west, 248 m above it.
pub const SHOT4_OFFSET: Vec3 = Vec3::new(-2452.0, 247.5, -2208.0);

/// The scale the sheets film and the bench measures from both views (the showcase's 0.25 kt).
pub const BENCH_SCALE: CrashScale = CrashScale::SHOWCASE;

/// The largest scale (1 kt), measured from shot 4's camera only.
pub const LARGEST_SCALE: CrashScale = CrashScale::LARGEST;

/// One measured window.
#[derive(Debug, Clone)]
pub struct CrashCostRow {
    /// Tier.
    pub tier: CrashTier,
    /// Crash scale.
    pub scale: CrashScale,
    /// View label (`shot4` 3.3 km, `near` 1.2 km).
    pub view: &'static str,
    /// Window label.
    pub phase: &'static str,
    /// Frame size.
    pub size: (u32, u32),
    /// Most live particles in the window.
    pub alive_max: u32,
    /// Mean VFX compute milliseconds.
    pub sim_mean_ms: f32,
    /// Mean VFX draw milliseconds.
    pub draw_mean_ms: f32,
    /// Worst frame (compute + draw) milliseconds.
    pub total_max_ms: f32,
    /// `gpu` (timestamps) or `wall` (CPU wall time of the whole frame).
    pub clock: &'static str,
}

impl CrashCostRow {
    /// One report line.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{} {} {} {:<10} {}x{}: {} alive max, sim {:.3} ms, draw {:.3} ms, total {:.3} ms mean, {:.3} ms worst ({})",
            self.tier.name(),
            self.scale.tag(),
            self.view,
            self.phase,
            self.size.0,
            self.size.1,
            self.alive_max,
            self.sim_mean_ms,
            self.draw_mean_ms,
            self.sim_mean_ms + self.draw_mean_ms,
            self.total_max_ms,
            self.clock,
        )
    }
}

/// The VFX profile a tier is measured on (Desktop High for full, Mobile High for lite).
#[must_use]
pub fn tier_profile(tier: CrashTier) -> VfxProfile {
    match tier {
        CrashTier::Full => VfxProfile::desktop_high(),
        CrashTier::Lite => VfxProfile::mobile_high(),
    }
}

/// A scene with one crash of `scale` fired at the origin.
fn crash_scene(tier: CrashTier, scale: CrashScale) -> Result<VfxScene, VfxError> {
    let mut scene = VfxScene::new(VfxLibrary::new());
    ensure_crash_effects(&mut scene, scale, tier, Some(Vec3::new(3.0, 0.0, 0.0)));
    let fired = spawn_crash(
        &mut scene,
        &CrashBurst {
            ground: GroundClass::Snow,
            scale,
            ..CrashBurst::default()
        },
    );
    if fired.len() < 5 {
        return Err(VfxError::Shader(format!("crash spawned {} layers", fired.len())));
    }
    Ok(scene)
}

/// The camera of `view`: shot 4's direction at 3.3 km (`shot4`) or 1.2 km (`near`), looking at
/// a point 450 m above the crash so the ground, the fireball and the cap are in frame.
#[must_use]
pub fn camera(view: &str) -> VfxCamera {
    let eye = if view == "near" {
        SHOT4_OFFSET * (1_200.0 / SHOT4_OFFSET.length())
    } else {
        SHOT4_OFFSET
    };
    VfxCamera {
        eye,
        target: Vec3::new(0.0, 450.0, 0.0),
        fov_y: (if view == "near" { 60.0f32 } else { 26.0 }).to_radians(),
        near: 1.0,
        far: 20_000.0,
    }
}

/// Sets the harness point light to the scene's strongest particle light (the flash, then the
/// fireball, then a fire).
fn light_harness(scene: &VfxScene, harness: &mut VfxHarness) {
    let mut lights = VfxFrameLights::default();
    scene.gather_lights(&mut lights, 4);
    match lights.lights.first() {
        Some(light) => harness.set_point_light(light.position, light.color, light.radius),
        None => harness.set_point_light(Vec3::ZERO, [0.0; 3], 0.0),
    }
}

/// Measures one tier and view at `size` (see the module docs). `quick` measures 6 frames at the
/// start of each window instead of the whole window (a software adapter) and skips the
/// 3-minute window.
///
/// # Errors
///
/// Returns [`VfxError`] when the GPU setup or a frame fails.
pub fn run_crash_cost(
    device: &RenderDevice,
    tier: CrashTier,
    scale: CrashScale,
    view: &'static str,
    size: (u32, u32),
    quick: bool,
) -> Result<Vec<CrashCostRow>, VfxError> {
    let cam = camera(view);
    let profile = tier_profile(tier);
    let mut scene = crash_scene(tier, scale)?;
    let mut harness = VfxHarness::new(device, profile)?;
    let timer = GpuTimer::new(&device.device, 4);
    let all: [(&'static str, f32, f32); 4] = [
        ("fireball", 0.8, 1.6),
        ("surge", 4.0, 5.0),
        ("column60", 59.0, 60.0),
        ("column180", 179.0, 180.0),
    ];
    let windows = if quick { &all[..3] } else { &all[..] };
    let mut rows = Vec::new();
    let mut t = 0.0f32;
    for &(phase, from, to) in windows {
        let to = if quick { from + 6.0 * DT } else { to };
        let (mut sims, mut draws, mut worst, mut alive) = (Vec::new(), Vec::new(), 0.0f32, 0u32);
        while t + 1e-4 < to {
            t += DT;
            scene.update(DT, &LodInput { camera_pos: cam.eye }, profile.particle_capacity);
            scene.upload(harness.gpu());
            light_harness(&scene, &mut harness);
            if t < from {
                harness.render(device, &cam, 64, 36, DT)?;
                continue;
            }
            let (sim, draw, counters) = match &timer {
                Some(timer) => {
                    let counters = harness.render_timed(device, &cam, size.0, size.1, DT, timer)?;
                    let ms = timer
                        .read_ms(&device.device, &device.queue)
                        .ok_or_else(|| VfxError::Shader("timestamp readback".into()))?;
                    (
                        ms.first().copied().unwrap_or(0.0),
                        ms.get(1).copied().unwrap_or(0.0),
                        counters,
                    )
                }
                None => {
                    let start = Instant::now();
                    let frame = harness.render(device, &cam, size.0, size.1, DT)?;
                    (0.0, start.elapsed().as_secs_f32() * 1_000.0, frame.counters)
                }
            };
            sims.push(sim);
            draws.push(draw);
            worst = worst.max(sim + draw);
            alive = alive.max(counters[3]);
        }
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
        rows.push(CrashCostRow {
            tier,
            scale,
            view,
            phase,
            size,
            alive_max: alive,
            sim_mean_ms: mean(&sims),
            draw_mean_ms: mean(&draws),
            total_max_ms: worst,
            clock: if timer.is_some() { "gpu" } else { "wall" },
        });
    }
    Ok(rows)
}

/// `--crash-bench [file] [quick]`: both tiers at 1440p and 4K: 0.25 kt from shot 4's camera
/// and from 1.2 km, 1 kt from shot 4's camera (`quick`: shot 4 only, 6 frames per window, no
/// 3-minute window). Returns the exit code.
pub fn run_crash_bench(out: Option<&Path>, quick: bool) -> i32 {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    let info = device.adapter.get_info();
    let mut lines = vec![
        format!("adapter: {} ({:?})", info.name, info.backend),
        "lines: tier, scale (TNT-equivalent), view, window".to_string(),
    ];
    let mut ok = true;
    for size in [(2560, 1440), (3840, 2160)] {
        for tier in CrashTier::ALL {
            let runs: &[(CrashScale, &'static str)] = if quick {
                &[(BENCH_SCALE, "shot4"), (LARGEST_SCALE, "shot4")]
            } else {
                &[
                    (BENCH_SCALE, "shot4"),
                    (BENCH_SCALE, "near"),
                    (LARGEST_SCALE, "shot4"),
                ]
            };
            for &(scale, view) in runs {
                match run_crash_cost(&device, tier, scale, view, size, quick) {
                    Ok(rows) => lines.extend(rows.iter().map(CrashCostRow::line)),
                    Err(err) => {
                        lines.push(format!(
                            "{} {} {view} {}x{}: FAILED {err}",
                            tier.name(),
                            scale.tag(),
                            size.0,
                            size.1
                        ));
                        ok = false;
                    }
                }
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if let Some(out) = out
        && let Err(err) = write_atomic(out, (lines.join("\n") + "\n").as_bytes())
    {
        eprintln!("writing {}: {err}", out.display());
        ok = false;
    }
    i32::from(!ok)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn png_bytes(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(pixels).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// The seconds the evidence sheet shows.
pub const SHEET_TIMES: [f32; 9] = [0.1, 0.6, 1.2, 3.0, 6.0, 15.0, 30.0, 60.0, 180.0];

/// `--crash-sheets <dir> [width height]`: for both tiers and both views (shot 4 at 3.3 km and
/// 1.2 km), one PNG per [`SHEET_TIMES`] entry plus a 3×3 contact sheet at a third of the size.
/// Returns the exit code.
pub fn run_crash_sheets(dir: &Path, size: (u32, u32)) -> i32 {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    let mut code = 0;
    for tier in CrashTier::ALL {
        for view in ["shot4", "near"] {
            if let Err(err) = sheets_for(&device, tier, view, dir, size) {
                eprintln!("{} {view} sheets: {err}", tier.name());
                code = 1;
            }
        }
    }
    code
}

/// Renders the crash of [`BENCH_SCALE`] on `tier` from `view` and returns the RGBA8 pixels of
/// the frame at each of `times` (seconds, ascending), at `size`. The frames between are drawn at
/// 64×36 so a software adapter finishes. The simulation steps a fixed `DT`, so two calls give
/// the same bytes (ADR-5991).
///
/// # Errors
///
/// A message when the GPU setup or a frame fails.
pub fn crash_sheet_frames(
    device: &RenderDevice,
    tier: CrashTier,
    view: &str,
    size: (u32, u32),
    times: &[f32],
) -> Result<Vec<Vec<u8>>, String> {
    let cam = camera(view);
    let profile = tier_profile(tier);
    let mut scene = crash_scene(tier, BENCH_SCALE).map_err(|e| e.to_string())?;
    let mut harness = VfxHarness::new(device, profile).map_err(|e| e.to_string())?;
    let mut frames = Vec::with_capacity(times.len());
    let mut t = 0.0f32;
    for &want in times {
        let mut frame = None;
        while t + 1e-4 < want {
            t += DT;
            scene.update(DT, &LodInput { camera_pos: cam.eye }, profile.particle_capacity);
            scene.upload(harness.gpu());
            light_harness(&scene, &mut harness);
            let (w, h) = if t + 1e-4 >= want { size } else { (64, 36) };
            let f = harness
                .render(device, &cam, w, h, DT)
                .map_err(|e| e.to_string())?;
            if (w, h) == size {
                frame = Some(f);
            }
        }
        frames.push(frame.ok_or("no frame")?.pixels);
    }
    Ok(frames)
}

fn sheets_for(
    device: &RenderDevice,
    tier: CrashTier,
    view: &str,
    dir: &Path,
    size: (u32, u32),
) -> Result<(), String> {
    let (cw, ch) = (size.0 / 3, size.1 / 3);
    let mut contact = vec![0u8; (cw * 3 * ch * 3 * 4) as usize];
    let frames = crash_sheet_frames(device, tier, view, size, &SHEET_TIMES)?;
    for (i, (want, pixels)) in SHEET_TIMES.into_iter().zip(frames).enumerate() {
        let name = format!("crash_{}_{view}_{:06.2}s.png", tier.name(), want);
        write_atomic(&dir.join(&name), &png_bytes(&pixels, size.0, size.1)?).map_err(|e| e.to_string())?;
        // Nearest-neighbour third-size tile into the contact sheet.
        let (ox, oy) = ((i as u32 % 3) * cw, (i as u32 / 3) * ch);
        for y in 0..ch {
            for x in 0..cw {
                let src = (((y * 3) * size.0 + x * 3) * 4) as usize;
                let dst = (((oy + y) * cw * 3 + ox + x) * 4) as usize;
                if let (Some(s), Some(d)) = (pixels.get(src..src + 4), contact.get_mut(dst..dst + 4)) {
                    d.copy_from_slice(s);
                }
            }
        }
        println!("{} {view} {want:6.2} s: {name}", tier.name());
    }
    let sheet = format!("crash_{}_{view}_contact.png", tier.name());
    write_atomic(&dir.join(&sheet), &png_bytes(&contact, cw * 3, ch * 3)?).map_err(|e| e.to_string())?;
    println!("{} {view}: {sheet}", tier.name());
    Ok(())
}
