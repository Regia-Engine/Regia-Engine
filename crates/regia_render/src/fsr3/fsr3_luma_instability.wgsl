// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 5: luma instability (render resolution).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_luma_instability.h, tag v1.1.4 only.
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
// Regia Engine changes: the farthest-depth sample the source takes and never uses is dropped;
// the instability factor is stored RGBA8 (FSR: an R16F intermediate). The luma history is
// kept as written by the source (zeros where instability is not evaluated).

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var motion_tex: texture_2d<f32>;
@group(0) @binding(2) var masks_tex: texture_2d<f32>;
@group(0) @binding(3) var luma_tex: texture_2d<f32>;
@group(0) @binding(4) var previous_luma_history_tex: texture_2d<f32>;
@group(0) @binding(5) var linear_clamp: sampler;
@group(0) @binding(6) var out_luma_history: texture_storage_2d<rgba16float, write>;
@group(0) @binding(7) var out_instability: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = irender_size();
    if (!is_on_screen(px, size)) {
        return;
    }
    var history = vec4<f32>(0.0);
    var instability = 0.0;

    let motion = textureLoad(motion_tex, px, 0).xy;
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw;
    let uv_current_jittered = uv + jitter() * frame.render_size.zw;
    let uv_previous_jittered = uv + previous_jitter() / previous_render_size();
    let reprojected = uv_previous_jittered + motion;

    if (is_uv_inside(reprojected) && frame_index() > 0.5) {
        let uv_hw = clamp_uv(uv_current_jittered, size, size);
        let masks = textureSampleLevel(masks_tex, linear_clamp, uv_hw, 0.0);
        let reactive = saturate(masks.x);
        let disocclusion = saturate(masks.y);
        let shading_change = saturate(masks.z);
        let accumulation = saturate(masks.w);

        if (accumulation > 0.9) {
            let current = textureSampleLevel(luma_tex, linear_clamp, uv_hw, 0.0).x * exposure();
            let previous_size = vec2<i32>(previous_render_size());
            let reprojected_hw = clamp_uv(reprojected, previous_size, previous_size);
            history = textureSampleLevel(previous_luma_history_tex, linear_clamp, reprojected_hw, 0.0)
                * delta_pre_exposure() * exposure();

            // ComputeLumaInstabilityFactor (N−1 .. N−4 in x .. w).
            let diff0 = current - history.x;
            let similarity0 = min_divided_by_max_or(current, history.x, 1.0);
            var max_similarity = similarity0;
            var luma_instability = 0.0;
            if (similarity0 < 1.0) {
                for (var i: i32 = 1; i <= 3; i = i + 1) {
                    let h = history[i];
                    let diff1 = current - h;
                    let similarity1 = min_divided_by_max(current, h);
                    if (sign(diff0) == sign(diff1)) {
                        max_similarity = max(max_similarity, similarity1);
                    }
                }
                luma_instability = f32(max_similarity > similarity0);
            }
            // Shift the history and store it without exposure.
            history = vec4<f32>(current, history.x, history.y, history.z) / exposure();
            instability = luma_instability * f32(history.w != 0.0);

            let velocity_weight = 1.0 - saturate(get_4k_velocity(motion) / 20.0);
            instability = instability * velocity_weight * (1.0 - disocclusion) * (1.0 - reactive)
                * (1.0 - shading_change);
        }
    }

    textureStore(out_luma_history, px, min(history, vec4<f32>(FP16_MAX)));
    textureStore(out_instability, px, vec4<f32>(instability, 0.0, 0.0, 0.0));
}
