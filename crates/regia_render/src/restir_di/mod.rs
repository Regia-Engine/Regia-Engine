// SPDX-License-Identifier: MPL-2.0
//! Stochastic many-light sampling for direct lighting (ADR-2470; `specs/26` § 4 "Many lights",
//! `specs/27` § 4.2 stage 2b, M15d): reservoir-based spatiotemporal importance resampling.
//!
//! Every pixel picks **one** local light per frame out of the lights that reach it and shades
//! with that light alone, weighted so that the expected value equals the sum over every light.
//! The pick is good because each frame's candidates are resampled against a target function
//! (the light's unshadowed irradiance at the pixel) and the chosen candidate is reused across
//! frames (temporal reuse, along the motion vectors, rejected at disocclusions) and across
//! neighboring pixels (spatial reuse, rejected across depth and normal edges).
//!
//! The module is self-contained and opt-in. Nothing in the default frame calls it:
//!
//! - [`cpu`] is the reference: a brute-force sum over every light and the resampled estimator,
//!   pass for pass the same as the GPU, with the statistics that show it unbiased
//!   ([`cpu::evaluate`]).
//! - [`scene`] builds the deterministic synthetic G-buffer and light sets the tests and the
//!   agent method `render.many_light_stats` run on.
//! - [`RestirDiGpu`] is the GPU twin (`shaders/restir_di.wgsl`): three compute passes recorded
//!   from explicit inputs ([`RestirInputs`]: linear depth, normal and roughness, albedo and
//!   metallic, motion, the light buffer, optionally the clustered light lists).
//! - [`RestirDiDebugPass`] is an evaluation-only [`crate::FramePass`] at
//!   [`crate::GraphSlot::AfterOpaque`] that reconstructs normals from the scene depth and shows
//!   the estimate (or the brute-force reference) over a real frame
//!   (`render.set_many_light_sampling`).
//!
//! The production hookup (the forward pass or the visibility resolve consuming the selected
//! light per pixel) is written out in ADR-2470; it needs frame lines this module does not own.
//!
//! ## Provenance
//!
//! Bitterli, Wyman, Pharr, Shirley, Lefohn, Jarosz, "Spatiotemporal reservoir resampling for
//! real-time ray tracing with dynamic direct lighting", ACM TOG 39(4) (SIGGRAPH 2020):
//! resampled importance sampling with a streaming weighted reservoir (their Alg. 1-3), temporal
//! and spatial reuse by combining reservoirs (Alg. 4), the history cap of 20 × M, and the
//! unbiased `1/Z` normalization that counts only the reservoirs whose target is non-zero at the
//! chosen sample (§ 4.3, Alg. 6). Talbot, Cline, Egbert, "Importance Resampling for Global
//! Illumination", EGSR 2005: resampled importance sampling. Chao, "Weighted sampling with random
//! access", 1982: the streaming reservoir. The random numbers are the PCG hash of Jarzynski and
//! Olano, "Hash Functions for GPU Rendering", JCGT 9(3), 2020. Written from the papers.

pub mod cpu;
mod debug_pass;
pub(crate) mod depth_gbuffer;
pub(crate) mod gpu;
pub mod scene;

pub use debug_pass::{
    ManyLightFeed, ManyLightMode, RESTIR_DI_PASS_NAME, RestirDebugView, RestirDiCounters, RestirDiDebugPass,
    restir_di_counters,
};
pub use depth_gbuffer::{DEPTH_GBUFFER_FORMATS, DepthGBuffer, DepthGBufferInputs};
pub use gpu::{RestirClusters, RestirDiGpu, RestirInputs};

use regia_core::glam::{Mat4, Vec3, Vec4};

/// Bytes of one [`ManyLight`] record: the layout of the frame's local-light buffer
/// (`GpuLight` in `lab_forward.wgsl` and `cluster_build.wgsl`), so the production hookup binds
/// the frame's own buffer.
pub const MANY_LIGHT_BYTES: usize = 64;

