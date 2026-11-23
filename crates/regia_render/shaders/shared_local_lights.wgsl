// SPDX-License-Identifier: MPL-2.0
// Clustered point and spot lights for a shared draw (the terrain): the forward pass's froxel
// lookup, windowed falloff, spot cone, local-light shadow atlas and isotropic BRDF, as one
// function a host shader calls (`regia_render::shared_local_lights_wgsl`). The host binds
// the frame's own buffers (`SharedPassDraw::set_engine_lights`), so a surface it lights takes
// exactly the light a mesh of the same material and normal takes there.
//
// Not carried over from `lab_forward.wgsl`: fitted area emitters (kind 4+, they need the LTC
// tables; skipped), the translucent casters' coloured shadow, clearcoat and anisotropy. The
// group and first binding (`ELL_GROUP`, `ELL_BINDING_*`) are filled in by the Rust side.
// With `ELL_PROFILES` (`shared_local_lights_profiled_wgsl`, ADR-3247) the light's IES profile
// shapes its intensity as on a mesh: the forward pass's emitter atlas, read at its bottom bands.

#import regia::clustering
#import regia::pbr::lighting
#import regia::pbr::brdf
#import regia::shadows

// The leading fields of the forward pass's `LabFrame` (the same uniform buffer is bound; the
// test `ell_frame_prefix_matches_the_lab_frame` keeps the offsets equal).
struct EllFrame {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    camera_pos: vec4<f32>,
    // x = cluster near, y = cluster far.
    params: vec4<f32>,
    // xy = colour target size (pixels), z = lights in the buffer, w = max lights per cluster.
    screen: vec4<f32>,
    // y = shadow atlas edge (texels).
    extra: vec4<f32>,
    post_exposure: vec4<f32>,
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    // x = shadow filter mode, w = temporal noise index.
    quality: vec4<f32>,
    prev_view_proj: mat4x4<f32>,
    unjittered_view_proj: mat4x4<f32>,
    // x, y = PCSS search / filter taps.
    pcss: vec4<f32>,
}

struct EllLight {
    position_kind: vec4<f32>,
    color_intensity: vec4<f32>,
    direction_radius: vec4<f32>,
    cone_shadow: vec4<f32>,
}

struct EllShadowTile {
    view_proj: mat4x4<f32>,
    uv: vec4<f32>,
    bias: vec4<f32>,
}

@group(ELL_GROUP) @binding(ELL_BINDING_FRAME) var<uniform> ell_frame: EllFrame;
@group(ELL_GROUP) @binding(ELL_BINDING_LIGHTS) var<storage, read> ell_lights: array<EllLight>;
@group(ELL_GROUP) @binding(ELL_BINDING_COUNTS) var<storage, read> ell_counts: array<u32>;
@group(ELL_GROUP) @binding(ELL_BINDING_INDICES) var<storage, read> ell_indices: array<u32>;
@group(ELL_GROUP) @binding(ELL_BINDING_TILES) var<storage, read> ell_tiles: array<EllShadowTile>;
@group(ELL_GROUP) @binding(ELL_BINDING_ATLAS) var ell_atlas: texture_depth_2d;
@group(ELL_GROUP) @binding(ELL_BINDING_SAMPLER) var ell_samp: sampler_comparison;
#ifdef ELL_PROFILES
// The forward pass's emitter probe atlas (`lab_forward.wgsl` binding 33): only the IES profile
// bands at its bottom are read. A host without the frame's atlas binds a stand-in smaller than a
// band, and a profiled light then shades unshaped (`ell_profile`).
@group(ELL_GROUP) @binding(ELL_BINDING_PROFILES) var ell_profiles: texture_2d<f32>;

// The IES relative intensity toward `d` (light → receiver) of light `li`, 1 for a light without a
// profile (a whole kind) or with no atlas bound.
fn ell_profile(li: u32, d: vec3<f32>) -> f32 {
    let kind_w = ell_lights[li].position_kind.w;
    if (fract(kind_w) <= 0.0) {
        return 1.0;
    }
    let code = regia::pbr::lighting::light_profile_code(kind_w);
    if (i32(textureDimensions(ell_profiles).y) < 32 * i32(code & 7u)) {
        return 1.0;
    }
    return regia::pbr::lighting::light_profile_relative(ell_profiles, code, ell_lights[li].direction_radius.xyz, d);
}
#endif

