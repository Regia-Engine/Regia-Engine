// SPDX-License-Identifier: MPL-2.0
// The lite forward fragment shader of the Mobile tiers on tile GPUs (ADR-1874,
// `crate::forward_lite`). `lab_forward.wgsl` stays the one desktop shader: its register pressure,
// not any single feature, is what a phone pays for (0.15-0.39 us per pixel on a Mali-G78 and an
// Adreno 643, ADR-1749), so this file is a separate small shader rather than more `#ifdef`s there.
//
// It draws opaque and masked standard materials on the CPU draw path, with the lab vertex stage
// and the same pipeline layout (a subset of its bindings). Blended, transmissive, graph and
// mask-decal draws keep their own pipelines. With `LIGHTMAP` it draws lightmap receivers
// (ADR-2107): the lab `LIGHTMAP` vertex stage, one bilinear fetch at Low, one more for the direction
// at High.
//
// What a frame here gives up against `lab_forward.wgsl`: skin, eye, hair, sheen, clearcoat and
// anisotropy shade as the standard surface; no detail maps, wear or parallax; no specular AA; no
// multi-scatter IBL, and the occlusion map as it is on the sky light (a lightmap receiver's baked
// light takes the lab shader's multi-scatter share and multi-bounce occlusion, ADR-5945); no SSAO,
// contact shadows, probes, baked GI volumes, mesh occluders, terrain sky occlusion, ground bounce
// or translucent shadows; the local light loop stops at `MAX_LIGHTS`.
//
// `FORWARD_LITE_LOW`: one sun cascade (Mobile Low plans one), hardware 2x2 PCF, 4 local lights, no
// local shadows. `FORWARD_LITE_HIGH`: two sun cascades (Mobile High plans two; no cross-fade), four
// bilinear taps, 8 local lights, one hardware 2x2 tap for a local shadow. Exactly one of the two is
// defined. Either tier reads every cascade a frame planned (a tier forced onto another profile,
// ADR-2403), one tap set per pixel.

// The texture table variant (`TEXTURE_TABLE`, ADR-863) declares a `binding_array`, which naga
// accepts only with this extension enabled, and naga_oil reads `enable` only at the top of a file:
// on for every variant, as in `lab_forward.wgsl`. Without it a lite tier on a table device failed to
// compose (ADR-7254).
enable wgpu_binding_array;

#import regia::utils
#import regia::pbr::lighting
#import regia::clustering
#import regia::shadows
#import regia::fog
#import regia::aerial
#import regia::forward::util::{cube_face, override_emissive, override_tint}
#ifdef LITE_SURFACE_WEATHER
#import regia::forward::surface_weather
#endif
#ifdef LIGHTMAP
#import regia::ddgi
#import regia::forward::occlusion
#endif

#ifdef FORWARD_LITE_LOW
const MAX_LIGHTS: u32 = 4u;
#endif
#ifdef FORWARD_LITE_HIGH
const MAX_LIGHTS: u32 = 8u;
#endif

// The interface and uniform blocks below are `lab_forward.wgsl`'s, field for field (the unit test
// `forward_lite::tests::the_lite_blocks_are_the_lab_blocks` compares them).

struct LabFrame {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    camera_pos: vec4<f32>,
    params: vec4<f32>,
    screen: vec4<f32>,
    extra: vec4<f32>,
    post_exposure: vec4<f32>,
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    quality: vec4<f32>,
    prev_view_proj: mat4x4<f32>,
    unjittered_view_proj: mat4x4<f32>,
    pcss: vec4<f32>,
    interior_center: vec4<f32>,
    interior_half: vec4<f32>,
    fill_dir: vec4<f32>,
    fill_color: vec4<f32>,
    specular_aa: vec4<f32>,
    cloud_sun: vec4<f32>,
    env_ground: vec4<f32>,
    env_ground_light: vec4<f32>,
    env_view: vec4<f32>,
    background: array<vec4<f32>, 3>,
    capture_center: array<vec4<f32>, 4>,
    capture_half: array<vec4<f32>, 4>,
    // Surface weather (spec 99 § 6.5): wetness, puddles, snow cover, ripple. Read only by the
    // `LITE_SURFACE_WEATHER` variant, which a dry frame never composes.
    weather: vec4<f32>,
    // The weather clock, the occlusion tile (-1 none), its extent, the precipitation's extinction.
    weather2: vec4<f32>,
    // Same trailing clock as `lab_forward.wgsl`. Mobile standard materials do not read it.
    clock: vec4<f32>,
    // Lights-and-views block (ADR-7211, `LightsViewsUniform`): x = the debug view
    // (`lights_views::shading_view_code`, 0 lit), y = extra directional lights, z = ambient volumes.
    view_mode: vec4<f32>,
    // Extra directional lights (up to 3): travel direction (w = 1), then color · lux.
    extra_dir: array<vec4<f32>, 6>,
    // Ambient volumes (up to 4): centre (w = blend distance), half extents, irradiance (lux).
    ambient_box: array<vec4<f32>, 12>,
    // Ambient volumes: the rotation from the world into each box's frame (quaternion xyzw, ADR-7275).
    ambient_rot: array<vec4<f32>, 4>,
}

struct GpuLight {
    position_kind: vec4<f32>,
    color_intensity: vec4<f32>,
    direction_radius: vec4<f32>,
    cone_shadow: vec4<f32>,
}

struct DirectionalLight {
    direction: vec3<f32>,
    _pad0: u32,
    color: vec3<f32>,
    illuminance: f32,
}

struct LabMaterial {
    base_color: vec4<f32>,
    factors: vec4<f32>,
    emissive: vec4<f32>,
    alpha: vec4<f32>,
    detail: vec4<f32>,
    detail_fade: vec4<f32>,
    coat: vec4<f32>,
    response: vec4<f32>,
    wear: vec4<f32>,
    wear_tint: vec4<f32>,
    pom: vec4<f32>,
    transmit: vec4<f32>,
    shading: vec4<f32>,
    shading_a: vec4<f32>,
    shading_b: vec4<f32>,
    shading_c: vec4<f32>,
    sheen: vec4<f32>,
    hair_a: vec4<f32>,
    hair_b: vec4<f32>,
    hair_c: vec4<f32>,
    hair_d: vec4<f32>,
    hair_e: vec4<f32>,
#ifdef TEXTURE_TABLE
    texture_indices: vec4<u32>,
#endif
#ifdef MATERIAL_GRAPH
    graph: array<vec4<f32>, 16>,
#endif
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) world_tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) @interpolate(flat) lod_dither: f32,
    @location(8) @interpolate(flat) overrides: vec2<u32>,
