// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::pbr::thin_film

// Thin-film interference over the base specular (`KHR_materials_iridescence`), shared by the
// forward pass's `THIN_FILM` variant and the reference path tracer. `regia_render::thin_film`
// mirrors every function here and checks it against a brute-force spectral integral.
//
// The Airy reflectance of one film between air (index 1) and the base, with real interface
// reflectances R1 and R2 and a = sqrt(R1 R2), is
//     R(d) = (R1 + R2 + 2 s a cos d) / (1 + R1 R2 + 2 s a cos d)
// for the round-trip phase d (Born & Wolf, Principles of Optics, ch. 7.6); s is the sign of the
// product of the two amplitude reflectances. Taking the Poisson kernel's Fourier series gives
//     R(d) = 1 - K (1 + 2 sum_m (-s a)^m cos(m d)),   K = (1 - R1)(1 - R2) / (1 - R1 R2).
// Harmonics 1 to 4 are kept; harmonic 5 stands for the rest of the geometric series, so the sum
// is exact for a film of no thickness however reflective the base.
// The phase runs linearly in the wavenumber v = 1 / wavelength, d = 2 pi OPD v, so a spectral
// average only needs the average of cos(2 pi m OPD v) (Belcour & Barla 2017 make the same
// observation): with the colour sensitivities as sums of Gaussians over v, each is a closed form,
//     w cos(2 pi m OPD mu) exp(-2 (pi m OPD sigma)^2).
// The seven lobes below are this project's own fit to the multi-lobe analytic CIE 1931 curves of
// Wyman, Sloan & Shirley 2013 for an equal-energy spectrum (wavenumbers in 1 / micrometre), fitted
// on the RGB averages themselves over path differences up to 6 micrometres.

// Lobes: X, X, X, Y in the vec4; Y, Z, Z in the vec3.
const LOBE_MU_A: vec4<f32> = vec4<f32>(1.63928, 2.24077, 1.75523, 1.79778);
const LOBE_MU_B: vec3<f32> = vec3<f32>(1.66969, 2.31791, 2.209);
const LOBE_SIGMA_A: vec4<f32> = vec4<f32>(0.07153, 0.10022, 0.0787, 0.12961);
const LOBE_SIGMA_B: vec3<f32> = vec3<f32>(0.06761, 0.0175, 0.11156);
// Each channel's lobe weights; they sum to 1.
const WEIGHT_X: vec3<f32> = vec3<f32>(0.5339, 0.16276, 0.30334);
const WEIGHT_Y: vec2<f32> = vec2<f32>(0.92041, 0.07959);
const WEIGHT_Z: vec2<f32> = vec2<f32>(0.03131, 0.96869);
// XYZ (each normalised to white = 1) to linear Rec. 709. Every row sums to 1, so a reflectance
// that does not depend on the wavelength stays neutral.
const TO_RGB_R: vec3<f32> = vec3<f32>(3.079954, -1.537138, -0.542816);
const TO_RGB_G: vec3<f32> = vec3<f32>(-0.921258, 1.876011, 0.045247);
const TO_RGB_B: vec3<f32> = vec3<f32>(0.052887, -0.204026, 1.151139);

const TAU: f32 = 6.283185307;
// A thickness below this fades the film out, so 0 nm is the base Fresnel exactly.
const FADE_NM: f32 = 30.0;

// Colour response of the lobes' spectral averages `v`.
fn to_rgb(a: vec4<f32>, b: vec3<f32>) -> vec3<f32> {
    let xyz = vec3<f32>(dot(WEIGHT_X, a.xyz), dot(WEIGHT_Y, vec2<f32>(a.w, b.x)), dot(WEIGHT_Z, b.yz));
    return vec3<f32>(dot(TO_RGB_R, xyz), dot(TO_RGB_G, xyz), dot(TO_RGB_B, xyz));
}

// Unpolarized reflectance of a dielectric interface from index n1 to n2, the cosines on each side.
fn dielectric_fresnel(cos_1: f32, cos_2: f32, n1: f32, n2: f32) -> f32 {
    let rs = (n1 * cos_1 - n2 * cos_2) / max(n1 * cos_1 + n2 * cos_2, 1e-6);
    let rp = (n2 * cos_1 - n1 * cos_2) / max(n2 * cos_1 + n1 * cos_2, 1e-6);
    return 0.5 * (rs * rs + rp * rp);
}

