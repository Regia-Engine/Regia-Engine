// SPDX-License-Identifier: MPL-2.0
//! Split-sum image-based lighting from the procedural sky.
//!
//! Specular prefiltering and the BRDF LUT follow Brian Karis, "Real Shading in the legacy engine4",
//! SIGGRAPH 2013 course notes. Diffuse irradiance is the cosine-weighted hemisphere integral of
//! the same sky (the diffuse term in those notes).

const SAMPLES: u32 = 24;
const PI: f32 = std::f32::consts::PI;

/// One RGBA8 image.
#[derive(Clone, Debug)]
pub struct IblImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA8, linear.
    pub rgba: Vec<u8>,
}

/// Irradiance, prefiltered specular mips and the split-sum BRDF LUT.
#[derive(Clone, Debug)]
pub struct IblMaps {
    /// Cosine-convolved sky.
    pub irradiance: IblImage,
    /// Specular mips, finest first.
    pub prefilter: Vec<IblImage>,
    /// RG stores scale and bias. BA are 1.
    pub brdf: IblImage,
    /// Unclipped RGBA16F texels of the irradiance and prefilter mips, when the bake keeps them
    /// (the studio gradient and equirect environments: 8-bit would band a smooth ramp seen
    /// in a mirror, and clip an HDR image). `None`: the upload widens the RGBA8 images.
    pub(crate) half: Option<IblHalf>,
    /// The environment's lower-hemisphere light, for an environment ground plane
    /// (`crate::env_ground`). `None`: this IBL takes no ground plane.
    pub(crate) lower: Option<IblLower>,
}

/// What an environment ground plane needs of the image light. The map goes up as mip 1 of the
/// irradiance texture, which no other lookup reads (they all take level 0), so a ground plane costs
/// no new binding.
#[derive(Clone, Debug)]
pub(crate) struct IblLower {
    /// Map width, half the irradiance map's.
    pub(crate) width: u32,
    /// Map height, half the irradiance map's.
    pub(crate) height: u32,
    /// RGBA16F bytes: per normal, the irradiance / π arriving from below the horizon only.
    pub(crate) half: Vec<u8>,
    /// Irradiance / π of a white horizontal surface under the upper hemisphere, cd/m²: a plane of
    /// albedo ρ shows ρ × this.
    pub(crate) plane_feed: [f32; 3],
}

/// RGBA16F bytes of the irradiance map and each prefilter mip (see [`IblMaps::half`]).
#[derive(Clone, Debug)]
pub(crate) struct IblHalf {
    /// The irradiance map.
    pub(crate) irradiance: Vec<u8>,
    /// One per prefilter mip, finest first.
    pub(crate) prefilter: Vec<Vec<u8>>,
}

