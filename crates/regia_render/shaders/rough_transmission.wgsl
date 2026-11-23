// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::pbr::rough_transmission

// Rough transmission through a thin dielectric (ADR-2075), shared by the forward pass's
// refraction pass (`fs_refract` in `lab_forward.wgsl`) and the reference path tracer
// (`regia_pt`'s `pt.wgsl`). `regia_render::rough_transmission` mirrors every function here.
//
// Model. A thin sheet (frosted glass, a scuffed visor, ice) has two rough faces close together.
// The light it transmits keeps the smooth sheet's weight, `t · (1 − F) · base · volume`, and only
// its direction spreads: the straight-through direction becomes the GGX reflection lobe of the
// sheet's normal, mirrored through the sheet's plane (Burley 2015, "Extending the Disney BRDF to a
// BSDF with Integrated Subsurface Scattering", § thin-surface BSDF, which treats thin transmission
// as the reflection lobe flipped into the lower hemisphere and scales its roughness by
// `0.65 η − 0.35`). Half vectors are drawn with density D(h) (n·h) (Walter et al. 2007, eq. 35).
// A drawn direction that would leave on the viewer's side goes straight through instead, so the
// lobe never loses energy. Shadowing-masking is not modelled (the weight is the smooth sheet's).

// GGX alpha of the transmission lobe for a perceptual roughness `roughness` and the dielectric's
// index `ior`: `((0.65 ior − 0.35) · roughness)²`, at least 1e-4.
fn thin_alpha(roughness: f32, ior: f32) -> f32 {
    let scale = max(0.65 * max(ior, 1.0) - 0.35, 0.05);
    let r = clamp(roughness, 0.0, 1.0) * scale;
    return max(r * r, 1e-4);
}

// The transmitted direction for the uniform pair `u` (each in [0, 1)): `wo` points from the
// surface to the viewer, `n` is the sheet's normal on the viewer's side (`dot(n, wo) > 0`).
// `u` = (0, ·) is the straight-through direction `-wo`.
fn sample_direction(wo: vec3<f32>, n: vec3<f32>, alpha: f32, u: vec2<f32>) -> vec3<f32> {
    let a2 = alpha * alpha;
    let cos2 = (1.0 - u.x) / max(1.0 + (a2 - 1.0) * u.x, 1e-7);
    let cos_t = sqrt(clamp(cos2, 0.0, 1.0));
    let sin_t = sqrt(clamp(1.0 - cos2, 0.0, 1.0));
    let phi = 6.283185307 * u.y;
    // Tangent frame of n (Duff et al. 2017, "Building an Orthonormal Basis, Revisited").
    let s = select(-1.0, 1.0, n.z >= 0.0);
    let a = -1.0 / (s + n.z);
    let b = n.x * n.y * a;
    let tx = vec3<f32>(1.0 + s * n.x * n.x * a, s * b, -s * n.x);
    let ty = vec3<f32>(b, s + n.y * n.y * a, -n.y);
    let h = normalize(tx * (sin_t * cos(phi)) + ty * (sin_t * sin(phi)) + n * cos_t);
    let r = 2.0 * dot(wo, h) * h - wo;
    let t = r - 2.0 * dot(n, r) * n;
    if (dot(n, t) > -1e-4) {
        return -wo;
    }
    return normalize(t);
}

// The i-th of `count` stratified pairs of the forward pass's fixed tap pattern, rotated by the
// pixel's `rotation` (two numbers in [0, 1); a Cranley-Patterson rotation): a Fibonacci lattice
// (the golden-ratio rank-1 lattice) over the unit square.
fn tap(i: u32, count: u32, rotation: vec2<f32>) -> vec2<f32> {
    let k = f32(i);
    let base = vec2<f32>((k + 0.5) / f32(count), fract(k * 0.618033989));
    return fract(base + rotation);
}

// Number of taps the forward pass averages (`regia_render::rough_transmission::FORWARD_TAPS`).
const FORWARD_TAPS: u32 = 16u;

// The forward pass's rough background: the mean of `FORWARD_TAPS` lobe directions over the
// refraction copy `scene` (with its opaque `depth`). Each direction is followed from the pane
// point `p` to the plane, square to the view ray, of the background the pixel sees straight
// through (a single-step screen-space march), projected with `view_proj`, and offset by
// `centre_px` − the pixel (`centre_px` is where the smooth pass samples: the bent pixel). Taps off
// screen or on something nearer than the pane are skipped; with none left the result is
// `fallback`. `clip_z` is the pane's depth, `near_far` the projection's planes, `eye` the camera,
// `frag` the pixel, `view_px` the view size, `rotation` the pixel's two numbers in [0, 1).
fn screen_average(
    scene: texture_2d<f32>,
    depth: texture_depth_2d,
    samp: sampler,
    view_proj: mat4x4<f32>,
    near_far: vec2<f32>,
    eye: vec3<f32>,
    p: vec3<f32>,
    n: vec3<f32>,
    wo: vec3<f32>,
    alpha: f32,
    clip_z: f32,
    frag: vec2<f32>,
    centre_px: vec2<f32>,
    view_px: vec2<f32>,
    rotation: vec2<f32>,
    fallback: vec3<f32>,
) -> vec3<f32> {
    let dims = vec2<f32>(textureDimensions(scene));
    let lin = near_far.x * near_far.y;
    let span = near_far.y - near_far.x;
    let z_bg = lin / max(near_far.y - textureLoad(depth, vec2<i32>(frag), 0) * span, 1e-6);
    let z_here = lin / max(near_far.y - clip_z * span, 1e-6);
    let dist = min(length(eye - p) * max(z_bg / max(z_here, 1e-6) - 1.0, 0.0), near_far.y);
    let here_clip = view_proj * vec4<f32>(p, 1.0);
    let here_ndc = here_clip.xy / max(here_clip.w, 1e-6);
    var sum = vec3<f32>(0.0);
    var count = 0.0;
    for (var i = 0u; i < FORWARD_TAPS; i = i + 1u) {
        let t = sample_direction(wo, n, alpha, tap(i, FORWARD_TAPS, rotation));
        let s = dist / max(dot(t, -wo), 0.25);
        let clip = view_proj * vec4<f32>(p + t * s, 1.0);
        if (clip.w <= 1e-4) {
            continue;
        }
        let d = clip.xy / clip.w - here_ndc;
        let px = centre_px + vec2<f32>(d.x, -d.y) * 0.5 * view_px;
        if (any(px < vec2<f32>(0.0)) || any(px >= min(view_px, dims))) {
            continue;
        }
        if (textureLoad(depth, vec2<i32>(px), 0) < clip_z) {
            continue;
        }
        sum = sum + textureSampleLevel(scene, samp, px / dims, 0.0).rgb;
        count = count + 1.0;
    }
    if (count <= 0.0) {
        return fallback;
    }
    return sum / count;
}
