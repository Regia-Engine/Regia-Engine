// SPDX-License-Identifier: MPL-2.0
//! Reflected components inside scene files that carry a frozen format of their own (ADR-2323):
//! `post-volume` (v3, `regia_render::post_volumes::PostVolume`), `physical-camera` (v1,
//! `regia_render::lens::PhysicalCamera`), `lens-effects` (v1, `regia_render::lens_effects::LensEffects`,
//! ADR-3196) and `procgen-painted-points` (v2, `regia_procgen::painted::PaintedPoints`, ADR-7215).
//!
//! Each owning crate migrates them at load time, so an old component always reads; `regia
//! upgrade-project` also lists them per scene file (`.level.ron`, `.prefab.ron`) and per level
//! directory (`.level.d/`) and, with `--write`, rewrites the components in place at the current
//! version: the file (or directory) is loaded, each matching component is decoded under its
//! registered type, run through its `migrate`, and encoded again, and the file is re-saved (the
//! same load and re-save `scene-level` uses; a directory rewrites only the entity files that
//! changed). A component newer than the engine reports the file as newer for that format and is
//! never touched.
//!
//! The version of a component is read from its own RON (`format_version`; a component that left
//! the field out is version 0). The plan reports the lowest version in the file, or the highest
//! when one is newer than the engine.

use regia_ecs::reflect::TypeRegistry;
use regia_ecs::serialize::ReflectCodec;
use regia_scene::format::{SceneEntity, SceneFile};
use std::path::Path;

/// One component format embedded in scene files.
pub(crate) struct EmbeddedFormat {
    /// Registry name (`docs/formats/format-hashes.ron`).
    pub(crate) name: &'static str,
    /// The component's pinned type path, the key it is stored under.
    pub(crate) type_path: &'static str,
    /// The current version.
    pub(crate) current: u32,
    /// Registers the types the component's RON needs.
    register: fn(&mut TypeRegistry),
    /// Decodes, migrates and encodes one component value (bare RON of the type).
    migrate_value: fn(&str, &TypeRegistry) -> Result<String, String>,
}

/// The type path a [`regia_render::PostVolume`] is stored under.
pub(crate) const POST_VOLUME_TYPE_PATH: &str = "regia_render::post_volumes::PostVolume";
/// The type path a [`regia_render::PhysicalCamera`] is stored under.
pub(crate) const PHYSICAL_CAMERA_TYPE_PATH: &str = "regia_render::lens::PhysicalCamera";
/// The type path a [`regia_procgen::painted::PaintedPoints`] is stored under.
pub(crate) const PAINTED_POINTS_TYPE_PATH: &str = "regia_procgen::painted::PaintedPoints";

/// The type path a [`regia_render::LensEffects`] is stored under.
pub(crate) const LENS_EFFECTS_TYPE_PATH: &str = "regia_render::lens_effects::LensEffects";

fn typed<T: regia_ecs::reflect::Reflect + regia_ecs::reflect::TypePath>(
    raw: &str,
    registry: &TypeRegistry,
) -> Result<T, String> {
    let registration = registry
        .get_with_type_path(T::type_path())
        .ok_or_else(|| format!("{} is not registered", T::type_path()))?;
    let value = ReflectCodec::new(registry)
        .from_ron_typed(raw, registration)
        .map_err(|e| e.to_string())?;
    value
        .downcast::<T>()
        .map(|b| *b)
        .map_err(|_| format!("not a {}", T::type_path()))
}

fn encode(value: &dyn regia_ecs::reflect::PartialReflect, registry: &TypeRegistry) -> Result<String, String> {
    ReflectCodec::new(registry)
        .to_ron_typed(value)
        .map_err(|e| e.to_string())
}

fn migrate_post_volume(raw: &str, registry: &TypeRegistry) -> Result<String, String> {
    let volume = typed::<regia_render::PostVolume>(raw, registry)?
        .migrate()
        .map_err(|e| e.to_string())?;
    encode(&volume, registry)
}

fn migrate_physical_camera(raw: &str, registry: &TypeRegistry) -> Result<String, String> {
    let lens = typed::<regia_render::PhysicalCamera>(raw, registry)?
        .migrate()
        .map_err(|e| e.to_string())?;
    encode(&lens, registry)
}

fn migrate_lens_effects(raw: &str, registry: &TypeRegistry) -> Result<String, String> {
    let effects = typed::<regia_render::LensEffects>(raw, registry)?
        .migrate()
        .map_err(|e| e.to_string())?;
    encode(&effects, registry)
}

