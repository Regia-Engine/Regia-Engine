// SPDX-License-Identifier: MIT
// Frame generation: the interpolated colour.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation.h,
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_common.h,
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
// Regia Engine changes (ADR-2067): the interpolation covers the whole display (no rectangle). The
// reactive mask, which the source does not take, lowers the trust in the game vector: a reactive
// texel (transparency, particles) has no vector of its own.

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var<storage, read> state: FgState;
@group(0) @binding(2) var previous_color: texture_2d<f32>;
@group(0) @binding(3) var current_color: texture_2d<f32>;
@group(0) @binding(4) var<storage, read> game_x: array<u32>;
@group(0) @binding(5) var<storage, read> game_y: array<u32>;
@group(0) @binding(6) var<storage, read> flow_x: array<u32>;
@group(0) @binding(7) var<storage, read> flow_y: array<u32>;
@group(0) @binding(8) var pyramid: texture_2d<f32>;
@group(0) @binding(9) var disocclusion: texture_2d<f32>;
@group(0) @binding(10) var reactive: texture_2d<f32>;
@group(0) @binding(11) var linear_clamp: sampler;
// rgb = the source colours' encoding, a = inpainting weight.
@group(0) @binding(12) var out_color: texture_storage_2d<rgba16float, write>;

// `SampleOpticalFlowMotionVectorField`, the vector alone.
fn sample_flow_vector(uv: vec2<f32>) -> vec2<f32> {
    let size = flow_size_i();
    let base = bilinear_base(uv, size);
    var mv = vec2<f32>(0.0);
    var sum = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            let w = bilinear_weight(uv, size, i);
            let index = u32(pos.y) * u32(size.x) + u32(pos.x);
            let entry = unpack_entry(vec2<u32>(flow_x[index], flow_y[index]));
            mv = mv + entry.mv * w;
            sum = sum + w;
        }
    }
    return mv / select(1.0, sum, sum > 0.0);
}

// `SampleTextureBilinear`: rgb, and the summed weight of the texels inside the image in `a`.
fn sample_source(is_current: bool, uv: vec2<f32>) -> vec4<f32> {
    let size = display_size_i();
    let base = bilinear_base(uv, size);
    var color = vec3<f32>(0.0);
    var sum = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            let w = bilinear_weight(uv, size, i);
            if (is_current) {
                color = color + textureLoad(current_color, pos, 0).rgb * w;
            } else {
                color = color + textureLoad(previous_color, pos, 0).rgb * w;
            }
            sum = sum + w;
        }
    }
    return vec4<f32>(color / select(1.0, sum, sum != 0.0), sum);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    if (!is_on_screen(px, display_size_i())) {
        return;
    }
    // Just reset: nothing to interpolate from, copy the current frame.
    if (state.frames_since_reset == 0u) {
        textureStore(out_color, px, vec4<f32>(textureLoad(current_color, px, 0).rgb, 0.0));
        return;
    }
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.display_size.zw;
    let game = load_inpainted_game_mv(uv);
    let flow_mv = sample_flow_vector(uv);

    // Binarize the disocclusion factors.
    let mask = saturate(textureSampleLevel(disocclusion, linear_clamp, uv, 0.0).xy);
    var factor = vec2<f32>(f32(mask.x == 1.0), f32(mask.y == 1.0));

    let prev_game = sample_source(false, uv + game.mv);
    let cur_game = sample_source(true, uv - game.mv);
    let prev_flow = sample_source(false, uv + flow_mv);
    let cur_flow = sample_source(true, uv - flow_mv);

    // Disocclusion logic. `factor.x` of 1: the interpolated position is on screen and visible in
    // the previous frame; `factor.y`: in the current frame.
    factor.x = factor.x * f32(!game.pos_outside);
    factor.y = factor.y * f32(!game.neg_outside);
    // Inpaint where neither frame shows the surface.
    var inpaint = saturate(f32(length(factor) <= EPSILON));
    let t = saturate(0.5 + 0.5 * (1.0 - factor.x) - 0.5 * (1.0 - factor.y));
    var color = mix(prev_game.rgb, cur_game.rgb, t);
    let disoccluded = saturate(1.0 - min(factor.x, factor.y));
    if (prev_game.a == 0.0) {
        color = cur_game.rgb;
    } else if (cur_game.a == 0.0) {
        color = prev_game.rgb;
    }
    if (prev_game.a == 0.0 && cur_game.a == 0.0) {
        inpaint = 1.0;
    }

    // Game vector or optical flow: whichever makes the two source colours agree.
    var flow_t = 1.0;
    if (prev_flow.a > 0.0 && cur_flow.a > 0.0) {
        flow_t = 0.5;
    } else if (prev_flow.a > 0.0) {
        flow_t = 0.0;
    }
    let flow_color = mix(prev_flow.rgb, cur_flow.rgb, flow_t);
    let flow_sim = normalized_dot3(prev_flow.rgb, cur_flow.rgb);
    var game_sim = normalized_dot3(prev_game.rgb, cur_game.rgb);
    game_sim = mix(max(EPSILON, game_sim), 1.0, disoccluded);
    var bias = saturate(game_sim / max(EPSILON, flow_sim));
    bias = mix(bias, 1.0, f32(state.frames_since_reset < 10u));
    if (frame.tune.w > 0.5) {
        bias = bias * (1.0 - saturate(textureSampleLevel(reactive, linear_clamp, uv, 0.0).r));
    }
    color = mix(flow_color, color, saturate(bias));
    textureStore(out_color, px, vec4<f32>(color, inpaint));
}
