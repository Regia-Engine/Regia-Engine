// SPDX-License-Identifier: MPL-2.0
//! ReSTIR GI over hardware ray queries (ADR-3349; `specs/27` § 4.2 stage 2b,
//! `specs/79` § 11).
//!
//! One cosine-distributed ray starts at each visible surface. Its hit point carries the
//! existing probe irradiance and a shadow-ray-tested direct sun term. Temporal reservoirs
//! reproject through camera motion and validate the surface and its connection to the reused
//! path vertex. An opaque hit is also dropped when the surface at that world point has moved
//! farther than [`HIT_MOTION_METRES`] (ADR-4001); a still surface keeps today's camera test.
//! Spatial reservoirs apply the solid-angle Jacobian and a visibility ray before
//! the existing `gi_denoise.wgsl` passes reconstruct the indirect diffuse screen lookup.
//!
//! The module is opt-in. [`crate::RenderProfile::restir_gi`] is false in every preset. A device
//! without ray queries, or without hit-vertex return, keeps the probe (DDGI / Depth) path and
//! reports why. Nothing in this module runs on Android.
//!
//! ## Provenance
//!
//! Ouyang, Liu, Kettunen, Pharr, Pantaleoni, "ReSTIR GI: Path Resampling for Real-Time Path
//! Tracing", Computer Graphics Forum (HPG) 2021, Algorithms 1–4 and Equation 11: path vertices,
//! temporal and spatial reservoirs, the cosine-weighted integrand target and the solid-angle
//! Jacobian.
//! Bitterli et al., SIGGRAPH 2020: weighted reservoir sampling and contribution weights.
//! Wyman and Panteleev, HPG 2021: practical visibility-aware temporal and spatial reuse.
//! Duff et al., JCGT 2017: the orthonormal basis for cosine hemisphere sampling. Jarzynski and
//! Olano, JCGT 2020: the PCG hash. Original Rust and WGSL from those public papers.

mod cpu;
mod gpu;

pub use cpu::{HIT_MOTION_METRES, RestirGiWork};
/// The composed ReSTIR GI module, for the DX12 shader checks (`shader_hlsl`).
#[cfg(test)]
pub(crate) use gpu::SOURCE;
pub use gpu::{RestirGiGpu, RestirGiInputs};

#[cfg(test)]
use regia_core::glam::Vec3;

/// Settings shared by the CPU checks and the GPU passes.
#[derive(Clone, Debug, PartialEq)]
pub struct RestirGiSettings {
    /// Reuse a reprojected reservoir from the previous frame.
    pub temporal: bool,
    /// Previous reservoir sample-count cap (30 in Ouyang et al.).
    pub history_cap: f32,
    /// Neighbor reservoirs tested per pixel (0 disables spatial reuse).
    pub spatial_taps: u32,
    /// Random neighbor disk radius in lookup pixels.
    pub spatial_radius_px: f32,
    /// Spatial reservoir sample-count cap (500 in Ouyang et al.).
    pub spatial_cap: f32,
    /// Largest relative linear-depth difference accepted for a history or neighbor surface.
    pub depth_tolerance: f32,
    /// Smallest normal cosine accepted for a history or neighbor surface (25 degrees by default).
    pub normal_tolerance: f32,
    /// Ray origin offset along the surface normal, metres.
    pub ray_offset_m: f32,
    /// Extra offset per metre from the eye, for depth precision.
    pub ray_offset_per_m: f32,
    /// Revalidate the selected temporal path's hit and radiance every this many frames (0 = never).
    pub validation_interval: u32,
    /// Reject a periodic revalidation when its luminance changed by more than this fraction.
    pub validation_luminance_tolerance: f32,
    /// Denoiser settings; the same `gi_denoise.wgsl` passes consume them.
    pub denoise: crate::gi_denoise::GiDenoiseSettings,
}

