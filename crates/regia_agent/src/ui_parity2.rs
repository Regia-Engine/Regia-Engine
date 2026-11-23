// SPDX-License-Identifier: MPL-2.0
//! `ui.text_style` and `ui.material` (ADR-273 T07/T09, ADR-2045; books `agents/ui-text-style.md`,
//! `agents/ui-material.md`), plus the drawn tree, data grid and color picker widget methods
//! (`ui.tree_view`, `ui.data_grid`, `ui.color_picker_widget`; W10/W11).
//!
//! Every method validates all parameters before it changes anything, so a failed call leaves
//! the world as it was.

use regia_ecs::prelude::*;
use regia_ui::color::UiColor;
use regia_ui::extract::{UiDamage, UiDamageSet};
use regia_ui::parity_widgets::{DataGridView, ProgressDialog, SearchBox, header_label, search_results};
use regia_ui::sdf_text::SdfText;
use regia_ui::text::{ShapedText, Text};
use regia_ui::text_effects::{
    MAX_FONT_FEATURES, MAX_TEXT_EFFECTS, TEXT_EFFECT_MAX_PX, TextEffect, parse_feature,
};
use regia_ui::tree::{DataGrid, GridColumn};
use regia_ui::ui_material::{UiMaterial, UiMaterialKind, material_mix};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn error(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

pub(crate) fn entity_of(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::params::opt_u64(params, "entity")?
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_entity",
                "`entity` must be entity bits from ui.query",
                "take entity ids from a ui.query result",
            )
        })?;
    if world.get_entity(entity).is_err() {
        return Err(error(
            NOT_FOUND,
            "unknown_entity",
            "the entity does not exist",
            "take entity ids from a ui.query result",
        ));
    }
    Ok(entity)
}

pub(crate) fn color_of(v: &Value, field: &str) -> Result<UiColor, RpcError> {
    let bad = || {
        error(
            INVALID_PARAMS,
            "bad_color",
            format!("`{field}` must be \"#RRGGBB\", \"#RRGGBBAA\" or [r, g, b, a] linear floats"),
            "colors are sRGB hex strings or linear RGBA arrays",
        )
    };
    match v {
        Value::String(s) => UiColor::from_hex(s).ok_or_else(bad),
        Value::Array(a) if a.len() == 4 => {
            let c: Option<Vec<f32>> = a
                .iter()
                .map(|x| x.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
                .collect();
            c.map(|c| UiColor([c[0], c[1], c[2], c[3]])).ok_or_else(bad)
        }
        _ => Err(bad()),
    }
}

pub(crate) fn number(v: &Value, field: &str, min: f64, max: f64) -> Result<f32, RpcError> {
    v.as_f64()
        .filter(|n| n.is_finite() && *n >= min && *n <= max)
        .map(|n| n as f32)
        .ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_param",
                format!("`{field}` must be a number from {min} to {max}"),
                "see the method's guide page for ranges",
            )
        })
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(error(
            INVALID_PARAMS,
            "bad_param",
            format!("`{name}` must be true or false"),
            "pass a JSON boolean",
        )),
    }
}

/// Asks extract for a full redraw (the next frame rebuilds every quad).
pub(crate) fn redraw(world: &mut World) {
    world.get_resource_or_insert_with(UiDamageSet::default).full = true;
    world.get_resource_or_insert_with(UiDamage::default).0 = true;
}

