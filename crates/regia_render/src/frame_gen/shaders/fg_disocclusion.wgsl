// SPDX-License-Identifier: MIT
// Frame generation: the two-sided disocclusion mask.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_disocclusion_mask.h,
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
// Regia Engine changes (ADR-2067): the distortion field is gone, and the source's unused locals.

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var dilated: texture_2d<f32>;
@group(0) @binding(2) var<storage, read> reconstructed_previous: array<u32>;
@group(0) @binding(3) var<storage, read> reconstructed_interpolated: array<u32>;
@group(0) @binding(4) var<storage, read> game_x: array<u32>;
@group(0) @binding(5) var<storage, read> game_y: array<u32>;
@group(0) @binding(6) var pyramid: texture_2d<f32>;
@group(0) @binding(7) var out_mask: texture_storage_2d<rgba8unorm, write>;

fn stored_depth(bits: u32) -> f32 {
    return 1.0 - bitcast<f32>(bits);
}

fn load_estimated_depth(which: u32, pos: vec2<i32>) -> f32 {
    if (which == 0u) {
        return stored_depth(reconstructed_previous[u32(pos.y) * u32(frame.render_size.x) + u32(pos.x)]);
    }
    return textureLoad(dilated, pos, 0).z;
}

fn view_space_position(pos: vec2<i32>, size: vec2<i32>, device_depth: f32) -> vec3<f32> {
    let z = device_to_view_depth(device_depth);
    let ndc = vec2<f32>(pos) / vec2<f32>(size) * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0);
    return vec3<f32>(frame.depth.z * ndc.x * z, frame.depth.w * ndc.y * z, z);
}

// `ComputeDepthClip`: how much of `uv`'s footprint shows a surface behind the current one.
fn depth_clip(which: u32, uv: vec2<f32>, current_depth: f32) -> f32 {
    let size = render_size_i();
    let current_view = device_to_view_depth(current_depth);
    let base = bilinear_base(uv, size);
    var clipped = 0.0;
    var weight_sum = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        let weight = bilinear_weight(uv, size, i);
        if (is_on_screen(pos, size) && weight > BILINEAR_WEIGHT_THRESHOLD) {
            let previous_depth = load_estimated_depth(which, pos);
            let previous_view = device_to_view_depth(previous_depth);
            let diff = current_view - previous_view;
            if (diff > 0.0) {
                let plane = max(previous_depth, current_depth);
                let center = view_space_position(size / 2, size, plane);
                let corner = view_space_position(vec2<i32>(0, 0), size, plane);
                let half_width = length(frame.render_size.xy);
                let threshold = min(current_view, previous_view);
                let ksep = 1.37e-05;
                let kfov = length(corner) / length(center);
                let required = ksep * kfov * half_width * threshold;
                clipped = clipped + f32(required / diff >= 1.0) * weight;
                weight_sum = weight_sum + weight;
            }
        }
    }
    return select(0.0, saturate(1.0 - clipped / weight_sum), weight_sum > 0.0);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = render_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    let depth = stored_depth(reconstructed_interpolated[u32(px.y) * u32(size.x) + u32(px.x)]);
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw;
    let game = load_inpainted_game_mv(uv);
    let to_previous = 1.0 - depth_clip(0u, uv + game.mv, depth);
    let to_current = 1.0 - depth_clip(1u, uv - game.mv, depth);
    var mask = vec2<f32>(f32(to_previous >= EPSILON), f32(to_current >= EPSILON));

    // No false disocclusion where the primary vector points outside the screen.
    let src = game.mv * 2.0;
    let prev_px = vec2<i32>((uv + src) * frame.render_size.xy);
    mask.x = saturate(mask.x + f32(!is_on_screen(prev_px, size)));
    let cur_px = vec2<i32>((uv - src) * frame.render_size.xy);
    mask.y = saturate(mask.y + f32(!is_on_screen(cur_px, size)));
    textureStore(out_mask, px, vec4<f32>(mask, 0.0, 0.0));
}
