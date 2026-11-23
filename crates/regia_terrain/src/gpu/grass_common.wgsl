// SPDX-License-Identifier: MPL-2.0
// Grass field (ADR-1400): structs and the hash shared by the compute generator
// (`grass_cull.wgsl`, which also prepends `grass_place.wgsl`) and the blade draw (`grass.wgsl`).
//
// Hash: the PCG-style integer hash of Jarzynski & Olano 2020 ("Hash Functions for GPU
// Rendering", JCGT 9(3)).

struct GrassParams {
    // Camera frustum planes (outward, unjittered).
    planes: array<vec4<f32>, 6>,
    // xyz eye, w = sway clock (s).
    eye: vec4<f32>,
    // half_width, world_width, sample_max.x, sample_max.y (the terrain's mapping).
    map: vec4<f32>,
    // height_scale, height_offset, patch edge (m), previous sway clock (s).
    heights: vec4<f32>,
    // full-density distance (m), density scale, max widen, sway scale.
    law: vec4<f32>,
    // near → mid LOD (m), mid → far LOD (m), max blades, debug view.
    lod: vec4<f32>,
    // Camera-body wake (ADR-1502, `regia_core::math::wake`): body now (position, gust at the
    // body; velocity, sin μ), the frame before (the same two), shape (radius, falloff, range,
    // pulse s), (max gust, radial share, enabled, 0), grass response (bend, half-bend m/s, 0, 0).
    wake: array<vec4<f32>, 7>,
    // patch count, shadows on (1/0), segments near, segments mid.
    counts: vec4<u32>,
    // capacity near, mid, far, shadow (clumps per region).
    caps: vec4<u32>,
    // segments far, wind style (0 classic, 1 natural; ADR-2446), unused ×2.
    counts2: vec4<u32>,
    // Wind (ADR-1853): direction x, z (normalised here), frequency scale, gust weight.
    wind: vec4<f32>,
    // Foliage transmission (ADR-3170): x = on (1/0), y = blade thickness (mm), zw unused.
    subsurface: vec4<f32>,
    // The foliage profile's two Gaussians: rgb = weight per channel (normalised over both),
    // w = variance (mm²). Always filled, so debug view 4 reads them with the switch off.
    subsurface_g0: vec4<f32>,
    subsurface_g1: vec4<f32>,
    // Blast gust (ADR-1497): centre x, centre z, front radius (m), bend (fraction of the blade
    // height; 0 = no gust).
    gust: vec4<f32>,
    // Front radius the frame before (m), positive-phase length (m), unused ×2.
    gust2: vec4<f32>,
}

struct GrassClassGpu {
    // blade height min, max, root width, clump radius (m).
    shape: vec4<f32>,
    // lean (rad), bend (0..1), green height (×), blades.
    shape2: vec4<f32>,
    // stem root colour (linear), roughness.
    base: vec4<f32>,
    // stem tip colour, translucency.
    tip: vec4<f32>,
    // shoot root colour, root occlusion.
    green: vec4<f32>,
    // shoot tip colour, colour jitter.
    green_tip: vec4<f32>,
    // range (m), shadow distance (m, 0 = none), sway (m), sway speed (Hz).
    misc: vec4<f32>,
    // stream seed, mask layer, unused ×2.
    seed: vec4<u32>,
}

struct GrassClasses {
    items: array<GrassClassGpu, 8>,
}

// One clump, written by `cs_write` (32 bytes).
struct GrassInst {
    pos: vec3<f32>,
    hash: u32,
    // unpack2x16unorm: widen / 8, fade.
    shape: u32,
    // pack4x8unorm: greenness, height / 2, tint, LOD blend.
    look: u32,
    // pack2x16snorm: terrain normal x, z (y is positive).
    normal: u32,
    // class index (low 8 bits), patch level (bits 8..11).
    class_level: u32,
}

const GRASS_TAU: f32 = 6.28318530718;

fn grass_pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn grass_unit(h: u32) -> f32 {
    return f32(h >> 8u) / 16777216.0;
}
