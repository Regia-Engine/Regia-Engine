// SPDX-License-Identifier: MPL-2.0
// GPU irradiance probes (DDGI): layouts, probe addressing and the shading lookup.
//
// This file has no bindings. A host shader declares, under these names:
//   ddgi: DdgiUniform (uniform)
//   ddgi_data: array<u32> (storage; read in shading, read_write in the update passes)
// and then appends this text and ddgi_sample.wgsl. `ddgi_data` holds, per probe slot:
//   probe record (12 words: offset xyz + active, world cell xyz + updates, frame stamp),
//   irradiance tile (8×8 texels, 6×6 interior + octahedral border, rgb f32 in 3 words; the 16 interior texels
//   next to a coordinate axis hold that axis's lobe, at the probe or at the surface behind it:
//   `ddgi_irr_texel_dir`, `ddgi_axis_texels`; 24 texels of the border ring hold the irradiance in the creases
//   of those surfaces: `ddgi_crease_texel`, ADR-5552),
//   distance-moment tile (16×16 texels, 14×14 interior + border, mean and mean² as unorm16
//   of the cascade's distance limit; no f16 packing, which naga gates behind a capability).
// Storage words rather than textures keep the forward shader within 16 sampled textures. Majercik et al., "Dynamic Diffuse Global Illumination with
// Ray-Traced Irradiance Fields", JCGT 8(2) 2019 (octahedral irradiance and distance moments,
// Chebyshev visibility, wrap and trilinear weights); Majercik, Marrs, Spjut, McGuire,
// "Scaling Probe-Based Real-Time Dynamic Global Illumination for Production", JCGT 10(2)
// 2021 (camera-relative scrolling volumes, probe relocation and classification).
// Octahedral mapping: Cigolle et al., JCGT 3(2) 2014.

const DDGI_IRR_RES: u32 = 6u;
const DDGI_IRR_TILE: u32 = 8u;
const DDGI_VIS_RES: u32 = 14u;
const DDGI_VIS_TILE: u32 = 16u;

struct DdgiCascade {
    // xyz = the volume's lowest corner before it snaps to whole cells, in cells (`base` plus
    // the eye's position inside its cell; `ddgi_blend_inset`), w = probe spacing (m).
    origin: vec4<f32>,
    // xyz = world cell of the volume's lowest corner probe.
    base: vec4<i32>,
}

struct DdgiUniform {
    cas_a: DdgiCascade,
    cas_b: DdgiCascade,
    cas_c: DdgiCascade,
    cas_d: DdgiCascade,
    // xyz = probes per axis, w = cascade count.
    dims: vec4<u32>,
    // x = hysteresis, y = normal bias (× spacing), z = view bias (× spacing), w = intensity.
    params: vec4<f32>,
    // x = rays per probe, y = frame stamp, z = update divisor, w = flags
    // (1 = shading enabled, 2 = heightfield bound, 4 = exact screen lookup (diagnostic),
    // 8 = the screen lookup is the baked volume's and holds the sky-body fill (`baked_gi.wgsl`),
    // bits 8..11 = log2 of the inactive probes' re-trace slowdown, 4096 = the light jumped,
    // bit 25 = the image light's ground plane is on, `ddgi_trace.wgsl` `miss_radiance`).
    ctrl: vec4<u32>,
    // xyz = the rows of this frame's ray rotation. w = the image light's ground plane (bit 25 of
    // `ctrl.w`, ADR-5716): its height (`rot_x.w`), its radius, 0 for unbounded (`rot_y.w`) and its
    // centre's x (`rot_z.w`).
    rot_x: vec4<f32>,
    rot_y: vec4<f32>,
    rot_z: vec4<f32>,
    // xyz = direction toward the sun, w > 0 when the sun is on.
    to_sun: vec4<f32>,
    // rgb = sun illuminance (lux × colour); w = the ground plane's centre z.
    sun: vec4<f32>,
    // x = sky map scale (0 = flat sky), y = sky map lod, z = max terrain ray (m),
    // w = visibility distance limit (× spacing).
    sky: vec4<f32>,
    // rgb = flat sky radiance when no sky map is bound; w = min frontface distance (× spacing).
    flat_sky: vec4<f32>,
    // Per cascade: SDF instance list offset / count.
    inst_offset: vec4<u32>,
    inst_count: vec4<u32>,
    // x = first irradiance word (f32 history), y = first distance word, z = first shading
    // header word, w = first packed (RGB9E5) irradiance word.
    words: vec4<u32>,
    // Screen lookup: x = first word, y/z = lookup width/height, w = pixels per lookup texel
    // along each axis (2 = half resolution, the default; 1 or 4 measure it, ADR-1033), 0 = off.
    screen: vec4<u32>,
    // x = emitters in the emitter list (`ddgi_blend.wgsl`), y = f32 bits of the short-range
    // emissive pass's range (0: none), z / w = f32 bits of the ground plane's radiance, green and blue.
    emitters: vec4<u32>,
    // xyz = direction toward the sky body's planet-shine fill, w > 0 when it is on.
    to_fill: vec4<f32>,
    // rgb = the fill's illuminance (lux × tint), unshadowed as in the forward pass; w = the ground
    // plane's radiance, red.
    fill: vec4<f32>,
    // The frame's extra directional lights (ADR-7254): per slot the direction toward the light
    // (w > 0 when on), then illuminance × colour (lux). Unshadowed, as the forward shades them.
    extra_dir: array<vec4<f32>, 6>,
}

