// SPDX-License-Identifier: MPL-2.0
// Motion-adaptive sharpen after the temporal resolve (display only).
//
// A moving view resamples its history every frame. Even a cubic fetch is a low-pass at a
// fractional offset (zero response at Nyquist halfway between texels), and the losses
// compound through the feedback: a 2.5 px/frame pan kept 0.76 of the 2 px line pairs and
// 0.27 of the 1 px ones (motion-look lane, round 2). This pass restores part of it with
// an unsharp mask (a 4-neighbour Laplacian, Gonzalez & Woods, "Digital Image Processing",
// § 3.6) whose strength follows the pixel's motion per axis: zero at rest, so held and
// converged frames are unchanged, full from `params.amount.y` output pixels per frame. The history
// keeps the unsharpened resolve: sharpening it would feed back and ring.
//
// Deringing: the result stays within the five taps' range widened by a quarter of it on
// each side (the clamp idea of AMD's FidelityFX CAS, public GPUOpen documentation), so a
// hard edge overshoots by at most a quarter of its own (log) contrast.
//
// v0.1.4 (white dots against a black sky): that margin is relative to the log range, and a
// black neighbour sits at the log floor, so a dim edge pixel next to black had a range of
// ~20 stops and was brightened up to ×84; after AgX and bloom (which reads this image) every
// moving silhouette against the sky drew a white outline. The margin and the brightening are
// now capped in absolute terms (`MAX_GAIN_LOG2`: at most +25 %), and the floor is 1e-4
// (pre-exposed units; darker reads as black after the tonemap) instead of 1e-8.

struct SharpenParams {
    // x = strength at full motion, y = motion (output px / frame) that reaches it.
    amount: vec4<f32>,
}

// Largest brightening of a pixel and largest overshoot past the taps' range: log2(1.25).
const MAX_GAIN_LOG2: f32 = 0.32;
// Log-luminance floor (pre-exposed HDR units).
const LOG_FLOOR: f32 = 1.0e-4;

@group(0) @binding(0) var resolved: texture_2d<f32>;
@group(0) @binding(1) var velocity_tex: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: SharpenParams;

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

fn tap(p: vec2<i32>, size: vec2<i32>) -> vec3<f32> {
    return textureLoad(resolved, clamp(p, vec2<i32>(0), size - 1), 0).rgb;
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(resolved));
    let p = vec2<i32>(position.xy);
    let c = tap(p, size);
    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let vp = clamp(vec2<i32>((vec2<f32>(p) + 0.5) * vec2<f32>(vsize) / vec2<f32>(size)), vec2<i32>(0), vsize - 1);
    // Motion is stored in UV units per frame. A fetch between texels blurs along the axis it
    // is fractional on only, so each axis is sharpened by its own motion: a horizontal pan
    // leaves horizontal lines (sharp in the history) alone.
    let motion_px = abs(textureLoad(velocity_tex, vp, 0).xy * vec2<f32>(size));
    let s = params.amount.x * clamp(motion_px / max(params.amount.y, 1e-3), vec2<f32>(0.0), vec2<f32>(1.0));
    if (max(s.x, s.y) <= 0.0) {
        return vec4<f32>(c, 1.0);
    }
    // In log luminance: the pre-exposure HDR spans decades, and a linear unsharp mask's
    // undershoot on the dark side of an edge is magnified by the tonemap's toe (the 4 px
    // line pairs came out 1.14× native, their edge energy 1.6×). The luminance ratio is
    // applied to the colour, so the sharpen cannot fringe.
    let w = vec3<f32>(0.2126, 0.7152, 0.0722);
    let lc = log2(max(dot(c, w), LOG_FLOOR));
    let ll = log2(max(dot(tap(p + vec2<i32>(-1, 0), size), w), LOG_FLOOR));
    let lr = log2(max(dot(tap(p + vec2<i32>(1, 0), size), w), LOG_FLOOR));
    let lu = log2(max(dot(tap(p + vec2<i32>(0, -1), size), w), LOG_FLOOR));
    let ld = log2(max(dot(tap(p + vec2<i32>(0, 1), size), w), LOG_FLOOR));
    let lo = min(lc, min(min(ll, lr), min(lu, ld)));
    let hi = max(lc, max(max(ll, lr), max(lu, ld)));
    let margin = min((hi - lo) * 0.25, MAX_GAIN_LOG2);
    // Per axis half the 1-D Laplacian: with equal strengths this is `s (c − mean of four)`.
    let lap = s.x * (lc - (ll + lr) * 0.5) + s.y * (lc - (lu + ld) * 0.5);
    let sharp = clamp(lc + 0.5 * lap, lo - margin, min(hi + margin, lc + MAX_GAIN_LOG2));
    return vec4<f32>(c * exp2(sharp - lc), 1.0);
}
