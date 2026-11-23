// SPDX-License-Identifier: MPL-2.0
//! Screen-space subsurface scattering for skin and foliage (ADR-2049).
//!
//! Light that enters a translucent surface leaves it some way off. A material with
//! [`SubsurfaceFactors`] draws its diffuse light into its own target; the frame blurs that target
//! with the profile's diffusion kernel and adds it back, so shadow edges soften and turn the
//! profile's color while the albedo, the specular and the silhouette stay sharp.
//!
//! **Frame.** Scattering materials draw in their own pass with three color targets
//! (`frame/subsurface_gpu.rs`), then two blur passes add them to the HDR image, then the forward
//! pass draws everything else over that image. Blended surfaces in front of the skin therefore
//! see the finished skin. The forward pipelines of every other draw do not change, and a frame
//! with no scattering draw records none of this.
//!
//! **Cost.** Nothing unless a render profile asks for it
//! ([`crate::RenderProfile::subsurface_scattering`], off in every preset) and a visible material
//! uses it. The blur then runs inside the screen bounds of the scattering draws. A frame that cannot scatter draws the material without it and says why
//! ([`SubsurfaceStats::reason`]): a skin material keeps its pre-integrated falloff
//! ([`crate::shading_model`]), a plain material is plainly diffuse.
//!
//! **Model.** Diffuse light is blurred before the albedo goes on (post-scatter texturing), with
//! a kernel from a sum of Gaussians per channel: d'Eon & Luebke 2007 for skin, an authored mixture
//! for leaves. The kernel is the radial profile's marginal, used in two 1D passes (Jimenez et al.
//! 2015): a straight edge along an axis sees the exact response, a diagonal one a slightly squarer
//! one ([`separable_error`]). Tests hold both to the integral.

use crate::shading_model::SKIN_PROFILE;

/// Taps of one kernel row (`TAPS` in `subsurface_scatter.wgsl`).
pub const SUBSURFACE_TAPS: usize = 17;

/// Profiles in the kernel table (`PROFILES` in `subsurface_scatter.wgsl`). The albedo target's alpha
/// tells two apart by its sign: a third needs a way to carry it there.
pub const SUBSURFACE_PROFILES: usize = 2;

/// The authored leaf profile: Gaussian variances in mm² and their RGB weights. Green travels
/// furthest through chlorophyll, red and blue are absorbed sooner. Authored for the look, not
/// measured.
pub const FOLIAGE_PROFILE: [(f32, [f32; 3]); 2] = [(0.25, [0.7, 0.35, 0.8]), (2.25, [0.3, 0.65, 0.2])];

/// Which diffusion profile a material scatters with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubsurfaceProfile {
    /// No scattering.
    #[default]
    Off,
    /// Human skin ([`SKIN_PROFILE`], d'Eon & Luebke 2007).
    Skin,
    /// Leaves and thin plant tissue ([`FOLIAGE_PROFILE`], authored).
    Foliage,
}

impl SubsurfaceProfile {
    /// Lower-case name (`off`, `skin`, `foliage`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Skin => "skin",
            Self::Foliage => "foliage",
        }
    }

    /// The name's profile, `None` for an unknown name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "skin" => Some(Self::Skin),
            "foliage" => Some(Self::Foliage),
            _ => None,
        }
    }

    /// The word the shader reads: 0 is no scattering, then the row of the kernel table plus 1.
    #[must_use]
    pub const fn word(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Skin => 1,
            Self::Foliage => 2,
        }
    }

    /// The Gaussians (variance in mm², RGB weights) at the profile's own reach.
    #[must_use]
    pub fn gaussians(self) -> &'static [(f32, [f32; 3])] {
        match self {
            Self::Off => &[],
            Self::Skin => &SKIN_PROFILE,
            Self::Foliage => &FOLIAGE_PROFILE,
        }
    }

    /// The profile's own reach in millimeters: three standard deviations of its widest Gaussian.
    #[must_use]
    pub fn reach_mm(self) -> f32 {
        self.gaussians().iter().fold(0.0, |widest, (variance, _)| {
            f32::max(widest, 3.0 * variance.sqrt())
        })
    }
}

/// A material's subsurface scattering. [`Self::OFF`] (the default) draws exactly as without it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubsurfaceFactors {
    /// The diffusion profile. [`SubsurfaceProfile::Off`] is no scattering.
    pub profile: SubsurfaceProfile,
    /// Reach in millimeters on a world in meters. `0` takes [`SubsurfaceProfile::reach_mm`].
    pub radius_mm: f32,
    /// Share of the light that scatters per channel, `0..=1`, linear. The rest stays where it
    /// landed; black is no scattering.
    pub tint: [f32; 3],
}

impl SubsurfaceFactors {
    /// No scattering.
    pub const OFF: Self = Self {
        profile: SubsurfaceProfile::Off,
        radius_mm: 0.0,
        tint: [1.0; 3],
    };

    /// `profile` at its own reach, every channel scattering.
    #[must_use]
    pub const fn new(profile: SubsurfaceProfile) -> Self {
        Self {
            profile,
            radius_mm: 0.0,
            tint: [1.0; 3],
        }
    }

