// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 2: shading change pyramid and farthest depth mip 1 (render / 2, / 4, / 8).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_shading_change_pyramid.h and
// ffx_fsr3upscaler_luma_pyramid.h, tag v1.1.4 only.
//
// Copyright (C) 2024 Advanced Micro Devices, Inc.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files(the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and /or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions :
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
//
// Regia Engine changes: FSR runs two full single-pass-downsampler (SPD) chains, but only three
// mips of the shading change pyramid and mip 1 of the farthest depth are read afterwards,
// and Regia Engine supplies the exposure (no auto exposure, so the luma pyramid's frame info is
// unused). One dispatch builds exactly those: each 8×8 group averages a 16×16 source block
// into mip 0 (shared memory), then mips 1 and 2, with the same clamped source loads and
// 2×2 box reductions as SPD (`SpdReduce4` is the mean). Farthest depth mip 1 rides in mip 0's
// `z`. The sorting network and the minimum-difference walk are unrolled into scalars (no
// function-local arrays, ADR-026).

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
// x = luma, y = farthest depth (metres); this frame and the previous one.
@group(0) @binding(1) var luma_tex: texture_2d<f32>;
@group(0) @binding(2) var previous_luma_tex: texture_2d<f32>;
@group(0) @binding(3) var motion_tex: texture_2d<f32>;
// x = signed luma difference, y = sign sum, z = farthest depth (mip 0 only).
@group(0) @binding(4) var out_mip0: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var out_mip1: texture_storage_2d<rgba16float, write>;
@group(0) @binding(6) var out_mip2: texture_storage_2d<rgba16float, write>;

var<workgroup> reduce_a: array<vec2<f32>, 64>;

// Five luma samples (centre, −x, +x, −y, +y).
struct Set5 {
    s0: f32,
    s1: f32,
    s2: f32,
    s3: f32,
    s4: f32,
}

fn sample_offset(i: i32) -> vec2<i32> {
    switch i {
        case 1: { return vec2<i32>(-1, 0); }
        case 2: { return vec2<i32>(1, 0); }
        case 3: { return vec2<i32>(0, -1); }
        case 4: { return vec2<i32>(0, 1); }
        default: { return vec2<i32>(0, 0); }
    }
}

fn set_get(s: Set5, i: i32) -> f32 {
    switch i {
        case 0: { return s.s0; }
        case 1: { return s.s1; }
        case 2: { return s.s2; }
        case 3: { return s.s3; }
        default: { return s.s4; }
    }
}

// The source's 9-comparator network for five values.
fn sort_set(s: Set5) -> Set5 {
    var a = s.s0;
    var b = s.s1;
    var c = s.s2;
    var d = s.s3;
    var e = s.s4;
    var t = 0.0;
    // (0,3)
    t = min(a, d); d = max(a, d); a = t;
    // (1,4)
    t = min(b, e); e = max(b, e); b = t;
    // (0,2)
    t = min(a, c); c = max(a, c); a = t;
    // (1,3)
    t = min(b, d); d = max(b, d); b = t;
    // (0,1)
    t = min(a, b); b = max(a, b); a = t;
    // (2,4)
    t = min(c, e); e = max(c, e); c = t;
    // (1,2)
    t = min(b, c); c = max(b, c); b = t;
    // (3,4)
    t = min(d, e); e = max(d, e); d = t;
    // (2,3)
    t = min(c, d); d = max(c, d); c = t;
    return Set5(a, b, c, d, e);
}

fn compute_minimum_difference(set0_in: Set5, set1_in: Set5) -> f32 {
    var min_diff = FP16_MAX - 1.0;
    var a: i32 = 0;
    var b: i32 = 0;
    let set0 = sort_set(set0_in);
    let set1 = sort_set(set1_in);
    let fmax = min(set0.s4, set1.s4);
    if (fmax > FP32_MIN) {
        for (var i: i32 = 0; i < 5; i = i + 1) {
            if (min_diff >= FP16_MAX) {
                break;
            }
            let va = set_get(set0, min(a, 4));
            let vb = set_get(set1, min(b, 4));
            var diff = va - vb;
            if (abs(diff) > FP16_MIN) {
                diff = sign(diff) * (1.0 - min_divided_by_max(va, vb));
                min_diff = select(min_diff, diff, abs(diff) < abs(min_diff));
                a = a + i32(va < vb);
                // As in the source, `b` compares against the already advanced `a`.
                b = b + i32(set_get(set0, min(a, 4)) >= vb);
            } else {
                min_diff = FP16_MAX;
            }
        }
    }
    return min_diff * f32(min_diff < FP16_MAX - 1.0);
}

