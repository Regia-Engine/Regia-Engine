// SPDX-License-Identifier: MPL-2.0
// Screen-space ambient occlusion from the depth buffer, in view space.
//
// Obscurance estimator: McGuire, Osman, Bukowski, Hennessy, "The Alchemy
// Screen-Space Ambient Obscurance Algorithm", HPG 2011 — reconstruct the view-space
// position and normal from depth, gather points on a spiral inside a world-space
// radius, and count how far each one rises above the receiver's tangent plane
// (cosine-weighted, falling off to zero at the radius). Per-pixel spiral rotation:
// interleaved gradient noise, Jimenez, "Next Generation Post Processing in Call of
// Duty: Advanced Warfare", SIGGRAPH 2014. A 4×4 depth-aware blur (`cs_blur`)
// removes the rotation pattern.
//
// Desktop tiers (`noise.y` = 1) also estimate how much of the specular lobe sees past the
// scene's enclosure, into G (a glossy lobe) and B (a rough one): see `reflection_visibility`.
//
// The previous kernel compared raw (non-linear) depth-buffer values against fixed
// thresholds and a fixed 12-pixel radius, so occlusion faded out a few metres from
// the camera and never reached contact scale on props.
//
// Probe-relative AO (ADR-1104, `baked_gi/ao_split.rs`): while a baked GI volume lights the
// frame, its screen pass runs first and leaves per lookup texel the share of the light its
// detail brick and terrain layer hold and their resolve distances (`probe_split`). The probe
// light already holds the occlusion of baked geometry above about half a probe cell, so for
// that share each sample's obscurance is weighted by `probe_ao_keep` of its height above the
// tangent plane; the rest of the light (the sky outside the volume) keeps the whole kernel.
//
// Bent normal (ADR-1375, `ssao/bent_normal.rs`): with `noise.z` = 1 the same samples also give the
// receiver's mean unoccluded direction (Landis, "Production-Ready Global Illumination",
// SIGGRAPH 2002 course 16; in screen space, Klehm, Ritschel, Eisemann, Seidel, "Bent Normals and
// Cones in Screen-space", VMV 2011): each sample's occluded share leans the normal away from its
// direction. Layer 1 of the AO target holds the bent normal minus the depth normal, in world
// space (`bent_encode`), so the forward pass can add it to its own (normal-mapped) normal. The
// share of an occluder the baked probes already hold (the probe-relative split above) does not
// lean it: their light already comes from the open side.

struct SsaoParams {
    // x = width, y = height (the AO target), z = world radius (m), w = sample count.
    screen: vec4<f32>,
    // x = 1 for the horizon (GTAO-row) quality, y = tangent-plane bias (cos),
    // z = intensity, w = max radius in AO pixels.
    mode: vec4<f32>,
    // x = projection[0][0], y = projection[1][1], z = near, w = far.
    proj: vec4<f32>,
    // x = temporal noise index (0 without temporal AA), y = 1 to estimate the reflection
    // visibility (G, B); 0 writes 1 there. z = 1 with bent normals on: the pipelines built with
    // `BENT_NORMALS` (the constant, not z, selects the code). w = 0 always: the blurs start their
    // sums from it (a zero the shader compiler cannot fold, ADR-1560).
    noise: vec4<f32>,
    // Probe-relative split: x = first word of `probe_split`, y/z = the baked lookup's width and
    // height (0 = no split: the whole kernel everywhere), w unused.
    probe: vec4<u32>,
    // The camera-to-world rotation's columns (xyz; view-space x, y and z axes in world space):
    // the bent normal's lean is stored in world space.
    to_world_x: vec4<f32>,
    to_world_y: vec4<f32>,
    to_world_z: vec4<f32>,
}

// Bent normals compiled in (ADR-1491): `SsaoPass` builds the kernel and the blur twice, with this
// pipeline constant false (bent normals off: neither kernel sums the lean or writes layer 1) and
// true (`noise.z` = 1). A runtime test of `noise.z` left the off pipelines 0.004-0.006 ms slower
// on the RTX 5090 than code without the lean.
override BENT_NORMALS: bool = false;

#ifdef BLUE_NOISE
// Spatiotemporal blue noise for the spiral rotation (ADR-2075), `noise.x` picks the slice.
#import regia::blue_noise
@group(1) @binding(0) var blue_noise_table: texture_2d_array<f32>;