const DDGI_PROBE_WORDS: u32 = 12u;
const DDGI_IRR_WORDS: u32 = 192u;
const DDGI_VIS_WORDS: u32 = 256u;
const DDGI_HEADER_WORDS: u32 = 2u;
const DDGI_PACKED_IRR_WORDS: u32 = 64u;
const DDGI_SCREEN_WORDS: u32 = 8u;
// `ctrl.w` bit: the screen lookup holds the capsule occluders' word per texel (`baked_gi_screen.wgsl`,
// `CTRL_BAKED_OCCLUDER_VIS`, ADR-2656).
const DDGI_CTRL_OCCLUDER_VIS: u32 = 32768u;


fn ddgi_cascade(c: u32) -> DdgiCascade {
    switch c {
        case 0u: { return ddgi.cas_a; }
        case 1u: { return ddgi.cas_b; }
        case 2u: { return ddgi.cas_c; }
        default: { return ddgi.cas_d; }
    }
}

fn ddgi_pick(v: vec4<u32>, c: u32) -> u32 {
    switch c {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

fn ddgi_count() -> u32 {
    return ddgi.dims.x * ddgi.dims.y * ddgi.dims.z;
}

fn ddgi_slot(c: u32, cell: vec3<i32>) -> u32 {
    let d = vec3<i32>(ddgi.dims.xyz);
    let s = ((cell % d) + d) % d;
    return c * ddgi_count() + u32(s.x + d.x * (s.y + d.y * s.z));
}

// World cell held by probe slot `index` (the cell ≡ slot mod dims inside the volume).
fn ddgi_slot_cell(index: u32) -> vec3<i32> {
    let count = ddgi_count();
    let c = index / count;
    let l = index % count;
    let d = vec3<i32>(ddgi.dims.xyz);
    let s = vec3<i32>(i32(l % ddgi.dims.x), i32((l / ddgi.dims.x) % ddgi.dims.y), i32(l / (ddgi.dims.x * ddgi.dims.y)));
    let base = ddgi_cascade(c).base.xyz;
    let rel = (((s - base) % d) + d) % d;
    return base + rel;
}

fn ddgi_probe_offset(index: u32) -> vec4<f32> {
    let b = index * DDGI_PROBE_WORDS;
    return bitcast<vec4<f32>>(vec4<u32>(ddgi_data[b], ddgi_data[b + 1u], ddgi_data[b + 2u], ddgi_data[b + 3u]));
}

fn ddgi_probe_tag(index: u32) -> vec4<i32> {
    let b = index * DDGI_PROBE_WORDS + 4u;
    return bitcast<vec4<i32>>(vec4<u32>(ddgi_data[b], ddgi_data[b + 1u], ddgi_data[b + 2u], ddgi_data[b + 3u]));
}

fn ddgi_probe_stamp(index: u32) -> u32 {
    return ddgi_data[index * DDGI_PROBE_WORDS + 8u];
}

// Word of irradiance texel (x, y) of a probe tile (border-padded, 8×8).
fn ddgi_irr_word(index: u32, t: vec2<u32>) -> u32 {
    return ddgi.words.x + index * DDGI_IRR_WORDS + (t.y * DDGI_IRR_TILE + t.x) * 3u;
}

// Word of distance texel (x, y) of a probe tile (border-padded, 16×16).
fn ddgi_vis_word(index: u32, t: vec2<u32>) -> u32 {
    return ddgi.words.y + index * DDGI_VIS_WORDS + t.y * DDGI_VIS_TILE + t.x;
}

// Shared-exponent RGB (9-bit mantissas, 5-bit exponent, bias 15): the format of
// EXT_texture_shared_exponent, packed by hand into one storage word.
fn ddgi_pack_rgb9e5(c: vec3<f32>) -> u32 {
    let m = clamp(c, vec3<f32>(0.0), vec3<f32>(65408.0));
    let maxc = max(m.r, max(m.g, m.b));
    var e = max(-16, i32(floor(log2(max(maxc, 1e-30))))) + 16;
    var denom = exp2(f32(e - 24));
    if (floor(maxc / denom + 0.5) >= 512.0) {
        denom = denom * 2.0;
        e = e + 1;
    }
    let q = vec3<u32>(min(floor(m / denom + vec3<f32>(0.5)), vec3<f32>(511.0)));
    return q.r | (q.g << 9u) | (q.b << 18u) | (u32(clamp(e, 0, 31)) << 27u);
}

fn ddgi_unpack_rgb9e5(v: u32) -> vec3<f32> {
    let s = exp2(f32(i32(v >> 27u) - 24));
    return vec3<f32>(f32(v & 511u), f32((v >> 9u) & 511u), f32((v >> 18u) & 511u)) * s;
}

// Shading header of a probe: word 0 = world cell mod 1024 per axis (10 bits each) and a
// "traced" bit 31; word 1 = relocation offset / spacing + 0.5 per axis (10 bits each) and
// an "active" bit 31.
fn ddgi_header_word(index: u32) -> u32 {
    return ddgi.words.z + index * DDGI_HEADER_WORDS;
}

fn ddgi_pack_cell(cell: vec3<i32>, traced: bool) -> u32 {
    let c = vec3<u32>(cell & vec3<i32>(1023));
    return c.x | (c.y << 10u) | (c.z << 20u) | select(0u, 1u << 31u, traced);
}

fn ddgi_pack_offset(o: vec3<f32>, s: f32, is_active: bool) -> u32 {
    let q = vec3<u32>(round(clamp(o / s + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(1.0)) * 1023.0));
    return q.x | (q.y << 10u) | (q.z << 20u) | select(0u, 1u << 31u, is_active);
}

fn ddgi_unpack_offset(v: u32, s: f32) -> vec3<f32> {
    let q = vec3<f32>(f32(v & 1023u), f32((v >> 10u) & 1023u), f32((v >> 20u) & 1023u));
    return (q / 1023.0 - vec3<f32>(0.5)) * s;
}

fn ddgi_packed_irr_load(index: u32, t: vec2<u32>) -> vec3<f32> {
    return ddgi_unpack_rgb9e5(ddgi_data[ddgi.words.w + index * DDGI_PACKED_IRR_WORDS + t.y * DDGI_IRR_TILE + t.x]);
}

fn ddgi_irr_load(index: u32, t: vec2<u32>) -> vec3<f32> {
    let w = ddgi_irr_word(index, t);
    return bitcast<vec3<f32>>(vec3<u32>(ddgi_data[w], ddgi_data[w + 1u], ddgi_data[w + 2u]));
}

// Distance moments are stored relative to this limit (metres).
fn ddgi_vis_limit(index: u32) -> f32 {
    return ddgi_cascade(index / ddgi_count()).origin.w * ddgi.sky.w;
}

fn ddgi_unorm16x2(v: u32) -> vec2<f32> {
    return vec2<f32>(f32(v & 0xffffu), f32(v >> 16u)) / 65535.0;
}

fn ddgi_pack_unorm16x2(v: vec2<f32>) -> u32 {
    let a = u32(round(clamp(v.x, 0.0, 1.0) * 65535.0));
    let b = u32(round(clamp(v.y, 0.0, 1.0) * 65535.0));
    return a | (b << 16u);
}

// (mean, mean²) of the hit distance in metres.
fn ddgi_vis_load(index: u32, t: vec2<u32>) -> vec2<f32> {
    let limit = ddgi_vis_limit(index);
    let m = ddgi_unorm16x2(ddgi_data[ddgi_vis_word(index, t)]);
    return vec2<f32>(m.x * limit, m.y * limit * limit);
}

fn ddgi_sign_nz(v: vec2<f32>) -> vec2<f32> {
    return select(vec2<f32>(-1.0), vec2<f32>(1.0), v >= vec2<f32>(0.0));
}

// Unit direction → [-1, 1]² (Cigolle et al. 2014).
fn ddgi_oct_encode(dir: vec3<f32>) -> vec2<f32> {
    let n = dir / max(abs(dir.x) + abs(dir.y) + abs(dir.z), 1e-6);
    if (n.z >= 0.0) {
        return n.xy;
    }
    return (vec2<f32>(1.0) - abs(n.yx)) * ddgi_sign_nz(n.xy);
}

fn ddgi_oct_decode(e: vec2<f32>) -> vec3<f32> {
    var v = vec3<f32>(e, 1.0 - abs(e.x) - abs(e.y));
    if (v.z < 0.0) {
        let xy = (vec2<f32>(1.0) - abs(v.yx)) * ddgi_sign_nz(v.xy);
        v = vec3<f32>(xy, v.z);
    }
    return normalize(v);
}

// `ctrl.w` bit 16: `DdgiSettings::visibility_blend` (ADR-2605: how a coarser cascade's light is kept or capped
// where the finest one sees the point, `ddgi_sample.wgsl` `ddgi_vis_blend_on`). Every tier sets it.
const DDGI_CTRL_VISIBILITY_BLEND: u32 = 65536u;

// The lookup of ADR-5374 and ADR-5457: probes weighted at the surface point (`ddgi_sample.wgsl`
// `ddgi_surface_weights`), axis-exact irradiance texels (`ddgi_irr_texel_dir`), the irradiance at the surface
// behind a probe in the axis texels of odd parity (`ddgi_axis_texels`) and, since ADR-5506, each face
// interpolated between the probes as its cube root (`ddgi_blend_root`). It rode the visibility blend's bit until ADR-5506,
// because two tests of layouts without the bit turned red with it; it is every layout's lookup now.
fn ddgi_tier_lookup() -> bool {
    return true;
}

// The direction an irradiance tile's interior texel at octahedral coordinates `e` integrates its cosine lobe
// about: the texel's own direction, except that on a tier layout (`ddgi_tier_lookup`) the texels next to a
// coordinate axis (the four around each pole of the octahedral map, the two on either side of each of the other
// four axes: 16 of the 36) take the axis itself. A bilinear lookup toward an axis then returns that axis's exact
// irradiance. With every texel on its own direction the lookup averages lobes tilted 19.5 degrees (the poles) or
// 11.3 degrees (the other axes) off the axis: such a lobe reaches below the horizon of an axis-aligned surface,
// where a probe standing on the surface sees the surface itself, so a sunlit wall adds 3 % of its own sunlit
// radiance to its indirect light (a fifth of it on the trench plate), and light arriving at a grazing angle
// reads too strong (ADR-5374). The tier lookups read the six axes only (`ddgi_axis_irradiance`), so they read
// exact lobes. The surface-point weights need this: they read the probes that stand on a surface alone. Of each
// axis's texels the ones of even interior parity hold the irradiance at the probe and the odd ones the
// irradiance at the surface behind it (ADR-5457, `ddgi_axis_texels`).
fn ddgi_irr_texel_dir(e: vec2<f32>) -> vec3<f32> {
    let dir = ddgi_oct_decode(e);
    let a = abs(dir);
    let m = max(a.x, max(a.y, a.z));
    // The axis texels' own directions have a largest component of 0.94 (poles) or 0.98; no other texel's
    // passes 0.81.
    if (ddgi_tier_lookup() && m > 0.9) {
        return select(vec3<f32>(0.0), sign(dir), a >= vec3<f32>(m));
    }
    return dir;
}

// Whether the interior texel at octahedral coordinates `e` is one of the 16 next to a coordinate axis
// (`ddgi_irr_texel_dir`): those hold an axis's lobe.
fn ddgi_irr_texel_on_axis(e: vec2<f32>) -> bool {
    let a = abs(ddgi_oct_decode(e));
    return max(a.x, max(a.y, a.z)) > 0.9;
}

// Tile texels (border included) of cube direction `j` (+x -x +y -y +z -z, `ddgi_cube_dir`) on a tier layout:
// xy = the texel that holds the irradiance at the probe (the axis texel of even interior parity), zw = the one
// that holds it at the surface behind the probe (odd parity, `ddgi_blend.wgsl` `blend_irradiance`, ADR-5457).
fn ddgi_axis_texels(j: u32) -> vec4<u32> {
    switch j {
        case 0u: { return vec4<u32>(6u, 4u, 6u, 3u); }
        case 1u: { return vec4<u32>(1u, 3u, 1u, 4u); }
        case 2u: { return vec4<u32>(4u, 6u, 3u, 6u); }
        case 3u: { return vec4<u32>(3u, 1u, 4u, 1u); }
        case 4u: { return vec4<u32>(3u, 3u, 4u, 3u); }
        default: { return vec4<u32>(1u, 1u, 6u, 1u); }
    }
}

// ---- Creases (ADR-5552) -------------------------------------------------------------------------------------
//
// A probe's surface texel holds the irradiance at one point of the surface behind it, kept a quarter spacing from
// the surfaces beside it (`ddgi_blend.wgsl` `surface_point`). Between that point and the next surface the light
// changes faster than anywhere else: the surface beside it fills up to half the hemisphere, and it is the strip of
// it next to the crease that counts (a sunlit strip beside a shaft's corner lights the wall beside it five times
// as strongly at the corner as half a metre before it; a wall's foot over a floor in shadow holds a third of the
// light a quarter metre up). The lookups read every point of that stretch as the surface point, which held the
// 95th percentile of the error on a shaft and a color room (`gi_acceptance`). A probe therefore also holds, for
// each face and each of the four sides across it, the irradiance at the crease point: on the face's surface,
// `DDGI_CREASE_CLEARANCE` spacings from the surface that lies beside the probe toward that side. The 24 values
// live in the tile's border ring, which no lookup reads since ADR-5506 (the lookups read the axis texels alone).
// A lookup scales the face's light by crease / surface between the crease point and the surface point
// (`ddgi_crease_gain`). A crease with no surface beside it holds the face's own light, and scales nothing.

// Cube direction `j` (+x -x +y -y +z -z) as a vector (`ddgi_sample.wgsl` `ddgi_cube_dir`, for the shaders that
// do not include that file).
fn ddgi_face_dir(j: u32) -> vec3<f32> {
    let sg = select(1.0, -1.0, (j & 1u) == 1u);
    let axis = j >> 1u;
    return vec3<f32>(select(0.0, sg, axis == 0u), select(0.0, sg, axis == 1u), select(0.0, sg, axis == 2u));
}

// The cube direction (`ddgi_face_dir`) of a signed unit axis vector.
fn ddgi_face_of(v: vec3<f32>) -> u32 {
    let a = abs(v);
    let axis = select(select(2u, 1u, a.y > 0.5), 0u, a.x > 0.5);
    return axis * 2u + select(0u, 1u, v.x + v.y + v.z < 0.0);
}

// Side `c` (0..3) of the face along `axis`: the two axes across it, each both ways (toward, away, toward, away).
fn ddgi_crease_side(axis: vec3<f32>, c: u32) -> vec3<f32> {
    return select(axis.yzx, axis.zxy, c >= 2u) * select(1.0, -1.0, (c & 1u) == 1u);
}

// Tile texel (border included) of the crease of face `j` toward its side `c`: crease `j * 4 + c` along the
// border ring, the corner (0, 0) left to the surface word (`ddgi_surface_word`).
fn ddgi_crease_texel(j: u32, c: u32) -> vec2<u32> {
    let i = j * 4u + c;
    if (i < 7u) {
        return vec2<u32>(i + 1u, 0u);
    }
    if (i < 15u) {
        return vec2<u32>(i - 7u, 7u);
    }
    if (i < 21u) {
        return vec2<u32>(0u, i - 14u);
    }
    return vec2<u32>(7u, i - 20u);
}

// The crease (face * 4 + side) tile texel `t` holds (`ddgi_crease_texel`'s inverse); 99: none.
fn ddgi_crease_index(t: vec2<u32>) -> u32 {
    if (t.y == 0u && t.x >= 1u) {
        return t.x - 1u;
    }
    if (t.y == 7u) {
        return 7u + t.x;
    }
    if (t.x == 0u && t.y >= 1u && t.y <= 6u) {
        return 14u + t.y;
    }
    if (t.x == 7u && t.y >= 1u && t.y <= 3u) {
        return 20u + t.y;
    }
    return 99u;
}

// A surface point keeps this many probe spacings from the surfaces beside it (`ddgi_blend.wgsl` `surface_point`).
const DDGI_SURFACE_CLEARANCE: f32 = 0.25;
// A crease point stands this many probe spacings from the surface beside it (`ddgi_blend.wgsl` `crease_point`):
// nearer, the strip that lights it is one or two of a probe's rays wide.
const DDGI_CREASE_CLEARANCE: f32 = 0.08;
// The least clearance in metres a crease scales its face at (`ddgi_crease_gain`).
const DDGI_CREASE_LEAST: f32 = 0.05;

// Where the surfaces behind a probe lie (ADR-5457): the shading copy of the tile's corner texel (0, 0), which
// no lookup reads as light, holds per cube direction (five bits each, +x first) how far behind the probe a flat
// surface lies along that direction, in steps of `DDGI_SURFACE_REACH` / 31 spacings; 0: none. Since ADR-5552 it
// lists the surfaces a probe stands on too (nearer than 0.15 spacings, where the direction's odd texel is the
// probe's own lobe): the creases read them.
const DDGI_SURFACE_REACH: f32 = 1.05;

fn ddgi_surface_word(index: u32) -> u32 {
    return ddgi_data[ddgi.words.w + index * DDGI_PACKED_IRR_WORDS];
}

fn ddgi_pack_surface_back(back: f32, s: f32) -> u32 {
    return u32(round(clamp(back / (DDGI_SURFACE_REACH * s), 0.0, 1.0) * 31.0));
}

// Irradiance / pi of probe `index` toward cube direction `j` for a point `relp` from the probe (point - probe),
// `s` the probe spacing, `backs` the probe's `ddgi_surface_word`, `bounce` whether the read feeds a bounce (every
// read but the screen lookup's: `ddgi_crease_gain`). Beside or in front of the probe along that
// direction it is the probe's irradiance; on the surface the probe stands in front of, the irradiance at that
// surface, scaled toward its creases (`ddgi_crease_gain`, ADR-5552); between the two, in proportion to the way
// there. A point behind the probe that lies on another, nearer surface (a table top over the floor the probe's
// texel holds) so takes part of each.
// A probe stands on a surface nearer than this many spacings (`DdgiSettings::min_frontface` pushes a probe 0.12
// spacings off a surface; the surface word's steps are 0.034 wide): the lookup then places the surface by that
// least distance (`ddgi_crease_gain`).
const DDGI_CREASE_STANDING: f32 = 0.18;
// A probe stands beside a surface nearer than this many spacings (a quarter, the clearance a surface point
// keeps: `DDGI_SURFACE_CLEARANCE`). A crease toward such a surface is estimated from beside it: the probe's rays
// reach the strip next to the crease as the crease point does. A crease toward a farther surface (a wall half a
// cell away) is estimated across the cell: few rays reach that strip, so its estimate weighs the surface and
// the rest of the view apart (`ddgi_blend.wgsl` `blend_irradiance`, ADR-5716). The frame takes a crease either
// way (the screen lookup, where it is the measured gain: a shaft's corner band, the Low tier's rooms). A read
// that feeds a bounce (the probes' own ray read, the radiance cache's cells, ReSTIR GI's probe read) takes a
// crease from beside in full and one from across the cell only where it adds light: an under-read there is read
// again by every bounce, and a tunnel and a shaft, lit by light that travels along their walls, lost 1.4 % and
// 1.0 % of their light to it (ADR-5552). Until ADR-5716 the bound between the two was `DDGI_CREASE_STANDING`: a
// probe 0.2 spacings before a wall (a 2.4 m room on 1 m probes) counted as across the cell, and its bounce kept
// the bright creases of a sunlit room without the dark ones (a color room at 1.02 times its light).
const DDGI_CREASE_BESIDE: f32 = 0.25;

// What the light of face `j` (along `axis`) of probe `index` is scaled by at a point `relp` from the probe for
// the crease toward the face's side `c`: 1 at the face's surface point and anywhere farther from the surface
// beside the probe than that point stands (`lean`: how far the face's texel leans on a surface point, which keeps
// its clearance; a probe that stands on the face's surface is its own surface point), crease / surface (`surf` the
// face's surface texel) at the crease point and nearer, in proportion between. 1 without a surface on that side.
// `bounce`: the read feeds a bounce (`DDGI_CREASE_STANDING`).
fn ddgi_crease_gain(
    index: u32,
    j: u32,
    c: u32,
    axis: vec3<f32>,
    relp: vec3<f32>,
    s: f32,
    backs: u32,
    lean: f32,
    surf: vec3<f32>,
    bounce: bool,
) -> vec3<f32> {
    let side = ddgi_crease_side(axis, c);
    let steps = (backs >> (5u * ddgi_face_of(-side))) & 31u;
    // No crease scaling where the crease point would stand under `DDGI_CREASE_LEAST` metres from the surface
    // beside it (probe spacings under 0.625 m): the strip that lights it is then as thin as the bricks' voxels,
    // and a color room on 0.4 m probes read 0.084 with the scaling against 0.0795 without (ADR-5552's open
    // decision 7, measured there as a shader variant and adopted by ADR-5716).
    if (steps == 0u || DDGI_CREASE_CLEARANCE * s < DDGI_CREASE_LEAST) {
        return vec3<f32>(1.0);
    }
    // How far beside the probe that surface lies. A probe that stands on it was pushed to the least front-face
    // distance (`ddgi.flat_sky.w` spacings, `ddgi_trace.wgsl` `relocate`): that distance, not the word's step,
    // which rounds just there (0.12 spacings are 3.5 steps) and would move the crease by a step's width.
    var reach = f32(steps) * (DDGI_SURFACE_REACH / 31.0) * s;
    let standing = reach <= DDGI_CREASE_STANDING * s;
    if (standing) {
        reach = clamp(ddgi.flat_sky.w, DDGI_CREASE_CLEARANCE * 1.3, DDGI_CREASE_STANDING) * s;
    }
    let d = reach - dot(relp, side);
    let start = mix(reach, max(reach, DDGI_SURFACE_CLEARANCE * s), lean);
    let near = DDGI_CREASE_CLEARANCE * s;
    if (d >= start || start <= near * 1.25) {
        return vec3<f32>(1.0);
    }
    let u = 1.0 - clamp((d - near) / (start - near), 0.0, 1.0);
    let crease = ddgi_packed_irr_load(index, ddgi_crease_texel(j, c));
    let eps = vec3<f32>(0.02 * max(surf.r, max(surf.g, surf.b)) + 1e-6);
    var ratio = clamp((crease + eps) / (surf + eps), vec3<f32>(0.125), vec3<f32>(8.0));
    // A read that feeds a bounce takes a crease toward a surface the probe does not stand beside only where it
    // adds light (`DDGI_CREASE_BESIDE`).
    if (bounce && reach > DDGI_CREASE_BESIDE * s) {
        ratio = max(ratio, vec3<f32>(1.0));
    }
    return mix(vec3<f32>(1.0), ratio, u);
}

fn ddgi_axis_irradiance(index: u32, j: u32, relp: vec3<f32>, s: f32, backs: u32, bounce: bool) -> vec3<f32> {
    let t = ddgi_axis_texels(j);
    let steps = (backs >> (5u * j)) & 31u;
    let axis = ddgi_face_dir(j);
    let behind = -dot(relp, axis);
    if (behind <= 0.0 || steps == 0u) {
        return ddgi_packed_irr_load(index, t.xy);
    }
    // A point on the surface stands a few hundredths of a spacing before it (a ray hit and the screen lookup's
    // point are lifted 0.02 spacings, and a hit stops within its brick's band): it reads the surface in full.
    let reach = f32(steps) * (DDGI_SURFACE_REACH / 31.0);
    let back = max(reach - 0.04, 0.05) * s;
    let k = clamp(behind / back, 0.0, 1.0);
    let surf = ddgi_packed_irr_load(index, t.zw);
    var on = surf;
    // The creases toward the four sides, where the probe lists a surface on any of them (the word's bits of
    // the other two axes).
    if ((backs & ~(1023u << (10u * (j >> 1u)))) != 0u) {
        // How far the face's texel leans on its surface point (`ddgi_blend.wgsl` `surface_point`'s ramp).
        let lean = clamp((reach - 0.15) / 0.2, 0.0, 1.0);
        on = surf * ddgi_crease_gain(index, j, 0u, axis, relp, s, backs, lean, surf, bounce)
            * ddgi_crease_gain(index, j, 1u, axis, relp, s, backs, lean, surf, bounce)
            * ddgi_crease_gain(index, j, 2u, axis, relp, s, backs, lean, surf, bounce)
            * ddgi_crease_gain(index, j, 3u, axis, relp, s, backs, lean, surf, bounce);
    }
    if (k >= 0.97) {
        return on;
    }
    return mix(ddgi_packed_irr_load(index, t.xy), on, k);
}

// Irradiance / pi of probe `index` for normal `n` at a point `rel` = probe position - point: the ambient cube
// of its axis texels (`ddgi_cube_dir`, the rebuild the shading uses), each face read for where the point lies
// along it.
fn ddgi_probe_irradiance(index: u32, n: vec3<f32>, rel: vec3<f32>, s: f32) -> vec3<f32> {
    let n2 = n * n;
    let backs = ddgi_surface_word(index);
    let e = ddgi_axis_irradiance(index, select(0u, 1u, n.x < 0.0), -rel, s, backs, true) * n2.x
        + ddgi_axis_irradiance(index, select(2u, 3u, n.y < 0.0), -rel, s, backs, true) * n2.y
        + ddgi_axis_irradiance(index, select(4u, 5u, n.z < 0.0), -rel, s, backs, true) * n2.z;
    return e / max(n2.x + n2.y + n2.z, 1e-6);
}

// A unit vector in 18 bits: octahedral, 9 bits a coordinate.
fn ddgi_pack_oct18(n: vec3<f32>) -> u32 {
    let e = clamp(ddgi_oct_encode(n) * 0.5 + vec2<f32>(0.5), vec2<f32>(0.0), vec2<f32>(1.0));
    let q = vec2<u32>(round(e * 511.0));
    return q.x | (q.y << 9u);
}

fn ddgi_unpack_oct18(v: u32) -> vec3<f32> {
    let e = vec2<f32>(f32(v & 511u), f32((v >> 9u) & 511u)) / 511.0 * 2.0 - vec2<f32>(1.0);
    return ddgi_oct_decode(e);
}

// A probe ray's radiance with its hit's surface normal in the six lowest mantissa bits of each channel (a
// change of 2^-17 of the value): the ray buffer keeps its four floats, and the blend reads where the hit faces
// (`ddgi_ray_normal`).
fn ddgi_ray_embed(l: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let b = ddgi_pack_oct18(n);
    let low = vec3<u32>(b & 63u, (b >> 6u) & 63u, (b >> 12u) & 63u);
    return bitcast<vec3<f32>>((bitcast<vec3<u32>>(l) & vec3<u32>(0xffffffc0u)) | low);
}

fn ddgi_ray_normal(l: vec3<f32>) -> vec3<f32> {
    let b = bitcast<vec3<u32>>(l) & vec3<u32>(63u);
    return ddgi_unpack_oct18(b.x | (b.y << 6u) | (b.z << 12u));
}

// The interior texel a tile texel (border included) copies: octahedral border wrap.
fn ddgi_border_source(t: vec2<i32>, res: i32) -> vec2<i32> {
    var i = t.x - 1;
    var j = t.y - 1;
    let ox = i < 0 || i >= res;
    let oy = j < 0 || j >= res;
    if (ox && oy) {
        return vec2<i32>(res - 1 - clamp(i, 0, res - 1), res - 1 - clamp(j, 0, res - 1));
    }
    if (oy) {
        i = res - 1 - i;
        j = clamp(j, 0, res - 1);
    }
    if (ox) {
        j = res - 1 - j;
        i = clamp(i, 0, res - 1);
    }
    return vec2<i32>(i, j);
}

// Spherical Fibonacci direction `i` of `n` (Keinert et al., TOG 2015), rotated per frame. Every probe traces
// the same directions: probes in like surroundings then hold like errors, so a view that moves along a wall or
// a corridor does not see the probes' errors pass by (per-probe orientations were measured and dropped,
// ADR-5457: a held view changed 4 % less from frame to frame, a walking one strayed 70 % more from its held
// image on the Low tier).
fn ddgi_ray_dir(i: u32, n: u32) -> vec3<f32> {
    let golden = 2.39996323;
    let z = 1.0 - (2.0 * f32(i) + 1.0) / f32(n);
    let r = sqrt(max(1.0 - z * z, 0.0));
    let phi = golden * f32(i);
    let d = vec3<f32>(r * cos(phi), r * sin(phi), z);
    return normalize(vec3<f32>(dot(ddgi.rot_x.xyz, d), dot(ddgi.rot_y.xyz, d), dot(ddgi.rot_z.xyz, d)));
}

// Surface normal of a screen texel from its depth neighbors, facing `v` (toward the eye). On
// each axis it takes the smaller of the two one-sided differences (`r`/`l`, `u`/`b`) so an edge
// does not bend it. At the screen border the clamped neighbor is the texel itself: that zero
// difference is never taken, and when no non-zero pair spans a plane (both differences on an
// axis zero, collinear, or a non-finite depth) the normal is `v`, never `normalize` of a zero
// cross product (NaN, which `ddgi_pack_rgb9e5` and the 2×2 screen gather would spread).
fn ddgi_depth_normal(r: vec3<f32>, l: vec3<f32>, u: vec3<f32>, b: vec3<f32>, v: vec3<f32>) -> vec3<f32> {
    let rr = dot(r, r);
    let ll = dot(l, l);
    let uu = dot(u, u);
    let bb = dot(b, b);
    let dx = select(l, r, ll == 0.0 || (rr > 0.0 && rr < ll));
    let dy = select(b, u, bb == 0.0 || (uu > 0.0 && uu < bb));
    let c = cross(dy, dx);
    let len2 = dot(c, c);
    // A bit test for NaN and ∞ (exponent bits all ones): a compiler may rewrite `!(a > b)` as
    // `a <= b`, which is false for NaN. The sign bit is shifted out rather than masked: the
    // NVIDIA 610.88 Windows Vulkan compiler folds `(bits & 0x7f800000) == 0x7f800000` to false
    // on a `dot(c, c)` result, so a NaN or ∞ length passed (ADR-3150); the shift form holds there,
    // on DX12 and on lavapipe.
    if ((bitcast<u32>(len2) << 1u) >= 0xff000000u || len2 <= 1.0e-30) {
        return v;
    }
    let n = c * inverseSqrt(len2);
    return select(n, -n, dot(n, v) < 0.0);
}

// Unit direction from a shaded point `p` to a probe at `probe_pos`. A surface through the probe
// (`p` on it, or closer than 1 µm) takes the surface normal `n`, the direction of full weight:
// `normalize` of a zero vector is NaN, and a NaN weight passes the `wsum <= 0` fallback, so the
// irradiance (and the TAA history that keeps it) would be NaN.
fn ddgi_dir_to_probe(probe_pos: vec3<f32>, p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let d = probe_pos - p;
    let len2 = dot(d, d);
    // NaN and ∞ by their exponent bits, sign shifted out (see `ddgi_depth_normal`).
    if ((bitcast<u32>(len2) << 1u) >= 0xff000000u || len2 <= 1.0e-12) {
        return n;
    }
    return d * inverseSqrt(len2);
}

// `v` when every component is finite, else `fallback`. NaN and ±∞ are the floats whose
// exponent bits are all ones; a `v != v` test, and on computed values the masked
// `(bits & 0x7f800000) == 0x7f800000` test, may be folded away by the shader compiler, so the
// test is `(bits << 1) >= 0xff000000` (ADR-3150, ADR-8366).
fn ddgi_finite_or3(v: vec3<f32>, fallback: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(v) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(v, fallback, any(bad));
}

fn ddgi_finite_or2(v: vec2<f32>, fallback: vec2<f32>) -> vec2<f32> {
    let bad = (bitcast<vec2<u32>>(v) << vec2<u32>(1u)) >= vec2<u32>(0xff000000u);
    return select(v, fallback, any(bad));
}

// `ctrl.w` bit 23: `DdgiSettings::front_weights` (ADR-5566; every tier sets it since ADR-5716). The lookups then
// also weigh a probe by which side of the point's surface it stands on (`ddgi_sample.wgsl` `ddgi_front_weight`).
// Off, every lookup is the one it was.
const DDGI_CTRL_FRONT_WEIGHTS: u32 = 8388608u;

fn ddgi_front_weights_on() -> bool {
    return (ddgi.ctrl.w & DDGI_CTRL_FRONT_WEIGHTS) != 0u;
}

// `ctrl.w` bit 24: `DdgiSettings::edge_visibility` (ADR-5566's second term, its own switch since ADR-5716; the Low
// and Medium tiers set it). A probe past the edge of the point's surface then has to see the point without doubt
// (`ddgi_sample.wgsl` `ddgi_edge_visibility`), and the blend keeps each probe's cover word (`ddgi_blend.wgsl`).
const DDGI_CTRL_EDGE_VISIBILITY: u32 = 16777216u;

fn ddgi_edge_visibility_on() -> bool {
    return (ddgi.ctrl.w & DDGI_CTRL_EDGE_VISIBILITY) != 0u;
}

// Probe record word 11, the cover word (ADR-5566; words 0 to 10 are the offset, the cell, the stamp and the seed):
// per cube direction `j` (five bits each, +x first, `ddgi_cube_dir`) how much of the probe's distance lobe toward
// the surface behind that face ends within `DDGI_SURFACE_REACH` spacings, 31 = all of it. A probe under a ceiling
// holds 31 for the downward face; a probe outside the roof's edge, with the sky over it, holds 0.
const DDGI_COVER_WORD: u32 = 11u;

fn ddgi_cover_word(index: u32) -> u32 {
    return ddgi_data[index * DDGI_PROBE_WORDS + DDGI_COVER_WORD];
}
