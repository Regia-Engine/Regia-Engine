// SPDX-License-Identifier: MPL-2.0
// Edge-avoiding a-trous wavelet filter, one iteration (ADR-2433). The GPU twin of
// `iterate_cpu` in `src/denoise_atrous.rs`; the two must stay in step.
//
// Dammertz, Sewtz, Hanika and Lensch, "Edge-Avoiding A-Trous Wavelet Transform for Fast Global
// Illumination Filtering" (HPG 2010): the B3-spline kernel applied with holes (tap spacing
// `step`), each tap weighted by edge-stopping functions. Schied et al., "Spatiotemporal
// Variance-Guided Filtering" (HPG 2017), spatial part only: the luminance tolerance scales with
// the standard deviation of the 3x3-filtered variance, and the variance is propagated as
// sum(w^2 var) / sum(w)^2.

struct Params {
    size: vec2<u32>,
    step: u32,
    pad: u32,
    // x luminance sigma, y normal exponent, z depth sigma.
    sigmas: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
// rgb demodulated signal, w variance of its luminance.
@group(0) @binding(1) var<storage, read> src: array<vec4<f32>>;
// xyz first-hit normal, w depth (0 = no surface).
@group(0) @binding(2) var<storage, read> guide: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> dst: array<vec4<f32>>;

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// B3 spline: 1/16, 1/4, 3/8, 1/4, 1/16.
fn kernel(i: i32) -> f32 {
    let a = abs(i);
    if (a == 0) {
        return 0.375;
    }
    if (a == 1) {
        return 0.25;
    }
    return 0.0625;
}

fn index(x: i32, y: i32) -> u32 {
    let w = i32(params.size.x);
    let h = i32(params.size.y);
    return u32(clamp(y, 0, h - 1) * w + clamp(x, 0, w - 1));
}

@compute @workgroup_size(8, 8, 1)
fn atrous(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.size.x || gid.y >= params.size.y) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let w = i32(params.size.x);
    let h = i32(params.size.y);
    let step = i32(params.step);
    let p = index(x, y);
    let c = src[p];
    let g = guide[p];
    let lp = luminance(c.rgb);

    var vg = 0.0;
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            var kx = 1.0;
            if (dx == 0) {
                kx = 2.0;
            }
            var ky = 1.0;
            if (dy == 0) {
                ky = 2.0;
            }
            vg = vg + kx * ky / 16.0 * src[index(x + dx, y + dy)].w;
        }
    }
    let sigma_l = params.sigmas.x * sqrt(max(vg, 0.0)) + 1e-4;
    let zx = 0.5 * (guide[index(x + 1, y)].w - guide[index(x - 1, y)].w);
    let zy = 0.5 * (guide[index(x, y + 1)].w - guide[index(x, y - 1)].w);
    let z_eps = 1e-3 * max(g.w, 1.0);

    var acc = vec3<f32>(0.0);
    var variance = 0.0;
    var wsum = 0.0;
    for (var dy = -2; dy <= 2; dy = dy + 1) {
        for (var dx = -2; dx <= 2; dx = dx + 1) {
            let qx = x + dx * step;
            let qy = y + dy * step;
            if (qx < 0 || qy < 0 || qx >= w || qy >= h) {
                continue;
            }
            let q = u32(qy * w + qx);
            let cq = src[q];
            let gq = guide[q];
            let wn = pow(max(dot(g.xyz, gq.xyz), 0.0), params.sigmas.y);
            let plane = abs(zx * f32(dx * step) + zy * f32(dy * step));
            let wz = exp(-abs(g.w - gq.w) / (params.sigmas.z * plane + z_eps));
            let lq = luminance(cq.rgb);
            let wl = exp(-abs(lp - lq) / sigma_l);
            let wt = kernel(dx) * kernel(dy) * wn * wz * wl;
            acc = acc + wt * cq.rgb;
            variance = variance + wt * wt * cq.w;
            wsum = wsum + wt;
        }
    }
    if (wsum > 0.0) {
        dst[p] = vec4<f32>(acc / wsum, variance / (wsum * wsum));
    } else {
        dst[p] = c;
    }
}
