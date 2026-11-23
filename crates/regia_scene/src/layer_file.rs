// SPDX-License-Identifier: MPL-2.0
//! Layer files: a data layer whose members are stored in their own file next to the level
//! (`<level>.<layer>.layer.ron`, format `layer-file` v1, `docs/formats/layer-file.md`, ADR-7792).
//!
//! Two people can then edit different layers of one level without touching the same file. The
//! level itself does not name its layer files: the sidecar's name is the reference (a level
//! `forest.level.ron` owns every `forest.<layer>.layer.ron` beside it whose `<layer>` is a valid
//! layer name, which holds no `.`, so the match is unambiguous). Every reader of a level source
//! ([`crate::level_dir::read_level_as_stored`], so the runtime loader, the cook, the bake tools and
//! the editor) folds the layer files back in ([`merge_layer_files`]); the result is the level as
//! if its layers had never moved.
//!
//! A record moves to the file of the first separated layer (by name) its `LayerMembership`
//! lists. Each moved record remembers its position in the whole level (`at`), so merging back an
//! unchanged split gives the original level's record order and therefore its exact bytes.
//! Records that a member's subtree holds without a membership of their own stay in the level.
//!
//! Only the single-file form (`.level.ron`) is split by the editor: the directory form
//! (`.level.d/`) already stores one file per entity. The readers merge layer files for both forms.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::format::{SCENE_FORMAT_VERSION, SceneEntity, SceneFile, SceneFormatError};
use crate::layers::{LayerMembership, valid_layer_name};
use crate::level_dir::{LevelDirError, level_paths};

/// Version of the `layer-file` format.
pub const LAYER_FILE_FORMAT_VERSION: u32 = 1;

/// Suffix of a layer file (`forest.props.layer.ron`).
pub const LAYER_FILE_SUFFIX: &str = ".layer.ron";

/// The serde field-list fingerprint of `.layer.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn layer_file_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LayerFile>(),
        regia_core::schema::serde_field_names::<LayerRecord>(),
    ])
}

/// One layer's members, stored beside the level (`<level>.<layer>.layer.ron`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerFile {
    /// [`LAYER_FILE_FORMAT_VERSION`] the file was written at.
    pub format_version: u32,
    /// `SCENE_FORMAT_VERSION` of the records.
    pub scene_format_version: u32,
    /// The layer (also the middle of the file name).
    pub layer: String,
    /// The records, in level order.
    #[serde(default)]
    pub entities: Vec<LayerRecord>,
}

/// One record of a layer file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerRecord {
    /// Position of the record in the whole level's record list when it was split off; merging
    /// puts it back there (records added later sit at the end).
    pub at: u32,
    /// Exactly the record a `SceneFile` would hold.
    pub entity: SceneEntity,
}

impl LayerFile {
    /// An empty file for `layer` at the current versions.
    #[must_use]
    pub fn new(layer: impl Into<String>) -> Self {
        Self {
            format_version: LAYER_FILE_FORMAT_VERSION,
            scene_format_version: SCENE_FORMAT_VERSION,
            layer: layer.into(),
            entities: Vec::new(),
        }
    }

    /// Canonical `.layer.ron` text.
    ///
    /// # Errors
    /// When serialization fails.
    pub fn to_ron_string(&self) -> Result<String, SceneFormatError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("LayerFile({rest}");
        }
        Ok(text)
    }
}

