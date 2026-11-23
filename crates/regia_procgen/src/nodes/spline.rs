// SPDX-License-Identifier: MPL-2.0
//! `sample.spline`, `spline.*` and `polygon.*`: spline and polygon nodes (spec 64 § 4).
//!
//! Splines travel as world-space [`SplineData`] items, polygons as [`PolygonData`] items. Crossings
//! and clips work on a dense uniform-distance polyline of each curve (at most 1 m apart), which is
//! exact for straight segments and within millimeters on curves of a few meters radius.

use std::sync::Arc;

use regia_core::math::{Vec2, Vec3};
use regia_scene::spline::{
    SPLINE_MAX_POINTS, Spline, SplineAttrDecl, SplineAttrType, SplineCurve, SplinePoint, SplineValue,
    TangentMode,
};

use crate::data::{
    AttrColumn, AttrValue, Bundle, DataItem, DataKind, KindSet, Payload, Point, PointSet, SplineData,
};
use crate::exec::NodeCtx;
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, input, num, pin_multi, pin_out,
    pin_req, set,
};
use crate::polygon::{BoolOp, Join, PolygonData};
use crate::rot::Rot;
use crate::value::SettingValue;

// `KindSet` bits follow `DataKind` order: Points 0, Table 1, Spline 2, Polygon 3.
pub(crate) const SPLINE: KindSet = KindSet(1 << DataKind::Spline as u16);
pub(crate) const POLYGON: KindSet = KindSet(1 << DataKind::Polygon as u16);
const POINTS_OR_SPLINE: KindSet = KindSet(1 << DataKind::Points as u16 | 1 << DataKind::Spline as u16);

/// Attributes `sample.spline` can write.
const SAMPLE_WRITES: [&str; 6] = ["along", "distance_m", "key", "tangent", "curvature", "turn_deg"];

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "sample.spline",
        "sample",
        "Points along, across (`band`) or inside a spline; rotation from the spline frame",
        &[
            set(
                "by",
                T::Enum(&["step", "count", "per_segment"]),
                "step",
                "Spacing rule along the curve",
            ),
            num("step_m", T::Float, "2.0", 0.001, 1.0e6, "Distance between points"),
            num("count", T::Int, "10", 1.0, 1.0e7, "Points for `by: count`"),
            num(
                "per_segment",
                T::Int,
                "4",
                1.0,
                1.0e6,
                "Points per segment for `by: per_segment`",
            ),
            set(
                "fit_ends",
                T::Bool,
                "true",
                "Adjust `step_m` so both ends get a point",
            ),
            num(
                "start_m",
                T::Float,
                "0.0",
                0.0,
                1.0e7,
                "Skip this much at the start",
            ),
            num("end_m", T::Float, "0.0", 0.0, 1.0e7, "Skip this much at the end"),
            set(
                "place",
                T::Enum(&["curve", "band", "inside"]),
                "curve",
                "On the curve, a band across it, or inside a closed spline",
            ),
            num(
                "band_m",
                T::Float,
                "1.0",
                0.0,
                1.0e6,
                "Band width factor (times the width scale)",
            ),
            num(
                "spacing_m",
                T::Float,
                "2.0",
                0.01,
                1.0e6,
                "Row and column spacing for `band` and `inside`",
            ),
            num(
                "jitter",
                T::Float,
                "0.0",
                0.0,
                1.0,
                "Random offset along the curve, fraction of half a step",
            ),
            set(
                "write",
                T::NameList,
                "[along, distance_m, key, tangent, curvature, turn_deg]",
                "Attributes to write",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("in", SPLINE)], vec![pin_out("out", POINTS)]),
        eval_sample,
    )
    .checked(check_write),
    BuiltinNode::new(
        "spline.from_points",
        "spline",
        "One spline per point item through its points; point attributes become spline values",
        &[
            set("closed", T::Bool, "false", "Close the spline"),
            set(
                "mode",
                T::Enum(&["Auto", "Linear"]),
                "Auto",
                "Tangent mode of every point",
            ),
            set(
                "tangent_attrs",
                T::Any,
                "none",
                "`none` or (in, out) Vec3 attribute names (points become Free)",
            ),
            set(
                "order",
                T::Enum(&["index", "nearest_chain"]),
                "index",
                "Point order",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("in", POINTS)], vec![pin_out("out", SPLINE)]),
        eval_from_points,
    ),
    BuiltinNode::new(
        "spline.simplify",
        "spline",
        "Merges close control points, then Ramer-Douglas-Peucker on the rest (ends, Free and Aligned kept)",
        &[
            num(
                "merge_m",
                T::Float,
                "0.05",
                0.0,
                1.0e6,
                "Drop points closer than this to the previous kept one",
            ),
            num("tolerance_m", T::Float, "0.1", 0.0, 1.0e6, "RDP tolerance"),
        ],
        |_| PinSpec::new(vec![pin_req("in", SPLINE)], vec![pin_out("out", SPLINE)]),
        eval_simplify,
    ),
    BuiltinNode::new(
        "spline.split",
        "spline",
        "Cuts each spline into pieces (tangents kept at the cuts); item scope `piece`",
        &[
            set(
                "at",
                T::Enum(&["distances", "every", "points", "attr"]),
                "every",
                "Where to cut",
            ),
            set("distances_m", T::Any, "[]", "Cut distances for `at: distances`"),
            num(
                "every_m",
                T::Float,
                "50.0",
                0.01,
                1.0e7,
                "Piece length for `at: every`",
            ),
            set(
                "attr",
                T::Name,
                "\"\"",
                "F32 spline value: cut at control points where it is not 0",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("in", SPLINE)], vec![pin_out("out", SPLINE)]),
        eval_split,
    ),
    BuiltinNode::new(
        "spline.intersect",
        "spline",
        "Crossings between all input splines (and self-crossings); optionally inserts control points there",
        &[
            set(
                "plane",
                T::Enum(&["xz", "3d"]),
                "xz",
                "Cross in XZ, or also within tolerance in 3D",
            ),
            num(
                "tolerance_m",
                T::Float,
                "0.05",
                0.0,
                1.0e6,
                "Height tolerance for `3d`",
            ),
            set(
                "insert",
                T::Bool,
                "true",
                "Insert Free control points at the crossings",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_multi("in", SPLINE)],
                vec![pin_out("out_splines", SPLINE), pin_out("out_points", POINTS)],
            )
        },
        eval_intersect,
    ),
    BuiltinNode::new(
        "spline.offset",
        "spline",
        "A parallel curve to the right (positive) or left, loops removed, rebuilt from a polyline",
        &[
            num(
                "distance_m",
                T::Float,
                "2.0",
                -1.0e6,
                1.0e6,
                "Sideways offset (positive = right)",
            ),
            num(
                "vertical_m",
                T::Float,
                "0.0",
                -1.0e6,
                1.0e6,
                "Offset along the frame's up",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("in", SPLINE)], vec![pin_out("out", SPLINE)]),
        eval_offset,
    ),
    BuiltinNode::new(
        "spline.reverse",
        "spline",
        "Reverses point order and swaps (negated) tangents",
        &[],
        |_| PinSpec::new(vec![pin_req("in", SPLINE)], vec![pin_out("out", SPLINE)]),
        eval_reverse,
    ),
    BuiltinNode::new(
        "spline.clip",
        "spline",
        "Splits splines where they cross polygon boundaries and routes pieces inside or outside",
        &[],
        |_| {
            PinSpec::new(
                vec![pin_req("in", SPLINE), pin_multi("polygons", POLYGON)],
                vec![pin_out("inside", SPLINE), pin_out("outside", SPLINE)],
            )
        },
        eval_clip,
    ),
    BuiltinNode::new(
        "polygon.from",
        "polygon",
        "A polygon from points (convex hull or ordered ring) or from a closed spline",
        &[
            set(
                "mode",
                T::Enum(&["hull", "ordered"]),
                "hull",
                "Points: convex hull or ring in order",
            ),
            num(
                "max_error_m",
                T::Float,
                "0.1",
                0.001,
                1.0e3,
                "Spline flattening error",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS_OR_SPLINE)],
                vec![pin_out("out", POLYGON)],
            )
        },
        eval_polygon_from,
    ),
    BuiltinNode::new(
        "polygon.bool",
        "polygon",
        "Boolean of all `a` rings against all `b` rings (each pin unioned first)",
        &[set(
            "op",
            T::Enum(&["union", "intersection", "difference", "xor"]),
            "union",
            "Operation",
        )],
        |_| {
            PinSpec::new(
                vec![pin_multi("a", POLYGON), pin_multi("b", POLYGON)],
                vec![pin_out("out", POLYGON)],
            )
        },
        eval_bool,
    ),
    BuiltinNode::new(
        "polygon.offset",
        "polygon",
        "Grows (positive) or shrinks polygons; collapsed parts disappear",
        &[
            num("distance_m", T::Float, "1.0", -1.0e6, 1.0e6, "Offset distance"),
            set(
                "join",
                T::Enum(&["miter", "round", "square"]),
                "miter",
                "Corner joins",
            ),
            num(
                "miter_limit",
                T::Float,
                "2.0",
                1.0,
                100.0,
                "Miter length limit (times the distance)",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("in", POLYGON)], vec![pin_out("out", POLYGON)]),
        eval_poly_offset,
    ),
    BuiltinNode::new(
        "polygon.cut",
        "polygon",
        "Splits polygons along cutter splines. The removed band is `gap_m` wide and at least 1 mm, even when `gap_m` is 0; one item per piece by centroid",
        &[num(
            "gap_m",
            T::Float,
            "0.0",
            0.0,
            1.0e6,
            "Band removed along each cut, at least 1 mm",
        )],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POLYGON), pin_multi("cutters", SPLINE)],
                vec![pin_out("out", POLYGON)],
            )
        },
        eval_cut,
    ),
    BuiltinNode::new(
        "polygon.to_spline",
        "polygon",
        "Each ring becomes a closed Linear spline",
        &[],
        |_| PinSpec::new(vec![pin_req("in", POLYGON)], vec![pin_out("out", SPLINE)]),
        eval_to_spline,
    ),
];

