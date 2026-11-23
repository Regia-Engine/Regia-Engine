// SPDX-License-Identifier: MPL-2.0
//! Attributes and tables (`attr.*`, `table.*`, spec 63 § 9). Attribute math is f64; trig goes
//! through [`crate::detmath`].

use crate::data::{
    AttrColumn, AttrType, AttrValue, Bundle, DataItem, DataKind, Payload, Selector, Table, is_reserved_name,
};
use crate::detmath;
use crate::exec::{CHUNK, NodeCtx};
use crate::nodes::common::{NumColumn, read_num, write_num};
use crate::nodes::{
    ANY, BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, kind, num, pin_out, pin_req,
    pins_any_inout, pins_points_inout, set,
};
use crate::value::SettingValue;

const OPS: &[&str] = &[
    "add",
    "sub",
    "mul",
    "div",
    "mod",
    "pow",
    "min",
    "max",
    "abs",
    "neg",
    "sign",
    "floor",
    "ceil",
    "round",
    "trunc",
    "frac",
    "sqrt",
    "clamp",
    "lerp",
    "one_minus",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "deg",
    "rad",
    "dot",
    "cross",
    "length",
    "normalize",
    "distance",
];

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::in_place(
        "attr.set",
        "attr",
        "Creates or overwrites an attribute with a constant or a copy of another selector",
        &[
            set(
                "target",
                T::Name,
                "\"value\"",
                "Attribute written (`item.` prefix for item scope)",
            ),
            set(
                "value",
                T::Any,
                "0.0",
                "Constant (number, bool, vector tuple or string)",
            ),
            set("from", T::Name, "\"\"", "Selector to copy instead of `value`"),
        ],
        pins_any_inout,
        eval_set,
    )
    .checked(check_set_value),
    BuiltinNode::in_place(
        "attr.math",
        "attr",
        "Element-wise f64 math (vectors component-wise, scalars broadcast)",
        &[
            set("op", T::Enum(OPS), "add", "Operation"),
            set(
                "a",
                T::Selector,
                "\"\"",
                "First operand (selector; empty = `$prev`)",
            ),
            set("b", T::Any, "0.0", "Second operand: selector string or number"),
            set("c", T::Any, "0.0", "Third operand (clamp hi, lerp t)"),
            set(
                "target",
                T::Name,
                "\"\"",
                "Attribute written (empty = the `a` attribute)",
            ),
        ],
        pins_any_inout,
        eval_math,
    )
    .checked(check_math_operands),
    BuiltinNode::in_place(
        "attr.random",
        "attr",
        "Uniform draw per element (slots 0..2)",
        &[
            set("target", T::Name, "\"random\"", "Attribute written"),
            set("type", T::Enum(&["Float", "Int", "Vec3"]), "Float", "Type"),
            set("min", T::Float, "0.0", "Low"),
            set("max", T::Float, "1.0", "High (exclusive for Int)"),
        ],
        pins_points_inout,
        eval_random,
    ),
    BuiltinNode::in_place(
        "attr.rename",
        "attr",
        "Renames a custom attribute (`attr_exists` unless `overwrite`)",
        &[
            set("from", T::Name, "\"\"", "Old name"),
            set("to", T::Name, "\"\"", "New name"),
            set("overwrite", T::Bool, "false", "Replace an existing `to`"),
        ],
        pins_any_inout,
        eval_rename,
    ),
    BuiltinNode::in_place(
        "attr.remove",
        "attr",
        "Removes custom attributes (`drop`) or keeps only some (`keep`); built-ins stay",
        &[
            set("drop", T::NameList, "[]", "Names to remove"),
            set("keep", T::NameList, "[]", "Names to keep (when not empty)"),
        ],
        pins_any_inout,
        eval_remove,
    ),
    BuiltinNode::new(
        "attr.reduce",
        "attr",
        "Reduces an attribute over elements in index order into an item-scope value or a one-row table",
        &[
            set("attr", T::Selector, "\"density\"", "Scalar to reduce"),
            set(
                "op",
                T::Enum(&["sum", "mean", "min", "max", "count"]),
                "sum",
                "Reduction",
            ),
            set("to", T::Enum(&["item", "table"]), "item", "Destination"),
            set("target", T::Name, "\"\"", "Name of the result (default: the op)"),
        ],
        |_| PinSpec::new(vec![pin_req("in", ANY)], vec![pin_out("out", ANY)]),
        eval_reduce,
    ),
    BuiltinNode::in_place(
        "attr.compare",
        "attr",
        "Writes a Bool: a op b (eq ne lt le gt ge), element-wise on scalars",
        &[
            set(
                "op",
                T::Enum(&["eq", "ne", "lt", "le", "gt", "ge"]),
                "eq",
                "Operator",
            ),
            set("a", T::Selector, "\"\"", "Left operand"),
            set("b", T::Any, "0.0", "Right operand: selector string or number"),
            set("target", T::Name, "\"cmp\"", "Attribute written"),
        ],
        pins_points_inout,
        eval_compare,
    )
    .checked(check_compare_operand),
    BuiltinNode::in_place(
        "attr.logic",
        "attr",
        "Bool ops and/or/xor/not/nand/nor (ints bitwise for and/or/xor/not); `not` reads only `a`",
        &[
            set(
                "op",
                T::Enum(&["and", "or", "xor", "not", "nand", "nor"]),
                "and",
                "Operator",
            ),
            set("a", T::Name, "\"\"", "Left attribute"),
            set("b", T::Name, "\"\"", "Right attribute"),
            set("target", T::Name, "\"logic\"", "Attribute written"),
        ],
        pins_points_inout,
        eval_logic,
    ),
    BuiltinNode::in_place(
        "attr.lookup",
        "attr",
        "Copies table columns onto elements whose key attributes equal the table's key columns",
        &[
            set("keys", T::Any, "[]", "[(attr, column)] key pairs"),
            set("columns", T::NameList, "[]", "Columns copied onto matches"),
            set(
                "on_miss",
                T::Enum(&["keep", "drop", "default"]),
                "keep",
                "Elements with no match",
            ),
            set(
                "multi",
                T::Enum(&["first", "weighted"]),
                "first",
                "Several matches: lowest row, or roulette (slot 0)",
            ),
            set("weight_column", T::Name, "\"weight\"", "Weights for `weighted`"),
            num(
                "nearest_within",
                T::Float,
                "-1.0",
                -1.0,
                1.0e9,
                "Float, vector and rotation keys match the closest row within this distance (Euclidean; rotations: angle in degrees); negative = exact",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_req("table", kind(DataKind::Table))],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_lookup,
    ),
    BuiltinNode::new(
        "points.from_table",
        "table",
        "One default point per row; listed columns fill built-ins; other columns become attributes (reserved names are skipped with a warning)",
        &[
            set("position", T::Name, "\"\"", "Vec3 column for positions"),
            set("rotation", T::Name, "\"\"", "Quat or Vec4 column for rotations"),
            set("scale", T::Name, "\"\"", "Vec3 column for scales"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", kind(DataKind::Table))],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_from_table,
    ),
    BuiltinNode::new(
        "table.from_points",
        "table",
        "One row per point with its custom attributes (built-ins only when listed in `include`)",
        &[set(
            "include",
            T::NameList,
            "[]",
            "Built-in properties to include",
        )],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS)],
                vec![pin_out("out", kind(DataKind::Table))],
            )
        },
        eval_from_points,
    ),
    BuiltinNode::new(
        "table.make",
        "table",
        "A literal table: [(name, type, values)]",
        &[set(
            "columns",
            T::Any,
            "[]",
            "Columns [(\"name\", Float, [1.0, 2.0])]",
        )],
        |_| PinSpec::new(vec![], vec![pin_out("out", kind(DataKind::Table))]),
        eval_make,
    )
    .checked(check_make),
];