fn effect_of(v: &Value) -> Result<TextEffect, RpcError> {
    let bad = |msg: &str| {
        error(
            INVALID_PARAMS,
            "bad_effect",
            msg.to_owned(),
            "an effect is { color, width?, blur?, offset?: [x, y] } in UI px; width and blur 0 to 8",
        )
    };
    let obj = v
        .as_object()
        .ok_or_else(|| bad("each effect must be an object"))?;
    let mut e = TextEffect::default();
    for (key, value) in obj {
        match key.as_str() {
            "color" => e.color = color_of(value, "effects[].color")?,
            "width" => {
                e.width = number(value, "effects[].width", 0.0, f64::from(TEXT_EFFECT_MAX_PX))
                    .map_err(|_| bad("`width` must be 0 to 8"))?
            }
            "blur" => {
                e.blur = number(value, "effects[].blur", 0.0, f64::from(TEXT_EFFECT_MAX_PX))
                    .map_err(|_| bad("`blur` must be 0 to 8"))?
            }
            "offset" => {
                let o = value
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .and_then(|a| {
                        a.iter()
                            .map(|x| x.as_f64().filter(|f| f.is_finite() && f.abs() <= 256.0))
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok_or_else(|| bad("`offset` must be [x, y] with |x|, |y| <= 256"))?;
                e.offset = [o[0] as f32, o[1] as f32];
            }
            other => return Err(bad(&format!("unknown effect field `{other}`"))),
        }
    }
    Ok(e)
}

fn effect_json(e: &TextEffect) -> Value {
    json!({ "color": e.color.0, "offset": e.offset, "width": e.width, "blur": e.blur })
}

/// `ui.text_style { entity, span?, letter_spacing?, weight?, underline?, strikethrough?,
/// features?, effects? }`.
pub(crate) fn text_style(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let Some(text) = world.get::<Text>(entity) else {
        return Err(error(
            NOT_FOUND,
            "no_text",
            "the node has no Text",
            "pick a label or other text node from ui.query",
        ));
    };
    let span_count = text.spans.len();
    let span = match params.get("span") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .map(|n| n as usize)
                .filter(|n| *n < span_count)
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_span",
                        format!("`span` must be a span index below {span_count}"),
                        "omit `span` to edit every span",
                    )
                })?,
        ),
    };
    // Parse everything first: a failed call changes nothing.
    let letter_spacing = match params.get("letter_spacing") {
        None | Some(Value::Null) => None,
        Some(v) => Some(number(v, "letter_spacing", -1.0, 4.0)?),
    };
    let weight = match params.get("weight") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(Some(
            v.as_u64()
                .filter(|n| (1..=1000).contains(n))
                .map(|n| n as u16)
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_param",
                        "`weight` must be a whole number from 1 to 1000, or null",
                        "400 is regular, 700 bold; null goes back to the bold flag",
                    )
                })?,
        )),
    };
    let underline = flag(params, "underline")?;
    let strikethrough = flag(params, "strikethrough")?;
    let sdf = flag(params, "sdf")?;
    let features = match params.get("features") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let list = v
                .as_array()
                .filter(|a| a.len() <= MAX_FONT_FEATURES)
                .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>())
                .filter(|l| l.iter().all(|f| parse_feature(f).is_some()))
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_feature",
                        format!("`features` must be at most {MAX_FONT_FEATURES} strings like \"smcp\" or \"liga=0\""),
                        "a feature is a four-character OpenType tag with an optional =value",
                    )
                })?;
            Some(list)
        }
    };
    let effects = match params.get("effects") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) if a.len() <= MAX_TEXT_EFFECTS => {
            Some(a.iter().map(effect_of).collect::<Result<Vec<_>, _>>()?)
        }
        Some(_) => {
            return Err(error(
                INVALID_PARAMS,
                "bad_effect",
                format!("`effects` must be an array of at most {MAX_TEXT_EFFECTS} effects"),
                "an effect is { color, width?, blur?, offset? }",
            ));
        }
    };
    match sdf {
        Some(true) if world.get::<SdfText>(entity).is_none() => {
            world.entity_mut(entity).insert(SdfText);
            redraw(world);
        }
        Some(false) if world.get::<SdfText>(entity).is_some() => {
            world.entity_mut(entity).remove::<SdfText>();
            redraw(world);
        }
        _ => {}
    }
    let changed = sdf.is_some()
        || letter_spacing.is_some()
        || weight.is_some()
        || underline.is_some()
        || strikethrough.is_some()
        || features.is_some()
        || effects.is_some();
    if changed && let Some(mut text) = world.get_mut::<Text>(entity) {
        for (index, s) in text.spans.iter_mut().enumerate() {
            if span.is_some_and(|only| only != index) {
                continue;
            }
            let st = &mut s.style;
            if let Some(v) = letter_spacing {
                st.letter_spacing = v;
            }
            if let Some(v) = weight {
                st.weight = v;
            }
            if let Some(v) = underline {
                st.underline = v;
            }
            if let Some(v) = strikethrough {
                st.strikethrough = v;
            }
            if let Some(v) = &features {
                st.features.clone_from(v);
            }
            if let Some(v) = &effects {
                st.effects.clone_from(v);
            }
        }
    }
    let text = world.get::<Text>(entity);
    let spans: Vec<Value> = text
        .map(|t| {
            t.spans
                .iter()
                .map(|s| {
                    let st = &s.style;
                    json!({
                        "letter_spacing": st.letter_spacing,
                        "weight": st.weight,
                        "bold": st.bold,
                        "underline": st.underline,
                        "strikethrough": st.strikethrough,
                        "features": st.features,
                        "effects": st.effects.iter().map(effect_json).collect::<Vec<_>>(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // The last shape (one frame old after an edit: shaping runs in PostUpdate).
    let shaped = world.get::<ShapedText>(entity).map(|s| {
        json!({
            "glyphs": s.glyphs.len(),
            "size": s.size,
            "decorations": s.decorations.iter().map(|d| json!({
                "rect": d.rect,
                "span": d.span,
                "kind": if d.strike { "strikethrough" } else { "underline" },
            })).collect::<Vec<_>>(),
        })
    });
    Ok(json!({
        "entity": entity.to_bits(),
        "changed": changed,
        "sdf": world.get::<SdfText>(entity).is_some(),
        "spans": spans,
        "shaped": shaped,
    }))
}

fn material_json(m: &UiMaterial) -> Value {
    json!({
        "kind": m.kind.name(),
        "color": m.color.0,
        "color2": m.color2.0,
        "params": m.params,
        "time": m.time,
        "animate": m.animate,
    })
}

/// `ui.material { entity, kind?, color?, color2?, params?, time?, animate?, remove?, probe? }`.
pub(crate) fn material(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let kinds: Vec<&str> = UiMaterialKind::ALL.iter().map(|k| k.name()).collect();
    let kind = match params.get("kind") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().and_then(UiMaterialKind::from_name).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_material",
                "`kind` is not a material kind",
                format!("use one of: {}", kinds.join(", ")),
            )
        })?),
    };
    let color = params.get("color").map(|v| color_of(v, "color")).transpose()?;
    let color2 = params.get("color2").map(|v| color_of(v, "color2")).transpose()?;
    let mparams = match params.get("params") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_array()
                .filter(|a| a.len() == 4)
                .and_then(|a| {
                    a.iter()
                        .map(|x| {
                            x.as_f64()
                                .filter(|f| f.is_finite() && f.abs() <= 1.0e6)
                                .map(|f| f as f32)
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .map(|a| [a[0], a[1], a[2], a[3]])
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_param",
                        "`params` must be four finite numbers",
                        "the kind's guide row lists what each number means",
                    )
                })?,
        ),
    };
    let time = match params.get("time") {
        None | Some(Value::Null) => None,
        Some(v) => Some(number(v, "time", 0.0, 3600.0)?),
    };
    let animate = flag(params, "animate")?;
    let remove = flag(params, "remove")?.unwrap_or(false);
    let probe = match params.get("probe") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_array()
                .filter(|a| a.len() == 2)
                .and_then(|a| a.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
                .filter(|a| a.iter().all(|f| f.is_finite()))
                .map(|a| [a[0] as f32, a[1] as f32])
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_param",
                        "`probe` must be [x, y] in px from the node's top-left",
                        "probe a pixel to read the color mix the shader computes there",
                    )
                })?,
        ),
    };
    if remove {
        if world.entity_mut(entity).take::<UiMaterial>().is_some() {
            redraw(world);
        }
        return Ok(json!({ "entity": entity.to_bits(), "material": Value::Null, "kinds": kinds }));
    }
    let edits = kind.is_some()
        || color.is_some()
        || color2.is_some()
        || mparams.is_some()
        || time.is_some()
        || animate.is_some();
    if edits {
        let mut m = world.get::<UiMaterial>(entity).copied().unwrap_or_default();
        if let Some(k) = kind
            && k != m.kind
        {
            // A new kind starts from its own parameters unless the call sets them.
            m.kind = k;
            m.params = k.default_params();
            m.animate = k.uses_time();
        }
        if let Some(c) = color {
            m.color = c;
        }
        if let Some(c) = color2 {
            m.color2 = c;
        }
        if let Some(p) = mparams {
            m.params = p;
        }
        if let Some(t) = time {
            m.time = t;
        }
        if let Some(a) = animate {
            m.animate = a;
        }
        world.entity_mut(entity).insert(m);
    }
    let m = world.get::<UiMaterial>(entity).copied();
    let mix = match (m, probe) {
        (Some(m), Some(p)) => {
            let size = world
                .get::<regia_ui::node::ComputedNode>(entity)
                .map_or([0.0, 0.0], |c| [c.rect[2], c.rect[3]]);
            material_mix(&m, p, size).map_or(Value::Null, |v| json!(v))
        }
        _ => Value::Null,
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "material": m.as_ref().map(material_json),
        "mix": mix,
        "kinds": kinds,
    }))
}

