// SPDX-License-Identifier: MPL-2.0
//! The VFX editor panel: emitter list, module stack with the palette and parameter rows, the curve
//! and gradient editors, the live preview and the per-emitter cost table.
//!
//! Every widget turns into one [`VfxChange`] applied after drawing (the same deferred pattern as the
//! other data panels), so the panel and `editor.vfx_edit` share one code path.

use regia_core::math::{Aabb, Vec3};
use regia_ecs::prelude::*;
use regia_render::RenderDevice;
use regia_vfx::curves::{sample_color, sample_scalar};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use super::curves::{CURVES, CurveRef, GRADIENTS, GradientRef, KeyEdit};
use super::modules::{self, ModuleKind, ParamKind, ParamValue};
use super::preview::{MAX_TIME, PreviewRequest, render_at};
use super::sim::{EmitterCost, emitter_costs, steps_for};
use super::{VfxChange, VfxEditorPanel, edit_effect, open_effect, redo, save_effect, undo};
use crate::data_panels::PanelError;

/// Preview width in the panel.
const PREVIEW_W: u32 = 320;
/// Preview height in the panel.
const PREVIEW_H: u32 = 180;

/// Computed emitter bounds and the `(revision, seed, bucket)` they were measured for.
pub type PreviewBounds = ((u64, u64, u32), Vec<Option<Aabb>>);

/// UI scratch and preview controls.
#[derive(Default)]
pub struct PanelUi {
    /// The path box.
    pub path_input: String,
    /// The selected emitter.
    pub selected: usize,
    /// The palette's pick.
    pub palette_pick: Option<ModuleKind>,
    /// The new-emitter name box.
    pub new_emitter: String,
    /// The selected gradient key `(gradient, index)`.
    pub color_key: Option<(GradientRef, usize)>,
    /// Preview: playing.
    pub playing: bool,
    /// Preview: the time shown, seconds.
    pub time: f32,
    /// Preview: the instance seed.
    pub seed: u64,
    /// The egui frame the play clock last advanced on (egui may run the UI twice per frame).
    pub clock_frame: u64,
    /// The preview texture and the `(revision, seed, steps)` it shows.
    pub texture: Option<((u64, u64, u32), egui::TextureHandle)>,
    /// Why the preview has no current image.
    pub preview_error: Option<String>,
    /// Cost rows and the `(revision, seed, bucket)` they were measured for.
    pub costs: Option<((u64, u64, u32), Vec<EmitterCost>)>,
    /// The key list of each curve (by curve name): the keyboard route of the curve canvases
    /// (FU-5).
    pub(crate) curve_keys: std::collections::HashMap<&'static str, crate::canvas_keys::ListKeys>,
    /// Emitter names disabled only in the preview.
    pub preview_disabled: BTreeSet<String>,
    /// Emitter names isolated only in the preview.
    pub preview_solo: BTreeSet<String>,
    /// Cached computed bounds at the preview's exact fixed step.
    pub bounds: Option<PreviewBounds>,
    /// Last egui frame that changed preview isolation or scrub controls.
    pub controls_frame: Option<u64>,
    /// Read the preview's GPU particle bounds (opt-in, P-225): a blocking reduction, refreshed at
    /// most once per half second of preview time.
    pub gpu_bounds_on: bool,
    /// The last GPU bounds read and the `(revision, seed, bucket)` they were read for.
    pub gpu_bounds: Option<PreviewBounds>,
    /// Last egui frame that changed the fixed bounds (the field, button or gizmo guard).
    pub bounds_frame: Option<u64>,
    /// The fixed-bounds gizmo handle being dragged: `(local axis, max face, box at drag start,
    /// accumulated local metres)`.
    pub gizmo_drag: Option<(usize, bool, regia_vfx::format::FixedBoundsDef, f32)>,
}

impl std::fmt::Debug for PanelUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelUi")
            .field("selected", &self.selected)
            .field("playing", &self.playing)
            .field("time", &self.time)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

/// Colour of a disabled module's header.
pub const DISABLED_COLOR: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
/// Colour of a curve's line.
pub const CURVE_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 180, 250);
/// Colour of a curve key.
pub const KEY_COLOR: egui::Color32 = egui::Color32::from_rgb(250, 200, 60);

/// Draws the panel. `device` drives the live preview (none: the preview says so).
pub fn vfx_editor_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
    let _span = tracing::info_span!("vfx_editor.panel").entered();
    let mut panel = world.remove_resource::<VfxEditorPanel>().unwrap_or_default();
    let mut actions = Actions::default();
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.ui.path_input).hint_text("vfx/explosion.vfx.ron"));
        if ui.button("Open").clicked() {
            actions.open = Some(panel.ui.path_input.clone());
        }
        let dirty = panel.doc.as_ref().is_some_and(super::VfxDoc::dirty);
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            actions.save = true;
        }
        if ui.button("Undo").clicked() {
            actions.undo = true;
        }
        if ui.button("Redo").clicked() {
            actions.redo = true;
        }
    });
    if panel.doc.is_some() {
        draw_doc(ui, &mut panel, &mut actions, device);
    } else {
        ui.weak("Open a .vfx.ron from the project to edit its emitters.");
    }
    if let Some(e) = &panel.last_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
    }
    world.insert_resource(panel);
    run_actions(world, actions);
}

/// The panel without a GPU device (the preview shows a note): for headless drawing.
pub fn vfx_editor_panel_no_gpu(ui: &mut egui::Ui, world: &mut World) {
    vfx_editor_panel(ui, world, None);
}

#[derive(Default)]
struct Actions {
    open: Option<String>,
    change: Option<VfxChange>,
    save: bool,
    undo: bool,
    redo: bool,
}

fn run_actions(world: &mut World, actions: Actions) {
    let mut result: Result<(), PanelError> = Ok(());
    if let Some(path) = actions.open {
        result = open_effect(world, &path);
    }
    if let Some(change) = actions.change {
        result = result.and_then(|()| edit_effect(world, &change).map(|_| ()));
    }
    if actions.save {
        result = result.and_then(|()| save_effect(world).map(|_| ()));
    }
    if actions.undo {
        undo(world);
    }
    if actions.redo {
        redo(world);
    }
    let mut panel = world.remove_resource::<VfxEditorPanel>().unwrap_or_default();
    panel.last_error = result.err().map(|e| e.message);
    world.insert_resource(panel);
}

fn draw_doc(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    actions: &mut Actions,
    device: Option<&RenderDevice>,
) {
    let Some(doc) = panel.doc.clone() else {
        return;
    };
    let def = &doc.state.def;
    if panel.ui.selected >= def.emitters.len() {
        panel.ui.selected = 0;
    }
    ui.horizontal(|ui| {
        ui.strong(&def.name);
        ui.weak(format!(
            "{}{}",
            doc.path,
            if doc.dirty() { " (edited)" } else { "" }
        ));
        ui.label("budget");
        let mut budget = def.system.budget;
        if ui
            .add(egui::DragValue::new(&mut budget).range(1..=10_000_000))
            .changed()
        {
            actions.change = Some(VfxChange::Budget(budget));
        }
    });
    ui.separator();
    let mut measured_doc = doc.clone();
    measured_doc.state.def = preview_definition(panel, def);
    measure_costs(panel, &measured_doc);
    overview_section(ui, panel, &doc);
    timeline_section(ui, panel, def);
    let mut preview_doc = doc.clone();
    preview_doc.state.def = preview_definition(panel, def);
    ui.columns(2, |cols| {
        let (left, right) = cols.split_at_mut(1);
        let left = &mut left[0];
        let right = &mut right[0];
        emitter_list(left, panel, def, actions);
        left.separator();
        module_stack(left, panel, &doc, actions);
        preview_ui(right, panel, &preview_doc, device, actions);
        right.separator();
        cost_table(right, panel, &preview_doc);
        right.separator();
        curve_editors(right, panel, &doc, actions);
    });
}

fn emitter_list(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    def: &regia_vfx::format::VfxSystemDef,
    actions: &mut Actions,
) {
    ui.label("Emitters");
    let n = def.emitters.len();
    for (i, e) in def.emitters.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui.selectable_label(panel.ui.selected == i, &e.name).clicked() {
                panel.ui.selected = i;
            }
            if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                actions.change = Some(VfxChange::MoveEmitter(i, i - 1));
                panel.ui.selected = i - 1;
            }
            if ui
                .add_enabled(i + 1 < n, egui::Button::new("Down").small())
                .clicked()
            {
                actions.change = Some(VfxChange::MoveEmitter(i, i + 1));
                panel.ui.selected = i + 1;
            }
            if ui
                .add_enabled(n > 1, egui::Button::new("Remove").small())
                .clicked()
            {
                actions.change = Some(VfxChange::RemoveEmitter(i));
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.ui.new_emitter).hint_text("sparks"));
        if ui.button("Add emitter").clicked() && !panel.ui.new_emitter.trim().is_empty() {
            actions.change = Some(VfxChange::AddEmitter(panel.ui.new_emitter.trim().to_owned()));
            panel.ui.new_emitter.clear();
        }
    });
}

