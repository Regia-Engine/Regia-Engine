// SPDX-License-Identifier: MPL-2.0
// GPU irradiance probes: trace probe rays and relocate probes (compute).
// Composed as ddgi_common.wgsl + ddgi_sample.wgsl + this file.
//
// Rays hit the terrain heightfield exactly, through a maximum-height pyramid
// (Tevs, Ihrke, Seidel, "Maximum Mipmaps for Fast, Accurate, and Scalable Dynamic Height
// Field Rendering", I3D 2008), and meshes through per-mesh signed-distance bricks (Hart,
// "Sphere Tracing", 1996). A hit is lit by the sun (a shadow ray through the same scene),
// by the previous frame's probes (multi-bounce, Majercik et al. 2019 § 5) and by its own
// emission; a miss sees the sky map.

@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read_write> ddgi_data: array<u32>;
@group(0) @binding(2) var<storage, read_write> ddgi_rays: array<vec4<f32>>;

struct Heightfield {
    // x = half width (m), y = world width (m), z = height scale, w = height offset.
    map: vec4<f32>,
    // x = samples per side, y = max-pyramid levels, z = 1 when bound, w = 1 with an albedo map.
    size: vec4<u32>,
    // rgb = ground albedo; w = cone footprint (cells per metre of ray), negative for the
    // mean-height coarse surface.
    albedo: vec4<f32>,    // x = the sun shadow rays' cone footprint, negative for the mean-height coarse surface;
    // yzw = the local origin in the heightfield's space (zero until a floating-origin rebase).
    shadow: vec4<f32>,
}

@group(1) @binding(0) var<uniform> hf: Heightfield;
@group(1) @binding(1) var hf_max: texture_2d<f32>;
@group(1) @binding(2) var hf_heights: texture_2d<f32>;
// Ground albedo over the map (size.w = 1 when bound; else `albedo`).
@group(1) @binding(3) var hf_albedo: texture_2d<f32>;
@group(1) @binding(4) var hf_albedo_samp: sampler;

// Holes through the heightfield (tunnel mouths, `SharedPassDraw::gi_holes`): [count, 0, 0, 0],
// then per window eight words (centre x, z, cos and sin of the rotation, 1 / half extent x, z,
// two zero; f32 bits), then each window's HF_HOLE_TEXELS² coverage bytes, four a word. Inside a
// hole (bilinear coverage ≥ 50 %, the terrain draw's `in_hole`) there is no ground: rays pass
// the surface, a probe there is not buried, and a ray that went down a hole and runs into the
// ground beside it below the surface meets the rock wall of the shaft.
@group(1) @binding(6) var<storage, read> hf_holes: array<u32>;
const HF_HOLE_TEXELS: u32 = 256u;

fn hf_hole_texel(cov: u32, x: i32, z: i32) -> f32 {
    let last = i32(HF_HOLE_TEXELS) - 1;
    let i = u32(clamp(z, 0, last)) * HF_HOLE_TEXELS + u32(clamp(x, 0, last));
    let word = hf_holes[cov + i / 4u];
    return f32((word >> ((i % 4u) * 8u)) & 255u) / 255.0;
}

fn hf_holed() -> bool {
    return hf_holes[0] > 0u;
}

// True where world XZ lies in a hole (`HoleWindow::coverage_at` mirrors it).
fn hf_in_hole(xz: vec2<f32>) -> bool {
    let count = hf_holes[0];
    let cov0 = 4u + count * 8u;
    for (var w = 0u; w < count; w = w + 1u) {
        let o = 4u + w * 8u;
        let dd = xz - vec2<f32>(bitcast<f32>(hf_holes[o]), bitcast<f32>(hf_holes[o + 1u]));
        let cs = vec2<f32>(bitcast<f32>(hf_holes[o + 2u]), bitcast<f32>(hf_holes[o + 3u]));
        let inv = vec2<f32>(bitcast<f32>(hf_holes[o + 4u]), bitcast<f32>(hf_holes[o + 5u]));
        let local = vec2<f32>(cs.x * dd.x + cs.y * dd.y, -cs.y * dd.x + cs.x * dd.y);
        let uv = local * inv * 0.5 + vec2<f32>(0.5);
        if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
            continue;
        }
        let f = uv * f32(HF_HOLE_TEXELS) - vec2<f32>(0.5);
        let i0 = vec2<i32>(floor(f));
        let t = f - floor(f);
        let cov = cov0 + w * (HF_HOLE_TEXELS * HF_HOLE_TEXELS / 4u);
        let a = hf_hole_texel(cov, i0.x, i0.y) + (hf_hole_texel(cov, i0.x + 1, i0.y) - hf_hole_texel(cov, i0.x, i0.y)) * t.x;
        let b = hf_hole_texel(cov, i0.x, i0.y + 1) + (hf_hole_texel(cov, i0.x + 1, i0.y + 1) - hf_hole_texel(cov, i0.x, i0.y + 1)) * t.x;
        if (a + (b - a) * t.y >= 0.5) {
            return true;
        }
    }
    return false;
}

fn hf_albedo_at(p: vec3<f32>) -> vec3<f32> {
    if (hf.size.w == 0u) {
        return hf.albedo.rgb;
    }
    let uv = (p.xz + hf.shadow.yw + vec2<f32>(hf.map.x)) / hf.map.y;
    return textureSampleLevel(hf_albedo, hf_albedo_samp, uv, 0.0).rgb;
}

struct SdfInstance {
    // Rows of the world → object affine transform.
    w2l0: vec4<f32>,
    w2l1: vec4<f32>,
    w2l2: vec4<f32>,
    // World bounds; aabb_min.w = hit distance (object units).
    aabb_min: vec4<f32>,
    aabb_max: vec4<f32>,
    // xyz = object-space centre of voxel 0.
    brick_origin: vec4<f32>,
    // xyz = voxel edge (object units).
    brick_voxel: vec4<f32>,
    // x = first value, yzw = voxels per axis.
    brick: vec4<u32>,
    // rgb = mean albedo, w = the material's perceptual roughness (`Hit::rough`).
    albedo: vec4<f32>,
    // rgb = mean emitted radiance, w = the material's metallic factor (`Hit::metal`).
    emissive: vec4<f32>,
}

@group(2) @binding(0) var<storage, read> sdf_values: array<f32>;
@group(2) @binding(1) var<storage, read> sdf_instances: array<SdfInstance>;
@group(2) @binding(2) var<storage, read> sdf_lists: array<u32>;

@group(3) @binding(0) var sky_tex: texture_2d<f32>;
@group(3) @binding(1) var sky_samp: sampler;

fn probe_store(index: u32, offset: vec4<f32>, tag: vec4<i32>, stamp: u32) {
    let b = index * DDGI_PROBE_WORDS;
    let o = bitcast<vec4<u32>>(offset);
    let t = bitcast<vec4<u32>>(tag);
    ddgi_data[b] = o.x;
    ddgi_data[b + 1u] = o.y;
    ddgi_data[b + 2u] = o.z;
    ddgi_data[b + 3u] = o.w;
    ddgi_data[b + 4u] = t.x;
    ddgi_data[b + 5u] = t.y;
    ddgi_data[b + 6u] = t.z;
    ddgi_data[b + 7u] = t.w;
    ddgi_data[b + 8u] = stamp;
}

const RAYS: u32 = 64u;
const PI: f32 = 3.14159265;
const MISS: f32 = 1.0e9;

struct Hit {
    // t < 0: miss.
    t: f32,
    n: vec3<f32>,
    albedo: vec3<f32>,
    emissive: vec3<f32>,
    back: bool,
    // The surface's perceptual roughness; 0: no material model, the hit reflects its albedo evenly (terrain).
    rough: f32,
    // The surface's metallic factor: that share reflects the albedo evenly (`hit_reflectance`).
    metal: f32,
}

fn miss_hit() -> Hit {
    return Hit(-1.0, vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0), vec3<f32>(0.0), false, 0.0, 0.0);
}

// ---- What a hit sends back (ADR-5506) --------------------------------------------------------
//
// A probe ray reads the radiance its hit sends back along the ray. Until ADR-5506 every hit was a Lambertian
// surface of the mesh's mean albedo. The forward pass and the path tracer shade a dielectric as Lambert under
// 1 - F(l.h) plus a GGX lobe (`pbr_brdf.wgsl` `isotropic_lobes`: Walter et al. 2007's D, Heitz 2014's
// height-correlated Smith term, Schlick's Fresnel at F0 0.04). Head-on that reflects about 1 % less than the
// albedo (a rough wall of albedo 0.5: 0.494), at a grazing angle more (0.527 at 6 degrees off the surface), and
// light that travels along a tunnel or down a shaft leaves every wall at a grazing angle: against a tracer with
// Lambertian materials the material tracer holds 5.6 % more light in a tunnel, 2.9 % more in a shaft and 2.4 %
// less in a room lit by a sun patch on its floor. A hit now reflects as that dielectric: the sun and the fill
// light through the lobes themselves, the probes' irradiance through the lobes' directional albedo (their
// integral under even light: the irradiance carries no directions). The metallic share of a hit keeps the plain
// albedo: a metal's reflectance is its base color, and whether its lobe is energy-compensated is not in the
// instance record.

