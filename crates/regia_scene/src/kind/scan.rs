// SPDX-License-Identifier: MPL-2.0
//! Reading a project's kind, prefab and level files for the lint and the project map (spec 101 § 6.3).
//!
//! [`scan_project`] walks `<project>/assets` in sorted order and parses every `.kind.ron`,
//! `.prefab.ron` and `.level.ron`; a prefab's asset id comes from its `.meta` sidecar (`id`), which
//! is how level files address prefabs. [`KindCatalog`] is the set of kinds a tool knows: data kinds
//! from the files, code kinds from a live [`KindRegistry`] or, offline, from
//! `.kind.ron` `marker` fields and the code kinds of an existing `project-map.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_core::id::Guid;

use super::file::{KIND_FILE_SUFFIX, KindFile};
use super::{KindInfo, KindRegistry, KindSource};
use crate::format::{SceneEntity, SceneFile};

/// Deepest directory nesting the walk follows.
const MAX_DEPTH: usize = 32;

/// Most files the walk visits.
const MAX_FILES: usize = 200_000;

/// The reflected type path of [`KindTag`](super::KindTag) in scene files.
pub const KIND_TAG_TYPE_PATH: &str = "regia_scene::kind::KindTag";

/// One parsed scene file.
#[derive(Clone, Debug)]
pub struct ScannedScene {
    /// Project-relative path with `/` separators.
    pub path: String,
    /// The asset id from the `.meta` sidecar, if any.
    pub guid: Option<Guid>,
    /// The file.
    pub file: SceneFile,
}

/// Everything [`scan_project`] read.
#[derive(Clone, Debug, Default)]
pub struct ProjectFiles {
    /// Kind declarations: path and parsed file.
    pub kinds: Vec<(String, KindFile)>,
    /// Prefabs, sorted by path.
    pub prefabs: Vec<ScannedScene>,
    /// Levels, sorted by path.
    pub levels: Vec<ScannedScene>,
    /// Files that failed to read or parse: path and error, sorted by path.
    pub errors: Vec<(String, String)>,
    /// Code kinds recorded by an earlier live run in `project-map.json`.
    pub recorded_kinds: Vec<KindInfo>,
}

impl ProjectFiles {
    /// The prefab whose asset id is `guid`.
    #[must_use]
    pub fn prefab(&self, guid: Guid) -> Option<&ScannedScene> {
        self.prefabs.iter().find(|p| p.guid == Some(guid))
    }
}

/// `path` relative to `root` with `/` separators.
#[must_use]
pub fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The `id` of a `.meta` sidecar (a guid string), if it has one.
fn meta_id(source: &Path) -> Option<Guid> {
    let mut meta = source.as_os_str().to_owned();
    meta.push(".meta");
    let text = std::fs::read_to_string(PathBuf::from(meta)).ok()?;
    let ron::Value::Map(map) = ron::from_str::<ron::Value>(&text).ok()? else {
        return None;
    };
    map.iter().find_map(|(key, value)| match (key, value) {
        (ron::Value::String(k), ron::Value::String(v)) if k == "id" => v.parse().ok(),
        _ => None,
    })
}

/// Sorted files under `dir` (hidden entries and `target` skipped).
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        // `symlink_metadata`: a link is not followed (no loops, nothing outside the project).
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(&path, depth + 1, out);
        } else if meta.is_file() {
            if out.len() >= MAX_FILES {
                return;
            }
            out.push(path);
        }
    }
}

