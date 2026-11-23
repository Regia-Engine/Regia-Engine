// SPDX-License-Identifier: MPL-2.0
//! Layout variants in the UI designer (P-271, addendum § 13.6, ADR-7858): the canvas toolbar's variant
//! switcher (Base, then each variant), edits that write a variant's overrides while it is active, the
//! inspector's list of the selected node's overridden fields with a reset for each, and the preview
//! size that matches the chosen variant. The agent route is `editor.ui_designer_variant`
//! (`book/agents/ui-designer.md`).
//!
//! The active variant is view state of the open document ([`PreviewSettings::variant`]): it is never
//! written to the file and takes no undo step. Adding, removing and resetting are document edits (one
//! undo step each); while a variant is active a field edit ([`DesignOp::Set`]) changes only that
//! variant's overrides, so the base stays byte for byte, and a structure edit (add, remove, move,
//! duplicate) is refused.

use std::borrow::Cow;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::authoring::{UiFile, UiNodeDef};
use regia_ui::layout_variants::{
    LOCKED_FIELDS, LayoutVariant, Orientation, VariantWhen, apply_variant, node_key, select_variant,
};
use serde_json::{Value, json};

use crate::ui_designer::{
    DesignOp, DesignerError, NodePath, UiDesign, UiDesigner, node_at, node_json, path_string, resolve_node,
    set_field,
};
use crate::ui_designer_preview::{MAX_PREVIEW_EDGE, PreviewSettings, SIZE_PRESETS};
use crate::viewport_agent::Handler;

/// The error codes only `editor.ui_designer_variant` returns (as `data.code`), besides the shared
/// `editor.ui_designer_*` ones; a structure edit during a variant returns `ui_designer_variant_active`
/// from the edit methods.
pub const VARIANT_ERROR_CODES: [&str; 5] = [
    "ui_designer_bad_variant",
    "ui_designer_bad_when",
    "ui_designer_variant_active",
    "ui_designer_variant_locked",
    "ui_designer_no_override",
];

/// The variant the designer edits: [`PreviewSettings::variant`] when it names one of the document's
/// variants.
#[must_use]
pub fn active(file: &UiFile, settings: &PreviewSettings) -> Option<usize> {
    settings.variant.filter(|i| *i < file.variants.len())
}

/// The variant the preview shows: the active one, else the first whose `when` matches the preview
/// size (the preview's OS scale is 1, so its size is the logical size the game compares).
#[must_use]
pub fn shown(file: &UiFile, settings: &PreviewSettings) -> Option<usize> {
    active(file, settings).or_else(|| select_variant(&file.variants, settings.size))
}

/// The document the preview lays out: the shown variant applied on top of the base (no `variants`
/// left), or the document itself.
#[must_use]
pub fn preview_document<'a>(file: &'a UiFile, settings: &PreviewSettings) -> Cow<'a, UiFile> {
    match shown(file, settings) {
        None => Cow::Borrowed(file),
        Some(i) => {
            let (root, _) = apply_variant(&file.root, &file.variants[i]);
            Cow::Owned(UiFile {
                version: file.version,
                component: file.component.clone(),
                root,
                variants: Vec::new(),
                back: file.back,
            })
        }
    }
}

/// The document edits start from: the active variant applied on top of the base, or the document
/// itself with Base selected (canvas drags and `transform` plan from it).
#[must_use]
pub fn edited_document<'a>(file: &'a UiFile, settings: &PreviewSettings) -> Cow<'a, UiFile> {
    match active(file, settings) {
        None => Cow::Borrowed(file),
        Some(i) => {
            let (root, _) = apply_variant(&file.root, &file.variants[i]);
            Cow::Owned(UiFile {
                version: file.version,
                component: file.component.clone(),
                root,
                variants: file.variants.clone(),
                back: file.back,
            })
        }
    }
}

/// The node the inspector shows at `path`: with a variant active, its values in that variant.
#[must_use]
pub fn inspected_node(doc: &UiDesign, path: &[usize]) -> Option<UiNodeDef> {
    let file = doc.file();
    match active(file, &doc.preview) {
        Some(i) => node_at(&apply_variant(&file.root, &file.variants[i]).0, path).cloned(),
        None => node_at(&file.root, path).cloned(),
    }
}

/// The override key a field edit is stored under: a typed widget field (`widget.max`) stores the
/// whole widget, `text` the whole text; other keys are stored as given.
fn stored_key(node: &UiNodeDef, key: &str) -> String {
    let key = key.trim();
    if let Some(field) = key.strip_prefix("widget.")
        && node
            .widget
            .as_ref()
            .is_some_and(|w| crate::ui_designer_widget::has_field(w, field))
    {
        return "widget".to_owned();
    }
    if key == "text" || (key.starts_with("text.") && node.text.is_none()) {
        return "text".to_owned();
    }
    key.to_owned()
}

/// The value at the dotted `key` of `json` (`null` when absent: the field holds its default).
fn value_at(json: &Value, key: &str) -> Value {
    let mut at = json;
    for part in key.split('.') {
        match at.get(part) {
            Some(next) => at = next,
            None => return Value::Null,
        }
    }
    at.clone()
}

/// Numbers that came from `f32` fields written at their shortest (`0.3`, not `0.30000001192092896`);
/// they decode to the same `f32`.
fn tidy(value: Value) -> Value {
    match value {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => {
            let Some(f) = n.as_f64() else {
                return Value::Number(n);
            };
            let narrow = f as f32;
            if f64::from(narrow) == f
                && let Ok(short) = narrow.to_string().parse::<f64>()
                && let Some(out) = serde_json::Number::from_f64(short)
            {
                return Value::Number(out);
            }
            Value::Number(n)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(tidy).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, tidy(v))).collect()),
        other => other,
    }
}

