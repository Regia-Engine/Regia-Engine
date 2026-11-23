// SPDX-License-Identifier: MPL-2.0
// Reference path tracer kernels (`specs/27` § 1).
//
// Wavefront shape over a persistent per-pixel path state: `generate` writes camera rays,
// then `intersect` → `shade` run once per bounce, then `accumulate` folds the sample into the
// AOV sums. Software BVH traversal only (two levels: a TLAS over instances, a BLAS per mesh,
// plus an implicit min/max quadtree over the terrain heightfield), so lavapipe, RADV and
// NVIDIA run the same code. The direct BRDF is the forward pass's (`regia::pbr::brdf`).
//
// Algorithms (see `docs/provenance.md`): unidirectional path tracing with next-event
// estimation and the power heuristic (Veach 1997), Russian roulette; GGX visible-normal
// sampling (Heitz 2018); watertight ray/triangle (Woop et al. 2013); binned-SAH BVH layout (Wald
// 2007, PBRT 4 ch. 7); maximum-mipmap heightfield traversal (Tevs, Ihrke, Seidel 2008);
// piecewise-constant 2-D sampling of a tabulated sky (PBRT 4 ch. A.5/12.5); ray origin offset
// (Wächter & Binder, Ray Tracing Gems 2019 ch. 6); orthonormal basis (Duff et al. 2017);
// PCG hash (O'Neill 2014; Jarzynski & Olano 2020). v2: participating media by free-flight
// sampling of an analytic optical depth with one-sample spectral MIS over the colour channels
// (PBRT 4 § 14.2–14.3; Miller, Georgiev & Jarosz 2019 for the chromatic weights), Henyey &
// Greenstein 1941 and Rayleigh phase functions, anisotropic GGX visible-normal sampling
// (Heitz 2018), concentric disc lens sampling (Shirley & Chiu 1997), uniform cone sampling of
// sky bodies (PBRT 4 § A.5.4). Thin-dielectric transmission as a delta lobe weighted like the
// forward pass (`t · (1 − F) · base · volume`, KHR_materials_transmission/volume); the airless
// star field is the raster sky's (`outdoor_sky.wgsl` `star_field`).

#import regia::pbr::lighting
#import regia::pbr::brdf
#import regia::pbr::thin_film
#import regia::pbr::rough_transmission
#import regia::pbr::surface
#import regia::cloud_density

const PI: f32 = 3.14159265359;
const TAU: f32 = 6.28318530718;
const INF: f32 = 1e30;
const NONE: u32 = 0xffffffffu;
const BVH_STACK: u32 = 48u;
const HF_STACK: u32 = 64u;
// Invocations per workgroup of the kernels that trace (`intersect`, `shade`).
const TRACE_GROUP: u32 = 64u;
// Entries one invocation's traversal stack holds: the top level's `BVH_STACK`, then the
// instance's (a mesh's `BVH_STACK` or a heightfield's `HF_STACK`, never both at once).
const TRACE_SLOTS: u32 = 112u;

// AOV layers in the accumulation buffer (`AOV_COUNT` vec4 per pixel; see `output.rs`).
const AOV_COUNT: u32 = 11u;
const AOV_BEAUTY: u32 = 0u;
const AOV_ALBEDO: u32 = 1u;
const AOV_NORMAL_DEPTH: u32 = 2u;
const AOV_DIRECT: u32 = 3u;
const AOV_IND_DIFFUSE: u32 = 4u;
const AOV_IND_SPECULAR: u32 = 5u;
const AOV_EMISSIVE: u32 = 6u;
const AOV_VOLUMETRIC: u32 = 7u;
const AOV_SKY: u32 = 8u;
const AOV_SHADOW: u32 = 9u;
const AOV_BEAUTY_SQ: u32 = 10u;

const FLAG_SUN: u32 = 1u;
const FLAG_JITTER: u32 = 2u;
const FLAG_TERRAIN: u32 = 4u;
// Probe mode (`probe.rs`): every path slot is (probe, ray) instead of a pixel.
const FLAG_PROBE: u32 = 8u;
// Probe mode, geometry only: rays record their first hit and end (relocation rounds).
const FLAG_PROBE_GEOMETRY: u32 = 16u;

// Probe mode: separable light components (`probe.rs` `COMPONENTS`), each linear in its light.
const COMP_SUN: u32 = 0u;
const COMP_SKY: u32 = 1u;
const COMP_FILL: u32 = 2u;
const COMP_EMISSIVE: u32 = 3u;
// Probe accumulator layout (vec4s per probe): 12 SH sums (component × channel, coefficients
// L00, L1x, L1y, L1z), then stats, closest back face, closest front face, clamp sums (luminance
// removed, contributions clamped), then the moment texels.
const PROBE_STATS: u32 = 12u;
const PROBE_BACK: u32 = 13u;
const PROBE_FRONT: u32 = 14u;
const PROBE_CLAMP: u32 = 15u;
const PROBE_MOMENTS: u32 = 16u;
const SH_Y00: f32 = 0.2820948;
const SH_Y1: f32 = 0.4886025;

// Instance flags (`Instance::info2.y`).
const INST_ALPHA: u32 = 1u;
const INST_TRANSMIT: u32 = 2u;

const LIGHT_POINT: u32 = 0u;
const LIGHT_SPOT: u32 = 1u;
const LIGHT_SPHERE: u32 = 2u;
const LIGHT_TRIANGLE: u32 = 3u;

// vec4s per triangle in `tri_attr`: 3 normals with packed UVs, UV + BVH index, colours, then
// 3 tangents (xyz, bitangent sign in w).
const TRI_ATTR: u32 = 8u;
// f32 words per medium layer and per sky-body header in `env` (`pack.rs`).
const MEDIUM_WORDS: u32 = 16u;
const BODY_WORDS: u32 = 16u;
// Free-flight ranges are capped here (metres): an escaping ray through a layer that never
// thins would otherwise integrate to infinity.
const MEDIUM_FAR: f32 = 2.0e5;

struct Globals {
    // xyz eye, w tan(fov_y / 2).
    cam_pos: vec4<f32>,
    // xyz forward, w aspect (width / height).
    cam_fwd: vec4<f32>,
    // xyz right, w thin-lens aperture radius (0 = pinhole).
    cam_right: vec4<f32>,
    // xyz up, w focus distance along the view axis.
    cam_up: vec4<f32>,
    // width, height, max bounces, seed.
    dims: vec4<u32>,
    // local light count, TLAS root (NONE = empty scene), Russian-roulette start bounce, flags.
    counts: vec4<u32>,
    // xyz toward the sun, w cos(angular radius); w >= 1 is a delta (point) sun.
    sun_dir: vec4<f32>,
    // rgb disc radiance (delta sun: irradiance at normal incidence), w 1 - cos(angular radius).
    sun_l: vec4<f32>,
    // rgb uniform sky radiance, w mode (0 none, 1 uniform, 2 sky-view table).
    sky: vec4<f32>,
    // table width, table height, CDF width, CDF height.
    sky_dims: vec4<u32>,
    // offsets (f32 words in `env`) of the table, marginal CDF, conditional CDFs, CDF function.
    sky_offs: vec4<u32>,
    // xyz sun direction the table is relative to, w integral of the CDF function.
    sky_sun: vec4<f32>,
    // DFG table offset in `env`, DFG size, the BRDF energy model (0 legacy, 1 compensated:
    // `regia_render::BrdfEnergy`, ADR-2632), 0.
    dfg: vec4<u32>,
    // terrain origin x, origin z, sample spacing x, sample spacing z.
    terrain0: vec4<f32>,
    // terrain samples x, samples z, pyramid levels, material.
    terrain1: vec4<u32>,
    // pyramid level offsets (f32 words in `terrain`), 16 levels.
    terrain_lv: array<vec4<u32>, 4>,
    // x ray-offset scale (unused, reserved), y sky radiance scale, z probe mode: the `env` offset
    // of the coherent ray order table as an integer-valued f32 (0 = index order; ADR-862), w
    // reserved.
    misc: vec4<f32>,
    // Medium layer count, `env` offset of the layers, 1 with a cloud field, its `env` offset.
    media: vec4<u32>,
    // x escape distance of the non-aerial layers (0 = unbounded).
    media_f: vec4<f32>,
    // Sky body count, `env` offset of their headers.
    bodies: vec4<u32>,
    // x star peak radiance (0 = none), y the camera's pixel angle (radians).
    stars: vec4<f32>,
    // World → clip of the terrain view albedo (`PtViewAlbedo`, 0..1 depth).
    view_albedo_vp: mat4x4<f32>,
    // x its texture (NONE = off), y its normal texture (NONE = none), z all bounces (0/1).
    view_albedo: vec4<u32>,
    // xyz the view's eye.
    view_albedo_eye: vec4<f32>,
    // Probe mode: rays per probe, probes in this batch, `env` offset of the probe records
    // (xyz position, w global probe index), octahedral moment resolution.
    probe: vec4<u32>,
    // Probe mode: x moment distance clamp (metres), y moment lobe exponent, z luminance cap of
    // contributions seen without scattering, w of contributions after a scattering event
    // (cd/m², 0 = off; ADR-1096).
    probe_f: vec4<f32>,
    // The frame's extra directional lights (ADR-7254), delta directions: per slot xyz toward the
    // light (w = 1 when on), then rgb irradiance at normal incidence. Credited to the sun.
    extra_sun: array<vec4<f32>, 6>,
}

struct Pass {
    sample_index: u32,
    bounce: u32,
    // Compacted bounces (ADR-862): 0 every slot, no queues; 1 every slot, live paths appended
    // to the output queue; 2 the input queue's slots, live paths appended to the output queue.
    // Input queue `bounce & 1`, output `(bounce + 1) & 1`.
    queue: u32,
    // First workgroup of an adaptive-sampling dispatch (0 for a full-image dispatch): the
    // pixel index of invocation `i` is `first_group * 64 + i` (ADR-2433).
    first_group: u32,
}

struct Node {
    bmin: vec3<f32>,
    // Interior: index of the left child (the right child follows it). Leaf: first primitive.
    a: u32,
    bmax: vec3<f32>,
    // 0 = interior, else primitive count.
    count: u32,
}

struct Instance {
    // World → object rows.
    w2o0: vec4<f32>,
    w2o1: vec4<f32>,
    w2o2: vec4<f32>,
    // Object → world rows.
    o2w0: vec4<f32>,
    o2w1: vec4<f32>,
    o2w2: vec4<f32>,
    // kind (0 mesh, 1 heightfield), BLAS root node, first triangle, material.
    info: vec4<u32>,
    // first light index of its emissive triangles (NONE = not emissive), flags (INST_ALPHA
    // alpha-tested, INST_TRANSMIT thin transmissive), 0, 0.
    info2: vec4<u32>,
}

struct Material {
    base_color: vec4<f32>,
    // rgb emitted radiance (cd/m²).
    emissive: vec4<f32>,
    // metallic, perceptual roughness, normal scale, alpha cutoff.
    params: vec4<f32>,
    // clearcoat, clearcoat roughness, multi-scatter (0/1), model (0 standard, 1 unlit, 2 ideal
    // Lambertian for validation scenes).
    coat: vec4<f32>,
    // albedo texture, normal texture, emissive texture (NONE = no texture), alpha mode.
    tex: vec4<u32>,
    // anisotropy, anisotropy rotation, packed ORM (0/1), one-sided emission (0/1).
    extra: vec4<f32>,
    // detail albedo / normal / roughness strength, fade start.
    detail: vec4<f32>,
    // detail UV scale, fade end, detail on (0/1).
    detail2: vec4<f32>,
    // ORM texture, detail image (RGB albedo, A roughness), detail normal (A mask), thin film map
    // (R coverage, G thickness).
    tex2: vec4<u32>,
    // transmission (KHR_materials_transmission), dielectric F0 (from the IOR), rough transmission's
    // perceptual roughness (ADR-2075; 0 goes straight through), 0.
    transmit: vec4<f32>,
    // rgb volume transmittance at normal incidence (KHR_materials_volume), 0.
    volume: vec4<f32>,
    // Thin film (KHR_materials_iridescence): coverage (0 = none), IOR, thickness at map 0 and 1 (nm).
    film: vec4<f32>,
    // Material graph (ADR-2725): graph id (a composed graph when < PT_GRAPHS), word offset of its
    // block in `textures`, 0, 0.
    graph: vec4<u32>,
    // Subsurface (ADR-2725): Burley's d per channel (m), on (0/1).
    sss: vec4<f32>,
    // Subsurface: scattered share per channel, 0.
    sss_tint: vec4<f32>,
}

struct Light {
    // xyz position.
    pos: vec4<f32>,
    // xyz spot direction (from the light into the scene), w sphere radius.
    dir: vec4<f32>,
    // rgb intensity (cd, point/spot) or radiance (sphere/triangle), w selection probability.
    color: vec4<f32>,
    // spot inner cos, spot outer cos, inclusive selection CDF, 0.
    extra: vec4<f32>,
    // triangle: instance, global triangle; kind; 0.
    refs: vec4<u32>,
}

struct Path {
    // xyz origin, w solid-angle pdf of the direction (0 = camera ray or delta).
    origin: vec4<f32>,
    // xyz direction, w alive (1/0).
    dir: vec4<f32>,
    throughput: vec4<f32>,
    // Share of everything after the first bounce credited to indirect diffuse (per channel);
    // w = 1 when the first path vertex is a medium scattering event.
    frac: vec4<f32>,
    direct: vec4<f32>,
    ind_d: vec4<f32>,
    ind_s: vec4<f32>,
    emis: vec4<f32>,
    sky: vec4<f32>,
    // rgb first-hit base colour, w sun visibility at the first hit.
    albedo: vec4<f32>,
    // xyz first-hit shading normal, w view depth.
    normal: vec4<f32>,
    // Radiance of paths whose first vertex is in a medium (the `volumetric` AOV).
    vol: vec4<f32>,
}

struct Hit {
    t: f32,
    inst: u32,
    prim: u32,
    u: f32,
    v: f32,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<uniform> pass_params: Pass;
@group(0) @binding(2) var<storage, read> nodes: array<Node>;
@group(0) @binding(3) var<storage, read> tri_pos: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> tri_attr: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read> instances: array<Instance>;
@group(0) @binding(6) var<storage, read> materials: array<Material>;
@group(0) @binding(7) var<storage, read> textures: array<u32>;
@group(0) @binding(8) var<storage, read> lights: array<Light>;
@group(0) @binding(9) var<storage, read> env: array<f32>;
@group(0) @binding(10) var<storage, read> terrain: array<f32>;
@group(0) @binding(11) var<storage, read_write> paths: array<Path>;
@group(0) @binding(12) var<storage, read_write> hits: array<vec4<f32>>;
@group(0) @binding(13) var<storage, read_write> accum: array<vec4<f32>>;
// Compacted bounces (ADR-862): two queues of live path slots, `pixel_count()` entries each, and
// `queue_count`: the two queue lengths, then the next indirect dispatch's workgroup counts
// (words 2..5, copied by the host into its indirect buffer).
@group(0) @binding(14) var<storage, read_write> queues: array<u32>;
@group(0) @binding(15) var<storage, read_write> queue_count: array<atomic<u32>>;

// The path `shade` works on, one variable per member of `Path` (ADR-7879): a Function or Private
// variable of a struct type has an explicit layout in naga's SPIR-V, which
// `VUID-StandaloneSpirv-None-10684` forbids. `load_path` reads a slot, `store_path` writes it back.
var<private> path_origin: vec4<f32>;
var<private> path_dir: vec4<f32>;
var<private> path_throughput: vec4<f32>;
var<private> path_frac: vec4<f32>;
var<private> path_direct: vec4<f32>;
var<private> path_ind_d: vec4<f32>;
var<private> path_ind_s: vec4<f32>;
var<private> path_emis: vec4<f32>;
var<private> path_sky: vec4<f32>;
var<private> path_albedo: vec4<f32>;
var<private> path_normal: vec4<f32>;
var<private> path_vol: vec4<f32>;

fn load_path(idx: u32) {
    let p = paths[idx];
    path_origin = p.origin;
    path_dir = p.dir;
    path_throughput = p.throughput;
    path_frac = p.frac;
    path_direct = p.direct;
    path_ind_d = p.ind_d;
    path_ind_s = p.ind_s;
    path_emis = p.emis;
    path_sky = p.sky;
    path_albedo = p.albedo;
    path_normal = p.normal;
    path_vol = p.vol;
}

fn store_path(idx: u32) {
    paths[idx] = Path(
        path_origin,
        path_dir,
        path_throughput,
        path_frac,
        path_direct,
        path_ind_d,
        path_ind_s,
        path_emis,
        path_sky,
        path_albedo,
        path_normal,
        path_vol,
    );
}

// ---------------------------------------------------------------------------------------------
// Random numbers: a 32-bit-state PCG (LCG step + RXS-M-XS output) seeded per
// (scene seed, pixel, sample, bounce); draws advance the stream in a fixed order.

var<private> rng_state: u32;

// The traversal stacks of the workgroup's invocations, interleaved: entry `i` of invocation
// `trace_lane` is `trace_stack[i * TRACE_GROUP + trace_lane]` (`stack_slot`). A Function array
// would be a variable with an explicit layout, which `VUID-StandaloneSpirv-None-10684` forbids
// (ADR-7587, ADR-7879). No barrier: each invocation touches only its own entries. Length
// `TRACE_SLOTS * TRACE_GROUP` (28 KiB: above WebGPU's 16 KiB floor, inside every desktop
// adapter's limit; `tracer.rs` checks the device).
var<workgroup> trace_stack: array<u32, 7168>;
// `local_invocation_index` of this invocation (set by `intersect` and `shade` before they trace).
var<private> trace_lane: u32;

// The `trace_stack` index of stack entry `i` of this invocation.
fn stack_slot(i: u32) -> u32 {
    return i * TRACE_GROUP + trace_lane;
}

fn pcg_hash(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn rng_init(pixel: u32, sample_index: u32, stream: u32) {
    rng_state = pcg_hash(globals.dims.w ^ pcg_hash(pixel ^ pcg_hash(sample_index ^ pcg_hash(stream + 0x9e3779b9u))));
}

fn rand() -> f32 {
    rng_state = rng_state * 747796405u + 2891336453u;
    let s = rng_state;
    let word = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    let out = (word >> 22u) ^ word;
    return f32(out >> 8u) * (1.0 / 16777216.0);
}

fn rand2() -> vec2<f32> {
    let a = rand();
    let b = rand();
    return vec2<f32>(a, b);
}

// ---------------------------------------------------------------------------------------------
// Small helpers.

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn max3(v: vec3<f32>) -> f32 {
    return max(v.x, max(v.y, v.z));
}

fn finite(v: vec3<f32>) -> bool {
    return all((bitcast<vec3<u32>>(v) << vec3<u32>(1u)) < vec3<u32>(0xff000000u));
}

fn power_heuristic(a: f32, b: f32) -> f32 {
    let a2 = a * a;
    let b2 = b * b;
    if (a2 + b2 <= 0.0) {
        return 0.0;
    }
    return a2 / (a2 + b2);
}

fn safe_inv(d: vec3<f32>) -> vec3<f32> {
    let big = vec3<f32>(INF);
    let s = select(vec3<f32>(1.0), vec3<f32>(-1.0), d < vec3<f32>(0.0));
    return select(1.0 / d, big * s, abs(d) < vec3<f32>(1e-20));
}

// Duff et al. 2017, branchless ONB around a unit normal.
fn onb(n: vec3<f32>) -> mat3x3<f32> {
    let sgn = select(-1.0, 1.0, n.z >= 0.0);
    let a = -1.0 / (sgn + n.z);
    let b = n.x * n.y * a;
    let t = vec3<f32>(1.0 + sgn * n.x * n.x * a, sgn * b, -sgn * n.x);
    let bt = vec3<f32>(b, sgn + n.y * n.y * a, -n.y);
    return mat3x3<f32>(t, bt, n);
}

// Wächter & Binder 2019: move a hit point off its surface along the geometric normal
// by a few ULPs (far from the origin) or a tiny absolute step (near it).
fn offset_ray(p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let origin = 1.0 / 32.0;
    let float_scale = 1.0 / 65536.0;
    let int_scale = 256.0;
    let of_i = vec3<i32>(int_scale * n);
    let pi = vec3<f32>(
        bitcast<f32>(bitcast<i32>(p.x) + select(of_i.x, -of_i.x, p.x < 0.0)),
        bitcast<f32>(bitcast<i32>(p.y) + select(of_i.y, -of_i.y, p.y < 0.0)),
        bitcast<f32>(bitcast<i32>(p.z) + select(of_i.z, -of_i.z, p.z < 0.0)),
    );
    return vec3<f32>(
        select(pi.x, p.x + float_scale * n.x, abs(p.x) < origin),
        select(pi.y, p.y + float_scale * n.y, abs(p.y) < origin),
        select(pi.z, p.z + float_scale * n.z, abs(p.z) < origin),
    );
}

fn xform_point(r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>, p: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(r0.xyz, p) + r0.w, dot(r1.xyz, p) + r1.w, dot(r2.xyz, p) + r2.w);
}

fn xform_dir(r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>, d: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(r0.xyz, d), dot(r1.xyz, d), dot(r2.xyz, d));
}

fn pixel_count() -> u32 {
    return globals.dims.x * globals.dims.y;
}

fn probe_mode() -> bool {
    return (globals.counts.w & FLAG_PROBE) != 0u;
}

// Probe `p` of the batch: xyz position, w global probe index (exact in f32 below 2^24).
fn probe_record(p: u32) -> vec4<f32> {
    let o = globals.probe.z + p * 4u;
    return vec4<f32>(env[o], env[o + 1u], env[o + 2u], env[o + 3u]);
}

// The random stream of a path slot: the pixel, or in probe mode (global probe, ray), so a
// bake does not depend on how the probes were split into batches.
fn rng_slot(idx: u32) -> u32 {
    if (probe_mode()) {
        let rays = globals.probe.x;
        return u32(probe_record(idx / rays).w) * rays + probe_ray_index(idx % rays);
    }
    return idx;
}

// The Fibonacci index a probe ray slot traces. In the coherent order (ADR-862, `probe_rays.rs`)
// a table sorts the set along a Morton curve so neighboring invocations trace neighboring
// directions; the slot's random stream follows the index, so each ray's path is the same in
// either order.
fn probe_ray_index(slot: u32) -> u32 {
    let order = u32(globals.misc.z);
    if (order == 0u) {
        return slot;
    }
    return u32(env[order + slot]);
}

