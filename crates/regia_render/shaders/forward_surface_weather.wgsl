// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::surface_weather

// The surface response to the weather (spec 99 § 6.3): the same rule as
// `apply_surface_weather` in `surface_weather.rs`, run after the surface stage and before
// lighting, behind a uniform branch at the call site. No derivatives, fixed loops.

struct WeatherSurface {
    albedo: vec3<f32>,
    roughness: f32,
    metallic: f32,
    normal: vec3<f32>,
}

fn weather_hash_step(state: u32) -> u32 {
    let word = state * 747796405u + 2891336453u;
    let mixed = ((word >> ((word >> 28u) + 4u)) ^ word) * 277803737u;
    return (mixed >> 22u) ^ mixed;
}

fn weather_cell_hash(cell: vec2<i32>, salt: u32) -> vec3<f32> {
    let a = weather_hash_step(bitcast<u32>(cell.x) ^ weather_hash_step(bitcast<u32>(cell.y) ^ salt));
    let b = weather_hash_step(a);
    let c = weather_hash_step(b);
    return vec3<f32>(f32(a >> 8u), f32(b >> 8u), f32(c >> 8u)) / 16777216.0;
}

fn weather_value_noise(p: vec2<f32>) -> f32 {
    let cell = floor(p);
    let f = p - cell;
    let u = f * f * (3.0 - 2.0 * f);
    let i = vec2<i32>(cell);
    let a = weather_cell_hash(i, 0u).x;
    let b = weather_cell_hash(i + vec2<i32>(1, 0), 0u).x;
    let c = weather_cell_hash(i + vec2<i32>(0, 1), 0u).x;
    let d = weather_cell_hash(i + vec2<i32>(1, 1), 0u).x;
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn weather_puddle_key(xz: vec2<f32>) -> f32 {
    return smoothstep(0.2, 0.8, 0.35 * weather_value_noise(xz / 1.5) + 0.65 * weather_value_noise(xz / 6.0));
}

const WEATHER_RIPPLE_CELL_M: f32 = 0.4;
const WEATHER_RIPPLE_WAVELENGTH_M: f32 = 0.06;
const WEATHER_RIPPLE_HEIGHT_M: f32 = 0.0012;
const WEATHER_RIPPLE_TAU: f32 = 6.2831853;

fn weather_ripple_gradient(xz: vec2<f32>, clock_s: f32, rate: f32) -> vec2<f32> {
    var gradient = vec2<f32>(0.0);
    for (var grid = 0u; grid < 2u; grid += 1u) {
        let offset = f32(grid) * 0.37;
        let p = xz / WEATHER_RIPPLE_CELL_M + vec2<f32>(offset, offset * 1.7);
        let base = vec2<i32>(floor(p - 0.5));
        for (var j = 0; j < 2; j += 1) {
            for (var i = 0; i < 2; i += 1) {
                let cell = base + vec2<i32>(i, j);
                let first = weather_cell_hash(cell, grid + 1u);
                let period = 0.5 + 0.4 * first.x;
                let t = clock_s / period + first.y;
                let cycle = floor(t);
                let age = t - cycle;
                let second = weather_cell_hash(cell, bitcast<u32>(i32(cycle)) * 31u + grid);
                if (second.z >= rate) {
                    continue;
                }
                let center = vec2<f32>(cell) + 0.25 + 0.5 * second.xy;
                let delta = (p - center) * WEATHER_RIPPLE_CELL_M;
                let distance = length(delta);
                if (distance < 1.0e-5) {
                    continue;
                }
                let x = distance - 0.15 * age;
                let window = 1.0 - smoothstep(0.0, 0.05, abs(x));
                let fade = (1.0 - age) * (1.0 - age);
                let phase = WEATHER_RIPPLE_TAU * x / WEATHER_RIPPLE_WAVELENGTH_M;
                let slope = WEATHER_RIPPLE_HEIGHT_M * WEATHER_RIPPLE_TAU / WEATHER_RIPPLE_WAVELENGTH_M * cos(phase) * fade * window;
                gradient += slope * delta / distance;
            }
        }
    }
    return gradient;
}

// The default porosity: rough dielectrics soak, polished and metal surfaces do not.
fn weather_porosity(roughness: f32, metallic: f32) -> f32 {
    return (1.0 - clamp(metallic, 0.0, 1.0)) * clamp((roughness - 0.25) / 0.5, 0.0, 1.0);
}

// `weather` = (wetness, puddles, snow cover, ripple). `response` = (porosity, wet, snow, puddles 0/1).
// `exposure` = 1 under open sky, 0 under cover. `extra_key` raises the puddle key where the ground
// is concave (terrain); 0 elsewhere.
fn weather_apply(
    surface: WeatherSurface,
    n_geo: vec3<f32>,
    world: vec3<f32>,
    weather: vec4<f32>,
    clock_s: f32,
    response: vec4<f32>,
    exposure: f32,
    extra_key: f32,
) -> WeatherSurface {
    let x = clamp(exposure, 0.0, 1.0);
    if (x <= 0.0) {
        return surface;
    }
    // Plain locals, not a struct: a function-local struct is an explicitly laid out variable,
    // which SPIR-V validation refuses.
    let phi = clamp(response.x, 0.0, 1.0);
    let wet = clamp(weather.x * x * response.y, 0.0, 1.0);
    var albedo = surface.albedo * (1.0 - 0.5 * phi * wet);
    let film = wet * (1.0 - phi) + 0.6 * phi * smoothstep(0.7, 1.0, wet);
    var roughness = mix(surface.roughness, 0.08, 0.9 * film);
    var metallic = surface.metallic;
    var normal = normalize(mix(surface.normal, n_geo, 0.5 * film));
    let cover = clamp(weather.z * x * response.z, 0.0, 1.0);
    let lumps = weather_value_noise(world.xz / 0.7) - 0.5;
    let snow = cover * smoothstep(0.5, 0.85, n_geo.y + 0.15 * lumps);
    if (response.w > 0.5 && n_geo.y > 0.97) {
        let threshold = 1.0 - 0.6 * clamp(weather.y * x, 0.0, 1.0);
        let key = clamp(weather_puddle_key(world.xz) + extra_key, 0.0, 1.0);
        let pool = smoothstep(threshold, threshold + 0.05, key) * (1.0 - snow);
        if (pool > 0.0) {
            let gradient = weather_ripple_gradient(world.xz, clock_s, weather.w);
            let up = normalize(vec3<f32>(-gradient.x, 1.0, -gradient.y));
            albedo = mix(albedo, surface.albedo * (1.0 - 0.5 * phi), pool);
            roughness = mix(roughness, 0.02, pool);
            metallic = mix(metallic, 0.0, pool);
            normal = normalize(mix(normal, up, pool));
        }
    }
    if (snow > 0.0) {
        let bent = normalize(n_geo + vec3<f32>(0.0, 0.5, 0.0));
        normal = normalize(mix(mix(normal, n_geo, snow), bent, 0.5 * snow));
        albedo = mix(albedo, vec3<f32>(0.82), snow);
        roughness = mix(roughness, 0.8, snow);
        metallic = mix(metallic, 0.0, snow);
    }
    return WeatherSurface(albedo, roughness, metallic, normal);
}

// The rain's or snow's constant extinction over the view distance, in-scattering `inscatter`.
fn weather_extinction(scene_rgb: vec3<f32>, distance: f32, extinction_per_m: f32, inscatter: vec3<f32>) -> vec3<f32> {
    return mix(inscatter, scene_rgb, exp(-extinction_per_m * max(distance, 0.0)));
}
