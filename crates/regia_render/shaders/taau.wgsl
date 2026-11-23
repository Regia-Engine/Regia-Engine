// SPDX-License-Identifier: MPL-2.0
// Temporal upscaler (TAAU).
//
// Jittered low-resolution colour is reprojected with motion vectors and clipped
// to the current neighbourhood before blending. Karis, "High Quality Temporal
// Supersampling", SIGGRAPH 2014 (YCoCg neighbourhood, closest-depth motion). The
// upscaling schedule — render below the output, accumulate into a history at output
// resolution — follows Yang, Liu & Salvi, "A Survey of Temporal Antialiasing
// Techniques", JCGT 2020, § 4.3 (temporal upsampling).
//
// Motion-look lane (every earlier judgement was on stills):
// - The history is fetched with Keys' cubic convolution filter at a = −1
//   (`HISTORY_CUBIC_A`; Keys, IEEE TASSP 1981; a = −0.5 would be Catmull–Rom) in five
//   bilinear taps (Sigg & Hadwiger, GPU Gems 2 ch. 20, the corner taps dropped).
//   A bilinear fetch at a fractional offset is a small blur, and a moving camera
//   paid it again every frame: a 2.4 px/frame pan kept 20 % of a 2.5 px line pair.
// - The motion vector is the one of the closest surface in the 3×3 neighbourhood
//   (Karis 2014), so a moving edge carries its own motion onto the pixels around it
//   instead of the background's.
// - The history is clipped toward the neighbourhood box's centre along the line to it
//   (Pedersen, "Temporal Reprojection Anti-Aliasing in INSIDE", GDC 2016) inside a
//   box of mean ± 1.25 σ (Salvi, "An Excursion in Temporal Supersampling", GDC 2016)
//   intersected with the min/max box. A per-channel min/max clamp kept a moving box's
//   colour wherever the background's own range contained it: 18 % ghost trails.
// - The history is trusted less where the motion stored for it last frame differs
//   from the motion it is fetched with (velocity weighting, Jimenez et al., "SMAA:
//   Enhanced Subpixel Morphological Antialiasing", Eurographics 2012, § 6). A box that
//   slid off a static wall left its colour in the history where the wall's range
//   contains it (same albedo); the wall's motion (0) is not the box's (3 px).
// - v0.1.4 (white dots in motion on RADV): the current and history are blended with
//   Karis's inverse-luminance weights 1 / (1 + L) (Karis 2014), so a lone sample far
//   above its history (the forward pass clamps a sun glint at 6e4) cannot print as a
//   white dot the frame the history is rejected; and non-finite values are removed with a
//   bit test on the exponent. `max(x, 0)` (SPIR-V FMax) is undefined for NaN and RADV/ACO
//   kept it, so one NaN texel stayed in the history for good.
// - v0.1.28 (temporal-quality lane, ADR-1151; measured with the CPU copy of this resolve in
//   `taau_quality.rs`): when upscaling, the velocity test reads the 2 × 2 render texels around
//   the history fetch, not the nearest one.

struct TaauParams {
    // xy = 1 / output size, zw = 1 / render size.
    inv_size: vec4<f32>,
    // x = history weight (≤ 0.98, `MAX_HISTORY_WEIGHT`), y = 1 when history is valid, z = 1 when the scene depth is
    // bound (closest-depth motion; 0 uses the pixel's own vector), w = 1 when the
    // previous frame's motion is bound.
    blend: vec4<f32>,
    // x = history scale: the HDR pre-exposure changed by this factor since the history was
    // written (0 reads as 1). y = 1 when the reactive mask is bound. zw unused.
    extra: vec4<f32>,
}

@group(0) @binding(0) var current_tex: texture_2d<f32>;
@group(0) @binding(1) var history_tex: texture_2d<f32>;
@group(0) @binding(2) var velocity_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: TaauParams;
@group(0) @binding(5) var depth_tex: texture_depth_2d;
@group(0) @binding(6) var prev_velocity_tex: texture_2d<f32>;
// How much to trust this frame over the history (red, 0..=1, at the render size): what the frame's
// translucent composite covered (ADR-2065). A translucent surface writes no motion and its light
// sits over the moving background, so the history cannot follow it.
@group(0) @binding(7) var reactive_tex: texture_2d<f32>;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return VertexOut(
        vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0),
        vec2<f32>(uv.x, 1.0 - uv.y),
    );
}

// YCoCg (Malvar & Sullivan, JVT-I014, 2003): Co = (R − B) / 2. The inverse below
// adds Co once to R and subtracts it once from B, so the forward transform must
// halve it — without the halving every resolve doubled the red–blue difference of
// the history and the colour drifted (orange/blue) as frames accumulated.
fn ycocg(rgb: vec3<f32>) -> vec3<f32> {
    let y = dot(rgb, vec3<f32>(0.25, 0.5, 0.25));
    return vec3<f32>(y, (rgb.r - rgb.b) * 0.5, rgb.g - y);
}

fn rgb_from_ycocg(c: vec3<f32>) -> vec3<f32> {
    let tmp = c.x - c.z;
    return vec3<f32>(tmp + c.y, c.x + c.z, tmp - c.y);
}

