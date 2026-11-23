// SPDX-License-Identifier: MPL-2.0
//! Prefab edit mode (`docs/specs/19-editor.md` § 2 "Asset editors: Prefab editor (isolated
//! scene)").
//!
//! Opening a prefab swaps the edit world to an isolated scene holding the prefab's contents as
//! plain editable entities (the level that was open is snapshotted and stashed). **Save**
//! writes the edited contents back to the `.prefab.ron`, re-bakes the template under the file's
//! asset id and re-instantiates every live instance of that prefab in the open level — so
//! instances update. **Close** restores the stashed level.
//!
//! The undo history does not survive the mode switch (undo entries reference entity ids that
//! respawn with new ids), so the stack is cleared on both transitions; a toast says so.
//!
//! **Edit in place** ([`enter_prefab_edit_in_context`], addendum § 9.2) opens the same document
//! from an instance in the level: the level is stashed as above, but it stays in the world as
//! read-only context ([`PrefabContext`]: not pickable, out of the Outliner, never part of the
//! prefab), the instance itself is hidden, and the prefab's contents spawn under a frame entity
//! that carries the instance's world transform ([`PrefabFrame`]; the contents stay pickable and
//! the Outliner lists them as roots). Save writes the prefab and rebuilds every
//! instance, the context ones included; close drops the context and restores the stash, as for
//! the isolated mode. A level save is refused while either session runs ([`level_save_refusal`]).
//! A variant cannot be opened (spec 83 § 9).

use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;

use regia_scene::level::open_level;
use regia_scene::prefab::PrefabTemplate;
use regia_scene::prelude::{
    Level, LevelId, LevelLibrary, LoadMode, PrefabInstance, PrefabLibrary, SceneFile, SpawnOptions,
    snapshot_scene, spawn_scene,
};
use regia_scene::spawn::UnknownTypePolicyOption;

use crate::command::UndoStack;
use crate::content::prefab_asset_id;
use crate::level_io::{self, OpenLevel};

/// The prefab being edited, plus the edit level stashed away while it is open.
#[derive(Resource, Debug, Clone, Default)]
pub struct PrefabEditMode {
    /// The `.prefab.ron` being edited (`None` = not in prefab mode).
    pub path: Option<PathBuf>,
    /// The file's stable asset id (instances reference it).
    asset: Option<Guid>,
    /// The edit level's contents and open state, restored on close.
    stash: Option<StashedLevel>,
    /// Status line for the shell / agent replies.
    pub toast: String,
    /// The prefab text the last autosave tick took (`crate::prefab_autosave`).
    autosave_text: Option<String>,
    /// Edit in place: the frame entity and the hidden instance (`None` for the isolated mode).
    in_context: Option<InContext>,
}

/// What an Edit in place session keeps beside the stash.
#[derive(Debug, Clone)]
struct InContext {
    /// The frame entity the prefab's contents hang under, as entity bits.
    frame: u64,
    /// The instance opened in place (hidden while the session runs), by guid.
    instance: Option<Guid>,
}

/// A level entity drawn around a prefab opened in place ([`enter_prefab_edit_in_context`]): not
/// pickable ([`crate::viewport_ops::EditorLocked`]), out of the Outliner and the selection
/// ([`crate::visibility::EditorHidden`]) and never part of the prefab. Session state (no
/// reflection registration): the level comes back from the stash when the mode closes. The
/// session's frame ([`PrefabFrame`]) is context too, but not locked.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefabContext;

/// The frame entity an Edit in place session hangs the prefab's contents under (it carries the
/// instance's world transform). It is [`PrefabContext`] (never part of the prefab, dropped on
/// close) and [`crate::visibility::EditorHidden`] (no proxy cube, not itself pickable, deletable
/// or a reparent target), but not [`crate::viewport_ops::EditorLocked`]: the lock walks
/// ancestors, so a locked frame would make the opened prefab unpickable. The Outliner lists the
/// frame's children as roots ([`is_in_place_frame`]).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefabFrame;

/// A member of another loaded level during an Edit in place session (P-160): drawn dimmed and not
/// pickable like the context, but it stays loaded when the session closes. It records what the
/// session added, so the close takes off exactly that (a lock or an override the entity had
/// before stays). Session state (no reflection registration).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefabOtherLevel {
    /// The session added the [`crate::viewport_ops::EditorLocked`].
    locked: bool,
    /// The session added the dimming [`regia_render::RenderOverride`].
    dimmed: bool,
}

/// How bright the context of an Edit in place session draws: its base colour times this factor
/// (an editor-only per-entity tint, [`context_dim`]; nothing of it is saved).
pub const CONTEXT_DIM_FACTOR: f32 = 0.35;

/// The dimming tint of the Edit in place context: [`CONTEXT_DIM_FACTOR`] on the base colour,
/// full coverage. A [`regia_render::RenderOverride`] is session state (not reflected), so a
/// level snapshot never holds it.
#[must_use]
pub fn context_dim() -> regia_render::RenderOverride {
    let f = CONTEXT_DIM_FACTOR;
    regia_render::RenderOverride::tinted([f, f, f, 1.0])
}

/// Whether `entity` is drawn as Edit in place context: a [`PrefabContext`] entity other than the
/// session's [`PrefabFrame`]. Such an entity is editor-internal (out of the Outliner, the
/// selection and picking) but still drawn, dimmed: its proxy cube and the terrain tool's
/// preview keep it in the picture.
#[must_use]
pub fn is_drawn_context(world: &World, entity: Entity) -> bool {
    world.get::<PrefabContext>(entity).is_some() && world.get::<PrefabFrame>(entity).is_none()
}

/// Whether `entity` is the frame of an Edit in place session ([`PrefabFrame`]).
#[must_use]
pub fn is_in_place_frame(world: &World, entity: Entity) -> bool {
    world.get::<PrefabFrame>(entity).is_some()
}

/// Why a level save is refused right now, or `None` when it may run: while a prefab is open in
/// place, the open level in the world carries the session's state (the hidden instance, the
/// read-only context markers, a cleared path) and the undo history is the prefab's, so a level
/// save would write that state into the level file and mark the prefab's edits saved. While a
/// prefab is open on its own (isolated), the open level is the mode's scratch level and a save
/// would write it over a level file (P-160). Close the prefab first; the level comes back from
/// the stash with its path.
#[must_use]
pub fn level_save_refusal(world: &World) -> Option<&'static str> {
    level_save_refusal_reason(world).map(|(_, message)| message)
}

/// [`level_save_refusal`] with its agent reason: `prefab_in_place` ([`LEVEL_SAVE_IN_PLACE`]) or
/// `prefab_open` ([`LEVEL_SAVE_IN_PREFAB`]).
#[must_use]
pub fn level_save_refusal_reason(world: &World) -> Option<(&'static str, &'static str)> {
    let mode = world.get_resource::<PrefabEditMode>()?;
    if mode.in_context() {
        Some(("prefab_in_place", LEVEL_SAVE_IN_PLACE))
    } else if mode.path.is_some() {
        Some(("prefab_open", LEVEL_SAVE_IN_PREFAB))
    } else {
        None
    }
}

/// [`level_save_refusal`]'s message while a prefab is open in place.
pub const LEVEL_SAVE_IN_PLACE: &str =
    "a prefab is open in place: save or close the prefab first, then save the level";

/// [`level_save_refusal`]'s message while a prefab is open on its own (isolated).
pub const LEVEL_SAVE_IN_PREFAB: &str =
    "a prefab is open: save or close the prefab first, then save the level";

/// The stashed edit level: its canonical scene and its open state.
#[derive(Debug, Clone)]
struct StashedLevel {
    /// The snapshot taken when prefab mode was entered.
    scene: SceneFile,
    /// The open-level state to restore.
    open: OpenLevel,
    /// The level had unsaved edits when prefab mode was entered: it is dirty again after the
    /// restore, and the level autosave writes the stash meanwhile.
    dirty: bool,
}

impl PrefabEditMode {
    /// The prefab file being edited, when the mode is active.
    #[must_use]
    pub fn editing(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The prefab text the last autosave tick took, when the prefab had unsaved edits then.
    #[must_use]
    pub fn autosave_text(&self) -> Option<&str> {
        self.autosave_text.as_deref()
    }

    /// Whether the open prefab was opened in place ([`enter_prefab_edit_in_context`]).
    #[must_use]
    pub fn in_context(&self) -> bool {
        self.in_context.is_some()
    }

    /// The stashed level as a level (its id, open state and contents), when it had unsaved
    /// edits as prefab mode was entered: what the level autosave writes while the prefab is open.
    #[must_use]
    pub fn dirty_stashed_level(&self) -> Option<(OpenLevel, Level)> {
        self.stash.as_ref().filter(|stash| stash.dirty).map(|stash| {
            (
                stash.open.clone(),
                Level::from_scene_file(stash.open.id, stash.scene.clone()),
            )
        })
    }
}

/// Whether prefab edit mode is active.
#[must_use]
pub fn in_prefab_mode(world: &World) -> bool {
    world
        .get_resource::<PrefabEditMode>()
        .is_some_and(|mode| mode.path.is_some())
}

/// The open prefab's dirty flag (ADR-7382): it has edits a save has not written. Prefab mode
/// owns the undo history while it is open (it starts empty on entry and is dropped on exit), so
/// the history's saved mark is the prefab's: every edit pushes an entry, [`save_prefab`] marks the
/// state saved, undoing back to it is clean again, and a restored autosave starts dirty.
#[must_use]
pub fn is_dirty(world: &World) -> bool {
    in_prefab_mode(world) && world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty)
}

