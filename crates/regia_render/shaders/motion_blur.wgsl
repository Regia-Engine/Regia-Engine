// SPDX-License-Identifier: MIT OR Apache-2.0
// Per-object, per-pixel motion blur along Regia Engine's velocity target.
//
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_post_process/src/motion_blur/motion_blur.wesl, translated from WESL to naga_oil WGSL.
// Copyright (c) Bevy contributors, dual-licensed MIT OR Apache-2.0; used here under the MIT
// licence (see docs/ported-sources.toml). The per-pixel jitter is interleaved gradient noise
// (Jimenez, "Next Generation Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014).
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
// Differences from Bevy:
// - Regia Engine's depth is standard Z (near 0, far 1; the sky is 1), Bevy's is reversed (sky 0). The
//   foreground test and the "no depth" case are rewritten for it.
// - The velocity target holds `previous uv - current uv` (Bevy: current - previous). The blur is
//   symmetric about the pixel and the occlusion weight uses |cos|, so the sign does not matter.
// - The velocity and depth targets may be smaller than the colour (a temporally upscaled frame):
//   both are addressed by uv, not by the colour's texel.
// - Single-sample only (Regia Engine's post chain never runs on an MSAA target).
// - High velocity (ADR-1152): the blur is capped at `max_blur` of the frame height (a teleported
//   object, a hitch frame or a spinning prop otherwise sampled pixels across the frame, and an
//   object faster than the frame vanished: every tap off screen or on unrelated pixels), and a
//   long blur takes more samples, up to `MAX_SAMPLES` per direction, so taps stay at most
//   `SAMPLE_SPACING` pixels apart (a few taps over 100 px drew a row of ghost copies).
// - Non-finite velocity reads as no motion, and non-finite taps are skipped: one broken texel
//   no longer spreads along its trail (the exposure pass would then print the trail black).

struct MotionBlurParams {
    /// Fraction of the frame the shutter is open (0.5 = 180°).
    shutter_angle: f32,
    /// Samples per direction.
    samples: u32,
    /// Frame index, decorrelates the noise between frames.
    frame: u32,
    /// Longest blur (the whole exposure vector), as a fraction of the frame height.
    max_blur: f32,
}

/// Most samples per direction a long blur takes (`MotionBlur::MAX_SAMPLES`).
const MAX_SAMPLES: i32 = 16;
/// Widest gap, pixels, between the taps of a long blur before it takes more samples
/// (`MotionBlur::SAMPLE_SPACING_PX`).
const SAMPLE_SPACING: f32 = 4.0;

@group(0) @binding(0) var screen_texture: texture_2d<f32>;
@group(0) @binding(1) var motion_vectors: texture_2d<f32>;
@group(0) @binding(2) var depth: texture_depth_2d;
@group(0) @binding(3) var texture_sampler: sampler;
@group(0) @binding(4) var<uniform> settings: MotionBlurParams;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> FullOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return FullOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0));
}

fn interleaved_gradient_noise(pixel: vec2<f32>, frame: u32) -> f32 {
    let xy = pixel + 5.588238 * f32(frame % 64u);
    return fract(52.9829189 * fract(0.06711056 * xy.x + 0.00583715 * xy.y));
}

/// `true` unless a component is NaN or infinite (bit test: the shader may be compiled assuming
/// finite maths, which folds `x != x`).
fn finite2(v: vec2<f32>) -> bool {
    return all((bitcast<vec2<u32>>(v) << vec2<u32>(1u)) < vec2<u32>(0xff000000u));
}

fn finite4(v: vec4<f32>) -> bool {
    return all((bitcast<vec4<u32>>(v) << vec4<u32>(1u)) < vec4<u32>(0xff000000u));
}

/// A motion vector, zero when not finite.
fn motion_at(uv: vec2<f32>) -> vec2<f32> {
    let m = textureSampleLevel(motion_vectors, texture_sampler, uv, 0.0).rg;
    return select(vec2<f32>(0.0), m, finite2(m));
}

/// `m` (a motion vector, uv) scaled down so its blur, `shutter_angle × m` in pixels of a `size`
/// frame, is at most `max_blur` of the frame height. Both the fragment's and each sample's motion
/// go through it, so the occlusion test below compares like with like.
fn capped(m: vec2<f32>, size: vec2<f32>) -> vec2<f32> {
    let blur_px = length(settings.shutter_angle * m * size);
    let max_px = settings.max_blur * size.y;
    return select(m, m * (max_px / blur_px), blur_px > max_px);
}