/// Most rows `ui.data_grid` accepts in one `set`.
const MAX_GRID_ROWS: usize = 100_000;
/// Most columns of one grid.
const MAX_GRID_COLUMNS: usize = 64;

fn grid_error(message: impl Into<String>) -> RpcError {
    error(
        INVALID_PARAMS,
        "bad_grid",
        message,
        "columns are [{ title, width }] (width 8 to 2000 UI px); rows are arrays of strings",
    )
}

fn grid_columns(v: &Value) -> Result<Vec<GridColumn>, RpcError> {
    let list = v
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= MAX_GRID_COLUMNS)
        .ok_or_else(|| grid_error(format!("`columns` must be 1 to {MAX_GRID_COLUMNS} columns")))?;
    list.iter()
        .map(|c| {
            let title = crate::params::opt_str(c, "title")?.filter(|t| t.len() <= 256);
            let width =
                crate::params::opt_f64(c, "width")?.filter(|w| w.is_finite() && (8.0..=2000.0).contains(w));
            match (title, width) {
                (Some(t), Some(w)) => Ok(GridColumn {
                    title: t.to_owned(),
                    width: w as f32,
                }),
                _ => Err(grid_error(
                    "each column needs a `title` and a `width` from 8 to 2000",
                )),
            }
        })
        .collect()
}