/// Applies `op` to variant `index` of `file`: a field edit becomes an override of the node in that
/// variant (the base is not touched); any other edit is refused. Returns the touched node.
pub(crate) fn apply_in_variant(
    file: &mut UiFile,
    index: usize,
    op: DesignOp,
) -> Result<NodePath, DesignerError> {
    let DesignOp::Set { node, key, value } = op else {
        return Err(DesignerError::new(
            "variant_active",
            "a layout variant changes fields only: select Base to add, remove or move nodes",
        ));
    };
    let variant = file
        .variants
        .get(index)
        .ok_or_else(|| bad_variant(index, file.variants.len()))?;
    let first = key.trim().split('.').next().unwrap_or_default();
    if LOCKED_FIELDS.contains(&first) {
        return Err(DesignerError::new(
            "variant_locked",
            format!("a layout variant cannot override `{first}`: select Base to change it"),
        ));
    }
    let (effective, _) = apply_variant(&file.root, variant);
    let current = node_at(&effective, &node)
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(&node))))?;
    let next = set_field(current, &key, value)?;
    let stored = stored_key(current, &key);
    let stored_value = tidy(value_at(&node_json(&next), &stored));
    let at = node_key(&file.root, &node);
    let overrides = file.variants[index].overrides.entry(at).or_default();
    let nested = format!("{stored}.");
    overrides.retain(|k, _| !k.starts_with(&nested));
    overrides.insert(stored, stored_value);
    Ok(node)
}

fn bad_variant(index: usize, count: usize) -> DesignerError {
    DesignerError::new(
        "bad_variant",
        format!("no variant {index} (the document has {count})"),
    )
}

/// The override keys variant `index` sets on the node at `path`.
#[must_use]
pub fn overridden_keys(file: &UiFile, index: usize, path: &[usize]) -> Vec<String> {
    let Some(variant) = file.variants.get(index) else {
        return Vec::new();
    };
    variant
        .overrides
        .iter()
        .filter(|(key, _)| regia_ui::layout_variants::find_node(&file.root, key).as_deref() == Some(path))
        .flat_map(|(_, fields)| fields.keys().cloned())
        .collect()
}

/// Whether the field `key` of the node at `path` is overridden in variant `index` (by the key itself,
/// a whole field it is part of, or a part of it).
#[must_use]
pub fn is_overridden(file: &UiFile, index: usize, path: &[usize], key: &str) -> bool {
    overridden_keys(file, index, path)
        .iter()
        .any(|k| k == key || key.starts_with(&format!("{k}.")) || k.starts_with(&format!("{key}.")))
}

/// Removes the overrides of the node at `path` in the active variant: `key` and its parts, or every
/// override of the node when `key` is `None`. One undo step; returns how many were removed.
pub fn reset_override(doc: &mut UiDesign, path: &[usize], key: Option<&str>) -> Result<usize, DesignerError> {
    let index = active(doc.file(), &doc.preview)
        .ok_or_else(|| DesignerError::new("bad_variant", "no variant is selected (Base edits the base)"))?;
    let mut file = doc.file().clone();
    if node_at(&file.root, path).is_none() {
        return Err(DesignerError::new(
            "bad_node",
            format!("no node at {}", path_string(path)),
        ));
    }
    let mut removed = 0;
    let variant = &mut file.variants[index];
    let root = &doc.file().root;
    let keys: Vec<String> = variant
        .overrides
        .keys()
        .filter(|k| regia_ui::layout_variants::find_node(root, k).as_deref() == Some(path))
        .cloned()
        .collect();
    for node in keys {
        let Some(fields) = variant.overrides.get_mut(&node) else {
            continue;
        };
        let before = fields.len();
        match key {
            Some(key) => {
                let nested = format!("{key}.");
                fields.retain(|k, _| k != key && !k.starts_with(&nested));
            }
            None => fields.clear(),
        }
        removed += before - fields.len();
        if fields.is_empty() {
            variant.overrides.remove(&node);
        }
    }
    if removed == 0 {
        return Err(DesignerError::new(
            "no_override",
            format!(
                "{} has no override{} in this variant",
                path_string(path),
                key.map(|k| format!(" of {k}")).unwrap_or_default()
            ),
        ));
    }
    doc.replace_as_step(file, path.to_vec());
    Ok(removed)
}

/// Adds a variant at the end (one undo step) and returns its index. The `when` must be able to match.
pub fn add_variant(
    doc: &mut UiDesign,
    name: Option<String>,
    when: VariantWhen,
) -> Result<usize, DesignerError> {
    if let Some(problem) = when.problem() {
        return Err(DesignerError::new("bad_when", problem));
    }
    let mut file = doc.file().clone();
    file.variants.push(LayoutVariant {
        name: name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
        when,
        overrides: Default::default(),
    });
    let index = file.variants.len() - 1;
    let selection = doc.selection.clone().unwrap_or_default();
    doc.replace_as_step(file, selection);
    Ok(index)
}

/// Removes variant `index` (one undo step); the active variant follows its position, or goes back to
/// Base when it was the removed one.
pub fn remove_variant(doc: &mut UiDesign, index: usize) -> Result<(), DesignerError> {
    let count = doc.file().variants.len();
    if index >= count {
        return Err(bad_variant(index, count));
    }
    let mut file = doc.file().clone();
    file.variants.remove(index);
    let active_now = active(doc.file(), &doc.preview);
    let selection = doc.selection.clone().unwrap_or_default();
    doc.replace_as_step(file, selection);
    doc.preview.variant = match active_now {
        Some(a) if a == index => None,
        Some(a) if a > index => Some(a - 1),
        other => other,
    };
    Ok(())
}

