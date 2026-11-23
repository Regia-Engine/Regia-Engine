// SPDX-License-Identifier: MIT OR Apache-2.0
// Depth of field, with both Gaussian and hexagonal bokeh kernels.
//
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_post_process/src/dof/dof.wesl and crates/bevy_post_process/src/gaussian_blur.wesl,
// translated from WESL to naga_oil WGSL. Copyright (c) Bevy contributors, dual-licensed
// MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// Gaussian blur is a separable convolution: first in X, then in Y.
//
// The bokeh blur is a separable convolution too, after Colin Barré-Brisebois, "Hexagonal Bokeh
// Blur Revisited" (https://colinbarrebrisebois.com/2017/04/18/hexagonal-bokeh-blur-revisited-part-2-improved-2-pass-version/):
// separable convolutions produce parallelograms as well as boxes, so three in sequence produce a
// hexagon. The first and second are done together with two render targets, so the whole blur is
// two passes.
//
// The circle of confusion is the thin-lens formula (Wikipedia, "Circle of confusion", "Determining
// a circle of confusion diameter from the object field").
//
// Differences from Bevy:
// - Regia Engine's depth is standard Z from a D3D-style perspective (near 0, far 1); the view depth is
//   linearised from near / far, not from Bevy's view uniform.
// - The depth target may be smaller than the colour (a temporally upscaled frame): it is
//   addressed by uv.
// - One bind group layout for the blur passes: the single-input passes bind the source as both
//   colour textures. Single-sample only. The first blur pass writes each pixel's view depth into
//   its alpha, so the second pass fetches colour and depth together.
// - Scatter-as-gather (Regia Engine addition), after Jimenez, "Next Generation Post Processing in Call
//   of Duty: Advanced Warfare" (SIGGRAPH 2014): every tap is weighted by its *own* kernel (its
//   circle of confusion), and it counts only where that kernel reaches the pixel being gathered.
//   Bevy weights every tap by the gathering pixel's kernel, so a blurred background gathers the
//   colour of a sharp object in front of it (a halo around everything in focus: a tracked car
//   against a blurred road).
// - The gather radius of a pixel is the largest circle of confusion near it, not its own: a
//   first pass writes the largest circle per 16 px tile, dilated by one tile (the tile-max idea
//   of McGuire et al., "A Reconstruction Filter for Plausible Motion Blur", I3D 2012, used for
//   depth of field in the same Jimenez talk). With the own-kernel weights, a blurred foreground
//   object then spreads over the sharp background behind its silhouette instead of ending in a
//   hard cut-out edge. A tap behind the gathering pixel spreads no wider than that pixel's own
//   circle (the usual depth-ordering rule of scatter-as-gather: the pixel occludes it), so a
//   blurred background does not wash over a sharp object either.
// - Edge cases (ADR-1152): the tile max is dilated, in its own pass over the tile grid, by as many
//   tiles as the largest radius spans (`tile_reach`; one tile cut a circle of confusion above
//   32 px off before its full radius), the
//   largest circle is capped on the CPU (`DepthOfField::MAX_CIRCLE_OF_CONFUSION_DIAMETER`: the
//   loops are as long as the radius), a non-finite colour tap is skipped (one broken texel no
//   longer spreads over a whole circle), and an infinite far plane is read from `near / far`.

struct DepthOfFieldParams {
    /// The distance in metres to the location in focus.
    focal_distance: f32,
    /// The focal length, metres (from the sensor height and the vertical field of view).
    focal_length: f32,
    /// `focal_length² / (sensor_height * aperture_f_stops)`.
    coc_scale_factor: f32,
    /// The largest circle of confusion diameter allowed, pixels.
    max_circle_of_confusion_diameter: f32,
    /// Objects are never farther than this as far as depth of field is concerned.
    max_depth: f32,
    /// Near plane, metres.
    near: f32,
    /// Near plane over far plane (0 for an infinite far plane).
    near_over_far: f32,
    /// Tiles the tile max is dilated by: the largest radius, in tiles, rounded up.
    tile_reach: f32,
}

/// The first bokeh pass outputs to two render targets.
struct DualOutput {
    /// The vertical output.
    @location(0) output_0: vec4<f32>,
    /// The diagonal output.
    @location(1) output_1: vec4<f32>,
}

