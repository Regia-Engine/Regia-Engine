// SPDX-License-Identifier: MPL-2.0
// GPU irradiance probes: the shading lookup (see ddgi_common.wgsl for the bindings a host
// declares). Appended after ddgi_common.wgsl.

// Bilinear lookup in a border-padded octahedral tile: (lower texel, weights).
fn ddgi_oct_bilinear(dir: vec3<f32>, res: u32) -> vec4<f32> {
    let e = ddgi_oct_encode(dir) * 0.5 + vec2<f32>(0.5);
    let pos = clamp(e * f32(res) - vec2<f32>(0.5), vec2<f32>(-0.5), vec2<f32>(f32(res) - 0.5));
    let fl = floor(pos);
    return vec4<f32>(fl + vec2<f32>(1.0), pos - fl);
}

fn ddgi_irradiance_texel(index: u32, dir: vec3<f32>) -> vec3<f32> {
    let b = ddgi_oct_bilinear(dir, DDGI_IRR_RES);
    let t = vec2<u32>(b.xy);
    let a = ddgi_packed_irr_load(index, t);
    let c = ddgi_packed_irr_load(index, t + vec2<u32>(1u, 0u));
    let d = ddgi_packed_irr_load(index, t + vec2<u32>(0u, 1u));
    let e = ddgi_packed_irr_load(index, t + vec2<u32>(1u, 1u));
    return mix(mix(a, c, b.z), mix(d, e, b.z), b.w);
}

// Mean and mean² of the hit distance toward `dir`.
fn ddgi_moments(index: u32, dir: vec3<f32>) -> vec2<f32> {
    let b = ddgi_oct_bilinear(dir, DDGI_VIS_RES);
    let t = vec2<u32>(b.xy);
    let a = ddgi_vis_load(index, t);
    let c = ddgi_vis_load(index, t + vec2<u32>(1u, 0u));
    let d = ddgi_vis_load(index, t + vec2<u32>(0u, 1u));
    let e = ddgi_vis_load(index, t + vec2<u32>(1u, 1u));
    return mix(mix(a, c, b.z), mix(d, e, b.z), b.w);
}

// How far inside cascade `c` (in cells) `p` lies; < 0.5 is too close to its edge to use.
fn ddgi_inset(c: u32, p: vec3<f32>) -> f32 {
    let cas = ddgi_cascade(c);
    let g = p / cas.origin.w - vec3<f32>(cas.base.xyz);
    let hi = vec3<f32>(ddgi.dims.xyz) - vec3<f32>(1.0) - g;
    return min(min(min(g.x, g.y), g.z), min(min(hi.x, hi.y), hi.z));
}

// How far inside cascade `c`'s blend window (in cells) `p` lies. The window is measured from
// the volume's unsnapped corner (`origin.xyz`), which moves with the eye, so the blend into
// the next cascade does not jump when the volume scrolls by a cell (`ddgi_inset` from the
// snapped `base` jumps by a whole cell there: a pop in a band at every cascade edge). Its far
// side stops a cell short, so `ddgi_inset(c, p)` ≥ this value at every eye position (the
// snapped corner lies within one cell below the unsnapped one) and the probes a weight > 0
// reads are always inside the volume.
fn ddgi_blend_inset(c: u32, p: vec3<f32>) -> f32 {
    let cas = ddgi_cascade(c);
    let g = p / cas.origin.w - cas.origin.xyz;
    let hi = vec3<f32>(ddgi.dims.xyz) - vec3<f32>(2.0) - g;
    return min(min(min(g.x, g.y), g.z), min(min(hi.x, hi.y), hi.z));
}

// `ctrl.w` bit 14: the layout has a cascade too coarse to resolve a room (`DdgiSettings::needs_room_rule`, every
// tier; ADR-1949), so its finest cascade holds the room around the eye.
const DDGI_CTRL_ROOM_RULE: u32 = 16384u;
// The margin of the finest cascade's blend window on such a layout, cells (`ddgi_blend_weight`, ADR-5909).
const DDGI_ROOM_MARGIN: f32 = 0.25;

// The share of its turn cascade `c` takes at `p` by its blend window: nothing within a margin of 0.75 cells of the
// window's end, all of it a cell further in. The margin keeps a point off the row of probes at the volume's face,
// which a scroll has just started over.
//
// On a layout that needs the room rule the finest cascade's margin is a quarter cell (ADR-5909); the ramp is the
// same cell wide, so the blend moves no faster under a moving eye. Past its window a point reads a cascade three
// or four times as coarse, which holds one probe where the finest holds a column of them, and on the tiers a probe
// that starts over is seeded from its neighbor (`DdgiSettings::seed_updates`, ADR-2605), so the row at the face
// holds usable light from its first update. The wide margin cost half a cell of the finest probes at each end of
// each axis: a view up a shaft read the 6 m cascade's two probes from 4.5 m over the eye on the Low tier (0.140
// against the path tracer where the quarter-cell margin reads 0.117). The volumes stand where they stood; the
// lookup uses more of the probes they hold.
fn ddgi_blend_weight(c: u32, p: vec3<f32>) -> f32 {
    let room = c == 0u && (ddgi.ctrl.w & DDGI_CTRL_ROOM_RULE) != 0u;
    return clamp(ddgi_blend_inset(c, p) - select(0.75, DDGI_ROOM_MARGIN, room), 0.0, 1.0);
}

