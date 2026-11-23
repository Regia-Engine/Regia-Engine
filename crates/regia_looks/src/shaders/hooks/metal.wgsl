// SPDX-License-Identifier: MPL-2.0
// regia_looks metal hook (ADR-2538): F0 + F82 tint (Hoffman 2023), the studio at the roughness mip
// with the viewer's split-sum table, three key-light highlights; an anodize table row in place of
// the Fresnel term when material.p0.w >= 0.
// base: F0 rgb, roughness. p0: F82 tint rgb, anodize row (-1 none). p1: x = grain size mm (0 none).

fn looks_schlick(f0: vec3<f32>, c: f32) -> vec3<f32> {
    let m = pow(1.0 - clamp(c, 0.0, 1.0), 5.0);
    return f0 + (vec3<f32>(1.0) - f0) * m;
}

fn looks_f82(f0: vec3<f32>, tint: vec3<f32>, cos_t: f32) -> vec3<f32> {
    let mu_bar = 1.0 / 7.0;
    let denom = mu_bar * pow(1.0 - mu_bar, 6.0);
    let a = looks_schlick(f0, mu_bar) * (vec3<f32>(1.0) - tint) / denom;
    let c = clamp(cos_t, 0.0, 1.0);
    return clamp(looks_schlick(f0, c) - a * c * pow(1.0 - c, 6.0), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn looks_metal_f(cos_t: f32) -> vec3<f32> {
    let anodized = material.p0.w >= 0.0;
    return select(looks_f82(material.base.rgb, material.p0.rgb, cos_t), lut_sample(max(material.p0.w, 0.0), cos_t), anodized);
}

fn shade_surface(s: Surface) -> vec3<f32> {
    let rough = antialiased_roughness(s.normal, clamp(material.base.w, 0.02, 1.0));
    // A blasted finish (p1.x = grain mm): pits shade the studio, grains catch the key lights.
    let grain_mm = material.p1.x;
    let footprint = length(fwidth(s.local_pos)) / max(grain_mm, 1e-3);
    var key_n = s.normal;
    var key_rough = rough;
    var pits = 1.0;
    var key_pits = 1.0;
    if (grain_mm > 0.0) {
        let tilt = looks_grain_tilt(s.local_pos, grain_mm) * looks_grain_fade(footprint);
        key_n = normalize(s.normal + (s.basis * tilt) * 0.3);
        key_rough = rough * 0.85;
        pits = looks_pit_shadow(rough);
        key_pits = mix(1.0, pits, 0.6);
    }
    let r = reflect(-s.view, s.normal);
    let ab = dfg(s.n_dot_v, rough);
    // The viewer's mips follow sqrt(roughness), sharper than the standalone chain: a blasted surface
    // looks further up, or a softbox stays a polished streak on it.
    let env_rough = select(rough, min(rough * 1.5, 1.0), grain_mm > 0.0);
    var c = env_radiance(r, env_rough) * looks_metal_f(s.n_dot_v) * (ab.x + ab.y) * pits;
    for (var k = 0; k < 3; k = k + 1) {
        let l = looks_key_dir(k);
        let h = normalize(s.view + l);
        c = c + frame.key_col[k].rgb * looks_metal_f(max(dot(l, h), 0.0)) * looks_ggx(key_n, s.view, l, key_rough) * key_pits;
    }
    return c;
}