/// Output pixels per circle-of-confusion tile edge.
const TILE: i32 = 16;
/// Largest depth, metres, the first blur pass carries in an f16 alpha.
const MAX_CARRIED_DEPTH: f32 = 60000.0;

@group(0) @binding(0) var<uniform> dof_params: DepthOfFieldParams;
@group(0) @binding(1) var color_texture_sampler: sampler;
@group(0) @binding(2) var depth_texture: texture_depth_2d;
/// The main colour texture.
@group(0) @binding(3) var color_texture_a: texture_2d<f32>;
/// The auxiliary colour texture (second bokeh pass only; otherwise the main texture again).
@group(0) @binding(4) var color_texture_b: texture_2d<f32>;
/// The dilated largest circle of confusion per tile (blur passes; not bound by the tile pass).
#ifdef DOF_BLUR
@group(0) @binding(5) var coc_tiles: texture_2d<f32>;
#endif

/// Used to ensure `depth * (focus - f)` is always a positive number.
const EPSILON: f32 = 1.19209290e-07;
/// cos(-30°), used for the bokeh blur.
const COS_NEG_FRAC_PI_6: f32 = 0.8660254037844387;
/// sin(-30°), used for the bokeh blur.
const SIN_NEG_FRAC_PI_6: f32 = -0.5;
/// cos(-150°), used for the bokeh blur.
const COS_NEG_FRAC_PI_5_6: f32 = -0.8660254037844387;
/// sin(-150°), used for the bokeh blur.
const SIN_NEG_FRAC_PI_5_6: f32 = -0.5;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> FullOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return FullOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0));
}

/// View depth, metres, of a standard-Z depth sample from a D3D-style perspective:
/// `n f / (f − raw (f − n))`, divided through by `f` so an infinite far plane is `n / (1 − raw)`.
fn linear_depth(raw: f32) -> f32 {
    let n = dof_params.near;
    return n / max(1.0 - raw * (1.0 - dof_params.near_over_far), EPSILON);
}

/// `true` unless a component is NaN or infinite (bit test: the shader may be compiled assuming
/// finite maths, which folds `x != x`).
fn finite3(v: vec3<f32>) -> bool {
    return all((bitcast<vec3<u32>>(v) << vec3<u32>(1u)) < vec3<u32>(0xff000000u));
}

/// The circle of confusion diameter, pixels, of the surface at `uv`.
fn coc_at(uv: vec2<f32>) -> f32 {
    return depth_coc_at(uv).y;
}

/// The view depth (metres, capped at `max_depth`) and circle of confusion diameter (pixels) of the
/// surface at `uv`.
fn depth_coc_at(uv: vec2<f32>) -> vec2<f32> {
    let depth_dims = vec2<f32>(textureDimensions(depth_texture));
    let texel = clamp(vec2<i32>(uv * depth_dims), vec2<i32>(0), vec2<i32>(depth_dims) - vec2<i32>(1));
    let raw_depth = textureLoad(depth_texture, texel, 0);
    // Capped below the f16 range: the first blur pass carries the depth in its alpha.
    let depth = min(min(linear_depth(raw_depth), dof_params.max_depth), MAX_CARRIED_DEPTH);
    return vec2<f32>(depth, coc_of(depth));
}

/// The circle of confusion diameter, pixels, of a surface `depth` metres away.
fn coc_of(depth: f32) -> f32 {
    let focus = dof_params.focal_distance;
    let f = dof_params.focal_length;
    let scale = dof_params.coc_scale_factor;
    let framebuffer_size = vec2<f32>(textureDimensions(color_texture_a));
    // The thin-lens circle of confusion as a fraction of the image height, then in pixels.
    let candidate_coc = scale * abs(depth - focus) / (depth * max(focus - f, EPSILON));
    return clamp(candidate_coc * framebuffer_size.y, 0.0, dof_params.max_circle_of_confusion_diameter);
}

/// A tap at `texel` of `tex`: its colour, and the view depth of its surface in `w`. The second
/// blur pass reads the depth from the alpha the first pass wrote (one fetch, not two).
fn tap_at(tex: texture_2d<f32>, texel: vec2<i32>, alpha_depth: bool) -> vec4<f32> {
    let c = textureLoad(tex, texel, 0);
    if alpha_depth {
        return c;
    }
    return vec4<f32>(c.rgb, depth_coc_at((vec2<f32>(texel) + 0.5) / vec2<f32>(textureDimensions(tex))).x);
}

