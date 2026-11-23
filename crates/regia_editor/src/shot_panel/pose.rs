// SPDX-License-Identifier: MPL-2.0
//! The Shots panel's pose tracks (ADR-7726): the selected shot camera's position path keys
//! (`camera.path.keys`) and rotation keys (`camera.rotation`, shot list v2). The panel draws the
//! path from above (X right, Z down) with its keys and, at each rotation key, the camera's view
//! direction; a key list edits each key's time and value. Add key at playhead keys the preview
//! pose, Delete key removes the selected key, and dragging a field moves it. Every edit goes
//! through [`apply_pose_key_edit`] (whose list rules are `edit_key_list`'s, the scalar tracks'
//! too) on a copy of the whole list, is validated, swapped into the live list (so the preview
//! follows) and recorded as one undo entry ([`ShotListEdit`]; a field drag is one entry).
//! `editor.shot_panel_edit { track: "position" | "rotation" }` runs the same code.

use regia_core::math::{EulerRot, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_framework::sequencer::SequencePlayer;
use regia_framework::shots::{
    Ease, MAX_PATH_COORDINATE, PathKey, RotationKey, ShotCamera, ShotPlayer, ShotSequence, look_rotation,
    sample_rotation,
};
use serde_json::{Value, json};

use super::{ListEdit, PanelError, ShotPanel, TimedKey, edit_key_list, refuse};
use crate::command::{EditorCommand, UndoStack, downcast_to};
use crate::shot_document::ShotHost;

/// A pose track of a shot camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoseTrack {
    /// The position path keys (`camera.path.keys`, at least one).
    Position,
    /// The orientation keys (`camera.rotation`; none: the look-at rule).
    Rotation,
}

impl PoseTrack {
    /// Both, in panel order.
    pub const ALL: [Self; 2] = [Self::Position, Self::Rotation];

    /// The name agents use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Position => "position",
            Self::Rotation => "rotation",
        }
    }

    /// The track with this name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A pose key's value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PoseValue {
    /// World position, metres.
    Position([f32; 3]),
    /// Orientation before roll, a quaternion `[x, y, z, w]` (stored normalized).
    Rotation([f32; 4]),
}

/// One edit of a pose track's keys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PoseKeyEdit {
    /// Insert a key (a rotation key eases linearly).
    Add {
        /// Seconds into the shot.
        time: f32,
        /// The key's value.
        value: PoseValue,
    },
    /// Move key `index` to a new time and value (a rotation key keeps its ease).
    Move {
        /// Key index.
        index: usize,
        /// Seconds into the shot.
        time: f32,
        /// The key's value.
        value: PoseValue,
    },
    /// Remove key `index` (the path keeps at least one key).
    Remove {
        /// Key index.
        index: usize,
    },
}

impl TimedKey for PathKey {
    fn time(&self) -> f32 {
        self.time
    }
}

impl TimedKey for RotationKey {
    fn time(&self) -> f32 {
        self.time
    }
}

fn position_ok(key: &PathKey) -> Result<(), PanelError> {
    if key
        .position
        .iter()
        .any(|v| !v.is_finite() || v.abs() > MAX_PATH_COORDINATE)
    {
        return refuse(
            "shot_panel_out_of_range",
            format!(
                "position {:?} must be finite and within {MAX_PATH_COORDINATE} m",
                key.position
            ),
        );
    }
    Ok(())
}

/// The unit quaternion of `q`, or `shot_panel_out_of_range` for a zero or non-finite one.
fn unit(q: [f32; 4]) -> Result<[f32; 4], PanelError> {
    let length = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !(length.is_finite() && length > 1.0e-6) {
        return refuse(
            "shot_panel_out_of_range",
            format!("rotation {q:?} must be a finite, non-zero quaternion"),
        );
    }
    Ok(q.map(|v| v / length))
}

