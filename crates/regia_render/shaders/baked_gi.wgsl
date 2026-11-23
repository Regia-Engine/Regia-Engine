// SPDX-License-Identifier: MPL-2.0
// Baked diffuse GI volume (`.fgiv`, `regia_asset::baked_gi`): the runtime lookup
// `static_gi(p, n, v)`. Mirrored on the CPU by `baked_gi.rs` (`sample_static_gi`); keep the two
// in step.
//
// This file has no bindings. A host shader declares, under these names:
//   bgi_u() -> BakedGiUniform: the volume the lookup reads (a uniform, or the screen pass's
//     current slot of its set). Each function that reads it binds `let bgi = bgi_u();` first,
//     so the lookup keeps no Private or Function copy of the struct: naga gives every struct an
//     explicit layout, which VUID-StandaloneSpirv-None-10684 forbids there (ADR-026, ADR-7587).
//     For the same reason no function here holds a struct or array `var`: running sums are one
//     `var` per member, and eight per-corner values are two `vec4` halves (`bgi_get8f`) written
//     through a lane mask (`bgi_lane_lo`), never at a dynamic component index (FXC, ADR-1583).
//   bgi_word(i: u32) -> u32: word `i` of the volume buffer (a storage array, read): the volume's
//     file sections uploaded unchanged (meta 2 words, SH 16 words, moments res² words per probe;
//     ground heights; brick table: 12 words per brick, origin xyz + first probe, spacing xyz +
//     flags, counts xyz + 0). The screen pass picks one of its volume slots there (ADR-2618).
//
// `bgi_visits` counts the bricks the lookup tested (the grid cell's list length, summed over
// every lookup since the host last cleared it): the brick-loop probe of ADR-2618.
//
// Lookup: the terrain layer bilinearly over its four columns and linearly between the two
// levels that bracket the height above the interpolated ground (Greger et al., "The Irradiance
// Volume", 1998; Tatarchuk, "Irradiance Volumes for Games", GDC 2005); a detail brick overrides
// it inside its box, blended over its last cell. An interior brick (`.fgiv` v2 flag) wins over
// any other brick that holds the point pushed half a cell along its normal, keeps the terrain
// layer out and does not blend over its last cell. Each of the eight corner probes is weighted as
// in DDGI (Majercik et al., JCGT 2019): trilinear, a wrap on the normal side, Chebyshev
// visibility from the baked distance moments (cubed), a crush of tiny weights; the receiver is
// first pushed along the normal and toward the viewer by a fraction of the smallest cell edge.
// The moments are filtered bilinearly across the octahedral folds (continuous over the sphere),
// and where a brick blends with the terrain layer each layer's share is weighted by how much of
// its cell sees the receiver (ADR-506).
// Where the bake found a wall between two corner probes (probe flag wall bits, ADR-366), only
// the corners on one side of it light the receiver: the group the cell's unblocked edges join
// that carries the most weight.
// Probes are L1 SH of radiance per separable light component (Ramamoorthi & Hanrahan 2001); the
// weighted SH is summed over the components (each scaled by its current light over its baked
// light) and corners, and the irradiance is evaluated once for any number of directions.
// Quality pass (ADR-861): the sun bounce of a probe lit from below follows the sun's elevation
// (`bgi_relight`), a strongly directional L1 is evaluated as a non-negative lobe with the same
// mean and first moment (`bgi_irradiance_over_pi`), and the wall split uses the receiver's
// normal near a wall and ignores edges to rejected probes (`bgi_wall_mask`).
// Leak control (ADR-1105): the visibility is by default one Chebyshev bound per bilinear moment
// tap, the bounds filtered bilinearly (percentage-closer filtering, Reeves et al. 1987), so a tap
// that sees a thin wall in front of the receiver is not averaged away by a neighbor that sees past
// it; then the light-bleeding reduction of variance shadow maps (Lauritzen, GPU Gems 3 ch. 8,
// 2007) remaps the bound's tail [amount, 1] to [0, 1]. Without `BGI_LOOKUP_PER_TAP` the moments
// are filtered first (DDGI as published), and `relight.y` = 0 turns the reduction off: both off is
// the lookup before ADR-1105, bit for bit.
// ZH3 from L1 (ADR-1377, `BGI_LOOKUP_ZH3`; mirrored by `baked_gi/zh3.rs`): the summed SH is
// evaluated with one quadratic zonal harmonic along the luminance L1 direction, its coefficient
// hallucinated from each channel's L1 on that axis (after Roughton et al., "ZH3: Quadratic Zonal
// Harmonics", I3D 2024; the curve is the band-2 coefficient of a uniform cap of light). It needs
// no stored data, replaces the de-ringing, and reads uniform light and any L1 weaker than a
// hemisphere's as L1, bit for bit.

struct BakedGiUniform {
    // xy = terrain origin (x, z), zw = column spacing (x, z).
    terrain: vec4<f32>,
    // x = columns along x, y = along z, z = levels (0: no terrain layer), w = bricks.
    counts: vec4<u32>,
    // Level heights above the ground (m), up to eight.
    levels_a: vec4<f32>,
    levels_b: vec4<f32>,
    // First word of the meta, SH, moment and ground sections.
    words: vec4<u32>,
    // x = first word of the brick table, y = moment resolution, z = flags (`BGI_LOOKUP_*`),
    // w = probes.
    extra: vec4<u32>,
    // x = moment distance clamp (m), y = normal bias, z = view bias (× smallest cell edge),
    // w = intensity.
    params: vec4<f32>,
    // Per component (sun bounce, sky, fill, emissive): rgb = current light / baked light /
    // encode exposure (the factor on the stored L00); w unused.
    scale_sun: vec4<f32>,
    scale_sky: vec4<f32>,
    scale_fill: vec4<f32>,
    scale_emissive: vec4<f32>,
    // x = the sun-bounce ground factor (`baked_gi.rs` `SunRelight::ground_factor`; 1 = as
    // baked), y = the light-bleeding reduction (0..0.95, 0 = off; ADR-1105), zw unused.
    relight: vec4<f32>,
}

