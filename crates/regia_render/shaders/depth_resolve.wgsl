// SPDX-License-Identifier: MPL-2.0
// Nearest-sample resolve of an MSAA scene depth onto a single-sample target.
//
// An MSAA frame writes only the multisampled depth. The translucent pass draws into the
// single-sample output and samples depth for soft particles and decal projection, so it
// needs a one-sample copy: the nearest of the pixel's samples (depth 0 is near), at the
// texel under the same UV (the output may also be larger than the scene).

@group(0) @binding(0) var src: texture_depth_multisampled_2d;

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
    let count = i32(textureNumSamples(src));
    var depth = 1.0;
    for (var i = 0; i < count; i = i + 1) {
        depth = min(depth, textureLoad(src, texel, i));
    }
    return FragOut(depth);
}