fn module_stack(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc, actions: &mut Actions) {
    let i = panel.ui.selected;
    let Some(e) = doc.state.def.emitters.get(i) else {
        return;
    };
    ui.label(format!("Module stack: {}", e.name));
    let stack = doc.stack(i);
    for (kind, enabled) in &stack {
        let title = format!("{} / {}", kind.stage().name(), kind.name());
        // One collapsible row per module (closed by default, so the whole stack stays in view): the
        // A/B checkbox, the name and Remove in the header, the parameter rows in the body.
        let id = ui.make_persistent_id(("vfx-module", i, kind.name()));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                if !kind.required() {
                    let mut on = *enabled;
                    if ui.checkbox(&mut on, "").on_hover_text("enable (A/B)").changed() {
                        actions.change = Some(VfxChange::SetEnabled(i, *kind, on));
                    }
                }
                if *enabled {
                    ui.strong(&title);
                } else {
                    ui.colored_label(DISABLED_COLOR, format!("{title} (disabled)"));
                }
                if !kind.required() && ui.small_button("Remove").clicked() {
                    actions.change = Some(VfxChange::RemoveModule(i, *kind));
                }
            })
            .body(|ui| {
                if *enabled {
                    param_rows(ui, i, *kind, e, actions);
                } else {
                    ui.weak("Disabled: enable it to edit its parameters.");
                }
            });
    }
    // The palette: the addable kinds the emitter does not have.
    let missing: Vec<ModuleKind> = modules::palette()
        .into_iter()
        .filter(|k| !stack.iter().any(|(s, _)| s == k))
        .collect();
    if panel.ui.palette_pick.is_none_or(|p| !missing.contains(&p)) {
        panel.ui.palette_pick = missing.first().copied();
    }
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("vfx-palette")
            .selected_text(panel.ui.palette_pick.map_or("(none)", ModuleKind::name))
            .show_ui(ui, |ui| {
                for k in &missing {
                    ui.selectable_value(&mut panel.ui.palette_pick, Some(*k), k.name());
                }
            });
        if ui
            .add_enabled(panel.ui.palette_pick.is_some(), egui::Button::new("Add module"))
            .clicked()
            && let Some(k) = panel.ui.palette_pick
        {
            actions.change = Some(VfxChange::AddModule(i, k));
        }
    });
}

fn param_rows(
    ui: &mut egui::Ui,
    i: usize,
    kind: ModuleKind,
    e: &regia_vfx::format::EmitterDef,
    actions: &mut Actions,
) {
    egui::Grid::new(("vfx-grid", i, kind.name()))
        .num_columns(2)
        .show(ui, |ui| {
            for p in modules::params(e, kind) {
                ui.label(p.name);
                let read_only = p.name == "keys" || p.name == "links";
                let new = match (&p.kind, &p.value) {
                    (_, ParamValue::Num(v)) if read_only => {
                        ui.weak(format!("{v}"));
                        None
                    }
                    (ParamKind::Num { min, max }, ParamValue::Num(v)) => {
                        let mut x = *v;
                        let speed = ((max - min).abs() * 1.0e-3).clamp(0.001, 1.0);
                        ui.add(egui::DragValue::new(&mut x).speed(speed).range(*min..=*max))
                            .changed()
                            .then_some(ParamValue::Num(x))
                    }
                    (ParamKind::Int { min, max }, ParamValue::Num(v)) => {
                        let mut x = *v as u32;
                        ui.add(egui::DragValue::new(&mut x).range(*min..=*max))
                            .changed()
                            .then_some(ParamValue::Num(x as f32))
                    }
                    (ParamKind::Bool, ParamValue::Bool(b)) => {
                        let mut x = *b;
                        ui.checkbox(&mut x, "").changed().then_some(ParamValue::Bool(x))
                    }
                    (ParamKind::Choice(list), ParamValue::Text(s)) => {
                        let mut x = s.clone();
                        egui::ComboBox::from_id_salt(("vfx-choice", i, kind.name(), p.name))
                            .selected_text(x.clone())
                            .show_ui(ui, |ui| {
                                for c in *list {
                                    ui.selectable_value(&mut x, (*c).to_owned(), *c);
                                }
                            });
                        (x != *s).then_some(ParamValue::Text(x))
                    }
                    (ParamKind::Color, ParamValue::Text(s)) => {
                        let mut x = s.clone();
                        let r = ui.add(egui::TextEdit::singleline(&mut x).desired_width(90.0));
                        (r.lost_focus() && x != *s).then_some(ParamValue::Text(x))
                    }
                    _ => None,
                };
                if let Some(v) = new {
                    actions.change = Some(VfxChange::SetParam(i, kind, p.name.to_owned(), v));
                }
                ui.end_row();
            }
        });
}

fn curve_editors(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc, actions: &mut Actions) {
    let i = panel.ui.selected;
    let Some(e) = doc.state.def.emitters.get(i) else {
        return;
    };
    ui.label(
        "Curves (linear between keys; double-click adds, drag moves, right-click deletes; \
         keyboard: each curve's key list)",
    );
    for c in CURVES {
        let keys = c.keys(e);
        if keys.is_empty()
            && !(c == CurveRef::RibbonWidth && e.render.mode == regia_vfx::format::RenderMode::Ribbon)
        {
            continue;
        }
        ui.weak(c.name());
        let list = panel.ui.curve_keys.entry(c.name()).or_default();
        if let Some(edit) = curve_widget(ui, (i, c.name()), c, keys, list.focused()) {
            actions.change = Some(VfxChange::Key(i, edit));
        }
        if let Some(edit) = curve_keys_list(ui, (i, c.name()), c, keys, list) {
            actions.change = Some(VfxChange::Key(i, edit));
        }
    }
    ui.label("Gradients (click a marker to select it; double-click the bar adds)");
    for g in GRADIENTS {
        let keys = g.keys(e);
        if keys.is_empty()
            && !(g == GradientRef::RibbonColor && e.render.mode == regia_vfx::format::RenderMode::Ribbon)
        {
            continue;
        }
        ui.weak(g.name());
        if let Some(edit) = gradient_widget(ui, panel, (i, g.name()), g, keys) {
            actions.change = Some(VfxChange::Key(i, edit));
        }
    }
}

/// The value range a curve plot shows: the keys' span with a margin, at least `[0, 1]`.
fn value_range(keys: &[regia_vfx::format::FloatKey]) -> (f32, f32) {
    let lo = keys.iter().map(|k| k.v).fold(0.0f32, f32::min);
    let hi = keys.iter().map(|k| k.v).fold(1.0f32, f32::max);
    let pad = (hi - lo) * 0.1;
    (lo - pad, hi + pad)
}

/// How far Left / Right move a curve key in time (Shift: [`KEY_T_COARSE`]).
pub const KEY_T_STEP: f32 = 0.01;
/// Shift+Left / Shift+Right.
pub const KEY_T_COARSE: f32 = 0.1;
/// Alt+Up / Alt+Down move a key's value by this share of the plot's value range (Shift: four).
pub const KEY_V_SHARE: f32 = 0.05;

/// The key a keyboard nudge moves key `k` to: `dx` steps in time, `dy` (screen direction: up is
/// -1) steps of the value range. `None` for a missing key.
#[must_use]
pub fn nudged_key(
    keys: &[regia_vfx::format::FloatKey],
    k: usize,
    dx: i32,
    dy: i32,
    coarse: bool,
) -> Option<(f32, f32)> {
    let key = keys.get(k)?;
    let (lo, hi) = value_range(keys);
    let t_step = if coarse { KEY_T_COARSE } else { KEY_T_STEP };
    let v_step = (hi - lo) * KEY_V_SHARE * if coarse { 4.0 } else { 1.0 };
    Some((
        (key.t + dx as f32 * t_step).clamp(0.0, 1.0),
        key.v - dy as f32 * v_step,
    ))
}

