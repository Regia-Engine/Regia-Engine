// SPDX-License-Identifier: MPL-2.0
// Rain streaks, snow flakes and splashes (spec 99 § 5). Stateless: each vertex rebuilds its drop
// from the instance index, the same formulas as `rain_drop`, `snow_flake` and `splash` in
// `precip.rs`. No storage buffers, no derivatives in the vertex stage, fixed loops (FXC-safe).

struct Precip {
    // Eye-relative view-projection (the eye is at the origin), so precision holds far out.
    view_proj: mat4x4<f32>,
    // The occlusion tile's matrix, world to clip, and its inverse.
    occ_view_proj: mat4x4<f32>,
    occ_inverse: mat4x4<f32>,
    // scale.xy, offset.zw of the tile in the atlas.
    occ_rect: vec4<f32>,
    // Atlas edge (texels), tile edge (texels), 1 when bound, unused.
    occ_info: vec4<f32>,
    // Box size B, xyz. w = splash disk radius.
    box_size: vec4<f32>,
    // Box center relative to the eye. w = the weather clock, seconds (wrapped to a day).
    shift_clock: vec4<f32>,
    // eye mod B. w = the streak shutter, seconds.
    eye_mod: vec4<f32>,
    // World eye. w = pixels per meter at depth 1.
    eye: vec4<f32>,
    // Wrapped drift x, z. Viewport width and height.
    drift_view: vec4<f32>,
    wind: vec4<f32>,
    eye_velocity: vec4<f32>,
    // Drawn counts: rain, snow, splashes, unused.
    counts: vec4<f32>,
    rain_fall: array<vec4<f32>, 2>,
    rain_speed: array<vec4<f32>, 2>,
    rain_diameter: array<vec4<f32>, 2>,
    snow_fall: vec4<f32>,
    snow_speed: vec4<f32>,
    // Camera right (xyz), near. Camera up (xyz), far. Camera forward (xyz), unused.
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
    // Toward the sun (xyz), cloud transmittance. Sun color (xyz), ambient scalar.
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    sky_up: vec4<f32>,
    sky_down: vec4<f32>,
    // Exposure divisor, target mode (0 display bytes, 1 linear, 2 pre-exposure HDR), the
    // resolution divisor (1 full, 2 half, 4 quarter; ADR-3249), unused.
    target_info: vec4<f32>,
}

@group(0) @binding(0) var<uniform> precip: Precip;
@group(0) @binding(1) var scene_depth: texture_depth_2d;
@group(0) @binding(2) var occ_atlas: texture_depth_2d;

const TAU: f32 = 6.2831853;
const EDGE_FADE: f32 = 0.1;
const COUNT_FADE: f32 = 0.05;
const MIN_STREAK_M: f32 = 0.02;
const SWAY_M: f32 = 0.15;
const MIN_COVERAGE: f32 = 0.5;

fn hash_step(state: u32) -> u32 {
    let word = state * 747796405u + 2891336453u;
    let mixed = ((word >> ((word >> 28u) + 4u)) ^ word) * 277803737u;
    return (mixed >> 22u) ^ mixed;
}

fn unit(bits: u32) -> f32 {
    return f32(bits >> 8u) / 16777216.0;
}

fn precip_hash4(index: u32) -> vec4<f32> {
    let a = hash_step(index);
    let b = hash_step(a);
    let c = hash_step(b);
    let d = hash_step(c);
    return vec4<f32>(unit(a), unit(b), unit(c), unit(d));
}

struct Wrapped {
    position: vec3<f32>,
    fade: f32,
}

fn wrapped(seed: vec4<f32>, offset: vec3<f32>) -> Wrapped {
    let b = precip.box_size.xyz;
    let half_size = b * 0.5;
    let shift = precip.shift_clock.xyz;
    let x = b * seed.xyz + offset;
    let c = precip.eye_mod.xyz + shift;
    let local = b * fract((x - c) / b) - half_size;
    let e = abs(local) / half_size;
    let edge = max(e.x, max(e.y, e.z));
    return Wrapped(shift + local, 1.0 - smoothstep(1.0 - EDGE_FADE, 1.0, edge));
}

