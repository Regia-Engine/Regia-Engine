// SPDX-License-Identifier: MPL-2.0
// Regia cheap looks (ADR-2538): metal, anodized metal, faceted gem, cabochon, opal, frosted.
//
// WebGL2-safe by construction: uniform buffers and textures only, no storage, no compute, no loop
// whose count depends on data, at most 5 environment samples per gem fragment. Units are
// millimeters, +y up, +z toward the default front view. The studio (environment and key lights)
// is fixed in world space; the piece turns, the studio does not.

struct Frame {
    view_proj: mat4x4<f32>,
    // xyz: eye position (mm); w: exposure.
    eye: vec4<f32>,
    // Three fixed studio key lights: xyz direction toward the light, w unused.
    key_dir: array<vec4<f32>, 3>,
    // rgb radiance, w unused.
    key_col: array<vec4<f32>, 3>,
    // x: environment intensity; y: max mip; z: anodize table rows; w: 1 = encode sRGB in the shader.
    env: vec4<f32>,
};

struct Object {
    model: mat4x4<f32>,
    p0: vec4<f32>,
    p1: vec4<f32>,
    p2: vec4<f32>,
    p3: vec4<f32>,
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var env_tex: texture_cube<f32>;
@group(0) @binding(2) var env_samp: sampler;
@group(0) @binding(3) var lut_tex: texture_2d<f32>;
@group(1) @binding(0) var<uniform> obj: Object;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) facet: f32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_n: vec3<f32>,
    @location(2) obj_pos: vec3<f32>,
    @location(3) obj_n: vec3<f32>,
    @location(4) @interpolate(flat) facet: f32,
};

const PI: f32 = 3.14159265;
const TAU: f32 = 6.28318531;

@vertex
fn vs_main(v: VsIn) -> VsOut {
    let wp = obj.model * vec4<f32>(v.position, 1.0);
    let m3 = mat3x3<f32>(obj.model[0].xyz, obj.model[1].xyz, obj.model[2].xyz);
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return VsOut(frame.view_proj * wp, wp.xyz, normalize(m3 * v.normal), v.position, v.normal, v.facet);
}

// ---------------------------------------------------------------------------------------------
// Shared helpers

fn env_at(dir: vec3<f32>, lod: f32) -> vec3<f32> {
    return textureSampleLevel(env_tex, env_samp, dir, lod).rgb * frame.env.x;
}

// Narkowicz 2015 fit of the ACES filmic curve, then optional sRGB encoding.
fn finish_color(c: vec3<f32>) -> vec4<f32> {
    let x = max(c * frame.eye.w, vec3<f32>(0.0));
    let t = clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = t * 12.92;
    let hi = 1.055 * pow(t, vec3<f32>(1.0 / 2.4)) - 0.055;
    let srgb = select(hi, lo, t <= vec3<f32>(0.0031308));
    return vec4<f32>(select(t, srgb, frame.env.w > 0.5), 1.0);
}

// Low-bias integer hash (stable across frames and backends).
fn hash_u(x: u32) -> u32 {
    var h = x;
    h = h ^ (h >> 16u);
    h = h * 0x7feb352du;
    h = h ^ (h >> 15u);
    h = h * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return h;
}

fn hash01(x: u32) -> f32 {
    return f32(hash_u(x) & 0xffffu) / 65535.0;
}

fn object_dir(world_dir: vec3<f32>) -> vec3<f32> {
    // The model is rotation times uniform scale, so its transpose maps directions back.
    let m3 = mat3x3<f32>(obj.model[0].xyz, obj.model[1].xyz, obj.model[2].xyz);
    return normalize(transpose(m3) * world_dir);
}

fn world_dir(object_d: vec3<f32>) -> vec3<f32> {
    let m3 = mat3x3<f32>(obj.model[0].xyz, obj.model[1].xyz, obj.model[2].xyz);
    return normalize(m3 * object_d);
}

// A mesh-space vector to world space, keeping its length relative to the model's uniform scale.
fn world_dir_raw(object_d: vec3<f32>) -> vec3<f32> {
    let m3 = mat3x3<f32>(obj.model[0].xyz, obj.model[1].xyz, obj.model[2].xyz);
    return m3 * object_d;
}

