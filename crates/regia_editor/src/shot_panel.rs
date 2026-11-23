// SPDX-License-Identifier: MPL-2.0
//! The Shots panel: a minimal curve editor for the loaded shot sequence (`render.sequence`,
//! `book/agents/shot-panel.md`, ADR-1522).
//!
//! It edits one scalar track of one shot at a time (field of view, roll, focus distance or
//! aperture) with the anim curve widget: drag a key to move it, double-click to add one, use the
//! key menu to delete it or change its ease. Every edit goes to the live `ShotPlayer` sequence
//! (`replace_sequence`), so the camera preview follows. The edit logic ([`apply_key_edit`]) is
//! plain data and shared with the agent methods `editor.shot_panel_state` and
//! `editor.shot_panel_edit`, so what an agent does is what the panel shows. The Track list also
//! has the shot camera's `position` path and `rotation` keys (`pose`, ADR-7726): the path drawn
//! from above with its keys, a key list, Add key at playhead and Delete key, each edit one undo
//! entry, through the same key list rules (`edit_key_list`) and the same agent method.
//!
//! The loaded list is a document (`crate::shot_document`, ADR-8730): the path row opens a project
//! `.shots.ron` and saves the list (Save, or Ctrl+S with the pointer over the panel), and
//! `editor.shot_panel_state` reports its `path` and `dirty` flag. A list another tool loads in its
//! place detaches the document (ADR-8940), and **Edit player's shots** (`editor.shot_panel_adopt`)
//! takes a sequence player's own shot list as an unsaved document; the panel then edits that list.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::curve::{Curve, CurveKey, CurveKeyKind};
use regia_ecs::prelude::*;
use regia_framework::sequencer::SequencePlayer;
use regia_framework::shots::{Ease, ScalarKey, Shot, ShotPlayer, ShotSequence, Track};
use serde_json::{Value, json};

use crate::anim::curve_view::{CurveAction, curve_editor};

// The shot camera's position path and rotation keys (ADR-7726).
mod pose;
use crate::anim::view2d::View2d;
use crate::viewport_agent::Handler;
pub use pose::{
    PoseKeyEdit, PoseTrack, PoseValue, ShotListEdit, ShotPosePanel, apply_pose_key_edit, edit_pose,
};

/// A scalar track of a shot the panel edits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShotTrack {
    /// Vertical field of view, degrees.
    #[default]
    Fov,
    /// Roll about the view axis, degrees.
    Roll,
    /// Depth of field: focus distance, metres.
    FocusDistance,
    /// Depth of field: aperture, f-stops.
    Aperture,
}

impl ShotTrack {
    /// Every track, in panel order.
    pub const ALL: [Self; 4] = [Self::Fov, Self::Roll, Self::FocusDistance, Self::Aperture];

    /// The name agents use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Fov => "fov",
            Self::Roll => "roll",
            Self::FocusDistance => "focus_distance",
            Self::Aperture => "aperture",
        }
    }

    /// The track with this name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The value range `ShotSequence::validate` accepts.
    #[must_use]
    pub fn range(self) -> (f32, f32) {
        match self {
            Self::Fov => (1.0, 170.0),
            Self::Roll => (-3600.0, 3600.0),
            Self::FocusDistance => (1.0e-3, 1.0e6),
            Self::Aperture => (0.1, 128.0),
        }
    }

    fn track(self, shot: &Shot) -> Option<&Track> {
        match self {
            Self::Fov => Some(&shot.camera.fov),
            Self::Roll => Some(&shot.camera.roll),
            Self::FocusDistance => shot.camera.dof.as_ref().map(|d| &d.focus_distance),
            Self::Aperture => shot.camera.dof.as_ref().map(|d| &d.aperture),
        }
    }

    fn track_mut(self, shot: &mut Shot) -> Option<&mut Track> {
        match self {
            Self::Fov => Some(&mut shot.camera.fov),
            Self::Roll => Some(&mut shot.camera.roll),
            Self::FocusDistance => shot.camera.dof.as_mut().map(|d| &mut d.focus_distance),
            Self::Aperture => shot.camera.dof.as_mut().map(|d| &mut d.aperture),
        }
    }
}

/// One edit of a track's keys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyEdit {
    /// Insert a key (linear ease).
    Add {
        /// Seconds into the shot.
        time: f32,
        /// Track value.
        value: f32,
    },
    /// Move key `index` to a new time and value (its ease is kept).
    Move {
        /// Key index.
        index: usize,
        /// Seconds into the shot.
        time: f32,
        /// Track value.
        value: f32,
    },
    /// Remove key `index`.
    Remove {
        /// Key index.
        index: usize,
    },
    /// Change the ease of the segment that starts at key `index`.
    SetEase {
        /// Key index.
        index: usize,
        /// The new ease.
        ease: Ease,
    },
}

/// Why an edit was refused: an agent error code (lowercase ascii, digits and `_`) and a message.
/// Nothing is changed when an edit fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelError {
    /// Agent error code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, PanelError> {
    Err(PanelError {
        code,
        message: message.into(),
    })
}

/// A key of a shot list with a time in the shot: scalar, position path and rotation keys.
pub(crate) trait TimedKey {
    /// Seconds into the shot.
    fn time(&self) -> f32;
}

impl TimedKey for ScalarKey {
    fn time(&self) -> f32 {
        self.time
    }
}

/// One edit of a timed key list ([`edit_key_list`]): what the scalar and the pose edits share.
pub(crate) enum ListEdit<K> {
    /// Insert this key.
    Add(K),
    /// Replace key `index` with this one (moved in time, new value).
    Move(usize, K),
    /// Remove key `index`.
    Remove(usize),
}

