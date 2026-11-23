// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::blue_noise

// Spatiotemporal blue noise (ADR-2075): the precomputed table of `src/blue_noise.rs`, a
// `texture_2d_array` of R8 unorm, 64 × 64 × 16. `regia_render::blue_noise::{scalar, pair}` mirror
// these functions.

// The number at pixel `pix` of frame `frame`, in (0, 1): the byte, centered in its bin.
fn scalar(table: texture_2d_array<f32>, pix: vec2<u32>, frame: u32) -> f32 {
    let t = textureLoad(table, vec2<i32>(pix & vec2<u32>(63u)), i32(frame & 15u), 0).r;
    return (floor(t * 255.0 + 0.5) + 0.5) / 256.0;
}

// Two numbers: the table at the pixel and at a far-off place in space and time.
fn pair(table: texture_2d_array<f32>, pix: vec2<u32>, frame: u32) -> vec2<f32> {
    return vec2<f32>(scalar(table, pix, frame), scalar(table, pix + vec2<u32>(23u, 41u), frame + 5u));
}
