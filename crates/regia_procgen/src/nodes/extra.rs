// SPDX-License-Identifier: MPL-2.0
//! More spec 63 nodes across families: `region.union`, `items.by_attr/partition/bounds`,
//! `flow.match/pick`, `sample.volume`, `points.dedupe/nearest/repeat/enclose`,
//! `attr.pack/unpack/string`, `out.graph`.

use std::collections::BTreeMap;

use regia_core::math::Vec3;

use crate::data::{AttrColumn, AttrValue, Bundle, DataItem, Payload, Point, PointSet, Table, quat};
use crate::exec::{CHUNK, NodeCtx};
use crate::nodes::common::{Combine, Region, read_num, world_bounds, write_num};
use crate::nodes::{
    ANY, BuiltinNode, NodeError, POINTS, PinSpec, SPATIAL, SettingType as T, Settings, TABLE, num, pin_in,
    pin_multi, pin_out, pin_req, pins_points_inout, pins_split, set,
};
use crate::value::SettingValue;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "region.union",
        "region",
        "Merges point sets; where items overlap, each point's density becomes the combined region value of the others",
        &[set(
            "mode",
            T::Enum(&["max", "add", "any"]),
            "max",
            "How the other items' values combine with the point's",
        )],
        |_| PinSpec::new(vec![pin_multi("in", POINTS)], vec![pin_out("out", POINTS)]),
        eval_union,
    ),
    BuiltinNode::new(
        "items.by_attr",
        "items",
        "Items that have the attribute go to `inside`",
        &[
            set("name", T::Name, "\"\"", "Attribute name"),
            set("scope", T::Enum(&["element", "item"]), "element", "Where to look"),
        ],
        |_| pins_split(ANY),
        eval_by_attr,
    ),
    BuiltinNode::new(
        "items.partition",
        "items",
        "Splits each item's elements into one item per distinct key tuple (first-occurrence order)",
        &[set("by", T::NameList, "[]", "Key selectors")],
        crate::nodes::pins_any_inout,
        eval_partition,
    ),
    BuiltinNode::new(
        "items.bounds",
        "items",
        "One table row per item: `min`, `max` (world AABB)",
        &[],
        |_| PinSpec::new(vec![pin_req("in", SPATIAL)], vec![pin_out("out", TABLE)]),
        eval_bounds,
    ),
    BuiltinNode::new(
        "flow.match",
        "flow",
        "Routes the input to the first case equal to the key, else `default`; untaken outputs are inactive",
        &[
            set("key", T::Name, "\"\"", "Item-scope selector (`item.kind`)"),
            set("cases", T::Any, "[]", "Case values (Int or Name)"),
        ],
        match_pins,
        eval_match,
    )
    .checked(check_cases),
    BuiltinNode::new(
        "flow.pick",
        "flow",
        "Forwards the input of the first case equal to the key, else `default`",
        &[
            set("key", T::Any, "0", "Key value (Int or Name)"),
            set("cases", T::Any, "[]", "Case values"),
        ],
        pick_pins,
        eval_pick,
    )
    .checked(check_pick),
    BuiltinNode::new(
        "sample.volume",
        "sample",
        "A 3D grid over the region's AABB; keeps cells whose center has region value > 0 (order y, z, x)",
        &[
            num("cell_m", T::Vec3, "(2.0, 2.0, 2.0)", 0.01, 1.0e6, "Cell size"),
            num(
                "jitter",
                T::Float,
                "0.0",
                0.0,
                1.0,
                "Fraction of a cell each point may move",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("region", SPATIAL)], vec![pin_out("out", POINTS)]),
        eval_volume,
    ),
    BuiltinNode::in_place(
        "points.dedupe",
        "points",
        "Points closer than radius_m collapse to one (the lowest index, or the average position)",
        &[
            num("radius_m", T::Float, "0.1", 0.0, 1.0e6, "Merge radius"),
            set(
                "merge",
                T::Enum(&["first", "average"]),
                "first",
                "Which point survives",
            ),
        ],
        pins_points_inout,
        eval_dedupe,
    ),
    BuiltinNode::in_place(
        "points.nearest",
        "points",
        "Distance (and vector) to the nearest point of `to` (excluding the same index when `to` is the same set)",
        &[
            set("write", T::Name, "\"nearest_dist\"", "Distance attribute"),
            set("write_vec", T::Name, "\"\"", "Vector attribute (empty = none)"),
            num("max_m", T::Float, "1000.0", 0.0, 1.0e9, "Search cap"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_req("to", POINTS)],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_nearest,
    ),
    BuiltinNode::in_place(
        "points.repeat",
        "points",
        "Emits each point followed by `count` copies; with chain, copy k applies the step k times",
        &[
            num("count", T::Int, "3", 0.0, 100_000.0, "Copies per point"),
            set("offset", T::Vec3, "(1.0, 0.0, 0.0)", "Step translation (local)"),
            set("rot_deg", T::Vec3, "(0.0, 0.0, 0.0)", "Step rotation"),
            set("scale", T::Vec3, "(1.0, 1.0, 1.0)", "Step scale"),
            set("chain", T::Bool, "true", "Each copy steps from the previous copy"),
        ],
        pins_points_inout,
        eval_repeat,
    ),
    BuiltinNode::in_place(
        "points.enclose",
        "points",
        "One point per item whose box encloses the item's points (world AABB)",
        &[set(
            "pivot",
            T::Enum(&["center", "min"]),
            "center",
            "Where the point sits",
        )],
        pins_points_inout,
        eval_enclose,
    ),
    BuiltinNode::in_place(
        "attr.pack",
        "attr",
        "Builds a vector attribute from 2-4 scalar selectors",
        &[
            set("parts", T::NameList, "[]", "Scalar selectors"),
            set("target", T::Name, "\"packed\"", "Attribute written"),
        ],
        crate::nodes::pins_any_inout,
        eval_pack,
    ),
    BuiltinNode::in_place(
        "attr.unpack",
        "attr",
        "Splits a vector attribute into `<prefix>_x`, `_y`, ...",
        &[
            set("from", T::Selector, "\"\"", "Vector selector"),
            set("prefix", T::Name, "\"v\"", "Name prefix"),
        ],
        crate::nodes::pins_any_inout,
        eval_unpack,
    ),
    BuiltinNode::in_place(
        "attr.string",
        "attr",
        "Name operations: concat parts, or format a pattern ({0}, {1} ... replaced by the parts' text)",
        &[
            set("op", T::Enum(&["concat", "format"]), "concat", "Operation"),
            set(
                "parts",
                T::Any,
                "[]",
                "Selectors (\"attr\") or literals ({\"lit\": text} as (lit: \"...\"))",
            ),
            set("pattern", T::Name, "\"\"", "Pattern for `format`"),
            set("target", T::Name, "\"label\"", "Attribute written"),
        ],
        pins_points_inout,
        eval_string,
    ),
    BuiltinNode::in_place(
        "points.align",
        "points",
        "Rotates each point so its up axis turns toward a direction by `amount` (normalized blend from the current rotation)",
        &[
            set("to", T::Enum(&["up", "ground_normal", "attr", "toward_point"]), "up", "Target direction"),
            set("attr", T::Selector, "\"normal\"", "Vec3 attribute for `to: attr`"),
            set("target", T::Vec3, "(0.0, 0.0, 0.0)", "World point for `toward_point`"),
            num("amount", T::Float, "1.0", 0.0, 1.0, "0 = unchanged, 1 = fully aligned"),
            set(
                "keep_yaw",
                T::Bool,
                "true",
                "Keep the heading about the new up (shortest turn from the current rotation); false = the shortest turn from no rotation, so the old heading is dropped",
            ),
        ],
        pins_points_inout,
        eval_align,
    )
    .reads_world_data(),
    BuiltinNode::in_place(
        "points.project",
        "points",
        "Moves points onto a target (the kind of the first `onto` item): Ground → Y = height; spline → the nearest curve point; polygon → its plane; points → the top of the highest box under the point. `rotation` aligns up to the ground normal, spline frame or target point; points outside the ground, polygon or boxes are dropped unless keep_outside",
        &[
            set("rotation", T::Bool, "false", "Align up to the target (ground normal, spline up, target point up)"),
            set(
                "keep_outside",
                T::Bool,
                "false",
                "Keep points outside the ground's rectangle, the polygon or every box (unmoved)",
            ),
            set(
                "attributes",
                T::Enum(&["none", "all", "list"]),
                "none",
                "Point targets: copy the target point's attributes onto the projected point (target wins)",
            ),
            set("list", T::NameList, "[]", "Attributes copied for `attributes: list`"),
            set(
                "color",
                T::Enum(&["source", "target", "mul"]),
                "source",
                "Point targets: keep the color, take the target point's, or multiply them",
            ),
            set(
                "tags",
                T::Enum(&["source", "target", "both"]),
                "source",
                "Item tags: keep them, take the `onto` items' tags, or add those",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![
                    pin_req("in", POINTS),
                    pin_req(
                        "onto",
                        crate::data::KindSet::of(&[
                            crate::data::DataKind::Ground,
                            crate::data::DataKind::Spline,
                            crate::data::DataKind::Polygon,
                            crate::data::DataKind::Points,
                        ]),
                    ),
                ],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_project,
    )
    .world(),
    BuiltinNode::new(
        "points.split",
        "points",
        "Cuts each point's box in two along a local axis at a fraction; `low` and `high` outputs with refit boxes",
        &[
            set("axis", T::Enum(&["x", "y", "z"]), "x", "Local axis"),
            num("at", T::Float, "0.5", 0.0, 1.0, "Cut position as a fraction of the box"),
        ],
        |_| PinSpec::new(vec![pin_req("in", POINTS)], vec![pin_out("low", POINTS), pin_out("high", POINTS)]),
        eval_split,
    ),
    BuiltinNode::in_place(
        "points.neighbors",
        "points",
        "Per point, statistics over the other points within radius_m: nb_count, nb_density, nb_center",
        &[num("radius_m", T::Float, "5.0", 0.0, 1.0e6, "Radius")],
        pins_points_inout,
        eval_neighbors,
    )
    .reaching(|s| s.f("radius_m")),
    BuiltinNode::in_place(
        "attr.rotation",
        "attr",
        "Quaternion math: from_euler (degrees, yaw-pitch-roll), look_at (−Z toward a direction), compose, invert",
        &[
            set("op", T::Enum(&["from_euler", "look_at", "compose", "invert"]), "from_euler", "Operation"),
            set("a", T::Selector, "\"\"", "First operand"),
            set("b", T::Selector, "\"rotation\"", "Second operand (compose)"),
            set("target", T::Name, "\"rotation\"", "Attribute written (`rotation` sets the point rotation)"),
        ],
        pins_points_inout,
        eval_rotation,
    ),
    BuiltinNode::new(
        "in.mask",
        "in",
        "One Mask item from a texture's CPU pixels mapped onto an XZ rectangle",
        &[
            set("asset", T::Asset, "\"\"", "Mask texture"),
            set("rect", T::Any, "[-50.0, -50.0, 50.0, 50.0]", "[min_x, min_z, max_x, max_z]"),
            set("channel", T::Enum(&["r", "g", "b", "a", "luma"]), "r", "Channel"),
            set("filter", T::Enum(&["bilinear", "nearest"]), "bilinear", "Sampling"),
        ],
        |_| PinSpec::new(vec![], vec![pin_out("out", crate::nodes::kind(crate::data::DataKind::Mask))]),
        eval_in_mask,
    )
    .uncached(),
    BuiltinNode::new(
        "out.mask",
        "out",
        "Exports a Mask item under a name (ProcOutput::mask)",
        &[set("name", T::Name, "\"mask\"", "Export name")],
        |_| PinSpec::new(vec![pin_req("in", crate::nodes::kind(crate::data::DataKind::Mask))], vec![]),
        |ctx, inputs, s| {
            let data = crate::nodes::input(inputs, 0);
            if let Some(m) = data.0.iter().find_map(|i| match &i.payload {
                Payload::Mask(m) => Some((**m).clone()),
                _ => None,
            }) {
                ctx.output().masks.insert(s.text("name").to_string(), m);
            }
            Ok(vec![])
        },
    )
    .uncached(),
    BuiltinNode::new(
        "in.mesh",
        "in",
        "One Mesh item: a mesh asset's CPU geometry (from the host's mesh library)",
        &[set("asset", T::Asset, "\"\"", "Mesh asset")],
        |_| PinSpec::new(vec![], vec![pin_out("out", crate::nodes::kind(crate::data::DataKind::Mesh))]),
        |ctx, _, s| {
            let path = s.text("asset");
            match ctx.world().mesh(path) {
                Some(m) => Ok(vec![Bundle::one(DataItem::new(Payload::Mesh(m)))]),
                None => {
                    ctx.warn("mesh_missing", format!("mesh `{path}` was not captured"));
                    Ok(vec![Bundle::empty()])
                }
            }
        },
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "sample.mesh_surface",
        "sample",
        "Area-weighted random points on a mesh's triangles (square-root barycentric), placed by each input point; normal-aligned",
        &[
            num("per_m2", T::Float, "1.0", 0.0, 1.0e6, "Points per square meter of surface"),
            num("max_points", T::Int, "100000", 1.0, 1.0e9, "Cap on points per mesh placement"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", crate::data::KindSet::of(&[crate::data::DataKind::Mesh, crate::data::DataKind::Points]))],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_mesh_surface,
    )
    .world()
    .reads_world_data(),
    BuiltinNode::in_place(
        "attr.transform",
        "attr",
        "Transform math on the point transform or Transform attributes: compose, invert, lerp, apply_point, apply_dir",
        &[
            set("op", T::Enum(&["compose", "invert", "lerp", "apply_point", "apply_dir"]), "apply_point", "Operation"),
            set("a", T::Name, "\"\"", "Transform attribute (empty = the point's own transform)"),
            set("b", T::Name, "\"\"", "Second transform (compose, lerp) or Vec3 attribute (apply_*)"),
            num("t", T::Float, "0.5", 0.0, 1.0, "Blend for lerp"),
            set("target", T::Name, "\"out\"", "Attribute written (Transform, or Vec3 for apply_*)"),
        ],
        pins_points_inout,
        eval_transform,
    ),
    BuiltinNode::new(
        "out.graph",
        "out",
        "In a graph used by graph.call: the same as graph.output",
        &[set("pin", T::Name, "\"out\"", "Graph output pin name")],
        |_| PinSpec::new(vec![pin_req("in", ANY)], vec![]),
        |ctx, inputs, s| {
            let data = crate::nodes::input(inputs, 0);
            ctx.output()
                .graph_outputs
                .entry(s.text("pin").to_string())
                .or_default()
                .0
                .extend(data.0);
            Ok(vec![])
        },
    )
    .uncached(),
];

fn eval_union(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let input = crate::nodes::input(inputs, 0);
    let combine = Combine::from_name(s.text("mode"));
    let mut merged = PointSet::new();
    let mut tags = std::collections::BTreeSet::new();
    for (k, item) in input.0.iter().enumerate() {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let others = Bundle(
            input
                .0
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != k)
                .map(|(_, x)| x.clone())
                .collect(),
        );
        let region = Region::new(&others, combine)?;
        let mut q = p.clone();
        for (d, pos) in q.density.iter_mut().zip(&q.position) {
            let r = region.value(*pos);
            if r > 0.0 {
                *d = match combine {
                    Combine::Add => (*d + r).min(1.0),
                    Combine::Any => 1.0,
                    Combine::Max => d.max(r),
                };
            }
        }
        merged
            .append(&q)
            .map_err(|n| NodeError::new("attr_type_conflict", format!("attribute `{n}` has two types")))?;
        tags.extend(item.tags.iter().cloned());
    }
    let mut out = DataItem::points(merged);
    out.tags = tags;
    if let Some(first) = input.0.first() {
        out.scope = first.scope.clone();
    }
    Ok(vec![Bundle::one(out)])
}

fn eval_by_attr(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let name = s.text("name").to_string();
    let item_scope = s.text("scope") == "item";
    let (mut a, mut b) = (Bundle::empty(), Bundle::empty());
    for item in crate::nodes::input(inputs, 0).0 {
        let has = if item_scope {
            item.scope.contains_key(&name)
        } else {
            match &item.payload {
                Payload::Points(p) => p.attrs.contains_key(&name),
                Payload::Table(t) => t.column(&name).is_some(),
                _ => false,
            }
        };
        if has { a.0.push(item) } else { b.0.push(item) }
    }
    Ok(vec![a, b])
}

fn key_of(cols: &[&AttrColumn], i: usize) -> Vec<String> {
    cols.iter()
        .map(|c| c.get(i).map(|v| v.key_text()).unwrap_or_default())
        .collect()
}

fn eval_partition(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let by = s.names("by");
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let cols: Vec<&AttrColumn> = match &item.payload {
            Payload::Points(p) => by
                .iter()
                .map(|n| {
                    p.attrs
                        .get(n)
                        .ok_or_else(|| NodeError::new("attr_missing", format!("no attribute `{n}`")))
                })
                .collect::<Result<_, _>>()?,
            Payload::Table(t) => by
                .iter()
                .map(|n| {
                    t.column(n)
                        .ok_or_else(|| NodeError::new("attr_missing", format!("no column `{n}`")))
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(NodeError::new(
                    "not_spatial",
                    "items.partition needs points or a table",
                ));
            }
        };
        let mut groups: Vec<(Vec<String>, Vec<usize>)> = Vec::new();
        for i in 0..item.payload.len() {
            let k = key_of(&cols, i);
            match groups.iter_mut().find(|(g, _)| *g == k) {
                Some((_, v)) => v.push(i),
                None => groups.push((k, vec![i])),
            }
        }
        for (key, idx) in groups {
            let payload = match &item.payload {
                Payload::Points(p) => Payload::Points(p.select(&idx)),
                Payload::Table(t) => Payload::Table(t.select(&idx)),
                other => other.clone(),
            };
            let mut it = item.with_payload(payload);
            for (n, v) in by.iter().zip(key) {
                it.scope.insert(n.clone(), AttrValue::Name(v));
            }
            out.0.push(it);
        }
    }
    Ok(vec![out])
}

fn eval_bounds(_: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut t = Table::default();
    let (mut mins, mut maxs) = (Vec::new(), Vec::new());
    for item in crate::nodes::input_items(inputs, 0) {
        let (lo, hi) = world_bounds(item).unwrap_or(([0.0; 3], [0.0; 3]));
        mins.push(lo.map(f64::from));
        maxs.push(hi.map(f64::from));
    }
    t.rows = mins.len();
    t.set_column("min", AttrColumn::Vec3(mins.into()));
    t.set_column("max", AttrColumn::Vec3(maxs.into()));
    Ok(vec![Bundle::one(DataItem::new(Payload::Table(t)))])
}

/// A case or key value that can match: an Int (or integral float) or a Name.
fn is_case_value(v: &SettingValue) -> bool {
    v.as_text().is_some() || v.as_i64().is_some()
}

/// `cases` holds Int or Name values: a float or a record never matches, so it used to route
/// everything to `default` silently (audit L8).
fn check_cases(s: &Settings) -> Vec<(String, String)> {
    match s.get("cases") {
        None => Vec::new(),
        Some(v) => match v.as_items() {
            Some(items) => items
                .iter()
                .find(|c| !is_case_value(c))
                .map(|c| {
                    vec![(
                        "setting_type".to_string(),
                        format!("case `{}` is not an Int or a Name", c.write()),
                    )]
                })
                .unwrap_or_default(),
            None => vec![("setting_type".to_string(), "`cases` must be a list".to_string())],
        },
    }
}

/// `flow.pick`: the cases as `flow.match`, and the key one such value.
fn check_pick(s: &Settings) -> Vec<(String, String)> {
    let mut e = check_cases(s);
    if let Some(k) = s.get("key")
        && !is_case_value(k)
    {
        e.push((
            "setting_type".to_string(),
            format!("key `{}` is not an Int or a Name", k.write()),
        ));
    }
    e
}

fn case_texts(s: &Settings) -> Vec<String> {
    s.get("cases")
        .and_then(SettingValue::as_items)
        .map(|v| {
            v.iter()
                .map(|c| {
                    c.as_text()
                        .map(str::to_string)
                        .or_else(|| c.as_i64().map(|i| i.to_string()))
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn match_pins(s: &Settings) -> PinSpec {
    let mut outs: Vec<_> = (0..case_texts(s).len())
        .map(|k| pin_out(&format!("case_{k}"), ANY))
        .collect();
    outs.push(pin_out("default", ANY));
    PinSpec::new(vec![pin_req("in", ANY)], outs)
}

fn pick_pins(s: &Settings) -> PinSpec {
    let mut ins: Vec<_> = (0..case_texts(s).len())
        .map(|k| pin_in(&format!("case_{k}"), ANY))
        .collect();
    ins.push(pin_in("default", ANY));
    PinSpec::new(ins, vec![pin_out("out", ANY)])
}

fn eval_match(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let cases = case_texts(s);
    let data = crate::nodes::input(inputs, 0);
    let key_name = s
        .text("key")
        .strip_prefix("item.")
        .unwrap_or(s.text("key"))
        .to_string();
    let key = data
        .0
        .first()
        .and_then(|i| i.scope.get(&key_name))
        .map(AttrValue::key_text)
        .unwrap_or_default();
    let hit = cases.iter().position(|c| *c == key).unwrap_or(cases.len());
    let mut outs = vec![Bundle::empty(); cases.len() + 1];
    for k in 0..outs.len() {
        if k != hit {
            ctx.set_inactive(k);
        }
    }
    outs[hit] = data;
    Ok(outs)
}

fn eval_pick(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let cases = case_texts(s);
    let key = s
        .get("key")
        .map(|v| {
            v.as_text()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|i| i.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let hit = cases.iter().position(|c| *c == key).unwrap_or(cases.len());
    Ok(vec![crate::nodes::input(inputs, hit)])
}

fn eval_volume(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let cell = s.v3("cell_m");
    let jitter = s.f("jitter");
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Some((lo, hi)) = world_bounds(item) else {
            out.0.push(item.with_payload(Payload::Points(PointSet::new())));
            continue;
        };
        let range = |k: usize| -> (i64, i64) {
            (
                (f64::from(lo[k]) / cell[k]).floor() as i64,
                ((f64::from(hi[k]) / cell[k]).ceil() as i64).saturating_sub(1),
            )
        };
        let (rx, ry, rz) = (range(0), range(1), range(2));
        let count = [rx, ry, rz].iter().fold(1u128, |n, (a, b)| {
            n.saturating_mul(crate::nodes::sample::cell_span(*a, *b))
        });
        ctx.check_points(usize::try_from(count).unwrap_or(usize::MAX))?;
        let one = Bundle::one(item.clone());
        let region = Region::new(&one, Combine::Max)?;
        let mut p = PointSet::new();
        let mut n = 0usize;
        for iy in ry.0..=ry.1 {
            for iz in rz.0..=rz.1 {
                for ix in rx.0..=rx.1 {
                    n += 1;
                    if n.is_multiple_of(CHUNK) {
                        ctx.check_cancel()?;
                    }
                    let c = [
                        ((ix as f64 + 0.5) * cell[0]) as f32,
                        ((iy as f64 + 0.5) * cell[1]) as f32,
                        ((iz as f64 + 0.5) * cell[2]) as f32,
                    ];
                    let seed = ctx.point_seed(c);
                    let pos: [f32; 3] = std::array::from_fn(|k| {
                        c[k] + ((ctx.draw(seed, k as u32) - 0.5) * jitter * cell[k]) as f32
                    });
                    let d = region.value(c);
                    if d <= 0.0 {
                        continue;
                    }
                    p.push(Point {
                        position: pos,
                        density: d,
                        seed: ctx.point_seed(pos),
                        bounds_min: cell.map(|v| (-v * 0.5) as f32),
                        bounds_max: cell.map(|v| (v * 0.5) as f32),
                        ..Point::default()
                    });
                }
            }
        }
        out.0.push(item.with_payload(Payload::Points(p)));
    }
    Ok(vec![out])
}

/// A uniform XZ-and-Y grid over positions for radius queries.
struct PosGrid {
    cell: f32,
    cells: BTreeMap<(i64, i64, i64), Vec<usize>>,
}

impl PosGrid {
    fn new(points: &[[f32; 3]], cell: f32) -> Self {
        let cell = cell.max(1e-4);
        let mut cells: BTreeMap<(i64, i64, i64), Vec<usize>> = BTreeMap::new();
        for (i, p) in points.iter().enumerate() {
            cells.entry(Self::key(p, cell)).or_default().push(i);
        }
        Self { cell, cells }
    }
    fn key(p: &[f32; 3], cell: f32) -> (i64, i64, i64) {
        let k = |v: f32| (v / cell).floor() as i64;
        (k(p[0]), k(p[1]), k(p[2]))
    }
    /// Indices in the 27 cells around `p`, ascending.
    fn near(&self, p: &[f32; 3]) -> Vec<usize> {
        let (x, y, z) = Self::key(p, self.cell);
        let mut v = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(c) = self.cells.get(&(x + dx, y + dy, z + dz)) {
                        v.extend_from_slice(c);
                    }
                }
            }
        }
        v.sort_unstable();
        v
    }
}

fn dist2(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    (0..3).map(|k| (a[k] - b[k]) * (a[k] - b[k])).sum()
}

fn eval_dedupe(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let r = s.f("radius_m") as f32;
    let average = s.text("merge") == "average";
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let grid = PosGrid::new(&p.position, r);
        let mut owner: Vec<Option<usize>> = vec![None; p.len()];
        let mut keep = vec![false; p.len()];
        let mut q = p.clone();
        for i in 0..p.len() {
            if owner[i].is_some() {
                continue;
            }
            keep[i] = true;
            owner[i] = Some(i);
            let mut sum = Vec3::from(p.position[i]);
            let mut n = 1.0f32;
            for j in grid.near(&p.position[i]) {
                if j > i && owner[j].is_none() && dist2(&p.position[i], &p.position[j]) < r * r {
                    owner[j] = Some(i);
                    sum += Vec3::from(p.position[j]);
                    n += 1.0;
                }
            }
            if average {
                q.position[i] = (sum / n).to_array();
            }
        }
        item.payload = Payload::Points(q.partition(&keep).0);
    }
    Ok(vec![out])
}

/// Distance tests `points.nearest` may run per node, as a multiple of `points_per_item` (4 Gi on
/// desktop); past it the node fails with `limit_exceeded` instead of running for hours.
const NEAREST_TESTS_PER_POINT_CAP: u64 = 1024;

fn eval_nearest(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let (write, write_vec) = (s.text("write").to_string(), s.text("write_vec").to_string());
    let max_m = s.f("max_m") as f32;
    let to = crate::nodes::input(inputs, 1);
    let mut targets: Vec<[f32; 3]> = Vec::new();
    for item in &to.0 {
        if let Payload::Points(p) = &item.payload {
            targets.extend_from_slice(&p.position);
        }
    }
    // Only targets within `max_m` can win, so a grid of `max_m` cells (plus a margin for rounding)
    // holds every candidate in the 27 cells around a point. Candidates come in ascending index
    // order and the first strictly nearer one wins, as in a full scan.
    let grid = PosGrid::new(&targets, max_m * 1.001);
    let cap = (ctx.limits().points_per_item as u64).saturating_mul(NEAREST_TESTS_PER_POINT_CAP);
    let mut tests = 0u64;
    let mut since_cancel = 0u64;
    let mut out = crate::nodes::take_input(inputs, 0);
    let same = out.0.len() == 1 && to.0.len() == 1 && out.0[0] == to.0[0];
    for item in &mut out.0 {
        let Some(p) = item.as_points().cloned() else {
            continue;
        };
        let (mut dists, mut vecs) = (Vec::with_capacity(p.len()), Vec::with_capacity(p.len() * 3));
        for (i, a) in p.position.iter().enumerate() {
            let near = grid.near(a);
            tests = tests.saturating_add(near.len() as u64);
            since_cancel = since_cancel.saturating_add(near.len() as u64 + 1);
            if tests > cap {
                return Err(NodeError::new(
                    "limit_exceeded",
                    format!(
                        "points_per_item: points.nearest needs more than {cap} distance tests \
                         ({NEAREST_TESTS_PER_POINT_CAP} per allowed point); lower `max_m` or the counts"
                    ),
                ));
            }
            if since_cancel >= CHUNK as u64 {
                since_cancel = 0;
                ctx.check_cancel()?;
            }
            let mut best: Option<(f32, usize)> = None;
            for j in near {
                if same && i == j {
                    continue;
                }
                let d = dist2(a, &targets[j]);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, j));
                }
            }
            match best.filter(|(d, _)| d.sqrt() <= max_m) {
                Some((d, j)) => {
                    dists.push(f64::from(d.sqrt()));
                    vecs.extend((0..3).map(|k| f64::from(targets[j][k] - a[k])));
                }
                None => {
                    dists.push(f64::from(max_m));
                    vecs.extend([0.0; 3]);
                }
            }
        }
        write_num(item, &write, 1, &dists)?;
        if !write_vec.is_empty() {
            write_num(item, &write_vec, 3, &vecs)?;
        }
    }
    Ok(vec![out])
}

fn eval_repeat(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let count = s.i("count").max(0) as usize;
    let off = Vec3::from(s.v3("offset").map(|v| v as f32));
    let step_r = crate::nodes::common::euler_deg(s.v3("rot_deg"));
    let step_s = Vec3::from(s.v3("scale").map(|v| v as f32));
    // `chain: false` applies the step k times in the original frame (spec 63): copy k sits at
    // `k` steps of the offset in the base point's frame, rotated `k` times and scaled `k` times.
    let chain = s.b("chain");
    let mut out = crate::nodes::take_input(inputs, 0);
    let mut emitted = 0usize;
    for item in &mut out.0 {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        ctx.check_points(p.len().saturating_mul(count + 1))?;
        let mut idx = Vec::with_capacity(p.len() * (count + 1));
        let mut points = Vec::with_capacity(p.len() * (count + 1));
        for i in 0..p.len() {
            let base = p.point(i);
            idx.push(i);
            points.push(base);
            let mut prev = base;
            let r0 = quat(base.rotation);
            let (mut acc_r, mut acc_s) = (r0, Vec3::from(base.scale));
            for k in 1..=count {
                emitted += 1;
                if emitted.is_multiple_of(CHUNK) {
                    ctx.check_cancel()?;
                }
                let mut np = prev;
                if chain {
                    let r = quat(np.rotation);
                    np.position = (Vec3::from(np.position) + r * (Vec3::from(np.scale) * off)).to_array();
                    np.rotation = (r * step_r).normalize().to_array();
                    np.scale = (Vec3::from(np.scale) * step_s).to_array();
                } else {
                    np.position = (Vec3::from(base.position)
                        + r0 * (Vec3::from(base.scale) * off * k as f32))
                        .to_array();
                    acc_r = (acc_r * step_r).normalize();
                    acc_s *= step_s;
                    np.rotation = acc_r.to_array();
                    np.scale = acc_s.to_array();
                }
                np.seed = regia_core::math::hash::mix(base.seed, k as u32);
                idx.push(i);
                points.push(np);
                prev = np;
            }
        }
        let mut q = p.select(&idx);
        let mut writer = q.writer();
        for (k, pt) in points.into_iter().enumerate() {
            writer.set_point(k, pt);
        }
        item.payload = Payload::Points(q);
    }
    Ok(vec![out])
}

fn eval_enclose(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let at_min = s.text("pivot") == "min";
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Some((lo, hi)) = world_bounds(item) else {
            continue;
        };
        let pos = if at_min {
            lo
        } else {
            std::array::from_fn(|k| (lo[k] + hi[k]) * 0.5)
        };
        let mut p = PointSet::new();
        p.push(Point {
            position: pos,
            bounds_min: std::array::from_fn(|k| lo[k] - pos[k]),
            bounds_max: std::array::from_fn(|k| hi[k] - pos[k]),
            seed: ctx.point_seed(pos),
            ..Point::default()
        });
        item.payload = Payload::Points(p);
    }
    Ok(vec![out])
}

fn eval_pack(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let parts = s.names("parts");
    if !(2..=4).contains(&parts.len()) {
        return Err(NodeError::new(
            "setting_out_of_range",
            "attr.pack takes 2 to 4 parts",
        ));
    }
    let target = s.text("target").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let cols = parts
            .iter()
            .map(|n| {
                let c = read_num(
                    item,
                    &crate::data::Selector::parse(n).map_err(|m| NodeError::new("setting_type", m))?,
                )?;
                if c.width == 1 {
                    Ok(c)
                } else {
                    Err(NodeError::new(
                        "attr_type_mismatch",
                        format!("`{n}` is not a scalar"),
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let n = item.payload.len();
        let values: Vec<f64> = (0..n)
            .flat_map(|i| cols.iter().map(move |c| c.scalar(i)))
            .collect();
        write_num(item, &target, parts.len(), &values)?;
    }
    Ok(vec![out])
}

fn eval_unpack(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let sel = s.sel("from");
    let prefix = s.text("prefix").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let v = read_num(item, &sel)?;
        for (k, letter) in ["x", "y", "z", "w"].iter().enumerate().take(v.width) {
            let comp: Vec<f64> = (0..v.len()).map(|i| v.get(i)[k]).collect();
            write_num(item, &format!("{prefix}_{letter}"), 1, &comp)?;
        }
    }
    Ok(vec![out])
}

/// Text of a number for `attr.string` (up to 6 decimals, trailing zeros trimmed).
pub(crate) fn num_text(x: f64) -> String {
    let s = format!("{x:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

fn eval_string(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let format = s.text("op") == "format";
    let pattern = s.text("pattern").to_string();
    let target = s.text("target").to_string();
    let parts: Vec<SettingValue> = s
        .get("parts")
        .and_then(SettingValue::as_items)
        .map(<[SettingValue]>::to_vec)
        .unwrap_or_default();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let n = item.payload.len();
        // Per part: one text per element.
        let mut texts: Vec<Vec<String>> = Vec::new();
        for part in &parts {
            if let Some(lit) = part.field("lit").and_then(SettingValue::as_text) {
                texts.push(vec![lit.to_string(); n]);
                continue;
            }
            let name = part.as_text().unwrap_or("");
            let col = match &item.payload {
                Payload::Points(p) => p.attrs.get(name).cloned(),
                Payload::Table(t) => t.column(name).cloned(),
                _ => None,
            };
            let col_texts: Vec<String> = match col {
                Some(AttrColumn::Name(v) | AttrColumn::Asset(v)) => v.into_vec(),
                _ => {
                    let c = read_num(
                        item,
                        &crate::data::Selector::parse(name).map_err(|m| NodeError::new("setting_type", m))?,
                    )?;
                    (0..c.len())
                        .map(|i| {
                            c.get(i)
                                .iter()
                                .map(|x| num_text(*x))
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .collect()
                }
            };
            texts.push(col_texts);
        }
        let result: Vec<String> = (0..n)
            .map(|i| {
                if format {
                    let mut r = pattern.clone();
                    for (k, t) in texts.iter().enumerate() {
                        r = r.replace(&format!("{{{k}}}"), &t[i]);
                    }
                    r
                } else {
                    texts.iter().map(|t| t[i].as_str()).collect()
                }
            })
            .collect();
        if let Payload::Points(p) = &mut item.payload {
            if crate::data::is_reserved_name(&target) {
                return Err(NodeError::new(
                    "attr_reserved_name",
                    format!("`{target}` is reserved"),
                ));
            }
            p.attrs.insert(target.clone(), AttrColumn::Name(result.into()));
            item.last_written = Some(target.clone());
        }
    }
    Ok(vec![out])
}

fn eval_align(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    use crate::nodes::common::rotation_between;
    let to = s.text("to").to_string();
    let amount = s.f("amount") as f32;
    let target = Vec3::from(s.v3("target").map(|v| v as f32));
    let sel = s.sel("attr");
    let keep_yaw = s.b("keep_yaw");
    let ground = ctx.world().ground();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let attr = if to == "attr" {
            Some(read_num(item, &sel)?)
        } else {
            None
        };
        if attr.as_ref().is_some_and(|a| a.width != 3) {
            return Err(NodeError::new("attr_type_mismatch", "`attr` must be a Vec3"));
        }
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        for i in 0..p.len() {
            let pos = Vec3::from(p.position[i]);
            let dir = match to.as_str() {
                "attr" => attr.as_ref().map_or(Vec3::Y, |a| {
                    let v = a.get(i);
                    Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32)
                }),
                "ground_normal" => ground
                    .as_ref()
                    .map_or(Vec3::Y, |g| Vec3::from(g.normal(pos.x, pos.z))),
                "toward_point" => target - pos,
                _ => Vec3::Y,
            };
            let len = dir.length();
            if len.is_nan() || len <= 1e-9 {
                continue;
            }
            let cur = quat(p.rotation[i]);
            let full = if keep_yaw {
                rotation_between(cur * Vec3::Y, dir / len) * cur
            } else {
                rotation_between(Vec3::Y, dir / len)
            };
            // Normalized linear blend (shortest arc), fixed order: deterministic and close to slerp.
            let (a, b) = (cur.to_array(), full.to_array());
            let dot: f32 = (0..4).map(|k| a[k] * b[k]).sum();
            let sign = if dot < 0.0 { -1.0 } else { 1.0 };
            let blended = crate::rot::Rot(std::array::from_fn(|k| {
                a[k] * (1.0 - amount) + sign * b[k] * amount
            }));
            p.rotation[i] = blended.normalize().to_array();
        }
    }
    Ok(vec![out])
}

fn eval_project(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    use crate::nodes::common::rotation_between;
    let (align, keep_outside) = (s.b("rotation"), s.b("keep_outside"));
    let targets = crate::nodes::input_items(inputs, 1);
    let first = targets.first().map(|i| i.payload.kind());
    let target_tags: std::collections::BTreeSet<String> =
        targets.iter().flat_map(|i| i.tags.iter().cloned()).collect();
    let tags_mode = s.text("tags").to_string();
    let retag = |out: &mut Bundle| {
        for item in &mut out.0 {
            match tags_mode.as_str() {
                "target" => item.tags = target_tags.clone(),
                "both" => item.tags.extend(target_tags.iter().cloned()),
                _ => {}
            }
        }
    };
    let opts = ProjectOpts {
        align,
        keep_outside,
        attributes: match s.text("attributes") {
            "all" => Some(None),
            "list" => Some(Some(s.names("list"))),
            _ => None,
        },
        color: s.text("color").to_string(),
    };
    if let Some(
        kind @ (crate::data::DataKind::Spline
        | crate::data::DataKind::Polygon
        | crate::data::DataKind::Points),
    ) = first
    {
        let target = ProjectTarget::gather(kind, targets);
        let mut out = crate::nodes::take_input(inputs, 0);
        project_onto(ctx, &mut out, &target, &opts)?;
        retag(&mut out);
        return Ok(vec![out]);
    }
    let ground = targets.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let mut out = crate::nodes::take_input(inputs, 0);
    let Some(g) = ground else {
        return Ok(vec![out]);
    };
    retag(&mut out);
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        let r = g.rect;
        let mut keep = vec![true; p.len()];
        let position = p.position.make_mut();
        // Only an aligning run writes rotations: the column stays shared otherwise.
        let mut rotation = align.then(|| p.rotation.make_mut());
        for (i, k) in keep.iter_mut().enumerate() {
            let [x, _, z] = position[i];
            let inside = x >= r[0] && x <= r[2] && z >= r[1] && z <= r[3];
            if !inside && !keep_outside {
                *k = false;
                continue;
            }
            position[i][1] = g.height(x, z);
            if let Some(rotation) = &mut rotation {
                let n = Vec3::from(g.normal(x, z));
                rotation[i] = (rotation_between(Vec3::Y, n) * quat(rotation[i]))
                    .normalize()
                    .to_array();
            }
        }
        *p = p.partition(&keep).0;
    }
    Ok(vec![out])
}

/// A target point's world AABB (min, max) and rotation.
type TargetBox = ([f32; 3], [f32; 3], [f32; 4]);

/// The non-Ground targets of `points.project` (every `onto` item of the first item's kind).
enum ProjectTarget {
    /// Spline curves, in input order.
    Splines(Vec<regia_scene::spline::SplineCurve>),
    /// Polygons, in input order.
    Polygons(Vec<std::sync::Arc<crate::polygon::PolygonData>>),
    /// Target point boxes in input order: world AABB and rotation, the `(set, point)` each came
    /// from, and the target point sets (for attributes and color).
    Boxes {
        boxes: Vec<TargetBox>,
        from: Vec<(usize, usize)>,
        sets: Vec<PointSet>,
    },
}

impl ProjectTarget {
    fn gather(kind: crate::data::DataKind, items: &[DataItem]) -> Self {
        match kind {
            crate::data::DataKind::Spline => Self::Splines(
                items
                    .iter()
                    .filter_map(|i| match &i.payload {
                        Payload::Spline(sp) => Some(sp.curve()),
                        _ => None,
                    })
                    .collect(),
            ),
            crate::data::DataKind::Polygon => Self::Polygons(
                items
                    .iter()
                    .filter_map(|i| match &i.payload {
                        Payload::Polygon(pg) => Some(pg.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => {
                let sets: Vec<PointSet> = items.iter().filter_map(|i| i.as_points().cloned()).collect();
                let (mut boxes, mut from) = (Vec::new(), Vec::new());
                for (si, p) in sets.iter().enumerate() {
                    for k in 0..p.len() {
                        let (lo, hi) = p.world_aabb(k);
                        boxes.push((lo, hi, p.rotation[k]));
                        from.push((si, k));
                    }
                }
                Self::Boxes { boxes, from, sets }
            }
        }
    }
}

/// An XZ bucket grid over target boxes (each box in every cell it overlaps).
struct BoxGrid {
    cell: f32,
    origin: [f32; 2],
    dims: [usize; 2],
    cells: Vec<Vec<u32>>,
}

impl BoxGrid {
    /// The most entries, and the most cells, a grid over `boxes` boxes may hold.
    const fn budget(boxes: usize) -> usize {
        16 * boxes + 65_536
    }

    /// Cells sized by the mean box footprint, coarsened until the grid holds at most
    /// [`BoxGrid::budget`] entries and at most as many cells (the answer does not depend on the
    /// grid). The cell bound keeps the grid content-sized: a few small boxes far apart used to get
    /// a span-sized grid (up to 4097 × 4097) of empty cells.
    fn new(boxes: &[TargetBox]) -> Self {
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        let mut mean = 0.0f64;
        for (a, b, _) in boxes {
            lo = [lo[0].min(a[0]), lo[1].min(a[2])];
            hi = [hi[0].max(b[0]), hi[1].max(b[2])];
            mean += f64::from((b[0] - a[0]).max(b[2] - a[2]));
        }
        if boxes.is_empty() || !lo.iter().chain(&hi).all(|v| v.is_finite()) {
            return Self {
                cell: 1.0,
                origin: [0.0; 2],
                dims: [0, 0],
                cells: Vec::new(),
            };
        }
        let span = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1e-3);
        let mut cell = ((mean / boxes.len() as f64) as f32).max(span / 4096.0).max(1e-3);
        let budget = Self::budget(boxes.len());
        loop {
            let dims = [
                ((hi[0] - lo[0]) / cell) as usize + 1,
                ((hi[1] - lo[1]) / cell) as usize + 1,
            ];
            let cells = dims[0].saturating_mul(dims[1]);
            // Coarsen on the cell count first: the entry count is not needed while it is over.
            let fits = cells <= budget
                && boxes
                    .iter()
                    .map(|(a, b, _)| {
                        let c0 = Self::range(a[0], b[0], lo[0], cell, dims[0]);
                        let c1 = Self::range(a[2], b[2], lo[1], cell, dims[1]);
                        (c0.1 - c0.0 + 1) * (c1.1 - c1.0 + 1)
                    })
                    .sum::<usize>()
                    <= budget;
            if fits || cells <= 1 {
                let mut cells = vec![Vec::new(); dims[0] * dims[1]];
                for (k, (a, b, _)) in boxes.iter().enumerate() {
                    let c0 = Self::range(a[0], b[0], lo[0], cell, dims[0]);
                    let c1 = Self::range(a[2], b[2], lo[1], cell, dims[1]);
                    for z in c1.0..=c1.1 {
                        for x in c0.0..=c0.1 {
                            cells[z * dims[0] + x].push(k as u32);
                        }
                    }
                }
                return Self {
                    cell,
                    origin: lo,
                    dims,
                    cells,
                };
            }
            cell *= 2.0;
        }
    }

    fn range(a: f32, b: f32, lo: f32, cell: f32, n: usize) -> (usize, usize) {
        let i = |v: f32| (((v - lo) / cell).max(0.0) as usize).min(n.saturating_sub(1));
        (i(a), i(b))
    }

    /// Box indices whose cell holds `(x, z)` (ascending).
    fn near(&self, x: f32, z: f32) -> &[u32] {
        if self.dims[0] == 0 {
            return &[];
        }
        let fx = (x - self.origin[0]) / self.cell;
        let fz = (z - self.origin[1]) / self.cell;
        if fx.is_nan() || fz.is_nan() || fx < 0.0 || fz < 0.0 {
            return &[];
        }
        let (cx, cz) = (fx as usize, fz as usize);
        if cx >= self.dims[0] || cz >= self.dims[1] {
            return &[];
        }
        &self.cells[cz * self.dims[0] + cx]
    }
}

/// `points.project` onto splines, polygons or point boxes (spec 63 § 6, ADR-3027).
/// `points.project` settings for non-Ground targets.
struct ProjectOpts {
    align: bool,
    keep_outside: bool,
    /// `None` = no attributes; `Some(None)` = all; `Some(Some(list))` = the listed ones.
    attributes: Option<Option<Vec<String>>>,
    /// `source`, `target` or `mul`.
    color: String,
}

/// Copies color and attributes from each point's target point (`hits`: index into `from`).
fn take_target_values(
    p: &mut PointSet,
    hits: &[Option<usize>],
    from: &[(usize, usize)],
    sets: &[PointSet],
    opts: &ProjectOpts,
) {
    let source = |i: usize| hits[i].map(|j| from[j]);
    if opts.color != "source" {
        let mul = opts.color == "mul";
        let color = p.color.make_mut();
        for (i, c) in color.iter_mut().enumerate() {
            if let Some((si, k)) = source(i) {
                let t = sets[si].color[k];
                *c = if mul {
                    std::array::from_fn(|n| c[n] * t[n])
                } else {
                    t
                };
            }
        }
    }
    let Some(which) = &opts.attributes else {
        return;
    };
    let names: Vec<String> = match which {
        Some(list) => list.clone(),
        None => sets
            .iter()
            .flat_map(|t| t.attrs.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    for name in names {
        let Some(ty) = sets.iter().find_map(|t| t.attrs.get(&name)).map(AttrColumn::ty) else {
            continue;
        };
        let existing = p.attrs.get(&name).filter(|c| c.ty() == ty).cloned();
        let mut col = AttrColumn::new(ty);
        for i in 0..p.len() {
            let v = source(i)
                .and_then(|(si, k)| sets[si].attrs.get(&name).and_then(|c| c.get(k)))
                .filter(|v| v.ty() == ty)
                .or_else(|| existing.as_ref().and_then(|c| c.get(i)))
                .or_else(|| AttrColumn::filled(ty, 1).get(0));
            if let Some(v) = v {
                col.push(v);
            }
        }
        p.attrs.insert(name, col);
    }
}

fn project_onto(
    ctx: &NodeCtx<'_>,
    out: &mut Bundle,
    target: &ProjectTarget,
    opts: &ProjectOpts,
) -> Result<(), NodeError> {
    use crate::nodes::common::rotation_between;
    let (align, keep_outside) = (opts.align, opts.keep_outside);
    let grid = match target {
        ProjectTarget::Boxes { boxes, .. } => Some(BoxGrid::new(boxes)),
        _ => None,
    };
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        let mut hits: Vec<Option<usize>> = vec![None; p.len()];
        let mut keep = vec![true; p.len()];
        let position = p.position.make_mut();
        let mut rotation = align.then(|| p.rotation.make_mut());
        for (i, k) in keep.iter_mut().enumerate() {
            if i.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let pos = Vec3::from(position[i]);
            // (new position, up to align with) or None = outside.
            let hit: Option<(Vec3, Option<Vec3>)> = match target {
                ProjectTarget::Splines(curves) => curves
                    .iter()
                    .map(|c| (c.closest(pos), c))
                    .min_by(|a, b| a.0.distance_m.total_cmp(&b.0.distance_m))
                    .map(|(h, c)| (h.position, Some(c.at_distance(h.along_m).up))),
                ProjectTarget::Polygons(polys) => polys
                    .iter()
                    .find(|pg| pg.contains(f64::from(pos.x), f64::from(pos.z)))
                    .map(|pg| (Vec3::new(pos.x, pg.origin[1] as f32, pos.z), None)),
                ProjectTarget::Boxes { boxes, .. } => {
                    let near = grid.as_ref().map_or(&[][..], |g| g.near(pos.x, pos.z));
                    let mut best: Option<usize> = None;
                    for &j in near {
                        let (a, b, _) = boxes[j as usize];
                        let over = pos.x >= a[0] && pos.x <= b[0] && pos.z >= a[2] && pos.z <= b[2];
                        if over && best.is_none_or(|k| b[1] > boxes[k].1[1]) {
                            best = Some(j as usize);
                        }
                    }
                    hits[i] = best;
                    best.map(|j| {
                        let (_, b, r) = boxes[j];
                        (Vec3::new(pos.x, b[1], pos.z), Some(quat(r) * Vec3::Y))
                    })
                }
            };
            match hit {
                Some((to, up)) => {
                    position[i] = to.to_array();
                    if let (Some(rotation), Some(up)) = (&mut rotation, up)
                        && up.length_squared() > 1e-12
                    {
                        rotation[i] = (rotation_between(Vec3::Y, up.normalize()) * quat(rotation[i]))
                            .normalize()
                            .to_array();
                    }
                }
                None => *k = keep_outside,
            }
        }
        if let ProjectTarget::Boxes { from, sets, .. } = target
            && (opts.attributes.is_some() || opts.color != "source")
        {
            take_target_values(p, &hits, from, sets, opts);
        }
        *p = p.partition(&keep).0;
    }
    Ok(())
}

fn eval_split(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let axis = match s.text("axis") {
        "y" => 1,
        "z" => 2,
        _ => 0,
    };
    let at = s.f("at") as f32;
    let (mut low, mut high) = (Bundle::empty(), Bundle::empty());
    for item in crate::nodes::input_items(inputs, 0) {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let (mut a, mut b) = (p.clone(), p.clone());
        let (a_max, b_min) = (a.bounds_max.make_mut(), b.bounds_min.make_mut());
        for i in 0..p.len() {
            let (lo, hi) = (p.bounds_min[i][axis], p.bounds_max[i][axis]);
            let cut = lo + (hi - lo) * at;
            a_max[i][axis] = cut;
            b_min[i][axis] = cut;
        }
        low.0.push(item.with_payload(Payload::Points(a)));
        high.0.push(item.with_payload(Payload::Points(b)));
    }
    Ok(vec![low, high])
}

fn eval_neighbors(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let r = s.f("radius_m") as f32;
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Some(p) = item.as_points().cloned() else {
            continue;
        };
        let grid = PosGrid::new(&p.position, r);
        let (mut count, mut density, mut center) = (Vec::new(), Vec::new(), Vec::new());
        for (i, a) in p.position.iter().enumerate() {
            if i.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let (mut n, mut d, mut c) = (0.0f64, 0.0f64, [0.0f64; 3]);
            for j in grid.near(a) {
                if j != i && dist2(a, &p.position[j]) <= r * r {
                    n += 1.0;
                    d += f64::from(p.density[j]);
                    for (ck, pk) in c.iter_mut().zip(p.position[j]) {
                        *ck += f64::from(pk);
                    }
                }
            }
            count.push(n);
            density.push(if n > 0.0 { d / n } else { 0.0 });
            center.extend(c.map(|v| if n > 0.0 { v / n } else { 0.0 }));
        }
        write_num(item, "nb_count", 1, &count)?;
        write_num(item, "nb_density", 1, &density)?;
        write_num(item, "nb_center", 3, &center)?;
    }
    Ok(vec![out])
}

fn eval_rotation(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    use crate::nodes::common::{euler_deg, rotation_between};
    let op = s.text("op").to_string();
    let target = s.text("target").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let a_sel =
            crate::data::Selector::parse(s.text("a")).map_err(|m| NodeError::new("setting_type", m))?;
        let a = read_num(item, &a_sel)?;
        let b = if op == "compose" {
            Some(read_num(item, &s.sel("b"))?)
        } else {
            None
        };
        let need = |w: usize, what: &str| -> Result<(), NodeError> {
            if a.width == w {
                Ok(())
            } else {
                Err(NodeError::new(
                    "attr_type_mismatch",
                    format!("`{op}` needs {what}"),
                ))
            }
        };
        let q4 =
            |v: &[f64]| crate::rot::Rot([v[0] as f32, v[1] as f32, v[2] as f32, v[3] as f32]).normalize();
        let n = a.len();
        let mut values = Vec::with_capacity(n * 4);
        for i in 0..n {
            let q = match op.as_str() {
                "from_euler" => {
                    need(3, "a Vec3 of degrees")?;
                    let v = a.get(i);
                    euler_deg([v[0], v[1], v[2]])
                }
                "look_at" => {
                    need(3, "a Vec3 direction")?;
                    let v = a.get(i);
                    let d = Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32);
                    let len = d.length();
                    if len > 1e-9 {
                        rotation_between(Vec3::NEG_Z, d / len)
                    } else {
                        crate::rot::Rot::IDENTITY
                    }
                }
                "invert" => {
                    need(4, "a quaternion")?;
                    q4(a.get(i)).inverse()
                }
                _ => {
                    need(4, "quaternions")?;
                    let b = b
                        .as_ref()
                        .filter(|b| b.width == 4)
                        .ok_or_else(|| NodeError::new("attr_type_mismatch", "`b` must be a quaternion"))?;
                    (q4(a.get(i)) * q4(b.get(i))).normalize()
                }
            };
            values.extend(q.to_array().map(f64::from));
        }
        if target == "rotation" {
            write_num(item, "rotation", 4, &values)?;
        } else if let Payload::Points(p) = &mut item.payload {
            if crate::data::is_reserved_name(&target) {
                return Err(NodeError::new(
                    "attr_reserved_name",
                    format!("`{target}` is reserved"),
                ));
            }
            p.attrs.insert(
                target.clone(),
                AttrColumn::Quat(
                    (0..n)
                        .map(|i| {
                            [
                                values[4 * i],
                                values[4 * i + 1],
                                values[4 * i + 2],
                                values[4 * i + 3],
                            ]
                        })
                        .collect(),
                ),
            );
            item.last_written = Some(target.clone());
        }
    }
    Ok(vec![out])
}

fn eval_in_mask(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let path = s.text("asset");
    let rect: Vec<f32> = s
        .get("rect")
        .and_then(SettingValue::as_items)
        .map(|v| v.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
        .unwrap_or_default();
    let [x0, z0, x1, z1] = rect[..] else {
        return Err(NodeError::new(
            "setting_type",
            "`rect` must be [min_x, min_z, max_x, max_z]",
        ));
    };
    let Some(img) = ctx.world().mask_image(path).cloned() else {
        ctx.warn("mask_missing", format!("mask `{path}` was not captured"));
        return Ok(vec![Bundle::empty()]);
    };
    let channel = s.text("channel").to_string();
    let values: Vec<f32> = img
        .rgba
        .iter()
        .map(|p| match channel.as_str() {
            "g" => p[1],
            "b" => p[2],
            "a" => p[3],
            "luma" => 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2],
            _ => p[0],
        })
        .collect();
    let m = crate::data::MaskData::new(
        [x0, z0, x1, z1],
        img.width,
        img.height,
        values,
        s.text("filter") == "bilinear",
    )
    .map_err(|e| NodeError::new("setting_type", e))?;
    Ok(vec![Bundle::one(DataItem::new(Payload::Mask(
        std::sync::Arc::new(m),
    )))])
}

/// Samples one placement of a mesh: `count` points chosen by area (slot 0), barycentric by the
/// square-root method (slots 1, 2), keyed by `base_seed` and the sample index.
fn surface_points(
    ctx: &mut NodeCtx<'_>,
    mesh: &crate::data::MeshData,
    place: (Vec3, crate::rot::Rot, Vec3),
    base_seed: u32,
    per_m2: f64,
    max_points: usize,
    out: &mut PointSet,
) -> Result<(), NodeError> {
    use crate::nodes::common::rotation_between;
    use regia_core::math::hash::mix;
    let (t, r, sc) = place;
    let world = |p: [f32; 3]| t + r * (sc * Vec3::from(p));
    let tris = mesh.indices.len() / 3;
    let mut cumulative = Vec::with_capacity(tris);
    let mut total = 0.0f64;
    for k in 0..tris {
        let ix = |j: usize| mesh.indices[3 * k + j] as usize;
        let (Some(a), Some(b), Some(c)) = (
            mesh.positions.get(ix(0)),
            mesh.positions.get(ix(1)),
            mesh.positions.get(ix(2)),
        ) else {
            return Err(NodeError::new(
                "mesh_bad_index",
                format!("`{}` triangle {k} indexes past its vertices", mesh.asset),
            ));
        };
        let (a, b, c) = (world(*a), world(*b), world(*c));
        total += f64::from((b - a).cross(c - a).length() * 0.5);
        cumulative.push(total);
    }
    if total <= 0.0 {
        return Ok(());
    }
    let wanted = (total * per_m2).round() as usize;
    if wanted > max_points {
        ctx.warn("max_points", format!("capped at {max_points} points"));
    }
    let count = wanted.min(max_points);
    ctx.check_points(out.len() + count)?;
    for n in 0..count {
        if n.is_multiple_of(CHUNK) {
            ctx.check_cancel()?;
        }
        let seed = mix(base_seed, n as u32);
        let pick = ctx.draw(seed, 0) * total;
        let k = cumulative.partition_point(|c| *c <= pick).min(tris - 1);
        let ix = |j: usize| mesh.indices[3 * k + j] as usize;
        let (a, b, c) = (
            world(mesh.positions[ix(0)]),
            world(mesh.positions[ix(1)]),
            world(mesh.positions[ix(2)]),
        );
        let (u1, u2) = (ctx.draw(seed, 1) as f32, ctx.draw(seed, 2) as f32);
        let su = u1.sqrt();
        let p = a * (1.0 - su) + b * (su * (1.0 - u2)) + c * (su * u2);
        let normal = (b - a).cross(c - a);
        let normal = if normal.length() > 1e-12 {
            normal / normal.length()
        } else {
            Vec3::Y
        };
        let position = p.to_array();
        out.push(Point {
            position,
            rotation: rotation_between(Vec3::Y, normal).to_array(),
            seed: ctx.point_seed(position),
            ..Point::default()
        });
        if let Some(AttrColumn::Vec3(v)) = out.attrs.get_mut("normal")
            && let Some(last) = v.last_mut()
        {
            *last = normal.to_array().map(f64::from);
        }
    }
    Ok(())
}

fn eval_mesh_surface(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let per_m2 = s.f("per_m2");
    let max_points = usize::try_from(s.i("max_points")).unwrap_or(usize::MAX);
    let mut out = Bundle::empty();
    for (k, item) in crate::nodes::input_items(inputs, 0).iter().enumerate() {
        let mut pts = PointSet::new();
        pts.attrs
            .insert("normal".to_string(), AttrColumn::Vec3(Vec::new().into()));
        match &item.payload {
            Payload::Mesh(m) => {
                // Later items mix in their index, so two bare mesh items do not draw the same
                // samples (audit L16); the first keeps the origin seed it always had.
                let origin = crate::seeds::point_seed([0.0; 3], ctx.tier_seed);
                let seed = match u32::try_from(k) {
                    Ok(0) => origin,
                    Ok(k) => regia_core::math::hash::mix(origin, k),
                    Err(_) => regia_core::math::hash::mix(origin, u32::MAX),
                };
                surface_points(
                    ctx,
                    m,
                    (Vec3::ZERO, crate::rot::Rot::IDENTITY, Vec3::ONE),
                    seed,
                    per_m2,
                    max_points,
                    &mut pts,
                )?;
            }
            Payload::Points(p) => {
                let Some(AttrColumn::Asset(names) | AttrColumn::Name(names)) = p.attrs.get("mesh") else {
                    return Err(NodeError::new(
                        "attr_missing",
                        "points need a `mesh` attribute (spawn.mesh writes it)",
                    ));
                };
                for (i, name) in names.iter().enumerate() {
                    if name.is_empty() {
                        continue;
                    }
                    let Some(m) = ctx.world().mesh(name) else {
                        ctx.warn("mesh_missing", format!("mesh `{name}` was not captured"));
                        continue;
                    };
                    let place = (
                        Vec3::from(p.position[i]),
                        quat(p.rotation[i]),
                        Vec3::from(p.scale[i]),
                    );
                    surface_points(ctx, &m, place, p.seed[i], per_m2, max_points, &mut pts)?;
                }
            }
            _ => continue,
        }
        out.0.push(item.with_payload(Payload::Points(pts)));
    }
    Ok(vec![out])
}

/// A transform attribute, or the points' own transforms when `name` is empty.
fn transforms(p: &PointSet, name: &str) -> Result<Vec<crate::data::Xform>, NodeError> {
    if name.is_empty() {
        return Ok((0..p.len())
            .map(|i| crate::data::Xform {
                t: p.position[i].map(f64::from),
                r: p.rotation[i].map(f64::from),
                s: p.scale[i].map(f64::from),
            })
            .collect());
    }
    match p.attrs.get(name) {
        Some(AttrColumn::Transform(v)) => Ok(v.to_vec()),
        Some(_) => Err(NodeError::new(
            "attr_type_mismatch",
            format!("`{name}` is not a Transform"),
        )),
        None => Err(NodeError::new(
            "attr_missing",
            format!("points have no attribute `{name}`"),
        )),
    }
}

fn xf_parts(x: &crate::data::Xform) -> (Vec3, crate::rot::Rot, Vec3) {
    (
        Vec3::from(x.t.map(|v| v as f32)),
        crate::rot::Rot(x.r.map(|v| v as f32)).normalize(),
        Vec3::from(x.s.map(|v| v as f32)),
    )
}

fn xf_make(t: Vec3, r: crate::rot::Rot, s: Vec3) -> crate::data::Xform {
    crate::data::Xform {
        t: t.to_array().map(f64::from),
        r: r.to_array().map(f64::from),
        s: s.to_array().map(f64::from),
    }
}

fn eval_transform(
    _: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let (an, bn, target) = (
        s.text("a").to_string(),
        s.text("b").to_string(),
        s.text("target").to_string(),
    );
    let t = s.f("t") as f32;
    if crate::data::is_reserved_name(&target) {
        return Err(NodeError::new(
            "attr_reserved_name",
            format!("`{target}` is reserved"),
        ));
    }
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        let a = transforms(p, &an)?;
        let col = match op.as_str() {
            "apply_point" | "apply_dir" => {
                let v = match p.attrs.get(&bn) {
                    Some(AttrColumn::Vec3(v)) => v.clone(),
                    _ => {
                        return Err(NodeError::new(
                            "attr_type_mismatch",
                            "`b` must be a Vec3 attribute",
                        ));
                    }
                };
                AttrColumn::Vec3(
                    a.iter()
                        .zip(&v)
                        .map(|(x, v)| {
                            let (tt, r, sc) = xf_parts(x);
                            let v = Vec3::from(v.map(|c| c as f32));
                            let o = if op == "apply_point" {
                                tt + r * (sc * v)
                            } else {
                                r * v
                            };
                            o.to_array().map(f64::from)
                        })
                        .collect(),
                )
            }
            "invert" => AttrColumn::Transform(
                a.iter()
                    .map(|x| {
                        let (tt, r, sc) = xf_parts(x);
                        let inv_s = Vec3::new(1.0 / sc.x, 1.0 / sc.y, 1.0 / sc.z);
                        let ri = r.inverse();
                        xf_make(-(inv_s * (ri * tt)), ri, inv_s)
                    })
                    .collect(),
            ),
            _ => {
                let b = transforms(p, &bn)?;
                AttrColumn::Transform(
                    a.iter()
                        .zip(&b)
                        .map(|(x, y)| {
                            let ((ta, ra, sa), (tb, rb, sb)) = (xf_parts(x), xf_parts(y));
                            if op == "lerp" {
                                let (ra4, rb4) = (ra.to_array(), rb.to_array());
                                let sign = if (0..4).map(|k| ra4[k] * rb4[k]).sum::<f32>() < 0.0 {
                                    -1.0
                                } else {
                                    1.0
                                };
                                let r = crate::rot::Rot(std::array::from_fn(|k| {
                                    ra4[k] * (1.0 - t) + sign * rb4[k] * t
                                }));
                                xf_make(ta + (tb - ta) * t, r.normalize(), sa + (sb - sa) * t)
                            } else {
                                // a ∘ b: apply b first, then a.
                                xf_make(ta + ra * (sa * tb), (ra * rb).normalize(), sa * sb)
                            }
                        })
                        .collect(),
                )
            }
        };
        if col.first_non_finite().is_some() {
            return Err(NodeError::new(
                "attr_nan",
                format!("`{target}` would be non-finite (zero scale?)"),
            ));
        }
        p.attrs.insert(target.clone(), col);
        item.last_written = Some(target.clone());
    }
    Ok(vec![out])
}

#[cfg(test)]
mod box_grid_tests {
    use super::*;

    fn unit_box(x: f32, z: f32) -> TargetBox {
        (
            [x - 0.5, 0.0, z - 0.5],
            [x + 0.5, 1.0, z + 0.5],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// v0.5.1 (deep-zai-procgen-runtime finding 5): two 1 m targets 16 km apart get a grid sized by
    /// their count, not by the span between them, and still find both.
    #[test]
    fn sparse_far_apart_boxes_get_a_content_sized_grid() {
        let boxes = [unit_box(-8000.0, -8000.0), unit_box(8000.0, 8000.0)];
        let grid = BoxGrid::new(&boxes);
        // The budget: 16 entries per box + 65 536.
        assert!(
            grid.cells.len() <= 16 * boxes.len() + 65_536,
            "{} cells ({:?}) for 2 boxes",
            grid.cells.len(),
            grid.dims
        );
        assert_eq!(grid.near(-8000.0, -8000.0), &[0]);
        assert_eq!(grid.near(8000.0, 8000.0), &[1]);
        // Dense content keeps its fine grid: 100 x 100 boxes 2 m apart.
        let dense: Vec<TargetBox> = (0..100 * 100)
            .map(|i| unit_box((i % 100) as f32 * 2.0, (i / 100) as f32 * 2.0))
            .collect();
        let grid = BoxGrid::new(&dense);
        assert!(grid.cells.len() <= 16 * dense.len() + 65_536);
        assert!(grid.near(50.0, 50.0).len() <= 4, "{:?}", grid.near(50.0, 50.0));
        assert!(grid.near(50.0, 50.0).contains(&(25 * 100 + 25)));
    }
}
