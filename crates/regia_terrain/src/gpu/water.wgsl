// SPDX-License-Identifier: MPL-2.0
// Water plane: depth-based colour against the terrain height texture, shoreline foam band,
// two-ripple displacement with analytic normals, Blinn sun specular. Translucent pass,
// depth-read only (no depth write). No simulation.
//
// Shading model (ADR-1237; CPU reference and tests: `regia_terrain::water`): Schlick
// Fresnel against the sky radiance (the frame's uniform sky, the probe fallback), the view
// ray refracted by Snell's law into the water, Beer-Lambert opacity over the refracted path,
// total internal reflection seen from below, a foam band measured horizontally from the
// shoreline, and an opacity that fades in from the waterline. The constants below are
// compared with the Rust ones by `shader_constants_mirror_the_reference`.

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

struct Terrain {
    half_width: f32,
    world_width: f32,
    height_scale: f32,
    height_offset: f32,
    sample_max: vec2<f32>,
    grid_size: f32,
    root_level: f32,
    // Unread. Present so `depth` sits at the same offset as in `terrain.wgsl` (ADR-8369).
    mip_info: vec4<f32>,
    // x = half depth, y = world depth, zw = 0. On a square map x == half_width and
    // y == world_width.
    depth: vec4<f32>,
};

struct Water {
    rect_min: vec2<f32>,
    rect_size: f32,
    level: f32,
    shallow: vec4<f32>,
    deep: vec4<f32>,
    foam: vec4<f32>,
    wave_params: vec4<f32>,  // foam_width, max_depth, alpha, wave_amp
    freq_pad: vec4<f32>,     // wave_freq, refraction (1 = scene_color/scene_depth bound), pad
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(1) @binding(0) var height_texture: texture_2d<f32>;
@group(1) @binding(2) var<uniform> terrain: Terrain;
@group(1) @binding(3) var<uniform> water: Water;
// Screen-space refraction sources (`render_offscreen`): the opaque frame before the water
// (display-linear) and its depth, read-only. 1x1 stand-ins when `freq_pad.y` is 0.
@group(1) @binding(4) var scene_color: texture_2d<f32>;
@group(1) @binding(5) var scene_depth: texture_depth_2d;

// Manual bilinear height lookup; see terrain.wgsl.
fn height_at(s: vec2<f32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(height_texture));
    let max_s = vec2<f32>(f32(dims.x - 1), f32(dims.y - 1));
    let p = clamp(s, vec2<f32>(0.0), max_s);
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let h00 = textureLoad(height_texture, i, 0).r;
    let h10 = textureLoad(height_texture, vec2<i32>(i1.x, i.y), 0).r;
    let h01 = textureLoad(height_texture, vec2<i32>(i.x, i1.y), 0).r;
    let h11 = textureLoad(height_texture, i1, 0).r;
    let a = mix(h00, h10, f.x);
    let b = mix(h01, h11, f.x);
    return mix(a, b, f.y) * terrain.height_scale + terrain.height_offset;
}

fn height_w(xz: vec2<f32>) -> f32 {
    // Square maps keep the historical expression. A rectangular map (ADR-8369) maps Z
    // through the depth lane, the same split as `terrain.wgsl` `sample_coord`.
    var s = (xz + vec2<f32>(terrain.half_width)) / terrain.world_width * terrain.sample_max;
    if (terrain.depth.y != terrain.world_width) {
        s = (xz + vec2<f32>(terrain.half_width, terrain.depth.x)) / vec2<f32>(terrain.world_width, terrain.depth.y) * terrain.sample_max;
    }
    return height_at(s);
}

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(0.1031, 0.1030));
    q += dot(q, q.yx + vec2<f32>(33.33));
    return fract((q.x + q.y) * q.x);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

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

const INV_PI: f32 = 0.3183098862;
const PI: f32 = 3.14159265;

const GRID: u32 = 64u;

const WATER_IOR: f32 = 1.333;
const WATER_F0: f32 = 0.020373;
const SHORE_CUTOFF: f32 = 0.05;
const SHORE_FADE: f32 = 0.2;
const ABSORPTION_AT_MAX_DEPTH: f32 = 6.0;
const FOAM_COVERAGE: f32 = 0.8;
const MIN_SHORE_SLOPE: f32 = 0.02;

