// SPDX-License-Identifier: MIT
#define_import_path regia::sssr
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/sssr/ffx_sssr_common.h and ffx_sssr_intersect.h,
// Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

// Shared definitions of the stochastic screen-space reflection passes (sssr_*.wgsl).
//
// Regia Engine works in world space with standard Z (0 near, 1 far) and decodes its surface inputs from
// the HDR alpha channel.

struct SsrProbe {
    center: vec4<f32>,
    // xyz = half extent, w = blend weight
    half: vec4<f32>,
}

struct SssrParams {
    inv_view_proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    // The view-projection of the previous SSSR frame (reprojection).
    prev_view_proj: mat4x4<f32>,
    // xyz = eye, w = frame index (noise).
    camera: vec4<f32>,
    // x = width, y = height, z = 1 / width, w = 1 / height
    screen: vec4<f32>,
    // Linear view depth = y / (d + x) for a standard-Z depth d. z = probe count, w = probe scale.
    proj: vec4<f32>,
    // x = perceptual roughness below which rays are traced, y = fade band below x,
    // z = depth thickness at 1 m (m), w = max traversal steps.
    tuning: vec4<f32>,
    // x = history clip weight (temporal stability), y = variance that forces a ray,
    // z = coarsest pyramid level, w = 1 when the history textures hold the previous frame.
    temporal: vec4<f32>,
    // xyz = camera forward, w = near plane distance.
    forward: vec4<f32>,
    // x = perceptual roughness from which rays trace at half resolution (the traversal stops at
    // pyramid level 1), y = their max steps, z = 1 when glossy pixels may not buy their own ray
    // from a noisy history (the cost bound, ADR-2557), w = the cost bound's block-rate roughness
    // (glossy pixels from it trace one ray per 4×4 block; 1 or more = off).
    budget: vec4<f32>,
    // The cost bound (ADR-2557; all zero when off, the base trace): x = 1 when on, y = own rays per
    // 8×8 tile above which the tile traces one ray per quad (0 = no cap), z = steps of a ray at the
    // roughness cutoff (0 = no roughness limit), w = view depth (m) past which the steps fall off.
    bound: vec4<f32>,
    probes: array<SsrProbe, 4>,
    // The local reflection captures' boxes (ADR-2860): center.xyz and w = blend weight (0 = none),
    // half.xyz = half extent and w = the fade distance outside the box.
    captures: array<SsrProbe, 4>,
}

// The cost probe's counters (`SSSR_STATS` variants of the classify and the intersect; word order
// is `sssr.rs` `SssrCostCounters`).
struct SssrCost {
    mirror_pixels: atomic<u32>,
    glossy_pixels: atomic<u32>,
    skipped_pixels: atomic<u32>,
    rays: atomic<u32>,
    variance_rays: atomic<u32>,
    budget_dropped_rays: atomic<u32>,
    shared_pixels: atomic<u32>,
    steps: atomic<u32>,
    exhausted_rays: atomic<u32>,
    half_res_rays: atomic<u32>,
    max_steps: atomic<u32>,
    hits: atomic<u32>,
}

fn cost_bound_on() -> bool {
    return sssr.bound.x > 0.5;
}

// Under the cost bound's block rate (ADR-2557), whether a pixel of this roughness traces one ray per
// 4×4 block (the block's first such pixel in raster order traces, the others share its ray):
// glossy, not a mirror, at least `budget.w`. Always false with the bound off.
fn block_rate(roughness: f32) -> bool {
    return cost_bound_on() && sssr.budget.w < 1.0 && roughness >= sssr.budget.w && is_glossy(roughness)
        && !is_mirror(roughness);
}

// The step limit of one ray under the cost bound: `steps` (the base limit) at a mirror, falling
// linearly to `bound.z` at the roughness cutoff, then scaled by `bound.w / view depth` (not below
// half) past `bound.w` metres. At least 8 steps.
fn bounded_steps(steps: i32, roughness: f32, view_depth: f32) -> i32 {
    var s = f32(steps);
    if sssr.bound.z > 0.0 {
        let t = clamp(roughness / max(sssr.tuning.x, 1e-3), 0.0, 1.0);
        s = mix(s, min(s, sssr.bound.z), t);
    }
    if sssr.bound.w > 0.0 {
        s = s * clamp(sssr.bound.w / max(view_depth, 1e-3), 0.5, 1.0);
    }
    return max(i32(round(s)), 8);
}

