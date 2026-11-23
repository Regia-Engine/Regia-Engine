// SPDX-License-Identifier: MPL-2.0
// GPU irradiance probes: blend this frame's rays into the octahedral irradiance and
// distance-moment atlases with hysteresis (Majercik et al., JCGT 2019 § 4).
// Composed as ddgi_common.wgsl + this file. One workgroup per probe; every tile texel,
// border included, computes the interior texel it copies, so the octahedral border needs
// no second pass.

@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read_write> ddgi_data: array<u32>;
@group(0) @binding(2) var<storage, read_write> ddgi_rays: array<vec4<f32>>;
// Probes traced this frame (`classify` in ddgi_trace.wgsl); one workgroup each.
@group(0) @binding(3) var<storage, read_write> ddgi_queue: array<u32>;
// Emissive surfaces as probe-scale emitters (`DdgiSettings::emitters`): each a sphere of
// `pos_radius.w` around a cluster of a mesh's emitting triangles. `power` = radiance × area
// (rgb); the cluster's flux is π · power. Its intensity toward ω blends a flat one-sided
// patch, power · max(n · ω, 0), with a closed blob, power / 4, by the flatness
// `normal_flat.w` (both carry the same flux). `power.w` = the short-range emissive pass's
// range for the emitter's mesh (0: none).
struct DdgiEmitter {
    pos_radius: vec4<f32>,
    power: vec4<f32>,
    normal_flat: vec4<f32>,
}
@group(0) @binding(4) var<storage, read> ddgi_emitters: array<DdgiEmitter>;

const RAYS: u32 = 64u;
const MISS: f32 = 1.0e9;
// Share of its own last value the shading copy of a settled probe's irradiance texel keeps at each update
// (`blend_irradiance`).
const DDGI_SHADE_HOLD: f32 = 0.5;

var<workgroup> wg_rays: array<vec4<f32>, 64>;
var<workgroup> wg_dirs: array<vec3<f32>, 64>;
// The surface normal at each ray's hit (`ddgi_ray_normal`), decoded once a probe: the irradiance blend reads it for
// every surface and crease texel and for the openings (ADR-5909).
var<workgroup> wg_norms: array<vec3<f32>, 64>;
var<workgroup> wg_due: u32;

fn probe_index(wid: vec3<u32>) -> u32 {
    return ddgi_queue[wid.x];
}

// Loads the probe's rays; returns 0 when the probe was not traced this frame. Every invocation
// reaches the barrier either way, and so must the callers' later barriers: no `return` may come
// before a barrier, since FXC cannot prove a value read from workgroup memory uniform (X4026,
// ADR-1583). A probe that is not due loads nothing (an empty loop) and returns after them.
fn load_rays(probe: u32, local: u32, threads: u32) -> u32 {
    if (local == 0u) {
        var due = 0u;
        // A probe whose rays this frame started inside geometry (`relocate`: offset.w 0)
        // keeps its history: those rays saw the inside of a mesh or the terrain.
        if (probe < ddgi_count() * ddgi.dims.w && ddgi_probe_stamp(probe) == ddgi.ctrl.y
            && ddgi_probe_offset(probe).w > 0.5) {
            due = 1u;
        }
        wg_due = due;
    }
    let due = workgroupUniformLoad(&wg_due);
    let rays = select(0u, RAYS, due != 0u);
    for (var r = local; r < rays; r = r + threads) {
        let ray = ddgi_rays[probe * RAYS + r];
        wg_rays[r] = ray;
        wg_dirs[r] = ddgi_ray_dir(r, RAYS);
        wg_norms[r] = ddgi_ray_normal(ray.rgb);
    }
    workgroupBarrier();
    return due;
}

// The settled blend weight of a probe's old value. A cascade that re-traces 2^c times less often keeps the
// same time constant: hysteresis^(2^c).
fn hysteresis_cap(probe: u32) -> f32 {
    let c = probe / ddgi_count();
    return pow(ddgi.params.x, f32(1u << min(c, 3u)));
}

// The blend weight of the old value: `hysteresis_cap`, or less while the probe is young.
fn hysteresis(probe: u32) -> f32 {
    // Out of geometry this frame after being inside it (`relocate`: offset.w 2): the history
    // holds what the probe saw before it moved out, so these rays replace it.
    if (ddgi_probe_offset(probe).w > 1.5) {
        return 0.0;
    }
    let n = max(ddgi_probe_tag(probe).w, 1);
    let c = probe / ddgi_count();
    let h = pow(ddgi.params.x, f32(1u << min(c, 3u)));
    return min(h, 1.0 - 1.0 / f32(n));
}

