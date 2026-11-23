// SPDX-License-Identifier: MPL-2.0
// Visibility-buffer clusters from a counted mesh's upload (ADR-7462). One invocation per vertex
// copies an uploaded `LabVertex` (`params.source_words` words: position, normal, uv, tangent,
// colour, joints, weights) into the cluster arena's 16-word vertex at `params.dst_word`, in the
// order the CPU path's `push_vertex` writes: position, uv.x, normal, uv.y, tangent, colour.
// The range is GPU-owned: the arena's CPU mirror never writes it.

struct RepackParams {
    // Vertices to copy.
    count: u32,
    // First geometry word of the destination range.
    dst_word: u32,
    // Workgroups per dispatch row (a dispatch past 65 535 groups uses rows).
    row_groups: u32,
    // Words per uploaded vertex.
    source_words: u32,
}

@group(0) @binding(0) var<storage, read> source: array<u32>;
@group(0) @binding(1) var<storage, read_write> geo: array<u32>;
@group(0) @binding(2) var<uniform> params: RepackParams;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = gid.y * params.row_groups * 64u + gid.x;
    if v >= params.count {
        return;
    }
    let s = v * params.source_words;
    let d = params.dst_word + v * 16u;
    // Position, then uv.x.
    geo[d] = source[s];
    geo[d + 1u] = source[s + 1u];
    geo[d + 2u] = source[s + 2u];
    geo[d + 3u] = source[s + 6u];
    // Normal, then uv.y.
    geo[d + 4u] = source[s + 3u];
    geo[d + 5u] = source[s + 4u];
    geo[d + 6u] = source[s + 5u];
    geo[d + 7u] = source[s + 7u];
    // Tangent and colour, in place.
    for (var i = 8u; i < 16u; i = i + 1u) {
        geo[d + i] = source[s + i];
    }
}
