// SPDX-License-Identifier: MPL-2.0
//! The sequencer timeline of the Sequence Curves tab (ADR-7322, book `agents/seq-curve-panel.md`):
//! a dope sheet of every keyed track, a marker row, a cut row with the shot list's fades, and a
//! transport, on one time ruler.
//!
//! - **Dope sheet**: every float, vec3 and bool track is a row of keys (a vec3 track opens into
//!   x, y and z rows). Click, Shift-click and box-select keys across tracks; drag to move them in
//!   time (frame snap unless **Snap** is off), Alt-drag to scale them about the playhead (section
//!   ends too); Delete, copy and paste at the playhead (also into another sequence: the clipboard
//!   is the panel's; a section with either end selected copies whole);
//!   **Open in curve editor** shows a selected track's curve below. Each gesture is one undo
//!   entry, through `regia_framework::sequencer::edit::timeline::apply_key_batch`, validated on a
//!   copy, so a refused edit changes nothing.
//! - **Markers**: add at the playhead, drag, rename, payload, delete, CSV import and export
//!   (`marker_sheet`).
//! - **Cuts and fades**: the shot list of the panel's player (or the `ShotPlayer`'s loaded list,
//!   as the Shots panel edits it) as bars: add a cut at the playhead from the selected or active
//!   camera, move a cut, trim the last shot, delete a cut, set a shot's fades; the fade curve is
//!   drawn over the bars. `.shots.ron` keeps cuts and fades (ADR-268).
//! - **Transport**: play, reverse, pause, stop, loop, an in/out play range (panel state, never
//!   written to a file), timecode or frames, wheel zoom and drag pan, the playhead followed while
//!   playing; keys Space, J/K/L, Home/End and `,` `.` while the pointer is over the timeline and
//!   no text field has focus. The edit world runs no sequencer schedule, so the panel drives the
//!   player's clock itself ([`tick`]).
//!
//! The agent methods `editor.seq_keys`, `editor.seq_marker_edit`, `editor.seq_cut_edit`,
//! `editor.seq_transport` and `editor.seq_timeline_state` run the same code.

use std::collections::BTreeSet;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::sequencer::edit::EditError;
use regia_framework::sequencer::edit::timeline::{
    CutEdit, CutPose, KeyBatch, KeyClip, KeyRef, MarkRef, apply_cut_edit, apply_key_batch, apply_mark_move,
    apply_mark_scale, copy_marks, cut_times, fade_curve, import_marker_csv, key_time, key_times, mark_time,
    marks_in_box, new_shot_list, ripple_shift, ripple_span, set_marker, snap_to_frame,
};
use regia_framework::sequencer::edit::tracks::{MarkerEdit, apply_marker_edit};
use regia_framework::sequencer::marker_sheet::markers_to_csv;
use regia_framework::sequencer::{Channel, Marker, Sequence, SequencePlayer};
use regia_framework::shots::{ShotPlayer, ShotSequence};
use regia_render::Camera;
use regia_scene::Transform;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack, downcast_to};
use crate::seq_curve_panel::{SeqCurvePanel, player_of};
use crate::viewport_agent::Handler;

/// Height of one dope-sheet row, points.
pub const ROW_HEIGHT: f32 = 18.0;
/// Width of the track-name column, points.
pub const NAME_WIDTH: f32 = 160.0;
const RULER_HEIGHT: f32 = 20.0;
const MARKER_ROW: f32 = 22.0;
const CUT_ROW: f32 = 26.0;
const KEY_RADIUS: f32 = 5.0;
const COMPONENTS: [&str; 3] = ["x", "y", "z"];

/// The visible time span: the time at the left edge and the zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeView {
    /// Seconds at the left edge of the key area.
    pub start: f32,
    /// Points per second.
    pub pixels_per_second: f32,
}

impl Default for TimeView {
    fn default() -> Self {
        Self {
            start: 0.0,
            pixels_per_second: 100.0,
        }
    }
}

impl TimeView {
    /// The x of `time` in an area whose left edge is `left`.
    #[must_use]
    pub fn x(&self, left: f32, time: f32) -> f32 {
        left + (time - self.start) * self.pixels_per_second
    }

    /// The time at `x` in an area whose left edge is `left`.
    #[must_use]
    pub fn time(&self, left: f32, x: f32) -> f32 {
        self.start + (x - left) / self.pixels_per_second
    }

    /// Zooms by `factor` keeping `anchor` (seconds) where it is.
    pub fn zoom(&mut self, factor: f32, anchor: f32) {
        let pps = (self.pixels_per_second * factor).clamp(2.0, 20_000.0);
        let left_secs = anchor - self.start;
        self.start = anchor - left_secs * self.pixels_per_second / pps;
        self.pixels_per_second = pps;
    }
}

/// How the transport shows time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeDisplay {
    /// `hh:mm:ss:ff` at the sequence's frame rate.
    #[default]
    Timecode,
    /// Frame numbers.
    Frames,
}

/// The transport: panel state only (never written to a file).
#[derive(Clone, Debug, PartialEq)]
pub struct Transport {
    /// The panel is advancing the clock.
    pub playing: bool,
    /// `1` forward, `-1` reverse (J).
    pub rate: f32,
    /// Wrap at the end of the play range instead of stopping.
    pub looping: bool,
    /// The in and out points, seconds (`None`: the whole sequence).
    pub range: Option<(f32, f32)>,
    /// Timecode or frames.
    pub display: TimeDisplay,
    /// Scroll the view to keep the playhead in sight while playing.
    pub follow: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playing: false,
            rate: 1.0,
            looping: false,
            range: None,
            display: TimeDisplay::Timecode,
            follow: true,
        }
    }
}

/// A running pointer gesture.
#[derive(Clone, Debug)]
enum Drag {
    /// Moving (or with `scale`, scaling about `pivot`) the selected marks (keys, section ends)
    /// from `origin`.
    Keys {
        origin: Sequence,
        marks: Vec<MarkRef>,
        start_time: f32,
        scale: bool,
        pivot: f32,
    },
    /// A selection box, screen points.
    Box {
        from: egui::Pos2,
        to: egui::Pos2,
        add: bool,
    },
    /// A marker being dragged.
    Marker(usize),
    /// A cut being dragged. With ripple on, every frame applies to `origin` (the shot list and
    /// sequence at the press), so each frame is one whole ripple from the press point.
    Cut { cut: usize, origin: Option<CutBase> },
    /// The end of a shot being dragged (with the same ripple origin as a cut).
    Trim { shot: usize, origin: Option<CutBase> },
    /// Scrubbing on the ruler.
    Scrub,
    /// Panning with the middle button.
    Pan,
}

/// The timeline's state; one resource shared by the egui panel and the agent methods.
#[derive(Resource, Debug)]
pub struct SeqTimeline {
    /// The selected marks: keys of keyed tracks and the starts and ends of sections.
    pub selection: Vec<MarkRef>,
    /// The copied keys (survives a change of player, so keys paste into another sequence).
    pub clipboard: Option<KeyClip>,
    /// Vec3 tracks (by name) whose x, y and z rows are open.
    pub expanded: BTreeSet<String>,
    /// Snap drags to frames.
    pub snap: bool,
    /// Cut moves and trims shift everything after the edit point (the toolbar's **Ripple**).
    pub ripple: bool,
    /// Pan and zoom.
    pub view: TimeView,
    /// Play state.
    pub transport: Transport,
    /// The selected marker.
    pub marker: Option<usize>,
    /// The selected shot of the cut row.
    pub shot: Option<usize>,
    /// The project-relative path of the marker CSV buttons.
    pub csv_path: String,
    /// The last refused edit.
    pub error: Option<String>,
    /// The running pointer gesture's undo group.
    pub gesture: Option<u64>,
    /// Counts gestures, so two never merge.
    pub gestures: u64,
    /// Dope-sheet rows drawn last frame (the rest are scrolled out of view and skipped).
    pub rows_drawn: usize,
    /// Where last frame drew each visible mark (keys and section ends), screen points (pointer
    /// tests aim at these).
    pub(crate) key_points: Vec<(MarkRef, egui::Pos2)>,
    /// Where last frame drew the cut row and the left edge of its time axis, screen points
    /// (pointer tests aim cut and trim drags at these).
    pub(crate) cut_row: Option<(egui::Rect, f32)>,
    drag: Option<Drag>,
}

impl Default for SeqTimeline {
    fn default() -> Self {
        Self {
            selection: Vec::new(),
            clipboard: None,
            expanded: BTreeSet::new(),
            snap: true,
            ripple: false,
            view: TimeView::default(),
            transport: Transport::default(),
            marker: None,
            shot: None,
            csv_path: String::new(),
            error: None,
            gesture: None,
            gestures: 0,
            rows_drawn: 0,
            key_points: Vec::new(),
            cut_row: None,
            drag: None,
        }
    }
}

impl SeqTimeline {
    fn begin_gesture(&mut self) {
        self.gestures += 1;
        self.gesture = Some(self.gestures);
    }
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// One undo entry of a timeline edit of a sequence (keys, markers): the sequence before and
/// after. Edits of one pointer gesture merge.
pub struct SeqTimelineEdit {
    player: Entity,
    label: &'static str,
    before: Sequence,
    after: Sequence,
    gesture: Option<u64>,
}

impl EditorCommand for SeqTimelineEdit {
    fn apply(&mut self, world: &mut World) {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.after.clone());
        }
        crate::seq_tracks::refresh_preview(world);
    }

    fn revert(&mut self, world: &mut World) {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.before.clone());
        }
        crate::seq_tracks::refresh_preview(world);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SeqTimelineEdit>(next) else {
            return false;
        };
        if self.gesture.is_some() && self.gesture == next.gesture && self.player == next.player {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        self.label.to_string()
    }
}

/// Where a shot list lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutHost {
    /// A sequence player's shot list (the camera takeover plays it).
    Player(Entity),
    /// The `ShotPlayer` resource's loaded sequence (`render.sequence`, the Shots panel).
    ShotPlayer,
}

impl CutHost {
    fn name(self) -> &'static str {
        match self {
            Self::Player(_) => "player",
            Self::ShotPlayer => "shot_player",
        }
    }
}

fn shots_of(world: &World, host: CutHost) -> Option<ShotSequence> {
    match host {
        CutHost::Player(e) => world.get::<SequencePlayer>(e).and_then(|p| p.shots().cloned()),
        CutHost::ShotPlayer => world
            .get_resource::<ShotPlayer>()
            .and_then(|p| p.sequence().cloned()),
    }
}

fn put_shots(world: &mut World, host: CutHost, shots: Option<ShotSequence>) {
    match host {
        CutHost::Player(e) => {
            if let Some(mut player) = world.get_mut::<SequencePlayer>(e) {
                player.set_shots(shots);
            }
        }
        CutHost::ShotPlayer => {
            if let Some(mut player) = world.get_resource_mut::<ShotPlayer>() {
                match shots {
                    Some(shots) => player.replace_sequence(shots),
                    None => player.clear(),
                }
            }
        }
    }
}

/// One undo entry of a cut or fade edit: the shot list before and after. Edits of one pointer
/// gesture merge. An entry on the `ShotPlayer`'s list belongs to the shot-list document it was
/// made on: once another list is opened (a new `crate::shot_document` generation) it does
/// nothing, so an undo never puts the old list into the new document (ADR-8730).
pub struct SeqCutsEdit {
    host: CutHost,
    before: Option<ShotSequence>,
    after: Option<ShotSequence>,
    gesture: Option<u64>,
    generation: u64,
}

impl SeqCutsEdit {
    fn current(&self, world: &World) -> bool {
        self.host != CutHost::ShotPlayer || crate::shot_document::generation(world) == self.generation
    }
}

impl EditorCommand for SeqCutsEdit {
    fn apply(&mut self, world: &mut World) {
        if self.current(world) {
            put_shots(world, self.host, self.after.clone());
        }
    }

    fn revert(&mut self, world: &mut World) {
        if self.current(world) {
            put_shots(world, self.host, self.before.clone());
        }
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SeqCutsEdit>(next) else {
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
        "Edit sequence cuts".to_string()
    }
}

fn push_undo(world: &mut World, command: Box<dyn EditorCommand>, gesture: Option<u64>) {
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    if gesture.is_none() {
        undo.seal();
    }
    undo.push(command);
    if gesture.is_none() {
        undo.seal();
    }
}

/// Runs `edit` on a copy of the panel's player's sequence (the edit validates), swaps it in and
/// records one undo entry (merged within the timeline's running gesture).
fn edit_sequence<T>(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &SeqTimeline,
    label: &'static str,
    edit: impl FnOnce(&mut Sequence) -> Result<T, EditError>,
) -> Result<T, EditError> {
    let Some(entity) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let before = player.sequence().clone();
    let mut next = before.clone();
    let out = edit(&mut next)?;
    if next == before {
        return Ok(out);
    }
    // A locked track keeps its keys (`.seq.ron` v6, ADR-8105).
    regia_framework::sequencer::edit::tracks::check_locked(&before, &next)?;
    if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
        player.replace_sequence(next.clone());
    }
    panel.player = Some(entity);
    crate::seq_tracks::refresh_preview(world);
    push_undo(
        world,
        Box::new(SeqTimelineEdit {
            player: entity,
            label,
            before,
            after: next,
            gesture: timeline.gesture,
        }),
        timeline.gesture,
    );
    Ok(out)
}