fn count_fade(index: u32, count: f32) -> f32 {
    if (count <= 0.0) {
        return 0.0;
    }
    return 1.0 - smoothstep((1.0 - COUNT_FADE) * count, count, f32(index));
}

// 1 under open sky, 0 below the first occluder along the fall direction.
fn sky_open(world: vec3<f32>) -> f32 {
    if (precip.occ_info.z < 0.5) {
        return 1.0;
    }
    let clip = precip.occ_view_proj * vec4<f32>(world, 1.0);
    let ndc = clip.xyz / clip.w;
    let local = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (local.x < 0.0 || local.y < 0.0 || local.x >= 1.0 || local.y >= 1.0) {
        return 1.0;
    }
    let texel = vec2<i32>((local * precip.occ_rect.xy + precip.occ_rect.zw) * precip.occ_info.x);
    let stored = textureLoad(occ_atlas, texel, 0);
    return select(0.0, 1.0, min(ndc.z, 1.0) <= stored + 1.0e-4);
}

struct DropOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color_alpha: vec4<f32>,
    @location(2) view_depth: f32,
}

// Two triangles: (0, 1, 2) and (3, 4, 5). No lookup table: an indexed function-local array is
// spilled to memory, which Mali's driver loses the device over (`tests/shader_rules.rs`).
fn corner(vertex: u32) -> vec2<f32> {
    let right = vertex == 1u || vertex == 4u || vertex == 5u;
    let top = vertex == 2u || vertex == 3u || vertex == 5u;
    return vec2<f32>(select(-1.0, 1.0, right), select(-1.0, 1.0, top));
}

fn hemisphere_light() -> vec3<f32> {
    if (precip.sky_up.w > 0.5) {
        return (precip.sky_up.rgb + precip.sky_down.rgb) * 0.5;
    }
    return vec3<f32>(precip.sun_color.w);
}

// Henyey-Greenstein at g = 0.75: a stand-in for a sphere's forward glint.
fn forward_lobe(cos_theta: f32) -> f32 {
    let g = 0.75;
    let d = 1.0 + g * g - 2.0 * g * cos_theta;
    return (1.0 - g * g) / (12.566371 * pow(d, 1.5));
}

// A ribbon along `along` (unit) through `center`, `half_len` long, `width_m` wide, facing the eye
// (the origin). A subpixel width widens to a pixel and pays for it in coverage, down to
// `MIN_COVERAGE`: below that a far drop leaves the 8-bit range and a storm reads as empty air.
fn ribbon(center: vec3<f32>, along: vec3<f32>, half_len: f32, width_m: f32, c: vec2<f32>, color: vec3<f32>, alpha: f32) -> DropOut {
    let depth = max(dot(center, precip.forward.xyz), precip.right.w);
    let pixel_m = depth / max(precip.eye.w, 1.0);
    let wide = max(width_m, pixel_m);
    let side = normalize(cross(along, center) + vec3<f32>(1.0e-6, 0.0, 0.0));
    let world = center + along * (c.y * half_len) + side * (c.x * wide * 0.5);
    let coverage = max(width_m / wide, MIN_COVERAGE);
    return DropOut(
        precip.view_proj * vec4<f32>(world, 1.0),
        c,
        vec4<f32>(color, alpha * coverage),
        dot(world, precip.forward.xyz),
    );
}

fn culled() -> DropOut {
    return DropOut(vec4<f32>(0.0, 0.0, 2.0, 1.0), vec2<f32>(0.0), vec4<f32>(0.0), 0.0);
}