// Zero for a NaN or infinite channel, by the exponent bits (no float compare or FMax, whose
// NaN behaviour SPIR-V leaves to the driver).
fn finite_or_zero(c: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(c) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(c, vec3<f32>(0.0), bad);
}

// Largest value the resolve writes: below the half-float maximum (65504) of the history.
const MAX_HDR: f32 = 6.0e4;

fn sample_current(uv: vec2<f32>) -> vec3<f32> {
    return clamp(finite_or_zero(textureSampleLevel(current_tex, samp, uv, 0.0).rgb), vec3<f32>(0.0), vec3<f32>(MAX_HDR));
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn output(c: vec3<f32>) -> vec4<f32> {
    return vec4<f32>(clamp(finite_or_zero(c), vec3<f32>(0.0), vec3<f32>(MAX_HDR)), 1.0);
}

// Keys' cubic convolution parameter of the history fetch (Keys, "Cubic Convolution
// Interpolation for Digital Image Processing", IEEE TASSP 1981). −0.5 is Catmull–Rom.
const HISTORY_CUBIC_A: f32 = -1.0;

// Cubic history fetch in five bilinear taps. Negative lobes can undershoot
// next to a bright edge, so the result is clamped at zero (the clip below bounds it).
fn sample_history(uv: vec2<f32>) -> vec3<f32> {
    let size = vec2<f32>(textureDimensions(history_tex));
    let pos = uv * size;
    let p1 = floor(pos - 0.5) + 0.5;
    let f = pos - p1;
    let a = HISTORY_CUBIC_A;
    let g = vec2<f32>(1.0) - f;
    let w0 = a * f * g * g;
    let w1 = ((a + 2.0) * f - (a + 3.0)) * f * f + 1.0;
    let w2 = ((a + 2.0) * g - (a + 3.0)) * g * g + 1.0;
    let w3 = a * g * f * f;
    let w12 = w1 + w2;
    let p0 = (p1 - 1.0) / size;
    let p3 = (p1 + 2.0) / size;
    let p12 = (p1 + w2 / w12) / size;
    var sum = textureSampleLevel(history_tex, samp, vec2<f32>(p12.x, p0.y), 0.0).rgb * (w12.x * w0.y);
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(p0.x, p12.y), 0.0).rgb * (w0.x * w12.y);
    sum += textureSampleLevel(history_tex, samp, p12, 0.0).rgb * (w12.x * w12.y);
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(p3.x, p12.y), 0.0).rgb * (w3.x * w12.y);
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(p12.x, p3.y), 0.0).rgb * (w12.x * w3.y);
    let weight = w12.x * w0.y + w0.x * w12.y + w12.x * w12.y + w3.x * w12.y + w12.x * w3.y;
    return max(sum / weight, vec3<f32>(0.0));
}

// The motion of the closest surface in the 3×3 render-resolution neighbourhood.
fn closest_motion(uv: vec2<f32>) -> vec2<f32> {
    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let vbase = clamp(vec2<i32>(uv * vec2<f32>(vsize)), vec2<i32>(0), vsize - 1);
    if (params.blend.z < 0.5) {
        return textureLoad(velocity_tex, vbase, 0).xy;
    }
    let dsize = vec2<i32>(textureDimensions(depth_tex));
    let base = clamp(vec2<i32>(uv * vec2<f32>(dsize)), vec2<i32>(0), dsize - 1);
    var best = base;
    var best_depth = 2.0;
    for (var y: i32 = -1; y <= 1; y = y + 1) {
        for (var x: i32 = -1; x <= 1; x = x + 1) {
            let c = clamp(base + vec2<i32>(x, y), vec2<i32>(0), dsize - 1);
            let d = textureLoad(depth_tex, c, 0);
            if (d < best_depth) {
                best_depth = d;
                best = c;
            }
        }
    }
    // Depth and velocity share the render resolution; clamp in case they do not.
    return textureLoad(velocity_tex, clamp(best, vec2<i32>(0), vsize - 1), 0).xy;
}

// Smallest half-width of the clip box on any channel, as a share of the box's top luma (Y).
// The history is a half float (11 significant bits): its YCoCg channels carry rounding of a
// few 1e-4 of Y. A channel whose neighbourhood range is narrower than that (the Cg of a warm
// grey wall, r : g : b = 1 : 0.9 : 0.8, is 0 exactly) read that rounding as the history lying
// 100 box widths outside, and the one-scale clip pulled the whole colour, luma too, toward the
// box centre: wave peaks lost 0.1 in ten frames at 0.67 scale (v0.1.29 gate, GPU against the
// f32 model of `taau_quality.rs`). 1/256 is about eight half-float steps.
const CLIP_EXTENT_FLOOR: f32 = 1.0 / 256.0;

