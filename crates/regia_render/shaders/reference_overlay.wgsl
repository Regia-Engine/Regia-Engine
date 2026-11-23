// SPDX-License-Identifier: MPL-2.0
// Reference overlay (ADR-2463): draws a reference image over the display image for look reviews.
// One full-view triangle; the render pass viewport is the view rectangle. Modes (u.params.x):
//   1 blend       reference mixed over the frame by opacity (alpha blending does the mix)
//   2 wipe        reference where the pixel is right of the split, the frame untouched left of it
//   3 difference  |frame - reference| per channel, mixed with the frame by opacity
//   4 flip        a perceptual colour-difference map (HyAB distance in CIELAB through a heat ramp)
// Difference and flip read a copy of the frame (`frame`); the others never touch it.

struct Uniforms {
    // View rectangle in target pixels: x, y, width, height.
    view: vec4<f32>,
    // The rectangle the fitted reference occupies, in view pixels: x, y, width, height.
    rect: vec4<f32>,
    // x: mode, y: opacity, z: split (0..1 of the view width), w: 1 when the target is not sRGB.
    params: vec4<f32>,
    // x: 1 when the reference maps 1:1 to view pixels (read with textureLoad), y/z: its size.
    exact: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var reference: texture_2d<f32>;
@group(0) @binding(2) var reference_sampler: sampler;
@group(0) @binding(3) var frame: texture_2d<f32>;

struct VsOut {
    @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let t = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return VsOut(vec4<f32>(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0));
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn lab_f(t: f32) -> f32 {
    let d = 6.0 / 29.0;
    return select(pow(t, 1.0 / 3.0), t / (3.0 * d * d) + 4.0 / 29.0, t <= d * d * d);
}

// Linear Rec.709 RGB to CIELAB (D65).
fn to_lab(rgb: vec3<f32>) -> vec3<f32> {
    let x = (0.4124564 * rgb.x + 0.3575761 * rgb.y + 0.1804375 * rgb.z) / 0.95047;
    let y = 0.2126729 * rgb.x + 0.7151522 * rgb.y + 0.0721750 * rgb.z;
    let z = (0.0193339 * rgb.x + 0.1191920 * rgb.y + 0.9503041 * rgb.z) / 1.08883;
    let fx = lab_f(x);
    let fy = lab_f(y);
    let fz = lab_f(z);
    return vec3<f32>(116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz));
}

// Error 0..1 to black, blue, magenta, orange, yellow-white.
fn heat(e: f32) -> vec3<f32> {
    let t = clamp(e, 0.0, 1.0) * 4.0;
    let c0 = vec3<f32>(0.0, 0.0, 0.0);
    let c1 = vec3<f32>(0.1, 0.1, 0.6);
    let c2 = vec3<f32>(0.7, 0.1, 0.5);
    let c3 = vec3<f32>(1.0, 0.5, 0.1);
    let c4 = vec3<f32>(1.0, 1.0, 0.8);
    if (t < 1.0) { return mix(c0, c1, t); }
    if (t < 2.0) { return mix(c1, c2, t - 1.0); }
    if (t < 3.0) { return mix(c2, c3, t - 2.0); }
    return mix(c3, c4, t - 3.0);
}

fn linear_if_encoded(c: vec3<f32>) -> vec3<f32> {
    return select(c, srgb_to_linear(c), u.params.w > 0.5);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.position.xy - u.view.xy;
    let local = p - u.rect.xy;
    if (local.x < 0.0 || local.y < 0.0 || local.x >= u.rect.z || local.y >= u.rect.w) {
        discard;
    }
    let mode = u32(u.params.x + 0.5);
    if (mode == 2u && p.x < u.params.z * u.view.z) {
        discard;
    }
    var r: vec3<f32>;
    if (u.exact.x > 0.5) {
        r = textureLoad(reference, vec2<i32>(local), 0).rgb;
    } else {
        r = textureSampleLevel(reference, reference_sampler, local / u.rect.zw, 0.0).rgb;
    }
    let opacity = clamp(u.params.y, 0.0, 1.0);
    if (mode == 1u) {
        return vec4<f32>(r, opacity);
    }
    if (mode == 2u) {
        return vec4<f32>(r, 1.0);
    }
    let f = textureLoad(frame, vec2<i32>(in.position.xy), 0).rgb;
    if (mode == 3u) {
        return vec4<f32>(abs(f - r), opacity);
    }
    let lf = to_lab(linear_if_encoded(f));
    let lr = to_lab(linear_if_encoded(r));
    let dl = abs(lf.x - lr.x);
    let dab = length(lf.yz - lr.yz);
    // HyAB; 100 is the full lightness range.
    let e = (dl + dab) / 100.0;
    return vec4<f32>(heat(e), opacity);
}
