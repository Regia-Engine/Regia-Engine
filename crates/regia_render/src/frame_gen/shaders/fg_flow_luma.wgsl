// SPDX-License-Identifier: MIT
// Frame generation optical flow, luma: the pyramid and the scene-change histograms.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_prepare_luma.h,
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_compute_luminance_pyramid.h,
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_generate_scd_histogram.h,
// sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_compute_scd_divergence.h,
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
// Regia Engine changes (ADR-2067, ADR-5162): the levels live in one storage buffer, four luma
// bytes per word, little-endian, each row padded to a multiple of four pixels, and follow
// `optical_flow::LumaImage` numerically (rounded mean of the four clamped taps, sizes rounded
// up), so the field equals the CPU reference vector for vector. The histograms are the
// reference's too (16 bins per 3 x 3 section over one pyramid level, a change needs 5 sections
// over 0.5) instead of the SDK's stochastic 256-bin KL divergence: `optical_flow.rs` documents
// them. The change history and its four-frame hold are the SDK's.

struct LumaParams {
    src_size: vec2<u32>,
    dst_size: vec2<u32>,
    src_offset: u32,
    dst_offset: u32,
    // 0 = sRGB-encoded, 1 = PQ, 2 = scRGB (`TransferFunction`).
    transfer: u32,
    // Writes the current frame's half of the histogram buffer: 0 or 144.
    histogram_base: u32,
    // PQ signal of the display's minimum and maximum luminance.
    pq_range: vec2<f32>,
    // 1 when the previous histograms belong to the frame before this one.
    has_history: u32,
    // 1 when the caller flagged a reset (a camera cut).
    reset: u32,
}

struct FgState {
    // Bit 0 = this frame changed scene, older frames above it.
    scene_history: u32,
    frames_since_reset: u32,
}

@group(0) @binding(0) var<uniform> params: LumaParams;
@group(0) @binding(1) var color_tex: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> luma: array<u32>;
@group(0) @binding(3) var<storage, read_write> histograms: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> state: FgState;

// SMPTE ST 2084 inverse EOTF for luminance in cd/m² (`optical_flow::pq_encode`).
fn pq_encode(nits: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 4096.0 * 128.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 4096.0 * 32.0;
    let c3 = 2392.0 / 4096.0 * 32.0;
    let y = pow(min(max(nits, 0.0) / 10000.0, 1.0), m1);
    return pow((c1 + c2 * y) / (1.0 + c3 * y), m2);
}

fn stretch(signal: f32) -> f32 {
    let lo = params.pq_range.x;
    let hi = params.pq_range.y;
    return select(signal, (signal - lo) / (hi - lo), hi > lo);
}

// `optical_flow::flow_luma_u8`.
fn flow_luma_u8(rgb: vec3<f32>) -> u32 {
    var l = 0.0;
    switch params.transfer {
        case 1u: { l = stretch(dot(rgb, vec3<f32>(0.2627, 0.6780, 0.0593))); }
        case 2u: { l = stretch(pq_encode(dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722)) * 80.0)); }
        default: { l = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722)); }
    }
    // A NaN or infinity fails this test and reads as black.
    if (!(l >= 0.0)) {
        l = 0.0;
    }
    return u32(floor(min(l, 1.0) * 255.0 + 0.5));
}

fn row_stride(width: u32) -> u32 {
    return (width + 3u) / 4u * 4u;
}

fn load_luma(offset: u32, stride: u32, x: u32, y: u32) -> u32 {
    let index = offset + y * stride + x;
    let word = luma[index >> 2u];
    return (word >> ((index & 3u) * 8u)) & 0xffu;
}

// One thread owns the whole word. The other three lanes of the word return.
fn owns_word(x: u32) -> bool {
    return (x & 3u) == 0u;
}

fn store_word(offset: u32, stride: u32, x: u32, y: u32, b0: u32, b1: u32, b2: u32, b3: u32) {
    let index = offset + y * stride + x;
    luma[index >> 2u] = (b0 & 0xffu) | ((b1 & 0xffu) << 8u) | ((b2 & 0xffu) << 16u) | ((b3 & 0xffu) << 24u);
}