// Between the probes each face of the lookup is interpolated as its cube root and cubed again (ADR-5506): the
// blend runs in a lightness space (CIE 1976 L* is a cube root of luminance), as Majercik et al. 2019 (listing 1)
// blend at a gamma of 2 and store at a gamma of 5. Irradiance falls off faster than linearly from a bright surface
// that is small or close (a sunlit strip at a wall's foot, the daylight at a tunnel's mouth or a doorway): a linear
// blend between the probe beside it and the next one a cell away reads too much everywhere between the two, and
// each bounce reads it again. As cube roots the blend bows toward the darker probe: between probes of like light
// it changes nothing (0.4 % less at a ratio of 0.8), halfway between a lit probe and an unlit one it reads an
// eighth of the lit one where the linear blend reads half. Measured on an RTX 5080 against the linear blend, the
// square root and the cube root (Desktop High mean FLIP): trench 0.0956 / 0.0803 / 0.0793 with the hit model of
// ADR-5506, the Low tier's door room 0.319 / 0.255 / 0.238; a tunnel and a shaft, which hold too little light
// already, read 0.004 and 0.002 worse with the cube root than with the square root.
fn ddgi_blend_root(e: vec3<f32>) -> vec3<f32> {
    return pow(max(e, vec3<f32>(0.0)), vec3<f32>(1.0 / 3.0));
}

// How far (in cells) a surface-point lookup looks along the normal to pick its cell (ADR-5517).
const DDGI_PLANE_PICK: f32 = 0.01;

// The base cell of a lookup at grid point `g` (normal `n`). With the weights at the surface point
// (`ddgi_surface_weights`, ADR-5374) a floor or wall that lies on a probe plane sits exactly on a cell
// boundary, and the rasterized position's rounding moved `floor(g)` across it pixel by pixel: the
// layer that gets the trilinear floor weight (`tri`'s 0.001) was then the one in front of the surface
// on some pixels and the one behind it on others. The in-plane probes' own weights are crushed there,
// so that floor weight decided the light and the plane showed dark streaks (the quick 3D golden's
// ground). The cell is picked from the point `DDGI_PLANE_PICK` cells along the normal, so the far
// layer is always the one in front; the weights still come from `g`, so a point further than that
// from a probe plane reads exactly as before, and one inside the band reads a weight at most
// `DDGI_PLANE_PICK` outside 0..1 on that axis. A layout without the surface weights is unchanged.
fn ddgi_lookup_base(g: vec3<f32>, n: vec3<f32>) -> vec3<i32> {
    let picked = select(g, g + n * DDGI_PLANE_PICK, ddgi_surface_weights());
    return vec3<i32>(floor(picked));
}