fn schlick(f0: vec3<f32>, cos_t: f32) -> vec3<f32> {
    let m = pow(1.0 - clamp(cos_t, 0.0, 1.0), 5.0);
    return f0 + (vec3<f32>(1.0) - f0) * m;
}

// F0 + F82 tint (Hoffman 2023, "Generalization of Adobe's Fresnel Model"): Schlick minus a term
// that peaks near 82 degrees, sized so F(cos = 1/7) = tint * Schlick(1/7).
fn fresnel_f82(f0: vec3<f32>, tint: vec3<f32>, cos_t: f32) -> vec3<f32> {
    let mu_bar = 1.0 / 7.0;
    let denom = mu_bar * pow(1.0 - mu_bar, 6.0);
    let fs_bar = schlick(f0, mu_bar);
    let a = fs_bar * (vec3<f32>(1.0) - tint) / denom;
    let c = clamp(cos_t, 0.0, 1.0);
    return clamp(schlick(f0, c) - a * c * pow(1.0 - c, 6.0), vec3<f32>(0.0), vec3<f32>(1.0));
}

// Karis 2014, "Physically Based Shading on Mobile": an analytic fit of the split-sum
// environment BRDF. Returns (scale, bias) for F0.
fn env_brdf_approx(roughness: f32, n_dot_v: f32) -> vec2<f32> {
    let c0 = vec4<f32>(-1.0, -0.0275, -0.572, 0.022);
    let c1 = vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let r = roughness * c0 + c1;
    let a004 = min(r.x * r.x, exp2(-9.28 * n_dot_v)) * r.x + r.y;
    return vec2<f32>(-1.04, 1.04) * a004 + r.zw;
}

fn ggx_spec(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, roughness: f32) -> f32 {
    let a = max(roughness * roughness, 0.002);
    let h = normalize(v + l);
    let n_h = max(dot(n, h), 0.0);
    let n_l = max(dot(n, l), 0.0);
    let n_v = max(dot(n, v), 1e-4);
    let a2 = a * a;
    let d = n_h * n_h * (a2 - 1.0) + 1.0;
    let dist = a2 / (PI * d * d);
    let vis = 0.5 / (n_l * (n_v * (1.0 - a) + a) + n_v * (n_l * (1.0 - a) + a) + 1e-5);
    return dist * vis * n_l;
}

fn anodize_lut(row: f32, cos_t: f32) -> vec3<f32> {
    let w = f32(textureDimensions(lut_tex).x);
    let u = (clamp(cos_t, 0.0, 1.0) * (w - 1.0) + 0.5) / w;
    let v = (row + 0.5) / max(frame.env.z, 1.0);
    return textureSampleLevel(lut_tex, env_samp, vec2<f32>(u, v), 0.0).rgb;
}

// ---------------------------------------------------------------------------------------------
// Metal and anodized metal

@fragment
fn fs_metal(i: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(i.world_n);
    let v = normalize(frame.eye.xyz - i.world_pos);
    let n_v = max(dot(n, v), 1e-4);
    let rough = clamp(obj.p0.w, 0.02, 1.0);
    // A blasted finish (p2.x = grain mm): pits shade the studio, grains catch the key lights.
    let grain_mm = obj.p2.x;
    let footprint = length(fwidth(i.obj_pos)) / max(grain_mm, 1e-3);
    var key_n = n;
    var key_rough = rough;
    var pits = 1.0;
    var key_pits = 1.0;
    if (grain_mm > 0.0) {
        let tilt = looks_grain_tilt(i.obj_pos, grain_mm) * looks_grain_fade(footprint);
        key_n = normalize(n + world_dir_raw(tilt) * 0.3);
        key_rough = rough * 0.85;
        pits = looks_pit_shadow(rough);
        key_pits = mix(1.0, pits, 0.6);
    }
    let r = reflect(-v, n);
    let f0 = obj.p0.rgb;
    let anodized = obj.p1.w >= 0.0;
    let f_env = select(fresnel_f82(f0, obj.p1.rgb, n_v), anodize_lut(max(obj.p1.w, 0.0), n_v), anodized);
    let ab = env_brdf_approx(rough, n_v);
    var c = env_at(r, rough * frame.env.y) * f_env * (ab.x + ab.y) * pits;
    for (var k = 0; k < 3; k = k + 1) {
        let l = normalize(frame.key_dir[k].xyz);
        let h = normalize(v + l);
        let f_l = select(fresnel_f82(f0, obj.p1.rgb, max(dot(l, h), 0.0)), anodize_lut(max(obj.p1.w, 0.0), max(dot(l, h), 0.0)), anodized);
        c = c + frame.key_col[k].rgb * f_l * ggx_spec(key_n, v, l, key_rough) * key_pits;
    }
    return finish_color(c);
}

