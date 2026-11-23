// SPDX-License-Identifier: MPL-2.0
// Screen-space subsurface scattering: the two blur passes (`crate::subsurface`, ADR-2049).
//
// `fs_horizontal` blurs the scattering light (`frame/subsurface_gpu.rs` renders it with the
// `SUBSURFACE` forward variant) along x into a pooled target. `fs_vertical` blurs that along y,
// puts the albedo back and adds the result to the colour target.
//
// The kernel is the marginal of the profile's radial diffusion, one row per profile: 17 taps of
// (RGB weight, offset in units of the reach). A straight shadow edge along an axis sees the radial
// profile's exact response (`crate::subsurface` tests). A tap reads between pixels, so a narrow lobe
// stays narrow on the pixel grid, and wanders within the span it stands for by a per-pixel noise
// (interleaved gradient noise, Jimenez 2014, moving per frame under TAA), so a hard edge does not turn
// into seventeen steps. A tap that lands off the scattering surface or across a depth step
// takes the center's light instead, as a separable blur that follows the surface must (Jimenez,
// Sundstedt & Gutierrez 2009), so the energy stays and nothing bleeds over silhouettes.
//
// The tangent-plane kernel (`size.w` > 0, `crate::subsurface::SubsurfaceKernel::Tangent`, ADR-3170)
// lays the kernel on the surface instead of the screen (Mikkelsen 2018): each pass measures how much
// surface one pixel spans along its axis from the depth of its neighbors on the scattering surface
// (`surface_step`, mirrored by `crate::subsurface::tangent_step`), so a surface seen at a grazing
// angle blurs over as much of itself as a facing one, and a tap's depth test measures its distance
// from that tangent plane. With `size.w` 0 the code below is the screen kernel, unchanged.

const TAPS: u32 = 17u;
const PROFILES: u32 = 2u;

struct ScatterParams {
    // [profile - 1][tap]: xyz = weight per channel, w = offset in units of the reach.
    taps: array<vec4<f32>, 34>,
    // x, y = the depth linearization: view distance = |y / (ndc depth + x)|; z = pixels per world
    // unit at distance 1; w = 1 on an orthographic view (the distance term is 1).
    view: vec4<f32>,
    // xy = target size in pixels, z = the noise's frame index (0 without TAA), w = the kernel: 0
    // the screen kernel, else the tangent-plane kernel with the internal view's width in pixels.
    size: vec4<f32>,
}

@group(0) @binding(0) var light_tex: texture_2d<f32>;
@group(0) @binding(1) var albedo_tex: texture_2d<f32>;
@group(0) @binding(2) var depth_tex: texture_depth_2d;
@group(0) @binding(3) var<uniform> params: ScatterParams;
@group(0) @binding(4) var light_samp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn view_distance(px: vec2<i32>) -> f32 {
    if (params.view.w > 0.5) {
        return 1.0;
    }
    let depth = textureLoad(depth_tex, px, 0);
    return abs(params.view.y / min(depth + params.view.x, -1e-6));
}

