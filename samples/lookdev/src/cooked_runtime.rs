// SPDX-License-Identifier: MIT-0
//! Cooked runtime parity (lane cooked-runtime): a project goes through `regia cook` into a pak,
//! the runtime asset server mounts that pak with no source file present, and the hero model it
//! loads ([`regia_render::cooked_model`]) renders the frame the direct glTF path
//! ([`crate::hero::load`]) renders.
//!
//! The pieces are shared by the test (`tests/cooked_runtime.rs`, an own fixture project written by
//! [`fixture::write_project`]) and the `cooked_runtime` bin (DO27's hero buildings through
//! `REGIA_DO27_CONTENT`):
//!
//! - [`regia_cli`] finds the `regia` binary for this target dir and profile, building it once
//!   when it is not already beside the executable;
//! - [`cook`] runs `regia cook` and returns the pak directory;
//! - [`mount_paks`] opens an [`AssetServer`] on the paks alone (an empty project root, so there is
//!   neither source nor cooked cache to fall back to);
//! - [`to_harness_frame`] puts the cooked model in the harness's frame (the cook turns glTF +Z to
//!   −Z with a 180° yaw; the harness mirrors X for DO27's left-handed content frame; the two differ
//!   by a Z mirror);
//! - [`render_model`] draws one three-quarter frame, [`diff`] compares frames, [`texture_vram`]
//!   counts the material-texture bytes the device holds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use regia_asset::{AssetServer, AssetServerSettings};
use regia_core::glam::Vec3;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset,
};

use crate::LookdevError;

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The `regia` CLI for this process's cargo target directory and profile.
///
/// A bin runs from `<target>/<profile>/` and a test from `<target>/<profile>/deps/`.
/// `REGIA_CLI` wins when it names a file. Otherwise the binary beside this executable is
/// used. When it is absent, it is built once with `cargo build -p regia_cli --bin regia`
/// into that same directory (one build per process).
///
/// # Panics
///
/// Panics when `REGIA_CLI` names a missing file, or that build fails. The message names the
/// missing binary and the command that builds it.
#[must_use]
#[track_caller]
pub fn regia_cli() -> PathBuf {
    match crate::workspace_bin::ensure_regia_cli() {
        Ok(path) => path,
        Err(message) => panic!("{message}"),
    }
}

/// What one `regia cook` run produced.
#[derive(Debug, Clone)]
pub struct CookRun {
    /// `build/paks/<platform>` of the project.
    pub pak_dir: PathBuf,
    /// Wall time of the cook (texture compression dominates).
    pub elapsed: Duration,
    /// Bytes of every `*.fpak` written.
    pub pak_bytes: u64,
    /// The CLI's stderr (the cook report).
    pub log: String,
}

/// Runs `regia cook --project <root> --platform <platform> --quality final`.
///
/// # Errors
///
/// The CLI is missing, fails, or writes no pak.
pub fn cook(cli: &Path, root: &Path, platform: &str) -> Result<CookRun, LookdevError> {
    cook_with_env(cli, root, platform, &[])
}

/// [`cook`] with extra environment variables for the CLI (for example
/// `REGIA_PAK_GPU_MESHES=1` for GPU-ready pak meshes).
///
/// # Errors
///
/// As [`cook`].
pub fn cook_with_env(
    cli: &Path,
    root: &Path,
    platform: &str,
    env: &[(&str, &str)],
) -> Result<CookRun, LookdevError> {
    let started = Instant::now();
    let output = Command::new(cli)
        .args(["cook", "--project"])
        .arg(root)
        .args(["--platform", platform, "--quality", "final"])
        .envs(env.iter().copied())
        .output()
        .map_err(io(cli))?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(LookdevError::Config(format!("regia cook failed:\n{log}")));
    }
    let pak_dir = root.join("build/paks").join(platform);
    let pak_bytes = pak_files(&pak_dir)?
        .iter()
        .map(|p| std::fs::metadata(p).map_or(0, |m| m.len()))
        .sum();
    if pak_bytes == 0 {
        return Err(LookdevError::Config(format!(
            "no pak in {}:\n{log}",
            pak_dir.display()
        )));
    }
    Ok(CookRun {
        pak_dir,
        elapsed: started.elapsed(),
        pak_bytes,
        log,
    })
}