/// Applies a batch key edit to the panel's player as one undo entry (or part of the running
/// gesture's) and selects the keys it reports.
pub fn edit_keys(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    batch: &KeyBatch,
) -> Result<Vec<KeyRef>, EditError> {
    let label = match batch {
        KeyBatch::Move { .. } => "Move sequence keys",
        KeyBatch::Scale { .. } => "Scale sequence keys",
        KeyBatch::Delete { .. } => "Delete sequence keys",
        KeyBatch::Paste { .. } => "Paste sequence keys",
    };
    let selected = edit_sequence(world, panel, timeline, label, |s| apply_key_batch(s, batch))?;
    timeline.selection = selected.iter().map(|k| MarkRef::Key(*k)).collect();
    Ok(selected)
}

/// Moves the given marks (keys of keyed tracks, section starts and ends) by `delta` seconds as
/// one undo entry (or part of the running gesture's), on the validated copy the dope sheet and
/// the section strip share (`apply_mark_move`). Selects the marks at their new places. Codes:
/// those of `apply_mark_move` (`seq_keys_*`, `seq_section_*`).
pub fn edit_marks_move(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    marks: &[MarkRef],
    delta: f32,
) -> Result<Vec<MarkRef>, EditError> {
    let selected = edit_sequence(world, panel, timeline, "Move sequence keys", |s| {
        apply_mark_move(s, marks, delta)
    })?;
    timeline.selection.clone_from(&selected);
    Ok(selected)
}

/// Scales the given marks (keys, section starts and ends) about `pivot` by `factor` as one undo
/// entry (or part of the running gesture's), on one validated copy (`apply_mark_scale`): a section
/// with both ends selected stretches, one with one end selected trims it. Selects the marks at
/// their new places. Codes: those of `apply_mark_scale` (`seq_keys_*`, `seq_section_*`).
pub fn edit_marks_scale(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    marks: &[MarkRef],
    pivot: f32,
    factor: f32,
) -> Result<Vec<MarkRef>, EditError> {
    let selected = edit_sequence(world, panel, timeline, "Scale sequence keys", |s| {
        apply_mark_scale(s, marks, pivot, factor)
    })?;
    timeline.selection.clone_from(&selected);
    Ok(selected)
}

/// The panel's player, its clock and its sequence.
fn player_state(world: &mut World, panel: &SeqCurvePanel) -> Option<(Entity, f32, Sequence, f32)> {
    let entity = player_of(world, panel)?;
    let player = world.get::<SequencePlayer>(entity)?;
    Some((
        entity,
        player.time() as f32,
        player.sequence().clone(),
        player.duration() as f32,
    ))
}

fn fps_of(sequence: &Sequence) -> f32 {
    if sequence.fps.is_finite() && sequence.fps > 0.0 {
        sequence.fps
    } else {
        30.0
    }
}

/// `time` as `hh:mm:ss:ff` at `fps`.
#[must_use]
pub fn timecode(time: f32, fps: f32) -> String {
    let fps_whole = fps.round().max(1.0) as u64;
    let frames = (f64::from(time.max(0.0)) * f64::from(fps)).round() as u64;
    let (seconds, frame) = (frames / fps_whole, frames % fps_whole);
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        frame
    )
}

// ---------------------------------------------------------------- markers

/// One marker edit of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkerOp {
    /// Add a marker (default name `marker<n>`) at `time` with `payload`.
    Add {
        /// The name (`None`: the next free `marker<n>`).
        name: Option<String>,
        /// Seconds.
        time: f32,
        /// The payload.
        payload: String,
    },
    /// Move marker `index` to `time`.
    Move {
        /// Marker index.
        index: usize,
        /// Seconds.
        time: f32,
    },
    /// Rename marker `index`.
    Rename {
        /// Marker index.
        index: usize,
        /// The new unique name.
        name: String,
    },
    /// Set marker `index`'s payload.
    Payload {
        /// Marker index.
        index: usize,
        /// The payload.
        payload: String,
    },
    /// Remove marker `index`.
    Delete {
        /// Marker index.
        index: usize,
    },
    /// Replace the markers from a marker CSV.
    ImportCsv {
        /// The sheet.
        csv: String,
    },
}

fn free_marker_name(sequence: &Sequence) -> String {
    let mut n = sequence.markers.len() + 1;
    loop {
        let name = format!("marker{n}");
        if sequence.markers.iter().all(|m| m.name != name) {
            return name;
        }
        n += 1;
    }
}

/// Applies a marker edit as one undo entry (or part of the running gesture's). Returns the
/// marker's index (the marker count after an import). Codes: `seq_curve_no_player`, the
/// `seq_marker_*` codes of the edit.
pub fn edit_marker(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    op: MarkerOp,
) -> Result<usize, EditError> {
    let out = edit_sequence(world, panel, timeline, "Edit sequence markers", |s| match op {
        MarkerOp::Add { name, time, payload } => {
            let name = name.unwrap_or_else(|| free_marker_name(s));
            apply_marker_edit(
                s,
                MarkerEdit::Add {
                    at: None,
                    marker: Marker { name, time, payload },
                },
            )
        }
        MarkerOp::Move { index, time } => set_marker(s, index, Some(time), None),
        MarkerOp::Rename { index, name } => apply_marker_edit(s, MarkerEdit::Rename { index, name }),
        MarkerOp::Payload { index, payload } => set_marker(s, index, None, Some(&payload)),
        MarkerOp::Delete { index } => apply_marker_edit(s, MarkerEdit::Remove { index }),
        MarkerOp::ImportCsv { csv } => import_marker_csv(s, &csv),
    })?;
    Ok(out)
}

/// The panel's player's markers as a marker CSV (`marker_sheet`).
pub fn export_markers(world: &mut World, panel: &SeqCurvePanel) -> Result<String, EditError> {
    let Some((_, _, sequence, _)) = player_state(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    Ok(markers_to_csv(&sequence.markers))
}

/// `path` under the project root, ending in `.csv`, without `..`. Codes: `seq_marker_bad_path`,
/// `seq_curve_no_project`.
fn csv_file(world: &World, path: &str) -> Result<std::path::PathBuf, EditError> {
    let relative = std::path::Path::new(path);
    if !path.ends_with(".csv")
        || !relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    {
        return refuse(
            "seq_marker_bad_path",
            format!("`{path}` must be a project-relative .csv path without `..`"),
        );
    }
    let Some(root) = crate::seq_curve_panel::project_root(world) else {
        return refuse("seq_curve_no_project", "no project is open");
    };
    Ok(root.join(relative))
}

/// Writes the markers CSV to a project file (atomic). Codes: those of [`export_markers`],
/// `seq_marker_bad_path`, `seq_curve_no_project`, `seq_marker_io`.
pub fn export_markers_to(world: &mut World, panel: &SeqCurvePanel, path: &str) -> Result<String, EditError> {
    let csv = export_markers(world, panel)?;
    let file = csv_file(world, path)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| EditError {
            code: "seq_marker_io",
            message: format!("cannot create {}: {e}", dir.display()),
        })?;
    }
    crate::level_io::write_atomic(&file, csv.as_bytes()).map_err(|message| EditError {
        code: "seq_marker_io",
        message,
    })?;
    Ok(csv)
}

/// Reads a project marker CSV. Codes: `seq_marker_bad_path`, `seq_curve_no_project`,
/// `seq_marker_io`.
pub fn read_marker_csv(world: &World, path: &str) -> Result<String, EditError> {
    let file = csv_file(world, path)?;
    std::fs::read_to_string(&file).map_err(|e| EditError {
        code: "seq_marker_io",
        message: format!("cannot read {path}: {e}"),
    })
}

// ---------------------------------------------------------------- cuts

/// The shot list the cut row edits: the panel's player's own list, else the `ShotPlayer`'s
/// loaded one.
pub fn cut_host(world: &mut World, panel: &SeqCurvePanel) -> Option<CutHost> {
    let player = player_of(world, panel);
    if let Some(e) = player
        && world
            .get::<SequencePlayer>(e)
            .is_some_and(|p| p.shots().is_some())
    {
        return Some(CutHost::Player(e));
    }
    if world
        .get_resource::<ShotPlayer>()
        .is_some_and(|p| p.sequence().is_some())
    {
        return Some(CutHost::ShotPlayer);
    }
    player.map(CutHost::Player)
}

/// The pose of `camera`, else of the primary selected entity when it is a camera, else of the
/// active camera (the highest `order`, then the lowest id).
pub fn camera_pose(world: &mut World, camera: Option<Entity>) -> Result<CutPose, EditError> {
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .filter(|e| world.get::<Camera>(*e).is_some());
    let entity = match camera.or(selected) {
        Some(e) => Some(e),
        None => {
            let mut query = world.query::<(Entity, &Camera)>();
            query
                .iter(world)
                .min_by_key(|(e, c)| (std::cmp::Reverse(c.order), e.to_bits()))
                .map(|(e, _)| e)
        }
    };
    let Some(entity) = entity else {
        return refuse("seq_cut_no_camera", "no camera to cut to: select one or add one");
    };
    let (Some(camera), Some(transform)) = (world.get::<Camera>(entity), world.get::<Transform>(entity))
    else {
        return refuse(
            "seq_cut_no_camera",
            format!("entity {} is not a camera", entity.to_bits()),
        );
    };
    Ok(CutPose {
        position: transform.translation.to_array(),
        rotation: transform.rotation.normalize().to_array(),
        fov_degrees: camera.fov_y.to_degrees().clamp(1.0, 170.0),
    })
}

/// One undo entry of a ripple edit (ADR-8640): the shot list and the sequence before and after,
/// so one undo restores both. Frames of one pointer gesture merge.
pub struct SeqRippleEdit {
    host: CutHost,
    player: Entity,
    shots_before: Option<ShotSequence>,
    shots_after: Option<ShotSequence>,
    sequence_before: Sequence,
    sequence_after: Sequence,
    gesture: Option<u64>,
    /// The shot-list document generation the entry was made on (see [`SeqCutsEdit`]): on the
    /// `ShotPlayer`'s list, an entry from an older document does nothing (ADR-8730).
    generation: u64,
}

impl SeqRippleEdit {
    fn current(&self, world: &World) -> bool {
        self.host != CutHost::ShotPlayer || crate::shot_document::generation(world) == self.generation
    }
}

impl EditorCommand for SeqRippleEdit {
    fn apply(&mut self, world: &mut World) {
        if !self.current(world) {
            return;
        }
        put_shots(world, self.host, self.shots_after.clone());
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.sequence_after.clone());
        }
        crate::seq_tracks::refresh_preview(world);
    }

    fn revert(&mut self, world: &mut World) {
        if !self.current(world) {
            return;
        }
        put_shots(world, self.host, self.shots_before.clone());
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.sequence_before.clone());
        }
        crate::seq_tracks::refresh_preview(world);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SeqRippleEdit>(next) else {
            return false;
        };
        if self.gesture.is_some()
            && self.gesture == next.gesture
            && self.host == next.host
            && self.generation == next.generation
        {
            self.shots_after = next.shots_after.clone();
            self.sequence_after = next.sequence_after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        "Ripple sequence cuts".to_string()
    }
}

/// Applies a cut edit to the cut row's shot list as one undo entry (or part of the running
/// gesture's). An add with no shot list creates one on the panel's player: one still shot of the
/// camera lasting the sequence, cut at `time` when `time` is inside it. With the edit's `ripple`
/// on, the panel player's keys and markers after the edge's old place ([`ripple_span`]) shift by
/// the edit's delta too, on the copy, and the whole edit lands as one undo entry
/// ([`SeqRippleEdit`]); a refused shift refuses the edit and changes nothing. An edit that does
/// not ripple is a plain cut entry (no sequence snapshot). Returns the edited shot. Codes:
/// `seq_curve_no_player`, `seq_cut_no_camera`, the `seq_cut_*` codes of the edit, and the
/// `seq_keys_*` and `seq_ripple_keys_in_span` codes of the ripple shift.
pub fn edit_cuts(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    edit: &CutEdit,
) -> Result<usize, EditError> {
    edit_cuts_from(world, panel, timeline, edit, None)
}

/// The shot list and sequence a rippling pointer drag started from.
type CutBase = (Option<ShotSequence>, Sequence);

