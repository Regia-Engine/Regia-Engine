// SPDX-License-Identifier: MPL-2.0
//! Sequence authoring in the Sequence Curves panel (ADR-9710, book `agents/seq-curve-panel.md`):
//! the structure the curve and section editors work inside.
//!
//! - **Tracks and markers**: add a track of any `.seq.ron` v5 kind (transform, float, vec3 and
//!   bool property, animation clip, flat and spatial audio, sub-sequence, video), remove, rename
//!   and reorder it; add, remove, rename and reorder markers.
//! - **Bindings**: bind a name to a scene entity (by its guid when it has one, so the binding
//!   survives a save) or to a spawnable prefab; remove a binding (refused while something uses it,
//!   unless `cascade`).
//! - **New sequence**: write an empty `.seq.ron` v5 (name, length, frame rate) under the project
//!   and open it.
//! - **Key from the scene**: the **Key** button writes the bound selected entity's transform (or
//!   a property) at the playhead, creating the track when missing; **Auto-key** keys a bound
//!   entity when a gizmo drag or an inspector edit on it ends, as one undo entry with the edit;
//!   the Inspector's **Add to sequence** item keys a reflected field.
//! - **Preview**: applies the sequence at the playhead to the edit world and puts back every
//!   value it touched when the preview stops or the panel closes; a level save never sees the
//!   previewed values. Clip sections pose their rigs (an `Animator` rig too, its animator
//!   suspended), sound sections play on the editor's audio output and a video section shows its
//!   picture on its bound video player (`media`, ADR-8940).
//!
//! - **Track list** (`.seq.ron` v6, ADR-8105): every track is a focusable row showing whether it is
//!   locked, muted or soloed. Right-click a row, or press Shift+F10 while it has the keyboard focus,
//!   for its menu: Rename, Duplicate, Delete, Lock, Mute, Solo, Move up, Move down and Select all
//!   keys. A locked track refuses every edit of its keys, sections, name, binding or position
//!   (`seq_track_locked`); mute is the track's saved `enabled` switch; solo is editor session state
//!   (never saved): while any track is soloed, the preview evaluates only the soloed ones.
//! - **Event keys** (`.seq.ron` v6): an event track's keys (time, event name, payload) are listed
//!   under the track list when it is the selected track; add one at the playhead, edit or remove.
//!
//! Every edit goes through `regia_framework::sequencer::edit::tracks` on a copy of the sequence,
//! so a refused edit changes nothing, and is one undo entry (the sequence and the player's
//! binding table before and after). The agent methods `editor.seq_track_edit`,
//! `editor.seq_binding_edit`, `editor.seq_new`, `editor.seq_key` and `editor.seq_preview` run the
//! same code.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectComponent};
use regia_framework::sequencer::edit::EditError;
use regia_framework::sequencer::edit::tracks::{
    self, BindingUses, EventKeyEdit, MarkerEdit, TrackEdit, TrackKind, TrackSpec, apply_event_key_edit,
    apply_marker_edit, apply_track_edit,
};
use regia_framework::sequencer::recorder::{REFLECT_PREFIX, apply_reflected, read_reflected};
use regia_framework::sequencer::{
    Binding, Channel, ChannelValue, EventKey, Marker, PropertyAppliers, PropertyReaders, Sequence,
    SequenceBindings, SequencePlayer, Spawnable,
};
use regia_scene::prelude::{EntityGuid, Transform};
use serde_json::{Value, json};

use crate::command::{CommandGroup, EditorCommand, SetProperty, SetTransform, TransformGesture, UndoStack};
use crate::seq_curve_panel::{SeqCurvePanel, number, player_of, rpc};
use crate::viewport_agent::Handler;

/// The authoring state: the auto-key toggle, the running preview and the row fields.
#[derive(Resource, Debug)]
pub struct SeqAuthoring {
    /// Key a bound entity when a gizmo drag or an inspector edit on it ends.
    pub auto_key: bool,
    /// The running edit-world preview.
    pub preview: Option<SeqPreview>,
    /// The last refused authoring edit, shown under the rows.
    pub error: Option<String>,
    /// The highest undo state the auto key has looked at (new edits get larger ids).
    seen_state: u64,
    /// The **Add track** row: the kind index into [`TrackKind::ALL`].
    pub new_kind: usize,
    /// The **Add track** row: name, binding, property and asset fields.
    pub new_fields: [String; 4],
    /// The **New sequence** row: path, length (seconds) and fps.
    pub new_path: String,
    /// The **New sequence** row: length in seconds.
    pub new_length: f32,
    /// The **New sequence** row: frames per second.
    pub new_fps: f32,
    /// The **Bind** row: the binding name.
    pub bind_name: String,
    /// The **Bind** row: remove with `cascade`.
    pub cascade: bool,
    /// The preview's voices and taken-over video players, and the session's voice counters.
    media: media::PreviewMedia,
    /// The soloed tracks of each player, by track name (`.seq.ron` v6 track solo, ADR-8105):
    /// editor session state, never saved. While a player has any, its preview evaluates only them.
    pub solo: BTreeMap<Entity, BTreeSet<String>>,
}

impl Default for SeqAuthoring {
    fn default() -> Self {
        Self {
            auto_key: false,
            preview: None,
            error: None,
            seen_state: 0,
            new_kind: 0,
            new_fields: Default::default(),
            new_path: String::new(),
            new_length: 5.0,
            new_fps: 30.0,
            bind_name: String::new(),
            cascade: false,
            media: media::PreviewMedia::default(),
            solo: BTreeMap::new(),
        }
    }
}

impl SeqAuthoring {
    /// Preview voices started and stopped so far this session (sound sections, ADR-8940).
    #[must_use]
    pub fn voice_counts(&self) -> (u64, u64) {
        (self.media.started, self.media.stopped)
    }
}

/// What one previewed entity looked like before the preview first wrote it.
struct Saved {
    entity: Entity,
    transform: Option<Transform>,
    /// Every other reflected component, by type, as it was.
    components: Vec<(TypeId, Box<dyn PartialReflect>)>,
    /// The component types the preview's writes changed (`Transform` included): only these are
    /// put back, so an unrelated edit made during the preview (a rename) stays.
    touched: Vec<TypeId>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("entity", &self.entity)
            .field("components", &self.components.len())
            .finish()
    }
}

/// A running edit-world preview of one player's sequence.
#[derive(Debug)]
pub struct SeqPreview {
    /// The previewed player.
    pub player: Entity,
    /// The sequence time last applied, seconds.
    pub time: f64,
    saved: Vec<Saved>,
    /// The sequence last applied (with `time`): an unchanged frame is not written again, so a
    /// gizmo drag on a previewed entity is not overwritten every frame.
    applied: Option<Sequence>,
    /// The egui frame the panel last drew in during this preview (`None`: not drawn yet, an
    /// agent-only preview).
    drawn_frame: Option<u64>,
    /// The joint entities a clip section has posed (their pre-preview pose is in `saved`): each
    /// apply puts them back first, so a clip blends over the rig's own pose (ADR-8730).
    posed_joints: Vec<Entity>,
    /// Rigs with an `Animator` a clip section has posed (ADR-8940): the animator is suspended
    /// (held out of the rig while the clips blend onto the joints, never written).
    suspended: Vec<Entity>,
}

impl SeqPreview {
    /// The entities the preview has written to.
    #[must_use]
    pub fn touched(&self) -> Vec<Entity> {
        self.saved.iter().map(|s| s.entity).collect()
    }

    /// The `Animator` rigs whose animator the preview has suspended to pose them (ADR-8940).
    #[must_use]
    pub fn suspended_animators(&self) -> &[Entity] {
        &self.suspended
    }
}

mod media;
// The sequence properties' Restore on stop row (`editor.seq_restore_on_stop`, ADR-7726).
pub mod restore;

pub use media::{
    PreviewFrame, VIDEO_BUDGET, hold_sounds, mark_play_step, mark_reverse_step, paint_preview_overlay,
    preview_overlay_frame,
};

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// One undo entry of an authoring edit: the sequence and the player's binding table before and
/// after.
pub struct SeqAuthorEdit {
    player: Entity,
    label: &'static str,
    before: Sequence,
    after: Sequence,
    table: Option<(Option<SequenceBindings>, Option<SequenceBindings>)>,
}

impl SeqAuthorEdit {
    fn swap_in(&self, world: &mut World, sequence: &Sequence, table: Option<&Option<SequenceBindings>>) {
        let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) else {
            return;
        };
        player.replace_sequence(sequence.clone());
        match table {
            Some(Some(table)) => {
                world.entity_mut(self.player).insert(table.clone());
            }
            Some(None) => {
                world.entity_mut(self.player).remove::<SequenceBindings>();
            }
            None => {}
        }
        refresh_preview(world);
    }
}

impl EditorCommand for SeqAuthorEdit {
    fn apply(&mut self, world: &mut World) {
        let table = self.table.as_ref().map(|(_, after)| after);
        self.swap_in(world, &self.after, table);
    }

    fn revert(&mut self, world: &mut World) {
        let table = self.table.as_ref().map(|(before, _)| before);
        self.swap_in(world, &self.before, table);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.to_string()
    }
}

/// Runs `edit` on a copy of the panel's player's sequence and binding table, validates the copy
/// and swaps it in. Returns the player, the edit's result and the undo entry (already applied;
/// the caller pushes or joins it).
fn author<T>(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    label: &'static str,
    edit: impl FnOnce(&mut Sequence, &mut Option<SequenceBindings>) -> Result<T, EditError>,
) -> Result<(T, SeqAuthorEdit), EditError> {
    let Some(entity) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let before = player.sequence().clone();
    let table_before = world.get::<SequenceBindings>(entity).cloned();
    let mut sequence = before.clone();
    let mut table = table_before.clone();
    let out = edit(&mut sequence, &mut table)?;
    if let Err(e) = sequence.validate() {
        return refuse("seq_track_invalid", e.to_string());
    }
    // A locked track keeps its keys, sections, name and binding (`.seq.ron` v6, ADR-8105).
    tracks::check_locked(&before, &sequence)?;
    let command = SeqAuthorEdit {
        player: entity,
        label,
        before,
        after: sequence,
        table: (table != table_before).then_some((table_before, table)),
    };
    let table = command.table.as_ref().map(|(_, after)| after);
    command.swap_in(world, &command.after, table);
    panel.player = Some(entity);
    Ok((out, command))
}

/// Pushes an authoring edit as its own undo entry.
fn push(world: &mut World, command: SeqAuthorEdit) {
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    undo.seal();
    undo.push(Box::new(command));
    undo.seal();
}

/// The panel's track index after a track list edit: keeps it on the same track where it can.
fn fix_track_selection(world: &mut World, panel: &mut SeqCurvePanel, selected: Option<usize>) {
    let count = player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map_or(0, |p| p.sequence().tracks.len());
    if let Some(i) = selected {
        panel.track = i;
    }
    if panel.track >= count {
        panel.track = count.saturating_sub(1);
    }
    panel.selected = None;
    panel.component = 0;
}

/// Adds the track `spec` describes at `at` (`None`: the end), as one undo entry. Returns its index.
pub fn add_track(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    spec: &TrackSpec,
    at: Option<usize>,
) -> Result<usize, EditError> {
    let track = tracks::starter_track(spec)?;
    add_built_track(world, panel, track, at)
}

/// Adds a built track at `at` as one undo entry. Returns its index.
fn add_built_track(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    track: regia_framework::sequencer::Track,
    at: Option<usize>,
) -> Result<usize, EditError> {
    let (index, command) = author(world, panel, "Add sequence track", |seq, _| {
        apply_track_edit(seq, TrackEdit::Add { at, track })
    })?;
    push(world, command);
    fix_track_selection(world, panel, Some(index));
    Ok(index)
}

