// SPDX-License-Identifier: MIT-0
//! `shots.json` and the text diff `lookdev --compare` prints.
//!
//! Measurements are taken on the full sRGB framebuffer. The PNG next to the report is a
//! box-downscale of the last frame, for the evidence directory.

use std::collections::BTreeMap;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use regia_render::RenderDevice;
use serde::{Deserialize, Serialize};

use crate::features::EngineFeatures;
use crate::measure::{self, FrameStats, PassMillis};
use crate::render::TerrainSession;
use crate::scene::generate;
use crate::shots::{ShotDesc, all_shots};
use crate::{LookdevConfig, LookdevError, Planet, ensure_dir, io_err};

/// `shots.json` schema version.
pub const REPORT_VERSION: u32 = 1;

/// One rendered shot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotRecord {
    /// `europa/close_1m/ultra`.
    pub id: String,
    /// Planet name.
    pub planet: String,
    /// Camera name.
    pub camera: String,
    /// Profile name.
    pub profile: String,
    /// Framebuffer width the measurements were taken on.
    pub width: u32,
    /// Framebuffer height.
    pub height: u32,
    /// Preview file, relative to the report directory.
    pub png: String,
    /// Preview width after the box downsample.
    pub png_width: u32,
    /// Preview height.
    pub png_height: u32,
    /// Mean BT.709 luminance of the last frame, 0..=255.
    pub mean_luminance: f32,
    /// Standard deviation of luminance.
    pub contrast: f32,
    /// Percent of pixels with luminance below 8.
    pub near_black_pct: f32,
    /// Percent of pixels with luminance above 250.
    pub blown_pct: f32,
    /// Mean channel spread, 0..=1.
    pub saturation: f32,
    /// Strongest secondary autocorrelation peak after the lag-0 lobe, 0..=1.
    /// A smooth gradient is 0.
    pub tiling_peak: f32,
    /// RMS 4-neighbour Laplacian divided by 255.
    pub sharpness: f32,
    /// `1` when consecutive frames match. `0` when every pixel jumps by 255.
    /// A static camera on this harness sits at 1; that is not proof the frame is lit.
    pub temporal_stability: f32,
    /// Mean BT.709 luminance of the bottom 45% of the last frame, 0..=255.
    /// Horizon cameras put the ground in that band.
    #[serde(default)]
    pub ground_luminance: f32,
    /// Mean GPU milliseconds over the shot's frames.
    pub gpu_ms: PassMillis,
}

/// The file `lookdev --shots` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotReport {
    /// [`REPORT_VERSION`].
    pub version: u32,
    /// Adapter the frames were drawn on.
    pub adapter: String,
    /// Per-pass GPU times are real (the device exposed timestamps and every read succeeded).
    pub gpu_timing: bool,
    /// The config that built the scenes.
    pub config: LookdevConfig,
    /// What the linked engine can shade, and which in-flight lanes were detected.
    pub features: EngineFeatures,
    /// One record per camera and profile.
    pub shots: Vec<ShotRecord>,
}

/// What the `lookdev` binary was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Print usage and exit 0.
    Help,
    /// Render the shot list into a directory.
    Shots {
        /// Output directory (`shots.json` and the PNGs).
        dir: PathBuf,
        /// Scene and framebuffer config.
        config: LookdevConfig,
    },
    /// Print metric deltas between two reports.
    Compare {
        /// Previous `shots.json` or its directory.
        old: PathBuf,
        /// New `shots.json` or its directory.
        new: PathBuf,
    },
    /// Render the hard-surface hull close-up to one PNG.
    Hull {
        /// Output PNG path.
        path: PathBuf,
    },
    /// Time each hard-surface feature on and off and write a markdown table.
    HullCost {
        /// Output markdown path.
        path: PathBuf,
    },
}

