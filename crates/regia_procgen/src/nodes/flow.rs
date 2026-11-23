// SPDX-License-Identifier: MPL-2.0
//! Control flow, debug and graph-output nodes (`flow.*`, `debug.*`, `graph.output`; spec 63 §§ 10-12).

use regia_render::quality::QualityTier;

use std::collections::BTreeMap;

use crate::data::{Bundle, Payload};
use crate::exec::NodeCtx;
use crate::nodes::common::read_num;
use crate::nodes::{
    ANY, BuiltinNode, NodeError, PinSpec, SettingType as T, Settings, TABLE, pin_in, pin_out, pin_req, set,
};
use crate::value::SettingValue;

/// Input pins of `flow.quality`, in the fallback order of spec 63 § 10.
pub const QUALITY_PINS: [&str; 7] = [
    "mobile_low",
    "mobile_high",
    "deck",
    "low",
    "medium",
    "high",
    "ultra",
];

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "flow.if",
        "flow",
        "Routes the input to `then` or `else`; the untaken output is inactive",
        &[
            set("cond", T::Name, "\"\"", "Item-scope selector (`item.flag`); empty = `value` or the `cond` pin"),
            set("value", T::Bool, "true", "Condition when nothing else decides"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", ANY), pin_in("cond", TABLE)],
                vec![pin_out("then", ANY), pin_out("else", ANY)],
            )
        },
        eval_if,
    ),
    BuiltinNode::new(
        "flow.choose",
        "flow",
        "Forwards `a` when the condition is true, else `b` (both computed)",
        &[set("cond", T::Name, "\"\"", "As flow.if"), set("value", T::Bool, "true", "As flow.if")],
        |_| {
            PinSpec::new(
                vec![pin_in("a", ANY), pin_in("b", ANY), pin_in("cond", TABLE)],
                vec![pin_out("out", ANY)],
            )
        },
        eval_choose,
    ),
    BuiltinNode::new(
        "flow.quality",
        "flow",
        "Forwards the input of the run's quality tier; unconnected tiers fall back lower, then higher",
        &[],
        |_| PinSpec::new(QUALITY_PINS.iter().map(|p| pin_in(p, ANY)).collect(), vec![pin_out("out", ANY)]),
        eval_quality,
    ),
    BuiltinNode::new(
        "debug.check",
        "debug",
        "Fails the run with `check_failed` when a value or the element count is out of range",
        &[
            set("attr", T::Name, "\"\"", "Selector to check (empty = counts only)"),
            set("min", T::Float, "-1e30", "Lowest allowed value"),
            set("max", T::Float, "1e30", "Highest allowed value"),
            crate::nodes::num("min_count", T::Int, "-1", -1.0, 1.0e12, "Fewest elements (-1 = no bound)"),
            crate::nodes::num("max_count", T::Int, "-1", -1.0, 1.0e12, "Most elements (-1 = no bound)"),
        ],
        crate::nodes::pins_any_inout,
        eval_check,
    ),
    BuiltinNode::new(
        "debug.log",
        "debug",
        "Logs once per run ({len}, {items}, {attr:<sel>} placeholders) and passes data through; shipping builds skip the log",
        &[
            set("message", T::Name, "\"{items} items, {len} elements\"", "Message"),
            set("level", T::Enum(&["info", "warn"]), "info", "Level"),
        ],
        crate::nodes::pins_any_inout,
        eval_log,
    ),
    BuiltinNode::new(
        "debug.show",
        "debug",
        "Passes data through; the editor records it for the debug overlay",
        &[
            set("color", T::Any, "(1.0, 0.0, 1.0, 1.0)", "Overlay color"),
            set("as", T::Enum(&["points", "boxes", "axes"]), "points", "Overlay form"),
        ],
        crate::nodes::pins_any_inout,
        |_, inputs, _| Ok(vec![crate::nodes::input(inputs, 0)]),
    ),
    BuiltinNode::new(
        "graph.call",
        "graph",
        "Runs another graph inline; its declared inputs and outputs are this node's pins. Recursion is allowed: a call with every input empty does not run. With graph_from, each item of the first input pin picks its graph from an item-scope value",
        &[
            set("graph", T::Asset, "\"\"", "Called .pgraph.ron (with graph_from: the pin interface and the fallback)"),
            set("params", T::Any, "()", "Parameter overrides (name: value)"),
            set(
                "graph_from",
                T::Name,
                "\"\"",
                "Item-scope Asset or Name value (`item.graph` or `graph`) naming the graph per item of the first input pin; empty = off",
            ),
            set(
                "graphs",
                T::NameList,
                "[]",
                "Graphs graph_from may name: captured with the run (any other path fails with unknown_graph); each must declare the same pins as `graph`",
            ),
        ],
        |_| PinSpec::default(),
        eval_call,
    )
    .uncached(),
    BuiltinNode::new(
        "graph.loop",
        "graph",
        "Runs another graph once per item of the `each` pins; `carry` pins pass each iteration's output to the next",
        &[
            set("graph", T::Asset, "\"\"", "Body .pgraph.ron"),
            set("each", T::NameList, "[]", "Input pins iterated item by item"),
            set("carry", T::NameList, "[]", "Pins carried from one iteration to the next"),
            set("params", T::Any, "()", "Parameter overrides (name: value)"),
        ],
        |_| PinSpec::default(),
        eval_loop,
    )
    .uncached(),
    BuiltinNode::new(
        "graph.input",
        "graph",
        "Inside a called graph: the caller's bundle on the declared input pin `pin`",
        &[set("pin", T::Name, "\"in\"", "Graph input pin name")],
        |_| PinSpec::new(vec![], vec![pin_out("out", ANY)]),
        |ctx, _, s| Ok(vec![ctx.frame.graph_inputs.get(s.text("pin")).cloned().unwrap_or_default()]),
    )
    .uncached(),
    BuiltinNode::new(
        "graph.loop_index",
        "graph",
        "Inside a loop body: a one-row table `index` (Int) of the innermost loop",
        &[],
        |_| PinSpec::new(vec![], vec![pin_out("out", TABLE)]),
        eval_loop_index,
    )
    .uncached(),
    BuiltinNode::new(
        "graph.depth",
        "graph",
        "A one-row table `depth`: the graph.call nesting depth (0 at the generator's graph)",
        &[],
        |_| PinSpec::new(vec![], vec![pin_out("out", TABLE)]),
        |ctx, _, _| {
            let t = crate::data::Table::one_row([("depth".to_string(), crate::data::AttrValue::Int(ctx.frame.depth as i64))]);
            Ok(vec![Bundle::one(crate::data::DataItem::new(Payload::Table(t)))])
        },
    )
    .uncached(),
    BuiltinNode::new(
        "graph.output",
        "graph",
        "Collects its input as the graph output pin `pin`",
        &[set("pin", T::Name, "\"out\"", "Graph output pin name")],
        |_| PinSpec::new(vec![pin_req("in", ANY)], vec![]),
        eval_graph_output,
    )
    .uncached(),
];