impl Default for RestirGiSettings {
    fn default() -> Self {
        Self {
            temporal: true,
            history_cap: 30.0,
            spatial_taps: 4,
            spatial_radius_px: 16.0,
            spatial_cap: 500.0,
            depth_tolerance: 0.05,
            normal_tolerance: 25.0_f32.to_radians().cos(),
            ray_offset_m: 0.004,
            ray_offset_per_m: 0.0008,
            validation_interval: 6,
            validation_luminance_tolerance: 0.25,
            // Longer temporal memory than the shared denoiser's defaults: the per-pixel
            // estimate re-rolls its reservoirs every frame, and the spec 27 § 2.3 held-view
            // bound (frame-to-frame FLIP ≤ 0.01) needs the extra smoothing.
            denoise: crate::gi_denoise::GiDenoiseSettings {
                alpha_color: 0.08,
                alpha_moments: 0.08,
                max_history: 96.0,
                ..crate::gi_denoise::GiDenoiseSettings::default()
            },
        }
    }
}

impl RestirGiSettings {
    /// Maximum spatial taps, kept small enough for the shader's fixed contributor list.
    pub const MAX_SPATIAL_TAPS: u32 = 8;

    /// Checks every setting before any GPU object is created.
    ///
    /// # Errors
    /// [`RestirGiError::InvalidSetting`] naming the first invalid field.
    pub fn validate(&self) -> Result<(), RestirGiError> {
        let bad = |field| Err(RestirGiError::InvalidSetting(field));
        if !(self.history_cap.is_finite() && self.history_cap >= 1.0) {
            return bad("history_cap");
        }
        if self.spatial_taps > Self::MAX_SPATIAL_TAPS {
            return bad("spatial_taps");
        }
        if !(self.spatial_radius_px.is_finite() && self.spatial_radius_px >= 1.0) {
            return bad("spatial_radius_px");
        }
        if !(self.spatial_cap.is_finite() && self.spatial_cap >= 1.0) {
            return bad("spatial_cap");
        }
        if !(self.depth_tolerance.is_finite() && self.depth_tolerance > 0.0) {
            return bad("depth_tolerance");
        }
        if !(self.normal_tolerance.is_finite() && (-1.0..=1.0).contains(&self.normal_tolerance)) {
            return bad("normal_tolerance");
        }
        if !(self.ray_offset_m.is_finite() && self.ray_offset_m > 0.0) {
            return bad("ray_offset_m");
        }
        if !(self.ray_offset_per_m.is_finite() && self.ray_offset_per_m >= 0.0) {
            return bad("ray_offset_per_m");
        }
        if !(self.validation_luminance_tolerance.is_finite() && self.validation_luminance_tolerance > 0.0) {
            return bad("validation_luminance_tolerance");
        }
        self.denoise
            .validate()
            .map_err(|_| RestirGiError::InvalidSetting("denoise"))
    }
}

/// Why a requested frame stays on the probe path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestirGiFallback {
    /// The profile switch is off.
    #[default]
    Off,
    /// `EXPERIMENTAL_RAY_QUERY` is absent.
    NoRayQuery,
    /// `EXPERIMENTAL_RAY_HIT_VERTEX_RETURN` is absent.
    NoVertexReturn,
    /// `restir_gi_fallback` returns this when `main_view` is false. The frame recorder does not
    /// ask: a texture view returns before the support record is built, and the published report
    /// stays the last main view.
    SecondaryView,
    /// The frame did not record single-sampled depth. That is `msaa_samples != 1`, or one sample
    /// with the depth prepass off: the frame sets `RestirGiSupport::single_sample` only when both
    /// hold, and any false value is this reason.
    Multisampled,
    /// The pass needs the linear HDR target.
    NotLinearHdr,
    /// The probes are not active, so hit radiance has no source and Depth remains the fallback.
    NoProbes,
    /// The acceleration structure was refused or has not been built.
    NoTlas,
    /// An input failed validation.
    InvalidInput,
}

impl RestirGiFallback {
    /// Stable lowercase protocol reason (`render.restir_gi`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoRayQuery => "no_ray_query",
            Self::NoVertexReturn => "no_vertex_return",
            Self::SecondaryView => "secondary_view",
            Self::Multisampled => "multisampled",
            Self::NotLinearHdr => "not_linear_hdr",
            Self::NoProbes => "no_probes",
            Self::NoTlas => "no_tlas",
            Self::InvalidInput => "invalid_input",
        }
    }

    /// One sentence for logs and the agent.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Off => "ReSTIR GI is off; the frame uses the probe tier",
            Self::NoRayQuery => "the device has no hardware ray queries; the frame uses the probe tier",
            Self::NoVertexReturn => "the device has no hit-vertex return; the frame uses the probe tier",
            Self::SecondaryView => {
                "only the main view runs ReSTIR GI; a texture view leaves the last main-view report"
            }
            Self::Multisampled => {
                "ReSTIR GI needs a single-sample depth prepass; the frame uses the probe tier"
            }
            Self::NotLinearHdr => "ReSTIR GI needs the linear HDR target; the frame uses the probe tier",
            Self::NoProbes => "the probes are not active; the existing indirect path is unchanged",
            Self::NoTlas => {
                "the traced scene has no usable top-level structure; the frame uses the probe tier"
            }
            Self::InvalidInput => "ReSTIR GI input was invalid; the frame uses the probe tier",
        }
    }
}

