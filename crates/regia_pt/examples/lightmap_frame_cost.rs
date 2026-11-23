// SPDX-License-Identifier: MPL-2.0
//! `lightmap_frame_cost`: what lightmaps cost a frame (the v0.3.0 timing evidence,
//! `docs/evidence/lightmaps-v030/`). Hand-run, never part of the gate; prints one JSON object per
//! line.
//!
//! ```text
//! cargo run --release -p regia_pt --example lightmap_frame_cost -- --mode lite \
//!     [--width 1280] [--height 720] [--frames 120]
//! cargo run --release -p regia_pt --features lightmap-gpu --example lightmap_frame_cost -- --mode bake \
//!     [--width 1920] [--height 1080] [--frames 120] [--density 96] [--samples 512] \
//!     [--tile-rays 1048576] [--yield-ms 2]
//! cargo run --release -p regia_pt --example lightmap_frame_cost -- --mode tiers \
//!     [--width 1920] [--height 1080] [--frames 240]
//! ```
//!
//! The scene is the GI validation colour room with a sphere on its floor (the interior of
//! `tests/lightmap_pt_evidence.rs`), every mesh a lightmap receiver, at Desktop High.
//!
//! - `--mode lite`: frame times with and without the lightmap on the full forward shader and on
//!   the Mobile Low lite shader (`ForwardLite::Low`), `--frames` frames each after a warm-up, with
//!   the renderer's lightmap counters (`lightmap_forward_stats`).
//! - `--mode tiers`: what the receivers' indirect specular from the lightmap costs (ADR-4762) on
//!   each render preset's shader and lightmap tier (Mobile Low and High on the lite shader, Desktop
//!   Low, High, Ultra, Deck and Cinematic on the full one): two renderers per preset, the specular off and on,
//!   timed in alternating batches of 20 frames (the card's clocks drift less between neighbors),
//!   `--frames` frames each after a warm-up. On the full shader also the reflection-capture
//!   candidate: the same energy partition, the image light on and one local capture of the room
//!   (ADR-2860), with the capture's bake time. GPU timestamps report the frame and forward
//!   pass separately from CPU submission plus GPU wait; timestamp samples lag by two frames.
//! - `--mode bake` (feature `lightmap-gpu`): the editor's case. Frames are drawn on one device while
//!   a GPU bake of the room at `--density` runs on a second device of the same adapter (the editor
//!   runs `regia bake-lightmaps --gpu` as a child process: its own device), first with the
//!   Lightmaps panel's pacing (`--tile-rays 1048576 --yield-ms 2`), then with the CLI's default
//!   (4194304 rays a tile, no pause). Frame times before the bake are the baseline.
//!
//! A frame is `render_into_texture` plus a wait for the GPU: the time an editor viewport frame
//! takes with vsync off. Pick the GPU with `REGIA_GPU_UUID` (and `REGIA_GPU_STRICT=1`).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use regia_asset::lightmap::{Lightmap, LightmapLayout};
use regia_asset_pipeline::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_atlas_layout};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::lightmap::{LightmapBakeSettings, LightmapScene, bake_lightmap};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapReceiverMesh, LightmapSettings, receiver_mesh};
use regia_render::{
    FrameQuality, FrameRenderer, FrameScene, GiMode, LabMesh, LabScene, RenderDevice, RenderProfile,
    RenderProfilePreset, uv_sphere,
};
use serde_json::json;

/// The texture-target id of the timed frames.
const TARGET_ID: u64 = 0x11A7_F7A3;
/// Frames drawn before timing starts (pipelines, uploads, shadow caches).
const WARM_FRAMES: u32 = 10;

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn arg_str<'a>(args: &'a [String], name: &str, default: &'a str) -> &'a str {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or(default, String::as_str)
}

fn quality() -> FrameQuality {
    quality_of(RenderProfilePreset::DesktopHigh)
}

