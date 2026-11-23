// SPDX-License-Identifier: MPL-2.0
// Immediate triangles (the quick draw layer): coloured, optionally textured, premultiplied.
//
// Vertex colours arrive linear with straight alpha and leave premultiplied; every blend state
// of the immediate pipelines expects premultiplied colour. `view.flags.x` selects the output
// encoding: 0 = the target encodes (sRGB formats) or is linear (float formats), 1 = encode sRGB
// in the shader (8-bit UNORM targets).

struct ImmediateView {
    clip_from_local: mat4x4<f32>,
    flags: vec4<u32>,
}

@group(0) @binding(0) var<uniform> view: ImmediateView;
@group(1) @binding(0) var image: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    let clip = view.clip_from_local * vec4<f32>(in.position, 1.0);
    // 2D overlay: depth is meaningless, keep every triangle inside the clip volume.
    return VertexOutput(
        vec4<f32>(clip.xy, 0.5 * clip.w, clip.w),
        in.uv,
        vec4<f32>(in.color.rgb * in.color.a, in.color.a),
    );
}

fn srgb_encode_channel(l: f32) -> f32 {
    if l <= 0.0031308 {
        return l * 12.92;
    }
    return 1.055 * pow(l, 1.0 / 2.4) - 0.055;
}

// Encodes a premultiplied linear colour for the target.
fn encode(c: vec4<f32>) -> vec4<f32> {
    if view.flags.x == 1u && c.a > 0.0 {
        let straight = clamp(c.rgb / c.a, vec3<f32>(0.0), vec3<f32>(1.0));
        let e = vec3<f32>(
            srgb_encode_channel(straight.r),
            srgb_encode_channel(straight.g),
            srgb_encode_channel(straight.b),
        );
        return vec4<f32>(e * c.a, c.a);
    }
    return c;
}

// RGBA images (premultiplied texels) times the vertex colour; untextured draws bind a white
// texel.
@fragment
fn fs_image(in: VertexOutput) -> @location(0) vec4<f32> {
    return encode(textureSample(image, image_sampler, in.uv) * in.color);
}

// Coverage masks (glyph atlases): the red channel scales the vertex colour.
@fragment
fn fs_mask(in: VertexOutput) -> @location(0) vec4<f32> {
    return encode(in.color * textureSample(image, image_sampler, in.uv).r);
}

// ---- Composite: the multisampled layer, resolved, over the frame ----

struct CompositeOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_composite(@builtin(vertex_index) index: u32) -> CompositeOutput {
    // One triangle covering the target.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return CompositeOutput(vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0), uv);
}

@fragment
fn fs_composite(in: CompositeOutput) -> @location(0) vec4<f32> {
    return encode(textureSample(image, image_sampler, in.uv));
}
