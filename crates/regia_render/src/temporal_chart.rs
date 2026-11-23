// SPDX-License-Identifier: MPL-2.0
//! The § 2.3 temporal chart (`specs/27` § 2.3; agent method `seeing.temporal_chart`,
//! `book/agents/temporal-chart.md`, ADR-2638).
//!
//! Renders a deterministic chart scene through the real frame (lavapipe or a card) with a
//! temporal resolve ([`crate::TemporalUpscaler`] at a render scale) and returns the five
//! numbers spec 27 § 2.3 judges a temporal upscaler on ([`ChartReport`]):
//!
//! 1. **Motion clarity**: MTF50 of the line-pair bands in the last frame of a pan
//!    (`px_per_frame` output pixels per frame) or an orbit (`deg_per_frame` around the
//!    chart's vertical axis, ending head-on), over the MTF50 of the native, un-jittered
//!    still of the same pose. Target ≥ 0.85.
//! 2. **Thin geometry**: the share of 1 px lattice bars (vertical members, in front of a far
//!    wall, so thin in luma and in depth) that keep at least half the native still's median
//!    bar contrast. Target ≥ 0.90.
//! 3. **Ghosting**: 8 frames behind a bright box moving [`GHOST_SPEED_PX`] px/frame over a
//!    striped ground, the mean signed difference toward the box between the frame and the
//!    same sequence rendered without the box (same upscaler), over the box/ground contrast;
//!    worst sampled frame (ADR-077 gates the signed bias; the absolute residual, which also
//!    counts the uncovered ground's re-accumulation noise, is reported beside it). Target ≤ 0.02.
//! 4. **Disocclusion**: behind the box's trailing edge, the length in output pixels of the
//!    run of pixels whose FLIP ([`crate::compare::flip`]) against the native still of the
//!    same pose exceeds by at least [`SMEAR_FLIP`] (a visible excess) both the box-free
//!    sequence's FLIP and a history-free box-free frame's (same jitter) against their native
//!    still: the box's colour dragged over the ground it uncovered, not the upscaler's
//!    single-frame softness. Median over the box rows, worst sampled frame. Target ≤ 2 px.
//! 5. **Stability**: a held chart's mean frame-to-frame FLIP over the last 8 frame pairs
//!    after [`STABILITY_FRAMES`] frames. Target ≤ 0.01.
//!
//! One world unit at the chart plane is one output pixel, and every bar edge sits on a pixel
//! edge at the scored pose, so the native still is not aliased and its MTF is 1 on every band.
//!
//! ## Provenance
//! Original code; the scene and the MTF/lattice/ghost measures were first written for the
//! lookdev sample's motion chart (`samples/lookdev/src/tsr_motion.rs`, ADR-077) and are moved
//! here so the renderer's tests and the agent protocol share them. MTF50: the spatial frequency
//! where the modulation transfer crosses 0.5 (ISO 12233 uses the same summary on slanted-edge
//! SFRs; here the modulation is read off line-pair bands). FLIP: Andersson et al., "FLIP: A
//! Difference Evaluator for Alternating Images", HPG 2020 (`compare.rs`).

use crate::compare::{DEFAULT_PPD, flip};
use crate::{
    DirectionalLightUniform, ForwardResult, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderError, TemporalUpscaler, TsrSettings,
};

/// Chart output size (pixels).
pub const CHART_SIZE: (u32, u32) = (320, 180);
/// Bar widths of the line-pair bands, top to bottom (output pixels).
pub const BAR_WIDTHS: [u32; 6] = [1, 2, 3, 4, 6, 8];
/// Rows of each line-pair band (`[top, bottom)`).
const BAND_ROWS: [(u32, u32); 6] = [(4, 22), (26, 44), (48, 66), (70, 88), (92, 110), (114, 132)];
/// Rows of the lattice band.
const LATTICE_ROWS: (u32, u32) = (138, 174);
/// Lattice spacing (vertical members, horizontal members), output pixels.
const LATTICE_PITCH: (u32, u32) = (5, 6);
const FOV_Y: f32 = 0.8;
const EXPOSURE_EV: f32 = 14.5;
/// Largest orbit angle (degrees either side of head-on): the orbit sweeps back and forth.
pub const ORBIT_AMPLITUDE_DEG: f32 = 30.0;
/// Speed of the ghosting scene's box, output pixels per frame.
pub const GHOST_SPEED_PX: f32 = 4.0;
/// Frames of the ghosting scene (scored every 5 frames from frame 16).
pub const GHOST_FRAMES: u32 = 48;
/// Frames of the held chart before the last 8 frame pairs are scored for stability.
pub const STABILITY_FRAMES: u32 = 48;
/// Excess FLIP at or above which a disoccluded pixel counts as visibly smeared.
pub const SMEAR_FLIP: f32 = 0.1;
/// Longest smear the measure looks for behind the edge (output pixels).
pub const MAX_SMEAR_PX: u32 = 24;
/// Most motion frames one chart run renders.
pub const MAX_FRAMES: u32 = 480;

