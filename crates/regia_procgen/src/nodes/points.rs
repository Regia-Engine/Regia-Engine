// SPDX-License-Identifier: MPL-2.0
//! Point operations (`points.*`, spec 63 § 6).

use regia_core::math::Vec3;
use regia_core::math::hash::mix;

use crate::rot::Rot as Quat;

use crate::data::{AttrColumn, Bundle, DataItem, DataKind, Payload, PointSet, quat};
use crate::exec::{CHUNK, FiniteCheck, NodeCtx};
use crate::nodes::common::{axis_angle, read_num};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, kind, num, pin_multi, pin_out,
    pin_req, pins_points_inout, set,
};

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::in_place(
        "points.jitter",
        "points",
        "Random offset (slots 0-2), rotation (3-5) and scale (6-8, or 6 for `one`) per point",
        &[
            set(
                "offset",
                T::Vec3Range,
                "[(0.0, 0.0, 0.0), (0.0, 0.0, 0.0)]",
                "Offset range [lo, hi]",
            ),
            set(
                "rotate_deg",
                T::Vec3Range,
                "[(0.0, 0.0, 0.0), (0.0, 0.0, 0.0)]",
                "Euler range (pitch X, yaw Y, roll Z)",
            ),
            set(
                "scale",
                T::Vec3Range,
                "[(1.0, 1.0, 1.0), (1.0, 1.0, 1.0)]",
                "Scale range",
            ),
            set(
                "scale_axes",
                T::Enum(&["one", "each"]),
                "one",
                "One draw for all axes (X range) or one per axis",
            ),
            set(
                "rotate_mode",
                T::Enum(&["add", "replace"]),
                "add",
                "Compose with or replace the rotation",
            ),
            set(
                "scale_mode",
                T::Enum(&["mul", "replace"]),
                "mul",
                "Multiply or replace the scale",
            ),
            set("space", T::Enum(&["local", "world"]), "local", "Offset frame"),
        ],
        pins_points_inout,
        eval_jitter,
    ),
    BuiltinNode::in_place(
        "points.reseed",
        "points",
        "Recomputes seeds: position hash, index or mix with the node seed",
        &[set("mode", T::Enum(&["position", "index", "mix"]), "mix", "Rule")],
        pins_points_inout,
        eval_reseed,
    ),
    BuiltinNode::in_place(
        "points.keep_random",
        "points",
        "Keeps each point when u < ratio (slot 0)",
        &[num("ratio", T::Float, "0.5", 0.0, 1.0, "Keep probability")],
        pins_points_inout,
        eval_keep_random,
    ),
    BuiltinNode::new(
        "points.prune",
        "points",
        "Removes overlaps: visits points by size and keeps those whose padded box overlaps no kept box",
        &[
            set(
                "order",
                T::Enum(&["large_first", "small_first", "random", "as_is"]),
                "large_first",
                "Visit order",
            ),
            set(
                "size",
                T::Enum(&["box", "density", "attr"]),
                "box",
                "Size measure",
            ),
            set("attr", T::Selector, "\"\"", "Attribute for `size: attr`"),
            num(
                "equal_within",
                T::Float,
                "0.0",
                0.0,
                1.0,
                "Relative size tolerance treated as equal",
            ),
            num("padding_m", T::Float, "0.0", 0.0, 1.0e6, "Box padding"),
            num(
                "max_footprint_m",
                T::Float,
                "8.0",
                0.0,
                1.0e6,
                "Largest box half-diagonal plus padding expected; bounds the declared reach (spec 66)",
            ),
        ],
        pins_points_inout,
        eval_prune,
    )
    .reaching(|s| 2.0 * s.f("max_footprint_m")),
    BuiltinNode::new(
        "points.merge",
        "points",
        "Concatenates all items into one (missing attributes filled with defaults)",
        &[],
        |_| PinSpec::new(vec![pin_multi("in", POINTS)], vec![pin_out("out", POINTS)]),
        eval_merge,
    ),
    BuiltinNode::in_place(
        "points.bake_scale",
        "points",
        "bounds *= scale, scale = sign(scale)",
        &[],
        pins_points_inout,
        eval_bake_scale,
    ),
    BuiltinNode::in_place(
        "points.bounds",
        "points",
        "Sets or scales the local box",
        &[
            set("mode", T::Enum(&["set", "scale"]), "set", "Rule"),
            set("min", T::Vec3, "(-0.5, -0.5, -0.5)", "Box minimum for `set`"),
            set("max", T::Vec3, "(0.5, 0.5, 0.5)", "Box maximum for `set`"),
            set("factor", T::Vec3, "(1.0, 1.0, 1.0)", "Factor for `scale`"),
        ],
        pins_points_inout,
        eval_bounds,
    ),
    BuiltinNode::in_place(
        "points.flat_only",
        "points",
        "Keeps points whose ground and 4 probes lie within max_dev_m of their least-squares plane",
        &[
            num("probe_m", T::Float, "1.0", 0.01, 1.0e4, "Probe distance"),
            num("max_dev_m", T::Float, "0.15", 0.0, 1.0e4, "Allowed deviation"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_req("ground", kind(DataKind::Ground))],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_flat_only,
    )
    .world(),
    BuiltinNode::new(
        "points.stamp",
        "points",
        "Copies the source set onto every target point (target-major order)",
        &[
            set(
                "transform",
                T::Enum(&["compose", "target", "source"]),
                "compose",
                "Placement",
            ),
            set(
                "color",
                T::Enum(&["multiply", "source", "target"]),
                "multiply",
                "Tint",
            ),
            set("seed", T::Enum(&["mix", "source", "target"]), "mix", "Seed rule"),
            set(
                "pairing",
                T::Enum(&["by_index", "all_pairs"]),
                "by_index",
                "Item pairing",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("source", POINTS), pin_req("targets", POINTS)],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_stamp,
    ),
];

fn lerp3(lo: [f64; 3], hi: [f64; 3], u: [f64; 3]) -> [f64; 3] {
    [
        lo[0] + (hi[0] - lo[0]) * u[0],
        lo[1] + (hi[1] - lo[1]) * u[1],
        lo[2] + (hi[2] - lo[2]) * u[2],
    ]
}

fn for_points(
    inputs: &mut [Bundle],
    mut f: impl FnMut(&mut PointSet) -> Result<(), NodeError>,
) -> Result<Vec<Bundle>, NodeError> {
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            f(p)?;
        }
    }
    Ok(vec![out])
}

