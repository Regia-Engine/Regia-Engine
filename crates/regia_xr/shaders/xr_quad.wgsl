// SPDX-License-Identifier: MPL-2.0
// A quad layer as the simulator's compositor (or the over-budget fallback) draws it: a textured
// rectangle in the panel's plane, no depth, blended over the eye image (spec 58 § 2.10).

struct Panel {
    clip_from_panel: mat4x4<f32>,
    half_size: vec2<f32>,
    pad: vec2<f32>,
    tint: vec4<f32>,
}

@group(0) @binding(0) var<uniform> panel: Panel;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

struct Quad {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Quad {
    // Two triangles over the unit square.
    let x = f32((0x1Cu >> index) & 1u);
    let y = f32((0x0Eu >> index) & 1u);
    let local = vec2<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0) * panel.half_size;
    // A constructor, not a struct `var` (ADR-026).
    return Quad(panel.clip_from_panel * vec4<f32>(local, 0.0, 1.0), vec2<f32>(x, y));
}

@fragment
fn fs_main(in: Quad) -> @location(0) vec4<f32> {
    return textureSample(image, image_sampler, in.uv) * panel.tint;
}