// ---------------------------------------------------------------------------------------------
// Faceted gem: reflection + fake refraction through two virtual pavilion mirrors, per channel

// The stone in "normalized" coordinates: x by the half length, y and z by the half width, so
// every outline is the unit circle at the girdle. The pavilion is the Tolkowsky cone (40.75
// degrees, apex at the culet) cut into facets by azimuth; the crown is the table plane plus kites.
const GIRDLE: f32 = 0.024;
const PAV_TAN: f32 = 0.8613;
const TABLE_Z: f32 = 0.347;
const TABLE_R: f32 = 0.53;
const SECTOR: f32 = 0.785398;

// Where a ray from inside the stone meets the pavilion cone (fixed math, no loop).
fn cone_hit(p: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    let za = -GIRDLE - PAV_TAN;
    let k2 = 1.0 / (PAV_TAN * PAV_TAN);
    let oz = p.z - za;
    let a = d.x * d.x + d.y * d.y - k2 * d.z * d.z;
    let b = 2.0 * (p.x * d.x + p.y * d.y - k2 * oz * d.z);
    let c = p.x * p.x + p.y * p.y - k2 * oz * oz;
    let sq = sqrt(max(b * b - 4.0 * a * c, 0.0));
    let a2 = select(2.0 * a, 2e-5, abs(a) < 1e-5);
    let t1 = (-b - sq) / a2;
    let t2 = (-b + sq) / a2;
    let tn = min(t1, t2);
    let tf = max(t1, t2);
    let t = select(tf, tn, tn > 1e-4);
    return p + d * clamp(t, 0.0, 4.0);
}

fn facet_normal(az: f32, angle: f32, down: bool) -> vec3<f32> {
    let s = sin(angle);
    let c = cos(angle);
    return vec3<f32>(cos(az) * s, sin(az) * s, select(c, -c, down));
}

// The pavilion facet under a point: 8 mains (kites that narrow toward the girdle) and 16 lower
// halves beside them.
fn pavilion_normal(h: vec3<f32>, turn: f32) -> vec3<f32> {
    let az = atan2(h.y, h.x) + turn;
    let r = length(h.xy);
    let sector = floor(az / SECTOR + 0.5);
    let main_az = sector * SECTOR;
    let off = az - main_az;
    let main_half = 0.5 * SECTOR * clamp((1.0 - r) / 0.75, 0.0, 1.0);
    let is_half = abs(off) > main_half;
    let half_az = main_az + sign(off) * 0.25 * SECTOR;
    let n_main = facet_normal(main_az - turn, 0.7112, true);
    let n_half = facet_normal(half_az - turn, 0.7392, true);
    return select(n_main, n_half, is_half);
}

// The crown facet a ray leaves through: the table, 8 stars, 8 bezels and 16 upper halves.
fn crown_normal(e: vec3<f32>, turn: f32) -> vec3<f32> {
    let az = atan2(e.y, e.x) + turn;
    let r = length(e.xy);
    let sector = floor(az / SECTOR + 0.5);
    let main_az = sector * SECTOR;
    let off = az - main_az;
    let edge = abs(off) / (0.5 * SECTOR);
    let n_bezel = facet_normal(main_az - turn, 0.6021, false);
    let n_star = facet_normal(main_az + sign(off) * 0.5 * SECTOR - turn, 0.3927, false);
    let n_upper = facet_normal(main_az + sign(off) * 0.25 * SECTOR - turn, 0.7243, false);
    let in_star = edge > 0.35 + 0.65 * clamp((r - TABLE_R) / 0.3, 0.0, 1.0);
    let in_upper = edge > 1.0 - 0.7 * clamp((r - 0.75) / 0.25, 0.0, 1.0);
    var n = select(n_bezel, n_star, in_star && r < 0.8);
    n = select(n, n_upper, in_upper && r >= 0.75);
    return select(n, vec3<f32>(0.0, 0.0, 1.0), r < TABLE_R * cos(0.5 * edge * 0.5 * SECTOR));
}