// The kernel rotation of AO texel `pix` in frame `frame`, in turns (ADR-5457): the interleaved
// gradient noise the kernel draws without the table (Jimenez, "Next Generation Post Processing in
// Call of Duty: Advanced Warfare", SIGGRAPH 2014), moved by the table's number within a sixteenth
// of a turn. The blur that follows (`cs_blur`) spans 4 × 4 texels, and the gradient noise is a
// lattice made for such a window: the rotations under the blur are evenly spread. With the table's
// number as the whole rotation they were independent draws, the blurred occlusion moved more from
// frame to frame, and on a frame the probes light, where the occlusion scales most of the indirect
// light, the resolved frame flickered 16 % more with the knob on than off
// (`tests/blue_noise_frames.rs` on an RTX 5090: 0.00142 against 0.00122). Sixteen strata per 4 × 4
// tile from a pair of Latin squares, stepped by the frame, read 0.00123; the lattice with the
// table's jitter reads 0.00100, and less grain than either (0.00264 against 0.00276 and 0.00309).
fn blue_rotation(pix: vec2<u32>, frame: u32) -> f32 {
    let lattice = ign(vec2<f32>(pix) + 5.588238 * params.noise.x);
    return fract(lattice + (regia::blue_noise::scalar(blue_noise_table, pix, frame) - 0.5) / 16.0);
}
#endif
@group(0) @binding(0) var<uniform> params: SsaoParams;
@group(0) @binding(1) var depth_tex: texture_depth_2d;
// Layer 0: R = AO, G/B = reflection visibility. Layer 1: the bent normal's lean (`bent_encode`).
@group(0) @binding(2) var out_tex: texture_storage_2d_array<rgba8unorm, write>;
@group(0) @binding(3) var raw_tex: texture_2d_array<f32>;
// The baked screen lookup's words (`baked_gi_screen.wgsl`): per lookup texel after its texels,
// pack2x16unorm(brick share, terrain share) and the brick's and the terrain layer's resolve
// distances (f32 bits; several volumes blend them per texel). A 16-byte placeholder without a
// split (`probe.y` = 0).
@group(0) @binding(4) var<storage, read> probe_split: array<u32>;

// BEGIN probe-relative AO (`baked_gi/ao_split.rs` `probe_ao_keep`, ADR-1104). This block is
// identical in ssao.wgsl and baked_gi_screen.wgsl (test `probe_ao_wgsl_copies_match`).
// How much of an occluder point `height` metres above the receiver's tangent plane the probe
// field does not already hold, for resolve distance `rho` (m): 1 below rho / 2, 0 past 1.5 rho.
fn probe_ao_keep(height: f32, rho: f32) -> f32 {
    return clamp(1.5 - height / max(rho, 1e-4), 0.0, 1.0);
}
// END probe-relative AO

// BEGIN bent normal (`ssao/bent_normal.rs`, ADR-1375). Mirrored on the CPU; keep the two in step.
// How far the occluded shares lean the normal: the open directions' cosine-weighted mean vector is
// (2/3) n minus the blocked directions' (Σ share · direction), so the bent normal is along
// n − 1.5 Σ share · direction (a floor beside a tall wall: 32.5°).
const BENT_TILT: f32 = 1.5;
// The largest lean from the normal (60°): a lean of at most unit length per component.
const BENT_MAX_ANGLE: f32 = 1.0471976;

// The bent normal minus `n` (both unit, view space) from `blocked` = Σ share · unit direction to
// each occluder point and `share` = Σ share (the occluded share, 1 − AO before the clamp).
fn bent_normal_lean(n: vec3<f32>, blocked: vec3<f32>, share: f32) -> vec3<f32> {
    // A clamped AO (share past 1) scales the lean back to a fully blocked receiver's.
    let scale = select(1.0, 1.0 / share, share > 1.0);
    let b = n - BENT_TILT * scale * blocked;
    let along = dot(b, n);
    let t = b - n * along;
    let tl = length(t);
    if (!(tl > 1e-6)) {
        return vec3<f32>(0.0);
    }
    let angle = min(atan2(tl, along), BENT_MAX_ANGLE);
    return n * cos(angle) + t * (sin(angle) / tl) - n;
}

// A lean (components in −1..1) as rgba8unorm codes 1..255, 128 = 0.
fn bent_encode(lean: vec3<f32>) -> vec3<f32> {
    return (clamp(lean, vec3<f32>(-1.0), vec3<f32>(1.0)) * 127.0 + vec3<f32>(128.0)) / 255.0;
}

// The lean from its codes. One code either side of 128 reads as 0, so a driver that rounds the
// storage conversion down still writes an exact zero lean.
fn bent_decode(enc: vec3<f32>) -> vec3<f32> {
    let d = (round(enc * 255.0) - vec3<f32>(128.0)) / 127.0;
    return select(d, vec3<f32>(0.0), abs(d) < vec3<f32>(1.5 / 127.0));
}
// END bent normal

// A view-space direction in world space.
fn to_world(v: vec3<f32>) -> vec3<f32> {
    return params.to_world_x.xyz * v.x + params.to_world_y.xyz * v.y + params.to_world_z.xyz * v.z;
}

// Writes both layers of AO texel `p`: layer 0 = (ao, reflection visibility), layer 1 = the
// encoded lean.
fn store_texel(p: vec2<i32>, ao_refl: vec3<f32>, lean: vec3<f32>) {
    textureStore(out_tex, p, 0, vec4<f32>(ao_refl, 1.0));
    // With bent normals off nothing reads layer 1 (the forward pass tests the same switch,
    // `interior_half.w` bit 4), so it is not written (ADR-1491).
    if (BENT_NORMALS) {
        textureStore(out_tex, p, 1, vec4<f32>(bent_encode(lean), 1.0));
    }
}

