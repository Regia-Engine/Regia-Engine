// SPDX-License-Identifier: MPL-2.0
// Grass blades (ADR-1400): one instanced indexed draw per LOD region (near, mid, far, shadow)
// over the clumps `grass_cull.wgsl` wrote. No vertex buffer: the vertex index picks the blade
// and the point on it, and the blade's shape is drawn from the clump's hash, so every blade is
// real tapered geometry (no alpha-tested cards).
//
// Each blade is a quadratic Bézier centre line (root, a control point along the lean, a tip
// bent further by `bend`), `segments` rings of two vertices tapering to a single tip vertex.
// Near clumps draw every blade with up to four segments; mid and far clumps draw a leading
// subset of the same blades, wider, with fewer segments, so a clump keeps its silhouette across
// the switch. Blades are lit like the ground they stand on (the engine cascades, SSAO, the sky
// irradiance, height fog and aerial perspective; `impostor.wgsl` is the model), plus light
// through a back-lit blade and a root occlusion. Prepended at load: `terrain.wgsl`,
// `grass_common.wgsl`.

// Group 2 bindings start at 24: `terrain.wgsl` (prepended) declares its node lists at 0..2 and the
// impostor draw uses 8..14.
struct GrassLod {
    // Region (0 near, 1 mid, 2 far, 3 shadow), blades drawn, segments, instance base.
    region: u32,
    blades: u32,
    segments: u32,
    base: u32,
}

@group(2) @binding(24) var<uniform> grass_params: GrassParams;
@group(2) @binding(25) var<storage, read> grass_instances: array<GrassInst>;
@group(2) @binding(26) var<uniform> grass_classes: GrassClasses;
@group(2) @binding(27) var<uniform> grass_lod: GrassLod;

struct GrassVtx {
    world: vec3<f32>,
    // Blade face normal, rounded across the width.
    normal: vec3<f32>,
    // Ground normal under the clump.
    ground: vec3<f32>,
    // 0 at the root, 1 at the tip.
    s: f32,
    albedo: vec3<f32>,
    // Root occlusion (1 = none).
    ao: f32,
    kind: u32,
    debug: vec3<f32>,
    // Height above the clump root and the clump's canopy top (tallest blade), metres.
    canopy: vec2<f32>,
    alive: bool,
}

fn grass_next(h: u32) -> u32 {
    return grass_pcg(h ^ 0x68e31da4u);
}

// Blades a clump of `blades` blades draws in `region` (near = all, mid half, far a quarter).
fn grass_region_blades(cls_blades: u32, region: u32) -> u32 {
    let near = max(min(cls_blades, u32(grass_params.lod.z)), 1u);
    switch region {
        case 0u: { return near; }
        case 2u: { return max((near + 3u) / 4u, 1u); }
        default: { return max((near + 1u) / 2u, 1u); }
    }
}

fn grass_dead_vtx(kind: u32) -> GrassVtx {
    let z = vec3<f32>(0.0);
    return GrassVtx(z, vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), 0.0, z, 1.0, kind, z, vec2<f32>(0.0), false);
}

// The natural wind style (ADR-2446), the GPU twin of `regia_terrain::wind_natural`: keep in step.
fn grass_lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    return grass_unit(grass_pcg((bitcast<u32>(ix) * 0x9E3779B1u) ^ grass_pcg(bitcast<u32>(iy) ^ seed)));
}

fn grass_vnoise(p: vec2<f32>, seed: u32) -> f32 {
    let f = floor(p);
    let t = p - f;
    let s = t * t * (3.0 - 2.0 * t);
    let ix = i32(f.x);
    let iy = i32(f.y);
    let a = grass_lattice(ix, iy, seed);
    let b = grass_lattice(ix + 1, iy, seed);
    let c = grass_lattice(ix, iy + 1, seed);
    let d = grass_lattice(ix + 1, iy + 1, seed);
    let top = a + (b - a) * s.x;
    let bottom = c + (d - c) * s.x;
    return top + (bottom - top) * s.y;
}

