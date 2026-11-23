// SPDX-License-Identifier: MPL-2.0
//! The Sequence Curves panel: the curve editor of a sequence player's float and vec3 tracks
//! (`.seq.ron` v2, ADR-1880, book `agents/seq-curve-panel.md`).
//!
//! It edits one curve at a time: one component of one keyed track of one `SequencePlayer`. The
//! curve is drawn from the engine's own sampling (`Channel::sample_component`), so Bezier
//! tangents, weights, steps and smoothstep look exactly as they play. Drag a key to move it, drag
//! a tangent handle to set the slope (both sides follow; Alt breaks the tangent), double-click to
//! add a key, drag empty space to scrub the player, and right-click a key for the ease, the
//! tangent mode and delete. Every edit goes through `regia_framework::sequencer::edit` and is
//! validated on a copy before the player's sequence is swapped (`replace_sequence`, the clock and
//! play state stay), so a refused edit changes nothing. Each edit is an undo entry (a drag is one
//! entry), and the panel opens and saves project `.seq.ron` files. The agent methods
//! `editor.seq_curve_state`, `editor.seq_curve_edit`, `editor.seq_curve_open` and
//! `editor.seq_curve_save` run the same code.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::sequencer::edit::{EditError, KeyEdit, TangentEdit, apply_key_edit};
use regia_framework::sequencer::{
    Binding, BindingRename, Channel, Ease, RecordedTake, Sequence, SequenceBindings, SequenceLibrary,
    SequencePlayer, TakeBinding, TakeChannel, TakeConfig, TakeError, TakeRecorder, TakeStatus, TakeTarget,
    Tangent, rename_binding, save_take,
};
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::anim::curve_view::{KEY_SIZE, handle_position, slope_from_handle};
use crate::anim::view2d::View2d;
use crate::command::{EditorCommand, UndoStack, downcast_to, resolve_entity};
use crate::viewport_agent::Handler;

/// The panel's selection and view; one resource shared by the egui panel and the agent methods.
#[derive(Resource, Debug, Default)]
pub struct SeqCurvePanel {
    /// The player being edited (`None`: the first one).
    pub player: Option<Entity>,
    /// Index of the track in the player's sequence.
    pub track: usize,
    /// The component (0 for a float track, 0..3 for a vec3 track).
    pub component: usize,
    /// The selected key.
    pub selected: Option<usize>,
    /// The widget's pan and zoom.
    pub view: View2d,
    /// The last refused edit, shown under the curve.
    pub error: Option<String>,
    /// Each player's project-relative `.seq.ron`, once opened or saved.
    pub paths: BTreeMap<Entity, String>,
    /// The path field of the panel, for the player in `path_player`.
    pub path_text: String,
    /// The player the path field shows: picking another shows that player's path, so **Save**
    /// never writes one sequence over another's file.
    pub path_player: Option<Entity>,
    /// The running drag: its edits share one undo entry.
    pub gesture: Option<u64>,
    /// Counts drags, so two drags never merge.
    pub gestures: u64,
    /// The keyboard route: the focusable list of the track's keys or sections (ADR-7383).
    pub keys: crate::canvas_keys::ListKeys,
    /// The multi-curve view: which of the sequence's other curves are drawn behind the edited
    /// one and which are locked (P-208; keys from [`seq_curve_key`]).
    pub curves: crate::anim::curve_view::MultiCurve,
}

/// The undo entry of a curve edit: the whole sequence before and after. Edits of one drag merge.
pub struct SeqCurveEdit {
    player: u64,
    before: Sequence,
    after: Sequence,
    gesture: Option<u64>,
    /// The player's binding table before and after (a binding rename moves an entry, ADR-2405);
    /// `None` when the edit leaves the bindings alone.
    bindings: Option<(Option<SequenceBindings>, Option<SequenceBindings>)>,
}

impl SeqCurveEdit {
    fn swap_in(&self, world: &mut World, sequence: Sequence, bindings: Option<&Option<SequenceBindings>>) {
        let Some(entity) = resolve_entity(world, self.player) else {
            return;
        };
        let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
            return;
        };
        player.replace_sequence(sequence);
        match bindings {
            Some(Some(table)) => {
                world.entity_mut(entity).insert(table.clone());
            }
            Some(None) => {
                world.entity_mut(entity).remove::<SequenceBindings>();
            }
            None => {}
        }
    }
}

impl EditorCommand for SeqCurveEdit {
    fn apply(&mut self, world: &mut World) {
        let bindings = self.bindings.as_ref().map(|(_, after)| after);
        self.swap_in(world, self.after.clone(), bindings);
    }

    fn revert(&mut self, world: &mut World) {
        let bindings = self.bindings.as_ref().map(|(before, _)| before);
        self.swap_in(world, self.before.clone(), bindings);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SeqCurveEdit>(next) else {
            return false;
        };
        if self.gesture.is_some()
            && self.gesture == next.gesture
            && self.player == next.player
            && self.bindings.is_none()
            && next.bindings.is_none()
        {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        "Edit sequence curve".to_string()
    }
}

/// What a pointer position hits on the curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurvePick {
    /// A key.
    Key(usize),
    /// A key's incoming tangent handle.
    In(usize),
    /// A key's outgoing tangent handle.
    Out(usize),
}

/// The keys of a curve as `(time, value)` points, `None` for a channel without curves.
#[must_use]
pub fn curve_points(channel: &Channel, component: usize) -> Option<Vec<(f32, f32)>> {
    match channel {
        Channel::Float(keys) if component == 0 => Some(keys.iter().map(|k| (k.time, k.value)).collect()),
        Channel::Vec3(keys) if component < 3 => {
            Some(keys.iter().map(|k| (k.time, k.value[component])).collect())
        }
        _ => None,
    }
}

/// The ease of every key of a curve's channel.
fn eases(channel: &Channel) -> Vec<Ease> {
    match channel {
        Channel::Float(keys) => keys.iter().map(|k| k.ease).collect(),
        Channel::Vec3(keys) => keys.iter().map(|k| k.ease).collect(),
        _ => Vec::new(),
    }
}

/// `true` when key `i` has a handle on that side: the Bezier segment on that side uses it.
#[must_use]
pub fn shows_handle(channel: &Channel, i: usize, outgoing: bool) -> bool {
    let eases = eases(channel);
    if outgoing {
        i + 1 < eases.len() && eases[i] == Ease::Bezier
    } else {
        i > 0 && eases.get(i - 1) == Some(&Ease::Bezier)
    }
}

/// What `pos` hits (keys first, then handles).
#[must_use]
pub fn pick(channel: &Channel, component: usize, view: &View2d, pos: egui::Pos2) -> Option<CurvePick> {
    let points = curve_points(channel, component)?;
    for (i, &(t, v)) in points.iter().enumerate() {
        let s = view.to_screen(egui::vec2(t, v));
        if egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE))
            .expand(2.0)
            .contains(pos)
        {
            return Some(CurvePick::Key(i));
        }
    }
    for (i, &(t, v)) in points.iter().enumerate() {
        let s = view.to_screen(egui::vec2(t, v));
        let Some((incoming, outgoing)) = channel.tangent_slopes(i, component) else {
            continue;
        };
        if shows_handle(channel, i, false)
            && handle_position(view, s, incoming, -1.0).distance(pos) <= KEY_SIZE * 0.75
        {
            return Some(CurvePick::In(i));
        }
        if shows_handle(channel, i, true)
            && handle_position(view, s, outgoing, 1.0).distance(pos) <= KEY_SIZE * 0.75
        {
            return Some(CurvePick::Out(i));
        }
    }
    None
}

/// The tangent edit a handle drag means: one slope on both sides, or (with `broken`, or when the
/// key's tangent is already broken) only the dragged side.
#[must_use]
pub fn handle_edit(
    channel: &Channel,
    component: usize,
    key: usize,
    outgoing: bool,
    slope: f32,
    broken: bool,
) -> Option<KeyEdit> {
    let (incoming, out) = channel.tangent_slopes(key, component)?;
    let already_broken = match channel {
        Channel::Float(keys) => matches!(keys.get(key)?.tangent, Tangent::Broken(..)),
        Channel::Vec3(keys) => matches!(keys.get(key)?.tangent, Tangent::Broken(..)),
        _ => return None,
    };
    let tangent = if broken || already_broken {
        if outgoing {
            TangentEdit::Broken(incoming, slope)
        } else {
            TangentEdit::Broken(slope, out)
        }
    } else {
        TangentEdit::Aligned(slope)
    };
    Some(KeyEdit::Tangent { index: key, tangent })
}

/// The panel's player: the selected one when it still exists, else the lowest entity with a
/// `SequencePlayer`.
pub(crate) fn player_of(world: &mut World, panel: &SeqCurvePanel) -> Option<Entity> {
    if let Some(entity) = panel.player
        && world.get::<SequencePlayer>(entity).is_some()
    {
        return Some(entity);
    }
    players(world).first().copied()
}

pub(crate) fn players(world: &mut World) -> Vec<Entity> {
    let mut query = world.query_filtered::<Entity, With<SequencePlayer>>();
    let mut list: Vec<Entity> = query.iter(world).collect();
    list.sort_unstable_by_key(|e| e.to_bits());
    list
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// Applies `edit` to the panel's curve of the panel's player: edits a copy, validates the whole
/// sequence, then swaps it in (the clock and play state stay). Updates the selection.
pub fn edit_selected(world: &mut World, panel: &mut SeqCurvePanel, edit: KeyEdit) -> Result<(), EditError> {
    let component = panel.component;
    edit_track(world, panel, |channel| apply_key_edit(channel, component, edit))
}

/// Applies `edit` to the channel of the panel's track of the panel's player, the way
/// [`edit_selected`] does (a copy, validated, swapped in, one undo entry per gesture); the section
/// strip (`crate::seq_sections`) edits through it too. `edit` returns the index to select.
pub(crate) fn edit_track(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    edit: impl FnOnce(&mut Channel) -> Result<Option<usize>, EditError>,
) -> Result<(), EditError> {
    let Some(entity) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let mut sequence = player.sequence().clone();
    let count = sequence.tracks.len();
    let Some(track) = sequence.tracks.get_mut(panel.track) else {
        return refuse(
            "seq_curve_bad_track",
            format!("no track {}: the sequence has {count}", panel.track),
        );
    };
    let selected = edit(&mut track.keys)?;
    if let Err(e) = sequence.validate() {
        return refuse("seq_curve_invalid", e.to_string());
    }
    // A locked track keeps its keys and sections (`.seq.ron` v6, ADR-8105).
    regia_framework::sequencer::edit::tracks::check_locked(player.sequence(), &sequence)?;
    let before = player.sequence().clone();
    player.replace_sequence(sequence.clone());
    panel.player = Some(entity);
    panel.selected = selected;
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    undo.push(Box::new(SeqCurveEdit {
        player: entity.to_bits(),
        before,
        after: sequence,
        gesture: panel.gesture,
        bindings: None,
    }));
    if panel.gesture.is_none() {
        undo.seal();
    }
    Ok(())
}

/// Renames binding `from` to `to` in the panel's player (ADR-2405): every track, spatial sound
/// section and sub-sequence binding table of its sequence that refers to `from`
/// (`regia_framework::sequencer::rename_binding`, through the player's sub-sequence library), and
/// the player's binding table entry, so the same entity stays bound. One undo entry, never merged
/// into a drag; undo restores the old name and the old table. Codes: `seq_curve_no_player`, the
/// `seq_binding_*` codes, and `seq_binding_table_in_use` (the player's table already binds `to`).
pub(crate) fn rename_player_binding(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    from: &str,
    to: &str,
) -> Result<BindingRename, EditError> {
    let Some(entity) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let before = player.sequence().clone();
    let mut sequence = before.clone();
    let done = rename_binding(&mut sequence, player.library(), from, to)?;
    // A locked track keeps its binding (`.seq.ron` v6, ADR-8105).
    regia_framework::sequencer::edit::tracks::check_locked(&before, &sequence)?;
    let table_before = world.get::<SequenceBindings>(entity).cloned();
    let table_after = match &table_before {
        Some(table) if table.get(to).is_some() => {
            return refuse(
                "seq_binding_table_in_use",
                format!("the player already binds `{to}`; unbind it first"),
            );
        }
        Some(table) => {
            let mut table = table.clone();
            if let Some(binding) = table.get(from) {
                table.unbind(from);
                table.bind(to, binding);
            }
            Some(table)
        }
        None => None,
    };
    let command = SeqCurveEdit {
        player: entity.to_bits(),
        before,
        after: sequence.clone(),
        gesture: None,
        bindings: Some((table_before, table_after.clone())),
    };
    command.swap_in(world, sequence, Some(&table_after));
    panel.player = Some(entity);
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    // Never part of a drag: close any open entry first.
    undo.seal();
    undo.push(Box::new(command));
    undo.seal();
    Ok(done)
}

/// The project root the panel's files live under.
pub(crate) fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .or_else(|| {
            world
                .get_resource::<regia_asset::AssetServer>()
                .map(|s| s.project_root().to_path_buf())
        })
}

