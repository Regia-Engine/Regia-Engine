// SPDX-License-Identifier: MIT-0
//! Temporal-upscaler motion chart (`specs/27` § 2.3).
//!
//! A flat chart one world unit per output pixel in front of the camera:
//!
//! * six bands of vertical line pairs, bar widths 1, 2, 3, 4, 6 and 8 px, each
//!   periodic and wider than the whole pan, so a band's statistics do not depend
//!   on the camera offset;
//! * a band of 1-px lattice bars (vertical every 5 px, horizontal every 6 px) in
//!   front of a far wall (twice the distance), so the lattice is thin in luma *and*
//!   in depth and the wall moves at half the pan speed (parallax);
//! * a ghosting scene: a bright box moving 4 px/frame over a striped ground.
//!
//! The camera pans 2 output px per frame. [`measure_mtf`] reads the panned frame's
//! line-pair contrast per band and its MTF50; [`measure_lattice`] counts the lattice
//! bars that keep at least half the native still's contrast; [`ghost_energy`] is the
//! residual of the box's trail 8 frames behind it.

use regia_render::{
    DirectionalLightUniform, ForwardResult, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice, TemporalUpscaler, TsrSettings,
};

use crate::LookdevError;

/// Bar widths of the line-pair bands, top to bottom (output pixels).
pub const BAR_WIDTHS: [u32; 6] = [1, 2, 3, 4, 6, 8];
/// Rows of each line-pair band (`[top, bottom)`, output pixels at 180 rows).
const BAND_ROWS: [(u32, u32); 6] = [(4, 22), (26, 44), (48, 66), (70, 88), (92, 110), (114, 132)];
/// Rows of the lattice band.
const LATTICE_ROWS: (u32, u32) = (138, 174);
/// Lattice spacing (vertical bars, horizontal bars), output pixels.
const LATTICE_PITCH: (u32, u32) = (5, 6);
/// Chart output size.
pub const CHART_SIZE: (u32, u32) = (320, 180);
const FOV_Y: f32 = 0.8;
const EXPOSURE_EV: f32 = 14.5;

/// Draw the chart GPU-driven (indirect draws, as Desktop High does) instead of the lab path.
pub static CHART_GPU_DRIVEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How a chart frame is resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChartMode {
    /// Native resolution, no jitter, no temporal pass: the reference still.
    NativeStill,
    /// Native-resolution TAA (the current TAAU at scale 1).
    NativeTaa,
    /// The current TAAU at a render scale.
    Taau(f32),
    /// The TSR-grade upscaler at a render scale.
    Tsr(f32),
    /// The FSR 3.1 upscaler (TSR's comparison baseline) at a render scale, SDK defaults.
    Fsr3(f32),
}

impl ChartMode {
    /// Short label for reports and file names.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::NativeStill => "native_still".into(),
            Self::NativeTaa => "native_taa".into(),
            Self::Taau(s) => format!("taau_{:03}", (s * 100.0).round() as u32),
            Self::Tsr(s) => format!("tsr_{:03}", (s * 100.0).round() as u32),
            Self::Fsr3(s) => format!("fsr3_{:03}", (s * 100.0).round() as u32),
        }
    }

    /// The frame quality and the upscaler this mode uses (screen-space extras off).
    #[must_use]
    pub fn quality(self) -> (FrameQuality, TemporalUpscaler) {
        let mut q = FrameQuality::LAB;
        let (taau, scale, upscaler) = match self {
            Self::NativeStill => (false, 1.0, TemporalUpscaler::Taau),
            Self::NativeTaa => (true, 1.0, TemporalUpscaler::Taau),
            Self::Taau(s) => (true, s, TemporalUpscaler::Taau),
            Self::Tsr(s) => (true, s, TemporalUpscaler::Tsr),
            Self::Fsr3(s) => (true, s, TemporalUpscaler::Fsr3),
        };
        q.taau = taau;
        q.gpu_driven = CHART_GPU_DRIVEN.load(std::sync::atomic::Ordering::Relaxed);
        q.render_scale_permille = (scale.clamp(0.05, 1.0) * 1000.0).round() as u16;
        (q, upscaler)
    }
}

