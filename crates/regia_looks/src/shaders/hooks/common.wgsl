// SPDX-License-Identifier: MPL-2.0
// regia_looks shared hook helpers (ADR-2538), prepended to every looks hook. Written against the
// regia_viewer prelude (Surface, material, frame, env_radiance, env_diffuse, dfg, lut_sample).
// Block layout of every looks material: base, p0, p1, p2 = regia_looks::MaterialParams p0..p3.

const LOOKS_PI: f32 = 3.14159265;
const LOOKS_TAU: f32 = 6.28318531;

// Low-bias integer hash: stable across frames and backends.
fn looks_hash01(x: u32) -> f32 {
    var h = x;
    h = h ^ (h >> 16u);
    h = h * 0x7feb352du;
    h = h ^ (h >> 15u);
    h = h * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return f32(h & 0xffffu) / 65535.0;
}

// The studio at an explicit fraction of the mip chain (0 sharp, 1 the last mip).
fn looks_env(dir: vec3<f32>, mip_fraction: f32) -> vec3<f32> {
    return textureSampleLevel(env_cube, env_sampler, dir, clamp(mip_fraction, 0.0, 1.0) * frame.env.x).rgb;
}

fn looks_ggx(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, roughness: f32) -> f32 {
    let a = max(roughness * roughness, 0.002);
    let h = normalize(v + l);
    let n_h = max(dot(n, h), 0.0);
    let n_l = max(dot(n, l), 0.0);
    let n_v = max(dot(n, v), 1e-4);
    let a2 = a * a;
    let d = n_h * n_h * (a2 - 1.0) + 1.0;
    let dist = a2 / (LOOKS_PI * d * d);
    let vis = 0.5 / (n_l * (n_v * (1.0 - a) + a) + n_v * (n_l * (1.0 - a) + a) + 1e-5);
    return dist * vis * n_l;
}

fn looks_key_dir(k: i32) -> vec3<f32> {
    return normalize(frame.key_dir[k].xyz);
}