/// Depth and circle of confusion of a tap from [`tap_at`].
fn tap_dc(t: vec4<f32>) -> vec2<f32> {
    return vec2<f32>(t.w, coc_of(t.w));
}

/// The circle of confusion a tap scatters with onto the pixel at `here` (its depth and circle):
/// its own, except that a tap behind the pixel spreads no wider than the pixel's own circle (the
/// pixel occludes what is behind it, so a blurred background cannot spread over a sharp object).
fn scatter_coc(tap: vec2<f32>, here: vec2<f32>) -> f32 {
    return select(tap.y, min(tap.y, here.y), tap.x > here.x);
}

/// The largest circle of confusion in this tile (every second pixel; a feature thinner than 2 px
/// may be missed, and then only blurs over its own radius). `coc_tile_dilate` then spreads it.
@fragment
fn coc_tile_max(in: FullOut) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(color_texture_a));
    let origin = vec2<i32>(in.pos.xy) * TILE;
    var largest = 0.0;
    for (var y = 0; y < TILE; y += 2) {
        for (var x = 0; x < TILE; x += 2) {
            let px = vec2<f32>(origin + vec2<i32>(x, y)) + 0.5;
            if all(px < size) {
                largest = max(largest, coc_at(px / size));
            }
        }
    }
    return vec4<f32>(largest, 0.0, 0.0, 1.0);
}

#ifdef DOF_BLUR
/// The largest tile circle within `tile_reach` tiles of this one (`coc_tiles` here holds the
/// per-tile maxima of `coc_tile_max`). A tap `k` tiles away can be as close as `16 (k − 1) + 1` px,
/// so the reach is the largest radius in tiles, rounded up. A separate pass over the tile grid:
/// (2 reach + 1)² tile loads per tile instead of 64 depth loads for each of those tiles.
@fragment
fn coc_tile_dilate(in: FullOut) -> @location(0) vec4<f32> {
    let tiles = vec2<i32>(textureDimensions(coc_tiles));
    let tile = vec2<i32>(in.pos.xy);
    let reach = i32(dof_params.tile_reach);
    var largest = 0.0;
    for (var ty = -reach; ty <= reach; ty += 1) {
        for (var tx = -reach; tx <= reach; tx += 1) {
            let t = tile + vec2<i32>(tx, ty);
            if all(t >= vec2<i32>(0)) && all(t < tiles) {
                largest = max(largest, textureLoad(coc_tiles, t, 0).r);
            }
        }
    }
    return vec4<f32>(largest, 0.0, 0.0, 1.0);
}

/// The gather radius of the pixel at `frag_coord`: half the largest circle of confusion near it.
fn gather_radius(frag_coord: vec4<f32>) -> f32 {
    let tiles = vec2<i32>(textureDimensions(coc_tiles));
    let tile = clamp(vec2<i32>(frag_coord.xy) / TILE, vec2<i32>(0), tiles - vec2<i32>(1));
    return textureLoad(coc_tiles, tile, 0).r * 0.5;
}

/// One direction of the separable Gaussian kernel over `color_texture_a`, gathered with every
/// tap's own kernel. `frag_offset` is the screen-space step between taps; `alpha_depth` reads the
/// depths from the source's alpha (the second pass). Returns the colour, with the pixel's depth
/// in alpha.
fn gaussian_blur(frag_coord: vec4<f32>, frag_offset: vec2<f32>, alpha_depth: bool) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(color_texture_a));
    let pixel = vec2<i32>(frag_coord.xy);
    let step = vec2<i32>(frag_offset);
    // Usually σ is half the radius, and the radius is half the CoC; 1.5σ is the support. The
    // largest kernel that can reach this pixel bounds the taps.
    let support = i32(ceil(gather_radius(frag_coord) * 0.75));
    let here = tap_dc(tap_at(color_texture_a, pixel, alpha_depth));

    var sum = vec3(0.0);
    var weight_sum = 0.0;
    for (var i = -support; i <= support; i += 1) {
        let texel = clamp(pixel + step * i, vec2<i32>(0), size - vec2<i32>(1));
        let t = tap_at(color_texture_a, texel, alpha_depth);
        let d = abs(f32(i));
        // The tap's own Gaussian (normalised by its σ, so a wide kernel spreads its energy thin),
        // counted only within its 1.5σ support: an in-focus tap reaches only itself. The
        // normalisation floors σ at ½ px so an in-focus tap is a finite peak.
        let sigma = scatter_coc(tap_dc(t), here) * 0.25;
        if finite3(t.rgb) && d <= floor(sigma * 1.5 + 0.5) {
            let s = max(sigma, 0.5);
            let w = exp(-d * d / (2.0 * s * s)) / s;
            sum += t.rgb * w;
            weight_sum += w;
        }
    }
    return vec4(sum / max(weight_sum, EPSILON), here.x);
}