// The two ripples: wave vectors per unit `wave_frequency` (x, z), angular speeds (rad/s) and
// amplitude gains.
const WAVE_DIR_0: vec2<f32> = vec2<f32>(0.7, 0.3);
const WAVE_DIR_1: vec2<f32> = vec2<f32>(-0.78, 1.3);
const WAVE_SPEED: vec2<f32> = vec2<f32>(1.1, 0.9);
const WAVE_GAIN: vec2<f32> = vec2<f32>(1.0, 0.6);

fn wave_phases(xz: vec2<f32>, t: f32) -> vec2<f32> {
    return vec2<f32>(dot(xz, WAVE_DIR_0), dot(xz, WAVE_DIR_1)) * water.freq_pad.x + t * WAVE_SPEED;
}

// Analytic normal of the full rippled surface (both ripples, whatever the grid resolves).
fn ripple_normal(xz: vec2<f32>, t: f32) -> vec3<f32> {
    let slope = cos(wave_phases(xz, t)) * WAVE_GAIN * (water.freq_pad.x * water.wave_params.w);
    let dydx = slope.x * WAVE_DIR_0.x + slope.y * WAVE_DIR_1.x;
    let dydz = slope.x * WAVE_DIR_0.y + slope.y * WAVE_DIR_1.y;
    return normalize(vec3<f32>(-dydx, 1.0, -dydz));
}

fn fresnel_schlick(cos_theta: f32) -> f32 {
    let c = 1.0 - clamp(cos_theta, 0.0, 1.0);
    let c2 = c * c;
    return WATER_F0 + (1.0 - WATER_F0) * c2 * c2 * c;
}

// Cosine of the refracted angle of a ray entering the water at `cos_incident` (Snell).
fn refracted_cos(cos_incident: f32) -> f32 {
    let c = clamp(cos_incident, 0.0, 1.0);
    let sin2_t = (1.0 - c * c) / (WATER_IOR * WATER_IOR);
    return sqrt(max(1.0 - sin2_t, 0.0));
}

// Reflectance seen from inside the water at `cos_up` to the normal: total past the
// critical angle, Schlick at the air-side angle inside the window.
fn reflectance_below(cos_up: f32) -> f32 {
    let c = min(cos_up, 1.0);
    let sin2_t = WATER_IOR * WATER_IOR * (1.0 - c * c);
    if (sin2_t >= 1.0) {
        return 1.0;
    }
    return fresnel_schlick(sqrt(1.0 - sin2_t));
}

// Beer-Lambert opacity of the water body over `path` meters: 95 % of alpha at half max_depth.
fn body_opacity(path: f32) -> f32 {
    let tau = ABSORPTION_AT_MAX_DEPTH * max(path, 0.0) / max(water.wave_params.y, 0.01);
    return water.wave_params.z * (1.0 - exp(-tau));
}

fn body_color(path: f32) -> vec3<f32> {
    let t = min(max(path, 0.0) / max(water.wave_params.y, 0.01), 1.0);
    return mix(water.shallow.rgb, water.deep.rgb, t);
}

// Foam coverage `shore_distance` meters (horizontal) from the shoreline.
fn foam_cover(shore_distance: f32, mottle: f32) -> f32 {
    let band = 1.0 - smoothstep(0.0, max(water.wave_params.x, 0.05), shore_distance);
    return clamp(band * (0.55 + 0.45 * clamp(mottle, 0.0, 1.0)), 0.0, 1.0) * FOAM_COVERAGE;
}

// 0 at the cutoff where the plane stops being drawn, 1 from SHORE_FADE - SHORE_CUTOFF meters
// of depth on: the waterline has no step.
fn shore_fade(depth: f32) -> f32 {
    return smoothstep(-SHORE_CUTOFF, SHORE_FADE - SHORE_CUTOFF, depth);
}

