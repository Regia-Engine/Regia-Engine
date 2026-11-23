// SPDX-License-Identifier: MPL-2.0
// Sky: full-screen triangle with a procedural horizon-to-zenith gradient.
// Drawn before terrain (no depth write) so terrain depth-tests on top.

struct Frame {
    view_proj: mat4x4<f32>,
    camera_pos_time: vec4<f32>,
    sun_dir_wetness: vec4<f32>,
    // rgb = sun colour (linear), a = illuminance_lux * exposure (pre-multiplied)
    sun_color_exposure: vec4<f32>,
    // rgb = ambient colour (linear), a = debug lod flag
    ambient_debug: vec4<f32>,
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    light_view_proj: mat4x4<f32>,
    shadow_params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> frame: Frame;

struct VOut {
    @builtin(position) position: vec4<f32>,
    // Screen-space UV: x = 0..1 left-to-right, y = 0..1 bottom-to-top.
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle from vertex index: covers clip space [-1,1]².
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    let x = f32(i32(vi & 1u) * 4) - 1.0;  // -1, 3, -1
    let y = f32(i32(vi & 2u) * 2) - 1.0;  // -1, -1, 3
    let uv_x = (x + 1.0) * 0.5;
    let uv_y = (y + 1.0) * 0.5;

    return VOut(vec4<f32>(x, y, 0.0, 1.0), vec2<f32>(uv_x, uv_y));
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    // Simple vertical gradient: bottom (y=0) = ground/black, horizon (~y=0.45) = warm,
    // top (y=1) = zenith blue.
    let screen_y = in.uv.y;

    let zenith  = vec3<f32>(0.15, 0.25, 0.55);
    let horizon = vec3<f32>(0.78, 0.85, 0.95);
    let ground  = vec3<f32>(0.06, 0.05, 0.04);

    // Warm the horizon when the sun is low.
    let sun_el = frame.sun_dir_wetness.y;
    let warmth = vec3<f32>(0.15, 0.06, -0.08) * max(0.0, 0.5 - sun_el) * 1.8;
    let hor = horizon + warmth;

    var colour = hor;
    if (screen_y > 0.45) {
        // Upper half: horizon → zenith.
        let f = (screen_y - 0.45) / 0.55;
        let f2 = 1.0 - exp(-f * 3.0);
        colour = mix(hor, zenith, f2);
    } else {
        // Lower half: horizon → ground.
        let f = (0.45 - screen_y) / 0.45;
        let f2 = 1.0 - exp(-f * 2.0);
        colour = mix(hor, ground, f2);
    }

    return vec4<f32>(colour, 1.0);
}