// Interleaved gradient noise at a pixel, 0..1.
fn gradient_noise(px: vec2<i32>) -> f32 {
    let p = vec2<f32>(px) + vec2<f32>(5.588238 * params.size.z);
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// The view depth along the axis at `px`, on a perspective or an orthographic view (the tangent
// kernel's depth; `view_distance` is 1 on an orthographic view).
fn axis_depth(px: vec2<i32>) -> f32 {
    if (params.view.w > 0.5) {
        let depth = textureLoad(depth_tex, px, 0);
        return abs((depth - params.view.y) / params.view.x);
    }
    return view_distance(px);
}

// The view depth's change per pixel along `dir` at `px` on the scattering surface: the smaller
// one-sided difference toward a neighbor on it, 0 without one, so a silhouette does not tilt it.
fn depth_slope(px: vec2<i32>, dir: vec2<i32>, center: f32) -> f32 {
    let last = vec2<i32>(params.size.xy) - vec2<i32>(1);
    let ahead = clamp(px + dir, vec2<i32>(0), last);
    let behind = clamp(px - dir, vec2<i32>(0), last);
    var slope = 0.0;
    var found = false;
    if (any(ahead != px) && textureLoad(albedo_tex, ahead, 0).a != 0.0) {
        slope = axis_depth(ahead) - center;
        found = true;
    }
    if (any(behind != px) && textureLoad(albedo_tex, behind, 0).a != 0.0) {
        let back = center - axis_depth(behind);
        if (!found || abs(back) < abs(slope)) {
            slope = back;
        }
    }
    return slope;
}

// World length of the surface one pixel spans along `dir` (`crate::subsurface::tangent_step`).
fn surface_step(px: vec2<i32>, dir: vec2<i32>, dist: f32, slope: f32) -> f32 {
    var lateral = 1.0 / params.view.z;
    var ray = 0.0;
    if (params.view.w <= 0.5) {
        lateral = dist / params.view.z;
        // The ray's slope along the axis: the pixel's offset from the principal point (the internal
        // view's center) over the focal length in pixels.
        let half = vec2<f32>(params.size.w, params.size.w * params.size.y / max(params.size.x, 1.0)) * 0.5;
        ray = dot((vec2<f32>(px) + vec2<f32>(0.5) - half) / params.view.z, vec2<f32>(dir));
    }
    let along = lateral + ray * slope;
    return sqrt(along * along + slope * slope);
}

// The blur along `dir`. `center` is the light at `px`, `reach` its reach in world units.
fn scatter(px: vec2<i32>, dir: vec2<f32>, center: vec3<f32>, reach: f32, profile: u32) -> vec3<f32> {
    let dist = view_distance(px);
    var reach_px = reach * params.view.z / max(dist, 1e-6);
    // The tangent-plane kernel: the reach in pixels along this axis, and the plane's depth.
    let tangent = params.size.w > 0.0;
    var plane_depth = 0.0;
    var plane_slope = 0.0;
    if (tangent) {
        plane_depth = axis_depth(px);
        plane_slope = depth_slope(px, vec2<i32>(dir), plane_depth);
        reach_px = reach / max(surface_step(px, vec2<i32>(dir), dist, plane_slope), 1e-9);
    }
    let last = vec2<i32>(params.size.xy) - vec2<i32>(1);
    let row = min(profile - 1u, PROFILES - 1u) * TAPS;
    let wander = gradient_noise(px) - 0.5;
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < TAPS; i = i + 1u) {
        let tap = params.taps[row + i];
        var light = center;
        // A tap at offset t|t| stands for a span of about 2|t| / 8 (`kernel_taps`).
        let span = sqrt(abs(tap.w)) * 0.25;
        let offset = dir * (tap.w + wander * span) * reach_px;
        let q = clamp(px + vec2<i32>(round(offset)), vec2<i32>(0), last);
        if (dot(offset, offset) > 1e-4 && textureLoad(albedo_tex, q, 0).a != 0.0) {
            // Bilinear over the scattering pixels only: off them the target holds zero, and its
            // alpha is the share of the read that was on them, whatever their reach.
            let uv = (vec2<f32>(px) + vec2<f32>(0.5) + offset) / params.size.xy;
            let read = textureSampleLevel(light_tex, light_samp, uv, 0.0);
            if (read.a > 1e-3) {
                // A depth step of half the reach is one sigma of the falloff.
                var step_over = (view_distance(q) - dist) / (0.5 * reach);
                if (tangent) {
                    // Measured from the tangent plane at the tap's pixel.
                    let along = dot(vec2<f32>(q - px), dir);
                    step_over = (axis_depth(q) - (plane_depth + plane_slope * along)) / (0.5 * reach);
                }
                let keep = exp(-0.5 * step_over * step_over);
                light = mix(center, read.rgb / read.a, keep);
            }
        }
        sum += tap.xyz * light;
    }
    return sum;
}

// The profile of a nonzero alpha of the albedo target: `shading.z` 1 is positive, 2 negative.
fn profile_of(word: f32) -> u32 {
    return select(1u, 2u, word < 0.0);
}

@fragment
fn fs_horizontal(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(position.xy);
    let word = textureLoad(albedo_tex, px, 0).a;
    if (word == 0.0) {
        discard;
    }
    let center = textureLoad(light_tex, px, 0).rgb;
    return vec4<f32>(scatter(px, vec2<f32>(1.0, 0.0), center, abs(word), profile_of(word)), 1.0);
}

@fragment
fn fs_vertical(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(position.xy);
    let albedo = textureLoad(albedo_tex, px, 0);
    if (albedo.a == 0.0) {
        discard;
    }
    let center = textureLoad(light_tex, px, 0).rgb;
    let light = scatter(px, vec2<f32>(0.0, 1.0), center, abs(albedo.a), profile_of(albedo.a));
    return vec4<f32>(light * albedo.rgb, 0.0);
}
