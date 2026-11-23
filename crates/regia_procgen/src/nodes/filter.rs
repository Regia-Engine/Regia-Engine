// SPDX-License-Identifier: MPL-2.0
//! Filters and region operations (`filter.*`, `region.*`, spec 63 § 7).

use crate::data::{Bundle, KindSet, Payload};
use crate::exec::{FiniteCheck, NodeCtx};
use crate::nodes::common::{Combine, Region, read_num};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SPATIAL, SettingType as T, Settings, num, pin_in, pin_multi,
    pin_out, pin_req, pins_split, set,
};

const COMBINE: &[&str] = &["max", "add", "any"];

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "filter.range",
        "filter",
        "Elements whose value lies in [min, max] go to `inside`, others to `outside` (points and table rows)",
        &[
            set("attr", T::Selector, "\"density\"", "Value to test (scalar)"),
            set("min", T::Float, "0.0", "Low bound"),
            set("max", T::Float, "1.0", "High bound"),
            set(
                "inclusive",
                T::Any,
                "[true, true]",
                "[low inclusive, high inclusive]",
            ),
        ],
        |_| {
            pins_split(KindSet::of(&[
                crate::data::DataKind::Points,
                crate::data::DataKind::Table,
            ]))
        },
        eval_range,
    )
    .checked(check_inclusive),
    BuiltinNode::new(
        "filter.compare",
        "filter",
        "Compares an attribute against a constant, another attribute (`rhs_attr`), column 0 of a one-row table on `rhs`, or the region value of `rhs`",
        &[
            set("attr", T::Selector, "\"density\"", "Left-hand value"),
            set(
                "op",
                T::Enum(&["eq", "ne", "lt", "le", "gt", "ge"]),
                "ge",
                "Operator",
            ),
            set("value", T::Float, "0.5", "Right-hand constant"),
            set(
                "rhs_attr",
                T::Name,
                "\"\"",
                "With `rhs_from: constant`: compare against this scalar selector of the same element instead of `value` (empty: off)",
            ),
            set(
                "rhs_from",
                T::Enum(&["constant", "table", "region"]),
                "constant",
                "Right-hand source",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_in("rhs", crate::nodes::ANY)],
                vec![pin_out("inside", POINTS), pin_out("outside", POINTS)],
            )
        },
        eval_compare,
    ),
    BuiltinNode::new(
        "filter.region",
        "filter",
        "Points whose region value is at least `min_value` go to `inside`",
        &[
            num("min_value", T::Float, "0.5", 0.0, 1.0, "Threshold"),
            set("combine", T::Enum(COMBINE), "max", "How region items combine"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_req("region", SPATIAL)],
                vec![pin_out("inside", POINTS), pin_out("outside", POINTS)],
            )
        },
        eval_filter_region,
    ),
    BuiltinNode::in_place(
        "region.subtract",
        "region",
        "scale: density *= 1 - r(p); cut: drop where r(p) >= cut_at",
        &[
            set("mode", T::Enum(&["scale", "cut"]), "cut", "Rule"),
            num("cut_at", T::Float, "0.5", 0.0, 1.0, "Cut threshold"),
            set("keep_zero", T::Bool, "false", "Keep zero-density points"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_multi("minus", SPATIAL)],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_subtract,
    ),
    BuiltinNode::in_place(
        "region.intersect",
        "region",
        "density = density · r (mul) or min(density, r); zero dropped",
        &[set("mode", T::Enum(&["mul", "min"]), "mul", "Rule")],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_multi("with", SPATIAL)],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_intersect,
    ),
];

/// `inclusive` is a list of up to two booleans: anything else used to read as `true` (audit L8).
fn check_inclusive(s: &Settings) -> Vec<(String, String)> {
    let bools = |i: &[crate::value::SettingValue]| i.len() <= 2 && i.iter().all(|x| x.as_bool().is_some());
    let ok = s.get("inclusive").is_none_or(|v| v.as_items().is_some_and(bools));
    if ok {
        Vec::new()
    } else {
        vec![(
            "setting_type".to_string(),
            "`inclusive` must be [low inclusive, high inclusive] booleans".to_string(),
        )]
    }
}

