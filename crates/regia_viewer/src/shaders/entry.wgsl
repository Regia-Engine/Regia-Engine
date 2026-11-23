// SPDX-License-Identifier: MPL-2.0
// Vertex stage, the fragment entry that calls the family's hook, exposure, tone mapping and output.

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) facet: f32,
    // The instance's affine transform, as three rows.
    @location(4) row0: vec4<f32>,
    @location(5) row1: vec4<f32>,
    @location(6) row2: vec4<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) local_pos: vec3<f32>,
    @location(4) local_normal: vec3<f32>,
    // Every vertex of a facet carries the same number, so any provoking vertex is right.
    @location(5) @interpolate(flat, either) facet: f32,
    @location(6) @interpolate(flat, either) basis_x: vec3<f32>,
    @location(7) @interpolate(flat, either) basis_y: vec3<f32>,
    @location(8) @interpolate(flat, either) basis_z: vec3<f32>,
}

@vertex
fn vs_main(v: VsIn) -> VsOut {
    let p = vec4<f32>(v.position, 1.0);
    let world = vec3<f32>(dot(v.row0, p), dot(v.row1, p), dot(v.row2, p));
    // The cofactor matrix transforms normals right under scale and mirroring.
    let c0 = cross(v.row1.xyz, v.row2.xyz);
    let c1 = cross(v.row2.xyz, v.row0.xyz);
    let c2 = cross(v.row0.xyz, v.row1.xyz);
    let det = dot(v.row0.xyz, c0);
    let flip = select(1.0, -1.0, det < 0.0);
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return VsOut(
        frame.view_proj * vec4<f32>(world, 1.0),
        world,
        vec3<f32>(dot(c0, v.normal), dot(c1, v.normal), dot(c2, v.normal)) * flip,
        v.uv,
        v.position,
        v.normal,
        v.facet,
        normalize(vec3<f32>(v.row0.x, v.row1.x, v.row2.x)),
        normalize(vec3<f32>(v.row0.y, v.row1.y, v.row2.y)),
        normalize(vec3<f32>(v.row0.z, v.row1.z, v.row2.z)),
    );
}

// Khronos PBR Neutral: true to base colors below the highlights, for product images.
fn tone_map(input: vec3<f32>) -> vec3<f32> {
    let start = 0.76;
    let desaturation = 0.15;
    var color = input;
    let x = min(color.r, min(color.g, color.b));
    let offset = select(0.04, x - 6.25 * x * x, x < 0.08);
    color -= vec3<f32>(offset);
    let peak = max(color.r, max(color.g, color.b));
    if (peak < start) {
        return color;
    }
    let d = 1.0 - start;
    let new_peak = 1.0 - d * d / (peak + d - start);
    color *= new_peak / peak;
    let g = 1.0 - 1.0 / (desaturation * (peak - new_peak) + 1.0);
    return mix(color, vec3<f32>(new_peak), g);
}

fn encode_srgb(c: vec3<f32>) -> vec3<f32> {
    let low = c * 12.92;
    let high = 1.055 * pow(max(c, vec3<f32>(0.0031308)), vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(high, low, c <= vec3<f32>(0.0031308));
}

fn finish(radiance: vec3<f32>, alpha: f32) -> vec4<f32> {
    var c = clamp(tone_map(radiance * frame.eye.w), vec3<f32>(0.0), vec3<f32>(1.0));
    if (frame.env.y > 0.5) {
        c = encode_srgb(c);
    }
    return vec4<f32>(c, alpha);
}

@fragment
fn fs_main(in: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    let side = select(-1.0, 1.0, front);
    let normal = normalize(in.world_normal) * side;
    let view = normalize(frame.eye.xyz - in.world_pos);
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    let s = Surface(
        in.world_pos,
        normal,
        view,
        max(dot(normal, view), 0.001),
        in.uv,
        in.local_pos,
        normalize(in.local_normal) * side,
        in.facet,
        mat3x3<f32>(in.basis_x, in.basis_y, in.basis_z),
    );
    return finish(shade_surface(s), 1.0);
}