/// Reads the project's kind, prefab and level files under `<root>/assets` (or `root` itself when it
/// has no `assets` folder), and the code kinds of `<root>/project-map.json` if present.
#[must_use]
pub fn scan_project(root: &Path) -> ProjectFiles {
    let _span = tracing::info_span!("kind_scan_project").entered();
    let assets = root.join("assets");
    let base = if assets.is_dir() {
        assets
    } else {
        root.to_path_buf()
    };
    let mut paths = Vec::new();
    walk(&base, 0, &mut paths);
    let mut files = ProjectFiles::default();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let rel = relative(root, &path);
        let is_kind = name.ends_with(KIND_FILE_SUFFIX);
        let is_prefab = name.ends_with(".prefab.ron");
        let is_level = name.ends_with(".level.ron");
        if !(is_kind || is_prefab || is_level) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                files.errors.push((rel, e.to_string()));
                continue;
            }
        };
        if is_kind {
            match KindFile::from_ron_str(&text) {
                Ok(kind) => files.kinds.push((rel, kind)),
                Err(e) => files.errors.push((rel, e.to_string())),
            }
            continue;
        }
        match SceneFile::from_ron_str(&text) {
            Ok(file) => {
                let scene = ScannedScene {
                    guid: meta_id(&path),
                    path: rel,
                    file,
                };
                if is_prefab {
                    files.prefabs.push(scene);
                } else {
                    files.levels.push(scene);
                }
            }
            Err(e) => files.errors.push((rel, e.to_string())),
        }
    }
    files.recorded_kinds = recorded_code_kinds(&root.join(super::map::MAP_JSON));
    files
}

/// A `.kind.ron` that could not be read: project-relative path, error code (`kind_parse`,
/// `kind_version_newer`, `kind_bad_param`) and message.
pub type KindFileIssue = (String, &'static str, String);

/// A cheap fingerprint of the `<root>/assets` tree for memoising [`scan_kind_files`]: the newest
/// entry mtime, the visited entry count and a digest of every entry's path, size and mtime. A file
/// edit, an add/remove and a rename each move it, and so does an edit that keeps the file's mtime
/// but not its size (`cp -p`, an mtime-preserving checkout or sync), so an unchanged fingerprint
/// means the scan would return the same declarations. (An edit that keeps both the size and the
/// mtime, such as two same-size writes inside one mtime tick of a coarse-timestamp filesystem, can
/// still hide; the farm hosts timestamp in nanoseconds.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetsStamp {
    /// The newest `modified` time over every visited entry (files and folders).
    pub newest: Option<std::time::SystemTime>,
    /// How many entries the walk visited.
    pub entries: u64,
    /// FNV-1a over every visited entry's path (relative to the assets folder), size and mtime,
    /// in walk order.
    pub digest: u64,
}

impl AssetsStamp {
    /// Folds bytes into [`digest`](Self::digest) (FNV-1a 64).
    fn fold(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.digest = (self.digest ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// The fingerprint of `<root>/assets` (nothing without an assets folder, like
/// [`scan_kind_files`]). A metadata-only walk with the same caps, skip rules and ordering, so it
/// stays cheap next to the read-and-parse scan.
#[must_use]
pub fn assets_stamp(root: &Path) -> Option<AssetsStamp> {
    let assets = root.join("assets");
    if !assets.is_dir() {
        return None;
    }
    let mut stamp = AssetsStamp {
        newest: None,
        entries: 0,
        digest: 0xcbf2_9ce4_8422_2325,
    };
    stamp_walk(&assets, &assets, 0, &mut stamp);
    Some(stamp)
}

/// Folds every entry under `dir` into `stamp` (same traversal as [`walk`], metadata only); paths
/// enter the digest relative to `assets`.
fn stamp_walk(assets: &Path, dir: &Path, depth: usize, stamp: &mut AssetsStamp) {
    if depth > MAX_DEPTH || stamp.entries >= MAX_FILES as u64 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        // `symlink_metadata`: a link is not followed (no loops, nothing outside the project).
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        stamp.entries += 1;
        let modified = meta.modified().ok();
        if let Some(modified) = modified
            && stamp.newest.is_none_or(|newest| modified > newest)
        {
            stamp.newest = Some(modified);
        }
        let rel = path.strip_prefix(assets).unwrap_or(&path);
        stamp.fold(rel.to_string_lossy().as_bytes());
        stamp.fold(&[0]);
        stamp.fold(&meta.len().to_le_bytes());
        // Seconds and nanoseconds either side of the epoch; no mtime folds a marker byte.
        match modified.map(|m| m.duration_since(std::time::UNIX_EPOCH)) {
            Some(Ok(after)) => {
                stamp.fold(&[1]);
                stamp.fold(&after.as_secs().to_le_bytes());
                stamp.fold(&after.subsec_nanos().to_le_bytes());
            }
            Some(Err(before)) => {
                stamp.fold(&[2]);
                stamp.fold(&before.duration().as_secs().to_le_bytes());
                stamp.fold(&before.duration().subsec_nanos().to_le_bytes());
            }
            None => stamp.fold(&[3]),
        }
        if meta.is_dir() {
            stamp_walk(assets, &path, depth + 1, stamp);
        }
    }
}

/// The `.kind.ron` declarations under `<root>/assets` only (no level or prefab is read, and unlike
/// [`scan_project`] a root without an `assets` folder yields nothing, so a stray working directory is
/// never walked). Returns the parsed files and the files that failed (path, error code, message),
/// both sorted by path.
#[must_use]
pub fn scan_kind_files(root: &Path) -> (Vec<(String, KindFile)>, Vec<KindFileIssue>) {
    let _span = tracing::info_span!("kind_scan_kind_files").entered();
    let assets = root.join("assets");
    if !assets.is_dir() {
        return (Vec::new(), Vec::new());
    }
    let mut paths = Vec::new();
    walk(&assets, 0, &mut paths);
    let mut kinds = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let is_kind = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(KIND_FILE_SUFFIX));
        if !is_kind {
            continue;
        }
        let rel = relative(root, &path);
        match std::fs::read_to_string(&path) {
            Ok(text) => match KindFile::from_ron_str(&text) {
                Ok(kind) => kinds.push((rel, kind)),
                Err(e) => errors.push((rel, e.code(), e.to_string())),
            },
            Err(e) => errors.push((rel, "kind_parse", e.to_string())),
        }
    }
    (kinds, errors)
}