// The split at depth texel `coord`: (brick share, terrain share, brick ρ, terrain ρ); zero
// shares without a split.
fn probe_split_at(coord: vec2<i32>, depth_size: vec2<i32>) -> vec4<f32> {
    if (params.probe.y == 0u || params.probe.z == 0u) {
        return vec4<f32>(0.0, 0.0, 1.0, 1.0);
    }
    let size = vec2<i32>(params.probe.yz);
    let t = clamp(coord * size / max(depth_size, vec2<i32>(1)), vec2<i32>(0), size - vec2<i32>(1));
    let word = params.probe.x + 3u * (u32(t.y) * params.probe.y + u32(t.x));
    let shares = unpack2x16unorm(probe_split[word]);
    return vec4<f32>(shares, bitcast<f32>(probe_split[word + 1u]), bitcast<f32>(probe_split[word + 2u]));
}

fn linear_depth(d: f32) -> f32 {
    let n = params.proj.z;
    let f = params.proj.w;
    return n * f / max(f - d * (f - n), 1e-6);
}

// View-space position (camera looks down −z) of depth texel `coord`.
fn view_position(coord: vec2<i32>, size: vec2<i32>) -> vec3<f32> {
    let c = clamp(coord, vec2<i32>(0), size - vec2<i32>(1));
    let d = textureLoad(depth_tex, c, 0);
    let z = linear_depth(d);
    let uv = (vec2<f32>(c) + vec2<f32>(0.5)) / vec2<f32>(size);
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    return vec3<f32>(ndc.x * z / params.proj.x, ndc.y * z / params.proj.y, -z);
}

fn ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// Depth texel under the centre of AO texel `p` (AO target `size`, depth `depth_size`), in
// integer arithmetic: `(2p + 1) · depth / (2 · size)`. The float form
// `((p + 0.5) / size) · depth` lands exactly on a texel boundary at half resolution
// (2p + 1), and its rounding differs between drivers — NVIDIA picked the neighbouring depth
// texel at the box/ground edge and lost a third of the contact AO there (round 7).
// The exact half-resolution case is a shift (integer division costs 0.03–0.04 ms in the
// blur on RDNA3 at 2560×1440).
fn depth_texel_of(p: vec2<i32>, size: vec2<i32>, depth_size: vec2<i32>) -> vec2<i32> {
    var t = p * 2 + vec2<i32>(1);
    if (any(depth_size != size * 2)) {
        t = (t * depth_size) / (size * 2);
    }
    return clamp(t, vec2<i32>(0), depth_size - vec2<i32>(1));
}

// How far the reflection rays test (m): a room's far wall, a cockpit's other side.
const REFLECTION_REACH_M: f32 = 8.0;

// Whether view-space point `s` is hidden behind the depth buffer by an occluder close enough to
// stand in the ray's way: 1 hidden, 0 visible, 2 visible against the sky, -1 unknown (off
// screen or behind the eye).
// SSDO's visibility test (Ritschel, Grosch, Seidel, "Approximating Dynamic Global Illumination in
// Image Space", I3D 2009): a sample point under the depth buffer is a blocked direction. The
// visible surface must be at most `reach` in front of the point; something much nearer the eye
// (a pole in front of an open reflection) hides the point without blocking the ray.
fn hidden(s: vec3<f32>, depth_size: vec2<i32>, reach: f32) -> f32 {
    let z = -s.z;
    if (z <= params.proj.z) {
        return -1.0;
    }
    let ndc = vec2<f32>(s.x * params.proj.x / z, s.y * params.proj.y / z);
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    if (any(uv <= vec2<f32>(0.0)) || any(uv >= vec2<f32>(1.0))) {
        return -1.0;
    }
    let d = textureLoad(depth_tex, vec2<i32>(uv * vec2<f32>(depth_size)), 0);
    if (d >= 1.0) {
        return 2.0;
    }
    let behind = z - linear_depth(d);
    let eps = 0.03 + 0.01 * z;
    return select(0.0, 1.0, behind > eps && behind < reach);
}

// Whether the ray from `p` along `dir` leaves the visible enclosure within `REFLECTION_REACH_M`:
// `count` points at fixed distances growing quadratically from the surface (no per-pixel jitter,
// so a ray's answer only changes where the geometry does); the first hidden one blocks it.
// 1 = open, 0 = blocked. A point between two samples is not tested, but a wall the ray crosses
// there hides the next sample.
fn ray_open(p: vec3<f32>, dir: vec3<f32>, count: i32, depth_size: vec2<i32>) -> f32 {
    for (var i = 0; i < count; i = i + 1) {
        let u = (f32(i) + 1.0) / f32(count);
        let t = 0.1 + (REFLECTION_REACH_M - 0.1) * u * u;
        let h = hidden(p + dir * t, depth_size, 2.0 * t + 0.5);
        // Off screen, or seen against the sky: the ray is out in the open.
        if (h < 0.0 || h > 1.5) {
            return 1.0;
        }
        if (h > 0.5) {
            return 0.0;
        }
    }
    return 1.0;
}