fn eval_range(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let (lo, hi) = (s.f("min"), s.f("max"));
    let inc: Vec<bool> = s
        .get("inclusive")
        .and_then(|v| v.as_items())
        .map(|v| v.iter().map(|x| x.as_bool().unwrap_or(true)).collect())
        .unwrap_or_else(|| vec![true, true]);
    let (inc_lo, inc_hi) = (
        inc.first().copied().unwrap_or(true),
        inc.get(1).copied().unwrap_or(true),
    );
    let sel = s.sel("attr");
    let test = |v: f64| (if inc_lo { v >= lo } else { v > lo }) && (if inc_hi { v <= hi } else { v < hi });
    let (mut inside, mut outside) = (Bundle::empty(), Bundle::empty());
    // Rows are only moved between sets, and every input was checked when its node produced it.
    ctx.finite_check = FiniteCheck::Nothing;
    for item in crate::nodes::input_items(inputs, 0) {
        // Built-in scalars and single components read straight from the point set (the values
        // `read_num` would give, without its f64 copy); anything else goes through `read_num`.
        let keep = match builtin_keep(item, &sel, &test) {
            Some(keep) => keep,
            None => {
                let vals = read_num(item, &sel)?;
                if vals.width != 1 {
                    return Err(NodeError::new(
                        "attr_type_mismatch",
                        "filter.range needs a scalar; add a `:x` suffix",
                    ));
                }
                vals.values.iter().map(|v| test(*v)).collect()
            }
        };
        match &item.payload {
            Payload::Points(p) => {
                let (a, b) = split_points(ctx, p, &keep)?;
                inside.0.push(item.with_payload(Payload::Points(a)));
                outside.0.push(item.with_payload(Payload::Points(b)));
            }
            _ => split(item, &keep, &mut inside, &mut outside),
        }
    }
    Ok(vec![inside, outside])
}

/// `test` of every point's built-in scalar named by `sel` (`density`, `hardness`, `seed`, `index`,
/// or one component of a vector built-in), or `None` for anything `read_num` must read.
fn builtin_keep(
    item: &crate::data::DataItem,
    sel: &crate::data::Selector,
    test: &impl Fn(f64) -> bool,
) -> Option<Vec<bool>> {
    let Payload::Points(p) = &item.payload else {
        return None;
    };
    let sel = sel.resolve(item).ok()?;
    if sel.item_scope {
        return None;
    }
    fn comp<const N: usize>(v: &[[f32; N]], c: usize, test: &impl Fn(f64) -> bool) -> Option<Vec<bool>> {
        (c < N).then(|| v.iter().map(|x| test(f64::from(x[c]))).collect())
    }
    match (sel.name.as_str(), sel.parts.as_slice()) {
        ("density", []) => Some(p.density.iter().map(|v| test(f64::from(*v))).collect()),
        ("hardness", []) => Some(p.hardness.iter().map(|v| test(f64::from(*v))).collect()),
        ("seed", []) => Some(p.seed.iter().map(|v| test(f64::from(*v))).collect()),
        ("index", []) => Some((0..p.len()).map(|i| test(i as f64)).collect()),
        ("position", [c]) => comp(&p.position, *c, test),
        ("scale", [c]) => comp(&p.scale, *c, test),
        ("bounds_min", [c]) => comp(&p.bounds_min, *c, test),
        ("bounds_max", [c]) => comp(&p.bounds_max, *c, test),
        ("rotation", [c]) => comp(&p.rotation, *c, test),
        ("color", [c]) => comp(&p.color, *c, test),
        _ => None,
    }
}