// Bed slope (rise over run): central differences one height sample apart.
fn bed_slope(xz: vec2<f32>) -> f32 {
    let spacing = terrain.world_width / max(terrain.sample_max.x, 1.0);
    let dx = (height_w(xz + vec2<f32>(spacing, 0.0)) - height_w(xz - vec2<f32>(spacing, 0.0))) / (2.0 * spacing);
    let dz = (height_w(xz + vec2<f32>(0.0, spacing)) - height_w(xz - vec2<f32>(0.0, spacing))) / (2.0 * spacing);
    return length(vec2<f32>(dx, dz));
}

// Where the refracted view ray meets the bed: from the surface point along Snell's
// refracted direction down to `depth` meters below it (the bed taken as locally flat). A
// ripple normal turned away from the eye (grazing views) refracts about the still plane's.
fn refracted_bed_point(world: vec3<f32>, view_dir: vec3<f32>, n: vec3<f32>, depth: f32) -> vec3<f32> {
    let facing = select(vec3<f32>(0.0, 1.0, 0.0), n, dot(n, view_dir) > 0.0);
    let t = refract(-view_dir, facing, 1.0 / WATER_IOR);
    return world + t * (max(depth, 0.0) / max(-t.y, 0.05));
}

// The opaque frame seen through the surface along the refracted ray: the scene copy at the
// bed point's pixel. That pixel's straight ray meets the bed point, or the bed before it,
// unless something above the water covers it there (a rock, the shore): the scene depth is
// nearer than where that ray crosses the water plane. Then this pixel's own straight-through
// view, so nothing above the water prints in it.
fn refracted_scene(frag: vec4<f32>, bed: vec3<f32>) -> vec3<f32> {
    let dims = vec2<i32>(textureDimensions(scene_color));
    let here = clamp(vec2<i32>(frag.xy), vec2<i32>(0), dims - vec2<i32>(1));
    let straight = textureLoad(scene_color, here, 0).rgb;
    let clip = frame.view_proj * vec4<f32>(bed, 1.0);
    if (clip.w <= 1.0e-4) {
        return straight;
    }
    let ndc = clip.xy / clip.w;
    let px = vec2<i32>(floor(vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * vec2<f32>(dims)));
    if (any(px < vec2<i32>(0)) || any(px >= dims)) {
        return straight;
    }
    let eye = frame.camera_pos_time.xyz;
    let s = clamp((eye.y - water.level) / max(eye.y - bed.y, 1.0e-4), 0.0, 1.0);
    let crossing = frame.view_proj * vec4<f32>(eye + (bed - eye) * s, 1.0);
    if (textureLoad(scene_depth, px, 0) < crossing.z / max(crossing.w, 1.0e-4)) {
        return straight;
    }
    return textureLoad(scene_color, px, 0).rgb;
}

struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VOut {
    // Two triangles per grid cell from the vertex index (no buffers).
    let cell = vi / 6u;
    let corner = vi % 6u;
    // Corners (0,0) (0,1) (1,0) (1,0) (0,1) (1,1), computed rather than read from a
    // dynamically indexed value array (ADR-026).
    let corner_uv = vec2<f32>(
        select(0.0, 1.0, corner == 2u || corner == 3u || corner == 5u),
        select(0.0, 1.0, corner == 1u || corner == 4u || corner == 5u),
    );
    let cx = cell % GRID;
    let cz = cell / GRID;
    let uv = (vec2<f32>(f32(cx), f32(cz)) + corner_uv) / f32(GRID);
    let xz = water.rect_min + uv * water.rect_size;

    // Each ripple keeps its displacement only where the grid resolves it: 1 with four
    // vertices per wavelength, 0 at two (Nyquist). An unresolved ripple aliases into a
    // coarse wobble of the whole plane against the shore.
    let cell_size = water.rect_size / f32(GRID);
    let wave_numbers = vec2<f32>(length(WAVE_DIR_0), length(WAVE_DIR_1)) * water.freq_pad.x;
    let fade = clamp(vec2<f32>(2.0) - 2.0 * cell_size * wave_numbers / PI, vec2<f32>(0.0), vec2<f32>(1.0));
    let t = frame.camera_pos_time.w;
    let y = water.level + dot(sin(wave_phases(xz, t)), WAVE_GAIN * fade) * water.wave_params.w;

    let world = vec3<f32>(xz.x, y, xz.y);
    return VOut(frame.view_proj * vec4<f32>(world, 1.0), world);
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let terrain_h = height_w(in.world.xz);
    let depth = water.level - terrain_h;
    if (depth < -SHORE_CUTOFF) {
        discard; // Water plane under dry land.
    }

    let t = frame.camera_pos_time.w;
    let n = ripple_normal(in.world.xz, t);
    let to_eye = frame.camera_pos_time.xyz - in.world;
    let eye_distance = length(to_eye);
    let view_dir = to_eye / max(eye_distance, 1.0e-4);

    // Lighting per unit albedo: same convention as terrain.wgsl / regia_render::forward —
    // π-normalised Lambert of the exposed sun radiance plus the sky ambient, then the AgX
    // tonemap; the sRGB target encodes once on store. The sky ambient is also the radiance
    // the surface reflects (the uniform sky the terrain is lit by).
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    let sun_radiance = frame.sun_color_exposure.rgb * frame.sun_color_exposure.a;
    let sky = frame.ambient_debug.rgb;
    let irradiance = sun_radiance * max(dot(n, sun_dir), 0.0) * INV_PI + sky;

    // `surface` is premultiplied by `coverage` (what of the pixel the water hides).
    var surface = vec3<f32>(0.0);
    var coverage = 0.0;
    var foam = 0.0;
    let above = frame.camera_pos_time.y >= water.level;
    if (above) {
        let cos_view = clamp(dot(n, view_dir), 0.0, 1.0);
        let reflectance = fresnel_schlick(cos_view);
        // The bed light crosses the water along the refracted view ray.
        let path = max(depth, 0.0) / refracted_cos(cos_view);
        let transmitted = (1.0 - reflectance) * body_opacity(path);
        // Blinn sun glint, capped so the sparkle stays inside the tonemap's working range
        // (an unbounded glint saturates large bands of the sea to flat white).
        let half_dir = normalize(view_dir + sun_dir);
        let glint = sun_radiance * INV_PI * min(pow(max(dot(n, half_dir), 0.0), 220.0), 0.5);
        surface = reflectance * sky + transmitted * body_color(path) * irradiance + glint;
        coverage = reflectance + transmitted;
        let shore_distance = max(depth, 0.0) / max(bed_slope(in.world.xz), MIN_SHORE_SLOPE);
        let mottle = vnoise(in.world.xz * 3.1 + vec2<f32>(t * 0.15, -t * 0.11));
        foam = foam_cover(shore_distance, mottle);
    } else {
        // From below: the surface mirrors the water body past the critical angle, and the eye
        // looks through the water between itself and the surface.
        let reflectance = reflectance_below(clamp(-dot(n, view_dir), 0.0, 1.0));
        coverage = reflectance + (1.0 - reflectance) * body_opacity(eye_distance);
        surface = coverage * water.deep.rgb * irradiance;
    }
    surface = (1.0 - foam) * surface + foam * water.foam.rgb * irradiance;
    coverage = (1.0 - foam) * coverage + foam;
    var lit = surface / max(coverage, 1.0e-4);

    let fp = frame.fog_params;
    if (fp.w > 0.5 && fp.x > 0.0) {
        let height_density = fp.x * exp(-fp.y * (in.world.y - fp.z));
        let fog_t = 1.0 - exp(-max(eye_distance, 0.0) * max(height_density, 0.0));
        lit = mix(lit, frame.fog_color.rgb, fog_t);
    }
    lit = clamp(lit, vec3<f32>(0.0), vec3<f32>(1.0e4));
    // The target is Rgba8UnormSrgb (the hardware encodes): store linear display values.
    lit = display_to_linear(default_display_transform(lit));
    let alpha = clamp(coverage * shore_fade(depth), 0.0, 1.0);

    // Screen-space refraction: what shows through is the opaque frame along the refracted
    // ray, composited here (alpha 1 overwrites). Without the scene copy, or from below, the
    // blend shows the frame straight behind. Both agree where the depth goes to zero.
    if (water.freq_pad.y > 0.5 && above) {
        let bed = refracted_bed_point(in.world, view_dir, n, depth);
        return vec4<f32>(mix(refracted_scene(in.position, bed), lit, alpha), 1.0);
    }
    return vec4<f32>(lit, alpha);
}
