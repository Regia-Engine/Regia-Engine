// SPDX-License-Identifier: MPL-2.0
// regia_fluid GPU path (ADR-2441): PIC/FLIP substep kernels on a MAC grid.
// Particles to grid with fixed-point integer atomics, one extrapolation layer, gravity, the
// divergence, red-black SOR pressure relaxation, the pressure-gradient update, and grid to
// particles with RK2 advection. The CPU reference in sim.rs is the contract; this path is checked
// against it within a tolerance.
// Moving solids (ADR-2577): up to MAX_SOLIDS boxes, spheres and capsules in the uniform, posed at
// the substep's time (a counter the `clear` kernel advances), rasterized into the faces every
// substep; a closed face carries its solid's velocity, the divergence counts the solid share of each
// face at that velocity, and particles are pushed out of the moving solids relative to their velocity.

const MAX_SOLIDS: u32 = 16u;

struct Solid {
    // kind (0 box, 1 sphere, 2 capsule), then half extents / radius / (half height, radius)
    shape: vec4<f32>,
    // position xyz
    pos: vec4<f32>,
    // rotation quaternion xyzw
    rot: vec4<f32>,
    // linear velocity xyz
    lin: vec4<f32>,
    // angular velocity xyz
    ang: vec4<f32>,
};

struct Params {
    // nx, ny, nz, particle count
    n: vec4<u32>,
    // first index of the u, v, w faces, total faces
    off: vec4<u32>,
    // origin xyz, dx
    origin_dx: vec4<f32>,
    // acceleration xyz, dt
    accel_dt: vec4<f32>,
    // flip ratio, dt / (density dx), SOR omega, unused
    misc: vec4<f32>,
    // moving solid count, unused x3
    solid_info: vec4<u32>,
    solids: array<Solid, 16>,
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> pos: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec4<f32>>;
// Four words per face: the weighted velocity sum and the weight sum, both fixed point and 64 bits
// wide as (low, high) word pairs, so a clump of particles on one face cannot wrap them.
@group(0) @binding(3) var<storage, read_write> acc: array<atomic<u32>>;
// Faces 0..total: the grid velocity; total..2 total: the velocity before forces (FLIP's old field).
@group(0) @binding(4) var<storage, read_write> grid: array<f32>;
// Per face: the static open fraction, the open fraction of this substep, the solid velocity, and the
// static colliders' signed distance at the face centre (uploaded once).
@group(0) @binding(5) var<storage, read_write> face_solid: array<vec4<f32>>;
@group(0) @binding(6) var<storage, read_write> liquid: array<atomic<u32>>;
// Cells 0..cells: pressure; cells..2 cells: the right-hand side; 2 cells: the substep counter;
// 2 cells + 1..3 cells + 1: the static colliders' signed distance at each cell centre (uploaded once).
@group(0) @binding(7) var<storage, read_write> cellf: array<f32>;

const SCALE: f32 = 65536.0;

fn cells() -> u32 {
    return P.n.x * P.n.y * P.n.z;
}

fn size_of(axis: u32) -> vec3<u32> {
    return P.n.xyz + vec3<u32>(select(0u, 1u, axis == 0u), select(0u, 1u, axis == 1u), select(0u, 1u, axis == 2u));
}

fn face_index(axis: u32, c: vec3<u32>) -> u32 {
    let s = size_of(axis);
    return P.off[axis] + c.x + s.x * (c.y + s.y * c.z);
}

fn cell_index(c: vec3<u32>) -> u32 {
    return c.x + P.n.x * (c.y + P.n.y * c.z);
}

fn local(x: vec3<f32>) -> vec3<f32> {
    return (x - P.origin_dx.xyz) / P.origin_dx.w;
}

fn stencil_s(axis: u32, q: vec3<f32>) -> vec3<f32> {
    let off = vec3<f32>(select(0.5, 0.0, axis == 0u), select(0.5, 0.0, axis == 1u), select(0.5, 0.0, axis == 2u));
    return q - off;
}

fn stencil_base(axis: u32, s: vec3<f32>) -> vec3<f32> {
    let hi = max(vec3<f32>(size_of(axis)) - vec3<f32>(2.0), vec3<f32>(0.0));
    return floor(clamp(s, vec3<f32>(0.0), hi));
}

fn corner_weight(f: vec3<f32>, d: vec3<u32>) -> f32 {
    let wx = select(1.0 - f.x, f.x, d.x == 1u);
    let wy = select(1.0 - f.y, f.y, d.y == 1u);
    let wz = select(1.0 - f.z, f.z, d.z == 1u);
    return wx * wy * wz;
}

// Trilinear sample of component `axis` from `grid` at `base` (0 = new, total = old).
fn sample(axis: u32, q: vec3<f32>, base: u32) -> f32 {
    let size = size_of(axis);
    let s = stencil_s(axis, q);
    let b = stencil_base(axis, s);
    let f = clamp(s - b, vec3<f32>(0.0), vec3<f32>(1.0));
    let bu = vec3<u32>(b);
    var sum = 0.0;
    for (var corner = 0u; corner < 8u; corner = corner + 1u) {
        let d = vec3<u32>(corner & 1u, (corner >> 1u) & 1u, (corner >> 2u) & 1u);
        let c = min(bu + d, size - vec3<u32>(1u));
        sum = sum + grid[base + face_index(axis, c)] * corner_weight(f, d);
    }
    return sum;
}

fn sample_velocity(q: vec3<f32>, base: u32) -> vec3<f32> {
    return vec3<f32>(sample(0u, q, base), sample(1u, q, base), sample(2u, q, base));
}

fn cell_of(q: vec3<f32>) -> vec3<u32> {
    let hi = vec3<f32>(P.n.xyz) - vec3<f32>(1.0);
    return vec3<u32>(clamp(floor(q), vec3<f32>(0.0), hi));
}

// Axis and coordinates of linear face `f`.
fn face_axis(f: u32) -> u32 {
    if f < P.off.y {
        return 0u;
    }
    if f < P.off.z {
        return 1u;
    }
    return 2u;
}

fn face_coords(f: u32, axis: u32) -> vec3<u32> {
    let s = size_of(axis);
    let l = f - P.off[axis];
    return vec3<u32>(l % s.x, (l / s.x) % s.y, l / (s.x * s.y));
}

fn is_liquid(c: vec3<u32>) -> bool {
    return atomicLoad(&liquid[cell_index(c)]) != 0u;
}

fn open_frac(f: u32) -> f32 {
    return face_solid[f].y;
}

fn solid_velocity(f: u32) -> f32 {
    return face_solid[f].z;
}

fn quat_mul(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(
        a.w * b.xyz + b.w * a.xyz + cross(a.xyz, b.xyz),
        a.w * b.w - dot(a.xyz, b.xyz),
    );
}

fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

// Deterministic substitutes for WGSL builtins whose results may differ between devices: `sin` and
// `cos` allow absolute error 2^-11 and `sqrt`/`inverseSqrt` a few ULP (WGSL § 15.7.4), none of
// which a lockstep kernel may call. These use only add, multiply, divide, round and bitcast
// (ADR-4753). That removes the largest device-to-device drift, not all of it: WGSL allows f32
// division 2.5 ULP and a driver may fuse a multiply-add, so two devices can still differ in the
// last bits (measured on an RTX 5090 against lavapipe, ADR-5598). One device repeats itself.

// Square root by bit-seeded Newton iteration.
fn sqrt_det(x: f32) -> f32 {
    if (x <= 0.0) {
        return x;
    }
    var y = bitcast<f32>((bitcast<u32>(x) >> 1u) + 0x1FBD1DF5u);
    for (var i = 0; i < 3; i = i + 1) {
        y = 0.5 * (y + x / y);
    }
    return y;
}

// `(sin(x), cos(x))`: Cody-Waite reduction onto [-pi/4, pi/4] (the two-part pi/2 keeps
// `x - k * pio2_hi` accurate for |k| < 512, far above this kernel's inputs; the product
// `k * pio2_hi` itself is only correctly rounded — pio2_hi carries a full 24-bit mantissa — but
// the second term absorbs that error) and a fixed polynomial.
fn sin_cos_det(x: f32) -> vec2<f32> {
    let k = round(x * 0.6366197723675814);
    let r = (x - k * 1.570796251296997) - k * 7.549789415861596e-08;
    let r2 = r * r;
    let s = r * (1.0 + r2 * (-0.16666666666666666 + r2 * (0.008333333333333333 + r2 * (-0.0001984126984126984 + r2 * 0.0000027557319223985893))));
    let c = 1.0 + r2 * (-0.5 + r2 * (0.041666666666666664 + r2 * (-0.001388888888888889 + r2 * (0.0000248015873015873 + r2 * (-0.00000027557319223985894)))));
    let q = i32(k) & 3;
    if (q == 0) {
        return vec2<f32>(s, c);
    }
    if (q == 1) {
        return vec2<f32>(c, -s);
    }
    if (q == 2) {
        return vec2<f32>(-s, -c);
    }
    return vec2<f32>(-c, s);
}

// Seconds since the start of the solids' uploaded pose: the substep counter times the substep dt.
fn solid_time() -> f32 {
    return max(cellf[2u * cells()] - 1.0, 0.0) * P.accel_dt.w;
}

// Solid `i` posed at time `t`: its position and rotation.
fn solid_pos(i: u32, t: f32) -> vec3<f32> {
    return P.solids[i].pos.xyz + P.solids[i].lin.xyz * t;
}

fn solid_rot(i: u32, t: f32) -> vec4<f32> {
    let turn = P.solids[i].ang.xyz * t;
    let angle = sqrt_det(dot(turn, turn));
    if (angle <= 0.0) {
        return P.solids[i].rot;
    }
    let h = 0.5 * angle;
    let sc = sin_cos_det(h);
    let q = vec4<f32>(turn / angle * sc.x, sc.y);
    let r = quat_mul(q, P.solids[i].rot);
    return r / sqrt_det(dot(r, r));
}

fn box_sdf(d: vec3<f32>, half: vec3<f32>) -> f32 {
    let q = abs(d) - half;
    let m = max(q, vec3<f32>(0.0));
    return sqrt_det(dot(m, m)) + min(max(q.x, max(q.y, q.z)), 0.0);
}

fn solid_sdf(i: u32, x: vec3<f32>, t: f32) -> f32 {
    let r = solid_rot(i, t);
    let q = quat_rotate(vec4<f32>(-r.xyz, r.w), x - solid_pos(i, t));
    let s = P.solids[i].shape;
    if s.x < 0.5 {
        return box_sdf(q, s.yzw);
    }
    if s.x < 1.5 {
        return sqrt_det(dot(q, q)) - s.y;
    }
    let y = clamp(q.y, -s.y, s.y);
    let d = q - vec3<f32>(0.0, y, 0.0);
    return sqrt_det(dot(d, d)) - s.z;
}

fn moving_sdf(x: vec3<f32>, t: f32) -> f32 {
    var d = 3.0e38;
    for (var i = 0u; i < P.solid_info.x; i = i + 1u) {
        d = min(d, solid_sdf(i, x, t));
    }
    return d;
}

fn nearest_solid(x: vec3<f32>, t: f32) -> u32 {
    var best = 0u;
    var d = 3.0e38;
    for (var i = 0u; i < P.solid_info.x; i = i + 1u) {
        let di = solid_sdf(i, x, t);
        if di < d {
            d = di;
            best = i;
        }
    }
    return best;
}

fn solid_velocity_at(i: u32, x: vec3<f32>, t: f32) -> vec3<f32> {
    return P.solids[i].lin.xyz + cross(P.solids[i].ang.xyz, x - solid_pos(i, t));
}

fn node_world(n: vec3<u32>) -> vec3<f32> {
    return P.origin_dx.xyz + vec3<f32>(n) * P.origin_dx.w;
}

// Metres past a moving solid's bounding radius. Matches `SOLID_BOUND_PAD` in solid.rs: one
// millimetre covers a one-ulp Newton square root, so a sample the shape can reach is not skipped.
const SOLID_BOUND_PAD: f32 = 0.001;

// Squared radius of the sphere about the shape origin that contains it at any rotation.
// Box: |half extents|². Sphere: r². Capsule: (half height + radius)². Same packing as GpuSolid.
fn bound_radius_sq(shape: vec4<f32>) -> f32 {
    if (shape.x < 0.5) {
        return dot(shape.yzw, shape.yzw);
    }
    if (shape.x < 1.5) {
        return shape.y * shape.y;
    }
    let reach = shape.y + shape.z;
    return reach * reach;
}

// True when `x` is farther than `margin` from every solid's bounding sphere at time `t`.
// The sphere is centred on the posed position and contains the shape, so a point this calls
// outside has a signed distance past `margin` and the kernels can skip `moving_sdf`.
fn outside_solid_margin(x: vec3<f32>, t: f32, margin: f32) -> bool {
    let n = P.solid_info.x;
    for (var i = 0u; i < n; i = i + 1u) {
        let centre = solid_pos(i, t);
        let r = sqrt_det(bound_radius_sq(P.solids[i].shape)) + margin + SOLID_BOUND_PAD;
        let d = x - centre;
        if (dot(d, d) <= r * r) {
            return false;
        }
    }
    return true;
}

@compute @workgroup_size(64)
fn clear(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i < 4u * P.off.w {
        atomicStore(&acc[i], 0u);
    }
    if i < cells() {
        atomicStore(&liquid[i], 0u);
    }
    if i == 0u {
        cellf[2u * cells()] = cellf[2u * cells()] + 1.0;
    }
}

// The open fraction and boundary velocity of every face for this substep: the static fraction,
// narrowed by the moving solids' coverage (4 x 4 bilinear subsamples of their node distances, as on
// the CPU; the union is approximated by the smaller open fraction), and the nearest moving solid's
// velocity on a face it (partly) closes, when it is at least as near as the static colliders at the
// face centre (the CPU's rule): a static wall's closed face stays at rest.
@compute @workgroup_size(64)
fn rasterize(@builtin(global_invocation_id) id: vec3<u32>) {
    let f = id.x;
    if f >= P.off.w {
        return;
    }
    let st = face_solid[f].x;
    if P.solid_info.x == 0u {
        face_solid[f] = vec4<f32>(st, st, 0.0, face_solid[f].w);
        return;
    }
    let axis = face_axis(f);
    let c = face_coords(f, axis);
    let s = size_of(axis);
    if c[axis] == 0u || c[axis] + 1u >= s[axis] {
        face_solid[f] = vec4<f32>(st, st, 0.0, face_solid[f].w);
        return;
    }
    let t = solid_time();
    let a1 = (axis + 1u) % 3u;
    let a2 = (axis + 2u) % 3u;
    var e1 = vec3<u32>(0u);
    e1[a1] = 1u;
    var e2 = vec3<u32>(0u);
    e2[a2] = 1u;
    let p00 = node_world(c);
    let p10 = node_world(c + e1);
    let p01 = node_world(c + e2);
    let p11 = node_world(c + e1 + e2);
    // All four corners outside every bounding sphere: the SDF is positive at each, the open
    // fraction stays the static one, and the face velocity stays zero. Same write as the full path.
    if (outside_solid_margin(p00, t, 0.0)
        && outside_solid_margin(p10, t, 0.0)
        && outside_solid_margin(p01, t, 0.0)
        && outside_solid_margin(p11, t, 0.0)) {
        face_solid[f] = vec4<f32>(st, st, 0.0, face_solid[f].w);
        return;
    }
    let c00 = moving_sdf(p00, t);
    let c10 = moving_sdf(p10, t);
    let c01 = moving_sdf(p01, t);
    let c11 = moving_sdf(p11, t);
    var open_m = 1.0;
    if !(c00 >= 0.0 && c10 >= 0.0 && c01 >= 0.0 && c11 >= 0.0) {
        var n = 0.0;
        for (var s2 = 0u; s2 < 4u; s2 = s2 + 1u) {
            let t2 = (f32(s2) + 0.5) / 4.0;
            for (var s1 = 0u; s1 < 4u; s1 = s1 + 1u) {
                let t1 = (f32(s1) + 0.5) / 4.0;
                let a = c00 * (1.0 - t1) + c10 * t1;
                let b = c01 * (1.0 - t1) + c11 * t1;
                if a * (1.0 - t2) + b * t2 >= 0.0 {
                    n = n + 1.0;
                }
            }
        }
        open_m = n / 16.0;
    }
    var v = 0.0;
    let static_d = face_solid[f].w;
    if open_m < 1.0 {
        var centre = vec3<f32>(c) + vec3<f32>(0.5);
        centre[axis] = centre[axis] - 0.5;
        let x = P.origin_dx.xyz + centre * P.origin_dx.w;
        if moving_sdf(x, t) <= static_d {
            let i = nearest_solid(x, t);
            v = solid_velocity_at(i, x, t)[axis];
        }
    }
    face_solid[f] = vec4<f32>(st, min(st, open_m), v, static_d);
}

// Adds `x` to the 64-bit two's-complement sum in words (`word`, `word + 1`) = (low, high): the low
// word wraps, and the high word takes the sign extension of `x` plus the carry out of the low word
// (read from the low word's previous value). Integer adds commute, so the sum is exact and does
// not depend on the order the threads add in.
fn acc_add(word: u32, x: i32) {
    let lo_add = bitcast<u32>(x);
    let old = atomicAdd(&acc[word], lo_add);
    var hi_add = select(0u, 0xffffffffu, x < 0);
    if old + lo_add < old {
        hi_add = hi_add + 1u;
    }
    if hi_add != 0u {
        atomicAdd(&acc[word + 1u], hi_add);
    }
}

// The 64-bit sum in words (`word`, `word + 1`) as f32. A sum that fits in i32 converts exactly as
// a single i32 word did; a wider one is assembled from parts that are exact in f32 with correctly
// rounded adds, so it is the same on every device.
fn acc_load(word: u32) -> f32 {
    let lo = atomicLoad(&acc[word]);
    let hi = bitcast<i32>(atomicLoad(&acc[word + 1u]));
    let lo_i = bitcast<i32>(lo);
    if (hi == 0 && lo_i >= 0) || (hi == -1 && lo_i < 0) {
        return f32(lo_i);
    }
    return (f32(hi) * 4294967296.0 + f32(lo & 0xffff0000u)) + f32(lo & 0xffffu);
}

// NaN and ±∞ are the floats whose exponent bits are all ones; a `v == v` test may be folded
// away by a fast-math shader compiler (Metal, ADR-3287), so test the bits. The sign bit is
// shifted out rather than masked: the NVIDIA 610.88 Windows Vulkan compiler folded
// `(bits & 0x7f800000) == 0x7f800000` to false on a `dot` result, so NaN and ∞ passed; the
// shift form held in every case ADR-3150 probed, on Vulkan, DX12 and lavapipe (ADR-5875).
fn finite3(v: vec3<f32>) -> bool {
    let bad = (bitcast<vec3<u32>>(v) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return !any(bad);
}

@compute @workgroup_size(64)
fn p2g(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = id.x;
    if p >= P.n.w {
        return;
    }
    let q = local(pos[p].xyz);
    let v = vel[p].xyz;
    // The CPU particles-to-grid skips a non-finite particle. WGSL `clamp` turns a NaN into the
    // low bound (-32767 m/s), which would scatter onto every neighbour face and mark cell 0.
    if !finite3(q) || !finite3(v) {
        return;
    }
    for (var axis = 0u; axis < 3u; axis = axis + 1u) {
        let size = size_of(axis);
        let s = stencil_s(axis, q);
        let b = stencil_base(axis, s);
        let f = clamp(s - b, vec3<f32>(0.0), vec3<f32>(1.0));
        let bu = vec3<u32>(b);
        for (var corner = 0u; corner < 8u; corner = corner + 1u) {
            let d = vec3<u32>(corner & 1u, (corner >> 1u) & 1u, (corner >> 2u) & 1u);
            let c = min(bu + d, size - vec3<u32>(1u));
            let fi = face_index(axis, c);
            let w = corner_weight(f, d);
            // Saturate the fixed-point scatter into the i32 range: WGSL's float-to-i32
            // conversion is indeterminate out of range, and only in-range i32 terms keep the
            // atomic sums order-free across devices. The bound is MAX_FIXEDPOINT_SPEED
            // (32767 m/s) in SCALE units; uploads clamp to the same ceiling (ADR-4753). The
            // sums themselves are 64 bits wide (`acc_add`), so many terms on one face add up
            // exactly instead of wrapping.
            let scaled = clamp(w * v[axis] * SCALE, -2147418112.0, 2147418112.0);
            acc_add(4u * fi, i32(round(scaled)));
            acc_add(4u * fi + 2u, i32(round(w * SCALE)));
        }
    }
    atomicStore(&liquid[cell_index(cell_of(q))], 1u);
}

// A cell whose centre is inside a solid (a static collider, from the uploaded centre distances, or a
// moving solid at this substep's pose) and that shares an open face with a particle cell counts as
// liquid (flag 2; particle cells are 1), the CPU's rule: left as air, its p = 0 would drain the
// liquid into the solid. The face fractions cannot decide it: a solid that holds the centre but no
// corner node leaves every face fully open.
@compute @workgroup_size(64)
fn solid_liquid(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= cells() {
        return;
    }
    if atomicLoad(&liquid[i]) != 0u {
        return;
    }
    let c = vec3<u32>(i % P.n.x, (i / P.n.x) % P.n.y, i / (P.n.x * P.n.y));
    var touches = false;
    for (var a = 0u; a < 3u; a = a + 1u) {
        let e = vec3<u32>(select(0u, 1u, a == 0u), select(0u, 1u, a == 1u), select(0u, 1u, a == 2u));
        let w_lo = open_frac(face_index(a, c));
        let w_hi = open_frac(face_index(a, c + e));
        if c[a] > 0u && w_lo > 0.0 && atomicLoad(&liquid[cell_index(c - e)]) == 1u {
            touches = true;
        }
        if c[a] + 1u < P.n[a] && w_hi > 0.0 && atomicLoad(&liquid[cell_index(c + e)]) == 1u {
            touches = true;
        }
    }
    if !touches {
        return;
    }
    var inside = cellf[2u * cells() + 1u + i] < 0.0;
    if (!inside && P.solid_info.x > 0u) {
        let centre = P.origin_dx.xyz + (vec3<f32>(c) + vec3<f32>(0.5)) * P.origin_dx.w;
        let t = solid_time();
        // Outside the bounding sphere the moving SDF is positive, so the cell stays air.
        if (!outside_solid_margin(centre, t, 0.0)) {
            inside = moving_sdf(centre, t) < 0.0;
        }
    }
    if inside {
        atomicStore(&liquid[i], 2u);
    }
}

fn acc_valid(f: u32) -> bool {
    let hi = bitcast<i32>(atomicLoad(&acc[4u * f + 3u]));
    return hi > 0 || (hi == 0 && atomicLoad(&acc[4u * f + 2u]) > 0u);
}

fn acc_value(f: u32) -> f32 {
    return acc_load(4u * f) / acc_load(4u * f + 2u);
}

// Normalise the sums; a face no particle reached takes the mean of its valid neighbours (one
// extrapolation layer). Closed faces are zero. Writes both the new and the old field.
@compute @workgroup_size(64)
fn normalize(@builtin(global_invocation_id) id: vec3<u32>) {
    let f = id.x;
    let total = P.off.w;
    if f >= total {
        return;
    }
    let axis = face_axis(f);
    let c = face_coords(f, axis);
    let s = size_of(axis);
    var v = 0.0;
    if acc_valid(f) {
        v = acc_value(f);
    } else {
        var sum = 0.0;
        var n = 0.0;
        for (var a = 0u; a < 3u; a = a + 1u) {
            let e = vec3<u32>(select(0u, 1u, a == 0u), select(0u, 1u, a == 1u), select(0u, 1u, a == 2u));
            if c[a] > 0u {
                let g = face_index(axis, c - e);
                if acc_valid(g) {
                    sum = sum + acc_value(g);
                    n = n + 1.0;
                }
            }
            if c[a] + 1u < s[a] {
                let g = face_index(axis, c + e);
                if acc_valid(g) {
                    sum = sum + acc_value(g);
                    n = n + 1.0;
                }
            }
        }
        if n > 0.0 {
            v = sum / n;
        }
    }
    if open_frac(f) <= 0.0 {
        v = solid_velocity(f);
    }
    grid[f] = v;
    grid[total + f] = v;
}

@compute @workgroup_size(64)
fn forces(@builtin(global_invocation_id) id: vec3<u32>) {
    let f = id.x;
    if f >= P.off.w {
        return;
    }
    if open_frac(f) > 0.0 {
        let axis = face_axis(f);
        grid[f] = grid[f] + P.accel_dt[axis] * P.accel_dt.w;
    }
}

// The weighted divergence's right-hand side; the pressure starts at zero.
@compute @workgroup_size(64)
fn divergence(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    let n = cells();
    if i >= n {
        return;
    }
    cellf[i] = 0.0;
    let c = vec3<u32>(i % P.n.x, (i / P.n.x) % P.n.y, i / (P.n.x * P.n.y));
    if !is_liquid(c) {
        cellf[n + i] = 0.0;
        return;
    }
    var div = 0.0;
    for (var a = 0u; a < 3u; a = a + 1u) {
        let e = vec3<u32>(select(0u, 1u, a == 0u), select(0u, 1u, a == 1u), select(0u, 1u, a == 2u));
        let lo = face_index(a, c);
        let hi = face_index(a, c + e);
        div = div + open_frac(hi) * grid[hi] - open_frac(lo) * grid[lo];
        div = div + (1.0 - open_frac(hi)) * solid_velocity(hi) - (1.0 - open_frac(lo)) * solid_velocity(lo);
    }
    cellf[n + i] = -div / P.misc.y;
}

fn relax(i: u32, color: u32) {
    let n = cells();
    if i >= n {
        return;
    }
    let c = vec3<u32>(i % P.n.x, (i / P.n.x) % P.n.y, i / (P.n.x * P.n.y));
    if ((c.x + c.y + c.z) & 1u) != color || !is_liquid(c) {
        return;
    }
    var diag = 0.0;
    var sum = 0.0;
    for (var a = 0u; a < 3u; a = a + 1u) {
        let e = vec3<u32>(select(0u, 1u, a == 0u), select(0u, 1u, a == 1u), select(0u, 1u, a == 2u));
        let w_lo = open_frac(face_index(a, c));
        let w_hi = open_frac(face_index(a, c + e));
        diag = diag + w_lo + w_hi;
        if c[a] > 0u && w_lo > 0.0 {
            let nb = c - e;
            if is_liquid(nb) {
                sum = sum + w_lo * cellf[cell_index(nb)];
            }
        }
        if c[a] + 1u < P.n[a] && w_hi > 0.0 {
            let nb = c + e;
            if is_liquid(nb) {
                sum = sum + w_hi * cellf[cell_index(nb)];
            }
        }
    }
    if diag <= 0.0 {
        cellf[i] = 0.0;
        return;
    }
    let gs = (cellf[n + i] + sum) / diag;
    cellf[i] = cellf[i] + P.misc.z * (gs - cellf[i]);
}

@compute @workgroup_size(64)
fn relax_red(@builtin(global_invocation_id) id: vec3<u32>) {
    relax(id.x, 0u);
}

@compute @workgroup_size(64)
fn relax_black(@builtin(global_invocation_id) id: vec3<u32>) {
    relax(id.x, 1u);
}

fn pressure_at(c: vec3<u32>) -> f32 {
    if is_liquid(c) {
        return cellf[cell_index(c)];
    }
    return 0.0;
}

@compute @workgroup_size(64)
fn project(@builtin(global_invocation_id) id: vec3<u32>) {
    let f = id.x;
    if f >= P.off.w {
        return;
    }
    if open_frac(f) <= 0.0 {
        grid[f] = solid_velocity(f);
        return;
    }
    let axis = face_axis(f);
    let c = face_coords(f, axis);
    let e = vec3<u32>(select(0u, 1u, axis == 0u), select(0u, 1u, axis == 1u), select(0u, 1u, axis == 2u));
    var p_lo = 0.0;
    var p_hi = 0.0;
    var touches = false;
    if c[axis] > 0u {
        let lo = c - e;
        touches = touches || is_liquid(lo);
        p_lo = pressure_at(lo);
    }
    if c[axis] < P.n[axis] {
        touches = touches || is_liquid(c);
        p_hi = pressure_at(c);
    }
    if touches {
        grid[f] = grid[f] - P.misc.y * (p_hi - p_lo);
    }
}

@compute @workgroup_size(64)
fn g2p_advect(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = id.x;
    if p >= P.n.w {
        return;
    }
    let total = P.off.w;
    let dt = P.accel_dt.w;
    var x = pos[p].xyz;
    let q = local(x);
    let new_v = sample_velocity(q, 0u);
    let old_v = sample_velocity(q, total);
    let flip = P.misc.x;
    var v = (vel[p].xyz + (new_v - old_v)) * flip + new_v * (1.0 - flip);
    let v1 = sample_velocity(q, 0u);
    let mid = x + v1 * (0.5 * dt);
    let v2 = sample_velocity(local(mid), 0u);
    x = x + v2 * dt;
    if (P.solid_info.x > 0u) {
        // Out of the moving solids, dropping the velocity into them relative to theirs.
        let t = solid_time();
        let margin = P.origin_dx.w * 0.05;
        // Past the bounding sphere plus the push margin, phi is at least `margin`.
        if (!outside_solid_margin(x, t, margin)) {
            let phi = moving_sdf(x, t);
            if (phi < margin) {
                let h = P.origin_dx.w * 0.25;
                let g = vec3<f32>(
                    moving_sdf(x + vec3<f32>(h, 0.0, 0.0), t) - moving_sdf(x - vec3<f32>(h, 0.0, 0.0), t),
                    moving_sdf(x + vec3<f32>(0.0, h, 0.0), t) - moving_sdf(x - vec3<f32>(0.0, h, 0.0), t),
                    moving_sdf(x + vec3<f32>(0.0, 0.0, h), t) - moving_sdf(x - vec3<f32>(0.0, 0.0, h), t),
                );
                let len = sqrt_det(dot(g, g));
                if len > 0.0 {
                    let n = g / len;
                    let vs = solid_velocity_at(nearest_solid(x, t), x, t);
                    x = x + n * (margin - phi);
                    let vn = dot(v - vs, n);
                    if vn < 0.0 {
                        v = v - n * vn;
                    }
                }
            }
        }
    }
    let eps = P.origin_dx.w * 0.001;
    let lo = P.origin_dx.xyz + vec3<f32>(eps);
    let hi = P.origin_dx.xyz + vec3<f32>(P.n.xyz) * P.origin_dx.w - vec3<f32>(eps);
    // -1 / +1: clamped onto the low / high wall of that axis. The CPU slide in
    // `resolve_boundaries` is the contract; static colliders are not on this path.
    var walls = vec3<i32>(0, 0, 0);
    for (var a = 0u; a < 3u; a = a + 1u) {
        if x[a] < lo[a] {
            x[a] = lo[a];
            v[a] = max(v[a], 0.0);
            walls[a] = -1;
        } else if x[a] > hi[a] {
            x[a] = hi[a];
            v[a] = min(v[a], 0.0);
            walls[a] = 1;
        }
    }
    if P.solid_info.x > 0u && (walls.x != 0 || walls.y != 0 || walls.z != 0) {
        let t = solid_time();
        let margin = P.origin_dx.w * 0.05;
        let h = P.origin_dx.w * 0.25;
        for (var iter = 0u; iter < 8u; iter = iter + 1u) {
            let phi = moving_sdf(x, t);
            if phi >= margin {
                break;
            }
            var g = vec3<f32>(
                moving_sdf(x + vec3<f32>(h, 0.0, 0.0), t) - moving_sdf(x - vec3<f32>(h, 0.0, 0.0), t),
                moving_sdf(x + vec3<f32>(0.0, h, 0.0), t) - moving_sdf(x - vec3<f32>(0.0, h, 0.0), t),
                moving_sdf(x + vec3<f32>(0.0, 0.0, h), t) - moving_sdf(x - vec3<f32>(0.0, 0.0, h), t),
            );
            for (var a = 0u; a < 3u; a = a + 1u) {
                if (walls[a] < 0 && g[a] < 0.0) || (walls[a] > 0 && g[a] > 0.0) {
                    g[a] = 0.0;
                }
            }
            let glen = sqrt_det(dot(g, g));
            var n = vec3<f32>(0.0);
            if glen > 0.0 {
                n = g / glen;
            } else {
                // Inward wall normal: a tangent step stays inside a solid that fills the wall.
                var inward = vec3<f32>(0.0);
                for (var a = 0u; a < 3u; a = a + 1u) {
                    if walls[a] < 0 {
                        inward[a] = 1.0;
                    } else if walls[a] > 0 {
                        inward[a] = -1.0;
                    }
                }
                let ilen = sqrt_det(dot(inward, inward));
                if ilen > 0.0 {
                    n = inward / ilen;
                }
            }
            let vs = solid_velocity_at(nearest_solid(x, t), x, t);
            x = clamp(x + n * (margin - phi), lo, hi);
            let vn = dot(v - vs, n);
            if vn < 0.0 {
                v = v - n * vn;
            }
        }
    }
    pos[p] = vec4<f32>(x, 0.0);
    vel[p] = vec4<f32>(v, 0.0);
}