    /// Whether the material scatters at all.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.profile != SubsurfaceProfile::Off && self.tint.iter().any(|&c| c > 0.0)
    }

    /// The reach this material uses, millimeters. Out-of-range values fall back to the profile's.
    #[must_use]
    pub fn reach_mm(&self) -> f32 {
        if self.radius_mm.is_finite() && self.radius_mm > 0.0 {
            self.radius_mm
        } else {
            self.profile.reach_mm()
        }
    }

    /// The material uniform's parts: the profile word (`shading.z`) and `shading_c` (reach in
    /// world units, then the tint). Out-of-range values are clamped.
    #[must_use]
    pub(crate) fn pack(&self) -> (f32, [f32; 4]) {
        if !self.is_on() {
            return (0.0, [0.0; 4]);
        }
        let channel = |c: f32| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 };
        (
            self.profile.word() as f32,
            [
                self.reach_mm() / 1000.0,
                channel(self.tint[0]),
                channel(self.tint[1]),
                channel(self.tint[2]),
            ],
        )
    }
}

impl Default for SubsurfaceFactors {
    fn default() -> Self {
        Self::OFF
    }
}

/// What the frame did with subsurface scattering (`render.subsurface`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubsurfaceStats {
    /// The frame's profile asks for it.
    pub requested: bool,
    /// Draws scattered in the last frame.
    pub drawn: usize,
    /// Draws of a scattering material drawn without it because the frame could not scatter or its
    /// pipelines were still building.
    pub fallback: usize,
    /// Draws of a scattering material this path cannot shade (a material graph, a transmissive or
    /// blended surface, a thin film, a lightmap receiver, a render override).
    pub unsupported: usize,
    /// [`Self::unsupported`] by reason, in the order of [`SubsurfaceUnsupported::ALL`]
    /// ([`Self::unsupported_reasons`], ADR-3170).
    pub unsupported_by: [u32; SubsurfaceUnsupported::COUNT],
    /// Frames that have run the blur.
    pub passes: u64,
    /// The share of the HDR image the last blur's rectangle covered, in thousandths.
    pub covered_permille: u32,
    /// Why the last frame did not scatter, `None` when it did. One of `profile_off`, `no_material`,
    /// `no_hdr_target`, `msaa`, `forward_lite`, `pipelines_building`, `no_visible_draw`.
    pub reason: Option<&'static str>,
}

/// Where the blur lays its kernel out ([`crate::RenderProfile::subsurface_kernel`], ADR-3170).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SubsurfaceKernel {
    /// The kernel's reach is the same number of pixels along both screen axes (ADR-2049): a surface
    /// seen at a grazing angle blurs over more of itself along its slope than the profile says, and
    /// only the depth test narrows it. The default: frames do not move.
    #[default]
    Screen,
    /// The kernel lies on the surface's tangent plane (Mikkelsen 2018): each pass measures how much
    /// surface one pixel spans along its axis from the depth of the neighboring pixels on the
    /// scattering surface, so the reach on screen shrinks with the slope, and the depth test
    /// measures a tap's distance from that plane instead of from the center's depth.
    Tangent,
}

impl SubsurfaceKernel {
    /// Lower-case name (`screen`, `tangent`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Tangent => "tangent",
        }
    }

    /// The name's kernel, `None` for an unknown name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "screen" => Some(Self::Screen),
            "tangent" => Some(Self::Tangent),
            _ => None,
        }
    }

    /// The word the blur reads (`size.w` of `ScatterParams`): 0 for [`Self::Screen`]; for
    /// [`Self::Tangent`] the width of the frame's internal view in pixels (at least 1), whose center
    /// is the projection's principal point.
    #[must_use]
    pub fn word(self, internal_width: u32) -> f32 {
        match self {
            Self::Screen => 0.0,
            Self::Tangent => internal_width.max(1) as f32,
        }
    }
}

/// World length of the scattering surface one pixel spans along a pass's axis, the tangent-plane
/// kernel's scale (`surface_step` in `subsurface_scatter.wgsl`). `lateral` is the world length one
/// pixel spans across the view ray at the pixel's distance (distance / focal length in pixels,
/// 1 / pixels per unit on an orthographic view), `depth_slope` the view depth's change per pixel
/// along the axis, `ray_slope` the view ray's slope along the axis (the pixel's offset from the
/// principal point over the focal length; 0 on an orthographic view). A surface facing the view
/// spans `lateral`; one at `θ` from it spans `lateral / cos θ`.
#[must_use]
pub fn tangent_step(lateral: f32, depth_slope: f32, ray_slope: f32) -> f32 {
    let along = lateral + ray_slope * depth_slope;
    (along * along + depth_slope * depth_slope).sqrt()
}

/// Why a draw of a scattering material was drawn without the blur (the frame's per-reason counts,
/// `unsupported_by` in `render.subsurface`; `book/agents/subsurface.md`). The codes are part of the
/// agent protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubsurfaceUnsupported {
    /// The eye model keeps its own block (`eye`).
    Eye,
    /// A material graph shades the surface (`material_graph`).
    MaterialGraph,
    /// A transmissive surface (`transmissive`).
    Transmissive,
    /// A blended surface (`blended`).
    Blended,
    /// An unlit material (`unlit`).
    Unlit,
    /// The hair model (`hair`).
    Hair,
    /// A thin film (`thin_film`).
    ThinFilm,
    /// An alpha-masked instance drawn through the mask path (`masked`).
    Masked,
    /// A lightmap receiver (`lightmap`).
    Lightmap,
    /// A Mobile lite forward frame (`forward_lite`).
    ForwardLite,
    /// A render override (x-ray, highlight) is on the draw (`render_override`).
    RenderOverride,
}

