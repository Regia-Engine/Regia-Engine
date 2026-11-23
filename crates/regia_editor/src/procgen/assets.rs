// SPDX-License-Identifier: MPL-2.0
//! What generators need from the open project, which `regia_procgen` leaves to its host: the bake
//! directory of the open level (spec 62 § 8) and the meshes and materials graphs name, by the same
//! asset paths (`regia_procgen::draw`).

use regia_ecs::prelude::*;
use regia_procgen::draw::{ProcDrawStats, ProcMaterialLibrary, ProcMeshLibrary};
use regia_procgen::world::{ProcGenSettings, level_bake_dir};

use crate::level_io::OpenLevel;
use crate::mesh_source::{MeshLibrary, project_root};

/// Keeps the bake directory on the open level. An unsaved level has none, and `procgen.bake`
/// answers `procgen_no_bake_dir` until it is saved.
pub(crate) fn follow_level(world: &mut World) {
    let dir = world
        .get_resource::<OpenLevel>()
        .and_then(|l| l.path.as_deref())
        .map(level_bake_dir);
    if let Some(mut settings) = world.get_resource_mut::<ProcGenSettings>()
        && settings.bake_dir != dir
    {
        settings.bake_dir = dir;
    }
}

/// Loads what the last frame could not draw. The cache answers a path that failed without touching
/// it again until its file changes, so this costs a `stat` per missing path. A model of several
/// parts registers its first: a library entry is one mesh and one material.
pub(crate) fn fill_libraries(world: &mut World) {
    let Some(stats) = world.get_resource::<ProcDrawStats>() else {
        return;
    };
    if stats.missing_meshes.is_empty() && stats.missing_materials.is_empty() {
        return;
    }
    let (meshes, materials) = (stats.missing_meshes.clone(), stats.missing_materials.clone());
    let Some(library) = world.get_resource::<MeshLibrary>().cloned() else {
        return;
    };
    let root = project_root(world);
    for path in meshes {
        let model = library.model(root.as_deref(), &path);
        if let Some(part) = model.ok().and_then(|m| m.parts.first().cloned()) {
            world
                .get_resource_or_insert_with(ProcMeshLibrary::default)
                .insert(path, part.mesh, part.material);
        }
    }
    for path in materials {
        if let Ok(material) = library.material(root.as_deref(), &path) {
            world
                .get_resource_or_insert_with(ProcMaterialLibrary::default)
                .insert(path, material);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use regia_scene::level::LevelId;

    use super::*;

    #[test]
    fn the_bake_directory_follows_the_open_level() {
        let mut w = World::new();
        w.insert_resource(ProcGenSettings::default());
        follow_level(&mut w);
        assert_eq!(w.resource::<ProcGenSettings>().bake_dir, None, "no level");
        w.insert_resource(OpenLevel {
            id: LevelId::default(),
            path: Some(PathBuf::from("assets/maps/main.level.ron")),
        });
        follow_level(&mut w);
        assert_eq!(
            w.resource::<ProcGenSettings>().bake_dir.as_deref(),
            Some(std::path::Path::new("assets/maps/main.procgen"))
        );
        w.resource_mut::<OpenLevel>().path = None;
        follow_level(&mut w);
        assert_eq!(w.resource::<ProcGenSettings>().bake_dir, None, "unsaved");
    }

    #[test]
    fn missing_paths_load_once_they_resolve_and_failures_stay_quiet() {
        let mut w = World::new();
        w.insert_resource(MeshLibrary::default());
        let mut stats = ProcDrawStats::default();
        stats.missing_meshes.insert("builtin:cube".to_string());
        stats.missing_meshes.insert("no/such.glb".to_string());
        stats.missing_materials.insert("no/such.material.ron".to_string());
        w.insert_resource(stats);
        fill_libraries(&mut w);
        fill_libraries(&mut w);
        let meshes = w.resource::<ProcMeshLibrary>();
        assert!(meshes.0.contains_key("builtin:cube"));
        assert!(!meshes.0.contains_key("no/such.glb"));
        assert!(
            w.get_resource::<ProcMaterialLibrary>()
                .is_none_or(|m| m.0.is_empty())
        );
    }
}
