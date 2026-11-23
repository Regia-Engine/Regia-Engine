// SPDX-License-Identifier: MPL-2.0
//! The UI Animation panel (ADR-3092, `book/agents/ui-anim-designer.md`): the animations of a
//! `.uianim.ron` library, their tracks and keys on a curve view, loop / ping-pong / reverse / speed,
//! and a scrub that samples through `UiAnimDef::compile` and `TweenPlan::sample` and lays the target
//! node out on a copy of the designer's document. Every edit is an [`AnimOp`] on the shared
//! [`UiAnimEditor`], one undo step each; the panel never changes the `.ui.ron`.
//!
//! Above the curve view, the **dope sheet** (w13, P-223) shows every track of the selected
//! animation as a row of key diamonds on one time axis. Click a key to select it (Shift adds or
//! removes), drag on empty space to box-select across tracks, and drag a selected key to move every
//! selected key in time together; the move is one undo step ([`move_dope_keys`], `AnimOp`s through
//! `UiAnimDoc::apply_all`, the `editor.ui_anim_edit` op `move_keys`). From the keyboard, the key
//! list under the sheet names every key: Enter or Space selects or deselects the focused one, and
//! Left / Right (Shift: coarser) move the selection (or the focused key) in time.
//!
//! Under the sheet, the **event track** (`.uianim.ron` v2, ADR-8900) lists the animation's frame
//! events (also drawn as flags on the sheet's last row): "Add event" puts one at the playhead, a
//! row selects an event for its time, name and payload fields, and "Remove event" deletes it. Each
//! commit is one undo step ([`AnimOp::SetEvent`], [`AnimOp::RemoveEvent`]; the agent's
//! `editor.ui_anim_edit` ops `set_event` and `remove_event`).

use std::path::PathBuf;

use regia_asset_pipeline::ui_anim_validate::{TARGET_SEPARATOR, target_of, validate_ui_anim};
use regia_ecs::prelude::*;
use regia_ui::anim_timeline::{AnimEvent, AnimKey, AnimValue, LoopMode, UiAnimDef};
use regia_ui::tween::{Ease, TweenProperty, TweenValue};

use crate::anim::curve_view::KEY_SIZE;
use crate::anim::view2d::View2d;
use crate::ui_anim_doc::{
    AnimOp, UiAnimDoc, UiAnimEditor, UiAnimError, apply_to_file, node_facts, sample_def,
};
use crate::ui_designer::UiDesigner;
use crate::ui_designer_preview::preview;

/// The panel's own state (nothing here is part of the document).
#[derive(Resource, Default)]
pub struct UiAnimPanelState {
    open_path: String,
    new_name: String,
    rename: String,
    /// The target node a new animation is bound to (the `node::` prefix).
    target: String,
    /// The property of the track on show, and of the next track to add.
    property: usize,
    /// The key on show, by time.
    selected_key: Option<f32>,
    view: View2d,
    fitted_for: Option<(String, String)>,
    last_frame: u64,
    dragging: Option<f32>,
    /// The key list under the curve view: the keyboard route of the key canvas (FU-5).
    keys: crate::canvas_keys::ListKeys,
    /// The dope sheet's selected keys, across tracks (w13).
    pub dope: Vec<DopeKey>,
    /// The dope sheet's running pointer drag.
    dope_drag: Option<DopeDrag>,
    /// The dope sheet's key list: its keyboard route.
    dope_keys: crate::canvas_keys::ListKeys,
    /// Where the dope sheet was drawn last frame, screen points (pointer tests aim at it).
    dope_rect: Option<egui::Rect>,
    /// The event track's selected event and its unsaved name and payload text.
    pub event: Option<EventDraft>,
}

/// The event track's selected event: its index and the name and payload being typed (committed when
/// the field loses focus).
#[derive(Clone, Debug, PartialEq)]
pub struct EventDraft {
    /// The event's place in the animation's `events` list.
    pub index: usize,
    /// The name being typed.
    pub name: String,
    /// The payload being typed.
    pub payload: String,
}

/// One key of the dope sheet: its track's property and its time.
#[derive(Clone, Debug, PartialEq)]
pub struct DopeKey {
    /// The track's tween property name.
    pub property: String,
    /// The key's time, seconds.
    pub time: f32,
}

/// A dope-sheet pointer drag: moving the selection, or a selection box.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DopeDrag {
    /// Moving the selected keys; the press was at `from` (seconds).
    Move { from: f32 },
    /// A box from `from` (screen points).
    Box { from: egui::Pos2 },
}

/// Height of one dope-sheet row, points.
pub const DOPE_ROW: f32 = 18.0;
/// Width of the dope sheet's property column, points.
pub const DOPE_NAME_WIDTH: f32 = 96.0;

/// Every key of `def` as dope-sheet keys, in track order then time order (the key list's rows).
#[must_use]
pub fn dope_keys(def: &UiAnimDef) -> Vec<DopeKey> {
    def.tracks
        .iter()
        .flat_map(|t| {
            t.keys.iter().map(|k| DopeKey {
                property: t.property.clone(),
                time: k.time,
            })
        })
        .collect()
}

/// The ops that move `keys` of animation `name` by `by` seconds (each new time snapped to 10 ms and
/// at least 0): every removal first, then every set, so keys of one track moving past each other
/// never meet half way. Returns the ops and the keys where they land. A key `def` lacks still gets
/// its removal, which `UiAnimDoc::apply_all` refuses (`unknown_key`) with nothing changed.
#[must_use]
pub fn dope_move_ops(def: &UiAnimDef, name: &str, keys: &[DopeKey], by: f32) -> (Vec<AnimOp>, Vec<DopeKey>) {
    let mut ops = Vec::new();
    let mut sets = Vec::new();
    let mut moved = Vec::new();
    for k in keys {
        ops.push(AnimOp::RemoveKey {
            name: name.to_owned(),
            property: k.property.clone(),
            time: k.time,
        });
        let found = def
            .tracks
            .iter()
            .find(|t| t.property == k.property)
            .and_then(|t| t.keys.iter().find(|key| key.time == k.time));
        if let Some(key) = found {
            let time = snap_time(k.time + by);
            sets.push(AnimOp::SetKey {
                name: name.to_owned(),
                property: k.property.clone(),
                key: AnimKey { time, ..key.clone() },
            });
            moved.push(DopeKey {
                property: k.property.clone(),
                time,
            });
        }
    }
    ops.extend(sets);
    (ops, moved)
}

/// Moves `keys` (across tracks) of animation `name` by `by` seconds as one undo step and returns
/// where they land. Errors (nothing changed): `bad_value` (no keys, or a key listed twice),
/// `unknown_animation`, `unknown_key`, `invalid` (the moved animation fails validation).
pub fn move_dope_keys(
    doc: &mut UiAnimDoc,
    name: &str,
    keys: &[DopeKey],
    by: f32,
) -> Result<Vec<DopeKey>, UiAnimError> {
    if keys.is_empty() || !by.is_finite() {
        return Err(UiAnimError::new(
            "bad_value",
            "move_keys needs keys and a finite `by`",
        ));
    }
    for (i, k) in keys.iter().enumerate() {
        if keys[..i].contains(k) {
            return Err(UiAnimError::new(
                "bad_value",
                format!("the `{}` key at {} s is listed twice", k.property, k.time),
            ));
        }
    }
    let def = doc
        .library()
        .animations
        .get(name)
        .cloned()
        .ok_or_else(|| UiAnimError::new("unknown_animation", format!("no animation named `{name}`")))?;
    let (ops, moved) = dope_move_ops(&def, name, keys, by);
    doc.apply_all(ops)?;
    Ok(moved)
}

