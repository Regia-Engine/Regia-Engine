// SPDX-License-Identifier: MPL-2.0
//! Specular indirect light from the probe fields (ADR-1103): the CPU mirror of
//! `probe_specular.wgsl`; keep the two in step.
//!
//! The probe light, baked ([`crate::baked_gi`]) or traced ([`crate::DdgiSettings`]), reaches the
//! forward pass as six irradiances / π toward ±x, ±y, ±z per half-resolution texel
//! ([`crate::DdgiScreenTexel`]). That is the cosine convolution of light linear in direction,
//! `L(l) = a + b·l`. For such light the average over any lobe `w(l)` is exactly `a + b·m`, where
//! `m = ∫ w(l) l dl / ∫ w(l) dl` is the lobe's mean vector. So the indirect specular reads the
//! probes once per pixel through the GGX specular lobe's mean vector ([`specular_lobe_vector`]):
//! a mirror reads the probe light in the mirror direction, a rough lobe leans to the normal and
//! averages over a wider cone, and the diffuse lookup is unchanged.
//!
//! Where the sky's image-based light is bound, its prefiltered map lends only its detail: the
//! ratio of the prefiltered sky to the sky's own light in the same lobe ([`specular_detail`],
//! capped at [`PROBE_SPECULAR_DETAIL_MAX`]) multiplies the probes' lobe light, the
//! normalization of Lazarov, "Getting More Physical in Call of Duty: Black Ops II" (SIGGRAPH 2013
//! course). The probes set the level and the color (so a reflection inside a canyon or a room
//! reflects its walls, not the open sky), the map adds what the smooth probe light cannot hold.
//!
//! Energy: the lobe is normalized (uniform light comes back unchanged at every roughness), the
//! gradient of a reconstructed field is held to what a non-negative field can have (`|b| ≤ 3a`),
//! the result is clamped at 0, and the detail ratio's cap only removes light. The tests below
//! check each of these against numerical integration of the lobe.

use regia_core::math::Vec3;

/// `probe_specular.wgsl`, appended to the probe shading module ([`crate::ddgi_shading_wgsl`]).
pub(crate) const PROBE_SPECULAR_WGSL: &str = include_str!("../shaders/probe_specular.wgsl");

/// `DdgiUniform::ctrl.w` bit: the forward pass reads its indirect specular through the probes'
/// lobe light ([`ProbeSpecular::Lobe`]).
pub(crate) const CTRL_PROBE_SPECULAR: u32 = 1 << 13;

/// `DdgiUniform::ctrl.w` bit: the lobe light keeps its hue and the image's ground plane reaches it
/// ([`ProbeSpecular::LobeHue`]; `PSPEC_CTRL_HUE` in the shader).
pub(crate) const CTRL_PROBE_SPECULAR_HUE: u32 = 1 << 7;

/// Cap on the sky prefilter's detail ratio ([`specular_detail`]; `PSPEC_DETAIL_MAX` in the
/// shader). The ratio is the prefiltered sky over the sky's smooth light in the same lobe; a
/// clear sky stays well under it, and the cap only removes light.
pub const PROBE_SPECULAR_DETAIL_MAX: f32 = 8.0;

/// Largest radiance gradient over the mean a non-negative field can have (`|b| ≤ 3a`).
const MAX_GRADIENT: f32 = 3.0;
/// Radiance gradient over the irradiance / π gradient (the clamped cosine keeps 2/3 of band 1).
const DECONVOLVE: f32 = 1.5;

