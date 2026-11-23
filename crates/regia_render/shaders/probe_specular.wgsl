// SPDX-License-Identifier: MPL-2.0
// Probe specular (ADR-1103): the indirect specular read from the probe light per roughness.
// Mirrored on the CPU by `probe_specular.rs`; keep the two in step. Appended after
// ddgi_sample.wgsl (`ddgi_shading_wgsl`), so it reads the same `ddgi` / `ddgi_data` bindings.
//
// The probe light is linear in direction: L(l) = a + b·l (L1 spherical harmonics, Ramamoorthi &
// Hanrahan 2001; the baked volume stores it and the six-direction screen lookup holds its cosine
// convolution, a + (2/3)·b·d toward d). For such light the average over any lobe w(l) is exactly
// a + b·m, with m = ∫ w(l) l dl / ∫ w(l) dl the lobe's mean vector. For the GGX specular lobe
// (BRDF × cosine with the height-correlated Smith term: Walter et al. 2007, Heitz 2014) m lies in
// the plane of the normal and the mirror direction. `probe_spec_lobe` fits it: its direction
// leans from the mirror direction toward the normal as the roughness grows (the lerp form of
// Lagarde & de Rousiers 2014, § 4.9.3, refitted to the mean rather than the peak, with a
// view-angle term) and its length falls from 1 (a mirror) to about 0.62 at α = 1 (the clamped
// cosine's 2/3, close). Fitted by least squares to numerical integration of that lobe; worst
// vector error 0.036 for α in [0, 1] and n·v ≥ 0.1.

// A non-negative field has |b| ≤ 3a (its L1 projection of a single direction); a larger
// gradient in a reconstructed field (clamped ambient-cube values) is scaled back to it.
const PSPEC_MAX_GRADIENT: f32 = 3.0;
// Radiance gradient over the irradiance / π gradient: the clamped cosine keeps 2/3 of band 1.
const PSPEC_DECONVOLVE: f32 = 1.5;
// Cap on the sky prefilter's detail ratio (`probe_spec_detail`): the cap only removes light.
const PSPEC_DETAIL_MAX: f32 = 8.0;
// `ctrl.w` bit of `ProbeSpecular::LobeHue` (ADR-2996, `CTRL_PROBE_SPECULAR_HUE`): the lobe light keeps
// at least this share of its channel's mean, so a lobe aimed away from a coloured sky cannot drive
// the bluest channel to zero first and leave the others. The linear form goes negative where the
// real light is merely dim (L1 ringing): the share is the level the four fields of the ADR's table
// fit best, per channel so that a red wall and a green wall opposite it keep their colors.
const PSPEC_CTRL_HUE: u32 = 128u;
const PSPEC_HUE_FLOOR: f32 = 0.2;
// The cosine between the probes' lobe light and the sky's: below LOW their colors differ, above
// HIGH they agree (`probe_spec_hue_agreement`).
const PSPEC_HUE_AGREE_LOW: f32 = 0.99;
const PSPEC_HUE_AGREE_HIGH: f32 = 0.9995;
// Below HIGH of its mean's luminance the lobe's color fades to the mean's, fully at LOW
// (`probe_spec_hue_fade`).
const PSPEC_HUE_FADE_LOW: f32 = 0.4;
const PSPEC_HUE_FADE_HIGH: f32 = 0.9;
const PSPEC_LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// Length of the GGX lobe's mean vector for roughness `alpha` (α = perceptual²) and n·v.
fn probe_spec_lobe_length(n_dot_v: f32, alpha: f32) -> f32 {
    let a = clamp(alpha, 0.0, 1.0);
    let u = max(1.0 - clamp(n_dot_v, 0.0, 1.0), 1e-4);
    let rational = (1.0 + a * (3.673155 + 7.571242 * a)) / (1.0 + a * (3.719715 + 15.010073 * a));
    return clamp(rational - 0.128804 * sqrt(a) * pow(u, 5.260887), 0.0, 1.0);
}

