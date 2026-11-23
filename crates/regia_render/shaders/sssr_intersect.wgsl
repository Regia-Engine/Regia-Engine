// SPDX-License-Identifier: MIT
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/sssr/ffx_sssr_intersect.h,
// Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

// SSSR ray march: one GGX-sampled reflection ray per traced pixel of each listed 2×2 quad (one
// thread per quad), traced through the min-depth
// pyramid, validated, and stored as premultiplied radiance (rgb = coverage × radiance,
// a = coverage) plus the ray length the denoiser reprojects with.
//
// Changes for Regia Engine: world-space setup with standard Z; no wave-occupancy early exit (no wave
// intrinsics); a miss falls back to the captured reflection probes (box-projected, Lagarde 2012)
// instead of an environment map, and with no probe the coverage is the hit confidence, so the
// forward pass's own environment specular stays.

#import regia::sssr::{sssr, glossy_weight, is_mirror, world_from_uvz, uvz_from_world,
    linear_depth, sample_reflection, equirect_uv, box_lookup, probe_influence, unpack_surface,
    cost_bound_on, bounded_steps, block_rate}
#ifdef SSSR_STATS
#import regia::sssr::SssrCost
#endif
#import regia::utils::default_display_transform
#ifdef BLUE_NOISE
#import regia::sssr::sample_reflection_with
#import regia::blue_noise
#endif

struct Counters {
    rays: u32,
    tiles: u32,
}

#ifdef BLUE_NOISE
// Spatiotemporal blue noise for the ray directions (ADR-2075).
@group(1) @binding(0) var blue_noise_table: texture_2d_array<f32>;
#endif
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
// Min-depth pyramid levels 1.. (texture level l − 1 is pyramid level l); level 0 is `depth0`.
@group(0) @binding(2) var pyramid: texture_2d<f32>;
@group(0) @binding(3) var normal_tex: texture_2d<u32>;
@group(0) @binding(4) var<storage, read> ray_list: array<u32>;
@group(0) @binding(5) var<storage, read> counters: Counters;
@group(0) @binding(6) var radiance_out: texture_storage_2d<rgba16float, write>;
// The ray length as f32 bits; the reproject replaces it with the packed variance.
@group(0) @binding(7) var ray_len_out: texture_storage_2d<r32uint, write>;
@group(0) @binding(8) var probe_tex: texture_2d_array<f32>;
@group(0) @binding(9) var probe_samp: sampler;
@group(0) @binding(10) var depth0: texture_2d<f32>;
#ifdef SSSR_STATS
// The cost probe (ADR-2557): steps, exhausted rays, half-resolution rays, hits, shared pixels.
@group(0) @binding(11) var<storage, read_write> cost: SssrCost;
#endif

// Steps the last `hierarchical_raymarch` took (the cost probe reads it).
var<private> march_steps: i32;

const FLOAT_MAX: f32 = 3.402823466e+38;

fn load_depth(p: vec2<i32>, mip: i32) -> f32 {
    if mip <= 0 {
        let dims0 = vec2<i32>(textureDimensions(depth0));
        return textureLoad(depth0, clamp(p, vec2<i32>(0), dims0 - vec2<i32>(1)), 0).r;
    }
    let dims = vec2<i32>(textureDimensions(pyramid, mip - 1));
    return textureLoad(pyramid, clamp(p, vec2<i32>(0), dims - vec2<i32>(1)), mip - 1).r;
}

fn mip_resolution(mip: i32) -> vec2<f32> {
    return sssr.screen.xy * exp2(-f32(mip));
}