fn grass_natural_wave(pos: vec2<f32>, dir: vec2<f32>, jitter: f32, hz: f32, height_m: f32, flutter_weight: f32, species: u32, time: f32) -> f32 {
    let u = pos.x * dir.x + pos.y * dir.y;
    let v = -pos.x * dir.y + pos.y * dir.x;
    let big = grass_vnoise(vec2<f32>((u - 3.2 * time) / 46.0, v / 30.0), 0x51ed270bu);
    let small = grass_vnoise(vec2<f32>((u - 2.1 * time) / 14.0 + 17.3, (v + 0.8 * time) / 11.0 + 5.1), 0x9c3a7d15u);
    let intensity = smoothstep(0.40, 0.74, 0.65 * big + 0.35 * small);
    let pa = grass_vnoise(vec2<f32>(pos.x / 6.0 + 31.0, pos.y / 6.0), 0x2f6b1a93u);
    let pb = grass_vnoise(vec2<f32>(pos.x / 2.3, pos.y / 2.3 + 9.0), 0x7be144c7u);
    let phase = GRASS_TAU * 2.0 * (0.6 * pa + 0.4 * pb) + 1.5 * jitter;
    let hf = clamp(sqrt(0.4 / max(height_m, 0.02)), 0.6, 1.25);
    let w = GRASS_TAU * hz * 0.55 * hf * time;
    let sway = 0.55 + 0.45 * sin(w + phase);
    let band = 2.6 + 2.0 * grass_unit(grass_pcg(species ^ 0xf10a77e5u));
    let wf = GRASS_TAU * hz * band * time;
    let flutter = flutter_weight * 0.35 * intensity * sin(wf + 1.9 * phase + 2.1);
    return intensity * (sway + flutter);
}

