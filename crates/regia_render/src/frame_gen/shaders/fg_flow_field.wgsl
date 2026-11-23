// SPDX-License-Identifier: MIT
// Frame generation: the optical flow vector field of the interpolated frame.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_optical_flow_vector_field.h,
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
// Regia Engine changes (ADR-2067): the guards on `pow` of a negative dot and on an empty 3 x 3
// (the HLSL relies on `max(0, NaN)`). X and Y are published in two dispatches so an equal-priority
// tie keeps one whole vector (audit-g-render3-correctness 1).

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var<storage, read> flow: array<vec2<i32>>;
@group(0) @binding(2) var previous_color: texture_2d<f32>;
@group(0) @binding(3) var current_color: texture_2d<f32>;
@group(0) @binding(4) var linear_clamp: sampler;
@group(0) @binding(5) var<storage, read_write> flow_x: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> flow_y: array<atomic<u32>>;

// The vector of a flow block in UV units; outside the field, none.
fn load_flow(pos: vec2<i32>) -> vec2<f32> {
    let size = flow_size_i();
    if (!is_on_screen(pos, size)) {
        return vec2<f32>(0.0);
    }
    return vec2<f32>(flow[u32(pos.y) * u32(size.x) + u32(pos.x)]) * frame.flow_size.zw;
}

fn length_factor(vs: vec2<f32>) -> f32 {
    let len = length(vs * frame.display_size.xy);
    return max(0.0, 512.0 - len) * f32(len > 1.0);
}

// Same whole-vector publish as `fg_game_field.wgsl`: phase 0 is the X word, phase 1 (a later
// dispatch) is the Y word of whoever owns that X word.
fn publish_entry(phase: u32, index: u32, packed_entry: vec2<u32>) {
    if (phase == 0u) {
        atomicMax(&flow_x[index], packed_entry.x);
    } else if (packed_entry.x == atomicLoad(&flow_x[index])) {
        atomicMax(&flow_y[index], packed_entry.y);
    }
}

fn publish(phase: u32, gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = flow_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    var average = vec2<f32>(0.0);
    var sum = 0.0;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let vs = load_flow(px + vec2<i32>(x, y));
            let w = length_factor(vs);
            average = average + vs * w;
            sum = sum + w;
        }
    }
    if (sum <= 0.0) {
        return;
    }
    average = average / sum;

    var vector = vec2<f32>(0.0);
    sum = 0.0;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let vs = load_flow(px + vec2<i32>(x, y));
            let w = pow(max(dot(average, vs), 0.0), 1.25) * length_factor(vs);
            vector = vector + vs * w;
            sum = sum + w;
        }
    }
    if (sum > EPSILON) {
        vector = vector / sum;
    }

    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) / frame.flow_size.xy;
    let velocity = length(vector * frame.display_size.xy);
    let high = u32(velocity > 1.0)
        * u32(saturate(velocity / length(frame.display_size.xy * 0.05)) * f32(PRIORITY_HIGH_MAX));
    if (high == 0u) {
        return;
    }
    let prev_col = textureSampleLevel(previous_color, linear_clamp, uv + vector, 0.0).rgb;
    let cur_col = textureSampleLevel(current_color, linear_clamp, uv, 0.0).rgb;
    let prev_luma = 0.001 + raw_to_luminance(prev_col);
    let cur_luma = 0.001 + raw_to_luminance(cur_col);
    let low = u32(round(min_divided_by_max(prev_luma, cur_luma) * f32(PRIORITY_LOW_MAX)))
        * u32(is_uv_inside(uv + vector));
    let half_vector = vector * 0.5;
    let packed_entry = pack_entry(true, high, low, half_vector);
    let target_uv = uv + half_vector;
    let base = bilinear_base(target_uv, size);
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            let index = u32(pos.y) * u32(size.x) + u32(pos.x);
            publish_entry(phase, index, packed_entry);
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