// How far the lobe's mean direction leans from the normal toward the mirror direction (1 at
// α = 0, 0 at α = 1).
fn probe_spec_lobe_lerp(n_dot_v: f32, alpha: f32) -> f32 {
    let a = clamp(alpha, 0.0, 1.0);
    let u = max(1.0 - clamp(n_dot_v, 0.0, 1.0), 1e-4);
    let x = 1.0 - a;
    let den = 1.0 + a * (1.97079 + 3.496811 * a) * pow(u, 4.880938) * (1.0 - 1.183977 * a) + 0.858009 * a * u;
    return clamp(x * (sqrt(x) + a) / den, 0.0, 1.0);
}

// The GGX specular lobe's mean vector (length ≤ 1) for normal `n`, mirror direction `r`.
fn probe_spec_lobe(n: vec3<f32>, r: vec3<f32>, n_dot_v: f32, alpha: f32) -> vec3<f32> {
    let d = mix(n, r, probe_spec_lobe_lerp(n_dot_v, alpha));
    let len2 = dot(d, d);
    let dir = select(n, d * inverseSqrt(len2), len2 > 1e-12);
    return dir * probe_spec_lobe_length(n_dot_v, alpha);
}

// Whether this frame's probe specular keeps its hue (`ProbeSpecular::LobeHue`, ADR-2996).
fn probe_spec_hue_on() -> bool {
    return (ddgi.ctrl.w & PSPEC_CTRL_HUE) != 0u;
}

// The lobe light `v` of a field of mean `a`, its luminance kept, its color fading to the mean's as
// its level falls below the mean's. The channels of a dim lobe say little: the bluest falls first
// under a blue sky, which leaves red or magenta over a neutral ground (ADR-2996). A lobe at a lit side
// keeps its own color, so a wall reads as that wall.
fn probe_spec_hue_fade(a: vec3<f32>, v: vec3<f32>) -> vec3<f32> {
    let mean = dot(a, PSPEC_LUMA);
    if (mean <= 1e-8) {
        return v;
    }
    let level = dot(v, PSPEC_LUMA) / mean;
    return mix(a * level, v, smoothstep(PSPEC_HUE_FADE_LOW, PSPEC_HUE_FADE_HIGH, level));
}

// a + b·m for one channel, the gradient `b` scaled back to |b| ≤ 3a, held at `floor` (0, or the
// channel's share under `ProbeSpecular::LobeHue`).
fn probe_spec_channel(a: f32, b: vec3<f32>, m: vec3<f32>, floor_share: f32) -> f32 {
    let len = length(b);
    let k = select(1.0, min(1.0, PSPEC_MAX_GRADIENT * max(a, 0.0) / len), len > 1e-20);
    return max(a + k * dot(b, m), floor_share * max(a, 0.0));
}

// Radiance in a lobe of mean vector `m` from the six irradiance / π values of the ambient cube
// (+x, −x, +y, −y, +z, −z): a = their mean, the gradient per axis half the difference of the
// pair, deconvolved by 3/2.
fn probe_spec_cube_lobe(
    px: vec3<f32>,
    nx: vec3<f32>,
    py: vec3<f32>,
    ny: vec3<f32>,
    pz: vec3<f32>,
    nz: vec3<f32>,
    m: vec3<f32>,
) -> vec3<f32> {
    let a = (px + nx + py + ny + pz + nz) * (1.0 / 6.0);
    let k = 0.5 * PSPEC_DECONVOLVE;
    let gx = (px - nx) * k;
    let gy = (py - ny) * k;
    let gz = (pz - nz) * k;
    let hue = probe_spec_hue_on();
    let floor_share = select(0.0, PSPEC_HUE_FLOOR, hue);
    let v = vec3<f32>(
        probe_spec_channel(a.r, vec3<f32>(gx.r, gy.r, gz.r), m, floor_share),
        probe_spec_channel(a.g, vec3<f32>(gx.g, gy.g, gz.g), m, floor_share),
        probe_spec_channel(a.b, vec3<f32>(gx.b, gy.b, gz.b), m, floor_share),
    );
    if (hue) {
        return probe_spec_hue_fade(max(a, vec3<f32>(0.0)), v);
    }
    return v;
}