/// Sizes the preview tries for a variant: the named presets (with their insets), then sizes built
/// from the variant's own bounds.
fn candidate_sizes(when: Option<&VariantWhen>) -> Vec<([f32; 2], [f32; 4])> {
    let mut out: Vec<([f32; 2], [f32; 4])> = SIZE_PRESETS.iter().map(|(_, s, i)| (*s, *i)).collect();
    let mut widths = vec![1080.0, 1920.0, 1280.0, 720.0, 2400.0, 800.0, 390.0, 2560.0];
    let mut aspects = vec![
        9.0 / 16.0,
        16.0 / 9.0,
        3.0 / 4.0,
        4.0 / 3.0,
        1.0,
        9.0 / 20.0,
        21.0 / 9.0,
    ];
    if let Some(w) = when {
        let mid = |a: Option<f32>, b: Option<f32>| a.zip(b).map(|(a, b)| (a + b) * 0.5);
        widths.splice(
            0..0,
            [mid(w.min_width, w.max_width), w.min_width, w.max_width]
                .into_iter()
                .flatten(),
        );
        aspects.splice(
            0..0,
            [mid(w.min_aspect, w.max_aspect), w.min_aspect, w.max_aspect]
                .into_iter()
                .flatten(),
        );
    }
    for w in &widths {
        for a in &aspects {
            if *a > 0.0 {
                out.push(([w.round(), (w / a).round()], [0.0; 4]));
            }
        }
    }
    out.retain(|(s, _)| {
        s.iter()
            .all(|v| v.is_finite() && *v >= 1.0 && *v <= MAX_PREVIEW_EDGE)
    });
    out
}

/// A preview size (with insets) at which `target` is what the game shows: variant `target`, or the
/// base for `None` (no variant matches). `None` when no tried size gets there (an earlier variant
/// covers every size this one matches).
#[must_use]
pub fn sample_size(variants: &[LayoutVariant], target: Option<usize>) -> Option<([f32; 2], [f32; 4])> {
    let when = target.and_then(|i| variants.get(i)).map(|v| &v.when);
    candidate_sizes(when)
        .into_iter()
        .find(|(size, _)| select_variant(variants, *size) == target)
}

/// Selects the variant the designer edits (`None`: Base) and switches the preview to a size where the
/// game shows it, when the current size does not. View state only. Returns whether such a size exists.
pub fn select(doc: &mut UiDesign, index: Option<usize>) -> Result<bool, DesignerError> {
    let variants = &doc.file().variants;
    if let Some(i) = index
        && i >= variants.len()
    {
        return Err(bad_variant(i, variants.len()));
    }
    let reachable = if select_variant(variants, doc.preview.size) == index {
        true
    } else if let Some((size, insets)) = sample_size(variants, index) {
        doc.preview.size = size;
        doc.preview.safe_area = insets;
        true
    } else {
        false
    };
    doc.preview.variant = index;
    Ok(reachable)
}

/// One variant as the agent and the switcher describe it.
fn variant_json(file: &UiFile, index: usize) -> Value {
    let v = &file.variants[index];
    let overrides: serde_json::Map<String, Value> = v
        .overrides
        .iter()
        .map(|(node, fields)| (node.clone(), json!(fields.keys().collect::<Vec<_>>())))
        .collect();
    json!({
        "index": index,
        "name": v.name,
        "label": v.label(index),
        "when": serde_json::to_value(&v.when).unwrap_or(Value::Null),
        "describe": v.when.describe(),
        "overrides": overrides,
        "sample_size": sample_size(&file.variants, Some(index)).map(|(s, _)| s),
    })
}

/// The `editor.ui_designer_variant` result: every variant, the active one and what the preview shows.
fn state_json(doc: &UiDesign) -> Value {
    let file = doc.file();
    json!({
        "variants": (0..file.variants.len()).map(|i| variant_json(file, i)).collect::<Vec<_>>(),
        "active": active(file, &doc.preview),
        "shown": shown(file, &doc.preview),
        "preview": { "size": doc.preview.size, "safe_area": doc.preview.safe_area },
        "revision": doc.revision(),
        "undo": doc.undo_len(),
    })
}

// ------------------------------------------------------------------------------------------- panel

/// Records a widget's rect for the panel tests (the key the designer's tests use).
#[cfg(test)]
fn test_rect(ui: &egui::Ui, key: &str, rect: egui::Rect) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("ui-designer-test-rect", key)), rect));
}

/// Records a widget's id so keyboard tests can focus it.
#[cfg(test)]
fn test_id(ui: &egui::Ui, key: &str, id: egui::Id) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("ui-designer-test-id", key)), id));
}

fn report<T>(error: &mut Option<String>, result: Result<T, DesignerError>) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            *error = Some(e.to_string());
            None
        }
    }
}