impl SubsurfaceUnsupported {
    /// How many reasons there are (the length of the per-reason counts).
    pub const COUNT: usize = 11;

    /// Every reason, in the order of the per-reason counts.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Eye,
        Self::MaterialGraph,
        Self::Transmissive,
        Self::Blended,
        Self::Unlit,
        Self::Hair,
        Self::ThinFilm,
        Self::Masked,
        Self::Lightmap,
        Self::ForwardLite,
        Self::RenderOverride,
    ];

    /// The stable lowercase code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Eye => "eye",
            Self::MaterialGraph => "material_graph",
            Self::Transmissive => "transmissive",
            Self::Blended => "blended",
            Self::Unlit => "unlit",
            Self::Hair => "hair",
            Self::ThinFilm => "thin_film",
            Self::Masked => "masked",
            Self::Lightmap => "lightmap",
            Self::ForwardLite => "forward_lite",
            Self::RenderOverride => "render_override",
        }
    }

    /// The reason's slot in the per-reason counts.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Why this path cannot scatter `material`, from the material alone (`None` when it can).
    /// `eye` is whether the entity shades with the eye model. What depends on the draw (a masked
    /// instance, a lightmap, a render override) or the frame (a lite tier) is not known here.
    #[must_use]
    pub fn of_material(material: &crate::LabMaterial, eye: bool) -> Option<Self> {
        if eye {
            Some(Self::Eye)
        } else if material.surface_graph.is_some() {
            Some(Self::MaterialGraph)
        } else if material.transmission.is_nan() || material.transmission > 0.0 {
            Some(Self::Transmissive)
        } else if material.alpha_mode >= crate::ALPHA_BLEND {
            Some(Self::Blended)
        } else if material.unlit {
            Some(Self::Unlit)
        } else if material.hair.enabled {
            Some(Self::Hair)
        } else if material.thin_film.is_on() {
            Some(Self::ThinFilm)
        } else {
            None
        }
    }
}

/// What a frame knows about one draw of a scattering material beyond the material itself
/// ([`SubsurfaceUnsupported::of_draw`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubsurfaceDrawFacts {
    /// The entity shades with the eye model.
    pub eye: bool,
    /// The draw blends (its alpha mode is [`crate::ALPHA_BLEND`] or above).
    pub blended: bool,
    /// The draw goes through the alpha-mask path.
    pub masked: bool,
    /// The draw has a thin film this frame.
    pub thin_film: bool,
    /// The draw receives a lightmap.
    pub lightmap: bool,
    /// The frame runs a Mobile lite forward tier.
    pub forward_lite: bool,
    /// A render override (x-ray, highlight) is on the draw.
    pub render_override: bool,
}

impl SubsurfaceUnsupported {
    /// Why the frame draws this draw of `material` without the blur, `None` when it scatters it:
    /// [`Self::of_material`]'s reasons with the draw's own blend and film, then what depends on
    /// the draw and the frame.
    #[must_use]
    pub fn of_draw(material: &crate::LabMaterial, facts: SubsurfaceDrawFacts) -> Option<Self> {
        if facts.eye {
            Some(Self::Eye)
        } else if material.surface_graph.is_some() {
            Some(Self::MaterialGraph)
        } else if material.transmission.is_nan() || material.transmission > 0.0 {
            Some(Self::Transmissive)
        } else if facts.blended {
            Some(Self::Blended)
        } else if material.unlit {
            Some(Self::Unlit)
        } else if material.hair.enabled {
            Some(Self::Hair)
        } else if facts.thin_film {
            Some(Self::ThinFilm)
        } else if facts.masked {
            Some(Self::Masked)
        } else if facts.lightmap {
            Some(Self::Lightmap)
        } else if facts.forward_lite {
            Some(Self::ForwardLite)
        } else if facts.render_override {
            Some(Self::RenderOverride)
        } else {
            None
        }
    }
}

/// The frame-level reasons [`SubsurfaceStats::reason`] can hold, in the order the frame checks
/// them. Stable lowercase codes, documented in `book/agents/subsurface.md`.
pub const SUBSURFACE_FRAME_REASONS: [&str; 7] = [
    "profile_off",
    "no_material",
    "no_hdr_target",
    "msaa",
    "forward_lite",
    "pipelines_building",
    "no_visible_draw",
];

/// The share of light a slab of the profile's tissue `thickness_mm` thick lets through, per
/// channel, for a material of reach `reach_mm`: the radial profile integrated over the plane at
/// that depth, `Σ w_i exp(-d² / 2v_i) / Σ w_i` (each Gaussian is normalized over the plane, so a
/// plane at `d` receives `exp(-d² / 2v)` of it). After the thin-slab transmittance of Jimenez et al.
/// 2010. 1 at no thickness; the terrain's grass reads it for light through a blade
/// (`regia_terrain`'s `grass.wgsl`, ADR-3170).
#[must_use]
pub fn slab_transmission(profile: SubsurfaceProfile, thickness_mm: f32, reach_mm: f32) -> [f32; 3] {
    let scale = reach_mm.max(1e-6) / profile.reach_mm().max(1e-6);
    let gaussians = profile.gaussians();
    if gaussians.is_empty() {
        return [0.0; 3];
    }
    let d2 = thickness_mm * thickness_mm;
    [0, 1, 2].map(|ch| {
        let (mut sum, mut total) = (0.0f32, 0.0f32);
        for (variance, w) in gaussians {
            sum += w[ch] * (-0.5 * d2 / (variance * scale * scale)).exp();
            total += w[ch];
        }
        sum / total.max(1e-6)
    })
}

