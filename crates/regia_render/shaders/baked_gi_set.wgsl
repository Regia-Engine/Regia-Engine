// SPDX-License-Identifier: MPL-2.0
// Several baked GI volumes at once (ADR-1097): `static_gi_set(p, n, v)`, the blend of up to
// BGI_MAX_VOLUMES volumes. Mirrored on the CPU by `baked_gi/blend.rs`
// (`sample_static_gi_blend`); keep the two in step.
//
// This file has no bindings. A host shader declares, under these names:
//   bgi_set: BakedGiSet (uniform): one `BakedGiUniform` per volume (their sections live in the
//     one `bgi_data` buffer, one after another; each uniform's word offsets point at its own)
//     and one `BakedGiLayerGpu` per volume, sorted by priority (highest first).
//   bgi_vol: u32 (var<private>): the volume `static_gi` reads (its `bgi_u()` returns
//     `bgi_set.volumes[bgi_vol]`); set here per volume. An index, not a Private copy of the
//     struct: naga lays every struct out explicitly, which VUID-StandaloneSpirv-None-10684
//     forbids in Private storage (ADR-026, ADR-7587).
//   bgi_slot: u32 (var<private>): the volume slot `bgi_word` reads; set here per volume
//     (ADR-2618: a split volume's parts may sit in separate storage bindings).
//
// Each volume's own lookup (`static_gi`: its light at full strength, its coverage) is weighted
// by layer weight × coverage × edge (1 without a blend distance; else the X, Y and Z ramps from
// 0 at the footprint's edge to 1 at the blend distance inside it, multiplied). Y ramps only for
// a closed brick box (`vertical.w`); a terrain layer is open in Y, so its factor stays 1.
// Volumes of one priority blend by weight, normalized, at coverage min(Σw, 1); priorities
// composite in order, each taking (1 − covered so far) × its coverage (Porter & Duff's "over").
// Every factor is continuous, so the light is continuous across the seams between volumes.

// `MAX_BAKED_GI_VOLUMES`.
const BGI_MAX_VOLUMES: u32 = 8u;

struct BakedGiLayerGpu {
    // xy = footprint min (x, z), zw = footprint max (x, z), meters.
    rect: vec4<f32>,
    // x = blend distance (m; 0: no edge ramp), y = weight (0–1), zw = reach (x, z): how far
    // outside the footprint the volume can still cover (`BakedGiFootprint::reach`).
    fade: vec4<f32>,
    // x = priority tier (0 = highest; the layers of one tier are adjacent), y = volume slot
    // (`bgi_word`), z = 1 when the edge weight is per edge (`edges`, `bands`), w unused.
    tier: vec4<u32>,
    // Per-edge fade (a split part beside its installed neighbors, ADR-2618): the edge positions
    // (min x, min z, max x, max z) and the ramp width at each (0: that edge does not fade).
    edges: vec4<f32>,
    bands: vec4<f32>,
    // x = min Y, y = max Y (meters), z = Y reach (recorded; the reach skip stays X and Z),
    // w = 1 when the volume is a closed brick box and the Y ramp applies. 0 for a terrain layer.
    vertical: vec4<f32>,
}

struct BakedGiSet {
    // x = volumes in use, y = 1 when the one volume lights alone (`static_gi` unchanged), z = 1
    // while the brick-loop probe counts (ADR-2618).
    count: vec4<u32>,
    volumes: array<BakedGiUniform, 8>,
    layers: array<BakedGiLayerGpu, 8>,
}

// One edge's ramp: clamp(d / band, 0, 1), 1 when the edge does not fade (band 0).
fn bgi_edge_ramp(d: f32, band: f32) -> f32 {
    if (band <= 0.0) {
        return 1.0;
    }
    return clamp(d / band, 0.0, 1.0);
}

// Y factor of the edge weight. 1 when the blend distance is 0 (a hard cut: the volume's own
// coverage supplies it) or the volume is open in Y (`vertical.w` is 0, a terrain layer).
fn bgi_vertical_ramp(k: u32, y: f32) -> f32 {
    let layer = bgi_set.layers[k];
    if (layer.vertical.w <= 0.0 || layer.fade.x <= 0.0) {
        return 1.0;
    }
    let d = min(y - layer.vertical.x, layer.vertical.y - y);
    return clamp(d / layer.fade.x, 0.0, 1.0);
}