/// The code kinds of a `project-map.json` written by an earlier live run (empty when absent or
/// unreadable).
#[must_use]
pub fn recorded_code_kinds(map_json: &Path) -> Vec<KindInfo> {
    let Ok(text) = std::fs::read_to_string(map_json) else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<super::map::ProjectMap>(&text) else {
        return Vec::new();
    };
    if map.map_version > super::map::MAP_VERSION {
        return Vec::new();
    }
    map.kinds
        .into_iter()
        .filter(|k| matches!(k.source, KindSource::Code { .. }))
        .filter_map(|k| k.to_info())
        .collect()
}

/// The kinds a tool knows, by name and by marker type path.
#[derive(Clone, Debug, Default)]
pub struct KindCatalog {
    kinds: BTreeMap<String, KindInfo>,
    markers: BTreeMap<String, String>,
}

impl KindCatalog {
    /// Adds a kind unless one of that name exists (first wins).
    pub fn add(&mut self, info: KindInfo) {
        if self.kinds.contains_key(&info.name) {
            return;
        }
        if let Some(marker) = info.source.marker() {
            self.markers.insert(marker.to_string(), info.name.clone());
        }
        self.kinds.insert(info.name.clone(), info);
    }

    /// A live registry's kinds first, then the files' declarations and recorded code kinds.
    #[must_use]
    pub fn new(registry: Option<&KindRegistry>, files: &ProjectFiles) -> Self {
        let mut catalog = Self::default();
        if let Some(registry) = registry {
            for kind in registry.kinds() {
                catalog.add(kind.clone());
            }
        }
        for (path, file) in &files.kinds {
            if let Ok(info) = file.to_info(path) {
                catalog.add(info);
            }
        }
        for kind in &files.recorded_kinds {
            catalog.add(kind.clone());
        }
        catalog
    }

    /// The kind called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&KindInfo> {
        self.kinds.get(name)
    }

    /// The kind whose marker type path is `type_path`.
    #[must_use]
    pub fn by_marker(&self, type_path: &str) -> Option<&KindInfo> {
        self.markers.get(type_path).and_then(|name| self.kinds.get(name))
    }

    /// Every kind, by name.
    pub fn iter(&self) -> impl Iterator<Item = &KindInfo> {
        self.kinds.values()
    }

    /// Whether the catalog holds no kind.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// The name in a `KindTag` raw value (`("Crate")` or `"Crate"`).