fn eval_set(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let target = s.text("target").to_string();
    let from = s.text("from").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let n = item.payload.len();
        if !from.is_empty() {
            let sel = Selector::parse(&from).map_err(|m| NodeError::new("setting_type", m))?;
            let v = read_num(item, &sel)?;
            write_num(item, &target, v.width, &v.values)?;
            continue;
        }
        match s.get("value") {
            Some(SettingValue::Str(text)) => {
                let name = target.strip_prefix("item.");
                if let Some(name) = name {
                    item.scope.insert(name.to_string(), AttrValue::Name(text.clone()));
                } else if let Payload::Points(p) = &mut item.payload {
                    if is_reserved_name(&target) {
                        return Err(NodeError::new(
                            "attr_reserved_name",
                            format!("`{target}` is reserved"),
                        ));
                    }
                    p.attrs
                        .insert(target.clone(), AttrColumn::Name(vec![text.clone(); n].into()));
                } else if let Payload::Table(t) = &mut item.payload {
                    t.set_column(&target, AttrColumn::Name(vec![text.clone(); n].into()));
                }
                item.last_written = Some(target.clone());
            }
            Some(SettingValue::Bool(b)) if !target.starts_with("item.") => {
                if let Payload::Points(p) = &mut item.payload {
                    if is_reserved_name(&target) {
                        return Err(NodeError::new(
                            "attr_reserved_name",
                            format!("`{target}` is reserved"),
                        ));
                    }
                    p.attrs
                        .insert(target.clone(), AttrColumn::Bool(vec![*b; n].into()));
                    item.last_written = Some(target.clone());
                } else {
                    write_num(item, &target, 1, &vec![f64::from(u8::from(*b)); n])?;
                }
            }
            Some(v) => {
                let comps: Vec<f64> = match (v.as_f64(), v.as_items(), v.as_bool()) {
                    (Some(x), _, _) => vec![x],
                    (_, Some(items), _) => items.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect(),
                    (_, _, Some(b)) => vec![f64::from(u8::from(b))],
                    _ => {
                        return Err(NodeError::new(
                            "setting_type",
                            "`value` must be a number, vector, bool or string",
                        ));
                    }
                };
                let w = comps.len().clamp(1, 4);
                let count = if target.starts_with("item.") { 1 } else { n };
                let values: Vec<f64> = (0..count).flat_map(|_| comps.iter().copied().take(w)).collect();
                write_num(item, &target, w, &values)?;
            }
            None => {}
        }
    }
    Ok(vec![out])
}

/// Why a constant operand is malformed: a vector component that is not a number, or a value that is
/// neither a selector string, a number nor a vector (audit L8: these used to read as 0).
fn operand_error(name: &str, v: &SettingValue) -> Option<String> {
    match v {
        SettingValue::Str(_) => None,
        v if v.as_f64().is_some() => None,
        v => match v.as_items() {
            Some(items) if !items.is_empty() && items.iter().all(|x| x.as_f64().is_some()) => None,
            Some(_) => Some(format!("`{name}`: vector components must be numbers")),
            None => Some(format!(
                "`{name}` must be a selector string, a number or a vector, not `{}`",
                v.write()
            )),
        },
    }
}

fn check_math_operands(s: &Settings) -> Vec<(String, String)> {
    ["b", "c"]
        .into_iter()
        .filter_map(|k| s.get(k).and_then(|v| operand_error(k, v)))
        .map(|m| ("setting_type".to_string(), m))
        .collect()
}

