// SPDX-License-Identifier: MPL-2.0
// Screen-space GI with the world-space radiance cache behind it (`radiance_cache.rs`, `specs/79`
// § 12, ADR-2085). Composed as ssgi.wgsl + radiance_cache_common.wgsl + this file, so a frame
// without the cache compiles the plain gather, unchanged.
//
// Each pixel marches short cosine-weighted rays through the depth buffer. What a ray finds:
//
//   - a hit: the lit colour of that surface;
//   - a miss: nothing by default; with a miss cap (`rc.gather.w` > 0, off since ADR-2605), the cache, read
//     where the ray last stood in known free space.
//
// The forward pass has already added the probes' irradiance at this pixel, so every ray subtracts
// what the cache holds toward its direction at the pixel (`co`). Per ray:
//
//   hit:  L_hit − co          the near surface the probes' spacing cannot resolve replaces them
//   miss: C(x, ω) − co_free   the parallax of walking toward ω before looking
//
// A miss compares like with like: `co_free` is the same read as `co` without the cells on the receiver's
// own plane, which `C` cannot pick either. Inverting irradiance into radiance multiplies a difference
// between cells by up to 2.5, and a cell on a surface differs from one above it, so comparing against `co`
// would darken every pixel by that step alone (ADR-2085).
//
// The result is scaled by the nominal receiver albedo, as the plain gather does (the forward path has
// no albedo buffer). `rc.limits.x` bounds how far it may go negative; by default not at all, so the
// pass only adds what exceeds the probes' light, where the plain gather adds every hit in full.
//
// Why this cannot carry light into a closed room:
//   - a miss never reads past known free space: a ray stops at the screen's edge or where it passes
//     behind the depth buffer, so its read position is on the camera's side of every visible wall;
//   - a miss's read is capped at a multiple of what the cache holds at the pixel, so a cell whose
//     interpolation reaches through a wall cannot exceed the room's own light by much;
//   - a cell behind the receiver's plane does not count in either read (one within the depth buffer's
//     error of it stands on the receiver's surface, ADR-4106);
//   - a hit is a surface in this frame's image, already lit by whatever really reaches it.
//
// Sources: Ward, Rubinstein, Clear, "A Ray Tracing Solution for Diffuse Interreflection",
// SIGGRAPH 1988, and Křivánek et al., "Radiance Caching for Efficient Global Illumination
// Computation", IEEE TVCG 2005 (reusing cached directional light at nearby points); Tanner, Migdal,
// Jones, "The Clipmap", SIGGRAPH 1998 (nested camera-centered grids, toroidal addressing); McGuire,
// Mara, "Efficient GPU Screen-Space Ray Tracing", JCGT 3(4) 2014 (marching the depth buffer, a
// thickness test, the screen's edge as a miss); Duff et al., "Building an Orthonormal Basis,
// Revisited", JCGT 6(1) 2017.

@group(1) @binding(0) var<uniform> rc: RcUniform;
@group(1) @binding(1) var<storage, read> rc_cells: array<u32>;
// Work counters (ADR-2605, `RadianceCacheSettings::count_work`): pixels, rays, depth steps, cell reads.
@group(1) @binding(2) var<storage, read_write> rc_work: array<atomic<u32>, 4>;

// The ray pattern: `rc.limits.y` of these eight rays a frame, taking turns (ADR-2605).
const RC_RAYS: u32 = 8u;
const RC_STEPS: u32 = 8u;

// Work counters (ADR-2605): what the gather did, by kind (0 = a pixel that gathered, 1 = a ray, 2 = a
// depth-buffer step, 3 = a cell read), counted only on a frame that asks (`rc.counts.y`).
fn rc_tally(kind: u32, n: u32) {
    if (rc.counts.y != 0u) {
        atomicAdd(&rc_work[kind], n);
    }
}
// How far off the receiver's plane a cell must lie to stand for free space, in cell spacings.
const RC_FREE_CELL: f32 = 0.25;

// What the gather writes (`rc.counts.z`, `RadianceCacheTerm`, ADR-4106): 0 = the shipped image,
// 1 = the same gathered afresh; from `RC_TERM_BASE` on, one positive term of the sum, unclamped, on
// top of the frame it corrects (so a term is a capture minus the `RC_TERM_BASE` capture).
const RC_TERM_BASE: u32 = 2u;
const RC_TERM_HIT_RADIANCE: u32 = 3u;
const RC_TERM_HIT_CORRECTION: u32 = 4u;
const RC_TERM_MISS_READ: u32 = 5u;
const RC_TERM_MISS_CORRECTION: u32 = 6u;
const RC_TERM_ORIGIN: u32 = 7u;
const RC_TERM_COVERAGE: u32 = 8u;
const RC_TERM_CELLS: u32 = 9u;
// The miss read's cap a term uses when the miss cap is off (the sweep's `miss4`).
const RC_TERM_MISS_CAP: f32 = 4.0;

