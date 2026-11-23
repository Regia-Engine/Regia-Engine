// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::pbr::brdf

#import regia::pbr::lighting
#import regia::pbr::thin_film

// The forward pass's isotropic direct BRDF, shared with the reference path tracer
// (`regia_pt`, `specs/27` § 1.1): Lambert diffuse weighted by 1 − F(l·h), GGX with the
// height-correlated Smith term, the KHR_materials_clearcoat layer and Turquin's 2019
// multi-scatter energy compensation. The cosine factor is not included. `lab_forward.wgsl` and the PT kernels
// both build their direct term from these functions, so a BRDF change moves both renderers and
// the PT's "forward == PT direct" test (`specs/27` § 1.5 item 4) keeps them honest.

struct BrdfLobes {
    diffuse: vec3<f32>,
    specular: vec3<f32>,
    // Schlick Fresnel at l·h, kept for callers that swap in an anisotropic specular.
    fresnel: vec3<f32>,
    // Clearcoat attenuation of the base layer (1 without a coat).
    coat_att: f32,
}

fn isotropic_lobes(
    no_v: f32,
    no_l: f32,
    no_h: f32,
    lo_h: f32,
    diffuse_color: vec3<f32>,
    f0: vec3<f32>,
    rough: f32,
) -> BrdfLobes {
    let d = regia::pbr::lighting::d_ggx(no_h, rough);
    let fresnel = regia::pbr::lighting::f_schlick(lo_h, f0);
    let vis = regia::pbr::lighting::v_smith_ggx_correlated(no_v, no_l, rough);
    let spec = d * vis * fresnel;
    let diff = diffuse_color * (1.0 / regia::pbr::lighting::PI) * (1.0 - fresnel);
    return BrdfLobes(diff, spec, fresnel, 1.0);
}

// `isotropic_lobes` with the thin film's Fresnel at l·h (`thin_film.wgsl`): `cover` is its weight,
// `f0` the base under the film.
fn isotropic_lobes_film(
    no_v: f32,
    no_l: f32,
    no_h: f32,
    lo_h: f32,
    diffuse_color: vec3<f32>,
    f0: vec3<f32>,
    rough: f32,
    cover: f32,
    ior: f32,
    thickness: f32,
) -> BrdfLobes {
    let d = regia::pbr::lighting::d_ggx(no_h, rough);
    let fresnel = regia::pbr::thin_film::fresnel(lo_h, f0, cover, ior, thickness);
    let vis = regia::pbr::lighting::v_smith_ggx_correlated(no_v, no_l, rough);
    let spec = d * vis * fresnel;
    let diff = diffuse_color * (1.0 / regia::pbr::lighting::PI) * (1.0 - fresnel);
    return BrdfLobes(diff, spec, fresnel, 1.0);
}

// Dielectric coat (F0 0.04) over the base: the base is attenuated by 1 − coat·F(n·v) and the
// coat's own GGX lobe is added to the specular.
fn with_clearcoat(
    lobes: BrdfLobes,
    no_v: f32,
    no_l: f32,
    no_h: f32,
    lo_h: f32,
    coat: f32,
    coat_roughness: f32,
) -> BrdfLobes {
    let coat_att = regia::pbr::lighting::clearcoat_attenuation(no_v, coat);
    let spec = lobes.specular * coat_att;
    let diff = lobes.diffuse * coat_att;
    let coat_a = max(coat_roughness * coat_roughness, regia::pbr::lighting::MIN_ALPHA);
    let dc = regia::pbr::lighting::d_ggx(no_h, coat_a);
    let vc = regia::pbr::lighting::v_smith_ggx_correlated(no_v, no_l, coat_a);
    let fc = regia::pbr::lighting::f_schlick_f32(lo_h, 0.04);
    return BrdfLobes(diff, spec + vec3<f32>(coat * dc * vc * fc), lobes.fresnel, coat_att);
}

