// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::occluder_geometry

// Large-mesh occluders (`mesh_occluders.rs`): their uniform layout and the form factors of the polygons and discs they cast.
// Split out of `lab_forward.wgsl` unchanged (ADR-1552); imported there by name.

// BEGIN mesh occluder geometry (`regia_render::mesh_occluders`): the structs and the pure
// form factors. This block (forward_occluder_geometry.wgsl) and the `mesh occluders` block
// (lab_forward.wgsl, the functions that read `mesh_occ`) are copied, in that order, into
// regia_terrain's terrain.wgsl (test `occluder_wgsl_copies_match`); `occluder_light` and
// `reflection_shadow` in mesh_occluders.rs are their CPU mirror. No function-local arrays
// (ADR-026): polygons are clipped edge by edge as their vertices are read.
struct MeshOccluder {
    // xyz = box centre, w = ground plane height.
    center: vec4<f32>,
    // Unit axes, w = half extent.
    axes: array<vec4<f32>, 3>,
    // Face radiance (+A0, −A0, +A1, −A1, +A2, −A2). w: [0] bounding radius, [1] shadow vertex
    // count, [2]/[3] shadow polygon centre x/z, [4] its radius, [5] its area.
    faces: array<vec4<f32>, 6>,
    // Sun shadow polygon on the ground plane, two XZ vertices per row.
    shadow: array<vec4<f32>, 3>,
}

struct MeshOccluders {
    // x = count (0: no term), y = fill shadow angular radius, z = far-band blob count, w =
    // proxies dropped (not read here).
    header: vec4<f32>,
    sky: vec4<f32>,
    ground: vec4<f32>,
    ground_sun: vec4<f32>,
    fill_dir: vec4<f32>,
    // The proxy grid over the XZ bounds of every reach sphere: xy = origin (m), z = cells per
    // metre (0: none). w = the far blobs' largest XZ reach (m).
    grid: vec4<f32>,
    // Per cell of the 32 × 32 grid (row-major, z rows), the occluders whose reach sphere's XZ
    // disc meets it (bit i = occluder i), four cells per row.
    cells: array<vec4<u32>, 256>,
    // Per occluder, a sphere holding every point its terms reach (xyz centre, w radius).
    bounds: array<vec4<f32>, 32>,
    // Per occluder, its sun shadow polygon for reflections: xy = centre (x, z), z = radius
    // (−1: no polygon), w = ground height.
    shadow_bounds: array<vec4<f32>, 32>,
    items: array<MeshOccluder, 32>,
    // Per occluder (four per row), how far its full term has faded into its contact blob at
    // the far band's boundary (0: full term, no blob; ADR-7970).
    near_fade: array<vec4<f32>, 8>,
    // The far band's contact blobs, sorted by x: xy = footprint centre (x, z), z = footprint
    // radius, w = ground height; `header.z` are valid.
    far: array<vec4<f32>, 128>,
    // Per far blob (four per row), its top above its ground.
    far_top: array<vec4<f32>, 32>,
}

struct OccLight {
    // Irradiance / π to add (negative where the ground or sky is hidden by something darker).
    delta: vec3<f32>,
    // Visibility of the sky body's fill.
    fill_vis: f32,
}

// acos on [−1, 1] within 6.7e−5 rad (Abramowitz & Stegun 4.4.45), cheaper than the builtin.
fn occ_acos(x: f32) -> f32 {
    let ax = abs(x);
    let r = sqrt(1.0 - ax) * (1.5707288 + ax * (-0.2121144 + ax * (0.0742610 - 0.0187293 * ax)));
    return select(r, 3.14159265 - r, x < 0.0);
}

// One edge of Lambert's polygon formula: θ · n·normalize(a × b), unit vertex directions
// relative to the receiver.
fn occ_edge(an: vec3<f32>, bn: vec3<f32>, n: vec3<f32>) -> f32 {
    let c = clamp(dot(an, bn), -1.0, 1.0);
    let s = sqrt(max(1.0 - c * c, 1e-12));
    return dot(cross(an, bn), n) * (occ_acos(c) / s);
}

fn occ_unit(v: vec3<f32>) -> vec3<f32> {
    return v / max(length(v), 1e-6);
}

// Far-field view factor of a flat patch of area `area` and normal `face_n` whose centre is at
// `fc` (relative to the receiver) as a disc: A·cosθ_face·cosθ_receiver / (π d² + A).
fn occ_disc_ff(area: f32, fc: vec3<f32>, face_n: vec3<f32>, n: vec3<f32>) -> f32 {
    let d2 = max(dot(fc, fc), 1e-4);
    let dir = fc / sqrt(d2);
    return area * max(-dot(dir, face_n), 0.0) * max(dot(dir, n), 0.0) / (3.14159265 * d2 + area);
}

// Whether the ray o + t·d (t ≥ 0, box frame) hits the box of half extents `h`.
fn occ_slab_hit(o: vec3<f32>, d: vec3<f32>, h: vec3<f32>) -> bool {
    let inv = 1.0 / select(d, vec3<f32>(1e-6), abs(d) < vec3<f32>(1e-6));
    let ta = (-h - o) * inv;
    let tb = (h - o) * inv;
    let t0 = max(max(max(min(ta.x, tb.x), min(ta.y, tb.y)), min(ta.z, tb.z)), 0.0);
    let t1 = min(min(max(ta.x, tb.x), max(ta.y, tb.y)), max(ta.z, tb.z));
    return t0 <= t1;
}

// Share of the sky and fill a contact blob hides at `p` (normal `n`; ADR-7970): the on-axis
// view factor R² / (R² + h²) of a disc of the footprint radius R at half the top h above the
// receiver (a differential area to a parallel coaxial disc), full within 0.6 R of the footprint
// centre `c` and gone at 1.25 R, faded out above half the top and by the receiver's cosine.
fn occ_blob(p: vec3<f32>, n: vec3<f32>, c: vec2<f32>, radius_in: f32, ground_y: f32, top_in: f32) -> f32 {
    let radius = max(radius_in, 0.05);
    let top = max(top_in, 0.05);
    let d = distance(p.xz, c);
    let radial = 1.0 - smoothstep(0.6 * radius, 1.25 * radius, d);
    let vertical = 1.0 - smoothstep(0.5 * top, top, abs(p.y - ground_y));
    let h = 0.5 * top;
    let strength = radius * radius / (radius * radius + h * h);
    return strength * radial * vertical * clamp(n.y, 0.0, 1.0);
}
// END mesh occluder geometry
