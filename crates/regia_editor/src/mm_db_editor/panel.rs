// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor window: file row (Open, Save, Undo, Redo, Rebuild, Cancel), the
//! schema form (typed widgets, values clamped to the runtime validator's ranges), the tag table,
//! the clip list with per-clip flags and tag chips, the validator's errors and the editor's checks,
//! and the last rebuild's warnings and stats. Every control edits through
//! [`super::edit`] with an [`MmDbOp`], so it does what `editor.mm_db_edit` does.
//!
//! Part B (w11-motiondb-editor-b) adds, in [`entry_extras`] under each clip's flags, the span
//! timeline ([`super::timeline`]: drag ranges, keyboard alternatives) and the pose preview of the
//! frame under the cursor ([`super::preview`]); the problem and warning rows select the entry and
//! frame they name; **Live** rebuilds after edits ([`super::live`]).

use std::collections::BTreeMap;
use std::path::PathBuf;

use regia_animation::motion_matching::{
    ContactSettings, FeatureGroup, MAX_BIAS, MAX_GROUP_WEIGHT, MAX_SAMPLE_WEIGHT, MotionDatabaseSource,
    NormalizationSource, SpanKindSource,
};
use regia_ecs::prelude::*;

use super::model::{issues, lints, present_groups};
use super::ops::{MmDbOp, weight_of};
use super::{MmDbEditor, MmDbRebuildJob, RebuildView};

/// Window state: text fields and the clip picker's choices.
#[derive(Debug, Default)]
pub struct PanelState {
    /// The window is shown.
    pub window_open: bool,
    /// The Open field.
    pub path_field: String,
    /// The new tag field.
    pub new_tag: String,
    /// The rename field per tag.
    pub rename: BTreeMap<String, String>,
    /// The new chip field per entry.
    pub chip: BTreeMap<usize, String>,
    /// The new clip field.
    pub new_clip: String,
    /// The new pose joint field.
    pub new_joint: String,
    /// The new mirror pair fields.
    pub new_pair: (String, String),
    /// Clip paths found in the project (`<file>#clip/<name>`), refreshed on demand.
    pub clip_choices: Option<Vec<String>>,
    /// Text fields of list values (feet, times), by field name, while typed.
    pub lists: BTreeMap<&'static str, String>,
    /// The span timeline and the scrub cursor (part B).
    pub timeline: super::timeline::TimelineState,
}

impl PanelState {
    /// Clears the per-document fields (a new document opened).
    pub fn reset(&mut self) {
        let open = self.window_open;
        let path = std::mem::take(&mut self.path_field);
        let choices = self.clip_choices.take();
        *self = Self {
            window_open: open,
            path_field: path,
            clip_choices: choices,
            ..Self::default()
        };
    }
}

/// The `<file>#clip/<name>` sub-asset paths of every glTF in the project (read from the `.meta`
/// files the glTF imports wrote).
#[must_use]
pub fn project_clip_paths(world: &mut World) -> Vec<String> {
    let Some(root) = crate::matgraph_editor::project_root(world) else {
        return Vec::new();
    };
    let assets = {
        let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
        index.ensure(Some(&root));
        index.assets()
    };
    let mut out = Vec::new();
    for (path, _) in assets.iter() {
        let lower = path.to_ascii_lowercase();
        if !(lower.ends_with(".glb") || lower.ends_with(".gltf")) {
            continue;
        }
        let file = [root.join(path), root.join("assets").join(path)]
            .into_iter()
            .find(|p| p.is_file());
        let Some(file) = file else { continue };
        let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(&file)) else {
            continue;
        };
        let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes) else {
            continue;
        };
        for sub in meta.sub_assets.iter().filter(|s| s.path.starts_with("clip/")) {
            out.push(format!("{path}#{}", sub.path));
        }
    }
    out.sort();
    out
}

fn apply(world: &mut World, label: &str, merge_key: Option<String>, op: MmDbOp) {
    if let Err(e) = super::edit(world, label, merge_key, &[op]) {
        world.get_resource_or_insert_with(MmDbEditor::default).toast = format!("{} ({})", e.message, e.code);
    }
}

fn seal(world: &mut World) {
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.seal();
    }
}

fn parse_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_times(text: &str) -> Option<Vec<f32>> {
    parse_list(text).iter().map(|s| s.parse::<f32>().ok()).collect()
}

