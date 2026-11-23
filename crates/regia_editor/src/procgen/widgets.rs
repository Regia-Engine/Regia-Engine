// SPDX-License-Identifier: MPL-2.0
//! Typed setting widgets of the procedural graph panel (spec 67 § 2.1, node settings): numbers
//! with ranges and drag, toggles, vectors and ranges as drag fields, enums, a small curve editor,
//! an asset picker and name lists, each generated from the node schema's `SettingType`. A value is
//! checked against the compiler before it is written ([`check_setting`]); an invalid one is shown
//! in red with the message and not written.

use regia_ecs::prelude::*;
use regia_procgen::SettingValue;
use regia_procgen::nodes::{SettingDesc, SettingType};
use regia_procgen::world::{ProcGenSettings, ProcGraphSource};

use super::doc::{self, DocError, EditCtx, ProcGraphDoc};

/// The widget a setting type draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    /// A drag number (with the schema's range).
    Number {
        /// Whole numbers only.
        integer: bool,
    },
    /// A checkbox.
    Toggle,
    /// Three drag numbers.
    Vec3,
    /// Two drag numbers `[lo, hi]`.
    Range2,
    /// Two rows of three drag numbers.
    Vec3Range,
    /// A dropdown of the schema's identifiers.
    Enum,
    /// The small curve editor.
    Curve,
    /// A path field with a project file picker.
    Asset,
    /// A text field.
    Name,
    /// A list of text fields with add and remove.
    NameList,
    /// RON text (types the node checks itself).
    Text,
}

impl WidgetKind {
    /// The agent name (`number`, `integer`, `toggle`, `vec3`, `range2`, `vec3_range`, `enum`,
    /// `curve`, `asset`, `name`, `name_list`, `text`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Number { integer: false } => "number",
            Self::Number { integer: true } => "integer",
            Self::Toggle => "toggle",
            Self::Vec3 => "vec3",
            Self::Range2 => "range2",
            Self::Vec3Range => "vec3_range",
            Self::Enum => "enum",
            Self::Curve => "curve",
            Self::Asset => "asset",
            Self::Name => "name",
            Self::NameList => "name_list",
            Self::Text => "text",
        }
    }
}

/// The widget of a setting type.
#[must_use]
pub fn widget_kind(ty: SettingType) -> WidgetKind {
    match ty {
        SettingType::Float => WidgetKind::Number { integer: false },
        SettingType::Int => WidgetKind::Number { integer: true },
        SettingType::Bool => WidgetKind::Toggle,
        SettingType::Vec3 => WidgetKind::Vec3,
        SettingType::Range2 => WidgetKind::Range2,
        SettingType::Vec3Range => WidgetKind::Vec3Range,
        SettingType::Enum(_) => WidgetKind::Enum,
        SettingType::Curve => WidgetKind::Curve,
        SettingType::Asset => WidgetKind::Asset,
        SettingType::Name | SettingType::Selector => WidgetKind::Name,
        SettingType::NameList => WidgetKind::NameList,
        SettingType::Any => WidgetKind::Text,
    }
}

fn pair(v: &SettingValue) -> Option<[f64; 2]> {
    match v.as_items()? {
        [a, b] => Some([a.as_f64()?, b.as_f64()?]),
        _ => None,
    }
}

/// The `(x, y)` keys of a curve value, in file order.
#[must_use]
pub fn curve_keys(v: &SettingValue) -> Option<Vec<[f64; 2]>> {
    v.as_items()?.iter().map(pair).collect()
}

/// A curve value from keys (`[(x, y), ...]`).
#[must_use]
pub fn curve_value(keys: &[[f64; 2]]) -> SettingValue {
    SettingValue::List(
        keys.iter()
            .map(|k| SettingValue::Tuple(vec![SettingValue::Float(k[0]), SettingValue::Float(k[1])]))
            .collect(),
    )
}

/// Inserts a key keeping the keys sorted by x (after any key with the same x); returns its index.
pub fn curve_insert(keys: &mut Vec<[f64; 2]>, key: [f64; 2]) -> usize {
    let at = keys.partition_point(|k| k[0] <= key[0]);
    keys.insert(at, key);
    at
}

/// Moves key `index` to `to`, its x held between its neighbors' so the keys stay sorted.
pub fn curve_move(keys: &mut [[f64; 2]], index: usize, to: [f64; 2]) {
    if index >= keys.len() {
        return;
    }
    let lo = if index > 0 {
        keys[index - 1][0]
    } else {
        f64::NEG_INFINITY
    };
    let hi = keys.get(index + 1).map_or(f64::INFINITY, |k| k[0]);
    keys[index] = [to[0].clamp(lo, hi), to[1]];
}