/// `path` under the project root: relative, without `..`, ending in `.seq.ron`, and no symlink
/// on the way out of the root. Codes: `seq_curve_bad_path`, `seq_curve_no_project`.
pub(crate) fn project_file(world: &World, path: &str) -> Result<PathBuf, EditError> {
    project_file_with(world, path, ".seq.ron")
}

/// [`project_file`] for files ending in `suffix` (the Shots panel's `.shots.ron`, ADR-8730).
/// Codes: `seq_curve_bad_path`, `seq_curve_no_project`.
pub(crate) fn project_file_with(world: &World, path: &str, suffix: &str) -> Result<PathBuf, EditError> {
    let relative = Path::new(path);
    if !path.ends_with(suffix) {
        return refuse("seq_curve_bad_path", format!("`{path}` is not a {suffix} path"));
    }
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return refuse(
            "seq_curve_bad_path",
            format!("`{path}` must be relative to the project root, without `..`"),
        );
    }
    let Some(root) = project_root(world) else {
        return refuse("seq_curve_no_project", "no project is open");
    };
    let target = root.join(relative);
    let canonical_root = root.canonicalize().map_err(|e| EditError {
        code: "seq_curve_no_project",
        message: format!("the project root {}: {e}", root.display()),
    })?;
    // The deepest part of the path that exists decides where it really leads.
    let existing = target
        .ancestors()
        .find_map(|p| p.canonicalize().ok())
        .unwrap_or_else(|| canonical_root.clone());
    if !existing.starts_with(&canonical_root) {
        return refuse("seq_curve_bad_path", format!("`{path}` leads out of the project"));
    }
    Ok(target)
}

/// Opens a project `.seq.ron`: its sub-sequences join the [`SequenceLibrary`], a stopped player
/// of the sequence is spawned and selected, on the first curve track. Returns the player. The
/// player is [`crate::editor_only::EditorOnly`]: the document is the file, never the level.
/// Codes: `seq_curve_bad_path`, `seq_curve_no_project`, `seq_curve_io`, `seq_curve_bad_file`.
pub fn open_sequence(world: &mut World, panel: &mut SeqCurvePanel, path: &str) -> Result<Entity, EditError> {
    let text = read_project_sequence(world, path)?;
    let entity = open_sequence_text(world, panel, path, &text)?;
    // What the file holds: the document is clean until an edit (`crate::seq_autosave`).
    if let Some(sequence) = world.get::<SequencePlayer>(entity).map(|p| p.sequence().clone()) {
        crate::seq_autosave::mark_saved(world, entity, sequence);
    }
    Ok(entity)
}

/// Reads the project `.seq.ron` at `path`. Codes: `seq_curve_bad_path`, `seq_curve_no_project`,
/// `seq_curve_io`.
fn read_project_sequence(world: &World, path: &str) -> Result<String, EditError> {
    let file = project_file(world, path)?;
    std::fs::read_to_string(&file).map_err(|e| EditError {
        code: "seq_curve_io",
        message: format!("cannot read {path}: {e}"),
    })
}

/// [`open_sequence`] of `text` instead of the file's own text (a recovered autosave,
/// `crate::seq_autosave`): parses it, loads its sub-sequences from the project, spawns a stopped
/// player of it and selects it, keyed by `path`. The caller records the saved copy. Codes: as
/// [`open_sequence`].
pub(crate) fn open_sequence_text(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    path: &str,
    text: &str,
) -> Result<Entity, EditError> {
    // The path is checked even though the text is given: the document saves there.
    project_file(world, path)?;
    let bad_file = |message: String| EditError {
        code: "seq_curve_bad_file",
        message,
    };
    let sequence = Sequence::from_ron(text).map_err(|e| bad_file(format!("{path}: {e}")))?;
    let subs = SequenceLibrary::read_tree(&sequence, |name| read_project_sequence(world, name), bad_file)?;
    let mut library = world.get_resource_or_insert_with(SequenceLibrary::default);
    for (name, sub) in subs {
        library.insert(name, sub);
    }
    panel.track = sequence
        .tracks
        .iter()
        .position(|t| matches!(t.keys, Channel::Float(_) | Channel::Vec3(_)))
        .unwrap_or(0);
    let name = regia_scene::components::Name::new(format!("sequence:{path}"));
    // The player previews a document saved to its own `.seq.ron`: editor-only, never written to a
    // level, an autosave, an export or Live link (ADR-8583), and so no level undo entry.
    let entity = world
        .spawn((
            SequencePlayer::new(sequence),
            name,
            crate::editor_only::EditorOnly,
        ))
        .id();
    panel.player = Some(entity);
    panel.component = 0;
    panel.selected = None;
    panel.paths.insert(entity, path.to_string());
    panel.path_text = path.to_string();
    panel.path_player = Some(entity);
    panel.error = None;
    Ok(entity)
}

/// Saves the panel's player's sequence to `path` (default: the path that player was opened from
/// or last saved to) with an atomic write. Returns the path. Codes: `seq_curve_no_player`,
/// `seq_curve_bad_path`, `seq_curve_no_project`, `seq_curve_io`, `seq_curve_invalid`.
pub fn save_sequence(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    path: Option<&str>,
) -> Result<String, EditError> {
    let entity = player_of(world, panel);
    let known = entity.and_then(|e| panel.paths.get(&e).cloned());
    let Some(path) = path.map(str::to_string).or(known) else {
        return refuse("seq_curve_bad_path", "no path yet: give one to save to");
    };
    let file = project_file(world, &path)?;
    let Some((entity, player)) =
        entity.and_then(|e| world.get::<SequencePlayer>(e).map(|player| (e, player)))
    else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let text = player.sequence().to_ron().map_err(|e| EditError {
        code: "seq_curve_invalid",
        message: e.to_string(),
    })?;
    let saved = player.sequence().clone();
    crate::level_io::write_atomic(&file, text.as_bytes()).map_err(|message| EditError {
        code: "seq_curve_io",
        message,
    })?;
    crate::seq_autosave::mark_saved(world, entity, saved);
    panel.paths.insert(entity, path.clone());
    panel.path_text = path.clone();
    panel.path_player = Some(entity);
    Ok(path)
}

/// Draws the Sequence Curves panel.
pub fn seq_curve_panel(ui: &mut egui::Ui, world: &mut World) {
    seq_curve_panel_with_play(ui, world, None);
}

/// Draws the Sequence Curves panel; while PIE runs (`play`), its Record take section records the
/// play world (ADR-2637) and everything else edits the edit world.
pub fn seq_curve_panel_with_play(ui: &mut egui::Ui, world: &mut World, play: Option<&mut World>) {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    draw(ui, &mut panel, world, play);
    world.insert_resource(panel);
}

const COMPONENTS: [&str; 3] = ["x", "y", "z"];

