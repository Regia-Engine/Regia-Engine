// SPDX-License-Identifier: MPL-2.0
#import regia::utils
#import regia::cloud_shadow

// Sky-view and aerial-perspective sampling. The tables are built on the CPU
// (Hillaire 2020) for the map's planet (Earth, Mars, airless). This shader looks
// them up and adds what the tables do not hold: the sun disc, stars, a lit sky
// body such as Jupiter seen from Europa, and the planet's surface on a ray that
// meets the ground (inscatter + transmittance × Lambertian radiance).

struct SkyGpu {
    inv_view_proj: mat4x4<f32>,
    // Toward the sun (xyz) and its angular radius (w, radians).
    sun: vec4<f32>,
    // Eye (xyz) and target width (w).
    camera: vec4<f32>,
    // Sun disc radiance (rgb, cd/m²) and an enable flag (w).
    disk: vec4<f32>,
    // x physical, y exposure (LDR), z encode LDR, w target height.
    misc: vec4<f32>,
    // Toward the sky body (xyz) and its angular radius (w, radians; 0 = none).
    body_dir: vec4<f32>,
    // Body albedo (rgb) and latitude-band strength (w).
    body_color: vec4<f32>,
    // x star radiance in daylight (cd/m²), y pixel angle (rad), z sun lux on the body, w airless.
    stars: vec4<f32>,
    // Two moons: direction (xyz) and angular radius (w, 0 = none), then tint (rgb) and a
    // textured flag (w = 1 samples that moon's albedo map instead of the tint).
    moon0_dir: vec4<f32>,
    moon0_color: vec4<f32>,
    moon1_dir: vec4<f32>,
    moon1_color: vec4<f32>,
    // x = 1 when the sky body samples `body_albedo`; y = the HDR target's pre-exposure
    // (`fs_sky_hdr` / `fs_aerial` write radiance × y); z = camera height above the
    // surface in kilometres (at least 0.001).
    body_tex: vec4<f32>,
    // Planet surface: rgb Lambertian radiance (cd/m²), w bottom radius (km). w ≤ 0
    // means the ray test never hits (the analytic sky does not use this).
    ground: vec4<f32>,
    // The `aerial_from_tables` switch (ADR-3294), read only by `fs_aerial_tables`,
    // `fs_aerial_tables_lite` and `fs_weather_veil` (the default entries never read it):
    // x = 1 full, 2 lite; y = the weather's extinction (1/m); z = how far a sky pixel's
    // rain reaches (m, the far plane); w = the drawn cloud deck's overcast factor (0..1).
    table_air: vec4<f32>,
    // The rain veil's constant part (ADR-3294, `rain_inscatter`): rgb = half the lit planet
    // ground and the transmitted sun over 4π, under the drawn deck (cd/m²); w unused. Read only
    // by `fs_weather_veil`.
    table_ground: vec4<f32>,
    // Up to four interior boxes (ADR-4757; the fog's `interiors` rows, `interior_slots`): center
    // (xyz, w = 1 used), then half extents (xyz). The air inside one takes no in-scatter from the
    // aerial composite (a roof is between it and the sky). All w = 0 leaves every entry as before.
    interiors: array<vec4<f32>, 8>,
    // The cloud transmittance map for the air under the layer (ADR-5944; `cloud_shadow.wgsl`):
    // extent (m), layer base y, layer top y, 1 when the aerial entries read it. All zero leaves
    // every entry as before.
    cloud: vec4<f32>,
    // x = the overcast factor the sky's share of the ground light takes under the layer
    // (ADR-321's rule; 0 leaves it). yzw unused. Read only under `cloud.w`.
    cloud_air: vec4<f32>,
    // The light on flat ground under a clear sky, the sun's share and the sky's (rgb, irradiance
    // / π; `ground_light`, atmosphere.rs): the air's multiple scattering follows it under the
    // layer (ADR-5944). w unused. Read only under `cloud.w`.
    cloud_ground_sun: vec4<f32>,
    cloud_ground_sky: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: SkyGpu;
@group(0) @binding(1) var sky_lut: texture_2d<f32>;
@group(0) @binding(2) var aerial_lut: texture_2d<f32>;
@group(0) @binding(3) var scene_depth: texture_depth_2d;
// Equirectangular albedo of the sky body (sRGB, decoded by the sampler).
@group(0) @binding(4) var body_albedo: texture_2d<f32>;
@group(0) @binding(5) var body_samp: sampler;
// Equirectangular albedo of each moon (sRGB, decoded by the sampler).
@group(0) @binding(6) var moon0_albedo: texture_2d<f32>;
@group(0) @binding(7) var moon1_albedo: texture_2d<f32>;
// The cloud transmittance map (ADR-320), or a 1×1 stand-in: loaded only under `u.cloud.w`.
@group(0) @binding(8) var cloud_map: texture_2d<f32>;
// The sun's single-scattering share of `aerial_lut`, in its layout (ADR-5944), or a 1×1
// stand-in: loaded only under `u.cloud.w`.
@group(0) @binding(9) var aerial_sun_lut: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
}

