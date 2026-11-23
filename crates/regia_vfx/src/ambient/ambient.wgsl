// SPDX-License-Identifier: MPL-2.0
// Ambient life (ADR-3024): motes, swarms, butterflies, fireflies, leaves and birds. Stateless
// except the birds: each vertex rebuilds its creature from the instance index and the seed, the
// same formulas as `mote_at`, `swarm_member_at`, `butterfly_at`, `firefly_at` and `leaf_at` in
// `ambient.rs`. No storage buffers, no indexed function-local arrays, fixed loops (FXC-safe).

struct Bird {
    // Eye-relative position (xyz), wingspan (w).
    position_size: vec4<f32>,
    // Unit heading (xyz), wing angle over the body plane (w, radians).
    heading_wing: vec4<f32>,
    // Linear color (rgb), opacity (a).
    color_alpha: vec4<f32>,
}

struct Ambient {
    // Eye-relative view-projection (the eye is at the origin), so precision holds far out.
    view_proj: mat4x4<f32>,
    // Camera right (xyz), near. Camera up (xyz), far. Camera forward (xyz), pixels per metre at
    // depth 1.
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
    // Toward the sun (xyz), sun through the cloud (w). Sun color (xyz), ambient scalar (w).
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    sky_up: vec4<f32>,
    sky_down: vec4<f32>,
    // Exposure divisor, target mode (0 display bytes, 1 linear, 2 pre-exposure HDR), the
    // animation clock (s, 0..256), the season index.
    target_info: vec4<f32>,
    // Drawn counts: motes, swarm members, butterflies, fireflies.
    counts_a: vec4<f32>,
    // Drawn counts: leaves, birds; unused.
    counts_b: vec4<f32>,
    mote_drift: vec4<f32>,
    leaf_fall: vec4<f32>,
    // Leaf drift x, z; unused.
    leaf_drift: vec4<f32>,
    // eye mod B of the motes, swarms, butterflies, fireflies and leaves boxes.
    eye_mod_motes: vec4<f32>,
    eye_mod_swarms: vec4<f32>,
    eye_mod_butterflies: vec4<f32>,
    eye_mod_fireflies: vec4<f32>,
    eye_mod_leaves: vec4<f32>,
    // The seed (x); unused.
    seed: vec4<u32>,
    // World eye (xyz); unused.
    eye: vec4<f32>,
    // The sun's occlusion view (the precipitation's view aimed along the sun): world to the tile's
    // clip space, the tile's scale xy and offset zw in the atlas, and atlas edge, tile edge, 1 when
    // bound, unused.
    occ_view_proj: mat4x4<f32>,
    occ_rect: vec4<f32>,
    occ_info: vec4<f32>,
    birds: array<Bird, 64>,
}

@group(0) @binding(0) var<uniform> amb: Ambient;
@group(0) @binding(1) var scene_depth: texture_depth_2d;
@group(0) @binding(2) var occ_atlas: texture_depth_2d;

const TAU: f32 = 6.2831853;
const EDGE_FADE: f32 = 0.15;
const COUNT_FADE: f32 = 0.1;
const SWARM_MEMBERS: u32 = 40u;
const FLY_SWARM_MEMBERS: u32 = 4u;
// See `vs_mote` and `vs_swarm`. Chosen by eye on a real forest at 1440p (ADR-3024).
const MOTE_GAIN: f32 = 14.0;
const SWARM_GAIN: f32 = 10.0;
const SWARM_MIN_PX: f32 = 2.0;
const SWARM_MIN_COVERAGE: f32 = 0.45;
const SWARM_LIT_SUN: f32 = 0.02;
const MOTE_MIN_PX: f32 = 2.4;
const MOTE_MIN_COVERAGE: f32 = 0.12;
// A bird's body against its wingspan, and the least width a wing or a body keeps on screen.
const BIRD_BODY_LENGTH: f32 = 0.6;
const BIRD_BODY_WIDTH: f32 = 0.16;
const BIRD_MIN_WIDTH_PX: f32 = 1.6;
const SALT_MOTE: u32 = 0x6d6f7465u;
const SALT_SWARM: u32 = 0x7377726du;
const SALT_MEMBER: u32 = 0x6d656d62u;
const SALT_BUTTERFLY: u32 = 0x62757466u;
const SALT_FIREFLY: u32 = 0x66697265u;
const SALT_LEAF: u32 = 0x6c656166u;

fn hash_step(state: u32) -> u32 {
    let word = state * 747796405u + 2891336453u;
    let mixed = ((word >> ((word >> 28u) + 4u)) ^ word) * 277803737u;
    return (mixed >> 22u) ^ mixed;
}

fn unit(bits: u32) -> f32 {
    return f32(bits >> 8u) / 16777216.0;
}

