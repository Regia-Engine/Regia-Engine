// SPDX-License-Identifier: MPL-2.0
//! Generators at cook and package time (spec 62 § 8): an `editor_only` generator and its bakes stay
//! out of what ships. The package step strips the generator entities (with their descendants) from
//! each staged level ([`strip_editor_only`]) and leaves their `.pgout` files out of the copied
//! assets ([`bake_ships`]). Neither needs the `pipeline` feature: both read and rewrite level text
//! only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_ecs::reflect::TypePath;
use regia_scene::format::{SceneEntity, SceneFile};

use crate::world::ProcGenerator;

/// Whether a component type path names [`ProcGenerator`].
pub fn is_generator_component(type_path: &str) -> bool {
    type_path == ProcGenerator::type_path()
}

/// Whether a saved [`ProcGenerator`] value (raw RON of the scene file) has `editor_only: true`. A
/// value that does not parse is not editor-only (it ships, and the level load reports it).
pub fn raw_is_editor_only(raw: &str) -> bool {
    let Ok(value) = ron::from_str::<ron::Value>(raw) else {
        return false;
    };
    let ron::Value::Map(map) = value else {
        return false;
    };
    map.iter().any(|(k, v)| {
        matches!(k, ron::Value::String(s) if s == "editor_only") && matches!(v, ron::Value::Bool(true))
    })
}

fn entity_is_editor_only(e: &SceneEntity) -> bool {
    e.components
        .iter()
        .any(|(path, raw)| is_generator_component(path) && raw_is_editor_only(raw.get_ron()))
}

/// The guids of the level's `editor_only` generators, sorted.
pub fn editor_only_generators(scene: &SceneFile) -> Vec<Guid> {
    let mut out: Vec<Guid> = scene
        .entities
        .iter()
        .filter(|e| entity_is_editor_only(e))
        .map(|e| e.guid)
        .collect();
    out.sort();
    out
}

/// What [`strip_editor_only`] took out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorOnlyStrip {
    /// The editor-only generators, sorted.
    pub generators: Vec<Guid>,
    /// Entities removed (the generators and their descendants).
    pub entities: usize,
}

/// Removes the level's `editor_only` generators and their descendants (spec 62 § 7.1: "removed from
/// cooked levels with its output").
pub fn strip_editor_only(scene: &mut SceneFile) -> EditorOnlyStrip {
    let generators = editor_only_generators(scene);
    if generators.is_empty() {
        return EditorOnlyStrip::default();
    }
    let mut gone: BTreeSet<Guid> = generators.iter().copied().collect();
    // Descendants, whatever order the records are in.
    loop {
        let before = gone.len();
        for e in &scene.entities {
            if e.parent.is_some_and(|p| gone.contains(&p)) {
                gone.insert(e.guid);
            }
        }
        if gone.len() == before {
            break;
        }
    }
    let total = scene.entities.len();
    scene.entities.retain(|e| !gone.contains(&e.guid));
    EditorOnlyStrip {
        generators,
        entities: total - scene.entities.len(),
    }
}

/// Whether level text may hold an `editor_only` generator (a cheap text test before parsing).
pub fn level_text_may_strip(text: &str) -> bool {
    text.contains(ProcGenerator::type_path()) && text.contains("editor_only: true")
}

/// The level a bake directory belongs to: `<stem>.level.ron` or `<stem>.level.d` beside
/// `<stem>.procgen/` (the inverse of [`crate::world::level_bake_dir`]), whichever exists.
pub fn level_of_bake_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let stem = name.strip_suffix(".procgen")?;
    [format!("{stem}.level.ron"), format!("{stem}.level.d")]
        .into_iter()
        .map(|n| dir.with_file_name(n))
        .find(|p| p.exists())
}

/// Whether the bake file at `path` (`<level>.procgen/<guid>[.<Tier>].pgout`) ships: not when its
/// level holds the generator as `editor_only`. Anything else ships as before (other files in the
/// directory, a level that cannot be read, an unknown guid).
pub fn bake_ships(path: &Path) -> bool {
    let Some(guid) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(crate::world::bake_file_guid)
    else {
        return true;
    };
    let Some(level) = path.parent().and_then(level_of_bake_dir) else {
        return true;
    };
    match regia_scene::level_dir::read_level_source(&level) {
        Ok(scene) => !editor_only_generators(&scene).contains(&guid),
        Err(e) => {
            tracing::warn!(target: "regia_procgen", level = %level.display(), "bake {} ships: its level does not read: {e}", path.display());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(guid: u128, parent: Option<u128>, generator: Option<&str>) -> SceneEntity {
        let mut components = std::collections::BTreeMap::new();
        if let Some(raw) = generator {
            components.insert(
                ProcGenerator::type_path().to_string(),
                ron::value::RawValue::from_boxed_ron(raw.into()).expect("raw ron"),
            );
        }
        SceneEntity {
            guid: Guid(guid),
            name: None,
            parent: parent.map(Guid),
            components,
            prefab_instance: None,
        }
    }

    #[test]
    fn editor_only_generators_and_their_children_are_stripped() {
        let mut scene = SceneFile::new();
        scene.entities = vec![
            // A child listed before its parent.
            entity(4, Some(2), None),
            entity(1, None, Some("(graph: \"a.pgraph.ron\", editor_only: false)")),
            entity(
                2,
                None,
                Some("ProcGenerator(graph: \"b.pgraph.ron\", editor_only: true)"),
            ),
            entity(5, Some(4), None),
            entity(3, None, None),
        ];
        assert_eq!(editor_only_generators(&scene), vec![Guid(2)]);
        let strip = strip_editor_only(&mut scene);
        assert_eq!(strip.generators, vec![Guid(2)]);
        assert_eq!(strip.entities, 3);
        let left: Vec<u128> = scene.entities.iter().map(|e| e.guid.0).collect();
        assert_eq!(left, vec![1, 3]);
        assert_eq!(strip_editor_only(&mut scene), EditorOnlyStrip::default());
    }

    #[test]
    fn unparsable_or_absent_flags_are_not_editor_only() {
        assert!(!raw_is_editor_only("(graph: \"a\")"));
        assert!(!raw_is_editor_only("not ron ("));
        assert!(raw_is_editor_only("(editor_only: true)"));
    }

    #[test]
    fn bake_names_map_to_guids_and_levels() {
        let g = Guid(0xabc);
        assert_eq!(crate::world::bake_file_guid(&format!("{g}.pgout")), Some(g));
        assert_eq!(
            crate::world::bake_file_guid(&format!("{g}.MobileLow.pgout")),
            Some(g)
        );
        assert_eq!(crate::world::bake_file_guid(&format!("{g}.Fancy.pgout")), None);
        assert_eq!(crate::world::bake_file_guid("notes.txt"), None);
        let dir = regia_test_utils::unique_temp_dir("procgen-cook-level-of");
        let bakes = dir.join("forest.procgen");
        assert_eq!(level_of_bake_dir(&bakes), None, "no level yet");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("forest.level.ron"), "()").expect("level");
        assert_eq!(level_of_bake_dir(&bakes), Some(dir.join("forest.level.ron")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