fn grid_rows(v: &Value) -> Result<Vec<Vec<String>>, RpcError> {
    let list = v
        .as_array()
        .filter(|a| a.len() <= MAX_GRID_ROWS)
        .ok_or_else(|| grid_error(format!("`rows` must be at most {MAX_GRID_ROWS} rows")))?;
    list.iter()
        .map(|r| {
            r.as_array()
                .filter(|a| a.len() <= MAX_GRID_COLUMNS)
                .and_then(|a| {
                    a.iter()
                        .map(|c| c.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| grid_error("each row must be an array of strings"))
        })
        .collect()
}

/// `ui.data_grid { entity, action?: "query" | "set" | "sort", columns?, rows?, column?, offset?, limit? }`.
pub(crate) fn data_grid(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    // A drawn grid answers on its outer node too.
    let body = world.get::<DataGridView>(entity).map_or(entity, |v| v.body);
    let action = crate::params::opt_str(params, "action")?.unwrap_or("query");
    let offset = match params.get("offset") {
        None | Some(Value::Null) => 0,
        Some(v) => number(v, "offset", 0.0, MAX_GRID_ROWS as f64)? as usize,
    };
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 100,
        Some(v) => number(v, "limit", 0.0, 500.0)? as usize,
    };
    match action {
        "query" => {}
        "set" => {
            let columns = match params.get("columns") {
                None | Some(Value::Null) => None,
                Some(v) => Some(grid_columns(v)?),
            };
            let rows = match params.get("rows") {
                None | Some(Value::Null) => None,
                Some(v) => Some(grid_rows(v)?),
            };
            if columns.is_none() && world.get::<DataGrid>(body).is_none() {
                return Err(grid_error("a new grid needs `columns`"));
            }
            let mut grid = world.get::<DataGrid>(body).cloned().unwrap_or_default();
            if let Some(c) = columns {
                if c.len() != grid.columns.len() {
                    grid.sort = None;
                }
                grid.columns = c;
            }
            if let Some(r) = rows {
                grid.rows = r;
            }
            if world.get::<regia_ui::parity_widgets::GridBody>(body).is_some()
                && world
                    .get::<DataGrid>(body)
                    .is_some_and(|g| g.columns.len() != grid.columns.len())
            {
                return Err(grid_error(
                    "a drawn grid keeps its column count; set rows only or the same number of columns",
                ));
            }
            world.entity_mut(body).insert(grid);
        }
        "sort" => {
            let column = crate::params::opt_u64(params, "column")?.map(|c| c as usize);
            let Some(mut grid) = world.get_mut::<DataGrid>(body) else {
                return Err(error(
                    NOT_FOUND,
                    "no_grid",
                    "the node has no data grid",
                    "create one with action \"set\"",
                ));
            };
            let ok = column.is_some_and(|c| grid.sort_by(c));
            if !ok {
                return Err(grid_error("`column` must be a column index of the grid"));
            }
        }
        _ => {
            return Err(error(
                INVALID_PARAMS,
                "bad_action",
                format!("unknown action `{action}`"),
                "use query, set or sort",
            ));
        }
    }
    let Some(grid) = world.get::<DataGrid>(body) else {
        return Err(error(
            NOT_FOUND,
            "no_grid",
            "the node has no data grid",
            "create one with action \"set\"",
        ));
    };
    let order = grid.order();
    let rows: Vec<Value> = order
        .iter()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(display, r)| json!({ "row": display, "source": r, "cells": grid.rows[*r] }))
        .collect();
    Ok(json!({
        "entity": body.to_bits(),
        "columns": grid.columns.iter().map(|c| json!({ "title": c.title, "width": c.width })).collect::<Vec<_>>(),
        "sort": grid.sort.map(|(c, asc)| json!({ "column": c, "ascending": asc })),
        "total": order.len(),
        "headers": (0..grid.columns.len()).map(|c| header_label(grid, c)).collect::<Vec<_>>(),
        "rows": rows,
        "drawn": world.get::<regia_ui::parity_widgets::GridBody>(body).is_some(),
    }))
}