/// Parses `.layer.ron` text (exposed for the fuzz target: random bytes give `Err`, never a
/// panic). A newer format or scene version is `unsupported_version`; a layer name that is not a
/// valid layer name is `level_parse_error`.
///
/// # Errors
/// See above.
pub fn parse_layer_file(path: &Path, text: &str) -> Result<LayerFile, LevelDirError> {
    let unsupported = |found: u32, supported: u32| LevelDirError::UnsupportedVersion {
        path: path.to_path_buf(),
        found,
        supported,
    };
    let file: LayerFile = match ron::from_str(text) {
        Ok(file) => file,
        Err(err) => {
            if let Ok(version) = regia_core::version::peek_ron_version(text)
                && version > LAYER_FILE_FORMAT_VERSION
            {
                return Err(unsupported(version, LAYER_FILE_FORMAT_VERSION));
            }
            return Err(LevelDirError::Parse {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
        }
    };
    if file.format_version == 0 || file.format_version > LAYER_FILE_FORMAT_VERSION {
        return Err(unsupported(file.format_version, LAYER_FILE_FORMAT_VERSION));
    }
    if file.scene_format_version == 0 || file.scene_format_version > SCENE_FORMAT_VERSION {
        return Err(unsupported(file.scene_format_version, SCENE_FORMAT_VERSION));
    }
    if !valid_layer_name(&file.layer) {
        return Err(LevelDirError::Parse {
            path: path.to_path_buf(),
            message: format!("`{}` is not a layer name", file.layer),
        });
    }
    Ok(file)
}

/// The stem every layer file of the level at `level` (either spelling) starts with: the level
/// path without `.level.ron` / `.level.d`.
fn level_stem(level: &Path) -> PathBuf {
    let (file, _) = level_paths(level);
    let text = file.to_string_lossy();
    PathBuf::from(
        text.strip_suffix(crate::level_dir::LEVEL_FILE_SUFFIX)
            .unwrap_or(&text)
            .to_owned(),
    )
}

/// The layer file of `layer` for the level at `level` (either spelling):
/// `<dir>/<stem>.<layer>.layer.ron`.
#[must_use]
pub fn layer_file_path(level: &Path, layer: &str) -> PathBuf {
    let stem = level_stem(level);
    PathBuf::from(format!("{}.{layer}{LAYER_FILE_SUFFIX}", stem.to_string_lossy()))
}

/// The layer named by a file name of the level whose file stem is `stem`, if it is one of its
/// layer files.
fn layer_of_name<'a>(stem: &str, name: &'a str) -> Option<&'a str> {
    let layer = name
        .strip_prefix(stem)?
        .strip_prefix('.')?
        .strip_suffix(LAYER_FILE_SUFFIX)?;
    valid_layer_name(layer).then_some(layer)
}

/// The layer files of the level at `level` (either spelling), sorted by layer name.
///
/// # Errors
/// `level_io_error` when the level's directory exists but cannot be listed.
pub fn list_layer_files(level: &Path) -> Result<Vec<(String, PathBuf)>, LevelDirError> {
    let stem = level_stem(level);
    let Some(stem_name) = stem.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(Vec::new());
    };
    let dir = match stem.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(LevelDirError::Io {
                path: dir,
                source: err,
                written: Vec::new(),
            });
        }
    };
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(layer) = layer_of_name(&stem_name, &name)
            && entry.file_type().is_ok_and(|t| t.is_file())
        {
            found.insert(layer.to_owned(), entry.path());
        }
    }
    Ok(found.into_iter().collect())
}

/// The separated layer a record moves to: the first of `separated` (by name) its membership
/// lists.
fn target_layer<'a>(record: &SceneEntity, separated: &'a BTreeSet<String>) -> Option<&'a String> {
    if separated.is_empty() {
        return None;
    }
    let membership = LayerMembership::from_record(record)?;
    separated.iter().find(|layer| membership.layers.contains(layer))
}