fn check_write(s: &Settings) -> Vec<(String, String)> {
    s.names("write")
        .into_iter()
        .filter(|n| !SAMPLE_WRITES.contains(&n.as_str()))
        .map(|n| {
            (
                "setting_invalid".to_string(),
                format!("`write` has unknown attribute `{n}`"),
            )
        })
        .collect()
}

fn spline_of(item: &DataItem) -> Option<&SplineData> {
    match &item.payload {
        Payload::Spline(s) => Some(s),
        _ => None,
    }
}

fn polygon_of(item: &DataItem) -> Option<&PolygonData> {
    match &item.payload {
        Payload::Polygon(p) => Some(p),
        _ => None,
    }
}

fn spline_item(from: &DataItem, spline: Spline) -> DataItem {
    from.with_payload(Payload::Spline(Arc::new(SplineData::new(spline))))
}

fn polygon_item(from: &DataItem, poly: PolygonData) -> DataItem {
    from.with_payload(Payload::Polygon(Arc::new(poly)))
}

fn poly_err(e: crate::polygon::PolygonError) -> NodeError {
    NodeError::new(e.code(), e.to_string())
}

/// Rotation whose local X is `t`, Y is `u` and Z is `t × u` (Shepperd's method, `sqrt` only).
pub(crate) fn frame_rot(t: Vec3, u: Vec3) -> Rot {
    let r = t.cross(u);
    // Columns x = t, y = u, z = r; m[row][col].
    let (m00, m01, m02) = (t.x, u.x, r.x);
    let (m10, m11, m12) = (t.y, u.y, r.y);
    let (m20, m21, m22) = (t.z, u.z, r.z);
    let tr = m00 + m11 + m22;
    let q = if tr > 0.0 {
        let s = (tr + 1.0).sqrt() * 2.0;
        [(m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [(m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [(m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s]
    };
    Rot(q).normalize()
}

/// Distances along `[start, end]` for `sample.spline` on the curve.
fn curve_distances(
    curve: &SplineCurve,
    s: &Settings,
    start: f64,
    end: f64,
    max_n: usize,
) -> Result<(Vec<f64>, f64), NodeError> {
    let usable = end - start;
    let too_many = |n: usize| -> Result<(), NodeError> {
        if n > max_n {
            Err(NodeError::new(
                "limit_exceeded",
                format!("points_per_item: {n} > {max_n}"),
            ))
        } else {
            Ok(())
        }
    };
    Ok(match s.text("by") {
        "count" => {
            let n = s.i("count").max(1) as usize;
            if n == 1 {
                return Ok((vec![start], usable));
            }
            too_many(n)?;
            let step = usable / (n - 1) as f64;
            ((0..n).map(|k| start + step * k as f64).collect(), step)
        }
        "per_segment" => {
            let per = s.i("per_segment").max(1) as usize;
            let starts = curve.segment_starts();
            let mut out = Vec::new();
            for (i, s0) in starts.iter().enumerate() {
                let (a, len) = (f64::from(*s0), f64::from(curve.segment_length(i)));
                for k in 0..per {
                    let d = a + len * k as f64 / per as f64;
                    if d >= start && d <= end {
                        out.push(d);
                    }
                }
            }
            // The end station, unless it repeats the first one (a closed curve's seam).
            let seam =
                curve.is_closed() && end >= curve.length_f64() && out.first().is_some_and(|d| *d <= start);
            if !seam && out.last().is_none_or(|d| *d < end) {
                out.push(end);
            }
            let step = usable / out.len().max(1) as f64;
            (out, step)
        }
        _ => {
            let step = s.f("step_m").max(1e-3);
            if usable <= 0.0 {
                (vec![start], step)
            } else if s.b("fit_ends") {
                let n = (usable / step).round().max(1.0) as usize;
                too_many(n)?;
                let st = usable / n as f64;
                ((0..=n).map(|k| start + st * k as f64).collect(), st)
            } else {
                let n = (usable / step + 1e-9).floor() as usize;
                too_many(n)?;
                ((0..=n).map(|k| start + step * k as f64).collect(), step)
            }
        }
    })
}

fn value_columns(spline: &Spline) -> Vec<(String, AttrColumn)> {
    spline
        .attrs
        .iter()
        .map(|a| {
            let col = match a.ty {
                SplineAttrType::F32 => AttrColumn::Float(Vec::new().into()),
                SplineAttrType::Vec3 => AttrColumn::Vec3(Vec::new().into()),
                SplineAttrType::Name => AttrColumn::Name(Vec::new().into()),
            };
            (a.name.clone(), col)
        })
        .collect()
}

fn push_values(cols: &mut [(String, AttrColumn)], values: &[SplineValue]) {
    for ((_, col), v) in cols.iter_mut().zip(values) {
        let v = match v {
            SplineValue::F32(x) => AttrValue::Float(f64::from(*x)),
            SplineValue::Vec3(x) => AttrValue::Vec3(x.to_array().map(f64::from)),
            SplineValue::Name(n) => AttrValue::Name(n.clone()),
        };
        col.push(v);
    }
}

fn eval_sample(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.sample_spline").entered();
    let write = s.names("write");
    let wants = |n: &str| write.iter().any(|w| w == n);
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Some(sd) = spline_of(item) else {
            continue;
        };
        let curve = sd.curve();
        let len = f64::from(curve.length());
        let mut points = PointSet::new();
        if sd.spline.points.len() < 2 || !crate::nodes::common::has_length(len) {
            ctx.warn(
                "spline_degenerate",
                "a spline with fewer than 2 points or no length gives no points",
            );
            out.0.push(item.with_payload(Payload::Points(points)));
            continue;
        }
        let start = s.f("start_m").min(len);
        let end = (len - s.f("end_m")).max(start);
        let mut cols = value_columns(&sd.spline);
        let mut along = Vec::new();
        let mut dist = Vec::new();
        let mut keys = Vec::new();
        let mut tangents = Vec::new();
        let mut curv = Vec::new();
        let mut turns = Vec::new();
        let mut prev_heading: Option<f64> = None;
        let mut emit =
            |ctx: &mut NodeCtx<'_>, points: &mut PointSet, d: f64, side: f64| -> Result<(), NodeError> {
                let sm = curve.at_distance_f64(d);
                let pos = sm.position + sm.right * side as f32;
                let position = pos.to_array();
                points.push(Point {
                    position,
                    rotation: frame_rot(sm.tangent, sm.up).to_array(),
                    seed: ctx.point_seed(position),
                    ..Point::default()
                });
                along.push(d / len);
                dist.push(d);
                keys.push(f64::from(sm.key));
                tangents.push(sm.tangent.to_array().map(f64::from));
                curv.push(f64::from(sm.curvature));
                let heading = crate::detmath::atan2(f64::from(sm.tangent.x), f64::from(sm.tangent.z));
                let turn = prev_heading.map_or(0.0, |p| {
                    let mut t = heading - p;
                    while t > std::f64::consts::PI {
                        t -= std::f64::consts::TAU;
                    }
                    while t < -std::f64::consts::PI {
                        t += std::f64::consts::TAU;
                    }
                    t.to_degrees()
                });
                prev_heading = Some(heading);
                turns.push(turn);
                push_values(&mut cols, &sm.values);
                ctx.check_points(points.len())
            };
        match s.text("place") {
            "inside" => {
                if !sd.spline.closed {
                    return Err(NodeError::new(
                        "spline_not_closed",
                        "`place: inside` needs a closed spline",
                    ));
                }
                let ring: Vec<[f64; 2]> = curve
                    .to_polyline(0.05)
                    .iter()
                    .map(|p| [f64::from(p.x), f64::from(p.z)])
                    .collect();
                let poly = PolygonData::new([0.0; 3], &[ring]).map_err(poly_err)?;
                let y = sd.spline.points.iter().map(|p| p.position.y).sum::<f32>()
                    / sd.spline.points.len() as f32;
                let spacing = s.f("spacing_m").max(0.01);
                if let Some(b) = poly.bounds() {
                    // The world lattice `sample.region` uses, half-open on the bounds.
                    let lo = |v: f64| (v / spacing).floor() as i64;
                    let hi = |v: f64| ((v / spacing).ceil() as i64).saturating_sub(1);
                    let (j0, j1) = (lo(b[0]), hi(b[2]));
                    let (k0, k1) = (lo(b[1]), hi(b[3]));
                    let span = crate::nodes::sample::cell_span;
                    ctx.check_points(
                        usize::try_from(span(j0, j1).saturating_mul(span(k0, k1))).unwrap_or(usize::MAX),
                    )?;
                    for k in k0..=k1 {
                        ctx.check_cancel()?;
                        for j in j0..=j1 {
                            let (x, z) = (spacing * (j as f64 + 0.5), spacing * (k as f64 + 0.5));
                            if !poly.contains(x, z) {
                                continue;
                            }
                            let position = [x as f32, y, z as f32];
                            points.push(Point {
                                position,
                                seed: ctx.point_seed(position),
                                ..Point::default()
                            });
                            ctx.check_points(points.len())?;
                        }
                    }
                }
                out.0.push(item.with_payload(Payload::Points(points)));
                continue;
            }
            "band" => {
                let spacing = s.f("spacing_m").max(0.01);
                let rows = ((end - start) / spacing + 1e-9).floor() as usize;
                for r in 0..=rows {
                    ctx.check_cancel()?;
                    let d = start + spacing * r as f64;
                    let w = s.f("band_m") * f64::from(curve.at_distance_f64(d).scale.x);
                    let n = (w / spacing + 1e-9).floor() as usize;
                    let first = -(n as f64) * spacing * 0.5;
                    for c in 0..=n {
                        emit(ctx, &mut points, d, first + spacing * c as f64)?;
                    }
                }
            }
            _ => {
                let (ds, step) = curve_distances(&curve, s, start, end, ctx.inputs.limits.points_per_item)?;
                let jitter = s.f("jitter");
                for d in ds {
                    let d = if jitter > 0.0 {
                        let p = curve.at_distance_f64(d).position.to_array();
                        let r = ctx.draw(ctx.point_seed(p), 0) * 2.0 - 1.0;
                        (d + r * jitter * step * 0.5).clamp(start, end)
                    } else {
                        d
                    };
                    emit(ctx, &mut points, d, 0.0)?;
                }
            }
        }
        let mut put = |name: &str, col: AttrColumn| {
            if wants(name) {
                points.attrs.insert(name.to_string(), col);
            }
        };
        put("along", AttrColumn::Float(along.into()));
        put("distance_m", AttrColumn::Float(dist.into()));
        put("key", AttrColumn::Float(keys.into()));
        put("tangent", AttrColumn::Vec3(tangents.into()));
        put("curvature", AttrColumn::Float(curv.into()));
        put("turn_deg", AttrColumn::Float(turns.into()));
        for (name, col) in cols {
            if !crate::data::is_reserved_name(&name) {
                points.attrs.insert(name, col);
            }
        }
        out.0.push(item.with_payload(Payload::Points(points)));
    }
    Ok(vec![out])
}

fn eval_from_points(
    _ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let mode = if s.text("mode") == "Linear" {
        TangentMode::Linear
    } else {
        TangentMode::Auto
    };
    let tangent_attrs = match s.get("tangent_attrs").and_then(SettingValue::as_items) {
        Some([a, b]) => Some((
            a.as_text().unwrap_or_default().to_string(),
            b.as_text().unwrap_or_default().to_string(),
        )),
        _ => None,
    };
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Some(p) = item.as_points() else {
            continue;
        };
        if p.len() > SPLINE_MAX_POINTS {
            return Err(NodeError::new(
                "spline_too_many_points",
                format!("{} points, the limit is {SPLINE_MAX_POINTS}", p.len()),
            ));
        }
        let order: Vec<usize> = if s.text("order") == "nearest_chain" {
            nearest_chain(&p.position)
        } else {
            (0..p.len()).collect()
        };
        // Point attributes that map to spline value types.
        let decls: Vec<(String, SplineAttrType)> = p
            .attrs
            .iter()
            .filter_map(|(k, c)| match c {
                AttrColumn::Float(_) => Some((k.clone(), SplineAttrType::F32)),
                AttrColumn::Vec3(_) => Some((k.clone(), SplineAttrType::Vec3)),
                AttrColumn::Name(_) => Some((k.clone(), SplineAttrType::Name)),
                _ => None,
            })
            .collect();
        let vec3_of = |name: &str, i: usize| match p.attrs.get(name) {
            Some(AttrColumn::Vec3(v)) => v.get(i).map(|x| Vec3::new(x[0] as f32, x[1] as f32, x[2] as f32)),
            _ => None,
        };
        let points = order
            .iter()
            .map(|&i| {
                let values = decls
                    .iter()
                    .map(|(k, ty)| match (p.attrs.get(k).and_then(|c| c.get(i)), ty) {
                        (Some(AttrValue::Float(x)), _) => SplineValue::F32(x as f32),
                        (Some(AttrValue::Vec3(x)), _) => {
                            SplineValue::Vec3(Vec3::new(x[0] as f32, x[1] as f32, x[2] as f32))
                        }
                        (Some(AttrValue::Name(n)), _) => SplineValue::Name(n),
                        (_, ty) => SplineValue::default_of(*ty),
                    })
                    .collect();
                let mut sp = SplinePoint {
                    position: Vec3::from(p.position[i]),
                    mode,
                    scale: Vec2::new(p.scale[i][0], p.scale[i][1]),
                    values,
                    ..SplinePoint::default()
                };
                if let Some((a, b)) = &tangent_attrs
                    && let (Some(ti), Some(to)) = (vec3_of(a, i), vec3_of(b, i))
                {
                    sp.tangent_in = ti;
                    sp.tangent_out = to;
                    sp.mode = TangentMode::Free;
                }
                sp
            })
            .collect();
        let spline = Spline {
            points,
            closed: s.b("closed"),
            attrs: decls
                .into_iter()
                .map(|(name, ty)| SplineAttrDecl { name, ty })
                .collect(),
            ..Spline::default()
        };
        out.0.push(spline_item(item, spline));
    }
    Ok(vec![out])
}

/// Greedy nearest-neighbor order from the first point (ties by index).
fn nearest_chain(pos: &[[f32; 3]]) -> Vec<usize> {
    let n = pos.len();
    let mut used = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut cur = 0usize;
    for _ in 0..n {
        used[cur] = true;
        order.push(cur);
        let c = Vec3::from(pos[cur]);
        let mut best: Option<(f32, usize)> = None;
        for (j, p) in pos.iter().enumerate() {
            if used[j] {
                continue;
            }
            let d = c.distance_squared(Vec3::from(*p));
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, j));
            }
        }
        match best {
            Some((_, j)) => cur = j,
            None => break,
        }
    }
    order
}