fn ambient_hash4(index: u32, salt: u32) -> vec4<f32> {
    let a = hash_step(index ^ salt ^ hash_step(amb.seed.x));
    let b = hash_step(a);
    let c = hash_step(b);
    let d = hash_step(c);
    return vec4<f32>(unit(a), unit(b), unit(c), unit(d));
}

// Whole cycles per loop for a frequency in lo..hi Hz picked by h, as Hz.
fn looped_hz(lo: f32, hi: f32, h: f32) -> f32 {
    return max(round((lo + (hi - lo) * h) * LOOP_S), 1.0) / LOOP_S;
}

fn clock() -> f32 {
    return amb.target_info.z;
}

struct Wrapped {
    position: vec3<f32>,
    fade: f32,
}

// dims: (half extent, half height, center height over the eye).
fn wrapped(seed: vec4<f32>, offset: vec3<f32>, dims: vec3<f32>, eye_mod: vec3<f32>) -> Wrapped {
    let half_size = vec3<f32>(dims.x, dims.y, dims.x);
    let b = half_size * 2.0;
    let shift = vec3<f32>(0.0, dims.z, 0.0);
    let x = b * seed.xyz + offset;
    let c = eye_mod + shift;
    let local = b * fract((x - c) / b) - half_size;
    let e = abs(local) / half_size;
    let edge = max(e.x, max(e.y, e.z));
    return Wrapped(shift + local, 1.0 - smoothstep(1.0 - EDGE_FADE, 1.0, edge));
}

fn count_fade(index: u32, count: f32) -> f32 {
    if (count <= 0.0) {
        return 0.0;
    }
    return min(1.0 - smoothstep((1.0 - COUNT_FADE) * count, count, f32(index)), clamp(count - f32(index), 0.0, 1.0));
}

struct CreatureOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color_alpha: vec4<f32>,
    @location(2) view_depth: f32,
    // 1 adds light (motes, fireflies), 0 covers (insects, wings, leaves).
    @location(3) additive: f32,
}

fn culled() -> CreatureOut {
    return CreatureOut(vec4<f32>(0.0, 0.0, 2.0, 1.0), vec2<f32>(0.0), vec4<f32>(0.0), 0.0, 0.0);
}

// Two triangles: (0, 1, 2) and (3, 4, 5). No lookup table: an indexed function-local array is
// spilled to memory, which Mali's driver loses the device over (`tests/shader_rules.rs`).
fn corner(vertex: u32) -> vec2<f32> {
    let right = vertex == 1u || vertex == 4u || vertex == 5u;
    let top = vertex == 2u || vertex == 3u || vertex == 5u;
    return vec2<f32>(select(-1.0, 1.0, right), select(-1.0, 1.0, top));
}

// 1 where the sun reaches the eye-relative point, 0 in the shade of the first occluder toward the
// sun; 1 everywhere without the view.
fn sun_open(relative: vec3<f32>) -> f32 {
    if (amb.occ_info.z < 0.5) {
        return 1.0;
    }
    let clip = amb.occ_view_proj * vec4<f32>(amb.eye.xyz + relative, 1.0);
    let ndc = clip.xyz / clip.w;
    let local = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (local.x < 0.0 || local.y < 0.0 || local.x >= 1.0 || local.y >= 1.0) {
        return 1.0;
    }
    let texel = vec2<i32>((local * amb.occ_rect.xy + amb.occ_rect.zw) * amb.occ_info.x);
    let stored = textureLoad(occ_atlas, texel, 0);
    return select(0.0, 1.0, min(ndc.z, 1.0) <= stored + 1.0e-4);
}

fn hemisphere_light() -> vec3<f32> {
    if (amb.sky_up.w > 0.5) {
        return (amb.sky_up.rgb + amb.sky_down.rgb) * 0.5;
    }
    return vec3<f32>(amb.sun_color.w);
}

fn hg_phase(g: f32, cos_theta: f32) -> f32 {
    let d = 1.0 + g * g - 2.0 * g * cos_theta;
    return (1.0 - g * g) / (12.566371 * pow(max(d, 1.0e-4), 1.5));
}

// A camera-facing square around `center` of `size_m`, at least `min_px` pixels wide; a smaller
// one pays for the widening in coverage (area), down to `min_coverage`.
fn billboard(center: vec3<f32>, size_m: f32, min_px: f32, min_coverage: f32, c: vec2<f32>, color: vec3<f32>, alpha: f32, additive: f32) -> CreatureOut {
    let depth = max(dot(center, amb.forward.xyz), amb.right.w);
    let pixel_m = depth / max(amb.forward.w, 1.0);
    let wide = max(size_m, pixel_m * min_px);
    let ratio = size_m / wide;
    let coverage = max(ratio * ratio, min_coverage);
    let world = center + (amb.right.xyz * c.x + amb.up.xyz * c.y) * (0.5 * wide);
    return CreatureOut(
        amb.view_proj * vec4<f32>(world, 1.0),
        c,
        vec4<f32>(color, alpha * coverage),
        dot(world, amb.forward.xyz),
        additive,
    );
}

