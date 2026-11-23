// SPDX-License-Identifier: MPL-2.0
//! Drag-and-drop payload types shared by the panels (`specs/19` § 3).
//!
//! egui payloads must be `Any + Send + Sync`; entities and asset paths cross as plain data.

use regia_ecs::ecs::entity::Entity;

use crate::command::entity_from_bits;

/// An entity dragged from the Outliner (drop on another row to reparent).
#[derive(Debug, Clone)]
pub struct EntityDrag {
    /// The dragged entity as bits.
    pub entity: u64,
}

impl EntityDrag {
    /// The dragged entity, when the id is still valid.
    #[must_use]
    pub fn entity(&self) -> Option<Entity> {
        entity_from_bits(self.entity)
    }
}

/// What kind of asset a content-browser drag carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentKind {
    /// A `.level.ron` (drop → open).
    Level,
    /// A `.prefab.ron` (drop → spawn an instance).
    Prefab,
    /// A `.gltf` or `.glb` (drop → an entity with `MeshSource`; a hidden part gets the renderer).
    Mesh,
    /// A material, a sound, or any other asset. A material assigns the mesh under the cursor.
    /// A sound spawns an emitter. Anything else is not placed and gives no toast.
    Other,
}

/// An asset dragged from the Content Browser (drop into the viewport to spawn/open).
#[derive(Debug, Clone)]
pub struct ContentDrag {
    /// Project-relative path (`assets/...`).
    pub path: String,
    /// What the asset is.
    pub kind: ContentKind,
}

/// Maps a project-relative asset path to its drag kind by extension.
#[must_use]
pub fn kind_of_path(path: &str) -> ContentKind {
    // Import lowercases the extension check, so `Hero.GLB` is a mesh like `hero.glb`.
    let path = path.to_ascii_lowercase();
    if path.ends_with(".level.ron") {
        ContentKind::Level
    } else if path.ends_with(".prefab.ron") || path.ends_with(".variant.ron") {
        ContentKind::Prefab
    } else if path.ends_with(".gltf") || path.ends_with(".glb") {
        ContentKind::Mesh
    } else {
        ContentKind::Other
    }
}

/// The stable name of a drag kind (`level`, `prefab`, `mesh`, `other`).
#[must_use]
pub fn kind_name(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Level => "level",
        ContentKind::Prefab => "prefab",
        ContentKind::Mesh => "mesh",
        ContentKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_of_path_ignores_extension_case() {
        assert_eq!(kind_of_path("assets/Arena.LEVEL.RON"), ContentKind::Level);
        assert_eq!(kind_of_path("assets/Crate.PREFAB.RON"), ContentKind::Prefab);
        assert_eq!(kind_of_path("assets/Hero.GLB"), ContentKind::Mesh);
        assert_eq!(kind_of_path("assets/notes.txt"), ContentKind::Other);
    }
}