/// How far Left / Right move a key in time, seconds (Shift: [`KEY_TIME_COARSE`]).
pub const KEY_TIME_STEP: f32 = 0.01;
/// Shift+Left / Shift+Right, seconds.
pub const KEY_TIME_COARSE: f32 = 0.1;
/// How far Alt+Up / Alt+Down change a number key's value (Shift: ten times).
pub const KEY_VALUE_STEP: f32 = 0.01;

/// A key time snapped to 10 ms, as the canvas drag snaps it.
fn snap_time(t: f32) -> f32 {
    (t.max(0.0) * 100.0).round() / 100.0
}

/// The key list under the curve view (FU-5, the convention of `canvas_keys`): Up / Down move the
/// focus; Enter selects the key (its fields show below); Left / Right move it in time, Alt+Up /
/// Alt+Down change a number key's value (Shift: coarser), each the canvas drag's
/// `commit_key_drag`; Delete is the right-click's `RemoveKey`; Ctrl+Shift+A adds a copy of the
/// key 0.1 s later (the canvas click's `SetKey`).
fn key_list(
    ui: &mut egui::Ui,
    doc: &mut UiAnimDoc,
    state: &mut UiAnimPanelState,
    (name, property): (&str, TweenProperty),
    keys: &[AnimKey],
) -> Result<(), UiAnimError> {
    use crate::canvas_keys::{RowKey, list_rows};
    let row_name = |i: usize| {
        keys.get(i).map_or_else(String::new, |k| match k.value {
            AnimValue::Number(n) => format!("{} key at {:.2} s, value {n:.2}", property.name(), k.time),
            AnimValue::Color(c) => format!(
                "{} key at {:.2} s, color {:.2} {:.2} {:.2} {:.2}",
                property.name(),
                k.time,
                c[0],
                c[1],
                c[2],
                c[3]
            ),
        })
    };
    let selected = state
        .selected_key
        .and_then(|t| keys.iter().position(|k| k.time == t));
    let height = (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * keys.len().clamp(1, 5) as f32;
    let mut result = Ok(());
    for event in list_rows(
        ui,
        "ui-anim-keys",
        &mut state.keys,
        keys.len(),
        height,
        selected,
        &row_name,
    ) {
        match event {
            RowKey::Activate(i) => state.selected_key = keys.get(i).map(|k| k.time),
            RowKey::Connect(_) => {}
            RowKey::Delete(i) => {
                let Some(key) = keys.get(i) else { continue };
                result = result.and(doc.apply(AnimOp::RemoveKey {
                    name: name.to_owned(),
                    property: property.name().to_owned(),
                    time: key.time,
                }));
                state.selected_key = None;
            }
            RowKey::Palette(i) => {
                let Some(key) = keys.get(i) else { continue };
                let mut added = key.clone();
                added.time = snap_time(key.time + KEY_TIME_COARSE);
                if keys.iter().any(|k| k.time == added.time) {
                    continue;
                }
                state.selected_key = Some(added.time);
                result = result.and(doc.apply(AnimOp::SetKey {
                    name: name.to_owned(),
                    property: property.name().to_owned(),
                    key: added,
                }));
            }
            RowKey::Arrow {
                row, dx, dy, coarse, ..
            } => {
                let Some(key) = keys.get(row) else { continue };
                let mut moved = key.clone();
                if dx != 0 {
                    let step = if coarse { KEY_TIME_COARSE } else { KEY_TIME_STEP };
                    moved.time = snap_time(key.time + dx as f32 * step);
                }
                if let AnimValue::Number(n) = &mut moved.value {
                    *n -= dy as f32 * KEY_VALUE_STEP * if coarse { 10.0 } else { 1.0 };
                }
                if moved == *key {
                    continue;
                }
                state.selected_key = Some(moved.time);
                result = result.and(commit_key_drag(doc, name, property.name(), key.time, moved));
            }
        }
    }
    result
}

fn color(c: [f32; 4]) -> egui::Color32 {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
    egui::Color32::from_rgba_unmultiplied(s(c[0]), s(c[1]), s(c[2]), s(c[3]))
}

/// Draws the UI Animation tab.
pub fn ui_anim_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut editor = world.remove_resource::<UiAnimEditor>().unwrap_or_default();
    let mut state = world.remove_resource::<UiAnimPanelState>().unwrap_or_default();
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let designer = world.get_resource::<UiDesigner>();
    let facts = designer.and_then(UiDesigner::doc).map(|d| node_facts(d.file()));
    let preview_data = designer
        .and_then(UiDesigner::doc)
        .map(|d| (d.file().clone(), d.preview.clone()));
    draw(
        ui,
        &mut editor,
        &mut state,
        root,
        facts.as_deref(),
        preview_data.as_ref(),
    );
    world.insert_resource(editor);
    world.insert_resource(state);
}

fn report(editor: &mut UiAnimEditor, result: Result<(), UiAnimError>) {
    if let Err(e) = result {
        editor.error = Some(e.to_string());
    }
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut UiAnimEditor,
    state: &mut UiAnimPanelState,
    project: Option<PathBuf>,
    nodes: Option<&[regia_asset_pipeline::ui_anim_validate::UiNodeFact]>,
    designer: Option<&(
        regia_ui::authoring::UiFile,
        crate::ui_designer_preview::PreviewSettings,
    )>,
) {
    let _span = tracing::debug_span!(target: "regia_editor", "ui_anim_panel").entered();
    toolbar(ui, editor, state, project.as_ref());
    if let Some(err) = editor.error.clone() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::from_rgb(230, 110, 90), err);
            if crate::a11y_audit::glyph_button(ui, "x", "Dismiss the error").clicked() {
                editor.error = None;
            }
        });
    }
    if editor.doc().is_none() {
        ui.label("Open a .uianim.ron library or start a new one.");
        return;
    }
    // Scrub playback advances with the frame clock, never with wall time in a test: the panel runs
    // its UI twice a frame, so a frame is only counted once.
    let frame = ui.ctx().cumulative_frame_nr();
    let step = if state.last_frame != frame {
        state.last_frame = frame;
        ui.input(|i| i.stable_dt)
    } else {
        0.0
    };
    ui.columns(3, |cols| {
        animation_list(&mut cols[0], editor, state, nodes);
        tracks_view(&mut cols[1], editor, state, step);
        side_view(&mut cols[2], editor, nodes, designer);
    });
}

fn toolbar(
    ui: &mut egui::Ui,
    editor: &mut UiAnimEditor,
    state: &mut UiAnimPanelState,
    project: Option<&PathBuf>,
) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.open_path)
                .hint_text("assets/ui/hud.uianim.ron")
                .desired_width(220.0),
        );
        let path = || {
            let p = PathBuf::from(state.open_path.trim());
            match project {
                Some(root) if p.is_relative() => root.join(p),
                _ => p,
            }
        };
        if ui.button("Open").clicked() {
            match UiAnimDoc::open(&path()) {
                Ok(doc) => editor.set_doc(doc),
                Err(e) => editor.error = Some(e.to_string()),
            }
        }
        if ui.button("New").clicked() {
            let target = (!state.open_path.trim().is_empty()).then(path);
            editor.set_doc(UiAnimDoc::new_document(target));
        }
        let Ok(doc) = editor.doc_mut() else {
            return;
        };
        let undo = ui
            .add_enabled(doc.undo_len() > 0, egui::Button::new("Undo"))
            .clicked();
        let redo = ui
            .add_enabled(doc.redo_len() > 0, egui::Button::new("Redo"))
            .clicked();
        let mut result = Ok(());
        if undo {
            result = doc.undo();
        } else if redo {
            result = doc.redo();
        }
        if ui.button(if doc.dirty() { "Save*" } else { "Save" }).clicked() {
            let target = (!state.open_path.trim().is_empty() && doc.path.is_none()).then(path);
            result = doc.save(target).map(|_| ());
        }
        if let Some(p) = &doc.path {
            ui.label(p.display().to_string());
        }
        report(editor, result);
    });
}

