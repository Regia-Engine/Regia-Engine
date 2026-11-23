// SPDX-License-Identifier: MPL-2.0
//! The Parameters tab of the procedural graph panel (spec 67 § 2.1): declare, rename (every `Bind`
//! follows), reorder, set type, default, range and doc. Each [`ParamAction`] is one undo entry; the
//! agent's `editor.procgen_param` applies several in one entry through [`apply_param_actions`].
//! On an open variant the default column edits the variant's override (reset removes it); the
//! declaration itself is the parent's and read-only.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_procgen::graph::ParamDecl;
use regia_procgen::{AttrType, ProcGraph, SettingValue};

use super::doc::{self, DocError, EditCtx, ProcGraphDoc};

/// One edit of the Parameters tab.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamAction {
    /// Declare a parameter of a type, with that type's zero default.
    Declare {
        /// The new name.
        name: String,
        /// Its type.
        ty: AttrType,
    },
    /// Rename a parameter and every `Bind` of it.
    Rename {
        /// The current name.
        from: String,
        /// The new name.
        to: String,
    },
    /// Move a parameter to position `to` in the list.
    Move {
        /// The parameter.
        name: String,
        /// The new index (clamped to the list).
        to: usize,
    },
    /// Change a parameter's type; the default converts when it can (see [`convert_default`]).
    SetType {
        /// The parameter.
        name: String,
        /// The new type.
        ty: AttrType,
    },
    /// Set the default (on a variant: the override; `None` resets the override).
    SetDefault {
        /// The parameter.
        name: String,
        /// The value, or `None` to reset a variant's override.
        value: Option<SettingValue>,
    },
    /// Set the inclusive range of a numeric parameter (`None` = unbounded).
    SetRange {
        /// The parameter.
        name: String,
        /// Minimum.
        min: Option<f64>,
        /// Maximum.
        max: Option<f64>,
    },
    /// Set the one-line doc.
    SetDoc {
        /// The parameter.
        name: String,
        /// The doc text.
        doc: String,
    },
    /// Remove a parameter; settings bound to it get its default.
    Remove {
        /// The parameter.
        name: String,
    },
}

impl ParamAction {
    fn label(&self) -> String {
        match self {
            Self::Declare { name, .. } => format!("add parameter {name}"),
            Self::Rename { from, to } => format!("rename parameter {from} to {to}"),
            Self::Move { name, .. } => format!("move parameter {name}"),
            Self::SetType { name, .. } => format!("set type of {name}"),
            Self::SetDefault { name, .. } => format!("set parameter {name}"),
            Self::SetRange { name, .. } => format!("set range of {name}"),
            Self::SetDoc { name, .. } => format!("set doc of {name}"),
            Self::Remove { name } => format!("remove parameter {name}"),
        }
    }
}

fn unknown(name: &str) -> DocError {
    DocError::new("procgen_unknown_param", format!("no parameter `{name}`"))
}

fn decl_mut<'a>(g: &'a mut ProcGraph, name: &str) -> Result<&'a mut ParamDecl, DocError> {
    g.params
        .iter_mut()
        .find(|p| p.name == name)
        .ok_or_else(|| unknown(name))
}

/// The zero value of a parameter type (a declared parameter's first default).
#[must_use]
pub fn zero_default(ty: AttrType) -> SettingValue {
    let floats = |n: usize| SettingValue::Tuple(vec![SettingValue::Float(0.0); n]);
    match ty {
        AttrType::Bool => SettingValue::Bool(false),
        AttrType::Int => SettingValue::Int(0),
        AttrType::Float => SettingValue::Float(0.0),
        AttrType::Vec2 => floats(2),
        AttrType::Vec3 => floats(3),
        AttrType::Vec4 => floats(4),
        AttrType::Quat => SettingValue::Tuple(vec![
            SettingValue::Float(0.0),
            SettingValue::Float(0.0),
            SettingValue::Float(0.0),
            SettingValue::Float(1.0),
        ]),
        AttrType::Transform => SettingValue::Record(
            None,
            vec![
                ("t".into(), floats(3)),
                (
                    "r".into(),
                    SettingValue::Tuple(vec![
                        SettingValue::Float(0.0),
                        SettingValue::Float(0.0),
                        SettingValue::Float(0.0),
                        SettingValue::Float(1.0),
                    ]),
                ),
                ("s".into(), SettingValue::Tuple(vec![SettingValue::Float(1.0); 3])),
            ],
        ),
        AttrType::Name | AttrType::Asset => SettingValue::Str(String::new()),
    }
}

