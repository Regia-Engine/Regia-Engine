// SPDX-License-Identifier: MPL-2.0
// Screen-space one-bounce diffuse (the cheaper GI tier). No probe trace.
//
// Each pixel gathers the lit colour of nearby on-screen surfaces that face it: the
// view-space position and normal come from the depth buffer, a sample contributes
// its radiance × the receiver's cosine toward it × a distance falloff, and the sum is
// scaled by a nominal receiver albedo (the forward pass has no albedo buffer).
// Coplanar neighbours (cosine ≈ 0) and the sky contribute nothing.
//
// The previous version added 55 % of a 12-pixel blur of the lit image to every
// pixel: a uniform glow that washed out sunlit frames instead of bouncing light
// into corners. Gather pattern after the screen-space indirect-diffuse approaches in
// Ritschel, Grosch, Seidel, "Approximating Dynamic Global Illumination in Image
// Space", I3D 2009 (directional occlusion + one indirect bounce from screen samples).
//
// Desktop High gathers the bounce alone at half resolution (`noise.y` = 2) and
// `fs_upsample` lifts it with a joint bilateral filter (Kopf, Cohen, Lischinski,
// Uyttendaele, "Joint Bilateral Upsampling", SIGGRAPH 2007) onto the full-resolution
// colour (`base_tex`). The half gather used to carry `colour + bounce`, so the frame
// showed one forward sample per 2×2 pixels (motion-look lane, round 2). `noise.y` = 1
// adds the bounce to the colour in one pass. `noise.y` = 4 gathers at quarter resolution
// and uses the same upsample (ADR-3964; off unless the host asks). `screen.xy` is always
// the full size.
//
// Pixels of emitters that fitted area lights carry (HDR alpha 8, `lab_forward.wgsl`
// `EMITTER_ALPHA`) are skipped, so their light is not counted twice (lane emissive-local).

struct SsgiParams {
    // x = full width, y = full height, z = world gather radius (m), w = strength.
    screen: vec4<f32>,
    // x = projection[0][0], y = projection[1][1], z = near, w = far.
    proj: vec4<f32>,
    // x = temporal noise index (0 without temporal AA).
    // y = full-texel step per gather pixel (1 = native, 2 = half, 4 = quarter).
    // z / w = gather width / height (the upsample's low-resolution size).
    noise: vec4<f32>,
    // x = 1: the upsample weighs taps by relative linear depth instead of raw device depth
    // (a measurement variant, ADR-1033; 0 = the shipped weights).
    up: vec4<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
}

#ifdef BLUE_NOISE
// Spatiotemporal blue noise for the spiral rotation (ADR-2075), `noise.x` picks the slice.
#import regia::blue_noise
@group(1) @binding(0) var blue_noise_table: texture_2d_array<f32>;
#endif
@group(0) @binding(0) var<uniform> params: SsgiParams;
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var depth_tex: texture_depth_2d;
// The full-resolution colour the upsampled bounce is added to (`fs_upsample` only).
@group(0) @binding(3) var base_tex: texture_2d<f32>;
// The glass front layer's transmittance (`lab_forward.wgsl` `front_layer_weight`; 1×1 white
// without glass): the bounce lights the opaque surface behind a pane, so it is seen through it.
// Added over the composited frame, it used to show at full strength through dark glass (DO27's
// X12 canopy: the cockpit's bounce over the canopy pixels, 2-4x of the glass's own light).
@group(0) @binding(4) var front_tex: texture_2d<f32>;

fn front_transmittance(pix: vec2<i32>) -> vec3<f32> {
    let size = vec2<i32>(textureDimensions(front_tex));
    return textureLoad(front_tex, clamp(pix, vec2<i32>(0), size - vec2<i32>(1)), 0).rgb;
}

// Nominal receiver albedo for the bounce (no albedo buffer in the forward path).
const RECEIVER_ALBEDO: f32 = 0.35;
const SAMPLES: u32 = 16u;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    // Corners (-1,-1), (3,-1), (-1,3), without a function-local array or struct.
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

fn linear_depth(d: f32) -> f32 {
    let n = params.proj.z;
    let f = params.proj.w;
    return n * f / max(f - d * (f - n), 1e-6);
}

fn view_position(coord: vec2<i32>) -> vec3<f32> {
    let size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
    let c = clamp(coord, vec2<i32>(0), size - vec2<i32>(1));
    let z = linear_depth(textureLoad(depth_tex, c, 0));
    let uv = (vec2<f32>(c) + vec2<f32>(0.5)) / vec2<f32>(size);
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    return vec3<f32>(ndc.x * z / params.proj.x, ndc.y * z / params.proj.y, -z);
}