/// Spec 27 § 2.3 targets (★ first guesses until calibrated).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartTargets {
    /// Motion clarity at least this.
    pub motion_clarity: f32,
    /// Thin geometry at least this.
    pub thin_geometry: f32,
    /// Ghosting at most this.
    pub ghosting: f32,
    /// Disocclusion smear at most this many pixels.
    pub disocclusion_px: f32,
    /// Stability (FLIP) at most this.
    pub stability: f32,
}

/// The § 2.3 targets.
pub const TARGETS: ChartTargets = ChartTargets {
    motion_clarity: 0.85,
    thin_geometry: 0.90,
    ghosting: 0.02,
    disocclusion_px: 2.0,
    stability: 0.01,
};

/// How the camera moves while the chart is scored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChartMotion {
    /// Horizontal pan, output pixels per frame (spec: 2).
    Pan {
        /// Output pixels per frame (negative pans left).
        px_per_frame: f32,
    },
    /// Orbit around the chart's vertical axis through its centre, degrees per frame (spec: 1),
    /// back and forth within ±[`ORBIT_AMPLITUDE_DEG`], ending head-on.
    Orbit {
        /// Degrees per frame.
        deg_per_frame: f32,
    },
}

impl ChartMotion {
    /// `"pan"` or `"orbit"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pan { .. } => "pan",
            Self::Orbit { .. } => "orbit",
        }
    }
}

/// One chart run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartRequest {
    /// Camera motion for the clarity and thin-geometry numbers.
    pub motion: ChartMotion,
    /// Motion frames (spec: 120); clamped to `2..=`[`MAX_FRAMES`].
    pub frames: u32,
    /// Render scale (0.25–1.0); 1.0 with [`TemporalUpscaler::Taau`] is native TAA.
    pub scale: f32,
    /// The temporal resolve.
    pub upscaler: TemporalUpscaler,
    /// TSR tuning; `None` keeps the renderer's default (what a game gets).
    pub tsr: Option<TsrSettings>,
}

impl ChartRequest {
    /// The spec's pan: 2 px/frame for 120 frames.
    #[must_use]
    pub fn pan(upscaler: TemporalUpscaler, scale: f32) -> Self {
        Self {
            motion: ChartMotion::Pan { px_per_frame: 2.0 },
            frames: 120,
            scale,
            upscaler,
            tsr: None,
        }
    }

    /// The spec's orbit: 1°/frame for 120 frames.
    #[must_use]
    pub fn orbit(upscaler: TemporalUpscaler, scale: f32) -> Self {
        Self {
            motion: ChartMotion::Orbit { deg_per_frame: 1.0 },
            ..Self::pan(upscaler, scale)
        }
    }

    fn frames(&self) -> u32 {
        self.frames.clamp(2, MAX_FRAMES)
    }

    fn scale(&self) -> f32 {
        if self.scale.is_finite() {
            self.scale.clamp(0.25, 1.0)
        } else {
            1.0
        }
    }
}

/// Line-pair modulation of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MtfReport {
    /// Modulation per band ([`BAR_WIDTHS`] order): Michelson contrast over the 8 px band's.
    pub modulation: [f32; 6],
    /// Spatial frequency (cycles per output pixel) where the modulation crosses 0.5.
    pub mtf50: f32,
}

/// The motion half of a chart run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionNumbers {
    /// The last moving frame's line pairs.
    pub moving: MtfReport,
    /// The native still's at the same pose.
    pub native: MtfReport,
    /// `moving.mtf50 / native.mtf50`.
    pub motion_clarity: f32,
    /// Lattice bars resolved (0–1).
    pub thin_geometry: f32,
}

/// The moving-object half of a chart run.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GhostNumbers {
    /// Signed trail bias toward the box, over its contrast (worst sampled frame).
    pub ghosting: f32,
    /// Mean absolute trail difference, over the box's contrast (worst sampled frame).
    pub ghost_residual: f32,
    /// Visible smear behind the trailing edge, output pixels (worst sampled frame).
    pub disocclusion_px: f32,
}

