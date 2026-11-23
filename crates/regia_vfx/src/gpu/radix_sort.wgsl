// SPDX-License-Identifier: MIT
// GPU radix sort of (key, payload) pairs with a GPU-side key count.
//
// Portions ported from FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// commit c6efa6b, tag v1.1.4), FidelityFX Parallel Sort
// (sdk/include/FidelityFX/gpu/parallelsort/ffx_parallelsort.h and the
// ffx_parallelsort_{setup_indirect_args,scan,scan_add}.h wrappers), tag v1.1.4 only.
//
// Copyright (C) 2024 Advanced Micro Devices, Inc.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files(the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and /or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions :
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
//
// Regia Engine changes: HLSL → WGSL; keys and payloads travel together as vec2<u32>; the wave
// intrinsics (WaveActiveSum, WavePrefixSum) are workgroup-memory scans so the sort runs
// without subgroup support (lavapipe, Android); the setup pass computes the constants
// (FidelityFX computes them on the CPU) from a count the simulation wrote on the GPU;
// the key preload array is a per-element load (no function-local arrays, ADR-026).
// The scan scratch is one u32 per index (`array<u32, 512>`): scan_prefix stores
// i*TG+lid from every invocation before the barrier, so a vec4 component
// read-modify-write would race (lanes 0, 32, 64 and 96 share one variable).
// Tie-break passes (`PassParams.payload == 1`) take the digit from the payload instead of the
// key, and out-of-range scatter items carry 0xFFFFFFFF in both words.

const TG: u32 = 128u;           // FFX_PARALLELSORT_THREADGROUP_SIZE
const EPT: u32 = 4u;            // FFX_PARALLELSORT_ELEMENTS_PER_THREAD
const BLOCK: u32 = 512u;        // EPT * TG
const BINS: u32 = 16u;          // FFX_PARALLELSORT_SORT_BIN_COUNT (4 bits per pass)
const MAX_TG: u32 = 800u;       // FFX_PARALLELSORT_MAX_THREADGROUPS_TO_RUN

struct SortConsts {
    num_keys: u32,
    blocks_per_tg: u32,
    num_tg: u32,
    num_tg_extra: u32,
    reduce_tg_per_bin: u32,
    num_scan_values: u32,
    pad0: u32,
    pad1: u32,
};

struct PassParams {
    shift: u32,
    src_base: u32,
    dst_base: u32,
    payload: u32,
};

@group(0) @binding(0) var<storage, read> src: array<vec2<u32>>;
@group(0) @binding(1) var<storage, read_write> dst: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read_write> sums: array<u32>;
@group(0) @binding(3) var<storage, read_write> reduced: array<u32>;
@group(0) @binding(4) var<storage, read> consts: SortConsts;
@group(1) @binding(0) var<uniform> pp: PassParams;

// The word this pass sorts by: the key, or the payload on a tie-break pass.
fn sort_word(item: vec2<u32>) -> u32 {
    return select(item.x, item.y, pp.payload == 1u);
}

var<workgroup> scan_lds: array<u32, 128>;
var<workgroup> scan_total: u32;

// Exclusive prefix sum of one value per invocation over the workgroup (Hillis–Steele).
// `scan_total` receives the inclusive total. Call from uniform control flow.
fn block_exclusive_scan(v: u32, lid: u32) -> u32 {
    scan_lds[lid] = v;
    workgroupBarrier();
    for (var off = 1u; off < TG; off = off << 1u) {
        var add = 0u;
        if (lid >= off) {
            add = scan_lds[lid - off];
        }
        workgroupBarrier();
        scan_lds[lid] = scan_lds[lid] + add;
        workgroupBarrier();
    }
    let inclusive = scan_lds[lid];
    if (lid == TG - 1u) {
        scan_total = inclusive;
    }
    workgroupBarrier();
    return inclusive - v;
}

// --- count: per-group 16-bin histogram of the current digit -----------------------------

