// SPDX-License-Identifier: MPL-2.0
// Merges the software rasterizer's pixels (`vis_software.wgsl`) into the ID target and the scene
// depth, with the hardware raster's depth test, so a software pixel loses to a nearer hardware
// pixel and everything downstream reads one visibility buffer.

struct MergeParams {
    // x = width.
    info: vec4<u32>,
}

@group(0) @binding(0) var<uniform> params: MergeParams;
@group(0) @binding(1) var<storage, read> depth_words: array<u32>;
@group(0) @binding(2) var<storage, read> id_words: array<u32>;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

struct MergeOut {
    @location(0) id: u32,
    @builtin(frag_depth) depth: f32,
}

@fragment
fn fs_merge(@builtin(position) position: vec4<f32>) -> MergeOut {
    let pixel = u32(position.y) * params.info.x + u32(position.x);
    let word = depth_words[pixel];
    let id = id_words[pixel];
    // No coverage: the cleared words.
    if (word == 0u || id == 0u) {
        discard;
    }
    return MergeOut(id, bitcast<f32>(~word));
}
