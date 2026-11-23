// SPDX-License-Identifier: MPL-2.0
// Frame generation: copies a finished image onto the present target (Regia Engine's own).
//
// `fs_copy` is for a source with the target's own format (the rendered frame): what the source's
// view decodes the target's view encodes again, bit for bit. `fs_decode_srgb` takes the
// interpolator's output, which holds sRGB-encoded values, to an sRGB target view.

@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

@fragment
fn fs_copy(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(source, vec2<i32>(position.xy), 0);
}

@fragment
fn fs_decode_srgb(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(source, vec2<i32>(position.xy), 0).rgb;
    let low = c / 12.92;
    let high = pow((max(c, vec3<f32>(0.0)) + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return vec4<f32>(select(high, low, c <= vec3<f32>(0.04045)), 1.0);
}
