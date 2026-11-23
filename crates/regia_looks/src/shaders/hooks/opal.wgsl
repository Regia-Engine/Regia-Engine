// SPDX-License-Identifier: MPL-2.0
// regia_looks opal hook (ADR-2538, ADR-2587): a milky, slightly cloudy body and a fine play of color
// (shared/opal_play.wgsl). Fixed 3x3 loop, one studio tap for the body, one for the coat.
// base: body rgb, patch size mm. p0: x = strength of the play.

fn shade_surface(s: Surface) -> vec3<f32> {
    let n = s.normal;
    let v = s.view;
    let f = 0.035 + 0.965 * pow(1.0 - s.n_dot_v, 5.0);
    let v_obj = normalize(transpose(s.basis) * v);
    let cell_mm = max(material.base.w, 0.05);
    let footprint = length(fwidth(s.local_pos.xy)) / cell_mm;
    let play = looks_opal_play(s.local_pos, s.local_normal, v_obj, cell_mm, material.p0.x, footprint);
    let cloud = looks_opal_cloud(s.local_pos.xy);
    let body = material.base.rgb * (0.8 + 0.3 * cloud) * looks_env(n, 1.0) * 0.9;
    let refl = looks_env(reflect(-v, n), 0.2);
    // A lit domain takes the place of the body under it, so color survives on a white stone.
    var c = (body * (1.0 - 0.65 * play.a) + play.rgb) * (1.0 - f) + refl * f;
    for (var k = 0; k < 3; k = k + 1) {
        c = c + frame.key_col[k].rgb * f * looks_ggx(n, v, looks_key_dir(k), 0.08);
    }
    return c;
}
