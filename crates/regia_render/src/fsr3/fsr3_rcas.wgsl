// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 7: robust contrast-adaptive sharpening (output resolution).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_rcas.h and the FP32 `FsrRcasF` of
// sdk/include/FidelityFX/gpu/fsr1/ffx_fsr1.h (with FSR_RCAS_DENOISE), tag v1.1.4 only.
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
// Regia Engine changes: one output pixel per invocation (the source runs four per thread with a
// quad swizzle); neighbour loads are clamped to the image (the source relies on
// out-of-bounds loads returning 0); the limiter divisions cannot produce NaN.

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var history_tex: texture_2d<f32>;
@group(0) @binding(2) var out_color: texture_storage_2d<rgba16float, write>;

const FSR_RCAS_LIMIT: f32 = 0.1875; // 0.25 − 1 / 16

fn approximate_reciprocal_medium(v: f32) -> f32 {
    let b = bitcast<f32>(0x7ef19fffu - bitcast<u32>(v));
    return b * (-b * v + 2.0);
}

fn rcas_load(p: vec2<i32>) -> vec3<f32> {
    let q = clamp(p, vec2<i32>(0), iupscale_size() - vec2<i32>(1));
    return textureLoad(history_tex, q, 0).rgb * exposure();
}

fn max3(a: f32, b: f32, c: f32) -> f32 {
    return max(a, max(b, c));
}

fn min3(a: f32, b: f32, c: f32) -> f32 {
    return min(a, min(b, c));
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let sp = vec2<i32>(gid.xy);
    if (!is_on_screen(sp, iupscale_size())) {
        return;
    }
    //    b
    //  d e f
    //    h
    let b = rcas_load(sp + vec2<i32>(0, -1));
    let d = rcas_load(sp + vec2<i32>(-1, 0));
    let e = rcas_load(sp);
    let f = rcas_load(sp + vec2<i32>(1, 0));
    let h = rcas_load(sp + vec2<i32>(0, 1));
    // Luma times 2.
    let bl = b.b * 0.5 + (b.r * 0.5 + b.g);
    let dl = d.b * 0.5 + (d.r * 0.5 + d.g);
    let el = e.b * 0.5 + (e.r * 0.5 + e.g);
    let fl = f.b * 0.5 + (f.r * 0.5 + f.g);
    let hl = h.b * 0.5 + (h.r * 0.5 + h.g);
    // Noise detection.
    var nz = 0.25 * bl + 0.25 * dl + 0.25 * fl + 0.25 * hl - el;
    nz = saturate(abs(nz) * approximate_reciprocal_medium(max3(max3(bl, dl, el), fl, hl) - min3(min3(bl, dl, el), fl, hl)));
    nz = -0.5 * nz + 1.0;
    // Min and max of the ring.
    let mn4 = min(min(min(b, d), f), h);
    let mx4 = max(max(max(b, d), f), h);
    let peak = vec2<f32>(1.0, -4.0);
    // Limiters (high precision reciprocals). The denominators are kept off zero, where the
    // source gets 0 × ∞ = NaN and relies on D3D's NaN-discarding max().
    let hit_min = mn4 / max(4.0 * mx4, vec3<f32>(1.0e-20));
    let den = 4.0 * mn4 + peak.y;
    let hit_max = (peak.x - mx4) / select(den, vec3<f32>(-1.0e-20), abs(den) < vec3<f32>(1.0e-20));
    let lobe_rgb = max(-hit_min, hit_max);
    var lobe = max(-FSR_RCAS_LIMIT, min(max3(lobe_rgb.r, lobe_rgb.g, lobe_rgb.b), 0.0)) * frame.tune2.z;
    // Noise removal.
    lobe = lobe * nz;
    let rcp_l = approximate_reciprocal_medium(4.0 * lobe + 1.0);
    let c = (lobe * b + lobe * d + lobe * h + lobe * f + e) * rcp_l;
    textureStore(out_color, sp, vec4<f32>(max(c / exposure(), vec3<f32>(0.0)), 1.0));
}