fn depth_at(uv: vec2<f32>) -> f32 {
    let dims = vec2<f32>(textureDimensions(depth));
    let texel = clamp(vec2<i32>(uv * dims), vec2<i32>(0), vec2<i32>(dims) - vec2<i32>(1));
    return textureLoad(depth, texel, 0);
}

@fragment
fn fs_main(in: FullOut) -> @location(0) vec4<f32> {
    let texture_size = vec2<f32>(textureDimensions(screen_texture));
    let frag_coords = vec2<i32>(in.pos.xy);
    let uv = in.pos.xy / texture_size;

    // The pixel's own texel, loaded: a bilinear fetch at the centre gives the same value but can
    // pick up a neighbour's NaN through its zero weight.
    let base_color = textureLoad(screen_texture, frag_coords, 0);
    let shutter_angle = settings.shutter_angle;
    let this_motion_vector = motion_at(uv);
    let this_depth = depth_at(uv);

    // The exposure vector is the distance that this fragment moved while the camera shutter was
    // open: the motion vector (total distance travelled in a frame) times the shutter angle (a
    // fraction of the frame). Values above 1 are non-physical but allowed for artistic reasons.
    // Its length is capped at `max_blur` of the frame height, in pixels.
    let exposure_vector = shutter_angle * capped(this_motion_vector, texture_size);
    let exposure_px = length(exposure_vector * texture_size);

    var accumulator = vec4<f32>(0.0);
    var weight_total = 0.0;
    // At least the asked-for samples, and enough that 2n taps over the blur stay
    // `SAMPLE_SPACING` apart, up to `MAX_SAMPLES`. At that cap the gap is
    // `height / 256` pixels when the blur is `max_blur` of the frame height.
    let wanted = i32(ceil(exposure_px / (2.0 * SAMPLE_SPACING)));
    let n_samples = clamp(wanted, i32(settings.samples), max(i32(settings.samples), MAX_SAMPLES));
    let noise = interleaved_gradient_noise(vec2<f32>(frag_coords), settings.frame); // 0 to 1

    for (var i = -n_samples; i < n_samples; i++) {
        // The current sample step vector, from uv.
        let step_vector = 0.5 * exposure_vector * (f32(i) + noise) / f32(n_samples);
        let sample_uv = uv + step_vector;

        // If the sample is off screen, skip it.
        if sample_uv.x < 0.0 || sample_uv.x > 1.0 || sample_uv.y < 0.0 || sample_uv.y > 1.0 {
            continue;
        }

        let sample_color = textureSampleLevel(screen_texture, texture_sampler, sample_uv, 0.0);
        if !finite4(sample_color) {
            continue;
        }
        let sample_motion = capped(motion_at(sample_uv), texture_size);
        let sample_depth = depth_at(sample_uv);

        var weight = 1.0;
        // Standard Z: the sky (depth 1) has no surface and counts as background, so skies and
        // other depth-less pixels are sampled freely.
        let is_sample_in_fg = sample_depth <= this_depth;
        if sample_depth < 1.0 && is_sample_in_fg {
            // Velocity-based rejection of background bleeding (a stationary foreground object
            // sampled while blurring a moving background, e.g. a tracked car against a blurred
            // road): is it possible that this sample was occluding the fragment? Project the
            // sample's motion onto the fragment's; if the sample did not cover enough distance
            // to reach the fragment it could not have influenced it, so it is discarded.
            let frag_speed = length(step_vector);
            let sample_speed = length(sample_motion) / 2.0; // Halved because the sample is centred.
            let cos_angle = dot(step_vector, sample_motion) / max(frag_speed * sample_speed * 2.0, 1.0e-12);
            let motion_similarity = clamp(abs(cos_angle), 0.0, 1.0);
            if sample_speed * motion_similarity < frag_speed {
                weight = 0.0;
            }
        }
        weight_total += weight;
        accumulator += weight * sample_color;
    }

    let has_moved_less_than_a_pixel =
        dot(this_motion_vector * texture_size, this_motion_vector * texture_size) < 1.0;
    // In case no samples were accepted, fall back to the base colour. Also fall back when the
    // motion is below a pixel, so antialiasing is not broken.
    if weight_total <= 0.0 || has_moved_less_than_a_pixel {
        accumulator = base_color;
        weight_total = 1.0;
    }
    return accumulator / weight_total;
}