fn invocation_index(gid: vec3<u32>, nwg: vec3<u32>) -> u32 {
    return gid.x + gid.y * nwg.x * 64u + pass_params.first_group * 64u;
}

// The path slot invocation `i` of an intersect or shade dispatch works on: `i` itself, or on a
// compacted bounce the `i`-th entry of the input queue (NONE past its end).
fn queued_slot(i: u32) -> u32 {
    if (i >= pixel_count()) {
        return NONE;
    }
    if (pass_params.queue < 2u) {
        return i;
    }
    let q = pass_params.bounce & 1u;
    if (i >= atomicLoad(&queue_count[q])) {
        return NONE;
    }
    return queues[q * pixel_count() + i];
}

// Appends a path that goes on to the next bounce to the output queue (compacted traces only).
fn enqueue(slot: u32) {
    if (pass_params.queue == 0u) {
        return;
    }
    let q = (pass_params.bounce + 1u) & 1u;
    let at = atomicAdd(&queue_count[q], 1u);
    queues[q * pixel_count() + at] = slot;
}

// ---------------------------------------------------------------------------------------------
// Textures: RGBA8 texels in one storage buffer, 4-word headers first
// (texel offset, width, height, flags: bit 0 sRGB). Bilinear, repeat, mip 0.

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn texel(offset: u32, w: u32, h: u32, x: i32, y: i32, srgb: bool) -> vec4<f32> {
    let xi = u32(((x % i32(w)) + i32(w)) % i32(w));
    let yi = u32(((y % i32(h)) + i32(h)) % i32(h));
    let c = unpack4x8unorm(textures[offset + yi * w + xi]);
    if (srgb) {
        return vec4<f32>(srgb_to_linear(c.rgb), c.a);
    }
    return c;
}

