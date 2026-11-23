// SPDX-License-Identifier: MPL-2.0
// Baked GI: the lookup `static_gi` (baked_gi.wgsl) at given receivers (compute;
// `baked_gi/points.rs` `gpu_sample_static_gi`). It holds the shader to its CPU mirror
// (`baked_gi.rs` `sample_static_gi`) in tests and for the agent's `render.sample_baked_gi`
// `{"gpu": true}`. Composed as baked_gi.wgsl + this file.

struct BgiPoint {
    // xyz = receiver position.
    p: vec4<f32>,
    // xyz = unit normal.
    n: vec4<f32>,
    // xyz = unit vector toward the viewer.
    v: vec4<f32>,
}

struct BgiPointRange {
    // x = receivers, y = first result vec4 of this dispatch.
    range: vec4<u32>,
}

@group(0) @binding(0) var<uniform> bgi: BakedGiUniform;
@group(0) @binding(1) var<storage, read> bgi_data: array<u32>;
@group(0) @binding(2) var<storage, read> bgi_points: array<BgiPoint>;
@group(0) @binding(3) var<storage, read_write> bgi_results: array<vec4<f32>>;
@group(0) @binding(4) var<uniform> bgi_range: BgiPointRange;

// `baked_gi.wgsl` reads the volume through these (the screen pass picks a slot instead).
fn bgi_u() -> BakedGiUniform {
    return bgi;
}

fn bgi_word(i: u32) -> u32 {
    return bgi_data[i];
}

// Four vec4 per receiver: (L00 rgb, coverage), (red L1, visibility), (green L1, the brick's
// fraction of the light), (blue L1, the terrain layer's fraction; `StaticGi::split`, ADR-1104).
@compute @workgroup_size(64)
fn baked_point_lookup(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= bgi_range.range.x) {
        return;
    }
    let q = bgi_points[i];
    let gi = static_gi(q.p.xyz, q.n.xyz, q.v.xyz);
    let at = bgi_range.range.y + i * 4u;
    bgi_results[at] = vec4<f32>(gi.sh.l0, gi.cov);
    bgi_results[at + 1u] = vec4<f32>(gi.sh.r, gi.vis);
    bgi_results[at + 2u] = vec4<f32>(gi.sh.g, gi.split.x);
    bgi_results[at + 3u] = vec4<f32>(gi.sh.b, gi.split.y);
}
