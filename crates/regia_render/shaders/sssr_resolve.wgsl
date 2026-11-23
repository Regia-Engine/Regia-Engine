// SPDX-License-Identifier: MIT
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/denoiser/ffx_denoiser_reflections_resolve_temporal.h and
// _common.h (FP32 path),
// Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence.
// The history AABB clip (in FidelityFX's ffx_denoiser_reflections_common.h) is from Playdead's
// "Temporal Reprojection Anti-Aliasing" (https://github.com/playdeadgames/temporal),
// Copyright (c) 2015 Playdead, MIT licence (same permission notice).
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

// SSSR denoiser, pass 3: temporal accumulation. The reprojected history is clipped to the local
// neighbourhood (Playdead TAA AABB clip), blended with the prefiltered radiance by the sample count,
// and written as the history and the reflection the composite reads.
//
// Changes for Regia Engine: premultiplied four-channel radiance; f32 workgroup memory; dispatch over the
// tile list.

#import regia::sssr::{sssr, is_glossy, is_mirror, luminance, unpack_surface, pack_pair, unpack_pair, finite_or_zero, non_finite4}

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
@group(0) @binding(5) var average_tex: texture_2d<f32>;
@group(0) @binding(6) var reprojected_tex: texture_2d<f32>;
@group(0) @binding(7) var normal_tex: texture_2d<u32>;
@group(0) @binding(8) var lin_samp: sampler;
@group(0) @binding(9) var radiance_out: texture_storage_2d<rgba16float, write>;
// In: pack_pair(prefiltered variance, sample count). Out: the temporal variance and the same
// sample count, the history the next frame's classify and reproject read. Own pixel only.
@group(0) @binding(10) var variance_io: texture_storage_2d<r32uint, read_write>;

const GAUSSIAN_K: f32 = 3.0;
const NEIGHBORHOOD_RADIUS: i32 = 4;
const RADIANCE_THRESHOLD: f32 = 0.0001;

var<workgroup> lds: array<array<vec4<f32>, 16>, 16>;

fn clamp_px(p: vec2<i32>) -> vec2<i32> {
    return clamp(p, vec2<i32>(0), vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y)) - vec2<i32>(1));
}

fn kernel_weight(i: f32) -> f32 {
    let radius = f32(NEIGHBORHOOD_RADIUS) + 1.0;
    return exp(-GAUSSIAN_K * (i * i) / (radius * radius));
}

fn temporal_variance(history: vec3<f32>, radiance: vec3<f32>) -> f32 {
    let hl = max(luminance(history), 0.001);
    let l = max(luminance(radiance), 0.001);
    let diff = abs(hl - l) / max(max(hl, l), 0.5);
    return diff * diff;
}

// Clips `prev` toward the centre of the box [lo, hi] (Playdead TAA).
fn clip_aabb(lo: vec4<f32>, hi: vec4<f32>, prev: vec4<f32>) -> vec4<f32> {
    let center = 0.5 * (hi + lo);
    let extent = 0.5 * (hi - lo) + vec4<f32>(0.001);
    let v = prev - center;
    let unit = abs(v / extent);
    let m = max(max(unit.x, unit.y), max(unit.z, unit.w));
    if m > 1.0 {
        return center + v / m;
    }
    return prev;
}

