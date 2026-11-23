// SPDX-License-Identifier: MPL-2.0
// Screen-space reflection plus box-projected probe specular (M15b).
// McGuire and Mara, JCGT 2014, simplified to a linear screen march (no Hi-Z).
// Lagarde, SIGGRAPH 2012, for the probe parallax correction.

struct SsrProbe {
    center: vec4<f32>,
    // xyz = half extent, w = blend weight
    half: vec4<f32>,
}

struct SsrParams {
    inv_view_proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    camera: vec4<f32>,
    // x = width, y = height, z = thickness, w = step length
    screen: vec4<f32>,
    // x = probe count, y = LDR-to-scene scale, z = SSR enabled, w = floor normal.y
    misc: vec4<f32>,
    probes: array<SsrProbe, 4>,
    // The local reflection captures' boxes (ADR-2860): center.w = blend weight (0 = none),
    // half.w = the fade distance outside the box.
    captures: array<SsrProbe, 4>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: SsrParams;
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var depth_tex: texture_depth_2d;
@group(0) @binding(3) var probe_tex: texture_2d_array<f32>;
@group(0) @binding(4) var probe_samp: sampler;

fn reconstruct(pix: vec2<i32>) -> vec3<f32> {
    let w = max(params.screen.x, 1.0);
    let h = max(params.screen.y, 1.0);
    let uv = (vec2<f32>(pix) + vec2<f32>(0.5)) / vec2<f32>(w, h);
    let depth = textureLoad(depth_tex, pix, 0);
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, depth);
    let world = params.inv_view_proj * vec4<f32>(ndc, 1.0);
    return world.xyz / select(world.w, 1e-6, abs(world.w) < 1e-6);
}

fn equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / 3.14159265) + 0.5, theta / 3.14159265);
}

fn box_lookup(origin: vec3<f32>, dir: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>) -> vec3<f32> {
    let rd = normalize(dir + vec3<f32>(1e-6));
    let rbmax = (bmax - origin) / rd;
    let rbmin = (bmin - origin) / rd;
    let rx = select(rbmin.x, rbmax.x, rd.x > 0.0);
    let ry = select(rbmin.y, rbmax.y, rd.y > 0.0);
    let rz = select(rbmin.z, rbmax.z, rd.z > 0.0);
    var fa = min(min(rx, ry), rz);
    if (fa < 0.0) {
        fa = 0.0;
    }
    let hit = origin + rd * fa;
    return hit - (bmin + bmax) * 0.5;
}

fn sample_probes(world: vec3<f32>, refl: vec3<f32>) -> vec3<f32> {
    let count = u32(params.misc.x + 0.5);
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    let n = min(count, 4u);
    for (var i: u32 = 0u; i < n; i = i + 1u) {
        let weight = params.probes[i].half.w;
        if (weight <= 0.0) {
            continue;
        }
        let center = params.probes[i].center.xyz;
        let half = max(params.probes[i].half.xyz, vec3<f32>(0.05));
        let lookup = box_lookup(world, refl, center - half, center + half);
        let uv = equirect_uv(lookup);
        acc += textureSampleLevel(probe_tex, probe_samp, uv, i32(i), 0.0).rgb * weight;
        wsum += weight;
    }
    if (wsum <= 1e-5) {
        return vec3<f32>(0.0);
    }
    return acc / wsum * params.misc.y;
}

// The share of `world` the local reflection captures cover (ADR-2860), weighed as the forward
// pass weighs them; there the forward pass already reflects them in HDR. 0 without captures.
// Depth reconstruction tolerance on a capture box's faces, metres (ADR-2860).
const CAPTURE_FACE_MARGIN_M: f32 = 0.02;