/// Parses argv after the program name.
///
/// # Errors
///
/// [`LookdevError::Config`] for an unknown flag, a missing value, or a config `validate` rejects.
pub fn parse_args(args: &[String]) -> Result<Command, LookdevError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(Command::Help);
    }
    let mut shots: Option<PathBuf> = None;
    let mut compare: Option<(PathBuf, PathBuf)> = None;
    let mut hull: Option<PathBuf> = None;
    let mut hull_cost: Option<PathBuf> = None;
    let mut config = LookdevConfig::baseline();
    let mut planets: Vec<Planet> = Vec::new();
    let mut saw_size = false;
    let mut saw_planet = false;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--shots" => {
                shots = Some(PathBuf::from(take_value(args, &mut index, flag)?));
            }
            "--compare" => {
                let old = PathBuf::from(take_value(args, &mut index, flag)?);
                let new = PathBuf::from(take_value(args, &mut index, flag)?);
                compare = Some((old, new));
            }
            "--hull" => {
                hull = Some(PathBuf::from(take_value(args, &mut index, flag)?));
            }
            "--hull-cost" => {
                hull_cost = Some(PathBuf::from(take_value(args, &mut index, flag)?));
            }
            "--width" => {
                config.width = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--height" => {
                config.height = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--heightfield" => {
                config.height_resolution = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--macro" => {
                config.macro_resolution = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--layer-height" => {
                config.layer_height_resolution = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--frames" => {
                config.frames = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--png-width" => {
                config.png_width = take_u32(args, &mut index, flag)?;
                saw_size = true;
            }
            "--scatter" => {
                config.scatter_cap = usize::try_from(take_u32(args, &mut index, flag)?).unwrap_or(usize::MAX);
                saw_size = true;
            }
            "--planet" => {
                let name = take_value(args, &mut index, flag)?;
                let planet = match name.as_str() {
                    "europa" => Planet::Europa,
                    "mars" => Planet::Mars,
                    other => {
                        return Err(LookdevError::Config(format!(
                            "unknown planet {other} (expected europa or mars)"
                        )));
                    }
                };
                if !planets.contains(&planet) {
                    planets.push(planet);
                }
                saw_planet = true;
            }
            other => return Err(LookdevError::Config(format!("unknown argument {other}"))),
        }
        index += 1;
    }
    if !planets.is_empty() {
        config.planets = planets;
    }
    let modes = [
        shots.is_some(),
        compare.is_some(),
        hull.is_some(),
        hull_cost.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    if modes > 1 {
        return Err(LookdevError::Config(
            "pass only one of --shots, --compare, --hull, --hull-cost".into(),
        ));
    }
    if shots.is_none() && (saw_size || saw_planet) {
        return Err(LookdevError::Config(
            "size and planet flags apply to --shots".into(),
        ));
    }
    if let Some(dir) = shots {
        config.validate()?;
        return Ok(Command::Shots { dir, config });
    }
    if let Some((old, new)) = compare {
        return Ok(Command::Compare { old, new });
    }
    if let Some(path) = hull {
        return Ok(Command::Hull { path });
    }
    if let Some(path) = hull_cost {
        return Ok(Command::HullCost { path });
    }
    Err(LookdevError::Config(
        "pass --shots <dir>, --compare <old> <new>, --hull <png>, or --hull-cost <md>".into(),
    ))
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, LookdevError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| LookdevError::Config(format!("{flag} needs a value")))
}

fn take_u32(args: &[String], index: &mut usize, flag: &str) -> Result<u32, LookdevError> {
    let raw = take_value(args, index, flag)?;
    raw.parse::<u32>()
        .map_err(|_| LookdevError::Config(format!("{flag} needs an integer, got {raw}")))
}

/// Usage text for stdout.
#[must_use]
pub fn help_text() -> &'static str {
    "\
lookdev — photoreal shot list for the Europa and Mars stand-ins

Usage:
  lookdev --shots <dir> [options]
  lookdev --compare <old.json|dir> <new.json|dir>
  lookdev --hull <png>
  lookdev --hull-cost <markdown>
  lookdev --help

Options (with --shots; defaults are the evidence baseline):
  --width <px>           Desktop framebuffer width (1920)
  --height <px>          Desktop framebuffer height (1080)
  --heightfield <n>      Heightfield edge (4096)
  --macro <n>            Macro-variation edge (2048; 8192 is the 8K target)
  --layer-height <n>     Per-layer height-map edge (512)
  --frames <n>           Frames per shot, including the temporal pair (30)
  --png-width <px>       Preview width (960). Narrower frames stay native
  --scatter <n>          Rock cap before the per-profile scale (700)
  --planet europa|mars   Restrict the list (repeatable; default is both)

Profiles: ultra (Desktop Ultra), high (Desktop High), deck-medium (Deck).
Cameras: close_1m, mid_50m, far_2km, low_sun, noon, tunnel_mouth, road_bend, sky_heavy.
"
}