// The same widening `billboard` applies, read here to cap the light per pixel: a near glint against
// the sun is as bright as a white surface in the sun, and does not blow out.
fn capped_glint(position: vec3<f32>, size_m: f32, min_px: f32, min_coverage: f32, glint: f32) -> f32 {
    let pixel_m = max(dot(position, amb.forward.xyz), amb.right.w) / max(amb.forward.w, 1.0);
    let ratio = size_m / max(size_m, pixel_m * min_px);
    let coverage = max(ratio * ratio, min_coverage);
    return min(glint * coverage, 1.0) / coverage;
}

@vertex
fn vs_mote(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    let h = ambient_hash4(index, SALT_MOTE);
    let w = wrapped(h, amb.mote_drift.xyz, MOTE_BOX, amb.eye_mod_motes.xyz);
    let alpha = w.fade * count_fade(index, amb.counts_a.x);
    if (alpha <= 0.0) {
        return culled();
    }
    let t = clock();
    let a = TAU * (looped_hz(0.08, 0.2, h.w) * t + h.x);
    let position = w.position + vec3<f32>(0.08 * sin(a), 0.05 * sin(2.0 * a + h.y * TAU), 0.08 * cos(a));
    // The size is what the eye and a lens see, the flake with the glare around it, not its body: a
    // real millimetre fleck is a dot a pixel wide at most at film resolution, and no one sees it.
    let size = 0.0025 + 0.0055 * h.w * h.w;
    // A broad side glow and a strong forward lobe, and a twinkle as the flake turns.
    let view = normalize(position);
    let scatter = (0.25 + hg_phase(0.65, dot(amb.sun_dir.xyz, view))) * amb.sun_dir.w * sun_open(position);
    let twinkle = 0.55 + 0.45 * sin(TAU * (looped_hz(1.0, 3.0, h.y) * t + h.z));
    let glow = capped_glint(position, size, MOTE_MIN_PX, MOTE_MIN_COVERAGE, scatter * twinkle * MOTE_GAIN);
    let color = amb.sun_color.rgb * glow + hemisphere_light() * 0.03;
    // A fleck right at the lens would be a large disc: it fades out before that.
    let close = smoothstep(0.4, 1.6, length(position));
    return billboard(position, size, MOTE_MIN_PX, MOTE_MIN_COVERAGE, corner(vertex), color, alpha * close, 1.0);
}

@vertex
fn vs_swarm(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    let swarm = index / SWARM_MEMBERS;
    let member = index % SWARM_MEMBERS;
    let hs = ambient_hash4(swarm, SALT_SWARM);
    let flies = hs.w > 0.7;
    if (flies && member >= FLY_SWARM_MEMBERS) {
        return culled();
    }
    let w = wrapped(hs, vec3<f32>(0.0), SWARM_BOX, amb.eye_mod_swarms.xyz);
    let alpha = w.fade * count_fade(index, amb.counts_a.y);
    if (alpha <= 0.0) {
        return culled();
    }
    let hm = ambient_hash4(index, SALT_MEMBER);
    let t = clock();
    var radius = 0.3 + 0.25 * hm.w;
    var lo = 0.6;
    var hi = 1.6;
    var size = 0.003;
    if (flies) {
        radius = 1.4;
        lo = 0.4;
        hi = 0.9;
        size = 0.009;
    }
    let ax = TAU * (looped_hz(lo, hi, hm.x) * t + hm.y);
    let ay = TAU * (looped_hz(lo, hi, hm.y) * t + hm.z);
    let az = TAU * (looped_hz(lo, hi, hm.z) * t + hm.x);
    let position = w.position + vec3<f32>(radius * sin(ax), 0.5 * radius * sin(ay), radius * sin(az));
    let sun = amb.sun_dir.w * sun_open(position);
    if (sun <= SWARM_LIT_SUN) {
        // In the shade, a dark speck.
        return billboard(position, size, SWARM_MIN_PX, SWARM_MIN_COVERAGE, corner(vertex), hemisphere_light() * 0.04, alpha * 0.9, 0.0);
    }
    // In the sun, wings glint as a mote does and shimmer as they beat: that is how a viewer sees a
    // swarm at all, a dark speck on a dark forest being invisible.
    let scatter = (0.25 + hg_phase(0.65, dot(amb.sun_dir.xyz, normalize(position)))) * sun;
    let shimmer = 0.55 + 0.45 * sin(TAU * (looped_hz(3.0, 8.0, hm.w) * t + hm.x));
    let glow = capped_glint(position, size, SWARM_MIN_PX, SWARM_MIN_COVERAGE, scatter * shimmer * SWARM_GAIN);
    return billboard(position, size, SWARM_MIN_PX, SWARM_MIN_COVERAGE, corner(vertex), amb.sun_color.rgb * glow, alpha, 1.0);
}

