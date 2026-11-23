// SPDX-License-Identifier: MIT
// Frame generation: the game motion vector field of the interpolated frame.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_game_motion_vector_field.h,
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

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var dilated: texture_2d<f32>;
@group(0) @binding(2) var previous_color: texture_2d<f32>;
@group(0) @binding(3) var current_color: texture_2d<f32>;
@group(0) @binding(4) var linear_clamp: sampler;
@group(0) @binding(5) var<storage, read_write> game_x: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> game_y: array<atomic<u32>>;

fn priority_from_view_depth(view_depth: f32) -> u32 {
    let d = pow(max(view_depth, 0.0), 0.33);
    let factor = u32((1.0 - d * (1.0 / (1.0 + d))) * f32(PRIORITY_HIGH_MAX));
    return max(1u, factor);
}

fn field_index(pos: vec2<i32>) -> u32 {
    return u32(pos.y) * u32(frame.render_size.x) + u32(pos.x);
}

// Phase 0 publishes the X word (priority and biased motion x). Phase 1 is a later dispatch,
// after every phase-0 `atomicMax` is visible, and publishes the Y word of the entries that own
// that X word. One `atomicMax` per component would tear an equal-priority tie into a vector
// nobody wrote (audit-g-render3-correctness 1).
fn publish_entry(phase: u32, index: u32, packed_entry: vec2<u32>) {
    if (phase == 0u) {
        atomicMax(&game_x[index], packed_entry.x);
    } else if (packed_entry.x == atomicLoad(&game_x[index])) {
        atomicMax(&game_y[index], packed_entry.y);
    }
}

// `UpdateGameMotionVectorFieldEx`, whole vector: see `publish_entry`.
fn update_field(phase: u32, pos: vec2<i32>, packed_entry: vec2<u32>) {
    publish_entry(phase, field_index(pos), packed_entry);
}

fn publish(phase: u32, gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = render_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw;
    let sample = textureLoad(dilated, px, 0);
    let motion = sample.xy;
    let motion_half = motion * 0.5;
    let interpolated_uv = uv + motion_half;
    let high_primary = priority_from_view_depth(device_to_view_depth(sample.z));

    // Pixel in the current frame + vector = pixel in the previous frame.
    let prev_col = textureSampleLevel(previous_color, linear_clamp, uv + motion, 0.0).rgb;
    let cur_col = textureSampleLevel(current_color, linear_clamp, uv, 0.0).rgb;
    let prev_luma = 0.001 + raw_to_luminance(prev_col);
    let cur_luma = 0.001 + raw_to_luminance(cur_col);
    let low_primary = u32(round(min_divided_by_max(prev_luma, cur_luma) * f32(PRIORITY_LOW_MAX)))
        * u32(is_uv_inside(uv + motion));

    // Primary vectors go to all four texels of the interpolated position.
    let primary = pack_entry(true, high_primary, low_primary, motion_half);
    let base = bilinear_base(interpolated_uv, size);
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            publish_entry(phase, field_index(pos), primary);
        }
    }

    // Secondary vectors walk back along the motion: they improve the inpainted vectors.
    // The walk always reaches break_dist. Stopping on the pre-image atomicMax returns
    // would make the stored words depend on the order other invocations of this dispatch
    // land their primary stores (audit-g-render3-determinism 1): two runs of the same
    // frame could store different secondary words past the early-out. The primary bit
    // dominates every secondary, so primaries that land later still replace the
    // secondaries this walk leaves behind.
    if (length(motion_half * frame.render_size.xy) > EPSILON) {
        let step_scale = length(frame.render_size.zw);
        let step_mv = normalize(motion);
        let break_dist = min(length(motion_half), length(vec2<f32>(0.5)));
        let high_secondary = max(1u, PRIORITY_HIGH_MAX - high_primary);
        var scale = step_scale;
        loop {
            if (scale > break_dist) {
                break;
            }
            let location = interpolated_uv - step_mv * scale;
            let pos = bilinear_base(location, size);
            if (!is_on_screen(pos, size)) {
                break;
            }
            // At the exact centre there is no direction to it: no bonus, instead of the NaN of
            // `normalize(0)` (audit-v035-wgsl 5).
            let center_offset = vec2<f32>(0.5) - location;
            let to_center = select(vec2<f32>(0.0), normalize(center_offset), dot(center_offset, center_offset) > 0.0);
            let low = u32(max(0.0, dot(to_center, step_mv)) * f32(PRIORITY_LOW_MAX));
            update_field(phase, pos, pack_entry(false, high_secondary, low, motion_half));
            scale = scale + step_scale;
        }
    }
}

@compute @workgroup_size(8, 8, 1)
fn cs_publish_x(@builtin(global_invocation_id) gid: vec3<u32>) {
    publish(0u, gid);
}

@compute @workgroup_size(8, 8, 1)
fn cs_publish_y(@builtin(global_invocation_id) gid: vec3<u32>) {
    publish(1u, gid);
}
