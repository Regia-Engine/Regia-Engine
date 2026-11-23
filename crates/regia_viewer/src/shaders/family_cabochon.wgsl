// SPDX-License-Identifier: MPL-2.0
// The default cabochon: a sharp coat over a soft body. A starting point for the looks lane.
// material.base: body, roughness. material.p0: ior, translucency.

fn shade_surface(s: Surface) -> vec3<f32> {
    let ior = material.p0.x;
    let translucency = material.p0.y;
    let roughness = antialiased_roughness(s.normal, material.base.w);

    let fresnel = fresnel_dielectric(s.n_dot_v, ior);
    let coat = env_radiance(reflect(-s.view, s.normal), roughness);
    let through = env_radiance(refract(-s.view, s.normal, 1.0 / ior), 0.7);
    let body = material.base.rgb * mix(env_diffuse(s.normal), through, translucency);
    return coat * fresnel + body * (1.0 - fresnel);
}