// Reflectance of a film of index `ior`, `thickness` nanometres thick, at `cos_i` from air, over a
// base whose normal-incidence reflectance against air is `f0`. The base's index comes from `f0`,
// per channel; a metal becomes a very high real index (reflective, no phase flip).
fn reflectance(cos_i: f32, ior: f32, thickness: f32, f0: vec3<f32>) -> vec3<f32> {
    let n2 = max(ior, 1.0);
    let c1 = clamp(cos_i, 0.0, 1.0);
    let c2 = sqrt(max(1.0 - (1.0 - c1 * c1) / (n2 * n2), 0.0));
    let r1 = dielectric_fresnel(c1, c2, 1.0, n2);
    let root = sqrt(clamp(f0, vec3<f32>(0.0), vec3<f32>(0.9998)));
    let n3 = (vec3<f32>(1.0) + root) / (vec3<f32>(1.0) - root);
    let r23 = (n3 - vec3<f32>(n2)) / (n3 + vec3<f32>(n2));
    let normal = r23 * r23;
    let x = 1.0 - c2;
    let x2 = x * x;
    let r2 = normal + (vec3<f32>(1.0) - normal) * (x2 * x2 * x);
    let a = sqrt(r1 * r2);
    let k = (1.0 - r1) * (vec3<f32>(1.0) - r2) / max(vec3<f32>(1.0) - r1 * r2, vec3<f32>(1e-5));
    // The air/film amplitude is negative (n2 >= 1): the film/base one has its sign for n3 > n2.
    let s = select(vec3<f32>(-1.0), vec3<f32>(1.0), n3 >= vec3<f32>(n2));
    let p = TAU * 2.0 * n2 * max(thickness, 0.0) * c2 * 0.001;
    let ca = cos(p * LOBE_MU_A);
    let cb = cos(p * LOBE_MU_B);
    let pa = p * LOBE_SIGMA_A;
    let pb = p * LOBE_SIGMA_B;
    let ea = exp(-0.5 * pa * pa);
    let eb = exp(-0.5 * pb * pb);
    // The higher harmonics from the first: cos (n + 1)x = 2 cos x cos nx - cos (n - 1)x, and the
    // Gaussian's e^(n^2).
    let ca_2 = 2.0 * ca * ca - vec4<f32>(1.0);
    let cb_2 = 2.0 * cb * cb - vec3<f32>(1.0);
    let ca_3 = 2.0 * ca * ca_2 - ca;
    let cb_3 = 2.0 * cb * cb_2 - cb;
    let ca_4 = 2.0 * ca * ca_3 - ca_2;
    let cb_4 = 2.0 * cb * cb_3 - cb_2;
    let ca_5 = 2.0 * ca * ca_4 - ca_3;
    let cb_5 = 2.0 * cb * cb_4 - cb_3;
    let ea_4 = ea * ea * ea * ea;
    let eb_4 = eb * eb * eb * eb;
    let ea_8 = ea_4 * ea_4;
    let eb_8 = eb_4 * eb_4;
    let s1 = to_rgb(ca * ea, cb * eb);
    let s2 = to_rgb(ca_2 * ea_4, cb_2 * eb_4);
    let s3 = to_rgb(ca_3 * ea_8 * ea, cb_3 * eb_8 * eb);
    let s4 = to_rgb(ca_4 * ea_8 * ea_8, cb_4 * eb_8 * eb_8);
    let s5 = to_rgb(ca_5 * ea_8 * ea_8 * ea_8 * ea, cb_5 * eb_8 * eb_8 * eb_8 * eb);
    let m1 = -s * a;
    let m2 = m1 * m1;
    let m3 = m2 * m1;
    let m4 = m2 * m2;
    // Harmonic 5 and every one past it: r^5 / (1 - r) for the ratio r = m1, the denominator
    // floored as on the CPU (`thin_film.rs`) so a ratio near 1 does not give 0 * inf = NaN.
    let m5 = m4 * m1 / max(vec3<f32>(1.0) - m1, vec3<f32>(1e-3));
    let out = vec3<f32>(1.0) - k * (vec3<f32>(1.0) + 2.0 * (m1 * s1 + m2 * s2 + m3 * s3 + m4 * s4 + m5 * s5));
    return clamp(out, vec3<f32>(0.0), vec3<f32>(1.0));
}

// How much of the Fresnel the film takes: the authored coverage, faded out below `FADE_NM`.
fn weight(coverage: f32, thickness: f32) -> f32 {
    let t = clamp(thickness / FADE_NM, 0.0, 1.0);
    return clamp(coverage, 0.0, 1.0) * t * t * (3.0 - 2.0 * t);
}

// The Fresnel at `cos_i`: Schlick of `f0`, or the film's where it covers the surface.
fn fresnel(cos_i: f32, f0: vec3<f32>, cover: f32, ior: f32, thickness: f32) -> vec3<f32> {
    let x = 1.0 - clamp(cos_i, 0.0, 1.0);
    let x2 = x * x;
    let schlick = f0 + (vec3<f32>(1.0) - f0) * (x2 * x2 * x);
    return mix(schlick, reflectance(cos_i, ior, thickness, f0), cover);
}

// The F0 that gives `fresnel` at n·v through Schlick's curve: the proxy for the split-sum
// environment terms and the multi-scatter energy, which take a Schlick F0.
fn schlick_f0(no_v: f32, f0: vec3<f32>, cover: f32, ior: f32, thickness: f32) -> vec3<f32> {
    let x = 1.0 - clamp(no_v, 0.0, 1.0);
    let x2 = x * x;
    let fc = x2 * x2 * x;
    let film = reflectance(no_v, ior, thickness, f0);
    let proxy = clamp((film - vec3<f32>(fc)) / max(1.0 - fc, 0.05), vec3<f32>(0.0), vec3<f32>(1.0));
    return mix(f0, proxy, cover);
}
