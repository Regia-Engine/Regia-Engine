// SPDX-License-Identifier: MPL-2.0
// Lab forward pass: depth-prepass vertex, clustered point/spot lights, directional light.
// BRDF and froxel indexing come from the engine modules.

// The texture table variant (`TEXTURE_TABLE`, ADR-863) declares a `binding_array`, which naga
// accepts only with this extension enabled; naga_oil reads `enable` directives only at the top of
// a file (before any `#ifdef`), so it is on for every variant. The engine hands wgpu naga IR, which
// carries no directive.
enable wgpu_binding_array;

#import regia::utils
#import regia::mesh
#import regia::pbr::lighting
#import regia::pbr::brdf
#import regia::pbr::shading_models
#import regia::pbr::rough_transmission
#import regia::clustering
#import regia::shadows
#import regia::fog
#import regia::gi
#ifdef FRAME_BLUE_NOISE
#import regia::blue_noise
#endif
#ifdef FRAME_FORWARD_DECALS
// Items, not the module: a bare `decals` would alias the module (the mask-decal block has a
// `decals` member).
#import regia::forward::decals::{fd_all_mask, fd_view_matches, fd_weight, fd_srgb_decode, fd_from_xy, fd_whiteout, fd_to_receiver}
#endif
#import regia::ddgi
#import regia::aerial
#import regia::cloud_shadow
#import regia::forward::ltc::{safe_unit, ltc_edge, ltc_polygon, ltc_sphere}
#import regia::forward::occluder_geometry::{MeshOccluder, MeshOccluders, OccLight, occ_acos, occ_edge, occ_unit, occ_disc_ff, occ_slab_hit, occ_blob}
#import regia::forward::occlusion::{multi_bounce_ao, specular_occlusion, SPEC_CONE_MIN_ANGLE, bent_decode, bent_shading_normal, spec_cone_half_angle, cap_solid_angle, cap_intersection, cone_specular_occlusion}
#import regia::forward::env_ground::{env_ground_cover, env_ground_reflection}
#import regia::forward::util::{oct_encode, equirect_uv, cube_face, gi_component, decal_unrotate, hair_hash, override_on, override_tint, override_emissive, glass_hash, ssr_pack}
#import regia::forward::surface_weather
#ifdef MATERIAL_GRAPH
#import regia::pbr::surface
#endif

struct LabFrame {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    // xyz = eye. w > 0.5 is an interior: neutral ambient floor, probe bounce added.
    camera_pos: vec4<f32>,
    params: vec4<f32>,
    screen: vec4<f32>,
    extra: vec4<f32>,
    // x = the linear HDR target's pre-exposure (> 0: post enabled, radiance × x is written;
    // 0: display-referred output), y = EV100 exposure multiplier (1 / (1.2 · 2^EV100)).
    post_exposure: vec4<f32>,
    // density, height_falloff, base_height, enabled
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    // x = shadow filter (0 hardware, 1 PCF 3×3, 2 PCF 5×5, 3 PCSS-lite),
    // y = 2 when dynamic irradiance probes are bound (0 leaves this shader unchanged),
    // z = reflection-probe budget, w = temporal noise index (0 without temporal AA).
    quality: vec4<f32>,
    prev_view_proj: mat4x4<f32>,
    unjittered_view_proj: mat4x4<f32>,
    // z = 1 when the profile allows parallax occlusion. w = local-shadow debug
    // (0 lit, 1 shadow factor, 2 atlas tile).
    pcss: vec4<f32>,
    // Interior volume containing the eye: centre (w = 1 when set) and half extents.
    // Only pixels inside it take the interior shade. `interior_half.w` holds flags: bit 0 =
    // hashed alpha test for masked materials (TSR frames, `alpha_rejects`); bit 1 = stochastic
    // SSR runs this frame, so opaque draws write its surface code into the HDR alpha; bit 2 =
    // fitted area lights carry the emitters (emissive-local); bit 3 = reflection occlusion
    // (`fs_transmit_add`). Bit 7 = the frame clock is live (`graph_clock`, ADR-5160).
    interior_center: vec4<f32>,
    interior_half: vec4<f32>,
    // Planet-shine fill: xyz toward the sky body (w = 1 when on), and colour · lux.
    // `fill_color.w` = material texture mip bias (negative under TSR upscaling, plus the
    // device's anisotropic-blend offset, ADR-5670; 0 on most adapters without TSR).
    fill_dir: vec4<f32>,
    fill_color: vec4<f32>,
    // Specular AA (`pbr_lighting::specular_aa_roughness`, `toksvig_roughness`): x = screen-space
    // variance (0 off), y = kernel threshold, z = Toksvig quantisation floor. w = 1 when the IBL is
    // a reference studio's environment (`FrameRenderer::set_studio_sky`): the sky draw shows it.
    specular_aa: vec4<f32>,
    // Cloud transmittance map (ADR-320): extent, layer base and top, w = 1 when it shadows the
    // sun term (0: unchanged). Not named after the `regia::cloud_shadow` import: naga_oil
    // rewrites that bare identifier as the module alias.
    cloud_sun: vec4<f32>,
    // The image light's ground plane (`env_ground.rs`): height, radius (0 = unbounded), centre x, z.
    env_ground: vec4<f32>,
    // rgb = the plane's radiance, cd/m²; w = 1 when the plane is on.
    env_ground_light: vec4<f32>,
    // x = image-light debug view (0 lit, 1 image light only, 2 occlusion terms: `DebugViewMode`),
    // y = sky draw's background (0 the environment, 1 solid, 2 gradient, 3 image),
    // z = local reflection captures, w = their highest mip.
    env_view: vec4<f32>,
    // Background (`StudioBackground`): [0] rgb = solid colour or the gradient's zenith, w = its
    // falloff; [1] rgb = horizon; [2] rgb = ground. cd/m².
    background: array<vec4<f32>, 3>,
    // Local reflection captures (`local_capture.rs`): box centre and blend weight,
    capture_center: array<vec4<f32>, 4>,
    // box half extents and the fade distance outside the box.
    capture_half: array<vec4<f32>, 4>,
    // Surface weather (spec 99 § 6.5): wetness, puddles, snow cover, ripple; all zero skips it.
    weather: vec4<f32>,
    // The weather clock, the occlusion tile (-1 none), its extent, and the precipitation's
    // extinction (1/m; 0 skips it).
    weather2: vec4<f32>,
    // Graph clock (ADR-5160): time, previous time, wind x, wind z. Read when bit 7 of
    // `interior_half.w` is set; otherwise graphs keep their own slot 0.
    clock: vec4<f32>,
    // Lights-and-views block (ADR-7211, `LightsViewsUniform`): x = the debug view
    // (`lights_views::shading_view_code`, 0 lit), y = extra directional lights, z = ambient volumes.
    view_mode: vec4<f32>,
    // Extra directional lights (up to 3): travel direction (w = 1), then color · lux.
    extra_dir: array<vec4<f32>, 6>,
    // Ambient volumes (up to 4): centre (w = blend distance), half extents, irradiance (lux).
    ambient_box: array<vec4<f32>, 12>,
    // Ambient volumes: the rotation from the world into each box's frame (quaternion xyzw, ADR-7275).
    ambient_rot: array<vec4<f32>, 4>,
}

struct GpuLight {
    position_kind: vec4<f32>,
    color_intensity: vec4<f32>,
    direction_radius: vec4<f32>,
    cone_shadow: vec4<f32>,
}

struct DirectionalLight {
    direction: vec3<f32>,
    _pad0: u32,
    color: vec3<f32>,
    illuminance: f32,
}

struct ObjectData {
    model: mat4x4<f32>,
    skin: vec4<u32>,
}

struct LabMaterial {
    base_color: vec4<f32>,
    factors: vec4<f32>,
    emissive: vec4<f32>,
    alpha: vec4<f32>,
    // xyz = detail albedo / normal / roughness strength
    detail: vec4<f32>,
    // xy = detail UV scale, z = fade start, w = fade end
    detail_fade: vec4<f32>,
    // x = clearcoat, y = clearcoat roughness, z = anisotropy, w = rotation
    coat: vec4<f32>,
    // x = specular occlusion, y = multi-scatter, z = parallax scale, w = parallax max steps
    response: vec4<f32>,
    // x = wear intensity, y = roughness bias, z = metallic bias, w = source (0 off, 1 curvature, 2 ao)
    wear: vec4<f32>,
    // xyz = wear tint, w = contrast
    wear_tint: vec4<f32>,
    // x = parallax min steps, y = wear bias, z = transmission (KHR_materials_transmission),
    // w = dielectric F0 from the IOR (0.04 at 1.5)
    pom: vec4<f32>,
    // rgb = volume transmittance at normal incidence (KHR_materials_volume), w = thickness
    transmit: vec4<f32>,
    // Shading model (`shading_models.wgsl`, `crate::shading_model`): x = model (0 standard,
    // 1 skin, 2 eye), y = 1 with a sheen lobe. Skin: a = scatter scale, curvature radius override
    // (mm), normal blur, transmission; b = second-lobe roughness and weight, thickness per mm of
    // radius, mm per world unit. Eye: a = iris radius, limbus width, limbus darkening, iris depth
    // (UV); b = pupil UV, cornea IOR, cornea roughness; c = socket occlusion and its UV radii.
    // sheen = colour and roughness. All zero on a standard material.
    shading: vec4<f32>,
    shading_a: vec4<f32>,
    shading_b: vec4<f32>,
    shading_c: vec4<f32>,
    sheen: vec4<f32>,
    // Hair cards (`hair.rs`); all zero unless the material enables hair, except `hair_a.x`, which
    // holds the transmission roughness of a material without hair (`rough_transmission.rs`).
    // x = primary shift, y = secondary shift, z = primary exponent, w = secondary exponent
    hair_a: vec4<f32>,
    // x = primary strength, y = secondary strength, z = transmission, w = alpha strategy
    // (0 = not hair, 1 hashed, 2 sorted, 3 coverage)
    hair_b: vec4<f32>,
    // rgb = tip color multiplier, w = tip alpha
    hair_c: vec4<f32>,
    // x = ramp start (card v), y = ramp end, z = root alpha, w = per-strand shift jitter
    hair_d: vec4<f32>,
    // rgb = root color multiplier, w = strands across the card's u
    hair_e: vec4<f32>,
#ifdef TEXTURE_TABLE
    // Texture table indices (`regia_render::texture_table`, ADR-863): two 16-bit indices per
    // word in slot order (base colour, normal, ORM, emissive, detail, detail normal, aux).
    // Only standard materials on the table path carry it; graph materials never do.
    texture_indices: vec4<u32>,
#endif
#ifdef MATERIAL_GRAPH
    // Material-graph parameters (`regia_matgraph`): slot 0 holds the built-ins (x = time in
    // seconds, y = previous time, zw = wind) unless bit 7 of `frame.interior_half.w` is set,
    // when `graph_clock()` returns `frame.clock` instead. The rest is one parameter per vec4.
    graph: array<vec4<f32>, 16>,
#endif
}

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) joints: vec4<u32>,
    @location(6) weights: vec4<f32>,
#ifdef MATERIAL_GRAPH_UV1
    // Second UV set: its own vertex stream (buffer slot 1), bound only for graphs that read it.
    @location(7) uv1: vec2<f32>,
#endif
#ifdef LIGHTMAP
    // A lightmap receiver's entry-local UV2 (`lightmap::receiver_mesh`): its own vertex stream
    // (buffer slot 1). Never with a material graph.
    @location(7) uv2: vec2<f32>,
#endif
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) world_tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    // LOD cross-fade dither (`FrameMesh::lod_dither`, object `skin.y`); 0 = drawn whole.
    @location(5) @interpolate(flat) lod_dither: f32,
    // Render override (`crate::render_override`, object `skin.zw`): x = tint (unorm8 RGBA, A =
    // coverage), y = flags (bit 0 on, 1 unlit, 2 x-ray, 3 blended, 4 emissive) and the emissive
    // colour (unorm8 RGB in bytes 1-3). Zero = no override.
    @location(8) @interpolate(flat) overrides: vec2<u32>,
#ifdef MATERIAL_GRAPH
    @location(6) uv1: vec2<f32>,
    @location(7) @interpolate(flat) object_position: vec3<f32>,
#endif
#ifdef LIGHTMAP
    // Entry-local UV2 of a lightmap receiver (ADR-1623).
    @location(9) lightmap_uv: vec2<f32>,
    // Its entry's UV2 → atlas transform; zero for a draw that is not a receiver (ADR-2107).
    @location(10) @interpolate(flat) lightmap_scale_offset: vec4<f32>,
#endif
#ifdef FRAME_FORWARD_DECALS
    // The forward decals' receiver layer (`forward_decals`, ADR-3255): 1 a rigid mesh, 2 a skinned
    // one (the decal receiver stamp's default layers).
    @location(11) @interpolate(flat) decal_layer: u32,
#endif
}

@group(0) @binding(0) var<uniform> frame: LabFrame;
@group(0) @binding(1) var<uniform> dir_light: DirectionalLight;
@group(0) @binding(2) var<storage, read> lights: array<GpuLight>;
@group(0) @binding(3) var<storage, read> counts: array<u32>;
@group(0) @binding(4) var<storage, read> indices: array<u32>;

struct ShadowUniform {
    light_view_proj: mat4x4<f32>,
    split_distance: f32,
    // Receiver-side shadow bias (deterministic across GPUs):
    // x = world size of one cascade texel, y = 1 / ortho depth extent.
    bias: vec3<f32>,
}

struct ShadowUniforms {
    cascades: array<ShadowUniform, 4>,
    // x = cascade count (1..=4), set from the render profile. y = cascades with a
    // translucency map this frame (0 = none), z = 1 when the local-light translucency atlas
    // is live (`translucent_shadow.wgsl`).
    count: vec4<f32>,
}

struct ShadowTile {
    view_proj: mat4x4<f32>,
    uv: vec4<f32>,
    // Receiver-side shadow bias: x = texel size (world units per shadow texel;
    // for perspective tiles this is the size at unit view depth, scaled by
    // clip.w at the receiver), y = 1/depth-extent term, z = 1 when the tile is
    // perspective (0 = orthographic), w unused.
    bias: vec4<f32>,
}

@group(0) @binding(5) var shadow_map_0: texture_depth_2d;
@group(0) @binding(6) var shadow_map_1: texture_depth_2d;
@group(0) @binding(7) var shadow_samp: sampler_comparison;
@group(0) @binding(8) var<uniform> csm: ShadowUniforms;
@group(0) @binding(9) var shadow_atlas: texture_depth_2d;
@group(0) @binding(10) var<storage, read> shadow_tiles: array<ShadowTile>;
@group(0) @binding(11) var irradiance_tex: texture_2d<f32>;
@group(0) @binding(12) var prefilter_tex: texture_2d<f32>;
@group(0) @binding(13) var brdf_lut: texture_2d<f32>;
@group(0) @binding(15) var shadow_map_2: texture_depth_2d;
@group(0) @binding(16) var shadow_map_3: texture_depth_2d;
@group(0) @binding(14) var ibl_samp: sampler;
// Screen-space occlusion. Off binds a 1×1 white texture, so indirect light is unchanged.
// Layer 0: AO and the reflection visibility; layer 1 (read only with `interior_half.w` bit 4):
// the bent normal's lean from the depth normal (`ssao.wgsl` `bent_encode`, ADR-1375).
@group(0) @binding(17) var ao_tex: texture_2d_array<f32>;
// Nearest, clamp-to-edge, bound non-filtering: the SSAO texture and the sun cascades'
// receiver-plane gathers (`regia::shadows::plane_quad_lit`) both read through it.
@group(0) @binding(18) var ao_samp: sampler;

struct GiProbe {
    sh_r: vec4<f32>,
    sh_g: vec4<f32>,
    sh_b: vec4<f32>,
    depth_mean: array<vec4<f32>, 4>,
    depth_msq: array<vec4<f32>, 4>,
}

struct GiGrid {
    // xyz = origin, w = intensity
    origin: vec4<f32>,
    // xyz = spacing, w = normal bias
    spacing: vec4<f32>,
    // xyz = dimensions, w = probe count
    dims: vec4<u32>,
}

@group(0) @binding(19) var<storage, read> gi_probes: array<GiProbe>;
@group(0) @binding(20) var<uniform> gi_grid: GiGrid;
// Light the shared terrain bounces onto the ground around the camera (`GroundBounce`):
// [0], [1] = per level (origin x, origin z, cell metres, cells per side; 0 = none),
// [2] = the horizon header (the atmosphere bake's ground radiance rgb, w = 1 when horizons
// follow), then the levels' bounce radiance cells (irradiance / π), row-major with rows along
// +Z, then (w = 1) the levels' horizon cells in the same layout: tan θ_h of the terrain in the
// four diagonal sectors (45°, 135°, 225°, 315° from +X toward +Z).
@group(0) @binding(21) var<storage, read> ground_bounce: array<vec4<f32>>;
// GGX linearly transformed cosines (`ltc.rs`): LTC_SIZE² texels, row = perceptual roughness,
// column = sqrt(1 − n·v), rgba = (a, b, c, d) of M⁻¹ = [[a, 0, b], [0, 1, 0], [c, 0, d]].
@group(0) @binding(32) var ltc_mat: texture_2d<f32>;
// Emissive lights' distance probes (`emissive_lights.rs`): tile k (16 per row) belongs to the
// light whose `cone_shadow.w` is k + 1 (−(k + 2) for a light the baked volume holds,
// `emitter_tag`); each texel is the distance from the light's centre to the first surface in
// that octahedral direction.
@group(0) @binding(33) var emitter_probes: texture_2d<f32>;
// Cloud transmittance map (ADR-320); a 1×1 white texture while there is none.
@group(0) @binding(34) var cloud_sun_map: texture_2d<f32>;
#ifndef STUDIO_IBL_OFF
// Local reflection captures (`local_capture.rs`): one layer per capture, equirect, GGX-filtered
// mips; rgb = premultiplied radiance over `SKY_IBL_SCALE`, a = surface coverage.
@group(0) @binding(35) var local_capture_tex: texture_2d_array<f32>;
#endif
// The studio background image (`StudioBackground::Image`), radiance over `SKY_IBL_SCALE`.
@group(0) @binding(36) var background_tex: texture_2d<f32>;
#ifdef RESTIR_DI
// Stochastic many-light sampling (ADR-2470, ADR-2652): one reservoir per render-resolution
// pixel of the opaque depth surface (`restir_di.wgsl` `Reservoir`): the chosen point or spot
// light (0xffffffff = none) and its contribution weight W. Opaque permutations only.
struct ManyLightReservoir {
    light: u32,
    w: f32,
    m: f32,
    pad: f32,
}
@group(0) @binding(37) var<storage, read> restir_reservoirs: array<ManyLightReservoir>;
#endif
#ifdef FRAME_BLUE_NOISE
// Spatiotemporal blue noise (ADR-2075, ADR-2652): the table, or a 1×1 stand-in while the frame
// does not use it (`interior_half.w` bit 6 says which). Every forward module of a device whose
// view layout has binding 38 declares it, so the prepass and the colour pass draw one noise.
@group(0) @binding(38) var frame_blue_noise: texture_2d_array<f32>;

// Whether this frame's stochastic forward terms (shadow rotation, hashed alpha, pane SSR) take
// the blue-noise table in place of interleaved gradient noise.
fn frame_blue_noise_on() -> bool {
    return (u32(frame.interior_half.w + 0.5) & 64u) != 0u;
}

// The table's number at `pix` for this frame's noise index, in (0, 1).
fn frame_blue_noise_at(pix: vec2<f32>) -> f32 {
    return regia::blue_noise::scalar(frame_blue_noise, vec2<u32>(max(pix, vec2<f32>(0.0))), u32(max(frame.quality.w, 0.0) + 0.5));
}
#endif

#ifdef FRAME_FORWARD_DECALS
// ---- Forward decals (`regia_render::forward_decals`, ADR-3255) ----
// Header, then the decals in draw order, then one 64-bit mask per screen tile (bit i = decal i).
struct FdHeader {
    // x = decal count, y = tiles across, z = tiles down, w = tile edge in pixels
    counts: vec4<u32>,
    // xyz = the eye the tiles were built for, w = view width
    view_a: vec4<f32>,
    // xyz = the view axis (third row of the view matrix), w = view height
    view_b: vec4<f32>,
    // x = 1 when the tile masks are valid, y = mask words per tile (2)
    flags: vec4<u32>,
}
struct FdDecal {
    // xyz unit axes; w = 1 / half extent
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
    // xyz centre, w opacity (lifetime fade in)
    center: vec4<f32>,
    // rgb linear albedo, w blend (0 modulate, 1 replace)
    albedo: vec4<f32>,
    // x roughness, y metallic, z normal strength, w edge softness
    material: vec4<f32>,
    // xy tangent-space normal, z angle-fade start, w angle-fade end
    normal: vec4<f32>,
    // x channels, y receiver mask (0 = all), z atlas layer (0xffffffff none), w pages (bit 0
    // normal page, bit 1 roughness/metallic page)
    params: vec4<u32>,
    // x distance-fade start, y end
    fades: vec4<f32>,
}
// The atlas follows the masks: packed RGBA8 texels, 64×64 per page, three pages per decal layer
// (albedo + coverage, normal, roughness/metallic multipliers), filtered in `fd_page`.
struct FdData {
    header: FdHeader,
    decals: array<FdDecal, 64>,
    masks: array<vec2<u32>, 8192>,
    atlas: array<u32>,
}
@group(0) @binding(39) var<storage, read> fd_data: FdData;

fn fd_texel(page: u32, x: i32, y: i32) -> vec4<f32> {
    let cx = u32(clamp(x, 0, 63));
    let cy = u32(clamp(y, 0, 63));
    return unpack4x8unorm(fd_data.atlas[page * 4096u + cy * 64u + cx]);
}

// One bilinear, edge-clamped sample of an atlas page (four reads; no implicit derivatives).
fn fd_page(page: u32, uv: vec2<f32>) -> vec4<f32> {
    let p = uv * 64.0 - vec2<f32>(0.5);
    let base = floor(p);
    let f = p - base;
    let x = i32(base.x);
    let y = i32(base.y);
    let top = mix(fd_texel(page, x, y), fd_texel(page, x + 1, y), f.x);
    let bottom = mix(fd_texel(page, x, y + 1), fd_texel(page, x + 1, y + 1), f.x);
    return mix(top, bottom, f.y);
}

// The surface inputs a decal may change.
struct FdSurface {
    base: vec3<f32>,
    n: vec3<f32>,
    perceptual: f32,
    metallic: f32,
}

// Blends the decals of this pixel's tile into the surface, in draw order (the CPU mirror is
// `forward_decals::blend_point`; the tracer's is `regia_pt`'s `pt_apply_decals`). Atlas pages are
// read from the storage buffer with no implicit derivatives, so the loop may diverge.
fn apply_forward_decals(s_in: FdSurface, world: vec3<f32>, pix: vec2<f32>, layer: u32) -> FdSurface {
    // Plain locals, not a struct variable (ADR-026).
    var s_base = s_in.base;
    var s_n = s_in.n;
    var s_perceptual = s_in.perceptual;
    var s_metallic = s_in.metallic;
    let count = min(fd_data.header.counts.x, 64u);
    let n_recv = s_in.n;
    let axis = vec3<f32>(frame.view[0][2], frame.view[1][2], frame.view[2][2]);
    var mask = fd_all_mask(count);
    if (fd_data.header.flags.x == 1u
        && fd_view_matches(fd_data.header.view_a, fd_data.header.view_b, frame.camera_pos.xyz, axis, frame.screen.xy)) {
        let tile_px = f32(max(fd_data.header.counts.w, 1u));
        let tx = min(u32(max(pix.x, 0.0) / tile_px), max(fd_data.header.counts.y, 1u) - 1u);
        let ty = min(u32(max(pix.y, 0.0) / tile_px), max(fd_data.header.counts.z, 1u) - 1u);
        mask = fd_data.masks[ty * fd_data.header.counts.y + tx];
    }
    for (var word = 0u; word < 2u; word = word + 1u) {
        var bits = select(mask.x, mask.y, word == 1u);
        loop {
            if (bits == 0u) {
                break;
            }
            let i = word * 32u + countTrailingZeros(bits);
            bits = bits & (bits - 1u);
            if (i >= count) {
                break;
            }
            let right = fd_data.decals[i].right;
            let up = fd_data.decals[i].up;
            let fwd = fd_data.decals[i].forward;
            let centre = fd_data.decals[i].center;
            let rel = world - centre.xyz;
            let local = vec3<f32>(dot(rel, right.xyz) * right.w, dot(rel, up.xyz) * up.w, dot(rel, fwd.xyz) * fwd.w);
            if (max(abs(local.x), max(abs(local.y), abs(local.z))) > 1.0) {
                continue;
            }
            let params = fd_data.decals[i].params;
            if (params.y != 0u && (params.y & (1u << min(layer, 31u))) == 0u) {
                continue;
            }
            let normal_p = fd_data.decals[i].normal;
            let material_p = fd_data.decals[i].material;
            let fades = vec4<f32>(normal_p.z, normal_p.w, fd_data.decals[i].fades.x, fd_data.decals[i].fades.y);
            let facing = abs(dot(n_recv, fwd.xyz));
            let distance = length(frame.camera_pos.xyz - centre.xyz);
            var w = fd_weight(centre.w, local, facing, material_p.w, fades, distance);
            let has_atlas = params.z != 0xffffffffu;
            let auv = clamp(vec2<f32>(local.x * 0.5 + 0.5, 0.5 - local.y * 0.5), vec2<f32>(0.0), vec2<f32>(1.0));
            let page0 = params.z * 3u;
            var tint = vec3<f32>(1.0);
            if (has_atlas) {
                let texel = fd_page(page0, auv);
                tint = fd_srgb_decode(texel.rgb);
                w = w * texel.a;
            }
            if (w <= 1e-4) {
                continue;
            }
            let channels = params.x;
            if ((channels & 1u) != 0u) {
                let albedo = fd_data.decals[i].albedo;
                let paint = albedo.rgb * tint;
                if (albedo.w < 0.5) {
                    s_base = s_base * mix(vec3<f32>(1.0), paint, w);
                } else {
                    s_base = mix(s_base, paint, w);
                }
            }
            if ((channels & 2u) != 0u) {
                var nts = fd_from_xy(normal_p.xy);
                if (has_atlas && (params.w & 1u) != 0u) {
                    let page_n = fd_page(page0 + 1u, auv).xy * 2.0 - vec2<f32>(1.0);
                    let strength = clamp(material_p.z, 0.0, 1.0);
                    nts = fd_whiteout(fd_from_xy(page_n * strength), nts);
                }
                let n_shade = fd_to_receiver(n_recv, right.xyz, up.xyz, nts);
                let n_decal = normalize(mix(s_n, n_shade, clamp(material_p.z, 0.0, 1.0)));
                s_n = normalize(mix(s_n, n_decal, w));
            }
            var rough_scale = 1.0;
            var metal_scale = 1.0;
            if (has_atlas && (params.w & 2u) != 0u) {
                let page_m = fd_page(page0 + 2u, auv);
                rough_scale = page_m.r * 2.0;
                metal_scale = page_m.g * 2.0;
            }
            if ((channels & 4u) != 0u) {
                s_perceptual = mix(s_perceptual, clamp(material_p.x * rough_scale, 0.0, 1.0), w);
            }
            if ((channels & 16u) != 0u) {
                s_metallic = mix(s_metallic, clamp(material_p.y * metal_scale, 0.0, 1.0), w);
            }
        }
    }
    return FdSurface(s_base, s_n, s_perceptual, s_metallic);
}
#endif
const PROBE_RES: i32 = 16;
const PROBE_COLUMNS: u32 = 16u;

#ifndef STUDIO_IBL_OFF
// The lookup direction of a local capture: the ray from `p` along `r` meets the capture's box
// (Lagarde, SIGGRAPH 2012, as `ssr.wgsl`'s `box_lookup`), seen from the box's centre.
fn local_capture_lookup(p: vec3<f32>, r: vec3<f32>, centre: vec3<f32>, half_extent: vec3<f32>) -> vec3<f32> {
    let rd = r + vec3<f32>(1e-6);
    let far_plane = (centre + sign(rd) * half_extent - p) / rd;
    let t = max(min(min(far_plane.x, far_plane.y), far_plane.z), 0.0);
    return p + rd * t - centre;
}

// The local captures around `p` for a reflection along `r`: premultiplied radiance in rgb (over
// `SKY_IBL_SCALE`), a = the share of the lobe they fill. A capture weighs 1 in its box and fades
// to 0 over its fade distance outside it; overlapping captures share by weight.
fn local_capture_reflection(p: vec3<f32>, r: vec3<f32>, perceptual: f32) -> vec4<f32> {
    let lod = perceptual * frame.env_view.w;
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    for (var i = 0u; i < min(u32(frame.env_view.z + 0.5), 4u); i = i + 1u) {
        let centre = frame.capture_center[i].xyz;
        let half_extent = frame.capture_half[i].xyz;
        let outside = length(max(abs(p - centre) - half_extent, vec3<f32>(0.0)));
        let weight = frame.capture_center[i].w * (1.0 - smoothstep(0.0, max(frame.capture_half[i].w, 1e-3), outside));
        if (weight <= 0.0) {
            continue;
        }
        let dir = local_capture_lookup(p, r, centre, half_extent);
        sum += textureSampleLevel(local_capture_tex, ibl_samp, equirect_uv(dir), i32(i), lod) * weight;
        total += weight;
    }
    return sum / max(total, 1.0);
}
#endif

// ---- Area lights fitted to emissive meshes (`emissive_lights.rs`, lane emissive-local) ----
// Heitz, Dupuy, Hill, Neubelt, "Real-Time Polygonal-Light Shading with Linearly Transformed
// Cosines", SIGGRAPH 2016: the clamped-cosine integral of a polygon is Lambert's edge sum, and
// the GGX lobe is a clamped cosine under the matrix M (the polygon is transformed by M⁻¹).
// Mirrors `ltc::polygon_integral` and `emissive_lights::shading_quad`.

// The HDR alpha of a pixel whose emission a fitted area light already carries: the screen-space
// GI skips it (`ssgi.wgsl`), and every SSR path reads it as fully rough (no reflection).
const EMITTER_ALPHA: f32 = 8.0;
const LTC_LAST: f32 = 63.0;
// Metres the interior box grows by for the per-pixel interior-shade test. A volume fitted to a
// tunnel puts the walls, floor and roof exactly on its faces, and the interpolated world position
// of a surface on a face lands a few ulps either side of it (−5.0000005 against a half extent of
// 5): the shade flipped per pixel between the interior floor and the sky, drawing stripes of
// constant depth along the walls that changed with every view (tunnel_scene, lane v020r). 2 cm
// covers that error with margin (an ulp is 1 mm at 10 km) and is far under any wall thickness.
const INTERIOR_FACE_MARGIN_M: f32 = 0.02;

// The widest band (metres) the emitter probe softens its shadow over (ADR-7661). The band grows with the
// hit distance (0.15 hit + 0.05) for the probe's 16 x 16 texels; uncapped it was 0.45 m at a 2.7 m storey, wider
// than a 0.3 m floor slab, and a ceiling panel lit the top of the walls of the storey under its floor
// (`regia_pt` `stacked_floors`). Capped, a receiver 0.25 m past the farthest first hit is dark.
const EMITTER_PROBE_BAND_MAX_M: f32 = 0.25;

// How much of an emitter reaches `v` (receiver − light centre) past the scene: its distance
// probe's first-hit distance against the receiver's, softened over a band and blended between
// the four nearest texels (a static, CPU-traced shadow map).
fn emitter_probe_visibility(tile: u32, v: vec3<f32>) -> f32 {
    let dist = length(v);
    let uv = oct_encode(v / max(dist, 1e-6)) * f32(PROBE_RES) - vec2<f32>(0.5);
    let base = vec2<i32>(i32(tile % PROBE_COLUMNS), i32(tile / PROBE_COLUMNS)) * PROBE_RES;
    let i0 = vec2<i32>(floor(uv));
    let f = uv - floor(uv);
    var vis = 0.0;
    for (var k = 0; k < 4; k = k + 1) {
        let o = vec2<i32>(k & 1, k >> 1u);
        let t = clamp(i0 + o, vec2<i32>(0), vec2<i32>(PROBE_RES - 1));
        let hit = textureLoad(emitter_probes, base + t, 0).r;
        let w = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        vis += w * regia::utils::saturate(1.0 - (dist - hit) / min(0.15 * hit + 0.05, EMITTER_PROBE_BAND_MAX_M));
    }
    return vis;
}

// A fitted emitter's `cone_shadow.w`: its probe code (k + 1 for probe tile k, 0 = none) in x,
// and in y 1 when the baked GI volume holds its light (stored as −1 − code; ADR-1386,
// `emissive_lights::BAKED_EMITTER_TAG`).
fn emitter_tag(li: u32) -> vec2<f32> {
    let w = lights[li].cone_shadow.w;
    let baked = w < -0.5;
    return vec2<f32>(select(w, -1.0 - w, baked), select(0.0, 1.0, baked));
}

// What `emitter_radiance` returns: the whole radiance toward the eye and its diffuse part (the
// rest is the specular lobe).
struct EmitterLight {
    total: vec3<f32>,
    diffuse: vec3<f32>,
}

// M⁻¹ for `perceptual` roughness and n·v, bilinear between the table's texels.
// Under this angular tangent a sphere leaves the LTC tables for the representative point (`LightSourceLtc`): the edge
// sum of `ltc_sphere` is f32 noise much below it. Mirrors `ltc::LTC_SPHERE_MIN_TAN`.
const LTC_SPHERE_MIN_TAN: f32 = 0.02;

fn ltc_inverse(perceptual: f32, no_v: f32) -> mat3x3<f32> {
    let x = sqrt(1.0 - regia::utils::saturate(no_v)) * LTC_LAST;
    let y = regia::utils::saturate(perceptual) * LTC_LAST;
    let x0 = min(floor(x), LTC_LAST - 1.0);
    let y0 = min(floor(y), LTC_LAST - 1.0);
    let fx = x - x0;
    let fy = y - y0;
    let ix = i32(x0);
    let iy = i32(y0);
    let c00 = textureLoad(ltc_mat, vec2<i32>(ix, iy), 0);
    let c10 = textureLoad(ltc_mat, vec2<i32>(ix + 1, iy), 0);
    let c01 = textureLoad(ltc_mat, vec2<i32>(ix, iy + 1), 0);
    let c11 = textureLoad(ltc_mat, vec2<i32>(ix + 1, iy + 1), 0);
    let e = mix(mix(c00, c10, fx), mix(c01, c11, fx), fy);
    return mat3x3<f32>(vec3<f32>(e.x, 0.0, e.z), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(e.y, 0.0, e.w));
}

// Radiance a fitted emitter (`lights[li]`, `position_kind.w` ≥ 4) sends toward the eye from the
// surface at `p`: exact diffuse polygon irradiance, the LTC GGX lobe scaled by the split-sum
// DFG (`spec_mag` = F0·A + B) and by `ms_comp`, the punctual multi-scatter energy compensation
// (Turquin 2019, `pbr_lighting::energy_compensation`; 1 when the material has none). Shadowless; one-sided quads leave the point behind them dark, and the
// range window matches the cluster build's sphere. Returns the total and its diffuse part.
fn emitter_radiance(
    li: u32,
    p: vec3<f32>,
    n: vec3<f32>,
    v_dir: vec3<f32>,
    no_v: f32,
    diffuse_color: vec3<f32>,
    spec_mag: vec3<f32>,
    perceptual: f32,
    ms_comp: vec3<f32>,
    f0: vec3<f32>,
    spec_scale: f32,
) -> EmitterLight {
#ifdef FORWARD_TRIM
    // Adreno (ADR-3145): fitted emitters send no light through the full shader.
    return EmitterLight(vec3<f32>(0.0), vec3<f32>(0.0));
#else
    let dark = EmitterLight(vec3<f32>(0.0), vec3<f32>(0.0));
    let c = lights[li].position_kind.xyz;
    let to_c = c - p;
    let d = length(to_c);
    let range = lights[li].direction_radius.w;
    var window = regia::utils::saturate(1.0 - pow(d / max(range, 1e-4), 4.0));
    if (window <= 0.0) {
        return dark;
    }
    let code = lights[li].position_kind.w - 4.0;
    let shape = floor(code);
    let ex = lights[li].direction_radius.xyz;
    var ey = lights[li].cone_shadow.xyz;
    var taper = fract(code) / 0.96 * 2.0 - 1.0;
    if (shape > 1.5) {
        // Tube: the silhouette quad of the cylinder, turned to face the point.
        let axis = safe_unit(ex);
        let side = -to_c - axis * dot(-to_c, axis);
        var facing = safe_unit(side);
        if (dot(side, side) < 1e-12) {
            facing = safe_unit(select(cross(axis, vec3<f32>(0.0, 1.0, 0.0)), cross(axis, vec3<f32>(1.0, 0.0, 0.0)), abs(axis.y) > 0.9));
        }
        ey = cross(facing, axis) * lights[li].cone_shadow.x;
        taper = 0.0;
    }
    // Cheap rejects before the probe and the polygon: the whole emitter below the receiver's
    // horizon, or the receiver behind a one-sided quad.
    let ex_len = length(ex);
    let ey_len = length(ey) * (1.0 + abs(taper));
    let half_diag = sqrt(ex_len * ex_len + ey_len * ey_len);
    let light_n = cross(ex, ey);
    if (dot(n, to_c) < -half_diag || (shape < 0.5 && dot(-to_c, light_n) <= 0.0)) {
        return dark;
    }
    let probe = emitter_tag(li).x;
    if (probe > 0.5) {
        // The probe looks from its anchor on the emitter (`emissive_lights::probe_origin`):
        // two 8-bit codes (s, t) along the axes in `color_intensity.w`.
        let anchor = lights[li].color_intensity.w;
        let hi = floor(anchor / 256.0);
        let st = vec2<f32>(hi, anchor - hi * 256.0) / 255.0 * 2.0 - vec2<f32>(1.0);
        var origin = c;
        if (shape < 1.5) {
            origin = c + ex * st.x + lights[li].cone_shadow.xyz * st.y;
        }
        window *= emitter_probe_visibility(u32(probe - 0.5), p - origin);
        if (window <= 0.0) {
            return dark;
        }
    }
    let radiance = lights[li].color_intensity.rgb;
    // Rough receivers: the LTC lobe is close to the clamped cosine, so the diffuse form factor
    // stands in for the specular one (saves the second polygon).
    let rough_receiver = perceptual > 0.85;
    var ff_diffuse = 0.0;
    var ff_spec = 0.0;
    if (d > 5.0 * half_diag) {
        // Far field (beyond five times the emitter's size, within ~2 %): a point of projected
        // area A·cos, the specular lobe evaluated as for a point light.
        let l = to_c / max(d, 1e-6);
        let area = 4.0 * ex_len * length(ey);
        let cos_l = select(abs(dot(safe_unit(light_n), l)), max(dot(safe_unit(light_n), -l), 0.0), shape < 0.5);
        let no_l = max(dot(n, l), 0.0);
        ff_diffuse = area * cos_l * no_l / (regia::pbr::lighting::PI * d * d);
        if (rough_receiver || ff_diffuse <= 0.0) {
            ff_spec = ff_diffuse;
        } else {
            let h = safe_unit(l + v_dir);
            let rough = max(perceptual * perceptual, regia::pbr::lighting::MIN_ALPHA);
            let point_spec = regia::pbr::lighting::d_ggx(max(dot(n, h), 0.0), rough)
                * regia::pbr::lighting::v_smith_ggx_correlated(no_v, no_l, rough)
                * regia::pbr::lighting::f_schlick(max(dot(l, h), 0.0), f0)
                * spec_scale
                * ms_comp;
            let diffuse_far = diffuse_color * (vec3<f32>(1.0) - spec_mag) * ff_diffuse;
            // Radiance = BRDF · L · (A cos_l / d²) · n·l = BRDF · L · π · ff.
            return EmitterLight(
                (diffuse_far + point_spec * regia::pbr::lighting::PI * ff_diffuse) * lights[li].color_intensity.rgb * window,
                diffuse_far * lights[li].color_intensity.rgb * window,
            );
        }
    } else {
        let w0 = to_c - ex - ey * (1.0 - taper);
        let w1 = to_c + ex - ey * (1.0 + taper);
        let w2 = to_c + ex + ey * (1.0 + taper);
        let w3 = to_c - ex + ey * (1.0 - taper);
        // Tangent frame: x toward the view in the surface plane (the table's frame), z = n.
        let t_raw = v_dir - n * dot(n, v_dir);
        var t1 = safe_unit(t_raw);
        if (dot(t_raw, t_raw) < 1e-10) {
            t1 = safe_unit(select(cross(n, vec3<f32>(0.0, 1.0, 0.0)), cross(n, vec3<f32>(1.0, 0.0, 0.0)), abs(n.y) > 0.9));
        }
        let t2 = cross(n, t1);
        let to_local = transpose(mat3x3<f32>(t1, t2, n));
        let l0 = to_local * w0;
        let l1 = to_local * w1;
        let l2 = to_local * w2;
        let l3 = to_local * w3;
        ff_diffuse = abs(ltc_polygon(l0, l1, l2, l3));
        if (ff_diffuse <= 0.0) {
            return dark;
        }
        ff_spec = ff_diffuse;
        if (!rough_receiver) {
            let minv = ltc_inverse(perceptual, no_v);
            ff_spec = abs(ltc_polygon(minv * l0, minv * l1, minv * l2, minv * l3));
        }
    }
    let diffuse = diffuse_color * (vec3<f32>(1.0) - spec_mag) * ff_diffuse;
    return EmitterLight((diffuse + spec_mag * ms_comp * ff_spec) * radiance * window, diffuse * radiance * window);
#endif
}

// A sphere light through the LTC tables per unit radiance (`LightSourceLtc`, ADR-2592): the form factors of the
// sphere at `to_c` under the clamped cosine and the GGX lobe, split like `emitter_radiance`.
fn sphere_ltc_light(
    to_c: vec3<f32>,
    radius: f32,
    n: vec3<f32>,
    v_dir: vec3<f32>,
    no_v: f32,
    diffuse_color: vec3<f32>,
    spec_mag: vec3<f32>,
    perceptual: f32,
    ms_comp: vec3<f32>,
) -> EmitterLight {
#ifdef FORWARD_TRIM
    // Adreno (ADR-3145): no LTC sphere light; the caller adds nothing.
    return EmitterLight(vec3<f32>(0.0), vec3<f32>(0.0));
#else
    let t_raw = v_dir - n * dot(n, v_dir);
    var t1 = safe_unit(t_raw);
    if (dot(t_raw, t_raw) < 1e-10) {
        t1 = safe_unit(select(cross(n, vec3<f32>(0.0, 1.0, 0.0)), cross(n, vec3<f32>(1.0, 0.0, 0.0)), abs(n.y) > 0.9));
    }
    let t2 = cross(n, t1);
    let local = transpose(mat3x3<f32>(t1, t2, n)) * to_c;
    let identity = mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 0.0, 1.0));
    let diffuse = diffuse_color * (vec3<f32>(1.0) - spec_mag) * ltc_sphere(identity, local, radius);
    let specular = spec_mag * ms_comp * ltc_sphere(ltc_inverse(perceptual, no_v), local, radius);
    return EmitterLight(diffuse + specular, diffuse);
#endif
}

// Screen-space contact shadows (R = sun visibility, `contact_shadows.wgsl`), at the colour
// target's resolution. Off binds a 1×1 white texture, so the sun term is unchanged.
@group(0) @binding(22) var contact_tex: texture_2d<f32>;

fn contact_shadow(pix: vec2<f32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(contact_tex));
    let texel = clamp(vec2<i32>(pix), vec2<i32>(0), dims - vec2<i32>(1));
    return textureLoad(contact_tex, texel, 0).r;
}
// Translucent shadow casters (colored shadow map, `translucent_shadow.wgsl`): per sun cascade
// the RGB transmittance product and the nearest translucent depth, the same pair for the
// local-light atlas (same tile UVs, a quarter of the edge), and their linear clamp sampler.
// 1² placeholders until a translucent caster appears; read only when `csm.count` y / z say so.
@group(0) @binding(26) var trans_csm_color: texture_2d_array<f32>;
@group(0) @binding(27) var trans_csm_depth: texture_depth_2d_array;
@group(0) @binding(28) var trans_atlas_color: texture_2d<f32>;
@group(0) @binding(29) var trans_atlas_depth: texture_depth_2d;
@group(0) @binding(30) var trans_samp: sampler;
// [0..4] = per cascade the shadow-map UV rect the translucent casters cover ([u0, v0, u1, v1];
// empty = [1, 1, 0, 0]): a receiver outside it has no translucent caster toward the sun.
// [4] = the casters' world bounding sphere (xyz, radius).
@group(0) @binding(31) var<uniform> trans_bounds: array<vec4<f32>, 5>;

// Large meshes' occlusion and bounce proxies (`MeshOccluderBlock`); count 0 = none.
@group(0) @binding(23) var<uniform> mesh_occ: MeshOccluders;

// BEGIN mesh occluders (`regia_render::mesh_occluders`): the functions that read `mesh_occ`.
// Their structs and pure helpers are the `mesh occluder geometry` block of
// `regia::forward::occluder_geometry` (ADR-1552); terrain.wgsl copies both blocks (test
// `occluder_wgsl_copies_match`).
// Vertex `j` (world) of proxy `i`'s polygon `kind`: 0..5 a box face (+A0, −A0, +A1, −A1, +A2,
// −A2; four corners in loop order), 6 the sun shadow on the ground plane.
fn occ_vertex(i: u32, kind: u32, j: u32) -> vec3<f32> {
    if (kind == 6u) {
        let row = mesh_occ.items[i].shadow[j / 2u];
        let xz = select(row.xy, row.zw, (j % 2u) == 1u);
        return vec3<f32>(xz.x, mesh_occ.items[i].center.w, xz.y);
    }
    let k = kind / 2u;
    let s = select(1.0, -1.0, (kind % 2u) == 1u);
    let k1 = (k + 1u) % 3u;
    let k2 = (k + 2u) % 3u;
    let a = mesh_occ.items[i].axes[k];
    let u = mesh_occ.items[i].axes[k1].xyz * mesh_occ.items[i].axes[k1].w;
    let v = mesh_occ.items[i].axes[k2].xyz * mesh_occ.items[i].axes[k2].w;
    let su = select(1.0, -1.0, j == 2u || j == 3u);
    let sv = select(1.0, -1.0, j == 1u || j == 2u);
    return mesh_occ.items[i].center.xyz + a.xyz * (s * a.w) + u * su + v * sv;
}

// Cosine-weighted view factor from `p` (normal `n`) of proxy `i`'s convex polygon `kind`
// (`count` vertices), clipped to the receiver's upper hemisphere: (1/2π)|Σ θ_i n·ĝ_i| over the
// clipped polygon's edges, the closing edge along the clip plane added from exit to entry.
// Each vertex is read and normalised once (carried to the next edge).
fn occ_polygon_ff(i: u32, kind: u32, count: u32, p: vec3<f32>, n: vec3<f32>) -> f32 {
    var sum = 0.0;
    var exit = vec3<f32>(0.0);
    var entry = vec3<f32>(0.0);
    var crossings = 0u;
    var inside = 0u;
    var a = occ_vertex(i, kind, 0u) - p;
    var an = occ_unit(a);
    var da = dot(a, n);
    for (var j = 0u; j < count; j++) {
        let b = occ_vertex(i, kind, (j + 1u) % count) - p;
        let bn = occ_unit(b);
        let db = dot(b, n);
        if (da >= 0.0) {
            inside += 1u;
        }
        if (da >= 0.0 && db >= 0.0) {
            sum += occ_edge(an, bn, n);
        } else if (da >= 0.0) {
            let x = occ_unit(mix(a, b, da / (da - db)));
            sum += occ_edge(an, x, n);
            exit = x;
            crossings += 1u;
        } else if (db >= 0.0) {
            let x = occ_unit(mix(a, b, da / (da - db)));
            sum += occ_edge(x, bn, n);
            entry = x;
            crossings += 1u;
        }
        a = b;
        an = bn;
        da = db;
    }
    if (inside == 0u) {
        return 0.0;
    }
    if (crossings == 2u) {
        sum += occ_edge(exit, entry, n);
    }
    return abs(sum) * 0.15915494;
}

// The occluders listed in the grid cell under `p` (bit i = occluder i): a superset of the reach
// spheres that hold it, 0 outside the grid (which bounds every sphere).
fn mesh_occluder_cell_mask(p: vec3<f32>) -> u32 {
    let g = mesh_occ.grid;
    let xz = (p.xz - g.xy) * g.z;
    if (!(all(xz >= vec2<f32>(0.0)) && all(xz <= vec2<f32>(32.0)))) {
        return 0u;
    }
    let c = min(vec2<u32>(xz), vec2<u32>(31u));
    let k = c.y * 32u + c.x;
    let q = mesh_occ.cells[k >> 2u];
    let lane = k & 3u;
    // Component by `select`, not `q[lane]` (no dynamic vector index; Mali-G78).
    return select(select(q.x, q.y, lane == 1u), select(q.z, q.w, lane == 3u), lane >= 2u);
}

// How far occluder `i`'s full term has faded into its contact blob (`near_fade`; 0 = none).
fn occ_near_fade(i: u32) -> f32 {
    let q = mesh_occ.near_fade[i >> 2u];
    let lane = i & 3u;
    // Component by `select`, not `q[lane]` (no dynamic vector index; Mali-G78).
    return select(select(q.x, q.y, lane == 1u), select(q.z, q.w, lane == 3u), lane >= 2u);
}

// The contact blob of near occluder `i` at `p` (`MeshOccluder::contact_blob` from the packed
// item: the XZ-projected half diagonal and the top above the ground).
fn occ_item_blob(i: u32, p: vec3<f32>, n: vec3<f32>) -> f32 {
    let a0 = mesh_occ.items[i].axes[0];
    let a1 = mesh_occ.items[i].axes[1];
    let a2 = mesh_occ.items[i].axes[2];
    let r2 = a0.w * a0.w * max(1.0 - a0.y * a0.y, 0.0) + a1.w * a1.w * max(1.0 - a1.y * a1.y, 0.0)
        + a2.w * a2.w * max(1.0 - a2.y * a2.y, 0.0);
    let up = abs(a0.y) * a0.w + abs(a1.y) * a1.w + abs(a2.y) * a2.w;
    let c = mesh_occ.items[i].center;
    return occ_blob(p, n, c.xz, sqrt(r2), c.w, c.y + up - c.w);
}

// The far band's contact blobs at `p` (ADR-7970): x = the sky share they hide (summed), y =
// the fill visibility they leave (the product). Sorted by x: a binary search finds the first
// blob within the largest reach of `p.x`, and the walk stops past it.
fn occ_far_blobs(p: vec3<f32>, n: vec3<f32>) -> vec2<f32> {
    var out = vec2<f32>(0.0, 1.0);
    let count = min(u32(mesh_occ.header.z + 0.5), 128u);
    let reach = mesh_occ.grid.w;
    var lo = 0u;
    var hi = count;
    while (lo < hi) {
        let mid = (lo + hi) / 2u;
        if (mesh_occ.far[mid].x < p.x - reach) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    for (var i = lo; i < count; i++) {
        let b = mesh_occ.far[i];
        if (b.x > p.x + reach) {
            break;
        }
        let t = mesh_occ.far_top[i >> 2u];
        let lane = i & 3u;
        let top = select(select(t.x, t.y, lane == 1u), select(t.z, t.w, lane == 3u), lane >= 2u);
        let o = occ_blob(p, n, b.xy, b.z, b.w, top);
        out = vec2<f32>(out.x + o, out.y * (1.0 - o));
    }
    return out;
}

// The light a point receives from the frame's large-mesh proxies. `terrain`: the receiver is
// the ground (no shadow polygon; the box hides sky); otherwise a mesh (the box's sun shadow on
// its ground plane removes the sun's share of the ground the IBL assumed lit).
fn mesh_occluder_light(p: vec3<f32>, n: vec3<f32>, terrain: bool) -> OccLight {
    var delta = vec3<f32>(0.0);
    var fill_vis = 1.0;
    let sky = mesh_occ.sky.rgb;
    let ground_sun = mesh_occ.ground_sun.rgb;
    let fill_dir = mesh_occ.fill_dir.xyz;
    let fill_on = mesh_occ.fill_dir.w > 0.5 && dot(n, fill_dir) > 0.0;
    // A tight pass over the reach spheres of the occluders listed in this point's grid cell
    // first: the full body below runs only for the occluders that reach it (most pixels: none).
    var mask = 0u;
    var listed = mesh_occluder_cell_mask(p);
    while (listed != 0u) {
        let i = firstTrailingBit(listed);
        listed &= listed - 1u;
        let b = mesh_occ.bounds[i];
        let d = p - b.xyz;
        if (dot(d, d) < b.w * b.w) {
            mask |= 1u << i;
        }
    }
    while (mask != 0u) {
        let i = firstTrailingBit(mask);
        mask &= mask - 1u;
        // Near the far band's boundary the full term fades into the contact blob (ADR-7970);
        // `full` is exactly 1 otherwise, so the terms below are unchanged.
        let fade = occ_near_fade(i);
        let full = 1.0 - fade;
        var fill_near = 1.0;
        let c = mesh_occ.items[i].center.xyz;
        let r = mesh_occ.items[i].faces[0].w;
        let dist = distance(p, c);
        if (dist < 5.5 * r) {
            let w = (1.0 - smoothstep(4.0 * r, 5.5 * r, dist)) * full;
            let a0 = mesh_occ.items[i].axes[0];
            let a1 = mesh_occ.items[i].axes[1];
            let a2 = mesh_occ.items[i].axes[2];
            let h = vec3<f32>(a0.w, a1.w, a2.w);
            let local = vec3<f32>(dot(p - c, a0.xyz), dot(p - c, a1.xyz), dot(p - c, a2.xyz));
            let inside = all(abs(local) < h + vec3<f32>(0.02));
            var q = p;
            if (inside && terrain) {
                // Ground under the box (between the wheels): seen from just below its bottom.
                q.y = c.y - dot(abs(vec3<f32>(a0.y, a1.y, a2.y)), h) - 0.01;
            }
            if (!inside || terrain) {
                let ql = vec3<f32>(dot(q - c, a0.xyz), dot(q - c, a1.xyz), dot(q - c, a2.xyz));
                // Exact clipped polygons within 1.5 × the largest half extent of the box's
                // surface; beyond, each face as a disc (far cheaper).
                let near = length(max(abs(ql) - h, vec3<f32>(0.0))) < 1.5 * max(h.x, max(h.y, h.z));
                for (var k = 0u; k < 3u; k++) {
                    // Axis and half extent from the uniform array, not `ql[k]` / `h[k]`: a
                    // dynamic vector index lost Mali-G78's device (terrain splat weights).
                    let ak4 = mesh_occ.items[i].axes[k];
                    let ak = ak4.xyz;
                    let l = dot(q - c, ak);
                    if (abs(l) <= ak4.w + 1e-4) {
                        continue;
                    }
                    let f = 2u * k + select(0u, 1u, l < 0.0);
                    let s = select(-1.0, 1.0, l > 0.0);
                    let fc = c + ak * (s * ak4.w) - q;
                    var ff = 0.0;
                    if (near) {
                        ff = occ_polygon_ff(i, f, 4u, q, n);
                    } else {
                        let area = 4.0 * mesh_occ.items[i].axes[(k + 1u) % 3u].w * mesh_occ.items[i].axes[(k + 2u) % 3u].w;
                        ff = occ_disc_ff(area, fc, ak * s, n);
                    }
                    if (ff <= 0.0) {
                        continue;
                    }
                    var hidden = sky;
                    if (!terrain) {
                        let t = smoothstep(-0.15, 0.15, normalize(fc).y);
                        hidden = mix(mesh_occ.ground.rgb - ground_sun * 0.5, sky, t);
                    }
                    delta += (mesh_occ.items[i].faces[f].rgb - hidden) * (ff * w);
                }
                // The fill's rays only reach the box when its bounding sphere meets their cone.
                let cq = c - q;
                let along = dot(cq, fill_dir);
                let spread = mesh_occ.header.y * 0.7;
                let reach = r + max(along, 0.0) * spread * 1.05 + 0.01;
                if (fill_on && along > -r && dot(cq, cq) - along * along < reach * reach) {
                    // Five rays over the fill's disc.
                    var t1 = cross(fill_dir, vec3<f32>(0.0, 1.0, 0.0));
                    if (dot(t1, t1) < 1e-6) {
                        t1 = vec3<f32>(1.0, 0.0, 0.0);
                    }
                    t1 = normalize(t1);
                    let t2 = cross(fill_dir, t1);
                    var hits = 0.0;
                    for (var j = 0u; j < 5u; j++) {
                        var off = vec2<f32>(0.0);
                        if (j == 1u) { off = vec2<f32>(spread, 0.0); }
                        if (j == 2u) { off = vec2<f32>(-spread, 0.0); }
                        if (j == 3u) { off = vec2<f32>(0.0, spread); }
                        if (j == 4u) { off = vec2<f32>(0.0, -spread); }
                        let dw = normalize(fill_dir + t1 * off.x + t2 * off.y);
                        let dl = vec3<f32>(dot(dw, a0.xyz), dot(dw, a1.xyz), dot(dw, a2.xyz));
                        hits += select(0.0, 1.0, occ_slab_hit(ql, dl, h));
                    }
                    fill_near = 1.0 - hits * 0.2;
                }
            }
        }
        let verts = min(u32(mesh_occ.items[i].faces[1].w + 0.5), 6u);
        let ground_y = mesh_occ.items[i].center.w;
        if (!terrain && verts >= 3u && p.y > ground_y + 0.01) {
            let pc = vec3<f32>(mesh_occ.items[i].faces[2].w, ground_y, mesh_occ.items[i].faces[3].w);
            let rp = max(mesh_occ.items[i].faces[4].w, 0.1);
            let dp = distance(p, pc);
            if (dp < 4.5 * rp) {
                let w = (1.0 - smoothstep(3.0 * rp, 4.5 * rp, dp)) * full;
                var ff = 0.0;
                if (dp < 2.0 * rp) {
                    ff = occ_polygon_ff(i, 6u, verts, p, n);
                } else {
                    // Far field: the polygon (area in faces[5].w) as a disc.
                    ff = occ_disc_ff(mesh_occ.items[i].faces[5].w, pc - p, vec3<f32>(0.0, 1.0, 0.0), n);
                }
                delta -= ground_sun * (ff * w);
            }
        }
        if (fade > 0.0) {
            // The rest is the proxy's contact blob: it hides the sky and, mixed with the full
            // term's fill shadow, the fill.
            let o = occ_item_blob(i, p, n);
            delta -= sky * (fade * o);
            fill_vis *= mix(1.0 - o, fill_near, full);
        } else {
            fill_vis *= fill_near;
        }
    }
    if (mesh_occ.header.z > 0.5) {
        let blobs = occ_far_blobs(p, n);
        delta -= sky * blobs.x;
        fill_vis *= blobs.y;
    }
    return OccLight(delta, fill_vis);
}

// Share of a reflected ray's ground hit that lies in a proxy's sun shadow, softened over the
// specular lobe's footprint (`rough` = GGX alpha): the IBL's lower half shows lit ground there.
fn mesh_occluder_reflection_shadow(p: vec3<f32>, r: vec3<f32>, rough: f32) -> f32 {
    var cover = 0.0;
    let count = min(u32(mesh_occ.header.x + 0.5), 32u);
    if (r.y > -1e-3) {
        return 0.0;
    }
    for (var i = 0u; i < count; i++) {
        // One vec4 per occluder decides most of them (`shadow_bounds`).
        let sb = mesh_occ.shadow_bounds[i];
        let ground_y = sb.w;
        if (sb.z < 0.0 || p.y <= ground_y + 0.01) {
            continue;
        }
        let t = (ground_y - p.y) / r.y;
        let hit = p.xz + r.xz * t;
        let width = max(t * rough * 2.0, 0.05);
        if (distance(hit, sb.xy) > sb.z + 2.0 * width) {
            continue;
        }
        let verts = min(u32(mesh_occ.items[i].faces[1].w + 0.5), 6u);
        var sd = 1e9;
        for (var j = 0u; j < verts; j++) {
            let a = occ_vertex(i, 6u, j).xz;
            let b = occ_vertex(i, 6u, (j + 1u) % verts).xz;
            let e = b - a;
            let inward = vec2<f32>(-e.y, e.x) / max(length(e), 1e-6);
            sd = min(sd, dot(hit - a, inward));
        }
        cover = max(cover, smoothstep(-width, width, sd) * (1.0 - occ_near_fade(i)));
    }
    return cover;
}
// END mesh occluders

const GROUND_BOUNCE_CELLS: i32 = 3;

// Grid coordinate of `world` in `GroundBounce` level `level` (cells, centred), its size in
// cells, and the coverage: 1 inside, falling to 0 over the last cell, 0 outside or absent.
fn ground_bounce_grid(world: vec3<f32>, level: u32) -> vec4<f32> {
    let h = ground_bounce[level];
    let size = h.w;
    if (size < 1.5) {
        return vec4<f32>(0.0);
    }
    let g = (world.xz - h.xy) / h.z - vec2<f32>(0.5);
    let edge = min(min(g.x + 0.5, g.y + 0.5), min(size - 0.5 - g.x, size - 0.5 - g.y));
    return vec4<f32>(g, size, clamp(edge, 0.0, 1.0));
}

// Bilinear cell value (rgba) of a level at grid coordinate `grid` (`ground_bounce_grid`),
// cells starting at `base`.
fn ground_bounce_cells(grid: vec4<f32>, base: i32) -> vec4<f32> {
    if (grid.w <= 0.0) {
        return vec4<f32>(0.0);
    }
    let size = i32(grid.z + 0.5);
    let g = grid.xy;
    let i0 = clamp(vec2<i32>(floor(g)), vec2<i32>(0), vec2<i32>(size - 1));
    let i1 = min(i0 + vec2<i32>(1), vec2<i32>(size - 1));
    let f = clamp(g - floor(g), vec2<f32>(0.0), vec2<f32>(1.0));
    let a = ground_bounce[base + i0.x + i0.y * size];
    let b = ground_bounce[base + i1.x + i0.y * size];
    let c = ground_bounce[base + i0.x + i1.y * size];
    let d = ground_bounce[base + i1.x + i1.y * size];
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

// Cells per level as packed (0 when the level is absent).
fn ground_bounce_sizes() -> vec2<i32> {
    return vec2<i32>(i32(ground_bounce[0].w + 0.5), i32(ground_bounce[1].w + 0.5));
}

// Light the shared surface (terrain) bounces onto the ground here (`GroundBounce`): the fine
// grid near the camera, blended into the coarse one around it.
fn ground_bounce_at(world: vec3<f32>) -> vec3<f32> {
    let n = ground_bounce_sizes();
    let gf = ground_bounce_grid(world, 0u);
    let gc = ground_bounce_grid(world, 1u);
    let fine = ground_bounce_cells(gf, GROUND_BOUNCE_CELLS).rgb;
    let coarse = ground_bounce_cells(gc, GROUND_BOUNCE_CELLS + n.x * n.x).rgb;
    return mix(coarse * gc.w, fine, gf.w);
}

// The terrain's horizon at `world` (`GroundBounceLevel::horizon`): tan θ_h per diagonal sector
// (45°, 135°, 225°, 315° from +X toward +Z), blended from the fine grid into the coarse one and
// fading to open sky (0) outside both or without horizons.
fn terrain_horizon_tan(world: vec3<f32>) -> vec4<f32> {
    // The switch as its readers test it, negated (`!(w > 0.5)`, not `w < 0.5`; it is 0 or 1):
    // `shade_forward` calls this under `if (ground_bounce[2].w > 0.5)`, the shape FXC's optimizer
    // does not come back from (ADR-5971). FXC lets a float read from a storage buffer off today,
    // which is not what should keep this shader compiling.
    if (!(ground_bounce[2].w > 0.5)) {
        return vec4<f32>(0.0);
    }
    let sizes = ground_bounce_sizes();
    let h_base = GROUND_BOUNCE_CELLS + sizes.x * sizes.x + sizes.y * sizes.y;
    let gf = ground_bounce_grid(world, 0u);
    let gc = ground_bounce_grid(world, 1u);
    let fine = ground_bounce_cells(gf, h_base);
    let coarse = ground_bounce_cells(gc, h_base + sizes.x * sizes.x);
    // Outside both grids the horizon fades to open sky.
    return max(mix(coarse * gc.w, fine, gf.w), vec4<f32>(0.0));
}

// Unit vector of diagonal sector `a` at elevation `e`.
fn horizon_sector_dir(a: u32, e: f32) -> vec3<f32> {
    let d = 0.70710677;
    var xz = vec2<f32>(d, d);
    if (a == 1u) {
        xz = vec2<f32>(-d, d);
    } else if (a == 2u) {
        xz = vec2<f32>(-d, -d);
    } else if (a == 3u) {
        xz = vec2<f32>(d, -d);
    }
    return vec3<f32>(xz.x * cos(e), sin(e), xz.y * cos(e));
}

// The sky the terrain hides from a receiver facing `n` (E/π, the IBL's units): per diagonal
// sector, the band between the horizon and θ_h, in two sub-bands each seen at its middle,
// `L(b) · max(n·b, 0) · (sin e₁ − sin e₀) / 2` (the band's solid angle is π/2 · Δ sin e; L from
// the unblurred sky, mip 0 of the prefiltered map: a blurred mip mixes the dark ground below the
// horizon into the bright horizon band and removed half of what the reference path tracer
// shows hidden). `w` returns the weights summed (the hidden share of the receiver's view).
fn terrain_sky_occlusion(world: vec3<f32>, n: vec3<f32>, ibl_scale: f32) -> vec4<f32> {
    let tan_h = terrain_horizon_tan(world);
    var removed = vec3<f32>(0.0);
    var weight = 0.0;
    for (var a: u32 = 0u; a < 4u; a++) {
        let theta = atan(gi_component(tan_h, a));
        if (theta <= 1e-3) {
            continue;
        }
        for (var k: u32 = 0u; k < 2u; k++) {
            let e0 = theta * 0.5 * f32(k);
            let e1 = theta * 0.5 * f32(k + 1u);
            let b = horizon_sector_dir(a, 0.5 * (e0 + e1));
            let w = max(dot(n, b), 0.0) * (sin(e1) - sin(e0)) * 0.5;
            if (w <= 0.0) {
                continue;
            }
            removed += textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(b), 0.0).rgb * ibl_scale * w;
            weight += w;
        }
    }
    return vec4<f32>(removed, weight);
}

// Whether a reflected ray `r` clears the terrain's horizon in its sector (soft over ±2°): the
// sky reflection below it sees the terrain instead.
fn terrain_reflection_visibility(world: vec3<f32>, r: vec3<f32>) -> f32 {
    let tan_h = terrain_horizon_tan(world);
    let d = 0.70710677;
    var w = max(vec4<f32>(d * (r.x + r.z), d * (r.z - r.x), -d * (r.x + r.z), d * (r.x - r.z)), vec4<f32>(0.0));
    let sw = w.x + w.y + w.z + w.w;
    w = select(vec4<f32>(0.25), w / max(sw, 1e-6), sw > 1e-4);
    let horizon = atan(dot(w, tan_h));
    let elevation = asin(clamp(r.y, -1.0, 1.0));
    return smoothstep(horizon - 0.035, horizon + 0.035, elevation);
}

// Constant indices only. `probe.depth_mean[row]` with a dynamic `row` makes
// naga 30 emit a Function variable of the array, which SPIR-V 1.6 rejects
// (VUID-StandaloneSpirv-None-10684, ADR-026).
fn probe_moment(probe: GiProbe, cell: u32, mean: bool) -> f32 {
    let row = cell / 4u;
    let col = cell % 4u;
    return gi_component(probe_depth_row(probe, row, mean), col);
}

// Bilinear mean and mean-square. Matches `depth_moments` in `gi.rs`.
// Majercik et al. bilinear-filter the octahedral depth; a point sample of
// this 4×4 map is a step, and the cap corner sat on that step.
fn depth_moments(probe: GiProbe, dir: vec3<f32>) -> vec2<f32> {
    let uv = regia::gi::oct_uv(dir);
    let fx = clamp(uv.x * 4.0 - 0.5, 0.0, 3.0);
    let fy = clamp(uv.y * 4.0 - 0.5, 0.0, 3.0);
    let x0 = u32(floor(fx));
    let y0 = u32(floor(fy));
    let x1 = min(x0 + 1u, 3u);
    let y1 = min(y0 + 1u, 3u);
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let m00 = probe_moment(probe, y0 * 4u + x0, true);
    let m10 = probe_moment(probe, y0 * 4u + x1, true);
    let m01 = probe_moment(probe, y1 * 4u + x0, true);
    let m11 = probe_moment(probe, y1 * 4u + x1, true);
    let s00 = probe_moment(probe, y0 * 4u + x0, false);
    let s10 = probe_moment(probe, y0 * 4u + x1, false);
    let s01 = probe_moment(probe, y1 * 4u + x0, false);
    let s11 = probe_moment(probe, y1 * 4u + x1, false);
    let mean = mix(mix(m00, m10, tx), mix(m01, m11, tx), ty);
    let msq = mix(mix(s00, s10, tx), mix(s01, s11, tx), ty);
    return vec2<f32>(mean, msq);
}

fn probe_depth_row(probe: GiProbe, row: u32, mean: bool) -> vec4<f32> {
    // One return after the switch: FXC (Direct3D 12 without DXC) cannot prove that a switch
    // whose every case returns covers all paths (error X3507), so the pipeline would not compile.
    var out = select(probe.depth_msq[3], probe.depth_mean[3], mean);
    switch row {
        case 0u: { out = select(probe.depth_msq[0], probe.depth_mean[0], mean); }
        case 1u: { out = select(probe.depth_msq[1], probe.depth_mean[1], mean); }
        case 2u: { out = select(probe.depth_msq[2], probe.depth_mean[2], mean); }
        default: {}
    }
    return out;
}

// Probe irradiance (E/π) in rgb; a = confidence (1 inside the probe box, fading to 0
// one cell outside it, 0 when every probe was rejected). The caller blends the IBL
// back in by 1 − a, so geometry outside the volume keeps sky light.
// `fade_start` (cells) is how far past the outermost probes full confidence holds.
fn sample_ddgi(world: vec3<f32>, n_in: vec3<f32>, fade_start: f32) -> vec4<f32> {
    let n = normalize(n_in);
    let biased = world + n * gi_grid.spacing.w;
    let dims = gi_grid.dims.xyz;
    let local = (biased - gi_grid.origin.xyz) / max(gi_grid.spacing.xyz, vec3<f32>(1e-4));
    let base_f = clamp(floor(local), vec3<f32>(0.0), vec3<f32>(dims) - vec3<f32>(1.0));
    let base = vec3<u32>(base_f);
    let next = min(base + vec3<u32>(1u), dims - vec3<u32>(1u));
    let frac = clamp(local - base_f, vec3<f32>(0.0), vec3<f32>(1.0));
    var accum = vec3<f32>(0.0);
    var weight_sum = 0.0;
    var trilinear_sum = 0.0;
    var seen_sum = 0.0;
    for (var dz: u32 = 0u; dz < 2u; dz = dz + 1u) {
        for (var dy: u32 = 0u; dy < 2u; dy = dy + 1u) {
            for (var dx: u32 = 0u; dx < 2u; dx = dx + 1u) {
                let ix = select(base.x, next.x, dx == 1u);
                let iy = select(base.y, next.y, dy == 1u);
                let iz = select(base.z, next.z, dz == 1u);
                let wx = select(1.0 - frac.x, frac.x, dx == 1u);
                let wy = select(1.0 - frac.y, frac.y, dy == 1u);
                let wz = select(1.0 - frac.z, frac.z, dz == 1u);
                let idx = ix + iy * dims.x + iz * dims.x * dims.y;
                if (idx >= gi_grid.dims.w) {
                    continue;
                }
                let probe_pos = gi_grid.origin.xyz + vec3<f32>(f32(ix), f32(iy), f32(iz)) * gi_grid.spacing.xyz;
                // Depth rays leave the probe, so visibility looks back at the receiver.
                let from_probe = biased - probe_pos;
                let dist = max(length(from_probe), 1e-4);
                let dir = from_probe / dist;
                let wrap = clamp(dot(n, -dir) * 0.5 + 0.5, 0.0, 1.0);
                trilinear_sum += wx * wy * wz * wrap * wrap;
                let probe = gi_probes[idx];
                let moments = depth_moments(probe, dir);
                let vis = regia::gi::chebyshev(moments.x, moments.y, dist);
                seen_sum += wx * wy * wz * wrap * wrap * smoothstep(0.05, 0.6, vis);
                // Do not let normalization revive a probe the depth test rejected.
                if (vis < 0.15) {
                    continue;
                }
                let weight = wx * wy * wz * wrap * wrap * vis;
                if (weight <= 0.0) {
                    continue;
                }
                accum += vec3<f32>(
                    regia::gi::irradiance_over_pi(probe.sh_r, n),
                    regia::gi::irradiance_over_pi(probe.sh_g, n),
                    regia::gi::irradiance_over_pi(probe.sh_b, n),
                ) * weight;
                weight_sum += weight;
            }
        }
    }
    if (weight_sum <= 1e-5) {
        return vec4<f32>(0.0);
    }
    let outside = max(max(-local, local - (vec3<f32>(dims) - vec3<f32>(1.0))), vec3<f32>(0.0));
    var confidence = clamp(1.0 + fade_start - max(outside.x, max(outside.y, outside.z)), 0.0, 1.0);
    if (fade_start > 0.0) {
        // Past the outermost probes the field is extrapolated; trust it only as far as
        // the probes the visibility test accepted (a wall behind them falls back to sky).
        // A smooth visibility weight: the binary rejection below 0.15 drew bands.
        confidence *= clamp(seen_sum / max(trilinear_sum, 1e-5), 0.0, 1.0);
    }
    return vec4<f32>(accum / weight_sum * gi_grid.origin.w, confidence);
}

// BEGIN cone specular occlusion (`ssao/bent_normal.rs`, ADR-1375). Mirrored on the CPU.
// The bent normal (the mean unoccluded direction) comes from the SSAO kernel as a lean from the
// depth normal; the visible directions are a cone around it that keeps the AO, and the specular
// occlusion is the share of the specular lobe's cone inside it (Jimenez, Wu, Pesce, Jarabek,
// "Practical Realtime Strategies for Accurate Indirect Occlusion", SIGGRAPH 2016 § 5), the caps'
// intersection after Oat & Sander, "Ambient Aperture Lighting", I3D 2007.

// END cone specular occlusion

// Receiver-side sun-cascade bias (lane csm-far; deterministic across GPUs).
//
// The shadow *writer* pipeline carries no depth bias: Vulkan's
// `depthBiasSlopeFactor` is in implementation-dependent units, so the same
// scene self-shadowed on llvmpipe but not on NVIDIA hardware. All biasing
// happens here in WGSL, identical arithmetic on every driver:
// 1. receiver-plane depth (Isidoro 2006, "Shadow Mapping: GPU-based Tips and
//    Techniques"): every filter texel compares against the receiver's own
//    plane at that texel's centre (`regia::shadows::shadow_pcf_plane`,
//    `shadow_pcss_sun`), so a planar receiver needs no slope bias and no
//    bilinear-footprint bias. The plane is the rasterised triangle's
//    (`csm_geometric_normal`), not the interpolated or normal-mapped one;
// 2. a constant depth bias and a normal offset (Holbert, "Normal Offset Shadows",
//    GDC 2011; ∝ sin θ), both a fraction of the cascade's texel and capped in
//    metres on cascades fine enough to resolve one unit's shadow on another
//    (texel ≤ CSM_FINE_TEXEL_M). The old bias was (2 + 4·slope) texels plus a
//    1.5-texel offset: on the 0.3–1.1 m texels past ~110 m that is metres, and
//    a caster 0.4 m up-sun of its receiver cast nothing
//    (`regia_pt/tests/csm_far_sphere_pair.rs`). Coarser cascades (the
//    300 m+ one, metres per texel) cannot resolve such gaps anyway; they ramp
//    up to a 1-texel constant and a 0.5-texel offset, so a low-poly
//    smooth-shaded surface (a 20 m terrain-mesh facet) does not stipple. A
//    1.5-texel offset there (6 m on a 4 m texel) lit the far ends of
//    16°-sun hill shadows (`lookdev/tests/default_look_sun_vs_pt.rs`).
const CSM_DEPTH_BIAS_TEXELS: f32 = 1.0;
const CSM_DEPTH_BIAS_MAX_M: f32 = 0.03;
const CSM_NORMAL_OFFSET_TEXELS: f32 = 1.0;
const CSM_NORMAL_OFFSET_MAX_M: f32 = 0.04;
const CSM_FINE_TEXEL_M: f32 = 0.5;
const CSM_COARSE_TEXEL_M: f32 = 1.5;
const CSM_COARSE_DEPTH_BIAS_TEXELS: f32 = 1.0;
const CSM_COARSE_NORMAL_OFFSET_TEXELS: f32 = 0.5;
// Receiver-plane slope cap: tan θ = 12 (a plane edge-on to the sun would push
// the compare across metres of empty space; the band below n·l ≈ 0.08 is dark).
const CSM_MAX_TAN: f32 = 12.0;
// Curvature allowance of the receiver plane: the compare depth drops 0.25 texel per texel of
// distance from the receiver (`regia::shadows::plane_depth`), a 14° bend, plus half the
// plane's own slope. Without it a 9-texel PCSS disc on a 3 m far-cascade texel stippled
// rolling snow, and a sphere's grazing-lit underside (n·l 0.1–0.2) took the ground beside
// it as a blocker. On a 0.3 m texel and n·l = 0.7 a 1.5-texel filter gives ~0.3 m at its rim.
// The plane's error grows with the square of the distance (d² / 2R on a curve of radius R),
// so on fine cascades the allowance fades with the texel: full at 0.3 m texels and up, a
// tenth on a 3 cm texel (a cockpit's rail then keeps its shadow on the wall beside it).
const CSM_BEND_TEXELS: f32 = 0.25;
const CSM_BEND_SLOPE: f32 = 0.5;
const CSM_BEND_FULL_TEXEL_M: f32 = 0.3;

// Constant indices only. A dynamic `csm.cascades[i].light_view_proj` is a
// mat4, which naga emits as a Function array<vec4, 4> (ADR-026).
fn cascade_split(index: u32) -> f32 {
    switch index {
        case 0u: { return csm.cascades[0].split_distance; }
        case 1u: { return csm.cascades[1].split_distance; }
        case 2u: { return csm.cascades[2].split_distance; }
        default: { return csm.cascades[3].split_distance; }
    }
}

fn cascade_bias(index: u32) -> vec3<f32> {
    switch index {
        case 0u: { return csm.cascades[0].bias; }
        case 1u: { return csm.cascades[1].bias; }
        case 2u: { return csm.cascades[2].bias; }
        default: { return csm.cascades[3].bias; }
    }
}

fn cascade_view_proj(index: u32) -> mat4x4<f32> {
    switch index {
        case 0u: { return csm.cascades[0].light_view_proj; }
        case 1u: { return csm.cascades[1].light_view_proj; }
        case 2u: { return csm.cascades[2].light_view_proj; }
        default: { return csm.cascades[3].light_view_proj; }
    }
}

// Sun angular radius used for the PCSS penumbra (`specs/13` § 6): the real sun's 0.27°
// (`AtmosphereParameters::sun_angular_radius`, mirrored by
// `regia_render::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN`). It was 1°, which spread a thin
// caster's shadow 3.7× wider than the path tracer's and Cycles' on DO27's X-12 (the fin
// over the cargo box lost its corners). The filter's texel floor (`shadow_pcss_sun`)
// already hides texel steps on distant cascades.
const SUN_ANGULAR_RADIUS_TAN: f32 = 0.004675;

// The rasterised triangle's normal from the world position's screen derivatives
// (uniform control flow only), turned to the side of `shading_n`. Falls back to
// `shading_n` where the derivatives degenerate.
fn csm_geometric_normal(world: vec3<f32>, shading_n: vec3<f32>) -> vec3<f32> {
    let c = cross(dpdx(world), dpdy(world));
    let len = length(c);
    if (!(len > 1e-12)) {
        return shading_n;
    }
    let g = c / len;
    return select(-g, g, dot(g, shading_n) >= 0.0);
}

// The sun cascades' maps by index. The cascade filters below read every texel through these,
// so a driver compiles each filter once for all four cascades. With the map chosen before the
// filter (one `shadow_pcf_plane` / `shadow_pcss_sun` call per map) every forward pipeline
// carried four copies of both, about a seventh of its cold compile on NVIDIA. The filters are
// `regia::shadows::shadow_pcf_plane` and `shadow_pcss_sun` tap for tap: keep them in step.
fn cascade_dims(cascade: u32) -> vec2<u32> {
    switch cascade {
        case 0u: { return textureDimensions(shadow_map_0, 0); }
        case 1u: { return textureDimensions(shadow_map_1, 0); }
        case 2u: { return textureDimensions(shadow_map_2, 0); }
        default: { return textureDimensions(shadow_map_3, 0); }
    }
}

fn cascade_gather(cascade: u32, uv: vec2<f32>) -> vec4<f32> {
    switch cascade {
        case 0u: { return textureGather(shadow_map_0, ao_samp, uv); }
        case 1u: { return textureGather(shadow_map_1, ao_samp, uv); }
        case 2u: { return textureGather(shadow_map_2, ao_samp, uv); }
        default: { return textureGather(shadow_map_3, ao_samp, uv); }
    }
}

fn cascade_load(cascade: u32, c: vec2<i32>) -> f32 {
    switch cascade {
        case 0u: { return textureLoad(shadow_map_0, c, 0); }
        case 1u: { return textureLoad(shadow_map_1, c, 0); }
        case 2u: { return textureLoad(shadow_map_2, c, 0); }
        default: { return textureLoad(shadow_map_3, c, 0); }
    }
}

// `regia::shadows::plane_quad_lit` on cascade `cascade`'s map.
fn cascade_quad_lit(
    cascade: u32,
    c: vec2<i32>,
    inv_dims: vec2<f32>,
    z: f32,
    slope: vec2<f32>,
    bend: f32,
    d: vec2<f32>,
) -> vec4<f32> {
    let g = cascade_gather(cascade, vec2<f32>(c + vec2<i32>(1)) * inv_dims);
    return regia::shadows::plane_quad_compare(g, z, slope, bend, d);
}

// `regia::shadows::plane_bilinear` on cascade `cascade`'s map.
fn cascade_bilinear(
    cascade: u32,
    inv_dims: vec2<f32>,
    p: vec2<f32>,
    uv_px: vec2<f32>,
    z: f32,
    slope: vec2<f32>,
    bend: f32,
) -> f32 {
    let q = p - vec2<f32>(0.5);
    let b = floor(q);
    let f = q - b;
    let s = cascade_quad_lit(cascade, vec2<i32>(b), inv_dims, z, slope, bend, b + vec2<f32>(0.5) - uv_px);
    return mix(mix(s.x, s.y, f.x), mix(s.z, s.w, f.x), f.y);
}

// `regia::shadows::shadow_pcf_plane` on cascade `cascade`'s map.
fn csm_filtered(cascade: u32, uv: vec2<f32>, compare_z: f32, slope: vec2<f32>, bend: f32, radius: i32) -> f32 {
    let dims = vec2<f32>(cascade_dims(cascade));
    let inv_dims = 1.0 / dims;
    let uv_px = uv * dims;
    let q = uv_px - vec2<f32>(0.5);
    let b = floor(q);
    let f = q - b;
    let c = vec2<i32>(b);
    let d0 = b + vec2<f32>(0.5) - uv_px;
    var lit = 0.0;
    for (var y: i32 = -radius; y <= radius; y += 2) {
        let wy = vec2<f32>(select(1.0, 1.0 - f.y, y == -radius), select(1.0, f.y, y + 1 == radius + 1));
        for (var x: i32 = -radius; x <= radius; x += 2) {
            let wx = vec2<f32>(select(1.0, 1.0 - f.x, x == -radius), select(1.0, f.x, x + 1 == radius + 1));
            let o = vec2<i32>(x, y);
            let s = cascade_quad_lit(cascade, c + o, inv_dims, compare_z, slope, bend, d0 + vec2<f32>(o));
            lit += dot(s, vec4<f32>(wx.x * wy.x, wx.y * wy.x, wx.x * wy.y, wx.y * wy.y));
        }
    }
    let n = f32(2 * radius + 1);
    return lit / (n * n);
}

// `regia::shadows::shadow_pcss_sun` on cascade `cascade`'s map.
fn csm_pcss(
    cascade: u32,
    uv: vec2<f32>,
    compare_z: f32,
    penumbra_per_depth: f32,
    slope: vec2<f32>,
    bend: f32,
    rot: f32,
    search_n: f32,
    filter_n: f32,
) -> f32 {
    let dims_i = vec2<i32>(cascade_dims(cascade));
    let hi = dims_i - vec2<i32>(1);
    let dims = vec2<f32>(dims_i);
    let uv_px = uv * dims;
    let search = 7.0;
    let search_taps = min(select(12u, u32(search_n + 0.5), search_n > 0.5), 12u);
    let filter_taps = min(select(16u, u32(filter_n + 0.5), filter_n > 0.5), 16u);
    var blocker = 0.0;
    var count = 0.0;
    // Fixed trip counts with the tap limit as a predicate (no `break`/`continue`): FXC (DX12)
    // must unroll these loops and fails with X3511/X3570 when it cannot prove the exit.
    for (var i: u32 = 0u; i < 12u; i = i + 1u) {
        if (i < search_taps) {
            let o = regia::shadows::vogel_disc(i, search_taps, rot) * search;
            let c = clamp(vec2<i32>(floor(uv_px + o)), vec2<i32>(0), hi);
            let d = cascade_load(cascade, c);
            if (d < regia::shadows::plane_depth(compare_z, slope, 0.0, vec2<f32>(c) + vec2<f32>(0.5) - uv_px)) {
                blocker += d;
                count += 1.0;
            }
        }
    }
    if (count < 0.5) {
        return 1.0;
    }
    let avg = blocker / count;
    let radius = clamp((compare_z - avg) * penumbra_per_depth, regia::shadows::SUN_PCSS_MIN_RADIUS_TEXELS, 9.0);
    var lit = 0.0;
    for (var i: u32 = 0u; i < 16u; i = i + 1u) {
        if (i < filter_taps) {
            let o = regia::shadows::vogel_disc(i, filter_taps, rot + 1.3) * radius;
            lit += cascade_bilinear(cascade, 1.0 / dims, uv_px + o, uv_px, compare_z, slope, bend);
        }
    }
    if (u32(frame.quality.x + 0.5) == 4u) {
        // Shadow filter mode 4: the sun's angular size below the filter's footprint (ADR-3202,
        // `regia::shadows::sun_penumbra_gain`).
        let gain = regia::shadows::sun_penumbra_gain((compare_z - avg) * penumbra_per_depth, radius);
        return regia::shadows::sun_penumbra_steepen(lit / f32(filter_taps), gain);
    }
    return lit / f32(filter_taps);
}

// `normal` is the receiver's geometric normal (`csm_geometric_normal`).
fn sample_cascade(cascade: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, rot: f32) -> f32 {
    let tile_bias = cascade_bias(cascade);
    let texel_world = max(tile_bias.x, 1e-6);
    // Normalised depth per metre along the light (1 / the ortho depth extent).
    let depth_scale = max(tile_bias.y, 1e-9);
    let vp = cascade_view_proj(cascade);
    let no_l = clamp(dot(normal, l), 0.0, 1.0);
    let sin_theta = sqrt(max(1.0 - no_l * no_l, 0.0));
    let coarse = smoothstep(CSM_FINE_TEXEL_M, CSM_COARSE_TEXEL_M, texel_world);
    let offset_m = mix(
        min(CSM_NORMAL_OFFSET_TEXELS * texel_world, CSM_NORMAL_OFFSET_MAX_M),
        CSM_COARSE_NORMAL_OFFSET_TEXELS * texel_world,
        coarse,
    ) * sin_theta;
    let clip = vp * vec4<f32>(world + normal * offset_m, 1.0);
    let uvz = regia::shadows::shadow_uv_from_clip(clip);
    if (uvz.x < 0.0 || uvz.y < 0.0 || uvz.x > 1.0 || uvz.y > 1.0) {
        return 1.0;
    }
    let texel = 1.0 / f32(textureDimensions(shadow_map_0, 0).x);
    // Receiver-plane depth change per texel of offset, capped at CSM_MAX_TAN.
    var slope = regia::shadows::receiver_plane_slope(vp, normal) * texel;
    let max_slope = CSM_MAX_TAN * texel_world * depth_scale;
    let slope_len = length(slope);
    if (slope_len > max_slope) {
        slope = slope * (max_slope / slope_len);
    }
    let constant_m = mix(
        min(CSM_DEPTH_BIAS_TEXELS * texel_world, CSM_DEPTH_BIAS_MAX_M),
        CSM_COARSE_DEPTH_BIAS_TEXELS * texel_world,
        coarse,
    );
    let z = uvz.z - constant_m * depth_scale;
    let bend = (CSM_BEND_TEXELS * texel_world * depth_scale + CSM_BEND_SLOPE * length(slope))
        * min(texel_world / CSM_BEND_FULL_TEXEL_M, 1.0);
    let mode = u32(frame.quality.x + 0.5);
#ifdef SUN_SHADOW_PCF_ONLY
    // Adreno 643 (driver 512.530) could not compile the forward shader with the PCSS cascade
    // filter and the near tile beside the rest (ADR-1749): PCF only; the PCSS mode draws PCF 3×3.
    return csm_filtered(cascade, uvz.xy, z, slope, bend, select(select(1, 2, mode == 2u), 0, mode == 0u));
#else
    if (mode < 3u) {
        // Hardware 2×2 / PCF 3×3 / PCF 5×5 as bilinear taps over the receiver plane (3 and 4
        // are PCSS: 4 adds the angular sun penumbra, ADR-3202).
        return csm_filtered(cascade, uvz.xy, z, slope, bend, select(select(1, 2, mode == 2u), 0, mode == 0u));
    }
    // PCSS: penumbra texels per unit of normalised depth.
    let penumbra_per_depth = SUN_ANGULAR_RADIUS_TAN / max(depth_scale * texel_world, 1e-9);
    return csm_pcss(cascade, uvz.xy, z, penumbra_per_depth, slope, bend, rot, frame.pcss.x, frame.pcss.y);
#endif
}

// Cascade 0's sun visibility from blockers more than `lift_m` metres up the sun ray from the
// receiver only (3×3 bilinear taps over the receiver plane, lowered by `lift_m`). The near sun
// tile's receivers take what the tile does not hold (the terrain heightfield, ADR-164) from here:
// `lift_m` is the height of the tile's topmost mesh surface over the receiver plus a margin, so a
// mesh part the tile already shadows with is never counted twice at cascade 0's coarser texels.
fn sample_cascade_above(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, lift_m: f32) -> f32 {
    let tile_bias = cascade_bias(0u);
    let texel_world = max(tile_bias.x, 1e-6);
    let depth_scale = max(tile_bias.y, 1e-9);
    let vp = cascade_view_proj(0u);
    let no_l = clamp(dot(normal, l), 0.0, 1.0);
    let sin_theta = sqrt(max(1.0 - no_l * no_l, 0.0));
    let offset_m = min(CSM_NORMAL_OFFSET_TEXELS * texel_world, CSM_NORMAL_OFFSET_MAX_M) * sin_theta;
    let uvz = regia::shadows::shadow_uv_from_clip(vp * vec4<f32>(world + normal * offset_m, 1.0));
    if (uvz.x < 0.0 || uvz.y < 0.0 || uvz.x > 1.0 || uvz.y > 1.0) {
        return 1.0;
    }
    let texel = 1.0 / f32(textureDimensions(shadow_map_0, 0).x);
    var slope = regia::shadows::receiver_plane_slope(vp, normal) * texel;
    let max_slope = CSM_MAX_TAN * texel_world * depth_scale;
    let slope_len = length(slope);
    if (slope_len > max_slope) {
        slope = slope * (max_slope / slope_len);
    }
    let z = uvz.z - (min(CSM_DEPTH_BIAS_TEXELS * texel_world, CSM_DEPTH_BIAS_MAX_M) + lift_m) * depth_scale;
    let bend = (CSM_BEND_TEXELS * texel_world * depth_scale + CSM_BEND_SLOPE * length(slope))
        * min(texel_world / CSM_BEND_FULL_TEXEL_M, 1.0);
    return regia::shadows::shadow_pcf_plane(shadow_map_0, ao_samp, uvz.xy, z, slope, bend, 1);
}

// Margin (metres) over the near sun tile's topmost mesh surface below which cascade 0's blockers
// are left to the tile (`sample_cascade_above`): cascade 0's 3×3 taps reach ±1.5 of its texels,
// so a sloped hull beside the receiver can stand that much higher in them.
const NEAR_SUN_ABOVE_MARGIN_M: f32 = 0.25;
const NEAR_SUN_TILE_MARGIN_TEXELS: f32 = 12.0;

// The near sun tile (ADR-164): an orthographic sun view over the first few metres of the view,
// a square of the shadow atlas at ~5 mm texels (Desktop High), sampled with the cascades'
// receiver-plane filter. x = sun visibility, y = metres from the receiver up the sun ray to the
// tile's topmost surface there (0 when the receiver is the topmost); x < 0 when the receiver's
// filter could leave the tile (the cascades take it).
fn sample_near_sun(tile: ShadowTile, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, rot: f32) -> vec2<f32> {
    let texel_world = max(tile.bias.x, 1e-6);
    let depth_scale = max(tile.bias.y, 1e-9);
    let vp = tile.view_proj;
    let no_l = clamp(dot(normal, l), 0.0, 1.0);
    let sin_theta = sqrt(max(1.0 - no_l * no_l, 0.0));
    let offset_m = min(CSM_NORMAL_OFFSET_TEXELS * texel_world, CSM_NORMAL_OFFSET_MAX_M) * sin_theta;
    let local = regia::shadows::shadow_uv_from_clip(vp * vec4<f32>(world + normal * offset_m, 1.0));
    let tile_px = max(tile.uv.x * frame.extra.y, 1.0);
    let margin = NEAR_SUN_TILE_MARGIN_TEXELS / tile_px;
    if (any(local.xy < vec2<f32>(margin)) || any(local.xy > vec2<f32>(1.0 - margin))) {
        return vec2<f32>(-1.0, 0.0);
    }
    let uv = local.xy * tile.uv.xy + tile.uv.zw;
    var slope = regia::shadows::receiver_plane_slope(vp, normal) / tile_px;
    let max_slope = CSM_MAX_TAN * texel_world * depth_scale;
    let slope_len = length(slope);
    if (slope_len > max_slope) {
        slope = slope * (max_slope / slope_len);
    }
    let z = local.z - min(CSM_DEPTH_BIAS_TEXELS * texel_world, CSM_DEPTH_BIAS_MAX_M) * depth_scale;
    let bend = (CSM_BEND_TEXELS * texel_world * depth_scale + CSM_BEND_SLOPE * length(slope))
        * min(texel_world / CSM_BEND_FULL_TEXEL_M, 1.0);
    let atlas_px = vec2<i32>(uv * frame.extra.y);
    let top = textureLoad(shadow_atlas, atlas_px, 0);
    let above_m = max(local.z - top, 0.0) / depth_scale;
    let mode = u32(frame.quality.x + 0.5);
    var vis: f32;
    if (mode < 3u) {
        vis = regia::shadows::shadow_pcf_plane(shadow_atlas, ao_samp, uv, z, slope, bend, select(select(1, 2, mode == 2u), 0, mode == 0u));
    } else {
        let penumbra_per_depth = SUN_ANGULAR_RADIUS_TAN / max(depth_scale * texel_world, 1e-9);
        vis = regia::shadows::shadow_pcss_sun(shadow_atlas, ao_samp, uv, z, penumbra_per_depth, slope, bend, rot, frame.pcss.x, frame.pcss.y, mode == 4u);
    }
    return vec2<f32>(vis, above_m);
}

// Directional shadow with a blend band at the far end of each cascade (no visible
// resolution seam) and a fade-out over the last cascade's band (no hard cut-off
// where the shadow distance ends). `pix` is the fragment position (per-pixel kernel
// rotation). Within the near sun tile's range a mesh receiver takes the tile (ADR-164),
// blending into the cascades over the range's last fifth.
fn sample_csm(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, pix: vec2<f32>) -> f32 {
    let view_z = length(world - frame.camera_pos.xyz);
    let noise_pix = pix + 5.588238 * frame.quality.w;
#ifdef FRAME_BLUE_NOISE
    var rot = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715)))) * 6.2831853;
    if (frame_blue_noise_on()) {
        rot = frame_blue_noise_at(pix) * 6.2831853;
    }
#else
    let rot = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715)))) * 6.2831853;
#endif
    // The near tile's visibility and its weight in the blend into the cascades. The cascades
    // are sampled at one call site only: a driver inlines each call with its whole filter, and
    // a second call site (the near tile's blend) doubled the compile of every forward pipeline.
    var close = 1.0;
    var near_t = 1.0;
    var has_near = false;
    // `SUN_SHADOW_PCF_ONLY` (Adreno, ADR-1749): no near tile; the cascades alone.
#ifndef SUN_SHADOW_PCF_ONLY
    let near_index = u32(csm.count.w + 0.5);
    if (near_index > 0u) {
        let tile = shadow_tiles[near_index - 1u];
        let range = tile.bias.w;
        if (view_z < range) {
            let near = sample_near_sun(tile, world, normal, l, rot);
            if (near.x >= 0.0) {
                close = min(near.x, sample_cascade_above(world, normal, l, near.y + NEAR_SUN_ABOVE_MARGIN_M));
                near_t = smoothstep(0.8 * range, range, view_z);
                if (near_t <= 0.0) {
                    return close;
                }
                has_near = true;
            }
        }
    }
#endif
    let cascades = sample_csm_cascades(world, normal, l, rot, view_z);
    if (has_near) {
        return mix(close, cascades, near_t);
    }
    return cascades;
}

fn sample_csm_cascades(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, rot: f32, view_z: f32) -> f32 {
    let count = clamp(u32(csm.count.x), 1u, 4u);
    // Cascade i covers view depths (split[i-1], split[i]); split[count-1] = far.
    var cascade = 0u;
    for (var i = 1u; i < 4u; i = i + 1u) {
        if (i >= count) {
            break;
        }
        if (view_z > cascade_split(i - 1u)) {
            cascade = i;
        }
    }
    let hi = cascade_split(cascade);
    var lo = 0.0;
    if (cascade > 0u) {
        lo = cascade_split(cascade - 1u);
    }
    let band = max((hi - lo) * 0.15, 1e-3);
    let t = clamp((view_z - (hi - band)) / band, 0.0, 1.0);
    // Past the shadow distance the last cascade has faded to lit: `mix(s0, 1.0, 1.0)` is 1
    // whatever `s0` is, so skip its filter. The cascade's box reaches a radius past its split,
    // and every receiver there (meshes beyond 400 m with an infinite far plane, 4 km on a
    // vista) paid a full PCF or PCSS for nothing (ADR-1236).
    if (t >= 1.0 && cascade + 1u >= count) {
        return 1.0;
    }
    let s0 = sample_cascade(cascade, world, normal, l, rot);
    if (t <= 0.0) {
        return s0;
    }
    if (cascade + 1u < count) {
        return mix(s0, sample_cascade(cascade + 1u, world, normal, l, rot), t);
    }
    return mix(s0, 1.0, t);
}

// Depth bias of the translucency-map compare: the texel-scaled receiver bias the opaque sun
// cascades used before their per-texel receiver-plane test (ADR-140), kept for the coloured
// map (ADR-139), which takes one bilinear compare per receiver, not a filter of plane taps.
fn translucent_shadow_depth_bias(
    clip_w: f32,
    tile_bias: vec4<f32>,
    texel_world: f32,
    no_l: f32,
) -> f32 {
    // Normalized shadow depth per world unit along the light's view z.
    // Perspective: d(z01)/d(z) = 1 / (z^2 * (1/n - 1/f)), z = clip.w.
    // Orthographic: constant 1 / depth extent.
    let dz_dw = select(
        tile_bias.y,
        1.0 / max(clip_w * clip_w * tile_bias.y, 1.0e-9),
        tile_bias.z > 0.5,
    );
    let slope = 1.0 - clamp(no_l, 0.0, 1.0);
    // Half a texel plus a slope term: a receiver never shadows itself in this map (translucent
    // surfaces do not look it up), so the bias only has to absorb the compare's quantization.
    // The opaque map's 2-6 texels put a panel 1.1 cm under DO27's glazing (scavenger) in front
    // of its glass: fully sunlit where Cycles passes the glass's 10 %.
    return (0.5 + 1.5 * slope) * texel_world * dz_dw;
}

// Light transmitted to `world` through translucent casters in cascade `cascade` (RGB, 1 = no
// translucent caster between it and the sun). Normal offset and depth bias are the opaque
// map's: the translucency map is coarser, but scaling the bias by its texel pushed receivers a
// metre under glass in front of it on a far cascade, and translucent surfaces never look
// themselves up (no self-tint to guard against). The depth compare gives the bilinear
// fraction of the footprint behind the translucent surface; the transmittance is the bilinear
// product there (McGuire & Enderton 2011 § 2).
fn cascade_transmittance(cascade: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let tile_bias = cascade_bias(cascade);
    let texel_world = tile_bias.x;
    let no_l = max(dot(normal, l), 0.0);
    let offset_world = world + normal * texel_world * 0.25;
    let clip = cascade_view_proj(cascade) * vec4<f32>(offset_world, 1.0);
    let uvz = regia::shadows::shadow_uv_from_clip(clip);
    let rect = trans_bounds[min(cascade, 3u)];
    if (uvz.x < rect.x || uvz.y < rect.y || uvz.x > rect.z || uvz.y > rect.w) {
        return vec3<f32>(1.0);
    }
    let bias = translucent_shadow_depth_bias(clip.w, vec4<f32>(tile_bias.xy, 0.0, 0.0), texel_world, no_l);
    // LessEqual compare: 1 where the receiver is in front of the translucent depth.
    let behind = 1.0 - textureSampleCompareLevel(trans_csm_depth, shadow_samp, uvz.xy, i32(cascade), uvz.z - bias);
    if (behind <= 0.0) {
        return vec3<f32>(1.0);
    }
    let t = textureSampleLevel(trans_csm_color, trans_samp, uvz.xy, i32(cascade), 0.0).rgb;
    return mix(vec3<f32>(1.0), t, behind);
}

// Blended and transmissive surfaces are the translucent casters: they do not tint themselves
// (nor, by the same test, glass seen through other glass). A hair card is one too (it casts
// through the same maps), so it does not look itself up either: a card is its own nearest
// translucent depth, and the map's bias is not made for that.
fn translucent_surface() -> bool {
    return material.alpha.x > 1.5 || material.pom.z > 0.0 || hair_on();
}

// Sun transmittance through translucent casters, with the cascade choice and blend band of
// `sample_csm`; past the last translucent cascade it fades to 1 over that cascade's band.
fn sample_csm_transmittance(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let trans_count = min(u32(csm.count.y + 0.5), clamp(u32(csm.count.x), 1u, 4u));
    if (trans_count == 0u) {
        return vec3<f32>(1.0);
    }
    // Cheap world-space reject first: outside the cylinder the casters' bounding sphere sweeps
    // away from the sun (or nearer the sun than all of it), nothing translucent is toward it.
    let sphere = trans_bounds[4];
    let d = world - sphere.xyz;
    let toward = dot(d, l);
    if (toward > sphere.w || dot(d, d) - toward * toward > sphere.w * sphere.w) {
        return vec3<f32>(1.0);
    }
    let view_z = length(world - frame.camera_pos.xyz);
    var cascade = 0u;
    for (var i = 1u; i < 4u; i = i + 1u) {
        if (i >= trans_count) {
            break;
        }
        if (view_z > cascade_split(i - 1u)) {
            cascade = i;
        }
    }
    let hi = cascade_split(cascade);
    if (view_z > hi) {
        return vec3<f32>(1.0);
    }
    var lo = 0.0;
    if (cascade > 0u) {
        lo = cascade_split(cascade - 1u);
    }
    let band = max((hi - lo) * 0.15, 1e-3);
    let t = clamp((view_z - (hi - band)) / band, 0.0, 1.0);
    let t0 = cascade_transmittance(cascade, world, normal, l);
    if (t <= 0.0) {
        return t0;
    }
    if (cascade + 1u < trans_count) {
        return mix(t0, cascade_transmittance(cascade + 1u, world, normal, l), t);
    }
    return mix(t0, vec3<f32>(1.0), t);
}

// Glass enclosure (ADR-165): the share of the sky and ground light a receiver under glass gets,
// as the colour that multiplies its indirect light (1 = no glass over it). The translucency map
// tells, along the sun ray, whether a transmissive or blended surface lies over the receiver, its
// transmittance and how far above it is. A receiver a few decimetres under a pane (a cockpit under
// its canopy) sees most of its sky through that pane, so its ambient takes the pane's
// transmittance; the weight falls to 0 over `GLASS_ENCLOSURE_GAP_M` (the ground in a canopy's
// coloured shadow metres away still sees the open sky). Without it a cockpit under DO27's 2 %
// tinted canopy was lit by the full sky and ground (it reads milky; Cycles and the path tracer
// show it black). One texel of the cascade the receiver's sun lookup uses; no filter.
const GLASS_ENCLOSURE_GAP_M: vec2<f32> = vec2<f32>(0.25, 1.2);

fn glass_enclosure(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let trans_count = min(u32(csm.count.y + 0.5), clamp(u32(csm.count.x), 1u, 4u));
    if (trans_count == 0u || translucent_surface()) {
        return vec3<f32>(1.0);
    }
    let sphere = trans_bounds[4];
    let d = world - sphere.xyz;
    let toward = dot(d, l);
    if (toward > sphere.w || dot(d, d) - toward * toward > sphere.w * sphere.w) {
        return vec3<f32>(1.0);
    }
    let view_z = length(world - frame.camera_pos.xyz);
    var cascade = 0u;
    for (var i = 1u; i < 4u; i = i + 1u) {
        if (i >= trans_count) {
            break;
        }
        if (view_z > cascade_split(i - 1u)) {
            cascade = i;
        }
    }
    if (view_z > cascade_split(cascade)) {
        return vec3<f32>(1.0);
    }
    let tile_bias = cascade_bias(cascade);
    let clip = cascade_view_proj(cascade) * vec4<f32>(world + normal * tile_bias.x * 0.25, 1.0);
    let uvz = regia::shadows::shadow_uv_from_clip(clip);
    let rect = trans_bounds[min(cascade, 3u)];
    if (uvz.x < rect.x || uvz.y < rect.y || uvz.x > rect.z || uvz.y > rect.w) {
        return vec3<f32>(1.0);
    }
    let dims = vec2<f32>(textureDimensions(trans_csm_depth, 0));
    let texel = vec2<i32>(clamp(uvz.xy * dims, vec2<f32>(0.0), dims - vec2<f32>(1.0)));
    let glass_z = textureLoad(trans_csm_depth, texel, i32(cascade), 0);
    // Metres from the receiver up the sun ray to the nearest translucent surface.
    let gap = (uvz.z - glass_z) / max(tile_bias.y, 1e-9);
    if (gap <= 0.0) {
        return vec3<f32>(1.0);
    }
    let w = 1.0 - smoothstep(GLASS_ENCLOSURE_GAP_M.x, GLASS_ENCLOSURE_GAP_M.y, gap);
    if (w <= 0.0) {
        return vec3<f32>(1.0);
    }
    let t = textureSampleLevel(trans_csm_color, trans_samp, uvz.xy, i32(cascade), 0.0).rgb;
    return mix(vec3<f32>(1.0), t, w);
}

// Filled while local lights are shaded. The debug view reads them and returns
// before tonemap, so a shadow factor is not crushed by AgX.
var<private> dbg_shadow_sum: f32 = 0.0;
var<private> dbg_shadow_n: f32 = 0.0;
var<private> dbg_atlas: vec3<f32> = vec3<f32>(0.2, 0.2, 0.2);
// Sun diagnostic (`shadow_debug` 3): shadow-map visibility, contact shadow, n·l.
var<private> dbg_sun: vec3<f32> = vec3<f32>(1.0, 1.0, 0.0);

fn note_atlas_sample(vis: f32, tile_index: u32, local_uv: vec2<f32>) {
    dbg_shadow_sum += vis;
    dbg_shadow_n += 1.0;
    // `(tile * k) % n` is 0 for tile 0 on every channel, so the first shadowed
    // light (the cockpit's only spot) painted a black overlay. Shift by one
    // and keep a floor so the darkest swatch still reads against the grey clear.
    let i = tile_index + 1u;
    let k = vec3<f32>(
        0.30 + 0.70 * f32((i * 17u) % 7u) / 6.0,
        0.25 + 0.75 * f32((i * 31u) % 5u) / 4.0,
        0.20 + 0.80 * f32((i * 13u) % 6u) / 5.0,
    );
    let grid = fract(local_uv.x * 4.0) * fract(local_uv.y * 4.0);
    dbg_atlas = k * (0.55 + 0.45 * grid);
}

// Isidoro receiver-plane slope, clamped so a grazing wall does not push the
// compare depth across metres of empty space (ndl floor 0.16 → tan ≤ ~6).
fn atlas_tan_theta(no_l: f32) -> f32 {
    let ndl = max(clamp(no_l, 0.0, 1.0), 0.16);
    return min(sqrt(max(1.0 - ndl * ndl, 0.0)) / ndl, 8.0);
}

fn sample_atlas_tile(tile_index: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>, pix: vec2<f32>) -> f32 {
    let tile = shadow_tiles[tile_index];
    let no_l = max(dot(normal, l), 0.0);
    let clip = tile.view_proj * vec4<f32>(world, 1.0);
    // Perspective tiles: the texel's world size scales with view depth (clip.w).
    let texel_world = tile.bias.x * select(1.0, max(clip.w, 1.0e-6), tile.bias.z > 0.5);
    // Normal offset: 1.5 shadow texels along the surface normal.
    let offset_world = world + normal * texel_world * 1.5;
    let offset_clip = tile.view_proj * vec4<f32>(offset_world, 1.0);
    let local = regia::shadows::shadow_uv_from_clip(offset_clip);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0 || offset_clip.w <= 0.0) {
        return 1.0;
    }
    let uv = local.xy * tile.uv.xy + tile.uv.zw;
    let tile_min = tile.uv.zw;
    let tile_max = tile.uv.zw + tile.uv.xy;
    let texel = 1.0 / max(frame.extra.y, 1.0);
    let dz_dw = select(
        tile.bias.y,
        1.0 / max(offset_clip.w * offset_clip.w * tile.bias.y, 1.0e-9),
        tile.bias.z > 0.5,
    );
    // Head-on stays the old 2 texels, so a light above a floor does not
    // peter-pan. The Isidoro tan term is in the base compare: a kernel-only
    // slope left the centre tap on the shadow map and painted a whole
    // grazing face black.
    let tan_theta = atlas_tan_theta(no_l);
    let constant_bias = (2.0 + tan_theta) * texel_world * dz_dw;
    let compare_z = local.z - constant_bias;
    let mode = u32(frame.quality.x + 0.5);
    let noise_pix = pix + 5.588238 * frame.quality.w;
#ifdef FRAME_BLUE_NOISE
    var rot = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715)))) * 6.2831853;
    if (frame_blue_noise_on()) {
        rot = frame_blue_noise_at(pix) * 6.2831853;
    }
#else
    let rot = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715)))) * 6.2831853;
#endif
    var vis: f32;
    // 3 and 4 are PCSS (4 only adds the angular sun penumbra, ADR-3202).
    if (mode >= 3u) {
        let slope_per_texel = texel_world * dz_dw * tan_theta;
        let penumbra_per_depth = tile.bias.w * tile.bias.y / max(tile.bias.x, 1.0e-8);
        vis = regia::shadows::shadow_pcss_atlas(
            shadow_atlas, shadow_samp, uv, compare_z, texel,
            tile_min, tile_max, penumbra_per_depth, slope_per_texel, rot,
            frame.pcss.x, frame.pcss.y,
        );
    } else if (mode == 0u) {
        let half = vec2<f32>(texel * 0.5);
        let clamped = clamp(uv, tile_min + half, tile_max - half);
        // Level 0, no implicit derivative: the local-light loop runs after the alpha-test `discard`
        // (the atlas has one mip, so this is the same sample).
        vis = textureSampleCompareLevel(shadow_atlas, shadow_samp, clamped, compare_z);
    } else {
        var radius: i32 = 1;
        if (mode == 2u) {
            radius = 2;
        }
        vis = regia::shadows::shadow_pcf_rect(
            shadow_atlas, shadow_samp, uv, compare_z, texel,
            tile_min, tile_max, radius,
        );
    }
    note_atlas_sample(vis, tile_index, local.xy);
    return vis;
}

// How open a surface is to the sky, 1 under open sky, 0 under cover (spec 99 § 5.6): the point
// moved 1.5 texels along its geometric normal, then a 3×3 comparison against the occlusion tile
// with a receiver-plane slope bias and 2 cm, so roofs and slopes do not cover themselves. Outside
// the map, a point in the eye's interior volume counts as covered.
fn precip_exposure(world: vec3<f32>, n_geo: vec3<f32>) -> f32 {
    let index = i32(frame.weather2.y);
    if (index >= 0) {
        let tile = shadow_tiles[u32(index)];
        let offset = world + n_geo * tile.bias.x * 1.5;
        let local = regia::shadows::shadow_uv_from_clip(tile.view_proj * vec4<f32>(offset, 1.0));
        if (local.x >= 0.0 && local.y >= 0.0 && local.x <= 1.0 && local.y <= 1.0) {
            let tile_min = tile.uv.zw;
            let tile_max = tile.uv.zw + tile.uv.xy;
            let uv = local.xy * tile.uv.xy + tile.uv.zw;
            let texel = 1.0 / max(frame.extra.y, 1.0);
            let slope = regia::shadows::receiver_plane_slope(tile.view_proj, n_geo) * texel / tile.uv.x;
            // Past the view's far plane it compares as the far plane: open, unless covered.
            let compare_z = min(local.z - abs(slope.x) - abs(slope.y) - 0.02 * tile.bias.y, 1.0);
            return regia::shadows::shadow_pcf_rect(shadow_atlas, shadow_samp, uv, compare_z, texel, tile_min, tile_max, 1);
        }
    }
    if (frame.interior_center.w > 0.5) {
        let inside = abs(world - frame.interior_center.xyz) - frame.interior_half.xyz;
        if (max(inside.x, max(inside.y, inside.z)) <= 0.0) {
            return 0.0;
        }
    }
    return 1.0;
}

// The radiance the rain's or snow's extinction scatters in: the fog's color in the fog's mode.
fn weather_inscatter(view_dir: vec3<f32>) -> vec3<f32> {
#ifdef FORWARD_TRIM
    // Adreno (ADR-3145): the fog colour without the aerial haze.
    return frame.fog_color.rgb / max(frame.post_exposure.y, 1e-8);
#else
    if (frame.fog_color.a > 0.5) {
        let tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
        return regia::aerial::haze_inscatter(
            view_dir,
            -normalize(dir_light.direction),
            dir_light.color * dir_light.illuminance,
            frame.post_exposure.z,
        ) * tint;
    }
    return frame.fog_color.rgb / max(frame.post_exposure.y, 1e-8);
#endif
}

// `weather_inscatter` toward a surface at `world_position`: on a frame with the cloud
// transmittance map the rain or snow between the eye and the surface is under the layer, and
// scatters what the air there has (ADR-5944), not a clear day's radiance. In the fog's sky
// in-scatter mode that is `haze_inscatter_under_clouds`. The flat colour is divided by the
// scene's exposure setting, so under an exposure that adapts it is a fixed radiance, a clear
// day's: it keeps the haze's floor of it under the layer. Without the map, `weather_inscatter`
// as it was.
fn weather_inscatter_at(view_dir: vec3<f32>, world_position: vec3<f32>) -> vec3<f32> {
#ifdef FORWARD_TRIM
    return weather_inscatter(view_dir);
#else
    if (rain_under_clouds()) {
        let sun_lit = haze_sun_lit(world_position);
        if (frame.fog_color.a > 0.5) {
            let tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
            return regia::aerial::haze_inscatter_under_clouds(
                view_dir,
                -normalize(dir_light.direction),
                dir_light.color * dir_light.illuminance,
                frame.post_exposure.z,
                sun_lit,
            ) * tint;
        }
        return weather_inscatter(view_dir) * mix(regia::aerial::HAZE_SKY_FLOOR, 1.0, clamp(sun_lit, 0.0, 1.0));
    }
    return weather_inscatter(view_dir);
#endif
}

// This material's weather response from the bits above the two flags of `alpha.w`
// (`WeatherResponse::packed_bits`): porosity, wet, snow, puddles.
fn weather_response(roughness: f32, metallic: f32) -> vec4<f32> {
    let bits = u32(material.alpha.w + 0.5);
    let wet = 1.0 - f32((bits >> 2u) & 15u) / 15.0;
    let snow = 1.0 - f32((bits >> 6u) & 15u) / 15.0;
    let puddles = f32((bits >> 10u) & 1u);
    let stored = (bits >> 11u) & 255u;
    var porosity = regia::forward::surface_weather::weather_porosity(roughness, metallic);
    if (stored > 0u) {
        porosity = f32(stored - 1u) / 254.0;
    }
    return vec4<f32>(porosity, wet, snow, puddles);
}

// Local-light transmittance through translucent casters for atlas tile `tile_index` (the
// translucency atlas shares the tile UVs at a quarter of the edge; see `cascade_transmittance`).
fn sample_atlas_transmittance(tile_index: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let tile = shadow_tiles[tile_index];
    let no_l = max(dot(normal, l), 0.0);
    let clip = tile.view_proj * vec4<f32>(world, 1.0);
    let edge = f32(max(textureDimensions(trans_atlas_depth, 0).x, 1u));
    // The opaque tile's bias (see `cascade_transmittance`).
    let texel_world = tile.bias.x * select(1.0, max(clip.w, 1.0e-6), tile.bias.z > 0.5);
    let offset_world = world + normal * texel_world * 1.5;
    let offset_clip = tile.view_proj * vec4<f32>(offset_world, 1.0);
    let local = regia::shadows::shadow_uv_from_clip(offset_clip);
    if (local.x < 0.0 || local.y < 0.0 || local.x > 1.0 || local.y > 1.0 || offset_clip.w <= 0.0) {
        return vec3<f32>(1.0);
    }
    let half = vec2<f32>(0.5 / edge);
    let uv = clamp(local.xy * tile.uv.xy + tile.uv.zw, tile.uv.zw + half, tile.uv.zw + tile.uv.xy - half);
    let dz_dw = select(
        tile.bias.y,
        1.0 / max(offset_clip.w * offset_clip.w * tile.bias.y, 1.0e-9),
        tile.bias.z > 0.5,
    );
    let compare_z = local.z - (2.0 + atlas_tan_theta(no_l)) * texel_world * dz_dw;
    let behind = 1.0 - textureSampleCompareLevel(trans_atlas_depth, shadow_samp, uv, compare_z);
    if (behind <= 0.0) {
        return vec3<f32>(1.0);
    }
    let t = textureSampleLevel(trans_atlas_color, trans_samp, uv, 0.0).rgb;
    return mix(vec3<f32>(1.0), t, behind);
}

#ifdef GPU_DRIVEN
@group(1) @binding(0) var<storage, read> objects: array<ObjectData>;
#ifdef GPU_INSTANCED
// Instanced GPU-driven draws: `instance_index` runs over this batch's visible slots (the cull
// packed them at first_instance = batch base), and each slot names an object. The order-fixed
// cull (`gpu_cull.wgsl` `params.ordered`) leaves culled instances in place as 0xffffffff: a zero
// object, whose zero model puts every vertex at one point, so the instance draws nothing.
@group(1) @binding(2) var<storage, read> instance_slots: array<u32>;
fn current_object(instance: u32) -> ObjectData {
    let slot = instance_slots[instance];
    if (slot == 0xffffffffu) {
        return ObjectData();
    }
    return objects[slot];
}
#else
fn current_object(slot: u32) -> ObjectData {
    return objects[slot];
}
#endif
#else
@group(1) @binding(0) var<uniform> object: ObjectData;
fn current_object(slot: u32) -> ObjectData {
    let _unused = slot;
    return object;
}
#endif
@group(1) @binding(1) var<storage, read> skin_joints: array<regia::mesh::Mat3x4>;

struct SkinnedVertex {
    pos: vec3<f32>,
    nrm: vec3<f32>,
    tan: vec3<f32>,
}

fn skin_vertex(pos: vec3<f32>, nrm: vec3<f32>, tan: vec3<f32>, joints: vec4<u32>, weights: vec4<f32>, skin_base: u32) -> SkinnedVertex {
    var p = pos;
    var n = nrm;
    var t = tan;
    let wsum = weights.x + weights.y + weights.z + weights.w;
    if (wsum >= 1e-5) {
        let base = skin_base;
        let j0 = skin_joints[base + joints.x];
        let j1 = skin_joints[base + joints.y];
        let j2 = skin_joints[base + joints.z];
        let j3 = skin_joints[base + joints.w];
        p = regia::mesh::skin_position(pos, j0, j1, j2, j3, weights);
        n = regia::mesh::skin_direction(nrm, j0, j1, j2, j3, weights);
        t = regia::mesh::skin_direction(tan, j0, j1, j2, j3, weights);
    }
    return SkinnedVertex(p, n, t);
}

// Material constants (`regia_render::bindless`, ADR-316). `MATERIAL_SSBO`: this material's
// record in the one storage buffer that holds every material (the bind group binds its slot);
// otherwise the material's own uniform buffer. Both declare the same struct over the same bytes.
#ifdef VIS_SINGLE_PASS
// The single-pass visibility resolve (`visbuffer/single_pass.rs`, ADR-2071): the whole arena and
// the frame's material slot -> arena record map. `material` below is rewritten to this pixel's
// record when the module is composed (`single_pass_source`): a `var<private>` copy of the struct
// is what ADR-026 forbids.
struct VisMaterialRecord {
    @size(#{VIS_RECORD_STRIDE}) record: LabMaterial,
}
@group(2) @binding(0) var<storage, read> vis_material_records: array<VisMaterialRecord>;
@group(2) @binding(1) var<storage, read> vis_slot_records: array<u32>;
var<private> vis_material_record: u32 = 0u;
#else
#ifdef MATERIAL_SSBO
@group(2) @binding(0) var<storage, read> material: LabMaterial;
#else
@group(2) @binding(0) var<uniform> material: LabMaterial;
#endif
#endif
// The draw's material constants, whichever binding holds them. Step 2 (per-draw slots) turns
// this into `material_constants(slot)` over the whole arena.
fn material_constants() -> LabMaterial {
    return material;
}
#ifdef MATERIAL_GRAPH
// Slot 0, or the frame clock when bit 7 of `frame.interior_half.w` is set (ADR-5160).
fn graph_clock() -> vec4<f32> {
    if ((u32(frame.interior_half.w + 0.5) & 128u) != 0u) {
        return frame.clock;
    }
    return material.graph[0];
}
#endif
#ifdef TEXTURE_TABLE
// Every material texture of the device in one fully bound table (group 4, ADR-863); the
// material record names its slots' indices. Same sampler as the per-material path.
@group(4) @binding(0) var texture_table: binding_array<texture_2d<f32>, #{TEXTURE_TABLE_SIZE}>;
@group(4) @binding(1) var tex_samp: sampler;
// The table index of texture `slot` (0 base colour, 1 normal, 2 ORM, 3 emissive, 4 detail,
// 5 detail normal, 6 aux) from the draw's material record.
fn table_index(slot: u32) -> u32 {
    let word = material.texture_indices[slot >> 1u];
    return (word >> ((slot & 1u) * 16u)) & 0xffffu;
}
// The shared sampler set (ADR-2071). The record's top 16 bits hold 2 bits per glTF slot 0-3: 0
// `tex_samp`, 1 clamp, 2 point clamp. Detail and aux slots read 0.
@group(4) @binding(3) var tex_samp_clamp: sampler;
@group(4) @binding(4) var tex_samp_point: sampler;
fn table_sampler_choice(slot: u32) -> u32 {
    return (material.texture_indices.w >> (16u + slot * 2u)) & 3u;
}
// `textureSampleLevel` at LOD 0 on `slot` through its chosen sampler.
fn table_sample_level(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let choice = table_sampler_choice(slot);
    if (choice == 1u) {
        return textureSampleLevel(texture_table[table_index(slot)], tex_samp_clamp, uv, 0.0);
    }
    if (choice == 2u) {
        return textureSampleLevel(texture_table[table_index(slot)], tex_samp_point, uv, 0.0);
    }
    return textureSampleLevel(texture_table[table_index(slot)], tex_samp, uv, 0.0);
}
// `table_sample` and `table_sample_bias` take implicit derivatives: draw paths only.
fn table_sample(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let choice = table_sampler_choice(slot);
    if (choice == 1u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_clamp, uv);
    }
    if (choice == 2u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_point, uv);
    }
    return textureSample(texture_table[table_index(slot)], tex_samp, uv);
}
fn table_sample_bias(slot: u32, uv: vec2<f32>, bias: f32) -> vec4<f32> {
    let choice = table_sampler_choice(slot);
    if (choice == 1u) {
        return textureSampleBias(texture_table[table_index(slot)], tex_samp_clamp, uv, bias);
    }
    if (choice == 2u) {
        return textureSampleBias(texture_table[table_index(slot)], tex_samp_point, uv, bias);
    }
    return textureSampleBias(texture_table[table_index(slot)], tex_samp, uv, bias);
}
#ifdef RESIDENCY_FEEDBACK
// Residency feedback (`regia_render::residency_feedback`, ADR-1110): one word per table index.
// Word 0 holds the frame's phase (written by the renderer); indices 0-3 are the neutral views
// and are never written. In each 4x4 pixel cell the pixel the phase names stores, for each of
// its material's streamed slots (base colour, normal, ORM, emissive), the largest texel need
// seen this frame: 1 + 256 * log2(texels along the texture's edge for one texel per pixel).
// Stores only: the pixel's colour does not depend on them.
@group(4) @binding(2) var<storage, read_write> table_feedback: array<atomic<u32>>;
fn note_residency_need(pix: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) {
    let p = vec2<u32>(max(pix, vec2<f32>(0.0)));
    let phase = atomicLoad(&table_feedback[0]) & 15u;
    if (((p.x & 3u) | ((p.y & 3u) << 2u)) != phase) {
        return;
    }
    let uv_per_px = max(max(length(dx), length(dy)), 1e-8);
    let code = 1u + u32(clamp(-log2(uv_per_px), 0.0, 15.996) * 256.0);
    for (var slot = 0u; slot < 4u; slot += 1u) {
        let index = table_index(slot);
        if (index >= 4u) {
            atomicMax(&table_feedback[index], code);
        }
    }
}
// The surface UV's screen gradients at the pixel (the same ones `sample_material_slot` starts
// from, with the frame's mip bias), then the gated stores. Called once per shaded pixel, in
// uniform control flow, before any branch on the material.
fn note_residency(pix: vec2<f32>, uv: vec2<f32>) {
    let k = exp2(frame.fill_color.w);
#ifdef VISBUFFER
    note_residency_need(pix, vis_uv_ddx * k, vis_uv_ddy * k);
#else
    note_residency_need(pix, dpdx(uv) * k, dpdy(uv) * k);
#endif
}
#endif
#else
@group(2) @binding(1) var albedo_tex: texture_2d<f32>;
@group(2) @binding(2) var tex_samp: sampler;
@group(2) @binding(3) var normal_tex: texture_2d<f32>;
@group(2) @binding(4) var occlusion_tex: texture_2d<f32>;
@group(2) @binding(5) var emissive_tex: texture_2d<f32>;
// Detail albedo (RGB, 0.5 is neutral) and roughness (A).
@group(2) @binding(6) var detail_tex: texture_2d<f32>;
// Detail normal (RGB) and mask (A).
@group(2) @binding(7) var detail_normal: texture_2d<f32>;
// Height (R, 1 = geometric surface) and curvature (G, 0.5 = flat).
@group(2) @binding(8) var aux_tex: texture_2d<f32>;
#endif

// Tangent-space normal in -1..1 from a normal-map sample, by the slot's channel layout
// (`material.alpha.z` bits 1-2, `NormalLayout`): 0 = XYZ in RGB; 1 = XY in RG (BC5, EAC RG11);
// 2 = X in RGB, Y in A (ASTC normal mode). The two-channel layouts rebuild Z from the unit length.
fn decode_normal_sample(s: vec4<f32>) -> vec3<f32> {
    let channels = (u32(material.alpha.z + 0.5) >> 1u) & 3u;
    if (channels == 0u) {
        return s.xyz * 2.0 - vec3<f32>(1.0);
    }
    var xy = s.xy * 2.0 - vec2<f32>(1.0);
    if (channels == 2u) {
        xy = vec2<f32>(s.x, s.w) * 2.0 - vec2<f32>(1.0);
    }
    return vec3<f32>(xy, sqrt(max(1.0 - dot(xy, xy), 0.0)));
}

// Streamed material maps (`regia_render::texture_stream`): while a newly resident set fades
// in, `material.detail.w` is the lowest LOD sampled. The UV gradients are stretched so the
// sampler's own LOD (anisotropy kept) never goes below it. At 0 this is the plain
// `textureSample`, so a resident material draws exactly as an unstreamed one.
//
// The visibility-buffer resolve is a full-screen pass: implicit derivatives would cross
// triangle and object edges, so it starts from the triangle's own UV gradients.
//
// `frame.fill_color.w` is the frame's texture mip bias: negative while TSR upscales (the
// gradients shrink by `2^bias`, so the sampled mip is the one the output resolution would pick),
// plus the device's anisotropic-blend offset (ADR-5670: ≥ 0, 0 where the anisotropic filter is
// never sharper than exact trilinear). At 0 every path samples exactly as before.
fn sample_material(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    let min_lod = material.detail.w;
    let bias = frame.fill_color.w;
#ifdef VISBUFFER
    if (min_lod <= 0.0) {
        let k = exp2(bias);
        return textureSampleGrad(t, tex_samp, uv, vis_uv_ddx * k, vis_uv_ddy * k);
    }
#endif
    if (min_lod > 0.0) {
        let size = vec2<f32>(textureDimensions(t, 0));
        let k = exp2(bias);
#ifdef VISBUFFER
        let dx = vis_uv_ddx * k;
        let dy = vis_uv_ddy * k;
#else
        let dx = dpdx(uv) * k;
        let dy = dpdy(uv) * k;
#endif
        let lod = log2(max(max(length(dx * size), length(dy * size)), 1e-8));
        let stretch = exp2(max(min_lod - lod, 0.0));
        return textureSampleGrad(t, tex_samp, uv, dx * stretch, dy * stretch);
    }
    return textureSampleBias(t, tex_samp, uv, bias);
}
#ifdef TEXTURE_TABLE
// `sample_material` on texture `slot` of the table (the same sampling, operation for operation),
// through the slot's sampler from the shared set.
fn sample_material_slot(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let choice = table_sampler_choice(slot);
    if (choice == 1u) {
        return sample_slot_with(slot, tex_samp_clamp, uv);
    }
    if (choice == 2u) {
        return sample_slot_with(slot, tex_samp_point, uv);
    }
    return sample_slot_with(slot, tex_samp, uv);
}
fn sample_slot_with(slot: u32, samp: sampler, uv: vec2<f32>) -> vec4<f32> {
    let min_lod = material.detail.w;
    let bias = frame.fill_color.w;
#ifdef VISBUFFER
    if (min_lod <= 0.0) {
        let k = exp2(bias);
        return textureSampleGrad(texture_table[table_index(slot)], samp, uv, vis_uv_ddx * k, vis_uv_ddy * k);
    }
#endif
#ifdef VIS_SINGLE_PASS
    // One full-screen draw: only gradients the resolve computed itself (ADR-356 fact 4).
    let size = vec2<f32>(textureDimensions(texture_table[table_index(slot)], 0));
    let k = exp2(bias);
    let dx = vis_uv_ddx * k;
    let dy = vis_uv_ddy * k;
    let lod = log2(max(max(length(dx * size), length(dy * size)), 1e-8));
    let stretch = exp2(max(min_lod - lod, 0.0));
    return textureSampleGrad(texture_table[table_index(slot)], samp, uv, dx * stretch, dy * stretch);
#else
    if (min_lod > 0.0) {
        let size = vec2<f32>(textureDimensions(texture_table[table_index(slot)], 0));
        let k = exp2(bias);
#ifdef VISBUFFER
        let dx = vis_uv_ddx * k;
        let dy = vis_uv_ddy * k;
#else
        let dx = dpdx(uv) * k;
        let dy = dpdy(uv) * k;
#endif
        let lod = log2(max(max(length(dx * size), length(dy * size)), 1e-8));
        let stretch = exp2(max(min_lod - lod, 0.0));
        return textureSampleGrad(texture_table[table_index(slot)], samp, uv, dx * stretch, dy * stretch);
    }
    return textureSampleBias(texture_table[table_index(slot)], samp, uv, bias);
#endif
}
#endif

#ifdef MASK_DECALS
// ---- Material mask recolor and UV decal slots (`regia_render::mask_decals`) ----
// One instance's block (`mask_decals::GpuMaskBlock`). Group 3 bindings 9-11 leave the refraction
// (0-2, 7, 8) and glass (3-6) bindings of this file free; the variant is never built with either.
struct MaskDecal {
    // xy = center in the base UV set, zw = half extents.
    rect: vec4<f32>,
    // x = cos, y = sin of the rotation; z = atlas cell (< 0: solid); w = blend (0 alpha,
    // 1 multiply, 2 emissive).
    params: vec4<f32>,
    // rgb = linear color (emissive: times its intensity), a = opacity.
    color: vec4<f32>,
}

struct MaskDecalBlock {
    primary: vec4<f32>,
    secondary: vec4<f32>,
    trim: vec4<f32>,
    // rgb = emissive color times intensity.
    emissive: vec4<f32>,
    // x = decal count, y = atlas columns, z = atlas rows, w = flags (bit 0 mask, bit 1 atlas).
    header: vec4<u32>,
    decals: array<MaskDecal, 8>,
}

@group(3) @binding(9) var<uniform> mask_block: MaskDecalBlock;
// Linear: R primary, G secondary, B trim, A emissive.
@group(3) @binding(10) var mask_tex: texture_2d<f32>;
// sRGB color, coverage in A, `header.y` x `header.z` cells.
@group(3) @binding(11) var decal_atlas: texture_2d<f32>;

struct MaskDecalOut {
    base: vec3<f32>,
    glow: vec3<f32>,
    // Coverage of paint-over decals: the metallic goes to 0 under it.
    paint: f32,
}

// Recolor by the mask, then the decal slots in order (mirrors `mask_decals::recolor` and
// `DecalSlot::local_uv`). Called in uniform control flow: the mask flag and the slot count come
// from a uniform, and the atlas taps take explicit gradients.
fn apply_mask_decals(base_in: vec3<f32>, uv: vec2<f32>) -> MaskDecalOut {
    var base = base_in;
    var glow = vec3<f32>(0.0);
    var paint = 0.0;
    let flags = mask_block.header.w;
    if (flags & 1u) != 0u {
        let m = sample_material(mask_tex, uv);
        let sum = m.r + m.g + m.b;
        let w = m.rgb / max(sum, 1.0);
        let tint = vec3<f32>(1.0 - min(sum, 1.0))
            + mask_block.primary.rgb * w.r
            + mask_block.secondary.rgb * w.g
            + mask_block.trim.rgb * w.b;
        base = base * tint;
        glow = mask_block.emissive.rgb * m.a;
    }
    let count = min(mask_block.header.x, 8u);
    let columns = max(mask_block.header.y, 1u);
    let grid = vec2<f32>(f32(columns), f32(max(mask_block.header.z, 1u)));
    let atlas_texel = grid / vec2<f32>(textureDimensions(decal_atlas, 0));
    let duv_dx = dpdx(uv);
    let duv_dy = dpdy(uv);
    for (var i = 0u; i < count; i = i + 1u) {
        let d = mask_block.decals[i];
        let size = max(d.rect.zw * 2.0, vec2<f32>(1e-6));
        let local = decal_unrotate(uv - d.rect.xy, d.params.x, d.params.y) / size + vec2<f32>(0.5);
        if any(local < vec2<f32>(0.0)) || any(local > vec2<f32>(1.0)) {
            continue;
        }
        var texel = vec4<f32>(1.0);
        if d.params.z >= 0.0 && (flags & 2u) != 0u {
            let cell = u32(d.params.z + 0.5);
            let cell_xy = vec2<f32>(f32(cell % columns), f32(cell / columns));
            // Half a level-0 texel inside the cell: bilinear taps never read the neighbor.
            let inset = clamp(local, atlas_texel * 0.5, vec2<f32>(1.0) - atlas_texel * 0.5);
            let to_atlas = 1.0 / (size * grid);
            texel = textureSampleGrad(
                decal_atlas,
                tex_samp,
                (cell_xy + inset) / grid,
                decal_unrotate(duv_dx, d.params.x, d.params.y) * to_atlas,
                decal_unrotate(duv_dy, d.params.x, d.params.y) * to_atlas,
            );
        }
        let color = texel.rgb * d.color.rgb;
        let a = regia::utils::saturate(texel.a * d.color.a);
        let mode = u32(d.params.w + 0.5);
        if mode == 0u {
            base = mix(base, color, a);
            paint = max(paint, a);
        } else if mode == 1u {
            base = base * mix(vec3<f32>(1.0), color, a);
        } else {
            glow = glow + color * a;
        }
    }
    return MaskDecalOut(base, glow, paint);
}
#endif

#ifdef LIGHTMAP
// Baked lightmap of a receiver (ADR-1623, `frame/lightmap_forward.rs`): the four bindings of
// `LightmapGpu` and the draw's entry block (`scale_offset`, at a dynamic offset) in group 3,
// clear of the refraction (0-2, 7, 8), glass (3-6) and mask-decal (9-11) bindings. An instanced
// draw reads `scale_offset` from a table per object slot instead (ADR-2107). The lookup functions
// of `lightmap.wgsl` are spliced in at the marker below
// (`regia_render::lightmap::lightmap_forward_source`).
@group(3) @binding(12) var lightmap_atlas: texture_2d<f32>;
@group(3) @binding(13) var lightmap_sampler: sampler;
@group(3) @binding(14) var<uniform> lightmap_params: LightmapParams;
@group(3) @binding(15) var lightmap_direction: texture_2d<f32>;
#ifdef GPU_INSTANCED
// Per object slot: the entry's UV2 → atlas transform, zero for an instance that is not a receiver
// (a non-receiver may share a receiver's batch and keeps the probe light).
@group(3) @binding(17) var<storage, read> lightmap_slots: array<vec4<f32>>;
fn lightmap_instance_scale_offset(instance: u32) -> vec4<f32> {
    let slot = instance_slots[instance];
    if (slot == 0xffffffffu) {
        return vec4<f32>(0.0);
    }
    return lightmap_slots[slot];
}
#else
struct LightmapDraw {
    // The entry's UV2 → atlas transform (`LightmapEntry::scale_offset`).
    scale_offset: vec4<f32>,
}
@group(3) @binding(16) var<uniform> lightmap_draw: LightmapDraw;
#endif
// @lightmap_lookup_source
#endif

#ifdef THIN_FILM
// Thin film over the base specular (`regia_render::thin_film`, ADR-1977): group 3 of a filmed
// material. x = coverage, y = film IOR, z = thickness at map 0, w = thickness at map 1 (nm).
struct ThinFilmBlock {
    params: vec4<f32>,
}
@group(3) @binding(17) var<uniform> film_block: ThinFilmBlock;
// Coverage (R) and thickness (G), packed by `regia_render::thin_film::pack_thin_film_map`. White
// without a map: full coverage at the maximum thickness.
@group(3) @binding(18) var film_map: texture_2d<f32>;

// What a filmed transmissive surface reflects at `no_v`, per channel, on a dielectric of `f0`: its
// `1 - F` is the light that crosses the film (`thin_film::transmitted`, `pt.wgsl`'s `transmit`).
fn transmit_film_fresnel(uv: vec2<f32>, no_v: f32, f0: f32) -> vec3<f32> {
    let film_sample = sample_material(film_map, uv);
    let film_nm = mix(film_block.params.z, film_block.params.w, film_sample.g);
    let film_cover = regia::pbr::thin_film::weight(film_block.params.x * film_sample.r, film_nm);
    return regia::pbr::thin_film::fresnel(no_v, vec3<f32>(f0), film_cover, film_block.params.y, film_nm);
}
#endif

#ifdef GPU_DRIVEN
@vertex
fn vs_main(in: VertexInput, @builtin(instance_index) instance: u32) -> VertexOutput {
    let object = current_object(instance);
#else
@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    let object = current_object(0u);
#endif
    let skinned = skin_vertex(in.position, in.normal, in.tangent.xyz, in.joints, in.weights, object.skin.x);
#ifdef MATERIAL_GRAPH
#ifdef MATERIAL_GRAPH_UV1
    let uv1 = in.uv1;
#else
    let uv1 = in.uv;
#endif
#endif
#ifdef MATERIAL_GRAPH_VERTEX
    let rest = (object.model * vec4<f32>(skinned.pos, 1.0)).xyz;
    let nrm = (object.model * vec4<f32>(skinned.nrm, 0.0)).xyz;
    let world = rest + material_vertex_offset(regia::pbr::surface::VertexOffsetInput(
        rest,
        nrm * inverseSqrt(max(dot(nrm, nrm), 1e-12)),
        in.uv,
        uv1,
        in.color,
        object.model[3].xyz,
        graph_clock().x,
    ));
#else
    let world = (object.model * vec4<f32>(skinned.pos, 1.0)).xyz;
    let nrm = (object.model * vec4<f32>(skinned.nrm, 0.0)).xyz;
#endif
    let tan = (object.model * vec4<f32>(skinned.tan, 0.0)).xyz;
    return VertexOutput(
        frame.view_proj * vec4<f32>(world, 1.0),
        world,
        nrm,
        in.uv,
        vec4<f32>(tan, in.tangent.w),
        in.color,
        bitcast<f32>(object.skin.y),
        object.skin.zw,
#ifdef MATERIAL_GRAPH
        uv1,
        object.model[3].xyz,
#endif
#ifdef LIGHTMAP
        in.uv2,
#ifdef GPU_INSTANCED
        lightmap_instance_scale_offset(instance),
#else
        lightmap_draw.scale_offset,
#endif
#endif
#ifdef FRAME_FORWARD_DECALS
        select(1u, 2u, in.weights.x + in.weights.y + in.weights.z + in.weights.w >= 1e-5),
#endif
    );
}

#ifdef MATERIAL_GRAPH_VERTEX
// The material graph's vertex stage (world position offset); spliced in like the surface.
// @material_graph_vertex_source
#endif

// LOD cross-fade (motion-look2): `code` c > 0 keeps the pixels whose dither threshold is below
// c (the incoming LOD), c < 0 keeps the others (the outgoing LOD), so the two copies of an
// instance cover each pixel exactly once. The threshold is interleaved gradient noise (Jimenez,
// "Next Generation Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014) advanced
// per frame by the golden ratio (an R1 sequence): no repeating cell, so a half-faded crown shows
// no regular dot lattice, and every pixel walks evenly spread thresholds so TAA averages the
// stipple into a blend. The phase follows the hashed alpha test (ADR-1489, ADR-3146): IGN on a held
// view, the scrambled hierarchy of `alpha_hash_phase` on a moving one, whose short history would
// leave IGN's residual on its lattice. `imp_dither_discard` in `regia_terrain`'s `impostor.wgsl` draws
// the same threshold, so a mesh and its impostor stay exact complements.
fn lod_dither_discard(code: f32, frag: vec2<f32>) -> bool {
    if (code == 0.0) {
        return false;
    }
    let t = f32(u32(max(frame.quality.w, 0.0) + 0.5) & 1023u);
    var phase = fract(52.9829189 * fract(dot(frag, vec2<f32>(0.06711056, 0.00583715))));
    if ((u32(frame.interior_half.w + 0.5) & 32u) == 0u) {
        phase = alpha_hash_phase(vec2<u32>(max(frag, vec2<f32>(0.0))));
    }
    let d = fract(phase + 0.618034 * t);
    if (code > 0.0) {
        return d >= code;
    }
    return d < -code;
}

// Tatarchuk, Practical Dynamic Parallax Occlusion Mapping (SIGGRAPH 2005): linear search + secant.
// Height 1 is the geometric surface, so a flat map hits at step 0 and returns `uv`.
fn parallax_uv(uv: vec2<f32>, view_ts: vec3<f32>, scale: f32) -> vec2<f32> {
    let vz = max(abs(view_ts.z), 0.15);
    let facing = regia::utils::saturate(abs(view_ts.z));
    let max_steps = clamp(material.response.w, 1.0, 16.0);
    let min_steps = clamp(material.pom.x, 1.0, max_steps);
    let steps_f = clamp(mix(max_steps, min_steps, facing), 1.0, 16.0);
    let steps = u32(steps_f + 0.5);
    // Divide by the rounded step count, like the CPU reference in
    // hard_surface.rs: the unrounded count leaves the march at steps/steps_f
    // of the depth and a flat deep height overshoots or stops short.
    let count = f32(steps);
    let layer = 1.0 / count;
    let delta = view_ts.xy / vz * scale / count;
    var current_uv = uv;
    var current_depth = 0.0;
    var prev_uv = uv;
    var prev_depth = 0.0;
    var hit = 0.0;
    for (var i: u32 = 0u; i < 16u; i = i + 1u) {
        // Height 1 is the surface. Hit once the ray depth reaches it (a flat
        // map hits at step 0; a deep groove marches first). Predicated, not `continue`, so
        // FXC can unroll the fixed 16 iterations; the fetch stays inside the guard so
        // finished or unused iterations cost no texture read.
        if (i < steps && hit < 0.5) {
            #ifdef TEXTURE_TABLE
            let h = textureSampleLevel(texture_table[table_index(6u)], tex_samp, current_uv, 0.0).r;
            #else
            let h = textureSampleLevel(aux_tex, tex_samp, current_uv, 0.0).r;
            #endif
            if (h >= 1.0 - current_depth) {
                hit = 1.0;
            } else {
                prev_uv = current_uv;
                prev_depth = current_depth;
                current_uv = current_uv - delta;
                current_depth = current_depth + layer;
            }
        }
    }
    if (hit < 0.5) {
        return current_uv;
    }
    #ifdef TEXTURE_TABLE
    let h_after = textureSampleLevel(texture_table[table_index(6u)], tex_samp, current_uv, 0.0).r;
    #else
    let h_after = textureSampleLevel(aux_tex, tex_samp, current_uv, 0.0).r;
    #endif
    #ifdef TEXTURE_TABLE
    let h_before = textureSampleLevel(texture_table[table_index(6u)], tex_samp, prev_uv, 0.0).r;
    #else
    let h_before = textureSampleLevel(aux_tex, tex_samp, prev_uv, 0.0).r;
    #endif
    let da = prev_depth - (1.0 - h_before);
    let db = current_depth - (1.0 - h_after);
    let denom = da - db;
    var t = 0.0;
    if (abs(denom) > 1e-5) {
        t = clamp(da / denom, 0.0, 1.0);
    }
    return mix(prev_uv, current_uv, t);
}

// Tangent of a double-sided back face. The caller flips the normal (`n = -n`) and builds the
// bitangent from it, which negates that too; negating the tangent as well makes the back face's
// tangent frame the front frame negated (the glTF sample viewer's recipe), so a normal-mapped
// back face shades with exactly the front's mapped normal reversed. Flipping the normal alone
// mirrors the map's X slope.
fn back_face_tangent(t: vec3<f32>, front_facing: bool) -> vec3<f32> {
    return select(-t, t, front_facing);
}

// The unit view direction in the surface's tangent frame (z along the geometric normal), for the
// eye model's cornea refraction; w = 0 when the mesh has no tangents.
fn eye_view_ts(in: VertexOutput, front_facing: bool) -> vec4<f32> {
    var geo = normalize(in.world_normal);
    if !front_facing {
        geo = -geo;
    }
    let raw = in.world_tangent.xyz;
    let t_raw = raw - geo * dot(geo, raw);
    if (length(t_raw) < 1e-5) {
        return vec4<f32>(0.0, 0.0, 1.0, 0.0);
    }
    let t = normalize(t_raw);
    let b = cross(geo, t) * in.world_tangent.w;
    let view = normalize(frame.camera_pos.xyz - in.world_position);
    return vec4<f32>(dot(view, t), dot(view, b), dot(view, geo), 1.0);
}

fn surface_uv(in: VertexOutput, front_facing: bool) -> vec2<f32> {
    // `pcss.z` is the profile gate. `quality.w` is the PCSS noise index.
    if (frame.pcss.z <= 0.5 || material.response.z <= 1e-5) {
        return in.uv;
    }
    var geo = normalize(in.world_normal);
    if !front_facing {
        geo = -geo;
    }
    let raw = in.world_tangent.xyz;
    if (length(raw) < 1e-5) {
        return in.uv;
    }
    let t = normalize(raw - geo * dot(geo, raw));
    if (length(t) < 1e-5) {
        return in.uv;
    }
    let b = cross(geo, t) * in.world_tangent.w;
    // Unit view. An unnormalized vector's Z is the camera distance, so the
    // step count saturates at `min_steps` for any shot farther than a metre.
    let view = normalize(frame.camera_pos.xyz - in.world_position);
    let view_ts = vec3<f32>(dot(view, t), dot(view, b), dot(view, geo));
    return parallax_uv(in.uv, view_ts, material.response.z);
}

#ifdef MATERIAL_GRAPH
// The material graph's surface function (`specs/13` § 7 contract). `regia_matgraph` emits
// `material_surface`; the renderer splices it in at the marker below before composing.
// @material_graph_source

// A back face gets its normal, its tangent (`back_face_tangent`) and its bitangent sign negated, so
// a graph that builds its frame as `(t, cross(n, t) * w, n)` gets the front frame negated, as the
// standard surface does: flipping the normal alone mirrored the map's X slope (audit G render5,
// ADR-4950).
fn graph_surface_input(in: VertexOutput, front_facing: bool) -> regia::pbr::surface::SurfaceInput {
    var geo = normalize(in.world_normal);
    if !front_facing {
        geo = -geo;
    }
    let to_eye = frame.camera_pos.xyz - in.world_position;
    let tangent = vec4<f32>(
        back_face_tangent(in.world_tangent.xyz, front_facing),
        select(-in.world_tangent.w, in.world_tangent.w, front_facing),
    );
    return regia::pbr::surface::SurfaceInput(
        in.world_position,
        geo,
        in.uv,
        normalize(to_eye),
        in.color,
        tangent,
        length(to_eye),
        in.uv1,
        in.object_position,
        select(-1.0, 1.0, front_facing),
    );
}
#endif

// Masked-material alpha test. A plain frame keeps the hard `alpha < cutoff`. On a TSR
// frame (`interior_half.w` bit 0) the test is hashed where the alpha texture is minified
// (Wyman & McGuire, "Hashed Alpha Testing", I3D 2017): the mip chain averages a thin mask
// toward its coverage, which a fixed cutoff turns into all or nothing (distant strands
// vanish or fill), so the cutoff moves toward a per-pixel, per-frame uniform threshold
// (interleaved gradient noise, Jimenez 2014, plus a golden-ratio step per frame) and a pixel
// survives with probability `alpha`; TSR's accumulation averages that into fractional
// coverage. Where the texture is magnified the cutoff stays hard: TSR's jittered samples
// already resolve such an edge like geometry, with less noise than any hash.
// `minified` is the sampled LOD clamped to [0, 1] (0 = magnified, 1 = a full mip down),
// taken by the caller in uniform control flow. The depth prepass and the colour pass
// evaluate the same inputs, so they discard the same pixels.
// The threshold is a per-pixel phase advanced per frame by the golden ratio (an R1 sequence),
// so every pixel steps through evenly spread thresholds. The phase (ADR-1489): on a held view
// (`interior_half.w` bit 5: no camera motion since the last frame, or no last frame) interleaved
// gradient noise as before, whose 3 × 3 neighbourhoods are the most even and converge fastest
// under a still history; on a moving view the scrambled hierarchy of `alpha_hash_phase`. IGN is
// one lattice (frequency (-0.444, 0.309) cycles per pixel), and under a moving camera the short
// history leaves its residual on that lattice: a 1 px stipple over every hashed canopy. The
// hierarchy's residual has no peak.
fn alpha_rejects(alpha: f32, cutoff: f32, minified: f32, frag: vec2<f32>) -> bool {
    let flags = u32(frame.interior_half.w + 0.5);
    if ((flags & 1u) == 0u) {
        return alpha < cutoff;
    }
    var phase = fract(52.9829189 * fract(dot(frag, vec2<f32>(0.06711056, 0.00583715))));
    if ((flags & 32u) == 0u) {
        phase = alpha_hash_phase(vec2<u32>(max(frag, vec2<f32>(0.0))));
    }
#ifdef FRAME_BLUE_NOISE
    var noise = fract(phase + 0.618034 * frame.quality.w);
    if (frame_blue_noise_on()) {
        // The slices already step through time: no golden-ratio advance on top.
        noise = frame_blue_noise_at(frag);
    }
#else
    let noise = fract(phase + 0.618034 * frame.quality.w);
#endif
    return alpha < mix(cutoff, noise, minified);
}

// PCG output permutation (O'Neill 2014, as in Jarzynski & Olano, "Hash Functions for GPU
// Rendering", JCGT 2020): one well-mixed 32-bit value per input.
fn alpha_hash_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

// A uniform per-pixel phase in [0, 1) without a lattice (ADR-1489): the pixel's rank in a
// three-level hierarchy of 2 × 2 cells (8 × 8 pixels), each cell ordering its four children by
// a permutation hashed from the cell's coordinates and level (the random-digit-permutation
// ordering of Ahmed & Wonka, "Screen-Space Blue-Noise Diffusion of Monte Carlo Sampling
// Error via Hierarchical Ordering of Pixels", SIGGRAPH Asia 2020; Owen 1995 scrambling).
// The finest level is the most significant digit, so every aligned 2 × 2 block holds one
// phase in each quarter of [0, 1); the last hash dithers inside the rank's 1/64 step.
// `regia_render::alpha_hash::hierarchy_phase` is the CPU mirror.
fn alpha_hash_phase(pix: vec2<u32>) -> f32 {
    var rank = 0u;
    var h = 0u;
    for (var level = 0u; level < 3u; level = level + 1u) {
        let pos = ((pix.x >> level) & 1u) | (((pix.y >> level) & 1u) << 1u);
        let cell = pix >> vec2<u32>(level + 1u);
        h = alpha_hash_pcg((cell.x * 0x8da6b343u) ^ (cell.y * 0xd8163841u) ^ ((level + 1u) * 0xcb1ab31fu));
        // Permutation `p` of S4: child `a` takes digit 0; the other three take 1, 2, 3 in one of
        // the six orders (rotation `k`, reflected when `p >= 12`).
        let p = h % 24u;
        let a = p & 3u;
        let k = (p >> 2u) % 3u;
        var j = pos - select(0u, 1u, pos > a);
        if (p >= 12u) {
            j = 2u - min(j, 2u);
        }
        let digit = select(1u + (j + k) % 3u, 0u, pos == a);
        rank = rank * 4u + digit;
    }
    return (f32(rank) + f32(h >> 8u) * (1.0 / 16777216.0)) * (1.0 / 64.0);
}

// `alpha_rejects`'s `minified`: the albedo LOD at `uv` (with the frame's texture bias).
fn alpha_minification(uv: vec2<f32>) -> f32 {
    #ifdef TEXTURE_TABLE
    let t = uv * vec2<f32>(textureDimensions(texture_table[table_index(0u)]));
    #else
    let t = uv * vec2<f32>(textureDimensions(albedo_tex));
    #endif
    let footprint = max(length(dpdx(t)), length(dpdy(t)));
    return regia::utils::saturate(log2(max(footprint, 1.0e-6)) + frame.fill_color.w);
}

// ---- Hair cards (`hair.rs`) -------------------------------------------------------------
// Kajiya & Kay 1989 with Scheuermann 2004's tangent shifts; a hair material leaves
// `hair_b.w` above 0.5 and every other material reads `false` here, so no other pixel changes.
fn hair_on() -> bool {
    return material.hair_b.w > 0.5;
}

// 0 at `ramp start`, 1 at `ramp end` along the card's v (a flat ramp is 0).
fn hair_ramp_t(uv: vec2<f32>) -> f32 {
    let span = material.hair_d.y - material.hair_d.x;
    if (abs(span) < 1e-6) {
        return 0.0;
    }
    return clamp((uv.y - material.hair_d.x) / span, 0.0, 1.0);
}

// Alpha multiplier root to tip (1 for a non-hair material).
fn hair_alpha_mul(uv: vec2<f32>) -> f32 {
    if (!hair_on()) {
        return 1.0;
    }
    return mix(material.hair_d.z, material.hair_c.w, hair_ramp_t(uv));
}

// Colour multiplier root to tip (white for a non-hair material).
fn hair_color_mul(uv: vec2<f32>) -> vec3<f32> {
    if (!hair_on()) {
        return vec3<f32>(1.0);
    }
    return mix(material.hair_e.rgb, material.hair_c.rgb, hair_ramp_t(uv));
}

// The hashed strategies take the stochastic threshold wherever TSR runs (`alpha_rejects`'
// `minified` = 1), in the prepass and the colour pass alike; other materials keep their
// texture-LOD driven blend of hard and hashed.
fn hair_minified(minified: f32) -> f32 {
    let strategy = u32(material.hair_b.w + 0.5);
    if (strategy == 1u || strategy == 3u) {
        return 1.0;
    }
    return minified;
}

// Per-strand jitter of the tangent shift (stable across a strand's width).
fn hair_strand_shift(uv: vec2<f32>) -> f32 {
    return (hair_hash(floor(uv.x * material.hair_e.w)) - 0.5) * material.hair_d.w;
}

// Diffuse cosine for hair: wrapped by the transmission so a card lit from behind glows.
fn hair_light_cos(n_dot_l: f32) -> f32 {
    if (!hair_on()) {
        return max(n_dot_l, 0.0);
    }
    let w = material.hair_b.z;
    return clamp((n_dot_l + w) / (1.0 + w), 0.0, 1.0);
}

// One Kajiya-Kay lobe, (e + 2) / 2pi * sin(T, H)^e, faded where the half vector passes the
// strand's mirror direction.
fn hair_kk(t: vec3<f32>, h: vec3<f32>, e: f32) -> f32 {
    let th = clamp(dot(t, h), -1.0, 1.0);
    let sin_th = sqrt(max(1.0 - th * th, 0.0));
    let atten = smoothstep(-1.0, 0.0, th);
    return atten * (e + 2.0) * (0.5 / regia::pbr::lighting::PI) * pow(sin_th, e);
}

// The two-lobe specular: the primary lobe takes the light's colour, the shifted secondary lobe
// the hair colour. `jitter` is the strand's shift offset.
fn hair_specular(t: vec3<f32>, n: vec3<f32>, h: vec3<f32>, hair_color: vec3<f32>, jitter: f32) -> vec3<f32> {
    let t1 = normalize(t + n * (material.hair_a.x + jitter));
    let t2 = normalize(t + n * (material.hair_a.y + jitter));
    let primary = material.hair_b.x * hair_kk(t1, h, material.hair_a.z);
    let secondary = material.hair_b.y * hair_kk(t2, h, material.hair_a.w);
    return vec3<f32>(primary) + hair_color * secondary;
}

@fragment
fn fs_depth(in: VertexOutput, @builtin(front_facing) front_facing: bool) {
    // `discard` disables early-Z for the whole pipeline. Opaque draws compile
    // this out (`ALPHA_TEST` is set on the mask and CPU modules only).
#ifdef ALPHA_TEST
#ifdef MATERIAL_GRAPH
    let graph = material_surface(graph_surface_input(in, front_facing));
    // A graph has no single alpha texture: the hard cutoff (still resolved by TSR's jitter).
    let mode = u32(material.alpha.x + 0.5);
    if (mode == 1u && alpha_rejects(graph.alpha, material.alpha.y, 0.0, in.clip_position.xy)) || lod_dither_discard(in.lod_dither, in.clip_position.xy) {
        discard;
    }
#else
    let uv = surface_uv(in, front_facing);
    #ifdef TEXTURE_TABLE
    let alb = sample_material_slot(0u, uv);
    #else
    let alb = sample_material(albedo_tex, uv);
    #endif
    let alpha = alb.a * material.base_color.a * in.color.a * hair_alpha_mul(in.uv);
    let alpha_minified = hair_minified(alpha_minification(uv));
    let mode = u32(material.alpha.x + 0.5);
    if (mode == 1u && alpha_rejects(alpha, material.alpha.y, alpha_minified, in.clip_position.xy)) || lod_dither_discard(in.lod_dither, in.clip_position.xy) {
        discard;
    }
#endif
#endif
}

// Transmission, first of two blends (`frame.rs` `transmit_mul`): what is behind the surface
// times the light that crosses it. The surface transmits `transmission · (1 − F)` (Schlick, the
// IOR's F0), tinted by the base colour (glTF) and by the volume (Beer–Lambert: the normal-
// incidence transmittance over a path that lengthens as `1 / N·V`). The second blend adds the
// surface's own reflection and remaining diffuse (`fs_main`).
@fragment
fn fs_transmit(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
#ifdef MATERIAL_GRAPH
    // A graph's base colour is its surface function's (graph set `transmit_mul`, ADR-347);
    // the standard block's colour and texture are not the graph's. A transmittance never
    // brightens what is behind, so graph maths above 1 is clamped.
    let graph = material_surface(graph_surface_input(in, front_facing));
    let base = clamp(graph.base_color, vec3<f32>(0.0), vec3<f32>(1.0));
#else
    let uv = surface_uv(in, front_facing);
    #ifdef TEXTURE_TABLE
    let alb = table_sample_bias(0u, uv, frame.fill_color.w);
    #else
    let alb = textureSampleBias(albedo_tex, tex_samp, uv, frame.fill_color.w);
    #endif
    let base = alb.rgb * material.base_color.rgb * in.color.rgb;
#endif
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
    let v = normalize(frame.camera_pos.xyz - in.world_position);
    let no_v = clamp(abs(dot(n, v)), 1e-3, 1.0);
    let f0 = select(0.04, material.pom.w, material.pom.w > 0.0);
    let fv = f0 + (1.0 - f0) * pow(1.0 - no_v, 5.0);
    let t = regia::utils::saturate(material.pom.z);
    let path = 1.0 / max(no_v, 0.2);
    let volume = pow(clamp(material.transmit.rgb, vec3<f32>(1e-4), vec3<f32>(1.0)), vec3<f32>(path));
#ifdef GLASS
    let gf = glass_fragment(in.world_position, in.uv, n, v);
    if gf.hole > 0.5 {
        discard;
    }
    return vec4<f32>(t * (1.0 - fv) * base * volume * glass_transmittance(gf), 1.0);
#else
#ifdef THIN_FILM
    return vec4<f32>(t * (vec3<f32>(1.0) - transmit_film_fresnel(uv, no_v, f0)) * base * volume, 1.0);
#else
    return vec4<f32>(t * (1.0 - fv) * base * volume, 1.0);
#endif
#endif
}

// Screen-space refraction (opt-in, `FrameQuality::screen_space_refraction`): a copy of the
// opaque HDR frame and the scene depth, bound only for `fs_refract` and `fs_transmit_add`.
@group(3) @binding(0) var refract_scene: texture_2d<f32>;
@group(3) @binding(1) var refract_depth: texture_depth_2d;
@group(3) @binding(2) var refract_samp: sampler;
// The glass front layer, drawn with the refraction copy (`frame.rs` "glass front layer"): the
// nearest transmissive layer's depth and transmittance (`fs_transmit`) per pixel.
@group(3) @binding(7) var front_depth: texture_depth_2d;
@group(3) @binding(8) var front_t: texture_2d<f32>;

// What a transmissive layer's own light (reflection, remaining diffuse) is seen through: 1 for
// the nearest layer, the nearest layer's transmittance for a layer behind it. The panes' own
// light is added after every multiply of their run (`frame.rs`), so without this the far side
// of a canopy, or the inner sheet of a two-sheet pane, added its reflection over the near side
// unattenuated (DO27's X12: dark glass, transmittance 0.017, read 5-27x Cycles). Exact for two
// layers; a third and further layers take the nearest's transmittance only (McGuire & Enderton
// 2011 composite the same product order-independently).
fn front_layer_weight(in: VertexOutput) -> vec3<f32> {
    let p = vec2<i32>(in.clip_position.xy);
    let d = textureLoad(front_depth, p, 0);
    let n = frame.params.x;
    let f = frame.params.y;
    let z_front = n * f / max(f - d * (f - n), 1e-6);
    let z_here = n * f / max(f - in.clip_position.z * (f - n), 1e-6);
    // The nearest layer finds its own depth; 2 mm + 0.05 % separates a pane's two sheets.
    if (z_here <= z_front * 1.0005 + 0.002) {
        return vec3<f32>(1.0);
    }
    return textureLoad(front_t, p, 0).rgb;
}

// The reflection visibility of a blended pane: 0 where its mirror ray went behind a surface it
// does not see the far side of (`pane_reflection_trace` on the nearest pane, where a thin hit
// replaces the environment through `pane_hit` instead; `pane_reflection_visibility` on the panes
// behind it), 1 elsewhere.
var<private> pane_visibility: f32 = 1.0;
// A transmissive pane's screen-space mirror hit, traced by `fs_transmit_add`
// (`pane_reflection_trace`): rgb = radiance at the hit (cd/m², not pre-exposed), a = its weight
// (0 = no hit; it fades to 0 at the screen border so the environment map takes over there).
var<private> pane_hit: vec4<f32> = vec4<f32>(0.0);

#ifdef GLASS
// Per-instance glass (`glass` module, `frame/glass_gpu.rs`): the crack page of a damaged instance
// and the own-canopy view mode. Bound with the refraction copy in group 3 (dummies when the frame
// does not refract).
struct GlassParams {
    // World -> page UV (xy) for the planar mapping.
    mask_from_world: mat4x4<f32>,
    // xyz = world page +U, w = 1 when the page maps UV0.
    axis_u: vec4<f32>,
    // xyz = world page +V (image down), w = metres per page UV unit.
    axis_v: vec4<f32>,
    // rgb = crack scatter (linear), a = transmission factor on a crack.
    scatter: vec4<f32>,
    // x = crack roughness, y = crazing haze, z = crazing roughness, w = crack-normal strength.
    shade: vec4<f32>,
    // x = hole cutoff, y = edge jitter, z = 1 pressure pane, w = pane gap (metres).
    holes: vec4<f32>,
    // rgb = inner pane tint (linear), w = 1 when damaged (a page is bound).
    inner: vec4<f32>,
    // x = 1 viewer inside (2: and nearest layer only, `fs_canopy_add_front`), y = specular scale,
    // z = radians per pixel, w = page size (texels).
    view: vec4<f32>,
}
@group(3) @binding(3) var<uniform> glass: GlassParams;
@group(3) @binding(4) var glass_mask: texture_2d<f32>;
@group(3) @binding(5) var glass_normal: texture_2d<f32>;
@group(3) @binding(6) var glass_samp: sampler;

// One fragment's crack state: `m` = page texel (R cracks, G crazing, B holes, A bend), `n` = the
// shading normal with the crack normal applied, `hole` = 1 where the pane is missing, `ghost` =
// the outer cracks seen on a pressure pane's clear inner pane (parallax-offset by the gap).
struct GlassFrag {
    m: vec4<f32>,
    n: vec3<f32>,
    hole: f32,
    ghost: f32,
}

fn glass_fragment(world: vec3<f32>, uv0: vec2<f32>, n_in: vec3<f32>, v: vec3<f32>) -> GlassFrag {
    // Plain locals, one struct built at the end: a function-scope struct `var` fails spirv-val.
    if glass.inner.w < 0.5 {
        return GlassFrag(vec4<f32>(0.0), n_in, 0.0, 0.0);
    }
    var uv = uv0;
    if glass.axis_u.w < 0.5 {
        uv = (glass.mask_from_world * vec4<f32>(world, 1.0)).xy;
    }
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) {
        return GlassFrag(vec4<f32>(0.0), n_in, 0.0, 0.0);
    }
    // Mip from distance and page density (no derivatives: callers sit after discards).
    let page = glass.view.w;
    let metres_per_texel = max(glass.axis_v.w / page, 1e-6);
    let pixel_m = length(frame.camera_pos.xyz - world) * glass.view.z;
    let lod = max(log2(max(pixel_m / metres_per_texel, 1.0)), 0.0);
    let m = textureSampleLevel(glass_mask, glass_samp, uv, lod);
    // Jagged hole edge: the cutoff wanders per two-texel cell.
    let jitter = (glass_hash(floor(uv * page * 0.5)) - 0.5) * glass.holes.y;
    let hole = select(0.0, 1.0, m.b > glass.holes.x + jitter);
    let nxy = textureSampleLevel(glass_normal, glass_samp, uv, lod).xy * 2.0 - vec2<f32>(1.0);
    let tu = glass.axis_u.xyz - n_in * dot(n_in, glass.axis_u.xyz);
    let tv = glass.axis_v.xyz - n_in * dot(n_in, glass.axis_v.xyz);
    var n = n_in;
    if dot(tu, tu) > 1e-8 && dot(tv, tv) > 1e-8 {
        // Tangent Y is image up = page −V.
        let bend = (normalize(tu) * nxy.x - normalize(tv) * nxy.y) * m.a * glass.shade.w;
        n = normalize(n_in + bend);
    }
    var ghost = 0.0;
    if glass.holes.z > 0.5 {
        let no_v = max(abs(dot(n_in, v)), 0.2);
        let shift = vec2<f32>(dot(v, glass.axis_u.xyz), dot(v, glass.axis_v.xyz)) * glass.holes.w
            / (no_v * max(glass.axis_v.w, 1e-4));
        let g = textureSampleLevel(glass_mask, glass_samp, uv - shift, lod);
        ghost = g.r * (1.0 - g.b);
    }
    return GlassFrag(m, n, hole, ghost);
}

// Light crossing a damaged fragment: cracks scatter instead of transmitting, crazing clouds, and a
// pressure pane's inner pane adds its tint and the ghost of the outer cracks.
fn glass_transmittance(f: GlassFrag) -> vec3<f32> {
    let crack = 1.0 - f.m.r * (1.0 - glass.scatter.a);
    let haze = 1.0 - f.m.g * glass.shade.y;
    var inner = vec3<f32>(1.0);
    if glass.holes.z > 0.5 && glass.inner.w > 0.5 {
        inner = glass.inner.rgb * (1.0 - 0.35 * f.ghost);
    }
    return vec3<f32>(crack * haze) * inner;
}
#endif

// Transmission, refraction pass (between `fs_transmit` and the reflection add): adds
// T·(behind(refracted) − behind(here)), so the multiply pass's unbent background becomes
// the bent one. The offset is the surface normal's part across the view, scaled by the
// volume thickness and (IOR − 1), projected to the screen. Off screen, or where the bent
// sample lands on something in front of the glass, it falls back to the unbent copy
// (a zero delta). A thin wall (thickness 0) bends nothing.
@fragment
fn fs_refract(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    let thickness = material.transmit.w;
    // Rough transmission (ADR-2075): `hair_a.x` of a material without hair; 0 is clear glass.
    let rough = select(material.hair_a.x, 0.0, hair_on());
#ifdef GLASS
    // Crack normals bend even a thin wall.
    if thickness <= 0.0 && glass.inner.w < 0.5 && rough <= 0.0 {
        return vec4<f32>(0.0);
    }
#else
    if thickness <= 0.0 && rough <= 0.0 {
        return vec4<f32>(0.0);
    }
#endif
    let uv = surface_uv(in, front_facing);
    #ifdef TEXTURE_TABLE
    let alb = table_sample(0u, uv);
    #else
    let alb = textureSample(albedo_tex, tex_samp, uv);
    #endif
    let base = alb.rgb * material.base_color.rgb * in.color.rgb;
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
    let v = normalize(frame.camera_pos.xyz - in.world_position);
    let no_v = clamp(abs(dot(n, v)), 1e-3, 1.0);
    let f0 = select(0.04, material.pom.w, material.pom.w > 0.0);
    let fv = f0 + (1.0 - f0) * pow(1.0 - no_v, 5.0);
    let t = regia::utils::saturate(material.pom.z);
    let path = 1.0 / max(no_v, 0.2);
    let volume = pow(clamp(material.transmit.rgb, vec3<f32>(1e-4), vec3<f32>(1.0)), vec3<f32>(path));
#ifdef GLASS
    let gf = glass_fragment(in.world_position, in.uv, n, v);
    if gf.hole > 0.5 {
        discard;
    }
    let transmittance = t * (1.0 - fv) * base * volume * glass_transmittance(gf);
#else
#ifdef THIN_FILM
    let transmittance = t * (vec3<f32>(1.0) - transmit_film_fresnel(uv, no_v, f0)) * base * volume;
#else
    let transmittance = t * (1.0 - fv) * base * volume;
#endif
#endif
    // IOR back from the normal-incidence reflectance: f0 = ((n − 1) / (n + 1))².
    let r0 = sqrt(clamp(f0, 0.0, 0.81));
    let ior = (1.0 + r0) / (1.0 - r0);
    let across = n - v * dot(n, v);
#ifdef GLASS
    // A crack face is a tilted micro-surface: its extra normal tilt bends what is behind over a
    // shallow effective depth (5 cm), on top of the pane's own bend.
    let across_crack = gf.n - v * dot(gf.n, v);
    let bent = in.world_position - across * thickness * (ior - 1.0) - (across_crack - across) * 0.05;
#else
    let bent = in.world_position - across * thickness * (ior - 1.0);
#endif
    let here_clip = frame.view_proj * vec4<f32>(in.world_position, 1.0);
    let bent_clip = frame.view_proj * vec4<f32>(bent, 1.0);
    let dims = vec2<f32>(textureDimensions(refract_scene));
    let here_px = vec2<i32>(in.clip_position.xy);
    let behind_here = textureLoad(refract_scene, here_px, 0).rgb;
    if bent_clip.w <= 1e-4 || here_clip.w <= 1e-4 {
        return vec4<f32>(0.0);
    }
    let delta_ndc = bent_clip.xy / bent_clip.w - here_clip.xy / here_clip.w;
    let view_px = max(frame.screen.xy, vec2<f32>(1.0));
    let bent_px = in.clip_position.xy + vec2<f32>(delta_ndc.x, -delta_ndc.y) * 0.5 * view_px;
    if any(bent_px < vec2<f32>(0.0)) || any(bent_px >= min(view_px, dims)) {
        return vec4<f32>(0.0);
    }
    // Something in front of the glass (nearer than this fragment) must not show through it.
    let bent_depth = textureLoad(refract_depth, vec2<i32>(bent_px), 0);
    if bent_depth < in.clip_position.z {
        return vec4<f32>(0.0);
    }
    var behind_bent = textureSampleLevel(refract_scene, refract_samp, bent_px / dims, 0.0).rgb;
    if rough > 0.0 {
        // The lobe's mean over the copy, centred where the smooth pass samples; per-pixel
        // rotation by interleaved gradient noise, advanced per frame by the golden ratio.
        let g = 52.9829189 * fract(dot(in.clip_position.xy, vec2<f32>(0.06711056, 0.00583715)));
        let rotation = fract(vec2<f32>(g, g * 1.618034) + 0.618034 * frame.quality.w);
        behind_bent = regia::pbr::rough_transmission::screen_average(refract_scene, refract_depth, refract_samp,
            frame.view_proj, frame.params.xy, frame.camera_pos.xyz, in.world_position, n, v,
            regia::pbr::rough_transmission::thin_alpha(rough, ior), in.clip_position.z, in.clip_position.xy,
            bent_px, view_px, rotation, behind_bent);
    }
    return vec4<f32>(transmittance * (behind_bent - behind_here), 0.0);
}

// The analytic haze reads the cloud transmittance map (ADR-5944): the frame has the map for its
// sun term (`cloud_sun.w`, ADR-320) and the aerial haze is on. False keeps `apply_aerial` as it
// was, with no load of the map.
fn haze_under_clouds() -> bool {
    return frame.cloud_sun.w > 0.5 && frame.fog_params.w > 0.5 && frame.fog_params.x > 0.0;
}

// The precipitation's veil reads the cloud transmittance map (ADR-5944 item 8): the frame has
// the map for its sun term (`cloud_sun.w`, ADR-320), whatever the fog's mode. False keeps
// `weather_inscatter` as it was, with no load of the map.
fn rain_under_clouds() -> bool {
    return frame.cloud_sun.w > 0.5;
}

// The share of its sun the haze between the eye and `world_position` keeps under the layer.
fn haze_sun_lit(world_position: vec3<f32>) -> f32 {
    return regia::cloud_shadow::cloud_sun_on_ray(
        cloud_sun_map,
        frame.cloud_sun,
        frame.camera_pos.xyz,
        world_position,
        -normalize(dir_light.direction),
    );
}

#ifdef SUBSURFACE
// Screen-space subsurface scattering (`crate::subsurface`, ADR-2049). This variant draws only the
// materials that scatter, into the colour target and two more: the light that scatters without the
// albedo (rgb, alpha 1, so a filtered read knows how much of it was on these pixels), and the albedo
// that goes back on after the blur (rgb; alpha the reach in world units, negative for the second
// profile, 0 outside these pixels). The colour target keeps the rest of the light.
// `frame/subsurface_gpu.rs` blurs and adds it back.
var<private> subsurface_light: vec4<f32>;
var<private> subsurface_albedo: vec4<f32>;

// What the view's media do to `color` (the aerial or fog term of `shade_forward`, the same call).
fn subsurface_media(color: vec3<f32>, world_position: vec3<f32>) -> vec3<f32> {
    var fogged = color;
    if (frame.fog_color.a > 0.5) {
        let tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
        if (haze_under_clouds()) {
            fogged = regia::aerial::apply_aerial_under_clouds(
                color,
                world_position,
                frame.camera_pos.xyz,
                frame.fog_params,
                tint,
                -normalize(dir_light.direction),
                dir_light.color * dir_light.illuminance,
                frame.post_exposure.z,
                haze_sun_lit(world_position),
            );
        } else {
            fogged = regia::aerial::apply_aerial(
                color,
                world_position,
                frame.camera_pos.xyz,
                frame.fog_params,
                tint,
                -normalize(dir_light.direction),
                dir_light.color * dir_light.illuminance,
                frame.post_exposure.z,
            );
        }
    } else {
        let exposure = max(frame.post_exposure.y, 1e-8);
        let fog_radiance = vec4<f32>(frame.fog_color.rgb / exposure, frame.fog_color.a);
        fogged = regia::fog::apply_fog(color, world_position, frame.camera_pos.xyz, frame.fog_params, fog_radiance);
    }
    // The falling rain or snow's own extinction, as `shade_forward` applies it.
    if (frame.weather2.w > 0.0) {
        fogged = regia::forward::surface_weather::weather_extinction(
            fogged,
            length(world_position - frame.camera_pos.xyz),
            frame.weather2.w,
            weather_inscatter_at(normalize(world_position - frame.camera_pos.xyz), world_position),
        );
    }
    return fogged;
}
#endif

// `v` turned by the unit quaternion `q` (xyzw): `v + w t + u × t`, `t = 2 u × v`. With
// `frame.ambient_rot` it takes a world offset into an ambient volume's frame (ADR-7275).
fn ambient_volume_local(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

// Ambient volumes (ADR-7211, `lights_views::FrameAmbientVolume::weight_at`): the irradiance the
// boxes add at `p`, each 1 inside and fading to 0 over its blend distance past the faces. The
// point is taken into each box's own frame first, so a turned box bounds along its axes (ADR-7275).
fn ambient_volume_irradiance(p: vec3<f32>) -> vec3<f32> {
    var e = vec3<f32>(0.0);
    let count = min(u32(frame.view_mode.z + 0.5), 4u);
    for (var i = 0u; i < count; i = i + 1u) {
        let c = frame.ambient_box[3u * i];
        let h = frame.ambient_box[3u * i + 1u].xyz;
        let local = ambient_volume_local(frame.ambient_rot[i], p - c.xyz);
        let outside = max(abs(local) - h, vec3<f32>(0.0));
        let d = length(outside);
        var w = 0.0;
        if (d <= 0.0) {
            w = 1.0;
        } else if (c.w > 0.0) {
            w = clamp(1.0 - d / c.w, 0.0, 1.0);
        }
        e += frame.ambient_box[3u * i + 2u].xyz * w;
    }
    return e;
}

// A blue → cyan → green → yellow → red ramp over t in [0, 1] (the heat views, ADR-7211).
fn debug_view_heat(t_in: f32) -> vec3<f32> {
    let t = clamp(t_in, 0.0, 1.0);
    let r = clamp(1.5 - abs(4.0 * t - 3.0), 0.0, 1.0);
    let g = clamp(1.5 - abs(4.0 * t - 2.0), 0.0, 1.0);
    let b = clamp(1.5 - abs(4.0 * t - 1.0), 0.0, 1.0);
    return max(vec3<f32>(r, g, b), vec3<f32>(0.05, 0.05, 0.2));
}

// The display-referred colour of debug view `code` (`lights_views::shading_view_code`, ADR-7211).
// `uv_footprint` is the larger UV step across the pixel, `tex_size` the base-colour texture's edge.
fn debug_view_color(
    code: u32,
    world: vec3<f32>,
    pix: vec2<f32>,
    base: vec3<f32>,
    n: vec3<f32>,
    perceptual: f32,
    metallic: f32,
    occlusion: f32,
    emissive_display: vec3<f32>,
    uv_footprint: f32,
    tex_size: f32,
) -> vec3<f32> {
    var out = base;
    switch code {
        case 1u: { out = base + emissive_display; }
        case 2u: { out = normalize(n) * 0.5 + vec3<f32>(0.5); }
        case 3u: { out = base; }
        case 4u: { out = vec3<f32>(perceptual); }
        case 5u: { out = vec3<f32>(metallic); }
        case 6u: { out = vec3<f32>(occlusion); }
        case 7u: {
            // Lights in this pixel's froxel list, 0 to 16+.
            let uv = pix / max(frame.screen.xy, vec2<f32>(1.0));
            let cx = min(u32(uv.x * 16.0), 15u);
            let cy = min(u32(uv.y * 9.0), 8u);
            let view_pos = (frame.view * vec4<f32>(world, 1.0)).xyz;
            let depth = max(-view_pos.z, frame.params.x);
            let cz = regia::clustering::depth_to_slice(depth, frame.params.x, frame.params.y);
            let count = counts[regia::clustering::cluster_index(cx, cy, cz)];
            out = debug_view_heat(f32(count) / 16.0);
        }
        case 8u: {
            // The cascade this pixel's sun shadow reads (`sample_csm_cascades`); gray past them
            // or without cascades.
            out = vec3<f32>(0.3);
            if (frame.extra.x > 0.5) {
                let view_z = -(frame.view * vec4<f32>(world, 1.0)).z;
                let count = clamp(u32(csm.count.x), 1u, 4u);
                var cascade = 0u;
                for (var i = 1u; i < 4u; i = i + 1u) {
                    if (i >= count) {
                        break;
                    }
                    if (view_z > cascade_split(i - 1u)) {
                        cascade = i;
                    }
                }
                // A switch, not a function-scope array: naga emits that as a Function variable.
                if (view_z <= cascade_split(count - 1u)) {
                    switch cascade {
                        case 0u: { out = vec3<f32>(0.9, 0.2, 0.2); }
                        case 1u: { out = vec3<f32>(0.2, 0.85, 0.25); }
                        case 2u: { out = vec3<f32>(0.25, 0.4, 0.95); }
                        default: { out = vec3<f32>(0.95, 0.85, 0.2); }
                    }
                }
            }
        }
        case 9u: {
            // Screen motion since the previous frame (camera motion: this surface's position
            // through both view-projections), 0.5 = still; one unit = an eighth of the view.
            let now = frame.unjittered_view_proj * vec4<f32>(world, 1.0);
            let prev = frame.prev_view_proj * vec4<f32>(world, 1.0);
            let mv = now.xy / max(abs(now.w), 1e-6) - prev.xy / max(abs(prev.w), 1e-6);
            out = vec3<f32>(clamp(vec2<f32>(0.5) + mv * 4.0, vec2<f32>(0.0), vec2<f32>(1.0)), 0.25);
        }
        case 10u: {
            // The base-colour texture's mip level here, 0 (blue) to 10+ (red).
            let lod = log2(max(uv_footprint * tex_size, 1e-6));
            out = debug_view_heat(clamp(lod, 0.0, 10.0) / 10.0);
        }
        default: {}
    }
    return out;
}

#ifdef VISBUFFER
// The visibility-buffer resolve (`visbuffer/vis_resolve.wgsl`) rebuilds `in` for the pixel's
// triangle and calls this body, so both paths share every lighting term. It is a full-screen
// pass: implicit derivatives would cross triangle and object edges, so the four surface
// textures take the triangle's own UV gradients instead. Graph materials stay forward.
var<private> vis_uv_ddx: vec2<f32>;
var<private> vis_uv_ddy: vec2<f32>;
// The same for the specular-AA term's shading-normal derivatives: the unit geometric normal's
// change across the pixel's 2×2 quad on its own triangle (what `dpdx(n)` reads in the forward
// pass, where helper invocations extrapolate the triangle), not a difference against whatever
// surface the neighbouring pixel shows. A normal map's own variation is left to the Toksvig term.
var<private> vis_n_ddx: vec3<f32>;
var<private> vis_n_ddy: vec3<f32>;
// The triangle's world-space face normal (unnormalised), for the sun cascades' receiver plane.
var<private> vis_face_n: vec3<f32>;

fn shade_surface(in: VertexOutput, front_facing: bool) -> vec4<f32> {
#else
// The forward surface (entry points `fs_main` and `fs_transmit_add` below).
fn shade_forward(in: VertexOutput, front_facing: bool) -> vec4<f32> {
#endif
    // Eye model (`shading.x` = 2): iris coverage under the cornea, socket occlusion.
    var eye_iris = 0.0;
    var eye_occlusion = 1.0;
#ifdef MATERIAL_GRAPH
    // A graph material replaces the surface stage only; lighting below is the one code path.
    let graph = material_surface(graph_surface_input(in, front_facing));
    var base = graph.base_color * override_tint(in.overrides).rgb;
    let alpha = graph.alpha;
    // Per-vertex ambient occlusion (ADR-1512) is honored on graph materials too: the factor is
    // read from the vertex color alpha, the graph's own alpha output stays the opacity.
    let vertex_ao_on = (u32(material.alpha.w + 0.5) & 2u) != 0u;
    let vertex_ao = select(1.0, clamp(in.color.a, 0.0, 1.0), vertex_ao_on);
    // The alpha test is decided here but the `discard` waits until the derivative work below
    // (specular AA, the shadow normal, skin curvature) is done: naga writes `discard` as `OpKill`,
    // after which a derivative in the same quad is undefined (ADR-265).
#ifdef ALPHA_TEST
    let mode = u32(material.alpha.x + 0.5);
    let alpha_rejected = (mode == 1u && alpha_rejects(alpha, material.alpha.y, 0.0, in.clip_position.xy)) || lod_dither_discard(in.lod_dither, in.clip_position.xy);
#else
    let alpha_rejected = false;
#endif
    var n = graph.normal;
    var metallic = regia::utils::saturate(graph.metallic);
    var perceptual = regia::utils::saturate(graph.roughness);
    let occlusion = graph.occlusion;
    let emissive = graph.emissive;
#else
    let uv = surface_uv(in, front_facing);
    #ifdef RESIDENCY_FEEDBACK
    note_residency(in.clip_position.xy, uv);
    #endif
    #ifdef TEXTURE_TABLE
    var alb = sample_material_slot(0u, uv);
    #else
    var alb = sample_material(albedo_tex, uv);
    #endif
    // Eye (shading model 2): the iris seen through the refracting cornea, the limbal ring.
    if (u32(material.shading.x + 0.5) == 2u) {
        let center = material.shading_b.xy;
        let rho = length(in.uv - center);
        let mask = regia::pbr::shading_models::eye_iris_mask(rho, material.shading_a.x, material.shading_a.y);
        eye_iris = mask.x;
        let view_ts = eye_view_ts(in, front_facing);
        if (material.shading_a.w > 0.0 && view_ts.w > 0.5 && mask.x > 0.0) {
            let iris_uv = regia::pbr::shading_models::eye_iris_uv(in.uv, center, material.shading_a.x, material.shading_a.w, material.shading_b.z, view_ts.xyz);
            #ifdef TEXTURE_TABLE
            let iris = sample_material_slot(0u, iris_uv);
            #else
            let iris = sample_material(albedo_tex, iris_uv);
            #endif
            alb = vec4<f32>(mix(alb.rgb, iris.rgb, mask.x), alb.a);
        }
        alb = vec4<f32>(alb.rgb * (1.0 - material.shading_a.z * mask.y), alb.a);
        eye_occlusion = 1.0 - material.shading_c.x * smoothstep(material.shading_c.y, material.shading_c.z, rho);
    }
    var base = alb.rgb * material.base_color.rgb * in.color.rgb * override_tint(in.overrides).rgb;
    base = base * hair_color_mul(in.uv);
    // Per-vertex ambient occlusion (`alpha.w` bit 1, opaque materials only; ADR-1512): the vertex
    // color's alpha is a 0..1 factor on the indirect light, not an opacity. Off (the default)
    // leaves the vertex alpha as the opacity, exactly as before.
    let vertex_ao_on = (u32(material.alpha.w + 0.5) & 2u) != 0u;
    let vertex_ao = select(1.0, clamp(in.color.a, 0.0, 1.0), vertex_ao_on);
    let alpha = alb.a * material.base_color.a * select(in.color.a, 1.0, vertex_ao_on) * hair_alpha_mul(in.uv);
    // The alpha test is decided here; the `discard` waits until the derivative work below is done
    // (see the graph branch and ADR-265).
#ifdef ALPHA_TEST
    let alpha_minified = hair_minified(alpha_minification(uv));
    let mode = u32(material.alpha.x + 0.5);
    let alpha_rejected = (mode == 1u && alpha_rejects(alpha, material.alpha.y, alpha_minified, in.clip_position.xy)) || lod_dither_discard(in.lod_dither, in.clip_position.xy);
#else
    let alpha_rejected = false;
#endif

    var n = normalize(in.world_normal);
    // Double-sided shading: a visible back face shades with the flipped
    // normal (the glTF doubleSided recipe), so both sides of an unculled
    // triangle light correctly.
    if !front_facing {
        n = -n;
    }
    let normal_scale = material.factors.z;
    // Length of the filtered normal-map sample: below 1 where the footprint averages
    // differing normals (`toksvig_roughness`).
    var normal_length = 1.0;
    if normal_scale > 0.0 {
        // `b` from the flipped normal is the front bitangent negated; `back_face_tangent`
        // negates the tangent too, so a back face's whole frame is the front one negated and
        // its mapped normal is exactly the front's reversed (the sheet's other side).
        let t_front = normalize(in.world_tangent.xyz);
        let b = cross(n, t_front) * in.world_tangent.w;
        let t = back_face_tangent(t_front, front_facing);
        #ifdef TEXTURE_TABLE
        let ts = decode_normal_sample(sample_material_slot(1u, uv));
        #else
        let ts = decode_normal_sample(sample_material(normal_tex, uv));
        #endif
        // Source (glTF) RGBA8 maps keep the filtered length for the Toksvig term. A cooked normal
        // map (`alpha.z` bit 3) has its variance baked into the cooked ORM mips instead: BC5 / EAC
        // RG / ASTC rebuild z (length 1), and its RGBA8 CPU decode must not count it twice.
        if ((u32(material.alpha.z + 0.5) & 8u) == 0u) {
            normal_length = length(ts);
        }
        let mapped = vec3<f32>(ts.x * normal_scale, ts.y * normal_scale, ts.z);
        n = normalize(mat3x3<f32>(t, b, n) * mapped);
    }

    #ifdef TEXTURE_TABLE
    let orm = sample_material_slot(2u, uv);
    #else
    let orm = sample_material(occlusion_tex, uv);
    #endif
    let ao = orm.r;
    // A packed ORM (`alpha.w` bit 0; bit 1 is the per-vertex occlusion flag): G and B are the glTF metallic-roughness channels and scale
    // the factors (glTF 2.0 § 3.9.2); the product is clamped, so an out-of-range factor
    // still respects the map.
    var mr_scale = vec2<f32>(1.0, 1.0);
    if ((u32(material.alpha.w + 0.5) & 1u) != 0u) {
        mr_scale = orm.gb;
    }
    var metallic = regia::utils::saturate(material.factors.x * mr_scale.y);
    var perceptual = regia::utils::saturate(material.factors.y * mr_scale.x);
    // Edge wear consumes a curvature or AO channel. Intensity 0 leaves the sample unused.
    if (material.wear.x > 1e-5 && material.wear.w > 0.5) {
        var wear_sample = 0.5;
        let source = u32(material.wear.w + 0.5);
        if (source == 1u) {
            #ifdef TEXTURE_TABLE
            wear_sample = textureSampleLevel(texture_table[table_index(6u)], tex_samp, uv, 0.0).g;
            #else
            wear_sample = textureSampleLevel(aux_tex, tex_samp, uv, 0.0).g;
            #endif
        } else if (source == 2u) {
            wear_sample = ao;
        }
        let wear = regia::utils::saturate((wear_sample - material.pom.y) * material.wear_tint.w) * material.wear.x;
        base = mix(base, material.wear_tint.rgb, wear);
        perceptual = regia::utils::saturate(perceptual + material.wear.y * wear);
        metallic = regia::utils::saturate(metallic + material.wear.z * wear);
    }
    // Detail maps. All three strengths at 0 skip the samples so a lab material stays bit-identical.
    if (material.detail.x > 1e-5 || material.detail.y > 1e-5 || material.detail.z > 1e-5) {
        let dist = length(frame.camera_pos.xyz - in.world_position);
        var fade = 1.0;
        if (material.detail_fade.w > material.detail_fade.z) {
            fade = regia::utils::saturate((material.detail_fade.w - dist) / (material.detail_fade.w - material.detail_fade.z));
        }
        let duv = uv * material.detail_fade.xy;
        #ifdef TEXTURE_TABLE
        let mask = textureSampleLevel(texture_table[table_index(5u)], tex_samp, duv, 0.0).a;
        #else
        let mask = textureSampleLevel(detail_normal, tex_samp, duv, 0.0).a;
        #endif
        if (material.detail.x > 1e-5) {
            #ifdef TEXTURE_TABLE
            let detail_alb = textureSampleLevel(texture_table[table_index(4u)], tex_samp, duv, 0.0).rgb;
            #else
            let detail_alb = textureSampleLevel(detail_tex, tex_samp, duv, 0.0).rgb;
            #endif
            let w = fade * mask * material.detail.x;
            base = base * mix(vec3<f32>(1.0), detail_alb * 2.0, w);
        }
        if (material.detail.z > 1e-5) {
            #ifdef TEXTURE_TABLE
            let detail_r = textureSampleLevel(texture_table[table_index(4u)], tex_samp, duv, 0.0).a;
            #else
            let detail_r = textureSampleLevel(detail_tex, tex_samp, duv, 0.0).a;
            #endif
            perceptual = mix(perceptual, detail_r, fade * mask * material.detail.z);
        }
        if (material.detail.y > 1e-5) {
            // Level 0: the tangent test below is non-uniform, so textureSample is
            // illegal here.
            #ifdef TEXTURE_TABLE
            let dn = textureSampleLevel(texture_table[table_index(5u)], tex_samp, duv, 0.0).xyz * 2.0 - vec3<f32>(1.0);
            #else
            let dn = textureSampleLevel(detail_normal, tex_samp, duv, 0.0).xyz * 2.0 - vec3<f32>(1.0);
            #endif
            var n1 = vec3<f32>(0.0, 0.0, 1.0);
            if (normal_scale > 0.0) {
                #ifdef TEXTURE_TABLE
                let ts = decode_normal_sample(table_sample_level(1u, uv));
                #else
                let ts = decode_normal_sample(textureSampleLevel(normal_tex, tex_samp, uv, 0.0));
                #endif
                n1 = normalize(vec3<f32>(ts.x * normal_scale, ts.y * normal_scale, ts.z));
            }
            if (length(in.world_tangent.xyz) > 1e-5) {
                var geo = normalize(in.world_normal);
                if !front_facing {
                    geo = -geo;
                }
                // The whole tangent frame negated on a back face, as for the base normal map.
                let t_front = normalize(in.world_tangent.xyz);
                let b = cross(geo, t_front) * in.world_tangent.w;
                let t = back_face_tangent(t_front, front_facing);
                let strength = fade * mask * material.detail.y;
                let n2 = vec3<f32>(dn.x * strength, dn.y * strength, dn.z);
                let blended = normalize(vec3<f32>(n1.x + n2.x, n1.y + n2.y, n1.z * n2.z));
                n = normalize(mat3x3<f32>(t, b, geo) * blended);
            }
        }
    }
#ifdef MASK_DECALS
    // Per-instance recolor and decals (`mask_decals`), after detail and wear.
    let masked = apply_mask_decals(base, uv);
    base = masked.base;
    metallic = metallic * (1.0 - masked.paint);
    let mask_glow = masked.glow;
#endif
    // Sampled with the other material textures, before the alpha-test `discard`.
    #ifdef TEXTURE_TABLE
    let emissive_tex_rgb = sample_material_slot(3u, uv).rgb;
    #else
    let emissive_tex_rgb = sample_material(emissive_tex, uv).rgb;
    #endif
#ifdef THIN_FILM
    // The film's coverage (R) and thickness (G): before the alpha-test `discard`, like the rest.
    let film_sample = sample_material(film_map, uv);
#endif
#endif

#ifdef FRAME_FORWARD_DECALS
    // Forward decals (ADR-3255): the tile's decals change the surface inputs before any light is
    // evaluated, so the decal's normal and roughness are lit (and read by the AOV outputs). A
    // uniform branch: a frame without forward decals runs none of it.
    if (fd_data.header.counts.x > 0u) {
        let decaled = apply_forward_decals(FdSurface(base, n, perceptual, metallic), in.world_position, in.clip_position.xy, in.decal_layer);
        base = decaled.base;
        n = decaled.n;
        perceptual = decaled.perceptual;
        metallic = decaled.metallic;
    }
#endif
    // Specular AA: widen the lobe by the normal variance one pixel covers, so minified normal
    // detail and tight curvature do not sparkle (uniform branch). The geometric term reads the
    // shading normal's screen derivatives; the Toksvig term the filtered normal map's length.
    if (frame.specular_aa.x > 0.0) {
#ifdef VISBUFFER
        perceptual = regia::pbr::lighting::specular_aa_roughness(perceptual, vis_n_ddx, vis_n_ddy, frame.specular_aa.x, frame.specular_aa.y);
#else
        perceptual = regia::pbr::lighting::specular_aa_roughness(perceptual, dpdx(n), dpdy(n), frame.specular_aa.x, frame.specular_aa.y);
#endif
#ifndef MATERIAL_GRAPH
        perceptual = regia::pbr::lighting::toksvig_roughness(perceptual, normal_length, frame.specular_aa.z);
#endif
    }
    // The sun cascades' receiver plane is the rasterised triangle's (`sample_cascade`),
    // taken here in uniform control flow.
#ifdef VISBUFFER
    let face_len = length(vis_face_n);
    var shadow_n = n;
    if (face_len > 1e-12) {
        shadow_n = select(-vis_face_n, vis_face_n, dot(vis_face_n, n) >= 0.0) / face_len;
    }
#else
    let shadow_n = csm_geometric_normal(in.world_position, n);
#endif

    // Skin (shading model 1, `shading_models.wgsl`): the curvature radius, the profile's ring
    // kernel at it (zero on flat surfaces: the clamped cosine), the per-channel diffuse normals
    // and the thin-skin transmittance. Uniform branch, so the derivatives are legal here.
    let shading_model = u32(material.shading.x + 0.5);
    var skin_geo = n;
    var skin_nr = n;
    var skin_ng = n;
    var skin_nb = n;
    var sk1 = vec3<f32>(0.0);
    var sk2 = vec3<f32>(0.0);
    var sk4 = vec3<f32>(0.0);
    var sk6 = vec3<f32>(0.0);
    var sk8 = vec3<f32>(0.0);
    var sk10 = vec3<f32>(0.0);
    var sk12 = vec3<f32>(0.0);
    var skin_t = vec3<f32>(0.0);
    if (shading_model == 1u) {
        var geo = normalize(in.world_normal);
        if !front_facing {
            geo = -geo;
        }
        skin_geo = geo;
#ifdef VISBUFFER
        // A full-screen resolve has no per-triangle position derivatives: the authored radius,
        // or a face's typical 30 mm.
        var radius_mm = select(30.0, material.shading_a.y, material.shading_a.y > 0.0);
#else
        let curvature = length(fwidth(geo)) / max(length(fwidth(in.world_position)), 1e-9);
        var radius_mm = material.shading_b.w / max(curvature, 1e-6);
        if (material.shading_a.y > 0.0) {
            radius_mm = material.shading_a.y;
        }
#endif
        let radius_eff = clamp(radius_mm / max(material.shading_a.x, 1e-3), 0.5, 1000.0);
        if (radius_eff < 200.0) {
            let k = regia::pbr::shading_models::skin_kernel(radius_eff);
            sk1 = k.first;
            sk2 = k.second;
            sk4 = k.fourth;
            sk6 = k.sixth;
            sk8 = k.eighth;
            sk10 = k.tenth;
            sk12 = k.twelfth;
        }
        let blur = regia::utils::saturate(material.shading_a.z);
        skin_nr = normalize(mix(n, geo, blur));
        skin_ng = normalize(mix(n, geo, blur * 0.5));
        skin_nb = normalize(mix(n, geo, blur * 0.25));
        if (material.shading_a.w > 0.0) {
            skin_t = regia::pbr::shading_models::skin_transmittance(material.shading_b.z * radius_eff) * material.shading_a.w;
        }
#ifdef SUBSURFACE
        // The blur stands in for the pre-integrated falloff and the blurred normals: the clamped
        // cosine at the mapped normal, so the same light is not scattered twice.
        sk1 = vec3<f32>(0.0);
        sk2 = vec3<f32>(0.0);
        sk4 = vec3<f32>(0.0);
        sk6 = vec3<f32>(0.0);
        sk8 = vec3<f32>(0.0);
        sk10 = vec3<f32>(0.0);
        sk12 = vec3<f32>(0.0);
        skin_nr = n;
        skin_ng = n;
        skin_nb = n;
#endif
    }

    // The mip-level debug view's UV footprint (ADR-7211), taken with the other derivatives.
#ifdef VISBUFFER
    let view_uv_footprint = max(length(vis_uv_ddx), length(vis_uv_ddy));
#else
    let view_uv_footprint = max(length(dpdx(in.uv)), length(dpdy(in.uv)));
#endif
    // Every implicit derivative of the surface (material samples, `dpdx(n)`, the shadow normal,
    // skin curvature) is behind us: reject the alpha-tested fragment now.
    if alpha_rejected {
        discard;
    }

    let v_dir = normalize(frame.camera_pos.xyz - in.world_position);
#ifdef GLASS
    // Damaged glass: holes are cut out; cracks turn frosted (rough, white, scattering instead of
    // transmitting) and tilt the normal. The own-canopy mode only lowers the specular below.
    let gf = glass_fragment(in.world_position, in.uv, n, v_dir);
    if gf.hole > 0.5 {
        discard;
    }
    n = gf.n;
    perceptual = mix(perceptual, max(perceptual, glass.shade.z), regia::utils::saturate(gf.m.g * 1.5));
    perceptual = mix(perceptual, max(perceptual, glass.shade.x), gf.m.r);
    base = mix(base, glass.scatter.rgb, regia::utils::saturate(gf.m.r + gf.m.g * glass.shade.y));
    let glass_spec = glass.view.y;
#endif
    // Surface weather (spec 99 § 6.5), after the surface stage and before lighting: a uniform
    // branch, so a dry frame runs none of it. Not on Adreno (`FORWARD_TRIM`, ADR-3145): the
    // surfaces of the full shader stay dry there (the lite shader has its own weather).
#ifndef FORWARD_TRIM
    if (frame.weather.x + frame.weather.y + frame.weather.z > 0.0) {
        let weathered = regia::forward::surface_weather::weather_apply(
            regia::forward::surface_weather::WeatherSurface(base, perceptual, metallic, n),
            shadow_n,
            in.world_position,
            frame.weather,
            frame.weather2.x,
            weather_response(perceptual, metallic),
            precip_exposure(in.world_position, shadow_n),
            0.0,
        );
        base = weathered.albedo;
        perceptual = weathered.roughness;
        metallic = weathered.metallic;
        n = weathered.normal;
    }
#endif
    // At most 1: N·V of a surface seen head-on rounds past 1, and `pow(1 − N·V, 5)` of the
    // negative base is NaN (`f_schlick`).
    let no_v = clamp(dot(n, v_dir), 1e-5, 1.0);
    let rough = max(perceptual * perceptual, regia::pbr::lighting::MIN_ALPHA);
    // Transmission (`pom.z`): the transmitted share replaces the diffuse lobe of a thin
    // dielectric; the specular reflection stays (see the blend alpha at the end).
#ifdef GLASS
    // Only cracks and crazing turn transmitted light into scatter; the inner pane's tint does not.
    let glass_scatter_free = (1.0 - gf.m.r * (1.0 - glass.scatter.a)) * (1.0 - gf.m.g * glass.shade.y);
    let transmission = regia::utils::saturate(material.pom.z) * glass_scatter_free;
#else
    let transmission = regia::utils::saturate(material.pom.z);
#endif
    let diffuse_color = base * (1.0 - metallic) * (1.0 - transmission);
    let f0_dielectric = select(0.04, material.pom.w, material.pom.w > 0.0);
    var f0 = mix(vec3<f32>(f0_dielectric), base, vec3<f32>(metallic));
    // The coat lobe: the material's clearcoat, or on an eye the wet cornea over everything,
    // with the iris's own reflection taken by the cornea above it.
    var coat_w = material.coat.x;
    var coat_r = material.coat.y;
    if (shading_model == 2u) {
        coat_w = 1.0;
        coat_r = material.shading_b.w;
        f0 = f0 * (1.0 - eye_iris);
    }
#ifdef THIN_FILM
    // Thin film (`thin_film.wgsl`). The direct lobes take its Fresnel at l·h over the base's `f0`
    // (`film_base`); from here on `f0` is the proxy the Schlick-shaped terms take (the environment,
    // the multi-scatter energy, the fitted area lights).
    let film_base = f0;
    let film_ior = film_block.params.y;
    let film_nm = mix(film_block.params.z, film_block.params.w, film_sample.g);
    let film_cover = regia::pbr::thin_film::weight(film_block.params.x * film_sample.r, film_nm);
    f0 = regia::pbr::thin_film::schlick_f0(no_v, film_base, film_cover, film_ior, film_nm);
#endif
    // Skin's second specular lobe (`shading_b.xy`), direct light only.
    let rough2 = max(material.shading_b.x * material.shading_b.x, regia::pbr::lighting::MIN_ALPHA);
    // Fabric sheen (`shading.y`): colour, α = roughness², the base layer's share at n·v.
    let sheen_on = material.shading.y > 0.5 && shading_model != 2u;
    let sheen_alpha = material.sheen.w * material.sheen.w;
    var sheen_base_v = 1.0;
    if (sheen_on) {
        sheen_base_v = regia::pbr::shading_models::sheen_scaling(material.sheen.rgb, no_v, material.sheen.w);
    }
#ifndef MATERIAL_GRAPH
    let occlusion = mix(1.0, ao, regia::utils::saturate(material.factors.w));
#ifdef MASK_DECALS
    let emissive = material.emissive.rgb * material.emissive.w * emissive_tex_rgb + mask_glow;
#else
    let emissive = material.emissive.rgb * material.emissive.w * emissive_tex_rgb;
#endif
#endif
    var aniso_t = vec3<f32>(1.0, 0.0, 0.0);
    var aniso_b = vec3<f32>(0.0, 1.0, 0.0);
    var aniso_ok = 0.0;
    if (abs(material.coat.z) > 1e-4 && length(in.world_tangent.xyz) > 1e-5) {
        let raw = in.world_tangent.xyz;
        let t_front = normalize(raw - n * dot(n, raw));
        if (length(t_front) > 1e-5) {
            // Negated frame on a back face (`back_face_tangent`), so the anisotropy direction
            // is the front's, not its mirror image.
            let b0 = cross(n, t_front) * in.world_tangent.w;
            let t0 = back_face_tangent(t_front, front_facing);
            let c = cos(material.coat.w);
            let s = sin(material.coat.w);
            aniso_t = t0 * c + b0 * s;
            aniso_b = b0 * c - t0 * s;
            aniso_ok = 1.0;
        }
    }
    // Hair: the strand direction (mesh tangent, orthogonalised against the shading normal) and
    // the strand's shift jitter. The lobes themselves are built per light.
    var hair_t = vec3<f32>(1.0, 0.0, 0.0);
    var hair_ok = 0.0;
    var hair_jitter = 0.0;
    if (hair_on() && length(in.world_tangent.xyz) > 1e-5) {
        let raw = in.world_tangent.xyz;
        let t_perp = raw - n * dot(n, raw);
        if (length(t_perp) > 1e-5) {
            hair_t = normalize(t_perp);
            hair_ok = 1.0;
            hair_jitter = hair_strand_shift(in.uv);
        }
    }
    // Directional albedo of the single-scatter GGX lobe at n·v (the DFG table's B channel) and its
    // hemispherical average (A), for the punctual multi-scatter energy compensation.
    var e_o = 1.0;
    var e_avg = 1.0;
    var es_o = vec3<f32>(0.0);
    if (material.response.y > 0.5) {
        let dfg_v = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0);
        e_o = dfg_v.z;
        e_avg = dfg_v.w;
        es_o = clamp(f0 * dfg_v.x + vec3<f32>(dfg_v.y), vec3<f32>(0.0), vec3<f32>(1.0));
    }
#ifdef BRDF_ENERGY_COMPENSATED
    // `BrdfEnergy::Compensated` (ADR-2632): the DFG sample at n·v for every material (the
    // diffuse coupling needs it with or without multi-scatter), and the image-light diffuse weight.
    let dfg_o = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0);
    let ms_on = material.response.y > 0.5;
#endif

    var lit = vec3<f32>(0.0);
#ifdef SUBSURFACE
    // The diffuse share of `lit`, kept beside it term by term (see each `sss_diffuse` below).
    var sss_diffuse = vec3<f32>(0.0);
#endif
    // Environment specular this pixel received (the share a traced reflection replaces).
    var ssr_env = vec3<f32>(0.0);
    // Bit 0 of `alpha.z`; bits 1-2 are the normal map's channel layout (`decode_normal_sample`).
    let unlit = select(f32(u32(material.alpha.z + 0.5) & 1u), 1.0, (in.overrides.y & 3u) == 3u);
    if unlit < 0.5 {
        let l = -normalize(dir_light.direction);
        let h = normalize(l + v_dir);
        let no_l = hair_light_cos(dot(n, l));
        let no_h = max(dot(n, h), 0.0);
        let lo_h = max(dot(l, h), 0.0);
        // The direct BRDF is shared with the reference path tracer (`pbr_brdf.wgsl`). The lobes
        // are carried in vector `var`s, never a `BrdfLobes` `var`: naga decorates every struct
        // with Offset and a Function-storage struct variable fails spirv-val (VUID 10684).
#ifdef THIN_FILM
        let iso = regia::pbr::brdf::isotropic_lobes_film(no_v, no_l, no_h, lo_h, diffuse_color, film_base, rough, film_cover, film_ior, film_nm);
#else
        let iso = regia::pbr::brdf::isotropic_lobes(no_v, no_l, no_h, lo_h, diffuse_color, f0, rough);
#endif
        var lobe_diff = iso.diffuse;
        var lobe_spec = iso.specular;
        var lobe_att = iso.coat_att;
        // The sun as the disc it is (ADR-3190): `dir_light._pad0` carries its angular radius as
        // f32 bits (`DirectionalLightUniform::with_specular_disc`). 0, the default, is the point
        // sun and takes no branch; a word that is no radius (NaN, infinite, wider than
        // `MAX_SPECULAR_DISC`) is the point sun too. Each isotropic GGX lobe becomes its mean
        // over the disc (`disc_lobe_scale`, at that lobe's own alpha); an anisotropic lobe keeps
        // the point sun.
        let sun_disc = bitcast<f32>(dir_light._pad0);
        let disc_on = sun_disc > 0.0 && sun_disc <= 0.2;
        if (disc_on) {
            lobe_spec *= regia::pbr::lighting::disc_lobe_scale(no_h, lo_h, rough, sun_disc);
        }
        if (aniso_ok > 0.5) {
            let ab = regia::pbr::lighting::anisotropic_alphas(rough, material.coat.z);
            let da = regia::pbr::lighting::d_ggx_anisotropic(dot(aniso_t, h), dot(aniso_b, h), no_h, ab.x, ab.y);
            let va = regia::pbr::lighting::v_smith_ggx_anisotropic(dot(aniso_t, v_dir), dot(aniso_b, v_dir), dot(aniso_t, l), dot(aniso_b, l), no_v, no_l, ab.x, ab.y);
            lobe_spec = da * va * iso.fresnel;
        }
        if (shading_model == 1u && material.shading_b.y > 0.0) {
            // The second lobe carries the same film as the first (audit wave 5, render M1).
#ifdef THIN_FILM
            let iso2 = regia::pbr::brdf::isotropic_lobes_film(no_v, no_l, no_h, lo_h, diffuse_color, film_base, rough2, film_cover, film_ior, film_nm);
#else
            let iso2 = regia::pbr::brdf::isotropic_lobes(no_v, no_l, no_h, lo_h, diffuse_color, f0, rough2);
#endif
            var spec2 = iso2.specular;
            if (disc_on) {
                spec2 *= regia::pbr::lighting::disc_lobe_scale(no_h, lo_h, rough2, sun_disc);
            }
            lobe_spec = mix(lobe_spec, spec2, material.shading_b.y);
        }
        if (hair_on() && hair_ok > 0.5) {
            lobe_spec = hair_specular(hair_t, n, h, base, hair_jitter);
        }
#ifdef BRDF_ENERGY_COMPENSATED
        {
            let dfg_l = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(max(no_l, 0.0), perceptual), 0.0);
            let cm = regia::pbr::brdf::with_compensation(regia::pbr::brdf::BrdfLobes(lobe_diff, lobe_spec, iso.fresnel, lobe_att), f0, diffuse_color, perceptual, dfg_o, dfg_l, ms_on);
            lobe_diff = cm.diffuse;
            lobe_spec = cm.specular;
        }
#else
        if (material.response.y > 0.5) {
            let ms = regia::pbr::brdf::with_multi_scatter(regia::pbr::brdf::BrdfLobes(lobe_diff, lobe_spec, iso.fresnel, lobe_att), f0, e_o, e_avg, diffuse_color, es_o);
            lobe_diff = ms.diffuse;
            lobe_spec = ms.specular;
        }
#endif
        if (sheen_on) {
            let base_share = min(sheen_base_v, regia::pbr::shading_models::sheen_scaling(material.sheen.rgb, no_l, material.sheen.w));
            let sheen_lobe = material.sheen.rgb * (regia::pbr::shading_models::d_charlie(no_h, sheen_alpha) * regia::pbr::shading_models::v_sheen(no_v, no_l));
            lobe_diff = lobe_diff * base_share;
            lobe_spec = lobe_spec * base_share + sheen_lobe;
        }
        if (coat_w > 1e-4) {
            let c = regia::pbr::brdf::with_clearcoat(regia::pbr::brdf::BrdfLobes(lobe_diff, lobe_spec, iso.fresnel, lobe_att), no_v, no_l, no_h, lo_h, coat_w, coat_r);
            lobe_diff = c.diffuse;
            lobe_spec = c.specular;
            lobe_att = c.coat_att;
        }
#ifdef GLASS
        lobe_spec *= glass_spec;
#endif
        var sun = (lobe_diff + lobe_spec) * dir_light.color * dir_light.illuminance * no_l;
        if (shading_model == 1u) {
            // Pre-integrated falloff per channel on the diffuse lobe; the specular keeps n·l.
            let c = vec3<f32>(dot(skin_nr, l), dot(skin_ng, l), dot(skin_nb, l));
            let falloff = regia::pbr::shading_models::skin_falloff(c, regia::pbr::shading_models::SkinKernel(sk1, sk2, sk4, sk6, sk8, sk10, sk12));
            sun = (lobe_diff * falloff + lobe_spec * no_l) * dir_light.color * dir_light.illuminance;
        }
#ifdef SUBSURFACE
        var sun_diffuse = lobe_diff * dir_light.color * dir_light.illuminance * no_l;
        if (shading_model == 1u) {
            sun_diffuse = lobe_diff * max(dot(n, l), 0.0) * dir_light.color * dir_light.illuminance;
        }
#endif
#ifdef LIGHTMAP
        // A lightmap baked with `--direct-sun` already holds this diffuse term (ADR-2024).
        if (lightmap_holds_sun_diffuse(in.lightmap_scale_offset)) {
            sun = lobe_spec * dir_light.color * dir_light.illuminance * no_l;
#ifdef SUBSURFACE
            sun_diffuse = vec3<f32>(0.0);
#endif
        }
#endif
        // The sun's visibility here (shadow map × contact), also used for the enclosure a
        // blocked reflection sees (below).
        var sun_visibility = 1.0;
        if (frame.extra.x > 0.5) {
            // Not `csm`: that name is the cascade uniform read below.
#ifdef SUN_SHADOW_RT
            // Ray-traced sun shadows (ADR-1149, ADR-2035, `regia_render::hwrt::variant`): the trace
            // wrote each pixel's sun visibility into the screen visibility texture (binding 22, read
            // below as `contact_shadow`), so it replaces the shadow map on the surfaces it started
            // from. Blended and transmissive surfaces and ghost overrides draw over that depth
            // whatever their alpha mode: they keep the shadow map.
            // One `sample_csm` call site, evaluated outside any branch, as in the default variant.
            let sun_traced = u32(material.alpha.x + 0.5) <= 1u
                && material.pom.z <= 0.0
                && (in.overrides.y & 9u) != 9u;
            let csm_vis = select(
                sample_csm(in.world_position, shadow_n, l, in.clip_position.xy),
                1.0,
                sun_traced,
            );
#else
            let csm_vis = sample_csm(in.world_position, shadow_n, l, in.clip_position.xy);
#endif
            sun *= csm_vis;
#ifdef SUBSURFACE
            sun_diffuse *= csm_vis;
#endif
            sun_visibility = csm_vis;
            dbg_sun = vec3<f32>(csm_vis, 1.0, no_l);
            // Blended surfaces are not in the depth the contact pass marched.
#ifdef SUN_SHADOW_RT
            if (sun_traced) {
#else
            if (u32(material.alpha.x + 0.5) <= 1u) {
#endif
                let contact = contact_shadow(in.clip_position.xy);
                sun *= contact;
#ifdef SUBSURFACE
                sun_diffuse *= contact;
#endif
                sun_visibility *= contact;
                dbg_sun.y = contact;
            }
            // Glass between the surface and the sun (colored shadow map); skipped, and the
            // frame bit-identical, when no translucent caster is drawn.
            if (csm.count.y > 0.5 && !translucent_surface()) {
                let sun_tint = sample_csm_transmittance(in.world_position, n, l);
                sun *= sun_tint;
#ifdef SUBSURFACE
                sun_diffuse *= sun_tint;
#endif
            }
        }
        if (frame.cloud_sun.w > 0.5) {
            let cloud_tint = regia::cloud_shadow::cloud_sun_transmittance(cloud_sun_map, frame.cloud_sun, in.world_position, l, frame.camera_pos.xyz);
            sun *= cloud_tint;
#ifdef SUBSURFACE
            sun_diffuse *= cloud_tint;
#endif
        }
        lit += sun;
#ifdef SUBSURFACE
        sss_diffuse += sun_diffuse;
#endif
        // More directional lights (ADR-7211): the sun's BRDF, unshadowed (only the sun has the
        // cascades). Zero extra lights skip the loop: the frame is unchanged.
        let extra_lights = min(u32(frame.view_mode.y + 0.5), 3u);
        for (var xi = 0u; xi < extra_lights; xi = xi + 1u) {
            let xl = -normalize(frame.extra_dir[2u * xi].xyz);
            let xh = normalize(xl + v_dir);
            let x_no_l = max(dot(n, xl), 0.0);
            if (x_no_l <= 0.0) {
                continue;
            }
            let x_no_h = max(dot(n, xh), 0.0);
            let x_lo_h = max(dot(xl, xh), 0.0);
#ifdef THIN_FILM
            let x_iso = regia::pbr::brdf::isotropic_lobes_film(no_v, x_no_l, x_no_h, x_lo_h, diffuse_color, film_base, rough, film_cover, film_ior, film_nm);
#else
            let x_iso = regia::pbr::brdf::isotropic_lobes(no_v, x_no_l, x_no_h, x_lo_h, diffuse_color, f0, rough);
#endif
            var x_diff = x_iso.diffuse;
            var x_spec = x_iso.specular;
            var x_att = x_iso.coat_att;
#ifdef BRDF_ENERGY_COMPENSATED
            {
                let x_dfg_l = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(x_no_l, perceptual), 0.0);
                let x_cm = regia::pbr::brdf::with_compensation(regia::pbr::brdf::BrdfLobes(x_diff, x_spec, x_iso.fresnel, x_att), f0, diffuse_color, perceptual, dfg_o, x_dfg_l, ms_on);
                x_diff = x_cm.diffuse;
                x_spec = x_cm.specular;
            }
#else
            if (material.response.y > 0.5) {
                let x_ms = regia::pbr::brdf::with_multi_scatter(regia::pbr::brdf::BrdfLobes(x_diff, x_spec, x_iso.fresnel, x_att), f0, e_o, e_avg, diffuse_color, es_o);
                x_diff = x_ms.diffuse;
                x_spec = x_ms.specular;
            }
#endif
            if (sheen_on) {
                let x_share = min(sheen_base_v, regia::pbr::shading_models::sheen_scaling(material.sheen.rgb, x_no_l, material.sheen.w));
                let x_sheen = material.sheen.rgb * (regia::pbr::shading_models::d_charlie(x_no_h, sheen_alpha) * regia::pbr::shading_models::v_sheen(no_v, x_no_l));
                x_diff = x_diff * x_share;
                x_spec = x_spec * x_share + x_sheen;
            }
            if (coat_w > 1e-4) {
                let x_c = regia::pbr::brdf::with_clearcoat(regia::pbr::brdf::BrdfLobes(x_diff, x_spec, x_iso.fresnel, x_att), no_v, x_no_l, x_no_h, x_lo_h, coat_w, coat_r);
                x_diff = x_c.diffuse;
                x_spec = x_c.specular;
            }
#ifdef GLASS
            x_spec *= glass_spec;
#endif
            let x_e = frame.extra_dir[2u * xi + 1u].rgb;
            lit += (x_diff + x_spec) * x_e * x_no_l;
#ifdef SUBSURFACE
            sss_diffuse += x_diff * x_e * x_no_l;
#endif
        }
        if (shading_model == 1u && material.shading_a.w > 0.0) {
            // Thin skin lit from behind (not shadowed: the caster is the skin itself).
            lit += skin_t * diffuse_color * (1.0 / regia::pbr::lighting::PI) * dir_light.color * dir_light.illuminance
                * regia::pbr::shading_models::skin_back_light(dot(skin_geo, l));
        }
        // Large meshes nearby (`MeshOccluderBlock`): the ground in their sun shadow, their
        // faces in place of the sky or ground they hide, their shadow on the fill.
        // (Two vector `var`s, not an `OccLight` `var`: see the lobes note above.)
        var occ_delta = vec3<f32>(0.0);
        var occ_fill = 1.0;
        if (mesh_occ.header.x > 0.5) {
            let o = mesh_occluder_light(in.world_position, n, false);
            occ_delta = o.delta;
            occ_fill = o.fill_vis;
        }
        // Planet-shine (Jupiter-shine on Europa): a Lambert fill from the sky body, shadowed
        // only by the large-mesh proxies.
        var fill_live = vec3<f32>(0.0);
        if (frame.fill_dir.w > 0.5) {
            let lf = normalize(frame.fill_dir.xyz);
            var fill = diffuse_color * (1.0 / regia::pbr::lighting::PI) * frame.fill_color.rgb * max(dot(n, lf), 0.0);
            if (mesh_occ.header.x > 0.5) {
                fill *= occ_fill;
            }
            fill_live = fill;
            lit += fill;
#ifdef SUBSURFACE
            sss_diffuse += fill;
#endif
        }
        // Ambient volumes (ADR-7211): an artist-placed fill on the ambient term inside each box,
        // diffuse, under the material's occlusion. None skips the lookup.
        if (frame.view_mode.z > 0.5) {
            let vol_e = ambient_volume_irradiance(in.world_position);
            let vol_fill = diffuse_color * (1.0 / regia::pbr::lighting::PI) * vol_e * occlusion;
            lit += vol_fill;
#ifdef SUBSURFACE
            sss_diffuse += vol_fill;
#endif
        }
        // Fitted emitters whose light the baked volume holds (ADR-1386): their diffuse and
        // specular parts, taken back out below where the volume covers the receiver.
        var baked_emit_diffuse = vec3<f32>(0.0);
        var baked_emit_spec = vec3<f32>(0.0);

        let pix = in.clip_position.xy;
        // The fragment position is in framebuffer pixels (top-left origin), the froxel grid's
        // layout (`cluster_build.wgsl`: cell y 0 at the top). Reading it as NDC sent every
        // fragment to the bottom-right column's list, so a light lit only what that corner's
        // froxels also reached (lane v017-black-hull).
        let uv = pix / max(frame.screen.xy, vec2<f32>(1.0));
        let cx = min(u32(uv.x * 16.0), 15u);
        let cy = min(u32(uv.y * 9.0), 8u);
        let view_pos = (frame.view * vec4<f32>(in.world_position, 1.0)).xyz;
        let depth = max(-view_pos.z, frame.params.x);
        let cz = regia::clustering::depth_to_slice(depth, frame.params.x, frame.params.y);
        let cid = regia::clustering::cluster_index(cx, cy, cz);
        let nlights = counts[cid];
        let max_per = u32(frame.screen.w);
        let base_i = cid * max_per;
        // Fitted emitters share the split-sum magnitude and the multi-scatter compensation of
        // this view.
        // Only frames with fitted emitters (`interior_half.w` bit 2) pay for the lookup.
        var area_dfg = vec2<f32>(0.0);
        var area_ms = vec3<f32>(1.0);
        if ((u32(frame.interior_half.w + 0.5) & 4u) != 0u) {
            area_dfg = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
            if (material.response.y > 0.5) {
                area_ms = regia::pbr::lighting::energy_compensation(f0, e_o, e_avg);
            }
        }
        var area_spec_scale = 1.0;
#ifdef GLASS
        area_spec_scale = glass_spec;
#endif
#ifdef RESTIR_DI
        // ADR-2652: the list's points and spots are the reservoir's. It shades as one more
        // iteration of this loop (the same body, weighted by W), and the list skips them, so a
        // light is never counted twice; fitted emitters and other kinds keep the loop.
        let rs_px = vec2<u32>(min(pix, max(frame.screen.xy - vec2<f32>(1.0), vec2<f32>(0.0))));
        let rs = restir_reservoirs[rs_px.y * u32(frame.screen.x) + rs_px.x];
        let rs_live = rs.light != 0xffffffffu && rs.light < u32(frame.screen.z) && rs.w > 0.0;
        let light_iterations = nlights + select(0u, 1u, rs_live);
        for (var i: u32 = 0u; i < light_iterations; i = i + 1u) {
            var li = rs.light;
            var rs_w = rs.w;
            if (i < nlights) {
                li = indices[base_i + i];
                rs_w = 1.0;
                let sampled_kind = u32(max(lights[li].position_kind.w + 0.5, 0.0));
                if (sampled_kind == 1u || sampled_kind == 2u) {
                    continue;
                }
            }
#else
        for (var i: u32 = 0u; i < nlights; i = i + 1u) {
            let li = indices[base_i + i];
#endif
            if (lights[li].position_kind.w > 3.5) {
                let emitted = emitter_radiance(li, in.world_position, n, v_dir, no_v, diffuse_color, (f0 * area_dfg.x + vec3<f32>(area_dfg.y)) * area_spec_scale, perceptual, area_ms, f0, area_spec_scale);
                lit += emitted.total;
#ifdef SUBSURFACE
                sss_diffuse += emitted.diffuse;
#endif
                if (emitter_tag(li).y > 0.5) {
                    baked_emit_diffuse += emitted.diffuse;
                    baked_emit_spec += emitted.total - emitted.diffuse;
                }
                continue;
            }
            let lp = lights[li].position_kind.xyz;
            let to_light = lp - in.world_position;
            let dist = length(to_light);
            let radius = lights[li].direction_radius.w;
            // Windowed falloff is exactly 0 past the radius. Leave before the
            // spot cone, the shadow fetch and the GGX terms.
            var att = regia::pbr::lighting::distance_attenuation_source(dist, radius, lights[li].cone_shadow.w);
            if (att <= 0.0) {
                continue;
            }
            let ldir = to_light / max(dist, 1e-4);
            let kind = u32(lights[li].position_kind.w + 0.5);
            if (kind == 2u) {
                let spot_dir = lights[li].direction_radius.xyz;
                att *= regia::pbr::lighting::spot_attenuation(ldir, spot_dir, lights[li].cone_shadow.x, lights[li].cone_shadow.y);
                if (att <= 0.0) {
                    continue;
                }
            }
            // An IES profile (`LightProfile`, ADR-2592): a fractional kind names its atlas band and
            // `direction_radius.xyz` is the nadir. Whole kinds (every light without one) skip it.
            if (fract(lights[li].position_kind.w) > 0.0) {
                att *= regia::pbr::lighting::light_profile_relative(emitter_probes, regia::pbr::lighting::light_profile_code(lights[li].position_kind.w), lights[li].direction_radius.xyz, -ldir);
                if (att <= 0.0) {
                    continue;
                }
            }
            // A sphere shaded through the LTC tables (`LightSourceLtc`, ADR-2592): a negative `cone_shadow.w`, the
            // radius with its sign flipped. The falloff is the tracer's physical one, not the punctual 1 m knee. Under
            // `LTC_SPHERE_MIN_TAN`, and for material lobes the tables do not describe, the representative point below
            // shades the sphere with that same falloff.
            if (lights[li].cone_shadow.w < 0.0) {
                let sphere_r = -lights[li].cone_shadow.w;
                let unsoftened = att * (dist * dist + max(1.0, sphere_r * sphere_r));
                if (sphere_r >= LTC_SPHERE_MIN_TAN * dist && shading_model == 0u && !hair_on() && coat_w <= 1e-4 && !sheen_on && aniso_ok <= 0.5) {
                    if (dot(n, to_light) <= -sphere_r) {
                        continue;
                    }
                    var sphere_shadow = vec3<f32>(1.0);
                    if (lights[li].cone_shadow.z >= 0.0) {
                        var sphere_tile = u32(lights[li].cone_shadow.z + 0.5);
                        if (kind == 1u) {
                            sphere_tile = sphere_tile + cube_face(in.world_position - lp);
                        }
                        sphere_shadow = vec3<f32>(sample_atlas_tile(sphere_tile, in.world_position, n, ldir, pix));
                        if (sphere_shadow.x <= 0.0) {
                            continue;
                        }
                        if (csm.count.z > 0.5 && !translucent_surface()) {
                            sphere_shadow *= sample_atlas_transmittance(sphere_tile, in.world_position, n, ldir);
                        }
                    }
                    // Its own lookup: `area_dfg` and `area_ms` are filled only in frames with fitted emitters.
                    let sphere_dfg = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
                    var sphere_ms = vec3<f32>(1.0);
                    if (material.response.y > 0.5) {
                        sphere_ms = regia::pbr::lighting::energy_compensation(f0, e_o, e_avg);
                    }
                    let sphere_light = sphere_ltc_light(to_light, sphere_r, n, v_dir, no_v, diffuse_color, (f0 * sphere_dfg.x + vec3<f32>(sphere_dfg.y)) * area_spec_scale, perceptual, sphere_ms);
                    // Radiance of a sphere of this intensity: I / (π r²).
                    let sphere_l = lights[li].color_intensity.rgb * lights[li].color_intensity.w * unsoftened * sphere_shadow / (regia::pbr::lighting::PI * sphere_r * sphere_r);
                    lit += sphere_light.total * sphere_l;
#ifdef SUBSURFACE
                    sss_diffuse += sphere_light.diffuse * sphere_l;
#endif
                    continue;
                }
                att = unsoftened / max(dist * dist, 1e-8);
            }
            // Hair wraps n·l for light through the strands (never negative); skin keeps the signed
            // cosine for its wrap past the terminator.
            let nol_signed = select(dot(n, ldir), hair_light_cos(dot(n, ldir)), hair_on());
            if (shading_model == 1u && material.shading_a.w > 0.0) {
                // Thin skin lit from behind, before the shadow test (the caster is the skin).
#ifdef RESTIR_DI
                let lc_t = lights[li].color_intensity.rgb * (lights[li].color_intensity.w * rs_w);
#else
                let lc_t = lights[li].color_intensity.rgb * lights[li].color_intensity.w;
#endif
                lit += skin_t * diffuse_color * (1.0 / regia::pbr::lighting::PI) * lc_t * att
                    * regia::pbr::shading_models::skin_back_light(dot(skin_geo, ldir));
            }
            // Skin wraps diffuse light past the terminator: keep lights up to 120° away.
            if (nol_signed <= select(0.0, -0.5, shading_model == 1u)) {
                continue;
            }
            let nol = max(nol_signed, 0.0);
            // Shadowless lights store a negative tile. The test is uniform
            // across the wave (it depends only on the light), so a shadowless
            // list never issues the atlas fetch.
            var local_shadow = 1.0;
            var local_tint = vec3<f32>(1.0);
            var tinted = false;
            let tile_f = lights[li].cone_shadow.z;
            if (tile_f >= 0.0) {
                var tile_index = u32(tile_f + 0.5);
                if (kind == 1u) {
                    tile_index = tile_index + cube_face(in.world_position - lp);
                }
                local_shadow = sample_atlas_tile(tile_index, in.world_position, n, ldir, pix);
                if (local_shadow <= 0.0) {
                    continue;
                }
                if (csm.count.z > 0.5 && !translucent_surface()) {
                    local_tint = sample_atlas_transmittance(tile_index, in.world_position, n, ldir);
                    tinted = true;
                }
            }
            // A sphere source (`LightSourceSize`, ADR-1367; `cone_shadow.w` = radius): the
            // specular is lit from a representative point of the sphere and its lobe renormalized
            // (Lagarde & de Rousiers 2014 § 4.7.5 with Karis 2013 eqs. 10, 11, 14, widened as a
            // variance; see `pbr_lighting.wgsl`); the diffuse stays at the centre. A light
            // without the component packs 0 and takes none of it.
            let src_r = abs(lights[li].cone_shadow.w);
            var l_spec = ldir;
            var nol_spec = nol;
            if (src_r > 0.0) {
                l_spec = regia::pbr::lighting::sphere_light_dir(to_light, reflect(-v_dir, n), src_r, rough);
                nol_spec = max(dot(n, l_spec), 0.0);
            }
            let hl = normalize(l_spec + v_dir);
            let noh = max(dot(n, hl), 0.0);
            let loh = max(dot(l_spec, hl), 0.0);
            // Vector `var`s, not a `BrdfLobes` `var` (VUID 10684, see the sun term above).
#ifdef THIN_FILM
            let iso_l = regia::pbr::brdf::isotropic_lobes_film(no_v, nol_spec, noh, loh, diffuse_color, film_base, rough, film_cover, film_ior, film_nm);
#else
            let iso_l = regia::pbr::brdf::isotropic_lobes(no_v, nol_spec, noh, loh, diffuse_color, f0, rough);
#endif
            var diff_l = iso_l.diffuse;
            var spec_l = iso_l.specular;
            var att_l = iso_l.coat_att;
            if (aniso_ok > 0.5) {
                let ab = regia::pbr::lighting::anisotropic_alphas(rough, material.coat.z);
                let da = regia::pbr::lighting::d_ggx_anisotropic(dot(aniso_t, hl), dot(aniso_b, hl), noh, ab.x, ab.y);
                let va = regia::pbr::lighting::v_smith_ggx_anisotropic(dot(aniso_t, v_dir), dot(aniso_b, v_dir), dot(aniso_t, l_spec), dot(aniso_b, l_spec), no_v, nol_spec, ab.x, ab.y);
                spec_l = da * va * iso_l.fresnel;
            }
            if (shading_model == 1u && material.shading_b.y > 0.0) {
#ifdef THIN_FILM
                let iso2 = regia::pbr::brdf::isotropic_lobes_film(no_v, nol_spec, noh, loh, diffuse_color, film_base, rough2, film_cover, film_ior, film_nm);
#else
                let iso2 = regia::pbr::brdf::isotropic_lobes(no_v, nol_spec, noh, loh, diffuse_color, f0, rough2);
#endif
                spec_l = mix(spec_l, iso2.specular, material.shading_b.y);
            }
            if (hair_on() && hair_ok > 0.5) {
                spec_l = hair_specular(hair_t, n, hl, base, hair_jitter);
            }
            // The lobe's energy term for the wider light, and its own cosine (the shared
            // `* nol` below is the centre's). The diffuse Fresnel goes back to the centre.
            let spec_cos = nol_spec / max(nol, 1e-4);
            if (src_r > 0.0) {
                let hc = normalize(ldir + v_dir);
#ifdef THIN_FILM
                diff_l = diffuse_color * (1.0 / regia::pbr::lighting::PI)
                    * (vec3<f32>(1.0) - regia::pbr::thin_film::fresnel(max(dot(ldir, hc), 0.0), film_base, film_cover, film_ior, film_nm));
#else
                diff_l = diffuse_color * (1.0 / regia::pbr::lighting::PI)
                    * (vec3<f32>(1.0) - regia::pbr::lighting::f_schlick(max(dot(ldir, hc), 0.0), f0));
#endif
                spec_l = spec_l * regia::pbr::lighting::sphere_light_gain(rough, src_r, to_light, reflect(-v_dir, n), noh, max(dot(n, hc), 0.0), spec_cos);
            }
#ifdef BRDF_ENERGY_COMPENSATED
            {
                // The diffuse coupling at the light's centre (as the 1 − F(l·h) form above).
                let dfg_l = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(max(nol, 0.0), perceptual), 0.0);
                let cm = regia::pbr::brdf::with_compensation(regia::pbr::brdf::BrdfLobes(diff_l, spec_l, iso_l.fresnel, att_l), f0, diffuse_color, perceptual, dfg_o, dfg_l, ms_on);
                diff_l = cm.diffuse;
                spec_l = cm.specular;
            }
#else
            if (material.response.y > 0.5) {
                let ms = regia::pbr::brdf::with_multi_scatter(regia::pbr::brdf::BrdfLobes(diff_l, spec_l, iso_l.fresnel, att_l), f0, e_o, e_avg, diffuse_color, es_o);
                diff_l = ms.diffuse;
                spec_l = ms.specular;
            }
#endif
            if (sheen_on) {
                let base_share = min(sheen_base_v, regia::pbr::shading_models::sheen_scaling(material.sheen.rgb, nol, material.sheen.w));
                let sheen_lobe = material.sheen.rgb * (regia::pbr::shading_models::d_charlie(noh, sheen_alpha) * regia::pbr::shading_models::v_sheen(no_v, nol));
                diff_l = diff_l * base_share;
                spec_l = spec_l * base_share + sheen_lobe;
            }
            if (coat_w > 1e-4) {
                let c = regia::pbr::brdf::with_clearcoat(regia::pbr::brdf::BrdfLobes(diff_l, spec_l, iso_l.fresnel, att_l), no_v, nol_spec, noh, loh, coat_w, coat_r);
                diff_l = c.diffuse;
                if (src_r > 0.0) {
                    // The coat's GGX lobe gets its own energy term (its α, not the base's).
                    let base_spec = spec_l * c.coat_att;
                    let coat_a = max(coat_r * coat_r, regia::pbr::lighting::MIN_ALPHA);
                    let coat_norm = regia::pbr::lighting::sphere_light_normalization(coat_a, src_r, dist);
                    spec_l = base_spec + (c.specular - base_spec) * (coat_norm * spec_cos);
                } else {
                    spec_l = c.specular;
                }
                att_l = c.coat_att;
            }
#ifdef RESTIR_DI
            let lc = lights[li].color_intensity.rgb * (lights[li].color_intensity.w * rs_w);
#else
            let lc = lights[li].color_intensity.rgb * lights[li].color_intensity.w;
#endif
#ifdef GLASS
            spec_l *= glass_spec;
#endif
            if (shading_model == 1u) {
                let c = vec3<f32>(dot(skin_nr, ldir), dot(skin_ng, ldir), dot(skin_nb, ldir));
                let falloff = regia::pbr::shading_models::skin_falloff(c, regia::pbr::shading_models::SkinKernel(sk1, sk2, sk4, sk6, sk8, sk10, sk12));
                lit += (diff_l * falloff + spec_l * nol) * lc * att * local_shadow * local_tint;
#ifdef SUBSURFACE
                sss_diffuse += diff_l * nol * lc * att * local_shadow * local_tint;
#endif
            } else if (tinted) {
                lit += (diff_l + spec_l) * lc * att * nol * local_shadow * local_tint;
#ifdef SUBSURFACE
                sss_diffuse += diff_l * lc * att * nol * local_shadow * local_tint;
#endif
            } else {
                lit += (diff_l + spec_l) * lc * att * nol * local_shadow;
#ifdef SUBSURFACE
                sss_diffuse += diff_l * lc * att * nol * local_shadow;
#endif
            }
        }

        let debug_mode = u32(frame.pcss.w + 0.5);
        if (debug_mode != 0u) {
            var vis = vec3<f32>(0.15);
            if (debug_mode == 1u) {
                let s = select(0.5, dbg_shadow_sum / max(dbg_shadow_n, 1.0), dbg_shadow_n > 0.0);
                vis = vec3<f32>(s);
            } else if (debug_mode == 3u) {
                vis = dbg_sun;
            } else if (debug_mode == 4u) {
                vis = diffuse_color;
            } else if (debug_mode == 5u) {
                vis = n * 0.5 + vec3<f32>(0.5);
            } else if (debug_mode == 6u) {
                vis = shadow_n * 0.5 + vec3<f32>(0.5);
            } else if (debug_mode == 7u) {
                // The roughness AOV (`frame_aov`, ADR-2951): perceptual roughness and metallic as
                // the lights above shaded with them.
                vis = vec3<f32>(perceptual, metallic, 0.0);
            } else {
                vis = dbg_atlas;
            }
            #ifdef skip_srgb_encode
            return vec4<f32>(vis, 1.0);
            #else
            return vec4<f32>(regia::utils::encode_srgb(vis), 1.0);
            #endif
        }

        // ---- Indirect light: sky/IBL, probe GI, occlusion ----
        let screen_uv = clamp(
            in.clip_position.xy / max(frame.screen.xy, vec2<f32>(1.0)),
            vec2<f32>(0.0),
            vec2<f32>(1.0),
        );
        // R = screen-space AO; G, B = the specular lobe's reflection visibility, glossy and rough
        // (`ssao.wgsl`; 1 on tiers without it and in the 1×1 white texture bound with SSAO off).
        // A blended or transmissive surface lies in front of the depth the SSAO pass saw: the
        // occlusion at its pixel is the cockpit's or room's behind it, not its own (ADR-165).
        let ao_sample = select(
            textureSampleLevel(ao_tex, ao_samp, screen_uv, 0, 0.0),
            vec4<f32>(1.0),
            translucent_surface(),
        );
        let ao = occlusion * ao_sample.r;
        // Bent normals (`interior_half.w` bit 4, ADR-1375): the sky, probe and baked irradiance
        // are read along the mean unoccluded direction (the normal leaned away from what the SSAO
        // kernel saw), and the specular occlusion is the cone form below. Opaque and masked
        // surfaces only, as the AO: a blended one lies in front of the depth the kernel saw.
        let bent_on = (u32(frame.interior_half.w + 0.5) & 16u) != 0u && !translucent_surface();
        var gi_n = n;
        if (bent_on) {
            gi_n = bent_shading_normal(n, bent_decode(textureSampleLevel(ao_tex, ao_samp, screen_uv, 1, 0.0).rgb));
        }
        var sky_diffuse = vec3<f32>(0.0);
        var spec_ibl = vec3<f32>(0.0);
        var env_brdf = vec2<f32>(0.0);
        var refl = reflect(-v_dir, n);
        // What the ground plane took: x of the diffuse hemisphere, y of the reflection.
        var env_ground_share = vec2<f32>(0.0);
        // The share of the reflection the local captures fill (ADR-2860). A capture is the
        // radiance of the walls around it, so the far-field sky visibility (a room's walls
        // occluding the sky) does not darken that share. 0 without captures.
        var capture_share = 0.0;
#ifdef LIGHTMAP
        // Premultiplied local radiance: preserve these shares exactly when the lightmap
        // replaces the remaining environment (ADR-4762).
        var lightmap_local_radiance = vec3<f32>(0.0);
#endif
        // What the ground plane took from the image's irradiance here and what it gives back:
        // the probes trace the whole image, so they take the same change (ADR-2000).
        var env_ground_hidden = vec3<f32>(0.0);
        var env_ground_add = vec3<f32>(0.0);
        var ibl_scale = frame.post_exposure.z;
        var have_ibl = false;
#ifdef BRDF_ENERGY_COMPENSATED
        // How much of the exterior and the interior diffuse environment light has its split-sum
        // specular added beside it (ADR-2632): the diffuse gives up only what a reflection returns.
        var env_cover_ext = 0.0;
        var env_cover_int = 0.0;
#endif
        // The share a pane's traced hit took of the sky reflection (the probe specular keeps it,
        // ADR-1103).
        var pane_share = 0.0;
        // camera_pos.w > 0.5 is an interior. quality.w is the PCSS noise index.
        // Sky IBL would leak outdoor light into a tunnel; the floor is a neutral
        // illuminance, not sky_color, so 40 lux is not the sky's blue.
        // With an interior box, surfaces inside it take the interior shade and the
        // exterior seen through the mouth keeps its sky. Without a box the whole view is
        // interior. Continuity at an open face comes from the probe field (below).
        // Surfaces on the box's faces (a fitted tunnel's walls) are inside: `INTERIOR_FACE_MARGIN_M`.
        let in_box = all(
            abs(in.world_position - frame.interior_center.xyz) <= frame.interior_half.xyz + vec3<f32>(INTERIOR_FACE_MARGIN_M),
        );
        let interior_view = frame.camera_pos.w > 0.5;
        let interior_w = select(0.0, 1.0, interior_view && (frame.interior_center.w < 0.5 || in_box));
        if (frame.extra.z > 0.5 && interior_w < 1.0) {
            // The procedural-sky maps are display-referred (≈0..1); `post_exposure.z` scales
            // them into physical sky radiance (cd/m²) so they survive EV100 exposure.
            ibl_scale = frame.post_exposure.z;
            sky_diffuse = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(gi_n), 0.0).rgb * ibl_scale;
            // The image light's ground plane: the lower half of the image it hides (mip 1 of the
            // irradiance map, `IblLower`) gives way to the plane's own radiance (ADR-2000).
#ifndef STUDIO_IBL_OFF
            if (frame.env_ground_light.w > 0.5) {
                let cover = env_ground_cover(in.world_position, gi_n, frame.env_ground);
                let hidden = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(gi_n), 1.0).rgb * ibl_scale;
                sky_diffuse = max(sky_diffuse - hidden * cover.y, vec3<f32>(0.0)) + frame.env_ground_light.rgb * cover.x;
                env_ground_share.x = cover.x;
                env_ground_hidden = hidden * cover.y;
                env_ground_add = frame.env_ground_light.rgb * cover.x;
            }
#endif
            // The terrain hides part of that sky (a canyon, a mesa beside the mesh): the band
            // under its horizon is taken out of the sky half, as the terrain's own sky term does.
            // What the walls send back instead is `ground_bounce_at` below (the sunlit walls'
            // bounce, the same term the floor gets), so nothing is added here.
            if (ground_bounce[2].w > 0.5) {
                let occ = terrain_sky_occlusion(in.world_position, n, ibl_scale);
                let ground_half = ground_bounce[2].rgb * (0.5 - 0.5 * n.y);
                let sky_half = max(sky_diffuse - ground_half, vec3<f32>(0.0));
                sky_diffuse -= min(occ.rgb, sky_half);
            }
            if (mesh_occ.header.x > 0.5) {
                sky_diffuse = max(sky_diffuse + occ_delta, vec3<f32>(0.0));
            }
            let lod = perceptual * frame.extra.w;
            var pref = textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(refl), lod).rgb * ibl_scale;
#ifndef STUDIO_IBL_OFF
            if (frame.env_ground_light.w > 0.5) {
                env_ground_share.y = env_ground_reflection(in.world_position, refl, perceptual, frame.env_ground);
                pref = mix(pref, frame.env_ground_light.rgb, env_ground_share.y);
            }
#endif
#ifndef STUDIO_IBL_OFF
            if (frame.env_view.z > 0.5) {
                let capture = local_capture_reflection(in.world_position, refl, perceptual);
                capture_share = clamp(capture.a, 0.0, 1.0);
#ifdef LIGHTMAP
                lightmap_local_radiance = capture.rgb * ibl_scale;
#endif
                pref = pref * (1.0 - capture.a) + capture.rgb * ibl_scale;
            }
#endif
            if (ground_bounce[2].w > 0.5 && refl.y > 0.0) {
                // Below the horizon a reflection sees the wall: its bounce, not the sky.
                pref = mix(ground_bounce_at(in.world_position), pref, terrain_reflection_visibility(in.world_position, refl));
            }
            if (mesh_occ.header.x > 0.5) {
                // A reflection that lands on a large mesh's sun shadow sees shadowed ground.
                let shade = mesh_occluder_reflection_shadow(in.world_position, refl, rough);
                pref = max(pref - mesh_occ.ground_sun.rgb * shade, vec3<f32>(0.0));
            }
            // A transmissive pane's traced mirror hit (`pane_hit`, ADR-165) replaces the
            // environment there; a rough smudge keeps half of the environment's blur.
            if (pane_hit.a > 0.0) {
                pane_share = pane_hit.a * (1.0 - 0.5 * smoothstep(0.3, 0.7, perceptual));
                pref = mix(pref, pane_hit.rgb, pane_share);
#ifdef LIGHTMAP
                lightmap_local_radiance = mix(lightmap_local_radiance, pane_hit.rgb, pane_share);
#endif
            }
            env_brdf = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
            spec_ibl = pref * (f0 * env_brdf.x + env_brdf.y);
            have_ibl = true;
#ifdef BRDF_ENERGY_COMPENSATED
            env_cover_ext = 1.0;
#endif
        } else if (interior_w < 1.0) {
            // Flat ambient (no IBL): a fraction of the sky's illuminance in lux,
            // directional-tinted. post_exposure.w carries the scene's ambient.
            sky_diffuse = regia::utils::sky_color(gi_n) * frame.post_exposure.w
                * (1.0 / regia::pbr::lighting::PI);
        }
        // The interior's neutral floor.
        let floor_diffuse = vec3<f32>(frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI));
        // Outdoor probe GI replaces the sky term where the volume covers the
        // receiver: those probes already integrate the sky. Adding both
        // double-counted the sky. An interior probe holds bounce (or, for a daylit
        // bake, the light through the openings), added on top of the floor.
        var exterior_diffuse = sky_diffuse;
        var exterior_spec = spec_ibl;
        var interior_diffuse = floor_diffuse;
        var interior_spec = vec3<f32>(0.0);
        // The share of the sky the probe field says the receiver sees (1 without probes).
        var probe_visibility = 1.0;
        // Uniform branch: quality.y is 0 on the lab baseline.
        if (frame.quality.y > 1.5 && gi_grid.dims.w > 0u) {
            // In an interior view the field covers its volume up to the faces (the
            // outermost probes sit up to a cell inside them) and fades over the next
            // cell: outside an open face the exterior then starts from the probe light
            // (which sees the roof) and eases to the open sky. A field that faded from
            // the outermost probes left the exterior half open sky right at the face,
            // a brightness seam across the floor where the volume ends.
            // Ground and decks run on through a mouth; walls past it are exterior, so the
            // extension follows the surface's up-facing (an extrapolated field on a wall
            // drew bands and wedges).
            let extend = select(0.0, smoothstep(0.3, 0.7, n.y), interior_view);
            let probe_gi = sample_ddgi(in.world_position, gi_n, extend);
            // Inside: the shell sits just outside the inset volume, so confidence is
            // already falling on the walls; the clamped probe value is the bounce those
            // walls should take. No sky reflection inside: a metal reflects the bounce
            // around it, and the probe irradiance (E/π) stands in for the blurred
            // environment a rough metal sees (metals have no diffuse).
            interior_diffuse = floor_diffuse + probe_gi.rgb;
            let env_in = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
            interior_spec = probe_gi.rgb * (f0 * env_in.x + env_in.y);
#ifdef BRDF_ENERGY_COMPENSATED
            // The floor has no reflection of its own; the probe light does.
            env_cover_int = regia::utils::luminance(probe_gi.rgb)
                / max(regia::utils::luminance(interior_diffuse), 1e-6);
#endif
            exterior_diffuse = mix(sky_diffuse, probe_gi.rgb, probe_gi.a);
            // The probe/sky irradiance ratio estimates how much of the sky the
            // receiver sees; it occludes the sky reflection inside enclosures (folded into
            // the specular occlusion below, so it applies once).
            let ratio = clamp(
                regia::utils::luminance(probe_gi.rgb) / max(regia::utils::luminance(sky_diffuse), 1e-4),
                0.0,
                1.0,
            );
            probe_visibility = mix(1.0, ratio, probe_gi.a);
        }
        // GPU irradiance probes (DDGI) replace the sky term and the ground-bounce estimate
        // where their cascades cover the receiver: the probes trace the sky, the terrain
        // and the meshes, so the bounce off sunlit walls is already in them.
        var bounce_weight = 1.0;
        if ((regia::ddgi::ddgi.ctrl.w & 1u) != 0u) {
            // The half-resolution screen lookup (the probes run only with the depth prepass);
            // no per-pixel probe gather here keeps this fragment shader's registers down.
            // With probe specular on (`ctrl.w` 8192, ADR-1103) the same texels also give the
            // probe light in the GGX lobe: its mean vector `spec_lobe` reads the probes' linear
            // light exactly, a mirror in the mirror direction, a rough lobe nearer the normal.
            let probe_spec_on = (regia::ddgi::ddgi.ctrl.w & 8192u) != 0u;
            var probes = vec4<f32>(0.0);
            var probe_lobe = vec3<f32>(0.0);
            var spec_lobe = vec3<f32>(0.0);
            if (probe_spec_on) {
                spec_lobe = regia::ddgi::probe_spec_lobe(n, refl, no_v, rough);
                let screen_gi = regia::ddgi::ddgi_screen_gi(
                    in.clip_position.xy,
                    in.world_position,
                    gi_n,
                    spec_lobe,
                    frame.camera_pos.xyz,
                );
                probes = screen_gi.diffuse;
                probe_lobe = screen_gi.spec;
            } else {
                probes = regia::ddgi::ddgi_screen_irradiance(
                    in.clip_position.xy,
                    in.world_position,
                    gi_n,
                    frame.camera_pos.xyz,
                );
            }
            if (probes.a > 0.0) {
                // The image light's ground plane. The GPU probes hold it themselves: their rays
                // that miss every surface land on it (`ddgi_trace.wgsl` `miss_radiance`, ADR-5716),
                // so nothing is swapped here (the swap counted the plane twice wherever the probes
                // also traced a ground mesh). A baked volume's rays reach the image's lower half,
                // which the plane hides; for its lookup (`ctrl.w` 8) the same form factor as the
                // sky term above swaps it for the plane (zero without a plane, ADR-2000). A frame
                // without a studio feature composes `STUDIO_IBL_OFF` (ADR-2308) and keeps this line:
                // its terms are zero there, and the clamp keeps that frame's pixels those of the
                // full shader. Only the Adreno workaround (`SUN_SHADOW_PCF_ONLY` with
                // `STUDIO_IBL_OFF`) drops it.
#ifndef SUN_SHADOW_PCF_ONLY
                if ((regia::ddgi::ddgi.ctrl.w & 8u) != 0u) {
                    probes = vec4<f32>(max(probes.rgb - env_ground_hidden, vec3<f32>(0.0)) + env_ground_add, probes.a);
                }
#endif
                // A baked volume (`baked_gi.wgsl`) holds the sky-body fill, direct and bounced and
                // shadowed by the bake: it replaces the live (unshadowed) fill where it covers.
                if ((regia::ddgi::ddgi.ctrl.w & 8u) != 0u) {
                    lit = lit - fill_live * probes.a;
#ifdef SUBSURFACE
                    sss_diffuse -= fill_live * probes.a;
#endif
                }
                let ratio = clamp(
                    regia::utils::luminance(probes.rgb) / max(regia::utils::luminance(sky_diffuse), 1e-4),
                    0.0,
                    1.0,
                );
                // A rough lobe reflects nearly the whole hemisphere around the normal (its
                // dominant direction leans from the mirror direction to the normal as the
                // roughness grows: Lagarde & de Rousiers 2014, § 4.9.3): the probes'
                // irradiance / π (what the terrain's own sky reflection takes under the probes)
                // stands in for the blurred sky there, the light the ground and the meshes
                // bounce included. The sky map alone left a grey mesh on a slope facing away
                // from the sun ~5 % under the grey terrain around it (mesh_terrain_light,
                // tilt −25°). That part needs no further sky-share occlusion: the probes carry it.
                var rough_share = 0.0;
                if (probe_spec_on) {
                    // Probe specular at every roughness: the probes set the lobe's level and color
                    // (a reflection in a canyon or a room shows its walls, not the open sky); the
                    // sky prefilter, read along the lobe's direction at the material's mip, lends
                    // only its detail: its ratio to the sky's own light in the same lobe (Lazarov
                    // 2013's normalization), capped. A pane's traced hit keeps its share. Uniform
                    // light comes back unchanged at every roughness.
                    var detail = 1.0;
                    // `ProbeSpecular::LobeHue` (ADR-2996): the ratio per channel, the plane in the lobe.
                    var detail_rgb = vec3<f32>(1.0);
                    if (have_ibl) {
                        let lobe_len = length(spec_lobe);
                        let lobe_dir = spec_lobe / max(lobe_len, 1e-6);
                        let sky_pos = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(lobe_dir), 0.0).rgb;
                        let sky_neg = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(-lobe_dir), 0.0).rgb;
                        let sky_lobe = regia::ddgi::probe_spec_axis_lobe(sky_pos, sky_neg, lobe_len);
                        let sky_pref = textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(lobe_dir), perceptual * frame.extra.w).rgb;
                        detail = regia::ddgi::probe_spec_detail(
                            regia::utils::luminance(sky_pref),
                            regia::utils::luminance(sky_lobe),
                        );
#ifndef SUN_SHADOW_PCF_ONLY
                        if (regia::ddgi::probe_spec_hue_on()) {
                            var hue_sky_lobe = regia::ddgi::probe_spec_hue_axis_lobe(sky_pos, sky_neg, lobe_len) * ibl_scale;
                            var hue_sky_pref = sky_pref * ibl_scale;
#ifndef STUDIO_IBL_OFF
                            if (frame.env_ground_light.w > 0.5) {
                                // The plane in the probe lobe, in the L1 form of the lobe itself: what it
                                // hides and what it sends toward the lobe's direction and away from it.
                                // The probes' lobe takes the swap only for a baked volume's lookup (`ctrl.w`
                                // 8), like the diffuse swap: traced probes hold the plane themselves (ADR-5716
                                // (7)), and a second swap counted it twice (ADR-9232). The sky's lobe always
                                // takes it: the image under it still has its lower half.
                                let cover_pos = env_ground_cover(in.world_position, lobe_dir, frame.env_ground);
                                let cover_neg = env_ground_cover(in.world_position, -lobe_dir, frame.env_ground);
                                let lower_pos = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(lobe_dir), 1.0).rgb * ibl_scale;
                                let lower_neg = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(-lobe_dir), 1.0).rgb * ibl_scale;
                                let hidden = regia::ddgi::probe_spec_axis_lobe(lower_pos * cover_pos.y, lower_neg * cover_neg.y, lobe_len);
                                let add = frame.env_ground_light.rgb * regia::ddgi::probe_spec_axis_lobe(vec3<f32>(cover_pos.x), vec3<f32>(cover_neg.x), lobe_len);
                                if ((regia::ddgi::ddgi.ctrl.w & 8u) != 0u) {
                                    probe_lobe = regia::ddgi::probe_spec_ground_swap(probe_lobe, hidden, add);
                                }
                                hue_sky_lobe = regia::ddgi::probe_spec_ground_swap(hue_sky_lobe, hidden, add);
                                hue_sky_pref = mix(hue_sky_pref, frame.env_ground_light.rgb, env_ground_reflection(in.world_position, lobe_dir, perceptual, frame.env_ground));
                            }
#endif
                            detail_rgb = mix(
                                vec3<f32>(1.0),
                                regia::ddgi::probe_spec_detail_rgb(
                                    hue_sky_pref,
                                    hue_sky_lobe,
                                    probe_lobe,
                                    regia::ddgi::probe_spec_detail(
                                        regia::utils::luminance(hue_sky_pref),
                                        regia::utils::luminance(hue_sky_lobe),
                                    ),
                                ),
                                regia::ddgi::probe_spec_lobe_lerp(no_v, rough),
                            );
                            detail = 1.0;
                        }
#endif
                        // The ratio is detail only while the lobe has a direction of its own. Toward
                        // α = 1 the lobe's mean leans back onto the normal and the prefilter's top mip
                        // (an N = V = R lobe, the lower hemisphere dropped) weights the zenith far more
                        // than the wide lobe does: under a sky brighter at the horizon (Mars) the ratio
                        // read well under 1 and a rough mesh printed 5 % under the grey terrain around
                        // it (mesh_terrain_light). Faded to 1 by the lobe's lean toward the mirror: full
                        // detail on a mirror, exactly 1 at α = 1, as ADR-1103 meant (ADR-1387).
                        detail = mix(1.0, detail, regia::ddgi::probe_spec_lobe_lerp(no_v, rough));
                    } else {
                        env_brdf = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
                    }
                    // A local capture's share keeps the capture too (ADR-2860): it is the room's
                    // own radiance at this roughness, sharper than the probes' lobe (× 1 without one).
                    rough_share = probes.a * (1.0 - pane_share) * (1.0 - capture_share);
                    exterior_spec = mix(exterior_spec, probe_lobe * detail * detail_rgb * (f0 * env_brdf.x + env_brdf.y), rough_share);
#ifdef BRDF_ENERGY_COMPENSATED
                    env_cover_ext = mix(env_cover_ext, 1.0, rough_share);
#endif
                } else if (have_ibl) {
                    rough_share = probes.a * smoothstep(0.5, 1.0, perceptual) * (1.0 - capture_share);
                    exterior_spec = mix(exterior_spec, probes.rgb * (f0 * env_brdf.x + env_brdf.y), rough_share);
                }
                // The baked volume holds the light of the emitters it traced, seen directly and
                // bounced (ADR-1386): where it covers, it replaces their fitted area lights'
                // diffuse, and their specular as far as the probes stand in for the rough lobe.
                // Elsewhere (and for every emitter the bake did not trace) the area light stays.
                // The interior shade takes the baked volume too (ADR-7975, below), so inside the
                // interior box the emitters' diffuse comes out where it covers as it does outside;
                // the specular keeps the exterior's share, the interior having no other lobe for it.
                if ((regia::ddgi::ddgi.ctrl.w & 16u) != 0u) {
                    // One sum, as before ADR-7975: outside the box (`interior_w` 0) the same operations.
                    lit = lit - (baked_emit_diffuse * probes.a + baked_emit_spec * rough_share * (1.0 - interior_w));
#ifdef SUBSURFACE
                    sss_diffuse -= baked_emit_diffuse * probes.a;
#endif
                }
                // The probes' sky share joins the specular occlusion below (applied once).
                probe_visibility = min(probe_visibility, mix(mix(1.0, ratio, probes.a), 1.0, rough_share));
                exterior_diffuse = mix(exterior_diffuse, probes.rgb, probes.a);
                bounce_weight = 1.0 - probes.a;
                // A baked volume lights the interior shade too (ADR-7975). Its screen uniform has no
                // cascades (`dims.w` 0: `DdgiUniformGpu::screen_only`); live probes always have one.
                // Where the bake covers the receiver its irradiance replaces the interior's neutral
                // floor and the legacy probe bounce: the bake traced the tunnel's own walls and
                // openings, so a lit tunnel shows its baked light, not a flat floor. Where it does not
                // cover (and with live probes) the interior shade is as before.
                if (regia::ddgi::ddgi.dims.w == 0u) {
                    interior_diffuse = mix(interior_diffuse, probes.rgb, probes.a);
                    // As the legacy probe bounce: the probe light in the reflection, no sky.
                    let env_baked = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
                    let baked_spec = select(probes.rgb, probe_lobe, probe_spec_on) * (f0 * env_baked.x + env_baked.y);
                    interior_spec = mix(interior_spec, baked_spec, probes.a);
#ifdef BRDF_ENERGY_COMPENSATED
                    env_cover_int = mix(env_cover_int, 1.0, probes.a);
#endif
                }
            }
        }
        // Outdoors, plus the light the terrain bounces onto the ground here (a rock on a
        // shadowed trench floor gets what the floor gets from the sunlit walls).
        // Under glass the sky and the ground reach the receiver through it (`glass_enclosure`).
        var glass_enc = vec3<f32>(1.0);
        if (frame.extra.x > 0.5 && csm.count.y > 0.5) {
            glass_enc = glass_enclosure(in.world_position, shadow_n, l);
        }
        let probe_diffuse_indirect = (mix(exterior_diffuse, interior_diffuse, interior_w)
            + ground_bounce_at(in.world_position) * (1.0 - interior_w) * bounce_weight) * glass_enc * vertex_ao;
#ifdef LIGHTMAP
        // A lightmap receiver (ADR-1623): the baked irradiance (lux, bent by the shading normal
        // when the lightmap has directions) replaces the probe / sky diffuse term, which is E/π
        // here. The bake already holds the receiver's sky, bounce and occlusion, so the vertex AO
        // and the glass enclosure do not apply again; only the material AO applies to the bake.
        // One lookup serves the diffuse term and the specular below (ADR-4762).
        let lightmap_receiver = lightmap_is_receiver(in.lightmap_scale_offset);
        // Keep mutable locals scalar/vector: a Function-storage struct gets explicit layout
        // decorations in Naga's SPIR-V output (VUID-StandaloneSpirv-None-10684).
        var lightmap_e = vec3<f32>(0.0);
        var lightmap_direction_sample = vec4<f32>(0.0);
        if (lightmap_receiver) {
            let lightmap_texel = lightmap_sample(in.lightmap_uv, in.lightmap_scale_offset);
            lightmap_e = lightmap_texel.e;
            lightmap_direction_sample = lightmap_texel.direction;
        }
        // The bake cannot hold what moves: a receiver takes the capsule occluders' visibility, which
        // the probe light (a non-receiver's `fallback`) holds already (ADR-2656). 1 without capsules.
        var lightmap_capsules = 1.0;
        if (lightmap_enabled() && in.lightmap_scale_offset.x > 0.0 && in.lightmap_scale_offset.y > 0.0) {
            lightmap_capsules = regia::ddgi::ddgi_screen_occluder_visibility(
                in.clip_position.xy,
                in.world_position,
                frame.camera_pos.xyz,
            );
        }
        var diffuse_indirect = select(
            probe_diffuse_indirect,
            lightmap_shade_directional(lightmap_e, lightmap_direction_sample, n) * lightmap_capsules * (1.0 / regia::pbr::lighting::PI),
            lightmap_receiver,
        );
#else
        let diffuse_indirect = probe_diffuse_indirect;
#endif
        spec_ibl = mix(exterior_spec, interior_spec, interior_w) * glass_enc * vertex_ao;
#ifdef LIGHTMAP
        // A receiver's indirect specular from the same baked light (ADR-4762): the lightmap's
        // radiance field (`lightmap_lobe_radiance`) read at the GGX lobe's mean vector, the same
        // lobe the probe specular reads (ADR-1103), times the split-sum scale and bias. It
        // replaces the sky, probe and interior specular as the lightmap replaced their diffuse;
        // a local capture's share and a pane's traced hit keep theirs (both see the room's own
        // radiance, sharper). The bake already holds what the walls hide, so that share takes
        // no far-field sky visibility (below), and the screen-space reflection replaces it like
        // any environment specular (`ssr_env`).
        var lightmap_indirect_spec = vec3<f32>(0.0);
        var lightmap_multi_scatter = vec3<f32>(0.0);
        if (lightmap_specular_on(in.lightmap_scale_offset)) {
            let lm_brdf = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
            let lm_lobe = regia::ddgi::probe_spec_lobe(n, refl, no_v, rough);
            let lm_single_diffuse_weight = lightmap_diffuse_weight(f0, lm_brdf);
            let lm_weight = vec3<f32>(1.0) - lm_single_diffuse_weight;
            var lm_diffuse_weight = lm_single_diffuse_weight;
            if (material.response.y > 0.5) {
                let lm_ms_weight = clamp(
                    regia::pbr::lighting::multi_scatter_ems(f0, lm_brdf),
                    vec3<f32>(0.0), lm_single_diffuse_weight,
                );
                // Reserve the broad multi-scatter share before partitioning diffuse.
                // Its incident light is the bake even when image lighting is disabled.
                lightmap_multi_scatter = diffuse_indirect * lm_ms_weight;
                lm_diffuse_weight -= lm_ms_weight;
            }
            let lm_spec = lightmap_lobe_radiance(lightmap_e, lightmap_direction_sample, lm_lobe)
                * lm_weight;
            let lightmap_spec_share = (1.0 - pane_share) * (1.0 - capture_share);
            // Keep the sources separate until each has taken its own occlusion. Multiplying
            // their blended radiance by blended visibility introduces cross terms at partial
            // capture coverage (ADR-4762).
            lightmap_indirect_spec = lm_spec * lightmap_spec_share + lightmap_multi_scatter;
            spec_ibl = lightmap_local_radiance * lm_weight;
            diffuse_indirect *= lm_diffuse_weight;
        }
#endif
        // Far-field visibility of the sky in the specular lobe: the screen-space reflection
        // visibility (G glossy, B rough; opaque and masked surfaces only: a blended surface or
        // glass lies in front of the depth the kernel saw, and takes `pane_visibility`, traced
        // by `fs_transmit_add` when the refraction copy binds the depth) and the probe field's
        // sky share. An interior already reflects its probe light, not the sky.
        var lobe_visibility = pane_visibility;
#ifndef GLASS
        if (material.alpha.x < 1.5) {
            let traced = mix(ao_sample.g, ao_sample.b, smoothstep(0.15, 0.5, perceptual));
            // A sunlit surface whose mirror ray dips below the horizon is outdoors looking at the
            // ground, which the environment's lower hemisphere already holds (a hull side over
            // sunlit sand): keep it. In shadow (a cockpit inside its hull) the block stands.
            lobe_visibility = mix(traced, 1.0, sun_visibility * smoothstep(0.0, -0.2, refl.y));
        }
#endif
        // The rays cover the mirror direction and a 30° cone. A lobe much wider than that (a
        // rough ground under a vehicle) is mostly outside them: hand it to the AO form alone.
        lobe_visibility = mix(lobe_visibility, 1.0, smoothstep(0.6, 1.0, perceptual));
        // Exact (× 1, + 0) without a capture.
        let sky_visibility = mix(mix(min(lobe_visibility, probe_visibility), 1.0, interior_w), 1.0, capture_share);
        // Clearcoat attenuates the base lobes. The coat is its own specular lobe
        // (sun above, and this IBL/probe lobe). Multi-scatter restores the energy
        // the single-scatter GGX drops on rough metals, lit by the irradiance that
        // actually reaches the surface after probe GI replaces the sky.
        var base_coat = 1.0;
        if (coat_w > 1e-4) {
            base_coat = regia::pbr::lighting::clearcoat_attenuation(no_v, coat_w);
        }
        var indirect_spec = spec_ibl;
        if (material.response.y > 0.5) {
#ifdef LIGHTMAP
            if (!lightmap_specular_on(in.lightmap_scale_offset) && have_ibl) {
                indirect_spec += diffuse_indirect * regia::pbr::lighting::multi_scatter_ems(f0, env_brdf);
            }
#else
            if (have_ibl) {
                indirect_spec += diffuse_indirect * regia::pbr::lighting::multi_scatter_ems(f0, env_brdf);
            }
#endif
        }
        if (coat_w > 1e-4) {
            indirect_spec = indirect_spec * base_coat;
            if (have_ibl) {
                let coat_rough = regia::utils::saturate(coat_r);
                var pref_c = textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(refl), coat_rough * frame.extra.w).rgb * ibl_scale;
#ifndef STUDIO_IBL_OFF
                if (frame.env_ground_light.w > 0.5) {
                    pref_c = mix(pref_c, frame.env_ground_light.rgb, env_ground_reflection(in.world_position, refl, coat_rough, frame.env_ground));
                }
#endif
                let env_c = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, coat_rough), 0.0).rg;
                let coat_spec = pref_c * (vec3<f32>(0.04) * env_c.x + vec3<f32>(env_c.y));
                indirect_spec = indirect_spec + coat_spec * coat_w;
            }
        }
        // Specular occlusion, applied once to the indirect specular (base, multi-scatter and
        // the coat lobe). Each estimate sees only some occluders, so the visible share is at
        // most each of them: the smallest of the AO form (contact scale; the always-on α form,
        // Lagarde & de Rousiers 2014, as Filament evaluates it, or, when the material authors
        // it, the stronger perceptual-roughness form in its place: applying both squared the
        // occlusion) and the far-field `sky_visibility` (a cockpit's or a room's walls).
        let contact_occlusion = select(
            specular_occlusion(no_v, ao, rough),
            regia::pbr::lighting::specular_occlusion(no_v, ao, perceptual),
            material.response.x > 0.5,
        );
        var ao_occlusion = contact_occlusion;
        if (bent_on) {
            // With bent normals the SSAO's share (it has a direction: the bent normal) takes the
            // cone form (`cone_specular_occlusion`); the material's occlusion texture (cavities, no
            // direction) keeps its AO form, the authored one included. Different scales, so the two
            // multiply, as the AO itself does (`ao = occlusion × SSAO`).
            let material_occlusion = select(
                specular_occlusion(no_v, occlusion, rough),
                regia::pbr::lighting::specular_occlusion(no_v, occlusion, perceptual),
                material.response.x > 0.5,
            );
            ao_occlusion = material_occlusion * cone_specular_occlusion(gi_n, ao_sample.r, n, refl, rough);
        }
        let spec_occlusion = min(ao_occlusion, sky_visibility);
        // What the enclosure blocks beyond the contact-scale occlusion (the AO form, which stays
        // dark, as before) reflects the enclosure instead of the sky: its walls taken as a mid-grey
        // (0.18) diffuse surface under the indirect light that reaches this one. The cone form
        // blocks by direction (a reflection into a wall at its foot): that part reflects the wall
        // too, not black.
        var enclosure = 0.18 * diffuse_indirect * ao * (f0 * env_brdf.x + env_brdf.y) * base_coat
            * max(contact_occlusion - spec_occlusion, 0.0);
#ifdef LIGHTMAP
        // A lightmap receiver's diffuse term holds the bake's occlusion already (the walls,
        // the floor's contact and the room's creases at texel scale): the screen-space AO on top
        // darkened them twice (ADR-2635). Its diffuse term keeps the material's occlusion only, as
        // the lite shader's `LIGHTMAP` branch; other specular sources keep the SSAO forms. Non-receivers in
        // a lightmapped batch (zero scale) and the flag off keep `ao`.
        let lightmap_ao = select(
            ao,
            occlusion,
            lightmap_enabled() && in.lightmap_scale_offset.x > 0.0 && in.lightmap_scale_offset.y > 0.0,
        );
        let ao_diffuse = multi_bounce_ao(lightmap_ao, diffuse_color + f0 * metallic);
#else
        let ao_diffuse = multi_bounce_ao(ao, diffuse_color + f0 * metallic);
#endif
#ifdef BRDF_ENERGY_COMPENSATED
        // The multiple scattering is added (above) only beside the image light.
        let env_kd = regia::pbr::brdf::compensated_env_diffuse_covered(
            f0,
            dfg_o.xy,
            mix(env_cover_ext, env_cover_int, interior_w),
            select(0.0, 1.0, ms_on && have_ibl),
        );
#endif
#ifdef GLASS
        indirect_spec *= glass_spec;
#endif
        var occluded_indirect_spec = indirect_spec * spec_occlusion;
#ifdef LIGHTMAP
        if (lightmap_specular_on(in.lightmap_scale_offset)) {
            // The bake carries screen/far-field visibility already; material cavities still
            // apply. Captures, panes and the coat above keep their original visibility. The
            // bake also carries the enclosure bounce, so do not synthesize a second one.
            let lm_occlusion = select(
                specular_occlusion(no_v, occlusion, rough),
                regia::pbr::lighting::specular_occlusion(no_v, occlusion, perceptual),
                material.response.x > 0.5,
            );
            var lm_occluded_spec = lightmap_indirect_spec * (lm_occlusion * base_coat);
#ifdef GLASS
            lm_occluded_spec *= glass_spec;
#endif
            occluded_indirect_spec += lm_occluded_spec;
            enclosure = vec3<f32>(0.0);
        }
#endif
        if (sheen_on) {
            // The base layer under the sheen, plus the sheen lobe lit by the irradiance (a lobe
            // this wide sees nearly the whole hemisphere).
            let sheen_e = regia::pbr::shading_models::sheen_albedo(no_v, material.sheen.w);
#ifdef BRDF_ENERGY_COMPENSATED
            // The diffuse environment under the specular layer's share (ADR-2632).
            lit += (diffuse_color * env_kd * diffuse_indirect * ao_diffuse + occluded_indirect_spec + enclosure) * sheen_base_v
                + material.sheen.rgb * sheen_e * diffuse_indirect * ao;
#ifdef SUBSURFACE
            sss_diffuse += diffuse_color * env_kd * diffuse_indirect * ao_diffuse * sheen_base_v;
#endif
#else
            lit += (diffuse_color * diffuse_indirect * ao_diffuse + occluded_indirect_spec + enclosure) * sheen_base_v
                + material.sheen.rgb * sheen_e * diffuse_indirect * ao;
#ifdef SUBSURFACE
            sss_diffuse += diffuse_color * diffuse_indirect * ao_diffuse * sheen_base_v;
#endif
#endif
        } else {
#ifdef BRDF_ENERGY_COMPENSATED
            lit += diffuse_color * env_kd * diffuse_indirect * ao_diffuse + occluded_indirect_spec + enclosure;
#ifdef SUBSURFACE
            sss_diffuse += diffuse_color * env_kd * diffuse_indirect * ao_diffuse;
#endif
#else
            lit += diffuse_color * diffuse_indirect * ao_diffuse + occluded_indirect_spec + enclosure;
#ifdef SUBSURFACE
            sss_diffuse += diffuse_color * diffuse_indirect * ao_diffuse;
#endif
#endif
        }
        ssr_env = occluded_indirect_spec + enclosure;
        // Image-light debug views (`DebugViewMode::ImageLight`, `ImageLightOcclusion`): this light
        // alone, or what occludes it, r = diffuse visibility, g = specular visibility, b = the
        // ground plane's share (over the exposure: the frame's exposure brings them back to 0..1).
#ifndef STUDIO_IBL_OFF
        if (frame.env_view.x > 0.5) {
            if (frame.env_view.x < 1.5) {
                lit = diffuse_color * diffuse_indirect * ao_diffuse + ssr_env;
            } else {
                lit = vec3<f32>(
                    regia::utils::luminance(ao_diffuse) * (1.0 - env_ground_share.x),
                    spec_occlusion * (1.0 - env_ground_share.y),
                    env_ground_share.x,
                ) / max(frame.post_exposure.y, 1e-8);
            }
        }
#endif
        if (shading_model == 2u) {
            lit *= eye_occlusion;
        }
    } else {
        // Unlit (glTF KHR_materials_unlit): base colour is linear. No lighting,
        // no exposure, no tonemap. A linear framebuffer sRGB-encodes it here;
        // `skip_srgb_encode` targets an sRGB framebuffer, where the hardware encodes.
        // A render override's coverage (ghost) applies after the alpha test above.
        let unlit_alpha = alpha * override_tint(in.overrides).a;
        #ifdef skip_srgb_encode
        return vec4<f32>(base, unlit_alpha);
        #else
        return vec4<f32>(regia::utils::encode_srgb(base), unlit_alpha);
        #endif
    }

    // Scene-referred colour: lighting plus emissive, both in photometric units (lux / nits).
    // Exposure (EV100) is applied exactly once — here when the post stack is off, or in the
    // post exposure pass when it is on (M13 terrain lane convention, `specs/13` § 6).
    var scene_color = lit + emissive;
#ifndef STUDIO_IBL_OFF
    if (frame.env_view.x > 0.5) {
        scene_color = lit;
    }
#endif
    // A render override's emissive is display-referred: divided by the exposure the frame
    // applies (in the post stack or below), so it reads the same in any scene.
    let override_glow = override_emissive(in.overrides);
    if any(override_glow > vec3<f32>(0.0)) {
        let exposure = select(frame.post_exposure.y, frame.post_exposure.x, frame.post_exposure.x > 0.0);
        scene_color += override_glow / max(exposure, 1e-8);
    }
#ifdef SUBSURFACE
    let sss_total = scene_color;
#endif
    // `FogSettings::color` is linear display-referred. Divide by the exposure multiplier
    // so the haze is a radiance in the same domain as `scene_color`; the exposure below
    // (or in the post stack) brings it back. Mixing the 0..1 colour in directly quantizes
    // to black under EV100 ~14.
    if (frame.fog_color.a > 0.5) {
        // Aerial perspective (the default): in-scatter the horizon sky radiance, so
        // distance recedes into the sky the frame shows. `color` only tints it.
        let tint = frame.fog_color.rgb / max(regia::utils::luminance(frame.fog_color.rgb), 1e-3);
        if (haze_under_clouds()) {
            // The haze between the eye and this surface is under the cloud layer too (ADR-5944).
            scene_color = regia::aerial::apply_aerial_under_clouds(
                scene_color,
                in.world_position,
                frame.camera_pos.xyz,
                frame.fog_params,
                tint,
                -normalize(dir_light.direction),
                dir_light.color * dir_light.illuminance,
                frame.post_exposure.z,
                haze_sun_lit(in.world_position),
            );
        } else {
            scene_color = regia::aerial::apply_aerial(
                scene_color,
                in.world_position,
                frame.camera_pos.xyz,
                frame.fog_params,
                tint,
                -normalize(dir_light.direction),
                dir_light.color * dir_light.illuminance,
                frame.post_exposure.z,
            );
        }
    } else {
        let exposure = max(frame.post_exposure.y, 1e-8);
        let fog_radiance = vec4<f32>(frame.fog_color.rgb / exposure, frame.fog_color.a);
        scene_color = regia::fog::apply_fog(
            scene_color,
            in.world_position,
            frame.camera_pos.xyz,
            frame.fog_params,
            fog_radiance,
        );
    }
    // The falling rain or snow's own constant extinction (spec 99 § 3.2), with the fog's color.
    if (frame.weather2.w > 0.0) {
        scene_color = regia::forward::surface_weather::weather_extinction(
            scene_color,
            length(in.world_position - frame.camera_pos.xyz),
            frame.weather2.w,
            weather_inscatter_at(normalize(in.world_position - frame.camera_pos.xyz), in.world_position),
        );
    }
    // Debug view modes (ADR-7211, `lights_views::shading_view_code`): a display-referred colour,
    // divided by the exposure the frame applies (here or in the post stack) so it reads the same in
    // any scene. Lit frames (code 0) skip it.
    let view_code = u32(frame.view_mode.x + 0.5);
    if (view_code != 0u) {
        let view_exposure = max(select(frame.post_exposure.y, frame.post_exposure.x, frame.post_exposure.x > 0.0), 1e-8);
#ifdef TEXTURE_TABLE
        // The base-colour slot's own table entry (ADR-7254): its real edge, not an assumed one.
        let view_tex_size = f32(max(textureDimensions(texture_table[table_index(0u)]).x, 1u));
#else
        let view_tex_size = f32(max(textureDimensions(albedo_tex).x, 1u));
#endif
        scene_color = debug_view_color(view_code, in.world_position, in.clip_position.xy, base, n, perceptual, metallic, occlusion, emissive * view_exposure, view_uv_footprint, view_tex_size) / view_exposure;
    }
#ifdef SUBSURFACE
    // The share that scatters (`shading_c.yzw`, the material's tint) leaves the colour target with
    // what the media did to it. The image-light debug views keep their whole term.
    var sss_scatter = vec3<f32>(0.0);
    if (frame.env_view.x < 0.5 && frame.post_exposure.x > 0.0) {
        let scatters = max(sss_diffuse, vec3<f32>(0.0)) * material.shading_c.yzw;
        let rest = subsurface_media(max(sss_total - scatters, vec3<f32>(0.0)), in.world_position);
        sss_scatter = regia::utils::finite_or_zero(max(scene_color - rest, vec3<f32>(0.0)));
        scene_color = rest;
    }
#endif
    // Blended materials (`alpha.x` ≥ 2) output their coverage. A transmissive surface lets
    // through what it neither reflects (Schlick Fresnel at this view) nor absorbs: its
    // colour (reflection + the remaining diffuse) is already premultiplied, and the frame
    // behind keeps `transmission · (1 − F)`.
    // A render override's coverage (ghost) applies after the alpha test above.
    var out_alpha = alpha * override_tint(in.overrides).a;
#ifdef MATERIAL_GRAPH
    // A transmissive graph draw has already multiplied what is behind by its tinted
    // transmittance (`fs_transmit`, graph set `transmit_mul`) and adds this light with the
    // additive pipeline: alpha 0 leaves the target's alpha as it was (ADR-347). The old single
    // premultiplied blend dimmed the view by the base colour's luminance and lost its hue.
    if (transmission > 0.0) {
        out_alpha = 0.0;
    }
    let blended = material.alpha.x > 1.5 || (in.overrides.y & 9u) == 9u || transmission > 0.0;
#else
    if (transmission > 0.0) {
        // glTF transmission is tinted by the base colour (a grey estimate here: the blend
        // cannot tint what is behind), so tinted glass darkens the view through it.
        let fv = f0_dielectric + (1.0 - f0_dielectric) * pow(1.0 - no_v, 5.0);
        let tint = regia::utils::saturate(regia::utils::luminance(base));
        out_alpha = 1.0 - transmission * tint * (1.0 - fv);
    }
    // A ghost or x-ray override draws an opaque material through the blend pass too.
    let blended = material.alpha.x > 1.5 || (in.overrides.y & 9u) == 9u;
#endif
    if (frame.post_exposure.x > 0.0) {
        // Alpha carries perceptual roughness for the SSR composite (opaque draws). The post
        // stack writes alpha 1, so the swapchain image is unchanged. A blended draw must
        // output its coverage: the blend state reads it. Radiance is pre-exposed.
        // `interior_half.w` bit 1: stochastic SSR runs and reads a packed surface code instead
        // (sssr_common.wgsl `decode_alpha`).
        var ssr_alpha = perceptual;
        if ((u32(frame.interior_half.w + 0.5) & 2u) != 0u) {
            let dfg_r = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
            let ssr_weight = regia::utils::luminance(f0 * dfg_r.x + vec3<f32>(dfg_r.y));
            let ssr_share = regia::utils::luminance(ssr_env) / max(regia::utils::luminance(lit + emissive), 1e-6);
            ssr_alpha = ssr_pack(perceptual, ssr_weight, ssr_share);
        }
#ifndef MATERIAL_GRAPH
        // Fitted area lights carry this emitter's light (`interior_half.w` bit 2): mark the pixel
        // so the screen-space GI does not gather it a second time.
        if ((u32(frame.interior_half.w + 0.5) & 4u) != 0u
            && regia::utils::luminance(emissive) > 0.5 * regia::utils::luminance(lit + emissive)) {
            ssr_alpha = EMITTER_ALPHA;
        }
#endif
        // Clamped below the half-float maximum: a polished surface's sun highlight at
        // `MIN_ALPHA` must not turn into inf in the Rgba16Float target. A NaN is zeroed by bit
        // test first: `max` does not remove it on every driver (RADV), and TAAU kept it.
        let finite = regia::utils::finite_or_zero(scene_color);
#ifdef SUBSURFACE
        // The albedo goes back on after the blur.
        let albedo_kept = clamp(diffuse_color, vec3<f32>(1e-5), vec3<f32>(1.0));
        let light = sss_scatter * frame.post_exposure.x / albedo_kept;
        subsurface_light = vec4<f32>(min(light, vec3<f32>(6.0e4)), 1.0);
        subsurface_albedo = vec4<f32>(albedo_kept, select(material.shading_c.x, -material.shading_c.x, material.shading.z > 1.5));
#endif
        return vec4<f32>(min(max(finite, vec3<f32>(0.0)) * frame.post_exposure.x, vec3<f32>(6.0e4)), select(ssr_alpha, out_alpha, blended));
    }
    // Clamp so the tonemap stays total (a NaN or inf through the log encode turns into
    // black — same class of bug as the M13 terrain shading fix). AgX saturates at +4 EV.
    let exposed = clamp(scene_color * frame.post_exposure.y, vec3<f32>(0.0), vec3<f32>(1.0e4));
    // AgX + the default look; the result is already display-encoded.
    var color = regia::utils::default_display_transform(exposed);
    // `skip_srgb_encode`: the target is an sRGB framebuffer — the hardware encodes, so
    // hand it linear values.
    #ifdef skip_srgb_encode
    color = regia::utils::decode_srgb(color);
    #endif
    return vec4<f32>(color, out_alpha);
}

#ifndef VISBUFFER
@fragment
fn fs_main(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    return shade_forward(in, front_facing);
}

#ifdef SUBSURFACE
struct SubsurfaceTargets {
    @location(0) color: vec4<f32>,
    @location(1) light: vec4<f32>,
    @location(2) albedo: vec4<f32>,
}

@fragment
fn fs_subsurface(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> SubsurfaceTargets {
    let color = shade_forward(in, front_facing);
    return SubsurfaceTargets(color, subsurface_light, subsurface_albedo);
}
#endif

#ifdef OIT
// Weighted blended order-independent transparency (`crate::oit`, ADR-2065; McGuire and Bavoil, JCGT
// 2013). This variant draws translucent materials into two targets instead of the image: their
// premultiplied light and coverage scaled by a weight, and what they let through.
// `shaders/oit_composite.wgsl` lays the result over the image. `crate::oit::OitFragment` is the same
// arithmetic.
struct OitTargets {
    @location(0) accum: vec4<f32>,
    @location(1) revealage: vec4<f32>,
}

// Equation 7 of the paper, on the radial distance.
fn oit_depth_weight(distance: f32) -> f32 {
    let near = distance / 5.0;
    let far = distance / 200.0;
    let far2 = far * far;
    return clamp(10.0 / (1.0e-5 + near * near + far2 * far2 * far2), 1.0e-2, 3.0e3);
}

@fragment
fn fs_oit(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> OitTargets {
    let shaded = shade_forward(in, front_facing);
    let premultiplied = u32(material.alpha.x + 0.5) == 3u;
    let alpha = clamp(shaded.a, 0.0, 1.0);
    let light = max(shaded.rgb, vec3<f32>(0.0)) * select(alpha, 1.0, premultiplied);
    let peak = max(light.r, max(light.g, light.b));
    // A premultiplied texel with alpha 0 only adds light: it keeps a sliver of coverage for the
    // composite to lay that light over the image with.
    let coverage = select(alpha, max(alpha, min(peak, 1.0 / 255.0)), premultiplied);
    let distance = length(in.world_position - frame.camera_pos.xyz);
    // Held down for a bright fragment: the target is a half float, and ten of them must fit.
    // And held up for a far, faint one: a half-float denormal would leave its revealage without its
    // color on hardware that flushes those while blending.
    let weight = min(
        max(max(coverage, 1.0e-2) * oit_depth_weight(distance), 1.0e-4 / max(coverage, 1.0e-6)),
        6.0e3 / max(peak, 1.0e-5),
    );
    return OitTargets(vec4<f32>(light * weight, coverage * weight), vec4<f32>(coverage, 0.0, 0.0, 0.0));
}
#endif

// Whether a point is hidden behind the opaque depth (`refract_depth`) by a surface within
// `reach` in front of it: 1 hidden, 0 visible, 2 against the sky, -1 unknown (off screen,
// behind the eye). The
// test of `ssao.wgsl`'s `hidden` (SSDO, Ritschel et al. 2009), in world space.
fn pane_hidden(s: vec3<f32>, reach: f32) -> f32 {
    let c = frame.view_proj * vec4<f32>(s, 1.0);
    if (c.w <= frame.params.x) {
        return -1.0;
    }
    let ndc = c.xyz / c.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (any(uv <= vec2<f32>(0.0)) || any(uv >= vec2<f32>(1.0))) {
        return -1.0;
    }
    let dims = vec2<f32>(textureDimensions(refract_depth));
    let d = textureLoad(refract_depth, vec2<i32>(uv * dims), 0);
    if (d >= 1.0) {
        return 2.0;
    }
    let n = frame.params.x;
    let f = frame.params.y;
    let scene_z = n * f / max(f - d * (f - n), 1e-6);
    let behind = c.w - scene_z;
    return select(0.0, 1.0, behind > 0.03 + 0.01 * c.w && behind < reach);
}

// The mirror-direction reflection visibility of a blended or transmissive pane (glass lies in
// front of the depth the SSAO pass traced): `ssao.wgsl`'s `ray_open` along the pane's own mirror
// ray (geometric normal, flipped on a back face), against the opaque depth. The far pane of a
// canopy seen through the near one reflects the cockpit below it, not the sky. Since ADR-165 only
// the panes behind the nearest one run it (they show through it at its transmittance); the
// nearest pane traces its reflection's colour (`pane_reflection_trace`).
fn pane_reflection_visibility(in: VertexOutput, front_facing: bool) -> f32 {
#ifdef FORWARD_TRIM
    // Adreno (ADR-3145): a pane behind the nearest one keeps its environment reflection.
    return 1.0;
#else
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
    let v = normalize(frame.camera_pos.xyz - in.world_position);
    var r = reflect(-v, n);
    r = normalize(r + n * max(0.1 - dot(r, n), 0.0));
    for (var i = 0; i < 5; i = i + 1) {
        let u = (f32(i) + 1.0) / 5.0;
        let t = 0.1 + 7.9 * u * u;
        let h = pane_hidden(in.world_position + r * t, 2.0 * t + 0.5);
        if (h < 0.0 || h > 1.5) {
            return 1.0;
        }
        if (h > 0.5) {
            return 0.0;
        }
    }
    return 1.0;
#endif
}

// The mirror reflection of a transmissive pane from the screen (ADR-165): the pane lies in front
// of the depth the SSAO and SSR passes saw, so it traces its own mirror ray (geometric normal,
// flipped on a back face) through the refraction copy's opaque depth. A hit takes that pixel's
// colour from the copy, so a canopy mirrors the hull beside it, the cockpit under its far pane and
// the ground at grazing angles. Before ADR-165 a hit only zeroed the sky reflection (a canopy over
// Europa's ice turned black, a Mars canopy showed a featureless sky over its cockpit). A ray that
// goes deep behind a nearby surface is still blocked (`pane_visibility` = 0, the old test); a miss
// (off screen, behind the eye, past the march, or sky) keeps the environment map.
// The result goes to `pane_hit`.

// March steps of `pane_reflection_trace` (quadratic spacing to 24 m) and bisection steps (16 + 3:
// the 20 + 4 first tried cost a canopy filling the screen 0.15 ms on an RTX 5080).
const PANE_SSR_STEPS: i32 = 16;
const PANE_SSR_REFINE: i32 = 3;
const PANE_SSR_REACH_M: f32 = 24.0;

fn pane_reflection_trace(in: VertexOutput, front_facing: bool) {
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
    let v = normalize(frame.camera_pos.xyz - in.world_position);
    var r = reflect(-v, n);
    r = normalize(r + n * max(0.1 - dot(r, n), 0.0));
    // Per-pixel start offset along the step (interleaved gradient noise), resolved by the TAA.
    let noise_pix = in.clip_position.xy + 5.588238 * frame.quality.w;
#ifdef FRAME_BLUE_NOISE
    var jitter = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715))));
    if (frame_blue_noise_on()) {
        jitter = frame_blue_noise_at(in.clip_position.xy);
    }
#else
    let jitter = fract(52.9829189 * fract(dot(noise_pix, vec2<f32>(0.06711056, 0.00583715))));
#endif
    let n_near = frame.params.x;
    let n_far = frame.params.y;
    let dims = vec2<f32>(textureDimensions(refract_depth));
    var prev_t = 0.0;
    for (var i = 0; i < PANE_SSR_STEPS; i = i + 1) {
        let u = (f32(i) + jitter) / f32(PANE_SSR_STEPS);
        let t = 0.02 + PANE_SSR_REACH_M * u * u;
        let s = in.world_position + r * t;
        let c = frame.view_proj * vec4<f32>(s, 1.0);
        if (c.w <= n_near) {
            return;
        }
        let ndc = c.xy / c.w;
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        if (any(uv <= vec2<f32>(0.0)) || any(uv >= vec2<f32>(1.0))) {
            return;
        }
        let d = textureLoad(refract_depth, vec2<i32>(uv * dims), 0);
        if (d < 1.0) {
            let scene_z = n_near * n_far / max(n_far - d * (n_far - n_near), 1e-6);
            let behind = c.w - scene_z;
            // A surface counts as hit when the ray passed behind it by less than an assumed
            // thickness (10 cm, 2 % of the depth and a quarter of the step): a ray climbing
            // over the ground behind a canopy passes behind the ground's pixels by metres and
            // must not take their colour (a bisection then finds the crossing of a real hit).
            let thick = 0.1 + 0.02 * c.w + 0.25 * (t - prev_t);
            // Behind a surface by more than the thickness but less than `2t + 0.5` (the reach of
            // the pre-ADR-165 test): the ray went into something near it whose far side the
            // screen does not show. The environment is blocked there, as before ADR-165 (the far
            // ground beside a hull seen from above the canopy is behind the hull, in its shadow).
            if (behind > thick && behind > 0.03 + 0.01 * c.w && behind < 2.0 * t + 0.5) {
                pane_visibility = 0.0;
                return;
            }
            if (behind > 0.0 && behind < thick) {
                var lo = prev_t;
                var hi = t;
                for (var j = 0; j < PANE_SSR_REFINE; j = j + 1) {
                    let mid = 0.5 * (lo + hi);
                    let cm = frame.view_proj * vec4<f32>(in.world_position + r * mid, 1.0);
                    let um = vec2<f32>(cm.x / cm.w * 0.5 + 0.5, 0.5 - cm.y / cm.w * 0.5);
                    let dm = textureLoad(refract_depth, vec2<i32>(clamp(um, vec2<f32>(0.0), vec2<f32>(0.9999)) * dims), 0);
                    let zm = n_near * n_far / max(n_far - dm * (n_far - n_near), 1e-6);
                    if (dm < 1.0 && cm.w > zm) {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                let ch = frame.view_proj * vec4<f32>(in.world_position + r * hi, 1.0);
                let uh = clamp(
                    vec2<f32>(ch.x / ch.w * 0.5 + 0.5, 0.5 - ch.y / ch.w * 0.5),
                    vec2<f32>(0.0),
                    vec2<f32>(1.0),
                );
                let colour = textureSampleLevel(refract_scene, refract_samp, uh, 0.0).rgb
                    / max(frame.post_exposure.x, 1e-8);
                // Fade to the environment over the outer 6 % of the screen.
                let border = min(min(uh.x, 1.0 - uh.x), min(uh.y, 1.0 - uh.y));
                pane_hit = vec4<f32>(colour, smoothstep(0.0, 0.06, border));
                return;
            }
        }
        prev_t = t;
    }
}

// The surface's own light of a transmissive pane drawn while the refraction copy binds the
// opaque depth (group 3, `refract_layout`): `fs_main` with the pane's reflection visibility.
@fragment
fn fs_transmit_add(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    // `interior_half.w` bit 3: reflection occlusion is on. Both faces: which side of a pane
    // faces into what it encloses is up to the asset's winding (the X12's far canopy pane shows
    // its front face from outside).
    // The nearest pane traces its reflection's colour (it needs the pre-exposed HDR copy,
    // `post_exposure.x` > 0); a pane behind it shows through it at its transmittance and keeps
    // the cheaper blocked-or-open test.
    let weight = front_layer_weight(in);
    if ((u32(frame.interior_half.w + 0.5) & 8u) != 0u) {
        if (all(weight >= vec3<f32>(1.0)) && frame.post_exposure.x > 0.0) {
            pane_reflection_trace(in, front_facing);
        } else {
            pane_visibility = pane_reflection_visibility(in, front_facing);
        }
    }
    return shade_forward(in, front_facing) * vec4<f32>(weight, 1.0);
}

#ifdef GLASS
// Per-instance glass's own light on a frame with the glass front layer (`fs_main` weighted as
// `fs_transmit_add` weights it; the own canopy keeps its sky, no traced visibility).
@fragment
fn fs_glass_add(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    return shade_forward(in, front_facing) * vec4<f32>(front_layer_weight(in), 1.0);
}

// One bilinear 2×2 compare of `cascade` with `sample_cascade`'s receiver-plane bias, its
// fine-to-coarse ramp included (`CSM_COARSE_*` once the texel passes `CSM_FINE_TEXEL_M`: a large
// first cascade, a low shadow resolution). −1 when `world` is off the cascade's map.
fn canopy_cascade_visibility(cascade: u32, world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> f32 {
    let tile_bias = cascade_bias(cascade);
    let texel_world = max(tile_bias.x, 1e-6);
    let depth_scale = max(tile_bias.y, 1e-9);
    let vp = cascade_view_proj(cascade);
    let no_l = clamp(dot(normal, l), 0.0, 1.0);
    let sin_theta = sqrt(max(1.0 - no_l * no_l, 0.0));
    let coarse = smoothstep(CSM_FINE_TEXEL_M, CSM_COARSE_TEXEL_M, texel_world);
    let offset_m = mix(
        min(CSM_NORMAL_OFFSET_TEXELS * texel_world, CSM_NORMAL_OFFSET_MAX_M),
        CSM_COARSE_NORMAL_OFFSET_TEXELS * texel_world,
        coarse,
    ) * sin_theta;
    let uvz = regia::shadows::shadow_uv_from_clip(vp * vec4<f32>(world + normal * offset_m, 1.0));
    if (uvz.x < 0.0 || uvz.y < 0.0 || uvz.x > 1.0 || uvz.y > 1.0) {
        return -1.0;
    }
    let texel = 1.0 / f32(textureDimensions(shadow_map_0, 0).x);
    var slope = regia::shadows::receiver_plane_slope(vp, normal) * texel;
    let max_slope = CSM_MAX_TAN * texel_world * depth_scale;
    let slope_len = length(slope);
    if (slope_len > max_slope) {
        slope = slope * (max_slope / slope_len);
    }
    let constant_m = mix(
        min(CSM_DEPTH_BIAS_TEXELS * texel_world, CSM_DEPTH_BIAS_MAX_M),
        CSM_COARSE_DEPTH_BIAS_TEXELS * texel_world,
        coarse,
    );
    let z = uvz.z - constant_m * depth_scale;
    let bend = (CSM_BEND_TEXELS * texel_world * depth_scale + CSM_BEND_SLOPE * length(slope))
        * min(texel_world / CSM_BEND_FULL_TEXEL_M, 1.0);
    return csm_filtered(cascade, uvz.xy, z, slope, bend, 0);
}

// Sun visibility on the own canopy: one bilinear 2×2 compare of cascade 0 (the glass is a metre
// from the eye), or of cascade 1 where the point is off cascade 0's map (a first cascade shorter
// than the canopy is far, a camera just outside the glass). Sunlit only off both. No PCSS, no
// near sun tile, no cascade blend.
fn canopy_sun_visibility(world: vec3<f32>, normal: vec3<f32>, l: vec3<f32>) -> f32 {
    let near = canopy_cascade_visibility(0u, world, normal, l);
    if (near >= 0.0) {
        return near;
    }
    if (clamp(u32(csm.count.x), 1u, 4u) > 1u) {
        let next = canopy_cascade_visibility(1u, world, normal, l);
        if (next >= 0.0) {
            return next;
        }
    }
    return 1.0;
}

// The own canopy's cheap light (`glass::CanopyShading::Cheap`, ADR-224). The camera sits inside
// this glass, so it covers most of the screen, often in two layers, in the clusters that hold the
// cockpit's lights and fitted emitters; `shade_forward` ran all of them per layer (DO27's cockpit
// forward cost about 4 ms per internal megapixel more than on foot). This keeps what a pilot sees
// on a clear canopy: the environment reflection with its Fresnel (split sum), the sun's specular
// and diffuse behind one 2×2 compare of cascade 0, the planet-shine fill, the crack shading, the
// material's own emission and the transmission weight. The multiply pass (`fs_transmit`) still
// tints and absorbs what is seen through it. Dropped: local-light and fitted-emitter light on the
// glass (their highlights), probe GI and its sky occlusion of the reflection, the terrain and
// large-mesh occlusion terms, the ground bounce, multi-scatter, clear coat, anisotropy, detail
// maps, fog and the shadow debug views.
//
// `drop_layer`: discard the fragment (`fs_canopy_add_front`'s nearest-layer option). Every
// implicit-derivative operation (`sample_material`'s `textureSampleBias`, `alpha_minification`,
// the specular-AA `dpdx`, `csm_geometric_normal`) runs before the first `discard`, in uniform
// control flow: naga 30 emits `discard` as SPIR-V `OpKill`, which ends the invocation (not
// `OpDemoteToHelperInvocation`, which would keep it as a helper), so once one lane of a quad has
// discarded the code below runs in non-uniform control flow, where derivatives are undefined.
// What follows the discard samples only with explicit levels or gathers
// (`tests/canopy_shader_checks.rs`).
fn shade_canopy(in: VertexOutput, front_facing: bool, drop_layer: bool) -> vec4<f32> {
    let uv = surface_uv(in, front_facing);
    #ifdef TEXTURE_TABLE
    let alb = sample_material_slot(0u, uv);
    #else
    let alb = sample_material(albedo_tex, uv);
    #endif
    var base = alb.rgb * material.base_color.rgb * in.color.rgb;
    let alpha = alb.a * material.base_color.a * in.color.a;
#ifdef ALPHA_TEST
    let mode = u32(material.alpha.x + 0.5);
    let minified = alpha_minification(uv);
    let alpha_rejected = (mode == 1u && alpha_rejects(alpha, material.alpha.y, minified, in.clip_position.xy))
        || lod_dither_discard(in.lod_dither, in.clip_position.xy);
#else
    let alpha_rejected = false;
#endif
    var n = normalize(in.world_normal);
    if !front_facing {
        n = -n;
    }
    let normal_scale = material.factors.z;
    var normal_length = 1.0;
    if normal_scale > 0.0 {
        let t_front = normalize(in.world_tangent.xyz);
        let b = cross(n, t_front) * in.world_tangent.w;
        let t = back_face_tangent(t_front, front_facing);
        #ifdef TEXTURE_TABLE
        let ts = decode_normal_sample(sample_material_slot(1u, uv));
        #else
        let ts = decode_normal_sample(sample_material(normal_tex, uv));
        #endif
        if ((u32(material.alpha.z + 0.5) & 8u) == 0u) {
            normal_length = length(ts);
        }
        let mapped = vec3<f32>(ts.x * normal_scale, ts.y * normal_scale, ts.z);
        n = normalize(mat3x3<f32>(t, b, n) * mapped);
    }
    #ifdef TEXTURE_TABLE
    let orm = sample_material_slot(2u, uv);
    #else
    let orm = sample_material(occlusion_tex, uv);
    #endif
    var mr_scale = vec2<f32>(1.0, 1.0);
    if ((u32(material.alpha.w + 0.5) & 1u) != 0u) {
        mr_scale = orm.gb;
    }
    let metallic = regia::utils::saturate(material.factors.x * mr_scale.y);
    var perceptual = regia::utils::saturate(material.factors.y * mr_scale.x);
    if (frame.specular_aa.x > 0.0) {
        perceptual = regia::pbr::lighting::specular_aa_roughness(perceptual, dpdx(n), dpdy(n), frame.specular_aa.x, frame.specular_aa.y);
        perceptual = regia::pbr::lighting::toksvig_roughness(perceptual, normal_length, frame.specular_aa.z);
    }
    let shadow_n = csm_geometric_normal(in.world_position, n);
    #ifdef TEXTURE_TABLE
    let emissive_tex_rgb = sample_material_slot(3u, uv).rgb;
    #else
    let emissive_tex_rgb = sample_material(emissive_tex, uv).rgb;
    #endif
    let v_dir = normalize(frame.camera_pos.xyz - in.world_position);
    // Explicit page levels (no derivatives).
    let gf = glass_fragment(in.world_position, in.uv, n, v_dir);
    // The derivative-using work is done: every discard sits after it.
    if alpha_rejected || drop_layer || gf.hole > 0.5 {
        discard;
    }
    n = gf.n;
    perceptual = mix(perceptual, max(perceptual, glass.shade.z), regia::utils::saturate(gf.m.g * 1.5));
    perceptual = mix(perceptual, max(perceptual, glass.shade.x), gf.m.r);
    base = mix(base, glass.scatter.rgb, regia::utils::saturate(gf.m.r + gf.m.g * glass.shade.y));
    let glass_spec = glass.view.y;
    let no_v = max(dot(n, v_dir), 1e-5);
    let rough = max(perceptual * perceptual, regia::pbr::lighting::MIN_ALPHA);
    let glass_scatter_free = (1.0 - gf.m.r * (1.0 - glass.scatter.a)) * (1.0 - gf.m.g * glass.shade.y);
    let transmission = regia::utils::saturate(material.pom.z) * glass_scatter_free;
    let diffuse_color = base * (1.0 - metallic) * (1.0 - transmission);
    let f0_dielectric = select(0.04, material.pom.w, material.pom.w > 0.0);
    let f0 = mix(vec3<f32>(f0_dielectric), base, vec3<f32>(metallic));
    let occlusion = mix(1.0, orm.r, regia::utils::saturate(material.factors.w));
    let emissive = material.emissive.rgb * material.emissive.w * emissive_tex_rgb;
    if ((u32(material.alpha.z + 0.5) & 1u) != 0u) {
        // Unlit: as `shade_forward`.
        #ifdef skip_srgb_encode
        return vec4<f32>(base, alpha);
        #else
        return vec4<f32>(regia::utils::encode_srgb(base), alpha);
        #endif
    }

    // The sun.
    let l = -normalize(dir_light.direction);
    let h = normalize(l + v_dir);
    let no_l = max(dot(n, l), 0.0);
    let iso = regia::pbr::brdf::isotropic_lobes(no_v, no_l, max(dot(n, h), 0.0), max(dot(l, h), 0.0), diffuse_color, f0, rough);
    var lit = (iso.diffuse + iso.specular * glass_spec) * dir_light.color * dir_light.illuminance * no_l;
    if (frame.extra.x > 0.5) {
        lit *= canopy_sun_visibility(in.world_position, shadow_n, l);
    }
    if (frame.fill_dir.w > 0.5) {
        let lf = normalize(frame.fill_dir.xyz);
        lit += diffuse_color * (1.0 / regia::pbr::lighting::PI) * frame.fill_color.rgb * max(dot(n, lf), 0.0);
    }

    // The environment: `shade_forward`'s sky IBL, flat ambient and interior floor, without the
    // probe and occlusion terms.
    let in_box = all(abs(in.world_position - frame.interior_center.xyz) <= frame.interior_half.xyz);
    let interior_view = frame.camera_pos.w > 0.5;
    let interior_w = select(0.0, 1.0, interior_view && (frame.interior_center.w < 0.5 || in_box));
    var sky_diffuse = vec3<f32>(0.0);
    var spec_ibl = vec3<f32>(0.0);
    if (frame.extra.z > 0.5 && interior_w < 1.0) {
        let ibl_scale = frame.post_exposure.z;
        sky_diffuse = textureSampleLevel(irradiance_tex, ibl_samp, equirect_uv(n), 0.0).rgb * ibl_scale;
        let refl = reflect(-v_dir, n);
        let pref = textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(refl), perceptual * frame.extra.w).rgb * ibl_scale;
        let env_brdf = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;
        spec_ibl = pref * (f0 * env_brdf.x + env_brdf.y);
    } else if (interior_w < 1.0) {
        sky_diffuse = regia::utils::sky_color(n) * frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI);
    }
    let floor_diffuse = vec3<f32>(frame.post_exposure.w * (1.0 / regia::pbr::lighting::PI));
    let diffuse_indirect = mix(sky_diffuse, floor_diffuse, interior_w);
    let spec_occlusion = select(
        specular_occlusion(no_v, occlusion, rough),
        regia::pbr::lighting::specular_occlusion(no_v, occlusion, perceptual),
        material.response.x > 0.5,
    );
    lit += diffuse_color * diffuse_indirect * multi_bounce_ao(occlusion, diffuse_color + f0 * metallic)
        + spec_ibl * (1.0 - interior_w) * glass_spec * spec_occlusion;

    let scene_color = lit + emissive;
    var out_alpha = alpha;
    if (transmission > 0.0) {
        let fv = f0_dielectric + (1.0 - f0_dielectric) * pow(1.0 - no_v, 5.0);
        let tint = regia::utils::saturate(regia::utils::luminance(base));
        out_alpha = 1.0 - transmission * tint * (1.0 - fv);
    }
    if (frame.post_exposure.x > 0.0) {
        let finite = regia::utils::finite_or_zero(scene_color);
        return vec4<f32>(min(max(finite, vec3<f32>(0.0)) * frame.post_exposure.x, vec3<f32>(6.0e4)), out_alpha);
    }
    let exposed = clamp(scene_color * frame.post_exposure.y, vec3<f32>(0.0), vec3<f32>(1.0e4));
    var color = regia::utils::default_display_transform(exposed);
    #ifdef skip_srgb_encode
    color = regia::utils::decode_srgb(color);
    #endif
    return vec4<f32>(color, out_alpha);
}

// `fs_main` for a viewer-inside instance on the cheap path.
@fragment
fn fs_canopy_add(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    return shade_canopy(in, front_facing, false);
}

// `fs_glass_add` for a viewer-inside instance on the cheap path. `view.x` 2 (nearest layer only,
// `CanopyGlass::nearest_layer_only`): a layer behind the nearest transmissive one adds nothing
// (its own light seen through the nearest is dropped; its multiply still tints). The layer is
// discarded inside `shade_canopy`, after its derivative-using work, not before it.
@fragment
fn fs_canopy_add_front(in: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    let weight = front_layer_weight(in);
    let drop_layer = glass.view.x > 1.5 && any(weight < vec3<f32>(1.0));
    return shade_canopy(in, front_facing, drop_layer) * vec4<f32>(weight, 1.0);
}
#endif
#endif

// ---- Sky background ----
// A full-screen triangle at the far plane, drawn after the opaque geometry with
// depth test LessEqual and no depth write, so only pixels nothing covered show sky.
// Before this pass the mesh renderer cleared the background to near-black.

struct SkyOut {
    @builtin(position) position: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex
fn vs_sky(@builtin(vertex_index) id: u32) -> SkyOut {
    let x = select(-1.0, 3.0, id == 1u);
    let y = select(-1.0, 3.0, id == 2u);
    return SkyOut(vec4<f32>(x, y, 1.0, 1.0), vec2<f32>(x, y));
}

@fragment
fn fs_sky(in: SkyOut) -> @location(0) vec4<f32> {
    let tan_half = tan(frame.params.z * 0.5);
    let dir_view = normalize(vec3<f32>(in.ndc.x * tan_half * frame.params.w, in.ndc.y * tan_half, -1.0));
    // `view` is world → view; its upper 3×3 is a rotation, so the transpose maps back.
    let rot = mat3x3<f32>(frame.view[0].xyz, frame.view[1].xyz, frame.view[2].xyz);
    let dir = normalize(transpose(rot) * dir_view);
    var radiance = regia::aerial::sky_radiance(
        dir,
        -normalize(dir_light.direction),
        dir_light.color * dir_light.illuminance,
        frame.post_exposure.z,
    );
    if (frame.specular_aa.w > 0.5) {
        // A reference studio's environment (`FrameRenderer::set_studio_sky`): the background is
        // the world the IBL was built from, as a path tracer shows it.
        radiance = textureSampleLevel(prefilter_tex, ibl_samp, equirect_uv(dir), 0.0).rgb * frame.post_exposure.z;
    }
    // A background chosen apart from the lighting (`StudioBackground`, ADR-2000).
    let background = u32(frame.env_view.y + 0.5);
    if (background == 1u) {
        radiance = frame.background[0].rgb;
    } else if (background == 2u) {
        radiance = frame.background[2].rgb;
        if (dir.y >= 0.0) {
            let t = pow(clamp(dir.y, 0.0, 1.0), frame.background[0].w);
            radiance = mix(frame.background[1].rgb, frame.background[0].rgb, t);
        }
    } else if (background == 3u) {
        radiance = textureSampleLevel(background_tex, ibl_samp, equirect_uv(dir), 0.0).rgb * frame.post_exposure.z;
    }
    if (frame.weather2.w > 0.0) {
        radiance = regia::forward::surface_weather::weather_extinction(radiance, frame.params.y, frame.weather2.w, weather_inscatter(dir));
    }
    if (frame.post_exposure.x > 0.0) {
        // Alpha = perceptual roughness for the SSR composite: the sky reflects nothing.
        return vec4<f32>(radiance * frame.post_exposure.x, 1.0);
    }
    let exposed = clamp(radiance * frame.post_exposure.y, vec3<f32>(0.0), vec3<f32>(1.0e4));
    var color = regia::utils::default_display_transform(exposed);
    #ifdef skip_srgb_encode
    color = regia::utils::decode_srgb(color);
    #endif
    return vec4<f32>(color, 1.0);
}