fn tex_sample(index: u32, uv: vec2<f32>) -> vec4<f32> {
    let header = vec4<u32>(textures[index * 4u], textures[index * 4u + 1u], textures[index * 4u + 2u], textures[index * 4u + 3u]);
    let w = max(header.y, 1u);
    let h = max(header.z, 1u);
    let srgb = (header.w & 1u) != 0u;
    let fu = (uv.x - floor(uv.x)) * f32(w) - 0.5;
    let fv = (uv.y - floor(uv.y)) * f32(h) - 0.5;
    let x0 = floor(fu);
    let y0 = floor(fv);
    let tx = fu - x0;
    let ty = fv - y0;
    let xi = i32(x0);
    let yi = i32(y0);
    let a = texel(header.x, w, h, xi, yi, srgb);
    let b = texel(header.x, w, h, xi + 1, yi, srgb);
    let c = texel(header.x, w, h, xi, yi + 1, srgb);
    let d = texel(header.x, w, h, xi + 1, yi + 1, srgb);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

// ---- Material graphs (ADR-2725, `regia_pt::graph`) ----
// A graph material's block in `textures`: 16 vec4 parameter slots (raw f32 bits), then the seven
// graph texture slots as texture indices (NONE samples white, as the forward pass binds white).
// `pt_graph_base` is the block of the material being evaluated, set by `graph_surface`.
var<private> pt_graph_base: u32;

// `material.graph[i]` of the forward contract.
fn pt_graph_param(i: u32) -> vec4<f32> {
    let o = pt_graph_base + min(i, 15u) * 4u;
    return bitcast<vec4<f32>>(vec4<u32>(textures[o], textures[o + 1u], textures[o + 2u], textures[o + 3u]));
}

// `textureSample(<slot>, tex_samp, uv)`: the tracer has no pixel quad and no mip chain, so every
// graph sample is the bilinear top level (the fixed-LOD rule), like the standard materials'.
fn pt_graph_tex(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let t = textures[pt_graph_base + 64u + min(slot, 6u)];
    if (t == NONE) {
        return vec4<f32>(1.0);
    }
    return tex_sample(t, uv);
}

// `textureSampleLevel`: the level is ignored (no mip chain), see `pt_graph_tex`.
fn pt_graph_tex_level(slot: u32, uv: vec2<f32>, lod: f32) -> vec4<f32> {
    return pt_graph_tex(slot, uv);
}

// The composed graphs and their dispatch. The tracer replaces this region with the scene's graphs
// (`regia_pt::graph::composed_source`); as shipped it composes none, so no material takes the
// graph path.
// @pt_material_graphs_begin
const PT_GRAPHS: u32 = 0u;
fn pt_graph_dispatch(id: u32, in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::default_surface();
}
// @pt_material_graphs_end

// Projected decals (ADR-2951): `regia_pt::decal::composed_source` fills this region and the two
// below for a scene with decals; as shipped they hold nothing, so a decal-free scene traces this
// module unchanged.
// @pt_decal_fns_begin
// @pt_decal_fns_end

fn is_graph(m: Material) -> bool {
    return m.graph.x < PT_GRAPHS;
}

// The raster frame's view of world point `p` (`PtViewAlbedo`): `p` projected through the
// raster camera to NDC xy; z = 1 inside the image, 0 outside (or behind the camera).
fn view_ndc(p: vec3<f32>) -> vec3<f32> {
    let clip = globals.view_albedo_vp * vec4<f32>(p, 1.0);
    if (clip.w <= 0.0) {
        return vec3<f32>(0.0);
    }
    let ndc = clip.xy / clip.w;
    if (abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0) {
        return vec3<f32>(0.0);
    }
    return vec3<f32>(ndc, 1.0);
}

// Whether the raster camera sees heightfield point `p` (instance `inst`): no heightfield
// between it and the view's eye. The segment stops short of `p` by a margin that grows with
// the distance (the heightfield's own triangles around a grazing hit).
fn seen_from_view_eye(inst: u32, p: vec3<f32>) -> bool {
    let to_p = p - globals.view_albedo_eye.xyz;
    let dist = length(to_p);
    if (dist <= 1e-3) {
        return true;
    }
    let d = to_p / dist;
    let tmax = dist - (0.05 + 2e-3 * dist);
    if (tmax <= 0.0) {
        return true;
    }
    let h = trace_heightfield(inst, globals.view_albedo_eye.xyz, d, Hit(tmax, NONE, NONE, 0.0, 0.0), true);
    return h.inst == NONE;
}

// Texture `index` at NDC `ndc` of the raster view, bilinear over the covered texels only
// (alpha-weighted, clamped to the image, so a silhouette does not blend in the black of
// uncovered sky texels). Returns rgb and the covered weight (0 = no terrain there).
fn view_texture(index: u32, ndc: vec2<f32>) -> vec4<f32> {
    let header = vec4<u32>(textures[index * 4u], textures[index * 4u + 1u], textures[index * 4u + 2u], textures[index * 4u + 3u]);
    let w = max(header.y, 1u);
    let h = max(header.z, 1u);
    let srgb = (header.w & 1u) != 0u;
    let fu = clamp((0.5 + 0.5 * ndc.x) * f32(w) - 0.5, 0.0, f32(w - 1u));
    let fv = clamp((0.5 - 0.5 * ndc.y) * f32(h) - 0.5, 0.0, f32(h - 1u));
    let x0 = floor(fu);
    let y0 = floor(fv);
    let tx = fu - x0;
    let ty = fv - y0;
    let xi = i32(x0);
    let yi = i32(y0);
    let x1 = min(xi + 1, i32(w) - 1);
    let y1 = min(yi + 1, i32(h) - 1);
    let a = texel(header.x, w, h, xi, yi, srgb);
    let b = texel(header.x, w, h, x1, yi, srgb);
    let c = texel(header.x, w, h, xi, y1, srgb);
    let d = texel(header.x, w, h, x1, y1, srgb);
    let wa = (1.0 - tx) * (1.0 - ty) * a.a;
    let wb = tx * (1.0 - ty) * b.a;
    let wc = (1.0 - tx) * ty * c.a;
    let wd = tx * ty * d.a;
    let sum = wa + wb + wc + wd;
    if (sum <= 1e-4) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>((a.rgb * wa + b.rgb * wb + c.rgb * wc + d.rgb * wd) / sum, sum);
}

// Split-sum DFG table (the forward pass's `brdf_lut` bytes), bilinear with clamp-to-edge like
// the hardware sampler. x = n·v, y = perceptual roughness; (scale, bias, single-scatter albedo E,
// its hemispherical average E_avg).
fn dfg(no_v: f32, perceptual: f32) -> vec4<f32> {
    let n = globals.dfg.y;
    let fx = clamp(no_v * f32(n) - 0.5, 0.0, f32(n - 1u));
    let fy = clamp(perceptual * f32(n) - 0.5, 0.0, f32(n - 1u));
    let x0 = u32(floor(fx));
    let y0 = u32(floor(fy));
    let x1 = min(x0 + 1u, n - 1u);
    let y1 = min(y0 + 1u, n - 1u);
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let o = globals.dfg.x;
    let a = dfg_texel(o, y0 * n + x0);
    let b = dfg_texel(o, y0 * n + x1);
    let c = dfg_texel(o, y1 * n + x0);
    let d = dfg_texel(o, y1 * n + x1);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

fn dfg_texel(o: u32, i: u32) -> vec4<f32> {
    return vec4<f32>(env[o + i * 4u], env[o + i * 4u + 1u], env[o + i * 4u + 2u], env[o + i * 4u + 3u]);
}

// ---------------------------------------------------------------------------------------------
// Sky: the runtime's sky-view table (Hillaire 2020 parameterisation, the same mapping as
// `outdoor_sky.wgsl::sky_uv` and `atmosphere.rs::sky_uv`), an equirect HDRI (mode 3, the same
// table lookup through `equirect_uv`) or a uniform radiance; sampled with a piecewise-constant
// 2-D distribution over an equirect grid.

fn sky_uv(view: vec3<f32>, sun: vec3<f32>) -> vec2<f32> {
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let sun_h = sun - up * dot(sun, up);
    let view_h = view - up * dot(view, up);
    let sun_n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(sun_h), length(sun_h) > 1e-4);
    let view_n = select(up, normalize(view_h), length(view_h) > 1e-4);
    let sin_az = dot(cross(sun_n, view_n), up);
    let cos_az = clamp(dot(sun_n, view_n), -1.0, 1.0);
    let azimuth = atan2(sin_az, cos_az);
    let uu = fract(azimuth / TAU + 0.5);
    let lat = asin(clamp(view.y, -1.0, 1.0));
    let vv = 0.5 + 0.5 * sign(lat) * sqrt(abs(lat) / (0.5 * PI));
    return vec2<f32>(uu, clamp(vv, 0.0, 1.0));
}

fn sky_table_texel(x: u32, y: u32) -> vec3<f32> {
    let i = globals.sky_offs.x + (y * globals.sky_dims.x + x) * 4u;
    return vec3<f32>(env[i], env[i + 1u], env[i + 2u]);
}

// Bilinear, U wrapped and V clamped — `LutImage::sample`.
fn sky_table(uv: vec2<f32>) -> vec3<f32> {
    let w = f32(globals.sky_dims.x);
    let h = f32(globals.sky_dims.y);
    let fu = fract(uv.x) * w - 0.5;
    let fv = clamp(uv.y, 0.0, 0.999) * h - 0.5;
    let x0 = floor(fu);
    let y0 = floor(fv);
    let tx = fu - x0;
    let ty = fv - y0;
    let wi = globals.sky_dims.x;
    let xa = u32(((i32(x0) % i32(wi)) + i32(wi)) % i32(wi));
    let xb = (xa + 1u) % wi;
    let ya = u32(clamp(y0, 0.0, h - 1.0));
    let yb = u32(clamp(y0 + 1.0, 0.0, h - 1.0));
    let a = sky_table_texel(xa, ya);
    let b = sky_table_texel(xb, ya);
    let c = sky_table_texel(xa, yb);
    let d = sky_table_texel(xb, yb);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

fn sky_mode() -> u32 {
    return u32(globals.sky.w + 0.5);
}

// Equirect map coordinates of `d` (`env_map.rs::equirect_uv`): the map's yaw is undone with
// (cos, sin) in `sky_sun.xy`; row 0 is the zenith, the centre column looks along −Z and u grows
// toward +X.
fn equirect_uv(d: vec3<f32>) -> vec2<f32> {
    let c = globals.sky_sun.x;
    let s = globals.sky_sun.y;
    let x = d.x * c - d.z * s;
    let z = d.x * s + d.z * c;
    let u = fract(0.5 + atan2(x, -z) / TAU);
    let v = acos(clamp(d.y, -1.0, 1.0)) / PI;
    return vec2<f32>(u, v);
}

fn sky_radiance(d: vec3<f32>) -> vec3<f32> {
    let mode = sky_mode();
    if (mode == 1u) {
        return globals.sky.rgb;
    }
    if (mode == 2u) {
        return sky_table(sky_uv(d, globals.sky_sun.xyz)) * globals.misc.y;
    }
    if (mode == 3u) {
        return sky_table(equirect_uv(d)) * globals.misc.y;
    }
    return vec3<f32>(0.0);
}

fn equirect_dir(u: f32, v: f32) -> vec3<f32> {
    let theta = v * PI;
    let phi = u * TAU;
    let s = sin(theta);
    return vec3<f32>(s * cos(phi), cos(theta), s * sin(phi));
}

fn find_interval(offset: u32, n: u32, u: f32) -> u32 {
    var lo = 0u;
    var hi = n;
    loop {
        if (hi - lo <= 1u) {
            break;
        }
        let mid = (lo + hi) / 2u;
        if (env[offset + mid] <= u) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    return lo;
}

fn sky_pdf(d: vec3<f32>) -> f32 {
    let integral = globals.sky_sun.w;
    if (sky_mode() == 0u || integral <= 0.0) {
        return 0.0;
    }
    let theta = acos(clamp(d.y, -1.0, 1.0));
    let s = sin(theta);
    if (s <= 1e-6) {
        return 0.0;
    }
    var phi = atan2(d.z, d.x);
    if (phi < 0.0) {
        phi = phi + TAU;
    }
    let w = globals.sky_dims.z;
    let h = globals.sky_dims.w;
    let iu = min(u32(phi / TAU * f32(w)), w - 1u);
    let iv = min(u32(theta / PI * f32(h)), h - 1u);
    let f = env[globals.sky_offs.w + iv * w + iu];
    return f / integral / (2.0 * PI * PI * s);
}

// Returns the direction in xyz and its solid-angle pdf in w (0 = no sample).
fn sample_sky(xi: vec2<f32>) -> vec4<f32> {
    let integral = globals.sky_sun.w;
    if (integral <= 0.0) {
        return vec4<f32>(0.0);
    }
    let w = globals.sky_dims.z;
    let h = globals.sky_dims.w;
    let marg = globals.sky_offs.y;
    let row = find_interval(marg, h, xi.y);
    let c0 = env[marg + row];
    let c1 = env[marg + row + 1u];
    let dv = select(0.5, (xi.y - c0) / (c1 - c0), c1 > c0);
    let cond = globals.sky_offs.z + row * (w + 1u);
    let col = find_interval(cond, w, xi.x);
    let r0 = env[cond + col];
    let r1 = env[cond + col + 1u];
    let du = select(0.5, (xi.x - r0) / (r1 - r0), r1 > r0);
    let u = (f32(col) + clamp(du, 0.0, 1.0)) / f32(w);
    let v = (f32(row) + clamp(dv, 0.0, 1.0)) / f32(h);
    let dir = equirect_dir(u, v);
    let s = sin(v * PI);
    if (s <= 1e-6) {
        return vec4<f32>(0.0);
    }
    let f = env[globals.sky_offs.w + row * w + col];
    return vec4<f32>(dir, f / integral / (2.0 * PI * PI * s));
}

// ---------------------------------------------------------------------------------------------
// Sun: a disc of the preset's angular radius (uniform cone) or a delta direction.

fn sun_on() -> bool {
    return (globals.counts.w & FLAG_SUN) != 0u;
}

fn sun_is_delta() -> bool {
    return globals.sun_dir.w >= 1.0;
}

fn sun_cone_pdf() -> f32 {
    return 1.0 / (TAU * max(globals.sun_l.w, 1e-12));
}

fn sun_disc(d: vec3<f32>) -> vec3<f32> {
    if (!sun_on() || sun_is_delta()) {
        return vec3<f32>(0.0);
    }
    if (dot(d, globals.sun_dir.xyz) >= globals.sun_dir.w) {
        return globals.sun_l.rgb;
    }
    return vec3<f32>(0.0);
}

// Uniform direction in the cone around `axis` with 1 - cos(half angle) = `omc`.
fn sample_cone(axis: vec3<f32>, omc: f32, xi: vec2<f32>) -> vec3<f32> {
    let one_minus = xi.x * omc;
    let cos_t = 1.0 - one_minus;
    let sin_t = sqrt(max(one_minus * (2.0 - one_minus), 0.0));
    let phi = TAU * xi.y;
    let frame = onb(axis);
    return normalize(frame[0] * (sin_t * cos(phi)) + frame[1] * (sin_t * sin(phi)) + axis * cos_t);
}

// ---------------------------------------------------------------------------------------------
// Traversal.

fn slab(bmin: vec3<f32>, bmax: vec3<f32>, o: vec3<f32>, inv: vec3<f32>, tmax: f32) -> f32 {
    let t0 = (bmin - o) * inv;
    let t1 = (bmax - o) * inv;
    let tn = min(t0, t1);
    let tf = max(t0, t1);
    let enter = max(max(tn.x, tn.y), max(tn.z, 0.0));
    let leave = min(min(tf.x, tf.y), tf.z);
    if (enter <= leave && enter < tmax) {
        return enter;
    }
    return INF;
}

// Watertight ray/triangle test (Woop, Benthin, Wald 2013, JCGT 2(1)): shear the triangle
// into the ray's frame and evaluate the three 2-D edge functions, so a ray through a shared edge
// or vertex hits at least one of the triangles (Möller–Trumbore lets such rays slip through).
// The edge functions take a few-ulp tolerance (see `ray_tri`) because GPU compilers may fuse
// their multiply-adds.
struct WtRay {
    o: vec3<f32>,
    k: vec3<u32>,
    s: vec3<f32>,
}

fn wt_ray(o: vec3<f32>, d: vec3<f32>) -> WtRay {
    let a = abs(d);
    var kz = 2u;
    if (a.x > a.y && a.x > a.z) {
        kz = 0u;
    } else if (a.y > a.z) {
        kz = 1u;
    }
    var kx = (kz + 1u) % 3u;
    var ky = (kx + 1u) % 3u;
    if (d[kz] < 0.0) {
        let t = kx;
        kx = ky;
        ky = t;
    }
    let sz = 1.0 / d[kz];
    return WtRay(o, vec3<u32>(kx, ky, kz), vec3<f32>(d[kx] * sz, d[ky] * sz, sz));
}

// γ(n) = n·u / (1 − n·u) with u = 2^-24, the relative error bound of n rounded f32 operations
// (PBRT 3rd ed. § 3.9.1), for `ray_tri`'s conservative t.
const F32_UNIT_ROUNDOFF: f32 = 5.9604645e-8;
const GAMMA2: f32 = 2.0 * F32_UNIT_ROUNDOFF / (1.0 - 2.0 * F32_UNIT_ROUNDOFF);
const GAMMA3: f32 = 3.0 * F32_UNIT_ROUNDOFF / (1.0 - 3.0 * F32_UNIT_ROUNDOFF);
const GAMMA5: f32 = 5.0 * F32_UNIT_ROUNDOFF / (1.0 - 5.0 * F32_UNIT_ROUNDOFF);

// Returns (t, u, v) with u, v the weights of p1 and p2; t = INF on a miss.
fn ray_tri(r: WtRay, p0: vec3<f32>, p1: vec3<f32>, p2: vec3<f32>, tmax: f32) -> vec3<f32> {
    let a = p0 - r.o;
    let b = p1 - r.o;
    let c = p2 - r.o;
    let ax = a[r.k.x] - r.s.x * a[r.k.z];
    let ay = a[r.k.y] - r.s.y * a[r.k.z];
    let bx = b[r.k.x] - r.s.x * b[r.k.z];
    let by = b[r.k.y] - r.s.y * b[r.k.z];
    let cx = c[r.k.x] - r.s.x * c[r.k.z];
    let cy = c[r.k.y] - r.s.y * c[r.k.z];
    let eu = cx * by - cy * bx;
    let ev = ax * cy - ay * cx;
    let ew = bx * ay - by * ax;
    // The exact test needs each edge function rounded the same way in both triangles that
    // share the edge. Drivers that contract `x * y - z * w` into a fused multiply-add (NVIDIA)
    // round them differently, so a ray along a shared edge was rejected by both triangles: a
    // crack (the diagonal of every quad). A few-ulp tolerance relative to the products closes
    // it; a ray that close to the edge may hit both triangles at the same t, which is harmless.
    let tu = 4e-7 * (abs(cx * by) + abs(cy * bx));
    let tv = 4e-7 * (abs(ax * cy) + abs(ay * cx));
    let tw = 4e-7 * (abs(bx * ay) + abs(by * ax));
    if ((eu < -tu || ev < -tv || ew < -tw) && (eu > tu || ev > tv || ew > tw)) {
        return vec3<f32>(INF, 0.0, 0.0);
    }
    let det = eu + ev + ew;
    if (det == 0.0) {
        return vec3<f32>(INF, 0.0, 0.0);
    }
    let az = r.s.z * a[r.k.z];
    let bz = r.s.z * b[r.k.z];
    let cz = r.s.z * c[r.k.z];
    let t = (eu * az + ev * bz + ew * cz) / det;
    if (!(t > 0.0) || t >= tmax) {
        return vec3<f32>(INF, 0.0, 0.0);
    }
    // A hit closer than this computation's own rounding error is the surface the ray left:
    // `t` weighs vertex depths that can lie a kilometre away on both sides of the origin, and
    // its error then exceeds `offset_ray`'s lift (a bounce off a 2 km ground "hit" the ground
    // again). Conservative bound of Pharr, Jakob & Humphreys, PBRT 3rd ed. § 3.9.6 (ADR-1411;
    // CPU mirror in `gpu_ray_mirror.rs`).
    let max_zt = max(max(abs(az), abs(bz)), abs(cz));
    let max_xt = max(max(abs(ax), abs(bx)), abs(cx));
    let max_yt = max(max(abs(ay), abs(by)), abs(cy));
    let delta_z = GAMMA3 * max_zt;
    let delta_x = GAMMA5 * (max_xt + max_zt);
    let delta_y = GAMMA5 * (max_yt + max_zt);
    let delta_e = 2.0 * (GAMMA2 * max_xt * max_yt + delta_y * max_xt + delta_x * max_yt);
    let max_e = max(max(abs(eu), abs(ev)), abs(ew));
    let delta_t = 3.0 * (GAMMA3 * max_e * max_zt + delta_e * max_zt + delta_z * max_e) / abs(det);
    if (t <= delta_t) {
        return vec3<f32>(INF, 0.0, 0.0);
    }
    // Inside the tolerance band the weights may dip a hair below 0.
    let u = clamp(ev / det, 0.0, 1.0);
    let v = clamp(ew / det, 0.0, 1.0 - u);
    return vec3<f32>(t, u, v);
}

fn tri_uv(tri: u32, u: f32, v: f32) -> vec2<f32> {
    let a0 = tri_attr[tri * TRI_ATTR];
    let a1 = tri_attr[tri * TRI_ATTR + 1u];
    let a2 = tri_attr[tri * TRI_ATTR + 2u];
    let a3 = tri_attr[tri * TRI_ATTR + 3u];
    let uv0 = vec2<f32>(a0.w, a1.w);
    let uv1 = vec2<f32>(a2.w, a3.x);
    let uv2 = vec2<f32>(a3.y, a3.z);
    return uv0 * (1.0 - u - v) + uv1 * u + uv2 * v;
}

// Alpha test for masked materials: the hit counts when base alpha × texture alpha × vertex
// alpha reaches the cutoff (blend modes use 0.5).
fn alpha_passes(inst: u32, tri: u32, u: f32, v: f32) -> bool {
    let m = materials[instances[inst].info.w];
    if (is_graph(m)) {
        // A graph's alpha is its own output (the forward pass tests `graph.alpha` alone). Any hit
        // has no view direction: the surface normal stands in for it.
        let s = surface_at(Hit(0.0, inst, tri, u, v));
        let g = graph_surface(m, s, inst, s.ns, s.ns, 1.0);
        return g.alpha >= select(0.5, m.params.w, m.tex.w == 1u);
    }
    var alpha = m.base_color.a;
    if (m.tex.x != NONE) {
        alpha = alpha * tex_sample(m.tex.x, tri_uv(tri, u, v)).a;
    }
    let a4 = tri_attr[tri * TRI_ATTR + 4u];
    let c0 = unpack4x8unorm(bitcast<u32>(a4.x)).a;
    let c1 = unpack4x8unorm(bitcast<u32>(a4.y)).a;
    let c2 = unpack4x8unorm(bitcast<u32>(a4.z)).a;
    alpha = alpha * (c0 * (1.0 - u - v) + c1 * u + c2 * v);
    let cutoff = select(0.5, m.params.w, m.tex.w == 1u);
    return alpha >= cutoff;
}

// The closest hit so far is one variable per member of `Hit` (ADR-7879). The stack is the
// instance's part of `trace_stack`, from entry `BVH_STACK`.
fn trace_blas(inst: u32, o: vec3<f32>, d: vec3<f32>, hit_in: Hit, any_hit: bool) -> Hit {
    var hit_t = hit_in.t;
    var hit_inst = hit_in.inst;
    var hit_prim = hit_in.prim;
    var hit_u = hit_in.u;
    var hit_v = hit_in.v;
    let info = instances[inst].info;
    let alpha_tested = (instances[inst].info2.y & INST_ALPHA) != 0u;
    let inv = safe_inv(d);
    let wr = wt_ray(o, d);
    var sp = 0u;
    var node_idx = info.y;
    loop {
        let node = nodes[node_idx];
        if (node.count > 0u) {
            for (var k = 0u; k < node.count; k = k + 1u) {
                let tri = node.a + k;
                let p0 = tri_pos[tri * 3u].xyz;
                let p1 = tri_pos[tri * 3u + 1u].xyz;
                let p2 = tri_pos[tri * 3u + 2u].xyz;
                let r = ray_tri(wr, p0, p1, p2, hit_t);
                if (r.x < hit_t) {
                    if (alpha_tested && !alpha_passes(inst, tri, r.y, r.z)) {
                        continue;
                    }
                    hit_t = r.x;
                    hit_inst = inst;
                    hit_prim = tri;
                    hit_u = r.y;
                    hit_v = r.z;
                    if (any_hit) {
                        return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
                    }
                }
            }
            if (sp == 0u) {
                break;
            }
            sp = sp - 1u;
            node_idx = trace_stack[stack_slot(BVH_STACK + sp)];
            continue;
        }
        let l = node.a;
        let r = node.a + 1u;
        let nl = nodes[l];
        let nr = nodes[r];
        let tl = slab(nl.bmin, nl.bmax, o, inv, hit_t);
        let tr = slab(nr.bmin, nr.bmax, o, inv, hit_t);
        if (tl < INF && tr < INF) {
            var near = l;
            var far = r;
            if (tr < tl) {
                near = r;
                far = l;
            }
            if (sp < BVH_STACK) {
                trace_stack[stack_slot(BVH_STACK + sp)] = far;
                sp = sp + 1u;
            }
            node_idx = near;
        } else if (tl < INF) {
            node_idx = l;
        } else if (tr < INF) {
            node_idx = r;
        } else {
            if (sp == 0u) {
                break;
            }
            sp = sp - 1u;
            node_idx = trace_stack[stack_slot(BVH_STACK + sp)];
        }
    }
    return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
}

// Heightfield: samples `terrain[j * nx + i]` at (x0 + i·dx, z0 + j·dz). Each cell is split along
// the (i, j+1)–(i+1, j) diagonal like the CDLOD grid: (p00, p01, p10) and (p10, p01, p11).
fn hf_height(i: u32, j: u32) -> f32 {
    return terrain[j * globals.terrain1.x + i];
}

// A cut sample (a hole: `PtHeightfield::cut_holes`, `CUT_HEIGHT` in pack.rs) is far below the
// map; every cell with a cut corner is dropped.
const HF_CUT: f32 = -1.0e8;

// Height of sample (i, j), or `fallback` where the sample is cut.
fn hf_height_or(i: u32, j: u32, fallback: f32) -> f32 {
    let h = hf_height(i, j);
    return select(h, fallback, h < HF_CUT);
}

fn hf_point(i: u32, j: u32) -> vec3<f32> {
    return vec3<f32>(
        globals.terrain0.x + f32(i) * globals.terrain0.z,
        hf_height(i, j),
        globals.terrain0.y + f32(j) * globals.terrain0.w,
    );
}

fn hf_level_dims(level: u32) -> vec2<u32> {
    let cw = globals.terrain1.x - 1u;
    let ch = globals.terrain1.y - 1u;
    let step = 1u << level;
    return vec2<u32>((cw + step - 1u) >> level, (ch + step - 1u) >> level);
}

fn hf_level_offset(level: u32) -> u32 {
    return globals.terrain_lv[level / 4u][level % 4u];
}

fn hf_box_t(level: u32, ci: u32, cj: u32, o: vec3<f32>, inv: vec3<f32>, tmax: f32) -> f32 {
    let dims = hf_level_dims(level);
    let off = hf_level_offset(level) + 2u * (cj * dims.x + ci);
    let lo_h = terrain[off];
    let hi_h = terrain[off + 1u];
    let i0 = ci << level;
    let j0 = cj << level;
    let i1 = min((ci + 1u) << level, globals.terrain1.x - 1u);
    let j1 = min((cj + 1u) << level, globals.terrain1.y - 1u);
    let bmin = vec3<f32>(
        globals.terrain0.x + f32(i0) * globals.terrain0.z,
        lo_h,
        globals.terrain0.y + f32(j0) * globals.terrain0.w,
    );
    let bmax = vec3<f32>(
        globals.terrain0.x + f32(i1) * globals.terrain0.z,
        hi_h,
        globals.terrain0.y + f32(j1) * globals.terrain0.w,
    );
    return slab(bmin, bmax, o, inv, tmax);
}

// Component indices, for writes through a lane mask (`select(v, x, LANES == vec4<u32>(i))`).
const LANES: vec4<u32> = vec4<u32>(0u, 1u, 2u, 3u);

fn hf_pack(level: u32, ci: u32, cj: u32) -> u32 {
    return (level << 28u) | (ci << 14u) | cj;
}

// As `trace_blas`: the hit one variable per member, the stack from entry `BVH_STACK` of
// `trace_stack`. The children's sort keys are `vec4`s written through a lane mask, never at a
// dynamic component index (FXC, ADR-1583).
fn trace_heightfield(inst: u32, o: vec3<f32>, d: vec3<f32>, hit_in: Hit, any_hit: bool) -> Hit {
    var hit_t = hit_in.t;
    var hit_inst = hit_in.inst;
    var hit_prim = hit_in.prim;
    var hit_u = hit_in.u;
    var hit_v = hit_in.v;
    let inv = safe_inv(d);
    let wr = wt_ray(o, d);
    let top = globals.terrain1.z - 1u;
    if (hf_box_t(top, 0u, 0u, o, inv, hit_t) >= INF) {
        return hit_in;
    }
    var sp = 1u;
    trace_stack[stack_slot(BVH_STACK)] = hf_pack(top, 0u, 0u);
    let cells_x = globals.terrain1.x - 1u;
    loop {
        if (sp == 0u) {
            break;
        }
        sp = sp - 1u;
        let e = trace_stack[stack_slot(BVH_STACK + sp)];
        let level = e >> 28u;
        let ci = (e >> 14u) & 0x3fffu;
        let cj = e & 0x3fffu;
        if (level == 0u) {
            let p00 = hf_point(ci, cj);
            let p10 = hf_point(ci + 1u, cj);
            let p01 = hf_point(ci, cj + 1u);
            let p11 = hf_point(ci + 1u, cj + 1u);
            if (min(min(p00.y, p10.y), min(p01.y, p11.y)) < HF_CUT) {
                continue;
            }
            let ra = ray_tri(wr, p00, p01, p10, hit_t);
            if (ra.x < hit_t) {
                hit_t = ra.x;
                hit_inst = inst;
                hit_prim = (cj * cells_x + ci) * 2u;
                hit_u = ra.y;
                hit_v = ra.z;
                if (any_hit) {
                    return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
                }
            }
            let rb = ray_tri(wr, p10, p01, p11, hit_t);
            if (rb.x < hit_t) {
                hit_t = rb.x;
                hit_inst = inst;
                hit_prim = (cj * cells_x + ci) * 2u + 1u;
                hit_u = rb.y;
                hit_v = rb.z;
                if (any_hit) {
                    return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
                }
            }
            continue;
        }
        if (hf_box_t(level, ci, cj, o, inv, hit_t) >= INF) {
            continue;
        }
        let child = level - 1u;
        let dims = hf_level_dims(child);
        var ts = vec4<f32>(INF);
        var es = vec4<u32>(0u);
        var n = 0u;
        for (var k = 0u; k < 4u; k = k + 1u) {
            let cx = ci * 2u + (k & 1u);
            let cz = cj * 2u + (k >> 1u);
            if (cx >= dims.x || cz >= dims.y) {
                continue;
            }
            let t = hf_box_t(child, cx, cz, o, inv, hit_t);
            if (t >= INF) {
                continue;
            }
            // Insertion sort, farthest first, so the nearest child is pushed last.
            var j = n;
            loop {
                if (j == 0u || ts[j - 1u] >= t) {
                    break;
                }
                let shift = LANES == vec4<u32>(j);
                ts = select(ts, vec4<f32>(ts[j - 1u]), shift);
                es = select(es, vec4<u32>(es[j - 1u]), shift);
                j = j - 1u;
            }
            let at = LANES == vec4<u32>(j);
            ts = select(ts, vec4<f32>(t), at);
            es = select(es, vec4<u32>(hf_pack(child, cx, cz)), at);
            n = n + 1u;
        }
        for (var k = 0u; k < n; k = k + 1u) {
            if (sp < HF_STACK) {
                trace_stack[stack_slot(BVH_STACK + sp)] = es[k];
                sp = sp + 1u;
            }
        }
    }
    return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
}

fn trace_instance(inst: u32, o: vec3<f32>, d: vec3<f32>, hit: Hit, any_hit: bool) -> Hit {
    let ins = instances[inst];
    // Shadow rays pass thin transmissive surfaces (glass casts no shadow; its tint and Fresnel
    // loss on next-event light are not modelled).
    if (any_hit && (ins.info2.y & INST_TRANSMIT) != 0u) {
        return hit;
    }
    if (ins.info.x == 1u) {
        return trace_heightfield(inst, o, d, hit, any_hit);
    }
    let lo = xform_point(ins.w2o0, ins.w2o1, ins.w2o2, o);
    let ld = xform_dir(ins.w2o0, ins.w2o1, ins.w2o2, d);
    return trace_blas(inst, lo, ld, hit, any_hit);
}

// The closest hit so far is one variable per member of `Hit` (ADR-7879); the top level's stack
// is entries `0..BVH_STACK` of this invocation's `trace_stack`.
fn trace(o: vec3<f32>, d: vec3<f32>, tmax: f32, any_hit: bool) -> Hit {
    let miss = Hit(tmax, NONE, NONE, 0.0, 0.0);
    let root = globals.counts.y;
    if (root == NONE) {
        return miss;
    }
    let inv = safe_inv(d);
    if (slab(nodes[root].bmin, nodes[root].bmax, o, inv, tmax) >= INF) {
        return miss;
    }
    var hit_t = miss.t;
    var hit_inst = miss.inst;
    var hit_prim = miss.prim;
    var hit_u = miss.u;
    var hit_v = miss.v;
    var sp = 0u;
    var node_idx = root;
    loop {
        let node = nodes[node_idx];
        if (node.count > 0u) {
            for (var k = 0u; k < node.count; k = k + 1u) {
                let h = trace_instance(node.a + k, o, d, Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v), any_hit);
                hit_t = h.t;
                hit_inst = h.inst;
                hit_prim = h.prim;
                hit_u = h.u;
                hit_v = h.v;
                if (any_hit && hit_inst != NONE) {
                    return h;
                }
            }
            if (sp == 0u) {
                break;
            }
            sp = sp - 1u;
            node_idx = trace_stack[stack_slot(sp)];
            continue;
        }
        let l = node.a;
        let r = node.a + 1u;
        let nl = nodes[l];
        let nr = nodes[r];
        let tl = slab(nl.bmin, nl.bmax, o, inv, hit_t);
        let tr = slab(nr.bmin, nr.bmax, o, inv, hit_t);
        if (tl < INF && tr < INF) {
            var near = l;
            var far = r;
            if (tr < tl) {
                near = r;
                far = l;
            }
            if (sp < BVH_STACK) {
                trace_stack[stack_slot(sp)] = far;
                sp = sp + 1u;
            }
            node_idx = near;
        } else if (tl < INF) {
            node_idx = l;
        } else if (tr < INF) {
            node_idx = r;
        } else {
            if (sp == 0u) {
                break;
            }
            sp = sp - 1u;
            node_idx = trace_stack[stack_slot(sp)];
        }
    }
    return Hit(hit_t, hit_inst, hit_prim, hit_u, hit_v);
}

fn occluded(o: vec3<f32>, d: vec3<f32>, tmax: f32) -> bool {
    return trace(o, d, tmax, true).inst != NONE;
}

// ---------------------------------------------------------------------------------------------
// Surfaces.

struct Surf {
    p: vec3<f32>,
    ng: vec3<f32>,
    ns: vec3<f32>,
    uv: vec2<f32>,
    color: vec4<f32>,
    material: u32,
    light: u32,
    // World tangent (xyz, bitangent sign in w): the vertex tangents like the forward pass, else
    // dP/du of the UV parameterisation; zero when neither exists.
    tangent: vec4<f32>,
    // Where rays leaving the surface start (before the float offset): `p`, or on a heightfield
    // the smooth surface the vertex normals describe (`terminator_point`).
    p_shadow: vec3<f32>,
}

// The shadow-terminator offset (Hanika 2021, "Hacking the Shadow Terminator", Ray Tracing
// Gems II ch. 4): `p` projected onto each vertex's tangent plane, the projections blended
// with the barycentrics. That is the curved surface the interpolated normals shade; where it
// lies outside the flat triangle (convex), rays leaving the point start on it, so a facet of
// a coarse heightfield does not shadow itself or its neighbours at a grazing sun.
fn terminator_point(
    p: vec3<f32>,
    ng: vec3<f32>,
    b: vec3<f32>,
    p0: vec3<f32>,
    p1: vec3<f32>,
    p2: vec3<f32>,
    n0: vec3<f32>,
    n1: vec3<f32>,
    n2: vec3<f32>,
) -> vec3<f32> {
    let q0 = p - min(dot(p - p0, n0), 0.0) * n0;
    let q1 = p - min(dot(p - p1, n1), 0.0) * n1;
    let q2 = p - min(dot(p - p2, n2), 0.0) * n2;
    let q = q0 * b.x + q1 * b.y + q2 * b.z;
    return select(p, q, dot(q - p, ng) > 0.0);
}

fn hf_vertex_normal(i: u32, j: u32) -> vec3<f32> {
    let nx = globals.terrain1.x;
    let nz = globals.terrain1.y;
    let il = select(i - 1u, i, i == 0u);
    let ir = min(i + 1u, nx - 1u);
    let jl = select(j - 1u, j, j == 0u);
    let jr = min(j + 1u, nz - 1u);
    // A cut neighbour (the rim of a hole) stands in at the centre height.
    let c = hf_height(i, j);
    let dx = (hf_height_or(ir, j, c) - hf_height_or(il, j, c)) / (f32(ir - il) * globals.terrain0.z);
    let dz = (hf_height_or(i, jr, c) - hf_height_or(i, jl, c)) / (f32(jr - jl) * globals.terrain0.w);
    return normalize(vec3<f32>(-dx, 1.0, -dz));
}

fn surface_at(hit: Hit) -> Surf {
    let ins = instances[hit.inst];
    let u = hit.u;
    let v = hit.v;
    let w = 1.0 - u - v;
    if (ins.info.x == 1u) {
        let cells_x = globals.terrain1.x - 1u;
        let cell = hit.prim / 2u;
        let ci = cell % cells_x;
        let cj = cell / cells_x;
        var i0 = vec2<u32>(ci, cj);
        var i1 = vec2<u32>(ci, cj + 1u);
        var i2 = vec2<u32>(ci + 1u, cj);
        if ((hit.prim & 1u) == 1u) {
            i0 = vec2<u32>(ci + 1u, cj);
            i1 = vec2<u32>(ci, cj + 1u);
            i2 = vec2<u32>(ci + 1u, cj + 1u);
        }
        let p0 = hf_point(i0.x, i0.y);
        let p1 = hf_point(i1.x, i1.y);
        let p2 = hf_point(i2.x, i2.y);
        let p = p0 * w + p1 * u + p2 * v;
        let ng = normalize(cross(p1 - p0, p2 - p0));
        let n0 = hf_vertex_normal(i0.x, i0.y);
        let n1 = hf_vertex_normal(i1.x, i1.y);
        let n2 = hf_vertex_normal(i2.x, i2.y);
        let ns = normalize(n0 * w + n1 * u + n2 * v);
        let extent = vec2<f32>(f32(cells_x) * globals.terrain0.z, f32(globals.terrain1.y - 1u) * globals.terrain0.w);
        let uv = (p.xz - globals.terrain0.xy) / extent;
        let up = select(ng, -ng, ng.y < 0.0);
        let p_shadow = terminator_point(p, up, vec3<f32>(w, u, v), p0, p1, p2, n0, n1, n2);
        return Surf(p, ng, ns, uv, vec4<f32>(1.0), globals.terrain1.w, NONE, vec4<f32>(1.0, 0.0, 0.0, 1.0), p_shadow);
    }
    let tri = hit.prim;
    let q0 = tri_pos[tri * 3u].xyz;
    let q1 = tri_pos[tri * 3u + 1u].xyz;
    let q2 = tri_pos[tri * 3u + 2u].xyz;
    let p0 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, q0);
    let p1 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, q1);
    let p2 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, q2);
    let p = p0 * w + p1 * u + p2 * v;
    let ng = normalize(cross(p1 - p0, p2 - p0));
    let a0 = tri_attr[tri * TRI_ATTR];
    let a1 = tri_attr[tri * TRI_ATTR + 1u];
    let a2 = tri_attr[tri * TRI_ATTR + 2u];
    let a3 = tri_attr[tri * TRI_ATTR + 3u];
    let a4 = tri_attr[tri * TRI_ATTR + 4u];
    let n_obj = a0.xyz * w + a1.xyz * u + a2.xyz * v;
    // Inverse transpose of object → world is the transpose of world → object.
    var ns = n_obj.x * ins.w2o0.xyz + n_obj.y * ins.w2o1.xyz + n_obj.z * ins.w2o2.xyz;
    if (dot(ns, ns) < 1e-20) {
        ns = ng;
    } else {
        ns = normalize(ns);
    }
    let uv0 = vec2<f32>(a0.w, a1.w);
    let uv1 = vec2<f32>(a2.w, a3.x);
    let uv2 = vec2<f32>(a3.y, a3.z);
    let uv = uv0 * w + uv1 * u + uv2 * v;
    let c0 = unpack4x8unorm(bitcast<u32>(a4.x));
    let c1 = unpack4x8unorm(bitcast<u32>(a4.y));
    let c2 = unpack4x8unorm(bitcast<u32>(a4.z));
    let color = c0 * w + c1 * u + c2 * v;
    // Emissive instances own one light per triangle, in the mesh's BVH order.
    var light = NONE;
    if (ins.info2.x != NONE) {
        light = ins.info2.x + bitcast<u32>(a3.w);
    }
    // Vertex tangents (object space, transformed like positions' directions), else the tangent
    // of the UV parameterisation (per triangle).
    let t0 = tri_attr[tri * TRI_ATTR + 5u];
    let t1 = tri_attr[tri * TRI_ATTR + 6u];
    let t2 = tri_attr[tri * TRI_ATTR + 7u];
    let t_obj = t0 * w + t1 * u + t2 * v;
    var tangent = vec4<f32>(0.0);
    if (dot(t_obj.xyz, t_obj.xyz) > 1e-12) {
        let tw = xform_dir(ins.o2w0, ins.o2w1, ins.o2w2, t_obj.xyz);
        tangent = vec4<f32>(tw, select(1.0, -1.0, t_obj.w < 0.0));
    } else {
        let duv1 = uv1 - uv0;
        let duv2 = uv2 - uv0;
        let det = duv1.x * duv2.y - duv2.x * duv1.y;
        if (abs(det) > 1e-12) {
            tangent = vec4<f32>(((p1 - p0) * duv2.y - (p2 - p0) * duv1.y) / det, 1.0);
        }
    }
    return Surf(p, ng, ns, uv, color, ins.info.w, light, tangent, p);
}