/// Applies `edit` to `track` of `camera`. Returns the index of the key the edit left selected
/// (`None` after a removal); on an error the camera is untouched. Codes: as `apply_key_edit`, plus
/// `shot_panel_bad_value` (a rotation value for the position track or the other way round) and
/// `shot_panel_last_key` (removing the path's only key).
///
/// # Errors
/// The codes above.
pub fn apply_pose_key_edit(
    camera: &mut ShotCamera,
    track: PoseTrack,
    duration: f32,
    edit: PoseKeyEdit,
) -> Result<Option<usize>, PanelError> {
    let (time, value, index) = match edit {
        PoseKeyEdit::Add { time, value } => (time, Some(value), None),
        PoseKeyEdit::Move { index, time, value } => (time, Some(value), Some(index)),
        PoseKeyEdit::Remove { index } => (0.0, None, Some(index)),
    };
    match track {
        PoseTrack::Position => {
            let key = match value {
                None => None,
                Some(PoseValue::Position(position)) => Some(PathKey { time, position }),
                Some(PoseValue::Rotation(_)) => {
                    return refuse("shot_panel_bad_value", "the position track takes [x, y, z]");
                }
            };
            let keys = &mut camera.path.keys;
            if key.is_none() && keys.len() == 1 && index == Some(0) {
                return refuse(
                    "shot_panel_last_key",
                    "the position path needs at least one key: move it instead",
                );
            }
            let list = match (key, index) {
                (Some(key), None) => ListEdit::Add(key),
                (Some(key), Some(index)) => ListEdit::Move(index, key),
                (None, index) => ListEdit::Remove(index.unwrap_or(0)),
            };
            edit_key_list(keys, duration, list, position_ok, |_, new| new)
        }
        PoseTrack::Rotation => {
            let key = match value {
                None => None,
                Some(PoseValue::Rotation(q)) => Some(RotationKey {
                    time,
                    rotation: unit(q)?,
                    ease: Ease::Linear,
                }),
                Some(PoseValue::Position(_)) => {
                    return refuse("shot_panel_bad_value", "the rotation track takes [x, y, z, w]");
                }
            };
            let list = match (key, index) {
                (Some(key), None) => ListEdit::Add(key),
                (Some(key), Some(index)) => ListEdit::Move(index, key),
                (None, index) => ListEdit::Remove(index.unwrap_or(0)),
            };
            edit_key_list(
                &mut camera.rotation,
                duration,
                list,
                |_| Ok(()),
                |old, new| RotationKey {
                    ease: old.ease,
                    ..new
                },
            )
        }
    }
}

/// The Shots panel's pose selection (a resource beside [`ShotPanel`]).
#[derive(Resource, Debug, Default, Clone)]
pub struct ShotPosePanel {
    /// The pose track being edited, `None` while a scalar track is.
    pub track: Option<PoseTrack>,
    /// The selected key of that track.
    pub selected: Option<usize>,
    /// The running field drag: its edits share one undo entry.
    gesture: Option<u64>,
    /// Counts drags, so two drags never merge.
    gestures: u64,
    /// The egui frame of the last Add or Delete (egui may run the panel twice in a frame).
    acted: Option<u64>,
}

/// One undo entry of a pose key edit: the shot list before and after. Edits of one field drag
/// merge. It belongs to the document and host it was made on (ADR-8730): after another list is
/// opened or the document moves to another host it does nothing.
pub struct ShotListEdit {
    host: ShotHost,
    generation: u64,
    before: ShotSequence,
    after: ShotSequence,
    gesture: Option<u64>,
}

impl ShotListEdit {
    fn current(&self, world: &World) -> bool {
        crate::shot_document::host(world) == self.host
            && crate::shot_document::generation(world) == self.generation
    }
}

impl EditorCommand for ShotListEdit {
    fn apply(&mut self, world: &mut World) {
        if self.current(world) {
            crate::shot_document::put(world, self.after.clone());
        }
    }