/// The canvas toolbar's variant switcher: Base, then each variant (each a focusable button: Tab and
/// Space or Enter switch from the keyboard), Add portrait / Add landscape, and Remove for the active
/// variant. A variant's size bounds are edited through `editor.ui_designer_variant` or the file.
pub fn variant_bar(ui: &mut egui::Ui, doc: &mut UiDesign, error: &mut Option<String>) {
    let file = doc.file();
    let current = active(file, &doc.preview);
    let showing = shown(file, &doc.preview);
    let labels: Vec<(String, String)> = file
        .variants
        .iter()
        .enumerate()
        .map(|(i, v)| (v.label(i), v.when.describe()))
        .collect();
    let mut pick: Option<Option<usize>> = None;
    let mut add: Option<Orientation> = None;
    let mut remove = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Variant");
        let base = ui
            .selectable_label(current.is_none(), "Base")
            .on_hover_text("Edit the base layout (the preview still shows the variant its size picks)");
        #[cfg(test)]
        {
            test_rect(ui, "variant:base", base.rect);
            test_id(ui, "variant:base", base.id);
        }
        if base.clicked() {
            pick = Some(None);
        }
        for (i, (label, describe)) in labels.iter().enumerate() {
            let chip = ui
                .selectable_label(current == Some(i), label)
                .on_hover_text(format!("Edit this variant's overrides ({describe})"));
            #[cfg(test)]
            {
                test_rect(ui, &format!("variant:{i}"), chip.rect);
                test_id(ui, &format!("variant:{i}"), chip.id);
            }
            if chip.clicked() {
                pick = Some(Some(i));
            }
        }
        let portrait = ui
            .button("Add portrait")
            .on_hover_text("Add a variant for portrait windows and edit it");
        #[cfg(test)]
        {
            test_id(ui, "variant:add-portrait", portrait.id);
            test_rect(ui, "variant:add-portrait", portrait.rect);
        }
        if portrait.clicked() {
            add = Some(Orientation::Portrait);
        }
        let landscape = ui
            .button("Add landscape")
            .on_hover_text("Add a variant for landscape windows and edit it");
        #[cfg(test)]
        {
            test_id(ui, "variant:add-landscape", landscape.id);
            test_rect(ui, "variant:add-landscape", landscape.rect);
        }
        if landscape.clicked() {
            add = Some(Orientation::Landscape);
        }
        if current.is_some() {
            let r = ui
                .button("Remove variant")
                .on_hover_text("Remove the variant being edited and its overrides (one undo step)");
            #[cfg(test)]
            test_id(ui, "variant:remove", r.id);
            if r.clicked() {
                remove = true;
            }
        }
        if current.is_none()
            && let Some(i) = showing
        {
            ui.label(format!(
                "Preview shows: {}",
                labels.get(i).map_or("", |(l, _)| l.as_str())
            ));
        }
    });
    if let Some(orientation) = add
        && let Some(index) = report(
            error,
            add_variant(
                doc,
                None,
                VariantWhen {
                    orientation: Some(orientation),
                    ..VariantWhen::default()
                },
            ),
        )
    {
        pick = Some(Some(index));
    }
    if remove && let Some(i) = current {
        report(error, remove_variant(doc, i));
    }
    if let Some(index) = pick
        && report(error, select(doc, index)) == Some(false)
    {
        *error = Some(
            "no preview size shows this variant: an earlier variant matches every size it does".to_owned(),
        );
    }
}

/// The inspector's Overrides section while a variant is active: each overridden field of the node
/// at `path`, highlighted in the theme's warning color, with a Reset button (and Reset all).
pub fn override_rows(ui: &mut egui::Ui, doc: &mut UiDesign, path: &[usize], error: &mut Option<String>) {
    let Some(index) = active(doc.file(), &doc.preview) else {
        return;
    };
    let file = doc.file();
    let keys = overridden_keys(file, index, path);
    let label = file.variants[index].label(index);
    let effective = inspected_node(doc, path)
        .map(|n| node_json(&n))
        .unwrap_or(Value::Null);
    let highlight = ui.visuals().warn_fg_color;
    let mut reset: Option<Option<String>> = None;
    ui.separator();
    ui.colored_label(highlight, format!("Overrides in {label}"));
    if keys.is_empty() {
        ui.label("None: this node shows its base values. Edits here add overrides.");
    }
    for key in &keys {
        ui.horizontal(|ui| {
            ui.colored_label(highlight, key.as_str());
            let mut shown_value = value_at(&effective, key).to_string();
            if shown_value.len() > 48 {
                let cut = (0..=45)
                    .rev()
                    .find(|i| shown_value.is_char_boundary(*i))
                    .unwrap_or(0);
                shown_value.truncate(cut);
                shown_value.push_str("...");
            }
            ui.label(shown_value);
            let r = ui
                .button("Reset")
                .on_hover_text(format!("Reset {key} to the base value"));
            #[cfg(test)]
            test_id(ui, &format!("override-reset:{key}"), r.id);
            if r.clicked() {
                reset = Some(Some(key.clone()));
            }
        });
    }
    if keys.len() > 1 {
        let all = ui
            .button("Reset all")
            .on_hover_text("Remove every override of this node in this variant");
        if all.clicked() {
            reset = Some(None);
        }
    }
    if let Some(key) = reset {
        report(error, reset_override(doc, path, key.as_deref()));
    }
}

// ------------------------------------------------------------------------------------------- agent

fn rpc(e: DesignerError) -> RpcError {
    let (code, hint) = match e.code {
        "no_document" => (NOT_FOUND, "call editor.ui_designer_open first"),
        "bad_node" => (
            NOT_FOUND,
            "editor.ui_designer_state lists node paths; a node is \"/0/2\" or \"#name\"",
        ),
        "bad_variant" => (
            NOT_FOUND,
            "action list numbers the variants from 0; select one before reset",
        ),
        "bad_when" => (
            INVALID_PARAMS,
            "when bounds are numbers >= 0 with min <= max; orientation is portrait or landscape",
        ),
        "no_override" => (
            NOT_FOUND,
            "action list shows each variant's overridden fields by node",
        ),
        _ => (INVALID_PARAMS, "the message names the field; nothing was changed"),
    };
    RpcError::new(code, &format!("ui_designer_{}", e.code), e.message, hint)
}

fn doc(world: &mut World) -> Result<&mut UiDesign, RpcError> {
    world
        .get_resource_or_insert_with(UiDesigner::default)
        .into_inner()
        .doc_mut()
        .map_err(rpc)
}

fn index_param(params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("index") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|i| Some(i as usize))
            .ok_or_else(|| RpcError::invalid_params("`index` is a variant number from 0 (or null for Base)")),
    }
}