/// Share of its channel's mean a lobe keeps under [`ProbeSpecular::LobeHue`] (`PSPEC_HUE_FLOOR` in
/// the shader). The linear form goes negative where the real light is merely dim: a mirror aimed
/// at the nadir under a blue sky drove blue to 0 first and green next, and left red.
pub const PROBE_SPECULAR_HUE_FLOOR: f32 = 0.2;
/// [`hue_agreement`]: below this cosine the two lights' colors differ (`PSPEC_HUE_AGREE_LOW`).
const HUE_AGREE_LOW: f32 = 0.99;
/// [`hue_agreement`]: above this cosine they agree (`PSPEC_HUE_AGREE_HIGH`).
const HUE_AGREE_HIGH: f32 = 0.9995;
/// [`hue_fade`]: at this share of its mean's luminance a lobe has its mean's color
/// (`PSPEC_HUE_FADE_LOW`).
const HUE_FADE_LOW: f32 = 0.4;
/// [`hue_fade`]: from this share up a lobe keeps its own color (`PSPEC_HUE_FADE_HIGH`).
const HUE_FADE_HIGH: f32 = 0.9;
/// Luminance weights (`PSPEC_LUMA`).
const LUMA: Vec3 = Vec3::new(0.2126, 0.7152, 0.0722);

/// The fit's constants, in the order the shader writes them (checked against the WGSL text).
const LEN_A: [f32; 2] = [3.673_155, 7.571_242];
const LEN_B: [f32; 2] = [3.719_715, 15.010_073];
const LEN_GRAZE: [f32; 2] = [0.128_804, 5.260_887];
const LERP_K: [f32; 5] = [1.970_79, 3.496_811, 4.880_938, 1.183_977, 0.858_009];

/// How the forward pass lights its indirect specular where probe light covers the surface
/// ([`crate::DdgiSettings::specular`], [`crate::BakedGiSettings::specular`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ProbeSpecular {
    /// Every roughness reads the probes' light through the GGX lobe's mean vector; the sky
    /// prefilter (when bound) adds its detail as a capped ratio. The default.
    #[default]
    Lobe,
    /// The look before ADR-1103: the sky prefilter, occluded by the probe/sky irradiance ratio,
    /// with the probes' irradiance toward the normal mixed in only above perceptual roughness
    /// 0.5. Kept for comparisons.
    RoughOnly,
    /// [`Self::Lobe`] with three changes for the light's color (opt-in, ADR-2996): each channel keeps
    /// [`PROBE_SPECULAR_HUE_FLOOR`] of its mean and a dim lobe takes the mean's color
    /// ([`cube_lobe_radiance_hue`]), the sky prefilter's
    /// detail is a ratio per channel where the probes and the sky agree in color
    /// ([`specular_detail_rgb`]), and the image's ground plane
    /// (`FrameRenderer::set_environment_ground`) reaches the lobe ([`ground_swap`]).
    LobeHue,
}

impl ProbeSpecular {
    /// The protocol name: `"lobe"`, `"rough_only"` or `"lobe_hue"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Lobe => "lobe",
            Self::RoughOnly => "rough_only",
            Self::LobeHue => "lobe_hue",
        }
    }

    /// The mode named `name` (as [`Self::name`] writes it), or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "lobe" => Some(Self::Lobe),
            "rough_only" => Some(Self::RoughOnly),
            "lobe_hue" => Some(Self::LobeHue),
            _ => None,
        }
    }

    /// Its `DdgiUniform::ctrl.w` bits.
    #[must_use]
    pub(crate) fn ctrl_bits(self) -> u32 {
        match self {
            Self::Lobe => CTRL_PROBE_SPECULAR,
            Self::RoughOnly => 0,
            Self::LobeHue => CTRL_PROBE_SPECULAR | CTRL_PROBE_SPECULAR_HUE,
        }
    }
}

/// The GGX specular lobe's mean vector, as a lean and a length ([`specular_lobe`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpecularLobe {
    /// How far the mean direction leans from the normal toward the mirror direction: the mean
    /// direction is `normalize(mix(n, r, lerp))`. 1 for a mirror, 0 at α = 1.
    pub lerp: f32,
    /// The mean vector's length: 1 for a mirror, about 0.62 at α = 1 and normal incidence.
    pub length: f32,
}