/// The held-chart half of a chart run.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StabilityNumbers {
    /// Mean frame-to-frame FLIP over the last 8 frame pairs.
    pub flip: f32,
    /// Mean frame-to-frame |ΔL| (8-bit luma levels) over the same pairs.
    pub luma_delta: f32,
}

/// The five § 2.3 numbers of one chart run, with their parts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartReport {
    /// The request that produced it (frames and scale as clamped).
    pub request: ChartRequest,
    /// Motion clarity and thin geometry.
    pub motion: MotionNumbers,
    /// Ghosting and disocclusion.
    pub ghost: GhostNumbers,
    /// Static stability.
    pub stability: StabilityNumbers,
    /// Frames rendered in total.
    pub frames_rendered: u32,
}

impl ChartReport {
    /// `(name, value, target, met)` for the five numbers, in spec order.
    #[must_use]
    pub fn checks(&self) -> [(&'static str, f32, f32, bool); 5] {
        let t = TARGETS;
        [
            (
                "motion_clarity",
                self.motion.motion_clarity,
                t.motion_clarity,
                self.motion.motion_clarity >= t.motion_clarity,
            ),
            (
                "thin_geometry",
                self.motion.thin_geometry,
                t.thin_geometry,
                self.motion.thin_geometry >= t.thin_geometry,
            ),
            (
                "ghosting",
                self.ghost.ghosting,
                t.ghosting,
                self.ghost.ghosting <= t.ghosting,
            ),
            (
                "disocclusion_px",
                self.ghost.disocclusion_px,
                t.disocclusion_px,
                self.ghost.disocclusion_px <= t.disocclusion_px,
            ),
            (
                "stability",
                self.stability.flip,
                t.stability,
                self.stability.flip <= t.stability,
            ),
        ]
    }

    /// Whether all five numbers meet § 2.3.
    #[must_use]
    pub fn meets_targets(&self) -> bool {
        self.checks().iter().all(|c| c.3)
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
pub fn chart_distance() -> f32 {
    CHART_SIZE.1 as f32 / (2.0 * (FOV_Y * 0.5).tan())
}

/// World y of the top edge of output row `row` (rows grow downward).
fn world_y(row: u32) -> f32 {
    CHART_SIZE.1 as f32 * 0.5 - row as f32
}

fn base_scene() -> LabScene {
    let (width, height) = CHART_SIZE;
    let d = chart_distance();
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
            // 2: mid ground stripe. Green and blue are 1 % above (0.15, 0.12): there the stripe
            // renders to one 8-bit color (146, 134, 119). At (0.15, 0.12) green or blue lands on a
            // rounding boundary, a residue far below one step flips whole pixels, and the ghosting
            // measure rises by up to 0.0018 by an amount that differs per adapter (ADR-5797,
            // ADR-5890).
            LabMaterial {
                base_color: [0.18, 0.1515, 0.1212, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 3: hot moving object.
            LabMaterial {
                base_color: [0.9, 0.85, 0.7, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
        ],
        meshes: Vec::new(),
    }
}

/// The line-pair and lattice chart, covering world x `[x_min, x_max]` (output pixels at the
/// head-on pose; the camera starts at x = 0).
#[must_use]
pub fn chart_scene(x_min: f32, x_max: f32) -> LabScene {
    let (width, height) = CHART_SIZE;
    let mut scene = base_scene();
    // Near backdrop behind the line pairs only (the lattice sees the far wall).
    let (mut v, mut i) = (Vec::new(), Vec::new());
    quad(
        x_min,
        world_y(BAND_ROWS[5].1 + 3),
        x_max,
        world_y(0) + 4.0,
        -1.0,
        &mut v,
        &mut i,
    );
    scene.meshes.push(mesh(v, i, 0));
    // Far wall at twice the distance, so twice the extent.
    let d = chart_distance();
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
        let mut x = (x_min / (2.0 * k)).floor() * 2.0 * k;
        while x < x_max {
            quad(x, world_y(rows.1), x + k, world_y(rows.0), 0.0, &mut v, &mut i);
            x += 2.0 * k;
        }
    }
    // Lattice (1 px members).
    let pitch = LATTICE_PITCH.0 as f32;
    let mut x = (x_min / pitch).floor() * pitch;
    while x < x_max {
        quad(
            x,
            world_y(LATTICE_ROWS.1),
            x + 1.0,
            world_y(LATTICE_ROWS.0),
            0.0,
            &mut v,
            &mut i,
        );
        x += pitch;
    }
    let mut row = LATTICE_ROWS.0 + 3;
    while row + 1 < LATTICE_ROWS.1 {
        let y = world_y(row);
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

/// Frame quality for a temporal resolve at `scale`, or the native still (`None`).
fn quality(scale: Option<f32>) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.taau = scale.is_some();
    // Every shipped preset takes a static surface's motion from depth. LAB draws it per mesh,
    // differencing the projected vertices and interpolating, which under a camera rotation is
    // pixels wrong on a large triangle: the chart's far wall read past 7.5 px/frame (1.9 true),
    // reset its history every frame and hid what the orbit is meant to measure.
    q.depth_velocity = true;
    q.render_scale_permille = (scale.unwrap_or(1.0).clamp(0.05, 1.0) * 1000.0).round() as u16;
    q
}

fn renderer(request: &ChartRequest) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(request.upscaler);
    if let Some(settings) = request.tsr {
        renderer.set_tsr_settings(settings);
    }
    renderer
}

/// Orbit angle (degrees) at `frame` of `frames`: a triangle wave of slope `deg` within
/// ±[`ORBIT_AMPLITUDE_DEG`], 0 at the last frame and rising there.
fn orbit_angle(deg: f32, frame: u32, frames: u32) -> f32 {
    let a = ORBIT_AMPLITUDE_DEG;
    let p = deg * (frame as f32 - (frames as f32 - 1.0));
    let q = (p + a).rem_euclid(4.0 * a);
    if q < 2.0 * a { q - a } else { 3.0 * a - q }
}

/// Places the camera for `frame` of `frames`; returns the camera's x on the chart plane at
/// the head-on poses (the orbit's is 0).
fn place_camera(scene: &mut LabScene, motion: ChartMotion, frame: u32, frames: u32) -> f32 {
    let d = chart_distance();
    match motion {
        ChartMotion::Pan { px_per_frame } => {
            let x = px_per_frame * frame as f32;
            scene.eye = [x, 0.0, d];
            scene.target = [x, 0.0, 0.0];
            x
        }
        ChartMotion::Orbit { deg_per_frame } => {
            let theta = orbit_angle(deg_per_frame, frame, frames).to_radians();
            scene.eye = [d * theta.sin(), 0.0, d * theta.cos()];
            scene.target = [0.0, 0.0, 0.0];
            0.0
        }
    }
}

fn render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &LabScene,
    quality: FrameQuality,
    post: &PostSettings,
) -> Result<ForwardResult, RenderError> {
    let mut fs = scene.as_frame_scene_with_quality(quality);
    fs.post = Some(post);
    renderer.render_offscreen(device, &fs)
}

fn luma(image: &ForwardResult, x: u32, y: u32) -> f32 {
    let i = ((y * image.width + x) * 4) as usize;
    match image.pixels.get(i..i + 3) {
        Some(p) => 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]),
        None => 0.0,
    }
}

fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let idx = ((values.len() - 1) as f32 * p).round() as usize;
    values[idx.min(values.len() - 1)]
}

