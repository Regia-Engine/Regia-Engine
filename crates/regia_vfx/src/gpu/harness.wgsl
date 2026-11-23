// SPDX-License-Identifier: MPL-2.0
// The offscreen harness backdrop: a gradient sky + a sun-lit ground plane.
//
// This is test/reference scenery, deliberately simple: the sky is a view-ray gradient with a
// sun disc (no atmosphere model), the ground a Lambert plane with a two-tone checker and
// distance fog toward the horizon colour. Effects composite on top through crate::gpu's
// transparent pass; the ground writes the scene depth decals project onto and particles
// collide with.

struct Harness {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,        // xyz eye
    sun_dir: vec4<f32>,        // xyz unit vector scene -> sun
    sun_color: vec4<f32>,      // rgb sunlight colour * strength, w ambient strength
    params: vec4<f32>,         // x fog density, y checker scale (m), z bloom, w unused
    point: vec4<f32>,          // xyz world position, w radius (0 = off)
    point_color: vec4<f32>,    // linear rgb * intensity
};

@group(0) @binding(0) var<uniform> h: Harness;

fn sky_colour(dir: vec3<f32>) -> vec3<f32> {
    // Values are LINEAR radiance; the sRGB attachment encodes them for display
    // (display ~ zenith 0.25/0.4/0.7, horizon ~0.78/0.82/0.88).
    let t = clamp(dir.y * 0.5 + 0.5, 0.0, 1.0);
    let zenith = vec3<f32>(0.05, 0.14, 0.46);
    let horizon = vec3<f32>(0.56, 0.63, 0.74);
    var col = mix(horizon, zenith, pow(t, 0.85));
    // below-horizon fade (never visible behind the ground plane, keeps the sky pass clean)
    col = mix(vec3<f32>(0.22, 0.26, 0.30), col, smoothstep(-0.08, 0.02, dir.y));
    let s = clamp(dot(dir, h.sun_dir.xyz), 0.0, 1.0);
    col = col + h.sun_color.rgb * (pow(s, 900.0) * 6.0 + pow(s, 12.0) * 0.05);
    return col;
}

struct SkyOut {
    @builtin(position) position: vec4<f32>,
    @location(0) ray: vec3<f32>,
};

@vertex
fn vs_sky(@builtin(vertex_index) vi: u32) -> SkyOut {
    // Corners (-1,-1), (3,-1), (-1,3). Selects, not a function-local array (ADR-026).
    // Full-screen triangle at the far plane; the fragment ray reconstructs the direction.
    let xy = vec2<f32>(select(-1.0, 3.0, vi == 1u), select(-1.0, 3.0, vi == 2u));
    let world = h.inv_view_proj * vec4<f32>(xy, 1.0, 1.0);
    let ray = normalize(world.xyz / world.w - h.cam_pos.xyz);
    return SkyOut(vec4<f32>(xy, 1.0, 1.0), ray);
}

@fragment
fn fs_sky(in: SkyOut) -> @location(0) vec4<f32> {
    return vec4<f32>(sky_colour(in.ray), 1.0);
}

struct GroundVertex {
    @location(0) position: vec3<f32>,
};

struct GroundOut {
    @builtin(position) position: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_ground(v: GroundVertex) -> GroundOut {
    return GroundOut(h.view_proj * vec4<f32>(v.position, 1.0), v.position);
}

@fragment
fn fs_ground(in: GroundOut) -> @location(0) vec4<f32> {
    let n = vec3<f32>(0.0, 1.0, 0.0);
    let cell = floor(in.world.xz / max(h.params.y, 1.0));
    let checker = (cell.x + cell.y) % 2.0;
    // Linear albedos; display roughly 0.35 (dark) / 0.45 (light) grass tones.
    let albedo = mix(vec3<f32>(0.075, 0.115, 0.038), vec3<f32>(0.115, 0.150, 0.062), checker);
    let sun = max(dot(n, h.sun_dir.xyz), 0.0);
    let ambient = vec3<f32>(0.32, 0.38, 0.52) * h.sun_color.w;
    var col = albedo * (h.sun_color.rgb * sun + ambient);
    // Windowed inverse-square. Same form as `distance_attenuation` in
    // regia_render's pbr_lighting.wgsl (Lagarde, de Rousiers, SIGGRAPH 2014).
    if (h.point.w > 1e-3) {
        let delta = h.point.xyz - in.world;
        let dist = length(delta);
        let r = max(h.point.w, 1e-4);
        let num = clamp(1.0 - pow(dist / r, 4.0), 0.0, 1.0);
        let falloff = (num * num) / (dist * dist + 1.0);
        col = col + albedo * h.point_color.rgb * falloff;
    }
    // distance fog toward the sky at the horizon
    let dist = length(in.world - h.cam_pos.xyz);
    let fog = 1.0 - exp(-dist * max(h.params.x, 0.0));
    col = mix(col, vec3<f32>(0.56, 0.63, 0.74), fog);
    return vec4<f32>(col, 1.0);
}