/// The condition of `flow.if` / `flow.choose`: the `cond` pin's one-row table (column 0), else the
/// `cond` item-scope selector on the first input item, else `value`.
fn condition(inputs: &[Bundle], cond_pin: usize, data_pin: usize, s: &Settings) -> Result<bool, NodeError> {
    let pin = crate::nodes::input(inputs, cond_pin);
    if let Some(t) = pin.0.iter().find_map(|i| match &i.payload {
        Payload::Table(t) => Some(t),
        _ => None,
    }) {
        let v = t
            .columns
            .first()
            .and_then(|(_, c)| c.components(0))
            .and_then(|c| c.first().copied())
            .ok_or_else(|| {
                NodeError::new(
                    "attr_missing",
                    "the `cond` table needs a numeric or bool column 0",
                )
            })?;
        return Ok(v != 0.0);
    }
    let sel = s.text("cond");
    if !sel.is_empty() {
        let name = sel.strip_prefix("item.").unwrap_or(sel);
        let data = crate::nodes::input(inputs, data_pin);
        let Some(first) = data.0.first() else {
            return Ok(false);
        };
        let v = first
            .scope
            .get(name)
            .and_then(|v| v.components())
            .and_then(|c| c.first().copied())
            .ok_or_else(|| NodeError::new("attr_missing", format!("item has no numeric `{name}`")))?;
        return Ok(v != 0.0);
    }
    Ok(s.b("value"))
}

fn eval_if(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let c = condition(inputs, 1, 0, s)?;
    let data = crate::nodes::input(inputs, 0);
    if c {
        ctx.set_inactive(1);
        Ok(vec![data, Bundle::empty()])
    } else {
        ctx.set_inactive(0);
        Ok(vec![Bundle::empty(), data])
    }
}

fn eval_choose(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let c = condition(inputs, 2, 0, s)?;
    Ok(vec![crate::nodes::input(inputs, if c { 0 } else { 1 })])
}