#[must_use]
pub fn tag_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .map_or(trimmed, str::trim);
    let inner = inner.strip_suffix(',').unwrap_or(inner).trim();
    ron::from_str::<String>(inner).ok()
}

/// Whether a type path is the `KindTag` (current or pre-rename spelling).
#[must_use]
pub fn is_kind_tag(type_path: &str) -> bool {
    type_path == KIND_TAG_TYPE_PATH || type_path == "ferrite_scene::kind::KindTag" // rename-keep: legacy type path
}

/// The kind a scene record carries itself: its markers (by kind name; the rest are conflicts) or its
/// tag. Returns `(name, conflicts)`.
#[must_use]
pub fn record_kind(catalog: &KindCatalog, record: &SceneEntity) -> Option<(String, Vec<String>)> {
    let mut markers: Vec<String> = record
        .components
        .keys()
        .filter_map(|path| catalog.by_marker(path).map(|k| k.name.clone()))
        .collect();
    markers.sort();
    markers.dedup();
    if !markers.is_empty() {
        let first = markers.remove(0);
        return Some((first, markers));
    }
    record
        .components
        .iter()
        .find(|(path, _)| is_kind_tag(path))
        .and_then(|(_, raw)| tag_name(raw.get_ron()))
        .map(|name| (name, Vec::new()))
}

/// The root record of a prefab file.
#[must_use]
pub fn prefab_root(file: &SceneFile) -> Option<&SceneEntity> {
    file.entities.iter().find(|e| e.parent.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_names_parse_in_both_spellings() {
        assert_eq!(tag_name("(\"Crate\")").as_deref(), Some("Crate"));
        assert_eq!(tag_name(" ( \"Crate\", ) ").as_deref(), Some("Crate"));
        assert_eq!(tag_name("\"Crate\"").as_deref(), Some("Crate"));
        assert_eq!(tag_name("(3)"), None);
    }

    /// An in-place edit that keeps the file's mtime (`cp -p`, an mtime-preserving checkout or
    /// sync) still moves the stamp when the file's size changes: each entry's own size and mtime
    /// are folded in, not only the newest mtime and the entry count. So does a file whose mtime
    /// moves back to an older time.
    #[test]
    fn an_mtime_preserving_edit_moves_the_stamp() {
        let root = regia_test_utils::unique_temp_dir("kind-assets-stamp");
        std::fs::create_dir_all(root.join("assets/kinds")).expect("mkdir");
        let file = root.join("assets/kinds/pickup.kind.ron");
        let other = root.join("assets/kinds/door.kind.ron");
        std::fs::write(&file, "(format_version: 1, name: \"Pickup\")").expect("write");
        std::fs::write(&other, "(format_version: 1, name: \"Door\")").expect("write");
        let mtime = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).expect("mtime");
        let set_mtime = |path: &Path, at: std::time::SystemTime| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_modified(at))
                .expect("set mtime");
        };
        let then = mtime(&file);
        let before = assets_stamp(&root).expect("assets");
        assert_eq!(
            assets_stamp(&root),
            Some(before),
            "an unchanged tree keeps its stamp"
        );

        std::fs::write(&file, "(format_version: 1, name: \"Pickup\", icon: \"gem\")").expect("edit");
        set_mtime(&file, then);
        assert_ne!(
            assets_stamp(&root),
            Some(before),
            "a same-mtime edit of another size"
        );

        // Back to the first stamp's contents, then an older mtime on the other file.
        std::fs::write(&file, "(format_version: 1, name: \"Pickup\")").expect("restore");
        set_mtime(&file, then);
        assert_eq!(
            assets_stamp(&root),
            Some(before),
            "the same sizes and mtimes again"
        );
        set_mtime(&other, then - std::time::Duration::from_secs(60));
        assert_ne!(assets_stamp(&root), Some(before), "an mtime moved back");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let root = Path::new("/p");
        assert_eq!(
            relative(root, &root.join("assets").join("a.level.ron")),
            "assets/a.level.ron"
        );
    }
}