/// `preset`'s quality without temporal upscaling, volumetrics or dynamic GI (the room's light is
/// the lightmap).
fn quality_of(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.taau = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

/// The colour room and its sphere at `width` × `height`.
fn room(width: u32, height: u32) -> LabScene {
    let mut lab = regia_test_utils::gi_scenes::color_room().lab;
    lab.width = width;
    lab.height = height;
    let (vertices, indices) = uv_sphere(0.35, 32, 16);
    lab.meshes.push(LabMesh {
        vertices,
        indices,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.45, 0.35, -0.45, 1.0],
        ],
        ..LabMesh::unskinned()
    });
    lab
}

/// The flattened scene, the atlas over every mesh at `density` and the receiving instances.
fn lay_out(lab: &LabScene, density: f32) -> Result<(LightmapScene, LightmapLayout, Vec<usize>), String> {
    let pt = from_frame(
        &lab.as_frame_scene_with_quality(quality()),
        &ExtractOptions::default(),
    );
    let flat = LightmapScene::from_pt_scene(&pt).map_err(|e| e.to_string())?;
    let receivers: Vec<usize> = flat
        .instances()
        .iter()
        .enumerate()
        .filter(|(_, i)| i.receiver && !i.indices.is_empty())
        .map(|(k, _)| k)
        .collect();
    let names: Vec<String> = receivers.iter().map(|k| format!("instance_{k}")).collect();
    let inputs: Vec<UnwrapInput<'_>> = receivers
        .iter()
        .zip(&names)
        .map(|(&k, name)| UnwrapInput {
            name,
            positions: &flat.instances()[k].positions,
            indices: &flat.instances()[k].indices,
            density_scale: 1.0,
        })
        .collect();
    let settings = LightmapUvSettings {
        texels_per_metre: density,
        padding: 2,
        max_atlas: 4096,
        ..LightmapUvSettings::default()
    };
    let (layout, _) = build_atlas_layout(&inputs, &settings, 0).map_err(|e| e.message)?;
    Ok((flat, layout, receivers))
}

/// Percentiles of `ms` (sorted copy).
fn summary(ms: &[f64]) -> serde_json::Value {
    let mut v = ms.to_vec();
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v.get(((v.len() as f64 - 1.0) * q).round() as usize).copied();
    let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
    json!({ "frames": v.len(), "mean_ms": mean, "p50_ms": at(0.5), "p95_ms": at(0.95),
            "p99_ms": at(0.99), "max_ms": v.last() })
}

/// A renderer drawing `lab` with `map` on every receiver (or none), on the `lite` shader or not.
struct Viewport<'a> {
    renderer: FrameRenderer,
    frame: FrameScene<'a>,
}