fn migrate_painted_points(raw: &str, registry: &TypeRegistry) -> Result<String, String> {
    let mut painted = typed::<regia_procgen::painted::PaintedPoints>(raw, registry)?;
    painted.migrate()?;
    encode(&painted, registry)
}

/// Every embedded component format.
pub(crate) fn embedded_formats() -> [EmbeddedFormat; 4] {
    [
        EmbeddedFormat {
            name: "post-volume",
            type_path: POST_VOLUME_TYPE_PATH,
            current: regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION,
            register: regia_render::post_volumes::register_post_volume_types,
            migrate_value: migrate_post_volume,
        },
        EmbeddedFormat {
            name: "physical-camera",
            type_path: PHYSICAL_CAMERA_TYPE_PATH,
            current: regia_render::PHYSICAL_CAMERA_FORMAT_VERSION,
            register: regia_render::register_physical_camera_types,
            migrate_value: migrate_physical_camera,
        },
        EmbeddedFormat {
            name: "lens-effects",
            type_path: LENS_EFFECTS_TYPE_PATH,
            current: regia_render::LENS_EFFECTS_FORMAT_VERSION,
            register: regia_render::register_lens_effects_types,
            migrate_value: migrate_lens_effects,
        },
        EmbeddedFormat {
            name: "procgen-painted-points",
            type_path: PAINTED_POINTS_TYPE_PATH,
            current: regia_procgen::painted::PAINTED_POINTS_FORMAT_VERSION,
            register: regia_procgen::painted::register_painted_points_types,
            migrate_value: migrate_painted_points,
        },
    ]
}

/// The format named `name`.
pub(crate) fn embedded_format(name: &str) -> Option<EmbeddedFormat> {
    embedded_formats().into_iter().find(|f| f.name == name)
}

/// The version of one stored component (`format_version`; 0 when the field is left out).
fn component_version(raw: &str) -> u32 {
    regia_core::version::peek_ron_version(raw).unwrap_or(0)
}

/// For each embedded format with at least one component among `entities`, the version to report:
/// the highest when one is newer than `current`, else the lowest.
fn entity_versions(entities: &[&SceneEntity]) -> Vec<(&'static str, u32, u32)> {
    let mut out = Vec::new();
    for format in embedded_formats() {
        let versions: Vec<u32> = entities
            .iter()
            .filter_map(|entity| entity.components.get(format.type_path))
            .map(|raw| component_version(raw.get_ron()))
            .collect();
        let (Some(&low), Some(&high)) = (versions.iter().min(), versions.iter().max()) else {
            continue;
        };
        let found = if high > format.current { high } else { low };
        out.push((format.name, found, format.current));
    }
    out
}

/// For each embedded format with at least one component in the scene file `bytes`, the version to
/// report: the highest when one is newer than `current`, else the lowest. `Ok(empty)` for a file
/// without such components; an error when the file does not load.
pub(crate) fn embedded_versions(bytes: &[u8]) -> Result<Vec<(&'static str, u32, u32)>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let file = SceneFile::from_ron_str(text).map_err(|e| e.to_string())?;
    Ok(entity_versions(&file.entities.iter().collect::<Vec<_>>()))
}

/// [`embedded_versions`] for a level directory (`<name>.level.d/`, spec 37 § 3.5): every entity
/// file under `entities/` is parsed on its own, so a directory whose files are at mixed scene
/// versions (which `read_level_dir` refuses until `upgrade_level_dir` ran) still lists its
/// components.
pub(crate) fn level_dir_embedded_versions(root: &Path) -> Result<Vec<(&'static str, u32, u32)>, String> {
    use regia_scene::level_dir::{ENTITIES_DIR, ENTITY_FILE_SUFFIX, parse_entity_file};
    let mut paths: Vec<std::path::PathBuf> = walkdir::WalkDir::new(root.join(ENTITIES_DIR))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(ENTITY_FILE_SUFFIX))
        .map(walkdir::DirEntry::into_path)
        .collect();
    paths.sort();
    let mut entities = Vec::with_capacity(paths.len());
    for path in &paths {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file = parse_entity_file(path, &text).map_err(|e| e.to_string())?;
        entities.push(file.entity);
    }
    Ok(entity_versions(&entities.iter().collect::<Vec<_>>()))
}