/// The fitted mean vector of the GGX specular lobe (BRDF × cosine, height-correlated Smith,
/// Fresnel 1) at `n_dot_v` and GGX `alpha` (α = perceptual²). Worst error of the vector against
/// numerical integration ([`ggx_lobe_reference`]): 0.036 for α in [0, 1] and n·v ≥ 0.1.
#[must_use]
pub fn specular_lobe(n_dot_v: f32, alpha: f32) -> SpecularLobe {
    let a = alpha.clamp(0.0, 1.0);
    let u = (1.0 - n_dot_v.clamp(0.0, 1.0)).max(1e-4);
    let rational = (1.0 + a * (LEN_A[0] + LEN_A[1] * a)) / (1.0 + a * (LEN_B[0] + LEN_B[1] * a));
    let length = (rational - LEN_GRAZE[0] * a.sqrt() * u.powf(LEN_GRAZE[1])).clamp(0.0, 1.0);
    let x = 1.0 - a;
    let [k1, k2, k3, k4, k5] = LERP_K;
    let den = 1.0 + a * (k1 + k2 * a) * u.powf(k3) * (1.0 - k4 * a) + k5 * a * u;
    let lerp = (x * (x.sqrt() + a) / den).clamp(0.0, 1.0);
    SpecularLobe { lerp, length }
}

/// The lobe's mean vector for unit normal `n` and mirror direction `r` (`probe_spec_lobe`).
#[must_use]
pub fn specular_lobe_vector(n: [f32; 3], r: [f32; 3], n_dot_v: f32, alpha: f32) -> [f32; 3] {
    let lobe = specular_lobe(n_dot_v, alpha);
    let n = Vec3::from(n);
    let d = n.lerp(Vec3::from(r), lobe.lerp);
    let dir = if d.length_squared() > 1e-12 {
        d.normalize()
    } else {
        n
    };
    (dir * lobe.length).to_array()
}

/// `a + k·(b·m)` held at `floor_share` of `a`, the gradient scaled back to `|b| ≤ 3a`.
fn channel(a: f32, b: Vec3, m: Vec3, floor_share: f32) -> f32 {
    let len = b.length();
    let k = if len > 1e-20 {
        (MAX_GRADIENT * a.max(0.0) / len).min(1.0)
    } else {
        1.0
    };
    (a + k * b.dot(m)).max(floor_share * a.max(0.0))
}

fn cube_lobe(e: &[[f32; 3]; 6], m: [f32; 3], floor_share: f32) -> [f32; 3] {
    let m = Vec3::from(m);
    std::array::from_fn(|c| {
        let a = e.iter().map(|v| v[c]).sum::<f32>() / 6.0;
        let k = 0.5 * DECONVOLVE;
        let b = Vec3::new(e[0][c] - e[1][c], e[2][c] - e[3][c], e[4][c] - e[5][c]) * k;
        channel(a, b, m, floor_share)
    })
}

/// Radiance in a lobe of mean vector `m` from the six irradiances / π of a screen texel
/// ([`crate::DdgiScreenTexel::e`]: +x, −x, +y, −y, +z, −z), as `probe_spec_cube_lobe`: the mean
/// of the six, plus the gradient (half the difference of each pair, × 3/2) along `m`.
#[must_use]
pub fn cube_lobe_radiance(e: &[[f32; 3]; 6], m: [f32; 3]) -> [f32; 3] {
    cube_lobe(e, m, 0.0)
}

/// The lobe light `v` of a field of mean `a`, its luminance kept, its color fading to the mean's as
/// its level falls below the mean's (`probe_spec_hue_fade`).
fn hue_fade(a: [f32; 3], v: [f32; 3]) -> [f32; 3] {
    let (a, v) = (Vec3::from(a), Vec3::from(v));
    let mean = a.dot(LUMA);
    if mean <= 1e-8 {
        return v.to_array();
    }
    let level = v.dot(LUMA) / mean;
    let t = ((level - HUE_FADE_LOW) / (HUE_FADE_HIGH - HUE_FADE_LOW)).clamp(0.0, 1.0);
    (a * level).lerp(v, t * t * (3.0 - 2.0 * t)).to_array()
}

