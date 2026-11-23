// SPDX-License-Identifier: MIT
// Portions ported from Wicked Engine (https://github.com/turanszkij/WickedEngine, commit 7f776d0),
// Copyright (c) 2026 Turánszki János, MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE
// OR OTHER DEALINGS IN THE SOFTWARE.

// Octahedral impostor draw (depth, colour, velocity, decal receiver), after Wicked Engine's
// `impostorVS.hlsl` / `impostorPS.hlsl`: a camera-facing card per visible instance (the
// cull's list, `impostor_cull.wgsl`) that samples the capture atlas — albedo + coverage and
// normal + depth — and is lit like the ground it stands on.
//
// Wicked picks one of N yaw captures; here the view direction, in the instance's object
// space, picks three captures of the hemi-octahedral grid (Cigolle et al. 2014) and blends
// them with barycentric weights. Each capture is sampled where the pixel's view ray crosses
// that capture's image plane through the bounding-sphere centre, and the stored depth moves
// the point off the plane, so the card writes the rock's own depth (contacts, fog, shadow
// lookups) and switches captures without sliding.
//
// Lighting reuses `terrain.wgsl` (this file is appended to it): the engine cascades, SSAO,
// the engine sky irradiance (or the hemisphere ambient), planet-shine, GGX sun specular,
// height fog and aerial perspective — the far-field terms a mesh rock gets from the forward
// pass. Prepended at load: `terrain.wgsl`, `impostor_common.wgsl`.

@group(2) @binding(8) var<uniform> imp_params: ImpParams;
@group(2) @binding(9) var<storage, read> imp_instances: array<ImpInstance>;
@group(2) @binding(10) var<storage, read> imp_protos: array<ImpProto>;
@group(2) @binding(11) var<storage, read> imp_visible: array<u32>;
@group(2) @binding(12) var imp_albedo: texture_2d_array<f32>;
@group(2) @binding(13) var imp_normal_depth: texture_2d_array<f32>;
@group(2) @binding(14) var imp_samp: sampler;

struct ImpOut {
    @builtin(position) position: vec4<f32>,
    // Point on the card, world metres.
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) index: u32,
    // Impostor share of the pixels (hand-over band), far shrink.
    @location(2) @interpolate(flat) band: f32,
    @location(3) @interpolate(flat) shrink: f32,
}

// A `switch`, not an indexed value array (ADR-026).
fn imp_corner(k: u32) -> vec2<f32> {
    switch k {
        case 0u: { return vec2<f32>(-1.0, -1.0); }
        case 1u: { return vec2<f32>(1.0, -1.0); }
        case 2u: { return vec2<f32>(1.0, 1.0); }
        case 3u: { return vec2<f32>(-1.0, -1.0); }
        case 4u: { return vec2<f32>(1.0, 1.0); }
        default: { return vec2<f32>(-1.0, 1.0); }
    }
}

fn imp_quat(inst: ImpInstance) -> vec4<f32> {
    let q = vec4<f32>(unpack2x16snorm(inst.rotation.x), unpack2x16snorm(inst.rotation.y));
    return q / max(length(q), 1.0e-6);
}

fn imp_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

// `hemi_oct_encode` / `hemi_oct_decode` in `scatter/impostor.rs` (Y up).
fn imp_oct_encode(d: vec3<f32>) -> vec2<f32> {
    let s = max(abs(d.x) + max(d.y, 0.0) + abs(d.z), 1.0e-6);
    let p = vec2<f32>(d.x, d.z) / s;
    return vec2<f32>(p.x + p.y, p.x - p.y);
}

fn imp_oct_decode(e: vec2<f32>) -> vec3<f32> {
    let px = (e.x + e.y) * 0.5;
    let pz = (e.x - e.y) * 0.5;
    let y = 1.0 - abs(px) - abs(pz);
    return normalize(vec3<f32>(px, max(y, 0.0), pz));
}

// `capture_basis`: right, with up = cross(forward, right).
fn imp_right(forward: vec3<f32>) -> vec3<f32> {
    let r = cross(vec3<f32>(0.0, 1.0, 0.0), forward);
    if (dot(r, r) < 1.0e-8) {
        return vec3<f32>(1.0, 0.0, 0.0);
    }
    return normalize(r);
}