/// Applies `edit` to `keys`, keeping them sorted, unique in time and inside the shot
/// (`0..=duration`); `check` judges a new key's value, and a move keeps what `keep` takes from the
/// old key (its ease). Returns the index of the key the edit left selected (`None` after a
/// removal). On an error the list is untouched. Codes: `shot_panel_bad_key`,
/// `shot_panel_out_of_range`, `shot_panel_key_time_taken`, and `check`'s.
pub(crate) fn edit_key_list<K: TimedKey>(
    keys: &mut Vec<K>,
    duration: f32,
    edit: ListEdit<K>,
    check: impl Fn(&K) -> Result<(), PanelError>,
    keep: impl FnOnce(&K, K) -> K,
) -> Result<Option<usize>, PanelError> {
    let in_shot = |key: &K| -> Result<(), PanelError> {
        let time = key.time();
        if !(time.is_finite() && (0.0..=duration).contains(&time)) {
            return refuse(
                "shot_panel_out_of_range",
                format!("time {time} is outside the shot (0..={duration})"),
            );
        }
        check(key)
    };
    let bad_key = |index: usize, len: usize| -> Result<(), PanelError> {
        if index < len {
            Ok(())
        } else {
            refuse(
                "shot_panel_bad_key",
                format!("no key {index}: the track has {len} keys"),
            )
        }
    };
    let taken = |time: f32| PanelError {
        code: "shot_panel_key_time_taken",
        message: format!("a key already sits at {time} s"),
    };
    let insert = |keys: &mut Vec<K>, key: K| -> usize {
        let at = keys.partition_point(|k| k.time() < key.time());
        keys.insert(at, key);
        at
    };
    match edit {
        ListEdit::Add(key) => {
            in_shot(&key)?;
            if keys.iter().any(|k| k.time() == key.time()) {
                return Err(taken(key.time()));
            }
            Ok(Some(insert(keys, key)))
        }
        ListEdit::Move(index, key) => {
            bad_key(index, keys.len())?;
            in_shot(&key)?;
            if keys
                .iter()
                .enumerate()
                .any(|(i, k)| i != index && k.time() == key.time())
            {
                return Err(taken(key.time()));
            }
            let old = keys.remove(index);
            let key = keep(&old, key);
            Ok(Some(insert(keys, key)))
        }
        ListEdit::Remove(index) => {
            bad_key(index, keys.len())?;
            keys.remove(index);
            Ok(None)
        }
    }
}

/// Applies `edit` to `track`, keeping the keys sorted and inside the shot (`0..=duration`) and the
/// values inside the track's range. Returns the index of the key the edit left selected
/// (`None` after a removal). On an error the track is untouched. The key list rules are
/// `edit_key_list`'s, shared with the position and rotation keys ([`apply_pose_key_edit`]).
pub fn apply_key_edit(
    track: &mut Track,
    kind: ShotTrack,
    duration: f32,
    edit: KeyEdit,
) -> Result<Option<usize>, PanelError> {
    let (min, max) = kind.range();
    let check = |key: &ScalarKey| -> Result<(), PanelError> {
        let value = key.value;
        if !(value.is_finite() && (min..=max).contains(&value)) {
            return refuse(
                "shot_panel_out_of_range",
                format!("{} value {value} is outside {min}..={max}", kind.name()),
            );
        }
        Ok(())
    };
    let keep_ease = |old: &ScalarKey, new: ScalarKey| ScalarKey {
        ease: old.ease,
        ..new
    };
    let key = |time: f32, value: f32| ScalarKey {
        time,
        value,
        ease: Ease::Linear,
    };
    match edit {
        KeyEdit::Add { time, value } => edit_key_list(
            &mut track.0,
            duration,
            ListEdit::Add(key(time, value)),
            check,
            keep_ease,
        ),
        KeyEdit::Move { index, time, value } => edit_key_list(
            &mut track.0,
            duration,
            ListEdit::Move(index, key(time, value)),
            check,
            keep_ease,
        ),
        KeyEdit::Remove { index } => {
            edit_key_list(&mut track.0, duration, ListEdit::Remove(index), check, keep_ease)
        }
        KeyEdit::SetEase { index, ease } => {
            if index >= track.0.len() {
                return refuse(
                    "shot_panel_bad_key",
                    format!("no key {index}: the track has {} keys", track.0.len()),
                );
            }
            track.0[index].ease = ease;
            Ok(Some(index))
        }
    }
}

/// The curve widget's view of a track: step keys hold, the other eases draw as straight lines.
#[must_use]
pub fn track_curve(track: &Track) -> Curve {
    Curve::from(
        track
            .0
            .iter()
            .map(|k| {
                let kind = if k.ease == Ease::Step {
                    CurveKeyKind::Constant
                } else {
                    CurveKeyKind::Linear
                };
                CurveKey::new(k.time, k.value, kind)
            })
            .collect::<Vec<_>>(),
    )
}

/// The key edit a curve widget action means, `None` for a scrub (which moves the clock instead).
#[must_use]
pub fn edit_of_action(action: &CurveAction) -> Option<KeyEdit> {
    Some(match action {
        CurveAction::Move(index, time, value) => KeyEdit::Move {
            index: *index,
            time: *time,
            value: *value,
        },
        CurveAction::Add(time, value) => KeyEdit::Add {
            time: *time,
            value: *value,
        },
        CurveAction::Remove(index) => KeyEdit::Remove { index: *index },
        CurveAction::SetKind(index, kind) => KeyEdit::SetEase {
            index: *index,
            ease: match kind {
                CurveKeyKind::Constant => Ease::Step,
                CurveKeyKind::Linear => Ease::Linear,
                CurveKeyKind::Cubic { .. } => Ease::Smooth,
            },
        },
        CurveAction::Scrub(_) => return None,
    })
}