    fn revert(&mut self, world: &mut World) {
        if self.current(world) {
            crate::shot_document::put(world, self.before.clone());
        }
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<ShotListEdit>(next) else {
            return false;
        };
        if self.gesture.is_some()
            && self.gesture == next.gesture
            && self.host == next.host
            && self.generation == next.generation
        {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        "Edit shot camera keys".to_string()
    }
}

/// Applies `edit` to the pose track `track` of `panel`'s shot in the document's list: edits a
/// copy, validates the whole list, swaps it in and records one undo entry (merged within
/// `gesture`). Returns the selected key.
///
/// # Errors
/// `shot_panel_no_sequence`, `shot_panel_bad_shot`, `shot_panel_invalid` and the codes of
/// [`apply_pose_key_edit`].
pub fn edit_pose(
    world: &mut World,
    panel: &ShotPanel,
    track: PoseTrack,
    edit: PoseKeyEdit,
    gesture: Option<u64>,
) -> Result<Option<usize>, PanelError> {
    crate::shot_document::sync(world);
    let Some(before) = crate::shot_document::loaded(world).cloned() else {
        return refuse("shot_panel_no_sequence", "no shot sequence is loaded");
    };
    let mut list = before.clone();
    let count = list.shots.len();
    let Some(shot) = list.shots.get_mut(panel.shot) else {
        return refuse(
            "shot_panel_bad_shot",
            format!("no shot {}: the sequence has {count} shots", panel.shot),
        );
    };
    let duration = shot.duration;
    let selected = apply_pose_key_edit(&mut shot.camera, track, duration, edit)?;
    if let Err(e) = list.validate() {
        return refuse("shot_panel_invalid", e.to_string());
    }
    if list == before {
        return Ok(selected);
    }
    crate::shot_document::put(world, list.clone());
    let command = ShotListEdit {
        host: crate::shot_document::host(world),
        generation: crate::shot_document::generation(world),
        before,
        after: list,
        gesture,
    };
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    if gesture.is_none() {
        undo.seal();
    }
    undo.push(Box::new(command));
    if gesture.is_none() {
        undo.seal();
    }
    Ok(selected)
}

/// The keys of `track` of `camera` as the agent methods report them.
#[must_use]
pub fn keys_json(camera: &ShotCamera, track: PoseTrack) -> Value {
    match track {
        PoseTrack::Position => json!(
            camera
                .path
                .keys
                .iter()
                .map(|k| json!({ "time": k.time, "value": k.position }))
                .collect::<Vec<_>>()
        ),
        PoseTrack::Rotation => json!(
            camera
                .rotation
                .iter()
                .map(|k| json!({ "time": k.time, "value": k.rotation, "ease": format!("{:?}", k.ease) }))
                .collect::<Vec<_>>()
        ),
    }
}

fn floats<const N: usize>(params: &Value, what: &str) -> Result<[f32; N], regia_agent::protocol::RpcError> {
    let bad = || regia_agent::protocol::RpcError::invalid_params(format!("`value` must be {what}"));
    let list = params.get("value").and_then(Value::as_array).ok_or_else(bad)?;
    if list.len() != N {
        return Err(bad());
    }
    let mut out = [0.0; N];
    for (slot, v) in out.iter_mut().zip(list) {
        *slot = v.as_f64().filter(|f| f.is_finite()).ok_or_else(bad)? as f32;
    }
    Ok(out)
}

/// The pose edit an `editor.shot_panel_edit` call on `track` asks for (`op` add, move or remove;
/// `value` `[x, y, z]` or `[x, y, z, w]`).
///
/// # Errors
/// `invalid_params` for a missing or malformed field.
pub fn edit_of_params(
    params: &Value,
    track: PoseTrack,
) -> Result<PoseKeyEdit, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`op` is required: add, move or remove"))?;
    let time = || -> Result<f32, RpcError> {
        params
            .get("time")
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite())
            .map(|f| f as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`time` is required for `{op}`")))
    };
    let value = || -> Result<PoseValue, RpcError> {
        Ok(match track {
            PoseTrack::Position => {
                PoseValue::Position(floats::<3>(params, "[x, y, z] for the position track")?)
            }
            PoseTrack::Rotation => PoseValue::Rotation(floats::<4>(
                params,
                "a quaternion [x, y, z, w] for the rotation track",
            )?),
        })
    };
    let index = || -> Result<usize, RpcError> {
        params
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`index` must be a whole number (the key index)"))
    };
    match op {
        "add" => Ok(PoseKeyEdit::Add {
            time: time()?,
            value: value()?,
        }),
        "move" => Ok(PoseKeyEdit::Move {
            index: index()?,
            time: time()?,
            value: value()?,
        }),
        "remove" => Ok(PoseKeyEdit::Remove { index: index()? }),
        other => Err(RpcError::invalid_params(format!(
            "unknown `op` `{other}` for a pose track (add, move, remove)"
        ))),
    }
}

