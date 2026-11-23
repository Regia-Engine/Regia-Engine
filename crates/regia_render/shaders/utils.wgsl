// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::utils

// Shared math and the display transform.
// AgX: Troy Sobotka's public AgX display transform, in the minimal sRGB-primaries form
// (inset matrix → log2 encode over [−12.47, +4.03] EV → sixth-order sigmoid fit → outset
// matrix) published by B. Wrensch, "Minimal AgX Implementation" (2023). The fit takes the
// *log-encoded, normalised* value; the output is display-encoded (≈ 2.2 gamma), so callers
// writing to an sRGB-encoding target decode it first. See `docs/provenance.md`.
// Sky is a horizon/zenith gradient.

const PI: f32 = 3.14159265359;

// Default look (PostSettings::default contrast / saturation). The no-post forward
// path and the terrain shaders use these; `look_defaults_match_post_settings` checks them.
const DEFAULT_LOOK_CONTRAST: f32 = 1.2;
const DEFAULT_LOOK_SATURATION: f32 = 1.4;

fn saturate(x: f32) -> f32 {
    return clamp(x, 0.0, 1.0);
}

fn agx_sigmoid(x: vec3<f32>) -> vec3<f32> {
    let x2 = x * x;
    let x4 = x2 * x2;
    return 15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x
        - vec3<f32>(0.00232);
}

/// AgX base transform: linear scene-referred (exposed) RGB → display-encoded RGB in [0, 1].
fn tonemap_agx_vec(color: vec3<f32>) -> vec3<f32> {
    let inset = mat3x3<f32>(
        vec3<f32>(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        vec3<f32>(0.0784335999999992, 0.878468636469772, 0.0784336),
        vec3<f32>(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let outset = mat3x3<f32>(
        vec3<f32>(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        vec3<f32>(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        vec3<f32>(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    let min_ev = -12.47393;
    let max_ev = 4.026069;
    var v = inset * max(color, vec3<f32>(1.0e-10));
    v = clamp(log2(max(v, vec3<f32>(1.0e-10))), vec3<f32>(min_ev), vec3<f32>(max_ev));
    v = (v - vec3<f32>(min_ev)) / (max_ev - min_ev);
    v = agx_sigmoid(v);
    v = outset * v;
    return clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
}

/// Display-space grade after the tonemap: an S-curve around mid grey (`contrast` > 1
/// deepens shadows and lifts highlights while keeping 0, 0.5 and 1 fixed) and a
/// saturation scale around Rec.709 luma. 1 / 1 is the identity.
fn display_look(display: vec3<f32>, contrast: f32, saturation: f32) -> vec3<f32> {
    let v = clamp(display, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 0.5 * pow(v * 2.0, vec3<f32>(contrast));
    let hi = vec3<f32>(1.0) - 0.5 * pow((vec3<f32>(1.0) - v) * 2.0, vec3<f32>(contrast));
    let curved = select(hi, lo, v < vec3<f32>(0.5));
    let luma = dot(curved, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(vec3<f32>(luma) + (curved - vec3<f32>(luma)) * saturation, vec3<f32>(0.0), vec3<f32>(1.0));
}

/// The engine's default display transform: AgX plus the default look.
/// Returns display-encoded RGB.
fn default_display_transform(color: vec3<f32>) -> vec3<f32> {
    return display_look(tonemap_agx_vec(color), DEFAULT_LOOK_CONTRAST, DEFAULT_LOOK_SATURATION);
}

// Narkowicz 2015, "ACES Filmic Tone Mapping Curve" (public fit of the ACES RRT+ODT).
fn tonemap_aces_vec(color: vec3<f32>) -> vec3<f32> {
    let x = max(color, vec3<f32>(0.0));
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

// Khronos 3D Formats working group, "PBR Neutral" tone mapper (public, 2024).
fn tonemap_neutral_vec(color: vec3<f32>) -> vec3<f32> {
    let start_compression = 0.76;
    let desaturation = 0.15;
    let x = min(color.r, min(color.g, color.b));
    let offset = select(0.04, x - 6.25 * x * x, x < 0.08);
    var mapped = max(color - vec3<f32>(offset), vec3<f32>(0.0));
    let peak = max(mapped.r, max(mapped.g, mapped.b));
    if (peak < start_compression) {
        return mapped;
    }
    let d = 1.0 - start_compression;
    let new_peak = 1.0 - d * d / (peak + d - start_compression);
    mapped = mapped * (new_peak / max(peak, 1e-5));
    let g = 1.0 - 1.0 / (desaturation * (peak - new_peak) + 1.0);
    return clamp(mix(mapped, vec3<f32>(new_peak), g), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn encode_srgb(color: vec3<f32>) -> vec3<f32> {
    return pow(max(color, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
}

/// Inverse of [`encode_srgb`]: display-referred value → linear.
/// Used when blitting an sRGB-encoded LDR image into a linear *Srgb target,
/// which sRGB-encodes on write (IEC 61966-2-1, as cited by `glTF 2.0` § 3.9.2).
fn decode_srgb(color: vec3<f32>) -> vec3<f32> {
    return pow(max(color, vec3<f32>(0.0)), vec3<f32>(2.2));
}

fn sky_color(dir: vec3<f32>) -> vec3<f32> {
    let t = dir.y;
    let horizon = vec3<f32>(0.78, 0.85, 0.95);
    let zenith = vec3<f32>(0.20, 0.40, 0.92);
    // Neutral ground bounce (albedo ~0.25 under the default sun), mirrored in `ibl.rs`.
    let ground = vec3<f32>(0.34, 0.33, 0.31);
    if t > 0.0 {
        let f = 1.0 - exp(-t * 3.0);
        return mix(horizon, zenith, f);
    } else {
        let f = 1.0 - exp(t * 2.0);
        return mix(horizon, ground, f);
    }
}

// Zero for a NaN or infinite channel, tested on the exponent bits: SPIR-V leaves `max(x, 0)`
// (FMax) undefined for NaN, and RADV/ACO keeps the NaN.
fn finite_or_zero(c: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(c) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(c, vec3<f32>(0.0), bad);
}

fn luminance(color: vec3<f32>) -> f32 {
    return dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
}
