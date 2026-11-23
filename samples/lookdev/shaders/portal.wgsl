// SPDX-License-Identifier: MIT-0
// Tunnel-mouth mesh. Lighting matches regia_terrain's terrain.wgsl so the portal
// sits in the same image: EV100 14.5 exposure, π-normalised Lambert, AgX (C. E. Hill,
// CC0), and the Rgba8UnormSrgb target encodes once on store. Cull is off in the
// pipeline; interiors are visible from the road whichever way the collision winding faces.

struct Uniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    // xyz = direction toward the sun, w = illuminance_lux * exposure
    sun_dir_exposure: vec4<f32>,
    sun_color: vec4<f32>,
    ambient: vec4<f32>,
    albedo: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
}

@vertex
fn vs_main(@location(0) position: vec3<f32>, @location(1) normal: vec3<f32>) -> VOut {
    // Constructor return: a function-local struct `var` is illegal SPIR-V 1.6 (ADR-026).
    return VOut(u.view_proj * vec4<f32>(position, 1.0), position, normal);
}

const INV_PI: f32 = 0.3183098862;

// AgX display transform + the engine's default look. Mirror of `regia_render`'s
// `utils.wgsl` (`tonemap_agx_vec`, `display_look`, `DEFAULT_LOOK_*`); the test
// `terrain_display_transform_mirrors_the_renderer` keeps the copies identical.
// Provenance: Sobotka's AgX, minimal form after B. Wrensch (2023); see docs/provenance.md.
// Output is display-encoded; `display_to_linear` hands an sRGB-encoding target linear values.
// BEGIN display-transform mirror
const DEFAULT_LOOK_CONTRAST: f32 = 1.2;
const DEFAULT_LOOK_SATURATION: f32 = 1.4;

fn agx_sigmoid(x: vec3<f32>) -> vec3<f32> {
    let x2 = x * x;
    let x4 = x2 * x2;
    return 15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x
        - vec3<f32>(0.00232);
}

fn tonemap_agx_vec(color: vec3<f32>) -> vec3<f32> {
    let inset = mat3x3<f32>(
        vec3<f32>(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        vec3<f32>(0.0784335999999992, 0.878468636469772, 0.0784336),
        vec3<f32>(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let outset = mat3x3<f32>(
        vec3<f32>(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        vec3<f32>(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        vec3<f32>(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    let min_ev = -12.47393;
    let max_ev = 4.026069;
    var v = inset * max(color, vec3<f32>(1.0e-10));
    v = clamp(log2(max(v, vec3<f32>(1.0e-10))), vec3<f32>(min_ev), vec3<f32>(max_ev));
    v = (v - vec3<f32>(min_ev)) / (max_ev - min_ev);
    v = agx_sigmoid(v);
    v = outset * v;
    return clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn display_look(display: vec3<f32>, contrast: f32, saturation: f32) -> vec3<f32> {
    let v = clamp(display, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 0.5 * pow(v * 2.0, vec3<f32>(contrast));
    let hi = vec3<f32>(1.0) - 0.5 * pow((vec3<f32>(1.0) - v) * 2.0, vec3<f32>(contrast));
    let curved = select(hi, lo, v < vec3<f32>(0.5));
    let luma = dot(curved, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(vec3<f32>(luma) + (curved - vec3<f32>(luma)) * saturation, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn default_display_transform(color: vec3<f32>) -> vec3<f32> {
    return display_look(tonemap_agx_vec(color), DEFAULT_LOOK_CONTRAST, DEFAULT_LOOK_SATURATION);
}
// END display-transform mirror

fn display_to_linear(display: vec3<f32>) -> vec3<f32> {
    return pow(max(display, vec3<f32>(0.0)), vec3<f32>(2.2));
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let len_n = length(in.normal);
    let n = select(vec3<f32>(0.0, 1.0, 0.0), in.normal / len_n, len_n > 1e-4);
    let sun_len = length(u.sun_dir_exposure.xyz);
    let sun_dir = select(vec3<f32>(0.0, 1.0, 0.0), u.sun_dir_exposure.xyz / sun_len, sun_len > 1e-4);
    // Two-sided. The collision winding points out of the tube; the camera looks in.
    let ndl = abs(dot(n, sun_dir));
    let sun_radiance = u.sun_color.rgb * u.sun_dir_exposure.w;
    var lit = u.albedo.rgb * (sun_radiance * ndl * INV_PI + u.ambient.rgb);
    lit = clamp(lit, vec3<f32>(0.0), vec3<f32>(1.0e4));
    // The target is Rgba8UnormSrgb (the hardware encodes): store linear display values.
    lit = display_to_linear(default_display_transform(lit));
    return vec4<f32>(lit, 1.0);
}