@compute @workgroup_size(8, 8, 1)
fn resolve(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let packed = tile_origin(wg, li);
    if packed == 0xffffffffu {
        return;
    }
    let origin = vec2<i32>(i32(packed & 0xffffu), i32((packed >> 16u) & 0xffffu));
    let gt = vec2<i32>(lid.xy);
    let p = origin + gt;
    for (var k = 0; k < 4; k = k + 1) {
        let off = vec2<i32>((k % 2) * 8, (k / 2) * 8);
        lds[gt.y + off.y][gt.x + off.x] = textureLoad(radiance_tex, clamp_px(p - vec2<i32>(4) + off), 0);
    }
    workgroupBarrier();
    if any(p >= vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y))) {
        return;
    }
    let g = gt + vec2<i32>(4);
    var signal = lds[g.y][g.x];
    let vs = unpack_pair(textureLoad(variance_io, p).r);
    let samples = vs.y;
    if signal.r + signal.g + signal.b + signal.a < RADIANCE_THRESHOLD {
        textureStore(radiance_out, p, vec4<f32>(0.0));
        textureStore(variance_io, p, vec4<u32>(pack_pair(vec2<f32>(0.0, samples)), 0u, 0u, 0u));
        return;
    }
    let roughness = unpack_surface(textureLoad(normal_tex, p, 0).r).w;
    var variance = vs.x;
    // Mirror pixels trace their own deterministic ray every frame: nothing to denoise, and the
    // outlier clip toward the 8×8 average would dim a small bright reflection (a lit object in a
    // mirror floor). Deviation from FidelityFX, which also resolves them (ADR-103).
    if roughness >= 0.0 && is_glossy(roughness) && !is_mirror(roughness) {
        let size8 = vec2<f32>((vec2<u32>(sssr.screen.xy) + vec2<u32>(7u)) & vec2<u32>(~7u));
        let uv8 = (vec2<f32>(p) + vec2<f32>(0.5)) / size8;
        let avg = textureSampleLevel(average_tex, lin_samp, uv8, 0.0);
        let old_signal = textureLoad(reprojected_tex, p, 0);
        // Local neighbourhood moments.
        var mean = vec4<f32>(0.0);
        var m2 = vec4<f32>(0.0);
        var acc = 0.0;
        for (var j = -NEIGHBORHOOD_RADIUS; j <= NEIGHBORHOOD_RADIUS; j = j + 1) {
            for (var i = -NEIGHBORHOOD_RADIUS; i <= NEIGHBORHOOD_RADIUS; i = i + 1) {
                let r = lds[g.y + j][g.x + i];
                let w = kernel_weight(f32(i)) * kernel_weight(f32(j));
                acc += w;
                mean += r * w;
                m2 += r * r * w;
            }
        }
        mean /= acc;
        let local_variance = abs(m2 / acc - mean * mean);
        // Clip history by the current local statistics.
        let color_std = (sqrt(local_variance) + vec4<f32>(length(mean - avg))) * sssr.temporal.x * 1.4;
        mean = mix(mean, avg, vec4<f32>(0.2));
        let clipped_old = clip_aabb(mean - color_std, mean + color_std, old_signal);
        let speed = 1.0 / max(samples, 1.0);
        let weight = 1.0 - speed;
        // Blend toward the average while the sample count is small, then clip outliers.
        signal = mix(signal, avg, 1.0 / max(samples + 1.0, 1.0));
        signal = clip_aabb(avg - color_std, avg + color_std, signal);
        signal = mix(signal, clipped_old, weight);
        variance = mix(temporal_variance(signal.rgb, clipped_old.rgb), variance, weight);
        // NaN/Infinity detected on the exponent bits (SPIR-V leaves `x != x` / FMax undefined for
        // NaN, and RADV/ACO keeps it), as in sssr_common.wgsl's `non_finite4`; the magnitude test
        // still catches large-but-finite values.
        let bad = non_finite4(signal)
            || any(abs(signal) > vec4<f32>(3.0e38))
            || (bitcast<u32>(variance) << 1u) >= 0xff000000u;
        if bad {
            signal = vec4<f32>(0.0);
            variance = 0.0;
        }
    }
    textureStore(radiance_out, p, vec4<f32>(max(finite_or_zero(signal.rgb), vec3<f32>(0.0)), max(signal.a, 0.0)));
    textureStore(variance_io, p, vec4<u32>(pack_pair(vec2<f32>(variance, samples)), 0u, 0u, 0u));
}
