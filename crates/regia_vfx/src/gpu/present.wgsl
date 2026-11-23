// SPDX-License-Identifier: MPL-2.0
// Display present for the offscreen harness.
//
// The sky, ground and particles are drawn into an Rgba16Float target (blendable HDR;
// Rgba32Float is not a blend target on Vulkan). This pass reads that target with
// textureLoad, optionally adds a small bloom of the luma above 1, then applies the
// hue-preserving peak scale from `quality::tonemap_display`. The colour target is
// Rgba8UnormSrgb, so the shader writes linear values and the attachment encodes them.
//
// Bloom is a fixed 8-tap average of the excess (`c * (luma - 1) / luma` when luma > 1),
// scaled by 0.85. It is the same idea as Kawase's small gather (Masaki Kawase,
// "Frame Buffer Postprocessing Effects in DOUBLE-S.T.E.A.L", GDC 2003) reduced to one
// pass over the HDR excess only — not a dual-filter pyramid.

struct Harness {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    params: vec4<f32>,      // x fog, y checker, z bloom (1 = on)
    point: vec4<f32>,       // matches harness.wgsl; this pass does not shade it
    point_color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> h: Harness;
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;

fn tonemap(rgb: vec3<f32>) -> vec3<f32> {
    let peak = max(max(rgb.r, rgb.g), max(rgb.b, 0.0));
    if (peak <= 1.0) {
        return max(rgb, vec3<f32>(0.0));
    }
    return rgb * (1.0 / peak);
}

/// Adds the gathered HDR excess. The base keeps its own hue: a pink core stays
/// pink, and this pass does not push pixels into `quality::in_fire_family`.
fn add_bloom(base: vec3<f32>, bloom: vec3<f32>) -> vec3<f32> {
    return base + bloom;
}

fn excess_at(p: vec2<i32>, dims: vec2<i32>) -> vec3<f32> {
    let x = clamp(p.x, 0, dims.x - 1);
    let y = clamp(p.y, 0, dims.y - 1);
    let s = max(textureLoad(hdr_tex, vec2<i32>(x, y), 0).rgb, vec3<f32>(0.0));
    let yv = dot(s, vec3<f32>(0.2126, 0.7152, 0.0722));
    if (yv <= 1.0) {
        return vec3<f32>(0.0);
    }
    return s * ((yv - 1.0) / yv);
}

@vertex
fn vs_present(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var x = -1.0;
    var y = -1.0;
    if (vi == 1u) {
        x = 3.0;
    }
    if (vi == 2u) {
        y = 3.0;
    }
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_present(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims_u = textureDimensions(hdr_tex);
    let dims = vec2<i32>(i32(dims_u.x), i32(dims_u.y));
    let px = vec2<i32>(pos.xy);
    let c = textureLoad(hdr_tex, px, 0).rgb;
    var bloom = vec3<f32>(0.0);
    if (h.params.z > 0.5) {
        // Radius ~5 px, eight taps, unrolled (no function-local array).
        let e0 = excess_at(px + vec2<i32>(5, 0), dims);
        let e1 = excess_at(px + vec2<i32>(-5, 0), dims);
        let e2 = excess_at(px + vec2<i32>(0, 5), dims);
        let e3 = excess_at(px + vec2<i32>(0, -5), dims);
        let e4 = excess_at(px + vec2<i32>(4, 4), dims);
        let e5 = excess_at(px + vec2<i32>(-4, 4), dims);
        let e6 = excess_at(px + vec2<i32>(4, -4), dims);
        let e7 = excess_at(px + vec2<i32>(-4, -4), dims);
        bloom = (e0 + e1 + e2 + e3 + e4 + e5 + e6 + e7) * (0.85 / 8.0);
    }
    let mapped = tonemap(add_bloom(c, bloom));
    return vec4<f32>(mapped, 1.0);
}