// How much of an emitter `reach` metres away toward `w` this probe sees: the share of this
// frame's rays within ~32° of `w` that travel at least that far (a back face or a nearer hit
// hides it). The rays are the probe's own, unclamped (the distance moments stop at
// `visibility_limit` spacings, which let a wall farther than that leak an emitter behind it),
// and they rotate every frame, so the hysteresis averages the cone.
fn emitter_visibility(w: vec3<f32>, reach: f32) -> f32 {
    var seen = 0.0;
    var total = 0.0;
    var best = -2.0;
    var best_open = 0.0;
    for (var r = 0u; r < RAYS; r = r + 1u) {
        let c = dot(w, wg_dirs[r]);
        let t = wg_rays[r].w;
        let open = select(0.0, 1.0, t >= reach);
        if (c > best) {
            best = c;
            best_open = open;
        }
        if (c > 0.85) {
            let k = c - 0.85;
            seen = seen + k * open;
            total = total + k;
        }
    }
    return select(best_open, seen / total, total > 0.0);
}

// Per probe, once: each emitter's direction and its irradiance / π on a surface facing it
// (intensity toward the probe × visibility / π d²); the texels then only take the cosine.
var<workgroup> wg_em_dir: array<vec3<f32>, 256>;
var<workgroup> wg_em_val: array<vec3<f32>, 256>;

// `due` = `load_rays`'s result: 0 prepares nothing (the probe returns after the barrier).
fn prepare_emitters(probe: u32, local: u32, threads: u32, due: u32) {
    let c = probe / ddgi_count();
    let s = ddgi_cascade(c).origin.w;
    let p = vec3<f32>(ddgi_slot_cell(probe)) * s + ddgi_probe_offset(probe).xyz;
    let count = select(0u, min(ddgi.emitters.x, 256u), due != 0u);
    for (var i = local; i < count; i = i + threads) {
        let em = ddgi_emitters[i];
        let v = em.pos_radius.xyz - p;
        let d2 = max(dot(v, v), 1e-6);
        let d = sqrt(d2);
        let w = v / d;
        let r = em.pos_radius.w;
        // Intensity toward the probe (along -w from the emitter).
        let toward = mix(0.25, max(dot(em.normal_flat.xyz, -w), 0.0), em.normal_flat.w);
        var val = vec3<f32>(0.0);
        if (toward > 0.0) {
            let vis = emitter_visibility(w, max(d - r, 0.0));
            val = em.power.rgb * (toward * vis / (3.14159265 * max(d2, r * r)));
            // The far part only when a short-range pass lights the near field: the range the
            // fitted emissive lights gave this emitter's mesh (`power.w`), else
            // `DdgiSettings::emitter_local_range`. 1 − the lights' own window
            // (`lab_forward.wgsl` `emitter_radiance`), so the two add up to the emitter once.
            let range = select(bitcast<f32>(ddgi.emitters.y), em.power.w, em.power.w > 0.0);
            if (range > 0.0) {
                let q = d / range;
                let window = clamp(1.0 - q * q * q * q, 0.0, 1.0);
                val = val * (1.0 - window);
            }
        }
        wg_em_dir[i] = w;
        wg_em_val[i] = val;
    }
    workgroupBarrier();
}

// Irradiance / π toward the texel direction `n` from the prepared emitters.
fn emitter_irradiance(n: vec3<f32>) -> vec3<f32> {
    var e = vec3<f32>(0.0);
    let count = min(ddgi.emitters.x, 256u);
    for (var i = 0u; i < count; i = i + 1u) {
        e = e + wg_em_val[i] * max(dot(n, wg_em_dir[i]), 0.0);
    }
    return e;
}

// Mean and mean square of the hit distance toward `dir` (`ddgi_moments` of ddgi_sample.wgsl, which this shader
// does not include).
fn blend_moments(index: u32, dir: vec3<f32>) -> vec2<f32> {
    let e = ddgi_oct_encode(dir) * 0.5 + vec2<f32>(0.5);
    let pos = clamp(e * f32(DDGI_VIS_RES) - vec2<f32>(0.5), vec2<f32>(-0.5), vec2<f32>(f32(DDGI_VIS_RES) - 0.5));
    let fl = floor(pos);
    let t = vec2<u32>(fl + vec2<f32>(1.0));
    let f = pos - fl;
    let a = ddgi_vis_load(index, t);
    let c = ddgi_vis_load(index, t + vec2<u32>(1u, 0u));
    let d = ddgi_vis_load(index, t + vec2<u32>(0u, 1u));
    let g = ddgi_vis_load(index, t + vec2<u32>(1u, 1u));
    return mix(mix(a, c, f.x), mix(d, g, f.x), f.y);
}