fn grass_blade(inst: GrassInst, vi: u32, time: f32, front: f32, sway_on: bool) -> GrassVtx {
    let segments = max(grass_lod.segments, 1u);
    let per_blade = 2u * segments + 1u;
    let b = vi / per_blade;
    let j = vi % per_blade;
    let kind = inst.class_level & 0xffu;
    let cls = grass_classes.items[min(kind, 7u)];
    let n_near = max(min(u32(cls.shape2.w), u32(grass_params.lod.z)), 1u);
    let n_blades = grass_region_blades(u32(cls.shape2.w), grass_lod.region);
    if (b >= n_blades) {
        return grass_dead_vtx(kind);
    }
    let shape = unpack2x16unorm(inst.shape);
    let widen = shape.x * 8.0;
    let fade = shape.y;
    let look = unpack4x8unorm(inst.look);
    let nxz = unpack2x16snorm(inst.normal);
    let ground = normalize(vec3<f32>(nxz.x, sqrt(max(1.0 - dot(nxz, nxz), 0.0)), nxz.y));
    // Per-blade draws.
    var h = grass_pcg(inst.hash ^ ((b + 1u) * 0x632be5abu));
    let u0 = grass_unit(h);
    h = grass_next(h);
    let u1 = grass_unit(h);
    h = grass_next(h);
    let u2 = grass_unit(h);
    h = grass_next(h);
    let u3 = grass_unit(h);
    h = grass_next(h);
    let u4 = grass_unit(h);
    h = grass_next(h);
    let u5 = grass_unit(h);
    h = grass_next(h);
    let u6 = grass_unit(h);
    h = grass_next(h);
    let u7 = grass_unit(h);
    h = grass_next(h);
    let u8 = grass_unit(h);
    let green = u2 < look.x;
    let angle = GRASS_TAU * u1;
    let radius = cls.shape.w * sqrt(u0);
    let base = inst.pos + vec3<f32>(cos(angle), 0.0, sin(angle)) * radius;
    // The blade leans outward from the clump centre and curls toward its face normal.
    let yaw = angle + (u3 - 0.5) * 0.9;
    let dir = vec3<f32>(cos(yaw), 0.0, sin(yaw));
    let side = vec3<f32>(-sin(yaw), 0.0, cos(yaw));
    var height = mix(cls.shape.x, cls.shape.y, u4) * look.y * 2.0 * fade;
    var lean = cls.shape2.x * (0.4 + 0.8 * u5);
    var bend = cls.shape2.y * (0.5 + u6);
    var width = cls.shape.z * widen * sqrt(f32(n_near) / f32(n_blades)) * (0.75 + 0.5 * u7) * sqrt(fade);
    if (green) {
        height = height * cls.shape2.z;
        lean = lean * 0.35;
        bend = bend * 0.3;
        width = width * 1.3;
    }
    // A single-segment blade is one triangle from the root to the tip: a fully drooped tip would
    // lay it flat on the ground, so the coarse LOD keeps the blade standing.
    let coarse = segments == 1u;
    let tip_angle = min(lean + bend * 1.2, select(2.0, 0.7, coarse));
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let p1 = base + (up * cos(lean) + dir * sin(lean)) * height * 0.5;
    let p2 = p1 + (up * cos(tip_angle) + dir * sin(tip_angle)) * height * 0.5;
    var ring = j / 2u;
    var side_sign = select(-1.0, 1.0, (j & 1u) == 1u);
    if (j == 2u * segments) {
        ring = segments;
        side_sign = 0.0;
    }
    let s = f32(ring) / f32(segments);
    var centre = (1.0 - s) * (1.0 - s) * base + 2.0 * s * (1.0 - s) * p1 + s * s * p2;
    var tangent = normalize(2.0 * (1.0 - s) * (p1 - base) + 2.0 * s * (p2 - p1) + vec3<f32>(0.0, 1.0e-5, 0.0));
    if (coarse) {
        tangent = normalize(p2 - base + vec3<f32>(0.0, 1.0e-5, 0.0));
    }
    // The camera-body wake (ADR-1502). The velocity pass calls with the previous sway clock: it
    // reads the body's previous state then.
    if (sway_on && grass_params.wake[5].z > 0.0) {
        let row = select(0u, 2u, time != grass_params.eye.w);
        centre = centre + grass_wake_tip(base, height, s, row);
    }
    if (sway_on && cls.misc.z > 0.0) {
        // The shared wind block (ADR-1853); its default is the old fixed (1, 0.3) wind. A scene wind
        // (spec 88 § 2.4) has already replaced the direction here and scaled the sway (law.w).
        let wind = normalize(grass_params.wind.xy);
        var gust = 0.0;
        if (grass_params.counts2.y == 1u) {
            // The natural style (ADR-2446); mirrored by `regia_terrain::wind_natural::natural_wave`.
            gust = grass_natural_wave(inst.pos.xz, wind, u8, cls.misc.w * grass_params.wind.z, height, grass_params.wind.w, cls.seed.x, time);
        } else {
            let phase = dot(inst.pos.xz, wind) * 0.4 + u8 * 1.5;
            let w = GRASS_TAU * cls.misc.w * grass_params.wind.z * time;
            gust = sin(w + phase) + grass_params.wind.w * sin(2.3 * w + 1.7 * phase);
        }
        centre = centre + vec3<f32>(wind.x, 0.0, wind.y) * (cls.misc.z * grass_params.law.w * s * s * gust);
    }
    // Blast gust (ADR-1497): behind the front the blade leans away from the crash with the
    // Friedlander wind pulse, a quick flick that eases back (slightly past upright in the
    // suction phase) over the positive phase.
    if (sway_on && grass_params.gust.w > 0.0) {
        let rel = inst.pos.xz - grass_params.gust.xy;
        let d = length(rel);
        let phase_m = max(grass_params.gust2.y, 0.5);
        let behind = front - d;
        if (behind >= 0.0 && behind < 4.0 * phase_m) {
            let x = behind / phase_m;
            let pulse = (1.0 - x) * exp(-x);
            let away = rel / max(d, 1.0e-3);
            centre = centre + vec3<f32>(away.x, 0.0, away.y) * (grass_params.gust.w * height * s * s * pulse);
        }
    }
    let half_width = width * 0.5 * pow(max(1.0 - s, 0.0), 0.6);
    let world = centre + side * (half_width * side_sign);
    let face = normalize(cross(tangent, side));
    let normal = normalize(face + side * (side_sign * 0.45));
    // Colour: stems root → tip, shoots green; per-clump jitter and the mask's tint patches.
    var albedo = mix(cls.base.rgb, cls.tip.rgb, pow(s, 0.8));
    if (green) {
        albedo = mix(cls.green.rgb, cls.green_tip.rgb, s);
    }
    let jitter = 1.0 + cls.green_tip.w * (grass_unit(grass_pcg(inst.hash ^ 0xa511e9b3u)) * 2.0 - 1.0);
    albedo = clamp(albedo * jitter * mix(0.85, 1.15, look.z), vec3<f32>(0.0), vec3<f32>(1.0));
    let ao = mix(cls.green.w, 1.0, smoothstep(0.0, 0.7, s));
    var debug = vec3<f32>(0.0);
    let view = u32(grass_params.lod.w + 0.5);
    if (view == 1u) {
        debug = select(select(vec3<f32>(0.1, 0.2, 1.0), vec3<f32>(0.1, 1.0, 0.2), grass_lod.region == 1u), vec3<f32>(1.0, 0.15, 0.1), grass_lod.region == 0u);
    } else if (view == 2u) {
        let level = f32((inst.class_level >> 8u) & 0xfu) / 4.0;
        debug = mix(vec3<f32>(0.1, 0.9, 0.2), vec3<f32>(1.0, 0.1, 0.6), level);
    } else if (view == 3u) {
        debug = vec3<f32>(fade);
    }
    let canopy = vec2<f32>(world.y - inst.pos.y, cls.shape.y * look.y * 2.0 * fade * 1.05);
    return GrassVtx(world, normal, ground, s, albedo, ao, kind, debug, canopy, true);
}