fn join_times(times: &[f32]) -> String {
    times
        .iter()
        .map(|t| format!("{t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A text field for a comma-separated list: committed on Enter or focus loss.
fn list_field(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    key: &'static str,
    current: String,
) -> Option<String> {
    let text = state.lists.entry(key).or_insert_with(|| current.clone());
    let response = ui.text_edit_singleline(text);
    if response.lost_focus() {
        let value = std::mem::take(text);
        state.lists.remove(key);
        (value != current).then_some(value)
    } else {
        if !response.has_focus() && *text != current {
            *text = current;
        }
        None
    }
}

/// Draws the window when it is open (the editor shell calls this every frame, over the dock).
pub fn window(ctx: &egui::Context, world: &mut World) {
    super::tick_job(world);
    let open = world
        .get_resource::<MmDbEditor>()
        .is_some_and(|e| e.ui.window_open);
    if !open {
        return;
    }
    super::live::live_tick(world, std::time::Instant::now());
    let mut still_open = true;
    egui::Window::new("Motion database")
        .id(egui::Id::new("mm_db_editor_window"))
        .open(&mut still_open)
        .default_width(520.0)
        .vscroll(true)
        .show(ctx, |ui| contents(ui, world));
    if !still_open && let Some(mut editor) = world.get_resource_mut::<MmDbEditor>() {
        editor.ui.window_open = false;
    }
}

/// The window's contents (also usable inside another panel).
pub fn contents(ui: &mut egui::Ui, world: &mut World) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    file_row(ui, world);
    let Some(editor) = world.get_resource::<MmDbEditor>() else {
        return;
    };
    if !editor.toast.is_empty() {
        ui.colored_label(tokens.muted, editor.toast.clone());
    }
    if editor.comments_warning {
        ui.colored_label(tokens.accents.alert, super::COMMENTS_WARNING);
    }
    let Some(doc) = editor.doc.clone() else {
        ui.weak("No motion database open: type a .motiondb.ron path and Open.");
        return;
    };
    let errors = issues(&doc);
    let checks = lints(&doc);
    if !errors.is_empty() || !checks.is_empty() {
        egui::CollapsingHeader::new(format!(
            "Problems ({} errors, {} checks)",
            errors.len(),
            checks.len()
        ))
        .id_salt("mm_db_problems")
        .default_open(true)
        .show(ui, |ui| {
            for issue in errors.iter().chain(&checks) {
                let color = if issue.severity == "error" {
                    tokens.accents.alert
                } else {
                    tokens.muted
                };
                let text = egui::RichText::new(format!("{}: {}", issue.code, issue.message)).color(color);
                let row = ui.selectable_label(false, text);
                if let Some(entry) = issue.entry
                    && row.on_hover_text("show this entry").clicked()
                {
                    let frame = issue
                        .span
                        .and_then(|s| doc.entries.get(entry)?.spans.get(s))
                        .map_or(0, |s| super::ops::time_frame(doc.schema.sample_rate, s.range.0));
                    super::timeline::select_frame(world, entry, frame, false);
                }
            }
        });
    }
    egui::CollapsingHeader::new("Schema")
        .id_salt("mm_db_schema")
        .default_open(true)
        .show(ui, |ui| schema_form(ui, world, &doc));
    egui::CollapsingHeader::new(format!("Tags ({})", doc.tags.len()))
        .id_salt("mm_db_tags")
        .default_open(true)
        .show(ui, |ui| tag_table(ui, world, &doc));
    egui::CollapsingHeader::new(format!("Clips ({})", doc.entries.len()))
        .id_salt("mm_db_clips")
        .default_open(true)
        .show(ui, |ui| clip_list(ui, world, &doc));
    egui::CollapsingHeader::new("Rebuild")
        .id_salt("mm_db_rebuild")
        .default_open(true)
        .show(ui, |ui| rebuild_view(ui, world));
}

fn file_row(ui: &mut egui::Ui, world: &mut World) {
    let dirty = world.get_resource_or_insert_with(MmDbEditor::default).is_dirty();
    let running = world.contains_resource::<MmDbRebuildJob>();
    let is_open = world
        .get_resource::<MmDbEditor>()
        .is_some_and(MmDbEditor::is_open);
    ui.horizontal(|ui| {
        let mut path = world
            .get_resource::<MmDbEditor>()
            .map(|e| e.ui.path_field.clone())
            .unwrap_or_default();
        ui.add(
            egui::TextEdit::singleline(&mut path)
                .hint_text("assets/anim/hero.motiondb.ron")
                .desired_width(220.0),
        );
        if let Some(mut editor) = world.get_resource_mut::<MmDbEditor>() {
            editor.ui.path_field = path.clone();
        }
        if ui.button("Open").clicked() && !path.is_empty() {
            let file = super::asset_file(world, &path);
            if let Err(e) = super::open(world, &file, false) {
                world.get_resource_or_insert_with(MmDbEditor::default).toast =
                    format!("{} ({})", e.message, e.code);
            }
        }
        if ui
            .add_enabled(is_open && dirty, egui::Button::new("Save"))
            .clicked()
            && let Err(e) = super::save(world, None)
        {
            world.get_resource_or_insert_with(MmDbEditor::default).toast =
                format!("{} ({})", e.message, e.code);
        }
        let (can_undo, can_redo) = world
            .get_resource::<crate::command::UndoStack>()
            .map_or((false, false), |s| (s.undo_depth() > 0, s.redo_depth() > 0));
        if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
            let _ = crate::command::undo_world(world);
        }
        if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
            let _ = crate::command::redo_world(world);
        }
        if ui
            .add_enabled(is_open && !running, egui::Button::new("Rebuild"))
            .clicked()
            && let Err(e) = super::start_rebuild(world)
        {
            world.get_resource_or_insert_with(MmDbEditor::default).toast =
                format!("{} ({})", e.message, e.code);
        }
        if running {
            ui.spinner();
            if ui.button("Cancel").clicked() {
                let _ = super::cancel_rebuild(world);
            }
        }
        let mut live = world
            .get_resource_or_insert_with(super::live::MmDbLive::default)
            .enabled;
        if ui
            .checkbox(&mut live, "Live")
            .on_hover_text(
                "rebuild in the background after edits settle, so warnings and stats follow the document",
            )
            .changed()
        {
            world
                .get_resource_or_insert_with(super::live::MmDbLive::default)
                .enabled = live;
        }
        if dirty {
            ui.label("●").on_hover_text("unsaved changes");
        }
    });
    if let Some(error) = world
        .get_resource::<super::live::MmDbLive>()
        .filter(|l| l.enabled)
        .and_then(|l| l.last_error.clone())
    {
        ui.weak(format!("Live rebuild off: {error}"));
    }
}

