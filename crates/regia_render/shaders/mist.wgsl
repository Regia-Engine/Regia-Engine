// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::mist

// Valley mist for the froxel fog (`volumetric_fog.wgsl`): extra extinction whose density is
// relative to the terrain under the sample, pooled in hollows and broken up by drifting value
// noise. Mirrors `mist.rs` (`ValleyMist::density_at`, `sun_transmittance`) line for line; the
// CPU copy is what the unit tests pin. Everything is a pure function of the sample position,
// the grid and the mist time.
//
// Rows, as `ValleyMist::gpu_rows`:
//   m0 = (density, scale height m, max height m, open-ground fraction)
//   m1 = (hollow depth m, noise scale m, noise strength, time s)
//   m2 = (wind x, wind z, sun steps, quality: 0 off, 1 reduced, 2 full)
//   m3 = (grid origin x, grid origin z, 1 / cell m (0: no grid), flat ground y)
// `grid` holds (height m, concavity m) per texel in `.rg`, read with `textureLoad`.

fn mist_lattice(c: vec3<i32>) -> f32 {
    var h = (bitcast<u32>(c.x) * 0x8da6b343u)
        ^ (bitcast<u32>(c.y) * 0xd8163841u)
        ^ (bitcast<u32>(c.z) * 0xcb1ab31fu);
    h = h ^ (h >> 15u);
    h = h * 0x2c1b3c6du;
    h = h ^ (h >> 12u);
    h = h * 0x297a2d39u;
    h = h ^ (h >> 15u);
    return f32(h >> 8u) / 16777216.0;
}

fn mist_noise(q: vec3<f32>) -> f32 {
    let f = floor(q);
    let t = q - f;
    let s = t * t * (vec3<f32>(3.0) - 2.0 * t);
    let i = vec3<i32>(f);
    let x00 = mix(mist_lattice(i), mist_lattice(i + vec3<i32>(1, 0, 0)), s.x);
    let x10 = mix(mist_lattice(i + vec3<i32>(0, 1, 0)), mist_lattice(i + vec3<i32>(1, 1, 0)), s.x);
    let x01 = mix(mist_lattice(i + vec3<i32>(0, 0, 1)), mist_lattice(i + vec3<i32>(1, 0, 1)), s.x);
    let x11 = mix(mist_lattice(i + vec3<i32>(0, 1, 1)), mist_lattice(i + vec3<i32>(1, 1, 1)), s.x);
    return mix(mix(x00, x10, s.y), mix(x01, x11, s.y), s.z);
}

fn mist_fbm(q: vec3<f32>, octaves: i32, shift: f32) -> f32 {
    var sum = 0.0;
    var total = 0.0;
    var freq = 1.0;
    for (var k = 0; k < 3; k++) {
        if (k >= octaves) {
            break;
        }
        let weight = select(select(0.14, 0.29, k == 1), 0.57, k == 0);
        let slide = shift * (1.0 + 0.35 * f32(k));
        sum += weight * mist_noise(vec3<f32>(q.x * freq + slide, q.y * freq, q.z * freq - slide));
        total += weight;
        freq *= 2.0;
    }
    return sum / total;
}

// (height, concavity) of the ground under world (x, z): bilinear, border-clamped; flat at
// `m3.w` with no grid.
fn mist_ground(grid: texture_2d<f32>, m3: vec4<f32>, x: f32, z: f32) -> vec2<f32> {
    if (m3.z <= 0.0) {
        return vec2<f32>(m3.w, 0.0);
    }
    let dims = vec2<i32>(textureDimensions(grid));
    let hi = vec2<f32>(dims - vec2<i32>(1));
    let uv = clamp(vec2<f32>(x - m3.x, z - m3.y) * m3.z, vec2<f32>(0.0), hi);
    let i0 = vec2<i32>(floor(uv));
    let i1 = min(i0 + vec2<i32>(1), dims - vec2<i32>(1));
    let t = uv - floor(uv);
    let a = textureLoad(grid, i0, 0).rg;
    let b = textureLoad(grid, vec2<i32>(i1.x, i0.y), 0).rg;
    let c = textureLoad(grid, vec2<i32>(i0.x, i1.y), 0).rg;
    let d = textureLoad(grid, i1, 0).rg;
    return mix(mix(a, b, t.x), mix(c, d, t.x), t.y);
}

fn mist_density(
    grid: texture_2d<f32>,
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    m3: vec4<f32>,
    p: vec3<f32>,
) -> f32 {
    if (m2.w < 0.5) {
        return 0.0;
    }
    let g = mist_ground(grid, m3, p.x, p.z);
    let above = max(p.y - g.x, 0.0);
    let hollow = smoothstep(0.0, max(m1.x, 1e-3), g.y);
    let open = clamp(m0.w, 0.0, 1.0);
    let pool = open + (1.0 - open) * hollow;
    let depth_scale = 0.5 + 0.5 * hollow;
    let top = max(m0.z * depth_scale, 1e-3);
    let profile = exp(-above / max(m0.y * depth_scale, 1e-3)) * (1.0 - smoothstep(0.6 * top, top, above));
    var noise = 1.0;
    if (m1.z > 0.0) {
        let scale = max(m1.y, 1.0);
        let q = vec3<f32>(
            (p.x - m2.x * m1.w) / scale,
            p.y / (scale * 0.5),
            (p.z - m2.y * m1.w) / scale,
        );
        let octaves = select(1, 3, m2.w > 1.5);
        let n = mist_fbm(q, octaves, m1.w / scale);
        noise = max((1.0 - m1.z) + m1.z * 2.0 * n, 0.0);
    }
    return max(m0.x * pool * profile * noise, 0.0);
}

// Fraction of the sun that gets through the mist from `p` toward the sun (`to_sun` unit).
// 1 below the full tier or with no steps.
fn mist_sun_transmittance(
    grid: texture_2d<f32>,
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    m3: vec4<f32>,
    p: vec3<f32>,
    to_sun: vec3<f32>,
) -> f32 {
    let steps = i32(m2.z);
    if (m2.w < 1.5 || steps <= 0) {
        return 1.0;
    }
    let g = mist_ground(grid, m3, p.x, p.z);
    let reach = clamp((m0.z - max(p.y - g.x, 0.0)) / max(to_sun.y, 0.05), 8.0, 800.0);
    let dt = reach / f32(steps);
    var optical = 0.0;
    for (var k = 0; k < 8; k++) {
        if (k >= steps) {
            break;
        }
        let q = p + to_sun * ((f32(k) + 0.5) * dt);
        optical += mist_density(grid, m0, m1, m2, m3, q) * dt;
    }
    return exp(-optical);
}
