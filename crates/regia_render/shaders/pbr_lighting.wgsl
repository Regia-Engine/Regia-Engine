// SPDX-License-Identifier: Apache-2.0
// Portions ported from Filament (https://github.com/google/filament, commit ac4a2a8554),
// Copyright (C) 2017 The Android Open Source Project, Apache License 2.0: `specular_aa_roughness`
// (from `shaders/src/surface_shading_lit.fs::normalFiltering`).
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file
// except in compliance with the License. You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0. Unless required by applicable law or agreed to in
// writing, software distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

#define_import_path regia::pbr::lighting

// GGX / Trowbridge-Reitz (Walter 2007), height-correlated Smith (Heitz 2014),
// Schlick Fresnel (Schlick 1994). Formulas re-derived; not copied from a shader.

const PI: f32 = 3.14159265359;

// Smallest GGX α the shading uses: perceptual roughness 0.045, squared (Filament's
// MIN_PERCEPTUAL_ROUGHNESS / MIN_ROUGHNESS, Guy & Agopian 2018 § 4.8.3.3). It keeps α² in f32
// range. Flooring α itself at 0.045 (as before) widened every lobe smoother than perceptual
// 0.21 into a broad, bright sun highlight (polished glass read 5-17x Cycles).
const MIN_ALPHA: f32 = 0.002025;

fn saturate_local(x: f32) -> f32 {
    return clamp(x, 0.0, 1.0);
}

fn d_ggx(no_h: f32, a: f32) -> f32 {
    let a2 = a * a;
    let f = (no_h * a2 - no_h) * no_h + 1.0;
    return a2 / (PI * f * f);
}

// GGX averaged over a small disc light (the sun, ADR-3190), as the ratio to the point light's
// `d_ggx` at the disc's centre, so a caller scales the specular it already has. GGX is a Cauchy
// distribution of microfacet slopes (Heitz 2014 § 5.2): D = α² / (π cos⁴θ (α² + tan²θ)²). A light
// cone of angular radius `disc` maps to a disc of half-vectors of solid angle π disc² / (4 l·h)
// (Walter et al. 2007 eq. 14), and to slope space by 1 / cos³θ, so to a slope disc of radius
// ρ = disc / (2 √(l·h cos³θ)). The mean of the slope density over a disc of radius ρ whose centre
// sits at slope distance t is the point-to-disc view factor in closed form (Howell's catalogue of
// configuration factors, B-12), rewritten without the cancellation at ρ → 0:
// mean = 2α² / (π S (S + A − ρ²)), A = α² + t², S = √((A − ρ²)² + 4ρ²α²). At ρ = 0 it is D.
// `disc` ≤ 0 returns exactly 1 (the point sun).
fn disc_lobe_scale(no_h: f32, lo_h: f32, a: f32, disc: f32) -> f32 {
    if (!(disc > 0.0)) {
        return 1.0;
    }
    let c = clamp(no_h, 1e-4, 1.0);
    let c2 = c * c;
    let t2 = max(1.0 - c2, 0.0) / c2;
    let a2 = a * a;
    let rho = disc / (2.0 * sqrt(max(lo_h, 1e-4) * c2 * c));
    let r2 = rho * rho;
    let big_a = a2 + t2;
    let d = big_a - r2;
    let s = sqrt(d * d + 4.0 * r2 * a2);
    // S + d without cancellation when the disc is wider than the lobe (d < 0): (S + d)(S − d) = 4ρ²α².
    let sd = select(4.0 * r2 * a2 / max(s - d, 1e-30), s + d, d >= 0.0);
    // Both as slope densities (the 1 / (π cos⁴θ) factor cancels): disc mean over the point value.
    let point = a2 / (big_a * big_a);
    let mean = 2.0 * a2 / max(s * sd, 1e-30);
    return mean / max(point, 1e-30);
}

fn v_smith_ggx_correlated(no_v: f32, no_l: f32, a: f32) -> f32 {
    let a2 = a * a;
    let ggxv = no_l * sqrt(no_v * no_v * (1.0 - a2) + a2);
    let ggxl = no_v * sqrt(no_l * no_l * (1.0 - a2) + a2);
    return 0.5 / (ggxv + ggxl);
}

// Schlick's base is clamped: a light at the eye makes `l == v`, so `h == v` and V·H rounds to
// 1 + 1 ulp; `pow` of the negative base is undefined in WGSL (NaN on lavapipe), and the NaN
// printed black pixels across a headlight's cone (tunnel_scene, lane v020r).
fn f_schlick(vo_h: f32, f0: vec3<f32>) -> vec3<f32> {
    let f = pow(saturate_local(1.0 - vo_h), 5.0);
    return f0 + (vec3<f32>(1.0) - f0) * f;
}