/// The snapshot filter that captures everything (the stash and the prefab save use it).
fn whole_scene_filter() -> regia_scene::spawn::SnapshotFilter {
    regia_scene::spawn::SnapshotFilter {
        level: None,
        resources: vec![regia_scene::level::LEVEL_SETTINGS_PATH.to_string()],
    }
}

/// Clears the world's scene content: a fresh empty level (like `level_io::new_level`), then every
/// entity a whole-scene snapshot writes and the level unload left behind (members of no level:
/// a prefab's contents, runtime spawns, persistent entities), except `keep`. The stash and the
/// prefab save snapshot the whole scene, so whatever survived the switch would show in the next
/// scene and be spawned a second time by the restore (the agent-parity finding: a closed
/// prefab's root in the next prefab, "duplicate entity guids" on reopening it).
fn clear_world(world: &mut World, keep: &[Entity]) {
    let open = level_io::new_level(world);
    world.insert_resource(open);
    // Content without a guid gets one first, so it is cleared with the rest.
    crate::visibility::ensure_snapshot_identity(world);
    for entity in crate::visibility::scene_content(world, keep) {
        // A parent despawned earlier took its children along.
        if let Ok(entry) = world.get_entity_mut(entity) {
            entry.despawn();
        }
    }
}

/// What prefab mode leaves where it is (out of the stash, the clear and the prefab's contents):
/// the editor documents that live in the edit world (the sequence players the Sequence Curves
/// panel opened) and the members of loaded levels other than the open one (the stash restores
/// into the open level only, so they would come back as its members).
fn kept_out(world: &mut World) -> Vec<Entity> {
    let mut keep = kept_out_of_context(world);
    let mut context = world.query_filtered::<Entity, With<PrefabContext>>();
    keep.extend(context.iter(world));
    keep
}

/// [`kept_out`] without the Edit in place context: what is neither the level nor the prefab.
fn kept_out_of_context(world: &mut World) -> Vec<Entity> {
    let mut keep = crate::seq_autosave::document_players(world);
    let open = world.get_resource::<OpenLevel>().map(|open| open.id);
    let mut members = world.query::<(Entity, &regia_scene::prelude::LevelMember)>();
    keep.extend(
        members
            .iter(world)
            .filter(|(_, member)| Some(member.level()) != open)
            .map(|(entity, _)| entity),
    );
    keep
}

/// Drops the records of `keep` from a whole-scene snapshot.
fn without_kept(world: &World, scene: &mut SceneFile, keep: &[Entity]) {
    let guids: Vec<Guid> = keep
        .iter()
        .filter_map(|entity| {
            world
                .get::<regia_scene::prelude::EntityGuid>(*entity)
                .map(|g| g.0)
        })
        .collect();
    if !guids.is_empty() {
        scene.entities.retain(|record| !guids.contains(&record.guid));
    }
}

/// Enters prefab edit mode: stash the open level, then load the prefab's contents as the
/// editable scene.
///
/// # Errors
///
/// When already editing, play-in-editor is running (the panels would show the play world, not
/// the isolated prefab scene), or the file cannot be read/parsed/spawned.
pub fn enter_prefab_edit(world: &mut World, path: &Path) -> Result<(), String> {
    enter_prefab_edit_with(world, path, None, None)
}

/// The level-authored instance root `entity` belongs to: itself when it is one, else the first
/// ancestor that carries [`PrefabInstance`] and is not prefab-internal.
fn authored_root_of(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    for _ in 0..256 {
        if world.get::<PrefabInstance>(cursor).is_some()
            && world.get::<regia_scene::prefab::PrefabLink>(cursor).is_none()
        {
            return Some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// `entity`'s world transform, composed from the local transforms of it and its ancestors (the
/// edit world does not run transform propagation between agent calls).
fn world_transform(world: &World, entity: Entity) -> regia_scene::prelude::Transform {
    let mut chain = Vec::new();
    let mut cursor = Some(entity);
    while let Some(at) = cursor {
        if chain.len() > 256 {
            break;
        }
        chain.push(at);
        cursor = world.get::<ChildOf>(at).map(ChildOf::parent);
    }
    let mut global = regia_scene::GlobalTransform::IDENTITY;
    for at in chain.into_iter().rev() {
        if let Some(local) = world.get::<regia_scene::prelude::Transform>(at) {
            global = global.mul_transform(local);
        }
    }
    global.to_transform()
}

/// Edit in place (addendum § 9.2): opens the prefab of the instance `entity` belongs to (the
/// instance root or any entity inside it) in prefab edit mode with the rest of the level drawn
/// around it as read-only [`PrefabContext`], the instance's world transform as the frame of the
/// prefab's contents and the instance itself hidden. Returns the prefab file.
///
/// # Errors
///
/// As [`enter_prefab_edit`]; also when `entity` is not inside a level-authored prefab instance,
/// no project is open, the prefab has no file in the project, or it is a variant (variant edit
/// mode is out of scope, spec 83 § 9).
pub fn enter_prefab_edit_in_context(world: &mut World, entity: Entity) -> Result<PathBuf, String> {
    let root = authored_root_of(world, entity)
        .ok_or_else(|| "the entity is not a prefab instance or inside one".to_string())?;
    let asset = world
        .get::<PrefabInstance>(root)
        .map(PrefabInstance::prefab_guid)
        .ok_or_else(|| "the instance root lost its prefab".to_string())?;
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| "no project is open".to_string())?;
    let path = crate::prefab_variant_edit::find_prefab_by_id(&project, asset)
        .ok_or_else(|| format!("prefab {asset} has no file in the project"))?;
    if crate::prefab_variant_edit::is_variant_path(&path) {
        return Err(format!(
            "{} is a prefab variant: variant edit mode is out of scope (spec 83 § 9); edit its base in place from an instance of the base, or change the variant's overrides",
            path.display()
        ));
    }
    let frame = world_transform(world, root);
    let instance = world
        .get::<regia_scene::prelude::EntityGuid>(root)
        .map(|guid| guid.0);
    enter_prefab_edit_with(world, &path, None, Some((frame, instance)))?;
    Ok(path)
}

/// Marks the open level's content as Edit in place context (everything a prefab switch would
/// clear, minus `keep` and the frame's subtree) and hides the instance opened in place.
fn mark_context(world: &mut World, keep: &[Entity], frame: Option<Entity>, instance: Option<Guid>) {
    let content = crate::visibility::scene_content(world, keep);
    for entity in content {
        if frame.is_some_and(|frame| entity == frame || is_under(world, entity, frame)) {
            continue;
        }
        let own_override = world.get::<regia_render::RenderOverride>(entity).is_some();
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert((
                PrefabContext,
                crate::visibility::EditorHidden,
                crate::viewport_ops::EditorLocked,
            ));
            // Drawn dimmed (P-160); an override of its own stays (the context is dropped on
            // close, the stash holds no override).
            if !own_override {
                entry.insert(context_dim());
            }
        }
    }
    mark_other_levels(world);
    if let Some(guid) = instance {
        let found = world
            .query::<(Entity, &regia_scene::prelude::EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(entity, _)| entity);
        if let Some(entity) = found {
            world
                .entity_mut(entity)
                .insert(regia_scene::prelude::Visibility::Hidden);
        }
    }
    // A selection of context entities would let the inspector edit them.
    if let Some(mut selection) = world.get_resource_mut::<crate::selection::Selection>() {
        *selection = crate::selection::Selection::default();
    }
}

/// Locks and dims every member of a loaded level other than the open one for an Edit in place
/// session ([`PrefabOtherLevel`]); a member already marked is left alone.
fn mark_other_levels(world: &mut World) {
    let open = world.get_resource::<OpenLevel>().map(|open| open.id);
    let others: Vec<Entity> = world
        .query_filtered::<(Entity, &regia_scene::prelude::LevelMember), Without<PrefabOtherLevel>>()
        .iter(world)
        .filter(|(_, member)| Some(member.level()) != open)
        .map(|(entity, _)| entity)
        .collect();
    for entity in others {
        let locked = world.get::<crate::viewport_ops::EditorLocked>(entity).is_none();
        let dimmed = world.get::<regia_render::RenderOverride>(entity).is_none();
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            if locked {
                entry.insert(crate::viewport_ops::EditorLocked);
            }
            if dimmed {
                entry.insert(context_dim());
            }
            entry.insert(PrefabOtherLevel { locked, dimmed });
        }
    }
}

/// Takes off what [`mark_other_levels`] added (the session closes).
fn unmark_other_levels(world: &mut World) {
    let marked: Vec<(Entity, PrefabOtherLevel)> = world
        .query::<(Entity, &PrefabOtherLevel)>()
        .iter(world)
        .map(|(entity, mark)| (entity, *mark))
        .collect();
    for (entity, mark) in marked {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.remove::<PrefabOtherLevel>();
            if mark.locked {
                entry.remove::<crate::viewport_ops::EditorLocked>();
            }
            if mark.dimmed {
                entry.remove::<regia_render::RenderOverride>();
            }
        }
    }
}