// Every SSSR pass binds its parameters here.
@group(0) @binding(0) var<uniform> sssr: SssrParams;

const PI: f32 = 3.14159265358979;

// Surface inputs decoded from the HDR alpha channel the forward pass wrote.
struct SurfaceCode {
    reflective: bool,
    // Perceptual roughness.
    roughness: f32,
    // Specular weight of the reflection (split-sum F0·A + B, luminance).
    weight: f32,
    // Share of the pixel's colour that is environment specular (replaced by a hit).
    env_fraction: f32,
}

// A negative alpha is a code from `ssr_pack` in lab_forward.wgsl: a normal half float whose sign is
// set, exponent 1..=16 and mantissa carry 14 bits (5 roughness, 5 √weight, 4 env share). Alpha in
// 0..1 is the older fade-coded roughness (`ssr.wgsl`) that the terrain, impostors and blended
// draws still write: not traced. A draw that wants stochastic reflections writes a code (the
// terrain does for a material that opts in, `SharedFrameInfo::sssr_codes`).
fn decode_alpha(a: f32) -> SurfaceCode {
    if a < 0.0 {
        // The half float's exponent and mantissa, exactly (frexp gives fract in [0.5, 1)).
        let fe = frexp(-a);
        let e = fe.exp + 14;
        if e < 1 || e > 16 {
            return SurfaceCode(false, 1.0, 0.0, 0.0);
        }
        let mantissa = u32(round((fe.fract * 2.0 - 1.0) * 1024.0));
        let code = (u32(e - 1) << 10u) | (mantissa & 1023u);
        let r = f32(code & 31u) / 31.0;
        let w = f32((code >> 5u) & 31u) / 31.0;
        let f = f32((code >> 10u) & 15u) / 15.0;
        return SurfaceCode(w > 0.0, r, w * w, f);
    }
    return SurfaceCode(false, 1.0, 0.0, 0.0);
}

// The per-pixel surface record the passes share (R32Uint): a 12:12 octahedral normal (Cigolle et
// al. 2014, "A Survey of Efficient Representations for Independent Unit Vectors", JCGT 3(2)) and
// the 5-bit roughness code in the top byte (255: no reflection). FidelityFX keeps a 10:10:10:2
// normal and an 8-bit roughness; WGSL has no 10:10:10:2 storage format.
fn oct_wrap(v: vec2<f32>) -> vec2<f32> {
    return (vec2<f32>(1.0) - abs(v.yx)) * select(vec2<f32>(-1.0), vec2<f32>(1.0), v >= vec2<f32>(0.0));
}

// `roughness` < 0 marks a pixel without a reflection.
fn pack_surface(n: vec3<f32>, roughness: f32) -> u32 {
    var p = n.xy / max(abs(n.x) + abs(n.y) + abs(n.z), 1e-12);
    if n.z < 0.0 {
        p = oct_wrap(p);
    }
    let q = vec2<u32>(round(clamp(p * 0.5 + vec2<f32>(0.5), vec2<f32>(0.0), vec2<f32>(1.0)) * 4095.0));
    let r = select(255u, u32(round(clamp(roughness, 0.0, 1.0) * 31.0)), roughness >= 0.0);
    return q.x | (q.y << 12u) | (r << 24u);
}