/// Row-averaged horizontal luma profile of a band's interior rows.
fn profile(image: &ForwardResult, rows: (u32, u32)) -> Vec<f32> {
    let (r0, r1) = (rows.0 + 3, rows.1.saturating_sub(3));
    let n = r1.saturating_sub(r0).max(1) as f32;
    (0..image.width)
        .map(|x| (r0..r1).map(|y| luma(image, x, y)).sum::<f32>() / n)
        .collect()
}

/// Line-pair modulation and MTF50 of `image` (8 px margins left and right).
#[must_use]
pub fn measure_mtf(image: &ForwardResult) -> MtfReport {
    let mut contrast = [0.0f32; 6];
    for (c, rows) in contrast.iter_mut().zip(BAND_ROWS) {
        let p = profile(image, rows);
        let mut interior: Vec<f32> = p.get(8..p.len().saturating_sub(8)).unwrap_or(&[]).to_vec();
        let hi = percentile(&mut interior, 0.95);
        let lo = percentile(&mut interior, 0.05);
        *c = (hi - lo) / (hi + lo).max(1.0);
    }
    let reference = contrast[5].max(1.0e-3);
    let modulation = contrast.map(|c| c / reference);
    // Frequencies, highest first: 1 / (2k).
    let freq = BAR_WIDTHS.map(|k| 1.0 / (2.0 * k as f32));
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
    MtfReport { modulation, mtf50 }
}

/// Per-bar contrast of the vertical lattice members (rows clear of the horizontal ones).
fn lattice_bar_contrasts(image: &ForwardResult, camera_x: f32) -> Vec<f32> {
    let width = image.width;
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
        let col = (n as f32 * pitch - camera_x + width as f32 * 0.5).floor() as i32;
        if col < 4 || col > width as i32 - 5 {
            continue;
        }
        out.push(column(col) - 0.5 * (column(col - 2) + column(col + 2)));
    }
    out
}

