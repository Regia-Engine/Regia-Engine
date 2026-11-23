// SPDX-License-Identifier: MPL-2.0
// Lightmap lookup (ADR-860, ADR-1556, `regia_render::lightmap`). The includer declares the four
// bindings of `LightmapGpu::bind_group_layout` in its own group:
//
//   @group(N) @binding(0) var lightmap_atlas: texture_2d<f32>;      // RGB9E5, irradiance × exposure
//   @group(N) @binding(1) var lightmap_sampler: sampler;            // linear, clamp to edge
//   @group(N) @binding(2) var<uniform> lightmap_params: LightmapParams;
//   @group(N) @binding(3) var lightmap_direction: texture_2d<f32>;  // RGBA8 unorm, see below
//
// and passes each vertex's second UV set (`uv2`, 0..1 over its mesh's rectangle) with the entry's
// packed `scale_offset` (`regia_asset::lightmap::LightmapEntry::packed_scale_offset`: its page,
// ADR-3300, rides in `z`). Gutters around every chart hold the dilated neighbours, so neither
// filter reads black at chart edges.
//
// Every texture read goes through four functions `regia_render::lightmap::lightmap_lookup_wgsl`
// appends for the binding form (`LIGHTMAP_PAGED_BINDINGS`): `lightmap_page_scale_offset` and
// `lightmap_page` (the in-page transform and the page of a packed `scale_offset`),
// `lightmap_atlas_sample` and `lightmap_direction_sample` (one bilinear tap of a page). With 2-D
// bindings they are the reads of a one-page atlas, unchanged; with texture arrays (one layer a
// page) they pick the entry's layer.
//
// CPU mirrors: `Lightmap::sample` (bilinear), `Lightmap::sample_bicubic`, `Lightmap::sample_direction`
// and `regia_asset::lightmap::shade_directional`.

struct LightmapParams {
    // x: intensity / exposure (decodes the stored texels to lux), y: 0 = the caller keeps its other
    // indirect light, 1 while lightmaps are enabled, 2 when they are and the lightmap holds the
    // direct sun (`sun_direct`, ADR-2024), z: 1 when the direction texture holds the lightmap's
    // directions, w: 1 for bicubic filtering (0 = bilinear).
    scale: vec4<f32>,
    // x: 1 when receivers take their indirect specular from the lightmap (ADR-4762,
    // `LightmapSettings::specular`); y, z, w: 0.
    spec: vec4<f32>,
}

const LIGHTMAP_INV_PI: f32 = 0.31830988618;

// Entry-local UV2 → atlas UV in the entry's page.
fn lightmap_atlas_uv(uv2: vec2<f32>, scale_offset: vec4<f32>) -> vec2<f32> {
    let so = lightmap_page_scale_offset(scale_offset);
    return uv2 * so.xy + so.zw;
}

// Uniform cubic B-spline weights of the four taps around fraction `t` (per axis).
fn lightmap_bspline(t: vec2<f32>) -> array<vec2<f32>, 4> {
    let t2 = t * t;
    let t3 = t2 * t;
    let s = vec2<f32>(1.0) - t;
    return array<vec2<f32>, 4>(
        s * s * s / 6.0,
        (3.0 * t3 - 6.0 * t2 + 4.0) / 6.0,
        (-3.0 * t3 + 3.0 * t2 + 3.0 * t + 1.0) / 6.0,
        t3 / 6.0,
    );
}

// Bicubic B-spline lookup of the stored atlas value with four bilinear taps (Sigg & Hadwiger
// 2005, GPU Gems 2 ch. 20): each tap sits between two texels so the hardware filter applies the
// pair's weight ratio.
fn lightmap_sample_bicubic(uv: vec2<f32>, page: i32) -> vec3<f32> {
    let size = vec2<f32>(textureDimensions(lightmap_atlas, 0));
    let p = uv * size - 0.5;
    let i = floor(p);
    let w = lightmap_bspline(p - i);
    let g0 = w[0] + w[1];
    let g1 = w[2] + w[3];
    // Tap positions in texel space (texel k's centre is at k + 0.5).
    let c0 = (i - 0.5 + w[1] / g0) / size;
    let c1 = (i + 1.5 + w[3] / g1) / size;
    let a = lightmap_atlas_sample(vec2<f32>(c0.x, c0.y), page).rgb;
    let b = lightmap_atlas_sample(vec2<f32>(c1.x, c0.y), page).rgb;
    let c = lightmap_atlas_sample(vec2<f32>(c0.x, c1.y), page).rgb;
    let d = lightmap_atlas_sample(vec2<f32>(c1.x, c1.y), page).rgb;
    return g0.y * (g0.x * a + g1.x * b) + g1.y * (g0.x * c + g1.x * d);
}