/// One local light, in the frame's local-light record layout.
///
/// - `position_kind`: world position, kind in `w` (1 point, 2 spot; 4 and up are fitted
///   emitters, which the sampler skips and the forward loop keeps).
/// - `color_intensity`: linear color, intensity in `w` (candela).
/// - `direction_radius`: spot travel direction, influence radius (the windowed falloff's
///   cut-off) in `w`.
/// - `cone_shadow`: cosines of the inner and outer spot cone, shadow tile (`< 0` none), sphere
///   source radius in metres.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ManyLight {
    /// World position (`xyz`) and kind (`w`).
    pub position_kind: [f32; 4],
    /// Linear color (`rgb`) and intensity in candela (`w`).
    pub color_intensity: [f32; 4],
    /// Spot travel direction (`xyz`) and influence radius (`w`).
    pub direction_radius: [f32; 4],
    /// Inner and outer cone cosines, shadow tile, sphere source radius.
    pub cone_shadow: [f32; 4],
}

const _: () = assert!(size_of::<ManyLight>() == MANY_LIGHT_BYTES);

impl ManyLight {
    /// A point light of `lumens` (converted to candela by `1 / 4π`, as the frame packs it), no
    /// shadow tile.
    #[must_use]
    pub fn point(position: [f32; 3], color: [f32; 3], lumens: f32, radius: f32, source_radius: f32) -> Self {
        Self {
            position_kind: [position[0], position[1], position[2], 1.0],
            color_intensity: [
                color[0],
                color[1],
                color[2],
                lumens / (4.0 * std::f32::consts::PI),
            ],
            direction_radius: [0.0, -1.0, 0.0, radius],
            cone_shadow: [1.0, 0.0, -1.0, source_radius.max(0.0)],
        }
    }

    /// A spot light of `candela` along `direction` (normalized here), `cone` = inner and outer
    /// half-angles in radians, no shadow tile.
    #[must_use]
    pub fn spot(
        position: [f32; 3],
        direction: [f32; 3],
        color: [f32; 3],
        candela: f32,
        cone: [f32; 2],
        range: f32,
        source_radius: f32,
    ) -> Self {
        let [inner_angle, outer_angle] = cone;
        let d = Vec3::from(direction).normalize_or(Vec3::NEG_Y);
        Self {
            position_kind: [position[0], position[1], position[2], 2.0],
            color_intensity: [color[0], color[1], color[2], candela],
            direction_radius: [d.x, d.y, d.z, range],
            cone_shadow: [inner_angle.cos(), outer_angle.cos(), -1.0, source_radius.max(0.0)],
        }
    }

    /// The record of a [`crate::PointLight`] component at `position` (the entity's transform).
    #[must_use]
    pub fn from_point_light(
        light: &crate::PointLight,
        position: [f32; 3],
        source_radius: Option<f32>,
    ) -> Self {
        Self::point(
            position,
            light.color,
            light.intensity,
            light.radius,
            crate::light_source::specular_source_radius(source_radius),
        )
    }

    /// The record of a [`crate::SpotLight`] component at `position`, aimed along `direction`
    /// (the entity's −Z).
    #[must_use]
    pub fn from_spot_light(
        light: &crate::SpotLight,
        position: [f32; 3],
        direction: [f32; 3],
        source_radius: Option<f32>,
    ) -> Self {
        Self::spot(
            position,
            direction,
            light.color,
            light.intensity,
            [light.inner_angle, light.outer_angle],
            light.range,
            crate::light_source::specular_source_radius(source_radius),
        )
    }

    /// Kind code: 1 point, 2 spot, other values are not sampled.
    #[must_use]
    pub fn kind(&self) -> u32 {
        (self.position_kind[3] + 0.5).max(0.0) as u32
    }

    /// Whether the sampler can pick this light (a point or a spot).
    #[must_use]
    pub fn sampled(&self) -> bool {
        matches!(self.kind(), 1 | 2)
    }
}