fn rc_read_face(word: u32, face: u32) -> vec3<f32> {
    return rc_unpack_rgb9e5(rc_cells[word + face]);
}

// What the cell at `word` holds toward unit `w`: E(w) = w.x² E(±x) + w.y² E(±y) + w.z² E(±z).
fn rc_read_dir(word: u32, w: vec3<f32>) -> vec3<f32> {
    let sq = w * w;
    return rc_read_face(word, select(0u, 1u, w.x < 0.0)) * sq.x
        + rc_read_face(word, select(2u, 3u, w.y < 0.0)) * sq.y
        + rc_read_face(word, select(4u, 5u, w.z < 0.0)) * sq.z;
}

// The six faces of a cube (+x −x +y −y +z −z). Six named fields and only ever a `let` value: a
// function-local array (or a struct `var`) carries an explicit layout Vulkan forbids in Function
// storage (ADR-026; the shader matrix's `ssgi_cached.wgsl` layout scan, ADR-2644).
struct RcCube {
    px: vec3<f32>,
    nx: vec3<f32>,
    py: vec3<f32>,
    ny: vec3<f32>,
    pz: vec3<f32>,
    nz: vec3<f32>,
}

// `rc_origin`'s result: the cube over every counted cell (`e`), over those of them in free space
// (`free`), `flags` bit 0 (any cell counted) and bit 1 (any free cell counted), and `cells`
// (attribution, ADR-4106): of the eight corners, how many hold their own cell (the tag), how many of
// those counted, and how many stand in free space.
struct RcOrigin {
    e: RcCube,
    free: RcCube,
    flags: u32,
    cells: vec3<f32>,
}

const RC_ORIGIN_COUNTED: u32 = 1u;
const RC_ORIGIN_FREE: u32 = 2u;

fn rc_cube_zero() -> RcCube {
    let z = vec3<f32>(0.0);
    return RcCube(z, z, z, z, z, z);
}

// The six faces at the pixel, trilinear over the cells on the receiver's side of its plane, and again
// (`free`) over those of them that stand in free space; `flags` says whether any cell counted and
// whether any free one did.
//
// `on_plane` (m) is how far the receiver's position is known: a cell up to that far behind its plane
// stands on the receiver's surface and counts (ADR-4106). A floor on a plane of cells (y = 0, the
// common case) otherwise dropped or kept its own layer on the sign of the depth buffer's rounding,
// and the renormalized trilinear weight then went to the layer half a cell up: a brighter correction
// toward the walls on one GPU, not on another.
fn rc_origin(p: vec3<f32>, n: vec3<f32>, on_plane: f32) -> RcOrigin {
    let level = rc_level_at(p);
    if (level == RC_NO_LEVEL) {
        return RcOrigin(rc_cube_zero(), rc_cube_zero(), 0u, vec3<f32>(0.0));
    }
    let s = rc.spacing[level];
    let g = p / s;
    let ibase = vec3<i32>(floor(g));
    let f = g - vec3<f32>(ibase);
    var a0 = vec3<f32>(0.0);
    var a1 = vec3<f32>(0.0);
    var a2 = vec3<f32>(0.0);
    var a3 = vec3<f32>(0.0);
    var a4 = vec3<f32>(0.0);
    var a5 = vec3<f32>(0.0);
    var f0 = vec3<f32>(0.0);
    var f1 = vec3<f32>(0.0);
    var f2 = vec3<f32>(0.0);
    var f3 = vec3<f32>(0.0);
    var f4 = vec3<f32>(0.0);
    var f5 = vec3<f32>(0.0);
    var cells = vec3<f32>(0.0);
    var wsum = 0.0;
    var wsum_free = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let o = vec3<i32>(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        let cell = ibase + o;
        let word = rc_slot(level, cell) * RC_CELL_WORDS;
        if (rc_cells[word + RC_TAG_WORD] != rc_tag(cell)) {
            continue;
        }
        cells.x += 1.0;
        let off_plane = dot(vec3<f32>(cell) * s - p, n);
        if (off_plane < -on_plane) {
            continue;
        }
        let free = off_plane >= RC_FREE_CELL * s;
        cells.y += 1.0;
        if (free) {
            cells.z += 1.0;
        }
        let tri_axes = mix(vec3<f32>(1.0) - f, f, vec3<f32>(o));
        let w = max(tri_axes.x * tri_axes.y * tri_axes.z, 0.001);
        let c0 = rc_read_face(word, 0u) * w;
        let c1 = rc_read_face(word, 1u) * w;
        let c2 = rc_read_face(word, 2u) * w;
        let c3 = rc_read_face(word, 3u) * w;
        let c4 = rc_read_face(word, 4u) * w;
        let c5 = rc_read_face(word, 5u) * w;
        a0 = a0 + c0;
        a1 = a1 + c1;
        a2 = a2 + c2;
        a3 = a3 + c3;
        a4 = a4 + c4;
        a5 = a5 + c5;
        if (free) {
            f0 = f0 + c0;
            f1 = f1 + c1;
            f2 = f2 + c2;
            f3 = f3 + c3;
            f4 = f4 + c4;
            f5 = f5 + c5;
        }
        wsum = wsum + w;
        if (free) {
            wsum_free = wsum_free + w;
        }
    }
    if (wsum <= 0.0) {
        return RcOrigin(rc_cube_zero(), rc_cube_zero(), 0u, cells);
    }
    let fw = max(wsum_free, 1e-6);
    let e = RcCube(a0 / wsum, a1 / wsum, a2 / wsum, a3 / wsum, a4 / wsum, a5 / wsum);
    let free_cube = RcCube(f0 / fw, f1 / fw, f2 / fw, f3 / fw, f4 / fw, f5 / fw);
    return RcOrigin(e, free_cube, RC_ORIGIN_COUNTED | select(0u, RC_ORIGIN_FREE, wsum_free > 0.0), cells);
}