/// Inputs of the device-independent frame selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestirGiSupport {
    /// The profile asks for ReSTIR GI.
    pub requested: bool,
    /// The device has `EXPERIMENTAL_RAY_QUERY`.
    pub ray_query: bool,
    /// The device has `EXPERIMENTAL_RAY_HIT_VERTEX_RETURN`.
    pub vertex_return: bool,
    /// This is the main view.
    pub main_view: bool,
    /// The depth buffer has one sample and the depth prepass ran. The frame sets this from
    /// `msaa_samples == 1 && depth_prepass`; a false value is reported as [`RestirGiFallback::Multisampled`].
    pub single_sample: bool,
    /// The frame uses the linear HDR target.
    pub linear_hdr: bool,
    /// The probe field is active.
    pub probes_active: bool,
    /// A built top-level structure is available.
    pub tlas: bool,
}

/// The first fallback of a frame, or `None` when ReSTIR GI can run.
#[must_use]
pub const fn restir_gi_fallback(s: RestirGiSupport) -> Option<RestirGiFallback> {
    if !s.requested {
        return Some(RestirGiFallback::Off);
    }
    if !s.ray_query {
        return Some(RestirGiFallback::NoRayQuery);
    }
    if !s.vertex_return {
        return Some(RestirGiFallback::NoVertexReturn);
    }
    if !s.main_view {
        return Some(RestirGiFallback::SecondaryView);
    }
    if !s.single_sample {
        return Some(RestirGiFallback::Multisampled);
    }
    if !s.linear_hdr {
        return Some(RestirGiFallback::NotLinearHdr);
    }
    if !s.probes_active {
        return Some(RestirGiFallback::NoProbes);
    }
    if !s.tlas {
        return Some(RestirGiFallback::NoTlas);
    }
    None
}

/// Last main-view state reported by `FrameRenderer::restir_gi_report`.
#[derive(Clone, Debug, PartialEq)]
pub struct RestirGiReport {
    /// The profile asks for the feature.
    pub requested: bool,
    /// The last frame ran the ray-query path and replaced the probe screen lookup.
    pub active: bool,
    /// Why the probe path was kept (`None` while active).
    pub fallback: Option<RestirGiFallback>,
    /// Ray-query frames recorded.
    pub frames: u64,
    /// History resets (camera cuts, resizes and setting changes).
    pub resets: u64,
    /// Per-size GPU resource rebuilds.
    pub resource_builds: u64,
    /// Input/parity bind-group rebuilds.
    pub bind_group_builds: u64,
    /// Last active lookup size.
    pub size: (u32, u32),
    /// Whether the next active frame can reproject reservoir and denoiser history.
    pub history_valid: bool,
    /// Latest asynchronous work counters, if one has completed.
    pub work: Option<RestirGiWork>,
    /// Sum of all completed work readbacks.
    pub total_work: RestirGiWork,
    /// Last preparation error, if any.
    pub last_error: Option<String>,
}

impl Default for RestirGiReport {
    fn default() -> Self {
        Self {
            requested: false,
            active: false,
            fallback: Some(RestirGiFallback::Off),
            frames: 0,
            resets: 0,
            resource_builds: 0,
            bind_group_builds: 0,
            size: (0, 0),
            history_valid: false,
            work: None,
            total_work: RestirGiWork::default(),
            last_error: None,
        }
    }
}

/// A settings or input error. [`Self::code`] is part of the agent protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestirGiError {
    /// A named setting is out of range.
    InvalidSetting(&'static str),
    /// An input's dimensions or resources do not agree.
    BadInput(String),
    /// The device lacks a required feature.
    Unsupported(&'static str),
}

impl RestirGiError {
    /// Stable lowercase protocol error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidSetting(_) => "restir_gi_invalid_setting",
            Self::BadInput(_) => "restir_gi_bad_input",
            Self::Unsupported(_) => "restir_gi_unsupported",
        }
    }
}