fn ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// Accepted gather steps are 1 (native), 2 (half) and 4 (quarter). Anything else snaps to
// native, so an unmeasured scale cannot ship. 1.0 and 2.0 stay exact (`abs` of zero).
fn gather_scale() -> f32 {
    let s = params.noise.y;
    if (abs(s - 2.0) < 0.25) {
        return 2.0;
    }
    if (abs(s - 4.0) < 0.25) {
        return 4.0;
    }
    return 1.0;
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    // Reduced-resolution fragments sample the full-resolution centre of their block.
    // Scale 1 keeps `pix == frag` (`i32(1) / 2 == 0`). Scale 2 centres a 2×2; scale 4 a 4×4.
    let scale = gather_scale();
    let frag_i = vec2<i32>(i32(frag.x), i32(frag.y));
    let size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
    let pix = clamp(frag_i * i32(scale) + i32(scale) / 2, vec2<i32>(0), size - vec2<i32>(1));
    // At step 2 or 4 the gather is the bounce alone; the upsample adds the colour.
    let half = scale > 1.5;
    var base = textureLoad(hdr_tex, pix, 0);
    if (half) {
        base = vec4<f32>(0.0);
    }
    let depth = textureLoad(depth_tex, pix, 0);
    if (depth >= 1.0 || params.screen.w <= 0.0) {
        return vec4<f32>(max(base.rgb, vec3<f32>(0.0)), base.a);
    }
    let p = view_position(pix);
    let px0 = view_position(pix - vec2<i32>(1, 0));
    let px1 = view_position(pix + vec2<i32>(1, 0));
    let py0 = view_position(pix - vec2<i32>(0, 1));
    let py1 = view_position(pix + vec2<i32>(0, 1));
    let dx = select(px1 - p, p - px0, abs(p.z - px0.z) < abs(px1.z - p.z));
    let dy = select(py1 - p, p - py0, abs(p.z - py0.z) < abs(py1.z - p.z));
    let c = cross(dy, dx);
    let c_len2 = dot(c, c);
    if (!(c_len2 > 1e-24)) {
        return vec4<f32>(max(base.rgb, vec3<f32>(0.0)), base.a);
    }
    var n = c * inverseSqrt(c_len2);
    if (dot(n, -p) < 0.0) {
        n = -n;
    }
    let radius = params.screen.z;
    let projected_px = radius * params.proj.y * 0.5 * params.screen.y / max(-p.z, 1e-3);
    // Far receivers: the gather radius is a couple of pixels and only picks up depth
    // noise on their own surface. Probe GI covers that scale.
    if (projected_px < 3.0) {
        return vec4<f32>(max(base.rgb, vec3<f32>(0.0)), base.a);
    }
    let radius_px = min(projected_px, params.screen.y * 0.08);
#ifdef BLUE_NOISE
    let rotation = regia::blue_noise::scalar(blue_noise_table, vec2<u32>(frag.xy), u32(params.noise.x)) * 6.2831853;
#else
    let rotation = ign(frag.xy + 5.588238 * params.noise.x) * 6.2831853;
#endif
    var sum = vec3<f32>(0.0);
    for (var i: u32 = 0u; i < SAMPLES; i = i + 1u) {
        let t = (f32(i) + 0.5) / f32(SAMPLES);
        let angle = f32(i) * 2.399963 + rotation;
        let offset = vec2<f32>(cos(angle), sin(angle)) * radius_px * sqrt(t);
        let sc = pix + vec2<i32>(offset);
        if (textureLoad(depth_tex, clamp(sc, vec2<i32>(0), vec2<i32>(i32(params.screen.x) - 1, i32(params.screen.y) - 1)), 0) >= 1.0) {
            continue;
        }
        let s = view_position(sc);
        let v = s - p;
        let dist2 = dot(v, v);
        let cos_r = dot(n, v) * inverseSqrt(max(dist2, 1e-6));
        if (cos_r <= 0.05) {
            continue;
        }
        let falloff = clamp(1.0 - dist2 / (radius * radius), 0.0, 1.0);
        let size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
        let texel = textureLoad(hdr_tex, clamp(sc, vec2<i32>(0), size - vec2<i32>(1)), 0);
        // An emitter whose light a fitted area light already carries (`lab_forward.wgsl`
        // `EMITTER_ALPHA`, 8) is not gathered twice.
        if (texel.a > 7.0) {
            continue;
        }
        sum += max(texel.rgb, vec3<f32>(0.0)) * cos_r * falloff;
    }
    var bounce = sum / f32(SAMPLES) * 2.0 * RECEIVER_ALBEDO * params.screen.w;
    if (!half) {
        bounce *= front_transmittance(pix);
    }

    // `base.a` is perceptual roughness for SSR.
    return vec4<f32>(max(base.rgb, vec3<f32>(0.0)) + bounce, base.a);
}