/// The key list under a curve: the keyboard route of the curve canvas (FU-5, the convention of
/// `canvas_keys`). Up / Down move the focus; Left / Right move the key in time (Shift coarser),
/// Alt+Up / Alt+Down change its value; Delete removes it (the right-click); Ctrl+Shift+A adds a
/// key halfway to the next one on the curve (the double-click). The same [`KeyEdit`]s as the
/// mouse.
fn curve_keys_list(
    ui: &mut egui::Ui,
    id: (usize, &str),
    curve: CurveRef,
    keys: &[regia_vfx::format::FloatKey],
    list: &mut crate::canvas_keys::ListKeys,
) -> Option<KeyEdit> {
    use crate::canvas_keys::{RowKey, list_rows};
    let n = keys.len();
    let name = |k: usize| {
        keys.get(k).map_or_else(String::new, |key| {
            format!(
                "{} key {} of {n}: time {:.2}, value {:.2}",
                curve.name(),
                k + 1,
                key.t,
                key.v
            )
        })
    };
    let height = (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * n.min(4) as f32;
    let mut edit = None;
    for event in list_rows(ui, ("vfx-curve-keys", id), list, n, height, None, &name) {
        match event {
            RowKey::Delete(k) => edit = Some(KeyEdit::DeleteKey(curve, k)),
            RowKey::Palette(k) => {
                let Some(key) = keys.get(k) else { continue };
                let next = keys.get(k + 1).map_or(1.0, |n| n.t);
                let t = if next > key.t {
                    (key.t + next) * 0.5
                } else {
                    (key.t + 0.1).min(1.0)
                };
                edit = Some(KeyEdit::AddKey(
                    curve,
                    t,
                    sample_scalar(keys, t, curve.empty_value()),
                ));
            }
            RowKey::Arrow {
                row, dx, dy, coarse, ..
            } => {
                if let Some((t, v)) = nudged_key(keys, row, dx, dy, coarse) {
                    edit = Some(KeyEdit::MoveKey(curve, row, t, v));
                }
            }
            RowKey::Activate(_) | RowKey::Connect(_) => {}
        }
    }
    edit
}

fn curve_widget(
    ui: &mut egui::Ui,
    id: (usize, &str),
    curve: CurveRef,
    keys: &[regia_vfx::format::FloatKey],
    focused: Option<usize>,
) -> Option<KeyEdit> {
    let size = egui::vec2(ui.available_width().clamp(120.0, 360.0), 80.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(28));
    let (lo, hi) = value_range(keys);
    let to_screen = |t: f32, v: f32| {
        egui::pos2(
            rect.left() + t * rect.width(),
            rect.bottom() - (v - lo) / (hi - lo).max(1.0e-6) * rect.height(),
        )
    };
    let from_screen = |p: egui::Pos2| {
        let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let v = lo + (rect.bottom() - p.y) / rect.height() * (hi - lo);
        (t, v)
    };
    // The curve as the simulation samples it.
    let line: Vec<egui::Pos2> = (0..=48)
        .map(|s| {
            let t = s as f32 / 48.0;
            to_screen(t, sample_scalar(keys, t, curve.empty_value()))
        })
        .collect();
    painter.add(egui::Shape::line(line, egui::Stroke::new(1.5, CURVE_COLOR)));
    let mut edit = None;
    let drag_id = egui::Id::new(("vfx-curve-drag", id));
    let mut dragging: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)).flatten();
    for (k, key) in keys.iter().enumerate() {
        let p = to_screen(key.t, key.v);
        painter.circle_filled(p, 4.0, KEY_COLOR);
        if focused == Some(k) {
            crate::canvas_keys::focus_ring(&painter, egui::Rect::from_center_size(p, egui::vec2(8.0, 8.0)));
        }
        if let Some(pointer) = response.interact_pointer_pos() {
            let near = pointer.distance(p) < 8.0;
            if response.secondary_clicked() && near {
                edit = Some(KeyEdit::DeleteKey(curve, k));
            }
            if response.drag_started() && near {
                dragging = Some(k);
            }
        }
    }
    if response.dragged()
        && let (Some(k), Some(pointer)) = (dragging, response.interact_pointer_pos())
        && k < keys.len()
    {
        let (t, v) = from_screen(pointer);
        edit = Some(KeyEdit::MoveKey(curve, k, t, v));
    }
    if response.drag_stopped() {
        dragging = None;
    }
    if response.double_clicked()
        && let Some(pointer) = response.interact_pointer_pos()
        && !keys
            .iter()
            .any(|key| pointer.distance(to_screen(key.t, key.v)) < 8.0)
    {
        let (t, v) = from_screen(pointer);
        edit = Some(KeyEdit::AddKey(curve, t, v));
    }
    ui.data_mut(|d| d.insert_temp(drag_id, dragging));
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("{} curve canvas, {} keys", curve.name(), keys.len()),
    );
    edit
}

fn color32(c: [f32; 4]) -> egui::Color32 {
    // `sample_color` returns linear RGBA; egui takes sRGB bytes.
    let enc = |x: f32| {
        let x = x.clamp(0.0, 1.0);
        let s = if x <= 0.003_130_8 {
            x * 12.92
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round() as u8
    };
    egui::Color32::from_rgb(enc(c[0]), enc(c[1]), enc(c[2]))
}

fn gradient_widget(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    id: (usize, &str),
    gradient: GradientRef,
    keys: &[regia_vfx::format::ColorKey],
) -> Option<KeyEdit> {
    let size = egui::vec2(ui.available_width().clamp(120.0, 360.0), 24.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    let steps = 32;
    for s in 0..steps {
        let t0 = s as f32 / steps as f32;
        let t1 = (s + 1) as f32 / steps as f32;
        let c = color32(sample_color(keys, (t0 + t1) * 0.5));
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + t0 * rect.width(), rect.top()),
                egui::pos2(rect.left() + t1 * rect.width(), rect.bottom()),
            ),
            0.0,
            c,
        );
    }
    let x_of = |t: f32| rect.left() + t * rect.width();
    let mut edit = None;
    for (k, key) in keys.iter().enumerate() {
        let p = egui::pos2(x_of(key.t), rect.bottom());
        let selected = panel.ui.color_key == Some((gradient, k));
        painter.circle_filled(p, if selected { 5.0 } else { 3.5 }, KEY_COLOR);
        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && (pointer.x - p.x).abs() < 6.0
        {
            panel.ui.color_key = Some((gradient, k));
        }
    }
    if response.double_clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let t = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let c = sample_color(keys, t);
        let hex = format!(
            "#{:02x}{:02x}{:02x}",
            color32(c).r(),
            color32(c).g(),
            color32(c).b()
        );
        edit = Some(KeyEdit::AddColor(gradient, t, hex));
    }
    if let Some((g, k)) = panel.ui.color_key
        && g == gradient
        && let Some(key) = keys.get(k)
    {
        ui.horizontal(|ui| {
            ui.label(format!("key {k}"));
            let mut t = key.t;
            let t_changed = ui
                .add(egui::DragValue::new(&mut t).speed(0.005).range(0.0..=1.0))
                .changed();
            let mut hex = key.color.clone();
            let color_changed = ui
                .add(
                    egui::TextEdit::singleline(&mut hex)
                        .desired_width(90.0)
                        .id_salt(("vfx-hex", id)),
                )
                .lost_focus()
                && hex != key.color;
            if t_changed || color_changed {
                edit = Some(KeyEdit::MoveColor(gradient, k, t, hex));
            }
            if ui.small_button("Delete").clicked() {
                edit = Some(KeyEdit::DeleteColor(gradient, k));
                panel.ui.color_key = None;
            }
        });
    }
    edit
}