#ifdef LIGHTMAP
    @location(9) lightmap_uv: vec2<f32>,
    @location(10) @interpolate(flat) lightmap_scale_offset: vec4<f32>,
#endif
}

struct ShadowUniform {
    light_view_proj: mat4x4<f32>,
    split_distance: f32,
    bias: vec3<f32>,
}

struct ShadowUniforms {
    cascades: array<ShadowUniform, 4>,
    count: vec4<f32>,
}

@group(0) @binding(0) var<uniform> frame: LabFrame;
@group(0) @binding(1) var<uniform> dir_light: DirectionalLight;
@group(0) @binding(2) var<storage, read> lights: array<GpuLight>;
@group(0) @binding(3) var<storage, read> counts: array<u32>;
@group(0) @binding(4) var<storage, read> indices: array<u32>;
@group(0) @binding(5) var shadow_map_0: texture_depth_2d;
// The other sun cascades. The Mobile profiles plan as many as their tier reads (Low 1, High 2), so
// those frames never sample these; a lite tier forced onto a profile with more cascades
// (`FrameRenderer::set_forward_lite`, `REGIA_FORWARD_LITE`) reads every cascade it planned
// (ADR-2403): a cascade past the tier's count used to fall back to unshadowed sun.
@group(0) @binding(6) var shadow_map_1: texture_depth_2d;
@group(0) @binding(15) var shadow_map_2: texture_depth_2d;
@group(0) @binding(16) var shadow_map_3: texture_depth_2d;
@group(0) @binding(7) var shadow_samp: sampler_comparison;
@group(0) @binding(8) var<uniform> csm: ShadowUniforms;
@group(0) @binding(11) var irradiance_tex: texture_2d<f32>;
@group(0) @binding(12) var prefilter_tex: texture_2d<f32>;
@group(0) @binding(13) var brdf_lut: texture_2d<f32>;
@group(0) @binding(14) var ibl_samp: sampler;

#ifdef FORWARD_LITE_HIGH
struct ShadowTile {
    view_proj: mat4x4<f32>,
    uv: vec4<f32>,
    bias: vec4<f32>,
}

@group(0) @binding(9) var shadow_atlas: texture_depth_2d;
@group(0) @binding(10) var<storage, read> shadow_tiles: array<ShadowTile>;
#endif
// The emitter probe atlas (`lab_forward.wgsl` binding 33): only its IES profile bands at the bottom are read here
// (`LightProfile`, ADR-2592; on the lite tiers since ADR-3247). The frame writes them on every tier.
@group(0) @binding(33) var emitter_probes: texture_2d<f32>;

#ifdef MATERIAL_SSBO
@group(2) @binding(0) var<storage, read> material: LabMaterial;
#else
@group(2) @binding(0) var<uniform> material: LabMaterial;
#endif

#ifdef TEXTURE_TABLE
@group(4) @binding(0) var texture_table: binding_array<texture_2d<f32>, #{TEXTURE_TABLE_SIZE}>;
@group(4) @binding(1) var tex_samp: sampler;

fn table_index(slot: u32) -> u32 {
    let word = material.texture_indices[slot >> 1u];
    return (word >> ((slot & 1u) * 16u)) & 0xffffu;
}
// The shared sampler set (ADR-2071, `lab_forward.wgsl` declares the same).
@group(4) @binding(3) var tex_samp_clamp: sampler;
@group(4) @binding(4) var tex_samp_point: sampler;
fn table_sample(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let choice = (material.texture_indices.w >> (16u + slot * 2u)) & 3u;
    if (choice == 1u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_clamp, uv);
    }
    if (choice == 2u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_point, uv);
    }
    return textureSample(texture_table[table_index(slot)], tex_samp, uv);
}
#else
@group(2) @binding(1) var albedo_tex: texture_2d<f32>;
@group(2) @binding(2) var tex_samp: sampler;
@group(2) @binding(3) var normal_tex: texture_2d<f32>;
@group(2) @binding(4) var occlusion_tex: texture_2d<f32>;
@group(2) @binding(5) var emissive_tex: texture_2d<f32>;
#endif

#ifdef LIGHTMAP
// The lab shader's group 3 without the draw block, which only the vertex stage reads.
@group(3) @binding(12) var lightmap_atlas: texture_2d<f32>;
@group(3) @binding(13) var lightmap_sampler: sampler;
@group(3) @binding(14) var<uniform> lightmap_params: LightmapParams;
@group(3) @binding(15) var lightmap_direction: texture_2d<f32>;
// @lightmap_lookup_source
#endif

// A tangent-space normal from its map's channel layout (`alpha.z` bits 1-2), as the lab shader.
fn decode_normal_sample(s: vec4<f32>) -> vec3<f32> {
    let channels = (u32(material.alpha.z + 0.5) >> 1u) & 3u;
    if (channels == 0u) {
        return s.xyz * 2.0 - vec3<f32>(1.0);
    }
    var xy = s.xy * 2.0 - vec2<f32>(1.0);
    if (channels == 2u) {
        xy = vec2<f32>(s.x, s.w) * 2.0 - vec2<f32>(1.0);
    }
    return vec3<f32>(xy, sqrt(max(1.0 - dot(xy, xy), 0.0)));
}

#ifdef ALPHA_TEST
// LOD cross-fade: the lab shader's Bayer test (`lod_dither_discard`), one fixed pattern per frame
// index.
fn lod_dither_rejects(code: f32, frag: vec2<f32>) -> bool {
    if (code == 0.0) {
        return false;
    }
    let t = u32(max(frame.quality.w, 0.0) + 0.5) & 15u;
    let ty = ((t >> 2u) & 1u) | ((t & 1u) << 1u);
    let tx = (((t >> 3u) & 1u) | (((t >> 1u) & 1u) << 1u)) ^ ty;
    let p = (vec2<u32>(frag) + vec2<u32>(tx, ty)) & vec2<u32>(3u);
    let v = p.x ^ p.y;
    let bayer = ((v & 1u) << 3u) | ((p.y & 1u) << 2u) | (((v >> 1u) & 1u) << 1u) | ((p.y >> 1u) & 1u);
    let d = (f32(bayer) + 0.5) / 16.0;
    if (code > 0.0) {
        return d >= code;
    }
    return d < -code;
}
#endif