const BGI_SH_WORDS: u32 = 16u;
const BGI_BRICK_WORDS: u32 = 12u;
// Words of the brick lookup grid's header (`BRICK_GRID_HEADER_WORDS`).
const BGI_GRID_HEADER_WORDS: u32 = 8u;
const BGI_FLAG_VALID: u32 = 1u;
// `BrickFlags::INTERIOR`.
const BGI_BRICK_INTERIOR: u32 = 1u;
// Added to an interior brick's selection key: it beats any non-interior brick.
const BGI_INTERIOR_PRIORITY: f32 = 1.0e4;
// Added to each layer's visibility where a brick blends with the terrain layer (`baked_gi.rs`
// `SEEN_FLOOR`): two blind layers keep the plain blend.
const BGI_SEEN_FLOOR: f32 = 0.02;
// Cells by which an interior brick holds a point outside its probe box (ADR-1364): a surface on
// the box's face lands a few ulps either side of it and must not flip to the outdoor light.
const BGI_INTERIOR_EDGE_SLACK: f32 = 0.125;
const BGI_Y00: f32 = 0.2820948;
const BGI_Y1: f32 = 0.48860252;
const BGI_SQRT3: f32 = 1.7320508;
// `extra.z` bits (`baked_gi.rs` `LOOKUP_DERING`, `LOOKUP_LEAK_GUARD`).
const BGI_LOOKUP_DERING: u32 = 1u;
const BGI_LOOKUP_LEAK_GUARD: u32 = 2u;
// `extra.z` bit (`baked_gi.rs` `LOOKUP_PER_TAP`): the Chebyshev bound per moment tap (ADR-1105).
const BGI_LOOKUP_PER_TAP: u32 = 4u;
// |L1| / L00 of radiance uniform over a hemisphere: Y1 π / (Y00 2π).
const BGI_HEMISPHERE_L1: f32 = 0.8660254;
// Normal component past which the wall split takes the side the receiver faces (leak guard).
const BGI_WALL_NORMAL: f32 = 0.5;
// `extra.z` bit (`baked_gi.rs` `LOOKUP_ZH3`): the ZH3 evaluation (ADR-1377).
const BGI_LOOKUP_ZH3: u32 = 8u;
// 5 / (16·√π): the convolved zonal term is BGI_ZH3_EVAL · L00 · g · (3x² − 1) (`zh3.rs`).
const BGI_ZH3_EVAL: f32 = 0.17630924;
// Below this squared luminance L1 there is no zonal axis: the evaluation is L1.
const BGI_ZH3_AXIS_MIN2: f32 = 1e-20;
// Luminance weights of the shared zonal axis (Rec. 709, as `bgi_relight`).
const BGI_LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// L1 radiance SH summed over components and probes: per channel L00 and (L1x, L1y, L1z).
struct BgiSh {
    l0: vec3<f32>,
    r: vec3<f32>,
    g: vec3<f32>,
    b: vec3<f32>,
}

// The lookup: weighted SH, coverage (0 outside the volume or with every corner rejected) and
// visibility (the share of the interpolation weight whose probes see the receiver; 0 to 1).
// split = the probe-relative AO split (ADR-1104, `baked_gi.rs` `LayerSplit`): x / y = the
// fractions of the light the detail brick and the terrain layer hold (summing to 1 where the
// volume lights the point), z = the brick's largest cell edge, w = the terrain layer's largest
// column spacing (m; 0 for a layer that holds nothing).
struct StaticGi {
    sh: BgiSh,
    cov: f32,
    vis: f32,
    split: vec4<f32>,
}

// `LayerSplit::accumulate`: adds split `s` at weight `w` to a running sum, the cell sizes
// summed weighted by their layer's light.
fn bgi_split_add(acc: vec4<f32>, s: vec4<f32>, w: f32) -> vec4<f32> {
    return acc + w * vec4<f32>(s.x, s.y, s.x * s.z, s.y * s.w);
}

// `LayerSplit::finish`: a running sum's fractions × `k`, its cell sizes averaged.
fn bgi_split_finish(acc: vec4<f32>, k: f32) -> vec4<f32> {
    let bc = select(0.0, acc.z / max(acc.x, 1e-30), acc.x > 0.0);
    let tc = select(0.0, acc.w / max(acc.y, 1e-30), acc.y > 0.0);
    return vec4<f32>(acc.x * k, acc.y * k, bc, tc);
}

fn bgi_sh_zero() -> BgiSh {
    return BgiSh(vec3<f32>(0.0), vec3<f32>(0.0), vec3<f32>(0.0), vec3<f32>(0.0));
}

// Element `k` (0 to 7) of eight values held as two halves: `lo` = 0 to 3, `hi` = 4 to 7.
fn bgi_get8f(lo: vec4<f32>, hi: vec4<f32>, k: u32) -> f32 {
    return select(lo[k & 3u], hi[k & 3u], k >= 4u);
}

fn bgi_get8u(lo: vec4<u32>, hi: vec4<u32>, k: u32) -> u32 {
    return select(lo[k & 3u], hi[k & 3u], k >= 4u);
}

// The lanes of the low (`bgi_lane_lo`) and high (`bgi_lane_hi`) half that hold element `k`:
// `lo = select(lo, vec4(x), bgi_lane_lo(k))` stores x as element k without a dynamic index.
fn bgi_lane_lo(k: u32) -> vec4<bool> {
    return vec4<u32>(0u, 1u, 2u, 3u) == vec4<u32>(k);
}

fn bgi_lane_hi(k: u32) -> vec4<bool> {
    return vec4<u32>(4u, 5u, 6u, 7u) == vec4<u32>(k);
}

fn bgi_rgb9e5(v: u32) -> vec3<f32> {
    let s = exp2(f32(i32(v >> 27u) - 24));
    return vec3<f32>(f32(v & 511u), f32((v >> 9u) & 511u), f32((v >> 18u) & 511u)) * s;
}

fn bgi_level(l: u32) -> f32 {
    let bgi = bgi_u();
    if (l < 4u) {
        return bgi.levels_a[l];
    }
    return bgi.levels_b[l - 4u];
}

fn bgi_scale(c: u32) -> vec3<f32> {
    let bgi = bgi_u();
    switch c {
        case 0u: { return bgi.scale_sun.rgb; }
        case 1u: { return bgi.scale_sky.rgb; }
        case 2u: { return bgi.scale_fill.rgb; }
        default: { return bgi.scale_emissive.rgb; }
    }
}

// Probe `i`'s relocation offset (xyz) and flags (w).
fn bgi_meta(i: u32) -> vec4<f32> {
    let bgi = bgi_u();
    let w0 = bgi_word(bgi.words.x + 2u * i);
    let w1 = bgi_word(bgi.words.x + 2u * i + 1u);
    let xy = unpack2x16float(w0);
    let z = unpack2x16float(w1).x;
    return vec4<f32>(xy, z, f32(w1 >> 16u));
}

fn bgi_valid(m: vec4<f32>) -> bool {
    return (u32(m.w) & BGI_FLAG_VALID) != 0u;
}

// The sun-bounce factor of one probe (`baked_gi.rs` `relight_factor`): 1 + (g − 1) · a · down,
// where g is the ground factor, a the luminance L1's strength against a uniform hemisphere
// (clamped to 1) and down how much the L1 points below the horizon. Light bounced off the ground
// scales with the sun's elevation; light from the side or above keeps the baked factor. g = 1
// leaves every probe as baked.
fn bgi_relight(l0: vec3<f32>, r: vec3<f32>, g: vec3<f32>, b: vec3<f32>) -> f32 {
    let bgi = bgi_u();
    let lum0 = 0.2126 * l0.r + 0.7152 * l0.g + 0.0722 * l0.b;
    let lum1 = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let len = length(lum1);
    if (!(len > 0.0) || !(lum0 > 0.0)) {
        return 1.0;
    }
    let a = min(len / (BGI_HEMISPHERE_L1 * lum0), 1.0);
    let down = max(-lum1.y / len, 0.0);
    return 1.0 + (bgi.relight.x - 1.0) * a * down;
}