/// Removes key `index`; a curve keeps at least two keys.
pub fn curve_remove(keys: &mut Vec<[f64; 2]>, index: usize) {
    if keys.len() > 2 && index < keys.len() {
        keys.remove(index);
    }
}

/// The names of a name-list value.
#[must_use]
pub fn name_list(v: &SettingValue) -> Option<Vec<String>> {
    v.as_items()?
        .iter()
        .map(|x| x.as_text().map(str::to_string))
        .collect()
}

/// A name-list value.
#[must_use]
pub fn name_list_value(names: &[String]) -> SettingValue {
    SettingValue::List(names.iter().map(|n| SettingValue::Str(n.clone())).collect())
}

/// A 3-vector value `(x, y, z)`.
#[must_use]
pub fn vec3_value(v: [f64; 3]) -> SettingValue {
    SettingValue::Tuple(v.iter().map(|c| SettingValue::Float(*c)).collect())
}

/// A pair value `[lo, hi]`.
#[must_use]
pub fn pair_value(v: [f64; 2]) -> SettingValue {
    SettingValue::List(v.iter().map(|c| SettingValue::Float(*c)).collect())
}

/// Checks a setting value against the compiler without writing it (what the widgets do before
/// they write).
///
/// # Errors
///
/// `procgen_not_open`, `procgen_unknown_node`, or the compiler's setting code
/// (`unknown_setting`, `setting_type`, `setting_out_of_range`, ...).
pub fn check_setting(world: &World, node: u32, setting: &str, value: &SettingValue) -> Result<(), DocError> {
    let registry = doc::registry(world);
    let d = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let mut graph = d.graph.clone();
    let mut ctx = EditCtx {
        graph: &mut graph,
        overrides: None,
        registry: &registry,
    };
    doc::set_setting(&mut ctx, node, setting, value.clone())
}

/// Most entries the asset picker lists.
pub const ASSET_CHOICES_MAX: usize = 200;

/// Project files whose path contains `search` (case-insensitive) for the asset picker, sorted:
/// files under the graph source directory, else under the agent's project directory (hidden
/// directories and `target` skipped, at most 20 000 entries walked, [`ASSET_CHOICES_MAX`] listed).
#[must_use]
pub fn asset_choices(world: &World, search: &str) -> Vec<String> {
    let needle = search.to_lowercase();
    let keep = |p: &str| needle.is_empty() || p.to_lowercase().contains(&needle);
    let root = match world.get_resource::<ProcGenSettings>().map(|s| &s.source) {
        Some(ProcGraphSource::Dir(root)) => Some(root.clone()),
        Some(ProcGraphSource::Memory(files)) => {
            let from_agent = world
                .get_resource::<regia_agent::AgentSettings>()
                .and_then(|a| a.project_dir.clone());
            if from_agent.is_none() {
                let mut out: Vec<String> = files.keys().filter(|k| keep(k)).cloned().collect();
                out.truncate(ASSET_CHOICES_MAX);
                return out;
            }
            from_agent
        }
        None => world
            .get_resource::<regia_agent::AgentSettings>()
            .and_then(|a| a.project_dir.clone()),
    };
    let Some(root) = root else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    let mut seen = 0usize;
    'walk: while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            seen += 1;
            if seen > 20_000 {
                break 'walk;
            }
            let p = entry.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                if name != "target" {
                    stack.push(p);
                }
            } else if let Ok(rel) = p.strip_prefix(&root) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if keep(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    out.truncate(ASSET_CHOICES_MAX);
    out
}

/// Draft state of the widgets (asset searches and new list entries), by `(node, setting)`.
#[derive(Default)]
pub(crate) struct WidgetDrafts {
    asset_search: std::collections::BTreeMap<(u32, String), String>,
    new_name: std::collections::BTreeMap<(u32, String), String>,
    /// The curve key being dragged: `(node, setting, index)`.
    dragging: Option<(u32, String, usize)>,
}

fn drag_numbers(ui: &mut egui::Ui, values: &mut [f64], lo: f64, hi: f64) -> bool {
    let mut changed = false;
    for v in values {
        changed |= ui
            .add(egui::DragValue::new(v).speed(0.01).range(lo..=hi))
            .changed();
    }
    changed
}