// Largest gain a hit takes when it is weighed for the surface point instead of the probe (`surface_weight`).
const DDGI_SURFACE_GAIN_MAX: f32 = 8.0;
// Updates a probe takes before its surface texels lean on the surface (`surface_point`).
const DDGI_SURFACE_MIN_UPDATES: i32 = 16;

// The point on the surface behind the probe along `axis` whose irradiance the probe's rays estimate, relative
// to the probe (xyz), and how far the texel leans on it (w, 0: no surface there). The surface is the one the
// probe's distance moments hold toward -`axis`, between 0.15 and 1.05 spacings away and flat across the moments'
// lobe (a lobe half through a doorway holds no surface). Nearer than 0.15 spacings the probe stands on the
// surface and its own irradiance is the surface's. The point then moves along the surface, away from any other
// surface within `DDGI_SURFACE_CLEARANCE` spacings beside the probe (a floor the probe stands on): a point in a
// crease takes half its light from a strip of the other surface that only a few of the probe's rays reach.
fn surface_point(probe: u32, axis: vec3<f32>) -> vec4<f32> {
    let s = ddgi_cascade(probe / ddgi_count()).origin.w;
    let m = blend_moments(probe, -axis);
    let back = m.x;
    if (back >= 1.05 * s || back <= 0.15 * s) {
        return vec4<f32>(0.0);
    }
    let spread = sqrt(max(m.y - back * back, 0.0)) / back;
    // A young probe's distance moments are still filling (a lobe holds a ray every other update), so whether
    // they show a surface changes from update to update: its texel leans on the surface from its 16th update,
    // in full from its 32nd, and holds the probe's own lobe until then.
    let age = f32(ddgi_probe_tag(probe).w - DDGI_SURFACE_MIN_UPDATES) / f32(DDGI_SURFACE_MIN_UPDATES);
    let lean = clamp((back / s - 0.15) / 0.2, 0.0, 1.0) * (1.0 - clamp((spread - 0.15) / 0.15, 0.0, 1.0))
        * clamp(age, 0.0, 1.0);
    if (lean <= 0.0) {
        return vec4<f32>(0.0);
    }
    var rel = -axis * back;
    let clear = DDGI_SURFACE_CLEARANCE * s;
    // The two axes across `axis`, both ways.
    let t0 = axis.yzx;
    let t1 = axis.zxy;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let side = select(t0, t1, k >= 2u) * select(1.0, -1.0, (k & 1u) == 1u);
        let near = blend_moments(probe, side).x;
        if (near < clear) {
            rel = rel - side * (clear - near);
        }
    }
    return vec4<f32>(rel, lean);
}

// The crease point of the surface behind the probe along `axis` toward `side` (ADR-5552, `ddgi_common.wgsl`
// `ddgi_crease_texel`), relative to the probe (xyz), and how far the crease's texel leans on it (w, 0: no crease
// there). The point lies on the surface behind the probe (any flat one within reach, the one a probe stands on
// included), `DDGI_CREASE_CLEARANCE` spacings from the flat surface the distance moments hold toward `side`. A
// probe that stands nearer to that surface than the clearance is at the crease already (the lean is full from
// a quarter over the clearance, well under the 0.12 spacings a probe pushed off a surface stands at, so that
// distance's last digits do not move it), and a young probe's moments are still filling (`surface_point`).
//
// A crease belongs to the probe row nearest its surface (ADR-5716): where the probe one cell toward `side` is
// active and stands before that surface, this probe holds no crease toward it. A floor on a probe plane has a row
// standing on it, and the row a cell above it sees the same floor within reach: both held the wall's crease
// toward the floor, and a wall point between the two rows took the gradient toward the floor twice, once from
// the blend between the rows and once from the upper row's own crease (a color room's sunlit floor drew a
// bright band a metre up its walls once the creases were estimated well; a tunnel's walls darkened toward
// floor and roof).
fn crease_point(probe: u32, axis: vec3<f32>, side: vec3<f32>) -> vec4<f32> {
    let s = ddgi_cascade(probe / ddgi_count()).origin.w;
    let m = blend_moments(probe, -axis);
    let back = m.x;
    if (back >= DDGI_SURFACE_REACH * s || back <= 0.03 * s) {
        return vec4<f32>(0.0);
    }
    let mb = blend_moments(probe, side);
    let reach = mb.x;
    let near = DDGI_CREASE_CLEARANCE * s;
    if (reach >= DDGI_SURFACE_REACH * s || reach <= near) {
        return vec4<f32>(0.0);
    }
    // The neighbor toward the surface: its header says whether it traced and is active, and where it stands.
    let ncell = ddgi_slot_cell(probe) + vec3<i32>(side);
    let nindex = ddgi_slot(probe / ddgi_count(), ncell);
    let nhead = ddgi_header_word(nindex);
    if (ddgi_data[nhead] == ddgi_pack_cell(ncell, true) && (ddgi_data[nhead + 1u] >> 31u) != 0u) {
        let ahead = s + dot(ddgi_unpack_offset(ddgi_data[nhead + 1u], s) - ddgi_probe_offset(probe).xyz, side);
        if (ahead < reach - 0.03 * s) {
            return vec4<f32>(0.0);
        }
    }
    let spread_a = sqrt(max(m.y - back * back, 0.0)) / back;
    let spread_b = sqrt(max(mb.y - reach * reach, 0.0)) / reach;
    let age = f32(ddgi_probe_tag(probe).w - DDGI_SURFACE_MIN_UPDATES) / f32(DDGI_SURFACE_MIN_UPDATES);
    let lean = (1.0 - clamp((spread_a - 0.15) / 0.15, 0.0, 1.0)) * (1.0 - clamp((spread_b - 0.15) / 0.15, 0.0, 1.0))
        * clamp(age, 0.0, 1.0) * clamp((reach - near) / (0.02 * s), 0.0, 1.0);
    if (lean <= 0.0) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(-axis * back + side * (reach - near), lean);
}