fn base_byte(x: u32, y: u32) -> u32 {
    if (x >= params.dst_size.x) {
        return 0u;
    }
    let rgb = textureLoad(color_tex, vec2<i32>(i32(x), i32(y)), 0).rgb;
    return flow_luma_u8(rgb);
}

// Level 0 from the presented colour. The thread at `x % 4 == 0` writes up to four pixels.
@compute @workgroup_size(8, 8, 1)
fn cs_base(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!owns_word(gid.x) || gid.x >= params.dst_size.x || gid.y >= params.dst_size.y) {
        return;
    }
    let stride = row_stride(params.dst_size.x);
    store_word(
        params.dst_offset,
        stride,
        gid.x,
        gid.y,
        base_byte(gid.x, gid.y),
        base_byte(gid.x + 1u, gid.y),
        base_byte(gid.x + 2u, gid.y),
        base_byte(gid.x + 3u, gid.y),
    );
}

fn src_clamped(x: u32, y: u32) -> u32 {
    let cx = min(x, params.src_size.x - 1u);
    let cy = min(y, params.src_size.y - 1u);
    return load_luma(params.src_offset, row_stride(params.src_size.x), cx, cy);
}

fn down_byte(x: u32, y: u32) -> u32 {
    if (x >= params.dst_size.x) {
        return 0u;
    }
    let sx = 2u * x;
    let sy = 2u * y;
    let sum = src_clamped(sx, sy) + src_clamped(sx + 1u, sy) + src_clamped(sx, sy + 1u)
        + src_clamped(sx + 1u, sy + 1u);
    return (sum + 2u) / 4u;
}

// The next level: `LumaImage::downsample`. Same word owner as `cs_base`.
@compute @workgroup_size(8, 8, 1)
fn cs_down(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!owns_word(gid.x) || gid.x >= params.dst_size.x || gid.y >= params.dst_size.y) {
        return;
    }
    let stride = row_stride(params.dst_size.x);
    store_word(
        params.dst_offset,
        stride,
        gid.x,
        gid.y,
        down_byte(gid.x, gid.y),
        down_byte(gid.x + 1u, gid.y),
        down_byte(gid.x + 2u, gid.y),
        down_byte(gid.x + 3u, gid.y),
    );
}

// `optical_flow::section_histograms` over the level in `src_*`.
@compute @workgroup_size(8, 8, 1)
fn cs_histogram(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.src_size.x || gid.y >= params.src_size.y) {
        return;
    }
    let value = load_luma(params.src_offset, row_stride(params.src_size.x), gid.x, gid.y);
    let sx = gid.x * 3u / params.src_size.x;
    let sy = gid.y * 3u / params.src_size.y;
    let bin = value * 16u / 256u;
    atomicAdd(&histograms[params.histogram_base + (sy * 3u + sx) * 16u + bin], 1u);
}

// `optical_flow::SceneChangeDetector::update`, then the SDK's history and frame counter
// (`ffx_frameinterpolation_setup.h`). One invocation.
@compute @workgroup_size(1, 1, 1)
fn cs_compare() {
    let previous_base = 144u - params.histogram_base;
    var changed = 0u;
    if (params.has_history == 1u) {
        for (var section = 0u; section < 9u; section = section + 1u) {
            var total_a = 0u;
            var total_b = 0u;
            var diff = 0u;
            for (var bin = 0u; bin < 16u; bin = bin + 1u) {
                let a = atomicLoad(&histograms[previous_base + section * 16u + bin]);
                let b = atomicLoad(&histograms[params.histogram_base + section * 16u + bin]);
                total_a = total_a + a;
                total_b = total_b + b;
                diff = diff + max(a, b) - min(a, b);
            }
            let total = max(max(total_a, total_b), 1u);
            if (f32(diff) / (2.0 * f32(total)) > 0.5) {
                changed = changed + 1u;
            }
        }
    }
    let change = select(0u, 1u, changed >= 5u);
    state.scene_history = (state.scene_history << 1u) | change;
    // The SDK holds a detected change for four frames.
    if (params.reset == 1u || (state.scene_history & 0xfu) != 0u) {
        state.frames_since_reset = 0u;
    } else {
        state.frames_since_reset = state.frames_since_reset + 1u;
    }
}
