// SPDX-License-Identifier: MPL-2.0
// Grass placement (ADR-1400), used by the generator only (`grass_cull.wgsl`). The CPU mirror is `regia_terrain::grass::select` (`candidate`, `walk_slot`, `density_fraction`).
//
// Positions: the R2 low-discrepancy sequence (Roberts 2018, "The unreasonable effectiveness of
// quasirandom sequences"), rotated per patch and jittered by the spacing. Prepended:
// `grass_common.wgsl`.

struct GrassPatch {
    fine: vec2<i32>,
    level: u32,
    kind: u32,
    candidates: u32,
    per_fine: u32,
    flags: u32,
    pad: u32,
}

const GRASS_REGIONS: u32 = 4u;
const GRASS_REGION_SHADOW: u32 = 3u;
const GRASS_PATCH_SHADOW: u32 = 2u;
// Set by the outdoor-portal cull only (`PATCH_NO_CAMERA`, ADR-8843): no camera blades.
const GRASS_PATCH_NO_CAMERA: u32 = 4u;

fn grass_patch_hash(seed: u32, fine: vec2<i32>) -> u32 {
    return grass_pcg(seed ^ grass_pcg(bitcast<u32>(fine.x) ^ grass_pcg(bitcast<u32>(fine.y))));
}

// Fine patch and candidate index of walk slot `k` in a level-`level` patch (Morton order).
struct GrassSlot {
    fine: vec2<i32>,
    index: u32,
}

fn grass_walk_slot(fine: vec2<i32>, level: u32, k: u32) -> GrassSlot {
    let sub_count = 1u << (2u * level);
    let sub = k % sub_count;
    var sx = 0u;
    var sz = 0u;
    for (var bit = 0u; bit < level; bit = bit + 1u) {
        sx = sx | (((sub >> (2u * bit)) & 1u) << bit);
        sz = sz | (((sub >> (2u * bit + 1u)) & 1u) << bit);
    }
    return GrassSlot(fine + vec2<i32>(i32(sx), i32(sz)), k / sub_count);
}

struct GrassCandidate {
    xz: vec2<f32>,
    rank: f32,
    hash: u32,
}

fn grass_candidate(seed: u32, fine: vec2<i32>, i: u32, n: u32, patch_m: f32) -> GrassCandidate {
    let h = grass_patch_hash(seed, fine);
    let rot = vec2<f32>(f32(h & 0xffffu), f32(h >> 16u)) / 65536.0;
    let c = grass_pcg(h ^ (i * 0x9e3779b9u));
    // Jitter by up to 0.45 of the spacing of the first i + 1 points (no lattice rows).
    let jitter = vec2<f32>(grass_unit(grass_pcg(c ^ 0x2545f491u)), grass_unit(grass_pcg(c ^ 0x68bc21ebu))) - vec2<f32>(0.5);
    let p = fract(rot + f32(i) * vec2<f32>(0.7548777, 0.5698403) + jitter * (0.9 / sqrt(f32(i + 1u))));
    let u = grass_unit(c);
    return GrassCandidate((vec2<f32>(fine) + p) * patch_m, (f32(i) + u) / f32(max(n, 1u)), c);
}

// `density_fraction` in `grass/select.rs`.
fn grass_density_fraction(d: f32, full_m: f32, range_m: f32) -> f32 {
    if (d >= range_m || full_m <= 0.0 || range_m <= 0.0) {
        return 0.0;
    }
    var base = 1.0;
    if (d > full_m) {
        base = (full_m / d) * (full_m / d);
    }
    return base * clamp((range_m - d) / (0.15 * range_m), 0.0, 1.0);
}