// Joint bilateral upsample of the reduced-resolution bounce (step 2 or 4) onto the
// full-resolution colour. `hdr_tex` is the gather (unfilterable). `depth_tex` is the
// full-resolution depth. A sky pixel (depth ≥ 1) does not mix with ground, and the reverse.
// The 2×2 neighbourhood is in gather texels: step 4 still weights four taps, each one
// standing for a 4×4 block of the full frame.
@fragment
fn fs_upsample(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let full = vec2<i32>(i32(frag.x), i32(frag.y));
    let full_size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
    let full_c = clamp(full, vec2<i32>(0), full_size - vec2<i32>(1));
    let depth = textureLoad(depth_tex, full_c, 0);
    let scale = gather_scale();
    let low_f = (vec2<f32>(full_c) + vec2<f32>(0.5)) / scale - vec2<f32>(0.5);
    let low_size = vec2<i32>(max(i32(params.noise.z), 1), max(i32(params.noise.w), 1));
    let base_x = i32(floor(low_f.x));
    let base_y = i32(floor(low_f.y));
    let sky_self = depth >= 1.0;
    // The relative linear-depth variant (`up.x`): a tap counts while its surface lies within
    // 2 % + 5 cm of this pixel's (the probe lookup's band, `ddgi_screen_irradiance`); none
    // within it takes the tap nearest in depth. exp(−80 |Δd|) on raw standard-Z depth barely
    // separates far silhouettes (10 m against 20 m with a 5 cm near plane: weight 0.82).
    let relative = params.up.x > 0.5;
    let z_self = linear_depth(depth);
    var best = vec3<f32>(0.0);
    var best_err = 1.0e30;
    var sum_rgb = vec3<f32>(0.0);
    var sum_w = 0.0;
    for (var oy: i32 = 0; oy < 2; oy = oy + 1) {
        for (var ox: i32 = 0; ox < 2; ox = ox + 1) {
            let lx = clamp(base_x + ox, 0, low_size.x - 1);
            let ly = clamp(base_y + oy, 0, low_size.y - 1);
            let cx = clamp(lx * i32(scale) + i32(scale) / 2, 0, full_size.x - 1);
            let cy = clamp(ly * i32(scale) + i32(scale) / 2, 0, full_size.y - 1);
            let nd = textureLoad(depth_tex, vec2<i32>(cx, cy), 0);
            let sky_n = nd >= 1.0;
            if (sky_self != sky_n) {
                continue;
            }
            let dx = low_f.x - f32(lx);
            let dy = low_f.y - f32(ly);
            let spatial = max(0.0, 1.0 - length(vec2<f32>(dx, dy)));
            var depth_w = exp(-abs(depth - nd) * 80.0);
            let tap = textureLoad(hdr_tex, vec2<i32>(lx, ly), 0);
            if (relative) {
                let err = abs(linear_depth(nd) - z_self) / (0.02 * z_self + 0.05);
                depth_w = max(1.0 - err, 0.0);
                if (err < best_err) {
                    best_err = err;
                    best = tap.rgb;
                }
            }
            let w = spatial * depth_w;
            sum_rgb += tap.rgb * w;
            sum_w += w;
        }
    }
    // `base.a` is perceptual roughness for SSR.
    let base = textureLoad(base_tex, full_c, 0);
    var bounce = vec3<f32>(0.0);
    if (sum_w < 1e-4 && relative && best_err < 1.0e29) {
        bounce = best;
    } else if (sum_w < 1e-4) {
        let nx = clamp(i32(floor(low_f.x + 0.5)), 0, low_size.x - 1);
        let ny = clamp(i32(floor(low_f.y + 0.5)), 0, low_size.y - 1);
        bounce = textureLoad(hdr_tex, vec2<i32>(nx, ny), 0).rgb;
    } else {
        bounce = sum_rgb / sum_w;
    }
    bounce *= front_transmittance(full_c);
    // The radiance cache's gather (`ssgi_cached.wgsl`) is signed; the plain gather never is below 0.
    return vec4<f32>(max(max(base.rgb, vec3<f32>(0.0)) + bounce, vec3<f32>(0.0)), base.a);
}