// Full-screen triangle. No function-local struct or array `var` (ADR-026).
@vertex
fn vs_fullscreen(@builtin(vertex_index) id: u32) -> VsOut {
    let x = select(-1.0, 3.0, id == 1u);
    let y = select(-1.0, 3.0, id == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

fn view_from_uv(uv: vec2<f32>) -> vec3<f32> {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let far_h = u.inv_view_proj * vec4<f32>(ndc, 1.0, 1.0);
    let near_h = u.inv_view_proj * vec4<f32>(ndc, 0.0, 1.0);
    return normalize(far_h.xyz / far_h.w - near_h.xyz / near_h.w);
}

fn sky_uv(view: vec3<f32>, sun: vec3<f32>) -> vec2<f32> {
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let sun_h = sun - up * dot(sun, up);
    let view_h = view - up * dot(view, up);
    let sun_n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(sun_h), length(sun_h) > 1e-4);
    let view_n = select(up, normalize(view_h), length(view_h) > 1e-4);
    let sin_az = dot(cross(sun_n, view_n), up);
    let cos_az = clamp(dot(sun_n, view_n), -1.0, 1.0);
    let azimuth = atan2(sin_az, cos_az);
    let uu = fract(azimuth / 6.28318530718 + 0.5);
    // Hillaire 2020 § 5.3 non-linear latitude: more rows near the horizon.
    let lat = asin(clamp(view.y, -1.0, 1.0));
    let vv = 0.5 + 0.5 * sign(lat) * sqrt(abs(lat) / 1.57079632679);
    return vec2<f32>(uu, clamp(vv, 0.0, 1.0));
}

fn analytic_sky(view: vec3<f32>) -> vec3<f32> {
    let t = view.y;
    let horizon = vec3<f32>(0.78, 0.85, 0.95);
    let zenith = vec3<f32>(0.15, 0.25, 0.55);
    let ground = vec3<f32>(0.12, 0.10, 0.08);
    if (t > 0.0) {
        let f = 1.0 - exp(-t * 3.0);
        return mix(horizon, zenith, f);
    }
    let f = 1.0 - exp(t * 2.0);
    return mix(horizon, ground, f);
}

fn sky_texel(x: i32, y: i32) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(sky_lut));
    let w = max(dims.x, 1);
    let h = max(dims.y, 1);
    var xi = x % w;
    if (xi < 0) {
        xi += w;
    }
    let yi = clamp(y, 0, h - 1);
    return textureLoad(sky_lut, vec2<i32>(xi, yi), 0);
}

fn sample_sky_lut(uv: vec2<f32>) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(sky_lut));
    let w = max(dims.x, 1.0);
    let h = max(dims.y, 1.0);
    let fu = fract(uv.x) * w - 0.5;
    let fv = clamp(uv.y, 0.0, 0.999) * h - 0.5;
    let x0 = i32(floor(fu));
    let y0 = i32(floor(fv));
    let tx = fu - floor(fu);
    let ty = fv - floor(fv);
    let a = sky_texel(x0, y0);
    let b = sky_texel(x0 + 1, y0);
    let c = sky_texel(x0, y0 + 1);
    let d = sky_texel(x0 + 1, y0 + 1);
    let row0 = mix(a, b, tx);
    let row1 = mix(c, d, tx);
    // rgb is inscatter. a is transmittance to the planet, or 0 when the ray misses.
    return mix(row0, row1, ty);
}

// True when `view` meets the planet. A hard test, not the LUT alpha: bilinear filtering
// would leak a ground texel into a ray that still reaches space. Matches `ray_sphere`
// (epsilon 1e-4 km) with the camera on the +Y axis at `body_tex.z` above the surface.
fn ray_hits_planet(view: vec3<f32>) -> bool {
    let radius = u.ground.w;
    if (radius <= 0.0) {
        return false;
    }
    let cam_r = radius + max(u.body_tex.z, 0.001);
    let b = cam_r * view.y;
    let c = cam_r * cam_r - radius * radius;
    let disc = b * b - c;
    if (disc < 0.0) {
        return false;
    }
    let s = sqrt(disc);
    let t1 = -b - s;
    let t2 = -b + s;
    let t = select(t2, t1, t1 > 1.0e-4);
    return t > 1.0e-4;
}

// Integer mix in the style of Jarzynski & Olano, JCGT 2020. One value per cell,
// so a star covers the whole cell instead of a sub-pixel speck.
fn star_hash(cell: vec3<i32>) -> f32 {
    var x = bitcast<u32>(cell.x);
    x = x ^ (bitcast<u32>(cell.y) * 0x9E3779B9u);
    x = x ^ (bitcast<u32>(cell.z) * 0x85EBCA6Bu);
    x = x * 747796405u + 2891336453u;
    x = ((x >> ((x >> 28u) + 4u)) ^ x) * 277803737u;
    x = (x >> 22u) ^ x;
    return f32(x) * (1.0 / 4294967296.0);
}

fn hash01(cell: vec3<i32>, salt: u32) -> f32 {
    return star_hash(cell + vec3<i32>(i32(salt) * 7919, i32(salt) * 104729, i32(salt) * 1299709));
}

// Point stars on a 3D direction grid (≈0.23° cells, about one in thirty holds a star).
// Each star is a Gaussian a pixel wide, with a power-law brightness and a colour
// between blue-white and orange.
fn star_field(view: vec3<f32>, peak: f32) -> vec3<f32> {
    let scale = 250.0;
    let p = view * scale;
    let cell = vec3<i32>(floor(p));
    let h = star_hash(cell);
    if (h < 0.965) {
        return vec3<f32>(0.0);
    }
    let offset = vec3<f32>(hash01(cell, 1u), hash01(cell, 2u), hash01(cell, 3u)) * 0.5 + 0.25;
    let centre = normalize((vec3<f32>(cell) + offset) / scale);
    let px = max(u.stars.y, 1e-5);
    let d = length(view - centre);
    let r = px * 0.7;
    let shape = exp(-(d * d) / (r * r));
    let mag = (h - 0.965) / 0.035;
    // `body_tex.w` > 0: the star-flux gain (ADR-2486), (height / 1080)^2, so a star a pixel wide keeps its light
    // at every output height.
    let gain = select(1.0, u.body_tex.w, u.body_tex.w > 0.0);
    let bright = peak * (pow(mag, 6.0) + 0.006) * gain;
    let warm = hash01(cell, 4u);
    let tint = mix(vec3<f32>(0.80, 0.88, 1.10), vec3<f32>(1.10, 0.92, 0.72), warm);
    return tint * (bright * shape);
}

// Moonlit air only. Hillaire's tables have no moon. Drawn while the sun is below
// the horizon and the view is above the star cutoff, including on a ray that
// meets the ground: this glow is the air between the camera and the surface.
fn night_air(view: vec3<f32>, radiance: vec3<f32>) -> vec3<f32> {
    if (u.stars.w > 0.5 || u.sun.y >= -0.02 || view.y <= -0.02) {
        return radiance;
    }
    let elev = max(view.y, 0.0);
    let horizon = exp(-elev * 7.0);
    var out = radiance + vec3<f32>(0.45, 0.62, 1.15) * (600.0 * horizon);
    let flat = vec3<f32>(-u.sun.x, 0.0, -u.sun.z);
    let az = select(vec3<f32>(0.0, 0.0, 1.0), normalize(flat), length(flat) > 1e-3);
    let moon = normalize(az * 0.98 + vec3<f32>(0.0, 0.22, 0.0));
    let lobe = pow(max(dot(view, moon), 0.0), 12.0);
    out += vec3<f32>(0.55, 0.70, 1.15) * (900.0 * lobe * max(horizon, 0.35));
    return out;
}