// One channel of the internal path for index `eta`: refract in at the crown facet, reflect off
// the pavilion facet the ray reaches, once more if it still runs down, then leave through the
// crown facet above (or leak out of the pavilion when the cut lets it). Returns the exit
// direction in object space and, in w, a weight (1 for a clean exit, less for leaks and total
// internal reflection).
fn gem_exit(v_obj: vec3<f32>, n_obj: vec3<f32>, p_obj: vec3<f32>, eta: f32, turn: f32) -> vec4<f32> {
    // x across the width, y along the length (the part kit's gem-local frame), z by the width.
    let sc = vec3<f32>(1.0 / max(obj.p2.z, 1e-3), 1.0 / max(obj.p2.y, 1e-3), 1.0 / max(min(obj.p2.y, obj.p2.z), 1e-3));
    let t0 = refract(-v_obj, n_obj, 1.0 / eta);
    let p0 = p_obj * sc;
    let d0 = normalize(t0 * sc);
    // Bounce 1.
    let h1 = cone_hit(p0, d0);
    let d1 = reflect(d0, pavilion_normal(h1, turn));
    // Bounce 2 only when the ray still runs down.
    let h2c = cone_hit(h1, d1);
    let second = d1.z < 0.0;
    let h2 = select(h1, h2c, second);
    let d2 = select(d1, reflect(d1, pavilion_normal(h2c, turn)), second);
    // Leaving upward through the crown.
    let t_up = (TABLE_Z - h2.z) / max(d2.z, 1e-3);
    let e_pt = h2 + d2 * t_up;
    let n_c = crown_normal(e_pt, turn);
    let e_up = refract(d2, -n_c, eta);
    let tir = dot(e_up, e_up) < 1e-6;
    // Total internal reflection at the crown: the light wanders and leaves dimmer, steeper.
    let e_tir = normalize(vec3<f32>(-d2.x, -d2.y, abs(d2.z) + 0.6));
    // Running down after two bounces: it leaks out of the pavilion (the dark "window").
    let h3 = cone_hit(h2, d2);
    let e_leak = refract(d2, -pavilion_normal(h3, turn), eta);
    let leak = d2.z <= 0.0;
    var e = select(e_up, e_tir, tir);
    e = select(e, select(d2, e_leak, dot(e_leak, e_leak) > 1e-6), leak);
    let w = select(select(1.0, 0.55, tir), 0.35, leak);
    return vec4<f32>(normalize(e / sc), w);
}

struct GemShade {
    color: vec3<f32>,
    fresnel: f32,
};

