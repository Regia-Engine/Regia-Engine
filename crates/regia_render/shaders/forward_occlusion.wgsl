// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::occlusion

// Ambient and specular occlusion: multi-bounce AO, the bent-normal cone, and cone-cap intersection.
// Split out of `lab_forward.wgsl` unchanged (ADR-1552); imported there by name.

// Multi-bounce ambient occlusion: Jimenez, Wu, Pesce, Jarabek, "Practical Realtime
// Strategies for Accurate Indirect Occlusion", SIGGRAPH 2016 (cubic fit, eq. 10).
// Bright surfaces lose less light to occlusion than single-bounce AO predicts.
fn multi_bounce_ao(visibility: f32, albedo: vec3<f32>) -> vec3<f32> {
    let a = 2.0404 * albedo - vec3<f32>(0.3324);
    let b = -4.7951 * albedo + vec3<f32>(0.6417);
    let c = 2.7552 * albedo + vec3<f32>(0.6903);
    let x = vec3<f32>(visibility);
    return max(x, ((x * a + b) * x + c) * x);
}

// Specular occlusion from ambient occlusion: Lagarde & de Rousiers, "Moving Frostbite
// to Physically Based Rendering", SIGGRAPH 2014 course notes § 4.10.2.
fn specular_occlusion(no_v: f32, ao: f32, roughness: f32) -> f32 {
    return clamp(pow(no_v + ao, exp2(-16.0 * roughness - 1.0)) - 1.0 + ao, 0.0, 1.0);
}

// The narrowest specular cone (radians, about 11°): the visibility cone from a scalar AO is no
// sharper than that, and a mirror-thin cone turned its edge into a hard line along wall bases.
const SPEC_CONE_MIN_ANGLE: f32 = 0.2;

// The lean from its codes. One code either side of 128 reads as 0, so a driver that rounds the
// storage conversion down still writes an exact zero lean.
fn bent_decode(enc: vec3<f32>) -> vec3<f32> {
    let d = (round(enc * 255.0) - vec3<f32>(128.0)) / 127.0;
    return select(d, vec3<f32>(0.0), abs(d) < vec3<f32>(1.5 / 127.0));
}

// The shading normal `n` leaned by `lean` (moved from the depth normal to the normal-mapped one);
// a sum that falls below the surface keeps `n`.
fn bent_shading_normal(n: vec3<f32>, lean: vec3<f32>) -> vec3<f32> {
    let b = n + lean;
    if (dot(b, n) <= 1e-3) {
        return n;
    }
    return normalize(b);
}

// Half-angle (radians) of the cone standing for a GGX lobe of roughness `alpha` around the mirror
// direction: twice the half-vector angle at which the distribution falls to half its peak,
// cos²θ = (1 − √2 α²) / (1 − α²); a lobe that never halves inside the hemisphere is the hemisphere.
fn spec_cone_half_angle(alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let c2 = (1.0 - 1.4142135 * a2) / max(1.0 - a2, 1e-6);
    if (!(c2 > 0.0)) {
        return 1.5707964;
    }
    return clamp(2.0 * acos(sqrt(min(c2, 1.0))), SPEC_CONE_MIN_ANGLE, 1.5707964);
}

// Solid angle of a spherical cap of half-angle `a`.
fn cap_solid_angle(a: f32) -> f32 {
    return 6.2831853 * (1.0 - cos(a));
}

// Approximate solid angle of the intersection of caps of half-angles `a1`, `a2` whose axes are `d`
// apart: exact when one holds the other, zero when apart, a smoothstep between.
fn cap_intersection(a1: f32, a2: f32, d: f32) -> f32 {
    let full = cap_solid_angle(min(a1, a2));
    let diff = abs(a1 - a2);
    if (d <= diff) {
        return full;
    }
    if (d >= a1 + a2) {
        return 0.0;
    }
    let x = 1.0 - clamp((d - diff) / max(a1 + a2 - diff, 1e-6), 0.0, 1.0);
    return full * x * x * (3.0 - 2.0 * x);
}

// The share of the specular cone (mirror direction `r`, roughness `alpha`) inside the visibility
// cone around `bent` that keeps `ao` (cos α_v = √(1 − ao)), over its share above the horizon of
// the normal `n` (the split-sum BRDF already integrates that part), so an open surface keeps 1.
fn cone_specular_occlusion(bent: vec3<f32>, ao: f32, n: vec3<f32>, r: vec3<f32>, alpha: f32) -> f32 {
    let spec = spec_cone_half_angle(alpha);
    let hemi = acos(0.0);
    let vis = acos(sqrt(clamp(1.0 - ao, 0.0, 1.0)));
    let seen = cap_intersection(vis, spec, acos(clamp(dot(bent, r), -1.0, 1.0)));
    let open = cap_intersection(hemi, spec, acos(clamp(dot(n, r), -1.0, 1.0)));
    if (open <= 1e-6) {
        return 1.0;
    }
    return clamp(seen / open, 0.0, 1.0);
}
