// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler: shared constants, frame uniform and helpers (prepended to every pass).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// FidelityFX Super Resolution 3.1 upscaler (sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_common.h
// and the Lanczos helpers of ffx_fsr3upscaler_sample.h), tag v1.1.4 only.
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
// Regia Engine changes: HLSL → WGSL, FP32 path only; the constant buffer is one uniform
// (`Fsr3Frame`, laid out by `fsr3/mod.rs`); render size = max render size and upscale
// size = max upscale size (targets are recreated on a size change), so the `ClampUv`
// resource sizes are the image sizes; view depth uses the projection's depth row
// derived on the CPU and stays positive and finite on an infinite projection.

const FP16_MIN: f32 = 6.10e-05;
const FP16_MAX: f32 = 65504.0;
const EPSILON: f32 = 6.10e-05;
const TONEMAP_EPSILON: f32 = 6.10e-05;
const FP32_MAX: f32 = 3.4028234e+38;
const FP32_MIN: f32 = 1.175494351e-38;

// Reconstructed depth usage.
const RECONSTRUCTED_DEPTH_BILINEAR_WEIGHT_THRESHOLD: f32 = 6.10e-04;

// Accumulation.
const UPSAMPLE_LANCZOS_WEIGHT_SCALE: f32 = 0.0625;
const AVERAGE_LANCZOS_WEIGHT_PER_FRAME: f32 = 0.04625; // 0.74 × 1 / 16

const LOCK_THRESHOLD: f32 = 1.0;
const LOCK_MAX: f32 = 2.0;

struct Fsr3Frame {
    // xy = render size, zw = 1 / render size.
    render_size: vec4<f32>,
    // xy = upscale (output) size, zw = 1 / upscale size.
    upscale_size: vec4<f32>,
    // xy = previous frame's render size, zw = previous frame's upscale size.
    previous_sizes: vec4<f32>,
    // xy = this frame's jitter in render pixels (FSR convention), zw = the previous frame's.
    jitter: vec4<f32>,
    // x, y = device depth → view depth (`y / (d − x)`), z = 1 when a reactive mask is bound,
    // w = frame index (0 on a reset).
    depth: vec4<f32>,
    // x = exposure, y = delta pre-exposure, z = jitter sequence length, w = velocity factor.
    tune: vec4<f32>,
    // x = accumulation added per frame, y = minimum disocclusion accumulation,
    // z = RCAS sharpness (linear, 0 = off), w = debug view.
    tune2: vec4<f32>,
    // x = reactiveness scale, y = shading change scale, zw unused.
    tune3: vec4<f32>,
    // NDC of the jittered raster → eye-relative world.
    inv_jittered: mat4x4<f32>,
    // Eye-relative world → this frame's jitter-free clip.
    cur_vp: mat4x4<f32>,
    // Eye-relative world → previous frame's jitter-free clip.
    prev_vp: mat4x4<f32>,
    // Eye-relative world → the previous clip the velocity pass used.
    velocity_prev_vp: mat4x4<f32>,
}

fn render_size() -> vec2<f32> {
    return frame.render_size.xy;
}

fn irender_size() -> vec2<i32> {
    return vec2<i32>(frame.render_size.xy);
}

fn upscale_size() -> vec2<f32> {
    return frame.upscale_size.xy;
}

fn iupscale_size() -> vec2<i32> {
    return vec2<i32>(frame.upscale_size.xy);
}

fn previous_render_size() -> vec2<f32> {
    return frame.previous_sizes.xy;
}

fn previous_upscale_size() -> vec2<f32> {
    return frame.previous_sizes.zw;
}

fn jitter() -> vec2<f32> {
    return frame.jitter.xy;
}

fn previous_jitter() -> vec2<f32> {
    return frame.jitter.zw;
}

fn downscale_factor() -> vec2<f32> {
    return frame.render_size.xy * frame.upscale_size.zw;
}

fn exposure() -> f32 {
    return frame.tune.x;
}

fn delta_pre_exposure() -> f32 {
    return frame.tune.y;
}

fn frame_index() -> f32 {
    return frame.depth.w;
}

fn shading_change_render_size() -> vec2<i32> {
    return vec2<i32>(render_size() * 0.5);
}