fn draw(ui: &mut egui::Ui, panel: &mut SeqCurvePanel, world: &mut World, play: Option<&mut World>) {
    let _span = tracing::info_span!(target: "regia_editor", "seq_curve_panel").entered();
    let all = players(world);
    let edited = player_of(world, panel);
    take_section(ui, panel, world, play, edited);
    crate::seq_tracks::authoring_rows(ui, panel, world, edited);
    // No player: one sentence, and no empty timeline or curve below it (ADR-8583).
    if player_of(world, panel).is_none() {
        ui.label("No sequence player: open or create a sequence above.");
        return;
    }
    crate::seq_timeline::timeline_section(ui, panel, world);
    let edited = player_of(world, panel);
    let Some(entity) = edited else {
        return;
    };
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return;
    };
    let sequence = player.sequence().clone();
    let playhead = player.time() as f32;
    if panel.path_player != Some(entity) {
        panel.path_player = Some(entity);
        panel.path_text = panel.paths.get(&entity).cloned().unwrap_or_default();
    }
    ui.horizontal(|ui| {
        let label = |e: Entity| format!("player {}", e.to_bits());
        let combo = egui::ComboBox::from_label("Player")
            .selected_text(label(entity))
            .show_ui(ui, |ui| {
                for &e in &all {
                    if ui.selectable_label(e == entity, label(e)).clicked() {
                        panel.player = Some(e);
                        panel.selected = None;
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Player");
        let name = |i: usize| {
            sequence.tracks.get(i).map_or_else(
                || "(none)".to_string(),
                |t| format!("{} ({})", t.name, t.keys.kind()),
            )
        };
        let combo = egui::ComboBox::from_label("Track")
            .selected_text(name(panel.track))
            .show_ui(ui, |ui| {
                for (i, track) in sequence.tracks.iter().enumerate() {
                    // Curves and section strips; bool tracks have neither.
                    if matches!(track.keys, Channel::Bool(_)) {
                        continue;
                    }
                    if ui.selectable_value(&mut panel.track, i, name(i)).changed() {
                        panel.selected = None;
                        panel.component = 0;
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Track");
        if matches!(
            sequence.tracks.get(panel.track).map(|t| &t.keys),
            Some(Channel::Vec3(_))
        ) {
            for (c, label) in COMPONENTS.iter().enumerate() {
                if ui.selectable_label(panel.component == c, *label).clicked() {
                    panel.component = c;
                    panel.selected = None;
                }
            }
        }
    });
    ui.horizontal(|ui| {
        let field =
            ui.add(egui::TextEdit::singleline(&mut panel.path_text).hint_text("project path of a .seq.ron"));
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Sequence path");
        let text = panel.path_text.trim().to_string();
        if ui.button("Open").clicked() {
            panel.error = open_sequence(world, panel, &text).err().map(|e| e.message);
        }
        if ui.button("Save").clicked() {
            let path = (!text.is_empty()).then_some(text.as_str());
            panel.error = save_sequence(world, panel, path).err().map(|e| e.message);
        }
    });
    let all_curves = sequence_curves(&sequence);
    curves_row(ui, panel, &sequence, &all_curves);
    let Some(track) = sequence.tracks.get(panel.track) else {
        ui.label("Pick a Float, Vec3, Clip, Audio, Sub or Video track.");
        return;
    };
    if !track.keys.is_keyed() {
        // Clip, audio, sub-sequence and video tracks: the section strip (ADR-2135).
        crate::seq_sections::section_strip(
            ui,
            panel,
            world,
            entity,
            (&track.name, &track.keys),
            playhead,
            sequence.fps,
        );
        return;
    }
    let Some(points) = curve_points(&track.keys, panel.component) else {
        ui.label(format!(
            "Track `{}` is a {} track: it has no curve.",
            track.name,
            track.keys.kind()
        ));
        return;
    };
    // An undo can leave the selection past the end of the keys.
    if panel.selected.is_some_and(|i| i >= points.len()) {
        panel.selected = None;
    }
    let listed = egui::Panel::left("seq-curve-keys")
        .resizable(true)
        .default_size(220.0)
        .show(ui, |ui| keys_list(ui, panel, &track.name, &points, sequence.fps))
        .inner;
    let was_dragging = panel.gesture.is_some();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let edited_key = seq_curve_key(panel.track, panel.component);
    let others: Vec<BackgroundCurve<'_>> = all_curves
        .iter()
        .enumerate()
        .filter_map(|(i, (t, c, _))| {
            let key = seq_curve_key(*t, *c);
            (key != edited_key && panel.curves.visible(&key)).then(|| BackgroundCurve {
                channel: &sequence.tracks[*t].keys,
                component: *c,
                color: crate::anim::curve_view::curve_color(&tokens, i),
                locked: panel.curves.is_locked(&key),
            })
        })
        .collect();
    let drawn = curve_widget(ui, &track.keys, panel, &points, playhead, &others);
    let mut action = listed.or(drawn);
    // A locked curve scrubs but does not edit (P-208).
    if panel.curves.is_locked(&edited_key) && matches!(action, Some(WidgetAction::Edit(_))) {
        action = None;
        panel.error = Some("this curve is locked: unlock it in the curve list to edit it".into());
    }
    if was_dragging && panel.gesture.is_none() {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
    if let Some(action) = action {
        panel.error = None;
        match action {
            WidgetAction::Edit(edit) => {
                if let Err(e) = edit_selected(world, panel, edit) {
                    panel.error = Some(e.message);
                }
            }
            WidgetAction::Scrub(time) => {
                if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
                    player.seek(f64::from(time));
                }
            }
        }
    }
    if let Some(error) = &panel.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
}

/// What the widget did this frame.
enum WidgetAction {
    Edit(KeyEdit),
    Scrub(f32),
}

fn bounds(points: &[(f32, f32)]) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for &(t, v) in points {
        b = [b[0].min(t), b[1].min(v), b[2].max(t), b[3].max(v)];
    }
    if points.is_empty() {
        b = [0.0, 0.0, 1.0, 1.0];
    }
    [b[0], b[1], b[2].max(b[0] + 1.0), b[3].max(b[1] + 0.5)]
}

/// The row (and accessible) name of key `i` of `track`, at `time` seconds.
#[must_use]
pub fn key_row_name(track: &str, i: usize, time: f32) -> String {
    format!("{track} key {i} at {time:.3} s")
}

/// One frame of the sequence, seconds (1/30 when the sequence has no rate).
#[must_use]
pub fn frame_step(fps: f32) -> f32 {
    if fps > 0.0 { 1.0 / fps } else { 1.0 / 30.0 }
}

/// The focusable list of the curve's keys (ADR-7383): Enter selects, Left / Right move the key by
/// one frame and Delete removes it, each press one undo entry through [`edit_selected`] (the
/// `editor.seq_curve_edit` path).
fn keys_list(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    track: &str,
    points: &[(f32, f32)],
    fps: f32,
) -> Option<WidgetAction> {
    use crate::canvas_keys::{RowKey, list_rows};
    if panel.selected.is_some() {
        panel.keys.retarget(panel.selected);
    }
    ui.strong(format!("Keys of {track}"));
    ui.weak("Left / Right: one frame · Delete");
    let name = |i: usize| {
        points
            .get(i)
            .map(|(t, _)| key_row_name(track, i, *t))
            .unwrap_or_default()
    };
    let height = ui.available_height().max(60.0);
    let selected = panel.selected;
    let events = list_rows(
        ui,
        "seq-curve-keys",
        &mut panel.keys,
        points.len(),
        height,
        selected,
        &name,
    );
    let mut action = None;
    for event in events {
        match event {
            RowKey::Activate(i) => panel.selected = Some(i),
            RowKey::Delete(i) => action = Some(WidgetAction::Edit(KeyEdit::Remove { index: i })),
            RowKey::Arrow {
                row, dx, alt: false, ..
            } if dx != 0 => {
                if let Some(&(t, value)) = points.get(row) {
                    panel.selected = Some(row);
                    action = Some(WidgetAction::Edit(KeyEdit::Move {
                        index: row,
                        time: (t + dx as f32 * frame_step(fps)).max(0.0),
                        value,
                    }));
                }
            }
            _ => {}
        }
    }
    action
}

/// The multi-curve key of component `component` of track `track` (P-208).
#[must_use]
pub fn seq_curve_key(track: usize, component: usize) -> String {
    format!("track:{track}:{component}")
}

/// Every curve of `sequence`: `(track index, component, label)`, in track order (a vec3 track
/// gives three, `name.x` to `name.z`). Palette colors follow this order.
#[must_use]
pub fn sequence_curves(sequence: &Sequence) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    for (t, track) in sequence.tracks.iter().enumerate() {
        match &track.keys {
            Channel::Float(_) => out.push((t, 0, track.name.clone())),
            Channel::Vec3(_) => {
                for (c, axis) in COMPONENTS.iter().enumerate() {
                    out.push((t, c, format!("{}.{axis}", track.name)));
                }
            }
            _ => {}
        }
    }
    out
}

/// The curve list of the multi-curve view: pick the edited curve, show or hide and lock each
/// curve (focusable checkboxes), and frame every visible curve.
fn curves_row(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    sequence: &Sequence,
    all: &[(usize, usize, String)],
) {
    if all.len() < 2 {
        return;
    }
    let tokens = crate::theme::ui_tokens(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        ui.strong("Curves");
        for (i, (t, c, label)) in all.iter().enumerate() {
            let key = seq_curve_key(*t, *c);
            let color = crate::anim::curve_view::curve_color(&tokens, i);
            let edited = panel.track == *t && panel.component == *c;
            if ui
                .selectable_label(edited, egui::RichText::new(label).color(color))
                .clicked()
            {
                panel.track = *t;
                panel.component = *c;
                panel.selected = None;
            }
            let mut visible = panel.curves.visible(&key);
            let r = crate::a11y_audit::name(
                ui.checkbox(&mut visible, ""),
                egui::WidgetType::Checkbox,
                &format!("Show {label}"),
            );
            if r.changed()
                && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("seq-curve-show", &key)))
            {
                panel.curves.set_visible(&key, visible);
            }
            let mut locked = panel.curves.is_locked(&key);
            let r = crate::a11y_audit::name(
                ui.checkbox(&mut locked, "lock"),
                egui::WidgetType::Checkbox,
                &format!("Lock {label}"),
            );
            if r.changed()
                && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("seq-curve-lock", &key)))
            {
                panel.curves.set_locked(&key, locked);
            }
        }
        if ui.button("Frame all").clicked() {
            let sets: Vec<Vec<(f32, f32)>> = all
                .iter()
                .filter(|(t, c, _)| {
                    (panel.track == *t && panel.component == *c)
                        || panel.curves.visible(&seq_curve_key(*t, *c))
                })
                .filter_map(|(t, c, _)| curve_points(&sequence.tracks[*t].keys, *c))
                .collect();
            panel.view.zoom_to_fit(
                crate::anim::curve_view::frame_all(sets.iter().map(Vec::as_slice)),
                0.1,
            );
        }
    });
}

/// A curve drawn behind the edited one.
struct BackgroundCurve<'a> {
    channel: &'a Channel,
    component: usize,
    color: egui::Color32,
    locked: bool,
}

fn curve_widget(
    ui: &mut egui::Ui,
    channel: &Channel,
    panel: &mut SeqCurvePanel,
    points: &[(f32, f32)],
    playhead: f32,
    others: &[BackgroundCurve<'_>],
) -> Option<WidgetAction> {
    let id = ui.id().with("seq_curve_panel");
    let size = ui.available_size().max(egui::vec2(120.0, 80.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Sequence curve, {} keys", points.len()),
    );
    let view = &mut panel.view;
    let fresh = view.rect.width() < 2.0 || ui.data(|d| d.get_temp::<bool>(id.with("fitted"))).is_none();
    view.rect = rect;
    if fresh {
        view.zoom_to_fit(bounds(points), 0.1);
        ui.data_mut(|d| d.insert_temp(id.with("fitted"), true));
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    view.paint_grid(&painter, |x| format!("{x:.2}s"), true);
    // The other visible curves, behind the edited one (P-208).
    for other in others {
        crate::anim::curve_view::paint_background_curve(
            &painter,
            view,
            &|t| other.channel.sample_component(t, other.component),
            &curve_points(other.channel, other.component).unwrap_or_default(),
            other.color,
            other.locked,
        );
    }
    let component = panel.component;
    // The curve, sampled per two pixels with the engine's own sampling.
    let steps = (rect.width() / 2.0).max(8.0) as usize;
    let line: Vec<egui::Pos2> = (0..=steps)
        .filter_map(|i| {
            let sx = rect.left() + rect.width() * i as f32 / steps as f32;
            let t = view.to_value(egui::pos2(sx, 0.0)).x;
            channel
                .sample_component(t, component)
                .map(|v| view.to_screen(egui::vec2(t, v)))
        })
        .collect();
    let color = [
        egui::Color32::from_rgb(235, 110, 100),
        egui::Color32::from_rgb(120, 220, 120),
        egui::Color32::from_rgb(110, 160, 255),
    ][component.min(2)];
    painter.add(egui::Shape::line(line, egui::Stroke::new(2.0, color)));
    for (i, &(t, v)) in points.iter().enumerate() {
        let s = view.to_screen(egui::vec2(t, v));
        if let Some((incoming, outgoing)) = channel.tangent_slopes(i, component) {
            for (show, slope, side) in [
                (shows_handle(channel, i, false), incoming, -1.0),
                (shows_handle(channel, i, true), outgoing, 1.0),
            ] {
                if show {
                    let h = handle_position(view, s, slope, side);
                    painter.line_segment([s, h], egui::Stroke::new(1.0, egui::Color32::from_gray(160)));
                    painter.circle_filled(h, 3.5, egui::Color32::from_rgb(230, 200, 90));
                }
            }
        }
        let fill = if panel.selected == Some(i) {
            crate::theme::ui_tokens(painter.ctx()).accents.active
        } else {
            egui::Color32::WHITE
        };
        painter.rect_filled(
            egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE)),
            1.0,
            fill,
        );
    }
    let x = view.to_screen(egui::vec2(playhead, 0.0)).x;
    painter.line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(1.5, crate::theme::ui_tokens(painter.ctx()).accents.alert),
    );

    if response.hovered() {
        let (scroll, shift, ctrl) =
            ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.shift, i.modifiers.ctrl));
        if scroll != 0.0
            && let Some(anchor) = response.hover_pos()
        {
            let k = (1.0 + scroll * 0.002).clamp(0.5, 2.0);
            let factor = if shift {
                egui::vec2(k, 1.0)
            } else if ctrl {
                egui::vec2(1.0, k)
            } else {
                egui::vec2(k, k)
            };
            view.zoom_about(anchor, factor);
        }
    }
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle)
    {
        view.pan(response.drag_delta());
        return None;
    }
    let drag_id = id.with("drag");
    if response.drag_started_by(egui::PointerButton::Primary) {
        // Where the button went down: a drag only starts once the pointer has left that spot.
        let hit = ui
            .input(|i| i.pointer.press_origin())
            .or(response.interact_pointer_pos())
            .and_then(|p| pick(channel, component, view, p));
        ui.data_mut(|d| d.insert_temp(drag_id, hit));
        if let Some(CurvePick::Key(i) | CurvePick::In(i) | CurvePick::Out(i)) = hit {
            panel.selected = Some(i);
            panel.gestures += 1;
            panel.gesture = Some(panel.gestures);
        }
    }
    let dragging: Option<CurvePick> = ui.data(|d| d.get_temp::<Option<CurvePick>>(drag_id)).flatten();
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<CurvePick>>(drag_id));
        panel.gesture = None;
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
    {
        let v = view.to_value(p);
        return match dragging {
            // The selection follows the key when the drag carries it past a neighbor.
            Some(CurvePick::Key(i)) => Some(WidgetAction::Edit(KeyEdit::Move {
                index: panel.selected.unwrap_or(i),
                time: v.x.max(0.0),
                value: v.y,
            })),
            Some(CurvePick::In(i) | CurvePick::Out(i)) => {
                let &(t, value) = points.get(i)?;
                let s = view.to_screen(egui::vec2(t, value));
                // The handle is dragged on its own side; a pointer across the key flips the side.
                let outgoing = matches!(dragging, Some(CurvePick::Out(_)));
                let slope = slope_from_handle(view, s, p);
                let broken = ui.input(|i| i.modifiers.alt);
                handle_edit(channel, component, i, outgoing, slope, broken).map(WidgetAction::Edit)
            }
            None => Some(WidgetAction::Scrub(v.x.max(0.0))),
        };
    }
    if response.double_clicked()
        && let Some(p) = response.interact_pointer_pos()
        && pick(channel, component, view, p).is_none()
    {
        let v = view.to_value(p);
        return Some(WidgetAction::Edit(KeyEdit::Add {
            time: v.x.max(0.0),
            value: v.y,
        }));
    }
    if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        panel.selected = match pick(channel, component, view, p) {
            Some(CurvePick::Key(i) | CurvePick::In(i) | CurvePick::Out(i)) => Some(i),
            None => None,
        };
    }
    // Right-click selects: the menu acts on that key whatever the pointer does inside it.
    if response.secondary_clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        panel.selected = match pick(channel, component, view, p) {
            Some(CurvePick::Key(i) | CurvePick::In(i) | CurvePick::Out(i)) => Some(i),
            None => None,
        };
    }
    let mut action = None;
    let selected = panel.selected;
    response.context_menu(|ui| {
        let Some(i) = selected else {
            if ui.button("Zoom to fit").clicked() {
                panel.view.zoom_to_fit(bounds(points), 0.1);
                ui.close();
            }
            return;
        };
        ui.label(format!("Key {i}"));
        ui.separator();
        ui.label("Ease to the next key");
        for (label, ease) in [
            ("Linear", Ease::Linear),
            ("Step", Ease::Step),
            ("Smooth", Ease::Smooth),
            ("Bezier", Ease::Bezier),
        ] {
            if ui.button(label).clicked() {
                action = Some(WidgetAction::Edit(KeyEdit::Ease { index: i, ease }));
                ui.close();
            }
        }
        ui.separator();
        ui.label("Tangent");
        let (incoming, outgoing) = channel.tangent_slopes(i, component).unwrap_or((0.0, 0.0));
        for (label, tangent) in [
            ("Auto", TangentEdit::Auto),
            ("Flat", TangentEdit::Flat),
            ("Linear", TangentEdit::Linear),
            ("Aligned", TangentEdit::Aligned(0.5 * (incoming + outgoing))),
            ("Broken", TangentEdit::Broken(incoming, outgoing)),
        ] {
            if ui.button(label).clicked() {
                action = Some(WidgetAction::Edit(KeyEdit::Tangent { index: i, tangent }));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Delete key").clicked() {
            action = Some(WidgetAction::Edit(KeyEdit::Remove { index: i }));
            ui.close();
        }
    });
    action
}