impl SubsurfaceStats {
    /// The reasons the last frame drew scattering materials without the blur, with their counts:
    /// only the reasons that occurred, in the order of [`SubsurfaceUnsupported::ALL`].
    pub fn unsupported_reasons(&self) -> impl Iterator<Item = (SubsurfaceUnsupported, u32)> + '_ {
        SubsurfaceUnsupported::ALL
            .into_iter()
            .map(|reason| (reason, self.unsupported_by[reason.index()]))
            .filter(|&(_, count)| count > 0)
    }
}

/// The kernel table the blur reads: `[profile word - 1][tap]`, xyz = weights per channel (each
/// sums to 1), w = offset in units of the reach.
#[must_use]
pub fn kernel_table() -> [[f32; 4]; SUBSURFACE_TAPS * SUBSURFACE_PROFILES] {
    let mut out = [[0.0; 4]; SUBSURFACE_TAPS * SUBSURFACE_PROFILES];
    for (row, profile) in [SubsurfaceProfile::Skin, SubsurfaceProfile::Foliage]
        .into_iter()
        .enumerate()
    {
        let taps = kernel_taps(profile);
        out[row * SUBSURFACE_TAPS..(row + 1) * SUBSURFACE_TAPS].copy_from_slice(&taps);
    }
    out
}

/// The taps of `profile`, in order of offset: denser toward the center (offset `t·|t|`, `t` evenly
/// spaced), each weighing the marginal's integral over the span nearest to it. The outer spans
/// run to infinity, so every channel sums to 1.
#[must_use]
pub fn kernel_taps(profile: SubsurfaceProfile) -> [[f32; 4]; SUBSURFACE_TAPS] {
    let half = (SUBSURFACE_TAPS / 2) as f32;
    let offset = |i: usize| {
        let t = (i as f32 - half) / half;
        t * t.abs()
    };
    let reach = f64::from(profile.reach_mm().max(1e-6));
    let mut out = [[0.0; 4]; SUBSURFACE_TAPS];
    for (i, tap) in out.iter_mut().enumerate() {
        let lower = if i == 0 {
            f64::NEG_INFINITY
        } else {
            f64::from(offset(i - 1) + offset(i)) * 0.5
        };
        let upper = if i == SUBSURFACE_TAPS - 1 {
            f64::INFINITY
        } else {
            f64::from(offset(i) + offset(i + 1)) * 0.5
        };
        for (ch, weight) in tap.iter_mut().take(3).enumerate() {
            *weight =
                (marginal_cdf(profile, upper * reach, ch) - marginal_cdf(profile, lower * reach, ch)) as f32;
        }
        tap[3] = offset(i);
    }
    out
}

/// The share of channel `ch`'s scattered light that lands before `x_mm` along an axis: the radial
/// profile integrated over the half plane, at the profile's own reach.
fn marginal_cdf(profile: SubsurfaceProfile, x_mm: f64, ch: usize) -> f64 {
    let gaussians = profile.gaussians();
    let total: f64 = gaussians.iter().map(|(_, w)| f64::from(w[ch])).sum();
    gaussians
        .iter()
        .map(|(variance, w)| f64::from(w[ch]) * normal_cdf(x_mm / f64::from(*variance).sqrt()))
        .sum::<f64>()
        / total.max(1e-12)
}