/// The panel's selection and view; one resource shared by the egui panel and the agent methods.
#[derive(Resource, Debug, Default)]
pub struct ShotPanel {
    /// Index of the shot being edited.
    pub shot: usize,
    /// The track being edited.
    pub track: ShotTrack,
    /// The selected key of that track.
    pub selected: Option<usize>,
    /// The curve widget's pan and zoom.
    pub view: View2d,
    /// Last error from the panel, shown under the curve.
    pub error: Option<String>,
    /// The path row's text: the `.shots.ron` Open reads and Save writes (ADR-8730).
    pub path_text: String,
}

/// Applies `edit` to the document's shot list (the `ShotPlayer`'s loaded sequence, or the adopted
/// sequence player's own list) through `panel`'s selection: edits a copy, validates the whole
/// sequence, then swaps it in (the clock and play state stay).
pub fn edit_loaded(world: &mut World, panel: &mut ShotPanel, edit: KeyEdit) -> Result<(), PanelError> {
    crate::shot_document::sync(world);
    let Some(mut sequence) = loaded(world).cloned() else {
        return refuse("shot_panel_no_sequence", "no shot sequence is loaded");
    };
    let Some(shot) = sequence.shots.get_mut(panel.shot) else {
        return refuse(
            "shot_panel_bad_shot",
            format!(
                "no shot {}: the sequence has {} shots",
                panel.shot,
                sequence.shots.len()
            ),
        );
    };
    let (duration, name) = (shot.duration, shot.name.clone());
    let Some(track) = panel.track.track_mut(shot) else {
        return refuse(
            "shot_panel_no_dof",
            format!(
                "shot `{name}` has no depth of field, so no {} track",
                panel.track.name()
            ),
        );
    };
    let selected = apply_key_edit(track, panel.track, duration, edit)?;
    if let Err(e) = sequence.validate() {
        return refuse("shot_panel_invalid", e.to_string());
    }
    crate::shot_document::put(world, sequence);
    panel.selected = selected;
    Ok(())
}

fn loaded(world: &World) -> Option<&ShotSequence> {
    crate::shot_document::loaded(world)
}

/// The sequence player whose own shot list **Edit player's shots** would adopt: the Sequence
/// Curves panel's player when it has one, else the first player (lowest id) that has one.
fn adoptable_player(world: &mut World) -> Option<Entity> {
    let probe = world
        .get_resource::<crate::seq_curve_panel::SeqCurvePanel>()
        .map(|p| crate::seq_curve_panel::SeqCurvePanel {
            player: p.player,
            ..Default::default()
        })
        .unwrap_or_default();
    if let Some(e) = crate::seq_curve_panel::player_of(world, &probe)
        && world
            .get::<SequencePlayer>(e)
            .is_some_and(|p| p.shots().is_some())
    {
        return Some(e);
    }
    let mut query = world.query::<(Entity, &SequencePlayer)>();
    query
        .iter(world)
        .filter(|(_, p)| p.shots().is_some())
        .map(|(e, _)| e)
        .min_by_key(|e| e.to_bits())
}

/// Draws the Shots panel.
pub fn shot_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<ShotPanel>().unwrap_or_default();
    draw(ui, &mut panel, world);
    world.insert_resource(panel);
}

/// The document row: the path field, Open, Save and the dirty mark.
fn document_row(ui: &mut egui::Ui, panel: &mut ShotPanel, world: &mut World) {
    if panel.path_text.is_empty()
        && let Some(path) = crate::shot_document::path(world)
    {
        panel.path_text = path;
    }
    ui.horizontal(|ui| {
        let field = ui
            .add(egui::TextEdit::singleline(&mut panel.path_text).hint_text("project path of a .shots.ron"));
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Shot list path");
        let text = panel.path_text.trim().to_string();
        if crate::agent_parity::action_button(ui, &crate::agent_parity::SHOTS_OPEN).clicked() {
            panel.error = crate::shot_document::open_shot_list(world, &text, false)
                .err()
                .map(|e| e.message);
            if panel.error.is_none() {
                panel.shot = 0;
                panel.selected = None;
            }
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::SHOTS_SAVE).clicked() {
            save_from_panel(panel, world, &text);
        }
        if let Some(player) = adoptable_player(world)
            && crate::shot_document::host(world) != crate::shot_document::ShotHost::Player(player)
            && crate::agent_parity::action_button(ui, &crate::agent_parity::SHOTS_ADOPT)
                .on_hover_text("Take the sequence player's own shot list into this panel as an unsaved list")
                .clicked()
        {
            panel.error = crate::shot_document::adopt_player_shots(world, player, false)
                .err()
                .map(|e| e.message);
        }
        if crate::shot_document::is_dirty(world) {
            ui.weak("● unsaved");
        }
    });
}

/// Save from the panel: to the path field's text, else the document's path.
fn save_from_panel(panel: &mut ShotPanel, world: &mut World, text: &str) {
    let path = (!text.is_empty()).then_some(text);
    panel.error = crate::shot_document::save_shot_list(world, path)
        .err()
        .map(|e| e.message);
}