fn eval_jitter(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = crate::nodes::take_input(inputs, 0);
    let [olo, ohi] = s.v3_range("offset");
    let [rlo, rhi] = s.v3_range("rotate_deg");
    let [slo, shi] = s.v3_range("scale");
    let one_scale = s.text("scale_axes") == "one";
    let rot_add = s.text("rotate_mode") == "add";
    let scale_mul = s.text("scale_mode") == "mul";
    let local = s.text("space") == "local";
    let rotates = rlo != [0.0; 3] || rhi != [0.0; 3] || !rot_add;
    let node_seed = ctx.node_seed;
    // An axis whose range is empty (`hi - lo` is ±0) gives `lo + (hi - lo) · u` with the same bits
    // for every draw `u` in [0, 1), so its draw is skipped (`u = 0`) and a constant rotation axis
    // keeps one quaternion: the result is bit-identical to drawing (spec 63 § 15 budgets).
    let fixed = |lo: [f64; 3], hi: [f64; 3]| [0usize, 1, 2].map(|k| hi[k] - lo[k] == 0.0);
    let (off_fixed, rot_fixed, scale_fixed) = (fixed(olo, ohi), fixed(rlo, rhi), fixed(slo, shi));
    let to_rad = std::f64::consts::PI / 180.0;
    let axes = [Vec3::X, Vec3::Y, Vec3::Z];
    let rot_const: [Option<Quat>; 3] = [0usize, 1, 2]
        .map(|k| rot_fixed[k].then(|| axis_angle(axes[k], lerp3(rlo, rhi, [0.0; 3])[k] * to_rad)));
    ctx.finite_check = FiniteCheck::Transform;
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        // Rows past the shortest of the four columns keep their values (columns of one set normally
        // have equal lengths).
        let jobs: Vec<_> = p
            .position
            .chunks_mut(CHUNK)
            .zip(p.rotation.chunks_mut(CHUNK))
            .zip(p.scale.chunks_mut(CHUNK))
            .zip(p.seed.chunks(CHUNK))
            .collect();
        ctx.for_each_job(jobs, |(((position, rotation), scale), seeds)| {
            let rows = position.iter_mut().zip(rotation).zip(scale).zip(seeds);
            for (((pos, rotation), cur), seed) in rows {
                let d = |slot: u32| crate::seeds::draw(*seed, node_seed, slot);
                let u = |fixed: [bool; 3], base: u32| {
                    [0usize, 1, 2].map(|k| if fixed[k] { 0.0 } else { d(base + k as u32) })
                };
                let off = lerp3(olo, ohi, u(off_fixed, 0));
                let r = quat(*rotation);
                let off = Vec3::from(off.map(|v| v as f32));
                let off = if local { r * off } else { off };
                *pos = (Vec3::from(*pos) + off).to_array();
                if rotates {
                    // `euler_deg`: yaw (Y) · pitch (X) · roll (Z), constant axes reused.
                    let deg = lerp3(rlo, rhi, u(rot_fixed, 3));
                    let q = |k: usize| rot_const[k].unwrap_or_else(|| axis_angle(axes[k], deg[k] * to_rad));
                    let delta = q(1) * q(0) * q(2);
                    let q = if rot_add { r * delta } else { delta };
                    *rotation = q.normalize().to_array();
                }
                let sc = if one_scale {
                    let u = if scale_fixed[0] { 0.0 } else { d(6) };
                    let x = slo[0] + (shi[0] - slo[0]) * u;
                    [x, x, x]
                } else {
                    lerp3(slo, shi, u(scale_fixed, 6))
                };
                *cur = if scale_mul {
                    [
                        cur[0] * sc[0] as f32,
                        cur[1] * sc[1] as f32,
                        cur[2] * sc[2] as f32,
                    ]
                } else {
                    sc.map(|v| v as f32)
                };
            }
        })?;
    }
    Ok(vec![out])
}