// ---- agent methods ----

const PLAYER: ParamSchema = ParamSchema {
    name: "player",
    kind: "number",
    required: false,
    description: "Sequence player entity id to select (default: the panel's player, else the first).",
};
const TRACK: ParamSchema = ParamSchema {
    name: "track",
    kind: "string",
    required: false,
    description: "Track name to select (a Float or Vec3 track).",
};
const COMPONENT: ParamSchema = ParamSchema {
    name: "component",
    kind: "number",
    required: false,
    description: "Component to select: 0 for a float track, 0..3 (x, y, z) for a vec3 track.",
};
const SAMPLES: ParamSchema = ParamSchema {
    name: "samples",
    kind: "number",
    required: false,
    description: "Return this many evenly spaced curve samples over the keys (2..=1024; default none).",
};

/// The agent methods of the panel.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.seq_curve_state",
                tool: "seq_curve_state",
                description: "Read the Sequence Curves panel: the players, the selected player, track and component, \
                    that curve's keys (time, value, ease, tangent mode, resolved slopes, weights) and, with \
                    `samples`, the curve sampled the way it plays. Optionally selects a player, track and component.",
                params: &[PLAYER, TRACK, COMPONENT, SAMPLES],
                result: "{ players: [id], player, path, dirty, tracks: [{ name, kind }], track, track_name, kind, component, components, \
                    selected, keys: [{ time, value, ease, tangent, slopes: [in, out], weights }], samples: [[time, value]], error }",
                mutating: false,
                permission: Permission::Read,
            },
            seq_curve_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_curve_edit",
                tool: "seq_curve_edit",
                description: "Edit one key of the selected curve, as the Sequence Curves panel would: add, move, remove, \
                    ease, tangent (auto, flat, linear, aligned with `slope`, broken with `in_slope` and `out_slope`) or \
                    weights. The whole sequence is validated first; a refused edit changes nothing.",
                params: &[
                    PLAYER,
                    TRACK,
                    COMPONENT,
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "add, move, remove, ease, tangent or weights.",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: false,
                        description: "move, remove, ease, tangent, weights: the key index.",
                    },
                    ParamSchema {
                        name: "time",
                        kind: "number",
                        required: false,
                        description: "add, move: seconds on the sequence clock.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "number",
                        required: false,
                        description: "add, move: the component's value.",
                    },
                    ParamSchema {
                        name: "ease",
                        kind: "string",
                        required: false,
                        description: "ease: Linear, Step, Smooth or Bezier.",
                    },
                    ParamSchema {
                        name: "tangent",
                        kind: "string",
                        required: false,
                        description: "tangent: auto, flat, linear, aligned or broken.",
                    },
                    ParamSchema {
                        name: "slope",
                        kind: "number",
                        required: false,
                        description: "tangent aligned: value units per second, both sides.",
                    },
                    ParamSchema {
                        name: "in_slope",
                        kind: "number",
                        required: false,
                        description: "tangent broken: the incoming slope.",
                    },
                    ParamSchema {
                        name: "out_slope",
                        kind: "number",
                        required: false,
                        description: "tangent broken: the outgoing slope.",
                    },
                    ParamSchema {
                        name: "weights",
                        kind: "array",
                        required: false,
                        description: "weights: [incoming, outgoing] handle lengths in (0, 1], or null for unweighted.",
                    },
                ],
                result: "{ applied, selected, keys: [{ time, value, ease, tangent, slopes, weights }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_curve_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_curve_open",
                tool: "seq_curve_open",
                description: "Open a project .seq.ron in the Sequence Curves panel: read it (and its sub-sequences \
                    into the sequence library), spawn a stopped sequence player of it and select it.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path ending in .seq.ron.",
                }],
                result: "{ player, path, tracks: [{ name, kind }], sub_sequences: [name] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_curve_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_curve_save",
                tool: "seq_curve_save",
                description: "Save the panel's sequence as .seq.ron with an atomic write (`path`, or where it was \
                    opened from or last saved to).",
                params: &[
                    PLAYER,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Project-relative path ending in .seq.ron (default: the panel's path).",
                    },
                ],
                result: "{ saved: path, version }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_curve_save as Handler,
        ),
    ]
}

pub(crate) fn rpc(e: EditError) -> RpcError {
    let hint = match e.code {
        "seq_curve_no_player" => "sequence.player { action: \"load\", seq_ron: ... } creates one",
        "seq_curve_bad_track" | "seq_curve_not_a_curve" => {
            "editor.seq_curve_state lists the tracks; only Float and Vec3 tracks have curves"
        }
        "seq_curve_key_time_taken" => "move or remove the key at that time first",
        "seq_curve_last_key" => "disable the track instead of removing its last key",
        "seq_curve_bad_path" => "use a project-relative path ending in .seq.ron, without `..`",
        "seq_curve_no_project" => "open a project first",
        _ => "the message says what is wrong; nothing was changed",
    };
    let class = match e.code {
        "seq_curve_no_player" | "seq_curve_bad_track" | "seq_curve_bad_key" => {
            regia_agent::protocol::NOT_FOUND
        }
        _ => regia_agent::protocol::INVALID_PARAMS,
    };
    RpcError::new(class, e.code, e.message, hint)
}

/// Selects the player, track and component named in `params` on `panel`.
pub(crate) fn select(world: &mut World, panel: &mut SeqCurvePanel, params: &Value) -> Result<(), EditError> {
    if let Some(value) = params.get("player").filter(|v| !v.is_null()) {
        let entity = value.as_u64().and_then(Entity::try_from_bits);
        match entity {
            Some(e) if world.get::<SequencePlayer>(e).is_some() => {
                if panel.player != Some(e) {
                    panel.selected = None;
                }
                panel.player = Some(e);
            }
            _ => return refuse("seq_curve_no_player", format!("{value} is not a sequence player")),
        }
    }
    if let Some(value) = params.get("track").filter(|value| !value.is_null()) {
        let Some(name) = value.as_str() else {
            return refuse(
                "seq_curve_bad_track",
                "`track` is a track name; a number or any other non-string is not an index",
            );
        };
        let entity = player_of(world, panel);
        let index = entity
            .and_then(|e| world.get::<SequencePlayer>(e))
            .and_then(|p| p.sequence().tracks.iter().position(|t| t.name == name));
        let Some(index) = index else {
            return refuse("seq_curve_bad_track", format!("no track named `{name}`"));
        };
        if index != panel.track {
            panel.selected = None;
            panel.component = 0;
        }
        panel.track = index;
    }
    if let Some(value) = params.get("component").filter(|v| !v.is_null()) {
        let Some(c) = value
            .as_u64()
            .and_then(|c| usize::try_from(c).ok())
            .filter(|&c| c < 3)
        else {
            return refuse("seq_curve_bad_component", "`component` is 0, 1 or 2");
        };
        if c != panel.component {
            panel.selected = None;
        }
        panel.component = c;
    }
    Ok(())
}

fn tangent_name(channel: &Channel, i: usize) -> &'static str {
    match channel {
        Channel::Float(keys) => keys.get(i).map_or("auto", |k| k.tangent.mode_name()),
        Channel::Vec3(keys) => keys.get(i).map_or("auto", |k| k.tangent.mode_name()),
        _ => "auto",
    }
}

fn keys_json(channel: &Channel, component: usize) -> Value {
    let Some(points) = curve_points(channel, component) else {
        return json!([]);
    };
    let eases = eases(channel);
    let weights = |i: usize| match channel {
        Channel::Float(keys) => keys.get(i).and_then(|k| k.weights),
        Channel::Vec3(keys) => keys.get(i).and_then(|k| k.weights),
        _ => None,
    };
    json!(
        points
            .iter()
            .enumerate()
            .map(|(i, &(time, value))| {
                let slopes = channel.tangent_slopes(i, component).unwrap_or((0.0, 0.0));
                json!({
                    "time": time,
                    "value": value,
                    "ease": format!("{:?}", eases.get(i).copied().unwrap_or_default()),
                    "tangent": tangent_name(channel, i),
                    "slopes": [slopes.0, slopes.1],
                    "weights": weights(i).map(|(a, b)| json!([a, b])),
                })
            })
            .collect::<Vec<_>>()
    )
}

fn state_json(world: &mut World, panel: &SeqCurvePanel, samples: Option<usize>) -> Value {
    let all: Vec<u64> = players(world).into_iter().map(Entity::to_bits).collect();
    // No player: the documented `path` and `dirty` still answer (`null`, false).
    let none = |panel: &SeqCurvePanel| json!({ "players": all, "player": null, "path": null, "dirty": false, "error": panel.error });
    let Some(entity) = player_of(world, panel) else {
        return none(panel);
    };
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return none(panel);
    };
    let sequence = player.sequence();
    // The document's file and dirty flag (`crate::seq_autosave`): `null` / false for a player
    // that was never opened from or saved to a file.
    let path = panel.paths.get(&entity).cloned();
    let dirty = path.is_some() && crate::seq_autosave::is_dirty(world, entity);
    let track = sequence.tracks.get(panel.track);
    let channel = track.map(|t| &t.keys);
    let components = match channel {
        Some(Channel::Float(_)) => 1,
        Some(Channel::Vec3(_)) => 3,
        _ => 0,
    };
    let curve = channel.and_then(|c| curve_points(c, panel.component));
    let sampled = match (samples, channel, &curve) {
        (Some(n), Some(c), Some(points)) if !points.is_empty() => {
            let start = points.first().map_or(0.0, |p| p.0);
            let end = points.last().map_or(1.0, |p| p.0).max(start + 1.0e-3);
            Some(
                (0..n)
                    .map(|i| {
                        let t = start + (end - start) * i as f32 / (n - 1) as f32;
                        json!([t, c.sample_component(t, panel.component)])
                    })
                    .collect::<Vec<_>>(),
            )
        }
        _ => None,
    };
    json!({
        "players": all,
        "player": entity.to_bits(),
        "path": path,
        "dirty": dirty,
        "time": player.time(),
        "tracks": sequence.tracks.iter().map(|t| json!({ "name": t.name, "kind": t.keys.kind() })).collect::<Vec<_>>(),
        "track": panel.track,
        "track_name": track.map(|t| t.name.clone()),
        "kind": channel.map(Channel::kind),
        "component": panel.component,
        "components": components,
        "has_curve": curve.is_some(),
        "selected": panel.selected,
        "keys": channel.map_or(json!([]), |c| keys_json(c, panel.component)),
        "samples": sampled,
        "error": panel.error,
    })
}

fn samples_param(params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("samples").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (2..=1024).contains(n))
            .map(|n| Some(n as usize))
            .ok_or_else(|| RpcError::invalid_params("`samples` must be a whole number in 2..=1024")),
    }
}

fn seq_curve_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let samples = samples_param(params)?;
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let selected = select(world, &mut panel, params);
    let out = match selected {
        Ok(()) => Ok(state_json(world, &panel, samples)),
        Err(e) => Err(rpc(e)),
    };
    world.insert_resource(panel);
    out
}

pub(crate) fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
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