/// Whether `entity` hangs (at any depth) under `ancestor`.
fn is_under(world: &World, entity: Entity, ancestor: Entity) -> bool {
    let mut cursor = world.get::<ChildOf>(entity).map(ChildOf::parent);
    for _ in 0..4096 {
        let Some(at) = cursor else {
            return false;
        };
        if at == ancestor {
            return true;
        }
        cursor = world.get::<ChildOf>(at).map(ChildOf::parent);
    }
    false
}

/// After a save rebuilt the context's instances (they lost their markers), marks them again.
fn refresh_context(world: &mut World) {
    let Some(context) = world
        .get_resource::<PrefabEditMode>()
        .and_then(|mode| mode.in_context.clone())
    else {
        return;
    };
    let frame = crate::command::resolve_entity(world, context.frame);
    let keep = kept_out_of_context(world);
    mark_context(world, &keep, frame, context.instance);
}

/// [`enter_prefab_edit`] with `text` as the prefab's contents instead of the file's (a recovered
/// autosave, `crate::prefab_autosave`): the prefab opens on `path`, dirty until a save.
///
/// # Errors
///
/// As [`enter_prefab_edit`].
pub fn enter_prefab_edit_with_text(world: &mut World, path: &Path, text: &str) -> Result<(), String> {
    enter_prefab_edit_with(world, path, Some(text), None)
}

fn enter_prefab_edit_with(
    world: &mut World,
    path: &Path,
    recovered: Option<&str>,
    context: Option<(regia_scene::prelude::Transform, Option<Guid>)>,
) -> Result<(), String> {
    if world
        .get_resource::<PrefabEditMode>()
        .is_some_and(|mode| mode.path.is_some())
    {
        return Err("already editing a prefab — close it first".to_string());
    }
    if world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some())
    {
        return Err("stop play-in-editor first — the panels show the play world while it runs".to_string());
    }
    if crate::prefab_variant_edit::is_variant_path(path) {
        return Err(format!(
            "{} is a prefab variant: open its base, or change the variant's overrides",
            path.display()
        ));
    }
    let text = match recovered {
        Some(text) => text.to_string(),
        None => std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?,
    };
    let file = SceneFile::from_ron_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let asset = prefab_asset_id(path)?.0;

    // Stash the current level (contents + state) and clear the undo history: its entries
    // reference entity ids that will not exist after the restore. Content without a guid gets
    // one first, so it is stashed (and restored) instead of staying in the prefab's scene.
    crate::visibility::ensure_snapshot_identity(world);
    let keep = kept_out(world);
    let filter = whole_scene_filter();
    let mut scene = snapshot_scene(world, &filter).map_err(|e| format!("stash snapshot: {e}"))?;
    without_kept(world, &mut scene, &keep);
    let open = world
        .get_resource::<OpenLevel>()
        .cloned()
        .unwrap_or_else(|| level_io::new_level(world));
    let dirty = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
    world.insert_resource(PrefabEditMode {
        path: Some(path.to_path_buf()),
        asset: Some(asset),
        stash: Some(StashedLevel { scene, open, dirty }),
        toast: format!(
            "editing {}{} — Save & Close to return (undo history cleared)",
            path.display(),
            if context.is_some() { " in place" } else { "" }
        ),
        autosave_text: None,
        in_context: None,
    });
    let was_dirty = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
    crate::activity_panel::retain_commands(world);
    world.insert_resource(UndoStack::new());
    // Edit in place keeps the level in the world as read-only context; the isolated mode clears it.
    let frame = match &context {
        None => {
            clear_world(world, &keep);
            None
        }
        Some((transform, instance)) => {
            mark_context(world, &keep, None, *instance);
            // Not `EditorLocked`: the lock walks ancestors, and the prefab's contents hang here
            // (see `PrefabFrame`).
            let frame = world
                .spawn((
                    *transform,
                    PrefabContext,
                    PrefabFrame,
                    crate::visibility::EditorHidden,
                ))
                .id();
            if let Some(mut mode) = world.get_resource_mut::<PrefabEditMode>() {
                mode.in_context = Some(InContext {
                    frame: frame.to_bits(),
                    instance: *instance,
                });
            }
            // The level stays loaded as context, but a level save must not write it (the hidden
            // instance, the frame): `level_save_refusal` refuses every level save while the
            // session runs, and the path is cleared as a second guard. The stash keeps the path.
            if let Some(mut open) = world.get_resource_mut::<OpenLevel>() {
                open.path = None;
            }
            Some(frame)
        }
    };

    let opts = SpawnOptions {
        level: None,
        persistent: false,
        unknown_types: UnknownTypePolicyOption::Preserve,
    };
    if let Err(err) = spawn_scene(world, &file, &opts) {
        // A spawn that fails after the stash (a nested prefab chain too deep) must not leave
        // the editor in prefab mode on an empty world: the level comes back (ADR-7412).
        let restored = exit_prefab_edit(world, false);
        if was_dirty {
            // The restored level still has the edits the cleared history held.
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .mark_dirty();
        }
        return Err(match restored {
            Ok(()) => format!(
                "cannot spawn the prefab contents: {err} (the level is back; its undo history was cleared)"
            ),
            Err(restore) => {
                format!("cannot spawn the prefab contents: {err}; restoring the level failed: {restore}")
            }
        });
    }
    if let Some(frame) = frame {
        // The prefab's roots hang under the frame: they sit where the instance sits.
        let roots: Vec<Guid> = file
            .entities
            .iter()
            .filter(|record| record.parent.is_none())
            .map(|record| record.guid)
            .collect();
        let spawned: Vec<Entity> = world
            .query::<(Entity, &regia_scene::prelude::EntityGuid)>()
            .iter(world)
            .filter(|(entity, guid)| roots.contains(&guid.0) && world.get::<PrefabContext>(*entity).is_none())
            .map(|(entity, _)| entity)
            .collect();
        for root in spawned {
            world.entity_mut(root).insert(ChildOf(frame));
        }
    }
    // Level files store `regia_scene::Name`; `scene.query_entities` reads the ECS `Name`.
    crate::visibility::ensure_scene_identity(world);
    if recovered.is_some() {
        // Recovered edits are not in the file: dirty until a save.
        world.resource_mut::<UndoStack>().mark_dirty();
    }
    Ok(())
}

/// Saves the edited contents to the prefab file, re-bakes the template and re-instantiates every
/// live instance. Returns how many instances updated.
///
/// # Errors
///
/// When not in prefab mode, the contents are not a single-root scene, or IO fails. When the file
/// was written but the live instances cannot spawn from the new template (it nests itself), the
/// instances keep the previous template and the error says the file was saved.
pub fn save_prefab(world: &mut World) -> Result<usize, String> {
    let mode = world.get_resource::<PrefabEditMode>().cloned();
    let Some(mode) = mode.filter(|mode| mode.path.is_some()) else {
        return Err("not editing a prefab".to_string());
    };
    let Some(path) = mode.path.clone() else {
        return Err("no prefab path".to_string());
    };
    let Some(asset) = mode.asset else {
        return Err("no asset id".to_string());
    };

    let (scene, text) = prefab_contents(world)?;
    let roots = scene.entities.iter().filter(|e| e.parent.is_none()).count();
    if roots != 1 {
        return Err(format!(
            "a prefab needs exactly one root entity, the scene has {roots} — reparent or delete the extras"
        ));
    }
    // The editor's shared atomic write: the temp file never outlives a failed save (ADR-7412).
    crate::atomic_write::write_atomic(&path, text.as_bytes())?;
    // The file holds the edit: the prefab is clean until the next one.
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .mark_saved();

    // Re-bake under the file's stable asset id (same id the instances reference).
    let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
    let template = PrefabTemplate::from_scene_file(asset, &scene, &registry.read())
        .map_err(|e| format!("prefab bake: {e}"))?;
    let before = library_checkpoint(world);
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);

    // Re-instantiate every live instance of this prefab (instances update). When they cannot
    // spawn from the saved template, they keep the previous one and the save says so.
    let respawned = respawn_instances(world, &[asset], &before);
    // Edit in place: the rebuilt context instances are context again (pass or fail).
    refresh_context(world);
    let updated = respawned.map_err(|e| format!("saved {}, but {e}", path.display()))?;
    if let Some(mut mode) = world.get_resource_mut::<PrefabEditMode>() {
        mode.toast = format!("saved {} · {} instance(s) updated", path.display(), updated);
    }
    Ok(updated)
}

