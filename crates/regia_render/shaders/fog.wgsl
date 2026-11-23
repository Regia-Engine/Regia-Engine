// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::fog

// Exponential height fog (Hoffman & Preetham GDC 2002 / Filament atmospheric notes).
// params = (density, height_falloff, base_height, enabled)
// color  = (r, g, b, _)

fn fog_factor(distance: f32, world_y: f32, params: vec4<f32>) -> f32 {
    if (params.w < 0.5 || params.x <= 0.0 || distance <= 0.0) {
        return 0.0;
    }
    let height_density = params.x * exp(-params.y * (world_y - params.z));
    let optical = max(distance, 0.0) * max(height_density, 0.0);
    return 1.0 - exp(-optical);
}

fn apply_fog(scene_rgb: vec3<f32>, world_pos: vec3<f32>, camera_pos: vec3<f32>, params: vec4<f32>, color: vec4<f32>) -> vec3<f32> {
    let distance = length(world_pos - camera_pos);
    let t = fog_factor(distance, world_pos.y, params);
    return mix(scene_rgb, color.rgb, t);
}