fn check_compare_operand(s: &Settings) -> Vec<(String, String)> {
    s.get("b")
        .and_then(|v| operand_error("b", v))
        .map(|m| vec![("setting_type".to_string(), m)])
        .unwrap_or_default()
}

/// `attr.set` `value`: a number, a bool, a string or a vector of numbers (audit L8: a vector with
/// a non-number component wrote 0 there).
fn check_set_value(s: &Settings) -> Vec<(String, String)> {
    match s.get("value").and_then(SettingValue::as_items) {
        Some(items) if !items.iter().all(|x| x.as_f64().is_some()) => vec![(
            "setting_type".to_string(),
            "`value`: vector components must be numbers".to_string(),
        )],
        _ => Vec::new(),
    }
}

/// An operand: a selector read or a constant.
fn operand(item: &DataItem, v: Option<&SettingValue>) -> Result<NumColumn, NodeError> {
    let n = item.payload.len();
    match v {
        Some(SettingValue::Str(sel)) => read_num(
            item,
            &Selector::parse(sel).map_err(|m| NodeError::new("setting_type", m))?,
        ),
        Some(v) => {
            if let Some(m) = operand_error("operand", v) {
                return Err(NodeError::new("setting_type", m));
            }
            let comps: Vec<f64> = match (v.as_f64(), v.as_items()) {
                (Some(x), _) => vec![x],
                (_, Some(items)) => items.iter().filter_map(SettingValue::as_f64).collect(),
                _ => vec![0.0],
            };
            Ok(NumColumn {
                width: comps.len(),
                values: (0..n).flat_map(|_| comps.iter().copied()).collect(),
            })
        }
        None => Ok(NumColumn {
            width: 1,
            values: vec![0.0; n],
        }),
    }
}

fn eval_math(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let a_sel = Selector::parse(s.text("a")).map_err(|m| NodeError::new("setting_type", m))?;
        let target = match s.text("target") {
            "" => a_sel
                .resolve(item)
                .map(|x| x.name)
                .map_err(|m| NodeError::new("attr_missing", m))?,
            t => t.to_string(),
        };
        let a = read_num(item, &a_sel)?;
        let b = operand(item, s.get("b"))?;
        let c = operand(item, s.get("c"))?;
        let n = item.payload.len();
        let (width, values) = math(ctx, &op, &a, &b, &c, n)?;
        write_num(item, &target, width, &values)?;
    }
    Ok(vec![out])
}

fn math(
    ctx: &mut NodeCtx<'_>,
    op: &str,
    a: &NumColumn,
    b: &NumColumn,
    c: &NumColumn,
    n: usize,
) -> Result<(usize, Vec<f64>), NodeError> {
    let comp = |col: &NumColumn, i: usize, k: usize| -> f64 {
        let e = col.get(i);
        if e.len() == 1 { e[0] } else { e[k] }
    };
    let mismatch = || NodeError::new("attr_type_mismatch", format!("`{op}`: operand widths differ"));
    let vec_ops = ["dot", "cross", "length", "normalize", "distance"];
    if vec_ops.contains(&op) {
        let w = a.width;
        if (op != "length" && op != "normalize") && b.width != w {
            return Err(mismatch());
        }
        let mut out = Vec::new();
        let mut width = 1;
        for i in 0..n {
            let (av, bv) = (a.get(i), b.get(i));
            match op {
                "dot" => out.push(av.iter().zip(bv).map(|(x, y)| x * y).sum()),
                "length" => out.push(av.iter().map(|x| x * x).sum::<f64>().sqrt()),
                "distance" => out.push(
                    av.iter()
                        .zip(bv)
                        .map(|(x, y)| (x - y) * (x - y))
                        .sum::<f64>()
                        .sqrt(),
                ),
                "normalize" => {
                    width = w;
                    let len = av.iter().map(|x| x * x).sum::<f64>().sqrt();
                    out.extend(av.iter().map(|x| if len < 1e-12 { 0.0 } else { x / len }));
                }
                _ => {
                    if w != 3 {
                        return Err(mismatch());
                    }
                    width = 3;
                    out.extend([
                        av[1] * bv[2] - av[2] * bv[1],
                        av[2] * bv[0] - av[0] * bv[2],
                        av[0] * bv[1] - av[1] * bv[0],
                    ]);
                }
            }
        }
        return Ok((width, out));
    }
    let widths = [a.width, b.width, c.width];
    let width = widths.iter().copied().max().unwrap_or(1);
    if widths.iter().any(|w| *w != 1 && *w != width) {
        return Err(mismatch());
    }
    let mut zero_div = false;
    let mut out = Vec::with_capacity(n * width);
    for i in 0..n {
        for k in 0..width {
            let (x, y, z) = (comp(a, i, k), comp(b, i, k), comp(c, i, k));
            let v = match op {
                "add" => x + y,
                "sub" => x - y,
                "mul" => x * y,
                "div" => {
                    if y == 0.0 {
                        zero_div = true;
                        0.0
                    } else {
                        x / y
                    }
                }
                "mod" => {
                    if y == 0.0 {
                        zero_div = true;
                        0.0
                    } else {
                        x - y * (x / y).floor()
                    }
                }
                "pow" => detmath::pow(x, y),
                "min" => x.min(y),
                "max" => x.max(y),
                "abs" => x.abs(),
                "neg" => -x,
                "sign" => {
                    if x > 0.0 {
                        1.0
                    } else if x < 0.0 {
                        -1.0
                    } else {
                        0.0
                    }
                }
                "floor" => x.floor(),
                "ceil" => x.ceil(),
                "round" => x.round(),
                "trunc" => x.trunc(),
                "frac" => x - x.floor(),
                "sqrt" => {
                    if x < 0.0 {
                        f64::NAN
                    } else {
                        x.sqrt()
                    }
                }
                "clamp" => x.max(y).min(z),
                "lerp" => x + (y - x) * z,
                "one_minus" => 1.0 - x,
                "sin" => detmath::sin_cos(x).0,
                "cos" => detmath::sin_cos(x).1,
                "tan" => {
                    let (s, co) = detmath::sin_cos(x);
                    if co == 0.0 { f64::NAN } else { s / co }
                }
                "asin" => detmath::asin(x),
                "acos" => detmath::acos(x),
                "atan" => detmath::atan(x),
                "atan2" => detmath::atan2(x, y),
                "deg" => x * (180.0 / std::f64::consts::PI),
                "rad" => x * (std::f64::consts::PI / 180.0),
                other => return Err(NodeError::new("setting_type", format!("unknown op `{other}`"))),
            };
            out.push(v);
        }
    }
    if zero_div {
        ctx.warn("div_by_zero", "division by zero yielded 0");
    }
    Ok((width, out))
}