// The graph surface of material `m` at hit `s` of instance `inst` (ADR-2725): the forward pass's
// `graph_surface_input` over the tracer's hit. `n_geo` is the interpolated normal on the viewer's
// side, `wo` the unit direction toward the ray origin, `facing` is 1 on a front face and -1 on a
// back face (an alpha hit has no view, so it passes 1). The tracer's meshes carry one UV set, so the
// second set is the first (the forward pass's rule for a mesh without one). A back face negates the
// tangent and its handedness with the normal, as the forward pass's `graph_surface_input` does, so
// the graph's tangent frame is the front one negated (a normal map is not mirrored).
fn graph_surface(m: Material, s: Surf, inst: u32, n_geo: vec3<f32>, wo: vec3<f32>, facing: f32) -> regia::pbr::surface::Surface {
    pt_graph_base = m.graph.y;
    let ins = instances[inst];
    let object = vec3<f32>(ins.o2w0.w, ins.o2w1.w, ins.o2w2.w);
    let input = regia::pbr::surface::SurfaceInput(
        s.p,
        n_geo,
        s.uv,
        wo,
        s.color,
        s.tangent * facing,
        length(globals.cam_pos.xyz - s.p),
        s.uv,
        object,
        facing,
    );
    return pt_graph_dispatch(m.graph.x, input);
}

// Burley's normalized diffusion (Christensen & Burley 2015), shape `d` (m): the profile per unit
// area `R(r) = (e^(-r/d) + e^(-r/(3d))) / (8π d r)` (it integrates to 1 over the plane) and the
// density of the radius `2π r R(r)`. A channel without scattering (d = 0) has none.
fn burley_profile(d: f32, r: f32) -> f32 {
    if (d <= 0.0) {
        return 0.0;
    }
    let rr = max(r, 1e-7);
    return (exp(-rr / d) + exp(-rr / (3.0 * d))) / (8.0 * PI * d * rr);
}

fn burley_radius_pdf(d: f32, r: f32) -> f32 {
    if (d <= 0.0) {
        return 0.0;
    }
    return (exp(-r / d) + exp(-r / (3.0 * d))) / (4.0 * d);
}

// The radius by inverting the two exponentials of the mixture: weight 1/4 at mean d, 3/4 at 3d.
fn burley_sample_radius(d: f32, u: f32) -> f32 {
    if (u < 0.25) {
        return -d * log(max(1.0 - u * 4.0, 1e-7));
    }
    return -3.0 * d * log(max(1.0 - (u - 0.25) / 0.75, 1e-7));
}

struct Shading {
    base: vec3<f32>,
    diffuse_color: vec3<f32>,
    f0: vec3<f32>,
    emissive: vec3<f32>,
    perceptual: f32,
    rough: f32,
    coat: f32,
    coat_roughness: f32,
    multi_scatter: bool,
    unlit: bool,
    lambert: bool,
    // Directional albedo of the single-scatter GGX lobe at n·v (multi-scatter compensation) and
    // the specular layer's albedo with Fresnel at n·v (the diffuse under it).
    e_o: f32,
    e_avg: f32,
    es_o: vec3<f32>,
    // Lobe selection probabilities: diffuse, specular (the coat takes the rest).
    p_diffuse: f32,
    p_specular: f32,
    // Anisotropic GGX: the frame (t, b) around the shading normal and the alphas along them;
    // `aniso` false = isotropic `rough`.
    aniso: bool,
    aniso_t: vec3<f32>,
    aniso_b: vec3<f32>,
    alpha_t: f32,
    alpha_b: f32,
    // Thin-dielectric transmission (straight through, a delta lobe): its throughput weight and
    // the probability of choosing it; the reflection lobes are chosen with 1 − p_transmit.
    transmit: vec3<f32>,
    p_transmit: f32,
    // Thin film (`thin_film.wgsl`): the base under it (`f0` above is the proxy the Schlick-shaped
    // terms take), its weight (0 = no film), IOR and thickness in nm.
    film_base: vec3<f32>,
    film_cover: f32,
    film_ior: f32,
    film_nm: f32,
    // GGX alpha of the transmitted lobe (`rough_transmission.wgsl`); 0 is the delta lobe.
    transmit_alpha: f32,
}

// Material inputs after textures, ORM and detail maps (the forward pass's surface stage).
struct SurfaceInputs {
    base: vec3<f32>,
    metallic: f32,
    perceptual: f32,
    // Shading normal after the normal map and the detail normal.
    n: vec3<f32>,
}

// Detail-map weight: the forward pass's linear fade in the camera distance.
fn detail_fade(m: Material, p: vec3<f32>) -> f32 {
    if (m.detail2.z > m.detail.w) {
        let dist = length(globals.cam_pos.xyz - p);
        return clamp((m.detail2.z - dist) / (m.detail2.z - m.detail.w), 0.0, 1.0);
    }
    return 1.0;
}

// Tangent of a double-sided back face (`lab_forward.wgsl`'s `back_face_tangent`): with the
// bitangent built from the flipped normal, negating the tangent makes the back face's frame the
// front's negated, so a normal-mapped back face shades with the front's mapped normal reversed.
fn back_face_tangent(t: vec3<f32>, back_face: bool) -> vec3<f32> {
    return select(t, -t, back_face);
}

// The forward pass's surface stage (`lab_forward.wgsl` fs_main): base colour × texture × vertex
// colour, packed ORM metallic/roughness, the normal map in the vertex-tangent frame, then the
// detail maps (albedo ×2 blend, roughness lerp, whiteout normal blend). `n_geo` is the
// interpolated normal already flipped toward the viewer; `back_face` says it was flipped.
fn surface_inputs(m: Material, s: Surf, n_geo: vec3<f32>, back_face: bool) -> SurfaceInputs {
    var base = m.base_color.rgb * s.color.rgb;
    if (m.tex.x != NONE) {
        base = base * tex_sample(m.tex.x, s.uv).rgb;
    }
    var mr = vec2<f32>(1.0, 1.0);
    if (m.extra.z > 0.5 && m.tex2.x != NONE) {
        mr = tex_sample(m.tex2.x, s.uv).gb;
    }
    var metallic = clamp(m.params.x * mr.y, 0.0, 1.0);
    var perceptual = clamp(m.params.y * mr.x, 0.0, 1.0);
    var n = n_geo;
    let has_tangent = dot(s.tangent.xyz, s.tangent.xyz) > 1e-20;
    var mapped = vec3<f32>(0.0, 0.0, 1.0);
    if (m.tex.y != NONE && m.params.z > 0.0 && has_tangent) {
        // `b` from the flipped normal is the front bitangent negated; `back_face_tangent`
        // negates the tangent too, so a back face's whole frame is the front one negated and
        // its mapped normal is exactly the front's reversed (`lab_forward.wgsl`).
        let t_front = normalize(s.tangent.xyz);
        let b = cross(n_geo, t_front) * s.tangent.w;
        let t = back_face_tangent(t_front, back_face);
        let ts = tex_sample(m.tex.y, s.uv).xyz * 2.0 - vec3<f32>(1.0);
        mapped = normalize(vec3<f32>(ts.x * m.params.z, ts.y * m.params.z, ts.z));
        n = normalize(mat3x3<f32>(t, b, n_geo) * vec3<f32>(ts.x * m.params.z, ts.y * m.params.z, ts.z));
    }
    if (m.detail2.w > 0.5 && (m.detail.x > 1e-5 || m.detail.y > 1e-5 || m.detail.z > 1e-5)) {
        let fade = detail_fade(m, s.p);
        let duv = s.uv * m.detail2.xy;
        // Fallbacks match the forward pass's neutral textures: 128 grey, flat normal + mask 1.
        var img = vec4<f32>(128.0 / 255.0);
        if (m.tex2.y != NONE) {
            img = tex_sample(m.tex2.y, duv);
        }
        var dnm = vec4<f32>(128.0 / 255.0, 128.0 / 255.0, 1.0, 1.0);
        if (m.tex2.z != NONE) {
            dnm = tex_sample(m.tex2.z, duv);
        }
        let mask = dnm.a;
        if (m.detail.x > 1e-5) {
            base = base * mix(vec3<f32>(1.0), img.rgb * 2.0, fade * mask * m.detail.x);
        }
        if (m.detail.z > 1e-5) {
            perceptual = mix(perceptual, img.a, fade * mask * m.detail.z);
        }
        if (m.detail.y > 1e-5 && has_tangent) {
            let dn = dnm.xyz * 2.0 - vec3<f32>(1.0);
            let t_front = normalize(s.tangent.xyz);
            let b = cross(n_geo, t_front) * s.tangent.w;
            let t = back_face_tangent(t_front, back_face);
            let strength = fade * mask * m.detail.y;
            let n2 = vec3<f32>(dn.x * strength, dn.y * strength, dn.z);
            let blended = normalize(vec3<f32>(mapped.x + n2.x, mapped.y + n2.y, mapped.z * n2.z));
            n = normalize(mat3x3<f32>(t, b, n_geo) * blended);
        }
    }
    return SurfaceInputs(base, metallic, perceptual, n);
}

fn make_shading(m: Material, s: Surf, inputs: SurfaceInputs, n: vec3<f32>, wo: vec3<f32>, back_face: bool) -> Shading {
    let base = inputs.base;
    var emissive = m.emissive.rgb;
    if (m.tex.z != NONE) {
        emissive = emissive * tex_sample(m.tex.z, s.uv).rgb;
    }
    let metallic = inputs.metallic;
    let perceptual = inputs.perceptual;
    let rough = max(perceptual * perceptual, regia::pbr::lighting::MIN_ALPHA);
    // Transmission replaces the diffuse lobe of a thin dielectric (`lab_forward.wgsl`); the
    // dielectric F0 comes from the IOR.
    let transmission = clamp(m.transmit.x, 0.0, 1.0);
    let diffuse_color = base * (1.0 - metallic) * (1.0 - transmission);
    let f0_dielectric = select(0.04, m.transmit.y, m.transmit.y > 0.0);
    let film_base = mix(vec3<f32>(f0_dielectric), base, vec3<f32>(metallic));
    var f0 = film_base;
    let no_v = max(dot(n, wo), 1e-5);
    // Thin film, as in the forward pass's `THIN_FILM` variant.
    var film_cover = 0.0;
    var film_ior = 1.0;
    var film_nm = 0.0;
    if (m.film.x > 0.0) {
        var film_map = vec4<f32>(1.0);
        if (m.tex2.w != NONE) {
            film_map = tex_sample(m.tex2.w, s.uv);
        }
        film_ior = m.film.y;
        film_nm = mix(m.film.z, m.film.w, film_map.g);
        film_cover = regia::pbr::thin_film::weight(m.film.x * film_map.r, film_nm);
        f0 = regia::pbr::thin_film::schlick_f0(no_v, film_base, film_cover, film_ior, film_nm);
    }
    var e_o = 1.0;
    var e_avg = 1.0;
    var es_o = vec3<f32>(0.0);
    let ms = m.coat.z > 0.5;
    if (ms) {
        let dv = dfg(no_v, perceptual);
        e_o = dv.z;
        e_avg = dv.w;
        es_o = clamp(f0 * dv.x + vec3<f32>(dv.y), vec3<f32>(0.0), vec3<f32>(1.0));
    }
    // Anisotropy in the forward pass's frame: the vertex tangent made orthogonal to the
    // shading normal, rotated by the material's angle.
    var aniso = false;
    var at = vec3<f32>(1.0, 0.0, 0.0);
    var ab = vec3<f32>(0.0, 1.0, 0.0);
    var alphas = vec2<f32>(rough, rough);
    if (abs(m.extra.x) > 1e-4 && dot(s.tangent.xyz, s.tangent.xyz) > 1e-20) {
        let raw = s.tangent.xyz;
        let t0v = raw - n * dot(n, raw);
        if (length(t0v) > 1e-5) {
            // The front's anisotropy direction on a back face (negated frame, as above).
            let t_front = normalize(t0v);
            let b0 = cross(n, t_front) * s.tangent.w;
            let t0 = back_face_tangent(t_front, back_face);
            let c = cos(m.extra.y);
            let sn = sin(m.extra.y);
            at = t0 * c + b0 * sn;
            ab = b0 * c - t0 * sn;
            alphas = regia::pbr::lighting::anisotropic_alphas(rough, m.extra.x);
            aniso = true;
        }
    }
    let lambert = m.coat.w > 1.5;
    if (lambert) {
        return Shading(base, base, vec3<f32>(0.0), emissive, perceptual, rough, 0.0, 0.0, false, false, true, 1.0, 1.0, vec3<f32>(0.0), 1.0, 0.0, false, at, ab, rough, rough, vec3<f32>(0.0), 0.0, vec3<f32>(0.0), 0.0, 1.0, 0.0, 0.0);
    }
    let fs = luminance(regia::pbr::lighting::f_schlick(no_v, f0));
    let fd = luminance(diffuse_color) * (1.0 - fs);
    let fc = select(0.0, m.coat.x * regia::pbr::lighting::f_schlick_f32(no_v, 0.04), m.coat.x > 1e-4);
    let total = fs + fd + fc;
    var pd = 1.0;
    var ps = 0.0;
    if (total > 0.0) {
        pd = fd / total;
        ps = fs / total;
    }
    // The forward's transmitted share: t · (1 − F) · base · volume^(1 / N·V).
    var transmit = vec3<f32>(0.0);
    var pt = 0.0;
    var transmit_alpha = 0.0;
    if (transmission > 0.0) {
        if (m.transmit.z > 0.0) {
            let r0 = sqrt(clamp(f0_dielectric, 0.0, 0.81));
            transmit_alpha = regia::pbr::rough_transmission::thin_alpha(m.transmit.z, (1.0 + r0) / (1.0 - r0));
        }
        let fv = f0_dielectric + (1.0 - f0_dielectric) * pow(1.0 - clamp(no_v, 1e-3, 1.0), 5.0);
        let path_len = 1.0 / max(no_v, 0.2);
        let volume = pow(clamp(m.volume.rgb, vec3<f32>(1e-4), vec3<f32>(1.0)), vec3<f32>(path_len));
        transmit = transmission * (1.0 - fv) * base * volume;
        let ft = luminance(transmit);
        if (ft + total > 0.0) {
            pt = ft / (ft + total);
        }
    }
    return Shading(
        base,
        diffuse_color,
        f0,
        emissive,
        perceptual,
        rough,
        select(0.0, m.coat.x, m.coat.x > 1e-4),
        m.coat.y,
        ms,
        m.coat.w > 0.5 && m.coat.w < 1.5,
        lambert,
        e_o,
        e_avg,
        es_o,
        pd,
        ps,
        aniso,
        at,
        ab,
        alphas.x,
        alphas.y,
        transmit,
        pt,
        film_base,
        film_cover,
        film_ior,
        film_nm,
        transmit_alpha,
    );
}

fn zero_lobes() -> regia::pbr::brdf::BrdfLobes {
    return regia::pbr::brdf::BrdfLobes(vec3<f32>(0.0), vec3<f32>(0.0), vec3<f32>(0.0), 1.0);
}

// The forward pass's direct BRDF (cosine excluded). Each step is a value (`let`), not a
// variable of the struct type (VUID-10684, ADR-7879); the `brdf_*` steps return their input
// unchanged when they do not apply.
fn eval_brdf(sh: Shading, n: vec3<f32>, wo: vec3<f32>, l: vec3<f32>) -> regia::pbr::brdf::BrdfLobes {
    let no_l = dot(n, l);
    if (no_l <= 0.0) {
        return zero_lobes();
    }
    if (sh.lambert) {
        return regia::pbr::brdf::BrdfLobes(sh.diffuse_color / PI, vec3<f32>(0.0), vec3<f32>(0.0), 1.0);
    }
    let no_v = max(dot(n, wo), 1e-5);
    let h = normalize(l + wo);
    let no_h = max(dot(n, h), 0.0);
    let lo_h = max(dot(l, h), 0.0);
    let base = brdf_base_lobes(sh, no_v, no_l, no_h, lo_h);
    let aniso = brdf_aniso_lobes(base, sh, wo, l, h, no_v, no_l, no_h);
    let energy = brdf_energy_lobes(aniso, sh, no_v, no_l);
    return brdf_coat_lobes(energy, sh, no_v, no_l, no_h, lo_h);
}

// `eval_brdf`'s isotropic lobes, or the thin-film ones under a film.
fn brdf_base_lobes(sh: Shading, no_v: f32, no_l: f32, no_h: f32, lo_h: f32) -> regia::pbr::brdf::BrdfLobes {
    if (sh.film_cover > 0.0) {
        return regia::pbr::brdf::isotropic_lobes_film(no_v, no_l, no_h, lo_h, sh.diffuse_color, sh.film_base, sh.rough, sh.film_cover, sh.film_ior, sh.film_nm);
    }
    return regia::pbr::brdf::isotropic_lobes(no_v, no_l, no_h, lo_h, sh.diffuse_color, sh.f0, sh.rough);
}

// The forward pass swaps in anisotropic GGX for the base specular (same Fresnel).
fn brdf_aniso_lobes(
    lobes: regia::pbr::brdf::BrdfLobes,
    sh: Shading,
    wo: vec3<f32>,
    l: vec3<f32>,
    h: vec3<f32>,
    no_v: f32,
    no_l: f32,
    no_h: f32,
) -> regia::pbr::brdf::BrdfLobes {
    if (!sh.aniso) {
        return lobes;
    }
    let da = regia::pbr::lighting::d_ggx_anisotropic(dot(sh.aniso_t, h), dot(sh.aniso_b, h), no_h, sh.alpha_t, sh.alpha_b);
    let va = regia::pbr::lighting::v_smith_ggx_anisotropic(dot(sh.aniso_t, wo), dot(sh.aniso_b, wo), dot(sh.aniso_t, l), dot(sh.aniso_b, l), no_v, no_l, sh.alpha_t, sh.alpha_b);
    return regia::pbr::brdf::BrdfLobes(lobes.diffuse, da * va * lobes.fresnel, lobes.fresnel, lobes.coat_att);
}

fn brdf_energy_lobes(lobes: regia::pbr::brdf::BrdfLobes, sh: Shading, no_v: f32, no_l: f32) -> regia::pbr::brdf::BrdfLobes {
    if (globals.dfg.z == 1u) {
        // `BrdfEnergy::Compensated` (ADR-2632): reciprocal multiple scattering and the diffuse
        // coupled to the specular layer at both cosines, as the forward pass's
        // `BRDF_ENERGY_COMPENSATED` variant.
        return regia::pbr::brdf::with_compensation(lobes, sh.f0, sh.diffuse_color, sh.perceptual, dfg(no_v, sh.perceptual), dfg(no_l, sh.perceptual), sh.multi_scatter);
    }
    if (sh.multi_scatter) {
        return regia::pbr::brdf::with_multi_scatter(lobes, sh.f0, sh.e_o, sh.e_avg, sh.diffuse_color, sh.es_o);
    }
    return lobes;
}

fn brdf_coat_lobes(lobes: regia::pbr::brdf::BrdfLobes, sh: Shading, no_v: f32, no_l: f32, no_h: f32, lo_h: f32) -> regia::pbr::brdf::BrdfLobes {
    if (sh.coat > 0.0) {
        return regia::pbr::brdf::with_clearcoat(lobes, no_v, no_l, no_h, lo_h, sh.coat, sh.coat_roughness);
    }
    return lobes;
}

fn smith_g1(no_v: f32, a: f32) -> f32 {
    let a2 = a * a;
    return 2.0 * no_v / (no_v + sqrt(a2 + (1.0 - a2) * no_v * no_v));
}

// Solid-angle pdf of a reflection sampled from the visible normals of GGX(a).
fn vndf_pdf(no_v: f32, no_h: f32, a: f32) -> f32 {
    return smith_g1(no_v, a) * regia::pbr::lighting::d_ggx(no_h, a) / (4.0 * no_v);
}

// The same for anisotropic GGX in the frame (t, b, n) with alphas (at, ab).
fn vndf_pdf_aniso(sh: Shading, n: vec3<f32>, wo: vec3<f32>, h: vec3<f32>) -> f32 {
    let no_v = max(dot(n, wo), 1e-5);
    let vt = dot(sh.aniso_t, wo) * sh.alpha_t;
    let vb = dot(sh.aniso_b, wo) * sh.alpha_b;
    let g1 = 2.0 * no_v / (no_v + sqrt(vt * vt + vb * vb + no_v * no_v));
    let d = regia::pbr::lighting::d_ggx_anisotropic(dot(sh.aniso_t, h), dot(sh.aniso_b, h), max(dot(n, h), 0.0), sh.alpha_t, sh.alpha_b);
    return g1 * d / (4.0 * no_v);
}

// Heitz 2018: a visible normal for the local view `ve` (z up), alphas along x and y.
fn sample_vndf(ve: vec3<f32>, ax: f32, ay: f32, xi: vec2<f32>) -> vec3<f32> {
    let vh = normalize(vec3<f32>(ax * ve.x, ay * ve.y, ve.z));
    let lensq = vh.x * vh.x + vh.y * vh.y;
    var t1 = vec3<f32>(1.0, 0.0, 0.0);
    if (lensq > 0.0) {
        t1 = vec3<f32>(-vh.y, vh.x, 0.0) * inverseSqrt(lensq);
    }
    let t2v = cross(vh, t1);
    let r = sqrt(xi.x);
    let phi = TAU * xi.y;
    let p1 = r * cos(phi);
    var p2 = r * sin(phi);
    let s = 0.5 * (1.0 + vh.z);
    p2 = (1.0 - s) * sqrt(max(1.0 - p1 * p1, 0.0)) + s * p2;
    let nh = p1 * t1 + p2 * t2v + sqrt(max(0.0, 1.0 - p1 * p1 - p2 * p2)) * vh;
    return normalize(vec3<f32>(ax * nh.x, ay * nh.y, max(0.0, nh.z)));
}