fn capture_cover(world: vec3<f32>) -> f32 {
    var cover = 0.0;
    for (var i: u32 = 0u; i < 4u; i = i + 1u) {
        let weight = params.captures[i].center.w;
        if (weight <= 0.0) {
            continue;
        }
        // `world` comes from the depth buffer: on a box face (a room's box fitted to its walls) it
        // scatters a few millimetres either side, so the face's margin counts as inside.
        let outside = max(length(max(abs(world - params.captures[i].center.xyz) - params.captures[i].half.xyz, vec3<f32>(0.0))) - CAPTURE_FACE_MARGIN_M, 0.0);
        cover += weight * (1.0 - smoothstep(0.0, max(params.captures[i].half.w, 1e-3), outside));
    }
    return clamp(cover, 0.0, 1.0);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    // Corners (-1,-1), (3,-1), (-1,3), without a function-local array or struct.
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let w = i32(params.screen.x);
    let h = i32(params.screen.y);
    let pix = vec2<i32>(i32(frag.x), i32(frag.y));
    let base = textureLoad(hdr_tex, pix, 0);
    // `base.a` is perceptual roughness from the forward pass. Same thresholds
    // as `ssr_roughness_weight`: the 0.35 golden floor stays a mirror, and a
    // surface at 0.7 or rougher (outdoor ground, sky dome) keeps its colour.
    let fade = clamp((0.7 - clamp(base.a, 0.0, 1.0)) / 0.35, 0.0, 1.0);
    if (fade <= 0.0 || pix.x >= w - 1 || pix.y >= h - 1) {
        return base;
    }
    // A depth crease is a silhouette, not a surface. Shading across it (McGuire
    // & Mara) reconstructs a normal that points at the floor and mixes a dark
    // probe into the edge. A flat floor moves about 0.001 in NDC per pixel.
    let depth = textureLoad(depth_tex, pix, 0);
    let dpx = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(1, 0), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dmx = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(-1, 0), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dpy = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(0, 1), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dmy = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(0, -1), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    if (dpx > 0.05 || dmx > 0.05 || dpy > 0.05 || dmy > 0.05) {
        return base;
    }
    let world = reconstruct(pix);
    let dx = reconstruct(pix + vec2<i32>(1, 0)) - world;
    let dy = reconstruct(pix + vec2<i32>(0, 1)) - world;
    var n = cross(dx, dy);
    let nlen = length(n);
    if (nlen < 1e-5) {
        return base;
    }
    n = n / nlen;
    let view_dir = normalize(params.camera.xyz - world);
    if (dot(n, view_dir) < 0.0) {
        n = -n;
    }
    if (n.y < params.misc.w) {
        return base;
    }
    let refl = reflect(-view_dir, n);
    let probe = sample_probes(world, refl);
    // The march only finds the hit's pixel; its colour is read once, after the loop (ADR-5713).
    // A colour fetch inside the loop, on the path that breaks out of it, was not repeatable on
    // Mesa RADV: the same held frame returned two different images.
    var hit = false;
    var hit_pix = pix;
    if (params.misc.z > 0.5) {
        let step_len = max(params.screen.w, 0.02);
        let thickness = params.screen.z;
        for (var s: i32 = 2; s < 18; s = s + 1) {
            let p = world + refl * (f32(s) * step_len);
            let clip = params.view_proj * vec4<f32>(p, 1.0);
            if (abs(clip.w) < 1e-4) {
                continue;
            }
            let ndc = clip.xyz / clip.w;
            let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
            if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
                break;
            }
            let sp = vec2<i32>(i32(uv.x * params.screen.x), i32(uv.y * params.screen.y));
            let scene_z = textureLoad(depth_tex, sp, 0);
            if (ndc.z > scene_z && ndc.z < scene_z + thickness) {
                hit = true;
                hit_pix = sp;
                break;
            }
        }
    }
    var hit_color = base.rgb;
    if (hit) {
        hit_color = textureLoad(hdr_tex, hit_pix, 0).rgb;
    }
    var refl_color = base.rgb;
    var apply = false;
    if (params.misc.z > 0.5 && hit) {
        refl_color = hit_color;
        apply = true;
    } else if (params.misc.x > 0.5) {
        refl_color = probe;
        // A miss inside a local capture keeps the forward pass's HDR capture (ADR-2860).
        let cover = capture_cover(world);
        if (cover >= 1.0) {
            // Fully covered: the forward pass's pixel as it is (`mix(base, base, k)` is not
            // always `base` to the last bit).
            return base;
        }
        if (cover > 0.0) {
            refl_color = mix(probe, base.rgb, cover);
        }
        apply = true;
    }
    if (!apply) {
        return base;
    }
    return vec4<f32>(mix(base.rgb, refl_color, 0.72 * fade), base.a);
}