fn preview_ui(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    doc: &super::VfxDoc,
    device: Option<&RenderDevice>,
    actions: &mut Actions,
) {
    ui.label("Preview");
    ui.horizontal(|ui| {
        if ui
            .button(if panel.ui.playing { "Pause" } else { "Play" })
            .clicked()
        {
            panel.ui.playing = !panel.ui.playing;
        }
        if ui.button("Restart").clicked() {
            panel.ui.time = 0.0;
            panel.preview = None;
        }
        ui.label("seed");
        ui.add(egui::DragValue::new(&mut panel.ui.seed));
    });
    ui.add(egui::Slider::new(&mut panel.ui.time, 0.0..=MAX_TIME).text("time s"));
    // The play clock advances once per egui frame (the UI may run twice in one frame).
    let frame = ui.ctx().cumulative_frame_nr();
    if panel.ui.playing && panel.ui.clock_frame != frame {
        panel.ui.clock_frame = frame;
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        panel.ui.time += dt;
        if panel.ui.time > MAX_TIME {
            panel.ui.time = 0.0;
        }
        ui.ctx().request_repaint();
    }
    if let Some(change) = bounds_readout(ui, panel, doc) {
        actions.change = Some(change);
    }
    let Some(device) = device else {
        ui.weak("No GPU device: the preview needs one.");
        return;
    };
    let want = (doc.revision, panel.ui.seed, steps_for(panel.ui.time));
    if panel.ui.texture.as_ref().is_none_or(|(have, _)| *have != want) {
        let request = PreviewRequest {
            revision: doc.revision,
            seed: panel.ui.seed,
            time: panel.ui.time,
            width: PREVIEW_W,
            height: PREVIEW_H,
        };
        match render_at(&mut panel.preview, device, &doc.state.def, &request) {
            Ok(frame) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.pixels,
                );
                let texture = ui
                    .ctx()
                    .load_texture("regia-vfx-preview", image, egui::TextureOptions::LINEAR);
                panel.ui.texture = Some((want, texture));
                panel.ui.preview_error = None;
            }
            Err(e) => panel.ui.preview_error = Some(format!("{e:?}")),
        }
    }
    if let Some((_, texture)) = &panel.ui.texture {
        let response = ui.image((texture.id(), egui::vec2(PREVIEW_W as f32, PREVIEW_H as f32)));
        draw_preview_bounds(ui, panel, doc, response.rect);
        if let Some(change) = fixed_bounds_gizmo(ui, panel, doc, response.rect) {
            actions.change = Some(change);
        }
    }
    read_gpu_bounds(panel, doc);
    if let Some(frame) = panel.preview.as_ref().and_then(|p| p.last()) {
        ui.weak(format!(
            "t {:.2} s, {} GPU particles alive, coverage {:.1} %{}",
            frame.time,
            frame.counters[3],
            frame.coverage * 100.0,
            frame
                .gpu_ms
                .map_or(String::new(), |ms| format!(", VFX GPU {ms:.3} ms"))
        ));
    }
    if let Some(e) = &panel.ui.preview_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
    }
}

fn measure_costs(panel: &mut VfxEditorPanel, doc: &super::VfxDoc) {
    // Re-measured per half second of preview time, not every played frame (the CPU reference
    // simulation runs from zero each time).
    let bucket = steps_for(panel.ui.time) / 30;
    let key = (doc.revision, panel.ui.seed, bucket);
    if panel.ui.costs.as_ref().is_none_or(|(have, _)| *have != key) {
        let gpu_ms = panel
            .preview
            .as_ref()
            .and_then(|p| p.last())
            .and_then(|f| f.gpu_ms);
        let rows = emitter_costs(&doc.state.def, panel.ui.seed, bucket as f32 * 0.5, gpu_ms);
        panel.ui.costs = Some((key, rows));
    }
}

fn cost_table(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc) {
    ui.label("Per-emitter cost");
    measure_costs(panel, doc);
    let Some((_, rows)) = &panel.ui.costs else {
        return;
    };
    egui::Grid::new("vfx-costs")
        .num_columns(6)
        .striped(true)
        .show(ui, |ui| {
            for h in ["emitter", "alive", "spawn/s", "list", "draws", "GPU ms"] {
                ui.strong(h);
            }
            ui.end_row();
            for r in rows {
                ui.label(&r.name);
                ui.label(r.alive.to_string());
                ui.label(
                    r.spawned_per_sec
                        .map_or("event".to_owned(), |s| format!("{s:.0}")),
                );
                ui.label(r.draw_list);
                ui.label(r.draw_calls.to_string());
                ui.label(
                    r.gpu_ms
                        .map_or("n/a (CPU counts)".to_owned(), |ms| format!("{ms:.3}")),
                );
                ui.end_row();
            }
        });
}

/// The definition simulated by the preview. Isolation never changes the document.
pub(crate) fn preview_definition(
    panel: &VfxEditorPanel,
    def: &regia_vfx::format::VfxSystemDef,
) -> regia_vfx::format::VfxSystemDef {
    let mut out = def.clone();
    let any_solo = def
        .emitters
        .iter()
        .any(|e| panel.ui.preview_solo.contains(&e.name));
    for e in &mut out.emitters {
        if panel.ui.preview_disabled.contains(&e.name)
            || (any_solo && !panel.ui.preview_solo.contains(&e.name))
        {
            e.spawn.count_scale = 0.0;
        }
    }
    out
}

/// The common GUI/agent operation for preview-only emitter isolation.
pub(crate) fn set_preview_emitter(panel: &mut VfxEditorPanel, name: &str, enabled: bool, solo: bool) {
    let changed = if enabled {
        panel.ui.preview_disabled.remove(name)
    } else {
        panel.ui.preview_disabled.insert(name.to_owned())
    };
    let changed_solo = if solo {
        panel.ui.preview_solo.insert(name.to_owned())
    } else {
        panel.ui.preview_solo.remove(name)
    };
    if changed || changed_solo {
        panel.preview = None;
        panel.ui.texture = None;
        panel.ui.costs = None;
        panel.ui.bounds = None;
    }
}

/// Scrubbing pauses playback, with a validated finite time shared by UI and agent.
pub(crate) fn scrub(panel: &mut VfxEditorPanel, time: f32) -> Result<(), PanelError> {
    if !time.is_finite() || !(0.0..=MAX_TIME).contains(&time) {
        return Err(super::err(
            "bad_value",
            format!("time must be in 0..={MAX_TIME} seconds"),
        ));
    }
    panel.ui.time = time;
    panel.ui.playing = false;
    Ok(())
}

/// Preview isolation flags and authored spawn summaries for each emitter.
pub(crate) fn overview_json(panel: &VfxEditorPanel, def: &regia_vfx::format::VfxSystemDef) -> Value {
    json!(
        def.emitters
            .iter()
            .map(|e| json!({
                "name": e.name,
                "enabled": !panel.ui.preview_disabled.contains(&e.name),
                "solo": panel.ui.preview_solo.contains(&e.name),
                "spawn": spawn_summary(e),
            }))
            .collect::<Vec<_>>()
    )
}

fn spawn_summary(e: &regia_vfx::format::EmitterDef) -> String {
    use regia_vfx::format::SpawnMode;
    match e.spawn.mode {
        SpawnMode::Rate(rate) => format!("{:.1} particles/s", rate * e.spawn.count_scale),
        SpawnMode::Burst(n) => format!("Burst {n} × {:.2}", e.spawn.count_scale),
        SpawnMode::PerDistance { every } => format!("Every {every:.2} m × {:.2}", e.spawn.count_scale),
        SpawnMode::Event => "Event-driven".to_owned(),
    }
}

/// Scheduling and lifetime intervals; event-driven and indefinite endpoints remain unknown.
pub(crate) fn timeline_json(def: &regia_vfx::format::VfxSystemDef) -> Value {
    use regia_vfx::format::SpawnMode;
    json!(
        def.emitters
            .iter()
            .map(|e| {
                let event = e.spawn.mode == SpawnMode::Event;
                let burst = match e.spawn.mode {
                    SpawnMode::Burst(n) => Some(n),
                    _ => None,
                };
                let end = if event {
                    None
                } else if burst.is_some() {
                    Some(e.delay)
                } else {
                    e.duration.map(|d| e.delay + d)
                };
                json!({ "name": e.name, "start": if event { None } else { Some(e.delay) },
            "spawn_end": end, "alive_until": end.map(|t| t + e.spawn.lifetime.max),
            "lifetime": [e.spawn.lifetime.min, e.spawn.lifetime.max],
            "burst_count": burst, "count_scale": e.spawn.count_scale, "event_driven": event })
            })
            .collect::<Vec<_>>()
    )
}

/// Measured bounds at an exact preview step: the CPU reference with every motion module, each
/// particle grown by its drawn extent ([`regia_vfx::bounds::measured_bounds`]).
fn computed_bounds(def: &regia_vfx::format::VfxSystemDef, seed: u64, time: f32) -> Vec<Option<Aabb>> {
    let _span = tracing::info_span!("vfx_editor.bounds").entered();
    regia_vfx::bounds::measured_bounds(
        def,
        super::sim::ORIGIN,
        super::sim::FORWARD,
        regia_vfx::probe::ProbeSettings {
            dt: super::sim::STEP,
            seed,
            ..Default::default()
        },
        steps_for(time),
    )
    .emitters
}

/// The axes of the preview instance (fired at `sim::ORIGIN` along `sim::FORWARD`).
fn preview_axes() -> (Vec3, Vec3, Vec3) {
    regia_vfx::sim::VfxInstance::new(0, 0, super::sim::ORIGIN, super::sim::FORWARD).axes()
}