// Share of a dielectric's diffuse albedo that even light leaves after the 1 - F(l.h) factor, minus the
// 1 - F0 every angle keeps: (1 / pi) * integral of (1 - l.h)^5 cos = `DDGI_DIFFUSE_FRESNEL_LOSS` * (1 - mu)^2.5
// within 0.0005 (mu = n.v; numerical integration of the forward pass's diffuse lobe).
const DDGI_DIFFUSE_FRESNEL_LOSS: f32 = 0.02717;
// F0 of a dielectric (`pbr` forward pass: index of refraction 1.5).
const DDGI_DIELECTRIC_F0: f32 = 0.04;
// Least perceptual roughness the lobes take: a smoother surface's bounce is a mirror image no probe holds.
const DDGI_HIT_ROUGHNESS_MIN: f32 = 0.45;

// Directional albedo of the dielectric GGX lobe (F0 0.04) toward a view at mu = n.v under even light: the
// split-sum F0 * A + B (Karis 2013) of the forward pass's lobe, as a cubic in 1 - mu whose coefficients are
// cubics in the perceptual roughness. Fitted by least squares to a numerical integration of that lobe over
// perceptual roughness 0.45 to 1 and mu 0.05 to 1: within 0.0032 everywhere (the albedo runs from 0.012 to
// 0.23). `regia_render` `ddgi::tests::the_trace_shaders_specular_albedo_fit_follows_the_forward_lobe` integrates
// the lobe again and reads these rows.
fn hit_specular_albedo(mu: f32, perceptual: f32) -> f32 {
    let r = clamp(perceptual, DDGI_HIT_ROUGHNESS_MIN, 1.0);
    let x = 1.0 - clamp(mu, 0.05, 1.0);
    let rr = vec4<f32>(1.0, r, r * r, r * r * r);
    let c0 = dot(rr, vec4<f32>(0.02688, 0.10347, -0.21420, 0.09489));
    let c1 = dot(rr, vec4<f32>(-0.27892, 0.99944, -1.10726, 0.42404));
    let c2 = dot(rr, vec4<f32>(0.73528, -2.85220, 3.31179, -1.29530));
    let c3 = dot(rr, vec4<f32>(0.58083, -1.14263, 0.95223, -0.26763));
    return max(((c3 * x + c2) * x + c1) * x + c0, 0.0);
}

// Reflectance of a hit toward `v` for the probes' irradiance (irradiance / pi in, radiance out).
fn hit_reflectance(hit: Hit, v: vec3<f32>) -> vec3<f32> {
    if (hit.rough <= 0.0) {
        return hit.albedo;
    }
    let mu = clamp(dot(hit.n, v), 0.05, 1.0);
    let diffuse = 1.0 - DDGI_DIELECTRIC_F0
        - (1.0 - DDGI_DIELECTRIC_F0) * DDGI_DIFFUSE_FRESNEL_LOSS * pow(1.0 - mu, 2.5);
    let dielectric = hit.albedo * diffuse + vec3<f32>(hit_specular_albedo(mu, hit.rough));
    return mix(dielectric, hit.albedo, hit.metal);
}

// The hit's BRDF times pi toward `v` for light from `l` (cosine excluded; n.l > 0): `albedo` for a Lambertian
// hit, so a caller multiplies by the light's irradiance over pi as before.
fn hit_brdf_pi(hit: Hit, v: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    if (hit.rough <= 0.0) {
        return hit.albedo;
    }
    let r = clamp(hit.rough, DDGI_HIT_ROUGHNESS_MIN, 1.0);
    let a = r * r;
    let a2 = a * a;
    let no_v = clamp(dot(hit.n, v), 0.05, 1.0);
    let no_l = clamp(dot(hit.n, l), 1e-4, 1.0);
    let h = normalize(l + v);
    let no_h = clamp(dot(hit.n, h), 0.0, 1.0);
    let lo_h = clamp(dot(l, h), 0.0, 1.0);
    let fresnel = DDGI_DIELECTRIC_F0 + (1.0 - DDGI_DIELECTRIC_F0) * pow(1.0 - lo_h, 5.0);
    let f = (no_h * a2 - no_h) * no_h + 1.0;
    let d = a2 / (PI * f * f);
    let vis = 0.5 / (no_l * sqrt(no_v * no_v * (1.0 - a2) + a2) + no_v * sqrt(no_l * no_l * (1.0 - a2) + a2));
    let dielectric = hit.albedo * (1.0 - fresnel) + vec3<f32>(PI * d * vis * fresnel);
    return mix(dielectric, hit.albedo, hit.metal);
}

// ---- Heightfield ----------------------------------------------------------------

fn hf_cells() -> i32 {
    return i32(hf.size.x) - 1;
}

fn hf_sample_world(i: vec2<i32>) -> f32 {
    let c = clamp(i, vec2<i32>(0), vec2<i32>(i32(hf.size.x) - 1));
    return textureLoad(hf_heights, c, 0).r * hf.map.z + hf.map.w;
}

fn hf_world_x(i: f32) -> f32 {
    return i / f32(hf_cells()) * hf.map.y - hf.map.x;
}

