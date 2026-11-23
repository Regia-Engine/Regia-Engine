// SPDX-License-Identifier: MPL-2.0
// Display stage (ADR-092): player calibration and HDR output encoding on the finished frame
// (scene + VFX + UI), 1:1 from the frame's intermediate into the swapchain.
// CPU mirror: `regia_render::calibration` (black_level_curve, DisplayCalibration::hdr_nits).
// sRGB transfer: IEC 61966-2-1 (piecewise, the exact inverse of the hardware sRGB decode).
// PQ: SMPTE ST 2084. Rec.709 → Rec.2020: ITU-R BT.2087.

struct DisplayParams {
    // x: black-level coefficient a, y: gamma exponent (g / 2.2),
    // z: input kind (0 display-encoded, 1 linear), w: output kind
    // (0 SDR encoded, 1 SDR linear for an sRGB target, 2 HDR10 PQ, 3 scRGB).
    sdr: vec4<f32>,
    // x: paper white nits, y: min nits, z: peak nits.
    hdr: vec4<f32>,
    // HDR test pattern rectangle in pixels (x0, y0, x1, y1).
    pattern_rect: vec4<f32>,
    // x: 1 = draw the pattern, y: background nits, z: logo nits, w: logo radius in pixels.
    pattern: vec4<f32>,
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: DisplayParams;

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

fn encode_srgb_exact(l: vec3<f32>) -> vec3<f32> {
    let c = max(l, vec3<f32>(0.0));
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

fn decode_srgb_exact(e: vec3<f32>) -> vec3<f32> {
    let c = max(e, vec3<f32>(0.0));
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// v' = ½(x + a·x·(1 − x)²) with x = 2v below mid grey (v' = v + a·v·(1 − 2v)²).
// Identity at and above 0.5. The slope at black is 1 + a.
fn black_curve(v: vec3<f32>, a: f32) -> vec3<f32> {
    if (a == 0.0) {
        return v;
    }
    let x = max(2.0 * v, vec3<f32>(0.0));
    let om = vec3<f32>(1.0) - x;
    let curved = 0.5 * (x + a * x * om * om);
    return select(v, curved, v < vec3<f32>(0.5));
}

fn pq(nits: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let y = clamp(nits / 10000.0, vec3<f32>(0.0), vec3<f32>(1.0));
    let ym = pow(y, vec3<f32>(m1));
    return pow((vec3<f32>(c1) + c2 * ym) / (vec3<f32>(1.0) + c3 * ym), vec3<f32>(m2));
}

fn rec709_to_rec2020(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(c, vec3<f32>(0.627404, 0.329283, 0.043313)),
        dot(c, vec3<f32>(0.069097, 0.919540, 0.011362)),
        dot(c, vec3<f32>(0.016391, 0.088013, 0.895595)),
    );
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(src_tex, vec2<i32>(pos.xy), 0).rgb;
    let out_kind = u32(p.sdr.w + 0.5);
    if (out_kind <= 1u) {
        var v = c;
        if (p.sdr.z > 0.5) {
            v = encode_srgb_exact(c);
        }
        v = clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
        if (p.sdr.y != 1.0) {
            v = pow(v, vec3<f32>(p.sdr.y));
        }
        v = black_curve(v, p.sdr.x);
        if (out_kind == 1u) {
            return vec4<f32>(decode_srgb_exact(v), 1.0);
        }
        return vec4<f32>(v, 1.0);
    }

    // HDR: linear input in paper-white units.
    var l = max(c, vec3<f32>(0.0));
    if (p.sdr.x != 0.0) {
        let curved = decode_srgb_exact(black_curve(encode_srgb_exact(l), p.sdr.x));
        l = select(l, curved, l < vec3<f32>(0.21404114));
    }
    let paper = p.hdr.x;
    let min_nits = p.hdr.y;
    let peak = p.hdr.z;
    var nits = min(l * paper, vec3<f32>(peak));
    nits = vec3<f32>(min_nits) + nits * (1.0 - min_nits / peak);
    if (p.pattern.x > 0.5 && all(pos.xy >= p.pattern_rect.xy) && all(pos.xy < p.pattern_rect.zw)) {
        // Peak test: a ring at the calibrated peak on a background brighter than any display.
        let centre = 0.5 * (p.pattern_rect.xy + p.pattern_rect.zw);
        let d = distance(pos.xy, centre);
        let r = p.pattern.w;
        let on_logo = d < r && d > 0.55 * r;
        nits = vec3<f32>(select(p.pattern.y, p.pattern.z, on_logo));
    }
    if (out_kind == 2u) {
        return vec4<f32>(pq(rec709_to_rec2020(nits)), 1.0);
    }
    return vec4<f32>(nits / 80.0, 1.0);
}