fn bsdf_pdf(sh: Shading, n: vec3<f32>, wo: vec3<f32>, l: vec3<f32>) -> f32 {
    let no_l = dot(n, l);
    if (no_l <= 0.0) {
        return 0.0;
    }
    let no_v = max(dot(n, wo), 1e-5);
    let h = normalize(l + wo);
    let no_h = max(dot(n, h), 0.0);
    var pdf = sh.p_diffuse * no_l / PI;
    if (sh.aniso) {
        pdf = pdf + sh.p_specular * vndf_pdf_aniso(sh, n, wo, h);
    } else {
        pdf = pdf + sh.p_specular * vndf_pdf(no_v, no_h, sh.rough);
    }
    let p_coat = max(1.0 - sh.p_diffuse - sh.p_specular, 0.0);
    if (p_coat > 0.0) {
        let coat_a = max(sh.coat_roughness * sh.coat_roughness, regia::pbr::lighting::MIN_ALPHA);
        pdf = pdf + p_coat * vndf_pdf(no_v, no_h, coat_a);
    }
    // The reflection lobes are sampled only when transmission is not chosen.
    return pdf * (1.0 - sh.p_transmit);
}

fn sample_bsdf_dir(sh: Shading, n: vec3<f32>, wo: vec3<f32>) -> vec3<f32> {
    let pick = rand();
    let xi = rand2();
    if (pick < sh.p_diffuse) {
        let frame = onb(n);
        let r = sqrt(xi.x);
        let phi = TAU * xi.y;
        let local = vec3<f32>(r * cos(phi), r * sin(phi), sqrt(max(0.0, 1.0 - xi.x)));
        return normalize(frame * local);
    }
    var frame = onb(n);
    var ax = sh.rough;
    var ay = sh.rough;
    if (pick >= sh.p_diffuse + sh.p_specular) {
        ax = max(sh.coat_roughness * sh.coat_roughness, regia::pbr::lighting::MIN_ALPHA);
        ay = ax;
    } else if (sh.aniso) {
        frame = mat3x3<f32>(sh.aniso_t, sh.aniso_b, n);
        ax = sh.alpha_t;
        ay = sh.alpha_b;
    }
    let ve = vec3<f32>(dot(wo, frame[0]), dot(wo, frame[1]), dot(wo, frame[2]));
    let hl = sample_vndf(ve, ax, ay, xi);
    let h = frame * hl;
    return reflect(-wo, h);
}

// ---------------------------------------------------------------------------------------------
// Local lights.

struct LightSample {
    // Unit direction toward the light.
    l: vec3<f32>,
    // Distance to the sampled point (INF for none).
    dist: f32,
    // Radiance (area lights) or irradiance (delta lights) arriving along `l`.
    li: vec3<f32>,
    // Solid-angle pdf of the direction (0 = delta).
    pdf: f32,
}

fn pick_light(u: f32) -> u32 {
    let n = globals.counts.x;
    var lo = 0u;
    var hi = n - 1u;
    loop {
        if (lo >= hi) {
            break;
        }
        let mid = (lo + hi) / 2u;
        if (lights[mid].extra.z > u) {
            hi = mid;
        } else {
            lo = mid + 1u;
        }
    }
    return lo;
}

fn triangle_world(inst: u32, tri: u32) -> array<vec3<f32>, 3> {
    let ins = instances[inst];
    let p0 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, tri_pos[tri * 3u].xyz);
    let p1 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, tri_pos[tri * 3u + 1u].xyz);
    let p2 = xform_point(ins.o2w0, ins.o2w1, ins.o2w2, tri_pos[tri * 3u + 2u].xyz);
    return array<vec3<f32>, 3>(p0, p1, p2);
}

fn triangle_radiance(light: Light, u: f32, v: f32) -> vec3<f32> {
    let m = materials[instances[light.refs.x].info.w];
    var e = m.emissive.rgb;
    if (m.tex.z != NONE) {
        e = e * tex_sample(m.tex.z, tri_uv(light.refs.y, u, v)).rgb;
    }
    return e;
}

// IES profile of an analytic light (`PtLightProfile`, ADR-2592): `refs.w` is the band's offset in `textures`
// plus 1 (0 = none), `refs.y` the horizontal turn in 2π/256 steps, `dir.xyz` the nadir. The band is 32 rows (φ)
// of 256 values (θ, 0..180°) as raw f32 bits; bilinear, as `regia_render`'s `light_profile_relative`. `d` is the
// unit direction from the light toward the receiver.
fn light_profile(light: Light, d: vec3<f32>) -> f32 {
    if (light.refs.w == 0u) {
        return 1.0;
    }
    let base = light.refs.w - 1u;
    let n = normalize(light.dir.xyz);
    let up = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.999);
    let a0 = normalize(cross(up, n));
    let a1 = cross(a0, n);
    let turn = f32(light.refs.y) * (TAU / 256.0);
    let t0 = a0 * cos(turn) + a1 * sin(turn);
    let t1 = cross(t0, n);
    let theta = acos(clamp(dot(d, n), -1.0, 1.0));
    var phi = atan2(dot(d, t1), dot(d, t0));
    if (phi < 0.0) {
        phi = phi + TAU;
    }
    let x = theta / PI * 255.0;
    let y = phi / TAU * 32.0;
    let xf = floor(x);
    let yf = floor(y);
    let fx = x - xf;
    let fy = y - yf;
    let x0 = u32(clamp(xf, 0.0, 255.0));
    let x1 = min(x0 + 1u, 255u);
    let y0 = u32(yf) % 32u;
    let y1 = (y0 + 1u) % 32u;
    let v00 = bitcast<f32>(textures[base + y0 * 256u + x0]);
    let v10 = bitcast<f32>(textures[base + y0 * 256u + x1]);
    let v01 = bitcast<f32>(textures[base + y1 * 256u + x0]);
    let v11 = bitcast<f32>(textures[base + y1 * 256u + x1]);
    return mix(mix(v00, v10, fx), mix(v01, v11, fx), fy);
}

fn sample_light(index: u32, p: vec3<f32>, xi: vec2<f32>) -> LightSample {
    let light = lights[index];
    let kind = light.refs.z;
    if (kind == LIGHT_POINT || kind == LIGHT_SPOT) {
        let to = light.pos.xyz - p;
        let d2 = max(dot(to, to), 1e-12);
        let dist = sqrt(d2);
        let l = to / dist;
        var li = light.color.rgb / d2;
        if (kind == LIGHT_SPOT) {
            li = li * regia::pbr::lighting::spot_attenuation(l, light.dir.xyz, light.extra.x, light.extra.y);
        }
        li = li * light_profile(light, -l);
        return LightSample(l, dist, li, 0.0);
    }
    if (kind == LIGHT_SPHERE) {
        let to = light.pos.xyz - p;
        let d2 = dot(to, to);
        let r = light.dir.w;
        if (d2 <= r * r) {
            return LightSample(vec3<f32>(0.0, 1.0, 0.0), INF, vec3<f32>(0.0), 0.0);
        }
        let dist_c = sqrt(d2);
        let axis = to / dist_c;
        let sin2 = r * r / d2;
        let cos_max = sqrt(max(0.0, 1.0 - sin2));
        let omc = sin2 / (1.0 + cos_max);
        let l = sample_cone(axis, omc, xi);
        let b = dot(l, to);
        let disc = b * b - (d2 - r * r);
        let t = b - sqrt(max(disc, 0.0));
        var li_s = light.color.rgb;
        // A sized spot (`PtLight::SphereSpot`, ADR-2592): the cone of the direction from the centre.
        if (light.extra.w > 0.5) {
            li_s = li_s * regia::pbr::lighting::spot_attenuation(axis, light.dir.xyz, light.extra.x, light.extra.y);
        }
        li_s = li_s * light_profile(light, -axis);
        return LightSample(l, max(t, 0.0), li_s, 1.0 / (TAU * omc));
    }
    // Emissive triangle: uniform area sampling.
    let tri = triangle_world(light.refs.x, light.refs.y);
    let su = sqrt(xi.x);
    let b1 = 1.0 - su;
    let b2 = xi.y * su;
    // Barycentrics relative to (p0, p1, p2) as (1 - u - v, u, v).
    let q = tri[0] * b1 + tri[1] * b2 + tri[2] * (1.0 - b1 - b2);
    let nl = cross(tri[1] - tri[0], tri[2] - tri[0]);
    let area = 0.5 * length(nl);
    let to = q - p;
    let d2 = dot(to, to);
    if (area <= 0.0 || d2 <= 0.0) {
        return LightSample(vec3<f32>(0.0, 1.0, 0.0), INF, vec3<f32>(0.0), 0.0);
    }
    let dist = sqrt(d2);
    let l = to / dist;
    let cos_l = abs(dot(nl, l)) / (2.0 * area);
    // A one-sided emitter (quad light) emits toward its winding normal only.
    let one_sided = materials[instances[light.refs.x].info.w].extra.w > 0.5;
    if (cos_l <= 1e-8 || (one_sided && dot(nl, l) >= 0.0)) {
        return LightSample(l, dist, vec3<f32>(0.0), 0.0);
    }
    let li = triangle_radiance(light, b2, 1.0 - b1 - b2);
    return LightSample(l, dist, li, d2 / (cos_l * area));
}

// Solid-angle pdf with which NEE would have chosen the point `q` of emissive triangle `index`
// as seen from `p` along `l`.
fn triangle_light_pdf(index: u32, p: vec3<f32>, q: vec3<f32>, l: vec3<f32>) -> f32 {
    let light = lights[index];
    let tri = triangle_world(light.refs.x, light.refs.y);
    let nl = cross(tri[1] - tri[0], tri[2] - tri[0]);
    let area = 0.5 * length(nl);
    let to = q - p;
    let d2 = dot(to, to);
    let cos_l = abs(dot(nl, l)) / max(2.0 * area, 1e-20);
    if (area <= 0.0 || cos_l <= 1e-8) {
        return 0.0;
    }
    return light.color.w * d2 / (cos_l * area);
}

// ---------------------------------------------------------------------------------------------
// Participating media: up to 8 layers, each `σ(y) = σ_ref · exp(−k (y − y_ref))` inside
// [y_min, y_max] (world Y). Optical depth along a ray is analytic per layer; free-flight
// distances invert the scattering depth numerically for one colour channel picked in
// proportion to the path throughput, weighted by the channel-averaged pdf (one-sample spectral
// MIS), so chromatic air stays unbiased; absorption is applied analytically. "Aerial" layers (the planet's air) only apply to segments
// that end at a surface or a local light: the sky table and the sun's illuminance integrate
// them for rays that escape.

fn media_on() -> bool {
    return globals.media.x > 0u;
}

fn layer_word(i: u32, k: u32) -> f32 {
    return env[globals.media.y + i * MEDIUM_WORDS + k];
}

fn layer_scattering(i: u32) -> vec3<f32> {
    return vec3<f32>(layer_word(i, 0u), layer_word(i, 1u), layer_word(i, 2u));
}

fn layer_extinction(i: u32) -> vec3<f32> {
    return layer_scattering(i) + vec3<f32>(layer_word(i, 4u), layer_word(i, 5u), layer_word(i, 6u));
}

fn layer_aerial(i: u32) -> bool {
    return layer_word(i, 11u) > 0.5;
}

// Relative density of layer `i` at height `y` (0 outside its bounds; the upper bound is
// exclusive so two layers that abut at a height do not both claim a ray at exactly that height).
fn layer_density(i: u32, y: f32) -> f32 {
    if (y < layer_word(i, 13u) || y >= layer_word(i, 14u)) {
        return 0.0;
    }
    return layer_exp(-layer_word(i, 7u) * (y - layer_word(i, 12u)));
}

// `exp` with the argument clamped so a point far below a layer's reference height stays finite
// (an unclamped `exp(120)` is +inf and turns the free-flight pdf into inf / inf).
fn layer_exp(x: f32) -> f32 {
    return exp(clamp(x, -80.0, 60.0));
}

// ∫ density dt over t ∈ [t0, t1] of the ray o + t·d for layer `i` (analytic: the bounds clip
// the interval, then an exponential in t).
fn layer_column(i: u32, o: vec3<f32>, d: vec3<f32>, t0: f32, t1: f32) -> f32 {
    let lo = layer_word(i, 13u);
    let hi = layer_word(i, 14u);
    var a = t0;
    var b = t1;
    if (abs(d.y) < 1e-9) {
        // Upper bound exclusive, as in `layer_density`: abutting layers share a boundary
        // height, and a horizontal ray exactly there belongs to the layer above.
        if (o.y < lo || o.y >= hi) {
            return 0.0;
        }
    } else {
        let ta = (lo - o.y) / d.y;
        let tb = (hi - o.y) / d.y;
        a = max(a, min(ta, tb));
        b = min(b, max(ta, tb));
    }
    if (!(b > a)) {
        return 0.0;
    }
    let k = layer_word(i, 7u);
    let ref_y = layer_word(i, 12u);
    let x = k * d.y * (b - a);
    if (x < 0.0) {
        // Density grows along the ray: anchor on the far end, ρ(b)·(e^x − 1)/x, so a long
        // descent through thin air never forms 0 · inf.
        let rho_b = layer_exp(-k * (o.y + b * d.y - ref_y));
        var g = 1.0 + 0.5 * x + x * x / 6.0;
        if (abs(x) > 1e-3) {
            g = (exp(x) - 1.0) / x;
        }
        return rho_b * (b - a) * g;
    }
    let rho_a = layer_exp(-k * (o.y + a * d.y - ref_y));
    // (1 − e^(−x)) / x, stable near 0.
    var f = 1.0 - 0.5 * x + x * x / 6.0;
    if (abs(x) > 1e-3) {
        f = (1.0 - exp(-x)) / x;
    }
    return rho_a * (b - a) * f;
}

// Optical depth (per channel) of the media over [0, t] along o + t·d.
fn medium_tau(o: vec3<f32>, d: vec3<f32>, t: f32, aerial: bool) -> vec3<f32> {
    var tau = vec3<f32>(0.0);
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        tau = tau + layer_extinction(i) * layer_column(i, o, d, 0.0, t);
    }
    return tau;
}

fn medium_transmittance(o: vec3<f32>, d: vec3<f32>, t: f32, aerial: bool) -> vec3<f32> {
    if (!media_on()) {
        return vec3<f32>(1.0);
    }
    return exp(-min(medium_tau(o, d, min(t, MEDIUM_FAR), aerial), vec3<f32>(80.0)));
}

// Transmittance toward a light at infinity (sun, sky, sky body): non-aerial layers out to the
// escape distance.
fn escape_transmittance(o: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    if (cloud_on()) {
        // The clouds' whole range: the layers' escape distance is the froxel fog's.
        return medium_transmittance(o, d, escape_distance(), false) * cloud_transmittance(o, d, MEDIUM_FAR);
    }
    if (!media_on()) {
        return vec3<f32>(1.0);
    }
    return medium_transmittance(o, d, escape_distance(), false);
}

fn escape_distance() -> f32 {
    return select(MEDIUM_FAR, min(globals.media_f.x, MEDIUM_FAR), globals.media_f.x > 0.0);
}

struct MediumPoint {
    sigma_s: vec3<f32>,
    sigma_t: vec3<f32>,
}

fn medium_at(p: vec3<f32>, aerial: bool) -> MediumPoint {
    var ss = vec3<f32>(0.0);
    var st = vec3<f32>(0.0);
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        let rho = layer_density(i, p.y);
        ss = ss + layer_scattering(i) * rho;
        st = st + layer_extinction(i) * rho;
    }
    return MediumPoint(ss, st);
}

fn hg(cos_t: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = max(1.0 + g2 - 2.0 * g * cos_t, 1e-6);
    return (1.0 - g2) / (4.0 * PI * denom * sqrt(denom));
}

// Phase of layer `i` per channel at the scattering angle with cosine `cos_t`
// (`cos_t = dot(propagation in, propagation out)`).
fn layer_phase(i: u32, cos_t: f32) -> vec3<f32> {
    if (layer_word(i, 3u) > 0.5) {
        return vec3<f32>(3.0 / (16.0 * PI) * (1.0 + cos_t * cos_t));
    }
    return vec3<f32>(hg(cos_t, layer_word(i, 8u)), hg(cos_t, layer_word(i, 9u)), hg(cos_t, layer_word(i, 10u)));
}

// The mixture phase at `p`: Σ σ_s,i p_i / σ_s per channel, and the sampling pdf (the mixture
// over layers weighted by the luminance of their scattering, each sampled at its luminance-mean
// asymmetry).
struct PhaseEval {
    value: vec3<f32>,
    pdf: f32,
}

fn phase_eval(p: vec3<f32>, aerial: bool, cos_t: f32) -> PhaseEval {
    var num = vec3<f32>(0.0);
    var den = vec3<f32>(0.0);
    var pdf = 0.0;
    var wsum = 0.0;
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        let ss = layer_scattering(i) * layer_density(i, p.y);
        let ph = layer_phase(i, cos_t);
        num = num + ss * ph;
        den = den + ss;
        let w = luminance(ss);
        wsum = wsum + w;
        pdf = pdf + w * layer_sample_pdf(i, cos_t);
    }
    let value = select(vec3<f32>(0.0), num / den, den > vec3<f32>(0.0));
    return PhaseEval(value, select(0.0, pdf / wsum, wsum > 0.0));
}

fn layer_mean_g(i: u32) -> f32 {
    return dot(vec3<f32>(layer_word(i, 8u), layer_word(i, 9u), layer_word(i, 10u)), vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn layer_sample_pdf(i: u32, cos_t: f32) -> f32 {
    if (layer_word(i, 3u) > 0.5) {
        return 3.0 / (16.0 * PI) * (1.0 + cos_t * cos_t);
    }
    return hg(cos_t, layer_mean_g(i));
}

// Samples an outgoing propagation direction around `d` at `p` (a layer by luminance of its
// scattering, then its phase: HG by inversion, Rayleigh by the cubic's closed form).
fn sample_phase(p: vec3<f32>, aerial: bool, d: vec3<f32>, u: f32, xi: vec2<f32>) -> vec3<f32> {
    var wsum = 0.0;
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        wsum = wsum + luminance(layer_scattering(i) * layer_density(i, p.y));
    }
    var pick = u * wsum;
    var chosen = 0u;
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        chosen = i;
        let w = luminance(layer_scattering(i) * layer_density(i, p.y));
        if (pick < w) {
            break;
        }
        pick = pick - w;
    }
    var cos_t = 0.0;
    if (layer_word(chosen, 3u) > 0.5) {
        // Rayleigh: invert the CDF (z = 2(2ξ − 1); cos θ = ∛(z + √(z²+1)) + ∛(z − √(z²+1))).
        let z = 2.0 * (2.0 * xi.x - 1.0);
        let r = sqrt(z * z + 1.0);
        let a = z + r;
        let b = z - r;
        cos_t = sign(a) * pow(abs(a), 1.0 / 3.0) + sign(b) * pow(abs(b), 1.0 / 3.0);
    } else {
        let g = layer_mean_g(chosen);
        if (abs(g) < 1e-3) {
            cos_t = 1.0 - 2.0 * xi.x;
        } else {
            let sq = (1.0 - g * g) / (1.0 + g - 2.0 * g * xi.x);
            cos_t = (1.0 + g * g - sq * sq) / (2.0 * g);
        }
    }
    cos_t = clamp(cos_t, -1.0, 1.0);
    let sin_t = sqrt(max(0.0, 1.0 - cos_t * cos_t));
    let phi = TAU * xi.y;
    let frame = onb(d);
    return normalize(frame[0] * (sin_t * cos(phi)) + frame[1] * (sin_t * sin(phi)) + d * cos_t);
}

fn channel(v: vec3<f32>, c: u32) -> f32 {
    if (c == 0u) {
        return v.x;
    }
    if (c == 1u) {
        return v.y;
    }
    return v.z;
}

// Scattering optical depth (per channel) over [0, t]: free flights are sampled against
// scattering only; absorption is applied exactly (the optical depths are analytic), so a pure
// absorber costs no variance.
fn medium_tau_s(o: vec3<f32>, d: vec3<f32>, t: f32, aerial: bool) -> vec3<f32> {
    var tau = vec3<f32>(0.0);
    for (var i = 0u; i < globals.media.x; i = i + 1u) {
        if (layer_aerial(i) && !aerial) {
            continue;
        }
        tau = tau + layer_scattering(i) * layer_column(i, o, d, 0.0, t);
    }
    return tau;
}

// Free flight along o + t·d over [0, tmax]. Returns (t, weight): t < tmax is a scattering
// event with `weight` = σ_s T / pdf; otherwise the ray passes with `weight` = T / P(no event).
// The channel whose scattering depth is inverted is picked in proportion to `thr`, and the pdf
// is the average over channels (one-sample spectral MIS); T includes absorption.
struct Flight {
    t: f32,
    weight: vec3<f32>,
}

fn free_flight(o: vec3<f32>, d: vec3<f32>, tmax_in: f32, aerial: bool, thr: vec3<f32>) -> Flight {
    let tmax = min(tmax_in, MEDIUM_FAR);
    let taus_max = medium_tau_s(o, d, tmax, aerial);
    let pick = rand();
    let u = rand();
    let tsum = thr.x + thr.y + thr.z;
    var pc = vec3<f32>(1.0 / 3.0);
    if (tsum > 0.0) {
        pc = thr / tsum;
    }
    var c = 2u;
    if (pick < pc.x) {
        c = 0u;
    } else if (pick < pc.x + pc.y) {
        c = 1u;
    }
    let tau_goal = -log(max(1.0 - u, 1e-12));
    if (channel(taus_max, c) <= tau_goal) {
        let tr = exp(-min(medium_tau(o, d, tmax, aerial), vec3<f32>(80.0)));
        let p_none = dot(pc, exp(-min(taus_max, vec3<f32>(80.0))));
        return Flight(tmax_in, select(vec3<f32>(0.0), tr / p_none, p_none > 0.0));
    }
    // Bisection on the monotone channel scattering depth.
    var lo = 0.0;
    var hi = tmax;
    for (var k = 0u; k < 48u; k = k + 1u) {
        let mid = 0.5 * (lo + hi);
        if (channel(medium_tau_s(o, d, mid, aerial), c) < tau_goal) {
            lo = mid;
        } else {
            hi = mid;
        }
        if (hi - lo <= 1e-5 * max(1.0, lo)) {
            break;
        }
    }
    let t = 0.5 * (lo + hi);
    let mp = medium_at(o + d * t, aerial);
    let tr = exp(-min(medium_tau(o, d, t, aerial), vec3<f32>(80.0)));
    let ts = exp(-min(medium_tau_s(o, d, t, aerial), vec3<f32>(80.0)));
    let pdf = dot(pc, mp.sigma_s * ts);
    return Flight(t, select(vec3<f32>(0.0), mp.sigma_s * tr / pdf, pdf > 0.0));
}

