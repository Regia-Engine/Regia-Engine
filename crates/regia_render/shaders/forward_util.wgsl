// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::util

// Small pure helpers of the forward shader: direction encodings, probe channel picks, render-override unpacking and hashes.
// Split out of `lab_forward.wgsl` unchanged (ADR-1552); imported there by name.

#import regia::utils

// Octahedral coordinates in [0, 1]² of a unit direction (+Y at the centre; Cigolle et al. 2014),
// the mapping `emissive_lights::oct_encode` traced the probes with.
fn oct_encode(d: vec3<f32>) -> vec2<f32> {
    let n = d / max(abs(d.x) + abs(d.y) + abs(d.z), 1e-12);
    var xz = n.xz;
    if (n.y < 0.0) {
        let s = select(vec2<f32>(-1.0), vec2<f32>(1.0), n.xz >= vec2<f32>(0.0));
        xz = (vec2<f32>(1.0) - abs(n.zx)) * s;
    }
    return xz * 0.5 + vec2<f32>(0.5);
}

fn equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / regia::utils::PI) + 0.5, theta / regia::utils::PI);
}

fn cube_face(dir: vec3<f32>) -> u32 {
    let a = abs(dir);
    if (a.x >= a.y && a.x >= a.z) {
        return select(1u, 0u, dir.x >= 0.0);
    }
    if (a.y >= a.z) {
        return select(3u, 2u, dir.y >= 0.0);
    }
    return select(5u, 4u, dir.z >= 0.0);
}

fn gi_component(v: vec4<f32>, i: u32) -> f32 {
    switch i {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

// Rotates a UV-space vector by −θ (into a decal's frame).
fn decal_unrotate(v: vec2<f32>, c: f32, s: f32) -> vec2<f32> {
    return vec2<f32>(c * v.x + s * v.y, -s * v.x + c * v.y);
}

fn hair_hash(cell: f32) -> f32 {
    return fract(sin(cell * 12.9898 + 78.233) * 43758.5453);
}

// Render override helpers (`crate::render_override`). An instance without one reads zeros.
fn override_on(o: vec2<u32>) -> bool {
    return (o.y & 1u) != 0u;
}

// Base-colour multiplier (rgb) and coverage (a); white without an override.
fn override_tint(o: vec2<u32>) -> vec4<f32> {
    return select(vec4<f32>(1.0), unpack4x8unorm(o.x), override_on(o));
}

// Added display-referred colour (bytes 1-3 of `y` when bit 4 is set).
fn override_emissive(o: vec2<u32>) -> vec3<f32> {
    if (o.y & 17u) != 17u {
        return vec3<f32>(0.0);
    }
    return unpack4x8unorm(o.y).yzw;
}

fn glass_hash(p: vec2<f32>) -> f32 {
    let q = fract(p * vec2<f32>(0.1031, 0.1030));
    let r = q + dot(q, q.yx + vec2<f32>(33.33));
    return fract((r.x + r.y) * r.x);
}

// Stochastic SSR surface code in the HDR alpha (decoded by sssr_common.wgsl `decode_alpha`): 5 bits
// perceptual roughness, 5 bits √(specular weight), 4 bits environment-specular share, stored as a
// negative normal half float (sign set, exponent 1..=16), so it survives the Rgba16Float target
// exactly and never reads as the 0..1 roughness of the older SSR.
fn ssr_pack(perceptual: f32, weight: f32, env_share: f32) -> f32 {
    let r = u32(round(regia::utils::saturate(perceptual) * 31.0));
    let w = u32(round(sqrt(regia::utils::saturate(weight)) * 31.0));
    let f = u32(round(regia::utils::saturate(env_share) * 15.0));
    let code = r | (w << 5u) | (f << 10u);
    // −(1 + m / 1024) · 2^(e − 15) with e = 1 + code / 1024, m = code % 1024: exact in f32 and in
    // the half-float target.
    let e = 1u + (code >> 10u);
    return -ldexp(f32(1024u + (code & 1023u)), i32(e) - 25);
}