/// Applies a remove, rename or move of a track as one undo entry. Returns the index the edit
/// reports (see `apply_track_edit`).
pub fn edit_track_list(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    edit: TrackEdit,
) -> Result<usize, EditError> {
    let label = match edit {
        TrackEdit::Add { .. } => "Add sequence track",
        TrackEdit::Remove { .. } => "Remove sequence track",
        TrackEdit::Rename { .. } => "Rename sequence track",
        TrackEdit::Move { .. } => "Move sequence track",
        TrackEdit::Duplicate { .. } => "Duplicate sequence track",
        TrackEdit::Lock { locked: true, .. } => "Lock sequence track",
        TrackEdit::Lock { locked: false, .. } => "Unlock sequence track",
        TrackEdit::Mute { muted: true, .. } => "Mute sequence track",
        TrackEdit::Mute { muted: false, .. } => "Unmute sequence track",
    };
    let removal = matches!(edit, TrackEdit::Remove { .. });
    let renamed = match &edit {
        TrackEdit::Rename { index, name } => Some((*index, name.trim().to_string())),
        _ => None,
    };
    let old_name = renamed
        .as_ref()
        .and_then(|(index, _)| track_name(world, panel, *index));
    let (index, command) = author(world, panel, label, |seq, _| apply_track_edit(seq, edit))?;
    push(world, command);
    // A soloed track stays soloed under its new name.
    if let (Some(old), Some((_, new)), Some(player)) = (old_name, renamed, player_of(world, panel))
        && let Some(mut state) = world.get_resource_mut::<SeqAuthoring>()
        && let Some(set) = state.solo.get_mut(&player)
        && set.remove(&old)
    {
        set.insert(new);
    }
    fix_track_selection(world, panel, (!removal).then_some(index));
    Ok(index)
}

/// The name of track `index` of the panel's player.
fn track_name(world: &mut World, panel: &SeqCurvePanel, index: usize) -> Option<String> {
    let player = player_of(world, panel)?;
    world
        .get::<SequencePlayer>(player)?
        .sequence()
        .tracks
        .get(index)
        .map(|t| t.name.clone())
}

/// The soloed track names of `player` (empty: none soloed, every enabled track plays).
#[must_use]
pub fn soloed(world: &World, player: Entity) -> BTreeSet<String> {
    world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.solo.get(&player).cloned())
        .unwrap_or_default()
}

/// Solos (`on`) or unsolos track `index` of the panel's player: editor session state, not an undo
/// entry and never saved (ADR-8105). While any track of the player is soloed, the preview
/// evaluates only the soloed tracks, so the others are muted there. Returns the player's soloed
/// track names. Errors: `seq_curve_no_player`, `seq_track_bad_index`.
pub fn set_solo(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    index: usize,
    on: bool,
) -> Result<BTreeSet<String>, EditError> {
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(name) = track_name(world, panel, index) else {
        return refuse("seq_track_bad_index", format!("no track {index}"));
    };
    let mut state = world.get_resource_or_insert_with::<SeqAuthoring>(SeqAuthoring::default);
    let set = state.solo.entry(player).or_default();
    if on {
        set.insert(name);
    } else {
        set.remove(&name);
    }
    let out = set.clone();
    if out.is_empty() {
        state.solo.remove(&player);
    }
    panel.player = Some(player);
    // The preview re-evaluates with the new solo set at once.
    if world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|a| a.preview.is_some())
    {
        apply_preview(world, true);
    }
    Ok(out)
}

/// `sequence` as the preview evaluates it under `solo`: with any track soloed, every other track is
/// disabled (muted) on this copy.
#[must_use]
pub fn with_solo(sequence: &Sequence, solo: &BTreeSet<String>) -> Sequence {
    let mut out = sequence.clone();
    if !solo.is_empty() {
        for track in &mut out.tracks {
            if !solo.contains(&track.name) {
                track.enabled = false;
            }
        }
    }
    out
}

/// Applies an edit of the event track `track`'s keys as one undo entry (`.seq.ron` v6). Returns
/// the key's index. Errors: see `apply_event_key_edit` (`seq_track_locked` on a locked track).
pub fn edit_event_keys(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    track: usize,
    edit: EventKeyEdit,
) -> Result<usize, EditError> {
    let (index, command) = author(world, panel, "Edit event keys", |seq, _| {
        apply_event_key_edit(seq, track, edit)
    })?;
    push(world, command);
    panel.track = track;
    Ok(index)
}

/// Selects every key of track `index` in the timeline (the keys of a keyed or event track, the
/// starts and ends of a section track's sections) and makes it the panel's track. Returns how many
/// marks are selected (event keys are timeline keys since ADR-8628: they copy, paste and ripple).
/// Errors: `seq_curve_no_player`, `seq_track_bad_index`.
pub fn select_track_keys(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    index: usize,
) -> Result<usize, EditError> {
    use regia_framework::sequencer::edit::timeline::{KeyRef, MarkRef};
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(track) = world
        .get::<SequencePlayer>(player)
        .and_then(|p| p.sequence().tracks.get(index).cloned())
    else {
        return refuse("seq_track_bad_index", format!("no track {index}"));
    };
    let marks: Vec<MarkRef> = if track.keys.is_keyed() || matches!(track.keys, Channel::Event(_)) {
        (0..track.keys.len())
            .map(|i| {
                MarkRef::Key(KeyRef {
                    track: index,
                    index: i,
                })
            })
            .collect()
    } else {
        (0..track.keys.len())
            .flat_map(|i| {
                [false, true].map(|end| MarkRef::SectionEnd {
                    track: index,
                    index: i,
                    end,
                })
            })
            .collect()
    };
    let count = marks.len();
    world
        .get_resource_or_insert_with::<crate::seq_timeline::SeqTimeline>(
            crate::seq_timeline::SeqTimeline::default,
        )
        .selection = marks;
    panel.player = Some(player);
    panel.track = index;
    panel.selected = None;
    Ok(count)
}

/// Applies a marker edit as one undo entry. Returns the marker's index.
pub fn edit_markers(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    edit: MarkerEdit,
) -> Result<usize, EditError> {
    let (index, command) = author(world, panel, "Edit sequence markers", |seq, _| {
        apply_marker_edit(seq, edit)
    })?;
    push(world, command);
    Ok(index)
}

/// What a binding is bound to when it is added.
#[derive(Clone, Debug, PartialEq)]
pub enum BindingSource {
    /// A scene entity: bound by its [`EntityGuid`] when it has one (survives a save), else by the
    /// entity itself.
    Entity(Entity),
    /// The entity carrying this scene guid.
    Guid(Guid),
    /// A prefab the player spawns (`.seq.ron` v5 spawnable).
    Spawnable(Spawnable),
}

/// Adds the binding `name`, as one undo entry. Errors: `seq_binding_empty`, `seq_binding_in_use`
/// (the player's table or the sequence's spawnables already have it), `seq_binding_no_entity`,
/// `seq_spawnable_invalid`.
pub fn add_binding(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    name: &str,
    source: BindingSource,
) -> Result<(), EditError> {
    if name.is_empty() {
        return refuse("seq_binding_empty", "binding names must not be empty");
    }
    let binding = match &source {
        BindingSource::Entity(entity) => {
            if world.get_entity(*entity).is_err() {
                return refuse(
                    "seq_binding_no_entity",
                    format!("entity {} does not exist", entity.to_bits()),
                );
            }
            Some(match world.get::<EntityGuid>(*entity) {
                Some(guid) => Binding::Guid(guid.0),
                None => Binding::Entity(*entity),
            })
        }
        BindingSource::Guid(guid) => Some(Binding::Guid(*guid)),
        BindingSource::Spawnable(_) => None,
    };
    let (_, command) = author(world, panel, "Add sequence binding", |seq, table| {
        let taken = table.as_ref().is_some_and(|t| t.get(name).is_some())
            || seq.spawnables.iter().any(|s| s.binding == name);
        if taken {
            return refuse(
                "seq_binding_in_use",
                format!("`{name}` is already bound; remove it first"),
            );
        }
        match (source, binding) {
            (BindingSource::Spawnable(mut spawn), _) => {
                spawn.binding = name.to_string();
                tracks::add_spawnable(seq, spawn)
            }
            (_, Some(binding)) => {
                table
                    .get_or_insert_with(SequenceBindings::default)
                    .bind(name, binding);
                Ok(())
            }
            (_, None) => refuse("seq_binding_no_entity", "nothing to bind"),
        }
    })?;
    push(world, command);
    Ok(())
}

/// Removes the binding `name`: its table entry, its spawnable, and with `cascade` everything that
/// uses it (`tracks::remove_binding`), as one undo entry. Errors: `seq_binding_used` (used and no
/// `cascade`; nothing changed), `seq_binding_unknown` (neither bound nor used).
pub fn remove_binding(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    name: &str,
    cascade: bool,
) -> Result<BindingUses, EditError> {
    let (uses, command) = author(world, panel, "Remove sequence binding", |seq, table| {
        let in_table = table.as_ref().is_some_and(|t| t.get(name).is_some());
        let uses = match tracks::remove_binding(seq, name, cascade) {
            Ok(uses) => uses,
            // Bound by hand only: nothing in the sequence uses it.
            Err(e) if e.code == "seq_binding_unknown" && in_table => BindingUses::default(),
            Err(e) => return Err(e),
        };
        if let Some(t) = table.as_mut() {
            t.unbind(name);
        }
        Ok(uses)
    })?;
    push(world, command);
    Ok(uses)
}

/// The scene entity binding `name` resolves to through `player`'s table (a guid or an entity).
#[must_use]
pub fn resolve_binding(world: &mut World, player: Entity, name: &str) -> Option<Entity> {
    match world.get::<SequenceBindings>(player)?.get(name)? {
        Binding::Entity(entity) => world.get_entity(entity).is_ok().then_some(entity),
        Binding::Guid(guid) => {
            let mut query = world.query::<(Entity, &EntityGuid)>();
            query.iter(world).find(|(_, g)| g.0 == guid).map(|(e, _)| e)
        }
    }
}

/// The binding names of `player`'s table that resolve to `entity`, sorted.
#[must_use]
pub fn bindings_of(world: &mut World, player: Entity, entity: Entity) -> Vec<String> {
    let names: Vec<String> = world
        .get::<SequenceBindings>(player)
        .map(|t| t.iter().map(|(n, _)| n.to_string()).collect())
        .unwrap_or_default();
    names
        .into_iter()
        .filter(|n| resolve_binding(world, player, n) == Some(entity))
        .collect()
}

/// Writes an empty `.seq.ron` v5 at project path `path` (refused when the file exists) and opens
/// it in the panel. Returns the new player. Codes: `seq_new_exists`, `seq_new_name_empty` (an
/// empty or blank name, also a file stem used as the name), `seq_new_bad_fps`,
/// `seq_new_bad_length`, and `editor.seq_curve_open`'s path codes.
pub fn new_sequence(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    path: &str,
    name: Option<&str>,
    length: f32,
    fps: f32,
) -> Result<Entity, EditError> {
    let default_name = path
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".seq.ron"))
        .unwrap_or("sequence")
        .to_string();
    let sequence = tracks::new_sequence(name.unwrap_or(&default_name), length, fps)?;
    let file = crate::seq_curve_panel::project_file(world, path)?;
    if file.exists() {
        return refuse("seq_new_exists", format!("{path} already exists"));
    }
    let text = sequence.to_ron().map_err(|e| EditError {
        code: "seq_curve_invalid",
        message: e.to_string(),
    })?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EditError {
            code: "seq_curve_io",
            message: format!("cannot create {}: {e}", parent.display()),
        })?;
    }
    crate::level_io::write_atomic(&file, text.as_bytes()).map_err(|message| EditError {
        code: "seq_curve_io",
        message,
    })?;
    crate::seq_curve_panel::open_sequence(world, panel, path)
}

/// Which values a key writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyChannel {
    /// Position, rotation and scale (three Vec3 tracks).
    Transform,
    /// `transform.position` only.
    Position,
    /// `transform.rotation` only (Euler degrees).
    Rotation,
    /// `transform.scale` only.
    Scale,
    /// One property path: a registered reader's path or `reflect:<Component>.<field>`.
    Property(String),
}