/// Ctrl+S (Cmd+S on macOS) saves while the pointer is over the panel and no text field has
/// focus; elsewhere the key is left alone. The key is consumed; should egui run a second UI pass
/// in the frame and see it again, that save writes the same bytes and changes nothing.
fn save_shortcut(ui: &egui::Ui, panel: &mut ShotPanel, world: &mut World) {
    if !ui.rect_contains_pointer(ui.max_rect()) || ui.ctx().text_edit_focused() {
        return;
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
        let text = panel.path_text.trim().to_string();
        save_from_panel(panel, world, &text);
    }
}

fn draw(ui: &mut egui::Ui, panel: &mut ShotPanel, world: &mut World) {
    // A list another tool loaded in the document's place detaches it before anything can save;
    // the path row stops naming the old file (this frame's panel is out of the world, so `sync`
    // cannot empty it itself).
    if let Some(old) = crate::shot_document::sync(world)
        && panel.path_text.trim() == old
    {
        panel.path_text.clear();
    }
    save_shortcut(ui, panel, world);
    document_row(ui, panel, world);
    let Some(sequence) = loaded(world).cloned() else {
        ui.label("No shot sequence is loaded. Open a .shots.ron above.");
        if let Some(error) = &panel.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        return;
    };
    panel.shot = panel.shot.min(sequence.shots.len().saturating_sub(1));
    ui.horizontal(|ui| {
        egui::ComboBox::from_label("Shot")
            .selected_text(sequence.shots[panel.shot].name.clone())
            .show_ui(ui, |ui| {
                for (i, shot) in sequence.shots.iter().enumerate() {
                    if ui.selectable_value(&mut panel.shot, i, &shot.name).changed() {
                        panel.selected = None;
                    }
                }
            });
        let mut pose = world.remove_resource::<ShotPosePanel>().unwrap_or_default();
        egui::ComboBox::from_label("Track")
            .selected_text(pose.track.map_or(panel.track.name(), PoseTrack::name))
            .show_ui(ui, |ui| {
                for t in ShotTrack::ALL {
                    let on = pose.track.is_none() && panel.track == t;
                    if ui.selectable_label(on, t.name()).clicked() {
                        panel.track = t;
                        panel.selected = None;
                        pose.track = None;
                    }
                }
                for t in PoseTrack::ALL {
                    if ui.selectable_label(pose.track == Some(t), t.name()).clicked() {
                        pose.track = Some(t);
                        pose.selected = None;
                    }
                }
            });
        world.insert_resource(pose);
    });
    if let Some(track) = world.get_resource::<ShotPosePanel>().and_then(|p| p.track) {
        pose::draw(ui, world, panel, &sequence, track);
        if let Some(error) = &panel.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        return;
    }
    let shot = &sequence.shots[panel.shot];
    let Some(track) = panel.track.track(shot) else {
        ui.label(format!("Shot `{}` has no depth of field.", shot.name));
        return;
    };
    let curve = track_curve(track);
    let host = crate::shot_document::host(world);
    let playhead = match host {
        crate::shot_document::ShotHost::ShotPlayer => world
            .get_resource::<ShotPlayer>()
            .and_then(ShotPlayer::sample)
            .filter(|s| s.shot == panel.shot)
            .map(|s| s.local_time),
        crate::shot_document::ShotHost::Player(e) => world.get::<SequencePlayer>(e).and_then(|p| {
            let local = p.time() - sequence.shot_start(panel.shot);
            (0.0..f64::from(shot.duration))
                .contains(&local)
                .then_some(local as f32)
        }),
    };
    let id = ui.id().with("shot_panel_curve");
    let action = curve_editor(ui, id, &curve, &mut panel.view, &mut panel.selected, playhead);
    if let Some(action) = action {
        panel.error = None;
        match edit_of_action(&action) {
            Some(edit) => {
                if let Err(e) = edit_loaded(world, panel, edit) {
                    panel.error = Some(e.message);
                }
            }
            None => {
                if let CurveAction::Scrub(local) = action {
                    let at = sequence.shot_start(panel.shot) + f64::from(local);
                    match host {
                        crate::shot_document::ShotHost::ShotPlayer => {
                            if let Some(mut player) = world.get_resource_mut::<ShotPlayer>() {
                                player.seek(at);
                            }
                        }
                        crate::shot_document::ShotHost::Player(e) => {
                            if let Some(mut player) = world.get_mut::<SequencePlayer>(e) {
                                player.seek(at);
                            }
                            crate::seq_tracks::refresh_preview(world);
                        }
                    }
                }
            }
        }
    }
    if let Some(error) = &panel.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
}

const SHOT: ParamSchema = ParamSchema {
    name: "shot",
    kind: "string",
    required: false,
    description: "Shot name to select (default: the panel's current shot).",
};
const TRACK: ParamSchema = ParamSchema {
    name: "track",
    kind: "string",
    required: false,
    description: "fov, roll, focus_distance, aperture, position or rotation (default: the panel's current track).",
};