fn rc_origin_dir(e: RcCube, w: vec3<f32>) -> vec3<f32> {
    let sq = w * w;
    return select(e.px, e.nx, w.x < 0.0) * sq.x
        + select(e.py, e.ny, w.y < 0.0) * sq.y
        + select(e.pz, e.nz, w.z < 0.0) * sq.z;
}

// One cell near `x` picked by `jitter` (stochastic trilinear: a share of one cell per ray), toward
// `w`. a = 0 where no valid cell holds it, or where the cell does not stand in free space off the
// receiver's plane (`p`, `n`): under a floor it reads the wrong side of the floor.
fn rc_read_at(x: vec3<f32>, w: vec3<f32>, jitter: vec3<f32>, p: vec3<f32>, n: vec3<f32>) -> vec4<f32> {
    let level = rc_level_at(x);
    if (level == RC_NO_LEVEL) {
        return vec4<f32>(0.0);
    }
    let s = rc.spacing[level];
    let cell = vec3<i32>(floor(x / s + jitter));
    if (dot(vec3<f32>(cell) * s - p, n) < RC_FREE_CELL * s) {
        return vec4<f32>(0.0);
    }
    let word = rc_slot(level, cell) * RC_CELL_WORDS;
    if (rc_cells[word + RC_TAG_WORD] != rc_tag(cell)) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(rc_read_dir(word, w), 1.0);
}