fn numbers(v: &SettingValue) -> Option<Vec<f64>> {
    match v {
        SettingValue::Tuple(items) | SettingValue::List(items) => {
            items.iter().map(SettingValue::as_f64).collect()
        }
        _ => None,
    }
}

/// Whether `v` is a value of type `ty` (numbers widen: an integer is a valid float).
#[must_use]
pub fn value_fits(ty: AttrType, v: &SettingValue) -> bool {
    match ty {
        AttrType::Bool => matches!(v, SettingValue::Bool(_)),
        AttrType::Int => matches!(v, SettingValue::Int(_)),
        AttrType::Float => matches!(v, SettingValue::Int(_) | SettingValue::Float(_)),
        AttrType::Vec2 | AttrType::Vec3 | AttrType::Vec4 | AttrType::Quat => {
            numbers(v).is_some_and(|n| Some(n.len()) == ty.width())
        }
        AttrType::Transform => matches!(v, SettingValue::Record(..) | SettingValue::Tuple(_)),
        AttrType::Name | AttrType::Asset => matches!(v, SettingValue::Str(_) | SettingValue::Ident(_)),
    }
}

/// The default a parameter keeps when its type changes to `ty`: numbers convert (an integer
/// rounds), a name stays a name or asset path, vectors keep their leading components; anything
/// else becomes [`zero_default`].
#[must_use]
pub fn convert_default(v: &SettingValue, ty: AttrType) -> SettingValue {
    match (ty, v) {
        (AttrType::Float, SettingValue::Int(i)) => SettingValue::Float(*i as f64),
        (AttrType::Float, SettingValue::Float(f)) => SettingValue::Float(*f),
        (AttrType::Int, SettingValue::Float(f)) if f.is_finite() => SettingValue::Int(f.round() as i64),
        (AttrType::Int, SettingValue::Int(i)) => SettingValue::Int(*i),
        (AttrType::Name | AttrType::Asset, SettingValue::Str(s) | SettingValue::Ident(s)) => {
            SettingValue::Str(s.clone())
        }
        (AttrType::Vec2 | AttrType::Vec3 | AttrType::Vec4, _) => match (numbers(v), ty.width()) {
            (Some(n), Some(w)) => SettingValue::Tuple(
                (0..w)
                    .map(|i| SettingValue::Float(n.get(i).copied().unwrap_or(0.0)))
                    .collect(),
            ),
            _ => zero_default(ty),
        },
        _ if value_fits(ty, v) => v.clone(),
        _ => zero_default(ty),
    }
}

