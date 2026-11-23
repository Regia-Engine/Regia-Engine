// SPDX-License-Identifier: MPL-2.0
//! Items and tags (`items.*`, spec 63 § 8).

use crate::data::{AttrColumn, Bundle, DataItem, DataKind, Payload, Table};
use crate::exec::NodeCtx;
use crate::nodes::common::read_num;
use crate::nodes::{
    ANY, BuiltinNode, NodeError, PinSpec, SettingType as T, Settings, TABLE, pin_multi, pin_out, pin_req,
    pins_any_inout, pins_split, set,
};

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::in_place(
        "items.tag",
        "items",
        "Edits tags in this order: rename, remove, keep_only (when not empty), add",
        &[
            set("add", T::NameList, "[]", "Tags to add"),
            set("remove", T::NameList, "[]", "Tags to remove"),
            set(
                "keep_only",
                T::NameList,
                "[]",
                "Keep only these tags (when not empty)",
            ),
            set("rename", T::Any, "[]", "[(from, to)] pairs"),
        ],
        pins_any_inout,
        eval_tag,
    ),
    BuiltinNode::new(
        "items.by_tag",
        "items",
        "Items with any (or all) of the tags go to `inside`",
        &[
            set("tags", T::NameList, "[]", "Tags"),
            set("all", T::Bool, "false", "Require all tags"),
        ],
        |_| pins_split(ANY),
        eval_by_tag,
    ),
    BuiltinNode::new(
        "items.by_index",
        "items",
        "Items selected by a Rust-style index list (\"0, 2, 4..6, 7.., -1\") go to `inside`",
        &[set("select", T::Name, "\"0\"", "Index list")],
        |_| pins_split(ANY),
        eval_by_index,
    )
    .checked(check_index),
    BuiltinNode::new(
        "items.by_kind",
        "items",
        "Items of the listed kinds go to `inside`",
        &[set("kinds", T::NameList, "[\"Points\"]", "Kind names")],
        |_| pins_split(ANY),
        eval_by_kind,
    )
    .checked(check_kinds),
    BuiltinNode::new(
        "items.count",
        "items",
        "One table row per input edge: `items` and `elements`",
        &[],
        |_| PinSpec::new(vec![pin_multi("in", ANY)], vec![pin_out("out", TABLE)]),
        eval_count,
    ),
    BuiltinNode::new(
        "items.gather",
        "items",
        "Concatenates `in` bundles in edge order; `after` edges only order execution",
        &[],
        |_| {
            PinSpec::new(
                vec![pin_multi("in", ANY), pin_multi("after", ANY)],
                vec![pin_out("out", ANY)],
            )
        },
        eval_gather,
    ),
    BuiltinNode::in_place(
        "items.sort",
        "items",
        "Sorts elements (not items) by the keys; stable, ties keep input order",
        &[
            set("by", T::NameList, "[]", "Selectors (scalars)"),
            set("descending", T::Bool, "false", "Descending"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req(
                    "in",
                    crate::data::KindSet::of(&[DataKind::Points, DataKind::Table]),
                )],
                vec![pin_out(
                    "out",
                    crate::data::KindSet::of(&[DataKind::Points, DataKind::Table]),
                )],
            )
        },
        eval_sort,
    ),
];

fn eval_tag(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let rename: Vec<(String, String)> = s
        .get("rename")
        .and_then(|v| v.as_items())
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
        .unwrap_or_default();
    let (add, remove, keep) = (s.names("add"), s.names("remove"), s.names("keep_only"));
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        for (from, to) in &rename {
            if item.tags.remove(from) {
                item.tags.insert(to.clone());
            }
        }
        for r in &remove {
            item.tags.remove(r);
        }
        if !keep.is_empty() {
            item.tags.retain(|t| keep.contains(t));
        }
        item.tags.extend(add.iter().cloned());
    }
    Ok(vec![out])
}

fn route(inputs: &[Bundle], mut pick: impl FnMut(usize, &DataItem) -> bool) -> Vec<Bundle> {
    let (mut a, mut b) = (Bundle::empty(), Bundle::empty());
    for (k, item) in crate::nodes::input(inputs, 0).0.into_iter().enumerate() {
        if pick(k, &item) {
            a.0.push(item);
        } else {
            b.0.push(item);
        }
    }
    vec![a, b]
}

fn eval_by_tag(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let tags = s.names("tags");
    let all = s.b("all");
    Ok(route(inputs, |_, i| {
        if all {
            tags.iter().all(|t| i.tags.contains(t))
        } else {
            tags.iter().any(|t| i.tags.contains(t))
        }
    }))
}