// ---------------------------------------------------------------------------------------------
// The volumetric cloud field (ADR-2127): the raster sky's density (`regia::cloud_density`, read
// from the same texels) between two curved shells of the planet. The density has no closed-form
// optical depth, so free flights use delta tracking (Woodcock et al. 1965; Coleman 1968) against
// a constant majorant and shadow rays use ratio tracking (Novák, Selle & Jarosz, "Residual ratio
// tracking for estimating attenuation in participating media", SIGGRAPH Asia 2014). Both are
// unbiased with any density at or below the majorant. Independent of the layers above: a segment
// takes the nearer of the two collisions.

const CLOUD_BASE: u32 = 0u;
const CLOUD_TOP: u32 = 1u;
const CLOUD_SIGMA: u32 = 2u;
const CLOUD_COVERAGE: u32 = 3u;
const CLOUD_DRIFT_X: u32 = 4u;
const CLOUD_DRIFT_Z: u32 = 5u;
const CLOUD_DOMAIN: u32 = 6u;
const CLOUD_WEATHER_N: u32 = 7u;
const CLOUD_SHAPE_TILE: u32 = 8u;
const CLOUD_DETAIL_TILE: u32 = 9u;
const CLOUD_EROSION: u32 = 10u;
const CLOUD_BREAKUP: u32 = 11u;
const CLOUD_BREAKUP_TILE: u32 = 12u;
const CLOUD_SWIRL: u32 = 13u;
const CLOUD_CURL_TILE: u32 = 14u;
const CLOUD_PLANET: u32 = 15u;
const CLOUD_ALBEDO: u32 = 16u;
const CLOUD_G1: u32 = 17u;
const CLOUD_G2: u32 = 18u;
const CLOUD_W1: u32 = 19u;
const CLOUD_MAJORANT: u32 = 20u;
const CLOUD_UNIFORM: u32 = 21u;
const CLOUD_WEATHER_AT: u32 = 22u;
const CLOUD_SHAPE_AT: u32 = 23u;
const CLOUD_DETAIL_AT: u32 = 24u;
const CLOUD_CURL_AT: u32 = 25u;
const CLOUD_NOISE_N: u32 = 26u;
const CLOUD_CURL_N: u32 = 27u;
// Most tracking steps per segment: 200 km through the densest layer the presets draw is about
// 15000. A segment that reaches it ends as if it had left the layer.
const CLOUD_MAX_STEPS: u32 = 32768u;
// Below this transmittance ratio tracking plays Russian roulette.
const CLOUD_RR_BELOW: f32 = 0.05;

fn cloud_on() -> bool {
    return globals.media.z > 0u;
}

fn cloud_word(k: u32) -> f32 {
    return env[globals.media.w + k];
}

fn cloud_offset(k: u32) -> u32 {
    return bitcast<u32>(cloud_word(k));
}

fn cloud_wrap(v: i32, n: i32) -> i32 {
    return ((v % n) + n) % n;
}

