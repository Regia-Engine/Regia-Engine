// SPDX-License-Identifier: MPL-2.0
// Boot branding screen: textured quads with a tint and an opacity (`boot.rs`).
// One quad per draw: `rect` is the quad in clip space (x0, y0, x1, y1), `tint` multiplies the
// texture, `tint.a` already includes the fade.

struct Quad {
    rect: vec4<f32>,
    tint: vec4<f32>,
}

@group(0) @binding(0) var<uniform> quad: Quad;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // Two triangles: (0,0) (1,0) (0,1) / (0,1) (1,0) (1,1). A switch, not an indexed local
    // array: naga 30 gives that array an ArrayStride, which SPIR-V 1.6 forbids on Function
    // storage (ADR-026, VUID-StandaloneSpirv-None-10684).
    var c: vec2<f32>;
    switch index {
        case 0u: { c = vec2<f32>(0.0, 0.0); }
        case 1u, 4u: { c = vec2<f32>(1.0, 0.0); }
        case 2u, 3u: { c = vec2<f32>(0.0, 1.0); }
        default: { c = vec2<f32>(1.0, 1.0); }
    }
    let x = mix(quad.rect.x, quad.rect.z, c.x);
    let y = mix(quad.rect.y, quad.rect.w, c.y);
    // Image rows run top to bottom; clip-space y runs bottom to top. A constructor, not a
    // `var out: VsOut` (ADR-026: a Function struct variable carries member Offsets).
    return VsOut(vec4<f32>(x, y, 0.0, 1.0), vec2<f32>(c.x, 1.0 - c.y));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(image, image_sampler, in.uv);
    return texel * quad.tint;
}