impl KeyChannel {
    /// `transform`, `position`, `rotation`, `scale`, or a property path.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text {
            "transform" => Self::Transform,
            "position" | "transform.position" => Self::Position,
            "rotation" | "transform.rotation" => Self::Rotation,
            "scale" | "transform.scale" => Self::Scale,
            other => Self::Property(other.to_string()),
        }
    }

    fn properties(&self) -> Vec<String> {
        match self {
            Self::Transform => vec![
                "transform.position".into(),
                "transform.rotation".into(),
                "transform.scale".into(),
            ],
            Self::Position => vec!["transform.position".into()],
            Self::Rotation => vec!["transform.rotation".into()],
            Self::Scale => vec!["transform.scale".into()],
            Self::Property(p) => vec![p.clone()],
        }
    }
}

/// Reads one property of `entity` the way a track would write it.
fn read_property(world: &World, entity: Entity, property: &str) -> Result<ChannelValue, EditError> {
    let read = if property.starts_with(REFLECT_PREFIX) {
        read_reflected(world, entity, property)
    } else {
        let readers = world
            .get_resource::<PropertyReaders>()
            .cloned()
            .unwrap_or_default();
        match readers.get(property) {
            Some(read) => read(world, entity),
            None => Err(format!(
                "no property reader for `{property}` (use reflect:<Component>.<field>)"
            )),
        }
    };
    read.map_err(|message| EditError {
        code: "seq_key_unreadable",
        message,
    })
}

/// The short track-name suffix of a property path (`transform.position` -> `position`).
fn property_suffix(property: &str) -> &str {
    let tail = property.strip_prefix(REFLECT_PREFIX).unwrap_or(property);
    tail.strip_prefix("transform.").unwrap_or(tail)
}

/// One key written by [`key_binding`].
#[derive(Clone, Debug, PartialEq)]
pub struct WrittenKey {
    /// The track's name.
    pub track: String,
    /// The property path.
    pub property: String,
    /// Seconds.
    pub time: f32,
    /// The value written.
    pub value: ChannelValue,
    /// `true` when the track was created for this key.
    pub created: bool,
}

/// Keys `channel` of the entity binding `binding` resolves to, at `time` (default: the playhead),
/// creating each missing track (`<binding>.<property>`). Returns the keys and the undo entry
/// (applied, not pushed). Codes: `seq_curve_no_player`, `seq_key_unbound` (the binding resolves
/// to no scene entity), `seq_key_unreadable`, and `tracks::key_property`'s codes.
fn key_command(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    binding: &str,
    channel: &KeyChannel,
    time: Option<f32>,
) -> Result<(Vec<WrittenKey>, SeqAuthorEdit), EditError> {
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(entity) = resolve_binding(world, player, binding) else {
        return refuse(
            "seq_key_unbound",
            format!("`{binding}` is not bound to a scene entity (editor.seq_binding_edit adds one)"),
        );
    };
    let time = match time {
        Some(t) => t,
        None => world
            .get::<SequencePlayer>(player)
            .map_or(0.0, |p| p.time() as f32),
    };
    let mut values = Vec::new();
    for property in channel.properties() {
        values.push((property.clone(), read_property(world, entity, &property)?));
    }
    author(world, panel, "Key sequence", |seq, _| {
        let mut written = Vec::new();
        for (property, value) in values {
            let name = format!("{binding}.{}", property_suffix(&property));
            let (track, _, created) = tracks::key_property(seq, binding, &property, &name, time, value)?;
            written.push(WrittenKey {
                track: seq.tracks[track].name.clone(),
                property,
                time,
                value,
                created,
            });
        }
        Ok(written)
    })
}

/// Keys `channel` of `binding` at `time` (default: the playhead) as one undo entry: the **Key**
/// button and `editor.seq_key`.
pub fn key_binding(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    binding: &str,
    channel: &KeyChannel,
    time: Option<f32>,
) -> Result<Vec<WrittenKey>, EditError> {
    let (written, command) = key_command(world, panel, binding, channel, time)?;
    push(world, command);
    Ok(written)
}

/// The binding of the selected entity in the panel's player (the first, sorted, when several
/// names bind it). Codes: `seq_curve_no_player`, `seq_key_no_selection`, `seq_key_unbound`.
pub fn selected_binding(world: &mut World, panel: &SeqCurvePanel) -> Result<String, EditError> {
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let Some(entity) = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
    else {
        return refuse("seq_key_no_selection", "select a bound entity first");
    };
    bindings_of(world, player, entity)
        .into_iter()
        .next()
        .ok_or_else(|| EditError {
            code: "seq_key_unbound",
            message: "the selected entity is not bound in this sequence (Bind adds it)".into(),
        })
}

/// The Inspector's **Add to sequence** on field `path` of `component` on `entity`: keys
/// `reflect:<Component>.<path>` at the playhead on the entity's binding (bound first under the
/// entity's name when it has none), creating the property track.
pub fn add_to_sequence(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<WrittenKey, EditError> {
    let short = component.rsplit("::").next().unwrap_or(component);
    let property = format!("{REFLECT_PREFIX}{short}.{path}");
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let result = (|| {
        let Some(player) = player_of(world, &panel) else {
            return refuse("seq_curve_no_player", "open or create a sequence first");
        };
        let binding = match bindings_of(world, player, entity).into_iter().next() {
            Some(name) => name,
            None => {
                let name = world
                    .get::<regia_scene::components::Name>(entity)
                    .map(|n| n.as_str().to_string())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("entity{}", entity.index()));
                add_binding(world, &mut panel, &name, BindingSource::Entity(entity))?;
                name
            }
        };
        let mut written = key_binding(world, &mut panel, &binding, &KeyChannel::Property(property), None)?;
        written.pop().ok_or_else(|| EditError {
            code: "seq_key_unreadable",
            message: "nothing was keyed".into(),
        })
    })();
    if let Err(e) = &result {
        world.get_resource_or_insert_with(SeqAuthoring::default).error = Some(e.message.clone());
    }
    world.insert_resource(panel);
    result
}

/// The scene entity and channel a finished undo entry edited, when it is a transform or a
/// reflected-field edit.
fn edited_entity(command: &dyn EditorCommand) -> Option<(u64, KeyChannel)> {
    use crate::command::downcast_to;
    if let Some(set) = downcast_to::<SetTransform>(command) {
        return Some((set.entity, KeyChannel::Transform));
    }
    if let Some(gesture) = downcast_to::<TransformGesture>(command) {
        return Some((gesture.0.entity, KeyChannel::Transform));
    }
    if let Some(set) = downcast_to::<SetProperty>(command) {
        let short = set.component.rsplit("::").next().unwrap_or(&set.component);
        if short == "Transform" {
            return Some((set.entity, KeyChannel::Transform));
        }
        if set.path.is_empty() {
            return None;
        }
        return Some((
            set.entity,
            KeyChannel::Property(format!("{REFLECT_PREFIX}{short}.{}", set.path)),
        ));
    }
    if let Some(group) = downcast_to::<CommandGroup>(command) {
        return group.commands.iter().find_map(|c| edited_entity(c.as_ref()));
    }
    None
}

/// The auto key: when a new scene edit has ended (the undo stack is sealed on a state it has not
/// seen) on an entity the panel's player binds, keys it at the playhead and joins the key to that
/// undo entry, so one undo reverts both. Runs when the panel draws, at each `editor.seq_*`
/// call and once per shell frame ([`auto_key_frame`]). Returns the keys written.
pub fn auto_key_tick(world: &mut World) -> Vec<WrittenKey> {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let written = auto_key_with(world, &mut panel);
    world.insert_resource(panel);
    written
}

/// The shell's once-per-frame auto key (ADR-8730): runs [`auto_key_tick`] when auto key is on,
/// so a gizmo move or an inspector edit is keyed on the next shell frame even while the
/// Sequence Curves panel is closed. It never keys an edit twice: the auto key remembers the
/// highest undo state it has looked at, so the panel draw (which egui may run twice in a frame),
/// an `editor.seq_*` call and this hook key each finished edit once between them. With auto key
/// off it touches nothing.
pub fn auto_key_frame(world: &mut World) -> Vec<WrittenKey> {
    if !world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|authoring| authoring.auto_key)
    {
        return Vec::new();
    }
    let _span = tracing::trace_span!(target: "regia_editor", "seq_auto_key_frame").entered();
    auto_key_tick(world)
}

/// [`auto_key_tick`] with the panel the caller holds (the panel draw has it out of the world).
fn auto_key_with(world: &mut World, panel: &mut SeqCurvePanel) -> Vec<WrittenKey> {
    let (state, sealed) = match world.get_resource::<UndoStack>() {
        Some(undo) => (undo.state_id(), undo.is_sealed()),
        None => return Vec::new(),
    };
    let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>() else {
        return Vec::new();
    };
    if !authoring.auto_key {
        authoring.seen_state = authoring.seen_state.max(state);
        return Vec::new();
    }
    if !sealed || state <= authoring.seen_state {
        return Vec::new();
    }
    authoring.seen_state = state;
    let edited = world
        .get_resource::<UndoStack>()
        .and_then(UndoStack::top)
        .and_then(edited_entity);
    let Some((bits, channel)) = edited else {
        return Vec::new();
    };
    let Some(entity) = crate::command::resolve_entity(world, bits) else {
        return Vec::new();
    };
    let mut written = Vec::new();
    if let Some(player) = player_of(world, panel)
        && let Some(binding) = bindings_of(world, player, entity).into_iter().next()
    {
        match key_command(world, panel, &binding, &channel, None) {
            Ok((keys, command)) => {
                let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
                undo.join_top(Box::new(command));
                let state = undo.state_id();
                if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>() {
                    authoring.seen_state = state;
                }
                written = keys;
            }
            Err(e) => {
                tracing::debug!(target: "regia_editor", "auto key: {}", e.message);
            }
        }
    }
    written
}

// ---- edit-world preview ----

/// Captures `entity` as it is before the preview first writes it.
fn capture(world: &World, entity: Entity) -> Saved {
    let transform = world.get::<Transform>(entity).copied();
    let mut components = Vec::new();
    if let (Some(registry), Ok(entity_ref)) =
        (world.get_resource::<AppTypeRegistry>(), world.get_entity(entity))
    {
        let registry = registry.read();
        if let Ok(infos) = world.inspect_entity(entity) {
            for (_, info) in infos {
                let Some(type_id) = info.type_id() else { continue };
                if type_id == TypeId::of::<Transform>() {
                    continue;
                }
                let Some(data) = registry.get_type_data::<ReflectComponent>(type_id) else {
                    continue;
                };
                if let Some(value) = data
                    .reflect(entity_ref)
                    .and_then(|v| crate::inspector::reflect_ui::clone_value(v.as_partial_reflect()))
                {
                    components.push((type_id, value));
                }
            }
        }
    }
    Saved {
        entity,
        transform,
        components,
        touched: Vec::new(),
    }
}

/// The component types of `before.entity` whose value differs from `before` (or that appeared).
fn changed_types(world: &World, before: &Saved) -> Vec<TypeId> {
    let mut out = Vec::new();
    if world.get::<Transform>(before.entity).copied() != before.transform {
        out.push(TypeId::of::<Transform>());
    }
    let (Some(registry), Ok(entity_ref), Ok(infos)) = (
        world.get_resource::<AppTypeRegistry>(),
        world.get_entity(before.entity),
        world.inspect_entity(before.entity),
    ) else {
        return out;
    };
    let registry = registry.read();
    for (_, info) in infos {
        let Some(type_id) = info.type_id() else { continue };
        if type_id == TypeId::of::<Transform>() {
            continue;
        }
        let Some(data) = registry.get_type_data::<ReflectComponent>(type_id) else {
            continue;
        };
        let old = before.components.iter().find(|(t, _)| *t == type_id);
        // A value reflection cannot compare (`None`, an opaque type such as `Name`) counts as
        // unchanged: the appliers write comparable fields, and an unrelated edit must not be undone.
        let same = match (old, data.reflect(entity_ref)) {
            (Some((_, old)), Some(now)) => {
                now.as_partial_reflect().reflect_partial_eq(old.as_ref()) != Some(false)
            }
            // Appeared during the write (a `visible` track inserts `Visibility`).
            (None, Some(_)) => false,
            _ => true,
        };
        if !same {
            out.push(type_id);
        }
    }
    out
}