/// `editor.ui_designer_variant`.
fn variant(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
    let doc = doc(world)?;
    let mut extra = serde_json::Map::new();
    match action {
        "list" => {}
        "add" => {
            let when: VariantWhen = match params.get("when") {
                None | Some(Value::Null) => VariantWhen::default(),
                Some(w) => serde_json::from_value(w.clone())
                    .map_err(|e| rpc(DesignerError::new("bad_when", format!("when: {e}"))))?,
            };
            let name = params.get("name").and_then(Value::as_str).map(str::to_owned);
            let index = add_variant(doc, name, when).map_err(rpc)?;
            let reachable = select(doc, Some(index)).map_err(rpc)?;
            extra.insert("index".into(), json!(index));
            extra.insert("reachable".into(), json!(reachable));
        }
        "select" => {
            let index = index_param(params)?;
            let reachable = select(doc, index).map_err(rpc)?;
            extra.insert("reachable".into(), json!(reachable));
        }
        "remove" => {
            let index = index_param(params)?
                .ok_or_else(|| RpcError::invalid_params("`index` (the variant to remove) is required"))?;
            remove_variant(doc, index).map_err(rpc)?;
        }
        "reset" => {
            let node = params
                .get("node")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`node` (a string) is required for reset"))?;
            let path = resolve_node(&doc.file().root, node).map_err(rpc)?;
            let key = params.get("key").and_then(Value::as_str);
            let removed = reset_override(doc, &path, key).map_err(rpc)?;
            extra.insert("removed".into(), json!(removed));
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "action {other:?} is not one of list, add, select, remove, reset"
            )));
        }
    }
    let mut out = state_json(doc);
    if let Value::Object(map) = &mut out {
        map.extend(extra);
    }
    Ok(out)
}

/// The variant method, appended to the `editor.ui_designer_*` table.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.ui_designer_variant",
            tool: "ui_designer_variant",
            description: "Layout variants of the open UI document (override sets by window orientation and size; \
                the first match applies on top of the base at run time). list: every variant with its when, \
                overridden fields by node and a preview size that shows it. add: append a variant (one undo \
                step) and select it. select: edit a variant (index) or the base (no index); the preview \
                switches to a size that shows it; while a variant is selected, editor.ui_designer_set writes \
                that variant's overrides only and structure edits are refused. remove: delete a variant (one \
                undo step). reset: remove the selected variant's override of node's key (or all of node's).",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "list (default), add, select, remove or reset.",
                },
                ParamSchema {
                    name: "index",
                    kind: "integer",
                    required: false,
                    description: "The variant (from 0) for select and remove; select without it edits the base.",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "add: the variant's display name.",
                },
                ParamSchema {
                    name: "when",
                    kind: "object",
                    required: false,
                    description: "add: { orientation?: portrait|landscape, min_width?, max_width?, min_aspect?, max_aspect? } (logical px; aspect is width / height; bounds inclusive). Empty matches every window.",
                },
                ParamSchema {
                    name: "node",
                    kind: "string",
                    required: false,
                    description: "reset: a node path (\"/0/2\") or \"#name\".",
                },
                ParamSchema {
                    name: "key",
                    kind: "string",
                    required: false,
                    description: "reset: the dotted field key; absent resets every override of the node.",
                },
            ],
            result: "{ variants: [{ index, name, label, when, describe, overrides: { <node>: [keys] }, sample_size }], active, shown, preview: { size, safe_area }, revision, undo, index?, reachable?, removed? }",
            mutating: true,
            permission: Permission::EditScene,
        },
        variant as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::Dimension;
    use regia_ui::{UiMigrators, parse_ui_ron, write_ui_ron};

    const DOC: &str = r#"// A menu with a portrait variant.
