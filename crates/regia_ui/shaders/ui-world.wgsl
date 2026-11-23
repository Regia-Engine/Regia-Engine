// SPDX-License-Identifier: MPL-2.0
// World-space UI panel composite (ADR-2406, `regia_ui::world_target`).
//
// Draws one panel's offscreen UI texture (premultiplied, sRGB-encoded storage) as a quad in the
// view, at the frame's `after_post` slot on the display-referred output. The depth test is done
// here against the scene depth (standard depth: 0 near, 1 far), so the pass needs no depth
// attachment and works at any output resolution. `regia_ui` swaps the depth declaration for
// `texture_depth_multisampled_2d` when the frame's depth is multisampled (sample 0 is read).

struct Panel {
    // Panel units (u right, v down, 0..1) to clip space.
    clip_from_unit: mat4x4<f32>,
    // The view rectangle in the colour target (x, y, w, h in pixels).
    viewport: vec4<f32>,
    // Scene depth size (w, h), depth test on (1) or off (0), panel alpha.
    depth: vec4<f32>,
    // Output encoding (0 hardware sRGB, 1 shader sRGB, 2 scRGB, 3 PQ), paper white nits.
    output: vec4<f32>,
};

@group(0) @binding(0) var<uniform> panel: Panel;
@group(0) @binding(1) var panel_sampler: sampler;
@group(0) @binding(2) var panel_texture: texture_2d<f32>;
@group(1) @binding(0) var scene_depth: texture_depth_2d;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    // Triangle strip over the unit square: (0,0) (1,0) (0,1) (1,1).
    let uv = vec2<f32>(f32(index & 1u), f32(index >> 1u));
    // Built by its constructor: a function-scope struct variable fails the SPIR-V layout check.
    return VsOut(panel.clip_from_unit * vec4<f32>(uv, 0.0, 1.0), uv);
}

// IEC 61966-2-1 sRGB transfer function, linear to encoded.
fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0031308)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// SMPTE ST 2084 inverse EOTF: absolute luminance in nits to the PQ signal.
fn pq_encode(nits: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let y = pow(clamp(nits / 10000.0, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(m1));
    return pow((c1 + c2 * y) / (1.0 + c3 * y), vec3<f32>(m2));
}

// ITU-R BT.2087 conversion of linear BT.709 primaries to BT.2020.
fn rec709_to_rec2020(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(c, vec3<f32>(0.6274, 0.3293, 0.0433)),
        dot(c, vec3<f32>(0.0691, 0.9195, 0.0114)),
        dot(c, vec3<f32>(0.0164, 0.0880, 0.8956)),
    );
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // Linear, premultiplied (the texture's sRGB view decodes).
    let texel = textureSample(panel_texture, panel_sampler, in.uv);
    if (panel.depth.z > 0.5) {
        let uv = (in.position.xy - panel.viewport.xy) / max(panel.viewport.zw, vec2<f32>(1.0));
        let size = vec2<i32>(panel.depth.xy);
        let coord = clamp(vec2<i32>(uv * panel.depth.xy), vec2<i32>(0), size - vec2<i32>(1));
        let scene = textureLoad(scene_depth, coord, 0);
        if (in.position.z > scene + 1e-6) {
            discard;
        }
    }
    let premul = texel * panel.depth.w;
    let alpha = premul.a;
    if (alpha <= 0.0) {
        discard;
    }
    let straight = premul.rgb / alpha;
    let mode = panel.output.x;
    var rgb = premul.rgb;
    if (mode > 2.5) {
        let nits = rec709_to_rec2020(straight) * panel.output.y;
        rgb = pq_encode(nits) * alpha;
    } else if (mode > 1.5) {
        // scRGB: 1.0 = 80 nits; SDR UI at paper white.
        rgb = premul.rgb * (panel.output.y / 80.0);
    } else if (mode > 0.5) {
        rgb = srgb_encode(straight) * alpha;
    }
    return vec4<f32>(rgb, alpha);
}
