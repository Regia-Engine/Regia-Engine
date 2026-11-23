// SPDX-License-Identifier: MIT
// Portions ported from Wicked Engine (https://github.com/turanszkij/WickedEngine, commit 7f776d0),
// Copyright (c) 2026 Turánszki János, MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE
// OR OTHER DEALINGS IN THE SOFTWARE.

// Impostor cull, after Wicked Engine's `impostor_prepareCS.hlsl`: one thread per resident
// instance; an instance past its class's fade start, inside its far range, not shrunk out and
// inside the frustum appends its index to the visible list and six vertices to the indirect
// draw. Wicked reduces the append to one atomic per wave (`WaveActiveCountBits`); WGSL has no
// portable wave ops, so the reduction is per workgroup (one global atomic per 64 instances).
// The billboard is built in the vertex shader (`impostor.wgsl`), not written out here.
// Prepended at load: `impostor_common.wgsl`.

@group(0) @binding(0) var<uniform> params: ImpParams;
@group(0) @binding(1) var<storage, read> instances: array<ImpInstance>;
@group(0) @binding(2) var<storage, read> protos: array<ImpProto>;
@group(0) @binding(3) var<storage, read_write> visible: array<u32>;
// draw_indirect args: vertex_count, instance_count, first_vertex, first_instance.
@group(0) @binding(4) var<storage, read_write> args: array<atomic<u32>, 4>;
// One bit per slot: its tile lies in some outdoor-portal frustum (read when `counts.z` != 0: the
// eye stands inside a cell, ADR-8843).
@group(0) @binding(5) var<storage, read> tile_mask: array<u32>;

var<workgroup> wg_count: atomic<u32>;
var<workgroup> wg_base: u32;

fn imp_visible(i: u32) -> bool {
    if (i >= params.counts.x) {
        return false;
    }
    let inst = instances[i];
    // A freed slot (its tile was evicted).
    if (inst.packed == 0xffffffffu) {
        return false;
    }
    // Behind the walls of the eye's cell: its tile is in no outdoor-portal frustum.
    if (params.counts.z != 0u && ((tile_mask[i >> 5u] >> (i & 31u)) & 1u) == 0u) {
        return false;
    }
    let p = protos[imp_proto_index(inst)];
    // Leafless under the class's season, and no bare card to draw (ADR-3146).
    if (p.extra.y < 0.0 && !imp_leaf_present(p, inst)) {
        return false;
    }
    let d = distance(inst.centre, params.eye.xyz);
    if (imp_band(p, d) <= 0.0 || d >= p.range.z) {
        return false;
    }
    let s = imp_shrink(params, p, inst, d);
    if (s < 0.02) {
        return false;
    }
    let c = inst.pivot + (inst.centre - inst.pivot) * s;
    let r = inst.radius * s * 1.1;
    for (var k = 0u; k < 6u; k++) {
        let plane = params.planes[k];
        if (dot(plane.xyz, c) + plane.w < -r) {
            return false;
        }
    }
    return true;
}

@compute @workgroup_size(64)
fn cs_cull(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    if (li == 0u) {
        atomicStore(&wg_count, 0u);
    }
    workgroupBarrier();
    let keep = imp_visible(gid.x);
    var local = 0u;
    if (keep) {
        local = atomicAdd(&wg_count, 1u);
    }
    workgroupBarrier();
    if (li == 0u) {
        let n = atomicLoad(&wg_count);
        var base = 0u;
        if (n > 0u) {
            base = atomicAdd(&args[0], n * 6u) / 6u;
        }
        wg_base = base;
    }
    workgroupBarrier();
    if (keep) {
        visible[wg_base + local] = gid.x;
    }
}