/// `ui.search_box { entity, query? }` (W11, ADR-2045): sets a search box's query (as typing
/// would) and reports what it shows. Filtering itself runs in the frame (`sync_search_boxes`); the
/// reply computes the same ranking so it is current at once.
pub(crate) fn search_box(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let query = match params.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= 256 => Some(s.clone()),
        Some(_) => {
            return Err(error(
                INVALID_PARAMS,
                "bad_query",
                "`query` must be a string of at most 256 bytes",
                "the text a player would type",
            ));
        }
    };
    let Some(sb) = world.get::<SearchBox>(entity).cloned() else {
        return Err(error(
            NOT_FOUND,
            "no_search_box",
            "the node is not a search box",
            "spawn one with a TextInput and a SearchBox component",
        ));
    };
    if let Some(q) = &query
        && let Some(mut input) = world.get_mut::<regia_ui::text_input::TextInput>(entity)
        && input.text != *q
    {
        input.text.clone_from(q);
        input.caret = q.len();
        input.selection_anchor = q.len();
    }
    let text = world
        .get::<regia_ui::text_input::TextInput>(entity)
        .map(|i| i.text.clone())
        .unwrap_or_default();
    let shown = search_results(&sb.items, &text, sb.limit);
    Ok(json!({
        "entity": entity.to_bits(),
        "query": text,
        "target": sb.target.map(Entity::to_bits),
        "total": sb.items.len(),
        "shown": shown.iter().map(|i| json!({ "index": i, "label": sb.items[*i] })).collect::<Vec<_>>(),
    }))
}