// Irradiance / π from cascade `c` at `p` (normal `n`, `v` toward the viewer); a = how much of the
// cascade sees `p`: the sum of its traced probes' weights (visibility and normal side, times their
// trilinear weights, which sum to 1 over all eight corners), so a corner that has not traced, is inside
// geometry or stands behind a wall counts as not seeing it. > 0 whenever any traced probe counted
// (ADR-2605 compares it between cascades).
fn ddgi_sample_cascade(c: u32, p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> vec4<f32> {
    let cas = ddgi_cascade(c);
    let s = cas.origin.w;
    // The bias moves the point the visibility test looks at (Majercik et al. 2019, listing 1: a test at the
    // surface would shadow itself). On a tier layout (`ddgi_surface_weights`, ADR-5374) the cell and the
    // trilinear weights are those of the surface point: taken at the biased point, a wall's light came 0.1 to
    // 0.4 cells in front of the wall, where a room's own light is stronger, and each bounce read it there again
    // (a door room at 1.15 times the tracer).
    let biased = p + (n * ddgi.params.y + v * ddgi.params.z) * s;
    let g = select(biased, p, ddgi_surface_weights()) / s;
    let ibase = ddgi_lookup_base(g, n);
    let f = g - vec3<f32>(ibase);
    var sum = vec3<f32>(0.0);
    var wsum = 0.0;
    // The three faces of the ambient cube `n` reads, each summed over the probes as its cube root
    // (`ddgi_blend_root`, ADR-5506).
    let n2 = n * n;
    let jx = select(0u, 1u, n.x < 0.0);
    let jy = select(2u, 3u, n.y < 0.0);
    let jz = select(4u, 5u, n.z < 0.0);
    var ex = vec3<f32>(0.0);
    var ey = vec3<f32>(0.0);
    var ez = vec3<f32>(0.0);
    for (var k = 0u; k < 8u; k = k + 1u) {
        let o = vec3<i32>(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        let cell = ibase + o;
        let index = ddgi_slot(c, cell);
        let hw = ddgi_header_word(index);
        let head = ddgi_data[hw];
        let off_word = ddgi_data[hw + 1u];
        if (head != ddgi_pack_cell(cell, true) || (off_word >> 31u) == 0u) {
            continue;
        }
        let offset = ddgi_unpack_offset(off_word, s);
        let tri_axes = mix(vec3<f32>(1.0) - f, f, vec3<f32>(o));
        let tri = max(tri_axes.x * tri_axes.y * tri_axes.z, 0.001);
        let probe_pos = vec3<f32>(cell) * s + offset.xyz;
        let to_probe = ddgi_dir_to_probe(probe_pos, p, n);
        var w = (dot(to_probe, n) + 1.0) * 0.5;
        w = w * w + 0.2;
        let from_probe = biased - probe_pos;
        let r = max(length(from_probe), 1e-4);
        let m = ddgi_moments(index, from_probe / r);
        if (r > m.x) {
            let variance = max(abs(m.y - m.x * m.x), 1e-4 * s * s);
            let d = r - m.x;
            var cheb = variance / (variance + d * d);
            cheb = ddgi_edge_visibility(cheb, index, probe_pos, p, n, v, s);
            cheb = cheb * cheb * cheb;
            w = w * max(cheb, 0.0);
        }
        w = max(w, 1e-6);
        w = w * ddgi_front_weight(probe_pos, p, n, v, s);
        // Crush tiny weights so a barely visible probe cannot tint the result.
        if (w < 0.2) {
            w = w * w * w / (0.2 * 0.2);
        }
        w = w * tri;
        if (ddgi_tier_lookup()) {
            // The ambient cube of the axis texels, each face at the probe or at the surface behind it
            // (ADR-5457): what the shading reads, so a probe's own bounce and the frame agree; this read feeds
            // a bounce, so it takes a far crease only where it adds light (`ddgi_crease_gain`, ADR-5552).
            let relp = p - probe_pos;
            let backs = ddgi_surface_word(index);
            ex = ex + w * ddgi_blend_root(ddgi_axis_irradiance(index, jx, relp, s, backs, true));
            ey = ey + w * ddgi_blend_root(ddgi_axis_irradiance(index, jy, relp, s, backs, true));
            ez = ez + w * ddgi_blend_root(ddgi_axis_irradiance(index, jz, relp, s, backs, true));
        } else {
            sum = sum + w * ddgi_irradiance_texel(index, n);
        }
        wsum = wsum + w;
    }
    // Any traced probe counts, however small its weight: falling back to the sky when every
    // probe is behind a wall would leak the sky into closed rooms.
    if (wsum <= 0.0) {
        return vec4<f32>(0.0);
    }
    if (ddgi_tier_lookup()) {
        let rx = ex / wsum;
        let ry = ey / wsum;
        let rz = ez / wsum;
        let e = rx * rx * rx * n2.x + ry * ry * ry * n2.y + rz * rz * rz * n2.z;
        return vec4<f32>(e / max(n2.x + n2.y + n2.z, 1e-6), max(wsum, 1e-30));
    }
    return vec4<f32>(sum / wsum, max(wsum, 1e-30));
}

// ADR-2605 (`DdgiSettings::visibility_blend`, `ctrl.w` bit 16): the share of a coarser cascade's
// blend weight it keeps, from how much of it sees the point (`coarse`) against the finest cascade
// that holds it (`fine`, both `ddgi_sample_cascade`'s a). Below three quarters of the finest one's
// the rest goes back to the finest cascade: its probes resolve the room the point is in.
fn ddgi_vis_keep(coarse: f32, fine: f32) -> f32 {
    return clamp(coarse / max(0.75 * fine, 1e-30), 0.0, 1.0);
}

// ADR-2605: where the finest cascade sees the point (`fine_vis` of its visible mass at least this),
// a coarser cascade's light there is capped at `DDGI_VIS_CAP` times the finest one's. A coarse probe
// that stands inside a tall room sees its ceiling as well as the fine probes do, but its own light was
// gathered at the coarse spacing, which cannot resolve the roof: on open ground the cascades agree and
// the cap never binds.
const DDGI_VIS_SEEN: f32 = 0.2;
const DDGI_VIS_CAP: f32 = 1.25;

// How much of the blend's keep and cap apply at `p` with cascade `c` the finest that holds it. Full a
// quarter cell inside `c`'s unsnapped window, 0 at its end. `edge` changes where the snapped inset
// crosses 0, which jumps a whole cell on each scroll; the snapped inset is never below the unsnapped
// one, so this has reached 0 by then and the seam moves with the eye (the pop ADR-424 removed from the
// cascade blend).
fn ddgi_vis_fade(c: u32, p: vec3<f32>) -> f32 {
    return clamp(ddgi_blend_inset(c, p) / 0.25, 0.0, 1.0);
}

// `fade` is `ddgi_vis_fade`. The cap also fades in over the last half of `DDGI_VIS_SEEN`: a scroll
// resets the probes at the finest cascade's face and moves its visible mass across that bound.
fn ddgi_vis_cap(coarse: vec3<f32>, fine: vec3<f32>, fine_vis: f32, fade: f32) -> vec3<f32> {
    let seen = clamp((fine_vis - 0.5 * DDGI_VIS_SEEN) / (0.5 * DDGI_VIS_SEEN), 0.0, 1.0);
    return mix(coarse, min(coarse, fine * DDGI_VIS_CAP + vec3<f32>(1e-6)), seen * fade);
}

fn ddgi_vis_blend_on() -> bool {
    return (ddgi.ctrl.w & DDGI_CTRL_VISIBILITY_BLEND) != 0u;
}

// ADR-5374: whether the lookups (shading and the probes' own ray read) weight the probes at the surface point
// and keep the normal and view bias for the visibility test alone. It rides the visibility blend's bit
// (`DdgiSettings::visibility_blend`, `ddgi_tier_lookup`), which every tier sets: a layout without it keeps the
// weights at the biased point. On a surface that lies on a probe plane those average the probe layer on it with
// the one a cell in front of it: the over-read, and about a quarter less of the probes' update noise, which the
// unseeded lab layout's walk bar is sized for (`tests/ddgi_temporal.rs`: 1.85 luma levels against 2.33 at the
// surface point, bar 2.11).
fn ddgi_surface_weights() -> bool {
    return ddgi_tier_lookup();
}

// Irradiance / π at `p` from the finest cascade that holds it, blended into the next one
// over the last cell of its blend window (`ddgi_blend_inset`). A point within a cascade's
// probes but past every blend window still reads that cascade (the trace's bounce light
// there). a = coverage: 0 inside a cascade whose probes have not traced yet, −1 outside every
// cascade.
fn ddgi_irradiance(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> vec4<f32> {
    return ddgi_irradiance_traced(p, n, v, 99u);
}

// `ddgi_irradiance` for a ray hit of a probe in cascade `limit`: wherever that cascade's own
// probes (or a finer cascade's) hold `p`, nothing coarser is read. A probe's light therefore
// comes from probes at least as fine as itself, whose spacing can tell the two sides of a thin
// wall apart; only a hit beyond its own volume takes a coarser cascade (ADR-1949). Without this
// the 18 and 54 m cascades, relocated out of a small room, carried the sky in through its walls.
fn ddgi_irradiance_traced(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, limit: u32) -> vec4<f32> {
    if (ddgi_vis_blend_on()) {
        return ddgi_irradiance_traced_vis(p, n, v, limit);
    }
    return ddgi_irradiance_traced_plain(p, n, v, limit);
}

// `ddgi_irradiance_traced` without the visibility blend, whatever `ctrl.w` says: the probes' own ray
// read (`ddgi_trace.wgsl`). The blend is for shading (ADR-2605): the ray rule already keeps a probe's
// hits on its own cascade or finer, and in the probes' far-field reads (through a door, across open
// ground) the blend's keep and cap moved light the room rule never needed to touch (a door room's floor
// read 0.0115 against the 1 m layout's 0.00494 with it, 0.00706 without).
fn ddgi_irradiance_traced_plain(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, limit: u32) -> vec4<f32> {
    var sum = vec3<f32>(0.0);
    var left = 1.0;
    var covered = 0.0;
    var edge = 99u;
    for (var c = 0u; c < ddgi.dims.w; c = c + 1u) {
        if (left <= 1e-3) {
            break;
        }
        let inset = ddgi_inset(c, p);
        if (inset >= 0.0 && edge == 99u) {
            edge = c;
        }
        var w = ddgi_blend_weight(c, p);
        if (c >= limit && inset >= 0.0) {
            w = 1.0;
        }
        if (w <= 0.0) {
            continue;
        }
        let r = ddgi_sample_cascade(c, p, n, v);
        if (r.a <= 0.0) {
            continue;
        }
        let take = w * left;
        sum = sum + r.rgb * take;
        covered = covered + take;
        left = left - take;
    }
    if (covered <= 1e-5) {
        if (edge == 99u) {
            return vec4<f32>(0.0, 0.0, 0.0, -1.0);
        }
        let r = ddgi_sample_cascade(edge, p, n, v);
        if (r.a <= 0.0) {
            return vec4<f32>(0.0);
        }
        return vec4<f32>(r.rgb * ddgi.params.w, 1.0);
    }
    return vec4<f32>(sum / covered * ddgi.params.w, covered);
}

// `ddgi_irradiance_traced` with the visibility blend (ADR-2605): the finest cascade whose probes hold
// `p` is sampled first, and each coarser cascade keeps only `ddgi_vis_keep` of its blend share; the
// rest reads the finest one. A separate function so the default lookup's code is unchanged.
fn ddgi_irradiance_traced_vis(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, limit: u32) -> vec4<f32> {
    var sum = vec3<f32>(0.0);
    var left = 1.0;
    var covered = 0.0;
    var edge = 99u;
    var fine = vec4<f32>(0.0);
    var fade = 0.0;
    var to_fine = 0.0;
    for (var c = 0u; c < ddgi.dims.w; c = c + 1u) {
        if (left <= 1e-3) {
            break;
        }
        let inset = ddgi_inset(c, p);
        if (inset >= 0.0 && edge == 99u) {
            edge = c;
            fine = ddgi_sample_cascade(c, p, n, v);
            fade = ddgi_vis_fade(c, p);
        }
        var w = ddgi_blend_weight(c, p);
        if (c >= limit && inset >= 0.0) {
            w = 1.0;
        }
        if (w <= 0.0) {
            continue;
        }
        var r = fine;
        if (c != edge) {
            r = ddgi_sample_cascade(c, p, n, v);
        }
        if (r.a <= 0.0) {
            continue;
        }
        let take = w * left;
        var keep = 1.0;
        var light = r.rgb;
        if (c > edge && edge != 99u && fine.a > 0.0) {
            keep = mix(1.0, ddgi_vis_keep(r.a, fine.a), fade);
            light = ddgi_vis_cap(r.rgb, fine.rgb, fine.a, fade);
        }
        sum = sum + light * (take * keep);
        to_fine = to_fine + take * (1.0 - keep);
        covered = covered + take;
        left = left - take;
    }
    sum = sum + fine.rgb * to_fine;
    if (covered <= 1e-5) {
        if (edge == 99u) {
            return vec4<f32>(0.0, 0.0, 0.0, -1.0);
        }
        if (fine.a <= 0.0) {
            return vec4<f32>(0.0);
        }
        return vec4<f32>(fine.rgb * ddgi.params.w, 1.0);
    }
    return vec4<f32>(sum / covered * ddgi.params.w, covered);
}

// Ambient cube (six axis directions, +x −x +y −y +z −z): the screen lookup stores the probe
// irradiance toward each, and a pixel rebuilds E(n) = n.x² E(±x) + n.y² E(±y) + n.z² E(±z)
// for its own normal. Exact for axis-aligned normals (floors, walls); a linear (L1) rebuild
// from four tetrahedron directions under-read an up-facing canyon floor by 7–9 %, the
// irradiance there falling off faster than linearly toward the walls.
// Built by `select`, not `d[k >> 1u] = ...`: FXC cannot store to a vector component at a
// dynamic index and unrolls every enclosing loop to make it constant, which fails for the
// cascade loop (X3550 then X3511, ADR-1583).
fn ddgi_cube_dir(k: u32) -> vec3<f32> {
    let s = select(1.0, -1.0, (k & 1u) == 1u);
    let axis = k >> 1u;
    return vec3<f32>(select(0.0, s, axis == 0u), select(0.0, s, axis == 1u), select(0.0, s, axis == 2u));
}

// Irradiance / π toward the six cube directions from cascade `c` at `p`, with the probe
// weights (visibility, normal side, trilinear) of the surface normal `n` computed once.
// cov = 1 when any traced probe contributed, else 0. The functions accumulate in six named
// vectors and build the struct at the end. Not in the struct: FXC cannot store to an array inside
// a struct at a dynamic index (`acc.e[j] = ...`) and unrolls every enclosing loop to make the index
// constant, which fails for the cascade loop, whose bound is a uniform (X3550 then X3511,
// ADR-1583). Not in a local array either: a function-local array carries naga's `ArrayStride`,
// which Vulkan forbids (ADR-026; the shader matrix's `ddgi_screen.wgsl`, `ddgi_trace.wgsl` and
// `radiance_cache_update.wgsl` layout scans, ADR-2644). The struct is only ever a `let` value
// read at constant indices, so it needs no variable.
struct DdgiBasis {
    e: array<vec3<f32>, 6>,
    cov: f32,
}

fn ddgi_sample_cascade_basis(c: u32, p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> DdgiBasis {
    let cas = ddgi_cascade(c);
    let s = cas.origin.w;
    // The cell and the trilinear weights as `ddgi_sample_cascade` takes them (ADR-5374).
    let biased = p + (n * ddgi.params.y + v * ddgi.params.z) * s;
    let g = select(biased, p, ddgi_surface_weights()) / s;
    let ibase = ddgi_lookup_base(g, n);
    let f = g - vec3<f32>(ibase);
    var e0 = vec3<f32>(0.0);
    var e1 = vec3<f32>(0.0);
    var e2 = vec3<f32>(0.0);
    var e3 = vec3<f32>(0.0);
    var e4 = vec3<f32>(0.0);
    var e5 = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let o = vec3<i32>(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        let cell = ibase + o;
        let index = ddgi_slot(c, cell);
        let hw = ddgi_header_word(index);
        let head = ddgi_data[hw];
        let off_word = ddgi_data[hw + 1u];
        if (head != ddgi_pack_cell(cell, true) || (off_word >> 31u) == 0u) {
            continue;
        }
        let offset = ddgi_unpack_offset(off_word, s);
        let tri_axes = mix(vec3<f32>(1.0) - f, f, vec3<f32>(o));
        let tri = max(tri_axes.x * tri_axes.y * tri_axes.z, 0.001);
        let probe_pos = vec3<f32>(cell) * s + offset.xyz;
        let to_probe = ddgi_dir_to_probe(probe_pos, p, n);
        var w = (dot(to_probe, n) + 1.0) * 0.5;
        w = w * w + 0.2;
        let from_probe = biased - probe_pos;
        let r = max(length(from_probe), 1e-4);
        let m = ddgi_moments(index, from_probe / r);
        if (r > m.x) {
            let variance = max(abs(m.y - m.x * m.x), 1e-4 * s * s);
            let d = r - m.x;
            var cheb = variance / (variance + d * d);
            cheb = ddgi_edge_visibility(cheb, index, probe_pos, p, n, v, s);
            cheb = cheb * cheb * cheb;
            w = w * max(cheb, 0.0);
        }
        w = max(w, 1e-6);
        w = w * ddgi_front_weight(probe_pos, p, n, v, s);
        if (w < 0.2) {
            w = w * w * w / (0.2 * 0.2);
        }
        w = w * tri;
        if (ddgi_tier_lookup()) {
            // Each face at the probe, or at the surface behind it where the point lies there (ADR-5457),
            // summed as its cube root (`ddgi_blend_root`, ADR-5506).
            // Named faces, not a loop over a local array (ADR-026, ADR-2644: `DdgiBasis`).
            let relp = p - probe_pos;
            let backs = ddgi_surface_word(index);
            e0 = e0 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 0u, relp, s, backs, false));
            e1 = e1 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 1u, relp, s, backs, false));
            e2 = e2 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 2u, relp, s, backs, false));
            e3 = e3 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 3u, relp, s, backs, false));
            e4 = e4 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 4u, relp, s, backs, false));
            e5 = e5 + w * ddgi_blend_root(ddgi_axis_irradiance(index, 5u, relp, s, backs, false));
        } else {
            e0 = e0 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(0u));
            e1 = e1 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(1u));
            e2 = e2 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(2u));
            e3 = e3 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(3u));
            e4 = e4 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(4u));
            e5 = e5 + w * ddgi_irradiance_texel(index, ddgi_cube_dir(5u));
        }
        wsum = wsum + w;
    }
    if (wsum <= 0.0) {
        return DdgiBasis(array<vec3<f32>, 6>(e0, e1, e2, e3, e4, e5), 0.0);
    }
    if (ddgi_tier_lookup()) {
        // The faces were summed as cube roots (`ddgi_blend_root`, ADR-5506). `let` values only: no local
        // array variable (ADR-026).
        let r0 = e0 / wsum;
        let r1 = e1 / wsum;
        let r2 = e2 / wsum;
        let r3 = e3 / wsum;
        let r4 = e4 / wsum;
        let r5 = e5 / wsum;
        let cubed = array<vec3<f32>, 6>(
            r0 * r0 * r0,
            r1 * r1 * r1,
            r2 * r2 * r2,
            r3 * r3 * r3,
            r4 * r4 * r4,
            r5 * r5 * r5,
        );
        return DdgiBasis(cubed, max(wsum, 1e-30));
    }
    let e = array<vec3<f32>, 6>(e0 / wsum, e1 / wsum, e2 / wsum, e3 / wsum, e4 / wsum, e5 / wsum);
    // cov > 0: `ddgi_sample_cascade`'s visible weight (ADR-2605).
    return DdgiBasis(e, max(wsum, 1e-30));
}