/// The `*.fpak` files of `dir`, sorted.
///
/// # Errors
///
/// `dir` cannot be read.
pub fn pak_files(dir: &Path) -> Result<Vec<PathBuf>, LookdevError> {
    let mut paks: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(io(dir))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "fpak"))
        .collect();
    paks.sort();
    Ok(paks)
}

/// An asset server that sees only the paks in `content`: its project root is `empty_root`,
/// which must hold no `assets/` and no cooked cache, so every load comes from a pak.
///
/// # Errors
///
/// The server does not open.
pub fn mount_paks(content: &Path, empty_root: &Path, platform: &str) -> Result<AssetServer, LookdevError> {
    std::fs::create_dir_all(empty_root).map_err(io(empty_root))?;
    AssetServer::open(AssetServerSettings {
        project_root: empty_root.to_path_buf(),
        platform: platform.to_string(),
        content_dir: Some(content.to_path_buf()),
    })
    .map_err(|e| LookdevError::Config(format!("asset server: {e}")))
}

/// Mirrors cooked meshes across Z (vertices, normals, tangents; winding and bitangent sign
/// flipped) so they sit where [`crate::hero::load`] puts the same glTF.
pub fn to_harness_frame(meshes: &mut [LabMesh]) {
    for mesh in meshes {
        for v in &mut mesh.vertices {
            v.position[2] = -v.position[2];
            v.normal[2] = -v.normal[2];
            v.tangent[2] = -v.tangent[2];
            v.tangent[3] = -v.tangent[3];
        }
        for tri in mesh.indices.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
    }
}

/// Axis-aligned bounds of `meshes`.
#[must_use]
pub fn bounds(meshes: &[LabMesh]) -> (Vec3, Vec3) {
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    for v in meshes.iter().flat_map(|m| &m.vertices) {
        let p = Vec3::from_array(v.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    (lo, hi)
}

/// A camera for [`render_model`]: eye and target in world space.
#[derive(Debug, Clone, Copy)]
pub struct View {
    /// Eye position.
    pub eye: Vec3,
    /// Look-at point.
    pub target: Vec3,
}

impl View {
    /// Three-quarter view of the box `(lo, hi)` from direction `dir`, `distance` box diagonals out.
    #[must_use]
    pub fn of_bounds(lo: Vec3, hi: Vec3, dir: Vec3, distance: f32) -> Self {
        let centre = (lo + hi) * 0.5;
        let size = (hi - lo).length().max(1.0);
        Self {
            eye: centre + dir.normalize_or_zero() * size * distance,
            target: centre,
        }
    }
}

/// One rendered frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, sRGB-encoded.
    pub pixels: Vec<u8>,
    /// Material-texture bytes on the device right after the frame, while the renderer still
    /// holds them ([`texture_vram`]); `None` without an allocator report.
    pub texture_vram: Option<u64>,
}

/// Renders `meshes` / `materials` from `view` at `width`×`height` on Desktop High (no TAA,
/// deterministic post, fixed sun and exposure).
///
/// # Errors
///
/// The frame does not render.
pub fn render_model(
    device: &RenderDevice,
    meshes: &[LabMesh],
    materials: Vec<LabMaterial>,
    view: View,
    width: u32,
    height: u32,
) -> Result<Frame, LookdevError> {
    render_model_owned(device, meshes.to_vec(), materials, view, width, height)
}

/// [`render_model`] taking the meshes by value: the frame draws them without a second copy of
/// every vertex, so a load bench measures the engine's peak RSS, not the harness's clone.
///
/// # Errors
///
/// The frame does not render.
pub fn render_model_owned(
    device: &RenderDevice,
    meshes: Vec<LabMesh>,
    materials: Vec<LabMaterial>,
    view: View,
    width: u32,
    height: u32,
) -> Result<Frame, LookdevError> {
    let post = PostSettings {
        exposure_ev: 13.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye: view.eye.to_array(),
        target: view.target.to_array(),
        fov_y: 0.8,
        near: 0.05,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.4, -0.7, -0.3], [1.0, 0.95, 0.9], 60_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    // The frames compare texture paths (cooked vs direct, BC vs CPU decode) under one light.
    // Desktop High's GPU irradiance probes change that light; the decode bounds were measured
    // under the analytic terms (fin_side's BC/CPU p99.9 sits at its bound of 8 there and reads
    // 9 under the probes, with no light difference between the two devices).
    quality.gi_probes = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        ..OutdoorParams::default()
    });
    let image = renderer.render_offscreen(device, &frame)?;
    let texture_vram = texture_vram(device);
    drop(renderer);
    Ok(Frame {
        width: image.width,
        height: image.height,
        pixels: image.pixels,
        texture_vram,
    })
}