fn eval_reseed(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mode = s.text("mode").to_string();
    let (ns, ts) = (ctx.node_seed, ctx.tier_seed);
    for_points(inputs, |p| {
        let seeds = p.seed.make_mut();
        for (i, seed) in seeds.iter_mut().enumerate() {
            *seed = match mode.as_str() {
                "position" => crate::seeds::point_seed(p.position[i], ts),
                "index" => mix(ns, i as u32),
                _ => mix(*seed, ns),
            };
        }
        Ok(())
    })
}

fn eval_keep_random(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let ratio = s.f("ratio");
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &item.payload {
            let keep: Vec<bool> = p.seed.iter().map(|sd| ctx.draw(*sd, 0) < ratio).collect();
            item.payload = Payload::Points(p.partition(&keep).0);
        }
    }
    Ok(vec![out])
}

fn boxes_overlap(a: &([f32; 3], [f32; 3]), b: &([f32; 3], [f32; 3])) -> bool {
    (0..3).all(|k| a.0[k] < b.1[k] && b.0[k] < a.1[k])
}

/// The broad-phase grid of `points.prune`: per cell, the kept rows that overlap it, met in the order
/// they were kept (the first hit decides `seam_mismatch_risk`). One flat entry pool with a
/// `(first, last)` per cell, so a million-point set costs no allocation per cell.
struct KeptGrid {
    /// `(first entry, last entry)` per cell, `NO_ENTRY` when empty.
    cells: Vec<(u32, u32)>,
    /// `(row, next entry of the same cell)`.
    entries: Vec<(u32, u32)>,
}

const NO_ENTRY: u32 = u32::MAX;

impl KeptGrid {
    fn new(cells: usize) -> Self {
        Self {
            cells: vec![(NO_ENTRY, NO_ENTRY); cells],
            entries: Vec::new(),
        }
    }