@fragment
fn fs_cached(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let scale = max(params.noise.y, 1.0);
    let frag_i = vec2<i32>(i32(frag.x), i32(frag.y));
    let size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
    let pix = clamp(frag_i * i32(scale) + i32(scale) / 2, vec2<i32>(0), size - vec2<i32>(1));
    // At half resolution the gather is the signed difference alone; the upsample adds the colour.
    let half = scale > 1.5;
    var base = textureLoad(hdr_tex, pix, 0);
    if (half) {
        base = vec4<f32>(0.0);
    }
    let unchanged = vec4<f32>(max(base.rgb, vec3<f32>(0.0)), base.a);
    let depth = textureLoad(depth_tex, pix, 0);
    if (depth >= 1.0 || params.screen.w <= 0.0) {
        return unchanged;
    }
    let p = view_position(pix);
    let px0 = view_position(pix - vec2<i32>(1, 0));
    let px1 = view_position(pix + vec2<i32>(1, 0));
    let py0 = view_position(pix - vec2<i32>(0, 1));
    let py1 = view_position(pix + vec2<i32>(0, 1));
    let dx = select(px1 - p, p - px0, abs(p.z - px0.z) < abs(px1.z - p.z));
    let dy = select(py1 - p, p - py0, abs(p.z - py0.z) < abs(py1.z - p.z));
    let c = cross(dy, dx);
    let c_len2 = dot(c, c);
    if (!(c_len2 > 1e-24)) {
        return unchanged;
    }
    var n = c * inverseSqrt(c_len2);
    if (dot(n, -p) < 0.0) {
        n = -n;
    }
    let ray_m = rc.gather.y;
    let z_self = max(-p.z, 1e-3);
    // Far receivers: the ray spans a couple of pixels and the probes cover that scale alone.
    let projected_px = ray_m * params.proj.y * 0.5 * params.screen.y / z_self;
    if (projected_px < 3.0) {
        return unchanged;
    }
    let to_world = rc.view_to_world;
    let p_w = (to_world * vec4<f32>(p, 1.0)).xyz;
    let n_w = normalize((to_world * vec4<f32>(n, 0.0)).xyz);
    // The ray's lift off the surface below is the same allowance for where the depth buffer put it.
    let bias = 0.005 * z_self + 0.02;
    let rc_o = rc_origin(p_w, n_w, bias);
    if ((rc_o.flags & RC_ORIGIN_COUNTED) == 0u) {
        // Outside the cache or not filled yet: the forward pass's probe light stands.
        return unchanged;
    }
    let e = rc_o.e;
    let e_free = rc_o.free;
    let free_ok = (rc_o.flags & RC_ORIGIN_FREE) != 0u;
    let cells = rc_o.cells;
    let exposure = rc.gather.x;
    let miss_cap = rc.gather.w;
    let noise = ign(frag.xy + 5.588238 * params.noise.x);
    let rotation = noise * 6.2831853;
    let noise2 = ign(frag.yx + 11.0 + 7.137 * params.noise.x);
    let start = p + n * bias;
    // An orthonormal basis around the normal (Duff et al. 2017).
    let sgn = select(-1.0, 1.0, n.z >= 0.0);
    let ba = -1.0 / (sgn + n.z);
    let bb = n.x * n.y * ba;
    let tangent = vec3<f32>(1.0 + sgn * n.x * n.x * ba, sgn * bb, -sgn * n.x);
    let bitangent = vec3<f32>(bb, sgn + n.y * n.y * ba, -n.y);
    var sum = vec3<f32>(0.0);
    // Rays this frame and the share of the pattern they take (`RadianceCacheSettings::rays`).
    let rays = clamp(u32(rc.limits.y), 1u, RC_RAYS);
    let stride = RC_RAYS / rays;
    let share = u32(max(params.noise.x, 0.0)) % stride;
    let steps = clamp(u32(rc.limits.z), 2u, RC_STEPS);
    let sky_stop = rc.limits.w > 0.5;
    // The terms of the sum, for attribution (ADR-4106); the shipped image never reads them.
    let term = rc.counts.z;
    let attributing = term >= RC_TERM_BASE;
    var hit_l = vec3<f32>(0.0);
    var hit_co = vec3<f32>(0.0);
    var miss_r = vec3<f32>(0.0);
    var miss_co = vec3<f32>(0.0);
    var origin = vec3<f32>(0.0);
    var n_hit = 0.0;
    var n_miss = 0.0;
    rc_tally(0u, 1u);
    for (var i = 0u; i < rays; i = i + 1u) {
        let g = i * stride + share;
        rc_tally(1u, 1u);
        let u = fract((f32(g) + 0.5) / f32(RC_RAYS) + noise2);
        let r = sqrt(u);
        let angle = f32(g) * 2.399963 + rotation;
        let dir = tangent * (r * cos(angle)) + bitangent * (r * sin(angle)) + n * sqrt(max(1.0 - u, 0.0));
        let dir_w = (to_world * vec4<f32>(dir, 0.0)).xyz;
        let co = rc_origin_dir(e, dir_w) * exposure;
        origin += co;
        // March: the last distance known to be free space, and the hit if one came.
        var last_free = 0.0;
        var hit = false;
        var hit_t = 0.0;
        var hit_pix = vec2<i32>(0);
        var prev_t = 0.0;
        for (var k = 0u; k < steps; k = k + 1u) {
            let a = (f32(k) + 0.25 + 0.75 * noise) / f32(steps);
            let t = ray_m * a * sqrt(a);
            let q = start + dir * t;
            if (q.z >= -params.proj.z) {
                break;
            }
            let z_q = -q.z;
            let ndc = vec2<f32>(q.x * params.proj.x, q.y * params.proj.y) / z_q;
            let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
            if (any(uv <= vec2<f32>(0.0)) || any(uv >= vec2<f32>(1.0))) {
                // Off screen: nothing known past here.
                break;
            }
            let q_pix = clamp(vec2<i32>(uv * params.screen.xy), vec2<i32>(0), size - vec2<i32>(1));
            let d_q = textureLoad(depth_tex, q_pix, 0);
            rc_tally(2u, 1u);
            if (d_q >= 1.0 && sky_stop) {
                // In front of the sky: free space here, and nothing behind it to march toward.
                last_free = t;
                break;
            }
            if (d_q < 1.0) {
                let z_s = linear_depth(d_q);
                if (z_q > z_s + 0.002 * z_s + 0.01) {
                    // Behind the depth buffer: a surface when close behind it; further back the
                    // ray has passed behind an object and nothing past that is known.
                    if (z_q - z_s < max(2.0 * (t - prev_t), 0.15 + 0.01 * z_s)) {
                        hit = true;
                        hit_t = t;
                        hit_pix = q_pix;
                    }
                    break;
                }
            }
            last_free = t;
            prev_t = t;
        }
        if (hit) {
            let texel = textureLoad(hdr_tex, hit_pix, 0);
            // An emitter a fitted area light already carries (`EMITTER_ALPHA`) is not gathered twice.
            if (texel.a > 7.0) {
                continue;
            }
            let fade = 1.0 - (hit_t * hit_t) / (ray_m * ray_m);
            sum += (max(texel.rgb, vec3<f32>(0.0)) - co) * clamp(fade, 0.0, 1.0);
            hit_l += max(texel.rgb, vec3<f32>(0.0)) * clamp(fade, 0.0, 1.0);
            hit_co += co * clamp(fade, 0.0, 1.0);
            n_hit += 1.0;
        } else if (free_ok && (miss_cap > 0.0 || attributing) && last_free > 0.0) {
            let x = p_w + n_w * bias + dir_w * last_free;
            let jitter = fract(vec3<f32>(noise + f32(i) * 0.618034, noise2 + f32(i) * 0.754878, noise * noise2 + f32(i) * 0.569840));
            let cached = rc_read_at(x, dir_w, jitter, p_w, n_w);
            rc_tally(3u, 1u);
            if (cached.a > 0.0) {
                let co_free = rc_origin_dir(e_free, dir_w) * exposure;
                let read = min(cached.rgb * exposure, co_free * select(RC_TERM_MISS_CAP, miss_cap, miss_cap > 0.0));
                if (miss_cap > 0.0) {
                    sum += read - co_free;
                }
                miss_r += read;
                miss_co += co_free;
                n_miss += 1.0;
            }
        }
    }
    var bounce = sum / f32(rays) * RECEIVER_ALBEDO * rc.gather.z;
    // The probe light this pixel holds bounds how far the difference may take it down: the cube's
    // cosine integral toward the normal (`radiance_cache_update.wgsl`).
    let cube_total = e.px + e.nx + e.py + e.ny + e.pz + e.nz;
    let cube_e = rc_origin_dir(e, n_w) * 0.375 - rc_origin_dir(e, -n_w) * 0.125 + cube_total * 0.125;
    let held = max(cube_e, vec3<f32>(0.0)) * exposure * RECEIVER_ALBEDO;
    bounce = max(bounce, -rc.limits.x * held);
    if (attributing) {
        let k = 1.0 / f32(rays) * RECEIVER_ALBEDO * rc.gather.z;
        switch term {
            case RC_TERM_HIT_RADIANCE: { bounce = hit_l * k; }
            case RC_TERM_HIT_CORRECTION: { bounce = hit_co * k; }
            case RC_TERM_MISS_READ: { bounce = miss_r * k; }
            case RC_TERM_MISS_CORRECTION: { bounce = miss_co * k; }
            case RC_TERM_ORIGIN: { bounce = origin * k; }
            case RC_TERM_COVERAGE: { bounce = vec3<f32>(n_hit, n_miss, f32(rays)) / f32(rays); }
            case RC_TERM_CELLS: { bounce = cells / 8.0; }
            default: { bounce = vec3<f32>(0.0); }
        }
    }
    if (half) {
        return vec4<f32>(bounce, 0.0);
    }
    bounce *= front_transmittance(pix);
    // `base.a` is perceptual roughness for SSR.
    return vec4<f32>(max(max(base.rgb, vec3<f32>(0.0)) + bounce, vec3<f32>(0.0)), base.a);
}