var<workgroup> hist: array<u32, 2048>;   // BINS * TG, one slot per invocation per bin
var<workgroup> wg_start: u32;
var<workgroup> wg_blocks: u32;

fn group_blocks(group: u32, lid: u32) {
    if (lid == 0u) {
        let ntg = consts.num_tg;
        let extra = consts.num_tg_extra;
        var start = BLOCK * consts.blocks_per_tg * group;
        var blocks = consts.blocks_per_tg;
        if (group >= ntg - extra) {
            start = start + (group - (ntg - extra)) * BLOCK;
            blocks = blocks + 1u;
        }
        wg_start = start;
        wg_blocks = blocks;
    }
}

@compute @workgroup_size(128)
fn count(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let group = wg.x;
    for (var b = 0u; b < BINS; b = b + 1u) {
        hist[b * TG + lid] = 0u;
    }
    group_blocks(group, lid);
    let blocks = workgroupUniformLoad(&wg_blocks);
    let start = workgroupUniformLoad(&wg_start);
    let nk = consts.num_keys;
    var block_index = start + lid;
    for (var bc = 0u; bc < blocks; bc = bc + 1u) {
        var di = block_index;
        for (var i = 0u; i < EPT; i = i + 1u) {
            if (di < nk) {
                let digit = (sort_word(src[pp.src_base + di]) >> pp.shift) & 0xfu;
                hist[digit * TG + lid] = hist[digit * TG + lid] + 1u;
            }
            di = di + TG;
        }
        block_index = block_index + BLOCK;
    }
    workgroupBarrier();
    if (lid < BINS) {
        var s = 0u;
        for (var i = 0u; i < TG; i = i + 1u) {
            s = s + hist[lid * TG + i];
        }
        sums[lid * consts.num_tg + group] = s;
    }
}

// --- reduce: per-bin partial sums of the group histograms --------------------------------

@compute @workgroup_size(128)
fn reduce(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let per_bin = max(consts.reduce_tg_per_bin, 1u);
    let ntg = consts.num_tg;
    let bin = wg.x / per_bin;
    let bin_offset = bin * ntg;
    let base = (wg.x % per_bin) * BLOCK;
    var s = 0u;
    for (var i = 0u; i < EPT; i = i + 1u) {
        let di = base + i * TG + lid;
        if (di < ntg) {
            s = s + sums[bin_offset + di];
        }
    }
    _ = block_exclusive_scan(s, lid);
    if (lid == 0u) {
        reduced[wg.x] = scan_total;
    }
}

// --- scan / scan_add: exclusive prefix over the reduced table, then over each bin's -----
// --- group sums plus the scanned partial ------------------------------------------------

// One scalar per index, one writer between barriers. Not a vec4 pack and not
// one variable per slot (that form made naga emit tens of thousands of lines).
var<workgroup> lds: array<u32, 512>;     // [row * TG + col], EPT rows

fn load_scan(i: u32, from_reduced: bool) -> u32 {
    if (from_reduced) {
        return reduced[i];
    }
    return sums[i];
}

fn store_scan(i: u32, v: u32, to_reduced: bool) {
    if (to_reduced) {
        reduced[i] = v;
    } else {
        sums[i] = v;
    }
}

fn scan_prefix(n: u32, lid: u32, group: u32, bin_offset: u32, base: u32, add_partial: bool, on_reduced: bool) {
    for (var i = 0u; i < EPT; i = i + 1u) {
        let di = base + i * TG + lid;
        let col = (i * TG + lid) / EPT;
        let row = (i * TG + lid) % EPT;
        var v = 0u;
        if (di < n) {
            v = load_scan(bin_offset + di, on_reduced);
        }
        lds[row * TG + col] = v;
    }
    workgroupBarrier();
    var tsum = 0u;
    for (var i = 0u; i < EPT; i = i + 1u) {
        let tmp = lds[i * TG + lid];
        lds[i * TG + lid] = tsum;
        tsum = tsum + tmp;
    }
    tsum = block_exclusive_scan(tsum, lid);
    var partial = 0u;
    if (add_partial) {
        partial = reduced[group];
    }
    for (var i = 0u; i < EPT; i = i + 1u) {
        lds[i * TG + lid] = lds[i * TG + lid] + tsum;
    }
    workgroupBarrier();
    for (var i = 0u; i < EPT; i = i + 1u) {
        let di = base + i * TG + lid;
        let col = (i * TG + lid) / EPT;
        let row = (i * TG + lid) % EPT;
        if (di < n) {
            store_scan(bin_offset + di, lds[row * TG + col] + partial, on_reduced);
        }
    }
}