/// Rewrites every component of `format` among `entities` at the current version.
fn migrate_entities<'a>(
    format: &EmbeddedFormat,
    entities: impl Iterator<Item = &'a mut SceneEntity>,
) -> Result<(), String> {
    let mut registry = TypeRegistry::default();
    (format.register)(&mut registry);
    for entity in entities {
        let Some(raw) = entity.components.get_mut(format.type_path) else {
            continue;
        };
        let migrated = (format.migrate_value)(raw.get_ron(), &registry)
            .map_err(|e| format!("{}: {e}", format.type_path))?;
        *raw = ron::value::RawValue::from_boxed_ron(migrated.into_boxed_str()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Rewrites every component of `format` in the scene file `bytes` at the current version and
/// re-saves the file. Components already current are re-encoded unchanged in value.
pub(crate) fn migrate_embedded(format: &EmbeddedFormat, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let mut file = SceneFile::from_ron_str(text).map_err(|e| e.to_string())?;
    migrate_entities(format, file.entities.iter_mut())?;
    let out = file.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// [`migrate_embedded`] for a level directory: reads it (every file must already be at the
/// current scene version, so `upgrade-project` runs `upgrade_level_dir` first), migrates the
/// components and writes the directory back, which rewrites only the entity files whose bytes
/// changed. Returns the number of files written.
pub(crate) fn migrate_level_dir_components(format: &EmbeddedFormat, root: &Path) -> Result<usize, String> {
    let (mut scene, _) = regia_scene::level_dir::read_level_dir(root).map_err(|e| e.to_string())?;
    migrate_entities(format, scene.entities.iter_mut())?;
    let report = regia_scene::level_dir::write_level_dir(root, &scene).map_err(|e| e.to_string())?;
    Ok(report.written.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A level with a v1 post volume (no `lut`, a box), a v3 one and a lens without `format_version`.
    fn level() -> String {
        format!(
            r#"(
    format_version: {scene},
    entities: [
        (
            guid: "3f7a1c2e-8b64-4a35-9d20-6f1e2b3c4d5a",
            components: {{
                "{pv}": (format_version: 1, enabled: true, shape: Box(half_extents: (2.0, 3.0, 4.0)), priority: 2, blend_distance: 1.5, weight: 0.75),
            }},
        ),
        (
            guid: "9b2e6f0a-1e42-4c11-8d5a-3c2f7e10d0aa",
            components: {{
                "{pv}": (format_version: 3, shape: Sphere(radius: 5.0), lut: Some("looks/warm.cube")),
                "{pc}": (shutter_angle: 0.25),
                "{le}": (flare_intensity: 1.5, starburst: 2.0, after_dof: true),
            }},
        ),
    ],
)"#,
            scene = regia_scene::format::SCENE_FORMAT_VERSION,
            pv = POST_VOLUME_TYPE_PATH,
            pc = PHYSICAL_CAMERA_TYPE_PATH,
            le = LENS_EFFECTS_TYPE_PATH,
        )
    }

    #[test]
    fn versions_are_read_per_format_lowest_first() {
        let found = embedded_versions(level().as_bytes()).expect("level loads");
        assert_eq!(
            found,
            vec![
                (
                    "post-volume",
                    1,
                    regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION
                ),
                ("physical-camera", 0, regia_render::PHYSICAL_CAMERA_FORMAT_VERSION),
                ("lens-effects", 0, regia_render::LENS_EFFECTS_FORMAT_VERSION),
            ]
        );
        // A file without such components lists nothing.
        let plain = format!(
            "(format_version: {}, entities: [])",
            regia_scene::format::SCENE_FORMAT_VERSION
        );
        assert!(embedded_versions(plain.as_bytes()).expect("loads").is_empty());
        // A newer component is reported as newer, even next to an old one.
        let newer = level().replace("format_version: 3,", "format_version: 99,");
        let found = embedded_versions(newer.as_bytes()).expect("loads");
        assert_eq!(
            found[0],
            (
                "post-volume",
                99,
                regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION
            )
        );
    }

    #[test]
    fn migration_brings_every_component_current_and_keeps_values() {
        let pv = embedded_format("post-volume").expect("registered");
        let out = migrate_embedded(&pv, level().as_bytes()).expect("migrates");
        let pc = embedded_format("physical-camera").expect("registered");
        let out = migrate_embedded(&pc, &out).expect("migrates");
        let le = embedded_format("lens-effects").expect("registered");
        let out = migrate_embedded(&le, &out).expect("migrates");
        let found = embedded_versions(&out).expect("loads");
        for (name, found, current) in &found {
            assert_eq!(found, current, "{name}");
        }
        // Values survive: decode the migrated components and compare.
        let file = SceneFile::from_ron_str(std::str::from_utf8(&out).expect("utf8")).expect("loads");
        let mut registry = TypeRegistry::default();
        regia_render::post_volumes::register_post_volume_types(&mut registry);
        regia_render::register_physical_camera_types(&mut registry);
        regia_render::register_lens_effects_types(&mut registry);
        let first: regia_render::PostVolume = typed(
            file.entities[0].components[POST_VOLUME_TYPE_PATH].get_ron(),
            &registry,
        )
        .expect("decodes");
        assert_eq!(
            first.shape,
            regia_render::PostVolumeShape::Box {
                half_extents: [2.0, 3.0, 4.0]
            }
        );
        assert_eq!(
            (first.priority, first.blend_distance, first.weight),
            (2, 1.5, 0.75)
        );
        assert_eq!(first.lut, None);
        let second: regia_render::PostVolume = typed(
            file.entities[1].components[POST_VOLUME_TYPE_PATH].get_ron(),
            &registry,
        )
        .expect("decodes");
        assert_eq!(second.lut.as_deref(), Some("looks/warm.cube"));
        let lens: regia_render::PhysicalCamera = typed(
            file.entities[1].components[PHYSICAL_CAMERA_TYPE_PATH].get_ron(),
            &registry,
        )
        .expect("decodes");
        assert_eq!(lens.shutter_angle, 0.25);
        assert_eq!(lens.format_version, regia_render::PHYSICAL_CAMERA_FORMAT_VERSION);
        let effects: regia_render::LensEffects = typed(
            file.entities[1].components[LENS_EFFECTS_TYPE_PATH].get_ron(),
            &registry,
        )
        .expect("decodes");
        assert_eq!(effects.format_version, regia_render::LENS_EFFECTS_FORMAT_VERSION);
        assert_eq!((effects.flare_intensity, effects.starburst), (1.5, 2.0));
        assert!(effects.after_dof);
        // A field left out takes the off value: the old component reads unchanged.
        assert_eq!(effects.dirt_intensity, 0.0);
    }

    #[test]
    fn a_newer_component_is_refused_not_rewritten() {
        let pv = embedded_format("post-volume").expect("registered");
        let newer = level().replace("format_version: 3,", "format_version: 99,");
        let err = migrate_embedded(&pv, newer.as_bytes()).expect_err("newer refused");
        assert!(err.contains("newer"), "{err}");
    }

    /// ADR-7215: a version 1 `PaintedPoints` (no `yaw`) is listed and rewritten at version 2 with
    /// its points kept and yaw 0.
    #[test]
    fn painted_points_v1_upgrade_to_v2_with_yaw_zero() {
        let level = format!(
            r#"(
    format_version: {scene},
    entities: [
        (
            guid: "5d1c0a2e-8b64-4a35-9d20-6f1e2b3c4d5b",
            components: {{
                "{pp}": (points: [(position: (1.0, 2.0, 3.0), normal: (0.0, 1.0, 0.0), layer: 2)], layer_names: ["rocks"], format_version: 1),
            }},
        ),
    ],
)"#,
            scene = regia_scene::format::SCENE_FORMAT_VERSION,
            pp = PAINTED_POINTS_TYPE_PATH,
        );
        let current = regia_procgen::painted::PAINTED_POINTS_FORMAT_VERSION;
        assert_eq!(
            embedded_versions(level.as_bytes()).expect("loads"),
            vec![("procgen-painted-points", 1, current)]
        );
        let format = embedded_format("procgen-painted-points").expect("registered");
        let out = migrate_embedded(&format, level.as_bytes()).expect("migrates");
        assert_eq!(
            embedded_versions(&out).expect("loads"),
            vec![("procgen-painted-points", current, current)]
        );
        let file = SceneFile::from_ron_str(std::str::from_utf8(&out).expect("utf8")).expect("loads");
        let mut registry = TypeRegistry::default();
        regia_procgen::painted::register_painted_points_types(&mut registry);
        let painted: regia_procgen::painted::PaintedPoints = typed(
            file.entities[0].components[PAINTED_POINTS_TYPE_PATH].get_ron(),
            &registry,
        )
        .expect("decodes");
        assert_eq!(painted.points.len(), 1);
        assert_eq!(painted.points[0].layer, 2);
        assert_eq!(painted.points[0].yaw, 0.0);
        assert_eq!(painted.layer_names, vec!["rocks".to_string()]);
    }
}