// xyz = unit normal, w = roughness (−1: no reflection).
fn unpack_surface(bits: u32) -> vec4<f32> {
    let p = vec2<f32>(f32(bits & 4095u), f32((bits >> 12u) & 4095u)) * (2.0 / 4095.0) - vec2<f32>(1.0);
    var n = vec3<f32>(p, 1.0 - abs(p.x) - abs(p.y));
    let t = max(-n.z, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    let r = bits >> 24u;
    return vec4<f32>(normalize(n), select(f32(r) / 31.0, -1.0, r == 255u));
}

// Two non-negative values as IEEE half floats in one u32 (`x` in the low half), the layout of
// `pack2x16float`, which naga only accepts with the optional SHADER_FLOAT16_IN_FLOAT32 capability.
// Round to nearest even; above 65504 clamps; NaN and negatives store 0.
fn half_bits(v: f32) -> u32 {
    let x = min(select(0.0, v, v > 0.0), 65504.0);
    if x < 6.103515625e-5 {
        // Subnormal: a multiple of 2^-24 (1024 is the smallest normal, which is also exact).
        return u32(round(x * 16777216.0));
    }
    let b = bitcast<u32>(x);
    let m = b & 0x7fffffu;
    var h = ((((b >> 23u) & 255u) - 112u) << 10u) | (m >> 13u);
    let rest = m & 0x1fffu;
    if rest > 0x1000u || (rest == 0x1000u && (h & 1u) == 1u) {
        h = h + 1u;
    }
    return h;
}

fn half_value(h: u32) -> f32 {
    let e = (h >> 10u) & 31u;
    let m = f32(h & 1023u);
    if e == 0u {
        return m * (1.0 / 16777216.0);
    }
    return ldexp(1.0 + m / 1024.0, i32(e) - 15);
}

fn pack_pair(v: vec2<f32>) -> u32 {
    return half_bits(v.x) | (half_bits(v.y) << 16u);
}

fn unpack_pair(bits: u32) -> vec2<f32> {
    return vec2<f32>(half_value(bits & 0xffffu), half_value(bits >> 16u));
}

// Coverage of a surface by traced reflections: 1 below the band, 0 at the threshold.
fn glossy_weight(roughness: f32) -> f32 {
    let thr = sssr.tuning.x;
    let band = max(sssr.tuning.y, 1e-3);
    return clamp((thr - roughness) / band, 0.0, 1.0);
}

fn is_glossy(roughness: f32) -> bool {
    return roughness < sssr.tuning.x;
}

fn is_mirror(roughness: f32) -> bool {
    // Code 0 of the 5-bit roughness: α = r² < 1e-4 in FidelityFX terms.
    return roughness < 0.01;
}

// Screen uv (y down) and standard-Z depth to world space.
fn world_from_uvz(uv: vec2<f32>, z: f32) -> vec3<f32> {
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, z, 1.0);
    let p = sssr.inv_view_proj * ndc;
    return p.xyz / select(p.w, 1e-7, abs(p.w) < 1e-7);
}

// World space to (uv, depth) through `m`.
fn uvz_from_world(m: mat4x4<f32>, p: vec3<f32>) -> vec3<f32> {
    let c = m * vec4<f32>(p, 1.0);
    let w = select(c.w, 1e-7, abs(c.w) < 1e-7);
    let ndc = c.xyz / w;
    return vec3<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5, ndc.z);
}

fn linear_depth(z: f32) -> f32 {
    return sssr.proj.y / max(z + sssr.proj.x, 1e-7);
}

// Zero for a NaN or infinite channel, by the exponent bits: SPIR-V leaves `max(x, 0)` (FMax)
// undefined for NaN and RADV/ACO keeps it, so a NaN stayed in the history (v0.1.4).
fn finite_or_zero(c: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(c) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(c, vec3<f32>(0.0), bad);
}

// True when any component is NaN or infinite, tested on the exponent bits (SPIR-V leaves `x != x`
// / FMax undefined for NaN, and RADV/ACO keeps it). The shape of `finite_or_zero` above.
fn non_finite4(v: vec4<f32>) -> bool {
    return any((bitcast<vec4<u32>>(v) << vec4<u32>(1u)) >= vec4<u32>(0xff000000u));
}

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

// Heitz 2018, "Sampling the GGX Distribution of Visible Normals" (JCGT 7(4)), as in
// ffx_sssr_intersect.h. `ve` is the view direction in the tangent frame (z = normal).
fn sample_ggx_vndf(ve: vec3<f32>, alpha: f32, ua: f32, ub: f32) -> vec3<f32> {
    let vh = normalize(vec3<f32>(alpha * ve.x, alpha * ve.y, ve.z));
    let lensq = vh.x * vh.x + vh.y * vh.y;
    let tx = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(-vh.y, vh.x, 0.0) * inverseSqrt(max(lensq, 1e-12)), lensq > 0.0);
    let ty = cross(vh, tx);
    let r = sqrt(ua);
    let phi = 2.0 * PI * ub;
    let pa = r * cos(phi);
    var pb = r * sin(phi);
    let s = 0.5 * (1.0 + vh.z);
    pb = (1.0 - s) * sqrt(max(1.0 - pa * pa, 0.0)) + s * pb;
    let nh = pa * tx + pb * ty + sqrt(max(0.0, 1.0 - pa * pa - pb * pb)) * vh;
    return normalize(vec3<f32>(alpha * nh.x, alpha * nh.y, max(0.0, nh.z)));
}

