// SPDX-License-Identifier: MIT
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/denoiser/ffx_denoiser_reflections_reproject.h,
// _common.h and _config.h (FP32 path),
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

// SSSR denoiser, pass 1: reprojects the reflection history (surface motion or hit parallax,
// whichever matches the local neighbourhood), tracks the temporal variance and sample count, and
// writes the 8×8 average radiance the later passes use against fireflies.
//
// Changes for Regia Engine: radiance is premultiplied rgb plus coverage (four channels denoised
// together); motion is reprojected from depth through the previous view-projection (static world);
// workgroup memory holds f32 instead of packed halves; dispatch over the tile list without the wave
// remap.

#import regia::sssr::{sssr, is_glossy, world_from_uvz, uvz_from_world, linear_depth, luminance, unpack_surface, pack_pair, unpack_pair, finite_or_zero, non_finite4}

struct Counters {
    rays: u32,
    tiles: u32,
}

@group(0) @binding(1) var<storage, read> tile_list: array<u32>;
@group(0) @binding(15) var<storage, read> counters: Counters;

var<workgroup> wg_tile: u32;

// The tile of this workgroup (row-major over 32768-wide rows), or 0xffffffff past the list. The
// value is workgroup-uniform, so a group past the list returns before any barrier.
fn tile_origin(wg: vec3<u32>, li: u32) -> u32 {
    if li == 0u {
        let index = wg.x + wg.y * 32768u;
        wg_tile = select(0xffffffffu, tile_list[min(index, arrayLength(&tile_list) - 1u)], index < counters.tiles);
    }
    return workgroupUniformLoad(&wg_tile);
}
@group(0) @binding(2) var radiance_tex: texture_2d<f32>;
@group(0) @binding(4) var normal_tex: texture_2d<u32>;
@group(0) @binding(5) var normal_history: texture_2d<u32>;
@group(0) @binding(6) var depth_tex: texture_2d<f32>;
@group(0) @binding(7) var depth_history: texture_2d<f32>;
@group(0) @binding(8) var radiance_history: texture_2d<f32>;
// pack_pair(temporal variance, sample count) of the previous resolve.
@group(0) @binding(9) var variance_history: texture_2d<u32>;
@group(0) @binding(11) var lin_samp: sampler;
// Nearest, clamp to edge: gathers the packed normal history's 2×2 footprint.
@group(0) @binding(16) var point_samp: sampler;
@group(0) @binding(12) var reprojected_out: texture_storage_2d<rgba16float, write>;
// In: the intersect's ray length (f32 bits). Out: pack_pair(temporal variance, sample count).
// Each invocation reads and writes only its own pixel.
@group(0) @binding(13) var ray_var: texture_storage_2d<r32uint, read_write>;
@group(0) @binding(14) var average_out: texture_storage_2d<rgba16float, write>;

// ffx_denoiser_reflections_config.h
const GAUSSIAN_K: f32 = 3.0;
const AVG_RADIANCE_LUMINANCE_WEIGHT: f32 = 0.3;
const REPROJECT_SURFACE_DISCARD_VARIANCE_WEIGHT: f32 = 1.5;
const DISOCCLUSION_NORMAL_WEIGHT: f32 = 1.4;
const DISOCCLUSION_DEPTH_WEIGHT: f32 = 1.0;
const DISOCCLUSION_THRESHOLD: f32 = 0.9;
const REPROJECTION_NORMAL_SIMILARITY_THRESHOLD: f32 = 0.9999;
const NEIGHBORHOOD_RADIUS: i32 = 4;
const MAX_SAMPLES: f32 = 32.0;

var<workgroup> lds: array<array<vec4<f32>, 16>, 16>;
var<workgroup> reduce: array<vec4<f32>, 64>;

fn screen_size() -> vec2<i32> {
    return vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y));
}

fn clamp_px(p: vec2<i32>) -> vec2<i32> {
    return clamp(p, vec2<i32>(0), screen_size() - vec2<i32>(1));
}

fn uv_px(uv: vec2<f32>) -> vec2<i32> {
    return clamp_px(vec2<i32>(floor(uv * sssr.screen.xy)));
}

fn history_surface(p: vec2<i32>) -> vec4<f32> {
    return unpack_surface(textureLoad(normal_history, clamp_px(p), 0).r);
}

