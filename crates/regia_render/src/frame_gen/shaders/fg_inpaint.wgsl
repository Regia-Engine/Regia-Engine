// SPDX-License-Identifier: MIT
// Frame generation: inpainting of what neither frame shows, the debug bars and indicators.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_inpainting.h,
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
// Regia Engine changes (ADR-2067): reads one image and writes another (no read-write float storage
// image in WGSL). The HUD-less clean-up is gone: the overlay never enters the interpolation here.

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var<storage, read> state: FgState;
@group(0) @binding(2) var interpolated: texture_2d<f32>;
@group(0) @binding(3) var pyramid: texture_2d<f32>;
@group(0) @binding(4) var out_color: texture_storage_2d<rgba16float, write>;

// `ComputeInpaintingLevel`.
fn inpainting_level(uv: vec2<f32>, mip: i32, size: vec2<i32>) -> vec4<f32> {
    let base = bilinear_base(uv, size);
    var color = vec4<f32>(0.0);
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            let s = textureLoad(pyramid, pos, mip);
            let w = bilinear_weight(uv, size, i) * f32(s.w > 0.0);
            color = color + vec4<f32>(s.rgb * w, w);
        }
    }
    return color;
}

// `ComputeInpainting`: rgb, and in `a` whether any level had data.
fn compute_inpainting(px: vec2<i32>) -> vec4<f32> {
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.display_size.zw;
    var color = vec4<f32>(0.0);
    var size = display_size_i();
    let levels = min(i32(textureNumLevels(pyramid)), 10);
    for (var mip = 0; mip < levels; mip = mip + 1) {
        size = size / 2;
        if (size.x == 0 || size.y == 0) {
            break;
        }
        let level = inpainting_level(uv, mip, size);
        if (level.w > 0.0) {
            let weight = pow(1.0 - f32(mip) / 10.0, 3.0) * level.w;
            color = color + vec4<f32>(level.rgb / level.w, 1.0) * weight;
        }
    }
    return vec4<f32>(color.rgb / select(1.0, color.w, color.w > 0.0), color.w);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    if (!is_on_screen(px, display_size_i())) {
        return;
    }
    let source = textureLoad(interpolated, px, 0);
    var color = source.rgb;
    if (source.w > EPSILON) {
        let filled = compute_inpainting(px);
        if (filled.w > 0.0) {
            color = mix(color, filled.rgb, source.w);
        }
    }
    let flags = frame.flags.z;
    if ((flags & FLAG_TEAR_LINES) != 0u) {
        if (px.x < 16) {
            color.g = 1.0;
        } else if (f32(px.x) > frame.display_size.x - 16.0) {
            color = color + frame.debug_bar.rgb;
        }
    }
    if ((flags & FLAG_RESET_INDICATORS) != 0u) {
        if (px.y < 32 && frame.flags.y != 0u) {
            color.r = 1.0;
        } else if (px.y > 32 && px.y < 64 && (state.scene_history & 0xfu) != 0u) {
            color.b = 1.0;
        }
    }
    textureStore(out_color, px, vec4<f32>(color, 1.0));
}