// Windowed inverse-square falloff (Lagarde, Frostbite 2014 course notes).
fn distance_attenuation(distance: f32, radius: f32) -> f32 {
    let r = max(radius, 1e-4);
    let d = max(distance, 0.0);
    let num = saturate_local(1.0 - pow(d / r, 4.0));
    return (num * num) / (d * d + 1.0);
}

// The same windowed inverse square for a light with a sphere source (`GpuLight::cone_shadow.w`,
// metres; ADR-1454): the irradiance of a sphere of radius s at distance d is bounded by
// I / s^2, so the 1 m knee of `distance_attenuation` becomes max(1, s^2). A point light and
// every source under 1 m keep `distance_attenuation` exactly.
fn distance_attenuation_source(distance: f32, radius: f32, source_radius: f32) -> f32 {
    let r = max(radius, 1e-4);
    let d = max(distance, 0.0);
    let num = saturate_local(1.0 - pow(d / r, 4.0));
    return (num * num) / (d * d + max(1.0, source_radius * source_radius));
}

fn spot_attenuation(l: vec3<f32>, spot_dir: vec3<f32>, inner_cos: f32, outer_cos: f32) -> f32 {
    let cos_angle = dot(normalize(-spot_dir), normalize(l));
    let denom = max(inner_cos - outer_cos, 1e-4);
    return saturate_local((cos_angle - outer_cos) / denom);
}

// Clamped base: see `f_schlick`.
fn f_schlick_f32(vo_h: f32, f0: f32) -> f32 {
    let f = pow(saturate_local(1.0 - vo_h), 5.0);
    return f0 + (1.0 - f0) * f;
}

// Dielectric coat in front of the base: 1 - factor * F(NoV, 0.04) (KHR_materials_clearcoat).
fn clearcoat_attenuation(no_v: f32, factor: f32) -> f32 {
    return 1.0 - factor * f_schlick_f32(no_v, 0.04);
}

// Lagarde & de Rousiers, Frostbite 2014 course notes v3 §4.10.2 (roughness form).
fn specular_occlusion(no_v: f32, ao: f32, perceptual_roughness: f32) -> f32 {
    let exponent = exp2(-16.0 * perceptual_roughness - 1.0);
    return saturate_local(pow(no_v + ao, exponent) - 1.0 + ao);
}

// Signed KHR_materials_anisotropy: αt = α(1+aniso), αb = α(1-aniso).
fn anisotropic_alphas(alpha: f32, anisotropy: f32) -> vec2<f32> {
    let at = max(alpha * (1.0 + anisotropy), 1e-4);
    let ab = max(alpha * (1.0 - anisotropy), 1e-4);
    return vec2<f32>(at, ab);
}

// Anisotropic GGX. at == ab == alpha matches d_ggx (the shader still calls d_ggx when aniso is 0).
fn d_ggx_anisotropic(dot_th: f32, dot_bh: f32, dot_nh: f32, at: f32, ab: f32) -> f32 {
    let a2 = at * ab;
    let f = vec3<f32>(ab * dot_th, at * dot_bh, a2 * dot_nh);
    let w2 = a2 / max(dot(f, f), 1e-8);
    return a2 * w2 * w2 / PI;
}

fn v_smith_ggx_anisotropic(dot_tv: f32, dot_bv: f32, dot_tl: f32, dot_bl: f32, no_v: f32, no_l: f32, at: f32, ab: f32) -> f32 {
    let ggxv = no_l * length(vec3<f32>(at * dot_tv, ab * dot_bv, no_v));
    let ggxl = no_v * length(vec3<f32>(at * dot_tl, ab * dot_bl, no_l));
    return 0.5 / max(ggxv + ggxl, 1e-7);
}

// Fdez-Agüera 2019: Favg = F0 + (1-F0)/21.
fn fresnel_average(f0: vec3<f32>) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * (1.0 / 21.0);
}

// FmsEms from a Karis split-sum sample (scale, bias). Ess is saturated so a LUT above 1 cannot add energy.
fn multi_scatter_ems(f0: vec3<f32>, dfg: vec2<f32>) -> vec3<f32> {
    let ess = saturate_local(dfg.x + dfg.y);
    let fss = f0 * dfg.x + vec3<f32>(dfg.y);
    let favg = fresnel_average(f0);
    let ems = 1.0 - ess;
    let denom = max(vec3<f32>(1.0) - favg * ems, vec3<f32>(1e-4));
    return fss * favg * ems / denom;
}

