// SPDX-License-Identifier: MPL-2.0
// The desktop mirror (spec 58 § 2.6): a cropped eye image into a rectangle of the window.

struct Mirror {
    // The cropped source rectangle: origin and size in uv.
    source: vec4<f32>,
    // 1 when the target is a plain 8-bit format that needs the sRGB encode here.
    encode: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

@group(0) @binding(0) var<uniform> mirror: Mirror;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

struct FullScreen {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> FullScreen {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    // A constructor, not a struct `var` (ADR-026).
    return FullScreen(
        vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0),
        mirror.source.xy + uv * mirror.source.zw,
    );
}

fn encode_srgb(c: vec3<f32>) -> vec3<f32> {
    let low = c * 12.92;
    let high = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(in: FullScreen) -> @location(0) vec4<f32> {
    let color = textureSample(image, image_sampler, in.uv);
    if mirror.encode > 0.5 {
        return vec4<f32>(encode_srgb(color.rgb), 1.0);
    }
    return vec4<f32>(color.rgb, 1.0);
}