fn schema_form(ui: &mut egui::Ui, world: &mut World, doc: &MotionDatabaseSource) {
    let s = &doc.schema;
    let mut state = std::mem::take(&mut world.get_resource_or_insert_with(MmDbEditor::default).ui);
    egui::Grid::new("mm_db_schema_grid")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Skeleton");
            if let Some(v) = list_field(ui, &mut state, "skeleton", doc.skeleton.clone()) {
                apply(world, "skeleton", None, MmDbOp::SetSkeleton { path: v });
            }
            ui.end_row();
            ui.label("Root joint");
            if let Some(v) = list_field(ui, &mut state, "root_joint", s.root_joint.clone()) {
                apply(world, "root joint", None, MmDbOp::SetRootJoint { joint: v });
            }
            ui.end_row();
            ui.label("Forward");
            ui.horizontal(|ui| {
                let mut axis = s.forward;
                let mut changed = false;
                for v in &mut axis {
                    changed |= ui
                        .add(egui::DragValue::new(v).speed(0.01).range(-1.0..=1.0))
                        .changed();
                }
                if changed {
                    apply(
                        world,
                        "forward",
                        Some("forward".into()),
                        MmDbOp::SetForward { axis },
                    );
                }
            });
            ui.end_row();
            ui.label("Feet");
            if let Some(v) = list_field(ui, &mut state, "feet", s.feet.join(", ")) {
                apply(world, "feet", None, MmDbOp::SetFeet { feet: parse_list(&v) });
            }
            ui.end_row();
            ui.label("Trajectory times (s)");
            if let Some(v) = list_field(
                ui,
                &mut state,
                "trajectory_times",
                join_times(&s.trajectory_times),
            ) {
                match parse_times(&v) {
                    Some(times) => apply(
                        world,
                        "trajectory times",
                        None,
                        MmDbOp::SetTrajectoryTimes { times },
                    ),
                    None => {
                        world.get_resource_or_insert_with(MmDbEditor::default).toast =
                            "trajectory times must be numbers separated by commas".into()
                    }
                }
            }
            ui.end_row();
            ui.label("History times (s)");
            if let Some(v) = list_field(ui, &mut state, "history_times", join_times(&s.history_times)) {
                match parse_times(&v) {
                    Some(times) => apply(world, "history times", None, MmDbOp::SetHistoryTimes { times }),
                    None => {
                        world.get_resource_or_insert_with(MmDbEditor::default).toast =
                            "history times must be numbers separated by commas".into()
                    }
                }
            }
            ui.end_row();
            ui.label("Sample rate (fps)");
            let mut rate = s.sample_rate;
            let response = ui.add(egui::DragValue::new(&mut rate).range(1.0..=240.0).speed(0.5));
            if response.changed() {
                apply(
                    world,
                    "sample rate",
                    Some("sample_rate".into()),
                    MmDbOp::SetSampleRate { rate },
                );
            }
            if response.drag_stopped() {
                seal(world);
            }
            ui.end_row();
            ui.label("Normalization");
            ui.horizontal(|ui| {
                for (mode, name) in [
                    (NormalizationSource::GroupDeviation, "group deviation"),
                    (NormalizationSource::PerDimension, "per dimension"),
                    (NormalizationSource::None, "none"),
                ] {
                    if ui.selectable_label(s.normalization == mode, name).clicked() && s.normalization != mode
                    {
                        apply(world, "normalization", None, MmDbOp::SetNormalization { mode });
                    }
                }
            });
            ui.end_row();
        });
    ui.label("Group weights");
    egui::Grid::new("mm_db_weights").num_columns(2).show(ui, |ui| {
        let present = present_groups(doc);
        for group in FeatureGroup::ALL {
            ui.add_enabled_ui(present.contains(&group), |ui| ui.label(group.name()));
            let mut value = weight_of(&s.weights, group);
            let response = ui.add(
                egui::DragValue::new(&mut value)
                    .range(0.0..=MAX_GROUP_WEIGHT)
                    .speed(0.05),
            );
            if response.changed() {
                apply(
                    world,
                    "weight",
                    Some(format!("weight:{}", group.name())),
                    MmDbOp::SetWeight {
                        group: group.name().to_string(),
                        value,
                    },
                );
            }
            if response.drag_stopped() {
                seal(world);
            }
            ui.end_row();
        }
    });
    for (list, times, weights) in [
        ("trajectory", &s.trajectory_times, &s.sample_weights.trajectory),
        ("history", &s.history_times, &s.sample_weights.history),
    ] {
        if times.is_empty() {
            continue;
        }
        ui.horizontal(|ui| {
            ui.label(format!("{list} sample weights"));
            for (index, time) in times.iter().enumerate() {
                let mut value = weights.get(index).copied().unwrap_or(1.0);
                let response = ui
                    .add(
                        egui::DragValue::new(&mut value)
                            .range(0.0..=MAX_SAMPLE_WEIGHT)
                            .speed(0.05),
                    )
                    .on_hover_text(format!("{time} s"));
                if response.changed() {
                    apply(
                        world,
                        "sample weight",
                        Some(format!("sample:{list}:{index}")),
                        MmDbOp::SetSampleWeight {
                            list: list.to_string(),
                            index,
                            value,
                        },
                    );
                }
                if response.drag_stopped() {
                    seal(world);
                }
            }
        });
    }
    ui.label("Pose joints");
    for (index, joint) in s.pose_joints.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(&joint.joint);
            let mut position = joint.position;
            if ui.checkbox(&mut position, "position").changed() {
                apply(
                    world,
                    "pose joint",
                    None,
                    MmDbOp::SetPoseJoint {
                        index,
                        position: Some(position),
                        velocity: None,
                        weight: None,
                    },
                );
            }
            let mut velocity = joint.velocity;
            if ui.checkbox(&mut velocity, "velocity").changed() {
                apply(
                    world,
                    "pose joint",
                    None,
                    MmDbOp::SetPoseJoint {
                        index,
                        position: None,
                        velocity: Some(velocity),
                        weight: None,
                    },
                );
            }
            let mut weight = joint.weight;
            let response = ui.add(
                egui::DragValue::new(&mut weight)
                    .range(0.0..=MAX_SAMPLE_WEIGHT)
                    .speed(0.05),
            );
            if response.changed() {
                apply(
                    world,
                    "pose joint weight",
                    Some(format!("pose:{index}")),
                    MmDbOp::SetPoseJoint {
                        index,
                        position: None,
                        velocity: None,
                        weight: Some(weight),
                    },
                );
            }
            if response.drag_stopped() {
                seal(world);
            }
            if ui.small_button("Remove").clicked() {
                apply(
                    world,
                    "remove pose joint",
                    None,
                    MmDbOp::RemovePoseJoint { index },
                );
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_joint)
                .hint_text("joint name")
                .desired_width(140.0),
        );
        if ui.button("Add pose joint").clicked() && !state.new_joint.trim().is_empty() {
            let joint = std::mem::take(&mut state.new_joint);
            apply(world, "add pose joint", None, MmDbOp::AddPoseJoint { joint });
        }
    });
    ui.horizontal(|ui| {
        let mut on = s.contacts.is_some();
        if ui.checkbox(&mut on, "Contact labels").changed() {
            let contacts = on.then(ContactSettings::default);
            apply(world, "contacts", None, MmDbOp::SetContacts { contacts });
        }
        if let Some(c) = s.contacts {
            let mut next = c;
            let mut changed = false;
            for (label, value) in [
                ("height on", &mut next.height_on),
                ("height off", &mut next.height_off),
                ("speed on", &mut next.speed_on),
                ("speed off", &mut next.speed_off),
            ] {
                changed |= ui
                    .add(
                        egui::DragValue::new(value)
                            .range(0.0..=10.0)
                            .speed(0.005)
                            .prefix(format!("{label} ")),
                    )
                    .changed();
            }
            changed |= ui.checkbox(&mut next.phase, "phase").changed();
            if changed {
                apply(
                    world,
                    "contacts",
                    Some("contacts".into()),
                    MmDbOp::SetContacts { contacts: Some(next) },
                );
            }
        }
    });
    ui.label("Mirror pairs");
    for (index, (left, right)) in doc.mirror_pairs.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(format!("{left} ↔ {right}"));
            if ui.small_button("Remove").clicked() {
                apply(
                    world,
                    "remove mirror pair",
                    None,
                    MmDbOp::RemoveMirrorPair { index },
                );
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_pair.0)
                .hint_text("left")
                .desired_width(100.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.new_pair.1)
                .hint_text("right")
                .desired_width(100.0),
        );
        if ui.button("Add pair").clicked() {
            let (left, right) = std::mem::take(&mut state.new_pair);
            apply(
                world,
                "add mirror pair",
                None,
                MmDbOp::AddMirrorPair { left, right },
            );
        }
    });
    world.get_resource_or_insert_with(MmDbEditor::default).ui = state;
}