/// Bakes IBL textures from [`sky_color`].
#[tracing::instrument]
pub fn build_procedural_ibl() -> IblMaps {
    let irradiance = equirect(64, 32, cosine_irradiance);
    let mut prefilter = Vec::new();
    let mut width = 64u32;
    let mut height = 32u32;
    let mip_count = 4u32;
    for mip in 0..mip_count {
        let roughness = mip as f32 / (mip_count - 1) as f32;
        prefilter.push(equirect(width, height, |dir| prefilter_specular(dir, roughness)));
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    IblMaps {
        irradiance,
        prefilter,
        brdf: build_brdf_lut(),
        half: None,
        lower: None,
    }
}

/// IBL maps of a uniform environment: radiance `value` × [`crate::SKY_IBL_SCALE`] from every
/// direction (`value` in 0..1 per channel; larger values clip). A uniform world, the flat sky of
/// a reference studio, convolves to itself, so the irradiance (stored as irradiance / π) and every
/// prefiltered mip hold `value`.
#[must_use]
pub fn build_uniform_ibl(value: [f32; 3]) -> IblMaps {
    let irradiance = equirect(64, 32, |_| value);
    let mut prefilter = Vec::new();
    let (mut width, mut height) = (64u32, 32u32);
    for _ in 0..4 {
        prefilter.push(equirect(width, height, |_| value));
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    IblMaps {
        irradiance,
        prefilter,
        brdf: build_brdf_lut(),
        half: None,
        lower: Some(two_tone_lower(value, value)),
    }
}

/// The lower-hemisphere light of an environment of radiance `sky` above the horizon and `ground`
/// below it (0..1 per channel, scaled as the maps are): a surface of normal `n` sees
/// `(1 − n.y) / 2` of its cosine-weighted hemisphere below the horizon.
fn two_tone_lower(sky: [f32; 3], ground: [f32; 3]) -> IblLower {
    let (width, height) = (32u32, 16u32);
    let mut texels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let n = dir_from_equirect((x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32);
            let down = (0.5 * (1.0 - n[1])).clamp(0.0, 1.0);
            texels.push(ground.map(|c| c * down));
        }
    }
    IblLower {
        width,
        height,
        half: rgb_to_rgba16f(&texels),
        plane_feed: sky.map(|c| c * crate::SKY_IBL_SCALE),
    }
}

/// IBL maps of a reference studio: a uniform sky of radiance `sky` above the horizon and a
/// uniform ground of radiance `ground` below it (both × [`crate::SKY_IBL_SCALE`], 0..1 per
/// channel). This is the environment a path tracer sees from an object standing on a large
/// diffuse plane under a flat world: the plane's first bounce stands in for the lower hemisphere,
/// as the procedural and atmosphere skies do for terrain. The irradiance is exact (a surface of
/// normal `n` sees `(1 + n.y) / 2` of its cosine-weighted hemisphere above the horizon); the
/// specular mips are GGX-prefiltered (Karis 2013) with 256 samples.
#[must_use]
pub fn build_studio_ibl(sky: [f32; 3], ground: [f32; 3]) -> IblMaps {
    let env = |d: [f32; 3]| if d[1] >= 0.0 { sky } else { ground };
    let irradiance = equirect(64, 32, |n| {
        let up = (0.5 * (1.0 + n[1])).clamp(0.0, 1.0);
        [0, 1, 2].map(|c| sky[c] * up + ground[c] * (1.0 - up))
    });
    let mut prefilter = Vec::new();
    let (mut width, mut height) = (64u32, 32u32);
    let mip_count = 4u32;
    for mip in 0..mip_count {
        let roughness = mip as f32 / (mip_count - 1) as f32;
        prefilter.push(equirect(width, height, |r| {
            if roughness <= 0.0 {
                return env(r);
            }
            let samples = 256;
            let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
            for i in 0..samples {
                let h = importance_sample_ggx(hammersley(i, samples), roughness, r);
                let l = reflect(r, h);
                let n_dot_l = dot(r, l);
                if n_dot_l > 0.0 {
                    let e = env(l);
                    for c in 0..3 {
                        sum[c] += e[c] * n_dot_l;
                    }
                    weight += n_dot_l;
                }
            }
            if weight <= 1e-5 {
                env(r)
            } else {
                sum.map(|v| v / weight)
            }
        }));
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    IblMaps {
        irradiance,
        prefilter,
        brdf: build_brdf_lut(),
        half: None,
        lower: Some(two_tone_lower(sky, ground)),
    }
}

/// The split-sum DFG table the forward pass samples (`brdf_lut`): 32×32 RGBA8, X = n·v,
/// Y = perceptual roughness, R = scale, G = bias (the image-based specular), B = the directional
/// albedo of the direct single-scatter GGX lobe with F = 1 ([`directional_albedo_ggx`]: the
/// height-correlated Smith term the punctual BRDF uses), which sets the punctual energy
/// compensation, A = that lobe's hemispherical average `E_avg = 2 ∫ E(μ) μ dμ` (constant along a
/// row), which sets how much of the missing energy a coloured conductor returns over several
/// bounces. The reference path tracer reads the same bytes (`specs/27` § 1.1).
#[must_use]
pub fn build_brdf_lut() -> IblImage {
    let brdf_size = 32u32;
    let mut brdf = vec![0u8; (brdf_size * brdf_size * 4) as usize];
    for y in 0..brdf_size {
        let roughness = (y as f32 + 0.5) / brdf_size as f32;
        // E(μ) along the row, then its cosine-weighted average (midpoint rule over the same μ).
        let e: Vec<f32> = (0..brdf_size)
            .map(|x| directional_albedo_ggx((x as f32 + 0.5) / brdf_size as f32, roughness))
            .collect();
        let e_avg = 2.0
            * e.iter()
                .enumerate()
                .map(|(x, e)| e * (x as f32 + 0.5) / brdf_size as f32)
                .sum::<f32>()
            / brdf_size as f32;
        for x in 0..brdf_size {
            let nov = (x as f32 + 0.5) / brdf_size as f32;
            let (scale, bias) = integrate_brdf(nov.clamp(0.05, 1.0), roughness.clamp(0.05, 1.0));
            let i = ((y * brdf_size + x) * 4) as usize;
            brdf[i] = to_u8(scale);
            brdf[i + 1] = to_u8(bias);
            brdf[i + 2] = to_u8(e[x as usize]);
            brdf[i + 3] = to_u8(e_avg);
        }
    }
    IblImage {
        width: brdf_size,
        height: brdf_size,
        rgba: brdf,
    }
}

fn equirect(width: u32, height: u32, f: impl Fn([f32; 3]) -> [f32; 3]) -> IblImage {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            let v = (y as f32 + 0.5) / height as f32;
            let color = f(dir_from_equirect(u, v));
            let i = ((y * width + x) * 4) as usize;
            rgba[i] = to_u8(color[0]);
            rgba[i + 1] = to_u8(color[1]);
            rgba[i + 2] = to_u8(color[2]);
            rgba[i + 3] = 255;
        }
    }
    IblImage { width, height, rgba }
}