/// [`cube_lobe_radiance`] under [`ProbeSpecular::LobeHue`]: each channel keeps
/// [`PROBE_SPECULAR_HUE_FLOOR`] of its mean, and the lobe's color fades to the mean's as its level
/// falls below the mean's (from 0.9 of the mean's luminance, fully at 0.4). The luminance is kept.
#[must_use]
pub fn cube_lobe_radiance_hue(e: &[[f32; 3]; 6], m: [f32; 3]) -> [f32; 3] {
    let mean = std::array::from_fn(|c| (e.iter().map(|v| v[c]).sum::<f32>() / 6.0).max(0.0));
    hue_fade(mean, cube_lobe(e, m, PROBE_SPECULAR_HUE_FLOOR))
}

/// Radiance in a lobe of mean-vector length `len` from the irradiance / π toward its direction
/// (`e_pos`) and away from it (`e_neg`), as `probe_spec_axis_lobe` reads the sky irradiance map.
#[must_use]
pub fn axis_lobe_radiance(e_pos: [f32; 3], e_neg: [f32; 3], len: f32) -> [f32; 3] {
    std::array::from_fn(|c| {
        let a = 0.5 * (e_pos[c] + e_neg[c]);
        let lim = MAX_GRADIENT * a.max(0.0);
        let b = (0.5 * DECONVOLVE * (e_pos[c] - e_neg[c])).clamp(-lim, lim);
        (a + b * len).max(0.0)
    })
}

/// [`axis_lobe_radiance`] under [`ProbeSpecular::LobeHue`] (`probe_spec_hue_axis_lobe`).
#[must_use]
pub fn axis_lobe_radiance_hue(e_pos: [f32; 3], e_neg: [f32; 3], len: f32) -> [f32; 3] {
    let mean: [f32; 3] = std::array::from_fn(|c| (0.5 * (e_pos[c] + e_neg[c])).max(0.0));
    let lobe = std::array::from_fn(|c| {
        let a = mean[c];
        let lim = MAX_GRADIENT * a;
        let b = (0.5 * DECONVOLVE * (e_pos[c] - e_neg[c])).clamp(-lim, lim);
        (a + b * len).max(PROBE_SPECULAR_HUE_FLOOR * a)
    });
    hue_fade(mean, lobe)
}

/// A lobe's light with what the image's ground plane hides there taken out and the plane's light
/// put in, each channel keeping [`PROBE_SPECULAR_HUE_FLOOR`] of itself
/// (`probe_spec_ground_swap`).
#[must_use]
pub fn ground_swap(lobe: [f32; 3], hidden: [f32; 3], add: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|c| (lobe[c] - hidden[c]).max(PROBE_SPECULAR_HUE_FLOOR * lobe[c]) + add[c])
}

