// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 4: prepare reactivity (render resolution).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_prepare_reactivity.h, tag v1.1.4 only.
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
// Regia Engine changes: no transparency-and-composition mask (Regia Engine composites VFX after the
// upscaler, so it is 0); the reactive mask is optional (`frame.depth.z`); new locks go to a
// storage buffer at the output resolution (FSR: an R8 UAV); a flat 3×3 (max = min) returns
// no lock explicitly instead of relying on 0 / 0 being NaN; motion divergence of a still
// texel is 0 instead of 0 / 0.

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var motion_tex: texture_2d<f32>;
@group(0) @binding(2) var<storage, read> reconstructed_depth: array<u32>;
@group(0) @binding(3) var reactive_tex: texture_2d<f32>;
@group(0) @binding(4) var luma_tex: texture_2d<f32>;
@group(0) @binding(5) var shading_change_tex: texture_2d<f32>;
@group(0) @binding(6) var previous_accumulation_tex: texture_2d<f32>;
@group(0) @binding(7) var linear_clamp: sampler;
// x = reactive (motion divergence), y = disocclusion, z = shading change, w = accumulation.
@group(0) @binding(8) var out_masks: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(9) var out_accumulation: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(10) var<storage, read_write> new_locks: array<f32>;

fn load_motion(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(motion_tex, clamp(p, vec2<i32>(0), irender_size() - vec2<i32>(1)), 0);
}

fn load_reconstructed_prev_depth(p: vec2<i32>) -> f32 {
    let size = irender_size();
    return 1.0 - bitcast<f32>(reconstructed_depth[u32(p.y) * u32(size.x) + u32(p.x)]);
}

fn compute_disocclusions(uv: vec2<f32>, motion_in: vec2<f32>, current_depth_view_space: f32) -> f32 {
    let nearest_depth_in_meters = min(current_depth_view_space, FP16_MAX);
    let threshold = reconstructed_depth_mv_px_threshold(nearest_depth_in_meters);
    let motion = motion_in * f32(get_4k_velocity(motion_in) > threshold);
    let reprojected = uv + motion;
    let sample = reprojected * render_size() - vec2<f32>(0.5);
    let base = vec2<i32>(floor(sample));
    let f = fract(sample);
    let weights = vec4<f32>((1.0 - f.x) * (1.0 - f.y), f.x * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * f.y);
    let size = irender_size();

    var disocclusion = 0.0;
    var weight_sum = 0.0;
    var potential = true;
    for (var i: i32 = 0; i < 4; i = i + 1) {
        if (!potential) {
            break;
        }
        let p = clamp_load(base, vec2<i32>(i & 1, i >> 1u), size);
        let weight = weights[i];
        if (weight > RECONSTRUCTED_DEPTH_BILINEAR_WEIGHT_THRESHOLD) {
            let previous = get_view_space_depth(load_reconstructed_prev_depth(p));
            let difference = current_depth_view_space - previous;
            potential = potential && difference > FP32_MIN;
            if (potential) {
                let half_viewport_width = length(render_size() * 0.5);
                let depth_threshold = max(current_depth_view_space, previous);
                let k_sep = 1.37e-05;
                let required = k_sep * half_viewport_width * depth_threshold;
                disocclusion = disocclusion + saturate(required / difference) * weight;
                weight_sum = weight_sum + weight;
            }
        }
    }
    return select(0.0, saturate(1.0 - disocclusion / max(weight_sum, FP32_MIN)), potential && weight_sum > 0.0);
}

fn compute_motion_divergence(uv: vec2<f32>, motion: vec2<f32>, current_depth: f32) -> f32 {
    let reprojected_px = vec2<i32>((uv + motion) * render_size());
    let reprojected = load_motion(reprojected_px);
    let reprojected_velocity = get_4k_velocity(reprojected.xy);
    let velocity = get_4k_velocity(motion);
    if (velocity <= 0.0) {
        return 0.0;
    }
    let nucleus = get_view_space_depth_in_meters(reprojected.z);
    let current = get_view_space_depth_in_meters(current_depth);
    let distance_factor = min_divided_by_max(nucleus, current);
    let velocity_factor = saturate(velocity / 10.0);
    return (1.0 - saturate(reprojected_velocity / velocity)) * distance_factor * velocity_factor;
}

fn dilate_reactive_masks(px: vec2<i32>) -> f32 {
    if (frame.depth.z < 0.5) {
        return 0.0;
    }
    let size = vec2<i32>(textureDimensions(reactive_tex));
    var dilated = 0.0;
    for (var y: i32 = -1; y <= 1; y = y + 1) {
        for (var x: i32 = -1; x <= 1; x = x + 1) {
            let p = clamp_load(px, vec2<i32>(x, y), size);
            dilated = max(dilated, textureLoad(reactive_tex, p, 0).r * frame.tune3.x);
        }
    }
    return dilated;
}

fn luma_at(px: vec2<i32>, offset: vec2<i32>) -> f32 {
    return textureLoad(luma_tex, clamp_load(px, offset, irender_size()), 0).x * exposure();
}