/// Puts back what the preview's writes changed on `saved.entity`: the transform, each touched
/// reflected component whose value differs, and touched components the preview added.
fn put_back(world: &mut World, saved: &Saved) {
    let entity = saved.entity;
    if world.get_entity(entity).is_err() {
        return;
    }
    if saved.touched.contains(&TypeId::of::<Transform>())
        && let Some(transform) = saved.transform
        && world.get::<Transform>(entity) != Some(&transform)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        e.insert(transform);
    }
    let Some(registry) = world.get_resource::<AppTypeRegistry>().cloned() else {
        return;
    };
    let registry = registry.read();
    let now: Vec<TypeId> = world
        .inspect_entity(entity)
        .map(|infos| infos.filter_map(|(_, i)| i.type_id()).collect())
        .unwrap_or_default();
    for type_id in now {
        if type_id == TypeId::of::<Transform>() || !saved.touched.contains(&type_id) {
            continue;
        }
        let Some(data) = registry.get_type_data::<ReflectComponent>(type_id) else {
            continue;
        };
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            return;
        };
        match saved.components.iter().find(|(t, _)| *t == type_id) {
            Some((_, old)) => {
                if let Some(mut current) = data.reflect_mut(&mut entity_mut)
                    && current.as_partial_reflect().reflect_partial_eq(old.as_ref()) != Some(true)
                    && let Err(e) = current.as_partial_reflect_mut().try_apply(old.as_ref())
                {
                    tracing::warn!(target: "regia_editor", "sequence preview restore: {e}");
                }
            }
            // Added by the preview (a `visible` track inserts `Visibility`).
            None => data.remove(&mut entity_mut),
        }
    }
}

/// Applies the previewed player's sequence at its time to the edit world: each keyed track whose
/// binding resolves through the player's table (sub-sequence tracks through the library), through
/// the property appliers (`reflect:` paths through reflection). An entity is captured before its
/// first write. Clip sections pose their rigs (`pose_clip_sections`); sound sections play and
/// video sections show on their bound players (`media`, ADR-8940).
fn apply_preview(world: &mut World, force: bool) {
    let _span = tracing::info_span!(target: "regia_editor", "seq_preview_apply").entered();
    let Some(player) = world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.preview.as_ref())
        .map(|p| p.player)
    else {
        return;
    };
    let Some(sequence_player) = world.get::<SequencePlayer>(player) else {
        stop_preview(world);
        return;
    };
    let time = sequence_player.time();
    // Solo (session state, ADR-8105): with any track soloed, only the soloed ones are evaluated.
    let sequence = with_solo(sequence_player.sequence(), &soloed(world, player));
    let unchanged = world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.preview.as_ref())
        .is_some_and(|p| p.time == time && p.applied.as_ref() == Some(&sequence));
    // A pitch-preserving section waiting on its stretch starts on the apply after the render
    // finishes, even with the playhead still.
    let stretch_ready = world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|a| a.media.stretch_finished());
    if unchanged && !force && !stretch_ready {
        // A tick that did not move the clock: the next move is judged on its own.
        if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>() {
            authoring.media.play_step = None;
        }
        return;
    }
    let library = world
        .get_resource::<regia_framework::sequencer::SequenceLibrary>()
        .cloned()
        .unwrap_or_default();
    let frame = sequence.sample_with(time, &library);
    let writes: Vec<(String, String, ChannelValue)> = frame
        .values
        .iter()
        .map(|v| (v.target.to_string(), v.property.to_string(), v.value))
        .collect();
    let clip_jobs: Vec<ClipPreviewJob> = frame
        .clips
        .iter()
        .map(|c| ClipPreviewJob {
            target: c.target.to_string(),
            blends: c
                .blends
                .iter()
                .map(|b| (b.clip.to_string(), b.clip_time, b.looping, b.weight))
                .collect(),
        })
        .collect();
    let sound_jobs: Vec<media::SoundJob> = frame
        .sounds
        .iter()
        .map(|s| media::SoundJob {
            path: s.path.clone(),
            track: s.track.to_string(),
            sound: s.sound.to_string(),
            position: s.position,
            rate: s.rate,
            volume_db: s.volume_db,
            fade: s.fade,
            bus: s.bus.map(str::to_string),
            spatial: s.spatial.cloned(),
            emitter: s.emitter.map(str::to_string),
            preserve: s.time_scale_mode == regia_framework::sequencer::TimeScaleMode::PreservePitch,
            section_offset: s.section_offset,
            section_source_secs: s.section_source_secs,
            slope: s
                .spatial
                .filter(|p| p.doppler > 0.0)
                .and(s.emitter)
                .and_then(|name| media::track_slope(&sequence, &library, time, name)),
        })
        .collect();
    // Pitch-preserving sections the playhead reaches within the lookahead render ahead (the
    // runtime's rule), while the transport plays forward.
    let playing_forward = world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|a| a.media.play_step == Some(media::PlayStep::Forward));
    let ahead: Vec<media::StretchAhead> = if playing_forward && media::has_preserve_pitch(&sequence) {
        let at = time + regia_framework::sequencer::STRETCH_LOOKAHEAD_SECS;
        sequence
            .sample_with(at, &library)
            .sounds
            .iter()
            .filter(|s| s.time_scale_mode == regia_framework::sequencer::TimeScaleMode::PreservePitch)
            .map(|s| media::StretchAhead {
                sound: s.sound.to_string(),
                section_offset: s.section_offset,
                section_source_secs: s.section_source_secs,
                rate: s.rate,
            })
            .collect()
    } else {
        Vec::new()
    };
    let video_jobs: Vec<media::VideoJob> = frame
        .videos
        .iter()
        .map(|v| media::VideoJob {
            path: v.path.clone(),
            track: v.track.to_string(),
            video: v.video.to_string(),
            target: v.target.clone(),
            binding: v.binding.map(str::to_string),
            position: v.position,
            looping: v.looping,
        })
        .collect();
    drop(frame);
    let appliers = world
        .get_resource::<PropertyAppliers>()
        .cloned()
        .unwrap_or_default();
    for (target, property, value) in writes {
        let Some(entity) = resolve_binding(world, player, &target) else {
            continue;
        };
        let captured = world
            .get_resource::<SeqAuthoring>()
            .and_then(|a| a.preview.as_ref())
            .is_some_and(|p| p.saved.iter().any(|s| s.entity == entity));
        if !captured {
            let saved = capture(world, entity);
            if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>()
                && let Some(preview) = authoring.preview.as_mut()
            {
                preview.saved.push(saved);
            }
        }
        let before = capture(world, entity);
        let result = match appliers.get(&property) {
            Some(applier) => match world.get_entity_mut(entity) {
                Ok(mut e) => applier(&mut e, value).map_err(|e| e.to_string()),
                Err(_) => continue,
            },
            None if property.starts_with(REFLECT_PREFIX) => apply_reflected(world, entity, &property, value),
            None => Err(format!("no applier for `{property}`")),
        };
        if let Err(reason) = result {
            tracing::debug!(target: "regia_editor", "sequence preview: {target}.{property}: {reason}");
        }
        let changed = changed_types(world, &before);
        if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>()
            && let Some(preview) = authoring.preview.as_mut()
            && let Some(saved) = preview.saved.iter_mut().find(|s| s.entity == entity)
        {
            for type_id in changed {
                if !saved.touched.contains(&type_id) {
                    saved.touched.push(type_id);
                }
            }
        }
    }
    pose_clip_sections(world, player, clip_jobs);
    let mut media = world
        .get_resource_mut::<SeqAuthoring>()
        .map(|mut a| std::mem::take(&mut a.media))
        .unwrap_or_default();
    media::sync_sounds(world, player, time, sound_jobs, ahead, &mut media);
    media::sync_videos(world, player, video_jobs, &mut media);
    if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>() {
        authoring.media = media;
        if let Some(preview) = authoring.preview.as_mut() {
            preview.time = time;
            preview.applied = Some(sequence);
        }
    }
}

/// One clip track in effect at the previewed time: the rig's binding name and each section's
/// clip name, clip time, looping flag and weight.
struct ClipPreviewJob {
    target: String,
    blends: Vec<(String, f64, bool, f32)>,
}

/// The preview's clip sections (ADR-8730): every joint a clip section posed before goes back to
/// its pre-preview pose first, then each rig a section plays now gets its clips (from the
/// `SequenceClips` resource, as at runtime) blended over that pose with
/// `regia_framework::sequencer::blend_clips_into`. A joint is captured before its first write and
/// Stop puts it back. A clip the library lacks is skipped. A rig with an `Animator` is posed the
/// same way with its animator suspended (ADR-8940): the edit world never evaluates animators, and
/// the animator is held out of the rig for the blend, so the clips go onto the joints (not into the
/// animator's evaluated pose) and the animator's state is never written; Stop puts the joints
/// back, which leaves rig and animator bit for bit as before.
fn pose_clip_sections(world: &mut World, player: Entity, jobs: Vec<ClipPreviewJob>) {
    let rests: Vec<(Entity, Transform)> = world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.preview.as_ref())
        .map(|p| {
            p.saved
                .iter()
                .filter(|s| p.posed_joints.contains(&s.entity))
                .filter_map(|s| s.transform.map(|t| (s.entity, t)))
                .collect()
        })
        .unwrap_or_default();
    for (joint, rest) in rests {
        if let Some(mut transform) = world.get_mut::<Transform>(joint)
            && *transform != rest
        {
            *transform = rest;
        }
    }
    if jobs.is_empty() {
        return;
    }
    let clips = world
        .get_resource::<regia_framework::sequencer::SequenceClips>()
        .cloned()
        .unwrap_or_default();
    for job in jobs {
        let Some(rig) = resolve_binding(world, player, &job.target) else {
            continue;
        };
        let Some(joints) = world
            .get::<regia_animation::SkinnedMesh>(rig)
            .map(|m| m.joints.clone())
        else {
            continue;
        };
        let blends: Vec<_> = job
            .blends
            .iter()
            .filter_map(|(name, time, looping, weight)| {
                let Some(clip) = clips.get(name) else {
                    tracing::debug!(target: "regia_editor", "sequence preview: no clip `{name}` in SequenceClips");
                    return None;
                };
                let at = regia_framework::sequencer::wrap_clip_time(*time, clip.duration, *looping);
                Some((std::sync::Arc::clone(clip), at, *weight))
            })
            .collect();
        if blends.is_empty() {
            continue;
        }
        for joint in joints {
            let known = world
                .get_resource::<SeqAuthoring>()
                .and_then(|a| a.preview.as_ref())
                .map(|p| {
                    (
                        p.saved.iter().any(|s| s.entity == joint),
                        p.posed_joints.contains(&joint),
                    )
                });
            let Some((captured, posed)) = known else {
                return;
            };
            if posed {
                continue;
            }
            let fresh = (!captured).then(|| capture(world, joint));
            if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>()
                && let Some(preview) = authoring.preview.as_mut()
            {
                if let Some(saved) = fresh {
                    preview.saved.push(saved);
                }
                if let Some(saved) = preview.saved.iter_mut().find(|s| s.entity == joint)
                    && !saved.touched.contains(&TypeId::of::<Transform>())
                {
                    saved.touched.push(TypeId::of::<Transform>());
                }
                preview.posed_joints.push(joint);
            }
        }
        // Suspend the rig's animator for the blend: without it the clips blend over the joints'
        // own pose and write the joints, as on an animator-less rig.
        let animator = world
            .get_entity_mut(rig)
            .ok()
            .and_then(|mut e| e.take::<regia_animation::Animator>());
        let blended = regia_framework::sequencer::blend_clips_into(world, rig, &blends);
        if let Some(animator) = animator {
            if let Ok(mut e) = world.get_entity_mut(rig) {
                e.insert(animator);
            }
            if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>()
                && let Some(preview) = authoring.preview.as_mut()
                && !preview.suspended.contains(&rig)
            {
                preview.suspended.push(rig);
            }
        }
        if let Err(error) = blended {
            tracing::debug!(target: "regia_editor", "sequence preview: clips on `{}`: {error}", job.target);
        }
    }
}