@vertex
fn vs_impostor(@builtin(vertex_index) vi: u32) -> ImpOut {
    let index = imp_visible[vi / 6u];
    let inst = imp_instances[index];
    let p = imp_protos[imp_proto_index(inst)];
    let eye = frame.camera_pos_time.xyz;
    let d = distance(inst.centre, eye);
    let band = imp_band(p, d);
    let s = imp_shrink(imp_params, p, inst, d);
    let c = inst.pivot + (inst.centre - inst.pivot) * s;
    // The card through the centre covers the sphere's silhouette (5 % margin: the
    // silhouette cone of a sphere is wider than its centre section by r² / 2d²).
    let r = inst.radius * s * 1.05;
    let to_eye = eye - c;
    let len = length(to_eye);
    var f = vec3<f32>(0.0, 0.0, 1.0);
    if (len > 1.0e-5) {
        f = to_eye / len;
    }
    let right = imp_right(f);
    let up = cross(f, right);
    let k = imp_corner(vi % 6u);
    let world = c + (right * k.x + up * k.y) * r;
    return ImpOut(frame.view_proj * vec4<f32>(world, 1.0), world, index, band, s);
}

struct ImpTap {
    albedo: vec3<f32>,
    alpha: f32,
    normal: vec3<f32>,
    // Surface point, object space.
    point: vec3<f32>,
}

// One capture of the grid, sampled where the view ray crosses its image plane.
fn imp_tap(
    eye_o: vec3<f32>,
    ray_o: vec3<f32>,
    c_o: vec3<f32>,
    r0: f32,
    cell: vec2<f32>,
    layer: i32,
    lod: f32,
) -> ImpTap {
    let grid = imp_params.atlas.x;
    let e = (cell + vec2<f32>(0.5)) / grid * 2.0 - vec2<f32>(1.0);
    let df = imp_oct_decode(e);
    let rf = imp_right(df);
    let uf = cross(df, rf);
    let denom = dot(ray_o, df);
    var t = 0.0;
    if (abs(denom) > 1.0e-6) {
        t = dot(c_o - eye_o, df) / denom;
    }
    let on_plane = eye_o + ray_o * t;
    let local = on_plane - c_o;
    let uv = vec2<f32>(dot(local, rf), -dot(local, uf)) / r0 * 0.5 + vec2<f32>(0.5);
    let inside = all(uv >= vec2<f32>(0.0)) && all(uv <= vec2<f32>(1.0));
    // Clamp half a texel (at this mip) inside the capture: no bleed from the neighbour.
    let texels = max(imp_params.atlas.y / exp2(floor(lod)), 1.0);
    let half_texel = 0.5 / texels;
    let st = (cell + clamp(uv, vec2<f32>(half_texel), vec2<f32>(1.0 - half_texel))) / grid;
    let a = textureSampleLevel(imp_albedo, imp_samp, st, layer, lod);
    let nd = textureSampleLevel(imp_normal_depth, imp_samp, st, layer, lod);
    let h = nd.a * 2.0 - 1.0;
    return ImpTap(
        a.rgb * 2.0,
        select(0.0, a.a, inside),
        nd.xyz * 2.0 - vec3<f32>(1.0),
        on_plane + df * (h * r0),
    );
}

struct ImpSurf {
    // Linear albedo × material base colour × class tint × variant shade.
    colour: vec3<f32>,
    alpha: f32,
    normal: vec3<f32>,
    world: vec3<f32>,
    roughness: f32,
}