/// The edit an `editor.seq_curve_edit` call asks for.
pub fn edit_of_params(params: &Value) -> Result<KeyEdit, RpcError> {
    let op = params.get("op").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`op` is required: add, move, remove, ease, tangent or weights")
    })?;
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
                Some("Bezier") => Ease::Bezier,
                _ => {
                    return Err(RpcError::invalid_params(
                        "`ease` is Linear, Step, Smooth or Bezier",
                    ));
                }
            };
            Ok(KeyEdit::Ease {
                index: index_param(params)?,
                ease,
            })
        }
        "tangent" => {
            let tangent = match params.get("tangent").and_then(Value::as_str) {
                Some("auto") => TangentEdit::Auto,
                Some("flat") => TangentEdit::Flat,
                Some("linear") => TangentEdit::Linear,
                Some("aligned") => TangentEdit::Aligned(need(number(params, "slope")?, "slope")?),
                Some("broken") => TangentEdit::Broken(
                    need(number(params, "in_slope")?, "in_slope")?,
                    need(number(params, "out_slope")?, "out_slope")?,
                ),
                _ => {
                    return Err(RpcError::invalid_params(
                        "`tangent` is auto, flat, linear, aligned or broken",
                    ));
                }
            };
            Ok(KeyEdit::Tangent {
                index: index_param(params)?,
                tangent,
            })
        }
        "weights" => {
            let weights = match params.get("weights") {
                None | Some(Value::Null) => None,
                Some(Value::Array(pair)) if pair.len() == 2 => {
                    let w = |v: &Value| v.as_f64().map(|f| f as f32);
                    match (w(&pair[0]), w(&pair[1])) {
                        (Some(a), Some(b)) => Some((a, b)),
                        _ => {
                            return Err(RpcError::invalid_params(
                                "`weights` is [incoming, outgoing] or null",
                            ));
                        }
                    }
                }
                Some(_) => {
                    return Err(RpcError::invalid_params(
                        "`weights` is [incoming, outgoing] or null",
                    ));
                }
            };
            Ok(KeyEdit::Weights {
                index: index_param(params)?,
                weights,
            })
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown `op` `{other}` (add, move, remove, ease, tangent, weights)"
        ))),
    }
}

fn seq_curve_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let opened = open_sequence(world, &mut panel, path);
    world.insert_resource(panel);
    let entity = opened.map_err(rpc)?;
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(RpcError::internal("the opened player is gone"));
    };
    let sequence = player.sequence();
    Ok(json!({
        "player": entity.to_bits(),
        "path": path,
        "tracks": sequence.tracks.iter().map(|t| json!({ "name": t.name, "kind": t.keys.kind() })).collect::<Vec<_>>(),
        "sub_sequences": sequence.sub_sequence_names(),
    }))
}

fn seq_curve_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`path` must be a string"))?,
        ),
    };
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let saved = select(world, &mut panel, params).and_then(|()| save_sequence(world, &mut panel, path));
    let version = player_of(world, &panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.sequence().version);
    world.insert_resource(panel);
    let saved = saved.map_err(rpc)?;
    Ok(json!({ "saved": saved, "version": version }))
}

fn seq_curve_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let edit = edit_of_params(params)?;
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    // An agent call is its own undo entry, even while a pointer drag is open (ADR-2135).
    let live = panel.gesture.take();
    let result = select(world, &mut panel, params).and_then(|()| edit_selected(world, &mut panel, edit));
    let out = match result {
        Ok(()) => {
            let keys = player_of(world, &panel)
                .and_then(|e| world.get::<SequencePlayer>(e))
                .and_then(|p| p.sequence().tracks.get(panel.track))
                .map_or(json!([]), |t| keys_json(&t.keys, panel.component));
            Ok(json!({ "applied": true, "selected": panel.selected, "keys": keys }))
        }
        Err(e) => Err(rpc(e)),
    };
    panel.gesture = live;
    world.insert_resource(panel);
    out
}

/// The panel's take recorder section (ADR-2637): which bindings and channels to record, the
/// countdown, and the last result. Record and Stop run the same recorder as the agent's
/// `sequence.record_start` / `sequence.record_stop`.
#[derive(Resource, Debug, Clone)]
pub struct SeqTakePanel {
    /// Target names of the edited player's binding table to record.
    pub targets: BTreeSet<String>,
    /// Records the active camera as the binding `camera`.
    pub camera: bool,
    /// Records the transform of each picked target.
    pub transform: bool,
    /// Records the dominant animation clip of each picked target.
    pub animation: bool,
    /// Property paths to record on each picked target, comma-separated.
    pub properties: String,
    /// Records [`regia_framework::sequencer::TakeCue`] messages as audio sections.
    pub audio_cues: bool,
    /// Also records targets a playing sequence drives.
    pub include_driven: bool,
    /// Gameplay frames to count down before the first sample.
    pub countdown: u32,
    /// The source sequence the take is saved next to (empty: the panel's path).
    pub source: String,
    /// The last take written or the last refusal.
    pub status: Option<Result<String, String>>,
    /// The edited player's clock when Record was pressed, if it plays the take's source: where
    /// the take's section goes in the source.
    pub started_at: Option<f32>,
}

impl Default for SeqTakePanel {
    fn default() -> Self {
        Self {
            targets: BTreeSet::new(),
            camera: false,
            transform: true,
            animation: false,
            properties: String::new(),
            audio_cues: false,
            include_driven: false,
            countdown: 60,
            source: String::new(),
            status: None,
            started_at: None,
        }
    }
}

/// The recording the take panel asks for: the picked targets of `entity`'s binding table with
/// the picked channels, and the active camera when asked.
///
/// `in_play` is `true` when the take records the PIE play world, a copy of the edit world: only
/// guid bindings name the same entity there, so an entity-bound target is refused.
pub fn take_config(
    world: &World,
    take: &SeqTakePanel,
    entity: Option<Entity>,
    in_play: bool,
) -> Result<TakeConfig, TakeError> {
    let mut channels = Vec::new();
    if take.transform {
        channels.push(TakeChannel::Transform);
    }
    if take.animation {
        channels.push(TakeChannel::Animation);
    }
    for path in take
        .properties
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let channel = if path.starts_with(regia_framework::sequencer::recorder::REFLECT_PREFIX) {
            TakeChannel::parse(path)?
        } else {
            TakeChannel::parse(&format!("property:{path}"))?
        };
        channels.push(channel);
    }
    let table = entity.and_then(|e| world.get::<SequenceBindings>(e));
    let mut bindings = Vec::new();
    for name in &take.targets {
        let Some(binding) = table.and_then(|t| t.get(name)) else {
            return Err(TakeError::BadBinding(format!(
                "`{name}` is not bound on the edited player"
            )));
        };
        if in_play && matches!(binding, Binding::Entity(_)) {
            return Err(TakeError::BadBinding(format!(
                "`{name}` is bound to an entity id, which names another entity in the play world; bind it by guid to record in PIE"
            )));
        }
        bindings.push(TakeBinding {
            name: name.clone(),
            target: TakeTarget::Binding(binding),
            channels: channels.clone(),
        });
    }
    if take.camera {
        bindings.push(TakeBinding {
            name: "camera".into(),
            target: TakeTarget::ActiveCamera,
            channels: vec![TakeChannel::Camera],
        });
    }
    let config = TakeConfig {
        bindings,
        audio_cues: take.audio_cues,
        countdown: take.countdown,
        include_driven: take.include_driven,
        ..TakeConfig::default()
    };
    config.validate()?;
    Ok(config)
}

/// **Record**: starts the take the panel describes, in the PIE play world while one runs (`play`),
/// else in the edit world.
pub fn start_take(
    world: &mut World,
    play: Option<&mut World>,
    panel: &SeqCurvePanel,
    take: &mut SeqTakePanel,
) -> Result<(), TakeError> {
    let entity = player_of(world, panel);
    let config = take_config(world, take, entity, play.is_some())?;
    let source = take_source(panel, take);
    let started_at = entity
        .filter(|e| panel.paths.get(e) == Some(&source))
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map(|p| p.time() as f32);
    let target = match play {
        Some(play) => play,
        None => world,
    };
    target
        .get_resource_or_insert_with(TakeRecorder::default)
        .start(config)?;
    take.started_at = started_at;
    take.status = None;
    Ok(())
}

/// The take's source sequence: the take panel's, else the curve panel's path.
fn take_source(panel: &SeqCurvePanel, take: &SeqTakePanel) -> String {
    if take.source.trim().is_empty() {
        panel.path_text.trim().to_string()
    } else {
        take.source.trim().to_string()
    }
}

/// The world a take records in: the play world while PIE runs, else the edit world.
fn take_world<'a>(world: &'a mut World, play: &'a mut Option<&mut World>) -> &'a mut World {
    match play {
        Some(play) => play,
        None => world,
    }
}

/// **Stop**: stops the take and writes it next to the source sequence (the take panel's source,
/// else the curve panel's path). When the edited player plays that source, it reloads the source
/// with its new take section. Returns the take's project path.
pub fn stop_take(
    world: &mut World,
    mut play: Option<&mut World>,
    panel: &mut SeqCurvePanel,
    take: &mut SeqTakePanel,
) -> Result<String, TakeError> {
    let source = take_source(panel, take);
    let recorded = {
        let mut recorder = take_world(world, &mut play)
            .get_resource_mut::<TakeRecorder>()
            .ok_or(TakeError::NotRecording)?;
        if recorder.status().is_active() {
            recorder.stop()?
        } else {
            // A take kept when its save failed: this is the retry.
            recorder.take_unsaved().ok_or(TakeError::NotRecording)?
        }
    };
    let mut keep = |world: &mut World, recorded: RecordedTake, e: TakeError| {
        if let Some(mut recorder) = take_world(world, &mut play).get_resource_mut::<TakeRecorder>() {
            recorder.keep_unsaved(recorded);
        }
        e
    };
    if let Err(e) = project_file(world, &source) {
        let refusal = if e.code == "seq_curve_no_project" {
            TakeError::Io(e.message)
        } else {
            TakeError::BadPath(e.message)
        };
        return Err(keep(world, recorded, refusal));
    }
    let Some(root) = project_root(world) else {
        return Err(keep(world, recorded, TakeError::Io("no project is open".into())));
    };
    let entity = player_of(world, panel);
    let at = take.started_at.unwrap_or(0.0);
    let files = match save_take(&root, &source, &recorded.sequence, at) {
        Ok(files) => files,
        Err(e) => return Err(keep(world, recorded, e)),
    };
    let mut named = recorded.sequence;
    named.name = files
        .take
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches(".seq.ron")
        .to_string();
    world
        .get_resource_or_insert_with(SequenceLibrary::default)
        .insert(files.take.clone(), named.clone());
    if let Some(play) = play {
        play.get_resource_or_insert_with(SequenceLibrary::default)
            .insert(files.take.clone(), named);
    }
    if let Some(entity) = entity
        && panel.paths.get(&entity) == Some(&source)
        && let Ok(text) = std::fs::read_to_string(root.join(&source))
        && let Ok(updated) = Sequence::from_ron(&text)
        && let Some(mut player) = world.get_mut::<SequencePlayer>(entity)
    {
        player.replace_sequence(updated.clone());
        // The player holds the file's text again (the take section went into the file).
        crate::seq_autosave::mark_saved(world, entity, updated);
    }
    take.started_at = None;
    take.status = Some(Ok(files.take.clone()));
    Ok(files.take)
}