// Equirectangular UV without `atan2` and `acos`, which drivers expand into long instruction runs.
// acos: Lagarde's linear fit (Inverse trigonometric functions GPU optimization for AMD GCN
// architecture, 2014). atan: Rajan, Wang and Inkol 2006, on the smaller ratio. The worst errors are
// in the unit test `forward_lite::tests::the_fast_equirect_fits_stay_inside_their_error`; the
// irradiance and prefilter maps are blurred far past them.
fn fast_equirect_uv(d: vec3<f32>) -> vec2<f32> {
    let ax = abs(d.x);
    let az = abs(d.z);
    let ratio = min(ax, az) / max(max(ax, az), 1e-6);
    let quarter = ratio * (0.7853982 + 0.273 * (1.0 - ratio));
    var phi = select(quarter, 1.5707963 - quarter, az > ax);
    phi = select(phi, 3.1415927 - phi, d.x < 0.0);
    phi = select(phi, -phi, d.z < 0.0);
    let ay = min(abs(d.y), 1.0);
    let acos_y = (1.5707963 - 0.156583 * ay) * sqrt(1.0 - ay);
    let theta = select(acos_y, 3.1415927 - acos_y, d.y < 0.0);
    return vec2<f32>(phi * 0.15915494 + 0.5, theta * 0.31830989);
}

// The split-sum scale and bias without a table fetch: Lazarov 2013 (Getting More Physical in Call
// of Duty: Black Ops II), with Karis 2014's fit (Physically Based Shading on Mobile, public blog).
fn env_brdf_approx(perceptual: f32, no_v: f32) -> vec2<f32> {
    let r = perceptual * vec4<f32>(-1.0, -0.0275, -0.572, 0.022) + vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let a004 = min(r.x * r.x, exp2(-9.28 * no_v)) * r.x + r.y;
    return vec2<f32>(-1.04, 1.04) * a004 + r.zw;
}

// One light's diffuse plus specular, before colour and the cosine. GGX with Walter et al. 2007's D,
// correlated Smith visibility (Heitz 2014), Schlick Fresnel and the material's optional energy
// compensation, as the full shader (ADR-4930). DFG is fetched once per fragment, not per light.
// Under `BRDF_ENERGY_COMPENSATED` the caller's `diffuse_color` already carries the fragment's one
// energy partition (1 − E_s at n·v, ADR-2632), so the diffuse takes no second weight here (ADR-5622).
fn lite_brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32, dfg: vec4<f32>) -> vec3<f32> {
    let h = normalize(l + v);
    let nv = max(dot(n, v), 1e-5);
    let nl = max(dot(n, l), 1e-5);
    let f = regia::pbr::lighting::f_schlick(max(dot(l, h), 0.0), f0);
    let rough = perceptual * perceptual;
    let spec = f * regia::pbr::lighting::d_ggx(max(dot(n, h), 0.0), rough)
        * regia::pbr::lighting::v_smith_ggx_correlated(nv, nl, rough);
    let multi = material.response.y > 0.5;
#ifdef BRDF_ENERGY_COMPENSATED
    let diffuse_weight = vec3<f32>(1.0);
#else
    let es = clamp(f0 * dfg.x + vec3<f32>(dfg.y), vec3<f32>(0.0), vec3<f32>(1.0));
    let diffuse_weight = select(vec3<f32>(1.0) - f, vec3<f32>(1.0) - es, multi);
#endif
    let compensation = select(vec3<f32>(1.0), regia::pbr::lighting::energy_compensation(f0, dfg.z, dfg.w), multi);
    return diffuse_color * diffuse_weight / regia::pbr::lighting::PI + spec * compensation;
}

// Depth bias and normal offset (texels) of the sun cascades, with a slope term in place of the lab
// shader's receiver-plane kernel, which a single hardware tap cannot use. `SUN_MAX_TAN` is the lab
// shader's `CSM_MAX_TAN`: a lower cap banded the faces the sun grazes on Mobile Low's coarse cascade.
const SUN_DEPTH_BIAS_TEXELS: f32 = 1.5;
const SUN_NORMAL_OFFSET_TEXELS: f32 = 1.5;
const SUN_MAX_TAN: f32 = 12.0;

// One hardware 2x2 compare tap in cascade `index`'s map.
fn cascade_tap(index: u32, uv: vec2<f32>, z: f32) -> f32 {
    switch index {
        case 0u: {
            return textureSampleCompareLevel(shadow_map_0, shadow_samp, uv, z);
        }
        case 1u: {
            return textureSampleCompareLevel(shadow_map_1, shadow_samp, uv, z);
        }
        case 2u: {
            return textureSampleCompareLevel(shadow_map_2, shadow_samp, uv, z);
        }
        default: {
            return textureSampleCompareLevel(shadow_map_3, shadow_samp, uv, z);
        }
    }
}

// Sun visibility from cascade `index`.
fn cascade_visibility(index: u32, world: vec3<f32>, n: vec3<f32>, no_l: f32) -> f32 {
    let bias = csm.cascades[index].bias;
    let texel_world = max(bias.x, 1e-6);
    let sin_theta = sqrt(max(1.0 - no_l * no_l, 0.0));
    let offset = world + n * (SUN_NORMAL_OFFSET_TEXELS * texel_world * sin_theta);
    let uvz = regia::shadows::shadow_uv_from_clip(csm.cascades[index].light_view_proj * vec4<f32>(offset, 1.0));
    if (uvz.x < 0.0 || uvz.y < 0.0 || uvz.x > 1.0 || uvz.y > 1.0) {
        return 1.0;
    }
    let tan_theta = min(sin_theta / max(no_l, 0.05), SUN_MAX_TAN);
    let z = uvz.z - SUN_DEPTH_BIAS_TEXELS * (1.0 + tan_theta) * texel_world * max(bias.y, 1e-9);
#ifdef FORWARD_LITE_HIGH
    // Four bilinear compare taps, half a texel out: the 3x3 box the Pcf3x3 row asks for.
    let t = 0.5 / f32(textureDimensions(shadow_map_0, 0).x);
    let a = vec2<f32>(t, t);
    let b = vec2<f32>(-t, t);
    return 0.25 * (cascade_tap(index, uvz.xy + a, z)
        + cascade_tap(index, uvz.xy - a, z)
        + cascade_tap(index, uvz.xy + b, z)
        + cascade_tap(index, uvz.xy - b, z));
#endif
#ifdef FORWARD_LITE_LOW
    return cascade_tap(index, uvz.xy, z);
#endif
}