/// The pin index of a quality tier in [`QUALITY_PINS`].
pub fn quality_index(t: QualityTier) -> usize {
    match t {
        QualityTier::MobileLow => 0,
        QualityTier::MobileHigh => 1,
        QualityTier::Deck => 2,
        // Very Low reads Low's pin (ADR-7882).
        QualityTier::Low | QualityTier::VeryLow => 3,
        QualityTier::Medium => 4,
        QualityTier::High => 5,
        QualityTier::Ultra => 6,
    }
}

fn eval_quality(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let want = quality_index(ctx.inputs.quality);
    let connected = |k: usize| ctx.connected.get(k).copied().unwrap_or(false);
    let pick = (0..=want)
        .rev()
        .find(|k| connected(*k))
        .or_else(|| (want + 1..QUALITY_PINS.len()).find(|k| connected(*k)));
    Ok(vec![
        pick.map(|k| crate::nodes::input(inputs, k)).unwrap_or_default(),
    ])
}

fn eval_check(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let data = crate::nodes::input(inputs, 0);
    let n = data.elements() as i64;
    let (min_c, max_c) = (s.i("min_count"), s.i("max_count"));
    if (min_c >= 0 && n < min_c) || (max_c >= 0 && n > max_c) {
        return Err(NodeError::new(
            "check_failed",
            format!("element count {n} is outside [{min_c}, {max_c}] (-1 = no bound)"),
        ));
    }
    let attr = s.text("attr");
    if !attr.is_empty() {
        let sel = crate::data::Selector::parse(attr).map_err(|m| NodeError::new("setting_type", m))?;
        let (lo, hi) = (s.f("min"), s.f("max"));
        for item in &data.0 {
            let v = read_num(item, &sel)?;
            if let Some(i) = v.values.iter().position(|x| *x < lo || *x > hi) {
                return Err(NodeError::new(
                    "check_failed",
                    format!(
                        "`{attr}` = {} at element {} is outside [{lo}, {hi}]",
                        v.values[i],
                        i / v.width.max(1)
                    ),
                ));
            }
        }
    }
    Ok(vec![data])
}

/// Whether `debug.log` writes its line: not in shipping builds (spec 63 § 12), which are release
/// builds without the `pipeline` feature (the game runtime `regia package` builds).
pub const DEBUG_LOG_ENABLED: bool = cfg!(debug_assertions) || cfg!(feature = "pipeline");

/// `debug.log`'s message with its placeholders filled: `{len}` (elements), `{items}` and
/// `{attr:<sel>}`, the selector's value at the first element of the first item (item scope with
/// `item.`): numbers as `attr.string` writes them (up to 6 decimals, components joined by `,`),
/// names and assets as text, `-` when there is no element, `?` when the selector cannot be read.
pub fn debug_log_message(message: &str, data: &Bundle) -> String {
    let mut msg = message
        .replace("{len}", &data.elements().to_string())
        .replace("{items}", &data.0.len().to_string());
    let mut from = 0;
    while let Some(start) = msg[from..].find("{attr:").map(|k| k + from) {
        let Some(end) = msg[start..].find('}').map(|k| k + start) else {
            break;
        };
        let text = attr_text(data, &msg[start + "{attr:".len()..end]);
        msg.replace_range(start..=end, &text);
        from = start + text.len();
    }
    msg
}

fn attr_text(data: &Bundle, sel: &str) -> String {
    use crate::data::{AttrColumn, AttrValue};
    let Some(item) = data.0.first() else {
        return "-".to_string();
    };
    let name = sel.split(':').next().unwrap_or(sel);
    if let Some(n) = name.strip_prefix("item.")
        && let Some(AttrValue::Name(t) | AttrValue::Asset(t)) = item.scope.get(n)
    {
        return t.clone();
    }
    let column = match &item.payload {
        Payload::Points(p) => p.attrs.get(name),
        Payload::Table(t) => t.column(name),
        _ => None,
    };
    if let Some(AttrColumn::Name(v) | AttrColumn::Asset(v)) = column {
        return v.first().cloned().unwrap_or_else(|| "-".to_string());
    }
    match crate::data::Selector::parse(sel)
        .ok()
        .and_then(|s| read_num(item, &s).ok())
    {
        Some(v) if v.len() > 0 => v
            .get(0)
            .iter()
            .map(|x| crate::nodes::extra::num_text(*x))
            .collect::<Vec<_>>()
            .join(","),
        Some(_) => "-".to_string(),
        None => "?".to_string(),
    }
}