// Reflection visibility: the fraction of the specular lobe of the depth surface (`p`, normal
// `n`, view space) whose rays leave the scene's enclosure, the visibility that specular
// occlusion stands for (Jimenez, Wu, Pesce, Jarabek, "Practical Realtime Strategies for Accurate
// Indirect Occlusion", SIGGRAPH 2016 § 5: the specular cone's visible part), here sampled with
// rays instead of intersected with an AO cone, because the cone of a contact-scale AO kernel
// never sees a cockpit's or a room's far walls. x = the mirror direction (a glossy lobe; five
// points), y = the mirror direction and three rays 30° around it (three points each; a rough
// lobe): at most 14 depth loads, fewer where a ray is blocked or reaches the sky early.
fn reflection_visibility(p: vec3<f32>, n: vec3<f32>, rotation: f32, depth_size: vec2<i32>) -> vec2<f32> {
    let v = normalize(-p);
    var r = reflect(-v, n);
    // Keep the rays above the tangent plane (a grazing mirror ray would start inside it).
    r = normalize(r + n * max(0.1 - dot(r, n), 0.0));
    let centre = ray_open(p, r, 5, depth_size);
    let helper = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), abs(r.x) > 0.7);
    let a = normalize(cross(r, helper));
    let b = cross(r, a);
    var wide = centre;
    for (var k = 0; k < 3; k = k + 1) {
        let phi = rotation + f32(k) * 2.0943951;
        // 30° off the mirror direction.
        var d = r * 0.8660254 + (a * cos(phi) + b * sin(phi)) * 0.5;
        d = normalize(d + n * max(0.1 - dot(d, n), 0.0));
        wide += ray_open(p, d, 3, depth_size);
    }
    return vec2<f32>(centre, wide * 0.25);
}