// Multi-scatter energy compensation of the base specular (Turquin 2019, see
// `pbr_lighting::energy_compensation`) and the diffuse under it. `e_o` is the directional albedo
// of the F = 1 single-scatter lobe at n·v (the DFG table's B channel), `e_avg` its hemispherical
// average (the A channel); `es_o` the specular
// layer's albedo with Fresnel at n·v (`F0 · scale + bias`). The diffuse becomes
// `diffuse_color / π · (1 − es_o)`: the share of the light the specular layer does not reflect
// toward the eye (albedo scaling, Kulla & Conty 2017), instead of Lambert · (1 − F(l·h)), which
// starves a rough dielectric lit from behind (l·h small at a back-lit rim). Apply it to the base
// lobes before `with_clearcoat`, which adds the coat's own lobe to the specular.
fn with_multi_scatter(lobes: BrdfLobes, f0: vec3<f32>, e_o: f32, e_avg: f32, diffuse_color: vec3<f32>, es_o: vec3<f32>) -> BrdfLobes {
    let spec = lobes.specular * regia::pbr::lighting::energy_compensation(f0, e_o, e_avg);
    let diff = diffuse_color * (1.0 / regia::pbr::lighting::PI) * (vec3<f32>(1.0) - es_o);
    return BrdfLobes(diff, spec, lobes.fresnel, lobes.coat_att);
}

// `BrdfEnergy::Compensated` (ADR-2632; Kulla & Conty 2017, "Revisiting Physically Based Shading
// at Imageworks", SIGGRAPH course notes; CPU mirror `regia_render::brdf_energy`). The forward
// pass calls these only under `BRDF_ENERGY_COMPENSATED` and the path tracer only with
// `globals.dfg.z == 1`; the default frame never does.

// The Fresnel-weighted single-scatter albedo E_F(μ) from a DFG sample `s` at μ (scale, bias, E,
// E_avg): the split sum's Fresnel share of the height-correlated lobe's albedo E.
fn single_scatter_albedo(f0: vec3<f32>, s: vec4<f32>) -> vec3<f32> {
    let split = s.x + s.y;
    if (split <= 1e-4) {
        return vec3<f32>(0.0);
    }
    return clamp((f0 * s.x + vec3<f32>(s.y)) / split * s.z, vec3<f32>(0.0), vec3<f32>(1.0));
}

// F_add = F_avg² E_avg / (1 − F_avg (1 − E_avg)): the share of the single-scatter lobe's lost
// energy a conductor with Schlick average F_avg returns over every further bounce.
fn multi_scatter_fresnel(f0: vec3<f32>, e_avg: f32) -> vec3<f32> {
    let favg = regia::pbr::lighting::fresnel_average(f0);
    let ea = clamp(e_avg, 0.0, 1.0);
    return favg * favg * ea / max(vec3<f32>(1.0) - favg * (1.0 - ea), vec3<f32>(1e-4));
}

// `regia_render::brdf_energy::fresnel_bias_average`: the cosine-weighted average of the Fresnel-
// bias part of `single_scatter_albedo` at perceptual roughness `perceptual`, a degree-6 fit of the
// DFG table's own average (smooth limit 1/21). The coefficients are the CPU mirror's
// `BIAS_AVERAGE_FIT`, highest power first.
fn fresnel_bias_average(perceptual: f32) -> f32 {
    let r = clamp(perceptual, 0.0, 0.984375);
    var acc = 1.2292322;
    acc = acc * r - 3.8261223;
    acc = acc * r + 4.299933;
    acc = acc * r - 1.9304305;
    acc = acc * r + 0.18521781;
    acc = acc * r - 0.0021520231;
    return acc * r + 0.047328003;
}

