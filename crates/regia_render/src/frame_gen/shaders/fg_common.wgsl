// SPDX-License-Identifier: MIT
// Frame generation: uniform and helpers of the interpolation passes.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
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
// Regia Engine changes (ADR-2067): motion is `previous uv - current uv` in UV units, as in the SDK.
// Depth is standard (0 near, 1 far), the SDK's non-inverted path. The vector fields and estimated
// depths are `array<atomic<u32>>` storage buffers: WGSL has no image atomics. A depth buffer holds
// `bitcast<u32>(1 - depth)` updated with `atomicMax`, so a cleared buffer reads as the far plane
// (the SDK clears its image to 1 and uses `atomicMin` on the depth bits). No distortion field, no
// letterbox rectangle: the interpolation covers the whole display.

struct FgFrame {
    // w, h, 1/w, 1/h
    render_size: vec4<f32>,
    // w, h, 1/w, 1/h
    display_size: vec4<f32>,
    // Flow blocks w, h; UV per flow pixel x, y.
    flow_size: vec4<f32>,
    // View depth = y / (depth - x); view x, y per NDC per view depth.
    depth: vec4<f32>,
    // Tan of half the horizontal field of view, min and max luminance, 1 with a reactive mask.
    tune: vec4<f32>,
    // Tear-line color, cycled per frame.
    debug_bar: vec4<f32>,
    // Transfer function, reset, dispatch flags, unused.
    flags: vec4<u32>,
    inv_jittered: mat4x4<f32>,
    cur_vp: mat4x4<f32>,
    prev_vp: mat4x4<f32>,
    velocity_prev_vp: mat4x4<f32>,
}

struct FgState {
    scene_history: u32,
    frames_since_reset: u32,
}

const EPSILON: f32 = 1.0e-3;
const PRIORITY_LOW_MAX: u32 = 31u;
const PRIORITY_HIGH_MAX: u32 = 1023u;
const PRIORITY_LOW_OFFSET: u32 = 16u;
const PRIORITY_HIGH_OFFSET: u32 = 21u;
const PRIMARY_BIT: u32 = 0x80000000u;
const FLAG_TEAR_LINES: u32 = 1u;
const FLAG_RESET_INDICATORS: u32 = 2u;
const FLAG_DEBUG_VIEW: u32 = 4u;
const BILINEAR_WEIGHT_THRESHOLD: f32 = EPSILON;

fn render_size_i() -> vec2<i32> {
    return vec2<i32>(frame.render_size.xy);
}

fn display_size_i() -> vec2<i32> {
    return vec2<i32>(frame.display_size.xy);
}

fn flow_size_i() -> vec2<i32> {
    return vec2<i32>(frame.flow_size.xy);
}

fn is_on_screen(pos: vec2<i32>, size: vec2<i32>) -> bool {
    return all(pos >= vec2<i32>(0)) && all(pos < size);
}

fn is_uv_inside(uv: vec2<f32>) -> bool {
    return uv.x > 0.0 && uv.x < 1.0 && uv.y > 0.0 && uv.y < 1.0;
}

// Device depth → positive view depth, as `get_view_space_depth` in `fsr3_common.wgsl`: kept
// finite (1e7 m) where the device depth is the far plane of an infinite projection, which
// would divide by zero (audit-v035-wgsl 2). Unchanged for every depth of a finite frustum.
fn device_to_view_depth(device_depth: f32) -> f32 {
    let den = device_depth - frame.depth.x;
    return min(abs(frame.depth.y) / max(abs(den), 1.0e-12), 1.0e7);
}

fn linear_from_srgb(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

fn linear_from_pq(c: vec3<f32>) -> vec3<f32> {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 4096.0 * 128.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 4096.0 * 32.0;
    let c3 = 2392.0 / 4096.0 * 32.0;
    let p = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / m2));
    return pow(max(p - vec3<f32>(c1), vec3<f32>(0.0)) / (vec3<f32>(c2) - vec3<f32>(c3) * p), vec3<f32>(1.0 / m1));
}

// `RawRGBToLinear`.
fn raw_to_linear(raw: vec3<f32>) -> vec3<f32> {
    switch frame.flags.x {
        case 1u: {
            return linear_from_pq(raw) * (10000.0 / frame.tune.z);
        }
        case 2u: {
            let lo = frame.tune.y / 80.0;
            let range = (frame.tune.z - frame.tune.y) / 80.0;
            return (raw - vec3<f32>(lo)) / range;
        }
        default: {
            return linear_from_srgb(raw);
        }
    }
}

