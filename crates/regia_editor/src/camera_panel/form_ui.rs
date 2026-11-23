// SPDX-License-Identifier: MPL-2.0
//! The typed widgets of the Cameras panel's forms (ADR-7734): one widget per form value of a node,
//! transition or rig ([`super::edit::Form`]), the same value kinds the ability form draws. Each
//! widget returns the edit it made; the panel turns it into one `editor.camera_panel_edit` op.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::abilities_panel::form::tree;

/// One form control's edit.
#[derive(Clone, Debug, PartialEq)]
pub enum FormEdit {
    /// Set the value at the field path.
    Set(String, Value),
    /// Append a default-filled entry to the list at the path.
    Add(String),
    /// Switch the enum at the path to the variant.
    Variant(String, String),
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// The rows of a form record (`fields`): one per field, with its widget. `skip` names fields shown
/// as text only (a `Group`'s `nodes` count).
pub fn form_rows(
    ui: &mut egui::Ui,
    id: &str,
    fields: &Value,
    enums: &BTreeMap<String, Vec<&'static str>>,
    skip: &[&str],
) -> Option<FormEdit> {
    let mut edit = None;
    egui::Grid::new(("camera-form", id))
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for (key, value) in fields.as_object().into_iter().flatten() {
                ui.label(key.as_str());
                if skip.contains(&key.as_str()) {
                    ui.weak(value.to_string());
                } else if let Some(e) = form_widget(ui, id, key, value, enums) {
                    edit = Some(e);
                }
                ui.end_row();
            }
        });
    edit
}

fn float_widget(ui: &mut egui::Ui, path: &str, mut f: f64) -> Option<FormEdit> {
    let response = ui.add(egui::DragValue::new(&mut f).speed(0.01).max_decimals(6));
    if crate::a11y_audit::name(response, egui::WidgetType::DragValue, path).changed() {
        serde_json::Number::from_f64(f).map(|n| FormEdit::Set(path.to_owned(), Value::Number(n)))
    } else {
        None
    }
}

/// The widget of the form value at `path`. Lists and records nest, a list adds and removes entries,
/// an enum value picks its variant, a `none` is shown as such.
fn form_widget(
    ui: &mut egui::Ui,
    id: &str,
    path: &str,
    value: &Value,
    enums: &BTreeMap<String, Vec<&'static str>>,
) -> Option<FormEdit> {
    let mut edit = None;
    if let Some(variants) = enums.get(path) {
        let current = match value {
            Value::String(unit) => Some(unit.as_str()),
            Value::Object(fields) if fields.len() == 1 => fields.keys().next().map(String::as_str),
            _ => None,
        };
        if let Some(current) = current {
            egui::ComboBox::from_id_salt(("camera-form-variant", id, path))
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for &variant in variants {
                        if ui.selectable_label(variant == current, variant).clicked() && variant != current {
                            edit = Some(FormEdit::Variant(path.to_owned(), variant.to_owned()));
                        }
                    }
                });
            if value.is_string() {
                return edit;
            }
        }
    }
    match value {
        Value::Bool(on) => {
            let mut on = *on;
            if crate::a11y_audit::bare_checkbox(ui, &mut on, path).changed() {
                edit = Some(FormEdit::Set(path.to_owned(), Value::Bool(on)));
            }
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64().filter(|_| !n.is_f64()) {
                let mut i = i;
                let response = ui.add(egui::DragValue::new(&mut i));
                if crate::a11y_audit::name(response, egui::WidgetType::DragValue, path).changed() {
                    edit = Some(FormEdit::Set(path.to_owned(), Value::from(i)));
                }
            } else if let Some(f) = n.as_f64() {
                edit = float_widget(ui, path, f);
            }
        }
        Value::Object(_) if tree::non_finite(value).is_some() => {
            let spelling = value[tree::NON_FINITE].as_str().unwrap_or("NaN");
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(235, 170, 80), spelling)
                    .on_hover_text("not a finite number in the file: drag or type a value to replace it");
                edit = float_widget(ui, path, 0.0);
            });
        }
        Value::String(text) => {
            let mut text = text.clone();
            let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(160.0));
            if crate::a11y_audit::name(response, egui::WidgetType::TextEdit, path).changed() {
                edit = Some(FormEdit::Set(path.to_owned(), Value::String(text)));
            }
        }
        Value::Null => {
            ui.weak("none");
        }
        Value::Array(items) => {
            ui.vertical(|ui| {
                for (i, item) in items.iter().enumerate() {
                    let child = format!("{path}[{i}]");
                    ui.horizontal(|ui| {
                        ui.weak(format!("[{i}]"));
                        if let Some(e) = form_widget(ui, id, &child, item, enums) {
                            edit = Some(e);
                        }
                        if ui
                            .small_button("Remove entry")
                            .on_hover_text(format!("Remove {child}"))
                            .clicked()
                        {
                            let mut rest = items.clone();
                            rest.remove(i);
                            edit = Some(FormEdit::Set(path.to_owned(), Value::Array(rest)));
                        }
                    });
                }
                if ui
                    .small_button("Add entry")
                    .on_hover_text(format!("Append a default-filled entry to {path}"))
                    .clicked()
                {
                    edit = Some(FormEdit::Add(path.to_owned()));
                }
            });
        }
        Value::Object(fields) => {
            ui.vertical(|ui| {
                for (key, item) in fields {
                    let child = join(path, key);
                    ui.horizontal(|ui| {
                        ui.weak(key.as_str());
                        if let Some(e) = form_widget(ui, id, &child, item, enums) {
                            edit = Some(e);
                        }
                    });
                }
            });
        }
    }
    edit
}