fn apply_one(c: &mut EditCtx<'_>, action: &ParamAction) -> Result<(), DocError> {
    match action {
        ParamAction::Declare { name, ty } => doc::add_param(
            c.graph,
            ParamDecl {
                name: name.clone(),
                ty: *ty,
                default: zero_default(*ty),
                min: None,
                max: None,
                doc: String::new(),
            },
        ),
        ParamAction::Rename { from, to } => doc::rename_param(c.graph, from, to).map(|_| ()),
        ParamAction::Move { name, to } => doc::reorder_param(c.graph, name, *to),
        ParamAction::SetType { name, ty } => {
            let p = decl_mut(c.graph, name)?;
            if p.ty != *ty {
                p.default = convert_default(&p.default, *ty);
                p.ty = *ty;
                if ty.width() != Some(1) {
                    p.min = None;
                    p.max = None;
                }
            }
            Ok(())
        }
        ParamAction::SetDefault { name, value } => {
            let ty = c.graph.param(name).ok_or_else(|| unknown(name))?.ty;
            if let Some(v) = value
                && !value_fits(ty, v)
            {
                return Err(DocError::new(
                    "procgen_param_type",
                    format!("`{}` is not a {} value", v.write(), ty.name()),
                ));
            }
            let value = value.as_ref().map(|v| convert_default(v, ty));
            doc::set_param_value(c, name, value)
        }
        ParamAction::SetRange { name, min, max } => {
            let p = decl_mut(c.graph, name)?;
            if (min.is_some() || max.is_some()) && p.ty.width() != Some(1) {
                return Err(DocError::new(
                    "procgen_param_range",
                    format!("`{name}` is {}: only number parameters have a range", p.ty.name()),
                ));
            }
            if let (Some(lo), Some(hi)) = (min, max)
                && lo > hi
            {
                return Err(DocError::new(
                    "procgen_param_range",
                    format!("min {lo} is above max {hi}"),
                ));
            }
            if min.is_some_and(|v| !v.is_finite()) || max.is_some_and(|v| !v.is_finite()) {
                return Err(DocError::new("procgen_param_range", "the range is not finite"));
            }
            p.min = *min;
            p.max = *max;
            Ok(())
        }
        ParamAction::SetDoc { name, doc } => {
            decl_mut(c.graph, name)?.doc.clone_from(doc);
            Ok(())
        }
        ParamAction::Remove { name } => doc::remove_param(c.graph, name),
    }
}

/// Applies one Parameters tab action as one undo entry (slider-style default edits of one
/// parameter coalesce).
///
/// # Errors
///
/// `procgen_not_open`, `procgen_read_only` (declaration edits on an open variant),
/// `procgen_unknown_param`, `procgen_param_name`, `procgen_param_type`, `procgen_param_range`,
/// `procgen_param_value`. On error nothing is written.
pub fn apply_param_action(world: &mut World, action: ParamAction) -> Result<(), DocError> {
    let merge = match &action {
        ParamAction::SetDefault { name, .. } => Some(format!("param:{name}")),
        _ => None,
    };
    doc::edit(world, &action.label(), merge, |c| apply_one(c, &action))
}

/// Applies several actions in order as one undo entry (all or none).
///
/// # Errors
///
/// As [`apply_param_action`].
pub fn apply_param_actions(world: &mut World, label: &str, actions: &[ParamAction]) -> Result<(), DocError> {
    doc::edit(world, label, None, |c| {
        actions.iter().try_for_each(|a| apply_one(c, a))
    })
}

/// Drafts of the tab's text fields (names and docs being typed), by parameter name.
#[derive(Default)]
pub(crate) struct ParamsTabUi {
    names: BTreeMap<String, String>,
    docs: BTreeMap<String, String>,
    new_name: String,
    new_ty: Option<AttrType>,
    /// The document revision the drafts belong to.
    rev: Option<u64>,
    /// Rows drawn in the last frame (tests read it through `panel::param_rows_drawn`).
    pub(crate) rows_drawn: usize,
}

fn type_combo(ui: &mut egui::Ui, salt: (&str, &str), current: AttrType) -> Option<AttrType> {
    let mut picked = None;
    egui::ComboBox::from_id_salt(salt)
        .selected_text(current.name())
        .width(80.0)
        .show_ui(ui, |ui| {
            for t in AttrType::ALL {
                if ui.selectable_label(t == current, t.name()).clicked() && t != current {
                    picked = Some(t);
                }
            }
        });
    picked
}

