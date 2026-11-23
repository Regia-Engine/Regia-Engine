// SPDX-License-Identifier: MIT
// Frame generation, prepare inputs: dilated depth and motion, reconstructed previous depth.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_reconstruct_dilated_velocity_and_previous_depth.h,
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_prepare_inputs.h,
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
// Regia Engine changes (ADR-2067): the per-pixel motion is built here, because the velocity target
// only holds object motion: camera motion comes from depth for every texel (sky included), object
// motion from the velocity target where the surface that wrote it is the visible one, the rule of
// `fsr3_prepare_inputs.wgsl`. It runs after any upscaler, or none.

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var depth_tex: texture_depth_2d;
@group(0) @binding(2) var velocity_tex: texture_2d<f32>;
// xy = dilated motion, z = dilated (nearest) device depth.
@group(0) @binding(3) var out_motion: texture_storage_2d<rgba32float, write>;
@group(0) @binding(4) var<storage, read_write> reconstructed_previous: array<atomic<u32>>;

fn uv_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

fn input_motion_vector(p: vec2<i32>, d: f32) -> vec2<f32> {
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) * frame.render_size.zw;
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, d, 1.0);
    let w = frame.inv_jittered * ndc;
    // A device depth on the far plane of an infinite projection unprojects to a point at
    // infinity (`w.w` = 0): keep it as that direction (`w` = 0 is exact homogeneous math, and
    // the camera-only motion of the sky is its rotation) instead of dividing by zero into NaN
    // motion (audit-v035-wgsl 3). Any point nearer than 1e7 m takes the division as before.
    let at_infinity = abs(w.w) * 1.0e7 <= length(w.xyz);
    let world = select(vec4<f32>(w.xyz / w.w, 1.0), vec4<f32>(w.xyz, 0.0), at_infinity);
    let cur_clip = frame.cur_vp * world;
    let prev_uv_camera = uv_of(frame.prev_vp * world);
    let camera_motion = prev_uv_camera - uv_of(cur_clip);
    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let velocity = textureLoad(velocity_tex, clamp(p, vec2<i32>(0), vsize - vec2<i32>(1)), 0);
    let accepted = velocity.a > 0.5 && velocity.z > 0.0
        && abs(velocity.z - cur_clip.w) <= 0.01 * cur_clip.w + 1.0e-3;
    if (accepted) {
        let used = uv_of(frame.velocity_prev_vp * world);
        return velocity.xy + (prev_uv_camera - used);
    }
    return camera_motion;
}

fn load_depth(p: vec2<i32>) -> f32 {
    return textureLoad(depth_tex, p, 0);
}

fn nearest_offset(i: i32) -> vec2<i32> {
    // In the source's order.
    switch i {
        case 1: { return vec2<i32>(1, 0); }
        case 2: { return vec2<i32>(0, 1); }
        case 3: { return vec2<i32>(0, -1); }
        case 4: { return vec2<i32>(-1, 0); }
        case 5: { return vec2<i32>(-1, 1); }
        case 6: { return vec2<i32>(1, 1); }
        case 7: { return vec2<i32>(-1, -1); }
        case 8: { return vec2<i32>(1, -1); }
        default: { return vec2<i32>(0, 0); }
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = render_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    var nearest = load_depth(px);
    var nearest_coord = px;
    for (var i = 1; i < 9; i = i + 1) {
        let q = px + nearest_offset(i);
        if (is_on_screen(q, size)) {
            let d = load_depth(q);
            if (d < nearest) {
                nearest = d;
                nearest_coord = q;
            }
        }
    }
    let motion = input_motion_vector(nearest_coord, nearest);
    textureStore(out_motion, px, vec4<f32>(motion, nearest, 0.0));

    // `ReconstructPrevDepth`: push to every texel the reprojected position touches.
    let kept = motion * f32(length(motion * frame.display_size.xy) > 0.1);
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw + kept;
    let base = bilinear_base(uv, size);
    let bits = bitcast<u32>(saturate(1.0 - nearest));
    for (var i = 0; i < 4; i = i + 1) {
        if (bilinear_weight(uv, size, i) > BILINEAR_WEIGHT_THRESHOLD) {
            let store = base + bilinear_offset(i);
            if (is_on_screen(store, size)) {
                atomicMax(&reconstructed_previous[u32(store.y) * u32(size.x) + u32(store.x)], bits);
            }
        }
    }
}
