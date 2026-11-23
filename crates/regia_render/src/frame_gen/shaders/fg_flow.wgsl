// SPDX-License-Identifier: MIT
// Frame generation optical flow: block search, middle-vector filter, 2x upscale.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_compute_optical_flow_v5.h,
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_filter_optical_flow_v5.h,
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_scale_optical_flow_advanced_v5.h,
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
// Regia Engine changes (ADR-2067, ADR-5162): WGSL has neither `msad4` nor wave operations, so one
// invocation searches one block over a plain loop instead of 64 lanes sharing a SAD map. The rules
// are `optical_flow.rs`'s, so the field equals the CPU reference vector for vector: the zero
// correction is scored first and the rest of the +-8 window keeps scan order, ties go to the
// shorter correction (the SDK searches -8..7 and fixes its bias with a flag), a candidate stops at
// the first pixel whose sum passes the best SAD, a middle vector by summed L1 distance (the SDK
// uses L2), and the four-parent upscale. Luma is four bytes per word (ADR-5162).

struct FlowLevel {
    // Pixels of this level, and where they start in the luma buffers.
    size: vec2<u32>,
    luma_offset: u32,
    // 1 on the coarsest level of the run.
    top: u32,
    // Blocks of this level.
    blocks: vec2<u32>,
    // Blocks of the next finer level (the upscale's output).
    fine_blocks: vec2<u32>,
}

struct FgState {
    scene_history: u32,
    frames_since_reset: u32,
}

@group(0) @binding(0) var<uniform> level: FlowLevel;
@group(0) @binding(1) var<storage, read> luma_current: array<u32>;
@group(0) @binding(2) var<storage, read> luma_previous: array<u32>;
@group(0) @binding(3) var<storage, read> input_field: array<vec2<i32>>;
@group(0) @binding(4) var<storage, read_write> output_field: array<vec2<i32>>;
@group(0) @binding(5) var<storage, read> state: FgState;

const BLOCK: i32 = 8;
const RADIUS: i32 = 8;

fn scene_changed() -> bool {
    return (state.scene_history & 0xfu) != 0u;
}

fn unpack_word(word: u32, index: u32) -> u32 {
    return (word >> ((index & 3u) * 8u)) & 0xffu;
}

fn luma_stride() -> u32 {
    return (level.size.x + 3u) / 4u * 4u;
}

// `sad` of `optical_flow.rs`: current pixels outside the image are skipped, previous ones clamped.
// Stops at the first pixel whose running sum passes `limit`: such a candidate cannot win.
fn block_sad(origin: vec2<i32>, offset: vec2<i32>, size: i32, limit: u32) -> u32 {
    let w = i32(level.size.x);
    let h = i32(level.size.y);
    let stride = luma_stride();
    var sum = 0u;
    for (var y = origin.y; y < origin.y + size; y = y + 1) {
        if (y >= h) {
            break;
        }
        let row_c = u32(y) * stride;
        let py = clamp(y + offset.y, 0, h - 1);
        let row_p = u32(py) * stride;
        for (var x = origin.x; x < origin.x + size; x = x + 1) {
            if (x >= w) {
                break;
            }
            let c_index = level.luma_offset + row_c + u32(x);
            let p_index = level.luma_offset + row_p + u32(clamp(x + offset.x, 0, w - 1));
            let c = unpack_word(luma_current[c_index >> 2u], c_index);
            let p = unpack_word(luma_previous[p_index >> 2u], p_index);
            sum = sum + max(c, p) - min(c, p);
            if (sum > limit) {
                return sum;
            }
        }
    }
    return sum;
}