/// `ui.progress_dialog { entity, progress?, status? }` (W11, ADR-2045): reads or drives a progress
/// dialog.
pub(crate) fn progress_dialog(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let progress = match params.get("progress") {
        None | Some(Value::Null) => None,
        Some(v) => Some(number(v, "progress", 0.0, 1.0)?),
    };
    let status = match params.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= 512 => Some(s.clone()),
        Some(_) => {
            return Err(error(
                INVALID_PARAMS,
                "bad_param",
                "`status` must be a string of at most 512 bytes",
                "the line shown under the bar",
            ));
        }
    };
    let Some(mut d) = world.get_mut::<ProgressDialog>(entity) else {
        return Err(error(
            NOT_FOUND,
            "no_progress_dialog",
            "the node is not a progress dialog",
            "spawn one with regia_ui::parity_widgets::spawn_progress_dialog",
        ));
    };
    if let Some(p) = progress {
        d.progress = p;
    }
    if let Some(s) = status {
        d.status = s;
    }
    let d = d.clone();
    let open = world
        .get::<regia_ui::widgets::Modal>(entity)
        .is_some_and(|m| m.open);
    Ok(json!({
        "entity": entity.to_bits(),
        "progress": d.progress,
        "status": d.status,
        "cancelled": d.cancelled,
        "open": open,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use regia_ui::text::TextSpan;

    fn code_of(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    fn world_with_text() -> (World, Entity) {
        let mut world = World::new();
        let e = world
            .spawn(Text {
                spans: vec![TextSpan::lit("Hello "), TextSpan::lit("world")],
                ..Default::default()
            })
            .id();
        (world, e)
    }

    #[test]
    fn text_style_edits_one_span_and_reports() {
        let (mut world, e) = world_with_text();
        let reply = text_style(
            &mut world,
            &json!({
                "entity": e.to_bits(),
                "span": 1,
                "letter_spacing": 0.1,
                "weight": 600,
                "underline": true,
                "features": ["smcp", "liga=0"],
                "effects": [{ "color": "#000000", "width": 2.0 }, { "color": [0, 0, 0, 0.5], "offset": [2, 2], "blur": 3 }],
            }),
        )
        .unwrap();
        assert_eq!(reply["changed"], true);
        assert_eq!(reply["sdf"], false);
        let t = world.get::<Text>(e).unwrap();
        assert_eq!(t.spans[0].style, regia_ui::text::TextStyle::default());
        let st = &t.spans[1].style;
        assert!((st.letter_spacing - 0.1).abs() < 1e-6);
        assert_eq!(st.weight, Some(600));
        assert!(st.underline && !st.strikethrough);
        assert_eq!(st.features, vec!["smcp".to_owned(), "liga=0".to_owned()]);
        assert_eq!(st.effects.len(), 2);
        assert_eq!(st.effects[1].offset, [2.0, 2.0]);
        assert_eq!(reply["spans"][1]["effects"][0]["width"], 2.0);
        let reply = text_style(&mut world, &json!({ "entity": e.to_bits(), "sdf": true })).unwrap();
        assert_eq!(reply["sdf"], true);
        assert!(world.get::<SdfText>(e).is_some());
        text_style(&mut world, &json!({ "entity": e.to_bits(), "sdf": false })).unwrap();
        assert!(world.get::<SdfText>(e).is_none());
    }

    #[test]
    fn text_style_failed_call_changes_nothing() {
        let (mut world, e) = world_with_text();
        for bad in [
            json!({ "entity": e.to_bits(), "underline": true, "features": ["toolong"] }),
            json!({ "entity": e.to_bits(), "underline": true, "effects": [{ "width": 9 }] }),
            json!({ "entity": e.to_bits(), "underline": true, "effects": [{ "glow": 1 }] }),
            json!({ "entity": e.to_bits(), "underline": true, "span": 5 }),
            json!({ "entity": e.to_bits(), "underline": true, "weight": 0 }),
            json!({ "entity": e.to_bits(), "underline": true, "letter_spacing": 9 }),
        ] {
            assert!(text_style(&mut world, &bad).is_err(), "{bad}");
        }
        assert!(
            world
                .get::<Text>(e)
                .unwrap()
                .spans
                .iter()
                .all(|s| !s.style.underline)
        );
        let plain = world.spawn_empty().id();
        let err = text_style(&mut world, &json!({ "entity": plain.to_bits() })).unwrap_err();
        assert_eq!(code_of(&err), "no_text");
    }

    #[test]
    fn material_set_probe_and_remove() {
        let mut world = World::new();
        let e = world.spawn_empty().id();
        let reply = material(
            &mut world,
            &json!({ "entity": e.to_bits(), "kind": "radial_wipe", "params": [1.0, 0.01, 0, 0], "probe": [0, 0] }),
        )
        .unwrap();
        assert_eq!(reply["material"]["kind"], "radial_wipe");
        assert_eq!(reply["material"]["animate"], false);
        assert_eq!(reply["mix"], 1.0);
        let m = world.get::<UiMaterial>(e).copied().unwrap();
        assert_eq!(m.params, [1.0, 0.01, 0.0, 0.0]);
        // Switching kind resets the parameters to the new kind's defaults.
        material(&mut world, &json!({ "entity": e.to_bits(), "kind": "pulse" })).unwrap();
        let m = world.get::<UiMaterial>(e).copied().unwrap();
        assert_eq!(m.params, UiMaterialKind::Pulse.default_params());
        assert!(m.animate);
        let err = material(&mut world, &json!({ "entity": e.to_bits(), "kind": "lava" })).unwrap_err();
        assert_eq!(code_of(&err), "bad_material");
        let err = material(&mut world, &json!({ "entity": e.to_bits(), "color": "#zz" })).unwrap_err();
        assert_eq!(code_of(&err), "bad_color");
        let reply = material(&mut world, &json!({ "entity": e.to_bits(), "remove": true })).unwrap();
        assert!(reply["material"].is_null());
        assert!(world.get::<UiMaterial>(e).is_none());
    }

    #[test]
    fn data_grid_set_sort_and_page() {
        let mut world = World::new();
        let e = world.spawn_empty().id();
        let err = data_grid(&mut world, &json!({ "entity": e.to_bits() })).unwrap_err();
        assert_eq!(code_of(&err), "no_grid");
        let err = data_grid(
            &mut world,
            &json!({ "entity": e.to_bits(), "action": "set", "rows": [] }),
        )
        .unwrap_err();
        assert_eq!(code_of(&err), "bad_grid");
        let reply = data_grid(
            &mut world,
            &json!({
                "entity": e.to_bits(),
                "action": "set",
                "columns": [{ "title": "Name", "width": 100 }, { "title": "HP", "width": 50 }],
                "rows": [["orc", "30"], ["elf", "9"], ["bat", "12"]],
            }),
        )
        .unwrap();
        assert_eq!(reply["total"], 3);
        let reply = data_grid(
            &mut world,
            &json!({ "entity": e.to_bits(), "action": "sort", "column": 1 }),
        )
        .unwrap();
        assert_eq!(reply["rows"][0]["cells"], json!(["elf", "9"]));
        assert_eq!(reply["rows"][0]["source"], 1);
        assert_eq!(reply["headers"][1], "HP \u{25B2}");
        let reply = data_grid(
            &mut world,
            &json!({ "entity": e.to_bits(), "offset": 2, "limit": 5 }),
        )
        .unwrap();
        assert_eq!(reply["rows"].as_array().unwrap().len(), 1);
        let err = data_grid(
            &mut world,
            &json!({ "entity": e.to_bits(), "action": "sort", "column": 9 }),
        )
        .unwrap_err();
        assert_eq!(code_of(&err), "bad_grid");
        let err = data_grid(&mut world, &json!({ "entity": e.to_bits(), "action": "flip" })).unwrap_err();
        assert_eq!(code_of(&err), "bad_action");
    }

    #[test]
    fn search_box_and_progress_dialog_methods() {
        let mut world = World::new();
        let e = world
            .spawn((
                regia_ui::text_input::TextInput::default(),
                SearchBox {
                    items: vec!["Open file".into(), "Save file".into(), "Quit".into()],
                    ..Default::default()
                },
            ))
            .id();
        let reply = search_box(&mut world, &json!({ "entity": e.to_bits(), "query": "file" })).unwrap();
        assert_eq!(reply["shown"].as_array().unwrap().len(), 2);
        assert_eq!(reply["shown"][0]["label"], "Open file");
        assert_eq!(
            world.get::<regia_ui::text_input::TextInput>(e).unwrap().text,
            "file"
        );
        let plain = world.spawn_empty().id();
        let err = search_box(&mut world, &json!({ "entity": plain.to_bits() })).unwrap_err();
        assert_eq!(code_of(&err), "no_search_box");
        let err = search_box(&mut world, &json!({ "entity": e.to_bits(), "query": 3 })).unwrap_err();
        assert_eq!(code_of(&err), "bad_query");

        let d = world
            .spawn((ProgressDialog::default(), regia_ui::widgets::Modal::default()))
            .id();
        let reply = progress_dialog(
            &mut world,
            &json!({ "entity": d.to_bits(), "progress": 0.5, "status": "half" }),
        )
        .unwrap();
        assert_eq!(reply["progress"], 0.5);
        assert_eq!(reply["status"], "half");
        assert_eq!(reply["open"], true);
        let err = progress_dialog(&mut world, &json!({ "entity": d.to_bits(), "progress": 2 })).unwrap_err();
        assert_eq!(code_of(&err), "bad_param");
        let err = progress_dialog(&mut world, &json!({ "entity": plain.to_bits() })).unwrap_err();
        assert_eq!(code_of(&err), "no_progress_dialog");
    }
}