// Probe `i`'s SH, every component scaled by its light (the sun bounce also by `bgi_relight`).
fn bgi_probe_sh(i: u32) -> BgiSh {
    let bgi = bgi_u();
    var acc_l0 = vec3<f32>(0.0);
    var acc_r = vec3<f32>(0.0);
    var acc_g = vec3<f32>(0.0);
    var acc_b = vec3<f32>(0.0);
    for (var c = 0u; c < 4u; c = c + 1u) {
        let k = bgi_scale(c);
        if (k.r + k.g + k.b <= 0.0) {
            continue;
        }
        let w = bgi.words.y + i * BGI_SH_WORDS + c * 4u;
        var l0 = bgi_rgb9e5(bgi_word(w)) * k;
        let a = unpack4x8snorm(bgi_word(w + 1u));
        let b = unpack4x8snorm(bgi_word(w + 2u));
        let d = unpack4x8snorm(bgi_word(w + 3u));
        var sr = vec3<f32>(a.x, a.y, a.z) * (BGI_SQRT3 * l0.r);
        var sg = vec3<f32>(a.w, b.x, b.y) * (BGI_SQRT3 * l0.g);
        var sb = vec3<f32>(b.z, b.w, d.x) * (BGI_SQRT3 * l0.b);
        if (c == 0u) {
            let f = bgi_relight(l0, sr, sg, sb);
            l0 = l0 * f;
            sr = sr * f;
            sg = sg * f;
            sb = sb * f;
        }
        acc_l0 = acc_l0 + l0;
        acc_r = acc_r + sr;
        acc_g = acc_g + sg;
        acc_b = acc_b + sb;
    }
    return BgiSh(acc_l0, acc_r, acc_g, acc_b);
}

fn bgi_sh_add(acc: BgiSh, s: BgiSh, w: f32) -> BgiSh {
    return BgiSh(acc.l0 + s.l0 * w, acc.r + s.r * w, acc.g + s.g * w, acc.b + s.b * w);
}

fn bgi_sh_mul(s: BgiSh, w: f32) -> BgiSh {
    return BgiSh(s.l0 * w, s.r * w, s.g * w, s.b * w);
}

// One channel's irradiance / π toward `d` from its L00 and L1 (`baked_gi.rs`
// `channel_irradiance`): r0 + r1·d with r0 = Y00·L00 and r1 = (2/3)·Y1·L1, clamped at 0. With
// `dering` and |r1| > r0 (the linear form would go negative behind the light) it is instead the
// lobe r0·(p + 1)·q^p, q = (1 + r̂1·d)/2, whose mean over the sphere is r0 and whose first moment
// is r1 for p = 4x/(3 − 2x), x = |r1|/(2·r0) clamped to 1: never negative, no energy added by a
// clamp, and equal to the linear form at |r1| = r0.
fn bgi_channel_irradiance(l0: f32, l1: vec3<f32>, d: vec3<f32>, dering: bool) -> f32 {
    let a1 = BGI_Y1 * (2.0 / 3.0);
    let r0 = BGI_Y00 * l0;
    let len = a1 * length(l1);
    if (dering && r0 > 0.0 && len > r0) {
        let x = min(len / (2.0 * r0), 1.0);
        let p = 4.0 * x / (3.0 - 2.0 * x);
        let q = max(0.5 * (1.0 + a1 * dot(l1, d) / len), 1e-6);
        return r0 * (p + 1.0) * pow(q, p);
    }
    // The pre-ADR-861 expression, term for term.
    return max(r0 + a1 * dot(l1, d), 0.0);
}

// One channel's irradiance / π toward `d` with the hallucinated ZH3 term (`zh3.rs`
// `zh3_irradiance_over_pi`): the L1 linear form plus, where the channel's L1 on the shared unit
// `axis` is stronger than a hemisphere's (t > 1/2), the band-2 coefficient of a cap with that
// ratio, √5·L00·t(2t − 1), convolved with the clamped cosine; clamped at 0. `has_axis` false
// (no luminance L1) or t ≤ 1/2: the linear form, as `bgi_channel_irradiance` without de-ringing.
fn bgi_zh3_irradiance(l0: f32, l1: vec3<f32>, d: vec3<f32>, axis: vec3<f32>, has_axis: bool) -> f32 {
    let a1 = BGI_Y1 * (2.0 / 3.0);
    let lin = BGI_Y00 * l0 + a1 * dot(l1, d);
    if (!has_axis || l0 <= 0.0) {
        return max(lin, 0.0);
    }
    let t = clamp(abs(dot(l1, axis)) / (BGI_SQRT3 * l0), 0.0, 1.0);
    let g = max(t * (2.0 * t - 1.0), 0.0);
    if (g <= 0.0) {
        return max(lin, 0.0);
    }
    let x = dot(axis, d);
    return max(lin + BGI_ZH3_EVAL * l0 * g * (3.0 * x * x - 1.0), 0.0);
}

// Irradiance / π toward `d` from summed SH, per channel: ZH3 (`BGI_LOOKUP_ZH3`, the axis shared
// by the channels) or L1 (`bgi_channel_irradiance`).
fn bgi_irradiance_over_pi(s: BgiSh, d: vec3<f32>) -> vec3<f32> {
    let bgi = bgi_u();
    if ((bgi.extra.z & BGI_LOOKUP_ZH3) != 0u) {
        let lum = BGI_LUMA.x * s.r + BGI_LUMA.y * s.g + BGI_LUMA.z * s.b;
        let l2 = dot(lum, lum);
        let has_axis = l2 > BGI_ZH3_AXIS_MIN2;
        let axis = lum * inverseSqrt(max(l2, BGI_ZH3_AXIS_MIN2));
        return vec3<f32>(
            bgi_zh3_irradiance(s.l0.r, s.r, d, axis, has_axis),
            bgi_zh3_irradiance(s.l0.g, s.g, d, axis, has_axis),
            bgi_zh3_irradiance(s.l0.b, s.b, d, axis, has_axis),
        );
    }
    let dering = (bgi.extra.z & BGI_LOOKUP_DERING) != 0u;
    return vec3<f32>(
        bgi_channel_irradiance(s.l0.r, s.r, d, dering),
        bgi_channel_irradiance(s.l0.g, s.g, d, dering),
        bgi_channel_irradiance(s.l0.b, s.b, d, dering),
    );
}

// Octahedral encode to [0, 1]² (Cigolle et al. 2014; fold on −Z), as `baked_gi::oct_encode`.
fn bgi_oct_encode(dir: vec3<f32>) -> vec2<f32> {
    let l1 = abs(dir.x) + abs(dir.y) + abs(dir.z);
    if (l1 <= 0.0) {
        return vec2<f32>(0.5);
    }
    var uv = dir.xy / l1;
    if (dir.z < 0.0) {
        let s = select(vec2<f32>(-1.0), vec2<f32>(1.0), uv >= vec2<f32>(0.0));
        uv = (vec2<f32>(1.0) - abs(uv.yx)) * s;
    }
    return uv * 0.5 + vec2<f32>(0.5);
}