/// Per-channel differences between two frames of the same size.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiffStats {
    /// Largest channel difference.
    pub max: u8,
    /// Mean channel difference over every RGB channel of the frame.
    pub mean: f64,
    /// Mean channel difference over the pixels where either frame differs from the other at all.
    pub mean_changed: f64,
    /// 99.9th percentile of the per-pixel largest channel difference.
    pub p999: u8,
    /// Pixels whose largest channel difference exceeds 8.
    pub over_8: usize,
    /// Pixels compared.
    pub pixels: usize,
}

/// Compares the RGB channels of two equally sized RGBA8 frames.
#[must_use]
pub fn diff(a: &[u8], b: &[u8]) -> DiffStats {
    let mut per_pixel: Vec<u8> = Vec::with_capacity(a.len() / 4);
    let (mut sum, mut changed_sum, mut changed) = (0u64, 0u64, 0u64);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = [0, 1, 2].map(|c| pa[c].abs_diff(pb[c]));
        let s: u64 = d.iter().map(|&x| u64::from(x)).sum();
        sum += s;
        if s > 0 {
            changed_sum += s;
            changed += 3;
        }
        per_pixel.push(d.into_iter().max().unwrap_or(0));
    }
    let pixels = per_pixel.len();
    let max = per_pixel.iter().copied().max().unwrap_or(0);
    let over_8 = per_pixel.iter().filter(|&&d| d > 8).count();
    per_pixel.sort_unstable();
    let p999 = per_pixel
        .get(((pixels as f64) * 0.999) as usize)
        .or(per_pixel.last())
        .copied()
        .unwrap_or(0);
    DiffStats {
        max,
        mean: sum as f64 / (pixels.max(1) * 3) as f64,
        mean_changed: changed_sum as f64 / changed.max(1) as f64,
        p999,
        over_8,
        pixels,
    }
}

/// Bytes of an RGBA8 texture with its full mip chain (what the renderer uploads for one
/// [`regia_render::LabTexture`]).
#[must_use]
pub fn rgba8_chain_bytes(width: u32, height: u32) -> u64 {
    let (mut w, mut h, mut total) = (u64::from(width.max(1)), u64::from(height.max(1)), 0u64);
    loop {
        total += w * h * 4;
        if w == 1 && h == 1 {
            return total;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
}

/// RGBA8 bytes (full mip chains) of the distinct images the materials hold in their
/// RGBA8 slots, counted once per image content.
#[must_use]
pub fn rgba8_material_bytes(materials: &[LabMaterial]) -> u64 {
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    for m in materials {
        for t in [&m.albedo, &m.normal_map, &m.occlusion_map, &m.emissive_map]
            .into_iter()
            .flatten()
        {
            let key = (t.width, t.height, t.rgba.as_ptr() as usize, t.rgba.len());
            if seen.insert(key) {
                total += rgba8_chain_bytes(t.width, t.height);
            }
        }
    }
    total
}

/// Material-texture bytes the device holds now, by the renderer's allocation labels
/// (`material texture`, `… (bcn)`, `… (streamed)`), from
/// [`regia_render::memory_report`]. `None` where the backend has no allocator report.
#[must_use]
pub fn texture_vram(device: &RenderDevice) -> Option<u64> {
    let report = regia_render::memory_report(device);
    report.available.then(|| {
        report
            .labels
            .iter()
            .filter(|l| l.label.starts_with("material texture"))
            .map(|l| l.bytes)
            .sum()
    })
}

/// A device that cannot sample block-compressed formats (the stock headless device minus
/// `TEXTURE_COMPRESSION_*`): cooked maps take the CPU-decode fallback.
///
/// # Errors
///
/// No adapter or device.
pub fn device_without_block_compression() -> Result<RenderDevice, LookdevError> {
    let base = RenderDevice::headless()?;
    let compression = wgpu::Features::TEXTURE_COMPRESSION_BC
        | wgpu::Features::TEXTURE_COMPRESSION_ASTC
        | wgpu::Features::TEXTURE_COMPRESSION_ETC2;
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("cooked-runtime device without block compression"),
        required_limits: base.device.limits(),
        required_features: base.device.features() - compression,
        ..Default::default()
    }))
    .map_err(|e| LookdevError::Config(e.to_string()))?;
    let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
    Ok(RenderDevice::from_parts(
        base.instance,
        base.adapter,
        device,
        queue,
        capabilities,
    ))
}

