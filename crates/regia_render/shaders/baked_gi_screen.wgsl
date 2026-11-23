// SPDX-License-Identifier: MPL-2.0
// Baked GI: the half-resolution screen lookup (compute, after the depth prepass; `ddgi.screen.w`
// pixels per texel, 2 unless measured, ADR-1033), in the
// layout the DDGI lookup writes (`ddgi_screen.wgsl`: six RGB9E5 irradiances / π toward the
// ambient-cube directions, distance to the eye, coverage). The forward and terrain passes read
// it through `ddgi_screen_irradiance` unchanged, so the baked volume reaches every probe-GI site
// (the forward pass's diffuse and rough specular, its probe/sky specular-occlusion ratio, and
// the terrain's `engine_gi`) without a per-pixel probe gather.
// Capsule occluders for dynamic units (`baked_gi/occluders.rs`, its CPU mirror) attenuate the
// baked irradiance toward each of the six directions here, so no other pass binds them.
// Several volumes (adjacent tiles, a streamed level's bake, a nested set piece) blend in
// `static_gi_set` (`baked_gi_set.wgsl`, ADR-1097).
// Probe-relative AO (ADR-1104, `baked_gi/ao_split.rs`): after the lookup's texels, three words per
// texel (row-major) tell the SSAO kernel, which runs after this pass, which share of the light
// the brick and the terrain layer hold and at what resolve distances; with the SSAO on, each
// capsule leaves to the kernel the share of its occlusion the kernel counts.
// Lightmap receivers (ADR-2656) do not read the ambient cube: while capsules are set, one more word
// per texel after the split words holds the occlusion at the depth normal (1 − `capsule_visibility`,
// zero = open), and the forward pass multiplies a receiver's baked diffuse term by it
// (`ddgi_screen_occluder_visibility`, behind `ctrl.w` bit 32768). No capsule: never written or read.
// The buffer always has room for the word (except a lookup at the limit of a storage binding).
// Composed as ddgi_common.wgsl + baked_gi.wgsl + baked_gi_set.wgsl + this file.

// `MAX_GI_OCCLUDERS`.
const BGI_MAX_OCCLUDERS: u32 = 64u;

struct BakedGiOccluders {
    // x = capsules in use (at most BGI_MAX_OCCLUDERS).
    count: vec4<u32>,
    // Capsule k: caps[2k] = (a.xyz, radius), caps[2k + 1] = (b.xyz, strength).
    caps: array<vec4<f32>, 128>,
}

@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read_write> ddgi_data: array<u32>;
@group(0) @binding(2) var<uniform> bgi_set: BakedGiSet;
// The volume `static_gi` reads (`static_gi_set` sets it per volume): an index into
// `bgi_set.volumes`, not a Private copy of the struct (VUID-StandaloneSpirv-None-10684, ADR-7587).
var<private> bgi_vol: u32;
@group(0) @binding(3) var<storage, read> bgi_data: array<u32>;
@group(0) @binding(4) var<uniform> bgi_occ: BakedGiOccluders;
// Volume slots 1-3 (ADR-2618): the parts of a split volume that do not fit beside the others in
// one storage binding. An unused slot binds slot 0's buffer.
@group(0) @binding(5) var<storage, read> bgi_data1: array<u32>;
@group(0) @binding(6) var<storage, read> bgi_data2: array<u32>;
@group(0) @binding(7) var<storage, read> bgi_data3: array<u32>;
// The brick-loop probe's histogram (ADR-2618): texels by bricks tested, written only while
// `bgi_set.count.z` is 1.
@group(0) @binding(8) var<storage, read_write> bgi_visit_hist: array<atomic<u32>>;

// The slot `static_gi` reads (`static_gi_set` sets it per volume).
var<private> bgi_slot: u32;

fn bgi_u() -> BakedGiUniform {
    return bgi_set.volumes[bgi_vol];
}

fn bgi_word(i: u32) -> u32 {
    switch (bgi_slot) {
        case 1u: { return bgi_data1[i]; }
        case 2u: { return bgi_data2[i]; }
        case 3u: { return bgi_data3[i]; }
        default: { return bgi_data[i]; }
    }
}

// `BRICK_VISIT_BINS`: the last bin holds every count at or past it.
const BGI_VISIT_BINS: u32 = 64u;

fn bgi_visit_note() {
    if (bgi_set.count.z == 1u) {
        atomicAdd(&bgi_visit_hist[min(bgi_visits, BGI_VISIT_BINS - 1u)], 1u);
    }
}

struct ScreenParams {
    inv_view_proj: mat4x4<f32>,
    // xyz = eye, w = the SSAO kernel's radius (m) when it runs after this pass with the
    // probe-relative split, else 0.
    eye: vec4<f32>,
    // xy = full-resolution size (the depth buffer).
    size: vec4<u32>,
    // xyz = the local origin in the volume's space (zero until a floating-origin rebase).
    origin: vec4<f32>,
}