/// The fixed bounds in preview world space (the instance at the preview origin, scale 1).
pub(crate) fn fixed_preview_box(def: &regia_vfx::format::VfxSystemDef) -> Option<Aabb> {
    let b = def.system.bounds.as_ref().filter(|b| b.is_valid())?;
    Some(regia_vfx::bounds::fixed_world_aabb(
        b,
        super::sim::ORIGIN,
        preview_axes(),
        1.0,
    ))
}

/// The fixed box that holds the measured system bounds at `time` (the "Set fixed bounds from
/// measured" action), in the system's local space. `None` when nothing is alive then.
pub(crate) fn fixed_from_measured(
    def: &regia_vfx::format::VfxSystemDef,
    seed: u64,
    time: f32,
) -> Option<regia_vfx::format::FixedBoundsDef> {
    let measured = union_bounds(&computed_bounds(def, seed, time))?;
    regia_vfx::bounds::fixed_from_measured(measured, super::sim::ORIGIN, preview_axes(), 1.0)
}

fn union_bounds(bounds: &[Option<Aabb>]) -> Option<Aabb> {
    bounds
        .iter()
        .flatten()
        .copied()
        .reduce(|a, b| Aabb::new(a.min.min(b.min), a.max.max(b.max)))
}

fn aabb_json(bounds: Option<Aabb>) -> Value {
    bounds.map_or(
        Value::Null,
        |b| json!({"min": b.min.to_array(), "max": b.max.to_array()}),
    )
}

/// Measured bounds (particle extents, every motion module) at an exact preview step, and the
/// authored fixed bounds when the effect has them (`mode: "fixed"`: they cull the effect).
pub(crate) fn bounds_json(def: &regia_vfx::format::VfxSystemDef, seed: u64, time: f32) -> Value {
    let bounds = computed_bounds(def, seed, time);
    let fixed = def.system.bounds.as_ref().filter(|b| b.is_valid());
    json!({ "mode": if fixed.is_some() { "fixed" } else { "computed" },
        "fixed": fixed.map(|b| json!({"min": b.min, "max": b.max})),
        "fixed_world": aabb_json(fixed_preview_box(def)),
        "kind": "particle_extents", "source": "cpu_reference", "space": "preview_world",
        "time": steps_for(time) as f32 * super::sim::STEP, "seed": seed,
        "system": aabb_json(union_bounds(&bounds)),
        "emitters": def.emitters.iter().zip(&bounds).map(|(e, b)| json!({"name": e.name, "aabb": aabb_json(*b)})).collect::<Vec<_>>() })
}

/// GPU bounds JSON (`editor.vfx_preview { bounds: true }`): per emitter and the union.
pub(crate) fn gpu_bounds_json(def: &regia_vfx::format::VfxSystemDef, bounds: &[Option<Aabb>]) -> Value {
    json!({ "source": "gpu_reduction", "kind": "particle_extents", "space": "preview_world",
        "system": aabb_json(union_bounds(bounds)),
        "emitters": def.emitters.iter().enumerate().map(|(i, e)| json!({"name": e.name, "aabb": aabb_json(bounds.get(i).copied().flatten())})).collect::<Vec<_>>() })
}

fn cached_bounds<'a>(panel: &'a mut VfxEditorPanel, doc: &super::VfxDoc) -> &'a [Option<Aabb>] {
    let key = (doc.revision, panel.ui.seed, steps_for(panel.ui.time));
    if panel.ui.bounds.as_ref().is_none_or(|(k, _)| *k != key) {
        panel.ui.bounds = Some((key, computed_bounds(&doc.state.def, panel.ui.seed, panel.ui.time)));
    }
    panel.ui.bounds.as_ref().map_or(&[], |(_, b)| b.as_slice())
}

fn overview_section(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc) {
    egui::CollapsingHeader::new("System overview").show(ui, |ui| overview_ui(ui, panel, doc));
}

fn timeline_section(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, def: &regia_vfx::format::VfxSystemDef) {
    egui::CollapsingHeader::new("Emitter timeline").show(ui, |ui| timeline_ui(ui, panel, def));
}

fn overview_ui(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc) {
    ui.weak("Enable and Solo isolate the preview; the saved effect is unchanged.");
    egui::ScrollArea::horizontal()
        .id_salt("vfx-overview")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, e) in doc.state.def.emitters.iter().enumerate() {
                    ui.push_id(("vfx-overview-emitter", &e.name), |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.vertical(|ui| {
                                if ui.selectable_label(panel.ui.selected == i, &e.name).clicked() {
                                    panel.ui.selected = i;
                                }
                                let mut enabled = !panel.ui.preview_disabled.contains(&e.name);
                                let mut solo = panel.ui.preview_solo.contains(&e.name);
                                let changed = ui.checkbox(&mut enabled, "Enable preview").changed();
                                let changed = ui.checkbox(&mut solo, "Solo preview").changed() || changed;
                                if changed && panel.ui.controls_frame != Some(ui.ctx().cumulative_frame_nr())
                                {
                                    panel.ui.controls_frame = Some(ui.ctx().cumulative_frame_nr());
                                    set_preview_emitter(panel, &e.name, enabled, solo);
                                }
                                ui.label(spawn_summary(e));
                                if let Some(row) = panel.ui.costs.as_ref().and_then(|(_, rows)| rows.get(i)) {
                                    ui.weak(format!("{} alive · {} draws", row.alive, row.draw_calls));
                                } else {
                                    ui.weak("Cost: awaiting preview measurement");
                                }
                            });
                        });
                    });
                }
            });
        });
}

fn timeline_ui(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, def: &regia_vfx::format::VfxSystemDef) {
    let mut time = panel.ui.time;
    let response = ui.add(egui::Slider::new(&mut time, 0.0..=MAX_TIME).text("Scrub preview (seconds)"));
    if response.changed() && panel.ui.controls_frame != Some(ui.ctx().cumulative_frame_nr()) {
        panel.ui.controls_frame = Some(ui.ctx().cumulative_frame_nr());
        let _ = scrub(panel, time);
    }
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let rows = timeline_json(def);
    // Standard named controls handle keyboard input; the timeline itself is a read-only diagram.
    let height = ui.spacing().interact_size.y;
    let label_width = def
        .emitters
        .iter()
        .map(|e| {
            ui.painter()
                .layout_no_wrap(
                    e.name.clone(),
                    egui::TextStyle::Body.resolve(ui.style()),
                    ui.visuals().text_color(),
                )
                .size()
                .x
        })
        .fold(80.0_f32, f32::max)
        .min(ui.available_width() * 0.35);
    ui.horizontal(|ui| {
        ui.allocate_exact_size(egui::vec2(label_width, height), egui::Sense::hover());
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().max(80.0), height),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        for tick in 0..=6 {
            let fraction = tick as f32 / 6.0;
            let align = if tick == 0 {
                egui::Align2::LEFT_CENTER
            } else if tick == 6 {
                egui::Align2::RIGHT_CENTER
            } else {
                egui::Align2::CENTER_CENTER
            };
            painter.text(
                egui::pos2(rect.left() + fraction * rect.width(), rect.center().y),
                align,
                format!("{:.0}", fraction * MAX_TIME),
                egui::TextStyle::Small.resolve(ui.style()),
                ui.visuals().text_color(),
            );
        }
    });
    for row in rows.as_array().into_iter().flatten() {
        ui.horizontal(|ui| {
            let name = row["name"].as_str().unwrap_or_default();
            ui.add_sized(egui::vec2(label_width, height), egui::Label::new(name).truncate())
                .on_hover_text(name);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width().max(80.0), height),
                egui::Sense::hover(),
            );
            let p = ui.painter_at(rect);
            p.line_segment(
                [rect.left_center(), rect.right_center()],
                egui::Stroke::new(1.0, tokens.overlay.grid_minor),
            );
            let x = |t: f32| rect.left() + (t / MAX_TIME).clamp(0.0, 1.0) * rect.width();
            if let Some(start) = row["start"].as_f64() {
                let end = row["alive_until"].as_f64().unwrap_or(f64::from(MAX_TIME)) as f32;
                p.line_segment(
                    [
                        egui::pos2(x(start as f32), rect.center().y),
                        egui::pos2(x(end), rect.center().y),
                    ],
                    egui::Stroke::new(height * 0.2, tokens.overlay.bounds),
                );
                if row["burst_count"].is_number() {
                    p.line_segment(
                        [
                            egui::pos2(x(start as f32), rect.top()),
                            egui::pos2(x(start as f32), rect.bottom()),
                        ],
                        egui::Stroke::new(2.0, tokens.overlay.selection_outline),
                    );
                }
            } else {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Event-driven",
                    egui::TextStyle::Small.resolve(ui.style()),
                    ui.visuals().text_color(),
                );
            }
            p.line_segment(
                [
                    egui::pos2(x(panel.ui.time), rect.top()),
                    egui::pos2(x(panel.ui.time), rect.bottom()),
                ],
                egui::Stroke::new(1.0, tokens.overlay.gizmo_ring),
            );
        });
    }
    ui.weak(format!(
        "0–{MAX_TIME:.0} s · bars include maximum particle lifetime · ticks are bursts"
    ));
}