/// Renders every shot in `config` and writes `dir/shots.json` plus one PNG per shot.
///
/// `progress` runs once per finished shot. The library also logs the shot at info.
///
/// # Errors
///
/// Config, terrain, render, or filesystem failures.
pub fn render_shot_list(
    device: &RenderDevice,
    config: &LookdevConfig,
    dir: &Path,
    mut progress: impl FnMut(&ShotRecord),
) -> Result<ShotReport, LookdevError> {
    config.validate()?;
    ensure_dir(dir)?;
    let info = device.adapter.get_info();
    let adapter = format!("{} ({}, {})", info.name, info.driver, info.backend);
    let can_time = device.device.features().contains(wgpu::Features::TIMESTAMP_QUERY)
        && device
            .device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    let mut features = EngineFeatures::detect(can_time);
    let mut gpu_timing = can_time;
    let mut records = Vec::new();
    let shots = all_shots(&config.planets);
    for planet in &config.planets {
        let scene = generate(*planet, config)?;
        let mut session = TerrainSession::new(device, &scene)?;
        features.uploaded_layer_height_maps = session.layer_height_bytes() > 0;
        features.uploaded_layer_height_bytes = session.layer_height_bytes();
        features.uploaded_macro_variation = session.macro_bytes() > 0;
        features.uploaded_macro_bytes = session.macro_bytes();
        if !session.gpu_timing() {
            gpu_timing = false;
        }
        tracing::info!(
            target: "lookdev",
            planet = planet.name(),
            gpu_timing = session.gpu_timing(),
            "stand-in ready"
        );
        for shot in shots.iter().copied().filter(|shot| shot.planet == *planet) {
            let record = render_shot(&mut session, device, &scene, shot, config, dir)?;
            if !session.gpu_timing() {
                gpu_timing = false;
            }
            tracing::info!(
                target: "lookdev",
                shot = %record.id,
                mean_luminance = record.mean_luminance,
                contrast = record.contrast,
                temporal_stability = record.temporal_stability,
                gpu_ms = record.gpu_ms.total,
                "shot"
            );
            progress(&record);
            records.push(record);
            let partial = ShotReport {
                version: REPORT_VERSION,
                adapter: adapter.clone(),
                gpu_timing,
                config: config.clone(),
                features: features.clone(),
                shots: records.clone(),
            };
            write_report(&dir.join("shots.json"), &partial)?;
        }
    }
    Ok(ShotReport {
        version: REPORT_VERSION,
        adapter,
        gpu_timing,
        config: config.clone(),
        features,
        shots: records,
    })
}

fn render_shot(
    session: &mut TerrainSession,
    device: &RenderDevice,
    scene: &crate::scene::Scene,
    shot: ShotDesc,
    config: &LookdevConfig,
    dir: &Path,
) -> Result<ShotRecord, LookdevError> {
    let (width, height) = crate::shots::framebuffer_size(config, shot.profile);
    let mut previous: Option<Vec<f32>> = None;
    let mut delta_sum = 0.0_f32;
    let mut pairs = 0_u32;
    let mut passes = Vec::with_capacity(config.frames as usize);
    let mut last_pixels = Vec::new();
    let mut last_stats = FrameStats {
        mean_luminance: 0.0,
        contrast: 0.0,
        near_black_pct: 0.0,
        blown_pct: 0.0,
        saturation: 0.0,
        tiling_peak: 0.0,
        sharpness: 0.0,
    };
    for frame in 0..config.frames {
        let capture = session.render_frame(device, scene, shot, frame, config)?;
        let luma = measure::luminance_plane(&capture.pixels, width, height);
        if let Some(prev) = &previous {
            delta_sum += measure::mean_abs_delta(prev, &luma);
            pairs += 1;
        }
        previous = Some(luma);
        last_stats = measure::frame_stats(&capture.pixels, width, height);
        passes.push(capture.passes);
        last_pixels = capture.pixels;
    }
    let temporal = if pairs == 0 {
        1.0
    } else {
        measure::stability_from_delta(delta_sum / pairs as f32)
    };
    let (png_pixels, png_width, png_height) =
        measure::downscale_to_width(&last_pixels, width, height, config.png_width);
    let png_name = format!("{}.png", shot.file_stem());
    write_png(&dir.join(&png_name), png_width, png_height, &png_pixels)?;
    Ok(ShotRecord {
        id: shot.id(),
        planet: shot.planet.name().to_string(),
        camera: shot.camera.name().to_string(),
        profile: shot.profile.name().to_string(),
        width,
        height,
        png: png_name,
        png_width,
        png_height,
        mean_luminance: last_stats.mean_luminance,
        contrast: last_stats.contrast,
        near_black_pct: last_stats.near_black_pct,
        blown_pct: last_stats.blown_pct,
        saturation: last_stats.saturation,
        tiling_peak: last_stats.tiling_peak,
        sharpness: last_stats.sharpness,
        temporal_stability: temporal,
        ground_luminance: measure::ground_luminance(&last_pixels, width, height),
        gpu_ms: PassMillis::mean(&passes),
    })
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), LookdevError> {
    let expected = (width as usize).saturating_mul(height as usize).saturating_mul(4);
    if pixels.len() != expected {
        return Err(LookdevError::Png(format!(
            "{} has {} bytes, expected {expected}",
            path.display(),
            pixels.len()
        )));
    }
    let file = std::fs::File::create(path).map_err(|source| io_err(path, source))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| LookdevError::Png(err.to_string()))?;
    writer
        .write_image_data(pixels)
        .map_err(|err| LookdevError::Png(err.to_string()))?;
    writer
        .finish()
        .map_err(|err| LookdevError::Png(err.to_string()))?;
    Ok(())
}

