// SPDX-License-Identifier: MPL-2.0
// regia_looks faceted gem hook (ADR-2538): one reflection and, per RGB channel, a refraction into
// a virtual Tolkowsky pavilion (two fixed bounces), an exit through the crown facet above, Beer tint
// and key-light glints. 4 environment samples, no data-dependent loop, no noise.
// base: attenuation per mm rgb, roughness. p0: n_d, Abbe, fire, path mm. p1: depth, half length,
// half width (mm), glints. p2: x = turn of the virtual facets.

// The stone in "normalized" coordinates: x and y by their half extents, z by the shorter of the two, so
// every outline is the unit circle at the girdle whichever way the stone lies. The pavilion is the Tolkowsky cone (40.75
// degrees, apex at the culet) cut into facets by azimuth; the crown is the table plane plus kites.
const LOOKS_GIRDLE: f32 = 0.024;
const LOOKS_PAV_TAN: f32 = 0.8613;
const LOOKS_TABLE_Z: f32 = 0.347;
const LOOKS_TABLE_R: f32 = 0.53;
const LOOKS_SECTOR: f32 = 0.785398;

// Where a ray from inside the stone meets the pavilion cone (fixed math, no loop).
fn looks_cone_hit(p: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    let za = -LOOKS_GIRDLE - LOOKS_PAV_TAN;
    let k2 = 1.0 / (LOOKS_PAV_TAN * LOOKS_PAV_TAN);
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

fn looks_facet_normal(az: f32, angle: f32, down: bool) -> vec3<f32> {
    let s = sin(angle);
    let c = cos(angle);
    return vec3<f32>(cos(az) * s, sin(az) * s, select(c, -c, down));
}

// The pavilion facet under a point: 8 mains (kites that narrow toward the girdle) and 16 lower
// halves beside them.
fn looks_pavilion_normal(h: vec3<f32>, turn: f32) -> vec3<f32> {
    let az = atan2(h.y, h.x) + turn;
    let r = length(h.xy);
    let sector = floor(az / LOOKS_SECTOR + 0.5);
    let main_az = sector * LOOKS_SECTOR;
    let off = az - main_az;
    let main_half = 0.5 * LOOKS_SECTOR * clamp((1.0 - r) / 0.75, 0.0, 1.0);
    let is_half = abs(off) > main_half;
    let half_az = main_az + sign(off) * 0.25 * LOOKS_SECTOR;
    let n_main = looks_facet_normal(main_az - turn, 0.7112, true);
    let n_half = looks_facet_normal(half_az - turn, 0.7392, true);
    return select(n_main, n_half, is_half);
}

// The crown facet a ray leaves through: the table, 8 stars, 8 bezels and 16 upper halves.
fn looks_crown_normal(e: vec3<f32>, turn: f32) -> vec3<f32> {
    let az = atan2(e.y, e.x) + turn;
    let r = length(e.xy);
    let sector = floor(az / LOOKS_SECTOR + 0.5);
    let main_az = sector * LOOKS_SECTOR;
    let off = az - main_az;
    let edge = abs(off) / (0.5 * LOOKS_SECTOR);
    let n_bezel = looks_facet_normal(main_az - turn, 0.6021, false);
    let n_star = looks_facet_normal(main_az + sign(off) * 0.5 * LOOKS_SECTOR - turn, 0.3927, false);
    let n_upper = looks_facet_normal(main_az + sign(off) * 0.25 * LOOKS_SECTOR - turn, 0.7243, false);
    let in_star = edge > 0.35 + 0.65 * clamp((r - LOOKS_TABLE_R) / 0.3, 0.0, 1.0);
    let in_upper = edge > 1.0 - 0.7 * clamp((r - 0.75) / 0.25, 0.0, 1.0);
    var n = select(n_bezel, n_star, in_star && r < 0.8);
    n = select(n, n_upper, in_upper && r >= 0.75);
    return select(n, vec3<f32>(0.0, 0.0, 1.0), r < LOOKS_TABLE_R * cos(0.5 * edge * 0.5 * LOOKS_SECTOR));
}

// One channel of the internal path for index `eta`: refract in at the crown facet, reflect off
// the pavilion facet the ray reaches, once more if it still runs down, then leave through the
// crown facet above (or leak out of the pavilion when the cut lets it). Returns the exit
// direction in object space and, in w, a weight (1 for a clean exit, less for leaks and total
// internal reflection).
fn looks_gem_exit(v_obj: vec3<f32>, n_obj: vec3<f32>, p_obj: vec3<f32>, eta: f32, turn: f32) -> vec4<f32> {
    // x across the width, y along the length (the part kit's gem-local frame), z by the width.
    let sc = vec3<f32>(1.0 / max(material.p1.z, 1e-3), 1.0 / max(material.p1.y, 1e-3), 1.0 / max(min(material.p1.y, material.p1.z), 1e-3));
    let t0 = refract(-v_obj, n_obj, 1.0 / eta);
    let p0 = p_obj * sc;
    let d0 = normalize(t0 * sc);
    // Bounce 1.
    let h1 = looks_cone_hit(p0, d0);
    let d1 = reflect(d0, looks_pavilion_normal(h1, turn));
    // Bounce 2 only when the ray still runs down.
    let h2c = looks_cone_hit(h1, d1);
    let second = d1.z < 0.0;
    let h2 = select(h1, h2c, second);
    let d2 = select(d1, reflect(d1, looks_pavilion_normal(h2c, turn)), second);
    // Leaving upward through the crown.
    let t_up = (LOOKS_TABLE_Z - h2.z) / max(d2.z, 1e-3);
    let e_pt = h2 + d2 * t_up;
    let n_c = looks_crown_normal(e_pt, turn);
    let e_up = refract(d2, -n_c, eta);
    let tir = dot(e_up, e_up) < 1e-6;
    // Total internal reflection at the crown: the light wanders and leaves dimmer, steeper.
    let e_tir = normalize(vec3<f32>(-d2.x, -d2.y, abs(d2.z) + 0.6));
    // Running down after two bounces: it leaks out of the pavilion (the dark "window").
    let h3 = looks_cone_hit(h2, d2);
    let e_leak = refract(d2, -looks_pavilion_normal(h3, turn), eta);
    let leak = d2.z <= 0.0;
    var e = select(e_up, e_tir, tir);
    e = select(e, select(d2, e_leak, dot(e_leak, e_leak) > 1e-6), leak);
    let w = select(select(1.0, 0.55, tir), 0.35, leak);
    return vec4<f32>(normalize(e / sc), w);
}

struct LooksGemShade {
    color: vec3<f32>,
    fresnel: f32,
};

fn looks_gem_shade(s: Surface, interior_mip: f32, reflect_mip: f32, glints: f32) -> LooksGemShade {
    let n = s.normal;
    let v = s.view;
    let n_obj = s.local_normal;
    let v_obj = normalize(transpose(s.basis) * v);
    let facet = u32(s.facet + 0.5);
    let n_d = material.p0.x;
    let dn = (n_d - 1.0) / max(material.p0.y, 1.0) * material.p0.z;
    let f0 = pow((n_d - 1.0) / (n_d + 1.0), 2.0);
    let n_v = max(dot(n, v), 1e-4);
    let f = f0 + (1.0 - f0) * pow(1.0 - n_v, 5.0);
    // A small per-stone turn of the virtual facets (p3.x) keeps neighboring stones different.
    let turn = material.p2.x;
    // 1 sample: surface reflection.
    let refl = looks_env(reflect(-v, n), reflect_mip);
    // 3 samples: the internal path per channel (dispersion spreads the index).
    var e_r = vec4<f32>(0.0);
    var e_g = vec4<f32>(0.0);
    var e_b = vec4<f32>(0.0);
    var path_seed = facet * 7u + 5u;
    // Step cuts (p2.y = 1) have their own pavilion: more spread between facets and whole-facet flashes.
    let step_cut = material.p2.y > 0.5;
    if (step_cut) {
        let sc = vec3<f32>(1.0 / max(material.p1.z, 1e-3), 1.0 / max(material.p1.y, 1e-3), 1.0 / max(min(material.p1.y, material.p1.z), 1e-3));
        let p0 = s.local_pos * sc;
        let x_r = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / (n_d - 0.5 * dn)) * sc), n_d - 0.5 * dn);
        let x_g = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / n_d) * sc), n_d);
        let x_b = looks_step_exit(p0, normalize(refract(-v_obj, n_obj, 1.0 / (n_d + 0.5 * dn)) * sc), n_d + 0.5 * dn);
        e_r = vec4<f32>(normalize(x_r.dir / sc), x_r.weight);
        e_g = vec4<f32>(normalize(x_g.dir / sc), x_g.weight);
        e_b = vec4<f32>(normalize(x_b.dir / sc), x_b.weight);
        path_seed = x_g.facet * 11u + 3u;
    } else {
        e_r = looks_gem_exit(v_obj, n_obj, s.local_pos, n_d - 0.5 * dn, turn);
        e_g = looks_gem_exit(v_obj, n_obj, s.local_pos, n_d, turn);
        e_b = looks_gem_exit(v_obj, n_obj, s.local_pos, n_d + 0.5 * dn, turn);
    }
    let w_r = normalize(s.basis * e_r.xyz);
    let w_g = normalize(s.basis * e_g.xyz);
    let w_b = normalize(s.basis * e_b.xyz);
    let interior = vec3<f32>(looks_env(w_r, interior_mip).r * e_r.w, looks_env(w_g, interior_mip).g * e_g.w, looks_env(w_b, interior_mip).b * e_b.w);
    // Body color: Beer absorption over about twice the depth, a little longer per facet.
    let path = material.p0.w * mix(select(0.8, 0.55, step_cut), select(1.25, 1.4, step_cut), looks_hash01(path_seed));
    let tint = exp(-material.base.rgb * path);
    // A low-index stone leaks more of the studio through its pavilion, so its dark zones are not black.
    // A pale quartz passes that light on almost unabsorbed: its center reads as body color, not as olive.
    let absorb = max(material.base.r, max(material.base.g, material.base.b));
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
    return LooksGemShade(c, f);
}

fn shade_surface(s: Surface) -> vec3<f32> {
    return looks_gem_shade(s, 0.0, sqrt(clamp(material.base.w, 0.0, 1.0)), material.p1.w).color;
}