fn tag_table(ui: &mut egui::Ui, world: &mut World, doc: &MotionDatabaseSource) {
    let mut state = std::mem::take(&mut world.get_resource_or_insert_with(MmDbEditor::default).ui);
    for name in &doc.tags {
        ui.horizontal(|ui| {
            ui.label(name);
            let field = state.rename.entry(name.clone()).or_insert_with(|| name.clone());
            ui.add(egui::TextEdit::singleline(field).desired_width(120.0));
            if ui.small_button("Rename").clicked() && field != name {
                let to = field.clone();
                apply(
                    world,
                    "rename tag",
                    None,
                    MmDbOp::RenameTag {
                        name: name.clone(),
                        to,
                    },
                );
                state.rename.clear();
            }
            if ui.small_button("Remove").clicked() {
                apply(
                    world,
                    "remove tag",
                    None,
                    MmDbOp::RemoveTag { name: name.clone() },
                );
                state.rename.clear();
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_tag)
                .hint_text("new tag")
                .desired_width(120.0),
        );
        if ui.button("Add tag").clicked() && !state.new_tag.trim().is_empty() {
            let name = std::mem::take(&mut state.new_tag);
            apply(world, "add tag", None, MmDbOp::AddTag { name });
        }
    });
    world.get_resource_or_insert_with(MmDbEditor::default).ui = state;
}