// The same from the irradiance / π toward the lobe's own direction (`e_pos`) and away from it
// (`e_neg`), for a lobe of mean-vector length `len`: the sky irradiance map's lobe.
fn probe_spec_axis_lobe(e_pos: vec3<f32>, e_neg: vec3<f32>, len: f32) -> vec3<f32> {
    let a = 0.5 * (e_pos + e_neg);
    let b = 0.5 * PSPEC_DECONVOLVE * (e_pos - e_neg);
    let lim = PSPEC_MAX_GRADIENT * max(a, vec3<f32>(0.0));
    return max(a + clamp(b, -lim, lim) * len, vec3<f32>(0.0));
}

// `probe_spec_axis_lobe` under `ProbeSpecular::LobeHue`: each channel keeps its floor share, as
// `probe_spec_cube_lobe` does, so the sky's lobe light and the probes' fall alike (ADR-2996).
fn probe_spec_hue_axis_lobe(e_pos: vec3<f32>, e_neg: vec3<f32>, len: f32) -> vec3<f32> {
    let a = max(0.5 * (e_pos + e_neg), vec3<f32>(0.0));
    let b = 0.5 * PSPEC_DECONVOLVE * (e_pos - e_neg);
    let lim = PSPEC_MAX_GRADIENT * a;
    return probe_spec_hue_fade(a, max(a + clamp(b, -lim, lim) * len, PSPEC_HUE_FLOOR * a));
}

// `lobe` with the light of what the image's ground plane hides taken out and the plane's own light
// put in, as the diffuse lookup does (`lab_forward.wgsl`, ADR-2000), each channel keeping its floor
// share of the lobe so the subtraction cannot separate the channels either (ADR-2996).
fn probe_spec_ground_swap(lobe: vec3<f32>, hidden: vec3<f32>, add: vec3<f32>) -> vec3<f32> {
    return max(lobe - hidden, PSPEC_HUE_FLOOR * lobe) + add;
}

// How far the probes' lobe light and the sky's agree in color, 0 to 1 from the cosine between them
// (`ProbeSpecular::LobeHue`, ADR-2996). A wall, or the object itself hiding the sky from the probes,
// parts them; only rounding and the map's band 2 part them under an open sky.
fn probe_spec_hue_agreement(probe_lobe: vec3<f32>, sky_lobe: vec3<f32>) -> f32 {
    let lengths = dot(probe_lobe, probe_lobe) * dot(sky_lobe, sky_lobe);
    if (lengths <= 1e-20) {
        return 0.0;
    }
    return smoothstep(PSPEC_HUE_AGREE_LOW, PSPEC_HUE_AGREE_HIGH, dot(probe_lobe, sky_lobe) * inverseSqrt(lengths));
}

// `probe_spec_detail` per channel under `ProbeSpecular::LobeHue` (ADR-2996), each channel with its own
// cap: where the probes and the sky agree in color the frame reads the prefilter's own color, which
// the probes' L1 light cannot hold (the horizon of a gradient sky, a neutral ground under a blue
// sky). Where they do not, the ratio is `luminance_detail`: a per-channel ratio of a sky the probes
// do not see would print the sky's complement on their light.
fn probe_spec_detail_rgb(
    prefilter: vec3<f32>,
    sky_lobe: vec3<f32>,
    probe_lobe: vec3<f32>,
    luminance_detail: f32,
) -> vec3<f32> {
    let ratio = clamp(prefilter / max(sky_lobe, vec3<f32>(1e-6)), vec3<f32>(0.0), vec3<f32>(PSPEC_DETAIL_MAX));
    let per_channel = select(ratio, vec3<f32>(1.0), sky_lobe <= vec3<f32>(1e-6));
    return mix(vec3<f32>(luminance_detail), per_channel, probe_spec_hue_agreement(probe_lobe, sky_lobe));
}