/// Starts (or refreshes) the preview of the panel's player at its current time. Codes:
/// `seq_curve_no_player`.
pub fn start_preview(world: &mut World, panel: &SeqCurvePanel) -> Result<(), EditError> {
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    let current = world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.preview.as_ref())
        .map(|p| p.player);
    if current.is_some_and(|p| p != player) {
        stop_preview(world);
    }
    let mut authoring = world.get_resource_or_insert_with(SeqAuthoring::default);
    if authoring.preview.is_none() {
        authoring.preview = Some(SeqPreview {
            player,
            time: 0.0,
            saved: Vec::new(),
            applied: None,
            drawn_frame: None,
            posed_joints: Vec::new(),
            suspended: Vec::new(),
        });
    }
    apply_preview(world, true);
    Ok(())
}

/// Re-applies a running preview (after an edit, an undo or a scrub).
pub fn refresh_preview(world: &mut World) {
    if world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|a| a.preview.is_some())
    {
        apply_preview(world, false);
    }
}

/// Stops the preview and puts back every value it touched, last captured first; stops its voices
/// and gives its taken-over video players back. Returns how many entities were restored.
pub fn stop_preview(world: &mut World) -> usize {
    let Some((preview, mut media)) = world.get_resource_mut::<SeqAuthoring>().and_then(|mut a| {
        let preview = a.preview.take()?;
        Some((preview, std::mem::take(&mut a.media)))
    }) else {
        return 0;
    };
    media::stop_sounds(world, &mut media);
    media::release_videos(world, &mut media);
    media.play_step = None;
    media.held = false;
    if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>() {
        authoring.media = media;
    }
    for saved in preview.saved.iter().rev() {
        put_back(world, saved);
    }
    preview.saved.len()
}

/// Runs `f` with the preview's values taken out of the world (put back, then re-applied after):
/// the level save path, so previewed values never reach a saved level. A previewed video needs no
/// lifting: its session lives in the never-saved `VideoPlayerState` and the authored `VideoPlayer`
/// is never written (ADR-7847), so a save, an autosave, an asset-watch pass or Play does not close
/// and re-open it.
pub fn with_preview_lifted<R>(world: &mut World, f: impl FnOnce(&mut World) -> R) -> R {
    let lifting = world
        .get_resource::<SeqAuthoring>()
        .is_some_and(|a| a.preview.as_ref().is_some_and(|p| !p.saved.is_empty()));
    if !lifting {
        return f(world);
    }
    // Put back without ending the preview: take the snapshots out, restore, return them.
    let snapshots = world
        .get_resource_mut::<SeqAuthoring>()
        .and_then(|mut a| a.preview.as_mut().map(|p| std::mem::take(&mut p.saved)))
        .unwrap_or_default();
    for s in snapshots.iter().rev() {
        put_back(world, s);
    }
    let out = f(world);
    if let Some(mut authoring) = world.get_resource_mut::<SeqAuthoring>()
        && let Some(preview) = authoring.preview.as_mut()
    {
        preview.saved = snapshots;
    }
    apply_preview(world, true);
    out
}

/// Ends the preview when the panel stopped drawing (closed or hidden) after it drew during the
/// preview. The shell calls it once per frame after the dock.
pub fn end_preview_when_hidden(world: &mut World, frame: u64) {
    let hidden = world
        .get_resource::<SeqAuthoring>()
        .and_then(|a| a.preview.as_ref())
        .and_then(|p| p.drawn_frame)
        .is_some_and(|drawn| drawn < frame);
    if hidden {
        stop_preview(world);
    }
}

// ---- panel rows ----

/// The authoring rows of the Sequence Curves panel: New sequence, Add track, track list edits,
/// Bind, Key / Auto-key and Preview. `player` is the panel's player, if any.
pub fn authoring_rows(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    world: &mut World,
    player: Option<Entity>,
) {
    let frame = ui.ctx().cumulative_frame_nr();
    let mut state = world.remove_resource::<SeqAuthoring>().unwrap_or_default();
    if let Some(preview) = state.preview.as_mut() {
        preview.drawn_frame = Some(frame);
    }
    world.insert_resource(state);
    auto_key_with(world, panel);
    refresh_preview(world);
    let mut state = world.remove_resource::<SeqAuthoring>().unwrap_or_default();
    let mut error: Option<EditError> = None;
    let mut keep = |r: Result<(), EditError>| {
        if let Err(e) = r {
            error = Some(e);
        }
    };
    ui.collapsing("Sequence authoring", |ui| {
        // New sequence (no Content Browser create path in this base; ADR-9710).
        ui.horizontal(|ui| {
            ui.label("New sequence");
            ui.add(egui::TextEdit::singleline(&mut state.new_path).hint_text("cine/new.seq.ron"));
            ui.add(
                egui::DragValue::new(&mut state.new_length)
                    .speed(0.1)
                    .range(0.0..=1.0e5)
                    .suffix(" s"),
            );
            ui.add(
                egui::DragValue::new(&mut state.new_fps)
                    .speed(1.0)
                    .range(1.0..=1000.0)
                    .suffix(" fps"),
            );
            if ui.button("Create").clicked() {
                let path = state.new_path.trim().to_string();
                keep(new_sequence(world, panel, &path, None, state.new_length, state.new_fps).map(|_| ()));
            }
        });
        let Some(player) = player else {
            return;
        };
        let Some(sequence) = world.get::<SequencePlayer>(player).map(|p| p.sequence().clone()) else {
            return;
        };
        // Add track.
        ui.horizontal(|ui| {
            ui.label("Add track");
            let kinds = TrackKind::ALL;
            egui::ComboBox::from_id_salt("seq_author_kind")
                .selected_text(kinds.get(state.new_kind).map_or("?", |k| k.name()))
                .show_ui(ui, |ui| {
                    for (i, kind) in kinds.iter().enumerate() {
                        ui.selectable_value(&mut state.new_kind, i, kind.name());
                    }
                });
            let hints = ["name", "binding", "property", "asset"];
            for (field, hint) in state.new_fields.iter_mut().zip(hints) {
                ui.add(
                    egui::TextEdit::singleline(field)
                        .hint_text(hint)
                        .desired_width(90.0),
                );
            }
            if ui.button("Add").clicked() {
                let kind = kinds.get(state.new_kind).copied().unwrap_or(TrackKind::Transform);
                let [name, binding, property, asset] = state.new_fields.clone();
                let mut spec = TrackSpec::new(kind, name.trim(), binding.trim());
                spec.property = property.trim().to_string();
                spec.asset = asset.trim().to_string();
                spec.time = world
                    .get::<SequencePlayer>(player)
                    .map_or(0.0, |p| p.time() as f32);
                spec.end = spec.time + 1.0;
                if spec.property == "transform.scale" {
                    spec.value = [1.0; 3];
                }
                keep(add_track(world, panel, &spec, None).map(|_| ()));
            }
        });
        // Every track as a focusable row with its menu (right-click or Shift+F10), and the
        // selected event track's keys (`.seq.ron` v6, ADR-8105).
        let solo = state.solo.get(&player).cloned().unwrap_or_default();
        for action in track_list(ui, panel, &sequence, &solo, player) {
            world.insert_resource(std::mem::take(&mut state));
            keep(run_track_row_action(world, panel, action));
            state = world.remove_resource::<SeqAuthoring>().unwrap_or_default();
        }
        if let Some(edit) = event_key_rows(
            ui,
            panel,
            &sequence,
            world
                .get::<SequencePlayer>(player)
                .map_or(0.0, |p| p.time() as f32),
        ) {
            let track = panel.track;
            keep(edit_event_keys(world, panel, track, edit).map(|_| ()));
        }
        // The selected track: up, down, rename, remove.
        if let Some(track) = sequence.tracks.get(panel.track) {
            ui.horizontal(|ui| {
                ui.label(format!("Track `{}`", track.name));
                let index = panel.track;
                if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                    keep(edit_track_list(world, panel, TrackEdit::Move { index, to: index - 1 }).map(|_| ()));
                }
                if ui
                    .add_enabled(index + 1 < sequence.tracks.len(), egui::Button::new("Down"))
                    .clicked()
                {
                    keep(edit_track_list(world, panel, TrackEdit::Move { index, to: index + 1 }).map(|_| ()));
                }
                let id = ui.id().with(("seq_author_rename", player));
                let mut text = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_default();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .hint_text("new name")
                        .desired_width(100.0),
                );
                ui.data_mut(|d| d.insert_temp(id, text.clone()));
                if response.lost_focus() && !text.trim().is_empty() {
                    ui.data_mut(|d| d.remove::<String>(id));
                    keep(
                        edit_track_list(
                            world,
                            panel,
                            TrackEdit::Rename {
                                index,
                                name: text.trim().to_string(),
                            },
                        )
                        .map(|_| ()),
                    );
                }
                if ui.button("Remove track").clicked() {
                    keep(edit_track_list(world, panel, TrackEdit::Remove { index }).map(|_| ()));
                }
            });
        }
        // Markers: add one at the playhead.
        ui.horizontal(|ui| {
            ui.label(format!("{} marker(s)", sequence.markers.len()));
            if ui.button("Add marker at playhead").clicked() {
                let time = world
                    .get::<SequencePlayer>(player)
                    .map_or(0.0, |p| p.time() as f32);
                let mut n = sequence.markers.len() + 1;
                while sequence.markers.iter().any(|m| m.name == format!("marker{n}")) {
                    n += 1;
                }
                let marker = Marker {
                    name: format!("marker{n}"),
                    time,
                    payload: String::new(),
                };
                keep(edit_markers(world, panel, MarkerEdit::Add { at: None, marker }).map(|_| ()));
            }
        });
        // Bind / unbind.
        ui.horizontal(|ui| {
            ui.label("Binding");
            ui.add(
                egui::TextEdit::singleline(&mut state.bind_name)
                    .hint_text("name")
                    .desired_width(100.0),
            );
            let selected = world
                .get_resource::<crate::selection::Selection>()
                .and_then(crate::selection::Selection::primary);
            if ui
                .add_enabled(selected.is_some(), egui::Button::new("Bind selected"))
                .clicked()
                && let Some(entity) = selected
            {
                let name = state.bind_name.trim().to_string();
                keep(add_binding(world, panel, &name, BindingSource::Entity(entity)));
            }
            ui.checkbox(&mut state.cascade, "cascade");
            if ui.button("Remove binding").clicked() {
                let name = state.bind_name.trim().to_string();
                keep(remove_binding(world, panel, &name, state.cascade).map(|_| ()));
            }
        });
        // Key, Auto-key, Preview.
        ui.horizontal(|ui| {
            if ui.button("Key").clicked() {
                keep(
                    selected_binding(world, panel)
                        .and_then(|b| key_binding(world, panel, &b, &KeyChannel::Transform, None))
                        .map(|_| ()),
                );
            }
            ui.checkbox(&mut state.auto_key, "Auto-key");
            let mut on = state.preview.is_some();
            if ui.checkbox(&mut on, "Preview in level").changed() {
                world.insert_resource(std::mem::take(&mut state));
                if on {
                    keep(start_preview(world, panel));
                } else {
                    stop_preview(world);
                }
                state = world.remove_resource::<SeqAuthoring>().unwrap_or_default();
            }
        });
        // Sequence properties: Restore on stop (`.seq.ron` `restore_on_stop`, one undo entry).
        keep(restore::row(ui, world, panel));
    });
    // `Create` and the preview toggle may have replaced the resource meanwhile; keep the newest
    // preview and the row fields.
    if let Some(inner) = world.remove_resource::<SeqAuthoring>() {
        state.preview = inner.preview.or(state.preview);
        state.seen_state = state.seen_state.max(inner.seen_state);
    }
    if let Some(e) = error {
        state.error = Some(e.message);
    }
    if let Some(e) = &state.error {
        ui.colored_label(egui::Color32::LIGHT_RED, e);
    }
    world.insert_resource(state);
}