// Stars sit on the celestial sphere, so a planet hit does not call this.
// An airless sky shows them at any hour; a sky with air only after sunset.
fn add_night(view: vec3<f32>, radiance: vec3<f32>) -> vec3<f32> {
    if (u.stars.w > 0.5) {
        if (view.y <= -0.02) {
            return radiance;
        }
        return radiance + star_field(view, u.stars.x);
    }
    let air = night_air(view, radiance);
    if (u.sun.y >= -0.02 || view.y <= -0.02) {
        return air;
    }
    return air + star_field(view, 60000.0);
}

// A lit sphere of angular radius `dir_radius.w` toward `dir_radius.xyz`: Lambert from
// the sun (so its phase follows the sun direction), mild limb darkening. `bands` > 0
// adds gas-giant latitude bands with a little turbulence; `textured` samples `albedo_tex`
// (the body's or a moon's equirectangular albedo map) instead. Returns radiance and
// pixel coverage.
fn lit_sphere(view: vec3<f32>, dir_radius: vec4<f32>, tint: vec3<f32>, bands: f32, textured: bool, albedo_tex: texture_2d<f32>) -> vec4<f32> {
    let radius = dir_radius.w;
    if (radius <= 0.0) {
        return vec4<f32>(0.0);
    }
    let c = normalize(dir_radius.xyz);
    let cos_v = clamp(dot(view, c), -1.0, 1.0);
    let ang = acos(cos_v);
    let px = max(u.stars.y, 1e-5);
    let coverage = 1.0 - smoothstep(radius - px, radius + px, ang);
    if (coverage <= 0.0) {
        return vec4<f32>(0.0);
    }
    // Unit view ray against a sphere at distance 1 with radius sin(radius).
    let sr = sin(radius);
    let disc = max(cos_v * cos_v - (1.0 - sr * sr), 0.0);
    let t = cos_v - sqrt(disc);
    let n = normalize(view * t - c);
    let up = vec3<f32>(0.0, 1.0, 0.0);
    var pole = up - c * dot(up, c);
    pole = select(vec3<f32>(1.0, 0.0, 0.0), normalize(pole), length(pole) > 1e-3);
    let east = normalize(cross(pole, c));
    let lat = dot(n, pole);
    let lon = atan2(dot(n, east), dot(n, -c));
    var albedo = tint;
    if (textured) {
        // Longitude 0 faces the viewer at the image centre; the north pole is the top row.
        let uv = vec2<f32>(fract(lon / 6.28318530718 + 0.5), 0.5 - asin(clamp(lat, -1.0, 1.0)) / 3.14159265359);
        albedo = tint * textureSampleLevel(albedo_tex, body_samp, uv, 0.0).rgb;
    } else if (bands > 0.0) {
        let swirl = sin(lon * 3.0 + lat * 41.0) * 0.5 + sin(lon * 7.0 - lat * 23.0) * 0.25;
        let band = 0.5 + 0.5 * sin(lat * 19.0 + 1.6 * sin(lat * 6.0) + 0.35 * swirl);
        let zone = vec3<f32>(1.04, 0.98, 0.88);
        let belt = vec3<f32>(0.72, 0.52, 0.36);
        let banded = mix(belt, zone, band);
        let polar = mix(vec3<f32>(1.0), vec3<f32>(0.78, 0.76, 0.74), smoothstep(0.75, 0.95, abs(lat)));
        albedo = tint * mix(vec3<f32>(1.0), banded * polar, clamp(bands, 0.0, 1.0));
    }
    let ndl = max(dot(n, u.sun.xyz), 0.0);
    let limb = pow(max(dot(n, -view), 0.0), 0.2);
    let radiance = albedo * (u.stars.z * ndl * limb / 3.14159265359);
    return vec4<f32>(radiance, coverage);
}

fn sky_body(view: vec3<f32>) -> vec4<f32> {
    return lit_sphere(view, u.body_dir, u.body_color.rgb, u.body_color.w, u.body_tex.x > 0.5, body_albedo);
}

// Disc a pixel wide at least: a 0.05° sun (Europa) would fall between pixels. The
// radiance is scaled by the area ratio so the disc's energy stays the same. Linear
// limb darkening (u = 0.6, Neckel & Labs 1994).
fn sun_disc(view: vec3<f32>) -> vec3<f32> {
    if (u.disk.w < 0.5 || view.y <= -0.01) {
        return vec3<f32>(0.0);
    }
    let px = max(u.stars.y, 1e-5);
    let true_r = max(u.sun.w, 1e-6);
    let r = max(true_r, px * 1.2);
    let ang = acos(clamp(dot(view, u.sun.xyz), -1.0, 1.0));
    if (ang > r + px) {
        return vec3<f32>(0.0);
    }
    let edge = 1.0 - smoothstep(r - px * 0.5, r + px * 0.5, ang);
    let x = clamp(ang / r, 0.0, 1.0);
    let limb = 1.0 - 0.6 * (1.0 - sqrt(max(1.0 - x * x, 0.0)));
    let area = (true_r * true_r) / (r * r);
    return u.disk.xyz * (edge * limb * area);
}

