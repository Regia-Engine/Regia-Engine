// SPDX-License-Identifier: MIT-0
//! Peak RSS of a level's first frame, on a synthetic large scene that needs no content drop
//! (ADR-1346): `--materials` materials, each with three distinct block-compressed `--side`²
//! mip chains (BC7 base colour, BC5 normal, BC7 ORM; RGBA8 base colour only on a device without
//! BC), one quad per material, drawn once through
//! [`lookdev::cooked_runtime::render_model_owned`] (the frame `pak_load_bench load --frame`
//! draws).
//!
//! ```text
//! load_rss_bench [--materials 64] [--side 2048] [--frame 1280x720]
//! REGIA_STAGING_FLUSH_MIB=0 load_rss_bench ...    # the flush off: the frame before ADR-1346
//! ```
//!
//! Prints `LOAD_RSS {json}`:
//! - `texture_mib`: the CPU texture bytes the materials hold (every level of every chain);
//! - `build_hwm_mib`: peak RSS after the scene is built (the CPU copies alone);
//! - `hwm_mib`, `rss_anon_mib`, `rss_file_mib`: peak RSS and resident heap / file pages after
//!   the first frame;
//! - `staging_cap_mib`, `flushes`, `waits`, `peak_pending_mib`: the staging flush
//!   ([`regia_render::staging_flush_stats`]);
//! - `first_frame_ms`: the frame with every upload.
//!
//! `hwm_mib - build_hwm_mib` is what the first frame added on top of the scene. On a discrete
//! GPU the textures live in VRAM and that delta is mostly wgpu's write staging; on lavapipe the
//! textures are host memory too (both runs pay them equally). Run the two settings in separate
//! processes: `VmHWM` never goes down.

use std::sync::Arc;
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::cooked_runtime::{self as cr, View};
use regia_core::glam::Vec3;
use regia_render::{
    CompressedMaps, CompressedTexture, LabMaterial, LabMesh, LabTexture, LabVertex, RenderDevice,
};

/// `VmHWM`, `RssAnon`, `RssFile` from `/proc/self/status`, MiB (zero where unavailable).
fn memory() -> (f64, f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
            .map_or(0.0, |kib| kib / 1024.0)
    };
    (field("VmHWM:"), field("RssAnon:"), field("RssFile:"))
}

/// `len` bytes from a xorshift64 stream seeded with `seed` (distinct per chain, so no two
/// materials share pages).
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len.next_multiple_of(8));
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A `side`² chain of 4×4 blocks of 16 bytes (BC5, BC7), level 0 first, down to one block.
fn block_chain(format: wgpu::TextureFormat, side: u32, seed: u64) -> CompressedTexture {
    let mut levels = Vec::new();
    let mut s = side;
    loop {
        let blocks = s.div_ceil(4) as usize;
        levels.push(noise(seed ^ u64::from(s), blocks * blocks * 16).into());
        if s <= 4 {
            break;
        }
        s /= 2;
    }
    CompressedTexture {
        format,
        width: side,
        height: side,
        levels,
        normal_layout: if format == wgpu::TextureFormat::Bc5RgUnorm {
            regia_render::NormalLayout::Xy
        } else {
            regia_render::NormalLayout::Xyz
        },
        mip_stream: None,
    }
}

