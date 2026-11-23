// SPDX-License-Identifier: MPL-2.0
// Scatter: instanced low-poly rocks placed by the deterministic CPU scatter pass.
// 24 vertices = 4 side triangles + 4 base fan triangles; normals from screen-space
// derivatives (flat shading).

struct Frame {
    view_proj: mat4x4<f32>,
    camera_pos_time: vec4<f32>,
    sun_dir_wetness: vec4<f32>,
    // rgb = sun colour (linear), a = illuminance_lux * exposure (pre-multiplied)
    sun_color_exposure: vec4<f32>,
    // rgb = ambient colour (linear), a = debug view (see `TerrainFrame::debug_view`;
    // scatter implements 2 = N·L, 3 = instance albedo, 5 = world normal)
    ambient_debug: vec4<f32>,
    fog_params: vec4<f32>,
    fog_color: vec4<f32>,
    light_view_proj: mat4x4<f32>,
    shadow_params: vec4<f32>,
};

struct ScatterParams {
    fade_start: f32,
    fade_end: f32,
    align_tilt: f32,
    pad: f32,
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(1) @binding(0) var<uniform> params: ScatterParams;

struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) tint: vec3<f32>,
    @location(2) fade: f32,
};

// A `switch`, not a dynamically indexed value array: naga 30 spills that to a
// layout-decorated Function local, which SPIR-V 1.6 rejects (ADR-026).
fn corner(i: u32) -> vec3<f32> {
    switch i {
        case 0u: { return vec3<f32>(-0.5, 0.0, -0.5); }
        case 1u: { return vec3<f32>(0.5, 0.0, -0.4); }
        case 2u: { return vec3<f32>(0.45, 0.0, 0.5); }
        case 3u: { return vec3<f32>(-0.45, 0.05, 0.5); }
        case 4u: { return vec3<f32>(0.02, 0.85, -0.02); }
        default: { return vec3<f32>(0.0, 0.0, 0.02); }
    }
}

// 8 triangles (24 vertices). Side faces reversed (c1, c0, top) so their normals point outward;
// base fan (c, c+1, centre) faces down (wound CW seen from above → normal -Y), closing the
// mesh underside.
// Side face f (0..3) = (f+1 mod 4, f, 4); base face f = (f, f+1 mod 4, 5). Computed, not
// looked up, for the same ADR-026 reason as `corner`.
fn face_vertex(vi: u32) -> u32 {
    let tri = vi / 3u;
    let k = vi % 3u;
    let f = tri % 4u;
    let next = (f + 1u) % 4u;
    if (tri < 4u) {
        return select(select(4u, f, k == 1u), next, k == 0u);
    }
    return select(select(5u, next, k == 1u), f, k == 0u);
}

fn scatter_world(vi: u32, pos_yaw: vec4<f32>, scale_pad: vec4<f32>, normal: vec4<f32>) -> vec3<f32> {
    let local = corner(face_vertex(vi));
    let scale = scale_pad.x;
    let yaw = pos_yaw.w;

    let s = sin(yaw);
    let c = cos(yaw);
    // Column-major yaw rotation about +Y.
    let rot = mat3x3<f32>(vec3<f32>(c, 0.0, -s), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(s, 0.0, c));

    // Tilt the up axis toward the surface normal by `align_tilt`, then rebuild the basis.
    // Every normalize is guarded (ADR-869): a zero normal, a tilt halfway to a downward normal
    // (the mix is zero) or an up axis along ±Z (a north/south cliff at full tilt: its cross
    // with −Z is zero) would make every vertex NaN and the instance vanish. Each fallback only
    // applies where the plain expression is NaN or near it, so ordinary instances are unchanged.
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let n_len2 = dot(normal.xyz, normal.xyz);
    let n_unit = select(up, normalize(normal.xyz), n_len2 > 1.0e-12);
    let tilted = mix(up, n_unit, params.align_tilt);
    let up_target = select(up, normalize(tilted), dot(tilted, tilted) > 1.0e-12);
    let side = cross(up_target, vec3<f32>(0.0, 0.0, -1.0));
    let right = select(
        normalize(cross(up_target, vec3<f32>(1.0, 0.0, 0.0))),
        normalize(side),
        dot(side, side) > 1.0e-8,
    );
    let fwd = cross(right, up_target);
    let basis = mat3x3<f32>(right, up_target, fwd);

    return basis * (rot * local) * scale + pos_yaw.xyz;
}

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) pos_yaw: vec4<f32>,
    @location(1) scale_pad: vec4<f32>,
    @location(2) normal: vec4<f32>,
    @location(3) tint: vec4<f32>,
) -> VOut {
    let world = scatter_world(vi, pos_yaw, scale_pad, normal);

    let d = distance(frame.camera_pos_time.xyz, world);
    var fade = 1.0;
    if (params.fade_end > params.fade_start) {
        fade = 1.0 - clamp((d - params.fade_start) / (params.fade_end - params.fade_start), 0.0, 1.0);
    }

    return VOut(
        frame.view_proj * vec4<f32>(world, 1.0),
        world,
        clamp(tint.rgb, vec3<f32>(0.0), vec3<f32>(2.0)),
        fade,
    );
}