@compute @workgroup_size(8, 8, 1)
fn cs_search(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= level.blocks.x || gid.y >= level.blocks.y) {
        return;
    }
    let index = gid.y * level.blocks.x + gid.x;
    if (scene_changed()) {
        output_field[index] = vec2<i32>(0, 0);
        return;
    }
    var guess = vec2<i32>(0, 0);
    if (level.top == 0u) {
        guess = input_field[index];
    }
    let origin = vec2<i32>(gid.xy) * BLOCK;
    var best_sad = 0xffffffffu;
    var best_len = 0x7fffffff;
    var best = guess;
    // Zero correction first, then the old scan order with that slot skipped.
    let window = RADIUS * 2 + 1;
    let count = window * window;
    let zero = RADIUS * window + RADIUS;
    for (var step = 0; step < count; step = step + 1) {
        var k = zero;
        if (step != 0) {
            k = step - 1;
            if (k >= zero) {
                k = k + 1;
            }
        }
        let dy = k / window - RADIUS;
        let dx = k % window - RADIUS;
        let v = guess + vec2<i32>(dx, dy);
        let sad = block_sad(origin, v, BLOCK, best_sad);
        let len = abs(dx) + abs(dy);
        if (sad < best_sad || (sad == best_sad && len < best_len)) {
            best_sad = sad;
            best_len = len;
            best = v;
        }
    }
    output_field[index] = best;
}

fn filter_cost(v: vec2<i32>, center: vec2<i32>) -> i32 {
    let w = i32(level.blocks.x);
    let h = i32(level.blocks.y);
    var cost = 0;
    for (var ny = max(center.y - 1, 0); ny <= min(center.y + 1, h - 1); ny = ny + 1) {
        for (var nx = max(center.x - 1, 0); nx <= min(center.x + 1, w - 1); nx = nx + 1) {
            let o = input_field[ny * w + nx];
            cost = cost + abs(v.x - o.x) + abs(v.y - o.y);
        }
    }
    return cost;
}

// `middle_filter`: of a block and its neighbours, the vector with the least summed L1 distance to
// the others; the block's own vector wins ties, then the first in scan order.
@compute @workgroup_size(8, 8, 1)
fn cs_filter(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= level.blocks.x || gid.y >= level.blocks.y) {
        return;
    }
    let w = i32(level.blocks.x);
    let h = i32(level.blocks.y);
    let center = vec2<i32>(gid.xy);
    var best = input_field[center.y * w + center.x];
    var best_cost = filter_cost(best, center);
    for (var ny = max(center.y - 1, 0); ny <= min(center.y + 1, h - 1); ny = ny + 1) {
        for (var nx = max(center.x - 1, 0); nx <= min(center.x + 1, w - 1); nx = nx + 1) {
            let v = input_field[ny * w + nx];
            let cost = filter_cost(v, center);
            if (cost < best_cost) {
                best = v;
                best_cost = cost;
            }
        }
    }
    output_field[gid.y * level.blocks.x + gid.x] = best;
}

fn coarse_at(x: i32, y: i32) -> vec2<i32> {
    let w = i32(level.blocks.x);
    let h = i32(level.blocks.y);
    return input_field[clamp(y, 0, h - 1) * w + clamp(x, 0, w - 1)];
}

// `upscale`: of the parent and the three parents next to the quadrant, the vector whose SAD over
// the quadrant (at this coarse level) is least, doubled.
@compute @workgroup_size(8, 8, 1)
fn cs_upscale(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= level.fine_blocks.x || gid.y >= level.fine_blocks.y) {
        return;
    }
    let index = gid.y * level.fine_blocks.x + gid.x;
    if (scene_changed()) {
        output_field[index] = vec2<i32>(0, 0);
        return;
    }
    let f = vec2<i32>(gid.xy);
    let p = min(f / 2, vec2<i32>(level.blocks) - vec2<i32>(1));
    let q = f % 2;
    let s = vec2<i32>(select(1, -1, q.x == 0), select(1, -1, q.y == 0));
    let half_block = BLOCK / 2;
    let origin = p * BLOCK + q * half_block;
    var best = coarse_at(p.x, p.y);
    var best_sad = block_sad(origin, best, half_block, 0xffffffffu);
    for (var i = 1; i < 4; i = i + 1) {
        var v = coarse_at(p.x + s.x, p.y);
        if (i == 2) {
            v = coarse_at(p.x, p.y + s.y);
        } else if (i == 3) {
            v = coarse_at(p.x + s.x, p.y + s.y);
        }
        let sad = block_sad(origin, v, half_block, best_sad);
        if (sad < best_sad) {
            best = v;
            best_sad = sad;
        }
    }
    output_field[index] = best * 2;
}