// Baked irradiance (lux, linear RGB) at the surface point with this UV2, bicubic or bilinear
// by the params.
fn lightmap_irradiance(uv2: vec2<f32>, scale_offset: vec4<f32>) -> vec3<f32> {
    let uv = lightmap_atlas_uv(uv2, scale_offset);
    let page = lightmap_page(scale_offset);
    var texel: vec3<f32>;
    if (lightmap_params.scale.w > 0.5) {
        texel = lightmap_sample_bicubic(uv, page);
    } else {
        texel = lightmap_atlas_sample(uv, page).rgb;
    }
    return texel * lightmap_params.scale.x;
}

// Decoded direction at this UV2: xyz the luminance-weighted mean light direction (world space,
// length 0..1), w its agreement with the baked normal (0..1).
fn lightmap_direction_at(uv2: vec2<f32>, scale_offset: vec4<f32>) -> vec4<f32> {
    let t = lightmap_direction_sample(lightmap_atlas_uv(uv2, scale_offset), lightmap_page(scale_offset));
    return vec4<f32>(t.xyz * 2.0 - 1.0, t.w);
}

// `irradiance` rebalanced for the unit shading normal `n`: E · max(1 + n · d, 0) / max(1 + w, 1).
// Identity at the baked normal; no change when the light has no dominant direction.
fn lightmap_shade_directional(irradiance: vec3<f32>, direction: vec4<f32>, n: vec3<f32>) -> vec3<f32> {
    let factor = max(1.0 + dot(n, direction.xyz), 0.0) / max(1.0 + direction.w, 1.0);
    return irradiance * factor;
}

// The full lookup for a receiver: irradiance at this UV2 for the unit shading normal `n`
// (directional when the lightmap has directions).
fn lightmap_irradiance_for_normal(uv2: vec2<f32>, scale_offset: vec4<f32>, n: vec3<f32>) -> vec3<f32> {
    let e = lightmap_irradiance(uv2, scale_offset);
    if (lightmap_params.scale.z > 0.5) {
        return lightmap_shade_directional(e, lightmap_direction_at(uv2, scale_offset), n);
    }
    return e;
}

// The atlas texel and its decoded direction at this UV2 (`direction` zero when the params do not
// use directions, which `lightmap_shade_directional` and `lightmap_lobe_radiance` read as light
// from everywhere): one lookup for a receiver's diffuse and specular terms (ADR-4762).
struct LightmapSample {
    e: vec3<f32>,
    direction: vec4<f32>,
}

fn lightmap_sample(uv2: vec2<f32>, scale_offset: vec4<f32>) -> LightmapSample {
    var direction = vec4<f32>(0.0);
    if (lightmap_params.scale.z > 0.5) {
        direction = lightmap_direction_at(uv2, scale_offset);
    }
    return LightmapSample(lightmap_irradiance(uv2, scale_offset), direction);
}

// True when this draw is a lightmap receiver and lightmaps are enabled.
fn lightmap_is_receiver(scale_offset: vec4<f32>) -> bool {
    return lightmap_enabled() && scale_offset.x > 0.0 && scale_offset.y > 0.0;
}

// The receiver rule (ADR-860 step 2, ADR-1556): a draw whose `scale_offset` has a non-zero scale
// is lightmapped and takes its diffuse indirect irradiance from the lightmap while lightmaps are
// enabled; every other draw (dynamic objects, unlightmapped statics, the flag off) keeps
// `fallback` (the probe volume / DDGI irradiance the caller computed).
fn lightmap_receiver_irradiance(uv2: vec2<f32>, scale_offset: vec4<f32>, n: vec3<f32>, fallback: vec3<f32>) -> vec3<f32> {
    if (!lightmap_enabled() || scale_offset.x <= 0.0 || scale_offset.y <= 0.0) {
        return fallback;
    }
    return lightmap_irradiance_for_normal(uv2, scale_offset, n);
}