fn eval_random(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let target = s.text("target").to_string();
    let (lo, hi) = (s.f("min"), s.f("max"));
    let ty = s.text("type").to_string();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        if is_reserved_name(&target) {
            return Err(NodeError::new(
                "attr_reserved_name",
                format!("`{target}` is reserved"),
            ));
        }
        let col = match ty.as_str() {
            "Int" => AttrColumn::Int(
                p.seed
                    .iter()
                    .map(|sd| (lo + (hi - lo) * ctx.draw(*sd, 0)).floor() as i64)
                    .collect(),
            ),
            "Vec3" => AttrColumn::Vec3(
                p.seed
                    .iter()
                    .map(|sd| std::array::from_fn(|k| lo + (hi - lo) * ctx.draw(*sd, k as u32)))
                    .collect(),
            ),
            _ => AttrColumn::Float(
                p.seed
                    .iter()
                    .map(|sd| lo + (hi - lo) * ctx.draw(*sd, 0))
                    .collect(),
            ),
        };
        p.attrs.insert(target.clone(), col);
        item.last_written = Some(target.clone());
    }
    Ok(vec![out])
}

fn eval_rename(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let (from, to) = (s.text("from").to_string(), s.text("to").to_string());
    let overwrite = s.b("overwrite");
    if is_reserved_name(&to) {
        return Err(NodeError::new(
            "attr_reserved_name",
            format!("`{to}` is reserved"),
        ));
    }
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        match &mut item.payload {
            Payload::Points(p) => {
                if p.attrs.contains_key(&to) && !overwrite {
                    return Err(NodeError::new("attr_exists", format!("`{to}` already exists")));
                }
                if let Some(c) = p.attrs.remove(&from) {
                    p.attrs.insert(to.clone(), c);
                }
            }
            Payload::Table(t) => {
                if t.column(&to).is_some() && !overwrite {
                    return Err(NodeError::new("attr_exists", format!("`{to}` already exists")));
                }
                if let Some(pos) = t.columns.iter().position(|(k, _)| *k == from) {
                    let (_, c) = t.columns.remove(pos);
                    t.set_column(&to, c);
                }
            }
            _ => {}
        }
        if item.last_written.as_deref() == Some(from.as_str()) {
            item.last_written = Some(to.clone());
        }
    }
    Ok(vec![out])
}

fn eval_remove(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let (drop, keep) = (s.names("drop"), s.names("keep"));
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let gone =
            |k: &str| drop.iter().any(|d| d == k) || (!keep.is_empty() && !keep.iter().any(|x| x == k));
        match &mut item.payload {
            Payload::Points(p) => p.attrs.retain(|k, _| !gone(k)),
            Payload::Table(t) => t.columns.retain(|(k, _)| !gone(k)),
            _ => {}
        }
    }
    Ok(vec![out])
}

fn eval_reduce(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let sel = s.sel("attr");
    let to_table = s.text("to") == "table";
    let target = match s.text("target") {
        "" => op.clone(),
        t => t.to_string(),
    };
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let n = item.payload.len();
        let value = if op == "count" {
            n as f64
        } else {
            let v = read_num(item, &sel)?;
            // Left to right in index order (spec 63 § 9).
            let mut acc = match op.as_str() {
                "min" => f64::INFINITY,
                "max" => f64::NEG_INFINITY,
                _ => 0.0,
            };
            for i in 0..v.len() {
                let x = v.scalar(i);
                acc = match op.as_str() {
                    "min" => acc.min(x),
                    "max" => acc.max(x),
                    _ => acc + x,
                };
            }
            match op.as_str() {
                "mean" if n > 0 => acc / n as f64,
                "mean" | "min" | "max" if n == 0 => 0.0,
                _ => acc,
            }
        };
        if to_table {
            out.0.push(item.with_payload(Payload::Table(Table::one_row([(
                target.clone(),
                AttrValue::Float(value),
            )]))));
        } else {
            let mut it = item.clone();
            it.scope.insert(target.clone(), AttrValue::Float(value));
            it.last_written = Some(format!("item.{target}"));
            out.0.push(it);
        }
    }
    Ok(vec![out])
}

fn eval_from_points(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let include = s.names("include");
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let mut t = Table {
            rows: p.len(),
            columns: Vec::new(),
        };
        for b in &include {
            let v = read_num(
                item,
                &Selector::parse(b).map_err(|m| NodeError::new("setting_type", m))?,
            )?;
            let col = match v.width {
                1 => AttrColumn::Float(v.values.clone().into()),
                3 => AttrColumn::Vec3(
                    (0..v.len())
                        .map(|i| [v.get(i)[0], v.get(i)[1], v.get(i)[2]])
                        .collect(),
                ),
                _ => AttrColumn::Vec4(
                    (0..v.len())
                        .map(|i| std::array::from_fn(|k| v.get(i)[k]))
                        .collect(),
                ),
            };
            t.set_column(b, col);
        }
        for (k, c) in &p.attrs {
            t.set_column(k, c.clone());
        }
        if p.attrs.is_empty() && include.is_empty() {
            t.rows = 0;
        }
        out.0.push(item.with_payload(Payload::Table(t)));
    }
    Ok(vec![out])
}