/// [`edit_cuts`], with the edit applied to `base` (a rippling drag's press state) rather than to
/// the current shot list and sequence: each drag frame is one whole ripple from the press, so the
/// result does not depend on the mouse speed (w9 review finding 3). The undo entry still runs
/// from the current state, so the gesture's merged entry runs from the press to the last frame;
/// a refused frame leaves the last good one.
fn edit_cuts_from(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    edit: &CutEdit,
    base: Option<&CutBase>,
) -> Result<usize, EditError> {
    let Some(host) = cut_host(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player or loaded shot list");
    };
    let before = shots_of(world, host);
    let input = base.map_or_else(|| before.clone(), |(shots, _)| shots.clone());
    let ripple = input.as_ref().and_then(|list| ripple_span(list, edit));
    let player = player_of(world, panel);
    let sequence_before = player
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.sequence().clone());
    let sequence_input = match base {
        Some((_, sequence)) => sequence_before.as_ref().map(|_| sequence.clone()),
        None => sequence_before.clone(),
    };
    let (next, index) = match (&input, edit) {
        (Some(list), _) => {
            let mut next = list.clone();
            let index = apply_cut_edit(&mut next, edit)?;
            (next, index)
        }
        (None, CutEdit::Add { time, pose, .. }) => {
            let length = player_state(world, panel).map_or(0.0, |(_, _, s, d)| d.max(1.0 / fps_of(&s)));
            let fps = player_state(world, panel).map_or(30.0, |(_, _, s, _)| fps_of(&s));
            let mut next = new_shot_list(*pose, length.max(*time + 1.0 / fps), fps)?;
            let index = if *time > 0.0 {
                apply_cut_edit(&mut next, edit)?
            } else {
                0
            };
            (next, index)
        }
        (None, _) => return refuse("seq_cut_no_shots", "the player has no shot list: add a cut first"),
    };
    // The ripple's sequence shift is prepared on a copy first: it must land for the edit to.
    let sequence_after = match (sequence_input, ripple) {
        (Some(sequence), Some((point, delta))) => {
            let mut shifted = sequence.clone();
            ripple_shift(&mut shifted, point, delta)?;
            // A ripple never moves a locked track's keys (`.seq.ron` v6, ADR-8105).
            regia_framework::sequencer::edit::tracks::check_locked(&sequence, &shifted)?;
            Some(shifted)
        }
        (sequence, None) => sequence,
        (None, Some(_)) => None,
    };
    if before.as_ref() == Some(&next) && sequence_after == sequence_before {
        return Ok(index);
    }
    put_shots(world, host, Some(next.clone()));
    timeline.shot = Some(index);
    let generation = crate::shot_document::generation(world);
    // Only a rippling edit (or a frame of a rippling drag, which may put the press state's
    // sequence back) snapshots the sequence (w9 review finding 6).
    let rippling = ripple.is_some() || base.is_some();
    match (player, sequence_after) {
        (Some(entity), Some(after)) if rippling => {
            if let Some(mut p) = world.get_mut::<SequencePlayer>(entity) {
                p.replace_sequence(after.clone());
            }
            panel.player = Some(entity);
            crate::seq_tracks::refresh_preview(world);
            push_undo(
                world,
                Box::new(SeqRippleEdit {
                    host,
                    player: entity,
                    shots_before: before,
                    shots_after: Some(next),
                    sequence_before: sequence_before.unwrap_or_default(),
                    sequence_after: after,
                    gesture: timeline.gesture,
                    generation,
                }),
                timeline.gesture,
            );
        }
        _ => push_undo(
            world,
            Box::new(SeqCutsEdit {
                host,
                before,
                after: Some(next),
                gesture: timeline.gesture,
                generation,
            }),
            timeline.gesture,
        ),
    }
    Ok(index)
}

// ---------------------------------------------------------------- transport

/// One transport command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransportOp {
    /// Play forward (L).
    Play,
    /// Play in reverse (J).
    Reverse,
    /// Pause (K).
    Pause,
    /// Play or pause (Space).
    Toggle,
    /// Pause and go to the play range's start.
    Stop,
    /// Go to a time, seconds.
    Seek(f32),
    /// Step frames (`,` back, `.` forward); pauses.
    Step(i32),
    /// The play range's start (Home).
    Home,
    /// The play range's end (End).
    End,
    /// Loop on or off.
    Loop(bool),
    /// The in and out points (`None` clears them).
    Range(Option<(f32, f32)>),
    /// Timecode or frames.
    Display(TimeDisplay),
    /// Snap drags to frames on or off (panel state).
    Snap(bool),
    /// Keep the playhead in view while playing, on or off.
    Follow(bool),
    /// Advance the clock by a delta, seconds (what each frame of the panel does while playing).
    Tick(f32),
}

fn seek_all(world: &mut World, entity: Entity, time: f32) {
    if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
        player.seek(f64::from(time));
    }
    let follows = world
        .get::<SequencePlayer>(entity)
        .is_some_and(|p| p.shots().is_none());
    if follows && let Some(mut shots) = world.get_resource_mut::<ShotPlayer>() {
        shots.seek(f64::from(time));
    }
    crate::seq_tracks::refresh_preview(world);
}

/// Advances a playing transport by `dt` seconds: wraps in the play range when looping, else stops
/// at its end (or start, in reverse). Returns the new time.
pub fn tick(world: &mut World, panel: &SeqCurvePanel, timeline: &mut SeqTimeline, dt: f32) -> Option<f32> {
    let (entity, time, _, duration) = player_state(world, panel)?;
    if !timeline.transport.playing || !dt.is_finite() {
        return Some(time);
    }
    let (lo, hi) = timeline.transport.range.unwrap_or((0.0, duration));
    let span = hi - lo;
    let mut next = time + dt * timeline.transport.rate;
    if timeline.transport.rate > 0.0 && next >= hi {
        if timeline.transport.looping && span > 0.0 {
            next = lo + (next - hi) % span;
        } else {
            next = hi;
            timeline.transport.playing = false;
        }
    } else if timeline.transport.rate < 0.0 && next <= lo {
        if timeline.transport.looping && span > 0.0 {
            next = hi - (lo - next) % span;
        } else {
            next = lo;
            timeline.transport.playing = false;
        }
    }
    // Playing on: the preview's voices inside their sections keep playing (ADR-8940); reverse
    // play is silent (w11 item 1b).
    if timeline.transport.rate < 0.0 {
        crate::seq_tracks::mark_reverse_step(world);
    } else {
        crate::seq_tracks::mark_play_step(world);
    }
    seek_all(world, entity, next);
    if !timeline.transport.playing {
        // A non-looping play reached its end: the preview's voices stop with it.
        crate::seq_tracks::hold_sounds(world);
    }
    Some(next)
}

/// Runs a transport command. Codes: `seq_curve_no_player`, `seq_transport_bad_range` (in not
/// before out, a negative or non-finite point), `seq_transport_bad_time`.
pub fn transport(
    world: &mut World,
    panel: &SeqCurvePanel,
    timeline: &mut SeqTimeline,
    op: TransportOp,
) -> Result<f32, EditError> {
    let Some((entity, time, sequence, duration)) = player_state(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let fps = fps_of(&sequence);
    let (lo, hi) = timeline.transport.range.unwrap_or((0.0, duration));
    let t = &mut timeline.transport;
    let seek = match op {
        TransportOp::Play | TransportOp::Reverse => {
            t.rate = if op == TransportOp::Play { 1.0 } else { -1.0 };
            t.playing = true;
            // From the far end of the range, start over.
            if t.rate > 0.0 && time >= hi {
                Some(lo)
            } else if t.rate < 0.0 && time <= lo {
                Some(hi)
            } else {
                None
            }
        }
        TransportOp::Pause => {
            t.playing = false;
            None
        }
        TransportOp::Toggle => {
            if t.playing {
                t.playing = false;
                None
            } else {
                t.playing = true;
                t.rate = 1.0;
                (time >= hi).then_some(lo)
            }
        }
        TransportOp::Stop => {
            t.playing = false;
            Some(lo)
        }
        TransportOp::Seek(at) => {
            if !at.is_finite() {
                return refuse("seq_transport_bad_time", format!("time {at} must be finite"));
            }
            Some(at.clamp(0.0, duration.max(0.0)))
        }
        TransportOp::Step(frames) => {
            t.playing = false;
            let frame = (f64::from(time) * f64::from(fps)).round() + f64::from(frames);
            Some(((frame / f64::from(fps)) as f32).clamp(0.0, duration.max(0.0)))
        }
        TransportOp::Home => Some(lo),
        TransportOp::End => Some(hi),
        TransportOp::Loop(on) => {
            t.looping = on;
            None
        }
        TransportOp::Range(range) => {
            if let Some((a, b)) = range
                && !(a.is_finite() && b.is_finite() && a >= 0.0 && a < b)
            {
                return refuse(
                    "seq_transport_bad_range",
                    format!("in {a} must be zero or more and before out {b}"),
                );
            }
            t.range = range;
            None
        }
        TransportOp::Display(display) => {
            t.display = display;
            None
        }
        TransportOp::Follow(on) => {
            t.follow = on;
            None
        }
        TransportOp::Snap(on) => {
            timeline.snap = on;
            None
        }
        TransportOp::Tick(dt) => {
            if !(dt.is_finite() && dt >= 0.0) {
                return refuse("seq_transport_bad_time", format!("dt {dt} must be zero or more"));
            }
            return Ok(tick(world, panel, timeline, dt).unwrap_or(time));
        }
    };
    if let Some(at) = seek {
        seek_all(world, entity, at);
    }
    // Pause, Stop and Space/K off stop the preview's voices with the clock; Play starts them
    // again at the playhead (sprint-w9-rev-v06-lanes § 1 finding 1).
    if matches!(op, TransportOp::Pause | TransportOp::Stop | TransportOp::Toggle)
        && !timeline.transport.playing
    {
        crate::seq_tracks::hold_sounds(world);
    }
    Ok(seek.unwrap_or(time))
}

// ---------------------------------------------------------------- rows

/// One dope-sheet row: a keyed track, or one component of an open vec3 track.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Track index.
    pub track: usize,
    /// `Some(c)` for component `c` of an open vec3 track.
    pub component: Option<usize>,
    /// What the name column shows.
    pub label: String,
    /// `true` for a vec3 track's own row (it opens and closes).
    pub opens: bool,
}

/// The dope-sheet rows of `sequence`: every keyed track and the x, y, z rows of the vec3 tracks
/// named in `expanded`, plus one row per clip, audio, sub-sequence or video section track
/// (ADR-8640: its sections' starts and ends are selectable marks) and one per event track (its
/// keys, ADR-8628).
#[must_use]
pub fn rows(sequence: &Sequence, expanded: &BTreeSet<String>) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, track) in sequence.tracks.iter().enumerate() {
        let vec3 = matches!(track.keys, Channel::Vec3(_));
        if !track.keys.is_keyed() {
            // Section tracks draw their sections' marks; an event track draws its keys
            // (ADR-8628: event keys box-select, copy, paste and ripple like the other keys).
            if !matches!(
                track.keys,
                Channel::Clip(_)
                    | Channel::Audio(_)
                    | Channel::Sub(_)
                    | Channel::Video(_)
                    | Channel::Event(_)
            ) {
                continue;
            }
            out.push(Row {
                track: i,
                component: None,
                label: format!("{} ({})", track.name, track.keys.kind()),
                opens: false,
            });
            continue;
        }
        out.push(Row {
            track: i,
            component: None,
            label: format!("{} ({})", track.name, track.keys.kind()),
            opens: vec3,
        });
        if vec3 && expanded.contains(&track.name) {
            for (c, name) in COMPONENTS.iter().enumerate() {
                out.push(Row {
                    track: i,
                    component: Some(c),
                    label: format!("    {name}"),
                    opens: false,
                });
            }
        }
    }
    out
}

