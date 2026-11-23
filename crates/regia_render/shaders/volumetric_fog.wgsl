// SPDX-License-Identifier: MPL-2.0
#import regia::utils
#import regia::cloud_shadow
#import regia::mist

// Froxel fog. Scatter is one texel per froxel; integration walks the slices
// front to back and blends the previous frame. Wronski, SIGGRAPH 2014.
// Fog coupling (ADR-320): the sun in-scatter is shadowed by the cloud transmittance map, so
// sunlit air shows god rays through gaps in the layer (Bauer, RDR2, SIGGRAPH 2019).

struct FogGpu {
    inv_view_proj: mat4x4<f32>,
    light_vp: mat4x4<f32>,
    prev_view_proj: mat4x4<f32>,
    camera: vec4<f32>,
    prev_camera: vec4<f32>,
    sun_travel: vec4<f32>,
    sun_color: vec4<f32>,
    grid: vec4<f32>,
    media: vec4<f32>,
    albedo: vec4<f32>,
    misc: vec4<f32>,
    screen: vec4<f32>,
    volumes: array<vec4<f32>, 8>,
    // Four local lights, three rows each: position (xyz), radius (w); colour (rgb), candela (w);
    // cone (xyz = -direction * s, w = -cos_outer * s, s = 1 / (cos_inner - cos_outer); a point
    // light is (0, 0, 0, 1)). FogCone::gpu in volumetrics.rs.
    lights: array<vec4<f32>, 12>,
    // Sky radiance in-scattered isotropically (rgb, cd/m²): shadowed fog still sees the sky.
    ambient: vec4<f32>,
    // Up to four interior boxes: centre (xyz, w = 1 used), half extents (xyz).
    interiors: array<vec4<f32>, 8>,
    // Cloud transmittance map: extent (m), layer base y, layer top y, 1 when the fog reads it
    // (`regia::cloud_shadow`). w = 0 leaves the in-scatter as before.
    cloud: vec4<f32>,
    // Per light slot, six rows: its spot tile's view-projection (four columns), the tile's atlas
    // rect (scale.xy, offset.zw) and bias (x = texel size per unit view depth, y = 1/near - 1/far,
    // z = perspective, w = 1 when shadowed). FogLightShadow in outdoor.rs.
    light_shadow: array<vec4<f32>, 24>,
    // Valley mist rows (`regia::mist`, `ValleyMist::gpu_rows`); row 2's w = 0 leaves it off.
    valley: array<vec4<f32>, 4>,
}

@group(0) @binding(0) var<uniform> u: FogGpu;
@group(0) @binding(1) var shadow_map: texture_depth_2d;
@group(0) @binding(2) var scatter_tex: texture_2d<f32>;
@group(0) @binding(3) var integrated_tex: texture_2d<f32>;
@group(0) @binding(4) var history_tex: texture_2d<f32>;
@group(0) @binding(5) var scene_depth: texture_depth_2d;
// The cloud transmittance map (R), or a 1×1 stand-in when `u.cloud.w` is 0.
@group(0) @binding(6) var cloud_map: texture_2d<f32>;
// The local-light shadow atlas (fog spot shafts); a 1x1 dummy when no fog light is shadowed.
@group(0) @binding(7) var local_shadow_map: texture_depth_2d;
// The valley mist's terrain grid (height, concavity), or a 1x1 stand-in (`MistHeightGrid`).
@group(0) @binding(8) var mist_grid: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
}