fn eval_log(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let data = crate::nodes::input(inputs, 0);
    if !DEBUG_LOG_ENABLED {
        return Ok(vec![data]);
    }
    let msg = debug_log_message(s.text("message"), &data);
    if s.text("level") == "warn" {
        tracing::warn!(target: "regia_procgen", node = ctx.node_id, "{msg}");
    } else {
        tracing::info!(target: "regia_procgen", node = ctx.node_id, "{msg}");
    }
    Ok(vec![data])
}

fn eval_graph_output(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let pin = s.text("pin").to_string();
    let data = crate::nodes::input(inputs, 0);
    let slot = ctx.output().graph_outputs.entry(pin).or_default();
    slot.0.extend(data.0);
    Ok(vec![])
}

/// Parameter overrides from a `params` setting (`(name: value, ...)` or `{ "name": value }`).
fn overrides(s: &Settings) -> BTreeMap<String, SettingValue> {
    s.get("params").map(crate::graph::call_params).unwrap_or_default()
}

/// A bundle carries no data: no items, or only point sets and tables with no elements (regions
/// such as shapes and ground count as data).
fn no_data(b: &Bundle) -> bool {
    b.0.iter()
        .all(|i| matches!(&i.payload, Payload::Points(_) | Payload::Table(_)) && i.payload.is_empty())
}

/// The called graph's declared input and output pin names.
fn declared(ctx: &NodeCtx<'_>, path: &str) -> Result<(Vec<String>, Vec<String>), NodeError> {
    let text = ctx
        .inputs
        .graphs
        .get(path)
        .ok_or_else(|| NodeError::new("unknown_graph", format!("graph `{path}` was not captured")))?;
    let g = crate::graph::ProcGraph::from_ron(text)
        .map_err(|e| NodeError::new("procgen_graph_invalid", e.to_string()))?;
    Ok((
        g.inputs.iter().map(|p| p.name.clone()).collect(),
        g.outputs.iter().map(|p| p.name.clone()).collect(),
    ))
}

fn eval_call(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let path = s.text("graph").to_string();
    let (ins, outs) = declared(ctx, &path)?;
    // A call whose every input is empty does not run (the recursion terminator, spec 63 § 11).
    if !ins.is_empty() && inputs.iter().all(no_data) {
        return Ok(vec![Bundle::empty(); outs.len()]);
    }
    let from = s.text("graph_from");
    if !from.is_empty() {
        return eval_call_from(ctx, inputs, s, (&path, &ins, &outs), from);
    }
    let graph_inputs = ins.iter().cloned().zip(inputs.iter().cloned()).collect();
    let r = ctx.run_subgraph(&path, &overrides(s), graph_inputs, None)?;
    Ok(outs
        .iter()
        .map(|o| r.output.graph_outputs.get(o).cloned().unwrap_or_default())
        .collect())
}

/// `graph.call` with `graph_from` (spec 63 § 11, ADR-3027): the items of the first input pin are
/// grouped by the graph their item-scope value names (items without one use `graph`), in order of
/// first appearance; each group runs its graph once with the group on the first pin and the other
/// pins whole, and the outputs are concatenated in group order. Each distinct graph compiles once per
/// run (the subgraph cache).
fn eval_call_from(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
    (fallback, ins, outs): (&str, &[String], &[String]),
    from: &str,
) -> Result<Vec<Bundle>, NodeError> {
    if ins.is_empty() {
        return Err(NodeError::new(
            "setting_type",
            format!("`graph_from` needs the called graph `{fallback}` to declare an input pin"),
        ));
    }
    let key = from.strip_prefix("item.").unwrap_or(from);
    let mut groups: Vec<(String, Bundle)> = Vec::new();
    for item in crate::nodes::input_items(inputs, 0) {
        let path = match item.scope.get(key) {
            Some(crate::data::AttrValue::Asset(p) | crate::data::AttrValue::Name(p)) if !p.is_empty() => {
                p.clone()
            }
            _ => fallback.to_string(),
        };
        match groups.iter_mut().find(|(p, _)| *p == path) {
            Some((_, b)) => b.0.push(item.clone()),
            None => groups.push((path, Bundle::one(item.clone()))),
        }
    }
    let params = overrides(s);
    // The `graphs` list is this node's allow-list: a path another node captured must not run here.
    let allowed = s.names("graphs");
    let mut gathered = vec![Bundle::empty(); outs.len()];
    for (path, items) in groups {
        ctx.check_cancel()?;
        if path != fallback && !allowed.contains(&path) {
            return Err(NodeError::new(
                "unknown_graph",
                format!("graph `{path}` (from `{key}`) is not in this node's `graphs` list"),
            ));
        }
        let (gi_names, go_names) = declared(ctx, &path)?;
        if gi_names != ins || go_names != outs {
            return Err(NodeError::new(
                "call_pins_mismatch",
                format!("graph `{path}` (from `{key}`) does not declare the pins of `{fallback}`"),
            ));
        }
        let mut gi: BTreeMap<String, Bundle> = BTreeMap::new();
        for (k, name) in ins.iter().enumerate() {
            let b = if k == 0 {
                items.clone()
            } else {
                inputs.get(k).cloned().unwrap_or_default()
            };
            gi.insert(name.clone(), b);
        }
        let r = ctx.run_subgraph(&path, &params, gi, None)?;
        for (slot, o) in gathered.iter_mut().zip(outs) {
            if let Some(b) = r.output.graph_outputs.get(o) {
                slot.0.extend(b.0.iter().cloned());
            }
        }
    }
    Ok(gathered)
}

