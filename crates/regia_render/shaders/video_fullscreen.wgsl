// SPDX-License-Identifier: MPL-2.0
// Fullscreen video (specs/50 § 2.7, ADR-2044): one picture drawn into the display image after the
// post stack. The render pass viewport is the visible part of the fit rectangle; `uv` maps it to
// the visible part of the picture. `flags.x` 1 draws the black bars of a letterboxed picture.

struct Draw {
    uv: vec4<f32>,
    // x: 1 draws a black bar, 0 the picture.
    flags: vec4<f32>,
}

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var picture: texture_2d<f32>;
@group(0) @binding(2) var picture_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // One triangle covering the viewport; `t` runs 0..1 across it.
    let t = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    let uv0 = draw.uv.xy;
    let uv1 = draw.uv.zw;
    return VsOut(vec4<f32>(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0), uv0 + (uv1 - uv0) * t);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (draw.flags.x > 0.5) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let c = textureSampleLevel(picture, picture_sampler, in.uv, 0.0);
    return vec4<f32>(c.rgb, 1.0);
}