struct ShadowOut {
    @builtin(position) position: vec4<f32>,
};

// Depth-only caster. Same world position as `vs_main`, projected by the sun cascade.
@vertex
fn vs_shadow(
    @builtin(vertex_index) vi: u32,
    @location(0) pos_yaw: vec4<f32>,
    @location(1) scale_pad: vec4<f32>,
    @location(2) normal: vec4<f32>,
) -> ShadowOut {
    let world = scatter_world(vi, pos_yaw, scale_pad, normal);
    return ShadowOut(frame.light_view_proj * vec4<f32>(world, 1.0));
}

// AgX display transform + the engine's default look. Mirror of `regia_render`'s
// `utils.wgsl` (`tonemap_agx_vec`, `display_look`, `DEFAULT_LOOK_*`); the test
// `terrain_display_transform_mirrors_the_renderer` keeps the copies identical.
// Provenance: Sobotka's AgX, minimal form after B. Wrensch (2023); see docs/provenance.md.
// Output is display-encoded; `display_to_linear` hands an sRGB-encoding target linear values.
// BEGIN display-transform mirror
const DEFAULT_LOOK_CONTRAST: f32 = 1.2;
const DEFAULT_LOOK_SATURATION: f32 = 1.4;

fn agx_sigmoid(x: vec3<f32>) -> vec3<f32> {
    let x2 = x * x;
    let x4 = x2 * x2;
    return 15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x
        - vec3<f32>(0.00232);
}