fn animation_list(
    ui: &mut egui::Ui,
    editor: &mut UiAnimEditor,
    state: &mut UiAnimPanelState,
    nodes: Option<&[regia_asset_pipeline::ui_anim_validate::UiNodeFact]>,
) {
    ui.heading("Animations");
    let mut result = Ok(());
    let Ok(doc) = editor.doc_mut() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_name)
                .hint_text("name")
                .desired_width(90.0),
        );
        if let Some(nodes) = nodes {
            egui::ComboBox::from_id_salt("ui-anim-target")
                .selected_text(if state.target.is_empty() {
                    "(any node)"
                } else {
                    &state.target
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.target, String::new(), "(any node)");
                    for n in nodes {
                        ui.selectable_value(&mut state.target, n.name.clone(), &n.name);
                    }
                });
        }
        if ui.button("Create").clicked() && !state.new_name.trim().is_empty() {
            let name = if state.target.is_empty() {
                state.new_name.trim().to_owned()
            } else {
                format!("{}{TARGET_SEPARATOR}{}", state.target, state.new_name.trim())
            };
            result = doc.apply(AnimOp::AddAnimation {
                name: name.clone(),
                property: None,
            });
            if result.is_ok() {
                doc.selected = Some(name);
                state.new_name.clear();
            }
        }
    });
    let names: Vec<String> = doc.library().animations.keys().cloned().collect();
    for name in &names {
        let selected = doc.selected.as_deref() == Some(name);
        let flagged = nodes.is_some_and(|n| {
            let mut single = regia_ui::anim_timeline::UiAnimLibrary::default();
            if let Some(def) = doc.library().animations.get(name) {
                single.animations.insert(name.clone(), def.clone());
            }
            validate_ui_anim(&single, n)
                .iter()
                .any(|f| f.severity == regia_asset_pipeline::validators::Severity::Error)
        });
        let label = if flagged {
            format!("! {name}")
        } else {
            name.clone()
        };
        if ui.selectable_label(selected, label).clicked() {
            doc.selected = Some(name.clone());
            state.rename = name.clone();
            state.selected_key = None;
        }
    }
    let Some(name) = doc.selected.clone() else {
        report(editor, result);
        return;
    };
    ui.separator();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.rename)
                .hint_text("rename")
                .desired_width(110.0),
        );
        if ui.button("Rename").clicked() && !state.rename.trim().is_empty() {
            result = doc.apply(AnimOp::RenameAnimation {
                from: name.clone(),
                to: state.rename.trim().to_owned(),
            });
        }
        if ui.button("Delete").clicked() {
            result = doc.apply(AnimOp::RemoveAnimation(name.clone()));
        }
    });
    if let Some(def) = doc.library().animations.get(&name).cloned() {
        let mut mode = def.mode;
        let mut loops = def.loops;
        let mut speed = def.speed;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("ui-anim-mode")
                .selected_text(match mode {
                    LoopMode::Once => "once",
                    LoopMode::Loop => "loop",
                    LoopMode::PingPong => "ping-pong",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut mode, LoopMode::Once, "once");
                    ui.selectable_value(&mut mode, LoopMode::Loop, "loop");
                    ui.selectable_value(&mut mode, LoopMode::PingPong, "ping-pong");
                });
            ui.add_enabled(
                mode != LoopMode::Once,
                egui::DragValue::new(&mut loops)
                    .range(1..=regia_ui::anim_timeline::MAX_LOOPS)
                    .prefix("x"),
            );
            ui.add(
                egui::DragValue::new(&mut speed)
                    .range(regia_ui::anim_timeline::SPEED_RANGE.0..=regia_ui::anim_timeline::SPEED_RANGE.1)
                    .speed(0.05)
                    .prefix("speed "),
            );
        });
        if (mode, loops, speed) != (def.mode, def.loops, def.speed) {
            result = doc.apply(AnimOp::SetPlayback {
                name: name.clone(),
                mode: Some(mode),
                loops: Some(loops),
                speed: Some(speed),
            });
        }
        if target_of(&name).is_none() {
            ui.weak("Generic animation: plays on any node. Name it `node::label` to bind a node.");
        }
    }
    report(editor, result);
}

fn tracks_view(ui: &mut egui::Ui, editor: &mut UiAnimEditor, state: &mut UiAnimPanelState, step: f32) {
    ui.heading("Tracks");
    let (mut time, mut reverse, mut playing) = (editor.time, editor.reverse, editor.playing);
    let mut result = Ok(());
    {
        let Ok(doc) = editor.doc_mut() else {
            return;
        };
        let Some(name) = doc.selected.clone() else {
            ui.label("Create or select an animation.");
            return;
        };
        let Some(def) = doc.library().animations.get(&name).cloned() else {
            return;
        };
        let used: Vec<&str> = def.tracks.iter().map(|t| t.property.as_str()).collect();
        ui.horizontal_wrapped(|ui| {
            for t in &def.tracks {
                let on = TweenProperty::parse(&t.property)
                    .and_then(|p| TweenProperty::ALL.iter().position(|q| *q == p))
                    == Some(state.property);
                if ui.selectable_label(on, &t.property).clicked() {
                    state.property = TweenProperty::parse(&t.property)
                        .and_then(|p| TweenProperty::ALL.iter().position(|q| *q == p))
                        .unwrap_or(0);
                    state.selected_key = None;
                }
            }
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("ui-anim-property")
                .selected_text(TweenProperty::ALL[state.property.min(12)].name())
                .show_ui(ui, |ui| {
                    for (i, p) in TweenProperty::ALL.iter().enumerate() {
                        ui.selectable_value(&mut state.property, i, p.name());
                    }
                });
            let property = TweenProperty::ALL[state.property.min(12)].name();
            if ui
                .add_enabled(!used.contains(&property), egui::Button::new("Add track"))
                .clicked()
            {
                result = doc.apply(AnimOp::AddTrack {
                    name: name.clone(),
                    property: property.to_owned(),
                });
            }
            if ui
                .add_enabled(used.contains(&property), egui::Button::new("Remove track"))
                .clicked()
            {
                result = doc.apply(AnimOp::RemoveTrack {
                    name: name.clone(),
                    property: property.to_owned(),
                });
            }
        });
        result = result.and(dope_sheet(ui, doc, state, &name, &def, time));
        result = result.and(event_track(ui, doc, state, &name, &def, time));
        let property = TweenProperty::ALL[state.property.min(12)];
        if let Some(track) = def.tracks.iter().find(|t| t.property == property.name()) {
            let view = TrackView {
                name: &name,
                def: &def,
                property,
                keys: track.keys.clone(),
                playhead: time,
            };
            result = result.and(key_editor(ui, doc, state, view));
        }
        // Scrub row.
        let duration = def.playback_duration(None).max(0.01);
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut time, 0.0..=duration).text("s"));
            ui.checkbox(&mut reverse, "reverse");
            if ui.button(if playing { "Pause" } else { "Play" }).clicked() {
                playing = !playing;
                if playing && time >= duration {
                    time = 0.0;
                }
            }
        });
        if playing {
            time = (time + step).min(duration);
            if time >= duration {
                playing = false;
            }
            ui.ctx().request_repaint();
        }
    }
    editor.time = time;
    editor.reverse = reverse;
    editor.playing = playing;
    report(editor, result);
}

/// The accessible name of dope-sheet key `k`, marked when selected.
fn dope_key_name(k: &DopeKey, selected: bool) -> String {
    let mut name = format!("Dope sheet: {} key at {:.2} s", k.property, k.time);
    if selected {
        name.push_str(", selected");
    }
    name
}