// `ddgi_irradiance` toward the six cube directions (the same cascade choice and blend), for
// shading. cov = coverage as `ddgi_irradiance`'s a: −1 outside every cascade. Past every blend
// window (the last cascade's outer shell) cov is 0, not the full cascade: the coverage has
// faded to 0 over the window, and jumping back to 1 there drew a ring of probe light beyond the
// fade that moved with each scroll (ADR-424).
fn ddgi_irradiance_basis(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> DdgiBasis {
    if (ddgi_vis_blend_on()) {
        return ddgi_irradiance_basis_vis(p, n, v);
    }
    var e0 = vec3<f32>(0.0);
    var e1 = vec3<f32>(0.0);
    var e2 = vec3<f32>(0.0);
    var e3 = vec3<f32>(0.0);
    var e4 = vec3<f32>(0.0);
    var e5 = vec3<f32>(0.0);
    var left = 1.0;
    var covered = 0.0;
    var edge = 99u;
    for (var c = 0u; c < ddgi.dims.w; c = c + 1u) {
        if (left <= 1e-3) {
            break;
        }
        let inset = ddgi_inset(c, p);
        if (inset >= 0.0 && edge == 99u) {
            edge = c;
        }
        let w = ddgi_blend_weight(c, p);
        if (w <= 0.0) {
            continue;
        }
        let r = ddgi_sample_cascade_basis(c, p, n, v);
        if (r.cov <= 0.0) {
            continue;
        }
        let take = w * left;
        e0 = e0 + r.e[0] * take;
        e1 = e1 + r.e[1] * take;
        e2 = e2 + r.e[2] * take;
        e3 = e3 + r.e[3] * take;
        e4 = e4 + r.e[4] * take;
        e5 = e5 + r.e[5] * take;
        covered = covered + take;
        left = left - take;
    }
    if (covered <= 1e-5) {
        let e = array<vec3<f32>, 6>(e0, e1, e2, e3, e4, e5);
        return DdgiBasis(e, select(0.0, -1.0, edge == 99u));
    }
    let gain = ddgi.params.w;
    let e = array<vec3<f32>, 6>(
        e0 / covered * gain,
        e1 / covered * gain,
        e2 / covered * gain,
        e3 / covered * gain,
        e4 / covered * gain,
        e5 / covered * gain,
    );
    return DdgiBasis(e, covered);
}

// `ddgi_irradiance_basis` with the visibility blend (ADR-2605, `ddgi_irradiance_traced_vis`). The
// finest cascade's faces live in a plain local array (FXC, ADR-1583: no store into a struct's array
// at a dynamic index).
fn ddgi_irradiance_basis_vis(p: vec3<f32>, n: vec3<f32>, v: vec3<f32>) -> DdgiBasis {
    // Six named faces instead of local arrays: a function-scope variable of an array or struct
    // type with explicit layout fails the SPIR-V layout scan (ADR-026, ADR-3002).
    var e0 = vec3<f32>(0.0);
    var e1 = vec3<f32>(0.0);
    var e2 = vec3<f32>(0.0);
    var e3 = vec3<f32>(0.0);
    var e4 = vec3<f32>(0.0);
    var e5 = vec3<f32>(0.0);
    var f0 = vec3<f32>(0.0);
    var f1 = vec3<f32>(0.0);
    var f2 = vec3<f32>(0.0);
    var f3 = vec3<f32>(0.0);
    var f4 = vec3<f32>(0.0);
    var f5 = vec3<f32>(0.0);
    var fine_cov = 0.0;
    var fade = 0.0;
    var left = 1.0;
    var covered = 0.0;
    var edge = 99u;
    var to_fine = 0.0;
    for (var c = 0u; c < ddgi.dims.w; c = c + 1u) {
        if (left <= 1e-3) {
            break;
        }
        let inset = ddgi_inset(c, p);
        if (inset >= 0.0 && edge == 99u) {
            edge = c;
            let f = ddgi_sample_cascade_basis(c, p, n, v);
            f0 = f.e[0];
            f1 = f.e[1];
            f2 = f.e[2];
            f3 = f.e[3];
            f4 = f.e[4];
            f5 = f.e[5];
            fine_cov = f.cov;
            fade = ddgi_vis_fade(c, p);
        }
        let w = ddgi_blend_weight(c, p);
        if (w <= 0.0) {
            continue;
        }
        var r0 = f0;
        var r1 = f1;
        var r2 = f2;
        var r3 = f3;
        var r4 = f4;
        var r5 = f5;
        var rcov = fine_cov;
        if (c != edge) {
            let s = ddgi_sample_cascade_basis(c, p, n, v);
            r0 = s.e[0];
            r1 = s.e[1];
            r2 = s.e[2];
            r3 = s.e[3];
            r4 = s.e[4];
            r5 = s.e[5];
            rcov = s.cov;
        }
        if (rcov <= 0.0) {
            continue;
        }
        let take = w * left;
        var keep = 1.0;
        let coarser = c > edge && edge != 99u && fine_cov > 0.0;
        if (coarser) {
            keep = mix(1.0, ddgi_vis_keep(rcov, fine_cov), fade);
            r0 = ddgi_vis_cap(r0, f0, fine_cov, fade);
            r1 = ddgi_vis_cap(r1, f1, fine_cov, fade);
            r2 = ddgi_vis_cap(r2, f2, fine_cov, fade);
            r3 = ddgi_vis_cap(r3, f3, fine_cov, fade);
            r4 = ddgi_vis_cap(r4, f4, fine_cov, fade);
            r5 = ddgi_vis_cap(r5, f5, fine_cov, fade);
        }
        let tk = take * keep;
        e0 = e0 + r0 * tk;
        e1 = e1 + r1 * tk;
        e2 = e2 + r2 * tk;
        e3 = e3 + r3 * tk;
        e4 = e4 + r4 * tk;
        e5 = e5 + r5 * tk;
        to_fine = to_fine + take * (1.0 - keep);
        covered = covered + take;
        left = left - take;
    }
    if (covered <= 1e-5) {
        let e = array<vec3<f32>, 6>(e0, e1, e2, e3, e4, e5);
        return DdgiBasis(e, select(0.0, -1.0, edge == 99u));
    }
    let gain = ddgi.params.w;
    let e = array<vec3<f32>, 6>(
        (e0 + f0 * to_fine) / covered * gain,
        (e1 + f1 * to_fine) / covered * gain,
        (e2 + f2 * to_fine) / covered * gain,
        (e3 + f3 * to_fine) / covered * gain,
        (e4 + f4 * to_fine) / covered * gain,
        (e5 + f5 * to_fine) / covered * gain,
    );
    return DdgiBasis(e, covered);
}

// The screen lookup (`ddgi_screen.wgsl` fills it after the depth prepass; half resolution unless
// `ddgi.screen.w` says otherwise): the 2×2 nearest texels, weighted bilinearly and by how close
// their surface distance is to this fragment's; the ambient cube rebuilds the irradiance for this
// fragment's own (normal-mapped) normal `n`. A surface no texel matches by distance (a thin,
// translucent or newly drawn one) takes the nearest texel's light. There is no per-pixel probe
// gather behind it (the probes run only with the lookup), which keeps the large forward and
// terrain fragment shaders' registers down. a = coverage; 0 when the lookup is off or every
// texel is sky.
fn ddgi_screen_irradiance(frag: vec2<f32>, p: vec3<f32>, n: vec3<f32>, eye: vec3<f32>) -> vec4<f32> {
    if (ddgi.screen.w == 0u) {
        return vec4<f32>(0.0);
    }
    let dist = length(p - eye);
    // Texel t holds pixel step · t. The scale is an exact constant per step, so the default half
    // resolution maps fragments exactly as before (× 0.5).
    let scale = select(select(0.25, 0.5, ddgi.screen.w == 2u), 1.0, ddgi.screen.w == 1u);
    let hc = (frag - vec2<f32>(0.5)) * scale;
    let fl = floor(hc);
    let f = hc - fl;
    let size = vec2<i32>(ddgi.screen.yz);
    // Cube slots and weights for this normal (weights sum to 1).
    let n2 = n * n;
    let sx = select(0u, 1u, n.x < 0.0);
    let sy = select(2u, 3u, n.y < 0.0);
    let sz = select(4u, 5u, n.z < 0.0);
    var sum = vec4<f32>(0.0);
    var wsum = 0.0;
    // The texel whose surface distance is nearest this fragment's.
    var best = vec4<f32>(0.0);
    var best_err = 1.0e30;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let o = vec2<i32>(i32(k & 1u), i32(k >> 1u));
        let t = clamp(vec2<i32>(fl) + o, vec2<i32>(0), size - vec2<i32>(1));
        let w_bi = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        let word = ddgi.screen.x + (u32(t.y) * ddgi.screen.y + u32(t.x)) * DDGI_SCREEN_WORDS;
        let d = bitcast<f32>(ddgi_data[word + 6u]);
        let err = abs(d - dist) / (0.02 * dist + 0.05);
        let w = (w_bi + 1e-3) * max(1.0 - err, 0.0);
        if (w <= 0.0 && !(err < best_err && d < 1.0e29)) {
            continue;
        }
        let e = ddgi_unpack_rgb9e5(ddgi_data[word + sx]) * n2.x
            + ddgi_unpack_rgb9e5(ddgi_data[word + sy]) * n2.y
            + ddgi_unpack_rgb9e5(ddgi_data[word + sz]) * n2.z;
        let texel = vec4<f32>(e / max(n2.x + n2.y + n2.z, 1e-6), bitcast<f32>(ddgi_data[word + 7u]));
        if (err < best_err && d < 1.0e29) {
            best = texel;
            best_err = err;
        }
        sum = sum + texel * w;
        wsum = wsum + w;
    }
    if (wsum <= 1e-4) {
        return best;
    }
    return sum / wsum;
}

