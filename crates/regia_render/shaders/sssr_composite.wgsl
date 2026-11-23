// SPDX-License-Identifier: MPL-2.0
// SSSR composite: replaces the share of the forward pass's environment specular that the traced
// reflection covers. With R = (coverage · radiance, coverage) from the denoiser, W the specular
// weight and f the environment-specular share of the pixel (both from the HDR alpha code):
//     out = hdr · (1 − R.a · f) + W · R.rgb
// A pixel the reflection does not cover keeps the forward colour exactly.

#import regia::sssr::{sssr, decode_alpha, is_glossy}

@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var reflection_tex: texture_2d<f32>;
@group(0) @binding(3) var depth_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let pix = vec2<i32>(frag.xy);
    let base = textureLoad(hdr_tex, pix, 0);
    let code = decode_alpha(base.a);
    let z = textureLoad(depth_tex, pix, 0).r;
    if !code.reflective || z >= 1.0 || !is_glossy(code.roughness) {
        return base;
    }
    let r = textureLoad(reflection_tex, pix, 0);
    let coverage = clamp(r.a, 0.0, 1.0);
    let rgb = max(base.rgb * (1.0 - coverage * code.env_fraction) + code.weight * max(r.rgb, vec3<f32>(0.0)), vec3<f32>(0.0));
    return vec4<f32>(rgb, base.a);
}