/// Settings of the resampled estimator; the CPU reference and the GPU passes read the same.
#[derive(Clone, Debug, PartialEq)]
pub struct RestirDiSettings {
    /// Candidates drawn per pixel per frame from its light list (1..=[`MAX_CANDIDATES`]).
    pub initial_candidates: u32,
    /// Reuse the previous frame's reservoir along the motion vector.
    pub temporal: bool,
    /// The previous reservoir's sample count is capped at this many times the current one's
    /// (Bitterli et al. use 20): bounds the weight of a stale history.
    pub history_cap: f32,
    /// Neighbor reservoirs combined per pixel (0..=[`MAX_SPATIAL_TAPS`]; 0 = no spatial reuse).
    pub spatial_taps: u32,
    /// Radius of the spatial neighborhood, pixels.
    pub spatial_radius_px: f32,
    /// A history or neighbor surface is rejected when it lies farther than this fraction of
    /// the pixel's view depth from the pixel's surface (the ADR-864 relative depth rule).
    pub depth_tolerance: f32,
    /// A history or neighbor surface is rejected when the cosine between its normal and the
    /// pixel's is below this (the ADR-864 normal rule).
    pub normal_tolerance: f32,
    /// Screen-space shadow ray steps toward the chosen light at shading (0 = unshadowed).
    pub trace_steps: u32,
    /// Thickness of a depth-buffer sample for the screen trace, as a fraction of its depth.
    pub trace_thickness: f32,
}

/// Largest [`RestirDiSettings::initial_candidates`].
pub const MAX_CANDIDATES: u32 = 64;
/// Largest [`RestirDiSettings::spatial_taps`].
pub const MAX_SPATIAL_TAPS: u32 = 8;

impl Default for RestirDiSettings {
    fn default() -> Self {
        Self {
            initial_candidates: 16,
            temporal: true,
            history_cap: 20.0,
            spatial_taps: 4,
            spatial_radius_px: 16.0,
            depth_tolerance: 0.1,
            normal_tolerance: 0.9,
            trace_steps: 0,
            trace_thickness: 0.05,
        }
    }
}

impl RestirDiSettings {
    /// One sample per frame, no reuse: plain resampled importance sampling.
    #[must_use]
    pub fn candidates_only(initial_candidates: u32) -> Self {
        Self {
            initial_candidates,
            temporal: false,
            spatial_taps: 0,
            ..Self::default()
        }
    }

    /// Checks every field's range.
    ///
    /// # Errors
    /// [`RestirDiError::InvalidSetting`] naming the first field out of range.
    pub fn validate(&self) -> Result<(), RestirDiError> {
        let bad = |field: &'static str| Err(RestirDiError::InvalidSetting(field));
        if !(1..=MAX_CANDIDATES).contains(&self.initial_candidates) {
            return bad("initial_candidates");
        }
        if !(self.history_cap.is_finite() && self.history_cap >= 1.0) {
            return bad("history_cap");
        }
        if self.spatial_taps > MAX_SPATIAL_TAPS {
            return bad("spatial_taps");
        }
        if !(self.spatial_radius_px.is_finite() && self.spatial_radius_px >= 1.0) {
            return bad("spatial_radius_px");
        }
        if !(self.depth_tolerance.is_finite() && self.depth_tolerance > 0.0) {
            return bad("depth_tolerance");
        }
        if !(self.normal_tolerance.is_finite() && (-1.0..=1.0).contains(&self.normal_tolerance)) {
            return bad("normal_tolerance");
        }
        if self.trace_steps > 64 {
            return bad("trace_steps");
        }
        if !(self.trace_thickness.is_finite() && self.trace_thickness > 0.0) {
            return bad("trace_thickness");
        }
        Ok(())
    }
}

/// Errors of the many-light sampler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestirDiError {
    /// A [`RestirDiSettings`] field is out of range.
    InvalidSetting(&'static str),
    /// An input's size or format does not match the others.
    BadInput(String),
}

impl RestirDiError {
    /// The agent-protocol error code: `invalid_setting` or `bad_input`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSetting(_) => "invalid_setting",
            Self::BadInput(_) => "bad_input",
        }
    }
}

