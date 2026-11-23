// SPDX-License-Identifier: MPL-2.0
//! Patch selection and the density law (CPU mirror of `grass_cull.wgsl`).
//!
//! The ground is cut into fine patches [`PATCH_M`] wide on a grid anchored at the world origin.
//! Fine patch `(fx, fz)` owns `n` candidate clumps; candidate `i` sits at the `i`-th point of
//! the R2 low-discrepancy sequence (Roberts 2018) rotated by the patch's hash, and has the rank
//! `t = (i + u) / n` (`u` a per-candidate hash in `0..1`), jittered by up to 0.45 of the spacing of
//! the first `i + 1` points so the sequence's lattice rows never show. A clump at distance `d` with mask
//! density `m` draws when `t < f(d) · m`, where [`density_fraction`] is 1 out to the tier's
//! full-density distance and `(full / d)²` past it: the clumps per screen area stay constant,
//! and each clump's visibility depends only on itself, so nothing pops at a patch edge.
//!
//! Patches far away are drawn coarse: a level-`L` patch covers `2^L × 2^L` fine patches and
//! walks only the first `ceil(n / 4^L)` candidates of each (`4^L · ceil(n / 4^L)` in all). The
//! selection gives a patch level `L` only where `f ≤ 4^-L` over the whole patch, so the
//! candidates a coarse patch skips are exactly ones the law would have culled: a coarse patch
//! draws the same clumps its fine patches would.

use super::quality::GrassBudget;
use crate::sampler::TerrainMap;

/// Edge of a fine patch, metres.
pub const PATCH_M: f32 = 8.0;

/// Coarsest patch level (a level-4 patch is 128 m wide).
pub const MAX_PATCH_LEVEL: u32 = 4;

/// Most patches one frame selects across every class (the GPU patch buffer's size).
pub const MAX_GRASS_PATCHES: usize = 4096;

/// Most candidates a fine patch owns (64 clumps / m² over an 8 m patch is 4096; the cap keeps
/// a coarse patch's walk bounded).
pub const MAX_CANDIDATES_PER_FINE_PATCH: u32 = 2048;

/// Patch flag: the patch's box meets the camera frustum.
pub const PATCH_CAMERA: u32 = 1;
/// Patch flag: the patch lies inside the shadow distance of a class that casts shadows.
pub const PATCH_SHADOW: u32 = 2;
/// Patch flag: the camera passes draw none of the patch's blades, set only by the outdoor-portal
/// cull (the patch lies behind the walls of the cell the eye is in, ADR-8843): a shadow patch
/// keeps casting into the cascades. `grass_cull.wgsl` skips the camera regions of such a patch.
pub const PATCH_NO_CAMERA: u32 = 4;

/// R2 sequence step (`1 / φ₂`, `1 / φ₂²`, φ₂ the plastic number; Roberts 2018).
pub const R2_STEP: [f32; 2] = [0.754_877_7, 0.569_840_3];

/// One selected patch, as the GPU reads it (`GrassPatch` in `grass_common.wgsl`, 32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GrassPatch {
    /// Fine patch coordinates of the patch's −X/−Z corner.
    pub fine: [i32; 2],
    /// Level: the patch covers `2^level × 2^level` fine patches.
    pub level: u32,
    /// Class index in the set.
    pub class: u32,
    /// Candidates the GPU walks (`4^level · ceil(n / 4^level)`).
    pub candidates: u32,
    /// Candidates each fine patch owns (`n`).
    pub per_fine: u32,
    /// [`PATCH_CAMERA`] | [`PATCH_SHADOW`] | [`PATCH_NO_CAMERA`].
    pub flags: u32,
    /// Unused (alignment).
    pub pad: u32,
}

impl GrassPatch {
    /// World XZ of the −X/−Z corner and the edge length, metres.
    #[must_use]
    pub fn rect(&self) -> ([f32; 2], f32) {
        (
            [self.fine[0] as f32 * PATCH_M, self.fine[1] as f32 * PATCH_M],
            PATCH_M * (1u32 << self.level) as f32,
        )
    }
}

/// What the selection needs from the frame.
#[derive(Clone, Copy, Debug)]
pub struct GrassView {
    /// Camera eye, world metres.
    pub eye: [f32; 3],
    /// Camera frustum planes (outward, Gribb–Hartmann, `regia_render::Frustum::planes`);
    /// `None` accepts every patch in range.
    pub planes: Option<[[f32; 4]; 6]>,
}