// Ray against one triangle; t in [t0, t1] or -1.
fn ray_triangle(o: vec3<f32>, d: vec3<f32>, a: vec3<f32>, b: vec3<f32>, c: vec3<f32>, t0: f32, t1: f32) -> f32 {
    let e1 = b - a;
    let e2 = c - a;
    let p = cross(d, e2);
    let det = dot(e1, p);
    if (abs(det) < 1e-9) {
        return -1.0;
    }
    let inv = 1.0 / det;
    let s = o - a;
    let u = dot(s, p) * inv;
    if (u < -1e-5 || u > 1.00001) {
        return -1.0;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if (v < -1e-5 || u + v > 1.00001) {
        return -1.0;
    }
    let t = dot(e2, q) * inv;
    if (t < t0 || t > t1) {
        return -1.0;
    }
    return t;
}

// Level-0 cell: two triangles split along (0,0)-(1,1). Returns (t, normal) or t < 0.
fn hf_cell_hit(cell: vec2<i32>, o: vec3<f32>, d: vec3<f32>, t0: f32, t1: f32) -> vec4<f32> {
    let x0 = hf_world_x(f32(cell.x));
    let x1 = hf_world_x(f32(cell.x + 1));
    let z0 = hf_world_x(f32(cell.y));
    let z1 = hf_world_x(f32(cell.y + 1));
    let p00 = vec3<f32>(x0, hf_sample_world(cell), z0);
    let p10 = vec3<f32>(x1, hf_sample_world(cell + vec2<i32>(1, 0)), z0);
    let p01 = vec3<f32>(x0, hf_sample_world(cell + vec2<i32>(0, 1)), z1);
    let p11 = vec3<f32>(x1, hf_sample_world(cell + vec2<i32>(1, 1)), z1);
    var best = -1.0;
    var n = vec3<f32>(0.0, 1.0, 0.0);
    let ta = ray_triangle(o, d, p00, p10, p11, t0, t1);
    if (ta >= 0.0) {
        best = ta;
        n = normalize(cross(p11 - p00, p10 - p00));
    }
    let tb = ray_triangle(o, d, p00, p11, p01, t0, t1);
    if (tb >= 0.0 && (best < 0.0 || tb < best)) {
        best = tb;
        n = normalize(cross(p01 - p00, p11 - p00));
    }
    if (n.y < 0.0) {
        n = -n;
    }
    return vec4<f32>(n, best);
}

// Terrain height under world (x, z): the cell's triangle, as drawn at the finest level.
fn hf_height_at(x: f32, z: f32) -> f32 {
    let k = f32(hf_cells()) / hf.map.y;
    let g = vec2<f32>((x + hf.map.x) * k, (z + hf.map.x) * k);
    let cell = clamp(vec2<i32>(floor(g)), vec2<i32>(0), vec2<i32>(hf_cells() - 1));
    let f = clamp(g - vec2<f32>(cell), vec2<f32>(0.0), vec2<f32>(1.0));
    let h00 = hf_sample_world(cell);
    let h10 = hf_sample_world(cell + vec2<i32>(1, 0));
    let h01 = hf_sample_world(cell + vec2<i32>(0, 1));
    let h11 = hf_sample_world(cell + vec2<i32>(1, 1));
    if (f.x >= f.y) {
        return h00 + (h10 - h00) * f.x + (h11 - h10) * f.y;
    }
    return h00 + (h11 - h01) * f.x + (h01 - h00) * f.y;
}

// The ground under world (x, z): `hf_height_at`, or far below the map inside a hole.
fn hf_ground_at(x: f32, z: f32) -> f32 {
    if (hf_holed() && hf_in_hole(vec2<f32>(x, z))) {
        return -1.0e30;
    }
    return hf_height_at(x, z);
}

// True where world point `p` is inside rock: under the ground and outside every hole. The margin
// is a few f32 steps of the height, no more: a ray grazing down a hole from a probe near the
// ground sinks slowly, and a 1 cm margin let it run metres past the shaft's wall.
fn hf_in_rock(p: vec3<f32>) -> bool {
    let ground = hf_height_at(p.x, p.z);
    return p.y < ground - (0.001 + abs(ground) * 1e-6) && !hf_in_hole(p.xz);
}

// The rock wall of a hole's shaft facing a ray along `d`: horizontal, toward the ray.
fn hf_wall_normal(d: vec3<f32>) -> vec3<f32> {
    let h = vec2<f32>(d.x, d.z);
    if (dot(h, h) < 1e-12) {
        return vec3<f32>(0.0, 1.0, 0.0);
    }
    let n = -normalize(h);
    return vec3<f32>(n.x, 0.0, n.y);
}

// Surface normal from the mean height pyramid at `level` (grid-space point `pg`).
fn hf_coarse_normal(pg: vec3<f32>, level: u32) -> vec3<f32> {
    let lv = min(level, textureNumLevels(hf_heights) - 1u);
    let dims = vec2<i32>(textureDimensions(hf_heights, i32(lv)));
    let scale = f32(1u << lv);
    let c = clamp(vec2<i32>(pg.xz / scale), vec2<i32>(1), dims - vec2<i32>(2));
    let hx = textureLoad(hf_heights, c + vec2<i32>(1, 0), i32(lv)).r - textureLoad(hf_heights, c - vec2<i32>(1, 0), i32(lv)).r;
    let hz = textureLoad(hf_heights, c + vec2<i32>(0, 1), i32(lv)).r - textureLoad(hf_heights, c - vec2<i32>(0, 1), i32(lv)).r;
    let cell_m = hf.map.y / f32(hf_cells()) * scale;
    return normalize(vec3<f32>(-hx * hf.map.z / (2.0 * cell_m), 1.0, -hz * hf.map.z / (2.0 * cell_m)));
}

// World height of the smooth (mean) terrain over the max-pyramid cell `cell` at `level`: the
// height texture's mip there when it has one, else its texel at the cell centre.
fn hf_mean_world(cell: vec2<i32>, level: u32) -> f32 {
    let lv = min(level, textureNumLevels(hf_heights) - 1u);
    let dims = vec2<i32>(textureDimensions(hf_heights, i32(lv)));
    let centre = (vec2<f32>(cell) + vec2<f32>(0.5)) * f32(1u << level) / f32(1u << lv);
    let c = clamp(vec2<i32>(centre), vec2<i32>(0), dims - vec2<i32>(1));
    return textureLoad(hf_heights, c, i32(lv)).r * hf.map.z + hf.map.w;
}

// Maximum-mipmap traversal. xyz = normal, w = t (< 0: miss). A ray starting under the
// surface returns t = 0 with a downward normal (a back face).
// `footprint`: the cone (cells per metre of ray), negative for the mean-height coarse surface.
fn hf_trace(o_local: vec3<f32>, d: vec3<f32>, tmax: f32, footprint: f32) -> vec4<f32> {
    if ((ddgi.ctrl.w & 2u) == 0u || hf.size.z == 0u) {
        return vec4<f32>(0.0, 1.0, 0.0, -1.0);
    }
    let o = o_local + hf.shadow.yzw;
    let cells = f32(hf_cells());
    let k = cells / hf.map.y;
    let og = vec3<f32>((o.x + hf.map.x) * k, o.y, (o.z + hf.map.x) * k);
    let dg = vec3<f32>(d.x * k, d.y, d.z * k);
    var t0 = 0.0;
    var t1 = tmax;
    // Clip to the map in x and z.
    if (abs(dg.x) < 1e-9) {
        if (og.x < 0.0 || og.x > cells) {
            return vec4<f32>(0.0, 1.0, 0.0, -1.0);
        }
    } else {
        let a = (0.0 - og.x) / dg.x;
        let b = (cells - og.x) / dg.x;
        t0 = max(t0, min(a, b));
        t1 = min(t1, max(a, b));
    }
    if (abs(dg.z) < 1e-9) {
        if (og.z < 0.0 || og.z > cells) {
            return vec4<f32>(0.0, 1.0, 0.0, -1.0);
        }
    } else {
        let a = (0.0 - og.z) / dg.z;
        let b = (cells - og.z) / dg.z;
        t0 = max(t0, min(a, b));
        t1 = min(t1, max(a, b));
    }
    if (t0 > t1) {
        return vec4<f32>(0.0, 1.0, 0.0, -1.0);
    }
    let holed = hf_holed();
    if (t0 <= 0.0 && o.y < hf_ground_at(o.x, o.z) - 0.01) {
        return vec4<f32>(0.0, -1.0, 0.0, 0.0);
    }
    let top = hf.size.y - 1u;
    let cone = max(abs(footprint), 1e-3);
    let mean_surface = footprint < 0.0;
    var level = top;
    var t = t0;
    let bias = select(vec2<f32>(-1e-3), vec2<f32>(1e-3), dg.xz > vec2<f32>(0.0));
    for (var it = 0u; it < 128u; it = it + 1u) {
        if (t > t1) {
            break;
        }
        let p = og + dg * t;
        // Cone footprint: at distance t a ray only needs cells about `cone` · t wide, so
        // the pyramid level whose max box counts as the surface grows with distance (the
        // exact triangles stay within the first ~1 / `cone` cells).
        let leaf = min(u32(max(floor(log2(max(t * cone * k, 1.0))), 0.0)), top);
        let size = f32(1u << level);
        let ldims = vec2<i32>(textureDimensions(hf_max, i32(level)));
        let cell = clamp(vec2<i32>(floor((p.xz + bias) / size)), vec2<i32>(0), ldims - vec2<i32>(1));
        let hmax = textureLoad(hf_max, cell, i32(level)).r;
        let lo = vec2<f32>(cell) * size;
        let hi = lo + vec2<f32>(size);
        var texit = t1;
        if (abs(dg.x) > 1e-9) {
            texit = min(texit, (select(lo.x, hi.x, dg.x > 0.0) - og.x) / dg.x);
        }
        if (abs(dg.z) > 1e-9) {
            texit = min(texit, (select(lo.y, hi.y, dg.z > 0.0) - og.z) / dg.z);
        }
        texit = max(texit, t);
        let y0 = og.y + dg.y * t;
        let y1 = og.y + dg.y * texit;
        if (min(y0, y1) > hmax) {
            t = texit + max(1e-4, texit * 1e-6);
            level = min(level + 1u, top);
            continue;
        }
        if (level > leaf) {
            level = level - 1u;
            if (dg.y < 0.0 && y0 > hmax) {
                t = max(t, (hmax - og.y) / dg.y);
            }
            continue;
        }
        if (level > 0u) {
            // At this footprint the surface is the cell's max box (conservative), or its mean
            // height (`DdgiSettings::hf_mean_surface`).
            let hmean = select(hmax, hf_mean_world(cell, level), mean_surface);
            if (min(y0, y1) > hmean) {
                t = texit + max(1e-4, texit * 1e-6);
                continue;
            }
            var th = t;
            if (dg.y < 0.0 && y0 > hmean) {
                th = (hmean - og.y) / dg.y;
            }
            if (holed && hf_in_hole((o + d * max(th, 0.0)).xz)) {
                // The coarse surface's hit lies in a hole, but the cell may hold solid ground
                // beside the mouth: look at its finer levels (down to the exact triangles, which
                // test the hole per point) instead of skipping the whole cell.
                level = level - 1u;
                continue;
            }
            return vec4<f32>(hf_coarse_normal(og + dg * th, level), max(th, 0.0));
        }
        if (holed && t > 1e-3 && hf_in_rock(o + d * t)) {
            // Below the surface outside a hole the ray is in rock: it came down a hole and
            // met the shaft's wall (at the cell's entry point).
            return vec4<f32>(hf_wall_normal(d), t);
        }
        let h = hf_cell_hit(cell, o, d, max(t - 1e-3, 0.0), texit + 1e-3);
        if (h.w >= 0.0) {
            if (!holed) {
                return h;
            }
            let ph = o + d * h.w;
            if (!hf_in_hole(ph.xz)) {
                // From below (a ray that came down a hole; the normal faces up): the shaft's
                // wall, not a back face.
                if (dot(h.xyz, d) > 0.0) {
                    return vec4<f32>(hf_wall_normal(d), h.w);
                }
                return h;
            }
        }
        if (holed && hf_in_rock(o + d * texit)) {
            // No solid ground in the cell, yet the ray leaves it inside rock: it went down a
            // hole whose edge (the shaft's wall) stands inside this cell. Bisect for that edge
            // instead of stopping at the next cell's entry, a cell past the wall.
            var a = max(t, 0.0);
            var b = texit;
            for (var i = 0u; i < 8u; i = i + 1u) {
                let m = 0.5 * (a + b);
                if (hf_in_rock(o + d * m)) {
                    b = m;
                } else {
                    a = m;
                }
            }
            return vec4<f32>(hf_wall_normal(d), b);
        }
        t = texit + max(1e-4, texit * 1e-6);
    }
    return vec4<f32>(0.0, 1.0, 0.0, -1.0);
}

// ---- Signed-distance bricks -------------------------------------------------------

fn brick_value(b: vec4<u32>, x: u32, y: u32, z: u32) -> f32 {
    return sdf_values[b.x + x + b.y * (y + b.z * z)];
}

fn brick_sample(inst: SdfInstance, pl: vec3<f32>) -> f32 {
    let dims = inst.brick.yzw;
    let maxi = vec3<f32>(dims) - vec3<f32>(1.0);
    let g = clamp((pl - inst.brick_origin.xyz) / inst.brick_voxel.xyz, vec3<f32>(0.0), maxi);
    let fl = min(floor(g), max(maxi - vec3<f32>(1.0), vec3<f32>(0.0)));
    let f = g - fl;
    let i = vec3<u32>(fl);
    let b = inst.brick;
    let c00 = mix(brick_value(b, i.x, i.y, i.z), brick_value(b, i.x + 1u, i.y, i.z), f.x);
    let c10 = mix(brick_value(b, i.x, i.y + 1u, i.z), brick_value(b, i.x + 1u, i.y + 1u, i.z), f.x);
    let c01 = mix(brick_value(b, i.x, i.y, i.z + 1u), brick_value(b, i.x + 1u, i.y, i.z + 1u), f.x);
    let c11 = mix(brick_value(b, i.x, i.y + 1u, i.z + 1u), brick_value(b, i.x + 1u, i.y + 1u, i.z + 1u), f.x);
    return mix(mix(c00, c10, f.y), mix(c01, c11, f.y), f.z);
}

fn to_local(inst: SdfInstance, p: vec3<f32>) -> vec3<f32> {
    let h = vec4<f32>(p, 1.0);
    return vec3<f32>(dot(inst.w2l0, h), dot(inst.w2l1, h), dot(inst.w2l2, h));
}

fn to_local_dir(inst: SdfInstance, d: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(inst.w2l0.xyz, d), dot(inst.w2l1.xyz, d), dot(inst.w2l2.xyz, d));
}