/// How rows go to the two sides of a split: coherent masks (points in spatial order) copy whole runs,
/// any other mask reads through index lists, so no column pays a mispredicted branch per row.
enum Rows {
    /// `(start, end, kept)` runs in row order.
    Runs(Vec<(usize, usize, bool)>),
    /// Kept and rejected row indices.
    Indices(Vec<usize>, Vec<usize>),
}

impl Rows {
    fn of(keep: &[bool]) -> Self {
        // Below this mean run length, one `memcpy` per run costs more than an index per row.
        const MIN_MEAN_RUN: usize = 16;
        let most = keep.len() / MIN_MEAN_RUN + 1;
        let mut runs = Vec::new();
        let mut start = 0;
        for run in keep.chunk_by(|a, b| a == b) {
            if runs.len() == most {
                let (ia, ib) = (0..keep.len()).partition(|&i| keep[i]);
                return Self::Indices(ia, ib);
            }
            runs.push((start, start + run.len(), run[0]));
            start += run.len();
        }
        Self::Runs(runs)
    }
}

/// Appends the rows of `src` to `a` (kept) and `b` (rejected), in order. A column shorter than the
/// mask gives only the rows it has.
fn scatter<T: Copy + Send + Sync>(src: &[T], rows: &Rows, kept: usize, a: &mut Vec<T>, b: &mut Vec<T>) {
    a.reserve_exact(kept);
    b.reserve_exact(src.len().saturating_sub(kept));
    match rows {
        Rows::Runs(runs) => {
            for &(start, end, k) in runs {
                let run = &src[start.min(src.len())..end.min(src.len())];
                if k {
                    a.extend_from_slice(run);
                } else {
                    b.extend_from_slice(run);
                }
            }
        }
        Rows::Indices(ia, ib) => {
            a.extend(ia.iter().filter_map(|&i| src.get(i).copied()));
            b.extend(ib.iter().filter_map(|&i| src.get(i).copied()));
        }
    }
}

/// `PointSet::partition` (kept, rejected; order kept) with each built-in column scattered by its
/// own job, in parallel when the run has a pool.
fn split_points(
    ctx: &NodeCtx<'_>,
    p: &crate::data::PointSet,
    keep: &[bool],
) -> Result<(crate::data::PointSet, crate::data::PointSet), NodeError> {
    let kept = keep.iter().filter(|k| **k).count();
    let rows = Rows::of(keep);
    let rows = &rows;
    let (mut a, mut b) = (crate::data::PointSet::default(), crate::data::PointSet::default());
    {
        use crate::data::PointSet as P;
        let P {
            position: a_pos,
            rotation: a_rot,
            scale: a_scale,
            color: a_color,
            density: a_density,
            seed: a_seed,
            bounds_min: a_min,
            bounds_max: a_max,
            hardness: a_hard,
            attrs: a_attrs,
        } = &mut a;
        let P {
            position: b_pos,
            rotation: b_rot,
            scale: b_scale,
            color: b_color,
            density: b_density,
            seed: b_seed,
            bounds_min: b_min,
            bounds_max: b_max,
            hardness: b_hard,
            attrs: b_attrs,
        } = &mut b;
        type Job<'a> = Box<dyn FnOnce() + Send + 'a>;
        let jobs: Vec<Job<'_>> = vec![
            Box::new(|| scatter(&p.position, rows, kept, a_pos, b_pos)),
            Box::new(|| scatter(&p.rotation, rows, kept, a_rot, b_rot)),
            Box::new(|| scatter(&p.scale, rows, kept, a_scale, b_scale)),
            Box::new(|| scatter(&p.color, rows, kept, a_color, b_color)),
            Box::new(|| scatter(&p.density, rows, kept, a_density, b_density)),
            Box::new(|| scatter(&p.seed, rows, kept, a_seed, b_seed)),
            Box::new(|| scatter(&p.bounds_min, rows, kept, a_min, b_min)),
            Box::new(|| scatter(&p.bounds_max, rows, kept, a_max, b_max)),
            Box::new(|| scatter(&p.hardness, rows, kept, a_hard, b_hard)),
            Box::new(|| {
                if p.attrs.is_empty() {
                    return;
                }
                let (ia, ib): (Vec<usize>, Vec<usize>) = (0..keep.len()).partition(|&i| keep[i]);
                *a_attrs = p.attrs.iter().map(|(k, c)| (k.clone(), c.select(&ia))).collect();
                *b_attrs = p.attrs.iter().map(|(k, c)| (k.clone(), c.select(&ib))).collect();
            }),
        ];
        ctx.for_each_job(jobs, |job| job())?;
    }
    Ok((a, b))
}