impl<'a> Viewport<'a> {
    fn new(
        lab: &'a LabScene,
        lit: Option<(&Arc<Lightmap>, &[usize], &'a [LightmapReceiverMesh])>,
        lite: Option<ForwardLite>,
    ) -> Self {
        Self::with(lab, lit, lite, quality(), true)
    }

    /// [`Self::new`] at `quality`, with the receivers' specular from the lightmap or not.
    fn with(
        lab: &'a LabScene,
        lit: Option<(&Arc<Lightmap>, &[usize], &'a [LightmapReceiverMesh])>,
        lite: Option<ForwardLite>,
        quality: FrameQuality,
        specular: bool,
    ) -> Self {
        let mut frame = lab.as_frame_scene_with_quality(quality);
        let mut renderer = FrameRenderer::new();
        renderer.set_forward_lite(lite);
        if let Some((map, receivers, meshes)) = lit {
            let mut keys = Vec::new();
            for (e, (&k, rx)) in receivers.iter().zip(meshes).enumerate() {
                let fm = &mut frame.meshes[k];
                fm.vertices = &rx.vertices;
                fm.indices = &rx.indices;
                fm.uv1 = &rx.uv2;
                fm.mesh_key ^= 0x11A7_0000_0000_0000;
                keys.push((fm.object_key, e as u32));
            }
            renderer.set_lightmap(
                Some(map.clone()),
                LightmapSettings {
                    enabled: true,
                    specular,
                    ..LightmapSettings::default()
                },
            );
            renderer.set_lightmap_receivers(&keys);
        }
        Self { renderer, frame }
    }

    /// The reflection-capture candidate of ADR-4762 on this viewport: the image light on (a uniform
    /// sky of `sky` cd/m²) and one local capture of the room baked from this frame. Returns the
    /// bake's seconds (six faces and the filter chain).
    fn bake_capture(&mut self, device: &RenderDevice, sky: [f32; 3]) -> Result<f64, String> {
        self.frame.ibl = true;
        self.renderer
            .set_environment_authority(regia_render::EnvironmentAuthority::Studio);
        let image =
            regia_render::StudioEquirect::new(64, 32, vec![sky; 64 * 32]).map_err(|e| e.to_string())?;
        if !self.renderer.set_studio_equirect(device, Some(&image)) {
            return Err("the uniform sky did not install".into());
        }
        self.renderer
            .place_local_capture(regia_render::LocalCaptureDesc::new(
                1,
                [0.0, 1.2, 0.0],
                [1.2, 1.2, 1.2],
            ))
            .map_err(|e| e.to_string())?;
        let t = Instant::now();
        self.renderer
            .bake_local_capture(device, &self.frame, 1)
            .map_err(|e| e.to_string())?;
        Ok(t.elapsed().as_secs_f64())
    }

    /// One frame and the wait for the GPU, milliseconds.
    fn frame(&mut self, device: &RenderDevice) -> Result<f64, String> {
        let t = Instant::now();
        self.renderer
            .render_into_texture(device, TARGET_ID, &self.frame)
            .map_err(|e| e.to_string())?;
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        Ok(t.elapsed().as_secs_f64() * 1000.0)
    }

    fn frames(&mut self, device: &RenderDevice, n: u32) -> Result<Vec<f64>, String> {
        (0..n).map(|_| self.frame(device)).collect()
    }
}

fn bake_cpu(lab: &LabScene, density: f32, samples: u32) -> Result<(Arc<Lightmap>, Vec<usize>), String> {
    let (flat, layout, receivers) = lay_out(lab, density)?;
    let settings = LightmapBakeSettings {
        samples,
        bounces: 4,
        ..LightmapBakeSettings::default()
    };
    let (map, _) =
        bake_lightmap(&flat, layout, &receivers, &settings, |_, _| {}).map_err(|e| e.to_string())?;
    Ok((Arc::new(map), receivers))
}

fn lite(device: &RenderDevice, args: &[String]) -> Result<(), String> {
    let (w, h) = (
        arg(args, "--width", 1280) as u32,
        arg(args, "--height", 720) as u32,
    );
    let frames = arg(args, "--frames", 120) as u32;
    let lab = room(w, h);
    let (map, receivers) = bake_cpu(&lab, 16.0, 64)?;
    let meshes: Vec<LightmapReceiverMesh> = receivers
        .iter()
        .enumerate()
        .map(|(e, &k)| receiver_mesh(&map, e, &lab.meshes[k].vertices, &lab.meshes[k].indices))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    for tier in [None, Some(ForwardLite::Low)] {
        for lit in [false, true] {
            let mut view = Viewport::new(&lab, lit.then_some((&map, &receivers[..], &meshes[..])), tier);
            view.frames(device, WARM_FRAMES)?;
            let ms = view.frames(device, frames)?;
            let stats = view.renderer.lightmap_forward_stats();
            println!(
                "{}",
                json!({
                    "mode": "lite",
                    "adapter": device.adapter.get_info().name,
                    "size": [w, h],
                    "shader": if tier.is_some() { "lite_low" } else { "full" },
                    "lightmap": lit,
                    "atlas": [map.width, map.height],
                    "counters": {
                        "receivers": stats.receivers,
                        "active": stats.active,
                        "drawn": stats.drawn,
                        "fallback": stats.fallback,
                    },
                    "timing": summary(&ms),
                })
            );
        }
    }
    Ok(())
}

/// Host frame latency and GPU timestamp samples, kept separately so host contention cannot
/// masquerade as shader cost. Each viewport has its own timer, so delayed samples never mix A/B.
#[derive(Default)]
struct TierSamples {
    host: Vec<f64>,
    gpu: Vec<f64>,
    forward: Vec<f64>,
}

impl TierSamples {
    fn batch(&mut self, viewport: &mut Viewport<'_>, device: &RenderDevice) -> Result<(), String> {
        for _ in 0..TIER_BATCH {
            self.host.push(viewport.frame(device)?);
            let timing = viewport.renderer.gpu_timings();
            if timing.frame_ms > 0.0 {
                self.gpu.push(f64::from(timing.frame_ms));
                self.forward.push(f64::from(timing.forward_ms));
            }
        }
        Ok(())
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "host": summary(&self.host),
            "gpu": (!self.gpu.is_empty()).then(|| summary(&self.gpu)),
            "forward": (!self.forward.is_empty()).then(|| summary(&self.forward)),
        })
    }
}