/// Ramer-Douglas-Peucker keep flags over positions (`keep` pre-set entries are always kept).
fn rdp(pos: &[Vec3], tol: f32, keep: &mut [bool]) {
    // An explicit stack of spans: a dense polyline would overflow a recursive version.
    fn spans(pos: &[Vec3], a: usize, b: usize, tol: f32, keep: &mut [bool]) {
        let mut stack = vec![(a, b)];
        while let Some((a, b)) = stack.pop() {
            if b <= a + 1 {
                continue;
            }
            let (pa, pb) = (pos[a], pos[b]);
            let ab = pb - pa;
            let l2 = ab.length_squared();
            let mut worst = (0.0f32, a);
            for (i, p) in pos.iter().enumerate().take(b).skip(a + 1) {
                let t = if l2 > 0.0 {
                    ((*p - pa).dot(ab) / l2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let d = (*p - (pa + ab * t)).length();
                if d > worst.0 {
                    worst = (d, i);
                }
            }
            if worst.0 > tol {
                keep[worst.1] = true;
                stack.push((worst.1, b));
                stack.push((a, worst.1));
            }
        }
    }
    let n = pos.len();
    if n < 3 {
        keep.iter_mut().for_each(|k| *k = true);
        return;
    }
    keep[0] = true;
    keep[n - 1] = true;
    let fixed: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    for w in fixed.windows(2) {
        spans(pos, w[0], w[1], tol, keep);
    }
}

fn eval_simplify(_ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let merge = s.f("merge_m") as f32;
    let tol = s.f("tolerance_m") as f32;
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Some(sd) = spline_of(item) else {
            continue;
        };
        let pts = &sd.spline.points;
        let n = pts.len();
        if n < 3 {
            out.0.push(item.clone());
            continue;
        }
        let pinned = |p: &SplinePoint| matches!(p.mode, TangentMode::Free | TangentMode::Aligned);
        // Merge pass.
        let mut kept: Vec<usize> = vec![0];
        for i in 1..n {
            let last = kept[kept.len() - 1];
            let close = pts[i].position.distance(pts[last].position) < merge;
            if !close || pinned(&pts[i]) || i == n - 1 {
                if close && i == n - 1 && kept.len() > 1 && !pinned(&pts[last]) {
                    kept.pop();
                }
                kept.push(i);
            }
        }
        let pos: Vec<Vec3> = kept.iter().map(|&i| pts[i].position).collect();
        let mut keep: Vec<bool> = kept.iter().map(|&i| pinned(&pts[i])).collect();
        rdp(&pos, tol, &mut keep);
        let mut spline = sd.spline.clone();
        spline.points = kept
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(&i, _)| pts[i].clone())
            .collect();
        out.0.push(spline_item(item, spline));
    }
    Ok(vec![out])
}