// The cascade whose slice holds the pixel, of the ones the frame planned (one on Mobile Low, two on
// Mobile High; up to four on a forced tier, ADR-2403). Only the last cascade fades out; each one
// before it hands over to the next at its split.
fn sun_visibility(world: vec3<f32>, n: vec3<f32>, no_l: f32, view_z: f32) -> f32 {
    let count = clamp(u32(csm.count.x + 0.5), 1u, 4u);
    var index = 0u;
    for (var i = 0u; i + 1u < count; i = i + 1u) {
        if (view_z > csm.cascades[i].split_distance) {
            index = i + 1u;
        }
    }
    if (index + 1u < count) {
        return cascade_visibility(index, world, n, no_l);
    }
    let hi = csm.cascades[index].split_distance;
    let fade = clamp((hi - view_z) / max(hi * 0.15, 1e-3), 0.0, 1.0);
    if (fade <= 0.0) {
        return 1.0;
    }
    return mix(1.0, cascade_visibility(index, world, n, no_l), fade);
}

#ifdef FORWARD_LITE_HIGH
// One hardware 2x2 compare tap in the local-light atlas (the lab shader's mode 0, without the debug
// views).
fn atlas_visibility(tile_index: u32, world: vec3<f32>, n: vec3<f32>, l: vec3<f32>) -> f32 {
    let tile = shadow_tiles[tile_index];
    let perspective = tile.bias.z > 0.5;
    let w_at = (tile.view_proj * vec4<f32>(world, 1.0)).w;
    let texel_world = tile.bias.x * select(1.0, max(w_at, 1.0e-6), perspective);
    let clip = tile.view_proj * vec4<f32>(world + n * texel_world * 1.5, 1.0);
    let local = regia::shadows::shadow_uv_from_clip(clip);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0 || clip.w <= 0.0) {
        return 1.0;
    }
    let dz_dw = select(tile.bias.y, 1.0 / max(clip.w * clip.w * tile.bias.y, 1.0e-9), perspective);
    let ndl = max(clamp(dot(n, l), 0.0, 1.0), 0.16);
    let tan_theta = min(sqrt(max(1.0 - ndl * ndl, 0.0)) / ndl, 8.0);
    let half_texel = vec2<f32>(0.5 / max(frame.extra.y, 1.0));
    let uv = clamp(local.xy * tile.uv.xy + tile.uv.zw, tile.uv.zw + half_texel, tile.uv.zw + tile.uv.xy - half_texel);
    return textureSampleCompareLevel(shadow_atlas, shadow_samp, uv, local.z - (2.0 + tan_theta) * texel_world * dz_dw);
}
#endif

// The clustered light loop, stopped at `MAX_LIGHTS`. Fitted emitters (kind 4) never reach a Mobile
// frame (`emissive_lights` is 0 there) and are skipped if a host turned them on.
fn local_lights(world: vec3<f32>, n: vec3<f32>, v: vec3<f32>, pix: vec2<f32>, view_z: f32, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32, dfg: vec4<f32>) -> vec3<f32> {
    let uv = pix / max(frame.screen.xy, vec2<f32>(1.0));
    let cx = min(u32(uv.x * 16.0), 15u);
    let cy = min(u32(uv.y * 9.0), 8u);
    let cz = regia::clustering::depth_to_slice(max(view_z, frame.params.x), frame.params.x, frame.params.y);
    let cid = regia::clustering::cluster_index(cx, cy, cz);
    let count = min(counts[cid], MAX_LIGHTS);
    let base_i = cid * u32(frame.screen.w);
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < count; i = i + 1u) {
        let li = indices[base_i + i];
        let kind = u32(lights[li].position_kind.w + 0.5);
        if (kind > 3u) {
            continue;
        }
        let lp = lights[li].position_kind.xyz;
        let to_light = lp - world;
        let dist = length(to_light);
        var att = regia::pbr::lighting::distance_attenuation_source(dist, lights[li].direction_radius.w, lights[li].cone_shadow.w);
        if (att <= 0.0) {
            continue;
        }
        let ldir = to_light / max(dist, 1e-4);
        if (kind == 2u) {
            att *= regia::pbr::lighting::spot_attenuation(ldir, lights[li].direction_radius.xyz, lights[li].cone_shadow.x, lights[li].cone_shadow.y);
        }
        // An IES profile (ADR-2592, ADR-3247): a fractional kind names its atlas band, `direction_radius.xyz` is
        // the nadir. Whole kinds (every light without one) skip it, so those frames do not move.
        if (att > 0.0 && fract(lights[li].position_kind.w) > 0.0) {
            att *= regia::pbr::lighting::light_profile_relative(emitter_probes, regia::pbr::lighting::light_profile_code(lights[li].position_kind.w), lights[li].direction_radius.xyz, -ldir);
        }
        let nol = dot(n, ldir);
        if (att <= 0.0 || nol <= 0.0) {
            continue;
        }
#ifdef FORWARD_LITE_HIGH
        let tile_f = lights[li].cone_shadow.z;
        if (tile_f >= 0.0) {
            var tile_index = u32(tile_f + 0.5);
            if (kind == 1u) {
                tile_index = tile_index + cube_face(world - lp);
            }
            att *= atlas_visibility(tile_index, world, n, ldir);
            if (att <= 0.0) {
                continue;
            }
        }
#endif
        sum += lite_brdf(n, v, ldir, diffuse_color, f0, perceptual, dfg)
            * lights[li].color_intensity.rgb * (lights[li].color_intensity.w * att * nol);
    }
    return sum;
}

