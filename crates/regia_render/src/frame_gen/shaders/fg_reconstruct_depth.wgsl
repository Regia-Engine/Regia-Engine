// SPDX-License-Identifier: MIT
// Frame generation: the depth estimate of the interpolated frame.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_reconstruct_previous_depth.h,
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
@group(0) @binding(2) var<storage, read_write> reconstructed_interpolated: array<atomic<u32>>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = render_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    let sample = textureLoad(dilated, px, 0);
    let uv = (vec2<f32>(px) + vec2<f32>(0.5)) * frame.render_size.zw + sample.xy * 0.5;
    let base = bilinear_base(uv, size);
    let bits = bitcast<u32>(saturate(1.0 - sample.z));
    for (var i = 0; i < 4; i = i + 1) {
        if (bilinear_weight(uv, size, i) > BILINEAR_WEIGHT_THRESHOLD) {
            let store = base + bilinear_offset(i);
            if (is_on_screen(store, size)) {
                atomicMax(&reconstructed_interpolated[u32(store.y) * u32(size.x) + u32(store.x)], bits);
            }
        }
    }
}
