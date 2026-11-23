// SPDX-License-Identifier: MPL-2.0
// regia_looks cabochon hook (ADR-2538): smooth dome; a sharp coat, one blurred look-up through the
// stone off its flat base, a little body glow. No dispersion, no glints beyond the coat's keys.
// base: attenuation per mm rgb, roughness. p0: n_d, Abbe, fire, path mm. p1: dome height, half
// length, half width (mm).

fn shade_surface(s: Surface) -> vec3<f32> {
    let n = s.normal;
    let v = s.view;
    let n_d = material.p0.x;
    let f0 = pow((n_d - 1.0) / (n_d + 1.0), 2.0);
    let f = f0 + (1.0 - f0) * pow(1.0 - s.n_dot_v, 5.0);
    let rough = material.base.w;
    let refl = env_radiance(reflect(-v, n), rough);
    // Into the dome, off the flat base, out again.
    let n_obj = s.local_normal;
    let t = refract(-normalize(transpose(s.basis) * v), n_obj, 1.0 / n_d);
    let back = reflect(t, vec3<f32>(0.0, 0.0, -1.0));
    // Down to the base and back up: about four dome heights at the center.
    let thick = material.p1.x * (1.5 + 3.0 * max(n_obj.z, 0.0));
    let tint = exp(-material.base.rgb * thick);
    let through = looks_env(normalize(s.basis * back), 0.8 + 0.2 * rough);
    let glow = looks_env(n, 1.0) * tint * 0.15;
    // The back of a set cabochon sits in metal: only part of the light comes back through.
    var c = refl * f + (through * tint * 0.3 + glow) * (1.0 - f);
    for (var k = 0; k < 3; k = k + 1) {
        c = c + frame.key_col[k].rgb * f * looks_ggx(n, v, looks_key_dir(k), max(rough, 0.05));
    }
    return c;
}