/// Splits `scene` (the whole level) into the level's own records and one [`LayerFile`] per layer
/// of `separated` (an empty file for a layer without members). Resources stay with the level.
#[must_use]
pub fn split_layers(
    scene: &SceneFile,
    separated: &BTreeSet<String>,
) -> (SceneFile, BTreeMap<String, LayerFile>) {
    let mut files: BTreeMap<String, LayerFile> = separated
        .iter()
        .map(|layer| (layer.clone(), LayerFile::new(layer.clone())))
        .collect();
    let mut rest = SceneFile {
        format_version: scene.format_version,
        entities: Vec::new(),
        resources: scene.resources.clone(),
    };
    for (index, record) in scene.entities.iter().enumerate() {
        match target_layer(record, separated).and_then(|layer| files.get_mut(layer)) {
            Some(file) => file.entities.push(LayerRecord {
                at: u32::try_from(index).unwrap_or(u32::MAX),
                entity: record.clone(),
            }),
            None => rest.entities.push(record.clone()),
        }
    }
    (rest, files)
}

/// Folds layer files into `scene` (the level's own records): each record goes back to its `at`
/// position (clamped to the end), in ascending `at` order, so merging an unchanged split gives
/// the original record order. A layer file at an older scene version is migrated first.
///
/// # Errors
/// `duplicate_entity` when a guid is in the level and a layer file (or two layer files);
/// `level_invalid` when a migration or the merged scene's validation fails.
pub fn merge_layer_files(
    scene: &mut SceneFile,
    files: Vec<(PathBuf, LayerFile)>,
) -> Result<(), LevelDirError> {
    if files.is_empty() {
        return Ok(());
    }
    let mut seen: BTreeMap<regia_core::id::Guid, PathBuf> = BTreeMap::new();
    let level_guids: HashSet<regia_core::id::Guid> = scene.entities.iter().map(|e| e.guid).collect();
    let mut moved: Vec<(u32, SceneEntity)> = Vec::new();
    for (path, file) in files {
        let at: Vec<u32> = file.entities.iter().map(|r| r.at).collect();
        let mut part = SceneFile {
            format_version: file.scene_format_version,
            entities: file.entities.into_iter().map(|r| r.entity).collect(),
            resources: BTreeMap::new(),
        };
        // The same normalization (and, for an older file, migration) a level's records get.
        part.normalize_and_migrate()?;
        for (at, entity) in at.into_iter().zip(part.entities) {
            if level_guids.contains(&entity.guid) {
                return Err(LevelDirError::DuplicateEntity {
                    guid: entity.guid,
                    first: PathBuf::from("<level>"),
                    second: path,
                });
            }
            if let Some(first) = seen.insert(entity.guid, path.clone()) {
                return Err(LevelDirError::DuplicateEntity {
                    guid: entity.guid,
                    first,
                    second: path,
                });
            }
            moved.push((at, entity));
        }
    }
    // Stable: equal positions (records added in two files) keep the file order.
    moved.sort_by_key(|(at, _)| *at);
    for (at, entity) in moved {
        let index = (at as usize).min(scene.entities.len());
        scene.entities.insert(index, entity);
    }
    scene.validate()?;
    Ok(())
}

/// Reads and parses the layer files of the level at `level`.
///
/// # Errors
/// When a file cannot be read or parsed.
pub fn read_layer_files(level: &Path) -> Result<Vec<(PathBuf, LayerFile)>, LevelDirError> {
    let _span = tracing::debug_span!("layer_files_read", level = %level.display()).entered();
    let mut out = Vec::new();
    for (_, path) in list_layer_files(level)? {
        let text = std::fs::read_to_string(&path).map_err(|e| LevelDirError::Io {
            path: path.clone(),
            source: e,
            written: Vec::new(),
        })?;
        let file = parse_layer_file(&path, &text)?;
        out.push((path, file));
    }
    Ok(out)
}

/// Folds the layer files of the level at `level` into `scene` ([`merge_layer_files`]).
///
/// # Errors
/// See [`read_layer_files`] and [`merge_layer_files`].
pub fn merge_layer_files_of(level: &Path, scene: &mut SceneFile) -> Result<(), LevelDirError> {
    let files = read_layer_files(level)?;
    merge_layer_files(scene, files)
}