fn tonemap_agx_vec(color: vec3<f32>) -> vec3<f32> {
    let inset = mat3x3<f32>(
        vec3<f32>(0.842479062253094, 0.0423282422610123, 0.0423756549057051),
        vec3<f32>(0.0784335999999992, 0.878468636469772, 0.0784336),
        vec3<f32>(0.0792237451477643, 0.0791661274605434, 0.879142973793104),
    );
    let outset = mat3x3<f32>(
        vec3<f32>(1.19687900512017, -0.0528968517574562, -0.0529716355144438),
        vec3<f32>(-0.0980208811401368, 1.15190312990417, -0.0980434501171241),
        vec3<f32>(-0.0990297440797205, -0.0989611768448433, 1.15107367264116),
    );
    let min_ev = -12.47393;
    let max_ev = 4.026069;
    var v = inset * max(color, vec3<f32>(1.0e-10));
    v = clamp(log2(max(v, vec3<f32>(1.0e-10))), vec3<f32>(min_ev), vec3<f32>(max_ev));
    v = (v - vec3<f32>(min_ev)) / (max_ev - min_ev);
    v = agx_sigmoid(v);
    v = outset * v;
    return clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn display_look(display: vec3<f32>, contrast: f32, saturation: f32) -> vec3<f32> {
    let v = clamp(display, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 0.5 * pow(v * 2.0, vec3<f32>(contrast));
    let hi = vec3<f32>(1.0) - 0.5 * pow((vec3<f32>(1.0) - v) * 2.0, vec3<f32>(contrast));
    let curved = select(hi, lo, v < vec3<f32>(0.5));
    let luma = dot(curved, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(vec3<f32>(luma) + (curved - vec3<f32>(luma)) * saturation, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn default_display_transform(color: vec3<f32>) -> vec3<f32> {
    return display_look(tonemap_agx_vec(color), DEFAULT_LOOK_CONTRAST, DEFAULT_LOOK_SATURATION);
}
// END display-transform mirror

fn display_to_linear(display: vec3<f32>) -> vec3<f32> {
    return pow(max(display, vec3<f32>(0.0)), vec3<f32>(2.2));
}

const INV_PI: f32 = 0.3183098862;

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    // Derivatives first: `fade` is interpolated, so the discard below is non-uniform, and a
    // `dpdx` after it runs in non-uniform control flow (ADR-869).
    let dwx = dpdx(in.world);
    let dwy = dpdy(in.world);
    if (in.fade <= 0.0) {
        discard;
    }
    // Flat-shaded normal from screen-space derivatives of the world position. WGSL's
    // framebuffer y grows DOWNWARD (Vulkan coordinate convention), so the GL-order
    // cross(dFdx, dFdy) yields the back-facing normal; the factors are swapped here to
    // get the outward normal of the visible (front-facing) triangle. With the GL order,
    // every visible fragment faced away from the sun, N·L clamped to 0 and the
    // instances rendered as ambient-only dark specks (M13 round 4 root cause).
    // A sliver or edge-on triangle has a zero cross product: shade it as facing up rather than
    // with a NaN normal (NaN survives the `clamp` below).
    let face = cross(dwy, dwx);
    let n = select(vec3<f32>(0.0, 1.0, 0.0), normalize(face), dot(face, face) > 1.0e-20);
    let sun_dir = normalize(frame.sun_dir_wetness.xyz);
    // Same lighting convention as terrain.wgsl / regia_render::forward: π-normalised
    // Lambert diffuse of the exposed sun radiance plus the sky ambient, AgX tonemap, and
    // the sRGB target encodes once on store.
    let ndl = max(dot(n, sun_dir), 0.0);
    let sun_radiance = frame.sun_color_exposure.rgb * frame.sun_color_exposure.a;
    let light = sun_radiance * ndl * INV_PI + frame.ambient_debug.rgb;
    // `tint` is the instance albedo (natural rock/dirt hues from the scatter pass).
    var colour = clamp(in.tint, vec3<f32>(0.0), vec3<f32>(2.0)) * light;
    // Face shading variation keeps the low-poly rocks readable.
    let shade = 0.85 + 0.3 * clamp(n.y, 0.0, 1.0);
    colour *= shade;

    // Debug views (same frame.debug_view values as terrain.wgsl where they apply): 2 = N·L
    // only, 3 = instance albedo (tint), 5 = world normal as colour. The terrain-only views
    // (1 = LOD, 4 = splat weights, 6 = height) do not apply to scatter and fall through to
    // full shading.
    let dbg = frame.ambient_debug.a;
    if (dbg > 1.5 && dbg < 2.5) {
        return vec4<f32>(vec3<f32>(ndl), 1.0); // N·L only
    }
    if (dbg > 2.5 && dbg < 3.5) {
        return vec4<f32>(in.tint, 1.0); // instance albedo (pre-lighting)
    }
    if (dbg > 4.5 && dbg < 5.5) {
        return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0); // world normal as colour
    }

    var lit = colour * in.fade + colour * 0.15 * (1.0 - in.fade);
    // Height fog (mirrors regia_render::fog).
    let fp = frame.fog_params;
    if (fp.w > 0.5 && fp.x > 0.0) {
        let distance = length(in.world - frame.camera_pos_time.xyz);
        let height_density = fp.x * exp(-fp.y * (in.world.y - fp.z));
        let t = 1.0 - exp(-max(distance, 0.0) * max(height_density, 0.0));
        lit = mix(lit, frame.fog_color.rgb, t);
    }
    lit = clamp(lit, vec3<f32>(0.0), vec3<f32>(1.0e4));
    // The target is Rgba8UnormSrgb (the hardware encodes): store linear display values.
    lit = display_to_linear(default_display_transform(lit));
    return vec4<f32>(lit, 1.0);
}