/// How far the probes' lobe light and the sky's agree in color, 0 to 1 from the cosine between them
/// (`probe_spec_hue_agreement`). A wall, or the object itself hiding the sky from the probes, parts
/// them; only rounding and the map's band 2 part them under an open sky.
#[must_use]
pub fn hue_agreement(probe_lobe: [f32; 3], sky_lobe: [f32; 3]) -> f32 {
    let (p, s) = (Vec3::from(probe_lobe), Vec3::from(sky_lobe));
    let lengths = p.length_squared() * s.length_squared();
    if lengths <= 1e-20 {
        return 0.0;
    }
    let t = ((p.dot(s) / lengths.sqrt() - HUE_AGREE_LOW) / (HUE_AGREE_HIGH - HUE_AGREE_LOW)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The sky prefilter's detail per channel under [`ProbeSpecular::LobeHue`], each capped at
/// [`PROBE_SPECULAR_DETAIL_MAX`], 1 where the sky's channel is dark (`probe_spec_detail_rgb`). It
/// holds where the probes and the sky agree in color ([`hue_agreement`]); where they do not, it is
/// `luminance_detail`, because a per-channel ratio of a sky the probes do not see would print the
/// sky's complement on their light.
#[must_use]
pub fn specular_detail_rgb(
    prefilter: [f32; 3],
    sky_lobe: [f32; 3],
    probe_lobe: [f32; 3],
    luminance_detail: f32,
) -> [f32; 3] {
    let agreement = hue_agreement(probe_lobe, sky_lobe);
    std::array::from_fn(|c| {
        let per_channel = if sky_lobe[c] <= 1e-6 {
            1.0
        } else {
            (prefilter[c] / sky_lobe[c]).clamp(0.0, PROBE_SPECULAR_DETAIL_MAX)
        };
        luminance_detail + (per_channel - luminance_detail) * agreement
    })
}

/// The sky prefilter's detail over the sky's own light in the same lobe (luminances), capped
/// at [`PROBE_SPECULAR_DETAIL_MAX`]; 1 for a dark sky (`probe_spec_detail`).
#[must_use]
pub fn specular_detail(prefilter_lum: f32, sky_lobe_lum: f32) -> f32 {
    if sky_lobe_lum <= 1e-6 {
        return 1.0;
    }
    (prefilter_lum / sky_lobe_lum).clamp(0.0, PROBE_SPECULAR_DETAIL_MAX)
}

/// The GGX specular lobe integrated numerically ([`ggx_lobe_reference`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LobeReference {
    /// The lobe's mean vector in the frame n = +z, v = (√(1 − μ²), 0, μ): its x is ≤ 0 (the
    /// mirror side), its y is 0.
    pub mean: [f32; 3],
    /// The lobe's directional albedo with Fresnel 1 (≤ 1: what single scattering keeps).
    pub albedo: f32,
}

/// The GGX specular lobe (BRDF × cosine, height-correlated Smith, Fresnel 1) at `n_dot_v` and
/// `alpha`, integrated by the midpoint rule over the hemisphere (192 × 384 cells): its mean
/// vector and its albedo. The reference [`specular_lobe`] is fitted to: its mean is about 10⁻⁴
/// accurate for α ≥ 0.1 and n·v ≥ 0.15, its albedo within 0.005 of a visible-normal estimate for
/// α ≥ 0.2. A narrower lobe at grazing (α 0.05, n·v 0.1) needs finer cells.
#[must_use]
pub fn ggx_lobe_reference(n_dot_v: f32, alpha: f32) -> LobeReference {
    const THETA_STEPS: u32 = 192;
    const PHI_STEPS: u32 = 384;
    let a = f64::from(alpha.clamp(1e-3, 1.0));
    let a2 = a * a;
    let nv = f64::from(n_dot_v.clamp(1e-3, 1.0));
    let v = [(1.0 - nv * nv).max(0.0).sqrt(), 0.0, nv];
    let d_theta = std::f64::consts::FRAC_PI_2 / f64::from(THETA_STEPS);
    let d_phi = std::f64::consts::TAU / f64::from(PHI_STEPS);
    let mut sum = [0.0f64; 3];
    let mut total = 0.0f64;
    for i in 0..THETA_STEPS {
        let theta = (f64::from(i) + 0.5) * d_theta;
        let (st, nl) = theta.sin_cos();
        // The height-correlated Smith visibility G2 / (4 n·l n·v) (Heitz 2014).
        let lv = nv * (nl * nl * (1.0 - a2) + a2).sqrt();
        let ll = nl * (nv * nv * (1.0 - a2) + a2).sqrt();
        let vis = 0.5 / (lv + ll);
        for j in 0..PHI_STEPS {
            let phi = (f64::from(j) + 0.5) * d_phi;
            let l = [st * phi.cos(), st * phi.sin(), nl];
            let h = [l[0] + v[0], l[1] + v[1], l[2] + v[2]];
            let h_len = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
            let nh = h[2] / h_len;
            let t = nh * nh * (a2 - 1.0) + 1.0;
            let d = a2 / (std::f64::consts::PI * t * t);
            let w = d * vis * nl * st;
            for (s, li) in sum.iter_mut().zip(l) {
                *s += w * li;
            }
            total += w;
        }
    }
    let mean = if total > 0.0 {
        sum.map(|s| (s / total) as f32)
    } else {
        [0.0, 0.0, 1.0]
    };
    LobeReference {
        mean,
        albedo: (total * d_theta * d_phi) as f32,
    }
}

#[cfg(test)]
mod tests;