/// The time of the playhead in `panel`'s shot (seconds into it), when the clock is inside it.
#[must_use]
pub fn playhead(world: &World, panel: &ShotPanel, sequence: &ShotSequence) -> Option<f32> {
    let shot = sequence.shots.get(panel.shot)?;
    match crate::shot_document::host(world) {
        ShotHost::ShotPlayer => world
            .get_resource::<ShotPlayer>()
            .and_then(ShotPlayer::sample)
            .filter(|s| s.shot == panel.shot)
            .map(|s| s.local_time),
        ShotHost::Player(e) => world.get::<SequencePlayer>(e).and_then(|p| {
            let local = p.time() - sequence.shot_start(panel.shot);
            (0.0..=f64::from(shot.duration))
                .contains(&local)
                .then_some(local as f32)
        }),
    }
}

/// The pose a key added at `time` of `panel`'s shot holds: the preview's eye for the position
/// track, its orientation before roll for the rotation track (the keyed orientation, else the
/// look-at rule's).
#[must_use]
pub fn pose_at(sequence: &ShotSequence, panel: &ShotPanel, track: PoseTrack, time: f32) -> Option<PoseValue> {
    let shot = sequence.shots.get(panel.shot)?;
    let sample = sequence.sample(sequence.shot_start(panel.shot) + f64::from(time))?;
    Some(match track {
        PoseTrack::Position => PoseValue::Position(sample.eye.to_array()),
        PoseTrack::Rotation => {
            let q = sample_rotation(&shot.camera.rotation, time)
                .unwrap_or_else(|| look_rotation(sample.eye, sample.target, Vec3::Y, 0.0).0);
            PoseValue::Rotation(q.normalize().to_array())
        }
    })
}

/// Yaw and pitch (degrees) of a rotation key, for its fields.
fn yaw_pitch(q: [f32; 4]) -> (f32, f32) {
    let (yaw, pitch, _) = Quat::from_array(q).normalize().to_euler(EulerRot::YXZ);
    (yaw.to_degrees(), pitch.to_degrees())
}

fn from_yaw_pitch(yaw: f32, pitch: f32) -> [f32; 4] {
    Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), pitch.to_radians(), 0.0).to_array()
}