// Lambertian outgoing radiance of `albedo` lit by `irradiance`.
fn lightmap_diffuse(albedo: vec3<f32>, irradiance: vec3<f32>) -> vec3<f32> {
    return albedo * irradiance * LIGHTMAP_INV_PI;
}

// True when this draw's diffuse sun light is already in the lightmap (baked with `--direct-sun`,
// ADR-2024): the receiver drops the live sun's diffuse term and keeps its specular, which no
// lightmap holds. Every other draw keeps the live sun whole.
fn lightmap_holds_sun_diffuse(scale_offset: vec4<f32>) -> bool {
    return lightmap_params.scale.y > 1.5 && scale_offset.x > 0.0 && scale_offset.y > 0.0;
}

// True while lightmaps are enabled (the flag in `LightmapSettings`).
fn lightmap_enabled() -> bool {
    return lightmap_params.scale.y > 0.5;
}

// ---- Indirect specular of a receiver (ADR-4762) ----
// The rebalance above is the irradiance of one radiance field, linear in the direction:
// L(ω) = E / (π · max(1 + w, 1)) · (1 + 3/2 · ω · d) has E(n) = E · (1 + n · d) / max(1 + w, 1) for
// every normal (the cosine lobe's first moment is 2/3 of its zeroth). The specular reads the
// same field in the GGX lobe: for radiance linear in ω the lobe's integral is the radiance at
// the lobe's mean vector `m` (`probe_spec_lobe`, ADR-1103), exactly. A mirror's `m` is the
// mirror direction (length ~1), a rough lobe's leans to the normal and shortens, so its light
// tends to the field's mean E / (π · max(1 + w, 1)). Without directions the field is uniform,
// E / π, at every roughness. The caller reserves the reflected energy with
// `lightmap_diffuse_weight` before adding the two lobes.
const LIGHTMAP_SPEC_GRADIENT: f32 = 1.5;

// Radiance (cd/m², linear RGB) the receiver's lobe of mean vector `m` (|m| ≤ 1) sees in the
// baked light at this UV2. Directions are read when the params use them.
fn lightmap_specular_radiance(uv2: vec2<f32>, scale_offset: vec4<f32>, m: vec3<f32>) -> vec3<f32> {
    let s = lightmap_sample(uv2, scale_offset);
    return lightmap_lobe_radiance(s.e, s.direction, m);
}

// The field's radiance at the lobe mean `m` for irradiance `e` and decoded direction `direction`.
fn lightmap_lobe_radiance(e: vec3<f32>, direction: vec4<f32>, m: vec3<f32>) -> vec3<f32> {
    let level = max(1.0 + LIGHTMAP_SPEC_GRADIENT * dot(m, direction.xyz), 0.0) / max(1.0 + direction.w, 1.0);
    return e * (level * LIGHTMAP_INV_PI);
}

// The GGX lobe's mean vector for a shader without `probe_spec_lobe` (the lite shader): its
// direction leans from the normal to the mirror direction by Lagarde & de Rousiers 2014's
// (1 − α)(√(1 − α) + α), its length is the ADR-1103 fit at n · v = 1 (the view term dropped).
fn lightmap_spec_lobe(n: vec3<f32>, r: vec3<f32>, alpha: f32) -> vec3<f32> {
    let a = clamp(alpha, 0.0, 1.0);
    let x = 1.0 - a;
    let d = mix(n, r, x * (sqrt(x) + a));
    let len = (1.0 + a * (3.673155 + 7.571242 * a)) / (1.0 + a * (3.719715 + 15.010073 * a));
    return d * (inverseSqrt(max(dot(d, d), 1e-12)) * len);
}

// True when this draw is a receiver and takes its indirect specular from the lightmap.
fn lightmap_specular_on(scale_offset: vec4<f32>) -> bool {
    return lightmap_is_receiver(scale_offset) && lightmap_params.spec.x > 0.5;
}

// Partition the uniform field's energy between the diffuse and split-sum specular lobes.
fn lightmap_diffuse_weight(f0: vec3<f32>, ab: vec2<f32>) -> vec3<f32> {
    return vec3<f32>(1.0) - clamp(f0 * ab.x + vec3<f32>(ab.y), vec3<f32>(0.0), vec3<f32>(1.0));
}
