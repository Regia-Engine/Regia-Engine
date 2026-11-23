// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063564), file
// crates/bevy_anti_alias/src/smaa/smaa.wesl, Copyright (c) Bevy contributors, MIT OR Apache-2.0,
// used under the MIT licence:
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
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// Bevy's file is itself a port of SMAA (http://www.iryoku.com/smaa/, https://github.com/iryoku/smaa):
//
// Copyright (C) 2013 Jorge Jimenez (jorge@iryoku.com)
// Copyright (C) 2013 Jose I. Echevarria (joseignacioechevarria@gmail.com)
// Copyright (C) 2013 Belen Masia (bmasia@unizar.es)
// Copyright (C) 2013 Fernando Navarro (fernandn@microsoft.com)
// Copyright (C) 2013 Diego Gutierrez (diegog@unizar.es)
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to
// do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software. As clarification, there
// is no requirement that the copyright notice and permission be included in
// binary distributions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// Regia Engine port: SMAA 1× at the HIGH preset (threshold 0.1, 16 orthogonal / 8 diagonal search
// steps, 25 % corner rounding) with luma edge detection; the three passes as entry-point pairs
// of this one module; every fetch is an explicit level-0 sample; no stencil (the weight pass
// returns early where the edge texture is empty). Edges are `Rg8Unorm`, weights `Rgba8Unorm`,
// both cleared by their pass. The input is the post stack's display-encoded image, so luma is
// gamma-space as SMAA asks. Area LUT 160×560 RG8, search LUT 64×16 R8 (`crate::smaa`).
//
// Bindings (one layout for the three passes):
//   edge pass:   tex_a = colour
//   weight pass: tex_a = edges, tex_b = area LUT, tex_c = search LUT
//   blend pass:  tex_a = colour, tex_b = blend weights

struct SmaaInfo {
    // (1 / width, 1 / height, width, height) of the image being anti-aliased.
    rt_metrics: vec4<f32>,
}

struct VertexVaryings {
    clip_coord: vec2<f32>,
    tex_coord: vec2<f32>,
}

struct EdgeDetectionVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset_0: vec4<f32>,
    @location(1) offset_1: vec4<f32>,
    @location(2) offset_2: vec4<f32>,
    @location(3) tex_coord: vec2<f32>,
}

struct BlendingWeightCalculationVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset_0: vec4<f32>,
    @location(1) offset_1: vec4<f32>,
    @location(2) offset_2: vec4<f32>,
    @location(3) tex_coord: vec2<f32>,
}

struct NeighborhoodBlendingVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset: vec4<f32>,
    @location(1) tex_coord: vec2<f32>,
}

@group(0) @binding(0) var tex_a: texture_2d<f32>;
@group(0) @binding(1) var smaa_sampler: sampler;
@group(0) @binding(2) var<uniform> smaa_info: SmaaInfo;
@group(0) @binding(3) var tex_b: texture_2d<f32>;
@group(0) @binding(4) var tex_c: texture_2d<f32>;

//-----------------------------------------------------------------------------
// SMAA preset HIGH

const SMAA_THRESHOLD: f32 = 0.1;
const SMAA_MAX_SEARCH_STEPS: u32 = 16u;
const SMAA_MAX_SEARCH_STEPS_DIAG: u32 = 8u;
const SMAA_CORNER_ROUNDING: u32 = 25u;
const SMAA_LOCAL_CONTRAST_ADAPTATION_FACTOR: f32 = 2.0;

//-----------------------------------------------------------------------------
// Non-configurable defines

const SMAA_AREATEX_MAX_DISTANCE: f32 = 16.0;
const SMAA_AREATEX_MAX_DISTANCE_DIAG: f32 = 20.0;
const SMAA_AREATEX_PIXEL_SIZE: vec2<f32> = (1.0 / vec2<f32>(160.0, 560.0));
const SMAA_AREATEX_SUBTEX_SIZE: f32 = (1.0 / 7.0);
const SMAA_SEARCHTEX_SIZE: vec2<f32> = vec2<f32>(66.0, 33.0);
const SMAA_SEARCHTEX_PACKED_SIZE: vec2<f32> = vec2<f32>(64.0, 16.0);
const SMAA_CORNER_ROUNDING_NORM: f32 = f32(SMAA_CORNER_ROUNDING) / 100.0;