    /// Appends `row` to `cell`; `Err` with the entry count when the pool's `u32` index runs out.
    fn push(&mut self, cell: usize, row: u32) -> Result<(), usize> {
        let at = self.entries.len();
        let Some(index) = u32::try_from(at).ok().filter(|i| *i != NO_ENTRY) else {
            return Err(at);
        };
        self.entries.push((row, NO_ENTRY));
        let slot = &mut self.cells[cell];
        if slot.0 == NO_ENTRY {
            slot.0 = index;
        } else {
            self.entries[slot.1 as usize].1 = index;
        }
        slot.1 = index;
        Ok(())
    }

    /// The first row of `cell`, in keeping order, that `hits`.
    #[inline]
    fn first_in(&self, cell: usize, hits: impl Fn(usize) -> bool) -> Option<usize> {
        let mut at = self.cells[cell].0;
        while at != NO_ENTRY {
            let (row, next) = self.entries[at as usize];
            if hits(row as usize) {
                return Some(row as usize);
            }
            at = next;
        }
        None
    }
}

fn eval_prune(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let order = s.text("order").to_string();
    let size_mode = s.text("size").to_string();
    let eq = s.f("equal_within");
    let pad = s.f("padding_m") as f32;
    let sel = s.sel("attr");
    // Rows are only dropped, and every input was checked when its node produced it.
    ctx.finite_check = FiniteCheck::Nothing;
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let attr = if size_mode == "attr" {
            Some(read_num(item, &sel)?)
        } else {
            None
        };
        let Payload::Points(p) = &item.payload else {
            out.0.push(item.clone());
            continue;
        };
        let n = p.len();
        let boxes: Vec<([f32; 3], [f32; 3])> = (0..n)
            .map(|i| {
                let (lo, hi) = p.world_aabb(i);
                (lo.map(|v| v - pad), hi.map(|v| v + pad))
            })
            .collect();
        let size: Vec<f64> = (0..n)
            .map(|i| match (&attr, size_mode.as_str()) {
                (Some(a), _) => a.scalar(i),
                (None, "density") => f64::from(p.density[i]),
                _ => {
                    let (lo, hi) = (boxes[i].0, boxes[i].1);
                    f64::from((hi[0] - lo[0]) * (hi[1] - lo[1]) * (hi[2] - lo[2]))
                }
            })
            .collect();
        // The visit order reads draws only for `random` and for tied sizes under a tolerance.
        let draw: Vec<f64> = if order == "random" || eq > 0.0 {
            p.seed.iter().map(|sd| ctx.draw(*sd, 0)).collect()
        } else {
            vec![0.0; n]
        };
        let mut idx: Vec<usize> = (0..n).collect();
        // Sizes within `equal_within` (relative) compare equal and are then ordered by draw.
        let log_step = crate::detmath::log2(1.0 + eq);
        let quant = |v: f64| -> f64 {
            if eq <= 0.0 || v <= 0.0 {
                v
            } else {
                (crate::detmath::log2(v) / log_step).floor()
            }
        };
        match order.as_str() {
            "large_first" | "small_first" => {
                let large = order == "large_first";
                // The sort keys per row, computed once and held by value next to the row index. The
                // index breaks every tie, so the order is total and an unstable sort gives the one
                // order there is.
                let mut keyed: Vec<(f64, f64, u32, usize)> =
                    (0..n).map(|i| (quant(size[i]), draw[i], p.seed[i], i)).collect();
                keyed.sort_unstable_by(|a, b| {
                    let c = if large {
                        b.0.total_cmp(&a.0)
                    } else {
                        a.0.total_cmp(&b.0)
                    };
                    let tie = if eq > 0.0 {
                        a.1.total_cmp(&b.1)
                    } else {
                        std::cmp::Ordering::Equal
                    };
                    c.then(tie).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3))
                });
                idx = keyed.into_iter().map(|k| k.3).collect();
            }
            "random" => {
                let mut keyed: Vec<(f64, usize)> = (0..n).map(|i| (draw[i], i)).collect();
                keyed.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                idx = keyed.into_iter().map(|k| k.1).collect();
            }
            _ => {}
        }
        // Uniform XZ grid over kept boxes: a kept box is listed in every cell it overlaps, a query
        // reads the cells its own box overlaps. Cells are about twice the mean box size.
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        let mut size_sum = 0.0f64;
        for b in &boxes {
            lo = [lo[0].min(b.0[0]), lo[1].min(b.0[2])];
            hi = [hi[0].max(b.1[0]), hi[1].max(b.1[2])];
            size_sum += f64::from((b.1[0] - b.0[0]).max(b.1[2] - b.0[2]));
        }
        let mean = if n > 0 { (size_sum / n as f64) as f32 } else { 1.0 };
        let extent = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1e-3);
        // At most 2048 cells per side, and about four cells per point in all, so two far-apart
        // points do not allocate a 2049 x 2049 grid (audit M13). The cell size only changes which
        // boxes are compared, never the result.
        let side = ((4.0 * n as f64).sqrt().ceil() as usize).clamp(1, 2048) as f32;
        let cell = (2.0 * mean).max(extent / side).max(1e-3);
        let dims = [
            (((hi[0] - lo[0]) / cell).floor() as usize + 1).min(2049),
            (((hi[1] - lo[1]) / cell).floor() as usize + 1).min(2049),
        ];
        let cl = |v: f32, o: f32, d: usize| (((v - o) / cell).floor().max(0.0) as usize).min(d - 1);
        let mut grid = KeptGrid::new(if n > 0 { dims[0] * dims[1] } else { 0 });
        let mut keep = vec![false; n];
        let mut seam = 0u64;
        for (k, &i) in idx.iter().enumerate() {
            if k.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let b = &boxes[i];
            let (x0, x1) = (cl(b.0[0], lo[0], dims[0]), cl(b.1[0], lo[0], dims[0]));
            let (z0, z1) = (cl(b.0[2], lo[1], dims[1]), cl(b.1[2], lo[1], dims[1]));
            let mut hit = None;
            'outer: for gz in z0..=z1 {
                for gx in x0..=x1 {
                    if let Some(j) = grid.first_in(gz * dims[0] + gx, |j| boxes_overlap(b, &boxes[j])) {
                        hit = Some(j);
                        break 'outer;
                    }
                }
            }
            // Tiled runs (spec 66 § 2): a decision made against a point near the halo edge may
            // differ from the untiled run.
            if let (Some(j), Some(r)) = (hit, ctx.inputs.run_rect) {
                let q = p.position[j];
                let band = crate::tiles::SEAM_RISK_BAND_M;
                if q[0] - r[0] < band || r[2] - q[0] < band || q[2] - r[1] < band || r[3] - q[2] < band {
                    seam += 1;
                }
            }
            if hit.is_none() {
                keep[i] = true;
                for gz in z0..=z1 {
                    for gx in x0..=x1 {
                        grid.push(gz * dims[0] + gx, i as u32).map_err(|e| {
                            NodeError::new(
                                "limit_exceeded",
                                format!("points.prune: {e} grid entries over {n} points"),
                            )
                        })?;
                    }
                }
            }
        }
        if seam > 0 {
            ctx.count("seam_mismatch_risk", seam);
        }
        let kept: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
        out.0.push(item.with_payload(Payload::Points(p.select(&kept))));
    }
    Ok(vec![out])
}