pub(crate) fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Physical sky radiance (cd/m²) toward `dir`: the procedural sky scaled by
/// [`crate::SKY_IBL_SCALE`]. The irradiance probes use this for rays that escape,
/// so probe GI and the IBL agree on what the open sky contributes.
#[must_use]
pub(crate) fn sky_radiance(dir: [f32; 3]) -> [f32; 3] {
    sky_color(dir).map(|c| c * crate::SKY_IBL_SCALE)
}

/// Matches `sky_color` in `utils.wgsl`.
///
/// Below the horizon is a neutral ground at the radiance of an albedo-0.25 surface
/// under the default ~100 klx sun (≈ 0.33 · [`crate::SKY_IBL_SCALE`]), so the lower
/// half of a prop is lit by ground bounce instead of the old near-black brown.
fn sky_color(dir: [f32; 3]) -> [f32; 3] {
    let t = dir[1];
    let horizon = [0.78, 0.85, 0.95];
    let zenith = [0.20, 0.40, 0.92];
    let ground = [0.34, 0.33, 0.31];
    if t > 0.0 {
        let f = 1.0 - (-t * 3.0).exp();
        mix(horizon, zenith, f)
    } else {
        let f = 1.0 - (t * 2.0).exp();
        mix(horizon, ground, f)
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

pub(crate) fn dir_from_equirect(u: f32, v: f32) -> [f32; 3] {
    let phi = (u - 0.5) * PI * 2.0;
    let theta = v * PI;
    let y = theta.cos();
    let radial = theta.sin();
    [radial * phi.cos(), y, radial * phi.sin()]
}

fn cosine_irradiance(n: [f32; 3]) -> [f32; 3] {
    let mut sum = [0.0; 3];
    for i in 0..SAMPLES {
        let xi = hammersley(i, SAMPLES);
        let local = cosine_hemisphere(xi);
        let l = to_world(local, n);
        let sky = sky_color(l);
        sum[0] += sky[0];
        sum[1] += sky[1];
        sum[2] += sky[2];
    }
    let inv = 1.0 / SAMPLES as f32;
    [sum[0] * inv, sum[1] * inv, sum[2] * inv]
}

fn prefilter_specular(r: [f32; 3], roughness: f32) -> [f32; 3] {
    let mut sum = [0.0; 3];
    let mut weight = 0.0;
    for i in 0..SAMPLES {
        let xi = hammersley(i, SAMPLES);
        let h = importance_sample_ggx(xi, roughness, r);
        let l = reflect(r, h);
        let n_dot_l = dot(r, l);
        if n_dot_l > 0.0 {
            let sky = sky_color(l);
            sum[0] += sky[0] * n_dot_l;
            sum[1] += sky[1] * n_dot_l;
            sum[2] += sky[2] * n_dot_l;
            weight += n_dot_l;
        }
    }
    if weight <= 1e-5 {
        return sky_color(r);
    }
    [sum[0] / weight, sum[1] / weight, sum[2] / weight]
}

/// Directional albedo `E(μ)` of the punctual single-scatter GGX lobe with F = 1 and the
/// height-correlated Smith masking-shadowing term (`pbr_lighting.wgsl::v_smith_ggx_correlated`),
/// at n·v = `n_dot_v` and perceptual roughness `perceptual` (α = max(perceptual², [`crate::hard_surface::MIN_GGX_ALPHA`]), as
/// the shader clamps it). Estimated by sampling the visible normals (Heitz 2018) with weight
/// G2 / G1(v), 256 Hammersley samples.
#[must_use]
pub fn directional_albedo_ggx(n_dot_v: f32, perceptual: f32) -> f32 {
    const N: u32 = 256;
    let a = (perceptual * perceptual).max(crate::hard_surface::MIN_GGX_ALPHA);
    let mu = n_dot_v.clamp(1e-3, 1.0);
    let v = [(1.0 - mu * mu).max(0.0).sqrt(), 0.0, mu];
    let lambda = |c: f32| {
        let c = c.max(1e-6);
        let t2 = (1.0 - c * c) / (c * c);
        0.5 * ((1.0 + a * a * t2).sqrt() - 1.0)
    };
    let lv = lambda(mu);
    // Stretched view (Heitz 2018, "Sampling the GGX Distribution of Visible Normals", JCGT 7(4)).
    let vh = normalize([a * v[0], a * v[1], v[2]]);
    let len_sq = vh[0] * vh[0] + vh[1] * vh[1];
    let t1 = if len_sq > 0.0 {
        let inv = 1.0 / len_sq.sqrt();
        [-vh[1] * inv, vh[0] * inv, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let t2 = [
        vh[1] * t1[2] - vh[2] * t1[1],
        vh[2] * t1[0] - vh[0] * t1[2],
        vh[0] * t1[1] - vh[1] * t1[0],
    ];
    let mut sum = 0.0;
    for i in 0..N {
        let [u1, u2] = hammersley(i, N);
        let r = u1.sqrt();
        let phi = 2.0 * PI * u2;
        let p1 = r * phi.cos();
        let s = 0.5 * (1.0 + vh[2]);
        let p2 = (1.0 - s) * (1.0 - p1 * p1).max(0.0).sqrt() + s * r * phi.sin();
        let p3 = (1.0 - p1 * p1 - p2 * p2).max(0.0).sqrt();
        let nh = [
            p1 * t1[0] + p2 * t2[0] + p3 * vh[0],
            p1 * t1[1] + p2 * t2[1] + p3 * vh[1],
            p1 * t1[2] + p2 * t2[2] + p3 * vh[2],
        ];
        let h = normalize([a * nh[0], a * nh[1], nh[2].max(1e-6)]);
        let l = reflect(v, h);
        if l[2] > 0.0 {
            sum += (1.0 + lv) / (1.0 + lv + lambda(l[2]));
        }
    }
    sum / N as f32
}

/// Returns (scale, bias) for `F0 * scale + bias`.
fn integrate_brdf(n_dot_v: f32, roughness: f32) -> (f32, f32) {
    let v = [(1.0 - n_dot_v * n_dot_v).sqrt(), 0.0, n_dot_v];
    let n = [0.0, 0.0, 1.0];
    let mut scale = 0.0;
    let mut bias = 0.0;
    for i in 0..SAMPLES {
        let xi = hammersley(i, SAMPLES);
        let h = importance_sample_ggx(xi, roughness, n);
        let l = reflect(v, h);
        let n_dot_l = l[2].max(0.0);
        let n_dot_h = h[2].max(0.0);
        let v_dot_h = dot(v, h).max(0.0);
        if n_dot_l > 0.0 && n_dot_h > 0.0 {
            let g = geometry_smith(n_dot_v, n_dot_l, roughness);
            let g_vis = (g * v_dot_h) / (n_dot_h * n_dot_v);
            let fc = (1.0 - v_dot_h).powi(5);
            scale += (1.0 - fc) * g_vis;
            bias += fc * g_vis;
        }
    }
    let inv = 1.0 / SAMPLES as f32;
    (scale * inv, bias * inv)
}

pub(crate) fn importance_sample_ggx(xi: [f32; 2], roughness: f32, n: [f32; 3]) -> [f32; 3] {
    let a = roughness * roughness;
    let phi = 2.0 * PI * xi[0];
    let cos_theta = ((1.0 - xi[1]) / (1.0 + (a * a - 1.0) * xi[1])).sqrt();
    let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
    let local = [sin_theta * phi.cos(), sin_theta * phi.sin(), cos_theta];
    to_world(local, n)
}

pub(crate) fn cosine_hemisphere(xi: [f32; 2]) -> [f32; 3] {
    let r = xi[0].sqrt();
    let phi = 2.0 * PI * xi[1];
    [r * phi.cos(), r * phi.sin(), (1.0 - xi[0]).max(0.0).sqrt()]
}

fn geometry_smith(n_dot_v: f32, n_dot_l: f32, roughness: f32) -> f32 {
    geometry_schlick(n_dot_v, roughness) * geometry_schlick(n_dot_l, roughness)
}

fn geometry_schlick(n_dot: f32, roughness: f32) -> f32 {
    let a = roughness * roughness;
    let k = a * 0.5;
    n_dot / (n_dot * (1.0 - k) + k).max(1e-5)
}

pub(crate) fn reflect(v: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let c = dot(v, n);
    [
        2.0 * c * n[0] - v[0],
        2.0 * c * n[1] - v[1],
        2.0 * c * n[2] - v[2],
    ]
}

pub(crate) fn to_world(local: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let up = if n[2].abs() < 0.999 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let t = normalize(cross(up, n));
    let b = cross(n, t);
    normalize([
        t[0] * local[0] + b[0] * local[1] + n[0] * local[2],
        t[1] * local[0] + b[1] * local[1] + n[1] * local[2],
        t[2] * local[0] + b[2] * local[1] + n[2] * local[2],
    ])
}

pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt().max(1e-8);
    [v[0] / len, v[1] / len, v[2] / len]
}

pub(crate) fn hammersley(i: u32, n: u32) -> [f32; 2] {
    [i as f32 / n as f32, radical_inverse_vdc(i)]
}

fn radical_inverse_vdc(mut bits: u32) -> f32 {
    bits = bits.rotate_right(16);
    bits = ((bits & 0x5555_5555) << 1) | ((bits & 0xAAAA_AAAA) >> 1);
    bits = ((bits & 0x3333_3333) << 2) | ((bits & 0xCCCC_CCCC) >> 2);
    bits = ((bits & 0x0F0F_0F0F) << 4) | ((bits & 0xF0F0_F0F0) >> 4);
    bits = ((bits & 0x00FF_00FF) << 8) | ((bits & 0xFF00_FF00) >> 8);
    bits as f32 * 2.328_306_4e-10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uniform_world_convolves_to_itself() {
        let maps = build_uniform_ibl([0.5, 0.25, 1.0]);
        for image in std::iter::once(&maps.irradiance).chain(maps.prefilter.iter()) {
            for px in image.rgba.as_chunks::<4>().0 {
                assert_eq!(px, &[128, 64, 255, 255]);
            }
        }
        assert_eq!(maps.prefilter.len(), 4);
    }

    #[test]
    fn a_studio_world_lights_by_hemisphere_fraction() {
        let maps = build_studio_ibl([0.0, 0.0, 0.0], [0.4, 0.4, 0.4]);
        let px = |img: &IblImage, x: u32, y: u32| img.rgba[((y * img.width + x) * 4) as usize];
        let irr = &maps.irradiance;
        // Top row faces up (sky only), bottom row faces down (ground only), the middle row is a
        // vertical wall that sees half of each.
        assert!(px(irr, 10, 0) <= 1);
        assert!((i32::from(px(irr, 10, irr.height - 1)) - 102).abs() <= 1);
        assert!((50..=56).contains(&px(irr, 10, irr.height / 2)));
        // The mirror mip keeps the step; the roughest mip blurs it across the horizon.
        let sharp = &maps.prefilter[0];
        assert_eq!(px(sharp, 3, sharp.height / 2 - 2), 0);
        assert_eq!(px(sharp, 3, sharp.height / 2 + 1), 102);
    }

    #[test]
    fn zenith_irradiance_is_bluer_than_ground() {
        let up = cosine_irradiance([0.0, 1.0, 0.0]);
        let down = cosine_irradiance([0.0, -1.0, 0.0]);
        assert!(up[2] > down[2], "up {up:?} down {down:?}");
        assert!(up[2] > up[0]);
    }

    #[test]
    fn brdf_scale_drops_as_roughness_rises() {
        let (smooth, _) = integrate_brdf(1.0, 0.1);
        let (rough, rough_bias) = integrate_brdf(1.0, 0.9);
        assert!(smooth > rough, "smooth {smooth} rough {rough}");
        assert!(rough_bias > 0.0);
        assert!(smooth <= 1.05);
    }
}

/// IEEE 754 binary16 bits of `v` (round to nearest even; overflow to infinity, NaN kept).
#[must_use]
pub(crate) fn f16_bits(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        // Subnormal half: shift the implicit-1 mantissa into place, round to nearest even.
        let m = mant | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        let round = u32::from(rem > mid || (rem == mid && (half & 1) == 1));
        return sign | (half + round) as u16;
    }
    let half = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    let round = u32::from(rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1));
    sign | (half + round) as u16
}

/// Little-endian RGBA16F bytes (8 per texel) of linear RGB values; alpha 1.
#[must_use]
pub(crate) fn rgb_to_rgba16f(texels: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(texels.len() * 8);
    for t in texels {
        for c in [t[0], t[1], t[2], 1.0] {
            out.extend_from_slice(&f16_bits(c.max(0.0)).to_le_bytes());
        }
    }
    out
}

/// [`rgb_to_rgba16f`] of an RGBA8 image (values 0..1).
#[must_use]
pub(crate) fn rgba8_to_rgba16f(rgba: &[u8]) -> Vec<u8> {
    let texels: Vec<[f32; 3]> = rgba
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
        .collect();
    rgb_to_rgba16f(&texels)
}

#[cfg(test)]
mod half_tests {
    use super::f16_bits;

    #[test]
    fn f16_matches_known_encodings() {
        assert_eq!(f16_bits(0.0), 0x0000);
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(-2.0), 0xc000);
        assert_eq!(f16_bits(0.5), 0x3800);
        assert_eq!(f16_bits(65504.0), 0x7bff);
        assert_eq!(f16_bits(1.0e6), 0x7c00);
        // Smallest normal and a subnormal.
        assert_eq!(f16_bits(6.103_515_6e-5), 0x0400);
        assert_eq!(f16_bits(5.960_464_5e-8), 0x0001);
        // 1/3 rounds to nearest.
        assert_eq!(f16_bits(1.0 / 3.0), 0x3555);
    }
}