// Pattern bit of a dissimilar-or-similar sample (index 1..8 in the source's 3×3 order:
// 1 2 3 / 4 0 5 / 6 7 8).
fn compute_thin_feature_confidence(px: vec2<i32>) -> f32 {
    let s0 = luma_at(px, vec2<i32>(0, 0));
    let s1 = luma_at(px, vec2<i32>(-1, -1));
    let s2 = luma_at(px, vec2<i32>(0, -1));
    let s3 = luma_at(px, vec2<i32>(1, -1));
    let s4 = luma_at(px, vec2<i32>(-1, 0));
    let s5 = luma_at(px, vec2<i32>(1, 0));
    let s6 = luma_at(px, vec2<i32>(-1, 1));
    let s7 = luma_at(px, vec2<i32>(0, 1));
    let s8 = luma_at(px, vec2<i32>(1, 1));
    let luma_min = min(min(min(min(s0, s1), min(s2, s3)), min(min(s4, s5), min(s6, s7))), s8);
    let luma_max = max(max(max(max(max(s0, s1), max(s2, s3)), max(max(s4, s5), max(s6, s7))), s8), FP32_MIN);
    let range = luma_max - luma_min;
    if (range <= 0.0) {
        return 0.0;
    }
    let threshold = 0.9;
    var dissimilar_min = FP32_MAX;
    var dissimilar_max = 0.0;
    var pattern: u32 = 1u;
    for (var i: u32 = 1u; i < 9u; i = i + 1u) {
        var s = s1;
        switch i {
            case 2u: { s = s2; }
            case 3u: { s = s3; }
            case 4u: { s = s4; }
            case 5u: { s = s5; }
            case 6u: { s = s6; }
            case 7u: { s = s7; }
            case 8u: { s = s8; }
            default: {}
        }
        if (abs(s - s0) / range < threshold) {
            pattern = pattern | (1u << i);
        } else {
            dissimilar_min = min(dissimilar_min, s);
            dissimilar_max = max(dissimilar_max, s);
        }
    }
    let is_ridge = s0 > dissimilar_max || s0 < dissimilar_min;
    if (!is_ridge) {
        return 0.0;
    }
    // Upper left, upper right, lower left, lower right quads (with the nucleus).
    let masks = vec4<u32>(
        (1u << 1u) | (1u << 2u) | (1u << 4u) | 1u,
        (1u << 2u) | (1u << 3u) | (1u << 5u) | 1u,
        (1u << 4u) | (1u << 6u) | (1u << 7u) | 1u,
        (1u << 5u) | (1u << 7u) | (1u << 8u) | 1u,
    );
    if ((pattern & masks.x) == masks.x || (pattern & masks.y) == masks.y
        || (pattern & masks.z) == masks.z || (pattern & masks.w) == masks.w) {
        return 0.0;
    }
    return 1.0 - luma_min / luma_max;
}

fn update_accumulation(px: vec2<i32>, uv: vec2<f32>, motion: vec2<f32>, disocclusion: f32, shading_change: f32) -> f32 {
    let reprojected = uv + motion;
    var accumulation = 0.0;
    if (is_uv_inside(reprojected) && frame_index() > 0.5) {
        let size = vec2<i32>(previous_render_size());
        let uv_hw = clamp_uv(reprojected, size, size);
        accumulation = saturate(textureSampleLevel(previous_accumulation_tex, linear_clamp, uv_hw, 0.0).x);
    }
    let added_per_frame = frame.tune2.x;
    accumulation = mix(accumulation, 0.0, shading_change);
    let min_disocclusion_accumulation = frame.tune2.y;
    accumulation = mix(accumulation, min(min_disocclusion_accumulation, accumulation), disocclusion);
    accumulation = accumulation * f32(round(accumulation * 100.0) > 1.0);
    textureStore(out_accumulation, px, vec4<f32>(saturate(accumulation + added_per_frame), 0.0, 0.0, 0.0));
    return accumulation;
}

fn compute_shading_change(uv: vec2<f32>) -> f32 {
    // Jitter re-applied here is reverted again when the accumulation pass samples.
    let size = shading_change_render_size();
    let sc_uv = clamp_uv(uv - jitter() * frame.render_size.zw, size, size);
    return saturate(textureSampleLevel(shading_change_tex, linear_clamp, sc_uv, 0.0).x * frame.tune3.y);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    if (!is_on_screen(px, irender_size())) {
        return;
    }
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw;
    let m = load_motion(px);
    let motion = m.xy;
    let dilated_depth = m.z;

    let disocclusion = compute_disocclusions(uv, motion, get_view_space_depth(dilated_depth));
    let shading_change = max(dilate_reactive_masks(px), compute_shading_change(uv));
    let motion_divergence = compute_motion_divergence(uv, motion, dilated_depth);
    // No transparency-and-composition mask: the final reactiveness is the divergence.
    let final_reactiveness = motion_divergence;
    let accumulation = update_accumulation(px, uv, motion, disocclusion, shading_change);

    textureStore(out_masks, px, vec4<f32>(final_reactiveness, disocclusion, shading_change, accumulation));

    let lock_strength = compute_thin_feature_confidence(px);
    if (lock_strength > 0.01) {
        let hr = compute_hr_pos_from_lr_pos(px);
        let out_size = iupscale_size();
        if (is_on_screen(hr, out_size)) {
            new_locks[u32(hr.y) * u32(out_size.x) + u32(hr.x)] = lock_strength;
        }
    }
}