/// Frames per alternating batch in `--mode tiers`.
const TIER_BATCH: u32 = 20;

fn tiers(device: &RenderDevice, args: &[String]) -> Result<(), String> {
    let (w, h) = (
        arg(args, "--width", 1920) as u32,
        arg(args, "--height", 1080) as u32,
    );
    let frames = (arg(args, "--frames", 240) as u32).max(TIER_BATCH);
    let lab = room(w, h);
    let (map, receivers) = bake_cpu(&lab, 16.0, 64)?;
    let meshes: Vec<LightmapReceiverMesh> = receivers
        .iter()
        .enumerate()
        .map(|(e, &k)| receiver_mesh(&map, e, &lab.meshes[k].vertices, &lab.meshes[k].indices))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let lit = Some((&map, &receivers[..], &meshes[..]));
    for preset in [
        RenderProfilePreset::MobileLow,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
        RenderProfilePreset::Deck,
        RenderProfilePreset::Cinematic,
    ] {
        let q = quality_of(preset);
        let lite = (q.forward_lite != ForwardLite::Off).then_some(q.forward_lite);
        let mut off = Viewport::with(&lab, lit, lite, q, false);
        let mut on = Viewport::with(&lab, lit, lite, q, true);
        // The capture candidate keeps the same energy partition and baked fallback.
        let mut captured = None;
        if lite.is_none() {
            let mut v = Viewport::with(&lab, lit, lite, q, true);
            let seconds = v.bake_capture(device, [2_000.0; 3])?;
            captured = Some((v, seconds));
        }
        off.renderer.set_gpu_timing(device, true);
        on.renderer.set_gpu_timing(device, true);
        if let Some((v, _)) = captured.as_mut() {
            v.renderer.set_gpu_timing(device, true);
        }
        off.frames(device, WARM_FRAMES)?;
        on.frames(device, WARM_FRAMES)?;
        if let Some((v, _)) = captured.as_mut() {
            v.frames(device, WARM_FRAMES)?;
        }
        let (mut ms_off, mut ms_on, mut ms_capture) = (
            TierSamples::default(),
            TierSamples::default(),
            TierSamples::default(),
        );
        for batch in 0..frames.div_ceil(TIER_BATCH) {
            if batch.is_multiple_of(2) {
                ms_off.batch(&mut off, device)?;
                ms_on.batch(&mut on, device)?;
            } else {
                ms_on.batch(&mut on, device)?;
                ms_off.batch(&mut off, device)?;
            }
            if let Some((v, _)) = captured.as_mut() {
                ms_capture.batch(v, device)?;
            }
        }
        let stats = on.renderer.lightmap_forward_stats();
        println!(
            "{}",
            json!({
                "mode": "tiers",
                "adapter": device.adapter.get_info().name,
                "size": [w, h],
                "preset": format!("{preset:?}"),
                "shader": if lite.is_some() { "lite" } else { "full" },
                "lightmap_tier": stats.tier,
                "counters": { "drawn": stats.drawn, "lite": stats.lite, "fallback": stats.fallback },
                "specular_off": ms_off.json(),
                "specular_on": ms_on.json(),
                "capture": captured.as_ref().map(|(_, seconds)| json!({
                    "bake_seconds": seconds,
                    "timing": ms_capture.json(),
                })),
            })
        );
    }
    Ok(())
}