/// The keys of `tracks` whose times are in `[from, to]`: a box selection over those rows.
#[must_use]
pub fn keys_in_box(sequence: &Sequence, tracks: &[usize], from: f32, to: f32) -> Vec<KeyRef> {
    let (lo, hi) = (from.min(to), from.max(to));
    let mut out: Vec<KeyRef> = tracks
        .iter()
        .flat_map(|&track| {
            key_times(sequence, track)
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .filter(move |(_, t)| *t >= lo && *t <= hi)
                .map(move |(index, _)| KeyRef { track, index })
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------- the panel

/// Draws the timeline as a collapsible section of the Sequence Curves tab.
pub fn timeline_section(ui: &mut egui::Ui, panel: &mut SeqCurvePanel, world: &mut World) {
    egui::CollapsingHeader::new("Timeline")
        .id_salt("seq_timeline")
        .default_open(true)
        .show(ui, |ui| {
            let mut timeline = world.remove_resource::<SeqTimeline>().unwrap_or_default();
            draw(ui, panel, &mut timeline, world);
            world.insert_resource(timeline);
        });
}

/// Draws the timeline on its own (headless tests and hosts that dock it separately).
pub fn seq_timeline_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let mut timeline = world.remove_resource::<SeqTimeline>().unwrap_or_default();
    draw(ui, &mut panel, &mut timeline, world);
    world.insert_resource(timeline);
    world.insert_resource(panel);
}

fn keep(timeline: &mut SeqTimeline, result: Result<impl Sized, EditError>) {
    match result {
        Ok(_) => timeline.error = None,
        Err(e) => timeline.error = Some(e.message),
    }
}

fn draw(ui: &mut egui::Ui, panel: &mut SeqCurvePanel, timeline: &mut SeqTimeline, world: &mut World) {
    let _span = tracing::info_span!(target: "regia_editor", "seq_timeline").entered();
    // The panel drives the clock while playing (the edit world runs no sequencer schedule).
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    if timeline.transport.playing {
        tick(world, panel, timeline, dt);
        ui.ctx().request_repaint();
    }
    let Some((_, playhead, sequence, duration)) = player_state(world, panel) else {
        ui.label("No sequence player: open or create a sequence above.");
        return;
    };
    let fps = fps_of(&sequence);
    toolbar(ui, panel, timeline, world, playhead, fps);
    marker_fields(ui, panel, timeline, world);
    let host = cut_host(world, panel);
    let shots = host.and_then(|h| shots_of(world, h));
    let rows = rows(&sequence, &timeline.expanded);
    timeline.selection.retain(|m| mark_time(&sequence, *m).is_some());

    let width = ui.available_width().max(NAME_WIDTH + 50.0);
    let header_height = RULER_HEIGHT + MARKER_ROW + CUT_ROW;
    let (header, header_response) =
        ui.allocate_exact_size(egui::vec2(width, header_height), egui::Sense::click_and_drag());
    let left = header.left() + NAME_WIDTH;
    let key_width = header.right() - left;
    // Follow the playhead while playing.
    if timeline.transport.playing && timeline.transport.follow {
        let visible = key_width / timeline.view.pixels_per_second;
        if playhead < timeline.view.start || playhead > timeline.view.start + visible {
            timeline.view.start = playhead - visible * 0.1;
        }
    }
    zoom_and_pan(ui, timeline, header.union(ui.max_rect()), left);
    let painter = ui.painter_at(header);
    let ruler = egui::Rect::from_min_size(header.min, egui::vec2(width, RULER_HEIGHT));
    let marker_rect = egui::Rect::from_min_size(
        egui::pos2(header.left(), ruler.bottom()),
        egui::vec2(width, MARKER_ROW),
    );
    let cut_rect = egui::Rect::from_min_size(
        egui::pos2(header.left(), marker_rect.bottom()),
        egui::vec2(width, CUT_ROW),
    );
    draw_ruler(
        &painter,
        ruler,
        left,
        &timeline.view,
        fps,
        timeline.transport.range,
    );
    painter.text(
        egui::pos2(marker_rect.left() + 4.0, marker_rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Markers",
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
    );
    painter.text(
        egui::pos2(cut_rect.left() + 4.0, cut_rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Cuts",
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
    );
    draw_markers(&painter, marker_rect, left, timeline, &sequence);
    if let Some(shots) = &shots {
        draw_cuts(&painter, cut_rect, left, timeline, shots);
    }
    timeline.cut_row = Some((cut_rect, left));
    let layout = Header {
        ruler,
        markers: marker_rect,
        cuts: cut_rect,
        left,
        sequence: &sequence,
        shots: shots.as_ref(),
    };
    header_gesture(ui, &header_response, panel, timeline, world, &layout);

    // The key rows, virtualized: only rows in view are laid out and painted.
    let mut hits: Vec<(MarkRef, egui::Pos2)> = Vec::new();
    let mut drawn = 0;
    let mut toggled: Option<String> = None;
    let mut row_responses: Vec<egui::Response> = Vec::new();
    let mut row_rects: Vec<(usize, egui::Rect)> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("seq_timeline_rows")
        .max_height(ui.available_height().clamp(ROW_HEIGHT * 4.0, 600.0))
        .auto_shrink([false, true])
        .scroll_source(egui::scroll_area::ScrollSource {
            drag: egui::scroll_area::DragScroll::Never,
            ..Default::default()
        })
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for row in &rows[range] {
                drawn += 1;
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click_and_drag());
                let painter = ui.painter_at(rect);
                if row.track.is_multiple_of(2) {
                    painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                }
                let name_rect = egui::Rect::from_min_max(rect.min, egui::pos2(left, rect.max.y));
                let mut label = row.label.clone();
                if row.opens {
                    let open = sequence
                        .tracks
                        .get(row.track)
                        .is_some_and(|t| timeline.expanded.contains(&t.name));
                    label = format!("{} {label}", if open { "v" } else { ">" });
                    if response.clicked()
                        && response
                            .interact_pointer_pos()
                            .is_some_and(|p| name_rect.contains(p))
                    {
                        toggled = sequence.tracks.get(row.track).map(|t| t.name.clone());
                    }
                }
                painter.text(
                    egui::pos2(name_rect.left() + 4.0, name_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(12.0),
                    ui.visuals().text_color(),
                );
                let times = key_times(&sequence, row.track).unwrap_or_default();
                let visible_end = timeline.view.time(left, rect.right());
                for (index, time) in times.iter().enumerate() {
                    if *time < timeline.view.start - 1.0 || *time > visible_end + 1.0 {
                        continue;
                    }
                    let key = KeyRef {
                        track: row.track,
                        index,
                    };
                    let center = egui::pos2(timeline.view.x(left, *time), rect.center().y);
                    if center.x < left {
                        continue;
                    }
                    let selected = timeline.selection.contains(&MarkRef::Key(key));
                    diamond(&painter, center, selected, row.component.is_some());
                    hits.push((MarkRef::Key(key), center));
                }
                // A section track's row draws its sections' starts and ends as selectable marks.
                if let Some(spans) =
                    regia_framework::sequencer::sections::section_spans(&sequence.tracks[row.track].keys)
                {
                    for span in &spans {
                        for (time, end) in [(span.start, false), (span.end, true)] {
                            if time < timeline.view.start - 1.0 || time > visible_end + 1.0 {
                                continue;
                            }
                            let center = egui::pos2(timeline.view.x(left, time), rect.center().y);
                            if center.x < left {
                                continue;
                            }
                            let mark = MarkRef::SectionEnd {
                                track: row.track,
                                index: span.index,
                                end,
                            };
                            let selected = timeline.selection.contains(&mark);
                            section_mark(&painter, center, selected);
                            hits.push((mark, center));
                        }
                    }
                }
                row_rects.push((row.track, rect));
                row_responses.push(response);
            }
        });
    timeline.rows_drawn = drawn;
    timeline.key_points.clone_from(&hits);
    if let Some(name) = toggled
        && !timeline.expanded.remove(&name)
    {
        timeline.expanded.insert(name);
    }
    // Playhead and range over everything drawn.
    let all = egui::Rect::from_min_max(
        egui::pos2(left, header.top()),
        egui::pos2(header.right(), ui.min_rect().bottom()),
    );
    let painter = ui.painter_at(all);
    let x = timeline.view.x(left, playhead);
    painter.line_segment(
        [egui::pos2(x, all.top()), egui::pos2(x, all.bottom())],
        egui::Stroke::new(1.5, egui::Color32::from_rgb(230, 80, 60)),
    );
    if let Some(Drag::Box { from, to, .. }) = &timeline.drag {
        painter.rect_stroke(
            egui::Rect::from_two_pos(*from, *to),
            0.0,
            egui::Stroke::new(1.0, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    let frame = KeyFrame {
        hits: &hits,
        rows: &row_rects,
        left,
        playhead,
        fps,
    };
    for response in &row_responses {
        key_gesture(ui, response, panel, timeline, world, &frame);
    }
    let hovered = ui.rect_contains_pointer(all.union(header));
    keyboard(ui, panel, timeline, world, hovered, playhead);
    ui.label(format!(
        "{} / {}   {} key(s) selected{}",
        show_time(playhead, fps, timeline.transport.display),
        show_time(duration, fps, timeline.transport.display),
        timeline.selection.len(),
        timeline
            .clipboard
            .as_ref()
            .map_or(String::new(), |c| format!(", {} on the clipboard", c.len()))
    ));
    if let Some(error) = &timeline.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
}

fn show_time(time: f32, fps: f32, display: TimeDisplay) -> String {
    match display {
        TimeDisplay::Timecode => timecode(time, fps),
        TimeDisplay::Frames => format!("{}", (f64::from(time) * f64::from(fps)).round() as i64),
    }
}

fn diamond(painter: &egui::Painter, center: egui::Pos2, selected: bool, faint: bool) {
    let r = if faint { KEY_RADIUS - 1.5 } else { KEY_RADIUS };
    let fill = if selected {
        egui::Color32::from_rgb(255, 200, 60)
    } else if faint {
        egui::Color32::from_gray(150)
    } else {
        egui::Color32::from_gray(210)
    };
    painter.add(egui::Shape::convex_polygon(
        vec![
            center + egui::vec2(0.0, -r),
            center + egui::vec2(r, 0.0),
            center + egui::vec2(0.0, r),
            center + egui::vec2(-r, 0.0),
        ],
        fill,
        egui::Stroke::new(1.0, egui::Color32::from_gray(40)),
    ));
}

/// A section start or end mark: a rounded square, blue unselected and yellow selected, so it does
/// not read as a key.
fn section_mark(painter: &egui::Painter, center: egui::Pos2, selected: bool) {
    let r = KEY_RADIUS;
    let fill = if selected {
        egui::Color32::from_rgb(255, 200, 60)
    } else {
        egui::Color32::from_rgb(120, 190, 255)
    };
    painter.rect_filled(
        egui::Rect::from_center_size(center, egui::vec2(r * 1.4, r * 1.4)),
        2.0,
        fill,
    );
}

/// A tick spacing in seconds that puts about 80 points between labels.
fn tick_step(pixels_per_second: f32) -> f32 {
    let raw = 80.0 / pixels_per_second;
    let mut step = 0.01;
    for s in [
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ] {
        step = s;
        if s >= raw {
            break;
        }
    }
    step
}

fn draw_ruler(
    painter: &egui::Painter,
    rect: egui::Rect,
    left: f32,
    view: &TimeView,
    fps: f32,
    range: Option<(f32, f32)>,
) {
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(35));
    if let Some((a, b)) = range {
        let r = egui::Rect::from_x_y_ranges(
            view.x(left, a).max(left)..=view.x(left, b).max(left),
            rect.y_range(),
        );
        painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(80, 140, 220, 60));
    }
    let step = tick_step(view.pixels_per_second);
    let end = view.time(left, rect.right());
    let mut t = (view.start.max(0.0) / step).floor() * step;
    let mut guard = 0;
    while t <= end && guard < 500 {
        guard += 1;
        let x = view.x(left, t);
        if x >= left {
            painter.line_segment(
                [egui::pos2(x, rect.bottom() - 6.0), egui::pos2(x, rect.bottom())],
                egui::Stroke::new(1.0, egui::Color32::from_gray(150)),
            );
            let label = if step >= 1.0 {
                format!("{t:.0}s")
            } else {
                format!("{t:.2}s")
            };
            painter.text(
                egui::pos2(x + 2.0, rect.top() + 2.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::monospace(10.0),
                egui::Color32::from_gray(190),
            );
        }
        t += step;
    }
    // Frame ticks when frames are wide enough to see.
    if view.pixels_per_second / fps >= 6.0 {
        let mut frame = (view.start.max(0.0) * fps).floor();
        let mut guard = 0;
        while frame / fps <= end && guard < 2000 {
            guard += 1;
            let x = view.x(left, frame / fps);
            if x >= left {
                painter.line_segment(
                    [egui::pos2(x, rect.bottom() - 3.0), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                );
            }
            frame += 1.0;
        }
    }
}

fn draw_markers(
    painter: &egui::Painter,
    rect: egui::Rect,
    left: f32,
    timeline: &SeqTimeline,
    sequence: &Sequence,
) {
    for (i, marker) in sequence.markers.iter().enumerate() {
        let x = timeline.view.x(left, marker.time);
        if x < left || x > rect.right() {
            continue;
        }
        let color = if timeline.marker == Some(i) {
            egui::Color32::from_rgb(255, 200, 60)
        } else {
            egui::Color32::from_rgb(120, 190, 255)
        };
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(x - 5.0, rect.top() + 2.0),
                egui::pos2(x + 5.0, rect.top() + 2.0),
                egui::pos2(x, rect.top() + 10.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
        painter.text(
            egui::pos2(x + 6.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            &marker.name,
            egui::FontId::proportional(11.0),
            color,
        );
    }
}

fn draw_cuts(
    painter: &egui::Painter,
    rect: egui::Rect,
    left: f32,
    timeline: &SeqTimeline,
    shots: &ShotSequence,
) {
    let inner = rect.shrink2(egui::vec2(0.0, 3.0));
    for (i, shot) in shots.shots.iter().enumerate() {
        let start = shots.shot_start(i) as f32;
        let a = timeline.view.x(left, start).max(left);
        let b = timeline.view.x(left, start + shot.duration).min(rect.right());
        if b <= a {
            continue;
        }
        let bar = egui::Rect::from_x_y_ranges(a..=b, inner.y_range());
        let fill = if timeline.shot == Some(i) {
            egui::Color32::from_rgb(110, 90, 40)
        } else if i.is_multiple_of(2) {
            egui::Color32::from_rgb(60, 70, 95)
        } else {
            egui::Color32::from_rgb(75, 60, 95)
        };
        painter.rect_filled(bar, 2.0, fill);
        painter.text(
            bar.left_center() + egui::vec2(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &shot.name,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
    // The fade curve: opacity of the fade color, bottom (picture) to top (color).
    let samples = ((inner.width() / 4.0) as usize).clamp(16, 512);
    let points: Vec<egui::Pos2> = fade_curve(shots, samples)
        .into_iter()
        .map(|(t, f)| egui::pos2(timeline.view.x(left, t), inner.bottom() - f * inner.height()))
        .filter(|p| p.x >= left && p.x <= rect.right())
        .collect();
    if points.len() >= 2 {
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5, egui::Color32::from_rgb(240, 240, 240)),
        ));
    }
}

fn zoom_and_pan(ui: &egui::Ui, timeline: &mut SeqTimeline, area: egui::Rect, left: f32) {
    if !ui.rect_contains_pointer(area) {
        return;
    }
    let (zoom, scroll, pointer, middle) = ui.input(|i| {
        (
            i.zoom_delta(),
            i.smooth_scroll_delta,
            i.pointer.hover_pos(),
            i.pointer.middle_down(),
        )
    });
    let anchor = pointer.map_or(timeline.view.start, |p| timeline.view.time(left, p.x));
    if (zoom - 1.0).abs() > f32::EPSILON {
        timeline.view.zoom(zoom, anchor);
    }
    // A horizontal wheel (or Shift+wheel) pans.
    if scroll.x.abs() > 0.0 {
        timeline.view.start -= scroll.x / timeline.view.pixels_per_second;
    }
    if middle {
        let delta = ui.input(|i| i.pointer.delta());
        timeline.view.start -= delta.x / timeline.view.pixels_per_second;
        timeline.drag.get_or_insert(Drag::Pan);
    } else if matches!(timeline.drag, Some(Drag::Pan)) {
        timeline.drag = None;
    }
}

fn snapped(timeline: &SeqTimeline, time: f32, fps: f32) -> f32 {
    if timeline.snap {
        snap_to_frame(time, fps)
    } else {
        time
    }
}

/// The header's rows and what they show this frame.
struct Header<'a> {
    ruler: egui::Rect,
    markers: egui::Rect,
    cuts: egui::Rect,
    left: f32,
    sequence: &'a Sequence,
    shots: Option<&'a ShotSequence>,
}

fn header_gesture(
    ui: &egui::Ui,
    response: &egui::Response,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    world: &mut World,
    layout: &Header<'_>,
) {
    let Header {
        ruler,
        markers,
        cuts,
        left,
        sequence,
        shots,
    } = *layout;
    let fps = fps_of(sequence);
    // Plain wheel over the header zooms.
    let wheel = ui.input(|i| i.smooth_scroll_delta.y);
    if wheel.abs() > 0.0
        && let Some(pos) = response.hover_pos()
    {
        timeline
            .view
            .zoom((wheel / 200.0).exp(), timeline.view.time(left, pos.x));
    }
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    let time = timeline.view.time(left, pos.x).max(0.0);
    if response.drag_started() || response.clicked() {
        // Hit-test where the button went down.
        let pos = ui.input(|i| i.pointer.press_origin()).unwrap_or(pos);
        let time = timeline.view.time(left, pos.x).max(0.0);
        timeline.drag = None;
        if ruler.contains(pos) {
            timeline.drag = Some(Drag::Scrub);
        } else if markers.contains(pos) {
            let hit = sequence
                .markers
                .iter()
                .position(|m| (timeline.view.x(left, m.time) - pos.x).abs() <= 6.0);
            timeline.marker = hit;
            if let Some(i) = hit {
                timeline.begin_gesture();
                timeline.drag = Some(Drag::Marker(i));
            }
        } else if cuts.contains(pos)
            && let Some(shots) = shots
        {
            let boundary = cut_times(shots)
                .iter()
                .position(|t| (timeline.view.x(left, *t) - pos.x).abs() <= 5.0);
            let end = shots.duration() as f32;
            // With ripple on, every frame of the drag applies to the state at the press, so each
            // frame is one whole ripple from the press point (a refused frame keeps the last
            // good one).
            let origin = timeline.ripple.then(|| capture_cut_base(world, panel));
            if let Some(cut) = boundary {
                timeline.begin_gesture();
                timeline.drag = Some(Drag::Cut { cut, origin });
            } else if (timeline.view.x(left, end) - pos.x).abs() <= 5.0 {
                timeline.begin_gesture();
                timeline.drag = Some(Drag::Trim {
                    shot: shots.shots.len().saturating_sub(1),
                    origin,
                });
            } else {
                timeline.shot = shots.locate(f64::from(time)).map(|(i, _)| i);
            }
        }
    }
    if response.clicked() || response.dragged() {
        match timeline.drag.clone() {
            Some(Drag::Scrub) => {
                if let Some((entity, ..)) = player_state(world, panel) {
                    seek_all(world, entity, snapped(timeline, time, fps));
                }
            }
            Some(Drag::Marker(i)) if response.dragged() => {
                let at = snapped(timeline, time, fps);
                let r = edit_marker(world, panel, timeline, MarkerOp::Move { index: i, time: at });
                keep(timeline, r);
            }
            Some(Drag::Cut { cut, origin }) if response.dragged() => {
                let at = snapped(timeline, time, fps);
                let edit = CutEdit::Move {
                    cut,
                    time: at,
                    ripple: timeline.ripple,
                };
                let r = edit_cuts_from(world, panel, timeline, &edit, origin.as_ref());
                keep(timeline, r);
            }
            Some(Drag::Trim { shot, origin }) if response.dragged() => {
                // The shot's start at the press (a trim never moves its own shot's start).
                let from = origin.as_ref().and_then(|(list, _)| list.as_ref()).or(shots);
                if let Some(list) = from {
                    let start = list.shot_start(shot) as f32;
                    let at = snapped(timeline, time, fps);
                    let edit = CutEdit::Trim {
                        shot,
                        duration: at - start,
                        ripple: timeline.ripple,
                    };
                    let r = edit_cuts_from(world, panel, timeline, &edit, origin.as_ref());
                    keep(timeline, r);
                }
            }
            _ => {}
        }
    }
    if response.drag_stopped() || response.clicked() {
        end_gesture(world, timeline);
    }
}

fn end_gesture(world: &mut World, timeline: &mut SeqTimeline) {
    if timeline.gesture.take().is_some() {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
    if !matches!(timeline.drag, Some(Drag::Box { .. })) {
        timeline.drag = None;
    }
}

/// The state a rippling cut or trim drag re-applies to: the cut host's shot list and the panel
/// player's sequence.
fn capture_cut_base(world: &mut World, panel: &SeqCurvePanel) -> CutBase {
    let host = cut_host(world, panel);
    let shots = host.and_then(|h| shots_of(world, h));
    let sequence = player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.sequence().clone())
        .unwrap_or_default();
    (shots, sequence)
}

/// Where the drawn marks (keys, section ends) and rows are this frame.
struct KeyFrame<'a> {
    hits: &'a [(MarkRef, egui::Pos2)],
    rows: &'a [(usize, egui::Rect)],
    left: f32,
    playhead: f32,
    fps: f32,
}

fn key_gesture(
    ui: &egui::Ui,
    response: &egui::Response,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    world: &mut World,
    frame: &KeyFrame<'_>,
) {
    let KeyFrame {
        hits,
        rows,
        left,
        playhead,
        fps,
    } = *frame;
    let (shift, alt) = ui.input(|i| (i.modifiers.shift, i.modifiers.alt));
    let hit_at = |pos: egui::Pos2| {
        hits.iter()
            .filter(|(_, c)| {
                (c.x - pos.x).abs() <= KEY_RADIUS + 1.0 && (c.y - pos.y).abs() <= KEY_RADIUS + 1.0
            })
            .min_by(|a, b| (a.1.x - pos.x).abs().total_cmp(&(b.1.x - pos.x).abs()))
            .map(|(m, _)| *m)
    };
    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && pos.x >= left
    {
        match hit_at(pos) {
            Some(mark) if shift => {
                if let Some(i) = timeline.selection.iter().position(|m| *m == mark) {
                    timeline.selection.remove(i);
                } else {
                    timeline.selection.push(mark);
                }
            }
            Some(mark) => timeline.selection = vec![mark],
            None if !shift => timeline.selection.clear(),
            None => {}
        }
        timeline.selection.sort_unstable();
    }
    // A drag starts where the button went down, not where the pointer is when it is decided.
    let origin = ui
        .input(|i| i.pointer.press_origin())
        .or_else(|| response.interact_pointer_pos());
    if response.drag_started()
        && let Some(pos) = origin
        && pos.x >= left
    {
        match hit_at(pos) {
            Some(mark) => {
                if !timeline.selection.contains(&mark) {
                    if !shift {
                        timeline.selection.clear();
                    }
                    timeline.selection.push(mark);
                    timeline.selection.sort_unstable();
                }
                if let Some((_, _, sequence, _)) = player_state(world, panel) {
                    timeline.begin_gesture();
                    timeline.drag = Some(Drag::Keys {
                        origin: sequence,
                        marks: timeline.selection.clone(),
                        start_time: timeline.view.time(left, pos.x),
                        // Alt scales the marks (keys and section ends) about the playhead.
                        scale: alt,
                        pivot: playhead,
                    });
                }
            }
            None => {
                let to = response.interact_pointer_pos().unwrap_or(pos);
                timeline.drag = Some(Drag::Box {
                    from: pos,
                    to,
                    add: shift,
                });
            }
        }
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        match &mut timeline.drag {
            Some(Drag::Box { to, .. }) => *to = pos,
            Some(Drag::Keys {
                origin,
                marks,
                start_time,
                scale,
                pivot,
            }) => {
                let now = timeline.view.time(left, pos.x);
                // Every frame of the drag edits the drag's starting sequence, so a refused
                // position leaves the last good one and nothing accumulates. The moved marks
                // come back at their new places (a key may re-sort to a new index).
                let moved = if *scale {
                    let from = *start_time - *pivot;
                    if from.abs() < 1e-4 {
                        None
                    } else {
                        let factor = ((now - *pivot) / from).max(0.01);
                        let mut next = origin.clone();
                        apply_mark_scale(&mut next, marks, *pivot, factor)
                            .ok()
                            .map(|selected| (next, selected))
                    }
                } else {
                    let raw = now - *start_time;
                    let delta = if timeline.snap {
                        snap_to_frame(raw, fps)
                    } else {
                        raw
                    };
                    let mut next = origin.clone();
                    apply_mark_move(&mut next, marks, delta)
                        .ok()
                        .map(|selected| (next, selected))
                };
                if let Some((next, selected)) = moved {
                    let r = edit_sequence(world, panel, timeline, "Move sequence keys", |s| {
                        *s = next.clone();
                        Ok(())
                    });
                    if r.is_ok() {
                        timeline.selection = selected;
                    }
                }
            }
            _ => {}
        }
    }
    if response.drag_stopped() {
        if let Some(Drag::Box { from, to, add }) = timeline.drag.take() {
            let area = egui::Rect::from_two_pos(from, to);
            let mut picked: Vec<MarkRef> = hits
                .iter()
                .filter(|(_, c)| area.contains(*c))
                .map(|(m, _)| *m)
                .collect();
            // Rows of a box that were not drawn keep nothing; a row's own marks decide.
            picked.retain(|m| {
                rows.iter().any(|(t, r)| {
                    *t == match m {
                        MarkRef::Key(k) => k.track,
                        MarkRef::SectionEnd { track, .. } => *track,
                    } && r.intersects(area)
                })
            });
            if add {
                timeline.selection.extend(picked);
            } else {
                timeline.selection = picked;
            }
            timeline.selection.sort_unstable();
            timeline.selection.dedup();
        }
        end_gesture(world, timeline);
    }
}

fn keyboard(
    ui: &egui::Ui,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    world: &mut World,
    hovered: bool,
    playhead: f32,
) {
    if !hovered || ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    use egui::Key;
    let pressed: Vec<Key> = ui.input(|i| {
        [
            Key::Space,
            Key::J,
            Key::K,
            Key::L,
            Key::Home,
            Key::End,
            Key::Comma,
            Key::Period,
            Key::Delete,
            Key::Backspace,
        ]
        .into_iter()
        .filter(|k| i.key_pressed(*k))
        .collect()
    });
    let (copy, paste) = ui.input(|i| {
        (
            i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))),
        )
    });
    for key in pressed {
        let op = match key {
            Key::Space => Some(TransportOp::Toggle),
            Key::J => Some(TransportOp::Reverse),
            Key::K => Some(TransportOp::Pause),
            Key::L => Some(TransportOp::Play),
            Key::Home => Some(TransportOp::Home),
            Key::End => Some(TransportOp::End),
            Key::Comma => Some(TransportOp::Step(-1)),
            Key::Period => Some(TransportOp::Step(1)),
            _ => None,
        };
        if let Some(op) = op {
            let r = transport(world, panel, timeline, op);
            keep(timeline, r);
        } else {
            // Delete removes the selected keys; section ends are not keys and stay selected.
            let keys: Vec<KeyRef> = timeline
                .selection
                .iter()
                .filter_map(|m| match m {
                    MarkRef::Key(k) => Some(*k),
                    MarkRef::SectionEnd { .. } => None,
                })
                .collect();
            if !keys.is_empty() {
                let r = edit_keys(world, panel, timeline, &KeyBatch::Delete { keys });
                if r.is_ok() {
                    timeline.selection.clear();
                }
                keep(timeline, r);
            }
        }
    }
    if copy {
        let r = copy_selection(world, panel, timeline);
        keep(timeline, r);
    }
    if paste {
        let r = paste_at(world, panel, timeline, playhead);
        keep(timeline, r);
    }
}

/// Copies the selected keys and sections to the timeline's clipboard (`copy_marks`: a section with
/// either end selected is copied whole). Returns how many keys and sections. Codes:
/// `seq_keys_empty` (nothing selected) and those of `copy_marks`.
pub fn copy_selection(
    world: &mut World,
    panel: &SeqCurvePanel,
    timeline: &mut SeqTimeline,
) -> Result<usize, EditError> {
    let Some((_, _, sequence, _)) = player_state(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    if timeline.selection.is_empty() {
        return refuse("seq_keys_empty", "no keys selected");
    }
    let clip = copy_marks(&sequence, &timeline.selection)?;
    let n = clip.len();
    timeline.clipboard = Some(clip);
    Ok(n)
}

/// Pastes the clipboard (keys and sections) at `time` into the panel's player (one undo entry);
/// selects the pasted keys and both ends of each pasted section. Code: `seq_keys_empty` (nothing
/// copied) and those of `paste_marks`.
pub fn paste_at(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    time: f32,
) -> Result<Vec<MarkRef>, EditError> {
    let Some(clip) = timeline.clipboard.clone() else {
        return refuse("seq_keys_empty", "nothing copied yet");
    };
    let selected = edit_sequence(world, panel, timeline, "Paste sequence keys", |s| {
        regia_framework::sequencer::edit::timeline::paste_marks(s, &clip, time)
    })?;
    timeline.selection.clone_from(&selected);
    Ok(selected)
}

/// Shows the first selected keyed track in the curve editor below (component x of a vec3).
pub fn open_in_curve_editor(panel: &mut SeqCurvePanel, timeline: &SeqTimeline) -> Result<usize, EditError> {
    let Some(MarkRef::Key(key)) = timeline.selection.iter().find(|m| matches!(m, MarkRef::Key(_))) else {
        return refuse("seq_keys_empty", "select a key of the track to open");
    };
    panel.track = key.track;
    panel.component = 0;
    panel.selected = Some(key.index);
    Ok(key.track)
}

fn toolbar(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    world: &mut World,
    playhead: f32,
    fps: f32,
) {
    ui.horizontal_wrapped(|ui| {
        let mut op = None;
        // Transport glyphs from the editor's symbol set, each with a spoken name (bug 25, ADR-8586).
        if crate::a11y_audit::name(ui.button("⏮"), egui::WidgetType::Button, "Go to start")
            .on_hover_text("Home: the play range's start")
            .clicked()
        {
            op = Some(TransportOp::Home);
        }
        if crate::a11y_audit::name(ui.button("⏪"), egui::WidgetType::Button, "Play in reverse")
            .on_hover_text("J: play in reverse")
            .clicked()
        {
            op = Some(TransportOp::Reverse);
        }
        let playing = timeline.transport.playing;
        if ui
            .button(if playing { "Pause" } else { "Play" })
            .on_hover_text("Space")
            .clicked()
        {
            op = Some(TransportOp::Toggle);
        }
        if ui.button("Stop").clicked() {
            op = Some(TransportOp::Stop);
        }
        if crate::a11y_audit::name(ui.button("⏭"), egui::WidgetType::Button, "Go to end")
            .on_hover_text("End: the play range's end")
            .clicked()
        {
            op = Some(TransportOp::End);
        }
        let mut looping = timeline.transport.looping;
        if ui.checkbox(&mut looping, "Loop").changed() {
            op = Some(TransportOp::Loop(looping));
        }
        if ui
            .button("In")
            .on_hover_text("Set the play range's start at the playhead")
            .clicked()
        {
            let out = timeline.transport.range.map_or(f32::MAX, |r| r.1);
            let end = player_state(world, panel).map_or(out, |(_, _, _, d)| out.min(d));
            op = Some(TransportOp::Range(Some((playhead, end))));
        }
        if ui
            .button("Out")
            .on_hover_text("Set the play range's end at the playhead")
            .clicked()
        {
            let start = timeline.transport.range.map_or(0.0, |r| r.0);
            op = Some(TransportOp::Range(Some((start, playhead))));
        }
        if timeline.transport.range.is_some() && ui.button("Clear range").clicked() {
            op = Some(TransportOp::Range(None));
        }
        let frames = timeline.transport.display == TimeDisplay::Frames;
        if ui
            .selectable_label(frames, if frames { "Frames" } else { "Timecode" })
            .on_hover_text(format!("{fps} fps"))
            .clicked()
        {
            op = Some(TransportOp::Display(if frames {
                TimeDisplay::Timecode
            } else {
                TimeDisplay::Frames
            }));
        }
        if let Some(op) = op {
            let r = transport(world, panel, timeline, op);
            keep(timeline, r);
        }
        ui.separator();
        ui.checkbox(&mut timeline.snap, "Snap");
        ui.checkbox(&mut timeline.ripple, "Ripple")
            .on_hover_text("Cut moves and trims shift later keys, markers and cuts");
        ui.checkbox(&mut timeline.transport.follow, "Follow");
        if ui.button("Copy").clicked() {
            let r = copy_selection(world, panel, timeline);
            keep(timeline, r);
        }
        if ui.button("Paste").on_hover_text("At the playhead").clicked() {
            let r = paste_at(world, panel, timeline, playhead);
            keep(timeline, r);
        }
        if ui.button("Delete keys").clicked() {
            let keys: Vec<KeyRef> = timeline
                .selection
                .iter()
                .filter_map(|m| match m {
                    MarkRef::Key(k) => Some(*k),
                    MarkRef::SectionEnd { .. } => None,
                })
                .collect();
            let r = edit_keys(world, panel, timeline, &KeyBatch::Delete { keys });
            if r.is_ok() {
                timeline.selection.clear();
            }
            keep(timeline, r);
        }
        if ui.button("Open in curve editor").clicked() {
            let r = open_in_curve_editor(panel, timeline);
            keep(timeline, r);
        }
    });
}

fn marker_fields(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    timeline: &mut SeqTimeline,
    world: &mut World,
) {
    let Some((_, playhead, sequence, _)) = player_state(world, panel) else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        if ui.button("Add marker").on_hover_text("At the playhead").clicked() {
            let r = edit_marker(
                world,
                panel,
                timeline,
                MarkerOp::Add {
                    name: None,
                    time: playhead,
                    payload: String::new(),
                },
            );
            if let Ok(i) = &r {
                timeline.marker = Some(*i);
            }
            keep(timeline, r);
        }
        if let Some(i) = timeline.marker.filter(|i| *i < sequence.markers.len()) {
            let marker = &sequence.markers[i];
            let mut name = marker.name.clone();
            // Accessible names for the marker and CSV fields (ADR-7383's panel audit).
            if crate::a11y_audit::name(
                ui.add(egui::TextEdit::singleline(&mut name).desired_width(100.0)),
                egui::WidgetType::TextEdit,
                "Marker name",
            )
            .lost_focus()
                && name != marker.name
            {
                let r = edit_marker(world, panel, timeline, MarkerOp::Rename { index: i, name });
                keep(timeline, r);
            }
            let mut payload = marker.payload.clone();
            if crate::a11y_audit::name(
                ui.add(
                    egui::TextEdit::singleline(&mut payload)
                        .hint_text("payload")
                        .desired_width(140.0),
                ),
                egui::WidgetType::TextEdit,
                "Marker payload",
            )
            .lost_focus()
                && payload != marker.payload
            {
                let r = edit_marker(world, panel, timeline, MarkerOp::Payload { index: i, payload });
                keep(timeline, r);
            }
            if ui.button("Delete marker").clicked() {
                let r = edit_marker(world, panel, timeline, MarkerOp::Delete { index: i });
                timeline.marker = None;
                keep(timeline, r);
            }
        }
        crate::a11y_audit::name(
            ui.add(
                egui::TextEdit::singleline(&mut timeline.csv_path)
                    .hint_text("markers .csv")
                    .desired_width(120.0),
            ),
            egui::WidgetType::TextEdit,
            "Markers CSV path",
        );
        let path = timeline.csv_path.trim().to_string();
        if ui.button("Import CSV").clicked() {
            let r = read_marker_csv(world, &path)
                .and_then(|csv| edit_marker(world, panel, timeline, MarkerOp::ImportCsv { csv }));
            keep(timeline, r);
        }
        if ui.button("Export CSV").clicked() {
            let r = export_markers_to(world, panel, &path);
            keep(timeline, r);
        }
        ui.separator();
        if ui
            .button("Add cut")
            .on_hover_text("At the playhead, from the selected or active camera")
            .clicked()
        {
            let r = camera_pose(world, None).and_then(|pose| {
                edit_cuts(
                    world,
                    panel,
                    timeline,
                    &CutEdit::Add {
                        time: playhead,
                        pose,
                        ripple: timeline.ripple,
                    },
                )
            });
            keep(timeline, r);
        }
        let host = cut_host(world, panel);
        let shots = host.and_then(|h| shots_of(world, h));
        let picked = shots
            .as_ref()
            .zip(timeline.shot)
            .filter(|(s, i)| *i < s.shots.len());
        if let Some((shots, i)) = picked {
            let shot = &shots.shots[i];
            ui.label(format!("{}:", shot.name));
            let (mut fade_in, mut fade_out) = (shot.fade_in, shot.fade_out);
            let a = ui.add(fade_drag(&mut fade_in, shot.duration, "fade in "));
            let b = ui.add(fade_drag(&mut fade_out, shot.duration, "fade out "));
            if a.drag_started() || b.drag_started() {
                timeline.begin_gesture();
            }
            if a.changed() || b.changed() {
                let r = edit_cuts(
                    world,
                    panel,
                    timeline,
                    &CutEdit::Fade {
                        shot: i,
                        fade_in: Some(fade_in),
                        fade_out: Some(fade_out),
                    },
                );
                keep(timeline, r);
            }
            if a.drag_stopped() || b.drag_stopped() || a.lost_focus() || b.lost_focus() {
                end_gesture(world, timeline);
            }
            if i > 0
                && ui
                    .button("Delete cut")
                    .on_hover_text("The cut at this shot's start")
                    .clicked()
            {
                let r = edit_cuts(world, panel, timeline, &CutEdit::Delete { cut: i - 1 });
                keep(timeline, r);
            }
        }
    });
}

fn fade_drag<'a>(value: &'a mut f32, max: f32, prefix: &'static str) -> egui::DragValue<'a> {
    egui::DragValue::new(value)
        .speed(0.01)
        .range(0.0..=max)
        .prefix(prefix)
}

// ---------------------------------------------------------------- agent methods

fn rpc(e: EditError) -> RpcError {
    let hint = match e.code {
        "seq_keys_bad_track" | "seq_keys_bad_key" | "seq_keys_not_keyed" => {
            "editor.seq_timeline_state lists the keyed tracks and their key times"
        }
        "seq_keys_time_taken" => "move the keys by another delta, or move the key in the way too",
        "seq_keys_last_key" => "keep one key per track, or remove the track with editor.seq_track_edit",
        "seq_keys_no_track" => "add a track with the same name, or the same binding and property, first",
        "seq_keys_empty" => "select or copy keys first",
        "seq_cut_no_camera" => "pass `camera`, select a camera, or add one to the scene",
        "seq_cut_no_shots" => "op add creates the shot list",
        "seq_ripple_keys_in_span" => {
            "move or delete the keys and markers in the removed span first, or edit without ripple"
        }
        "seq_marker_bad_path" => "use a project-relative path ending in .csv, without `..`",
        _ => return crate::seq_curve_panel::rpc(e),
    };
    let class = match e.code {
        "seq_keys_bad_track" | "seq_keys_bad_key" => regia_agent::protocol::NOT_FOUND,
        _ => regia_agent::protocol::INVALID_PARAMS,
    };
    RpcError::new(class, e.code, e.message, hint)
}

fn with_timeline(
    world: &mut World,
    params: &Value,
    f: impl FnOnce(&mut World, &mut SeqCurvePanel, &mut SeqTimeline) -> Result<Value, RpcError>,
) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let mut timeline = world.remove_resource::<SeqTimeline>().unwrap_or_default();
    // An agent call is its own undo entry, even while a pointer drag is open.
    let live = timeline.gesture.take();
    let only_player = json!({ "player": params.get("player").cloned().unwrap_or(Value::Null) });
    let out = crate::seq_curve_panel::select(world, &mut panel, &only_player)
        .map_err(rpc)
        .and_then(|()| f(world, &mut panel, &mut timeline));
    timeline.gesture = live;
    world.insert_resource(timeline);
    world.insert_resource(panel);
    out
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|v| Some(v as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number"))),
    }
}