// Hierarchical traversal of the min-depth pyramid in (uv, depth) space
// (FFX_SSSR_HierarchicalRaymarch, standard Z: smaller depth is nearer).
// `min_mip` is FidelityFX's most detailed mip: 1 traces a rough ray at half resolution.
fn hierarchical_raymarch(origin: vec3<f32>, direction: vec3<f32>, min_mip: i32, max_mip: i32, max_steps: i32) -> vec4<f32> {
    let inv_direction = select(vec3<f32>(FLOAT_MAX), vec3<f32>(1.0) / direction, direction != vec3<f32>(0.0));
    var current_mip = min_mip;
    var res = mip_resolution(current_mip);
    var res_inv = vec2<f32>(1.0) / res;
    var uv_offset = 0.005 / sssr.screen.xy;
    uv_offset = select(uv_offset, -uv_offset, direction.xy < vec2<f32>(0.0));
    let floor_offset = select(vec2<f32>(1.0), vec2<f32>(0.0), direction.xy < vec2<f32>(0.0));
    // Initial advance to the first cell boundary avoids immediate self-intersection.
    var xy_plane = (floor(res * origin.xy) + floor_offset) * res_inv + uv_offset;
    let t0 = xy_plane * inv_direction.xy - origin.xy * inv_direction.xy;
    var current_t = min(t0.x, t0.y);
    var position = origin + current_t * direction;
    var i = 0;
    loop {
        if i >= max_steps || current_mip < min_mip {
            break;
        }
        // Off screen or past the depth range: nothing to hit (FidelityFX rejects it later).
        if any(position.xy < vec2<f32>(0.0)) || any(position.xy > vec2<f32>(1.0)) || position.z < 0.0 || position.z > 1.0 {
            break;
        }
        let mip_pos = res * position.xy;
        let surface_z = load_depth(vec2<i32>(mip_pos), current_mip);
        xy_plane = (floor(mip_pos) + floor_offset) * res_inv + uv_offset;
        let planes = vec3<f32>(xy_plane, surface_z);
        var t = planes * inv_direction - origin * inv_direction;
        t.z = select(FLOAT_MAX, t.z, direction.z > 0.0);
        let t_min = min(min(t.x, t.y), t.z);
        let above_surface = surface_z > position.z;
        let skipped_tile = bitcast<u32>(t_min) != bitcast<u32>(t.z) && above_surface;
        current_t = select(current_t, t_min, above_surface);
        position = origin + current_t * direction;
        let next_out_of_range = skipped_tile && current_mip >= max_mip;
        if !next_out_of_range {
            if skipped_tile {
                current_mip = current_mip + 1;
                res = res * 0.5;
                res_inv = res_inv * 2.0;
            } else {
                current_mip = current_mip - 1;
                res = res * 2.0;
                res_inv = res_inv * 0.5;
            }
        }
        i = i + 1;
    }
    march_steps = i;
    let valid = select(0.0, 1.0, i <= max_steps && current_mip < min_mip);
    return vec4<f32>(position, valid);
}

// FFX_SSSR_ValidateHit: frustum, self-hit, sky, back-face and thickness tests plus a border fade.
fn validate_hit(hit: vec3<f32>, uv: vec2<f32>, world_ray: vec3<f32>) -> f32 {
    if any(hit.xy < vec2<f32>(0.0)) || any(hit.xy > vec2<f32>(1.0)) {
        return 0.0;
    }
    let manhattan = abs(hit.xy - uv);
    if all(manhattan < 2.0 * sssr.screen.zw) {
        return 0.0;
    }
    let texel = vec2<i32>(sssr.screen.xy * hit.xy);
    let surface_z = load_depth(texel / 2, 1);
    if surface_z >= 1.0 {
        return 0.0;
    }
    let hit_normal = unpack_surface(textureLoad(normal_tex, clamp(texel, vec2<i32>(0), vec2<i32>(sssr.screen.xy) - vec2<i32>(1)), 0).r).xyz;
    if dot(hit_normal, world_ray) > 0.0 {
        return 0.0;
    }
    let surface = world_from_uvz(hit.xy, surface_z);
    let ray_point = world_from_uvz(hit.xy, hit.z);
    let distance = length(surface - ray_point);
    let fov = 0.05 * vec2<f32>(sssr.screen.y * sssr.screen.z, 1.0);
    let border = smoothstep(vec2<f32>(0.0), fov, hit.xy) * (vec2<f32>(1.0) - smoothstep(vec2<f32>(1.0) - fov, vec2<f32>(1.0), hit.xy));
    let vignette = border.x * border.y;
    // Thickness grows with distance: a pixel spans more depth far away.
    let thickness = sssr.tuning.z * max(linear_depth(surface_z), 1.0);
    var confidence = 1.0 - smoothstep(0.0, thickness, distance);
    confidence = confidence * confidence;
    return vignette * confidence;
}