@compute @workgroup_size(128)
fn scan(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    scan_prefix(consts.num_scan_values, lid, wg.x, 0u, BLOCK * wg.x, false, true);
}

@compute @workgroup_size(128)
fn scan_add(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let per_bin = max(consts.reduce_tg_per_bin, 1u);
    let bin = wg.x / per_bin;
    let bin_offset = bin * consts.num_tg;
    let base = (wg.x % per_bin) * BLOCK;
    scan_prefix(consts.num_tg, lid, wg.x, bin_offset, base, true, false);
}

// --- scatter: local 2-bit splits in workgroup memory, then the global write -------------

var<workgroup> bin_offset_cache: array<u32, 16>;
var<workgroup> local_hist: array<atomic<u32>, 16>;
var<workgroup> lds_scratch: array<u32, 16>;
var<workgroup> reorder: array<vec2<u32>, 128>;

@compute @workgroup_size(128)
fn scatter(@builtin(local_invocation_index) lid: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let group = wg.x;
    let nk = consts.num_keys;
    if (lid < BINS) {
        bin_offset_cache[lid] = sums[lid * consts.num_tg + group];
    }
    group_blocks(group, lid);
    let blocks = workgroupUniformLoad(&wg_blocks);
    let start = workgroupUniformLoad(&wg_start);
    var block_index = start + lid;
    for (var bc = 0u; bc < blocks; bc = bc + 1u) {
        var di = block_index;
        for (var i = 0u; i < EPT; i = i + 1u) {
            if (lid < BINS) {
                atomicStore(&local_hist[lid], 0u);
            }
            var item = vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
            if (di < nk) {
                item = src[pp.src_base + di];
            }
            // Sort this round's 128 items by the digit in workgroup memory, two bits at a time.
            for (var bit = 0u; bit < 4u; bit = bit + 2u) {
                let digit = (sort_word(item) >> pp.shift) & 0xfu;
                let two = (digit >> bit) & 0x3u;
                var packed = 1u << (two * 8u);
                var local_sum = block_exclusive_scan(packed, lid);
                // Totals of the four 2-bit keys, one per byte; byte k gets the totals below k.
                let totals = scan_total;
                local_sum = local_sum + (totals << 8u) + (totals << 16u) + (totals << 24u);
                let offset = (local_sum >> (two * 8u)) & 0xffu;
                reorder[offset] = item;
                workgroupBarrier();
                item = reorder[lid];
                workgroupBarrier();
            }
            let digit = (sort_word(item) >> pp.shift) & 0xfu;
            atomicAdd(&local_hist[digit], 1u);
            workgroupBarrier();
            if (lid < BINS) {
                var p = 0u;
                for (var b = 0u; b < lid; b = b + 1u) {
                    p = p + atomicLoad(&local_hist[b]);
                }
                lds_scratch[lid] = p;
            }
            let global_offset = bin_offset_cache[digit];
            workgroupBarrier();
            let local_offset = lid - lds_scratch[digit];
            let total = global_offset + local_offset;
            if (total < nk) {
                dst[pp.dst_base + total] = item;
            }
            workgroupBarrier();
            if (lid < BINS) {
                bin_offset_cache[lid] = bin_offset_cache[lid] + atomicLoad(&local_hist[lid]);
            }
            workgroupBarrier();
            di = di + TG;
        }
        block_index = block_index + BLOCK;
    }
}
