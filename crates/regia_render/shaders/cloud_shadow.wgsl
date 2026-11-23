// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::cloud_shadow

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