/// One class as the selection sees it.
#[derive(Clone, Copy, Debug)]
pub struct ClassRange {
    /// Range after the tier's cap, metres.
    pub range_m: f32,
    /// Candidates per fine patch ([`candidates_per_fine_patch`]).
    pub per_fine: u32,
    /// Shadow distance for this class (0 when it casts none or the tier has none).
    pub shadow_m: f32,
    /// Tallest blade, metres (the patch boxes' headroom).
    pub max_height_m: f32,
}

/// The density fraction `f(d)` of the law (module docs): 1 inside `full_m`, `(full_m / d)²`
/// past it, faded to 0 over the last 15 % of `range_m`. 0 at or past the range.
#[must_use]
pub fn density_fraction(d: f32, full_m: f32, range_m: f32) -> f32 {
    if d >= range_m || full_m <= 0.0 || range_m <= 0.0 {
        return 0.0;
    }
    let base = if d <= full_m {
        1.0
    } else {
        (full_m / d) * (full_m / d)
    };
    let edge = ((range_m - d) / (0.15 * range_m)).clamp(0.0, 1.0);
    base * edge
}

/// The coarsest level a patch whose nearest point is `d_min` away may take: the largest
/// `L ≤ MAX_PATCH_LEVEL` with `full_m · 2^L ≤ d_min` (then `f ≤ 4^-L` over the patch).
#[must_use]
pub fn level_for_distance(d_min: f32, full_m: f32) -> u32 {
    if full_m <= 0.0 {
        return MAX_PATCH_LEVEL;
    }
    let mut level = 0;
    while level < MAX_PATCH_LEVEL && full_m * (2u32 << level) as f32 <= d_min {
        level += 1;
    }
    level
}

/// Candidates a fine patch owns for a class of `density` clumps per square metre under a tier
/// scaling it by `density_scale`.
#[must_use]
pub fn candidates_per_fine_patch(density: f32, density_scale: f32) -> u32 {
    let n = (density.max(0.0) * density_scale.clamp(0.0, 1.0) * PATCH_M * PATCH_M).ceil();
    (n as u32).min(MAX_CANDIDATES_PER_FINE_PATCH)
}

/// Candidates a level-`level` patch walks.
#[must_use]
pub fn candidates_at_level(per_fine: u32, level: u32) -> u32 {
    let sub = 1u32 << (2 * level);
    sub * per_fine.div_ceil(sub)
}

/// PCG-style 32-bit hash (Jarzynski & Olano 2020, "Hash Functions for GPU Rendering").
#[must_use]
pub fn pcg_hash(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// The hash of fine patch `(fx, fz)` in a class stream.
#[must_use]
pub fn patch_hash(seed: u32, fx: i32, fz: i32) -> u32 {
    pcg_hash(seed ^ pcg_hash((fx as u32) ^ pcg_hash(fz as u32)))
}

/// One candidate clump (CPU mirror of `grass_candidate` in `grass_common.wgsl`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    /// World X, metres.
    pub x: f32,
    /// World Z, metres.
    pub z: f32,
    /// Rank `t` in `0..1`.
    pub rank: f32,
    /// The candidate's own hash (shape and colour draws).
    pub hash: u32,
}

/// Candidate `i` of fine patch `(fx, fz)` with `n` candidates.
#[must_use]
pub fn candidate(seed: u32, fx: i32, fz: i32, i: u32, n: u32) -> Candidate {
    let h = patch_hash(seed, fx, fz);
    let rot = [(h & 0xffff) as f32 / 65536.0, (h >> 16) as f32 / 65536.0];
    let fract = |v: f32| v - v.floor();
    let c = pcg_hash(h ^ i.wrapping_mul(0x9e37_79b9));
    // Jitter each point by up to 0.45 of the spacing of the first `i + 1` points: the R2
    // lattice's rows would otherwise read as ploughed lines, at every thinning level.
    let unit = |v: u32| (v >> 8) as f32 / 16_777_216.0;
    let spread = 0.9 / ((i + 1) as f32).sqrt();
    let px = fract(rot[0] + i as f32 * R2_STEP[0] + (unit(pcg_hash(c ^ 0x2545_f491)) - 0.5) * spread);
    let pz = fract(rot[1] + i as f32 * R2_STEP[1] + (unit(pcg_hash(c ^ 0x68bc_21eb)) - 0.5) * spread);
    let u = unit(c);
    Candidate {
        x: (fx as f32 + px) * PATCH_M,
        z: (fz as f32 + pz) * PATCH_M,
        rank: (i as f32 + u) / n.max(1) as f32,
        hash: c,
    }
}