// Punctual multi-scatter energy compensation (Turquin 2019, "Practical multiple scattering
// compensation for microfacet models"): the single-scatter GGX lobe scaled by
// 1 + Fms (1 − E(μo)) / E(μo), where E is that lobe's directional albedo with F = 1 (the DFG
// table's B channel, height-correlated Smith). It keeps the lobe's shape, so the energy single
// scattering loses comes back where the lobe already points (forward at grazing angles).
// Fms = Favg E_avg / (1 − Favg (1 − E_avg)) is the share of that energy a conductor returns when
// every further bounce is attenuated by the average Fresnel (Kulla & Conty 2017's series, with
// the single-scatter Fresnel factored out); E_avg is the table's A channel. A white conductor
// (Favg = 1) reflects all of it, E · (1 + (1 − E) / E) = 1; a coloured one less than F0 would
// give (rough metal r0.9 with F0 = 0.8: Fms 0.68).
fn energy_compensation(f0: vec3<f32>, e_o: f32, e_avg: f32) -> vec3<f32> {
    let e = clamp(e_o, 0.05, 1.0);
    let ea = clamp(e_avg, 0.05, 1.0);
    let favg = fresnel_average(f0);
    let fms = favg * ea / (vec3<f32>(1.0) - favg * (1.0 - ea));
    return vec3<f32>(1.0) + fms * ((1.0 - e) / e);
}


// Geometric specular anti-aliasing (Kaplanyan & Hoffman 2016, "Stable specular highlights";
// Tokuyoshi & Kaplanyan 2019, "Improved geometric specular antialiasing"), the Filament
// `normalFiltering` form. The screen-space derivatives of the shading normal estimate the
// variance of the normals one pixel covers; twice that variance (clamped to `threshold`) widens
// the GGX lobe: α'² = α² + min(2σ², threshold). Returns perceptual roughness (α = r²).
//
// `dn_dx`/`dn_dy` are `dpdx(n)`/`dpdy(n)` of the world shading normal, taken by the caller in
// uniform control flow. `variance` 0 returns `perceptual` unchanged.
fn specular_aa_roughness(perceptual: f32, dn_dx: vec3<f32>, dn_dy: vec3<f32>, variance: f32, threshold: f32) -> f32 {
    let sigma2 = variance * (dot(dn_dx, dn_dx) + dot(dn_dy, dn_dy));
    let kernel = min(2.0 * sigma2, threshold);
    if (kernel <= 0.0) {
        return perceptual;
    }
    let a = perceptual * perceptual;
    return max(perceptual, sqrt(sqrt(saturate_local(a * a + kernel))));
}

// Normal-map variance (Toksvig 2005, "Mipmapping normal maps"): a filtered normal-map sample
// averages unit normals (the mips store the averages unnormalised; trilinear and anisotropic
// filtering average further), and its length |N̄| < 1 measures their spread. Toksvig's
// effective exponent 1/s' = 1/s + (1 − |N̄|)/|N̄| with α² ≈ 2/s gives α'² = α² + 2σ²,
// σ² = (1 − |N̄|)/|N̄|. `floor` removes the 8-bit quantisation noise of a single texel's length,
// so a magnified map (one normal per sample) does not roughen. Returns perceptual roughness.
fn toksvig_roughness(perceptual: f32, normal_length: f32, floor: f32) -> f32 {
    let l = clamp(normal_length, 1e-4, 1.0);
    let sigma2 = (1.0 - l) / l - floor;
    if (sigma2 <= 0.0) {
        return perceptual;
    }
    let a = perceptual * perceptual;
    return max(perceptual, sqrt(sqrt(saturate_local(a * a + 2.0 * sigma2))));
}

// Sphere lights (`LightSourceSize`, ADR-1367). Lagarde & de Rousiers, "Moving Frostbite to
// Physically Based Rendering" v3, SIGGRAPH 2014, § 4.7.5, light sphere sources with Karis's
// representative point and energy term: Karis, "Real Shading in the legacy engine4", SIGGRAPH
// 2013 course notes (public), eqs. 10, 11 and 14. The widening term is eq. 10's half-angle
// r / (2d), added to α as a variance (α'² = α² + (r / 2d)², as the specular-AA and Toksvig
// widenings above add theirs) instead of linearly: the linear sum darkened rough metals by up to
// 26 % against the path tracer's sphere light (ADR-1367). The representative point moves from
// the centre toward the reflection ray by 1 − α/α' of the way, the shift for which a Gaussian
// lobe of width α at the moved point, times (α/α')², equals the widened lobe at the centre; it
// is eq. 11's point for a glossy lobe and the centre for a rough one. Mirrored on the CPU by
// `regia_render::light_source`.

// α'² of a lobe of α lit by a sphere of `radius` at `distance`, saturated at 1.
fn sphere_light_alpha2(a: f32, radius: f32, distance: f32) -> f32 {
    let w = radius / (2.0 * max(distance, 1e-4));
    return min(a * a + w * w, 1.0);
}