fn gem_shade(i: VsOut, interior_lod: f32, reflect_lod: f32, glints: f32) -> GemShade {
    let n = normalize(i.world_n);
    let v = normalize(frame.eye.xyz - i.world_pos);
    let n_obj = normalize(i.obj_n);
    let v_obj = object_dir(v);
    let facet = u32(i.facet + 0.5);
    let n_d = obj.p1.x;
    let dn = (n_d - 1.0) / max(obj.p1.y, 1.0) * obj.p1.z;
    let f0 = pow((n_d - 1.0) / (n_d + 1.0), 2.0);
    let n_v = max(dot(n, v), 1e-4);
    let f = f0 + (1.0 - f0) * pow(1.0 - n_v, 5.0);
    // A small per-stone turn of the virtual facets (p3.x) keeps neighboring stones different.
    let turn = obj.p3.x;
    // 1 sample: surface reflection.
    let refl = env_at(reflect(-v, n), reflect_lod);
    // 3 samples: the internal path per channel (dispersion spreads the index).
    var e_r = vec4<f32>(0.0);
    var e_g = vec4<f32>(0.0);
    var e_b = vec4<f32>(0.0);
    var path_seed = facet * 7u + 5u;
    // Step cuts (p3.y = 1) have their own pavilion: more spread between facets and whole-facet flashes.
    let step_cut = obj.p3.y > 0.5;
    if (step_cut) {
        let sc = vec3<f32>(1.0 / max(obj.p2.z, 1e-3), 1.0 / max(obj.p2.y, 1e-3), 1.0 / max(min(obj.p2.y, obj.p2.z), 1e-3));
        let p0 = i.obj_pos * sc;
        let x_r = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / (n_d - 0.5 * dn)) * sc), n_d - 0.5 * dn);
        let x_g = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / n_d) * sc), n_d);
        let x_b = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / (n_d + 0.5 * dn)) * sc), n_d + 0.5 * dn);
        e_r = vec4<f32>(normalize(x_r.dir / sc), x_r.weight);
        e_g = vec4<f32>(normalize(x_g.dir / sc), x_g.weight);
        e_b = vec4<f32>(normalize(x_b.dir / sc), x_b.weight);
        path_seed = x_g.facet * 11u + 3u;
    } else {
        e_r = gem_exit(v_obj, n_obj, i.obj_pos, n_d - 0.5 * dn, turn);
        e_g = gem_exit(v_obj, n_obj, i.obj_pos, n_d, turn);
        e_b = gem_exit(v_obj, n_obj, i.obj_pos, n_d + 0.5 * dn, turn);
    }
    let w_r = world_dir(e_r.xyz);
    let w_g = world_dir(e_g.xyz);
    let w_b = world_dir(e_b.xyz);
    let interior = vec3<f32>(env_at(w_r, interior_lod).r * e_r.w, env_at(w_g, interior_lod).g * e_g.w, env_at(w_b, interior_lod).b * e_b.w);
    // Body color: Beer absorption over about twice the depth, a little longer per facet.
    let path = obj.p1.w * mix(select(0.8, 0.55, step_cut), select(1.25, 1.4, step_cut), hash01(path_seed));
    let tint = exp(-obj.p0.rgb * path);
    // A low-index stone leaks more of the studio through its pavilion, so its dark zones are not black.
    // A pale quartz passes that light on almost unabsorbed: its center reads as body color, not as olive.
    let absorb = max(obj.p0.r, max(obj.p0.g, obj.p0.b));
    let leak_lift = 0.1 * (1.0 - smoothstep(1.5, 1.9, n_d)) + 0.12 * (1.0 - smoothstep(1.55, 1.57, n_d)) * (1.0 - smoothstep(0.45, 0.8, absorb));
    // A step cut's table is one big mirror: cap the studio it shows, or a softbox whites out the pavilion.
    let surface = select(refl, min(refl, vec3<f32>(2.5)), step_cut);
    var c = surface * f + (interior + vec3<f32>(leak_lift)) * tint * (1.0 - f) * (1.0 - f) * 1.15;
    // Glints: the fixed key lights seen in the facet (surface) and along the exit rays (fire).
    for (var k = 0; k < 3; k = k + 1) {
        let l = normalize(frame.key_dir[k].xyz);
        let lc = frame.key_col[k].rgb;
        let h = normalize(v + l);
        let surf = pow(max(dot(n, h), 0.0), 900.0) * f * 40.0;
        // A step cut's facets are larger: a wider lobe lights a whole facet as it turns past a light.
        let lobe = select(220.0, 70.0, step_cut);
        let fire = vec3<f32>(pow(max(dot(w_r, l), 0.0), lobe), pow(max(dot(w_g, l), 0.0), lobe), pow(max(dot(w_b, l), 0.0), lobe));
        c = c + lc * (surf + fire * tint * 4.0) * glints;
    }
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return GemShade(c, f);
}

@fragment
fn fs_gem(i: VsOut) -> @location(0) vec4<f32> {
    return finish_color(gem_shade(i, 0.0, 0.0, obj.p2.w).color);
}

@fragment
fn fs_frosted(i: VsOut) -> @location(0) vec4<f32> {
    // Same path, every look-up at a rough mip, no glints: a soft milky glow with the cut's shape.
    let rough = clamp(obj.p0.w, 0.2, 1.0);
    let s = gem_shade(i, rough * frame.env.y, rough * frame.env.y * 0.8, 0.0);
    let n = normalize(i.world_n);
    let haze = env_at(n, frame.env.y) * exp(-obj.p0.rgb * obj.p1.w) * 0.35;
    return finish_color(s.color * 0.85 + haze);
}