// Clips `history` toward the centre of the box `[lo, hi]` onto its surface (Pedersen
// 2016): one scale for all three channels, so the clipped colour stays on the line
// between the history and the neighbourhood instead of taking a new hue.
fn clip_to_box(history: vec3<f32>, lo: vec3<f32>, hi: vec3<f32>) -> vec3<f32> {
    let floor_extent = max(CLIP_EXTENT_FLOOR * hi.x, 1.0e-6);
    let extent = max((hi - lo) * 0.5, vec3<f32>(floor_extent));
    let mid = (hi + lo) * 0.5;
    let offset = history - mid;
    let units = abs(offset / extent);
    let scale = max(units.x, max(units.y, units.z));
    if (scale > 1.0) {
        return mid + offset / scale;
    }
    return history;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let texel = params.inv_size.zw;
    var min_c = vec3<f32>(1.0e+6);
    var max_c = vec3<f32>(-1.0e+6);
    var m1 = vec3<f32>(0.0);
    var m2 = vec3<f32>(0.0);
    for (var y: i32 = -1; y <= 1; y = y + 1) {
        for (var x: i32 = -1; x <= 1; x = x + 1) {
            let s = sample_current(uv + vec2<f32>(f32(x), f32(y)) * texel);
            let ycc = ycocg(s);
            min_c = min(min_c, ycc);
            max_c = max(max_c, ycc);
            m1 += ycc;
            m2 += ycc * ycc;
        }
    }
    let current = sample_current(uv);
    if (params.blend.y < 0.5) {
        return output(current);
    }
    let mean = m1 / 9.0;
    let sigma = sqrt(max(m2 / 9.0 - mean * mean, vec3<f32>(0.0)));
    // The σ box always holds this pixel's own sample: on a 1 px line pair the centre sits
    // 1.4 σ from the mean, and a box without it pulled every dark line toward grey.
    let centre = ycocg(current);
    let lo = max(min_c, min(mean - 1.25 * sigma, centre));
    let hi = min(max_c, max(mean + 1.25 * sigma, centre));
    let motion = closest_motion(uv);
    let history_uv = uv + motion;
    // History from outside the last frame does not exist: take the current colour.
    if (any(history_uv < vec2<f32>(0.0)) || any(history_uv > vec2<f32>(1.0))) {
        return output(current);
    }
    let history = finite_or_zero(sample_history(history_uv) * select(1.0, params.extra.x, params.extra.x > 0.0));
    let clipped = rgb_from_ycocg(clip_to_box(ycocg(history), lo, hi));
    var weight = clamp(params.blend.x, 0.0, 0.98);
    // A fetch between texels (motion with a fractional part) low-passes the history a
    // little every frame and the losses compound at a 0.9 feedback: trust the history
    // less the further the fetch lands from a texel centre (Yang et al. 2020 § 5,
    // velocity-adaptive blending). 4 f (1 − f) is 0 on the grid and 1 halfway between.
    let hsize = vec2<f32>(textureDimensions(history_tex));
    let f = fract(history_uv * hsize - 0.5);
    let between = 4.0 * f * (1.0 - f);
    weight = weight * (1.0 - 0.35 * max(between.x, between.y));
    if (params.blend.w > 0.5) {
        // Motion in output pixels; full trust within ½ px of agreement, none past 2 px.
        let out_size = 1.0 / params.inv_size.xy;
        let psize = vec2<i32>(textureDimensions(prev_velocity_tex));
        var disagree = 0.0;
        if (any(params.inv_size.zw > params.inv_size.xy * 1.0001)) {
            // Upscaling (ADR-1151): the output-resolution history pixel mixes what the 2 × 2
            // render texels around the fetch held, and the nearest one alone can miss a moving
            // object that still tinted it. A box inside the wall's colour range (which the clip
            // keeps) then left 10-15 % of its colour on the uncovered wall for four frames and
            // more at 0.67 scale. Take the largest disagreement of the four.
            let base = vec2<i32>(floor(history_uv * vec2<f32>(psize) - 0.5));
            for (var y: i32 = 0; y <= 1; y = y + 1) {
                for (var x: i32 = 0; x <= 1; x = x + 1) {
                    let pc = clamp(base + vec2<i32>(x, y), vec2<i32>(0), psize - 1);
                    let prev_motion = textureLoad(prev_velocity_tex, pc, 0).xy;
                    disagree = max(disagree, length((prev_motion - motion) * out_size));
                }
            }
        } else {
            let pc = clamp(vec2<i32>(history_uv * vec2<f32>(psize)), vec2<i32>(0), psize - 1);
            let prev_motion = textureLoad(prev_velocity_tex, pc, 0).xy;
            disagree = length((prev_motion - motion) * out_size);
        }
        weight *= clamp(1.0 - (disagree - 0.5) / 1.5, 0.0, 1.0);
    }
    if (params.extra.y > 0.5) {
        weight *= 1.0 - clamp(textureSampleLevel(reactive_tex, samp, uv, 0.0).r, 0.0, 1.0);
    }
    // Karis 2014: weigh each side by 1 / (1 + luminance) before the blend.
    let wc = (1.0 - weight) / (1.0 + luma(current));
    let wh = weight / (1.0 + luma(max(clipped, vec3<f32>(0.0))));
    return output((current * wc + clipped * wh) / max(wc + wh, 1.0e-6));
}
