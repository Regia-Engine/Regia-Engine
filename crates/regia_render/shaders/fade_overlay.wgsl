// SPDX-License-Identifier: MPL-2.0
// Fullscreen fade (`fade_overlay.rs`): one triangle that blends `color.rgb` over the picture with
// weight `color.a`. `flags.x` = 1 encodes the colour to sRGB here. `fade_overlay.rs` writes 0: its
// `fade_output` already encodes the colour for an 8-bit UNORM target (sRGB and float targets take it
// linear), so the colour is encoded exactly once.

struct Fade {
    color: vec4<f32>,
    flags: vec4<f32>,
}

@group(0) @binding(0) var<uniform> fade: Fade;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One oversized triangle covers the target: (-1,-1) (3,-1) (-1,3).
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
}

fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    var rgb = fade.color.rgb;
    if fade.flags.x > 0.5 {
        rgb = srgb_encode(rgb);
    }
    return vec4<f32>(rgb, fade.color.a);
}