fn imp_surface(in: ImpOut) -> ImpSurf {
    let inst = imp_instances[in.index];
    let p = imp_protos[imp_proto_index(inst)];
    let q = imp_quat(inst);
    let qi = vec4<f32>(-q.xyz, q.w);
    let scale = max(inst.scale * in.shrink, 1.0e-6);
    let eye = frame.camera_pos_time.xyz;
    let eye_o = imp_rotate(qi, eye - inst.pivot) / scale;
    let ray_o = normalize(imp_rotate(qi, in.world - eye));
    let c_o = vec3<f32>(0.0, p.card.y, 0.0);
    let r0 = max(p.card.z, 1.0e-4);
    var v = eye_o - c_o;
    v.y = max(v.y, 0.0);
    if (dot(v, v) < 1.0e-10) {
        v = vec3<f32>(0.0, 1.0, 0.0);
    }
    v = normalize(v);
    let grid = imp_params.atlas.x;
    let g = clamp((imp_oct_encode(v) * 0.5 + vec2<f32>(0.5)) * grid - vec2<f32>(0.5), vec2<f32>(0.0), vec2<f32>(grid - 1.0));
    let i0 = floor(g);
    let f = g - i0;
    let i1 = min(i0 + vec2<f32>(1.0), vec2<f32>(grid - 1.0));
    // Mip: one atlas texel per screen pixel.
    let d = max(distance(eye, inst.pivot + (inst.centre - inst.pivot) * in.shrink), 1.0e-3);
    let pixel_m = d * imp_params.atlas.w;
    let texel_m = 2.0 * r0 * scale / max(imp_params.atlas.y, 1.0);
    let lod = clamp(log2(max(pixel_m / max(texel_m, 1.0e-6), 1.0)), 0.0, max(imp_params.atlas.z - 1.0, 0.0));
    // A leafless instance draws its bare card (the cull drops one that has none, ADR-3146).
    let leafy = imp_leaf_present(p, inst);
    let layer = i32(select(p.extra.y, p.card.x, leafy) + 0.5);
    // Three captures around the view direction (the cell's triangle), barycentric weights.
    var cb = vec2<f32>(i1.x, i0.y);
    var w = vec3<f32>(1.0 - f.x, f.x - f.y, f.y);
    if (f.y > f.x) {
        cb = vec2<f32>(i0.x, i1.y);
        w = vec3<f32>(1.0 - f.y, f.y - f.x, f.x);
    }
    let ta = imp_tap(eye_o, ray_o, c_o, r0, i0, layer, lod);
    let tb = imp_tap(eye_o, ray_o, c_o, r0, cb, layer, lod);
    let tc = imp_tap(eye_o, ray_o, c_o, r0, i1, layer, lod);
    let wa = w.x * ta.alpha;
    let wb = w.y * tb.alpha;
    let wc = w.z * tc.alpha;
    let alpha = wa + wb + wc;
    let inv = 1.0 / max(alpha, 1.0e-4);
    let albedo = (ta.albedo * wa + tb.albedo * wb + tc.albedo * wc) * inv;
    let n_o = ta.normal * wa + tb.normal * wb + tc.normal * wc;
    let point_o = (ta.point * wa + tb.point * wb + tc.point * wc) * inv;
    let world = inst.pivot + imp_rotate(q, point_o * scale);
    var n = imp_rotate(q, n_o);
    if (dot(n, n) < 1.0e-10) {
        n = normalize(eye - world);
    }
    // Tint variant shade (`build_prototype`: 1 ± tint_jitter over the variants).
    let variants = max(p.extra.x, 1.0);
    let variant = f32((inst.packed >> 16u) & 0xffu);
    var shade = 1.0;
    if (variants > 1.5) {
        shade = 1.0 + p.tint.a * (2.0 * variant / (variants - 1.0) - 1.0);
    }
    var base = clamp(p.tint.rgb * shade, vec3<f32>(0.0), vec3<f32>(1.0));
    if (leafy) {
        base *= p.season.rgb;
    }
    return ImpSurf(albedo * base, alpha, normalize(n), world, clamp(p.card.w, 0.04, 1.0));
}

// The hand-over dither, the threshold of `lod_dither_discard` in `lab_forward.wgsl` (ADR-3146): the
// impostor keeps the pixels below `band`, the mesh the others. A held view (`counts.y` bit 0, the
// frame's own test: the unjittered view-projection equals the last frame's, or no last frame) takes
// interleaved gradient noise, and a moving one the scrambled hierarchy, whose residual under a short
// history has no lattice. Golden-ratio step per frame on both.
fn imp_dither_discard(band: f32, frag: vec2<f32>) -> bool {
    if (band >= 1.0) {
        return false;
    }
    let t = f32(u32(max(imp_params.screen.w, 0.0) + 0.5) & 1023u);
    var phase = fract(52.9829189 * fract(dot(frag, vec2<f32>(0.06711056, 0.00583715))));
    if ((imp_params.counts.y & 1u) == 0u) {
        phase = alpha_hash_phase(vec2<u32>(max(frag, vec2<f32>(0.0))));
    }
    let d = fract(phase + 0.618034 * t);
    return d >= band;
}