#ifdef LITE_SURFACE_WEATHER
// `lab_forward.wgsl`'s `precip_exposure`, less the atlas bookkeeping: 1 under open sky, 0 under
// cover. Only the High tier binds the atlas; the Low tier has no occlusion view (spec 99 § 5.7)
// and counts every point outside the eye's interior volume as open.
fn lite_precip_exposure(world: vec3<f32>, n_geo: vec3<f32>) -> f32 {
#ifdef FORWARD_LITE_HIGH
    let index = i32(frame.weather2.y);
    if (index >= 0) {
        let tile = shadow_tiles[u32(index)];
        let offset = world + n_geo * tile.bias.x * 1.5;
        let local = regia::shadows::shadow_uv_from_clip(tile.view_proj * vec4<f32>(offset, 1.0));
        if (local.x >= 0.0 && local.y >= 0.0 && local.x <= 1.0 && local.y <= 1.0) {
            let tile_min = tile.uv.zw;
            let tile_max = tile.uv.zw + tile.uv.xy;
            let uv = local.xy * tile.uv.xy + tile.uv.zw;
            let texel = 1.0 / max(frame.extra.y, 1.0);
            let slope = regia::shadows::receiver_plane_slope(tile.view_proj, n_geo) * texel / tile.uv.x;
            let compare_z = min(local.z - abs(slope.x) - abs(slope.y) - 0.02 * tile.bias.y, 1.0);
            return regia::shadows::shadow_pcf_rect(shadow_atlas, shadow_samp, uv, compare_z, texel, tile_min, tile_max, 1);
        }
    }
#endif
    if (frame.interior_center.w > 0.5) {
        let inside = abs(world - frame.interior_center.xyz) - frame.interior_half.xyz;
        if (max(inside.x, max(inside.y, inside.z)) <= 0.0) {
            return 0.0;
        }
    }
    return 1.0;
}

// `lab_forward.wgsl`'s `weather_response`: porosity, wet, snow, puddles from `alpha.w`'s bits.
fn lite_weather_response(roughness: f32, metallic: f32) -> vec4<f32> {
    let bits = u32(material.alpha.w + 0.5);
    let wet = 1.0 - f32((bits >> 2u) & 15u) / 15.0;
    let snow = 1.0 - f32((bits >> 6u) & 15u) / 15.0;
    let puddles = f32((bits >> 10u) & 1u);
    let stored = (bits >> 11u) & 255u;
    var porosity = regia::forward::surface_weather::weather_porosity(roughness, metallic);
    if (stored > 0u) {
        porosity = f32(stored - 1u) / 254.0;
    }
    return vec4<f32>(porosity, wet, snow, puddles);
}

// `lab_forward.wgsl`'s `weather_inscatter`: the fog's color in the fog's mode.
fn lite_weather_inscatter(view_dir: vec3<f32>) -> vec3<f32> {
#ifdef FORWARD_TRIM
    // Adreno (ADR-3145): the fog colour without the aerial haze.
    return frame.fog_color.rgb / max(frame.post_exposure.y, 1e-8);
#else
    if (frame.fog_color.a > 0.5) {
        let tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
        return regia::aerial::haze_inscatter(
            view_dir,
            -normalize(dir_light.direction),
            dir_light.color * dir_light.illuminance,
            frame.post_exposure.z,
        ) * tint;
    }
    return frame.fog_color.rgb / max(frame.post_exposure.y, 1e-8);
#endif
}
#endif

// A blue → cyan → green → yellow → red ramp over t in [0, 1]: `lab_forward.wgsl`'s
// `debug_view_heat` (ADR-7211).
fn lite_view_heat(t_in: f32) -> vec3<f32> {
    let t = clamp(t_in, 0.0, 1.0);
    let r = clamp(1.5 - abs(4.0 * t - 3.0), 0.0, 1.0);
    let g = clamp(1.5 - abs(4.0 * t - 2.0), 0.0, 1.0);
    let b = clamp(1.5 - abs(4.0 * t - 1.0), 0.0, 1.0);
    return max(vec3<f32>(r, g, b), vec3<f32>(0.05, 0.05, 0.2));
}

// The display-referred colour of debug view `code` (`lights_views::shading_view_code`): the lab
// shader's `debug_view_color` on the lite frame's inputs (ADR-7254). Light complexity counts the
// froxel's whole list (the lite loop stops at `MAX_LIGHTS`); shadow cascades colours the cascades
// the frame planned.
fn lite_debug_view_color(
    code: u32,
    world: vec3<f32>,
    pix: vec2<f32>,
    view_z: f32,
    base: vec3<f32>,
    n: vec3<f32>,
    perceptual: f32,
    metallic: f32,
    // Not `occlusion`: naga_oil reads a bare identifier named like an imported module as that
    // module, and the lightmap variant imports `regia::forward::occlusion` (integ-post05b, ADR-7310).
    material_ao: f32,
    emissive_display: vec3<f32>,
    uv_footprint: f32,
    tex_size: f32,
) -> vec3<f32> {
    var out = base;
    switch code {
        case 1u: { out = base + emissive_display; }
        case 2u: { out = normalize(n) * 0.5 + vec3<f32>(0.5); }
        case 3u: { out = base; }
        case 4u: { out = vec3<f32>(perceptual); }
        case 5u: { out = vec3<f32>(metallic); }
        case 6u: { out = vec3<f32>(material_ao); }
        case 7u: {
            let uv = pix / max(frame.screen.xy, vec2<f32>(1.0));
            let cx = min(u32(uv.x * 16.0), 15u);
            let cy = min(u32(uv.y * 9.0), 8u);
            let cz = regia::clustering::depth_to_slice(max(view_z, frame.params.x), frame.params.x, frame.params.y);
            out = lite_view_heat(f32(counts[regia::clustering::cluster_index(cx, cy, cz)]) / 16.0);
        }
        case 8u: {
            out = vec3<f32>(0.3);
            if (frame.extra.x > 0.5) {
                let count = clamp(u32(csm.count.x + 0.5), 1u, 4u);
                var cascade = 0u;
                for (var i = 0u; i + 1u < count; i = i + 1u) {
                    if (view_z > csm.cascades[i].split_distance) {
                        cascade = i + 1u;
                    }
                }
                if (view_z <= csm.cascades[count - 1u].split_distance) {
                    switch cascade {
                        case 0u: { out = vec3<f32>(0.9, 0.2, 0.2); }
                        case 1u: { out = vec3<f32>(0.2, 0.85, 0.25); }
                        case 2u: { out = vec3<f32>(0.25, 0.4, 0.95); }
                        default: { out = vec3<f32>(0.95, 0.85, 0.2); }
                    }
                }
            }
        }
        case 9u: {
            let now = frame.unjittered_view_proj * vec4<f32>(world, 1.0);
            let prev = frame.prev_view_proj * vec4<f32>(world, 1.0);
            let mv = now.xy / max(abs(now.w), 1e-6) - prev.xy / max(abs(prev.w), 1e-6);
            out = vec3<f32>(clamp(vec2<f32>(0.5) + mv * 4.0, vec2<f32>(0.0), vec2<f32>(1.0)), 0.25);
        }
        case 10u: {
            let lod = log2(max(uv_footprint * tex_size, 1e-6));
            out = lite_view_heat(clamp(lod, 0.0, 10.0) / 10.0);
        }
        default: {}
    }
    return out;
}