fn need_number(params: &Value, field: &str, op: &str) -> Result<f32, RpcError> {
    number(params, field)?.ok_or_else(|| RpcError::invalid_params(format!("op `{op}` needs `{field}`")))
}

fn index_field(params: &Value, field: &str) -> Result<Option<usize>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an index"))),
    }
}

fn op_of(params: &Value) -> Result<&str, RpcError> {
    params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`op` (a string) is required"))
}

/// The `keys` param: `[{ track, index }]` for a key, or `[{ track, index, end: "start"|"end" }]`
/// for a section start or end (a track by index or by name; the default selection holds both
/// kinds).
fn keys_param(sequence: &Sequence, params: &Value) -> Result<Option<Vec<MarkRef>>, RpcError> {
    let Some(list) = params.get("keys").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(list) = list.as_array() else {
        return Err(RpcError::invalid_params(
            "`keys` is an array of { track, index } or section refs { track, index, end }",
        ));
    };
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let track =
            match item.get("track") {
                Some(Value::String(name)) => sequence
                    .tracks
                    .iter()
                    .position(|t| &t.name == name)
                    .ok_or_else(|| {
                        rpc(EditError {
                            code: "seq_keys_bad_track",
                            message: format!("no track named `{name}`"),
                        })
                    })?,
                Some(v) => v
                    .as_u64()
                    .map(|v| v as usize)
                    .ok_or_else(|| RpcError::invalid_params("a key's `track` is an index or a track name"))?,
                None => return Err(RpcError::invalid_params("a key needs `track`")),
            };
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .ok_or_else(|| RpcError::invalid_params("a key needs `index`"))?;
        match item.get("end") {
            None | Some(Value::Null) => out.push(MarkRef::Key(KeyRef { track, index })),
            Some(Value::String(which)) if which == "start" => {
                out.push(MarkRef::SectionEnd {
                    track,
                    index,
                    end: false,
                });
            }
            Some(Value::String(which)) if which == "end" => {
                out.push(MarkRef::SectionEnd {
                    track,
                    index,
                    end: true,
                });
            }
            Some(_) => {
                return Err(RpcError::invalid_params(
                    "a section ref's `end` is \"start\" or \"end\"; a key ref has no `end`",
                ));
            }
        }
    }
    Ok(Some(out))
}