// `alpha_hash_pcg` and `alpha_hash_phase` of `lab_forward.wgsl`, unchanged: both sides of the
// hand-over must draw one threshold (`scatter_impostor_stipple` compares the sources).
fn alpha_hash_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn alpha_hash_phase(pix: vec2<u32>) -> f32 {
    var rank = 0u;
    var h = 0u;
    for (var level = 0u; level < 3u; level = level + 1u) {
        let pos = ((pix.x >> level) & 1u) | (((pix.y >> level) & 1u) << 1u);
        let cell = pix >> vec2<u32>(level + 1u);
        h = alpha_hash_pcg((cell.x * 0x8da6b343u) ^ (cell.y * 0xd8163841u) ^ ((level + 1u) * 0xcb1ab31fu));
        // Permutation `p` of S4: child `a` takes digit 0; the other three take 1, 2, 3 in one of
        // the six orders (rotation `k`, reflected when `p >= 12`).
        let p = h % 24u;
        let a = p & 3u;
        let k = (p >> 2u) % 3u;
        var j = pos - select(0u, 1u, pos > a);
        if (p >= 12u) {
            j = 2u - min(j, 2u);
        }
        let digit = select(1u + (j + k) % 3u, 0u, pos == a);
        rank = rank * 4u + digit;
    }
    return (f32(rank) + f32(h >> 8u) * (1.0 / 16777216.0)) * (1.0 / 64.0);
}

fn imp_depth(world: vec3<f32>) -> f32 {
    let clip = frame.view_proj * vec4<f32>(world, 1.0);
    return clamp(clip.z / max(clip.w, 1.0e-6), 0.0, 1.0);
}

@fragment
fn fs_impostor_depth(in: ImpOut) -> @builtin(frag_depth) f32 {
    let s = imp_surface(in);
    if (s.alpha < 0.5 || imp_dither_discard(in.band, in.position.xy)) {
        discard;
    }
    return imp_depth(s.world);
}

struct ImpColorOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

