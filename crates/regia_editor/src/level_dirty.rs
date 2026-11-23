// SPDX-License-Identifier: MPL-2.0
//! The editor feeds spec 37 § 3.4's per-level dirty set (ADR-7203).
//!
//! Change tracking reads the ECS change ticks: a feed pass marks every entity of the open level
//! whose components changed (or were added) since the previous pass, every entity added or
//! removed since then (by guid), and the header when [`LevelSettings`] or [`LevelLayers`]
//! changed. A prefab-internal entity marks its instance root (the record that holds its
//! overrides). The pass runs each frame the undo history changed (a push, an undo or a redo; so
//! an undo dirties exactly what it put back), and once more right before a save, so an edit that
//! bypassed the history is never lost.
//!
//! [`crate::level_io::save_level_file`] writes a directory-form level with
//! `write_level_dir_dirty` when the tracking is complete for that path: the level was opened
//! from it (or last saved to it) in this session. Any other save (a first save, a save-as, a
//! level that was not opened from that directory) writes the whole level as before; the byte
//! comparison of `write_level_dir` still skips unchanged files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::prelude::*;
use regia_scene::layers::LevelLayers;
use regia_scene::level::{LevelId, LevelMember, LevelSettings};
use regia_scene::level_dir::LevelDirtyTracker;
use regia_scene::prefab::PrefabLink;
use regia_scene::prelude::EntityGuid;

use crate::command::UndoStack;

/// The feed's state: which level it watches and what it saw last.
#[derive(Resource, Debug)]
pub struct LevelDirtyFeed {
    level: LevelId,
    /// The directory the tracking is complete for (`level_paths(..).1`).
    path: PathBuf,
    /// The change tick of the last pass; changes after it are new.
    last: Tick,
    /// Guids of the level's saved records at the last pass.
    known: BTreeSet<Guid>,
    /// The undo history's change tick at the last per-frame check.
    history: Option<Tick>,
}

impl LevelDirtyFeed {
    /// Whether the feed tracks `level` saved at `path` (either spelling).
    #[must_use]
    pub fn tracks(&self, level: LevelId, path: &Path) -> bool {
        self.level == level && self.path == regia_scene::level_dir::level_paths(path).1
    }
}

/// The guids of the records a save writes for `level`: its members, a prefab-internal entity
/// counting as its instance root.
fn record_guids(world: &mut World, level: LevelId) -> BTreeSet<Guid> {
    let mut query = world.query::<(Entity, &EntityGuid, &LevelMember)>();
    let members: Vec<Entity> = query
        .iter(world)
        .filter(|(_, _, member)| member.level() == level)
        .map(|(entity, _, _)| entity)
        .collect();
    members
        .into_iter()
        .filter_map(|entity| record_of(world, entity))
        .collect()
}