fn keys_json(sequence: &Sequence, marks: &[MarkRef]) -> Value {
    marks
        .iter()
        .map(|m| match m {
            MarkRef::Key(k) => json!({
                "track": k.track,
                "name": sequence.tracks.get(k.track).map(|t| t.name.as_str()),
                "index": k.index,
                "time": key_time(sequence, *k),
            }),
            MarkRef::SectionEnd { track, index, end } => json!({
                "track": track,
                "name": sequence.tracks.get(*track).map(|t| t.name.as_str()),
                "index": index,
                "end": if *end { "end" } else { "start" },
                "time": mark_time(sequence, *m),
            }),
        })
        .collect()
}

/// The whole timeline as JSON (what `editor.seq_timeline_state` returns).
pub fn state_json(world: &mut World, panel: &SeqCurvePanel, timeline: &SeqTimeline) -> Value {
    let Some((entity, time, sequence, duration)) = player_state(world, panel) else {
        return json!({ "player": null });
    };
    let fps = fps_of(&sequence);
    let host = cut_host(world, panel);
    let shots = host.and_then(|h| shots_of(world, h));
    let cuts = shots.as_ref().map(|list| {
        json!({
            "host": host.map(CutHost::name),
            "cuts": cut_times(list),
            "shots": list.shots.iter().enumerate().map(|(i, s)| json!({
                "name": s.name,
                "start": list.shot_start(i),
                "duration": s.duration,
                "fade_in": s.fade_in,
                "fade_out": s.fade_out,
            })).collect::<Vec<_>>(),
        })
    });
    let t = &timeline.transport;
    json!({
        "player": entity.to_bits(),
        "time": time,
        "duration": duration,
        "fps": fps,
        "timecode": timecode(time, fps),
        "frame": (f64::from(time) * f64::from(fps)).round() as i64,
        "transport": {
            "playing": t.playing,
            "rate": t.rate,
            "loop": t.looping,
            "range": t.range.map(|(a, b)| json!([a, b])),
            "display": match t.display { TimeDisplay::Timecode => "timecode", TimeDisplay::Frames => "frames" },
            "follow": t.follow,
        },
        "snap": timeline.snap,
        "ripple": timeline.ripple,
        "rows": rows(&sequence, &timeline.expanded).iter().filter(|r| r.component.is_none()).map(|r| {
            let track = &sequence.tracks[r.track];
            let sections = regia_framework::sequencer::sections::section_spans(&track.keys)
                .map(|spans| spans.iter().map(|s| json!({
                    "index": s.index,
                    "start": s.start,
                    "end": s.end,
                    "label": s.label,
                })).collect::<Vec<_>>())
                .unwrap_or_default();
            json!({
                "track": r.track,
                "name": track.name,
                "kind": track.keys.kind(),
                "keys": key_times(&sequence, r.track).unwrap_or_default(),
                "sections": sections,
            })
        }).collect::<Vec<_>>(),
        "selection": keys_json(&sequence, &timeline.selection),
        "clipboard": timeline.clipboard.as_ref().map_or(0, KeyClip::len),
        "markers": sequence.markers.iter().map(|m| json!({ "name": m.name, "time": m.time, "payload": m.payload })).collect::<Vec<_>>(),
        "cuts": cuts,
    })
}