// Texel (x, y) of a res × res octahedral map, where x and y may lie one texel past an edge
// (`baked_gi.rs` `oct_texel`). Each edge of the map folds at its midpoint (the directions at u
// and 1 − u along the top and bottom edges, v and 1 − v along the sides, are the same), so the
// texel across the right edge at row y is (res − 1, res − 1 − y), and across a corner the
// opposite corner: the border DDGI copies around each probe (Majercik et al. 2019 § 4.2).
fn bgi_oct_texel(x: i32, y: i32, res: i32) -> vec2<u32> {
    var t = vec2<i32>(x, y);
    if (t.x < 0 || t.x >= res) {
        t = vec2<i32>(clamp(t.x, 0, res - 1), res - 1 - t.y);
    }
    if (t.y < 0 || t.y >= res) {
        t = vec2<i32>(res - 1 - t.x, clamp(t.y, 0, res - 1));
    }
    return vec2<u32>(t);
}

// Probe `i`'s four bilinear moment taps toward `dir` (unorm: mean / far, mean² / far²) and the
// bilinear fractions. A tap past an edge of the map reads the texel across the fold
// (`bgi_oct_texel`), so the filter is continuous over the whole sphere.
struct BgiTaps {
    a: vec2<f32>,
    b: vec2<f32>,
    c: vec2<f32>,
    d: vec2<f32>,
    fr: vec2<f32>,
}

fn bgi_moment_taps(i: u32, dir: vec3<f32>) -> BgiTaps {
    let bgi = bgi_u();
    let res = bgi.extra.y;
    let ri = i32(res);
    let rf = f32(res);
    let uv = clamp(bgi_oct_encode(dir), vec2<f32>(0.0), vec2<f32>(1.0));
    let f = uv * rf - vec2<f32>(0.5);
    let fl = floor(f);
    let x0 = i32(fl.x);
    let y0 = i32(fl.y);
    let base = bgi.words.z + i * res * res;
    let ta = bgi_oct_texel(x0, y0, ri);
    let tb = bgi_oct_texel(x0 + 1, y0, ri);
    let tc = bgi_oct_texel(x0, y0 + 1, ri);
    let td = bgi_oct_texel(x0 + 1, y0 + 1, ri);
    return BgiTaps(
        unpack2x16unorm(bgi_word(base + ta.y * res + ta.x)),
        unpack2x16unorm(bgi_word(base + tb.y * res + tb.x)),
        unpack2x16unorm(bgi_word(base + tc.y * res + tc.x)),
        unpack2x16unorm(bgi_word(base + td.y * res + td.x)),
        f - fl,
    );
}

// A unorm moment tap in metres: (mean, mean²).
fn bgi_tap_metres(t: vec2<f32>) -> vec2<f32> {
    let bgi = bgi_u();
    let far = bgi.params.x;
    return vec2<f32>(t.x * far, t.y * far * far);
}

// Chebyshev upper bound (Donnelly & Lauritzen 2006) on the chance that a receiver `r` metres
// out is visible from moments `m` (metres): 1 at or before the mean; `floor_var` floors the
// variance (a texel that saw one distance has none).
fn bgi_chebyshev(m: vec2<f32>, r: f32, floor_var: f32) -> f32 {
    if (r <= m.x) {
        return 1.0;
    }
    let variance = max(abs(m.y - m.x * m.x), floor_var);
    let d = r - m.x;
    return variance / (variance + d * d);
}

// The visibility of a receiver `r` metres from probe `i` toward `dir`, cell scale `s`
// (`baked_gi.rs` `visibility`; keep the two in step): the Chebyshev bound per tap filtered
// bilinearly (`BGI_LOOKUP_PER_TAP`) or on the bilinear moments, then the light-bleeding
// reduction (`relight.y`).
fn bgi_visibility(i: u32, dir: vec3<f32>, r: f32, s: f32) -> f32 {
    let bgi = bgi_u();
    let t = bgi_moment_taps(i, dir);
    let floor_var = 1e-4 * s * s;
    var vis = 1.0;
    if ((bgi.extra.z & BGI_LOOKUP_PER_TAP) != 0u) {
        let ca = bgi_chebyshev(bgi_tap_metres(t.a), r, floor_var);
        let cb = bgi_chebyshev(bgi_tap_metres(t.b), r, floor_var);
        let cc = bgi_chebyshev(bgi_tap_metres(t.c), r, floor_var);
        let cd = bgi_chebyshev(bgi_tap_metres(t.d), r, floor_var);
        vis = mix(mix(ca, cb, t.fr.x), mix(cc, cd, t.fr.x), t.fr.y);
    } else {
        let m = mix(mix(t.a, t.b, t.fr.x), mix(t.c, t.d, t.fr.x), t.fr.y);
        vis = bgi_chebyshev(bgi_tap_metres(m), r, floor_var);
    }
    let k = clamp(bgi.relight.y, 0.0, 0.95);
    if (k > 0.0 && vis < 1.0) {
        vis = clamp((vis - k) / (1.0 - k), 0.0, 1.0);
    }
    return vis;
}

// The DDGI corner weight of a probe at `probe_pos` for the receiver `p` (normal `n`), the
// biased point `biased` and the cell scale `s`; `tri` is the corner's interpolation weight.
// Returns (weight, visible share): the visible share is the Chebyshev visibility alone (1 when
// the probe sees the biased point) times the corner's interpolation weight, so the corners'
// sum over their interpolation weights says how much of the cell sees the receiver.
fn bgi_corner(i: u32, probe_pos: vec3<f32>, p: vec3<f32>, biased: vec3<f32>, n: vec3<f32>, s: f32, tri: f32) -> vec2<f32> {
    let to_probe = normalize(probe_pos - p + vec3<f32>(1e-6, 0.0, 0.0));
    var w = (dot(to_probe, n) + 1.0) * 0.5;
    w = w * w + 0.2;
    let from_probe = biased - probe_pos;
    let r = max(length(from_probe), 1e-4);
    let v0 = bgi_visibility(i, from_probe / r, r, s);
    var vis = 1.0;
    if (v0 < 1.0) {
        vis = max(v0 * v0 * v0, 0.0);
        w = w * vis;
    }
    w = max(w, 1e-6);
    if (w < 0.2) {
        w = w * w * w / (0.2 * 0.2);
    }
    let t = max(tri, 0.001);
    return vec2<f32>(w * t, vis * t);
}