/// The agent methods of the panel.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.shot_panel_state",
                tool: "shot_panel_state",
                description: "Read the Shots panel: the loaded sequence's shots, the selected shot and track, \
                    and that track's keys (time, value, ease). Optionally selects a shot and a track first.",
                params: &[SHOT, TRACK],
                result: "{ loaded, path, dirty, host: \"shot_player\" | { player }, shots: [name], shot, shot_name, duration, track, tracks: [name], range: [min, max], has_track, selected, keys: [{ time, value, ease }], error }",
                mutating: false,
                permission: Permission::Read,
            },
            shot_panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.shot_panel_edit",
                tool: "shot_panel_edit",
                description: "Edit one key of the selected track of the loaded sequence, as a drag in the Shots panel \
                    would: add, move, remove or set the ease. Track position or rotation edits the shot camera's \
                    path or rotation keys (add, move, remove; one undo entry each). The whole sequence is \
                    validated first; a refused edit changes nothing.",
                params: &[
                    SHOT,
                    TRACK,
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "add, move, remove or ease.",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: false,
                        description: "move, remove, ease: the key index.",
                    },
                    ParamSchema {
                        name: "time",
                        kind: "number",
                        required: false,
                        description: "add, move: seconds into the shot.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "add, move: the track value (a number; position: [x, y, z]; rotation: a \
                            quaternion [x, y, z, w], stored normalized).",
                    },
                    ParamSchema {
                        name: "ease",
                        kind: "string",
                        required: false,
                        description: "ease: Linear, Step or Smooth.",
                    },
                ],
                result: "{ applied, selected, keys: [{ time, value, ease }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            shot_panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.shot_panel_open",
                tool: "shot_panel_open",
                description: "Open a project .shots.ron into the ShotPlayer as the Shots panel's document \
                    (stopped at zero; the timeline's cut row edits it when the sequence player has no shot list \
                    of its own). Refused while the loaded list has unsaved edits unless `discard`.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path ending in .shots.ron.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop the loaded list's unsaved edits (default false).",
                    },
                ],
                result: "{ path, shots, dirty }",
                mutating: true,
                permission: Permission::EditScene,
            },
            shot_panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.shot_panel_save",
                tool: "shot_panel_save",
                description: "Save the loaded shot list as .shots.ron with an atomic write (`path`, or the file it \
                    was opened from or last saved to) and make that the document's path. Saving an unchanged \
                    list again writes the same bytes; a failed save keeps the document dirty.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Project-relative path ending in .shots.ron (default: the document's path).",
                }],
                result: "{ path, bytes, dirty }",
                mutating: true,
                permission: Permission::Assets,
            },
            shot_panel_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.shot_panel_adopt",
                tool: "shot_panel_adopt",
                description: "Take a sequence player's own shot list (set by the host with SequencePlayer::set_shots) \
                    as the Shots panel's document, unsaved and with no path (the panel's Edit player's shots). \
                    editor.shot_panel_save { path } then writes it as .shots.ron at the current version and it \
                    behaves like an opened document. Refused while the loaded list has unsaved edits unless \
                    `discard`.",
                params: &[
                    ParamSchema {
                        name: "player",
                        kind: "number",
                        required: false,
                        description: "Sequence player entity id (default: the Sequence Curves panel's player when it \
                            has a shot list, else the first player that has one).",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop the loaded list's unsaved edits (default false).",
                    },
                ],
                result: "{ player, shots, path, dirty }",
                mutating: true,
                permission: Permission::EditScene,
            },
            shot_panel_adopt as Handler,
        ),
    ]
}

fn rpc(e: PanelError) -> RpcError {
    let hint = match e.code {
        "shot_panel_no_sequence" => "render.sequence { action: \"load\", ... } or camera.set_path first",
        "shot_panel_no_dof" => "camera.set_path { shot, path, dof: {...} } adds depth of field",
        "shot_panel_key_time_taken" => "move or remove the key at that time first",
        "shot_panel_unsaved" => {
            "editor.shot_panel_save first, or editor.shot_panel_open { path, discard: true }"
        }
        "shot_panel_bad_path" => "a project-relative path ending in .shots.ron, inside the project",
        "shot_panel_no_project" => "open a project first",
        "shot_panel_io" => "check that the file exists (open) or that its folder can be written (save)",
        "shot_panel_bad_file" => "the file is not a valid .shots.ron: the message says why",
        "shot_panel_detached" => {
            "editor.shot_panel_save { path } writes the new list to a file of its own; open the old file again \
             with editor.shot_panel_open"
        }
        "shot_panel_no_player" => "a sequence player entity id (editor.seq_curve_state lists them)",
        _ => "the message says what is out of range; nothing was changed",
    };
    let rpc_code = match e.code {
        "shot_panel_no_sequence"
        | "shot_panel_bad_shot"
        | "shot_panel_no_dof"
        | "shot_panel_bad_key"
        | "shot_panel_no_player" => regia_agent::protocol::NOT_FOUND,
        "shot_panel_io" | "shot_panel_no_project" => regia_agent::protocol::INTERNAL_ERROR,
        _ => regia_agent::protocol::INVALID_PARAMS,
    };
    RpcError::new(rpc_code, e.code, e.message, hint)
}

/// Selects the shot and track named in `params` on `panel` (a pose track name selects that pose
/// track instead, through `pose_selection`, which the caller stores).
fn select_into(
    world: &World,
    panel: &mut ShotPanel,
    params: &Value,
    pose_selection: &mut Option<ShotPosePanel>,
) -> Result<(), PanelError> {
    if let Some(name) = params.get("shot").and_then(Value::as_str) {
        let Some(index) = loaded(world).and_then(|s| s.shot_index(name)) else {
            return refuse("shot_panel_bad_shot", format!("no shot named `{name}`"));
        };
        if index != panel.shot {
            panel.selected = None;
            let mut pose = world.get_resource::<ShotPosePanel>().cloned().unwrap_or_default();
            pose.selected = None;
            pose_selection.replace(pose);
        }
        panel.shot = index;
    }
    if let Some(name) = params.get("track").and_then(Value::as_str) {
        if let Some(track) = PoseTrack::parse(name) {
            let mut pose = pose_selection
                .take()
                .or_else(|| world.get_resource::<ShotPosePanel>().cloned())
                .unwrap_or_default();
            if pose.track != Some(track) {
                pose.selected = None;
            }
            pose.track = Some(track);
            pose_selection.replace(pose);
            return Ok(());
        }
        let Some(track) = ShotTrack::parse(name) else {
            return refuse(
                "shot_panel_bad_track",
                format!("`{name}` is not a track (fov, roll, focus_distance, aperture, position, rotation)"),
            );
        };
        let mut pose = pose_selection
            .take()
            .or_else(|| world.get_resource::<ShotPosePanel>().cloned())
            .unwrap_or_default();
        pose.track = None;
        pose_selection.replace(pose);
        if track != panel.track {
            panel.selected = None;
        }
        panel.track = track;
    }
    Ok(())
}