/// Draws the take recorder section of the panel; `play` is the PIE play world while one runs.
fn take_section(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    world: &mut World,
    mut play: Option<&mut World>,
    entity: Option<Entity>,
) {
    let mut take = world.remove_resource::<SeqTakePanel>().unwrap_or_default();
    let in_play = play.is_some();
    let (status, countdown_left, frames, unsaved) = take_world(world, &mut play)
        .get_resource::<TakeRecorder>()
        .map_or((TakeStatus::Idle, 0, 0, false), |r| {
            (r.status(), r.countdown_left(), r.frames(), r.has_unsaved())
        });
    // Open while a take counts down or records, so Stop is always in view.
    let header = egui::CollapsingHeader::new("Record take").open(status.is_active().then_some(true));
    header.show(ui, |ui| {
        let targets: Vec<String> = entity
            .and_then(|e| world.get::<SequenceBindings>(e))
            .map(|t| t.iter().map(|(name, _)| name.to_string()).collect())
            .unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label("Bindings:");
            for name in &targets {
                let mut on = take.targets.contains(name);
                if ui.checkbox(&mut on, name.as_str()).changed() {
                    if on {
                        take.targets.insert(name.clone());
                    } else {
                        take.targets.remove(name);
                    }
                }
            }
            ui.checkbox(&mut take.camera, "Active camera");
        });
        take.targets.retain(|name| targets.contains(name));
        ui.horizontal(|ui| {
            ui.label("Channels:");
            ui.checkbox(&mut take.transform, "Transform");
            ui.checkbox(&mut take.animation, "Animation");
            let field = ui.add(
                egui::TextEdit::singleline(&mut take.properties)
                    .hint_text("properties: light.intensity, reflect:PointLight.radius"),
            );
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Recorded properties");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut take.audio_cues, "Audio cues");
            ui.checkbox(&mut take.include_driven, "Include driven");
            ui.label("Countdown (frames)");
            ui.add(egui::DragValue::new(&mut take.countdown).range(0..=10_000));
            let field = ui.add(
                egui::TextEdit::singleline(&mut take.source)
                    .hint_text("source .seq.ron (default: path above)"),
            );
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Take source sequence");
        });
        ui.horizontal(|ui| {
            if status.is_active() {
                if ui.button("Stop").clicked()
                    && let Err(e) = stop_take(world, play, panel, &mut take)
                {
                    take.status = Some(Err(format!("{} ({})", e, e.code())));
                }
                match status {
                    TakeStatus::Countdown => ui.label(format!("Starting in {countdown_left} frames")),
                    TakeStatus::Full => ui.label(format!("Full: {frames} frames; Stop writes them")),
                    _ => ui.colored_label(egui::Color32::LIGHT_RED, format!("Recording: {frames} frames")),
                };
            } else if unsaved {
                ui.colored_label(egui::Color32::LIGHT_RED, "A take could not be saved.");
                let save = ui.button("Save again").clicked();
                let drop_it = ui.button("Drop it").clicked();
                if save {
                    if let Err(e) = stop_take(world, play, panel, &mut take) {
                        take.status = Some(Err(format!("{} ({})", e, e.code())));
                    }
                } else if drop_it
                    && let Some(mut recorder) =
                        take_world(world, &mut play).get_resource_mut::<TakeRecorder>()
                {
                    let _ = recorder.cancel();
                    take.started_at = None;
                }
            } else if ui
                .button(if in_play { "Record (PIE)" } else { "Record" })
                .clicked()
                && let Err(e) = start_take(world, play, panel, &mut take)
            {
                take.status = Some(Err(format!("{} ({})", e, e.code())));
            }
        });
        match &take.status {
            Some(Ok(path)) => {
                ui.label(format!("Wrote {path}"));
            }
            Some(Err(message)) => {
                ui.colored_label(egui::Color32::LIGHT_RED, message);
            }
            None => {}
        }
    });
    world.insert_resource(take);
}

#[cfg(test)]
mod tests {
    use regia_framework::sequencer::{ScalarKey, Sequence, Track, Vec3Key};

    use super::*;

    fn world_with_player() -> (World, Entity) {
        let mut world = World::new();
        let sequence = Sequence {
            tracks: vec![
                Track {
                    name: "fade".into(),
                    target: "ui".into(),
                    property: "opacity".into(),
                    keys: Channel::Float(vec![
                        ScalarKey::new(0.0, 0.0, Ease::Bezier),
                        ScalarKey::new(2.0, 1.0, Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "move".into(),
                    target: "hero".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![
                        Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Bezier),
                        Vec3Key::new(1.0, [1.0, 2.0, 3.0], Ease::Bezier),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "fx".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Sub(vec![regia_framework::sequencer::SubSection {
                        sequence: "fx".into(),
                        start: 0.0,
                        end: 1.0,
                        offset: 0.0,
                        speed: 1.0,
                        bindings: Default::default(),
                        repeat: Default::default(),
                        loop_length: None,
                    }]),
                    enabled: true,
                    locked: false,
                },
            ],
            ..Sequence::default()
        };
        sequence.validate().expect("valid fixture");
        let player = world.spawn(SequencePlayer::new(sequence)).id();
        (world, player)
    }

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// The table reaches each handler under its wire name.
    #[test]
    fn every_wire_name_reaches_its_handler() {
        let (mut world, _) = world_with_player();
        let table = methods();
        let mut call = |name: &str, params: Value| {
            let (_, handler) = table.iter().find(|(s, _)| s.name == name).expect(name);
            handler(&mut world, &params)
        };
        call("editor.seq_curve_state", json!({})).expect("state");
        assert!(call("editor.seq_curve_edit", json!({ "op": "explode" })).is_err());
        assert!(call("editor.seq_curve_open", json!({})).is_err(), "no path");
        assert!(
            call("editor.seq_curve_save", json!({})).is_err(),
            "nothing is open"
        );
    }

    #[test]
    fn state_lists_tracks_keys_slopes_and_samples() {
        let (mut world, player) = world_with_player();
        let state = seq_curve_state(&mut world, &json!({ "track": "fade", "samples": 5 })).expect("state");
        assert_eq!(state["player"], json!(player.to_bits()));
        assert_eq!(state["tracks"][2]["kind"], json!("Sub"));
        assert_eq!(state["kind"], json!("Float"));
        assert_eq!(state["keys"][0]["ease"], json!("Bezier"));
        assert_eq!(state["keys"][0]["tangent"], json!("auto"));
        assert_eq!(state["keys"][0]["slopes"], json!([0.0, 0.0]));
        let samples = state["samples"].as_array().expect("samples");
        assert_eq!(samples.len(), 5);
        assert_eq!(
            samples[2],
            json!([1.0, 0.5]),
            "flat auto ends: the midpoint is half way"
        );
        let sub = seq_curve_state(&mut world, &json!({ "track": "fx" })).expect("sub track");
        assert_eq!(sub["has_curve"], json!(false));
        assert_eq!(
            code(seq_curve_state(&mut world, &json!({ "track": "nope" })).expect_err("bad")),
            "seq_curve_bad_track"
        );
        assert_eq!(
            code(seq_curve_state(&mut world, &json!({ "track": 1 })).expect_err("index")),
            "seq_curve_bad_track",
            "a numeric track is not an index"
        );
        assert_eq!(
            code(seq_curve_state(&mut world, &json!({ "component": 7 })).expect_err("bad")),
            "seq_curve_bad_component"
        );
        assert_eq!(
            code(seq_curve_state(&mut world, &json!({ "player": 12345 })).expect_err("bad")),
            "seq_curve_no_player"
        );
        assert!(seq_curve_state(&mut world, &json!({ "samples": 1 })).is_err());
    }

    #[test]
    fn edits_reach_the_player_and_refused_ones_change_nothing() {
        let (mut world, player) = world_with_player();
        let out = seq_curve_edit(
            &mut world,
            &json!({ "track": "fade", "op": "tangent", "index": 0, "tangent": "aligned", "slope": 1.5 }),
        )
        .expect("tangent");
        assert_eq!(out["keys"][0]["tangent"], json!("aligned"));
        assert_eq!(out["keys"][0]["slopes"], json!([1.5, 1.5]));
        seq_curve_edit(
            &mut world,
            &json!({ "op": "weights", "index": 0, "weights": [0.5, 0.5] }),
        )
        .expect("weights");
        seq_curve_edit(&mut world, &json!({ "op": "add", "time": 1.0, "value": 0.25 })).expect("add");
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Float(keys) = &p.sequence().tracks[0].keys else {
            panic!("float")
        };
        assert_eq!(keys.len(), 3);
        let before = keys.len();
        let refused = seq_curve_edit(&mut world, &json!({ "track": 1, "op": "remove", "index": 0 }))
            .expect_err("a number is not a track index");
        assert_eq!(code(refused), "seq_curve_bad_track");
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Float(keys) = &p.sequence().tracks[0].keys else {
            panic!("float")
        };
        assert_eq!(keys.len(), before, "the previous track was not edited");
        assert_eq!(world.resource::<SeqCurvePanel>().track, 0);
        assert_eq!(keys[0].weights, Some((0.5, 0.5)));
        assert_eq!(keys[0].tangent, Tangent::Aligned(1.5));
        let before = p.sequence().clone();
        let refused =
            |world: &mut World, params: Value| code(seq_curve_edit(world, &params).expect_err("refused"));
        assert_eq!(
            refused(&mut world, json!({ "op": "add", "time": 1.0, "value": 0.0 })),
            "seq_curve_key_time_taken"
        );
        assert_eq!(
            refused(&mut world, json!({ "op": "remove", "index": 9 })),
            "seq_curve_bad_key"
        );
        assert_eq!(
            refused(
                &mut world,
                json!({ "op": "weights", "index": 0, "weights": [2.0, 0.5] })
            ),
            "seq_curve_bad_weights"
        );
        assert_eq!(
            refused(&mut world, json!({ "track": "fx", "op": "remove", "index": 0 })),
            "seq_curve_not_a_curve"
        );
        assert!(
            seq_curve_edit(
                &mut world,
                &json!({ "track": "fade", "op": "tangent", "index": 0, "tangent": "aligned" })
            )
            .is_err()
        );
        assert!(seq_curve_edit(&mut world, &json!({ "op": "spin" })).is_err());
        assert_eq!(
            world.get::<SequencePlayer>(player).expect("player").sequence(),
            &before
        );
        // Vec3: one component at a time.
        let out = seq_curve_edit(
            &mut world,
            &json!({ "track": "move", "component": 2, "op": "move", "index": 1, "time": 1.0, "value": 9.0 }),
        )
        .expect("move z");
        assert_eq!(out["keys"][1]["value"], json!(9.0));
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Vec3(keys) = &p.sequence().tracks[1].keys else {
            panic!("vec3")
        };
        assert_eq!(keys[1].value, [1.0, 2.0, 9.0]);
    }

    fn fade_keys(world: &World, player: Entity) -> Vec<(f32, f32)> {
        let p = world.get::<SequencePlayer>(player).expect("player");
        curve_points(&p.sequence().tracks[0].keys, 0).expect("curve")
    }

    #[test]
    fn each_edit_is_an_undo_entry_and_a_drag_is_one() {
        let (mut world, player) = world_with_player();
        let start = fade_keys(&world, player);
        seq_curve_edit(&mut world, &json!({ "op": "add", "time": 1.0, "value": 0.25 })).expect("add");
        seq_curve_edit(&mut world, &json!({ "op": "remove", "index": 1 })).expect("remove");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2, "two entries");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(fade_keys(&world, player).len(), 3, "the removal came back");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(fade_keys(&world, player), start);
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(fade_keys(&world, player).len(), 3);
        assert!(crate::command::undo_world(&mut world));

        // A drag: many moves, one entry; the next drag is another.
        let mut panel = SeqCurvePanel {
            player: Some(player),
            gesture: Some(1),
            ..SeqCurvePanel::default()
        };
        for t in [0.5, 0.75, 1.0] {
            let edit = KeyEdit::Move {
                index: 0,
                time: t,
                value: t,
            };
            edit_selected(&mut world, &mut panel, edit).expect("move");
        }
        panel.gesture = Some(2);
        let edit = KeyEdit::Move {
            index: 0,
            time: 1.5,
            value: 0.0,
        };
        edit_selected(&mut world, &mut panel, edit).expect("move");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(fade_keys(&world, player)[0], (1.0, 1.0), "the second drag undone");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(fade_keys(&world, player), start, "the first drag undone whole");
    }

    fn keys_frame(
        ctx: &egui::Context,
        world: &mut World,
        events: Vec<egui::Event>,
    ) -> egui::accesskit::TreeUpdate {
        crate::canvas_keys::test_input::frame(ctx, [900.0, 600.0], events, |ui| {
            egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, world));
        })
    }