fn reconstructed_depth_mv_px_threshold(nearest_depth_in_meters: f32) -> f32 {
    return mix(0.25, 0.75, saturate(nearest_depth_in_meters / 100.0));
}

fn get_4k_velocity(motion_vector: vec2<f32>) -> f32 {
    return length(motion_vector * vec2<f32>(3840.0, 2160.0));
}

fn min_divided_by_max_or(v0: f32, v1: f32, on_zero: f32) -> f32 {
    let m = max(v0, v1);
    return select(on_zero, min(v0, v1) / m, m != 0.0);
}

fn min_divided_by_max(v0: f32, v1: f32) -> f32 {
    return min_divided_by_max_or(v0, v1, 0.0);
}

fn ycocg_to_rgb(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(c.x + c.y - c.z, c.x + c.z, c.x - c.y - c.z);
}

fn rgb_to_ycocg(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        0.25 * c.r + 0.5 * c.g + 0.25 * c.b,
        0.5 * c.r - 0.5 * c.b,
        -0.25 * c.r + 0.5 * c.g - 0.25 * c.b,
    );
}

fn rgb_to_luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn tonemap(c: vec3<f32>) -> vec3<f32> {
    return c / (max(max(0.0, c.r), max(c.g, c.b)) + 1.0);
}

fn inverse_tonemap(c: vec3<f32>) -> vec3<f32> {
    return c / max(TONEMAP_EPSILON, 1.0 - max(c.r, max(c.g, c.b)));
}

fn is_uv_inside(uv: vec2<f32>) -> bool {
    return uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
}

fn clamp_load(p: vec2<i32>, offset: vec2<i32>, size: vec2<i32>) -> vec2<i32> {
    return clamp(p + offset, vec2<i32>(0), size - vec2<i32>(1));
}

fn clamp_uv(uv: vec2<f32>, texture_size: vec2<i32>, resource_size: vec2<i32>) -> vec2<f32> {
    let location = uv * vec2<f32>(texture_size);
    let clamped = max(vec2<f32>(0.5), min(location, vec2<f32>(texture_size) - vec2<f32>(0.5)));
    return clamped / vec2<f32>(resource_size);
}

fn is_on_screen(p: vec2<i32>, size: vec2<i32>) -> bool {
    return all(vec2<u32>(p) < vec2<u32>(size));
}

fn compute_hr_pos_from_lr_pos(lr: vec2<i32>) -> vec2<i32> {
    let jittered = vec2<f32>(lr) + vec2<f32>(0.5) - jitter();
    let in_hr = (jittered / render_size()) * upscale_size();
    return vec2<i32>(floor(in_hr));
}

// Device depth → positive linear view depth (metres). FSR's `fDeviceToViewDepth[1] /
// (d − fDeviceToViewDepth[0])`, kept finite at the far plane of an infinite projection.
fn get_view_space_depth(device_depth: f32) -> f32 {
    let den = device_depth - frame.depth.x;
    return min(abs(frame.depth.y) / max(abs(den), 1.0e-12), 1.0e7);
}

// Regia Engine works in metres: `ViewSpaceToMetersFactor` is 1.
fn get_view_space_depth_in_meters(device_depth: f32) -> f32 {
    return get_view_space_depth(device_depth);
}

// FSR1 Lanczos approximation. Input is x² and must be ≤ 4.
fn lanczos2_approx_sq(x2_in: f32) -> f32 {
    let x2 = min(x2_in, 4.0);
    let a = (2.0 / 5.0) * x2 - 1.0;
    let b = (1.0 / 4.0) * x2 - 1.0;
    return ((25.0 / 16.0) * a * a - (25.0 / 16.0 - 1.0)) * (b * b);
}

fn lanczos2(x_in: f32) -> f32 {
    let x = min(abs(x_in), 2.0);
    let pi = 3.141592653589793;
    if (abs(x) < EPSILON) {
        return 1.0;
    }
    return (sin(pi * x) / (pi * x)) * (sin(0.5 * pi * x) / (0.5 * pi * x));
}

// Zero for a NaN or infinite channel, by the exponent bits: SPIR-V leaves `max(x, 0)` (FMax)
// undefined for NaN and RADV/ACO keeps it, so a NaN stayed in the history (v0.1.4).
fn finite_or_zero(c: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(c) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(c, vec3<f32>(0.0), bad);
}