fn bounds_text(b: Option<Aabb>) -> String {
    b.map_or_else(
        || "empty".to_owned(),
        |b| format!("min {:.2?} · max {:.2?}", b.min.to_array(), b.max.to_array()),
    )
}

/// The bounds readout: measured bounds, the opt-in GPU bounds and the fixed-bounds editor.
/// Returns the fixed-bounds change the user made this frame (at most one per egui frame).
fn bounds_readout(ui: &mut egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc) -> Option<VfxChange> {
    let measured = union_bounds(cached_bounds(panel, doc));
    ui.strong("Measured bounds (CPU reference)").on_hover_text(
        "Every live particle grown by its drawn size, with every motion module (gravity, drag, wind, curl, \
         vortex, attractor). Excludes the screen-size floor.",
    );
    ui.label(format!("System: {}", bounds_text(measured)));
    let bounds = cached_bounds(panel, doc).to_vec();
    for (e, b) in doc.state.def.emitters.iter().zip(&bounds) {
        ui.label(format!("{}: {}", e.name, bounds_text(*b)));
    }
    ui.checkbox(&mut panel.ui.gpu_bounds_on, "Read GPU bounds")
        .on_hover_text("Reduces the preview's live particles on the GPU (blocking, at most twice a second of preview time).");
    if panel.ui.gpu_bounds_on {
        match &panel.ui.gpu_bounds {
            Some((_, gpu)) => ui.label(format!("GPU: {}", bounds_text(union_bounds(gpu)))),
            None => ui.weak("GPU: waiting for a preview frame"),
        };
    }
    fixed_bounds_editor(ui, panel, doc, measured)
}

/// The fixed-bounds controls: a toggle, six fields and "Set fixed bounds from measured".
fn fixed_bounds_editor(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    doc: &super::VfxDoc,
    measured: Option<Aabb>,
) -> Option<VfxChange> {
    use regia_vfx::format::FixedBoundsDef;
    let current = doc.state.def.system.bounds;
    let mut change = None;
    let mut on = current.is_some();
    if ui
        .checkbox(&mut on, "Fixed bounds")
        .on_hover_text("An authored box in the effect's local space. When set it culls the effect instead of the computed bounds; it never kills particles.")
        .changed()
    {
        change = Some(VfxChange::FixedBounds(if on {
            fixed_from_measured(&doc.state.def, panel.ui.seed, panel.ui.time).or(Some(FixedBoundsDef {
                min: [-1.0; 3],
                max: [1.0; 3],
            }))
        } else {
            None
        }));
    }
    if let Some(b) = current {
        let mut edited = b;
        for (label, corner) in [("Min", &mut edited.min), ("Max", &mut edited.max)] {
            ui.horizontal(|ui| {
                ui.label(label);
                for (axis, v) in ["x", "y", "z"].iter().zip(corner.iter_mut()) {
                    ui.add(
                        egui::DragValue::new(v)
                            .speed(0.05)
                            .max_decimals(3)
                            .prefix(format!("{} {axis} ", label.to_lowercase())),
                    );
                }
            });
        }
        if edited != b {
            // Keep min <= max while one face is dragged past the other.
            for i in 0..3 {
                if edited.min[i] > edited.max[i] {
                    if edited.min[i] != b.min[i] {
                        edited.min[i] = edited.max[i];
                    } else {
                        edited.max[i] = edited.min[i];
                    }
                }
            }
            change = Some(VfxChange::FixedBounds(Some(edited)));
        }
    } else {
        ui.weak("Fixed bounds: off (the computed bounds cull the effect).");
    }
    if ui
        .add_enabled(
            measured.is_some(),
            egui::Button::new("Set fixed bounds from measured"),
        )
        .on_hover_text("Replace the fixed bounds with a box holding the measured bounds at the preview time.")
        .clicked()
    {
        change = fixed_from_measured(&doc.state.def, panel.ui.seed, panel.ui.time)
            .map(|b| VfxChange::FixedBounds(Some(b)));
    }
    // egui may run the UI twice in one frame: one change per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    if change.is_some() {
        if panel.ui.bounds_frame == Some(frame) {
            return None;
        }
        panel.ui.bounds_frame = Some(frame);
    }
    change
}

/// Refreshes the opt-in GPU bounds from the preview session, at most once per half second of
/// preview time (30 steps) and only when the session moved.
fn read_gpu_bounds(panel: &mut VfxEditorPanel, doc: &super::VfxDoc) {
    if !panel.ui.gpu_bounds_on {
        panel.ui.gpu_bounds = None;
        return;
    }
    let Some(session) = panel.preview.as_mut() else {
        return;
    };
    let key = (doc.revision, panel.ui.seed, session.steps() / 30);
    if panel.ui.gpu_bounds.as_ref().is_some_and(|(k, _)| *k == key) {
        return;
    }
    if let Some(bounds) = session.gpu_bounds() {
        panel.ui.gpu_bounds = Some((key, bounds));
    }
}

/// The fixed box's viewport gizmo: six face handles on the preview image (drag a handle to move
/// that face along its local axis; the fields above are the keyboard route). Returns the change.
fn fixed_bounds_gizmo(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    doc: &super::VfxDoc,
    rect: egui::Rect,
) -> Option<VfxChange> {
    let b = doc
        .state
        .def
        .system
        .bounds
        .filter(regia_vfx::format::FixedBoundsDef::is_valid)?;
    let camera = panel
        .preview
        .as_ref()
        .map(super::preview::PreviewSession::camera)?;
    gizmo_for(ui, panel, b, camera, rect)
}

/// [`fixed_bounds_gizmo`] for box `b` seen by `camera` on the image `rect`.
fn gizmo_for(
    ui: &mut egui::Ui,
    panel: &mut VfxEditorPanel,
    b: regia_vfx::format::FixedBoundsDef,
    camera: regia_vfx::VfxCamera,
    rect: egui::Rect,
) -> Option<VfxChange> {
    let (_, _, vp) = camera.matrices(rect.aspect_ratio());
    let project = |p: Vec3| project_point(vp, rect, p);
    let axes = preview_axes();
    let axis_dirs = [axes.0, axes.1, axes.2];
    let to_world = |l: Vec3| super::sim::ORIGIN + axes.0 * l.x + axes.1 * l.y + axes.2 * l.z;
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let colors = [
        tokens.overlay.gizmo_x,
        tokens.overlay.gizmo_y,
        tokens.overlay.gizmo_z,
    ];
    let world = regia_vfx::bounds::fixed_world_aabb(&b, super::sim::ORIGIN, axes, 1.0);
    paint_boxes(ui, &[(world, tokens.overlay.gizmo_ring)], camera, rect);
    // Points: the handles scale with `appearance.ui_scale` like every other widget.
    let radius = 5.0;
    let centre = b.center();
    let half = b.half_extents();
    let mut change = None;
    for (axis, dir) in axis_dirs.iter().enumerate() {
        for max in [false, true] {
            let mut local = centre;
            local[axis] += if max { half[axis] } else { -half[axis] };
            let p = to_world(local);
            let (Some(at), Some(tip)) = (project(p), project(p + *dir)) else {
                continue;
            };
            let id = ui.id().with(("vfx-fixed-bounds-handle", axis, max));
            let hit = egui::Rect::from_center_size(at, egui::vec2(radius * 2.5, radius * 2.5));
            let response = ui.interact(hit, id, egui::Sense::drag());
            let name = format!(
                "Fixed bounds {} {} face",
                if max { "max" } else { "min" },
                ["x", "y", "z"][axis]
            );
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &name));
            ui.painter_at(rect).circle_filled(at, radius, colors[axis]);
            if response.drag_started() {
                panel.ui.gizmo_drag = Some((axis, max, b, 0.0));
            }
            if response.dragged()
                && let Some((a, m, start, moved)) = panel.ui.gizmo_drag.as_mut()
                && *a == axis
                && *m == max
            {
                // Screen drag along the projected axis, in metres along the local axis.
                let screen = tip - at;
                let len2 = screen.length_sq();
                if len2 > 1e-6 {
                    *moved += response.drag_delta().dot(screen) / len2;
                }
                let mut next = *start;
                if max {
                    next.max[axis] = (start.max[axis] + *moved).max(next.min[axis]);
                } else {
                    next.min[axis] = (start.min[axis] + *moved).min(next.max[axis]);
                }
                if next != b {
                    change = Some(VfxChange::FixedBounds(Some(next)));
                }
            }
            if response.drag_stopped() {
                panel.ui.gizmo_drag = None;
            }
        }
    }
    let frame = ui.ctx().cumulative_frame_nr();
    if change.is_some() {
        if panel.ui.bounds_frame == Some(frame) {
            return None;
        }
        panel.ui.bounds_frame = Some(frame);
    }
    change
}

