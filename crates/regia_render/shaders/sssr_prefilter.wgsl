// SPDX-License-Identifier: MIT
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/denoiser/ffx_denoiser_reflections_prefilter.h and
// _config.h (FP32 path),
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

// SSSR denoiser, pass 2: an edge-stopping spatial filter over 15 Halton taps, weighted by normal,
// depth and distance from the 8×8 average radiance, scaled by the temporal variance.
//
// Changes for Regia Engine: premultiplied four-channel radiance; f32 workgroup memory; dispatch over the
// tile list.

#import regia::sssr::{sssr, is_glossy, is_mirror, linear_depth, unpack_surface, pack_pair, unpack_pair, finite_or_zero}

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
// pack_pair(temporal variance, sample count) from the reproject.
@group(0) @binding(3) var variance_tex: texture_2d<u32>;
@group(0) @binding(4) var normal_tex: texture_2d<u32>;
@group(0) @binding(5) var depth_tex: texture_2d<f32>;
@group(0) @binding(6) var average_tex: texture_2d<f32>;
@group(0) @binding(7) var lin_samp: sampler;
@group(0) @binding(8) var radiance_out: texture_storage_2d<rgba16float, write>;
// pack_pair(prefiltered variance, sample count); the resolve updates it in place.
@group(0) @binding(9) var variance_out: texture_storage_2d<r32uint, write>;

const RADIANCE_WEIGHT_BIAS: f32 = 0.6;
const RADIANCE_WEIGHT_VARIANCE_K: f32 = 0.1;
const PREFILTER_VARIANCE_WEIGHT: f32 = 4.4;
const PREFILTER_VARIANCE_BIAS: f32 = 0.1;
const PREFILTER_NORMAL_SIGMA: f32 = 512.0;
const PREFILTER_DEPTH_SIGMA: f32 = 4.0;

struct Neighbor {
    radiance: vec4<f32>,
    variance: f32,
    normal: vec3<f32>,
    depth: f32,
}

var<workgroup> lds_radiance: array<array<vec4<f32>, 16>, 16>;
var<workgroup> lds_normal_variance: array<array<vec4<f32>, 16>, 16>;
var<workgroup> lds_depth: array<array<f32, 16>, 16>;

fn clamp_px(p: vec2<i32>) -> vec2<i32> {
    return clamp(p, vec2<i32>(0), vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y)) - vec2<i32>(1));
}

fn neighbor(g: vec2<i32>) -> Neighbor {
    let nv = lds_normal_variance[g.y][g.x];
    return Neighbor(lds_radiance[g.y][g.x], nv.w, nv.xyz, lds_depth[g.y][g.x]);
}

// First 15 Halton(2,3) points stretched to [-3, 3], centre skipped (a switch, not a function-scope
// array: SPIR-V 1.6 rejects naga's laid-out array type there).
fn halton_offset(i: i32) -> vec2<i32> {
    switch i {
        case 0: { return vec2<i32>(0, 1); }
        case 1: { return vec2<i32>(-2, 1); }
        case 2: { return vec2<i32>(2, -3); }
        case 3: { return vec2<i32>(-3, 0); }
        case 4: { return vec2<i32>(1, 2); }
        case 5: { return vec2<i32>(-1, -2); }
        case 6: { return vec2<i32>(3, 0); }
        case 7: { return vec2<i32>(-3, 3); }
        case 8: { return vec2<i32>(0, -3); }
        case 9: { return vec2<i32>(-1, -1); }
        case 10: { return vec2<i32>(2, 1); }
        case 11: { return vec2<i32>(-2, -2); }
        case 12: { return vec2<i32>(1, 0); }
        case 13: { return vec2<i32>(0, 2); }
        default: { return vec2<i32>(3, -1); }
    }
}

fn normal_weight(np: vec3<f32>, nq: vec3<f32>) -> f32 {
    return pow(max(dot(np, nq), 0.0), PREFILTER_NORMAL_SIGMA);
}

fn depth_weight(center: f32, other: f32) -> f32 {
    return exp(-abs(center - other) * center * PREFILTER_DEPTH_SIGMA);
}

fn radiance_weight(center: vec4<f32>, other: vec4<f32>, variance: f32) -> f32 {
    return max(exp(-(RADIANCE_WEIGHT_BIAS + variance * RADIANCE_WEIGHT_VARIANCE_K) * length(center.rgb - other.rgb)), 1e-2);
}

@compute @workgroup_size(8, 8, 1)
fn prefilter(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let packed = tile_origin(wg, li);
    if packed == 0xffffffffu {
        return;
    }
    let origin = vec2<i32>(i32(packed & 0xffffu), i32((packed >> 16u) & 0xffffu));
    let gt = vec2<i32>(lid.xy);
    let p = origin + gt;
    for (var k = 0; k < 4; k = k + 1) {
        let off = vec2<i32>((k % 2) * 8, (k / 2) * 8);
        let q = clamp_px(p - vec2<i32>(4) + off);
        lds_radiance[gt.y + off.y][gt.x + off.x] = textureLoad(radiance_tex, q, 0);
        let n = unpack_surface(textureLoad(normal_tex, q, 0).r).xyz;
        lds_normal_variance[gt.y + off.y][gt.x + off.x] = vec4<f32>(n, unpack_pair(textureLoad(variance_tex, q, 0).r).x);
        lds_depth[gt.y + off.y][gt.x + off.x] = linear_depth(textureLoad(depth_tex, q, 0).r);
    }
    workgroupBarrier();
    if any(p >= vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y))) {
        return;
    }
    let g = gt + vec2<i32>(4);
    let center = neighbor(g);
    let roughness = unpack_surface(textureLoad(normal_tex, p, 0).r).w;
    let samples = unpack_pair(textureLoad(variance_tex, p, 0).r).y;
    var resolved = center.radiance;
    var resolved_variance = center.variance;
    let needs_denoiser = center.variance > 0.0 && roughness >= 0.0 && is_glossy(roughness) && !is_mirror(roughness);
    if needs_denoiser {
        let size8 = vec2<f32>((vec2<u32>(sssr.screen.xy) + vec2<u32>(7u)) & vec2<u32>(~7u));
        let uv8 = (vec2<f32>(p) + vec2<f32>(0.5)) / size8;
        let avg = textureSampleLevel(average_tex, lin_samp, uv8, 0.0);
        var weight_sum = radiance_weight(avg, center.radiance, center.variance);
        var radiance_sum = center.radiance * weight_sum;
        var variance_sum = center.variance * weight_sum * weight_sum;
        let variance_weight = max(PREFILTER_VARIANCE_BIAS, 1.0 - exp(-(center.variance * PREFILTER_VARIANCE_WEIGHT)));
        for (var i = 0; i < 15; i = i + 1) {
            let nb = neighbor(g + halton_offset(i));
            var w = normal_weight(center.normal, nb.normal);
            w *= depth_weight(center.depth, nb.depth);
            w *= radiance_weight(avg, nb.radiance, center.variance);
            w *= variance_weight;
            weight_sum += w;
            radiance_sum += w * nb.radiance;
            variance_sum += w * w * nb.variance;
        }
        resolved = radiance_sum / weight_sum;
        resolved_variance = variance_sum / (weight_sum * weight_sum);
    }
    textureStore(radiance_out, p, vec4<f32>(finite_or_zero(resolved.rgb), resolved.a));
    textureStore(variance_out, p, vec4<u32>(pack_pair(vec2<f32>(resolved_variance, samples)), 0u, 0u, 0u));
}