// `RawRGBToLuminance`.
fn raw_to_luminance(raw: vec3<f32>) -> f32 {
    let weights = select(
        vec3<f32>(0.2126, 0.7152, 0.0722),
        vec3<f32>(0.2627, 0.678, 0.0593),
        frame.flags.x == 1u,
    );
    return dot(raw_to_linear(raw), weights);
}

fn min_divided_by_max(a: f32, b: f32) -> f32 {
    let m = max(a, b);
    return select(0.0, min(a, b) / m, m != 0.0);
}

fn normalized_dot3(a: vec3<f32>, b: vec3<f32>) -> f32 {
    let m = max(length(a), length(b));
    return select(1.0, dot(a / m, b / m), m > 0.0);
}

// The bilinear footprint of `uv` on a `size` image (`GetBilinearSamplingData`).
fn bilinear_base(uv: vec2<f32>, size: vec2<i32>) -> vec2<i32> {
    return vec2<i32>(floor(uv * vec2<f32>(size) - vec2<f32>(0.5)));
}

fn bilinear_weight(uv: vec2<f32>, size: vec2<i32>, index: i32) -> f32 {
    let f = fract(uv * vec2<f32>(size) - vec2<f32>(0.5));
    let wx = select(1.0 - f.x, f.x, (index & 1) == 1);
    let wy = select(1.0 - f.y, f.y, (index >> 1u) == 1);
    return wx * wy;
}

fn bilinear_offset(index: i32) -> vec2<i32> {
    return vec2<i32>(index & 1, index >> 1u);
}

// A UV component in 16 bits, stored unsigned and biased by 32767. The source stores half floats;
// naga's validation here refuses the `pack2x16float` builtins without a capability, and 1/32767 of
// the image is finer than a half float's step over most of the range (a texel at 8K is 1/7680).
// A signed low half would make a field `atomicMax` prefer every negative component. The bias keeps
// 0 at 0 after unpack, and a larger component is a larger unsigned word, so a priority tie breaks
// toward the greater component of one whole vector (audit-g-render3-correctness 1).
fn pack_uv(v: f32) -> u32 {
    return u32(round(clamp(v, -1.0, 1.0) * 32767.0 + 32767.0));
}

fn unpack_uv(bits: u32) -> f32 {
    return (f32(bits & 0xffffu) - 32767.0) / 32767.0;
}

// `PackVectorFieldEntries`: one word per component, the priority on top.
fn pack_entry(primary: bool, high: u32, low: u32, mv: vec2<f32>) -> vec2<u32> {
    let priority = select(0u, PRIMARY_BIT, primary)
        | ((high & PRIORITY_HIGH_MAX) << PRIORITY_HIGH_OFFSET)
        | ((low & PRIORITY_LOW_MAX) << PRIORITY_LOW_OFFSET);
    return vec2<u32>(priority | pack_uv(mv.x), priority | pack_uv(mv.y));
}

struct VectorFieldEntry {
    mv: vec2<f32>,
    high: f32,
    low: f32,
    valid: bool,
    primary: bool,
    secondary: bool,
    in_painted: bool,
    velocity: f32,
    neg_outside: bool,
    pos_outside: bool,
}

// `UnpackVectorFieldEntries`.
fn unpack_entry(packed_entry: vec2<u32>) -> VectorFieldEntry {
    let primary = (packed_entry.x & PRIMARY_BIT) != 0u;
    let raw_high = f32((packed_entry.x >> PRIORITY_HIGH_OFFSET) & PRIORITY_HIGH_MAX) / f32(PRIORITY_HIGH_MAX);
    let low = f32((packed_entry.x >> PRIORITY_LOW_OFFSET) & PRIORITY_LOW_MAX) / f32(PRIORITY_LOW_MAX);
    let valid = raw_high > 0.0;
    let secondary = valid && !primary;
    // Secondary vectors reverse their priority.
    let high = select(raw_high, 1.0 - raw_high, secondary);
    // A cleared word is 0. Biased 0 unpacks as -1, and an invalid entry has no vector.
    let mv = select(
        vec2<f32>(0.0),
        vec2<f32>(unpack_uv(packed_entry.x), unpack_uv(packed_entry.y)),
        valid,
    );
    return VectorFieldEntry(mv, high, low, valid, primary, secondary, false, 0.0, false, false);
}