@group(1) @binding(0) var<uniform> screen: ScreenParams;
@group(1) @binding(1) var depth: texture_depth_2d;

fn world_at(px: vec2<i32>) -> vec4<f32> {
    let full = vec2<i32>(screen.size.xy);
    let c = clamp(px, vec2<i32>(0), full - vec2<i32>(1));
    let d = textureLoad(depth, c, 0);
    let uv = (vec2<f32>(c) + vec2<f32>(0.5)) / vec2<f32>(full);
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, d, 1.0);
    let w = screen.inv_view_proj * clip;
    return vec4<f32>(w.xyz / w.w, d);
}

// Fraction of the cosine-weighted hemisphere around each direction that a sphere blocks, from
// the cosines `c` between the directions and the sphere's centre and `x` = r / d: the
// configuration factor x²·c while the sphere is wholly above the horizon (c ≥ x), the quadratic
// (c + x)²/(4x) across it (same value and slope at c = x, 0 at c = −x).
fn bgi_sphere_blocked(c: vec3<f32>, x: f32) -> vec3<f32> {
    let t = max(c + vec3<f32>(x), vec3<f32>(0.0));
    return select(0.25 * x * t * t, x * x * c, c >= vec3<f32>(x));
}

// Visibility of the six directions at `p` under the capsules (pos = +x, +y, +z; neg = −x, −y,
// −z): each capsule as the sphere at its segment's point nearest `p`, combined as a product.
struct BgiOcclusion {
    pos: vec3<f32>,
    neg: vec3<f32>,
    // Toward the depth normal, whole: a receiver's diffuse term takes no SSAO (ADR-2635), so nothing
    // is left to the kernel (the CPU's `capsule_visibility`).
    at_normal: f32,
}

// `PROBE_AO_RESOLVE` (`baked_gi/ao_split.rs`): ρ = this × a probe cell's edge.
const PROBE_AO_RESOLVE: f32 = 0.5;

// BEGIN probe-relative AO (`baked_gi/ao_split.rs` `probe_ao_keep`, ADR-1104). This block is
// identical in ssao.wgsl and baked_gi_screen.wgsl (test `probe_ao_wgsl_copies_match`).
// How much of an occluder point `height` metres above the receiver's tangent plane the probe
// field does not already hold, for resolve distance `rho` (m): 1 below rho / 2, 0 past 1.5 rho.
fn probe_ao_keep(height: f32, rho: f32) -> f32 {
    return clamp(1.5 - height / max(rho, 1e-4), 0.0, 1.0);
}
// END probe-relative AO

// The share of a capsule's occlusion the SSAO kernel (radius `ssao_r`, 0 = off) counts: its
// nearest sphere `d` away, radius `r`, at cosine `cos_n` from the normal; `split` = (brick
// share, terrain share, brick ρ, terrain ρ) (`ProbeAoSplit::capsule_ssao_share`).
fn bgi_capsule_ssao_share(d: f32, r: f32, cos_n: f32, ssao_r: f32, split: vec4<f32>) -> f32 {
    if (ssao_r <= 0.0) {
        return 0.0;
    }
    let t = max(0.5 * ((d - r) + sqrt(max(d * d - r * r, 0.0))), 0.0);
    let falloff = clamp(1.0 - t * t / (ssao_r * ssao_r), 0.0, 1.0);
    let covered = split.x + split.y;
    if (covered <= 1e-6) {
        return falloff;
    }
    let height = max(cos_n, 0.0) * t;
    return falloff * (split.x * probe_ao_keep(height, split.z) + split.y * probe_ao_keep(height, split.w)) / covered;
}

// `n` (the depth normal) and `split` steer the SSAO share only (`ssao_r` = 0: none).
fn bgi_capsule_occlusion(p: vec3<f32>, n: vec3<f32>, ssao_r: f32, split: vec4<f32>) -> BgiOcclusion {
    // One `var` per member (no struct `var`, ADR-7587).
    var o_pos = vec3<f32>(1.0);
    var o_neg = vec3<f32>(1.0);
    var o_at_normal = 1.0;
    let count = min(bgi_occ.count.x, BGI_MAX_OCCLUDERS);
    for (var k = 0u; k < count; k = k + 1u) {
        let a = bgi_occ.caps[2u * k];
        let b = bgi_occ.caps[2u * k + 1u];
        let ab = b.xyz - a.xyz;
        let t = clamp(dot(p - a.xyz, ab) / max(dot(ab, ab), 1e-12), 0.0, 1.0);
        let to_c = a.xyz + ab * t - p;
        let d = length(to_c);
        let inv = 1.0 / max(d, 1e-6);
        let x = min(a.w * inv, 1.0);
        let dir = to_c * inv;
        let strength = b.w * (1.0 - bgi_capsule_ssao_share(d, a.w, dot(n, dir), ssao_r, split));
        o_pos = o_pos * (vec3<f32>(1.0) - strength * bgi_sphere_blocked(dir, x));
        o_neg = o_neg * (vec3<f32>(1.0) - strength * bgi_sphere_blocked(-dir, x));
        o_at_normal = o_at_normal * (1.0 - b.w * bgi_sphere_blocked(vec3<f32>(dot(n, dir)), x).x);
    }
    return BgiOcclusion(o_pos, o_neg, o_at_normal);
}

