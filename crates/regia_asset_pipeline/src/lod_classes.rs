// SPDX-License-Identifier: MPL-2.0
//! Screen-size thresholds of authored LOD chains from data (ADR-260, lane v0119d,
//! `docs/formats/gltf-conventions.md` § LOD thresholds).
//!
//! An authored `Stem_LOD0`…`Stem_LODn` chain takes each level's
//! [`regia_asset::LodLevel::screen_relative_size`] from, in priority order:
//!
//! 1. the `_LODn` node's glTF extras `{"lod_screen_size": x}` (inside the GLB, so a delivery is
//!    self-contained);
//! 2. the project's class table, [`LOD_CLASSES_FILE`] next to `assets/`: class name → screen
//!    size per LOD. A file picks its class with the root node's extras `{"lod_class": "battle"}`,
//!    else the first `[[assign]]` glob that matches its asset path, then an exact class-name
//!    match to its filename stem;
//! 3. the importer default ([`crate::default_lod_screen_size`]).
//!
//! A chain that used any data must be strictly decreasing; a level that is not is repaired to half
//! the previous level, with a warning. A chain that used no data uses the importer's halving
//! defaults (ADR-7869). Hostile files never panic: every size, count and length is capped, and a bad
//! entry is dropped with a warning (lookup falls back to the next source).

use std::collections::BTreeMap;
use std::path::Path;

/// Class-table file name, read from the project root (next to `assets/`).
pub const LOD_CLASSES_FILE: &str = "lod_classes.toml";
/// The class-table format version this cook reads.
pub const LOD_CLASSES_VERSION: u32 = 1;
/// Largest class table read (bytes); a larger file is ignored with a warning.
pub const MAX_LOD_CLASSES_BYTES: u64 = 64 * 1024;
/// Most classes in a table.
pub const MAX_LOD_CLASSES: usize = 256;
/// Most levels per class (and per chain).
pub const MAX_LOD_LEVELS: usize = 16;
/// Most `[[assign]]` rules in a table.
pub const MAX_LOD_ASSIGN_RULES: usize = 1024;
/// Largest screen size accepted (the bounding diameter 64 times the viewport height).
pub const MAX_LOD_SCREEN_SIZE: f32 = 64.0;
/// Smallest non-zero screen size accepted (0 is allowed on the last level only).
pub const MIN_LOD_SCREEN_SIZE: f32 = 1.0e-4;
/// Longest class name.
const MAX_NAME_LEN: usize = 64;
/// Longest glob pattern.
const MAX_GLOB_LEN: usize = 512;
/// Largest glTF JSON chunk scanned for extras.
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;

/// One `[[assign]]` rule: asset paths matching `glob` use `class`.
#[derive(Clone, Debug, PartialEq)]
pub struct LodClassRule {
    /// Glob over the project-relative asset path (`assets/units/battle/*.glb`): `*` matches
    /// within one path segment, `**` across segments, `?` one character.
    pub glob: String,
    /// Class name.
    pub class: String,
}

/// A parsed, validated [`LOD_CLASSES_FILE`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LodClassTable {
    /// Class name → screen size per LOD (LOD 0 first, strictly decreasing).
    pub classes: BTreeMap<String, Vec<f32>>,
    /// Path rules, first match wins.
    pub assign: Vec<LodClassRule>,
}

/// Whether `value` is a usable screen size for a level (`last`: the chain's final level).
fn valid_size(value: f32, last: bool) -> bool {
    value.is_finite()
        && ((MIN_LOD_SCREEN_SIZE..=MAX_LOD_SCREEN_SIZE).contains(&value) || (last && value == 0.0))
}

/// Why a class's sizes are unusable, or `None` when they are valid.
fn class_problem(sizes: &[f32]) -> Option<String> {
    if sizes.is_empty() || sizes.len() > MAX_LOD_LEVELS {
        return Some(format!("needs 1 to {MAX_LOD_LEVELS} sizes, has {}", sizes.len()));
    }
    for (i, size) in sizes.iter().enumerate() {
        if !valid_size(*size, i + 1 == sizes.len()) {
            return Some(format!(
                "LOD{i} size {size} is outside {MIN_LOD_SCREEN_SIZE}..={MAX_LOD_SCREEN_SIZE} (0 only on the last level)"
            ));
        }
    }
    if let Some(i) = sizes.windows(2).position(|w| w[1] >= w[0]) {
        return Some(format!(
            "sizes must be strictly decreasing; LOD{} {} is not below LOD{i} {}",
            i + 1,
            sizes[i + 1],
            sizes[i]
        ));
    }
    None
}