(
    version: 11,
    root: (
        style: (size: (Percent(100.0), Percent(100.0))),
        children: [
            // The main button.
            (
                name: "play",
                interactable: true,
                style: (position: absolute, inset: (left: 40.0, top: 40.0), size: (200.0, 60.0)),
            ),
            (
                name: "logo",
                style: (position: absolute, inset: (left: 400.0, top: 40.0), size: (100.0, 100.0)),
            ),
        ],
    ),
    variants: [
        (
            name: "portrait",
            when: (orientation: portrait),
            overrides: {
                "play": { "style.inset": [40.0, 600.0, "Auto", "Auto"] },
            },
        ),
    ],
)
"#;

    fn designer() -> UiDesigner {
        let mut d = UiDesigner::default();
        d.open_text(DOC, None).expect("open");
        d
    }

    fn base_text(file: &UiFile) -> String {
        write_ui_ron(&UiFile {
            variants: Vec::new(),
            ..file.clone()
        })
        .expect("write")
    }

    /// The method's handler as the editor registers it: found by name in the whole extension table
    /// (book-check needs the name as a test literal; review g17 row 8).
    fn registered() -> Handler {
        let table = crate::agent_ext::all_methods();
        let found: Vec<Handler> = table
            .iter()
            .filter(|(s, _)| s.name == "editor.ui_designer_variant")
            .map(|(_, h)| *h)
            .collect();
        assert_eq!(found.len(), 1, "editor.ui_designer_variant is registered once");
        found[0]
    }

    fn call(designer: UiDesigner, params: Value) -> (UiDesigner, Value) {
        let mut world = World::new();
        world.insert_resource(designer);
        let handler = registered();
        let out = handler(&mut world, &params).unwrap_or_else(|e| panic!("{params}: {e:?}"));
        (world.remove_resource::<UiDesigner>().expect("designer"), out)
    }

    fn call_err(designer: UiDesigner, params: Value) -> (UiDesigner, RpcError) {
        let mut world = World::new();
        world.insert_resource(designer);
        let handler = registered();
        let err = handler(&mut world, &params).expect_err("fails");
        (world.remove_resource::<UiDesigner>().expect("designer"), err)
    }

    fn data_code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    /// The Play button's top edge in UI px (the preview scales with its size).
    fn play_top(frame: &crate::ui_designer_preview::PreviewFrame) -> f32 {
        let top = frame
            .nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("play"))
            .expect("play")
            .rect[1];
        // Layout snaps to whole physical px.
        (top / frame.scale).round()
    }

    /// Acceptance: resizing the preview from landscape to portrait applies the portrait variant.
    #[test]
    fn the_preview_applies_the_variant_its_size_picks() {
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        doc.preview.use_preset("1920x1080").expect("preset");
        let frame = crate::ui_designer_preview::preview(doc.file(), &doc.preview, None).expect("preview");
        assert_eq!(play_top(&frame), 40.0, "landscape shows the base");
        doc.preview.use_preset("1080x2400").expect("preset");
        let frame = crate::ui_designer_preview::preview(doc.file(), &doc.preview, None).expect("preview");
        assert_eq!(play_top(&frame), 600.0, "portrait shows the portrait variant");
        assert_eq!(shown(doc.file(), &doc.preview), Some(0));
        assert_eq!(
            active(doc.file(), &doc.preview),
            None,
            "Base is still what is edited"
        );
    }

    /// Acceptance: editing in a variant leaves the base unchanged (bytes); the save keeps every other
    /// byte of the hand-written file; reset removes the override; undo and redo cover it.
    #[test]
    fn editing_in_a_variant_writes_overrides_only_and_reset_removes_them() {
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        let base_before = base_text(doc.file());
        assert!(select(doc, Some(0)).expect("select"), "a portrait size exists");
        assert!(
            doc.preview.size[1] > doc.preview.size[0],
            "the preview switched to portrait"
        );
        doc.apply(DesignOp::Set {
            node: vec![1],
            key: "opacity".to_owned(),
            value: json!(0.3),
        })
        .expect("set in variant");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "style.inset".to_owned(),
            value: json!([40.0, 700.0, "Auto", "Auto"]),
        })
        .expect("set in variant");
        assert_eq!(
            base_text(doc.file()),
            base_before,
            "the base is unchanged, byte for byte"
        );
        let v = &doc.file().variants[0];
        assert_eq!(
            v.overrides["logo"]["opacity"],
            json!(0.3),
            "f32 written at its shortest"
        );
        assert_eq!(
            v.overrides["play"]["style.inset"],
            json!([40.0, 700.0, "Auto", "Auto"])
        );
        assert!(is_overridden(doc.file(), 0, &[1], "opacity"));
        assert!(!is_overridden(doc.file(), 0, &[1], "z"));
        assert_eq!(
            inspected_node(doc, &[1]).expect("node").opacity,
            Some(0.3),
            "the inspector shows the variant's value"
        );

        // The save merges into the source: everything before `variants` is the original bytes.
        let (saved, canonical) = doc.save_text().expect("save text");
        assert!(!canonical, "merged, not rewritten");
        let cut = DOC.find("    variants:").expect("variants in source");
        assert_eq!(
            &saved[..cut],
            &DOC[..cut],
            "the base bytes are untouched:\n{saved}"
        );
        let back = parse_ui_ron(&saved, "back.ui.ron", &UiMigrators::default()).expect("parses");
        assert_eq!(&back, doc.file());

        // Structure edits are refused while a variant is active; so is renaming.
        let err = doc.apply(DesignOp::Remove(vec![1])).expect_err("refused");
        assert_eq!(err.code, "variant_active");
        let err = doc
            .apply(DesignOp::Set {
                node: vec![1],
                key: "name".to_owned(),
                value: json!("x"),
            })
            .expect_err("refused");
        assert_eq!(err.code, "variant_locked");

        // Reset removes the override (one undo step) and undo brings it back.
        let undo = doc.undo_len();
        assert_eq!(reset_override(doc, &[1], Some("opacity")).expect("reset"), 1);
        assert_eq!(doc.undo_len(), undo + 1);
        assert!(!doc.file().variants[0].overrides.contains_key("logo"));
        assert_eq!(inspected_node(doc, &[1]).expect("node").opacity, None);
        assert_eq!(
            reset_override(doc, &[1], Some("opacity"))
                .expect_err("nothing left")
                .code,
            "no_override"
        );
        doc.undo().expect("undo");
        assert_eq!(doc.file().variants[0].overrides["logo"]["opacity"], json!(0.3));

        // Base edits the base again.
        select(doc, None).expect("base");
        doc.apply(DesignOp::Set {
            node: vec![1],
            key: "opacity".to_owned(),
            value: json!(0.5),
        })
        .expect("base edit");
        assert_eq!(doc.file().root.children[1].opacity, Some(0.5));
        assert_ne!(base_text(doc.file()), base_before);
    }

    #[test]
    fn typed_widget_fields_store_the_whole_widget_and_apply_at_runtime() {
        let mut d = UiDesigner::default();
        d.open_text(
            "(version: 11, root: (children: [(name: \"s\", widget: slider(0.5, 0.0, 1.0, 0.1))]), variants: [(when: (orientation: portrait))])",
            None,
        )
        .expect("open");
        let doc = d.doc_mut().expect("doc");
        select(doc, Some(0)).expect("select");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "widget.max".to_owned(),
            value: json!(5.0),
        })
        .expect("typed set");
        let fields = &doc.file().variants[0].overrides["s"];
        assert_eq!(fields.keys().collect::<Vec<_>>(), ["widget"]);
        let (root, problems) = apply_variant(&doc.file().root, &doc.file().variants[0]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            Some(root.children[0].clone()),
            inspected_node(doc, &[0]),
            "the runtime applies what the designer shows"
        );
        assert_ne!(root.children[0].widget, doc.file().root.children[0].widget);
    }

    #[test]
    fn add_select_remove_and_reset_through_the_dispatcher() {
        let (d, out) = call(designer(), json!({ "action": "list" }));
        assert_eq!(out["variants"].as_array().expect("list").len(), 1);
        assert_eq!(out["variants"][0]["overrides"]["play"], json!(["style.inset"]));
        assert_eq!(out["active"], Value::Null);

        let (d, out) = call(
            d,
            json!({ "action": "add", "name": "ultrawide", "when": { "orientation": "landscape", "min_aspect": 2.2 } }),
        );
        assert_eq!(out["index"], json!(1));
        assert_eq!(out["active"], json!(1));
        assert_eq!(out["reachable"], json!(true));
        let size = out["preview"]["size"].as_array().expect("size");
        let (w, h) = (size[0].as_f64().expect("w"), size[1].as_f64().expect("h"));
        assert!(w / h >= 2.2, "the preview switched to an ultrawide size: {w}x{h}");

        let (d, out) = call(d, json!({ "action": "select", "index": 0 }));
        assert_eq!(out["active"], json!(0));
        let (d, out) = call(
            d,
            json!({ "action": "reset", "node": "#play", "key": "style.inset" }),
        );
        assert_eq!(out["removed"], json!(1));
        assert_eq!(out["variants"][0]["overrides"], json!({}));

        let (d, out) = call(d, json!({ "action": "remove", "index": 0 }));
        assert_eq!(out["variants"].as_array().expect("list").len(), 1);
        assert_eq!(
            out["active"],
            Value::Null,
            "the removed variant was active: back to Base"
        );
        let (d, out) = call(d, json!({ "action": "select" }));
        assert_eq!(out["active"], Value::Null);

        let (d, err) = call_err(d, json!({ "action": "select", "index": 9 }));
        assert_eq!(data_code(&err), Some("ui_designer_bad_variant"));
        let (d, err) = call_err(
            d,
            json!({ "action": "add", "when": { "min_width": 900, "max_width": 100 } }),
        );
        assert_eq!(data_code(&err), Some("ui_designer_bad_when"));
        let (d, err) = call_err(d, json!({ "action": "reset", "node": "#play" }));
        assert_eq!(data_code(&err), Some("ui_designer_bad_variant"));
        let (_, err) = call_err(d, json!({ "action": "fly" }));
        assert_eq!(err.code, INVALID_PARAMS);
        for code in VARIANT_ERROR_CODES {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    /// A canvas move through `editor.ui_designer_edit` `transform` while a variant is selected starts
    /// from the variant's position and writes the variant's override; the base keeps its bytes.
    #[test]
    fn a_transform_in_a_variant_moves_the_override_not_the_base() {
        let (d, _) = call(designer(), json!({ "action": "select", "index": 0 }));
        let base_before = base_text(d.doc().expect("doc").file());
        let mut world = World::new();
        world.insert_resource(d);
        let table = crate::ui_designer_agent::methods();
        let (_, edit) = table
            .iter()
            .find(|(s, _)| s.name == "editor.ui_designer_edit")
            .expect("edit method");
        let out = edit(
            &mut world,
            &json!({ "op": "transform", "nodes": ["#play"], "delta": [0, 50], "snap": false }),
        )
        .unwrap_or_else(|e| panic!("transform: {e:?}"));
        let d = world.remove_resource::<UiDesigner>().expect("designer");
        let doc = d.doc().expect("doc");
        assert_eq!(base_text(doc.file()), base_before, "the base is unchanged: {out}");
        // A canvas move sets the whole `style`, which replaces the narrower `style.inset` override.
        let keys: Vec<&String> = doc.file().variants[0].overrides["play"].keys().collect();
        assert_eq!(keys, ["style"], "{out}");
        let node = inspected_node(doc, &[0]).expect("play");
        let Dimension::Points(top) = node.style.inset.top else {
            panic!("points: {:?}", node.style.inset);
        };
        assert!(
            top > 600.0,
            "moved down from the variant's 600, not the base's 40: {top}"
        );
    }

    #[test]
    fn an_unreachable_variant_is_reported_and_keeps_the_size() {
        let mut d = UiDesigner::default();
        d.open_text(
            "(version: 11, root: (), variants: [(when: ()), (when: (orientation: portrait))])",
            None,
        )
        .expect("open");
        let doc = d.doc_mut().expect("doc");
        let size = doc.preview.size;
        assert!(
            !select(doc, Some(1)).expect("select"),
            "the first variant matches every size"
        );
        assert_eq!(doc.preview.size, size);
        assert_eq!(active(doc.file(), &doc.preview), Some(1), "still edited");
        assert_eq!(
            sample_size(&doc.file().variants, None),
            None,
            "the base never shows"
        );
    }

    // ------------------------------------------------------------------ keyboard path (a11y bar)

    struct Bar {
        ctx: egui::Context,
        designer: UiDesigner,
        error: Option<String>,
        /// Ask egui for a second pass of every frame (as a layout change does); it sees the same input.
        discard: bool,
        /// UI passes run by the last frame.
        passes: usize,
    }

    impl Bar {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                designer: designer(),
                error: None,
                discard: false,
                passes: 0,
            }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let (designer, error, discard) = (&mut self.designer, &mut self.error, self.discard);
            let mut passes = 0;
            let mut output = self.ctx.run_ui(raw, |ui| {
                passes += 1;
                if discard && ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: a second pass of the frame");
                }
                let Ok(doc) = designer.doc_mut() else {
                    return;
                };
                variant_bar(ui, doc, error);
                let path = doc.selection.clone().unwrap_or_default();
                override_rows(ui, doc, &path, error);
            });
            output.textures_delta.clear();
            self.passes = passes;
        }

        fn focus(&mut self, key: &str) {
            let id = self
                .ctx
                .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("ui-designer-test-id", key))))
                .unwrap_or_else(|| panic!("no id for {key}"));
            self.ctx.memory_mut(|m| m.request_focus(id));
            self.frame(Vec::new());
        }

        fn press(&mut self, key: egui::Key) {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(Vec::new());
        }

        /// A primary click (move, press, release in one frame) at the center of `key`'s widget.
        fn click(&mut self, key: &str) {
            let rect = self
                .ctx
                .data_mut(|d| d.get_temp::<egui::Rect>(egui::Id::new(("ui-designer-test-rect", key))))
                .unwrap_or_else(|| panic!("no rect for {key}"));
            let pos = rect.center();
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![button(true), button(false)]);
            self.frame(Vec::new());
        }

        fn doc(&mut self) -> &mut UiDesign {
            self.designer.doc_mut().expect("doc")
        }
    }

    /// The accessibility bar: the switcher, Add, Remove and each override's Reset work from the
    /// keyboard alone (focus, then Space or Enter), with no pointer event.
    #[test]
    fn the_variant_switcher_and_reset_work_from_the_keyboard() {
        let mut bar = Bar::new();
        bar.doc().select(Some(vec![0])).expect("select play");
        bar.frame(Vec::new());
        bar.focus("variant:0");
        bar.press(egui::Key::Space);
        assert_eq!(
            bar.doc().preview.variant,
            Some(0),
            "Space on the chip selects the variant"
        );
        assert!(
            bar.doc().preview.size[1] > bar.doc().preview.size[0],
            "portrait preview"
        );
        bar.focus("override-reset:style.inset");
        bar.press(egui::Key::Enter);
        assert!(
            bar.doc().file().variants[0].overrides.is_empty(),
            "Enter on Reset removes the override: {:?}",
            bar.error
        );
        bar.focus("variant:base");
        bar.press(egui::Key::Space);
        assert_eq!(bar.doc().preview.variant, None, "back to Base");
        // The document's variant is portrait, so a landscape one is the one a preview size can show.
        bar.focus("variant:add-landscape");
        bar.press(egui::Key::Enter);
        assert_eq!(bar.doc().file().variants.len(), 2, "a variant was added");
        assert_eq!(bar.doc().preview.variant, Some(1), "and selected");
        bar.focus("variant:remove");
        bar.press(egui::Key::Space);
        assert_eq!(bar.doc().file().variants.len(), 1, "and removed");
        assert_eq!(bar.doc().preview.variant, None);
        assert_eq!(bar.error, None);
    }
    /// egui runs a frame's UI again when a pass asks for a discard (an added chip changes the toolbar's
    /// layout). Review g17 row 12 expected that pass to see the same press again; egui 0.36 hands the
    /// frame's events to the first pass only, so one Add adds one variant and one undo step. This locks it.
    #[test]
    fn one_add_on_a_frame_with_a_second_pass_adds_one_variant() {
        for (button, orientation) in [
            ("variant:add-portrait", Orientation::Portrait),
            ("variant:add-landscape", Orientation::Landscape),
        ] {
            let mut bar = Bar::new();
            bar.frame(Vec::new());
            let (variants, undo) = (bar.doc().file().variants.len(), bar.doc().undo_len());
            bar.discard = true;
            bar.focus(button);
            bar.press(egui::Key::Enter);
            assert_eq!(bar.passes, 2, "the frame ran a second pass");
            let added: Vec<Option<Orientation>> = bar.doc().file().variants[variants..]
                .iter()
                .map(|v| v.when.orientation)
                .collect();
            assert_eq!(added, [Some(orientation)], "{button}: one variant added");
            assert_eq!(bar.doc().undo_len(), undo + 1, "{button}: one undo step");
            assert_eq!(
                bar.doc().preview.variant,
                Some(variants),
                "{button}: and selected"
            );
        }
    }

    /// The pointer form of the test above: the click frame (press and release) runs a second pass.
    #[test]
    fn one_add_click_on_a_frame_with_a_second_pass_adds_one_variant() {
        for (button, orientation) in [
            ("variant:add-portrait", Orientation::Portrait),
            ("variant:add-landscape", Orientation::Landscape),
        ] {
            let mut bar = Bar::new();
            bar.frame(Vec::new());
            let (variants, undo) = (bar.doc().file().variants.len(), bar.doc().undo_len());
            bar.discard = true;
            bar.click(button);
            assert_eq!(bar.passes, 2, "the frame ran a second pass");
            let added: Vec<Option<Orientation>> = bar.doc().file().variants[variants..]
                .iter()
                .map(|v| v.when.orientation)
                .collect();
            assert_eq!(added, [Some(orientation)], "{button}: one variant added");
            assert_eq!(bar.doc().undo_len(), undo + 1, "{button}: one undo step");
        }
    }

    /// The same second pass on an override's Reset (the rows below shift up into the removed row's
    /// widget ids): one Reset resets one field.
    #[test]
    fn one_reset_on_a_frame_with_a_second_pass_resets_one_override() {
        let mut bar = Bar::new();
        let doc = bar.doc();
        select(doc, Some(0)).expect("select the variant");
        doc.select(Some(vec![0])).expect("select play");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "opacity".to_owned(),
            value: json!(0.5),
        })
        .expect("a second override");
        let mut keys: Vec<String> = bar.doc().file().variants[0].overrides["play"]
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(keys, ["opacity", "style.inset"]);
        let undo = bar.doc().undo_len();
        bar.frame(Vec::new());
        bar.discard = true;
        // The first row (sorted keys) is the one whose ids the next row inherits on the second pass.
        let first = overridden_keys(bar.doc().file(), 0, &[0])
            .into_iter()
            .next()
            .expect("a row");
        bar.focus(&format!("override-reset:{first}"));
        bar.press(egui::Key::Enter);
        assert_eq!(bar.passes, 2, "the frame ran a second pass");
        let left: Vec<String> = bar.doc().file().variants[0].overrides["play"]
            .keys()
            .cloned()
            .collect();
        assert_eq!(left.len(), 1, "one override reset, the other kept: {left:?}");
        assert!(!left.contains(&first), "the pressed one went: {left:?}");
        assert_eq!(bar.doc().undo_len(), undo + 1, "one undo step");
        assert_eq!(bar.error, None);
    }
}