/// The fine patch and candidate index of walk slot `k` in a level-`level` patch whose corner is
/// fine patch `fine` (sub-patch `k mod 4^level` in Morton order, candidate `k / 4^level`).
#[must_use]
pub fn walk_slot(fine: [i32; 2], level: u32, k: u32) -> ([i32; 2], u32) {
    let sub_count = 1u32 << (2 * level);
    let sub = k % sub_count;
    let i = k / sub_count;
    let mut sx = 0u32;
    let mut sz = 0u32;
    for bit in 0..level {
        sx |= ((sub >> (2 * bit)) & 1) << bit;
        sz |= ((sub >> (2 * bit + 1)) & 1) << bit;
    }
    ([fine[0] + sx as i32, fine[1] + sz as i32], i)
}

fn aabb_outside(planes: &[[f32; 4]; 6], lo: [f32; 3], hi: [f32; 3]) -> bool {
    planes.iter().any(|p| {
        let v = [
            if p[0] >= 0.0 { hi[0] } else { lo[0] },
            if p[1] >= 0.0 { hi[1] } else { lo[1] },
            if p[2] >= 0.0 { hi[2] } else { lo[2] },
        ];
        p[0] * v[0] + p[1] * v[1] + p[2] * v[2] + p[3] < 0.0
    })
}

fn rect_distance(eye: [f32; 3], lo: [f32; 2], size: f32) -> f32 {
    let dx = (lo[0] - eye[0]).max(eye[0] - (lo[0] + size)).max(0.0);
    let dz = (lo[1] - eye[2]).max(eye[2] - (lo[1] + size)).max(0.0);
    (dx * dx + dz * dz).sqrt()
}

// Height samples `patch_heights` took on this thread. Other tests call it too, so a
// process-wide counter would count their samples.
#[cfg(test)]
thread_local! {
    static PATCH_HEIGHTS_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Samples [`patch_heights`] has taken on this thread since the last reset.
#[cfg(test)]
fn patch_heights_calls() -> u32 {
    PATCH_HEIGHTS_CALLS.with(std::cell::Cell::get)
}

/// Zeroes [`patch_heights_calls`] on this thread.
#[cfg(test)]
fn reset_patch_heights_calls() {
    PATCH_HEIGHTS_CALLS.with(|calls| calls.set(0));
}

/// Height range of the ground under a patch (5 × 5 samples, padded by the slope between them
/// and `headroom`).
pub(crate) fn patch_heights(map: &TerrainMap, lo: [f32; 2], size: f32, headroom: f32) -> (f32, f32) {
    #[cfg(test)]
    PATCH_HEIGHTS_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    let mut min_h = f32::INFINITY;
    let mut max_h = f32::NEG_INFINITY;
    for r in 0..5 {
        for c in 0..5 {
            let h = map.sample_bilinear(lo[0] + size * c as f32 / 4.0, lo[1] + size * r as f32 / 4.0);
            min_h = min_h.min(h);
            max_h = max_h.max(h);
        }
    }
    // Ground between two samples can rise above both by about the slope over a quarter patch.
    let pad = (max_h - min_h) * 0.5 + 0.5;
    (min_h - pad, max_h + pad + headroom)
}

struct SelectCtx<'a> {
    view: &'a GrassView,
    map: &'a TerrainMap,
    budget: &'a GrassBudget,
    class: u32,
    range: ClassRange,
    half: f32,
    /// Y range that contains every box [`patch_heights`] can return, before headroom.
    /// `None` when the view has no frustum (nothing samples heights).
    slab_y: Option<(f32, f32)>,
}