/// Parses `table.make` columns: `[(name, type, [values])]`.
fn make_columns(v: &SettingValue) -> Result<Table, String> {
    let items = v.as_items().ok_or("`columns` must be a list")?;
    let mut t = Table::default();
    for (k, c) in items.iter().enumerate() {
        let parts = c
            .as_items()
            .filter(|p| p.len() == 3)
            .ok_or("each column is (name, type, [values])")?;
        let name = parts[0].as_text().ok_or("column name must be a string")?;
        let ty_name = parts[1].as_text().ok_or("column type must be an identifier")?;
        let ty = AttrType::from_name(ty_name).ok_or_else(|| format!("unknown type `{ty_name}`"))?;
        let values = parts[2].as_items().ok_or("column values must be a list")?;
        let mut col = AttrColumn::new(ty);
        for v in values {
            let value = match ty {
                AttrType::Bool => v.as_bool().map(AttrValue::Bool),
                AttrType::Int => v.as_i64().map(AttrValue::Int),
                // Non-finite numbers are refused here: the run's `attr_nan` gate only sees points
                // (zai audit L4).
                AttrType::Float => v.as_f64().filter(|x| x.is_finite()).map(AttrValue::Float),
                AttrType::Vec3 => v
                    .as_vec3()
                    .filter(|x| x.iter().all(|c| c.is_finite()))
                    .map(AttrValue::Vec3),
                AttrType::Name => v.as_text().map(|s| AttrValue::Name(s.to_string())),
                AttrType::Asset => v.as_text().map(|s| AttrValue::Asset(s.to_string())),
                _ => None,
            }
            .ok_or_else(|| format!("column `{name}`: {} is not a {ty_name}", v.write()))?;
            col.push(value);
        }
        if k == 0 {
            t.rows = col.len();
        } else if col.len() != t.rows {
            return Err("table_ragged".to_string());
        }
        t.set_column(name, col);
    }
    Ok(t)
}

fn check_make(s: &Settings) -> Vec<(String, String)> {
    match s.get("columns").map(make_columns) {
        Some(Err(m)) if m == "table_ragged" => {
            vec![("table_ragged".into(), "columns differ in length".into())]
        }
        Some(Err(m)) => vec![("setting_type".into(), m)],
        _ => Vec::new(),
    }
}

fn eval_make(_: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let t = s
        .get("columns")
        .map(make_columns)
        .transpose()
        .map_err(|m| NodeError::new("setting_type", m))?
        .unwrap_or_default();
    Ok(vec![Bundle::one(DataItem::new(Payload::Table(t)))])
}

fn eval_compare(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let target = s.text("target").to_string();
    if is_reserved_name(&target) {
        return Err(NodeError::new(
            "attr_reserved_name",
            format!("`{target}` is reserved"),
        ));
    }
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let a = read_num(
            item,
            &Selector::parse(s.text("a")).map_err(|m| NodeError::new("setting_type", m))?,
        )?;
        let b = operand(item, s.get("b"))?;
        if a.width != 1 || b.width != 1 {
            return Err(NodeError::new("attr_type_mismatch", "attr.compare needs scalars"));
        }
        let col: Vec<bool> = (0..a.len())
            .map(|i| {
                let (x, y) = (a.scalar(i), b.scalar(i));
                match op.as_str() {
                    "ne" => x != y,
                    "lt" => x < y,
                    "le" => x <= y,
                    "gt" => x > y,
                    "ge" => x >= y,
                    _ => x == y,
                }
            })
            .collect();
        if let Payload::Points(p) = &mut item.payload {
            p.attrs.insert(target.clone(), AttrColumn::Bool(col.into()));
            item.last_written = Some(target.clone());
        }
    }
    Ok(vec![out])
}

fn eval_logic(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let op = s.text("op").to_string();
    let target = s.text("target").to_string();
    if is_reserved_name(&target) {
        return Err(NodeError::new(
            "attr_reserved_name",
            format!("`{target}` is reserved"),
        ));
    }
    let (an, bn) = (s.text("a").to_string(), s.text("b").to_string());
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        let get = |n: &str| -> Result<AttrColumn, NodeError> {
            p.attrs
                .get(n)
                .cloned()
                .ok_or_else(|| NodeError::new("attr_missing", format!("points have no attribute `{n}`")))
        };
        let a = get(&an)?;
        let b = if op == "not" { a.clone() } else { get(&bn)? };
        let col = match (a, b) {
            (AttrColumn::Bool(a), AttrColumn::Bool(b)) => AttrColumn::Bool(
                a.iter()
                    .zip(&b)
                    .map(|(x, y)| match op.as_str() {
                        "or" => *x || *y,
                        "xor" => x != y,
                        "not" => !*x,
                        "nand" => !(*x && *y),
                        "nor" => !(*x || *y),
                        _ => *x && *y,
                    })
                    .collect(),
            ),
            (AttrColumn::Int(a), AttrColumn::Int(b))
                if matches!(op.as_str(), "and" | "or" | "xor" | "not") =>
            {
                AttrColumn::Int(
                    a.iter()
                        .zip(&b)
                        .map(|(x, y)| match op.as_str() {
                            "or" => x | y,
                            "xor" => x ^ y,
                            "not" => !x,
                            _ => x & y,
                        })
                        .collect(),
                )
            }
            _ => {
                return Err(NodeError::new(
                    "attr_type_mismatch",
                    "attr.logic needs two Bool (or Int for and/or/xor/not) attributes",
                ));
            }
        };
        p.attrs.insert(target.clone(), col);
        item.last_written = Some(target.clone());
    }
    Ok(vec![out])
}

