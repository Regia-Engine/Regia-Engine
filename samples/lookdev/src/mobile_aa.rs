// SPDX-License-Identifier: MIT-0
//! The non-temporal AA and upscale chart of the mobile-aa lane: SMAA 1× and SGSR 1 against
//! FXAA and the bilinear upsample (`specs/27` § 2.3 thin-geometry row, `specs/13` § 3).
//!
//! The chart is a lattice tower silhouetted against a bright backdrop, one world unit per
//! output pixel: [`LEGS`] 1-px legs, each leaning by its own small angle (so every leg has
//! a different stair-step pattern), crossed by 1-px rungs every [`RUNG_PITCH`] rows. A
//! "thin tower" in DO27's wide shots is exactly this: dark 1-px members against the sky.
//!
//! The reference is the same chart rendered at 4× the output size without AA and box-filtered
//! down in linear light (16 samples per pixel). [`measure`] compares each configuration with it:
//!
//! * **Lattice bars resolved:** per leg and per sampled row (rows clear of the rungs), the leg's
//!   contrast is the backdrop luma halfway to the next leg minus the darkest pixel within
//!   1.5 px of the leg's analytic centre. A leg is resolved when the 20th percentile over its
//!   rows of (contrast / reference contrast) is at least 0.5: a leg that is broken into dashes
//!   in more than a fifth of its rows, or blurred to less than half its contrast, is lost.
//! * **SSIM** of the whole chart against the reference ([`regia_render::compare::ssim`]).
//!
//! Every configuration is rendered at four sub-pixel camera offsets (0, ¼, ½, ¾ px) and the
//! numbers are pooled, so a result does not depend on one lucky phase.

use regia_render::{
    DirectionalLightUniform, ForwardResult, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, SpatialUpscaler,
};

use crate::LookdevError;

/// Chart output size.
pub const CHART_SIZE: (u32, u32) = (320, 180);
/// Number of tower legs.
pub const LEGS: usize = 24;
/// Horizontal spacing of the legs, output pixels.
pub const LEG_PITCH: f32 = 12.0;
/// Vertical spacing of the rungs, output pixels.
pub const RUNG_PITCH: u32 = 24;
/// Rows the tower spans (`[top, bottom)`).
const TOWER_ROWS: (u32, u32) = (14, 166);
/// Supersampling factor of the reference (per axis).
const REFERENCE_SCALE: u32 = 4;
/// Sub-pixel camera offsets every configuration is rendered at.
pub const PHASES: [f32; 4] = [0.0, 0.25, 0.5, 0.75];
const FOV_Y: f32 = 0.8;
const EXPOSURE_EV: f32 = 14.5;

/// One way to anti-alias and upscale the chart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AaConfig {
    /// Short label for reports and file names.
    pub label: &'static str,
    /// Internal render scale (1 = native).
    pub scale: f32,
    /// FXAA after the tonemap.
    pub fxaa: bool,
    /// SMAA 1× after the tonemap.
    pub smaa: bool,
    /// How a scaled frame reaches the output.
    pub upscaler: SpatialUpscaler,
}

impl AaConfig {
    const fn new(label: &'static str, scale: f32, fxaa: bool, smaa: bool, upscaler: SpatialUpscaler) -> Self {
        Self {
            label,
            scale,
            fxaa,
            smaa,
            upscaler,
        }
    }

    /// Native, no AA.
    pub const NATIVE_NONE: Self = Self::new("native_none", 1.0, false, false, SpatialUpscaler::Bilinear);
    /// Native with FXAA (the baseline of the thin-geometry criterion).
    pub const NATIVE_FXAA: Self = Self::new("native_fxaa", 1.0, true, false, SpatialUpscaler::Bilinear);
    /// Native with SMAA 1×.
    pub const NATIVE_SMAA: Self = Self::new("native_smaa", 1.0, false, true, SpatialUpscaler::Bilinear);
    /// The previous Mobile High: 85 %, bilinear HDR upsample, FXAA at the output size.
    pub const MOBILE_HIGH_BEFORE: Self =
        Self::new("bilinear_fxaa_085", 0.85, true, false, SpatialUpscaler::Bilinear);
    /// 75 % with the bilinear upsample and FXAA (what 0.75 cost before SGSR).
    pub const BILINEAR_FXAA_075: Self =
        Self::new("bilinear_fxaa_075", 0.75, true, false, SpatialUpscaler::Bilinear);
    /// 75 % with SGSR, no AA.
    pub const SGSR_075: Self = Self::new("sgsr_075", 0.75, false, false, SpatialUpscaler::Sgsr);
    /// 75 % with SMAA at the internal size and SGSR to the output (the scale the brief asked for).
    pub const SGSR_SMAA_075: Self = Self::new("sgsr_smaa_075", 0.75, false, true, SpatialUpscaler::Sgsr);
    /// The new Mobile High: 85 %, SMAA at the internal size, SGSR to the output.
    pub const MOBILE_HIGH: Self = Self::new("sgsr_smaa_085", 0.85, false, true, SpatialUpscaler::Sgsr);
    /// Mobile Low: 70 %, no AA, SGSR to the output.
    pub const MOBILE_LOW: Self = Self::new("sgsr_070", 0.7, false, false, SpatialUpscaler::Sgsr);