/// [`select_into`], storing the pose selection.
fn select(world: &mut World, panel: &mut ShotPanel, params: &Value) -> Result<(), PanelError> {
    let mut pose = None;
    let out = select_into(world, panel, params, &mut pose);
    if out.is_ok()
        && let Some(pose) = pose
    {
        world.insert_resource(pose);
    }
    out
}

fn keys_json(track: Option<&Track>) -> Value {
    json!(
        track
            .map(|t| t
                .0
                .iter()
                .map(|k| json!({ "time": k.time, "value": k.value, "ease": format!("{:?}", k.ease) }))
                .collect::<Vec<_>>())
            .unwrap_or_default()
    )
}

fn host_json(world: &World) -> Value {
    match crate::shot_document::host(world) {
        crate::shot_document::ShotHost::ShotPlayer => json!("shot_player"),
        crate::shot_document::ShotHost::Player(e) => json!({ "player": e.to_bits() }),
    }
}

fn state_json(world: &World, panel: &ShotPanel) -> Value {
    let path = crate::shot_document::path(world);
    let dirty = crate::shot_document::is_dirty(world);
    let host = host_json(world);
    let Some(sequence) = loaded(world) else {
        return json!({ "loaded": false, "path": path, "dirty": dirty, "host": host, "shots": [], "error": panel.error });
    };
    let shot = sequence.shots.get(panel.shot);
    let tracks: Vec<&str> = ShotTrack::ALL
        .map(ShotTrack::name)
        .into_iter()
        .chain(PoseTrack::ALL.map(PoseTrack::name))
        .collect();
    if let Some(pose) = world
        .get_resource::<ShotPosePanel>()
        .filter(|p| p.track.is_some())
    {
        let track = pose.track.unwrap_or(PoseTrack::Position);
        return json!({
            "loaded": true,
            "path": path,
            "dirty": dirty,
            "host": host,
            "shots": sequence.shots.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            "shot": panel.shot,
            "shot_name": shot.map(|s| s.name.clone()),
            "duration": shot.map(|s| s.duration),
            "track": track.name(),
            "tracks": tracks,
            "range": Value::Null,
            "has_track": shot.is_some(),
            "selected": pose.selected,
            "keys": shot.map_or_else(|| json!([]), |s| pose::keys_json(&s.camera, track)),
            "error": panel.error,
        });
    }
    let track = shot.and_then(|s| panel.track.track(s));
    let (min, max) = panel.track.range();
    json!({
        "loaded": true,
        "path": path,
        "dirty": dirty,
        "host": host,
        "shots": sequence.shots.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
        "shot": panel.shot,
        "shot_name": shot.map(|s| s.name.clone()),
        "duration": shot.map(|s| s.duration),
        "track": panel.track.name(),
        "tracks": tracks,
        "range": [min, max],
        "has_track": track.is_some(),
        "selected": panel.selected,
        "keys": keys_json(track),
        "error": panel.error,
    })
}

/// A read (`mutating: false`): a pending detach is reported (path `null`, dirty) through the
/// document's reads, which see it at once, and is left for the next mutating call or panel draw
/// to commit (sprint-w9-rev-v06-lanes § 1 finding 8).
fn shot_panel_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<ShotPanel>().unwrap_or_default();
    let selected = select(world, &mut panel, params);
    let out = selected.map(|()| state_json(world, &panel)).map_err(rpc);
    world.insert_resource(panel);
    out
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a finite number"))),
    }
}

fn index_param(params: &Value) -> Result<usize, RpcError> {
    params
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| RpcError::invalid_params("`index` must be a whole number (the key index)"))
}

fn edit_of_params(params: &Value) -> Result<KeyEdit, RpcError> {
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`op` is required: add, move, remove or ease"))?;
    let need = |value: Option<f32>, field: &str| {
        value.ok_or_else(|| RpcError::invalid_params(format!("`{field}` is required for `{op}`")))
    };
    match op {
        "add" => Ok(KeyEdit::Add {
            time: need(number(params, "time")?, "time")?,
            value: need(number(params, "value")?, "value")?,
        }),
        "move" => Ok(KeyEdit::Move {
            index: index_param(params)?,
            time: need(number(params, "time")?, "time")?,
            value: need(number(params, "value")?, "value")?,
        }),
        "remove" => Ok(KeyEdit::Remove {
            index: index_param(params)?,
        }),
        "ease" => {
            let ease = match params.get("ease").and_then(Value::as_str) {
                Some("Linear") => Ease::Linear,
                Some("Step") => Ease::Step,
                Some("Smooth") => Ease::Smooth,
                _ => return Err(RpcError::invalid_params("`ease` is Linear, Step or Smooth")),
            };
            Ok(KeyEdit::SetEase {
                index: index_param(params)?,
                ease,
            })
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown `op` `{other}` (add, move, remove, ease)"
        ))),
    }
}