/// Parses an index list over `n` items (spec 63 § 8): single indices, `a..b`, `a..=b`, `a..`;
/// negative numbers count from the end. Returns the selected indices in ascending order and whether
/// any entry was a duplicate or out of range.
///
/// Ranges are clipped to `0..n` arithmetically and merged, so the work is bounded by the entry count
/// and `n`, never by the numbers written (`0..99999999999` over 10 items costs 10 steps).
pub fn parse_index_list(text: &str, n: usize) -> Result<(Vec<usize>, bool), String> {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    let fix = |v: i64| if v < 0 { n.saturating_add(v) } else { v };
    let num = |t: &str| {
        t.trim()
            .parse::<i64>()
            .map_err(|_| format!("`{t}` is not an index"))
    };
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    let mut odd = false;
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (lo, hi) = if let Some((a, b)) = part.split_once("..=") {
            (fix(num(a)?), fix(num(b)?).saturating_add(1))
        } else if let Some((a, b)) = part.split_once("..") {
            let hi = if b.trim().is_empty() { n } else { fix(num(b)?) };
            (fix(num(a)?), hi)
        } else {
            let v = fix(num(part)?);
            (v, v.saturating_add(1))
        };
        if lo >= hi {
            continue;
        }
        let (clo, chi) = (lo.max(0), hi.min(n));
        if clo != lo || chi != hi {
            odd = true;
        }
        if clo < chi {
            ranges.push((clo, chi));
        }
    }
    // Overlapping ranges are duplicates; the union is the selection.
    ranges.sort_unstable();
    let mut sel = Vec::new();
    let mut end = 0i64;
    for (lo, hi) in ranges {
        if lo < end {
            odd = true;
        }
        let from = lo.max(end);
        if from < hi {
            sel.extend((from..hi).filter_map(|i| usize::try_from(i).ok()));
            end = hi;
        }
    }
    Ok((sel, odd))
}

fn check_index(s: &Settings) -> Vec<(String, String)> {
    match parse_index_list(s.text("select"), 0) {
        Ok(_) => Vec::new(),
        Err(m) => vec![("setting_type".into(), m)],
    }
}

fn eval_by_index(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let n = crate::nodes::input_items(inputs, 0).len();
    let (sel, odd) = parse_index_list(s.text("select"), n).map_err(|m| NodeError::new("setting_type", m))?;
    if odd {
        ctx.warn("index_ignored", "duplicate or out-of-range indices were ignored");
    }
    Ok(route(inputs, |k, _| sel.binary_search(&k).is_ok()))
}

fn check_kinds(s: &Settings) -> Vec<(String, String)> {
    s.names("kinds")
        .iter()
        .filter(|k| DataKind::from_name(k).is_none())
        .map(|k| ("setting_type".to_string(), format!("unknown kind `{k}`")))
        .collect()
}

fn eval_by_kind(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let kinds: Vec<DataKind> = s
        .names("kinds")
        .iter()
        .filter_map(|k| DataKind::from_name(k))
        .collect();
    Ok(route(inputs, |_, i| kinds.contains(&i.payload.kind())))
}

fn eval_count(_: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    // One row for the (multi) input pin; edges are already concatenated in edge order.
    let b = crate::nodes::input(inputs, 0);
    let mut t = Table {
        rows: 1,
        columns: Vec::new(),
    };
    t.set_column("items", AttrColumn::Int(vec![b.0.len() as i64].into()));
    t.set_column("elements", AttrColumn::Int(vec![b.elements() as i64].into()));
    Ok(vec![Bundle::one(DataItem::new(Payload::Table(t)))])
}

fn eval_gather(_: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    Ok(vec![crate::nodes::input(inputs, 0)])
}

fn eval_sort(_: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let keys = s.names("by");
    let desc = s.b("descending");
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let cols = keys
            .iter()
            .map(|k| {
                let sel = crate::data::Selector::parse(k).map_err(|m| NodeError::new("setting_type", m))?;
                read_num(item, &sel)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let n = item.payload.len();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            let mut o = std::cmp::Ordering::Equal;
            for c in &cols {
                o = o.then(
                    c.get(a)
                        .iter()
                        .zip(c.get(b))
                        .fold(std::cmp::Ordering::Equal, |acc, (x, y)| acc.then(x.total_cmp(y))),
                );
            }
            if desc { o.reverse() } else { o }
        });
        item.payload = match &item.payload {
            Payload::Points(p) => Payload::Points(p.select(&idx)),
            Payload::Table(t) => Payload::Table(t.select(&idx)),
            other => other.clone(),
        };
    }
    Ok(vec![out])
}