/// Draws the pose track `track` of `panel`'s shot: the path from above with its keys, and the key
/// list with its fields, Add key at playhead and Delete key.
pub fn draw(
    ui: &mut egui::Ui,
    world: &mut World,
    panel: &mut ShotPanel,
    sequence: &ShotSequence,
    track: PoseTrack,
) {
    let mut pose = world.remove_resource::<ShotPosePanel>().unwrap_or_default();
    let Some(shot) = sequence.shots.get(panel.shot).cloned() else {
        world.insert_resource(pose);
        return;
    };
    let frame = ui.ctx().cumulative_frame_nr();
    let playhead = playhead(world, panel, sequence);
    plot(ui, &shot.camera, shot.duration, track, pose.selected, playhead);
    let mut edit: Option<(PoseKeyEdit, Option<u64>)> = None;
    let mut any_drag = false;
    let len = match track {
        PoseTrack::Position => shot.camera.path.keys.len(),
        PoseTrack::Rotation => shot.camera.rotation.len(),
    };
    egui::ScrollArea::vertical()
        .id_salt("shot_pose_keys")
        .max_height(140.0)
        .show(ui, |ui| {
            for index in 0..len {
                ui.horizontal(|ui| {
                    let chosen = pose.selected == Some(index);
                    if ui.radio(chosen, format!("#{index}")).clicked() {
                        pose.selected = Some(index);
                    }
                    let (mut time, value) = match track {
                        PoseTrack::Position => {
                            let k = &shot.camera.path.keys[index];
                            (k.time, PoseValue::Position(k.position))
                        }
                        PoseTrack::Rotation => {
                            let k = &shot.camera.rotation[index];
                            (k.time, PoseValue::Rotation(k.rotation))
                        }
                    };
                    let mut changed = false;
                    let mut dragging = false;
                    let mut field = |ui: &mut egui::Ui, v: &mut f32, prefix: &str, speed: f64| {
                        let r = ui.add(egui::DragValue::new(v).speed(speed).prefix(prefix));
                        changed |= r.changed();
                        dragging |= r.dragged();
                    };
                    field(ui, &mut time, "t ", 0.01);
                    let value = match value {
                        PoseValue::Position(mut p) => {
                            for (v, axis) in p.iter_mut().zip(["x ", "y ", "z "]) {
                                field(ui, v, axis, 0.05);
                            }
                            PoseValue::Position(p)
                        }
                        PoseValue::Rotation(q) => {
                            let (mut yaw, mut pitch) = yaw_pitch(q);
                            let (y0, p0) = (yaw, pitch);
                            field(ui, &mut yaw, "yaw ", 0.5);
                            field(ui, &mut pitch, "pitch ", 0.5);
                            if yaw == y0 && pitch == p0 {
                                PoseValue::Rotation(q)
                            } else {
                                PoseValue::Rotation(from_yaw_pitch(yaw, pitch))
                            }
                        }
                    };
                    any_drag |= dragging;
                    if changed {
                        pose.selected = Some(index);
                        if dragging && pose.gesture.is_none() {
                            pose.gestures += 1;
                            pose.gesture = Some(pose.gestures);
                        }
                        let gesture = if dragging { pose.gesture } else { None };
                        edit = Some((PoseKeyEdit::Move { index, time, value }, gesture));
                    }
                });
            }
        });
    if !any_drag {
        pose.gesture = None;
    }
    ui.horizontal(|ui| {
        let at = playhead.unwrap_or(0.0);
        let add = crate::agent_parity::action_enabled(
            ui,
            playhead.is_some(),
            &crate::agent_parity::SHOTS_POSE_KEY_ADD,
            None,
        )
        .on_hover_text("Key the preview's pose at the playhead");
        if add.clicked()
            && pose.acted != Some(frame)
            && let Some(value) = pose_at(sequence, panel, track, at)
        {
            pose.acted = Some(frame);
            edit = Some((PoseKeyEdit::Add { time: at, value }, None));
        }
        let delete = crate::agent_parity::action_enabled(
            ui,
            pose.selected.is_some_and(|i| i < len),
            &crate::agent_parity::SHOTS_POSE_KEY_DELETE,
            None,
        );
        if delete.clicked()
            && pose.acted != Some(frame)
            && let Some(index) = pose.selected
        {
            pose.acted = Some(frame);
            edit = Some((PoseKeyEdit::Remove { index }, None));
        }
    });
    if let Some((edit, gesture)) = edit {
        match edit_pose(world, panel, track, edit, gesture) {
            Ok(selected) => {
                pose.selected = selected;
                panel.error = None;
            }
            Err(e) => panel.error = Some(e.message),
        }
    }
    world.insert_resource(pose);
}