@fragment
fn fs_main(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    // Every implicit derivative (the material samples) comes before the `discard` (ADR-265).
#ifdef TEXTURE_TABLE
    let alb = table_sample(0u, in.uv);
    let orm = table_sample(2u, in.uv);
#else
    let alb = textureSample(albedo_tex, tex_samp, in.uv);
    let orm = textureSample(occlusion_tex, tex_samp, in.uv);
#endif
    var emissive = vec3<f32>(0.0);
    if (material.emissive.w > 0.0) {
#ifdef TEXTURE_TABLE
        let e = table_sample(3u, in.uv).rgb;
#else
        let e = textureSample(emissive_tex, tex_samp, in.uv).rgb;
#endif
        emissive = material.emissive.rgb * material.emissive.w * e;
    }
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
#ifdef LITE_SURFACE_WEATHER
    let n_geo = n;
#endif
    let normal_scale = material.factors.z;
    if (normal_scale > 0.0) {
#ifdef TEXTURE_TABLE
        let ts = decode_normal_sample(table_sample(1u, in.uv));
#else
        let ts = decode_normal_sample(textureSample(normal_tex, tex_samp, in.uv));
#endif
        let t_front = normalize(in.world_tangent.xyz);
        let b = cross(n, t_front) * in.world_tangent.w;
        let t = select(-t_front, t_front, front_facing);
        n = normalize(mat3x3<f32>(t, b, n) * vec3<f32>(ts.x * normal_scale, ts.y * normal_scale, ts.z));
    }
    // The mip-level debug view's UV footprint (ADR-7254), before the `discard` like every other
    // derivative; behind the frame uniform's view code, so a lit frame pays nothing for it.
    var view_uv_footprint = 0.0;
    if (u32(frame.view_mode.x + 0.5) == 10u) {
        view_uv_footprint = max(length(dpdx(in.uv)), length(dpdy(in.uv)));
    }
    let tint = override_tint(in.overrides);
    var base = alb.rgb * material.base_color.rgb * in.color.rgb * tint.rgb;
    // Per-vertex occlusion (`alpha.w` bit 1, ADR-1512): the vertex alpha is a factor on the
    // indirect light, not an opacity.
    let alpha_flags = u32(material.alpha.w + 0.5);
    let vertex_ao_on = (alpha_flags & 2u) != 0u;
    let vertex_ao = select(1.0, clamp(in.color.a, 0.0, 1.0), vertex_ao_on);
#ifdef ALPHA_TEST
    let alpha = alb.a * material.base_color.a * select(in.color.a, 1.0, vertex_ao_on);
    if ((u32(material.alpha.x + 0.5) == 1u && alpha < material.alpha.y) || lod_dither_rejects(in.lod_dither, in.clip_position.xy)) {
        discard;
    }
#endif
    // Unlit (`alpha.z` bit 0, or a render override): display-referred, as the lab shader.
    if (((u32(material.alpha.z + 0.5) & 1u) != 0u) || (in.overrides.y & 3u) == 3u) {
#ifdef skip_srgb_encode
        return vec4<f32>(base, 1.0);
#else
        return vec4<f32>(regia::utils::encode_srgb(base), 1.0);
#endif
    }

    // A packed ORM (`alpha.w` bit 0): G and B scale the glTF roughness and metallic factors.
    let mr_scale = select(vec2<f32>(1.0), orm.gb, (alpha_flags & 1u) != 0u);
    var metallic = regia::utils::saturate(material.factors.x * mr_scale.y);
    // The lab shader's `MIN_ALPHA` floor as a perceptual roughness: no firefly on a mirror.
    var perceptual = max(regia::utils::saturate(material.factors.y * mr_scale.x), 0.045);
#ifdef LITE_SURFACE_WEATHER
    // Surface weather (spec 99 § 6.5), the lab shader's rule after the surface and before
    // lighting. Still behind the lab's uniform branch: an extinction-only frame skips it.
    if (frame.weather.x + frame.weather.y + frame.weather.z > 0.0) {
        let weathered = regia::forward::surface_weather::weather_apply(
            regia::forward::surface_weather::WeatherSurface(base, perceptual, metallic, n),
            n_geo,
            in.world_position,
            frame.weather,
            frame.weather2.x,
            lite_weather_response(perceptual, metallic),
            lite_precip_exposure(in.world_position, n_geo),
            0.0,
        );
        base = weathered.albedo;
        perceptual = max(weathered.roughness, 0.045);
        metallic = weathered.metallic;
        n = weathered.normal;
    }
#endif
    let ao = mix(1.0, orm.r, regia::utils::saturate(material.factors.w)) * vertex_ao;

    let v = normalize(frame.camera_pos.xyz - in.world_position);
    let no_v = clamp(dot(n, v), 1e-5, 1.0);
    let dfg = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0);
#ifdef BRDF_ENERGY_COMPENSATED
    // `BrdfEnergy::Compensated` (ADR-2632), the lite form: every diffuse term takes the share the
    // specular layer leaves toward the eye, 1 − (F0·scale + bias) from the analytic split-sum fit
    // (no table fetch, no per-light cost). Not reciprocal, unlike the lab shader's coupling.
    let f0 = mix(vec3<f32>(select(0.04, material.pom.w, material.pom.w > 0.0)), base, vec3<f32>(metallic));
    let lite_es = env_brdf_approx(perceptual, no_v);
    let diffuse_color = base * (1.0 - metallic)
        * (vec3<f32>(1.0) - clamp(f0 * lite_es.x + vec3<f32>(lite_es.y), vec3<f32>(0.0), vec3<f32>(1.0)));
#else
    let diffuse_color = base * (1.0 - metallic);
    let f0 = mix(vec3<f32>(select(0.04, material.pom.w, material.pom.w > 0.0)), base, vec3<f32>(metallic));