/// Renders the hard-surface hull close-up (detail, coat, anisotropy, occlusion,
/// multi-scatter, parallax, edge wear) and writes one PNG.
///
/// # Errors
///
/// Render or filesystem failures.
#[tracing::instrument(skip(device))]
pub fn render_hull(device: &RenderDevice, path: &Path) -> Result<(), LookdevError> {
    let scene = regia_render::hard_surface::hull_closeup_scene();
    let quality = regia_render::hard_surface::hull_closeup_quality();
    let mut renderer = regia_render::FrameRenderer::new();
    let frame = renderer.render_offscreen(device, &scene.as_frame_scene_with_quality(quality))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_dir(parent)?;
    }
    write_png(path, scene.width, scene.height, &frame.pixels)?;
    let luma = mean_luma(&frame.pixels);
    println!(
        "hull close-up {}x{} mean luma {luma:.1} -> {}",
        scene.width,
        scene.height,
        path.display()
    );
    Ok(())
}

/// Times one hard-surface feature on and off (12 hulls, 1280×720) and writes a markdown table.
///
/// Each side is rendered twice. The forward milliseconds are the second read: timestamp
/// queries land when the pixel read waits. The parallax profile gate is on for every pair;
/// a material with `pom_scale` 0 still returns the geometric UV.
///
/// # Errors
///
/// Render or filesystem failures.
#[tracing::instrument(skip(device))]
pub fn render_hull_cost(device: &RenderDevice, path: &Path) -> Result<(), LookdevError> {
    let info = device.adapter.get_info();
    let adapter = format!("{} ({}, {})", info.name, info.driver, info.backend);
    let mut renderer = regia_render::FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    let quality = regia_render::hard_surface::hull_closeup_quality();
    let mut body = format!(
        "# Hard-surface feature cost\n\n\
         Adapter: {adapter}\n\n\
         1280×720, 12 panelled hulls, one feature toggled. The number is \
         `GpuTimings::forward_ms` of the second offscreen read. The parallax \
         profile gate is on for every pair.\n\n\
         | feature | off ms | on ms | delta ms |\n\
         |---|---:|---:|---:|\n"
    );
    for feature in regia_render::hard_surface::HardSurfaceFeature::ALL {
        let off = timed_forward(&mut renderer, device, feature, false, quality)?;
        let on = timed_forward(&mut renderer, device, feature, true, quality)?;
        body.push_str(&format!(
            "| {} | {off:.3} | {on:.3} | {:.3} |\n",
            feature.name(),
            on - off
        ));
        println!("hull-cost {} off {off:.3} ms on {on:.3} ms", feature.name());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_dir(parent)?;
    }
    std::fs::write(path, body).map_err(|source| io_err(path, source))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn timed_forward(
    renderer: &mut regia_render::FrameRenderer,
    device: &RenderDevice,
    feature: regia_render::hard_surface::HardSurfaceFeature,
    enabled: bool,
    quality: regia_render::FrameQuality,
) -> Result<f32, LookdevError> {
    let scene = regia_render::hard_surface::feature_cost_scene(feature, enabled, 1280, 720);
    let frame = scene.as_frame_scene_with_quality(quality);
    let _ = renderer.render_offscreen(device, &frame)?;
    let _ = renderer.render_offscreen(device, &frame)?;
    Ok(renderer.gpu_timings().forward_ms)
}

fn mean_luma(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let px = i * 4;
        sum += 0.2126 * f64::from(pixels[px])
            + 0.7152 * f64::from(pixels[px + 1])
            + 0.0722 * f64::from(pixels[px + 2]);
    }
    sum / n as f64
}

