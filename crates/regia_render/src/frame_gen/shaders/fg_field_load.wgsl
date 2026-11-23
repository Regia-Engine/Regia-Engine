// SPDX-License-Identifier: MIT
// Frame generation: reads the game vector field, with its inpainting.
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

// `ComputeMvInpaintingLevel`.
fn inpainting_level(uv: vec2<f32>, mip: i32, size: vec2<i32>) -> vec4<f32> {
    let base = bilinear_base(uv, size);
    var sum = 0.0;
    var color = vec4<f32>(0.0);
    for (var i = 0; i < 4; i = i + 1) {
        let pos = base + bilinear_offset(i);
        if (is_on_screen(pos, size)) {
            let s = textureLoad(pyramid, pos, mip);
            let w = bilinear_weight(uv, size, i) * f32(s.z > 0.0) * s.z;
            sum = sum + w;
            color = color + s * w;
        }
    }
    return color / select(1.0, sum, sum > 0.0);
}

// `LoadInpaintedGameFieldMv`.
fn load_inpainted_game_mv(uv: vec2<f32>) -> VectorFieldEntry {
    let size = render_size_i();
    let px = clamp(vec2<i32>(uv * frame.render_size.xy), vec2<i32>(0), size - vec2<i32>(1));
    let index = u32(px.y) * u32(size.x) + u32(px.x);
    let entry = unpack_entry(vec2<u32>(game_x[index], game_y[index]));
    var mv = entry.mv;
    var high = entry.high;
    var low = entry.low;
    var in_painted = false;
    if (!entry.valid) {
        var tex_size = size;
        var found = vec4<f32>(0.0);
        let levels = i32(textureNumLevels(pyramid));
        for (var mip = 0; mip < levels && found.w == 0.0; mip = mip + 1) {
            tex_size = tex_size / 2;
            if (tex_size.x == 0 || tex_size.y == 0) {
                break;
            }
            found = inpainting_level(uv, mip, tex_size);
        }
        mv = found.xy;
        high = found.z;
        low = found.w;
        in_painted = true;
    }
    return VectorFieldEntry(
        mv,
        high,
        low,
        entry.valid,
        entry.primary,
        entry.secondary,
        in_painted,
        length(mv),
        !is_uv_inside(uv - mv),
        !is_uv_inside(uv + mv),
    );
}