// Bilinear tap of last frame's normal and roughness (clamp to edge), decoded per texel: what a
// linear sampler returned from the old four-channel float target. One gather fetches the 2×2
// (w = top left, z = top right, x = bottom left, y = bottom right).
fn sample_normal_history(uv: vec2<f32>) -> vec4<f32> {
    let g = textureGather(0, normal_history, point_samp, uv);
    let f = fract(uv * sssr.screen.xy - vec2<f32>(0.5));
    let top = mix(unpack_surface(g.w), unpack_surface(g.z), f.x);
    let bottom = mix(unpack_surface(g.x), unpack_surface(g.y), f.x);
    return mix(top, bottom, f.y);
}

fn temporal_variance(history: vec3<f32>, radiance: vec3<f32>) -> f32 {
    let hl = max(luminance(history), 0.001);
    let l = max(luminance(radiance), 0.001);
    let diff = abs(hl - l) / max(max(hl, l), 0.5);
    return diff * diff;
}

fn kernel_weight(i: f32) -> f32 {
    let radius = f32(NEIGHBORHOOD_RADIUS) + 1.0;
    return exp(-GAUSSIAN_K * (i * i) / (radius * radius));
}

struct Moments {
    mean: vec4<f32>,
    variance: vec4<f32>,
}

fn local_neighborhood(g: vec2<i32>) -> Moments {
    var mean = vec4<f32>(0.0);
    var variance = vec4<f32>(0.0);
    var acc = 0.0;
    for (var j = -NEIGHBORHOOD_RADIUS; j <= NEIGHBORHOOD_RADIUS; j = j + 1) {
        for (var i = -NEIGHBORHOOD_RADIUS; i <= NEIGHBORHOOD_RADIUS; i = i + 1) {
            let r = lds[g.y + j][g.x + i];
            let w = kernel_weight(f32(i)) * kernel_weight(f32(j));
            acc += w;
            mean += r * w;
            variance += r * r * w;
        }
    }
    mean /= acc;
    variance /= acc;
    return Moments(mean, abs(variance - mean * mean));
}

fn disocclusion(normal: vec3<f32>, history_normal: vec3<f32>, lin: f32, history_lin: f32) -> f32 {
    return exp(-abs(1.0 - max(0.0, dot(normal, history_normal))) * DISOCCLUSION_NORMAL_WEIGHT)
        * exp(-abs(history_lin - lin) / lin * DISOCCLUSION_DEPTH_WEIGHT);
}

fn history_linear_depth(uv: vec2<f32>) -> f32 {
    return linear_depth(textureLoad(depth_history, uv_px(uv), 0).r);
}

struct Reprojection {
    disocclusion: f32,
    uv: vec2<f32>,
    radiance: vec4<f32>,
}