fn float_list(s: &Settings, name: &str) -> Vec<f64> {
    s.get(name)
        .and_then(SettingValue::as_items)
        .map(|v| v.iter().filter_map(SettingValue::as_f64).collect())
        .unwrap_or_default()
}

fn eval_split(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Some(sd) = spline_of(item) else {
            continue;
        };
        let curve = sd.curve();
        let len = curve.length_f64();
        let starts: Vec<f64> = curve.segment_starts().into_iter().map(f64::from).collect();
        let cuts: Vec<f64> = match s.text("at") {
            "distances" => float_list(s, "distances_m"),
            "points" => starts.iter().skip(1).copied().collect(),
            "attr" => {
                let name = s.text("attr");
                let Some(k) = sd.spline.attr_index(name) else {
                    return Err(NodeError::new(
                        "attr_missing",
                        format!("the spline has no value `{name}`"),
                    ));
                };
                sd.spline
                    .points
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| matches!(p.values.get(k), Some(SplineValue::F32(v)) if *v != 0.0))
                    .filter_map(|(i, _)| starts.get(i).copied())
                    .collect()
            }
            _ => {
                let every = s.f("every_m").max(0.01);
                let n = (len / every).ceil() as usize;
                ctx.check_points(n)?;
                (1..n).map(|k| every * k as f64).collect()
            }
        };
        for (k, piece) in curve.split_at(&cuts).into_iter().enumerate() {
            ctx.check_cancel()?;
            let mut it = spline_item(item, piece);
            it.scope.insert("piece".to_string(), AttrValue::Int(k as i64));
            out.0.push(it);
        }
    }
    Ok(vec![out])
}