impl LodClassTable {
    /// Parses a class table. `None` when the file is unusable (not TOML, wrong version); bad
    /// classes and rules are dropped. Warnings name every problem.
    #[must_use]
    pub fn parse(text: &str) -> (Option<Self>, Vec<String>) {
        let mut warnings = Vec::new();
        if text.len() as u64 > MAX_LOD_CLASSES_BYTES {
            warnings.push(format!(
                "{LOD_CLASSES_FILE}: {} bytes is over the {MAX_LOD_CLASSES_BYTES}-byte cap; ignored",
                text.len()
            ));
            return (None, warnings);
        }
        let root: toml::Table = match text.parse() {
            Ok(root) => root,
            Err(err) => {
                warnings.push(format!("{LOD_CLASSES_FILE}: not valid TOML ({err}); ignored"));
                return (None, warnings);
            }
        };
        match root.get("version").and_then(toml::Value::as_integer) {
            Some(v) if v == i64::from(LOD_CLASSES_VERSION) => {}
            other => {
                warnings.push(format!(
                    "{LOD_CLASSES_FILE}: version {} is not {LOD_CLASSES_VERSION}; ignored",
                    other.map_or_else(|| "missing".to_string(), |v| v.to_string())
                ));
                return (None, warnings);
            }
        }
        let mut table = Self::default();
        match root.get("classes") {
            None => {}
            Some(toml::Value::Table(classes)) => {
                for (name, value) in classes {
                    if table.classes.len() >= MAX_LOD_CLASSES {
                        warnings.push(format!(
                            "{LOD_CLASSES_FILE}: more than {MAX_LOD_CLASSES} classes; the rest are ignored"
                        ));
                        break;
                    }
                    if name.is_empty() || name.len() > MAX_NAME_LEN {
                        warnings.push(format!(
                            "{LOD_CLASSES_FILE}: class name of {} bytes ignored (1..={MAX_NAME_LEN})",
                            name.len()
                        ));
                        continue;
                    }
                    let sizes: Option<Vec<f32>> = value.as_array().and_then(|items| {
                        (items.len() <= MAX_LOD_LEVELS)
                            .then(|| {
                                items
                                    .iter()
                                    .map(|item| {
                                        item.as_float()
                                            .or_else(|| item.as_integer().map(|i| i as f64))
                                            .map(|f| f as f32)
                                    })
                                    .collect::<Option<Vec<f32>>>()
                            })
                            .flatten()
                    });
                    let Some(sizes) = sizes else {
                        warnings.push(format!(
                            "{LOD_CLASSES_FILE}: class `{name}` must be an array of at most {MAX_LOD_LEVELS} numbers; ignored"
                        ));
                        continue;
                    };
                    if let Some(problem) = class_problem(&sizes) {
                        warnings.push(format!("{LOD_CLASSES_FILE}: class `{name}`: {problem}; ignored"));
                        continue;
                    }
                    table.classes.insert(name.clone(), sizes);
                }
            }
            Some(_) => warnings.push(format!("{LOD_CLASSES_FILE}: `classes` must be a table; ignored")),
        }
        match root.get("assign") {
            None => {}
            Some(toml::Value::Array(rules)) => {
                for (i, rule) in rules.iter().enumerate() {
                    if table.assign.len() >= MAX_LOD_ASSIGN_RULES {
                        warnings.push(format!(
                            "{LOD_CLASSES_FILE}: more than {MAX_LOD_ASSIGN_RULES} assign rules; the rest are ignored"
                        ));
                        break;
                    }
                    let glob = rule.get("glob").and_then(toml::Value::as_str);
                    let class = rule.get("class").and_then(toml::Value::as_str);
                    match (glob, class) {
                        (Some(glob), Some(class))
                            if !glob.is_empty()
                                && glob.len() <= MAX_GLOB_LEN
                                && !class.is_empty()
                                && class.len() <= MAX_NAME_LEN =>
                        {
                            table.assign.push(LodClassRule {
                                glob: glob.to_string(),
                                class: class.to_string(),
                            });
                        }
                        _ => warnings.push(format!(
                            "{LOD_CLASSES_FILE}: assign rule {i} needs `glob` (1..={MAX_GLOB_LEN} bytes) and `class` (1..={MAX_NAME_LEN}); ignored"
                        )),
                    }
                }
            }
            Some(_) => warnings.push(format!(
                "{LOD_CLASSES_FILE}: `assign` must be an array of tables ([[assign]]); ignored"
            )),
        }
        (Some(table), warnings)
    }