/// A typed editor of one parameter value; returns the new value when it changed.
fn value_ui(ui: &mut egui::Ui, p: &ParamDecl, shown: &SettingValue) -> Option<SettingValue> {
    match p.ty {
        AttrType::Float | AttrType::Int => {
            let mut v = shown.as_f64().unwrap_or(0.0);
            let lo = p.min.unwrap_or(f64::NEG_INFINITY);
            let hi = p.max.unwrap_or(f64::INFINITY);
            let mut drag = egui::DragValue::new(&mut v).speed(0.01).range(lo..=hi);
            if p.ty == AttrType::Int {
                drag = drag.speed(0.2).fixed_decimals(0);
            }
            ui.add(drag).changed().then(|| {
                if p.ty == AttrType::Int {
                    SettingValue::Int(v.round() as i64)
                } else {
                    SettingValue::Float(v)
                }
            })
        }
        AttrType::Bool => {
            let mut b = matches!(shown, SettingValue::Bool(true));
            crate::a11y_audit::bare_checkbox(ui, &mut b, &p.name)
                .changed()
                .then_some(SettingValue::Bool(b))
        }
        AttrType::Vec2 | AttrType::Vec3 | AttrType::Vec4 | AttrType::Quat => {
            let w = p.ty.width().unwrap_or(3);
            let mut n = numbers(shown).unwrap_or_default();
            n.resize(w, 0.0);
            let mut changed = false;
            for c in &mut n {
                changed |= ui.add(egui::DragValue::new(c).speed(0.01)).changed();
            }
            changed.then(|| SettingValue::Tuple(n.into_iter().map(SettingValue::Float).collect()))
        }
        AttrType::Name | AttrType::Asset => {
            let mut text = shown.as_text().unwrap_or("").to_string();
            let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(120.0));
            r.changed().then_some(SettingValue::Str(text))
        }
        AttrType::Transform => {
            ui.monospace(shown.write());
            None
        }
    }
}