    /// FU-5 (ADR-7383): the curve is a named canvas, the panel has no unnamed widget, and the
    /// keys list moves a key by three frames with Right ×3 (three undo entries, the
    /// `editor.seq_curve_edit` path), keys only; undo takes each press back.
    #[test]
    fn the_keys_list_moves_a_key_three_frames_as_three_undo_entries() {
        use crate::a11y_audit::{unnamed_widgets, widget_names};
        use crate::canvas_keys::test_input::{focused_name, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        let (mut world, player) = world_with_player();
        world.insert_resource(UndoStack::new());
        let ctx = egui::Context::default();
        keys_frame(&ctx, &mut world, vec![]);
        let first = keys_frame(&ctx, &mut world, vec![]);
        assert_eq!(
            unnamed_widgets(&first),
            Vec::<String>::new(),
            "{:?}",
            widget_names(&first)
        );
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas).contains(&"Sequence curve, 2 keys".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        tab_to(&ctx, [900.0, 600.0], "fade key 1 at 2.000 s", false, 100, |ui| {
            egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, &mut world));
        });
        for _ in 0..3 {
            keys_frame(&ctx, &mut world, vec![key(Key::ArrowRight, Modifiers::NONE)]);
        }
        let moved = keys_frame(&ctx, &mut world, vec![]);
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            3,
            "one entry per press"
        );
        let t = fade_keys(&world, player)[1].0;
        assert!((t - 2.1).abs() < 1e-4, "three frames at 30 fps: {t}");
        assert_eq!(
            focused_name(&moved).as_deref(),
            Some("fade key 1 at 2.100 s"),
            "the focus stays on the key"
        );
        for expect in [2.0 + 2.0 / 30.0, 2.0 + 1.0 / 30.0, 2.0] {
            assert!(crate::command::undo_world(&mut world));
            assert!((fade_keys(&world, player)[1].0 - expect).abs() < 1e-4);
        }
        // Delete removes the focused key (one more entry).
        keys_frame(&ctx, &mut world, vec![]);
        keys_frame(&ctx, &mut world, vec![key(Key::Delete, Modifiers::NONE)]);
        assert_eq!(fade_keys(&world, player).len(), 1);
    }

    /// The section strip's list moves a section by three frames (three entries, the
    /// `editor.seq_section_edit` path) with keys only, and names the strip.
    #[test]
    fn the_section_list_moves_a_section_three_frames_by_keyboard() {
        use crate::canvas_keys::test_input::{key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        let (mut world, player) = world_with_player();
        world.insert_resource(UndoStack::new());
        world.insert_resource(SeqCurvePanel {
            track: 2,
            ..SeqCurvePanel::default()
        });
        let ctx = egui::Context::default();
        keys_frame(&ctx, &mut world, vec![]);
        let first = keys_frame(&ctx, &mut world, vec![]);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&first), Vec::<String>::new());
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Sub section strip, 1 sections".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        tab_to(
            &ctx,
            [900.0, 600.0],
            "fx section 0 at 0.000 s, fx",
            false,
            100,
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, &mut world));
            },
        );
        for _ in 0..3 {
            keys_frame(&ctx, &mut world, vec![key(Key::ArrowRight, Modifiers::NONE)]);
        }
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 3);
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Sub(sections) = &p.sequence().tracks[2].keys else {
            panic!("sub track");
        };
        assert!((sections[0].start - 0.1).abs() < 1e-4, "{}", sections[0].start);
    }

    #[test]
    fn a_key_dragged_past_its_neighbor_stays_selected() {
        let (mut world, player) = world_with_player();
        let mut panel = SeqCurvePanel {
            player: Some(player),
            ..SeqCurvePanel::default()
        };
        let edit = KeyEdit::Move {
            index: 0,
            time: 3.0,
            value: 2.0,
        };
        edit_selected(&mut world, &mut panel, edit).expect("move");
        assert_eq!(panel.selected, Some(1), "the next drag event moves the same key");
        assert_eq!(fade_keys(&world, player), vec![(2.0, 1.0), (3.0, 2.0)]);
    }

    const OPEN: &str = r#"(version: 1, name: "intro", tracks: [
        (name: "flag", target: "ui", property: "visible", keys: Bool([(time: 0.0, value: true)])),
        (name: "fade", target: "ui", property: "opacity",
            keys: Float([(time: 0.0, value: 0.0, ease: Bezier), (time: 2.0, value: 1.0)])),
        (name: "fx", keys: Sub([(sequence: "cine/fx.seq.ron", start: 0.0, end: 1.0)])),
    ])"#;

    fn project_world(tag: &str) -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("cine")).expect("dirs");
        std::fs::write(root.join("cine/intro.seq.ron"), OPEN).expect("write");
        std::fs::write(
            root.join("cine/fx.seq.ron"),
            "(version: 2, markers: [(name: \"m\", time: 0.5)])",
        )
        .expect("write");
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    #[test]
    fn open_edit_and_save_round_trip_a_project_file() {
        let (mut world, root) = project_world("seq_curve_open");
        let opened = seq_curve_open(&mut world, &json!({ "path": "cine/intro.seq.ron" })).expect("open");
        assert_eq!(opened["sub_sequences"], json!(["cine/fx.seq.ron"]));
        assert!(
            world
                .resource::<SequenceLibrary>()
                .get("cine/fx.seq.ron")
                .is_some(),
            "the sub-sequence joined the library"
        );
        let panel = world.resource::<SeqCurvePanel>();
        assert_eq!(panel.track, 1, "the first track with a curve");
        seq_curve_edit(&mut world, &json!({ "op": "add", "time": 1.0, "value": 0.25 })).expect("add");
        let saved = seq_curve_save(&mut world, &json!({})).expect("save to the opened path");
        assert_eq!(saved["saved"], json!("cine/intro.seq.ron"));
        assert_eq!(
            saved["version"],
            json!(regia_framework::sequencer::SEQUENCE_FORMAT_VERSION),
            "a v1 file saves at the current version"
        );
        let text = std::fs::read_to_string(root.join("cine/intro.seq.ron")).expect("read");
        let back = Sequence::from_ron(&text).expect("re-parses");
        assert_eq!(back.tracks[1].keys.len(), 3);

        let copy = seq_curve_save(&mut world, &json!({ "path": "cine/new/copy.seq.ron" })).expect("copy");
        assert_eq!(copy["saved"], json!("cine/new/copy.seq.ron"));
        assert_eq!(
            std::fs::read_to_string(root.join("cine/new/copy.seq.ron")).expect("read"),
            text
        );
        let panel = world.resource::<SeqCurvePanel>();
        let player = panel.player.expect("player");
        assert_eq!(
            panel.paths.get(&player).map(String::as_str),
            Some("cine/new/copy.seq.ron"),
            "the next save goes here"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn open_and_save_refuse_paths_and_files_they_should() {
        let (mut world, root) = project_world("seq_curve_paths");
        let open = |world: &mut World, path: &str| {
            code(seq_curve_open(world, &json!({ "path": path })).expect_err("refused"))
        };
        assert_eq!(open(&mut world, "../outside.seq.ron"), "seq_curve_bad_path");
        assert_eq!(open(&mut world, "/etc/passwd.seq.ron"), "seq_curve_bad_path");
        assert_eq!(open(&mut world, "cine/intro.ron"), "seq_curve_bad_path");
        assert_eq!(open(&mut world, "cine/none.seq.ron"), "seq_curve_io");
        std::fs::write(root.join("cine/bad.seq.ron"), "(version: 99)").expect("write");
        assert_eq!(open(&mut world, "cine/bad.seq.ron"), "seq_curve_bad_file");
        std::fs::write(
            root.join("cine/loop.seq.ron"),
            r#"(version: 2, tracks: [(name: "s", keys: Sub([(sequence: "cine/loop.seq.ron", start: 0.0, end: 1.0)]))])"#,
        )
        .expect("write");
        assert_eq!(
            open(&mut world, "cine/loop.seq.ron"),
            "seq_curve_bad_file",
            "a sequence that contains itself"
        );
        assert_eq!(
            code(seq_curve_save(&mut world, &json!({})).expect_err("no player")),
            "seq_curve_bad_path",
            "nothing opened, no path"
        );
        assert_eq!(
            code(seq_curve_save(&mut world, &json!({ "path": "cine/x.seq.ron" })).expect_err("no player")),
            "seq_curve_no_player"
        );
        let mut bare = World::new();
        assert_eq!(
            code(seq_curve_open(&mut bare, &json!({ "path": "a.seq.ron" })).expect_err("no project")),
            "seq_curve_no_project"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_never_writes_one_players_sequence_over_anothers_file() {
        let (mut world, root) = project_world("seq_curve_two_players");
        seq_curve_open(&mut world, &json!({ "path": "cine/intro.seq.ron" })).expect("open");
        let before = std::fs::read_to_string(root.join("cine/intro.seq.ron")).expect("read");
        let other = world.spawn(SequencePlayer::new(Sequence::default())).id();
        assert_eq!(
            code(seq_curve_save(&mut world, &json!({ "player": other.to_bits() })).expect_err("no path")),
            "seq_curve_bad_path",
            "the other player has no file yet"
        );
        // The panel's field follows the selection, so **Save** cannot aim at the old path.
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        let panel = world.resource::<SeqCurvePanel>();
        assert_eq!(panel.path_text, "", "{:?}", panel.paths);
        assert_eq!(
            std::fs::read_to_string(root.join("cine/intro.seq.ron")).expect("read"),
            before
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_is_refused() {
        let (mut world, root) = project_world("seq_curve_link");
        let outside = regia_test_utils::unique_temp_dir("seq_curve_outside");
        std::fs::write(outside.join("a.seq.ron"), OPEN).expect("write");
        std::os::unix::fs::symlink(&outside, root.join("out")).expect("link");
        assert_eq!(
            code(seq_curve_open(&mut world, &json!({ "path": "out/a.seq.ron" })).expect_err("refused")),
            "seq_curve_bad_path"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// Drives the real widget: frames of synthetic pointer events through egui.
    struct Pointer {
        ctx: egui::Context,
        time: f64,
    }

    impl Pointer {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                time: 0.0,
            }
        }

        fn frame(&mut self, world: &mut World, events: Vec<egui::Event>) {
            self.time += 0.1;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                time: Some(self.time),
                ..Default::default()
            };
            let mut output = self.ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, world));
            });
            output.textures_delta.clear();
        }

        fn drag(&mut self, world: &mut World, from: egui::Pos2, path: &[egui::Pos2]) {
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            };
            self.frame(world, vec![egui::Event::PointerMoved(from)]);
            self.frame(world, vec![button(from, true)]);
            for &to in path {
                self.frame(world, vec![egui::Event::PointerMoved(to)]);
            }
            let end = path.last().copied().unwrap_or(from);
            self.frame(world, vec![button(end, false)]);
            self.frame(world, vec![]);
        }
    }

    fn at(world: &World, time: f32, value: f32) -> egui::Pos2 {
        let view = &world.resource::<SeqCurvePanel>().view;
        view.to_screen(egui::vec2(time, value))
    }

    #[test]
    fn a_pointer_drag_carries_a_key_past_its_neighbor_and_back_as_one_undo_entry() {
        let (mut world, player) = world_with_player();
        let start = fade_keys(&world, player);
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        let from = at(&world, 0.0, 0.0);
        let path = [at(&world, 1.0, 0.5), at(&world, 2.1, 0.5), at(&world, 1.5, 0.25)];
        pointer.drag(&mut world, from, &path);
        let keys = fade_keys(&world, player);
        assert_eq!(keys.len(), 2);
        assert!((keys[0].0 - 1.5).abs() < 0.02, "the dragged key: {keys:?}");
        assert!((keys[0].1 - 0.25).abs() < 0.02, "{keys:?}");
        assert_eq!(keys[1], (2.0, 1.0), "its neighbor did not move");
        let panel = world.resource::<SeqCurvePanel>();
        assert_eq!(panel.selected, Some(0));
        assert_eq!(panel.gesture, None, "the drag ended");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one entry");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(fade_keys(&world, player), start);
    }

    #[test]
    fn a_pointer_on_a_handle_sets_the_slope_and_on_empty_space_scrubs() {
        let (mut world, player) = world_with_player();
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        // The outgoing handle of key 0 (a flat auto tangent) sits right of the key.
        let handle = {
            let panel = world.resource::<SeqCurvePanel>();
            let key = panel.view.to_screen(egui::vec2(0.0, 0.0));
            handle_position(&panel.view, key, 0.0, 1.0)
        };
        pointer.drag(&mut world, handle, &[handle + egui::vec2(0.0, -60.0)]);
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Float(keys) = &p.sequence().tracks[0].keys else {
            panic!("float")
        };
        let Tangent::Aligned(slope) = keys[0].tangent else {
            panic!("aligned: {:?}", keys[0].tangent)
        };
        assert!(slope > 0.0, "dragged up: {slope}");
        assert_eq!(world.resource::<SeqCurvePanel>().selected, Some(0));

        let (empty, scrubbed) = (at(&world, 1.0, 1.0), at(&world, 1.5, 1.0));
        pointer.drag(&mut world, empty, &[scrubbed]);
        let time = world.get::<SequencePlayer>(player).expect("player").time();
        assert!((time - 1.5).abs() < 0.02, "scrubbed: {time}");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "a scrub is not an edit"
        );
    }

    #[test]
    fn a_right_click_selects_the_key_its_menu_acts_on() {
        let (mut world, _) = world_with_player();
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        let key = at(&world, 2.0, 1.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: key,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        pointer.frame(&mut world, vec![egui::Event::PointerMoved(key)]);
        pointer.frame(&mut world, vec![button(true)]);
        pointer.frame(&mut world, vec![button(false)]);
        assert_eq!(world.resource::<SeqCurvePanel>().selected, Some(1));
    }

    #[test]
    fn the_panel_draws_every_kind_of_track_and_no_player_at_all() {
        let mut pointer = Pointer::new();
        let mut empty = World::new();
        pointer.frame(&mut empty, vec![]);
        let (mut world, _) = world_with_player();
        for (track, component) in [(1, 2), (2, 0), (9, 0)] {
            world.insert_resource(SeqCurvePanel {
                track,
                component,
                ..SeqCurvePanel::default()
            });
            pointer.frame(&mut world, vec![]);
        }
    }

    #[test]
    fn handle_drags_align_or_break_the_tangent() {
        let (world, player) = world_with_player();
        let channel = world.get::<SequencePlayer>(player).expect("p").sequence().tracks[0]
            .keys
            .clone();
        assert!(
            shows_handle(&channel, 0, true),
            "the Bezier segment starts at key 0"
        );
        assert!(!shows_handle(&channel, 0, false), "no segment before key 0");
        assert!(
            shows_handle(&channel, 1, false),
            "the Bezier segment ends at key 1"
        );
        assert!(!shows_handle(&channel, 1, true));
        assert_eq!(
            handle_edit(&channel, 0, 0, true, 2.0, false),
            Some(KeyEdit::Tangent {
                index: 0,
                tangent: TangentEdit::Aligned(2.0)
            })
        );
        assert_eq!(
            handle_edit(&channel, 0, 0, true, 2.0, true),
            Some(KeyEdit::Tangent {
                index: 0,
                tangent: TangentEdit::Broken(0.0, 2.0)
            })
        );
        // Picking: a key under the pointer.
        let mut view = View2d {
            rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0)),
            ..View2d::default()
        };
        view.zoom_to_fit([0.0, 0.0, 2.0, 1.0], 0.0);
        let key = view.to_screen(egui::vec2(2.0, 1.0));
        assert_eq!(pick(&channel, 0, &view, key), Some(CurvePick::Key(1)));
        let start = view.to_screen(egui::vec2(0.0, 0.0));
        let handle = handle_position(&view, start, 0.0, 1.0);
        assert_eq!(pick(&channel, 0, &view, handle), Some(CurvePick::Out(0)));
    }

    // -- Take recorder section (ADR-2637) --

    #[test]
    fn record_and_stop_write_a_take_and_reload_the_edited_source() {
        use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
        use regia_framework::sequencer::SequencerPlugin;
        use regia_scene::Transform;

        let root = regia_test_utils::unique_temp_dir("seq_take_panel");
        std::fs::create_dir_all(root.join("cine")).expect("dirs");
        std::fs::write(root.join("cine/intro.seq.ron"), OPEN).expect("write");
        std::fs::write(
            root.join("cine/fx.seq.ron"),
            "(version: 2, markers: [(name: \"m\", time: 0.5)])",
        )
        .expect("write");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_nanos(16_666_667),
            ))
            .insert_resource(EditorProject {
                root: Some(root.clone()),
            });
        let mut panel = SeqCurvePanel::default();
        let player = open_sequence(app.world_mut(), &mut panel, "cine/intro.seq.ron").expect("open");
        let hero = app.world_mut().spawn(Transform::default()).id();
        app.world_mut()
            .entity_mut(player)
            .insert(SequenceBindings::default().with("hero", Binding::Entity(hero)));
        let mut take = SeqTakePanel {
            countdown: 3,
            ..SeqTakePanel::default()
        };
        take.targets.insert("ghost".into());
        assert_eq!(
            start_take(app.world_mut(), None, &panel, &mut take)
                .expect_err("refused")
                .code(),
            "take_bad_binding",
            "a target the player does not bind is refused"
        );
        take.targets.clear();
        take.targets.insert("hero".into());
        take.properties = "nonsense:".into();
        assert!(
            start_take(app.world_mut(), None, &panel, &mut take).is_ok(),
            "a property path is free text"
        );
        app.world_mut()
            .resource_mut::<TakeRecorder>()
            .cancel()
            .expect("cancel");
        take.properties.clear();
        start_take(app.world_mut(), None, &panel, &mut take).expect("record");
        assert_eq!(
            app.world().resource::<TakeRecorder>().status(),
            TakeStatus::Countdown
        );
        for k in 0..30 {
            app.world_mut()
                .get_mut::<Transform>(hero)
                .expect("hero")
                .translation
                .y = k as f32 * 0.05;
            app.update();
        }
        let path = stop_take(app.world_mut(), None, &mut panel, &mut take).expect("stop");
        assert_eq!(path, "cine/intro_take1.seq.ron");
        assert!(root.join(&path).exists());
        let sequence = app
            .world()
            .get::<SequencePlayer>(player)
            .expect("player")
            .sequence()
            .clone();
        assert!(
            sequence.tracks.iter().any(|t| t.name == "take1"),
            "the edited player reloaded its source with the take section"
        );
        assert!(app.world().resource::<SequenceLibrary>().get(&path).is_some());
        assert_eq!(
            stop_take(app.world_mut(), None, &mut panel, &mut take)
                .expect_err("refused")
                .code(),
            "take_not_recording"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn during_pie_the_take_records_the_play_world_by_guid() {
        use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
        use regia_core::id::Guid;
        use regia_framework::sequencer::SequencerPlugin;
        use regia_scene::Transform;
        use regia_scene::components::EntityGuid;

        let root = regia_test_utils::unique_temp_dir("seq_take_pie");
        std::fs::create_dir_all(root.join("cine")).expect("dirs");
        let mut edit = World::new();
        edit.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        let guid = Guid::from_u128(0x5eed);
        let edit_hero = edit.spawn((Transform::default(), EntityGuid(guid))).id();
        let player = edit
            .spawn((
                SequencePlayer::new(Sequence::default()),
                SequenceBindings::default()
                    .with("hero", Binding::Guid(guid))
                    .with("prop", Binding::Entity(edit_hero)),
            ))
            .id();
        let mut panel = SeqCurvePanel {
            player: Some(player),
            path_text: "cine/pie.seq.ron".into(),
            ..SeqCurvePanel::default()
        };
        let mut play = App::new();
        play.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_nanos(16_666_667),
            ));
        let play_hero = play
            .world_mut()
            .spawn((Transform::default(), EntityGuid(guid)))
            .id();

        let mut take = SeqTakePanel {
            countdown: 0,
            ..SeqTakePanel::default()
        };
        take.targets.insert("prop".into());
        assert_eq!(
            start_take(&mut edit, Some(play.world_mut()), &panel, &mut take)
                .expect_err("refused")
                .code(),
            "take_bad_binding",
            "an entity-id binding names another entity in the play world"
        );
        take.targets.clear();
        take.targets.insert("hero".into());
        start_take(&mut edit, Some(play.world_mut()), &panel, &mut take).expect("record in PIE");
        assert!(
            !edit.contains_resource::<TakeRecorder>(),
            "the edit world does not record"
        );
        for k in 0..20 {
            play.world_mut()
                .get_mut::<Transform>(play_hero)
                .expect("hero")
                .translation
                .z = k as f32;
            play.update();
        }
        let path = stop_take(&mut edit, Some(play.world_mut()), &mut panel, &mut take).expect("stop");
        assert_eq!(path, "cine/pie_take1.seq.ron");
        let text = std::fs::read_to_string(root.join(&path)).expect("take");
        let recorded = Sequence::from_ron(&text).expect("loads");
        let Some(Channel::Vec3(keys)) = recorded
            .tracks
            .iter()
            .find(|t| t.name == "hero/transform.position")
            .map(|t| &t.keys)
        else {
            panic!("a position track");
        };
        assert!(
            keys.last().expect("keys").value[2] > 10.0,
            "the play world's motion was recorded"
        );
        assert!(play.world().resource::<SequenceLibrary>().get(&path).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_take_section_draws_idle_recording_and_in_pie() {
        use regia_scene::Transform;

        fn frame(ctx: &egui::Context, world: &mut World, play: Option<&mut World>) {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let mut play = play;
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default()
                    .show(ui, |ui| seq_curve_panel_with_play(ui, world, play.as_deref_mut()));
            });
            output.textures_delta.clear();
        }
        let ctx = egui::Context::default();
        let mut world = World::new();
        frame(&ctx, &mut world, None);
        assert!(world.contains_resource::<SeqTakePanel>(), "drawn with no player");
        let hero = world.spawn(Transform::default()).id();
        world.spawn((
            SequencePlayer::new(Sequence::default()),
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        frame(&ctx, &mut world, None);
        world
            .get_resource_or_insert_with(TakeRecorder::default)
            .start(TakeConfig {
                bindings: vec![TakeBinding {
                    name: "camera".into(),
                    target: TakeTarget::ActiveCamera,
                    channels: vec![TakeChannel::Camera],
                }],
                countdown: 10,
                ..TakeConfig::default()
            })
            .expect("start");
        frame(&ctx, &mut world, None);
        let mut play = World::new();
        frame(&ctx, &mut world, Some(&mut play));
        assert_eq!(
            world.resource::<TakeRecorder>().status(),
            TakeStatus::Countdown,
            "drawing never starts or stops a take"
        );
    }

    /// Lane w7-editor-smalls-z (5): with no player, `editor.seq_curve_state` still answers the
    /// documented `path` (`null`) and `dirty` (`false`) beside `player: null` and the error.
    #[test]
    fn state_without_a_player_answers_path_and_dirty() {
        let mut world = World::new();
        let table = methods();
        let (_, handler) = table
            .iter()
            .find(|(s, _)| s.name == "editor.seq_curve_state")
            .expect("state");
        let out = handler(&mut world, &json!({})).expect("state");
        assert_eq!(out["players"], json!([]), "{out}");
        assert!(out["player"].is_null(), "{out}");
        assert!(out.get("path").is_some_and(Value::is_null), "path: null: {out}");
        assert_eq!(out.get("dirty"), Some(&json!(false)), "dirty: false: {out}");
        assert!(out.get("error").is_some(), "{out}");
    }

    /// P-208 (spec addendum § 11.3): the multi-curve view lists every curve of the sequence (a
    /// vec3 track gives three), hides and locks them from the keyboard, and a locked curve refuses
    /// an edit from its key list.
    #[test]
    fn the_curve_list_hides_and_locks_curves_from_the_keyboard() {
        use crate::canvas_keys::test_input::{key, tab_to};
        use egui::{Key, Modifiers};
        let (mut world, player) = world_with_player();
        world.insert_resource(UndoStack::new());
        let sequence = world
            .get::<SequencePlayer>(player)
            .map(|p| p.sequence().clone())
            .expect("player");
        let labels: Vec<String> = sequence_curves(&sequence).into_iter().map(|c| c.2).collect();
        assert_eq!(labels, ["fade", "move.x", "move.y", "move.z"]);
        let ctx = egui::Context::default();
        keys_frame(&ctx, &mut world, vec![]);
        keys_frame(&ctx, &mut world, vec![]);
        for name in ["Show move.y", "Lock fade"] {
            tab_to(&ctx, [900.0, 600.0], name, false, 100, |ui| {
                egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, &mut world));
            });
            keys_frame(&ctx, &mut world, vec![key(Key::Space, Modifiers::NONE)]);
            keys_frame(&ctx, &mut world, vec![]);
        }
        {
            let panel = world.resource::<SeqCurvePanel>();
            assert!(!panel.curves.visible(&seq_curve_key(1, 1)), "move.y hidden");
            assert!(panel.curves.visible(&seq_curve_key(1, 0)));
            assert!(panel.curves.is_locked(&seq_curve_key(0, 0)), "fade locked");
        }
        let before = fade_keys(&world, player);
        tab_to(&ctx, [900.0, 600.0], "fade key 1 at 2.000 s", false, 100, |ui| {
            egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, &mut world));
        });
        keys_frame(&ctx, &mut world, vec![key(Key::ArrowRight, Modifiers::NONE)]);
        keys_frame(&ctx, &mut world, vec![]);
        assert_eq!(fade_keys(&world, player), before, "a locked curve is not edited");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        assert!(world.resource::<SeqCurvePanel>().error.is_some());
    }

    /// Lane v06-editor-undo-agent-routes (smoke bug 11): with no sequence player the panel says so
    /// once, in one user-facing sentence, without an agent method's syntax.
    #[test]
    fn no_player_says_so_once_without_protocol_text() {
        let mut world = World::new();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, &mut world));
            },
        );
        output.textures_delta.clear();
        fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }
        let no_player: Vec<&String> = texts
            .iter()
            .filter(|t| t.contains("No sequence player"))
            .collect();
        assert_eq!(no_player.len(), 1, "one no-player line: {texts:?}");
        assert!(
            texts
                .iter()
                .all(|t| !t.contains("sequence.player") && !t.contains("action:") && !t.contains('{')),
            "no protocol syntax in the panel text: {texts:?}"
        );
    }
}