/// The edited contents as a prefab scene and its text, as [`save_prefab`] writes them (without
/// the one-root rule, which only a save enforces): every user entity gets its identity first, the
/// sequence documents stay out, and the resources map stays empty (resources are not part of a
/// template).
fn prefab_contents(world: &mut World) -> Result<(SceneFile, String), String> {
    crate::visibility::ensure_snapshot_identity(world);
    let keep = kept_out(world);
    let filter = whole_scene_filter();
    let mut scene = snapshot_scene(world, &filter).map_err(|e| format!("prefab snapshot: {e}"))?;
    without_kept(world, &mut scene, &keep);
    if world
        .get_resource::<PrefabEditMode>()
        .is_some_and(PrefabEditMode::in_context)
    {
        // Edit in place: the prefab's roots hang under the frame, which is not part of it.
        let guids: std::collections::HashSet<Guid> =
            scene.entities.iter().map(|record| record.guid).collect();
        for record in &mut scene.entities {
            if record.parent.is_some_and(|parent| !guids.contains(&parent)) {
                record.parent = None;
            }
        }
    }
    scene.resources.clear();
    scene
        .validate()
        .map_err(|e| format!("the edited contents are not a prefab: {e}"))?;
    let text = scene
        .to_ron_string()
        .map_err(|e| format!("prefab serialize: {e}"))?;
    Ok((scene, text))
}

/// The open prefab's members in the order a save writes them (parents before children as the
/// snapshot lists them); empty outside prefab mode or when the contents cannot be snapshot.
/// The prefab's logic actions use it (`crate::prefab_logic`, P-165).
pub fn prefab_members(world: &mut World) -> Vec<Entity> {
    if !in_prefab_mode(world) {
        return Vec::new();
    }
    let Ok((scene, _)) = prefab_contents(world) else {
        return Vec::new();
    };
    let mut guids = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
    let by_guid: std::collections::HashMap<Guid, Entity> = guids.iter(world).map(|(e, g)| (g.0, e)).collect();
    scene
        .entities
        .iter()
        .filter_map(|record| by_guid.get(&record.guid).copied())
        .collect()
}

/// The open prefab's root entity (the first member without a parent in the prefab, the one a
/// save writes as the root): where **New graph for this prefab** puts its `LogicGraphRef`.
pub fn prefab_root(world: &mut World) -> Option<Entity> {
    if !in_prefab_mode(world) {
        return None;
    }
    let (scene, _) = prefab_contents(world).ok()?;
    let guid = scene.entities.iter().find(|record| record.parent.is_none())?.guid;
    let mut guids = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
    guids.iter(world).find(|(_, g)| g.0 == guid).map(|(e, _)| e)
}

/// Takes the open prefab's autosave text when it has unsaved edits (the autosave tick's
/// preparation, `crate::prefab_autosave`; the snapshot needs the world mutably, the tick's
/// snapshot hooks only read it). A clean or closed prefab keeps no text.
pub(crate) fn prepare_autosave(world: &mut World) {
    let text = if is_dirty(world) {
        match prefab_contents(world) {
            Ok((_, text)) => Some(text),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "prefab autosave: {err}");
                None
            }
        }
    } else {
        None
    };
    if let Some(mut mode) = world.get_resource_mut::<PrefabEditMode>() {
        mode.autosave_text = text;
    }
}

/// Exits prefab edit mode: optionally saves first, then restores the stashed level.
///
/// # Errors
///
/// When not in prefab mode, or the save (requested) failed.
pub fn exit_prefab_edit(world: &mut World, save: bool) -> Result<(), String> {
    if save {
        let _ = save_prefab(world)?;
    }
    let mode = world.get_resource::<PrefabEditMode>().cloned();
    let Some(mode) = mode.filter(|mode| mode.stash.is_some()) else {
        return Err("not editing a prefab".to_string());
    };
    let Some(stash) = mode.stash else {
        return Err("not editing a prefab".to_string());
    };
    // The undo history does not survive the restore (entity ids change).
    crate::activity_panel::retain_commands(world);
    world.insert_resource(UndoStack::new());
    // Edit in place: the context (the level as it was drawn, and the frame with the prefab's
    // contents) goes; the stash brings the level back.
    let context: Vec<Entity> = world
        .query_filtered::<Entity, With<PrefabContext>>()
        .iter(world)
        .collect();
    for entity in context {
        if let Ok(entry) = world.get_entity_mut(entity) {
            entry.despawn();
        }
    }
    // The other loaded levels stay: only the session's lock and dim come off.
    unmark_other_levels(world);
    let keep = kept_out(world);
    clear_world(world, &keep);
    // Reopen the stashed level under its own id (Replace unloads the scratch level), so the
    // restored entities are level members and the next save writes them.
    let level = Level::from_scene_file(stash.open.id, stash.scene);
    world
        .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
        .insert(level);
    open_level(world, stash.open.id, LoadMode::Replace).map_err(|e| format!("restore level: {e}"))?;
    world.insert_resource(stash.open);
    if stash.dirty {
        // The level's unsaved edits are back, and so is its dirty flag (close prompt, autosave).
        world.resource_mut::<UndoStack>().mark_dirty();
    }
    world.insert_resource(PrefabEditMode::default());
    Ok(())
}

/// A `.prefab.ron` changed on disk (live reload): re-bake its template under the file's asset id
/// and re-instantiate every live instance (transforms, names and overrides kept). Returns how
/// many instances updated. A prefab open in prefab edit mode is left alone (the edits there win
/// until saved); a prefab with no `.meta` yet has no instances to update.
///
/// # Errors
///
/// The file cannot be read, parsed or baked, or the instances cannot spawn from it (a template
/// that nests itself); the old template and instances stay.
pub fn reload_prefab_file(world: &mut World, path: &Path) -> Result<usize, String> {
    let editing = world
        .get_resource::<PrefabEditMode>()
        .and_then(|mode| mode.path.clone())
        .is_some_and(|open| open == path);
    if editing || !regia_asset::meta_path_for(path).is_file() {
        return Ok(0);
    }
    // The editor's own Apply-override write (ADR-7203): the instances already match it.
    if let Some(echo) = world.get_resource::<crate::prefab_link::PrefabWriteEcho>()
        && std::fs::read_to_string(path).is_ok_and(|text| echo.is_echo(path, &text))
    {
        return Ok(0);
    }
    // A variant, or a base with variants built on it: register the file (and its chain) and update
    // the instances of every variant that depends on it as well (ADR-2323).
    let before = library_checkpoint(world);
    if crate::prefab_variant_edit::is_variant_path(path) {
        let asset =
            crate::prefab_variant_edit::register_prefab_file(world, path).map_err(|e| e.to_string())?;
        return crate::prefab_variant_edit::respawn_with_dependents(world, asset, &before);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let file = regia_scene::prelude::SceneFile::from_ron_str(&text).map_err(|e| format!("{e}"))?;
    let asset = prefab_asset_id(path)?.0;
    let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read())
        .map_err(|e| format!("prefab bake: {e}"))?;
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);
    crate::prefab_variant_edit::respawn_with_dependents(world, asset, &before)
}

/// The prefab library as it is now (an empty one when there is none): what a template change
/// passes to [`respawn_instances`] so a failed instance update can put it back.
pub(crate) fn library_checkpoint(world: &World) -> PrefabLibrary {
    world.get_resource::<PrefabLibrary>().cloned().unwrap_or_default()
}

/// Despawns every live instance of any of `assets` and re-instantiates it from the current
/// templates. Returns how many level-authored instance roots were rebuilt; a root that holds
/// instances of several of the assets (a base and its variant) is rebuilt and counted once.
///
/// Each level-authored instance is snapshotted with its whole subtree the way a level save
/// writes it (guid, transform, name, every component, overrides, added and removed components,
/// the entities the level added on top of it, plain children), despawned and spawned again from
/// that snapshot, which instantiates the prefab from the library's current template. Level
/// membership, `Persistent`, the parent and the place among the siblings come back through the
/// undo stack's burial record, which also maps the old entity ids to the new ones so undo
/// entries keyed by id still find them (ADR-7412). An instance nested inside another prefab's
/// template is rebuilt by respawning the level-authored instance that contains it.
///
/// `before` is the library as it was before the template change that asks for this update
/// ([`library_checkpoint`]). When the instances cannot spawn from the new templates (a template
/// that nests itself, directly or through another prefab), the library goes back to `before`,
/// the instances are spawned again from their snapshot with the templates they had, the groups
/// already rebuilt are rebuilt again from those, and the spawn error is returned: no instance is
/// lost (rc2 review, editor-undo finding 1).
///
/// # Errors
///
/// The instances could not spawn from the new templates; the old ones are back in the library.
pub(crate) fn respawn_instances(
    world: &mut World,
    assets: &[Guid],
    before: &PrefabLibrary,
) -> Result<usize, String> {
    let groups = instance_groups(world, assets);
    let mut updated = 0;
    let mut rebuilt: Vec<(Option<LevelId>, Vec<u64>)> = Vec::new();
    for (level, bits) in groups {
        match respawn_group(world, level, &bits) {
            GroupRespawn::Skipped => {}
            GroupRespawn::Rebuilt(count) => {
                updated += count;
                rebuilt.push((level, bits));
            }
            GroupRespawn::Failed { file, error } => {
                world.insert_resource(before.clone());
                if let Err(again) = crate::command::try_spawn_and_exhume(world, &file, level) {
                    tracing::error!(target: "regia_editor",
                        "instance update: the instances could not be restored either: {again}");
                }
                for (level, bits) in rebuilt {
                    if let GroupRespawn::Failed { error, .. } = respawn_group(world, level, &bits) {
                        tracing::error!(target: "regia_editor",
                            "instance update: an updated group could not go back to the old template: {error}");
                    }
                }
                return Err(format!(
                    "the instances could not spawn from the new template ({error}); they keep the previous one"
                ));
            }
        }
    }
    Ok(updated)
}