impl std::fmt::Display for RestirDiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSetting(field) => write!(f, "many-light sampling setting `{field}` is out of range"),
            Self::BadInput(message) => write!(f, "many-light sampling input: {message}"),
        }
    }
}

impl std::error::Error for RestirDiError {}

/// The camera the passes reconstruct surfaces with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RestirCamera {
    /// Unjittered view-projection.
    pub clip_from_world: Mat4,
    /// View matrix (the cluster grid's froxels are in view space).
    pub view_from_world: Mat4,
    /// Camera position.
    pub eye: [f32; 3],
    /// Unit view direction (the view-space −Z in world space).
    pub forward: [f32; 3],
    /// Vertical field of view, radians (the cluster grid's froxels).
    pub fov_y: f32,
    /// Near plane (the cluster grid's slices).
    pub near: f32,
    /// Far plane (the cluster grid's slices).
    pub far: f32,
}

impl RestirCamera {
    /// A perspective camera at `eye` looking at `target` (+Y up), standard depth 0..1.
    #[must_use]
    pub fn look_at(eye: [f32; 3], target: [f32; 3], fov_y: f32, aspect: f32, near: f32, far: f32) -> Self {
        let e = Vec3::from(eye);
        let t = Vec3::from(target);
        let view = regia_core::glam::camera::rh::view::look_at_mat4(e, t, Vec3::Y);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov_y, aspect, near, far);
        let forward = (t - e).normalize_or(Vec3::NEG_Z);
        Self {
            clip_from_world: proj * view,
            view_from_world: view,
            eye,
            forward: forward.to_array(),
            fov_y,
            near,
            far,
        }
    }

    /// World position of pixel centre `(x, y)` of a `w × h` image at linear view depth `depth`.
    #[must_use]
    pub fn world_at(&self, x: u32, y: u32, w: u32, h: u32, depth: f32) -> Vec3 {
        let uv = [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32];
        world_at_uv(
            self.clip_from_world.inverse(),
            Vec3::from(self.eye),
            Vec3::from(self.forward),
            uv,
            depth,
        )
    }
}

/// The reconstruction both sides use: the view ray through `uv` (from two points of the
/// inverse view-projection) scaled so its view depth is `depth`.
#[must_use]
pub(crate) fn world_at_uv(world_from_clip: Mat4, eye: Vec3, forward: Vec3, uv: [f32; 2], depth: f32) -> Vec3 {
    let ndc = [uv[0] * 2.0 - 1.0, 1.0 - uv[1] * 2.0];
    let a = world_from_clip * Vec4::new(ndc[0], ndc[1], 0.25, 1.0);
    let b = world_from_clip * Vec4::new(ndc[0], ndc[1], 0.75, 1.0);
    let a = a.truncate() / a.w;
    let b = b.truncate() / b.w;
    let mut dir = (b - a).normalize_or(forward);
    if dir.dot(forward) < 0.0 {
        dir = -dir;
    }
    eye + dir * (depth / dir.dot(forward).max(1e-4))
}

/// Linear-light luminance (Rec. 709).
#[must_use]
pub fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The PCG hash (Jarzynski and Olano 2020), the same in `restir_di.wgsl`.
#[must_use]
pub(crate) fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// Random stream of one pixel, frame and pass (`stream`), from the run's `seed`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rng(u32);

impl Rng {
    pub(crate) fn new(pixel: u32, frame: u32, stream: u32, seed: u32) -> Self {
        Self(pcg(pixel ^ pcg(frame ^ pcg(stream ^ pcg(seed)))))
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn next(&mut self) -> f32 {
        self.0 = pcg(self.0);
        (self.0 >> 8) as f32 * (1.0 / 16_777_216.0)
    }
}

/// Random streams (`Rng::new`'s `stream`).
pub(crate) const STREAM_INITIAL: u32 = 0;
pub(crate) const STREAM_TEMPORAL: u32 = 1;
pub(crate) const STREAM_SPATIAL: u32 = 2;

/// One shading point: what the target function and the BRDF read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    /// World position.
    pub position: Vec3,
    /// Unit shading normal.
    pub normal: Vec3,
    /// Linear view depth (> 0).
    pub depth: f32,
}