/// A one-sided box blur over `tex` in the direction `frag_offset` (a ray of the hexagon), gathered
/// with every tap's own kernel: a tap counts where its own radius reaches this pixel, weighted
/// by the inverse of that radius. Taps load the texel whose depth they test: the rays are
/// diagonal, and a bilinear fetch between texels would mix an occluding neighbour's colour into an
/// accepted tap (a fringe). Returns the colour, with the pixel's depth in alpha.
fn box_blur(tex: texture_2d<f32>, frag_coord: vec4<f32>, frag_offset: vec2<f32>, alpha_depth: bool) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(tex));
    let uv = frag_coord.xy / size;
    let offset = frag_offset / size;
    let support = i32(round(gather_radius(frag_coord)));
    let here = tap_dc(tap_at(tex, vec2<i32>(frag_coord.xy), alpha_depth));

    var sum = vec3(0.0);
    var weight_sum = 0.0;
    for (var i = 0; i <= support; i += 1) {
        let tap = uv + offset * f32(i);
        let texel = clamp(vec2<i32>(tap * size), vec2<i32>(0), vec2<i32>(size) - vec2<i32>(1));
        let t = tap_at(tex, texel, alpha_depth);
        let radius = floor(scatter_coc(tap_dc(t), here) * 0.5 + 0.5);
        if finite3(t.rgb) && f32(i) <= radius {
            let w = 1.0 / (1.0 + radius);
            sum += t.rgb * w;
            weight_sum += w;
        }
    }
    return vec4(sum / max(weight_sum, EPSILON), here.x);
}

/// The horizontal half of the separable Gaussian blur.
@fragment
fn gaussian_horizontal(in: FullOut) -> @location(0) vec4<f32> {
    return gaussian_blur(in.pos, vec2(1.0, 0.0), false);
}

/// The vertical half of the separable Gaussian blur.
@fragment
fn gaussian_vertical(in: FullOut) -> @location(0) vec4<f32> {
    return vec4(gaussian_blur(in.pos, vec2(0.0, 1.0), true).rgb, 1.0);
}

/// The vertical and first diagonal components of the hexagonal bokeh blur.
///
///         ╱
///        ╱
///       •
///       │
///       │
@fragment
fn bokeh_pass_a(in: FullOut) -> DualOutput {
    let vertical = box_blur(color_texture_a, in.pos, vec2(0.0, 1.0), false);
    let diagonal = box_blur(color_texture_a, in.pos, vec2(COS_NEG_FRAC_PI_6, SIN_NEG_FRAC_PI_6), false);

    // The diagonal part is pre-mixed with the vertical component. Built in the return, not in a
    // local `var`: naga 30 emits a struct local with member offsets, which SPIR-V 1.6 rejects.
    return DualOutput(vertical, mix(vertical, diagonal, 0.5));
}

/// The second diagonal components of the hexagonal bokeh blur.
///
///     ╲   ╱
///      ╲ ╱
///       •
@fragment
fn bokeh_pass_b(in: FullOut) -> @location(0) vec4<f32> {
    let output_0 = box_blur(color_texture_a, in.pos, vec2(COS_NEG_FRAC_PI_6, SIN_NEG_FRAC_PI_6), true);
    let output_1 = box_blur(color_texture_b, in.pos, vec2(COS_NEG_FRAC_PI_5_6, SIN_NEG_FRAC_PI_5_6), true);
    return vec4(mix(output_0, output_1, 0.5).rgb, 1.0);
}
#endif