/// The dope sheet of `def` (animation `name`) and its key list; see the module docs.
fn dope_sheet(
    ui: &mut egui::Ui,
    doc: &mut UiAnimDoc,
    state: &mut UiAnimPanelState,
    name: &str,
    def: &UiAnimDef,
    playhead: f32,
) -> Result<(), UiAnimError> {
    let all = dope_keys(def);
    // Keys an edit or an undo took away leave the selection.
    state.dope.retain(|k| all.contains(k));
    let end = def.duration().max(playhead).max(0.5);
    let rows = def.tracks.len();
    // The frame events get a row of their own under the tracks (`.uianim.ron` v2).
    let event_rows = usize::from(!def.events.is_empty());
    let width = ui.available_width().max(160.0);
    let height = (rows + event_rows) as f32 * DOPE_ROW + 6.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
    state.dope_rect = Some(rect);
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("UI animation dope sheet, {rows} tracks, {} keys", all.len()),
    );
    let left = rect.left() + DOPE_NAME_WIDTH;
    let scale = (rect.right() - left - 8.0).max(40.0) / end;
    let x_of = |t: f32| left + t * scale;
    let t_of = |x: f32| ((x - left) / scale).max(0.0);
    let row_of = |property: &str| def.tracks.iter().position(|t| t.property == property);
    let center = |k: &DopeKey| {
        row_of(&k.property).map(|r| egui::pos2(x_of(k.time), rect.top() + 3.0 + (r as f32 + 0.5) * DOPE_ROW))
    };
    let pick = |p: egui::Pos2| {
        all.iter()
            .filter_map(|k| center(k).map(|c| (k, c.distance(p))))
            .filter(|(_, d)| *d <= KEY_SIZE + 2.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| k.clone())
    };
    let shift = ui.input(|i| i.modifiers.shift);
    let mut result = Ok(());
    // Pointer: press on a key selects it and starts a move; on empty space, a box.
    if response.drag_started_by(egui::PointerButton::Primary)
        && let Some(origin) = ui
            .input(|i| i.pointer.press_origin())
            .or(response.interact_pointer_pos())
    {
        state.dope_drag = Some(match pick(origin) {
            Some(k) => {
                if !state.dope.contains(&k) {
                    if !shift {
                        state.dope.clear();
                    }
                    state.dope.push(k);
                }
                DopeDrag::Move { from: t_of(origin.x) }
            }
            None => DopeDrag::Box { from: origin },
        });
    }
    let pointer = response.interact_pointer_pos().or(response.hover_pos());
    if response.drag_stopped()
        && let Some(drag) = state.dope_drag.take()
        && let Some(at) = pointer
    {
        match drag {
            DopeDrag::Move { from } => {
                let by = snap_time(t_of(at.x)) - snap_time(from);
                if by != 0.0 && crate::canvas_keys::first_in_this_frame(ui, response.id.with("dope_move")) {
                    match move_dope_keys(doc, name, &state.dope, by) {
                        Ok(moved) => state.dope = moved,
                        Err(e) => result = Err(e),
                    }
                }
            }
            DopeDrag::Box { from } => {
                let area = egui::Rect::from_two_pos(from, at);
                if !shift {
                    state.dope.clear();
                }
                for k in &all {
                    if center(k).is_some_and(|c| area.contains(c)) && !state.dope.contains(k) {
                        state.dope.push(k.clone());
                    }
                }
            }
        }
    } else if response.clicked()
        && let Some(at) = pointer
    {
        match pick(at) {
            Some(k) if shift => {
                if let Some(i) = state.dope.iter().position(|s| *s == k) {
                    state.dope.remove(i);
                } else {
                    state.dope.push(k);
                }
            }
            Some(k) => state.dope = vec![k],
            None if !shift => state.dope.clear(),
            None => {}
        }
    }
    // Paint: rows, names, keys (the selection at its dragged place), the box, the playhead.
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);
    let font = egui::FontId::proportional(11.0);
    for (r, track) in def.tracks.iter().enumerate() {
        let y = rect.top() + 3.0 + r as f32 * DOPE_ROW;
        if r % 2 == 1 {
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(rect.width(), DOPE_ROW)),
                0.0,
                visuals.faint_bg_color,
            );
        }
        painter.text(
            egui::pos2(rect.left() + 4.0, y + DOPE_ROW / 2.0),
            egui::Align2::LEFT_CENTER,
            &track.property,
            font.clone(),
            visuals.text_color(),
        );
    }
    let offset = match (state.dope_drag, pointer) {
        (Some(DopeDrag::Move { from }), Some(at)) if response.dragged() => {
            snap_time(t_of(at.x)) - snap_time(from)
        }
        _ => 0.0,
    };
    for k in &all {
        let selected = state.dope.contains(k);
        let Some(mut c) = center(k) else { continue };
        if selected {
            c.x = x_of((k.time + offset).max(0.0));
        }
        let fill = if selected {
            tokens.accents.select
        } else {
            visuals.text_color()
        };
        let r = KEY_SIZE * 0.8;
        painter.add(egui::Shape::convex_polygon(
            vec![
                c + egui::vec2(0.0, -r),
                c + egui::vec2(r, 0.0),
                c + egui::vec2(0.0, r),
                c + egui::vec2(-r, 0.0),
            ],
            fill,
            egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        ));
    }
    if let (Some(DopeDrag::Box { from }), Some(at)) = (state.dope_drag, pointer)
        && response.dragged()
    {
        painter.rect_stroke(
            egui::Rect::from_two_pos(from, at),
            0.0,
            egui::Stroke::new(1.0, tokens.accents.active),
            egui::StrokeKind::Inside,
        );
    }
    if event_rows > 0 {
        let y = rect.top() + 3.0 + rows as f32 * DOPE_ROW;
        painter.text(
            egui::pos2(rect.left() + 4.0, y + DOPE_ROW / 2.0),
            egui::Align2::LEFT_CENTER,
            "events",
            font.clone(),
            visuals.text_color(),
        );
        for (i, e) in def.events.iter().enumerate() {
            let x = x_of(e.time);
            let picked = state.event.as_ref().is_some_and(|d| d.index == i);
            let fill = if picked {
                tokens.accents.select
            } else {
                tokens.accents.active
            };
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(x, y + 2.0),
                    egui::pos2(x + KEY_SIZE, y + DOPE_ROW / 2.0),
                    egui::pos2(x, y + DOPE_ROW - 2.0),
                ],
                fill,
                egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
            ));
        }
    }
    let x = x_of(playhead);
    painter.line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(1.0, tokens.accents.alert),
    );
    // Keyboard: the key list.
    let names: Vec<String> = all
        .iter()
        .map(|k| dope_key_name(k, state.dope.contains(k)))
        .collect();
    let events = crate::canvas_keys::list_rows(
        ui,
        "ui-anim-dope-keys",
        &mut state.dope_keys,
        all.len(),
        crate::canvas_keys::LIST_HEIGHT / 2.0,
        None,
        &|i| names.get(i).cloned().unwrap_or_default(),
    );
    for event in events {
        match event {
            crate::canvas_keys::RowKey::Activate(i) => {
                if let Some(k) = all.get(i) {
                    if let Some(at) = state.dope.iter().position(|s| s == k) {
                        state.dope.remove(at);
                    } else {
                        state.dope.push(k.clone());
                    }
                }
            }
            crate::canvas_keys::RowKey::Arrow {
                row,
                dx,
                alt: false,
                coarse,
                ..
            } if dx != 0 => {
                let keys: Vec<DopeKey> = if state.dope.is_empty() {
                    all.get(row).cloned().into_iter().collect()
                } else {
                    state.dope.clone()
                };
                let step = if coarse { KEY_TIME_COARSE } else { KEY_TIME_STEP };
                match move_dope_keys(doc, name, &keys, dx as f32 * step) {
                    Ok(moved) => {
                        if !state.dope.is_empty() {
                            state.dope = moved;
                        }
                    }
                    Err(e) => result = Err(e),
                }
            }
            _ => {}
        }
    }
    result
}