/// The path from above: the sampled path, its keys (the selected one larger), the playhead's eye
/// and, for the rotation track, the view direction at each rotation key.
fn plot(
    ui: &mut egui::Ui,
    camera: &ShotCamera,
    duration: f32,
    track: PoseTrack,
    selected: Option<usize>,
    playhead: Option<f32>,
) {
    const STEPS: usize = 64;
    let size = egui::vec2(ui.available_width().max(120.0), 140.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let points: Vec<Vec3> = (0..=STEPS)
        .map(|i| camera.path.position(duration * i as f32 / STEPS as f32))
        .collect();
    let (mut min, mut max) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for p in &points {
        min = min.min(*p);
        max = max.max(*p);
    }
    let span = (max.x - min.x).max(max.z - min.z).max(1.0e-3);
    let centre = (min + max) * 0.5;
    let scale = (rect.width().min(rect.height()) - 16.0).max(8.0) / span;
    let to_screen = |p: Vec3| rect.center() + egui::vec2((p.x - centre.x) * scale, (p.z - centre.z) * scale);
    let stroke = egui::Stroke::new(1.5, ui.visuals().text_color());
    painter.add(egui::Shape::line(
        points.iter().map(|p| to_screen(*p)).collect(),
        stroke,
    ));
    let accent = ui.visuals().selection.bg_fill;
    for (i, key) in camera.path.keys.iter().enumerate() {
        let on = track == PoseTrack::Position && selected == Some(i);
        painter.circle_filled(
            to_screen(Vec3::from_array(key.position)),
            if on { 5.0 } else { 3.0 },
            accent,
        );
    }
    if track == PoseTrack::Rotation {
        for (i, key) in camera.rotation.iter().enumerate() {
            let eye = camera.path.position(key.time);
            let forward = Quat::from_array(key.rotation).normalize() * Vec3::NEG_Z;
            let tip = to_screen(eye) + egui::vec2(forward.x, forward.z) * 18.0;
            let width = if selected == Some(i) { 3.0 } else { 1.5 };
            painter.line_segment([to_screen(eye), tip], egui::Stroke::new(width, accent));
            painter.circle_filled(to_screen(eye), 2.5, accent);
        }
    }
    if let Some(t) = playhead {
        painter.circle_stroke(to_screen(camera.path.position(t)), 6.0, stroke);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shot_panel::shot_panel_edit;

    const SHOTS: &str = "assets/cine/fly.shots.ron";
    /// Shot `fly` (4 s): a Catmull-Rom path through three keys, looking at a point, no rotation
    /// keys; shot `hold` (2 s) after it.
    const TEXT: &str = r#"(version: 2, name: "fly", fps: 30.0, shots: [
        (name: "fly", duration: 4.0, camera: (path: (keys: [
            (time: 0.0, position: (0.0, 1.0, 5.0)), (time: 2.0, position: (2.0, 1.5, 4.0)),
            (time: 4.0, position: (4.0, 1.0, 5.0))], look_at: Point((0.0, 0.0, 0.0))))),
        (name: "hold", duration: 2.0, camera: (path: (keys: [(time: 0.0, position: (0.0, 2.0, 6.0))]))),
    ])"#;

    fn project(tag: &str) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir(&format!("shot-pose-{tag}"));
        let path = root.join(SHOTS);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        regia_test_utils::write_atomic(&path, TEXT.as_bytes()).expect("file");
        root
    }

    fn opened(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world.insert_resource(ShotPlayer::default());
        crate::shot_document::open_shot_list(&mut world, SHOTS, false).expect("open");
        world
    }

    fn camera(world: &World, shot: usize) -> ShotCamera {
        crate::shot_document::loaded(world).expect("loaded").shots[shot]
            .camera
            .clone()
    }

    #[test]
    fn pose_edits_keep_the_key_list_rules_and_name_codes() {
        let list = ShotSequence::from_ron(TEXT).expect("parses");
        let mut cam = list.shots[0].camera.clone();
        let at = |time: f32, p: [f32; 3]| PoseKeyEdit::Add {
            time,
            value: PoseValue::Position(p),
        };
        let code = |r: Result<Option<usize>, PanelError>| r.err().map(|e| e.code);
        assert_eq!(
            apply_pose_key_edit(&mut cam, PoseTrack::Position, 4.0, at(1.0, [1.0, 1.0, 4.5])),
            Ok(Some(1)),
            "sorted in"
        );
        let before = cam.clone();
        let refused = [
            (
                PoseTrack::Position,
                at(1.0, [0.0; 3]),
                "shot_panel_key_time_taken",
            ),
            (PoseTrack::Position, at(4.5, [0.0; 3]), "shot_panel_out_of_range"),
            (
                PoseTrack::Position,
                at(3.0, [f32::NAN, 0.0, 0.0]),
                "shot_panel_out_of_range",
            ),
            (
                PoseTrack::Position,
                PoseKeyEdit::Add {
                    time: 3.0,
                    value: PoseValue::Rotation([0.0, 0.0, 0.0, 1.0]),
                },
                "shot_panel_bad_value",
            ),
            (
                PoseTrack::Rotation,
                PoseKeyEdit::Add {
                    time: 3.0,
                    value: PoseValue::Rotation([0.0; 4]),
                },
                "shot_panel_out_of_range",
            ),
            (PoseTrack::Rotation, at(3.0, [0.0; 3]), "shot_panel_bad_value"),
            (
                PoseTrack::Position,
                PoseKeyEdit::Remove { index: 9 },
                "shot_panel_bad_key",
            ),
            (
                PoseTrack::Position,
                PoseKeyEdit::Move {
                    index: 0,
                    time: 2.0,
                    value: PoseValue::Position([0.0; 3]),
                },
                "shot_panel_key_time_taken",
            ),
        ];
        for (track, edit, want) in refused {
            assert_eq!(
                code(apply_pose_key_edit(&mut cam, track, 4.0, edit)),
                Some(want),
                "{edit:?}"
            );
            assert_eq!(cam, before, "a refused edit changes nothing");
        }
        // The path keeps one key.
        let mut hold = list.shots[1].camera.clone();
        assert_eq!(
            code(apply_pose_key_edit(
                &mut hold,
                PoseTrack::Position,
                2.0,
                PoseKeyEdit::Remove { index: 0 }
            )),
            Some("shot_panel_last_key")
        );
        // Rotation keys are stored normalized; a move keeps the ease.
        let added = apply_pose_key_edit(
            &mut cam,
            PoseTrack::Rotation,
            4.0,
            PoseKeyEdit::Add {
                time: 1.0,
                value: PoseValue::Rotation([0.0, 0.0, 0.0, 2.0]),
            },
        );
        assert_eq!(added, Ok(Some(0)));
        assert_eq!(cam.rotation[0].rotation, [0.0, 0.0, 0.0, 1.0]);
        cam.rotation[0].ease = Ease::Smooth;
        let moved = apply_pose_key_edit(
            &mut cam,
            PoseTrack::Rotation,
            4.0,
            PoseKeyEdit::Move {
                index: 0,
                time: 3.0,
                value: PoseValue::Rotation(from_yaw_pitch(30.0, 0.0)),
            },
        );
        assert_eq!(moved, Ok(Some(0)));
        assert_eq!((cam.rotation[0].time, cam.rotation[0].ease), (3.0, Ease::Smooth));
    }

    /// Acceptance (3): keys added in the panel (the code its buttons and fields call) are one undo
    /// entry each, round-trip through the shot list file, and the preview pose at a key's time is
    /// the key (exact for the position, quaternion dot > 0.9999 for the rotation).
    #[test]
    fn panel_keys_round_trip_and_the_preview_pose_matches() {
        let root = project("round-trip");
        let mut world = opened(&root);
        let panel = ShotPanel::default();
        let position = [1.25, 1.75, 4.5];
        edit_pose(
            &mut world,
            &panel,
            PoseTrack::Position,
            PoseKeyEdit::Add {
                time: 1.0,
                value: PoseValue::Position(position),
            },
            None,
        )
        .expect("position key");
        let rotation = from_yaw_pitch(35.0, -12.0);
        edit_pose(
            &mut world,
            &panel,
            PoseTrack::Rotation,
            PoseKeyEdit::Add {
                time: 3.0,
                value: PoseValue::Rotation(rotation),
            },
            None,
        )
        .expect("rotation key");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            2,
            "one undo entry per key"
        );
        world.resource_scope(|world, mut undo: Mut<UndoStack>| assert!(undo.undo(world)));
        assert!(camera(&world, 0).rotation.is_empty(), "undone");
        world.resource_scope(|world, mut undo: Mut<UndoStack>| assert!(undo.redo(world)));
        assert_eq!(camera(&world, 0).rotation.len(), 1, "redone");
        crate::shot_document::save_shot_list(&mut world, None).expect("save");
        let text = std::fs::read_to_string(root.join(SHOTS)).expect("saved");
        let reread = ShotSequence::from_ron(&text).expect("parses");
        assert_eq!(
            &reread,
            crate::shot_document::loaded(&world).expect("loaded"),
            "round trip"
        );
        let mut player = ShotPlayer::default();
        player.load(reread);
        player.seek(1.0);
        let sample = player.sample().expect("sample").clone();
        assert_eq!(sample.eye.to_array(), position, "the preview eye is the key");
        player.seek(3.0);
        let sample = player.sample().expect("sample").clone();
        let dot = sample
            .rotation
            .normalize()
            .dot(Quat::from_array(rotation).normalize())
            .abs();
        assert!(dot > 0.9999, "the preview orientation is the key: dot {dot}");
        // Add key at playhead keys the preview's own pose, so the preview does not move.
        let sequence = crate::shot_document::loaded(&world).cloned().expect("loaded");
        let before = sequence.sample(2.5).expect("sample");
        let value = pose_at(&sequence, &panel, PoseTrack::Position, 2.5).expect("pose");
        edit_pose(
            &mut world,
            &panel,
            PoseTrack::Position,
            PoseKeyEdit::Add { time: 2.5, value },
            None,
        )
        .expect("key at playhead");
        let after = crate::shot_document::loaded(&world)
            .expect("loaded")
            .sample(2.5)
            .expect("sample");
        assert_eq!(after.eye, before.eye);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance (3): the agent op writes the same bytes as the panel's op.
    #[test]
    fn the_agent_op_writes_the_bytes_of_the_panel_op() {
        let rotation = from_yaw_pitch(-20.0, 8.0);
        let gui = {
            let root = project("gui");
            let mut world = opened(&root);
            let panel = ShotPanel::default();
            let edits = [
                (
                    PoseTrack::Position,
                    PoseKeyEdit::Add {
                        time: 1.0,
                        value: PoseValue::Position([1.0, 2.0, 3.0]),
                    },
                ),
                (
                    PoseTrack::Position,
                    PoseKeyEdit::Move {
                        index: 1,
                        time: 1.5,
                        value: PoseValue::Position([1.0, 2.5, 3.0]),
                    },
                ),
                (PoseTrack::Position, PoseKeyEdit::Remove { index: 2 }),
                (
                    PoseTrack::Rotation,
                    PoseKeyEdit::Add {
                        time: 0.5,
                        value: PoseValue::Rotation(rotation),
                    },
                ),
            ];
            for (track, edit) in edits {
                edit_pose(&mut world, &panel, track, edit, None).expect("gui edit");
            }
            crate::shot_document::save_shot_list(&mut world, None).expect("save");
            let bytes = std::fs::read(root.join(SHOTS)).expect("bytes");
            let _ = std::fs::remove_dir_all(&root);
            bytes
        };
        let agent = {
            let root = project("agent");
            let mut world = opened(&root);
            let calls = [
                json!({ "track": "position", "op": "add", "time": 1.0, "value": [1.0, 2.0, 3.0] }),
                json!({ "track": "position", "op": "move", "index": 1, "time": 1.5, "value": [1.0, 2.5, 3.0] }),
                json!({ "track": "position", "op": "remove", "index": 2 }),
                json!({ "track": "rotation", "op": "add", "time": 0.5, "value": rotation }),
            ];
            for call in calls {
                let out = shot_panel_edit(&mut world, &call).expect("agent edit");
                assert_eq!(out["applied"], true, "{out}");
            }
            let state = crate::shot_panel::shot_panel_state(&mut world, &json!({ "track": "rotation" }))
                .expect("state");
            assert_eq!(state["track"], "rotation");
            assert_eq!(state["keys"].as_array().map(Vec::len), Some(1), "{state}");
            assert_eq!(
                world.resource::<UndoStack>().undo_depth(),
                4,
                "one undo entry per call"
            );
            // A scalar track name goes back to the scalar keys.
            let state =
                crate::shot_panel::shot_panel_state(&mut world, &json!({ "track": "fov" })).expect("state");
            assert_eq!(state["track"], "fov");
            let bad = shot_panel_edit(
                &mut world,
                &json!({ "track": "position", "op": "add", "time": 1.0, "value": 3.0 }),
            );
            assert!(bad.is_err(), "a position needs [x, y, z]");
            crate::shot_document::save_shot_list(&mut world, None).expect("save");
            let bytes = std::fs::read(root.join(SHOTS)).expect("bytes");
            let _ = std::fs::remove_dir_all(&root);
            bytes
        };
        assert_eq!(String::from_utf8_lossy(&gui), String::from_utf8_lossy(&agent));
        assert_eq!(gui, agent);
    }
}