fn bgi_layer_edge(k: u32, p: vec3<f32>) -> f32 {
    let layer = bgi_set.layers[k];
    var xz = 1.0;
    if (layer.tier.z == 1u) {
        let lo = p.xz - layer.edges.xy;
        let hi = layer.edges.zw - p.xz;
        let ex = min(bgi_edge_ramp(lo.x, layer.bands.x), bgi_edge_ramp(hi.x, layer.bands.z));
        let ez = min(bgi_edge_ramp(lo.y, layer.bands.y), bgi_edge_ramp(hi.y, layer.bands.w));
        xz = ex * ez;
    } else if (layer.fade.x > 0.0) {
        let d = min(p.xz - layer.rect.xy, layer.rect.zw - p.xz);
        let e = clamp(d / layer.fade.x, vec2<f32>(0.0), vec2<f32>(1.0));
        xz = e.x * e.y;
    }
    return xz * bgi_vertical_ramp(k, p.y);
}

// Whether volume `k` can cover `p` at all (the footprint grown by its reach); outside, its
// coverage is 0 and the lookup is skipped.
fn bgi_layer_reaches(k: u32, p: vec3<f32>) -> bool {
    let layer = bgi_set.layers[k];
    let lo = layer.rect.xy - layer.fade.zw;
    let hi = layer.rect.zw + layer.fade.zw;
    return all(p.xz >= lo) && all(p.xz <= hi);
}

fn bgi_layer_tier(k: u32) -> u32 {
    return bgi_set.layers[k].tier.x;
}

// The blended baked irradiance at `p` (normal `n`, `v` toward the viewer): summed SH at full
// strength, coverage, visibility and the probe-relative AO split (each volume's weighted like its
// light), as `static_gi` for one volume.
fn static_gi_set(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> StaticGi {
    let count = min(bgi_set.count.x, BGI_MAX_VOLUMES);
    if (bgi_set.count.y == 1u) {
        bgi_vol = 0u;
        bgi_slot = bgi_set.layers[0].tier.y;
        return static_gi(p, n, v);
    }
    // Running SH sums, one `var` per member (no struct `var`, ADR-7587).
    var blended_l0 = vec3<f32>(0.0);
    var blended_r = vec3<f32>(0.0);
    var blended_g = vec3<f32>(0.0);
    var blended_b = vec3<f32>(0.0);
    var acc = 0.0;
    var vis = 0.0;
    var split = vec4<f32>(0.0);
    var k = 0u;
    loop {
        if (k >= count || acc >= 1.0) {
            break;
        }
        let tier = bgi_layer_tier(k);
        var tier_l0 = vec3<f32>(0.0);
        var tier_r = vec3<f32>(0.0);
        var tier_g = vec3<f32>(0.0);
        var tier_b = vec3<f32>(0.0);
        var tier_w = 0.0;
        var tier_vis = 0.0;
        var tier_split = vec4<f32>(0.0);
        loop {
            if (k >= count || bgi_layer_tier(k) != tier) {
                break;
            }
            var w0 = 0.0;
            if (bgi_layer_reaches(k, p)) {
                w0 = bgi_set.layers[k].fade.y * bgi_layer_edge(k, p);
            }
            if (w0 > 0.0) {
                bgi_vol = k;
                bgi_slot = bgi_set.layers[k].tier.y;
                let g = static_gi(p, n, v);
                let w = w0 * g.cov;
                if (w > 0.0) {
                    let sh = bgi_sh_add(BgiSh(tier_l0, tier_r, tier_g, tier_b), g.sh, w);
                    tier_l0 = sh.l0;
                    tier_r = sh.r;
                    tier_g = sh.g;
                    tier_b = sh.b;
                    tier_w = tier_w + w;
                    tier_vis = tier_vis + w * g.vis;
                    tier_split = bgi_split_add(tier_split, g.split, w);
                }
            }
            k = k + 1u;
        }
        if (tier_w > 0.0) {
            let take = (1.0 - acc) * min(tier_w, 1.0);
            let sh = bgi_sh_add(BgiSh(blended_l0, blended_r, blended_g, blended_b), BgiSh(tier_l0, tier_r, tier_g, tier_b), take / tier_w);
            blended_l0 = sh.l0;
            blended_r = sh.r;
            blended_g = sh.g;
            blended_b = sh.b;
            vis = vis + take * tier_vis / tier_w;
            split = bgi_split_add(split, bgi_split_finish(tier_split, 1.0 / tier_w), take);
            acc = acc + take;
        }
    }
    if (acc <= 1e-5) {
        return StaticGi(bgi_sh_zero(), 0.0, 0.0, vec4<f32>(0.0));
    }
    return StaticGi(bgi_sh_mul(BgiSh(blended_l0, blended_r, blended_g, blended_b), 1.0 / acc), acc, vis / acc, bgi_split_finish(split, 1.0 / acc));
}