/// Writes `report` as pretty JSON.
///
/// # Errors
///
/// Filesystem errors, or a measurement serde cannot encode.
pub fn write_report(path: &Path, report: &ShotReport) -> Result<(), LookdevError> {
    let json = serde_json::to_string_pretty(report).map_err(|err| LookdevError::Json(err.to_string()))?;
    std::fs::write(path, json).map_err(|source| io_err(path, source))?;
    Ok(())
}

/// Reads a report. A directory is treated as `dir/shots.json`.
///
/// # Errors
///
/// Filesystem errors, JSON errors, or a version other than [`REPORT_VERSION`].
pub fn read_report(path: &Path) -> Result<ShotReport, LookdevError> {
    let path = report_path(path);
    let text = std::fs::read_to_string(&path).map_err(|source| io_err(&path, source))?;
    let report: ShotReport =
        serde_json::from_str(&text).map_err(|err| LookdevError::Json(err.to_string()))?;
    if report.version != REPORT_VERSION {
        return Err(LookdevError::Json(format!(
            "unsupported version {} (this harness writes {REPORT_VERSION})",
            report.version
        )));
    }
    Ok(report)
}

fn report_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("shots.json")
    } else {
        path.to_path_buf()
    }
}

/// Text table of per-shot deltas (`new - old`) and any feature-flag changes.
#[must_use]
pub fn compare_reports(old: &ShotReport, new: &ShotReport) -> String {
    let mut out = String::new();
    push_line(&mut out, "lookdev compare");
    push_line(&mut out, &format!("old adapter: {}", old.adapter));
    push_line(&mut out, &format!("new adapter: {}", new.adapter));
    let feature_lines = feature_delta(&old.features, &new.features);
    if feature_lines.is_empty() {
        push_line(&mut out, "features: unchanged");
    } else {
        for line in feature_lines {
            push_line(&mut out, &line);
        }
    }
    let old_shots = index_shots(old);
    let new_shots = index_shots(new);
    let mut missing_new = Vec::new();
    let mut missing_old = Vec::new();
    push_line(
        &mut out,
        &format!(
            "{:<36} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            "shot",
            "d_lum",
            "d_con",
            "d_black",
            "d_blow",
            "d_sat",
            "d_tile",
            "d_sharp",
            "d_stab",
            "d_gnd",
            "d_gpu"
        ),
    );
    for (id, previous) in &old_shots {
        let Some(current) = new_shots.get(id) else {
            missing_new.push((*id).to_string());
            continue;
        };
        push_line(&mut out, &delta_line(id, previous, current));
    }
    for id in new_shots.keys() {
        if !old_shots.contains_key(id) {
            missing_old.push((*id).to_string());
        }
    }
    push_line(&mut out, &format!("missing_in_new: {}", missing_new.len()));
    for id in &missing_new {
        push_line(&mut out, &format!("  {id}"));
    }
    push_line(&mut out, &format!("missing_in_old: {}", missing_old.len()));
    for id in &missing_old {
        push_line(&mut out, &format!("  {id}"));
    }
    out
}

fn push_line(out: &mut String, text: &str) {
    out.push_str(text);
    out.push('\n');
}

fn index_shots(report: &ShotReport) -> BTreeMap<&str, &ShotRecord> {
    report.shots.iter().map(|shot| (shot.id.as_str(), shot)).collect()
}

