// SPDX-License-Identifier: MIT-0
//! `vis_plates`: DO27's landscape plate cameras through the full runtime frame (`lookdev::chase`
//! with `--plates`), forward and through the visibility buffer, with the SSIM between them.
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> cargo run --release -p lookdev --example vis_plates --
//!     [--map switchbacks|bore_twelve] [--shot switchbacks_mid] [--size 2560x1440] [--warmup 30]
//!     [--frames 30] [--png-dir DIR] [--threshold-px 1] [--shadow-threshold-px 1]
//! ```
//!
//! Three frames per plate: forward, the visibility buffer with its instances casting through the
//! forward shadow passes (round 1, `cluster_shadows: false`), and with cluster shadows (round 2).
//! Prints the SSIM of round 2 against round 1 and against forward.
//!
//! Every rigid opaque mesh of the frame (scatter rocks, set pieces) gets clusters from
//! `regia_asset_pipeline::build_meshlet_mesh_with_attributes` on first sight; terrain, sky and
//! the rest of
//! the frame are unchanged.

use std::process::ExitCode;
use std::sync::Arc;

use lookdev::chase::{ChaseOptions, MeshletSource, run};
use lookdev::look_r2::{R2Planets, R2Shot};
use lookdev::vis_stress::median;
use lookdev::{LookdevError, Planet};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::{LabVertex, MeshletBuilder};

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn cooker() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

fn read_png(path: &std::path::Path) -> Result<Vec<u8>, LookdevError> {
    let io = |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    };
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).map_err(io)?));
    let mut reader = decoder
        .read_info()
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| LookdevError::Config("png too large".into()))?;
    let mut buf = vec![0; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    buf.truncate(info.buffer_size());
    Ok(buf)
}

/// The runtime device plus block compression (DO27's cooked terrain is BC7/BC5/BC4), as
/// `do27_chase` builds it.
fn device() -> Result<regia_render::RenderDevice, LookdevError> {
    let base = regia_render::RenderDevice::headless()?;
    let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
    if !base.adapter.features().contains(bc) || base.device.features().contains(bc) {
        return Ok(base);
    }
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("vis plates device"),
        required_limits: base.device.limits(),
        required_features: base.device.features() | bc,
        ..Default::default()
    }))
    .map_err(|e| LookdevError::Config(e.to_string()))?;
    let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
    Ok(regia_render::RenderDevice::from_parts(
        base.instance,
        base.adapter,
        device,
        queue,
        capabilities,
    ))
}

fn main_inner(args: &[String]) -> Result<(), LookdevError> {
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let mut options = ChaseOptions::default();
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.warmup = value(args, "--warmup").and_then(|v| v.parse().ok()).unwrap_or(30);
    options.frames = value(args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(30);
    options.held = true;
    let png_dir = std::path::PathBuf::from(value(args, "--png-dir").unwrap_or("vis_plates"));
    std::fs::create_dir_all(&png_dir).map_err(|source| LookdevError::Io {
        path: png_dir.display().to_string(),
        source,
    })?;
    let maps: Vec<Planet> = match value(args, "--map") {
        Some("switchbacks") => vec![Planet::Mars],
        Some("bore_twelve") => vec![Planet::Europa],
        _ => vec![Planet::Europa, Planet::Mars],
    };
    let settings = regia_render::VisBufferSettings {
        lod_threshold_px: value(args, "--threshold-px")
            .and_then(|v| v.parse().ok())
            .unwrap_or(regia_render::VisBufferSettings::default().lod_threshold_px),
        shadow_lod_threshold_px: value(args, "--shadow-threshold-px")
            .and_then(|v| v.parse().ok())
            .unwrap_or(regia_render::VisBufferSettings::default().shadow_lod_threshold_px),
        ..regia_render::VisBufferSettings::default()
    };
    let round1 = regia_render::VisBufferSettings {
        cluster_shadows: false,
        ..settings
    };
    let only: Option<&str> = value(args, "--shot");
    let device = device()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let mut planets = R2Planets::default();
    let landscape = [
        R2Shot::BoreTwelveWide,
        R2Shot::BoreTwelveMid,
        R2Shot::BoreTwelveClose,
        R2Shot::SwitchbacksWide,
        R2Shot::SwitchbacksMid,
        R2Shot::SwitchbacksClose,
    ];
    let mut worst = f64::MAX;
    let mut worst_r1 = f64::MAX;
    for shot in landscape
        .into_iter()
        .filter(|s| maps.contains(&s.planet()) && only.is_none_or(|n| n == s.name()))
    {
        options.plate = Some(shot);
        let mut paths = Vec::new();
        let mut gpu = Vec::new();
        for (label, meshlets) in [
            ("forward", None),
            (
                "vis_r1",
                Some(MeshletSource {
                    builder: cooker(),
                    settings: round1,
                }),
            ),
            (
                "vis",
                Some(MeshletSource {
                    builder: cooker(),
                    settings,
                }),
            ),
        ] {
            options.meshlets = meshlets;
            let path = png_dir.join(format!("{}_{label}.png", shot.name()));
            options.png = Some(path.clone());
            let result = run(&device, &mut planets, &content, shot.planet(), &options)?;
            let frame: Vec<f32> = result.samples.iter().map(|s| s.gpu.frame_ms).collect();
            let rocks = result.samples.last().map_or(0, |s| s.rocks);
            println!(
                "{:<18} {label:<8} gpu p50 {:>7.3} ms, {rocks} rocks, vis: {} instances, {} meshes resident, {} declined",
                shot.name(),
                median(&frame),
                result.visbuffer.instances,
                result.visbuffer.resident_meshes,
                result.visbuffer.declined_meshes,
            );
            gpu.push(median(&frame));
            paths.push(path);
        }
        let a = read_png(&paths[0])?;
        let r1 = read_png(&paths[1])?;
        let b = read_png(&paths[2])?;
        let ssim_of = |x: &[u8], y: &[u8]| {
            regia_test_utils::golden::ssim_rgba8(x, y, options.width, options.height)
                .map_err(|e| LookdevError::Config(e.to_string()))
        };
        let ssim = ssim_of(&a, &b)?;
        let ssim_r1 = ssim_of(&r1, &b)?;
        worst = worst.min(ssim);
        worst_r1 = worst_r1.min(ssim_r1);
        println!(
            "{:<18} SSIM cluster shadows vs round 1 {ssim_r1:.4}, vs forward {ssim:.4}  (GPU forward {:.3}, round 1 {:.3}, round 2 {:.3} ms)",
            shot.name(),
            gpu[0],
            gpu[1],
            gpu[2]
        );
    }
    println!(
        "worst plate SSIM vs round 1 {worst_r1:.4} (target ≥ 0.998), vs forward {worst:.4} (target ≥ 0.98)"
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match main_inner(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vis_plates: {err}");
            ExitCode::from(1)
        }
    }
}
