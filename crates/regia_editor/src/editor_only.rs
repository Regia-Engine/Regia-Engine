// SPDX-License-Identifier: MPL-2.0
//! The `EditorOnly` save filter (`docs/specs/19-editor.md` § 1, ADR-7205).
//!
//! Editor scaffolding (helper meshes, tool previews, glTF hierarchies spawned with
//! `GltfSpawnOptions::editor_only`) lives in the edit world next to level content. An entity marked
//! [`EditorOnly`] or [`GltfEditorOnly`] is left out of every saved runtime level, together with its
//! descendants: [`crate::level_io::save_level_file`] (and the level-dir writer, the autosave and the
//! glTF export, which all go through [`strip_scene`]) writes the snapshot without them. Cooks read
//! the saved level, so they never see them either.

use std::collections::HashSet;

use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::prelude::SceneFile;

use crate::engine::ecs::prelude::*;
use crate::gltf_hierarchy::GltfEditorOnly;

/// Marks an edit-world entity (and its descendants) as editor scaffolding: never written to a saved
/// level, an autosave or an export.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorOnly;

/// The guids of every entity marked [`EditorOnly`] or [`GltfEditorOnly`].
#[must_use]
pub fn editor_only_guids(world: &mut World) -> HashSet<Guid> {
    let mut guids: HashSet<Guid> = world
        .query_filtered::<&EntityGuid, With<EditorOnly>>()
        .iter(world)
        .map(|guid| guid.0)
        .collect();
    guids.extend(
        world
            .query_filtered::<&EntityGuid, With<GltfEditorOnly>>()
            .iter(world)
            .map(|guid| guid.0),
    );
    guids
}

/// Removes the records of `marked` entities and of their descendants from `scene` (records may come
/// in any order). Returns how many records went.
pub fn strip_records(scene: &mut SceneFile, marked: &HashSet<Guid>) -> usize {
    if marked.is_empty() {
        return 0;
    }
    let mut gone: HashSet<Guid> = marked.clone();
    loop {
        let before = gone.len();
        for record in &scene.entities {
            if record.parent.is_some_and(|parent| gone.contains(&parent)) {
                gone.insert(record.guid);
            }
        }
        if gone.len() == before {
            break;
        }
    }
    let count = scene.entities.len();
    scene.entities.retain(|record| !gone.contains(&record.guid));
    count - scene.entities.len()
}

/// Strips the world's editor-only entities from `scene` (a snapshot of that world).
pub fn strip_scene(world: &mut World, scene: &mut SceneFile) -> usize {
    let marked = editor_only_guids(world);
    let removed = strip_records(scene, &marked);
    if removed > 0 {
        tracing::debug!(target: "regia_editor", removed, "editor-only entities left out of the saved level");
    }
    removed
}