/// The small curve editor: keys on a grid, drag a key, double-click empty space to add one,
/// right-click a key to remove it. Returns the new keys when they changed.
fn curve_ui(
    ui: &mut egui::Ui,
    keys: &[[f64; 2]],
    key: (u32, &str),
    drafts: &mut WidgetDrafts,
) -> Option<Vec<[f64; 2]>> {
    let size = egui::vec2(ui.available_width().clamp(120.0, 220.0), 90.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let (mut x0, mut x1, mut y0, mut y1) = (0.0_f64, 1.0_f64, 0.0_f64, 1.0_f64);
    for k in keys {
        x0 = x0.min(k[0]);
        x1 = x1.max(k[0]);
        y0 = y0.min(k[1]);
        y1 = y1.max(k[1]);
    }
    let to_screen = |k: [f64; 2]| {
        egui::pos2(
            rect.left() + ((k[0] - x0) / (x1 - x0).max(1e-9)) as f32 * rect.width(),
            rect.bottom() - ((k[1] - y0) / (y1 - y0).max(1e-9)) as f32 * rect.height(),
        )
    };
    let from_screen = |p: egui::Pos2| {
        [
            x0 + f64::from((p.x - rect.left()) / rect.width()) * (x1 - x0),
            y0 + f64::from((rect.bottom() - p.y) / rect.height()) * (y1 - y0),
        ]
    };
    let line = egui::Stroke::new(1.5, ui.visuals().text_color());
    for w in keys.windows(2) {
        painter.line_segment([to_screen(w[0]), to_screen(w[1])], line);
    }
    let pointer = response.interact_pointer_pos().or(response.hover_pos());
    let near = |p: egui::Pos2| keys.iter().position(|k| to_screen(*k).distance(p) <= 6.0);
    for (i, k) in keys.iter().enumerate() {
        let hot = drafts
            .dragging
            .as_ref()
            .is_some_and(|(n, s, d)| (*n, s.as_str(), *d) == (key.0, key.1, i));
        let fill = if hot {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().text_color()
        };
        painter.circle_filled(to_screen(*k), 3.5, fill);
    }
    let mut out = None;
    if response.drag_started()
        && let Some(i) = pointer.and_then(near)
    {
        drafts.dragging = Some((key.0, key.1.to_string(), i));
    }
    if response.dragged()
        && let (Some((n, s, i)), Some(p)) = (drafts.dragging.clone(), pointer)
        && (n, s.as_str()) == key
    {
        let mut next = keys.to_vec();
        curve_move(&mut next, i, from_screen(p));
        out = Some(next);
    }
    if response.drag_stopped() {
        drafts.dragging = None;
    }
    if response.double_clicked()
        && let Some(p) = pointer
        && near(p).is_none()
    {
        let mut next = keys.to_vec();
        curve_insert(&mut next, from_screen(p));
        out = Some(next);
    }
    if response.secondary_clicked()
        && let Some(i) = pointer.and_then(near)
    {
        let mut next = keys.to_vec();
        curve_remove(&mut next, i);
        out = Some(next);
    }
    response.on_hover_text("Drag a key; double-click to add one; right-click a key to remove it");
    out.filter(|next| next.as_slice() != keys)
}

/// Draws the typed widget of one setting; returns a new value when the user changed it.
pub(crate) fn setting_ui(
    ui: &mut egui::Ui,
    world: &World,
    node: u32,
    desc: &SettingDesc,
    current: &SettingValue,
    drafts: &mut WidgetDrafts,
    text_draft: &mut String,
) -> Option<SettingValue> {
    let lo = desc.min.unwrap_or(f64::NEG_INFINITY);
    let hi = desc.max.unwrap_or(f64::INFINITY);
    let key = (node, desc.name.to_string());
    match widget_kind(desc.ty) {
        WidgetKind::Number { integer } => {
            let mut v = current.as_f64().unwrap_or(0.0);
            let mut drag = egui::DragValue::new(&mut v).speed(0.01).range(lo..=hi);
            if integer {
                drag = drag.speed(0.2).fixed_decimals(0);
            }
            ui.add(drag).changed().then(|| {
                if integer {
                    SettingValue::Int(v.round() as i64)
                } else {
                    SettingValue::Float(v)
                }
            })
        }
        WidgetKind::Toggle => {
            let mut b = matches!(current, SettingValue::Bool(true));
            crate::a11y_audit::bare_checkbox(ui, &mut b, desc.name)
                .changed()
                .then_some(SettingValue::Bool(b))
        }
        WidgetKind::Vec3 => {
            let mut v = current.as_vec3().unwrap_or([0.0; 3]);
            drag_numbers(ui, &mut v, lo, hi).then(|| vec3_value(v))
        }
        WidgetKind::Range2 => {
            let mut v = pair(current).unwrap_or([0.0, 1.0]);
            drag_numbers(ui, &mut v, lo, hi).then(|| pair_value(v))
        }
        WidgetKind::Vec3Range => {
            let items = current.as_items().unwrap_or(&[]);
            let mut a = items.first().and_then(SettingValue::as_vec3).unwrap_or([0.0; 3]);
            let mut b = items.get(1).and_then(SettingValue::as_vec3).unwrap_or([0.0; 3]);
            let changed = ui
                .vertical(|ui| {
                    let ca = ui.horizontal(|ui| drag_numbers(ui, &mut a, lo, hi)).inner;
                    let cb = ui.horizontal(|ui| drag_numbers(ui, &mut b, lo, hi)).inner;
                    ca || cb
                })
                .inner;
            changed.then(|| SettingValue::List(vec![vec3_value(a), vec3_value(b)]))
        }
        WidgetKind::Enum => {
            let SettingType::Enum(options) = desc.ty else {
                return None;
            };
            let cur = current.as_text().unwrap_or("");
            let mut picked = None;
            let combo = egui::ComboBox::from_id_salt(("procgen-enum", node, desc.name))
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for o in options {
                        if ui.selectable_label(cur == *o, *o).clicked() && cur != *o {
                            picked = Some(SettingValue::Ident((*o).to_string()));
                        }
                    }
                });
            // Its accessible name is the setting's (ADR-7383).
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, desc.name);
            picked
        }
        WidgetKind::Curve => {
            let keys = curve_keys(current).unwrap_or_else(|| vec![[0.0, 0.0], [1.0, 1.0]]);
            curve_ui(ui, &keys, (node, desc.name), drafts).map(|k| curve_value(&k))
        }
        WidgetKind::Asset => {
            let mut text = current.as_text().unwrap_or("").to_string();
            let mut out = None;
            ui.vertical(|ui| {
                if ui
                    .add(egui::TextEdit::singleline(&mut text).desired_width(160.0))
                    .lost_focus()
                    && Some(text.as_str()) != current.as_text()
                {
                    out = Some(SettingValue::Str(text.clone()));
                }
                let search = drafts.asset_search.entry(key.clone()).or_default();
                egui::ComboBox::from_id_salt(("procgen-asset", node, desc.name))
                    .selected_text("Pick…")
                    .show_ui(ui, |ui| {
                        ui.add(egui::TextEdit::singleline(search).hint_text("search"));
                        // Listed only while the list is open (a project walk).
                        for path in asset_choices(world, search) {
                            if ui.selectable_label(path == text, &path).clicked() {
                                out = Some(SettingValue::Str(path));
                            }
                        }
                    });
            });
            out
        }
        WidgetKind::Name => {
            let mut text = current.as_text().unwrap_or("").to_string();
            let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(140.0));
            (r.lost_focus() && Some(text.as_str()) != current.as_text()).then_some(SettingValue::Str(text))
        }
        WidgetKind::NameList => {
            let mut names = name_list(current).unwrap_or_default();
            let mut changed = false;
            ui.vertical(|ui| {
                let mut remove = None;
                for (i, n) in names.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= ui
                            .add(egui::TextEdit::singleline(n).desired_width(120.0))
                            .lost_focus();
                        if ui.small_button("×").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    names.remove(i);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    let draft = drafts.new_name.entry(key.clone()).or_default();
                    ui.add(
                        egui::TextEdit::singleline(draft)
                            .hint_text("add")
                            .desired_width(120.0),
                    );
                    if ui.small_button("+").clicked() && !draft.is_empty() {
                        names.push(std::mem::take(draft));
                        changed = true;
                    }
                });
            });
            let value = name_list_value(&names);
            (changed && value != *current).then_some(value)
        }
        WidgetKind::Text => {
            let parsed = SettingValue::parse(text_draft);
            let color = if parsed.is_ok() {
                ui.visuals().text_color()
            } else {
                egui::Color32::from_rgb(230, 80, 70)
            };
            let r = crate::a11y_audit::name(
                ui.add(egui::TextEdit::singleline(text_draft).text_color(color)),
                egui::WidgetType::TextEdit,
                desc.name,
            );
            match parsed {
                Ok(v) if r.lost_focus() && v != *current => Some(v),
                _ => None,
            }
        }
    }
}
