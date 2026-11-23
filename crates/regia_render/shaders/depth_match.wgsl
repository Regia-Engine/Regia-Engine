// SPDX-License-Identifier: MPL-2.0
// Nearest copy of scene depth onto a larger target.
//
// A scaled frame stores depth at the internal size. The translucent pass draws
// into the full-size output, and wgpu rejects a depth attachment of a different
// size. Each output pixel loads the source texel under the same UV.

@group(0) @binding(0) var src: texture_depth_2d;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct FragOut {
    @builtin(frag_depth) depth: f32,
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return VertexOut(
        vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0),
        vec2<f32>(uv.x, 1.0 - uv.y),
    );
}

@fragment
fn fs_main(in: VertexOut) -> FragOut {
    let dims = vec2<i32>(textureDimensions(src));
    let texel = clamp(vec2<i32>(in.uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    return FragOut(textureLoad(src, texel, 0));
}