/// What a track row (or its menu) asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackRowAction {
    /// Make the track the panel's track (a click, Enter or Space).
    Select(usize),
    /// Rename the track.
    Rename(usize, String),
    /// Duplicate the track after itself.
    Duplicate(usize),
    /// Delete the track.
    Delete(usize),
    /// Lock (`true`) or unlock it.
    Lock(usize, bool),
    /// Mute (`true`) or unmute it.
    Mute(usize, bool),
    /// Solo (`true`) or unsolo it (session state).
    Solo(usize, bool),
    /// Move it one row up.
    MoveUp(usize),
    /// Move it one row down.
    MoveDown(usize),
    /// Select every key of it in the timeline.
    SelectKeys(usize),
}

/// Runs a track row action through the same edits as `editor.seq_track_edit` (one undo entry each;
/// solo and select are not undo entries).
pub fn run_track_row_action(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    action: TrackRowAction,
) -> Result<(), EditError> {
    match action {
        TrackRowAction::Select(index) => {
            panel.track = index;
            panel.selected = None;
            panel.component = 0;
            Ok(())
        }
        TrackRowAction::Rename(index, name) => {
            edit_track_list(world, panel, TrackEdit::Rename { index, name }).map(|_| ())
        }
        TrackRowAction::Duplicate(index) => {
            edit_track_list(world, panel, TrackEdit::Duplicate { index }).map(|_| ())
        }
        TrackRowAction::Delete(index) => {
            edit_track_list(world, panel, TrackEdit::Remove { index }).map(|_| ())
        }
        TrackRowAction::Lock(index, locked) => {
            edit_track_list(world, panel, TrackEdit::Lock { index, locked }).map(|_| ())
        }
        TrackRowAction::Mute(index, muted) => {
            edit_track_list(world, panel, TrackEdit::Mute { index, muted }).map(|_| ())
        }
        TrackRowAction::Solo(index, on) => set_solo(world, panel, index, on).map(|_| ()),
        TrackRowAction::MoveUp(index) => {
            if index == 0 {
                return refuse("seq_track_bad_index", "the first track cannot move up");
            }
            edit_track_list(world, panel, TrackEdit::Move { index, to: index - 1 }).map(|_| ())
        }
        TrackRowAction::MoveDown(index) => {
            edit_track_list(world, panel, TrackEdit::Move { index, to: index + 1 }).map(|_| ())
        }
        TrackRowAction::SelectKeys(index) => select_track_keys(world, panel, index).map(|_| ()),
    }
}

/// The accessible name (and text) of a track row: the name, the kind, and its lock, mute and solo
/// state.
#[must_use]
pub fn track_row_name(track: &regia_framework::sequencer::Track, soloed: bool) -> String {
    let mut text = format!("{} ({})", track.name, track.keys.kind());
    for (on, word) in [
        (track.locked, "locked"),
        (!track.enabled, "muted"),
        (soloed, "solo"),
    ] {
        if on {
            text.push_str(", ");
            text.push_str(word);
        }
    }
    text
}

/// A track row's context menu, also reachable from the keyboard: Shift+F10 while the row has the
/// focus opens it under the row (Tab then walks its items); a right-click opens it at the pointer.
fn row_menu(response: &egui::Response, add: impl FnOnce(&mut egui::Ui)) {
    let ctx = &response.ctx;
    let id = egui::Popup::default_response_id(response);
    let by_key =
        response.has_focus() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F10));
    let mut popup = egui::Popup::context_menu(response);
    if by_key {
        popup = popup.open_memory(egui::SetOpenCommand::Bool(true));
    }
    if by_key || (egui::Popup::is_id_open(ctx, id) && egui::Popup::position_of_id(ctx, id).is_none()) {
        popup = popup.at_position(response.rect.left_bottom());
    }
    let _ = popup.show(add);
}

/// The track rows: one focusable row per track (click, Enter or Space selects it) with its menu.
fn track_list(
    ui: &mut egui::Ui,
    panel: &SeqCurvePanel,
    sequence: &Sequence,
    solo: &BTreeSet<String>,
    player: Entity,
) -> Vec<TrackRowAction> {
    let mut out = Vec::new();
    if sequence.tracks.is_empty() {
        return out;
    }
    ui.label(format!("Tracks ({})", sequence.tracks.len()));
    let count = sequence.tracks.len();
    for (i, track) in sequence.tracks.iter().enumerate() {
        let soloed = solo.contains(&track.name);
        let name = track_row_name(track, soloed);
        let row = egui::UiBuilder::new().id(egui::Id::new(("seq_track_row", player, i)));
        let response = ui
            .scope_builder(row, |ui| ui.selectable_label(panel.track == i, name.as_str()))
            .inner;
        if response.clicked() && crate::canvas_keys::first_in_this_frame(ui, response.id.with("select")) {
            out.push(TrackRowAction::Select(i));
        }
        let mut pick = |ui: &mut egui::Ui, action: TrackRowAction, salt: &str| {
            if crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("seq_track_menu", player, i, salt)))
            {
                out.push(action);
            }
            ui.close();
        };
        row_menu(&response, |ui| {
            ui.label(format!("Track {}", track.name));
            let id = egui::Id::new(("seq_track_rename", player, i));
            let mut text = ui
                .data(|d| d.get_temp::<String>(id))
                .unwrap_or_else(|| track.name.clone());
            let field = ui.add_enabled(
                !track.locked,
                egui::TextEdit::singleline(&mut text).desired_width(140.0),
            );
            let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Rename track");
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
            if field.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !text.trim().is_empty()
                && text.trim() != track.name
            {
                ui.data_mut(|d| d.remove::<String>(id));
                pick(ui, TrackRowAction::Rename(i, text.trim().to_string()), "rename");
            }
            ui.separator();
            if ui.button("Duplicate").clicked() {
                pick(ui, TrackRowAction::Duplicate(i), "duplicate");
            }
            if ui
                .add_enabled(!track.locked, egui::Button::new("Delete"))
                .clicked()
            {
                pick(ui, TrackRowAction::Delete(i), "delete");
            }
            if ui.button(if track.locked { "Unlock" } else { "Lock" }).clicked() {
                pick(ui, TrackRowAction::Lock(i, !track.locked), "lock");
            }
            if ui.button(if track.enabled { "Mute" } else { "Unmute" }).clicked() {
                pick(ui, TrackRowAction::Mute(i, track.enabled), "mute");
            }
            if ui.button(if soloed { "Unsolo" } else { "Solo" }).clicked() {
                pick(ui, TrackRowAction::Solo(i, !soloed), "solo");
            }
            if ui
                .add_enabled(i > 0 && !track.locked, egui::Button::new("Move up"))
                .clicked()
            {
                pick(ui, TrackRowAction::MoveUp(i), "up");
            }
            if ui
                .add_enabled(i + 1 < count && !track.locked, egui::Button::new("Move down"))
                .clicked()
            {
                pick(ui, TrackRowAction::MoveDown(i), "down");
            }
            if ui.button("Select all keys").clicked() {
                pick(ui, TrackRowAction::SelectKeys(i), "select_keys");
            }
        });
    }
    out
}

/// The selected event track's keys: time, event and payload fields per key (a change commits when
/// the drag or the edit ends), **Remove key**, and **Add event key** at the playhead.
fn event_key_rows(
    ui: &mut egui::Ui,
    panel: &SeqCurvePanel,
    sequence: &Sequence,
    playhead: f32,
) -> Option<EventKeyEdit> {
    let track = sequence.tracks.get(panel.track)?;
    let Channel::Event(keys) = &track.keys else {
        return None;
    };
    let mut out = None;
    ui.label(format!("Event keys of {}", track.name));
    for (index, key) in keys.iter().enumerate() {
        ui.horizontal(|ui| {
            let id = egui::Id::new(("seq_event_key", &track.name, index));
            let mut edited: EventKey = ui
                .data(|d| d.get_temp::<EventKey>(id))
                .unwrap_or_else(|| key.clone());
            let time = ui.add(
                egui::DragValue::new(&mut edited.time)
                    .speed(0.01)
                    .range(0.0..=1.0e5)
                    .suffix(" s"),
            );
            let time = crate::a11y_audit::name(time, egui::WidgetType::DragValue, "Event key time");
            let event = ui.add(egui::TextEdit::singleline(&mut edited.event).desired_width(100.0));
            let event = crate::a11y_audit::name(event, egui::WidgetType::TextEdit, "Event name");
            let payload = ui.add(egui::TextEdit::singleline(&mut edited.payload).desired_width(120.0));
            let payload = crate::a11y_audit::name(payload, egui::WidgetType::TextEdit, "Event payload");
            let commit = time.drag_stopped()
                || (time.changed() && !time.dragged())
                || event.lost_focus()
                || payload.lost_focus();
            if commit && edited != *key {
                ui.data_mut(|d| d.remove::<EventKey>(id));
                if crate::canvas_keys::first_in_this_frame(ui, id.with("set")) {
                    out = Some(EventKeyEdit::Set { index, key: edited });
                }
            } else if edited != *key {
                ui.data_mut(|d| d.insert_temp(id, edited));
            } else {
                ui.data_mut(|d| d.remove::<EventKey>(id));
            }
            if ui.button("Remove key").clicked()
                && crate::canvas_keys::first_in_this_frame(ui, id.with("remove"))
            {
                out = Some(EventKeyEdit::Remove { index });
            }
        });
    }
    if ui.button("Add event key at playhead").clicked()
        && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new(("seq_event_key_add", &track.name)))
    {
        out = Some(EventKeyEdit::Add {
            key: EventKey::new(playhead.max(0.0), track.name.clone()),
        });
    }
    out
}

// ---- agent methods ----

fn text<'a>(params: &'a Value, field: &str) -> Option<&'a str> {
    params.get(field).and_then(Value::as_str)
}

fn need_text<'a>(params: &'a Value, field: &str, why: &str) -> Result<&'a str, RpcError> {
    text(params, field)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (a string) is required {why}")))
}

fn index_of(params: &Value, field: &str) -> Result<Option<usize>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a whole number"))),
    }
}

/// Runs `f` with the panel resource taken out, after the auto key had its tick.
fn with_panel(
    world: &mut World,
    params: &Value,
    f: impl FnOnce(&mut World, &mut SeqCurvePanel) -> Result<Value, RpcError>,
) -> Result<Value, RpcError> {
    auto_key_tick(world);
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    // An agent call is its own undo entry, even while a pointer drag is open.
    let live = panel.gesture.take();
    let out = crate::seq_curve_panel::select(world, &mut panel, params)
        .map_err(rpc)
        .and_then(|()| f(world, &mut panel));
    panel.gesture = live;
    world.insert_resource(panel);
    out
}