/// The `(attribute, column)` key pairs of `attr.lookup`.
fn lookup_keys(s: &Settings) -> Vec<(String, String)> {
    s.get("keys")
        .and_then(SettingValue::as_items)
        .map(|v| {
            v.iter()
                .filter_map(|p| {
                    let t = p.as_items()?;
                    Some((
                        t.first()?.as_text()?.to_string(),
                        t.get(1)?.as_text()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn eval_lookup(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let keys = lookup_keys(s);
    if keys.is_empty() {
        return Err(NodeError::new(
            "setting_type",
            "`keys` needs at least one (attr, column) pair",
        ));
    }
    let columns = s.names("columns");
    let (on_miss, weighted) = (s.text("on_miss").to_string(), s.text("multi") == "weighted");
    let weight_col = s.text("weight_column").to_string();
    let table = crate::nodes::input(inputs, 1)
        .0
        .into_iter()
        .find_map(|i| match i.payload {
            Payload::Table(t) => Some(t),
            _ => None,
        })
        .ok_or_else(|| NodeError::new("attr_missing", "attr.lookup needs a table on `table`"))?;
    let key_text = |c: &AttrColumn, i: usize| c.get(i).map(|v| v.key_text());
    let mut table_keys: Vec<Vec<Option<String>>> = vec![Vec::new(); table.rows];
    for (_, col) in &keys {
        let c = table
            .column(col)
            .ok_or_else(|| NodeError::new("attr_missing", format!("table has no column `{col}`")))?;
        for (r, k) in table_keys.iter_mut().enumerate() {
            k.push(key_text(c, r));
        }
    }
    // One index over the table's keys (rows ascending per key, as the old per-element scan found
    // them), so a lookup is a map probe instead of a scan of every row (audit M14).
    if table.rows > ctx.limits().total_elements {
        return Err(NodeError::new(
            "limit_exceeded",
            format!(
                "total_elements: the table has {} rows > {}",
                table.rows,
                ctx.limits().total_elements
            ),
        ));
    }
    let mut index: std::collections::BTreeMap<&[Option<String>], Vec<usize>> =
        std::collections::BTreeMap::new();
    for (r, k) in table_keys.iter().enumerate() {
        index.entry(k.as_slice()).or_default().push(r);
    }
    let near = NearLookup::new(s.f("nearest_within"), &keys, &table, &table_keys);
    let weights: Vec<f64> = match table.column(&weight_col) {
        Some(c) => (0..table.rows)
            .map(|r| c.components(r).and_then(|x| x.first().copied()).unwrap_or(0.0))
            .collect(),
        None => vec![1.0; table.rows],
    };
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let mut elem_keys: Vec<Vec<Option<String>>> = vec![Vec::new(); p.len()];
        for (attr, _) in &keys {
            let c = p.attrs.get(attr).ok_or_else(|| {
                NodeError::new("attr_missing", format!("points have no attribute `{attr}`"))
            })?;
            for (i, k) in elem_keys.iter_mut().enumerate() {
                k.push(key_text(c, i));
            }
        }
        // `nearest_within`: each element's float-key components (per key; `None` = not numeric).
        let elem_values: Vec<Vec<Option<Vec<f64>>>> = match &near {
            Some(near) => near
                .float_keys
                .iter()
                .map(|&k| {
                    let c = p.attrs.get(&keys[k].0);
                    (0..p.len()).map(|i| c.and_then(|c| c.components(i))).collect()
                })
                .collect(),
            None => Vec::new(),
        };
        let mut picked: Vec<Option<usize>> = Vec::with_capacity(p.len());
        let mut scan = RowScan::default();
        for (i, ek) in elem_keys.iter().enumerate() {
            if i.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let near_rows;
            let rows: &[usize] = match &near {
                Some(near) => {
                    near_rows = near.rows(ek, &elem_values, i, &mut scan, ctx.cancel)?;
                    &near_rows
                }
                None => index.get(ek.as_slice()).map_or(&[], Vec::as_slice),
            };
            let row = if rows.is_empty() {
                None
            } else if weighted {
                let total: f64 = rows.iter().map(|r| weights[*r].max(0.0)).sum();
                if total <= 0.0 {
                    rows.first().copied()
                } else {
                    let u = ctx.draw(p.seed[i], 0) * total;
                    let mut acc = 0.0;
                    rows.iter()
                        .copied()
                        .find(|r| {
                            acc += weights[*r].max(0.0);
                            u < acc
                        })
                        .or(rows.last().copied())
                }
            } else {
                rows.first().copied()
            };
            picked.push(row);
        }
        let keep: Vec<bool> = picked.iter().map(|r| r.is_some() || on_miss != "drop").collect();
        let mut q = p.clone();
        for name in &columns {
            let src = table
                .column(name)
                .ok_or_else(|| NodeError::new("attr_missing", format!("table has no column `{name}`")))?;
            let mut col = AttrColumn::new(src.ty());
            for (i, r) in picked.iter().enumerate() {
                let v = match r {
                    Some(r) => src.get(*r),
                    None if on_miss == "keep" => q.attrs.get(name).and_then(|c| c.get(i)),
                    None => None,
                };
                let v = v
                    .filter(|v| v.ty() == src.ty())
                    .or_else(|| AttrColumn::filled(src.ty(), 1).get(0));
                if let Some(v) = v {
                    col.push(v);
                }
            }
            q.attrs.insert(name.clone(), col);
        }
        let (kept, _) = q.partition(&keep);
        item.payload = Payload::Points(kept);
    }
    Ok(vec![out])
}

/// Table rows a `nearest_within` scan visits between two cancel checks (ADR-7262): one element's
/// bucket can hold every row of the table, so the element loop's `CHUNK` check alone left the cancel
/// latency proportional to the table size.
const NEAR_ROW_BUDGET: usize = 4096;

/// The row budget of a `nearest_within` lookup, carried across elements: `since_check` counts rows
/// since the last cancel check, `scanned` all rows visited (the tests read it).
#[derive(Default)]
struct RowScan {
    since_check: usize,
    scanned: usize,
}

/// `attr.lookup nearest_within` (spec 63 § 9, ADR-3027): keys whose table column is a float, vector or
/// rotation match the closest row within the distance; the other keys still match exactly.
struct NearLookup {
    within: f64,
    /// Indices into `keys` of the float-like keys, and their table column's type.
    float_keys: Vec<usize>,
    rotation: Vec<bool>,
    /// Per float key, per row: the table value's components.
    table_values: Vec<Vec<Option<Vec<f64>>>>,
    /// The exact (non-float) keys of each row, rows grouped by them in ascending order.
    exact: std::collections::BTreeMap<Vec<Option<String>>, Vec<usize>>,
}

impl NearLookup {
    fn new(
        within: f64,
        keys: &[(String, String)],
        table: &Table,
        table_keys: &[Vec<Option<String>>],
    ) -> Option<Self> {
        if within.is_nan() || within < 0.0 {
            return None;
        }
        let float_like = |t: AttrType| {
            matches!(
                t,
                AttrType::Float | AttrType::Vec2 | AttrType::Vec3 | AttrType::Vec4 | AttrType::Quat
            )
        };
        let mut float_keys = Vec::new();
        let mut rotation = Vec::new();
        let mut table_values = Vec::new();
        for (k, (_, col)) in keys.iter().enumerate() {
            let Some(c) = table.column(col) else {
                continue;
            };
            if float_like(c.ty()) {
                float_keys.push(k);
                rotation.push(c.ty() == AttrType::Quat);
                table_values.push((0..table.rows).map(|r| c.components(r)).collect());
            }
        }
        if float_keys.is_empty() {
            return None;
        }
        let mut exact: std::collections::BTreeMap<Vec<Option<String>>, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (r, tk) in table_keys.iter().enumerate() {
            exact
                .entry(Self::exact_part(&float_keys, tk))
                .or_default()
                .push(r);
        }
        Some(Self {
            within,
            float_keys,
            rotation,
            table_values,
            exact,
        })
    }

    /// The key texts with the float keys left out.
    fn exact_part(float_keys: &[usize], keys: &[Option<String>]) -> Vec<Option<String>> {
        keys.iter()
            .enumerate()
            .filter(|(k, _)| !float_keys.contains(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// The distance between two values of one key: Euclidean, or for rotations the angle in degrees.
    fn distance(rotation: bool, a: &[f64], b: &[f64]) -> Option<f64> {
        if a.len() != b.len() {
            return None;
        }
        if rotation {
            let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
            let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
            if na <= 0.0 || nb <= 0.0 {
                return None;
            }
            let c = (dot / (na * nb)).abs().min(1.0);
            // detmath, not libm: one ulp changes which row a nearest-within query copies.
            return Some(2.0 * crate::detmath::acos(c).to_degrees());
        }
        Some(
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f64>()
                .sqrt(),
        )
    }

    /// The rows at the smallest distance within the limit (the largest per-key distance of a row),
    /// ascending; empty when none is within. Checks `cancel` every [`NEAR_ROW_BUDGET`] rows of
    /// `scan` (ADR-7262); the checks change no result.
    ///
    /// # Errors
    ///
    /// `cancelled` when the token is tripped at a check.
    fn rows(
        &self,
        elem_keys: &[Option<String>],
        elem_values: &[Vec<Option<Vec<f64>>>],
        i: usize,
        scan: &mut RowScan,
        cancel: &crate::exec::CancelToken,
    ) -> Result<Vec<usize>, NodeError> {
        let Some(bucket) = self.exact.get(&Self::exact_part(&self.float_keys, elem_keys)) else {
            return Ok(Vec::new());
        };
        let mut best = f64::INFINITY;
        let mut rows = Vec::new();
        'row: for &r in bucket {
            scan.scanned += 1;
            scan.since_check += 1;
            if scan.since_check >= NEAR_ROW_BUDGET {
                scan.since_check = 0;
                if cancel.is_cancelled() {
                    return Err(NodeError::new("cancelled", "run cancelled"));
                }
            }
            let mut d = 0.0f64;
            for (k, rot) in self.rotation.iter().enumerate() {
                let (Some(a), Some(b)) = (&elem_values[k][i], &self.table_values[k][r]) else {
                    continue 'row;
                };
                match Self::distance(*rot, a, b) {
                    Some(x) if x.is_finite() => d = d.max(x),
                    _ => continue 'row,
                }
            }
            if d > self.within {
                continue;
            }
            if d < best {
                best = d;
                rows.clear();
            }
            if d == best {
                rows.push(r);
            }
        }
        Ok(rows)
    }
}

fn eval_from_table(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Payload::Table(t) = &item.payload else {
            continue;
        };
        ctx.check_points(t.rows)?;
        let mut p = crate::data::PointSet::new();
        for _ in 0..t.rows {
            p.push(crate::data::Point::default());
        }
        let vecs = |name: &str, w: usize| -> Result<Option<Vec<Vec<f64>>>, NodeError> {
            if name.is_empty() {
                return Ok(None);
            }
            let c = t
                .column(name)
                .ok_or_else(|| NodeError::new("attr_missing", format!("table has no column `{name}`")))?;
            if c.ty().width() != Some(w) {
                return Err(NodeError::new(
                    "attr_type_mismatch",
                    format!("column `{name}` needs {w} components"),
                ));
            }
            Ok(Some(
                (0..t.rows).map(|r| c.components(r).unwrap_or_default()).collect(),
            ))
        };
        if let Some(v) = vecs(s.text("position"), 3)? {
            let position = p.position.make_mut();
            for (i, x) in v.iter().enumerate() {
                position[i] = [x[0] as f32, x[1] as f32, x[2] as f32];
            }
        }
        if let Some(v) = vecs(s.text("rotation"), 4)? {
            let rotation = p.rotation.make_mut();
            for (i, x) in v.iter().enumerate() {
                rotation[i] =
                    crate::data::quat([x[0] as f32, x[1] as f32, x[2] as f32, x[3] as f32]).to_array();
            }
        }
        if let Some(v) = vecs(s.text("scale"), 3)? {
            let scale = p.scale.make_mut();
            for (i, x) in v.iter().enumerate() {
                scale[i] = [x[0] as f32, x[1] as f32, x[2] as f32];
            }
        }
        for (seed, pos) in p.seed.iter_mut().zip(&p.position) {
            *seed = ctx.point_seed(*pos);
        }
        for (name, c) in &t.columns {
            if is_reserved_name(name) {
                // Built-ins come only from the `position`/`rotation`/`scale` settings; a reserved
                // column those do not read is reported rather than dropped silently (audit L9).
                if [s.text("position"), s.text("rotation"), s.text("scale")].contains(&name.as_str()) {
                    continue;
                }
                ctx.warn(
                    "column_reserved",
                    format!("column `{name}` has a built-in or reserved name and is not copied"),
                );
            } else {
                p.attrs.insert(name.clone(), c.clone());
            }
        }
        out.0.push(item.with_payload(Payload::Points(p)));
    }
    Ok(vec![out])
}

#[cfg(test)]
mod rotation_distance_tests {
    use super::NearLookup;

    #[test]
    fn rotation_nearest_distance_uses_deterministic_acos() {
        let a = [0.0, 0.0, 0.0, 1.0];
        let s = std::f64::consts::FRAC_1_SQRT_2;
        let b = [0.0, s, 0.0, s];
        let got = NearLookup::distance(true, &a, &b).expect("angle");
        let dot: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
        let c = (dot / (na * nb)).abs().min(1.0);
        let expect = 2.0 * crate::detmath::acos(c).to_degrees();
        assert_eq!(
            got.to_bits(),
            expect.to_bits(),
            "platform acos {got} vs detmath {expect}"
        );
    }
}

#[cfg(test)]
mod near_cancel_tests {
    use super::{NEAR_ROW_BUDGET, NearLookup, RowScan};
    use crate::exec::CancelToken;

    /// One bucket of `n` float rows (`0, 1, 2, ...`), every row within reach.
    fn lookup(n: usize) -> NearLookup {
        let mut exact = std::collections::BTreeMap::new();
        exact.insert(Vec::new(), (0..n).collect());
        NearLookup {
            within: f64::MAX,
            float_keys: vec![0],
            rotation: vec![false],
            table_values: vec![(0..n).map(|r| Some(vec![r as f64])).collect()],
            exact,
        }
    }

    /// A cancel that arrives while one element scans a large bucket stops the scan within one row
    /// budget (5j): counted in rows, no wall clock.
    #[test]
    fn rows_scanned_after_a_cancel_stay_within_the_budget() {
        let n = NEAR_ROW_BUDGET * 8 + 17;
        let near = lookup(n);
        let values = vec![vec![Some(vec![3.0])]];
        let token = CancelToken::new();
        token.cancel();
        let mut scan = RowScan::default();
        let got = near.rows(&[None], &values, 0, &mut scan, &token);
        assert_eq!(
            got.map_err(|e| e.code),
            Err("cancelled".to_string()),
            "a cancelled token ends the scan"
        );
        assert!(
            scan.scanned <= NEAR_ROW_BUDGET,
            "{} rows scanned after the cancel, budget {NEAR_ROW_BUDGET}",
            scan.scanned
        );

        // The budget carries across elements: a cancel between two calls stops the next one
        // within what is left of the budget.
        let live = CancelToken::new();
        let mut scan = RowScan::default();
        let small = lookup(NEAR_ROW_BUDGET / 2 + 1);
        assert_eq!(
            small.rows(&[None], &values, 0, &mut scan, &live).ok(),
            Some(vec![3])
        );
        let before = scan.scanned;
        live.cancel();
        assert!(near.rows(&[None], &values, 0, &mut scan, &live).is_err());
        assert!(
            scan.scanned - before <= NEAR_ROW_BUDGET,
            "{} rows after the cancel",
            scan.scanned - before
        );
    }

    /// An uncancelled scan visits every row and answers the same rows as before the budget.
    #[test]
    fn an_uncancelled_scan_is_unchanged() {
        let n = NEAR_ROW_BUDGET * 3 + 5;
        let near = lookup(n);
        let values = vec![vec![Some(vec![1234.4])]];
        let mut scan = RowScan::default();
        let got = near.rows(&[None], &values, 0, &mut scan, &CancelToken::new());
        assert_eq!(got.ok(), Some(vec![1234]));
        assert_eq!(scan.scanned, n);
    }
}
