// SPDX-License-Identifier: MIT
// Portions ported from Wicked Engine (https://github.com/turanszkij/WickedEngine, commit
// 7f776d0209), shaders/virtualTextureTileRequestsCS.hlsl and
// shaders/virtualTextureTileAllocateCS.hlsl. Copyright (c) 2026 Turánszki János, MIT licence
// (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// Terrain virtual texture: page-table updates, feedback reduction and page requests
// (ADR-117 "Terrain virtual texture"; the bake and the lookup are in terrain.wgsl).
//
// Ported: the feedback → request pyramid (a min-LOD reduction over the page mip chain) and the
// allocation list (every page requested at its own LOD or finer, appended for the CPU).
// Adapted: one global virtual texture over the terrain's XZ instead of per-chunk sparse
// textures; feedback stores 255 − LOD with atomicMax into zero-cleared buffers; the
// allocation list is double-buffered — appended in arrival order (lossless while the count
// fits) and atomicMax-ed into a hashed overflow region, so an over-capacity request burst
// keeps the same set on every GPU and driver instead of whichever pages won the arrival
// race; the CPU reads the list back, allocates atlas slots and schedules bakes (`gpu::vt`),
// and its table writes reach the GPU through `cs_table`; no residency-map pass (a pixel
// whose pages are not resident shades the surface in full instead of falling back to a
// coarser page).

struct VtReduce {
    // x = LOD-0 pages per side, y = LOD count, z = allocation capacity, w = total pages
    dims: vec4<u32>,
    lod_offset: array<vec4<u32>, 4>,
    // x = page-table writes in `updates` this frame
    misc: vec4<u32>,
};

@group(0) @binding(0) var<uniform> params: VtReduce;
// LOD-0 page grid: 255 − the finest LOD a pixel there asked for (0 = none); then the two
// counters the colour pass keeps (sampled pixels that missed, that hit).
@group(0) @binding(1) var<storage, read_write> feedback: array<atomic<u32>>;
// The page pyramid: 255 − the finest LOD requested inside each page (0 = none).
@group(0) @binding(2) var<storage, read_write> requests: array<atomic<u32>>;
// [0] = count, [1] = missed, [2] = hit, [3] unused, then two capacity-sized regions:
//   [4, 4 + dims.z)            the arrival list: one packed page per entry, in atomic
//                              arrival order, complete while the request count fits;
//   [4 + dims.z, 4 + 2*dims.z) the deterministic overflow list: every requested page
//                              atomicMax-es its stored word at `hash_overflow_slot(key)`,
//                              so the surviving set is a pure function of the request set
//                              (the maximum wins whatever the scheduling order, coarse
//                              words compare larger than fine ones, and within a LOD the
//                              permuted position decides, so no rows are preferred).
// A stored word is ALLOC_VALID | lod << 26 | perm26(y << 13 | x); 0 marks a slot no page
// hashed to. The CPU inverts `perm26` (`vt.rs`, `alloc_key`).
@group(0) @binding(3) var<storage, read_write> allocation: array<atomic<u32>>;
// The page tables (atlas slot + 1, 0 = not resident).
@group(0) @binding(4) var<storage, read_write> pages: array<u32>;
// This frame's table writes: x = page index, y = value.
@group(0) @binding(5) var<storage, read> updates: array<vec2<u32>>;

const ALLOC_HEADER: u32 = 4u;
// Set on every stored allocation word; keys keep to bits 0..29 (lod < 16).
const ALLOC_VALID: u32 = 0x80000000u;