// The weight of a ray's hit (direction `w_dir` from the probe, `t` metres away, surface normal `m`) in the
// irradiance at the point `rel` from the probe on a surface that faces along `axis`: the cosine there times
// the hit's solid angle from there over its solid angle from the probe (the squared distances, and how the hit's
// surface faces either point), capped at `DDGI_SURFACE_GAIN_MAX`. 0 when the hit lies behind that surface or
// faces away from the point.
fn surface_weight(w_dir: vec3<f32>, t: f32, m: vec3<f32>, rel: vec3<f32>, axis: vec3<f32>) -> f32 {
    let h = w_dir * t - rel;
    let len2 = max(dot(h, h), 1e-8);
    let hn = h * inverseSqrt(len2);
    let cosine = dot(hn, axis);
    let facing = -dot(m, hn);
    if (cosine <= 0.0 || facing <= 0.0) {
        return 0.0;
    }
    let gain = (t * t / len2) * (facing / max(-dot(m, w_dir), 0.05));
    return cosine * min(gain, DDGI_SURFACE_GAIN_MAX);
}

// ---- Openings (ADR-5909) --------------------------------------------------------------------------------------
//
// `surface_weight` moves a ray's light from the probe to a point up to 1.05 spacings away (2.1 m on the Low and
// Medium tiers) by where the ray's hit lies, and a miss by its direction alone. That holds while the point sees
// what the probe saw. It does not hold for light that reached the probe past the edge of a nearer surface: through
// a doorway or a window, out of a tunnel's mouth, over a trench's rim. What bounds that light is the opening, not
// the ground or the sky far behind it: seen from the point the opening lies at another distance and angle, and
// weighed by its far hit the ray kept the solid angle the probe saw it under. On 2 m probes a room lit through a
// door read 1.13 times the path tracer's light (its walls stand a metre from the probe planes; the same room with
// its walls on the probe planes read 0.97), and a tunnel's and a shaft's walls, which see more of the mouth than
// the probes on the centre line do, read 0.95 and 0.94.
//
// A ray passed such an edge where it crosses the plane of another ray's hit from the front, well before its own
// hit (`DDGI_OPENING_FAR` of the way at most; a miss has no hit), and that other ray lies within
// `DDGI_OPENING_CONE` of it: the surface in that plane ends between the two. The ray's light is then weighed from
// where it crosses the nearest such plane, as a patch there (`opening`): the opening as an area source, which
// carries its own solid angle to the point (a portal, as Bitterli, Novak and Jarosz 2015 place one in a window to
// sample the environment through it; found here from the probe's own hits). Where the ray meets the plane steeply,
// the patch lies in the plane: a hole in a wall before the probe. Where it grazes the plane (a tunnel's wall, past
// its mouth), the edge it passed lies somewhere before the crossing and the patch faces the probe, which keeps
// its solid angle and moves only its direction.

// The cosine of the widest angle between a ray and the ray whose hit bounds it (35 degrees: the 64 rays stand
// about 25 degrees apart, so a ray has six to eight others that near).
const DDGI_OPENING_CONE: f32 = 0.82;
// A ray's own hit lies at least 1 / this times as far as the plane it crosses: a hit just behind the plane (the top
// of a box past its side) is weighed where it lies.
const DDGI_OPENING_FAR: f32 = 0.75;
// The cosine at the crossed plane under which the patch faces the probe, and over which it lies in the plane.
const DDGI_OPENING_GRAZE: f32 = 0.25;
const DDGI_OPENING_STEEP: f32 = 0.6;

