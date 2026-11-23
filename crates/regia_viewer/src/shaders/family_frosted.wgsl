// SPDX-License-Identifier: MPL-2.0
// The default frosted stone: rough reflection over a rough, tinted look through. A starting point for
// the looks lane. material.base: tint, roughness. material.p0: ior.

fn shade_surface(s: Surface) -> vec3<f32> {
    let ior = material.p0.x;
    let roughness = max(material.base.w, 0.35);
    let fresnel = fresnel_dielectric(s.n_dot_v, ior);
    let reflection = env_radiance(reflect(-s.view, s.normal), roughness);
    let through = env_radiance(refract(-s.view, s.normal, 1.0 / ior), min(roughness + 0.2, 1.0));
    return reflection * fresnel + through * material.base.rgb * (1.0 - fresnel);
}