/// Appends the kept and rejected parts of an item to two bundles.
fn split(item: &crate::data::DataItem, keep: &[bool], inside: &mut Bundle, outside: &mut Bundle) {
    match &item.payload {
        Payload::Points(p) => {
            let (a, b) = p.partition(keep);
            inside.0.push(item.with_payload(Payload::Points(a)));
            outside.0.push(item.with_payload(Payload::Points(b)));
        }
        Payload::Table(t) => {
            let a: Vec<usize> = (0..t.rows).filter(|i| keep[*i]).collect();
            let b: Vec<usize> = (0..t.rows).filter(|i| !keep[*i]).collect();
            inside.0.push(item.with_payload(Payload::Table(t.select(&a))));
            outside.0.push(item.with_payload(Payload::Table(t.select(&b))));
        }
        _ => {}
    }
}

fn cmp(op: &str, a: f64, b: f64) -> bool {
    match op {
        "eq" => a == b,
        "ne" => a != b,
        "lt" => a < b,
        "le" => a <= b,
        "gt" => a > b,
        _ => a >= b,
    }
}

fn eval_compare(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let sel = s.sel("attr");
    let rhs = crate::nodes::input(inputs, 1);
    let from = s.text("rhs_from").to_string();
    let constant = match from.as_str() {
        "table" => {
            let t = rhs.0.iter().find_map(|i| match &i.payload {
                Payload::Table(t) if t.rows >= 1 => Some(t.clone()),
                _ => None,
            });
            let v = t
                .and_then(|t| t.columns.first().and_then(|(_, c)| c.components(0)))
                .and_then(|c| c.first().copied())
                .ok_or_else(|| {
                    NodeError::new("attr_missing", "`rhs` needs a table with a numeric column 0")
                })?;
            Some(v)
        }
        "region" => None,
        _ => Some(s.f("value")),
    };
    let region = if from == "region" {
        Some(Region::new(&rhs, Combine::Max)?)
    } else {
        None
    };
    // `rhs_attr` replaces the constant with a per-element value (spec 63 § 7).
    let rhs_attr = match s.text("rhs_attr") {
        "" => None,
        _ if from != "constant" => None,
        text => Some(crate::data::Selector::parse(text).map_err(|m| NodeError::new("setting_type", m))?),
    };
    let scalar_only = || {
        NodeError::new(
            "attr_type_mismatch",
            "filter.compare needs scalars; add a `:x` suffix",
        )
    };
    let (mut inside, mut outside) = (Bundle::empty(), Bundle::empty());
    for item in crate::nodes::input_items(inputs, 0) {
        let vals = read_num(item, &sel)?;
        // A vector read as a scalar is an error unless a suffix picks one part (spec 62 § 3.5).
        if vals.width != 1 {
            return Err(scalar_only());
        }
        let rhs_vals = match &rhs_attr {
            Some(r) => {
                let v = read_num(item, r)?;
                if v.width != 1 {
                    return Err(scalar_only());
                }
                Some(v)
            }
            None => None,
        };
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let keep: Vec<bool> = (0..p.len())
            .map(|i| {
                let b = match &rhs_vals {
                    Some(v) => v.scalar(i),
                    None => constant.unwrap_or_else(|| {
                        region.as_ref().map_or(0.0, |r| f64::from(r.value(p.position[i])))
                    }),
                };
                cmp(&op, vals.scalar(i), b)
            })
            .collect();
        split(item, &keep, &mut inside, &mut outside);
    }
    Ok(vec![inside, outside])
}