fn sample_probes(world: vec3<f32>, refl: vec3<f32>) -> vec4<f32> {
    let count = min(u32(sssr.proj.z + 0.5), 4u);
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    var total = 0.0;
    for (var i = 0u; i < count; i = i + 1u) {
        let weight = sssr.probes[i].half.w;
        if weight <= 0.0 {
            continue;
        }
        let center = sssr.probes[i].center.xyz;
        let half = max(sssr.probes[i].half.xyz, vec3<f32>(0.05));
        total += weight;
        let w = weight * probe_influence(world, center, half);
        if w <= 0.0 {
            continue;
        }
        let lookup = box_lookup(world, refl, center - half, center + half);
        acc += textureSampleLevel(probe_tex, probe_samp, equirect_uv(lookup), i32(i), 0.0).rgb * w;
        wsum += w;
    }
    if wsum <= 1e-5 {
        return vec4<f32>(0.0);
    }
    // a = the share of the probe set that covers this point (0 outside every box).
    return vec4<f32>(probe_radiance(acc / wsum) * sssr.proj.w, clamp(wsum / max(total, 1e-5), 0.0, 1.0));
}

// The share of `world` the local reflection captures cover (ADR-2860), weighed as the forward
// pass weighs them (1 in the box, fading to 0 over the fade distance outside it). The forward pass
// already reflects them there in HDR with the lobe's mip, so a miss defers to it instead of the
// coarse display-referred probe. 0 without captures.
// Depth reconstruction tolerance on a capture box's faces, metres (ADR-2860).
const CAPTURE_FACE_MARGIN_M: f32 = 0.02;

fn capture_cover(world: vec3<f32>) -> f32 {
    var cover = 0.0;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let weight = sssr.captures[i].center.w;
        if weight <= 0.0 {
            continue;
        }
        // `world` comes from the depth buffer: on a box face (a room's box fitted to its walls) it
        // scatters a few millimetres either side, so the face's margin counts as inside.
        let outside = max(length(max(abs(world - sssr.captures[i].center.xyz) - sssr.captures[i].half.xyz, vec3<f32>(0.0))) - CAPTURE_FACE_MARGIN_M, 0.0);
        cover += weight * (1.0 - smoothstep(0.0, max(sssr.captures[i].half.w, 1e-3), outside));
    }
    return clamp(cover, 0.0, 1.0);
}

fn srgb_encode(v: f32) -> f32 {
    return select(1.055 * pow(v, 1.0 / 2.4) - 0.055, v * 12.92, v <= 0.0031308);
}