// Where each of the probe's rays passes an opening (`opening`): one ray an invocation of `blend_irradiance`.
var<workgroup> wg_open: array<vec4<f32>, 64>;

// The patch ray `i` of the probe is weighed from for a surface or crease point: xyz = its normal, w = how far along
// the ray it lies (0: the ray passes no edge, and is weighed by its own hit or direction). `s` = the probe spacing.
fn opening(i: u32, s: f32) -> vec4<f32> {
    let ti = wg_rays[i].w;
    let miss = ti >= 1.0e8;
    let di = wg_dirs[i];
    var normal = vec3<f32>(0.0);
    var depth = 1.0e30;
    // A back face (the probe is inside geometry) holds no light: its loop is empty. One exit, and no `return`
    // before the caller's barrier (ADR-1583).
    let count = select(RAYS, 0u, ti < 0.0);
    for (var j = 0u; j < count; j = j + 1u) {
        let tj = wg_rays[j].w;
        if (j == i || tj <= 0.0 || tj >= 1.0e8 || dot(wg_dirs[j], di) < DDGI_OPENING_CONE) {
            continue;
        }
        let mj = wg_norms[j];
        // The ray enters the plane of hit j from its front.
        let dn = dot(di, mj);
        if (dn >= -0.05) {
            continue;
        }
        let u = dot(wg_dirs[j] * tj, mj) / dn;
        if (u <= 0.05 * s || (!miss && u >= DDGI_OPENING_FAR * ti)) {
            continue;
        }
        if (u < depth) {
            depth = u;
            normal = mj;
        }
    }
    // Without a crossing `normal` is zero and the patch below is the probe-facing one, which is not used.
    let steep = clamp(
        (-dot(di, normal) - DDGI_OPENING_GRAZE) / (DDGI_OPENING_STEEP - DDGI_OPENING_GRAZE),
        0.0,
        1.0,
    );
    let patch_normal = normalize(mix(-di, normal, steep));
    return select(vec4<f32>(patch_normal, depth), vec4<f32>(0.0), depth > 1.0e29);
}

// Fills `wg_open`, one ray an invocation (`local`); `due` = `load_rays`'s result (a probe that did not trace has no
// rays, and every invocation reaches the barrier either way: ADR-1583).
fn find_openings(probe: u32, local: u32, due: u32) {
    var gap = vec4<f32>(0.0);
    if (due != 0u) {
        gap = opening(local, ddgi_cascade(probe / ddgi_count()).origin.w);
    }
    wg_open[local] = gap;
    workgroupBarrier();
}

// `hysteresis` for the irradiance tile, with a seed (ADR-2605, `ddgi_trace.wgsl` `seed_probe`): a
// probe that started over with a seed worth `seed` updates averages its first updates with it, and a
// probe out of geometry again blends this frame's rays with the seed written this frame instead of
// replacing its history. Without a seed (word 9 is 0) this is `hysteresis` exactly.
fn hysteresis_irr(probe: u32) -> f32 {
    let seed = f32(ddgi_data[probe * DDGI_PROBE_WORDS + 9u]);
    // `DdgiSettings::recent_weight` (`ctrl.w` bits 21..22): the k-th update weighs k^(w − 1), a running
    // average of `1 − w / n` (ADR-2605). 1 is the equal-weight average, unchanged.
    let w = f32(((ddgi.ctrl.w >> 21u) & 3u) + 1u);
    if (ddgi_probe_offset(probe).w > 1.5) {
        let fresh = ddgi_data[probe * DDGI_PROBE_WORDS + 10u] == ddgi.ctrl.y;
        // The first update of the average `relocate` restarted with the seed.
        return select(0.0, max(1.0 - w / (1.0 + seed), 0.0), fresh && seed > 0.0);
    }
    let n = max(ddgi_probe_tag(probe).w, 1);
    let c = probe / ddgi_count();
    let h = pow(ddgi.params.x, f32(1u << min(c, 3u)));
    return min(h, max(1.0 - w / (f32(n) + seed), 0.0));
}