/// Material `i` and the CPU bytes its textures hold.
fn material(i: u64, side: u32, bc: bool) -> (LabMaterial, u64) {
    if !bc {
        let rgba = noise(i, (side * side * 4) as usize);
        let bytes = rgba.len() as u64;
        let albedo = LabTexture {
            width: side,
            height: side,
            rgba,
        };
        return (
            LabMaterial {
                albedo: Some(albedo),
                ..LabMaterial::default()
            },
            bytes,
        );
    }
    let maps = CompressedMaps {
        albedo: Some(block_chain(wgpu::TextureFormat::Bc7RgbaUnormSrgb, side, 3 * i)),
        normal: Some(block_chain(wgpu::TextureFormat::Bc5RgUnorm, side, 3 * i + 1)),
        occlusion: Some(block_chain(wgpu::TextureFormat::Bc7RgbaUnorm, side, 3 * i + 2)),
        ..CompressedMaps::default()
    };
    let bytes = [&maps.albedo, &maps.normal, &maps.occlusion]
        .into_iter()
        .flatten()
        .flat_map(|t| &t.levels)
        .map(|level| level.len() as u64)
        .sum();
    (
        LabMaterial {
            compressed: Some(Arc::new(maps)),
            ..LabMaterial::default()
        },
        bytes,
    )
}

/// A unit quad facing +Z at grid cell `i` of a `columns`-wide grid, drawing material `i`.
fn quad(i: usize, columns: usize) -> LabMesh {
    let (x, y) = ((i % columns) as f32 * 1.2, (i / columns) as f32 * 1.2);
    let n = [0.0, 0.0, 1.0];
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x, y, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 1.0, y, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 1.0, y + 1.0, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x, y + 1.0, 0.0], n, [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: i,
        ..LabMesh::unskinned()
    }
}

fn run(materials: usize, side: u32, (width, height): (u32, u32)) -> Result<(), LookdevError> {
    let device = RenderDevice::headless()?;
    let bc = device
        .device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
    let started = Instant::now();
    let mut texture_bytes = 0u64;
    let mut mats = Vec::with_capacity(materials);
    for i in 0..materials {
        let (m, bytes) = material(i as u64 + 1, side, bc);
        texture_bytes += bytes;
        mats.push(m);
    }
    let columns = (materials as f64).sqrt().ceil().max(1.0) as usize;
    let meshes: Vec<LabMesh> = (0..materials).map(|i| quad(i, columns)).collect();
    let build_ms = started.elapsed().as_secs_f64() * 1e3;
    let (build_hwm, build_anon, _) = memory();

    let (lo, hi) = cr::bounds(&meshes);
    let view = View::of_bounds(lo, hi, Vec3::new(0.0, 0.0, 1.0), 0.8);
    let frame_started = Instant::now();
    let frame = cr::render_model_owned(&device, meshes, mats, view, width, height)?;
    let first_frame_ms = frame_started.elapsed().as_secs_f64() * 1e3;
    let (hwm, anon, file) = memory();
    let staging = regia_render::staging_flush_stats();
    let mib = |b: u64| b as f64 / 1048576.0;
    println!(
        "LOAD_RSS {{\"materials\":{materials},\"side\":{side},\"block_compressed\":{bc},\"texture_mib\":{:.1},\
         \"build_ms\":{build_ms:.1},\"build_hwm_mib\":{build_hwm:.0},\"build_rss_anon_mib\":{build_anon:.0},\
         \"first_frame_ms\":{first_frame_ms:.1},\"hwm_mib\":{hwm:.0},\"rss_anon_mib\":{anon:.0},\"rss_file_mib\":{file:.0},\
         \"frame_added_mib\":{:.0},\"staging_cap_mib\":{:.0},\"flushes\":{},\"flushed_mib\":{:.1},\"waits\":{},\
         \"peak_pending_mib\":{:.1},\"texture_vram_mib\":{}}}",
        mib(texture_bytes),
        hwm - build_hwm,
        mib(staging.cap_bytes),
        staging.flushes,
        mib(staging.flushed_bytes),
        staging.waits,
        mib(staging.peak_pending_bytes),
        frame
            .texture_vram
            .map_or_else(|| "null".to_owned(), |b| format!("{:.1}", mib(b))),
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let materials = value("--materials").and_then(|v| v.parse().ok()).unwrap_or(64);
    let side = value("--side")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2048)
        .clamp(4, 16384);
    let frame = value("--frame")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280, 720));
    match run(materials, side, frame) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("load_rss_bench: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
