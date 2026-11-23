// SPDX-License-Identifier: MIT
// FSR 3.1 upscaler, pass 3: shading change (render / 2).
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/fsr3upscaler/ffx_fsr3upscaler_shading_change.h, tag v1.1.4 only.
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
// Regia Engine changes: the three pyramid mips are separate textures (sampled at their own
// level 0) instead of one mip chain; the store is RGBA8 (FSR: R8).

@group(0) @binding(0) var<uniform> frame: Fsr3Frame;
@group(0) @binding(1) var mip0_tex: texture_2d<f32>;
@group(0) @binding(2) var mip1_tex: texture_2d<f32>;
@group(0) @binding(3) var mip2_tex: texture_2d<f32>;
@group(0) @binding(4) var linear_clamp: sampler;
@group(0) @binding(5) var out_shading_change: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = shading_change_render_size();
    if (!is_on_screen(px, size)) {
        return;
    }
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) / vec2<f32>(size);
    let uv_jittered = uv + jitter() * frame.render_size.zw;
    let mip_uv = clamp_uv(uv_jittered, size, vec2<i32>(textureDimensions(mip0_tex)));
    let s0 = textureSampleLevel(mip0_tex, linear_clamp, mip_uv, 0.0).xy;
    let s1 = textureSampleLevel(mip1_tex, linear_clamp, mip_uv, 0.0).xy;
    let s2 = textureSampleLevel(mip2_tex, linear_clamp, mip_uv, 0.0).xy;
    // Mips 0..3 with scale 1 + 0 / 3 = 1 (integer division in the source).
    var shading_change = 0.0;
    let c0 = abs(s0.x * s0.y);
    let c1 = abs(s1.x * s1.y);
    let c2 = abs(s2.x * s2.y);
    if (c0 > 0.0) {
        shading_change = max(shading_change, c0);
    }
    if (c1 > 0.0) {
        shading_change = max(shading_change, c1);
    }
    if (c2 > 0.0) {
        shading_change = max(shading_change, c2);
    }
    textureStore(out_shading_change, px, vec4<f32>(saturate(shading_change), 0.0, 0.0, 0.0));
}