@compute @workgroup_size(8, 8)
fn blend_irradiance(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(local_invocation_index) local: u32,
) {
    let probe = probe_index(wid);
    let due = load_rays(probe, local, 64u);
    // Uniform branch: the emitter count is a uniform. Its barrier comes before the return below
    // (ADR-1583).
    if (ddgi.emitters.x > 0u) {
        prepare_emitters(probe, local, 64u, due);
    }
    // The openings the rays passed (ADR-5909); its barrier too comes before the return.
    find_openings(probe, local, due);
    if (due == 0u) {
        return;
    }
    let res = i32(DDGI_IRR_RES);
    let src = ddgi_border_source(vec2<i32>(lid.xy), res);
    let e = (vec2<f32>(src) + vec2<f32>(0.5)) / f32(res) * 2.0 - vec2<f32>(1.0);
    // The texel's lobe direction: its own, or on a tier the axis next to it (`ddgi_irr_texel_dir`, ADR-5374).
    var dir = ddgi_irr_texel_dir(e);
    // A border texel of the crease set holds the irradiance at a crease point of its face (`crease_point`,
    // ADR-5552): the face's own light where the probe has no crease there.
    let ci = ddgi_crease_index(lid.xy);
    let is_crease = ci < 24u;
    var side = vec3<f32>(0.0);
    if (is_crease) {
        dir = ddgi_face_dir(ci / 4u);
        side = ddgi_crease_side(dir, ci % 4u);
    }
    // On a tier, an axis texel of odd parity holds the irradiance at the surface behind the probe along its axis
    // (`surface_point`, ADR-5457): this frame's rays as that surface point sees them.
    var aim = vec4<f32>(0.0);
    var crease = vec4<f32>(0.0);
    if (ddgi_tier_lookup() && (is_crease || (ddgi_irr_texel_on_axis(e) && ((src.x + src.y) & 1) == 1)) && ddgi_probe_tag(probe).w > 1) {
        aim = surface_point(probe, dir);
        if (is_crease) {
            crease = crease_point(probe, dir, side);
        }
    }
    let spacing = ddgi_cascade(probe / ddgi_count()).origin.w;
    var sum = vec3<f32>(0.0);
    var wsum = 0.0;
    var surface = vec3<f32>(0.0);
    var creased = vec3<f32>(0.0);
    var cwsum = 0.0;
    // The crease point's rays that end on the surface beside the crease, apart from the rest (`across`).
    var creased_beside = vec3<f32>(0.0);
    var cwsum_beside = 0.0;
    // Whether the crease is toward a surface the probe does not stand beside (`DDGI_CREASE_BESIDE`): its rays
    // are then weighed in two strata (below).
    var across = false;
    if (crease.w > 0.0) {
        across = blend_moments(probe, side).x > DDGI_CREASE_BESIDE * spacing;
    }
    for (var r = 0u; r < RAYS; r = r + 1u) {
        let ray = wg_rays[r];
        if (ray.w < 0.0) {
            continue;
        }
        let w = max(dot(dir, wg_dirs[r]), 0.0);
        sum = sum + ray.rgb * w;
        wsum = wsum + w;
        // Light that came past the edge of a nearer surface is weighed from that opening (`opening`, ADR-5909);
        // only a texel that holds a surface or crease point reads it.
        var gap = vec4<f32>(0.0);
        if (aim.w > 0.0 || crease.w > 0.0) {
            gap = wg_open[r];
        }
        if (aim.w > 0.0) {
            // A miss that passed no edge is the sky, as far from the surface point as from the probe.
            var ws = w;
            if (gap.w > 0.0) {
                ws = surface_weight(wg_dirs[r], gap.w, gap.xyz, aim.xyz, dir);
            } else if (ray.w < 1.0e8) {
                ws = surface_weight(wg_dirs[r], ray.w, wg_norms[r], aim.xyz, dir);
            }
            surface = surface + ray.rgb * ws;
        }
        if (crease.w > 0.0) {
            // The rays as the crease point sees them. A hit past the plane of the surface beside the crease is
            // hidden from the point, and so is the sky on that side. A hit on that surface itself (within the
            // plane's tolerance, facing the point) is kept apart for a crease seen from across the cell.
            var wc = 0.0;
            var beside = false;
            if (gap.w > 0.0) {
                // The opening's patch, where it does not lie past the surface beside the crease.
                let h = wg_dirs[r] * gap.w - crease.xyz;
                if (dot(h, side) - DDGI_CREASE_CLEARANCE * spacing <= 0.03 * spacing) {
                    wc = surface_weight(wg_dirs[r], gap.w, gap.xyz, crease.xyz, dir);
                }
            } else if (ray.w < 1.0e8) {
                let h = wg_dirs[r] * ray.w - crease.xyz;
                let past = dot(h, side) - DDGI_CREASE_CLEARANCE * spacing;
                if (past <= 0.03 * spacing) {
                    let m = wg_norms[r];
                    wc = surface_weight(wg_dirs[r], ray.w, m, crease.xyz, dir);
                    beside = across && past >= -0.03 * spacing && dot(m, side) < -0.7;
                }
            } else if (dot(wg_dirs[r], side) <= 0.05) {
                wc = w;
            }
            if (beside) {
                creased_beside = creased_beside + ray.rgb * wc;
                cwsum_beside = cwsum_beside + wc;
            } else {
                creased = creased + ray.rgb * wc;
                cwsum = cwsum + wc;
            }
        }
    }
    let word = ddgi_irr_word(probe, lid.xy);
    // The history is raw f32 bits blended with hysteresis: one NaN or ∞ stored here would stay
    // for good (mix(x, NaN, h) is NaN) and light every surface near the probe with it. A
    // non-finite history restarts from black, and a non-finite frame keeps the history.
    let old = ddgi_finite_or3(ddgi_irr_load(probe, lid.xy), vec3<f32>(0.0));
    // No ray reached this texel's hemisphere (all back faces): nothing new to blend.
    var fresh = select(old, sum / max(wsum, 1e-4), wsum > 0.0);
    if (aim.w > 0.0 && wsum > 0.0) {
        fresh = mix(fresh, surface / max(wsum, 1e-4), aim.w);
    }
    // The crease point's light is the weighted mean of what its rays hold: the strip beside the point is a few
    // rays wide and their gain is capped, so the weights do not sum to the hemisphere, and dividing by the
    // probe's own sum darkened every crease (a door room read 0.93 of its light). Too few rays: the face's light.
    //
    // A crease seen from across the cell (ADR-5716): the surface beside it fills half of the point's
    // cosine-weighted hemisphere whatever the clearance, but only a few of the probe's rays end on the strip of
    // it that the point sees, and their gain is capped, so in one mean that half counted for a fraction of its
    // share: beside a sunlit wall the crease read the far field and stayed dark (a shaft's corner band). The two
    // halves are therefore averaged apart, each over its own weights, and count half each. A crease toward a
    // surface the probe stands beside keeps the one mean: its strip lies under the probe's own rays, and the
    // point itself may lie a cell away (a wall's foot seen from the floor's probe), where the strip's rays are
    // too few to stand for half of anything (a tunnel lost 0.003 of mean FLIP to it, a color room 0.001).
    if (crease.w > 0.0 && wsum > 0.0) {
        if (cwsum_beside > 0.0 && cwsum > 0.0) {
            fresh = mix(fresh, 0.5 * creased_beside / cwsum_beside + 0.5 * creased / cwsum, crease.w);
        } else if (cwsum + cwsum_beside > 0.2 * wsum) {
            fresh = mix(fresh, (creased + creased_beside) / (cwsum + cwsum_beside), crease.w);
        }
    }
    // Emitters (irradiance / π, like the rays' mean): E = I · cos / d², visible where this
    // frame's rays toward the emitter reach it (`emitter_visibility`). No extra rays, no bricks.
    if (ddgi.emitters.x > 0u) {
        fresh = fresh + emitter_irradiance(dir);
    }
    fresh = ddgi_finite_or3(fresh, old);
    // Each tile texel keeps its own history (a border texel holds the same value as the
    // interior texel it copies, so both blend to the same result).
    let h = hysteresis_irr(probe);
    var value = mix(fresh, old, h);
    // A large change (a light switched, a door opened) converges faster.
    if (h > 0.0 && any(abs(value - old) > vec3<f32>(0.25) * max(max(old.r, max(old.g, old.b)), 1e-3))) {
        value = mix(fresh, old, h * 0.75);
    }
    let bits = bitcast<vec3<u32>>(value);
    ddgi_data[word] = bits.x;
    ddgi_data[word + 1u] = bits.y;
    ddgi_data[word + 2u] = bits.z;
    // The shading copy: one shared-exponent word per texel. Once the probe's history runs at its settled rate
    // (`hysteresis_cap`), the copy follows it through a second, short filter (`DDGI_SHADE_HOLD` of its own last
    // value): a history step of (1 - hysteresis) times this frame's error then reaches the frame spread over
    // the next updates, so the light a held camera sees changes less from frame to frame for the same history
    // and one update of lag (ADR-5457). A younger probe's history is the plain mean of its updates, and its copy
    // is that mean: holding the copy there kept a probe's first, least converged updates in the light the other
    // probes' rays read.
    let shade_word = ddgi.words.w + probe * DDGI_PACKED_IRR_WORDS + lid.y * DDGI_IRR_TILE + lid.x;
    let hold = select(0.0, DDGI_SHADE_HOLD, h >= hysteresis_cap(probe) - 1e-6);
    let shown = ddgi_unpack_rgb9e5(ddgi_data[shade_word]);
    if (ddgi_tier_lookup() && lid.x == 0u && lid.y == 0u) {
        // The corner texel's shading copy: where the surfaces behind the probe lie (`ddgi_surface_word`).
        var backs = 0u;
        if (ddgi_probe_tag(probe).w > 1) {
            let sp = ddgi_cascade(probe / ddgi_count()).origin.w;
            for (var j = 0u; j < 6u; j = j + 1u) {
                let sg = select(1.0, -1.0, (j & 1u) == 1u);
                let axis = vec3<f32>(select(0.0, sg, (j >> 1u) == 0u), select(0.0, sg, (j >> 1u) == 1u), select(0.0, sg, (j >> 1u) == 2u));
                // Every flat surface within reach, the ones the probe stands on included (the creases read them).
                let mj = blend_moments(probe, -axis);
                if (mj.x < DDGI_SURFACE_REACH * sp && mj.x > 0.03 * sp
                    && sqrt(max(mj.y - mj.x * mj.x, 0.0)) / mj.x < 0.3) {
                    backs = backs | (max(ddgi_pack_surface_back(mj.x, sp), 1u) << (5u * j));
                }
            }
        }
        ddgi_data[shade_word] = backs;
    } else {
        ddgi_data[shade_word] = ddgi_pack_rgb9e5(mix(value, shown, hold));
    }
    if (ddgi_edge_visibility_on() && lid.x == 0u && lid.y == 0u) {
        // The cover word (ADR-5566, `ddgi_cover_word`): per cube direction, how much of the probe's distance
        // lobe toward the surface behind that face ends within `DDGI_SURFACE_REACH` spacings, from the mean
        // distance the moments hold there (the limit they clamp at counts as open). The moments read here are
        // the last update's: a probe on its first update, or out of geometry again, counts as covered all
        // round until it has its own.
        var cover = 0x3fffffffu;
        if (ddgi_probe_tag(probe).w > 1) {
            let sp = ddgi_cascade(probe / ddgi_count()).origin.w;
            let limit = sp * ddgi.sky.w;
            let reach = DDGI_SURFACE_REACH * sp;
            cover = 0u;
            for (var j = 0u; j < 6u; j = j + 1u) {
                let sg = select(1.0, -1.0, (j & 1u) == 1u);
                let axis = vec3<f32>(select(0.0, sg, (j >> 1u) == 0u), select(0.0, sg, (j >> 1u) == 1u), select(0.0, sg, (j >> 1u) == 2u));
                let mean = blend_moments(probe, -axis).x;
                let covered = clamp((limit - mean) / max(limit - reach, 1e-3), 0.0, 1.0);
                cover = cover | (u32(round(covered * 31.0)) << (5u * j));
            }
        }
        ddgi_data[probe * DDGI_PROBE_WORDS + DDGI_COVER_WORD] = cover;
    }
}