@vertex
fn vs_rain(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> DropOut {
    let k = index % 8u;
    let seed = precip_hash4(index);
    let box_place = wrapped(seed, vec3<f32>(precip.drift_view.x, -precip.rain_fall[k / 4u][k % 4u], precip.drift_view.y));
    let diameter = precip.rain_diameter[k / 4u][k % 4u];
    var alpha = box_place.fade * count_fade(index, precip.counts.x) * clamp(0.25 * diameter / 2.0, 0.1, 0.5);
    if (alpha <= 0.0) {
        return culled();
    }
    let v = precip.wind.xyz - vec3<f32>(0.0, precip.rain_speed[k / 4u][k % 4u], 0.0);
    let v_rel = v - precip.eye_velocity.xyz;
    let speed = length(v_rel);
    let along = v_rel / max(speed, 1.0e-4);
    let half_len = 0.5 * max(speed * precip.eye_mod.w, MIN_STREAK_M);
    // The streak's lower end: a drop just over a roof would poke through it otherwise.
    let lower = box_place.position + vec3<f32>(0.0, -abs(along.y) * half_len, 0.0);
    alpha *= sky_open(precip.eye.xyz + lower);
    if (alpha <= 0.0) {
        return culled();
    }
    // A drop reflects and refracts what is around it: the sky rows, and the sun's forward glint.
    let view = normalize(box_place.position);
    let glint = forward_lobe(dot(precip.sun_dir.xyz, view)) * precip.sun_dir.w;
    let color = hemisphere_light() * 0.6 + precip.sun_color.rgb * glint;
    return ribbon(box_place.position, along, half_len, diameter * 0.001, corner(vertex), color, alpha);
}

@vertex
fn vs_snow(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> DropOut {
    let k = index % 4u;
    let seed = precip_hash4(index);
    let box_place = wrapped(seed, vec3<f32>(precip.drift_view.x, -precip.snow_fall[k], precip.drift_view.y));
    let period = 1.5 + 1.5 * seed.w;
    let angle = (precip.shift_clock.w / period) * TAU + seed.x * TAU;
    let position = box_place.position + vec3<f32>(SWAY_M * sin(angle), 0.0, SWAY_M * cos(angle));
    var alpha = box_place.fade * count_fade(index, precip.counts.y) * 0.9;
    alpha *= sky_open(precip.eye.xyz + position);
    if (alpha <= 0.0) {
        return culled();
    }
    let size_m = (3.0 + 7.0 * seed.w) * 0.001;
    let v = precip.wind.xyz - vec3<f32>(0.0, precip.snow_speed[k], 0.0);
    let v_rel = v - precip.eye_velocity.xyz;
    let speed = length(v_rel);
    let along = v_rel / max(speed, 1.0e-4);
    let half_len = 0.5 * max(speed * precip.eye_mod.w, size_m);
    // Lit like a lit particle: a wrapped Lambert toward the eye-facing sprite, then the sky rows.
    let view = normalize(position);
    let wrap = 0.5 - 0.5 * dot(precip.sun_dir.xyz, view);
    let color = 0.85 * (precip.sun_color.rgb * wrap * precip.sun_dir.w + hemisphere_light());
    return ribbon(position, along, half_len, size_m, corner(vertex), color, alpha);
}

@vertex
fn vs_splash(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> DropOut {
    if (precip.occ_info.z < 0.5) {
        return culled();
    }
    // A life cycle of 0.3 s on a per-instance phase; each cycle takes a new place.
    let start = precip_hash4(index ^ 0x5bd1e995u).w;
    let age = precip.shift_clock.w / 0.3 + start;
    let cycle = floor(age);
    let phase = age - cycle;
    let seed = precip_hash4(index ^ (u32(cycle) * 0x9e3779b1u));
    let radius = precip.box_size.w * sqrt(seed.x);
    let angle = seed.y * TAU;
    // Snapped to world space, so a splash does not slide with the camera while it lives.
    let center = floor(precip.eye.xz);
    let xz = center + radius * vec2<f32>(cos(angle), sin(angle));
    // The point's texel in the tile: the top surface the rain hits there, rebuilt from the stored
    // depth through the inverse matrix, exact on a tilted view.
    let probe = precip.occ_view_proj * vec4<f32>(xz.x, precip.eye.y, xz.y, 1.0);
    let ndc = probe.xy / probe.w;
    let local = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (local.x < 0.0 || local.y < 0.0 || local.x >= 1.0 || local.y >= 1.0) {
        return culled();
    }
    let texel = vec2<i32>((local * precip.occ_rect.xy + precip.occ_rect.zw) * precip.occ_info.x);
    let stored = textureLoad(occ_atlas, texel, 0);
    if (stored >= 1.0) {
        return culled();
    }
    let top = precip.occ_inverse * vec4<f32>(ndc, stored, 1.0);
    let hit = top.xyz / top.w;
    let alpha = (1.0 - phase) * count_fade(index, precip.counts.z) * 0.5;
    if (alpha <= 0.0) {
        return culled();
    }
    let c = corner(vertex);
    let center_rel = hit - precip.eye.xyz;
    // A small upward crown that widens over its life.
    let size_m = 0.02 + 0.08 * phase;
    let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), center_rel) + vec3<f32>(1.0e-6, 0.0, 0.0));
    let world = center_rel + side * (c.x * size_m) + vec3<f32>(0.0, (c.y * 0.5 + 0.5) * size_m, 0.0);
    let glint = forward_lobe(dot(precip.sun_dir.xyz, normalize(center_rel))) * precip.sun_dir.w;
    return DropOut(
        precip.view_proj * vec4<f32>(world, 1.0),
        c,
        vec4<f32>(hemisphere_light() * 0.6 + precip.sun_color.rgb * glint, alpha),
        dot(world, precip.forward.xyz),
    );
}