fn seq_timeline_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_timeline(world, params, |world, panel, timeline| {
        Ok(state_json(world, panel, timeline))
    })
}

fn seq_keys(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = op_of(params)?.to_string();
    with_timeline(world, params, |world, panel, timeline| {
        let Some((_, playhead, sequence, _)) = player_state(world, panel) else {
            return Err(rpc(EditError {
                code: "seq_curve_no_player",
                message: "no sequence player exists".into(),
            }));
        };
        let given = keys_param(&sequence, params)?;
        let keys = given.clone().unwrap_or_else(|| timeline.selection.clone());
        let key_refs = |marks: &[MarkRef]| -> Vec<KeyRef> {
            marks
                .iter()
                .filter_map(|m| match m {
                    MarkRef::Key(k) => Some(*k),
                    MarkRef::SectionEnd { .. } => None,
                })
                .collect()
        };
        match op.as_str() {
            "select" => {
                let mut picked = keys;
                if let Some(range) = params.get("range").and_then(Value::as_array) {
                    let (Some(a), Some(b)) = (
                        range.first().and_then(Value::as_f64),
                        range.get(1).and_then(Value::as_f64),
                    ) else {
                        return Err(RpcError::invalid_params("`range` is [from, to] in seconds"));
                    };
                    let tracks: Vec<usize> = match params.get("tracks").and_then(Value::as_array) {
                        Some(list) => list
                            .iter()
                            .filter_map(Value::as_u64)
                            .map(|v| v as usize)
                            .collect(),
                        None => Vec::new(),
                    };
                    picked = marks_in_box(&sequence, &tracks, a as f32, b as f32);
                } else if given.is_none() {
                    picked = Vec::new();
                }
                for mark in &picked {
                    if mark_time(&sequence, *mark).is_none() {
                        return Err(rpc(EditError {
                            code: "seq_keys_bad_key",
                            message: match mark {
                                MarkRef::Key(k) => format!("no key {} on track {}", k.index, k.track),
                                MarkRef::SectionEnd { track, index, .. } => {
                                    format!("no section {index} on track {track}")
                                }
                            },
                        }));
                    }
                }
                match params.get("mode").and_then(Value::as_str).unwrap_or("replace") {
                    "replace" => timeline.selection = picked,
                    "add" => timeline.selection.extend(picked),
                    "toggle" => {
                        for key in picked {
                            if let Some(i) = timeline.selection.iter().position(|k| *k == key) {
                                timeline.selection.remove(i);
                            } else {
                                timeline.selection.push(key);
                            }
                        }
                    }
                    other => {
                        return Err(RpcError::invalid_params(format!(
                            "mode `{other}`: replace, add or toggle"
                        )));
                    }
                }
                timeline.selection.sort_unstable();
                timeline.selection.dedup();
            }
            "move" => {
                let mut delta = need_number(params, "delta", "move")?;
                let snap = params.get("snap").and_then(Value::as_bool).unwrap_or(false);
                if snap {
                    delta = snap_to_frame(delta, fps_of(&sequence));
                }
                // Keys and section ends move together through the same validated copy.
                edit_marks_move(world, panel, timeline, &keys, delta).map_err(rpc)?;
            }
            "scale" => {
                let factor = need_number(params, "factor", "scale")?;
                let pivot = number(params, "pivot")?.unwrap_or(playhead);
                // Keys and section ends scale together through one validated copy (w11).
                edit_marks_scale(world, panel, timeline, &keys, pivot, factor).map_err(rpc)?;
            }
            "delete" => {
                edit_keys(
                    world,
                    panel,
                    timeline,
                    &KeyBatch::Delete {
                        keys: key_refs(&keys),
                    },
                )
                .map_err(rpc)?;
                timeline.selection.clear();
            }
            "copy" => {
                timeline.selection = keys;
                copy_selection(world, panel, timeline).map_err(rpc)?;
            }
            "paste" => {
                let at = number(params, "time")?.unwrap_or(playhead);
                paste_at(world, panel, timeline, at).map_err(rpc)?;
            }
            "open_curve" => {
                if given.is_some() {
                    timeline.selection = keys;
                }
                open_in_curve_editor(panel, timeline).map_err(rpc)?;
            }
            other => {
                return Err(RpcError::invalid_params(format!(
                    "op `{other}`: select, move, scale, delete, copy, paste or open_curve"
                )));
            }
        }
        let (_, _, sequence, _) =
            player_state(world, panel).unwrap_or((Entity::PLACEHOLDER, 0.0, sequence, 0.0));
        Ok(json!({
            "op": op,
            "selection": keys_json(&sequence, &timeline.selection),
            "clipboard": timeline.clipboard.as_ref().map_or(0, KeyClip::len),
            "curve_track": panel.track,
        }))
    })
}

fn marker_index(sequence: &Sequence, params: &Value) -> Result<usize, RpcError> {
    match params.get("marker") {
        Some(Value::String(name)) => sequence
            .markers
            .iter()
            .position(|m| &m.name == name)
            .ok_or_else(|| {
                rpc(EditError {
                    code: "seq_marker_bad_index",
                    message: format!("no marker named `{name}`"),
                })
            }),
        Some(v) => v
            .as_u64()
            .map(|v| v as usize)
            .ok_or_else(|| RpcError::invalid_params("`marker` is an index or a marker name")),
        None => Err(RpcError::invalid_params(
            "this op needs `marker` (an index or a name)",
        )),
    }
}

fn seq_marker_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = op_of(params)?.to_string();
    with_timeline(world, params, |world, panel, timeline| {
        let Some((_, playhead, sequence, _)) = player_state(world, panel) else {
            return Err(rpc(EditError {
                code: "seq_curve_no_player",
                message: "no sequence player exists".into(),
            }));
        };
        let text = |field: &str| params.get(field).and_then(Value::as_str).map(str::to_string);
        let mut csv_out = None;
        let index = match op.as_str() {
            "add" => Some(edit_marker(
                world,
                panel,
                timeline,
                MarkerOp::Add {
                    name: text("name"),
                    time: number(params, "time")?.unwrap_or(playhead),
                    payload: text("payload").unwrap_or_default(),
                },
            )),
            "move" => {
                let index = marker_index(&sequence, params)?;
                let time = need_number(params, "time", "move")?;
                Some(edit_marker(
                    world,
                    panel,
                    timeline,
                    MarkerOp::Move { index, time },
                ))
            }
            "rename" => {
                let index = marker_index(&sequence, params)?;
                let name =
                    text("name").ok_or_else(|| RpcError::invalid_params("op `rename` needs `name`"))?;
                Some(edit_marker(
                    world,
                    panel,
                    timeline,
                    MarkerOp::Rename { index, name },
                ))
            }
            "payload" => {
                let index = marker_index(&sequence, params)?;
                let payload = text("payload").unwrap_or_default();
                Some(edit_marker(
                    world,
                    panel,
                    timeline,
                    MarkerOp::Payload { index, payload },
                ))
            }
            "delete" => {
                let index = marker_index(&sequence, params)?;
                Some(edit_marker(world, panel, timeline, MarkerOp::Delete { index }))
            }
            "import_csv" => {
                let csv = match (text("csv"), text("path")) {
                    (Some(csv), _) => csv,
                    (None, Some(path)) => read_marker_csv(world, &path).map_err(rpc)?,
                    (None, None) => {
                        return Err(RpcError::invalid_params("op `import_csv` needs `csv` or `path`"));
                    }
                };
                Some(edit_marker(world, panel, timeline, MarkerOp::ImportCsv { csv }))
            }
            "export_csv" => {
                let csv = match text("path") {
                    Some(path) => export_markers_to(world, panel, &path),
                    None => export_markers(world, panel),
                }
                .map_err(rpc)?;
                csv_out = Some(csv);
                None
            }
            other => {
                return Err(RpcError::invalid_params(format!(
                    "op `{other}`: add, move, rename, payload, delete, import_csv or export_csv"
                )));
            }
        };
        let index = index.transpose().map_err(rpc)?;
        if op != "import_csv" {
            timeline.marker = index;
        }
        let (_, _, sequence, _) =
            player_state(world, panel).unwrap_or((Entity::PLACEHOLDER, 0.0, sequence, 0.0));
        Ok(json!({
            "op": op,
            "marker": if op == "import_csv" { None } else { index },
            "markers": sequence.markers.iter().map(|m| json!({ "name": m.name, "time": m.time, "payload": m.payload })).collect::<Vec<_>>(),
            "csv": csv_out,
        }))
    })
}