/// A dense polyline of a curve with the distance of each vertex.
pub(crate) fn dense(curve: &SplineCurve) -> Vec<(Vec3, f64)> {
    let len = curve.length_f64();
    if !crate::nodes::common::has_length(len) {
        return vec![(curve.at_distance(0.0).position, 0.0)];
    }
    let n = ((len / 1.0).ceil() as usize)
        .clamp(16, 1 << 16)
        .max((len / 0.25).ceil().min(4096.0) as usize);
    (0..=n)
        .map(|k| {
            let d = len * k as f64 / n as f64;
            (curve.at_distance_f64(d).position, d)
        })
        .collect()
}

/// A spline item with its evaluator and dense polyline.
type Traced<'a> = (&'a DataItem, SplineCurve, Vec<(Vec3, f64)>);

/// Proper crossing of XZ segments `p0→p1` and `q0→q1`: the parameters on both.
pub(crate) fn cross_xz(p0: Vec3, p1: Vec3, q0: Vec3, q1: Vec3) -> Option<(f64, f64)> {
    let (ax, az) = (f64::from(p0.x), f64::from(p0.z));
    let (rx, rz) = (f64::from(p1.x) - ax, f64::from(p1.z) - az);
    let (bx, bz) = (f64::from(q0.x), f64::from(q0.z));
    let (sx, sz) = (f64::from(q1.x) - bx, f64::from(q1.z) - bz);
    let den = rx * sz - rz * sx;
    if den.abs() <= 1e-15 {
        return None;
    }
    let (dx, dz) = (bx - ax, bz - az);
    let t = (dx * sz - dz * sx) / den;
    let u = (dx * rz - dz * rx) / den;
    ((0.0..1.0).contains(&t) && (0.0..1.0).contains(&u)).then_some((t, u))
}