@vertex
fn vs_firefly(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    let h = ambient_hash4(index, SALT_FIREFLY);
    let w = wrapped(h, vec3<f32>(0.0), FIREFLY_BOX, amb.eye_mod_fireflies.xyz);
    let alpha = w.fade * count_fade(index, amb.counts_a.w);
    if (alpha <= 0.0) {
        return culled();
    }
    let t = clock();
    let a = TAU * (looped_hz(0.03, 0.08, h.w) * t + h.x);
    let position = w.position + vec3<f32>(0.6 * sin(a), 0.25 * sin(2.0 * a), 0.6 * cos(a));
    let f = looped_hz(1.0 / 6.0, 1.0 / 2.5, h.w);
    let period = 1.0 / f;
    let phase = fract(f * t + h.y);
    let on = 0.3 / period;
    let glow = smoothstep(0.0, 0.08 / period, phase) * (1.0 - smoothstep(on, on + 0.2 / period, phase));
    if (glow <= 0.0) {
        return culled();
    }
    // A cold yellow-green (bioluminescence peaks near 560 nm), sized as its halo.
    let color = vec3<f32>(0.55, 1.0, 0.18) * glow * 3.0;
    return billboard(position, 0.06, 4.0, 0.35, corner(vertex), color, alpha, 1.0);
}

// One of two wing quads (vertices 0..5 the left wing, 6..11 the right), hinged on the body axis
// along `heading`, raised by `angle` over the body plane. uv.x runs root (0) to tip (1), uv.y
// tail (-1) to head (1).
struct Wing {
    world: vec3<f32>,
    uv: vec2<f32>,
    normal: vec3<f32>,
    // Root to tip, and tail to head.
    span_dir: vec3<f32>,
    chord_dir: vec3<f32>,
}