/// Every vertical lattice member of `image` as `(column, contrast)`, left to right; the numbers
/// [`measure_lattice`] counts, for finding which bars a change lost.
#[must_use]
pub fn lattice_bars(image: &ForwardResult, camera_x: f32) -> Vec<(i32, f32)> {
    let width = image.width;
    let pitch = LATTICE_PITCH.0 as f32;
    let first = ((camera_x - width as f32 * 0.5) / pitch).floor() as i64 - 1;
    let last = ((camera_x + width as f32 * 0.5) / pitch).ceil() as i64 + 1;
    let columns = (first..=last)
        .map(|n| (n as f32 * pitch - camera_x + width as f32 * 0.5).floor() as i32)
        .filter(|col| *col >= 4 && *col <= width as i32 - 5);
    columns.zip(lattice_bar_contrasts(image, camera_x)).collect()
}

/// Share of lattice bars whose contrast is at least half the native still's median bar.
#[must_use]
pub fn measure_lattice(image: &ForwardResult, camera_x: f32, native: &ForwardResult) -> f32 {
    let mut reference = lattice_bar_contrasts(native, camera_x);
    if reference.is_empty() {
        return 0.0;
    }
    let median = percentile(&mut reference, 0.5).max(1.0);
    let bars = lattice_bar_contrasts(image, camera_x);
    let resolved = bars.iter().filter(|c| **c >= 0.5 * median).count();
    resolved as f32 / bars.len().max(1) as f32
}

/// Renders the moving chart for `request` and the native still of its last pose; returns
/// the numbers, the last moving frame and the native still.
///
/// # Errors
/// A device or frame failure.
pub fn measure_motion(
    device: &RenderDevice,
    request: &ChartRequest,
) -> Result<(MotionNumbers, ForwardResult, ForwardResult), RenderError> {
    let _span = tracing::info_span!("temporal_chart.motion").entered();
    let frames = request.frames();
    let width = CHART_SIZE.0 as f32;
    let (x_min, x_max) = match request.motion {
        ChartMotion::Pan { px_per_frame } => {
            let travel = px_per_frame * (frames - 1) as f32;
            (travel.min(0.0) - width - 16.0, travel.max(0.0) + width + 16.0)
        }
        ChartMotion::Orbit { .. } => (-3.0 * width, 3.0 * width),
    };
    let mut scene = chart_scene(x_min, x_max);
    let post = post();
    let moving_quality = quality(Some(request.scale()));
    let mut temporal = renderer(request);
    let mut camera_x = 0.0;
    let mut moving = None;
    for frame in 0..frames {
        camera_x = place_camera(&mut scene, request.motion, frame, frames);
        moving = Some(render(device, &mut temporal, &scene, moving_quality, &post)?);
    }
    let moving = moving.ok_or_else(|| RenderError::Readback("temporal chart rendered no frame".into()))?;
    let native = render(device, &mut FrameRenderer::new(), &scene, quality(None), &post)?;
    let moving_mtf = measure_mtf(&moving);
    let native_mtf = measure_mtf(&native);
    let numbers = MotionNumbers {
        moving: moving_mtf,
        native: native_mtf,
        motion_clarity: moving_mtf.mtf50 / native_mtf.mtf50.max(1.0e-3),
        thin_geometry: measure_lattice(&moving, camera_x, &native),
    };
    Ok((numbers, moving, native))
}

/// The ghosting scene: a 24×24 px box over 4 px ground stripes.
fn ghost_scene(with_box: bool) -> LabScene {
    let mut scene = base_scene();
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
    if with_box {
        let (mut v, mut i) = (Vec::new(), Vec::new());
        quad(-12.0, -12.0, 12.0, 12.0, 0.0, &mut v, &mut i);
        scene.meshes.push(mesh(v, i, 3));
    }
    scene
}

/// Frames of the ghosting scene that are scored: every 5th from frame 16, so the box (4 px per
/// frame) is scored at both of its phases against the 8 px ground period.
fn is_ghost_sample(frame: u32) -> bool {
    frame >= 16 && (frame - 16).is_multiple_of(5)
}

/// Box centre x (output px from the screen centre) at `frame`.
fn box_x(frame: u32) -> f32 {
    -120.0 + GHOST_SPEED_PX * frame as f32
}

fn to_unit_rgb(image: &ForwardResult) -> Vec<[f32; 3]> {
    image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2]].map(|c| f32::from(c) / 255.0))
        .collect()
}