    /// Reads `<project_root>/lod_classes.toml`. `(None, [])` when the file does not exist.
    #[must_use]
    pub fn load(project_root: &Path) -> (Option<Self>, Vec<String>) {
        let path = project_root.join(LOD_CLASSES_FILE);
        let Ok(meta) = std::fs::metadata(&path) else {
            return (None, Vec::new());
        };
        if meta.len() > MAX_LOD_CLASSES_BYTES {
            return (
                None,
                vec![format!(
                    "{}: {} bytes is over the {MAX_LOD_CLASSES_BYTES}-byte cap; ignored",
                    path.display(),
                    meta.len()
                )],
            );
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(err) => (
                None,
                vec![format!("{}: unreadable ({err}); ignored", path.display())],
            ),
        }
    }

    /// The class of the asset at `asset_path` (project-relative, `/`-separated): the root node's
    /// `lod_class` extras first, else the first matching `[[assign]]` glob, then an exact class-name
    /// match to the filename stem (ADR-8054). Globs match ignoring
    /// ASCII case, so a project cooks the same on a case-insensitive volume (Windows, macOS) and
    /// on Linux.
    #[must_use]
    pub fn class_for<'a>(&'a self, asset_path: &str, root_class: Option<&'a str>) -> Option<&'a str> {
        root_class
            .or_else(|| {
                let path = asset_path.to_ascii_lowercase();
                self.assign
                    .iter()
                    .find(|rule| glob_match(&rule.glob.to_ascii_lowercase(), &path))
                    .map(|rule| rule.class.as_str())
            })
            .or_else(|| {
                let stem = Path::new(asset_path).file_stem()?.to_str()?;
                self.classes.get_key_value(stem).map(|(name, _)| name.as_str())
            })
    }

    /// Screen sizes of `class`, LOD 0 first.
    #[must_use]
    pub fn sizes(&self, class: &str) -> Option<&[f32]> {
        self.classes.get(class).map(Vec::as_slice)
    }
}

/// Glob over a `/`-separated path, for LOD class rules, `validation.ron` excludes and registry
/// roots. This delegates to [`regia_asset::import_rules::glob_match`] so every glob in the
/// pipeline agrees with the import rules (ADR-5586: `**/` matches zero or more whole folders
/// only, never a partial folder name like `myprops/`).
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    regia_asset::import_rules::glob_match(pattern, path)
}

/// LOD data a glTF carries in its extras: per-node `lod_screen_size` and the root's `lod_class`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GltfLodExtras {
    /// Node name → its valid `lod_screen_size` (invalid values are dropped with a warning).
    pub node_sizes: BTreeMap<String, f32>,
    /// The first root node's `lod_class`.
    pub root_class: Option<String>,
}

/// The JSON of a `.gltf`, or the JSON chunk of a `.glb` (bounded).
fn gltf_json(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.starts_with(b"glTF") {
        let len = bytes
            .get(12..16)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)? as usize;
        if bytes.get(16..20) != Some(b"JSON".as_slice()) || len > MAX_JSON_BYTES {
            return None;
        }
        bytes.get(20..20usize.checked_add(len)?)
    } else if bytes.len() <= MAX_JSON_BYTES {
        Some(bytes)
    } else {
        None
    }
}

