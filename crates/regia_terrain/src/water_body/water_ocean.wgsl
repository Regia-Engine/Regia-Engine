// SPDX-License-Identifier: MPL-2.0
// The FFT ocean's compute pass (ADR-1969, docs/specs/water-ocean.md): one spectrum evolution and
// 2 log2(N) inverse FFT stages per band, in storage buffers; the frame then copies the result into
// the texture array `water_body.wgsl` reads. The CPU mirror is `ocean.rs` (`OceanFrame`).
//
// Sources (docs/provenance.md): the spectrum evolution and the choppy offset after Tessendorf 2001;
// the Stockham autosort FFT (Cochran et al. 1967), in the radix-2 form Bainville's public OpenCL FFT
// article gives; whole-loop periodic frequencies after Tessendorf 2001 § 4.3.
//
// A map holds two layers of `size * size` vec4s per band, row along z:
//   layer 0: (Dx, h, Dz, Dxz)
//   layer 1: (hx, hz, Dxx, Dzz)
// With lingering foam on (ADR-2843) the first work buffer also holds, after every band's layers,
// one history layer per band: (foam, 0, 0, 0). No FFT stage writes there.
// Each vec4 is two complex numbers while the transform runs: the spectrum `A + i B` of two real
// fields, whose inverse transform has A in its real part and B in its imaginary part.

struct Params {
    size: u32,
    // First vec4 of the band in the map buffers.
    base: u32,
    // Spectrum stage: the first vec4 of the band in `modes`. FFT stage: the twiddle step.
    table: u32,
    // Spectrum stage: the loop fraction in 2^-32. FFT stage: the sorted run length `p`.
    tau_or_p: u32,
    // 0: along x, 1: along z. Foam stage: the bits of the foam onset.
    axis: u32,
    // Foam stage: the band's normalization of its compression.
    choppiness: f32,
    // Foam stage: growth and decay of this step (0 in the other stages).
    growth: f32,
    decay: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
// Per mode, row along z: (a, b as two complex numbers), (k x, k z, n, 1 / |k|).
@group(0) @binding(1) var<storage, read> modes: array<vec4<f32>>;
// exp(2 pi i j / size), j < size / 2.
@group(0) @binding(2) var<storage, read> twiddles: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> dst: array<vec4<f32>>;

const PHASE_SCALE: f32 = 1.4629180792671596e-9;

// One mode of the band at (x, z): the spectrum of every field at this instant.
@compute @workgroup_size(16, 16)
fn cs_spectrum(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = params.size;
    if (id.x >= n || id.y >= n) {
        return;
    }
    let index = id.y * n + id.x;
    let at = params.table + 2u * index;
    let amp = modes[at];
    let m = modes[at + 1u];
    // Wrapping integer phase: the same on the CPU (`mode_phasor`), at any time.
    let th = f32(u32(m.z) * params.tau_or_p) * PHASE_SCALE;
    let c = cos(th);
    let s = sin(th);
    // h = a e^(-i th) + b e^(i th)
    let hr = amp.x * c + amp.y * s + amp.z * c - amp.w * s;
    let hi = amp.y * c - amp.x * s + amp.w * c + amp.z * s;
    let chop = params.choppiness;
    let ux = m.x * m.w;
    let uz = m.y * m.w;
    let q = -chop * m.w;
    let xz = q * m.x * m.y;
    let xx = q * m.x * m.x;
    let zz = q * m.y * m.y;
    // i h = (-hi, hr); a real field F times i, added as `A + i F`, is (-F.im, F.re).
    let c0 = vec2<f32>(-chop * ux * hi - hi, chop * ux * hr + hr);
    let c1 = vec2<f32>(-chop * uz * hi - xz * hi, chop * uz * hr + xz * hr);
    let c2 = vec2<f32>(-m.x * hi - m.y * hr, m.x * hr - m.y * hi);
    let c3 = vec2<f32>(xx * hr - zz * hi, xx * hi + zz * hr);
    let out = params.base + index;
    dst[out] = vec4<f32>(c0, c1);
    dst[out + n * n] = vec4<f32>(c2, c3);
}

// Two complex numbers times one.
fn twiddled(a: vec4<f32>, w: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        a.x * w.x - a.y * w.y,
        a.x * w.y + a.y * w.x,
        a.z * w.x - a.w * w.y,
        a.z * w.y + a.w * w.x,
    );
}

// The buffer index of element `i` of line `line`.
fn element(i: u32, line: u32) -> u32 {
    if (params.axis == 0u) {
        return line * params.size + i;
    }
    return i * params.size + line;
}

// One Stockham radix-2 stage over one line, for butterfly `i`.
fn butterfly(first: u32, i: u32, line: u32) {
    let n = params.size;
    let p = params.tau_or_p;
    let k = i & (p - 1u);
    let out = (i << 1u) - k;
    let w = twiddles[k * params.table];
    let u0 = src[first + element(i, line)];
    let u1 = twiddled(src[first + element(i + n / 2u, line)], w);
    dst[first + element(out, line)] = u0 + u1;
    dst[first + element(out + p, line)] = u0 - u1;
}

// One inverse FFT stage: x is the butterfly, y the line.
@compute @workgroup_size(16, 16)
fn cs_fft(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = params.size;
    if (id.x >= n / 2u || id.y >= n) {
        return;
    }
    butterfly(params.base, id.x, id.y);
    butterfly(params.base + n * n, id.x, id.y);
}

// Crest foam ramp of `swell.rs` `FOAM_RAMP` (the water shader's crest_foam uses the same).
const FOAM_RAMP: f32 = 0.3;

// One step of a history texel (CPU: `ocean_foam::foam_step`): growth toward the crest's foam,
// or linear decay that never goes below it.
fn foam_step(previous: f32, inject: f32) -> f32 {
    if (inject >= previous) {
        return previous + (inject - previous) * params.growth;
    }
    return max(previous - params.decay, inject);
}

// Lingering foam (ADR-2843, CPU: `ocean_foam::FoamHistory::step`): one band's texel (x, z). The
// finished maps of the band start at `base`, its history at `table`, both in `dst` (the first
// work buffer). `tau_or_p` 1 restarts the history from no foam.
@compute @workgroup_size(16, 16)
fn cs_foam(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = params.size;
    if (id.x >= n || id.y >= n) {
        return;
    }
    let index = id.y * n + id.x;
    let t0 = dst[params.base + index];
    let t1 = dst[params.base + n * n + index];
    let j = (1.0 + t1.z) * (1.0 + t1.w) - t0.w * t0.w;
    let onset = bitcast<f32>(params.axis);
    let x = clamp(((1.0 - j) * params.choppiness - onset) / FOAM_RAMP, 0.0, 1.0);
    let inject = x * x * (3.0 - 2.0 * x);
    let at = params.table + index;
    var previous = dst[at].x;
    if (params.tau_or_p != 0u) {
        previous = 0.0;
    }
    dst[at] = vec4<f32>(foam_step(previous, inject), 0.0, 0.0, 0.0);
}