fn physical_sky(view: vec3<f32>) -> vec3<f32> {
    let uv = sky_uv(view, u.sun.xyz);
    let sky = sample_sky_lut(uv);
    // The planet hides the celestial sphere. A few metres of air in front of the
    // surface scatter almost nothing, so the RGB table alone is black there.
    // The moonlit glow is that air, so it stays; stars do not.
    if (ray_hits_planet(view)) {
        if (u.cloud.w > 0.5) {
            // The planet's surface beyond the terrain is under the cloud layer too (ADR-5944).
            let hit_m = planet_hit_km(view) * 1000.0;
            return night_air(view, planet_air_under_clouds(view, hit_m, sky.rgb))
                + clamp(sky.a, 0.0, 1.0) * planet_ground_under_clouds(view, hit_m);
        }
        return night_air(view, sky.rgb) + clamp(sky.a, 0.0, 1.0) * u.ground.xyz;
    }
    var radiance = add_night(view, sky.rgb);
    // Moons first: the main body (Jupiter) is drawn over them where they overlap.
    let moon0 = lit_sphere(view, u.moon0_dir, u.moon0_color.rgb, 0.0, u.moon0_color.w > 0.5, moon0_albedo);
    radiance = mix(radiance, sky.rgb + moon0.rgb, moon0.a);
    let moon1 = lit_sphere(view, u.moon1_dir, u.moon1_color.rgb, 0.0, u.moon1_color.w > 0.5, moon1_albedo);
    radiance = mix(radiance, sky.rgb + moon1.rgb, moon1.a);
    let body = sky_body(view);
    // The body hides the stars behind it; the air in front of it still scatters.
    radiance = mix(radiance, sky.rgb + body.rgb, body.a);
    let covered = max(body.a, max(moon0.a, moon1.a));
    radiance += sun_disc(view) * (1.0 - covered);
    return radiance;
}

const AERIAL_SLICES: i32 = 16;
// Slice i is the integral out to (i + 1) * AERIAL_STEP_KM.
const AERIAL_STEP_KM: f32 = 2.0;

fn aerial_fetch(x: i32, y: i32, slice: i32, w: i32, h: i32) -> vec4<f32> {
    var xi = x % w;
    if (xi < 0) {
        xi += w;
    }
    let yi = clamp(y, 0, h - 1);
    return textureLoad(aerial_lut, vec2<i32>(xi + slice * w, yi), 0);
}

// Bilinear inside one slice: azimuth wraps, latitude clamps.
fn aerial_texel(uv: vec2<f32>, slice: i32) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(aerial_lut));
    let w = max(dims.x / AERIAL_SLICES, 1);
    let h = max(dims.y, 1);
    let fu = fract(uv.x) * f32(w) - 0.5;
    let fv = clamp(uv.y, 0.0, 1.0) * f32(h) - 0.5;
    let x0 = i32(floor(fu));
    let y0 = i32(floor(fv));
    let tx = fu - floor(fu);
    let ty = fv - floor(fv);
    let a = aerial_fetch(x0, y0, slice, w, h);
    let b = aerial_fetch(x0 + 1, y0, slice, w, h);
    let c = aerial_fetch(x0, y0 + 1, slice, w, h);
    let d = aerial_fetch(x0 + 1, y0 + 1, slice, w, h);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

// Below the first slice, blend from clear air so a nearby surface is not coloured
// as if it were a whole slice away.
fn aerial_at(view: vec3<f32>, dist_m: f32) -> vec4<f32> {
    let uv = sky_uv(view, u.sun.xyz);
    let pos = clamp((dist_m / 1000.0) / AERIAL_STEP_KM, 0.0, f32(AERIAL_SLICES));
    if (pos <= 1.0) {
        return mix(vec4<f32>(0.0, 0.0, 0.0, 1.0), aerial_texel(uv, 0), pos);
    }
    let f = pos - 1.0;
    let s0 = min(i32(floor(f)), AERIAL_SLICES - 1);
    let s1 = min(s0 + 1, AERIAL_SLICES - 1);
    let t = f - f32(s0);
    return mix(aerial_texel(uv, s0), aerial_texel(uv, s1), t);
}

fn world_distance(uv: vec2<f32>, depth: f32, eye: vec3<f32>) -> f32 {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
    return length(h.xyz / h.w - eye);
}

// ---------------------------------------------------------------------------------------------
// Interior volumes (ADR-4757). The aerial tables hold the in-scatter of the air between the eye
// and a point on the view ray, integrated from the eye: L(0 → b) − L(0 → a) is what the stretch
// [a, b] adds (Hillaire 2020 § 5.4; single scattering along a fixed ray). Air inside an interior
// box is under a roof, so its stretch adds nothing: the composite subtracts it. The light that
// reaches the eye from beyond still crosses that air, so the transmittance is unchanged. The
// stretch is a continuous function of the eye, so walking out of a door or a cave has no pop: the
// span shrinks to zero at the box's face. A ray that crosses no box takes the old path exactly.
// ---------------------------------------------------------------------------------------------

// Spans shorter than this (m) are not subtracted (a ray grazing a box's face).
const INTERIOR_MIN_SPAN_M: f32 = 1.0e-3;
// An empty span: sorts after every real one.
const INTERIOR_NO_SPAN: vec2<f32> = vec2<f32>(3.0e38, 3.0e38);

// The stretch [t0, t1] of the ray `eye + view t`, t in [0, dist], inside interior slot `i`
// (slab test, Kay & Kajiya 1986), or INTERIOR_NO_SPAN.
fn interior_span(eye: vec3<f32>, view: vec3<f32>, dist: f32, i: i32) -> vec2<f32> {
    let head = u.interiors[i * 2];
    if (head.w < 0.5) {
        return INTERIOR_NO_SPAN;
    }
    let half = u.interiors[i * 2 + 1].xyz;
    var t0 = 0.0;
    var t1 = dist;
    for (var a = 0; a < 3; a++) {
        let lo = head[a] - half[a] - eye[a];
        let hi = head[a] + half[a] - eye[a];
        let d = view[a];
        if (abs(d) < 1.0e-9) {
            if (lo > 0.0 || hi < 0.0) {
                return INTERIOR_NO_SPAN;
            }
        } else {
            let ta = lo / d;
            let tb = hi / d;
            t0 = max(t0, min(ta, tb));
            t1 = min(t1, max(ta, tb));
        }
    }
    if (t1 - t0 < INTERIOR_MIN_SPAN_M) {
        return INTERIOR_NO_SPAN;
    }
    return vec2<f32>(t0, t1);
}

// Swaps `a` and `b` so `a` starts first.
fn interior_order(a: ptr<function, vec2<f32>>, b: ptr<function, vec2<f32>>) {
    if ((*b).x < (*a).x) {
        let t = *a;
        *a = *b;
        *b = t;
    }
}

