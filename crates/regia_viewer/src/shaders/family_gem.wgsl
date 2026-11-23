// SPDX-License-Identifier: MPL-2.0
// The default cut stone. A starting point for the looks lane, which replaces this file's body.
// material.base: tint, roughness. material.p0: ior, dispersion, sparkle, seed.
//
// No ray tracing and no bounce loop: a reflection, one faked refraction per color channel, and a stable
// per-facet glint.

fn facet_glint(s: Surface, light: i32) -> vec3<f32> {
    let half_vector = normalize(frame.key_dir[light].xyz + s.view);
    let aligned = max(dot(s.normal, half_vector), 0.0);
    return frame.key_col[light].rgb * pow(aligned, 512.0);
}

fn shade_surface(s: Surface) -> vec3<f32> {
    let ior = material.p0.x;
    let dispersion = material.p0.y;
    let sparkle = material.p0.z;
    let seed = material.p0.w;
    let roughness = material.base.w;

    let fresnel = fresnel_dielectric(s.n_dot_v, ior);
    let reflection = env_radiance(reflect(-s.view, s.normal), roughness);

    // Each facet looks into the stone along its own refracted ray, turned back toward the viewer by a
    // facet-dependent amount: a cheap stand-in for the light that leaves through the crown.
    let spread = 0.03 + 0.12 * dispersion;
    let turn = normalize(vec3<f32>(hash11(s.facet + seed) - 0.5, hash11(s.facet + seed + 7.0) - 0.5, 0.5));
    let inner = s.normal + turn * 0.6;
    let r_dir = refract(-s.view, normalize(inner), 1.0 / (ior - spread));
    let g_dir = refract(-s.view, normalize(inner), 1.0 / ior);
    let b_dir = refract(-s.view, normalize(inner), 1.0 / (ior + spread));
    let transmitted = vec3<f32>(
        env_radiance(reflect(r_dir, s.normal), roughness).r,
        env_radiance(reflect(g_dir, s.normal), roughness).g,
        env_radiance(reflect(b_dir, s.normal), roughness).b,
    ) * material.base.rgb;

    // A narrow highlight of each fixed studio light on the facet: stable while still, flashing as the
    // view turns. Three lights, unrolled: no loop.
    let glint = (facet_glint(s, 0) + facet_glint(s, 1) + facet_glint(s, 2)) * sparkle;

    return reflection * fresnel + transmitted * (1.0 - fresnel) + glint;
}