fn quad(x0: f32, y0: f32, x1: f32, y1: f32, z: f32, vertices: &mut Vec<LabVertex>, indices: &mut Vec<u32>) {
    let base = vertices.len() as u32;
    let n = [0.0, 0.0, 1.0];
    vertices.push(LabVertex::from_pos_normal_uv([x0, y0, z], n, [0.0, 1.0]));
    vertices.push(LabVertex::from_pos_normal_uv([x1, y0, z], n, [1.0, 1.0]));
    vertices.push(LabVertex::from_pos_normal_uv([x1, y1, z], n, [1.0, 0.0]));
    vertices.push(LabVertex::from_pos_normal_uv([x0, y1, z], n, [0.0, 0.0]));
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn mesh(vertices: Vec<LabVertex>, indices: Vec<u32>, material: usize) -> LabMesh {
    LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

/// Camera distance that maps one world unit at `z = 0` to one output pixel.
#[must_use]
pub fn chart_distance(height: u32) -> f32 {
    height as f32 / (2.0 * (FOV_Y * 0.5).tan())
}

/// World y of the top edge of output row `row` (rows grow downward).
fn world_y(row: u32, height: u32) -> f32 {
    height as f32 * 0.5 - row as f32
}

fn base_scene(width: u32, height: u32) -> LabScene {
    let d = chart_distance(height);
    LabScene {
        width,
        height,
        eye: [0.0, 0.0, d],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: d * 0.25,
        far: d * 4.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EXPOSURE_EV,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.05, -0.1, -1.0], [1.0, 1.0, 1.0], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![
            // 0: dark backdrop.
            LabMaterial {
                base_color: [0.03, 0.03, 0.03, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 1: bright bars.
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 2: mid ground stripe.
            LabMaterial {
                base_color: [0.18, 0.15, 0.12, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 3: hot moving object.
            LabMaterial {
                base_color: [0.9, 0.85, 0.7, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
        ],
        meshes: Vec::new(),
    }
}

/// The pan chart for a camera travelling `travel` world units (= output px) to the right.
#[must_use]
pub fn pan_chart(travel: f32) -> LabScene {
    let (width, height) = CHART_SIZE;
    let mut scene = base_scene(width, height);
    let x_min = -(width as f32) - 16.0;
    let x_max = travel + width as f32 + 16.0;
    // Near backdrop behind the line pairs only (the lattice sees the far wall).
    let (mut v, mut i) = (Vec::new(), Vec::new());
    let top = world_y(0, height) + 4.0;
    let bottom = world_y(BAND_ROWS[5].1 + 3, height);
    quad(x_min, bottom, x_max, top, -1.0, &mut v, &mut i);
    scene.meshes.push(mesh(v, i, 0));
    // Far wall: twice the distance, so twice the extent.
    let d = chart_distance(height);
    let (mut v, mut i) = (Vec::new(), Vec::new());
    quad(
        2.0 * x_min - width as f32,
        -2.0 * height as f32,
        2.0 * x_max + width as f32,
        2.0 * height as f32,
        -d,
        &mut v,
        &mut i,
    );
    scene.meshes.push(mesh(v, i, 0));
    // Line pairs.
    let (mut v, mut i) = (Vec::new(), Vec::new());
    for (k, rows) in BAR_WIDTHS.iter().zip(BAND_ROWS) {
        let k = *k as f32;
        let (y_top, y_bottom) = (world_y(rows.0, height), world_y(rows.1, height));
        let mut x = (x_min / (2.0 * k)).floor() * 2.0 * k;
        while x < x_max {
            quad(x, y_bottom, x + k, y_top, 0.0, &mut v, &mut i);
            x += 2.0 * k;
        }
    }
    // Lattice (1-px members).
    let (y_top, y_bottom) = (world_y(LATTICE_ROWS.0, height), world_y(LATTICE_ROWS.1, height));
    let mut x = (x_min / LATTICE_PITCH.0 as f32).floor() * LATTICE_PITCH.0 as f32;
    while x < x_max {
        quad(x, y_bottom, x + 1.0, y_top, 0.0, &mut v, &mut i);
        x += LATTICE_PITCH.0 as f32;
    }
    let mut row = LATTICE_ROWS.0 + 3;
    while row + 1 < LATTICE_ROWS.1 {
        let y = world_y(row, height);
        quad(x_min, y - 1.0, x_max, y, 0.0, &mut v, &mut i);
        row += LATTICE_PITCH.1;
    }
    scene.meshes.push(mesh(v, i, 1));
    scene
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: EXPOSURE_EV,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

fn set_camera(scene: &mut LabScene, x: f32) {
    let d = chart_distance(scene.height);
    scene.eye = [x, 0.0, d];
    scene.target = [x, 0.0, 0.0];
}

/// Renders the pan chart for `frames` frames, the camera moving `pan_px` output
/// pixels per frame, and returns the last frame and the camera's final x.
///
/// # Errors
/// A device or frame failure.
pub fn render_pan(
    device: &RenderDevice,
    mode: ChartMode,
    frames: u32,
    pan_px: f32,
) -> Result<(ForwardResult, f32), LookdevError> {
    render_pan_with(device, mode, frames, pan_px, None)
}

/// [`render_pan`] with explicit TSR tuning (debug views, sweeps).
///
/// # Errors
/// A device or frame failure.
pub fn render_pan_with(
    device: &RenderDevice,
    mode: ChartMode,
    frames: u32,
    pan_px: f32,
    tsr: Option<TsrSettings>,
) -> Result<(ForwardResult, f32), LookdevError> {
    let travel = pan_px * frames as f32;
    let mut scene = pan_chart(travel.abs());
    let post = post();
    scene.post = Some(post.clone());
    let (quality, upscaler) = mode.quality();
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    if let Some(settings) = tsr {
        renderer.set_tsr_settings(settings);
    } else if let ChartMode::Tsr(scale) = mode {
        renderer.set_tsr_settings(TsrSettings::for_scale(scale));
    }
    let mut last = None;
    let mut x = 0.0;
    for frame in 0..frames.max(1) {
        x = pan_px * frame as f32;
        set_camera(&mut scene, x);
        let mut fs = scene.as_frame_scene_with_quality(quality);
        fs.post = Some(&post);
        last = Some(renderer.render_offscreen(device, &fs)?);
    }
    let image = last.ok_or_else(|| LookdevError::Config("no frames".into()))?;
    Ok((image, x))
}

fn luma(image: &ForwardResult, x: u32, y: u32) -> f32 {
    let i = ((y * image.width + x) * 4) as usize;
    let p = &image.pixels[i..i + 3];
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Row-averaged horizontal profile of `rows` (interior only).
fn profile(image: &ForwardResult, rows: (u32, u32)) -> Vec<f32> {
    let (r0, r1) = (rows.0 + 3, rows.1.saturating_sub(3));
    let n = (r1 - r0).max(1) as f32;
    (0..image.width)
        .map(|x| (r0..r1).map(|y| luma(image, x, y)).sum::<f32>() / n)
        .collect()
}

fn percentile(values: &mut [f32], p: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    let idx = ((values.len() - 1) as f32 * p).round() as usize;
    values[idx.min(values.len() - 1)]
}

/// Per-band line-pair modulation and MTF50 of one frame.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MtfReport {
    /// Michelson contrast per band ([`BAR_WIDTHS`] order).
    pub contrast: [f32; 6],
    /// Contrast normalised by the 8-px band.
    pub modulation: [f32; 6],
    /// Spatial frequency (cycles per output pixel) where the modulation crosses 0.5.
    pub mtf50: f32,
}

/// Measures the line-pair bands of `image` (8 px margins left and right).
#[must_use]
pub fn measure_mtf(image: &ForwardResult) -> MtfReport {
    let mut contrast = [0.0f32; 6];
    for (band, rows) in BAND_ROWS.iter().enumerate() {
        let p = profile(image, *rows);
        let mut interior: Vec<f32> = p[8..p.len() - 8].to_vec();
        let hi = percentile(&mut interior, 0.95);
        let lo = percentile(&mut interior, 0.05);
        contrast[band] = (hi - lo) / (hi + lo).max(1.0);
    }
    let reference = contrast[5].max(1.0e-3);
    let mut modulation = [0.0f32; 6];
    for (m, c) in modulation.iter_mut().zip(contrast) {
        *m = c / reference;
    }
    // Frequencies, highest first: 1 / (2k).
    let freq: Vec<f32> = BAR_WIDTHS.iter().map(|k| 1.0 / (2.0 * *k as f32)).collect();
    let mut mtf50 = freq[5];
    if modulation[0] >= 0.5 {
        mtf50 = freq[0];
    } else {
        for band in 1..6 {
            if modulation[band] >= 0.5 {
                let (m_hi, m_lo) = (modulation[band - 1], modulation[band]);
                let t = (m_lo - 0.5) / (m_lo - m_hi).max(1.0e-6);
                mtf50 = freq[band] + t * (freq[band - 1] - freq[band]);
                break;
            }
        }
    }
    MtfReport {
        contrast,
        modulation,
        mtf50,
    }
}

/// Screen column of the lattice bar at world x `bar_x` for a camera at `camera_x`.
fn bar_column(bar_x: f32, camera_x: f32, width: u32) -> i32 {
    (bar_x - camera_x + width as f32 * 0.5).floor() as i32
}

/// Per-bar contrast of the vertical lattice members (rows between the horizontal members).
fn lattice_bar_contrasts(image: &ForwardResult, camera_x: f32) -> Vec<f32> {
    let width = image.width;
    // Rows that are clear of the horizontal members.
    let rows: Vec<u32> = (LATTICE_ROWS.0 + 2..LATTICE_ROWS.1 - 2)
        .filter(|row| {
            let rel = (row + LATTICE_PITCH.1 - (LATTICE_ROWS.0 + 2)) % LATTICE_PITCH.1;
            rel > 1 && rel < LATTICE_PITCH.1 - 1
        })
        .collect();
    let column = |x: i32| -> f32 {
        let x = x.clamp(0, width as i32 - 1) as u32;
        rows.iter().map(|&y| luma(image, x, y)).sum::<f32>() / rows.len().max(1) as f32
    };
    let pitch = LATTICE_PITCH.0 as f32;
    let first = ((camera_x - width as f32 * 0.5) / pitch).floor() as i64 - 1;
    let last = ((camera_x + width as f32 * 0.5) / pitch).ceil() as i64 + 1;
    let mut out = Vec::new();
    for n in first..=last {
        let col = bar_column(n as f32 * pitch, camera_x, width);
        if col < 4 || col > width as i32 - 5 {
            continue;
        }
        let bar = column(col);
        let gap = 0.5 * (column(col - 2) + column(col + 2));
        out.push(bar - gap);
    }
    out
}

/// Fraction of lattice bars whose contrast is at least half of the native still's median bar.
#[must_use]
pub fn measure_lattice(image: &ForwardResult, camera_x: f32, native: &ForwardResult, native_x: f32) -> f32 {
    let mut reference = lattice_bar_contrasts(native, native_x);
    if reference.is_empty() {
        return 0.0;
    }
    let median = percentile(&mut reference, 0.5).max(1.0);
    let bars = lattice_bar_contrasts(image, camera_x);
    let resolved = bars.iter().filter(|c| **c >= 0.5 * median).count();
    resolved as f32 / bars.len().max(1) as f32
}

/// Ghosting scene: a 24×24 px box moving 4 px/frame over 4-px ground stripes.
fn ghost_scene(with_object: bool) -> LabScene {
    let (width, height) = CHART_SIZE;
    let mut scene = base_scene(width, height);
    let (mut v, mut i) = (Vec::new(), Vec::new());
    quad(-400.0, -200.0, 400.0, 200.0, -2.0, &mut v, &mut i);
    scene.meshes.push(mesh(v, i, 0));
    let (mut v, mut i) = (Vec::new(), Vec::new());
    let mut x = -400.0;
    while x < 400.0 {
        quad(x, -200.0, x + 4.0, 200.0, -1.0, &mut v, &mut i);
        x += 8.0;
    }
    scene.meshes.push(mesh(v, i, 2));
    if with_object {
        let (mut v, mut i) = (Vec::new(), Vec::new());
        quad(-12.0, -12.0, 12.0, 12.0, 0.0, &mut v, &mut i);
        scene.meshes.push(mesh(v, i, 3));
    }
    scene
}

/// Box centre x (world = output px offset from the screen centre) at `frame`.
fn box_x(frame: u32) -> f32 {
    -120.0 + 4.0 * frame as f32
}

/// Trail residual and ghost bias 8 frames behind the moving box, as fractions of
/// the box/ground contrast, worst over the sampled frames, and the last sampled frame.
///
/// # Errors
/// A device or frame failure.
pub fn ghost_energy(
    device: &RenderDevice,
    mode: ChartMode,
    frames: u32,
    tsr: Option<TsrSettings>,
) -> Result<(GhostReport, Option<ForwardResult>), LookdevError> {
    let post = post();
    let (quality, upscaler) = mode.quality();
    let mut last = None;
    let mut with = ghost_scene(true);
    let mut without = ghost_scene(false);
    with.post = Some(post.clone());
    without.post = Some(post.clone());
    let mut r_with = FrameRenderer::new();
    let mut r_without = FrameRenderer::new();
    r_with.set_temporal_upscaler(upscaler);
    r_without.set_temporal_upscaler(upscaler);
    if let ChartMode::Tsr(scale) = mode {
        let settings = tsr.unwrap_or_else(|| TsrSettings::for_scale(scale));
        r_with.set_tsr_settings(settings);
        r_without.set_tsr_settings(settings);
    }
    let (width, height) = CHART_SIZE;
    let mut worst = 0.0f32;
    let mut worst_bias = 0.0f32;
    for frame in 0..frames {
        let x = box_x(frame);
        if let Some(object) = with.meshes.get_mut(2) {
            object.model = [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [x, 0.0, 0.0, 1.0],
            ];
        }
        let mut fs = with.as_frame_scene_with_quality(quality);
        fs.post = Some(&post);
        let a = r_with.render_offscreen(device, &fs)?;
        let mut fs = without.as_frame_scene_with_quality(quality);
        fs.post = Some(&post);
        let b = r_without.render_offscreen(device, &fs)?;
        if frame < 16 || frame % 8 != 0 {
            continue;
        }
        // Object/ground contrast, from the object's interior now.
        let cx = (x + width as f32 * 0.5) as i32;
        let cy = (height / 2) as i32;
        let mut object = 0.0;
        let mut ground = 0.0;
        let mut n = 0.0;
        for dy in -6..=6 {
            for dx in -6..=6 {
                let (px, py) = ((cx + dx) as u32, (cy + dy) as u32);
                object += luma(&a, px, py);
                ground += luma(&b, px, py);
                n += 1.0;
            }
        }
        let contrast = ((object - ground) / n).abs().max(1.0);
        // Trail: where the box was 8 frames ago, minus its current footprint (+2 px).
        let old = box_x(frame - 8) + width as f32 * 0.5;
        let now = x + width as f32 * 0.5;
        let mut residual = 0.0;
        let mut bias = 0.0;
        let mut count = 0.0;
        for py in (cy - 10)..=(cy + 10) {
            for px in ((old - 10.0) as i32)..=((old + 10.0) as i32) {
                if (px as f32 - now).abs() < 14.0 || px < 0 || px >= width as i32 {
                    continue;
                }
                let d = luma(&a, px as u32, py as u32) - luma(&b, px as u32, py as u32);
                residual += d.abs();
                bias += d;
                count += 1.0;
            }
        }
        if count > 0.0 {
            worst = worst.max(residual / count / contrast);
            // Signed toward the object (it is brighter than the ground).
            worst_bias = worst_bias.max(bias / count / contrast);
        }
        last = Some(a);
    }
    Ok((
        GhostReport {
            residual: worst,
            bias: worst_bias,
        },
        last,
    ))
}

/// Trail measurements 8 frames behind the moving box, worst over the sampled frames.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct GhostReport {
    /// Mean |frame − object-free frame| in the trail, over the object's contrast.
    /// Counts the re-accumulation noise of the uncovered ground as well as ghosting.
    pub residual: f32,
    /// Mean signed difference toward the object's colour, over its contrast: the
    /// part of the trail that is a copy of the object (the ghost itself).
    pub bias: f32,
}

/// All § 2.3 numbers for one mode.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MotionReport {
    /// Mode label.
    pub mode: String,
    /// Line pairs while panning.
    pub pan: MtfReport,
    /// Lattice bars resolved while panning (0–1, vs the native still).
    pub lattice: f32,
    /// Trail 8 frames behind the moving box (fractions of its contrast).
    pub ghost: GhostReport,
    /// The last measured frame of the ghosting scene.
    #[serde(skip)]
    pub ghost_frame: Option<ForwardResult>,
}

/// Renders the pan and the ghost scene for `mode` and measures them against `native`.
///
/// # Errors
/// A device or frame failure.
pub fn measure_mode(
    device: &RenderDevice,
    mode: ChartMode,
    frames: u32,
    native: &(ForwardResult, f32),
    tsr: Option<TsrSettings>,
) -> Result<(MotionReport, ForwardResult), LookdevError> {
    let (image, x) = render_pan_with(device, mode, frames, 2.0, tsr)?;
    let pan = measure_mtf(&image);
    let lattice = measure_lattice(&image, x, &native.0, native.1);
    let (ghost, ghost_frame) = ghost_energy(device, mode, 64, tsr)?;
    Ok((
        MotionReport {
            mode: mode.label(),
            pan,
            lattice,
            ghost,
            ghost_frame,
        },
        image,
    ))
}

/// Texel densities (texels per output pixel) of the three textured-plane columns, left
/// to right: native picks mips 0, ~0.7 and ~1.6, a 0.67 render without a mip bias
/// ~0.6, ~1.3 and ~2.2.
pub const TEXTURE_DENSITIES: [f32; 3] = [1.0, 1.6, 3.0];
const TEXTURE_SIZE: u32 = 256;

/// Deterministic 256² grey texture with detail at every scale: one-texel hash noise
/// over 4- and 16-texel value blobs (sRGB-encoded bytes).
#[must_use]
pub fn detail_texture() -> LabTexture {
    fn hash(x: u32, y: u32, seed: u32) -> f32 {
        let mut h =
            x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2c1b_3c6d);
        h ^= h >> 12;
        (h & 0xffff) as f32 / 65535.0
    }
    let n = TEXTURE_SIZE;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let fine = hash(x, y, 1);
            let mid = hash(x / 4, y / 4, 2);
            let coarse = hash(x / 16, y / 16, 3);
            let v = 0.5 * fine + 0.3 * mid + 0.2 * coarse;
            let byte = (40.0 + 180.0 * v).round() as u8;
            rgba.extend_from_slice(&[byte, byte, byte, 255]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

/// A fronto-parallel plane filling the chart, split into three columns that map
/// [`TEXTURE_DENSITIES`] texels of [`detail_texture`] to each output pixel.
#[must_use]
pub fn texture_plane() -> LabScene {
    let (width, height) = CHART_SIZE;
    let mut scene = base_scene(width, height);
    scene.materials.push(LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.9,
        albedo: Some(detail_texture()),
        ..LabMaterial::default()
    });
    let material = scene.materials.len() - 1;
    let column = width as f32 / 3.0;
    let (top, bottom) = (height as f32 * 0.5 + 4.0, -(height as f32) * 0.5 - 4.0);
    for (k, density) in TEXTURE_DENSITIES.iter().enumerate() {
        let x0 = -(width as f32) * 0.5 + column * k as f32;
        let x1 = x0 + column;
        // u, v in texture repeats: `density` texels per world unit (= output pixel).
        let s = density / TEXTURE_SIZE as f32;
        let n = [0.0, 0.0, 1.0];
        let vertices = vec![
            LabVertex::from_pos_normal_uv([x0, bottom, 0.0], n, [x0 * s, -bottom * s]),
            LabVertex::from_pos_normal_uv([x1, bottom, 0.0], n, [x1 * s, -bottom * s]),
            LabVertex::from_pos_normal_uv([x1, top, 0.0], n, [x1 * s, -top * s]),
            LabVertex::from_pos_normal_uv([x0, top, 0.0], n, [x0 * s, -top * s]),
        ];
        scene
            .meshes
            .push(mesh(vertices, vec![0, 1, 2, 0, 2, 3], material));
    }
    scene
}

/// Renders [`texture_plane`] with a still camera for `frames` frames and returns the last.
///
/// # Errors
/// A device or frame failure.
pub fn render_texture_plane(
    device: &RenderDevice,
    mode: ChartMode,
    frames: u32,
    tsr: Option<TsrSettings>,
) -> Result<(ForwardResult, f32), LookdevError> {
    let mut scene = texture_plane();
    let post = post();
    scene.post = Some(post.clone());
    let (quality, upscaler) = mode.quality();
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    if let Some(settings) = tsr {
        renderer.set_tsr_settings(settings);
    } else if let ChartMode::Tsr(scale) = mode {
        renderer.set_tsr_settings(TsrSettings::for_scale(scale));
    }
    let mut last = None;
    for _ in 0..frames.max(1) {
        let mut fs = scene.as_frame_scene_with_quality(quality);
        fs.post = Some(&post);
        last = Some(renderer.render_offscreen(device, &fs)?);
    }
    let image = last.ok_or_else(|| LookdevError::Config("no frames".into()))?;
    Ok((image, renderer.texture_lod_bias()))
}

/// Texture detail per [`TEXTURE_DENSITIES`] column: the mean absolute difference between
/// each pixel's luma and its 3×3 neighbourhood mean (8 px margins), in 8-bit luma units.
#[must_use]
pub fn measure_texture_detail(image: &ForwardResult) -> [f32; 3] {
    let column = image.width / 3;
    let mut out = [0.0f32; 3];
    for (k, value) in out.iter_mut().enumerate() {
        let (x0, x1) = (k as u32 * column + 8, (k as u32 + 1) * column - 8);
        let (y0, y1) = (8, image.height - 8);
        let (mut sum, mut count) = (0.0f32, 0u32);
        for y in y0..y1 {
            for x in x0..x1 {
                let mut mean = 0.0;
                for dy in 0..3 {
                    for dx in 0..3 {
                        mean += luma(image, x + dx - 1, y + dy - 1);
                    }
                }
                sum += (luma(image, x, y) - mean / 9.0).abs();
                count += 1;
            }
        }
        *value = sum / count.max(1) as f32;
    }
    out
}

/// Alpha mask of the mask chart's left half: slanted stripes 8 texels wide (64² texels,
/// magnified 2 px per texel), so every edge is a long shallow diagonal.
fn slanted_mask() -> LabTexture {
    let n = 64u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let t = ((x as f32 - 0.35 * y as f32) / 16.0).rem_euclid(1.0);
            let a = if t < 0.5 { 255 } else { 0 };
            rgba.extend_from_slice(&[230, 230, 230, a]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

/// Alpha mask of the right half: one-texel vertical strands every third texel (63²
/// texels, minified to 2.7 texels per output pixel): a third of the area is covered, and
/// the mip chain averages the strands toward alpha ⅓ (below the ½ cutoff).
fn strand_mask() -> LabTexture {
    let n = 63u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for _ in 0..n {
        for x in 0..n {
            let a = if x % 3 == 0 { 255 } else { 0 };
            rgba.extend_from_slice(&[230, 230, 230, a]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

/// The alpha-tested chart: a dark backdrop and, in front of it, a masked plane whose
/// left half is `slanted_mask` (magnified edges) and right half `strand_mask`
/// (minified strands). `supersample` renders the same framing at that many times the
/// chart's resolution (the reference).
#[must_use]
pub fn mask_chart(supersample: u32) -> LabScene {
    let (width, height) = CHART_SIZE;
    let mut scene = base_scene(width, height);
    scene.width = width * supersample.max(1);
    scene.height = height * supersample.max(1);
    scene.double_sided = true;
    let (top, bottom) = (height as f32 * 0.5 + 4.0, -(height as f32) * 0.5 - 4.0);
    let (left, right) = (-(width as f32) * 0.5 - 4.0, width as f32 * 0.5 + 4.0);
    let (mut v, mut i) = (Vec::new(), Vec::new());
    quad(left, bottom, right, top, -2.0, &mut v, &mut i);
    scene.meshes.push(mesh(v, i, 0));
    for (k, (texture, texels_per_px, size)) in [(slanted_mask(), 0.5f32, 64.0f32), (strand_mask(), 2.7, 63.0)]
        .into_iter()
        .enumerate()
    {
        scene.materials.push(LabMaterial {
            base_color: [1.0, 1.0, 1.0, 1.0],
            roughness: 0.9,
            alpha_mode: regia_render::ALPHA_MASK,
            alpha_cutoff: 0.5,
            albedo: Some(texture),
            ..LabMaterial::default()
        });
        let material = scene.materials.len() - 1;
        let x0 = if k == 0 { left } else { 0.0 };
        let x1 = if k == 0 { 0.0 } else { right };
        let s = texels_per_px / size;
        let n = [0.0, 0.0, 1.0];
        let vertices = vec![
            LabVertex::from_pos_normal_uv([x0, bottom, 0.0], n, [x0 * s, -bottom * s]),
            LabVertex::from_pos_normal_uv([x1, bottom, 0.0], n, [x1 * s, -bottom * s]),
            LabVertex::from_pos_normal_uv([x1, top, 0.0], n, [x1 * s, -top * s]),
            LabVertex::from_pos_normal_uv([x0, top, 0.0], n, [x0 * s, -top * s]),
        ];
        scene
            .meshes
            .push(mesh(vertices, vec![0, 1, 2, 0, 2, 3], material));
    }
    scene
}

/// Renders [`mask_chart`] with a still camera for `frames` frames and returns the last.
///
/// # Errors
/// A device or frame failure.
pub fn render_mask_chart(
    device: &RenderDevice,
    mode: ChartMode,
    supersample: u32,
    frames: u32,
    tsr: Option<TsrSettings>,
) -> Result<ForwardResult, LookdevError> {
    let mut scene = mask_chart(supersample);
    let post = post();
    scene.post = Some(post.clone());
    let (quality, upscaler) = mode.quality();
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    if let Some(settings) = tsr {
        renderer.set_tsr_settings(settings);
    } else if let ChartMode::Tsr(scale) = mode {
        renderer.set_tsr_settings(TsrSettings::for_scale(scale));
    }
    let mut last = None;
    for _ in 0..frames.max(1) {
        let mut fs = scene.as_frame_scene_with_quality(quality);
        fs.post = Some(&post);
        last = Some(renderer.render_offscreen(device, &fs)?);
    }
    last.ok_or_else(|| LookdevError::Config("no frames".into()))
}

/// Box-downsamples `image` by `factor` (a supersampled reference to chart resolution).
#[must_use]
pub fn downsample(image: &ForwardResult, factor: u32) -> ForwardResult {
    let factor = factor.max(1);
    let (w, h) = (image.width / factor, image.height / factor);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..4 {
                let mut sum = 0u32;
                for dy in 0..factor {
                    for dx in 0..factor {
                        let i = (((y * factor + dy) * image.width + x * factor + dx) * 4 + c) as usize;
                        sum += u32::from(image.pixels[i]);
                    }
                }
                pixels.push(((sum + factor * factor / 2) / (factor * factor)) as u8);
            }
        }
    }
    ForwardResult {
        width: w,
        height: h,
        pixels,
    }
}

/// Mean absolute luma error of `image` against `reference` over the mask chart's two
/// halves `[slanted edges, strands]` (8 px margins), 8-bit luma units.
#[must_use]
pub fn mask_error(image: &ForwardResult, reference: &ForwardResult) -> [f32; 2] {
    let mut out = [0.0f32; 2];
    let half = image.width / 2;
    for (k, value) in out.iter_mut().enumerate() {
        let (x0, x1) = (k as u32 * half + 8, (k as u32 + 1) * half - 8);
        let (mut sum, mut count) = (0.0f32, 0u32);
        for y in 8..image.height - 8 {
            for x in x0..x1 {
                sum += (luma(image, x, y) - luma(reference, x, y)).abs();
                count += 1;
            }
        }
        *value = sum / count.max(1) as f32;
    }
    out
}