impl std::fmt::Display for RestirGiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSetting(field) => write!(f, "ReSTIR GI setting `{field}` is invalid"),
            Self::BadInput(message) => write!(f, "ReSTIR GI input: {message}"),
            Self::Unsupported(feature) => write!(f, "ReSTIR GI requires {feature}"),
        }
    }
}

impl std::error::Error for RestirGiError {}

#[cfg(test)]
pub(crate) fn surface_compatible(
    position: Vec3,
    normal: Vec3,
    depth: f32,
    other_position: Vec3,
    other_normal: Vec3,
    settings: &RestirGiSettings,
) -> bool {
    depth.is_finite()
        && depth > 0.0
        && position.is_finite()
        && other_position.is_finite()
        && normal.is_finite()
        && other_normal.is_finite()
        && normal.normalize_or_zero().dot(other_normal.normalize_or_zero()) >= settings.normal_tolerance
        && position.distance(other_position) <= settings.depth_tolerance * depth
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support() -> RestirGiSupport {
        RestirGiSupport {
            requested: true,
            ray_query: true,
            vertex_return: true,
            main_view: true,
            single_sample: true,
            linear_hdr: true,
            probes_active: true,
            tlas: true,
        }
    }

    #[test]
    fn selection_runs_only_with_every_requirement_and_names_each_fallback() {
        assert_eq!(restir_gi_fallback(support()), None);
        let cases = [
            ("requested", RestirGiFallback::Off),
            ("ray_query", RestirGiFallback::NoRayQuery),
            ("vertex_return", RestirGiFallback::NoVertexReturn),
            ("main_view", RestirGiFallback::SecondaryView),
            ("single_sample", RestirGiFallback::Multisampled),
            ("linear_hdr", RestirGiFallback::NotLinearHdr),
            ("probes_active", RestirGiFallback::NoProbes),
            ("tlas", RestirGiFallback::NoTlas),
        ];
        for (field, expected) in cases {
            let mut s = support();
            match field {
                "requested" => s.requested = false,
                "ray_query" => s.ray_query = false,
                "vertex_return" => s.vertex_return = false,
                "main_view" => s.main_view = false,
                "single_sample" => s.single_sample = false,
                "linear_hdr" => s.linear_hdr = false,
                "probes_active" => s.probes_active = false,
                "tlas" => s.tlas = false,
                _ => unreachable!(),
            }
            assert_eq!(restir_gi_fallback(s), Some(expected), "{field}");
        }
    }

    #[test]
    fn settings_and_reason_codes_are_stable() {
        RestirGiSettings::default().validate().expect("defaults");
        assert!(
            RestirGiSettings {
                spatial_taps: 9,
                ..RestirGiSettings::default()
            }
            .validate()
            .is_err()
        );
        for reason in [
            RestirGiFallback::Off,
            RestirGiFallback::NoRayQuery,
            RestirGiFallback::NoVertexReturn,
            RestirGiFallback::SecondaryView,
            RestirGiFallback::Multisampled,
            RestirGiFallback::NotLinearHdr,
            RestirGiFallback::NoProbes,
            RestirGiFallback::NoTlas,
            RestirGiFallback::InvalidInput,
        ] {
            assert!(
                reason
                    .as_str()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_')
            );
            assert!(!reason.message().is_empty());
        }
    }

    #[test]
    fn history_rejects_depth_normal_and_non_finite_changes() {
        let s = RestirGiSettings::default();
        let p = Vec3::new(0.0, 1.0, -4.0);
        let n = Vec3::Y;
        assert!(surface_compatible(p, n, 4.0, p + Vec3::X * 0.1, n, &s));
        assert!(!surface_compatible(p, n, 4.0, p + Vec3::X * 0.21, n, &s));
        assert!(!surface_compatible(p, n, 4.0, p, Vec3::X, &s));
        assert!(!surface_compatible(p, n, 4.0, Vec3::splat(f32::NAN), n, &s));
        assert!(!surface_compatible(p, n, f32::INFINITY, p, n, &s));
    }
}
