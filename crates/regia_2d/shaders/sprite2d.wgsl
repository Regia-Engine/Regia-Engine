// SPDX-License-Identifier: MPL-2.0
// Regia Engine 2D sprite pass (specs/22 § T2.10): instanced textured quads, nearest sampling, straight
// alpha "over" blending; plus the pixel-perfect blit that scales the virtual screen.

struct View {
    clip_from_world: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var sprite_texture: texture_2d<f32>;
@group(1) @binding(1) var sprite_sampler: sampler;

struct Instance {
    @location(0) origin: vec2<f32>,
    @location(1) axis_x: vec2<f32>,
    @location(2) axis_y: vec2<f32>,
    @location(3) uv_min: vec2<f32>,
    @location(4) uv_max: vec2<f32>,
    @location(5) color: vec4<f32>,
    @location(6) flags: u32,
}

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

// Two triangles: (0,0) (1,0) (1,1) / (0,0) (1,1) (0,1); corner (0, 0) is the quad's
// bottom-left. A switch, not an indexed local array: naga 30 gives that array an ArrayStride,
// which SPIR-V 1.6 forbids on Function storage (VUID-StandaloneSpirv-None-10684).
fn corner(index: u32) -> vec2<f32> {
    switch index {
        case 1u: { return vec2<f32>(1.0, 0.0); }
        case 2u, 4u: { return vec2<f32>(1.0, 1.0); }
        case 5u: { return vec2<f32>(0.0, 1.0); }
        default: { return vec2<f32>(0.0, 0.0); }
    }
}

@vertex
fn vs_sprite(@builtin(vertex_index) vertex: u32, instance: Instance) -> VsOut {
    // A non-finite alpha (IEEE 754 binary32: all-ones exponent) rejects the sprite here, on the
    // instance attribute as fetched: Metal compiles with fast math by default, which may assume an
    // interpolated value finite and fold the fragment stage's test away (an infinite alpha printed
    // white on Apple M5, ADR-7335). Every corner collapses to one point, so nothing rasterizes.
    if (bitcast<u32>(instance.color.a) << 1u) >= 0xff000000u {
        return VsOut(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec2<f32>(0.0, 0.0), vec4<f32>(0.0, 0.0, 0.0, 0.0));
    }
    let c = corner(vertex);
    let world = instance.origin + c.x * instance.axis_x + c.y * instance.axis_y;
    // Texture space has v down: the quad's top edge samples the top of the source rectangle.
    var t = vec2<f32>(c.x, 1.0 - c.y);
    // Tiled applies the diagonal flip first, then the mirrors (as image operations); sampling
    // inverts that order.
    if (instance.flags & 2u) != 0u {
        t.y = 1.0 - t.y;
    }
    if (instance.flags & 1u) != 0u {
        t.x = 1.0 - t.x;
    }
    if (instance.flags & 4u) != 0u {
        t = t.yx;
    }
    // A constructor, not a `var out: VsOut` (ADR-026: a Function struct variable carries member
    // Offsets).
    return VsOut(
        view.clip_from_world * vec4<f32>(world, 0.0, 1.0),
        mix(instance.uv_min, instance.uv_max, t),
        instance.color,
    );
}

@fragment
fn fs_sprite(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(sprite_texture, sprite_sampler, in.uv);
    // IEEE 754 binary32: an all-ones exponent marks infinity or NaN. Sample before the
    // varying discard so the implicit texture derivatives remain in uniform control flow.
    if (bitcast<u32>(in.color.a) << 1u) >= 0xff000000u {
        discard;
    }
    let color = texel * vec4<f32>(in.color.rgb, clamp(in.color.a, 0.0, 1.0));
    if color.a <= 0.0 {
        discard;
    }
    return color;
}

struct BlitOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// Full-viewport quad sampling the virtual screen (the viewport is the letterbox rectangle). The
// view matrix maps the quad's UV to the visible part of the virtual screen (identity unless the
// window is smaller than the virtual screen and it is cropped).
@vertex
fn vs_blit(@builtin(vertex_index) vertex: u32) -> BlitOut {
    let c = corner(vertex);
    return BlitOut(
        vec4<f32>(c * 2.0 - 1.0, 0.0, 1.0),
        (view.clip_from_world * vec4<f32>(c.x, 1.0 - c.y, 0.0, 1.0)).xy,
    );
}

@fragment
fn fs_blit(in: BlitOut) -> @location(0) vec4<f32> {
    return textureSample(sprite_texture, sprite_sampler, in.uv);
}