/// Writes `frame` as a PNG.
///
/// # Errors
///
/// The file cannot be written.
pub fn save(frame: &Frame, path: &Path) -> Result<(), LookdevError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    crate::default_look::write_png(path, frame.width, frame.height, &frame.pixels)
}

/// A per-pixel difference image (largest channel difference × `gain`, grey).
#[must_use]
pub fn diff_image(a: &Frame, b: &Frame, gain: u8) -> Frame {
    let pixels = a
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.pixels.as_chunks::<4>().0)
        .flat_map(|(pa, pb)| {
            let d = (0..3).map(|c| pa[c].abs_diff(pb[c])).max().unwrap_or(0);
            let v = d.saturating_mul(gain);
            [v, v, v, 255]
        })
        .collect();
    Frame {
        width: a.width,
        height: a.height,
        pixels,
        texture_vram: None,
    }
}

pub mod fixture {
    //! An own DO27-like project (no third-party content): a small outpost building glTF with
    //! textured walls (albedo, normal, packed ORM), a tinted double-sided glass pane
    //! (`KHR_materials_transmission` / `_volume` / `_ior`), an emissive lamp
    //! (`KHR_materials_emissive_strength`) and a double-sided fin; a terrain (tiled heightfield +
    //! one layer); a rock scatter file; and a level.

    use std::path::Path;

    use serde_json::json;

    use crate::LookdevError;

