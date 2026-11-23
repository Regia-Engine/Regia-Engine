// SPDX-License-Identifier: MPL-2.0
// Weighted blended order-independent transparency: the composite (`crate::oit`, ADR-2065;
// McGuire and Bavoil, "Weighted Blended Order-Independent Transparency", JCGT 2(2), 2013).
//
// The `OIT` forward variant added each fragment's premultiplied light and coverage, weighted, to
// the accumulation target, and multiplied the revealage target by what it lets through. This pass
// lays the weighted average of the light over the image in proportion to what the layers covered
// (the blend: source alpha, one minus source alpha; the image's alpha keeps the reflection code)
// and writes the reactive mask the temporal upscalers read.

// `crate::oit::REACTIVE_SCALE`.
const REACTIVE_SCALE: f32 = 0.9;
// Largest value written: below the half-float maximum of the image (65504).
const MAX_HDR: f32 = 6.0e4;

@group(0) @binding(0) var accum_tex: texture_2d<f32>;
@group(0) @binding(1) var revealage_tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

// A channel that overflowed the half float reads as the largest value kept, by the exponent bits
// (`min` on a NaN or infinity is up to the driver).
fn bounded(c: vec4<f32>) -> vec4<f32> {
    let bad = (bitcast<vec4<u32>>(c) << vec4<u32>(1u)) >= vec4<u32>(0xff000000u);
    return min(select(c, vec4<f32>(MAX_HDR), bad), vec4<f32>(MAX_HDR));
}

struct CompositeOut {
    @location(0) color: vec4<f32>,
    @location(1) reactive: vec4<f32>,
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> CompositeOut {
    let pixel = vec2<i32>(position.xy);
    let revealage = clamp(textureLoad(revealage_tex, pixel, 0).r, 0.0, 1.0);
    let accum = bounded(textureLoad(accum_tex, pixel, 0));
    if (accum.a <= 0.0 && revealage >= 1.0) {
        discard;
    }
    let covered = 1.0 - revealage;
    let average = min(accum.rgb / max(accum.a, 1.0e-7), vec3<f32>(MAX_HDR));
    return CompositeOut(
        vec4<f32>(average, covered),
        vec4<f32>(covered * REACTIVE_SCALE, 0.0, 0.0, 1.0),
    );
}