// Cube face of a point light's shadow for direction `dir` (light to receiver).
fn ell_cube_face(dir: vec3<f32>) -> u32 {
    let a = abs(dir);
    if (a.x >= a.y && a.x >= a.z) {
        return select(1u, 0u, dir.x >= 0.0);
    }
    if (a.y >= a.z) {
        return select(3u, 2u, dir.y >= 0.0);
    }
    return select(5u, 4u, dir.z >= 0.0);
}

// Isidoro receiver-plane slope, clamped (`lab_forward.wgsl::atlas_tan_theta`).
fn ell_atlas_tan_theta(no_l: f32) -> f32 {
    let ndl = max(clamp(no_l, 0.0, 1.0), 0.16);
    return min(sqrt(max(1.0 - ndl * ndl, 0.0)) / ndl, 8.0);
}

// The forward pass's atlas lookup (`lab_forward.wgsl::sample_atlas_tile`) for tile
// `tile_index`: normal offset, receiver-plane bias and the frame's filter mode.
fn ell_sample_atlas_tile(tile_index: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, pix: vec2<f32>) -> f32 {
    let tile = ell_tiles[tile_index];
    let no_l = max(dot(normal, l), 0.0);
    let clip = tile.view_proj * vec4<f32>(world, 1.0);
    let texel_world = tile.bias.x * select(1.0, max(clip.w, 1.0e-6), tile.bias.z > 0.5);
    let offset_world = world + normal * texel_world * 1.5;
    let offset_clip = tile.view_proj * vec4<f32>(offset_world, 1.0);
    let local = regia::shadows::shadow_uv_from_clip(offset_clip);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0 || offset_clip.w <= 0.0) {
        return 1.0;
    }
    let uv = local.xy * tile.uv.xy + tile.uv.zw;
    let tile_min = tile.uv.zw;
    let tile_max = tile.uv.zw + tile.uv.xy;
    let texel = 1.0 / max(ell_frame.extra.y, 1.0);
    let dz_dw = select(
        tile.bias.y,
        1.0 / max(offset_clip.w * offset_clip.w * tile.bias.y, 1.0e-9),
        tile.bias.z > 0.5,
    );
    let tan_theta = ell_atlas_tan_theta(no_l);
    let constant_bias = (2.0 + tan_theta) * texel_world * dz_dw;
    let compare_z = local.z - constant_bias;
    let mode = u32(ell_frame.quality.x + 0.5);
    let noise_pix = pix + 5.588238 * ell_frame.quality.w;
    let rot = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715)))) * 6.2831853;
    var vis: f32;
    // 3 and 4 are PCSS (4 only adds the angular sun penumbra, ADR-3202).
    if (mode >= 3u) {
        let slope_per_texel = texel_world * dz_dw * tan_theta;
        let penumbra_per_depth = tile.bias.w * tile.bias.y / max(tile.bias.x, 1.0e-8);
        vis = regia::shadows::shadow_pcss_atlas(
            ell_atlas, ell_samp, uv, compare_z, texel,
            tile_min, tile_max, penumbra_per_depth, slope_per_texel, rot,
            ell_frame.pcss.x, ell_frame.pcss.y,
        );
    } else if (mode == 0u) {
        let half = vec2<f32>(texel * 0.5);
        let clamped = clamp(uv, tile_min + half, tile_max - half);
        vis = textureSampleCompareLevel(ell_atlas, ell_samp, clamped, compare_z);
    } else {
        var radius: i32 = 1;
        if (mode == 2u) {
            radius = 2;
        }
        vis = regia::shadows::shadow_pcf_rect(
            ell_atlas, ell_samp, uv, compare_z, texel,
            tile_min, tile_max, radius,
        );
    }
    return vis;
}

// Whether the frame has any clustered light for this draw (uniform: the caller can skip the
// call and its registers).
fn engine_local_lights_on() -> bool {
    return ell_frame.screen.z > 0.5;
}