/// The standard normal's CDF (Abramowitz & Stegun 26.2.17, error under 7.5e-8).
fn normal_cdf(x: f64) -> f64 {
    if x.is_infinite() {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.231_641_9 * z);
    let poly = t
        * (0.319_381_530
            + t * (-0.356_563_782 + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    let tail = (-0.5 * z * z).exp() / (std::f64::consts::TAU).sqrt() * poly;
    if x >= 0.0 { 1.0 - tail } else { tail }
}

/// The radial diffusion profile per channel at `r_mm`, per mm², normalized over the plane:
/// `Σ w_i exp(-r² / 2v_i) / (2π v_i)`. `reach_mm` scales it as a material's radius does.
#[must_use]
pub fn radial_profile(profile: SubsurfaceProfile, r_mm: f32, reach_mm: f32) -> [f32; 3] {
    let scale = reach_mm.max(1e-6) / profile.reach_mm().max(1e-6);
    let mut out = [0.0f32; 3];
    for (variance, w) in profile.gaussians() {
        let v = variance * scale * scale;
        let g = (-0.5 * r_mm * r_mm / v).exp() / (std::f32::consts::TAU * v);
        for ch in 0..3 {
            out[ch] += w[ch] * g;
        }
    }
    let total = profile.gaussians().iter().fold([0.0f32; 3], |sum, (_, w)| {
        [sum[0] + w[0], sum[1] + w[1], sum[2] + w[2]]
    });
    [0, 1, 2].map(|ch| out[ch] / total[ch].max(1e-6))
}

/// What a straight light edge turns into under the radial profile: the light reaching a point
/// `x_mm` inside the lit side, per channel (0.5 on the edge, 1 far inside). The analytic reference
/// of a plate with a hard shadow edge, for a radius of `reach_mm`.
#[must_use]
pub fn edge_response(profile: SubsurfaceProfile, x_mm: f32, reach_mm: f32) -> [f32; 3] {
    let scale = f64::from(reach_mm.max(1e-6) / profile.reach_mm().max(1e-6));
    [0, 1, 2].map(|ch| marginal_cdf(profile, f64::from(x_mm) / scale, ch) as f32)
}

/// [`edge_response`] through the discrete kernel the frame uses (one pass, a straight edge along
/// its axis), without the shader's noise, reading between pixels as it does: `reach_px` pixels per reach, the pixel
/// `x_px` pixels inside the lit side (pixel centers lit from `x_px >= 0`).
#[must_use]
pub fn kernel_edge_response(profile: SubsurfaceProfile, x_px: i32, reach_px: f32) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for tap in kernel_taps(profile) {
        // Between the pixel centers below and above, linearly.
        let position = x_px as f32 + tap[3] * reach_px;
        let lit = (position + 1.0).clamp(0.0, 1.0);
        for ch in 0..3 {
            out[ch] += tap[ch] * lit;
        }
    }
    out
}

/// How far the two 1D passes are from the radial profile: for channel `ch`, the integral of
/// `|k(x) k(y) - R(r)|` over the plane on a `reach`-wide grid. 0 would be exact; 2 is disjoint.
#[must_use]
pub fn separable_error(profile: SubsurfaceProfile, ch: usize) -> f32 {
    let reach = profile.reach_mm();
    let steps = 400i32;
    let dx = 2.0 * reach / steps as f32;
    let marginal = |x: f32| -> f32 {
        let gaussians = profile.gaussians();
        let total: f32 = gaussians.iter().map(|(_, w)| w[ch]).sum();
        gaussians
            .iter()
            .map(|(v, w)| w[ch] * (-0.5 * x * x / v).exp() / (std::f32::consts::TAU * v).sqrt())
            .sum::<f32>()
            / total.max(1e-6)
    };
    let mut sum = 0.0f32;
    for iy in 0..steps {
        let y = -reach + (iy as f32 + 0.5) * dx;
        let ky = marginal(y);
        for ix in 0..steps {
            let x = -reach + (ix as f32 + 0.5) * dx;
            let radial = radial_profile(profile, (x * x + y * y).sqrt(), reach)[ch];
            sum += (marginal(x) * ky - radial).abs() * dx * dx;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_black_are_no_scattering() {
        assert!(!SubsurfaceFactors::OFF.is_on());
        assert!(!SubsurfaceFactors::default().is_on());
        assert_eq!(SubsurfaceFactors::OFF.pack(), (0.0, [0.0; 4]));
        let black = SubsurfaceFactors {
            tint: [0.0; 3],
            ..SubsurfaceFactors::new(SubsurfaceProfile::Skin)
        };
        assert!(!black.is_on());
        assert_eq!(black.pack(), (0.0, [0.0; 4]));
    }

    #[test]
    fn pack_is_reach_in_meters_and_clamps_the_tint() {
        let factors = SubsurfaceFactors {
            profile: SubsurfaceProfile::Foliage,
            radius_mm: 4.0,
            tint: [2.0, f32::NAN, 0.5],
        };
        assert_eq!(factors.pack(), (2.0, [0.004, 1.0, 0.0, 0.5]));
        // A bad radius takes the profile's own reach.
        let nan = SubsurfaceFactors {
            radius_mm: f32::NAN,
            ..SubsurfaceFactors::new(SubsurfaceProfile::Skin)
        };
        assert_eq!(nan.reach_mm(), SubsurfaceProfile::Skin.reach_mm());
    }

    #[test]
    fn profile_names_round_trip() {
        for profile in [
            SubsurfaceProfile::Off,
            SubsurfaceProfile::Skin,
            SubsurfaceProfile::Foliage,
        ] {
            assert_eq!(SubsurfaceProfile::from_name(profile.name()), Some(profile));
        }
        assert_eq!(SubsurfaceProfile::from_name("wax"), None);
    }

    /// The shader indexes rows by the word: the table's order is the words' order.
    #[test]
    fn every_kernel_row_sums_to_one_per_channel_and_is_symmetric() {
        let table = kernel_table();
        for (word, profile) in [(1, SubsurfaceProfile::Skin), (2, SubsurfaceProfile::Foliage)] {
            assert_eq!(profile.word(), word);
            let row = &table[(word as usize - 1) * SUBSURFACE_TAPS..word as usize * SUBSURFACE_TAPS];
            for ch in 0..3 {
                let sum: f32 = row.iter().map(|tap| tap[ch]).sum();
                assert!((sum - 1.0).abs() < 1e-4, "{profile:?} channel {ch}: {sum}");
            }
            for i in 0..SUBSURFACE_TAPS {
                let (a, b) = (row[i], row[SUBSURFACE_TAPS - 1 - i]);
                assert!((a[3] + b[3]).abs() < 1e-6, "offsets {a:?} {b:?}");
                for ch in 0..3 {
                    assert!((a[ch] - b[ch]).abs() < 1e-4, "weights {a:?} {b:?}");
                }
            }
            assert_eq!(row[SUBSURFACE_TAPS / 2][3], 0.0);
            assert_eq!((row[0][3], row[SUBSURFACE_TAPS - 1][3]), (-1.0, 1.0));
        }
    }

    /// Red reaches furthest through skin, green through a leaf.
    #[test]
    fn the_profiles_spread_their_channels_as_measured() {
        let spread = |profile: SubsurfaceProfile, ch: usize| -> f32 {
            kernel_taps(profile).iter().map(|t| t[ch] * t[3] * t[3]).sum()
        };
        let skin = SubsurfaceProfile::Skin;
        assert!(spread(skin, 0) > 2.0 * spread(skin, 1), "skin red over green");
        assert!(spread(skin, 1) > spread(skin, 2), "skin green over blue");
        let leaf = SubsurfaceProfile::Foliage;
        assert!(
            spread(leaf, 1) > spread(leaf, 0) && spread(leaf, 1) > spread(leaf, 2),
            "leaf green"
        );
    }

    /// The radial profile really is normalized (the reference the frame is held to).
    #[test]
    fn the_radial_profile_integrates_to_one() {
        for (profile, reach) in [
            (SubsurfaceProfile::Skin, 8.0f32),
            (SubsurfaceProfile::Skin, 2.0),
            (SubsurfaceProfile::Foliage, 4.5),
        ] {
            let scale = reach / profile.reach_mm();
            for ch in 0..3 {
                let (mut sum, n) = (0.0f64, 40_000);
                let far = f64::from(profile.reach_mm() * scale * 4.0);
                for i in 0..n {
                    // ρ = t² puts the samples where the narrow Gaussians are.
                    let t = (f64::from(i) + 0.5) / f64::from(n);
                    let r = far * t * t;
                    let dr = far * 2.0 * t / f64::from(n);
                    sum += f64::from(radial_profile(profile, r as f32, reach)[ch])
                        * std::f64::consts::TAU
                        * r
                        * dr;
                }
                assert!(
                    (sum - 1.0).abs() < 3e-3,
                    "{profile:?} ch {ch} reach {reach}: {sum}"
                );
            }
        }
    }

    /// The edge response is the radial profile's integral over the half plane.
    #[test]
    fn the_edge_response_is_the_half_plane_integral() {
        let profile = SubsurfaceProfile::Skin;
        let reach = profile.reach_mm();
        for x in [-4.0f32, -1.0, -0.25, 0.0, 0.25, 1.0, 4.0] {
            let analytic = edge_response(profile, x, reach);
            let mut sum = [0.0f64; 3];
            let (extent, n) = (30.0f64, 1500);
            let d = 2.0 * extent / f64::from(n);
            // Lit where u < x: the integrand is the profile around the receiving point.
            for iv in 0..n {
                let v = -extent + (f64::from(iv) + 0.5) * d;
                for iu in 0..n {
                    let u = -extent + (f64::from(iu) + 0.5) * d;
                    if u < f64::from(x) {
                        let r = (u * u + v * v).sqrt() as f32;
                        let value = radial_profile(profile, r, reach);
                        for ch in 0..3 {
                            sum[ch] += f64::from(value[ch]) * d * d;
                        }
                    }
                }
            }
            for ch in 0..3 {
                assert!(
                    (f64::from(analytic[ch]) - sum[ch]).abs() < 0.02,
                    "x {x} ch {ch}: analytic {} integral {}",
                    analytic[ch],
                    sum[ch]
                );
            }
        }
    }

    /// The discrete kernel on the pixel grid stays close to the analytic edge, for a face
    /// filling the view and for one in the distance.
    #[test]
    fn the_kernel_follows_the_analytic_edge_at_every_scale() {
        let profile = SubsurfaceProfile::Skin;
        let reach = profile.reach_mm();
        for (reach_px, bound) in [(40.0f32, 0.05f32), (16.0, 0.06), (6.0, 0.12)] {
            let mut worst = 0.0f32;
            for x in -40..=40i32 {
                let x_mm = (x as f32 + 0.5) * reach / reach_px;
                let exact = edge_response(profile, x_mm, reach);
                let tapped = kernel_edge_response(profile, x, reach_px);
                for ch in 0..3 {
                    worst = worst.max((exact[ch] - tapped[ch]).abs());
                }
            }
            assert!(worst < bound, "reach {reach_px} px: worst {worst} ≥ {bound}");
        }
    }

    /// The price of two 1D passes: the product of marginals against the radial profile. A pure
    /// Gaussian would be exact; the skin mixture's wide red is the worst case.
    #[test]
    fn the_separable_kernel_stays_near_the_radial_profile() {
        for (profile, ch, bound) in [
            (SubsurfaceProfile::Skin, 0usize, 0.6f32),
            (SubsurfaceProfile::Skin, 1, 0.45),
            (SubsurfaceProfile::Skin, 2, 0.35),
            (SubsurfaceProfile::Foliage, 1, 0.4),
        ] {
            let error = separable_error(profile, ch);
            assert!(error < bound, "{profile:?} channel {ch}: {error} ≥ {bound}");
        }
    }

    #[test]
    fn kernel_names_and_words_round_trip() {
        for kernel in [SubsurfaceKernel::Screen, SubsurfaceKernel::Tangent] {
            assert_eq!(SubsurfaceKernel::from_name(kernel.name()), Some(kernel));
        }
        assert_eq!(SubsurfaceKernel::from_name("world"), None);
        assert_eq!(SubsurfaceKernel::default(), SubsurfaceKernel::Screen);
        // The screen kernel's word is the word every frame wrote before the switch.
        assert_eq!(SubsurfaceKernel::Screen.word(1920).to_bits(), 0.0f32.to_bits());
        assert_eq!(SubsurfaceKernel::Tangent.word(1920), 1920.0);
        assert_eq!(SubsurfaceKernel::Tangent.word(0), 1.0);
    }

    /// A surface at θ from the view spans `1 / cos θ` of a facing one, whatever way it slopes.
    #[test]
    fn the_tangent_step_is_the_slope_of_the_surface() {
        for degrees in [0.0f32, 30.0, 45.0, 70.0, 85.0] {
            let theta = degrees.to_radians();
            let step = tangent_step(0.01, 0.01 * theta.tan(), 0.0);
            assert!(
                (step - 0.01 / theta.cos()).abs() < 1e-6 * step.max(1.0),
                "{degrees}: {step}"
            );
            let down = tangent_step(0.01, -0.01 * theta.tan(), 0.0);
            assert!((down - step).abs() < 1e-7, "{degrees}");
        }
        // Off the view's center a ray is slanted: a plate square to that ray spans less of the
        // screen axis than its lateral width says (the view sees it face on along the ray).
        assert!(tangent_step(1.0, -0.5, 0.5) < tangent_step(1.0, 0.0, 0.5));
    }

    /// The tangent-plane kernel on a plate tilted away from the view, mirrored from
    /// `subsurface_scatter.wgsl`'s pass along the slope (orthographic, without the taps' noise):
    /// against the half-plane response measured on the surface, its error stays where it is on a
    /// facing plate, while the screen kernel's grows with the tilt.
    #[test]
    fn the_tangent_kernel_follows_a_slanted_edge_where_the_screen_kernel_does_not() {
        let profile = SubsurfaceProfile::Skin;
        let reach = profile.reach_mm();
        // World millimeters per pixel across the view: 40 pixels per reach on a facing plate.
        let pixel = reach / 40.0;
        let error = |kernel: SubsurfaceKernel, degrees: f32| -> f32 {
            let theta = degrees.to_radians();
            // Row `y` (continuous) is `y · pixel / cos θ` along the surface, `y · pixel · tan θ`
            // deeper; the edge is at row 0 and the lit side above it.
            let surface = |y: f32| y * pixel / theta.cos();
            let depth = |y: f32| y * pixel * theta.tan();
            let lit = |y: f32| if surface(y) >= 0.0 { 1.0f32 } else { 0.0 };
            let slope = pixel * theta.tan();
            let reach_px = match kernel {
                SubsurfaceKernel::Screen => reach / pixel,
                SubsurfaceKernel::Tangent => reach / tangent_step(pixel, slope, 0.0),
            };
            let (mut sum, mut n) = (0.0f32, 0);
            for i in -400..=400 {
                let y = i as f32 * 0.25;
                if surface(y).abs() > reach {
                    continue;
                }
                let mut out = [0.0f32; 3];
                for tap in kernel_taps(profile) {
                    let offset = tap[3] * reach_px;
                    let q = y + offset;
                    let expected = match kernel {
                        SubsurfaceKernel::Screen => depth(y),
                        SubsurfaceKernel::Tangent => depth(y) + slope * offset,
                    };
                    let step_over = (depth(q) - expected) / (0.5 * reach);
                    let keep = (-0.5 * step_over * step_over).exp();
                    let light = lit(y) + (lit(q) - lit(y)) * keep;
                    for ch in 0..3 {
                        out[ch] += tap[ch] * light;
                    }
                }
                let exact = edge_response(profile, surface(y), reach);
                for ch in 0..3 {
                    sum += (out[ch] - exact[ch]).abs();
                    n += 1;
                }
            }
            sum / n as f32
        };
        let facing = error(SubsurfaceKernel::Tangent, 0.0);
        assert!(
            (facing - error(SubsurfaceKernel::Screen, 0.0)).abs() < 1e-6,
            "facing plates agree"
        );
        // Measured: facing 0.0046; 45° screen 0.0070, tangent 0.0046; 70° screen 0.0184, tangent 0.0056.
        // The screen kernel's depth test narrows it at 45°, not enough at 70°.
        for (degrees, ratio) in [(45.0f32, 0.8f32), (70.0, 0.5)] {
            let (screen, tangent) = (
                error(SubsurfaceKernel::Screen, degrees),
                error(SubsurfaceKernel::Tangent, degrees),
            );
            eprintln!(
                "CPU mirror, {degrees}°: mean error screen {screen:.4}, tangent {tangent:.4} (facing {facing:.4})"
            );
            assert!(
                tangent < ratio * screen,
                "{degrees}°: tangent {tangent} screen {screen}"
            );
            assert!(
                tangent < facing + 0.002,
                "{degrees}°: tangent {tangent} facing {facing}"
            );
        }
    }

    #[test]
    fn unsupported_codes_are_stable_lowercase_and_in_order() {
        let mut seen = std::collections::HashSet::new();
        for (i, reason) in SubsurfaceUnsupported::ALL.into_iter().enumerate() {
            assert_eq!(reason.index(), i);
            let code = reason.code();
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(seen.insert(code), "{code} twice");
        }
        for code in SUBSURFACE_FRAME_REASONS {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    /// Every code the agent can see is on the agent page.
    #[test]
    fn every_code_is_documented_in_the_book() {
        let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/agents/subsurface.md");
        let text = std::fs::read_to_string(&page).expect("book/agents/subsurface.md");
        for code in SubsurfaceUnsupported::ALL
            .iter()
            .map(|r| r.code())
            .chain(SUBSURFACE_FRAME_REASONS)
            .chain([SubsurfaceKernel::Screen.name(), SubsurfaceKernel::Tangent.name()])
        {
            assert!(
                text.contains(&format!("`{code}`")),
                "`{code}` is not in {}",
                page.display()
            );
        }
    }

    #[test]
    fn the_material_reasons_follow_the_agents_order() {
        let base = crate::LabMaterial {
            subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
            ..crate::LabMaterial::default()
        };
        assert_eq!(SubsurfaceUnsupported::of_material(&base, false), None);
        assert_eq!(
            SubsurfaceUnsupported::of_material(&base, true),
            Some(SubsurfaceUnsupported::Eye)
        );
        let glass = crate::LabMaterial {
            transmission: 0.5,
            unlit: true,
            ..base.clone()
        };
        assert_eq!(
            SubsurfaceUnsupported::of_material(&glass, false),
            Some(SubsurfaceUnsupported::Transmissive),
            "the first reason wins"
        );
        let flat = crate::LabMaterial { unlit: true, ..base };
        assert_eq!(
            SubsurfaceUnsupported::of_material(&flat, false),
            Some(SubsurfaceUnsupported::Unlit)
        );
    }

    /// Each draw-level fact alone names its own reason; none is no reason.
    #[test]
    fn every_draw_fact_names_its_reason() {
        let material = crate::LabMaterial::default();
        assert_eq!(
            SubsurfaceUnsupported::of_draw(&material, SubsurfaceDrawFacts::default()),
            None
        );
        let one = |set: fn(&mut SubsurfaceDrawFacts)| {
            let mut facts = SubsurfaceDrawFacts::default();
            set(&mut facts);
            SubsurfaceUnsupported::of_draw(&material, facts)
        };
        use SubsurfaceUnsupported as U;
        assert_eq!(one(|f| f.eye = true), Some(U::Eye));
        // The frame's old predicate was `transmission <= 0.0`: a NaN does not scatter.
        let nan = crate::LabMaterial {
            transmission: f32::NAN,
            ..crate::LabMaterial::default()
        };
        assert_eq!(
            SubsurfaceUnsupported::of_draw(&nan, SubsurfaceDrawFacts::default()),
            Some(U::Transmissive)
        );
        assert_eq!(one(|f| f.blended = true), Some(U::Blended));
        assert_eq!(one(|f| f.masked = true), Some(U::Masked));
        assert_eq!(one(|f| f.thin_film = true), Some(U::ThinFilm));
        assert_eq!(one(|f| f.lightmap = true), Some(U::Lightmap));
        assert_eq!(one(|f| f.forward_lite = true), Some(U::ForwardLite));
        assert_eq!(one(|f| f.render_override = true), Some(U::RenderOverride));
        let hair = crate::LabMaterial {
            hair: crate::HairFactors {
                enabled: true,
                ..Default::default()
            },
            ..crate::LabMaterial::default()
        };
        assert_eq!(
            SubsurfaceUnsupported::of_draw(&hair, SubsurfaceDrawFacts::default()),
            Some(U::Hair)
        );
    }

    /// The slab transmittance is the radial profile integrated over a plane at the slab's depth.
    #[test]
    fn slab_transmission_is_the_profile_through_a_plane() {
        for (profile, reach) in [
            (SubsurfaceProfile::Foliage, 4.5f32),
            (SubsurfaceProfile::Skin, 8.0),
        ] {
            assert_eq!(slab_transmission(profile, 0.0, reach), [1.0; 3]);
            for depth in [0.3f32, 1.0, 2.5] {
                let analytic = slab_transmission(profile, depth, reach);
                for (ch, &expected) in analytic.iter().enumerate() {
                    let (mut sum, n) = (0.0f64, 40_000);
                    let far = f64::from(reach * 4.0);
                    for i in 0..n {
                        let t = (f64::from(i) + 0.5) / f64::from(n);
                        let r = far * t * t;
                        let dr = far * 2.0 * t / f64::from(n);
                        let at = (r * r + f64::from(depth * depth)).sqrt() as f32;
                        sum += f64::from(radial_profile(profile, at, reach)[ch])
                            * std::f64::consts::TAU
                            * r
                            * dr;
                    }
                    assert!(
                        (sum - f64::from(expected)).abs() < 3e-3,
                        "{profile:?} {depth} mm ch {ch}: {expected} vs {sum}"
                    );
                }
            }
        }
        // Through a leaf, green gets furthest; thicker passes less.
        let leaf = slab_transmission(SubsurfaceProfile::Foliage, 1.5, 4.5);
        assert!(leaf[1] > leaf[0] && leaf[1] > leaf[2], "{leaf:?}");
        let thick = slab_transmission(SubsurfaceProfile::Foliage, 3.0, 4.5);
        assert!((0..3).all(|ch| thick[ch] < leaf[ch]), "{thick:?} {leaf:?}");
        assert_eq!(slab_transmission(SubsurfaceProfile::Off, 1.0, 1.0), [0.0; 3]);
    }
}