@compute @workgroup_size(16, 16)
fn blend_visibility(
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(local_invocation_index) local: u32,
) {
    let probe = probe_index(wid);
    if (load_rays(probe, local, 256u) == 0u) {
        return;
    }
    let res = i32(DDGI_VIS_RES);
    let src = ddgi_border_source(vec2<i32>(lid.xy), res);
    let e = (vec2<f32>(src) + vec2<f32>(0.5)) / f32(res) * 2.0 - vec2<f32>(1.0);
    let dir = ddgi_oct_decode(e);
    let c = probe / ddgi_count();
    let limit = ddgi_cascade(c).origin.w * ddgi.sky.w;
    var sum = vec2<f32>(0.0);
    var wsum = 0.0;
    for (var r = 0u; r < RAYS; r = r + 1u) {
        let cosine = max(dot(dir, wg_dirs[r]), 0.0);
        if (cosine <= 0.0) {
            continue;
        }
        // Sharp lobe (exponent 50): distances are directional.
        let w = pow(cosine, 50.0);
        var dist = wg_rays[r].w;
        // A back face counts as very close so a probe inside a wall cannot light past it.
        if (dist < 0.0) {
            dist = -dist * 0.2;
        }
        dist = min(dist, limit);
        sum = sum + vec2<f32>(dist, dist * dist) * w;
        wsum = wsum + w;
    }
    let word = ddgi_vis_word(probe, lid.xy);
    let old = ddgi_vis_load(probe, lid.xy);
    // A non-finite ray distance keeps the history (unorm16 packing of NaN is undefined).
    let fresh = ddgi_finite_or2(sum / max(wsum, 1e-6), old);
    let h = hysteresis(probe);
    let value = mix(fresh, old, h);
    ddgi_data[word] = ddgi_pack_unorm16x2(vec2<f32>(value.x / limit, value.y / (limit * limit)));
}