fn eval_loop(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let path = s.text("graph").to_string();
    let (ins, outs) = declared(ctx, &path)?;
    let (each, carry) = (s.names("each"), s.names("carry"));
    let by_name: BTreeMap<&str, &Bundle> = ins.iter().map(String::as_str).zip(inputs.iter()).collect();
    let mut count: Option<usize> = None;
    for e in &each {
        let n = by_name.get(e.as_str()).map_or(0, |b| b.0.len());
        if count.is_some_and(|c| c != n) {
            return Err(NodeError::new(
                "loop_count_mismatch",
                "every `each` pin needs the same item count",
            ));
        }
        count = Some(n);
    }
    let count = count.unwrap_or(0);
    if count > ctx.limits().loop_iterations {
        return Err(NodeError::new(
            "limit_exceeded",
            format!("loop_iterations: {count} > {}", ctx.limits().loop_iterations),
        ));
    }
    let mut carried: BTreeMap<String, Bundle> = carry
        .iter()
        .map(|c| {
            (
                c.clone(),
                by_name.get(c.as_str()).map(|b| (*b).clone()).unwrap_or_default(),
            )
        })
        .collect();
    let mut gathered: BTreeMap<String, Bundle> = BTreeMap::new();
    let params = overrides(s);
    // Every iteration may legally return a full item, so the gather is budgeted as it grows: the
    // parent frame's own check only runs after this node has returned.
    let (mut held_elements, mut held_bytes) = (0usize, 0usize);
    for k in 0..count {
        ctx.check_cancel()?;
        let mut gi = BTreeMap::new();
        for name in &ins {
            let b = if each.contains(name) {
                by_name
                    .get(name.as_str())
                    .and_then(|b| b.0.get(k).cloned())
                    .map(Bundle::one)
                    .unwrap_or_default()
            } else if let Some(c) = carried.get(name) {
                c.clone()
            } else {
                by_name
                    .get(name.as_str())
                    .map(|b| (*b).clone())
                    .unwrap_or_default()
            };
            gi.insert(name.clone(), b);
        }
        let r = ctx.run_subgraph(&path, &params, gi, Some(k as i64))?;
        for o in &outs {
            let b = r.output.graph_outputs.get(o).cloned().unwrap_or_default();
            if carry.contains(o) {
                carried.insert(o.clone(), b);
            } else {
                held_elements = held_elements.saturating_add(b.elements());
                held_bytes = held_bytes.saturating_add(crate::exec::estimate_bytes(&b));
                let limits = ctx.limits();
                if held_elements > limits.total_elements {
                    return Err(NodeError::new(
                        "limit_exceeded",
                        format!("total_elements: {held_elements} > {}", limits.total_elements),
                    ));
                }
                if held_bytes > limits.memory_bytes {
                    return Err(NodeError::new(
                        "limit_exceeded",
                        format!("memory_bytes: {held_bytes} > {}", limits.memory_bytes),
                    ));
                }
                gathered.entry(o.clone()).or_default().0.extend(b.0);
            }
        }
    }
    Ok(outs
        .iter()
        .map(|o| {
            if carry.contains(o) {
                carried.get(o).cloned().unwrap_or_default()
            } else {
                gathered.get(o).cloned().unwrap_or_default()
            }
        })
        .collect())
}

fn eval_loop_index(ctx: &mut NodeCtx<'_>, _: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let Some(i) = ctx.frame.loop_index.last().copied() else {
        return Err(NodeError::new(
            "not_in_loop",
            "graph.loop_index outside a loop body",
        ));
    };
    let t = crate::data::Table::one_row([("index".to_string(), crate::data::AttrValue::Int(i))]);
    Ok(vec![Bundle::one(crate::data::DataItem::new(Payload::Table(t)))])
}