fn eval_merge(_: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let input = crate::nodes::input(inputs, 0);
    let Some(first) = input.0.first() else {
        return Ok(vec![Bundle::empty()]);
    };
    let mut merged = PointSet::new();
    let mut tags = first.tags.clone();
    for item in &input.0 {
        if let Payload::Points(p) = &item.payload {
            merged.append(p).map_err(|name| {
                NodeError::new("attr_type_conflict", format!("attribute `{name}` has two types"))
            })?;
        }
        tags.extend(item.tags.iter().cloned());
    }
    let mut out = DataItem::points(merged);
    out.tags = tags;
    out.scope = first.scope.clone();
    Ok(vec![Bundle::one(out)])
}

fn eval_bake_scale(
    _: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    _: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    for_points(inputs, |p| {
        let (lo, hi, scale) = (
            p.bounds_min.make_mut(),
            p.bounds_max.make_mut(),
            p.scale.make_mut(),
        );
        for ((lo, hi), scale) in lo.iter_mut().zip(hi.iter_mut()).zip(scale.iter_mut()) {
            let s = *scale;
            *lo = std::array::from_fn(|k| lo[k] * s[k]);
            *hi = std::array::from_fn(|k| hi[k] * s[k]);
            *scale = s.map(|v| if v < 0.0 { -1.0 } else { 1.0 });
        }
        Ok(())
    })
}