// Radiance the frame's point and spot lights reflect toward `v` from the surface point `p`
// at framebuffer pixel `pix` (top-left origin): shading normal `n`, geometric normal `n_geo`
// (the shadow lookup's normal offset), Lambert colour `diffuse_color` (albedo · (1 − metallic)),
// specular F0 `f0`, perceptual roughness `perceptual`. Scene radiance, not pre-exposed.
fn engine_local_lights(
    pix: vec2<f32>,
    p: vec3<f32>,
    n: vec3<f32>,
    n_geo: vec3<f32>,
    v: vec3<f32>,
    diffuse_color: vec3<f32>,
    f0: vec3<f32>,
    perceptual: f32,
) -> vec3<f32> {
    var lit = vec3<f32>(0.0);
    // The froxel the forward pass reads for this pixel (ADR-186: pixels / target size).
    let uv = pix / max(ell_frame.screen.xy, vec2<f32>(1.0));
    let cx = min(u32(uv.x * 16.0), 15u);
    let cy = min(u32(uv.y * 9.0), 8u);
    let view_pos = (ell_frame.view * vec4<f32>(p, 1.0)).xyz;
    let depth = max(-view_pos.z, ell_frame.params.x);
    let cz = regia::clustering::depth_to_slice(depth, ell_frame.params.x, ell_frame.params.y);
    let cid = regia::clustering::cluster_index(cx, cy, cz);
    let nlights = ell_counts[cid];
    let base_i = cid * u32(ell_frame.screen.w);
    let rough = max(perceptual * perceptual, regia::pbr::lighting::MIN_ALPHA);
    let no_v = max(dot(n, v), 1e-4);
    for (var i: u32 = 0u; i < nlights; i = i + 1u) {
        let li = ell_indices[base_i + i];
        // Fitted area emitters (kind 4+) need the LTC tables: meshes only.
        if (ell_lights[li].position_kind.w > 3.5) {
            continue;
        }
        let lp = ell_lights[li].position_kind.xyz;
        let to_light = lp - p;
        let dist = length(to_light);
        let radius = ell_lights[li].direction_radius.w;
        var att = regia::pbr::lighting::distance_attenuation_source(dist, radius, ell_lights[li].cone_shadow.w);
        if (att <= 0.0) {
            continue;
        }
        let ldir = to_light / max(dist, 1e-4);
        let kind = u32(ell_lights[li].position_kind.w + 0.5);
        if (kind == 2u) {
            let spot_dir = ell_lights[li].direction_radius.xyz;
            att *= regia::pbr::lighting::spot_attenuation(ldir, spot_dir, ell_lights[li].cone_shadow.x, ell_lights[li].cone_shadow.y);
            if (att <= 0.0) {
                continue;
            }
        }
#ifdef ELL_PROFILES
        att *= ell_profile(li, -ldir);
        if (att <= 0.0) {
            continue;
        }
#endif
        let nol = dot(n, ldir);
        if (nol <= 0.0) {
            continue;
        }
        // Shadowless lights store a negative tile.
        var local_shadow = 1.0;
        let tile_f = ell_lights[li].cone_shadow.z;
        if (tile_f >= 0.0) {
            var tile_index = u32(tile_f + 0.5);
            if (kind == 1u) {
                tile_index = tile_index + ell_cube_face(p - lp);
            }
            local_shadow = ell_sample_atlas_tile(tile_index, p, n_geo, ldir, pix);
            if (local_shadow <= 0.0) {
                continue;
            }
        }
        // A sphere source (`LightSourceSize`, ADR-1367; `cone_shadow.w` = radius), as the forward pass shades
        // it (ADR-2592): the specular from the representative point with the sphere's energy term, the
        // diffuse at the centre with the centre's Fresnel. A light without one packs 0 and takes none of it.
        // The sign marks a forward-only LTC sphere (`LightSourceLtc`); the size is the magnitude.
        let src_r = abs(ell_lights[li].cone_shadow.w);
        var l_spec = ldir;
        var nol_spec = nol;
        if (src_r > 0.0) {
            l_spec = regia::pbr::lighting::sphere_light_dir(to_light, reflect(-v, n), src_r, rough);
            nol_spec = max(dot(n, l_spec), 0.0);
        }
        let hl = normalize(l_spec + v);
        let noh = max(dot(n, hl), 0.0);
        let loh = max(dot(l_spec, hl), 0.0);
        let lobes = regia::pbr::brdf::isotropic_lobes(no_v, nol_spec, noh, loh, diffuse_color, f0, rough);
        var diff_l = lobes.diffuse;
        var spec_l = lobes.specular;
        if (src_r > 0.0) {
            let hc = normalize(ldir + v);
            diff_l = diffuse_color * (1.0 / regia::pbr::lighting::PI)
                * (vec3<f32>(1.0) - regia::pbr::lighting::f_schlick(max(dot(ldir, hc), 0.0), f0));
            let spec_cos = nol_spec / max(nol, 1e-4);
            spec_l = spec_l * regia::pbr::lighting::sphere_light_gain(rough, src_r, to_light, reflect(-v, n), noh, max(dot(n, hc), 0.0), spec_cos);
        }
        let lc = ell_lights[li].color_intensity.rgb * ell_lights[li].color_intensity.w;
        lit += (diff_l + spec_l) * lc * att * nol * local_shadow;
    }
    return lit;
}