// The capsule occluders' visibility of the cosine-weighted hemisphere at this fragment (1 = open):
// the word `baked_gi_screen.wgsl` writes after the lookup's texels and the three split words, through
// the same weights as `ddgi_screen_irradiance`. A lightmap receiver multiplies its baked diffuse term
// by it, because it does not read the ambient cube the capsules darken. 1 without capsules.
fn ddgi_screen_occluder_visibility(frag: vec2<f32>, p: vec3<f32>, eye: vec3<f32>) -> f32 {
    if ((ddgi.ctrl.w & DDGI_CTRL_OCCLUDER_VIS) == 0u) {
        return 1.0;
    }
    let dist = length(p - eye);
    let scale = select(select(0.25, 0.5, ddgi.screen.w == 2u), 1.0, ddgi.screen.w == 1u);
    let hc = (frag - vec2<f32>(0.5)) * scale;
    let fl = floor(hc);
    let f = hc - fl;
    let size = vec2<i32>(ddgi.screen.yz);
    let first = ddgi.screen.x + ddgi.screen.y * ddgi.screen.z * (DDGI_SCREEN_WORDS + 3u);
    var sum = 0.0;
    var wsum = 0.0;
    var best = 0.0;
    var best_err = 1.0e30;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let o = vec2<i32>(i32(k & 1u), i32(k >> 1u));
        let t = clamp(vec2<i32>(fl) + o, vec2<i32>(0), size - vec2<i32>(1));
        let w_bi = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        let index = u32(t.y) * ddgi.screen.y + u32(t.x);
        let d = bitcast<f32>(ddgi_data[ddgi.screen.x + index * DDGI_SCREEN_WORDS + 6u]);
        let err = abs(d - dist) / (0.02 * dist + 0.05);
        let w = (w_bi + 1e-3) * max(1.0 - err, 0.0);
        let occlusion = bitcast<f32>(ddgi_data[first + index]);
        if (err < best_err && d < 1.0e29) {
            best = occlusion;
            best_err = err;
        }
        sum = sum + occlusion * w;
        wsum = wsum + w;
    }
    if (wsum <= 1e-4) {
        return 1.0 - best;
    }
    return 1.0 - sum / wsum;
}