    /// Every configuration [`measure_all`] reports, in order.
    pub const ALL: [Self; 9] = [
        Self::NATIVE_NONE,
        Self::NATIVE_FXAA,
        Self::NATIVE_SMAA,
        Self::MOBILE_HIGH_BEFORE,
        Self::BILINEAR_FXAA_075,
        Self::SGSR_075,
        Self::SGSR_SMAA_075,
        Self::MOBILE_HIGH,
        Self::MOBILE_LOW,
    ];

    fn quality(self) -> FrameQuality {
        let mut q = FrameQuality::LAB;
        q.render_scale_permille = (self.scale.clamp(0.05, 1.0) * 1000.0).round() as u16;
        q.smaa = self.smaa;
        q.spatial_upscaler = self.upscaler;
        q
    }
}

/// What [`measure`] found for one configuration.
#[derive(Debug, Clone)]
pub struct AaResult {
    /// The configuration.
    pub config: AaConfig,
    /// Fraction of legs resolved, pooled over [`PHASES`].
    pub bars_resolved: f32,
    /// Mean leg contrast relative to the reference, pooled.
    pub mean_contrast: f32,
    /// Mean SSIM against the reference over [`PHASES`].
    pub ssim: f32,
    /// The frame at phase 0 (RGBA8, display-encoded).
    pub image: ForwardResult,
}

/// Lean of leg `k` in radians: from −6° to +6°, never exactly 0 or 45° (no pixel-aligned leg).
fn lean(k: usize) -> f32 {
    let t = k as f32 / (LEGS - 1) as f32;
    (-6.0 + 12.0 * t + 0.37).to_radians()
}

/// World x of leg `k`'s centre at world height `y`.
fn leg_x(k: usize, y: f32) -> f32 {
    let x0 = (k as f32 - (LEGS as f32 - 1.0) * 0.5) * LEG_PITCH;
    x0 + y * lean(k).tan()
}

/// World y of the top edge of output row `row` (rows grow downward).
fn world_y(row: f32, height: u32) -> f32 {
    height as f32 * 0.5 - row
}

/// Camera distance that maps one world unit at `z = 0` to one output pixel of `height` rows.
fn chart_distance(height: u32) -> f32 {
    height as f32 / (2.0 * (FOV_Y * 0.5).tan())
}