fn clip_list(ui: &mut egui::Ui, world: &mut World, doc: &MotionDatabaseSource) {
    let mut state = std::mem::take(&mut world.get_resource_or_insert_with(MmDbEditor::default).ui);
    let count = doc.entries.len();
    for (index, entry) in doc.entries.iter().enumerate() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(format!("{index}"));
                ui.label(&entry.clip);
                if ui
                    .add_enabled(index > 0, egui::Button::new("Up").small())
                    .clicked()
                {
                    apply(
                        world,
                        "move clip",
                        None,
                        MmDbOp::MoveClip {
                            from: index,
                            to: index - 1,
                        },
                    );
                }
                if ui
                    .add_enabled(index + 1 < count, egui::Button::new("Down").small())
                    .clicked()
                {
                    apply(
                        world,
                        "move clip",
                        None,
                        MmDbOp::MoveClip {
                            from: index,
                            to: index + 1,
                        },
                    );
                }
                if ui.small_button("Remove").clicked() {
                    apply(world, "remove clip", None, MmDbOp::RemoveClip { index });
                }
            });
            ui.horizontal(|ui| {
                let flag = |looping, mirrored, distance_matched| MmDbOp::SetClip {
                    index,
                    clip: None,
                    looping,
                    mirrored,
                    distance_matched,
                    trim: None,
                };
                let mut looping = entry.looping;
                if ui.checkbox(&mut looping, "looping").changed() {
                    apply(world, "looping", None, flag(Some(looping), None, None));
                }
                let mut mirrored = entry.mirrored;
                if ui.checkbox(&mut mirrored, "mirrored").changed() {
                    apply(world, "mirrored", None, flag(None, Some(mirrored), None));
                }
                let mut distance = entry.distance_matched;
                if ui.checkbox(&mut distance, "distance matched").changed() {
                    apply(world, "distance matched", None, flag(None, None, Some(distance)));
                }
                let mut trim = entry.trim;
                let a = ui.add(
                    egui::DragValue::new(&mut trim.0)
                        .range(0.0..=60.0)
                        .speed(0.01)
                        .prefix("trim start "),
                );
                let b = ui.add(
                    egui::DragValue::new(&mut trim.1)
                        .range(0.0..=60.0)
                        .speed(0.01)
                        .prefix("trim end "),
                );
                if a.changed() || b.changed() {
                    apply(
                        world,
                        "trim",
                        Some(format!("trim:{index}")),
                        MmDbOp::SetClip {
                            index,
                            clip: None,
                            looping: None,
                            mirrored: None,
                            distance_matched: None,
                            trim: Some(trim),
                        },
                    );
                }
                if a.drag_stopped() || b.drag_stopped() {
                    seal(world);
                }
            });
            ui.horizontal_wrapped(|ui| {
                for tag in &entry.tags {
                    if ui
                        .small_button(format!("{tag} ×"))
                        .on_hover_text("remove this tag from the clip")
                        .clicked()
                    {
                        apply(
                            world,
                            "remove clip tag",
                            None,
                            MmDbOp::RemoveClipTag {
                                index,
                                tag: tag.clone(),
                            },
                        );
                    }
                }
                let field = state.chip.entry(index).or_default();
                let response = ui.add(
                    egui::TextEdit::singleline(field)
                        .hint_text("+ tag")
                        .desired_width(80.0),
                );
                if response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !field.trim().is_empty()
                {
                    let tag = std::mem::take(field);
                    apply(world, "add clip tag", None, MmDbOp::AddClipTag { index, tag });
                }
            });
            entry_extras(ui, world, &mut state, index);
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_clip)
                .hint_text("file.glb#clip/name")
                .desired_width(200.0),
        );
        let choices = state.clip_choices.clone().unwrap_or_default();
        egui::ComboBox::from_id_salt("mm_db_clip_picker")
            .selected_text("Pick")
            .show_ui(ui, |ui| {
                if choices.is_empty() {
                    ui.weak("No clips found (import a glTF with animations, or Refresh)");
                }
                for choice in &choices {
                    if ui
                        .selectable_label(state.new_clip == *choice, choice.as_str())
                        .clicked()
                    {
                        state.new_clip = choice.clone();
                    }
                }
            });
        if ui.small_button("Refresh").clicked() || state.clip_choices.is_none() {
            state.clip_choices = Some(project_clip_paths(world));
        }
        if ui.button("Add clip").clicked() && !state.new_clip.trim().is_empty() {
            let clip = std::mem::take(&mut state.new_clip);
            apply(world, "add clip", None, MmDbOp::AddClip { clip, index: None });
        }
    });
    world.get_resource_or_insert_with(MmDbEditor::default).ui = state;
}