fn draw_preview_bounds(ui: &egui::Ui, panel: &mut VfxEditorPanel, doc: &super::VfxDoc, rect: egui::Rect) {
    let Some(camera) = panel.preview.as_ref().map(super::preview::PreviewSession::camera) else {
        return;
    };
    let bounds = cached_bounds(panel, doc);
    paint_bounds(ui, bounds, camera, rect);
}

/// A world point on the preview image, `None` behind the camera.
fn project_point(vp: regia_core::math::Mat4, rect: egui::Rect, p: Vec3) -> Option<egui::Pos2> {
    let c = vp * p.extend(1.0);
    (c.is_finite() && c.w > 0.0 && c.z >= 0.0).then(|| {
        egui::pos2(
            rect.left() + (c.x / c.w + 1.0) * 0.5 * rect.width(),
            rect.top() + (1.0 - c.y / c.w) * 0.5 * rect.height(),
        )
    })
}

fn paint_bounds(ui: &egui::Ui, bounds: &[Option<Aabb>], camera: regia_vfx::VfxCamera, rect: egui::Rect) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let boxes: Vec<(Aabb, egui::Color32)> = bounds
        .iter()
        .flatten()
        .map(|b| (*b, tokens.overlay.bounds))
        .chain(union_bounds(bounds).map(|b| (b, tokens.overlay.selection_outline)))
        .collect();
    paint_boxes(ui, &boxes, camera, rect);
}

/// Draws each box's twelve edges on the preview image in its color.
fn paint_boxes(
    ui: &egui::Ui,
    boxes: &[(Aabb, egui::Color32)],
    camera: regia_vfx::VfxCamera,
    rect: egui::Rect,
) {
    let (_, _, vp) = camera.matrices(rect.aspect_ratio());
    let project = |p: Vec3| project_point(vp, rect, p);
    let painter = ui.painter_at(rect);
    for &(b, color) in boxes {
        let corners: [Vec3; 8] = std::array::from_fn(|i| {
            Vec3::new(
                if i & 1 == 0 { b.min.x } else { b.max.x },
                if i & 2 == 0 { b.min.y } else { b.max.y },
                if i & 4 == 0 { b.min.z } else { b.max.z },
            )
        });
        for (i, a) in corners.iter().enumerate() {
            for bit in [1, 2, 4] {
                if i & bit == 0
                    && let (Some(a), Some(b)) = (project(*a), project(corners[i | bit]))
                {
                    painter.line_segment([a, b], egui::Stroke::new(1.0, color));
                }
            }
        }
    }
}

#[cfg(test)]
mod w12_keyboard_tests {
    use super::*;

    fn definition() -> regia_vfx::format::VfxSystemDef {
        regia_vfx::format::VfxSystemDef::from_ron(r#"(version:7,name:"test",system:(),emitters:[
            (name:"burst",delay:0.2,spawn:(mode:Burst(4),lifetime:(min:1.0,max:2.0)),
            init:(shape:Point,velocity:(mode:Directional(dir:(0.0,0.0,1.0),spread:0.0,speed:(min:1.0,max:1.0))),size:(min:0.1,max:0.1)),render:(mode:Billboard,blend:Additive,sprite:SoftDisc))])"#).expect("definition")
    }

    fn doc(def: regia_vfx::format::VfxSystemDef) -> super::super::VfxDoc {
        super::super::VfxDoc {
            path: "test.vfx.ron".into(),
            opened_text: String::new(),
            opened_def: def.clone(),
            saved_text: String::new(),
            saved_def: def.clone(),
            state: super::super::DocState {
                def,
                disabled: Vec::new(),
            },
            revision: 1,
        }
    }

    fn event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn w12_preview_boxes_use_theme_tokens_and_empty_bounds_draw_nothing() {
        let ctx = egui::Context::default();
        let camera = regia_vfx::VfxCamera::looking_at(Vec3::new(3.0, 3.0, 5.0), Vec3::ZERO);
        let render = |bounds: &[Option<Aabb>]| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                paint_bounds(
                    ui,
                    bounds,
                    camera,
                    egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(300.0, 200.0)),
                );
            });
            output.textures_delta.clear();
            ctx.tessellate(output.shapes, output.pixels_per_point)
        };
        let colors = crate::theme::ui_tokens(&ctx);
        let empty = render(&[None]);
        assert!(!crate::data_panels::test_ui::has_color(
            &empty,
            colors.overlay.bounds
        ));
        let drawn = render(&[
            Some(Aabb::new(Vec3::splat(-0.5), Vec3::splat(0.5))),
            Some(Aabb::new(Vec3::X, Vec3::ONE)),
        ]);
        assert!(
            crate::data_panels::test_ui::has_color(&drawn, colors.overlay.bounds),
            "per-emitter boxes"
        );
        assert!(
            crate::data_panels::test_ui::has_color(&drawn, colors.overlay.selection_outline),
            "system union box"
        );
    }

    #[test]
    fn w12_overview_and_timeline_operate_by_keyboard_at_two_scales() {
        for scale in [1.0, 1.75] {
            for timeline in [false, true] {
                let ctx = egui::Context::default();
                ctx.set_zoom_factor(scale);
                ctx.all_styles_mut(|style| style.animation_time = 0.0);
                let mut panel = VfxEditorPanel::default();
                let doc = doc(definition());
                let frame = |events, panel: &mut VfxEditorPanel| {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            events,
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1200.0, 600.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                if timeline {
                                    timeline_section(ui, panel, &doc.state.def);
                                } else {
                                    overview_section(ui, panel, &doc);
                                }
                            });
                        },
                    );
                    output.textures_delta.clear();
                };
                frame(vec![], &mut panel);
                // Open the disclosure from an unfocused, fresh context, then operate its controls.
                frame(vec![event(egui::Key::Tab)], &mut panel);
                frame(vec![event(egui::Key::Space)], &mut panel);
                for _ in 0..8 {
                    frame(vec![event(egui::Key::Tab)], &mut panel);
                    frame(
                        vec![event(if timeline {
                            egui::Key::ArrowRight
                        } else {
                            egui::Key::Space
                        })],
                        &mut panel,
                    );
                    if (timeline && panel.ui.time > 0.0)
                        || (!timeline && panel.ui.preview_disabled.contains("burst"))
                    {
                        break;
                    }
                }
                if timeline {
                    assert!(panel.ui.time > 0.0, "Tab and arrow scrub at {scale}");
                    assert!(!panel.ui.playing);
                } else {
                    assert!(
                        panel.ui.preview_disabled.contains("burst"),
                        "Tab and Space enable/disable at {scale}"
                    );
                }
            }
        }
    }

    #[test]
    fn w12_bounds_are_empty_before_delay_and_isolation_removes_particles() {
        let def = definition();
        assert_eq!(bounds_json(&def, 1, 0.0)["system"], Value::Null);
        let mut panel = VfxEditorPanel::default();
        let full = bounds_json(&def, 1, 0.5);
        assert!(full["system"]["min"].is_array());
        set_preview_emitter(&mut panel, "burst", false, false);
        let isolated = preview_definition(&panel, &def);
        assert_eq!(bounds_json(&isolated, 1, 0.5)["system"], Value::Null);
        assert_eq!(def.emitters[0].spawn.count_scale, 1.0);
        let timeline = timeline_json(&def);
        assert_eq!(timeline[0]["burst_count"], 4);
        assert!((timeline[0]["alive_until"].as_f64().expect("end") - 2.2).abs() < 1e-5);
    }
}

#[cfg(test)]
mod w13_bounds_tests {
    use super::*;
    use regia_vfx::format::FixedBoundsDef;