fn eval_bounds(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let set_mode = s.text("mode") == "set";
    let (lo, hi, f) = (s.v3("min"), s.v3("max"), s.v3("factor"));
    for_points(inputs, |p| {
        for (bmin, bmax) in p.bounds_min.make_mut().iter_mut().zip(p.bounds_max.make_mut()) {
            if set_mode {
                *bmin = lo.map(|v| v as f32);
                *bmax = hi.map(|v| v as f32);
            } else {
                *bmin = std::array::from_fn(|k| bmin[k] * f[k] as f32);
                *bmax = std::array::from_fn(|k| bmax[k] * f[k] as f32);
            }
        }
        Ok(())
    })
}

/// Least-squares plane `y = a x + b z + c` through `pts`; returns the largest |deviation|.
fn plane_deviation(pts: &[[f64; 3]]) -> f64 {
    let n = pts.len() as f64;
    let (mut sx, mut sz, mut sy, mut sxx, mut szz, mut sxz, mut sxy, mut szy) =
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for p in pts {
        sx += p[0];
        sz += p[2];
        sy += p[1];
        sxx += p[0] * p[0];
        szz += p[2] * p[2];
        sxz += p[0] * p[2];
        sxy += p[0] * p[1];
        szy += p[2] * p[1];
    }
    // Normal equations [sxx sxz sx; sxz szz sz; sx sz n] [a b c] = [sxy szy sy], by Cramer's rule.
    let det3 = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let m = [[sxx, sxz, sx], [sxz, szz, sz], [sx, sz, n]];
    let r = [sxy, szy, sy];
    let d = det3(m);
    if d.abs() < 1e-12 {
        return f64::INFINITY;
    }
    let col = |k: usize| {
        let mut mk = m;
        for (row, rv) in mk.iter_mut().zip(r) {
            row[k] = rv;
        }
        det3(mk) / d
    };
    let (a, b, c) = (col(0), col(1), col(2));
    pts.iter()
        .map(|p| (p[1] - (a * p[0] + b * p[2] + c)).abs())
        .fold(0.0, f64::max)
}

fn eval_flat_only(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("points.flat_only").entered();
    let probe = s.f("probe_m") as f32;
    let max_dev = s.f("max_dev_m");
    let ground = crate::nodes::input(inputs, 1);
    let Some(g) = ground.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    }) else {
        // Like `sample.ground`: an empty result says why (audit L10).
        ctx.warn("no_terrain", "no ground item: every point is dropped");
        return Ok(vec![Bundle::empty()]);
    };
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &item.payload {
            let keep: Vec<bool> = (0..p.len())
                .map(|i| {
                    let [x, _, z] = p.position[i];
                    let pts = [
                        [f64::from(x), f64::from(g.height(x, z)), f64::from(z)],
                        [
                            f64::from(x + probe),
                            f64::from(g.height(x + probe, z)),
                            f64::from(z),
                        ],
                        [
                            f64::from(x - probe),
                            f64::from(g.height(x - probe, z)),
                            f64::from(z),
                        ],
                        [
                            f64::from(x),
                            f64::from(g.height(x, z + probe)),
                            f64::from(z + probe),
                        ],
                        [
                            f64::from(x),
                            f64::from(g.height(x, z - probe)),
                            f64::from(z - probe),
                        ],
                    ];
                    plane_deviation(&pts) <= max_dev
                })
                .collect();
            item.payload = Payload::Points(p.partition(&keep).0);
        }
    }
    Ok(vec![out])
}