// Which of a cell's eight corners light the receiver, bit k for corner k (`baked_gi.rs`
// `wall_mask`; keep the two in step). Without a wall code on any edge of the cell: all eight, and
// the lookup is as before ADR-366. Otherwise an edge is blocked when its low corner's flags hold a
// wall toward the high corner (`same` marks an edge clamped to one probe, never blocked), and the
// corners split into the groups the unblocked edges join. The receiver's corner takes, along each
// corner bit, the high end when the receiver's fraction `f` is past the walls on that bit's edges
// (their positions interpolated by `f` on the other two bits), else the low end (the nearer end
// along a bit without a wall); its group lights the receiver. If that group holds no valid probe,
// the group with the largest summed weight does, ties to the lowest corner's. `axes` maps corner
// bits to flag axes.
// With the leak guard (ADR-861, `leak_guard`): an edge with a rejected end (`valid` bit clear) joins
// nothing and its code does not move the wall position (a segment to a probe under the ground or
// inside geometry crosses that surface, not the wall); and along a bit whose walls all sit on such
// edges (the position is unknown), a receiver whose normal `nrm` (in corner-bit order) points
// along the bit by at least `BGI_WALL_NORMAL` takes the side its normal faces, instead of the
// middle of the cell.
// `flags_*` and `weights_*` are the eight corners' flags and weights as two halves (`bgi_get8u`).
fn bgi_wall_mask(flags_lo: vec4<u32>, flags_hi: vec4<u32>, weights_lo: vec4<f32>, weights_hi: vec4<f32>, axes: vec3<u32>, same: vec3<bool>, f_in: vec3<f32>, valid: u32, nrm_in: vec3<f32>, leak_guard: bool) -> u32 {
    var fv = f_in;
    var nv = nrm_in;
    // Bit e: edge e is blocked.
    var blocked = 0u;
    // Per corner bit b, written through a lane mask: FXC cannot store to a vector component at
    // a dynamic index (`wall_sum[b] = ...`) and unrolls every enclosing loop to make it
    // constant, which fails inside `static_gi_set`'s volume loop (X3550 then X3511, ADR-1583).
    var wall_sum = vec3<f32>(0.0);
    var wall_weight = vec3<f32>(0.0);
    // Bit b: some edge along corner bit b carries a wall code.
    var has_wall = 0u;
    var any_wall = false;
    for (var e = 0u; e < 12u; e = e + 1u) {
        let b = e / 4u;
        let j = e % 4u;
        let lo = ((j >> b) << (b + 1u)) | (j & ((1u << b) - 1u));
        let axis = select(select(axes.z, axes.y, b == 1u), axes.x, b == 0u);
        let clamped = select(select(same.z, same.y, b == 1u), same.x, b == 0u);
        let code = (bgi_get8u(flags_lo, flags_hi, lo) >> (4u + 4u * axis)) & 0xfu;
        if (clamped || code == 0u) {
            continue;
        }
        blocked = blocked | (1u << e);
        any_wall = true;
        has_wall = has_wall | (1u << b);
        let hi = lo | (1u << b);
        if (leak_guard && (((valid >> lo) & 1u) == 0u || ((valid >> hi) & 1u) == 0u)) {
            continue;
        }
        var w = 1.0;
        for (var c = 0u; c < 3u; c = c + 1u) {
            if (c != b) {
                w = w * select(1.0 - fv[c], fv[c], (lo & (1u << c)) != 0u);
            }
        }
        w = w + 1e-4;
        let on = vec3<u32>(0u, 1u, 2u) == vec3<u32>(b);
        let sum_b = wall_sum[b] + w * ((f32(code) - 0.5) / 15.0);
        let weight_b = wall_weight[b] + w;
        wall_sum = select(wall_sum, vec3<f32>(sum_b), on);
        wall_weight = select(wall_weight, vec3<f32>(weight_b), on);
    }
    if (!any_wall) {
        return 0xffu;
    }
    if (leak_guard) {
        for (var e = 0u; e < 12u; e = e + 1u) {
            let b = e / 4u;
            let j = e % 4u;
            let lo = ((j >> b) << (b + 1u)) | (j & ((1u << b) - 1u));
            let hi = lo | (1u << b);
            if (((valid >> lo) & 1u) == 0u || ((valid >> hi) & 1u) == 0u) {
                blocked = blocked | (1u << e);
            }
        }
    }
    // Corner k's group label (0 to 7) in bits 4k to 4k + 3; each corner starts as its own.
    var label = 0x76543210u;
    // A path inside a cube has at most seven edges: seven sweeps settle every group's minimum.
    for (var sweep = 0u; sweep < 7u; sweep = sweep + 1u) {
        for (var e = 0u; e < 12u; e = e + 1u) {
            if (((blocked >> e) & 1u) != 0u) {
                continue;
            }
            let b = e / 4u;
            let j = e % 4u;
            let lo = ((j >> b) << (b + 1u)) | (j & ((1u << b) - 1u));
            let hi = lo | (1u << b);
            let m = min((label >> (4u * lo)) & 0xfu, (label >> (4u * hi)) & 0xfu);
            label = (label & ~(0xfu << (4u * lo))) | (m << (4u * lo));
            label = (label & ~(0xfu << (4u * hi))) | (m << (4u * hi));
        }
    }
    var anchor = 0u;
    for (var b = 0u; b < 3u; b = b + 1u) {
        var at = 0.5;
        if (wall_weight[b] > 0.0) {
            at = wall_sum[b] / wall_weight[b];
        }
        var take_high = fv[b] > at;
        if (leak_guard && (has_wall & (1u << b)) != 0u && !(wall_weight[b] > 0.0) && abs(nv[b]) >= BGI_WALL_NORMAL) {
            take_high = nv[b] > 0.0;
        }
        if (take_high) {
            anchor = anchor | (1u << b);
        }
    }
    // Each group's summed weight, at its label (two halves, as the weights).
    var sum_lo = vec4<f32>(0.0);
    var sum_hi = vec4<f32>(0.0);
    for (var k = 0u; k < 8u; k = k + 1u) {
        let l = (label >> (4u * k)) & 0xfu;
        let s = bgi_get8f(sum_lo, sum_hi, l) + bgi_get8f(weights_lo, weights_hi, k);
        sum_lo = select(sum_lo, vec4<f32>(s), bgi_lane_lo(l));
        sum_hi = select(sum_hi, vec4<f32>(s), bgi_lane_hi(l));
    }
    var best = (label >> (4u * anchor)) & 0xfu;
    if (!(bgi_get8f(sum_lo, sum_hi, best) > 0.0)) {
        best = 0u;
        for (var l = 1u; l < 8u; l = l + 1u) {
            if (bgi_get8f(sum_lo, sum_hi, l) > bgi_get8f(sum_lo, sum_hi, best)) {
                best = l;
            }
        }
    }
    var mask = 0u;
    for (var k = 0u; k < 8u; k = k + 1u) {
        if (((label >> (4u * k)) & 0xfu) == best) {
            mask = mask | (1u << k);
        }
    }
    return mask;
}

// The terrain layer or a brick at `p`: weighted SH (unnormalised) in sh, weight sum, coverage,
// whether it is an interior brick, and over the corners used the visible share (`bgi_corner`)
// and the interpolation weight (their ratio: how much of the cell sees the receiver).
struct BgiPart {
    sh: BgiSh,
    wsum: f32,
    cov: f32,
    interior: bool,
    vis: f32,
    tri: f32,
    // A brick's largest cell edge (m); 0 for the terrain layer (ADR-1104).
    edge: f32,
}

fn bgi_part_zero() -> BgiPart {
    return BgiPart(bgi_sh_zero(), 0.0, 0.0, false, 0.0, 0.0, 0.0);
}

fn bgi_ground(i: u32, j: u32) -> f32 {
    let bgi = bgi_u();
    return bitcast<f32>(bgi_word(bgi.words.w + j * bgi.counts.x + i));
}