/// What [`respawn_group`] did.
enum GroupRespawn {
    /// No instance of the group is alive any more, or none could be snapshotted.
    Skipped,
    /// This many instance roots were rebuilt.
    Rebuilt(usize),
    /// The spawn failed after the group was despawned; `file` is its snapshot.
    Failed {
        /// The group's snapshot, to spawn it back.
        file: regia_scene::prelude::SceneFile,
        /// The spawn error.
        error: String,
    },
}

/// The level-authored instance roots that hold an instance of any of `assets`, grouped by level
/// (each group spawns its new prefab-internal entities into its level), as entity bits.
fn instance_groups(world: &mut World, assets: &[Guid]) -> Vec<(Option<LevelId>, Vec<u64>)> {
    let wanted: std::collections::HashSet<Guid> = assets.iter().copied().collect();
    let found: Vec<Entity> = {
        let mut query = world.query::<(Entity, &PrefabInstance)>();
        query
            .iter(world)
            .filter(|(_, instance)| wanted.contains(&instance.prefab_guid()))
            .map(|(entity, _)| entity)
            .collect()
    };
    let mut seen: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    let mut groups: Vec<(Option<LevelId>, Vec<u64>)> = Vec::new();
    for entity in found {
        let Some(root) = authored_instance_root(world, entity) else {
            continue;
        };
        if !seen.insert(root) {
            continue;
        }
        let level = world
            .get::<regia_scene::prelude::LevelMember>(root)
            .map(|member| member.level());
        match groups.iter_mut().find(|(l, _)| *l == level) {
            Some((_, members)) => members.push(root.to_bits()),
            None => groups.push((level, vec![root.to_bits()])),
        }
    }
    groups
}

/// Snapshots, despawns and respawns one level's instance roots (recorded as `bits`).
fn respawn_group(world: &mut World, level: Option<LevelId>, bits: &[u64]) -> GroupRespawn {
    // An earlier group's respawn may have rebuilt (and renumbered) one of these.
    let live: Vec<Entity> = bits
        .iter()
        .filter_map(|b| crate::command::resolve_entity(world, *b))
        .collect();
    if live.is_empty() {
        return GroupRespawn::Skipped;
    }
    let file = crate::command::subtree_snapshot(world, bits);
    if file.entities.is_empty() {
        tracing::error!(target: "regia_editor", "instance update: the instances could not be snapshotted");
        return GroupRespawn::Skipped;
    }
    crate::command::bury_and_despawn(world, &live, "instance update");
    match crate::command::try_spawn_and_exhume(world, &file, level) {
        Ok(()) => GroupRespawn::Rebuilt(
            bits.iter()
                .filter(|b| crate::command::resolve_entity(world, **b).is_some())
                .count(),
        ),
        Err(error) => {
            tracing::error!(target: "regia_editor", "instance update failed: {error}");
            GroupRespawn::Failed {
                file,
                error: error.to_string(),
            }
        }
    }
}