fn wing_vertex(vertex: u32, center: vec3<f32>, heading: vec3<f32>, angle: f32, span: f32, chord: f32) -> Wing {
    let fwd = normalize(heading + vec3<f32>(1.0e-5, 0.0, 0.0));
    let side = normalize(cross(fwd, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(0.0, 0.0, 1.0e-5));
    let up = cross(side, fwd);
    let c = corner(vertex % 6u);
    var dir = side * cos(angle) + up * sin(angle);
    if (vertex >= 6u) {
        dir = -side * cos(angle) + up * sin(angle);
    }
    let s = c.x * 0.5 + 0.5;
    let world = center + dir * (s * span) + fwd * (c.y * 0.5 * chord);
    return Wing(world, vec2<f32>(s, c.y), normalize(cross(dir, fwd)), dir, fwd);
}

// Temperate wings: pale whites, sulfur yellow, orange, a soft blue, and brown.
fn butterfly_color(pick: f32) -> vec3<f32> {
    if (pick < 0.33) {
        return vec3<f32>(0.85, 0.85, 0.78);
    }
    if (pick < 0.53) {
        return vec3<f32>(0.85, 0.74, 0.28);
    }
    if (pick < 0.76) {
        return vec3<f32>(0.78, 0.36, 0.08);
    }
    if (pick < 0.86) {
        return vec3<f32>(0.38, 0.46, 0.72);
    }
    return vec3<f32>(0.3, 0.19, 0.1);
}

fn lit_two_sided(albedo: vec3<f32>, normal: vec3<f32>, view: vec3<f32>) -> vec3<f32> {
    let l = amb.sun_dir.xyz;
    let n_dot_l = dot(normal, l);
    // The side the eye sees; sun on the far side shines through (thin wing or leaf).
    let facing = select(-1.0, 1.0, dot(normal, -view) >= 0.0);
    let front = max(n_dot_l * facing, 0.0);
    let through = max(-n_dot_l * facing, 0.0) * 0.45;
    let sun = amb.sun_color.rgb * (front + through) * amb.sun_dir.w;
    return albedo * (sun + hemisphere_light());
}

@vertex
fn vs_butterfly(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    let h = ambient_hash4(index, SALT_BUTTERFLY);
    let w = wrapped(h, vec3<f32>(0.0), BUTTERFLY_BOX, amb.eye_mod_butterflies.xyz);
    let alpha = w.fade * count_fade(index, amb.counts_a.z);
    if (alpha <= 0.0) {
        return culled();
    }
    let t = clock();
    let f1 = looped_hz(0.05, 0.11, h.x);
    let f2 = looped_hz(0.13, 0.23, h.y);
    let fy = looped_hz(0.3, 0.6, h.z);
    let a1 = TAU * (f1 * t + h.y);
    let a2 = TAU * (f2 * t + h.z);
    let ay = TAU * (fy * t + h.w);
    let offset = vec3<f32>(3.0 * sin(a1) + 1.2 * cos(a2), 0.35 * sin(ay), 3.0 * cos(a1) + 1.2 * sin(a2));
    let velocity = vec3<f32>(
        TAU * (3.0 * f1 * cos(a1) - 1.2 * f2 * sin(a2)),
        TAU * 0.35 * fy * cos(ay),
        TAU * (-3.0 * f1 * sin(a1) + 1.2 * f2 * cos(a2)),
    );
    let center = w.position + offset;
    let flap = sin(TAU * (looped_hz(5.0, 9.0, h.z) * t + h.x));
    let glide = smoothstep(0.55, 0.75, sin(TAU * (looped_hz(0.2, 0.4, h.y) * t + h.w)));
    let angle = (0.15 + 0.6 * (1.0 + flap)) * (1.0 - glide) + 0.35 * glide;
    let span = 0.03 + 0.02 * h.w;
    let wing = wing_vertex(vertex, center, velocity, angle, span, span * 1.3);
    let color = lit_two_sided(butterfly_color(h.y), wing.normal, normalize(center));
    // Far butterflies keep at least about a pixel and fade instead.
    let depth = max(dot(center, amb.forward.xyz), amb.right.w);
    let px = span * amb.forward.w / depth;
    let coverage = clamp(px / 1.5, 0.5, 1.0);
    return CreatureOut(
        amb.view_proj * vec4<f32>(wing.world, 1.0),
        wing.uv,
        vec4<f32>(color, alpha * coverage),
        dot(wing.world, amb.forward.xyz),
        0.0,
    );
}

fn leaf_color(season: f32, pick: f32) -> vec3<f32> {
    if (season < 0.5) {
        // Spring: fresh greens.
        return select(vec3<f32>(0.2, 0.36, 0.07), vec3<f32>(0.34, 0.42, 0.12), pick > 0.6);
    }
    if (season < 1.5) {
        // Summer: greens and the odd early yellow.
        if (pick < 0.45) {
            return vec3<f32>(0.1, 0.25, 0.05);
        }
        if (pick < 0.85) {
            return vec3<f32>(0.07, 0.16, 0.04);
        }
        return vec3<f32>(0.4, 0.38, 0.1);
    }
    if (season < 2.5) {
        // Autumn: oranges, reds, yellows, browns.
        if (pick < 0.3) {
            return vec3<f32>(0.55, 0.2, 0.03);
        }
        if (pick < 0.5) {
            return vec3<f32>(0.45, 0.06, 0.03);
        }
        if (pick < 0.75) {
            return vec3<f32>(0.6, 0.42, 0.05);
        }
        return vec3<f32>(0.25, 0.13, 0.05);
    }
    // Winter: dry browns.
    return select(vec3<f32>(0.2, 0.12, 0.06), vec3<f32>(0.18, 0.15, 0.1), pick > 0.5);
}

// Rotates `v` about the unit `axis` by `angle` (Rodrigues).
fn rotate(v: vec3<f32>, axis: vec3<f32>, angle: f32) -> vec3<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return v * c + cross(axis, v) * s + axis * dot(axis, v) * (1.0 - c);
}

@vertex
fn vs_leaf(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    let k = index % 4u;
    let h = ambient_hash4(index, SALT_LEAF);
    let w = wrapped(h, vec3<f32>(amb.leaf_drift.x, -amb.leaf_fall[k], amb.leaf_drift.y), LEAF_BOX, amb.eye_mod_leaves.xyz);
    let alpha = w.fade * count_fade(index, amb.counts_b.x);
    if (alpha <= 0.0) {
        return culled();
    }
    let t = clock();
    let sway = 0.3 + 0.4 * h.w;
    let a = TAU * (looped_hz(0.35, 0.8, h.x) * t + h.y);
    let dir = TAU * h.z;
    let s = sway * sin(a);
    let center = w.position + vec3<f32>(s * cos(dir), 0.15 * sway * cos(2.0 * a), s * sin(dir));
    let size = 0.04 + 0.03 * h.w;
    // The leaf rocks with its sway (the pendulum of a falling plate) and slowly tumbles.
    let axis = normalize(vec3<f32>(h.y - 0.5, 0.3, h.x - 0.5));
    let tumble = TAU * (looped_hz(0.1, 0.5, h.z) * t + h.w);
    let rock = 0.8 * cos(a);
    let e1 = rotate(rotate(vec3<f32>(1.0, 0.0, 0.0), axis, tumble), vec3<f32>(cos(dir), 0.0, sin(dir)), rock);
    let e2 = rotate(rotate(vec3<f32>(0.0, 0.0, 1.0), axis, tumble), vec3<f32>(cos(dir), 0.0, sin(dir)), rock);
    let c = corner(vertex);
    let world = center + (e1 * c.x * 0.35 + e2 * c.y * 0.5) * size;
    let normal = normalize(cross(e1, e2));
    let color = lit_two_sided(leaf_color(amb.target_info.w, h.y), normal, normalize(center));
    let depth = max(dot(center, amb.forward.xyz), amb.right.w);
    let px = size * amb.forward.w / depth;
    let coverage = clamp(px / 1.5, 0.3, 1.0);
    return CreatureOut(
        amb.view_proj * vec4<f32>(world, 1.0),
        c,
        vec4<f32>(color, alpha * coverage),
        dot(world, amb.forward.xyz),
        0.0,
    );
}