impl GltfLodExtras {
    /// Reads the LOD extras of a `.gltf`/`.glb`. Unreadable JSON gives no extras (the glTF parser
    /// reports the real error).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> (Self, Vec<String>) {
        let mut out = Self::default();
        let mut warnings = Vec::new();
        let Some(json) = gltf_json(bytes) else {
            return (out, warnings);
        };
        let Ok(root) = serde_json::from_slice::<serde_json::Value>(json) else {
            return (out, warnings);
        };
        let nodes = root.get("nodes").and_then(serde_json::Value::as_array);
        for (index, node) in nodes.into_iter().flatten().enumerate() {
            let Some(value) = node.pointer("/extras/lod_screen_size") else {
                continue;
            };
            let name = node
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if crate::gltf_conventions::parse_lod_name(name).is_none() {
                warnings.push(format!(
                    "node {index} `{name}`: lod_screen_size on a node that is not `Stem_LODn`; ignored"
                ));
                continue;
            }
            let size = value.as_f64().map(|v| v as f32);
            match size {
                // 0 is accepted here; whether it is the last level is checked on the chain.
                Some(size) if size == 0.0 || valid_size(size, false) => {
                    out.node_sizes.entry(name.to_string()).or_insert(size);
                }
                _ => warnings.push(format!(
                    "node `{name}`: lod_screen_size {value} is not a number in {MIN_LOD_SCREEN_SIZE}..={MAX_LOD_SCREEN_SIZE}; ignored"
                )),
            }
        }
        let scene = root.get("scene").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
        let roots: Vec<usize> = root
            .get("scenes")
            .and_then(serde_json::Value::as_array)
            .and_then(|scenes| scenes.get(scene))
            .and_then(|s| s.get("nodes"))
            .and_then(serde_json::Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .map(|id| id as usize)
                    .collect()
            })
            .unwrap_or_default();
        for id in roots {
            let Some(value) = nodes
                .and_then(|n| n.get(id))
                .and_then(|n| n.pointer("/extras/lod_class"))
            else {
                continue;
            };
            match value.as_str() {
                Some(class) if !class.is_empty() && class.len() <= MAX_NAME_LEN => {
                    out.root_class = Some(class.to_string());
                    break;
                }
                _ => warnings.push(format!(
                    "root node {id}: lod_class must be a string of 1..={MAX_NAME_LEN} bytes; ignored"
                )),
            }
        }
        (out, warnings)
    }
}

/// What an import knows about LOD thresholds before it groups the chains.
#[derive(Clone, Debug, Default)]
pub struct LodThresholdInputs {
    /// Valid per-node sizes from the extras.
    pub node_sizes: BTreeMap<String, f32>,
    /// The class's sizes when a class was named and found.
    pub class_sizes: Option<Vec<f32>>,
    /// The class name (for warnings and the cache key).
    pub class: Option<String>,
}

impl LodThresholdInputs {
    /// Resolves the class of the glTF in `bytes` at `asset_path` against `table`. Warnings cover
    /// bad extras and a class that names nothing in the table (that file keeps the defaults).
    #[must_use]
    pub fn resolve(bytes: &[u8], asset_path: &str, table: Option<&LodClassTable>) -> (Self, Vec<String>) {
        let (extras, mut warnings) = GltfLodExtras::from_bytes(bytes);
        let mut out = Self {
            node_sizes: extras.node_sizes,
            ..Self::default()
        };
        // Without a table there is nothing to look a class up in (a plain import).
        let class = table.and_then(|table| {
            table
                .class_for(asset_path, extras.root_class.as_deref())
                .map(str::to_string)
        });
        if let Some(class) = class {
            match table.and_then(|t| t.sizes(&class)) {
                Some(sizes) => out.class_sizes = Some(sizes.to_vec()),
                None => warnings.push(format!(
                    "{asset_path}: LOD class `{class}` is not in {LOD_CLASSES_FILE}; the importer defaults apply"
                )),
            }
            out.class = Some(class);
        }
        (out, warnings)
    }

    /// Whether any data source applies (otherwise every chain uses the importer defaults).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node_sizes.is_empty() && self.class_sizes.is_none()
    }
}

