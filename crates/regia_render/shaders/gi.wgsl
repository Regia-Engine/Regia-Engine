// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::gi

// L1 irradiance and Chebyshev helpers shared with the CPU (`src/gi.rs`).
// Majercik et al., JCGT 2019, reduced to L1 SH plus a 4×4 octahedral depth.

const Y00: f32 = 0.2820948;
const Y1: f32 = 0.48860252;
const PI: f32 = 3.14159265;

fn irradiance_over_pi(sh: vec4<f32>, n: vec3<f32>) -> f32 {
    let a0 = PI;
    let a1 = 2.0 * PI / 3.0;
    let e = a0 * sh.x * Y00 + a1 * Y1 * dot(sh.yzw, n);
    return max(e / PI, 0.0);
}

fn chebyshev(mean: f32, mean_sq: f32, dist: f32) -> f32 {
    if (dist <= mean + 0.05) {
        return 1.0;
    }
    // Floor matches `CHEBYSHEV_MIN_VARIANCE` in `gi.rs`. A one-ray 4×4 cell
    // has zero variance; without the floor the compare is a step and a wall
    // a few centimetres past that ray is fully black.
    let variance = max(mean_sq - mean * mean, 0.008);
    let d = dist - mean;
    return clamp(variance / (variance + d * d), 0.0, 1.0);
}

fn sgn(v: f32) -> f32 {
    return select(-1.0, 1.0, v >= 0.0);
}

fn oct_uv(dir: vec3<f32>) -> vec2<f32> {
    let len = length(dir);
    var n = dir;
    if (len > 1e-4) {
        n = dir / len;
    } else {
        n = vec3<f32>(0.0, 1.0, 0.0);
    }
    let l1 = abs(n.x) + abs(n.y) + abs(n.z);
    var u = n.x / l1;
    var v = n.y / l1;
    if (n.z < 0.0) {
        let ou = u;
        u = (1.0 - abs(v)) * sgn(ou);
        v = (1.0 - abs(ou)) * sgn(v);
    }
    let su = clamp(u * 0.5 + 0.5, 0.0, 0.999);
    let sv = clamp(v * 0.5 + 0.5, 0.0, 0.999);
    return vec2<f32>(su, sv);
}

fn oct_cell(dir: vec3<f32>) -> u32 {
    let uv = oct_uv(dir);
    let x = u32(uv.x * 4.0);
    let y = u32(uv.y * 4.0);
    return y * 4u + x;
}