fn pick_reprojection(p: vec2<i32>, g: vec2<i32>, roughness: f32, ray_length: f32) -> Reprojection {
    let neighborhood = local_neighborhood(g);
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) * sssr.screen.zw;
    let normal = unpack_surface(textureLoad(normal_tex, p, 0).r).xyz;
    let z = textureLoad(depth_tex, p, 0).r;
    let world = world_from_uvz(uv, z);
    // Surface motion from depth (the world is static between frames).
    let surface_uv = uvz_from_world(sssr.prev_view_proj, world).xy;
    // Hit parallax: the virtual point straight through the reflector at the ray length.
    let to_surface = world - sssr.camera.xyz;
    let surface_distance = length(to_surface);
    let virtual_hit = sssr.camera.xyz + to_surface / max(surface_distance, 1e-6) * (surface_distance + ray_length);
    let hit_uv = uvz_from_world(sssr.prev_view_proj, virtual_hit).xy;
    let surface_nr = sample_normal_history(surface_uv);
    let hit_nr = sample_normal_history(hit_uv);
    let surface_history = textureSampleLevel(radiance_history, lin_samp, surface_uv, 0.0);
    let hit_history = textureSampleLevel(radiance_history, lin_samp, hit_uv, 0.0);
    let hit_similarity = dot(normalize(hit_nr.xyz + vec3<f32>(1e-7)), normal);
    let surface_similarity = dot(normalize(surface_nr.xyz + vec3<f32>(1e-7)), normal);
    var history_normal: vec3<f32>;
    var history_lin: f32;
    // Plain locals (no function-scope struct or array: SPIR-V 1.6 rejects naga's laid-out types there).
    var occ = 0.0;
    var ruv = surface_uv;
    var rad = vec4<f32>(0.0);
    if hit_similarity > REPROJECTION_NORMAL_SIMILARITY_THRESHOLD && hit_similarity + 1e-3 > surface_similarity
        && abs(hit_nr.w - roughness) < abs(surface_nr.w - roughness) + 1e-3 {
        history_normal = hit_nr.xyz;
        history_lin = history_linear_depth(hit_uv);
        ruv = hit_uv;
        rad = hit_history;
    } else {
        let d = surface_history - neighborhood.mean;
        if dot(d, d) < REPROJECT_SURFACE_DISCARD_VARIANCE_WEIGHT * length(neighborhood.variance) {
            history_normal = surface_nr.xyz;
            history_lin = history_linear_depth(surface_uv);
            ruv = surface_uv;
            rad = surface_history;
        } else {
            return Reprojection(0.0, surface_uv, vec4<f32>(0.0));
        }
    }
    let lin = linear_depth(z);
    occ = disocclusion(normal, history_normal, lin, history_lin);
    if occ > DISOCCLUSION_THRESHOLD {
        return Reprojection(occ, ruv, rad);
    }
    // Search the 3×3 around the reprojection for a better match.
    let dudv = sssr.screen.zw;
    let centre_uv = ruv;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let suv = centre_uv + vec2<f32>(f32(x), f32(y)) * dudv;
            let hn = sample_normal_history(suv).xyz;
            let w = disocclusion(normal, hn, lin, history_linear_depth(suv));
            if w > occ {
                occ = w;
                ruv = suv;
            }
        }
    }
    rad = textureSampleLevel(radiance_history, lin_samp, ruv, 0.0);
    // Rare slow path at edges: rebuild the bilinear tap from the history texels that match.
    if occ < DISOCCLUSION_THRESHOLD {
        let fx = fract(sssr.screen.x * ruv.x + 0.5);
        let fy = fract(sssr.screen.y * ruv.y + 0.5);
        let t = vec2<i32>(sssr.screen.xy * ruv - vec2<f32>(0.5));
        let q0 = clamp_px(t);
        let q1 = clamp_px(t + vec2<i32>(1, 0));
        let q2 = clamp_px(t + vec2<i32>(0, 1));
        let q3 = clamp_px(t + vec2<i32>(1, 1));
        let n0 = history_surface(q0).xyz;
        let n1 = history_surface(q1).xyz;
        let n2 = history_surface(q2).xyz;
        let n3 = history_surface(q3).xyz;
        let d0 = linear_depth(textureLoad(depth_history, q0, 0).r);
        let d1 = linear_depth(textureLoad(depth_history, q1, 0).r);
        let d2 = linear_depth(textureLoad(depth_history, q2, 0).r);
        let d3 = linear_depth(textureLoad(depth_history, q3, 0).r);
        let half_thr = DISOCCLUSION_THRESHOLD / 2.0;
        var w = vec4<f32>(
            select(0.0, 1.0, disocclusion(normal, n0, lin, d0) > half_thr),
            select(0.0, 1.0, disocclusion(normal, n1, lin, d1) > half_thr),
            select(0.0, 1.0, disocclusion(normal, n2, lin, d2) > half_thr),
            select(0.0, 1.0, disocclusion(normal, n3, lin, d3) > half_thr),
        );
        w = w * vec4<f32>((1.0 - fx) * (1.0 - fy), fx * (1.0 - fy), (1.0 - fx) * fy, fx * fy);
        w = w / max(w.x + w.y + w.z + w.w, 1e-3);
        rad = textureLoad(radiance_history, q0, 0) * w.x + textureLoad(radiance_history, q1, 0) * w.y
            + textureLoad(radiance_history, q2, 0) * w.z + textureLoad(radiance_history, q3, 0) * w.w;
        let hl = d0 * w.x + d1 * w.y + d2 * w.z + d3 * w.w;
        let hn = n0 * w.x + n1 * w.y + n2 * w.z + n3 * w.w;
        occ = disocclusion(normal, hn, lin, hl);
    }
    occ = select(occ, 0.0, occ < DISOCCLUSION_THRESHOLD);
    return Reprojection(occ, ruv, rad);
}

