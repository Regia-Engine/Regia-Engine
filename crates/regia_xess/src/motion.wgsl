// SPDX-License-Identifier: MPL-2.0
// Original channel conversion: preserve Regia's XY velocity without filtering (ADR-4656).
@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pixel: vec4<f32>) -> @location(0) vec2<f32> {
    return textureLoad(source, vec2<i32>(pixel.xy), 0).xy;
}