#[cfg(feature = "lightmap-gpu")]
fn bake(device: &RenderDevice, args: &[String]) -> Result<(), String> {
    use regia_pt::lightmap::{LightmapGpuControl, LightmapGpuOptions, bake_lightmap_gpu_with};
    let (w, h) = (
        arg(args, "--width", 1920) as u32,
        arg(args, "--height", 1080) as u32,
    );
    let frames = arg(args, "--frames", 120) as u32;
    let density = arg(args, "--density", 96) as f32;
    let samples = arg(args, "--samples", 512) as u32;
    let lab = room(w, h);
    let mut view = Viewport::new(&lab, None, None);
    view.frames(device, WARM_FRAMES)?;
    let baseline = view.frames(device, frames)?;
    println!(
        "{}",
        json!({ "mode": "bake", "phase": "baseline", "adapter": device.adapter.get_info().name,
                "size": [w, h], "timing": summary(&baseline) })
    );
    let paced = (
        "panel",
        arg(args, "--tile-rays", 1 << 20),
        arg(args, "--yield-ms", 2),
    );
    let default = ("cli_default", regia_pt::lightmap::DEFAULT_GPU_TILE_RAYS, 0);
    for (label, tile_rays, yield_ms) in [paced, default] {
        let (flat, layout, receivers) = lay_out(&lab, density)?;
        let texels = u64::from(layout.width) * u64::from(layout.height);
        let settings = LightmapBakeSettings {
            samples,
            bounces: 4,
            ..LightmapBakeSettings::default()
        };
        let options = LightmapGpuOptions {
            tile_rays,
            yield_between_tiles: std::time::Duration::from_millis(yield_ms),
            ..LightmapGpuOptions::default()
        };
        let done = std::sync::atomic::AtomicBool::new(false);
        let loop_start = Instant::now();
        let (during, bake) = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let started = Instant::now();
                // When the first tile finished, seconds after the bake started (device, upload, shader).
                let mut first_tile: Option<f64> = None;
                let result = RenderDevice::headless()
                    .map_err(|e| e.to_string())
                    .and_then(|bake_device| {
                        bake_lightmap_gpu_with(
                            &bake_device,
                            &flat,
                            layout,
                            &receivers,
                            &settings,
                            &options,
                            |_| {
                                first_tile.get_or_insert_with(|| started.elapsed().as_secs_f64());
                                LightmapGpuControl::Continue
                            },
                        )
                        .map_err(|e| e.to_string())
                    });
                done.store(true, std::sync::atomic::Ordering::Release);
                result.map(|(_, _, stats)| (started.elapsed().as_secs_f64(), stats, first_tile))
            });
            let mut ms = Vec::new();
            // Frames over one 60 Hz frame: (index, ms, seconds since the bake started at its end).
            let mut long = Vec::new();
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                match view.frame(device) {
                    Ok(t) => {
                        if t > 1000.0 / 60.0 {
                            long.push(json!([ms.len(), t, loop_start.elapsed().as_secs_f64()]));
                        }
                        ms.push(t);
                    }
                    Err(e) => return (Err(e), worker.join()),
                }
            }
            (Ok((ms, long)), worker.join())
        });
        let (during, long) = during?;
        let (seconds, stats, first_tile) = bake
            .map_err(|_| "the bake thread panicked".to_string())?
            .map_err(|e| format!("bake: {e}"))?;
        println!(
            "{}",
            json!({
                "mode": "bake", "phase": label, "adapter": device.adapter.get_info().name,
                "size": [w, h], "tile_rays": tile_rays, "yield_ms": yield_ms,
                "bake": { "seconds": seconds, "atlas_texels": texels, "samples": samples,
                          "tiles": stats.tiles, "chunks": stats.chunks, "peak_bytes": stats.peak_bytes },
                "first_tile_s": first_tile,
                "long_frames": long,
                "timing": summary(&during),
            })
        );
    }
    Ok(())
}

#[cfg(not(feature = "lightmap-gpu"))]
fn bake(_device: &RenderDevice, _args: &[String]) -> Result<(), String> {
    Err("--mode bake needs `--features lightmap-gpu`".into())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("no GPU: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match arg_str(&args, "--mode", "lite") {
        "lite" => lite(&device, &args),
        "bake" => bake(&device, &args),
        "tiers" => tiers(&device, &args),
        other => Err(format!("unknown --mode {other} (lite, tiers or bake)")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lightmap_frame_cost: {e}");
            ExitCode::FAILURE
        }
    }
}