fn shot_panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<ShotPanel>().unwrap_or_default();
    if let Err(e) = select(world, &mut panel, params) {
        world.insert_resource(panel);
        return Err(rpc(e));
    }
    if let Some(track) = world.get_resource::<ShotPosePanel>().and_then(|p| p.track) {
        let out = pose::edit_of_params(params, track).and_then(|edit| {
            let selected = edit_pose(world, &panel, track, edit, None).map_err(rpc)?;
            if let Some(mut pose) = world.get_resource_mut::<ShotPosePanel>() {
                pose.selected = selected;
            }
            let keys = crate::shot_document::loaded(world)
                .and_then(|s| s.shots.get(panel.shot))
                .map_or_else(|| json!([]), |s| pose::keys_json(&s.camera, track));
            Ok(json!({ "applied": true, "selected": selected, "keys": keys }))
        });
        world.insert_resource(panel);
        return out;
    }
    let edit = match edit_of_params(params) {
        Ok(edit) => edit,
        Err(e) => {
            world.insert_resource(panel);
            return Err(e);
        }
    };
    let result = edit_loaded(world, &mut panel, edit).map_err(rpc);
    let out = result.map(|()| {
        let keys = loaded(world)
            .and_then(|s| s.shots.get(panel.shot))
            .and_then(|s| panel.track.track(s));
        json!({ "applied": true, "selected": panel.selected, "keys": keys_json(keys) })
    });
    world.insert_resource(panel);
    out
}

fn shot_panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (a project .shots.ron path) is required"))?;
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let shots = crate::shot_document::open_shot_list(world, path, discard).map_err(rpc)?;
    Ok(json!({ "path": path, "shots": shots, "dirty": crate::shot_document::is_dirty(world) }))
}

fn shot_panel_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let (path, bytes) = crate::shot_document::save_shot_list(world, path).map_err(rpc)?;
    Ok(json!({ "path": path, "bytes": bytes, "dirty": crate::shot_document::is_dirty(world) }))
}