fn visit(ctx: &SelectCtx<'_>, fine: [i32; 2], level: u32, out: &mut Vec<GrassPatch>) {
    if out.len() >= MAX_GRASS_PATCHES {
        return;
    }
    let size = PATCH_M * (1u32 << level) as f32;
    let lo = [fine[0] as f32 * PATCH_M, fine[1] as f32 * PATCH_M];
    // Off the map: nothing grows there.
    if lo[0] >= ctx.half || lo[1] >= ctx.half || lo[0] + size <= -ctx.half || lo[1] + size <= -ctx.half {
        return;
    }
    let d_min = rect_distance(ctx.view.eye, lo, size);
    if d_min >= ctx.range.range_m {
        return;
    }
    let shadow = d_min < ctx.range.shadow_m;
    let camera = match ctx.view.planes.as_ref() {
        None => true,
        Some(planes) => {
            let headroom = ctx.range.max_height_m;
            // The map's height slab already contains the patch box. A node it misses
            // never needs the 5×5 samples.
            let outside_slab = ctx.slab_y.is_some_and(|(y0, y1)| {
                aabb_outside(
                    planes,
                    [lo[0], y0, lo[1]],
                    [lo[0] + size, y1 + headroom, lo[1] + size],
                )
            });
            if outside_slab {
                false
            } else {
                let (y0, y1) = patch_heights(ctx.map, lo, size, headroom);
                !aabb_outside(planes, [lo[0], y0, lo[1]], [lo[0] + size, y1, lo[1] + size])
            }
        }
    };
    if !camera && !shadow {
        return;
    }
    if level == 0 || level_for_distance(d_min, ctx.budget.full_density_m) >= level {
        let mut flags = 0;
        if camera {
            flags |= PATCH_CAMERA;
        }
        if shadow {
            flags |= PATCH_SHADOW;
        }
        out.push(GrassPatch {
            fine,
            level,
            class: ctx.class,
            candidates: candidates_at_level(ctx.range.per_fine, level),
            per_fine: ctx.range.per_fine,
            flags,
            pad: 0,
        });
        return;
    }
    let half = 1i32 << (level - 1);
    for (dz, dx) in [(0, 0), (0, half), (half, 0), (half, half)] {
        visit(ctx, [fine[0] + dx, fine[1] + dz], level - 1, out);
    }
}

/// Selects the patches of class `class` around the eye into `out` (appending; the caller
/// clears it once per frame). Root tiles (level [`MAX_PATCH_LEVEL`]) are visited nearest
/// first, so the patch cap ([`MAX_GRASS_PATCHES`]) drops the farthest ground; the order is a
/// pure function of the inputs.
pub fn select_patches(
    view: &GrassView,
    map: &TerrainMap,
    budget: &GrassBudget,
    class: u32,
    range: ClassRange,
    out: &mut Vec<GrassPatch>,
) {
    let mut roots = Vec::new();
    walk_patches(view, map, budget, class, range, out, &mut roots);
}

/// One class remembered by [`GrassSelectCache`].
#[derive(Default)]
struct ClassSlot {
    key: Option<GrassSelectKey>,
    patches: Vec<GrassPatch>,
    roots: Vec<(f32, i32, i32)>,
}

/// Inputs that change the selected patches. Floats are stored as bits.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GrassSelectKey {
    eye: [u32; 3],
    planes: Option<[[u32; 4]; 6]>,
    full_density: u32,
    density_scale: u32,
    budget_range: u32,
    max_instances: u32,
    max_blades: u32,
    range_m: u32,
    per_fine: u32,
    shadow_m: u32,
    max_height_m: u32,
    heights_generation: u64,
    map_width: u32,
    map_height: u32,
    world_width: u32,
    heights_addr: usize,
    /// Patches the earlier classes appended: the shared [`MAX_GRASS_PATCHES`] cap stops this
    /// class's walk at `MAX_GRASS_PATCHES - out_start`.
    out_start: usize,
}

/// Last selection per class. A repeat of the same view, budget, range and height generation
/// copies the patches and does not walk the quadtree. The root list is reused across misses.
#[derive(Default)]
pub(crate) struct GrassSelectCache {
    slots: Vec<ClassSlot>,
    /// Height generation included in the next [`Self::select`] key.
    heights_generation: u64,
}