/// Per-entry rows under a clip's flags (part B): the span timeline, the span controls (the kind
/// new spans get, add at the cursor, the selected span's frames, bias and Delete) and, when the
/// scrub cursor is on this entry, the pose preview with the frame's feature vector.
pub fn entry_extras(ui: &mut egui::Ui, world: &mut World, state: &mut PanelState, index: usize) {
    let spans = world
        .get_resource::<MmDbEditor>()
        .and_then(|e| e.doc.as_ref()?.entries.get(index).map(|e| e.spans.len()))
        .unwrap_or(0);
    egui::CollapsingHeader::new(format!("Spans ({spans})"))
        .id_salt(("mm_db_spans", index))
        .default_open(true)
        .show(ui, |ui| {
            super::timeline::timeline_ui(ui, world, &mut state.timeline, index);
            span_controls(ui, world, &mut state.timeline, index);
        });
    if state.timeline.cursor.is_some_and(|c| c.entry == index) {
        preview_section(ui, world, &mut state.timeline, index);
    }
}

fn span_controls(ui: &mut egui::Ui, world: &mut World, t: &mut super::timeline::TimelineState, index: usize) {
    use super::timeline::{NewKind, Part, apply_span_op, move_op, new_kind_source, span_frames};
    let Some(doc) = world.get_resource::<MmDbEditor>().and_then(|e| e.doc.clone()) else {
        return;
    };
    let Some(entry) = doc.entries.get(index) else {
        return;
    };
    let frames = t.geometry.get(&index).map_or(1, |g| g.frames);
    ui.horizontal_wrapped(|ui| {
        ui.label("New span:");
        for kind in NewKind::ALL {
            if ui.selectable_label(t.new_kind == kind, kind.name()).clicked() {
                t.new_kind = kind;
            }
        }
        match t.new_kind {
            NewKind::LandingBias => {
                ui.add(
                    egui::DragValue::new(&mut t.new_bias)
                        .range(-MAX_BIAS..=MAX_BIAS)
                        .speed(0.05)
                        .prefix("bias "),
                );
            }
            NewKind::Tag => {
                ui.add(
                    egui::TextEdit::singleline(&mut t.new_tag)
                        .hint_text("tag")
                        .desired_width(80.0),
                );
            }
            NewKind::Unindexed | NewKind::PassThrough => {}
        }
        let at = t
            .cursor
            .filter(|c| c.entry == index)
            .map_or(0, |c| c.frame)
            .min(frames.saturating_sub(1));
        if ui
            .button("Add at cursor")
            .on_hover_text("a one-frame span of the chosen kind at the cursor (N)")
            .clicked()
        {
            let kind = new_kind_source(t.new_kind, t.new_bias, &t.new_tag);
            apply_span_op(
                world,
                t,
                "add span",
                None,
                MmDbOp::AddSpan {
                    index,
                    kind,
                    range: None,
                    frames: Some((at, at + 1)),
                },
            );
        }
    });
    let Some(sel) = t
        .selected
        .filter(|s| s.entry == index && s.span < entry.spans.len())
    else {
        return;
    };
    let span = &entry.spans[sel.span];
    let rate = doc.schema.sample_rate;
    let (a, b) = span_frames(rate, span);
    ui.horizontal_wrapped(|ui| {
        ui.label(format!(
            "Span {}: {} [{:.3}, {:.3}) s",
            sel.span,
            super::timeline::kind_name(&span.kind),
            span.range.0,
            span.range.1
        ));
        let merge = Some(format!("span:{index}:{}", sel.span));
        for (part, value, label) in [(Part::Start, a, "start frame "), (Part::End, b, "end frame ")] {
            let mut v = value;
            let response = ui.add(
                egui::DragValue::new(&mut v)
                    .range(0..=frames.max(b))
                    .prefix(label),
            );
            if response.changed()
                && let Some(op) = move_op(
                    &doc,
                    index,
                    sel.span,
                    part,
                    i64::from(v) - i64::from(value),
                    frames,
                )
            {
                apply_span_op(world, t, "move span", merge.clone(), op);
            }
            if response.drag_stopped() {
                seal(world);
            }
        }
        if let SpanKindSource::LandingBias(bias) = span.kind {
            let mut v = bias;
            let response = ui.add(
                egui::DragValue::new(&mut v)
                    .range(-MAX_BIAS..=MAX_BIAS)
                    .speed(0.05)
                    .prefix("bias "),
            );
            if response.changed() {
                apply_span_op(
                    world,
                    t,
                    "span bias",
                    merge.clone(),
                    MmDbOp::SetSpan {
                        index,
                        span: sel.span,
                        kind: Some(SpanKindSource::LandingBias(v)),
                        range: None,
                        frames: None,
                    },
                );
            }
            if response.drag_stopped() {
                seal(world);
            }
        }
        if ui.button("Delete span").clicked() {
            apply_span_op(
                world,
                t,
                "delete span",
                None,
                MmDbOp::RemoveSpan {
                    index,
                    span: sel.span,
                },
            );
        }
    });
}