/// Screen sizes for the authored levels `levels` (ascending LOD indices) of chain `stem`:
/// node extras (`node_sizes[i]`), then `class`, then the importer default. When any data was
/// used the result is strictly decreasing (a level that is not is set to half the previous one,
/// with a warning); otherwise it is exactly the importer defaults.
#[must_use]
pub fn resolve_lod_screen_sizes(
    stem: &str,
    levels: &[u32],
    node_sizes: &[Option<f32>],
    class: Option<&[f32]>,
) -> (Vec<f32>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut used_data = false;
    let mut sizes: Vec<f32> = levels
        .iter()
        .enumerate()
        .map(|(i, level)| {
            if let Some(size) = node_sizes.get(i).copied().flatten() {
                used_data = true;
                return size;
            }
            if let Some(class) = class {
                used_data = true;
                match class.get(*level as usize) {
                    Some(size) => return *size,
                    None => warnings.push(format!(
                        "{stem}_LOD{level}: the class lists {} levels; the importer default applies",
                        class.len()
                    )),
                }
            }
            crate::gltf_conventions::default_lod_screen_size(*level)
        })
        .collect();
    if !used_data {
        return (sizes, warnings);
    }
    let last = sizes.len().saturating_sub(1);
    for i in 0..sizes.len() {
        let size = sizes[i];
        let ok = valid_size(size, i == last) && (i == 0 || size < sizes[i - 1]);
        if ok {
            continue;
        }
        let repaired = if i == 0 {
            crate::gltf_conventions::default_lod_screen_size(levels[0])
        } else {
            sizes[i - 1] * 0.5
        };
        warnings.push(format!(
            "{stem}_LOD{}: screen size {size} breaks the strictly decreasing chain; using {repaired}",
            levels[i]
        ));
        sizes[i] = repaired;
    }
    (sizes, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DO27: &str = r#"
version = 1

[classes]
light = [0.9, 0.35, 0.12, 0.04]
battle = [1.2, 0.45, 0.16, 0.05]
heavy = [1.4, 0.5, 0.18, 0.06]
walker = [1.3, 0.5, 0.18, 0.06]
structure = [2.0, 0.8, 0.3, 0.1]
interior = [3.0]
cockpit = [4.0]

[[assign]]
glob = "assets/units/light/**"
class = "light"

[[assign]]
glob = "assets/units/*/battle_*.glb"
class = "battle"
"#;

    #[test]
    fn parses_the_do27_classes_and_assigns_by_glob_and_extras() {
        let (table, warnings) = LodClassTable::parse(DO27);
        assert!(warnings.is_empty(), "{warnings:?}");
        let table = table.expect("table");
        assert_eq!(table.classes.len(), 7);
        assert_eq!(table.sizes("battle"), Some([1.2, 0.45, 0.16, 0.05].as_slice()));
        assert_eq!(
            table.class_for("assets/units/light/skimmer/hull.glb", None),
            Some("light")
        );
        assert_eq!(
            table.class_for("assets/units/rem/battle_x11.glb", None),
            Some("battle")
        );
        assert_eq!(
            table.class_for("assets/units/rem/deep/battle_x11.glb", None),
            None,
            "`*` stays in a segment"
        );
        // The root node's extras beat the globs.
        assert_eq!(
            table.class_for("assets/units/light/a.glb", Some("heavy")),
            Some("heavy")
        );
    }

    #[test]
    fn globs_match_segments_and_never_blow_up() {
        assert!(glob_match("assets/**/*.glb", "assets/a/b/c.glb"));
        assert!(
            glob_match("assets/**/*.glb", "assets/c.glb"),
            "`**/` matches no directory"
        );
        assert!(!glob_match("assets/*.glb", "assets/a/c.glb"));
        assert!(glob_match("assets/?.glb", "assets/c.glb"));
        assert!(!glob_match("assets/?.glb", "assets/cc.glb"));
        assert!(glob_match("**", "anything/at/all"));
        assert!(!glob_match("a", "b"));
        // A pattern built to backtrack exponentially stays linear.
        let pattern = "*a".repeat(200);
        let path = "a".repeat(4000) + "b";
        assert!(!glob_match(&pattern, &path));
        assert!(!glob_match(&"x".repeat(600), "x"), "over-long pattern");
    }

    /// fixrev2-asset-k: this glob delegates to `regia_asset::import_rules::glob_match`
    /// (ADR-5586), so `**/` matches whole folders only — the same reading a project's
    /// `import_rules.ron` gets.
    #[test]
    fn glob_double_star_slash_matches_whole_folders_only() {
        assert!(glob_match("**/props/*.glb", "assets/props/rock.glb"));
        assert!(
            !glob_match("**/props/*.glb", "assets/myprops/rock.glb"),
            "the delegate never cuts a folder name in half"
        );
    }

    #[test]
    fn hostile_tables_never_panic_and_drop_bad_entries() {
        for text in [
            "",
            "not toml = = =",
            "version = 2\n[classes]\na = [1.0]",
            "version = \"1\"",
            "version = 1\nclasses = 5",
            "version = 1\nassign = \"x\"",
        ] {
            let (table, warnings) = LodClassTable::parse(text);
            assert!(table.is_none() || !warnings.is_empty(), "{text}");
        }
        let (table, warnings) = LodClassTable::parse(
            r#"
version = 1
[classes]
nan = [1.0, nan]
negative = [1.0, -0.5]
rising = [0.5, 0.7]
flat = [0.5, 0.5]
huge = [1.0e30, 0.5]
zero_mid = [1.0, 0.0, 0.1]
too_long = [16.0, 15.0, 14.0, 13.0, 12.0, 11.0, 10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0, 0.5]
strings = ["a"]
empty = []
good = [1.0, 0.25, 0.0]
ints = [2, 1]
[[assign]]
glob = ""
class = "good"
[[assign]]
class = "good"
"#,
        );
        let table = table.expect("the file is usable");
        assert_eq!(
            table.classes.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["good", "ints"]
        );
        assert!(table.assign.is_empty());
        assert_eq!(warnings.len(), 11, "{warnings:#?}");
        let big = "#".repeat(MAX_LOD_CLASSES_BYTES as usize + 1);
        assert!(LodClassTable::parse(&big).0.is_none());
    }

    #[test]
    fn missing_file_is_silent_and_a_bad_file_warns() {
        let dir = std::env::temp_dir().join(format!("regia-lod-classes-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(LOD_CLASSES_FILE));
        assert_eq!(LodClassTable::load(&dir), (None, Vec::new()));
        std::fs::write(
            dir.join(LOD_CLASSES_FILE),
            "version = 1\n[classes]\nbattle = [1.0, 0.5]\n",
        )
        .expect("write");
        let (table, warnings) = LodClassTable::load(&dir);
        assert!(warnings.is_empty());
        assert_eq!(
            table.and_then(|t| t.sizes("battle").map(<[f32]>::to_vec)),
            Some(vec![1.0, 0.5])
        );
        std::fs::write(
            dir.join(LOD_CLASSES_FILE),
            vec![b'#'; MAX_LOD_CLASSES_BYTES as usize + 10],
        )
        .expect("write");
        let (table, warnings) = LodClassTable::load(&dir);
        assert!(table.is_none() && warnings.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn priority_is_node_extras_then_class_then_default() {
        let levels = [0, 1, 2, 3];
        let class = [1.2, 0.45, 0.16, 0.05];
        // No data: each default level halves the previous threshold (ADR-7869).
        let (sizes, warnings) = resolve_lod_screen_sizes("Hull", &levels, &[], None);
        assert_eq!(sizes, vec![1.0, 0.5, 0.25, 0.125]);
        assert!(warnings.is_empty());
        // The class.
        let (sizes, warnings) = resolve_lod_screen_sizes("Hull", &levels, &[], Some(&class));
        assert_eq!(sizes, class.to_vec());
        assert!(warnings.is_empty());
        // A node value beats the class on its level only.
        let node = [None, Some(0.6), None, None];
        let (sizes, _) = resolve_lod_screen_sizes("Hull", &levels, &node, Some(&class));
        assert_eq!(sizes, vec![1.2, 0.6, 0.16, 0.05]);
        // Node values without a class fill over the defaults, and the chain is repaired.
        let node = [None, None, None, Some(0.1)];
        let (sizes, warnings) = resolve_lod_screen_sizes("Hull", &levels, &node, None);
        assert_eq!(sizes, vec![1.0, 0.5, 0.25, 0.1]);
        assert!(warnings.is_empty());
        let node = [None, None, Some(0.2), None];
        let (sizes, warnings) = resolve_lod_screen_sizes("Hull", &levels, &node, None);
        assert_eq!(
            sizes,
            vec![1.0, 0.5, 0.2, 0.125],
            "LOD3's default is already below LOD2"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // A class shorter than the chain: defaults past its end, with a warning, repaired.
        let (sizes, warnings) = resolve_lod_screen_sizes("Hull", &levels, &[], Some(&[3.0]));
        assert_eq!(sizes, vec![3.0, 0.5, 0.25, 0.125]);
        assert_eq!(
            warnings.len(),
            3,
            "one missing-class-size warning per fallback: {warnings:#?}"
        );
    }

    #[test]
    fn hostile_node_values_are_repaired_never_panic() {
        let levels = [0, 1, 2];
        for (node, expect) in [
            ([Some(0.2), Some(0.5), Some(0.9)], vec![0.2, 0.1, 0.05]),
            ([Some(0.0), None, None], vec![1.0, 0.5, 0.25]),
            ([None, Some(0.0), Some(0.0)], vec![1.0, 0.5, 0.0]),
            (
                [Some(f32::NAN), Some(f32::INFINITY), Some(-1.0)],
                vec![1.0, 0.5, 0.25],
            ),
            ([Some(1.0e30), None, None], vec![1.0, 0.5, 0.25]),
        ] {
            let (sizes, _) = resolve_lod_screen_sizes("Hull", &levels, &node, None);
            assert_eq!(sizes, expect, "{node:?}");
            assert!(sizes.windows(2).all(|w| w[1] < w[0]), "{sizes:?}");
        }
    }

    fn glb(json: &str) -> Vec<u8> {
        let mut json = json.as_bytes().to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(20 + json.len() as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json);
        out
    }

    #[test]
    fn extras_are_read_from_lod_nodes_and_the_root() {
        let bytes = glb(
            r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[
            {"name":"Root","children":[1,2,3,4],"extras":{"lod_class":"battle"}},
            {"name":"Hull_LOD0","extras":{"lod_screen_size":1.5}},
            {"name":"Hull_LOD1","extras":{"lod_screen_size":"big"}},
            {"name":"Hull_LOD2","extras":{"lod_screen_size":-3}},
            {"name":"Turret","extras":{"lod_screen_size":0.4}}]}"#,
        );
        let (extras, warnings) = GltfLodExtras::from_bytes(&bytes);
        assert_eq!(extras.root_class.as_deref(), Some("battle"));
        assert_eq!(extras.node_sizes.len(), 1);
        assert_eq!(extras.node_sizes.get("Hull_LOD0"), Some(&1.5));
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        // Truncated or lying GLB headers give nothing, not a panic.
        for cut in [0, 4, 12, 19, 25, bytes.len() - 1] {
            let _ = GltfLodExtras::from_bytes(&bytes[..cut]);
        }
        let mut lying = bytes.clone();
        lying[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(GltfLodExtras::from_bytes(&lying).0, GltfLodExtras::default());
    }

    #[test]
    fn an_unknown_class_warns_and_keeps_the_defaults() {
        let (table, _) = LodClassTable::parse(DO27);
        let bytes = glb(
            r#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[0]}],"nodes":[{"name":"R","extras":{"lod_class":"dropship"}}]}"#,
        );
        let (inputs, warnings) = LodThresholdInputs::resolve(&bytes, "assets/x.glb", table.as_ref());
        assert!(inputs.is_empty());
        assert_eq!(inputs.class.as_deref(), Some("dropship"));
        assert_eq!(warnings.len(), 1);
        let (inputs, warnings) = LodThresholdInputs::resolve(
            &glb(r#"{"asset":{"version":"2.0"}}"#),
            "assets/units/light/a.glb",
            table.as_ref(),
        );
        assert!(warnings.is_empty());
        assert_eq!(inputs.class_sizes, Some(vec![0.9, 0.35, 0.12, 0.04]));
    }

    #[test]
    fn assign_globs_match_ignoring_ascii_case() {
        let (table, _) = LodClassTable::parse(DO27);
        let table = table.expect("table");
        assert_eq!(
            table.class_for("assets/Units/Light/Skimmer/Hull.GLB", None),
            Some("light")
        );
    }
}