fn summary(world: &mut World, panel: &SeqCurvePanel) -> Value {
    let Some(player) = player_of(world, panel) else {
        return json!({ "player": null });
    };
    let table: Vec<Value> = world
        .get::<SequenceBindings>(player)
        .map(|t| {
            t.iter()
                .map(|(name, binding)| match binding {
                    Binding::Guid(g) => json!({ "name": name, "guid": g.to_string() }),
                    Binding::Entity(e) => json!({ "name": name, "entity": e.to_bits() }),
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(p) = world.get::<SequencePlayer>(player) else {
        return json!({ "player": null });
    };
    let seq = p.sequence();
    let solo = soloed(world, player);
    json!({
        "player": player.to_bits(),
        "tracks": seq.tracks.iter().map(|t| {
            let mut row = json!({
                "name": t.name, "kind": t.keys.kind(), "target": t.target, "property": t.property, "len": t.keys.len(),
                "locked": t.locked, "muted": !t.enabled, "solo": solo.contains(&t.name)
            });
            if let Channel::Event(keys) = &t.keys {
                row["events"] = json!(keys.iter().map(|k| json!({ "time": k.time, "event": k.event, "payload": k.payload })).collect::<Vec<_>>());
            }
            row
        }).collect::<Vec<_>>(),
        "markers": seq.markers.iter().map(|m| json!({ "name": m.name, "time": m.time })).collect::<Vec<_>>(),
        "spawnables": seq.spawnables.iter().map(|s| json!({ "binding": s.binding, "prefab": s.prefab })).collect::<Vec<_>>(),
        "bindings": table,
        "track": panel.track,
    })
}

fn track_index(world: &mut World, panel: &SeqCurvePanel, params: &Value) -> Result<usize, EditError> {
    if let Some(name) = text(params, "track") {
        let found = player_of(world, panel)
            .and_then(|e| world.get::<SequencePlayer>(e))
            .and_then(|p| p.sequence().tracks.iter().position(|t| t.name == name));
        return found.ok_or_else(|| EditError {
            code: "seq_track_bad_index",
            message: format!("no track named `{name}`"),
        });
    }
    Ok(panel.track)
}

fn marker_index(world: &mut World, panel: &SeqCurvePanel, params: &Value) -> Result<usize, EditError> {
    let name = text(params, "name").unwrap_or_default();
    player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .and_then(|p| p.sequence().markers.iter().position(|m| m.name == name))
        .ok_or_else(|| EditError {
            code: "seq_marker_bad_index",
            message: format!("no marker named `{name}`"),
        })
}

fn vec3_value(params: &Value) -> Result<Option<[f32; 3]>, RpcError> {
    match params.get("value").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|f| Some([f as f32, 0.0, 0.0]))
            .ok_or_else(|| RpcError::invalid_params("`value` must be a number or [x, y, z]")),
        Some(Value::Bool(b)) => Ok(Some([f32::from(u8::from(*b)), 0.0, 0.0])),
        Some(Value::Array(a)) if a.len() == 3 => {
            let mut out = [0.0; 3];
            for (slot, v) in out.iter_mut().zip(a) {
                *slot = v
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params("`value` must be a number or [x, y, z]"))?
                    as f32;
            }
            Ok(Some(out))
        }
        Some(_) => Err(RpcError::invalid_params(
            "`value` must be a number, a bool or [x, y, z]",
        )),
    }
}

/// `editor.seq_track_edit`.
fn seq_track_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = need_text(params, "op", "(add, remove, rename or move)")?.to_string();
    let kind = text(params, "kind").unwrap_or("").to_string();
    with_panel(world, params, |world, panel| {
        let index = if kind == "marker" {
            let edit = match op.as_str() {
                "add" => MarkerEdit::Add {
                    at: index_of(params, "at")?,
                    marker: Marker {
                        name: need_text(params, "name", "for a marker")?.to_string(),
                        time: number(params, "time")?.unwrap_or(0.0),
                        payload: text(params, "payload").unwrap_or_default().to_string(),
                    },
                },
                "remove" => MarkerEdit::Remove {
                    index: marker_index(world, panel, params).map_err(rpc)?,
                },
                "rename" => MarkerEdit::Rename {
                    index: marker_index(world, panel, params).map_err(rpc)?,
                    name: need_text(params, "new_name", "for rename")?.to_string(),
                },
                "move" => MarkerEdit::Move {
                    index: marker_index(world, panel, params).map_err(rpc)?,
                    to: index_of(params, "to")?
                        .ok_or_else(|| RpcError::invalid_params("`to` is required for move"))?,
                },
                other => return Err(RpcError::invalid_params(format!("unknown `op` `{other}`"))),
            };
            edit_markers(world, panel, edit).map_err(rpc)?
        } else {
            match op.as_str() {
                "add" => {
                    let kind = TrackKind::parse(&kind).ok_or_else(|| {
                        rpc(EditError {
                            code: "seq_track_kind_unknown",
                            message: format!(
                                "`kind` `{kind}` is not one of transform, float, vec3, bool, clip, audio, spatial_audio, sub, video, fade, event, marker"
                            ),
                        })
                    })?;
                    let binding = text(params, "binding").unwrap_or_default();
                    let name = text(params, "name")
                        .map_or_else(|| format!("{binding}.{}", kind.name()), str::to_string);
                    let mut spec = TrackSpec::new(kind, name, binding);
                    spec.property = text(params, "property").unwrap_or_default().to_string();
                    spec.asset = text(params, "asset").unwrap_or_default().to_string();
                    spec.time = number(params, "time")?.unwrap_or(0.0);
                    spec.end = number(params, "end")?.unwrap_or(spec.time + 1.0);
                    if let Some(v) = vec3_value(params)? {
                        spec.value = v;
                    } else if spec.property == "transform.scale" {
                        spec.value = [1.0; 3];
                    }
                    if let Some(event) = text(params, "event") {
                        spec.asset = event.to_string();
                    }
                    let mut track = tracks::starter_track(&spec).map_err(rpc)?;
                    if let (Channel::Event(keys), Some(payload)) = (&mut track.keys, text(params, "payload"))
                    {
                        for key in keys.iter_mut() {
                            key.payload = payload.to_string();
                        }
                    }
                    add_built_track(world, panel, track, index_of(params, "at")?).map_err(rpc)?
                }
                "duplicate" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    edit_track_list(world, panel, TrackEdit::Duplicate { index }).map_err(rpc)?
                }
                "lock" | "mute" | "solo" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    let on = match params.get("on").filter(|v| !v.is_null()) {
                        None => true,
                        Some(v) => v
                            .as_bool()
                            .ok_or_else(|| RpcError::invalid_params("`on` must be a bool"))?,
                    };
                    match op.as_str() {
                        "lock" => edit_track_list(world, panel, TrackEdit::Lock { index, locked: on })
                            .map_err(rpc)?,
                        "mute" => edit_track_list(world, panel, TrackEdit::Mute { index, muted: on })
                            .map_err(rpc)?,
                        _ => {
                            set_solo(world, panel, index, on).map_err(rpc)?;
                            index
                        }
                    }
                }
                "select_keys" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    let count = select_track_keys(world, panel, index).map_err(rpc)?;
                    let mut out = summary(world, panel);
                    out["applied"] = json!(true);
                    out["index"] = json!(index);
                    out["selected"] = json!(count);
                    return Ok(out);
                }
                "event_key" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    let action = need_text(params, "action", "for event_key (add, set or remove)")?;
                    let key_index = || -> Result<usize, RpcError> {
                        index_of(params, "key")?
                            .ok_or_else(|| RpcError::invalid_params("`key` (the key index) is required"))
                    };
                    let new_key = || -> Result<EventKey, RpcError> {
                        Ok(EventKey {
                            time: number(params, "time")?.unwrap_or(0.0),
                            event: need_text(params, "event", "for an event key")?.to_string(),
                            payload: text(params, "payload").unwrap_or_default().to_string(),
                        })
                    };
                    let edit = match action {
                        "add" => EventKeyEdit::Add { key: new_key()? },
                        "set" => EventKeyEdit::Set {
                            index: key_index()?,
                            key: new_key()?,
                        },
                        "remove" => EventKeyEdit::Remove { index: key_index()? },
                        other => {
                            return Err(RpcError::invalid_params(format!(
                                "unknown event_key `action` `{other}` (add, set, remove)"
                            )));
                        }
                    };
                    let key = edit_event_keys(world, panel, index, edit).map_err(rpc)?;
                    let mut out = summary(world, panel);
                    out["applied"] = json!(true);
                    out["index"] = json!(index);
                    out["key"] = json!(key);
                    return Ok(out);
                }
                "remove" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    edit_track_list(world, panel, TrackEdit::Remove { index }).map_err(rpc)?
                }
                "rename" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    let name = need_text(params, "new_name", "for rename")?.to_string();
                    edit_track_list(world, panel, TrackEdit::Rename { index, name }).map_err(rpc)?
                }
                "move" => {
                    let index = track_index(world, panel, params).map_err(rpc)?;
                    let to = index_of(params, "to")?
                        .ok_or_else(|| RpcError::invalid_params("`to` is required for move"))?;
                    edit_track_list(world, panel, TrackEdit::Move { index, to }).map_err(rpc)?
                }
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "unknown `op` `{other}` (add, remove, rename, move, duplicate, lock, mute, solo, select_keys, event_key)"
                    )));
                }
            }
        };
        let mut out = summary(world, panel);
        out["applied"] = json!(true);
        out["index"] = json!(index);
        Ok(out)
    })
}

/// `editor.seq_binding_edit`.
fn seq_binding_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = need_text(params, "op", "(add or remove)")?.to_string();
    let binding = need_text(params, "binding", "(the binding name)")?.to_string();
    with_panel(world, params, |world, panel| {
        let removed = match op.as_str() {
            "add" => {
                let source = if let Some(prefab) = text(params, "prefab") {
                    let end = match params.get("end") {
                        None | Some(Value::Null) => None,
                        Some(_) => number(params, "end")?,
                    };
                    BindingSource::Spawnable(Spawnable {
                        binding: binding.clone(),
                        prefab: prefab.to_string(),
                        start: number(params, "start")?.unwrap_or(0.0),
                        end,
                        keep_on_stop: params
                            .get("keep_on_stop")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                } else if let Some(guid) = text(params, "guid") {
                    let guid = guid
                        .parse::<Guid>()
                        .map_err(|_| RpcError::invalid_params("`guid` must be a scene guid"))?;
                    BindingSource::Guid(guid)
                } else {
                    let bits = params.get("entity").and_then(Value::as_u64).ok_or_else(|| {
                        RpcError::invalid_params("`entity`, `guid` or `prefab` is required for add")
                    })?;
                    let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
                        rpc(EditError {
                            code: "seq_binding_no_entity",
                            message: format!("entity {bits} does not exist"),
                        })
                    })?;
                    BindingSource::Entity(entity)
                };
                add_binding(world, panel, &binding, source).map_err(rpc)?;
                None
            }
            "remove" => {
                let cascade = params.get("cascade").and_then(Value::as_bool).unwrap_or(false);
                Some(remove_binding(world, panel, &binding, cascade).map_err(rpc)?)
            }
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown `op` `{other}` (add, remove)"
                )));
            }
        };
        let mut out = summary(world, panel);
        out["applied"] = json!(true);
        if let Some(uses) = removed {
            out["removed"] = json!({
                "tracks": uses.tracks, "sounds": uses.sounds, "videos": uses.videos,
                "sub_entries": uses.sub_entries, "spawnable": uses.spawnable,
            });
        }
        Ok(out)
    })
}

/// `editor.seq_new`.
fn seq_new(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = need_text(params, "path", "(a project .seq.ron path)")?.to_string();
    let length = number(params, "length")?.unwrap_or(5.0);
    let fps = number(params, "fps")?.unwrap_or(30.0);
    let name = text(params, "name").map(str::to_string);
    auto_key_tick(world);
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let made = new_sequence(world, &mut panel, &path, name.as_deref(), length, fps);
    world.insert_resource(panel);
    let player = made.map_err(rpc)?;
    let duration = world
        .get::<SequencePlayer>(player)
        .map_or(0.0, SequencePlayer::duration);
    Ok(json!({ "player": player.to_bits(), "path": path, "length": duration, "fps": fps }))
}

fn key_json(keys: &[WrittenKey]) -> Value {
    json!(
        keys.iter()
            .map(|k| {
                let value = match k.value {
                    ChannelValue::Float(v) => json!(v),
                    ChannelValue::Vec3(v) => json!(v),
                    ChannelValue::Bool(v) => json!(v),
                };
                json!({ "track": k.track, "property": k.property, "time": k.time, "value": value, "created": k.created })
            })
            .collect::<Vec<_>>()
    )
}