// Slab test against the instance's world bounds: (enter, exit).
fn aabb_span(inst: SdfInstance, o: vec3<f32>, d: vec3<f32>) -> vec2<f32> {
    let inv = vec3<f32>(1.0) / select(vec3<f32>(1e-12), d, abs(d) > vec3<f32>(1e-12));
    let a = (inst.aabb_min.xyz - o) * inv;
    let b = (inst.aabb_max.xyz - o) * inv;
    let lo = min(a, b);
    let hi = max(a, b);
    return vec2<f32>(max(max(lo.x, lo.y), lo.z), min(min(hi.x, hi.y), hi.z));
}

// Closest (or any, `any_hit`) brick hit along the ray between `t0` and `tmax` in cascade `c`'s instance list.
//
// A probe ray's hit is where the ray crosses the brick's surface (ADR-5506): the first sample inside it, moved
// back to the crossing by the two distances on either side (a secant step). Until then the march stopped at
// the first sample within half a voxel of a surface, which also stopped the rays that pass a surface's edge
// within that band: every opening read half a voxel narrower on each side (a 3 m tunnel mouth of 0.1 m voxels
// 2.9 m, a 0.4 m slit 0.3 m), and less light entered every room. A shadow ray (`any_hit`) keeps the band: its
// own-surface rule below is sized by it.
fn sdf_trace(o: vec3<f32>, d: vec3<f32>, t0: f32, tmax: f32, c: u32, any_hit: bool) -> Hit {
    var best = tmax;
    var found = -1;
    let first = ddgi_pick(ddgi.inst_offset, c);
    let count = ddgi_pick(ddgi.inst_count, c);
    for (var k = 0u; k < count; k = k + 1u) {
        let index = sdf_lists[first + k];
        let inst = sdf_instances[index];
        let span = aabb_span(inst, o, d);
        let ta = max(span.x, t0);
        let tb = min(span.y, best);
        if (ta > tb) {
            continue;
        }
        let ol = to_local(inst, o);
        let dl = to_local_dir(inst, d);
        let dl_len = max(length(dl), 1e-6);
        let eps = inst.aabb_min.w;
        let min_step = 0.25 * min(min(inst.brick_voxel.x, inst.brick_voxel.y), inst.brick_voxel.z) / dl_len;
        var t = ta;
        // The last sample outside the surface (`d_out` < 0: none yet).
        var t_out = ta;
        var d_out = -1.0;
        for (var s = 0u; s < 64u; s = s + 1u) {
            if (t > tb) {
                break;
            }
            let dist = brick_sample(inst, ol + dl * t);
            // A shadow ray (`any_hit`) leaves a ray hit that lies inside its own surface's
            // band (the march stops anywhere within `eps`, and a thin plate's `eps` is an
            // eighth of its thickness, more than the start's `0.02 · spacing` lift): a sample
            // within two bands of the start is that surface, not an occluder. Without this the
            // probes read every sunlit floor thicker than a few centimetres as shadowed
            // (ADR-4948 finding C, ADR-5001; the robust spawn offset of Pharr et al., PBRT 4th
            // ed. § 6.8.6). A wall is at least eight bands thick, so the skip cannot cross one.
            if (any_hit) {
                if (dist < eps && t * dl_len >= 2.0 * eps) {
                    best = t;
                    found = i32(index);
                    break;
                }
            } else if (dist <= 0.0) {
                // Crossed between the last sample outside and this one; a ray that starts inside the surface
                // takes its start (a back face at no distance: `relocate`'s start band).
                var crossing = t;
                if (d_out > 0.0) {
                    crossing = t_out + (t - t_out) * d_out / (d_out - dist);
                }
                best = crossing;
                found = i32(index);
                break;
            }
            t_out = t;
            d_out = dist;
            t = t + max(dist / dl_len, min_step);
        }
        if (found >= 0 && any_hit) {
            break;
        }
    }
    if (found < 0) {
        return miss_hit();
    }
    let inst = sdf_instances[u32(found)];
    let pl = to_local(inst, o + d * best);
    let v = inst.brick_voxel.xyz;
    let gx = brick_sample(inst, pl + vec3<f32>(v.x, 0.0, 0.0)) - brick_sample(inst, pl - vec3<f32>(v.x, 0.0, 0.0));
    let gy = brick_sample(inst, pl + vec3<f32>(0.0, v.y, 0.0)) - brick_sample(inst, pl - vec3<f32>(0.0, v.y, 0.0));
    let gz = brick_sample(inst, pl + vec3<f32>(0.0, 0.0, v.z)) - brick_sample(inst, pl - vec3<f32>(0.0, 0.0, v.z));
    let nl = vec3<f32>(gx / v.x, gy / v.y, gz / v.z);
    // Object → world normal: the transpose of world → object.
    var n = inst.w2l0.xyz * nl.x + inst.w2l1.xyz * nl.y + inst.w2l2.xyz * nl.z;
    n = n / max(length(n), 1e-6);
    return Hit(best, n, inst.albedo.rgb, inst.emissive.rgb, dot(n, d) > 0.0, inst.albedo.w, inst.emissive.w);
}

// ---- Scene ---------------------------------------------------------------------------

fn mesh_reach(c: u32) -> f32 {
    let s = ddgi_cascade(c).origin.w;
    return s * f32(max(max(ddgi.dims.x, ddgi.dims.y), ddgi.dims.z));
}

