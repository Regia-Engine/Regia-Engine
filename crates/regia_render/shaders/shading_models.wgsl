// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::pbr::shading_models

#import regia::pbr::lighting

// Character shading models (Tincal B3 skin, B4 eye, B6 fabric sheen; `crate::shading_model`).
// Pure functions: `lab_forward.wgsl` calls them only for a material whose shading block selects
// them, so a standard material's code path is unchanged. Each one has a Rust mirror in
// `shading_model.rs` that the unit tests check against a numerical reference.

// ---- Skin: pre-integrated diffuse scattering (Penner & Borshukov 2011) ----
//
// Penner's pre-integrated falloff is the clamped cosine convolved around a ring of radius r
// with the skin diffusion profile R(d) (d'Eon & Luebke 2007: six Gaussians, variances in mm²):
//   D(θ, r) = ∫ cos⁺(θ + x) R(2r sin(x/2)) dx / ∫ R(2r sin(x/2)) dx.
// On the ring, one Gaussian of variance v is exp(a cos x) up to a constant, a = r²/v: a von
// Mises kernel, whose Fourier coefficients are I_k(a) / I_0(a) (Abramowitz & Stegun 9.6.19).
// The clamped cosine's Fourier series is 1/π + cos θ / 2 + Σ_even c_k cos kθ, so the falloff
// is the clamped cosine plus Σ c_k (κ_k − 1) cos kθ, κ_k the profile mixture's coefficients.
// Here κ_k uses the wrapped-normal form ρ^(k²), ρ ≈ I_1/I_0, and the mixture weights
// e^(−a) I_0(a) ≈ 1 / √(1 + 2πa). Harmonics up to 12; the error against the integral is below
// 0.025 at a 1 mm radius and below 0.01 from 5 mm (`shading_model` tests).

const SKIN_VARIANCES_A: vec3<f32> = vec3<f32>(0.0064, 0.0484, 0.187);
const SKIN_VARIANCES_B: vec3<f32> = vec3<f32>(0.567, 1.99, 7.41);

fn skin_weight(i: u32) -> vec3<f32> {
    switch i {
        case 0u: { return vec3<f32>(0.233, 0.455, 0.649); }
        case 1u: { return vec3<f32>(0.100, 0.336, 0.344); }
        case 2u: { return vec3<f32>(0.118, 0.198, 0.0); }
        case 3u: { return vec3<f32>(0.113, 0.007, 0.007); }
        case 4u: { return vec3<f32>(0.358, 0.004, 0.0); }
        default: { return vec3<f32>(0.078, 0.0, 0.0); }
    }
}

fn skin_variance(i: u32) -> f32 {
    if (i < 3u) {
        return SKIN_VARIANCES_A[i];
    }
    return SKIN_VARIANCES_B[i - 3u];
}

// The profile's ring coefficients κ_k − 1 for k = 1, 2, 4, 6, 8, 10, 12 at a curvature radius
// in millimetres (already divided by the scatter scale), one vec3 (RGB) per harmonic.
// (naga_oil rejects module identifiers that end in a digit: the harmonics are spelled out.)
struct SkinKernel {
    first: vec3<f32>,
    second: vec3<f32>,
    fourth: vec3<f32>,
    sixth: vec3<f32>,
    eighth: vec3<f32>,
    tenth: vec3<f32>,
    twelfth: vec3<f32>,
}

fn skin_kernel(radius_mm: f32) -> SkinKernel {
    let r2 = radius_mm * radius_mm;
    var sum = vec3<f32>(0.0);
    var s1 = vec3<f32>(0.0);
    var s2 = vec3<f32>(0.0);
    var s4 = vec3<f32>(0.0);
    var s6 = vec3<f32>(0.0);
    var s8 = vec3<f32>(0.0);
    var s10 = vec3<f32>(0.0);
    var s12 = vec3<f32>(0.0);
    for (var i = 0u; i < 6u; i = i + 1u) {
        let v = skin_variance(i);
        let a = r2 / v;
        // ρ = I_1(a) / I_0(a): a / 2 near 0, 1 − 1 / (2a) for large a.
        let rho = a / (0.5 + sqrt(a * a + 2.25));
        let w = skin_weight(i) / (v * sqrt(1.0 + 6.2831853 * a));
        let p2 = rho * rho;
        let p4 = p2 * p2;
        let p16 = p4 * p4 * p4 * p4;
        let p32 = p16 * p16;
        let p36 = p32 * p4;
        let p64 = p32 * p32;
        let p100 = p64 * p36;
        let p144 = p100 * p36 * p4 * p4;
        sum += w;
        s1 += w * rho;
        s2 += w * p4;
        s4 += w * p16;
        s6 += w * p36;
        s8 += w * p64;
        s10 += w * p100;
        s12 += w * p144;
    }
    let inv = vec3<f32>(1.0) / max(sum, vec3<f32>(1e-12));
    let one = vec3<f32>(1.0);
    return SkinKernel(s1 * inv - one, s2 * inv - one, s4 * inv - one, s6 * inv - one, s8 * inv - one, s10 * inv - one, s12 * inv - one);
}