/// Per-pixel FLIP error of the run with the box (`a`) against its native still, minus the
/// larger of two errors against the box-free native still: the box-free run's (`b`, the
/// converged ground) and a box-free frame whose history was dropped at the same jitter
/// (`fresh`, the best a pixel uncovered this frame can show). What is left is what the moving
/// box adds: its colour dragged over the uncovered ground. Comparing `a` with `b` directly
/// would score the box itself, which FLIP's edge kernel (σ ≈ 2.7 px at 67 ppd) spreads
/// several pixels past its edge, and would count an upscaler's single-frame softness on newly
/// uncovered pixels as smear.
fn excess_error(
    a: &ForwardResult,
    b: &ForwardResult,
    fresh: &ForwardResult,
    native_a: &ForwardResult,
    native_b: &ForwardResult,
) -> Vec<f32> {
    let (width, height) = CHART_SIZE;
    let nb = to_unit_rgb(native_b);
    let ea = flip(
        &to_unit_rgb(native_a),
        &to_unit_rgb(a),
        width,
        height,
        DEFAULT_PPD,
    )
    .map;
    let eb = flip(&nb, &to_unit_rgb(b), width, height, DEFAULT_PPD).map;
    let ef = flip(&nb, &to_unit_rgb(fresh), width, height, DEFAULT_PPD).map;
    ea.iter()
        .zip(eb.iter().zip(&ef))
        .map(|(x, (y, z))| x - y.max(*z))
        .collect()
}

/// Drops `renderer`'s temporal history without restarting its jitter sequence (a camera cut
/// would restart it): switching the upscaler away and back invalidates every history.
fn drop_history(renderer: &mut FrameRenderer, upscaler: TemporalUpscaler) {
    let other = if upscaler == TemporalUpscaler::Taau {
        TemporalUpscaler::Tsr
    } else {
        TemporalUpscaler::Taau
    };
    renderer.set_temporal_upscaler(other);
    renderer.set_temporal_upscaler(upscaler);
}

/// Smear behind the trailing edge at screen column `edge` (the first column the box covers):
/// the median over `rows` of the run of columns left of the edge with FLIP ≥ [`SMEAR_FLIP`].
fn smear_px(map: &[f32], width: u32, edge: i32, rows: std::ops::RangeInclusive<i32>) -> f32 {
    let mut runs: Vec<f32> = rows
        .map(|y| {
            let mut run = 0;
            for d in 1..=MAX_SMEAR_PX as i32 {
                let x = edge - d;
                if x < 0 {
                    break;
                }
                let v = map
                    .get((y as u32 * width + x as u32) as usize)
                    .copied()
                    .unwrap_or(0.0);
                if v < SMEAR_FLIP {
                    break;
                }
                run += 1;
            }
            run as f32
        })
        .collect();
    percentile(&mut runs, 0.5)
}

/// Renders the ghosting scene with and without the box through `request`'s resolve and
/// returns ghosting, the absolute residual and the disocclusion smear.
///
/// # Errors
/// A device or frame failure.
pub fn measure_ghost(device: &RenderDevice, request: &ChartRequest) -> Result<GhostNumbers, RenderError> {
    let _span = tracing::info_span!("temporal_chart.ghost").entered();
    let post = post();
    let q = quality(Some(request.scale()));
    let mut with = ghost_scene(true);
    let without = ghost_scene(false);
    let mut r_with = renderer(request);
    let mut r_without = renderer(request);
    let mut r_fresh = renderer(request);
    let mut r_native = FrameRenderer::new();
    let native_without = render(device, &mut r_native, &without, quality(None), &post)?;
    let (width, height) = CHART_SIZE;
    let mut out = GhostNumbers::default();
    for frame in 0..GHOST_FRAMES {
        let x = box_x(frame);
        if let Some(object) = with.meshes.get_mut(2) {
            object.model = [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [x, 0.0, 0.0, 1.0],
            ];
        }
        let a = render(device, &mut r_with, &with, q, &post)?;
        let b = render(device, &mut r_without, &without, q, &post)?;
        if !is_ghost_sample(frame) {
            // Keeps the fresh renderer's jitter in step with the other two.
            render(device, &mut r_fresh, &without, q, &post)?;
            continue;
        }
        drop_history(&mut r_fresh, request.upscaler);
        let fresh = render(device, &mut r_fresh, &without, q, &post)?;
        let cx = (x + width as f32 * 0.5) as i32;
        let cy = (height / 2) as i32;
        // Box/ground contrast from the box's interior now.
        let (mut object, mut ground, mut n) = (0.0, 0.0, 0.0);
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
        let (mut residual, mut bias, mut count) = (0.0, 0.0, 0.0);
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
            out.ghost_residual = out.ghost_residual.max(residual / count / contrast);
            // Signed toward the box (it is brighter than the ground).
            out.ghosting = out.ghosting.max(bias / count / contrast);
        }
        let native_with = render(device, &mut r_native, &with, quality(None), &post)?;
        let map = excess_error(&a, &b, &fresh, &native_with, &native_without);
        let edge = (now - 12.0).round() as i32;
        out.disocclusion_px = out
            .disocclusion_px
            .max(smear_px(&map, width, edge, (cy - 10)..=(cy + 10)));
    }
    Ok(out)
}