/// What [`write_split_level`] wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerFilesWritten {
    /// Files whose bytes changed and were rewritten (the level first when it changed).
    pub written: Vec<PathBuf>,
    /// Files left alone because their bytes were already the same.
    pub unchanged: Vec<PathBuf>,
    /// Layer files deleted (layers merged back).
    pub deleted: Vec<PathBuf>,
}

/// Writes `text` to `path` unless the file already holds exactly these bytes.
fn write_if_changed(path: &Path, text: &str, out: &mut LayerFilesWritten) -> Result<(), LevelDirError> {
    if std::fs::read(path).is_ok_and(|bytes| bytes == text.as_bytes()) {
        out.unchanged.push(path.to_path_buf());
        return Ok(());
    }
    crate::level_dir::write_atomic(path, text.as_bytes()).map_err(|e| LevelDirError::Io {
        path: path.to_path_buf(),
        source: e,
        written: out.written.clone(),
    })?;
    out.written.push(path.to_path_buf());
    Ok(())
}

/// Which files [`write_split_level`] may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitTarget {
    /// The level and every layer file.
    All,
    /// Only the level file.
    Level,
    /// Only the file of this layer.
    Layer(String),
}

/// Writes `level` (canonical `.level.ron` text of the whole level, as a [`crate::level::Level`]
/// saves it) to the single-file level `path` with the members of `separated` in their layer
/// files. `target` limits what is written (a save of one open document); a file whose bytes are
/// unchanged is never rewritten. With `prune`, layer files of layers not in `separated` are
/// deleted after every write succeeded (a merge back).
///
/// # Errors
/// When the text does not parse or a write fails.
pub fn write_split_level(
    path: &Path,
    level_text: &str,
    separated: &BTreeSet<String>,
    target: &SplitTarget,
    prune: bool,
) -> Result<LayerFilesWritten, LevelDirError> {
    let _span = tracing::info_span!("layer_files_write", level = %path.display()).entered();
    let mut out = LayerFilesWritten::default();
    if separated.is_empty() {
        if matches!(target, SplitTarget::All | SplitTarget::Level) {
            write_if_changed(path, level_text, &mut out)?;
        }
    } else {
        let whole = SceneFile::from_ron_str(level_text)?;
        let (rest, files) = split_layers(&whole, separated);
        for (layer, file) in &files {
            if matches!(target, SplitTarget::All) || matches!(target, SplitTarget::Layer(l) if l == layer) {
                write_if_changed(&layer_file_path(path, layer), &file.to_ron_string()?, &mut out)?;
            }
        }
        if matches!(target, SplitTarget::All | SplitTarget::Level) {
            write_if_changed(path, &rest.to_ron_string()?, &mut out)?;
        }
    }
    if prune {
        for (layer, file) in list_layer_files(path)? {
            if !separated.contains(&layer) {
                std::fs::remove_file(&file).map_err(|e| LevelDirError::Io {
                    path: file.clone(),
                    source: e,
                    written: out.written.clone(),
                })?;
                out.deleted.push(file);
            }
        }
    }
    Ok(out)
}