// Soft fade against the scene depth, so a streak does not cut into the ground. A reduced target
// (no depth attachment) reads the scene depth at its pixel's place in the frame: the fade is then
// its only depth test, 0 behind the scene.
fn soft_fade(position: vec4<f32>, view_depth: f32) -> f32 {
    let scale = max(precip.target_info.z, 1.0);
    var at = vec2<i32>(position.xy * scale);
    if (scale > 1.0) {
        at = min(at, vec2<i32>(textureDimensions(scene_depth)) - vec2<i32>(1));
    }
    let stored = textureLoad(scene_depth, at, 0);
    let near = precip.right.w;
    let far = precip.up.w;
    let scene = near * far / (far - stored * (far - near));
    return clamp((scene - view_depth) / 0.3, 0.0, 1.0);
}

fn encode(color: vec3<f32>, alpha: f32) -> vec4<f32> {
    var c = color / max(precip.target_info.x, 1.0e-8);
    if (precip.target_info.y < 0.5) {
        c = pow(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
    }
    return vec4<f32>(c * alpha, alpha);
}

@fragment
fn fs_rain(in: DropOut) -> @location(0) vec4<f32> {
    let across = 1.0 - in.uv.x * in.uv.x;
    let along = 1.0 - in.uv.y * in.uv.y;
    let alpha = in.color_alpha.a * across * along * soft_fade(in.clip, in.view_depth);
    return encode(in.color_alpha.rgb, alpha);
}

@fragment
fn fs_flake(in: DropOut) -> @location(0) vec4<f32> {
    let r2 = dot(in.uv, in.uv);
    let round_sprite = 1.0 - smoothstep(0.4, 1.0, r2);
    let alpha = in.color_alpha.a * round_sprite * soft_fade(in.clip, in.view_depth);
    return encode(in.color_alpha.rgb, alpha);
}

// The reduced-resolution composite (ADR-3249): one full-screen triangle over the frame's pass,
// the reduced target sampled bilinearly. It holds premultiplied color in the frame's encoding.
@group(1) @binding(0) var reduced_color: texture_2d<f32>;
@group(1) @binding(1) var reduced_sampler: sampler;

struct CompositeOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_composite(@builtin(vertex_index) vertex: u32) -> CompositeOut {
    let x = f32((vertex << 1u) & 2u);
    let y = f32(vertex & 2u);
    return CompositeOut(vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0), vec2<f32>(x, y));
}

@fragment
fn fs_composite(in: CompositeOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(reduced_color, reduced_sampler, in.uv, 0.0);
}