/// The frame's windowed inverse square with a sphere source (`pbr_lighting.wgsl`
/// `distance_attenuation_source`).
fn distance_attenuation(distance: f32, radius: f32, source_radius: f32) -> f32 {
    let r = radius.max(1e-4);
    let d = distance.max(0.0);
    let num = (1.0 - (d / r).powi(4)).clamp(0.0, 1.0);
    (num * num) / (d * d + (source_radius * source_radius).max(1.0))
}

/// The frame's spot cone (`pbr_lighting.wgsl` `spot_attenuation`); `l` points to the light.
fn spot_attenuation(l: Vec3, spot_dir: Vec3, inner_cos: f32, outer_cos: f32) -> f32 {
    let cos_angle = (-spot_dir.normalize_or(Vec3::NEG_Y)).dot(l);
    let denom = (inner_cos - outer_cos).max(1e-4);
    ((cos_angle - outer_cos) / denom).clamp(0.0, 1.0)
}

/// Unit direction to the light, its attenuation (distance window, spot cone) and `n · l`, or
/// `None` when the light does not reach the surface.
fn reach(light: &ManyLight, surface: &Surface) -> Option<(Vec3, f32, f32)> {
    reach_position(light, surface).filter(|&(_, _, nol)| nol > 0.0)
}

/// [`reach`] without the `n · l` test: the light reaches the surface's position (distance
/// window, spot cone); `n · l` may be negative. The target's support (ADR-2652): the frame
/// shades the chosen light with its own normal, which a normal rebuilt from depth may disagree
/// with at silhouettes, so a light behind the rebuilt normal must stay possible.
fn reach_position(light: &ManyLight, surface: &Surface) -> Option<(Vec3, f32, f32)> {
    if !light.sampled() {
        return None;
    }
    let to = Vec3::new(
        light.position_kind[0],
        light.position_kind[1],
        light.position_kind[2],
    ) - surface.position;
    let d = to.length();
    let mut att = distance_attenuation(d, light.direction_radius[3], light.cone_shadow[3]);
    if att <= 0.0 {
        return None;
    }
    let l = to / d.max(1e-4);
    if light.kind() == 2 {
        let dir = Vec3::new(
            light.direction_radius[0],
            light.direction_radius[1],
            light.direction_radius[2],
        );
        att *= spot_attenuation(l, dir, light.cone_shadow[0], light.cone_shadow[1]);
        if att <= 0.0 {
            return None;
        }
    }
    Some((l, att, surface.normal.dot(l)))
}

/// Weight of the support term in [`target`]: small, so the target follows the shaded
/// luminance, but it keeps the target positive exactly where the light reaches the surface's
/// position (also on black materials and behind the normal), which is what the `1/Z`
/// normalization tests at neighbors.
pub const SUPPORT_WEIGHT: f32 = 1e-3;

/// Whether `light` reaches `surface`'s position with a positive intensity, whatever the normal:
/// the support of [`target`] (a neighbor's reservoir counts in the `1/Z` normalization exactly
/// when this holds there).
#[must_use]
pub fn supports(light: &ManyLight, surface: &Surface) -> bool {
    reach_position(light, surface).is_some_and(|(_, att, _)| arriving(light, att) > 0.0)
}

/// Luminance of the light arriving at a point (no cosine).
fn arriving(light: &ManyLight, att: f32) -> f32 {
    let c = [
        light.color_intensity[0],
        light.color_intensity[1],
        light.color_intensity[2],
    ];
    luminance(c) * light.color_intensity[3] * att
}