/// The layers of the level at `path` stored in their own files (the names of its layer files).
///
/// # Errors
/// See [`list_layer_files`].
pub fn separated_layers(path: &Path) -> Result<BTreeSet<String>, LevelDirError> {
    Ok(list_layer_files(path)?
        .into_iter()
        .map(|(layer, _)| layer)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::id::Guid;

    fn record(n: u128, layers: &[&str]) -> String {
        let membership = if layers.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = layers.iter().map(|l| format!("\"{l}\"")).collect();
            format!(
                "\"regia_scene::layers::LayerMembership\": (layers: [{}]),",
                list.join(", ")
            )
        };
        format!(
            "(guid: \"{}\", name: \"e{n}\", components: {{ {membership} \"regia_scene::transform::Transform\": () }}),",
            Guid::from_u128(0x7792_0000_0000_4000_8000_0000_0000_0000 | n)
        )
    }

    /// A level whose records are deliberately not in guid order.
    fn level_text() -> String {
        let records = [
            record(5, &[]),
            record(1, &["props"]),
            record(4, &["lights", "props"]),
            record(2, &[]),
            record(3, &["lights"]),
        ];
        let scene = SceneFile::from_ron_str(&format!(
            "SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [{}], resources: {{}})",
            records.join("\n")
        ))
        .expect("scene");
        scene.to_ron_string().expect("text")
    }

    #[test]
    fn a_split_then_merge_gives_the_original_bytes() {
        let original = level_text();
        let scene = SceneFile::from_ron_str(&original).expect("scene");
        let separated: BTreeSet<String> = ["lights".to_owned(), "props".to_owned()].into();
        let (rest, files) = split_layers(&scene, &separated);
        assert_eq!(rest.entities.len(), 2, "two records belong to no separated layer");
        // `e4` is in both layers: it moves to the first one by name.
        assert_eq!(files["lights"].entities.len(), 2);
        assert_eq!(files["props"].entities.len(), 1);
        let mut merged = rest;
        let files: Vec<(PathBuf, LayerFile)> = files
            .into_values()
            .map(|file| {
                let text = file.to_ron_string().expect("text");
                (
                    PathBuf::from("x"),
                    parse_layer_file(Path::new("x"), &text).expect("parse"),
                )
            })
            .collect();
        merge_layer_files(&mut merged, files).expect("merge");
        assert_eq!(merged.to_ron_string().expect("text"), original);
    }

    #[test]
    fn files_on_disk_round_trip_and_the_reader_folds_them_in() {
        let dir = regia_test_utils::unique_temp_dir("layer-file-disk");
        let path = dir.join("forest.level.ron");
        let original = level_text();
        std::fs::write(&path, &original).expect("level");
        let before = crate::level_dir::read_level_as_stored(&path).expect("read");

        let separated: BTreeSet<String> = ["props".to_owned()].into();
        let report =
            write_split_level(&path, &original, &separated, &SplitTarget::All, false).expect("split");
        assert_eq!(report.written.len(), 2, "{report:?}");
        assert!(layer_file_path(&path, "props").is_file());
        assert_eq!(separated_layers(&path).expect("list"), separated);
        let level_only = std::fs::read_to_string(&path).expect("level");
        assert!(!level_only.contains("\"e1\""), "the member left the level file");

        // The reader (runtime, cook, editor) sees the merged level.
        let after = crate::level_dir::read_level_as_stored(&path).expect("read");
        assert_eq!(after, before);
        assert_eq!(after.to_ron_string().expect("text"), original);

        // Merge back: no separated layers, prune the old file.
        let report =
            write_split_level(&path, &original, &BTreeSet::new(), &SplitTarget::All, true).expect("merge");
        assert_eq!(report.deleted, vec![layer_file_path(&path, "props")]);
        assert_eq!(std::fs::read_to_string(&path).expect("level"), original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_of_one_layer_writes_only_its_file() {
        let dir = regia_test_utils::unique_temp_dir("layer-file-target");
        let path = dir.join("town.level.ron");
        let original = level_text();
        std::fs::write(&path, &original).expect("level");
        let separated: BTreeSet<String> = ["lights".to_owned(), "props".to_owned()].into();
        write_split_level(&path, &original, &separated, &SplitTarget::All, false).expect("split");
        let level_bytes = std::fs::read(&path).expect("level");
        let props_bytes = std::fs::read(layer_file_path(&path, "props")).expect("props");

        // Rename `e3` (a `lights` member) and save the `lights` document only.
        let edited = original.replace("\"e3\"", "\"e3_renamed\"");
        let report = write_split_level(
            &path,
            &edited,
            &separated,
            &SplitTarget::Layer("lights".into()),
            false,
        )
        .expect("save");
        assert_eq!(report.written, vec![layer_file_path(&path, "lights")]);
        assert_eq!(std::fs::read(&path).expect("level"), level_bytes);
        assert_eq!(
            std::fs::read(layer_file_path(&path, "props")).expect("props"),
            props_bytes
        );
        let lights = std::fs::read_to_string(layer_file_path(&path, "lights")).expect("lights");
        assert!(lights.contains("e3_renamed"), "{lights}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opens the level at `path` the way the runtime does (read the source, put it in the
    /// library, open it) and answers what spawned: `(guid, name, layers)` sorted by guid.
    fn spawned(path: &Path) -> Vec<(Guid, String, Vec<String>)> {
        use crate::level::{Level, LevelId, LevelLibrary, LoadMode};
        let mut app = regia_app::App::new();
        app.add_plugins(crate::ScenePlugin);
        let id = LevelId(Guid::from_u128(0x7792));
        let scene = crate::level_dir::read_level_as_stored(path).expect("read");
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(id, scene));
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let world = app.world_mut();
        let mut query = world.query::<(
            &crate::components::EntityGuid,
            &crate::components::Name,
            Option<&LayerMembership>,
        )>();
        let mut out: Vec<(Guid, String, Vec<String>)> = query
            .iter(world)
            .map(|(guid, name, membership)| {
                (
                    guid.0,
                    name.as_str().to_owned(),
                    membership.map(|m| m.layers.clone()).unwrap_or_default(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn the_runtime_opens_a_level_with_a_layer_file_like_the_merged_level() {
        let dir = regia_test_utils::unique_temp_dir("layer-file-runtime");
        let merged = dir.join("merged.level.ron");
        let split = dir.join("split.level.ron");
        let original = level_text();
        std::fs::write(&merged, &original).expect("merged");
        let separated: BTreeSet<String> = ["props".to_owned()].into();
        write_split_level(&split, &original, &separated, &SplitTarget::All, false).expect("split");
        let from_merged = spawned(&merged);
        assert_eq!(from_merged.len(), 5, "{from_merged:?}");
        assert_eq!(spawned(&split), from_merged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_only_match_the_level_s_own_layer_files() {
        assert_eq!(layer_of_name("forest", "forest.props.layer.ron"), Some("props"));
        assert_eq!(layer_of_name("forest", "forest.a.b.layer.ron"), None);
        assert_eq!(layer_of_name("forest", "forestx.props.layer.ron"), None);
        assert_eq!(layer_of_name("forest", "forest.Props.layer.ron"), None);
        assert_eq!(
            layer_file_path(Path::new("a/forest.level.d"), "props"),
            PathBuf::from("a/forest.props.layer.ron")
        );
    }

    #[test]
    fn duplicates_and_bad_files_are_refused() {
        let scene = SceneFile::from_ron_str(&level_text()).expect("scene");
        let separated: BTreeSet<String> = ["props".to_owned()].into();
        let (_, files) = split_layers(&scene, &separated);
        let mut whole = scene.clone();
        let err = merge_layer_files(&mut whole, vec![(PathBuf::from("p"), files["props"].clone())])
            .expect_err("guid twice");
        assert_eq!(err.reason(), "duplicate_entity");

        let newer = "LayerFile(format_version: 99, scene_format_version: 1, layer: \"a\", entities: [])";
        assert_eq!(
            parse_layer_file(Path::new("n"), newer)
                .expect_err("newer")
                .reason(),
            "unsupported_version"
        );
        let bad_name = format!(
            "LayerFile(format_version: 1, scene_format_version: {SCENE_FORMAT_VERSION}, layer: \"A.b\", entities: [])"
        );
        assert_eq!(
            parse_layer_file(Path::new("n"), &bad_name)
                .expect_err("name")
                .reason(),
            "level_parse_error"
        );
        assert!(parse_layer_file(Path::new("n"), "\u{0}garbage").is_err());
    }
}