// What the composite's air function gives at `t` (m along the ray): entry 0 `aerial_at`, 1
// `tables_air` full, 2 `tables_air` lite. `at_dist` is its value at the surface, already computed. Entry 3 measures the length only (the veil).
fn interior_air(view: vec3<f32>, t: f32, dist: f32, at_dist: vec3<f32>, entry: i32) -> vec3<f32> {
    if (entry >= 3) {
        return vec3<f32>(0.0);
    }
    if (t >= dist) {
        return at_dist;
    }
    if (t <= 0.0) {
        return vec3<f32>(0.0);
    }
    if (entry == 0) {
        return aerial_at(view, t).rgb;
    }
    return tables_air(view, t, entry == 2).rgb;
}

// The in-scatter the merged spans add (rgb) and their total length (w).
fn interior_removed(view: vec3<f32>, s: vec2<f32>, dist: f32, at_dist: vec3<f32>, entry: i32) -> vec4<f32> {
    if (s.x >= INTERIOR_NO_SPAN.x) {
        return vec4<f32>(0.0);
    }
    let rgb = interior_air(view, s.y, dist, at_dist, entry) - interior_air(view, s.x, dist, at_dist, entry);
    return vec4<f32>(max(rgb, vec3<f32>(0.0)), s.y - s.x);
}

// The interior air on the ray to a surface `dist` away: rgb the in-scatter it would add, w its
// length (m). All zero when the ray crosses no box (the caller then keeps its value bit for bit).
fn interior_air_on_ray(view: vec3<f32>, dist: f32, at_dist: vec3<f32>, entry: i32) -> vec4<f32> {
    let eye = u.camera.xyz;
    var s0 = interior_span(eye, view, dist, 0);
    var s1 = interior_span(eye, view, dist, 1);
    var s2 = interior_span(eye, view, dist, 2);
    var s3 = interior_span(eye, view, dist, 3);
    if (min(min(s0.x, s1.x), min(s2.x, s3.x)) >= INTERIOR_NO_SPAN.x) {
        return vec4<f32>(0.0);
    }
    // Sort the four spans by start (a five-swap network), then merge overlapping ones so a stretch
    // inside two boxes is subtracted once.
    interior_order(&s0, &s1);
    interior_order(&s2, &s3);
    interior_order(&s0, &s2);
    interior_order(&s1, &s3);
    interior_order(&s1, &s2);
    var out = vec4<f32>(0.0);
    var cur = s0;
    if (s1.x <= cur.y) {
        cur.y = max(cur.y, s1.y);
    } else {
        out += interior_removed(view, cur, dist, at_dist, entry);
        cur = s1;
    }
    if (s2.x <= cur.y) {
        cur.y = max(cur.y, s2.y);
    } else {
        out += interior_removed(view, cur, dist, at_dist, entry);
        cur = s2;
    }
    if (s3.x <= cur.y) {
        cur.y = max(cur.y, s3.y);
    } else {
        out += interior_removed(view, cur, dist, at_dist, entry);
        cur = s3;
    }
    out += interior_removed(view, cur, dist, at_dist, entry);
    return out;
}

// `air` (rgb in-scatter, a transmittance) with the interior stretches' in-scatter taken out.
fn without_interior_air(view: vec3<f32>, dist: f32, air: vec4<f32>, entry: i32) -> vec4<f32> {
    let inside = interior_air_on_ray(view, dist, air.rgb, entry);
    if (inside.w <= 0.0) {
        return air;
    }
    return vec4<f32>(max(air.rgb - inside.rgb, vec3<f32>(0.0)), air.a);
}

// ---------------------------------------------------------------------------------------------
// The air under a cloud layer (ADR-5944). The aerial tables are built for a clear sky. With the
// cloud transmittance map on (ADR-320) the sun reaches a point of the air through the layer, as
// it reaches a surface, so the stretch of the view ray under a cloud loses the sun's single
// scattering: Σ (S(b) − S(a)) (1 − V) over the table's segments, S the sun's share
// (`aerial_sun_lut`, linear between nodes 2 km apart, node 0 the eye) and V the map's mean at a
// few midpoints of the segment (Bruneton & Neyret 2008 § 5 take the shadowed stretch out of the
// in-scatter integral the same way; Hillaire 2020 § 5.5 gives the two terms). The multiple
// scattering (the light of the sky and of the lit ground around, scattered again) follows the
// light on the ground under the layer, so under one cover the air and the ground behind it fall
// together (Koschmieder 1924). Where the map says 1 nothing is removed.
// ---------------------------------------------------------------------------------------------

fn aerial_sun_fetch(x: i32, y: i32, slice: i32, w: i32, h: i32) -> vec3<f32> {
    var xi = x % w;
    if (xi < 0) {
        xi += w;
    }
    let yi = clamp(y, 0, h - 1);
    return textureLoad(aerial_sun_lut, vec2<i32>(xi + slice * w, yi), 0).rgb;
}