@vertex
fn vs_bird(@builtin(vertex_index) vertex: u32, @builtin(instance_index) index: u32) -> CreatureOut {
    if (f32(index) >= amb.counts_b.y) {
        return culled();
    }
    let bird = amb.birds[index];
    let alpha = bird.color_alpha.a;
    if (alpha <= 0.0) {
        return culled();
    }
    let center = bird.position_size.xyz;
    // A far bird keeps a wing of 2 pixels by 1.2 and pays for the widening in coverage, down to
    // a third, so a distant flock still reads as specks against the sky.
    let depth = max(dot(center, amb.forward.xyz), amb.right.w);
    let pixel_m = depth / max(amb.forward.w, 1.0);
    let true_span = 0.5 * bird.position_size.w;
    let span = max(true_span, 2.0 * pixel_m);
    let chord = max(0.45 * span, 1.2 * pixel_m);
    let coverage = clamp((true_span * true_span * 0.45) / (span * chord), 0.35, 1.0);
    let view = normalize(center);
    if (vertex >= 12u) {
        return bird_body(vertex, center, bird, view, pixel_m, alpha * coverage);
    }
    let wing = wing_vertex(vertex, center, bird.heading_wing.xyz, bird.heading_wing.w, span, chord);
    // Seen in its own plane a wing has no area and draws nothing, as a gliding bird at about the
    // eye's height does: the quad keeps a width across the wing's line on screen, as thick as a
    // wing is, and at least enough pixels to cover a row.
    let across = cross(wing.span_dir, view);
    let across_len = length(across);
    let perpendicular = across / max(across_len, 1.0e-4);
    let along_across = dot(wing.chord_dir, perpendicular);
    let least = max(0.2 * chord, BIRD_MIN_WIDTH_PX * pixel_m) * smoothstep(0.0, 0.2, across_len);
    let extra = max(least - chord * abs(along_across), 0.0);
    // Both wings widen the same way up when the chord has no direction on screen (seen head-on).
    let side = select(-1.0, 1.0, along_across + 0.01 * perpendicular.y >= 0.0);
    let world = wing.world + perpendicular * side * wing.uv.y * 0.5 * extra;
    let color = lit_two_sided(bird.color_alpha.rgb, wing.normal, view);
    return CreatureOut(
        amb.view_proj * vec4<f32>(world, 1.0),
        wing.uv,
        vec4<f32>(color, alpha * coverage),
        dot(world, amb.forward.xyz),
        0.0,
    );
}

// The body, vertices 12..17: a quad facing the eye along the heading as the eye sees it, so a bird
// reads from every side. Its uv.x lies in -3..-1, which tells `fs_wing` it is a body.
fn bird_body(vertex: u32, center: vec3<f32>, bird: Bird, view: vec3<f32>, pixel_m: f32, alpha: f32) -> CreatureOut {
    let wingspan = bird.position_size.w;
    let heading = bird.heading_wing.xyz;
    let seen = heading - view * dot(heading, view);
    let seen_len = length(seen);
    // Head-on the heading has no direction on screen, and the body is as long as it is wide.
    let axis = select(amb.up.xyz, seen / max(seen_len, 1.0e-4), seen_len > 1.0e-3);
    let side = normalize(cross(axis, view));
    let width = max(BIRD_BODY_WIDTH * wingspan, BIRD_MIN_WIDTH_PX * pixel_m);
    let body_length = max(BIRD_BODY_LENGTH * wingspan * seen_len, width);
    let c = corner(vertex % 6u);
    let world = center + axis * (c.y * 0.5 * body_length) + side * (c.x * 0.5 * width);
    // Lit as the side that faces the eye.
    let color = lit_two_sided(bird.color_alpha.rgb, -view, view);
    return CreatureOut(
        amb.view_proj * vec4<f32>(world, 1.0),
        vec2<f32>(c.x - 2.0, c.y),
        vec4<f32>(color, alpha),
        dot(world, amb.forward.xyz),
        0.0,
    );
}