fn shot_panel_adopt(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let player = match params.get("player").filter(|v| !v.is_null()) {
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`player` must be an entity id"))?,
        ),
        None => adoptable_player(world),
    };
    let Some(player) = player else {
        return Err(rpc(PanelError {
            code: "shot_panel_no_player",
            message: "no sequence player has a shot list of its own".into(),
        }));
    };
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let shots = crate::shot_document::adopt_player_shots(world, player, discard).map_err(rpc)?;
    Ok(json!({
        "player": player.to_bits(),
        "shots": shots,
        "path": crate::shot_document::path(world),
        "dirty": crate::shot_document::is_dirty(world),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::shots::{CameraPath, DepthOfFieldTracks, LookAt, PathInterp, PathKey, ShotCamera};

    fn key(time: f32, value: f32) -> ScalarKey {
        ScalarKey {
            time,
            value,
            ease: Ease::Linear,
        }
    }

    fn sequence_with_dof() -> ShotSequence {
        let shot = |name: &str, dof: bool| Shot {
            name: name.into(),
            duration: 4.0,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::Linear,
                    keys: vec![PathKey {
                        time: 0.0,
                        position: [0.0; 3],
                    }],
                    look_at: LookAt::Point([0.0, 0.0, -5.0]),
                },
                fov: Track(vec![key(0.0, 40.0), key(2.0, 60.0)]),
                roll: Track::default(),
                dof: dof.then(DepthOfFieldTracks::default),
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: [0.0; 3],
        };
        ShotSequence {
            shots: vec![shot("a", false), shot("b", true)],
            ..ShotSequence::default()
        }
    }

    fn world_with_sequence() -> World {
        let mut world = World::new();
        let mut player = ShotPlayer::default();
        player.load(sequence_with_dof());
        world.insert_resource(player);
        world
    }

    #[test]
    fn edits_keep_keys_sorted_in_range_and_unique() {
        let mut track = Track(vec![key(0.0, 40.0), key(2.0, 60.0)]);
        let fov = ShotTrack::Fov;
        assert_eq!(
            apply_key_edit(
                &mut track,
                fov,
                4.0,
                KeyEdit::Add {
                    time: 1.0,
                    value: 50.0
                }
            ),
            Ok(Some(1))
        );
        assert_eq!(
            track.0.iter().map(|k| k.time).collect::<Vec<_>>(),
            [0.0, 1.0, 2.0]
        );
        // A move past a neighbor re-sorts and reports the new index; the ease travels with the key.
        track.0[1].ease = Ease::Smooth;
        assert_eq!(
            apply_key_edit(
                &mut track,
                fov,
                4.0,
                KeyEdit::Move {
                    index: 1,
                    time: 3.0,
                    value: 55.0
                }
            ),
            Ok(Some(2))
        );
        assert_eq!(
            track.0[2],
            ScalarKey {
                time: 3.0,
                value: 55.0,
                ease: Ease::Smooth
            }
        );
        assert_eq!(
            apply_key_edit(
                &mut track,
                fov,
                4.0,
                KeyEdit::SetEase {
                    index: 0,
                    ease: Ease::Step
                }
            ),
            Ok(Some(0))
        );
        assert_eq!(track.0[0].ease, Ease::Step);
        assert_eq!(
            apply_key_edit(&mut track, fov, 4.0, KeyEdit::Remove { index: 2 }),
            Ok(None)
        );
        assert_eq!(track.0.len(), 2);
    }

    #[test]
    fn refused_edits_change_nothing_and_name_a_code() {
        let original = Track(vec![key(0.0, 40.0), key(2.0, 60.0)]);
        let mut track = original.clone();
        let code = |track: &mut Track, kind: ShotTrack, edit: KeyEdit| {
            apply_key_edit(track, kind, 4.0, edit).expect_err("refused").code
        };
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Fov,
                KeyEdit::Add {
                    time: 2.0,
                    value: 50.0
                }
            ),
            "shot_panel_key_time_taken"
        );
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Fov,
                KeyEdit::Add {
                    time: 5.0,
                    value: 50.0
                }
            ),
            "shot_panel_out_of_range"
        );
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Fov,
                KeyEdit::Add {
                    time: 1.0,
                    value: 500.0
                }
            ),
            "shot_panel_out_of_range"
        );
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Fov,
                KeyEdit::Add {
                    time: f32::NAN,
                    value: 50.0
                }
            ),
            "shot_panel_out_of_range"
        );
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Fov,
                KeyEdit::Move {
                    index: 0,
                    time: 2.0,
                    value: 50.0
                }
            ),
            "shot_panel_key_time_taken"
        );
        assert_eq!(
            code(&mut track, ShotTrack::Fov, KeyEdit::Remove { index: 9 }),
            "shot_panel_bad_key"
        );
        assert_eq!(
            code(
                &mut track,
                ShotTrack::Aperture,
                KeyEdit::Add {
                    time: 1.0,
                    value: 0.01
                }
            ),
            "shot_panel_out_of_range"
        );
        assert_eq!(track, original);
    }

    #[test]
    fn curve_actions_map_to_edits() {
        assert_eq!(
            edit_of_action(&CurveAction::Add(1.0, 2.0)),
            Some(KeyEdit::Add {
                time: 1.0,
                value: 2.0
            })
        );
        assert_eq!(
            edit_of_action(&CurveAction::SetKind(3, CurveKeyKind::Constant)),
            Some(KeyEdit::SetEase {
                index: 3,
                ease: Ease::Step
            })
        );
        assert_eq!(edit_of_action(&CurveAction::Scrub(1.0)), None);
        let curve = track_curve(&Track(vec![
            ScalarKey {
                time: 0.0,
                value: 1.0,
                ease: Ease::Step,
            },
            key(1.0, 2.0),
        ]));
        assert_eq!(curve.keys.len(), 2);
        assert_eq!(curve.keys[0].kind, CurveKeyKind::Constant);
    }

    #[test]
    fn the_agent_methods_edit_the_loaded_sequence() {
        let mut world = world_with_sequence();
        let state = shot_panel_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["loaded"], true);
        assert_eq!(state["shots"], json!(["a", "b"]));
        assert_eq!(state["track"], "fov");
        assert_eq!(state["keys"].as_array().map(Vec::len), Some(2));

        let out =
            shot_panel_edit(&mut world, &json!({ "op": "add", "time": 1.0, "value": 50.0 })).expect("add");
        assert_eq!(out["selected"], 1);
        assert_eq!(out["keys"].as_array().map(Vec::len), Some(3));
        let in_player = world
            .resource::<ShotPlayer>()
            .sequence()
            .map(|s| s.shots[0].camera.fov.0.len());
        assert_eq!(in_player, Some(3), "the live sequence changed");

        let out = shot_panel_edit(&mut world, &json!({ "op": "ease", "index": 1, "ease": "Smooth" }))
            .expect("ease");
        assert_eq!(out["keys"][1]["ease"], "Smooth");
        shot_panel_edit(&mut world, &json!({ "op": "remove", "index": 1 })).expect("remove");
        let state = shot_panel_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["keys"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn agent_errors_carry_codes_and_change_nothing() {
        let mut world = World::new();
        let code = |e: RpcError| {
            e.data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let err =
            shot_panel_edit(&mut world, &json!({ "op": "remove", "index": 0 })).expect_err("nothing loaded");
        assert_eq!(code(err), "shot_panel_no_sequence");
        let state = shot_panel_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["loaded"], false);

        let mut world = world_with_sequence();
        let before = world.resource::<ShotPlayer>().sequence().cloned();
        let err = shot_panel_edit(
            &mut world,
            &json!({ "op": "add", "time": 1.0, "value": 50.0, "shot": "zzz" }),
        )
        .expect_err("unknown shot");
        assert_eq!(code(err), "shot_panel_bad_shot");
        let err = shot_panel_edit(
            &mut world,
            &json!({ "op": "add", "time": 1.0, "value": 50.0, "track": "zoom" }),
        )
        .expect_err("unknown track");
        assert_eq!(code(err), "shot_panel_bad_track");
        // Shot `a` has no depth of field.
        let err = shot_panel_edit(
            &mut world,
            &json!({ "op": "add", "time": 1.0, "value": 3.0, "track": "aperture" }),
        )
        .expect_err("no dof");
        assert_eq!(code(err), "shot_panel_no_dof");
        let err = shot_panel_edit(
            &mut world,
            &json!({ "op": "add", "time": 9.0, "value": 50.0, "track": "fov" }),
        )
        .expect_err("range");
        assert_eq!(code(err), "shot_panel_out_of_range");
        assert!(shot_panel_edit(&mut world, &json!({ "op": "dance" })).is_err());
        assert!(shot_panel_edit(&mut world, &json!({ "op": "add", "time": 1.0 })).is_err());
        assert_eq!(
            world.resource::<ShotPlayer>().sequence().cloned(),
            before,
            "nothing changed"
        );

        // Shot `b` has depth of field: its aperture track can be keyed.
        let out = shot_panel_edit(
            &mut world,
            &json!({ "op": "add", "time": 1.0, "value": 2.8, "shot": "b", "track": "aperture" }),
        )
        .expect("aperture key");
        // Track values are f32: compare at that precision (JSON carries the f32 widened to f64).
        assert_eq!(out["keys"][0]["value"].as_f64().map(|v| v as f32), Some(2.8_f32));
    }
}