fn imp_shade(s: ImpSurf, frag: vec2<f32>) -> vec4<f32> {
    let n = s.normal;
    let world = s.world;
    let colour = s.colour;
    var ssao = 1.0;
    if (engine_csm.screen.z > 0.5) {
        let ao_dims = vec2<f32>(textureDimensions(engine_ao));
        let target_px = max(engine_csm.screen.xy, vec2<f32>(1.0));
        let texel = clamp(vec2<i32>(frag * ao_dims / target_px), vec2<i32>(0), vec2<i32>(ao_dims) - vec2<i32>(1));
        ssao = textureLoad(engine_ao, texel, 0).r;
    }
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    let ndl = max(dot(n, sun_dir), 0.0);
    let sun_radiance = frame.sun_color_exposure.rgb * frame.sun_color_exposure.a;
    var sun_vis = sample_engine_csm(world, n);
    // The engine's cloud transmittance map, as on the terrain (`shared_sun_shadow`, ADR-320).
    if (engine_csm.cloud.w > 0.5) {
        sun_vis *= cloud_sun_transmittance(engine_cloud, engine_csm.cloud, world - engine_csm.origin.xyz, sun_dir, frame.camera_pos_time.xyz - engine_csm.origin.xyz);
    }
    // Sky light: the engine's irradiance map about the normal (as on a mesh), or the
    // hemisphere sky/ground ambient.
    var ambient = frame.ambient_debug.rgb;
    if (frame.ground_ambient.w > 0.5) {
        let up = 0.5 + 0.5 * n.y;
        ambient = frame.ambient_debug.rgb * up + frame.ground_ambient.rgb * (1.0 - up);
    }
    if (frame.output_mode.w > 0.0) {
        ambient = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(n), 0.0).rgb * frame.output_mode.w;
    }
    var lit = colour * (sun_radiance * ndl * sun_vis * INV_PI + ambient * ssao);
    if (frame.fill_dir.w > 0.5) {
        lit += colour * frame.fill_color.rgb * max(dot(n, normalize(frame.fill_dir.xyz)), 0.0) * INV_PI * ssao;
    }
    let view_dir = normalize(frame.camera_pos_time.xyz - world);
    lit += sun_radiance * specular_ggx(n, view_dir, sun_dir, s.roughness, 0.04) * sun_vis;
    if (frame.output_mode.w > 0.0) {
        let no_v = max(dot(n, view_dir), 1.0e-3);
        let uv = clamp(vec2<f32>(no_v, s.roughness), vec2<f32>(0.01), vec2<f32>(0.99));
        let ab = textureSampleLevel(sky_brdf, sky_samp, uv, 0.0).rg;
        lit += ambient * (0.04 * ab.x * no_v) * ssao;
    }
    if (frame.output_mode.x > 0.5) {
        var rgb = max(lit, vec3<f32>(0.0));
        let params = frame.fog_params;
        if (params.w > 0.5 && params.x > 0.0) {
            if (frame.fog_color.a > 0.5) {
                let tint = frame.fog_color.rgb / max(luma3(frame.fog_color.rgb), 1.0e-3);
                // The haze in front of the impostor is under the cloud layer too (ADR-5944).
                let haze_sun = shared_haze_sun(world);
                if (haze_sun < 1.0) {
                    rgb = apply_aerial_under_clouds(
                        rgb,
                        world,
                        frame.camera_pos_time.xyz,
                        params,
                        tint,
                        sun_dir,
                        sun_radiance,
                        max(frame.output_mode.z, 1.0),
                        haze_sun,
                    );
                } else {
                    rgb = apply_aerial(
                        rgb,
                        world,
                        frame.camera_pos_time.xyz,
                        params,
                        tint,
                        sun_dir,
                        sun_radiance,
                        max(frame.output_mode.z, 1.0),
                    );
                }
            } else {
                let distance = length(world - frame.camera_pos_time.xyz);
                let height_density = params.x * exp(-params.y * (world.y - params.z));
                let t = 1.0 - exp(-max(distance, 0.0) * max(height_density, 0.0));
                let inscatter = frame.fog_color.rgb * max(frame.output_mode.z, 1.0);
                rgb = mix(rgb, inscatter, clamp(t, 0.0, 1.0));
            }
        }
        let hdr_scale = select(1.0, frame.fill_color.w, frame.fill_color.w > 0.0);
        return vec4<f32>(rgb * hdr_scale, ssr_roughness(s.roughness, max(dot(n, view_dir), 0.0)));
    }
    // LDR shared target: the terrain's own display transform.
    lit = clamp(apply_height_fog(lit, world), vec3<f32>(0.0), vec3<f32>(1.0e4));
    let display = default_display_transform(lit);
    if (frame.output_mode.y > 0.5) {
        return vec4<f32>(display, 1.0);
    }
    return vec4<f32>(display_to_linear(display), 1.0);
}

@fragment
fn fs_impostor_color(in: ImpOut) -> ImpColorOut {
    let s = imp_surface(in);
    if (s.alpha < 0.5 || imp_dither_discard(in.band, in.position.xy)) {
        discard;
    }
    return ImpColorOut(imp_shade(s, in.position.xy), imp_depth(s.world));
}

struct ImpVelOut {
    @location(0) motion: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

@fragment
fn fs_impostor_velocity(in: ImpOut) -> ImpVelOut {
    let s = imp_surface(in);
    if (s.alpha < 0.5 || imp_dither_discard(in.band, in.position.xy)) {
        discard;
    }
    let current_uv = clip_uv(frame.unjittered_view_proj * vec4<f32>(s.world, 1.0));
    let prev_uv = clip_uv(frame.prev_view_proj * vec4<f32>(s.world, 1.0));
    return ImpVelOut(vec4<f32>(prev_uv - current_uv, 0.0, 1.0), imp_depth(s.world));
}

struct ImpReceiverOut {
    @location(0) receiver: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

// Decal receiver: the static-mesh layer (1, like the mesh rock it replaces) and the normal.
@fragment
fn fs_impostor_receiver(in: ImpOut) -> ImpReceiverOut {
    let s = imp_surface(in);
    if (s.alpha < 0.5 || imp_dither_discard(in.band, in.position.xy)) {
        discard;
    }
    return ImpReceiverOut(vec4<f32>(1.0 / 255.0, s.normal * 0.5 + vec3<f32>(0.5)), imp_depth(s.world));
}