/// `editor.seq_key`'s `entity` (ADR-7747): the scene entity whose binding to key instead of the
/// selection's. Refused with `binding` (they name the same thing twice).
fn key_entity(world: &World, params: &Value) -> Result<Option<Entity>, RpcError> {
    let Some(value) = params.get("entity").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if text(params, "binding").is_some() {
        return Err(rpc(EditError {
            code: "seq_key_binding_and_entity",
            message: "pass `binding` or `entity`, not both".into(),
        }));
    }
    let bits = value
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
    crate::command::resolve_entity(world, bits)
        .map(Some)
        .ok_or_else(|| {
            rpc(EditError {
                code: "seq_key_no_entity",
                message: format!("entity {bits} does not exist"),
            })
        })
}

/// The first binding of `entity` in the panel's sequence (`seq_key_unbound` when it has none).
fn entity_binding(world: &mut World, panel: &SeqCurvePanel, entity: Entity) -> Result<String, EditError> {
    let Some(player) = player_of(world, panel) else {
        return refuse("seq_curve_no_player", "no sequence player exists");
    };
    bindings_of(world, player, entity)
        .into_iter()
        .next()
        .ok_or_else(|| EditError {
            code: "seq_key_unbound",
            message: "the entity is not bound in this sequence (Bind adds it)".into(),
        })
}

/// `editor.seq_key`.
fn seq_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = key_entity(world, params)?;
    let auto = params.get("auto").filter(|v| !v.is_null());
    if let Some(auto) = auto {
        let on = auto
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`auto` must be true or false"))?;
        // The edits before the toggle are not keyed.
        let state = world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id);
        let mut authoring = world.get_resource_or_insert_with(SeqAuthoring::default);
        authoring.auto_key = on;
        authoring.seen_state = authoring.seen_state.max(state);
        if text(params, "binding").is_none() && entity.is_none() {
            return Ok(json!({ "auto_key": on, "keys": [] }));
        }
    }
    let channel = KeyChannel::parse(text(params, "channel").unwrap_or("transform"));
    let time = number(params, "time")?;
    with_panel(world, params, |world, panel| {
        let binding = match (text(params, "binding"), entity) {
            (Some(b), _) => b.to_string(),
            (None, Some(entity)) => entity_binding(world, panel, entity).map_err(rpc)?,
            (None, None) => selected_binding(world, panel).map_err(rpc)?,
        };
        let keys = key_binding(world, panel, &binding, &channel, time).map_err(rpc)?;
        let auto_key = world.get_resource::<SeqAuthoring>().is_some_and(|a| a.auto_key);
        Ok(json!({ "binding": binding, "keys": key_json(&keys), "auto_key": auto_key }))
    })
}

/// `editor.seq_preview`.
fn seq_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`on` (true or false) is required"))?;
    let time = number(params, "time")?;
    with_panel(world, params, |world, panel| {
        if !on {
            let restored = stop_preview(world);
            return Ok(json!({ "on": false, "restored": restored, "voices": voices_json(world) }));
        }
        if let Some(t) = time
            && let Some(player) = player_of(world, panel)
            && let Some(mut p) = world.get_mut::<SequencePlayer>(player)
        {
            p.seek(f64::from(t.max(0.0)));
        }
        start_preview(world, panel).map_err(rpc)?;
        let voices = voices_json(world);
        let authoring = world.get_resource::<SeqAuthoring>();
        let preview = authoring.and_then(|a| a.preview.as_ref());
        Ok(json!({
            "on": true,
            "time": preview.map(|p| p.time),
            "touched": preview.map(|p| p.touched().iter().map(|e| e.to_bits()).collect::<Vec<_>>()),
            "animators": preview.map(|p| p.suspended.iter().map(|e| e.to_bits()).collect::<Vec<_>>()),
            "sounds": authoring.map(|a| a.media.sounds_json.clone()).unwrap_or_default(),
            "videos": authoring.map(|a| a.media.videos_json.clone()).unwrap_or_default(),
            "video_opens": authoring.map_or(0, |a| a.media.video_opens),
            "voices": voices,
            "media": authoring.map(|a| a.media.stats_json()),
        }))
    })
}

/// The session's preview voice counters as `{ started, stopped }`.
fn voices_json(world: &World) -> Value {
    let (started, stopped) = world
        .get_resource::<SeqAuthoring>()
        .map_or((0, 0), SeqAuthoring::voice_counts);
    json!({ "started": started, "stopped": stopped })
}

const PLAYER: ParamSchema = ParamSchema {
    name: "player",
    kind: "number",
    required: false,
    description: "Sequence player entity id (default: the panel's player, else the first).",
};

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

const TRACK_EDIT_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "op",
        "string",
        true,
        "add, remove, rename or move; tracks only: duplicate, lock, mute, solo (with `on`), select_keys, event_key.",
    ),
    param(
        "kind",
        "string",
        false,
        "add: the track kind; `marker` edits markers for every op.",
    ),
    param(
        "track",
        "string",
        false,
        "remove, rename, move: the track name (default: the panel's track).",
    ),
    param(
        "name",
        "string",
        false,
        "add: the new track's or marker's name; marker remove/rename/move: the marker.",
    ),
    param("new_name", "string", false, "rename: the new name."),
    param(
        "binding",
        "string",
        false,
        "add: the binding (target, spatial emitter or video player).",
    ),
    param(
        "property",
        "string",
        false,
        "add float/vec3/bool: the property path; transform: position, rotation or scale path.",
    ),
    param(
        "asset",
        "string",
        false,
        "add clip/audio/sub/video: the clip, sound, sub-sequence or video.",
    ),
    param(
        "time",
        "number",
        false,
        "add: the first key's time or the first section's start (seconds).",
    ),
    param(
        "end",
        "number",
        false,
        "add sections: the first section's end (default time + 1).",
    ),
    param(
        "value",
        "any",
        false,
        "add keyed: the first value (a number, a bool or [x, y, z]).",
    ),
    param(
        "payload",
        "string",
        false,
        "marker add: the payload; add kind event and event_key add/set: the event key's payload.",
    ),
    param(
        "at",
        "number",
        false,
        "add: the index to insert at (default: the end).",
    ),
    param(
        "to",
        "number",
        false,
        "move: the index the track or marker ends at.",
    ),
    param(
        "on",
        "boolean",
        false,
        "lock, mute, solo: true (the default) turns it on, false off. Solo is editor session state.",
    ),
    param(
        "event",
        "string",
        false,
        "add kind event: the first key's event name (default: the track name); event_key add/set: the event name.",
    ),
    param("action", "string", false, "event_key: add, set or remove."),
    param("key", "number", false, "event_key set/remove: the key index."),
];

const BINDING_EDIT_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param("op", "string", true, "add or remove."),
    param("binding", "string", true, "The binding name tracks target."),
    param("entity", "number", false, "add: the scene entity id to bind."),
    param("guid", "string", false, "add: the scene guid to bind."),
    param(
        "prefab",
        "string",
        false,
        "add: a spawnable's .prefab.ron path or guid:<32 hex>.",
    ),
    param("start", "number", false, "add spawnable: seconds it appears at."),
    param(
        "end",
        "number",
        false,
        "add spawnable: seconds it disappears at (default: the end).",
    ),
    param(
        "keep_on_stop",
        "boolean",
        false,
        "add spawnable: leave it in the world when the player stops.",
    ),
    param(
        "cascade",
        "boolean",
        false,
        "remove: also remove every track and section that uses the binding.",
    ),
];

const NEW_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        true,
        "Project-relative path ending in .seq.ron.",
    ),
    param(
        "length",
        "number",
        false,
        "Seconds (default 5; 0 for no end marker).",
    ),
    param("fps", "number", false, "Authoring frame rate (default 30)."),
    param("name", "string", false, "Display name (default: the file stem)."),
];

const KEY_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "binding",
        "string",
        false,
        "The binding (default: the selected entity's binding).",
    ),
    param(
        "entity",
        "number",
        false,
        "A bound scene entity: key its binding instead of the selection's (not with `binding`).",
    ),
    param(
        "channel",
        "string",
        false,
        "transform (default), position, rotation, scale or a property path.",
    ),
    param("time", "number", false, "Seconds (default: the playhead)."),
    param(
        "auto",
        "boolean",
        false,
        "Set the Auto-key toggle; without `binding` or `entity` nothing is keyed.",
    ),
];

const PREVIEW_PARAMS: &[ParamSchema] = &[
    PLAYER,
    param(
        "on",
        "boolean",
        true,
        "true starts or refreshes the preview, false stops it.",
    ),
    param(
        "time",
        "number",
        false,
        "on: seek the player here first (seconds).",
    ),
];

/// The agent methods of the authoring rows.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.seq_track_edit",
                tool: "seq_track_edit",
                description: "Add, remove, rename or move a track of the panel's sequence (kinds transform, float, \
                    vec3, bool, clip, audio, spatial_audio, sub, video, event; fade is refused: fades are .shots.ron \
                    data), or with kind `marker` a marker; duplicate, lock, mute or solo a track, select its keys, \
                    or add, set or remove an event track's keys (event_key). A locked track refuses edits of its \
                    keys, sections, name, binding or position (seq_track_locked). Validated on a copy: a refused \
                    edit changes nothing. One undo entry (solo and select_keys are session state).",
                params: TRACK_EDIT_PARAMS,
                result: "{ applied, index, player, tracks: [{ name, kind, target, property, len, locked, muted, solo, events? }], markers: [{ name, time }], spawnables, bindings, track, selected?, key? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_track_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_binding_edit",
                tool: "seq_binding_edit",
                description: "Add a binding to the panel's sequence player (a scene entity, by its guid when it has \
                    one; a scene guid; or a spawnable prefab) or remove one (refused with seq_binding_used while \
                    tracks use it, unless `cascade`, which removes those tracks and sections too). One undo entry.",
                params: BINDING_EDIT_PARAMS,
                result: "{ applied, player, tracks, markers, spawnables, bindings: [{ name, guid | entity }], removed? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_binding_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_new",
                tool: "seq_new",
                description: "Create an empty .seq.ron v5 under the project (refused when the file exists) and open it \
                    in the Sequence Curves panel. The length is an `end` marker (a sequence ends at its last key, \
                    section or marker).",
                params: NEW_PARAMS,
                result: "{ player, path, length, fps }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_new as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_key",
                tool: "seq_key",
                description: "Key a bound entity at the playhead from its current values (the panel's Key button), \
                    creating each missing track: channel transform (position, rotation and scale), position, \
                    rotation, scale or a property path (reflect:<Component>.<field> for any reflected float, vec3 or \
                    bool). `auto` turns the Auto-key toggle on or off. One undo entry.",
                params: KEY_PARAMS,
                result: "{ binding, keys: [{ track, property, time, value, created }], auto_key }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_key as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_preview",
                tool: "seq_preview",
                description: "Preview the panel's sequence in the edit world at the playhead (`time` seeks first), or \
                    stop and put back every value the preview touched. Clip sections pose their rigs (an Animator \
                    rig with its animator suspended), sound sections play on the editor's audio output (flat, or \
                    spatial at the bound emitter, with the runtime's Doppler, occlusion and pitch-preserving \
                    stretch) and a video section shows the picture of its source time on its bound video player, \
                    over the viewport (fullscreen) or on its mesh's material slot; stop silences the voices and \
                    gives the players and materials back. `on: true` with no `time` reads the state. A level save \
                    never sees previewed values.",
                params: PREVIEW_PARAMS,
                result: "{ on, time?, touched?: [entity], animators?: [entity], sounds?: [{ path, track, sound, position, spatial, emitter, volume_db, pan, pitch, doppler, velocity, occlusion, cutoff_hz, stretched, playing, error }], videos?: [{ path, track, video, target, entity, source_time, frame, pts_ns, publishes?, error, message }], video_opens?, media?: { stretches: { rendered, inline, pending }, occlusion_mirror_builds }, voices: { started, stopped }, restored? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_preview as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