fn bgi_terrain(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> BgiPart {
    let bgi = bgi_u();
    let levels = bgi.counts.z;
    if (levels == 0u) {
        return bgi_part_zero();
    }
    let nx = bgi.counts.x;
    let nz = bgi.counts.y;
    let origin = bgi.terrain.xy;
    let spacing = bgi.terrain.zw;
    // The bracket at the unbiased point sets the cell scale of the bias.
    let g0 = (p.xz - origin) / spacing;
    let n_max = vec2<f32>(f32(nx) - 1.0, f32(nz) - 1.0);
    let inset = min(min(g0.x, g0.y), min(n_max.x - g0.x, n_max.y - g0.y));
    // Full inside the outermost columns, fading to 0 one column spacing outside them.
    let cov_xz = clamp(inset + 1.0, 0.0, 1.0);
    if (cov_xz <= 0.0) {
        return bgi_part_zero();
    }
    let top = bgi_level(levels - 1u);
    var s = min(spacing.x, spacing.y);
    {
        let gc = clamp(g0, vec2<f32>(0.0), n_max);
        let i0 = vec2<u32>(min(floor(gc), max(n_max - vec2<f32>(1.0), vec2<f32>(0.0))));
        let i1 = min(i0 + vec2<u32>(1u), vec2<u32>(nx - 1u, nz - 1u));
        let f = gc - vec2<f32>(i0);
        let ground = mix(
            mix(bgi_ground(i0.x, i0.y), bgi_ground(i1.x, i0.y), f.x),
            mix(bgi_ground(i0.x, i1.y), bgi_ground(i1.x, i1.y), f.x),
            f.y,
        );
        let h = p.y - ground;
        var gap = bgi_level(0u);
        for (var l = 0u; l + 1u < levels; l = l + 1u) {
            if (h >= bgi_level(l)) {
                gap = bgi_level(l + 1u) - bgi_level(l);
            }
        }
        s = min(s, max(gap, 0.25));
    }
    let biased = p + (n * bgi.params.y + v * bgi.params.z) * s;
    let g = clamp((biased.xz - origin) / spacing, vec2<f32>(0.0), n_max);
    let i0 = vec2<u32>(min(floor(g), max(n_max - vec2<f32>(1.0), vec2<f32>(0.0))));
    let i1 = min(i0 + vec2<u32>(1u), vec2<u32>(nx - 1u, nz - 1u));
    let f = g - vec2<f32>(i0);
    let ground = mix(
        mix(bgi_ground(i0.x, i0.y), bgi_ground(i1.x, i0.y), f.x),
        mix(bgi_ground(i0.x, i1.y), bgi_ground(i1.x, i1.y), f.x),
        f.y,
    );
    let h = biased.y - ground;
    // The two levels around h (clamped to the lowest and highest).
    var l0 = 0u;
    var t = 0.0;
    if (h >= top) {
        l0 = levels - 1u;
    } else if (h > bgi_level(0u)) {
        for (var l = 0u; l + 1u < levels; l = l + 1u) {
            let lo = bgi_level(l);
            let hi = bgi_level(l + 1u);
            if (h >= lo && h < hi) {
                l0 = l;
                t = (h - lo) / max(hi - lo, 1e-4);
            }
        }
    }
    let l1 = min(l0 + 1u, levels - 1u);
    // Above the top level the probes still hold the open sky; fade out over the top's height
    // again (a flyer far above the layer falls back to the analytic terms).
    let cov_y = clamp(2.0 - h / max(top, 1e-3), 0.0, 1.0);
    if (cov_y <= 0.0) {
        return bgi_part_zero();
    }
    // Per corner, as two halves (`bgi_get8f`).
    var c_index_lo = vec4<u32>(0u);
    var c_index_hi = vec4<u32>(0u);
    var c_flags_lo = vec4<u32>(0u);
    var c_flags_hi = vec4<u32>(0u);
    var c_weight_lo = vec4<f32>(0.0);
    var c_weight_hi = vec4<f32>(0.0);
    var c_vis_lo = vec4<f32>(0.0);
    var c_vis_hi = vec4<f32>(0.0);
    var c_tri_lo = vec4<f32>(0.0);
    var c_tri_hi = vec4<f32>(0.0);
    var c_valid = 0u;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let ox = k & 1u;
        let oz = (k >> 1u) & 1u;
        let ol = (k >> 2u) & 1u;
        let ci = select(i0.x, i1.x, ox == 1u);
        let cj = select(i0.y, i1.y, oz == 1u);
        let cl = select(l0, l1, ol == 1u);
        let wx = select(1.0 - f.x, f.x, ox == 1u);
        let wz = select(1.0 - f.y, f.y, oz == 1u);
        let wl = select(1.0 - t, t, ol == 1u);
        let index = (cj * nx + ci) * levels + cl;
        let m = bgi_meta(index);
        let lane_lo = bgi_lane_lo(k);
        let lane_hi = bgi_lane_hi(k);
        c_index_lo = select(c_index_lo, vec4<u32>(index), lane_lo);
        c_index_hi = select(c_index_hi, vec4<u32>(index), lane_hi);
        c_flags_lo = select(c_flags_lo, vec4<u32>(u32(m.w)), lane_lo);
        c_flags_hi = select(c_flags_hi, vec4<u32>(u32(m.w)), lane_hi);
        if (!bgi_valid(m)) {
            continue;
        }
        let nominal = vec3<f32>(
            origin.x + f32(ci) * spacing.x,
            bgi_ground(ci, cj) + bgi_level(cl),
            origin.y + f32(cj) * spacing.y,
        );
        let w = bgi_corner(index, nominal + m.xyz, p, biased, n, s, wx * wz * wl);
        let tri = max(wx * wz * wl, 0.001);
        c_weight_lo = select(c_weight_lo, vec4<f32>(w.x), lane_lo);
        c_weight_hi = select(c_weight_hi, vec4<f32>(w.x), lane_hi);
        c_vis_lo = select(c_vis_lo, vec4<f32>(w.y), lane_lo);
        c_vis_hi = select(c_vis_hi, vec4<f32>(w.y), lane_hi);
        c_tri_lo = select(c_tri_lo, vec4<f32>(tri), lane_lo);
        c_tri_hi = select(c_tri_hi, vec4<f32>(tri), lane_hi);
        c_valid = c_valid | (1u << k);
    }
    // Corner bits: 0 = column x, 1 = column z, 2 = level (the flags' y axis).
    let wall_guard = (bgi.extra.z & BGI_LOOKUP_LEAK_GUARD) != 0u;
    let keep = c_valid & bgi_wall_mask(c_flags_lo, c_flags_hi, c_weight_lo, c_weight_hi, vec3<u32>(0u, 2u, 1u), vec3<bool>(i0.x == i1.x, i0.y == i1.y, l0 == l1), vec3<f32>(f, t), c_valid, n.xzy, wall_guard);
    var acc_l0 = vec3<f32>(0.0);
    var acc_r = vec3<f32>(0.0);
    var acc_g = vec3<f32>(0.0);
    var acc_b = vec3<f32>(0.0);
    var acc_wsum = 0.0;
    var acc_vis = 0.0;
    var acc_tri = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        if ((keep & (1u << k)) == 0u) {
            continue;
        }
        let weight = bgi_get8f(c_weight_lo, c_weight_hi, k);
        let sh = bgi_sh_add(BgiSh(acc_l0, acc_r, acc_g, acc_b), bgi_probe_sh(bgi_get8u(c_index_lo, c_index_hi, k)), weight);
        acc_l0 = sh.l0;
        acc_r = sh.r;
        acc_g = sh.g;
        acc_b = sh.b;
        acc_wsum = acc_wsum + weight;
        acc_vis = acc_vis + bgi_get8f(c_vis_lo, c_vis_hi, k);
        acc_tri = acc_tri + bgi_get8f(c_tri_lo, c_tri_hi, k);
    }
    return BgiPart(BgiSh(acc_l0, acc_r, acc_g, acc_b), acc_wsum, cov_xz * cov_y, false, acc_vis, acc_tri, 0.0);
}