fn seq_cut_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = op_of(params)?.to_string();
    with_timeline(world, params, |world, panel, timeline| {
        let playhead = player_state(world, panel).map_or(0.0, |(_, t, _, _)| t);
        let ripple = |params: &Value| {
            params
                .get("ripple")
                .and_then(Value::as_bool)
                .ok_or_else(|| RpcError::invalid_params("`ripple` is a boolean"))
        };
        let edit = match op.as_str() {
            "add" => {
                let camera = match params.get("camera") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_u64()
                            .and_then(Entity::try_from_bits)
                            .ok_or_else(|| RpcError::invalid_params("`camera` is an entity id"))?,
                    ),
                };
                let pose = camera_pose(world, camera).map_err(rpc)?;
                CutEdit::Add {
                    time: number(params, "time")?.unwrap_or(playhead),
                    pose,
                    ripple: match params.get("ripple") {
                        None | Some(Value::Null) => false,
                        Some(_) => ripple(params)?,
                    },
                }
            }
            "move" => CutEdit::Move {
                cut: index_field(params, "cut")?
                    .ok_or_else(|| RpcError::invalid_params("op `move` needs `cut`"))?,
                time: need_number(params, "time", "move")?,
                ripple: match params.get("ripple") {
                    None | Some(Value::Null) => false,
                    Some(_) => ripple(params)?,
                },
            },
            "trim" => CutEdit::Trim {
                shot: index_field(params, "shot")?
                    .ok_or_else(|| RpcError::invalid_params("op `trim` needs `shot`"))?,
                duration: need_number(params, "duration", "trim")?,
                ripple: match params.get("ripple") {
                    None | Some(Value::Null) => false,
                    Some(_) => ripple(params)?,
                },
            },
            "delete" => CutEdit::Delete {
                cut: index_field(params, "cut")?
                    .ok_or_else(|| RpcError::invalid_params("op `delete` needs `cut`"))?,
            },
            "fade" => CutEdit::Fade {
                shot: index_field(params, "shot")?
                    .ok_or_else(|| RpcError::invalid_params("op `fade` needs `shot`"))?,
                fade_in: number(params, "fade_in")?,
                fade_out: number(params, "fade_out")?,
            },
            other => {
                return Err(RpcError::invalid_params(format!(
                    "op `{other}`: add, move, trim, delete or fade"
                )));
            }
        };
        let shot = edit_cuts(world, panel, timeline, &edit).map_err(rpc)?;
        let mut state = state_json(world, panel, timeline);
        Ok(json!({ "op": op, "shot": shot, "cuts": state["cuts"].take() }))
    })
}

fn seq_transport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = op_of(params)?.to_string();
    with_timeline(world, params, |world, panel, timeline| {
        let parsed = match op.as_str() {
            "play" => TransportOp::Play,
            "reverse" => TransportOp::Reverse,
            "pause" => TransportOp::Pause,
            "toggle" => TransportOp::Toggle,
            "stop" => TransportOp::Stop,
            "seek" => TransportOp::Seek(need_number(params, "time", "seek")?),
            "step" => TransportOp::Step(
                params
                    .get("frames")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    .clamp(-100_000, 100_000) as i32,
            ),
            "home" => TransportOp::Home,
            "end" => TransportOp::End,
            "loop" => TransportOp::Loop(
                params
                    .get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| RpcError::invalid_params("op `loop` needs `on`"))?,
            ),
            "range" => match (number(params, "in")?, number(params, "out")?) {
                (Some(a), Some(b)) => TransportOp::Range(Some((a, b))),
                (None, None) => TransportOp::Range(None),
                _ => {
                    return Err(RpcError::invalid_params(
                        "op `range` needs both `in` and `out`, or neither",
                    ));
                }
            },
            "snap" => TransportOp::Snap(
                params
                    .get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| RpcError::invalid_params("op `snap` needs `on`"))?,
            ),
            "follow" => TransportOp::Follow(
                params
                    .get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| RpcError::invalid_params("op `follow` needs `on`"))?,
            ),
            "display" => TransportOp::Display(match params.get("mode").and_then(Value::as_str) {
                Some("timecode") => TimeDisplay::Timecode,
                Some("frames") => TimeDisplay::Frames,
                _ => {
                    return Err(RpcError::invalid_params(
                        "op `display` needs `mode`: timecode or frames",
                    ));
                }
            }),
            "tick" => TransportOp::Tick(need_number(params, "dt", "tick")?),
            other => {
                return Err(RpcError::invalid_params(format!(
                    "op `{other}`: play, reverse, pause, toggle, stop, seek, step, home, end, loop, range, display, snap, follow or tick"
                )));
            }
        };
        transport(world, panel, timeline, parsed).map_err(rpc)?;
        let mut state = state_json(world, panel, timeline);
        Ok(json!({
            "op": op,
            "time": state["time"].take(),
            "timecode": state["timecode"].take(),
            "frame": state["frame"].take(),
            "transport": state["transport"].take(),
        }))
    })
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const PLAYER: ParamSchema = param(
    "player",
    "integer",
    false,
    "The sequence player entity (default: the panel's, else the first).",
);

const STATE_PARAMS: &[ParamSchema] = &[PLAYER];

const KEYS_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "op",
        "string",
        true,
        "select, move, scale, delete, copy, paste or open_curve.",
    ),
    param(
        "keys",
        "array",
        false,
        "[{ track, index }] (track: an index or a track name) or section refs [{ track, index, \
         end: \"start\"|\"end\" }]. Default: the selection.",
    ),
    param(
        "mode",
        "string",
        false,
        "select: replace (default), add or toggle.",
    ),
    param(
        "range",
        "array",
        false,
        "select: [from, to] seconds, a box over `tracks` (default every track); replaces `keys`.",
    ),
    param(
        "tracks",
        "array",
        false,
        "select with range: the track indices the box covers.",
    ),
    param("delta", "number", true, "move: seconds."),
    param("snap", "boolean", false, "move: round the delta to whole frames."),
    param(
        "factor",
        "number",
        true,
        "scale: greater than zero (keys and section ends; a section with both ends given stretches).",
    ),
    param(
        "pivot",
        "number",
        false,
        "scale: seconds (default: the playhead).",
    ),
    param("time", "number", false, "paste: seconds (default: the playhead)."),
];

const MARKER_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "op",
        "string",
        true,
        "add, move, rename, payload, delete, import_csv or export_csv.",
    ),
    param(
        "marker",
        "integer|string",
        false,
        "The marker: an index or a name (not for add).",
    ),
    param("name", "string", false, "add (default marker<n>), rename."),
    param(
        "time",
        "number",
        false,
        "add (default: the playhead), move: seconds.",
    ),
    param("payload", "string", false, "add, payload."),
    param("csv", "string", false, "import_csv: the sheet's text."),
    param(
        "path",
        "string",
        false,
        "import_csv, export_csv: a project-relative .csv file (export without one returns the text).",
    ),
];

const CUT_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param("op", "string", true, "add, move, trim, delete or fade."),
    param(
        "time",
        "number",
        false,
        "add (default: the playhead), move: seconds.",
    ),
    param(
        "camera",
        "integer",
        false,
        "add: the camera entity (default: the selected camera, else the active one).",
    ),
    param(
        "cut",
        "integer",
        false,
        "move, delete: the cut index (between shot cut and cut + 1).",
    ),
    param("shot", "integer", false, "trim, fade: the shot index."),
    param("duration", "number", false, "trim: seconds, greater than zero."),
    param("fade_in", "number", false, "fade: seconds (omitted keeps it)."),
    param("fade_out", "number", false, "fade: seconds (omitted keeps it)."),
    param(
        "ripple",
        "boolean",
        false,
        "add, move, trim: shift every later cut, and the sequence's keys and markers after the \
         edge's old place, by the edit's delta (default false; one undo entry). Shortening refuses \
         keys or markers in the removed span (seq_ripple_keys_in_span). An add splits a shot in \
         place, so its ripple delta is zero.",
    ),
];

const TRANSPORT_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "op",
        "string",
        true,
        "play, reverse, pause, toggle, stop, seek, step, home, end, loop, range, display, snap, follow or tick.",
    ),
    param("time", "number", false, "seek: seconds."),
    param(
        "frames",
        "integer",
        false,
        "step: frames (default 1; negative steps back).",
    ),
    param("on", "boolean", false, "loop, snap, follow."),
    param(
        "in",
        "number",
        false,
        "range: the in point, seconds (omit both to clear).",
    ),
    param("out", "number", false, "range: the out point, seconds."),
    param("mode", "string", false, "display: timecode or frames."),
    param(
        "dt",
        "number",
        false,
        "tick: seconds to advance a playing transport.",
    ),
];

/// The agent methods of the timeline.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.seq_timeline_state",
                tool: "seq_timeline_state",
                description: "The sequencer timeline of the Sequence Curves tab: the playhead (seconds, timecode, \
                    frame), the transport, every track's key times or sections, the key and section-mark selection \
                    and clipboard, the markers, and the shot list's cuts and fades.",
                params: STATE_PARAMS,
                result: "{ player, time, duration, fps, timecode, frame, transport, snap, ripple, rows: [{ track, \
                    name, kind, keys, sections: [{ index, start, end, label }] }], selection, clipboard, markers, \
                    cuts: { host, cuts, shots } | null }",
                mutating: false,
                permission: Permission::Read,
            },
            seq_timeline_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_keys",
                tool: "seq_keys",
                description: "Dope-sheet edits across tracks: select (keys, or a time range over tracks), move by a \
                    delta, scale about a pivot, delete, copy, paste at a time (also into another player), \
                    open_curve (show the first selected track in the curve editor). A key reference is \
                    { track, index }; a section track's start or end is { track, index, end: \"start\"|\"end\" } and \
                    moves, scales, copies (the whole section) and pastes with the keys. Each edit is validated on \
                    a copy and is one undo entry.",
                params: KEYS_PARAMS,
                result: "{ op, selection: [{ track, name, index, time } | { track, name, index, end, time }], \
                    clipboard, curve_track }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_keys as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_marker_edit",
                tool: "seq_marker_edit",
                description: "Edit the sequence's markers on the timeline: add (at the playhead by default), move, \
                    rename, payload, delete, import_csv and export_csv (the marker sheet). One undo entry each.",
                params: MARKER_PARAMS,
                result: "{ op, marker, markers: [{ name, time, payload }], csv }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_marker_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_cut_edit",
                tool: "seq_cut_edit",
                description: "Edit the camera cuts and fades of the player's shot list (or the loaded ShotPlayer \
                    list): add a cut at a time from a camera (creates the list when there is none), move a cut, \
                    trim a shot, delete a cut, set a shot's fades. `ripple: true` (add, move, trim) shifts every \
                    later cut and the sequence's keys and markers after the edit point by the edit's delta, as one \
                    undo entry. One undo entry each.",
                params: CUT_PARAMS,
                result: "{ op, shot, cuts: { host, cuts, shots: [{ name, start, duration, fade_in, fade_out }] } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_cut_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_transport",
                tool: "seq_transport",
                description: "The timeline's transport: play, reverse, pause, toggle, stop, seek, step frames, \
                    home and end of the play range, loop, the in/out range (panel state, not saved), timecode or \
                    frames display, snap and follow toggles, and tick (advance a playing transport by dt \
                    seconds, as each frame does).",
                params: TRANSPORT_PARAMS,
                result: "{ op, time, timecode, frame, transport: { playing, rate, loop, range, display, follow } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_transport as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