// ADR-5566 (`DdgiSettings::front_weights`, `ctrl.w` bit 23, every tier; `DdgiSettings::edge_visibility`, bit 24,
// the Low and Medium tiers): whether a lookup with view vector `v` is one at a surface. Both terms need a
// surface: `p` on it and `n` its normal, as the screen lookup (a depth texel) and a probe ray's hit have them. A
// lookup without a view vector is a point in free space (the radiance cache's cells,
// `radiance_cache_update.wgsl`, read the six cube directions at a cell's centre with `v` = 0): no surface, so no
// side, and it keeps the weights it had.
fn ddgi_surface_lookup(v: vec3<f32>) -> bool {
    return dot(v, v) > 0.0;
}

fn ddgi_front_lookup(v: vec3<f32>) -> bool {
    return ddgi_front_weights_on() && ddgi_surface_lookup(v);
}

// ADR-5566. A probe behind the plane of the surface the point lies on counts a thousandth: its weight fades over
// `DDGI_FRONT_RAMP` spacings behind the plane. Seen from behind that plane, the hemisphere about the normal holds
// the back of the very wall the point is on, so the probe's light for the normal is the other side's. The
// visibility test is there to drop such a probe (the wall is nearer to it than the point), but it has a blind
// spot: a probe behind a wall that sees the wall's front through an opening in it. The wrap weight's floor of 0.2
// (Majercik et al. 2019) then lets it in, and on 2 m probes the probe outside a door put the daylight on the door
// wall's inner face. The floor stays above zero, so where every visible probe stands behind the plane (the top of
// a thin post) they still share the point between them as before.
const DDGI_FRONT_RAMP: f32 = 0.15;
const DDGI_BACK_FLOOR: f32 = 0.001;