@compute @workgroup_size(8, 8, 1)
fn reproject(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    let packed = tile_origin(wg, li);
    if packed == 0xffffffffu {
        return;
    }
    let origin = vec2<i32>(i32(packed & 0xffffu), i32((packed >> 16u) & 0xffffu));
    let gt = vec2<i32>(lid.xy);
    let p = origin + gt;
    // 16×16 radiance neighbourhood (4-texel apron) in workgroup memory.
    for (var k = 0; k < 4; k = k + 1) {
        let off = vec2<i32>((k % 2) * 8, (k / 2) * 8);
        lds[gt.y + off.y][gt.x + off.x] = textureLoad(radiance_tex, clamp_px(p - vec2<i32>(4) + off), 0);
    }
    workgroupBarrier();
    let inside = all(p < screen_size());
    let q = clamp_px(p);
    var radiance = textureLoad(radiance_tex, q, 0);
    let roughness = unpack_surface(textureLoad(normal_tex, q, 0).r).w;
    if inside {
        let ray_length = bitcast<f32>(textureLoad(ray_var, q).r);
        if roughness >= 0.0 && is_glossy(roughness) {
            var reprojected = vec4<f32>(0.0);
            var variance = 1.0;
            var samples = 1.0;
            if sssr.temporal.w > 0.5 {
                let pick = pick_reprojection(q, gt + vec2<i32>(4), roughness, ray_length);
                if all(pick.uv > vec2<f32>(0.0)) && all(pick.uv < vec2<f32>(1.0)) && pick.disocclusion >= DISOCCLUSION_THRESHOLD {
                    let hp = uv_px(pick.uv);
                    let history_vs = unpack_pair(textureLoad(variance_history, hp, 0).r);
                    let prev_variance = history_vs.x;
                    let alpha = roughness * roughness;
                    let s_max = max(8.0, MAX_SAMPLES * (1.0 - exp(-alpha * 100.0)));
                    samples = min(s_max, history_vs.y * pick.disocclusion + 1.0);
                    let new_variance = temporal_variance(radiance.rgb, pick.radiance.rgb);
                    variance = mix(new_variance, prev_variance, 1.0 / samples);
                    reprojected = pick.radiance;
                    // Mix the reprojection into the radiance that feeds the 8×8 average.
                    radiance = mix(radiance, reprojected, 0.3);
                }
            }
            textureStore(reprojected_out, q, vec4<f32>(finite_or_zero(reprojected.rgb), reprojected.a));
            textureStore(ray_var, q, vec4<u32>(pack_pair(vec2<f32>(variance, samples)), 0u, 0u, 0u));
        } else {
            textureStore(reprojected_out, q, vec4<f32>(0.0));
            textureStore(ray_var, q, vec4<u32>(0u));
        }
    }
    // 8×8 → 1 luminance-weighted average radiance.
    var weight = max(exp(-max(luminance(radiance.rgb), 0.001) * AVG_RADIANCE_LUMINANCE_WEIGHT), 1e-2);
    var weighted = radiance * weight;
    // NaN/Infinity detected on the exponent bits (SPIR-V leaves `x != x` / FMax undefined for
    // NaN, and RADV/ACO keeps it), as in sssr_common.wgsl's `non_finite4`; the magnitude test still
    // catches large-but-finite values.
    let bad = non_finite4(weighted) || any(abs(weighted) > vec4<f32>(3.0e38)) || weight > 1e3;
    if !inside || bad {
        weighted = vec4<f32>(0.0);
        weight = 0.0;
    }
    reduce[li] = vec4<f32>(weighted.rgb, weight);
    // The coverage channel is averaged unweighted alongside (stored in a second half below).
    workgroupBarrier();
    for (var stride = 32u; stride > 0u; stride = stride / 2u) {
        if li < stride {
            reduce[li] = reduce[li] + reduce[li + stride];
        }
        workgroupBarrier();
    }
    let sum_rgbw = reduce[0];
    workgroupBarrier();
    reduce[li] = vec4<f32>(select(0.0, weighted.a, inside && !bad), 0.0, 0.0, 0.0);
    workgroupBarrier();
    for (var stride = 32u; stride > 0u; stride = stride / 2u) {
        if li < stride {
            reduce[li] = reduce[li] + reduce[li + stride];
        }
        workgroupBarrier();
    }
    if li == 0u {
        let wsum = max(sum_rgbw.w, 1e-3);
        textureStore(average_out, origin / 8, vec4<f32>(sum_rgbw.rgb / wsum, reduce[0].x / wsum));
    }
}