    fn definition() -> regia_vfx::format::VfxSystemDef {
        regia_vfx::format::VfxSystemDef::from_ron(r#"(version:8,name:"test",system:(),emitters:[
            (name:"burst",delay:0.2,spawn:(mode:Burst(4),lifetime:(min:1.0,max:2.0)),
            init:(shape:Point,velocity:(mode:Directional(dir:(0.0,0.0,1.0),spread:0.0,speed:(min:1.0,max:1.0))),size:(min:0.1,max:0.1)),render:(mode:Billboard,blend:Additive,sprite:SoftDisc))])"#).expect("definition")
    }

    fn doc(def: regia_vfx::format::VfxSystemDef) -> super::super::VfxDoc {
        super::super::VfxDoc {
            path: "test.vfx.ron".into(),
            opened_text: String::new(),
            opened_def: def.clone(),
            saved_text: String::new(),
            saved_def: def.clone(),
            state: super::super::DocState {
                def,
                disabled: Vec::new(),
            },
            revision: 1,
        }
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn raw(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        }
    }

    /// One frame of the bounds readout: the change it returned and the widgets that gained focus.
    fn readout(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        panel: &mut VfxEditorPanel,
        doc: &super::super::VfxDoc,
    ) -> (Option<VfxChange>, Vec<egui::WidgetInfo>) {
        let mut change = None;
        let mut output = ctx.run_ui(raw(events), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                if let Some(c) = bounds_readout(ui, panel, doc) {
                    change = Some(c);
                }
            });
        });
        // No painter here: the font atlas upload is dropped on purpose.
        output.textures_delta.clear();
        let focused = output
            .platform_output
            .events
            .iter()
            .filter_map(|e| match e {
                egui::output::OutputEvent::FocusGained(info) => Some(info.clone()),
                _ => None,
            })
            .collect();
        (change, focused)
    }

    /// Presses Tab until a widget matching `want` gains focus (at most 16 presses).
    fn tab_to(
        ctx: &egui::Context,
        panel: &mut VfxEditorPanel,
        doc: &super::super::VfxDoc,
        want: impl Fn(&egui::WidgetInfo) -> bool,
    ) {
        for _ in 0..16 {
            let (_, focused) = readout(ctx, vec![key(egui::Key::Tab)], panel, doc);
            if focused.iter().any(&want) {
                return;
            }
        }
        panic!("no focusable widget matched");
    }

    fn labeled(label: &'static str) -> impl Fn(&egui::WidgetInfo) -> bool {
        move |i| i.label.as_deref() == Some(label)
    }

    fn apply(doc: &mut super::super::VfxDoc, change: Option<VfxChange>) -> FixedBoundsDef {
        let Some(VfxChange::FixedBounds(Some(b))) = change else {
            panic!("expected a fixed-bounds change, got {change:?}");
        };
        doc.state.def.system.bounds = Some(b);
        doc.revision += 1;
        b
    }

    /// The fixed-bounds controls work from the keyboard alone at two UI scales: Tab to "Fixed
    /// bounds" and Space turns the box on (sized from the measured bounds), Tab to a field and an
    /// arrow key edits it, Tab to "Set fixed bounds from measured" and Space resets it.
    #[test]
    fn w13_fixed_bounds_operate_by_keyboard_at_two_scales() {
        let def = definition();
        let measured = fixed_from_measured(&def, 1, 0.5).expect("particles alive at 0.5 s");
        for scale in [1.0, 1.75] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            ctx.all_styles_mut(|style| style.animation_time = 0.0);
            let mut panel = VfxEditorPanel::default();
            panel.ui.time = 0.5;
            panel.ui.seed = 1;
            let mut doc = doc(def.clone());
            readout(&ctx, vec![], &mut panel, &doc);
            tab_to(&ctx, &mut panel, &doc, labeled("Fixed bounds"));
            let (change, _) = readout(&ctx, vec![key(egui::Key::Space)], &mut panel, &doc);
            assert_eq!(
                apply(&mut doc, change),
                measured,
                "Space on Fixed bounds at {scale}"
            );
            tab_to(&ctx, &mut panel, &doc, |i| i.typ == egui::WidgetType::DragValue);
            let (change, _) = readout(&ctx, vec![key(egui::Key::ArrowUp)], &mut panel, &doc);
            let edited = apply(&mut doc, change);
            assert_ne!(
                edited, measured,
                "an arrow key edits the focused field at {scale}"
            );
            tab_to(&ctx, &mut panel, &doc, labeled("Set fixed bounds from measured"));
            let (change, _) = readout(&ctx, vec![key(egui::Key::Space)], &mut panel, &doc);
            assert_eq!(
                apply(&mut doc, change),
                measured,
                "Space on the button at {scale}"
            );
        }
    }

    /// The gizmo draws the box and its handles with theme tokens and a handle drag moves only
    /// that face (the drag is mouse-only; the fields are its keyboard route).
    #[test]
    fn w13_fixed_box_gizmo_drags_one_face_with_theme_tokens() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let b = FixedBoundsDef {
            min: [-1.0, -1.0, -1.0],
            max: [1.0, 1.0, 1.0],
        };
        let camera = regia_vfx::VfxCamera::looking_at(
            super::super::sim::ORIGIN + Vec3::new(4.0, 3.0, 6.0),
            super::super::sim::ORIGIN,
        );
        let rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(480.0, 320.0));
        let mut panel = VfxEditorPanel::default();
        let mut changes = Vec::new();
        let mut frame = |events: Vec<egui::Event>, panel: &mut VfxEditorPanel| {
            let mut output = ctx.run_ui(raw(events), |ui| {
                if let Some(c) = gizmo_for(ui, panel, b, camera, rect) {
                    changes.push(c);
                }
            });
            output.textures_delta.clear();
            ctx.tessellate(output.shapes, output.pixels_per_point)
        };
        let drawn = frame(vec![], &mut panel);
        let colors = crate::theme::ui_tokens(&ctx);
        for c in [
            colors.overlay.gizmo_ring,
            colors.overlay.gizmo_x,
            colors.overlay.gizmo_y,
            colors.overlay.gizmo_z,
        ] {
            assert!(crate::data_panels::test_ui::has_color(&drawn, c), "{c:?} drawn");
        }
        // The max-x face handle and its axis on the image.
        let (_, _, vp) = camera.matrices(rect.aspect_ratio());
        let axes = preview_axes();
        let face = super::super::sim::ORIGIN + axes.0 * b.max[0];
        let at = project_point(vp, rect, face).expect("visible handle");
        let tip = project_point(vp, rect, face + axes.0).expect("visible axis");
        let dir = (tip - at).normalized();
        let pointer = |p: egui::Pos2| egui::Event::PointerMoved(p);
        let button = |p: egui::Pos2, pressed| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![pointer(at)], &mut panel);
        frame(vec![button(at, true)], &mut panel);
        for step in 1..=4 {
            frame(vec![pointer(at + dir * (10.0 * step as f32))], &mut panel);
        }
        let end = at + dir * 40.0;
        frame(vec![button(end, false)], &mut panel);
        let Some(VfxChange::FixedBounds(Some(moved))) = changes.last().cloned() else {
            panic!("the drag changed nothing: {changes:?}");
        };
        assert!(moved.max[0] > b.max[0] + 0.05, "{moved:?}");
        assert_eq!(moved.min, b.min);
        assert_eq!(&moved.max[1..], &b.max[1..]);
        assert!(panel.ui.gizmo_drag.is_none(), "the drag ended");
    }

    /// The readout JSON reports the fixed box (local and preview-world) and `mode: "fixed"`.
    #[test]
    fn w13_bounds_json_reports_fixed_bounds() {
        let mut def = definition();
        let plain = bounds_json(&def, 1, 0.5);
        assert_eq!(plain["mode"], "computed");
        assert_eq!(plain["fixed"], Value::Null);
        assert_eq!(plain["kind"], "particle_extents");
        def.system.bounds = fixed_from_measured(&def, 1, 0.5);
        let fixed = bounds_json(&def, 1, 0.5);
        assert_eq!(fixed["mode"], "fixed");
        assert!(fixed["fixed"]["min"].is_array(), "{fixed}");
        // Set from measured, the fixed box holds the measured system box.
        for i in 0..3 {
            let f = |k: &str, v: &Value| v[k][i].as_f64().expect("number");
            assert!(
                f("min", &fixed["fixed_world"]) <= f("min", &fixed["system"]) + 1e-4,
                "{fixed}"
            );
            assert!(
                f("max", &fixed["fixed_world"]) >= f("max", &fixed["system"]) - 1e-4,
                "{fixed}"
            );
        }
    }
}