/// The accessible name of event row `i`.
fn event_row_name(i: usize, e: &AnimEvent) -> String {
    if e.payload.is_empty() {
        format!("Event {i}: {} at {:.2} s", e.name, e.time)
    } else {
        format!("Event {i}: {} at {:.2} s, payload {}", e.name, e.time, e.payload)
    }
}

/// The event track of `def` (animation `name`): see the module docs. Every commit is one
/// [`AnimOp`] through [`UiAnimDoc::apply`].
fn event_track(
    ui: &mut egui::Ui,
    doc: &mut UiAnimDoc,
    state: &mut UiAnimPanelState,
    name: &str,
    def: &UiAnimDef,
    playhead: f32,
) -> Result<(), UiAnimError> {
    // An event an edit or an undo took away leaves the selection.
    if state.event.as_ref().is_some_and(|d| d.index >= def.events.len()) {
        state.event = None;
    }
    let mut result = Ok(());
    ui.horizontal(|ui| {
        ui.label(format!("Events ({})", def.events.len()));
        if ui.button("Add event").clicked() {
            let time = snap_time(playhead.min(def.duration()));
            result = doc.apply(AnimOp::SetEvent {
                name: name.to_owned(),
                index: None,
                event: AnimEvent {
                    time,
                    name: "event".to_owned(),
                    payload: String::new(),
                },
            });
            if result.is_ok() {
                state.event = Some(EventDraft {
                    index: def.events.len(),
                    name: "event".to_owned(),
                    payload: String::new(),
                });
            }
        }
    });
    for (i, e) in def.events.iter().enumerate() {
        let on = state.event.as_ref().is_some_and(|d| d.index == i);
        if ui.selectable_label(on, event_row_name(i, e)).clicked() {
            state.event = (!on).then(|| EventDraft {
                index: i,
                name: e.name.clone(),
                payload: e.payload.clone(),
            });
        }
    }
    let Some(draft) = state.event.as_mut() else {
        return result;
    };
    let Some(current) = def.events.get(draft.index) else {
        return result;
    };
    let mut edited = current.clone();
    let mut remove = false;
    ui.horizontal(|ui| {
        ui.label("time");
        let time = ui.add(
            egui::DragValue::new(&mut edited.time)
                .speed(0.01)
                .range(0.0..=def.duration()),
        );
        crate::a11y_audit::name(time, egui::WidgetType::DragValue, "Event time");
        ui.label("name");
        let field = ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(96.0));
        if crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Event name").lost_focus() {
            edited.name.clone_from(&draft.name);
        }
        ui.label("payload");
        let field = ui.add(egui::TextEdit::singleline(&mut draft.payload).desired_width(96.0));
        if crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Event payload").lost_focus() {
            edited.payload.clone_from(&draft.payload);
        }
        if ui.button("Remove event").clicked() {
            remove = true;
        }
    });
    let index = draft.index;
    if remove {
        state.event = None;
        result = result.and(doc.apply(AnimOp::RemoveEvent {
            name: name.to_owned(),
            index,
        }));
    } else if &edited != current {
        result = result.and(doc.apply(AnimOp::SetEvent {
            name: name.to_owned(),
            index: Some(index),
            event: edited,
        }));
    }
    result
}

/// What the key editor draws: one track of one animation and the scrub position.
struct TrackView<'a> {
    name: &'a str,
    def: &'a UiAnimDef,
    property: TweenProperty,
    keys: Vec<AnimKey>,
    playhead: f32,
}