fn key_at(curve: &SplineCurve, d: f64) -> f64 {
    let (i, u) = curve.locate_distance(d);
    i as f64 + u
}

fn eval_intersect(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.spline_intersect").entered();
    let three_d = s.text("plane") == "3d";
    let tol = s.f("tolerance_m") as f32;
    let bundle = input(inputs, 0);
    let splines: Vec<Traced<'_>> = bundle
        .0
        .iter()
        .filter_map(|it| spline_of(it).map(|sd| (it, sd.curve())))
        .map(|(it, c)| {
            let d = dense(&c);
            (it, c, d)
        })
        .collect();
    // (a, b, along_a, along_b, position)
    let mut hits: Vec<(usize, usize, f64, f64, Vec3)> = Vec::new();
    for a in 0..splines.len() {
        for b in a..splines.len() {
            ctx.check_cancel()?;
            let (pa, pb) = (&splines[a].2, &splines[b].2);
            for i in 0..pa.len().saturating_sub(1) {
                let j0 = if a == b { i + 2 } else { 0 };
                for j in j0..pb.len().saturating_sub(1) {
                    let (p0, p1, q0, q1) = (pa[i].0, pa[i + 1].0, pb[j].0, pb[j + 1].0);
                    // Box reject.
                    if p0.x.max(p1.x) < q0.x.min(q1.x)
                        || q0.x.max(q1.x) < p0.x.min(p1.x)
                        || p0.z.max(p1.z) < q0.z.min(q1.z)
                        || q0.z.max(q1.z) < p0.z.min(p1.z)
                    {
                        continue;
                    }
                    let Some((t, u)) = cross_xz(p0, p1, q0, q1) else {
                        continue;
                    };
                    let (ha, hb) = (p0.lerp(p1, t as f32), q0.lerp(q1, u as f32));
                    if three_d && (ha.y - hb.y).abs() > tol {
                        continue;
                    }
                    let da = pa[i].1 + (pa[i + 1].1 - pa[i].1) * t;
                    let db = pb[j].1 + (pb[j + 1].1 - pb[j].1) * u;
                    // A closed curve's seam meets itself: not a crossing.
                    if a == b
                        && splines[a].1.is_closed()
                        && (db - da - splines[a].1.length_f64()).abs() < 1e-6
                    {
                        continue;
                    }
                    hits.push((a, b, da, db, (ha + hb) * 0.5));
                }
            }
        }
    }
    hits.sort_by(|x, y| {
        x.0.cmp(&y.0)
            .then(x.1.cmp(&y.1))
            .then(key_at(&splines[x.0].1, x.2).total_cmp(&key_at(&splines[y.0].1, y.2)))
    });
    let mut points = PointSet::new();
    let (mut ca, mut cb, mut ka, mut kb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (a, b, da, db, p) in &hits {
        let position = p.to_array();
        points.push(Point {
            position,
            seed: ctx.point_seed(position),
            ..Point::default()
        });
        ca.push(*a as i64);
        cb.push(*b as i64);
        ka.push(key_at(&splines[*a].1, *da));
        kb.push(key_at(&splines[*b].1, *db));
    }
    points.attrs.insert("a".into(), AttrColumn::Int(ca.into()));
    points.attrs.insert("b".into(), AttrColumn::Int(cb.into()));
    points.attrs.insert("key_a".into(), AttrColumn::Float(ka.into()));
    points.attrs.insert("key_b".into(), AttrColumn::Float(kb.into()));
    let mut out_splines = Bundle::empty();
    for (k, (item, curve, _)) in splines.iter().enumerate() {
        if !s.b("insert") {
            out_splines.0.push((*item).clone());
            continue;
        }
        let cuts: Vec<f64> = hits
            .iter()
            .flat_map(|h| {
                let mut v = Vec::new();
                if h.0 == k {
                    v.push(h.2);
                }
                if h.1 == k {
                    v.push(h.3);
                }
                v
            })
            .collect();
        if cuts.is_empty() {
            out_splines.0.push((*item).clone());
        } else {
            out_splines.0.push(spline_item(item, curve.with_points_at(&cuts)));
        }
    }
    Ok(vec![out_splines, Bundle::one(DataItem::points(points))])
}

/// Removes loops from an open polyline: where segment `i` crosses a later segment `j`, the
/// vertices between are dropped and the crossing inserted.
fn remove_loops(mut pts: Vec<(Vec3, f64)>) -> Vec<(Vec3, f64)> {
    let mut i = 0;
    while i + 1 < pts.len() {
        let mut cut: Option<(usize, Vec3, f64)> = None;
        for j in (i + 2..pts.len().saturating_sub(1)).rev() {
            if let Some((t, _)) = cross_xz(pts[i].0, pts[i + 1].0, pts[j].0, pts[j + 1].0) {
                cut = Some((j, pts[i].0.lerp(pts[i + 1].0, t as f32), t));
                break;
            }
        }
        if let Some((j, x, t)) = cut {
            // The crossing's station on segment `i`, so the attributes copied from it (roll,
            // scale, values) are the crossing's, not the segment start's (audit wave 5).
            let d = pts[i].1 + (pts[i + 1].1 - pts[i].1) * t;
            pts.drain(i + 1..=j);
            pts.insert(i + 1, (x, d));
        }
        i += 1;
    }
    pts
}

fn eval_offset(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let side = s.f("distance_m") as f32;
    let up = s.f("vertical_m") as f32;
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Some(sd) = spline_of(item) else {
            continue;
        };
        let curve = sd.curve();
        if !crate::nodes::common::has_length(f64::from(curve.length())) {
            out.0.push(item.clone());
            continue;
        }
        let moved: Vec<(Vec3, f64)> = dense(&curve)
            .into_iter()
            .map(|(_, d)| {
                let sm = curve.at_distance_f64(d);
                (sm.position + sm.right * side + sm.up * up, d)
            })
            .collect();
        let cleaned = remove_loops(moved);
        let pos: Vec<Vec3> = cleaned.iter().map(|p| p.0).collect();
        let mut keep = vec![false; pos.len()];
        rdp(&pos, 0.02, &mut keep);
        let mut spline = Spline {
            closed: sd.spline.closed,
            up: sd.spline.up,
            attrs: sd.spline.attrs.clone(),
            ..Spline::default()
        };
        for ((p, d), k) in cleaned.iter().zip(&keep) {
            if !*k {
                continue;
            }
            let sm = curve.at_distance_f64(*d);
            spline.points.push(SplinePoint {
                position: *p,
                roll_deg: sm.roll_deg,
                scale: sm.scale,
                values: sm.values,
                ..SplinePoint::default()
            });
        }
        if spline.closed && spline.points.len() > 2 {
            spline.points.pop();
        }
        out.0.push(spline_item(item, spline));
    }
    Ok(vec![out])
}