// The sky prefilter's detail over its own lobe light (luminances): what the probes' smooth lobe
// light does not hold (a bright horizon band in a glossy reflection). 1 for a dark sky.
fn probe_spec_detail(prefilter_lum: f32, sky_lobe_lum: f32) -> f32 {
    if (sky_lobe_lum <= 1e-6) {
        return 1.0;
    }
    return clamp(prefilter_lum / sky_lobe_lum, 0.0, PSPEC_DETAIL_MAX);
}

// The screen lookup for a surface: `diffuse` as `ddgi_screen_irradiance` returns it (the same
// texels, weights and ambient-cube rebuild, at the lookup's resolution step `ddgi.screen.w`) and
// `spec`, the radiance in the lobe of mean vector `m` (`probe_spec_lobe`), rebuilt from the same
// texels.
struct DdgiScreenGi {
    diffuse: vec4<f32>,
    spec: vec3<f32>,
}

fn ddgi_screen_gi(frag: vec2<f32>, p: vec3<f32>, n: vec3<f32>, m: vec3<f32>, eye: vec3<f32>) -> DdgiScreenGi {
    if (ddgi.screen.w == 0u) {
        return DdgiScreenGi(vec4<f32>(0.0), vec3<f32>(0.0));
    }
    let dist = length(p - eye);
    // The texel scale of `ddgi_screen_irradiance`: the lookup's resolution step `ddgi.screen.w`
    // (1 full, 2 half, 4 quarter; ADR-1377: a fixed half scale read the wrong texels under the
    // Full and Quarter lookups).
    let scale = select(select(0.25, 0.5, ddgi.screen.w == 2u), 1.0, ddgi.screen.w == 1u);
    let hc = (frag - vec2<f32>(0.5)) * scale;
    let fl = floor(hc);
    let f = hc - fl;
    let size = vec2<i32>(ddgi.screen.yz);
    let n2 = n * n;
    var sum = vec4<f32>(0.0);
    var spec_sum = vec3<f32>(0.0);
    var wsum = 0.0;
    var best = vec4<f32>(0.0);
    var best_spec = vec3<f32>(0.0);
    var best_err = 1.0e30;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let o = vec2<i32>(i32(k & 1u), i32(k >> 1u));
        let t = clamp(vec2<i32>(fl) + o, vec2<i32>(0), size - vec2<i32>(1));
        let w_bi = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        let word = ddgi.screen.x + (u32(t.y) * ddgi.screen.y + u32(t.x)) * DDGI_SCREEN_WORDS;
        let d = bitcast<f32>(ddgi_data[word + 6u]);
        let err = abs(d - dist) / (0.02 * dist + 0.05);
        let w = (w_bi + 1e-3) * max(1.0 - err, 0.0);
        if (w <= 0.0 && !(err < best_err && d < 1.0e29)) {
            continue;
        }
        let px = ddgi_unpack_rgb9e5(ddgi_data[word]);
        let nx = ddgi_unpack_rgb9e5(ddgi_data[word + 1u]);
        let py = ddgi_unpack_rgb9e5(ddgi_data[word + 2u]);
        let ny = ddgi_unpack_rgb9e5(ddgi_data[word + 3u]);
        let pz = ddgi_unpack_rgb9e5(ddgi_data[word + 4u]);
        let nz = ddgi_unpack_rgb9e5(ddgi_data[word + 5u]);
        let e = select(px, nx, n.x < 0.0) * n2.x + select(py, ny, n.y < 0.0) * n2.y + select(pz, nz, n.z < 0.0) * n2.z;
        let texel = vec4<f32>(e / max(n2.x + n2.y + n2.z, 1e-6), bitcast<f32>(ddgi_data[word + 7u]));
        let s = probe_spec_cube_lobe(px, nx, py, ny, pz, nz, m);
        if (err < best_err && d < 1.0e29) {
            best = texel;
            best_spec = s;
            best_err = err;
        }
        sum = sum + texel * w;
        spec_sum = spec_sum + s * w;
        wsum = wsum + w;
    }
    if (wsum <= 1e-4) {
        return DdgiScreenGi(best, best_spec);
    }
    return DdgiScreenGi(sum / wsum, spec_sum / wsum);
}