fn grass_instance(ii: u32) -> GrassInst {
    return grass_instances[grass_lod.base + ii];
}

// A vertex no triangle survives (outside the clip volume; every vertex of a dropped blade
// lands here, so its triangles have no area either).
const GRASS_DEAD: vec4<f32> = vec4<f32>(0.0, 0.0, -2.0, 1.0);

struct GrassOut {
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) ground: vec3<f32>,
    @location(3) albedo: vec3<f32>,
    // x = s, y = root occlusion.
    @location(4) sa: vec2<f32>,
    @location(5) @interpolate(flat) kind: u32,
    @location(6) debug: vec3<f32>,
    @location(7) canopy: vec2<f32>,
}

@vertex
fn vs_grass(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GrassOut {
    let v = grass_blade(grass_instance(ii), vi, grass_params.eye.w, grass_params.gust.z, true);
    var position = GRASS_DEAD;
    if (v.alive) {
        position = frame.view_proj * vec4<f32>(v.world, 1.0);
    }
    return GrassOut(position, v.world, v.normal, v.ground, v.albedo, vec2<f32>(v.s, v.ao), v.kind, v.debug, v.canopy);
}

struct GrassPos {
    @invariant @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_grass_depth(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GrassPos {
    let v = grass_blade(grass_instance(ii), vi, grass_params.eye.w, grass_params.gust.z, true);
    if (!v.alive) {
        return GrassPos(GRASS_DEAD);
    }
    return GrassPos(frame.view_proj * vec4<f32>(v.world, 1.0));
}

// Sun cascades: the rest pose (no sway), so a retained cascade stays valid while the grass
// sways a few centimetres.
@vertex
fn vs_grass_shadow(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GrassPos {
    let v = grass_blade(grass_instance(ii), vi, 0.0, 0.0, false);
    if (!v.alive) {
        return GrassPos(GRASS_DEAD);
    }
    return GrassPos(cascade_vp.light_view_proj * vec4<f32>(v.world, 1.0));
}

// Sun cascades the dynamic-caster class refreshes (spec 88 § 2.4, ADR-2028): the swayed blade,
// as the colour pass draws it. The renderer redraws these cascades' grass pages every frame.
@vertex
fn vs_grass_shadow_sway(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GrassPos {
    let v = grass_blade(grass_instance(ii), vi, grass_params.eye.w, grass_params.gust.z, true);
    if (!v.alive) {
        return GrassPos(GRASS_DEAD);
    }
    return GrassPos(cascade_vp.light_view_proj * vec4<f32>(v.world, 1.0));
}

struct GrassVelOut {
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) motion: vec2<f32>,
}

@vertex
fn vs_grass_velocity(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> GrassVelOut {
    let inst = grass_instance(ii);
    let v = grass_blade(inst, vi, grass_params.eye.w, grass_params.gust.z, true);
    if (!v.alive) {
        return GrassVelOut(GRASS_DEAD, vec2<f32>(0.0));
    }
    let prev = grass_blade(inst, vi, grass_params.heights.w, grass_params.gust2.x, true);
    let current_uv = clip_uv(frame.unjittered_view_proj * vec4<f32>(v.world, 1.0));
    let prev_uv = clip_uv(frame.prev_view_proj * vec4<f32>(prev.world, 1.0));
    return GrassVelOut(frame.view_proj * vec4<f32>(v.world, 1.0), prev_uv - current_uv);
}

@fragment
fn fs_grass_velocity(in: GrassVelOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.motion, 0.0, 1.0);
}

// @surface-weather-begin
// The weather response of the ground layer under a blade (`terrain_weather_response`'s porosity,
// wet scale and snow scale): the layers' roughness blended by the same band weights the ground
// uses (times the splat packs on the photoreal path), at the ground normal's slope. The layers'
// textures are not read, so a textured layer's porosity comes from its authored roughness.
fn grass_ground_response(world: vec3<f32>, ground: vec3<f32>, footprint: f32) -> vec4<f32> {
    let slope = acos(clamp(ground.y, -1.0, 1.0));
    let photoreal = material.params0.x >= 0.5;
    let count = i32(clamp(layers.count.x, 1.0, 8.0));
    // Plain locals, not a `SplatPacks` var (no function-local struct variables, ADR-026).
    var pack_a = vec4<f32>(1.0);
    var pack_b = vec4<f32>(1.0);
    if (photoreal) {
        let packs = splat_packs(world.xz, footprint);
        pack_a = packs.a;
        pack_b = packs.b;
    }
    var total = 0.0;
    var rough = 0.0;
    for (var i = 0; i < count; i++) {
        let slot = layers.slots[i];
        var w = 0.0;
        if (photoreal) {
            let ws = slope_band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
            let wl = altitude_band(world.y, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
            w = max(ws * wl, 0.0) * pick_weight(pack_a, pack_b, i);
        } else {
            let ws = band(slope, slot.slope_alt.x, slot.slope_alt.y, slot.params.y);
            let wa = band(world.y, slot.slope_alt.z, slot.slope_alt.w, slot.params.y);
            w = max(ws * wa, 0.0);
        }
        total += w;
        rough += w * slot.albedo_roughness.a;
    }
    if (total <= 1e-5) {
        rough = layers.slots[0].albedo_roughness.a;
        total = 1.0;
    }
    return terrain_weather_response(world, slope, rough / total, footprint, photoreal);
}

// A blade under the weather (spec 99 § 6.4, grass stage 3): wetness darkens and glosses it by
// the ground layer's response (the § 6.3 film rule, `weather_apply`'s first step), and snow
// whitens its upper third by the layer's snow amount, `σ·x · 0.7`, on ground flat enough to
// hold snow. `s` runs 0 at the root to 1 at the tip. Returns the albedo and the roughness.
fn grass_weather(world: vec3<f32>, ground: vec3<f32>, s: f32, dist: f32, albedo: vec3<f32>, rough: f32) -> vec4<f32> {
    let x = clamp(terrain_weather_exposure(world, ground), 0.0, 1.0);
    if (x <= 0.0) {
        return vec4<f32>(albedo, rough);
    }
    let response = grass_ground_response(world, ground, max(dist * 0.002, 0.01));
    let phi = clamp(response.x, 0.0, 1.0);
    let wet = clamp(material.weather.x * x * response.y, 0.0, 1.0);
    var a = albedo * (1.0 - 0.5 * phi * wet);
    let film = wet * (1.0 - phi) + 0.6 * phi * smoothstep(0.7, 1.0, wet);
    var r = mix(rough, 0.08, 0.9 * film);
    let cover = clamp(material.weather.z * x * response.z, 0.0, 1.0) * smoothstep(0.5, 0.85, ground.y);
    let tip = 0.7 * cover * smoothstep(0.6, 0.75, s);
    a = mix(a, vec3<f32>(0.82), tip);
    r = mix(r, 0.8, tip);
    return vec4<f32>(a, r);
}
// @surface-weather-end

// The foliage profile's share of light through `path_mm` of blade, per channel: the radial profile
// integrated over a plane that deep (`regia_render::subsurface::slab_transmission`, ADR-3170).
fn grass_transmission(path_mm: f32) -> vec3<f32> {
    let d2 = path_mm * path_mm;
    let g0 = grass_params.subsurface_g0;
    let g1 = grass_params.subsurface_g1;
    return g0.rgb * exp(-0.5 * d2 / max(g0.w, 1.0e-6)) + g1.rgb * exp(-0.5 * d2 / max(g1.w, 1.0e-6));
}

fn grass_shade(in: GrassOut) -> vec4<f32> {
    let cls = grass_classes.items[min(in.kind, 7u)];
    let world = in.world;
    let eye = frame.camera_pos_time.xyz;
    let view_dir = normalize(eye - world);
    // Double-sided: the face toward the viewer, blended toward the ground normal so a field
    // shades as one surface at a distance instead of as noise.
    // (The face toward the viewer by the normal itself: the blade's winding says nothing about
    // which side `cross(tangent, side)` points to.)
    var n = normalize(in.normal);
    if (dot(n, view_dir) < 0.0) {
        n = -n;
    }
    let ground = normalize(in.ground);
    let dist = length(eye - world);
    let n_shade = normalize(mix(n, ground, clamp(0.3 + dist / 120.0, 0.3, 0.8)));
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    let sun_radiance = frame.sun_color_exposure.rgb * frame.sun_color_exposure.a;
    var ssao = 1.0;
    if (engine_csm.screen.z > 0.5) {
        let ao_dims = vec2<f32>(textureDimensions(engine_ao));
        let target_px = max(engine_csm.screen.xy, vec2<f32>(1.0));
        let texel = clamp(vec2<i32>(in.position.xy * ao_dims / target_px), vec2<i32>(0), vec2<i32>(ao_dims) - vec2<i32>(1));
        // Screen-space AO over a field of thin blades reads every gap as a crevice: take a
        // third of it (the root occlusion carries the clump's own darkening).
        ssao = mix(1.0, textureLoad(engine_ao, texel, 0).r, 0.35);
    }
    // The cascades with the normal that faces the sun (a thin blade is lit from either side).
    let n_sun = select(-n, n, dot(n, sun_dir) >= 0.0);
    // Grass under grass: thin blades are sub-texel, noisy occluders in the cascades, so a blade
    // looks the cascades up where its sun ray leaves the canopy (terrain, rocks and trees still
    // shadow it) and the field in between attenuates the sun as a turbid medium (Beer–Lambert
    // through the canopy, Monsi & Saeki 1953). The grass still casts its own shadows on the ground.
    let below_top = max(in.canopy.y - in.canopy.x, 0.0);
    let path = below_top / max(sun_dir.y, 0.08);
    let shadow_at = world + sun_dir * (path + 0.05 + dist * 0.002) + n_sun * 0.02;
    var sun_vis = sample_engine_csm(shadow_at, n_sun);
    sun_vis *= exp(-(1.2 - cls.tip.w) * 0.8 * path);
    if (engine_csm.cloud.w > 0.5) {
        sun_vis *= cloud_sun_transmittance(engine_cloud, engine_csm.cloud, world - engine_csm.origin.xyz, sun_dir, eye - engine_csm.origin.xyz);
    }
    let ndl = dot(n_shade, sun_dir);
    let front_lit = max(ndl, 0.0);
    // Light through the blade when the sun is behind it (thin translucent leaf).
    let back_lit = max(-dot(n, sun_dir), 0.0) * max(dot(-view_dir, sun_dir), 0.0) * cls.tip.w;
    let ao = in.sa.y * ssao;
    var ambient = frame.ambient_debug.rgb;
    if (frame.ground_ambient.w > 0.5) {
        let up = 0.5 + 0.5 * n_shade.y;
        ambient = frame.ambient_debug.rgb * up + frame.ground_ambient.rgb * (1.0 - up);
    }
    if (frame.output_mode.w > 0.0) {
        ambient = textureSampleLevel(sky_irradiance, sky_samp, sky_equirect_uv(n_shade), 0.0).rgb * frame.output_mode.w;
    }
    // @surface-weather-off let albedo = in.albedo;
    // @surface-weather-begin
    // Wet and snowy blades (spec 99 § 6.4, grass stage 3, ADR-3249): only the weather variant
    // compiles this; a dry frame keeps the pipeline and bytes it always had.
    var albedo = in.albedo;
    var blade_rough = clamp(cls.base.w, 0.04, 1.0);
    if (material.weather.x + material.weather.z > 0.0) {
        let weathered = grass_weather(world, ground, in.sa.x, dist, albedo, blade_rough);
        albedo = weathered.rgb;
        blade_rough = weathered.a;
    }
    // @surface-weather-end
    var back = back_lit * albedo * 2.0;
    // With the render profile's subsurface switch (`TerrainBudget::foliage_subsurface`, ADR-3170)
    // the light through the blade is the foliage profile's: diffuse transmission through the blade's
    // thickness along the sun ray, for the share of the class's translucency.
    let path_mm = grass_params.subsurface.y / max(abs(dot(n, sun_dir)), 0.1);
    if (grass_params.subsurface.x > 0.5) {
        back = grass_transmission(path_mm) * (max(-dot(n, sun_dir), 0.0) * cls.tip.w) * albedo;
    }
    var lit = albedo * (sun_radiance * (front_lit + back) * sun_vis * INV_PI + ambient * ao);
    if (frame.fill_dir.w > 0.5) {
        lit += albedo * frame.fill_color.rgb * max(dot(n_shade, normalize(frame.fill_dir.xyz)), 0.0) * INV_PI * ao;
    }
    // @surface-weather-off let rough = clamp(cls.base.w, 0.04, 1.0);
    // @surface-weather-begin
    let rough = blade_rough;
    // @surface-weather-end
    lit += sun_radiance * specular_ggx(n_shade, view_dir, sun_dir, rough, 0.04) * sun_vis * in.sa.x;
    // The frame's extra directional lights, unshadowed (ADR-7254; 0 without any).
    lit += extra_lights_direct(n_shade, view_dir, albedo, rough, 0.04);
    if (grass_params.lod.w > 0.5) {
        lit = in.debug * (sun_radiance * 0.3 * INV_PI + ambient);
    }
    if (u32(grass_params.lod.w + 0.5) == 4u) {
        // Debug view 4: the foliage transmission through this blade toward the sun, raw (ADR-3170).
        let raw = select(1.0, frame.fill_color.w, frame.output_mode.x > 0.5 && frame.fill_color.w > 0.0);
        return vec4<f32>(grass_transmission(path_mm) * raw, 1.0);
    }
    if (frame.output_mode.x > 0.5) {
        var rgb = max(lit, vec3<f32>(0.0));
        let params = frame.fog_params;
        // The haze and the rain in front of the blade are under the cloud layer too (ADR-5944):
        // 1, and no load of the map, without the map or with neither of the two.
        let haze_sun = shared_haze_sun(world);
        if (params.w > 0.5 && params.x > 0.0) {
            if (frame.fog_color.a > 0.5) {
                let tint = frame.fog_color.rgb / max(luma3(frame.fog_color.rgb), 1.0e-3);
                if (haze_sun < 1.0) {
                    rgb = apply_aerial_under_clouds(rgb, world, eye, params, tint, sun_dir, sun_radiance, max(frame.output_mode.z, 1.0), haze_sun);
                } else {
                    rgb = apply_aerial(rgb, world, eye, params, tint, sun_dir, sun_radiance, max(frame.output_mode.z, 1.0));
                }
            } else {
                let distance = length(world - eye);
                let height_density = params.x * exp(-params.y * (world.y - params.z));
                let t = 1.0 - exp(-max(distance, 0.0) * max(height_density, 0.0));
                let inscatter = frame.fog_color.rgb * max(frame.output_mode.z, 1.0);
                rgb = mix(rgb, inscatter, clamp(t, 0.0, 1.0));
            }
        }
        // @surface-weather-begin
        // The rain's or snow's own extinction, as the ground under the blades takes it.
        if (material.weather2.z > 0.0) {
            let view_ray = world - eye;
            var scattered = frame.fog_color.rgb * max(frame.output_mode.z, 1.0);
            if (frame.fog_color.a > 0.5) {
                let tint = frame.fog_color.rgb / max(luma3(frame.fog_color.rgb), 1e-3);
                if (haze_sun < 1.0) {
                    // Rain under the cloud layer scatters the light under the layer (ADR-5944).
                    scattered = tint * haze_inscatter_under_clouds(normalize(view_ray), sun_dir, sun_radiance, max(frame.output_mode.z, 1.0), haze_sun);
                } else {
                    scattered = tint * haze_inscatter(normalize(view_ray), sun_dir, sun_radiance, max(frame.output_mode.z, 1.0));
                }
            } else if (haze_sun < 1.0) {
                // The flat colour stands for a clear day's sky: under the layer it keeps the
                // haze's floor of it (ADR-5944).
                scattered *= mix(HAZE_SKY_FLOOR, 1.0, haze_sun);
            }
            rgb = weather_extinction(rgb, length(view_ray), material.weather2.z, scattered);
        }
        // @surface-weather-end
        let hdr_scale = select(1.0, frame.fill_color.w, frame.fill_color.w > 0.0);
        return vec4<f32>(rgb * hdr_scale, ssr_roughness(rough, max(dot(n_shade, view_dir), 0.0)));
    }
    lit = clamp(apply_height_fog(lit, world), vec3<f32>(0.0), vec3<f32>(1.0e4));
    let display = default_display_transform(lit);
    if (frame.output_mode.y > 0.5) {
        return vec4<f32>(display, 1.0);
    }
    return vec4<f32>(display_to_linear(display), 1.0);
}

@fragment
fn fs_grass_color(in: GrassOut) -> @location(0) vec4<f32> {
    return grass_shade(in);
}

// Camera-body wake (ADR-1502): `regia_core::math::wake::gust_from_words` in WGSL. `row` 0 reads
// the body now, 2 the body the frame before (the velocity pass). The gust at `p`, m/s.
fn grass_wake_gust(p: vec3<f32>, row: u32) -> vec3<f32> {
    let pos = grass_params.wake[row];
    let vel = grass_params.wake[row + 1u];
    let shape = grass_params.wake[4];
    let misc = grass_params.wake[5];
    let speed = length(vel.xyz);
    if (misc.z <= 0.0 || pos.w <= 0.0 || speed <= 1.0e-3) {
        return vec3<f32>(0.0);
    }
    let dir = vel.xyz / speed;
    let rel = p - pos.xyz;
    let along = dot(rel, dir);
    let radial = rel - dir * along;
    let lateral = length(radial);
    if (lateral >= shape.z) {
        return vec3<f32>(0.0);
    }
    let sin_mu = vel.w;
    let cos_mu = sqrt(max(1.0 - sin_mu * sin_mu, 0.0));
    let since = (-along * sin_mu - lateral * cos_mu) / (speed * sin_mu);
    let x = since / shape.w;
    if (since < 0.0 || x >= 6.0) {
        return vec3<f32>(0.0);
    }
    let fade = 1.0 - smoothstep(0.6 * shape.z, shape.z, lateral);
    let peak = min(pos.w * pow(shape.x / (shape.x + lateral), shape.y) * fade, misc.x);
    let pulse = (1.0 - x) * exp(-x) * (1.0 - smoothstep(4.0, 6.0, x));
    // On the axis itself (under 0.1 mm) the push is along the travel only.
    let outward = select(vec3<f32>(0.0), radial / max(lateral, 1.0e-4), lateral > 1.0e-4);
    let direction = normalize(dir * sin_mu + outward * max(cos_mu, misc.y));
    return direction * (peak * pulse);
}

// The wake's displacement of a blade point `s` of the way up a blade `height` tall rooted at
// `root`: it leans with the horizontal gust (saturating at `bend` of the height) and drops a
// little so it does not stretch. CPU twin: `regia_terrain::grass::GrassWake::tip_offset`.
fn grass_wake_tip(root: vec3<f32>, height: f32, s: f32, row: u32) -> vec3<f32> {
    let g = grass_wake_gust(root, row).xz;
    let m = length(g);
    if (m <= 1.0e-4) {
        return vec3<f32>(0.0);
    }
    let response = grass_params.wake[6].x * m / (m + max(grass_params.wake[6].y, 1.0e-3));
    let lean = g / m * (response * height * s * s);
    return vec3<f32>(lean.x, -0.5 * response * response * height * s * s, lean.y);
}