// Probes are captured through the display transform (`capture_reflection_probe`: AgX and the
// default look, then sRGB bytes), so their texels are display values, not radiance; scaling them
// by 1 / exposure alone lifts and flattens a sky. Undo the transform on luminance: bisect the grey
// curve for the exposed scene value that prints this display luminance, keep the texel's hue.
// Exact for greys; saturated highlights keep the transform's desaturation.
fn probe_radiance(texel: vec3<f32>) -> vec3<f32> {
    let lum = dot(texel, vec3<f32>(0.2126, 0.7152, 0.0722));
    if lum <= 1e-6 {
        return vec3<f32>(0.0);
    }
    let target_display = srgb_encode(min(lum, 1.0));
    // AgX's log2 encode spans [−12.47, +4.03] EV around exposed 1.
    var lo = -12.47;
    var hi = 4.03;
    for (var i = 0; i < 14; i = i + 1) {
        let mid = 0.5 * (lo + hi);
        let printed = default_display_transform(vec3<f32>(exp2(mid))).g;
        if printed < target_display {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    return texel * (exp2(0.5 * (lo + hi)) / lum);
}

// One reflection ray from pixel `p` (written to its radiance and ray length); returns the
// premultiplied radiance and the ray length bits for a quad neighbour that shares it.
struct Traced {
    radiance: vec4<f32>,
    ray_length: u32,
}

fn trace_pixel(coords: vec2<u32>, nr: vec4<f32>) -> Traced {
    let p = vec2<i32>(coords);
    let uv = (vec2<f32>(coords) + vec2<f32>(0.5)) * sssr.screen.zw;
    let n = nr.xyz;
    let roughness = max(nr.w, 0.0);
    let alpha = roughness * roughness;
    let z = load_depth(p, 0);
    let origin = vec3<f32>(uv, z);
    let world = world_from_uvz(uv, z);
    let v = normalize(world - sssr.camera.xyz);
#ifdef BLUE_NOISE
    let refl = sample_reflection_with(v, n, select(alpha, 0.0, is_mirror(roughness)),
        regia::blue_noise::pair(blue_noise_table, coords, u32(sssr.camera.w)));
#else
    let refl = sample_reflection(v, n, select(alpha, 0.0, is_mirror(roughness)), coords);
#endif
    // A point on the ray in front of the near plane gives the screen-space direction (a projective
    // map keeps lines straight).
    let ahead = dot(refl, sssr.forward.xyz);
    let lin = linear_depth(z);
    var len = 1.0;
    if ahead < 0.0 {
        len = min(1.0, 0.5 * max(lin - sssr.forward.w, 0.0) / max(-ahead, 1e-6));
    }
    let end = uvz_from_world(sssr.view_proj, world + refl * max(len, 1e-4));
    let direction = end - origin;
    let max_mip = i32(sssr.temporal.z);
    // Rough reflections (the denoiser blurs them anyway) march the half-resolution level.
    let half_res = roughness >= sssr.budget.x && max_mip >= 1;
    let min_mip = select(0, 1, half_res);
    var max_steps = select(i32(sssr.tuning.w), i32(sssr.budget.y), half_res);
    if cost_bound_on() {
        // The cost bound's step limit by roughness and distance (ADR-2557).
        max_steps = bounded_steps(max_steps, roughness, lin);
    }
    let march = hierarchical_raymarch(origin, direction, min_mip, max_mip, max_steps);
    let hit = march.xyz;
    let world_hit = world_from_uvz(hit.xy, hit.z);
    let world_ray = world_hit - world;
    var confidence = 0.0;
    if march.w > 0.5 {
        confidence = validate_hit(hit, uv, world_ray);
    }
#ifdef SSSR_STATS
    atomicAdd(&cost.steps, u32(max(march_steps, 0)));
    atomicMax(&cost.max_steps, u32(max(march_steps, 0)));
    if march.w <= 0.5 && march_steps >= max_steps {
        atomicAdd(&cost.exhausted_rays, 1u);
    }
    if half_res {
        atomicAdd(&cost.half_res_rays, 1u);
    }
    if confidence > 0.0 {
        atomicAdd(&cost.hits, 1u);
    }
#endif
    var radiance = vec3<f32>(0.0);
    if confidence > 0.0 {
        radiance = textureLoad(hdr_tex, vec2<i32>(sssr.screen.xy * hit.xy), 0).rgb * confidence;
    }
    var coverage = confidence;
    if confidence < 1.0 && sssr.proj.z > 0.5 {
        // Off-screen misses below the horizon take the probe (the local ground the frame's sky
        // IBL does not know); misses toward the sky keep the forward pass's HDR IBL, which
        // carries the frame's own sky (the probe holds an LDR capture of it).
        let below = smoothstep(0.1, -0.1, refl.y);
        let probe = sample_probes(world, refl);
        // A local capture covering the point is the better miss (ADR-2860): the forward pass's
        // HDR capture stays under the uncovered share. Exact (× 1) without captures.
        let w = (1.0 - confidence) * probe.a * below * (1.0 - capture_cover(world));
        radiance += probe.rgb * w;
        coverage += w;
    }
    let fade = glossy_weight(roughness);
    let result = vec4<f32>(max(radiance, vec3<f32>(0.0)), coverage) * fade;
    let ray_length = bitcast<u32>(select(length(world_ray), 1.0e3, confidence <= 0.0));
    textureStore(radiance_out, p, result);
    textureStore(ray_len_out, p, vec4<u32>(ray_length, 0u, 0u, 0u));
    return Traced(result, ray_length);
}

fn surface_at(p: vec2<u32>) -> vec4<f32> {
    let q = min(p, vec2<u32>(sssr.screen.xy) - vec2<u32>(1u));
    return unpack_surface(textureLoad(normal_tex, vec2<i32>(q), 0).r);
}

// The cost bound's block rate (ADR-2557): a block lead's ray serves the other block-rate pixels of
// its 4×4 block (each written by exactly this thread, so the result is deterministic).
fn share_with_block(lead: vec2<u32>, radiance: vec4<f32>, ray_length: u32) {
    let origin = lead & vec2<u32>(~3u);
    for (var k = 0u; k < 16u; k = k + 1u) {
        let coords = origin + vec2<u32>(k % 4u, k / 4u);
        if all(coords == lead) || any(coords >= vec2<u32>(sssr.screen.xy)) {
            continue;
        }
        let r = surface_at(coords).w;
        if r >= 0.0 && block_rate(r) {
            textureStore(radiance_out, vec2<i32>(coords), radiance);
            textureStore(ray_len_out, vec2<i32>(coords), vec4<u32>(ray_length, 0u, 0u, 0u));
#ifdef SSSR_STATS
            atomicAdd(&cost.shared_pixels, 1u);
#endif
        }
    }
}

@compute @workgroup_size(64, 1, 1)
fn intersect(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let entry_index = (wg.x + wg.y * 32768u) * 64u + li;
    if entry_index >= counters.rays {
        return;
    }
    let packed = ray_list[entry_index];
    let base = vec2<u32>(packed & 16383u, (packed >> 14u) & 8191u) * 2u;
    let mask = packed >> 27u;
    // Plain locals (no function-scope struct: SPIR-V 1.6 rejects naga's laid-out types there).
    let bound = cost_bound_on();
    // The ray the quad shares: the even-even pixel's (the base), or under the cost bound the first
    // traced pixel's, since a tile over its ray budget keeps one ray per quad (ADR-2557).
    var shared_radiance = vec4<f32>(0.0);
    var shared_length = 0u;
    var have_shared = false;
    for (var k = 0u; k < 4u; k = k + 1u) {
        if (mask & (1u << k)) == 0u {
            continue;
        }
        let coords = base + vec2<u32>(k & 1u, k >> 1u);
        let surface = surface_at(coords);
        let traced = trace_pixel(coords, surface);
        if bound && block_rate(surface.w) {
            share_with_block(coords, traced.radiance, traced.ray_length);
        }
        if k == 0u || (bound && !have_shared) {
            shared_radiance = traced.radiance;
            shared_length = traced.ray_length;
            have_shared = true;
        }
    }
    if (mask & 1u) == 0u && !bound {
        return;
    }
    // The shared ray also serves the quad's glossy pixels that trace none (rough enough for the
    // denoiser, not a mirror; FidelityFX samples-per-quad 1). Under the cost bound a mirror pixel
    // the tile budget left without a ray takes it too.
    for (var k = select(1u, 0u, bound); k < 4u; k = k + 1u) {
        let coords = base + vec2<u32>(k & 1u, k >> 1u);
        if (mask & (1u << k)) != 0u || any(coords >= vec2<u32>(sssr.screen.xy)) {
            continue;
        }
        let r = surface_at(coords).w;
        if r >= 0.0 && r < sssr.tuning.x && (bound || !is_mirror(r)) && !block_rate(r) {
            textureStore(radiance_out, vec2<i32>(coords), shared_radiance);
            textureStore(ray_len_out, vec2<i32>(coords), vec4<u32>(shared_length, 0u, 0u, 0u));
#ifdef SSSR_STATS
            atomicAdd(&cost.shared_pixels, 1u);
#endif
        }
    }
}