/// The spline reversed: same curve, opposite direction.
pub(crate) fn reversed(spline: &Spline) -> Spline {
    let mut out = spline.clone();
    out.points.reverse();
    for p in &mut out.points {
        let (ti, to) = match p.mode {
            TangentMode::Aligned => (
                p.tangent_out.normalize_or_zero() * p.tangent_in.length(),
                p.tangent_out,
            ),
            _ => (p.tangent_in, p.tangent_out),
        };
        if matches!(p.mode, TangentMode::Free | TangentMode::Aligned) {
            p.mode = TangentMode::Free;
        }
        p.tangent_in = -to;
        p.tangent_out = -ti;
    }
    out
}

fn eval_reverse(_ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        match spline_of(item) {
            Some(sd) => out.0.push(spline_item(item, reversed(&sd.spline))),
            None => out.0.push(item.clone()),
        }
    }
    Ok(vec![out])
}

fn eval_clip(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let polys_b = input(inputs, 1);
    let polys: Vec<&PolygonData> = polys_b.0.iter().filter_map(polygon_of).collect();
    let (mut inside, mut outside) = (Bundle::empty(), Bundle::empty());
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Some(sd) = spline_of(item) else {
            continue;
        };
        let curve = sd.curve();
        let pts = dense(&curve);
        let mut cuts = Vec::new();
        for poly in &polys {
            for ring in poly.world_rings() {
                let n = ring.len();
                for e in 0..n {
                    let (a, b) = (ring[e], ring[(e + 1) % n]);
                    let (qa, qb) = (
                        Vec3::new(a[0] as f32, 0.0, a[1] as f32),
                        Vec3::new(b[0] as f32, 0.0, b[1] as f32),
                    );
                    for w in pts.windows(2) {
                        if let Some((t, _)) = cross_xz(w[0].0, w[1].0, qa, qb) {
                            cuts.push(w[0].1 + (w[1].1 - w[0].1) * t);
                        }
                    }
                }
            }
        }
        let pieces = if cuts.is_empty() {
            vec![sd.spline.clone()]
        } else {
            curve.split_at(&cuts)
        };
        for piece in pieces {
            let mid = piece.at_param(0.5).position;
            let is_in = polys
                .iter()
                .any(|p| p.contains(f64::from(mid.x), f64::from(mid.z)));
            let it = spline_item(item, piece);
            if is_in {
                inside.0.push(it);
            } else {
                outside.0.push(it);
            }
        }
    }
    Ok(vec![inside, outside])
}

/// Andrew's monotone chain hull of XZ points (counter-clockwise, no collinear points).
pub(crate) fn convex_hull(mut pts: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross =
        |o: [f64; 2], a: [f64; 2], b: [f64; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let mut lower: Vec<[f64; 2]> = Vec::new();
    for p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], *p) <= 0.0 {
            lower.pop();
        }
        lower.push(*p);
    }
    let mut upper: Vec<[f64; 2]> = Vec::new();
    for p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], *p) <= 0.0 {
            upper.pop();
        }
        upper.push(*p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// A polygon from world XZ rings at height `y`, origin at the rings' bounds center.
pub(crate) fn polygon_from_world(rings: &[Vec<[f64; 2]>], y: f64) -> Result<PolygonData, NodeError> {
    let all = rings.iter().flatten();
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for p in all {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    if !lo[0].is_finite() {
        return Ok(PolygonData {
            origin: [0.0, y, 0.0],
            rings: Vec::new(),
        });
    }
    // Snap the origin to the grid so world coordinates stay on it.
    let snap = |v: f64| (v * 0.5 / crate::polygon::GRID_M).round() * crate::polygon::GRID_M;
    let o = [snap(lo[0] + hi[0]), snap(lo[1] + hi[1])];
    let local: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .map(|r| r.iter().map(|p| [p[0] - o[0], p[1] - o[1]]).collect())
        .collect();
    PolygonData::new([o[0], y, o[1]], &local).map_err(poly_err)
}

fn eval_polygon_from(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let (ring, y) = match &item.payload {
            Payload::Points(p) => {
                let pts: Vec<[f64; 2]> = p
                    .position
                    .iter()
                    .map(|q| [f64::from(q[0]), f64::from(q[2])])
                    .collect();
                let y = p.position.iter().map(|q| f64::from(q[1])).sum::<f64>() / p.len().max(1) as f64;
                let ring = if s.text("mode") == "ordered" {
                    pts
                } else {
                    convex_hull(pts)
                };
                (ring, y)
            }
            Payload::Spline(sd) => {
                if !sd.spline.closed {
                    ctx.warn("spline_not_closed", "an open spline makes no polygon");
                    continue;
                }
                let line = sd.curve().to_polyline(s.f("max_error_m") as f32);
                let y = line.iter().map(|p| f64::from(p.y)).sum::<f64>() / line.len().max(1) as f64;
                (line.iter().map(|p| [f64::from(p.x), f64::from(p.z)]).collect(), y)
            }
            _ => continue,
        };
        out.0.push(polygon_item(item, polygon_from_world(&[ring], y)?));
    }
    Ok(vec![out])
}

fn union_of(b: &Bundle) -> Result<Option<(DataItem, PolygonData)>, NodeError> {
    let polys: Vec<&PolygonData> = b.0.iter().filter_map(polygon_of).collect();
    let first = b.0.iter().find(|i| polygon_of(i).is_some()).cloned();
    match (first, PolygonData::union_all(&polys).map_err(poly_err)?) {
        (Some(item), Some(p)) => Ok(Some((item, p))),
        _ => Ok(None),
    }
}

fn eval_bool(_ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = BoolOp::from_name(s.text("op")).unwrap_or(BoolOp::Union);
    let a = union_of(&input(inputs, 0))?;
    let b = union_of(&input(inputs, 1))?;
    let result = match (a, b) {
        (Some((item, pa)), Some((_, pb))) => Some((item, pa.boolean(&pb, op).map_err(poly_err)?)),
        (Some((item, pa)), None) => match op {
            BoolOp::Intersection => Some((
                item.clone(),
                PolygonData {
                    rings: Vec::new(),
                    ..pa
                },
            )),
            _ => Some((item, pa)),
        },
        (None, Some((item, pb))) => match op {
            BoolOp::Union | BoolOp::Xor => Some((item, pb)),
            _ => None,
        },
        (None, None) => None,
    };
    Ok(vec![match result {
        Some((item, p)) => Bundle::one(polygon_item(&item, p)),
        None => Bundle::empty(),
    }])
}

fn eval_poly_offset(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let join = Join::from_name(s.text("join"));
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        if let Some(p) = polygon_of(item) {
            let r = p
                .offset(s.f("distance_m"), join, s.f("miter_limit"))
                .map_err(poly_err)?;
            out.0.push(polygon_item(item, r));
        }
    }
    Ok(vec![out])
}