impl GrassSelectCache {
    /// Height generation the next [`Self::select`] keys on. The draw bumps this when the
    /// heightfield changes in place (the buffer address stays).
    pub(crate) fn set_heights_generation(&mut self, generation: u64) {
        self.heights_generation = generation;
    }

    /// Appends class `class` into `out`, reusing the last result when the view, budget, range,
    /// the generation from [`Self::set_heights_generation`] and the length of `out` (the
    /// patches the earlier classes took from the shared cap) still match. A class index of
    /// 64 or higher is not cached.
    pub(crate) fn select(
        &mut self,
        view: &GrassView,
        map: &TerrainMap,
        budget: &GrassBudget,
        class: u32,
        range: ClassRange,
        out: &mut Vec<GrassPatch>,
    ) {
        if class as usize >= 64 {
            let mut roots = Vec::new();
            walk_patches(view, map, budget, class, range, out, &mut roots);
            return;
        }
        if self.slots.len() <= class as usize {
            self.slots.resize_with(class as usize + 1, ClassSlot::default);
        }
        let key = GrassSelectKey {
            eye: view.eye.map(f32::to_bits),
            planes: view.planes.map(|planes| planes.map(|row| row.map(f32::to_bits))),
            full_density: budget.full_density_m.to_bits(),
            density_scale: budget.density_scale.to_bits(),
            budget_range: budget.range_m.to_bits(),
            max_instances: budget.max_instances,
            max_blades: budget.max_blades,
            range_m: range.range_m.to_bits(),
            per_fine: range.per_fine,
            shadow_m: range.shadow_m.to_bits(),
            max_height_m: range.max_height_m.to_bits(),
            heights_generation: self.heights_generation,
            map_width: map.width(),
            map_height: map.height(),
            world_width: map.world_width().to_bits(),
            heights_addr: map.heights_addr(),
            out_start: out.len(),
        };
        let slot = &mut self.slots[class as usize];
        if slot.key == Some(key) {
            out.extend_from_slice(&slot.patches);
            return;
        }
        let start = out.len();
        walk_patches(view, map, budget, class, range, out, &mut slot.roots);
        slot.patches.clear();
        slot.patches.extend_from_slice(&out[start..]);
        slot.key = Some(key);
    }

    /// Capacity of the reused root buffer for `class` (tests).
    #[cfg(test)]
    fn roots_capacity(&self, class: u32) -> usize {
        self.slots
            .get(class as usize)
            .map(|s| s.roots.capacity())
            .unwrap_or(0)
    }
}

fn slab_y(view: &GrassView, map: &TerrainMap) -> Option<(f32, f32)> {
    view.planes.as_ref().map(|_| {
        let (min_h, max_h) = map.min_max_height();
        let pad = (max_h - min_h) * 0.5 + 0.5;
        (min_h - pad, max_h + pad)
    })
}