// Full-screen triangle. No function-local struct or array `var` (ADR-026).
@vertex
fn vs_fullscreen(@builtin(vertex_index) id: u32) -> VsOut {
    let x = select(-1.0, 3.0, id == 1u);
    let y = select(-1.0, 3.0, id == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let gg = clamp(g, -0.99, 0.99);
    let g2 = gg * gg;
    let denom = pow(max(1.0 + g2 - 2.0 * gg * cos_theta, 1e-4), 1.5);
    return (1.0 - g2) / (12.5663706144 * denom);
}

fn volume_density(p: vec3<f32>) -> f32 {
    var extra = 0.0;
    for (var i = 0; i < 4; i++) {
        let head = u.volumes[i * 2];
        let tail = u.volumes[i * 2 + 1];
        if (head.w < 0.0) {
            continue;
        }
        let d = p - head.xyz;
        if (head.w < 0.5) {
            if (abs(d.x) <= tail.x && abs(d.y) <= tail.y && abs(d.z) <= tail.z) {
                extra += tail.w;
            }
        } else if (dot(d, d) <= tail.x * tail.x) {
            extra += tail.w;
        }
    }
    return extra;
}

fn extinction_at(p: vec3<f32>) -> f32 {
    let height = u.media.x * exp(-u.media.y * (p.y - u.media.z));
    return max(height, 0.0) + volume_density(p) + mist_extinction(p);
}

fn mist_extinction(p: vec3<f32>) -> f32 {
    return regia::mist::mist_density(mist_grid, u.valley[0], u.valley[1], u.valley[2], u.valley[3], p);
}

// Metres an interior box grows by for `sky_open`: a froxel sample clamped onto a wall that lies on
// the box's face is inside (`INTERIOR_FACE_MARGIN_M` in lab_forward.wgsl, same value).
const INTERIOR_FACE_MARGIN_M: f32 = 0.02;

// The sky does not light air inside an interior volume (a roof is in the way).
fn sky_open(p: vec3<f32>) -> f32 {
    for (var i = 0; i < 4; i++) {
        let head = u.interiors[i * 2];
        let half = u.interiors[i * 2 + 1].xyz;
        if (head.w > 0.5 && all(abs(p - head.xyz) <= half + vec3<f32>(INTERIOR_FACE_MARGIN_M))) {
            return 0.0;
        }
    }
    return 1.0;
}

// Path length the layer integrals below run to (metres). Mirrors
// `FOG_LAYER_RANGE_M` in volumetrics.rs.
const FOG_LAYER_RANGE_M: f32 = 20000.0;
// Diffusivity factor: a cosine-weighted hemisphere of paths through a slab sees
// about 1.66 times the vertical optical depth (Elsasser 1942).
const DIFFUSIVITY: f32 = 1.66;

// Optical depth of the exponential height fog from `p` along the unit direction
// `d`: rho(p) * (1 - exp(-k d.y L)) / (k d.y), the closed form for a horizontally
// uniform layer. A low sun crosses the whole layer; a high one only what is above.
fn layer_optical_depth(p: vec3<f32>, d: vec3<f32>) -> f32 {
    let rho = max(u.media.x * exp(-u.media.y * (p.y - u.media.z)), 0.0);
    let a = u.media.y * d.y;
    if (abs(a) * FOG_LAYER_RANGE_M < 1e-4) {
        return rho * FOG_LAYER_RANGE_M;
    }
    let x = clamp(a * FOG_LAYER_RANGE_M, -30.0, 30.0);
    return rho * (1.0 - exp(-x)) / a;
}

// Distance (metres) from the eye to the opaque surface under screen `uv`, or the
// fog far plane where the depth is clear (sky) or scatter clamping is off.
fn surface_distance(uv: vec2<f32>) -> f32 {
    let far = max(u.prev_camera.w, 1.0);
    if (u.misc.w < 0.5) {
        return far;
    }
    let dims = vec2<i32>(textureDimensions(scene_depth));
    let texel = clamp(vec2<i32>(uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    let depth = textureLoad(scene_depth, texel, 0);
    if (depth >= 1.0) {
        return far;
    }
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
    return length(h.xyz / h.w - u.camera.xyz);
}

fn shadow_at(world: vec3<f32>) -> f32 {
    if (u.misc.x < 0.5) {
        return 1.0;
    }
    let clip = u.light_vp * vec4<f32>(world, 1.0);
    let ndc = clip.xyz / max(clip.w, 1e-5);
    let suv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
    if (suv.x < 0.0 || suv.x > 1.0 || suv.y < 0.0 || suv.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
        return 1.0;
    }
    let size = max(u.misc.y, 1.0);
    let texel = vec2<i32>(suv * size);
    let stored = textureLoad(shadow_map, texel, 0);
    // Legacy (`u.grid.w` 0): the cascade may keep a placement for many frames (cascade
    // anchoring), so its edge is not centred on the view: fade the shadow out over the outer
    // 15 % of the square so a re-placement moves a soft edge, not a hard one. Calibrated (1,
    // ADR-3122): the compare decides inside the whole square; shadowed air near the edge stays
    // shadowed. `u.sun_color.w` is the rule's depth bias (`FogSunCascade::bias_ndc`).
    let edge = select(smoothstep(0.85, 1.0, max(abs(ndc.x), abs(ndc.y))), 0.0, u.grid.w > 0.5);
    if (ndc.z > stored + u.sun_color.w) {
        return edge;
    }
    return 1.0;
}

fn slice_center(s: i32) -> f32 {
    let count = max(u.grid.z, 1.0);
    let near = max(u.camera.w, 0.05);
    let far = max(u.prev_camera.w, near + 0.05);
    let t = (f32(s) + 0.5) / count;
    return near * pow(far / near, t);
}

fn slice_start(s: i32) -> f32 {
    let count = max(u.grid.z, 1.0);
    let near = max(u.camera.w, 0.05);
    let far = max(u.prev_camera.w, near + 0.05);
    return near * pow(far / near, f32(s) / count);
}

fn slice_length(s: i32) -> f32 {
    let count = max(u.grid.z, 1.0);
    let near = max(u.camera.w, 0.05);
    let far = max(u.prev_camera.w, near + 0.05);
    let a = near * pow(far / near, f32(s) / count);
    let b = near * pow(far / near, f32(s + 1) / count);
    return max(b - a, 0.01);
}

fn ray_at(x: i32, y: i32) -> vec3<f32> {
    return ray_at_offset(x, y, vec2<f32>(0.5));
}

// View ray through froxel column (x, y) at sub-cell position `sub` (0..1 on each axis).
fn ray_at_offset(x: i32, y: i32, sub: vec2<f32>) -> vec3<f32> {
    let gx = max(u.grid.x, 1.0);
    let gy = max(u.grid.y, 1.0);
    let uu = (f32(x) + sub.x) / gx;
    let vv = (f32(y) + sub.y) / gy;
    let ndc = vec2<f32>(uu * 2.0 - 1.0, 1.0 - vv * 2.0);
    let far_h = u.inv_view_proj * vec4<f32>(ndc, 1.0, 1.0);
    let near_h = u.inv_view_proj * vec4<f32>(ndc, 0.0, 1.0);
    return normalize(far_h.xyz / far_h.w - near_h.xyz / near_h.w);
}

// One tap of light slot i's spot tile in the local atlas: 0 when a caster is between the lamp
// and p, 1 when lit, unshadowed or outside the tile (ADR-1365 phase 2). The froxel's four
// sub-rays and the history blend filter the single tap.
fn local_shadow_at(i: i32, p: vec3<f32>) -> f32 {
    let base = i * 6;
    let bias = u.light_shadow[base + 5];
    if (bias.w < 0.5) {
        return 1.0;
    }
    let view_proj = mat4x4<f32>(
        u.light_shadow[base],
        u.light_shadow[base + 1],
        u.light_shadow[base + 2],
        u.light_shadow[base + 3],
    );
    let clip = view_proj * vec4<f32>(p, 1.0);
    if (clip.w <= 1e-6) {
        return 1.0;
    }
    let ndc = clip.xyz / clip.w;
    // NDC +y is the tile's top row (shadow_uv_from_clip in shadows.wgsl).
    let local = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
        return 1.0;
    }
    let rect = u.light_shadow[base + 4];
    let dims = vec2<f32>(textureDimensions(local_shadow_map, 0));
    let lo = rect.zw * dims;
    let hi = max((rect.zw + rect.xy) * dims - vec2<f32>(1.0), lo);
    let texel = vec2<i32>(clamp((local * rect.xy + rect.zw) * dims, lo, hi));
    let stored = textureLoad(local_shadow_map, texel, 0);
    // Three texels of slope bias in depth: the texel's world size grows with view depth, and
    // dz/dw = 1 / (w^2 (1/near - 1/far)) on a perspective tile.
    let texel_world = bias.x * max(clip.w, 1e-6);
    let dz_dw = 1.0 / max(clip.w * clip.w * bias.y, 1e-9);
    let compare = ndc.z - 3.0 * texel_world * dz_dw;
    return select(0.0, 1.0, compare <= stored);
}

fn inscatter_at(p: vec3<f32>, view_dir: vec3<f32>) -> vec4<f32> {
    let ext = extinction_at(p);
    // Only clear air skips the lighting. A 1e-5 /m cut dropped Mars haze (1.2e-5 /m at its base,
    // thinning upward) to nothing a hundred metres above the base: the mesa tops and the air
    // over them had no haze at all.
    if (ext <= 0.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    let to_sun = -u.sun_travel.xyz;
    let phase = henyey_greenstein(clamp(dot(view_dir, to_sun), -1.0, 1.0), u.media.w);
    // The fog layer extinguishes its own sunlight: a sun a few degrees up crosses
    // kilometres of ground haze before it reaches a valley froxel.
    var sun_t = exp(-layer_optical_depth(p, to_sun));
    // Terrain-hugging mist between the sample and a low sun dims the bank it lights.
    sun_t *= regia::mist::mist_sun_transmittance(mist_grid, u.valley[0], u.valley[1], u.valley[2], u.valley[3], p, to_sun);
    var light = u.sun_color.rgb * u.sun_travel.w * phase * shadow_at(p) * sun_t;
    if (u.cloud.w > 0.5) {
        light *= regia::cloud_shadow::cloud_sun_transmittance(cloud_map, u.cloud, p, to_sun, u.camera.xyz);
    }
    for (var i = 0; i < 4; i++) {
        let lamp = u.lights[i * 3];
        let col = u.lights[i * 3 + 1];
        let cone_row = u.lights[i * 3 + 2];
        if (lamp.w <= 0.0) {
            continue;
        }
        let offset = lamp.xyz - p;
        let dist = max(length(offset), 1e-3);
        // The forward pass's windowed inverse square (distance_attenuation in pbr_lighting.wgsl):
        // the +1 keeps a lamp at the eye (a headlight) from piling its light into the first slices.
        let window = clamp(1.0 - pow(dist / lamp.w, 4.0), 0.0, 1.0);
        let falloff = window * window / (dist * dist + 1.0);
        let to_light = offset / dist;
        // Spot cone: the forward pass's linear ramp between the outer and inner cosines
        // (spot_attenuation), folded into one dot product; 1 for a point light.
        let cone = clamp(dot(cone_row.xyz, to_light) + cone_row.w, 0.0, 1.0);
        if (falloff * cone <= 0.0) {
            continue;
        }
        let phase_l = henyey_greenstein(clamp(dot(view_dir, to_light), -1.0, 1.0), u.media.w);
        light += col.rgb * col.w * falloff * cone * phase_l * local_shadow_at(i, p);
    }
    // Skylight: isotropic phase over the whole sphere of incident sky radiance,
    // dimmed by the fog column above the sample.
    let sky_t = exp(-DIFFUSIVITY * layer_optical_depth(p, vec3<f32>(0.0, 1.0, 0.0)));
    light += u.ambient.rgb * (sky_t * sky_open(p));
    return vec4<f32>(ext * u.albedo.rgb * light, ext);
}

// Distance along sub-ray `i` the scatter pass samples (`fog_sample_depth` in fog.rs, ADR-3122).
// Legacy (`u.grid.w` 0): the sample at `mid + len (fract(0.375 + 0.618034 i) - 0.5) / 2`, clamped
// to `surface`, the farthest surface the froxel's sub-rays meet; samples beyond it pile onto it.
// Calibrated (1): `surface` is this sub-ray's own surface, and when it lies inside the slice the
// sample's place in the slice is scaled onto the air in front of it, `[start, surface]` (a sub-ray
// whose surface is in front of the slice sees no air there: `fs_scatter` skips it).
fn sample_depth(start: f32, mid: f32, len: f32, i: i32, surface: f32) -> f32 {
    let depth = mid + len * (fract(0.375 + 0.618034 * f32(i)) - 0.5) * 0.5;
    if (u.grid.w < 0.5 || surface >= start + len || surface <= start) {
        return min(depth, surface);
    }
    return start + (depth - start) * ((surface - start) / max(len, 1e-6));
}

// Rotated-grid sub-ray offsets inside a froxel column (no local arrays, ADR-026).
fn sub_offset(i: i32) -> vec2<f32> {
    let fi = f32(i);
    return vec2<f32>(fract(0.125 + 0.25 * fi + 0.5 * f32(i & 1)), 0.125 + 0.25 * fi);
}

@fragment
fn fs_scatter(in: VsOut) -> @location(0) vec4<f32> {
    let gx = i32(u.grid.x);
    let px = i32(in.pos.x);
    let py = i32(in.pos.y);
    let z = px / max(gx, 1);
    let x = px - z * gx;
    // Four rotated-grid taps inside the froxel: a shadow edge that crosses the cell
    // gives a partial value instead of a hard 0/1 step (supersampled froxels).
    let len = slice_length(z);
    let mid = slice_center(z);
    // A froxel behind every surface its sub-rays hit holds no visible air. Sample the
    // medium no deeper than the farthest of those surfaces: below the ground,
    // exponential height fog grows without bound, and a surface pixel reads part of
    // the slice it sits in. One sky sub-ray (or a far surface) leaves the froxel
    // unclamped, so sky pixels sharing it keep the air along their rays.
    // The calibrated rule (`u.grid.w`, ADR-3122) places each sub-ray's samples in front of its own
    // surface, so none lies on a roof's underside or behind a nearer sub-ray's wall.
    var surface = 0.0;
    var own = vec4<f32>(0.0);
    for (var i = 0; i < 4; i++) {
        let tap_uv = (vec2<f32>(f32(x), f32(py)) + sub_offset(i)) / max(u.grid.xy, vec2<f32>(1.0));
        own[i] = surface_distance(tap_uv);
        surface = max(surface, own[i]);
    }
    let calibrated = u.grid.w > 0.5;
    let start = slice_start(z);
    var acc = vec4<f32>(0.0);
    var seen = 0.0;
    for (var i = 0; i < 4; i++) {
        // Calibrated: each sub-ray weighs by the share of the slice it sees air in
        // (`fog_sample_weight`): one whose surface is in front of the slice sees none, and a thin
        // layer in front of a surface (within the compare's bias of a roof) does not stand for
        // the whole slice that pixels behind it read.
        let weight = select(1.0, clamp((own[i] - start) / max(len, 1e-6), 0.0, 1.0), calibrated);
        if (weight <= 0.0) {
            continue;
        }
        let dir = ray_at_offset(x, py, sub_offset(i));
        let depth = sample_depth(start, mid, len, i, select(surface, own[i], calibrated));
        acc += weight * inscatter_at(u.camera.xyz + dir * depth, dir);
        seen += weight;
    }
    if (!calibrated) {
        return acc * 0.25;
    }
    if (seen <= 0.0) {
        // Behind every sub-ray's surface: no visible air, no in-scatter (the extinction only
        // dims slices no pixel of this column reads).
        return vec4<f32>(0.0, 0.0, 0.0, extinction_at(u.camera.xyz + ray_at(x, py) * min(mid, surface)));
    }
    return acc / seen;
}

fn integrate_column(x: i32, y: i32, up_to: i32) -> vec4<f32> {
    var lit = vec3<f32>(0.0);
    var transmit = 1.0;
    let limit = min(up_to, 15);
    for (var s = 0; s < 16; s++) {
        if (s > limit) {
            break;
        }
        let texel = textureLoad(scatter_tex, vec2<i32>(x + s * i32(u.grid.x), y), 0);
        let ext = texel.a;
        let len = slice_length(s);
        let optical = ext * len;
        let step_t = exp(-optical);
        // (1 − e^(−σL)) / σ, the CPU mirror's `integrate_segment`. A floor of 1e-4 /m on σ here
        // (it only guards the branch `select` discards) scaled a thin haze's in-scatter by σ/1e-4:
        // Mars dust at 2e-5 /m lost 80 % of its light at every distance.
        let integ = select((1.0 - step_t) / max(ext, 1e-20), len, optical < 1e-4);
        lit += texel.rgb * integ * transmit;
        transmit *= step_t;
    }
    return vec4<f32>(lit, transmit);
}

// `v` with each NaN or ±∞ component replaced by `fallback`'s (exponent bits all ones; SPIR-V
// leaves `max`/`clamp`/`mix` of a NaN undefined, so they do not remove it).
fn finite_or4(v: vec4<f32>, fallback: vec4<f32>) -> vec4<f32> {
    // `(bits << 1) >= 0xff000000`: exponent all ones, a form no driver folds (ADR-3150, ADR-8366).
    let bad = (bitcast<vec4<u32>>(v) << vec4<u32>(1u)) >= vec4<u32>(0xff000000u);
    return select(v, fallback, bad);
}

fn history_blend(current: vec4<f32>, world: vec3<f32>) -> vec4<f32> {
    if (u.misc.z < 0.5) {
        return current;
    }
    let clip = u.prev_view_proj * vec4<f32>(world, 1.0);
    if (abs(clip.w) < 1e-4) {
        return current;
    }
    let ndc = clip.xyz / clip.w;
    if (ndc.x < -1.0 || ndc.x > 1.0 || ndc.y < -1.0 || ndc.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
        return current;
    }
    let uu = ndc.x * 0.5 + 0.5;
    let vv = 1.0 - (ndc.y * 0.5 + 0.5);
    let gx = u.grid.x;
    let gy = u.grid.y;
    let hx = i32(clamp(uu, 0.0, 0.999) * gx);
    let hy = i32(clamp(vv, 0.0, 0.999) * gy);
    let dist = length(world - u.prev_camera.xyz);
    let near = max(u.camera.w, 0.05);
    let far = max(u.prev_camera.w, near + 0.05);
    let slice = clamp(log(clamp(dist, near, far) / near) / log(far / near), 0.0, 0.999);
    let hs = i32(slice * u.grid.z);
    let past = textureLoad(history_tex, vec2<i32>(hx + hs * i32(gx), hy), 0);
    let w = clamp(u.albedo.w, 0.0, 0.95);
    return mix(current, past, w);
}

@fragment
fn fs_integrate(in: VsOut) -> @location(0) vec4<f32> {
    let gx = i32(u.grid.x);
    let px = i32(in.pos.x);
    let py = i32(in.pos.y);
    let z = px / max(gx, 1);
    let x = px - z * gx;
    // The output is next frame's history (ADR-869): a non-finite froxel is clear air
    // (in-scatter 0, transmittance 1), and a non-finite history texel is replaced by this
    // frame's value instead of being blended into every later frame.
    let current = finite_or4(integrate_column(x, py, z), vec4<f32>(0.0, 0.0, 0.0, 1.0));
    let dir = ray_at(x, py);
    let world = u.camera.xyz + dir * slice_center(z);
    return finite_or4(history_blend(current, world), current);
}

fn froxel_at(x: i32, y: i32, s: i32) -> vec4<f32> {
    let gx = i32(max(u.grid.x, 1.0));
    let gy = i32(max(u.grid.y, 1.0));
    let xi = clamp(x, 0, gx - 1);
    let yi = clamp(y, 0, gy - 1);
    return textureLoad(integrated_tex, vec2<i32>(xi + s * gx, yi), 0);
}

// Bilinear inside one depth slice. Neighbouring slices are different distances,
// so they are not mixed.
fn accumulated(uv: vec2<f32>, dist: f32) -> vec4<f32> {
    let gx = max(u.grid.x, 1.0);
    let gy = max(u.grid.y, 1.0);
    let slices = max(u.grid.z, 1.0);
    let fx = clamp(uv.x, 0.0, 0.999) * gx - 0.5;
    let fy = clamp(uv.y, 0.0, 0.999) * gy - 0.5;
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let tx = fract(fx);
    let ty = fract(fy);
    let near = max(u.camera.w, 0.05);
    let far = max(u.prev_camera.w, near + 0.05);
    let slice = clamp(log(clamp(dist, near, far) / near) / log(far / near), 0.0, 1.0);
    // Integrated slice s holds the fog from the eye to its far edge, so a surface
    // at the far edge of slice s reads slice s and one at its near edge reads
    // slice s - 1 (clear air before slice 0). Reading at the slice centre added
    // half a slice of fog behind the surface: hundreds of metres at range.
    let fs = slice * slices - 1.0;
    let clear = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    if (fs < 0.0) {
        let first = mix(
            mix(froxel_at(x0, y0, 0), froxel_at(x0 + 1, y0, 0), tx),
            mix(froxel_at(x0, y0 + 1, 0), froxel_at(x0 + 1, y0 + 1, 0), tx),
            ty,
        );
        return mix(clear, first, fs + 1.0);
    }
    let s0 = clamp(i32(floor(fs)), 0, i32(slices) - 1);
    let s1 = min(s0 + 1, i32(slices) - 1);
    let tz = clamp(fs - floor(fs), 0.0, 1.0);
    let near_slice = mix(
        mix(froxel_at(x0, y0, s0), froxel_at(x0 + 1, y0, s0), tx),
        mix(froxel_at(x0, y0 + 1, s0), froxel_at(x0 + 1, y0 + 1, s0), tx),
        ty,
    );
    let far_slice = mix(
        mix(froxel_at(x0, y0, s1), froxel_at(x0 + 1, y0, s1), tx),
        mix(froxel_at(x0, y0 + 1, s1), froxel_at(x0 + 1, y0 + 1, s1), tx),
        ty,
    );
    return mix(near_slice, far_slice, tz);
}

@fragment
fn fs_fog_blend(in: VsOut) -> @location(0) vec4<f32> {
    let dims = max(u.screen.xy, vec2<f32>(1.0));
    let uv = in.pos.xy / dims;
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
    let dist = length(h.xyz / h.w - u.camera.xyz);
    let acc = accumulated(uv, dist);
    // `screen.w`: the HDR target's pre-exposure (0 reads as 1).
    let scale = select(1.0, u.screen.w, u.screen.w > 0.0);
    return vec4<f32>(acc.rgb * scale, clamp(acc.a, 0.0, 1.0));
}

struct OccluderIn {
    @location(0) position: vec3<f32>,
}

@vertex
fn vs_occluder(in: OccluderIn) -> VsOut {
    return VsOut(u.light_vp * vec4<f32>(in.position, 1.0));
}

@fragment
fn fs_fog_ldr(in: VsOut) -> @location(0) vec4<f32> {
    let dims = max(u.screen.xy, vec2<f32>(1.0));
    let uv = in.pos.xy / dims;
    let far = max(u.prev_camera.w, 1.0);
    let acc = accumulated(uv, far);
    let mapped = regia::utils::tonemap_agx_vec(acc.rgb * u.screen.z);
    return vec4<f32>(regia::utils::encode_srgb(mapped), 1.0);
}