// The cascade whose instance list held `sdf_trace_far`'s last hit (the ray's own cascade on a miss).
var<private> far_list: u32;

// The mesh hit of a ray that starts inside cascade `c`: that cascade's instance list out to its reach, then each
// coarser cascade's list over the stretch past the one before (a cascade's list holds every instance within its
// reach of its volume, and the finer volumes lie inside it). A ray that stopped at its own cascade's reach read
// the sky from there on: the sunlit ground past 20 m of a 1 m cascade was sky to its probes, and a tunnel's
// walls, lit by that ground through the mouth, read 0.73 times the tracer (ADR-5374).
fn sdf_trace_far(o: vec3<f32>, d: vec3<f32>, c: u32, any_hit: bool) -> Hit {
    var t0 = 0.0;
    for (var k = c; k < ddgi.dims.w; k = k + 1u) {
        let reach = mesh_reach(k);
        let hit = sdf_trace(o, d, t0, reach, k, any_hit);
        if (hit.t >= 0.0) {
            far_list = k;
            return hit;
        }
        t0 = reach;
    }
    far_list = c;
    return miss_hit();
}

fn trace_scene(o: vec3<f32>, d: vec3<f32>, c: u32) -> Hit {
    // `let`: a `var` of `Hit` carries an explicit layout Vulkan forbids in Function storage
    // (ADR-026; the shader matrix's `ddgi_trace.wgsl` layout scan, ADR-2644).
    let hit = sdf_trace_far(o, d, c, false);
    let tmax = select(ddgi.sky.z, hit.t, hit.t >= 0.0);
    let h = hf_trace(o, d, tmax, hf.albedo.w);
    if (h.w >= 0.0) {
        let back = h.w <= 0.0 && h.y < 0.0;
        // The terrain reflects its albedo evenly (rough 0: no material model).
        return Hit(h.w, h.xyz, hf_albedo_at(o + d * h.w), vec3<f32>(0.0), back || dot(h.xyz, d) > 0.0, 0.0, 0.0);
    }
    return hit;
}

fn sun_visible(p: vec3<f32>, c: u32) -> f32 {
    let l = ddgi.to_sun.xyz;
    if (sdf_trace_far(p, l, c, true).t >= 0.0) {
        return 0.0;
    }
    if (hf_trace(p, l, ddgi.sky.z, hf.shadow.x).w >= 0.0) {
        return 0.0;
    }
    return 1.0;
}

fn equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / PI) + 0.5, theta / PI);
}

fn sky_radiance(d: vec3<f32>) -> vec3<f32> {
    if (ddgi.sky.x > 0.0) {
        return textureSampleLevel(sky_tex, sky_samp, equirect_uv(d), ddgi.sky.y).rgb * ddgi.sky.x;
    }
    return ddgi.flat_sky.rgb;
}

// `ctrl.w` bit 25: the image light's ground plane is on (`DdgiLight::ground`, ADR-5716).
const DDGI_CTRL_GROUND_PLANE: u32 = 33554432u;

// What a ray from `o` toward `d` that met no surface reads: the sky, or the image light's ground plane where the
// ray lands on it (ADR-2000's plane: a horizontal plane or disc of one radiance that stands for the ground under
// a studio image; `env_ground.rs`). The forward pass swaps the image's lower half for that plane in its own sky
// term by a form factor. Until ADR-5716 it did the same to the probes' irradiance, on the premise that every
// probe ray toward the ground read the image's lower half: a scene with a ground mesh under the plane, which the
// probes trace, then lost the hidden half's light twice and gained the plane's twice (`studio_measure`: the far
// low side of a sphere at 0.72 of its reference). The plane is now in the probes' own world, behind every mesh.
// Its numbers ride components of the uniform that held nothing (`DdgiUniform`).
//
// A ray that starts on the plane or under it lands straight under its origin. A probe is not a shaded point: it
// stands for the points around it, and the layer of probes at the plane's own height (or the one just under it)
// carries most of the weight of every point in the first cell over the plane. With the image's lower half in its
// downward rays the plane never reached the underside of anything that stood on it (a sphere's underside at 2.50
// of its reference with the plane and without). The plane hides the image's lower half from the forward pass's
// points over it; the probes that light those points see it hidden too.
fn miss_radiance(o: vec3<f32>, d: vec3<f32>) -> vec3<f32> {
    if ((ddgi.ctrl.w & DDGI_CTRL_GROUND_PLANE) != 0u && d.y < -1e-4) {
        let h = max(o.y - ddgi.rot_x.w, 0.0);
        let landing = o.xz + d.xz * (h / -d.y) - vec2<f32>(ddgi.rot_z.w, ddgi.sun.w);
        let radius = ddgi.rot_y.w;
        if (radius <= 0.0 || dot(landing, landing) <= radius * radius) {
            return vec3<f32>(ddgi.fill.w, bitcast<f32>(ddgi.emitters.z), bitcast<f32>(ddgi.emitters.w));
        }
    }
    return sky_radiance(d);
}

var<workgroup> wg_origin: vec4<f32>;


// Updates a new probe takes every frame before it joins its cascade's cadence (`classify`). The same count its
// distance moments take to fill (`ddgi_blend.wgsl` `DDGI_SURFACE_MIN_UPDATES`): until then its surface texels
// hold the probe's own lobe, and its light is the plain mean of that many frames of rays. It was 8 until
// ADR-5506: with the probes weighted at the surface point, a floor on a probe plane reads four probes where the
// weights at the biased point read eight, so a walk showed the newest probes' error a quarter more (an invariant
// corridor strayed 2.43 luma levels from its held view against 1.91; 1.98 with 16).
const DDGI_YOUNG_UPDATES: i32 = 16;

// Probes due this frame (`classify`), one per trace and blend workgroup.
@group(0) @binding(3) var<storage, read_write> ddgi_queue: array<u32>;
// Indirect dispatch arguments for the due probes (x = count, y = z = 1).
@group(1) @binding(5) var<storage, read_write> ddgi_args: array<atomic<u32>, 4>;

// One thread per probe slot: reset a slot whose stored cell scrolled out, decide whether it
// re-traces this frame (young probes every frame, then a cascade-staggered cadence), stamp it
// and queue it. The trace and blend passes then run one workgroup per queued probe only:
// launching a heavy trace workgroup per slot just to find it idle cost ~0.5 ms on its own.
@compute @workgroup_size(64)
fn classify(@builtin(global_invocation_id) gid: vec3<u32>) {
    let probe = gid.x;
    if (probe == 0u) {
        atomicStore(&ddgi_args[1], 1u);
        atomicStore(&ddgi_args[2], 1u);
    }
    if (probe >= ddgi_count() * ddgi.dims.w) {
        return;
    }
    let cell = ddgi_slot_cell(probe);
    let c = probe / ddgi_count();
    var offset = ddgi_probe_offset(probe);
    var tag = ddgi_probe_tag(probe);
    var stamp = ddgi_probe_stamp(probe);
    if (any(tag.xyz != cell)) {
        offset = vec4<f32>(0.0, 0.0, 0.0, 1.0);
        tag = vec4<i32>(cell, 0);
        stamp = 0u;
        // Not shaded until it has traced (`relocate` sets the header).
        ddgi_data[ddgi_header_word(probe)] = ddgi_pack_cell(cell, false);
        // No seed yet (`seed_probe` sets one, ADR-2605).
        ddgi_data[probe * DDGI_PROBE_WORDS + DDGI_SEED_WORD] = 0u;
    }
    // The light jumped (`ctrl.w` bit 12, `light_cut`): every probe starts over as a young one
    // (its cell and offset kept), so the next `DDGI_YOUNG_UPDATES` updates replace the old light.
    if ((ddgi.ctrl.w & 4096u) != 0u) {
        tag.w = 0;
        ddgi_data[probe * DDGI_PROBE_WORDS + DDGI_SEED_WORD] = 0u;
    }
    // Coarser cascades change more slowly: they re-trace half as often per level.
    var divisor = max(ddgi.ctrl.z, 1u) << min(c, 3u);
    // A settled probe classified inactive (inside the terrain or a mesh, `relocate`) shades
    // nothing; it re-traces `DdgiSettings::inactive_slowdown` times less often, only to notice
    // geometry that moved off it (Majercik et al. 2021 § 4.2 skip inactive probes).
    if (tag.w >= DDGI_YOUNG_UPDATES && offset.w < 0.5) {
        divisor = divisor << ((ddgi.ctrl.w >> 8u) & 15u);
    }
    var due = tag.w < DDGI_YOUNG_UPDATES || ((probe + ddgi.ctrl.y) % divisor) == 0u;
    // The young-probe budget (`young_select`, ADR-7870): past it a young probe waits for a later frame. Off
    // (budget 0, every shipped tier) this is the update above, unchanged.
    if (tag.w < DDGI_YOUNG_UPDATES && ddgi_young.params.x != 0u && ddgi_young.state.w == DDGI_YOUNG_THRESHOLD) {
        due = young_key(probe, cell, stamp) <= ddgi_young.state.y;
    }
    if (due) {
        stamp = ddgi.ctrl.y;
        let slot = atomicAdd(&ddgi_args[0], 1u);
        ddgi_queue[slot] = probe;
    }
    probe_store(probe, offset, tag, stamp);
}