/// The level-authored instance root that owns `entity`: `entity` itself when it is not
/// prefab-internal, else the first ancestor without a `PrefabLink` (`None` when that is not an
/// instance, which only a malformed hierarchy gives).
fn authored_instance_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    let mut steps = 0usize;
    while world.get::<regia_scene::prefab::PrefabLink>(cursor).is_some() {
        cursor = world.get::<ChildOf>(cursor)?.parent();
        steps += 1;
        if steps > 10_000 {
            return None; // a parent cycle
        }
    }
    world.get::<PrefabInstance>(cursor).is_some().then_some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::EditorCommand as _;
    use regia_app::prelude::App;
    use regia_scene::prelude::{Name, Transform};

    fn temp_project(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
        dir
    }

    fn app_with_level(dir: &Path) -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.to_path_buf()),
        });
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.insert_resource(UndoStack::new());
        app
    }

    /// A prefab file: one root named `stem` with a child.
    fn write_prefab(dir: &Path, stem: &str) -> PathBuf {
        let path = dir.join("assets/props").join(format!("{stem}.prefab.ron"));
        let text = r#"( format_version: 1, entities: [
            ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
              components: { "regia_scene::transform::Transform": () } ),
            ( guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Child", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
              components: { "regia_scene::transform::Transform": ( translation: (0.0, 1.0, 0.0) ) } ),
        ] )"#;
        std::fs::write(&path, text).expect("prefab");
        path
    }

    fn names(world: &mut World) -> Vec<String> {
        let mut query = world.query::<(Entity, &Name)>();
        let mut found: Vec<String> = query
            .iter(world)
            .map(|(_, name)| name.as_str().to_string())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn open_edit_save_updates_instances_and_close_restores() {
        let dir = temp_project("prefab-edit");
        let mut app = app_with_level(&dir);

        // Seed the edit level with content the mode switch must preserve.
        let open_before = app.world().resource::<OpenLevel>().clone();
        let mut seed = crate::command::SpawnEntities::new_entity(
            app.world(),
            Guid::new_random(),
            "LevelRock",
            Transform::default(),
        )
        .expect("spawn");
        seed.apply(app.world_mut());
        let level_text_snapshot = {
            let filter = whole_scene_filter();
            let file = snapshot_scene(app.world_mut(), &filter).expect("snap");
            file.to_ron_string().expect("ron")
        };

        // Create + spawn a prefab instance.
        let path = write_prefab(&dir, "crate");
        let _instance_root = crate::content::spawn_prefab_asset(
            app.world_mut(),
            &path,
            regia_core::math::Vec3::new(3.0, 0.0, 1.0),
        )
        .expect("instance");

        // Enter prefab edit mode: the level content is stashed, the prefab contents are editable.
        app.world_mut().insert_resource(PrefabEditMode::default());
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        let mode_names = names(app.world_mut());
        assert!(
            mode_names.contains(&"Root".to_string()) && mode_names.contains(&"Child".to_string()),
            "{mode_names:?}"
        );
        assert!(
            !mode_names.contains(&"LevelRock".to_string()),
            "the level is stashed: {mode_names:?}"
        );
        assert!(app.world().resource::<PrefabEditMode>().editing().is_some());

        // Edit: move the child entity through the world (the inspector/gizmo edit path).
        // Template bakes keep component values but drop names (identity is the local guid), so
        // the observable edit for instance updates is the child's transform.
        let child_guid = Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_0002u128);
        let child = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &regia_scene::prefab::PrefabLink)>();
            // In prefab edit mode the contents are plain entities (no PrefabLink); find by guid.
            let mut by_guid = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
            by_guid
                .iter(world)
                .find(|(_, guid)| guid.0 == child_guid)
                .map(|(entity, _)| entity)
                .or_else(|| {
                    query
                        .iter(world)
                        .find(|(_, link)| link.guid() == child_guid)
                        .map(|(entity, _)| entity)
                })
                .expect("child entity")
        };
        app.world_mut()
            .get_entity_mut(child)
            .expect("child")
            .insert(Transform {
                translation: regia_core::math::Vec3::new(0.0, 2.5, 0.0),
                ..Transform::default()
            });

        // Save: the file updates; no live instances exist in the isolated prefab scene.
        let updated = save_prefab(app.world_mut()).expect("save");
        assert_eq!(updated, 0, "no live instances in the isolated prefab scene");
        let saved = std::fs::read_to_string(&path).expect("reread");
        assert!(saved.contains("2.5"), "the file kept the edit: {saved}");

        // Close (without saving again): the stashed level comes back, and its instances were
        // re-baked from the saved template — the instance respawns with the new content.
        exit_prefab_edit(app.world_mut(), false).expect("exit");
        let restored = names(app.world_mut());
        assert!(
            restored.contains(&"LevelRock".to_string()),
            "the level content returned: {restored:?}"
        );
        assert!(
            restored.contains(&"crate".to_string()),
            "the instance wrapper is back: {restored:?}"
        );
        let instance_entity = entity_of(app.world_mut(), "crate");
        assert!(
            app.world().get::<Transform>(instance_entity).is_some(),
            "the instance kept its transform"
        );
        assert!(app.world().resource::<PrefabEditMode>().editing().is_none());

        // The instance's subtree re-instantiated from the SAVED template: the child (by its
        // local guid) carries the edited translation.
        let child_translation = {
            let world = app.world_mut();
            let mut links = world.query::<(Entity, &regia_scene::prefab::PrefabLink)>();
            links
                .iter(world)
                .find(|(_, link)| link.guid() == child_guid)
                .and_then(|(entity, _)| world.get::<Transform>(entity))
                .map(|transform| transform.translation)
        };
        assert_eq!(
            child_translation,
            Some(regia_core::math::Vec3::new(0.0, 2.5, 0.0)),
            "the live instance updated from the saved template"
        );
        assert_eq!(
            app.world().resource::<OpenLevel>().id,
            open_before.id,
            "the level id returned"
        );
        assert!(level_text_snapshot.contains("LevelRock"));
        // The restored level is the manager's current one and a later save keeps its members.
        assert_eq!(
            app.world()
                .resource::<regia_scene::prelude::LevelManager>()
                .current(),
            Some(open_before.id)
        );
        let saved_level = regia_scene::level::snapshot_level(app.world_mut(), open_before.id)
            .expect("snapshot")
            .to_ron_string()
            .expect("ron");
        assert!(saved_level.contains("LevelRock"), "{saved_level}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entity_of(world: &mut World, name: &str) -> Entity {
        let mut query = world.query::<(Entity, &Name)>();
        query
            .iter(world)
            .find(|(_, n)| n.as_str() == name)
            .map(|(entity, _)| entity)
            .expect("named entity")
    }

    /// rc1 editor-undo finding 9 (ADR-7412): the prefab save's hand-rolled `.new.tmp` + rename
    /// left the temp file behind when the rename failed; the shared atomic write removes it.
    #[test]
    fn a_failed_prefab_save_leaves_no_temp_file() {
        let dir = temp_project("prefab-save-fail");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        // The file became a non-empty directory: the rename over it fails on every platform.
        std::fs::remove_file(&path).expect("remove the prefab file");
        std::fs::create_dir_all(path.join("inside")).expect("blocking dir");
        let folder = dir.join("assets/props");
        let listing = |folder: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(folder)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        };
        let before = listing(&folder);
        save_prefab(app.world_mut()).expect_err("a directory cannot be replaced");
        assert_eq!(
            listing(&folder),
            before,
            "no temp file is left next to the prefab"
        );
        assert_eq!(
            app.world().resource::<PrefabEditMode>().editing(),
            Some(path.as_path()),
            "the failed save keeps the edit open"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_refuses_multi_root_scenes_and_errors_are_clean() {
        let dir = temp_project("prefab-guard");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");

        // Not in prefab mode: refuse.
        assert!(save_prefab(app.world_mut()).is_err());
        assert!(exit_prefab_edit(app.world_mut(), false).is_err());

        app.world_mut().insert_resource(PrefabEditMode::default());
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        // A second root breaks the prefab shape.
        let mut extra = crate::command::SpawnEntities::new_entity(
            app.world_mut(),
            Guid::new_random(),
            "ExtraRoot",
            Transform::default(),
        )
        .expect("spawn");
        extra.apply(app.world_mut());
        let err = save_prefab(app.world_mut()).expect_err("multi-root refused");
        assert!(err.contains("exactly one root"), "{err}");
        assert!(err.contains("2"), "{err}");

        // Close without saving discards the edit.
        exit_prefab_edit(app.world_mut(), false).expect("exit");
        let after = std::fs::read_to_string(&path).expect("file");
        assert!(!after.contains("ExtraRoot"), "nothing was written: {after}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_failures_leave_the_level_and_history_alone() {
        let dir = temp_project("prefab-enter-fail");
        let mut app = app_with_level(&dir);
        let before = names(app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().mark_dirty();

        let err = enter_prefab_edit(app.world_mut(), &dir.join("assets/props/ghost.prefab.ron"))
            .expect_err("missing file");
        assert!(err.contains("cannot read"), "{err}");
        let bad = dir.join("assets/props/bad.prefab.ron");
        std::fs::write(&bad, "this is not ron").expect("bad file");
        let err = enter_prefab_edit(app.world_mut(), &bad).expect_err("unparseable file");
        assert!(err.contains("cannot parse"), "{err}");

        assert!(
            app.world()
                .get_resource::<PrefabEditMode>()
                .is_none_or(|mode| mode.editing().is_none()),
            "a failed open never enters prefab mode"
        );
        assert_eq!(names(app.world_mut()), before, "the level is untouched");
        assert!(
            app.world().resource::<UndoStack>().is_dirty(),
            "the history is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rc1 editor-undo finding 11 (ADR-7412): prefab contents that fail to spawn after the level
    /// was stashed left the editor in prefab mode on an empty world. The level comes back, and
    /// unsaved level edits stay marked dirty.
    #[test]
    fn an_unspawnable_prefab_gives_the_level_back() {
        let dir = temp_project("prefab-enter-unspawnable");
        let mut app = app_with_level(&dir);
        let mut seed = crate::command::SpawnEntities::new_entity(
            app.world(),
            Guid::new_random(),
            "LevelRock",
            Transform::default(),
        )
        .expect("spawn");
        seed.apply(app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().mark_dirty();
        let before = names(app.world_mut());
        // Two templates that instantiate each other: the file parses and validates, but its
        // instance of `A` cannot spawn (the nesting never ends), which is only found after the
        // level was stashed.
        let a = "8b6f2a11-0000-4000-8000-0000000000a1";
        let b = "8b6f2a11-0000-4000-8000-0000000000b1";
        let nesting = |inner: &str| {
            SceneFile::from_ron_str(&format!(
                r#"( format_version: 1, entities: [
                    ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
                      prefab_instance: Some(( prefab: "asset:{inner}", overrides: [], added_components: [],
                        removed_components: [], added_children: [], removed_children: [] )) ),
                ] )"#
            ))
            .expect("template RON")
        };
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .clone();
        for (asset, inner) in [(a, b), (b, a)] {
            let asset: Guid = asset.parse().expect("guid");
            let template =
                PrefabTemplate::from_scene_file(asset, &nesting(inner), &registry.read()).expect("bake");
            app.world_mut()
                .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
                .insert(template);
        }
        let path = dir.join("assets/props/broken.prefab.ron");
        let text = nesting(a).to_ron_string().expect("ron");
        std::fs::write(&path, text).expect("prefab");
        let err = enter_prefab_edit(app.world_mut(), &path).expect_err("unspawnable prefab");
        assert!(err.contains("cannot spawn"), "{err}");
        assert!(
            app.world()
                .get_resource::<PrefabEditMode>()
                .is_none_or(|mode| mode.editing().is_none()),
            "a failed open never enters prefab mode"
        );
        assert_eq!(names(app.world_mut()), before, "the level is untouched");
        assert!(
            app.world().resource::<UndoStack>().is_dirty(),
            "the unsaved level edits stay marked dirty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The child translation of every live instance of the `write_prefab` template, by the
    /// child's local guid.
    fn child_translations(world: &mut World) -> Vec<regia_core::math::Vec3> {
        let child_guid = Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_0002u128);
        let mut links = world.query::<(Entity, &regia_scene::prefab::PrefabLink)>();
        let found: Vec<Entity> = links
            .iter(world)
            .filter(|(_, link)| link.guid() == child_guid)
            .map(|(entity, _)| entity)
            .collect();
        found
            .into_iter()
            .filter_map(|entity| world.get::<Transform>(entity).map(|t| t.translation))
            .collect()
    }

    /// rc2 review (rev-zai-rc-b editor-undo finding 1): a live reload whose template can no
    /// longer spawn (it nests itself) despawned every instance of the asset and then failed to
    /// spawn them again, so the level lost them, with the entities the level added under them,
    /// and the next save wrote the loss. The reload now fails, the old template stays, and the
    /// instances (with their level-added children) come back as they were.
    #[test]
    fn a_reload_to_a_self_nesting_template_keeps_the_instances() {
        let dir = temp_project("prefab-reload-cycle");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        let instance = crate::content::spawn_prefab_asset(
            app.world_mut(),
            &path,
            regia_core::math::Vec3::new(3.0, 0.0, 1.0),
        )
        .expect("instance");
        // A child the level added on top of the instance (saved in its `added_children`).
        let mut added = crate::command::SpawnEntities::new_entity(
            app.world(),
            Guid::new_random(),
            "LevelAddedLamp",
            Transform::default(),
        )
        .expect("spawn");
        added.apply(app.world_mut());
        let lamp = entity_of(app.world_mut(), "LevelAddedLamp");
        app.world_mut().entity_mut(instance).add_child(lamp);
        let instance_guid = app
            .world()
            .get::<regia_scene::prelude::EntityGuid>(instance)
            .expect("guid")
            .0;
        let before = names(app.world_mut());
        assert!(before.contains(&"crate".to_string()), "{before:?}");
        assert_eq!(
            child_translations(app.world_mut()),
            vec![regia_core::math::Vec3::new(0.0, 1.0, 0.0)]
        );

        // The file now nests an instance of itself under its root (an outside edit).
        let asset = prefab_asset_id(&path).expect("asset id").0;
        let text = format!(
            r#"( format_version: 1, entities: [
            ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
              components: {{ "regia_scene::transform::Transform": () }} ),
            ( guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Child", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
              components: {{ "regia_scene::transform::Transform": ( translation: (0.0, 7.0, 0.0) ) }} ),
            ( guid: "8b6f2a11-0000-4000-8000-000000000003", name: "Self", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
              prefab_instance: Some(( prefab: "asset:{asset}", overrides: [], added_components: [],
                removed_components: [], added_children: [], removed_children: [] )) ),
        ] )"#
        );
        std::fs::write(&path, text).expect("rewrite the prefab");

        let err =
            reload_prefab_file(app.world_mut(), &path).expect_err("a self-nesting template cannot spawn");
        assert!(err.contains("instance"), "{err}");
        let after = names(app.world_mut());
        assert_eq!(
            after, before,
            "every instance and its added children are still in the level"
        );
        assert_eq!(
            child_translations(app.world_mut()),
            vec![regia_core::math::Vec3::new(0.0, 1.0, 0.0)],
            "the instance still uses the old template"
        );
        let restored = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
            query
                .iter(world)
                .find(|(_, guid)| guid.0 == instance_guid)
                .map(|(entity, _)| entity)
                .expect("the instance root is back under its guid")
        };
        assert!(app.world().get::<PrefabInstance>(restored).is_some());
        let level = app.world().resource::<OpenLevel>().id;
        let saved = regia_scene::level::snapshot_level(app.world_mut(), level)
            .expect("snapshot")
            .to_ron_string()
            .expect("ron");
        assert!(
            saved.contains("crate") && saved.contains("LevelAddedLamp"),
            "a save after the failed reload keeps the instance: {saved}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_open_is_refused_while_editing() {
        let dir = temp_project("prefab-reenter");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        let editing = names(app.world_mut());
        let err = enter_prefab_edit(app.world_mut(), &path).expect_err("already editing");
        assert!(err.contains("already editing"), "{err}");
        assert_eq!(names(app.world_mut()), editing, "the open prefab is untouched");
        assert_eq!(
            app.world().resource::<PrefabEditMode>().editing(),
            Some(path.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_save_and_close_stays_in_prefab_mode() {
        let dir = temp_project("prefab-save-close");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        let mut extra = crate::command::SpawnEntities::new_entity(
            app.world_mut(),
            Guid::new_random(),
            "ExtraRoot",
            Transform::default(),
        )
        .expect("spawn");
        extra.apply(app.world_mut());

        let err = exit_prefab_edit(app.world_mut(), true).expect_err("save refused");
        assert!(err.contains("exactly one root"), "{err}");
        assert_eq!(
            app.world().resource::<PrefabEditMode>().editing(),
            Some(path.as_path()),
            "the user keeps the edit instead of losing it"
        );
        assert!(
            names(app.world_mut()).iter().any(|name| name == "ExtraRoot"),
            "the unsaved edit is still in the world"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 14 § A3: a plain instance (no edit mode, no variant) keeps its overrides when its
    /// prefab file changes on disk; an instance without overrides follows the file.
    #[test]
    fn a_plain_instance_keeps_its_overrides_through_a_live_prefab_reload() {
        let dir = temp_project("prefab-reload-overrides");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        let edited = crate::content::spawn_prefab_asset(app.world_mut(), &path, regia_core::math::Vec3::ZERO)
            .expect("edited instance");
        let plain = crate::content::spawn_prefab_asset(app.world_mut(), &path, regia_core::math::Vec3::X)
            .expect("plain instance");
        let child_guid = Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_0002u128);
        // The child under `root`: walk up from each linked entity with the child's local id.
        fn child_under(world: &mut World, root: Entity, child_guid: Guid) -> Entity {
            let mut links = world.query::<(Entity, &regia_scene::prefab::PrefabLink)>();
            let candidates: Vec<Entity> = links
                .iter(world)
                .filter(|(_, link)| link.guid() == child_guid)
                .map(|(entity, _)| entity)
                .collect();
            candidates
                .into_iter()
                .find(|entity| {
                    let mut cursor = Some(*entity);
                    while let Some(at) = cursor {
                        if at == root {
                            return true;
                        }
                        cursor = world.get::<ChildOf>(at).map(ChildOf::parent);
                    }
                    false
                })
                .expect("child of the instance")
        }
        let child_y = |world: &mut World, root: Entity| -> f32 {
            let child = child_under(world, root, child_guid);
            world.get::<Transform>(child).expect("transform").translation.y
        };
        let child = child_under(app.world_mut(), edited, child_guid);
        crate::inspector::edit_value(
            app.world_mut(),
            &[child],
            "Transform",
            "translation",
            Box::new(regia_core::math::Vec3::new(0.0, 4.0, 0.0)),
        )
        .expect("override");
        assert_eq!(
            app.world()
                .get::<PrefabInstance>(edited)
                .expect("instance")
                .overrides
                .len(),
            1
        );

        // Someone edits the prefab file: the child moves to y = 2.
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("(0.0, 1.0, 0.0)", "(0.0, 2.0, 0.0)")).expect("edit file");
        let updated = reload_prefab_file(app.world_mut(), &path).expect("reload");
        assert_eq!(updated, 2, "both instances were rebuilt");

        let world = app.world_mut();
        let mut roots = world.query::<(Entity, &PrefabInstance)>();
        let roots: Vec<(Entity, usize)> = roots
            .iter(world)
            .map(|(entity, instance)| (entity, instance.overrides.len()))
            .collect();
        assert_eq!(roots.len(), 2, "{roots:?}");
        let with_override = roots.iter().find(|(_, n)| *n == 1).expect("override kept").0;
        let without = roots.iter().find(|(_, n)| *n == 0).expect("plain one").0;
        assert_eq!(
            child_y(world, with_override),
            4.0,
            "the override survives the reload"
        );
        assert_eq!(
            child_y(world, without),
            2.0,
            "the plain instance follows the file"
        );
        let _ = plain;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entering_and_leaving_clear_the_undo_history() {
        let dir = temp_project("prefab-history");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        let mut spawn = crate::command::SpawnEntities::new_entity(
            app.world_mut(),
            Guid::new_random(),
            "LevelThing",
            Transform::default(),
        )
        .expect("spawn");
        spawn.apply(app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().push(Box::new(spawn));
        assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 1);

        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        assert_eq!(
            app.world().resource::<UndoStack>().undo_depth(),
            0,
            "level history references entities that are gone"
        );
        assert!(!names(app.world_mut()).iter().any(|name| name == "LevelThing"));

        exit_prefab_edit(app.world_mut(), false).expect("exit");
        assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 0);
        assert!(
            names(app.world_mut()).iter().any(|name| name == "LevelThing"),
            "the stashed level is back"
        );
        assert!(app.world().resource::<PrefabEditMode>().editing().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Runs one editor agent method through its installed handler.
    fn call(world: &mut World, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .unwrap_or_else(|| panic!("{method} is installed"));
        handler(world, &params).map_err(|e| format!("{method}: {}", e.message))
    }

    /// The agent-parity finding (w6 item 3): in one editor world, create prefab A, open it, save,
    /// close without saving, then create and open prefab B. B's scene holds only B's root;
    /// closing B and reopening A work.
    #[test]
    fn a_closed_prefab_leaves_nothing_in_the_next_prefab_session() {
        use serde_json::json;
        let dir = temp_project("prefab-leak");
        let mut app = app_with_level(&dir);
        app.world_mut().insert_resource(PrefabEditMode::default());
        let world = app.world_mut();
        let level = names(world);

        call(
            world,
            "editor.create_asset",
            json!({ "kind": "prefab", "path": "assets/props/a.prefab.ron" }),
        )
        .expect("create A");
        call(
            world,
            "editor.open_prefab",
            json!({ "path": "assets/props/a.prefab.ron" }),
        )
        .expect("open A");
        let in_a = names(world);
        assert_eq!(in_a.len(), 1, "A is one root: {in_a:?}");
        call(world, "editor.save_prefab", json!({})).expect("save A");
        call(world, "editor.close_prefab", json!({ "save": false })).expect("close A");
        assert_eq!(
            names(world),
            level,
            "closing A gives back the level, nothing of A"
        );

        call(
            world,
            "editor.create_asset",
            json!({ "kind": "prefab", "path": "assets/props/b.prefab.ron" }),
        )
        .expect("create B");
        call(
            world,
            "editor.open_prefab",
            json!({ "path": "assets/props/b.prefab.ron" }),
        )
        .expect("open B");
        let in_b = names(world);
        assert_eq!(
            in_b.len(),
            1,
            "B's scene is B's root only: {in_b:?} (A was {in_a:?})"
        );
        call(world, "editor.close_prefab", json!({})).expect("close B with a save");
        assert_eq!(names(world), level);

        call(
            world,
            "editor.open_prefab",
            json!({ "path": "assets/props/a.prefab.ron" }),
        )
        .expect("reopen A");
        assert_eq!(names(world), in_a);
        call(world, "editor.close_prefab", json!({ "save": false })).expect("close A again");
        assert_eq!(names(world), level);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same leak through the stash: an entity with identity but outside the open level (a
    /// runtime spawn a frame tagged) is stashed on entry, so the clear must take it too, or the
    /// restore on exit spawns it a second time (duplicate guids on the next save).
    #[test]
    fn a_non_member_entity_is_not_duplicated_by_a_prefab_session() {
        let dir = temp_project("prefab-non-member");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        let world = app.world_mut();
        world.spawn((
            Name::new("Loose"),
            Transform::default(),
            regia_scene::prelude::EntityGuid(Guid::new_random()),
        ));
        enter_prefab_edit(world, &path).expect("enter");
        let editing = names(world);
        assert!(
            !editing.iter().any(|n| n == "Loose"),
            "stashed, not shown: {editing:?}"
        );
        exit_prefab_edit(world, false).expect("exit");
        let back = names(world);
        assert_eq!(
            back.iter().filter(|n| *n == "Loose").count(),
            1,
            "the loose entity comes back once: {back:?}"
        );
        let filter = whole_scene_filter();
        snapshot_scene(world, &filter)
            .expect("snapshot")
            .validate()
            .expect("no duplicate guids");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The palette's level switches must not unload the prefab session's scene: the File menu
    /// hides them in prefab mode, so the commands have to refuse too (review fixrev8 finding 1).
    #[test]
    fn palette_level_switches_are_refused_while_editing_a_prefab() {
        let dir = temp_project("prefab-palette-guard");
        let mut app = app_with_level(&dir);
        let path = write_prefab(&dir, "crate");
        app.world_mut().insert_resource(PrefabEditMode::default());
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        let level = app.world().resource::<OpenLevel>().id;
        let before = names(app.world_mut());

        for id in ["file.new_level", "file.new_level_discard"] {
            let msg = crate::command_palette::run(app.world_mut(), id).expect("palette command");
            assert!(msg.contains("prefab"), "{id}: {msg}");
            assert_eq!(
                app.world().resource::<OpenLevel>().id,
                level,
                "{id} is refused: the same level stays open"
            );
        }
        assert_eq!(names(app.world_mut()), before, "the prefab scene is untouched");

        // Restoring an autosave would replace the level under the prefab session.
        let msg = crate::command_palette::run(app.world_mut(), "recovery.restore").expect("palette command");
        assert!(msg.contains("prefab"), "{msg}");
        assert!(
            app.world()
                .get_resource::<crate::editor_layout::LayoutRequests>()
                .is_none_or(|requests| {
                    !requests
                        .pending
                        .contains(&crate::editor_layout::LayoutRequest::RestoreRecovery)
                }),
            "no restore is queued"
        );
        // Discarding the autosave never touches the scene: still allowed.
        let msg = crate::command_palette::run(app.world_mut(), "recovery.discard").expect("palette command");
        assert_eq!(msg, "Discarding the offered autosave");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P-160 (4): while a prefab is open on its own (isolated), every level save is refused like
    /// in place (File ▸ Save, `editor.save_level` with reason `prefab_open`, an open level's own
    /// Save): the scratch level never reaches a level file. After the close the level saves.
    #[test]
    fn an_isolated_prefab_edit_refuses_a_level_save() {
        let dir = temp_project("prefab-isolated-save");
        let mut app = app_with_level(&dir);
        let level = dir.join("assets/l.level.ron");
        let open = app.world().resource::<OpenLevel>().clone();
        level_io::save_level_and_terrain(app.world_mut(), &open, &level).expect("first save");
        let mut open = open;
        open.path = Some(level.clone());
        app.world_mut().insert_resource(open);
        let before = std::fs::read(&level).expect("level");
        let path = write_prefab(&dir, "crate");
        enter_prefab_edit(app.world_mut(), &path).expect("enter");
        assert!(!app.world().resource::<PrefabEditMode>().in_context(), "isolated");

        let open_now = app.world().resource::<OpenLevel>().clone();
        let err = level_io::save_level_and_terrain(app.world_mut(), &open_now, &level).expect_err("refused");
        assert_eq!(err, LEVEL_SAVE_IN_PREFAB);
        let err = crate::agent_ext::editor_save_level(
            app.world_mut(),
            &serde_json::json!({ "path": level.display().to_string() }),
        )
        .expect_err("agent save refused");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("prefab_open"),
            "{err:?}"
        );
        let err = level_io::save_level_document(app.world_mut(), open_now.id.0.as_u128())
            .expect_err("document save refused");
        assert_eq!(err.code, "prefab_open");
        assert_eq!(std::fs::read(&level).expect("level"), before, "nothing written");
        assert!(in_prefab_mode(app.world()), "still editing the prefab");

        exit_prefab_edit(app.world_mut(), false).expect("close");
        assert_eq!(level_save_refusal(app.world()), None);
        let open = app.world().resource::<OpenLevel>().clone();
        level_io::save_level_and_terrain(app.world_mut(), &open, &level).expect("saves after the close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P-160 (3): during Edit in place the level's context draws dimmed (an editor-only
    /// `RenderOverride` tint, never saved) and is not pickable; its proxy cubes stay drawn with
    /// the dim; a level opened beside the host is locked and dimmed too, and the close takes off
    /// exactly what the session added (a lock the user set stays). The prefab's own contents are
    /// neither dimmed nor locked.
    #[test]
    fn context_entities_are_dimmed_and_not_pickable() {
        let dir = temp_project("prefab-context-dim");
        let mut app = app_with_level(&dir);
        crate::mesh_source::register_level_types(app.world_mut());
        // The host becomes the current level (as File ▸ Open leaves it), so the level opened
        // beside it below stays additive.
        app.update();
        let path = write_prefab(&dir, "crate");
        let instance = crate::content::spawn_prefab_asset(app.world_mut(), &path, regia_core::math::Vec3::X)
            .expect("instance");
        let plain = app
            .world_mut()
            .spawn((
                Name::new("plain"),
                Transform::IDENTITY,
                regia_scene::prelude::EntityGuid::new_random(),
            ))
            .id();
        crate::visibility::ensure_snapshot_identity(app.world_mut());

        // A level beside the host: two members, one of them locked by the user.
        let beside = dir.join("assets/beside.level.ron");
        let record = |guid: u128, name: &str| {
            format!(
                r#"(guid: "{}", name: "{name}", components: {{ "regia_scene::transform::Transform": () }}),"#,
                Guid::from_u128(guid)
            )
        };
        std::fs::write(
            &beside,
            format!(
                "SceneFile(format_version: {}, entities: [{}{}], resources: {{}})",
                regia_scene::format::SCENE_FORMAT_VERSION,
                record(0x8128_0001, "far_rock"),
                record(0x8128_0002, "far_locked")
            ),
        )
        .expect("level");
        let meta = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(Guid::from_u128(0x8128_00aa)));
        std::fs::write(
            regia_asset::meta_path_for(&beside),
            meta.to_ron_string().expect("meta"),
        )
        .expect("meta");
        level_io::open_level_document(app.world_mut(), &beside).expect("open beside");
        let by_guid = |world: &mut World, guid: u128| {
            world
                .query::<(Entity, &regia_scene::prelude::EntityGuid)>()
                .iter(world)
                .find(|(_, g)| g.0 == Guid::from_u128(guid))
                .map(|(entity, _)| entity)
                .expect("member")
        };
        let far = by_guid(app.world_mut(), 0x8128_0001);
        let far_locked = by_guid(app.world_mut(), 0x8128_0002);
        app.world_mut()
            .entity_mut(far_locked)
            .insert(crate::viewport_ops::EditorLocked);

        enter_prefab_edit_in_context(app.world_mut(), instance).expect("in place");
        let world = app.world_mut();
        let dim = Some(context_dim());
        // The host's context: dimmed, locked, drawn (a proxy cube that keeps the dim).
        assert_eq!(regia_render::resolve_render_override(world, plain), dim);
        assert!(crate::viewport_ops::is_locked(world, plain) && !crate::viewport_ops::pickable(world, plain));
        assert!(is_drawn_context(world, plain));
        let proxies = crate::render::proxy_entities(world);
        assert!(
            proxies.iter().any(|(entity, _)| *entity == plain),
            "the context keeps its proxy cube"
        );
        assert!(
            crate::render::proxy_overrides(world, &proxies)
                .iter()
                .any(|(key, over)| *key == crate::render::proxy_object_key(plain) && Some(*over) == dim),
            "the cube draws dimmed"
        );
        // The other level: dimmed and locked too.
        for entity in [far, far_locked] {
            assert_eq!(regia_render::resolve_render_override(world, entity), dim);
            assert!(crate::viewport_ops::is_locked(world, entity));
        }
        // The prefab's contents: plain, pickable rows.
        let frame = world
            .query_filtered::<Entity, With<PrefabFrame>>()
            .iter(world)
            .next()
            .expect("the session's frame");
        assert!(!is_drawn_context(world, frame), "the frame is not drawn");
        let contents: Vec<Entity> = world
            .query_filtered::<Entity, With<regia_scene::prelude::EntityGuid>>()
            .iter(world)
            .filter(|entity| is_under(world, *entity, frame))
            .collect();
        assert_eq!(contents.len(), 2, "Root and Child");
        for entity in contents {
            assert_eq!(regia_render::resolve_render_override(world, entity), None);
            assert!(!crate::viewport_ops::is_locked(world, entity));
        }

        exit_prefab_edit(world, false).expect("close");
        assert!(
            world.get_entity(far).is_ok() && world.get_entity(far_locked).is_ok(),
            "the level beside the host stays loaded"
        );
        assert!(world.get::<PrefabOtherLevel>(far).is_none());
        assert!(
            world.get::<regia_render::RenderOverride>(far).is_none(),
            "the dim comes off"
        );
        assert!(
            !crate::viewport_ops::is_locked(world, far),
            "the session lock comes off"
        );
        assert!(world.get::<regia_render::RenderOverride>(far_locked).is_none());
        assert!(
            world
                .get::<crate::viewport_ops::EditorLocked>(far_locked)
                .is_some(),
            "the user's lock stays"
        );
        let restored: Vec<Entity> = world
            .query_filtered::<Entity, With<regia_render::RenderOverride>>()
            .iter(world)
            .collect();
        assert!(restored.is_empty(), "nothing of the dim is left: {restored:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