// `aerial_texel`'s filter over the sun table.
fn aerial_sun_texel(uv: vec2<f32>, slice: i32) -> vec3<f32> {
    let dims = vec2<i32>(textureDimensions(aerial_sun_lut));
    let w = max(dims.x / AERIAL_SLICES, 1);
    let h = max(dims.y, 1);
    let fu = fract(uv.x) * f32(w) - 0.5;
    let fv = clamp(uv.y, 0.0, 1.0) * f32(h) - 0.5;
    let x0 = i32(floor(fu));
    let y0 = i32(floor(fv));
    let tx = fu - floor(fu);
    let ty = fv - floor(fv);
    let a = aerial_sun_fetch(x0, y0, slice, w, h);
    let b = aerial_sun_fetch(x0 + 1, y0, slice, w, h);
    let c = aerial_sun_fetch(x0, y0 + 1, slice, w, h);
    let d = aerial_sun_fetch(x0 + 1, y0 + 1, slice, w, h);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

// Node `k` of the sun table along a ray: node 0 is the eye, node k is slice k − 1.
fn aerial_sun_node(uv: vec2<f32>, k: i32) -> vec3<f32> {
    if (k <= 0) {
        return vec3<f32>(0.0);
    }
    return aerial_sun_texel(uv, min(k, AERIAL_SLICES) - 1);
}

// The sun's share of `aerial_at` at `dist_m` (the same interpolation).
fn aerial_sun_at(uv: vec2<f32>, dist_m: f32) -> vec3<f32> {
    let pos = clamp((dist_m / 1000.0) / AERIAL_STEP_KM, 0.0, f32(AERIAL_SLICES));
    let k = min(i32(floor(pos)), AERIAL_SLICES - 1);
    return mix(aerial_sun_node(uv, k), aerial_sun_node(uv, k + 1), pos - f32(k));
}

// Map samples in the n-th table segment a ray crosses under the layer: 8, 4, then 2
// (`AERIAL_CLOUD_STEPS`, atmosphere.rs).
fn cloud_air_steps(n: i32) -> i32 {
    if (n <= 0) {
        return 8;
    }
    if (n == 1) {
        return 4;
    }
    return 2;
}

// Inside an interior box (ADR-4757): `without_interior_air` already takes that air out whole.
fn in_interior(p: vec3<f32>) -> bool {
    for (var i = 0; i < 4; i++) {
        let head = u.interiors[i * 2];
        if (head.w > 0.5 && all(abs(p - head.xyz) <= u.interiors[i * 2 + 1].xyz)) {
            return true;
        }
    }
    return false;
}

// The in-scatter the cloud layer takes out of `table` (the air function's rgb toward a surface
// `dist` m along `view`): the sun's share of the shadowed stretches, and what the multiple
// scattering loses when the light on the ground under the layer falls (the sun's share of that
// light by the shadowed share of the ray's air; the sky's share by the overcast rule of ADR-321
// for the part of the ray below the layer's top). Zero when `u.cloud.w` is 0 (no load of the map
// or the sun table). Mirrors `sample_aerial_under_clouds` (atmosphere.rs).
fn cloud_air_removed(view: vec3<f32>, dist: f32, table: vec3<f32>) -> vec3<f32> {
    if (u.cloud.w < 0.5) {
        return vec3<f32>(0.0);
    }
    let eye = u.camera.xyz;
    let to_sun = u.sun.xyz;
    let span = regia::cloud_shadow::cloud_air_span(u.cloud, eye, view, dist, to_sun);
    let uv = sky_uv(view, to_sun);
    let step_m = AERIAL_STEP_KM * 1000.0;
    var removed = vec3<f32>(0.0);
    let a = max(span.x, 0.0);
    let b = min(span.y, min(dist, step_m * f32(AERIAL_SLICES)));
    if (b > a) {
        let pos_a = a / step_m;
        let pos_b = b / step_m;
        let first = min(i32(floor(pos_a)), AERIAL_SLICES - 1);
        var lo_node = aerial_sun_node(uv, first);
        for (var k = first; k < AERIAL_SLICES; k++) {
            if (f32(k) >= pos_b) {
                break;
            }
            let hi_node = aerial_sun_node(uv, k + 1);
            let lo = max(pos_a, f32(k));
            let hi = min(pos_b, f32(k) + 1.0);
            if (hi > lo) {
                let n = cloud_air_steps(k - first);
                var dark = 0.0;
                for (var i = 0; i < n; i++) {
                    let p = eye + view * ((lo + (hi - lo) * (f32(i) + 0.5) / f32(n)) * step_m);
                    if (!in_interior(p)) {
                        let lit = regia::cloud_shadow::cloud_sun_transmittance(cloud_map, u.cloud, p, to_sun, eye);
                        dark += 1.0 - clamp(lit, 0.0, 1.0);
                    }
                }
                removed += (hi_node - lo_node) * ((hi - lo) * dark / f32(n));
            }
            lo_node = hi_node;
        }
    }
    let luma = vec3<f32>(0.2126, 0.7152, 0.0722);
    let lost = dot(removed, luma);
    let o = clamp(u.cloud_air.x, 0.0, 1.0);
    if ((lost > 0.0 || o > 0.0) && span.z > 0.0) {
        let sun_all = aerial_sun_at(uv, dist);
        // The shadowed share of the ray's air, weighted as the air scatters the sun.
        let shade = clamp(lost / max(dot(sun_all, luma), 1e-6), 0.0, 1.0);
        var sky = u.cloud_ground_sky.rgb;
        if (o > 0.0) {
            let dimmed = mix(sky, vec3<f32>(dot(sky, luma)), o) * (1.0 - TABLE_OVERCAST_DIM * o);
            sky = mix(sky, dimmed, min(span.z, 1.0));
        }
        let clear = u.cloud_ground_sun.rgb + u.cloud_ground_sky.rgb;
        // What the ground under the layer keeps of its clear light, per channel (1 where there
        // is no light to compare).
        let under = u.cloud_ground_sun.rgb * (1.0 - shade) + sky;
        let keep = select(vec3<f32>(1.0), clamp(under / max(clear, vec3<f32>(1e-6)), vec3<f32>(0.0), vec3<f32>(4.0)), clear > vec3<f32>(1e-6));
        removed += max(table - sun_all, vec3<f32>(0.0)) * (vec3<f32>(1.0) - keep);
    }
    return removed;
}

// `air` (the composite's in-scatter after the interiors, a transmittance) under the cloud layer;
// `table` is the air function's rgb before the interiors. Unchanged, bit for bit, with the map
// off. The transmittance stays: shadowed air still absorbs.
fn under_clouds(view: vec3<f32>, dist: f32, air: vec4<f32>, table: vec3<f32>) -> vec4<f32> {
    if (u.cloud.w < 0.5) {
        return air;
    }
    return vec4<f32>(max(air.rgb - cloud_air_removed(view, dist, table), vec3<f32>(0.0)), air.a);
}

// Kilometres along `view` to the planet, or −1: `ray_hits_planet`'s roots.
fn planet_hit_km(view: vec3<f32>) -> f32 {
    let radius = u.ground.w;
    if (radius <= 0.0) {
        return -1.0;
    }
    let cam_r = radius + max(u.body_tex.z, 0.001);
    let b = cam_r * view.y;
    let c = cam_r * cam_r - radius * radius;
    let disc = b * b - c;
    if (disc < 0.0) {
        return -1.0;
    }
    let s = sqrt(disc);
    let t1 = -b - s;
    let t2 = -b + s;
    let t = select(t2, t1, t1 > 1.0e-4);
    return select(-1.0, t, t > 1.0e-4);
}

// The air in front of the planet's surface `hit_m` metres along `view`, under the cloud layer
// (ADR-5944): the sky table's in-scatter toward the hit with the layer's share taken out, as for
// a surface at that distance (`cloud_air_removed`). The share is measured on the aerial table;
// the sky table's value for the same path can be smaller (two integrations of one path), so
// the removal is scaled to it and never takes more than its share. `inscatter` unchanged with
// the map off.
fn planet_air_under_clouds(view: vec3<f32>, hit_m: f32, inscatter: vec3<f32>) -> vec3<f32> {
    if (u.cloud.w < 0.5 || hit_m <= 0.0) {
        return inscatter;
    }
    let table = aerial_at(view, hit_m).rgb;
    let removed = cloud_air_removed(view, hit_m, table);
    let scale = min(inscatter / max(table, vec3<f32>(1e-6)), vec3<f32>(1.0));
    return max(inscatter - removed * scale, vec3<f32>(0.0));
}

// The planet's surface `hit_m` metres along `view` under the cloud layer (ADR-5944): the sun's
// share of its light times the map's value at the hit, the sky's share under the overcast rule,
// as the terrain in front of it is lit. `u.ground` unchanged with the map off.
fn planet_ground_under_clouds(view: vec3<f32>, hit_m: f32) -> vec3<f32> {
    if (u.cloud.w < 0.5 || hit_m <= 0.0) {
        return u.ground.xyz;
    }
    let eye = u.camera.xyz;
    let lit = clamp(regia::cloud_shadow::cloud_sun_transmittance(cloud_map, u.cloud, eye + view * hit_m, u.sun.xyz, eye), 0.0, 1.0);
    var sky = u.cloud_ground_sky.rgb;
    let o = clamp(u.cloud_air.x, 0.0, 1.0);
    if (o > 0.0) {
        sky = mix(sky, vec3<f32>(dot(sky, vec3<f32>(0.2126, 0.7152, 0.0722))), o) * (1.0 - TABLE_OVERCAST_DIM * o);
    }
    let clear = u.cloud_ground_sun.rgb + u.cloud_ground_sky.rgb;
    let keep = select(vec3<f32>(1.0), clamp((u.cloud_ground_sun.rgb * lit + sky) / max(clear, vec3<f32>(1e-6)), vec3<f32>(0.0), vec3<f32>(4.0)), clear > vec3<f32>(1e-6));
    return u.ground.xyz * keep;
}

fn encode_radiance(radiance: vec3<f32>) -> vec4<f32> {
    if (u.misc.z > 0.5) {
        let mapped = regia::utils::tonemap_agx_vec(radiance * u.misc.y);
        return vec4<f32>(regia::utils::encode_srgb(mapped), 1.0);
    }
    return vec4<f32>(radiance, 1.0);
}

// Equirectangular panorama (tests): x is azimuth clockwise from north (+Z) through east
// (+X), y is elevation from +90° at the top to -10° at the bottom.
fn panorama_view(uv: vec2<f32>) -> vec3<f32> {
    let az = uv.x * 6.28318530718;
    let el = radians(90.0 - uv.y * 100.0);
    return vec3<f32>(sin(az) * cos(el), sin(el), cos(az) * cos(el));
}

@fragment
fn fs_sky_ldr(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.pos.xy / max(vec2<f32>(u.camera.w, u.misc.w), vec2<f32>(1.0));
    let view = select(view_from_uv(uv), panorama_view(uv), u.misc.x > 1.5);
    if (u.misc.x < 0.5) {
        return vec4<f32>(regia::utils::encode_srgb(analytic_sky(view)), 1.0);
    }
    return encode_radiance(physical_sky(view));
}

@fragment
fn fs_sky_hdr(in: VsOut) -> @location(0) vec4<f32> {
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    // Clear is exactly 1. A sky dome inside the far plane projects to ~0.9999
    // and must keep the colour the forward pass already wrote.
    if (depth < 1.0) {
        discard;
    }
    let uv = in.pos.xy / max(vec2<f32>(u.camera.w, u.misc.w), vec2<f32>(1.0));
    let view = view_from_uv(uv);
    return vec4<f32>(physical_sky(view) * hdr_scale(), 1.0);
}

// Pre-exposure of the HDR target (0 from an older uniform reads as 1).
fn hdr_scale() -> f32 {
    return select(1.0, u.body_tex.y, u.body_tex.y > 0.0);
}

@fragment
fn fs_aerial(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let uv = in.pos.xy / max(dims, vec2<f32>(1.0));
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    // Only the cleared far value is sky. A `>= 0.999` cut would drop aerial
    // perspective beyond ~100 m with a 0.1 m near plane.
    if (depth >= 1.0) {
        discard;
    }
    let view = view_from_uv(uv);
    let dist = world_distance(uv, depth, u.camera.xyz);
    let table = aerial_at(view, dist);
    let aerial = under_clouds(view, dist, without_interior_air(view, dist, table, 0), table.rgb);
    return vec4<f32>(aerial.rgb * hdr_scale(), clamp(aerial.a, 0.0, 1.0));
}

// ---------------------------------------------------------------------------------------------
// `aerial_from_tables` (ADR-3294): the ground's haze and the weather's extinction from the same
// tables as the sky. The weather is one term for the sky, the clouds and the ground
// (`fs_weather_veil`, after the clouds); the forward pass, the lite pass and the terrain leave
// their analytic weather in-scatter off on these frames.
// ---------------------------------------------------------------------------------------------

// The end of the last aerial slice (m): AERIAL_SLICES × AERIAL_STEP_KM.
const TABLE_AIR_RANGE_M: f32 = 32000.0;

// The sky's last row above the horizon toward `view`'s azimuth: the in-scatter of the longest
// path the air holds. A far surface and the rain both fade toward it (four texel loads). Row
// h/2 is the first row above the horizon (Hillaire 2020 § 5.3 latitude mapping, `sky_uv`).
fn horizon_sky(view: vec3<f32>) -> vec3<f32> {
    let uv = sky_uv(view, u.sun.xyz);
    let h = max(f32(textureDimensions(sky_lut).y), 1.0);
    return sample_sky_lut(vec2<f32>(uv.x, 0.5 + 0.5 / h)).rgb;
}

// `CLOUD_OVERCAST_DIM` (clouds.rs; a unit test keeps them equal).
const TABLE_OVERCAST_DIM: f32 = 0.6;

// The rain's in-scatter: the radiance a drop sees, averaged over the sphere (isotropic drops).
// The upper half is the horizon row (the longest path the air holds), shaded by a drawn deck with
// `overcast_sky`'s rule (ADR-321): greyed toward its luminance and dimmed by up to
// TABLE_OVERCAST_DIM with the deck's overcast factor. `table_ground` adds the rest, the same for
// every pixel and built on the CPU (`horizon_seam::rain_inscatter_constant`): half the planet
// ground under the same deck rule — under a deck the raining cloud blocks the direct sun, so the
// ground half is lit by the diffuse sky alone and the sun's share is 0 (ADR-3294 (13)); with no
// deck it is the fully lit ground plus the transmitted sun's irradiance over 4π. The rule is
// linear in rgb, so the halves take it apart.
fn rain_inscatter(view: vec3<f32>) -> vec3<f32> {
    var sky = 0.5 * horizon_sky(view);
    let o = clamp(u.table_air.w, 0.0, 1.0);
    if (o > 0.0) {
        let luma = dot(sky, vec3<f32>(0.2126, 0.7152, 0.0722));
        sky = mix(sky, vec3<f32>(luma), o) * (1.0 - TABLE_OVERCAST_DIM * o);
    }
    return sky + max(u.table_ground.xyz, vec3<f32>(0.0));
}

// Mobile: one bilinear tap of one slice (four texel loads), the first slice whose end is at or
// past the surface. The air inside it is taken as even, so T(d) = T_s^(d/d_s) and
// L(d) = L_s (1 − T(d)) / (1 − T_s).
fn lite_aerial(view: vec3<f32>, dist_m: f32) -> vec4<f32> {
    let uv = sky_uv(view, u.sun.xyz);
    let km = max(dist_m, 0.0) / 1000.0;
    let slice = clamp(i32(ceil(km / AERIAL_STEP_KM)) - 1, 0, AERIAL_SLICES - 1);
    let s = aerial_texel(uv, slice);
    let end_km = f32(slice + 1) * AERIAL_STEP_KM;
    let ts = clamp(s.a, 1.0e-6, 1.0);
    let x = min(km / end_km, 1.0);
    let t = pow(ts, x);
    let scale = select(x, (1.0 - t) / (1.0 - ts), 1.0 - ts > 1.0e-4);
    return vec4<f32>(s.rgb * scale, t);
}

// Air between the eye and a surface `dist_m` away (rgb in-scatter, a transmittance). Past the
// last slice the air keeps thickening toward the horizon sky: T(d) = T_edge^(d / range) and
// L(d) = L_edge + (L_horizon − L_edge)(1 − T(d) / T_edge), so the farthest ground meets the
// sky's last row instead of stopping at the 32 km slice.
fn tables_air(view: vec3<f32>, dist_m: f32, lite: bool) -> vec4<f32> {
    var air: vec4<f32>;
    if (lite) {
        air = lite_aerial(view, dist_m);
    } else {
        air = aerial_at(view, dist_m);
    }
    if (dist_m <= TABLE_AIR_RANGE_M) {
        return air;
    }
    // The visible sky's last row, not the deck-dimmed one: the cloud layer fades out toward the
    // horizon, so the row the far ground meets is the clear table's.
    let horizon = horizon_sky(view);
    let edge_t = clamp(air.a, 1.0e-6, 1.0);
    let t = pow(edge_t, dist_m / TABLE_AIR_RANGE_M);
    return vec4<f32>(mix(air.rgb, horizon, 1.0 - t / edge_t), t);
}

// The precipitation's extinction σ (spec 99 § 3.2) over everything the eye sees: the ground at
// its depth, the sky and the clouds at the far plane. The rain scatters the light around it
// (`rain_inscatter`) toward the eye, so the image mixes toward that by exp(−σ d):
// rgb = L_rain (1 − T), a = T (the blend keeps dst × T). One term for the sky and the ground
// (four texel loads per pixel).
@fragment
fn fs_weather_veil(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let uv = in.pos.xy / max(dims, vec2<f32>(1.0));
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    let view = view_from_uv(uv);
    var dist = max(u.table_air.z, 0.0);
    if (depth < 1.0) {
        dist = world_distance(uv, depth, u.camera.xyz);
    }
    // No rain falls inside an interior (ADR-4757): its stretch of the ray neither dims nor scatters.
    let inside = interior_air_on_ray(view, dist, vec3<f32>(0.0), 3).w;
    if (inside > 0.0) {
        dist = max(dist - inside, 0.0);
    }
    let t = exp(-max(u.table_air.y, 0.0) * dist);
    return vec4<f32>(rain_inscatter(view) * (1.0 - t) * hdr_scale(), t);
}

fn tables_aerial_out(pos: vec2<f32>, lite: bool) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let uv = pos / max(dims, vec2<f32>(1.0));
    let depth = textureLoad(scene_depth, vec2<i32>(pos), 0);
    let view = view_from_uv(uv);
    let dist = world_distance(uv, depth, u.camera.xyz);
    let table = tables_air(view, dist, lite);
    let air = under_clouds(view, dist, without_interior_air(view, dist, table, select(1, 2, lite)), table.rgb);
    return vec4<f32>(air.rgb * hdr_scale(), clamp(air.a, 0.0, 1.0));
}

@fragment
fn fs_aerial_tables(in: VsOut) -> @location(0) vec4<f32> {
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    if (depth >= 1.0) {
        discard;
    }
    return tables_aerial_out(in.pos.xy, false);
}

@fragment
fn fs_aerial_tables_lite(in: VsOut) -> @location(0) vec4<f32> {
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    if (depth >= 1.0) {
        discard;
    }
    return tables_aerial_out(in.pos.xy, true);
}