// Interleaved gradient noise (Jimenez 2014, "Next Generation Post Processing in Call of Duty:
// Advanced Warfare").
fn ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// Two uniform numbers per pixel and frame: the R2 sequence (Roberts 2018, "The Unreasonable
// Effectiveness of Quasirandom Sequences") over frames, rotated per pixel by interleaved gradient
// noise. Stands in for FidelityFX's Sobol/ranking blue-noise tables.
fn random_pair(pix: vec2<u32>) -> vec2<f32> {
    let frame = f32(u32(sssr.camera.w) % 1024u);
    let seq = fract(vec2<f32>(0.5) + frame * vec2<f32>(0.7548776662, 0.5698402910));
    let p = vec2<f32>(pix);
    return fract(seq + vec2<f32>(ign(p), ign(p.yx + vec2<f32>(37.0, 11.0))));
}

// A reflected direction drawn from the GGX visible normals of `n` (world space). `v` points from the
// eye to the surface.
fn sample_reflection(v: vec3<f32>, n: vec3<f32>, alpha: f32, pix: vec2<u32>) -> vec3<f32> {
    return sample_reflection_with(v, n, alpha, random_pair(pix));
}

// `sample_reflection` for the two uniform numbers `u` (the blue-noise variant draws its own).
fn sample_reflection_with(v: vec3<f32>, n: vec3<f32>, alpha: f32, u: vec2<f32>) -> vec3<f32> {
    if alpha < 1e-4 {
        return reflect(v, n);
    }
    // Tangent frame of n (Frisvad-style branch as in ffx_sssr_intersect.h).
    var t: vec3<f32>;
    if abs(n.z) > 0.0 {
        let k = sqrt(n.y * n.y + n.z * n.z);
        t = vec3<f32>(0.0, -n.z / k, n.y / k);
    } else {
        let k = sqrt(n.x * n.x + n.y * n.y);
        t = vec3<f32>(n.y / k, -n.x / k, 0.0);
    }
    let b = cross(n, t);
    let ve = vec3<f32>(dot(t, -v), dot(b, -v), dot(n, -v));
    let m = sample_ggx_vndf(ve, alpha, u.x, u.y);
    let r = reflect(-ve, m);
    return normalize(t * r.x + b * r.y + n * r.z);
}

fn equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / PI) + 0.5, theta / PI);
}

// How much a probe whose box is `center` ± `half` reflects at `world`: 1 inside the box, easing
// to 0 over a margin outside it (a quarter of its largest half extent, at least 0.5 m). A probe
// is a capture of its own surroundings; past its box the lookup direction no longer means
// anything (a tunnel's probe on the plain outside), so the frame's environment takes over.
fn probe_influence(world: vec3<f32>, center: vec3<f32>, half: vec3<f32>) -> f32 {
    let outside = length(max(abs(world - center) - half, vec3<f32>(0.0)));
    let margin = max(0.25 * max(half.x, max(half.y, half.z)), 0.5);
    return 1.0 - smoothstep(0.0, margin, outside);
}

// Lagarde, SIGGRAPH 2012: the lookup direction of a box-projected probe.
fn box_lookup(origin: vec3<f32>, dir: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>) -> vec3<f32> {
    let rd = normalize(dir + vec3<f32>(1e-6));
    let rbmax = (bmax - origin) / rd;
    let rbmin = (bmin - origin) / rd;
    let rx = select(rbmin.x, rbmax.x, rd.x > 0.0);
    let ry = select(rbmin.y, rbmax.y, rd.y > 0.0);
    let rz = select(rbmin.z, rbmax.z, rd.z > 0.0);
    let fa = max(min(min(rx, ry), rz), 0.0);
    return origin + rd * fa - (bmin + bmax) * 0.5;
}