// ---------------------------------------------------------------------------------------------
// Cabochon: smooth dome; reflection + one refraction tap + tint; no dispersion, no glints.

@fragment
fn fs_cabochon(i: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(i.world_n);
    let v = normalize(frame.eye.xyz - i.world_pos);
    let n_v = max(dot(n, v), 1e-4);
    let n_d = obj.p1.x;
    let f0 = pow((n_d - 1.0) / (n_d + 1.0), 2.0);
    let f = f0 + (1.0 - f0) * pow(1.0 - n_v, 5.0);
    let rough = obj.p0.w;
    let refl = env_at(reflect(-v, n), rough * frame.env.y);
    // Into the dome, off the flat base, out again: a blurred look-up through the stone.
    let n_obj = normalize(i.obj_n);
    let t = refract(-object_dir(v), n_obj, 1.0 / n_d);
    let back = reflect(t, vec3<f32>(0.0, 0.0, -1.0));
    // Down to the base and back up: about four dome heights at the center.
    let thick = obj.p2.x * (1.5 + 3.0 * max(n_obj.z, 0.0));
    let tint = exp(-obj.p0.rgb * thick);
    let through = env_at(world_dir(back), 2.5 + rough * frame.env.y);
    // A little scattered body glow from the environment's diffuse light.
    let glow = env_at(n, frame.env.y) * tint * 0.15;
    // The back of a set cabochon sits in metal: only part of the light comes back through.
    var c = refl * f + (through * tint * 0.3 + glow) * (1.0 - f);
    for (var k = 0; k < 3; k = k + 1) {
        c = c + frame.key_col[k].rgb * f * ggx_spec(n, v, normalize(frame.key_dir[k].xyz), max(rough, 0.05));
    }
    return finish_color(c);
}

// ---------------------------------------------------------------------------------------------
// Opal: milky body + view-dependent play of color in cells, soft gloss.

@fragment
fn fs_opal(i: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(i.world_n);
    let v = normalize(frame.eye.xyz - i.world_pos);
    let n_v = max(dot(n, v), 1e-4);
    let f = 0.035 + 0.965 * pow(1.0 - n_v, 5.0);
    let v_obj = object_dir(v);
    let cell_mm = max(obj.p0.w, 0.05);
    let footprint = length(fwidth(i.obj_pos.xy)) / cell_mm;
    let play = looks_opal_play(i.obj_pos, normalize(i.obj_n), v_obj, cell_mm, obj.p1.x, footprint);
    let cloud = looks_opal_cloud(i.obj_pos.xy);
    let body = obj.p0.rgb * (0.8 + 0.3 * cloud) * env_at(n, frame.env.y) * 0.9;
    let refl = env_at(reflect(-v, n), 1.0);
    // A lit domain takes the place of the body under it, so color survives on a white stone.
    var c = (body * (1.0 - 0.65 * play.a) + play.rgb) * (1.0 - f) + refl * f;
    for (var k = 0; k < 3; k = k + 1) {
        c = c + frame.key_col[k].rgb * f * ggx_spec(n, v, normalize(frame.key_dir[k].xyz), 0.08);
    }
    return finish_color(c);
}

// ---------------------------------------------------------------------------------------------
// Background: a soft studio sweep (a full-screen triangle drawn first, no depth write).

struct BgOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_background(@builtin(vertex_index) vi: u32) -> BgOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return BgOut(vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 1.0, 1.0), vec2<f32>(x, y));
}

@fragment
fn fs_background(i: BgOut) -> @location(0) vec4<f32> {
    let t = clamp(i.uv.y, 0.0, 1.0);
    let top = vec3<f32>(0.20, 0.205, 0.215);
    let bottom = vec3<f32>(0.075, 0.077, 0.082);
    let d = length(i.uv - vec2<f32>(0.5, 0.45));
    let c = mix(top, bottom, smoothstep(0.0, 1.0, t)) * (1.0 - 0.35 * d);
    return finish_color(c / max(frame.eye.w, 1e-3));
}