fn eval_filter_region(
    _: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let min = s.f("min_value") as f32;
    let reg = crate::nodes::input(inputs, 1);
    let region = Region::new(&reg, Combine::from_name(s.text("combine")))?;
    let (mut inside, mut outside) = (Bundle::empty(), Bundle::empty());
    for item in crate::nodes::input_items(inputs, 0) {
        if let Payload::Points(p) = &item.payload {
            let keep: Vec<bool> = p.position.iter().map(|x| region.value(*x) >= min).collect();
            split(item, &keep, &mut inside, &mut outside);
        }
    }
    Ok(vec![inside, outside])
}

fn eval_subtract(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let cut = s.text("mode") == "cut";
    let cut_at = s.f("cut_at") as f32;
    let keep_zero = s.b("keep_zero");
    let minus = crate::nodes::input(inputs, 1);
    let region = Region::new(&minus, Combine::Max)?;
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            let mut keep = vec![true; p.len()];
            let density = p.density.make_mut();
            for (i, k) in keep.iter_mut().enumerate() {
                let r = region.value(p.position[i]);
                if cut {
                    *k = r < cut_at;
                } else {
                    density[i] = (density[i] * (1.0 - r)).clamp(0.0, 1.0);
                }
                if !keep_zero && density[i] <= 0.0 {
                    *k = false;
                }
            }
            *p = p.partition(&keep).0;
        }
    }
    Ok(vec![out])
}

fn eval_intersect(
    _: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let mul = s.text("mode") == "mul";
    let with = crate::nodes::input(inputs, 1);
    let region = Region::new(&with, Combine::Max)?;
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            let mut keep = vec![true; p.len()];
            let density = p.density.make_mut();
            for (i, k) in keep.iter_mut().enumerate() {
                let r = region.value(p.position[i]);
                density[i] = if mul { density[i] * r } else { density[i].min(r) };
                *k = density[i] > 0.0;
            }
            *p = p.partition(&keep).0;
        }
    }
    Ok(vec![out])
}

#[cfg(test)]
mod split_tests {
    use super::{Rows, scatter};

    /// Both ways of moving rows give the plain per-row split, for every mask shape and for a column
    /// shorter or longer than the mask.
    #[test]
    fn scatter_matches_the_per_row_split_for_every_mask_shape() {
        let n = 5000;
        let mut x = 0x2545_F491u32;
        let mut random = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x & 1 == 1
        };
        let masks: Vec<(Vec<bool>, bool)> = vec![
            (vec![true; n], true),
            (vec![false; n], true),
            ((0..n).map(|i| i < n / 3).collect(), true),
            ((0..n).map(|i| (i / 400).is_multiple_of(2)).collect(), true),
            ((0..n).map(|i| i.is_multiple_of(2)).collect(), false),
            ((0..n).map(|_| random()).collect(), false),
            (Vec::new(), true),
        ];
        for (keep, runs) in masks {
            let rows = Rows::of(&keep);
            assert_eq!(matches!(rows, Rows::Runs(_)), runs, "mask shape");
            let kept = keep.iter().filter(|k| **k).count();
            for len in [n, n - 100, n + 100] {
                let src: Vec<[f32; 2]> = (0..len).map(|i| [i as f32, -(i as f32)]).collect();
                let (mut a, mut b) = (Vec::new(), Vec::new());
                scatter(&src, &rows, kept, &mut a, &mut b);
                let (mut want_a, mut want_b) = (Vec::new(), Vec::new());
                for (v, k) in src.iter().zip(&keep) {
                    if *k {
                        want_a.push(*v);
                    } else {
                        want_b.push(*v);
                    }
                }
                assert_eq!((a, b), (want_a, want_b), "len {len}");
            }
        }
    }
}