fn eval_cut(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let cutters_b = input(inputs, 1);
    let lines: Vec<Vec<[f64; 2]>> = cutters_b
        .0
        .iter()
        .filter_map(spline_of)
        .map(|sd| {
            sd.curve()
                .to_polyline(0.05)
                .iter()
                .map(|p| [f64::from(p.x), f64::from(p.z)])
                .collect()
        })
        .collect();
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Some(p) = polygon_of(item) else {
            continue;
        };
        for (k, piece) in p
            .cut(&lines, s.f("gap_m"))
            .map_err(poly_err)?
            .into_iter()
            .enumerate()
        {
            let mut it = polygon_item(item, piece);
            it.scope.insert("piece".to_string(), AttrValue::Int(k as i64));
            out.0.push(it);
        }
    }
    Ok(vec![out])
}

fn eval_to_spline(
    _ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    _s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Some(p) = polygon_of(item) else {
            continue;
        };
        let y = p.origin[1] as f32;
        for ring in p.world_rings() {
            let spline = Spline {
                points: ring
                    .iter()
                    .map(|q| {
                        SplinePoint::at(Vec3::new(q[0] as f32, y, q[1] as f32)).with_mode(TangentMode::Linear)
                    })
                    .collect(),
                closed: true,
                ..Spline::default()
            };
            out.0.push(spline_item(item, spline));
        }
    }
    Ok(vec![out])
}

/// Helper for tests and hosts: a spline item.
pub fn spline_data_item(spline: Spline) -> DataItem {
    DataItem::new(Payload::Spline(Arc::new(SplineData::new(spline))))
}

/// Helper for tests and hosts: a polygon item.
pub fn polygon_data_item(poly: PolygonData) -> DataItem {
    DataItem::new(Payload::Polygon(Arc::new(poly)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_at_a_shared_vertex_is_owned_by_the_next_segment() {
        let v = |x: f32, z: f32| Vec3::new(x, 0.0, z);
        let before = cross_xz(v(0.0, 0.0), v(50.0, 0.0), v(50.0, -10.0), v(50.0, 0.0));
        let after = cross_xz(v(50.0, 0.0), v(100.0, 0.0), v(50.0, 0.0), v(50.0, 10.0));
        assert!(before.is_none());
        assert_eq!(after, Some((0.0, 0.0)));
    }

    #[test]
    fn frame_rotation_maps_axes() {
        let t = Vec3::new(0.0, 0.0, 1.0);
        let u = Vec3::Y;
        let q = frame_rot(t, u);
        let m = q.to_mat3();
        assert!(m.x_axis.abs_diff_eq(t, 1e-5) && m.y_axis.abs_diff_eq(u, 1e-5));
    }

    #[test]
    fn hull_is_counter_clockwise() {
        let h = convex_hull(vec![
            [0.0, 0.0],
            [2.0, 0.0],
            [1.0, 1.0],
            [2.0, 2.0],
            [0.0, 2.0],
            [1.0, 0.0],
        ]);
        assert_eq!(h.len(), 4);
        assert!(crate::polygon::ring_area(&h) > 0.0);
    }

    #[test]
    fn loops_are_removed() {
        let pts = vec![
            (Vec3::new(0.0, 0.0, 0.0), 0.0),
            (Vec3::new(4.0, 0.0, 0.0), 1.0),
            (Vec3::new(4.0, 0.0, 2.0), 2.0),
            (Vec3::new(2.0, 0.0, -2.0), 3.0),
            (Vec3::new(8.0, 0.0, -2.0), 4.0),
        ];
        let out = remove_loops(pts);
        assert_eq!(out.len(), 4, "{out:?}");
        // Audit wave 5 (procgen-g2 M): the crossing (x = 3, 3/4 of the way along the first
        // segment) keeps its own station, 0.75, not the segment start's 0.0.
        assert_eq!(out[1].0, Vec3::new(3.0, 0.0, 0.0));
        assert!((out[1].1 - 0.75).abs() < 1e-9, "{out:?}");
    }
}
