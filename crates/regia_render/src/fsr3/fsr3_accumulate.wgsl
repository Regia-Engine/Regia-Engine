// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 6: accumulate (output resolution).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_accumulate.h, ffx_fsr3upscaler_upsample.h,
// ffx_fsr3upscaler_reproject.h and the Lanczos history sampler of ffx_fsr3upscaler_sample.h,
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
// Regia Engine changes: the generic (non-Xbox) FP32 path with HDR input, the low-resolution
// motion vectors and the approximate Lanczos upsample kernel, as the SDK builds it by
// default; reprojection uses the reference Lanczos (the source's default). The 3×3 sample
// array and the 4×4 history taps are walked in loops that keep scalar running sums (no
// function-local arrays, ADR-026). Farthest depth mip 1 is mip 0's `z` of the pyramid. New
// locks come from a storage buffer. The debug views (`frame.tune2.w`) are Regia Engine's. A
// restarting pixel takes its nearest sample instead of the 3×3 mean (ADR-7678).

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var color_tex: texture_2d<f32>;
@group(0) @binding(2) var motion_tex: texture_2d<f32>;
@group(0) @binding(3) var masks_tex: texture_2d<f32>;
@group(0) @binding(4) var instability_tex: texture_2d<f32>;
@group(0) @binding(5) var pyramid_mip0_tex: texture_2d<f32>;
@group(0) @binding(6) var previous_history_tex: texture_2d<f32>;
@group(0) @binding(7) var linear_clamp: sampler;
@group(0) @binding(8) var<storage, read_write> new_locks: array<f32>;
// rgb = accumulated colour (pre-exposed input domain), a = lock.
@group(0) @binding(9) var out_history: texture_storage_2d<rgba16float, write>;
// Final colour when RCAS is off, or a debug view.
@group(0) @binding(10) var out_color: texture_storage_2d<rgba16float, write>;

// Lanczos-2 interpolation of four values at fraction `t` (the source's `Lanczos2(c0..c3, t)`).
fn lanczos_weights(t: f32) -> vec4<f32> {
    return vec4<f32>(lanczos2(-1.0 - t), lanczos2(-t), lanczos2(1.0 - t), lanczos2(2.0 - t));
}

fn load_history(p: vec2<i32>, size: vec2<i32>) -> vec4<f32> {
    // ClampCoord: [1, size − 2].
    let q = max(vec2<i32>(1), min(p, size - vec2<i32>(2)));
    return textureLoad(previous_history_tex, clamp(q, vec2<i32>(0), size - vec2<i32>(1)), 0);
}

// Reference Lanczos-2 4×4 history sample with deringing to the inner 2×2.
fn history_sample(uv: vec2<f32>, size_f: vec2<f32>) -> vec4<f32> {
    let size = vec2<i32>(size_f);
    var p = uv * size_f - vec2<f32>(0.5);
    let frac = fract(p);
    p = clamp(p, vec2<f32>(0.0), size_f - vec2<f32>(1.0));
    let base = vec2<i32>(floor(p));
    let wx = lanczos_weights(frac.x);
    let wy = lanczos_weights(frac.y);
    let wx_sum = wx.x + wx.y + wx.z + wx.w;
    var sum = vec4<f32>(0.0);
    var lo = vec4<f32>(FP32_MAX);
    var hi = vec4<f32>(-FP32_MAX);
    for (var y: i32 = 0; y < 4; y = y + 1) {
        let row = base.y - 1 + y;
        let c0 = load_history(vec2<i32>(base.x - 1, row), size);
        let c1 = load_history(vec2<i32>(base.x, row), size);
        let c2 = load_history(vec2<i32>(base.x + 1, row), size);
        let c3 = load_history(vec2<i32>(base.x + 2, row), size);
        let row_color = (wx.x * c0 + wx.y * c1 + wx.z * c2 + wx.w * c3) / wx_sum;
        sum = sum + wy[y] * row_color;
        if (y == 1 || y == 2) {
            lo = min(lo, min(c1, c2));
            hi = max(hi, max(c1, c2));
        }
    }
    let color = sum / (wy.x + wy.y + wy.z + wy.w);
    return clamp(color, lo, hi);
}