// The young-probe trace budget (ADR-7870, `DdgiSettings::young_budget`). A start-up, a light cut or a far camera
// jump makes every probe young at once, and `classify` queues every young probe every frame for its first
// `DDGI_YOUNG_UPDATES` updates. With a budget B > 0 a frame traces at most B young probes: the B with the least
// key, where the key orders by distance from the eye (in finest-cascade spacings), then in view before out of
// view, then the frame the probe last traced (the longest waiting first), then the slot. Keys are unique, so the
// B least are one set whatever order the threads run in; the passes below find the B-th least key exactly by a
// most-significant-digit radix select (four 8-bit digits; Hoare's selection by counting): each histogram pass
// counts the young probes whose key holds the digits chosen so far, by their next digit, and `young_select` takes
// the digit where the count reaches the rank still wanted. Atomic additions only count, so the chosen key does not
// depend on the order they ran in. Settled probes keep their cadence and are not counted.
struct DdgiYoung {
    // The frame's view-projection matrix (when `eye.w` is 1).
    view_proj: mat4x4<f32>,
    // xyz = the eye (m), w = 1 when `view_proj` holds this frame's view (else every probe counts as in view).
    eye: vec4<f32>,
    // x = the budget (young probes a frame, 0 = off), y = f32 bits of the finest spacing (m).
    params: vec4<u32>,
    // Written by the passes: x = histogram passes done, y = the key digits chosen so far, z = the rank still
    // wanted among the probes that hold them, w = `DDGI_YOUNG_SELECTING`, `DDGI_YOUNG_ALL` or
    // `DDGI_YOUNG_THRESHOLD` (y is then the B-th least key). Zeroed each frame before the first pass.
    state: vec4<u32>,
    hist: array<atomic<u32>, 256>,
}
@group(1) @binding(7) var<storage, read_write> ddgi_young: DdgiYoung;

const DDGI_YOUNG_SELECTING: u32 = 0u;
// Every young probe fits the budget this frame: all of them trace.
const DDGI_YOUNG_ALL: u32 = 1u;
// `state.y` is the B-th least key: a young probe traces when its key is not greater.
const DDGI_YOUNG_THRESHOLD: u32 = 2u;

// The updates and the last-trace frame `classify` gives slot `probe` this frame (after a scroll's and a light cut's
// resets), without storing them: x = updates, y = stamp.
fn young_state(probe: u32, cell: vec3<i32>) -> vec2<u32> {
    let tag = ddgi_probe_tag(probe);
    var age = tag.w;
    var stamp = ddgi_probe_stamp(probe);
    if (any(tag.xyz != cell)) {
        age = 0;
        stamp = 0u;
    }
    if ((ddgi.ctrl.w & 4096u) != 0u) {
        age = 0;
    }
    return vec2<u32>(u32(max(age, 0)), stamp);
}

// The budget's order key of the young probe at slot `probe` (world cell `cell`, last traced at frame `stamp`): the
// distance ring (8 bits), out of view (1 bit), 255 minus the frames since its last trace (8 bits), then the slot in
// the low bits. Past 2^15 slots the lowest bits of the priority give way to the slot, so keys stay unique.
fn young_key(probe: u32, cell: vec3<i32>, stamp: u32) -> u32 {
    let c = probe / ddgi_count();
    let s = ddgi_cascade(c).origin.w;
    let pos = vec3<f32>(cell) * s;
    let unit = max(bitcast<f32>(ddgi_young.params.y), 1e-3);
    let ring = u32(min(distance(pos, ddgi_young.eye.xyz) / unit, 255.0));
    var out_of_view = 0u;
    if (ddgi_young.eye.w > 0.5) {
        // In view: inside the frustum widened by about a probe spacing (a probe a cell outside it still lights the
        // pixels at the frame's edge).
        let clip = ddgi_young.view_proj * vec4<f32>(pos, 1.0);
        let pad = 2.0 * s;
        let inside = clip.w > -s && abs(clip.x) <= clip.w + pad && abs(clip.y) <= clip.w + pad;
        out_of_view = select(1u, 0u, inside);
    }
    let wait = min(ddgi.ctrl.y - stamp, 255u);
    let priority = (ring << 9u) | (out_of_view << 8u) | (255u - wait);
    let total = ddgi_count() * ddgi.dims.w;
    let bits = 32u - countLeadingZeros(max(total, 2u) - 1u);
    let low = max(bits, 15u);
    return ((priority >> (low - 15u)) << low) | probe;
}

// One thread per probe slot: counts this frame's young probes by the next 8-bit digit of their key (`DdgiYoung`).
@compute @workgroup_size(64)
fn young_histogram(@builtin(global_invocation_id) gid: vec3<u32>) {
    let probe = gid.x;
    if (probe >= ddgi_count() * ddgi.dims.w || ddgi_young.state.w != DDGI_YOUNG_SELECTING) {
        return;
    }
    let cell = ddgi_slot_cell(probe);
    let young = young_state(probe, cell);
    if (young.x >= u32(DDGI_YOUNG_UPDATES)) {
        return;
    }
    let key = young_key(probe, cell, young.y);
    let done = ddgi_young.state.x;
    if (done > 0u && (key >> (32u - 8u * done)) != ddgi_young.state.y) {
        return;
    }
    atomicAdd(&ddgi_young.hist[(key >> (24u - 8u * done)) & 255u], 1u);
}

// One thread: takes the digit at which the histogram reaches the rank still wanted (the budget on the first
// pass), and clears the histogram for the next pass. After the fourth pass `state.y` is the B-th least key.
@compute @workgroup_size(1)
fn young_select() {
    var st = ddgi_young.state;
    if (st.w != DDGI_YOUNG_SELECTING) {
        return;
    }
    var need = st.z;
    if (st.x == 0u) {
        need = ddgi_young.params.x;
        var young = 0u;
        for (var d = 0u; d < 256u; d = d + 1u) {
            young = young + atomicLoad(&ddgi_young.hist[d]);
        }
        if (young <= need) {
            for (var d = 0u; d < 256u; d = d + 1u) {
                atomicStore(&ddgi_young.hist[d], 0u);
            }
            st.w = DDGI_YOUNG_ALL;
            ddgi_young.state = st;
            return;
        }
    }
    var below = 0u;
    var digit = 255u;
    var found = false;
    for (var d = 0u; d < 256u; d = d + 1u) {
        let h = atomicLoad(&ddgi_young.hist[d]);
        atomicStore(&ddgi_young.hist[d], 0u);
        if (!found) {
            if (below + h >= need) {
                digit = d;
                found = true;
            } else {
                below = below + h;
            }
        }
    }
    st.y = (st.y << 8u) | digit;
    st.z = need - below;
    st.x = st.x + 1u;
    if (st.x == 4u) {
        st.w = DDGI_YOUNG_THRESHOLD;
    }
    ddgi_young.state = st;
}