/// Draws the Parameters tab and pushes its actions.
pub(crate) fn params_ui(
    ui: &mut egui::Ui,
    d: &ProcGraphDoc,
    tab: &mut ParamsTabUi,
    out: &mut Vec<ParamAction>,
) {
    if tab.rev != Some(d.revision) {
        tab.names.clear();
        tab.docs.clear();
        tab.rev = Some(d.revision);
    }
    let read_only = d.read_only();
    let overrides = d.variant.as_ref().map(|v| &v.variant.overrides);
    if read_only {
        ui.weak(
            "Variant: the declarations are the parent's; the value column edits this variant's overrides.",
        );
    }
    let count = d.graph.params.len();
    let mut rows = 0;
    egui::ScrollArea::vertical()
        .id_salt("procgen-params")
        .show(ui, |ui| {
            for (i, p) in d.graph.params.iter().enumerate() {
                rows += 1;
                ui.push_id(("procgen-param", i), |ui| {
                    ui.horizontal(|ui| {
                        // Name: typed in a draft, renamed on Enter or focus loss when valid.
                        let draft = tab.names.entry(p.name.clone()).or_insert_with(|| p.name.clone());
                        let valid = *draft == p.name
                            || (regia_procgen::graph::valid_param_name(draft)
                                && d.graph.param(draft).is_none());
                        let color = if valid {
                            ui.visuals().text_color()
                        } else {
                            egui::Color32::from_rgb(230, 80, 70)
                        };
                        let r = ui.add_enabled(
                            !read_only,
                            egui::TextEdit::singleline(draft)
                                .desired_width(110.0)
                                .text_color(color),
                        );
                        let r = if valid {
                            r
                        } else {
                            r.on_hover_text("Not a new, valid parameter name")
                        };
                        if r.lost_focus() && valid && *draft != p.name {
                            out.push(ParamAction::Rename {
                                from: p.name.clone(),
                                to: draft.clone(),
                            });
                        }
                        if !read_only {
                            if ui.add_enabled(i > 0, egui::Button::new("▲").small()).clicked() {
                                out.push(ParamAction::Move {
                                    name: p.name.clone(),
                                    to: i - 1,
                                });
                            }
                            if ui
                                .add_enabled(i + 1 < count, egui::Button::new("▼").small())
                                .clicked()
                            {
                                out.push(ParamAction::Move {
                                    name: p.name.clone(),
                                    to: i + 1,
                                });
                            }
                            if let Some(ty) = type_combo(ui, ("procgen-param-ty", p.name.as_str()), p.ty) {
                                out.push(ParamAction::SetType {
                                    name: p.name.clone(),
                                    ty,
                                });
                            }
                        } else {
                            ui.weak(p.ty.name());
                        }
                        let over = overrides.and_then(|o| o.get(&p.name));
                        let shown = over.unwrap_or(&p.default);
                        if let Some(v) = value_ui(ui, p, shown) {
                            out.push(ParamAction::SetDefault {
                                name: p.name.clone(),
                                value: Some(v),
                            });
                        }
                        if over.is_some() && ui.small_button("reset").clicked() {
                            out.push(ParamAction::SetDefault {
                                name: p.name.clone(),
                                value: None,
                            });
                        }
                        if !read_only && ui.small_button("remove").clicked() {
                            out.push(ParamAction::Remove { name: p.name.clone() });
                        }
                    });
                    if read_only {
                        if !p.doc.is_empty() {
                            ui.weak(&p.doc);
                        }
                        return;
                    }
                    ui.horizontal(|ui| {
                        if p.ty.width() == Some(1) {
                            let mut has_min = p.min.is_some();
                            let mut has_max = p.max.is_some();
                            let mut lo = p.min.unwrap_or(0.0);
                            let mut hi = p.max.unwrap_or(1.0);
                            let mut changed = ui.checkbox(&mut has_min, "min").changed();
                            changed |= ui
                                .add_enabled(has_min, egui::DragValue::new(&mut lo).speed(0.01))
                                .changed();
                            changed |= ui.checkbox(&mut has_max, "max").changed();
                            changed |= ui
                                .add_enabled(has_max, egui::DragValue::new(&mut hi).speed(0.01))
                                .changed();
                            if changed {
                                out.push(ParamAction::SetRange {
                                    name: p.name.clone(),
                                    min: has_min.then_some(lo),
                                    max: has_max.then_some(hi.max(if has_min { lo } else { hi })),
                                });
                            }
                        }
                        let doc_draft = tab.docs.entry(p.name.clone()).or_insert_with(|| p.doc.clone());
                        let r = ui.add(
                            egui::TextEdit::singleline(doc_draft)
                                .hint_text("doc")
                                .desired_width(160.0),
                        );
                        if r.lost_focus() && *doc_draft != p.doc {
                            out.push(ParamAction::SetDoc {
                                name: p.name.clone(),
                                doc: doc_draft.clone(),
                            });
                        }
                    });
                    ui.separator();
                });
            }
        });
    tab.rows_drawn = rows;
    if read_only {
        return;
    }
    ui.horizontal(|ui| {
        let valid =
            regia_procgen::graph::valid_param_name(&tab.new_name) && d.graph.param(&tab.new_name).is_none();
        crate::a11y_audit::name(
            ui.add(
                egui::TextEdit::singleline(&mut tab.new_name)
                    .hint_text("new parameter")
                    .desired_width(110.0),
            ),
            egui::WidgetType::TextEdit,
            "New parameter name",
        );
        let ty = tab.new_ty.unwrap_or(AttrType::Float);
        if let Some(t) = type_combo(ui, ("procgen-param-new", ""), ty) {
            tab.new_ty = Some(t);
        }
        if ui.add_enabled(valid, egui::Button::new("Add")).clicked() {
            out.push(ParamAction::Declare {
                name: std::mem::take(&mut tab.new_name),
                ty,
            });
        }
    });
}
