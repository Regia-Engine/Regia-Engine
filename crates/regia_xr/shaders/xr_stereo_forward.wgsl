// SPDX-License-Identifier: MPL-2.0
// The reduced forward set of the multiview strategy (spec 58 § 2.6): one directional light, ambient
// and a material's factors, for a standalone headset's frame budget. The view index comes from
// `multiview_entry` when one pass fills both layers, and from `single.layer` when each layer takes
// its own pass, so both strategies shade with the same code and can be compared.

struct Stereo {
    view_proj: array<mat4x4<f32>, 2>,
    eye: array<vec4<f32>, 2>,
    // Direction toward the light in xyz, exposure in w.
    sun: vec4<f32>,
    // Linear light colour times illuminance over pi in rgb.
    sun_color: vec4<f32>,
    // Ambient illuminance over pi in rgb.
    ambient: vec4<f32>,
}

struct Single {
    layer: u32,
}

struct Object {
    model: mat4x4<f32>,
    normal_x: vec4<f32>,
    normal_y: vec4<f32>,
    normal_z: vec4<f32>,
    base_color: vec4<f32>,
    // Emissive radiance in rgb, roughness in a.
    emissive: vec4<f32>,
}

@group(0) @binding(0) var<uniform> stereo: Stereo;
@group(0) @binding(1) var<uniform> single: Single;
@group(1) @binding(0) var<uniform> object: Object;

struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
}

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
    @location(3) @interpolate(flat) view: u32,
}

fn shade_vertex(in: VertexIn, view: u32) -> VertexOut {
    let world = object.model * vec4<f32>(in.position, 1.0);
    let normal = vec3<f32>(
        dot(object.normal_x.xyz, in.normal),
        dot(object.normal_y.xyz, in.normal),
        dot(object.normal_z.xyz, in.normal),
    );
    // A constructor, not a struct `var` (ADR-026).
    return VertexOut(stereo.view_proj[view] * world, world.xyz, normal, in.color, view);
}

@vertex
fn vs_single(in: VertexIn) -> VertexOut {
    return shade_vertex(in, single.layer);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = stereo.sun.xyz;
    let v = normalize(stereo.eye[in.view].xyz - in.world);
    let h = normalize(l + v);
    let albedo = object.base_color.rgb * in.color.rgb;
    let n_dot_l = max(dot(n, l), 0.0);
    let shininess = 2.0 / max(object.emissive.a * object.emissive.a, 0.002);
    let specular = pow(max(dot(n, h), 0.0), shininess) * n_dot_l * 0.04;
    let radiance = albedo * (stereo.ambient.rgb + stereo.sun_color.rgb * n_dot_l)
        + stereo.sun_color.rgb * specular
        + object.emissive.rgb;
    let exposed = radiance * stereo.sun.w;
    return vec4<f32>(exposed / (exposed + vec3<f32>(1.0)), 1.0);
}