@compute @workgroup_size(64)
fn trace(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) ray: u32) {
    let probe = ddgi_queue[wid.x];
    if (ray == 0u) {
        let c = probe / ddgi_count();
        let s = ddgi_cascade(c).origin.w;
        wg_origin = vec4<f32>(vec3<f32>(ddgi_slot_cell(probe)) * s + ddgi_probe_offset(probe).xyz, s);
    }
    workgroupBarrier();
    let c = probe / ddgi_count();
    let origin = wg_origin.xyz;
    let dir = ddgi_ray_dir(ray, RAYS);
    let hit = trace_scene(origin, dir, c);
    // The list that held the hit: the hit's sun shadow ray starts in that list (the occluders around a far hit
    // are not in the probe's own list).
    let hit_list = far_list;
    var out = vec4<f32>(0.0);
    if (hit.t < 0.0) {
        out = vec4<f32>(miss_radiance(origin, dir), MISS);
    } else if (hit.back) {
        // A back face: the probe is inside geometry. Negative distance marks it.
        out = vec4<f32>(0.0, 0.0, 0.0, -max(hit.t, 1e-4));
    } else {
        let p = origin + dir * hit.t + hit.n * 0.02 * wg_origin.w;
        // What the hit sends back toward the probe of each direct light: its irradiance times the hit's BRDF
        // (times pi: `e_sun / PI` below), the forward pass's lobes for a mesh (`hit_brdf_pi`, ADR-5506).
        var e_sun = vec3<f32>(0.0);
        if (ddgi.to_sun.w > 0.0) {
            let ndl = max(dot(hit.n, ddgi.to_sun.xyz), 0.0);
            if (ndl > 0.0) {
                e_sun = ddgi.sun.rgb * ndl * sun_visible(p, hit_list) * hit_brdf_pi(hit, -dir, ddgi.to_sun.xyz);
            }
        }
        // The sky body's planet-shine (Jupiter on Europa): the forward pass lights every
        // surface with it, so the ground the probes see carries it too. On an airless map it
        // is most of what a sun-shadowed floor sends back.
        if (ddgi.to_fill.w > 0.0) {
            let ndf = max(dot(hit.n, ddgi.to_fill.xyz), 0.0);
            if (ndf > 0.0) {
                e_sun += ddgi.fill.rgb * ndf * hit_brdf_pi(hit, -dir, ddgi.to_fill.xyz);
            }
        }
        // The frame's extra directional lights (ADR-7254), unshadowed as the forward shades them.
        for (var xi = 0u; xi < 3u; xi = xi + 1u) {
            let to_x = ddgi.extra_dir[2u * xi];
            if (to_x.w > 0.0) {
                e_sun += ddgi.extra_dir[2u * xi + 1u].rgb * max(dot(hit.n, to_x.xyz), 0.0);
            }
        }
        // Light already in the probes (the bounces before this one), from this probe's cascade
        // or finer on a layout with a cascade too coarse for a room (`ctrl.w` bit 14, ADR-1949).
        // Outside every cascade the open sky stands in; inside one whose probes have not traced
        // yet, nothing (a sky stand-in there would light a closed room until the probes forgot it).
        let limit = select(99u, c, (ddgi.ctrl.w & 16384u) != 0u);
        let gi = ddgi_irradiance_traced_plain(p, hit.n, -dir, limit);
        var indirect = gi.rgb * max(gi.a, 0.0);
        if (gi.a < 0.0) {
            indirect = ddgi.flat_sky.rgb;
        }
        out = vec4<f32>(
            ddgi_ray_embed(e_sun / PI + hit_reflectance(hit, -dir) * indirect + hit.emissive, hit.n),
            hit.t,
        );
    }
    ddgi_rays[probe * RAYS + ray] = out;
    // Seeding (ADR-2605, `DdgiSettings::seed_updates`, `ctrl.w` bits 17..20). Uniform: the weight
    // is a uniform, so the barrier inside is reached by the whole workgroup.
    let weight = (ddgi.ctrl.w >> 17u) & 15u;
    if (weight != 0u) {
        seed_probe(probe, ray, weight);
    }
}

// Probe record word: the seed weight of the probe's last start-over (0 = none), and the frame it
// was seeded (ADR-2605). The records keep 12 words; 9..11 held nothing before.
const DDGI_SEED_WORD: u32 = 9u;
const DDGI_SEED_FRAME_WORD: u32 = 10u;

var<workgroup> wg_seed_miss: atomic<u32>;
var<workgroup> wg_seed_kind: u32;

// How much of `p` the traced, active probe at slot `index` (world position `probe_pos`) sees: the
// Chebyshev bound of its distance moments toward `p`, 1 when nothing nearer lies in between.
fn seed_visibility(index: u32, probe_pos: vec3<f32>, p: vec3<f32>, s: f32) -> f32 {
    let from_probe = p - probe_pos;
    let r = max(length(from_probe), 1e-4);
    let m = ddgi_moments(index, from_probe / r);
    if (r <= m.x) {
        return 1.0;
    }
    let variance = max(abs(m.y - m.x * m.x), 1e-4 * s * s);
    let d = r - m.x;
    let cheb = variance / (variance + d * d);
    return cheb * cheb * cheb;
}

// Whether the probe at slot `index` holding world cell `cell` has traced and is active.
fn seed_source_ok(index: u32, cell: vec3<i32>) -> bool {
    let hw = ddgi_header_word(index);
    return ddgi_data[hw] == ddgi_pack_cell(cell, true) && (ddgi_data[hw + 1u] >> 31u) != 0u;
}

// The seed from where the probe stood before its slot scrolled: the neighbor one cell toward the
// volume's inside on every axis where the probe lies on the volume's face (the probe the scroll
// stepped past), texel `t` of its tile. a = 0 when it has no such neighbor, that one has not traced,
// or its distance moments put a surface between the two (a wall: its light is the other side's).
fn seed_from_neighbor(c: u32, cell: vec3<i32>, pos: vec3<f32>, t: vec2<u32>) -> vec4<f32> {
    let d = vec3<i32>(ddgi.dims.xyz);
    let rel = cell - ddgi_cascade(c).base.xyz;
    let inward = select(vec3<i32>(0), vec3<i32>(1), rel == vec3<i32>(0))
        - select(vec3<i32>(0), vec3<i32>(1), rel == d - vec3<i32>(1));
    if (all(inward == vec3<i32>(0))) {
        return vec4<f32>(0.0);
    }
    let ncell = cell + inward;
    let index = ddgi_slot(c, ncell);
    if (!seed_source_ok(index, ncell)) {
        return vec4<f32>(0.0);
    }
    let s = ddgi_cascade(c).origin.w;
    let npos = vec3<f32>(ncell) * s + ddgi_unpack_offset(ddgi_data[ddgi_header_word(index) + 1u], s);
    if (seed_visibility(index, npos, pos, s) < SEED_MIN_VISIBILITY) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(ddgi_packed_irr_load(index, t), 1.0);
}

// The seed from the next coarser cascade: its probes' irradiance / π toward `dir`, trilinear at
// `pos` and weighted by how well each sees `pos`. a = 0 past the coarsest cascade, outside its
// volume, or unless the probes that see `pos` carry at least `SEED_MIN_VISIBILITY` of the trilinear
// weight (a probe that has not traced, is inside geometry or stands behind a wall counts as not
// seeing it). A coarse cascade cannot resolve a room, so only where it sees the point clearly does its
// light stand for the finer probe's; anywhere else its light is the leak ADR-1949 keeps out.
fn seed_from_coarser(c: u32, pos: vec3<f32>, dir: vec3<f32>) -> vec4<f32> {
    let next = c + 1u;
    if (next >= ddgi.dims.w || ddgi_inset(next, pos) < 0.0) {
        return vec4<f32>(0.0);
    }
    let s = ddgi_cascade(next).origin.w;
    let g = pos / s;
    let ibase = vec3<i32>(floor(g));
    let f = g - vec3<f32>(ibase);
    var sum = vec3<f32>(0.0);
    var seen = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let o = vec3<i32>(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        let cell = ibase + o;
        let index = ddgi_slot(next, cell);
        if (!seed_source_ok(index, cell)) {
            continue;
        }
        let tri_axes = mix(vec3<f32>(1.0) - f, f, vec3<f32>(o));
        let tri = tri_axes.x * tri_axes.y * tri_axes.z;
        let probe_pos = vec3<f32>(cell) * s + ddgi_unpack_offset(ddgi_data[ddgi_header_word(index) + 1u], s);
        let w = tri * seed_visibility(index, probe_pos, pos, s);
        if (ddgi_tier_lookup()) {
            // The coarse probes' light at their own positions (`rel` 0: no face reads a surface texel).
            sum = sum + w * ddgi_probe_irradiance(index, dir, vec3<f32>(0.0), s);
        } else {
            sum = sum + w * ddgi_irradiance_texel(index, dir);
        }
        seen = seen + w;
    }
    if (seen < SEED_MIN_VISIBILITY) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(sum / seen, 1.0);
}

// Least visibility a seed's source must have of the seeded probe (ADR-2605).
const SEED_MIN_VISIBILITY: f32 = 0.9;