fn quad(corners: [[f32; 2]; 4], z: f32, vertices: &mut Vec<LabVertex>, indices: &mut Vec<u32>) {
    let base = vertices.len() as u32;
    let n = [0.0, 0.0, 1.0];
    for (c, uv) in corners
        .iter()
        .zip([[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]])
    {
        vertices.push(LabVertex::from_pos_normal_uv([c[0], c[1], z], n, uv));
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// The lattice tower chart for an output of `width × height` (the reference renders at 4×
/// with the same world: the camera and FOV make one world unit `height / 180` pixels).
#[must_use]
pub fn chart(width: u32, height: u32, camera_x: f32) -> LabScene {
    // The world is laid out in output pixels of the 320×180 chart.
    let (cw, ch) = CHART_SIZE;
    let d = chart_distance(ch);
    let mut backdrop = (Vec::new(), Vec::new());
    let (x0, x1) = (-(cw as f32), cw as f32);
    let (y0, y1) = (-(ch as f32), ch as f32);
    quad(
        [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
        -d,
        &mut backdrop.0,
        &mut backdrop.1,
    );
    // The backdrop sits at twice the distance, so twice the extent covers the view.
    for v in &mut backdrop.0 {
        v.position[0] *= 2.0;
        v.position[1] *= 2.0;
    }
    let mut tower = (Vec::new(), Vec::new());
    let (top, bottom) = (world_y(TOWER_ROWS.0 as f32, ch), world_y(TOWER_ROWS.1 as f32, ch));
    for k in 0..LEGS {
        let (xt, xb) = (leg_x(k, top), leg_x(k, bottom));
        quad(
            [
                [xb - 0.5, bottom],
                [xb + 0.5, bottom],
                [xt + 0.5, top],
                [xt - 0.5, top],
            ],
            0.0,
            &mut tower.0,
            &mut tower.1,
        );
    }
    let (left, right) = (leg_x(0, 0.0) - 4.0, leg_x(LEGS - 1, 0.0) + 4.0);
    let mut row = TOWER_ROWS.0 + RUNG_PITCH / 2;
    while row + 1 < TOWER_ROWS.1 {
        let y = world_y(row as f32, ch);
        quad(
            [[left, y - 1.0], [right, y - 1.0], [right, y], [left, y]],
            0.0,
            &mut tower.0,
            &mut tower.1,
        );
        row += RUNG_PITCH;
    }
    let mesh = |(vertices, indices): (Vec<LabVertex>, Vec<u32>), material| LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    };
    LabScene {
        width,
        height,
        eye: [camera_x, 0.0, d],
        target: [camera_x, 0.0, 0.0],
        fov_y: FOV_Y,
        near: d * 0.25,
        far: d * 4.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EXPOSURE_EV,
        ambient_lux: 5_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.05, -0.1, -1.0], [1.0, 1.0, 1.0], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![
            // 0: bright sky-like backdrop.
            LabMaterial {
                base_color: [0.75, 0.78, 0.82, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 1: dark steel members.
            LabMaterial {
                base_color: [0.03, 0.03, 0.03, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![mesh(backdrop, 0), mesh(tower, 1)],
    }
}

fn post(fxaa: bool) -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: EXPOSURE_EV,
        bloom: false,
        taa: false,
        fxaa,
        ..PostSettings::golden_deterministic()
    }
}

fn render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    config: AaConfig,
    camera_x: f32,
) -> Result<ForwardResult, LookdevError> {
    let (w, h) = CHART_SIZE;
    let scene = chart(w, h, camera_x);
    let post = post(config.fxaa);
    let mut fs = scene.as_frame_scene_with_quality(config.quality());
    fs.post = Some(&post);
    Ok(renderer.render_offscreen(device, &fs)?)
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// The 4× supersampled reference at `camera_x`, box-filtered in linear light, as RGB 0–1.
///
/// # Errors
/// A device or frame failure.
pub fn reference(device: &RenderDevice, camera_x: f32) -> Result<Vec<[f32; 3]>, LookdevError> {
    let (w, h) = CHART_SIZE;
    let s = REFERENCE_SCALE;
    let scene = chart(w * s, h * s, camera_x);
    let post = post(false);
    let mut fs = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    fs.post = Some(&post);
    let big = FrameRenderer::new().render_offscreen(device, &fs)?;
    let mut out = vec![[0.0f32; 3]; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            for sy in 0..s {
                for sx in 0..s {
                    let i = (((y * s + sy) * big.width + x * s + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += srgb_to_linear(f32::from(big.pixels[i + c]) / 255.0);
                    }
                }
            }
            let n = (s * s) as f32;
            out[(y * w + x) as usize] = acc.map(|a| linear_to_srgb(a / n));
        }
    }
    Ok(out)
}

fn rgb(image: &ForwardResult) -> Vec<[f32; 3]> {
    image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Rows sampled for the leg contrast: inside the tower, at least 3 rows from any rung.
fn sample_rows() -> Vec<u32> {
    let first_rung = TOWER_ROWS.0 + RUNG_PITCH / 2;
    (TOWER_ROWS.0 + 3..TOWER_ROWS.1 - 3)
        .filter(|row| {
            // Distance in rows to the nearest rung (rungs repeat every RUNG_PITCH rows).
            let rel = (row + RUNG_PITCH - first_rung % RUNG_PITCH) % RUNG_PITCH;
            rel.min(RUNG_PITCH - rel) > 3
        })
        .collect()
}

/// Per-leg, per-row contrast (backdrop minus darkest leg pixel), display luma 0–1.
fn leg_contrasts(image: &[[f32; 3]], camera_x: f32) -> Vec<Vec<f32>> {
    let (w, h) = CHART_SIZE;
    let at = |x: i32, y: u32| luma(image[(y * w + x.clamp(0, w as i32 - 1) as u32) as usize]);
    let rows = sample_rows();
    (0..LEGS)
        .map(|k| {
            rows.iter()
                .map(|&row| {
                    // Pixel x of the leg centre at the centre of this row.
                    let cx = leg_x(k, world_y(row as f32 + 0.5, h)) - camera_x + w as f32 * 0.5;
                    let lo = (cx - 1.5).floor() as i32;
                    let hi = (cx + 1.5).ceil() as i32;
                    let bar = (lo..=hi).map(|x| at(x, row)).fold(f32::INFINITY, f32::min);
                    let half = LEG_PITCH * 0.5;
                    let gap =
                        0.5 * (at((cx - half).floor() as i32, row) + at((cx + half).floor() as i32, row));
                    (gap - bar).max(0.0)
                })
                .collect()
        })
        .collect()
}

fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let idx = ((values.len() - 1) as f32 * p).round() as usize;
    values[idx.min(values.len() - 1)]
}

/// Measures `config` against the references (one per [`PHASES`] entry, from [`reference()`]).
///
/// # Errors
/// A device or frame failure.
pub fn measure(
    device: &RenderDevice,
    config: AaConfig,
    references: &[Vec<[f32; 3]>],
) -> Result<AaResult, LookdevError> {
    let (w, h) = CHART_SIZE;
    let mut renderer = FrameRenderer::new();
    let (mut resolved, mut legs, mut ratio_sum, mut ratio_n, mut ssim_sum) =
        (0usize, 0usize, 0.0f64, 0usize, 0.0f64);
    let mut first = None;
    for (phase, reference) in PHASES.iter().zip(references) {
        let image = render(device, &mut renderer, config, *phase)?;
        let pixels = rgb(&image);
        ssim_sum += f64::from(regia_render::compare::ssim(&pixels, reference, w, h));
        let got = leg_contrasts(&pixels, *phase);
        let want = leg_contrasts(reference, *phase);
        for (g, r) in got.iter().zip(&want) {
            let mut ratios: Vec<f32> = g.iter().zip(r).map(|(g, r)| g / r.max(1.0e-3)).collect();
            ratio_sum += ratios.iter().map(|v| f64::from(*v)).sum::<f64>();
            ratio_n += ratios.len();
            if percentile(&mut ratios, 0.2) >= 0.5 {
                resolved += 1;
            }
            legs += 1;
        }
        if first.is_none() {
            first = Some(image);
        }
    }
    let image = first.ok_or_else(|| LookdevError::Config("no phases".into()))?;
    Ok(AaResult {
        config,
        bars_resolved: resolved as f32 / legs.max(1) as f32,
        mean_contrast: (ratio_sum / ratio_n.max(1) as f64) as f32,
        ssim: (ssim_sum / PHASES.len() as f64) as f32,
        image,
    })
}

/// [`measure`] for every configuration in [`AaConfig::ALL`], plus the phase-0 reference as RGBA8.
///
/// # Errors
/// A device or frame failure.
pub fn measure_all(device: &RenderDevice) -> Result<(Vec<AaResult>, Vec<u8>), LookdevError> {
    let references = PHASES
        .iter()
        .map(|phase| reference(device, *phase))
        .collect::<Result<Vec<_>, _>>()?;
    let results = AaConfig::ALL
        .iter()
        .map(|config| measure(device, *config, &references))
        .collect::<Result<Vec<_>, _>>()?;
    let reference_rgba = references
        .first()
        .map(|r| {
            r.iter()
                .flat_map(|c| {
                    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                    [q(c[0]), q(c[1]), q(c[2]), 255]
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((results, reference_rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legs_lean_and_none_is_pixel_aligned() {
        for k in 0..LEGS {
            let deg = lean(k).to_degrees();
            assert!(deg.abs() > 0.1 && deg.abs() < 7.0, "leg {k} leans {deg}°");
        }
        assert!(lean(0) < 0.0 && lean(LEGS - 1) > 0.0);
    }

    #[test]
    fn sampled_rows_avoid_the_rungs() {
        let rows = sample_rows();
        assert!(rows.len() > 60, "{} rows", rows.len());
        let mut rung = TOWER_ROWS.0 + RUNG_PITCH / 2;
        while rung < TOWER_ROWS.1 {
            assert!(rows.iter().all(|r| r.abs_diff(rung) > 3), "row near rung {rung}");
            rung += RUNG_PITCH;
        }
    }

    #[test]
    fn legs_fit_inside_the_chart() {
        let (w, h) = CHART_SIZE;
        for k in 0..LEGS {
            for row in [TOWER_ROWS.0, TOWER_ROWS.1] {
                let x = leg_x(k, world_y(row as f32, h)) + w as f32 * 0.5;
                assert!(x > 4.0 && x < w as f32 - 5.0, "leg {k} at x {x}");
            }
        }
    }
}