fn current_luma_set(uv: vec2<f32>) -> Set5 {
    let size = irender_size();
    let base = vec2<i32>(floor((uv + jitter() * frame.render_size.zw) * render_size()));
    let e = exposure();
    return Set5(
        max(textureLoad(luma_tex, clamp_load(base, sample_offset(0), size), 0).x * e, EPSILON),
        max(textureLoad(luma_tex, clamp_load(base, sample_offset(1), size), 0).x * e, EPSILON),
        max(textureLoad(luma_tex, clamp_load(base, sample_offset(2), size), 0).x * e, EPSILON),
        max(textureLoad(luma_tex, clamp_load(base, sample_offset(3), size), 0).x * e, EPSILON),
        max(textureLoad(luma_tex, clamp_load(base, sample_offset(4), size), 0).x * e, EPSILON),
    );
}

fn previous_luma_set(base: vec2<i32>) -> Set5 {
    let size = vec2<i32>(previous_render_size());
    let e = delta_pre_exposure() * exposure();
    return Set5(
        max(textureLoad(previous_luma_tex, clamp_load(base, sample_offset(0), size), 0).x * e, EPSILON),
        max(textureLoad(previous_luma_tex, clamp_load(base, sample_offset(1), size), 0).x * e, EPSILON),
        max(textureLoad(previous_luma_tex, clamp_load(base, sample_offset(2), size), 0).x * e, EPSILON),
        max(textureLoad(previous_luma_tex, clamp_load(base, sample_offset(3), size), 0).x * e, EPSILON),
        max(textureLoad(previous_luma_tex, clamp_load(base, sample_offset(4), size), 0).x * e, EPSILON),
    );
}

fn compute_diff(uv: vec2<f32>, motion: vec2<f32>) -> f32 {
    let current = current_luma_set(uv);
    let reprojected = uv + previous_jitter() / previous_render_size() + motion;
    if (!is_uv_inside(reprojected) || frame_index() < 0.5) {
        return 0.0;
    }
    let base = vec2<i32>(floor(reprojected * previous_render_size()));
    return compute_minimum_difference(current, previous_luma_set(base));
}

// SPD source value: (difference, sign) for shading change, farthest depth for the luma pyramid.
fn source_value(p: vec2<i32>) -> vec3<f32> {
    let q = clamp_load(p, vec2<i32>(0), irender_size());
    let motion = textureLoad(motion_tex, q, 0).xy;
    let uv = (vec2<f32>(q) + vec2<f32>(0.5)) * frame.render_size.zw;
    let diff = compute_diff(uv, motion);
    let sign_sum = select(0.0, sign(diff), diff != 0.0);
    let farthest = textureLoad(luma_tex, q, 0).y;
    return vec3<f32>(diff, sign_sum, farthest);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let local = vec2<i32>(lid.xy);
    let m0 = vec2<i32>(wg.xy) * 8 + local;
    let src = m0 * 2;
    let v = (source_value(src) + source_value(src + vec2<i32>(1, 0))
        + source_value(src + vec2<i32>(0, 1)) + source_value(src + vec2<i32>(1, 1))) * 0.25;
    let size0 = vec2<i32>(textureDimensions(out_mip0));
    if (all(m0 < size0)) {
        textureStore(out_mip0, m0, vec4<f32>(v, 0.0));
    }
    let index = lid.y * 8u + lid.x;
    reduce_a[index] = v.xy;
    workgroupBarrier();

    var v1 = vec2<f32>(0.0);
    if (all(local < vec2<i32>(4))) {
        let i = u32(local.y * 16 + local.x * 2);
        v1 = (reduce_a[i] + reduce_a[i + 1u] + reduce_a[i + 8u] + reduce_a[i + 9u]) * 0.25;
        let m1 = vec2<i32>(wg.xy) * 4 + local;
        let size1 = vec2<i32>(textureDimensions(out_mip1));
        if (all(m1 < size1)) {
            textureStore(out_mip1, m1, vec4<f32>(v1, 0.0, 0.0));
        }
    }
    workgroupBarrier();
    if (all(local < vec2<i32>(4))) {
        reduce_a[u32(local.y * 4 + local.x)] = v1;
    }
    workgroupBarrier();
    if (all(local < vec2<i32>(2))) {
        let i = u32(local.y * 8 + local.x * 2);
        let v2 = (reduce_a[i] + reduce_a[i + 1u] + reduce_a[i + 4u] + reduce_a[i + 5u]) * 0.25;
        let m2 = vec2<i32>(wg.xy) * 2 + local;
        let size2 = vec2<i32>(textureDimensions(out_mip2));
        if (all(m2 < size2)) {
            textureStore(out_mip2, m2, vec4<f32>(v2, 0.0, 0.0));
        }
    }
}