/// A fullscreen triangle from vertex indices 0..3: UV (0, 0) top-left, interpolating to
/// (1, 1) at the bottom-right of the visible clip rectangle.
fn calculate_vertex_varyings(vertex_index: u32) -> VertexVaryings {
    let uv = vec2<f32>(f32(vertex_index >> 1u), f32(vertex_index & 1u)) * 2.0;
    let clip_position = vec2<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0));
    return VertexVaryings(clip_position, uv);
}

//-----------------------------------------------------------------------------
// Vertex shaders

@vertex
fn edge_detection_vertex_main(@builtin(vertex_index) vertex_index: u32) -> EdgeDetectionVaryings {
    // Built with `let` and a constructor, not a struct `var`: naga lays out a function-local
    // struct explicitly, which Vulkan's SPIR-V rules forbid (ADR-026).
    let varyings = calculate_vertex_varyings(vertex_index);
    let offset_0 = smaa_info.rt_metrics.xyxy * vec4<f32>(-1.0, 0.0, 0.0, -1.0) + varyings.tex_coord.xyxy;
    let offset_1 = smaa_info.rt_metrics.xyxy * vec4<f32>(1.0, 0.0, 0.0, 1.0) + varyings.tex_coord.xyxy;
    let offset_2 = smaa_info.rt_metrics.xyxy * vec4<f32>(-2.0, 0.0, 0.0, -2.0) + varyings.tex_coord.xyxy;
    return EdgeDetectionVaryings(
        vec4<f32>(varyings.clip_coord, 0.0, 1.0),
        offset_0,
        offset_1,
        offset_2,
        varyings.tex_coord,
    );
}

@vertex
fn blending_weight_calculation_vertex_main(@builtin(vertex_index) vertex_index: u32)
        -> BlendingWeightCalculationVaryings {
    let varyings = calculate_vertex_varyings(vertex_index);
    // Offsets for the searches (see @PSEUDO_GATHER4).
    let offset_0 = smaa_info.rt_metrics.xyxy * vec4<f32>(-0.25, -0.125, 1.25, -0.125) + varyings.tex_coord.xyxy;
    let offset_1 = smaa_info.rt_metrics.xyxy * vec4<f32>(-0.125, -0.25, -0.125, 1.25) + varyings.tex_coord.xyxy;
    // The ends of the search loops.
    let offset_2 = smaa_info.rt_metrics.xxyy * vec4<f32>(-2.0, 2.0, -2.0, 2.0) * f32(SMAA_MAX_SEARCH_STEPS)
        + vec4<f32>(offset_0.xz, offset_1.yw);
    return BlendingWeightCalculationVaryings(
        vec4<f32>(varyings.clip_coord, 0.0, 1.0),
        offset_0,
        offset_1,
        offset_2,
        varyings.tex_coord,
    );
}

@vertex
fn neighborhood_blending_vertex_main(@builtin(vertex_index) vertex_index: u32)
        -> NeighborhoodBlendingVaryings {
    let varyings = calculate_vertex_varyings(vertex_index);
    let offset = smaa_info.rt_metrics.xyxy * vec4<f32>(1.0, 0.0, 0.0, 1.0) + varyings.tex_coord.xyxy;
    return NeighborhoodBlendingVaryings(vec4<f32>(varyings.clip_coord, 0.0, 1.0), offset, varyings.tex_coord);
}

//-----------------------------------------------------------------------------
// Edge detection (first pass)