fn preview_section(
    ui: &mut egui::Ui,
    world: &mut World,
    t: &mut super::timeline::TimelineState,
    index: usize,
) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let Some(cursor) = t.cursor.filter(|c| c.entry == index) else {
        return;
    };
    let mirrored_entry = world
        .get_resource::<MmDbEditor>()
        .and_then(|e| e.doc.as_ref()?.entries.get(index).map(|e| e.mirrored))
        .unwrap_or(false);
    let view = super::preview::frame_view(world, index, cursor.frame as usize, cursor.mirrored);
    let skeleton = super::preview::entry_preview(world, index)
        .ok()
        .map(|p| p.skeleton);
    let view = match (view, skeleton) {
        (Ok(view), Some(skeleton)) => (view, skeleton),
        (Err(e), _) => {
            ui.colored_label(tokens.accents.alert, format!("Preview: {}", e.message))
                .on_hover_text(format!("Agent error code: {}", e.code));
            return;
        }
        (Ok(_), None) => return,
    };
    let (view, skeleton) = view;
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!(
            "Frame {} of {} · {:.3} s",
            view.frame, view.frames, view.time
        ));
        if mirrored_entry {
            let mut mirrored = cursor.mirrored;
            if ui
                .checkbox(&mut mirrored, "mirrored copy")
                .on_hover_text("the mirrored copy's features (the pose is drawn unmirrored)")
                .changed()
            {
                t.cursor = Some(super::timeline::Cursor { mirrored, ..cursor });
            }
        }
        match view.meta {
            Some(meta) => {
                ui.label(format!(
                    "{} · bias {:+} · tags [{}]",
                    if meta.is_target() {
                        "jump target"
                    } else {
                        "not a target"
                    },
                    meta.bias,
                    view.tag_names.join(", ")
                ));
            }
            None => {
                ui.weak("spans not applied (see Problems)");
            }
        }
    });
    let globals = super::preview::globals(&skeleton, &view.pose);
    ui.horizontal_top(|ui| {
        ui.allocate_ui(egui::vec2(180.0, 150.0), |ui| {
            crate::anim::preview::preview_widget(
                ui,
                &format!("mm-db-preview-{index}"),
                &skeleton,
                &globals,
                &mut t.orbit,
                None,
                None,
            );
        });
        ui.vertical(|ui| {
            ui.weak("Raw feature vector (the runtime builder's row for this frame)");
            egui::Grid::new(("mm_db_features", index))
                .striped(true)
                .show(ui, |ui| {
                    for group in &view.groups {
                        ui.label(group.group);
                        ui.monospace(
                            group
                                .values
                                .iter()
                                .map(|v| format!("{v:.3}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        ui.end_row();
                    }
                });
        });
    });
}

fn rebuild_view(ui: &mut egui::Ui, world: &mut World) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    if let Some(job) = world.get_resource::<MmDbRebuildJob>() {
        let cx = job.job.context();
        ui.label(format!("running {}: {}", job.job.id, cx.message()));
        return;
    }
    let Some((rebuild, current)) = world
        .get_resource::<MmDbEditor>()
        .map(|e| (e.rebuild.clone(), e.revision))
    else {
        return;
    };
    match &rebuild {
        None => {
            ui.weak("Not rebuilt yet: Rebuild builds the database in memory (nothing is written).");
        }
        Some(RebuildView::Cancelled) => {
            ui.weak("Cancelled.");
        }
        Some(RebuildView::Failed(e)) => {
            ui.colored_label(tokens.accents.alert, crate::data_panels::sentence(&e.message))
                .on_hover_text(format!("Agent error code: {}", e.code));
        }
        Some(RebuildView::Done { report, revision }) => {
            if *revision != current {
                ui.colored_label(tokens.muted, "The document changed since this rebuild.");
            }
            ui.label(format!(
                "{} poses · {} dims · {} ranges · {} targets · {} untagged · tree {} nodes, depth {} · hash {:016x}",
                report.frames,
                report.dimensions,
                report.ranges,
                report.targets,
                report.untagged_frames,
                report.kd.0,
                report.kd.2,
                report.content_hash
            ));
            if report.warnings.is_empty() {
                ui.label("No build warnings.");
            }
            let mut pick = None;
            for w in &report.warnings {
                let text =
                    egui::RichText::new(format!("{}: clip {:?} (entry {})", w.kind, w.clip_name, w.entry))
                        .color(tokens.accents.alert);
                if ui
                    .selectable_label(false, text)
                    .on_hover_text("show this clip")
                    .clicked()
                {
                    pick = Some((w.entry, w.clip_name.ends_with(".mirrored")));
                }
            }
            if let Some((entry, mirrored)) = pick {
                super::timeline::select_frame(world, entry, 0, mirrored);
                return;
            }
            egui::Grid::new("mm_db_coverage").striped(true).show(ui, |ui| {
                for head in ["entry", "clip", "frames", "targets"] {
                    ui.strong(head);
                }
                ui.end_row();
                for c in &report.coverage {
                    ui.label(c.entry.to_string());
                    ui.label(if c.mirrored {
                        format!("{} (mirrored)", c.clip)
                    } else {
                        c.clip.clone()
                    });
                    ui.label(c.frames.to_string());
                    ui.label(c.targets.to_string());
                    ui.end_row();
                }
            });
            egui::Grid::new("mm_db_ranges").striped(true).show(ui, |ui| {
                for head in ["group", "weight", "dims", "min", "max"] {
                    ui.strong(head);
                }
                ui.end_row();
                for g in &report.groups {
                    ui.label(g.group);
                    ui.label(format!("{}", g.weight));
                    ui.label(g.dims.to_string());
                    ui.label(format!("{:.3}", g.min));
                    ui.label(format!("{:.3}", g.max));
                    ui.end_row();
                }
            });
            if !report.tag_frames.is_empty() {
                let tags: Vec<String> = report
                    .tag_frames
                    .iter()
                    .map(|(n, c)| format!("{n} ({c})"))
                    .collect();
                ui.label(format!("frames per tag: {}", tags.join(", ")));
            }
        }
    }
}

/// The Motion Matching panel's "Edit database" target: the file of `asset` (a registry path).
#[must_use]
pub fn edit_target(world: &World, asset: &str) -> PathBuf {
    super::asset_file(world, asset)
}