@compute @workgroup_size(8, 8)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<u32>(textureDimensions(out_tex));
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let coord = depth_texel_of(vec2<i32>(gid.xy), vec2<i32>(size), depth_size);
    let d = textureLoad(depth_tex, coord, 0);
    if (d >= 1.0) {
        store_texel(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    // The last depth column and row have no neighbour on their far side: the clamped tap is the
    // texel itself, and the one-sided difference below is only zero when the two inlined
    // `view_position` calls round alike. The RTX 3080 Ti's compiler left a residue of about 1e-8
    // there, which passed the degenerate test and gave those texels a random normal (a dark
    // bent-normal fringe along the right edge, ADR-1626). Decided on the coordinate instead:
    // no occlusion, as the exact zero always gave.
    if (any(coord + vec2<i32>(1) >= depth_size)) {
        store_texel(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    let p = view_position(coord, depth_size);
    // Normal from the smaller of the two one-sided differences per axis, so a
    // silhouette edge does not tilt the normal of the surface in front of it.
    let px0 = view_position(coord - vec2<i32>(1, 0), depth_size);
    let px1 = view_position(coord + vec2<i32>(1, 0), depth_size);
    let py0 = view_position(coord - vec2<i32>(0, 1), depth_size);
    let py1 = view_position(coord + vec2<i32>(0, 1), depth_size);
    let dx = select(px1 - p, p - px0, abs(p.z - px0.z) < abs(px1.z - p.z));
    let dy = select(py1 - p, p - py0, abs(p.z - py0.z) < abs(py1.z - p.z));
    let c = cross(dy, dx);
    let c_len2 = dot(c, c);
    if (!(c_len2 > 1e-24)) {
        // Degenerate depth neighbourhood (far-plane precision): no occlusion.
        store_texel(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    var n = c * inverseSqrt(c_len2);
    if (dot(n, -p) < 0.0) {
        n = -n;
    }

    let radius = params.screen.z;
    let samples = u32(clamp(params.screen.w, 4.0, 32.0));
    // World radius projected into AO-target pixels at this depth.
    let radius_px = clamp(radius * params.proj.y * 0.5 * f32(size.y) / max(-p.z, 1e-3), 1.5, params.mode.w);
    let to_depth = vec2<f32>(depth_size) / vec2<f32>(size);
#ifdef BLUE_NOISE
    let rotation = blue_rotation(gid.xy, u32(params.noise.x)) * 6.2831853;
#else
    let rotation = ign(vec2<f32>(gid.xy) + 5.588238 * params.noise.x) * 6.2831853;
#endif
    let bias = params.mode.y;
    let split = probe_split_at(coord, depth_size);
    var occlusion = 0.0;
    // The same obscurance weighted for the brick's and the terrain layer's share of the light.
    var occ_brick = 0.0;
    var occ_terrain = 0.0;
    // Without a split (rest = 1, shares 0) the whole kernel.
    let rest = max(1.0 - split.x - split.y, 0.0);
    // Σ obscurance × the share no probe holds, and the same along each sample's direction.
    var lean_share = 0.0;
    var lean_dir = vec3<f32>(0.0);
    let bent_on = BENT_NORMALS;
    for (var i: u32 = 0u; i < samples; i = i + 1u) {
        let t = (f32(i) + 0.5) / f32(samples);
        let angle = f32(i) * 2.399963 + rotation; // golden angle spiral
        let offset = vec2<f32>(cos(angle), sin(angle)) * radius_px * t;
        let s_coord = vec2<i32>((vec2<f32>(gid.xy) + vec2<f32>(0.5) + offset) * to_depth);
        let s = view_position(s_coord, depth_size);
        let v = s - p;
        let vv = dot(v, v);
        let falloff = clamp(1.0 - vv / (radius * radius), 0.0, 1.0);
        let cos_rise = dot(v, n) * inverseSqrt(max(vv, 1e-8));
        let o = max(cos_rise - bias, 0.0) * falloff;
        occlusion += o;
        let height = max(dot(v, n), 0.0);
        let keep_brick = probe_ao_keep(height, split.z);
        let keep_terrain = probe_ao_keep(height, split.w);
        occ_brick += o * keep_brick;
        occ_terrain += o * keep_terrain;
        if (bent_on) {
            let kept = o * (rest + split.x * keep_brick + split.y * keep_terrain);
            lean_share += kept;
            lean_dir += v * (kept * inverseSqrt(max(vv, 1e-8)));
        }
    }
    // Without a split this is the whole kernel's value exactly.
    let n_samples = f32(samples);
    let ao = rest * clamp(1.0 - occlusion / n_samples * params.mode.z, 0.0, 1.0)
        + split.x * clamp(1.0 - occ_brick / n_samples * params.mode.z, 0.0, 1.0)
        + split.y * clamp(1.0 - occ_terrain / n_samples * params.mode.z, 0.0, 1.0);
    var refl = vec2<f32>(1.0);
    // Traced on a fixed checkerboard of AO texels (`traced`); the blur fills the others.
    if (params.noise.y > 0.5 && traced(vec2<i32>(gid.xy))) {
        refl = reflection_visibility(p, n, rotation, depth_size);
    }
    var lean = vec3<f32>(0.0);
    if (bent_on) {
        let k = params.mode.z / n_samples;
        lean = to_world(bent_normal_lean(n, lean_dir * k, lean_share * k));
    }
    store_texel(vec2<i32>(gid.xy), vec3<f32>(ao, refl), lean);
}

// BEGIN horizon AO (ADR-3202, `RenderProfile::horizon_ao`; `SsaoPass` runs this entry point instead
// of `cs_main` on the GTAO rows when it is on). Ground-truth-based ambient occlusion: Jimenez, Wu,
// Pesce, Jarabek, "Practical Realtime Strategies for Accurate Indirect Occlusion", SIGGRAPH 2016.
// Per slice (a screen direction and the view vector) the highest occluder on each side is found
// along the depth buffer, and the cosine-weighted arc between the two horizons is integrated in
// closed form, with the normal projected into the slice. The point kernel above counts each sample
// by its own direction only, so a tall occluder seen from above (a box's top beside its foot) was
// one far sample, and its distance falloff (1 m against the 1.2 m radius) dropped most of it: the
// spec 27 box darkened its foot 9.5 % against the tracer's 29.9 % (`ao_contact`).
//
// Column falloff (this file's own): a depth sample stands for a column behind it, away from the
// eye, `HORIZON_THICKNESS` radii thick (the paper's thickness heuristic). A sample counts with the
// highest point of its column that lies within the world radius of the receiver, so a wall beside
// the receiver occludes up to the direction whose ray meets it at the radius, however far its top
// edge is, and a pole far in front of the receiver (its column ends before the radius) does not.
// Multiple bounces are the forward pass's (`multi_bounce_ao`, the same paper's fit).

// Slices per texel and steps per side of each slice.
const HORIZON_SLICES: u32 = 4u;
const HORIZON_STEPS: u32 = 4u;
// Depth of each sample's column behind it, in world radii.
const HORIZON_THICKNESS: f32 = 1.0;
const HALF_PI: f32 = 1.5707963;

// The cosine (to `v_dir`, the receiver's view direction) of the highest point within `radius` of
// the receiver on the column behind sample offset `v` (sample minus receiver, view space), or -1
// when no point of the column is in reach. Exposed for the CPU mirror in
// `tests/ao_contact_band.rs`.
fn column_horizon_cos(v: vec3<f32>, v_dir: vec3<f32>, radius: f32) -> f32 {
    let a = dot(v, v_dir);
    let lateral2 = max(dot(v, v) - a * a, 0.0);
    let r2 = radius * radius;
    if (lateral2 >= r2) {
        return -1.0;
    }
    // The column runs from the sample away from the eye: v - t · v_dir, t in [0, thickness].
    let t = max(a - sqrt(r2 - lateral2), 0.0);
    if (t > HORIZON_THICKNESS * radius) {
        return -1.0;
    }
    let along = a - t;
    return along * inverseSqrt(max(lateral2 + along * along, 1e-12));
}

// The cosine-weighted visible arc of one side of a slice up to horizon angle `h` (signed, from the
// view vector) for the projected normal at angle `n` (the paper's inner integral, one side).
fn horizon_arc(h: f32, n: f32) -> f32 {
    return 0.25 * (-cos(2.0 * h - n) + cos(n) + 2.0 * h * sin(n));
}

@compute @workgroup_size(8, 8)
fn cs_horizon(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<u32>(textureDimensions(out_tex));
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let coord = depth_texel_of(vec2<i32>(gid.xy), vec2<i32>(size), depth_size);
    let d = textureLoad(depth_tex, coord, 0);
    if (d >= 1.0 || any(coord + vec2<i32>(1) >= depth_size)) {
        store_texel(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    // The receiver and its depth normal, as `cs_main` reconstructs them.
    let p = view_position(coord, depth_size);
    let px0 = view_position(coord - vec2<i32>(1, 0), depth_size);
    let px1 = view_position(coord + vec2<i32>(1, 0), depth_size);
    let py0 = view_position(coord - vec2<i32>(0, 1), depth_size);
    let py1 = view_position(coord + vec2<i32>(0, 1), depth_size);
    let dx = select(px1 - p, p - px0, abs(p.z - px0.z) < abs(px1.z - p.z));
    let dy = select(py1 - p, p - py0, abs(p.z - py0.z) < abs(py1.z - p.z));
    let c = cross(dy, dx);
    let c_len2 = dot(c, c);
    if (!(c_len2 > 1e-24)) {
        store_texel(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    var n = c * inverseSqrt(c_len2);
    if (dot(n, -p) < 0.0) {
        n = -n;
    }
    let v_dir = normalize(-p);
    let radius = params.screen.z;
    let radius_px = clamp(radius * params.proj.y * 0.5 * f32(size.y) / max(-p.z, 1e-3), 1.5, params.mode.w);
    let to_depth = vec2<f32>(depth_size) / vec2<f32>(size);
#ifdef BLUE_NOISE
    let rotation = blue_rotation(gid.xy, u32(params.noise.x)) * 6.2831853;
#else
    let rotation = ign(vec2<f32>(gid.xy) + 5.588238 * params.noise.x) * 6.2831853;
#endif
    // The step jitter: a second, decorrelated value from the same noise.
    let jitter = fract(rotation * 2.6180340);
    let bias = params.mode.y;
    let split = probe_split_at(coord, depth_size);
    let rest = max(1.0 - split.x - split.y, 0.0);
    var visible = 0.0;
    var vis_brick = 0.0;
    var vis_terrain = 0.0;
    var lean_dir = vec3<f32>(0.0);
    var lean_share = 0.0;
    let bent_on = BENT_NORMALS;
    for (var slice: u32 = 0u; slice < HORIZON_SLICES; slice = slice + 1u) {
        let phi = (f32(slice) + rotation / 6.2831853) * (3.1415927 / f32(HORIZON_SLICES));
        let dir_px = vec2<f32>(cos(phi), sin(phi));
        // Screen y runs down, view y up.
        let omega = vec3<f32>(dir_px.x, -dir_px.y, 0.0);
        let t_dir = normalize(omega - v_dir * dot(omega, v_dir));
        let axis = cross(t_dir, v_dir);
        let n_proj = n - axis * dot(n, axis);
        let n_len = length(n_proj);
        if (!(n_len > 1e-4)) {
            continue;
        }
        let cos_n = clamp(dot(n_proj, v_dir) / n_len, -1.0, 1.0);
        let n_ang = select(-1.0, 1.0, dot(n_proj, t_dir) >= 0.0) * acos(cos_n);
        // Highest occluder cosine on the + and − sides, and the offset to it.
        var best = vec2<f32>(-1.0);
        var best_plus = vec3<f32>(0.0);
        var best_minus = vec3<f32>(0.0);
        for (var k: u32 = 0u; k < HORIZON_STEPS; k = k + 1u) {
            let t = (f32(k) + jitter) / f32(HORIZON_STEPS);
            let dist_px = max(t * radius_px, f32(k) + 1.0);
            for (var side: i32 = 0; side < 2; side = side + 1) {
                let sgn = select(-1.0, 1.0, side == 0);
                let s_coord = vec2<i32>((vec2<f32>(gid.xy) + vec2<f32>(0.5) + dir_px * (sgn * dist_px)) * to_depth);
                let v = view_position(s_coord, depth_size) - p;
                let vv = dot(v, v);
                // The tangent-plane bias of `cs_main`: points within a few degrees of the plane
                // are the receiver's own surface.
                if (dot(v, n) * inverseSqrt(max(vv, 1e-8)) <= bias) {
                    continue;
                }
                let h = column_horizon_cos(v, v_dir, radius);
                if (side == 0 && h > best.x) {
                    best.x = h;
                    best_plus = v;
                }
                if (side == 1 && h > best.y) {
                    best.y = h;
                    best_minus = v;
                }
            }
        }
        // Signed horizon angles, clamped to the hemisphere about the projected normal.
        let h_plus = n_ang + min(acos(clamp(best.x, -1.0, 1.0)) - n_ang, HALF_PI);
        let h_minus = n_ang + max(-acos(clamp(best.y, -1.0, 1.0)) - n_ang, -HALF_PI);
        let open_plus = horizon_arc(n_ang + HALF_PI, n_ang);
        let open_minus = horizon_arc(n_ang - HALF_PI, n_ang);
        // Each side's term is its arc from the view vector out to its horizon (the paper's two
        // terms; both positive).
        let vis_plus = horizon_arc(h_plus, n_ang);
        let vis_minus = horizon_arc(h_minus, n_ang);
        let blocked_plus = max(open_plus - vis_plus, 0.0) * n_len;
        let blocked_minus = max(open_minus - vis_minus, 0.0) * n_len;
        let slice_vis = (vis_plus + vis_minus) * n_len;
        visible += slice_vis;
        // Probe-relative split: each side's blocked share is weighted by what the probes do not
        // hold at its horizon point's height (`probe_ao_keep`).
        let height_plus = max(dot(best_plus, n), 0.0);
        let height_minus = max(dot(best_minus, n), 0.0);
        let keep_b = vec2<f32>(probe_ao_keep(height_plus, split.z), probe_ao_keep(height_minus, split.z));
        let keep_t = vec2<f32>(probe_ao_keep(height_plus, split.w), probe_ao_keep(height_minus, split.w));
        let open_slice = slice_vis + blocked_plus + blocked_minus;
        vis_brick += open_slice - blocked_plus * keep_b.x - blocked_minus * keep_b.y;
        vis_terrain += open_slice - blocked_plus * keep_t.x - blocked_minus * keep_t.y;
        if (bent_on) {
            let kept_plus = blocked_plus * (rest + split.x * keep_b.x + split.y * keep_t.x);
            let kept_minus = blocked_minus * (rest + split.x * keep_b.y + split.y * keep_t.y);
            lean_share += kept_plus + kept_minus;
            lean_dir += best_plus * (kept_plus * inverseSqrt(max(dot(best_plus, best_plus), 1e-8)))
                + best_minus * (kept_minus * inverseSqrt(max(dot(best_minus, best_minus), 1e-8)));
        }
    }
    let k = 1.0 / f32(HORIZON_SLICES);
    // `mode.z` scales the occluded share (the post volume's AO strength; 1 = as integrated).
    let strength = params.mode.z;
    let ao = rest * clamp(1.0 - (1.0 - visible * k) * strength, 0.0, 1.0)
        + split.x * clamp(1.0 - (1.0 - vis_brick * k) * strength, 0.0, 1.0)
        + split.y * clamp(1.0 - (1.0 - vis_terrain * k) * strength, 0.0, 1.0);
    var refl = vec2<f32>(1.0);
    if (params.noise.y > 0.5 && traced(vec2<i32>(gid.xy))) {
        refl = reflection_visibility(p, n, rotation, depth_size);
    }
    var lean = vec3<f32>(0.0);
    if (bent_on) {
        lean = to_world(bent_normal_lean(n, lean_dir * (k * strength), lean_share * (k * strength)));
    }
    store_texel(vec2<i32>(gid.xy), vec3<f32>(ao, refl), lean);
}
// END horizon AO

// Whether AO texel `p` traces the reflection visibility: half of them, on a fixed checkerboard
// (the rays are most of the kernel's cost; the visibility is low-frequency). Fixed, not
// alternating per frame, so a still frame is stable without temporal AA.
fn traced(p: vec2<i32>) -> bool {
    return ((p.x + p.y) & 1) == 0;
}

// 4×4 depth-aware box blur of `raw_tex` (AO and the two reflection visibilities, and the bent
// normal's lean when `noise.z` = 1) into `out_tex` (same size). The reflection visibilities
// average only the traced texels (8 of the 16). Tap by tap: the pipeline for bent normals off;
// with them on `cs_blur_tiled` gives the same bytes faster (ADR-1491).
@compute @workgroup_size(8, 8)
fn cs_blur(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<i32>(textureDimensions(out_tex));
    let pix = vec2<i32>(gid.xy);
    if (pix.x >= size.x || pix.y >= size.y) {
        return;
    }
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let centre = view_position(depth_texel_of(pix, size, depth_size), depth_size).z;
    // The sums start from `noise.w` (0), not a literal 0: unrolled, a literal lets the compiler
    // fold the first tap's `0 + t · w` to a product and fuse the first two taps the other way
    // round (`fma(t0, w0, t1 · w1)`, seen on Mesa's Intel compiler), where a loop sums
    // `fma(t, w, sum)` tap by tap. Both blurs then match however the driver unrolls (ADR-1560).
    let zero = params.noise.w;
    var sum = vec3<f32>(zero);
    var lean_sum = vec3<f32>(zero);
    var weight = zero;
    var refl_weight = zero;
    let bent_on = BENT_NORMALS;
    for (var y: i32 = -2; y <= 1; y = y + 1) {
        for (var x: i32 = -2; x <= 1; x = x + 1) {
            let q = clamp(pix + vec2<i32>(x, y), vec2<i32>(0), size - vec2<i32>(1));
            let z = view_position(depth_texel_of(q, size, depth_size), depth_size).z;
            let w = 1.0 / (1e-3 + abs(z - centre) / max(abs(centre) * 0.02, 1e-3));
            let t = textureLoad(raw_tex, q, 0, 0).rgb;
            let wr = select(0.0, w, traced(q) || params.noise.y < 0.5);
            sum += vec3<f32>(t.r * w, t.gb * wr);
            if (bent_on) {
                lean_sum += bent_decode(textureLoad(raw_tex, q, 1, 0).rgb) * w;
            }
            weight += w;
            refl_weight += wr;
        }
    }
    let ao = sum.r / max(weight, 1e-6);
    let refl = select(vec2<f32>(1.0), sum.gb / max(refl_weight, 1e-6), refl_weight > 0.0);
    store_texel(pix, vec3<f32>(ao, refl), lean_sum / max(weight, 1e-6));
}

// `cs_blur` from a workgroup tile, the pipeline for bent normals on (ADR-1491). Each 8×8
// workgroup first loads the 11×11 texels its taps reach (2 before, 1 after: depth, layer-0
// value, decoded lean) into workgroup memory once each, instead of once per tap, and the taps
// read the tile: it saves 15 of every 16 lean loads and decodes. Same values, same order of the
// sums: the bytes match `cs_blur` (`tests/ssao_blur_tile.rs`). Without the lean the per-tap form
// is faster on the RTX 5090, and both forms behind one branch slowed the per-tap one, hence two
// entry points.
//
// Same arithmetic, not only the same formulas (ADR-1560): drivers contract `cs_blur`'s view depth
// into the weight's difference (`z - centre` becomes `fma(-n·f, 1 / (f - d·(f - n)), -centre)`,
// one rounding fewer), so the tile holds the raw depth and each tap computes its view depth as
// `cs_blur` does; a view depth stored to the tile is rounded first (one LSB apart on the RTX
// 5090). The centre is loaded from the depth texture as in `cs_blur`, not from the tile: the
// centre tap would read the same tile word, and a compiler that sees that folds its
// `z - centre` to 0 where `cs_blur` gets the product's rounding error (Mesa's Intel compiler:
// a centre weight of 1000).
const BLUR_TILE: i32 = 11;
var<workgroup> blur_d: array<f32, 121>;
var<workgroup> blur_t: array<vec3<f32>, 121>;
var<workgroup> blur_lean: array<vec3<f32>, 121>;

@compute @workgroup_size(8, 8)
fn cs_blur_tiled(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    let size = vec2<i32>(textureDimensions(out_tex));
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let bent_on = BENT_NORMALS;
    // The tile's first texel: two before the workgroup's first pixel.
    let origin = vec2<i32>(wid.xy) * 8 - vec2<i32>(2);
    // Two rounds of 64 invocations cover the 121 tile texels (a fixed trip count: the barrier
    // below stays in uniform control flow). Texels past the edges clamp as the taps do.
    for (var j = 0; j < 2; j = j + 1) {
        let k = i32(lid) + j * 64;
        if (k < BLUR_TILE * BLUR_TILE) {
            let q = clamp(origin + vec2<i32>(k % BLUR_TILE, k / BLUR_TILE), vec2<i32>(0), size - vec2<i32>(1));
            blur_d[k] = textureLoad(depth_tex, depth_texel_of(q, size, depth_size), 0);
            blur_t[k] = textureLoad(raw_tex, q, 0, 0).rgb;
            if (bent_on) {
                blur_lean[k] = bent_decode(textureLoad(raw_tex, q, 1, 0).rgb);
            }
        }
    }
    workgroupBarrier();
    let pix = vec2<i32>(gid.xy);
    if (pix.x >= size.x || pix.y >= size.y) {
        return;
    }
    let local = pix - origin;
    let centre = view_position(depth_texel_of(pix, size, depth_size), depth_size).z;
    // From `noise.w` as in `cs_blur`.
    let zero = params.noise.w;
    var sum = vec3<f32>(zero);
    var lean_sum = vec3<f32>(zero);
    var weight = zero;
    var refl_weight = zero;
    for (var y: i32 = -2; y <= 1; y = y + 1) {
        for (var x: i32 = -2; x <= 1; x = x + 1) {
            let q = clamp(pix + vec2<i32>(x, y), vec2<i32>(0), size - vec2<i32>(1));
            let k = (local.y + y) * BLUR_TILE + local.x + x;
            let z = -linear_depth(blur_d[k]);
            let w = 1.0 / (1e-3 + abs(z - centre) / max(abs(centre) * 0.02, 1e-3));
            let t = blur_t[k];
            let wr = select(0.0, w, traced(q) || params.noise.y < 0.5);
            sum += vec3<f32>(t.r * w, t.gb * wr);
            if (bent_on) {
                lean_sum += blur_lean[k] * w;
            }
            weight += w;
            refl_weight += wr;
        }
    }
    let ao = sum.r / max(weight, 1e-6);
    let refl = select(vec2<f32>(1.0), sum.gb / max(refl_weight, 1e-6), refl_weight > 0.0);
    store_texel(pix, vec3<f32>(ao, refl), lean_sum / max(weight, 1e-6));
}