fn walk_patches(
    view: &GrassView,
    map: &TerrainMap,
    budget: &GrassBudget,
    class: u32,
    range: ClassRange,
    out: &mut Vec<GrassPatch>,
    roots: &mut Vec<(f32, i32, i32)>,
) {
    roots.clear();
    if range.per_fine == 0 || range.range_m <= 0.0 || !budget.draws() {
        return;
    }
    let root = 1i32 << MAX_PATCH_LEVEL;
    let root_m = PATCH_M * root as f32;
    let reach = range.range_m;
    let lo_x = ((view.eye[0] - reach) / root_m).floor() as i32;
    let hi_x = ((view.eye[0] + reach) / root_m).floor() as i32;
    let lo_z = ((view.eye[2] - reach) / root_m).floor() as i32;
    let hi_z = ((view.eye[2] + reach) / root_m).floor() as i32;
    for rz in lo_z..=hi_z {
        for rx in lo_x..=hi_x {
            let lo = [rx as f32 * root_m, rz as f32 * root_m];
            let d = rect_distance(view.eye, lo, root_m);
            if d < reach {
                roots.push((d, rz, rx));
            }
        }
    }
    roots.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let ctx = SelectCtx {
        view,
        map,
        budget,
        class,
        range,
        half: map.world_width() * 0.5,
        slab_y: slab_y(view, map),
    };
    for (_, rz, rx) in roots.iter().copied() {
        visit(&ctx, [rx * root, rz * root], MAX_PATCH_LEVEL, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patch cap is shared by every class, so a later class's selection depends on how
    /// many patches the earlier classes appended. A cached class must not replay a list that
    /// was cut short by a fuller earlier class.
    #[test]
    fn a_cached_class_is_not_replayed_after_an_earlier_class_shrinks() {
        let map = flat_map();
        let mut b = budget();
        b.full_density_m = 1000.0;
        let view = GrassView {
            eye: [0.0, 1.7, 0.0],
            planes: None,
        };
        let big = ClassRange {
            range_m: 300.0,
            per_fine: 4,
            shadow_m: 0.0,
            max_height_m: 1.0,
        };
        let small = ClassRange { range_m: 20.0, ..big };
        let mut cache = GrassSelectCache::default();
        let mut out = Vec::new();
        cache.select(&view, &map, &b, 0, big, &mut out);
        assert!(
            out.len() >= MAX_GRASS_PATCHES,
            "class 0 alone fills the cap: {}",
            out.len()
        );
        cache.select(&view, &map, &b, 1, big, &mut out);
        out.clear();
        cache.select(&view, &map, &b, 0, small, &mut out);
        cache.select(&view, &map, &b, 1, big, &mut out);
        let mut fresh = Vec::new();
        select_patches(&view, &map, &b, 0, small, &mut fresh);
        select_patches(&view, &map, &b, 1, big, &mut fresh);
        assert_eq!(out.len(), fresh.len(), "class 1 replayed its capped list");
        assert!(out == fresh);
    }

    fn flat_map() -> TerrainMap {
        TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 2048.0, 1.0, 0.0).expect("map")
    }

    fn budget() -> GrassBudget {
        super::super::GrassQuality::Ultra.budget()
    }

    fn range() -> ClassRange {
        ClassRange {
            range_m: 300.0,
            per_fine: 384,
            shadow_m: 0.0,
            max_height_m: 0.6,
        }
    }

    #[test]
    fn density_fades_to_zero_over_the_last_fifteen_percent_of_range() {
        // Before the ramp (d <= 0.85 * range) the law is pure (full / d)^2.
        assert!((density_fraction(200.0, 40.0, 300.0) - 0.04).abs() < 1e-6);
        // At 90 % of the range the ramp is 2/3 of the inverse-square value.
        let pure = (40.0f32 / 270.0).powi(2);
        assert!((density_fraction(270.0, 40.0, 300.0) - pure * (2.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn density_law_is_continuous_and_falls_as_inverse_square() {
        assert_eq!(density_fraction(5.0, 40.0, 300.0), 1.0);
        assert!((density_fraction(80.0, 40.0, 300.0) - 0.25).abs() < 1e-6);
        assert_eq!(density_fraction(300.0, 40.0, 300.0), 0.0);
        let mut last = 1.0;
        for step in 0..3000 {
            let d = step as f32 * 0.1;
            let f = density_fraction(d, 40.0, 300.0);
            assert!(f <= last + 1e-6, "not monotone at {d}");
            assert!(last - f < 0.01, "jump at {d}: {last} -> {f}");
            last = f;
        }
    }

    #[test]
    fn a_coarse_patch_needs_the_law_below_its_share() {
        let full = 40.0;
        for level in 0..=MAX_PATCH_LEVEL {
            let d_min = full * (1u32 << level) as f32;
            assert!(level_for_distance(d_min, full) >= level);
            let f = density_fraction(d_min, full, 10_000.0);
            assert!(f <= 1.0 / (1u32 << (2 * level)) as f32 + 1e-6);
        }
        assert_eq!(level_for_distance(10.0, 40.0), 0);
        assert_eq!(level_for_distance(1.0e6, 40.0), MAX_PATCH_LEVEL);
    }

    #[test]
    fn walk_slots_cover_each_fine_patch_once_per_round() {
        let level = 2;
        let mut seen = std::collections::BTreeMap::new();
        for k in 0..candidates_at_level(20, level) {
            let (fine, i) = walk_slot([10, -4], level, k);
            assert!(
                (10..14).contains(&fine[0]) && (-4..0).contains(&fine[1]),
                "{fine:?}"
            );
            *seen.entry((fine, i)).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), candidates_at_level(20, level) as usize);
        assert!(seen.values().all(|&c| c == 1));
    }

    /// The clumps a coarse patch draws are exactly the clumps its fine patches draw.
    #[test]
    fn coarse_patches_draw_what_their_fine_patches_draw() {
        let seed = 0x1234_5678;
        let n = 384;
        let full = 40.0;
        let eye = [0.0f32, 1.7, 0.0];
        for level in 1..=MAX_PATCH_LEVEL {
            // A patch exactly as near as the selection allows at this level.
            let d_min = full * (1u32 << level) as f32;
            let fx = (d_min / PATCH_M).ceil() as i32;
            let fine = [fx, 0];
            let keep = |c: &Candidate| {
                let d = ((c.x - eye[0]).powi(2) + eye[1].powi(2) + (c.z - eye[2]).powi(2)).sqrt();
                c.rank < density_fraction(d, full, 10_000.0)
            };
            let mut coarse = std::collections::BTreeSet::new();
            for k in 0..candidates_at_level(n, level) {
                let (f, i) = walk_slot(fine, level, k);
                let c = candidate(seed, f[0], f[1], i, n);
                if keep(&c) {
                    coarse.insert((f, i));
                }
            }
            let mut finer = std::collections::BTreeSet::new();
            let side = 1i32 << level;
            for dz in 0..side {
                for dx in 0..side {
                    for i in 0..n {
                        let f = [fine[0] + dx, fine[1] + dz];
                        if keep(&candidate(seed, f[0], f[1], i, n)) {
                            finer.insert((f, i));
                        }
                    }
                }
            }
            assert_eq!(coarse, finer, "level {level}");
        }
    }

    #[test]
    fn candidates_are_inside_their_patch_and_well_spread() {
        let n = 256;
        let mut cells = [0u32; 16];
        for i in 0..n {
            let c = candidate(99, -3, 7, i, n);
            assert!(c.x >= -3.0 * PATCH_M && c.x < -2.0 * PATCH_M, "{c:?}");
            assert!(c.z >= 7.0 * PATCH_M && c.z < 8.0 * PATCH_M, "{c:?}");
            assert!(c.rank >= i as f32 / n as f32 && c.rank < (i + 1) as f32 / n as f32);
            let cx = ((c.x / PATCH_M + 3.0) * 4.0) as usize;
            let cz = ((c.z / PATCH_M - 7.0) * 4.0) as usize;
            cells[cz.min(3) * 4 + cx.min(3)] += 1;
        }
        // Jittered R2: each of the 16 cells gets 16 ± 6 of 256 points (plain random: ± 11).
        assert!(cells.iter().all(|&c| (10..=24).contains(&c)), "{cells:?}");
        // The first 16 points are spread too (the far-field subset).
        let mut first = [0u32; 4];
        for i in 0..16 {
            let c = candidate(99, -3, 7, i, n);
            let q = usize::from(c.x / PATCH_M + 3.0 >= 0.5) + 2 * usize::from(c.z / PATCH_M - 7.0 >= 0.5);
            first[q] += 1;
        }
        assert!(first.iter().all(|&c| (1..=9).contains(&c)), "{first:?}");
    }

    #[test]
    fn selection_is_deterministic_and_coarsens_with_distance() {
        let map = flat_map();
        let view = GrassView {
            eye: [3.0, 1.7, -5.0],
            planes: None,
        };
        let mut a = Vec::new();
        select_patches(&view, &map, &budget(), 0, range(), &mut a);
        let mut b = Vec::new();
        select_patches(&view, &map, &budget(), 0, range(), &mut b);
        assert_eq!(a, b);
        assert!(!a.is_empty() && a.len() < MAX_GRASS_PATCHES);
        for p in &a {
            let (lo, size) = p.rect();
            let d = rect_distance(view.eye, lo, size);
            assert!(d < 300.0);
            assert!(
                p.level == 0 || level_for_distance(d, 40.0) >= p.level,
                "{p:?} at {d}"
            );
            assert_eq!(p.candidates, candidates_at_level(384, p.level));
        }
        // Patches do not overlap and tile the whole range disc.
        let mut covered = 0.0f64;
        for p in &a {
            let (_, size) = p.rect();
            covered += f64::from(size * size);
        }
        let disc = std::f64::consts::PI * 300.0 * 300.0;
        assert!(covered >= disc * 0.98, "covered {covered} of {disc}");
        // Inside 300 m with full density to 40 m: level 2 from 160 m (level 3 needs 320 m).
        assert!(a.iter().any(|p| p.level == 2));
        assert!(a.iter().any(|p| p.level == 0));
    }

    #[test]
    fn frustum_culls_patches_behind_the_camera_but_shadows_keep_them() {
        let map = flat_map();
        // Looking down +X: the plane x >= eye.x - 1 stands in for the frustum.
        let plane = [1.0, 0.0, 0.0, 1.0];
        let all = [[0.0, 0.0, 0.0, 1.0]; 6];
        let mut planes = all;
        planes[0] = plane;
        let view = GrassView {
            eye: [0.0, 1.7, 0.0],
            planes: Some(planes),
        };
        let mut out = Vec::new();
        select_patches(&view, &map, &budget(), 0, range(), &mut out);
        assert!(out.iter().all(|p| p.rect().0[0] + p.rect().1 >= -1.0));
        let mut shadowed = Vec::new();
        let r = ClassRange {
            shadow_m: 30.0,
            ..range()
        };
        select_patches(&view, &map, &budget(), 0, r, &mut shadowed);
        assert!(
            shadowed
                .iter()
                .any(|p| p.flags == PATCH_SHADOW && p.rect().0[0] + p.rect().1 < -1.0)
        );
    }

    #[test]
    fn nothing_selected_off_the_map_or_when_off() {
        let map = flat_map();
        let view = GrassView {
            eye: [5000.0, 1.7, 0.0],
            planes: None,
        };
        let mut out = Vec::new();
        select_patches(&view, &map, &budget(), 0, range(), &mut out);
        assert!(out.is_empty());
        let view = GrassView {
            eye: [0.0, 1.7, 0.0],
            planes: None,
        };
        select_patches(
            &view,
            &map,
            &super::super::GrassQuality::Off.budget(),
            0,
            range(),
            &mut out,
        );
        assert!(out.is_empty());
    }

    /// Planes that accept every finite box (constant +1).
    fn open_planes() -> [[f32; 4]; 6] {
        [[0.0, 0.0, 0.0, 1.0]; 6]
    }

    #[test]
    fn a_held_view_does_not_sample_patch_heights_again() {
        let map = flat_map();
        let view = GrassView {
            eye: [3.0, 1.7, -5.0],
            planes: Some(open_planes()),
        };
        let mut cache = GrassSelectCache::default();
        let mut first = Vec::new();
        reset_patch_heights_calls();
        cache.set_heights_generation(0);
        cache.select(&view, &map, &budget(), 0, range(), &mut first);
        let sampled = patch_heights_calls();
        assert!(sampled > 0, "the first walk samples heights");
        let roots = cache.roots_capacity(0);
        assert!(roots > 0);
        reset_patch_heights_calls();
        let mut second = Vec::new();
        cache.select(&view, &map, &budget(), 0, range(), &mut second);
        assert_eq!(
            patch_heights_calls(),
            0,
            "a repeat of the same view samples nothing"
        );
        assert_eq!(first, second);
        assert!(cache.roots_capacity(0) >= roots);
        // A height edit (new generation) walks again. The root buffer stays.
        reset_patch_heights_calls();
        let mut third = Vec::new();
        cache.set_heights_generation(1);
        cache.select(&view, &map, &budget(), 0, range(), &mut third);
        assert!(patch_heights_calls() > 0);
        assert_eq!(third, first);
        assert!(cache.roots_capacity(0) >= roots);
    }

    #[test]
    fn a_node_outside_the_height_slab_does_not_sample_heights() {
        let map = flat_map();
        // x >= 1e6. The map sits inside ±1024 m, so every node misses at every height.
        let mut planes = open_planes();
        planes[0] = [1.0, 0.0, 0.0, -1.0e6];
        let view = GrassView {
            eye: [0.0, 1.7, 0.0],
            planes: Some(planes),
        };
        reset_patch_heights_calls();
        let mut out = Vec::new();
        select_patches(&view, &map, &budget(), 0, range(), &mut out);
        assert!(out.is_empty());
        assert_eq!(
            patch_heights_calls(),
            0,
            "the map height slab rejects the node before the 5×5 samples"
        );
    }
}
