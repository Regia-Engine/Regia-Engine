// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 1: prepare inputs (render resolution).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_prepare_inputs.h, tag v1.1.4 only.
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
// Regia Engine changes: the per-pixel input motion vector is built here, because Regia Engine's
// velocity target only holds object motion. Camera motion comes from depth for every texel
// (sky included), object motion from the velocity target where the surface that wrote it is
// the visible one (the same rule as `tsr_prepare.wgsl`). FSR's low-resolution motion vector
// path (the SDK default). The reconstructed previous depth is a storage buffer of
// `bitcast<u32>(1 − depth)` updated with `atomicMax` (FSR: an R32_UINT image with
// `InterlockedMin` on the depth bits), so a cleared buffer reads as the far plane. Current
// luma and farthest depth share one RGBA16F target (x, y).

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var depth_tex: texture_depth_2d;
@group(0) @binding(2) var velocity_tex: texture_2d<f32>;
@group(0) @binding(3) var color_tex: texture_2d<f32>;
// xy = dilated motion (previous uv − current uv), z = dilated (nearest) device depth.
@group(0) @binding(4) var out_motion: texture_storage_2d<rgba32float, write>;
// x = current luma, y = farthest depth in metres.
@group(0) @binding(5) var out_luma: texture_storage_2d<rgba16float, write>;
@group(0) @binding(6) var<storage, read_write> reconstructed_depth: array<atomic<u32>>;

fn load_input_depth(p: vec2<i32>) -> f32 {
    return textureLoad(depth_tex, clamp(p, vec2<i32>(0), irender_size() - vec2<i32>(1)), 0);
}

fn uv_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

// Regia Engine's input motion vector of texel `p` at device depth `d`.
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
    var motion = prev_uv_camera - uv_of(cur_clip);
    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let velocity = textureLoad(velocity_tex, clamp(p, vec2<i32>(0), vsize - vec2<i32>(1)), 0);
    // The velocity pass has no depth test: its motion counts only where the surface that
    // wrote it (view depth in `z`) is the visible one. `z` = 0 (terrain) is static.
    let accepted = velocity.a > 0.5 && velocity.z > 0.0
        && abs(velocity.z - cur_clip.w) <= 0.01 * cur_clip.w + 1.0e-3;
    if (accepted) {
        // The camera-only difference between the jitter-free previous matrix and the one
        // the velocity pass used converts its convention exactly.
        let used = uv_of(frame.velocity_prev_vp * world);
        motion = velocity.xy + (prev_uv_camera - used);
    }
    return motion;
}

fn reconstruct_prev_depth(px: vec2<i32>, depth: f32, motion_in: vec2<f32>) {
    let nearest_depth_in_meters = min(get_view_space_depth_in_meters(depth), FP16_MAX);
    let threshold = reconstructed_depth_mv_px_threshold(nearest_depth_in_meters);
    // Discard small motion vectors.
    let motion = motion_in * f32(get_4k_velocity(motion_in) > threshold);
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw;
    let reprojected = uv + motion;
    // Bilinear footprint of the reprojected position; push to every texel with weight.
    let sample = reprojected * render_size() - vec2<f32>(0.5);
    let base = vec2<i32>(floor(sample));
    let f = fract(sample);
    let weights = vec4<f32>((1.0 - f.x) * (1.0 - f.y), f.x * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * f.y);
    let bits = bitcast<u32>(saturate(1.0 - depth));
    let size = irender_size();
    for (var i: i32 = 0; i < 4; i = i + 1) {
        let offset = vec2<i32>(i & 1, i >> 1u);
        let weight = weights[i];
        if (weight > RECONSTRUCTED_DEPTH_BILINEAR_WEIGHT_THRESHOLD) {
            let store = base + offset;
            if (is_on_screen(store, size)) {
                atomicMax(&reconstructed_depth[u32(store.y) * u32(size.x) + u32(store.x)], bits);
            }
        }
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = irender_size();
    if (!is_on_screen(px, size)) {
        return;
    }

    // FindDepthExtents: nearest of the 3×3 (standard depth: smaller is nearer). As in the
    // source, the farthest value only moves when a nearer sample replaces the nucleus.
    let d0 = load_input_depth(px);
    var nearest = d0;
    var nearest_coord = px;
    var farthest = d0;
    for (var i: i32 = 1; i < 9; i = i + 1) {
        // Offsets in the source's order: (+1,0) (0,+1) (0,−1) (−1,0) (−1,+1) (+1,+1) (−1,−1) (+1,−1).
        var o = vec2<i32>(1, 0);
        switch i {
            case 2: { o = vec2<i32>(0, 1); }
            case 3: { o = vec2<i32>(0, -1); }
            case 4: { o = vec2<i32>(-1, 0); }
            case 5: { o = vec2<i32>(-1, 1); }
            case 6: { o = vec2<i32>(1, 1); }
            case 7: { o = vec2<i32>(-1, -1); }
            case 8: { o = vec2<i32>(1, -1); }
            default: {}
        }
        let q = px + o;
        if (is_on_screen(q, size)) {
            let d = load_input_depth(q);
            if (d < nearest) {
                farthest = max(farthest, d);
                nearest_coord = q;
                nearest = d;
            }
        }
    }

    let motion = input_motion_vector(nearest_coord, nearest);
    reconstruct_prev_depth(px, nearest, motion);
    textureStore(out_motion, px, vec4<f32>(motion, nearest, 0.0));

    let farthest_in_meters = min(get_view_space_depth_in_meters(farthest), FP16_MAX);
    // Linear input; the luma stays pre-exposed (Exposure() is applied by the readers).
    let rgb = max(vec3<f32>(0.0), finite_or_zero(textureLoad(color_tex, px, 0).rgb));
    textureStore(out_luma, px, vec4<f32>(min(rgb_to_luma(rgb), FP16_MAX), farthest_in_meters, 0.0, 0.0));
}
