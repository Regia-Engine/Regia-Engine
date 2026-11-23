// SPDX-License-Identifier: MIT
// Radix sort setup: dispatch sizes and per-sort constants from a GPU-side key count.
//
// Portions ported from FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// commit c6efa6b, tag v1.1.4), FidelityFX Parallel Sort
// (ffxParallelSortSetConstantAndDispatchData in ffx_parallelsort.h and
// ffx_parallelsort_setup_indirect_args.h), tag v1.1.4 only. MIT licence:
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
// Regia Engine change: the CPU-side constant computation runs here, on the GPU, so the sort
// covers exactly the keys the simulation gathered this frame.

const BLOCK: u32 = 512u;
const BINS: u32 = 16u;
const MAX_TG: u32 = 800u;

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

struct SetupParams {
    count_index: u32,
    max_keys: u32,
    pad0: u32,
    pad1: u32,
};

@group(0) @binding(0) var<storage, read> counters: array<u32>;
@group(0) @binding(1) var<storage, read_write> consts: SortConsts;
@group(0) @binding(2) var<storage, read_write> args: array<u32>;
@group(0) @binding(3) var<uniform> sp: SetupParams;

@compute @workgroup_size(1)
fn setup() {
    let n = min(counters[sp.count_index], sp.max_keys);
    let blocks = (n + BLOCK - 1u) / BLOCK;
    var tg = MAX_TG;
    var per_tg = blocks / tg;
    var extra = blocks % tg;
    if (blocks < tg) {
        per_tg = 1u;
        tg = blocks;
        extra = 0u;
    }
    var reduce_groups = BINS;
    if (BLOCK <= tg) {
        reduce_groups = BINS * ((tg + BLOCK - 1u) / BLOCK);
    }
    consts.num_keys = n;
    consts.blocks_per_tg = per_tg;
    consts.num_tg = tg;
    consts.num_tg_extra = extra;
    consts.reduce_tg_per_bin = reduce_groups / BINS;
    consts.num_scan_values = reduce_groups;
    consts.pad0 = 0u;
    consts.pad1 = 0u;
    // Count and scatter run one group per thread group; reduce and scan_add one per scan value.
    args[0] = tg;
    args[1] = 1u;
    args[2] = 1u;
    args[3] = reduce_groups;
    args[4] = 1u;
    args[5] = 1u;
}