#endif

    let view_z = -(frame.view[0].z * in.world_position.x + frame.view[1].z * in.world_position.y
        + frame.view[2].z * in.world_position.z + frame.view[3].z);

    var lit = vec3<f32>(0.0);
    let l = -normalize(dir_light.direction);
    let no_l = dot(n, l);
    if (no_l > 0.0) {
#ifdef LIGHTMAP
        // A lightmap baked with `--direct-sun` already holds this diffuse term (ADR-2024).
        let sun_diffuse = select(diffuse_color, vec3<f32>(0.0), lightmap_holds_sun_diffuse(in.lightmap_scale_offset));
#else
        let sun_diffuse = diffuse_color;
#endif
        var sun = lite_brdf(n, v, l, sun_diffuse, f0, perceptual, dfg) * dir_light.color * (dir_light.illuminance * no_l);
        if (frame.extra.x > 0.5) {
            sun *= sun_visibility(in.world_position, n, no_l, view_z);
        }
        lit += sun;
    }
    // Planet-shine fill, unshadowed.
    if (frame.fill_dir.w > 0.5) {
        let lf = normalize(frame.fill_dir.xyz);
        lit += diffuse_color * frame.fill_color.rgb * (max(dot(n, lf), 0.0) * (1.0 / regia::pbr::lighting::PI));
    }
    // More directional lights (ADR-7211): the sun's BRDF, unshadowed.
    let extra_lights = min(u32(frame.view_mode.y + 0.5), 3u);
    for (var xi = 0u; xi < extra_lights; xi = xi + 1u) {
        let xl = -normalize(frame.extra_dir[2u * xi].xyz);
        let x_no_l = max(dot(n, xl), 0.0);
        if (x_no_l > 0.0) {
            lit += lite_brdf(n, v, xl, diffuse_color, f0, perceptual, dfg) * frame.extra_dir[2u * xi + 1u].rgb * x_no_l;
        }
    }
    // Ambient volumes (ADR-7211): a diffuse fill inside each box, fading over its blend distance.
    // The offset is turned into the box's own frame (`frame.ambient_rot`, ADR-7275), as the lab
    // shader's `ambient_volume_local`.
    let vol_count = min(u32(frame.view_mode.z + 0.5), 4u);
    for (var vi = 0u; vi < vol_count; vi = vi + 1u) {
        let vc = frame.ambient_box[3u * vi];
        let vq = frame.ambient_rot[vi];
        let vrel = in.world_position - vc.xyz;
        let vt = 2.0 * cross(vq.xyz, vrel);
        let vlocal = vrel + vq.w * vt + cross(vq.xyz, vt);
        let d = length(max(abs(vlocal) - frame.ambient_box[3u * vi + 1u].xyz, vec3<f32>(0.0)));
        var w = select(0.0, 1.0, d <= 0.0);
        if (d > 0.0 && vc.w > 0.0) {
            w = clamp(1.0 - d / vc.w, 0.0, 1.0);
        }
        lit += diffuse_color * frame.ambient_box[3u * vi + 2u].xyz * (w * ao * (1.0 / regia::pbr::lighting::PI));
    }
    lit += local_lights(in.world_position, n, v, in.clip_position.xy, view_z, diffuse_color, f0, perceptual, dfg);

    // Indirect: one irradiance and one prefilter fetch. An interior view (or the interior box)
    // takes the neutral floor, as the lab shader's no-probe path.
    let floor_diffuse = vec3<f32>(frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI));
    let in_box = all(abs(in.world_position - frame.interior_center.xyz) <= frame.interior_half.xyz);
#ifdef LIGHTMAP
    // A receiver: the baked irradiance replaces the diffuse term of all three branches below, and
    // holds the vertex occlusion already, so only the material's occlusion applies (as the lab
    // shader). The specular replaces the sky reflection while the receiver setting is on.
#ifdef FORWARD_LITE_LOW
    let lm_e = lightmap_atlas_sample(lightmap_atlas_uv(in.lightmap_uv, in.lightmap_scale_offset), lightmap_page(in.lightmap_scale_offset)).rgb * lightmap_params.scale.x;
    let baked = lm_e;
    // No direction lookup at Low: the specular reads the field as uniform (ADR-4762).
    let lm_direction = vec4<f32>(0.0);
#else
    let lm_texel = lightmap_sample(in.lightmap_uv, in.lightmap_scale_offset);
    let lm_e = lm_texel.e;
    let lm_direction = lm_texel.direction;
    let baked = lightmap_shade_directional(lm_e, lm_direction, n);
#endif
    // The capsule occluders the bake cannot hold (ADR-2656): one uniform read without capsules.
    let capsules = regia::ddgi::ddgi_screen_occluder_visibility(in.clip_position.xy, in.world_position, frame.camera_pos.xyz);
    let lm_ao = mix(1.0, orm.r, regia::utils::saturate(material.factors.w));
    let lm_ab = dfg.rg;
    let lm_diffuse_weight = select(vec3<f32>(1.0), lightmap_diffuse_weight(f0, lm_ab), lightmap_specular_on(in.lightmap_scale_offset));
#ifdef BRDF_ENERGY_COMPENSATED
    // One partition for the baked diffuse (ADR-5622): with the baked specular on, `lm_diffuse_weight`
    // is it; with only the sky reflection beside it, `diffuse_color`'s; with no reflection, none.
    let lm_sky_reflects = !lightmap_specular_on(in.lightmap_scale_offset)
        && (!(frame.camera_pos.w > 0.5 && (frame.interior_center.w < 0.5 || in_box)) && frame.extra.z > 0.5);
    let lm_diffuse_color = select(base * (1.0 - metallic), diffuse_color, lm_sky_reflects);
#else
    let lm_diffuse_color = diffuse_color;