// Writes a seed into the irradiance history of a probe that starts over without its own light, one
// tile texel per invocation (`local`, 8 × 8 with the octahedral border), so the blend's first updates
// average with it at `weight` updates (`ddgi_blend.wgsl` `hysteresis_irr`) instead of starting from
// one frame of rays or from the light of the cell the slot held before. Two cases:
//   - the slot scrolled to a new cell (`classify` reset it: no updates, header not traced): seeded from
//     its neighbor toward the volume's inside, then the next coarser cascade;
//   - the probe was inside geometry on its last trace (offset.w 0): `relocate` may find it out now,
//     and its own history is the light from before it went in. Seeded from the next coarser cascade,
//     then its neighbor. While it stays inside it shades nothing, so the seed is harmless.
// Every texel must find a seed, or none is written. Nothing reads a probe that has not traced, and a
// probe inside geometry is not active, so no other workgroup of this pass reads the words written.
fn seed_probe(probe: u32, local: u32, weight: u32) {
    if (local == 0u) {
        atomicStore(&wg_seed_miss, 0u);
        let tag = ddgi_probe_tag(probe);
        let head = ddgi_data[ddgi_header_word(probe)];
        var kind = 0u;
        // On a light cut (`ctrl.w` bit 12) every source still holds the old light.
        if ((ddgi.ctrl.w & 4096u) != 0u) {
            kind = 0u;
        } else if (tag.w == 0 && (head >> 31u) == 0u) {
            kind = 1u;
        } else if (tag.w > 0 && ddgi_probe_offset(probe).w < 0.5) {
            kind = 2u;
        }
        wg_seed_kind = kind;
    }
    let kind = workgroupUniformLoad(&wg_seed_kind);
    let c = probe / ddgi_count();
    let s = ddgi_cascade(c).origin.w;
    let cell = ddgi_slot_cell(probe);
    let pos = vec3<f32>(cell) * s + ddgi_probe_offset(probe).xyz;
    let t = vec2<u32>(local % DDGI_IRR_TILE, local / DDGI_IRR_TILE);
    let res = i32(DDGI_IRR_RES);
    let src = ddgi_border_source(vec2<i32>(t), res);
    var dir = ddgi_irr_texel_dir((vec2<f32>(src) + vec2<f32>(0.5)) / f32(res) * 2.0 - vec2<f32>(1.0));
    // A crease texel starts as its face's light (ADR-5552): a crease scales nothing until the probe's own rays
    // have filled it.
    let crease = ddgi_crease_index(t);
    if (crease < 24u) {
        dir = ddgi_face_dir(crease / 4u);
    }
    var seed = vec4<f32>(0.0);
    if (kind == 1u) {
        seed = seed_from_neighbor(c, cell, pos, t);
        if (seed.a <= 0.0) {
            seed = seed_from_coarser(c, pos, dir);
        }
    } else if (kind == 2u) {
        seed = seed_from_coarser(c, pos, dir);
        if (seed.a <= 0.0) {
            seed = seed_from_neighbor(c, cell, pos, t);
        }
    }
    let value = ddgi_finite_or3(seed.rgb, vec3<f32>(0.0));
    if (kind != 0u && !(seed.a > 0.0)) {
        atomicOr(&wg_seed_miss, 1u);
    }
    workgroupBarrier();
    // No barrier follows, so this return need not be uniform (ADR-1583).
    if (kind == 0u || atomicLoad(&wg_seed_miss) != 0u) {
        return;
    }
    let word = ddgi_irr_word(probe, t);
    let bits = bitcast<vec3<u32>>(value);
    ddgi_data[word] = bits.x;
    ddgi_data[word + 1u] = bits.y;
    ddgi_data[word + 2u] = bits.z;
    if (local == 0u) {
        ddgi_data[probe * DDGI_PROBE_WORDS + DDGI_SEED_WORD] = weight;
        ddgi_data[probe * DDGI_PROBE_WORDS + DDGI_SEED_FRAME_WORD] = ddgi.ctrl.y;
    }
}

// A back-face ray within this share of the probe spacing stopped at its start (`relocate`).
const DDGI_START_BAND: f32 = 0.02;

// Relocation and classification (Majercik et al. 2021 § 4): a probe that sees many back
// faces sits inside geometry and moves toward the nearest one (through it); a probe too
// close to a front face moves away. A probe still inside after that is disabled.
@compute @workgroup_size(64)
fn relocate(@builtin(global_invocation_id) gid: vec3<u32>) {
    let probe = gid.x;
    let total = ddgi_count() * ddgi.dims.w;
    if (probe >= total) {
        return;
    }
    if (ddgi_probe_stamp(probe) != ddgi.ctrl.y) {
        return;
    }
    let tag = ddgi_probe_tag(probe);
    let c = probe / ddgi_count();
    let s = ddgi_cascade(c).origin.w;
    var backs = 0u;
    var near_back = MISS;
    var near_back_dir = vec3<f32>(0.0);
    var near_front = MISS;
    var near_front_dir = vec3<f32>(0.0);
    // Back-face rays that stopped at their start: the probe sits on (or in) a surface's band, and
    // every one of them is equally near. Their mean is the way out (the surface's normal on a face,
    // the bisector on an edge); the single nearest of them is whichever this frame's ray rotation
    // listed first, so a probe on a surface moved a different way each time it was relocated and the
    // field depended on the camera's path (ADR-5338: exact brick edges put a 1 m grid's probes on
    // wall tops and feet).
    var start_dir = vec3<f32>(0.0);
    // Front faces nearer than the least distance (`DdgiSettings::min_frontface`), each ray weighed by how far
    // inside that distance it hit: their mean is the way toward the surface (its normal over a face). The single
    // nearest of them is whichever ray this frame's rotation put closest to that normal, up to ten degrees off
    // it, so a probe pushed off a floor also slid a few centimetres along it, a different way on each path that
    // brought it there: a floor 5 mm over a probe plane and one 5 mm under it gave different fields (ADR-5506,
    // `gi_radiance_cache` `the_lifted_trench...`: 0.0009 of the shadow luminance apart, 0.0003 with the mean).
    var front_dir = vec3<f32>(0.0);
    for (var r = 0u; r < RAYS; r = r + 1u) {
        let dist = ddgi_rays[probe * RAYS + r].w;
        let dir = ddgi_ray_dir(r, RAYS);
        if (dist >= 0.0 && dist < ddgi.flat_sky.w * s) {
            front_dir = front_dir + dir * (ddgi.flat_sky.w * s - dist);
        }
        if (dist < 0.0) {
            backs = backs + 1u;
            if (-dist < near_back) {
                near_back = -dist;
                near_back_dir = dir;
            }
            if (-dist <= DDGI_START_BAND * s) {
                start_dir = start_dir + dir;
            }
        } else if (dist < near_front) {
            near_front = dist;
            near_front_dir = dir;
        }
    }
    let previous = ddgi_probe_offset(probe);
    var offset = previous.xyz;
    let back_frac = f32(backs) / f32(RAYS);
    let cell = vec3<f32>(tag.xyz) * s;
    if (back_frac > 0.25) {
        // Under the terrain every ray reports t = 0: lift to the surface instead.
        let pos = cell + offset;
        let ground = hf_ground_at(pos.x + hf.shadow.y, pos.z + hf.shadow.w) - hf.shadow.z;
        if ((ddgi.ctrl.w & 2u) != 0u && hf.size.z != 0u && pos.y < ground) {
            offset.y = offset.y + (ground - pos.y) + 0.1 * s;
        } else if (near_back < MISS) {
            var way_out = near_back_dir;
            let start_len = length(start_dir);
            if (near_back <= DDGI_START_BAND * s && start_len > 0.5) {
                way_out = start_dir / start_len;
            }
            offset = offset + way_out * (near_back + 0.05 * s);
        }
    } else if (near_front < ddgi.flat_sky.w * s) {
        var away = near_front_dir;
        let front_len = length(front_dir);
        if (front_len > 1e-6 * s) {
            away = front_dir / front_len;
        }
        offset = offset - away * (ddgi.flat_sky.w * s - near_front);
    }
    let limit = 0.45 * s;
    let clamped = clamp(offset, vec3<f32>(-limit), vec3<f32>(limit));
    let is_active = back_frac <= 0.25;
    // offset.w: 0 = inside geometry: this frame's rays started inside a mesh or the terrain,
    // so the blend passes leave the probe's history alone (blending them took the light a
    // probe sees from under a slab or below the ground into every later frame). 1 = active.
    // 2 = active again after being inside: the blend restarts its history from these rays.
    let state = select(0.0, select(2.0, 1.0, previous.w > 0.5), is_active);
    // Out of geometry again with a seed written this frame (ADR-2605): the running average restarts
    // with it, or a settled probe's plain hysteresis would keep the seed long past its weight. Without
    // seeding word 9 is 0, so the age runs on as before.
    var age = tag.w + 1;
    if (state > 1.5) {
        age = 1;
    }
    probe_store(probe, vec4<f32>(clamped, state), vec4<i32>(tag.xyz, age), ddgi.ctrl.y);
    let hw = ddgi_header_word(probe);
    ddgi_data[hw] = ddgi_pack_cell(tag.xyz, true);
    ddgi_data[hw + 1u] = ddgi_pack_offset(clamped, s, is_active);
}