fn key_editor(
    ui: &mut egui::Ui,
    doc: &mut UiAnimDoc,
    state: &mut UiAnimPanelState,
    track: TrackView<'_>,
) -> Result<(), UiAnimError> {
    let TrackView {
        name,
        def,
        property,
        keys,
        playhead,
    } = track;
    let mut result = Ok(());
    let is_color = property.is_color();
    let number = |k: &AnimKey| match k.value {
        AnimValue::Number(n) => n,
        AnimValue::Color(c) => c[3],
    };
    if is_color {
        // Colour tracks are edited as swatches; the curve view shows the alpha channel.
        ui.horizontal_wrapped(|ui| {
            for k in &keys {
                if let AnimValue::Color(c) = k.value {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                    ui.painter().rect_filled(rect, 2.0, color(c));
                    if response.clicked() {
                        state.selected_key = Some(k.time);
                    }
                }
            }
        });
    }
    let points: Vec<(f32, f32)> = keys.iter().map(|k| (k.time, number(k))).collect();
    let size = egui::vec2(ui.available_width().max(120.0), 160.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let fit = Some((name.to_owned(), property.name().to_owned()));
    let view = &mut state.view;
    view.rect = rect;
    if state.fitted_for != fit {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for &(_, v) in &points {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        let end = def.duration().max(1.0);
        view.zoom_to_fit([0.0, lo.min(0.0), end, hi.max(lo + 0.5)], 0.1);
        state.fitted_for = fit;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    view.paint_grid(&painter, |x| format!("{x:.2}s"), true);
    // The curve: the engine's own sampling of this one track.
    let mut single = def.clone();
    single.tracks.retain(|t| t.property == property.name());
    single.mode = LoopMode::Once;
    single.loops = 1;
    single.speed = 1.0;
    let steps = (rect.width() / 3.0).max(8.0) as usize;
    let line: Vec<egui::Pos2> = (0..=steps)
        .filter_map(|i| {
            let t = view
                .to_value(egui::pos2(
                    rect.left() + rect.width() * i as f32 / steps as f32,
                    0.0,
                ))
                .x
                .max(0.0);
            let values = sample_def(&single, t, false, None).ok()?;
            let v = match values.first()?.1 {
                TweenValue::Number(n) => n,
                TweenValue::Color(c) => c.0[3],
            };
            Some(view.to_screen(egui::vec2(t, v)))
        })
        .collect();
    painter.add(egui::Shape::line(
        line,
        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 255)),
    ));
    for (t, v) in &points {
        let s = view.to_screen(egui::vec2(*t, *v));
        let fill = if state.selected_key == Some(*t) {
            crate::theme::ui_tokens(painter.ctx()).accents.active
        } else {
            egui::Color32::WHITE
        };
        painter.rect_filled(
            egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE)),
            1.0,
            fill,
        );
        if state
            .keys
            .focused()
            .and_then(|i| points.get(i))
            .is_some_and(|p| p.0 == *t)
        {
            crate::canvas_keys::focus_ring(
                &painter,
                egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE)),
            );
        }
    }
    let x = view.to_screen(egui::vec2(playhead, 0.0)).x;
    painter.line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(1.5, crate::theme::ui_tokens(painter.ctx()).accents.alert),
    );
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle)
    {
        view.pan(response.drag_delta());
    }
    let nearest = |pos: egui::Pos2| {
        points
            .iter()
            .map(|&(t, v)| (t, view.to_screen(egui::vec2(t, v)).distance(pos)))
            .filter(|(_, d)| *d <= KEY_SIZE * 1.5)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(t, _)| t)
    };
    if response.drag_started_by(egui::PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
    {
        state.dragging = nearest(pos);
        if state.dragging.is_some() {
            state.selected_key = state.dragging;
        }
    }
    if response.drag_stopped()
        && let (Some(from), Some(pos)) = (state.dragging.take(), response.interact_pointer_pos())
        && !is_color
    {
        // One undo step per drag: move the key to where it was dropped (time snapped to 10 ms).
        let at = view.to_value(pos);
        let time = ((at.x.max(0.0)) * 100.0).round() / 100.0;
        if let Some(key) = keys.iter().find(|k| k.time == from) {
            let mut moved = key.clone();
            moved.time = time;
            moved.value = AnimValue::Number(at.y);
            result = result.and(commit_key_drag(doc, name, property.name(), from, moved));
            state.selected_key = Some(time);
        }
    } else if response.clicked_by(egui::PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
    {
        match nearest(pos) {
            Some(t) => state.selected_key = Some(t),
            None if !is_color => {
                let at = view.to_value(pos);
                let time = ((at.x.max(0.0)) * 100.0).round() / 100.0;
                result = doc.apply(AnimOp::SetKey {
                    name: name.to_owned(),
                    property: property.name().to_owned(),
                    key: AnimKey {
                        time,
                        value: AnimValue::Number(at.y),
                        ease: None,
                    },
                });
                state.selected_key = Some(time);
            }
            None => {}
        }
    } else if response.secondary_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(t) = nearest(pos)
    {
        result = doc.apply(AnimOp::RemoveKey {
            name: name.to_owned(),
            property: property.name().to_owned(),
            time: t,
        });
        state.selected_key = None;
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("{} key canvas, {} keys", property.name(), keys.len()),
    );
    result = result.and(key_list(ui, doc, state, (name, property), &keys));
    // The selected key's fields.
    if let Some(key) = state.selected_key.and_then(|t| keys.iter().find(|k| k.time == t)) {
        let mut edited = key.clone();
        let mut delete_key = false;
        ui.horizontal(|ui| {
            ui.label(format!("key at {:.2}s", key.time));
            match &mut edited.value {
                AnimValue::Number(n) => {
                    ui.add(egui::DragValue::new(n).speed(0.01));
                }
                AnimValue::Color(c) => {
                    let mut rgba = egui::Rgba::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
                    if egui::color_picker::color_edit_button_rgba(
                        ui,
                        &mut rgba,
                        egui::color_picker::Alpha::OnlyBlend,
                    )
                    .changed()
                    {
                        *c = rgba.to_array();
                    }
                }
            }
            let current = edited.ease.clone().unwrap_or_else(|| "linear".to_owned());
            egui::ComboBox::from_id_salt("ui-anim-ease")
                .selected_text(&current)
                .show_ui(ui, |ui| {
                    for (ease, _) in Ease::NAMED {
                        if ui.selectable_label(current == ease, ease).clicked() {
                            edited.ease = (ease != "linear").then(|| ease.to_owned());
                        }
                    }
                });
            if ui.button("Delete key").clicked() {
                delete_key = true;
            }
        });
        if delete_key {
            result = result.and(doc.apply(AnimOp::RemoveKey {
                name: name.to_owned(),
                property: property.name().to_owned(),
                time: key.time,
            }));
            state.selected_key = None;
        } else if &edited != key {
            result = result.and(doc.apply(AnimOp::SetKey {
                name: name.to_owned(),
                property: property.name().to_owned(),
                key: edited,
            }));
        }
    }
    result
}

/// The drop of a key drag: the key at `from` becomes `moved` (its new time and value) as one undo
/// step, all or nothing (review rc1-rev-editor-k finding 6: the remove and the set were two steps, and
/// a refused set left the key removed).
fn commit_key_drag(
    doc: &mut UiAnimDoc,
    name: &str,
    property: &str,
    from: f32,
    moved: AnimKey,
) -> Result<(), UiAnimError> {
    let mut ops = Vec::new();
    if (moved.time - from).abs() > f32::EPSILON {
        ops.push(AnimOp::RemoveKey {
            name: name.to_owned(),
            property: property.to_owned(),
            time: from,
        });
    }
    ops.push(AnimOp::SetKey {
        name: name.to_owned(),
        property: property.to_owned(),
        key: moved,
    });
    doc.apply_all(ops)
}

fn side_view(
    ui: &mut egui::Ui,
    editor: &mut UiAnimEditor,
    nodes: Option<&[regia_asset_pipeline::ui_anim_validate::UiNodeFact]>,
    designer: Option<&(
        regia_ui::authoring::UiFile,
        crate::ui_designer_preview::PreviewSettings,
    )>,
) {
    ui.heading("Preview");
    let (time, reverse) = (editor.time, editor.reverse);
    let Ok(doc) = editor.doc_mut() else {
        return;
    };
    let Some(name) = doc.selected.clone() else {
        return;
    };
    let values = doc.sample(&name, time, reverse, None);
    match &values {
        Ok(values) => {
            for (p, v) in values {
                match v {
                    TweenValue::Number(n) => ui.label(format!("{}: {n:.3}", p.name())),
                    TweenValue::Color(c) => ui.colored_label(color(c.0), format!("{}: {:?}", p.name(), c.0)),
                };
            }
        }
        Err(e) => {
            ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
        }
    }
    // The target node laid out with the sampled values, on a copy of the designer's document.
    if let (Ok(values), Some((file, settings)), Some(target)) = (&values, designer, target_of(&name)) {
        match apply_to_file(file, target, values) {
            Some((applied, skipped)) => match preview(&applied, settings, None) {
                Ok(frame) => {
                    let width = ui.available_width().clamp(120.0, 320.0);
                    let scale = width / frame.size[0].max(1.0);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(width, (frame.size[1] * scale).min(400.0)),
                        egui::Sense::hover(),
                    );
                    let painter = ui.painter_at(rect);
                    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(30));
                    let alpha = values
                        .iter()
                        .find_map(|(p, v)| match (p, v) {
                            (TweenProperty::Opacity, TweenValue::Number(n)) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(1.0);
                    for n in frame.nodes.iter().filter(|n| n.visible) {
                        let r = egui::Rect::from_min_size(
                            rect.min + egui::vec2(n.rect[0], n.rect[1]) * scale,
                            egui::vec2(n.rect[2], n.rect[3]) * scale,
                        );
                        let hit = n.name.as_deref() == Some(target);
                        let stroke = if hit {
                            egui::Stroke::new(
                                2.0,
                                egui::Color32::from_rgba_unmultiplied(
                                    120,
                                    200,
                                    255,
                                    (alpha.clamp(0.0, 1.0) * 255.0) as u8,
                                ),
                            )
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::from_gray(90))
                        };
                        painter.rect_stroke(r, 0.0, stroke, egui::StrokeKind::Inside);
                    }
                    if !skipped.is_empty() {
                        ui.weak(format!("Runtime-only here: {}", skipped.join(", ")));
                    }
                }
                Err(e) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
                }
            },
            None => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("The document has no node named `{target}`."),
                );
            }
        }
    } else if designer.is_none() {
        ui.weak("Open a .ui.ron in the UI Designer to preview the target node.");
    }
    ui.separator();
    ui.heading("Problems");
    match nodes {
        Some(nodes) => {
            let found = validate_ui_anim(doc.library(), nodes);
            if found.is_empty() {
                ui.label("No problems.");
            }
            for f in &found {
                let c = if f.severity == regia_asset_pipeline::validators::Severity::Error {
                    egui::Color32::from_rgb(230, 110, 90)
                } else {
                    egui::Color32::from_rgb(220, 180, 90)
                };
                ui.colored_label(c, format!("{}: {}", f.code, f.text));
            }
        }
        None => {
            ui.weak("Targets are checked against the UI Designer's open document.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(doc: &UiAnimDoc) -> Vec<f32> {
        doc.library().animations["fade"].tracks[0]
            .keys
            .iter()
            .map(|k| k.time)
            .collect()
    }

    fn key(time: f32, value: f32) -> AnimKey {
        AnimKey {
            time,
            value: AnimValue::Number(value),
            ease: None,
        }
    }

    /// A doc with animation `fade`: one `opacity` track, keys at 0 s and (with `second`) 0.5 s.
    fn doc(second: bool) -> UiAnimDoc {
        let mut doc = UiAnimDoc::new_document(None);
        doc.apply(AnimOp::AddAnimation {
            name: "fade".into(),
            property: None,
        })
        .expect("add");
        if second {
            doc.apply(AnimOp::SetKey {
                name: "fade".into(),
                property: "opacity".into(),
                key: key(0.5, 0.5),
            })
            .expect("second key");
        }
        doc
    }

    /// Review rc1-rev-editor-k finding 6: a key drag is one undo step (it was a remove and a set,
    /// two steps, so Ctrl+Z after a drag left the key deleted).
    #[test]
    fn a_key_drag_is_one_undo_step() {
        let mut doc = doc(true);
        let before = doc.undo_len();
        commit_key_drag(&mut doc, "fade", "opacity", 0.5, key(0.75, 0.25)).expect("drag");
        assert_eq!(times(&doc), [0.0, 0.75]);
        assert_eq!(doc.undo_len(), before + 1, "one step per drag");
        doc.undo().expect("undo");
        assert_eq!(times(&doc), [0.0, 0.5], "one Ctrl+Z puts the key back");
    }

    /// Finding 6, the transaction half: a drop the set refuses leaves the key where it was (the
    /// remove used to stay applied), and a track's only key can move (the remove alone emptied the
    /// track, which validation refused).
    #[test]
    fn a_refused_drop_keeps_the_key_and_a_lone_key_moves() {
        let mut doc = doc(true);
        let before = (doc.undo_len(), doc.revision());
        let refused = commit_key_drag(&mut doc, "fade", "opacity", 0.5, key(0.75, f32::NAN));
        assert_eq!(refused.expect_err("non-finite value").code, "invalid");
        assert_eq!(times(&doc), [0.0, 0.5]);
        assert_eq!((doc.undo_len(), doc.revision()), before);

        let mut lone = self::doc(false);
        commit_key_drag(&mut lone, "fade", "opacity", 0.0, key(0.25, 1.0)).expect("lone key moves");
        assert_eq!(times(&lone), [0.25]);
    }

    /// FU-5: the key list selects a key (Enter), moves it (Left, one undo step), changes its value
    /// (Alt+Up) and deletes the other key (Delete) with keys only; the library equals the one the
    /// canvas's drag and right-click ops build.
    #[test]
    fn the_key_list_edits_by_keyboard_what_the_mouse_edits() {
        use crate::canvas_keys::test_input::{focused_name, frame, key as press, labels_of, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [900.0, 700.0];
        let mut mouse = doc(true);
        let first = doc(true).library().animations["fade"].tracks[0].keys.clone();
        let mut moved = first[1].clone();
        moved.time = snap_time(0.5 - KEY_TIME_STEP);
        commit_key_drag(&mut mouse, "fade", "opacity", 0.5, moved.clone()).expect("drag in time");
        let mut lifted = moved.clone();
        if let AnimValue::Number(n) = &mut lifted.value {
            *n += KEY_VALUE_STEP;
        }
        commit_key_drag(&mut mouse, "fade", "opacity", moved.time, lifted).expect("drag in value");
        mouse
            .apply(AnimOp::RemoveKey {
                name: "fade".into(),
                property: "opacity".into(),
                time: 0.0,
            })
            .expect("right-click delete");

        let mut doc = doc(true);
        let mut state = UiAnimPanelState::default();
        let ctx = egui::Context::default();
        let draw = |doc: &mut UiAnimDoc, state: &mut UiAnimPanelState, ui: &mut egui::Ui| {
            let def = doc.library().animations["fade"].clone();
            let keys = def.tracks[0].keys.clone();
            let view = TrackView {
                name: "fade",
                def: &def,
                property: TweenProperty::Opacity,
                keys,
                playhead: 0.0,
            };
            key_editor(ui, doc, state, view).expect("no refused edit");
        };
        let start = frame(&ctx, SIZE, vec![], |ui| draw(&mut doc, &mut state, ui));
        assert!(
            labels_of(&start, egui::accesskit::Role::Canvas)
                .contains(&"opacity key canvas, 2 keys".to_string()),
            "{:?}",
            labels_of(&start, egui::accesskit::Role::Canvas)
        );
        let second = format!(
            "opacity key at 0.50 s, value {:.2}",
            match first[1].value {
                AnimValue::Number(n) => n,
                AnimValue::Color(c) => c[3],
            }
        );
        tab_to(&ctx, SIZE, &second, false, 40, |ui| {
            draw(&mut doc, &mut state, ui)
        });
        frame(&ctx, SIZE, vec![press(Key::Enter, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(state.selected_key, Some(0.5), "Enter selects");
        let depth = doc.undo_len();
        frame(&ctx, SIZE, vec![press(Key::ArrowLeft, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(doc.undo_len(), depth + 1, "one undo step per press");
        frame(&ctx, SIZE, vec![press(Key::ArrowUp, Modifiers::ALT)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        frame(&ctx, SIZE, vec![press(Key::ArrowUp, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        let up = frame(&ctx, SIZE, vec![], |ui| draw(&mut doc, &mut state, ui));
        assert!(
            focused_name(&up).is_some_and(|n| n.starts_with("opacity key at 0.00 s")),
            "{:?}",
            focused_name(&up)
        );
        frame(&ctx, SIZE, vec![press(Key::Delete, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(doc.library(), mouse.library());
        assert_eq!(doc.text().ok(), mouse.text().ok());
    }

    /// w13 (P-223): the dope sheet selects keys on two tracks from the keyboard (Enter on each
    /// key-list row) and Right moves both in one undo step; one undo puts both back. The mouse
    /// drag's batch (`move_dope_keys`) is the same single step.
    #[test]
    fn the_dope_sheet_moves_keys_across_tracks_in_one_undo_step() {
        use crate::canvas_keys::test_input::{frame, key as press, labels_of, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [900.0, 700.0];
        let mut doc = doc(true);
        doc.apply(AnimOp::SetKey {
            name: "fade".into(),
            property: "translate_x".into(),
            key: key(0.5, 10.0),
        })
        .expect("second track");
        let track_times = |doc: &UiAnimDoc, property: &str| -> Vec<f32> {
            doc.library().animations["fade"]
                .tracks
                .iter()
                .find(|t| t.property == property)
                .map(|t| t.keys.iter().map(|k| k.time).collect())
                .unwrap_or_default()
        };
        let before = (track_times(&doc, "opacity"), track_times(&doc, "translate_x"));
        let mut state = UiAnimPanelState::default();
        let ctx = egui::Context::default();
        let draw = |doc: &mut UiAnimDoc, state: &mut UiAnimPanelState, ui: &mut egui::Ui| {
            let def = doc.library().animations["fade"].clone();
            dope_sheet(ui, doc, state, "fade", &def, 0.0).expect("no refused edit");
        };
        let start = frame(&ctx, SIZE, vec![], |ui| draw(&mut doc, &mut state, ui));
        let canvases = labels_of(&start, egui::accesskit::Role::Canvas);
        assert!(
            canvases.contains(&"UI animation dope sheet, 2 tracks, 3 keys".to_string()),
            "{canvases:?}"
        );
        for row in [
            "Dope sheet: opacity key at 0.50 s",
            "Dope sheet: translate_x key at 0.50 s",
        ] {
            tab_to(&ctx, SIZE, row, false, 20, |ui| draw(&mut doc, &mut state, ui));
            frame(&ctx, SIZE, vec![press(Key::Enter, Modifiers::NONE)], |ui| {
                draw(&mut doc, &mut state, ui)
            });
        }
        assert_eq!(
            state.dope.len(),
            2,
            "Enter selected a key on each track: {:?}",
            state.dope
        );
        let depth = doc.undo_len();
        frame(&ctx, SIZE, vec![press(Key::ArrowRight, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(doc.undo_len(), depth + 1, "both keys moved in one undo step");
        assert_eq!(track_times(&doc, "opacity"), [0.0, 0.51]);
        assert_eq!(track_times(&doc, "translate_x"), [0.51]);
        assert_eq!(state.dope.len(), 2, "the selection follows the keys");
        doc.undo().expect("undo");
        assert_eq!(
            (track_times(&doc, "opacity"), track_times(&doc, "translate_x")),
            before,
            "one undo puts both back"
        );
        // The drag's batch: a key of each track, the first past its neighbour, one step.
        let keys = [
            DopeKey {
                property: "opacity".into(),
                time: 0.0,
            },
            DopeKey {
                property: "translate_x".into(),
                time: 0.5,
            },
        ];
        let depth = doc.undo_len();
        let moved = move_dope_keys(&mut doc, "fade", &keys, 0.75).expect("move");
        assert_eq!(doc.undo_len(), depth + 1);
        assert_eq!(moved.iter().map(|k| k.time).collect::<Vec<_>>(), [0.75, 1.25]);
        assert_eq!(track_times(&doc, "opacity"), [0.5, 0.75]);
        assert_eq!(track_times(&doc, "translate_x"), [1.25]);
        // Refusals change nothing: a key listed twice, a key that is not there.
        let revision = doc.revision();
        let twice = [keys[0].clone(), keys[0].clone()];
        assert_eq!(
            move_dope_keys(&mut doc, "fade", &twice, 0.1)
                .expect_err("twice")
                .code,
            "bad_value"
        );
        assert_eq!(
            move_dope_keys(&mut doc, "fade", &keys, 0.1)
                .expect_err("gone")
                .code,
            "unknown_key"
        );
        assert_eq!(doc.revision(), revision);
    }

    /// The event track (`.uianim.ron` v2, ADR-8900) from the keyboard: Add event puts one at the
    /// playhead, its row selects it, a typed name commits when the field loses focus, Remove event
    /// deletes it; each commit is one undo step. A refused event (past the end) changes nothing.
    #[test]
    fn the_event_track_adds_renames_and_removes_events_by_keyboard() {
        use crate::canvas_keys::test_input::{frame, key as press, tab_to, text};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [900.0, 700.0];
        let mut doc = doc(true);
        let mut state = UiAnimPanelState::default();
        let ctx = egui::Context::default();
        let draw = |doc: &mut UiAnimDoc, state: &mut UiAnimPanelState, ui: &mut egui::Ui| {
            let def = doc.library().animations["fade"].clone();
            event_track(ui, doc, state, "fade", &def, 0.25).expect("no refused edit");
        };
        let events = |doc: &UiAnimDoc| doc.library().animations["fade"].events.clone();
        frame(&ctx, SIZE, vec![], |ui| draw(&mut doc, &mut state, ui));
        let depth = doc.undo_len();
        tab_to(&ctx, SIZE, "Add event", false, 20, |ui| {
            draw(&mut doc, &mut state, ui)
        });
        frame(&ctx, SIZE, vec![press(Key::Enter, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(doc.undo_len(), depth + 1, "one step");
        assert_eq!(
            events(&doc),
            [AnimEvent {
                time: 0.25,
                name: "event".into(),
                payload: String::new(),
            }]
        );
        assert_eq!(
            state.event.as_ref().map(|d| d.index),
            Some(0),
            "the new event is selected"
        );
        // The name field: type, Tab away commits once.
        tab_to(&ctx, SIZE, "Event name", false, 20, |ui| {
            draw(&mut doc, &mut state, ui)
        });
        if let Some(d) = state.event.as_mut() {
            d.name.clear();
        }
        frame(&ctx, SIZE, vec![text("sfx_pop")], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert_eq!(doc.undo_len(), depth + 1, "typing commits nothing yet");
        frame(&ctx, SIZE, vec![press(Key::Tab, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        frame(&ctx, SIZE, vec![], |ui| draw(&mut doc, &mut state, ui));
        assert_eq!(events(&doc)[0].name, "sfx_pop");
        assert_eq!(doc.undo_len(), depth + 2, "the rename is one step");
        tab_to(&ctx, SIZE, "Remove event", false, 20, |ui| {
            draw(&mut doc, &mut state, ui)
        });
        frame(&ctx, SIZE, vec![press(Key::Enter, Modifiers::NONE)], |ui| {
            draw(&mut doc, &mut state, ui)
        });
        assert!(events(&doc).is_empty());
        assert_eq!(doc.undo_len(), depth + 3);
        doc.undo().expect("undo");
        assert_eq!(events(&doc)[0].name, "sfx_pop", "undo brings it back");
        // Refusals: an event past the animation's end, an index that is not there.
        let revision = doc.revision();
        let past = doc
            .apply(AnimOp::SetEvent {
                name: "fade".into(),
                index: None,
                event: AnimEvent {
                    time: 9.0,
                    name: "late".into(),
                    payload: String::new(),
                },
            })
            .expect_err("past the end");
        assert_eq!(past.code, "invalid");
        let gone = doc
            .apply(AnimOp::RemoveEvent {
                name: "fade".into(),
                index: 5,
            })
            .expect_err("no event 5");
        assert_eq!(gone.code, "unknown_event");
        assert_eq!(doc.revision(), revision, "refused edits change nothing");
    }

    /// w13 (P-223) pointer path: a box dragged over empty space selects a key on each of two
    /// tracks, and dragging one of them moves both in one undo step.
    #[test]
    fn a_box_selects_across_tracks_and_a_drag_moves_them_in_one_step() {
        use crate::canvas_keys::test_input::raw;
        let mut doc = doc(true);
        doc.apply(AnimOp::SetKey {
            name: "fade".into(),
            property: "translate_x".into(),
            key: key(0.5, 10.0),
        })
        .expect("second track");
        let mut state = UiAnimPanelState::default();
        let ctx = egui::Context::default();
        let run = |doc: &mut UiAnimDoc, state: &mut UiAnimPanelState, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(raw([900.0, 700.0], events), |ui| {
                let def = doc.library().animations["fade"].clone();
                dope_sheet(ui, doc, state, "fade", &def, 0.0).expect("no refused edit");
            });
            out.textures_delta.clear();
        };
        run(&mut doc, &mut state, vec![]);
        let rect = state.dope_rect.expect("drawn");
        // The sheet's axis: the animation ends at 0.5 s.
        let left = rect.left() + DOPE_NAME_WIDTH;
        let scale = (rect.right() - left - 8.0).max(40.0) / 0.5;
        let at = |t: f32, row: f32| egui::pos2(left + t * scale, rect.top() + 3.0 + (row + 0.5) * DOPE_ROW);
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let gesture =
            |doc: &mut UiAnimDoc, state: &mut UiAnimPanelState, from: egui::Pos2, to: egui::Pos2| {
                let mid = from + (to - from) * 0.5;
                for events in [
                    vec![egui::Event::PointerMoved(from)],
                    vec![press(from, true)],
                    vec![egui::Event::PointerMoved(mid)],
                    vec![egui::Event::PointerMoved(to)],
                    vec![press(to, false)],
                    vec![],
                ] {
                    run(doc, state, events);
                }
            };
        // A box from the middle of the first row to past the second row's 0.5 s keys.
        gesture(
            &mut doc,
            &mut state,
            at(0.25, -0.4),
            at(0.5, 1.0) + egui::vec2(6.0, 6.0),
        );
        assert_eq!(
            state.dope,
            [
                DopeKey {
                    property: "opacity".into(),
                    time: 0.5
                },
                DopeKey {
                    property: "translate_x".into(),
                    time: 0.5
                },
            ],
            "the box selected the 0.5 s key of both tracks"
        );
        // Drag the first row's selected key 0.25 s left: both move, one undo step.
        let depth = doc.undo_len();
        gesture(&mut doc, &mut state, at(0.5, 0.0), at(0.25, 0.0));
        assert_eq!(doc.undo_len(), depth + 1, "one undo step for the drag");
        let times = |doc: &UiAnimDoc, property: &str| -> Vec<f32> {
            doc.library().animations["fade"]
                .tracks
                .iter()
                .find(|t| t.property == property)
                .map(|t| t.keys.iter().map(|k| k.time).collect())
                .unwrap_or_default()
        };
        assert_eq!(times(&doc, "opacity"), [0.0, 0.25]);
        assert_eq!(times(&doc, "translate_x"), [0.25]);
    }
}