fn lod_offset(lod: u32) -> u32 {
    let v = params.lod_offset[lod / 4u];
    switch lod % 4u {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

// Applies the CPU's page-table writes (pages made resident or evicted) before the bake and
// the colour pass read the tables.
@compute @workgroup_size(64, 1, 1)
fn cs_table(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.misc.x) {
        return;
    }
    let u = updates[gid.x];
    pages[u.x] = u.y;
}

// Each LOD-0 page's request propagates to the pages covering it at every coarser LOD.
@compute @workgroup_size(8, 8, 1)
fn cs_requests(@builtin(global_invocation_id) gid: vec3<u32>) {
    let side = params.dims.x;
    if (gid.x == 0u && gid.y == 0u) {
        atomicStore(&allocation[1], atomicLoad(&feedback[side * side]));
        atomicStore(&allocation[2], atomicLoad(&feedback[side * side + 1u]));
    }
    if (gid.x >= side || gid.y >= side) {
        return;
    }
    let v = atomicLoad(&feedback[gid.y * side + gid.x]);
    if (v == 0u) {
        return;
    }
    for (var lod = 0u; lod < params.dims.y; lod++) {
        let l_side = max(side >> lod, 1u);
        let idx = lod_offset(lod) + (gid.y >> lod) * l_side + (gid.x >> lod);
        atomicMax(&requests[idx], v);
    }
}

// The overflow slot a page hashes to: an avalanche finalizer (C. Wellons' lowbias32,
// xor-multiply scramble, public-domain sources, see docs/provenance.md) of the *packed key* —
// LOD included — so pages that share a `local` on different LODs, or sit a multiple of a
// power-of-two capacity apart within one, do not systematically collide (the old
// `(local * 2654435761) % capacity` kept only the low bits for power-of-two capacities and
// starved whole strips of the map; fixrev-22-k, Muse review of fix-terrain1-determinism
// finding 1). `atomicMax` on the stored word still resolves the remaining collisions, coarse
// first.
// Twin of the `#[cfg(test)]` mirror in `vt.rs`: edit both together.
fn hash_overflow_slot(key: u32, capacity: u32) -> u32 {
    var h = key;
    h = h ^ (h >> 16u);
    h = h * 0x7feb352du;
    h = h ^ (h >> 15u);
    h = h * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return h % capacity;
}

// A bijection of the 26 position bits `y << 13 | x` (odd multiply, xorshift, odd multiply,
// xorshift, modulo 2^26): the overflow priority within a LOD. `atomicMax` on the raw key kept
// the largest y, then x, at every colliding slot, so an over-capacity burst granted pages
// only to the window's last rows (rev-kimi-19, fixrev-22-k finding 3). Twin of `perm26` in
// `vt.rs`, whose `unperm26` inverts it: edit both together.
fn perm26(v: u32) -> u32 {
    var p = (v * 0x2c1b3c6du) & 0x3ffffffu;
    p = p ^ (p >> 13u);
    p = (p * 0x297a2d39u) & 0x3ffffffu;
    return p ^ (p >> 13u);
}

// Workgroups per row of the allocation dispatch (2-D past the 65 535-group limit).
const ALLOC_ROW_GROUPS: u32 = 32768u;

// Every page requested at its own LOD or finer (it is in the requesting pixel's chain of
// coarser pages) goes to the allocation list, twice:
// - appended in arrival order (complete while the count in allocation[0] fits the
//   capacity: the common case stays lossless);
// - atomicMax-ed into the overflow region at `hash_overflow_slot(key)`, so when the count
//   exceeds the capacity the surviving set is still identical for every workgroup
//   scheduling order — the stored-word maximum wins the slot whatever order the writes
//   land in, a coarser page's word compares larger than a finer one's, and within a LOD
//   the permuted position (`perm26`) decides.
@compute @workgroup_size(64, 1, 1)
fn cs_allocate(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.y * ALLOC_ROW_GROUPS * 64u + gid.x;
    if (i >= params.dims.w) {
        return;
    }
    let req = atomicLoad(&requests[i]);
    if (req == 0u) {
        return;
    }
    var lod = 0u;
    for (var l = 1u; l < params.dims.y; l++) {
        if (lod_offset(l) <= i) {
            lod = l;
        }
    }
    if (255u - req > lod) {
        return;
    }
    let l_side = max(params.dims.x >> lod, 1u);
    let local = i - lod_offset(lod);
    let x = local % l_side;
    let y = local / l_side;
    let key = ALLOC_VALID | (lod << 26u) | (y << 13u) | x;
    let word = ALLOC_VALID | (lod << 26u) | perm26((y << 13u) | x);
    let n = atomicAdd(&allocation[0], 1u);
    if (n < params.dims.z) {
        atomicStore(&allocation[ALLOC_HEADER + n], word);
    }
    let h = hash_overflow_slot(key, params.dims.z);
    atomicMax(&allocation[ALLOC_HEADER + params.dims.z + h], word);
}