// Brick `b`: origin (xyz) + first probe (w bits), spacing (xyz) + flags (w bits), counts (xyz).
fn bgi_brick_f(b: u32, k: u32) -> vec4<f32> {
    let bgi = bgi_u();
    let w = bgi.extra.x + b * BGI_BRICK_WORDS + k * 4u;
    return bitcast<vec4<f32>>(vec4<u32>(bgi_word(w), bgi_word(w + 1u), bgi_word(w + 2u), bgi_word(w + 3u)));
}

fn bgi_brick_u(b: u32, k: u32) -> vec4<u32> {
    let bgi = bgi_u();
    let w = bgi.extra.x + b * BGI_BRICK_WORDS + k * 4u;
    return vec4<u32>(bgi_word(w), bgi_word(w + 1u), bgi_word(w + 2u), bgi_word(w + 3u));
}

// The detail brick that holds `p` deepest (an interior brick first): its weighted SH and its
// blend weight in cov (1 a cell inside its faces, 0 at them; always 1 for an interior brick).
// An interior brick is tested at `p` pushed half its smallest cell along `n`: a receiver on
// an outer wall faces out of the box and reads the outdoor light, one on an inner wall the
// interior's. The test allows `BGI_INTERIOR_EDGE_SLACK` cells outside the probe box, so a floor
// pixel by a wall (a concave edge) does not flip to the outdoor light.
// Bricks the lookups tested since the host cleared it (ADR-2618 brick-loop probe).
var<private> bgi_visits: u32;

fn bgi_brick(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> BgiPart {
    let bgi = bgi_u();
    var best = 0u;
    var best_key = 0.0;
    var best_inset = 0.0;
    // Only the bricks the lookup grid lists for the cell holding `p` (ADR-1835, `BrickGrid`):
    // ascending index, so the first-highest key is the one the full scan picks.
    if (bgi.counts.w == 0u) {
        return bgi_part_zero();
    }
    let gw = bgi.extra.x + bgi.counts.w * BGI_BRICK_WORDS;
    let gdims = vec3<u32>(bgi_word(gw + 4u), bgi_word(gw + 5u), bgi_word(gw + 6u));
    let gc = (p - bitcast<vec3<f32>>(vec3<u32>(bgi_word(gw), bgi_word(gw + 1u), bgi_word(gw + 2u))))
        * bitcast<f32>(bgi_word(gw + 3u));
    // A NaN point fails the comparison and reads no brick.
    if (!(all(gc >= vec3<f32>(0.0)) && all(gc < vec3<f32>(gdims)))) {
        return bgi_part_zero();
    }
    let gi = vec3<u32>(gc);
    let cell = gw + BGI_GRID_HEADER_WORDS + 2u * ((gi.z * gdims.y + gi.y) * gdims.x + gi.x);
    let list = gw + bgi_word(gw + 7u) + bgi_word(cell);
    let listed = bgi_word(cell + 1u);
    bgi_visits = bgi_visits + listed;
    for (var k = 0u; k < listed; k = k + 1u) {
        let b = bgi_word(list + k);
        let o = bgi_brick_f(b, 0u).xyz;
        // Spacing and flags read as words: the flags never pass through a float.
        let spu = bgi_brick_u(b, 1u);
        let sp = bitcast<vec3<f32>>(spu.xyz);
        let c = vec3<f32>(bgi_brick_u(b, 2u).xyz);
        let interior = (spu.w & BGI_BRICK_INTERIOR) != 0u;
        var q = p;
        if (interior) {
            q = p + n * (0.5 * min(min(sp.x, sp.y), sp.z));
        }
        let g = (q - o) / sp;
        let hi = c - vec3<f32>(1.0) - g;
        let inset = min(min(min(g.x, g.y), g.z), min(min(hi.x, hi.y), hi.z));
        var key = inset;
        if (interior && inset + BGI_INTERIOR_EDGE_SLACK > 0.0) {
            key = inset + BGI_INTERIOR_EDGE_SLACK + BGI_INTERIOR_PRIORITY;
        }
        if (key > best_key) {
            best_key = key;
            best_inset = inset;
            best = b + 1u;
        }
    }
    if (best == 0u) {
        return bgi_part_zero();
    }
    let b = best - 1u;
    let o = bgi_brick_f(b, 0u);
    let spu = bgi_brick_u(b, 1u);
    let sp = bitcast<vec3<f32>>(spu.xyz);
    let cu = bgi_brick_u(b, 2u).xyz;
    let part_interior = (spu.w & BGI_BRICK_INTERIOR) != 0u;
    let part_edge = max(max(sp.x, sp.y), sp.z);
    let first = bitcast<u32>(o.w);
    let s = min(min(sp.x, sp.y), sp.z);
    let biased = p + (n * bgi.params.y + v * bgi.params.z) * s;
    let c_max = vec3<f32>(cu) - vec3<f32>(1.0);
    let g = clamp((biased - o.xyz) / sp, vec3<f32>(0.0), c_max);
    let i0 = vec3<u32>(min(floor(g), max(c_max - vec3<f32>(1.0), vec3<f32>(0.0))));
    let i1 = min(i0 + vec3<u32>(1u), cu - vec3<u32>(1u));
    let f = g - vec3<f32>(i0);
    // Per corner, as two halves (`bgi_get8f`).
    var c_index_lo = vec4<u32>(0u);
    var c_index_hi = vec4<u32>(0u);
    var c_flags_lo = vec4<u32>(0u);
    var c_flags_hi = vec4<u32>(0u);
    var c_weight_lo = vec4<f32>(0.0);
    var c_weight_hi = vec4<f32>(0.0);
    var c_vis_lo = vec4<f32>(0.0);
    var c_vis_hi = vec4<f32>(0.0);
    var c_tri_lo = vec4<f32>(0.0);
    var c_tri_hi = vec4<f32>(0.0);
    var c_valid = 0u;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let oc = vec3<u32>(k & 1u, (k >> 1u) & 1u, (k >> 2u) & 1u);
        let cell = select(i0, i1, oc == vec3<u32>(1u));
        let wa = select(vec3<f32>(1.0) - f, f, oc == vec3<u32>(1u));
        let index = first + (cell.z * cu.y + cell.y) * cu.x + cell.x;
        let m = bgi_meta(index);
        let lane_lo = bgi_lane_lo(k);
        let lane_hi = bgi_lane_hi(k);
        c_index_lo = select(c_index_lo, vec4<u32>(index), lane_lo);
        c_index_hi = select(c_index_hi, vec4<u32>(index), lane_hi);
        c_flags_lo = select(c_flags_lo, vec4<u32>(u32(m.w)), lane_lo);
        c_flags_hi = select(c_flags_hi, vec4<u32>(u32(m.w)), lane_hi);
        if (!bgi_valid(m)) {
            continue;
        }
        let nominal = o.xyz + vec3<f32>(cell) * sp;
        let w = bgi_corner(index, nominal + m.xyz, p, biased, n, s, wa.x * wa.y * wa.z);
        let tri = max(wa.x * wa.y * wa.z, 0.001);
        c_weight_lo = select(c_weight_lo, vec4<f32>(w.x), lane_lo);
        c_weight_hi = select(c_weight_hi, vec4<f32>(w.x), lane_hi);
        c_vis_lo = select(c_vis_lo, vec4<f32>(w.y), lane_lo);
        c_vis_hi = select(c_vis_hi, vec4<f32>(w.y), lane_hi);
        c_tri_lo = select(c_tri_lo, vec4<f32>(tri), lane_lo);
        c_tri_hi = select(c_tri_hi, vec4<f32>(tri), lane_hi);
        c_valid = c_valid | (1u << k);
    }
    let wall_guard = (bgi.extra.z & BGI_LOOKUP_LEAK_GUARD) != 0u;
    let keep = c_valid & bgi_wall_mask(c_flags_lo, c_flags_hi, c_weight_lo, c_weight_hi, vec3<u32>(0u, 1u, 2u), i0 == i1, f, c_valid, n, wall_guard);
    var acc_l0 = vec3<f32>(0.0);
    var acc_r = vec3<f32>(0.0);
    var acc_g = vec3<f32>(0.0);
    var acc_b = vec3<f32>(0.0);
    var acc_wsum = 0.0;
    var acc_vis = 0.0;
    var acc_tri = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        if ((keep & (1u << k)) == 0u) {
            continue;
        }
        let weight = bgi_get8f(c_weight_lo, c_weight_hi, k);
        let sh = bgi_sh_add(BgiSh(acc_l0, acc_r, acc_g, acc_b), bgi_probe_sh(bgi_get8u(c_index_lo, c_index_hi, k)), weight);
        acc_l0 = sh.l0;
        acc_r = sh.r;
        acc_g = sh.g;
        acc_b = sh.b;
        acc_wsum = acc_wsum + weight;
        acc_vis = acc_vis + bgi_get8f(c_vis_lo, c_vis_hi, k);
        acc_tri = acc_tri + bgi_get8f(c_tri_lo, c_tri_hi, k);
    }
    var cov = clamp(best_inset, 0.0, 1.0);
    if (part_interior) {
        cov = 1.0;
    }
    return BgiPart(BgiSh(acc_l0, acc_r, acc_g, acc_b), acc_wsum, cov, part_interior, acc_vis, acc_tri, part_edge);
}