fn luma_at(uv: vec2<f32>) -> f32 {
    return dot(textureSampleLevel(tex_a, smaa_sampler, uv, 0.0).rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

/// Luma edge detection. The input must be gamma-encoded (the display image).
@fragment
fn luma_edge_detection_fragment_main(in: EdgeDetectionVaryings) -> @location(0) vec4<f32> {
    let threshold = vec2<f32>(SMAA_THRESHOLD);

    let l = luma_at(in.tex_coord);
    let l_left = luma_at(in.offset_0.xy);
    let l_top = luma_at(in.offset_0.zw);

    var delta = vec4<f32>(abs(l - vec2<f32>(l_left, l_top)), 0.0, 0.0);
    var edges = step(threshold, delta.xy);

    // No edge: keep the cleared texel.
    if (dot(edges, vec2<f32>(1.0)) == 0.0) {
        discard;
    }

    let l_right = luma_at(in.offset_1.xy);
    let l_bottom = luma_at(in.offset_1.zw);
    delta = vec4<f32>(delta.xy, abs(l - vec2<f32>(l_right, l_bottom)));

    // The maximum delta in the direct neighbourhood.
    var max_delta = max(delta.xy, delta.zw);

    let l_left_left = luma_at(in.offset_2.xy);
    let l_top_top = luma_at(in.offset_2.zw);
    delta = vec4<f32>(delta.xy, abs(vec2<f32>(l_left, l_top) - vec2<f32>(l_left_left, l_top_top)));

    max_delta = max(max_delta.xy, delta.zw);
    let final_delta = max(max_delta.x, max_delta.y);

    // Local contrast adaptation.
    edges *= step(vec2<f32>(final_delta), SMAA_LOCAL_CONTRAST_ADAPTATION_FACTOR * delta.xy);

    return vec4<f32>(edges, 0.0, 1.0);
}

//-----------------------------------------------------------------------------
// Diagonal search

/// Decodes two binary values from a bilinear-filtered access (offset 0.25).
fn decode_diag_bilinear_access_2(in_e: vec2<f32>) -> vec2<f32> {
    var e = in_e;
    e.r = e.r * abs(5.0 * e.r - 5.0 * 0.75);
    return round(e);
}

fn decode_diag_bilinear_access_4(e: vec4<f32>) -> vec4<f32> {
    let e_rb = e.rb * abs(5.0 * e.rb - 5.0 * 0.75);
    return round(vec4<f32>(e_rb.x, e.g, e_rb.y, e.a));
}

fn search_diag_1(tex_coord: vec2<f32>, dir: vec2<f32>, e: ptr<function, vec2<f32>>) -> vec2<f32> {
    var coord = vec4<f32>(tex_coord, -1.0, 1.0);
    let t = vec3<f32>(smaa_info.rt_metrics.xy, 1.0);
    while (coord.z < f32(SMAA_MAX_SEARCH_STEPS_DIAG - 1u) && coord.w > 0.9) {
        coord = vec4<f32>(t * vec3<f32>(dir, 1.0) + coord.xyz, coord.w);
        *e = textureSampleLevel(tex_a, smaa_sampler, coord.xy, 0.0).rg;
        coord.w = dot(*e, vec2<f32>(0.5));
    }
    return coord.zw;
}

fn search_diag_2(tex_coord: vec2<f32>, dir: vec2<f32>, e: ptr<function, vec2<f32>>) -> vec2<f32> {
    var coord = vec4<f32>(tex_coord, -1.0, 1.0);
    coord.x += 0.25 * smaa_info.rt_metrics.x; // See @SearchDiag2Optimization
    let t = vec3<f32>(smaa_info.rt_metrics.xy, 1.0);
    while (coord.z < f32(SMAA_MAX_SEARCH_STEPS_DIAG - 1u) && coord.w > 0.9) {
        coord = vec4<f32>(t * vec3<f32>(dir, 1.0) + coord.xyz, coord.w);
        // Both edges at once through bilinear filtering.
        *e = textureSampleLevel(tex_a, smaa_sampler, coord.xy, 0.0).rg;
        *e = decode_diag_bilinear_access_2(*e);
        coord.w = dot(*e, vec2<f32>(0.5));
    }
    return coord.zw;
}

/// The area for a diagonal distance and crossing edges `e`.
fn area_diag(dist: vec2<f32>, e: vec2<f32>, offset: f32) -> vec2<f32> {
    var tex_coord = vec2<f32>(SMAA_AREATEX_MAX_DISTANCE_DIAG) * e + dist;
    // Scale and bias into texel space.
    tex_coord = SMAA_AREATEX_PIXEL_SIZE * tex_coord + 0.5 * SMAA_AREATEX_PIXEL_SIZE;
    // Diagonal areas are on the second half of the texture.
    tex_coord.x += 0.5;
    // The subpixel offset's sub-texture.
    tex_coord.y += SMAA_AREATEX_SUBTEX_SIZE * offset;
    return textureSampleLevel(tex_b, smaa_sampler, tex_coord, 0.0).rg;
}

/// Searches diagonal patterns and returns their weights.
fn calculate_diag_weights(tex_coord: vec2<f32>, e: vec2<f32>, subsample_indices: vec4<f32>) -> vec2<f32> {
    var weights = vec2<f32>(0.0, 0.0);

    // Search for the line ends.
    var d = vec4<f32>(0.0);
    var end = vec2<f32>(0.0);
    if (e.r > 0.0) {
        let d_xz = search_diag_1(tex_coord, vec2<f32>(-1.0, 1.0), &end);
        d = vec4<f32>(d_xz.x, d.y, d_xz.y, d.w);
        d.x += f32(end.y > 0.9);
    } else {
        d = vec4<f32>(0.0, d.y, 0.0, d.w);
    }
    let d_yw = search_diag_1(tex_coord, vec2<f32>(1.0, -1.0), &end);
    d = vec4<f32>(d.x, d_yw.x, d.z, d_yw.y);

    if (d.x + d.y > 2.0) { // d.x + d.y + 1 > 3
        // Fetch the crossing edges.
        let coords = vec4<f32>(-d.x + 0.25, d.x, d.y, -d.y - 0.25) * smaa_info.rt_metrics.xyxy + tex_coord.xyxy;
        var c = vec4<f32>(
            textureSampleLevel(tex_a, smaa_sampler, coords.xy, 0.0, vec2<i32>(-1, 0)).rg,
            textureSampleLevel(tex_a, smaa_sampler, coords.zw, 0.0, vec2<i32>(1, 0)).rg,
        );
        let c_yxwz = decode_diag_bilinear_access_4(c.xyzw);
        c = c_yxwz.yxwz;

        // Merge the crossing edges at each side into one value.
        var cc = vec2<f32>(2.0) * c.xz + c.yw;

        // Remove the crossing edge where the end of the line was not found.
        cc = select(cc, vec2<f32>(0.0, 0.0), vec2<bool>(step(vec2<f32>(0.9), d.zw)));

        weights += area_diag(d.xy, cc, subsample_indices.z);
    }

    // Search for the line ends.
    let d_xz = search_diag_2(tex_coord, vec2<f32>(-1.0, -1.0), &end);
    if (textureSampleLevel(tex_a, smaa_sampler, tex_coord, 0.0, vec2<i32>(1, 0)).r > 0.0) {
        let d_yw = search_diag_2(tex_coord, vec2<f32>(1.0, 1.0), &end);
        d = vec4<f32>(d_xz.x, d_yw.x, d_xz.y, d_yw.y);
        d.y += f32(end.y > 0.9);
    } else {
        d = vec4<f32>(d_xz.x, 0.0, d_xz.y, 0.0);
    }

    if (d.x + d.y > 2.0) { // d.x + d.y + 1 > 3
        // Fetch the crossing edges.
        let coords = vec4<f32>(-d.x, -d.x, d.y, d.y) * smaa_info.rt_metrics.xyxy + tex_coord.xyxy;
        let c = vec4<f32>(
            textureSampleLevel(tex_a, smaa_sampler, coords.xy, 0.0, vec2<i32>(-1, 0)).g,
            textureSampleLevel(tex_a, smaa_sampler, coords.xy, 0.0, vec2<i32>(0, -1)).r,
            textureSampleLevel(tex_a, smaa_sampler, coords.zw, 0.0, vec2<i32>(1, 0)).gr,
        );
        var cc = vec2<f32>(2.0) * c.xz + c.yw;

        // Remove the crossing edge where the end of the line was not found.
        cc = select(cc, vec2<f32>(0.0, 0.0), vec2<bool>(step(vec2<f32>(0.9), d.zw)));

        weights += area_diag(d.xy, cc, subsample_indices.w).gr;
    }

    return weights;
}

//-----------------------------------------------------------------------------
// Horizontal / vertical search

/// How much length to add in the last search step: 0, 1 or 2 depending on which edges and
/// crossing edges the bilinear fetch (@PSEUDO_GATHER4) saw.
fn search_length(e: vec2<f32>, offset: f32) -> f32 {
    // The texture is flipped vertically; left and right take half of it each.
    var scale = SMAA_SEARCHTEX_SIZE * vec2<f32>(0.5, -1.0);
    var bias = SMAA_SEARCHTEX_SIZE * vec2<f32>(offset, 1.0);

    // Scale and bias to texel centres.
    scale += vec2<f32>(-1.0, 1.0);
    bias += vec2<f32>(0.5, -0.5);

    // Pixel coordinates to texture coordinates (the texture is cropped to the packed size).
    scale *= 1.0 / SMAA_SEARCHTEX_PACKED_SIZE;
    bias *= 1.0 / SMAA_SEARCHTEX_PACKED_SIZE;

    return textureSampleLevel(tex_c, smaa_sampler, scale * e + bias, 0.0).r;
}

fn search_x_left(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;
    // @PSEUDO_GATHER4: the coordinate is offset by (-0.25, -0.125) to sample between edges,
    // fetching four edges in a row; the offsets per direction disambiguate them.
    var e = vec2<f32>(0.0, 1.0);
    while (tex_coord.x > end && e.g > 0.8281 && e.r == 0.0) {
        e = textureSampleLevel(tex_a, smaa_sampler, tex_coord, 0.0).rg;
        tex_coord += -vec2<f32>(2.0, 0.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e, 0.0) + 3.25;
    return smaa_info.rt_metrics.x * offset + tex_coord.x;
}

fn search_x_right(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;
    var e = vec2<f32>(0.0, 1.0);
    while (tex_coord.x < end && e.g > 0.8281 && e.r == 0.0) {
        e = textureSampleLevel(tex_a, smaa_sampler, tex_coord, 0.0).rg;
        tex_coord += vec2<f32>(2.0, 0.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e, 0.5) + 3.25;
    return -smaa_info.rt_metrics.x * offset + tex_coord.x;
}

fn search_y_up(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;
    var e = vec2<f32>(1.0, 0.0);
    while (tex_coord.y > end && e.r > 0.8281 && e.g == 0.0) {
        e = textureSampleLevel(tex_a, smaa_sampler, tex_coord, 0.0).rg;
        tex_coord += -vec2<f32>(0.0, 2.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e.gr, 0.0) + 3.25;
    return smaa_info.rt_metrics.y * offset + tex_coord.y;
}

fn search_y_down(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;
    var e = vec2<f32>(1.0, 0.0);
    while (tex_coord.y < end && e.r > 0.8281 && e.g == 0.0) {
        e = textureSampleLevel(tex_a, smaa_sampler, tex_coord, 0.0).rg;
        tex_coord += vec2<f32>(0.0, 2.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e.gr, 0.5) + 3.25;
    return -smaa_info.rt_metrics.y * offset + tex_coord.y;
}

/// The areas at each side of the current edge, from the distances and crossing edges.
fn area(dist: vec2<f32>, e1: f32, e2: f32, offset: f32) -> vec2<f32> {
    // Rounding prevents precision errors of bilinear filtering.
    var tex_coord = SMAA_AREATEX_MAX_DISTANCE * round(4.0 * vec2<f32>(e1, e2)) + dist;
    tex_coord = SMAA_AREATEX_PIXEL_SIZE * tex_coord + 0.5 * SMAA_AREATEX_PIXEL_SIZE;
    tex_coord.y += SMAA_AREATEX_SUBTEX_SIZE * offset;
    return textureSampleLevel(tex_b, smaa_sampler, tex_coord, 0.0).rg;
}

//-----------------------------------------------------------------------------
// Corner detection

fn detect_horizontal_corner_pattern(weights: vec2<f32>, tex_coord: vec4<f32>, d: vec2<f32>) -> vec2<f32> {
    let left_right = step(d.xy, d.yx);
    var rounding = (1.0 - SMAA_CORNER_ROUNDING_NORM) * left_right;
    // Less blending for pixels in the centre of a line.
    rounding /= left_right.x + left_right.y;

    var factor = vec2<f32>(1.0, 1.0);
    factor.x -= rounding.x * textureSampleLevel(tex_a, smaa_sampler, tex_coord.xy, 0.0, vec2<i32>(0, 1)).r;
    factor.x -= rounding.y * textureSampleLevel(tex_a, smaa_sampler, tex_coord.zw, 0.0, vec2<i32>(1, 1)).r;
    factor.y -= rounding.x * textureSampleLevel(tex_a, smaa_sampler, tex_coord.xy, 0.0, vec2<i32>(0, -2)).r;
    factor.y -= rounding.y * textureSampleLevel(tex_a, smaa_sampler, tex_coord.zw, 0.0, vec2<i32>(1, -2)).r;
    return weights * saturate(factor);
}

fn detect_vertical_corner_pattern(weights: vec2<f32>, tex_coord: vec4<f32>, d: vec2<f32>) -> vec2<f32> {
    let left_right = step(d.xy, d.yx);
    var rounding = (1.0 - SMAA_CORNER_ROUNDING_NORM) * left_right;
    rounding /= left_right.x + left_right.y;

    var factor = vec2<f32>(1.0, 1.0);
    factor.x -= rounding.x * textureSampleLevel(tex_a, smaa_sampler, tex_coord.xy, 0.0, vec2<i32>(1, 0)).g;
    factor.x -= rounding.y * textureSampleLevel(tex_a, smaa_sampler, tex_coord.zw, 0.0, vec2<i32>(1, 1)).g;
    factor.y -= rounding.x * textureSampleLevel(tex_a, smaa_sampler, tex_coord.xy, 0.0, vec2<i32>(-2, 0)).g;
    factor.y -= rounding.y * textureSampleLevel(tex_a, smaa_sampler, tex_coord.zw, 0.0, vec2<i32>(-2, 1)).g;
    return weights * saturate(factor);
}

//-----------------------------------------------------------------------------
// Blending weight calculation (second pass)

@fragment
fn blending_weight_calculation_fragment_main(in: BlendingWeightCalculationVaryings) -> @location(0) vec4<f32> {
    // Zero for SMAA 1× (@SUBSAMPLE_INDICES).
    let subsample_indices = vec4<f32>(0.0);

    var weights = vec4<f32>(0.0);

    let e = textureSampleLevel(tex_a, smaa_sampler, in.tex_coord, 0.0).rg;
    if (e.r == 0.0 && e.g == 0.0) {
        return weights;
    }

    if (e.g > 0.0) { // Edge at north
        // Diagonals have both north and west edges, so searching one boundary is enough.
        weights = vec4<f32>(calculate_diag_weights(in.tex_coord, e, subsample_indices), weights.ba);

        // Diagonals take priority: skip the horizontal/vertical search when one was found.
        if (weights.r + weights.g != 0.0) {
            return weights;
        }

        var d: vec2<f32>;

        // The distance to the left.
        var coords: vec3<f32>;
        coords.x = search_x_left(in.offset_0.xy, in.offset_2.x);
        // offset_1.y = tex_coord.y - 0.25 · texel (@CROSSING_OFFSET)
        coords.y = in.offset_1.y;
        d.x = coords.x;

        // The left crossing edges, two at a time through bilinear filtering; sampling at
        // -0.25 (@CROSSING_OFFSET) tells which value each edge has.
        let e1 = textureSampleLevel(tex_a, smaa_sampler, coords.xy, 0.0).r;

        // The distance to the right.
        coords.z = search_x_right(in.offset_0.zw, in.offset_2.y);
        d.y = coords.z;

        // Distances in pixels.
        d = abs(round(smaa_info.rt_metrics.zz * d - in.position.xx));

        // The area texture is compressed quadratically.
        let sqrt_d = sqrt(d);

        // The right crossing edges.
        let e2 = textureSampleLevel(tex_a, smaa_sampler, coords.zy, 0.0, vec2<i32>(1, 0)).r;

        weights = vec4<f32>(area(sqrt_d, e1, e2, subsample_indices.y), weights.ba);

        // Fix corners.
        coords.y = in.tex_coord.y;
        weights = vec4<f32>(detect_horizontal_corner_pattern(weights.rg, coords.xyzy, d), weights.ba);
    }

    if (e.r > 0.0) { // Edge at west
        var d: vec2<f32>;

        // The distance to the top.
        var coords: vec3<f32>;
        coords.y = search_y_up(in.offset_1.xy, in.offset_2.z);
        // offset_0.x = tex_coord.x - 0.25 · texel
        coords.x = in.offset_0.x;
        d.x = coords.y;

        // The top crossing edges.
        let e1 = textureSampleLevel(tex_a, smaa_sampler, coords.xy, 0.0).g;

        // The distance to the bottom.
        coords.z = search_y_down(in.offset_1.zw, in.offset_2.w);
        d.y = coords.z;

        // Distances in pixels.
        d = abs(round(smaa_info.rt_metrics.ww * d - in.position.yy));

        let sqrt_d = sqrt(d);

        // The bottom crossing edges.
        let e2 = textureSampleLevel(tex_a, smaa_sampler, coords.xz, 0.0, vec2<i32>(0, 1)).g;

        weights = vec4<f32>(weights.rg, area(sqrt_d, e1, e2, subsample_indices.x));

        // Fix corners.
        coords.x = in.tex_coord.x;
        weights = vec4<f32>(weights.rg, detect_vertical_corner_pattern(weights.ba, coords.xyxz, d));
    }

    return weights;
}

//-----------------------------------------------------------------------------
// Neighbourhood blending (third pass)

@fragment
fn neighborhood_blending_fragment_main(in: NeighborhoodBlendingVaryings) -> @location(0) vec4<f32> {
    // The blending weights of this pixel.
    let a = vec4<f32>(
        textureSampleLevel(tex_b, smaa_sampler, in.offset.xy, 0.0).a, // Right
        textureSampleLevel(tex_b, smaa_sampler, in.offset.zw, 0.0).g, // Top
        textureSampleLevel(tex_b, smaa_sampler, in.tex_coord, 0.0).zx, // Bottom / Left
    );

    if (dot(a, vec4<f32>(1.0)) < 1.0e-5) {
        return textureSampleLevel(tex_a, smaa_sampler, in.tex_coord, 0.0);
    }
    let h = max(a.x, a.z) > max(a.y, a.w); // max(horizontal) > max(vertical)

    // The blending offsets.
    var blending_offset = vec4<f32>(0.0, a.y, 0.0, a.w);
    var blending_weight = a.yw;
    blending_offset = select(blending_offset, vec4<f32>(a.x, 0.0, a.z, 0.0), h);
    blending_weight = select(blending_weight, a.xz, h);
    blending_weight /= dot(blending_weight, vec2<f32>(1.0));

    let blending_coord = blending_offset * vec4<f32>(smaa_info.rt_metrics.xy, -smaa_info.rt_metrics.xy)
        + in.tex_coord.xyxy;

    // Bilinear filtering mixes this pixel with the chosen neighbour.
    var color = blending_weight.x * textureSampleLevel(tex_a, smaa_sampler, blending_coord.xy, 0.0);
    color += blending_weight.y * textureSampleLevel(tex_a, smaa_sampler, blending_coord.zw, 0.0);
    return color;
}
