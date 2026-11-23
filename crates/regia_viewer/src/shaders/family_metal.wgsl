// SPDX-License-Identifier: MPL-2.0
// Metal and anodized metal: base color + roughness under the environment.
// material.base: rgb reflectance, roughness. material.p0: x = film mode (1 = lookup), y = lookup row.

fn shade_surface(s: Surface) -> vec3<f32> {
    let roughness = antialiased_roughness(s.normal, material.base.w);
    var f0 = material.base.rgb;
    if (material.p0.x > 0.5) {
        // A baked cos(theta) to reflectance table: the view angle changes the film's color.
        f0 = lut_sample(material.p0.y, s.n_dot_v);
    }
    return env_specular(s, f0, roughness);
}