// Bilinear with repeat over an RGBA8 image in `textures`: the raster's linear sampler.
fn cloud_sample2(at: u32, n: u32, uv: vec2<f32>) -> vec4<f32> {
    let ni = i32(n);
    let f = uv * f32(n) - vec2<f32>(0.5);
    let i0 = floor(f);
    let w = f - i0;
    let x0 = cloud_wrap(i32(i0.x), ni);
    let y0 = cloud_wrap(i32(i0.y), ni);
    let x1 = cloud_wrap(x0 + 1, ni);
    let y1 = cloud_wrap(y0 + 1, ni);
    let a = unpack4x8unorm(textures[at + u32(y0 * ni + x0)]);
    let b = unpack4x8unorm(textures[at + u32(y0 * ni + x1)]);
    let c = unpack4x8unorm(textures[at + u32(y1 * ni + x0)]);
    let d = unpack4x8unorm(textures[at + u32(y1 * ni + x1)]);
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

// Trilinear with repeat over an RGBA8 volume in `textures` (`(z·n + y)·n + x`).
fn cloud_sample3(at: u32, n: u32, q: vec3<f32>) -> vec4<f32> {
    let ni = i32(n);
    let f = q * f32(n) - vec3<f32>(0.5);
    let i0 = floor(f);
    let w = f - i0;
    let x0 = cloud_wrap(i32(i0.x), ni);
    let y0 = cloud_wrap(i32(i0.y), ni);
    let z0 = cloud_wrap(i32(i0.z), ni);
    let x1 = cloud_wrap(x0 + 1, ni);
    let y1 = cloud_wrap(y0 + 1, ni);
    let z1 = cloud_wrap(z0 + 1, ni);
    let row00 = at + u32((z0 * ni + y0) * ni);
    let row10 = at + u32((z0 * ni + y1) * ni);
    let row01 = at + u32((z1 * ni + y0) * ni);
    let row11 = at + u32((z1 * ni + y1) * ni);
    let c000 = unpack4x8unorm(textures[row00 + u32(x0)]);
    let c100 = unpack4x8unorm(textures[row00 + u32(x1)]);
    let c010 = unpack4x8unorm(textures[row10 + u32(x0)]);
    let c110 = unpack4x8unorm(textures[row10 + u32(x1)]);
    let c001 = unpack4x8unorm(textures[row01 + u32(x0)]);
    let c101 = unpack4x8unorm(textures[row01 + u32(x1)]);
    let c011 = unpack4x8unorm(textures[row11 + u32(x0)]);
    let c111 = unpack4x8unorm(textures[row11 + u32(x1)]);
    let near = mix(mix(c000, c100, w.x), mix(c010, c110, w.x), w.y);
    let far = mix(mix(c001, c101, w.x), mix(c011, c111, w.x), w.y);
    return mix(near, far, w.z);
}

// The normalised density at `p`: the same steps as `density_at` in `clouds.wgsl` with the detail
// always on (no cheap light-march taps in a reference).
fn cloud_density_at(p: vec3<f32>) -> f32 {
    let alt = regia::cloud_density::cloud_altitude(p, cloud_word(CLOUD_PLANET));
    let bottom = cloud_word(CLOUD_BASE);
    let top = cloud_word(CLOUD_TOP);
    if (alt < bottom || alt > top) {
        return 0.0;
    }
    let uniform_density = cloud_word(CLOUD_UNIFORM);
    if (uniform_density >= 0.0) {
        return uniform_density;
    }
    let h = regia::cloud_density::cloud_layer_fraction(alt, bottom, top);
    let drift2 = vec2<f32>(cloud_word(CLOUD_DRIFT_X), cloud_word(CLOUD_DRIFT_Z));
    let weather_uv = regia::cloud_density::density_weather_uv(p.xz, drift2, cloud_word(CLOUD_DOMAIN));
    var w = cloud_sample2(cloud_offset(CLOUD_WEATHER_AT), u32(cloud_word(CLOUD_WEATHER_N)), weather_uv).rg;
    let noise_n = u32(cloud_word(CLOUD_NOISE_N));
    let shape_at = cloud_offset(CLOUD_SHAPE_AT);
    let breakup = cloud_word(CLOUD_BREAKUP);
    if (breakup > 0.0) {
        let bc = regia::cloud_density::density_breakup_coord(p.xz, drift2, cloud_word(CLOUD_BREAKUP_TILE));
        w.x = regia::cloud_density::density_breakup(w.x, breakup, cloud_sample3(shape_at, noise_n, bc));
    }
    let cov = regia::cloud_density::density_cover(w.x, cloud_word(CLOUD_COVERAGE));
    let grad = regia::cloud_density::type_gradient(h, regia::cloud_density::sat(w.y));
    if (cov * grad <= 0.0) {
        return 0.0;
    }
    let drift = vec3<f32>(drift2.x, 0.0, drift2.y);
    let sp = regia::cloud_density::density_shape_coord(p, drift, cloud_word(CLOUD_SHAPE_TILE));
    let base = regia::cloud_density::density_base(cloud_sample3(shape_at, noise_n, sp), cov, grad);
    if (base <= 0.0) {
        return 0.0;
    }
    var dq = regia::cloud_density::density_detail_anchor(p, drift);
    let swirl = cloud_word(CLOUD_SWIRL);
    if (swirl > 0.0) {
        let cuv = regia::cloud_density::density_curl_uv(dq, cloud_word(CLOUD_CURL_TILE));
        let rg = cloud_sample2(cloud_offset(CLOUD_CURL_AT), u32(cloud_word(CLOUD_CURL_N)), cuv).rg;
        dq = regia::cloud_density::density_curl_swirl(dq, rg, swirl, h);
    }
    let dp = regia::cloud_density::density_detail_coord(dq, cloud_word(CLOUD_DETAIL_TILE));
    let d = cloud_sample3(cloud_offset(CLOUD_DETAIL_AT), noise_n, dp);
    return regia::cloud_density::density_erode(base, d, h, cloud_word(CLOUD_EROSION));
}

// The part of [0, tmax] along o + t·d inside the top shell: (t0, t1), and the chord of the base
// shell (no cloud in there): (hole start, hole end). t1 <= t0 when the ray misses the layer.
fn cloud_span(o: vec3<f32>, d: vec3<f32>, tmax: f32) -> vec4<f32> {
    let planet = cloud_word(CLOUD_PLANET);
    let top = regia::cloud_density::cloud_shell(o, d, planet + cloud_word(CLOUD_TOP), planet);
    if (top.y <= 0.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    let bottom = regia::cloud_density::cloud_shell(o, d, planet + cloud_word(CLOUD_BASE), planet);
    return vec4<f32>(max(top.x, 0.0), min(top.y, tmax), bottom.x, bottom.y);
}

// The next tentative collision after `t`: a step of the majorant process, skipping the base
// shell's chord (a landing in there is a null collision, and the process is memoryless).
fn cloud_step(t: f32, inv_majorant: f32, hole: vec2<f32>) -> f32 {
    var next = t - log(max(1.0 - rand(), 1e-12)) * inv_majorant;
    if (next > hole.x && next < hole.y) {
        next = hole.y;
    }
    return next;
}

// Delta tracking along o + t·d over [0, tmax]: the distance of the first real collision, or INF.
fn cloud_flight(o: vec3<f32>, d: vec3<f32>, tmax: f32) -> f32 {
    let majorant = cloud_word(CLOUD_MAJORANT);
    let sigma = cloud_word(CLOUD_SIGMA) * majorant;
    if (sigma <= 0.0) {
        return INF;
    }
    let span = cloud_span(o, d, tmax);
    if (span.y <= span.x) {
        return INF;
    }
    let inv = 1.0 / sigma;
    let hole = span.zw;
    var t = span.x;
    for (var k = 0u; k < CLOUD_MAX_STEPS; k = k + 1u) {
        t = cloud_step(t, inv, hole);
        if (t >= span.y) {
            return INF;
        }
        if (t > hole.x && t <= hole.y) {
            continue;
        }
        if (rand() * majorant < cloud_density_at(o + d * t)) {
            return t;
        }
    }
    return INF;
}

// Ratio tracking along o + t·d over [0, tmax]: an unbiased estimate of the transmittance. Russian
// roulette ends the walk once little is left, without a bias.
fn cloud_transmittance(o: vec3<f32>, d: vec3<f32>, tmax: f32) -> f32 {
    let majorant = cloud_word(CLOUD_MAJORANT);
    let sigma = cloud_word(CLOUD_SIGMA) * majorant;
    if (sigma <= 0.0) {
        return 1.0;
    }
    let span = cloud_span(o, d, tmax);
    if (span.y <= span.x) {
        return 1.0;
    }
    let inv = 1.0 / sigma;
    let hole = span.zw;
    var t = span.x;
    var tr = 1.0;
    for (var k = 0u; k < CLOUD_MAX_STEPS; k = k + 1u) {
        t = cloud_step(t, inv, hole);
        if (t >= span.y) {
            break;
        }
        if (t > hole.x && t <= hole.y) {
            continue;
        }
        tr = tr * (1.0 - min(cloud_density_at(o + d * t) / majorant, 1.0));
        if (tr < CLOUD_RR_BELOW) {
            if (tr <= 0.0 || rand() >= 0.5) {
                return 0.0;
            }
            tr = tr * 2.0;
        }
    }
    return tr;
}

// The two-lobe phase at the scattering angle with cosine `cos_t` (`cos_t = dot(in, out)`
// propagation directions). The sampler below draws exactly this density.
fn cloud_phase(cos_t: f32) -> f32 {
    let w = cloud_word(CLOUD_W1);
    return w * hg(cos_t, cloud_word(CLOUD_G1)) + (1.0 - w) * hg(cos_t, cloud_word(CLOUD_G2));
}

// A propagation direction around `d` from `cloud_phase`.
fn cloud_sample_phase(d: vec3<f32>, u: f32, xi: vec2<f32>) -> vec3<f32> {
    let g = select(cloud_word(CLOUD_G2), cloud_word(CLOUD_G1), u < cloud_word(CLOUD_W1));
    var cos_t = 1.0 - 2.0 * xi.x;
    if (abs(g) >= 1e-3) {
        let sq = (1.0 - g * g) / (1.0 + g - 2.0 * g * xi.x);
        cos_t = (1.0 + g * g - sq * sq) / (2.0 * g);
    }
    cos_t = clamp(cos_t, -1.0, 1.0);
    let sin_t = sqrt(max(0.0, 1.0 - cos_t * cos_t));
    let phi = TAU * xi.y;
    let frame = onb(d);
    return normalize(frame[0] * (sin_t * cos(phi)) + frame[1] * (sin_t * sin(phi)) + d * cos_t);
}

// ---------------------------------------------------------------------------------------------
// Sky bodies: discs with a tabulated radiance (`PtSkyBody`), drawn over the sky table in list
// order (a later body hides an earlier one) and over the sun disc.

fn body_word(i: u32, k: u32) -> f32 {
    return env[globals.bodies.y + i * BODY_WORDS + k];
}

fn body_centre(i: u32) -> vec3<f32> {
    return vec3<f32>(body_word(i, 0u), body_word(i, 1u), body_word(i, 2u));
}

fn body_contains(i: u32, d: vec3<f32>) -> bool {
    return dot(d, body_centre(i)) >= body_word(i, 3u);
}

// Radiance of body `i` toward `d` (0 outside it), bilinear over its table.
fn body_radiance(i: u32, d: vec3<f32>) -> vec3<f32> {
    let c = body_centre(i);
    let cd = dot(d, c);
    if (cd < body_word(i, 3u) || cd <= 0.0) {
        return vec3<f32>(0.0);
    }
    let e1 = vec3<f32>(body_word(i, 4u), body_word(i, 5u), body_word(i, 6u));
    let e2 = vec3<f32>(body_word(i, 8u), body_word(i, 9u), body_word(i, 10u));
    let tan_r = body_word(i, 7u);
    let q = d / cd - c;
    let x = dot(q, e1) / tan_r;
    let y = dot(q, e2) / tan_r;
    let n = u32(body_word(i, 11u));
    let nf = f32(n);
    let fx = clamp((x * 0.5 + 0.5) * nf - 0.5, 0.0, nf - 1.0);
    let fy = clamp((0.5 - y * 0.5) * nf - 0.5, 0.0, nf - 1.0);
    let x0 = u32(floor(fx));
    let y0 = u32(floor(fy));
    let x1 = min(x0 + 1u, n - 1u);
    let y1 = min(y0 + 1u, n - 1u);
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let base = bitcast<u32>(body_word(i, 12u));
    let a = body_texel(base, (y0 * n + x0));
    let b = body_texel(base, (y0 * n + x1));
    let cc = body_texel(base, (y1 * n + x0));
    let dd = body_texel(base, (y1 * n + x1));
    return mix(mix(a, b, tx), mix(cc, dd, tx), ty);
}

fn body_texel(base: u32, k: u32) -> vec3<f32> {
    return vec3<f32>(env[base + k * 3u], env[base + k * 3u + 1u], env[base + k * 3u + 2u]);
}

// No later body covers `d`.
fn body_in_front(i: u32, d: vec3<f32>) -> bool {
    for (var j = i + 1u; j < globals.bodies.x; j = j + 1u) {
        if (body_contains(j, d)) {
            return false;
        }
    }
    return true;
}

// Any body covers `d` (it hides the sun disc there).
fn any_body(d: vec3<f32>) -> bool {
    for (var j = 0u; j < globals.bodies.x; j = j + 1u) {
        if (body_contains(j, d)) {
            return true;
        }
    }
    return false;
}

// The front-most body toward `d`, or NONE.
fn front_body(d: vec3<f32>) -> u32 {
    var found = NONE;
    for (var j = 0u; j < globals.bodies.x; j = j + 1u) {
        if (body_contains(j, d)) {
            found = j;
        }
    }
    return found;
}

// ---------------------------------------------------------------------------------------------
// The airless star field (`outdoor_sky.wgsl` `star_field`): point stars a pixel wide on a 3-D
// direction grid; background only.

// Integer mix in the style of Jarzynski & Olano, JCGT 2020 (`outdoor_sky.wgsl` `star_hash`).
fn star_hash(cell: vec3<i32>) -> f32 {
    var x = bitcast<u32>(cell.x);
    x = x ^ (bitcast<u32>(cell.y) * 0x9E3779B9u);
    x = x ^ (bitcast<u32>(cell.z) * 0x85EBCA6Bu);
    x = x * 747796405u + 2891336453u;
    x = ((x >> ((x >> 28u) + 4u)) ^ x) * 277803737u;
    x = (x >> 22u) ^ x;
    return f32(x) * (1.0 / 4294967296.0);
}

fn star_hash01(cell: vec3<i32>, salt: u32) -> f32 {
    return star_hash(cell + vec3<i32>(i32(salt) * 7919, i32(salt) * 104729, i32(salt) * 1299709));
}

fn star_field(view: vec3<f32>) -> vec3<f32> {
    let peak = globals.stars.x;
    if (peak <= 0.0 || view.y <= -0.02) {
        return vec3<f32>(0.0);
    }
    let scale = 250.0;
    let p = view * scale;
    let cell = vec3<i32>(floor(p));
    let h = star_hash(cell);
    if (h < 0.965) {
        return vec3<f32>(0.0);
    }
    let offset = vec3<f32>(star_hash01(cell, 1u), star_hash01(cell, 2u), star_hash01(cell, 3u)) * 0.5 + 0.25;
    let centre = normalize((vec3<f32>(cell) + offset) / scale);
    let px = max(globals.stars.y, 1e-5);
    let dd = length(view - centre);
    let r = px * 0.7;
    let shape = exp(-(dd * dd) / (r * r));
    let mag = (h - 0.965) / 0.035;
    let bright = peak * (pow(mag, 6.0) + 0.006);
    let warm = star_hash01(cell, 4u);
    let tint = mix(vec3<f32>(0.80, 0.88, 1.10), vec3<f32>(1.10, 0.92, 0.72), warm);
    return tint * (bright * shape);
}

// ---------------------------------------------------------------------------------------------
// Camera lens.

// Shirley & Chiu 1997 concentric square-to-disc map.
fn concentric_disc(u: vec2<f32>) -> vec2<f32> {
    let a = 2.0 * u - vec2<f32>(1.0);
    if (a.x == 0.0 && a.y == 0.0) {
        return vec2<f32>(0.0);
    }
    var r = a.y;
    var theta = PI * 0.5 - PI * 0.25 * (a.x / a.y);
    if (abs(a.x) > abs(a.y)) {
        r = a.x;
        theta = PI * 0.25 * (a.y / a.x);
    }
    return r * vec2<f32>(cos(theta), sin(theta));
}

// ---------------------------------------------------------------------------------------------
// Kernels.

@compute @workgroup_size(64)
fn generate(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let idx = invocation_index(gid, nwg);
    if (idx >= pixel_count()) {
        return;
    }
    if (probe_mode()) {
        probe_generate(idx);
        return;
    }
    rng_init(idx, pass_params.sample_index, 0xffffu);
    let w = globals.dims.x;
    let x = idx % w;
    let y = idx / w;
    var jitter = vec2<f32>(0.5);
    if ((globals.counts.w & FLAG_JITTER) != 0u) {
        jitter = rand2();
    }
    let sx = ((f32(x) + jitter.x) / f32(w)) * 2.0 - 1.0;
    let sy = 1.0 - ((f32(y) + jitter.y) / f32(globals.dims.y)) * 2.0;
    let th = globals.cam_pos.w;
    var dir = normalize(
        globals.cam_fwd.xyz + globals.cam_right.xyz * (sx * th * globals.cam_fwd.w) + globals.cam_up.xyz * (sy * th),
    );
    var origin = globals.cam_pos.xyz;
    if (globals.cam_right.w > 0.0) {
        // Thin lens: every ray through the pixel meets at the focus plane.
        let focus = origin + dir * (globals.cam_up.w / dot(dir, globals.cam_fwd.xyz));
        let lens = concentric_disc(rand2()) * globals.cam_right.w;
        origin = origin + globals.cam_right.xyz * lens.x + globals.cam_up.xyz * lens.y;
        dir = normalize(focus - origin);
    }
    let z = vec4<f32>(0.0);
    paths[idx] = Path(
        vec4<f32>(origin, 0.0),
        vec4<f32>(dir, 1.0),
        vec4<f32>(1.0, 1.0, 1.0, 0.0),
        vec4<f32>(1.0, 1.0, 1.0, 0.0),
        z,
        z,
        z,
        z,
        z,
        z,
        z,
        z,
    );
}

@compute @workgroup_size(64)
fn intersect(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) local: u32,
) {
    trace_lane = local;
    let idx = queued_slot(invocation_index(gid, nwg));
    if (idx == NONE) {
        return;
    }
    let path = paths[idx];
    if (path.dir.w < 0.5) {
        return;
    }
    let hit = trace(path.origin.xyz, path.dir.xyz, INF, false);
    hits[idx * 2u] = vec4<f32>(hit.t, bitcast<f32>(hit.inst), bitcast<f32>(hit.prim), hit.u);
    hits[idx * 2u + 1u] = vec4<f32>(hit.v, 0.0, 0.0, 0.0);
}

// Luminance cap that keeps the color (`cap` 0 = off).
fn cap_luminance(c: vec3<f32>, cap: f32) -> vec3<f32> {
    let l = luminance(c);
    if (cap > 0.0 && l > cap) {
        return c * (cap / l);
    }
    return c;
}

// Caustic-path state (ADR-2433), kept in the unused `throughput.w` of a path: bit 0 a diffuse
// vertex lies behind, bit 1 the vertex that produced the light being credited is mirror-like
// (smooth reflection or a transmission event). A contribution is a caustic when both are set:
// light reached from a diffuse bounce through a specular vertex.
fn caustic_path() -> bool {
    return (u32(path_throughput.w) & 3u) == 3u;
}

// A vertex whose lobes are all (nearly) mirror-like.
fn mirror_vertex(sh: Shading) -> bool {
    return sh.perceptual <= 0.12 && sh.p_diffuse <= 0.02;
}

// Caustic mode (`globals.misc.w`: 0 full, 1 clamped, 2 suppress) and the indirect clamp
// (`globals.media_f.y`, luminance, 0 = off) applied to light credited after the first vertex.
// Both default to off, so the base arithmetic is untouched.
fn limit_indirect(c_in: vec3<f32>) -> vec3<f32> {
    var c = c_in;
    let mode = globals.misc.w;
    if (mode > 0.5 && caustic_path()) {
        if (mode > 1.5) {
            return vec3<f32>(0.0);
        }
        c = cap_luminance(c, globals.media_f.z);
    }
    return cap_luminance(c, globals.media_f.y);
}

fn add_indirect(c_raw: vec3<f32>) {
    let c = limit_indirect(c_raw);
    if (path_frac.w > 0.5) {
        path_vol = vec4<f32>(path_vol.rgb + c, 0.0);
        return;
    }
    let f = path_frac.rgb;
    path_ind_d = vec4<f32>(path_ind_d.rgb + c * f, 0.0);
    path_ind_s = vec4<f32>(path_ind_s.rgb + c * (vec3<f32>(1.0) - f), 0.0);
}

fn add_direct(c: vec3<f32>) {
    if (path_frac.w > 0.5) {
        path_vol = vec4<f32>(path_vol.rgb + c, 0.0);
        return;
    }
    path_direct = vec4<f32>(path_direct.rgb + c, 0.0);
}

// Light arriving at a path vertex: first vertex → `direct`, later → indirect.
fn add_light(bounce: u32, c: vec3<f32>) {
    if (bounce == 0u) {
        add_direct(c);
    } else {
        add_indirect(c);
    }
}

// ---------------------------------------------------------------------------------------------
// Probe mode (`probe.rs`): rays leave probe positions along a randomly rotated spherical
// Fibonacci set (Keinert et al. 2015; one uniform rotation per probe and sample, Shoemake 1992),
// and the radiance each ray brings back is credited to the light it came from, so the baked
// components stay separable (the sun seen directly is live light and never baked).

// Light arriving at the probe, credited to component `comp`. `scattered`: the light reflected or
// scattered at least once on its way (else the probe ray sees it directly). A contribution above
// its luminance cap (`globals.probe_f.z` direct, `.w` scattered; 0 = off) is scaled down to it,
// keeping its color; the removed luminance and the count go to `sky.xy` (unused in probe mode).
fn add_probe(comp: u32, c_in: vec3<f32>, scattered: bool) {
    var c = c_in;
    let cap = select(globals.probe_f.z, globals.probe_f.w, scattered);
    let l = luminance(c);
    if (cap > 0.0 && l > cap) {
        c = c * (cap / l);
        path_sky = vec4<f32>(path_sky.x + (l - cap), path_sky.y + 1.0, 0.0, 0.0);
    }
    switch comp {
        case 0u: {
            path_direct = vec4<f32>(path_direct.rgb + c, 0.0);
        }
        case 1u: {
            path_ind_d = vec4<f32>(path_ind_d.rgb + c, 0.0);
        }
        case 2u: {
            path_ind_s = vec4<f32>(path_ind_s.rgb + c, 0.0);
        }
        default: {
            path_emis = vec4<f32>(path_emis.rgb + c, 0.0);
        }
    }
}

// Next-event light at a path vertex: the probe component in probe mode, else `add_light`.
fn credit(comp: u32, bounce: u32, c: vec3<f32>) {
    if (probe_mode()) {
        add_probe(comp, c, true);
    } else {
        add_light(bounce, c);
    }
}

// Spherical Fibonacci direction `i` of `n`, Y up (`regia_render::gi::fibonacci_direction`).
fn fibonacci_dir(i: u32, n: u32) -> vec3<f32> {
    let golden = PI * (3.0 - sqrt(5.0));
    let y = 1.0 - (2.0 * f32(i) + 1.0) / f32(max(n, 1u));
    let r = sqrt(max(1.0 - y * y, 0.0));
    let theta = golden * f32(i);
    return vec3<f32>(r * cos(theta), y, r * sin(theta));
}

// A uniformly distributed unit quaternion from three uniforms (Shoemake 1992).
fn random_quat(u: vec3<f32>) -> vec4<f32> {
    let a = sqrt(1.0 - u.x);
    let b = sqrt(u.x);
    return vec4<f32>(a * sin(TAU * u.y), a * cos(TAU * u.y), b * sin(TAU * u.z), b * cos(TAU * u.z));
}

fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

// Octahedral decode of `uv` in [0, 1]² (Cigolle et al. 2014; the fold is on −Z, like
// `regia_render::gi`'s `oct_uv` and `regia_asset::baked_gi::oct_decode`).
fn oct_decode(uv: vec2<f32>) -> vec3<f32> {
    var e = uv * 2.0 - vec2<f32>(1.0);
    let z = 1.0 - abs(e.x) - abs(e.y);
    if (z < 0.0) {
        let sx = select(-1.0, 1.0, e.x >= 0.0);
        let sy = select(-1.0, 1.0, e.y >= 0.0);
        e = vec2<f32>((1.0 - abs(e.y)) * sx, (1.0 - abs(e.x)) * sy);
    }
    return normalize(vec3<f32>(e.x, e.y, z));
}

// Writes the probe ray of slot `idx` (probe `idx / rays`, ray `idx % rays`).
fn probe_generate(idx: u32) {
    let rays = globals.probe.x;
    let rec = probe_record(idx / rays);
    // One rotation per (probe, sample) shared by all of the probe's rays.
    rng_init(u32(rec.w), pass_params.sample_index, 0xfffeu);
    let u = vec3<f32>(rand(), rand(), rand());
    let dir = normalize(quat_rotate(random_quat(u), fibonacci_dir(probe_ray_index(idx % rays), rays)));
    let z = vec4<f32>(0.0);
    paths[idx] = Path(
        vec4<f32>(rec.xyz, 0.0),
        vec4<f32>(dir, 1.0),
        vec4<f32>(1.0, 1.0, 1.0, 0.0),
        vec4<f32>(1.0, 1.0, 1.0, 0.0),
        z,
        z,
        z,
        z,
        z,
        z,
        // xyz the probe ray's direction, w its first-hit distance (INF = escaped).
        vec4<f32>(dir, INF),
        z,
    );
}

// Records the probe ray's first hit: distance (`normal.w`) and back face (`albedo.w`).
fn probe_first_hit(hit: Hit, d: vec3<f32>) {
    var dist = INF;
    var back = 0.0;
    if (hit.inst != NONE) {
        dist = hit.t;
        let s = surface_at(hit);
        if (dot(s.ng, d) > 0.0) {
            back = 1.0;
        }
    }
    path_normal.w = dist;
    path_albedo.w = back;
}

// A probe-mode ray that escaped after vertex `bounce`.
fn probe_escaped(bounce: u32, d: vec3<f32>, last_pdf: f32, thr: vec3<f32>) {
    let body = front_body(d);
    if (bounce == 0u) {
        // Seen straight from the probe: the sky and a body (at its lighting scale); the sun disc
        // is direct light the runtime keeps live, and stars light nothing.
        if (body != NONE) {
            add_probe(COMP_FILL, thr * body_radiance(body, d) * body_word(body, 13u), false);
        }
        add_probe(COMP_SKY, thr * sky_radiance(d), false);
        return;
    }
    let e = escaped_light(d, last_pdf);
    add_probe(select(COMP_SUN, COMP_FILL, body != NONE), thr * e.light, true);
    add_probe(COMP_SKY, thr * e.sky, true);
}

// What a ray that escaped after a path vertex (`bounce` ≥ 1) receives toward `d`, MIS-weighted
// against next-event estimation with the solid-angle pdf `last_pdf` (0 = delta): `.light` the
// sun disc or the front sky body (× its light scale), `.sky` the sky table. Kept apart so the
// AOVs split them without subtracting a 10⁹ cd/m² sun from a sum.
struct Escaped {
    light: vec3<f32>,
    sky: vec3<f32>,
}

fn escaped_light(d: vec3<f32>, last_pdf: f32) -> Escaped {
    let body = front_body(d);
    var sky_w = 1.0;
    var light_w = 1.0;
    if (last_pdf > 0.0) {
        sky_w = power_heuristic(last_pdf, sky_pdf(d));
        if (body != NONE) {
            light_w = power_heuristic(last_pdf, body_word(body, 14u));
        } else if (sun_on() && !sun_is_delta()) {
            light_w = power_heuristic(last_pdf, sun_cone_pdf());
        }
    }
    var light = sun_disc(d) * light_w;
    if (body != NONE) {
        light = body_radiance(body, d) * body_word(body, 13u) * light_w;
    }
    return Escaped(light, sky_radiance(d) * sky_w);
}

// Next-event estimation of the sun, the sky and the sky bodies at a path vertex. `bsdf` is
// a closure-free stand-in: the caller passes the vertex kind and its data.
struct Vertex {
    p: vec3<f32>,
    // Surfaces: geometric and shading normals; media: unused.
    ng: vec3<f32>,
    ns: vec3<f32>,
    wo: vec3<f32>,
    // Origin for shadow rays (offset off a surface; the point itself in a medium).
    p_off: vec3<f32>,
    medium: bool,
    aerial: bool,
    // A collision in the cloud field (`medium` is true as well): its own phase.
    cloud: bool,
}

struct Response {
    // Reflected (or scattered) fraction per unit radiance along `l`, cosine included, split
    // into diffuse and specular for the AOVs (a medium reports everything as diffuse).
    diffuse: vec3<f32>,
    specular: vec3<f32>,
    pdf: f32,
}

fn vertex_response(v: Vertex, sh: Shading, l: vec3<f32>) -> Response {
    if (v.cloud) {
        let ph = cloud_phase(dot(-v.wo, l));
        return Response(vec3<f32>(ph), vec3<f32>(0.0), ph);
    }
    if (v.medium) {
        let ph = phase_eval(v.p, v.aerial, dot(-v.wo, l));
        return Response(ph.value, vec3<f32>(0.0), ph.pdf);
    }
    let no_l = dot(v.ns, l);
    if (no_l <= 0.0 || dot(v.ng, l) <= 0.0) {
        return Response(vec3<f32>(0.0), vec3<f32>(0.0), 0.0);
    }
    let f = eval_brdf(sh, v.ns, v.wo, l);
    return Response(f.diffuse * no_l, f.specular * no_l, bsdf_pdf(sh, v.ns, v.wo, l));
}

// What `shade_vertex` hands `shade`: the path vertex and its shading (a zero `Shading` off a
// surface), the throughput after it, and `alive` false when the path ended there (already stored).
// Values, not variables: a Function variable of a struct type breaks VUID-10684 (ADR-7879).
struct ShadeVertex {
    sh: Shading,
    vtx: Vertex,
    thr: vec3<f32>,
    alive: bool,
}

// `sh` with its emission and unlit flag replaced (a material graph's, then the decals').
fn shading_with_emission(sh: Shading, emissive: vec3<f32>, unlit: bool) -> Shading {
    return Shading(sh.base, sh.diffuse_color, sh.f0, emissive, sh.perceptual, sh.rough, sh.coat, sh.coat_roughness, sh.multi_scatter, unlit, sh.lambert, sh.e_o, sh.e_avg, sh.es_o, sh.p_diffuse, sh.p_specular, sh.aniso, sh.aniso_t, sh.aniso_b, sh.alpha_t, sh.alpha_b, sh.transmit, sh.p_transmit, sh.film_base, sh.film_cover, sh.film_ior, sh.film_nm, sh.transmit_alpha);
}

// `sh` with its diffuse colour replaced (the share subsurface scattering leaves).
fn shading_with_diffuse(sh: Shading, diffuse_color: vec3<f32>) -> Shading {
    return Shading(sh.base, diffuse_color, sh.f0, sh.emissive, sh.perceptual, sh.rough, sh.coat, sh.coat_roughness, sh.multi_scatter, sh.unlit, sh.lambert, sh.e_o, sh.e_avg, sh.es_o, sh.p_diffuse, sh.p_specular, sh.aniso, sh.aniso_t, sh.aniso_b, sh.alpha_t, sh.alpha_b, sh.transmit, sh.p_transmit, sh.film_base, sh.film_cover, sh.film_ior, sh.film_nm, sh.transmit_alpha);
}

// The vertex of `shade`'s path segment: a medium scattering event (`scatter_t` < INF), the sky
// (the path ends), or the surface `hit` with its material, emission and subsurface step.
fn shade_vertex(
    idx: u32,
    hit: Hit,
    d: vec3<f32>,
    last_pdf: f32,
    thr: vec3<f32>,
    probe: bool,
    bounce: u32,
    scatter_t: f32,
    bounded: bool,
    at_cloud: bool,
    surface_hit: bool,
) -> ShadeVertex {
    if (scatter_t < INF) {
        // A medium vertex.
        let p = path_origin.xyz + d * scatter_t;
        if (bounce == 0u && !probe) {
            path_frac.w = 1.0;
            path_normal = vec4<f32>(0.0, 0.0, 0.0, scatter_t * dot(d, globals.cam_fwd.xyz));
        }
        // No surface: a zero `Shading` (only the medium's phase is read).
        return ShadeVertex(Shading(), Vertex(p, vec3<f32>(0.0), vec3<f32>(0.0), -d, p, true, bounded, at_cloud), thr, true);
    }
    if (!surface_hit) {
        if (probe) {
            probe_escaped(bounce, d, last_pdf, thr);
        } else if (bounce == 0u) {
            let body = front_body(d);
            var sky = sky_radiance(d);
            if (body != NONE) {
                sky = sky + body_radiance(body, d);
            } else {
                // A body hides the sun disc and the stars behind it (the raster sky's order).
                sky = sky + sun_disc(d) + star_field(d);
            }
            // Seen directly (through the fog in front of it): the `sky` AOV.
            path_sky = vec4<f32>(path_sky.rgb + thr * sky, path_sky.w);
        } else {
            // The sun or a sky body through the first bounce is direct light; the sky is
            // indirect.
            let e = escaped_light(d, last_pdf);
            if (bounce == 1u) {
                add_direct(thr * e.light);
                add_indirect(thr * e.sky);
            } else {
                add_indirect(thr * (e.light + e.sky));
            }
        }
        path_dir.w = 0.0;
        path_throughput = vec4<f32>(thr, path_throughput.w);
        store_path(idx);
        return ShadeVertex(Shading(), Vertex(), thr, false);
    }
    let s = surface_at(hit);
    let wo = -d;
    var ng = s.ng;
    var ns = s.ns;
    // Interpolated normals on the winding's side, so the facing flip below keeps them.
    if (dot(ns, ng) < 0.0) {
        ns = -ns;
    }
    // A visible back face shades with the flipped normals (glTF doubleSided, as in the forward).
    let back_face = dot(ng, wo) < 0.0;
    if (back_face) {
        ng = -ng;
        ns = -ns;
    }
    // The heightfield's smooth normal (before a view normal replaces `ns`).
    let ns_geo = ns;
    let m = materials[s.material];
    // The surface inputs are one variable per member of `SurfaceInputs` (ADR-7879).
    let inputs = surface_inputs(m, s, ns, back_face);
    var in_base = inputs.base;
    var in_metallic = inputs.metallic;
    var in_perceptual = inputs.perceptual;
    var in_n = inputs.n;
    // A material graph replaces the surface stage (ADR-2725); lighting is the one code path.
    let graph_on = is_graph(m);
    var graph_emissive = vec3<f32>(0.0);
    var graph_unlit = false;
    if (graph_on) {
        let g = graph_surface(m, s, hit.inst, ns, wo, select(-1.0, 1.0, !back_face));
        in_base = g.base_color;
        in_metallic = clamp(g.metallic, 0.0, 1.0);
        in_perceptual = clamp(g.roughness, 0.0, 1.0);
        in_n = ns;
        if (dot(g.normal, g.normal) > 1e-12) {
            in_n = normalize(g.normal);
        }
        graph_emissive = g.emissive;
        graph_unlit = g.unlit > 0.5;
    }
    // A heightfield hit the raster camera sees takes the albedo (and, when given, the
    // shading normal) the frame shaded that pixel with (`PtViewAlbedo`): every camera ray,
    // and with `all_bounces` every bounce hit in the image that the heightfield does not
    // hide from the eye. Terrain the camera does not see keeps the map-wide texture and
    // the heightfield's smooth normal.
    let is_terrain = instances[hit.inst].info.x == 1u;
    if (is_terrain && globals.view_albedo.x != NONE && (bounce == 0u || globals.view_albedo.z != 0u)) {
        let ndc = view_ndc(s.p);
        if (ndc.z > 0.0 && (bounce == 0u || seen_from_view_eye(hit.inst, s.p))) {
            let va = view_texture(globals.view_albedo.x, ndc.xy);
            if (va.w > 0.0) {
                in_base = m.base_color.rgb * va.rgb;
            }
            if (globals.view_albedo.y != NONE) {
                let vn = view_texture(globals.view_albedo.y, ndc.xy);
                let n = vn.xyz * 2.0 - vec3<f32>(1.0);
                // Kept on the geometric side (the raster's normal of a cliff texel seen
                // past a silhouette may face away from this face).
                if (vn.w > 0.0 && dot(n, n) > 1e-6 && dot(n, ns) > 0.0) {
                    in_n = normalize(n);
                }
            }
        }
    }
    // @pt_decal_apply_begin
    // @pt_decal_apply_end
    ns = in_n;
    if (dot(ns, wo) <= 0.0) {
        ns = ng;
    }
    let sh_made = make_shading(m, s, SurfaceInputs(in_base, in_metallic, in_perceptual, in_n), ns, wo, back_face);
    var sh_emissive = sh_made.emissive;
    var sh_unlit = sh_made.unlit;
    if (graph_on) {
        sh_emissive = graph_emissive;
        sh_unlit = sh_unlit || graph_unlit;
    }
    // @pt_decal_emit_begin
    // @pt_decal_emit_end
    let sh = shading_with_emission(sh_made, sh_emissive, sh_unlit);

    if (bounce == 0u && !probe) {
        path_albedo = vec4<f32>(sh.base, 0.0);
        path_normal = vec4<f32>(ns, dot(s.p - globals.cam_pos.xyz, globals.cam_fwd.xyz));
        // The first hit's perceptual roughness, for the `roughness` AOV (ADR-2951); `sky.w`
        // is otherwise unused outside probe mode.
        path_sky.w = sh.perceptual;
    }

    // Emission (a one-sided emitter only from its front).
    let emits = !(m.extra.w > 0.5 && back_face);
    if ((max3(sh.emissive) > 0.0 || sh.unlit) && emits) {
        var e = sh.emissive;
        if (sh.unlit) {
            e = sh.base;
        }
        if (probe) {
            var w = 1.0;
            if (last_pdf > 0.0 && s.light != NONE) {
                w = power_heuristic(last_pdf, triangle_light_pdf(s.light, path_origin.xyz, s.p, d));
            }
            // Seen at the first hit: direct; reached by a bounce: scattered.
            add_probe(COMP_EMISSIVE, thr * e * w, bounce > 0u);
        } else if (bounce == 0u) {
            path_emis = vec4<f32>(path_emis.rgb + thr * e, 0.0);
        } else {
            var w = 1.0;
            if (last_pdf > 0.0 && s.light != NONE) {
                w = power_heuristic(last_pdf, triangle_light_pdf(s.light, path_origin.xyz, s.p, d));
            }
            let c = thr * e * w;
            if (bounce == 1u) {
                add_direct(c);
            } else {
                add_indirect(c);
            }
        }
    }
    if (sh.unlit) {
        path_dir.w = 0.0;
        store_path(idx);
        return ShadeVertex(sh, Vertex(), thr, false);
    }
    // A heightfield's facets approximate a smooth surface: rays leave it from that surface
    // (`terminator_point`) and its facing tests use the smooth normal, so a facet tilted
    // away from a grazing sun is not black where the shading normal faces the sun (the
    // shadow-terminator problem; the raster shades the same smooth surface).
    var n_face = ng;
    var origin = s.p;
    if (is_terrain) {
        n_face = ns_geo;
        origin = select(s.p, s.p_shadow, !back_face);
    }
    let vtx = Vertex(s.p, n_face, ns, wo, offset_ray(origin, n_face), false, true, false);
    // Subsurface (ADR-2725): Burley's normalized diffusion by disc projection (Christensen &
    // Burley 2015; King, Kulla, Conty & Fajardo 2013). With probability 1/2 the vertex is the
    // scattered share: the diffuse light is gathered at a point the profile picks around this
    // one (a probe ray along the normal through the sampled disc point finds it on the same
    // instance) and weighted by `tint · R(|x − y|) / pdf(y)`, post-scatter textured (this
    // point's diffuse colour), with no specular there. Otherwise the vertex shades as usual
    // with the diffuse share `1 − tint` that stays. Both branches carry 1 / ½.
    if (m.sss.w > 0.5 && !sh.unlit && !sh.lambert) {
        if (rand() < 0.5) {
            let dd = m.sss.xyz;
            // Pick among the channels with reach only (pack keeps at least one): a channel
            // with d = 0 would put a radius-0 delta the mixture pdf below does not count.
            let live = vec3<f32>(select(0.0, 1.0, dd.x > 0.0), select(0.0, 1.0, dd.y > 0.0), select(0.0, 1.0, dd.z > 0.0));
            let channels = max(live.x + live.y + live.z, 1.0);
            var pick = rand() * channels;
            var dc = dd.z;
            if (live.x > 0.0 && pick < 1.0) {
                dc = dd.x;
            } else {
                pick = pick - live.x;
                if (live.y > 0.0 && pick < 1.0) {
                    dc = dd.y;
                }
            }
            let r = burley_sample_radius(dc, rand());
            let phi = TAU * rand();
            let frame_n = onb(ns);
            let disc = s.p + r * (cos(phi) * frame_n[0] + sin(phi) * frame_n[1]);
            let reach = max(16.0 * max3(dd), r);
            let probe_hit = trace(disc + ns * reach, -ns, 2.0 * reach, false);
            if (probe_hit.inst != hit.inst) {
                // The profile reached past this instance: the light is lost (thin or curved
                // geometry under a wide profile; a slab of the profile's reach keeps it).
                path_dir.w = 0.0;
                store_path(idx);
                return ShadeVertex(sh, vtx, thr, false);
            }
            let s2 = surface_at(probe_hit);
            var ng2 = s2.ng;
            var ns2 = s2.ns;
            if (dot(ns2, ng2) < 0.0) {
                ns2 = -ns2;
            }
            if (dot(ng2, ns) < 0.0) {
                ng2 = -ng2;
                ns2 = -ns2;
            }
            let dist = length(s2.p - s.p);
            let pdf_r = (burley_radius_pdf(dd.x, r) + burley_radius_pdf(dd.y, r) + burley_radius_pdf(dd.z, r)) / channels;
            let pdf_area = pdf_r / (TAU * max(r, 1e-7)) * max(abs(dot(ns, ng2)), 1e-3);
            let profile = vec3<f32>(burley_profile(dd.x, dist), burley_profile(dd.y, dist), burley_profile(dd.z, dist));
            let thr_scattered = thr * m.sss_tint.rgb * profile * (2.0 / max(pdf_area, 1e-20));
            let sh_scattered = Shading(sh.base, sh.diffuse_color, vec3<f32>(0.0), vec3<f32>(0.0), sh.perceptual, sh.rough, 0.0, 0.0, false, false, true, 1.0, 1.0, vec3<f32>(0.0), 1.0, 0.0, false, sh.aniso_t, sh.aniso_b, sh.rough, sh.rough, vec3<f32>(0.0), 0.0, vec3<f32>(0.0), 0.0, 1.0, 0.0, 0.0);
            let vtx_scattered = Vertex(s2.p, ng2, ns2, ns2, offset_ray(s2.p, ng2), false, true, false);
            return ShadeVertex(sh_scattered, vtx_scattered, thr_scattered, true);
        }
        return ShadeVertex(shading_with_diffuse(sh, sh.diffuse_color * (vec3<f32>(1.0) - m.sss_tint.rgb)), vtx, thr * 2.0, true);
    }
    return ShadeVertex(sh, vtx, thr, true);
}

@compute @workgroup_size(64)
fn shade(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) local: u32,
) {
    trace_lane = local;
    let idx = queued_slot(invocation_index(gid, nwg));
    if (idx == NONE) {
        return;
    }
    load_path(idx);
    if (path_dir.w < 0.5) {
        return;
    }
    let bounce = pass_params.bounce;
    rng_init(rng_slot(idx), pass_params.sample_index, bounce);
    let h0 = hits[idx * 2u];
    let h1 = hits[idx * 2u + 1u];
    let hit = Hit(h0.x, bitcast<u32>(h0.y), bitcast<u32>(h0.z), h0.w, h1.x);
    let d = path_dir.xyz;
    let last_pdf = path_origin.w;
    var thr = path_throughput.rgb;
    let probe = probe_mode();
    if (probe && bounce == 0u) {
        probe_first_hit(hit, d);
        if ((globals.counts.w & FLAG_PROBE_GEOMETRY) != 0u) {
            // Relocation rounds read only first-hit distances and faces (ADR-862): no shading,
            // no next-event rays.
            path_dir.w = 0.0;
            store_path(idx);
            return;
        }
    }

    // Participating media along the segment: a free-flight sample either scatters before the
    // surface (or the sky) or passes through with its transmittance weight.
    var scatter_t = INF;
    let surface_hit = hit.inst != NONE;
    // The cloud field's collision on this segment, INF for none. It ends the segment the way a
    // surface does, so the air applies up to it.
    var cloud_t = INF;
    if (cloud_on()) {
        cloud_t = cloud_flight(path_origin.xyz, d, select(MEDIUM_FAR, hit.t, surface_hit));
    }
    let bounded = surface_hit || cloud_t < INF;
    if (media_on()) {
        var seg = escape_distance();
        if (surface_hit) {
            seg = hit.t;
        }
        if (cloud_t < INF) {
            seg = cloud_t;
        }
        let flight = free_flight(path_origin.xyz, d, seg, bounded, thr);
        thr = thr * flight.weight;
        if (flight.t < seg) {
            scatter_t = flight.t;
        }
    }
    let at_cloud = cloud_t < INF && scatter_t == INF;
    if (at_cloud) {
        scatter_t = cloud_t;
        thr = thr * cloud_word(CLOUD_ALBEDO);
    }

    let sv = shade_vertex(idx, hit, d, last_pdf, thr, probe, bounce, scatter_t, bounded, at_cloud, surface_hit);
    if (!sv.alive) {
        return;
    }
    let sh = sv.sh;
    let vtx = sv.vtx;
    thr = sv.thr;

    // Caustic tracking: is this vertex mirror-like (bit 1; bit 0, a diffuse vertex behind, is
    // kept)? Light credited from here on, and the light the next ray reaches, is judged by it.
    let mirror = !vtx.medium && mirror_vertex(sh);
    path_throughput.w = f32((u32(path_throughput.w) & 1u) | select(0u, 2u, mirror));

    // Sun.
    if (sun_on()) {
        let delta = sun_is_delta();
        var l = globals.sun_dir.xyz;
        let xi = rand2();
        if (!delta) {
            l = sample_cone(globals.sun_dir.xyz, globals.sun_l.w, xi);
        }
        var visible = 0.0;
        let facing = vtx.medium || dot(vtx.ng, l) > 0.0;
        if (facing && !any_body(l) && !occluded(vtx.p_off, l, INF)) {
            visible = 1.0;
        }
        if (bounce == 0u && !vtx.medium && !probe) {
            path_albedo.w = visible;
        }
        if (visible > 0.0) {
            let r = vertex_response(vtx, sh, l);
            var c = (r.diffuse + r.specular) * globals.sun_l.rgb * escape_transmittance(vtx.p_off, l);
            if (!delta) {
                let pdf = sun_cone_pdf();
                c = c * power_heuristic(pdf, r.pdf) / pdf;
            }
            credit(COMP_SUN, bounce, thr * c);
        }
    }
    // The frame's extra directional lights, as delta suns (ADR-7254; no slot on in a one-sun
    // scene, so its random stream and bytes are unchanged).
    for (var xi = 0u; xi < 3u; xi = xi + 1u) {
        let x_toward = globals.extra_sun[2u * xi];
        if (x_toward.w > 0.5) {
            let l = x_toward.xyz;
            let facing = vtx.medium || dot(vtx.ng, l) > 0.0;
            if (facing && !occluded(vtx.p_off, l, INF)) {
                let r = vertex_response(vtx, sh, l);
                let c = (r.diffuse + r.specular) * globals.extra_sun[2u * xi + 1u].rgb * escape_transmittance(vtx.p_off, l);
                credit(COMP_SUN, bounce, thr * c);
            }
        }
    }

    // Sky.
    if (sky_mode() != 0u) {
        let smp = sample_sky(rand2());
        let l = smp.xyz;
        let pdf = smp.w;
        if (pdf > 0.0 && (vtx.medium || dot(vtx.ng, l) > 0.0)) {
            let r = vertex_response(vtx, sh, l);
            if (max3(r.diffuse + r.specular) > 0.0 && !occluded(vtx.p_off, l, INF)) {
                let li = sky_radiance(l) * escape_transmittance(vtx.p_off, l);
                let w = power_heuristic(pdf, r.pdf) / pdf;
                let cd = thr * r.diffuse * li * w;
                let cs = thr * r.specular * li * w;
                if (probe) {
                    add_probe(COMP_SKY, cd + cs, true);
                } else if (bounce == 0u && !vtx.medium) {
                    path_ind_d = vec4<f32>(path_ind_d.rgb + cd, 0.0);
                    path_ind_s = vec4<f32>(path_ind_s.rgb + cs, 0.0);
                } else {
                    add_indirect(cd + cs);
                }
            }
        }
    }

    // Sky bodies (each one cone sample; a body behind a later one is hidden there).
    for (var b = 0u; b < globals.bodies.x; b = b + 1u) {
        let xi = rand2();
        let l = sample_cone(body_centre(b), body_word(b, 15u), xi);
        if (!(vtx.medium || dot(vtx.ng, l) > 0.0) || !body_in_front(b, l)) {
            continue;
        }
        let li = body_radiance(b, l) * body_word(b, 13u);
        if (max3(li) <= 0.0) {
            continue;
        }
        let r = vertex_response(vtx, sh, l);
        if (max3(r.diffuse + r.specular) > 0.0 && !occluded(vtx.p_off, l, INF)) {
            let pdf = body_word(b, 14u);
            let c = (r.diffuse + r.specular) * li * escape_transmittance(vtx.p_off, l) * (power_heuristic(pdf, r.pdf) / pdf);
            credit(COMP_FILL, bounce, thr * c);
        }
    }

    // One local light, power-weighted.
    if (globals.counts.x > 0u) {
        let pick = rand();
        let xi = rand2();
        let index = pick_light(pick);
        let sel = lights[index].color.w;
        let ls = sample_light(index, vtx.p, xi);
        // A bake keeps local lights live: only emissive surfaces (triangle lights) are baked.
        let live = probe && lights[index].refs.z != LIGHT_TRIANGLE;
        if (!live && sel > 0.0 && max3(ls.li) > 0.0 && (vtx.medium || dot(vtx.ng, ls.l) > 0.0)) {
            let r = vertex_response(vtx, sh, ls.l);
            let tmax = ls.dist * (1.0 - 1e-4);
            if (max3(r.diffuse + r.specular) > 0.0 && !occluded(vtx.p_off, ls.l, tmax)) {
                // Delta lights: no pdf. Sphere lights are not geometry, so BSDF rays never reach
                // them and NEE carries their full weight; emissive triangles are MIS-weighted.
                var light_pdf = sel;
                var w = 1.0;
                if (ls.pdf > 0.0) {
                    light_pdf = sel * ls.pdf;
                    if (lights[index].refs.z == LIGHT_TRIANGLE) {
                        w = power_heuristic(light_pdf, r.pdf);
                    }
                }
                // Every layer applies between two points of the scene.
                var tr = medium_transmittance(vtx.p_off, ls.l, ls.dist, true);
                if (cloud_on()) {
                    tr = tr * cloud_transmittance(vtx.p_off, ls.l, ls.dist);
                }
                let c = thr * (r.diffuse + r.specular) * ls.li * tr * (w / light_pdf);
                credit(COMP_EMISSIVE, bounce, c);
            }
        }
    }

    // Continue the path.
    if (bounce + 1u >= globals.dims.z) {
        path_dir.w = 0.0;
        store_path(idx);
        return;
    }
    var l: vec3<f32>;
    var pdf: f32;
    var t: vec3<f32>;
    var next_origin: vec3<f32>;
    if (!vtx.medium && sh.p_transmit > 0.0 && rand() < sh.p_transmit) {
        // Thin transmission: straight through (a delta lobe, so what it reaches takes no MIS
        // weight: pdf 0 like a camera ray).
        if (bounce == 0u) {
            path_frac = vec4<f32>(vec3<f32>(0.0), path_frac.w);
        }
        l = d;
        if (sh.transmit_alpha > 0.0) {
            // Rough transmission (ADR-2075): the weight stays the sheet's, the direction spreads.
            // Nothing connects to a light through this lobe, so what it reaches also takes no MIS
            // weight. A direction the geometric normal puts on the viewer's side goes straight on.
            let spread = regia::pbr::rough_transmission::sample_direction(vtx.wo, vtx.ns, sh.transmit_alpha, rand2());
            if (dot(vtx.ng, spread) * dot(vtx.ng, d) > 0.0) {
                l = spread;
            }
        }
        pdf = 0.0;
        t = thr * sh.transmit / sh.p_transmit;
        next_origin = offset_ray(vtx.p, -vtx.ng);
        // A transmission event is specular whatever the surface's reflection lobes are.
        path_throughput.w = f32(u32(path_throughput.w) | 2u);
    } else if (vtx.cloud) {
        let u = rand();
        let xi = rand2();
        l = cloud_sample_phase(d, u, xi);
        pdf = cloud_phase(dot(d, l));
        if (pdf <= 0.0) {
            path_dir.w = 0.0;
            store_path(idx);
            return;
        }
        // The sampler draws exactly the phase, so its value over its pdf is 1.
        t = thr;
        next_origin = vtx.p;
    } else if (vtx.medium) {
        let u = rand();
        let xi = rand2();
        l = sample_phase(vtx.p, vtx.aerial, d, u, xi);
        let ph = phase_eval(vtx.p, vtx.aerial, dot(d, l));
        pdf = ph.pdf;
        if (pdf <= 0.0) {
            path_dir.w = 0.0;
            store_path(idx);
            return;
        }
        t = thr * ph.value / pdf;
        next_origin = vtx.p;
    } else {
        l = sample_bsdf_dir(sh, vtx.ns, vtx.wo);
        pdf = bsdf_pdf(sh, vtx.ns, vtx.wo, l);
        let no_l = dot(vtx.ns, l);
        if (pdf <= 0.0 || no_l <= 0.0 || dot(vtx.ng, l) <= 0.0) {
            path_dir.w = 0.0;
            store_path(idx);
            return;
        }
        let f = eval_brdf(sh, vtx.ns, vtx.wo, l);
        let total = f.diffuse + f.specular;
        if (bounce == 0u) {
            path_frac = vec4<f32>(select(vec3<f32>(0.0), f.diffuse / total, total > vec3<f32>(0.0)), path_frac.w);
        }
        t = thr * total * no_l / pdf;
        next_origin = vtx.p_off;
    }
    if (bounce + 1u >= globals.counts.z) {
        let q = min(max3(t), 0.95);
        if (rand() >= q) {
            path_dir.w = 0.0;
            store_path(idx);
            return;
        }
        t = t / q;
    }
    if (!finite(t)) {
        path_dir.w = 0.0;
        store_path(idx);
        return;
    }
    // A diffuse-class vertex (the path just left a non-mirror reflection) sets bit 0.
    let state = u32(path_throughput.w);
    let diffuse_seen = (state & 1u) | select(1u, 0u, (state & 2u) != 0u || vtx.medium);
    path_throughput = vec4<f32>(t, f32(diffuse_seen | (state & 2u)));
    path_origin = vec4<f32>(next_origin, pdf);
    path_dir = vec4<f32>(l, 1.0);
    store_path(idx);
    enqueue(idx);
}