fn ddgi_front_weight(probe_pos: vec3<f32>, p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, s: f32) -> f32 {
    if (!ddgi_front_lookup(v)) {
        return 1.0;
    }
    let side = dot(probe_pos - p, n) / s;
    return max(clamp(1.0 + side / DDGI_FRONT_RAMP, 0.0, 1.0), DDGI_BACK_FLOOR);
}

// How much the point lies on a surface the probe does not hold behind itself, 0 to 1 (ADR-5566): over the cube
// faces the normal reads, how far behind the probe the point lies along the face (from `DDGI_EDGE_NEAR` spacings,
// in full `DDGI_EDGE_RAMP` further) times how open the probe's own lobe is on that side (the cover word,
// `ddgi_cover_word`). 0 for a probe under the same ceiling or before the same wall as the point, and for any probe
// the point lies beside or in front of; 1 for a probe past the edge of the point's surface with open air behind it.
const DDGI_EDGE_NEAR: f32 = 0.25;
const DDGI_EDGE_RAMP: f32 = 0.15;

fn ddgi_uncovered(index: u32, probe_pos: vec3<f32>, p: vec3<f32>, n: vec3<f32>, s: f32) -> f32 {
    let cover = ddgi_cover_word(index);
    let n2 = n * n;
    let sg = select(vec3<f32>(1.0), vec3<f32>(-1.0), n < vec3<f32>(0.0));
    let behind = (probe_pos - p) * sg / s;
    let jx = select(0u, 1u, n.x < 0.0);
    let jy = select(2u, 3u, n.y < 0.0);
    let jz = select(4u, 5u, n.z < 0.0);
    let open = vec3<f32>(1.0) - vec3<f32>(
        f32((cover >> (5u * jx)) & 31u),
        f32((cover >> (5u * jy)) & 31u),
        f32((cover >> (5u * jz)) & 31u),
    ) / 31.0;
    let far = clamp((behind - vec3<f32>(DDGI_EDGE_NEAR)) / DDGI_EDGE_RAMP, vec3<f32>(0.0), vec3<f32>(1.0));
    return dot(n2, open * far) / max(n2.x + n2.y + n2.z, 1e-6);
}