/// The ghosting scene at `frame` (debugging the disocclusion number): the frame with the box,
/// the frame without it, the excess FLIP map the smear is read from, and the trailing edge's
/// screen column.
///
/// # Errors
/// A device or frame failure.
pub fn ghost_sample(
    device: &RenderDevice,
    request: &ChartRequest,
    frame: u32,
) -> Result<(ForwardResult, ForwardResult, Vec<f32>, i32), RenderError> {
    let post = post();
    let q = quality(Some(request.scale()));
    let mut with = ghost_scene(true);
    let without = ghost_scene(false);
    let mut r_with = renderer(request);
    let mut r_without = renderer(request);
    let mut r_fresh = renderer(request);
    let mut r_native = FrameRenderer::new();
    let native_without = render(device, &mut r_native, &without, quality(None), &post)?;
    let mut last = None;
    for f in 0..=frame {
        if let Some(object) = with.meshes.get_mut(2) {
            object.model[3][0] = box_x(f);
        }
        let a = render(device, &mut r_with, &with, q, &post)?;
        let b = render(device, &mut r_without, &without, q, &post)?;
        if f == frame {
            drop_history(&mut r_fresh, request.upscaler);
        }
        let fresh = render(device, &mut r_fresh, &without, q, &post)?;
        last = Some((a, b, fresh));
    }
    let (a, b, fresh) =
        last.ok_or_else(|| RenderError::Readback("temporal chart rendered no frame".into()))?;
    let native_with = render(device, &mut r_native, &with, quality(None), &post)?;
    let map = excess_error(&a, &b, &fresh, &native_with, &native_without);
    let width = CHART_SIZE.0;
    let edge = (box_x(frame) + width as f32 * 0.5 - 12.0).round() as i32;
    Ok((a, b, map, edge))
}

/// Renders the held chart `frames` times through `request`'s resolve and returns every frame.
///
/// # Errors
/// A device or frame failure.
pub fn held_frames(
    device: &RenderDevice,
    request: &ChartRequest,
    frames: u32,
) -> Result<Vec<ForwardResult>, RenderError> {
    let width = CHART_SIZE.0 as f32;
    let scene = chart_scene(-width - 16.0, width + 16.0);
    let post = post();
    let q = quality(Some(request.scale()));
    let mut r = renderer(request);
    (0..frames)
        .map(|_| render(device, &mut r, &scene, q, &post))
        .collect()
}

/// Renders the held chart for [`STABILITY_FRAMES`] frames and scores the last 8 frame pairs.
///
/// # Errors
/// A device or frame failure.
pub fn measure_stability(
    device: &RenderDevice,
    request: &ChartRequest,
) -> Result<StabilityNumbers, RenderError> {
    let _span = tracing::info_span!("temporal_chart.stability").entered();
    let frames = held_frames(device, request, STABILITY_FRAMES)?;
    let (w, h) = CHART_SIZE;
    let (mut flip_sum, mut luma_sum, mut pairs) = (0.0, 0.0, 0.0f32);
    for pair in frames.windows(2).skip(frames.len().saturating_sub(9)) {
        let (prev, image) = (&pair[0], &pair[1]);
        flip_sum += flip(&to_unit_rgb(prev), &to_unit_rgb(image), w, h, DEFAULT_PPD).mean;
        let mut delta = 0.0;
        for y in 0..h {
            for x in 0..w {
                delta += (luma(image, x, y) - luma(prev, x, y)).abs();
            }
        }
        luma_sum += delta / (w * h) as f32;
        pairs += 1.0;
    }
    Ok(StabilityNumbers {
        flip: flip_sum / pairs.max(1.0),
        luma_delta: luma_sum / pairs.max(1.0),
    })
}