fn delta_line(id: &str, old: &ShotRecord, new: &ShotRecord) -> String {
    format!(
        "{id:<36} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3} {:+8.3}",
        new.mean_luminance - old.mean_luminance,
        new.contrast - old.contrast,
        new.near_black_pct - old.near_black_pct,
        new.blown_pct - old.blown_pct,
        new.saturation - old.saturation,
        new.tiling_peak - old.tiling_peak,
        new.sharpness - old.sharpness,
        new.temporal_stability - old.temporal_stability,
        new.ground_luminance - old.ground_luminance,
        new.gpu_ms.total - old.gpu_ms.total,
    )
}

fn feature_delta(old: &EngineFeatures, new: &EngineFeatures) -> Vec<String> {
    let flags = [
        ("cdlod", old.cdlod, new.cdlod),
        (
            "authored_layer_height_maps",
            old.authored_layer_height_maps,
            new.authored_layer_height_maps,
        ),
        (
            "gpu_samples_layer_height_maps",
            old.gpu_samples_layer_height_maps,
            new.gpu_samples_layer_height_maps,
        ),
        (
            "authored_macro_variation",
            old.authored_macro_variation,
            new.authored_macro_variation,
        ),
        (
            "gpu_samples_macro_variation",
            old.gpu_samples_macro_variation,
            new.gpu_samples_macro_variation,
        ),
        (
            "ssr_in_linked_crates",
            old.ssr_in_linked_crates,
            new.ssr_in_linked_crates,
        ),
        (
            "gi_probes_in_linked_crates",
            old.gi_probes_in_linked_crates,
            new.gi_probes_in_linked_crates,
        ),
        (
            "volumetric_atmosphere_in_linked_crates",
            old.volumetric_atmosphere_in_linked_crates,
            new.volumetric_atmosphere_in_linked_crates,
        ),
        (
            "volumetric_clouds_in_linked_crates",
            old.volumetric_clouds_in_linked_crates,
            new.volumetric_clouds_in_linked_crates,
        ),
        (
            "terrain_streaming_in_linked_crates",
            old.terrain_streaming_in_linked_crates,
            new.terrain_streaming_in_linked_crates,
        ),
        ("timestamp_query", old.timestamp_query, new.timestamp_query),
        (
            "uploaded_layer_height_maps",
            old.uploaded_layer_height_maps,
            new.uploaded_layer_height_maps,
        ),
        (
            "uploaded_macro_variation",
            old.uploaded_macro_variation,
            new.uploaded_macro_variation,
        ),
    ];
    let mut lines: Vec<String> = flags
        .into_iter()
        .filter(|(_, before, after)| before != after)
        .map(|(name, before, after)| format!("feature {name}: {before} -> {after}"))
        .collect();
    let old_profiles = profile_summary(old);
    let new_profiles = profile_summary(new);
    if old_profiles != new_profiles {
        lines.push(format!("feature profiles: {old_profiles} -> {new_profiles}"));
    }
    for (name, before, after) in [
        (
            "uploaded_layer_height_bytes",
            old.uploaded_layer_height_bytes,
            new.uploaded_layer_height_bytes,
        ),
        (
            "uploaded_macro_bytes",
            old.uploaded_macro_bytes,
            new.uploaded_macro_bytes,
        ),
    ] {
        if before != after {
            lines.push(format!("feature {name}: {before} -> {after}"));
        }
    }
    lines
}