fn load_prepared_color(p: vec2<i32>) -> vec3<f32> {
    let rgb = max(vec3<f32>(0.0), finite_or_zero(textureLoad(color_tex, p, 0).rgb)) * exposure();
    return rgb_to_ycocg(rgb);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let out_size = iupscale_size();
    if (!is_on_screen(px, out_size)) {
        return;
    }
    let rsize = irender_size();

    // ---- InitPassData ----
    let hr_uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.upscale_size.zw;
    let lr_uv_jittered = hr_uv + jitter() * frame.render_size.zw;
    let lr_uv_hw = clamp_uv(lr_uv_jittered, rsize, rsize);
    let motion = textureLoad(motion_tex, clamp(vec2<i32>(hr_uv * render_size()), vec2<i32>(0), rsize - vec2<i32>(1)), 0).xy;
    let velocity_4k = get_4k_velocity(motion);
    let reprojected_hr_uv = hr_uv + motion;
    let is_existing_sample = is_uv_inside(reprojected_hr_uv);
    let luma_instability = textureSampleLevel(instability_tex, linear_clamp, clamp_uv(hr_uv, rsize, rsize), 0.0).x;
    let half_size = rsize / vec2<i32>(2);
    let farthest_uv = clamp_uv(lr_uv_jittered, half_size, vec2<i32>(textureDimensions(pyramid_mip0_tex)));
    let farthest_depth_in_meters = textureSampleLevel(pyramid_mip0_tex, linear_clamp, farthest_uv, 0.0).z;
    let is_new_sample = !is_existing_sample || frame_index() < 0.5;
    let masks = textureSampleLevel(masks_tex, linear_clamp, lr_uv_hw, 0.0);
    let reactive = saturate(masks.x);
    let disocclusion = saturate(masks.y);
    let shading_change = saturate(masks.z);
    var accumulation = saturate(masks.w);
    accumulation = accumulation * f32(round(accumulation * 100.0) > 1.0);

    // ---- ReprojectHistoryColor ----
    var history_color = vec3<f32>(0.0);
    var lock = 0.0;
    if (is_existing_sample && !is_new_sample) {
        let h = history_sample(reprojected_hr_uv, previous_upscale_size());
        history_color = rgb_to_ycocg(h.rgb * delta_pre_exposure() * exposure());
        lock = h.w;
    }

    // ---- UpdateLockStatus ----
    let buffer_index = u32(px.y) * u32(out_size.x) + u32(px.x);
    lock = lock * f32(!is_new_sample);
    let lifetime_decrease_factor = max(shading_change, max(reactive, disocclusion));
    lock = max(0.0, lock - lifetime_decrease_factor * LOCK_MAX);
    let lock_contribution = saturate(saturate(lock - LOCK_THRESHOLD) * (LOCK_MAX - LOCK_THRESHOLD));
    let new_lock_intensity = new_locks[buffer_index] * (1.0 - max(shading_change * 0.0, reactive));
    lock = max(0.0, min(lock + new_lock_intensity, LOCK_MAX));
    let lifetime_decrease = (0.1 / frame.tune.z) * (1.0 - lifetime_decrease_factor);
    lock = max(0.0, lock - lifetime_decrease);
    lock = lock * f32(is_uv_inside(hr_uv - motion));

    // ---- ComputeBaseAccumulationWeight ----
    var history_weight = min(
        accumulation,
        mix(accumulation, 0.15, saturate(max(0.0, velocity_4k * frame.tune.w) / 0.5)),
    );

    // ---- ComputeUpsampledColorAndWeight ----
    let dst_output_pos = vec2<f32>(px) + vec2<f32>(0.5);
    let src_output_pos = dst_output_pos * downscale_factor();
    let src_input_pos = vec2<i32>(floor(src_output_pos));
    let src_unjittered_pos = vec2<f32>(src_input_pos) + vec2<f32>(0.5) - jitter();
    let base_sample_offset = src_unjittered_pos - src_output_pos;
    let flip_col = src_unjittered_pos.x > src_output_pos.x;
    let flip_row = src_unjittered_pos.y > src_output_pos.y;
    let offset_tl = vec2<i32>(select(-1, -2, flip_col), select(-1, -2, flip_row));
    let is_initial_sample = accumulation == 0.0;

    let kernel_bias_max = min(1.99, 1.0 / downscale_factor().x);
    let kernel_bias_min = max(1.0, (1.0 + kernel_bias_max) * 0.3);
    let kernel_bias_weight = min(1.0 - disocclusion * 0.5, min(1.0 - shading_change, saturate(history_weight * 5.0)));
    let kernel_bias = mix(kernel_bias_min, kernel_bias_max, kernel_bias_weight);

    var upsampled_color = vec3<f32>(0.0);
    var upsampled_weight = 0.0;
    var box_min = vec3<f32>(0.0);
    var box_max = vec3<f32>(0.0);
    var box_center = vec3<f32>(0.0);
    var box_vec = vec3<f32>(0.0);
    var box_weight = 0.0;
    // The on-screen sample nearest the output pixel (a restarting pixel takes it, ADR-7678).
    var nearest_color = vec3<f32>(0.0);
    var nearest_d2 = FP32_MAX;
    for (var row: i32 = 0; row < 3; row = row + 1) {
        for (var col: i32 = 0; col < 3; col = col + 1) {
            let col_row = vec2<i32>(select(col, 3 - col, flip_col), select(row, 3 - row, flip_row));
            let src_sample_pos = src_input_pos + offset_tl + col_row;
            var s = load_prepared_color(clamp_load(src_sample_pos, vec2<i32>(0), rsize));
            if (is_initial_sample) {
                // YCoCg → RGB → tonemap → YCoCg (an RGB tonemap avoids desaturation).
                s = rgb_to_ycocg(tonemap(ycocg_to_rgb(s)));
            }
            let src_sample_offset = base_sample_offset + vec2<f32>(offset_tl + col_row);
            let on_screen = f32(is_on_screen(src_sample_pos, rsize));
            if (!is_initial_sample) {
                let biased = src_sample_offset * kernel_bias;
                let w = on_screen * lanczos2_approx_sq(dot(biased, biased));
                upsampled_color = upsampled_color + s * w;
                upsampled_weight = upsampled_weight + w;
            }
            let d2 = dot(src_sample_offset, src_sample_offset);
            if (on_screen > 0.0 && d2 < nearest_d2) {
                nearest_d2 = d2;
                nearest_color = s;
            }
            // Rectification box.
            let bw = exp(-2.3 * dot(src_sample_offset, src_sample_offset)) * on_screen;
            if (row == 0 && col == 0) {
                box_min = s;
                box_max = s;
                box_center = s * bw;
                box_vec = s * s * bw;
                box_weight = bw;
            } else {
                box_min = min(box_min, s);
                box_max = max(box_max, s);
                box_center = box_center + s * bw;
                box_vec = box_vec + s * s * bw;
                box_weight = box_weight + bw;
            }
        }
    }
    // RectificationBoxComputeVarianceBoxData.
    box_weight = select(1.0, box_weight, abs(box_weight) > FP32_MIN);
    box_center = box_center / box_weight;
    box_vec = sqrt(abs(box_vec / box_weight - box_center * box_center));

    upsampled_weight = upsampled_weight * f32(upsampled_weight > EPSILON);
    if (upsampled_weight > EPSILON) {
        upsampled_color = upsampled_color / upsampled_weight;
        upsampled_weight = upsampled_weight * AVERAGE_LANCZOS_WEIGHT_PER_FRAME;
        // Deringing.
        upsampled_color = clamp(upsampled_color, box_min, box_max);
    }
    if (is_initial_sample) {
        // Regia Engine (ADR-7678): a restarting pixel takes its nearest sample, not the source's
        // Gaussian mean of the 3×3. The mean borrows the neighbours' light, and at an edge that
        // borrowed light outlived the accumulation ramp under the wide box of an accumulated pixel.
        upsampled_color = rgb_to_ycocg(inverse_tonemap(ycocg_to_rgb(nearest_color)));
        upsampled_weight = 1.0;
        history_weight = 0.0;
    }

    // ---- RectifyHistory ----
    {
        let velocity_factor = saturate(velocity_4k / 20.0);
        let distance_factor = saturate(0.75 - farthest_depth_in_meters / 20.0);
        let accumulation_factor = 1.0 - accumulation;
        let reactive_factor = pow(reactive, 0.5);
        let t = max(velocity_factor, max(distance_factor, max(accumulation_factor, max(reactive_factor, shading_change))));
        let box_scale = mix(3.0, 1.0, t);
        let scaled_box_vec = box_vec * vec3<f32>(1.7, 1.0, 1.0) * box_scale;
        let clamped_scaled_box_vec = max(scaled_box_vec, vec3<f32>(1.193e-7));
        let transformed = (history_color - box_center) / clamped_scaled_box_vec;
        if (length(transformed) > 1.0) {
            let clamped_history = normalize(transformed) * scaled_box_vec + box_center;
            let contribution = max(luma_instability, lock_contribution) * accumulation * (1.0 - disocclusion);
            history_color = mix(clamped_history, history_color, saturate(contribution));
        }
    }

    // ---- Accumulate ----
    history_weight = history_weight * f32(history_weight > FP16_MIN);
    history_weight = max(EPSILON, history_weight + upsampled_weight);
    let up_tm = rgb_to_ycocg(tonemap(ycocg_to_rgb(upsampled_color)));
    let hist_tm = rgb_to_ycocg(tonemap(ycocg_to_rgb(history_color)));
    let alpha = saturate(upsampled_weight / history_weight);
    var color = inverse_tonemap(ycocg_to_rgb(mix(hist_tm, up_tm, alpha)));
    color = max(finite_or_zero(color / exposure()), vec3<f32>(0.0));
    color = min(color, vec3<f32>(FP16_MAX));

    textureStore(out_history, px, vec4<f32>(color, lock));
    new_locks[buffer_index] = 0.0;

    let debug_view = u32(frame.tune2.w);
    if (debug_view != 0u) {
        var v = 0.0;
        switch debug_view {
            case 1u: { v = accumulation; }
            case 2u: { v = lock / LOCK_MAX; }
            case 3u: { v = disocclusion; }
            case 4u: { v = shading_change; }
            case 5u: { v = luma_instability; }
            default: { v = reactive; }
        }
        textureStore(out_color, px, vec4<f32>(vec3<f32>(v) / exposure(), 1.0));
    } else if (frame.tune2.z <= 0.0) {
        textureStore(out_color, px, vec4<f32>(color, 1.0));
    }
}