// Soft fade against the scene depth, so a creature does not cut into the ground.
fn soft_fade(position: vec4<f32>, view_depth: f32) -> f32 {
    let stored = textureLoad(scene_depth, vec2<i32>(position.xy), 0);
    let near = amb.right.w;
    let far = amb.up.w;
    let scene = near * far / (far - stored * (far - near));
    return clamp((scene - view_depth) / 0.1, 0.0, 1.0);
}

fn encode(color: vec3<f32>, alpha: f32, additive: f32) -> vec4<f32> {
    let exposure = max(amb.target_info.x, 1.0e-8);
    if (additive > 0.5) {
        // Light only: the coverage scales the energy before the display clamp, so a bright
        // sub-pixel glint keeps its brightness instead of being clamped to its coverage.
        var light = color * alpha / exposure;
        if (amb.target_info.y < 0.5) {
            light = pow(clamp(light, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
        }
        return vec4<f32>(light, 0.0);
    }
    var c = color / exposure;
    if (amb.target_info.y < 0.5) {
        c = pow(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
    }
    // Premultiplied.
    return vec4<f32>(c * alpha, alpha);
}

@fragment
fn fs_dot(in: CreatureOut) -> @location(0) vec4<f32> {
    let r2 = dot(in.uv, in.uv);
    let round_sprite = 1.0 - smoothstep(0.25, 1.0, r2);
    let alpha = in.color_alpha.a * round_sprite * soft_fade(in.clip, in.view_depth);
    return encode(in.color_alpha.rgb, alpha, in.additive);
}

// A firefly: a hot center and a soft halo, no edge. It clamps at the center on a display target.
@fragment
fn fs_glow(in: CreatureOut) -> @location(0) vec4<f32> {
    let falloff = max(1.0 - dot(in.uv, in.uv), 0.0);
    let alpha = in.color_alpha.a * falloff * falloff * falloff * soft_fade(in.clip, in.view_depth);
    return encode(in.color_alpha.rgb, alpha, 1.0);
}

// An edge's coverage, from how far inside a shape this pixel is (`inside`, in the shape's own units).
// The layer draws after the temporal resolve, so nothing else softens it: the edge is at least a
// pixel wide (`pixel`, `fwidth` of the coordinate) and centered on the shape's line when the pixel
// is what limits it, and `soft` wide inside the line when the shape is large enough to show that.
fn edge_coverage(inside: f32, soft: f32, pixel: f32) -> f32 {
    return smoothstep(-0.5 * pixel, max(soft, 0.5 * pixel), inside);
}

// A bird's wing: rounded fore and hind lobes from the root (uv.x 0) to the tip (uv.x 1). A body has
// uv.x in -3..-1: an egg, fattest toward the head at uv.y 1.
@fragment
fn fs_wing(in: CreatureOut) -> @location(0) vec4<f32> {
    // Taken before the branch: a derivative needs the same control flow in every pixel of a quad.
    let pixel = fwidth(in.uv);
    if (in.uv.x < -0.5) {
        let y = in.uv.y;
        let half_width = sqrt(max(1.0 - y * y, 0.0)) * (0.75 + 0.25 * y);
        let body = edge_coverage(half_width - abs(in.uv.x + 2.0), 0.3 * half_width, pixel.x);
        return encode(in.color_alpha.rgb, in.color_alpha.a * body * soft_fade(in.clip, in.view_depth), 0.0);
    }
    let s = in.uv.x;
    let along = in.uv.y;
    // The chord narrows to the tip and sweeps back.
    let half_chord = (1.0 - 0.55 * s) * (1.0 - s * s * 0.5);
    let swept = along + 0.35 * s;
    let inside = edge_coverage(half_chord - abs(swept), 0.2 * half_chord, pixel.y + 0.35 * pixel.x);
    let tip = edge_coverage(1.0 - s, 0.15, pixel.x);
    let alpha = in.color_alpha.a * inside * tip * soft_fade(in.clip, in.view_depth);
    return encode(in.color_alpha.rgb, alpha, 0.0);
}

// A butterfly's wing: a broad forewing and a rounder hindwing from the root (uv.x 0), with a dark
// margin, a pale spot near the tip, and the dark body along the hinge.
@fragment
fn fs_butterfly(in: CreatureOut) -> @location(0) vec4<f32> {
    // Wing space in true proportions: x from the root to the tip, y from tail to head.
    let p = vec2<f32>(in.uv.x, in.uv.y * 0.65);
    // The forewing: a triangle with rounded corners and a pointed tip, its leading edge bowed.
    let root = vec2<f32>(-0.1, 0.04);
    let apex = vec2<f32>(0.93, 0.46);
    let tornus = vec2<f32>(0.7, -0.14);
    let along = clamp(p.x / apex.x, 0.0, 1.0);
    let leading = edge_distance(p, apex, root) - 0.16 * along * (1.0 - along);
    let outer = edge_distance(p, tornus, apex);
    let trailing = edge_distance(p, root, tornus);
    let fore = smooth_max(smooth_max(leading, outer, 0.05), trailing, 0.05);
    // The hindwing: round, with a scalloped edge.
    let h = (p - vec2<f32>(0.24, -0.23)) / vec2<f32>(0.31, 0.29);
    let hind = (length(h) - 1.0 + 0.02 * cos(atan2(h.y, h.x) * 10.0)) * 0.3;
    let d = min(fore, hind);
    let aa = max(fwidth(d), 0.004);
    let inside = 1.0 - smoothstep(-aa, aa, d);
    // Dark: the margin, the forewing's tip, the seam where the wings overlap, and the hairy root.
    let margin = smoothstep(-0.075, -0.028, d) * 0.9;
    let tip = (1.0 - smoothstep(0.14, 0.36, length(p - apex))) * step(fore, 0.0);
    let seam = (1.0 - smoothstep(0.0, 0.02, abs(fore - hind))) * step(max(fore, hind), 0.0);
    let hairs = (1.0 - smoothstep(0.05, 0.26, length(p))) * 0.45;
    // Veins: rays from the root, gone when they would be finer than a pixel.
    let angle = atan2(p.y, p.x) * 3.2;
    let ray = abs(fract(angle) - 0.5) * 2.0;
    let fine = 1.0 - smoothstep(0.08, 0.3, fwidth(angle));
    let veins = (1.0 - smoothstep(0.0, 0.1, ray)) * fine * smoothstep(0.1, 0.25, length(p)) * 0.35;
    let dark = max(max(max(margin, tip), max(seam, hairs)), veins);
    // Pale: spots on the forewing's tip and in the hindwing's margin, and the fringe at the very edge.
    let spots = max(
        max(
            1.0 - smoothstep(0.03, 0.055, length(p - vec2<f32>(0.74, 0.3))),
            1.0 - smoothstep(0.02, 0.04, length(p - vec2<f32>(0.62, 0.2))),
        ),
        1.0 - smoothstep(0.022, 0.042, length(p - vec2<f32>(0.36, -0.42))),
    );
    let fringe = smoothstep(-0.012, -0.004, d) * 0.35;
    let pale = max(spots, fringe);
    let lit = in.color_alpha.rgb;
    let grey = vec3<f32>(dot(lit, vec3<f32>(0.2126, 0.7152, 0.0722)));
    var color = mix(lit, lit * 0.1 + grey * 0.03, dark);
    color = mix(color, mix(lit, grey, 0.75) * 1.35, pale);
    // The body: dark, between the wings, along the hinge.
    let body_d = length(p - vec2<f32>(0.0, clamp(p.y, -0.3, 0.3))) - 0.028;
    let body = 1.0 - smoothstep(-aa, aa, body_d);
    color = mix(color, grey * 0.1, body);
    let alpha = in.color_alpha.a * max(inside, body) * soft_fade(in.clip, in.view_depth);
    return encode(color, alpha, 0.0);
}

// How far `p` lies outside the edge from `a` to `b`, with the shape on the edge's left.
fn edge_distance(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let e = normalize(b - a);
    return dot(p - a, vec2<f32>(e.y, -e.x));
}

fn smooth_max(a: f32, b: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (a - b) / k, 0.0, 1.0);
    return mix(b, a, h) + k * h * (1.0 - h);
}

// A leaf: pointed at both ends, widest a little below the middle.
@fragment
fn fs_leaf(in: CreatureOut) -> @location(0) vec4<f32> {
    let y = in.uv.y;
    let half_width = (1.0 - y * y) * (0.9 - 0.2 * y);
    let inside = 1.0 - smoothstep(half_width * 0.85, half_width, abs(in.uv.x));
    let alpha = in.color_alpha.a * inside * soft_fade(in.clip, in.view_depth);
    // A midrib and side veins, gone when they would be finer than a pixel. One half a little darker
    // than the other, and a drier edge.
    let fine = 1.0 - smoothstep(0.1, 0.3, fwidth(y));
    let rib = 1.0 - smoothstep(0.0, 0.07, abs(in.uv.x));
    let veins = 1.0 - smoothstep(0.0, 0.2, abs(fract((y - abs(in.uv.x) * 0.7) * 3.5) - 0.5) * 2.0);
    let side = select(1.0, 0.86, in.uv.x > 0.0);
    let edge = smoothstep(half_width * 0.45, half_width * 0.85, abs(in.uv.x));
    let shade = side * mix(1.0, 0.8, edge) * (1.0 - (0.4 * rib + 0.2 * veins) * fine);
    return encode(in.color_alpha.rgb * shade, alpha, 0.0);
}