fn profile_summary(features: &EngineFeatures) -> String {
    features
        .profiles
        .iter()
        .map(|profile| {
            format!(
                "{} requested {} cascades, applied {}",
                profile.profile, profile.csm_cascades, profile.applied_terrain_cascades
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::EngineFeatures;
    use crate::shots::ProfileKind;

    fn sample_record(id: &str, luminance: f32) -> ShotRecord {
        ShotRecord {
            id: id.to_string(),
            planet: "europa".into(),
            camera: "close_1m".into(),
            profile: "ultra".into(),
            width: 64,
            height: 36,
            png: "europa_close_1m_ultra.png".into(),
            png_width: 64,
            png_height: 36,
            mean_luminance: luminance,
            contrast: 4.0,
            near_black_pct: 1.0,
            blown_pct: 0.0,
            saturation: 0.2,
            tiling_peak: 0.1,
            sharpness: 0.05,
            temporal_stability: 1.0,
            ground_luminance: 48.0,
            gpu_ms: PassMillis::from_passes(0.1, 0.1, 0.4, 0.2, 0.0),
        }
    }

    fn sample_report(luminance: f32) -> ShotReport {
        ShotReport {
            version: REPORT_VERSION,
            adapter: "test".into(),
            gpu_timing: false,
            config: LookdevConfig::tiny(),
            features: EngineFeatures::detect(false),
            shots: vec![sample_record("europa/close_1m/ultra", luminance)],
        }
    }

    #[test]
    fn compare_of_a_report_to_itself_is_zero() {
        let report = sample_report(40.0);
        let text = compare_reports(&report, &report);
        assert!(text.contains("features: unchanged"), "{text}");
        assert!(text.contains("missing_in_new: 0"), "{text}");
        assert!(text.contains("missing_in_old: 0"), "{text}");
        assert!(text.contains("+0.000"), "{text}");
        assert!(
            !text.contains("+1.")
                && !text
                    .lines()
                    .any(|line| line.contains("europa/") && !line.contains("+0.000"))
        );
    }

    #[test]
    fn compare_prints_a_luminance_delta_and_a_missing_shot() {
        let old = sample_report(40.0);
        let mut new = sample_report(43.5);
        new.shots[0].id = "europa/close_1m/ultra".into();
        new.shots.push(sample_record("mars/far_2km/high", 10.0));
        let text = compare_reports(&old, &new);
        assert!(text.contains("europa/close_1m/ultra"), "{text}");
        assert!(text.contains("+3.500"), "{text}");
        assert!(text.contains("missing_in_old: 1"), "{text}");
        assert!(text.contains("mars/far_2km/high"), "{text}");
    }

    #[test]
    fn feature_flag_change_is_a_line() {
        let mut old = sample_report(1.0);
        // `detect` is already true on this tree (M15b). Force the previous side off
        // so the line is the transition a merge would print.
        old.features.ssr_in_linked_crates = false;
        let mut new = sample_report(1.0);
        new.features.ssr_in_linked_crates = true;
        let text = compare_reports(&old, &new);
        assert!(
            text.contains("feature ssr_in_linked_crates: false -> true"),
            "{text}"
        );
    }

    #[test]
    fn shots_json_roundtrips() {
        let report = sample_report(12.5);
        let dir = std::env::temp_dir().join(format!("regia-lookdev-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("shots.json");
        write_report(&path, &report).expect("write");
        let read = read_report(&dir).expect("read directory");
        assert_eq!(read.version, REPORT_VERSION);
        assert_eq!(read.shots[0].mean_luminance, 12.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_shots_and_compare() {
        let shots = parse_args(&[
            "--shots".into(),
            "/tmp/out".into(),
            "--planet".into(),
            "mars".into(),
        ])
        .expect("shots");
        match shots {
            Command::Shots { dir, config } => {
                assert_eq!(dir, PathBuf::from("/tmp/out"));
                assert_eq!(config.planets, vec![Planet::Mars]);
                assert_eq!(config.height_resolution, LookdevConfig::HEIGHT_4K);
            }
            other => panic!("expected shots, got {other:?}"),
        }
        let compare = parse_args(&["--compare".into(), "old".into(), "new".into()]).expect("compare");
        assert!(matches!(compare, Command::Compare { .. }));
        let err = parse_args(&["--nope".into()]).expect_err("unknown");
        assert!(err.to_string().contains("unknown"));
        assert!(matches!(
            parse_args(&["--help".into()]).expect("help"),
            Command::Help
        ));
        let hull = parse_args(&["--hull".into(), "hull.png".into()]).expect("hull");
        assert!(matches!(hull, Command::Hull { .. }));
        let cost = parse_args(&["--hull-cost".into(), "cost.md".into()]).expect("cost");
        assert!(matches!(cost, Command::HullCost { .. }));
        let both = parse_args(&["--hull".into(), "a.png".into(), "--shots".into(), "out".into()]);
        assert!(both.expect_err("one mode").to_string().contains("only one"));
        let sized = parse_args(&["--hull".into(), "a.png".into(), "--width".into(), "64".into()]);
        assert!(sized.expect_err("shots only").to_string().contains("--shots"));
    }

    #[test]
    fn deck_medium_name_is_stable_in_the_help() {
        assert!(help_text().contains("deck-medium"));
        assert_eq!(ProfileKind::DeckMedium.name(), "deck-medium");
    }
}