fn stamp_one(
    ctx: &NodeCtx<'_>,
    src: &PointSet,
    tgt: &PointSet,
    s: &Settings,
    out: &mut PointSet,
) -> Result<(), NodeError> {
    // No targets means the old loop never appended, so the output set stays as it was.
    if tgt.is_empty() {
        return Ok(());
    }
    for (name, col) in &src.attrs {
        if let Some(mine) = out.attrs.get(name)
            && mine.ty() != col.ty()
        {
            return Err(NodeError::new(
                "attr_type_conflict",
                format!("attribute `{name}` has two types"),
            ));
        }
    }
    let n0 = out.len();
    for (name, col) in &src.attrs {
        out.attrs
            .entry(name.clone())
            .or_insert_with(|| AttrColumn::filled(col.ty(), n0));
    }
    let (tmode, cmode, smode) = (s.text("transform"), s.text("color"), s.text("seed"));
    let mut since_cancel = 0usize;
    let mut w = out.writer();
    for t in 0..tgt.len() {
        since_cancel += src.len() + 1;
        if since_cancel >= CHUNK {
            since_cancel = 0;
            ctx.check_cancel()?;
        }
        let tp = tgt.point(t);
        let (tq, ts, tt) = (quat(tp.rotation), Vec3::from(tp.scale), Vec3::from(tp.position));
        for i in 0..src.len() {
            let sp = src.point(i);
            let (sq, ss, st) = (quat(sp.rotation), Vec3::from(sp.scale), Vec3::from(sp.position));
            let (pos, q, sc): (Vec3, Quat, Vec3) = match tmode {
                "target" => (tt, tq, ts),
                "source" => (st + tt, sq, ss),
                _ => (tt + tq * (ts * st), tq * sq, ts * ss),
            };
            let mut np = sp;
            np.position = pos.to_array();
            np.rotation = q.normalize().to_array();
            np.scale = sc.to_array();
            np.color = match cmode {
                "source" => sp.color,
                "target" => tp.color,
                _ => std::array::from_fn(|k| sp.color[k] * tp.color[k]),
            };
            np.seed = match smode {
                "source" => sp.seed,
                "target" => tp.seed,
                _ => mix(sp.seed, tp.seed),
            };
            w.push(np);
            let dst = w.position.len() - 1;
            for (name, col) in &src.attrs {
                if let Some(mine) = w.attrs.get_mut(name) {
                    mine.copy_at(dst, col, i);
                }
            }
        }
    }
    Ok(())
}

fn eval_stamp(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let (src, tgt) = (crate::nodes::input(inputs, 0), crate::nodes::input(inputs, 1));
    let by_index = s.text("pairing") == "by_index";
    let mut out = Bundle::empty();
    let pts = |i: &DataItem| i.as_points().cloned().unwrap_or_default();
    let len = |i: &DataItem| i.as_points().map_or(0, PointSet::len);
    let (ns, nt) = (src.0.len(), tgt.0.len());
    // Pair k: `(source item, target item)`; all pairs run target-major.
    let pair = |k: usize| {
        if by_index {
            (if ns == 1 { 0 } else { k }, if nt == 1 { 0 } else { k })
        } else {
            (k % ns, k / ns)
        }
    };
    let pairs = if by_index {
        if ns != nt && ns != 1 && nt != 1 {
            return Err(NodeError::new(
                "stamp_count_mismatch",
                format!("by_index pairs {ns} source items with {nt} target items"),
            ));
        }
        if ns == 0 || nt == 0 { 0 } else { ns.max(nt) }
    } else {
        ns.saturating_mul(nt)
    };
    // Each pair emits |source| x |target| points: check the per-item cap and the run's element
    // budget before allocating anything (audit H2).
    let budget = ctx.limits().total_elements;
    let too_many = |what: &str, n: usize| {
        NodeError::new(
            "limit_exceeded",
            format!("total_elements: points.stamp would emit {n} {what} > {budget}"),
        )
    };
    if pairs > budget {
        return Err(too_many("items", pairs));
    }
    let mut total = 0usize;
    for k in 0..pairs {
        let (si, ti) = pair(k);
        let n = len(&src.0[si]).saturating_mul(len(&tgt.0[ti]));
        ctx.check_points(n)?;
        total = total.saturating_add(n);
    }
    if total > budget {
        return Err(too_many("points", total));
    }
    for k in 0..pairs {
        let (si, ti) = pair(k);
        let (si, ti) = (&src.0[si], &tgt.0[ti]);
        let mut set = PointSet::new();
        stamp_one(ctx, &pts(si), &pts(ti), s, &mut set)?;
        out.0.push(ti.with_payload(Payload::Points(set)));
    }
    Ok(vec![out])
}