// Penner's D per channel, `c` the unclamped cosine to the light per channel (the per-channel
// diffuse normals). Replaces `max(n·l, 0)` on the diffuse lobe; never below 0.
fn skin_falloff(c: vec3<f32>, k: SkinKernel) -> vec3<f32> {
    // cos kθ from cos θ (Chebyshev: T_2 = 2c² − 1, T_2n = 2 T_n² − 1, T_m+n = 2 T_m T_n − T_|m−n|).
    let t2 = 2.0 * c * c - vec3<f32>(1.0);
    let t4 = 2.0 * t2 * t2 - vec3<f32>(1.0);
    let t6 = 2.0 * t2 * t4 - t2;
    let t8 = 2.0 * t4 * t4 - vec3<f32>(1.0);
    let t10 = 2.0 * t4 * t6 - t2;
    let t12 = 2.0 * t6 * t6 - vec3<f32>(1.0);
    var d = max(c, vec3<f32>(0.0));
    d += 0.5 * k.first * c;
    // c_k = (2/π)(−1)^(k/2+1) / (k² − 1).
    d += 0.21220659 * k.second * t2;
    d -= 0.04244132 * k.fourth * t4;
    d += 0.01818914 * k.sixth * t6;
    d -= 0.01010507 * k.eighth * t8;
    d += 0.00643050 * k.tenth * t10;
    d -= 0.00445188 * k.twelfth * t12;
    return max(d, vec3<f32>(0.0));
}

// Transmittance of the skin profile through `thickness_mm` (Jimenez, Whelan, Sundstedt &
// Gutierrez 2010, "Real-Time Realistic Skin Translucency"): Σ w_i exp(−s² / v_i).
fn skin_transmittance(thickness_mm: f32) -> vec3<f32> {
    let s2 = thickness_mm * thickness_mm;
    var t = vec3<f32>(0.0);
    for (var i = 0u; i < 6u; i = i + 1u) {
        t += skin_weight(i) * exp(-s2 / skin_variance(i));
    }
    return t;
}

// Back-lit share of a light for the transmitted term: Jimenez et al.'s wrap, saturate(0.3 − n·l).
fn skin_back_light(no_l_signed: f32) -> f32 {
    return clamp(0.3 - no_l_signed, 0.0, 1.0);
}

// ---- Fabric sheen: Charlie distribution (Estevez & Kulla 2017) ----

// The sheen roughness floor (KHR_materials_sheen implementations clamp it; 0.07 keeps the
// exponent 1/α² below 205).
const SHEEN_MIN_ROUGHNESS: f32 = 0.07;

// D(h) = (2 + 1/α) sin(θh)^(1/α) / 2π, α = roughness² (KHR_materials_sheen).
fn d_charlie(no_h: f32, alpha: f32) -> f32 {
    let inv = 1.0 / alpha;
    let sin2 = max(1.0 - no_h * no_h, 1e-7);
    return (2.0 + inv) * pow(sin2, 0.5 * inv) * (1.0 / (2.0 * regia::pbr::lighting::PI));
}

// Ashikhmin's visibility as Neubelt & Pettineo 2013 write it: 1 / (4 (n·l + n·v − n·l n·v)).
fn v_sheen(no_v: f32, no_l: f32) -> f32 {
    return 1.0 / max(4.0 * (no_l + no_v - no_l * no_v), 1e-4);
}

// Directional albedo of the sheen lobe above (white sheen colour) at cos θ = `mu`, sheen
// roughness `r` in 0.07..1. A rational fit of the numerical integral (`shading_model`
// tests; worst 0.053 on the fitting grid, mean 0.006), clamped to 0..1.
fn sheen_albedo(mu: f32, r: f32) -> f32 {
    let m = max(mu, 0.02);
    let a = 0.042 + r * (1.0505 + r * (0.3251 - 0.984 * r));
    let b = abs(-0.044 + r * (0.3808 + r * (2.0542 - 1.7916 * r)));
    let c = -0.4447 + r * (0.371 + r * (-0.5375 + 0.4956 * r));
    return clamp(a / (m + b) + c, 0.0, 1.0);
}

// The base layer's share under the sheen (KHR_materials_sheen albedo scaling):
// 1 − max(sheen colour) · E(cos θ).
fn sheen_scaling(color: vec3<f32>, mu: f32, r: f32) -> f32 {
    let peak = max(color.r, max(color.g, color.b));
    return 1.0 - peak * sheen_albedo(mu, r);
}

// ---- Eye: refracted iris (Snell at the cornea) ----

// The UV the iris is seen at through the cornea. `view_ts` is the unit view direction in the
// surface's tangent frame (z along the normal), `ior` the cornea's index, `depth` the anterior
// chamber's depth at the pupil in UV units (the iris plane sits `depth · (1 − (ρ/R)²)` below the
// cornea at UV radius ρ, so the offset fades to 0 at the limbus). The refracted ray enters at
// `uv` and meets the iris plane `h` deeper, offset by `r.xy / −r.z · h` (the parallax-mapping
// convention of `parallax_uv`).
fn eye_iris_uv(uv: vec2<f32>, center: vec2<f32>, radius: f32, depth: f32, ior: f32, view_ts: vec3<f32>) -> vec2<f32> {
    let rho = length(uv - center) / max(radius, 1e-4);
    let h = depth * max(1.0 - rho * rho, 0.0);
    let incident = -normalize(vec3<f32>(view_ts.xy, max(view_ts.z, 1e-3)));
    let r = refract(incident, vec3<f32>(0.0, 0.0, 1.0), 1.0 / max(ior, 1.0));
    return uv + r.xy / max(-r.z, 0.15) * h;
}

// Iris coverage at UV radius `rho` (1 inside the iris, 0 on the sclera) and the limbal ring's
// darkening weight (a Gaussian band of half-width `width` at the iris edge).
fn eye_iris_mask(rho: f32, radius: f32, width: f32) -> vec2<f32> {
    let w = max(width, 1e-4);
    let inside = 1.0 - smoothstep(radius - 0.5 * w, radius + 0.5 * w, rho);
    let x = (rho - radius) / w;
    return vec2<f32>(inside, exp(-x * x * 2.0));
}