/// The target function: the luminance of the light's unshadowed shaded contribution
/// ([`shade`]) plus [`SUPPORT_WEIGHT`] times the luminance arriving at the point. Positive
/// exactly where [`supports`] holds, so resampling against it stays unbiased for any integrand
/// that is zero where the light does not reach (ADR-2652: also behind the rebuilt normal).
#[must_use]
pub fn target(light: &ManyLight, surface: &Surface, view: Vec3, material: &ShadingInputs) -> f32 {
    let Some((l, att, nol)) = reach_position(light, surface) else {
        return 0.0;
    };
    let arrive = arriving(light, att);
    if arrive <= 0.0 {
        return 0.0;
    }
    let shaded = if nol > 0.0 {
        luminance(radiance(light, surface, view, material, l, att, nol))
    } else {
        0.0
    };
    shaded + SUPPORT_WEIGHT * arrive
}

/// The material the estimator shades with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadingInputs {
    /// Linear base color.
    pub albedo: [f32; 3],
    /// Perceptual roughness (0..1).
    pub roughness: f32,
    /// Metallic (0..1).
    pub metallic: f32,
}

/// Outgoing radiance from one light (unshadowed): Lambert diffuse plus GGX specular with the
/// height-correlated Smith visibility and Schlick Fresnel (Karis 2013; the frame's
/// `isotropic_lobes` without the coat and multi-scatter terms), times the light's radiance,
/// attenuation and `n · l`. `view` points from the surface to the eye.
#[must_use]
pub fn shade(light: &ManyLight, surface: &Surface, view: Vec3, material: &ShadingInputs) -> [f32; 3] {
    match reach(light, surface) {
        Some((l, att, nol)) => radiance(light, surface, view, material, l, att, nol),
        None => [0.0; 3],
    }
}

fn radiance(
    light: &ManyLight,
    surface: &Surface,
    view: Vec3,
    material: &ShadingInputs,
    l: Vec3,
    att: f32,
    nol: f32,
) -> [f32; 3] {
    let n = surface.normal;
    let nov = n.dot(view).max(1e-4);
    let h = (l + view).normalize_or(n);
    let noh = n.dot(h).max(0.0);
    let loh = l.dot(h).max(0.0);
    let rough = material.roughness.clamp(0.045, 1.0);
    let a = rough * rough;
    let a2 = a * a;
    let dd = noh * noh * (a2 - 1.0) + 1.0;
    let d = a2 / (std::f32::consts::PI * dd * dd).max(1e-8);
    let gv = nol * (nov * nov * (1.0 - a2) + a2).sqrt();
    let gl = nov * (nol * nol * (1.0 - a2) + a2).sqrt();
    let vis = 0.5 / (gv + gl).max(1e-6);
    let fc = (1.0 - loh).powi(5);
    let m = material.metallic.clamp(0.0, 1.0);
    let mut out = [0.0f32; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let albedo = material.albedo[c].clamp(0.0, 1.0);
        let f0 = 0.04 * (1.0 - m) + albedo * m;
        let f = f0 + (1.0 - f0) * fc;
        let diffuse = albedo * (1.0 - m) / std::f32::consts::PI;
        let le = light.color_intensity[c] * light.color_intensity[3];
        *o = (diffuse + d * vis * f) * le * att * nol;
    }
    out
}

/// Cluster grid of the frame (`clustering.wgsl`): 16 × 9 cells, 24 exponential depth slices
/// (Olsson, Billeter and Assarsson 2012).
pub const CLUSTER_GRID: [u32; 3] = [16, 9, 24];

/// The cluster of a pixel at uv `uv` and linear view depth `depth` (`clustering.wgsl`
/// `cluster_index` of `depth_to_slice`).
#[must_use]
pub fn cluster_of(uv: [f32; 2], depth: f32, near: f32, far: f32) -> u32 {
    let [gx, gy, gz] = CLUSTER_GRID;
    let cx = ((uv[0] * gx as f32) as u32).min(gx - 1);
    let cy = ((uv[1] * gy as f32) as u32).min(gy - 1);
    let cz = if depth <= near {
        0
    } else if depth >= far {
        gz - 1
    } else {
        let t = (depth / near).ln() / (far / near).ln();
        ((t * gz as f32).floor() as u32).min(gz - 1)
    };
    cx + gx * (cy + gy * cz)
}