/// Runs all three parts of the chart for `request`.
///
/// # Errors
/// A device or frame failure.
pub fn run(device: &RenderDevice, request: &ChartRequest) -> Result<ChartReport, RenderError> {
    let _span = tracing::info_span!("temporal_chart").entered();
    let (motion, _, _) = measure_motion(device, request)?;
    let ghost = measure_ghost(device, request)?;
    let stability = measure_stability(device, request)?;
    let mut request = *request;
    request.frames = request.frames();
    request.scale = request.scale();
    Ok(ChartReport {
        request,
        motion,
        ghost,
        stability,
        // Motion frames and their native still; the ghosting scene three times (with the box,
        // without, without and history dropped when scored), its box-free native still and one
        // native still per scored frame; the held chart.
        frames_rendered: request.frames
            + 1
            + 3 * GHOST_FRAMES
            + 1
            + (0..GHOST_FRAMES).filter(|f| is_ghost_sample(*f)).count() as u32
            + STABILITY_FRAMES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orbit_ends_head_on_and_stays_in_range() {
        for frames in [2, 30, 120, 121] {
            assert!(orbit_angle(1.0, frames - 1, frames).abs() < 1e-5);
            for f in 0..frames {
                let a = orbit_angle(1.0, f, frames);
                assert!(a.abs() <= ORBIT_AMPLITUDE_DEG + 1e-4, "{a}");
            }
            // 1 degree per frame everywhere (the triangle never jumps).
            for f in 1..frames {
                let step = (orbit_angle(1.0, f, frames) - orbit_angle(1.0, f - 1, frames)).abs();
                assert!((step - 1.0).abs() < 1e-3, "frame {f}: {step}");
            }
        }
    }

    #[test]
    fn mtf50_reads_the_crossing() {
        let mut image = ForwardResult {
            width: CHART_SIZE.0,
            height: CHART_SIZE.1,
            pixels: vec![0; (CHART_SIZE.0 * CHART_SIZE.1 * 4) as usize],
        };
        // Perfect line pairs on every band.
        for (k, rows) in BAR_WIDTHS.iter().zip(BAND_ROWS) {
            for y in rows.0..rows.1 {
                for x in 0..CHART_SIZE.0 {
                    let v = if (x / k).is_multiple_of(2) { 200 } else { 10 };
                    let i = ((y * CHART_SIZE.0 + x) * 4) as usize;
                    image.pixels[i..i + 3].fill(v);
                }
            }
        }
        let report = measure_mtf(&image);
        assert!((report.mtf50 - 0.5).abs() < 1e-6, "{report:?}");
        assert!(report.modulation.iter().all(|m| (m - 1.0).abs() < 1e-3));
    }

    #[test]
    fn ghost_samples_see_both_stripe_phases() {
        let phases: std::collections::BTreeSet<u32> = (0..GHOST_FRAMES)
            .filter(|f| is_ghost_sample(*f))
            .map(|f| (box_x(f) as i32).rem_euclid(8) as u32)
            .collect();
        assert_eq!(phases.len(), 2, "{phases:?}");
        assert!((0..GHOST_FRAMES).filter(|f| is_ghost_sample(*f)).all(|f| f >= 16));
    }

    #[test]
    fn smear_counts_the_contiguous_run_behind_the_edge() {
        let width = 64;
        let mut map = vec![0.0f32; (width * 4) as usize];
        // Row 1: 3 visible pixels right behind the edge at x = 40, then a gap, then more.
        for x in 37..40 {
            map[(width + x) as usize] = 0.5;
        }
        map[(width + 30) as usize] = 0.5;
        assert_eq!(smear_px(&map, width, 40, 1..=1), 3.0);
        assert_eq!(smear_px(&map, width, 40, 0..=0), 0.0);
        // Median over rows 0..=2: runs 0, 3, 0.
        assert_eq!(smear_px(&map, width, 40, 0..=2), 0.0);
    }

    #[test]
    fn checks_follow_the_targets() {
        let report = ChartReport {
            request: ChartRequest::pan(TemporalUpscaler::Tsr, 0.67),
            motion: MotionNumbers {
                moving: MtfReport {
                    modulation: [1.0; 6],
                    mtf50: 0.5,
                },
                native: MtfReport {
                    modulation: [1.0; 6],
                    mtf50: 0.5,
                },
                motion_clarity: 1.0,
                thin_geometry: 0.95,
            },
            ghost: GhostNumbers {
                ghosting: 0.01,
                ghost_residual: 0.03,
                disocclusion_px: 2.0,
            },
            stability: StabilityNumbers {
                flip: 0.02,
                luma_delta: 0.1,
            },
            frames_rendered: 0,
        };
        let checks = report.checks();
        assert_eq!(checks.map(|c| c.3), [true, true, true, true, false]);
        assert!(!report.meets_targets());
    }
}
