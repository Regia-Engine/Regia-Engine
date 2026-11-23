// SPDX-License-Identifier: MPL-2.0
// regia_looks blasted-metal grain (ADR-2587), shared by the standalone renderer and the viewer hook.
// No bindings. The grain tilts the normal by an amount that depends on position only, so it turns
// with the piece and never changes on a still view. Grains too small to resolve fade to the smooth
// normal instead of shimmering.

fn looks_grain_hash(x: vec3<i32>) -> f32 {
    var h = (u32(x.x + 65536) * 73856093u) ^ (u32(x.y + 65536) * 19349663u) ^ (u32(x.z + 65536) * 83492791u);
    h = h ^ (h >> 16u);
    h = h * 0x7feb352du;
    h = h ^ (h >> 15u);
    h = h * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return f32(h & 0xffffu) / 32767.5 - 1.0;
}

// The gradient of trilinear value noise (Quilez, "Value Noise Derivatives"): eight hashes, smooth, and
// no periodic pattern to read at any zoom.
fn looks_grain_gradient(q: vec3<f32>) -> vec3<f32> {
    let c = floor(q);
    let i = vec3<i32>(c);
    let f = q - c;
    let u = f * f * (vec3<f32>(3.0) - 2.0 * f);
    let du = 6.0 * f * (vec3<f32>(1.0) - f);
    let a = looks_grain_hash(i);
    let b = looks_grain_hash(i + vec3<i32>(1, 0, 0));
    let d = looks_grain_hash(i + vec3<i32>(0, 1, 0));
    let e = looks_grain_hash(i + vec3<i32>(1, 1, 0));
    let g = looks_grain_hash(i + vec3<i32>(0, 0, 1));
    let h = looks_grain_hash(i + vec3<i32>(1, 0, 1));
    let k = looks_grain_hash(i + vec3<i32>(0, 1, 1));
    let l = looks_grain_hash(i + vec3<i32>(1, 1, 1));
    let k1 = b - a;
    let k2 = d - a;
    let k3 = g - a;
    let k4 = a - b - d + e;
    let k5 = a - d - g + k;
    let k6 = a - b - g + h;
    let k7 = -a + b + d - e + g - h - k + l;
    return du * vec3<f32>(
        k1 + k4 * u.y + k6 * u.z + k7 * u.y * u.z,
        k2 + k5 * u.z + k4 * u.x + k7 * u.z * u.x,
        k3 + k6 * u.x + k5 * u.y + k7 * u.x * u.y,
    );
}

// The tilt of the grain at `p_mm`, in the mesh's own space: two octaves on turned axes, so neither
// lattice lines up with the other.
fn looks_grain_tilt(p_mm: vec3<f32>, grain_mm: f32) -> vec3<f32> {
    let q = p_mm / grain_mm;
    let turned = vec3<f32>(q.y + 0.6 * q.z, q.z - 0.6 * q.x, q.x + 0.6 * q.y) * 1.7 + vec3<f32>(11.3, 5.1, 7.7);
    return looks_grain_gradient(q) * 0.3 + looks_grain_gradient(turned) * 0.15;
}

// How much of the grain a pixel can resolve: 1 when grains are large on screen, 0 when they are far
// smaller than a pixel. `footprint` is the pixel size in grains (fwidth of position over the grain
// size, taken at the caller's top level).
fn looks_grain_fade(footprint: f32) -> f32 {
    return 1.0 - smoothstep(0.6, 1.6, footprint);
}

// The share of the studio a blasted surface sees back out of its pits (an estimate, by eye). A share:
// never below 0, or a finish rougher than about 0.83 (or a curved one the viewer's antialiased
// roughness lifts there) would subtract the studio and turn the metal black.
fn looks_pit_shadow(roughness: f32) -> f32 {
    return clamp(1.0 - 1.2 * roughness, 0.0, 1.0);
}