/// The guid of the saved record that holds `entity`: its own, or for a prefab-internal entity
/// the nearest instance root above it that is not itself prefab-internal.
fn record_of(world: &World, entity: Entity) -> Option<Guid> {
    let mut cursor = entity;
    for _ in 0..256 {
        if world.get::<PrefabLink>(cursor).is_none() {
            return world.get::<EntityGuid>(cursor).map(|guid| guid.0);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// Starts complete tracking of `level` saved at `path`: nothing is dirty, and changes from now on
/// are tracked. Called when the editor opens a level from `path` and after it saved there.
pub fn begin_tracking(world: &mut World, level: LevelId, path: &Path) {
    let known = record_guids(world, level);
    let last = world.increment_change_tick();
    let dir = regia_scene::level_dir::level_paths(path).1;
    world
        .get_resource_or_insert_with(LevelDirtyTracker::default)
        .clear(&dir);
    // The history as it is now: only a later push, undo or redo starts a pass.
    let history = world
        .get_resource_change_ticks::<UndoStack>()
        .map(|ticks| ticks.changed);
    world.insert_resource(LevelDirtyFeed {
        level,
        path: dir,
        last,
        known,
        history,
    });
}

/// One feed pass (see the module docs). Does nothing without a [`LevelDirtyFeed`].
pub fn feed_level_dirty(world: &mut World) {
    let Some(mut feed) = world.remove_resource::<LevelDirtyFeed>() else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_editor", "level_dirty_feed").entered();
    let this = world.increment_change_tick();
    let mut dirty: BTreeSet<Guid> = BTreeSet::new();
    let mut members: Vec<Entity> = {
        let mut query = world.query::<(Entity, &LevelMember)>();
        query
            .iter(world)
            .filter(|(_, member)| member.level() == feed.level)
            .map(|(entity, _)| entity)
            .collect()
    };
    members.sort_unstable();
    let mut now: BTreeSet<Guid> = BTreeSet::new();
    for entity in members {
        let Some(record) = record_of(world, entity) else {
            continue;
        };
        now.insert(record);
        let Ok(entity_ref) = world.get_entity(entity) else {
            continue;
        };
        let changed = entity_ref.archetype().components().iter().any(|id| {
            entity_ref
                .get_change_ticks_by_id(*id)
                .is_some_and(|ticks| ticks.is_changed(feed.last, this))
        });
        if changed {
            dirty.insert(record);
        }
    }
    // Added and removed records.
    dirty.extend(now.symmetric_difference(&feed.known).copied());
    let header = [
        world.get_resource_change_ticks::<LevelSettings>(),
        world.get_resource_change_ticks::<LevelLayers>(),
    ]
    .into_iter()
    .flatten()
    .any(|ticks| ticks.is_changed(feed.last, this));
    if !dirty.is_empty() || header {
        let mut tracker = world.get_resource_or_insert_with(LevelDirtyTracker::default);
        for guid in &dirty {
            tracker.mark_entity(&feed.path, *guid);
        }
        if header {
            tracker.mark_header(&feed.path);
        }
    }
    feed.known = now;
    feed.last = this;
    world.insert_resource(feed);
}

/// Per-frame entry: a feed pass when the undo history changed since the last check (a push, an
/// undo, a redo). The editor calls it once per frame. The per-document feed of the open levels
/// (ADR-7792) runs on the same trigger.
pub fn feed_on_history_change(world: &mut World) {
    feed_documents_on_history_change(world);
    let Some(ticks) = world.get_resource_change_ticks::<UndoStack>() else {
        return;
    };
    let Some(mut feed) = world.get_resource_mut::<LevelDirtyFeed>() else {
        return;
    };
    if feed.history == Some(ticks.changed) {
        return;
    }
    feed.history = Some(ticks.changed);
    feed_level_dirty(world);
}

/// What a save of `level` to `path` may write: `Some(set)` when the tracking is complete for
/// that path (after a final feed pass), `None` when the whole level must be written.
pub fn dirty_set_for_save(
    world: &mut World,
    level: LevelId,
    path: &Path,
) -> Option<regia_scene::level_dir::LevelDirtySet> {
    if !world
        .get_resource::<LevelDirtyFeed>()
        .is_some_and(|feed| feed.tracks(level, path))
    {
        return None;
    }
    feed_level_dirty(world);
    Some(
        world
            .get_resource::<LevelDirtyTracker>()
            .and_then(|tracker| tracker.get(path).cloned())
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------------------------
// Per-document dirty state of the open levels and layer files (ADR-7792)

/// The per-document feed's state: what each open document held at the last pass.
#[derive(Resource, Debug, Default)]
pub struct DocumentFeed {
    /// The change tick of the last pass (`None`: not started).
    last: Option<Tick>,
    /// Per document id, the guids of its records at the last pass.
    known: std::collections::BTreeMap<u128, BTreeSet<Guid>>,
    /// The undo history's change tick at the last per-frame check.
    history: Option<Tick>,
}

/// The document of `record` (an entity holding a saved record) of `level`: the first layer file
/// (by name) its membership lists, else the level.
fn document_of(world: &World, record: Entity, level: LevelId, separated: Option<&BTreeSet<String>>) -> u128 {
    if let Some(separated) = separated.filter(|s| !s.is_empty())
        && let Some(membership) = world.get::<regia_scene::layers::LayerMembership>(record)
        && let Some(layer) = separated.iter().find(|layer| membership.layers.contains(layer))
    {
        return crate::level_io::layer_document_id(level, layer);
    }
    level.0.as_u128()
}

/// The entity that holds the saved record `entity` belongs to (itself, or the nearest instance
/// root above a prefab-internal entity).
fn record_entity(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    for _ in 0..256 {
        if world.get::<PrefabLink>(cursor).is_none() {
            return Some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// One pass over the open levels' members: per document, its record guids now, and the
/// documents whose members changed since `since`.
fn scan_documents(
    world: &mut World,
    since: Option<Tick>,
    this: Tick,
) -> (std::collections::BTreeMap<u128, BTreeSet<Guid>>, BTreeSet<u128>) {
    let docs = crate::level_io::documents(world);
    let mut now: std::collections::BTreeMap<u128, BTreeSet<Guid>> =
        docs.iter().map(|doc| (doc.id, BTreeSet::new())).collect();
    let mut changed = BTreeSet::new();
    let separated = world
        .get_resource::<crate::level_io::OpenLevels>()
        .map(|model| model.separated.clone())
        .unwrap_or_default();
    let levels: BTreeSet<LevelId> = docs.iter().map(|doc| doc.level).collect();
    let mut members: Vec<(Entity, LevelId)> = {
        let mut query = world.query::<(Entity, &LevelMember)>();
        query
            .iter(world)
            .filter(|(_, member)| levels.contains(&member.level()))
            .map(|(entity, member)| (entity, member.level()))
            .collect()
    };
    members.sort_unstable();
    for (entity, level) in members {
        let Some(holder) = record_entity(world, entity) else {
            continue;
        };
        let Some(guid) = world.get::<EntityGuid>(holder).map(|g| g.0) else {
            continue;
        };
        let doc = document_of(world, holder, level, separated.get(&level));
        now.entry(doc).or_default().insert(guid);
        let Some(since) = since else {
            continue;
        };
        let Ok(entity_ref) = world.get_entity(entity) else {
            continue;
        };
        if entity_ref.archetype().components().iter().any(|id| {
            entity_ref
                .get_change_ticks_by_id(*id)
                .is_some_and(|ticks| ticks.is_changed(since, this))
        }) {
            changed.insert(doc);
        }
    }
    (now, changed)
}

/// One per-document pass: the documents whose members changed, appeared or disappeared since
/// the last pass tag the top undo entry when it is new (a push or a merge; an undo or redo
/// replays entries that are already tagged). Does nothing without an open level.
pub fn feed_documents(world: &mut World) {
    if world.get_resource::<crate::level_io::OpenLevel>().is_none() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "level_documents_feed").entered();
    let mut feed = world.remove_resource::<DocumentFeed>().unwrap_or_default();
    let this = world.increment_change_tick();
    let (now, mut changed) = scan_documents(world, feed.last, this);
    if feed.last.is_some() {
        for (doc, guids) in &now {
            if feed.known.get(doc).is_some_and(|known| known != guids) {
                changed.insert(*doc);
            }
        }
        // A record that moved between documents (a membership change) changes both.
        for (doc, known) in &feed.known {
            if !now.contains_key(doc) && !known.is_empty() {
                changed.insert(*doc);
            }
        }
    }
    // Tags are bookkeeping, not an edit: they must not look like a history change (which would
    // start the history-triggered feeds again next frame).
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        let stack = stack.bypass_change_detection();
        if stack.take_new_entry() && !changed.is_empty() {
            let docs: Vec<u128> = changed.into_iter().collect();
            stack.tag_top(&docs);
        }
    }
    feed.known = now;
    feed.last = Some(this);
    world.insert_resource(feed);
}

/// Starts the per-document feed over from what the open documents hold now, without tagging
/// anything: after a level opened, closed, saved or a layer moved to or from its own file.
pub fn resync_documents(world: &mut World) {
    let this = world.increment_change_tick();
    let (now, _) = scan_documents(world, None, this);
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        stack.bypass_change_detection().take_new_entry();
    }
    let history = world
        .get_resource_change_ticks::<UndoStack>()
        .map(|ticks| ticks.changed);
    let mut feed = world.get_resource_or_insert_with(DocumentFeed::default);
    feed.known = now;
    feed.last = Some(this);
    feed.history = history;
}

/// Per-frame trigger of [`feed_documents`]: when the undo history changed since the last check.
pub fn feed_documents_on_history_change(world: &mut World) {
    let Some(ticks) = world.get_resource_change_ticks::<UndoStack>() else {
        return;
    };
    if world
        .get_resource::<DocumentFeed>()
        .is_some_and(|feed| feed.history == Some(ticks.changed))
    {
        return;
    }
    feed_documents(world);
    if let Some(mut feed) = world.get_resource_mut::<DocumentFeed>() {
        feed.history = Some(ticks.changed);
    }
}

/// Marks every document of `level` saved (after File ▸ Save wrote the level and its layer
/// files).
pub fn mark_level_documents_saved(world: &mut World, level: LevelId) {
    let docs: Vec<u128> = crate::level_io::documents(world)
        .into_iter()
        .filter(|doc| doc.level == level)
        .map(|doc| doc.id)
        .collect();
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        for doc in docs {
            stack.mark_document_saved(doc);
        }
    }
    resync_documents(world);
}

/// Whether one open document has unsaved edits (a feed pass first).
pub fn is_document_dirty(world: &mut World, document: u128) -> bool {
    feed_documents(world);
    world
        .get_resource::<UndoStack>()
        .is_some_and(|stack| stack.is_document_dirty(document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::App;
    use regia_core::id::Guid;
    use regia_core::math::Vec3;
    use regia_scene::level_dir::{LevelDirtyTracker, entity_rel_path};
    use regia_scene::prelude::{EntityGuid, SceneFile};

    use crate::command::{UndoStack, undo_world};

    const A: u128 = 0x5a00_0000_0000_4000_8000_0000_0000_000a;
    const B: u128 = 0x5a00_0000_0000_4000_8000_0000_0000_000b;
    const C: u128 = 0x5a00_0000_0000_4000_8000_0000_0000_000c;

    fn level_dir(root: &std::path::Path) -> std::path::PathBuf {
        let text = format!(
            r#"SceneFile(format_version: {v}, entities: [
                (guid: "{a}", name: "a", components: {{ "regia_scene::transform::Transform": () }}),
                (guid: "{b}", name: "b", components: {{ "regia_scene::transform::Transform": () }}),
                (guid: "{c}", name: "c", components: {{ "regia_scene::transform::Transform": () }}),
            ], resources: {{}})"#,
            v = regia_scene::format::SCENE_FORMAT_VERSION,
            a = Guid::from_u128(A),
            b = Guid::from_u128(B),
            c = Guid::from_u128(C),
        );
        let scene = SceneFile::from_ron_str(&text).expect("scene");
        let dir = root.join("town.level.d");
        regia_scene::level_dir::write_level_dir(&dir, &scene).expect("write");
        dir
    }

    fn entity_of(world: &mut World, guid: u128) -> Entity {
        let mut query = world.query::<(Entity, &EntityGuid)>();
        query
            .iter(world)
            .find(|(_, g)| g.0 == Guid::from_u128(guid))
            .map(|(entity, _)| entity)
            .expect("entity")
    }

    #[test]
    fn editing_one_entity_rewrites_one_file_and_undo_feeds_the_tracker() {
        let root = regia_test_utils::unique_temp_dir("level-dirty-save");
        let dir = level_dir(&root);
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(UndoStack::new());
        let open = crate::level_io::open_level_file(app.world_mut(), &dir).expect("open");
        app.world_mut().insert_resource(open.clone());

        let file = |guid: u128| dir.join(entity_rel_path(Guid::from_u128(guid)));
        // A byte-different (still valid) copy of `c` on disk: a full save would rewrite it, a
        // save of the dirty set leaves it alone.
        let mut tampered = std::fs::read(file(C)).expect("c");
        tampered.extend_from_slice(b"\n\n");
        std::fs::write(file(C), &tampered).expect("tamper");
        let before_b = std::fs::read(file(B)).expect("b");
        let before_header = std::fs::read(dir.join(regia_scene::level_dir::HEADER_FILE)).expect("header");

        let a = entity_of(app.world_mut(), A);
        crate::inspector::edit_value(
            app.world_mut(),
            &[a],
            "Transform",
            "translation",
            Box::new(Vec3::new(1.0, 2.0, 3.0)),
        )
        .expect("edit");
        feed_level_dirty(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty set");
        assert_eq!(
            set.entities.into_iter().collect::<Vec<_>>(),
            vec![Guid::from_u128(A)],
            "only the edited entity"
        );
        assert!(!set.header, "entity edits never dirty level.ron");

        crate::level_io::save_level_file(app.world_mut(), &open, &dir).expect("save");
        assert!(
            String::from_utf8(std::fs::read(file(A)).expect("a"))
                .expect("utf8")
                .contains("2.0"),
            "the edited entity was written"
        );
        assert_eq!(std::fs::read(file(B)).expect("b"), before_b);
        assert_eq!(
            std::fs::read(file(C)).expect("c"),
            tampered,
            "not in the dirty set: untouched"
        );
        assert_eq!(
            std::fs::read(dir.join(regia_scene::level_dir::HEADER_FILE)).expect("header"),
            before_header
        );
        assert!(
            app.world().resource::<LevelDirtyTracker>().get(&dir).is_none(),
            "a save clears the set"
        );

        // Undo is a change too.
        assert!(undo_world(app.world_mut()));
        feed_level_dirty(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty");
        assert!(set.entities.contains(&Guid::from_u128(A)));

        // Deleting an entity dirties its guid (its file goes on save).
        let b = entity_of(app.world_mut(), B);
        let mut delete = crate::command::DespawnEntities {
            entities: vec![b.to_bits()],
            snapshot: None,
        };
        crate::command::EditorCommand::apply(&mut delete, app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().push(Box::new(delete));
        feed_level_dirty(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty");
        assert!(set.entities.contains(&Guid::from_u128(B)), "{set:?}");
        crate::level_io::save_level_file(app.world_mut(), &open, &dir).expect("save");
        assert!(!file(B).exists(), "the deleted entity's file is gone");
        assert_eq!(std::fs::read(file(C)).expect("c"), tampered);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A prefab-internal edit marks the instance's own record (the internal entities are never
    /// files); a settings change marks only the header; a frame without a history change runs no
    /// pass, the next history change does.
    #[test]
    fn prefab_members_mark_their_instance_settings_mark_the_header_and_history_gates_the_pass() {
        use regia_scene::prefab::{PrefabLibrary, PrefabLink, PrefabTemplate, PrefabWorldExt};
        use regia_scene::prelude::{SpawnOptions, Transform};
        let root = regia_test_utils::unique_temp_dir("level-dirty-prefab");
        let dir = level_dir(&root);
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(UndoStack::new());
        let open = crate::level_io::open_level_file(app.world_mut(), &dir).expect("open");
        app.world_mut().insert_resource(open.clone());

        // A two-entity prefab (root + one child), instantiated into the open level.
        let template_text = format!(
            r#"SceneFile(format_version: {v}, entities: [
                (guid: "7e000000-0000-4000-8000-000000000001", components: {{ "regia_scene::transform::Transform": () }}),
                (guid: "7e000000-0000-4000-8000-000000000002", parent: Some("7e000000-0000-4000-8000-000000000001"),
                 components: {{ "regia_scene::transform::Transform": () }}),
            ], resources: {{}})"#,
            v = regia_scene::format::SCENE_FORMAT_VERSION
        );
        let asset = Guid::from_u128(0x007e_a55e);
        let template = {
            let file = SceneFile::from_ron_str(&template_text).expect("template");
            let registry = app
                .world()
                .resource::<regia_ecs::reflect::AppTypeRegistry>()
                .clone();
            PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake")
        };
        app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
        let instance_guid = Guid::from_u128(0x017e_0001);
        let opts = SpawnOptions {
            level: Some(open.id),
            ..SpawnOptions::default()
        };
        app.world_mut()
            .spawn_prefab(asset, Some(Transform::default()), &opts, instance_guid)
            .expect("instance");
        feed_level_dirty(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty");
        assert_eq!(
            set.entities.iter().copied().collect::<Vec<_>>(),
            vec![instance_guid],
            "the new instance is one record; its members are not files"
        );

        // Start clean (as after a save), then edit a prefab member.
        begin_tracking(app.world_mut(), open.id, &dir);
        let member = {
            let world = app.world_mut();
            let mut links = world.query::<(Entity, &PrefabLink)>();
            links
                .iter(world)
                .find(|(_, link)| link.local_id == 0x7e00_0000_0000_4000_8000_0000_0000_0002)
                .map(|(entity, _)| entity)
                .expect("member")
        };
        app.world_mut()
            .get_mut::<Transform>(member)
            .expect("transform")
            .translation = Vec3::new(0.0, 9.0, 0.0);
        // No history change yet: the per-frame entry runs no pass.
        feed_on_history_change(app.world_mut());
        feed_on_history_change(app.world_mut());
        assert!(
            app.world().resource::<LevelDirtyTracker>().get(&dir).is_none(),
            "no pass without a history change"
        );
        app.world_mut().resource_mut::<UndoStack>().mark_dirty();
        feed_on_history_change(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty");
        assert_eq!(
            set.entities.iter().copied().collect::<Vec<_>>(),
            vec![instance_guid]
        );
        assert!(!set.header);

        // A settings change dirties only level.ron.
        begin_tracking(app.world_mut(), open.id, &dir);
        app.world_mut().resource_mut::<LevelSettings>().gravity = Vec3::new(0.0, -3.7, 0.0);
        feed_level_dirty(app.world_mut());
        let set = app
            .world()
            .resource::<LevelDirtyTracker>()
            .get(&dir)
            .cloned()
            .expect("dirty");
        assert!(set.header);
        assert!(set.entities.is_empty(), "{set:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A save keeps the level's layer binding (`LevelLayers`) in both forms; before ADR-7203 the
    /// snapshot dropped it (the save wrote settings only).
    #[test]
    fn a_save_keeps_the_levels_layer_binding() {
        let root = regia_test_utils::unique_temp_dir("level-binding-save");
        let path = root.join("town.level.ron");
        std::fs::write(
            &path,
            format!(
                r#"SceneFile(format_version: {v}, entities: [(guid: "5a00000a-0000-4000-8000-00000000000a", components: {{}})], resources: {{ "regia_scene::layers::LevelLayers": (defs: ["asset:1a7e0000-0000-4000-8000-000000000001"], entries: []) }})"#,
                v = regia_scene::format::SCENE_FORMAT_VERSION
            ),
        )
        .expect("write");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(UndoStack::new());
        let open = crate::level_io::open_level_file(app.world_mut(), &path).expect("open");
        crate::level_io::save_level_file(app.world_mut(), &open, &path).expect("save");
        let saved = SceneFile::from_ron_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        let binding = regia_scene::layers::LevelLayers::from_scene(&saved)
            .expect("the binding was kept")
            .expect("parses");
        assert_eq!(
            binding.defs,
            vec!["asset:1a7e0000-0000-4000-8000-000000000001".to_string()]
        );

        // The directory form: a settings change rewrites level.ron, binding included.
        let dir = root.join("town.level.d");
        regia_scene::level_dir::split_level_file(&path, &dir).expect("split");
        std::fs::remove_file(&path).expect("one form");
        let open = crate::level_io::open_level_file(app.world_mut(), &dir).expect("open dir");
        app.world_mut().insert_resource(open.clone());
        app.world_mut().resource_mut::<LevelSettings>().gravity = Vec3::new(0.0, -1.6, 0.0);
        crate::level_io::save_level_file(app.world_mut(), &open, &dir).expect("save dir");
        let (saved, _) = regia_scene::level_dir::read_level_dir(&dir).expect("read dir");
        assert!(
            regia_scene::layers::LevelLayers::from_scene(&saved).is_some(),
            "level.ron kept the binding"
        );
        assert!(
            std::fs::read_to_string(dir.join(regia_scene::level_dir::HEADER_FILE))
                .expect("header")
                .contains("-1.6"),
            "the settings change was written"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
