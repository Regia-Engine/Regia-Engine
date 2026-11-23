// SPDX-License-Identifier: Apache-2.0
// Portions ported from Filament (https://github.com/google/filament, commit ac4a2a8554),
// Copyright (C) 2017 The Android Open Source Project, Apache License 2.0: `specular_aa_roughness`
// (from `shaders/src/surface_shading_lit.fs::normalFiltering`).
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file
// except in compliance with the License. You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0. Unless required by applicable law or agreed to in
// writing, software distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.
//
// Terrain: CDLOD vertex shader (heightfield sampling + morph + finite-difference normals)
// and splat-layered material fragment shader. The CPU mirrors of this math live in
// `cdlod.rs` (vertex positions) and `layers.rs` (band weights).
//
// Lighting follows the same EV100 / sun illuminance convention as `regia_render::forward`:
// the sun term is the π-normalised Lambert diffuse of the exposed sun radiance
// (`albedo · sun_color · lux · exposure / π · N·L`, matching `diffuse = albedo/π` there),
// plus the sky ambient, then the AgX tonemap. The colour target is `Rgba8UnormSrgb`, so the
// hardware applies the single sRGB encode on store — no manual gamma here.
//
// The fragment shader has two paths, switched by `material.params0.x`:
//  - the legacy path (no material installed): flat layer colours + procedural mottle,
//    byte-for-byte the M13 look;
//  - the photoreal material path (`TerrainGpu::set_material`): per-layer albedo / normal /
//    height textures with stochastic anti-tiling, height-blended layer transitions,
//    triplanar projection on steep faces, distance-faded micro normals, and whole-map macro
//    variation from tiled mip-chained textures with a coarse fallback for unstreamed tiles.
//    All texture fetches use an explicit LOD derived from the screen-space footprint, so no
//    sampling depends on helper-lane derivatives inside divergent control flow.

struct Frame {
    view_proj: mat4x4<f32>,
    camera_pos_time: vec4<f32>,
    sun_dir_wetness: vec4<f32>,
    // rgb = sun colour (linear), a = illuminance_lux * exposure (pre-multiplied)
    sun_color_exposure: vec4<f32>,
    // rgb = ambient colour (linear), a = debug view (0 = shaded, 1 = LOD tint, 2 = N·L,
    // 3 = albedo, 4 = splat weights, 5 = slope, 6 = height, 7 = visual-vs-sim delta;
    // see `TerrainFrame::debug_view`)
    ambient_debug: vec4<f32>,
    // density, height_falloff, base_height, enabled — mirrors regia_render::FogSettings
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    // Single sun cascade (scatter casts, terrain receives).
    light_view_proj: mat4x4<f32>,
    // x = enabled, y = normalised depth bias, z = world texel size, w = map resolution
    shadow_params: vec4<f32>,
    // x > 0.5: write scene radiance (shared HDR target; post applies exposure once)
    // and roughness in alpha. z = sky radiance scale for height-fog inscatter.
    output_mode: vec4<f32>,
    prev_view_proj: mat4x4<f32>,
    unjittered_view_proj: mat4x4<f32>,
    // rgb = ground-bounce ambient, w > 0.5: hemisphere sky/ground ambient (shared path).
    ground_ambient: vec4<f32>,
    // Planet-shine fill: xyz toward the sky body (w > 0.5 when on), rgb colour · lux.
    fill_dir: vec4<f32>,
    fill_color: vec4<f32>,
    // x = texture mip bias (≤ 0; scales the pixel footprint, `pixel_footprint`); y > 0.5: the
    // shared HDR alpha carries the engine's stochastic-SSR surface code (`sssr_pack`); z = the
    // terrain's anisotropic filtering level (≤ 1 off, `TerrainBudget::anisotropy`); w > 0.5: the
    // shader assembles the anisotropic layer taps itself (`aniso_footprint`, ADR-3957); w ≤ 0: the
    // sampler path filters -w mips softer (`aniso_lod_offset`, ADR-5459).
    texture_lod: vec4<f32>,
    // The frame's extra directional lights (ADR-7254, `extra_lights_direct`): per slot the travel
    // direction (w = 1 when the slot is on), then colour · lux scaled like the sun's illuminance.
    extra_dir: array<vec4<f32>, 6>,
};

// Cascade view-projection for the shared shadow pass. A separate group so the
// colour pipeline can keep group 3 as the terrain sun map.
struct CascadeVp {
    light_view_proj: mat4x4<f32>,
    // xz: the eye the cascade's node list was selected from (its CDLOD morph centre);
    // w = 0 morphs around the frame camera instead.
    morph_eye: vec4<f32>,
}
@group(4) @binding(0) var<uniform> cascade_vp: CascadeVp;

@group(3) @binding(0) var shadow_map: texture_depth_2d;
@group(3) @binding(1) var shadow_samp: sampler_comparison;
// Engine CSM, shared colour entry only (`fs_shared`). `shadow_map` is cascade 0.
// The offscreen entry (`fs_main`) does not reference these bindings.
@group(3) @binding(2) var csm_1: texture_depth_2d;
@group(3) @binding(3) var csm_2: texture_depth_2d;
@group(3) @binding(4) var csm_3: texture_depth_2d;

struct EngineCsm {
    vp0: mat4x4<f32>,
    vp1: mat4x4<f32>,
    vp2: mat4x4<f32>,
    vp3: mat4x4<f32>,
    split: vec4<f32>,
    bias0: vec4<f32>,
    bias1: vec4<f32>,
    bias2: vec4<f32>,
    bias3: vec4<f32>,
    // x = cascade count, y unused, z unused, w = PCF tap count (0 → 16)
    params: vec4<f32>,
    // x, y = colour target size (pixels), z = 1 when the engine SSAO is bound, w = 1 when the
    // engine contact shadows are bound.
    screen: vec4<f32>,
    // The engine's cloud transmittance map (ADR-320): extent, layer base and top, w = 1 when the
    // sun term reads it (`engine_cloud`).
    cloud: vec4<f32>,
    // xyz: the local origin in the terrain's space (zero until a floating-origin rebase). The
    // terrain shades in its own space; the engine's froxel lights and cloud map are local.
    origin: vec4<f32>,
}
@group(3) @binding(5) var<uniform> engine_csm: EngineCsm;
// The frame's screen-space AO (R, white = none; may be half resolution).
@group(3) @binding(6) var engine_ao: texture_2d<f32>;
// The frame's screen-space contact shadows (R = sun visibility, white = lit).
@group(3) @binding(7) var engine_contact: texture_2d<f32>;
// The engine's cloud transmittance map (`regia_render::cloud_shadow_wgsl`, ADR-320).
@group(3) @binding(17) var engine_cloud: texture_2d<f32>;
// @surface-weather-begin
// The shadow atlas holding the precipitation occlusion tile (spec 99 § 5.6).
@group(3) @binding(18) var engine_precip: texture_depth_2d;
// @surface-weather-end

struct Terrain {
    half_width: f32,
    world_width: f32,
    height_scale: f32,
    height_offset: f32,
    sample_max: vec2<f32>,
    grid_size: f32,
    root_level: f32,
    // x = mip levels of `height_texture`; y = terrain budget flags (1 hardware-filtered
    // normals, 2 far layers, 4 far vertex normal), z = far-layer distance (m), w = vertex-
    // normal distance (m). See `regia_render::TerrainBudget`.
    mip_info: vec4<f32>,
    // x = half depth (world_depth / 2 − origin z), y = world depth, zw = 0.
    // On a square map x equals half_width and y equals world_width (ADR-8369).
    depth: vec4<f32>,
};

fn budget_on(bit: u32) -> bool {
    return (u32(terrain.mip_info.y) & bit) != 0u;
}

// Width of the band a budget cut blends in over, metres.
const BUDGET_BAND_M: f32 = 20.0;

// Size of pyramid level `k` (the texture's own mip size, max(1, size >> k)) and the level
// count, from the uniform: `textureDimensions` / `textureNumLevels` with a run-time level
// are texture-unit queries, one per tap of every height read (lane terrain-perf). The CPU
// mirror is `height_mips::level_dims`; the uploaded texels are centred means that read the
// last row and column of an odd side too (`height_mips::parent_taps`, ADR-3367).
fn level_dims(k: u32) -> vec2<i32> {
    let d0 = vec2<i32>(terrain.sample_max) + vec2<i32>(1);
    return max(d0 >> vec2<u32>(k), vec2<i32>(1));
}

fn height_levels() -> u32 {
    return u32(terrain.mip_info.x);
}

struct LayerSlot {
    albedo_roughness: vec4<f32>,
    slope_alt: vec4<f32>,
    params: vec4<f32>,   // tiling (repeats/metre), blend_sharpness, triplanar, layer texel size (m)
    params2: vec4<f32>,  // detail_normal_strength, height_blend, has_height_tex, has_albedo_tex
    params3: vec4<f32>,  // has_orm, weather porosity + 1 (0 = derived, −1 = ORM blue), 1 − weather snow, stack weather flags (slot 0)
};

struct Layers {
    count: vec4<f32>,
    slots: array<LayerSlot, 8>,
};

struct ScorchUniform {
    rows: array<vec4<f32>, 17>,
};

struct Node {
    rect_min: vec2<f32>,
    rect_size: f32,
    level: f32,
    morph_start: f32,
    morph_end: f32,
    grid_size: f32,
    root_level: f32,
};

struct Material {
    // enabled, macro_world_size (m), anti_tiling_cell (m), macro_strength
    params0: vec4<f32>,
    // detail_fade_start (m), detail_fade_end (m), triplanar_auto_slope (rad), macro_normal_strength
    params1: vec4<f32>,
    // sim ground mapping: half_width, world_width, height_scale, height_offset (world_width > 0 = set)
    sim: vec4<f32>,
    // tiles per side, 1/tiles, 0 (unused: `macro_mask` holds the residency), macro texel size (m)
    tiles: vec4<f32>,
    // Road decal segment, world XZ: ax, az, bx, bz. Drawn in this shader so it
    // shares the terrain depth (no z-fight). half_width <= 0 disables it.
    road0: vec4<f32>,
    // half_width (m), road colour rgb (linear)
    road1: vec4<f32>,
    // Strata on steep faces: strength (0 = off), bed thickness (m), warp amplitude (m),
    // ledge strength.
    strata: vec4<f32>,
    // Near-field relief: heightfield sun-occlusion distance (m, 0 = off), cavity AO
    // strength, glint (sparkle) strength, near-field detail strength.
    relief: vec4<f32>,
    // Strata dip: tan(dip)·direction (x, z), lateral drift amplitude (m), talus strength.
    strata2: vec4<f32>,
    // Fractured-rock detail normal strength on steep faces, mid-range relief strength,
    // terrain bounce, 1 − layer ORM occlusion strength (0 = as authored).
    rock: vec4<f32>,
    // Cliff relief (geometry on steep faces): strength (0 = off), fine-grid radius (m),
    // fine-grid fade band (m), minimum slope (rad).
    cliff: vec4<f32>,
    // @ground-look-begin
    // Ground look (ADR-1408, `ground_look.rs`; all zero = off): variation strength, largest
    // variation patch (m), remix share, remix scale.
    ground: vec4<f32>,
    // Snow amount (0 = none), snow line (m), aspect, melt edge.
    winter: vec4<f32>,
    // Snow patch size (m), mud wetness, hoarfrost, 0.
    winter2: vec4<f32>,
    // @ground-look-end
    // Keeps the rows below at their offsets when the ground look is compiled out.
    // @ground-look-off ground_rows: array<vec4<f32>, 3>,
    // Macro tile residency (ADR-3367): bit i of the 256-bit mask set when tile i
    // (x + y · tiles) is resident, and the resident tiles before each word (`macro_layer`).
    macro_mask: array<vec4<u32>, 2>,
    macro_prefix: array<vec4<u32>, 2>,
    // @surface-weather-begin
    // Surface weather (spec 99 § 6.4): wetness, puddles, snow cover, ripple.
    weather: vec4<f32>,
    // The weather clock, 1 when the occlusion tile is bound, the precipitation's extinction
    // (1/m), the atlas edge (texels).
    weather2: vec4<f32>,
    // The occlusion tile: its view-projection, its scale `xy` and offset `zw` in the atlas, and
    // (texel m, 1 / depth extent, 0, 0).
    weather_tile: mat4x4<f32>,
    weather_rect: vec4<f32>,
    weather_bias: vec4<f32>,
    // @surface-weather-end
};

@group(0) @binding(0) var<uniform> frame: Frame;
// The engine's sky irradiance map (equirectangular, irradiance / π over `output_mode.w`),
// the one meshes are lit with; read only when `output_mode.w` > 0 (shared HDR frames).
@group(0) @binding(1) var sky_irradiance: texture_2d<f32>;
@group(0) @binding(2) var sky_samp: sampler;
// The engine's split-sum BRDF table (u = N·V, v = perceptual roughness; R scale, G bias).
@group(0) @binding(3) var sky_brdf: texture_2d<f32>;

// Large meshes' occlusion and bounce proxies (`regia_render::MeshOccluderBlock`, from
// `SharedFrameInfo::mesh_occluders`); count 0 (always on the private path) = none.
@group(0) @binding(4) var<uniform> mesh_occ: MeshOccluders;

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

// BEGIN mesh occluders (`regia_render::mesh_occluders`): the functions that read `mesh_occ`.
// Their structs and pure helpers are the `mesh occluder geometry` block of
// `regia::forward::occluder_geometry` (ADR-1552); terrain.wgsl copies both blocks (test
// `occluder_wgsl_copies_match`).
// Vertex `j` (world) of proxy `i`'s polygon `kind`: 0..5 a box face (+A0, −A0, +A1, −A1, +A2,
// −A2; four corners in loop order), 6 the sun shadow on the ground plane.
fn occ_vertex(i: u32, kind: u32, j: u32) -> vec3<f32> {
    if (kind == 6u) {
        let row = mesh_occ.items[i].shadow[j / 2u];
        let xz = select(row.xy, row.zw, (j % 2u) == 1u);
        return vec3<f32>(xz.x, mesh_occ.items[i].center.w, xz.y);
    }
    let k = kind / 2u;
    let s = select(1.0, -1.0, (kind % 2u) == 1u);
    let k1 = (k + 1u) % 3u;
    let k2 = (k + 2u) % 3u;
    let a = mesh_occ.items[i].axes[k];
    let u = mesh_occ.items[i].axes[k1].xyz * mesh_occ.items[i].axes[k1].w;
    let v = mesh_occ.items[i].axes[k2].xyz * mesh_occ.items[i].axes[k2].w;
    let su = select(1.0, -1.0, j == 2u || j == 3u);
    let sv = select(1.0, -1.0, j == 1u || j == 2u);
    return mesh_occ.items[i].center.xyz + a.xyz * (s * a.w) + u * su + v * sv;
}

// Cosine-weighted view factor from `p` (normal `n`) of proxy `i`'s convex polygon `kind`
// (`count` vertices), clipped to the receiver's upper hemisphere: (1/2π)|Σ θ_i n·ĝ_i| over the
// clipped polygon's edges, the closing edge along the clip plane added from exit to entry.
// Each vertex is read and normalised once (carried to the next edge).
fn occ_polygon_ff(i: u32, kind: u32, count: u32, p: vec3<f32>, n: vec3<f32>) -> f32 {
    var sum = 0.0;
    var exit = vec3<f32>(0.0);
    var entry = vec3<f32>(0.0);
    var crossings = 0u;
    var inside = 0u;
    var a = occ_vertex(i, kind, 0u) - p;
    var an = occ_unit(a);
    var da = dot(a, n);
    for (var j = 0u; j < count; j++) {
        let b = occ_vertex(i, kind, (j + 1u) % count) - p;
        let bn = occ_unit(b);
        let db = dot(b, n);
        if (da >= 0.0) {
            inside += 1u;
        }
        if (da >= 0.0 && db >= 0.0) {
            sum += occ_edge(an, bn, n);
        } else if (da >= 0.0) {
            let x = occ_unit(mix(a, b, da / (da - db)));
            sum += occ_edge(an, x, n);
            exit = x;
            crossings += 1u;
        } else if (db >= 0.0) {
            let x = occ_unit(mix(a, b, da / (da - db)));
            sum += occ_edge(x, bn, n);
            entry = x;
            crossings += 1u;
        }
        a = b;
        an = bn;
        da = db;
    }
    if (inside == 0u) {
        return 0.0;
    }
    if (crossings == 2u) {
        sum += occ_edge(exit, entry, n);
    }
    return abs(sum) * 0.15915494;
}

// The occluders listed in the grid cell under `p` (bit i = occluder i): a superset of the reach
// spheres that hold it, 0 outside the grid (which bounds every sphere).
fn mesh_occluder_cell_mask(p: vec3<f32>) -> u32 {
    let g = mesh_occ.grid;
    let xz = (p.xz - g.xy) * g.z;
    if (!(all(xz >= vec2<f32>(0.0)) && all(xz <= vec2<f32>(32.0)))) {
        return 0u;
    }
    let c = min(vec2<u32>(xz), vec2<u32>(31u));
    let k = c.y * 32u + c.x;
    let q = mesh_occ.cells[k >> 2u];
    let lane = k & 3u;
    // Component by `select`, not `q[lane]` (no dynamic vector index; Mali-G78).
    return select(select(q.x, q.y, lane == 1u), select(q.z, q.w, lane == 3u), lane >= 2u);
}

// How far occluder `i`'s full term has faded into its contact blob (`near_fade`; 0 = none).
fn occ_near_fade(i: u32) -> f32 {
    let q = mesh_occ.near_fade[i >> 2u];
    let lane = i & 3u;
    // Component by `select`, not `q[lane]` (no dynamic vector index; Mali-G78).
    return select(select(q.x, q.y, lane == 1u), select(q.z, q.w, lane == 3u), lane >= 2u);
}

// The contact blob of near occluder `i` at `p` (`MeshOccluder::contact_blob` from the packed
// item: the XZ-projected half diagonal and the top above the ground).
fn occ_item_blob(i: u32, p: vec3<f32>, n: vec3<f32>) -> f32 {
    let a0 = mesh_occ.items[i].axes[0];
    let a1 = mesh_occ.items[i].axes[1];
    let a2 = mesh_occ.items[i].axes[2];
    let r2 = a0.w * a0.w * max(1.0 - a0.y * a0.y, 0.0) + a1.w * a1.w * max(1.0 - a1.y * a1.y, 0.0)
        + a2.w * a2.w * max(1.0 - a2.y * a2.y, 0.0);
    let up = abs(a0.y) * a0.w + abs(a1.y) * a1.w + abs(a2.y) * a2.w;
    let c = mesh_occ.items[i].center;
    return occ_blob(p, n, c.xz, sqrt(r2), c.w, c.y + up - c.w);
}

// The far band's contact blobs at `p` (ADR-7970): x = the sky share they hide (summed), y =
// the fill visibility they leave (the product). Sorted by x: a binary search finds the first
// blob within the largest reach of `p.x`, and the walk stops past it.
fn occ_far_blobs(p: vec3<f32>, n: vec3<f32>) -> vec2<f32> {
    var out = vec2<f32>(0.0, 1.0);
    let count = min(u32(mesh_occ.header.z + 0.5), 128u);
    let reach = mesh_occ.grid.w;
    var lo = 0u;
    var hi = count;
    while (lo < hi) {
        let mid = (lo + hi) / 2u;
        if (mesh_occ.far[mid].x < p.x - reach) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    for (var i = lo; i < count; i++) {
        let b = mesh_occ.far[i];
        if (b.x > p.x + reach) {
            break;
        }
        let t = mesh_occ.far_top[i >> 2u];
        let lane = i & 3u;
        let top = select(select(t.x, t.y, lane == 1u), select(t.z, t.w, lane == 3u), lane >= 2u);
        let o = occ_blob(p, n, b.xy, b.z, b.w, top);
        out = vec2<f32>(out.x + o, out.y * (1.0 - o));
    }
    return out;
}

// The light a point receives from the frame's large-mesh proxies. `terrain`: the receiver is
// the ground (no shadow polygon; the box hides sky); otherwise a mesh (the box's sun shadow on
// its ground plane removes the sun's share of the ground the IBL assumed lit).
fn mesh_occluder_light(p: vec3<f32>, n: vec3<f32>, terrain: bool) -> OccLight {
    var delta = vec3<f32>(0.0);
    var fill_vis = 1.0;
    let sky = mesh_occ.sky.rgb;
    let ground_sun = mesh_occ.ground_sun.rgb;
    let fill_dir = mesh_occ.fill_dir.xyz;
    let fill_on = mesh_occ.fill_dir.w > 0.5 && dot(n, fill_dir) > 0.0;
    // A tight pass over the reach spheres of the occluders listed in this point's grid cell
    // first: the full body below runs only for the occluders that reach it (most pixels: none).
    var mask = 0u;
    var listed = mesh_occluder_cell_mask(p);
    while (listed != 0u) {
        let i = firstTrailingBit(listed);
        listed &= listed - 1u;
        let b = mesh_occ.bounds[i];
        let d = p - b.xyz;
        if (dot(d, d) < b.w * b.w) {
            mask |= 1u << i;
        }
    }
    while (mask != 0u) {
        let i = firstTrailingBit(mask);
        mask &= mask - 1u;
        // Near the far band's boundary the full term fades into the contact blob (ADR-7970);
        // `full` is exactly 1 otherwise, so the terms below are unchanged.
        let fade = occ_near_fade(i);
        let full = 1.0 - fade;
        var fill_near = 1.0;
        let c = mesh_occ.items[i].center.xyz;
        let r = mesh_occ.items[i].faces[0].w;
        let dist = distance(p, c);
        if (dist < 5.5 * r) {
            let w = (1.0 - smoothstep(4.0 * r, 5.5 * r, dist)) * full;
            let a0 = mesh_occ.items[i].axes[0];
            let a1 = mesh_occ.items[i].axes[1];
            let a2 = mesh_occ.items[i].axes[2];
            let h = vec3<f32>(a0.w, a1.w, a2.w);
            let local = vec3<f32>(dot(p - c, a0.xyz), dot(p - c, a1.xyz), dot(p - c, a2.xyz));
            let inside = all(abs(local) < h + vec3<f32>(0.02));
            var q = p;
            if (inside && terrain) {
                // Ground under the box (between the wheels): seen from just below its bottom.
                q.y = c.y - dot(abs(vec3<f32>(a0.y, a1.y, a2.y)), h) - 0.01;
            }
            if (!inside || terrain) {
                let ql = vec3<f32>(dot(q - c, a0.xyz), dot(q - c, a1.xyz), dot(q - c, a2.xyz));
                // Exact clipped polygons within 1.5 × the largest half extent of the box's
                // surface; beyond, each face as a disc (far cheaper).
                let near = length(max(abs(ql) - h, vec3<f32>(0.0))) < 1.5 * max(h.x, max(h.y, h.z));
                for (var k = 0u; k < 3u; k++) {
                    // Axis and half extent from the uniform array, not `ql[k]` / `h[k]`: a
                    // dynamic vector index lost Mali-G78's device (terrain splat weights).
                    let ak4 = mesh_occ.items[i].axes[k];
                    let ak = ak4.xyz;
                    let l = dot(q - c, ak);
                    if (abs(l) <= ak4.w + 1e-4) {
                        continue;
                    }
                    let f = 2u * k + select(0u, 1u, l < 0.0);
                    let s = select(-1.0, 1.0, l > 0.0);
                    let fc = c + ak * (s * ak4.w) - q;
                    var ff = 0.0;
                    if (near) {
                        ff = occ_polygon_ff(i, f, 4u, q, n);
                    } else {
                        let area = 4.0 * mesh_occ.items[i].axes[(k + 1u) % 3u].w * mesh_occ.items[i].axes[(k + 2u) % 3u].w;
                        ff = occ_disc_ff(area, fc, ak * s, n);
                    }
                    if (ff <= 0.0) {
                        continue;
                    }
                    var hidden = sky;
                    if (!terrain) {
                        let t = smoothstep(-0.15, 0.15, normalize(fc).y);
                        hidden = mix(mesh_occ.ground.rgb - ground_sun * 0.5, sky, t);
                    }
                    delta += (mesh_occ.items[i].faces[f].rgb - hidden) * (ff * w);
                }
                // The fill's rays only reach the box when its bounding sphere meets their cone.
                let cq = c - q;
                let along = dot(cq, fill_dir);
                let spread = mesh_occ.header.y * 0.7;
                let reach = r + max(along, 0.0) * spread * 1.05 + 0.01;
                if (fill_on && along > -r && dot(cq, cq) - along * along < reach * reach) {
                    // Five rays over the fill's disc.
                    var t1 = cross(fill_dir, vec3<f32>(0.0, 1.0, 0.0));
                    if (dot(t1, t1) < 1e-6) {
                        t1 = vec3<f32>(1.0, 0.0, 0.0);
                    }
                    t1 = normalize(t1);
                    let t2 = cross(fill_dir, t1);
                    var hits = 0.0;
                    for (var j = 0u; j < 5u; j++) {
                        var off = vec2<f32>(0.0);
                        if (j == 1u) { off = vec2<f32>(spread, 0.0); }
                        if (j == 2u) { off = vec2<f32>(-spread, 0.0); }
                        if (j == 3u) { off = vec2<f32>(0.0, spread); }
                        if (j == 4u) { off = vec2<f32>(0.0, -spread); }
                        let dw = normalize(fill_dir + t1 * off.x + t2 * off.y);
                        let dl = vec3<f32>(dot(dw, a0.xyz), dot(dw, a1.xyz), dot(dw, a2.xyz));
                        hits += select(0.0, 1.0, occ_slab_hit(ql, dl, h));
                    }
                    fill_near = 1.0 - hits * 0.2;
                }
            }
        }
        let verts = min(u32(mesh_occ.items[i].faces[1].w + 0.5), 6u);
        let ground_y = mesh_occ.items[i].center.w;
        if (!terrain && verts >= 3u && p.y > ground_y + 0.01) {
            let pc = vec3<f32>(mesh_occ.items[i].faces[2].w, ground_y, mesh_occ.items[i].faces[3].w);
            let rp = max(mesh_occ.items[i].faces[4].w, 0.1);
            let dp = distance(p, pc);
            if (dp < 4.5 * rp) {
                let w = (1.0 - smoothstep(3.0 * rp, 4.5 * rp, dp)) * full;
                var ff = 0.0;
                if (dp < 2.0 * rp) {
                    ff = occ_polygon_ff(i, 6u, verts, p, n);
                } else {
                    // Far field: the polygon (area in faces[5].w) as a disc.
                    ff = occ_disc_ff(mesh_occ.items[i].faces[5].w, pc - p, vec3<f32>(0.0, 1.0, 0.0), n);
                }
                delta -= ground_sun * (ff * w);
            }
        }
        if (fade > 0.0) {
            // The rest is the proxy's contact blob: it hides the sky and, mixed with the full
            // term's fill shadow, the fill.
            let o = occ_item_blob(i, p, n);
            delta -= sky * (fade * o);
            fill_vis *= mix(1.0 - o, fill_near, full);
        } else {
            fill_vis *= fill_near;
        }
    }
    if (mesh_occ.header.z > 0.5) {
        let blobs = occ_far_blobs(p, n);
        delta -= sky * blobs.x;
        fill_vis *= blobs.y;
    }
    return OccLight(delta, fill_vis);
}

// Share of a reflected ray's ground hit that lies in a proxy's sun shadow, softened over the
// specular lobe's footprint (`rough` = GGX alpha): the IBL's lower half shows lit ground there.
fn mesh_occluder_reflection_shadow(p: vec3<f32>, r: vec3<f32>, rough: f32) -> f32 {
    var cover = 0.0;
    let count = min(u32(mesh_occ.header.x + 0.5), 32u);
    if (r.y > -1e-3) {
        return 0.0;
    }
    for (var i = 0u; i < count; i++) {
        // One vec4 per occluder decides most of them (`shadow_bounds`).
        let sb = mesh_occ.shadow_bounds[i];
        let ground_y = sb.w;
        if (sb.z < 0.0 || p.y <= ground_y + 0.01) {
            continue;
        }
        let t = (ground_y - p.y) / r.y;
        let hit = p.xz + r.xz * t;
        let width = max(t * rough * 2.0, 0.05);
        if (distance(hit, sb.xy) > sb.z + 2.0 * width) {
            continue;
        }
        let verts = min(u32(mesh_occ.items[i].faces[1].w + 0.5), 6u);
        var sd = 1e9;
        for (var j = 0u; j < verts; j++) {
            let a = occ_vertex(i, 6u, j).xz;
            let b = occ_vertex(i, 6u, (j + 1u) % verts).xz;
            let e = b - a;
            let inward = vec2<f32>(-e.y, e.x) / max(length(e), 1e-6);
            sd = min(sd, dot(hit - a, inward));
        }
        cover = max(cover, smoothstep(-width, width, sd) * (1.0 - occ_near_fade(i)));
    }
    return cover;
}
// END mesh occluders

// How much ground point `p` lies in a proxy's sun shade (`FillCanyonScope::NearOccluders`,
// ADR-8412; CPU mirror `regia_terrain::near_occluder_shade`): per proxy with a shadow polygon,
// 1 inside it on its ground plane, falling to 0 over a quarter of its radius (at least 0.1 m)
// outside the edge and over one to two box heights (at least 0.5 m) above or below the plane;
// the largest over the proxies listed in this point's grid cell. Terrain only (not in the
// block the forward pass shares).
fn mesh_occluder_shade(p: vec3<f32>) -> f32 {
    var cover = 0.0;
    var listed = mesh_occluder_cell_mask(p);
    while (listed != 0u) {
        let i = firstTrailingBit(listed);
        listed &= listed - 1u;
        let sb = mesh_occ.shadow_bounds[i];
        if (sb.z < 0.0) {
            continue;
        }
        let soft = max(0.25 * sb.z, 0.1);
        if (distance(p.xz, sb.xy) > sb.z + soft) {
            continue;
        }
        let c = mesh_occ.items[i].center;
        let a0 = mesh_occ.items[i].axes[0];
        let a1 = mesh_occ.items[i].axes[1];
        let a2 = mesh_occ.items[i].axes[2];
        let depth = abs(a0.y) * a0.w + abs(a1.y) * a1.w + abs(a2.y) * a2.w;
        let tall = max(c.y + depth - sb.w, 0.5);
        let vertical = 1.0 - smoothstep(tall, 2.0 * tall, abs(p.y - sb.w));
        if (vertical <= 0.0) {
            continue;
        }
        let verts = min(u32(mesh_occ.items[i].faces[1].w + 0.5), 6u);
        var sd = 1e9;
        for (var j = 0u; j < verts; j++) {
            let a = occ_vertex(i, 6u, j).xz;
            let b = occ_vertex(i, 6u, (j + 1u) % verts).xz;
            let e = b - a;
            let inward = vec2<f32>(-e.y, e.x) / max(length(e), 1e-6);
            sd = min(sd, dot(p.xz - a, inward));
        }
        cover = max(cover, smoothstep(-soft, 0.0, sd) * vertical);
    }
    return cover;
}

// Same mapping as the mesh path's `equirect_uv` (lab_forward.wgsl).
fn sky_equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / AERIAL_PI) + 0.5, theta / AERIAL_PI);
}
// The engine's irradiance probes at a point (`engine_gi`): `diffuse` = irradiance / π toward
// the normal (a = how much of it to take); `spec` = the probe radiance in the GGX specular lobe
// of the perceptual roughness given (its mean vector leans from the mirror direction toward the
// normal as the roughness grows: `probe_spec_lobe`, ADR-1103 / ADR-1377), valid when `has_spec`
// (probe specular on, `render.set_probe_specular`).
struct EngineGi {
    diffuse: vec4<f32>,
    spec: vec3<f32>,
    has_spec: bool,
}
// @engine-gi-begin
// The engine's irradiance probes (`EngineGi`). The shared colour pass replaces this stub with
// the probe lookup (`with_engine_gi`); the private path has no probes.
fn engine_gi(frag: vec2<f32>, p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, eye: vec3<f32>, perceptual: f32) -> EngineGi {
    return EngineGi(vec4<f32>(0.0), vec3<f32>(0.0), false);
}
// Whether `engine_gi`'s light holds the sky-body fill (a baked volume's): the live fill then
// gives way to it where it covers.
fn engine_gi_has_fill() -> bool {
    return false;
}
// @engine-gi-end

// @engine-lights-begin
// The frame's point and spot lights (`regia_render::shared_local_lights_wgsl`: the forward
// pass's froxel lists, attenuation, shadow atlas and BRDF). The shared colour pass replaces this
// stub with that code (`with_engine_lights`, group 3 bindings 10–16); the private path has no
// clustered lights.
fn engine_local_lights_on() -> bool {
    return false;
}
fn engine_local_lights(pix: vec2<f32>, p: vec3<f32>, n: vec3<f32>, n_geo: vec3<f32>, v: vec3<f32>, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32) -> vec3<f32> {
    return vec3<f32>(0.0);
}
// @engine-lights-end

@group(1) @binding(0) var height_texture: texture_2d<f32>;
@group(1) @binding(2) var<uniform> terrain: Terrain;
@group(1) @binding(3) var<uniform> layers: Layers;
@group(1) @binding(4) var<uniform> scorch: ScorchUniform;
@group(1) @binding(5) var macro_albedo: texture_2d_array<f32>;
@group(1) @binding(6) var macro_normal: texture_2d_array<f32>;
@group(1) @binding(7) var macro_coarse_albedo: texture_2d_array<f32>;
@group(1) @binding(8) var macro_coarse_normal: texture_2d_array<f32>;
@group(1) @binding(9) var layer_albedo: texture_2d_array<f32>;
@group(1) @binding(10) var layer_normal: texture_2d_array<f32>;
@group(1) @binding(11) var layer_height: texture_2d_array<f32>;
@group(1) @binding(12) var<uniform> material: Material;
@group(1) @binding(13) var material_samp: sampler;
// The same trilinear repeat sampler with anisotropic filtering (clamp 16; the shader limits
// the ratio to the frame's level, `aniso_grads`).
@group(1) @binding(22) var material_aniso_samp: sampler;
@group(1) @binding(14) var sim_height_texture: texture_2d<f32>;
@group(1) @binding(15) var layer_orm: texture_2d_array<f32>;
// Splat weight packs over the whole map (`TerrainGpu::set_splat_weights`, ADR-1402): layer 0 holds the
// weights of material layers 0–3 in RGBA, layer 1 those of 4–7. `layers.count.w` is a pack texel in map
// UV, 0 when the map has no packs (then a 1×1 zero texture is bound and never read).
@group(1) @binding(23) var splat_weights: texture_2d_array<f32>;

// The two splat packs at world `xz`, filtered at the pixel footprint `footprint_m` (metres). Both are
// all ones without packs, and where the packs hold no weight (the slope and altitude bands decide there).
// A struct, not an array: naga gives a function-local array a stride SPIR-V 1.6 rejects (ADR-026).
struct SplatPacks {
    a: vec4<f32>,
    b: vec4<f32>,
};

fn splat_packs(xz: vec2<f32>, footprint_m: f32) -> SplatPacks {
    let t = layers.count.w;
    if (t <= 0.0) {
        return SplatPacks(vec4<f32>(1.0), vec4<f32>(1.0));
    }
    let uv = clamp(
        (xz + vec2<f32>(terrain.half_width)) / terrain.world_width,
        vec2<f32>(0.5 * t),
        vec2<f32>(1.0 - 0.5 * t),
    );
    let k = far_splat_fade(terrain.world_width * t);
    // Far field: half a level coarser with the B-spline (`FAR_SPLAT_LOD_BIAS`); `k` = 0 adds nothing.
    let lod = min(
        clamp(log2(max(footprint_m, 1e-6) / max(terrain.world_width * t, 1e-6)), 0.0, 12.0) + FAR_SPLAT_LOD_BIAS * k,
        12.0,
    );
    var p0 = vec4<f32>(0.0);
    var p1 = vec4<f32>(0.0);
    if (k > 0.0) {
        // Four bilinear taps at the offsets and weights of the cubic B-spline (mixed with linear
        // interpolation by `k`), placed on the texel grid of the level the footprint reads.
        let level = min(u32(lod), textureNumLevels(splat_weights) - 1u);
        let dims = vec2<f32>(textureDimensions(splat_weights, level));
        let x = uv * dims - vec2<f32>(0.5);
        let base = floor(x) + vec2<f32>(0.5);
        let ax = far_splat_axis(x - floor(x), k);
        let lo = vec2<f32>(0.5 * t);
        let hi = vec2<f32>(1.0 - 0.5 * t);
        let uv00 = clamp((base + ax.h0) / dims, lo, hi);
        let uv11 = clamp((base + ax.h1) / dims, lo, hi);
        let uv10 = vec2<f32>(uv11.x, uv00.y);
        let uv01 = vec2<f32>(uv00.x, uv11.y);
        let w00 = ax.g0.x * ax.g0.y;
        let w10 = ax.g1.x * ax.g0.y;
        let w01 = ax.g0.x * ax.g1.y;
        let w11 = ax.g1.x * ax.g1.y;
        p0 = textureSampleLevel(splat_weights, material_samp, uv00, 0, lod) * w00
            + textureSampleLevel(splat_weights, material_samp, uv10, 0, lod) * w10
            + textureSampleLevel(splat_weights, material_samp, uv01, 0, lod) * w01
            + textureSampleLevel(splat_weights, material_samp, uv11, 0, lod) * w11;
        p1 = textureSampleLevel(splat_weights, material_samp, uv00, 1, lod) * w00
            + textureSampleLevel(splat_weights, material_samp, uv10, 1, lod) * w10
            + textureSampleLevel(splat_weights, material_samp, uv01, 1, lod) * w01
            + textureSampleLevel(splat_weights, material_samp, uv11, 1, lod) * w11;
    } else {
        p0 = textureSampleLevel(splat_weights, material_samp, uv, 0, lod);
        p1 = textureSampleLevel(splat_weights, material_samp, uv, 1, lod);
    }
    if (dot(p0, vec4<f32>(1.0)) + dot(p1, vec4<f32>(1.0)) < 1e-3) {
        return SplatPacks(vec4<f32>(1.0), vec4<f32>(1.0));
    }
    return SplatPacks(p0, p1);
}

// Far-field splat reconstruction (ADR-1456; CPU mirror `regia_terrain::far_splat`). Bilinear
// interpolation bends at every texel edge, and the height blend's contrast (`surface_layers`)
// draws the 0.5 contour of that field as a sharp outline: where a pack texel spans only a few
// pixels, the bends read as a notched, stair-stepped outline around every painted patch (look
// round 2, finding 8; switching the height blend off only blurred them). There the packs are read
// with a cubic B-spline instead (smooth across texel edges; four bilinear taps, Sigg & Hadwiger
// 2005), mixed in as the footprint's short axis grows from `FAR_SPLAT_START` to `FAR_SPLAT_FULL`
// pack texels, and read half a level coarser by the same share (`FAR_SPLAT_LOD_BIAS`): the data's own
// stair of a few texels along a slanted edge still read as steps at the level the footprint picks.
// Nearer ground (a texel over 10 pixels across) keeps the two bilinear taps exactly.
const FAR_SPLAT_START: f32 = 0.1;
const FAR_SPLAT_FULL: f32 = 0.25;
const FAR_SPLAT_LOD_BIAS: f32 = 0.5;

// Share of the cubic B-spline in the splat reconstruction for pack texels of `texel_m` metres:
// 0 where no footprint was measured (the virtual-texture bake).
fn far_splat_fade(texel_m: f32) -> f32 {
    let short_m = min(length(aniso_dx), length(aniso_dy));
    let r = short_m / max(texel_m, 1e-6);
    return smoothstep01((r - FAR_SPLAT_START) / (FAR_SPLAT_FULL - FAR_SPLAT_START));
}

// Per axis (x, y lanes) the two bilinear taps of a 4-texel filter: weights `g0`, `g1` and offsets
// `h0`, `h1` in texels from the centre of the texel at or before the sample. The filter is the cubic
// B-spline at fraction `f` mixed by `k` with linear interpolation (k = 0: the taps sit on the two
// nearest texel centres with the bilinear weights).
struct FarSplatAxis {
    g0: vec2<f32>,
    g1: vec2<f32>,
    h0: vec2<f32>,
    h1: vec2<f32>,
};

fn far_splat_axis(f: vec2<f32>, k: f32) -> FarSplatAxis {
    let one = vec2<f32>(1.0);
    let f2 = f * f;
    let f3 = f2 * f;
    let r = one - f;
    let b0 = r * r * r / 6.0;
    let b1 = (3.0 * f3 - 6.0 * f2 + vec2<f32>(4.0)) / 6.0;
    let b2 = (-3.0 * f3 + 3.0 * f2 + 3.0 * f + one) / 6.0;
    let b3 = f3 / 6.0;
    let a0 = b0 * k;
    let a1 = mix(r, b1, k);
    let a2 = mix(f, b2, k);
    let a3 = b3 * k;
    let g0 = a0 + a1;
    let g1 = a2 + a3;
    let h0 = a1 / max(g0, vec2<f32>(1e-6)) - one;
    let h1 = a3 / max(g1, vec2<f32>(1e-6)) + one;
    return FarSplatAxis(g0, g1, h0, h1);
}

// Scatter contact grounding (`regia_terrain::scatter::contact`): an R8 occlusion window
// around the camera. window = (origin x, 1/size, origin z, live); strength = (ambient, sun,
// ground tint, -).
struct ContactParams {
    window: vec4<f32>,
    strength: vec4<f32>,
};
@group(1) @binding(16) var contact_map: texture_2d<f32>;
@group(1) @binding(17) var<uniform> contact: ContactParams;

// Occlusion under scatter instances at world XZ (0 = open ground). Fades out over the last
// 1/16 of the window so the window edge never shows.
fn contact_occlusion(xz: vec2<f32>) -> f32 {
    if (contact.window.w < 0.5) {
        return 0.0;
    }
    let uv = vec2<f32>((xz.x - contact.window.x) * contact.window.y, (xz.y - contact.window.z) * contact.window.y);
    if (uv.x <= 0.0 || uv.y <= 0.0 || uv.x >= 1.0 || uv.y >= 1.0) {
        return 0.0;
    }
    let edge = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    let fade = clamp(edge * 16.0, 0.0, 1.0);
    return textureSampleLevel(contact_map, material_samp, uv, 0.0).r * fade;
}
// Every node the frame draws, and the per-pass draw lists: instance `i` of a draw is
// node `nodes[node_slots[i]]` (one instanced draw per pass instead of one draw per node).
@group(2) @binding(0) var<storage, read> nodes: array<Node>;
@group(2) @binding(1) var<storage, read> node_slots: array<u32>;

fn load_node(instance: u32) -> Node {
    return nodes[node_slots[instance]];
}

// Terrain holes (`crate::holes`, ADR-088): [0] window count, [1..4) unused, then 8 words per
// window (centre x, z, cos, sin of its rotation, 1 / half extent x, z, two unused; f32 bits),
// then each window's HOLE_TEXELS² R8 coverage, four texels a word. Bound for every pass, read
// only by the hole-cut pipelines, which draw the nodes a hole partly covers: the `*_holes`
// entry points (depth-only passes and velocity) and the colour / receiver entry points
// compiled with `hole_cut` = true.
@group(2) @binding(2) var<storage, read> holes: array<u32>;
const HOLE_TEXELS: u32 = 256u;
// Pipeline-overridable: true in the hole-cut colour, offscreen and receiver pipelines. The cut
// sits in the entry point itself (not a shared helper called by two entry points): a
// `fs_shared` that only called a helper cost the terrain forward pass 0.09 ms on the
// RTX 5090 (bore_twelve, desktop-high), with no hole in the level.
override hole_cut: bool = false;

fn hole_texel(cov: u32, x: i32, z: i32) -> f32 {
    let last = i32(HOLE_TEXELS) - 1;
    let i = u32(clamp(z, 0, last)) * HOLE_TEXELS + u32(clamp(x, 0, last));
    let word = holes[cov + i / 4u];
    return f32((word >> ((i % 4u) * 8u)) & 255u) / 255.0;
}

// A lattice window (word 6 = 1, ADR-8470; `LatticeWindow::byte_at` mirrors it): the cell byte
// under `p` in bits 0-7 and 256 when `p` is in the cell's upper triangle (the `(x0, z1)-(x1, z0)`
// diagonal: `fu + fv > 1`), or 0 outside the window. Words 0-1 are the corner, 2-3 the size in
// cells, 4-5 one over the pitch; the block holds one byte per cell, HOLE_TEXELS cells a row.
fn lattice_byte(o: u32, cov: u32, p: vec2<f32>) -> u32 {
    let corner = vec2<f32>(bitcast<f32>(holes[o]), bitcast<f32>(holes[o + 1u]));
    let size = vec2<f32>(bitcast<f32>(holes[o + 2u]), bitcast<f32>(holes[o + 3u]));
    let inv = vec2<f32>(bitcast<f32>(holes[o + 4u]), bitcast<f32>(holes[o + 5u]));
    let f = (p - corner) * inv;
    if (!(f.x >= 0.0 && f.y >= 0.0 && f.x < size.x && f.y < size.y)) {
        return 0u;
    }
    let c = floor(f);
    let r = f - c;
    let i = u32(c.y) * HOLE_TEXELS + u32(c.x);
    let byte = (holes[cov + i / 4u] >> ((i % 4u) * 8u)) & 255u;
    return byte | select(0u, 256u, r.x + r.y > 1.0);
}

// True where the pre-morph grid position `ref_xz` lies in a lattice window cell that keeps no
// cliff relief (bit 2: within two cells of a mesh-layer mouth's cut, ADR-8470).
fn lattice_near_ring(ref_xz: vec2<f32>) -> bool {
    let count = holes[0];
    let cov0 = 4u + count * 8u;
    for (var w = 0u; w < count; w = w + 1u) {
        let o = 4u + w * 8u;
        if (holes[o + 6u] == 1u && (lattice_byte(o, cov0 + w * (HOLE_TEXELS * HOLE_TEXELS / 4u), ref_xz) & 4u) != 0u) {
            return true;
        }
    }
    return false;
}

// True where world XZ lies in a hole: a coverage window's bilinear coverage reaches 50 % at
// `xz`, the heightfield point before cliff relief (`VOut::base`), so the cut is fixed in XZ under
// LOD morphing and relief, and the same in every pass (`HoleWindow::coverage_at` mirrors it); or
// a lattice window (a mesh-layer mouth, ADR-8470) cuts the lattice triangle under `ref_xz`, the
// interpolated pre-morph grid position, so on every drawn triangle the cut is the affine image
// of the cooked lattice cut (`LatticeHoleCut::cuts` mirrors it).
fn in_hole(xz: vec2<f32>, ref_xz: vec2<f32>) -> bool {
    let count = holes[0];
    let cov0 = 4u + count * 8u;
    for (var w = 0u; w < count; w = w + 1u) {
        let o = 4u + w * 8u;
        if (holes[o + 6u] == 1u) {
            let b = lattice_byte(o, cov0 + w * (HOLE_TEXELS * HOLE_TEXELS / 4u), ref_xz);
            if ((b & select(1u, 2u, b >= 256u)) != 0u) {
                return true;
            }
            continue;
        }
        let d = xz - vec2<f32>(bitcast<f32>(holes[o]), bitcast<f32>(holes[o + 1u]));
        let cs = vec2<f32>(bitcast<f32>(holes[o + 2u]), bitcast<f32>(holes[o + 3u]));
        let inv = vec2<f32>(bitcast<f32>(holes[o + 4u]), bitcast<f32>(holes[o + 5u]));
        let local = vec2<f32>(cs.x * d.x + cs.y * d.y, -cs.y * d.x + cs.x * d.y);
        let uv = local * inv * 0.5 + vec2<f32>(0.5);
        if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
            continue;
        }
        let f = uv * f32(HOLE_TEXELS) - vec2<f32>(0.5);
        let i0 = vec2<i32>(floor(f));
        let t = f - floor(f);
        let cov = cov0 + w * (HOLE_TEXELS * HOLE_TEXELS / 4u);
        let a = hole_texel(cov, i0.x, i0.y) + (hole_texel(cov, i0.x + 1, i0.y) - hole_texel(cov, i0.x, i0.y)) * t.x;
        let b = hole_texel(cov, i0.x, i0.y + 1) + (hole_texel(cov, i0.x + 1, i0.y + 1) - hole_texel(cov, i0.x, i0.y + 1)) * t.x;
        if (a + (b - a) * t.y >= 0.5) {
            return true;
        }
    }
    return false;
}

// Height at continuous sample coordinates: manual bilinear over texel loads, the exact math
// of `TerrainMap::sample_bilinear_grid` (border-clamped). Manual filtering keeps the base
// surface identical to the CPU sampler and needs no filterable-float texture support.
fn height_at(s: vec2<f32>) -> f32 {
    let dims = level_dims(0u);
    let max_s = vec2<f32>(f32(dims.x - 1), f32(dims.y - 1));
    let p = clamp(s, vec2<f32>(0.0), max_s);
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let h00 = textureLoad(height_texture, i, 0).r;
    let h10 = textureLoad(height_texture, vec2<i32>(i1.x, i.y), 0).r;
    let h01 = textureLoad(height_texture, vec2<i32>(i.x, i1.y), 0).r;
    let h11 = textureLoad(height_texture, i1, 0).r;
    let a = mix(h00, h10, f.x);
    let b = mix(h01, h11, f.x);
    return mix(a, b, f.y) * terrain.height_scale + terrain.height_offset;
}

// Same manual bilinear over an arbitrary heightfield texture (the sim ground mirror).
fn tex_height_at(tex: texture_2d<f32>, s: vec2<f32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(tex));
    let max_s = vec2<f32>(f32(dims.x - 1), f32(dims.y - 1));
    let p = clamp(s, vec2<f32>(0.0), max_s);
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let h00 = textureLoad(tex, i, 0).r;
    let h10 = textureLoad(tex, vec2<i32>(i1.x, i.y), 0).r;
    let h01 = textureLoad(tex, vec2<i32>(i.x, i1.y), 0).r;
    let h11 = textureLoad(tex, i1, 0).r;
    let a = mix(h00, h10, f.x);
    let b = mix(h01, h11, f.x);
    return mix(a, b, f.y);
}

// Height at world XZ.
fn height_w(xz: vec2<f32>) -> f32 {
    return height_at(sample_coord(xz));
}

// ---------------------------------------------------------------------------
// Box-filtered height pyramid (Williams 1983 mip-map, applied to geometry). CPU mirror:
// `height_mips.rs` (`HeightMips::sample`). Texel i of level k represents the level-0
// sample coordinate i·2^k + (2^k − 1)/2; level 0 is exactly `height_at`.
// ---------------------------------------------------------------------------

fn height_level(s: vec2<f32>, k: u32) -> f32 {
    let scale = f32(1u << k);
    let dims = level_dims(k);
    let max_s = vec2<f32>(f32(dims.x - 1), f32(dims.y - 1));
    let p = clamp((s - vec2<f32>((scale - 1.0) * 0.5)) / scale, vec2<f32>(0.0), max_s);
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let level = i32(k);
    let h00 = textureLoad(height_texture, i, level).r;
    let h10 = textureLoad(height_texture, vec2<i32>(i1.x, i.y), level).r;
    let h01 = textureLoad(height_texture, vec2<i32>(i.x, i1.y), level).r;
    let h11 = textureLoad(height_texture, i1, level).r;
    let a = mix(h00, h10, f.x);
    let b = mix(h01, h11, f.x);
    return mix(a, b, f.y) * terrain.height_scale + terrain.height_offset;
}

// Level-0 height on the mesh's own triangles: each cell split along its (+x, +z) diagonal
// like the terrain grid (`grid_indices`) and the reference tracer's heightfield, where
// `height_level` blends the four corners bilinearly. They part by up to a quarter of the
// cell's twist, which decides a grazing sun's shadow behind a one-texel lip.
fn height_tri0(s: vec2<f32>) -> f32 {
    let dims = level_dims(0u);
    let p = clamp(s, vec2<f32>(0.0), vec2<f32>(f32(dims.x - 1), f32(dims.y - 1)));
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let h00 = textureLoad(height_texture, i, 0).r;
    let h10 = textureLoad(height_texture, vec2<i32>(i1.x, i.y), 0).r;
    let h01 = textureLoad(height_texture, vec2<i32>(i.x, i1.y), 0).r;
    let h11 = textureLoad(height_texture, i1, 0).r;
    var h = h00 + f.x * (h10 - h00) + f.y * (h01 - h00);
    if (f.x + f.y > 1.0) {
        h = h11 + (1.0 - f.x) * (h01 - h11) + (1.0 - f.y) * (h10 - h11);
    }
    return h * terrain.height_scale + terrain.height_offset;
}

// Level-0 height sample `i` in metres (clamped to the grid).
fn height_at0(i: vec2<i32>) -> f32 {
    let c = clamp(i, vec2<i32>(0), level_dims(0u) - vec2<i32>(1));
    return textureLoad(height_texture, c, 0).r * terrain.height_scale + terrain.height_offset;
}

// The heightfield's vertex normal at level-0 sample `i`: central differences, one-sided at
// the edges (the reference tracer's `hf_vertex_normal`).
fn hf_vertex_normal0(i: vec2<i32>) -> vec3<f32> {
    let dims = level_dims(0u);
    let lo = max(i - vec2<i32>(1), vec2<i32>(0));
    let hi = min(i + vec2<i32>(1), dims - vec2<i32>(1));
    let t = texel_m();
    let dx = (height_at0(vec2<i32>(hi.x, i.y)) - height_at0(vec2<i32>(lo.x, i.y))) / (f32(max(hi.x - lo.x, 1)) * t);
    let dz = (height_at0(vec2<i32>(i.x, hi.y)) - height_at0(vec2<i32>(i.x, lo.y))) / (f32(max(hi.y - lo.y, 1)) * t);
    return normalize(vec3<f32>(-dx, 1.0, -dz));
}

// The smooth surface of the level-0 mesh at a point (`hf_terminator_start`).
struct HfSmooth {
    // Where rays leave the surface.
    p: vec3<f32>,
    // The barycentric blend of the corners' vertex normals (unit).
    n: vec3<f32>,
};

// Where a ray toward the sun leaves the level-0 mesh at world `xz`: the point on the mesh's
// own triangle (`height_tri0`) projected onto each corner's tangent plane, the projections
// blended with the barycentrics, kept where it lies above the flat triangle (convex) — the
// smooth surface the vertex normals shade (Hanika 2021, "Hacking the Shadow Terminator";
// the reference tracer starts its rays there too, `terminator_point`) — and that surface's
// interpolated normal.
fn hf_terminator_start(xz: vec2<f32>) -> HfSmooth {
    let dims = level_dims(0u);
    let s = clamp(sample_coord(xz), vec2<f32>(0.0), vec2<f32>(f32(dims.x - 1), f32(dims.y - 1)));
    let i = vec2<i32>(floor(s));
    let f = s - vec2<f32>(i);
    let t = texel_m();
    // Square maps place both axes from the X lane. A rectangular map (ADR-8369) places Z
    // from the depth lane; equal spacing then keeps the terminator on the samples.
    var origin = vec2<f32>(i) * t - vec2<f32>(terrain.half_width);
    if (terrain.depth.y != terrain.world_width) {
        let tz = terrain.depth.y / max(terrain.sample_max.y, 1.0);
        origin = vec2<f32>(f32(i.x) * t - terrain.half_width, f32(i.y) * tz - terrain.depth.x);
    }
    // Corners and barycentrics of the cell's triangle that holds `xz` (the (1,0)–(0,1) split).
    var c0 = i;
    var c1 = i + vec2<i32>(1, 0);
    var c2 = i + vec2<i32>(0, 1);
    var b = vec3<f32>(1.0 - f.x - f.y, f.x, f.y);
    if (f.x + f.y > 1.0) {
        c0 = i + vec2<i32>(1, 1);
        b = vec3<f32>(f.x + f.y - 1.0, 1.0 - f.y, 1.0 - f.x);
    }
    let p0 = vec3<f32>(origin.x + f32(c0.x - i.x) * t, height_at0(c0), origin.y + f32(c0.y - i.y) * t);
    let p1 = vec3<f32>(origin.x + f32(c1.x - i.x) * t, height_at0(c1), origin.y + f32(c1.y - i.y) * t);
    let p2 = vec3<f32>(origin.x + f32(c2.x - i.x) * t, height_at0(c2), origin.y + f32(c2.y - i.y) * t);
    let p = p0 * b.x + p1 * b.y + p2 * b.z;
    let n0 = hf_vertex_normal0(c0);
    let n1 = hf_vertex_normal0(c1);
    let n2 = hf_vertex_normal0(c2);
    let q = (p - min(dot(p - p0, n0), 0.0) * n0) * b.x
        + (p - min(dot(p - p1, n1), 0.0) * n1) * b.y
        + (p - min(dot(p - p2, n2), 0.0) * n2) * b.z;
    var ng = cross(p2 - p0, p1 - p0);
    ng = select(ng, -ng, ng.y < 0.0);
    return HfSmooth(select(p, q, dot(q - p, ng) > 0.0), normalize(n0 * b.x + n1 * b.y + n2 * b.z));
}

// Height at level-0 sample coordinates from fractional pyramid level `m`.
fn height_mip(s: vec2<f32>, m: f32) -> f32 {
    let top = f32(height_levels() - 1u);
    let mm = clamp(m, 0.0, top);
    let k0 = u32(floor(mm));
    let f = mm - f32(k0);
    let a = height_level(s, k0);
    if (f <= 0.0 || f32(k0) >= top) {
        return a;
    }
    return mix(a, height_level(s, k0 + 1u), f);
}

fn sample_coord(xz: vec2<f32>) -> vec2<f32> {
    // Square maps (depth.y == world_width) keep the historical expression, bit for bit.
    // A rectangular map (ADR-8369) maps Z through the depth lane.
    var s = (xz + vec2<f32>(terrain.half_width)) / terrain.world_width * terrain.sample_max;
    if (terrain.depth.y != terrain.world_width) {
        s = (xz + vec2<f32>(terrain.half_width, terrain.depth.x)) / vec2<f32>(terrain.world_width, terrain.depth.y) * terrain.sample_max;
    }
    return s;
}

// Height at world XZ from pyramid level `m`.
fn height_w_mip(xz: vec2<f32>, m: f32) -> f32 {
    return height_mip(sample_coord(xz), m);
}

// Heightfield sample spacing, metres.
fn texel_m() -> f32 {
    return terrain.world_width / max(terrain.sample_max.x, 1.0);
}

// World-space pixel footprint, height included (on a cliff the XZ derivatives alone shrink
// toward zero). Scaled by the frame's texture mip bias (`texture_lod.x`, negative only while
// TSR upscales, 2^bias ≈ render / output): every footprint-driven choice — layer, macro and
// micro-normal LODs, the heightfield normal level, the near-detail fades — then matches
// what the output resolution would pick, and TSR's jittered accumulation resolves it.
// Call in uniform control flow (derivatives).
fn pixel_footprint(world: vec3<f32>) -> f32 {
    let dx = dpdx(world);
    let dy = dpdy(world);
    let bias = exp2(min(frame.texture_lod.x, 0.0));
    // The footprint's two axes, for anisotropic filtering (`aniso_grads`, `fragment_normal`).
    aniso_dx = dx * bias;
    aniso_dy = dy * bias;
    // The sampler path's LOD offset, read from the uniform here, once per pixel (ADR-5943).
    aniso_lod_offset = max(-frame.texture_lod.w, 0.0);
    return max(length(dx), length(dy)) * bias;
}

// The pixel footprint's screen-space axes in world space (metres, mip bias applied), stored by
// `pixel_footprint`. Zero where no footprint was measured (the virtual-texture bake): every
// anisotropic path then falls back to its isotropic sample.
var<private> aniso_dx: vec3<f32>;
var<private> aniso_dy: vec3<f32>;

// The sampler path's LOD offset in mips (`-texture_lod.w` when it is negative;
// `TerrainGpu::set_anisotropy_lod_offset`, ADR-5459), stored by `pixel_footprint`. With an
// anisotropic sampler bound, RADV blends the two mips of a footprint toward the sharper one (an
// isotropic 1.25-texel footprint, mip fraction 0.32, read LOD 0.15 there and 0.64 on NVIDIA; the
// isotropic sampler reads 0.32 on both), and under TAAU its terrain shimmered 1.5x as much per
// unit of detail. Gradients 2^offset longer on both axes keep the tap count and move the filtered
// LOD up by the offset (`aniso_grads` adds it). Exactly 0 elsewhere, on the footprint path
// (w = 1) and where no footprint was measured.
//
// It is a per-pixel value and not a function of the uniform on purpose (ADR-5943). The driver's
// compiler inlines every tap and every projection, and what a site derives from a uniform it
// derives again at every site: as `select(1, exp2(-w), -w > 0)` in `layer_tap` it cost an
// RTX 5090 1.5 % of the terrain forward pass with the offset 0 (0.6 % even with anisotropy
// off, when no such tap runs), and as `max(-w, 0)` in `aniso_grads` still 0.4 %. Read once here
// it costs nothing that can be measured.
var<private> aniso_lod_offset: f32;

// Anisotropic filtering level for this frame (`TerrainBudget::anisotropy`); ≤ 1 is off.
fn aniso_level() -> f32 {
    return frame.texture_lod.z;
}

// Pyramid level whose texel matches a footprint of `size_m` metres (0 at or below a texel).
fn footprint_mip(size_m: f32) -> f32 {
    return clamp(log2(max(size_m, 1e-6) / texel_m()), 0.0, 12.0);
}

// Central-difference normal at one integer pyramid level (four bilinear taps).
fn normal_level(xz: vec2<f32>, k: u32) -> vec3<f32> {
    let level = min(k, height_levels() - 1u);
    let step = texel_m() * f32(1u << level);
    let s = sample_coord(xz);
    let ds = step / texel_m();
    let h_l = height_level(s - vec2<f32>(ds, 0.0), level);
    let h_r = height_level(s + vec2<f32>(ds, 0.0), level);
    let h_d = height_level(s - vec2<f32>(0.0, ds), level);
    let h_u = height_level(s + vec2<f32>(0.0, ds), level);
    return normalize(vec3<f32>(h_l - h_r, 2.0 * step, h_d - h_u));
}

// Central-difference normal of the pyramid surface at level `m`, one level-`m` texel each way.
fn normal_mip(xz: vec2<f32>, m: f32) -> vec3<f32> {
    let step = texel_m() * exp2(m);
    let h_l = height_w_mip(xz - vec2<f32>(step, 0.0), m);
    let h_r = height_w_mip(xz + vec2<f32>(step, 0.0), m);
    let h_d = height_w_mip(xz - vec2<f32>(0.0, step), m);
    let h_u = height_w_mip(xz + vec2<f32>(0.0, step), m);
    return normalize(vec3<f32>(h_l - h_r, 2.0 * step, h_d - h_u));
}

// Sim ground height at world XZ (enabled when material.sim.y > 0).
fn sim_height_w(xz: vec2<f32>) -> f32 {
    let s = (xz + vec2<f32>(material.sim.x)) / max(material.sim.y, 1e-6) * vec2<f32>(
        f32(textureDimensions(sim_height_texture).x - 1),
        f32(textureDimensions(sim_height_texture).y - 1),
    );
    return tex_height_at(sim_height_texture, s) * material.sim.z + material.sim.w;
}

// Value-noise hash (Hoskins-style float hash, public; visual detail only).
fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(0.1031, 0.1030));
    q += dot(q, q.yx + vec2<f32>(33.33));
    return fract((q.x + q.y) * q.x);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p: vec2<f32>) -> f32 {
    return vnoise(p) * 0.6667 + vnoise(p * 2.13 + vec2<f32>(17.1, -9.3)) * 0.3333;
}

fn smoothstep01(t: f32) -> f32 {
    let c = clamp(t, 0.0, 1.0);
    return c * c * (3.0 - 2.0 * c);
}

// One-dimensional value noise (visual detail only).
fn vnoise1(x: f32) -> f32 {
    let i = floor(x);
    let f = x - i;
    let u = f * f * (3.0 - 2.0 * f);
    return mix(hash21(vec2<f32>(i, 7.13)), hash21(vec2<f32>(i + 1.0, 7.13)), u);
}

// One of three bed tones (pale, mid, dark-weathered) picked by the bed's hash.
// The tones scale the layer's own colour and keep its hue: a pale bed is a lighter, slightly
// greyer version of the rock and a dark bed a deeper, slightly redder one (iron staining).
// The round-1b pale tone multiplied green over blue and turned red-brown mesas olive-yellow.
fn bed_tone(bed: f32) -> vec3<f32> {
    let pick = hash21(vec2<f32>(bed, 3.7));
    if (pick < 0.34) {
        return vec3<f32>(1.16, 1.13, 1.12);
    }
    if (pick < 0.72) {
        return vec3<f32>(0.97, 0.97, 0.97);
    }
    return vec3<f32>(0.70, 0.63, 0.61);
}

// Ridged value noise in 0..1: sharp creases like fractured rock.
fn ridged(p: vec2<f32>) -> f32 {
    return 1.0 - abs(fbm(p) * 2.0 - 1.0);
}

// Soft band membership; CPU mirror in `layers.rs::band`.
fn band(v: f32, lo: f32, hi: f32, sharp: f32) -> f32 {
    let span = max(hi - lo, 1e-5);
    let t = clamp((v - lo) / span, 0.0, 1.0);
    let s = clamp(sharp / span, 0.01, 0.5);
    return smoothstep01(t / s) * (1.0 - smoothstep01((t - (1.0 - s)) / s));
}

// Photoreal slope band: like `band`, but a layer whose slope range starts at 0 keeps full
// weight on flat ground (slope cannot go below 0, so a lower ramp there only starved
// perfectly flat fragments of every layer). The legacy path keeps `band` unchanged.
fn slope_band(v: f32, lo: f32, hi: f32, sharp: f32) -> f32 {
    if (lo > 0.0) {
        return band(v, lo, hi, sharp);
    }
    let span = max(hi - lo, 1e-5);
    let t = clamp((v - lo) / span, 0.0, 1.0);
    let s = clamp(sharp / span, 0.01, 0.5);
    return 1.0 - smoothstep01((t - (1.0 - s)) / s);
}

// Photoreal altitude band. A band that reaches the map's lowest or highest ground
// (`layers.count.y/z`) keeps full weight there: with the plain `band`, a layer whose range
// ended at the summit faded out over the top few metres, every layer did the same, and the
// crest line fell through to the fallback layer — thin pale "caps" traced along every ridge
// (switchbacks_mid). Same idea as `slope_band` at slope 0.
fn altitude_band(v: f32, lo: f32, hi: f32, sharp: f32) -> f32 {
    let span = max(hi - lo, 1e-5);
    let t = clamp((v - lo) / span, 0.0, 1.0);
    let s = clamp(sharp / span, 0.01, 0.5);
    var lower = smoothstep01(t / s);
    var upper = 1.0 - smoothstep01((t - (1.0 - s)) / s);
    if (lo <= layers.count.y + 1e-3) {
        lower = 1.0;
    }
    if (hi >= layers.count.z - 1e-3) {
        upper = 1.0;
    }
    return lower * upper;
}

// AgX display transform + the engine's default look. Mirror of `regia_render`'s
// `utils.wgsl` (`tonemap_agx_vec`, `display_look`, `DEFAULT_LOOK_*`); the test
// `terrain_display_transform_mirrors_the_renderer` keeps the copies identical.
// Provenance: Sobotka's AgX, minimal form after B. Wrensch (2023); see docs/provenance.md.
// Output is display-encoded; `display_to_linear` hands an sRGB-encoding target linear values.
// BEGIN display-transform mirror
const DEFAULT_LOOK_CONTRAST: f32 = 1.2;
const DEFAULT_LOOK_SATURATION: f32 = 1.4;

fn agx_sigmoid(x: vec3<f32>) -> vec3<f32> {
    let x2 = x * x;
    let x4 = x2 * x2;
    return 15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x
        - vec3<f32>(0.00232);
}

fn tonemap_agx_vec(color: vec3<f32>) -> vec3<f32> {
    let inset = mat3x3<f32>(
        vec3<f32>(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        vec3<f32>(0.0784335999999992, 0.878468636469772, 0.0784336),
        vec3<f32>(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let outset = mat3x3<f32>(
        vec3<f32>(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        vec3<f32>(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        vec3<f32>(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    let min_ev = -12.47393;
    let max_ev = 4.026069;
    var v = inset * max(color, vec3<f32>(1.0e-10));
    v = clamp(log2(max(v, vec3<f32>(1.0e-10))), vec3<f32>(min_ev), vec3<f32>(max_ev));
    v = (v - vec3<f32>(min_ev)) / (max_ev - min_ev);
    v = agx_sigmoid(v);
    v = outset * v;
    return clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn display_look(display: vec3<f32>, contrast: f32, saturation: f32) -> vec3<f32> {
    let v = clamp(display, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 0.5 * pow(v * 2.0, vec3<f32>(contrast));
    let hi = vec3<f32>(1.0) - 0.5 * pow((vec3<f32>(1.0) - v) * 2.0, vec3<f32>(contrast));
    let curved = select(hi, lo, v < vec3<f32>(0.5));
    let luma = dot(curved, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(vec3<f32>(luma) + (curved - vec3<f32>(luma)) * saturation, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn default_display_transform(color: vec3<f32>) -> vec3<f32> {
    return display_look(tonemap_agx_vec(color), DEFAULT_LOOK_CONTRAST, DEFAULT_LOOK_SATURATION);
}
// END display-transform mirror

fn display_to_linear(display: vec3<f32>) -> vec3<f32> {
    return pow(max(display, vec3<f32>(0.0)), vec3<f32>(2.2));
}

const INV_PI: f32 = 0.3183098862;

// Aerial perspective — the same integral and in-scatter as `regia_render`'s
// `aerial.wgsl` (Wenzel, SIGGRAPH 2006; Henyey–Greenstein 1941). Used when
// `fog_color.a > 0.5` (`FogSettings::sky_inscatter`).
const AERIAL_PI: f32 = 3.14159265359;

fn luma3(color: vec3<f32>) -> f32 {
    return dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn sky_color_dir(dir: vec3<f32>) -> vec3<f32> {
    let t = dir.y;
    let horizon = vec3<f32>(0.78, 0.85, 0.95);
    let zenith = vec3<f32>(0.20, 0.40, 0.92);
    let ground = vec3<f32>(0.34, 0.33, 0.31);
    if (t > 0.0) {
        let f = 1.0 - exp(-t * 3.0);
        return mix(horizon, zenith, f);
    }
    let f = 1.0 - exp(t * 2.0);
    return mix(horizon, ground, f);
}

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = max(1.0 + g2 - 2.0 * g * cos_theta, 1e-4);
    return (1.0 - g2) / (4.0 * AERIAL_PI * denom * sqrt(denom));
}

fn haze_inscatter(view_dir: vec3<f32>, to_sun: vec3<f32>, sun_irradiance: vec3<f32>, sky_scale: f32) -> vec3<f32> {
    var flat_dir = vec3<f32>(view_dir.x, 0.0, view_dir.z);
    if (dot(flat_dir, flat_dir) < 1e-6) {
        flat_dir = vec3<f32>(1.0, 0.0, 0.0);
    }
    let horizon = normalize(normalize(flat_dir) + vec3<f32>(0.0, 0.06, 0.0));
    let sky = sky_color_dir(horizon) * sky_scale;
    let glow = sun_irradiance * 0.02 * henyey_greenstein(dot(view_dir, to_sun), 0.6);
    return sky + glow;
}

fn height_fog_optical_depth(camera: vec3<f32>, world: vec3<f32>, params: vec4<f32>) -> f32 {
    let distance = length(world - camera);
    let k = max(params.y, 0.0);
    let start = params.x * exp(-k * (camera.y - params.z));
    let dh = world.y - camera.y;
    var integral = 1.0;
    if (abs(k * dh) > 1e-4) {
        integral = (1.0 - exp(-k * dh)) / (k * dh);
    }
    return max(start * distance * integral, 0.0);
}

fn apply_aerial(
    scene_rgb: vec3<f32>,
    world: vec3<f32>,
    camera: vec3<f32>,
    params: vec4<f32>,
    tint: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
) -> vec3<f32> {
    if (params.w < 0.5 || params.x <= 0.0) {
        return scene_rgb;
    }
    let view = world - camera;
    let len = length(view);
    if (len <= 0.0) {
        return scene_rgb;
    }
    let transmittance = exp(-height_fog_optical_depth(camera, world, params));
    let inscatter = haze_inscatter(view / len, to_sun, sun_irradiance, sky_scale) * tint;
    return mix(inscatter, scene_rgb, transmittance);
}

// The analytic haze under a cloud layer (ADR-5944), as `regia_render`'s `aerial.wgsl`
// (`haze_inscatter_under_clouds`, `apply_aerial_under_clouds`): the air on the view ray keeps
// `sun_lit` of its sun, so the glow falls with it and the horizon sky term down to its floor
// (the sky's share of the clear ground light).
const HAZE_SKY_FLOOR: f32 = 0.12;

fn haze_inscatter_under_clouds(
    view_dir: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
    sun_lit: f32,
) -> vec3<f32> {
    var flat_dir = vec3<f32>(view_dir.x, 0.0, view_dir.z);
    if (dot(flat_dir, flat_dir) < 1e-6) {
        flat_dir = vec3<f32>(1.0, 0.0, 0.0);
    }
    let lit = clamp(sun_lit, 0.0, 1.0);
    let horizon = normalize(normalize(flat_dir) + vec3<f32>(0.0, 0.06, 0.0));
    let sky = sky_color_dir(horizon) * sky_scale * mix(HAZE_SKY_FLOOR, 1.0, lit);
    let glow = sun_irradiance * 0.02 * henyey_greenstein(dot(view_dir, to_sun), 0.6) * lit;
    return sky + glow;
}

fn apply_aerial_under_clouds(
    scene_rgb: vec3<f32>,
    world: vec3<f32>,
    camera: vec3<f32>,
    params: vec4<f32>,
    tint: vec3<f32>,
    to_sun: vec3<f32>,
    sun_irradiance: vec3<f32>,
    sky_scale: f32,
    sun_lit: f32,
) -> vec3<f32> {
    if (params.w < 0.5 || params.x <= 0.0) {
        return scene_rgb;
    }
    let view = world - camera;
    let len = length(view);
    if (len <= 0.0) {
        return scene_rgb;
    }
    let transmittance = exp(-height_fog_optical_depth(camera, world, params));
    let inscatter = haze_inscatter_under_clouds(view / len, to_sun, sun_irradiance, sky_scale, sun_lit) * tint;
    return mix(inscatter, scene_rgb, transmittance);
}

// The share of its sun the haze on this pixel's view ray keeps under the cloud layer
// (ADR-5944). The shared colour entries set it (`shared_haze_sun`); 1, the default, keeps
// `apply_aerial` as it was (the stand-alone entry has no cloud map).
var<private> haze_sun_lit: f32 = 1.0;

// Exponential height fog — mirrors `regia_render::fog` (Hoffman & Preetham GDC 2002).
fn apply_height_fog(scene_rgb: vec3<f32>, world: vec3<f32>) -> vec3<f32> {
    let params = frame.fog_params;
    if (params.w < 0.5 || params.x <= 0.0) {
        return scene_rgb;
    }
    let distance = length(world - frame.camera_pos_time.xyz);
    let height_density = params.x * exp(-params.y * (world.y - params.z));
    let t = 1.0 - exp(-max(distance, 0.0) * max(height_density, 0.0));
    return mix(scene_rgb, frame.fog_color.rgb, t);
}

// 3×3 percentage-closer filter. Y is flipped the same way as `regia_render`'s
// `shadow_uv_from_clip`: wgpu samples v = 0 at the top of the depth texture.
// CompareFunction::LessEqual → 1 when the receiver is in front of the stored depth.
fn sample_sun_shadow(world: vec3<f32>, n: vec3<f32>) -> f32 {
    if (frame.shadow_params.x < 0.5) {
        return 1.0;
    }
    let clip = frame.light_view_proj * vec4<f32>(world, 1.0);
    let ndc = clip.xyz / max(clip.w, 1e-6);
    let uv = vec3<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5), ndc.z);
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || uv.z < 0.0 || uv.z > 1.0) {
        return 1.0;
    }
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    let slope = 1.0 - clamp(dot(n, sun_dir), 0.0, 1.0);
    let bias = frame.shadow_params.y * (1.0 + slope * 3.0);
    let compare_z = clamp(uv.z - bias, 0.0, 1.0);
    let texel = 1.0 / max(frame.shadow_params.w, 1.0);
    // 16 bilinear comparison taps on a Vogel disc of radius 1.75 texels: taps sit less
    // than a texel apart, so the edge is a smooth ramp instead of the 3×3 grid's
    // visible texel stair-steps on long terrain shadows. The pattern is fixed (no
    // per-pixel rotation: this path has no temporal accumulation to hide noise).
    var shadow = 0.0;
    for (var i: u32 = 0u; i < 16u; i++) {
        let r = sqrt((f32(i) + 0.5) / 16.0) * 1.75;
        let a = f32(i) * 2.399963;
        let offset = vec2<f32>(cos(a), sin(a)) * r * texel;
        shadow += textureSampleCompareLevel(shadow_map, shadow_samp, uv.xy + offset, compare_z);
    }
    return shadow / 16.0;
}

fn engine_vp(index: u32) -> mat4x4<f32> {
    switch index {
        case 0u: { return engine_csm.vp0; }
        case 1u: { return engine_csm.vp1; }
        case 2u: { return engine_csm.vp2; }
        default: { return engine_csm.vp3; }
    }
}

fn engine_bias(index: u32) -> vec4<f32> {
    switch index {
        case 0u: { return engine_csm.bias0; }
        case 1u: { return engine_csm.bias1; }
        case 2u: { return engine_csm.bias2; }
        default: { return engine_csm.bias3; }
    }
}

fn sample_csm_tex(index: u32, uv: vec2<f32>, compare_z: f32) -> f32 {
    if (index == 0u) {
        return textureSampleCompareLevel(shadow_map, shadow_samp, uv, compare_z);
    }
    if (index == 1u) {
        return textureSampleCompareLevel(csm_1, shadow_samp, uv, compare_z);
    }
    if (index == 2u) {
        return textureSampleCompareLevel(csm_2, shadow_samp, uv, compare_z);
    }
    return textureSampleCompareLevel(csm_3, shadow_samp, uv, compare_z);
}

// Depth slope of the receiver plane (normal `n`) in the cascade's shadow space: the change of
// the compare depth per unit of shadow UV along u and v (receiver-plane depth bias, Isidoro
// 2006, "Shadow Mapping: GPU-based Tips and Techniques"). The cascade is orthographic, so two
// tangents of the plane map linearly. Zero when the plane is edge-on to the light.
fn receiver_plane_slope(vp: mat4x4<f32>, n: vec3<f32>) -> vec2<f32> {
    let helper = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), abs(n.x) > 0.9);
    let t1 = normalize(cross(n, helper));
    let t2 = cross(n, t1);
    let a = (vp * vec4<f32>(t1, 0.0)).xyz;
    let b = (vp * vec4<f32>(t2, 0.0)).xyz;
    // NDC → UV: u = x/2 + 1/2, v = 1/2 − y/2.
    let au = vec3<f32>(a.x * 0.5, -a.y * 0.5, a.z);
    let bu = vec3<f32>(b.x * 0.5, -b.y * 0.5, b.z);
    let det = au.x * bu.y - au.y * bu.x;
    if (abs(det) < 1e-12) {
        return vec2<f32>(0.0);
    }
    return vec2<f32>(au.z * bu.y - au.y * bu.z, au.x * bu.z - au.z * bu.x) / det;
}

// One engine cascade. Cascade 0 (≤ 16 m, where characters stand) compares every PCF tap's
// depth against the receiver plane (`receiver_plane_slope`), so the disc no longer needs a
// constant bias as large as the plane's depth change across it: the grazing-lit slope beside
// the camera on bore_twelve_close stays lit, and a character's shadow starts at its feet
// (`lookdev dyn_light`: the old 2 + 4·slope + 1.75·tan θ texels plus a 1.5-texel normal
// offset put the shadow of a mannequin 0.35–0.5 m away from its feet under a 24° sun, with
// the ground at its feet in full sun). What stays: 1.5 texels for the cascade's own terrain
// (its CDLOD selection may be coarser than the camera's), the plane's depth change across
// the 2×2 bilinear compare footprint, and a 0.5-texel normal offset.
//
// Beyond cascade 0 the receiver-plane compare is not needed and was a measurable forward-pass
// regression (gpu-farm4: +0.12–0.13 ms on Mars across the far cascades that cover most
// terrain pixels), so those cascades keep the pre-dyn-light normal offset (1.5 texels) and
// slope-scaled constant bias. The terrain's self-shadow at grazing angles still comes from
// the heightfield march (`heightfield_sun_occlusion`). The compare is a Vogel disc of
// `meta.w` taps (Fernando 2005 / the mesh PCF), capped at 16.
fn sample_engine_cascade(index: u32, world: vec3<f32>, n: vec3<f32>, to_sun: vec3<f32>) -> f32 {
    let bias = engine_bias(index);
    let texel_world = max(bias.x, 1e-6);
    let vp = engine_vp(index);
    let texel = 1.0 / max(bias.z, 1.0);
    var taps = u32(engine_csm.params.w + 0.5);
    if (taps < 4u) {
        taps = 16u;
    }
    if (taps > 16u) {
        taps = 16u;
    }
    if (index != 0u) {
        // Pre-dyn-light path: a 1.5-texel normal offset and a slope-scaled constant bias.
        let offset_world = world + n * texel_world * 1.5;
        let clip = vp * vec4<f32>(offset_world, 1.0);
        let ndc = clip.xyz / max(clip.w, 1e-6);
        let uv = vec3<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5), ndc.z);
        if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || uv.z < 0.0 || uv.z > 1.0) {
            return 1.0;
        }
        let no_l = max(dot(n, to_sun), 0.0);
        let slope = 1.0 - clamp(no_l, 0.0, 1.0);
        let tan_t = min(sqrt(max(1.0 - no_l * no_l, 0.0)) / max(no_l, 1e-3), 12.0);
        let depth_bias = (2.0 + 4.0 * slope + 1.75 * tan_t) * texel_world * max(bias.y, 0.0);
        let compare_z = clamp(uv.z - depth_bias, 0.0, 1.0);
        var shadow = 0.0;
        for (var i: u32 = 0u; i < 16u; i++) {
            if (i >= taps) {
                break;
            }
            let r = sqrt((f32(i) + 0.5) / f32(taps)) * 1.75;
            let a = f32(i) * 2.399963;
            let offset = vec2<f32>(cos(a), sin(a)) * r * texel;
            shadow += sample_csm_tex(index, uv.xy + offset, compare_z);
        }
        return shadow / f32(taps);
    }
    // Cascade 0: the receiver-plane compare (unchanged).
    let offset_world = world + n * texel_world * 0.5;
    let clip = vp * vec4<f32>(offset_world, 1.0);
    let ndc = clip.xyz / max(clip.w, 1e-6);
    let uv = vec3<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5), ndc.z);
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || uv.z < 0.0 || uv.z > 1.0) {
        return 1.0;
    }
    // Normalised depth of one texel_world along the light, and the plane's depth slope
    // capped at tan θ = 12 (an edge-on plane would otherwise push taps across metres).
    let depth_texel = texel_world * max(bias.y, 0.0);
    var slope = receiver_plane_slope(vp, n);
    let max_slope = 12.0 * depth_texel / texel;
    let slope_len = length(slope);
    if (slope_len > max_slope) {
        slope = slope * (max_slope / slope_len);
    }
    let footprint = (abs(slope.x) + abs(slope.y)) * texel;
    let compare_z = uv.z - 1.5 * depth_texel - footprint;
    var shadow = 0.0;
    for (var i: u32 = 0u; i < 16u; i++) {
        if (i >= taps) {
            break;
        }
        let r = sqrt((f32(i) + 0.5) / f32(taps)) * 1.75;
        let a = f32(i) * 2.399963;
        let offset = vec2<f32>(cos(a), sin(a)) * r * texel;
        let z = clamp(compare_z + dot(slope, offset), 0.0, 1.0);
        shadow += sample_csm_tex(index, uv.xy + offset, z);
    }
    return shadow / f32(taps);
}

// One bilinear compare tap of the engine cascades at `p` (no normal offset; a small fixed
// depth bias), 1 outside every cascade.
fn engine_csm_point(p: vec3<f32>) -> f32 {
    let count = min(u32(engine_csm.params.x + 0.5), 4u);
    if (count == 0u) {
        return 1.0;
    }
    let view_z = length(p - frame.camera_pos_time.xyz);
    var cascade = 0u;
    for (var i: u32 = 1u; i < 4u; i++) {
        if (i >= count) {
            break;
        }
        if (view_z > engine_csm.split[i - 1u]) {
            cascade = i;
        }
    }
    let bias = engine_bias(cascade);
    let clip = engine_vp(cascade) * vec4<f32>(p, 1.0);
    let ndc = clip.xyz / max(clip.w, 1e-6);
    let uv = vec3<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5), ndc.z);
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || uv.z < 0.0 || uv.z > 1.0) {
        return 1.0;
    }
    return sample_csm_tex(cascade, uv.xy, clamp(uv.z - 4.0 * max(bias.x, 1e-6) * max(bias.y, 0.0), 0.0, 1.0));
}

// Debug: hard heightfield sun visibility over ~2 km (x) and the distance of the first
// occluder (y, metres; 0 when none): 64 steps growing ×1.12 from 0.5 m, level-0 bilinear.
fn far_heightfield_sun(p: vec3<f32>, to_sun: vec3<f32>) -> vec2<f32> {
    let horiz = max(length(to_sun.xz), 1e-4);
    let dir = to_sun.xz / horiz;
    let rise = to_sun.y / horiz;
    var d = 0.5;
    for (var i = 0; i < 64; i++) {
        let q = p.xz + dir * d;
        let h = height_w(q);
        if (h > p.y + 0.05 + rise * d) {
            return vec2<f32>(0.0, d);
        }
        d *= 1.12;
    }
    return vec2<f32>(1.0, 0.0);
}

// Debug: how far in front of the receiver the cascade's stored depth is (metres along the
// light; > 0 = something occludes), at the nearest texel, no bias, and the cascade used.
fn engine_csm_occluder_m(world: vec3<f32>) -> vec2<f32> {
    let count = min(u32(engine_csm.params.x + 0.5), 4u);
    if (count == 0u) {
        return vec2<f32>(0.0, -1.0);
    }
    let view_z = length(world - frame.camera_pos_time.xyz);
    var cascade = 0u;
    for (var i: u32 = 1u; i < 4u; i++) {
        if (i >= count) {
            break;
        }
        if (view_z > engine_csm.split[i - 1u]) {
            cascade = i;
        }
    }
    let clip = engine_vp(cascade) * vec4<f32>(world, 1.0);
    let ndc = clip.xyz / max(clip.w, 1e-6);
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
    let res = max(engine_bias(cascade).z, 1.0);
    let texel = clamp(vec2<i32>(uv * res), vec2<i32>(0), vec2<i32>(i32(res) - 1));
    var stored = 1.0;
    if (cascade == 0u) {
        stored = textureLoad(shadow_map, texel, 0);
    } else if (cascade == 1u) {
        stored = textureLoad(csm_1, texel, 0);
    } else if (cascade == 2u) {
        stored = textureLoad(csm_2, texel, 0);
    } else {
        stored = textureLoad(csm_3, texel, 0);
    }
    let per_m = max(engine_bias(cascade).y, 1e-9);
    return vec2<f32>((ndc.z - stored) / per_m, f32(cascade));
}

// Blend across the cascade seam the same way the mesh `sample_csm` does.
fn sample_engine_csm(world: vec3<f32>, n: vec3<f32>) -> f32 {
    let count = min(u32(engine_csm.params.x + 0.5), 4u);
    if (count == 0u) {
        return 1.0;
    }
    let to_sun = normalize(frame.sun_dir_wetness.xyz);
    let view_z = length(world - frame.camera_pos_time.xyz);
    var cascade = 0u;
    for (var i: u32 = 1u; i < 4u; i++) {
        if (i >= count) {
            break;
        }
        if (view_z > engine_csm.split[i - 1u]) {
            cascade = i;
        }
    }
    let hi = engine_csm.split[cascade];
    var lo = 0.0;
    if (cascade > 0u) {
        lo = engine_csm.split[cascade - 1u];
    }
    let band = max((hi - lo) * 0.15, 1e-3);
    let edge = clamp((view_z - (hi - band)) / band, 0.0, 1.0);
    // Past the shadow distance the last cascade has faded to lit (`mix(s0, 1.0, 1.0)`): skip
    // its 16-tap filter. Terrain beyond 4 km on a 12 km vista paid it on every pixel the
    // cascade's box still covered (ADR-1236).
    if (edge >= 1.0 && cascade + 1u >= count) {
        return 1.0;
    }
    let s0 = sample_engine_cascade(cascade, world, n, to_sun);
    if (edge <= 0.0) {
        return s0;
    }
    if (cascade + 1u < count) {
        return mix(s0, sample_engine_cascade(cascade + 1u, world, n, to_sun), edge);
    }
    return mix(s0, 1.0, edge);
}

// ---------------------------------------------------------------------------
// Stochastic anti-tiling on a triangle grid (Heitz & Neyret 2018, Deliot & Heitz 2019).
// The plane is cut into equilateral triangles of edge `anti_tiling_cell` metres; every
// lattice vertex carries a hashed UV offset and rotation, and a fragment blends the three
// taps of its triangle with barycentric weights. A vertex's weight is zero on the edge
// opposite it, so the blend is continuous everywhere: no seam on any cell edge (the old
// two-square-grid version cut a hard seam along every cell edge — the vertical seam in
// round 3's near shot). The weights are sharpened (power 3, renormalised) so the blend
// keeps texture contrast in triangle centres (Mikkelsen 2022, hex-tiling contrast).
// Rotated taps rotate their tangent-space normals back (`unrotate`).
// That continuity needs a vertex to hash to the same bits from each of its triangles: see
// the lattice ids in `stochastic_taps`.
// ---------------------------------------------------------------------------

struct Stoch {
    // Sharpened barycentric weights (sum 1).
    w: vec3<f32>,
    // Per-vertex rotated world XZ (metres); the layer UV is `rw * tiling + o`.
    rw1: vec2<f32>,
    rw2: vec2<f32>,
    rw3: vec2<f32>,
    // Per-vertex UV offsets (texture repeats).
    o1: vec2<f32>,
    o2: vec2<f32>,
    o3: vec2<f32>,
    // Per-vertex rotation (cos, sin).
    cs1: vec2<f32>,
    cs2: vec2<f32>,
    cs3: vec2<f32>,
}

fn vertex_rotation(vid: vec2<f32>) -> vec2<f32> {
    let a = hash21(vid + vec2<f32>(31.7, -12.9)) * 6.2831853;
    return vec2<f32>(cos(a), sin(a));
}

fn vertex_offset(vid: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(hash21(vid + vec2<f32>(7.31, -7.31)), hash21(vid * 1.31 + vec2<f32>(-19.17, 19.17)));
}

fn rotate2(p: vec2<f32>, cs: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(cs.x * p.x - cs.y * p.y, cs.y * p.x + cs.x * p.y);
}

// Inverse rotation: maps a tangent-space XY sampled through `rotate2` back to world XZ.
fn unrotate(n: vec2<f32>, cs: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(cs.x * n.x + cs.y * n.y, -cs.y * n.x + cs.x * n.y);
}

// Weight under which a stochastic tap is dropped (`stochastic_taps`).
const STOCH_TAP_CUT: f32 = 0.05;
// Share of the layer blend under which a layer is dropped (`surface_layers`).
const LAYER_CUT: f32 = 0.03;
// Plane weight under which a triplanar plane is dropped (`surface_layers`).
const TRIPLANAR_CUT: f32 = 0.05;

fn stochastic_taps(world: vec2<f32>, cell_m: f32) -> Stoch {
    let one = vec2<f32>(1.0, 0.0);
    let zero = vec2<f32>(0.0);
    if (cell_m <= 0.0) {
        return Stoch(vec3<f32>(1.0, 0.0, 0.0), world, world, world, zero, zero, zero, one, one, one);
    }
    // Equilateral lattice basis e1 = (1, 0), e2 = (1/2, √3/2); skewed = B⁻¹ p.
    let p = world / cell_m;
    let s = vec2<f32>(p.x - 0.57735027 * p.y, 1.15470054 * p.y);
    let cell = floor(s);
    let f = s - cell;
    // Lattice vertex ids are integers until they are hashed (ADR-5792). A vertex is shared by six
    // triangles, which reach it as `base`, `base + (1, 0)`, `base + (0, 1)` or `base + (1, 1)`.
    // With float ids the RTX 5080 and 5090 did not give a vertex the same hash from all of its
    // triangles (at every tier, 130 m from the origin; lavapipe did): the vertex tapped
    // different texels from two triangles and the blend cut a hard step along their shared
    // edge. A float sum taken in another order (the offset folded into the hash's own constant,
    // `(base + 1.0) + 31.7` → `base + 32.7`) is one rounding step apart, and the hash and the
    // rotation about the world origin turn that step into another texture position. Integer
    // sums are exact in any order, and the one conversion gives every triangle the same float.
    let base = vec2<i32>(cell);
    var bary = vec3<f32>(0.0);
    var i1 = base;
    if (f.x + f.y < 1.0) {
        bary = vec3<f32>(1.0 - f.x - f.y, f.x, f.y);
    } else {
        bary = vec3<f32>(f.x + f.y - 1.0, 1.0 - f.y, 1.0 - f.x);
        i1 = base + vec2<i32>(1, 1);
    }
    let v1 = vec2<f32>(i1);
    let v2 = vec2<f32>(base + vec2<i32>(1, 0));
    let v3 = vec2<f32>(base + vec2<i32>(0, 1));
    let sharp = bary * bary * bary;
    let w0 = sharp / max(sharp.x + sharp.y + sharp.z, 1e-6);
    // Taps under `STOCH_TAP_CUT` of the blend are dropped and the rest renormalised: the
    // weight goes to 0 continuously (no seam), and with the cubic sharpening only ~1.8 of
    // the three taps are left on average (the largest is always ≥ ⅓). `tri_sample` and
    // `tri_normal` skip zero-weight taps.
    let wc = max(w0 - vec3<f32>(STOCH_TAP_CUT), vec3<f32>(0.0));
    let w = wc / (wc.x + wc.y + wc.z);
    let cs1 = vertex_rotation(v1);
    let cs2 = vertex_rotation(v2);
    let cs3 = vertex_rotation(v3);
    return Stoch(
        w,
        rotate2(world, cs1),
        rotate2(world, cs2),
        rotate2(world, cs3),
        vertex_offset(v1),
        vertex_offset(v2),
        vertex_offset(v3),
        cs1,
        cs2,
        cs3,
    );
}

// ---------------------------------------------------------------------------
// Ground look (ADR-1408, `ground_look.rs`). Off (all rows zero) unless a host sets it.
//
// Every line of it sits between `@ground-look-begin` / `@ground-look-end` markers (ADR-1452):
// the shared colour pass compiles `without_ground_look` (the markers' lines removed, each
// `@ground-look-off` comment made code) unless a ground-look row is non-zero. Compiled in but
// never taken, the look cost the terrain forward pass 0.3-0.45 ms on the RTX 5090 (DO27
// switchbacks; most of it from the one `max` with the look's local wetness: this shader sits
// at a compiler cliff). Keep any new ground-look code inside markers, and each marked call
// site a test that is false while the rows are zero (`tests/ground_look_variant.rs`).
// ---------------------------------------------------------------------------
// @ground-look-begin

// Remix: a second tap of the layer at `material.ground.w` times its tiling, rotated, mixed in
// by a 5 m noise mask, so neighbouring texture repeats differ in their mid-scale structure
// (multi-scale detail blending; the stochastic lattice hides only the repeat itself).
fn ground_remix(tex: vec4<f32>, xz: vec2<f32>, tiling: f32, layer: i32, lod: f32, fade: f32) -> vec4<f32> {
    let rs = clamp(material.ground.w, 0.1, 0.9);
    let angle = 1.1 + 0.83 * f32(layer);
    let cs = vec2<f32>(cos(angle), sin(angle));
    let uv2 = rotate2(xz, cs) * (tiling * rs) + vec2<f32>(0.37, 0.71) * f32(layer + 1);
    // Texels `1 / rs` times larger: the footprint's level drops by log2(1 / rs).
    let lod2 = max(lod + log2(rs), 0.0);
    let t2 = layer_tap(layer_albedo, uv2, layer, lod2, aniso_grads_top(lod2), cs);
    let m = smoothstep(0.3, 0.7, fbm(xz / 5.3 + vec2<f32>(f32(layer) * 7.1, 2.3)));
    return mix(tex, t2, clamp(material.ground.z, 0.0, 1.0) * m * fade);
}

// How strongly normal `n` faces away from the sun along `to_sun`, times the slope's
// steepness (0 on flat ground or under an overhead sun). Matches `ground_look::shade_facing`.
fn shade_facing(n: vec3<f32>, to_sun: vec3<f32>) -> f32 {
    let horiz = length(to_sun.xz);
    if (horiz < 1e-4) {
        return 0.0;
    }
    return -dot(n.xz, to_sun.xz) / horiz;
}

struct GroundLook {
    albedo: vec3<f32>,
    roughness: f32,
    // Local wetness 0..1 (melt fringe, mud): darker and glossier through the frame wetness path.
    wet: f32,
};

// Variation, thin patchy snow with a wet melting fringe, and wet mud on the photoreal surface.
fn ground_look(p: vec3<f32>, n: vec3<f32>, albedo_in: vec3<f32>, rough_in: f32) -> GroundLook {
    var albedo = albedo_in;
    var rough = rough_in;
    var wet = 0.0;
    let xz = p.xz;
    let var_amt = clamp(material.ground.x, 0.0, 2.0);
    if (var_amt > 0.0) {
        // Brightness over three octaves of world noise (about 0.12, 0.4 and 1 × the patch
        // size) and a dry (warm, light) / damp (cool, dark) tint over a fourth.
        let s = max(material.ground.y, 4.0);
        let n1 = fbm(xz / (s * 0.12) + vec2<f32>(3.1, 7.7));
        let n2 = fbm(xz / (s * 0.4) + vec2<f32>(-11.3, 2.9));
        let n3 = fbm(xz / s + vec2<f32>(5.3, -13.1));
        let v = (n1 - 0.5) * 0.6 + (n2 - 0.5) * 0.8 + (n3 - 0.5);
        let t = (fbm(xz / (s * 0.7) + vec2<f32>(23.0, 41.0)) - 0.5) * 2.0;
        let tint = vec3<f32>(1.0 + 0.07 * t, 1.0 + 0.02 * t, 1.0 - 0.08 * t);
        albedo = albedo * max(1.0 + var_amt * 0.5 * v, 0.3) * mix(vec3<f32>(1.0), tint, min(var_amt, 1.0));
    }
    if (material.winter.x > 0.0) {
        // Snow amount: the snow line ramp, the aspect away from the sun, shed off steep faces
        // (`ground_look::snow_amount`); patches from two noise octaves around a threshold.
        let to_sun = normalize(frame.sun_dir_wetness.xyz);
        let above_line = clamp((p.y - material.winter.y) / 80.0 + 0.5, 0.0, 1.0);
        let aspect = material.winter.z * shade_facing(n, to_sun) * 2.0;
        let shed = clamp((n.y - 0.55) / 0.25, 0.0, 1.0);
        let amount = max(material.winter.x * (0.25 + 0.75 * above_line + aspect) * shed, 0.0);
        let ps = max(material.winter2.x, 0.5);
        let patch_n = fbm(xz / ps + vec2<f32>(-3.7, 9.1)) * 0.6 + fbm(xz / (ps * 3.7) + vec2<f32>(13.3, -1.9)) * 0.4;
        let fine_n = fbm(xz / (ps * 0.13) + vec2<f32>(1.3, 4.4));
        let snow_key = amount + (patch_n - 0.5) * 1.1 + (fine_n - 0.5) * 0.25 - 0.5;
        let cover = smoothstep(0.0, 0.12, snow_key);
        // Thin snow lets the ground show through near the patch edge; old spring snow is
        // about 0.7 (Warren 1982), slightly blue.
        let deep = smoothstep(0.0, 0.35, snow_key);
        let snow_col = vec3<f32>(0.70, 0.73, 0.78);
        let snow = mix(mix(albedo, snow_col, 0.6), snow_col, deep);
        albedo = mix(albedo, snow, cover);
        rough = mix(rough, 0.7, cover);
        // Slushy, greyer snow at the edge, and a wet dark fringe of melt water around it.
        let melt = clamp(material.winter.w, 0.0, 1.0);
        albedo = mix(albedo, albedo * vec3<f32>(0.86, 0.88, 0.9), cover * (1.0 - deep) * melt);
        let width = 0.06 + 0.22 * melt;
        let fringe = smoothstep(-width, -0.3 * width, snow_key) * (1.0 - cover) * melt;
        albedo = albedo * (1.0 - 0.3 * fringe);
        wet = max(wet, fringe);
    }
    let mud = clamp(material.winter2.y, 0.0, 1.0);
    if (mud > 0.0) {
        // Wet mud: dark (soil-coloured) level ground in noise patches (Lekner & Dorf 1988: a
        // water film darkens rough ground and adds a sheen).
        let soil = 1.0 - smoothstep(0.05, 0.16, luma3(albedo));
        let level = smoothstep(0.85, 0.97, n.y);
        let pud = smoothstep(0.35, 0.65, fbm(xz / 6.0 + vec2<f32>(31.0, -17.0)));
        let m = mud * soil * level * pud;
        albedo = albedo * (1.0 - 0.25 * m);
        wet = max(wet, m);
    }
    return GroundLook(albedo, rough, wet);
}

// Hoarfrost on ground the sun does not reach (`sun_lit` 0..1): a pale, desaturated, faintly
// blue film on sky-facing ground, in small noise patches.
fn hoarfrost(colour: vec3<f32>, p: vec3<f32>, n: vec3<f32>, sun_lit: f32) -> vec3<f32> {
    let k = clamp(material.winter2.z, 0.0, 1.0) * (1.0 - sun_lit) * smoothstep(0.6, 0.95, n.y)
        * (0.6 + 0.4 * fbm(p.xz * 2.3 + vec2<f32>(-5.1, 8.3)));
    let pale = vec3<f32>(luma3(colour)) * 1.25 + vec3<f32>(0.05, 0.055, 0.065);
    return mix(colour, pale, 0.45 * k);
}
// @ground-look-end

// Anti-tiling cell for one layer, metres. The stochastic lattice only hides repetition
// when a lattice cell is no larger than about one texture repeat: the old single
// world-space cell (4 m by default) held four to five repeats of a 1.1 /m layer, so at
// 1 m every repeat inside a cell was the same tile — the regular wave pattern of
// switchbacks_close. `cell_m <= 0` keeps anti-tiling off.
fn layer_cell(cell_m: f32, tiling: f32) -> f32 {
    if (cell_m <= 0.0 || ab_off(90.0)) {
        return 0.0;
    }
    return min(cell_m, 0.85 / max(tiling, 1e-3));
}

// Blended stochastic sample of one layer of a texture array.
// Zero-weight taps (`STOCH_TAP_CUT`) are not fetched.
fn tri_sample(tex: texture_2d_array<f32>, st: Stoch, tiling: f32, layer: i32, lod: f32, g: Grad2) -> vec4<f32> {
    var sum = vec4<f32>(0.0);
    if (st.w.x > 0.0) {
        sum += layer_tap(tex, st.rw1 * tiling + st.o1, layer, lod, g, st.cs1) * st.w.x;
    }
    if (st.w.y > 0.0) {
        sum += layer_tap(tex, st.rw2 * tiling + st.o2, layer, lod, g, st.cs2) * st.w.y;
    }
    if (st.w.z > 0.0) {
        sum += layer_tap(tex, st.rw3 * tiling + st.o3, layer, lod, g, st.cs3) * st.w.z;
    }
    return sum;
}

// Blended stochastic micro normal as a world-XZ slope vector (tangent XY rotated back).
fn tri_normal(st: Stoch, tiling: f32, layer: i32, lod: f32, g: Grad2) -> vec2<f32> {
    var sum = vec2<f32>(0.0);
    if (st.w.x > 0.0) {
        let a = layer_tap(layer_normal, st.rw1 * tiling + st.o1, layer, lod, g, st.cs1).rg * 2.0 - vec2<f32>(1.0);
        sum += unrotate(a, st.cs1) * st.w.x;
    }
    if (st.w.y > 0.0) {
        let b = layer_tap(layer_normal, st.rw2 * tiling + st.o2, layer, lod, g, st.cs2).rg * 2.0 - vec2<f32>(1.0);
        sum += unrotate(b, st.cs2) * st.w.y;
    }
    if (st.w.z > 0.0) {
        let c = layer_tap(layer_normal, st.rw3 * tiling + st.o3, layer, lod, g, st.cs3).rg * 2.0 - vec2<f32>(1.0);
        sum += unrotate(c, st.cs3) * st.w.z;
    }
    return sum;
}

// Explicit LOD from the screen-space footprint (metres per pixel / metres per texel). The
// footprint already carries the frame's texture mip bias (`pixel_footprint`).
fn footprint_lod(footprint_m: f32, texel_m: f32) -> f32 {
    return clamp(log2(max(footprint_m, 1e-6) / max(texel_m, 1e-9)), 0.0, 12.0);
}

// Anisotropic filtering (ADR-192). Every layer, triplanar and macro tap picks an explicit
// isotropic LOD from the footprint's long axis (`footprint_lod`). With anisotropy on, the tap
// instead goes through `textureSampleGrad` with gradients along the footprint's two real axes,
// scaled so the long one is exactly that LOD's texel size: the hardware then filters at the
// short axis' level with up to `aniso_level` taps along the long one, and every tuned LOD
// choice (the 3D footprint on slopes, the macro stretch, the mip bias) is kept. The gradients
// come from the world-space footprint, not from the UVs, so tile wraps, stochastic cells
// and layer boundaries have no derivative discontinuities.
//
// `x`, `y`: the gradients in level-0 texels along the projection's own axes (rotate them for
// a stochastic tap, divide by the texture size); `on` false keeps the isotropic tap.
struct Grad2 {
    x: vec2<f32>,
    y: vec2<f32>,
    on: bool,
};

// Gradients for a planar projection whose footprint axes are `gx`, `gy` (metres in the
// projection plane) at explicit `lod`. The shorter axis is lengthened to at least
// 1/`aniso_level` of the longer (the setting's level; the sampler itself clamps at 16),
// and a degenerate one becomes the longer axis' perpendicular.
//
// The sampler path's LOD offset (`aniso_lod_offset`, ADR-5459) is added to the exponent here,
// once per projection: both gradients then come out 2^offset longer, which is what the sampler
// needs, and `layer_tap` (once per texture tap, tens per pixel) computes nothing for it
// (ADR-5943). The offset is 0 on every adapter without one and on the footprint path, and
// `lod + 0` is `lod`: their gradients are bit for bit what they were.
fn aniso_grads(gx: vec2<f32>, gy: vec2<f32>, lod: f32) -> Grad2 {
    let n = aniso_level();
    let lx = length(gx);
    let ly = length(gy);
    let major = max(lx, ly);
    if (n < 1.5 || major < 1e-12) {
        return Grad2(vec2<f32>(0.0), vec2<f32>(0.0), false);
    }
    let floor_len = major / n;
    var x = gx;
    var y = gy;
    if (lx < floor_len) {
        x = select(vec2<f32>(-gy.y, gy.x) / ly, gx / max(lx, 1e-30), lx > major * 1e-6) * floor_len;
    }
    if (ly < floor_len) {
        y = select(vec2<f32>(gx.y, -gx.x) / lx, gy / max(ly, 1e-30), ly > major * 1e-6) * floor_len;
    }
    let k = exp2(lod + aniso_lod_offset) / major;
    return Grad2(x * k, y * k, true);
}

// Gradients of the top (XZ) projection.
fn aniso_grads_top(lod: f32) -> Grad2 {
    return aniso_grads(aniso_dx.xz, aniso_dy.xz, lod);
}

// One tap of a texture array at `uv`: anisotropic through `g` (rotated by the stochastic
// tap's `cs`) when on, else the isotropic explicit-LOD tap. On drivers that drop the
// sampler's anisotropy for explicit-gradient array samples (`aniso_footprint`) the
// anisotropic tap is assembled in the shader instead (`layer_tap_footprint`).
//
// This runs once per texture tap. Keep it to the rotation, the two `/ dims` and the sample:
// anything that does not depend on the tap belongs in `aniso_grads` (once per projection) or,
// when it comes from a uniform, in a private that `pixel_footprint` stores once per pixel
// (`aniso_lod_offset`; `tests/layer_tap_cost.rs`, ADR-5943).
fn layer_tap(tex: texture_2d_array<f32>, uv: vec2<f32>, layer: i32, lod: f32, g: Grad2, cs: vec2<f32>) -> vec4<f32> {
    if (g.on) {
        let dims = vec2<f32>(textureDimensions(tex));
        let gx = rotate2(g.x, cs);
        let gy = rotate2(g.y, cs);
        if (aniso_footprint()) {
            return layer_tap_footprint(tex, uv, layer, gx, gy, dims);
        }
        return textureSampleGrad(tex, material_aniso_samp, uv, layer, gx / dims, gy / dims);
    }
    return textureSampleLevel(tex, material_samp, uv, layer, lod);
}

// The shader takes the anisotropic taps itself (`TerrainGpu::set_anisotropy_footprint`,
// ADR-3957): Mesa's Intel Vulkan driver filters an explicit-gradient sample of an ARRAY texture
// at the long axis' level whatever the sampler's anisotropy (measured on an Arc Pro B65: 2D
// samples keep it), so every Arc player saw isotropic terrain.
//
// The shared colour pass compiles the test as `false` (the `@aniso-footprint` markers,
// `without_aniso_footprint`) on a terrain whose footprint path is off: `layer_tap` is inlined at
// every texture tap site, and the footprint loop compiled in beside each one, never taken, cost
// the terrain forward pass on NVIDIA (ADR-7886).
fn aniso_footprint() -> bool {
    // @aniso-footprint-begin
    return frame.texture_lod.w > 0.5;
    // @aniso-footprint-end
    // @aniso-footprint-off return false;
}

// Most taps `layer_tap_footprint` spreads along a footprint's long axis (the sampler's clamp).
const FOOTPRINT_ANISO_TAPS: f32 = 16.0;

// An anisotropic tap assembled from trilinear taps, the footprint assembly anisotropic filtering
// does: `gx`, `gy` are the pixel's footprint axes in level-0 texels. N = the long axis over the
// short one (the parallelogram's height across the long axis, at least 1/level of it, as
// `aniso_grads` floors it), at most the frame's level, equally weighted taps sit evenly along the
// long axis, each at the level of its share of that axis. An 8-tap cap (and Gaussian weights)
// were tried: with up to a mip more blur across the short axis than the sampler, the frame under
// TAAU changed 1.5-1.8x as much per unit of detail as the sampler's (`terrain_aniso`, 5090).
fn layer_tap_footprint(tex: texture_2d_array<f32>, uv: vec2<f32>, layer: i32, gx: vec2<f32>, gy: vec2<f32>, dims: vec2<f32>) -> vec4<f32> {
    let lx = length(gx);
    let ly = length(gy);
    let major = max(max(lx, ly), 1e-6);
    let level = clamp(aniso_level(), 1.0, FOOTPRINT_ANISO_TAPS);
    let minor = max(abs(gx.x * gy.y - gx.y * gy.x) / major, major / level);
    let taps = clamp(ceil(major / minor - 0.01), 1.0, level);
    let lod = max(log2(max(major / taps, minor)), 0.0);
    let axis = select(gy, gx, lx > ly) / dims;
    var sum = vec4<f32>(0.0);
    let count = i32(taps);
    for (var i = 0; i < count; i++) {
        let t = (f32(i) + 0.5) / taps - 0.5;
        sum += textureSampleLevel(tex, material_samp, uv + axis * t, layer, lod);
    }
    return sum / taps;
}

// Lane `i` (0..3) of `v`, and `v` with lane `i` replaced, without a dynamic vector index.
// Mali-G78 (r54p3) lost the device on the photoreal material path when the splat weights
// were read and written as `v[i]` (Pixel 6, 2026-10-08).
fn lane(v: vec4<f32>, i: i32) -> f32 {
    return select(select(v.w, v.z, i == 2), select(v.y, v.x, i == 0), i < 2);
}

fn with_lane(v: vec4<f32>, i: i32, x: f32) -> vec4<f32> {
    return select(v, vec4<f32>(x), vec4<i32>(i) == vec4<i32>(0, 1, 2, 3));
}

// Splat weights live in two vec4 locals (layers 0..3 and 4..7) instead of a function-local
// array: naga 30 gives a Function `array` an ArrayStride that SPIR-V 1.6 rejects (ADR-026).
fn pick_weight(wa: vec4<f32>, wb: vec4<f32>, i: i32) -> f32 {
    if (i < 4) {
        return lane(wa, i);
    }
    return lane(wb, i - 4);
}

// Per-fragment heightfield normal at the pixel's footprint: the pyramid level whose texel
// matches the footprint, differenced one texel of that level each way. Distant fragments
// shade with the prefiltered relief their pixel covers — not the coarse CDLOD vertex
// normal, and not a point-sampled difference of the full-resolution map that aliases
// (the old version sampled level 0 at ±footprint).
//
// The level never drops more than one below the level the triangle's vertices were read
// from (`geo_mip`): a normal much sharper than the geometry it shades puts the true crease
// of a cliff lip somewhere on a coarse ramp, and the piecewise-linear triangles map that
// straight crease to a row of lit and unlit teeth along every lip.
//
// The normal comes from the precomputed slope pyramid (`normal_slope`, two filtered fetches);
// it replaced `normal_mip` here (32 height loads per pixel, lane terrain-shader-perf) and,
// being cheaper, the terrain budget's hardware-filtered `normal_hw` too.
//
// With anisotropic filtering on (ADR-192) the level follows the footprint's short axis and
// up to `NORMAL_ANISO_TAPS` normals spread along its long axis are averaged (a footprint
// assembly of the slope pyramid: the hardware cannot filter it anisotropically because
// each level has its own texel mapping), so grazing ground keeps the relief of its short
// axis instead of the long axis' mean.
fn fragment_normal(xz: vec2<f32>, footprint: f32, geo_mip: f32) -> vec3<f32> {
    let lx = length(aniso_dx);
    let ly = length(aniso_dy);
    let major = max(lx, ly);
    let tap_cap = min(floor(aniso_level() * 0.5), NORMAL_ANISO_TAPS);
    if (tap_cap < 1.5 || major <= 0.0) {
        let m = max(footprint_mip(footprint), geo_mip - 1.0);
        return normal_slope(xz, m);
    }
    // The parallelogram's height across the long axis is its short axis.
    let minor = length(cross(aniso_dx, aniso_dy)) / major;
    let ratio = major / max(minor, major * 1e-4);
    let taps = clamp(round(ratio), 1.0, tap_cap);
    let m = max(footprint_mip(footprint / min(ratio, taps)), geo_mip - 1.0);
    if (taps < 1.5) {
        return normal_slope(xz, m);
    }
    // Long axis on the ground, scaled to the footprint the frame measured.
    let axis = select(aniso_dy.xz, aniso_dx.xz, lx > ly) * (footprint / major);
    var sum = vec3<f32>(0.0);
    let count = i32(taps);
    for (var i = 0; i < count; i++) {
        let t = (f32(i) + 0.5) / taps - 0.5;
        sum += normal_slope(xz + axis * t, m);
    }
    return normalize(sum);
}

// Most heightfield normals `fragment_normal` averages along a footprint's long axis.
const NORMAL_ANISO_TAPS: f32 = 4.0;

// Slope pyramid (`slope_mips.rs`): level k holds the central-difference slope
// (dh/dx, dh/dz) of height level k at its own texel spacing, border-clamped. A one-texel
// central difference of a bilinear surface is linear in the heights and keeps the fraction,
// so one bilinear fetch of level k equals `normal_level(xz, k)`'s four bilinear height taps.
@group(1) @binding(20) var slope_texture: texture_2d<f32>;
@group(1) @binding(21) var slope_samp: sampler;

// Slope of integer level `k` at level-0 sample coordinates `s`: the texel coordinate of the
// pyramid convention (`height_level`), clamped like it, mapped to that level's own UV so
// non-power-of-two maps line up too.
fn slope_level(s: vec2<f32>, k: u32) -> vec2<f32> {
    let scale = f32(1u << k);
    let dims = level_dims(k);
    let max_s = vec2<f32>(f32(dims.x - 1), f32(dims.y - 1));
    let p = clamp((s - vec2<f32>((scale - 1.0) * 0.5)) / scale, vec2<f32>(0.0), max_s);
    let uv = (p + vec2<f32>(0.5)) / vec2<f32>(dims);
    return textureSampleLevel(slope_texture, slope_samp, uv, f32(k)).rg;
}

// Heightfield normal at fractional pyramid level `m`: the two nearest levels' slopes blended
// by the fraction (the CPU mirror is `slope_mips::sample_slope`).
fn normal_slope(xz: vec2<f32>, m: f32) -> vec3<f32> {
    let top = f32(height_levels() - 1u);
    let mm = clamp(m, 0.0, top);
    let k0 = u32(floor(mm));
    let f = mm - f32(k0);
    let s = sample_coord(xz);
    var g = slope_level(s, k0);
    if (f > 0.0 && f32(k0) < top) {
        g = mix(g, slope_level(s, k0 + 1u), f);
    }
    return normalize(vec3<f32>(-g.x, 1.0, -g.y));
}

// @hw-height-begin (replaced by `hw_height_source` when the device filters 32-bit floats)
// Height from the pyramid with hardware trilinear filtering (terrain budget). Without
// filterable floats this is the manual read, and the budget bit is never set.
fn height_hw(xz: vec2<f32>, m: f32) -> f32 {
    return height_w_mip(xz, m);
}
// @hw-height-end

// `normal_mip` through `height_hw`: one filtered fetch per tap.
fn normal_hw(xz: vec2<f32>, m: f32) -> vec3<f32> {
    let step = texel_m() * exp2(m);
    let h_l = height_hw(xz - vec2<f32>(step, 0.0), m);
    let h_r = height_hw(xz + vec2<f32>(step, 0.0), m);
    let h_d = height_hw(xz - vec2<f32>(0.0, step), m);
    let h_u = height_hw(xz + vec2<f32>(0.0, step), m);
    return normalize(vec3<f32>(h_l - h_r, 2.0 * step, h_d - h_u));
}

// ---------------------------------------------------------------------------
// Near-field relief and the specular lobe
// ---------------------------------------------------------------------------

// Eye distance (m) inside which the relief march resolves single heightfield texels
// (`heightfield_sun_occlusion`).
const HF_NEAR_STEP_M: f32 = 20.0;

// Set by `shade` when the cascade already hides the sun completely (and no debug view
// reads the relief term): the relief march cannot change `min(shadow, relief)`, so
// `surface_rest` skips it. The virtual texture's bake never sets it.
var<private> relief_sun_hidden: bool = false;

// Heightfield sun occlusion (soft): march from the surface toward the sun over the height
// pyramid and keep the smallest clearance-to-distance ratio (penumbra estimate after
// Quilez 2010, "soft shadows" for ray-marched height/distance fields). Steps grow
// geometrically from about one pixel footprint; each reads the pyramid level that matches
// its step, so the march sees prefiltered relief. It resolves what the cascades cannot —
// decimetre crust, the lit/shadowed sides of every ridge at any distance — and is
// combined with the cascade term by `min`.
//
// Near the eye (`dist` under `HF_NEAR_STEP_M`, blended out by then) a lip one texel wide
// shadows the ground behind it under a low sun, and those shadows are many pixels across.
// The reference tracer put them where the raster lit the ground (Europa's close plate: its
// occluders sit 0.1–0.7 m up-sun, inside the old first step of 1.5 texels, and they rise a
// few centimetres above a 17° ray). There the march (`heightfield_sun_occlusion_near`)
// - starts at a quarter of a texel and grows ×1.3 over its first 1.5 m (up to six more taps,
//   so it reaches as far as before),
// - reads the mesh's own triangles (`height_tri0`) for level-0 taps, not the bilinear patch
//   (they part by centimetres inside a cell),
// - leaves from the smooth surface the vertex normals describe, 2 mm up (was 2 cm above the
//   flat facet), and is dark where that surface faces away from the sun, as the tracer's
//   rays and facing test are (`hf_terminator_start`),
// - centres the penumbra on the lip (the far march keeps its half-penumbra of slack).
// Beyond 20 m the march is unchanged.
fn heightfield_sun_occlusion(world: vec3<f32>, to_sun: vec3<f32>, footprint: f32, reach: f32, dist: f32) -> f32 {
    let horiz = length(to_sun.xz);
    if (reach <= 0.0 || horiz < 1e-4 || to_sun.y <= 0.0) {
        return 1.0;
    }
    let dir_xz = to_sun.xz / horiz;
    let rise = to_sun.y / horiz; // metres up per metre along the ground
    if (dist < HF_NEAR_STEP_M) {
        return heightfield_sun_occlusion_near(world, dir_xz, rise, footprint, reach, dist);
    }
    // The far march: a constant trip count (the compiler unrolls it).
    var step = max(footprint, texel_m()) * 1.5;
    var travelled = 0.0;
    var visibility = 1.0;
    // Start a hair above the surface so the first tap does not self-occlude.
    let base = world.y + 0.02 + 0.5 * step * 0.25;
    for (var i = 0; i < 10; i++) {
        travelled += step;
        if (travelled > reach) {
            break;
        }
        let p = world.xz + dir_xz * travelled;
        // One pyramid level per tap (no blend between levels): half the loads, and the
        // occlusion test does not need the level-continuity the geometry needs.
        let level = u32(clamp(round(footprint_mip(step * 0.5)), 0.0, f32(height_levels() - 1u)));
        let ground = height_level(sample_coord(p), level);
        let ray = base + rise * travelled;
        // Penumbra width grows with distance (a sun of ~2° across, softened).
        visibility = min(visibility, clamp((ray - ground) / (travelled * 0.06) + 0.5, 0.0, 1.0));
        if (visibility <= 0.0) {
            break;
        }
        step *= 1.65;
    }
    return visibility;
}

// `heightfield_sun_occlusion` within `HF_NEAR_STEP_M` of the eye, blended into the far march
// by then (`far_t` 0 at 5 m .. 1 at 20 m).
fn heightfield_sun_occlusion_near(world: vec3<f32>, dir_xz: vec2<f32>, rise: f32, footprint: f32, reach: f32, dist: f32) -> f32 {
    // 0.25 at the eye .. 1 at `HF_NEAR_STEP_M`.
    let near = clamp(dist / HF_NEAR_STEP_M, 0.25, 1.0);
    let far_t = (near - 0.25) / 0.75;
    var step = max(footprint, texel_m() * near) * 1.5;
    // Taps that restore the reach a full first step had (×1.65 per tap), plus three for
    // the ×1.3 stretch.
    let taps = 13u + min(u32(ceil(log2(1.0 / near) / log2(1.65))), 3u);
    var travelled = 0.0;
    var visibility: f32;
    // Rays leave the smooth surface the vertex normals describe (`hf_terminator_start`), a
    // hair above it so the first tap does not self-occlude. That surface turned away from
    // the sun is in shadow whatever the detail normals say (past a crest under a low sun;
    // the reference tracer tests facing with the same normal), with a ±0.6° edge.
    let surf0 = hf_terminator_start(world.xz);
    let start = mix(surf0.p, world, far_t);
    let base = start.y + mix(0.002, 0.02, far_t) + 0.5 * step * 0.25 * far_t;
    let edge = 0.5 * far_t;
    let facing = clamp(dot(surf0.n, normalize(vec3<f32>(dir_xz.x, rise, dir_xz.y))) * 50.0 + 0.5, 0.0, 1.0);
    visibility = mix(facing, 1.0, far_t);
    if (visibility <= 0.0) {
        return 0.0;
    }
    for (var i = 0u; i < taps; i++) {
        travelled += step;
        if (travelled > reach) {
            break;
        }
        let p = start.xz + dir_xz * travelled;
        let level = u32(clamp(round(footprint_mip(step * 0.5)), 0.0, f32(height_levels() - 1u)));
        var ground: f32;
        if (level == 0u) {
            ground = height_tri0(sample_coord(p));
        } else {
            ground = height_level(sample_coord(p), level);
        }
        let ray = base + rise * travelled;
        visibility = min(visibility, clamp((ray - ground) / (travelled * 0.06) + edge, 0.0, 1.0));
        if (visibility <= 0.0) {
            break;
        }
        step *= select(1.65, 1.3, travelled < 1.5);
    }
    return visibility;
}

// Concavity of the prefiltered surface at the footprint scale: how far the point sits below
// the surface averaged over an 8× wider footprint, relative to that width (obscurance
// from local height differences — Zhukov, Iones & Kronin 1998; the depth unsharp mask of
// Luft, Colditz & Deussen 2006). 1 on crests and plains, < 1 in gullies, crevasse floors
// and at the foot of walls. Keeps far relief reading under flat sky light.
fn height_cavity(xz: vec2<f32>, y: f32, footprint: f32, strength: f32) -> f32 {
    if (strength <= 0.0) {
        return 1.0;
    }
    let m = footprint_mip(footprint) + 3.0;
    let wide = texel_m() * exp2(m);
    let blurred = height_w_mip(xz, m);
    let depth = max(blurred - y, 0.0) / max(wide, 1e-3);
    // Saturating: a gully or the foot of a wall loses at most about half its sky; a deep
    // trench is not black (its floor still sees a slot of sky and the walls' bounce).
    let occluded = depth / (depth + 0.6);
    return 1.0 - clamp(strength, 0.0, 1.0) * 0.55 * occluded;
}

// Canyon-scale horizon (16–256 m) from the heightfield: for four azimuths, the highest
// horizon slope `t` over three distances; a horizontal surface under a uniform sky dome gets
// `cos²(atan t) = 1 / (1 + t²)` of the sky irradiance from each sector (horizon-based ambient
// occlusion, Bavoil, Sainz & Dimitrov 2008, applied to the height field). The cavity term
// above covers the footprint scale; this is the canyon wall that hides part of the sky from
// the floor between them.
//
// Sky visibility (x) and the sunlit-terrain bounce factor (y) from the same 16 horizon loads.
//
// x: the open-sky fraction above the surface's own tangent plane (normal `n`; a planar slope
// is already in the normal-oriented sky light), counted only where it falls under 0.9 so
// rolling ground stays 1 and terrain keeps matching a mesh lying on it (meshes take no
// horizon term); canyon floors darken.
//
// The terrain that hides the sky in each of the four sectors is itself lit by the sun: its
// radiance is `ρ_wall · E_sun · max(n_wall·L, 0) / π`. A horizontal receiver sees the blocked
// part of a sector as a cosine-weighted view factor of `sin²θ_h` (θ_h the horizon elevation),
// a quarter of the hemisphere per sector, so the bounce irradiance over π is
// `Σ_sectors ¼ · sin²θ_h · ρ_wall · E_sun · max(n_wall·L, 0) / π`. The wall normal is the
// plane from the receiver up to its horizon, facing back toward the receiver; the wall's
// albedo is the receiver's layer albedo (the caller multiplies). `bounce_up` returns
// `Σ ¼ · sin²θ_h · max(n_wall·L, 0) · lit`: bounded by the blocked view factor, so the bounce
// never exceeds albedo × the direct irradiance the walls receive. `bounce` is the same sum
// with the view factor of the receiver's own normal `n` (a face turned toward a wall sees
// more of it than the floor does; see the loop). `lit` is the part of each
// wall above the opposite wall's shadow (round 8; before it, the walls' own shadowing was
// ignored and the term was an upper bound, 2.3× the path tracer's GI in canyon shadow).
// Per-sector open-sky fraction (sectors at 45°, 135°, 225°, 315° from +X toward +Z) and the
// bounce factor.
struct Horizon {
    open: vec4<f32>,
    bounce: f32,
    // The same bounce for a horizontal receiver (`Σ ¼ · sin²θ_h · cos_w · lit`): the light the
    // ground around this point gets from the sunlit walls, for the ground this face sees.
    bounce_up: f32,
    // Per sector, the steepest slope to the four horizon points (signed; the tangent-plane
    // term `open` takes from it with the normal: `horizon_open`).
    slopes: vec4<f32>,
};

// The open-sky fraction per sector for normal `n` from the sectors' steepest horizon slopes
// (the part of `horizon_sky_and_bounce` that depends on the normal; the terrain virtual
// texture caches `slopes` and evaluates this per pixel).
fn horizon_open(slopes: vec4<f32>, n: vec3<f32>) -> vec4<f32> {
    var open4 = vec4<f32>(0.0);
    for (var a = 0; a < 4; a++) {
        let ang = f32(a) * 1.5707964 + 0.7853982;
        let dir = vec2<f32>(cos(ang), sin(ang));
        let plane = -(n.x * dir.x + n.z * dir.y) / max(n.y, 0.2);
        let pick = vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3));
        let t = max(max(plane, dot(slopes, pick)) - plane, 0.0);
        open4 += pick / (1.0 + t * t);
    }
    return open4;
}

fn horizon_sky_and_bounce(world: vec3<f32>, footprint: f32, n: vec3<f32>, to_sun: vec3<f32>) -> Horizon {
    // Level count and sizes from the uniform (`level_dims`): texture-size queries are
    // texture-unit work, one per tap.
    let top = height_levels() - 1u;
    let s0 = sample_coord(world.xz);
    let to_s = terrain.sample_max.x / terrain.world_width;
    var open4 = vec4<f32>(0.0);
    // Raw horizon slope per sector and the distance it was found at (the wall).
    var tv = vec4<f32>(0.0);
    var dv = vec4<f32>(10.0);
    // Slope of the terrain just below each horizon point (the wall's own face, steeper than
    // the chord from the receiver).
    var wv = vec4<f32>(0.0);
    // Steepest slope per sector, any sign.
    var sv = vec4<f32>(-1e9);
    // Four azimuths (diagonals) × four distances (10, 30, 90, 270 m), one point load each at
    // the pyramid level whose texel is a quarter of the distance: 16 loads. (Three distances
    // from 16 m missed walls 10–40 m away: a canyon floor saw too much sky and too little
    // wall, against the reference path tracer.)
    for (var a = 0; a < 4; a++) {
        let ang = f32(a) * 1.5707964 + 0.7853982;
        let dir = vec2<f32>(cos(ang), sin(ang));
        // Slope of the surface's own tangent plane along `dir`: a planar slope is already
        // in the normal-oriented sky light, so only relief above that plane occludes.
        let plane = -(n.x * dir.x + n.z * dir.y) / max(n.y, 0.2);
        // Raw horizon slope (the bounce's wall) and the horizon above the tangent plane.
        var max_t = 0.0;
        var d_at = 10.0;
        var wall_t = 0.0;
        var max_rel = plane;
        var max_steep = -1e9;
        var d = 10.0;
        var prev_h = world.y;
        var prev_d = 0.0;
        for (var k = 0; k < 4; k++) {
            let level = min(u32(round(footprint_mip(max(d * 0.25, footprint)))), top);
            let scale = f32(1u << level);
            let dims = level_dims(level);
            let p = (s0 + dir * d * to_s - vec2<f32>((scale - 1.0) * 0.5)) / scale;
            let texel = clamp(vec2<i32>(round(p)), vec2<i32>(0), dims - vec2<i32>(1));
            let h = textureLoad(height_texture, texel, i32(level)).r * terrain.height_scale + terrain.height_offset;
            let slope = (h - world.y) / d;
            if (slope > max_t) {
                max_t = slope;
                d_at = d;
                wall_t = (h - prev_h) / (d - prev_d);
            }
            max_rel = max(max_rel, slope);
            max_steep = max(max_steep, slope);
            prev_h = h;
            prev_d = d;
            d *= 3.0;
        }
        let t = max(max_rel - plane, 0.0);
        open4 += vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3)) / (1.0 + t * t);
        let pick = vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3));
        // `select`, not `mix`: from the −1e9 start, `mix` evaluated as a + t·(b − a) (NVIDIA)
        // rounds max_steep + 1e9 to 1e9 in f32 and stored a slope of 0 for every sector.
        sv = select(sv, vec4<f32>(max_steep), pick > vec4<f32>(0.5));
        tv += pick * max_t;
        wv += pick * max(wall_t, max_t);
        dv = mix(dv, vec4<f32>(d_at), pick);
    }
    // Bounce from the raw horizons: sin²θ_h = 1 − open; the wall faces back toward the
    // receiver. The wall is lit only above the shadow the opposite sector's wall casts on it
    // (a canyon at a low sun: the far wall's foot is in the near wall's shadow): with the
    // walls' heights H = t·d above the receiver and the sun's elevation in the section,
    // the shadow line on the wall sits at H_opp − (d + d_opp)·tan(el), and the wall's lit
    // fraction is the part of its height above it.
    let opp_t = tv.zwxy;
    let opp_d = dv.zwxy;
    var bounce = 0.0;
    var bounce_up = 0.0;
    for (var a = 0; a < 4; a++) {
        let ang = f32(a) * 1.5707964 + 0.7853982;
        let dir = vec2<f32>(cos(ang), sin(ang));
        let pick = vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3));
        let max_t = dot(tv, pick);
        let d_at = dot(dv, pick);
        let open = 1.0 / (1.0 + max_t * max_t);
        // The wall's face: the slope just below its top, not the chord from the receiver
        // (a canyon wall is steeper than the chord, and the chord's upward tilt caught the
        // overhead sun the real face does not).
        let face_t = dot(wv, pick);
        let wall_n = normalize(vec3<f32>(-dir.x * face_t, 1.0, -dir.y * face_t));
        let cos_w = max(dot(wall_n, to_sun), 0.0);
        let toward = -dot(to_sun.xz, dir);
        var lit = 1.0;
        let h1 = max_t * d_at;
        if (toward > 1e-3 && h1 > 1e-3) {
            let shadow_h = dot(opp_t, pick) * dot(opp_d, pick) - (d_at + dot(opp_d, pick)) * to_sun.y / toward;
            lit = clamp(1.0 - max(shadow_h, 0.0) / h1, 0.0, 1.0);
        }
        // The receiver's view factor of the wall: the band from the horizontal up to θ_h,
        // 90° of azimuth wide, has the vector irradiance V = √2·A·dir + (π/2)·B·up per unit
        // radiance (A = θ/2 + sin 2θ/4, B = sin²θ/2); the receiver takes n·V/π of it. A
        // horizontal receiver gets B/2 = ¼·sin²θ_h, the term's original factor; a wall
        // facing across a trench sees 2.3× that at θ_h = 45° (the opposite wall fills its
        // view), and a face turned away from the sector (its own slope rising behind it)
        // sees none of it.
        let theta = atan(max_t);
        let band_a = 0.5 * theta + 0.5 * max_t * open;
        let band_b = 0.5 * (1.0 - open);
        let view = max(dot(n, vec3<f32>(dir.x * 1.4142135 * band_a, 1.5707964 * band_b, dir.y * 1.4142135 * band_a)), 0.0) * INV_PI;
        bounce += view * cos_w * lit;
        bounce_up += 0.5 * band_b * cos_w * lit;
    }
    // The receiver's own view factor is for walls, which face the opposite wall across a
    // trench. Gentle ground keeps the horizontal receiver's factor: it took 0.1–0.15 more
    // of the path tracer's light in shade from the slope's tilt toward the nearest wall
    // (Europa's shaded flats at 1.3×), where the tracer sees that wall partly hidden by the
    // ground's own relief.
    bounce = mix(bounce_up, bounce, clamp((0.75 - n.y) / 0.2, 0.0, 1.0));
    return Horizon(open4, bounce, bounce_up, sv);
}

// Multi-bounce ambient occlusion (Jimenez, Wu, Pesce & Jarabo, "Practical Real-Time
// Strategies for Accurate Indirect Occlusion", 2016, eq. 10): the visibility `v` of a crease
// mapped to the light that reaches it after interreflection off walls of albedo `rho`.
fn multi_bounce_ao(v: f32, rho: vec3<f32>) -> vec3<f32> {
    let a = 2.0404 * rho - vec3<f32>(0.3324);
    let b = -4.7951 * rho + vec3<f32>(0.6417);
    let c = 2.7552 * rho + vec3<f32>(0.6903);
    let x = vec3<f32>(clamp(v, 0.0, 1.0));
    return max(x, ((x * a + b) * x + c) * x);
}

// Geometric specular AA, the mesh path's `pbr_lighting::specular_aa_roughness` (Kaplanyan &
// Hoffman 2016; Tokuyoshi & Kaplanyan 2019; the Filament `normalFiltering` form) with the
// engine's defaults (`regia_render::specular_aa`): the screen-space derivative energy of the
// shading normal widens the GGX lobe, α'² = α² + min(2 · 0.15 · (|∂n/∂x|² + |∂n/∂y|²), 0.2),
// so the cliff relief, crust and grain normals do not sparkle as the camera moves.
const SPECULAR_AA_VARIANCE: f32 = 0.15;
const SPECULAR_AA_THRESHOLD: f32 = 0.2;
fn specular_aa_roughness(perceptual: f32, dn_dx: vec3<f32>, dn_dy: vec3<f32>) -> f32 {
    let sigma2 = SPECULAR_AA_VARIANCE * (dot(dn_dx, dn_dx) + dot(dn_dy, dn_dy));
    let kernel = min(2.0 * sigma2, SPECULAR_AA_THRESHOLD);
    let a = perceptual * perceptual;
    return max(perceptual, sqrt(sqrt(clamp(a * a + kernel, 0.0, 1.0))));
}

// GGX / Trowbridge–Reitz NDF (Walter et al. 2007).
fn d_ggx(n_h: f32, alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let d = n_h * n_h * (a2 - 1.0) + 1.0;
    return a2 * INV_PI / max(d * d, 1e-7);
}

// Height-correlated Smith visibility (Heitz 2014), the form `regia_render` uses.
fn v_smith(n_v: f32, n_l: f32, alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let gv = n_l * sqrt(n_v * n_v * (1.0 - a2) + a2);
    let gl = n_v * sqrt(n_l * n_l * (1.0 - a2) + a2);
    return 0.5 / max(gv + gl, 1e-5);
}

// Cook–Torrance specular (GGX, Smith, Schlick Fresnel) of a dielectric with reflectance
// `f0`, times N·L. Ice ≈ 0.018, rock and soil ≈ 0.04.
fn specular_ggx(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, roughness: f32, f0: f32) -> f32 {
    let h = normalize(v + l);
    let n_l = max(dot(n, l), 0.0);
    let n_v = max(dot(n, v), 1e-4);
    if (n_l <= 0.0) {
        return 0.0;
    }
    let alpha = max(roughness * roughness, 0.002);
    // V·H clamped to 1 as well: a light at the eye gives h == v and V·H = 1 + 1 ulp, and `pow` of
    // the negative base is NaN (black pixels in a headlight's cone; tunnel_scene, lane v020r).
    let f = f0 + (1.0 - f0) * pow(1.0 - clamp(dot(v, h), 0.0, 1.0), 5.0);
    return d_ggx(max(dot(n, h), 0.0), alpha) * v_smith(n_v, n_l, alpha) * f * n_l;
}

// The frame's extra directional lights (ADR-7211, ADR-7254; `TerrainGpu::set_extra_lights`):
// the light a surface of this albedo, normal and roughness takes from each, unshadowed, as the
// forward shades them. Lambert diffuse plus the GGX lobe of a dielectric of reflectance `f0`.
// Slots whose direction row has w = 0 are off, so a frame without extra lights adds exactly 0.
fn extra_lights_direct(n: vec3<f32>, v: vec3<f32>, albedo: vec3<f32>, roughness: f32, f0: f32) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < 3u; i = i + 1u) {
        let row = frame.extra_dir[2u * i];
        if (row.w > 0.5) {
            let l = -normalize(row.xyz);
            let n_l = max(dot(n, l), 0.0);
            if (n_l > 0.0) {
                let e = frame.extra_dir[2u * i + 1u].rgb;
                sum += e * (albedo * n_l * INV_PI + vec3<f32>(specular_ggx(n, v, l, roughness, f0)));
            }
        }
    }
    return sum;
}

// Ice and snow sparkle as discrete microfacets (Jakob et al. 2014, "Discrete stochastic
// microfacet models"; Zirr & Kaplanyan 2016): the surface is cut into crystal cells of a few
// millimetres; a few cells hold a flat, mirror-like facet with a random tilt. A facet that
// happens to bisect the view and sun directions flashes with the GGX lobe of a near-mirror.
// The cell grid is only evaluated while a cell covers at least about a pixel (it fades out
// as cells go sub-pixel, where the smooth lobe already carries their average), so it does
// not alias. Returns the extra specular (× N·L) to add.
fn glint(world: vec3<f32>, n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, footprint: f32, density: f32) -> f32 {
    let cell = 0.006;
    let resolve = 1.0 - smoothstep01((footprint - cell) / (cell * 3.0));
    if (resolve <= 0.0) {
        return 0.0;
    }
    // Cell lattice in the surface's dominant plane.
    var uv = world.xz;
    if (abs(n.x) > abs(n.y) && abs(n.x) > abs(n.z)) {
        uv = world.zy;
    } else if (abs(n.z) > abs(n.y)) {
        uv = world.xy;
    }
    let id = floor(uv / cell);
    let pick = hash21(id + vec2<f32>(3.7, 11.3));
    if (pick > density) {
        return 0.0;
    }
    let a = hash21(id * 1.37 + vec2<f32>(-5.1, 2.9)) * 6.2831853;
    let tilt = hash21(id * 0.71 + vec2<f32>(9.4, -1.2)) * 0.45;
    // Tilt the facet normal around the surface normal.
    var t = normalize(cross(n, vec3<f32>(0.0, 0.0, 1.0)));
    if (abs(n.z) > 0.9) {
        t = normalize(cross(n, vec3<f32>(1.0, 0.0, 0.0)));
    }
    let b = cross(n, t);
    let facet = normalize(n + (t * cos(a) + b * sin(a)) * tan(tilt));
    let h = normalize(v + l);
    let n_l = max(dot(n, l), 0.0);
    let n_v = max(dot(n, v), 1e-3);
    // A near-mirror facet (alpha 0.02) filling the cell: the full Cook–Torrance term with
    // Fresnel of ice, faded as the cell drops below a pixel.
    let alpha = 0.02;
    let lobe = d_ggx(max(dot(facet, h), 0.0), alpha) * v_smith(n_v, max(n_l, 1e-3), alpha);
    let f = 0.018 + 0.982 * pow(1.0 - clamp(dot(v, h), 0.0, 1.0), 5.0);
    return lobe * f * n_l * resolve;
}

// The roughness the shared HDR target carries for the screen-space reflection pass.
// That pass blends a single sharp ray into any surface below 0.7 roughness at up to 72 %
// (`regia_render` ssr.wgsl: weight = 0.72 · clamp((0.7 − a) / 0.35)), with no Fresnel and
// no roughness blur — right for the lab's polished floors, a mirror on ice. Terrain is a
// dielectric (f0 ≈ 0.02–0.04), so it hands the pass the value whose blend equals the energy
// of its own specular reflection: Schlick Fresnel at the view angle times the share of a
// GGX lobe that stays near-specular, (1 − r)². Rough ground (r ≥ 0.7) and anything seen
// head-on get no screen-space reflection; glazed ice at grazing angles gets a faint one.
fn ssr_roughness(perceptual: f32, n_v: f32) -> f32 {
    let r = clamp(perceptual, 0.0, 1.0);
    let fresnel = 0.03 + 0.97 * pow(1.0 - clamp(n_v, 0.0, 1.0), 5.0);
    let weight = fresnel * (1.0 - r) * (1.0 - r) * (1.0 - smoothstep01((r - 0.45) / 0.25));
    let fade = clamp(weight / 0.72, 0.0, 1.0);
    if (r >= 0.7) {
        return r;
    }
    return clamp(0.7 - 0.35 * fade, 0.05, 1.0);
}

// The engine's stochastic-SSR surface code (`regia_render` lab_forward.wgsl `ssr_pack`, decoded
// by sssr_common.wgsl `decode_alpha`): 5 bits perceptual roughness, 5 bits √(specular weight),
// 4 bits the share of the pixel that is environment specular, as a negative normal half float
// (exact in the Rgba16Float target, never a 0..1 alpha). Written only when the terrain opted in
// (`TerrainSharedDraw::set_screen_space_reflections`).
fn sssr_pack(perceptual: f32, weight: f32, env_share: f32) -> f32 {
    let r = u32(round(clamp(perceptual, 0.0, 1.0) * 31.0));
    let w = u32(round(sqrt(clamp(weight, 0.0, 1.0)) * 31.0));
    let f = u32(round(clamp(env_share, 0.0, 1.0) * 15.0));
    let code = r | (w << 5u) | (f << 10u);
    let e = 1u + (code >> 10u);
    return -ldexp(f32(1024u + (code & 1023u)), i32(e) - 25);
}

// ---------------------------------------------------------------------------
// Near-field detail (crust plates for ice/snow, grain and pebbles for dust)
// ---------------------------------------------------------------------------

// Worley (cellular) noise, Worley 1996: distance to the nearest and second-nearest jittered
// feature point, and the nearest cell's id.
struct Cell {
    f1: f32,
    f2: f32,
    id: vec2<f32>,
}

fn worley(p: vec2<f32>) -> Cell {
    let base = floor(p);
    var f1 = 8.0;
    var f2 = 8.0;
    var id = base;
    for (var j = -1; j <= 1; j++) {
        for (var i = -1; i <= 1; i++) {
            let c = base + vec2<f32>(f32(i), f32(j));
            let feature = c + vec2<f32>(hash21(c + vec2<f32>(0.3, 9.1)), hash21(c + vec2<f32>(5.7, -2.3)));
            let d = distance(p, feature);
            if (d < f1) {
                f2 = f1;
                f1 = d;
                id = c;
            } else if (d < f2) {
                f2 = d;
            }
        }
    }
    return Cell(f1, f2, id);
}

struct Crust {
    // 1 on a crack between plates.
    crack: f32,
    // 1 on frost: the patches (ice) or the fine flakes.
    flake: f32,
    // 1 on a fine frost flake alone (the snow look).
    fine: f32,
    // Plate tilt as a world-XZ slope vector.
    tilt: vec2<f32>,
    // Sun visibility from the neighbouring plates (lifted edges cast short shadows).
    shadow: f32,
}

// Ice crust of broken plates (~22 cm) with frost flakes (~6 cm patches). Plates are slightly
// tilted, cracks are dark, and a plate edge higher than the point toward the sun shadows it.
fn near_crust(xz: vec2<f32>, to_sun: vec3<f32>, footprint: f32) -> Crust {
    let plate_m = 0.22;
    let resolve = 1.0 - smoothstep01((footprint - plate_m * 0.04) / (plate_m * 0.2));
    if (resolve <= 0.0 && footprint > 0.3) {
        return Crust(0.0, 0.0, 0.0, vec2<f32>(0.0), 1.0);
    }
    let cell = worley(xz / plate_m);
    let edge = cell.f2 - cell.f1;
    let crack_w = max(0.06, footprint / plate_m * 1.5);
    // Only some plate boundaries are open cracks; the rest are sintered shut.
    let open = smoothstep01((fbm(xz / 0.9 + vec2<f32>(3.1, -7.4)) - 0.45) / 0.2);
    let crack = (1.0 - smoothstep01(edge / crack_w)) * resolve * open;
    // Plates settle a few degrees off level; a larger random tilt lit every plate as its own
    // facet (cobblestones at 3–8 m).
    let tilt = (vec2<f32>(hash21(cell.id + vec2<f32>(1.3, 4.4)), hash21(cell.id + vec2<f32>(-6.2, 2.8))) - 0.5) * 0.1;
    // Frost flakes are one continuous field across the plates (a per-plate offset printed
    // every plate as its own tile: cobblestones), in soft-edged blotches.
    let flake_n = 0.65 * fbm(xz / 0.11 + vec2<f32>(4.2, 1.7)) + 0.35 * fbm(xz / 0.035 + vec2<f32>(-2.6, 9.3));
    let fine_flake = smoothstep01((flake_n - 0.42) / 0.22) * resolve;
    // Wind-packed frost patches 0.3–1.5 m across, edged by the fine flakes (the UE
    // bore_twelve crust: white patches on darker blue-grey ice). They resolve from much
    // farther than the plates.
    let blotch_res = 1.0 - smoothstep01((footprint - 0.05) / 0.25);
    let blotch_n = 0.6 * fbm(xz / 0.85 + vec2<f32>(-7.3, 2.9)) + 0.4 * fbm(xz / 0.3 + vec2<f32>(5.1, -3.6))
        + 0.5 * (flake_n - 0.5) * resolve;
    let blotch = smoothstep01((blotch_n - 0.55) / 0.08) * blotch_res;
    let flake = max(fine_flake * 0.6, blotch);
    // A plate tilted away from a low sun is shadowed along the crack on its sun side (the
    // neighbour's raised edge), without a second cell search.
    let sun_xz = to_sun.xz / max(length(to_sun.xz), 1e-4);
    let away = clamp(-dot(tilt, sun_xz) * 12.0, 0.0, 1.0) * open;
    let grazing = 1.0 - smoothstep01((to_sun.y - 0.15) / 0.5);
    let shadow = 1.0 - 0.6 * away * grazing * (1.0 - smoothstep01(edge / (crack_w * 3.0))) * resolve;
    return Crust(crack, flake, fine_flake, tilt * resolve, shadow);
}

struct MidRelief {
    // Normal perturbation as a world-XZ slope vector.
    slope: vec2<f32>,
    // 1 in a hollow, 0 on a rise.
    hollow: f32,
    // Signed relief: +1 in a hollow, −1 on a rise.
    relief: f32,
    // Sun visibility from two taps toward the sun.
    shadow: f32,
}

// Height of the mid-range relief, metres: 6 m swells with pits and 2.2 m ridged rubble.
// Octaves fade out before they drop below about two pixels.
fn mid_height(p: vec2<f32>, footprint: f32) -> f32 {
    let f1 = 1.0 - smoothstep01((footprint - 0.6) / 1.2);
    let f2 = 1.0 - smoothstep01((footprint - 0.2) / 0.45);
    let f3 = 1.0 - smoothstep01((footprint - 0.06) / 0.14);
    let swell = (vnoise(p / 6.0 + vec2<f32>(3.7, -1.3)) - 0.5) * 1.1 * f1;
    let rubble = (0.5 - abs(vnoise(p / 2.2 + vec2<f32>(-8.1, 5.2)) - 0.5)) * 0.8 * f2;
    // Pitted grit at ~0.8 m (non-periodic: a sine ripple here read as a regular stripe
    // pattern on every dune).
    // Round 6: the finest octaves carry UE's 7 px-scale crunch at 20–200 m (DO27
    // bore_twelve_mid fine detail measured 0.052 against UE's 0.124): sharper, ridged grit
    // at 0.8 m and pebble-scale pits at 0.32 m.
    var grit = 0.0;
    if (f3 > 0.0) {
        grit = (0.5 - abs(vnoise(p / 0.8 + vec2<f32>(1.9, 7.3)) - 0.5)) * 0.32 * f3;
    }
    let f4 = 1.0 - smoothstep01((footprint - 0.025) / 0.06);
    if (f4 > 0.0) {
        grit += (vnoise(p / 0.32 + vec2<f32>(-4.4, 2.6)) - 0.5) * 0.12 * f4;
    }
    return swell + rubble + grit;
}

fn mid_relief(xz: vec2<f32>, to_sun: vec3<f32>, footprint: f32) -> MidRelief {
    // Difference step follows the pixel, so the finest octaves still shade (0.08–0.4 m).
    let e = clamp(footprint * 1.5, 0.08, 0.4);
    let h0 = mid_height(xz, footprint);
    let hx = mid_height(xz + vec2<f32>(e, 0.0), footprint);
    let hz = mid_height(xz + vec2<f32>(0.0, e), footprint);
    let slope = -vec2<f32>(hx - h0, hz - h0) / e;
    let hollow = smoothstep01((-h0 - 0.05) / 0.25);
    // Signed: +1 deep in a hollow, −1 on a rise (zero-mean over the noise).
    let relief = clamp(-h0 / 0.45, -1.0, 1.0);
    // Two taps toward the sun (1.5 m and 4.5 m): a rise between the point and the sun
    // shades it (soft, the same penumbra slope as the heightfield march).
    let horiz = max(length(to_sun.xz), 1e-4);
    let dir = to_sun.xz / horiz;
    let rise = to_sun.y / horiz;
    var vis = 1.0;
    for (var i = 0; i < 2; i++) {
        let d = select(4.5, 1.5, i == 0);
        let dh = mid_height(xz + dir * d, footprint) - h0;
        vis = min(vis, clamp((rise * d - dh) / (d * 0.08) + 0.5, 0.0, 1.0));
    }
    return MidRelief(slope, hollow, relief, 1.0 - 0.75 * (1.0 - vis));
}

struct Grain {
    // Albedo multiplier (grain tone, darker pebbles).
    albedo: f32,
    // Normal perturbation as a world-XZ slope vector.
    slope: vec2<f32>,
    // Sun visibility (pebbles shadow the ground behind them).
    shadow: f32,
}

// Gradient noise (Perlin, "Improving Noise", SIGGRAPH 2002: random unit gradients, quintic
// fade) with its analytic derivative (Quilez, "Gradient noise derivatives", 2017):
// x = value (σ ≈ 0.22, about ±0.7), yz = d/dp.
fn gnoise_d(p: vec2<f32>) -> vec3<f32> {
    let i = floor(p);
    let f = p - i;
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let du = 30.0 * f * f * (f * (f - 2.0) + 1.0);
    let aa = hash21(i + vec2<f32>(0.5, 3.1)) * 6.2831853;
    let ab = hash21(i + vec2<f32>(1.5, 3.1)) * 6.2831853;
    let ac = hash21(i + vec2<f32>(0.5, 4.1)) * 6.2831853;
    let ad = hash21(i + vec2<f32>(1.5, 4.1)) * 6.2831853;
    let ga = vec2<f32>(cos(aa), sin(aa));
    let gb = vec2<f32>(cos(ab), sin(ab));
    let gc = vec2<f32>(cos(ac), sin(ac));
    let gd = vec2<f32>(cos(ad), sin(ad));
    let va = dot(ga, f);
    let vb = dot(gb, f - vec2<f32>(1.0, 0.0));
    let vc = dot(gc, f - vec2<f32>(0.0, 1.0));
    let vd = dot(gd, f - vec2<f32>(1.0, 1.0));
    let k = va - vb - vc + vd;
    let value = va + u.x * (vb - va) + u.y * (vc - va) + u.x * u.y * k;
    let grad = ga + u.x * (gb - ga) + u.y * (gc - ga) + u.x * u.y * (ga - gb - gc + gd)
        + du * (u.yx * k + vec2<f32>(vb, vc) - va);
    return vec3<f32>(value, grad);
}

// Fine mineral grain (1.5 cm and 5 cm gradient noise) with sparse pebbles (1–3.5 cm domes
// in 12 cm cells). Each scale fades before it is smaller than a few pixels.
//
// The grain was value noise differenced a quarter cell each way: value noise with a
// smoothstep fade has zero slope across every lattice line, so its normal shaded a square
// grid (5 cm cells, ~30 px at 1440p on DO27's Switchbacks foreground) that the sharper
// motion-look resolve made plain. Gradient noise has a random slope at the lattice points
// and each octave's lattice is rotated off the world axes (and off the other octave's).
// Amplitudes match the old RMS slope (0.225) and tone spread (σ 0.215).
fn near_grain(xz: vec2<f32>, to_sun: vec3<f32>, footprint: f32) -> Grain {
    var slope = vec2<f32>(0.0);
    var tone = 1.0;
    for (var o = 0; o < 2; o++) {
        let s = select(0.05, 0.015, o == 1);
        let r = 1.0 - smoothstep01((footprint - s * 0.15) / (s * 0.4));
        if (r <= 0.0) {
            continue;
        }
        let cs = select(vec2<f32>(0.8776, 0.4794), vec2<f32>(0.4536, 0.8912), o == 1);
        let q = rotate2(xz / s, cs) + vec2<f32>(f32(o) * 3.3, 1.9);
        let n = gnoise_d(q);
        slope += unrotate(n.yz, cs) * 0.36 * r;
        tone *= 1.0 + n.x * 0.32 * r;
    }
    // Pebbles.
    let pebble_cell = 0.12;
    let pr = 1.0 - smoothstep01((footprint - 0.004) / 0.012);
    var shadow = 1.0;
    if (pr > 0.0) {
        let c = floor(xz / pebble_cell);
        if (hash21(c + vec2<f32>(2.1, 7.7)) < 0.34) {
            let centre = (c + vec2<f32>(0.3, 0.3) + 0.4 * vec2<f32>(hash21(c + vec2<f32>(4.0, 1.0)), hash21(c + vec2<f32>(9.0, 3.0)))) * pebble_cell;
            let radius = mix(0.01, 0.035, hash21(c + vec2<f32>(6.6, 6.6)));
            let d = xz - centre;
            let r = length(d) / radius;
            if (r < 1.0) {
                // Dome: slope grows toward the rim.
                slope += d / max(length(d), 1e-5) * (r / sqrt(max(1.0 - r * r, 0.05))) * 0.6 * pr;
                tone *= mix(1.0, 0.62 + 0.25 * hash21(c), pr);
            } else {
                // Shadow behind the pebble, on the side away from the sun.
                let sun_xz = to_sun.xz / max(length(to_sun.xz), 1e-4);
                let back = dot(d / max(length(d), 1e-5), -sun_xz);
                let reach = radius * (1.0 + 1.5 * (1.0 - smoothstep01((to_sun.y - 0.1) / 0.6)));
                if (back > 0.5 && length(d) < radius + reach) {
                    shadow = 1.0 - 0.55 * pr;
                }
            }
        }
    }
    return Grain(tone, slope, shadow);
}

// ---------------------------------------------------------------------------
// Cliff relief: real geometry on steep faces (terrain-look round 8, ADR-075)
// ---------------------------------------------------------------------------

// Strata bed coordinate at a point on the heightfield: `y` the bed coordinate (beds of
// `strata.y` metres, dipping and warped), `bed` the bed index, `f` the position inside the bed
// (0 bottom, 1 top), and the dip and warp offsets (metres). Shared by the bed shading and the
// ledge geometry.
struct Bed {
    y: f32,
    bed: f32,
    f: f32,
    warp: f32,
    dip: f32,
};

fn strata_bed(p: vec3<f32>) -> Bed {
    let thick = max(material.strata.y, 0.1);
    let drift = material.strata2.z;
    let dip = dot(p.xz, material.strata2.xy);
    let warp = drift * (fbm(p.xz / 170.0 + vec2<f32>(5.3, -1.7)) - 0.5) * 2.0
        + (drift * 0.45 + material.strata.z) * (fbm(p.xz / 37.0 + vec2<f32>(-3.1, 8.4)) - 0.5) * 2.0
        + thick * 0.35 * (fbm(p.xz / 3.1 + vec2<f32>(-13.1, 2.2)) - 0.5) * 2.0;
    let y = (p.y + dip + warp) / thick;
    // Irregular bed thickness: stretch the bed coordinate with slow 1-D noise.
    let yb = y + 0.55 * vnoise1(y * 0.29 + 1.7);
    let bed = floor(yb);
    return Bed(y, bed, yb - bed, warp, dip);
}

// Where along a hard bed it protrudes (0..1), ~9 m segments. World XZ: a steep face's XZ
// trace runs along the face, so the segments run along it whatever the face's normal.
fn ledge_segment(p: vec3<f32>, bed: f32) -> f32 {
    return smoothstep01((fbm(p.xz / 9.0 + vec2<f32>(bed * 3.7, bed * -1.3)) - 0.38) / 0.24);
}

// Three-dimensional value noise (visual detail only): `hash21` on the cell's XY, offset by Z.
fn hash31(i: vec3<f32>) -> f32 {
    return hash21(i.xy + vec2<f32>(i.z * 37.17, i.z * -17.31));
}

fn vnoise3(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = p - i;
    let u = f * f * (3.0 - 2.0 * f);
    let a = mix(
        mix(hash31(i), hash31(i + vec3<f32>(1.0, 0.0, 0.0)), u.x),
        mix(hash31(i + vec3<f32>(0.0, 1.0, 0.0)), hash31(i + vec3<f32>(1.0, 1.0, 0.0)), u.x),
        u.y,
    );
    let b = mix(
        mix(hash31(i + vec3<f32>(0.0, 0.0, 1.0)), hash31(i + vec3<f32>(1.0, 0.0, 1.0)), u.x),
        mix(hash31(i + vec3<f32>(0.0, 1.0, 1.0)), hash31(i + vec3<f32>(1.0, 1.0, 1.0)), u.x),
        u.y,
    );
    return mix(a, b, u.z);
}

// Ice-cliff flutes (0..1): vertical columns ~1.6 m apart (XZ only, so they run straight down
// a vertical face).
fn cliff_flute(p: vec3<f32>) -> f32 {
    return 0.6 * vnoise(p.xz / 1.6 + vec2<f32>(2.3, 4.1)) + 0.4 * vnoise(p.xz / 0.6 + vec2<f32>(3.1, -7.7));
}

// Outward relief of a steep face at heightfield point `p`, metres along the face normal.
// The vertex shader displaces by it at the vertex spacing and the colour pass shades with its
// gradient at the pixel footprint, so silhouette and shading agree. Each term fades out before
// its wavelength drops under 4× `spacing` (no aliasing at any LOD). `big` scales buttresses
// and fractured blocks, `small` the ledges and flutes (the strata shading's own scale).
// `icy` picks ice flutes over rock ledges.
fn cliff_relief(p: vec3<f32>, spacing: f32, icy: f32, big: f32, small: f32) -> f32 {
    return cliff_relief_at(p, spacing, icy, big, small, false, 0.0, 0.0, 0.0, 0.0);
}

// `cliff_relief` with the strata bed index and position (`bed`, `bed_f`), its ledge segment
// and the ice flutes at `p` supplied when the caller already has them (`have`:
// `surface_rest`'s strata shading at the pixel), instead of evaluating the same noise again.
// (Scalars, not a struct local: ADR-026.)
fn cliff_relief_at(p: vec3<f32>, spacing: f32, icy: f32, big: f32, small: f32, have: bool, bed: f32, bed_f: f32, segment: f32, flute: f32) -> f32 {
    let sp = max(spacing, 1e-4);
    // Leaning a little with height, so spurs and gullies are not plumb.
    let q = p.xz + vec2<f32>(0.23, -0.19) * p.y;
    var d = 0.0;
    if (big > 0.0) {
        // Buttresses and gullies ~16 m apart (ridged: sharp spurs, rounded gullies).
        let f1 = smoothstep01((16.0 / sp - 4.0) / 4.0);
        if (f1 > 0.0) {
            d += big * 1.4 * (ridged(q / 16.0 + vec2<f32>(7.3, -2.9)) - 0.62) * 2.0 * f1;
        }
        // Bulges and recesses ~11 m (3-D: they vary down the face too, so a cliff's profile
        // against the sky is not a straight line).
        let f2 = smoothstep01((11.0 / sp - 4.0) / 4.0);
        if (f2 > 0.0) {
            d += big * 1.6 * (vnoise3(p / 11.0 + vec3<f32>(3.7, 1.3, -8.1)) - 0.5) * 2.0 * f2;
        }
        // Fractured blocks ~4.5 m (3-D ridged: sharp edges).
        let f3 = smoothstep01((4.5 / sp - 4.0) / 4.0);
        if (f3 > 0.0) {
            let r = 1.0 - abs(
                (0.6667 * vnoise3(p / 4.5 + vec3<f32>(-4.1, 12.7, 2.2))
                    + 0.3333 * vnoise3(p / 2.1 + vec3<f32>(9.4, -3.3, 5.8))) * 2.0 - 1.0,
            );
            d += big * 0.7 * (r - 0.62) * 2.0 * f3;
        }
    }
    if (small > 0.0) {
        // Rock: hard beds protrude as broken ledges (lit top, shaded underside).
        let thick = max(material.strata.y, 0.1);
        let fl = smoothstep01((thick / sp - 3.0) / 3.0) * (1.0 - icy);
        let amp = 0.1 * thick * clamp(material.strata.w, 0.0, 2.0) * clamp(material.strata.x, 0.0, 1.0);
        if (fl > 0.0 && amp > 0.0) {
            var b_bed = bed;
            var b_f = bed_f;
            var seg = segment;
            if (!have) {
                let b = strata_bed(p);
                b_bed = b.bed;
                b_f = b.f;
                seg = ledge_segment(p, b.bed);
            }
            let hard = step(0.5, hash21(vec2<f32>(b_bed, 9.1)));
            let profile = smoothstep01(b_f / 0.14) * (1.0 - smoothstep01((b_f - 0.8) / 0.15));
            d += small * amp * hard * seg * profile * fl;
        }
        // Ice: flutes.
        let f3 = smoothstep01((1.6 / sp - 4.0) / 4.0) * icy * clamp(material.strata.x, 0.0, 1.0);
        if (f3 > 0.0) {
            var fl_t = flute;
            if (!have) {
                fl_t = cliff_flute(p);
            }
            d += small * 0.12 * (fl_t - 0.5) * 2.0 * f3;
        }
    }
    return d;
}

// Below the hover drive limit (rise/run 4, ~76°) a hull may press against or slide down a
// face, so relief there only carves into the rock (0 to −2 m · strength, no outward block);
// above it the full ± relief returns over the next 6° (no seam). `w` is the relief weight
// (slope gate × strength; 0 = shading-only relief, returned unshaped), `slope` the face's
// slope from up.
const CLIFF_FULL_RELIEF_SLOPE: f32 = 1.3258177; // atan(4)
fn cliff_shape(d: f32, w: f32, slope: f32) -> f32 {
    if (w <= 0.0) {
        return d;
    }
    // −w·(1 − tanh(1.6·d / w)): −w at d = 0, 1.6× the relief's own variation there, never
    // outside the surface (→ 0) or deeper than 2w (→ −2w), and no plateaus from a clamp.
    let inward = -w * (1.0 - tanh(1.6 * d / w));
    let full = smoothstep01((slope - CLIFF_FULL_RELIEF_SLOPE) / 0.105);
    return mix(inward, d, full);
}

// How icy (neutral-to-cool) the layer blend at this slope and altitude is: the fragment's
// `icy_face` test on the layers' mean albedo (each texture's last mip), for the vertex shader.
fn layer_icy(slope: f32, altitude: f32) -> f32 {
    let count = i32(clamp(layers.count.x, 0.0, 8.0));
    let top = f32(textureNumLevels(layer_albedo) - 1u);
    var rgb = vec3<f32>(0.0);
    var total = 0.0;
    for (var i = 0; i < count; i++) {
        let slot = layers.slots[i];
        let wi = max(
            slope_band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y)
                * altitude_band(altitude, slot.slope_alt.z, slot.slope_alt.w, slot.params.y),
            0.0,
        );
        if (wi <= 0.0) {
            continue;
        }
        var a = slot.albedo_roughness.rgb;
        if (slot.params2.w > 0.5) {
            a *= textureSampleLevel(layer_albedo, material_samp, vec2<f32>(0.5), i, top).rgb;
        }
        rgb += a * wi;
        total += wi;
    }
    if (total <= 1e-5) {
        return 0.0;
    }
    rgb /= total;
    return smoothstep01((rgb.b / max(rgb.r, 1e-4) - 0.72) / 0.13);
}

struct VOut {
    @invariant @builtin(position) position: vec4<f32>,
    // Drawn (displaced) world position.
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) @interpolate(flat) level: f32,
    // Pyramid level the vertex heights were read from (interpolated across the triangle).
    @location(3) geo_mip: f32,
    // The point on the heightfield before cliff relief (the shading reads the terrain here).
    @location(4) base: vec3<f32>,
    // x = the vertex's icy estimate (`layer_icy`), y = its cliff-relief weight.
    @location(5) cliff: vec2<f32>,
    // The pre-morph grid position (the node's own grid, before both morphs): the lattice hole
    // windows are tested here (`in_hole`, ADR-8470).
    @location(6) ref_xz: vec2<f32>,
};

// A CDLOD vertex: `world` drawn, `base` on the (prefiltered) heightfield, `m` the pyramid
// level its height was read from, `icy` and `w` (cliff-relief weight) for the shading.
struct Vtx {
    world: vec3<f32>,
    base: vec3<f32>,
    m: f32,
    icy: f32,
    w: f32,
    // Pre-morph grid position (`VOut::ref_xz`).
    ref_xz: vec2<f32>,
};

// CDLOD grid position after the parent-grid morph, height read from the pyramid level
// that matches the vertex cell (`height_mips::vertex_mip`): max(0, log2(cell / texel) + t).
// A fully morphed vertex reads its parent's level, so the crack-free guarantee holds.
// Shared by the colour, depth, cascade-shadow and velocity entry points so the
// silhouette matches.
fn displaced_world(vi: u32, node: Node) -> vec3<f32> {
    return displaced(vi, node, frame.camera_pos_time.xz).world;
}

// `eye_xz` is the morph centre: the camera, or the eye a shadow cascade's list was
// selected from (the morph must match the selection or neighbouring levels crack).
//
// Fine-grid nodes (`grid_size` twice the terrain's, chosen on the CPU for steep ground near
// the camera) first morph toward the regular grid — fully on the node's border, beyond the
// fine radius, where the parent morph starts and on gentle ground — so their border is the
// regular grid exactly (no cracks against regular neighbours), switching grids is seamless,
// and driveable ground is drawn exactly as `rendered_height` has it.
//
// Cliff relief then moves the vertex along the heightfield normal by `cliff_relief`. Every
// input (the morphed XZ, the pyramid level, the height and normal read there) is the same on
// a vertex two nodes share, so the displaced border still matches.
fn displaced(vi: u32, node: Node, eye_xz: vec2<f32>) -> Vtx {
    if (vi >= BAND_VERTEX_BASE) {
        return band_vertex(vi - BAND_VERTEX_BASE);
    }
    let line = u32(node.grid_size) + 1u;
    let gx = vi % line;
    let gz = vi / line;
    let mg = morph_grid(vec2<f32>(f32(gx), f32(gz)), node, eye_xz);
    // The node's own grid point before either morph: affine across every drawn triangle.
    let ref_xz = node.rect_min + vec2<f32>(f32(gx), f32(gz)) * (node.rect_size / node.grid_size);
    return vertex_at_relief(mg.xz, mg.m, ref_xz);
}

// Mesh-layer mouth bands (ADR-8569): a draw whose `vertex_index` starts at `BAND_VERTEX_BASE`
// (`gpu::BAND_VERTEX_BASE`) draws band triangles, not the node grid. The hole words carry them:
// holes[1] = the band's word offset `b`, holes[2] = its corner count `c`, holes[3] = its vertex
// count `v`; then `c` corner indices at `b`, then `BAND_CAMERAS * BAND_PASSES` sets of `v` xyz
// positions (f32 bits) at `b + c`, placed on the CPU on each camera's and pass's own selection and
// morph eye (`MouthBands::place`; ADR-8824). Corner `k` of set `p` is
// `vertex_index - BAND_VERTEX_BASE = p * c + k`.
// The vertex is its own heightfield point and reference position: the band pipelines are the
// plain (no hole cut) ones, and relief, morph and the pyramid level do not apply.
const BAND_VERTEX_BASE: u32 = 16777216u;

fn band_vertex(rel: u32) -> Vtx {
    let corners = max(holes[2], 1u);
    let pass_set = rel / corners;
    let b = holes[1];
    let o = b + corners + (pass_set * holes[3] + holes[b + rel - pass_set * corners]) * 3u;
    let p = vec3<f32>(bitcast<f32>(holes[o]), bitcast<f32>(holes[o + 1u]), bitcast<f32>(holes[o + 2u]));
    return Vtx(p, p, 0.0, 0.0, 0.0, p.xz);
}

// A CDLOD vertex after both morphs: world XZ and the pyramid level its height is read at.
struct Morphed {
    xz: vec2<f32>,
    m: f32,
};

// The morph part of `displaced` for grid coordinate `g_in` (no height reads on the regular
// grid): the terrain virtual texture's bake walks the morphed grid with it to find the
// triangle a texel lies on.
fn morph_grid(g_in: vec2<f32>, node: Node, eye_xz: vec2<f32>) -> Morphed {
    var g = g_in;
    var step_w = node.rect_size / node.grid_size;
    var fine_mip = 0.0;
    if (node.grid_size > terrain.grid_size + 0.5) {
        let gmax = node.grid_size;
        let edge = min(min(g.x, g.y), min(gmax - g.x, gmax - g.y));
        let own = node.rect_min + g * step_w;
        let d0 = distance(eye_xz, own);
        var a = 1.0 - clamp((edge - 1.0) / 3.0, 0.0, 1.0);
        a = max(a, smoothstep01((d0 - (material.cliff.y - material.cliff.z)) / max(material.cliff.z, 1e-3)));
        if (node.morph_end > node.morph_start) {
            a = max(a, clamp((d0 - node.morph_start) / (node.morph_end - node.morph_start) * 4.0, 0.0, 1.0));
        }
        if (a < 1.0) {
            let m_reg = clamp(log2(max(step_w * 2.0, 1e-6) / texel_m()), 0.0, 12.0);
            let nn = normal_mip(own, m_reg);
            let sl = acos(clamp(nn.y, -1.0, 1.0));
            a = max(a, 1.0 - smoothstep01((sl - (material.cliff.w - 0.1)) / 0.1));
        }
        let c = floor((g + vec2<f32>(1.0)) * 0.5) * 2.0;
        g = (g + (c - g) * a) * 0.5;
        step_w = step_w * 2.0;
        fine_mip = a - 1.0;
    }

    // Morph toward the parent grid: grid coordinate rounds to the nearest even coordinate.
    let coarse = floor((g + vec2<f32>(1.0)) * 0.5) * 2.0;
    let fine_world = node.rect_min + g * step_w;
    let d = distance(eye_xz, fine_world);
    var t = 0.0;
    if (node.morph_end > node.morph_start) {
        t = clamp((d - node.morph_start) / (node.morph_end - node.morph_start), 0.0, 1.0);
    }
    let gm = g + (coarse - g) * t;
    var world_xz = node.rect_min + gm * step_w;
    // A rectangular map's quadtree square overhangs the shorter side (ADR-8369). Vertices of
    // a node that straddles that side sit on the map edge. A square map keeps the line above.
    if (terrain.depth.y != terrain.world_width) {
        let lo = vec2<f32>(-terrain.half_width, -terrain.depth.x);
        let hi = vec2<f32>(terrain.world_width - terrain.half_width, terrain.depth.y - terrain.depth.x);
        world_xz = clamp(world_xz, lo, hi);
    }
    let m = clamp(log2(max(step_w, 1e-6) / texel_m()) + t + fine_mip, 0.0, 12.0);
    return Morphed(world_xz, m);
}

// A vertex's height at pyramid level `m`, its cliff-relief weight `w` and icy estimate, and
// the heightfield normal and slope there (`n`, `sl`: read only when cliff relief is on).
struct VtxCore {
    h: f32,
    n: vec3<f32>,
    sl: f32,
    icy: f32,
    w: f32,
    spacing: f32,
};

fn vertex_core(world_xz: vec2<f32>, m: f32) -> VtxCore {
    let h = height_w_mip(world_xz, m);
    var n = vec3<f32>(0.0, 1.0, 0.0);
    var sl = 0.0;
    var icy = 0.0;
    var w = 0.0;
    let spacing = texel_m() * exp2(m);
    if (material.cliff.x > 0.0 && material.params0.x >= 0.5 && spacing < 4.0) {
        n = normal_mip(world_xz, m);
        sl = acos(clamp(n.y, -1.0, 1.0));
        w = smoothstep01((sl - material.cliff.w) / 0.3) * clamp(material.cliff.x, 0.0, 2.0);
        if (w > 0.0) {
            icy = layer_icy(sl, h);
        }
    }
    return VtxCore(h, n, sl, icy, w, spacing);
}

// The rest of `displaced`: the height at pyramid level `m` and the cliff relief.
fn vertex_at(world_xz: vec2<f32>, m: f32) -> Vtx {
    let c = vertex_core(world_xz, m);
    let base = vec3<f32>(world_xz.x, c.h, world_xz.y);
    var world = base;
    if (c.w > 0.0) {
        world = base + c.n * cliff_shape(cliff_relief(base, c.spacing, c.icy, c.w, c.w), c.w, c.sl);
    }
    return Vtx(world, base, m, c.icy, c.w, world_xz);
}

// `vertex_at` for the terrain's own vertex entry points (`displaced`): no cliff relief where the
// pre-morph grid position `ref_xz` is within two lattice cells of a mesh-layer mouth's cut
// (`lattice_near_ring`, ADR-8470), so the drawn ring stays on the lattice ring the band is welded
// to. Kept apart from `vertex_at` so shaders that embed this file without the holes buffer
// (grass, impostors) never reach it.
fn vertex_at_relief(world_xz: vec2<f32>, m: f32, ref_xz: vec2<f32>) -> Vtx {
    let c = vertex_core(world_xz, m);
    var w = c.w;
    if (w > 0.0 && lattice_near_ring(ref_xz)) {
        w = 0.0;
    }
    let base = vec3<f32>(world_xz.x, c.h, world_xz.y);
    var world = base;
    if (w > 0.0) {
        world = base + c.n * cliff_shape(cliff_relief(base, c.spacing, c.icy, w, w), w, c.sl);
    }
    return Vtx(world, base, m, c.icy, w, ref_xz);
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
    let node = load_node(ii);
    let d = displaced(vi, node, frame.camera_pos_time.xz);
    let world = d.world;
    // Normal of the same prefiltered surface the vertex sits on (nearest pyramid level: the
    // photoreal path recomputes the shading normal per fragment; this one feeds the legacy
    // path and the shadow normal offset).
    // The photoreal path recomputes the normal per fragment and takes the shadow offset
    // normal from screen derivatives, so it skips these taps (16 loads per vertex).
    // The terrain budget's far vertex normal needs it too.
    var n = vec3<f32>(0.0, 1.0, 0.0);
    if (material.params0.x < 0.5 || budget_on(4u)) {
        n = normal_level(d.base.xz, u32(round(d.m)));
    }
    return VOut(frame.view_proj * vec4<f32>(world, 1.0), world, n, node.level, d.m, d.base, vec2<f32>(d.icy, d.w), d.ref_xz);
}

struct PosOut {
    @invariant @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_depth(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> PosOut {
    let world = displaced_world(vi, load_node(ii));
    return PosOut(frame.view_proj * vec4<f32>(world, 1.0));
}

@vertex
fn vs_shadow(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> PosOut {
    let eye = select(frame.camera_pos_time.xz, cascade_vp.morph_eye.xz, cascade_vp.morph_eye.w > 0.5);
    let world = displaced(vi, load_node(ii), eye).world;
    return PosOut(cascade_vp.light_view_proj * vec4<f32>(world, 1.0));
}

struct VelOut {
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) motion: vec2<f32>,
}

fn clip_uv(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

@vertex
fn vs_velocity(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VelOut {
    let world = displaced_world(vi, load_node(ii));
    let clip = frame.view_proj * vec4<f32>(world, 1.0);
    let current_uv = clip_uv(frame.unjittered_view_proj * vec4<f32>(world, 1.0));
    let prev_uv = clip_uv(frame.prev_view_proj * vec4<f32>(world, 1.0));
    return VelOut(clip, prev_uv - current_uv);
}

@fragment
fn fs_velocity(in: VelOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.motion, 0.0, 1.0);
}

// ---- Hole-cut variants (nodes a hole partly covers; `in_hole`) ----

struct HoleOut {
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) base_xz: vec2<f32>,
    @location(1) ref_xz: vec2<f32>,
}

@vertex
fn vs_depth_holes(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> HoleOut {
    let d = displaced(vi, load_node(ii), frame.camera_pos_time.xz);
    return HoleOut(frame.view_proj * vec4<f32>(d.world, 1.0), d.base.xz, d.ref_xz);
}

@vertex
fn vs_shadow_holes(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> HoleOut {
    let eye = select(frame.camera_pos_time.xz, cascade_vp.morph_eye.xz, cascade_vp.morph_eye.w > 0.5);
    let d = displaced(vi, load_node(ii), eye);
    return HoleOut(cascade_vp.light_view_proj * vec4<f32>(d.world, 1.0), d.base.xz, d.ref_xz);
}

// Depth-only passes (prepass, every sun cascade): the cut casts and occludes nothing.
@fragment
fn fs_holes(in: HoleOut) {
    if (in_hole(in.base_xz, in.ref_xz)) {
        discard;
    }
}

struct VelHoleOut {
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) motion: vec2<f32>,
    @location(1) base_xz: vec2<f32>,
    @location(2) ref_xz: vec2<f32>,
}

@vertex
fn vs_velocity_holes(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VelHoleOut {
    let d = displaced(vi, load_node(ii), frame.camera_pos_time.xz);
    let clip = frame.view_proj * vec4<f32>(d.world, 1.0);
    let current_uv = clip_uv(frame.unjittered_view_proj * vec4<f32>(d.world, 1.0));
    let prev_uv = clip_uv(frame.prev_view_proj * vec4<f32>(d.world, 1.0));
    return VelHoleOut(clip, prev_uv - current_uv, d.base.xz, d.ref_xz);
}

@fragment
fn fs_velocity_holes(in: VelHoleOut) -> @location(0) vec4<f32> {
    if (in_hole(in.base_xz, in.ref_xz)) {
        discard;
    }
    return vec4<f32>(in.motion, 0.0, 1.0);
}

// The terrain's view-independent surface at a point of the heightfield (photoreal path):
// layer blend, strata, relief, near and mid detail, macro tint, and the sun-dependent but
// view-independent relief shadows and canyon horizon. `footprint` is the shading footprint
// (metres: the pixel's, or a virtual-texture texel's), `dist` the distance it is shaded for,
// `geo_mip` the pyramid level of the geometry under it, `vertex_n` the vertex normal (terrain
// budget only) and `icy_v` the vertex's icy estimate (cliff relief). The colour pass calls it
// per pixel; the terrain virtual texture bakes it per texel.
struct Surf {
    albedo: vec3<f32>,
    roughness: f32,
    n_geo: vec3<f32>,
    slope: f32,
    n_shade: vec3<f32>,
    cavity: f32,
    w_first3: vec3<f32>,
    relief_shadow: f32,
    horizon_sky: f32,
    terrain_bounce: f32,
    indirect_ao: f32,
    // Baked heightfield AO from the macro map's alpha (DO27: cavity AO over 2–30 m), kept
    // apart from the layers' texture-scale ORM occlusion in `indirect_ao` (look-r2 round 10).
    macro_ao: f32,
    sparkle: f32,
    micro_shadow: f32,
    frost: f32,
    sky_visibility: f32,
    near_dbg: vec2<f32>,
    // The cliff relief's shading-normal gradient (`surface_rest`; the virtual texture caches it).
    cliff_grad: vec3<f32>,
    // The horizon's steepest slope per sector (`Horizon::slopes`; −1000 where it was not
    // computed), for the virtual texture.
    horizon_slopes: vec4<f32>,
    // What strata and talus multiplied the albedo by, and the fractured-rock normal
    // gradient (for the virtual texture's relief mode).
    rest_factor: vec3<f32>,
    rock_grad: vec3<f32>,
    // Whole-texture mean of `indirect_ao` (the layers' last ORM mip, blended and scaled like
    // it): what the texture's AO costs a flat surface on average (the probes divide it out).
    orm_mean: f32,
};

// `surface_photoreal`, or a flat grey stand-in for the surface-cost probe (debug 98).
fn surface_or_probe(probe: bool, cached: Surf, use_cached: bool, p: vec3<f32>, footprint: f32, dist: f32, geo_mip: f32, vertex_n: vec3<f32>, icy_v: f32) -> Surf {
    // The terrain virtual texture's surface for this pixel (`vt_lookup`), when it hit: the
    // whole surface, or (split mode) the terms of `surface_rest` it caches, the rest per pixel.
    if (use_cached) {
        // `slope` < 0 marks the split modes' cached terms (`vt_lookup`: −1 lighting, −2
        // relief; this path also serves pipelines without the texture's bindings).
        if (cached.slope >= 0.0) {
            return cached;
        }
        let mode = u32(-cached.slope + 0.5);
        return surface_rest(surface_layers(p, footprint, dist, geo_mip, vertex_n), p, footprint, dist, icy_v, mode, cached);
    }
    if (probe) {
        let n = normalize(vertex_n);
        return Surf(vec3<f32>(0.4), 0.9, n, acos(clamp(n.y, -1.0, 1.0)), n, 1.0, vec3<f32>(0.0), 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, vec2<f32>(0.0), vec3<f32>(0.0), vec4<f32>(-1000.0), vec3<f32>(1.0), vec3<f32>(0.0), 1.0);
    }
    return surface_photoreal(p, footprint, dist, geo_mip, vertex_n, icy_v);
}

// Debug views 78–89: one term of `surface_photoreal` switched off, for cost studies (the
// shaded result is wrong; `do27_chase` times the pass).
// 78 switches off every term the virtual texture's split mode caches (83, 86, 87, 88).
// 90 switches the stochastic anti-tiling off (one tap per layer), 91 the height blend (plain band
// weights), 92 both: the snow-tiling A/B (ADR-1428); both keep the rest of the shading.
// Both tests are false for every other view, and the shared colour pass compiles them as
// `false` (the `@cost-views` markers, `without_cost_views`) unless one of views 78–92 is on:
// testing the view in every pixel's layer loop cost the terrain forward pass 0.026 ms on the
// RTX 5090 (ADR-1491).
fn cost_off(view: f32) -> bool {
    // @cost-views-begin
    let v = frame.ambient_debug.a;
    let cached = v > 77.5 && v < 78.5 && (view == 83.0 || view == 86.0 || view == 87.0 || view == 88.0);
    return cached || (v > view - 0.5 && v < view + 0.5);
    // @cost-views-end
    // @cost-views-off return false;
}

// The snow-tiling A/B switches: view 92 turns both 90 and 91 off.
fn ab_off(view: f32) -> bool {
    // @cost-views-begin
    let v = frame.ambient_debug.a;
    return cost_off(view) || (v > 91.5 && v < 92.5);
    // @cost-views-end
    // @cost-views-off return false;
}

// The layer part of `surface_photoreal`: the shading normal at the footprint, the layer
// blend (band weights, height blend, stochastic and triplanar texture taps, micro normal,
// crust). Its texture detail is at the pixel's footprint; the terrain virtual texture keeps
// it per pixel and caches what `surface_rest` adds (ADR-117).
struct SurfLayers {
    albedo: vec3<f32>,
    roughness: f32,
    n_geo: vec3<f32>,
    slope: f32,
    n_shade: vec3<f32>,
    indirect_ao: f32,
    w_first3: vec3<f32>,
    // Whole-texture mean of `indirect_ao` (`Surf::orm_mean`).
    orm_mean: f32,
};

fn surface_layers(in_world: vec3<f32>, footprint: f32, dist: f32, geo_mip: f32, vertex_n: vec3<f32>) -> SurfLayers {
    var n_geo = normalize(vertex_n);
    var slope = acos(clamp(n_geo.y, -1.0, 1.0));
    let altitude = in_world.y;
    let count = i32(clamp(layers.count.x, 0.0, 8.0));
    var albedo = vec3<f32>(0.0);
    var roughness = 0.0;
    var n_shade = n_geo;
    var w_first3 = vec3<f32>(0.0);
    var indirect_ao = 1.0;
    // Shade with the heightfield's own normal at this pixel's footprint (any CDLOD level).
    // Terrain budget: past `vertex_normal_m` the vertex normal, blended in over a band.
    var t_vn = 0.0;
    if (budget_on(4u)) {
        t_vn = smoothstep01((dist - terrain.mip_info.w) / BUDGET_BAND_M);
    }
    if (t_vn < 1.0 && !cost_off(79.0)) {
        n_geo = fragment_normal(in_world.xz, footprint, geo_mip);
        if (t_vn > 0.0) {
            n_geo = normalize(mix(n_geo, normalize(vertex_n), t_vn));
        }
    } else {
        n_geo = normalize(vertex_n);
    }
    slope = acos(clamp(n_geo.y, -1.0, 1.0));
    n_shade = n_geo;

    // Terrain budget: past `far_layers_m` only the two strongest band weights, without
    // the height-texture blend, blended in over a band (no height reads past it).
    var t_fl = 0.0;
    if (budget_on(2u)) {
        t_fl = smoothstep01((dist - terrain.mip_info.z) / BUDGET_BAND_M);
    }

    // Pass 1: band weights (times the splat packs' weights, ADR-1402), then each covering
    // layer's height texture value.
    let layer_count = select(count, 0, cost_off(80.0));
    var wa = vec4<f32>(0.0);
    var wb = vec4<f32>(0.0);
    var ha = vec4<f32>(0.5);
    var hbk = vec4<f32>(0.5);
    var max_hb = 0.0;
    let packs = splat_packs(in_world.xz, footprint);
    let splat_a = packs.a;
    let splat_b = packs.b;
    for (var i = 0; i < layer_count; i++) {
        let slot = layers.slots[i];
        let ws = slope_band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
        let wl = altitude_band(altitude, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
        let wi = max(ws * wl, 0.0) * pick_weight(splat_a, splat_b, i);
        max_hb = max(max_hb, clamp(slot.params2.y, 0.0, 1.0));
        if (i < 4) {
            wa = with_lane(wa, i, wi);
        } else {
            wb = with_lane(wb, i - 4, wi);
        }
    }
    if (ab_off(91.0)) {
        max_hb = 0.0;
    }
    var total_w = dot(wa, vec4<f32>(1.0)) + dot(wb, vec4<f32>(1.0));
    let covered = total_w > 1e-5;
    if (!covered) {
        wa = vec4<f32>(1.0, 0.0, 0.0, 0.0);
        wb = vec4<f32>(0.0);
        total_w = 1.0;
    }
    wa = wa / total_w;
    wb = wb / total_w;
    total_w = 1.0;
    // A layer's height-blend key is its weight + hb·(h − ½) with h in 0..1, so a layer whose
    // highest possible key is under every other covering layer's lowest key by the blend
    // depth ends at weight 0 whatever its height: its height taps are skipped (exact; the
    // crust term below weights the height by that 0 too).
    let blend_on = max_hb > 0.001 && t_fl < 1.0;
    var key_floor = -1e3;
    if (blend_on) {
        for (var i = 0; i < layer_count; i++) {
            let wi = pick_weight(wa, wb, i);
            if (wi > 0.0) {
                key_floor = max(key_floor, wi - 0.5 * clamp(layers.slots[i].params2.y, 0.0, 1.0));
            }
        }
        key_floor -= mix(0.5, 0.12, max_hb);
    }
    for (var i = 0; i < layer_count; i++) {
        let wi = pick_weight(wa, wb, i);
        let slot = layers.slots[i];
        let hb = clamp(slot.params2.y, 0.0, 1.0);
        var hh = 0.5;
        if (covered && wi > 0.0 && t_fl < 1.0 && wi + 0.5 * hb > key_floor) {
            if (slot.params2.z > 0.5) {
                let lod = footprint_lod(footprint, slot.params.w);
                let st = stochastic_taps(in_world.xz, layer_cell(material.params0.z, slot.params.x));
                hh = tri_sample(layer_height, st, slot.params.x, i, lod, aniso_grads_top(lod)).r;
            } else if (hb > 0.0) {
                // No height texture: procedural stand-in.
                hh = fbm(in_world.xz * 0.35 + vec2<f32>(f32(i) * 17.13, f32(i) * -9.71));
            }
        }
        if (i < 4) {
            ha = with_lane(ha, i, hh);
        } else {
            hbk = with_lane(hbk, i - 4, hh);
        }
    }
    // The two strongest band weights, renormalised (terrain budget, past the band). The third strongest
    // weight is subtracted from every layer first (`max(w − w₃, 0)` leaves at most two layers, and a layer
    // that is overtaken by the third fades to 0 as it meets it), so the selection is continuous in the
    // weights. A hard "two largest" pick jumps wherever two weights cross: the splat packs' texel-level
    // noise then drew a stair-stepped outline around every painted patch from far away (look round 2,
    // finding 8; ADR-1446). Renormalising the survivors keeps each pixel's total at 1.
    var ra = vec4<f32>(0.0);
    var rb = vec4<f32>(0.0);
    if (t_fl > 0.0) {
        var i1 = 0;
        var i2 = -1;
        var w1 = -1.0;
        var w2 = -1.0;
        var w3 = -1.0;
        for (var i = 0; i < count; i++) {
            let wi = pick_weight(wa, wb, i);
            if (wi > w1) {
                i2 = i1;
                w3 = w2;
                w2 = w1;
                i1 = i;
                w1 = wi;
            } else if (wi > w2) {
                w3 = w2;
                i2 = i;
                w2 = wi;
            } else if (wi > w3) {
                w3 = wi;
            }
        }
        let floor_w = max(w3, 0.0);
        let r1 = max(w1 - floor_w, 0.0);
        let r2 = select(0.0, max(w2 - floor_w, 0.0), i2 >= 0);
        let sum = r1 + r2;
        // Equal weights down to the third strongest leave nothing: keep the strongest layer alone.
        let k1 = select(1.0, r1 / sum, sum > 1e-5);
        let k2 = select(0.0, r2 / sum, sum > 1e-5);
        for (var i = 0; i < count; i++) {
            var r = 0.0;
            if (i == i1) {
                r = k1;
            } else if (i == i2) {
                r = k2;
            }
            if (i < 4) {
                ra = with_lane(ra, i, r);
            } else {
                rb = with_lane(rb, i - 4, r);
            }
        }
    }
    // Height-based blend (Mishkinis 2013): each covering layer's key is its normalised
    // band weight plus `height_blend · (h − ½)`; only layers within `depth` of the
    // highest key share the pixel. The tallest texels of the overlapping layers win the
    // transition locally (snow settles in the cracks of the rock) instead of a global
    // contrast exponent snapping a whole band edge to one layer (the old pow(w, 1+4·hb)
    // turned soft altitude bands into hard contours). A zero-weight layer stays zero;
    // `height_blend` = 0 everywhere keeps the plain normalised weights.
    if (max_hb > 0.001 && t_fl < 1.0) {
        var ka = vec4<f32>(-1e3);
        var kb = vec4<f32>(-1e3);
        var kmax = -1e3;
        for (var i = 0; i < count; i++) {
            let wi = pick_weight(wa, wb, i);
            if (wi <= 0.0) {
                continue;
            }
            let hb = clamp(layers.slots[i].params2.y, 0.0, 1.0);
            let key = wi + hb * (pick_weight(ha, hbk, i) - 0.5);
            kmax = max(kmax, key);
            if (i < 4) {
                ka = with_lane(ka, i, key);
            } else {
                kb = with_lane(kb, i - 4, key);
            }
        }
        let depth = mix(0.5, 0.12, max_hb);
        let floor_key = vec4<f32>(kmax - depth);
        wa = max(ka - floor_key, vec4<f32>(0.0));
        wb = max(kb - floor_key, vec4<f32>(0.0));
        total_w = max(dot(wa, vec4<f32>(1.0)) + dot(wb, vec4<f32>(1.0)), 1e-5);
    }
    if (t_fl > 0.0) {
        wa = mix(wa / total_w, ra, t_fl);
        wb = mix(wb / total_w, rb, t_fl);
        total_w = 1.0;
    }
    // Layers under `LAYER_CUT` of the blend are dropped and the rest renormalised, so a
    // faint band tail does not cost a full set of texture taps; the weight still reaches 0
    // continuously (the largest of ≤ 8 weights is ≥ ⅛ of the total, over the cut).
    let cut = LAYER_CUT * total_w;
    wa = max(wa - vec4<f32>(cut), vec4<f32>(0.0));
    wb = max(wb - vec4<f32>(cut), vec4<f32>(0.0));
    total_w = max(dot(wa, vec4<f32>(1.0)) + dot(wb, vec4<f32>(1.0)), 1e-5);

    // Triplanar blend factor: per-layer flag or the automatic slope switch.
    let auto_tp = smoothstep01(
        (slope - material.params1.z) / 0.25,
    );

    let fade = 1.0 - smoothstep01(
        (dist - material.params1.x) / max(material.params1.y - material.params1.x, 1e-3),
    );

    // Pass 2: combine layer data (albedo, micro normal, triplanar sides). Micro normals
    // accumulate as a world-space slope vector `micro` (each plane's tangent XY mapped
    // to the world axes its UV follows), later projected onto the surface.
    var micro = vec3<f32>(0.0);
    var ao_sum = 0.0;
    var ao_mean_sum = 0.0;
    var ao_w = 0.0;
    var crust = 0.0;
    for (var i = 0; i < layer_count; i++) {
        let wi = pick_weight(wa, wb, i) / total_w;
        if (wi <= 0.001) {
            continue;
        }
        let slot = layers.slots[i];
        let lod = footprint_lod(footprint, slot.params.w);
        let g_top = aniso_grads_top(lod);
        let st = stochastic_taps(in_world.xz, layer_cell(material.params0.z, slot.params.x));
        crust += pick_weight(ha, hbk, i) * wi;

        var a_rgb = slot.albedo_roughness.rgb;
        // Top plane: texture U ↔ world X, V ↔ world Z.
        var m_world = vec3<f32>(0.0);
        if (slot.params2.w > 0.5) {
            // @ground-look-begin
            var tex = tri_sample(layer_albedo, st, slot.params.x, i, lod, g_top);
            if (material.ground.z > 0.0 && fade > 0.0) {
                tex = ground_remix(tex, in_world.xz, slot.params.x, i, lod, fade);
            }
            a_rgb = a_rgb * tex.rgb;
            // @ground-look-end
            // @ground-look-off a_rgb = a_rgb * tri_sample(layer_albedo, st, slot.params.x, i, lod, g_top).rgb;
            if (fade > 0.0 && slot.params2.x > 0.0) {
                let n_top = tri_normal(st, slot.params.x, i, lod, g_top);
                m_world = vec3<f32>(n_top.x, 0.0, n_top.y);
            }
        } else {
            // Legacy look for this layer: flat colour with the procedural mottle.
            let mottle = fbm(in_world.xz * 0.35 + vec2<f32>(f32(i) * 17.13, f32(i) * -9.71));
            a_rgb = a_rgb * (0.90 + 0.20 * mottle);
        }

        // Triplanar (Geiss, GPU Gems 3): weights are |N|, so the top plane stays
        // when the flag is on and the surface is flat. `tp` blends from top-only
        // (flag off, gentle slope) toward that three-plane result. Dropping the
        // side terms, or replacing the top with sides only, fails the tests.
        let tp = clamp(max(slot.params.z, auto_tp), 0.0, 1.0);
        if (tp > 0.01 && slot.params2.w > 0.5) {
            let side_u = vec2<f32>(in_world.z, -in_world.y) * slot.params.x;
            let side_v = vec2<f32>(in_world.x, in_world.y) * slot.params.x;
            // Plane weights |N| / Σ|N|; a plane under `TRIPLANAR_CUT` is dropped and the
            // rest renormalised (continuous; the largest is ≥ ⅓), so a side plane that
            // barely shows is not fetched.
            let an = abs(n_geo) / max(abs(n_geo.x) + abs(n_geo.y) + abs(n_geo.z), 1e-5);
            let aw = max(an - vec3<f32>(TRIPLANAR_CUT), vec3<f32>(0.0));
            let aw_sum = aw.x + aw.y + aw.z;
            // Side planes' footprint axes: U ↔ world Z, V ↔ −world Y (X-facing); U ↔ X, V ↔ Y.
            let g_u = aniso_grads(vec2<f32>(aniso_dx.z, -aniso_dx.y), vec2<f32>(aniso_dy.z, -aniso_dy.y), lod);
            let g_v = aniso_grads(aniso_dx.xy, aniso_dy.xy, lod);
            let no_rot = vec2<f32>(1.0, 0.0);
            var a_x = vec3<f32>(0.0);
            var a_z = vec3<f32>(0.0);
            if (aw.x > 0.0) {
                a_x = layer_tap(layer_albedo, side_u, i, lod, g_u, no_rot).rgb;
            }
            if (aw.z > 0.0) {
                a_z = layer_tap(layer_albedo, side_v, i, lod, g_v, no_rot).rgb;
            }
            let tri = a_rgb * (aw.y / aw_sum)
                + slot.albedo_roughness.rgb * a_x * (aw.x / aw_sum)
                + slot.albedo_roughness.rgb * a_z * (aw.z / aw_sum);
            a_rgb = mix(a_rgb, tri, tp);
            // X-facing plane: U ↔ world Z, V ↔ −world Y. Z-facing plane: U ↔ X, V ↔ Y.
            if (fade > 0.0 && slot.params2.x > 0.0) {
                var n_x = vec2<f32>(0.0);
                var n_z = vec2<f32>(0.0);
                if (aw.x > 0.0) {
                    n_x = layer_tap(layer_normal, side_u, i, lod, g_u, no_rot).rg * 2.0 - vec2<f32>(1.0);
                }
                if (aw.z > 0.0) {
                    n_z = layer_tap(layer_normal, side_v, i, lod, g_v, no_rot).rg * 2.0 - vec2<f32>(1.0);
                }
                let m_tri = m_world * (aw.y / aw_sum)
                    + vec3<f32>(0.0, -n_x.y, n_x.x) * (aw.x / aw_sum)
                    + vec3<f32>(n_z.x, n_z.y, 0.0) * (aw.z / aw_sum);
                m_world = mix(m_world, m_tri, tp);
            }
        }

        // glTF ORM (Khronos): R occlusion, G roughness. B (metallic) is never read here: terrain
        // is not metal; a layer may read it as porosity on wet frames (`terrain_orm_porosity`).
        var ao = 1.0;
        var ao_mean = 1.0;
        var rough = slot.albedo_roughness.a;
        if (slot.params3.x > 0.5) {
            let orm = tri_sample(layer_orm, st, slot.params.x, i, lod, g_top);
            ao = orm.r;
            rough = orm.g;
            let orm_top = f32(textureNumLevels(layer_orm) - 1u);
            ao_mean = textureSampleLevel(layer_orm, material_samp, vec2<f32>(0.5), i, orm_top).r;
        }

        albedo += a_rgb * wi;
        ao_sum += ao * wi;
        ao_mean_sum += ao_mean * wi;
        ao_w += wi;
        roughness += rough * wi;
        // Authored range is 0..=2 (`LayerDesc::detail_normal_strength`).
        micro += m_world * clamp(slot.params2.x, 0.0, 2.0) * wi;
        if (i == 0) {
            w_first3.x = wi;
        } else if (i == 1) {
            w_first3.y = wi;
        } else if (i == 2) {
            w_first3.z = wi;
        }
    }

    // `rock.w`: 1 − the layers' ORM occlusion strength (`TerrainRelief::layer_occlusion`).
    indirect_ao = mix(select(1.0, ao_sum / ao_w, ao_w > 1e-4), 1.0, clamp(material.rock.w, 0.0, 1.0));
    let orm_mean = mix(select(1.0, ao_mean_sum / ao_w, ao_w > 1e-4), 1.0, clamp(material.rock.w, 0.0, 1.0));

    // Micro normal: project the world slope vector onto the surface plane and tilt the
    // geometric normal by it (a normal map with z = 1), faded with distance — tiling
    // micro detail is only worth its cost up close. (Round 3 rebuilt the normal as
    // `t·x + n·|x|`, a 45° tilt for any non-zero micro slope, through a tangent frame
    // that flipped where |N.y| crossed 0.99.)
    let micro_t = micro - n_geo * dot(micro, n_geo);
    n_shade = normalize(n_geo + micro_t * fade);

    // Crust: the blended layer height darkens the low texels (cracks, pits between
    // grains) — the self-occlusion of relief the normal map alone cannot show.
    let crust_ao = clamp(0.62 + 0.76 * crust, 0.62, 1.0);
    albedo *= mix(1.0, crust_ao, clamp(material.relief.y, 0.0, 1.0) * fade);
    return SurfLayers(albedo, roughness, n_geo, slope, n_shade, indirect_ao, w_first3, orm_mean);
}

fn surface_photoreal(in_world: vec3<f32>, footprint: f32, dist: f32, geo_mip: f32, vertex_n: vec3<f32>, icy_v: f32) -> Surf {
    return surface_rest(surface_layers(in_world, footprint, dist, geo_mip, vertex_n), in_world, footprint, dist, icy_v, 0u, no_surf());
}

// Layer of macro tile `flat_i` in the resident tile array, or -1 when the tile is not
// resident (ADR-3367): its bit in the 256-bit `macro_mask` and its rank among the resident
// tiles (the words' running counts plus the set bits below it in its own word). The CPU
// mirror is `material::macro_residency`; the upload order is `material::macro_upload_order`.
fn macro_layer(flat_i: u32) -> i32 {
    if (flat_i >= 256u) {
        return -1;
    }
    let w = flat_i >> 5u;
    let word = macro_lane(material.macro_mask[w >> 2u], w & 3u);
    let bit = 1u << (flat_i & 31u);
    if ((word & bit) == 0u) {
        return -1;
    }
    return i32(macro_lane(material.macro_prefix[w >> 2u], w & 3u) + countOneBits(word & (bit - 1u)));
}

// Lane `i` (0..3) of `q` by selects, not a dynamic vector index (the Mali-G78 note at `lane`).
fn macro_lane(q: vec4<u32>, i: u32) -> u32 {
    return select(select(q.x, q.y, i == 1u), select(q.z, q.w, i == 3u), i >= 2u);
}

// The rest of `surface_photoreal` on top of the layers: mid and near relief, strata, cliff
// relief and fractured-rock normals, talus, sky visibility, heightfield sun occlusion,
// cavity, horizon sky and bounce, glints and the macro map.
//
// `pre_mode` > 0: terms the terrain virtual texture caches come from `pre`. 1 (lighting):
// the cliff relief's normal gradient, the heightfield sun occlusion, the cavity, the
// horizon (its per-sector slopes; the sky term is finished with this pixel's normal) and the
// bounce. 2 (relief) also the strata and talus albedo factor and the fractured-rock normal
// gradient. Measured on the DO27 plates, those terms reconstruct from texels; the layers and
// the mid relief (with its speckle) carry the pixel-scale detail and stay per pixel.
fn surface_rest(lay: SurfLayers, in_world: vec3<f32>, footprint: f32, dist: f32, icy_v: f32, pre_mode: u32, pre: Surf) -> Surf {
    let pre_on = pre_mode > 0u;
    let pre_relief = pre_mode > 1u;
    var albedo = lay.albedo;
    var roughness = lay.roughness;
    let n_geo = lay.n_geo;
    let slope = lay.slope;
    var n_shade = lay.n_shade;
    var indirect_ao = lay.indirect_ao;
    let orm_mean = lay.orm_mean;
    let w_first3 = lay.w_first3;
    var sky_visibility = 1.0;
    var relief_shadow = 1.0;
    var cavity = 1.0;
    var horizon_sky = 1.0;
    var terrain_bounce = 0.0;
    var macro_ao = 1.0;
    var sparkle = 0.0;
    var micro_shadow = 1.0;
    var frost = 0.0;
    var near_dbg = vec2<f32>(0.0);
    var cliff_grad = vec3<f32>(0.0);
    var horizon_slopes = vec4<f32>(-1000.0);
    var rock_grad = vec3<f32>(0.0);
    var ground_bounce = 0.0;

    // Near-field detail (1–20 m, gone by 30 m): the millimetre-to-decimetre relief a
    // tiling texture cannot carry. Bright layers (ice, snow) get a crust of broken
    // plates with frost flakes; darker layers (dust, regolith) get fine grain and
    // pebbles. Each feature fades out before it is smaller than a pixel.
    let near_s = clamp(material.relief.w, 0.0, 2.0)
        * (1.0 - smoothstep01((dist - 6.0) / 18.0))
        * (1.0 - smoothstep01((slope - 0.9) / 0.3));
    // Mid-range relief (12–250 m): the 2–20 m rock, pits and ripples the heightfield
    // texels flatten and the near detail no longer covers — UE's dark, self-shadowed
    // Europa ground at 20–200 m. Fades in as the near detail fades out.
    let mid_s = clamp(material.rock.y, 0.0, 2.0)
        * smoothstep01((dist - 10.0) / 14.0)
        * (1.0 - smoothstep01((dist - 180.0) / 70.0))
        * (1.0 - smoothstep01((slope - 0.8) / 0.3));
    if (mid_s > 0.001 && !cost_off(81.0)) {
        let mid = mid_relief(in_world.xz, normalize(frame.sun_dir_wetness.xyz), footprint);
        n_shade = normalize(n_shade + vec3<f32>(mid.slope.x, 0.0, mid.slope.y) * mid_s);
        // Tone around the layer's own albedo, mean-preserving: darker regolith in the
        // hollows and wind-scoured brighter crust on the rises; on cool (icy) ground also
        // 30–60 m regolith and frost patches. DO27's cooked layers carry their own
        // regolith, so a one-sided darkening (round 3) dimmed their ground ~10 %.
        let cool_g = smoothstep01((albedo.b / max(albedo.r, 1e-4) - 0.72) / 0.13);
        let patch_s = clamp((fbm(in_world.xz / 31.0 + vec2<f32>(11.0, -3.0)) - 0.5) / 0.12, -1.0, 1.0);
        let d = clamp(0.55 * mid.relief + 0.5 * patch_s * cool_g, -1.0, 1.0) * min(mid_s, 1.0);
        let shade = select(vec3<f32>(1.0 + 0.32 * -d), vec3<f32>(1.0) - vec3<f32>(0.36, 0.38, 0.4) * d, d > 0.0);
        albedo = min(albedo * shade, vec3<f32>(0.95));
        // Fine regolith speckle (dark grains and pale frost specks at ~0.5 m), mean-
        // preserving, faded before it is under two pixels.
        let speck_r = 1.0 - smoothstep01((footprint - 0.12) / 0.2);
        if (speck_r > 0.0) {
            let sp = vnoise(in_world.xz / 0.55 + vec2<f32>(13.7, -6.1)) - 0.5;
            let fine_r = 1.0 - smoothstep01((footprint - 0.04) / 0.08);
            let sp2 = (vnoise(in_world.xz / 0.18 + vec2<f32>(-2.3, 8.8)) - 0.5) * fine_r;
            albedo = min(albedo * max(1.0 + (sp * 0.8 * speck_r + sp2 * 1.4) * min(mid_s, 1.0), 0.2), vec3<f32>(0.95));
        }
        micro_shadow = min(micro_shadow, mix(1.0, mid.shadow, min(mid_s, 1.0)));
    }

    if (near_s > 0.001) {
        let to_sun_n = normalize(frame.sun_dir_wetness.xyz);
        // Crust on ice and snow — bright AND neutral-to-cool (blue ≥ ~0.85 red) —, grain
        // on everything else, bright warm sand included: the brightness-only test gave
        // a sunlit Mars sand (luma ≈ 0.6) the ice crust, a coarse swirling marble over
        // the whole near ground. Both run only in the narrow transition between them.
        let cool = smoothstep01((albedo.b / max(albedo.r, 1e-4) - 0.72) / 0.13);
        // Brightness only separates ice from dark rock: Europa's ice is ~0.45–0.6
        // reflectance, so a snow-white threshold (0.38 luma) put pebbles on it.
        let icy = smoothstep01((luma3(albedo) - 0.18) / 0.1) * cool;
        near_dbg = vec2<f32>(near_s, icy);
        if (icy > 0.01) {
            let crust_near = near_crust(in_world.xz, to_sun_n, footprint);
            // x = crack, y = frost flake, zw = plate tilt (world xz slope), plus shadow.
            let k = near_s * icy;
            // Ice (Europa, luma ≲ 0.65) takes the frost patches; snow-bright ground
            // (luma ≳ 0.8) keeps the round-4 fine flakes, relative frost and light
            // bluing (a snowfield is all frost: patches of darker ice on it read as stains).
            let snow = smoothstep01((luma3(albedo) - 0.65) / 0.15);
            let flake = mix(crust_near.flake, crust_near.fine, snow);
            let uncracked = albedo;
            albedo *= 1.0 - 0.3 * crust_near.crack * k;
            // Ice: frost patches are near-white (fresh frost reflects ~0.85–0.9) on darker,
            // bluer bare ice, as in the UE crust (frost ~2× the display level of the ice
            // between): a contrast that reads in sun and in shade alike (the relative frost,
            // 1.35× the ice under it, all but vanished under a grazing sun or the fill).
            // Frost lies over the cracks: it brightens from the uncracked ice.
            let frost_ice = max(uncracked, min(max(uncracked * 1.2, vec3<f32>(0.84, 0.87, 0.91)), vec3<f32>(0.92)));
            let frost_snow = min(albedo * 1.35, vec3<f32>(0.92)) * (0.96 + 0.04 * flake);
            albedo = mix(albedo, mix(frost_ice, frost_snow, snow), flake * k * mix(0.85, 0.8, snow));
            let bare = mix(vec3<f32>(0.66, 0.72, 0.82), vec3<f32>(0.88, 0.9, 0.93), snow);
            albedo *= mix(vec3<f32>(1.0), bare, (1.0 - flake) * k * mix(0.85, 0.8, snow));
            n_shade = normalize(n_shade + vec3<f32>(crust_near.tilt.x, 0.0, crust_near.tilt.y) * k);
            micro_shadow = mix(1.0, crust_near.shadow, k);
            frost = flake * k;
            // Frost flakes are rough; bare plate ice is smoother.
            roughness = mix(roughness, mix(0.55, 0.92, flake), k * 0.7);
        }
        if (icy < 0.99) {
            let grain_near = near_grain(in_world.xz, to_sun_n, footprint);
            // Pebbles and grain belong to dust and regolith, not to ice.
            let k = near_s * (1.0 - icy);
            albedo *= mix(1.0, grain_near.albedo, k);
            n_shade = normalize(n_shade + vec3<f32>(grain_near.slope.x, 0.0, grain_near.slope.y) * k);
            micro_shadow = min(micro_shadow, mix(1.0, grain_near.shadow, k));
            roughness = mix(roughness, max(roughness, 0.9) + 0.08 * grain_near.albedo, k);
        }
    }

    // Rock faces (strata, ledges, fractured surface, talus). Specs/26 § 1 altitude-banded
    // layering, art-directed against the UE plates: beds dip across the map and drift
    // laterally (the band coordinate is altitude + a per-map dip plane + large-scale
    // noise warp, so a band does not trace an altitude contour); beds vary in thickness
    // and take one of three tones; harder beds protrude as broken ledges (lit top,
    // shadowed underside, dark where a ledge ends); a high-frequency fractured-rock
    // normal covers steep faces at 1–50 m; debris at the foot of each face is darker.
    let steep = smoothstep01((slope - 0.55) / 0.4);
    let strata_w = clamp(material.strata.x, 0.0, 1.0) * steep;
    // Horizontal direction along the face (for ledge segments).
    var along_dir = vec2<f32>(-n_geo.z, n_geo.x);
    if (dot(along_dir, along_dir) < 1e-6) {
        along_dir = vec2<f32>(1.0, 0.0);
    }
    along_dir = normalize(along_dir);
    let along = dot(in_world.xz, along_dir);
    // Strata and talus only scale the albedo: the relief mode takes their product from the
    // cache here.
    let albedo_pre_strata = albedo;
    // The strata shading's bed, ledge segment and flutes at the pixel, reused by the cliff
    // relief's centre tap below (same point, same noise).
    var strata_have = false;
    var strata_bed_i = 0.0;
    var strata_bed_f = 0.0;
    var strata_segment = 0.0;
    var strata_flute = 0.0;
    if (pre_relief) {
        albedo *= pre.rest_factor;
    } else if (strata_w > 0.001 && !cost_off(82.0)) {
        let thick = max(material.strata.y, 0.1);
        let bed_c = strata_bed(in_world);
        let y = bed_c.y;
        let bed = bed_c.bed;
        let f = bed_c.f;
        let dip = bed_c.dip;
        let warp = bed_c.warp;
        // Three bed tones; neighbouring beds meet at a narrow contact.
        let tone = mix(bed_tone(bed), bed_tone(bed + 1.0), smoothstep01((f - 0.93) / 0.07));
        // Weathering inside a bed: streaks that run along the face.
        let streak = 0.88 + 0.24 * fbm(vec2<f32>(along / 6.0, y * 2.3) + vec2<f32>(bed * 3.1, 0.0));
        // Ice cliffs (neutral-to-cool albedo) are not bedded rock: they show vertical
        // flutes and melt/sublimation streaks (the UE bore_twelve cliff), so the tone
        // runs across the face and stretches down it. The flutes are the cliff relief's
        // (`cliff_flute`), so tone and geometry line up.
        let icy_face = smoothstep01((albedo.b / max(albedo.r, 1e-4) - 0.72) / 0.13);
        let flute_t = cliff_flute(in_world);
        let drip = 0.94 + 0.12 * fbm(vec2<f32>(along / 0.45, in_world.y / 16.0) + vec2<f32>(-5.0, 1.0));
        let ice_tone = mix(0.74, 1.16, smoothstep01(flute_t)) * drip;
        // Formations: groups of beds (~4.5× a bed) with their own strong tone, so the
        // banding still reads where single beds are a pixel or less (far mesas at
        // 300–500 m) and the faces carry UE-like thick pale/brown bands up close.
        let fy = (in_world.y + dip + warp * 1.6) / (thick * 4.5);
        let formation = floor(fy + 0.35 * vnoise1(fy * 0.37 + 2.0));
        let f_tone = mix(
            bed_tone(formation * 7.3 + 101.0),
            bed_tone(formation * 7.3 + 108.3),
            smoothstep01((fract(fy + 0.35 * vnoise1(fy * 0.37 + 2.0)) - 0.9) / 0.1),
        );
        let fine_fade = smoothstep01((thick / max(footprint, 1e-4) - 1.5) / 3.0);
        let rock_albedo = albedo * f_tone * mix(vec3<f32>(1.0), tone * streak, 0.35 + 0.65 * fine_fade);
        let ice_albedo = albedo * ice_tone;
        albedo = mix(albedo, mix(rock_albedo, ice_albedo, icy_face), strata_w);
        // Broken ledges: hard beds protrude in segments along the face. Their shape (lit
        // top, shaded underside) is the cliff relief's ledge term, shaded from its
        // gradient below; here only the underside and the ledge ends darken.
        let bed_px = thick / max(footprint, 1e-4);
        let resolve = smoothstep01((bed_px - 2.0) / 6.0);
        let hard = step(0.5, hash21(vec2<f32>(bed, 9.1)));
        let segment = ledge_segment(in_world, bed);
        strata_have = true;
        strata_bed_i = bed;
        strata_bed_f = f;
        strata_segment = segment;
        strata_flute = flute_t;
        let top = smoothstep01((f - 0.8) / 0.15);
        let under = 1.0 - smoothstep01(f / 0.14);
        // Shadowed underside, and a dark notch where a ledge terminates.
        let ends = segment * (1.0 - segment) * 4.0 * hard;
        albedo *= 1.0
            - strata_w * resolve * (1.0 - icy_face) * (0.5 * under * hard * segment + 0.35 * ends * (1.0 - top));
    }
    // Cliff relief (`cliff_relief`): the shading normal of the displaced face, from the
    // relief's gradient over the face at this pixel's footprint. The vertices carry the
    // same function at their spacing, so lit ledges, spurs and flutes sit where the
    // geometry has them; finer terms than the vertices resolve stay shading-only.
    let cliff_gate = smoothstep01((slope - material.cliff.w) / 0.3);
    // Every term has faded once the footprint passes 4 m (`cliff_relief`).
    if (pre_on) {
        if (dot(pre.cliff_grad, pre.cliff_grad) > 0.0) {
            cliff_grad = pre.cliff_grad;
            n_shade = normalize(n_shade - cliff_grad);
        }
    } else if (cliff_gate > 0.001 && footprint < 4.0 && (strata_w > 0.001 || material.cliff.x > 0.0) && !cost_off(83.0)) {
        let big = cliff_gate * clamp(material.cliff.x, 0.0, 2.0);
        let small = cliff_gate * max(clamp(material.cliff.x, 0.0, 2.0), 1.0);
        let t1 = vec3<f32>(along_dir.x, 0.0, along_dir.y);
        let t2 = normalize(cross(n_geo, t1));
        let e = max(footprint, 0.02);
        let icy_v = icy_v;
        let d0 = cliff_shape(cliff_relief_at(in_world, footprint, icy_v, big, small, strata_have, strata_bed_i, strata_bed_f, strata_segment, strata_flute), big, slope);
        let d1 = cliff_shape(cliff_relief(in_world + t1 * e, footprint, icy_v, big, small), big, slope);
        let d2 = cliff_shape(cliff_relief(in_world + t2 * e, footprint, icy_v, big, small), big, slope);
        let grad = (t1 * (d1 - d0) + t2 * (d2 - d0)) / e;
        cliff_grad = grad;
        n_shade = normalize(n_shade - grad);
    }
    // Fractured-rock detail normal on steep faces: ridged noise at 2.4 m and 0.6 m in the
    // face's dominant plane, each octave faded before it drops below a few pixels.
    let rock_w = clamp(material.rock.x, 0.0, 2.0) * steep;
    if (pre_relief) {
        if (dot(pre.rock_grad, pre.rock_grad) > 0.0) {
            rock_grad = pre.rock_grad;
            let tangential = rock_grad - n_shade * dot(rock_grad, n_shade);
            n_shade = normalize(n_shade - tangential);
        }
    } else if (rock_w > 0.001 && !cost_off(84.0)) {
        let x_facing = abs(n_geo.x) > abs(n_geo.z);
        var uv = in_world.xy;
        if (x_facing) {
            uv = in_world.zy;
        }
        var grad = vec2<f32>(0.0);
        for (var o = 0; o < 2; o++) {
            let scale = select(0.6, 2.4, o == 0);
            let fade = 1.0 - smoothstep01((footprint - scale * 0.08) / (scale * 0.25));
            if (fade <= 0.0) {
                continue;
            }
            let q = uv / scale + vec2<f32>(f32(o) * 11.3, f32(o) * -4.1);
            let e = 0.12;
            let r0 = ridged(q);
            let gx = ridged(q + vec2<f32>(e, 0.0)) - r0;
            let gy = ridged(q + vec2<f32>(0.0, e)) - r0;
            grad += vec2<f32>(gx, gy) / e * 0.45 * fade;
        }
        var world_grad = vec3<f32>(grad.x, grad.y, 0.0);
        if (x_facing) {
            world_grad = vec3<f32>(0.0, grad.y, grad.x);
        }
        let tangential = world_grad - n_shade * dot(world_grad, n_shade);
        n_shade = normalize(n_shade - tangential * rock_w);
        rock_grad = world_grad * rock_w;
    }
    // Talus: the concave foot of a face (measured over ~6 m, whatever the pixel size)
    // collects darker debris.
    let talus_s = clamp(material.strata2.w, 0.0, 1.0);
    if (talus_s > 0.0 && !pre_relief && !cost_off(85.0)) {
        let top_level = f32(height_levels() - 1u);
        let m_talus = min(round(max(footprint_mip(footprint) + 3.0, log2(6.0 / texel_m()))), top_level);
        let wide = texel_m() * exp2(m_talus);
        let blurred = height_level(sample_coord(in_world.xz), u32(m_talus));
        let depth = max(blurred - in_world.y, 0.0) / max(wide, 1e-3);
        let foot = smoothstep01((depth - 0.04) / 0.25) * (1.0 - smoothstep01((slope - 1.1) / 0.2));
        let debris = 0.8 + 0.3 * vnoise(in_world.xz / 0.35);
        albedo = mix(albedo, albedo * vec3<f32>(0.66, 0.62, 0.6) * debris, foot * talus_s);
    }

    let rest_factor = min(albedo / max(albedo_pre_strata, vec3<f32>(1e-4)), vec3<f32>(16.0));

    // Hemispherical sky light: a face sees the sky dome in proportion to
    // (1 + N·up) / 2 (the irradiance of a uniform upper hemisphere, with a dark
    // ground below). Without it every shaded slope got the same flat ambient and low-sun
    // shots read as one tinted slab. Floor 0.35 stands in for ground bounce.
    sky_visibility = max(0.5 + 0.5 * n_shade.y, 0.35);

    // Relief lighting: heightfield self-shadowing toward the sun (near-field crust and
    // every ridge's shadow side, beyond what a cascade texel resolves), concavity
    // occlusion of the sky light, and ice/snow sparkle on bright, crystalline layers.
    let to_sun = normalize(frame.sun_dir_wetness.xyz);
    // Beyond 300 m the cascades and the prefiltered normal carry the relief (the far
    // path skips the march).
    if (pre_on) {
        relief_shadow = pre.relief_shadow;
    } else if (material.relief.x > 0.0 && dist < 300.0 && !cost_off(86.0) && !relief_sun_hidden) {
        let reach = min(material.relief.x + dist * 0.2, 400.0);
        relief_shadow = heightfield_sun_occlusion(in_world, to_sun, footprint, reach, dist);
    }
    if (pre_on) {
        cavity = pre.cavity;
    } else if (!cost_off(87.0)) {
        cavity = height_cavity(in_world.xz, in_world.y, footprint, clamp(material.relief.y, 0.0, 1.0));
    }
    // Canyon-scale sky occlusion of the hemisphere sky term (shared path, within 2 km).
    if (pre_on) {
        // The sky fraction with this pixel's own normal over the cached horizon.
        horizon_slopes = pre.horizon_slopes;
        horizon_sky = clamp(dot(horizon_open(horizon_slopes, n_shade), vec4<f32>(0.25)) / 0.9, 0.0, 1.0);
        terrain_bounce = pre.terrain_bounce;
    } else if (frame.ground_ambient.w > 0.5 && dist < 2000.0 && !cost_off(88.0)) {
        let hb = horizon_sky_and_bounce(in_world, footprint, n_shade, to_sun);
        // (Weighting the sectors by the sky map's irradiance toward each changed nothing
        // measurable against the path tracer: the cosine-convolved map is too smooth.)
        horizon_sky = clamp(dot(hb.open, vec4<f32>(0.25)) / 0.9, 0.0, 1.0);
        terrain_bounce = hb.bounce * clamp(material.rock.z, 0.0, 2.0);
        ground_bounce = hb.bounce_up * clamp(material.rock.z, 0.0, 2.0);
        horizon_slopes = hb.slopes;
    }
    // Glints only on bright, smooth (crystalline) layers: ice and snow, never dust.
    // Glints on ice and snow (bright, neutral-to-cool albedo) whatever the layer's mean
    // roughness — a snow crust is rough on average but its crystals are mirror facets —
    // and never on warm dust or rock.
    let glint_cool = smoothstep01((albedo.b / max(albedo.r, 1e-4) - 0.72) / 0.13);
    sparkle = clamp(material.relief.z, 0.0, 4.0) * smoothstep01((luma3(albedo) - 0.3) / 0.2) * glint_cool;

    // Whole-map macro variation from the tiled mip-chained bake (with the coarse
    // fallback for tiles outside the streaming residency mask).
    if (material.params0.y > 0.0 && !cost_off(89.0)) {
        let uv_m = (in_world.xz + vec2<f32>(terrain.half_width)) / material.params0.y;
        let tvec = uv_m * material.tiles.x;
        let tile = clamp(vec2<i32>(floor(tvec)), vec2<i32>(0), vec2<i32>(i32(material.tiles.x) - 1));
        let in_tile = fract(tvec);
        let flat_i = u32(tile.x + tile.y * i32(material.tiles.x));
        let layer_m = macro_layer(flat_i);
        // The macro map is a top projection: on a steep face its texels stretch down the
        // fall line by 1/n.y, and a texel-sharp lookup printed long vertical streaks over
        // cliffs (bore_twelve_close's far wall, a smeared slab). Filter it by the stretch
        // (the face then takes the map's local mean, and the triplanar layers and the
        // strata carry its detail).
        let stretch = 1.0 / max(abs(n_geo.y), 0.08);
        let lod_m = footprint_lod(footprint * stretch, material.tiles.w);
        var macro_rgba = vec4<f32>(1.0);
        var macro_n = vec4<f32>(0.5, 0.5, 0.5, 0.5);
        let g_m = aniso_grads_top(lod_m);
        let no_rot = vec2<f32>(1.0, 0.0);
        if (layer_m >= 0) {
            let layer_i = layer_m;
            // A tile wraps at its edges (repeat sampler): a long anisotropic footprint that
            // reaches past the edge would average in the far side of the tile, so near the
            // edge the tap stays isotropic (bilinear already wraps there by under a texel).
            let edge = min(min(in_tile.x, 1.0 - in_tile.x), min(in_tile.y, 1.0 - in_tile.y));
            let edge_texels = edge * f32(textureDimensions(macro_albedo).x);
            let g_tile = Grad2(g_m.x, g_m.y, g_m.on && 0.5 * max(length(g_m.x), length(g_m.y)) < edge_texels);
            macro_rgba = layer_tap(macro_albedo, in_tile, layer_i, lod_m, g_tile, no_rot);
            macro_n = layer_tap(macro_normal, in_tile, layer_i, lod_m, g_tile, no_rot);
        } else {
            macro_rgba = layer_tap(macro_coarse_albedo, uv_m, 0, lod_m, g_m, no_rot);
            macro_n = layer_tap(macro_coarse_normal, uv_m, 0, lod_m, g_m, no_rot);
        }
        let mstr = clamp(material.params0.w, 0.0, 1.0);
        // The macro map is a relative tint around the layers' authored albedo (DO27:
        // "macro_albedo cropped to the map and normalised"): divide by its whole-map mean
        // (the coarse map's last mip). Multiplying the raw macro albedo in (mean ≈ 0.29 /
        // 0.15 / 0.08 on Switchbacks) printed the ground 3–4× under its reflectance.
        // The sRGB arrays decode to linear in hardware.
        let top_mip = f32(textureNumLevels(macro_coarse_albedo) - 1u);
        let macro_mean = textureSampleLevel(macro_coarse_albedo, material_samp, vec2<f32>(0.5), 0, top_mip).rgb;
        let tint = macro_rgba.rgb / max(macro_mean, vec3<f32>(1e-3));
        albedo = mix(albedo, albedo * tint, mstr);
        // AO in the alpha channel occludes indirect light only.
        macro_ao = mix(1.0, macro_rgba.a, mstr);
        let delta = vec2<f32>(macro_n.r, macro_n.g) * 2.0 - vec2<f32>(1.0);
        n_shade = normalize(n_shade + vec3<f32>(delta.x, 0.0, delta.y) * material.params1.w * mstr);
    }
    // The ground a sloped face sees (`shade`: the irradiance map's ground part, replaced by the
    // ground the point actually sees) is lit by more than the sun and the open sky: by the
    // sunlit walls around it too (`Horizon::bounce_up`, the wall bounce of a horizontal
    // receiver; a shadowed trench floor's main light under an airless black sky). That light
    // reaches this face through the ground's view factor (1 − n.y) / 2 and the ground's albedo
    // (the same layer), and like the wall bounce it is reflected by this surface, so it joins
    // `terrain_bounce` here — the virtual texture caches it with the rest of the bounce.
    // Without it a wall above a shadowed ice floor saw a black floor (Europa's trench walls
    // at 0.28x the path tracer's indirect light).
    if (!pre_on && frame.output_mode.w > 0.0) {
        let ground_view = clamp(0.5 - 0.5 * n_shade.y, 0.0, 1.0);
        terrain_bounce += ground_bounce * ground_view * luma3(min(albedo, vec3<f32>(0.95)));
    }
    return Surf(
        albedo,
        roughness,
        n_geo,
        slope,
        n_shade,
        cavity,
        w_first3,
        relief_shadow,
        horizon_sky,
        terrain_bounce,
        indirect_ao,
        macro_ao,
        sparkle,
        micro_shadow,
        frost,
        sky_visibility,
        near_dbg,
        cliff_grad,
        horizon_slopes,
        rest_factor,
        rock_grad,
        orm_mean,
    );
}

// ---------------------------------------------------------------------------
// Terrain virtual texture (ADR-117 "Terrain virtual texture")
// ---------------------------------------------------------------------------
// The surface (`surface_photoreal`) cached in pages of a virtual texture over the map's XZ:
// one page table per LOD, pages baked into an atlas by the same function (`cs_vt_bake`),
// and a pixel whose two LOD pages are resident reads them (`vt_lookup`) instead of shading
// the surface; every other pixel shades it in full (a miss costs time, never a wrong pixel).
// Feedback (the LOD each pixel wanted) drives which pages get baked (`vt.wgsl`, after Wicked
// Engine). The bake rebuilds, per texel, what the colour pass interpolates for a pixel at
// that XZ (`vt_geo`: the camera's CDLOD triangle under it, its height, pyramid level and
// icy estimate) and shades it at the camera's distance, so a page baked from the current
// camera is what the pixel would have computed, up to the footprint (texel vs. pixel) and
// the bilinear reconstruction. The CPU re-bakes pages whose distance-dependent terms the
// camera has moved since (`gpu::vt`).

struct VtParams {
    // x = LOD-0 texel (m), y = page texels (payload), z = border texels, w = slot texels
    dims: vec4<f32>,
    // x = LOD count, y = LOD-0 pages per side, z = atlas texels per side, w = 1 when on
    info: vec4<f32>,
    // x = oversample (texel footprints per texel), y = nearest distance served (m),
    // z = smallest |n.y| served, w = frame index (feedback pattern)
    view: vec4<f32>,
    // x = slots per atlas side, y = node-map cells per side, z = node-map cell (m),
    // w = what is cached: 0 the whole surface, 1 the lighting terms, 2 the relief (see
    // `surface_rest`)
    atlas: vec4<f32>,
    // x = 1: stochastic LOD (one of the two LODs per pixel, picked with the trilinear weight;
    // the temporal filter averages them), yzw unused
    extra: vec4<f32>,
    // Page-table offset of each LOD (16 LODs, 4 per vec4).
    lod_offset: array<vec4<u32>, 4>,
};

// Shared colour pass with the virtual texture (`fs_shared_vt`) only: group 4 (the shadow
// pass's cascade group number; binding 0 is the cascade there).
@group(4) @binding(1) var<uniform> vt: VtParams;
// Page tables: atlas slot + 1 of each resident page, 0 when the page is not resident.
@group(4) @binding(2) var<storage, read> vt_pages: array<u32>;
// LOD-0 page grid: 255 − the finest LOD a pixel there asked for (0 = none); then two
// counters (sampled pixels that missed, that hit).
@group(4) @binding(3) var<storage, read_write> vt_feedback: array<atomic<u32>>;
// All four maps RGBA16F. Whole surface: A = albedo (linear) + roughness, B = shading
// normal + cavity, C = relief shadow, horizon sky, bounce, micro shadow, D = sparkle,
// indirect AO, macro AO, sky visibility (frost belongs to the near field, which has faded
// where this mode serves). Split modes: A = strata/talus albedo factor + cavity, B = the
// horizon's steepest slope per sector, C = the cliff relief's normal gradient + relief
// shadow, D = the fractured-rock normal gradient + bounce.
@group(4) @binding(4) var vt_atlas_a: texture_2d<f32>;
@group(4) @binding(5) var vt_atlas_b: texture_2d<f32>;
@group(4) @binding(6) var vt_atlas_c: texture_2d<f32>;
@group(4) @binding(7) var vt_atlas_d: texture_2d<f32>;
@group(4) @binding(8) var vt_samp: sampler;
// Map E, whole-surface mode only (`cs_vt_bake_e` or `cs_vt_bake_horizon`; a 1-texel stand-in in the split
// modes, whose map B holds the same): the horizon's steepest slope per sector
// (`Horizon::slopes`, −1000 where it was not computed). The colour pass finishes the
// planet-shine fill's and the dusty sky's horizon terms (`fill_horizon_vis`,
// `sky_excess_hidden`) from it per pixel, as the uncached path does (ADR-117 addendum v0.1.2).
@group(4) @binding(9) var vt_atlas_e: texture_2d<f32>;

fn vt_lod_offset(lod: u32) -> u32 {
    let v = vt.lod_offset[lod / 4u];
    switch lod % 4u {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

// A flat placeholder surface (no virtual-texture hit).
fn no_surf() -> Surf {
    let n = vec3<f32>(0.0, 1.0, 0.0);
    return Surf(vec3<f32>(0.0), 1.0, n, 0.0, n, 1.0, vec3<f32>(0.0), 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, vec2<f32>(0.0), vec3<f32>(0.0), vec4<f32>(-1000.0), vec3<f32>(1.0), vec3<f32>(0.0), 1.0);
}

struct VtHit {
    surf: Surf,
    hit: bool,
};

// One LOD's page under world XZ: its table entry (slot + 1, 0 = none) and the atlas UV of
// the point inside that slot.
struct VtPage {
    entry: u32,
    uv: vec2<f32>,
};

fn vt_page(lod: u32, xz: vec2<f32>) -> VtPage {
    let texel_m = vt.dims.x * f32(1u << lod);
    let page_m = texel_m * vt.dims.y;
    let side = max(u32(vt.info.y) >> lod, 1u);
    var rel = xz + vec2<f32>(terrain.half_width);
    // A map deeper than it is wide covers a quadtree of side `depth.y` (ADR-8369). Square
    // maps and wider maps keep the X lane, which is that quadtree's corner.
    if (terrain.depth.y > terrain.world_width) {
        rel = xz + vec2<f32>(terrain.depth.x);
    }
    let pc = clamp(floor(rel / page_m), vec2<f32>(0.0), vec2<f32>(f32(side - 1u)));
    let entry = vt_pages[vt_lod_offset(lod) + u32(pc.y) * side + u32(pc.x)];
    let local = clamp(rel / texel_m - pc * vt.dims.y, vec2<f32>(0.0), vec2<f32>(vt.dims.y));
    let slot = max(entry, 1u) - 1u;
    let slots = u32(vt.atlas.x);
    let origin = vec2<f32>(f32(slot % slots), f32(slot / slots)) * vt.dims.w + vec2<f32>(vt.dims.z);
    return VtPage(entry, (origin + local) / vt.info.z);
}

// The atlas texels of one resident page, bilinear inside its slot (the border texels are
// the neighbouring pages' edge texels, baked from the same XZ).
struct VtTexel {
    a: vec4<f32>,
    b: vec4<f32>,
    c: vec4<f32>,
    d: vec4<f32>,
};

fn vt_fetch(uv: vec2<f32>) -> VtTexel {
    return VtTexel(
        textureSampleLevel(vt_atlas_a, vt_samp, uv, 0.0),
        textureSampleLevel(vt_atlas_b, vt_samp, uv, 0.0),
        textureSampleLevel(vt_atlas_c, vt_samp, uv, 0.0),
        textureSampleLevel(vt_atlas_d, vt_samp, uv, 0.0),
    );
}

// The cached surface for this pixel. The pixel's footprint (as `shade` measures it) times
// its triangle's |n.y| is the horizontal texel size it wants; the two LODs around it blend
// (trilinear). Pixels nearer than `view.y`, on faces steeper than `view.z`, or finer than
// LOD 0 shade in full and request nothing. One pixel in each 4×4 block (rotating per frame)
// records the LOD it wanted on its LOD-0 page (feedback); one in each 32×32 counts hit/miss.
// Most virtual-texture lookups `vt_lookup` averages along a footprint's long axis.
const VT_ANISO_TAPS: f32 = 2.0;

fn vt_lookup(in: VOut) -> VtHit {
    let p = in.base;
    let dx = dpdx(p);
    let dy = dpdy(p);
    let footprint = max(length(dx), length(dy));
    let tri = cross(dy, dx);
    let ny = abs(tri.y) / max(length(tri), 1e-20);
    // Debug views bypass it, except its own (96) and the surface-term switches (78–89).
    let probe_view = frame.ambient_debug.a > 77.5 && frame.ambient_debug.a < 92.5;
    if (vt.info.w < 0.5 || material.params0.x < 0.5 || (frame.ambient_debug.a > 0.5 && !vt_geo_debug() && !probe_view)) {
        return VtHit(no_surf(), false);
    }
    let lod_count = u32(vt.info.x);
    // Anisotropic filtering (ADR-192): like `fragment_normal`, the LOD follows the footprint's
    // short axis and up to `VT_ANISO_TAPS` lookups spread along its long axis (each finds its
    // own pages, so page edges need no border). The atlas holds no mip chain to filter with
    // gradients. Two taps make a pixel ask for at most one LOD finer: the page demand (and
    // the atlas a view needs) grows by one level, not two. Whole-surface mode only: the split
    // modes cache lighting terms the uncached path evaluates at the pixel center, and their
    // layers are sampled live (anisotropically) anyway.
    let tap_cap = select(1.0, min(floor(aniso_level() * 0.5), VT_ANISO_TAPS), vt.atlas.w < 0.5);
    var taps = 1.0;
    var axis = vec2<f32>(0.0);
    if (tap_cap > 1.5 && footprint > 0.0) {
        let minor = length(cross(dx, dy)) / footprint;
        let ratio = footprint / max(minor, footprint * 1e-4);
        taps = clamp(round(ratio), 1.0, tap_cap);
        axis = select(dy.xz, dx.xz, length(dx) > length(dy));
    }
    let lod_f = log2(max(footprint * ny / taps, 1e-9) / (vt.dims.x * vt.view.x));
    let dist = distance(frame.camera_pos_time.xyz, p);
    // The whole-surface mode never serves nearer than 24 m: the near-field detail (`near_s`,
    // its frost) has faded out there, and the atlas does not store frost. The split mode
    // shades the near field per pixel.
    let near_m = select(max(vt.view.y, 24.0), vt.view.y, vt.atlas.w > 0.5);
    if (dist < near_m || ny < vt.view.z || lod_f < 0.0 || lod_f >= f32(lod_count - 1u)) {
        return VtHit(no_surf(), false);
    }
    let l0 = u32(lod_f);
    let t = lod_f - f32(l0);
    let count = i32(taps);
    var hit = true;
    for (var i = 0; i < count; i++) {
        let xz = p.xz + axis * ((f32(i) + 0.5) / taps - 0.5);
        hit = hit && vt_page(l0, xz).entry != 0u && vt_page(l0 + 1u, xz).entry != 0u;
    }
    let pix = vec2<u32>(in.position.xy);
    let frame_i = u32(vt.view.w);
    let side0 = u32(vt.info.y);
    if ((pix.x % 4u) + 4u * (pix.y % 4u) == frame_i % 16u) {
        var page_xz = p.xz + vec2<f32>(terrain.half_width);
        if (terrain.depth.y > terrain.world_width) {
            page_xz = p.xz + vec2<f32>(terrain.depth.x);
        }
        let pc = clamp(
            vec2<i32>(floor(page_xz / (vt.dims.x * vt.dims.y))),
            vec2<i32>(0),
            vec2<i32>(i32(side0) - 1),
        );
        atomicMax(&vt_feedback[u32(pc.y) * side0 + u32(pc.x)], 255u - l0);
    }
    // Hit/miss counters: one pixel in each 32×32 (two global atomics; any denser and the
    // pixels queue on them).
    if ((pix.x % 32u) + 32u * (pix.y % 32u) == (frame_i * 97u) % 1024u) {
        atomicAdd(&vt_feedback[side0 * side0 + select(0u, 1u, hit)], 1u);
    }
    if (!hit) {
        return VtHit(no_surf(), false);
    }
    let up = vec3<f32>(0.0, 1.0, 0.0);
    // (Four vec4 locals, not a `VtTexel` one: ADR-026.)
    var a = vec4<f32>(0.0);
    var b = vec4<f32>(0.0);
    var c = vec4<f32>(0.0);
    var d = vec4<f32>(0.0);
    // Map E (whole-surface mode): the horizon slopes. A blend with a −1000 texel ("not
    // computed") stays far below any horizon, which both horizon terms read as open sky,
    // as they read −1000.
    var e = vec4<f32>(-1000.0);
    let whole = vt.atlas.w < 0.5;
    // Stochastic LOD: interleaved gradient noise (Jimenez 2014), shifted every frame.
    let q = in.position.xy + vec2<f32>(f32(frame_i % 64u) * 5.588238);
    let ign = fract(52.9829189 * fract(dot(q, vec2<f32>(0.06711056, 0.00583715))));
    if (count > 1) {
        e = vec4<f32>(0.0);
    }
    for (var i = 0; i < count; i++) {
        let xz = p.xz + axis * ((f32(i) + 0.5) / taps - 0.5);
        let p0 = vt_page(l0, xz);
        let p1 = vt_page(l0 + 1u, xz);
        var ta = vec4<f32>(0.0);
        var tb = vec4<f32>(0.0);
        var tc = vec4<f32>(0.0);
        var td = vec4<f32>(0.0);
        var te = vec4<f32>(-1000.0);
        if (vt.extra.x > 0.5) {
            let uv = select(p0.uv, p1.uv, ign < t);
            let x = vt_fetch(uv);
            ta = x.a;
            tb = x.b;
            tc = x.c;
            td = x.d;
            if (whole) {
                te = textureSampleLevel(vt_atlas_e, vt_samp, uv, 0.0);
            }
        } else {
            let x0 = vt_fetch(p0.uv);
            let x1 = vt_fetch(p1.uv);
            ta = mix(x0.a, x1.a, t);
            tb = mix(x0.b, x1.b, t);
            tc = mix(x0.c, x1.c, t);
            td = mix(x0.d, x1.d, t);
            if (whole) {
                te = mix(
                    textureSampleLevel(vt_atlas_e, vt_samp, p0.uv, 0.0),
                    textureSampleLevel(vt_atlas_e, vt_samp, p1.uv, 0.0),
                    t,
                );
            }
        }
        if (count == 1) {
            a = ta;
            b = tb;
            c = tc;
            d = td;
            e = te;
        } else {
            a += ta / taps;
            b += tb / taps;
            c += tc / taps;
            d += td / taps;
            e += te / taps;
        }
    }
    if (vt.atlas.w > 0.5) {
        // Split modes: the cached terms of `surface_rest` (through `surface_or_probe`),
        // marked by `slope` = −mode.
        return VtHit(
            Surf(vec3<f32>(0.0), 0.0, up, -vt.atlas.w, up, a.w, vec3<f32>(0.0), c.w, 1.0, d.w, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, vec2<f32>(0.0), c.xyz, b, a.xyz, d.xyz, 1.0),
            true,
        );
    }
    let n = normalize(b.xyz);
    // The pages do not carry `orm_mean` yet (1: the irradiance probes see the plain layer AO);
    // the VT must store it before it is wired to the colour pass (gi-probes round 3 fix).
    return VtHit(
        Surf(a.xyz, a.w, up, 0.0, n, b.w, vec3<f32>(0.0), c.x, c.y, c.z, d.y, d.z, d.x, c.w, 0.0, d.w, vec2<f32>(0.0), vec3<f32>(0.0), e, vec3<f32>(1.0), vec3<f32>(0.0), 1.0),
        true,
    );
}

// Debug view 96: the bake stores its rebuilt geometry (`vt_geo`) in atlas B, and VT-served
// pixels show how far it is from what the rasteriser interpolated (`vt_geo_error`).
fn vt_geo_debug() -> bool {
    return frame.ambient_debug.a > 95.5 && frame.ambient_debug.a < 96.5;
}

// Debug view 96: the bake's geometry against this pixel's (see the colours below); black
// where the texture missed.
fn vt_geo_error(in: VOut) -> vec4<f32> {
    let p = in.base;
    let dx = dpdx(p);
    let dy = dpdy(p);
    let footprint = max(length(dx), length(dy));
    let tri = cross(dy, dx);
    let ny = abs(tri.y) / max(length(tri), 1e-20);
    let lod_f = log2(max(footprint * ny, 1e-9) / (vt.dims.x * vt.view.x));
    let k = luma3(frame.sun_color_exposure.rgb * frame.sun_color_exposure.a) * INV_PI;
    if (lod_f < 0.0 || lod_f >= vt.info.x - 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let pg = vt_page(u32(lod_f), p.xz);
    if (pg.entry == 0u) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    // The nearest texel (its centre is within half a texel of this pixel's point).
    let b = textureLoad(vt_atlas_b, vec2<i32>(floor(pg.uv * vt.info.z)), 0);
    let wrapped = abs(b.x - fract(p.y));
    let e = vec3<f32>(min(wrapped, 1.0 - wrapped), abs(b.y - in.geo_mip), abs(b.z - ny) * 4.0);
    // Mid grey where the bake matches (auto exposure keeps it grey), tinted by the error.
    // Hue survives auto exposure: green = within 5 cm / 0.05 level / 0.02 n.y, red = height
    // off, blue = level off, magenta = both; yellow = only n.y off.
    let bad = vec3<f32>(step(0.05, e.x), step(0.05, e.y), step(0.08, e.z));
    let c = select(vec3<f32>(bad.x, 0.0, bad.y), vec3<f32>(1.0, 1.0, 0.0), bad.z > 0.5 && bad.x + bad.y < 0.5);
    return vec4<f32>(select(c, vec3<f32>(0.0, 1.0, 0.0), bad.x + bad.y + bad.z < 0.5) * k * 0.004, 1.0);
}

// Bake group (compute only): the node buffer (binding 0, as the CDLOD passes bind it), the
// virtual-texture uniform, the page jobs, the atlas and the node map.
@group(2) @binding(4) var<uniform> vt_bake: VtParams;
// One job per page: x = LOD, y = page x, z = page y, w = atlas slot.
@group(2) @binding(5) var<storage, read> vt_jobs: array<vec4<u32>>;
@group(2) @binding(6) var vt_out_a: texture_storage_2d<rgba16float, write>;
@group(2) @binding(7) var vt_out_b: texture_storage_2d<rgba16float, write>;
@group(2) @binding(8) var vt_out_c: texture_storage_2d<rgba16float, write>;
@group(2) @binding(9) var vt_out_d: texture_storage_2d<rgba16float, write>;
// The camera's CDLOD selection over a grid of leaf-node cells: node index + 1 (0 = none).
@group(2) @binding(10) var<storage, read> vt_node_map: array<u32>;
// Map E: written by `cs_vt_bake_e` with the other four maps, or, on devices limited to four
// storage textures per stage (regia_render's mobile tier), by its own pass
// `cs_vt_bake_horizon`.
@group(2) @binding(11) var vt_out_e: texture_storage_2d<rgba16float, write>;

fn cross2(a: vec2<f32>, b: vec2<f32>) -> f32 {
    return a.x * b.y - a.y * b.x;
}

// Barycentric coordinates of `p` in the XZ triangle (a, b, c); −1s when it is degenerate.
fn bary2(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>, c: vec2<f32>) -> vec3<f32> {
    let den = cross2(b - a, c - a);
    if (abs(den) < 1e-12) {
        return vec3<f32>(-1.0);
    }
    let l1 = cross2(p - a, c - a) / den;
    let l2 = cross2(b - a, p - a) / den;
    return vec3<f32>(1.0 - l1 - l2, l1, l2);
}

// What the colour pass interpolates for a pixel whose heightfield point is at `xz`.
struct VtGeo {
    y: f32,
    m: f32,
    icy: f32,
    vertex_n: vec3<f32>,
    // |n.y| of the triangle (the footprint's stretch).
    ny: f32,
    ok: bool,
};

fn vt_vertex_n(xz: vec2<f32>, m: f32) -> vec3<f32> {
    // `vs_main`: the photoreal path reads the vertex normal only for the budget's far normal.
    if (budget_on(4u)) {
        return normal_level(xz, u32(round(m)));
    }
    return vec3<f32>(0.0, 1.0, 0.0);
}

// The camera's CDLOD triangle under `xz` (the node from the node map, then a walk over its
// morphed grid: the morph moves odd vertices toward their even neighbour, so a point can sit
// in a cell below the one its unmorphed coordinate names; the walk steps across whichever
// cell edge the point lies beyond), and its vertices' attributes interpolated at `xz` with
// the barycentrics the rasteriser would use (the base XZ is affine over the triangle).
fn vt_geo(xz: vec2<f32>) -> VtGeo {
    let none = VtGeo(0.0, 0.0, 0.0, vec3<f32>(0.0, 1.0, 0.0), 1.0, false);
    let cells = u32(vt_bake.atlas.y);
    var cell_xz = xz + vec2<f32>(terrain.half_width);
    if (terrain.depth.y > terrain.world_width) {
        cell_xz = xz + vec2<f32>(terrain.depth.x);
    }
    let cell = vec2<i32>(floor(cell_xz / vt_bake.atlas.z));
    if (any(cell < vec2<i32>(0)) || any(cell >= vec2<i32>(i32(cells)))) {
        return none;
    }
    let entry = vt_node_map[u32(cell.y) * cells + u32(cell.x)];
    if (entry == 0u) {
        return none;
    }
    let node = nodes[entry - 1u];
    let eye = frame.camera_pos_time.xz;
    let step_w = node.rect_size / node.grid_size;
    let gmax = node.grid_size;
    var g = clamp(floor((xz - node.rect_min) / step_w), vec2<f32>(0.0), vec2<f32>(gmax - 1.0));
    // The best triangle so far: vertex XZ and pyramid levels (no struct locals: ADR-026).
    var a_xz = vec2<f32>(0.0);
    var b_xz = vec2<f32>(0.0);
    var c_xz = vec2<f32>(0.0);
    var tri_m = vec3<f32>(0.0);
    var lam = vec3<f32>(-1.0);
    var best = -1e9;
    let eps = 1e-5 * step_w;
    for (var it = 0; it < 8; it++) {
        let v00 = morph_grid(g, node, eye);
        let v01 = morph_grid(g + vec2<f32>(0.0, 1.0), node, eye);
        let v10 = morph_grid(g + vec2<f32>(1.0, 0.0), node, eye);
        let v11 = morph_grid(g + vec2<f32>(1.0, 1.0), node, eye);
        // The cell's two triangles (`grid_indices`): (00, 01, 10) and (10, 01, 11).
        let la = bary2(xz, v00.xz, v01.xz, v10.xz);
        let lb = bary2(xz, v10.xz, v01.xz, v11.xz);
        let ma = min(min(la.x, la.y), la.z);
        let mb = min(min(lb.x, lb.y), lb.z);
        if (ma > best) {
            best = ma;
            a_xz = v00.xz;
            b_xz = v01.xz;
            c_xz = v10.xz;
            tri_m = vec3<f32>(v00.m, v01.m, v10.m);
            lam = la;
        }
        if (mb > best) {
            best = mb;
            a_xz = v10.xz;
            b_xz = v01.xz;
            c_xz = v11.xz;
            tri_m = vec3<f32>(v10.m, v01.m, v11.m);
            lam = lb;
        }
        if (best >= -1e-5) {
            break;
        }
        var stepg = vec2<f32>(0.0);
        if (cross2(v01.xz - v00.xz, xz - v00.xz) > eps) {
            stepg.x = -1.0;
        } else if (cross2(v11.xz - v10.xz, xz - v10.xz) < -eps) {
            stepg.x = 1.0;
        }
        if (cross2(v10.xz - v00.xz, xz - v00.xz) < -eps) {
            stepg.y = -1.0;
        } else if (cross2(v11.xz - v01.xz, xz - v01.xz) > eps) {
            stepg.y = 1.0;
        }
        let next = clamp(g + stepg, vec2<f32>(0.0), vec2<f32>(gmax - 1.0));
        if (all(next == g)) {
            break;
        }
        g = next;
    }
    // A fully morphed fine grid (cliff relief's dense nodes) collapses whole cells to zero
    // area, where the edge tests cannot tell which way to step: search the cells the morph
    // can have moved the point into (it moves vertices up to three fine cells toward +x/+z).
    if (best < -1e-5) {
        let g0 = clamp(floor((xz - node.rect_min) / step_w), vec2<f32>(0.0), vec2<f32>(gmax - 1.0));
        for (var dz = -3; dz <= 1; dz++) {
            for (var dx = -3; dx <= 1; dx++) {
                let gc = g0 + vec2<f32>(f32(dx), f32(dz));
                if (any(gc < vec2<f32>(0.0)) || any(gc > vec2<f32>(gmax - 1.0))) {
                    continue;
                }
                let v00 = morph_grid(gc, node, eye);
                let v01 = morph_grid(gc + vec2<f32>(0.0, 1.0), node, eye);
                let v10 = morph_grid(gc + vec2<f32>(1.0, 0.0), node, eye);
                let v11 = morph_grid(gc + vec2<f32>(1.0, 1.0), node, eye);
                let la = bary2(xz, v00.xz, v01.xz, v10.xz);
                let lb = bary2(xz, v10.xz, v01.xz, v11.xz);
                let ma = min(min(la.x, la.y), la.z);
                let mb = min(min(lb.x, lb.y), lb.z);
                if (ma > best) {
                    best = ma;
                    a_xz = v00.xz;
                    b_xz = v01.xz;
                    c_xz = v10.xz;
                    tri_m = vec3<f32>(v00.m, v01.m, v10.m);
                    lam = la;
                }
                if (mb > best) {
                    best = mb;
                    a_xz = v10.xz;
                    b_xz = v01.xz;
                    c_xz = v11.xz;
                    tri_m = vec3<f32>(v10.m, v01.m, v11.m);
                    lam = lb;
                }
            }
        }
    }
    lam = max(lam, vec3<f32>(0.0));
    lam = lam / max(lam.x + lam.y + lam.z, 1e-6);
    let ca = vertex_core(a_xz, tri_m.x);
    let cb = vertex_core(b_xz, tri_m.y);
    let cc = vertex_core(c_xz, tri_m.z);
    let pa = vec3<f32>(a_xz.x, ca.h, a_xz.y);
    let pb = vec3<f32>(b_xz.x, cb.h, b_xz.y);
    let pc = vec3<f32>(c_xz.x, cc.h, c_xz.y);
    let n = cross(pb - pa, pc - pa);
    let vn = vt_vertex_n(a_xz, tri_m.x) * lam.x + vt_vertex_n(b_xz, tri_m.y) * lam.y + vt_vertex_n(c_xz, tri_m.z) * lam.z;
    return VtGeo(
        dot(lam, vec3<f32>(ca.h, cb.h, cc.h)),
        dot(lam, tri_m),
        dot(lam, vec3<f32>(ca.icy, cb.icy, cc.icy)),
        vn,
        abs(n.y) / max(length(n), 1e-20),
        true,
    );
}

// Bakes one texel of one page (workgroup z = job): the surface at the texel centre on the
// camera's triangle there, with the footprint of a pixel that would pick this LOD exactly
// (`oversample` texels, stretched by 1 / |n.y|) and the camera's distance to it.
// Where one bake invocation writes: the texel centre's XZ, the LOD's texel size and the
// atlas texel (`ok` false past the slot).
struct VtBakeAt {
    xz: vec2<f32>,
    texel_m: f32,
    texel: vec2<u32>,
    ok: bool,
};

fn vt_bake_at(gid: vec3<u32>) -> VtBakeAt {
    let job = vt_jobs[gid.z];
    let slot_size = u32(vt_bake.dims.w);
    let lod = job.x;
    let texel_m = vt_bake.dims.x * f32(1u << lod);
    let page_m = texel_m * vt_bake.dims.y;
    var origin = vec2<f32>(f32(job.y), f32(job.z)) * page_m - vec2<f32>(terrain.half_width);
    // Texels past the map's edge (a page's border, the last pages' overhang) bake the edge's
    // surface: baked as "no surface" (normal −1000, relief shadow 0), the bilinear filter
    // blended them into the last real texels, a thin dark line along the terrain's silhouette
    // against whatever lies beyond the map (arc-sweep, Switchbacks flyover).
    // (Inset by a quarter texel, not a fixed millimetre, which rounds back onto the edge on
    // very large maps.)
    // The quadtree spans `-half_width .. world_width - half_width` (its grid origin folded into
    // `half_width`, ADR-8179); for a map on the world origin both ends are `±half_width` exactly.
    // A map deeper than it is wide uses the depth lane for that square (ADR-8369).
    var edge_lo = vec2<f32>(-terrain.half_width + 0.25 * texel_m);
    var edge_hi = vec2<f32>((terrain.world_width - terrain.half_width) - 0.25 * texel_m);
    if (terrain.depth.y > terrain.world_width) {
        origin = vec2<f32>(f32(job.y), f32(job.z)) * page_m - vec2<f32>(terrain.depth.x);
        edge_lo = vec2<f32>(-terrain.depth.x + 0.25 * texel_m);
        edge_hi = vec2<f32>((terrain.depth.y - terrain.depth.x) - 0.25 * texel_m);
    }
    let xz = clamp(origin + (vec2<f32>(gid.xy) - vec2<f32>(vt_bake.dims.z) + vec2<f32>(0.5)) * texel_m, edge_lo, edge_hi);
    let slots = u32(vt_bake.atlas.x);
    let texel = vec2<u32>((job.w % slots) * slot_size, (job.w / slots) * slot_size) + gid.xy;
    return VtBakeAt(xz, texel_m, texel, gid.x < slot_size && gid.y < slot_size);
}

// What one bake invocation stores: maps A–D, the horizon slopes for map E (whole-surface
// mode) and how much of it (`write`: 0 = nothing, past the slot; 1 = map B only, debug view
// 96; 4 = every map).
struct VtBakeOut {
    a: vec4<f32>,
    b: vec4<f32>,
    c: vec4<f32>,
    d: vec4<f32>,
    e: vec4<f32>,
    texel: vec2<u32>,
    write: u32,
};

fn vt_bake_texel(gid: vec3<u32>) -> VtBakeOut {
    let at = vt_bake_at(gid);
    let none = vec4<f32>(0.0);
    if (!at.ok) {
        return VtBakeOut(none, none, none, none, none, at.texel, 0u);
    }
    let xz = at.xz;
    let texel_m = at.texel_m;
    let g = vt_geo(xz);
    if (!g.ok) {
        return VtBakeOut(
            vec4<f32>(1.0, 1.0, 1.0, 1.0),
            vec4<f32>(-1000.0),
            vec4<f32>(0.0, 0.0, 0.0, 1.0),
            vec4<f32>(0.0, 0.0, 0.0, 0.0),
            vec4<f32>(-1000.0),
            at.texel,
            4u,
        );
    }
    let p = vec3<f32>(xz.x, g.y, xz.y);
    if (vt_geo_debug()) {
        let b = vec4<f32>(fract(g.y), g.m, g.ny, distance(frame.camera_pos_time.xyz, p));
        return VtBakeOut(none, b, none, none, none, at.texel, 1u);
    }
    let footprint = vt_bake.view.x * texel_m / max(g.ny, 1e-3);
    let dist = distance(frame.camera_pos_time.xyz, p);
    let sf = surface_photoreal(p, footprint, dist, g.m, g.vertex_n, g.icy);
    if (vt_bake.atlas.w > 0.5) {
        // Split modes: the terms `surface_rest` takes from the cache (`pre_mode`).
        return VtBakeOut(
            vec4<f32>(sf.rest_factor, sf.cavity),
            sf.horizon_slopes,
            vec4<f32>(sf.cliff_grad, sf.relief_shadow),
            vec4<f32>(sf.rock_grad, sf.terrain_bounce),
            sf.horizon_slopes,
            at.texel,
            4u,
        );
    }
    return VtBakeOut(
        vec4<f32>(sf.albedo, sf.roughness),
        vec4<f32>(sf.n_shade, sf.cavity),
        vec4<f32>(sf.relief_shadow, sf.horizon_sky, sf.terrain_bounce, sf.micro_shadow),
        vec4<f32>(sf.sparkle, sf.indirect_ao, sf.macro_ao, sf.sky_visibility),
        sf.horizon_slopes,
        at.texel,
        4u,
    );
}

@compute @workgroup_size(8, 8, 1)
fn cs_vt_bake(@builtin(global_invocation_id) gid: vec3<u32>) {
    let o = vt_bake_texel(gid);
    if (o.write == 0u) {
        return;
    }
    textureStore(vt_out_b, o.texel, o.b);
    if (o.write == 1u) {
        return;
    }
    textureStore(vt_out_a, o.texel, o.a);
    textureStore(vt_out_c, o.texel, o.c);
    textureStore(vt_out_d, o.texel, o.d);
}

// `cs_vt_bake` plus map E in the same pass (whole-surface mode, on devices with five or more
// storage textures per stage): the slopes `surface_photoreal` computed anyway, no second
// geometry walk. Devices with four use `cs_vt_bake` + `cs_vt_bake_horizon`.
@compute @workgroup_size(8, 8, 1)
fn cs_vt_bake_e(@builtin(global_invocation_id) gid: vec3<u32>) {
    let o = vt_bake_texel(gid);
    if (o.write == 0u) {
        return;
    }
    textureStore(vt_out_b, o.texel, o.b);
    if (o.write == 1u) {
        return;
    }
    textureStore(vt_out_a, o.texel, o.a);
    textureStore(vt_out_c, o.texel, o.c);
    textureStore(vt_out_d, o.texel, o.d);
    textureStore(vt_out_e, o.texel, o.e);
}

// The horizon's steepest slope per sector: `Horizon::slopes` of `horizon_sky_and_bounce`
// (the same 16 loads and the same arithmetic, without the open-sky and bounce terms; keep
// the two in step). The terrain virtual texture's whole-surface mode stores it in map E.
fn horizon_slopes_only(world: vec3<f32>, footprint: f32) -> vec4<f32> {
    let top = height_levels() - 1u;
    let s0 = sample_coord(world.xz);
    let to_s = terrain.sample_max.x / terrain.world_width;
    var sv = vec4<f32>(-1e9);
    for (var a = 0; a < 4; a++) {
        let ang = f32(a) * 1.5707964 + 0.7853982;
        let dir = vec2<f32>(cos(ang), sin(ang));
        var max_steep = -1e9;
        var d = 10.0;
        for (var k = 0; k < 4; k++) {
            let level = min(u32(round(footprint_mip(max(d * 0.25, footprint)))), top);
            let scale = f32(1u << level);
            let dims = level_dims(level);
            let p = (s0 + dir * d * to_s - vec2<f32>((scale - 1.0) * 0.5)) / scale;
            let texel = clamp(vec2<i32>(round(p)), vec2<i32>(0), dims - vec2<i32>(1));
            let h = textureLoad(height_texture, texel, i32(level)).r * terrain.height_scale + terrain.height_offset;
            max_steep = max(max_steep, (h - world.y) / d);
            d *= 3.0;
        }
        let pick = vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3));
        sv = select(sv, vec4<f32>(max_steep), pick > vec4<f32>(0.5));
    }
    return sv;
}

// Map E of the whole-surface mode (same jobs as `cs_vt_bake`): the horizon slopes at the
// texel, under the condition and with the footprint `surface_rest` computes them with
// (−1000 = not computed, as there). The slopes depend on the heightfield alone, so the
// page needs no re-bake for a sun, sky or fill change.
@compute @workgroup_size(8, 8, 1)
fn cs_vt_bake_horizon(@builtin(global_invocation_id) gid: vec3<u32>) {
    let at = vt_bake_at(gid);
    if (!at.ok) {
        return;
    }
    let g = vt_geo(at.xz);
    var slopes = vec4<f32>(-1000.0);
    if (g.ok) {
        let p = vec3<f32>(at.xz.x, g.y, at.xz.y);
        let dist = distance(frame.camera_pos_time.xyz, p);
        if (frame.ground_ambient.w > 0.5 && dist < 2000.0 && !cost_off(88.0)) {
            let footprint = vt_bake.view.x * at.texel_m / max(g.ny, 1e-3);
            slopes = horizon_slopes_only(p, footprint);
        }
    }
    textureStore(vt_out_e, at.texel, slopes);
}

// Low-sky radiance toward horizon sector `a` (the diagonal sectors of
// `horizon_sky_and_bounce`), from the sky irradiance (E/π) of a vertical face turned to the
// sector with the map's ground half taken off. A uniform quarter sky gives that face L/2;
// the cosine-weighted quarter spreads a glow concentrated near the horizon band over the
// whole quarter, so the band's radiance is taken as twice that mean (×2 fitted to the path
// tracer on Mars switchbacks_wide's sun-facing shaded walls).
fn sector_sky_radiance(a: i32, k_map: f32, ground_half: vec3<f32>) -> vec3<f32> {
    let ang = f32(a) * 1.5707964 + 0.7853982;
    let face = vec3<f32>(cos(ang), 0.0, sin(ang));
    let e = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(face), 0.0).rgb * k_map;
    return max(4.0 * (e - ground_half), vec3<f32>(0.0));
}

// The bright part of an anisotropic sky that the terrain horizon hides (E/π). The horizon term
// (`horizon_sky`) takes an isotropic share of the sky light off; under a dusty sky the
// circumsolar glow is several times the rest of the horizon, and the ridge that puts a
// sun-facing slope in shadow hides it too (Mars switchbacks_wide: sun-facing shaded walls at
// 1.4–1.5× the path tracer, walls turned away at 1.0–1.2). Per sector: the band from the
// horizontal up to the horizon slope, 90° wide, seen through the receiver's view factor
// (n·V/π, the band vector irradiance of `horizon_sky_and_bounce`), times the sector's sky
// radiance above the four sectors' mean. Zero under an isotropic or black (airless) sky.
fn sky_excess_hidden(slopes: vec4<f32>, n: vec3<f32>, k_map: f32, ground_half: vec3<f32>) -> vec3<f32> {
    if (slopes.x < -999.0) {
        return vec3<f32>(0.0);
    }
    let l0 = sector_sky_radiance(0, k_map, ground_half);
    let l1 = sector_sky_radiance(1, k_map, ground_half);
    let l2 = sector_sky_radiance(2, k_map, ground_half);
    let l3 = sector_sky_radiance(3, k_map, ground_half);
    let mean = 0.25 * (l0 + l1 + l2 + l3);
    var hidden = vec3<f32>(0.0);
    for (var a = 0; a < 4; a++) {
        let ang = f32(a) * 1.5707964 + 0.7853982;
        let dir = vec2<f32>(cos(ang), sin(ang));
        let pick = vec4<f32>(f32(a == 0), f32(a == 1), f32(a == 2), f32(a == 3));
        let t = max(dot(slopes, pick), 0.0);
        let open = 1.0 / (1.0 + t * t);
        let band_a = 0.5 * atan(t) + 0.5 * t * open;
        let band_b = 0.5 * (1.0 - open);
        let view = max(dot(n, vec3<f32>(dir.x * 1.4142135 * band_a, 1.5707964 * band_b, dir.y * 1.4142135 * band_a)), 0.0) * INV_PI;
        let l = l0 * pick.x + l1 * pick.y + l2 * pick.z + l3 * pick.w;
        hidden += view * max(l - mean, vec3<f32>(0.0));
    }
    return hidden;
}

// Visibility of the sky body's fill over the terrain horizon: the horizon slope toward the
// body (`slopes`, the four diagonal sectors of `horizon_sky_and_bounce`, interpolated in
// azimuth) against the body's elevation, with a soft edge of about ±7°. A trench floor whose
// walls hide Jupiter got its full fill (the path tracer's disc light is shadowed: Europa's
// shaded flats printed at 1.3× the tracer). 1 where the horizon was not computed. Gentle
// ground (`shade`); steep faces march toward the body (`fill_horizon_march`).
fn fill_horizon_vis(slopes: vec4<f32>, to_fill: vec3<f32>) -> f32 {
    if (slopes.x < -999.0) {
        return 1.0;
    }
    let s = fract((atan2(to_fill.z, to_fill.x) - 0.7853982) / 6.2831853) * 4.0;
    let i0 = u32(floor(s)) % 4u;
    let i1 = (i0 + 1u) % 4u;
    let p0 = vec4<f32>(f32(i0 == 0u), f32(i0 == 1u), f32(i0 == 2u), f32(i0 == 3u));
    let p1 = vec4<f32>(f32(i1 == 0u), f32(i1 == 1u), f32(i1 == 2u), f32(i1 == 3u));
    let horizon_t = mix(dot(slopes, p0), dot(slopes, p1), fract(s));
    let fill_t = to_fill.y / max(length(to_fill.xz), 1e-4);
    return clamp((atan(fill_t) - atan(horizon_t)) * 4.0 + 0.5, 0.0, 1.0);
}

// Visibility of the sky body's fill over the terrain horizon, marched toward the body: the
// steepest slope to nine heightfield points along its azimuth, 1.5 m to 0.4 km out (×2 per
// tap, each a point load at the pyramid level whose texel is an eighth of the distance),
// against the body's elevation with a soft edge of about ±7° (Jupiter's disc is 12° across).
// A trench floor whose walls hide the body must not get its fill (the path tracer's disc
// light is shadowed). The four diagonal sectors of `horizon_sky_and_bounce` (10/30/90/270 m,
// interpolated in azimuth) that did this before missed a borehole's far rim
// between their taps and the relief a few metres from a wall: Bore Twelve's shaded borehole
// walls took 1.6× the path tracer's Jupiter-shine (Jupiter 17° up, 12° across); against the
// heightfield's own disc visibility this march is within 0.01–0.12 on walls (the sectors
// 0.15–0.3). Fourteen taps (1 m ×1.6) were 0.01–0.09 but cost Bore Twelve's forward pass
// 0.2–0.27 ms on the RTX 5080.
fn fill_horizon_march(p: vec3<f32>, to_fill: vec3<f32>, footprint: f32) -> f32 {
    let horiz = length(to_fill.xz);
    if (horiz < 1e-4) {
        return 1.0;
    }
    let dir = to_fill.xz / horiz;
    let fill_t = to_fill.y / horiz;
    let top = height_levels() - 1u;
    let to_s = terrain.sample_max.x / terrain.world_width;
    let s0 = sample_coord(p.xz);
    var max_t = -1e9;
    // The horizon slope past which the result is 0 whatever the remaining taps find (it
    // only grows): stop there.
    let hidden_t = tan(min(atan(fill_t) + 0.13, 1.55));
    var d = 1.5;
    for (var k = 0; k < 9; k++) {
        let level = min(u32(round(footprint_mip(max(d * 0.125, footprint)))), top);
        let scale = f32(1u << level);
        let dims = level_dims(level);
        let q = (s0 + dir * d * to_s - vec2<f32>((scale - 1.0) * 0.5)) / scale;
        let texel = clamp(vec2<i32>(round(q)), vec2<i32>(0), dims - vec2<i32>(1));
        let h = textureLoad(height_texture, texel, i32(level)).r * terrain.height_scale + terrain.height_offset;
        max_t = max(max_t, (h - p.y) / d);
        if (max_t >= hidden_t) {
            return 0.0;
        }
        d *= 2.0;
    }
    return clamp((atan(fill_t) - atan(max_t)) * 4.0 + 0.5, 0.0, 1.0);
}

// `occluders`: the large-mesh proxies apply (the shared pass; a constant per entry, so the private
// entry compiles without the term: Mesa 26.1's lavapipe (LLVM 19) crashes running `fs_main`
// with it, even when only the count is read).
fn shade(in: VOut, shadow: f32, ssao: f32, ground_sun: f32, cached: Surf, use_cached: bool, occluders: bool) -> vec4<f32> {
    // The terrain is read and shaded at the heightfield point under the pixel (`base`):
    // cliff relief moves the drawn surface (`in.world`; shadows and depth use it), and its
    // effect on the shading is the normal from the relief's gradient (`cliff_relief`).
    let in_world = in.base;
    var n_geo = normalize(in.normal);
    var slope = acos(clamp(n_geo.y, -1.0, 1.0));
    let altitude = in_world.y;
    let count = i32(clamp(layers.count.x, 0.0, 8.0));

    // Outputs both material paths fill (debug views read them too).
    var albedo = vec3<f32>(0.0);
    var roughness = 0.0;
    var n_shade = n_geo;
    var w_first3 = vec3<f32>(0.0);
    // Sky-ambient scale (legacy path: 1 = the flat M13 ambient).
    var sky_visibility = 1.0;
    // Photoreal-path extras for the lighting below (identity on the legacy path).
    var relief_shadow = 1.0;
    var cavity = 1.0;
    // Canyon-scale open-sky fraction for the hemisphere sky term (1 = open sky).
    var horizon_sky = 1.0;
    // Sunlit-terrain bounce factor (`horizon_sky_and_bounce`).
    var terrain_bounce = 0.0;
    // The horizon's steepest slope per sector (`Surf::horizon_slopes`; −1000 where not computed).
    var horizon_slopes = vec4<f32>(-1000.0);
    // Occlusion of indirect (sky/ground) light from the layers' ORM maps and the macro AO
    // (glTF 2.0 `occlusionTexture`: indirect lighting only — it does not dim the sun).
    var indirect_ao = 1.0;
    // Whole-texture mean of that ORM occlusion (the layers' last mip, blended like it): what
    // the texture's AO costs a flat surface on average.
    var orm_mean = 1.0;
    // Baked heightfield AO from the macro map's alpha (DO27: cavity AO over 2–30 m), kept
    // apart from the layers' texture-scale ORM occlusion in `indirect_ao`.
    var macro_ao = 1.0;
    var sparkle = 0.0;
    var footprint_px = 1.0;
    // Near-field detail outputs: micro self-shadow toward the sun, frost (glint density).
    var micro_shadow = 1.0;
    var frost = 0.0;
    // Near-field detail weights for debug view 10: x near-field strength, y icy (crust).
    var near_dbg = vec2<f32>(0.0);
    // @ground-look-begin
    // Local wetness from the ground look (ADR-1408): melt fringes and wet mud.
    var ground_wet = 0.0;
    // @ground-look-end

    if (material.params0.x >= 0.5) {
        // Photoreal material path: the view-independent surface (`surface_photoreal`).
        // World-space pixel footprint, height included: on a cliff the XZ derivatives alone
        // shrink toward zero and the triplanar side planes sampled their finest mip at any
        // distance (vertical streaks on far walls).
        let footprint = pixel_footprint(in_world);
        footprint_px = footprint;
        // Under the cascade's full shadow the relief march is moot (`relief_sun_hidden`).
        relief_sun_hidden = shadow <= 0.0 && frame.ambient_debug.a < 0.5;
        // Debug 98: the surface-cost probe. A flat grey surface in place of
        // `surface_photoreal` measures what caching the surface (terrain virtual texture)
        // can save at most.
        let probe = frame.ambient_debug.a > 97.5 && frame.ambient_debug.a < 98.5;
        let sf = surface_or_probe(
            probe,
            cached,
            use_cached,
            in_world,
            footprint,
            distance(frame.camera_pos_time.xyz, in_world),
            in.geo_mip,
            in.normal,
            in.cliff.x,
        );
        albedo = sf.albedo;
        roughness = sf.roughness;
        n_geo = sf.n_geo;
        slope = sf.slope;
        n_shade = sf.n_shade;
        cavity = sf.cavity;
        w_first3 = sf.w_first3;
        relief_shadow = sf.relief_shadow;
        horizon_sky = sf.horizon_sky;
        terrain_bounce = sf.terrain_bounce;
        horizon_slopes = sf.horizon_slopes;
        indirect_ao = sf.indirect_ao;
        orm_mean = sf.orm_mean;
        macro_ao = sf.macro_ao;
        sparkle = sf.sparkle;
        micro_shadow = sf.micro_shadow;
        frost = sf.frost;
        sky_visibility = sf.sky_visibility;
        near_dbg = sf.near_dbg;
        // @ground-look-begin
        if (material.ground.x > 0.0 || material.winter.x > 0.0 || material.winter2.y > 0.0) {
            let gl = ground_look(in_world, n_geo, albedo, roughness);
            albedo = gl.albedo;
            roughness = gl.roughness;
            ground_wet = gl.wet;
        }
        // @ground-look-end
    } else {
        // =====================================================================
        // Legacy path (M13 look, unchanged)
        // =====================================================================
        // Splat weights: slope/altitude bands, mottled by per-layer noise (height-blend look).
        // The mottle factor stays strictly positive so weights blend smoothly; the normalised
        // albedo is always a convex blend of the layer albedos.
        var total_w = 0.0;
        var trip_w = 0.0;
        for (var i = 0; i < count; i++) {
            let slot = layers.slots[i];
            let ws = band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
            let wa = band(altitude, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
            let mottle = fbm(in_world.xz * 0.35 + vec2<f32>(f32(i) * 17.13, f32(i) * -9.71));
            let w = max(ws * wa, 0.0) * (0.55 + 0.9 * mottle);
            albedo += slot.albedo_roughness.rgb * w;
            roughness += slot.albedo_roughness.a * w;
            trip_w += w * clamp(slot.params.z, 0.0, 1.0);
            total_w += w;
            if (i == 0) {
                w_first3.x = w;
            } else if (i == 1) {
                w_first3.y = w;
            } else if (i == 2) {
                w_first3.z = w;
            }
        }
        if (total_w <= 1e-5) {
            // No band covers this fragment: fall back to the base layer.
            albedo = layers.slots[0].albedo_roughness.rgb;
            roughness = layers.slots[0].albedo_roughness.a;
            trip_w = 0.0;
            w_first3 = vec3<f32>(1.0, 0.0, 0.0);
        } else {
            albedo /= total_w;
            roughness /= total_w;
            trip_w /= total_w;
            w_first3 /= total_w;
        }

        // Detail modulation; triplanar on steep faces, weighted by the blended layer flags.
        let steep = clamp((slope - 0.45) * 2.2, 0.0, 1.0);
        let flat_detail = fbm(in_world.xz * 1.7);
        let cliff_detail = fbm(in_world.zy * 1.7) * abs(n_geo.x) + fbm(in_world.xy * 1.7) * abs(n_geo.z);
        let detail = mix(flat_detail, cliff_detail, steep * (0.25 + 0.75 * trip_w));
        // Amplitude kept modest: bright layers (snow, foam) have little headroom below the
        // AgX shoulder, and a +14% detail spike would push them into flat near-white.
        albedo = albedo * (0.90 + 0.20 * detail);
    }

    // Road decal: same fragment, same depth. A mesh drawn in a second pass would
    // z-fight; replacing the albedo here cannot.
    if (material.road1.x > 0.0) {
        let a = material.road0.xy;
        let b = material.road0.zw;
        let ab = b - a;
        let t = clamp(dot(in_world.xz - a, ab) / max(dot(ab, ab), 1e-6), 0.0, 1.0);
        let closest = a + ab * t;
        if (distance(in_world.xz, closest) < material.road1.x) {
            albedo = material.road1.yzw;
            roughness = 0.85;
        }
    }

    var colour = albedo;

    // Scorch darkening.
    let scorch_count = i32(clamp(scorch.rows[16].x, 0.0, 16.0));
    for (var m = 0; m < scorch_count; m++) {
        let row = scorch.rows[m];
        let dist = distance(in_world.xz, row.xy) / max(row.z, 1e-3);
        if (dist < 1.0) {
            let s = clamp(row.w, 0.0, 1.0) * (1.0 - dist) * (1.0 - dist);
            colour *= 1.0 - 0.85 * s;
        }
    }

    // @surface-weather-begin
    // The surface weather (spec 99 § 6.4), after the surface stage and before lighting. The
    // ground look's static snow and wet mud are already in `colour`: this goes over them.
    if (material.weather.x + material.weather.y + material.weather.z > 0.0) {
        let response = terrain_weather_response(in_world, slope, roughness, footprint_px, material.params0.x >= 0.5);
        var hollow = 0.0;
        if (material.weather.y > 0.0 && n_geo.y > 0.97 && response.w > 0.5) {
            hollow = terrain_weather_hollow(in_world.xz);
        }
        let weathered = weather_apply(
            WeatherSurface(colour, roughness, 0.0, n_shade),
            n_geo,
            in_world,
            material.weather,
            material.weather2.x,
            response,
            weather_exposure,
            hollow,
        );
        colour = weathered.albedo;
        roughness = weathered.roughness;
        n_shade = weathered.normal;
    }
    // @surface-weather-end
    // Wetness: darker, glossier.
    // @ground-look-begin
    let wetness = clamp(max(frame.sun_dir_wetness.w, ground_wet), 0.0, 1.0);
    // @ground-look-end
    // @ground-look-off let wetness = clamp(frame.sun_dir_wetness.w, 0.0, 1.0);
    colour *= 1.0 - 0.35 * wetness;

    // Lighting, `regia_render::forward` convention: π-normalised Lambert diffuse of the
    // EV100-exposed sun radiance plus the sky ambient. The missing 1/π in earlier rounds
    // blew sun-lit ground past AgX's shoulder to flat white.
    let n = normalize(n_shade);
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    let ndl = max(dot(n, sun_dir), 0.0);
    let sun_radiance = frame.sun_color_exposure.rgb * frame.sun_color_exposure.a;
    // `shadow` is the private sun map (`fs_main`) or the engine CSM (`fs_shared`); the
    // photoreal path adds the heightfield's own self-shadowing.
    let sun_vis = min(shadow, relief_shadow) * micro_shadow;
    // @ground-look-begin
    if (material.winter2.z > 0.0 && material.params0.x >= 0.5) {
        colour = hoarfrost(colour, in_world, n_geo, sun_vis * smoothstep(0.0, 0.2, ndl));
        roughness = mix(roughness, 0.95, 0.5 * material.winter2.z * (1.0 - sun_vis));
    }
    // @ground-look-end
    // Sky light: the hemisphere sky/ground model on the shared path (`SkyAmbient`), the
    // legacy sky-visibility ambient otherwise.
    var ambient = frame.ambient_debug.rgb * sky_visibility;
    if (frame.ground_ambient.w > 0.5) {
        let up = 0.5 + 0.5 * n.y;
        ambient = frame.ambient_debug.rgb * up * horizon_sky + frame.ground_ambient.rgb * (1.0 - up);
    }
    // With the engine's irradiance map the sky light is directional, exactly as on a mesh
    // of this normal: a slope toward a low sun sees the bright circumsolar sky, one away
    // from it less (the uniform dome above gave both the same, and DO27's road and pad
    // meshes printed up to 1.7x the ground they lie on).
    if (frame.output_mode.w > 0.0) {
        let k_map = frame.output_mode.w;
        let e_n = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(n), 0.0).rgb * k_map;
        // The map's lower hemisphere is an infinite, sunlit ground of the planet's albedo
        // (`environment_radiance`): its radiance is what a down-facing normal receives, and a
        // normal n takes it through the (1 − n.y)/2 view factor. Split it off: the sky part
        // keeps the canyon occlusion below; the ground part becomes the ground this point
        // actually sees — its own layer albedo, lit by the sun only where the ground is
        // sunlit (`ground_sun`: the cascades 8 m out in front of the face, so a wall's foot in
        // its own shadow and a shadowed canyon floor light it with sky alone) plus the sky.
        // The sunlit walls above
        // the horizon are the bounce term's (no double counting: that is the upper
        // hemisphere). The path tracer put real-time GI in canyon shadow at 1.56× with the
        // bounce off (switchbacks_wide), mostly this sunlit-ground light on shadowed slopes.
        let l_ground_map = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(vec3<f32>(0.0, -1.0, 0.0)), 0.0).rgb * k_map;
        let e_up = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(vec3<f32>(0.0, 1.0, 0.0)), 0.0).rgb * k_map;
        let ground_view = clamp(0.5 - 0.5 * n.y, 0.0, 1.0);
        let sky_part = max(e_n - l_ground_map * ground_view, vec3<f32>(0.0));
        let ground_lit = sun_radiance * max(sun_dir.y, 0.0) * INV_PI * ground_sun;
        // The ground is lit by more than the sun and the open sky: by the planet-shine fill,
        // and by the sunlit walls around it (in `terrain_bounce`: `surface_rest`). Without
        // them a wall above a shadowed ice floor saw a black floor (Europa's trench walls at
        // 0.28x the path tracer's indirect light).
        var ground_fill = vec3<f32>(0.0);
        if (frame.fill_dir.w > 0.5) {
            ground_fill = frame.fill_color.rgb * max(normalize(frame.fill_dir.xyz).y, 0.0) * INV_PI;
        }
        let l_ground = min(colour, vec3<f32>(0.95)) * (ground_lit + e_up * horizon_sky + ground_fill);
        // Canyon walls hide part of the sky from up-facing ground (`horizon_sky_and_bounce`;
        // 1 on open ground, so a slab on open terrain still matches a mesh).
        // Under a dusty sky the horizon also hides the circumsolar glow on slopes and walls
        // (`sky_excess_hidden`); gentle ground keeps the isotropic estimate, as the wall
        // bounce keeps the horizontal receiver's factor there (`horizon_sky_and_bounce`).
        let sky_seen = sky_part * mix(1.0, horizon_sky, 0.5 + 0.5 * n.y);
        let wall_w = clamp((0.75 - n.y) / 0.2, 0.0, 1.0);
        var hidden = vec3<f32>(0.0);
        if (wall_w > 0.0) {
            hidden = sky_excess_hidden(horizon_slopes, n, k_map, 0.5 * l_ground_map) * wall_w;
        }
        ambient = max(sky_seen - hidden, vec3<f32>(0.0)) + l_ground * ground_view;
    }
    // The engine's irradiance probes (shared path, when the frame runs them) trace the sky,
    // the terrain and the meshes: where they cover this point they replace the sky and
    // ground term (its canyon occlusion included) and the sunlit-wall bounce estimate below.
    var bounce_weight = 1.0;
    // The specular lobe of the sky reflection below (its roughness, as the reflection's BRDF
    // table reads it).
    let probe = engine_gi(
        in.position.xy,
        in.world,
        n,
        normalize(frame.camera_pos_time.xyz - in.world),
        frame.camera_pos_time.xyz,
        clamp(roughness, 0.0, 1.0),
    );
    let probe_gi = probe.diffuse;
    if (probe_gi.a > 0.0) {
        ambient = mix(ambient, probe_gi.rgb, probe_gi.a);
        bounce_weight = 1.0 - probe_gi.a;
        // Under the probes the layers' ORM occlusion only redistributes light within the
        // texture (crevices darker, crests up to the probe irradiance): divided by its own
        // whole-texture mean. The probes already carry the occlusion of the geometry they trace,
        // and a texture's mean AO is no geometry: DO27's dune sand (an aerial scan, 16-64 m
        // tiles) averages 0.36, which printed every close-up floor at 0.6x its irradiance
        // (gi-probes round 3: Depth 0.48x the path tracer on the Earth-sky close-up). The
        // analytic terms keep the plain AO (their ambient is calibrated with it).
        let orm_rel = min(indirect_ao / max(orm_mean, 0.05), 1.0);
        indirect_ao = mix(indirect_ao, orm_rel, probe_gi.a);
    }
    // A parked hull or crate (`MeshOccluderBlock`): its faces replace the sky they hide (its
    // bounce onto this ground and the darkening under it), and it shadows the fill. Where the
    // irradiance probes cover this point they already trace the meshes, so the box term only
    // fills the rest (`1 − probe_gi.a`) and the hull is not counted twice.
    var occ_fill = 1.0;
    if (occluders && mesh_occ.header.x > 0.5) {
        let o = mesh_occluder_light(in.world, n, true);
        ambient = max(ambient + o.delta * (1.0 - probe_gi.a), vec3<f32>(0.0));
        occ_fill = o.fill_vis;
    }
    // The engine SSAO darkens the sky and ground light at contacts and cliff feet, as on
    // meshes (1 on the private path and without SSAO); the analytic rock pool adds to it.
    // Contact grounding: rocks sit in a soft pool — less sky, a little less sun (the crease the
    // shadow map is too coarse to resolve), and a darker, dustier ground right at the base.
    let occ = contact_occlusion(in_world.xz);
    colour *= 1.0 - contact.strength.z * occ;
    // Sky visibility of the indirect light. The heightfield cavity, the baked macro AO and the
    // screen-space AO all estimate the same occlusion over overlapping ranges (cavity: the
    // footprint ×8; DO27's macro AO: 2–30 m; SSAO: the near screen neighbourhood), so they
    // combine by min — their product counted one crease two or three times (look-r2 round
    // 10: switchbacks_close's shadows printed at 0.57× the path tracer). The layers' ORM
    // occlusion is texture-scale and multiplies. Multi-bounce (Jimenez et al. 2016): light
    // that enters a crease bounces off its bright walls, so occlusion on a high-albedo
    // surface removes less light than the visibility alone says. SSAO joins the min after
    // that mapping: its strength was tuned by eye on the final contact shade (and the ao-il
    // lane owns it), so it is not lifted a second time.
    let vis = min(
        multi_bounce_ao(min(cavity, macro_ao) * indirect_ao, colour),
        vec3<f32>(ssao * indirect_ao),
    );
    // The irradiance probes trace the heightfield itself, so the light they hand this point
    // already lacks the sky and walls its relief hides: the heightfield cavity and the baked
    // macro AO (relief occlusion over 2–30 m) are not applied to it a second time. The probe
    // share keeps the texture-scale ORM occlusion and the screen-space AO, as a mesh under the
    // probes does (`lab_forward.wgsl`: its probe light takes only its own AO and the SSAO).
    // Stacking both printed Mars's shaded canyon walls at 0.65× the path tracer (ADR-169).
    var ambient_vis = vis;
    if (probe_gi.a > 0.0) {
        let probe_vis = min(multi_bounce_ao(indirect_ao, colour), vec3<f32>(ssao * indirect_ao));
        ambient_vis = mix(vis, probe_vis, probe_gi.a);
    }
    var lit = colour
        * (sun_radiance * ndl * sun_vis * INV_PI * (1.0 - contact.strength.y * occ)
            + ambient * ambient_vis * (1.0 - contact.strength.x * occ)
            // Light bounced off sunlit walls and slopes onto this point (ice trench floors):
            // the walls' radiance ρ_wall · E_sun · cos / π over their view factor (ρ_wall ≈
            // this albedo), reflected by this surface (× `colour` outside the bracket).
            + min(colour, vec3<f32>(0.95)) * sun_radiance * terrain_bounce * bounce_weight * INV_PI * vis);
    // Planet-shine (Jupiter-shine): a Lambert fill from the sky body, hidden where the terrain
    // horizon toward it rises above it (`fill_horizon_march`, `fill_horizon_vis`).
    var fill_horizon = 1.0;
    if (frame.fill_dir.w > 0.5) {
        // Steep faces march toward the body (nine loads), where the fill lights them; gentle
        // ground (most of the screen) reads the four horizon sectors it already has.
        if (horizon_slopes.x >= -999.0 && dot(n, normalize(frame.fill_dir.xyz)) > 0.0) {
            if (n.y < 0.85) {
                fill_horizon = fill_horizon_march(in_world, normalize(frame.fill_dir.xyz), footprint_px);
            } else {
                fill_horizon = fill_horizon_vis(horizon_slopes, normalize(frame.fill_dir.xyz));
            }
        }
        // `fill_dir.w` = 2 (`FillCanyonScope::MapWide`): only canyon-scale and contact
        // occlusion; texture-scale crevice AO is a cosine-weighted sky estimate, not the
        // visibility of a small disc above the local horizon. 3 (`NearOccluders`, ADR-8412):
        // that only in a mesh proxy's sun shade (`mesh_occluder_shade`: under and beside a
        // hull), the full stack elsewhere (trench and canyon floors keep theirs). CPU mirror:
        // `regia_terrain::fill_visibility`.
        var fill_vis = vis;
        if (frame.fill_dir.w > 2.5) {
            if (occluders && mesh_occ.header.x > 0.5) {
                let toward = mesh_occluder_shade(in.world);
                if (toward > 0.0) {
                    fill_vis = mix(vis, vec3<f32>(min(macro_ao, ssao)), toward);
                }
            }
        } else if (frame.fill_dir.w > 1.5) {
            fill_vis = vec3<f32>(min(macro_ao, ssao));
        }
        var fill = colour * frame.fill_color.rgb * max(dot(n, normalize(frame.fill_dir.xyz)), 0.0) * INV_PI * fill_vis
            * fill_horizon;
        if (occluders && mesh_occ.header.x > 0.5) {
            fill *= occ_fill;
        }
        // A baked volume's light already holds the fill, shadowed by the bake.
        if (engine_gi_has_fill()) {
            fill *= 1.0 - probe_gi.a;
        }
        lit += fill;
    }

    let view_dir = normalize(frame.camera_pos_time.xyz - in_world);
    // Perceptual roughness of the specular lobe (wet ground is smoother); the photoreal path
    // adds specular anti-aliasing below. The local lights use it too.
    var spec_perceptual = clamp(mix(roughness, 0.2, wetness * 0.8), 0.04, 1.0);
    if (material.params0.x >= 0.5) {
        // Cook–Torrance GGX specular of a dielectric (f0 0.03: between ice 0.018 and rock
        // 0.04); wet ground is smoother. Bright crystalline layers add discrete facet
        // glints (`glint`).
        // Uniform branch (`material` is a uniform), so the derivatives are defined.
        let rough = specular_aa_roughness(spec_perceptual, dpdx(n), dpdy(n));
        spec_perceptual = rough;
        // In full shadow the sun's lobe adds nothing (and costs the glint hash).
        if (sun_vis > 0.0) {
            var spec = specular_ggx(n, view_dir, sun_dir, rough, 0.03);
            if (sparkle > 0.0) {
                spec += sparkle * glint(in_world, n, view_dir, sun_dir, footprint_px, 0.06 + 0.16 * frost);
            }
            lit += sun_radiance * spec * sun_vis;
        }
    } else {
        // Legacy Blinn specular on wet ground (M13 look). The blended roughness is clamped
        // to 0..1 before the exponent mix: an unclamped blend above 1 extrapolated the mix
        // to a negative exponent, and pow(x, negative) exploded to +inf → NaN blotches.
        let half_dir = normalize(view_dir + sun_dir);
        let exponent = mix(96.0, 16.0, clamp(roughness, 0.0, 1.0));
        let specular = pow(max(dot(n, half_dir), 0.0), exponent);
        lit += sun_radiance * INV_PI * specular * shadow * (0.03 + 0.5 * wetness) * (1.0 - roughness * 0.5);
    }
    // Sky reflection: the F0 part of the mesh path's split-sum weight (F0·A from the
    // engine's table) on the sky about the normal, faded by N·V. Near normal incidence this
    // is the sky reflection a rough dielectric mesh gets (A · 0.04, B ≈ 0), so grey ground
    // and a grey mesh match. Toward grazing it fades out instead of growing: over rough
    // regolith the reflected sky there is the horizon (Mars's brightest sky), which canyon
    // walls and the relief itself hide; reflecting it lifted shadowed canyon floors by a
    // third (look_r4 lit/shadowed 3.5 → 2.7).
    // Kept for the stochastic-SSR surface code: the weight and radiance a traced hit replaces.
    var sky_weight = 0.0;
    var sky_spec = vec3<f32>(0.0);
    if (frame.output_mode.w > 0.0) {
        let no_v = max(dot(n, view_dir), 1e-3);
        // Clamped half a texel in from the edges: the sampler repeats in u for the sky map.
        let uv = clamp(vec2<f32>(no_v, clamp(roughness, 0.0, 1.0)), vec2<f32>(0.01), vec2<f32>(0.99));
        let ab = textureSampleLevel(sky_brdf, sky_samp, uv, 0.0).rg;
        sky_weight = 0.04 * ab.x * no_v;
        // Under the probes with probe specular on, their share of the reflected light is the
        // probe radiance in the specular lobe, not their irradiance toward the normal: wet,
        // glossy ground reflects the lit wall it faces rather than its own up-facing light
        // (ADR-1377, as a mesh's probe specular does). `ambient` holds the probes' irradiance at
        // their share `a`, which the lobe light replaces; uniform light reads the same either way.
        var reflected = ambient;
        if (probe.has_spec && probe_gi.a > 0.0) {
            reflected = max(ambient + (probe.spec - probe_gi.rgb) * probe_gi.a, vec3<f32>(0.0));
        }
        sky_spec = reflected * sky_weight * ambient_vis;
        lit += sky_spec;
    }
    // Point and spot lights (muzzle flashes, hull lamps, explosions): the light a mesh of
    // this albedo, roughness and normal takes here, with the local lights' shadows (gap A of
    // lane v017-black-hull). The drawn surface (`in.world`, relief included) is what the
    // froxels and the shadow atlas see; the heightfield normal offsets the shadow lookup.
    if (engine_local_lights_on()) {
        lit += engine_local_lights(
            in.position.xy,
            in.world,
            n,
            normalize(n_geo),
            normalize(frame.camera_pos_time.xyz - in.world),
            colour,
            vec3<f32>(0.03),
            spec_perceptual,
        );
    }
    // The frame's extra directional lights, unshadowed (ADR-7254; 0 without any).
    lit += extra_lights_direct(n, view_dir, colour, spec_perceptual, 0.03);

    // Debug views 1–17 (`TerrainFrame::debug_view`); 98 is the surface-cost probe (no view).
    let dbg = select(frame.ambient_debug.a, 0.0, frame.ambient_debug.a > 20.0);
    // Debug views print raw values; on the shared HDR target they are scaled so 1 prints
    // like a white Lambert surface in full sun (the frame's exposure applies after).
    let dbg_k = select(1.0, max(luma3(sun_radiance) * INV_PI, 1e-6), frame.output_mode.x > 0.5);
    if (dbg > 16.5 && dbg < 17.5) {
        // Drawn geometry: R the cliff relief's vertical offset of the drawn surface from the
        // heightfield (0.5 + metres / 16), G the drawn surface's distance from the eye
        // (metres / 1000), B 1 (the scale reference).
        let lift = 0.5 + (in.world.y - in.base.y) / 16.0;
        let dist = distance(frame.camera_pos_time.xyz, in.world) / 1000.0;
        return vec4<f32>(vec3<f32>(lift, dist, 1.0) * dbg_k, 1.0);
    }
    if (dbg > 15.5 && dbg < 16.5) {
        // The shading normal the sun and sky light use (world, 0.5 + 0.5·n): the reference
        // tracer's camera-ray terrain normal (`render_normal_view`).
        return vec4<f32>((n * 0.5 + vec3<f32>(0.5)) * dbg_k, 1.0);
    }
    if (dbg > 14.5 && dbg < 15.5) {
        // Indirect occlusion inputs (2/2): R layers' ORM occlusion, G screen-space AO, B 1.
        return vec4<f32>(vec3<f32>(indirect_ao, ssao, 1.0) * dbg_k, 1.0);
    }
    if (dbg > 13.5 && dbg < 14.5) {
        // Indirect occlusion inputs (1/2): R heightfield cavity, G baked macro AO, B probe
        // coverage (the share of the sky term the irradiance probes replace).
        return vec4<f32>(vec3<f32>(cavity, macro_ao, max(probe_gi.a, 0.0)) * dbg_k, 1.0);
    }
    if (dbg > 10.5 && dbg < 11.5) {
        // Sun visibility as grey: the cascade (engine CSM) shadow alone.
        return vec4<f32>(vec3<f32>(shadow) * dbg_k, 1.0);
    }
    if (dbg > 9.5) {
        // Near-field detail: R strength (distance and slope fade), G icy (crust vs grain),
        // B frost coverage.
        return vec4<f32>(vec3<f32>(near_dbg.x, near_dbg.y, frost) * dbg_k, 1.0);
    }
    if (dbg > 8.5) {
        // Indirect light split, stored raw (not times dbg_k). R sky and ground ambient,
        // G planet-shine fill, B the terrain bounce before probes replace it. The lit
        // frame multiplies that bounce by `1 - probe coverage`.
        var fill_l = 0.0;
        if (frame.fill_dir.w > 0.5) {
            fill_l = luma3(frame.fill_color.rgb) * max(dot(n, normalize(frame.fill_dir.xyz)), 0.0) * INV_PI * luma3(vis) * fill_horizon;
        }
        return vec4<f32>(
            luma3(ambient * ambient_vis),
            fill_l,
            luma3(min(colour, vec3<f32>(0.95)) * sun_radiance) * terrain_bounce * INV_PI * luma3(vis),
            1.0,
        );
    }
    if (dbg > 7.5) {
        // Sun light split: R cascade (engine CSM) shadow, G heightfield times micro
        // self-shadow, B N·L, each times dbg_k. The lit visibility is
        // min(shadow, relief_shadow) * micro_shadow. R*G/dbg_k² equals that only when
        // one of the two shadows is 0 or 1.
        return vec4<f32>(vec3<f32>(shadow, relief_shadow * micro_shadow, max(ndl, 0.0)) * dbg_k, 1.0);
    }
    if (dbg > 6.5) {
        // Visual-vs-sim agreement flags. Thresholds match `regia_terrain::sim`:
        // driveable (slope < 0.6 rad) flags red outside ±0.25 m, else green;
        // cliffs flag yellow outside ±2 m, else blue. A 1 m cliff error stays blue —
        // the old 0.5 m heat saturated that case to red.
        var view = vec3<f32>(0.0);
        if (material.sim.y > 0.0) {
            // The authored heightfield against the sim (cliff relief is a render-only
            // detail on faces steeper than the driveable limit and is not compared).
            let delta = abs(in_world.y - sim_height_w(in_world.xz));
            let driveable = slope < 0.6;
            let over = select(delta > 2.0, delta > 0.25, driveable);
            if (over) {
                view = select(vec3<f32>(1.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), driveable);
            } else {
                view = select(vec3<f32>(0.15, 0.35, 1.0), vec3<f32>(0.0, 0.85, 0.2), driveable);
            }
        }
        return vec4<f32>(view * dbg_k, 1.0);
    }
    if (dbg > 5.5 && dbg <= 6.5) {
        let h01 = clamp((altitude - terrain.height_offset) / max(terrain.height_scale, 1e-3), 0.0, 1.0);
        return vec4<f32>(vec3<f32>(h01) * dbg_k, 1.0); // normalised height
    }
    if (dbg > 4.5) {
        let s01 = clamp(slope * 0.6366, 0.0, 1.0);
        return vec4<f32>(vec3<f32>(s01) * dbg_k, 1.0); // slope, π/2 → 1
    }
    if (dbg > 3.5) {
        return vec4<f32>(w_first3 * dbg_k, 1.0); // normalised weights of the first three layers
    }
    if (dbg > 2.5) {
        return vec4<f32>(albedo * dbg_k, 1.0); // blended albedo (pre-lighting)
    }
    if (dbg > 1.5 && dbg <= 2.5) {
        return vec4<f32>(vec3<f32>(ndl) * dbg_k, 1.0); // N·L only
    }
    if (dbg > 0.5 && dbg <= 1.5) {
        // LOD debug tint.
        let hue = fract(in.level * 0.61803);
        lit = mix(lit, vec3<f32>(hue, 1.0 - hue, fract(hue + 0.5)), 0.7);
    }

    // Shared HDR target: scene radiance, roughness in alpha. Post applies exposure
    // once. Height fog uses the same integral as `apply_height_fog`; inscatter is
    // the fog colour lifted into the sky radiance domain (`output_mode.z`).
    if (frame.output_mode.x > 0.5) {
        var rgb = max(lit, vec3<f32>(0.0));
        let params = frame.fog_params;
        if (params.w > 0.5 && params.x > 0.0) {
            if (frame.fog_color.a > 0.5) {
                let tint = frame.fog_color.rgb / max(luma3(frame.fog_color.rgb), 1e-3);
                if (haze_sun_lit < 1.0) {
                    rgb = apply_aerial_under_clouds(
                        rgb,
                        in_world,
                        frame.camera_pos_time.xyz,
                        params,
                        tint,
                        normalize(frame.sun_dir_wetness.xyz),
                        frame.sun_color_exposure.rgb * frame.sun_color_exposure.a,
                        max(frame.output_mode.z, 1.0),
                        haze_sun_lit,
                    );
                } else {
                    rgb = apply_aerial(
                        rgb,
                        in_world,
                        frame.camera_pos_time.xyz,
                        params,
                        tint,
                        normalize(frame.sun_dir_wetness.xyz),
                        frame.sun_color_exposure.rgb * frame.sun_color_exposure.a,
                        max(frame.output_mode.z, 1.0),
                    );
                }
            } else {
                let distance = length(in_world - frame.camera_pos_time.xyz);
                let height_density = params.x * exp(-params.y * (in_world.y - params.z));
                let t = 1.0 - exp(-max(distance, 0.0) * max(height_density, 0.0));
                let inscatter = frame.fog_color.rgb * max(frame.output_mode.z, 1.0);
                rgb = mix(rgb, inscatter, clamp(t, 0.0, 1.0));
            }
        }
        // @surface-weather-begin
        // The rain's or snow's own constant extinction (spec 99 § 3.2), with the fog's color.
        if (material.weather2.z > 0.0) {
            let view_ray = in_world - frame.camera_pos_time.xyz;
            var scattered = frame.fog_color.rgb * max(frame.output_mode.z, 1.0);
            if (frame.fog_color.a > 0.5) {
                let tint = frame.fog_color.rgb / max(luma3(frame.fog_color.rgb), 1e-3);
                if (haze_sun_lit < 1.0) {
                    // Rain under the cloud layer scatters the light under the layer (ADR-5944).
                    scattered = tint * haze_inscatter_under_clouds(
                        normalize(view_ray),
                        normalize(frame.sun_dir_wetness.xyz),
                        frame.sun_color_exposure.rgb * frame.sun_color_exposure.a,
                        max(frame.output_mode.z, 1.0),
                        haze_sun_lit,
                    );
                } else {
                    scattered = tint * haze_inscatter(
                        normalize(view_ray),
                        normalize(frame.sun_dir_wetness.xyz),
                        frame.sun_color_exposure.rgb * frame.sun_color_exposure.a,
                        max(frame.output_mode.z, 1.0),
                    );
                }
            } else if (haze_sun_lit < 1.0) {
                // The flat colour stands for a clear day's sky: under the layer it keeps the
                // haze's floor of it (ADR-5944).
                scattered *= mix(HAZE_SKY_FLOOR, 1.0, haze_sun_lit);
            }
            rgb = weather_extinction(rgb, length(view_ray), material.weather2.z, scattered);
        }
        // @surface-weather-end
        // `fill_color.w`: the shared HDR target's pre-exposure (0 from an older host = 1).
        let hdr_scale = select(1.0, frame.fill_color.w, frame.fill_color.w > 0.0);
        var ssr_alpha = ssr_roughness(roughness, max(dot(n, view_dir), 0.0));
        if (frame.texture_lod.y > 0.5) {
            ssr_alpha = sssr_pack(roughness, sky_weight, luma3(sky_spec) / max(luma3(max(lit, vec3<f32>(0.0))), 1e-6));
        }
        return vec4<f32>(rgb * hdr_scale, ssr_alpha);
    }

    // AgX tonemap. NOTE: no luminance floor — round 2 floored near-black NaNs to flat
    // dark grey and masked the real bug. The input is clamped into the polynomial's
    // working range so the tonemap stays total (inf would turn inf−inf into NaN).
    lit = apply_height_fog(lit, in_world);
    lit = clamp(lit, vec3<f32>(0.0), vec3<f32>(1.0e4));
    // An sRGB target (the hardware encodes) stores linear display values; a plain UNORM
    // LDR target (`output_mode.y`) takes the display-encoded colour directly.
    let display = default_display_transform(lit);
    if (frame.output_mode.y > 0.5) {
        return vec4<f32>(display, 1.0);
    }
    lit = display_to_linear(display);

    // The Rgba8UnormSrgb target applies the sRGB encode on store; no manual gamma here
    // (round 2 encoded twice and washed the image out).
    return vec4<f32>(lit, 1.0);
}

// Normal for the shadow receiver offset: the vertex normal on the legacy path, the
// triangle's own normal from screen derivatives on the photoreal path (its vertex shader
// skips the normal taps). `cross(dpdy, dpdx)` because window y grows downward.
// A sliver triangle (a zero cross product) takes +Y instead of a NaN normal, which would
// reach the slope bias and the shadow compare (ADR-869).
fn receiver_normal(in: VOut) -> vec3<f32> {
    let c = cross(dpdy(in.world), dpdx(in.world));
    let flat_n = select(vec3<f32>(0.0, 1.0, 0.0), normalize(c), dot(c, c) > 1.0e-20);
    let face = select(flat_n, -flat_n, flat_n.y < 0.0);
    return select(normalize(in.normal), face, material.params0.x >= 0.5);
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    // The derivatives in `receiver_normal` before the per-pixel hole discard, so they run in
    // uniform control flow (as `fs_shared` shades, then cuts).
    let rn = receiver_normal(in);
    if (hole_cut && in_hole(in.base.xz, in.ref_xz)) {
        discard;
    }
    let sun = sample_sun_shadow(in.world, rn);
    return shade(in, sun, 1.0, sun, no_surf(), false, false);
}

// The engine SSAO at this pixel (1 without SSAO).
fn shared_ssao(in: VOut) -> f32 {
    var ssao = 1.0;
    if (engine_csm.screen.z > 0.5) {
        let ao_dims = vec2<f32>(textureDimensions(engine_ao));
        let target_px = max(engine_csm.screen.xy, vec2<f32>(1.0));
        let texel = clamp(vec2<i32>(in.position.xy * ao_dims / target_px), vec2<i32>(0), vec2<i32>(ao_dims) - vec2<i32>(1));
        ssao = textureLoad(engine_ao, texel, 0).r;
    }
    return ssao;
}

// Debug views 12 and 13 of the shared pass (`shared_debug_on` says whether one is selected).
fn shared_debug_on() -> bool {
    return frame.ambient_debug.a > 11.5 && frame.ambient_debug.a < 13.5;
}

fn shared_debug(in: VOut) -> vec4<f32> {
    let k = luma3(frame.sun_color_exposure.rgb * frame.sun_color_exposure.a) * INV_PI;
    if (frame.ambient_debug.a > 12.5) {
        // Debug view 13: the heightfield's sun visibility out to ~2 km (coarse march).
        let v = far_heightfield_sun(in.base, normalize(frame.sun_dir_wetness.xyz));
        return vec4<f32>(vec3<f32>(v.x, v.y / 2000.0, 0.0) * k, 1.0);
    }
    // Debug view 12: R = metres the cascade's stored depth sits in front of this point
    // (÷ 2 m), B = metres behind (÷ 2 m), G = cascade index ÷ 3.
    let o = engine_csm_occluder_m(in.world);
    return vec4<f32>(vec3<f32>(clamp(o.x * 0.5, 0.0, 1.0), o.y / 3.0, clamp(-o.x * 0.5, 0.0, 1.0)) * k, 1.0);
}

// The ground a sloped face looks down on lies in front of it: the sunlit fraction of it
// lights the ground part of the face's ambient (`shade`). Three cascade taps along the face's
// horizontal normal `rn`, 6, 20 and 60 m out, weighted for the view factor of a face a few
// metres above them (the near ground fills most of its lower hemisphere). One tap at 8 m
// called the whole view dark for a wall over a shadowed trench floor with sunlit ice beyond
// it (Europa's trench walls at 0.3x the path tracer's indirect light). Flat ground does not
// see the ground (1).
fn ground_sun_ahead(base_xz: vec2<f32>, rn: vec3<f32>) -> f32 {
    var ground_sun = 1.0;
    let nh = vec2<f32>(rn.x, rn.z);
    if (dot(nh, nh) > 0.01) {
        let dir = normalize(nh);
        // Written out (no loop over a vector of reaches): a runtime index into a function-local
        // vector is spilled to memory (and loses the device on Mali, `shader_rules`).
        // Gentle slopes (the ground fills under a tenth of their view) keep one tap at 8 m:
        // the three cost the forward pass 0.05 ms on every sloped pixel.
        if (rn.y > 0.8) {
            ground_sun = ground_sun_tap(base_xz + dir * 8.0);
        } else {
            ground_sun = 0.45 * ground_sun_tap(base_xz + dir * 6.0)
                + 0.35 * ground_sun_tap(base_xz + dir * 20.0)
                + 0.2 * ground_sun_tap(base_xz + dir * 60.0);
        }
    }
    return ground_sun;
}

// The cascades' sun visibility 0.3 m above the terrain at `xz`.
fn ground_sun_tap(xz: vec2<f32>) -> f32 {
    return engine_csm_point(vec3<f32>(xz.x, height_w(xz) + 0.3, xz.y));
}

// The engine cascade's sun visibility at this pixel, times the screen-space contact shadow
// when the frame has one (`engine_csm.screen.w`) and the cloud layer's transmittance along the
// sun when the frame has a cloud map (`cloud_sun`: `shared_cloud_sun`, ADR-320).
fn shared_sun_shadow(in: VOut, rn: vec3<f32>, cloud_sun: f32) -> f32 {
    var sun_shadow = sample_engine_csm(in.world, rn);
    if (engine_csm.screen.w > 0.5) {
        let cs_dims = vec2<f32>(textureDimensions(engine_contact));
        let target_px = max(engine_csm.screen.xy, vec2<f32>(1.0));
        let texel = clamp(vec2<i32>(in.position.xy * cs_dims / target_px), vec2<i32>(0), vec2<i32>(cs_dims) - vec2<i32>(1));
        sun_shadow *= textureLoad(engine_contact, texel, 0).r;
    }
    // @cloud-map-begin
    if (engine_csm.cloud.w > 0.5) {
        sun_shadow *= cloud_sun;
    }
    // @cloud-map-end
    return sun_shadow;
}

// The cloud layer's transmittance along the sun at this pixel when the frame has a cloud map
// (`engine_csm.cloud.w`, ADR-320); 1, and no load of the map, without one. Every read of the
// cloud map in the shared colour pass sits between `@cloud-map` markers: a terrain drawn
// without a cloud map compiles them out (`without_cloud_map`, ADR-7886; compiled in but never
// taken, they and the footprint loop cost an RTX 5090 terrain forward time). Read once per pixel:
// the sun term takes it (`shared_sun_shadow`), and so does the sunlit share of the ground a
// sloped face looks down on (`shared_ground_sun`).
fn shared_cloud_sun(in: VOut) -> f32 {
    // @cloud-map-begin
    if (engine_csm.cloud.w > 0.5) {
        return cloud_sun_transmittance(engine_cloud, engine_csm.cloud, in.world - engine_csm.origin.xyz, normalize(frame.sun_dir_wetness.xyz), frame.camera_pos_time.xyz - engine_csm.origin.xyz);
    }
    // @cloud-map-end
    return 1.0;
}

// `ground_sun_ahead` under the engine's cloud layer (ADR-5944 item 10): the ground a sloped face
// looks down on, 6 to 60 m before it, is under the layer with the face, and the cascades do not
// hold the layer. Its sunlit share falls with the cloud map at the face (the map is smooth
// over those metres). Without a cloud map `ground_sun_ahead` as it was.
fn shared_ground_sun(base_xz: vec2<f32>, rn: vec3<f32>, cloud_sun: f32) -> f32 {
    let ground_sun = ground_sun_ahead(base_xz, rn);
    // @cloud-map-begin
    if (engine_csm.cloud.w > 0.5) {
        return ground_sun * cloud_sun;
    }
    // @cloud-map-end
    return ground_sun;
}

// The share of its sun the aerial haze, or the precipitation's veil, between the eye and `world`
// keeps under the engine's cloud layer (ADR-5944): the mean of the cloud transmittance map along
// the view ray. 1 (and no load of the map) without a map, or with neither the aerial haze (the
// fog's sky in-scatter mode) nor precipitation.
fn shared_haze_sun(world: vec3<f32>) -> f32 {
    // @cloud-map-begin
    var wanted = frame.fog_params.w >= 0.5 && frame.fog_params.x > 0.0 && frame.fog_color.a >= 0.5;
    // @surface-weather-begin
    wanted = wanted || material.weather2.z > 0.0;
    // @surface-weather-end
    if (engine_csm.cloud.w < 0.5 || !wanted) {
        return 1.0;
    }
    return cloud_sun_on_ray(engine_cloud, engine_csm.cloud, frame.camera_pos_time.xyz - engine_csm.origin.xyz, world - engine_csm.origin.xyz, normalize(frame.sun_dir_wetness.xyz));
    // @cloud-map-end
    // @cloud-map-off return 1.0;
}

// @surface-weather-begin
// ---- The surface rule: the text of `regia_render::surface_weather_wgsl()` ----
// (kept identical; `terrain_surface_weather_matches_the_engine` checks it)

// The surface response to the weather (spec 99 § 6.3): the same rule as
// `apply_surface_weather` in `surface_weather.rs`, run after the surface stage and before
// lighting, behind a uniform branch at the call site. No derivatives, fixed loops.

struct WeatherSurface {
    albedo: vec3<f32>,
    roughness: f32,
    metallic: f32,
    normal: vec3<f32>,
}

fn weather_hash_step(state: u32) -> u32 {
    let word = state * 747796405u + 2891336453u;
    let mixed = ((word >> ((word >> 28u) + 4u)) ^ word) * 277803737u;
    return (mixed >> 22u) ^ mixed;
}

fn weather_cell_hash(cell: vec2<i32>, salt: u32) -> vec3<f32> {
    let a = weather_hash_step(bitcast<u32>(cell.x) ^ weather_hash_step(bitcast<u32>(cell.y) ^ salt));
    let b = weather_hash_step(a);
    let c = weather_hash_step(b);
    return vec3<f32>(f32(a >> 8u), f32(b >> 8u), f32(c >> 8u)) / 16777216.0;
}

fn weather_value_noise(p: vec2<f32>) -> f32 {
    let cell = floor(p);
    let f = p - cell;
    let u = f * f * (3.0 - 2.0 * f);
    let i = vec2<i32>(cell);
    let a = weather_cell_hash(i, 0u).x;
    let b = weather_cell_hash(i + vec2<i32>(1, 0), 0u).x;
    let c = weather_cell_hash(i + vec2<i32>(0, 1), 0u).x;
    let d = weather_cell_hash(i + vec2<i32>(1, 1), 0u).x;
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn weather_puddle_key(xz: vec2<f32>) -> f32 {
    return smoothstep(0.2, 0.8, 0.35 * weather_value_noise(xz / 1.5) + 0.65 * weather_value_noise(xz / 6.0));
}

const WEATHER_RIPPLE_CELL_M: f32 = 0.4;
const WEATHER_RIPPLE_WAVELENGTH_M: f32 = 0.06;
const WEATHER_RIPPLE_HEIGHT_M: f32 = 0.0012;
const WEATHER_RIPPLE_TAU: f32 = 6.2831853;

fn weather_ripple_gradient(xz: vec2<f32>, clock_s: f32, rate: f32) -> vec2<f32> {
    var gradient = vec2<f32>(0.0);
    for (var grid = 0u; grid < 2u; grid += 1u) {
        let offset = f32(grid) * 0.37;
        let p = xz / WEATHER_RIPPLE_CELL_M + vec2<f32>(offset, offset * 1.7);
        let base = vec2<i32>(floor(p - 0.5));
        for (var j = 0; j < 2; j += 1) {
            for (var i = 0; i < 2; i += 1) {
                let cell = base + vec2<i32>(i, j);
                let first = weather_cell_hash(cell, grid + 1u);
                let period = 0.5 + 0.4 * first.x;
                let t = clock_s / period + first.y;
                let cycle = floor(t);
                let age = t - cycle;
                let second = weather_cell_hash(cell, bitcast<u32>(i32(cycle)) * 31u + grid);
                if (second.z >= rate) {
                    continue;
                }
                let center = vec2<f32>(cell) + 0.25 + 0.5 * second.xy;
                let delta = (p - center) * WEATHER_RIPPLE_CELL_M;
                let distance = length(delta);
                if (distance < 1.0e-5) {
                    continue;
                }
                let x = distance - 0.15 * age;
                let window = 1.0 - smoothstep(0.0, 0.05, abs(x));
                let fade = (1.0 - age) * (1.0 - age);
                let phase = WEATHER_RIPPLE_TAU * x / WEATHER_RIPPLE_WAVELENGTH_M;
                let slope = WEATHER_RIPPLE_HEIGHT_M * WEATHER_RIPPLE_TAU / WEATHER_RIPPLE_WAVELENGTH_M * cos(phase) * fade * window;
                gradient += slope * delta / distance;
            }
        }
    }
    return gradient;
}

// The default porosity: rough dielectrics soak, polished and metal surfaces do not.
fn weather_porosity(roughness: f32, metallic: f32) -> f32 {
    return (1.0 - clamp(metallic, 0.0, 1.0)) * clamp((roughness - 0.25) / 0.5, 0.0, 1.0);
}

// `weather` = (wetness, puddles, snow cover, ripple). `response` = (porosity, wet, snow, puddles 0/1).
// `exposure` = 1 under open sky, 0 under cover. `extra_key` raises the puddle key where the ground
// is concave (terrain); 0 elsewhere.
fn weather_apply(
    surface: WeatherSurface,
    n_geo: vec3<f32>,
    world: vec3<f32>,
    weather: vec4<f32>,
    clock_s: f32,
    response: vec4<f32>,
    exposure: f32,
    extra_key: f32,
) -> WeatherSurface {
    let x = clamp(exposure, 0.0, 1.0);
    if (x <= 0.0) {
        return surface;
    }
    // Plain locals, not a struct: a function-local struct is an explicitly laid out variable,
    // which SPIR-V validation refuses.
    let phi = clamp(response.x, 0.0, 1.0);
    let wet = clamp(weather.x * x * response.y, 0.0, 1.0);
    var albedo = surface.albedo * (1.0 - 0.5 * phi * wet);
    let film = wet * (1.0 - phi) + 0.6 * phi * smoothstep(0.7, 1.0, wet);
    var roughness = mix(surface.roughness, 0.08, 0.9 * film);
    var metallic = surface.metallic;
    var normal = normalize(mix(surface.normal, n_geo, 0.5 * film));
    let cover = clamp(weather.z * x * response.z, 0.0, 1.0);
    let lumps = weather_value_noise(world.xz / 0.7) - 0.5;
    let snow = cover * smoothstep(0.5, 0.85, n_geo.y + 0.15 * lumps);
    if (response.w > 0.5 && n_geo.y > 0.97) {
        let threshold = 1.0 - 0.6 * clamp(weather.y * x, 0.0, 1.0);
        let key = clamp(weather_puddle_key(world.xz) + extra_key, 0.0, 1.0);
        let pool = smoothstep(threshold, threshold + 0.05, key) * (1.0 - snow);
        if (pool > 0.0) {
            let gradient = weather_ripple_gradient(world.xz, clock_s, weather.w);
            let up = normalize(vec3<f32>(-gradient.x, 1.0, -gradient.y));
            albedo = mix(albedo, surface.albedo * (1.0 - 0.5 * phi), pool);
            roughness = mix(roughness, 0.02, pool);
            metallic = mix(metallic, 0.0, pool);
            normal = normalize(mix(normal, up, pool));
        }
    }
    if (snow > 0.0) {
        let bent = normalize(n_geo + vec3<f32>(0.0, 0.5, 0.0));
        normal = normalize(mix(mix(normal, n_geo, snow), bent, 0.5 * snow));
        albedo = mix(albedo, vec3<f32>(0.82), snow);
        roughness = mix(roughness, 0.8, snow);
        metallic = mix(metallic, 0.0, snow);
    }
    return WeatherSurface(albedo, roughness, metallic, normal);
}

// The rain's or snow's constant extinction over the view distance, in-scattering `inscatter`.
fn weather_extinction(scene_rgb: vec3<f32>, distance: f32, extinction_per_m: f32, inscatter: vec3<f32>) -> vec3<f32> {
    return mix(inscatter, scene_rgb, exp(-extinction_per_m * max(distance, 0.0)));
}

// How open the ground under the pixel is to the sky (`fs_shared` sets it): the occlusion tile
// through a 3×3 comparison, the mirror of `precip_exposure` in `lab_forward.wgsl`.
var<private> weather_exposure: f32 = 1.0;

fn terrain_weather_exposure(world: vec3<f32>, n_geo: vec3<f32>) -> f32 {
    if (material.weather2.y < 0.5) {
        return 1.0;
    }
    let offset = world + n_geo * material.weather_bias.x * 1.5;
    let clip = material.weather_tile * vec4<f32>(offset, 1.0);
    let ndc = clip.xyz / clip.w;
    let local = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0) {
        return 1.0;
    }
    let texel = 1.0 / max(material.weather2.w, 1.0);
    let uv = local * material.weather_rect.xy + material.weather_rect.zw;
    // Past the view's far plane it compares as the far plane: open, unless covered.
    let compare = min(ndc.z - 0.02 * material.weather_bias.y, 1.0);
    let tile_min = material.weather_rect.zw;
    let tile_max = tile_min + material.weather_rect.xy;
    var open = 0.0;
    for (var y = -1; y <= 1; y += 1) {
        for (var x = -1; x <= 1; x += 1) {
            let tap = uv + vec2<f32>(f32(x), f32(y)) * texel;
            // A tap off the tile would read a neighbor: open sky, as the lookup past the map.
            if (any(tap < tile_min) || any(tap > tile_max)) {
                open += 1.0;
            } else {
                open += textureSampleCompareLevel(engine_precip, shadow_samp, tap, compare);
            }
        }
    }
    return open / 9.0;
}

// A layer's porosity from its ORM map's blue channel (`.terrain.layers.ron` v5
// `porosity_from_orm`, ADR-3367): the colour pass's own top-plane ORM tap (`surface_layers`:
// the same LOD, stochastic cells and gradients), so the porosity lines up with the albedo.
fn terrain_orm_porosity(xz: vec2<f32>, footprint: f32, tiling: f32, texel_m: f32, layer: i32) -> f32 {
    let lod = footprint_lod(footprint, texel_m);
    let st = stochastic_taps(xz, layer_cell(material.params0.z, tiling));
    return clamp(tri_sample(layer_orm, st, tiling, layer, lod, aniso_grads_top(lod)).b, 0.0, 1.0);
}

// The layers' weather blocks (`.terrain.layers.ron` v4, spec 99 § 7.3) as the `weather_apply`
// response (porosity, 1, snow scale, puddles 0/1). Slot 0's `params3.w` holds the stack's flags:
// +1 a layer overrides the default response, +2 the stack holds no puddles. Without an override
// this is the default response, exactly as before v4: the porosity from the blended roughness,
// full snow. With one, each layer's porosity (its own, or the derived one where it names none)
// and snow scale blend by its band weights, times the splat packs' on the photoreal path. The
// height blend and the mottle are not repeated: they move a transition by a few centimetres,
// where the two layers' responses mix anyway (`LayerWeather`).
fn terrain_weather_response(world: vec3<f32>, slope: f32, roughness: f32, footprint: f32, photoreal: bool) -> vec4<f32> {
    let flags = u32(clamp(layers.slots[0].params3.w, 0.0, 3.0) + 0.5);
    let derived = weather_porosity(roughness, 0.0);
    let puddles = select(1.0, 0.0, (flags & 2u) != 0u);
    if ((flags & 1u) == 0u) {
        return vec4<f32>(derived, 1.0, 1.0, puddles);
    }
    let count = i32(clamp(layers.count.x, 1.0, 8.0));
    // Plain locals, not a `SplatPacks` var (no function-local struct variables, ADR-026).
    var pack_a = vec4<f32>(1.0);
    var pack_b = vec4<f32>(1.0);
    if (photoreal) {
        let packs = splat_packs(world.xz, footprint);
        pack_a = packs.a;
        pack_b = packs.b;
    }
    var total = 0.0;
    // Σ w over the layers that name a porosity, Σ w · that porosity, Σ w · (1 − snow scale).
    var named = 0.0;
    var named_porosity = 0.0;
    var snow_off = 0.0;
    for (var i = 0; i < count; i++) {
        let slot = layers.slots[i];
        var w = 0.0;
        if (photoreal) {
            let ws = slope_band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
            let wl = altitude_band(world.y, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
            w = max(ws * wl, 0.0) * pick_weight(pack_a, pack_b, i);
        } else {
            let ws = band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
            let wa = band(world.y, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
            w = max(ws * wa, 0.0);
        }
        var has = select(0.0, 1.0, slot.params3.y > 0.5);
        var porosity_i = clamp(slot.params3.y - 1.0, 0.0, 1.0);
        // −1: this layer's ORM blue is its porosity (v5), where the layer has an ORM map.
        if (photoreal && slot.params3.y < -0.5 && slot.params3.x > 0.5 && w > 0.0) {
            has = 1.0;
            porosity_i = terrain_orm_porosity(world.xz, footprint, slot.params.x, slot.params.w, i);
        }
        total += w;
        named += w * has;
        named_porosity += w * has * porosity_i;
        snow_off += w * clamp(slot.params3.z, 0.0, 1.0);
    }
    if (total <= 1e-5) {
        // No band covers this fragment: the base layer, as the colour falls back to it.
        let base = layers.slots[0];
        var has = select(0.0, 1.0, base.params3.y > 0.5);
        var porosity_0 = clamp(base.params3.y - 1.0, 0.0, 1.0);
        if (photoreal && base.params3.y < -0.5 && base.params3.x > 0.5) {
            has = 1.0;
            porosity_0 = terrain_orm_porosity(world.xz, footprint, base.params.x, base.params.w, 0);
        }
        total = 1.0;
        named = has;
        named_porosity = has * porosity_0;
        snow_off = clamp(base.params3.z, 0.0, 1.0);
    }
    let porosity = named_porosity / total + (1.0 - named / total) * derived;
    return vec4<f32>(clamp(porosity, 0.0, 1.0), 1.0, clamp(1.0 - snow_off / total, 0.0, 1.0), puddles);
}

// How much a hollow raises the puddle key, 0..0.5: the height under the mean of four taps at 2 m
// and at 8 m, a 5 cm hollow over 2 m counting as fully concave. Ruts and dips pool first.
fn terrain_weather_hollow(xz: vec2<f32>) -> f32 {
    let h = height_w(xz);
    let near = 0.25 * (height_w(xz + vec2<f32>(2.0, 0.0)) + height_w(xz - vec2<f32>(2.0, 0.0))
        + height_w(xz + vec2<f32>(0.0, 2.0)) + height_w(xz - vec2<f32>(0.0, 2.0)));
    let far = 0.25 * (height_w(xz + vec2<f32>(8.0, 0.0)) + height_w(xz - vec2<f32>(8.0, 0.0))
        + height_w(xz + vec2<f32>(0.0, 8.0)) + height_w(xz - vec2<f32>(0.0, 8.0)));
    return 0.5 * clamp(max(near - h, 0.25 * (far - h)) / 0.05, 0.0, 1.0);
}
// @surface-weather-end

// ---- Cloud transmittance map lookup: the text of `regia_render::cloud_shadow_wgsl()` ----
// (kept identical; `terrain_cloud_lookup_matches_the_engine` checks it)

// Reads the cloud transmittance map (`clouds.wgsl` `fs_cloud_shadow_map`, ADR-320) for a point
// lit by the sun: the forward sun term, the terrain's (`regia_terrain` splices the text after
// the first line, `cloud_shadow_wgsl`) and the froxel fog's in-scatter. Mirrors
// `clouds::cloud_shadow_lookup`.
//
// `params` = (map extent m, layer base y, layer top y, 1 when the map is on). `to_sun` is unit
// length, toward the sun; `eye` is the camera. The map is indexed by where the sun line through
// `p` crosses the layer base (toroidal, bilinear by hand so any texture binding works). A point
// inside the layer keeps the part of the column above it (`T^fraction`, a uniform-column
// approximation); one above the layer is lit. The map fades to lit over the outer 15 % of its
// window around the camera (Hillaire 2016; Bauer 2019).

fn cloud_shadow_texel(shadow_tex: texture_2d<f32>, texel: vec2<i32>, dims: vec2<i32>) -> f32 {
    let wrapped = (texel % dims + dims) % dims;
    return textureLoad(shadow_tex, wrapped, 0).r;
}

fn cloud_sun_transmittance(
    shadow_tex: texture_2d<f32>,
    params: vec4<f32>,
    p: vec3<f32>,
    to_sun: vec3<f32>,
    eye: vec3<f32>,
) -> f32 {
    // 0.01: `CLOUD_SHADOW_MIN_SUN_Y`. The switch is its readers' own test negated (`!(w > 0.5)`,
    // not `w < 0.5`): the forward pass, the fog and the terrain call this under `if (w > 0.5)`,
    // and FXC's optimizer does not come back from a strict comparison tested the other way round
    // under itself when a second value is held under the same constant there (the forward
    // pass's `unlit < 0.5`): it overflows any stack (ADR-5971,
    // `shader_hlsl::fxc_assertion_problems`).
    if (!(params.w > 0.5) || to_sun.y < 0.01 || p.y >= params.z) {
        return 1.0;
    }
    let base = params.y;
    let top = params.z;
    let extent = max(params.x, 1.0);
    let xz = p.xz + to_sun.xz * ((base - p.y) / to_sun.y);
    let center = eye.xz + to_sun.xz * ((base - min(eye.y, base)) / to_sun.y);
    let off = abs(xz - center);
    let d = max(off.x, off.y) / (0.5 * extent);
    if (d >= 1.0) {
        return 1.0;
    }
    let dims = max(vec2<i32>(textureDimensions(shadow_tex)), vec2<i32>(1));
    let f = xz / extent * vec2<f32>(dims) - vec2<f32>(0.5);
    let i0 = vec2<i32>(floor(f));
    let t = f - floor(f);
    let a = cloud_shadow_texel(shadow_tex, i0, dims);
    let b = cloud_shadow_texel(shadow_tex, i0 + vec2<i32>(1, 0), dims);
    let c = cloud_shadow_texel(shadow_tex, i0 + vec2<i32>(0, 1), dims);
    let e = cloud_shadow_texel(shadow_tex, i0 + vec2<i32>(1, 1), dims);
    var trans = mix(mix(a, b, t.x), mix(c, e, t.x), t.y);
    if (p.y > base) {
        let frac = clamp((top - p.y) / max(top - base, 1.0), 0.0, 1.0);
        trans = pow(max(trans, 1e-6), frac);
    }
    // 0.85: `CLOUD_SHADOW_EDGE_FADE`.
    let edge = smoothstep(0.85, 1.0, d);
    return mix(trans, 1.0, edge);
}

// One axis of the map window's cut of a view ray (`cloud_air_span`): the ray's offset from the
// window's centre on that axis is `off0 + slope t`, and the window is |offset| < `half`. Returns
// the cut interval (x, y) and 1 in z, or z = 0 when the ray never enters on this axis.
fn cloud_air_slab(span: vec2<f32>, off0: f32, slope: f32, half: f32) -> vec3<f32> {
    if (abs(slope) < 1e-9) {
        return vec3<f32>(span, select(1.0, 0.0, abs(off0) >= half));
    }
    let ta = (-half - off0) / slope;
    let tb = (half - off0) / slope;
    return vec3<f32>(max(span.x, min(ta, tb)), min(span.y, max(ta, tb)), 1.0);
}

// The stretch of the view ray `eye + view t`, t in [0, dist], the layer can shadow (ADR-5944):
// x, y = its ends in metres from the eye (x >= y: none), z = the share of the ray's length below
// the layer's top (the window does not count). The ray is cut to the half space below the top
// and to the map's window; a point's place in the window is linear in t, so the window cuts the
// ray to one interval (a slab test per axis, Kay & Kajiya 1986). Outside it
// `cloud_sun_transmittance` is 1. Mirrors `clouds::cloud_air_span`.
fn cloud_air_span(params: vec4<f32>, eye: vec3<f32>, view: vec3<f32>, dist: f32, to_sun: vec3<f32>) -> vec3<f32> {
    // 0.01: `CLOUD_SHADOW_MIN_SUN_Y`. `!(dist > 0)` also catches a NaN distance.
    if (params.w < 0.5 || to_sun.y < 0.01 || !(dist > 0.0)) {
        return vec3<f32>(0.0);
    }
    let base = params.y;
    let top = params.z;
    var t0 = 0.0;
    var t1 = dist;
    if (abs(view.y) < 1e-6) {
        if (eye.y >= top) {
            return vec3<f32>(0.0);
        }
    } else {
        let t_top = (top - eye.y) / view.y;
        if (view.y > 0.0) {
            t1 = min(t1, t_top);
        } else {
            t0 = max(t0, t_top);
        }
    }
    if (t1 <= t0) {
        return vec3<f32>(0.0);
    }
    let under = clamp((t1 - t0) / dist, 0.0, 1.0);
    let half = 0.5 * max(params.x, 1.0);
    let lift = ((base - eye.y) - (base - min(eye.y, base))) / to_sun.y;
    let rise = view.y / to_sun.y;
    let cut_x = cloud_air_slab(vec2<f32>(t0, t1), to_sun.x * lift, view.x - to_sun.x * rise, half);
    let cut_z = cloud_air_slab(cut_x.xy, to_sun.z * lift, view.z - to_sun.z * rise, half);
    if (cut_x.z < 0.5 || cut_z.z < 0.5 || cut_z.y <= cut_z.x) {
        return vec3<f32>(0.0, 0.0, under);
    }
    return vec3<f32>(cut_z.xy, under);
}

// Mean sun transmittance of the air on the segment from `eye` to `world`: the share of the sun
// the analytic haze on that view ray keeps under the layer (ADR-5944). Four midpoints
// (`HAZE_CLOUD_STEPS`); 1 with the map off. Mirrors `clouds::cloud_shadow_on_ray`.
fn cloud_sun_on_ray(
    shadow_tex: texture_2d<f32>,
    params: vec4<f32>,
    eye: vec3<f32>,
    world: vec3<f32>,
    to_sun: vec3<f32>,
) -> f32 {
    if (params.w < 0.5) {
        return 1.0;
    }
    var sum = 0.0;
    for (var i = 0; i < 4; i++) {
        let p = mix(eye, world, (f32(i) + 0.5) * 0.25);
        sum += cloud_sun_transmittance(shadow_tex, params, p, to_sun, eye);
    }
    return sum * 0.25;
}
// ---- end of the cloud transmittance map lookup ----

// The shared colour pass without the virtual texture. The body is written out here and in
// `fs_shared_vt` instead of both calling one helper: an entry point that only called a shared
// colour helper cost the terrain forward pass 0.09–0.1 ms on the RTX 5090 (bore_twelve,
// desktop-high), and the hole cut is the `hole_cut` constant for the same reason.
//
// Shade, then cut: Mesa 26.1's lavapipe (LLVM 19) reads a null pointer running the hole-cut
// entry with the large-mesh proxies after an early `discard`. The output is the same.
@fragment
fn fs_shared(in: VOut) -> @location(0) vec4<f32> {
    let ssao = shared_ssao(in);
    var c: vec4<f32>;
    if (shared_debug_on()) {
        c = shared_debug(in);
    } else {
        let rn = receiver_normal(in);
        let cloud_sun = shared_cloud_sun(in);
        let ground_sun = shared_ground_sun(in.base.xz, rn, cloud_sun);
        // @surface-weather-begin
        weather_exposure = terrain_weather_exposure(in.base, rn);
        // @surface-weather-end
        haze_sun_lit = shared_haze_sun(in.world);
        c = shade(in, shared_sun_shadow(in, rn, cloud_sun), ssao, ground_sun, no_surf(), false, true);
    }
    if (hole_cut && in_hole(in.base.xz, in.ref_xz)) {
        discard;
    }
    return c;
}

// The shared colour pass with the terrain virtual texture (`vt_lookup`): the surface for
// this pixel from the texture when it hit. Forced early depth tests: the feedback writes
// would otherwise move the depth test after the shader and run the full surface for every
// occluded fragment the prepass already rejects. Hole-cut nodes draw with `fs_shared`
// compiled with `hole_cut` (a discard must not leave early-written depth behind).
@fragment @early_depth_test(force)
fn fs_shared_vt(in: VOut) -> @location(0) vec4<f32> {
    let hit = vt_lookup(in);
    if (vt_geo_debug()) {
        return vt_geo_error(in);
    }
    let ssao = shared_ssao(in);
    if (shared_debug_on()) {
        return shared_debug(in);
    }
    let rn = receiver_normal(in);
    let cloud_sun = shared_cloud_sun(in);
    let ground_sun = shared_ground_sun(in.base.xz, rn, cloud_sun);
    // @surface-weather-begin
    weather_exposure = terrain_weather_exposure(in.base, rn);
    // @surface-weather-end
    haze_sun_lit = shared_haze_sun(in.world);
    return shade(in, shared_sun_shadow(in, rn, cloud_sun), ssao, ground_sun, hit.surf, hit.hit, true);
}

// Decal receiver: layer 0 and the same footprint normal the colour pass shades with.
// R is layer/255 (terrain is 0). GBA is the world normal as `n * 0.5 + 0.5`.
@fragment
fn fs_receiver(in: VOut) -> @location(0) vec4<f32> {
    if (hole_cut && in_hole(in.base.xz, in.ref_xz)) {
        discard;
    }
    // Same footprint and geometry mip as the colour pass (see `shade`), so the
    // receiver normal is the one the decals' receiver test compares against.
    let footprint = pixel_footprint(in.world);
    let n = fragment_normal(in.base.xz, footprint, in.geo_mip);
    let enc = n * 0.5 + vec3<f32>(0.5);
    return vec4<f32>(0.0, enc);
}