// ADR-5566: the Chebyshev bound `cheb` of a probe that may not see the point, for a probe past the edge of the
// point's surface. The bound is loose where a probe's distance lobe straddles an edge (a door's lintel: half the
// lobe ends on the wall, half runs through the opening), and the view bias moves the tested point up to 0.3
// spacings toward the eye, on 2 m probes from behind the lintel into the opening. A probe under the point's own
// ceiling keeps the bound as it is. One that stands under none (outside the door, the sunlit ground in its
// downward face) has to see the point without doubt: the bound's tail under `DDGI_EDGE_STRICT` counts as hidden.
// Light-bleeding reduction as for variance shadow maps (Lauritzen, GPU Gems 3, ch. 8), applied only to those
// probes. A probe AT the edge, with part of that side of its lobe on the surface (`ddgi_uncovered` under
// `DDGI_EDGE_OPEN`: the probe in a tunnel's mouth plane, half under the roof), is a true neighbor of the surface
// beside it and keeps the plain bound; the strictness is full `DDGI_EDGE_OPEN_RAMP` above that. With every
// uncovered probe strict, a tunnel's walls by its mouths lost light they do receive.
const DDGI_EDGE_STRICT: f32 = 0.9;
const DDGI_EDGE_OPEN: f32 = 0.6;
const DDGI_EDGE_OPEN_RAMP: f32 = 0.3;

fn ddgi_edge_visibility(
    cheb: f32,
    index: u32,
    probe_pos: vec3<f32>,
    p: vec3<f32>,
    n: vec3<f32>,
    v: vec3<f32>,
    s: f32,
) -> f32 {
    if (!(ddgi_edge_visibility_on() && ddgi_surface_lookup(v))) {
        return cheb;
    }
    let past = clamp((ddgi_uncovered(index, probe_pos, p, n, s) - DDGI_EDGE_OPEN) / DDGI_EDGE_OPEN_RAMP, 0.0, 1.0);
    let strict = DDGI_EDGE_STRICT * past;
    return clamp((cheb - strict) / (1.0 - strict), 0.0, 1.0);
}
