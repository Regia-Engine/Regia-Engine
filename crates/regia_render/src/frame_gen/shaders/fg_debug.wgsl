// SPDX-License-Identifier: MIT
// Frame generation: the debug view of the interpolator's internal surfaces.
//
// Ported from AMD FidelityFX SDK v1.1.4 (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
// sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation_debug_view.h,
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
// Regia Engine changes (ADR-2067): four fixed panes in place of the source's layout. Top left: the
// game vectors, top right: the optical flow vectors (both |uv| x 16, on grey), bottom left: the
// disocclusion mask (red = visible in the previous frame, green = in the current), bottom right:
// the interpolated colour before inpainting, with the inpainted texels in magenta.

@group(0) @binding(0) var<uniform> frame: FgFrame;
@group(0) @binding(1) var<storage, read> game_x: array<u32>;
@group(0) @binding(2) var<storage, read> game_y: array<u32>;
@group(0) @binding(3) var<storage, read> flow_x: array<u32>;
@group(0) @binding(4) var<storage, read> flow_y: array<u32>;
@group(0) @binding(5) var disocclusion: texture_2d<f32>;
@group(0) @binding(6) var interpolated: texture_2d<f32>;
@group(0) @binding(7) var linear_clamp: sampler;
@group(0) @binding(8) var out_color: texture_storage_2d<rgba16float, write>;

fn vector_color(mv: vec2<f32>) -> vec3<f32> {
    return vec3<f32>(saturate(vec2<f32>(0.5) + mv * 16.0), 0.5);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = vec2<i32>(gid.xy);
    let size = display_size_i();
    if (!is_on_screen(px, size)) {
        return;
    }
    let half_size = size / 2;
    let pane = vec2<i32>(i32(px.x >= half_size.x), i32(px.y >= half_size.y));
    // The pane's position as a full-image UV.
    let pane_px = px - pane * half_size;
    let uv = saturate((vec2<f32>(pane_px) + vec2<f32>(0.5)) / vec2<f32>(max(half_size, vec2<i32>(1))));
    var color = vec3<f32>(0.0);
    if (pane.x == 0 && pane.y == 0) {
        let render = render_size_i();
        let p = min(vec2<i32>(uv * vec2<f32>(render)), render - vec2<i32>(1));
        let index = u32(p.y) * u32(render.x) + u32(p.x);
        color = vector_color(unpack_entry(vec2<u32>(game_x[index], game_y[index])).mv);
    } else if (pane.y == 0) {
        let flow_size = flow_size_i();
        let p = min(vec2<i32>(uv * vec2<f32>(flow_size)), flow_size - vec2<i32>(1));
        let index = u32(p.y) * u32(flow_size.x) + u32(p.x);
        color = vector_color(unpack_entry(vec2<u32>(flow_x[index], flow_y[index])).mv);
    } else if (pane.x == 0) {
        color = vec3<f32>(textureSampleLevel(disocclusion, linear_clamp, uv, 0.0).xy, 0.0);
    } else {
        let s = textureSampleLevel(interpolated, linear_clamp, uv, 0.0);
        color = mix(s.rgb, vec3<f32>(1.0, 0.0, 1.0), saturate(s.w));
    }
    textureStore(out_color, px, vec4<f32>(color, 1.0));
}
