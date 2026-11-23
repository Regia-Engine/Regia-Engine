// SPDX-License-Identifier: MPL-2.0
// Video convert pass (specs/50 § 2.7, ADR-2044): planar YUV samples packed in one storage buffer
// to encoded R'G'B'A8 in a storage texture (sampled through a sRGB view). The affine rows come
// from `regia_render::video::convert_rows` (ITU-R BT.601 / BT.709 / BT.2020 NCL, limited or full
// range); chroma is upsampled by nearest sample (co-sited left), as the CPU paths do.

struct Params {
    size: vec2<u32>,
    chroma_size: vec2<u32>,
    shift: vec2<u32>,
    sample_bytes: u32,
    _pad0: u32,
    // Byte offsets of the Y, U and V planes in `data` (multiples of 4).
    offsets: vec4<u32>,
    row_r: vec4<f32>,
    row_g: vec4<f32>,
    row_b: vec4<f32>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> data: array<u32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;

fn fetch(offset: u32, index: u32) -> f32 {
    let byte = offset + index * params.sample_bytes;
    let word = data[byte >> 2u];
    let shift = (byte & 3u) * 8u;
    if (params.sample_bytes == 2u) {
        return f32((word >> shift) & 0xffffu);
    }
    return f32((word >> shift) & 0xffu);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.size.x || id.y >= params.size.y) {
        return;
    }
    let y = fetch(params.offsets.x, id.y * params.size.x + id.x);
    let cx = id.x >> params.shift.x;
    let cy = id.y >> params.shift.y;
    let ci = cy * params.chroma_size.x + cx;
    let u = fetch(params.offsets.y, ci);
    let v = fetch(params.offsets.z, ci);
    let s = vec4<f32>(y, u, v, 1.0);
    let rgb = vec3<f32>(dot(params.row_r, s), dot(params.row_g, s), dot(params.row_b, s));
    let c = clamp(round(rgb), vec3<f32>(0.0), vec3<f32>(255.0)) / 255.0;
    textureStore(out_tex, vec2<i32>(id.xy), vec4<f32>(c, 1.0));
}