// The indirect dispatch of a compacted bounce (ADR-862): workgroup counts for the input queue of
// `pass_params.bounce` into `queue_count[2..5]` (2-D past 65535 groups, as the host does), and
// the output queue emptied for this bounce's `shade`.
@compute @workgroup_size(1)
fn queue_dispatch() {
    let q = pass_params.bounce & 1u;
    let groups = max((atomicLoad(&queue_count[q]) + 63u) / 64u, 1u);
    let gx = min(groups, 65535u);
    atomicStore(&queue_count[2], gx);
    atomicStore(&queue_count[3], (groups + gx - 1u) / gx);
    atomicStore(&queue_count[4], 1u);
    atomicStore(&queue_count[q ^ 1u], 0u);
}

@compute @workgroup_size(64)
fn accumulate(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let idx = invocation_index(gid, nwg);
    if (idx >= pixel_count()) {
        return;
    }
    let path = paths[idx];
    let beauty = path.direct.rgb + path.ind_d.rgb + path.ind_s.rgb + path.emis.rgb + path.sky.rgb + path.vol.rgb;
    let base = idx * AOV_COUNT;
    if (!finite(beauty)) {
        // Count the rejected sample; a non-zero count is a tracer bug, reported in the metadata.
        accum[base + AOV_VOLUMETRIC].w = accum[base + AOV_VOLUMETRIC].w + 1.0;
        return;
    }
    accum[base + AOV_BEAUTY] = accum[base + AOV_BEAUTY] + vec4<f32>(beauty, 1.0);
    // w: the first hit's perceptual roughness (ADR-2951).
    accum[base + AOV_ALBEDO] = accum[base + AOV_ALBEDO] + vec4<f32>(path.albedo.rgb, path.sky.w);
    accum[base + AOV_NORMAL_DEPTH] = accum[base + AOV_NORMAL_DEPTH] + path.normal;
    accum[base + AOV_DIRECT] = accum[base + AOV_DIRECT] + vec4<f32>(path.direct.rgb, 0.0);
    accum[base + AOV_IND_DIFFUSE] = accum[base + AOV_IND_DIFFUSE] + vec4<f32>(path.ind_d.rgb, 0.0);
    accum[base + AOV_IND_SPECULAR] = accum[base + AOV_IND_SPECULAR] + vec4<f32>(path.ind_s.rgb, 0.0);
    accum[base + AOV_EMISSIVE] = accum[base + AOV_EMISSIVE] + vec4<f32>(path.emis.rgb, 0.0);
    accum[base + AOV_SKY] = accum[base + AOV_SKY] + vec4<f32>(path.sky.rgb, 0.0);
    accum[base + AOV_VOLUMETRIC] = accum[base + AOV_VOLUMETRIC] + vec4<f32>(path.vol.rgb, 0.0);
    accum[base + AOV_SHADOW] = accum[base + AOV_SHADOW] + vec4<f32>(path.albedo.w, 0.0, 0.0, 0.0);
    accum[base + AOV_BEAUTY_SQ] = accum[base + AOV_BEAUTY_SQ] + vec4<f32>(beauty * beauty, 0.0);
}

// Probe mode: folds this sample's rays into the per-probe sums. Invocations `0..probes` gather
// the SH projections and the stats of one probe each; the rest gather one octahedral moment
// texel each (`probes × res²`), weighting every ray by `max(0, cos)^exponent` around the texel
// direction (Majercik et al. 2019).
@compute @workgroup_size(64)
fn probe_accumulate(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let idx = invocation_index(gid, nwg);
    let probes = globals.probe.y;
    let rays = globals.probe.x;
    let res = globals.probe.w;
    let texels = res * res;
    let stride = PROBE_MOMENTS + texels;
    let far = globals.probe_f.x;
    if (idx < probes) {
        let base = idx * stride;
        // The 12 SH sums (4 components × rgb, `base + 0..12`), one variable each: a Function
        // array would carry an explicit layout (VUID-10684, ADR-7879).
        var sh0 = vec4<f32>(0.0);
        var sh1 = vec4<f32>(0.0);
        var sh2 = vec4<f32>(0.0);
        var sh3 = vec4<f32>(0.0);
        var sh4 = vec4<f32>(0.0);
        var sh5 = vec4<f32>(0.0);
        var sh6 = vec4<f32>(0.0);
        var sh7 = vec4<f32>(0.0);
        var sh8 = vec4<f32>(0.0);
        var sh9 = vec4<f32>(0.0);
        var sh10 = vec4<f32>(0.0);
        var sh11 = vec4<f32>(0.0);
        var stats = accum[base + PROBE_STATS];
        var back = accum[base + PROBE_BACK];
        var front = accum[base + PROBE_FRONT];
        var clamped = accum[base + PROBE_CLAMP];
        for (var k = 0u; k < rays; k = k + 1u) {
            let path = paths[idx * rays + k];
            let c0 = path.direct.rgb;
            let c1 = path.ind_d.rgb;
            let c2 = path.ind_s.rgb;
            let c3 = path.emis.rgb;
            if (!finite(c0 + c1 + c2 + c3)) {
                stats.w = stats.w + 1.0;
                continue;
            }
            clamped = clamped + vec4<f32>(path.sky.xy, 0.0, 0.0);
            let dir = path.normal.xyz;
            let y = vec4<f32>(SH_Y00, SH_Y1 * dir.x, SH_Y1 * dir.y, SH_Y1 * dir.z);
            sh0 = sh0 + c0.r * y;
            sh1 = sh1 + c0.g * y;
            sh2 = sh2 + c0.b * y;
            sh3 = sh3 + c1.r * y;
            sh4 = sh4 + c1.g * y;
            sh5 = sh5 + c1.b * y;
            sh6 = sh6 + c2.r * y;
            sh7 = sh7 + c2.g * y;
            sh8 = sh8 + c2.b * y;
            sh9 = sh9 + c3.r * y;
            sh10 = sh10 + c3.g * y;
            sh11 = sh11 + c3.b * y;
            let dist = path.normal.w;
            stats.x = stats.x + 1.0;
            if (dist < INF) {
                stats.z = stats.z + 1.0;
                if (path.albedo.w > 0.5) {
                    stats.y = stats.y + 1.0;
                    if (back.w == 0.0 || dist < back.w) {
                        back = vec4<f32>(dir, dist);
                    }
                } else if (front.w == 0.0 || dist < front.w) {
                    front = vec4<f32>(dir, dist);
                }
            }
        }
        accum[base + 0u] = accum[base + 0u] + sh0;
        accum[base + 1u] = accum[base + 1u] + sh1;
        accum[base + 2u] = accum[base + 2u] + sh2;
        accum[base + 3u] = accum[base + 3u] + sh3;
        accum[base + 4u] = accum[base + 4u] + sh4;
        accum[base + 5u] = accum[base + 5u] + sh5;
        accum[base + 6u] = accum[base + 6u] + sh6;
        accum[base + 7u] = accum[base + 7u] + sh7;
        accum[base + 8u] = accum[base + 8u] + sh8;
        accum[base + 9u] = accum[base + 9u] + sh9;
        accum[base + 10u] = accum[base + 10u] + sh10;
        accum[base + 11u] = accum[base + 11u] + sh11;
        accum[base + PROBE_STATS] = stats;
        accum[base + PROBE_BACK] = back;
        accum[base + PROBE_FRONT] = front;
        accum[base + PROBE_CLAMP] = clamped;
        return;
    }
    let t = idx - probes;
    if (t >= probes * texels) {
        return;
    }
    let p = t / texels;
    let texel = t % texels;
    let uv = (vec2<f32>(f32(texel % res), f32(texel / res)) + vec2<f32>(0.5)) / f32(res);
    let tdir = oct_decode(uv);
    var m = vec4<f32>(0.0);
    for (var k = 0u; k < rays; k = k + 1u) {
        let n = paths[p * rays + k].normal;
        let w = pow(max(dot(tdir, n.xyz), 0.0), globals.probe_f.y);
        if (w > 0.0) {
            let dist = min(n.w, far);
            m = m + vec4<f32>(w * dist, w * dist * dist, w, 0.0);
        }
    }
    let at = p * stride + PROBE_MOMENTS + texel;
    accum[at] = accum[at] + m;
}