#endif
    // The multi-scatter share of the receiver's reflection (ADR-5945, the lab shader's rule): what
    // the single-scatter lobe loses comes back as a broad lobe lit by the same baked light, taken
    // out of the diffuse share first. Zero unless the baked specular is on and the material asks
    // for multi-scatter, which an authored material does by default: without it a rough metal
    // receiver kept as little as half of the baked light the lab shader gives it.
    var lm_multi_scatter = vec3<f32>(0.0);
    if (lightmap_specular_on(in.lightmap_scale_offset) && material.response.y > 0.5) {
        lm_multi_scatter = clamp(regia::pbr::lighting::multi_scatter_ems(f0, lm_ab), vec3<f32>(0.0), lm_diffuse_weight);
        lit += lightmap_diffuse(lm_multi_scatter, baked) * capsules * lm_ao;
    }
    // The baked diffuse takes the multi-bounce form of the material's occlusion (ADR-5945, the lab
    // shader's `ao_diffuse`): a bright surface loses less baked light in a cavity than its
    // occlusion map says. Only where the map occludes, so a material without one draws the same
    // frame. The reflection keeps the occlusion as it is: the lab shader's specular-occlusion
    // forms differ from it only on a smooth receiver.
    var lm_ao_diffuse = vec3<f32>(lm_ao);
    if (lm_ao < 1.0) {
        lm_ao_diffuse = regia::forward::occlusion::multi_bounce_ao(lm_ao, base * (1.0 - metallic) + f0 * metallic);
    }
    lit += lightmap_diffuse(lm_diffuse_color, baked) * capsules * lm_ao_diffuse * (lm_diffuse_weight - lm_multi_scatter);
    if (lightmap_specular_on(in.lightmap_scale_offset)) {
        // The receiver's indirect specular from the same baked light (ADR-4762, the lab shader's
        // rule): it replaces the sky's reflection, with the material's occlusion only.
        let lm_r = reflect(-v, n);
        let lm_lobe = lightmap_spec_lobe(n, lm_r, perceptual * perceptual);
        let lm_weight = vec3<f32>(1.0) - lm_diffuse_weight;
        lit += lightmap_lobe_radiance(lm_e, lm_direction, lm_lobe) * lm_weight * lm_ao;
    } else if (!(frame.camera_pos.w > 0.5 && (frame.interior_center.w < 0.5 || in_box)) && frame.extra.z > 0.5) {
        let pref = textureSampleLevel(prefilter_tex, ibl_samp, fast_equirect_uv(reflect(-v, n)), perceptual * frame.extra.w).rgb * frame.post_exposure.z;
        let ab = env_brdf_approx(perceptual, no_v);
        lit += pref * (f0 * ab.x + vec3<f32>(ab.y)) * ao;
    }
#else
    if (frame.camera_pos.w > 0.5 && (frame.interior_center.w < 0.5 || in_box)) {
#ifdef BRDF_ENERGY_COMPENSATED
        // No reflection beside the floor: its diffuse keeps the whole base (ADR-2632).
        lit += base * (1.0 - metallic) * floor_diffuse * ao;
#else
        lit += diffuse_color * floor_diffuse * ao;
#endif
    } else if (frame.extra.z > 0.5) {
        let ibl_scale = frame.post_exposure.z;
        let sky = textureSampleLevel(irradiance_tex, ibl_samp, fast_equirect_uv(n), 0.0).rgb * ibl_scale;
        let r = reflect(-v, n);
        let pref = textureSampleLevel(prefilter_tex, ibl_samp, fast_equirect_uv(r), perceptual * frame.extra.w).rgb * ibl_scale;
        let ab = env_brdf_approx(perceptual, no_v);
        lit += (diffuse_color * sky + pref * (f0 * ab.x + vec3<f32>(ab.y))) * ao;
    } else {
#ifdef BRDF_ENERGY_COMPENSATED
        // Flat ambient: no reflection beside it, so the whole base (ADR-2632).
        lit += base * (1.0 - metallic) * regia::utils::sky_color(n) * (frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI) * ao);
#else
        lit += diffuse_color * regia::utils::sky_color(n) * (frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI) * ao);
#endif
    }
#endif

    var scene_color = lit + emissive;
    let override_glow = override_emissive(in.overrides);
    if any(override_glow > vec3<f32>(0.0)) {
        let exposure = select(frame.post_exposure.y, frame.post_exposure.x, frame.post_exposure.x > 0.0);
        scene_color += override_glow / max(exposure, 1e-8);
    }
    if (frame.fog_color.a > 0.5) {
        let fog_tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
        scene_color = regia::aerial::apply_aerial(
            scene_color,
            in.world_position,
            frame.camera_pos.xyz,
            frame.fog_params,
            fog_tint,
            l,
            dir_light.color * dir_light.illuminance,
            frame.post_exposure.z,
        );
    } else {
        let exposure = max(frame.post_exposure.y, 1e-8);
        scene_color = regia::fog::apply_fog(
            scene_color,
            in.world_position,
            frame.camera_pos.xyz,
            frame.fog_params,
            vec4<f32>(frame.fog_color.rgb / exposure, frame.fog_color.a),
        );
    }
#ifdef LITE_SURFACE_WEATHER
    // The falling rain or snow's own constant extinction (spec 99 § 3.2), with the fog's color.
    if (frame.weather2.w > 0.0) {
        scene_color = regia::forward::surface_weather::weather_extinction(
            scene_color,
            length(in.world_position - frame.camera_pos.xyz),
            frame.weather2.w,
            lite_weather_inscatter(normalize(in.world_position - frame.camera_pos.xyz)),
        );
    }
#endif
    // Debug view modes (ADR-7254): the lab shader's display-referred colour over the exposure the
    // frame applies, so it reads the same in any scene. Lit frames (code 0) skip it.
    let view_code = u32(frame.view_mode.x + 0.5);
    if (view_code != 0u) {
        let view_exposure = max(select(frame.post_exposure.y, frame.post_exposure.x, frame.post_exposure.x > 0.0), 1e-8);
#ifdef TEXTURE_TABLE
        let view_tex_size = f32(max(textureDimensions(texture_table[table_index(0u)]).x, 1u));
#else
        let view_tex_size = f32(max(textureDimensions(albedo_tex).x, 1u));
#endif
        scene_color = lite_debug_view_color(view_code, in.world_position, in.clip_position.xy, view_z, base, n, perceptual, metallic, ao, emissive * view_exposure, view_uv_footprint, view_tex_size) / view_exposure;
    }

    if (frame.post_exposure.x > 0.0) {
        // Pre-exposed radiance for the post stack. Alpha is the lab shader's roughness for the SSR
        // composite; no Mobile frame reads it.
        let finite = regia::utils::finite_or_zero(scene_color);
        return vec4<f32>(min(max(finite, vec3<f32>(0.0)) * frame.post_exposure.x, vec3<f32>(6.0e4)), perceptual);
    }
    let exposed = clamp(scene_color * frame.post_exposure.y, vec3<f32>(0.0), vec3<f32>(1.0e4));
    var color = regia::utils::default_display_transform(exposed);
#ifdef skip_srgb_encode
    color = regia::utils::decode_srgb(color);
#endif
    return vec4<f32>(color, 1.0);
}