// The light direction for the specular: from the shading point toward the sphere (centre
// `to_center`), moved toward the reflection ray `r` by 1 − α/α' of the centre's offset from the
// ray and never past the sphere (eq. 11's clamp).
fn sphere_light_dir(to_center: vec3<f32>, r: vec3<f32>, radius: f32, a: f32) -> vec3<f32> {
    let center_to_ray = dot(to_center, r) * r - to_center;
    let len = max(length(center_to_ray), 1e-6);
    let share = 1.0 - a / sqrt(max(sphere_light_alpha2(a, radius, length(to_center)), 1e-12));
    let closest = to_center + center_to_ray * min(saturate_local(share), saturate_local(radius / len));
    return normalize(closest);
}

// Eq. 14 with the widened α: (α / α')², the energy term of the moved lobe. 1 for radius 0.
fn sphere_light_normalization(a: f32, radius: f32, distance: f32) -> f32 {
    return min(a * a / max(sphere_light_alpha2(a, radius, distance), 1e-12), 1.0);
}

// The factor on the specular lit from the representative point: the energy term times that
// point's own cosine ratio `spec_cos`. Where the reflection ray `r` misses the sphere, the sphere's
// integral is never below the punctual value at its centre (the lobe falls off away from the ray),
// and GGX's long tail is not narrowed by a small source, so there the factor is at least
// D(centre) / D(point): the tail keeps the point light's value instead of the narrowed lobe's.
fn sphere_light_gain(a: f32, radius: f32, to_center: vec3<f32>, r: vec3<f32>, no_h: f32, no_h_center: f32, spec_cos: f32) -> f32 {
    let gain = sphere_light_normalization(a, radius, length(to_center)) * spec_cos;
    let along = dot(to_center, r);
    if (along > 0.0 && length(along * r - to_center) <= radius) {
        return gain;
    }
    return max(gain, d_ggx(no_h_center, a) / max(d_ggx(no_h, a), 1e-30));
}

// IES photometric profiles (`LightProfile`, ADR-2592; CPU mirror `regia_render::light_profiles`).
// A light with a profile stores `kind + code / 4096` in `position_kind.w` (the integer kind every
// reader takes with `u32(w + 0.5)` is unchanged): `code & 7` is its atlas slot (1..7, 0 = none) and
// `code >> 3` turns the profile's horizontal 0° by that many 2π/256 steps about the nadir. Slot s is
// the 256 × 32 band of rows `height − 32 s ..` of the emitter probe atlas: x = vertical angle θ
// (0..180° in 255 steps), y = horizontal angle φ (32 steps, wrapping), value = intensity / peak.
fn light_profile_code(kind_w: f32) -> u32 {
    return u32(round(fract(kind_w) * 4096.0));
}

// The profile's horizontal 0° axis before the code's turn: perpendicular to the nadir, from world +Y
// (world +X when the nadir is vertical).
fn light_profile_axis(nadir: vec3<f32>) -> vec3<f32> {
    let up = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(nadir.y) > 0.999);
    return normalize(cross(up, nadir));
}

// Relative intensity (peak 1) toward `d` (unit, light → receiver) of the profile `code` names, for a
// light whose nadir is `nadir`: bilinear on the band, as `CookedIesProfile::relative_at_angles`.
fn light_profile_relative(atlas: texture_2d<f32>, code: u32, nadir: vec3<f32>, d: vec3<f32>) -> f32 {
    let slot = code & 7u;
    if (slot == 0u) {
        return 1.0;
    }
    let n = normalize(nadir);
    let a0 = light_profile_axis(n);
    let a1 = cross(a0, n);
    let turn = f32(code >> 3u) * (2.0 * PI / 256.0);
    let t0 = a0 * cos(turn) + a1 * sin(turn);
    let t1 = cross(t0, n);
    let theta = acos(clamp(dot(d, n), -1.0, 1.0));
    var phi = atan2(dot(d, t1), dot(d, t0));
    if (phi < 0.0) {
        phi = phi + 2.0 * PI;
    }
    let x = theta / PI * 255.0;
    let y = phi / (2.0 * PI) * 32.0;
    let base = i32(textureDimensions(atlas).y) - 32 * i32(slot);
    let xf = floor(x);
    let yf = floor(y);
    let fx = x - xf;
    let fy = y - yf;
    let x0 = clamp(i32(xf), 0, 255);
    let x1 = min(x0 + 1, 255);
    let y0 = i32(yf) % 32;
    let y1 = (y0 + 1) % 32;
    let v00 = textureLoad(atlas, vec2<i32>(x0, base + y0), 0).r;
    let v10 = textureLoad(atlas, vec2<i32>(x1, base + y0), 0).r;
    let v01 = textureLoad(atlas, vec2<i32>(x0, base + y1), 0).r;
    let v11 = textureLoad(atlas, vec2<i32>(x1, base + y1), 0).r;
    return mix(mix(v00, v10, fx), mix(v01, v11, fx), fy);
}
