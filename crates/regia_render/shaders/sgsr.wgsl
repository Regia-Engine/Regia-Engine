// SPDX-License-Identifier: BSD-3-Clause
// Portions ported from Filament (https://github.com/google/filament, commit ac4a2a8554),
// file filament/src/materials/sgsr/sgsr1_shader_mobile.fs, which is
// Snapdragon Game Super Resolution 1 by Qualcomm:
//
//                  Copyright (c) 2023, Qualcomm Innovation Center, Inc. All rights reserved.
//                              SPDX-License-Identifier: BSD-3-Clause
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.
//
// Regia Engine port: WGSL, the RGBA operation mode (green is the luma proxy, as Filament's opaque
// variant) with UseEdgeDirection, EdgeThreshold 8/255 and EdgeSharpness 2 (Filament's
// `sgsr1.mat`), bilinear base sample (Filament's sampler choice). It is the last pass of the
// post stack: it reads the display-encoded image at the internal size and writes the output.
// Like the blit, it decodes sRGB when `extra.x` > 0.5, and when `extra.w` > 1 (HDR headroom,
// peak / paper white) it expands highlights above the 0.5 knee with the same C¹ quadratic as
// `post.wgsl` mode 7 (ADR-092). The ported filter stays in the 8-bit display-encoded domain
// (its 8/255 and 23/255 thresholds); an SDR frame, where `extra.w` is 0, is unchanged.
//
// params.zw = 1 / output view size. extra.x = 1: decode sRGB; extra.yz = viewport origin.

#import regia::utils

struct PostParams {
    params: vec4<f32>,
    extra: vec4<f32>,
    wb: vec4<f32>,
    lift: vec4<f32>,
    gamma: vec4<f32>,
    gain: vec4<f32>,
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> post: PostParams;

const EDGE_THRESHOLD: f32 = 8.0 / 255.0;
const EDGE_SHARPNESS: f32 = 2.0;

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

fn fast_lanczos2(x: f32) -> f32 {
    var wa = x - 4.0;
    let wb = x * wa - wa;
    wa = wa * wa;
    return wb * wa;
}

/// Weight and weighted luma of one tap at offset (dx, dy) from the sample, along the edge.
fn weight_y(dx: f32, dy: f32, c: f32, data: vec3<f32>) -> vec2<f32> {
    let std_deviation = data.x;
    let dir = data.yz;
    let edge_dis = dx * dir.y + dy * dir.x;
    let x = (dx * dx + dy * dy)
        + (edge_dis * edge_dis) * (clamp(c * c * std_deviation, 0.0, 1.0) * 0.7 - 1.0);
    let w = fast_lanczos2(x);
    return vec2<f32>(w, w * c);
}

fn edge_direction(left: vec4<f32>, right: vec4<f32>) -> vec2<f32> {
    let rx_lz = right.x - left.z;
    let rw_ly = right.w - left.y;
    let delta = vec2<f32>(rx_lz + rw_ly, rx_lz - rw_ly);
    let length_inv = inverseSqrt(delta.x * delta.x + 3.075740e-05 + delta.y * delta.y);
    return delta * length_inv;
}

/// One output pixel at `uv` (output-relative, 0..1) from the input `src_tex`.
fn sgsr(uv: vec2<f32>) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(src_tex));
    let texel = 1.0 / size;
    var color = vec4<f32>(textureSampleLevel(src_tex, src_samp, uv, 0.0).xyz, 0.0);
    let img_coord = uv * size + vec2<f32>(-0.5, 0.5);
    let img_coord_pixel = floor(img_coord);
    var coord = img_coord_pixel * texel;
    let pl = img_coord - img_coord_pixel;
    var left = textureGather(1, src_tex, src_samp, coord);

    let edge_vote = abs(left.z - left.y) + abs(color.y - left.y) + abs(color.y - left.z);
    if (edge_vote > EDGE_THRESHOLD) {
        coord.x += texel.x;

        var right = textureGather(1, src_tex, src_samp, coord + vec2<f32>(texel.x, 0.0));
        var up_down = vec4<f32>(
            textureGather(1, src_tex, src_samp, coord + vec2<f32>(0.0, -texel.y)).wz,
            textureGather(1, src_tex, src_samp, coord + vec2<f32>(0.0, texel.y)).yx,
        );

        let mean = (left.y + left.z + right.x + right.w) * 0.25;
        left = left - vec4<f32>(mean);
        right = right - vec4<f32>(mean);
        up_down = up_down - vec4<f32>(mean);
        color.w = color.y - mean;

        let sum = dot(abs(left), vec4<f32>(1.0)) + dot(abs(right), vec4<f32>(1.0))
            + dot(abs(up_down), vec4<f32>(1.0));

        let sum_mean = 1.014185e+01 / sum;
        let std_deviation = sum_mean * sum_mean;
        let data = vec3<f32>(std_deviation, edge_direction(left, right));

        var awy = weight_y(pl.x, pl.y + 1.0, up_down.x, data);
        awy += weight_y(pl.x - 1.0, pl.y + 1.0, up_down.y, data);
        awy += weight_y(pl.x - 1.0, pl.y - 2.0, up_down.z, data);
        awy += weight_y(pl.x, pl.y - 2.0, up_down.w, data);
        awy += weight_y(pl.x + 1.0, pl.y - 1.0, left.x, data);
        awy += weight_y(pl.x, pl.y - 1.0, left.y, data);
        awy += weight_y(pl.x, pl.y, left.z, data);
        awy += weight_y(pl.x + 1.0, pl.y, left.w, data);
        awy += weight_y(pl.x - 1.0, pl.y - 1.0, right.x, data);
        awy += weight_y(pl.x - 2.0, pl.y - 1.0, right.y, data);
        awy += weight_y(pl.x - 2.0, pl.y, right.z, data);
        awy += weight_y(pl.x - 1.0, pl.y, right.w, data);

        let final_y = awy.y / awy.x;
        let max_y = max(max(left.y, left.z), max(right.x, right.w));
        let min_y = min(min(left.y, left.z), min(right.x, right.w));
        var delta_y = clamp(EDGE_SHARPNESS * final_y, min_y, max_y) - color.w;

        // Smooth high-contrast input.
        delta_y = clamp(delta_y, -23.0 / 255.0, 23.0 / 255.0);

        // Still display-encoded. HDR headroom is applied in `fs_main`, after this filter,
        // so these 8-bit clamps stay the Qualcomm thresholds on an SDR frame.
        color = vec4<f32>(clamp(color.xyz + vec3<f32>(delta_y), vec3<f32>(0.0), vec3<f32>(1.0)), color.w);
    }
    // The alpha channel is not used.
    return vec4<f32>(color.xyz, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = (pos.xy - post.extra.yz) * post.params.zw;
    var c = sgsr(uv).rgb;
    if (post.extra.x > 0.5) {
        c = regia::utils::decode_srgb(c);
    }
    if (post.extra.w > 1.0) {
        // HDR display stage (ADR-092): same expansion as the post blit (mode 7).
        // Mirror of `calibration::hdr_highlight_expand`. Runs after the upscale so the
        // ported filter still sees display-encoded 8-bit values.
        let k = 0.5;
        let t = clamp((c - vec3<f32>(k)) / (1.0 - k), vec3<f32>(0.0), vec3<f32>(1.0));
        let expanded = vec3<f32>(k) + (1.0 - k) * t + (post.extra.w - 1.0) * t * t;
        c = select(c, expanded, c > vec3<f32>(k));
    }
    return vec4<f32>(c, 1.0);
}