// How much of a part's cell sees the receiver (0 with no corner used).
fn bgi_part_visibility(part: BgiPart) -> f32 {
    if (part.tri <= 0.0) {
        return 0.0;
    }
    return clamp(part.vis / part.tri, 0.0, 1.0);
}

// The baked irradiance at `p` (normal `n`, `v` toward the viewer): summed SH (scaled by the
// intensity), coverage and visibility. A brick overrides the terrain layer, blended over its
// last cell; an interior brick replaces it outright (with no valid corner it reads black at
// full coverage, so no outdoor term leaks in). Where both blend, each layer's share of the
// light is also weighted by how much of its cell sees the receiver (`BGI_SEEN_FLOOR` keeps
// the plain blend when neither does): a layer whose probes all sit behind a wall yields to
// the one that sees the point, and the coverage (what the passes blend the analytic terms
// by) is unchanged.
fn static_gi(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> StaticGi {
    let bgi = bgi_u();
    let br = bgi_brick(p, n, v);
    var wb = 0.0;
    var vb = 0.0;
    if (br.wsum > 0.0 && br.cov > 0.0) {
        wb = br.cov;
        vb = bgi_part_visibility(br);
    }
    var cov = wb;
    var wt = 0.0;
    var vt = 0.0;
    // The terrain part's SH and weight sum, kept only where its share `wt` is set.
    var tr_l0 = vec3<f32>(0.0);
    var tr_r = vec3<f32>(0.0);
    var tr_g = vec3<f32>(0.0);
    var tr_b = vec3<f32>(0.0);
    var tr_wsum = 0.0;
    if (br.interior) {
        cov = 1.0;
    } else if (wb < 1.0) {
        let tr = bgi_terrain(p, n, v);
        if (tr.wsum > 0.0 && tr.cov > 0.0) {
            // The terrain share is what the brick leaves, at the terrain's own coverage.
            wt = (1.0 - wb) * tr.cov;
            vt = bgi_part_visibility(tr);
            cov = cov + wt;
            tr_l0 = tr.sh.l0;
            tr_r = tr.sh.r;
            tr_g = tr.sh.g;
            tr_b = tr.sh.b;
            tr_wsum = tr.wsum;
        }
    }
    if (cov <= 1e-5) {
        return StaticGi(bgi_sh_zero(), 0.0, 0.0, vec4<f32>(0.0));
    }
    let kb = wb * (vb + BGI_SEEN_FLOOR);
    let kt = wt * (vt + BGI_SEEN_FLOOR);
    let k = kb + kt;
    var sh_l0 = vec3<f32>(0.0);
    var sh_r = vec3<f32>(0.0);
    var sh_g = vec3<f32>(0.0);
    var sh_b = vec3<f32>(0.0);
    if (kb > 0.0) {
        let sh = bgi_sh_mul(br.sh, kb / br.wsum);
        sh_l0 = sh.l0;
        sh_r = sh.r;
        sh_g = sh.g;
        sh_b = sh.b;
    }
    if (kt > 0.0) {
        let sh = bgi_sh_add(BgiSh(sh_l0, sh_r, sh_g, sh_b), BgiSh(tr_l0, tr_r, tr_g, tr_b), kt / tr_wsum);
        sh_l0 = sh.l0;
        sh_r = sh.r;
        sh_g = sh.g;
        sh_b = sh.b;
    }
    // Normalise to full strength: coverage carries the fade, the light is the covered part's.
    if (k > 0.0) {
        return StaticGi(
            bgi_sh_mul(BgiSh(sh_l0, sh_r, sh_g, sh_b), bgi.params.w / k),
            cov,
            (wb * vb + wt * vt) / (wb + wt),
            vec4<f32>(
                kb / k,
                kt / k,
                select(0.0, br.edge, kb > 0.0),
                select(0.0, max(bgi.terrain.z, bgi.terrain.w), kt > 0.0),
            ),
        );
    }
    return StaticGi(bgi_sh_zero(), cov, 0.0, vec4<f32>(0.0));
}