// The compensated base lobes: the single-scatter specular plus the reciprocal multiple-scattering
// lobe F_add (1 − E(μo)) (1 − E(μi)) / (π (1 − E_avg)) (with `multi_scatter`), and the diffuse
// coupled to the whole specular layer on both sides,
// ρ/π (1 − E_spec(μo)) (1 − E_spec(μi)) / (1 − E_spec,avg), E_spec,avg = F0 (E_avg − b̄) + b̄ +
// F_add (1 − E_avg). `so` and `si` are the DFG samples at n·v and n·l, `perceptual` their row. Replaces both `isotropic_lobes`' 1 − F(l·h) diffuse and `with_multi_scatter`;
// apply it before `with_clearcoat`.
fn with_compensation(lobes: BrdfLobes, f0: vec3<f32>, diffuse_color: vec3<f32>, perceptual: f32, so: vec4<f32>, si: vec4<f32>, multi_scatter: bool) -> BrdfLobes {
    let ea = clamp(so.w, 0.0, 1.0);
    var f_add = vec3<f32>(0.0);
    if (multi_scatter) {
        f_add = multi_scatter_fresnel(f0, ea);
    }
    let e_o = clamp(so.z, 0.0, 1.0);
    let e_i = clamp(si.z, 0.0, 1.0);
    let inv_pi = 1.0 / regia::pbr::lighting::PI;
    // One 8-bit step of headroom on 1 − E_avg (`MS_NORMALIZATION_MARGIN`): the lobe can only lose.
    let ms = f_add * ((1.0 - e_o) * (1.0 - e_i) * inv_pi / (1.0 - ea + 1.0 / 255.0));
    let spec_o = min(single_scatter_albedo(f0, so) + f_add * (1.0 - e_o), vec3<f32>(1.0));
    let spec_i = min(single_scatter_albedo(f0, si) + f_add * (1.0 - e_i), vec3<f32>(1.0));
    let bias = fresnel_bias_average(perceptual);
    let spec_avg = min(f0 * max(ea - bias, 0.0) + vec3<f32>(bias) + f_add * (1.0 - ea), vec3<f32>(1.0));
    let diff = diffuse_color * (vec3<f32>(1.0) - spec_o) * (vec3<f32>(1.0) - spec_i) * inv_pi
        / max(vec3<f32>(1.0) - spec_avg, vec3<f32>(1e-4));
    return BrdfLobes(diff, lobes.specular + ms, lobes.fresnel, lobes.coat_att);
}

// The share of the diffuse environment light the specular layer leaves: 1 − (F0·scale + bias)
// − ms, where ms is the split sum's multiple scattering (`multi_scatter_ems`, with
// `multi_scatter`). `dfg` is the DFG sample's (scale, bias) at n·v.
fn compensated_env_diffuse(f0: vec3<f32>, dfg: vec2<f32>, multi_scatter: bool) -> vec3<f32> {
    let fss = f0 * dfg.x + vec3<f32>(dfg.y);
    var ms = vec3<f32>(0.0);
    if (multi_scatter) {
        ms = regia::pbr::lighting::multi_scatter_ems(f0, dfg);
    }
    return clamp(vec3<f32>(1.0) - fss - ms, vec3<f32>(0.0), vec3<f32>(1.0));
}

// `compensated_env_diffuse` where only part of the environment's specular is actually added:
// `spec_cover` is the share of the diffuse environment light whose split-sum specular the shader
// adds beside it, `ms_cover` the share whose multiple scattering it adds. The diffuse gives up
// only what a specular term returns: 1 − spec_cover·(F0·scale + bias) − ms_cover·ms. A flat
// ambient with no reflection (cover 0) keeps the full diffuse; cover 1 is
// `compensated_env_diffuse` (ADR-2632).
fn compensated_env_diffuse_covered(f0: vec3<f32>, dfg: vec2<f32>, spec_cover: f32, ms_cover: f32) -> vec3<f32> {
    let fss = f0 * dfg.x + vec3<f32>(dfg.y);
    let ms = regia::pbr::lighting::multi_scatter_ems(f0, dfg);
    let sc = clamp(spec_cover, 0.0, 1.0);
    let mc = clamp(ms_cover, 0.0, 1.0);
    return clamp(vec3<f32>(1.0) - fss * sc - ms * mc, vec3<f32>(0.0), vec3<f32>(1.0));
}
