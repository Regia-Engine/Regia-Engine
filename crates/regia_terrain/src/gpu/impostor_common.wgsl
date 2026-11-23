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

// Octahedral impostors, shared by the cull (`impostor_cull.wgsl`) and the draw
// (`impostor.wgsl`): the instance and prototype layouts (`ImpostorInstance`,
// `ImpostorProto` in `scatter/impostor.rs`), the per-frame parameters, the hand-over band
// and the far shrink. Structure after Wicked Engine's impostor shaders (`impostorHF.hlsli`).

struct ImpInstance {
    // Bounding-sphere centre (world, unseated) and radius.
    centre: vec3<f32>,
    radius: f32,
    // Model origin (buried pivot) and uniform scale.
    pivot: vec3<f32>,
    scale: f32,
    // Rotation quaternion xyzw as snorm16 pairs.
    rotation: vec2<u32>,
    // Prototype (low 16 bits), tint variant (bits 16..24).
    packed: u32,
    // Place in the leaf order, 24 bits (`scatter/season.rs`, `leaf_rank`).
    leaf_rank: u32,
}

struct ImpProto {
    // rgb = material base colour × class tint, a = tint jitter.
    tint: vec4<f32>,
    // x = card (atlas layer), y = centre height, z = bounding radius, w = roughness.
    card: vec4<f32>,
    // x = mesh fade start, y = mesh fade end, z = impostor far end, w = metallic.
    range: vec4<f32>,
    // x = tint variants, y = the card a leafless instance draws (negative: none).
    extra: vec4<f32>,
    // rgb = the class season's tint on the leaf card, a = its density (all 1 without a season).
    season: vec4<f32>,
}

struct ImpParams {
    // Camera frustum (unjittered), `dot(p.xyz, x) + p.w >= 0` inside.
    planes: array<vec4<f32>, 6>,
    // xyz = eye, w unused.
    eye: vec4<f32>,
    // x = target height (px), y = 1 / tan(fov_y / 2), z = min_pixels, w = dither frame index.
    screen: vec4<f32>,
    // x = grid, y = cell (px), z = mip count, w = world metres per pixel per metre of distance.
    atlas: vec4<f32>,
    // x = instance count, y bit 0 = the view is held (the hand-over dither's phase, ADR-3146).
    counts: vec4<u32>,
}

// The impostor's share of the instance's pixels: 0 before the class's fade band, 1 past it
// (`impostor_band` in `scatter/field.rs`; the mesh keeps the complement).
fn imp_band(p: ImpProto, d: f32) -> f32 {
    return clamp((d - p.range.x) / max(p.range.y - p.range.x, 1.0e-3), 0.0, 1.0);
}

// Far shrink about the buried pivot (1 = full size): over the last 15 % of the far range,
// and as the projected radius drops from 2 × min_pixels to min_pixels.
fn imp_shrink(params: ImpParams, p: ImpProto, inst: ImpInstance, d: f32) -> f32 {
    let far = p.range.z;
    let t_far = clamp((far - d) / max(0.15 * far, 1.0e-3), 0.0, 1.0);
    let px = inst.radius * params.screen.x * 0.5 * params.screen.y / max(d, 1.0e-3);
    let min_px = max(params.screen.z, 1.0e-3);
    let t_px = clamp((px - min_px) / min_px, 0.0, 1.0);
    let t = min(t_far, t_px);
    return t * t * (3.0 - 2.0 * t);
}

// Whether the instance keeps its leaves (ADR-3146): `rank_present` in `scatter/season.rs`, the same
// two floats compared (a 24-bit rank and 2^-24 are exact in f32; the table's density is in 0..=1).
fn imp_leaf_present(p: ImpProto, inst: ImpInstance) -> bool {
    return f32(inst.leaf_rank) * (1.0 / 16777216.0) < p.season.a;
}

fn imp_proto_index(inst: ImpInstance) -> u32 {
    return inst.packed & 0xffffu;
}
