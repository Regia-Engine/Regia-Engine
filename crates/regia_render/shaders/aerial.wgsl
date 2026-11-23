// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::aerial

#import regia::utils

// Minimal sky + aerial perspective for the default look (`src/aerial.rs`).
// The M15c atmosphere lane replaces this module; nothing else depends on its internals.
//
// - Sky: the procedural horizon/zenith gradient (the same function the IBL is baked
//   from, so the visible sky matches the light it casts) plus a sun aureole
//   (Henyey–Greenstein phase, Henyey & Greenstein 1941) and a sun disc.
// - Aerial perspective: exponential height fog with the optical depth integrated
//   along the view ray (Wenzel, "Real-time Atmospheric Effects in Games", SIGGRAPH
//   2006 course; Quilez, "Better fog", public article), in-scattering the horizon sky
//   radiance in the view direction plus a forward-scattering lobe toward the sun, so
//   distant terrain recedes into the sky colour instead of a flat grey.

const AERIAL_PI: f32 = 3.14159265359;

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = max(1.0 + g2 - 2.0 * g * cos_theta, 1e-4);
    return (1.0 - g2) / (4.0 * AERIAL_PI * denom * sqrt(denom));
}

// Sky radiance (cd/m²) toward `dir`. `to_sun` points at the sun; `sun_irradiance` is
// the sun's colour × illuminance (lux); `sky_scale` is `SKY_IBL_SCALE`.
fn sky_radiance(dir: vec3<f32>, to_sun: vec3<f32>, sun_irradiance: vec3<f32>, sky_scale: f32) -> vec3<f32> {
    let base = regia::utils::sky_color(dir) * sky_scale;
    let c = dot(dir, to_sun);
    // The ground hides the sun and its glow below the horizon.
    let above = smoothstep(-0.03, 0.02, dir.y);
    let aureole = sun_irradiance * 0.035 * henyey_greenstein(c, 0.8);
    // The real disc is ~1.5e9 cd/m² under a 100 klx sun; capped so bloom stays a glow.
    let disc = sun_irradiance * 3.0 * smoothstep(0.99985, 0.99996, c);
    return base + (aureole + disc) * above;
}

// Radiance in-scattered along a hazy view ray looking along `view_dir`.
fn haze_inscatter(view_dir: vec3<f32>, to_sun: vec3<f32>, sun_irradiance: vec3<f32>, sky_scale: f32) -> vec3<f32> {
    var flat_dir = vec3<f32>(view_dir.x, 0.0, view_dir.z);
    if (dot(flat_dir, flat_dir) < 1e-6) {
        flat_dir = vec3<f32>(1.0, 0.0, 0.0);
    }
    let horizon = normalize(normalize(flat_dir) + vec3<f32>(0.0, 0.06, 0.0));
    let sky = regia::utils::sky_color(horizon) * sky_scale;
    let glow = sun_irradiance * 0.02 * henyey_greenstein(dot(view_dir, to_sun), 0.6);
    return sky + glow;
}

// Share of the horizon sky term a closed cloud layer leaves (`HAZE_SKY_FLOOR`, clouds.rs): the
// sky's share of the light on flat ground under a clear sky, so the haze and the ground behind it
// fall together (ADR-5944).
const HAZE_SKY_FLOOR: f32 = 0.12;

// `haze_inscatter` for air that keeps `sun_lit` (0..1) of its sun under a cloud layer
// (ADR-5944): the sun glow falls with it, and so does the horizon sky term down to its floor.
// `sun_lit` is the mean of the cloud transmittance map along the view ray
// (`regia::cloud_shadow::cloud_sun_on_ray`).
fn haze_inscatter_under_clouds(
    view_dir: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
    sun_lit: f32,
) -> vec3<f32> {
    var flat_dir = vec3<f32>(view_dir.x, 0.0, view_dir.z);
    if (dot(flat_dir, flat_dir) < 1e-6) {
        flat_dir = vec3<f32>(1.0, 0.0, 0.0);
    }
    let lit = clamp(sun_lit, 0.0, 1.0);
    let horizon = normalize(normalize(flat_dir) + vec3<f32>(0.0, 0.06, 0.0));
    let sky = regia::utils::sky_color(horizon) * sky_scale * mix(HAZE_SKY_FLOOR, 1.0, lit);
    let glow = sun_irradiance * 0.02 * henyey_greenstein(dot(view_dir, to_sun), 0.6) * lit;
    return sky + glow;
}

// Optical depth of exponential height fog between the camera and `world`, integrated
// along the segment. params = (density at base height, height falloff, base height, enabled).
fn height_fog_optical_depth(camera: vec3<f32>, world: vec3<f32>, params: vec4<f32>) -> f32 {
    let distance = length(world - camera);
    let k = max(params.y, 0.0);
    let start = params.x * exp(-k * (camera.y - params.z));
    let dh = world.y - camera.y;
    var integral = 1.0;
    if (abs(k * dh) > 1e-4) {
        integral = (1.0 - exp(-k * dh)) / (k * dh);
    }
    return max(start * distance * integral, 0.0);
}

// Aerial perspective over a lit surface. `tint` is the fog colour normalised to unit
// luminance (FogSettings::color keeps its legacy meaning for the flat-colour mode).
fn apply_aerial(
    scene_rgb: vec3<f32>,
    world: vec3<f32>,
    camera: vec3<f32>,
    params: vec4<f32>,
    tint: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
) -> vec3<f32> {
    if (params.w < 0.5 || params.x <= 0.0) {
        return scene_rgb;
    }
    let view = world - camera;
    let len = length(view);
    if (len <= 0.0) {
        return scene_rgb;
    }
    let transmittance = exp(-height_fog_optical_depth(camera, world, params));
    let inscatter = haze_inscatter(view / len, to_sun, sun_irradiance, sky_scale) * tint;
    return mix(inscatter, scene_rgb, transmittance);
}

// `apply_aerial` under a cloud layer (ADR-5944): the haze between the camera and the surface
// keeps `sun_lit` of its sun (`haze_inscatter_under_clouds`). The extinction is the same:
// shadowed haze still hides what is behind it.
fn apply_aerial_under_clouds(
    scene_rgb: vec3<f32>,
    world: vec3<f32>,
    camera: vec3<f32>,
    params: vec4<f32>,
    tint: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
    sun_lit: f32,
) -> vec3<f32> {
    if (params.w < 0.5 || params.x <= 0.0) {
        return scene_rgb;
    }
    let view = world - camera;
    let len = length(view);
    if (len <= 0.0) {
        return scene_rgb;
    }
    let transmittance = exp(-height_fog_optical_depth(camera, world, params));
    let inscatter = haze_inscatter_under_clouds(view / len, to_sun, sun_irradiance, sky_scale, sun_lit) * tint;
    return mix(inscatter, scene_rgb, transmittance);
}