// The first of texel `gid`'s three probe-relative AO split words (after the lookup's texels):
// pack2x16unorm(brick share, terrain share), then the brick's and the terrain layer's resolve
// distances as f32 bits.
fn bgi_split_word(gid: vec2<u32>) -> u32 {
    return ddgi.screen.x + ddgi.screen.y * ddgi.screen.z * DDGI_SCREEN_WORDS + 3u * (gid.y * ddgi.screen.y + gid.x);
}

// Texel `gid`'s occluder word, after the split words (`ddgi_screen_occluder_visibility` reads it): the
// occlusion at the depth normal as f32 bits. Only there while capsules are set.
fn bgi_occluder_word(gid: vec2<u32>) -> u32 {
    return ddgi.screen.x + ddgi.screen.y * ddgi.screen.z * (DDGI_SCREEN_WORDS + 3u) + gid.y * ddgi.screen.y + gid.x;
}

@compute @workgroup_size(8, 8)
fn baked_screen_lookup(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= ddgi.screen.y || gid.y >= ddgi.screen.z) {
        return;
    }
    let word = ddgi.screen.x + (gid.y * ddgi.screen.y + gid.x) * DDGI_SCREEN_WORDS;
    // The pixel under the block's top-left corner (`ddgi.screen.w` pixels per texel).
    let px = vec2<i32>(gid.xy * ddgi.screen.w);
    let c = world_at(px);
    if (c.w >= 1.0) {
        for (var j = 0u; j < 6u; j = j + 1u) {
            ddgi_data[word + j] = 0u;
        }
        ddgi_data[word + 6u] = bitcast<u32>(1.0e30);
        ddgi_data[word + 7u] = bitcast<u32>(0.0);
        let sky = bgi_split_word(gid.xy);
        ddgi_data[sky] = 0u;
        ddgi_data[sky + 1u] = 0u;
        ddgi_data[sky + 2u] = 0u;
        if ((ddgi.ctrl.w & DDGI_CTRL_OCCLUDER_VIS) != 0u) {
            ddgi_data[bgi_occluder_word(gid.xy)] = 0u;
        }
        return;
    }
    bgi_visits = 0u;
    let p = c.xyz;
    // The depth normal (the smaller difference on each axis, as the DDGI lookup) only steers
    // the probe weights; the light is stored toward fixed directions for each fragment's own.
    let r = world_at(px + vec2<i32>(1, 0)).xyz - p;
    let l = p - world_at(px - vec2<i32>(1, 0)).xyz;
    let u = world_at(px + vec2<i32>(0, 1)).xyz - p;
    let b = p - world_at(px - vec2<i32>(0, 1)).xyz;
    let v = normalize(screen.eye.xyz - p);
    let n = ddgi_depth_normal(r, l, u, b, v);
    let gi = static_gi_set(p + screen.origin.xyz, n, v);
    bgi_visit_note();
    // The split: the brick's and the terrain layer's shares of the coverage and their resolve
    // distances.
    let split = vec4<f32>(gi.cov * gi.split.xy, PROBE_AO_RESOLVE * gi.split.zw);
    let sw = bgi_split_word(gid.xy);
    ddgi_data[sw] = pack2x16unorm(split.xy);
    ddgi_data[sw + 1u] = bitcast<u32>(split.z);
    ddgi_data[sw + 2u] = bitcast<u32>(split.w);
    // No occluders (the usual case): every factor stays 1 and the loop does not run.
    let occ = bgi_capsule_occlusion(p, n, screen.eye.w, split);
    if ((ddgi.ctrl.w & DDGI_CTRL_OCCLUDER_VIS) != 0u) {
        ddgi_data[bgi_occluder_word(gid.xy)] = bitcast<u32>(1.0 - occ.at_normal);
    }
    for (var j = 0u; j < 6u; j = j + 1u) {
        var d = vec3<f32>(0.0);
        d[j >> 1u] = select(1.0, -1.0, (j & 1u) == 1u);
        var side = select(occ.pos, occ.neg, (j & 1u) == 1u);
        let k = side[j >> 1u];
        ddgi_data[word + j] = ddgi_pack_rgb9e5(bgi_irradiance_over_pi(gi.sh, d) * k);
    }
    ddgi_data[word + 6u] = bitcast<u32>(length(screen.eye.xyz - p));
    ddgi_data[word + 7u] = bitcast<u32>(gi.cov);
}