    /// Deterministic hash noise in 0..1.
    fn hash(x: u32, y: u32, seed: u32) -> f32 {
        let mut h =
            x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1_e995);
        h ^= h >> 15;
        (h & 0xffff) as f32 / 65535.0
    }

    /// Smooth value noise (bilinear over a `cell`-texel lattice).
    fn noise(x: u32, y: u32, cell: u32, seed: u32) -> f32 {
        let (cx, cy) = (x / cell, y / cell);
        let (fx, fy) = ((x % cell) as f32 / cell as f32, (y % cell) as f32 / cell as f32);
        let s = |t: f32| t * t * (3.0 - 2.0 * t);
        let (sx, sy) = (s(fx), s(fy));
        let a = hash(cx, cy, seed) * (1.0 - sx) + hash(cx + 1, cy, seed) * sx;
        let b = hash(cx, cy + 1, seed) * (1.0 - sx) + hash(cx + 1, cy + 1, seed) * sx;
        a * (1.0 - sy) + b * sy
    }

    /// Panel grooves: distance (texels) to the nearest groove line of a 64-texel grid with a
    /// staggered row offset.
    fn groove(x: u32, y: u32) -> f32 {
        let row = y / 64;
        let xs = x + if row % 2 == 1 { 32 } else { 0 };
        let dx = (xs % 64).min(64 - xs % 64) as f32;
        let dy = (y % 64).min(64 - y % 64) as f32;
        dx.min(dy)
    }

    fn image(size: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        (0..size)
            .flat_map(|y| (0..size).map(move |x| (x, y)))
            .flat_map(|(x, y)| f(x, y))
            .collect()
    }

    /// Weathered concrete panels with rust streaks (sRGB bytes).
    #[must_use]
    pub fn albedo(size: u32) -> Vec<u8> {
        image(size, |x, y| {
            let n = noise(x, y, 16, 1) * 0.6 + noise(x, y, 4, 2) * 0.4;
            let g = groove(x, y);
            let streak = (noise(x, y / 8, 6, 3) - 0.6).max(0.0) * 2.5;
            let base = 120.0 + 70.0 * n - if g < 2.0 { 60.0 } else { 0.0 };
            [
                (base + 50.0 * streak).clamp(0.0, 255.0) as u8,
                (base * 0.96 + 15.0 * streak).clamp(0.0, 255.0) as u8,
                (base * 0.9 - 20.0 * streak).clamp(0.0, 255.0) as u8,
                255,
            ]
        })
    }

    /// Groove bevels and fine grain as a tangent-space normal map (+Y up, linear bytes).
    #[must_use]
    pub fn normal(size: u32) -> Vec<u8> {
        let h = |x: u32, y: u32| -> f32 {
            let g = groove(x, y);
            (g.min(3.0) / 3.0) * 1.0 + noise(x, y, 3, 7) * 0.15
        };
        image(size, |x, y| {
            let (xl, xr) = (h(x.saturating_sub(1), y), h((x + 1).min(size - 1), y));
            let (yu, yd) = (h(x, y.saturating_sub(1)), h(x, (y + 1).min(size - 1)));
            let n = regia_core::glam::Vec3::new(-(xr - xl) * 1.5, (yd - yu) * 1.5, 1.0).normalize();
            let e = |c: f32| ((c * 0.5 + 0.5) * 255.0).round() as u8;
            [e(n.x), e(n.y), e(n.z), 255]
        })
    }

    /// Packed ORM: occlusion in the grooves (R), roughness 0.45–0.95 (G), metal trim bands (B).
    #[must_use]
    pub fn orm(size: u32) -> Vec<u8> {
        image(size, |x, y| {
            let g = groove(x, y);
            let ao = if g < 3.0 { 110.0 + 48.0 * g } else { 255.0 };
            let rough = 115.0 + 125.0 * noise(x, y, 8, 11);
            let metal = if (y % 128) < 10 { 255 } else { 0 };
            [ao as u8, rough as u8, metal, 255]
        })
    }

    /// Warm lamp face with a hot centre (sRGB bytes).
    #[must_use]
    pub fn emissive(size: u32) -> Vec<u8> {
        image(size, |x, y| {
            let c = size as f32 * 0.5;
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c;
            let v = (1.0 - d).clamp(0.0, 1.0);
            [
                (255.0 * v.sqrt()) as u8,
                (200.0 * v) as u8,
                (120.0 * v * v) as u8,
                255,
            ]
        })
    }

    fn png_bytes(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, LookdevError> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| LookdevError::Png(e.to_string()))?;
        w.write_image_data(rgba)
            .map_err(|e| LookdevError::Png(e.to_string()))?;
        w.finish().map_err(|e| LookdevError::Png(e.to_string()))?;
        Ok(out)
    }

    fn gray16_png(samples: &[u16], width: u32, height: u32) -> Result<Vec<u8>, LookdevError> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Sixteen);
        let mut w = enc.write_header().map_err(|e| LookdevError::Png(e.to_string()))?;
        let be: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
        w.write_image_data(&be)
            .map_err(|e| LookdevError::Png(e.to_string()))?;
        w.finish().map_err(|e| LookdevError::Png(e.to_string()))?;
        Ok(out)
    }

    fn write(path: &Path, bytes: &[u8]) -> Result<(), LookdevError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| LookdevError::Io {
                path: dir.display().to_string(),
                source,
            })?;
        }
        std::fs::write(path, bytes).map_err(|source| LookdevError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    /// One primitive: positions, normals, UVs, indices.
    #[derive(Default)]
    struct Prim {
        p: Vec<[f32; 3]>,
        n: Vec<[f32; 3]>,
        uv: Vec<[f32; 2]>,
        i: Vec<u32>,
    }

    impl Prim {
        /// A quad from corner `o` along `u` and `v` (normal `u × v`), UV 0..`tile`.
        fn quad(&mut self, o: [f32; 3], u: [f32; 3], v: [f32; 3], tile: [f32; 2]) {
            let (o, u, v) = (
                regia_core::glam::Vec3::from_array(o),
                regia_core::glam::Vec3::from_array(u),
                regia_core::glam::Vec3::from_array(v),
            );
            let n = u.cross(v).normalize().to_array();
            let base = self.p.len() as u32;
            for (a, b) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                self.p.push((o + u * a + v * b).to_array());
                self.n.push(n);
                self.uv.push([a * tile[0], (1.0 - b) * tile[1]]);
            }
            self.i
                .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        /// An axis-aligned box `lo..hi`, outward faces, UVs scaled by `texel_m` metres per tile.
        fn cuboid(&mut self, lo: [f32; 3], hi: [f32; 3], tile_m: f32) {
            let [x0, y0, z0] = lo;
            let [x1, y1, z1] = hi;
            let (dx, dy, dz) = (x1 - x0, y1 - y0, z1 - z0);
            let t = |a: f32, b: f32| [a / tile_m, b / tile_m];
            self.quad([x0, y0, z1], [dx, 0.0, 0.0], [0.0, dy, 0.0], t(dx, dy)); // +Z
            self.quad([x1, y0, z0], [-dx, 0.0, 0.0], [0.0, dy, 0.0], t(dx, dy)); // -Z
            self.quad([x1, y0, z1], [0.0, 0.0, -dz], [0.0, dy, 0.0], t(dz, dy)); // +X
            self.quad([x0, y0, z0], [0.0, 0.0, dz], [0.0, dy, 0.0], t(dz, dy)); // -X
            self.quad([x0, y1, z1], [dx, 0.0, 0.0], [0.0, 0.0, -dz], t(dx, dz)); // +Y
            self.quad([x0, y0, z0], [dx, 0.0, 0.0], [0.0, 0.0, dz], t(dx, dz)); // -Y
        }
    }

    /// Writes `<dir>/<stem>.gltf` + `.bin` with one node per `(name, primitive, material)`.
    fn write_gltf(
        dir: &Path,
        stem: &str,
        parts: &[(&str, Prim, usize)],
        materials: serde_json::Value,
        images: &[&str],
    ) -> Result<(), LookdevError> {
        let mut bin: Vec<u8> = Vec::new();
        let mut views = Vec::new();
        let mut accessors = Vec::new();
        let mut meshes = Vec::new();
        let mut nodes = Vec::new();
        let mut push = |bytes: Vec<u8>, target: u32| -> usize {
            while !bin.len().is_multiple_of(4) {
                bin.push(0);
            }
            views.push(
                json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len(), "target": target}),
            );
            bin.extend(bytes);
            views.len() - 1
        };
        for (name, prim, material) in parts {
            let f32s = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
            let pos: Vec<f32> = prim.p.iter().flatten().copied().collect();
            let nrm: Vec<f32> = prim.n.iter().flatten().copied().collect();
            let uv: Vec<f32> = prim.uv.iter().flatten().copied().collect();
            let idx: Vec<u8> = prim.i.iter().flat_map(|x| x.to_le_bytes()).collect();
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for p in &prim.p {
                for c in 0..3 {
                    lo[c] = lo[c].min(p[c]);
                    hi[c] = hi[c].max(p[c]);
                }
            }
            let count = prim.p.len();
            let vp = push(f32s(&pos), 34962);
            let vn = push(f32s(&nrm), 34962);
            let vu = push(f32s(&uv), 34962);
            let vi = push(idx, 34963);
            let a = accessors.len();
            accessors.push(json!({"bufferView": vp, "componentType": 5126, "count": count, "type": "VEC3", "min": lo, "max": hi}));
            accessors.push(json!({"bufferView": vn, "componentType": 5126, "count": count, "type": "VEC3"}));
            accessors.push(json!({"bufferView": vu, "componentType": 5126, "count": count, "type": "VEC2"}));
            accessors.push(
                json!({"bufferView": vi, "componentType": 5125, "count": prim.i.len(), "type": "SCALAR"}),
            );
            meshes.push(json!({"name": name, "primitives": [{
                "attributes": {"POSITION": a, "NORMAL": a + 1, "TEXCOORD_0": a + 2},
                "indices": a + 3, "material": material}]}));
            nodes.push(json!({"name": name, "mesh": meshes.len() - 1}));
        }
        let images: Vec<_> = images.iter().map(|uri| json!({"uri": uri})).collect();
        let textures: Vec<_> = (0..images.len())
            .map(|i| json!({"source": i, "sampler": 0}))
            .collect();
        let doc = json!({
            "asset": {"version": "2.0", "generator": "regia lookdev cooked_runtime fixture"},
            "extensionsUsed": ["KHR_materials_transmission", "KHR_materials_volume", "KHR_materials_ior",
                               "KHR_materials_emissive_strength"],
            "scene": 0,
            "scenes": [{"nodes": (0..nodes.len()).collect::<Vec<_>>()}],
            "nodes": nodes,
            "meshes": meshes,
            "accessors": accessors,
            "bufferViews": views,
            "buffers": [{"uri": format!("{stem}.bin"), "byteLength": bin.len()}],
            "materials": materials,
            "samplers": [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}],
            "textures": textures,
            "images": images,
        });
        write(&dir.join(format!("{stem}.bin")), &bin)?;
        let text = serde_json::to_string_pretty(&doc).map_err(|e| LookdevError::Json(e.to_string()))?;
        write(&dir.join(format!("{stem}.gltf")), text.as_bytes())
    }

    /// The outpost's material list (glTF JSON), in the order the parts reference.
    fn outpost_materials() -> serde_json::Value {
        json!([
            {"name": "Walls", "pbrMetallicRoughness": {
                "baseColorTexture": {"index": 0}, "metallicRoughnessTexture": {"index": 2},
                "metallicFactor": 1.0, "roughnessFactor": 1.0},
             "normalTexture": {"index": 1}, "occlusionTexture": {"index": 2}},
            {"name": "Glass", "doubleSided": true, "alphaMode": "BLEND",
             "pbrMetallicRoughness": {"baseColorFactor": [1.0, 1.0, 1.0, 1.0], "metallicFactor": 0.0,
                                      "roughnessFactor": 0.05},
             "extensions": {"KHR_materials_transmission": {"transmissionFactor": 0.92},
                            "KHR_materials_ior": {"ior": 1.5},
                            "KHR_materials_volume": {"thicknessFactor": 0.02,
                                                     "attenuationColor": [0.55, 0.85, 0.8],
                                                     "attenuationDistance": 0.05}}},
            {"name": "Lamp", "pbrMetallicRoughness": {"baseColorFactor": [0.2, 0.2, 0.2, 1.0],
                                                     "metallicFactor": 0.0, "roughnessFactor": 0.6},
             "emissiveFactor": [1.0, 0.85, 0.6], "emissiveTexture": {"index": 3},
             "extensions": {"KHR_materials_emissive_strength": {"emissiveStrength": 40.0}}},
            {"name": "Fin", "doubleSided": true, "pbrMetallicRoughness": {
                "baseColorTexture": {"index": 0}, "metallicFactor": 0.0, "roughnessFactor": 0.7}},
        ])
    }

    /// Writes the outpost glTF and its textures into `dir` (`outpost.gltf`).
    ///
    /// # Errors
    ///
    /// A file cannot be written.
    pub fn write_outpost(dir: &Path) -> Result<(), LookdevError> {
        let s = 256;
        write(&dir.join("outpost_albedo.png"), &png_bytes(s, s, &albedo(s))?)?;
        write(&dir.join("outpost_normal.png"), &png_bytes(s, s, &normal(s))?)?;
        write(&dir.join("outpost_orm.png"), &png_bytes(s, s, &orm(s))?)?;
        write(
            &dir.join("outpost_emissive.png"),
            &png_bytes(64, 64, &emissive(64))?,
        )?;
        let mut walls = Prim::default();
        walls.cuboid([-4.0, 0.0, -3.0], [4.0, 3.5, 3.0], 2.0);
        walls.cuboid([-1.5, 3.5, -1.5], [1.5, 5.0, 1.5], 2.0);
        let mut glass = Prim::default();
        // A window pane just proud of the +Z wall and a sloped skylight over the roof box.
        glass.quad([-2.8, 1.0, 3.02], [2.2, 0.0, 0.0], [0.0, 1.6, 0.0], [1.0, 1.0]);
        glass.quad([0.6, 1.0, 3.02], [2.2, 0.0, 0.0], [0.0, 1.6, 0.0], [1.0, 1.0]);
        let mut lamp = Prim::default();
        lamp.cuboid([-0.3, 2.7, 3.0], [0.3, 3.0, 3.25], 1.0);
        let mut fin = Prim::default();
        // A single-sided-geometry fin leaning off the +X wall, its front facing the wall: from outside
        // only per-material double-sided culling draws it.
        fin.quad([4.0, 0.2, -1.0], [0.0, 0.0, 2.0], [1.6, 2.6, 0.0], [1.0, 1.0]);
        write_gltf(
            dir,
            "outpost",
            &[
                ("Walls", walls, 0),
                ("Glass", glass, 1),
                ("Lamp", lamp, 2),
                ("Fin", fin, 3),
            ],
            outpost_materials(),
            &[
                "outpost_albedo.png",
                "outpost_normal.png",
                "outpost_orm.png",
                "outpost_emissive.png",
            ],
        )
    }

    /// Writes a small rock glTF (`rock.gltf`) for the scatter class.
    ///
    /// # Errors
    ///
    /// A file cannot be written.
    pub fn write_rock(dir: &Path) -> Result<(), LookdevError> {
        let mut rock = Prim::default();
        rock.cuboid([-0.4, 0.0, -0.3], [0.4, 0.5, 0.3], 1.0);
        write_gltf(
            dir,
            "rock",
            &[("Rock", rock, 0)],
            json!([{"name": "Rock", "pbrMetallicRoughness": {"baseColorFactor": [0.35, 0.3, 0.27, 1.0],
                                                              "metallicFactor": 0.0, "roughnessFactor": 0.9}}]),
            &[],
        )
    }

    /// The terrain layer file of the fixture (one ground layer).
    pub const TERRAIN_LAYERS: &str = r#"TerrainLayers(
    version: 1,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "ground",
            albedo: (0.42, 0.30, 0.22),
            roughness: 0.95,
            tiling: 8.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
        ),
    ],
)
"#;

    /// The scatter file of the fixture (format version 1; the cook migrates it).
    pub const SCATTER: &str = r#"ScatterRule(
    version: 1,
    seed: 2747,
    mesh: "props/rock.gltf",
    cell_size: 8.0,
    instances_per_cell: 0.7,
    min_scale: 0.6,
    max_scale: 1.8,
    yaw_jitter_radians: 6.283,
    align_to_normal: false,
    tilt_max_radians: 0.12,
    height_min: -1000.0,
    height_max: 1000.0,
    slope_max: 0.7,
    layer_index: None,
    layer_min_weight: 0.5,
    avoid_holes: true,
    fade_start_distance: 220.0,
    fade_end_distance: 320.0,
)
"#;

    /// Writes the whole project under `root`: `Regia.toml`, `assets/hero/outpost.gltf`,
    /// `assets/props/rock.gltf`, the terrain (`assets/ground.terrain.layers.ron`, a 64² 16-bit
    /// heightfield over 64 m, one layer albedo), `assets/rocks.scatter.ron` and
    /// `assets/levels/outpost.level.ron` (raw: levels ship beside the paks).
    ///
    /// # Errors
    ///
    /// A file cannot be written.
    pub fn write_project(root: &Path, level: &str) -> Result<(), LookdevError> {
        let assets = root.join("assets");
        write(
            &root.join("Regia.toml"),
            b"[project]\nname = \"cooked-outpost\"\nid = \"dev.regia.cooked_outpost\"\nversion = \"0.1.0\"\nbuild_number = 1\nengine = \"0.1\"\nstartup_level = \"levels/outpost.level.ron\"\norganization = \"Regia Engine\"\n\n[platforms.linux]\nformats = [\"tar\"]\n",
        )?;
        write_outpost(&assets.join("hero"))?;
        write_rock(&assets.join("props"))?;
        write(
            &assets.join("ground.terrain.layers.ron"),
            TERRAIN_LAYERS.as_bytes(),
        )?;
        let (w, h) = (64u32, 64u32);
        let samples: Vec<u16> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| (20_000.0 + 3_000.0 * noise(x * 4, y * 4, 64, 5)) as u16)
            .collect();
        write(&assets.join("heightfield.png"), &gray16_png(&samples, w, h)?)?;
        write(
            &assets.join("heightfield.json"),
            br#"{ "size_m": 64.0, "origin": [0.0, 0.0, 0.0] }"#,
        )?;
        write(
            &assets.join("detail/ground/albedo.png"),
            &png_bytes(256, 256, &albedo(256))?,
        )?;
        write(&assets.join("rocks.scatter.ron"), SCATTER.as_bytes())?;
        write(&assets.join("levels/outpost.level.ron"), level.as_bytes())
    }
}
