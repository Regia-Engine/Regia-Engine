// SPDX-License-Identifier: MIT
// Frame generation: the inpainting pyramid, of the game vectors or of the interpolated colour.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_compute_game_vector_field_inpainting_pyramid.h,
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_compute_inpainting_pyramid.h,
// sdk/include/FidelityFX/gpu/spd/ffx_spd.h,
// tag v1.1.4 only.
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
// Regia Engine changes (ADR-2067): one dispatch per mip instead of the single-pass downsampler,
// whose group-shared intermediate and wave paths do not carry over; each level is the SDK's reduce
// of 2 x 2 texels. Sizes are truncated (the readers halve theirs the same way).

struct PyramidLevel {
    size: vec2<u32>,
    source_size: vec2<u32>,
}

@group(0) @binding(0) var<uniform> level: PyramidLevel;
@group(0) @binding(1) var source: texture_2d<f32>;
@group(0) @binding(2) var out_mip: texture_storage_2d<rgba16float, write>;
@group(0) @binding(3) var<storage, read> game_x: array<u32>;
@group(0) @binding(4) var<storage, read> game_y: array<u32>;
// The shared module's other helpers read it; these entry points do not, so it has no binding entry.
@group(0) @binding(5) var<uniform> frame: FgFrame;

// `SpdLoadSourceImage` of the game vector pyramid.
fn game_sample(p: vec2<u32>) -> vec4<f32> {
    let index = p.y * level.source_size.x + p.x;
    let entry = unpack_entry(vec2<u32>(game_x[index], game_y[index]));
    return vec4<f32>(entry.mv, entry.high, entry.low);
}

// `SpdLoadSourceImage` of the colour pyramid: the sample weight reversed.
fn color_sample(p: vec2<u32>) -> vec4<f32> {
    let c = textureLoad(source, vec2<i32>(p), 0);
    return vec4<f32>(c.rgb, saturate(1.0 - c.w));
}

fn reduce_game(v0: vec4<f32>, v1: vec4<f32>, v2: vec4<f32>, v3: vec4<f32>) -> vec4<f32> {
    let w0 = f32(v0.z > 0.0);
    let w1 = f32(v1.z > 0.0);
    let w2 = f32(v2.z > 0.0);
    let w3 = f32(v3.z > 0.0);
    let sum = w0 + w1 + w2 + w3;
    let total = v0 * w0 + v1 * w1 + v2 * w2 + v3 * w3;
    return total / select(1.0, sum, sum > EPSILON);
}

fn reduce_color(v0: vec4<f32>, v1: vec4<f32>, v2: vec4<f32>, v3: vec4<f32>) -> vec4<f32> {
    let sum = v0.w + v1.w + v2.w + v3.w;
    if (sum == 0.0) {
        return vec4<f32>(0.0);
    }
    return (v0 * v0.w + v1 * v1.w + v2 * v2.w + v3 * v3.w) / sum;
}

fn in_level(gid: vec3<u32>) -> bool {
    return gid.x < level.size.x && gid.y < level.size.y;
}

@compute @workgroup_size(8, 8, 1)
fn cs_game_first(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_level(gid)) {
        return;
    }
    let s = gid.xy * 2u;
    textureStore(out_mip, vec2<i32>(gid.xy), reduce_game(
        game_sample(s),
        game_sample(s + vec2<u32>(1u, 0u)),
        game_sample(s + vec2<u32>(0u, 1u)),
        game_sample(s + vec2<u32>(1u, 1u)),
    ));
}

@compute @workgroup_size(8, 8, 1)
fn cs_game_down(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_level(gid)) {
        return;
    }
    let s = vec2<i32>(gid.xy) * 2;
    textureStore(out_mip, vec2<i32>(gid.xy), reduce_game(
        textureLoad(source, s, 0),
        textureLoad(source, s + vec2<i32>(1, 0), 0),
        textureLoad(source, s + vec2<i32>(0, 1), 0),
        textureLoad(source, s + vec2<i32>(1, 1), 0),
    ));
}

@compute @workgroup_size(8, 8, 1)
fn cs_color_first(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_level(gid)) {
        return;
    }
    let s = gid.xy * 2u;
    textureStore(out_mip, vec2<i32>(gid.xy), reduce_color(
        color_sample(s),
        color_sample(s + vec2<u32>(1u, 0u)),
        color_sample(s + vec2<u32>(0u, 1u)),
        color_sample(s + vec2<u32>(1u, 1u)),
    ));
}

@compute @workgroup_size(8, 8, 1)
fn cs_color_down(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_level(gid)) {
        return;
    }
    let s = vec2<i32>(gid.xy) * 2;
    textureStore(out_mip, vec2<i32>(gid.xy), reduce_color(
        textureLoad(source, s, 0),
        textureLoad(source, s + vec2<i32>(1, 0), 0),
        textureLoad(source, s + vec2<i32>(0, 1), 0),
        textureLoad(source, s + vec2<i32>(1, 1), 0),
    ));
}
