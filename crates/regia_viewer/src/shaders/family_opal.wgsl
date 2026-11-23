// SPDX-License-Identifier: MPL-2.0
// The default opal. A starting point for the looks lane.
// material.base: body, roughness. material.p0: fire, patch size in mm, seed.

fn shade_surface(s: Surface) -> vec3<f32> {
    let fire = material.p0.x;
    let scale = material.p0.y;
    let seed = material.p0.z;
    let roughness = antialiased_roughness(s.normal, material.base.w);

    let cell = floor(s.local_pos / scale + vec3<f32>(seed));
    let pick = hash11(cell.x * 13.0 + cell.y * 7.0 + cell.z * 3.0);
    let phase = pick * 6.2831 + dot(s.normal, s.view) * 9.0;
    let play = vec3<f32>(0.5) + 0.5 * vec3<f32>(sin(phase), sin(phase + 2.094), sin(phase + 4.188));

    let fresnel = fresnel_dielectric(s.n_dot_v, 1.45);
    let body = material.base.rgb * env_diffuse(s.normal) + play * fire * 0.35;
    let coat = env_radiance(reflect(-s.view, s.normal), roughness) * fresnel;
    return body * (1.0 - fresnel) + coat;
}
