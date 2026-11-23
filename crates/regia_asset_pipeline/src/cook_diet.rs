// SPDX-License-Identifier: MPL-2.0
//! Cook diet rules (DO27-absorb row C-04, ADR-8461): per-glob texture edge caps, a triangle ceiling
//! that drops the finest LODs of the largest meshes, and deterministic picks among asset variants.
//! Read from `Regia.toml`:
//!
//! ```toml
//! [content.diet]
//! texture_caps = [
//!     { glob = "**/props/**", max_edge = 2048 },   # the first match wins
//!     { glob = "**/damage/**", max_edge = 2048 },
//! ]
//! default_max_edge = 4096      # textures no glob matches; 0 = no cap
//! max_triangles = 1500000      # a profile's ceiling over every shipped mesh; 0 = no ceiling
//! variants = [{ glob = "props/rocks/**", pick = "seeded" }]  # or pick = "first"
//! seed = 0                     # the seeded pick's seed
//! path_aliases = [{ from = "old_tree/", to = "new_tree/" }]   # the catalog-path rule
//!
//! [profiles.mobile.diet]       # a build profile replaces any key it names
//! max_triangles = 400000
//! seed = 7
//! ```
//!
//! Where each rule acts:
//!
//! - **Texture caps** act in the cook: every texture a source cooks (a standalone image, or the
//!   material maps of a glTF, capped by the *glTF's* path) is box-filtered until its largest edge is
//!   at most the cap ([`with_texture_cap`](crate::cook_diet::with_texture_cap), applied by
//!   [`crate::TextureImporterSettings::fit`]). The cache keys on the cap of each source, so changing
//!   a cap re-cooks only the sources it covers. A profile's `[profiles.<name>.diet]` texture keys are
//!   reported but do not re-cook (the cook has no profile).
//! - **The triangle ceiling** and **the variant picks** act when a build profile's pak is written
//!   ([`plan_diet`](crate::cook_diet::plan_diet)): a mesh whose finest LOD is dropped ships its
//!   coarser levels only, and a variant that is not picked is left out of the pak.
//!
//! The ceiling counts each mesh at its finest shipped LOD. While the total is over the ceiling, the
//! mesh with the most triangles at its current level (ties: the smaller path, then the smaller id)
//! loses that level. A mesh's last LOD is never dropped, and neither is a level of a mesh with more
//! than one submesh (its submesh ranges cover LOD0 only). When no mesh can drop a level the total
//! stays over the ceiling and the report says so (`diet_ceiling_unreached`).
//!
//! A variant group is the set of files a `variants` glob matches inside one folder (a sub-asset
//! goes with its file). `first` keeps the first path in byte order; `seeded` keeps
//! `fnv1a32(folder) % members` with `seed = 0` (the rule DO27's release diet used for scatter
//! props), and mixes a non-zero seed into the hash first
//! ([`variant_index`](crate::cook_diet::variant_index)). A left-out variant that a shipped asset
//! depends on is kept and reported (`diet_variant_referenced`).
//!
//! Requirements source: DO27's release diet tool (`shell-regia/tools/pack_diet.py`, regia-absorb-4
//! `c9513623f`, requirements only, not copied): the 2048/4096 edge caps by folder, the
//! 1 500 000-triangle LOD ceiling, the hashed prop pick, and the pre-rename catalog path rule.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, CookedHeader, CookedMesh, cooked_file_name, decode_cooked_file, encode_cooked_file,
};
use serde::{Deserialize, Serialize};

/// The default triangle ceiling of a `[content.diet]` table: DO27's LOD ceiling.
pub const DEFAULT_MAX_TRIANGLES: u64 = 1_500_000;

/// The default edge cap of a `[content.diet]` table for textures no glob matches: DO27's hero cap.
pub const DEFAULT_MAX_EDGE: u32 = 4096;

/// The report [`plan_diet`]'s caller writes beside a profile's pak.
pub const DIET_REPORT_FILE: &str = "diet-report.json";

/// The version of the diet report's JSON layout.
pub const DIET_REPORT_VERSION: u32 = 1;

/// The cooked mesh type path the ceiling counts.
pub const COOKED_MESH_TYPE_PATH: &str = "regia_asset::CookedMesh";

/// One `texture_caps` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextureCap {
    /// Glob over the path under `assets/` (`*`, `**`, `?`; ASCII case ignored).
    pub glob: String,
    /// The largest texture edge, in texels.
    pub max_edge: u32,
}

/// How a variant group keeps one member.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantPick {
    /// The first member in path order.
    #[default]
    First,
    /// A member chosen by a hash of the group and the diet's seed ([`variant_index`]).
    Seeded,
}

impl VariantPick {
    /// `first` or `seeded`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Seeded => "seeded",
        }
    }
}

/// One `variants` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantRule {
    /// Glob over the path under `assets/`; its matches are grouped by folder.
    pub glob: String,
    /// How each group keeps its member.
    pub pick: VariantPick,
}

/// One `path_aliases` entry: a path starting with `from` is read as starting with `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAlias {
    /// The old prefix.
    pub from: String,
    /// The prefix that replaces it.
    pub to: String,
}

/// The diet of a project, or of one build profile ([`DietRules::for_profile`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DietRules {
    /// Edge caps by glob, in order: the first match wins.
    pub texture_caps: Vec<TextureCap>,
    /// The cap of a texture no glob matches (0 = none).
    pub default_max_edge: u32,
    /// The triangle ceiling over a profile's meshes (0 = none).
    pub max_triangles: u64,
    /// Variant groups and how each keeps a member.
    pub variants: Vec<VariantRule>,
    /// The seed of [`VariantPick::Seeded`].
    pub seed: u64,
    /// Path prefixes read as other prefixes before any glob matches (the catalog-path rule).
    pub path_aliases: Vec<PathAlias>,
}

impl Default for DietRules {
    fn default() -> Self {
        Self {
            texture_caps: Vec::new(),
            default_max_edge: DEFAULT_MAX_EDGE,
            max_triangles: DEFAULT_MAX_TRIANGLES,
            variants: Vec::new(),
            seed: 0,
            path_aliases: Vec::new(),
        }
    }
}

/// A diet that cannot be read or applied. Each error names its code (lowercase, `diet_*`).
#[derive(Debug, thiserror::Error)]
pub enum DietError {
    /// The `Regia.toml` diet table is malformed.
    #[error("{0} [diet_invalid]")]
    Invalid(String),
    /// `Regia.toml` cannot be read or parsed.
    #[error("Regia.toml: {0} [diet_manifest_unreadable]")]
    Manifest(String),
    /// A cooked file of the closure cannot be read or decoded.
    #[error("{0} [diet_cooked_unreadable]")]
    Cooked(String),
    /// A rewritten cooked file cannot be encoded.
    #[error("{0} [diet_write_failed]")]
    Write(String),
}

impl DietError {
    /// The error's code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "diet_invalid",
            Self::Manifest(_) => "diet_manifest_unreadable",
            Self::Cooked(_) => "diet_cooked_unreadable",
            Self::Write(_) => "diet_write_failed",
        }
    }
}

fn invalid(msg: impl Into<String>) -> DietError {
    DietError::Invalid(msg.into())
}

/// `path` as the rules match it: `/`-separated, without a leading `./` or `assets/`, lowercase.
fn rule_path(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    if let Some(rest) = p.strip_prefix("assets://") {
        p = rest.to_string();
    } else if let Some(rest) = p.strip_prefix("assets/") {
        p = rest.to_string();
    }
    p.to_ascii_lowercase()
}

/// The file part of an asset path (a sub-asset `file.glb#mesh0` goes with `file.glb`).
fn source_path(path: &str) -> &str {
    path.split_once('#').map_or(path, |(file, _)| file)
}

fn glob_hits(glob: &str, path: &str) -> bool {
    crate::lod_classes::glob_match(&glob.replace('\\', "/").to_ascii_lowercase(), path)
}

impl DietRules {
    /// The keys a diet table may hold.
    pub const KEYS: [&'static str; 6] = [
        "texture_caps",
        "default_max_edge",
        "max_triangles",
        "variants",
        "seed",
        "path_aliases",
    ];

    /// Applies the keys of one diet table on top of `self` (each key it names replaces).
    ///
    /// # Errors
    ///
    /// An unknown key or a value of the wrong shape ([`DietError::Invalid`]).
    pub fn apply_table(&mut self, table: &toml::Table, at: &str) -> Result<(), DietError> {
        for (key, value) in table {
            match key.as_str() {
                "texture_caps" => self.texture_caps = parse_caps(value, at)?,
                "default_max_edge" => {
                    self.default_max_edge = parse_u32(value, &format!("{at} default_max_edge"))?
                }
                "max_triangles" => self.max_triangles = parse_u64(value, &format!("{at} max_triangles"))?,
                "variants" => self.variants = parse_variants(value, at)?,
                "seed" => self.seed = parse_u64(value, &format!("{at} seed"))?,
                "path_aliases" => self.path_aliases = parse_aliases(value, at)?,
                other => {
                    return Err(invalid(format!(
                        "{at}: unknown key `{other}` (expected one of {})",
                        Self::KEYS.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    /// The diet of a parsed `Regia.toml`: `[content.diet]`, then `[profiles.<profile>.diet]` on
    /// top. `None` when neither table exists (no diet: nothing is capped, dropped or picked).
    ///
    /// # Errors
    ///
    /// A diet table that is not a table, or is malformed.
    pub fn from_manifest_table(root: &toml::Table, profile: Option<&str>) -> Result<Option<Self>, DietError> {
        let base = root
            .get("content")
            .and_then(|c| c.as_table())
            .and_then(|c| c.get("diet"));
        let over = profile.and_then(|name| {
            root.get("profiles")
                .and_then(|p| p.as_table())
                .and_then(|p| p.get(name))
                .and_then(|p| p.as_table())
                .and_then(|p| p.get("diet"))
                .map(|d| (name, d))
        });
        if base.is_none() && over.is_none() {
            return Ok(None);
        }
        let mut rules = Self::default();
        if let Some(base) = base {
            let table = base
                .as_table()
                .ok_or_else(|| invalid("[content.diet] must be a table"))?;
            rules.apply_table(table, "[content.diet]")?;
        }
        if let Some((name, over)) = over {
            let at = format!("[profiles.{name}.diet]");
            let table = over
                .as_table()
                .ok_or_else(|| invalid(format!("{at} must be a table")))?;
            rules.apply_table(table, &at)?;
        }
        Ok(Some(rules))
    }

    /// [`Self::from_manifest_table`] over `Regia.toml` text.
    ///
    /// # Errors
    ///
    /// The text is not TOML, or a diet table is malformed.
    pub fn from_manifest_toml(text: &str, profile: Option<&str>) -> Result<Option<Self>, DietError> {
        let root: toml::Table = text.parse().map_err(|e| DietError::Manifest(format!("{e}")))?;
        Self::from_manifest_table(&root, profile)
    }

    /// The project's diet from `<project_root>/Regia.toml` (`None` without the file or a table).
    ///
    /// # Errors
    ///
    /// The file cannot be read, or the diet is malformed.
    pub fn for_profile(project_root: &Path, profile: Option<&str>) -> Result<Option<Self>, DietError> {
        match std::fs::read_to_string(project_root.join("Regia.toml")) {
            Ok(text) => Self::from_manifest_toml(&text, profile),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DietError::Manifest(e.to_string())),
        }
    }

    /// `path` (under `assets/`, or with an `assets/` prefix) with the first matching
    /// `path_aliases` prefix replaced, in the form the globs match.
    #[must_use]
    pub fn alias(&self, path: &str) -> String {
        let p = rule_path(path);
        for alias in &self.path_aliases {
            let from = rule_path(&alias.from);
            if let Some(rest) = p.strip_prefix(&from) {
                return format!("{}{rest}", rule_path(&alias.to));
            }
        }
        p
    }

    /// The edge cap of a texture cooked for the source `path`, and the glob that set it (`None`
    /// for the default): the first `texture_caps` glob that matches wins, else
    /// `default_max_edge`. `None` when neither caps it.
    #[must_use]
    pub fn cap_rule(&self, path: &str) -> Option<(u32, Option<&str>)> {
        let p = self.alias(path);
        for cap in &self.texture_caps {
            if glob_hits(&cap.glob, &p) {
                return Some((cap.max_edge, Some(cap.glob.as_str())));
            }
        }
        (self.default_max_edge > 0).then_some((self.default_max_edge, None))
    }

    /// The edge cap of `path` ([`Self::cap_rule`] without the glob).
    #[must_use]
    pub fn cap_for(&self, path: &str) -> Option<u32> {
        self.cap_rule(path).map(|(edge, _)| edge)
    }

    /// The first `variants` rule whose glob matches `path`.
    #[must_use]
    pub fn variant_rule(&self, path: &str) -> Option<&VariantRule> {
        let p = self.alias(source_path(path));
        self.variants.iter().find(|v| glob_hits(&v.glob, &p))
    }
}

fn parse_u64(value: &toml::Value, at: &str) -> Result<u64, DietError> {
    value
        .as_integer()
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| invalid(format!("{at} must be a non-negative integer")))
}

fn parse_u32(value: &toml::Value, at: &str) -> Result<u32, DietError> {
    value
        .as_integer()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| invalid(format!("{at} must be an integer from 0 to {}", u32::MAX)))
}

fn entries<'a>(value: &'a toml::Value, at: &str, key: &str) -> Result<Vec<&'a toml::Table>, DietError> {
    let list = value
        .as_array()
        .ok_or_else(|| invalid(format!("{at} {key} must be a list of tables")))?;
    list.iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_table()
                .ok_or_else(|| invalid(format!("{at} {key}[{i}] must be a table")))
        })
        .collect()
}

fn string_key(table: &toml::Table, key: &str, at: &str) -> Result<String, DietError> {
    match table.get(key) {
        Some(toml::Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(invalid(format!("{at}: `{key}` must be a non-empty string"))),
    }
}

fn only_keys(table: &toml::Table, keys: &[&str], at: &str) -> Result<(), DietError> {
    match table.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(invalid(format!(
            "{at}: unknown key `{k}` (expected {})",
            keys.join(", ")
        ))),
        None => Ok(()),
    }
}

fn parse_caps(value: &toml::Value, at: &str) -> Result<Vec<TextureCap>, DietError> {
    entries(value, at, "texture_caps")?
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let here = format!("{at} texture_caps[{i}]");
            only_keys(t, &["glob", "max_edge"], &here)?;
            let glob = string_key(t, "glob", &here)?;
            let max_edge = t
                .get("max_edge")
                .ok_or_else(|| invalid(format!("{here}: `max_edge` is required")))
                .and_then(|v| parse_u32(v, &format!("{here} max_edge")))?;
            if max_edge == 0 {
                return Err(invalid(format!("{here}: `max_edge` must be at least 1")));
            }
            Ok(TextureCap { glob, max_edge })
        })
        .collect()
}

fn parse_variants(value: &toml::Value, at: &str) -> Result<Vec<VariantRule>, DietError> {
    entries(value, at, "variants")?
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let here = format!("{at} variants[{i}]");
            only_keys(t, &["glob", "pick"], &here)?;
            let glob = string_key(t, "glob", &here)?;
            let pick = match t.get("pick") {
                None => VariantPick::First,
                Some(toml::Value::String(s)) if s == "first" => VariantPick::First,
                Some(toml::Value::String(s)) if s == "seeded" => VariantPick::Seeded,
                Some(other) => {
                    return Err(invalid(format!(
                        "{here}: `pick` must be \"first\" or \"seeded\", got {other}"
                    )));
                }
            };
            Ok(VariantRule { glob, pick })
        })
        .collect()
}

fn parse_aliases(value: &toml::Value, at: &str) -> Result<Vec<PathAlias>, DietError> {
    entries(value, at, "path_aliases")?
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let here = format!("{at} path_aliases[{i}]");
            only_keys(t, &["from", "to"], &here)?;
            Ok(PathAlias {
                from: string_key(t, "from", &here)?,
                to: string_key(t, "to", &here)?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The catalog-path rule.

/// `root.join(rel)` after the first `aliases` prefix that `rel` starts with is replaced. Only a
/// prefix of the whole relative path is rewritten: a folder of the same name deeper in the path is
/// left alone.
#[must_use]
pub fn catalog_path(root: &Path, rel: &str, aliases: &[PathAlias]) -> PathBuf {
    let rel = rel.replace('\\', "/");
    for alias in aliases {
        if let Some(rest) = rel.strip_prefix(alias.from.as_str()) {
            return root.join(format!("{}{rest}", alias.to));
        }
    }
    root.join(rel)
}

/// What [`collect_catalog`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CatalogCollect {
    /// Paths (relative to the root, `/`-separated, after the aliases) of rows whose file exists,
    /// in row order without repeats.
    pub paths: Vec<String>,
    /// One note per row whose file does not exist (`stale catalog row <rel>`): it can never load,
    /// so the diet leaves it out instead of failing on it.
    pub notes: Vec<String>,
}

/// The files a catalog's rows name under `root`, each read through the catalog-path rule.
#[must_use]
pub fn collect_catalog(root: &Path, rows: &[String], aliases: &[PathAlias]) -> CatalogCollect {
    let mut out = CatalogCollect::default();
    let mut seen = BTreeSet::new();
    for rel in rows {
        let path = catalog_path(root, rel, aliases);
        if !path.is_file() {
            out.notes.push(format!("stale catalog row {rel}"));
            continue;
        }
        let shown = path
            .strip_prefix(root)
            .map_or_else(|_| rel.clone(), |p| p.to_string_lossy().replace('\\', "/"));
        if seen.insert(shown.clone()) {
            out.paths.push(shown);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Variant picks.

/// FNV-1a, 32 bits, over `bytes`.
#[must_use]
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The member a group of `len` keeps: 0 for [`VariantPick::First`]; for
/// [`VariantPick::Seeded`], `fnv1a32(key) % len` with seed 0, else `fnv1a32(seed_le ++ key) % len`.
/// `None` for an empty group.
#[must_use]
pub fn variant_index(key: &str, len: usize, pick: VariantPick, seed: u64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match pick {
        VariantPick::First => Some(0),
        VariantPick::Seeded => {
            let hash = if seed == 0 {
                fnv1a32(key.as_bytes())
            } else {
                let mut bytes = seed.to_le_bytes().to_vec();
                bytes.extend_from_slice(key.as_bytes());
                fnv1a32(&bytes)
            };
            Some(hash as usize % len)
        }
    }
}

/// One variant group and its pick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantGroup {
    /// The rule's glob.
    pub rule: String,
    /// `first` or `seeded`.
    pub pick: VariantPick,
    /// The group key: the members' folder (under `assets/`, after the aliases).
    pub group: String,
    /// Every member, in byte order of its path.
    pub members: Vec<String>,
    /// The member kept.
    pub picked: String,
    /// Members left out of the pak.
    pub left_out: Vec<String>,
    /// Members not picked but kept because a shipped asset depends on them.
    pub kept_referenced: Vec<String>,
}

/// The variant groups of `paths` (asset paths; sub-assets go with their file) under `rules`.
/// Each path joins the group of the first rule that matches it. Groups and members are sorted, so
/// the result does not depend on the order of `paths`.
#[must_use]
pub fn variant_groups(rules: &DietRules, paths: &[String]) -> Vec<VariantGroup> {
    let mut groups: BTreeMap<(usize, String), BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        let file = source_path(path);
        let Some(index) = rules
            .variants
            .iter()
            .position(|v| glob_hits(&v.glob, &rules.alias(file)))
        else {
            continue;
        };
        let aliased = rules.alias(file);
        let folder = aliased.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
        groups
            .entry((index, folder))
            .or_default()
            .insert(file.to_string());
    }
    groups
        .into_iter()
        .filter_map(|((index, group), members)| {
            let rule = &rules.variants[index];
            let members: Vec<String> = members.into_iter().collect();
            let picked_at = variant_index(&group, members.len(), rule.pick, rules.seed)?;
            let picked = members[picked_at].clone();
            let left_out = members.iter().filter(|m| **m != picked).cloned().collect();
            Some(VariantGroup {
                rule: rule.glob.clone(),
                pick: rule.pick,
                group,
                members,
                picked,
                left_out,
                kept_referenced: Vec::new(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The triangle ceiling.

/// One mesh as the ceiling sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshLods {
    /// The mesh's asset path.
    pub path: String,
    /// Its asset id (the tie-break after the path).
    pub id: AssetId,
    /// Triangles of each LOD, finest first.
    pub lod_triangles: Vec<u64>,
    /// How many of its finest levels the diet may drop (`lod_triangles.len() - 1`, or 0 for a mesh
    /// that may not lose a level).
    pub droppable: usize,
}

impl MeshLods {
    /// A mesh whose every level but the last may drop.
    #[must_use]
    pub fn new(path: impl Into<String>, id: AssetId, lod_triangles: Vec<u64>) -> Self {
        let droppable = lod_triangles.len().saturating_sub(1);
        Self {
            path: path.into(),
            id,
            lod_triangles,
            droppable,
        }
    }
}

/// The finest levels the ceiling drops from one mesh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LodDrop {
    /// The mesh's asset path.
    pub path: String,
    /// The mesh's asset id.
    pub id: String,
    /// LOD levels the cooked mesh has.
    pub levels: usize,
    /// How many of its finest levels are dropped.
    pub dropped: usize,
    /// Triangles at its finest level before.
    pub triangles_before: u64,
    /// Triangles at its finest shipped level.
    pub triangles_after: u64,
}

/// The ceiling's result over a set of meshes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LodPlan {
    /// The ceiling (0 = none).
    pub max_triangles: u64,
    /// Triangles over every mesh at its finest level.
    pub triangles_before: u64,
    /// Triangles over every mesh at its finest shipped level.
    pub triangles_after: u64,
    /// Whether the total is at most the ceiling (always with no ceiling).
    pub reached: bool,
    /// The meshes that lose levels, in path order.
    pub drops: Vec<LodDrop>,
}

/// Triangles of `mesh` at `level` (0 past its chain).
fn tris_at(mesh: &MeshLods, level: usize) -> u64 {
    mesh.lod_triangles.get(level).copied().unwrap_or(0)
}

/// Plans the ceiling over `meshes` (see the module docs).
#[must_use]
pub fn plan_lod_drops(meshes: &[MeshLods], max_triangles: u64) -> LodPlan {
    // Sorted by path then id, so ties break the same way whatever the input order.
    let mut order: Vec<&MeshLods> = meshes.iter().collect();
    order.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
    let before: u64 = order.iter().map(|m| tris_at(m, 0)).sum();
    let mut level = vec![0usize; order.len()];
    let mut total = before;
    if max_triangles > 0 {
        // A max-heap on (triangles at the current level, then the smaller path/id first).
        let mut heap: std::collections::BinaryHeap<(u64, std::cmp::Reverse<usize>)> = order
            .iter()
            .enumerate()
            .filter(|(_, m)| m.droppable.min(m.lod_triangles.len().saturating_sub(1)) > 0)
            .map(|(i, m)| (tris_at(m, 0), std::cmp::Reverse(i)))
            .collect();
        while total > max_triangles {
            let Some((tris, std::cmp::Reverse(i))) = heap.pop() else {
                break;
            };
            let mesh = order[i];
            level[i] += 1;
            let next = tris_at(mesh, level[i]);
            total = total - tris + next;
            let limit = mesh.droppable.min(mesh.lod_triangles.len().saturating_sub(1));
            if level[i] < limit {
                heap.push((next, std::cmp::Reverse(i)));
            }
        }
    }
    let drops = order
        .iter()
        .zip(&level)
        .filter(|(_, l)| **l > 0)
        .map(|(m, l)| LodDrop {
            path: m.path.clone(),
            id: m.id.to_string(),
            levels: m.lod_triangles.len(),
            dropped: *l,
            triangles_before: tris_at(m, 0),
            triangles_after: tris_at(m, *l),
        })
        .collect();
    LodPlan {
        max_triangles,
        triangles_before: before,
        triangles_after: total,
        reached: max_triangles == 0 || total <= max_triangles,
        drops,
    }
}

/// Removes the `count` finest LODs of `mesh` and compacts its index buffer to the levels it keeps.
/// Its last LOD always stays (`count` is clamped). A mesh's single submesh, which covers LOD0,
/// moves to the new finest level.
///
/// # Errors
///
/// A mesh with more than one submesh, or LOD ranges outside its index buffer.
pub fn drop_finest_lods(mesh: &mut CookedMesh, count: usize) -> Result<usize, DietError> {
    let count = count.min(mesh.lods.len().saturating_sub(1));
    if count == 0 {
        return Ok(0);
    }
    if !mesh_may_drop(mesh) {
        return Err(DietError::Write(format!(
            "a mesh with {} submeshes cannot drop LODs",
            mesh.submeshes.len()
        )));
    }
    let mut indices = Vec::new();
    let mut lods = Vec::with_capacity(mesh.lods.len() - count);
    for lod in &mesh.lods[count..] {
        let start = lod.index_byte_offset as usize;
        let range = mesh
            .indices
            .get(start..start + lod.index_byte_length as usize)
            .ok_or_else(|| DietError::Write("a LOD range is outside the index buffer".into()))?;
        let offset = u32::try_from(indices.len())
            .map_err(|_| DietError::Write("the index buffer is too large".into()))?;
        indices.extend_from_slice(range);
        lods.push(regia_asset::CookedMeshLod {
            screen_relative_size: lod.screen_relative_size,
            index_byte_offset: offset,
            index_byte_length: lod.index_byte_length,
        });
    }
    if let (Some(sub), Some(first)) = (mesh.submeshes.first_mut(), lods.first()) {
        sub.index_byte_offset = first.index_byte_offset;
        sub.index_byte_length = first.index_byte_length;
    }
    mesh.indices = indices;
    mesh.lods = lods;
    Ok(count)
}

/// Triangles of each LOD of a cooked mesh, finest first (one level for a mesh without a chain).
#[must_use]
pub fn mesh_lod_triangles(mesh: &CookedMesh) -> Vec<u64> {
    let width: u64 = match mesh.index_width {
        regia_asset::CookedIndexWidth::U16 => 2,
        regia_asset::CookedIndexWidth::U32 => 4,
    };
    if mesh.lods.is_empty() {
        return vec![mesh.indices.len() as u64 / width / 3];
    }
    mesh.lods
        .iter()
        .map(|l| u64::from(l.index_byte_length) / width / 3)
        .collect()
}

/// Whether the diet may drop levels of `mesh`: at most one submesh, and that one covering LOD0.
#[must_use]
pub fn mesh_may_drop(mesh: &CookedMesh) -> bool {
    match (mesh.submeshes.as_slice(), mesh.lods.first()) {
        ([], _) => true,
        ([only], Some(first)) => {
            only.index_byte_offset == first.index_byte_offset
                && only.index_byte_length == first.index_byte_length
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Texture caps in the cook.

thread_local! {
    static TEXTURE_CAP: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Runs `f` with `cap` as the diet's edge cap of every texture it cooks (restored afterwards).
pub fn with_texture_cap<R>(cap: Option<u32>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<u32>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEXTURE_CAP.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(TEXTURE_CAP.with(|c| c.replace(cap)));
    f()
}

/// The diet's edge cap of the texture being cooked on this thread, if any.
#[must_use]
pub fn texture_cap() -> Option<u32> {
    TEXTURE_CAP.with(Cell::get)
}

/// The cook cache key text of a source under the diet cap `cap` (empty without one, so caches
/// cooked before the diet existed stay valid).
#[must_use]
pub fn cap_key_text(cap: Option<u32>) -> String {
    cap.map_or_else(String::new, |edge| format!(";diet_max_edge={edge}"))
}

// ---------------------------------------------------------------------------------------------
// The profile plan and its report.

/// One capped texture in the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DietTexture {
    /// The texture's asset path.
    pub path: String,
    /// The cap that applies.
    pub max_edge: u32,
    /// The glob that set it (`None`: `default_max_edge`).
    pub rule: Option<String>,
    /// The shipped width.
    pub width: u32,
    /// The shipped height.
    pub height: u32,
}

/// Totals before and after the diet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DietTotals {
    /// Cooked assets.
    pub assets: usize,
    /// Cooked bytes.
    pub bytes: u64,
    /// Triangles over every mesh at its finest level (before: every mesh of the closure; after:
    /// the shipped meshes at their finest shipped level).
    pub triangles: u64,
}

/// What a diet did to one profile's pak: written as `diet-report.json` and returned by the agent
/// method `assets.diet_report`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DietReport {
    /// [`DIET_REPORT_VERSION`].
    pub version: u32,
    /// The build profile.
    pub profile: String,
    /// The cook platform.
    pub platform: String,
    /// The triangle ceiling (0 = none).
    pub max_triangles: u64,
    /// The seed of the seeded picks.
    pub seed: u64,
    /// Whether the shipped triangles are at most the ceiling.
    pub ceiling_reached: bool,
    /// Totals of the closure before the diet.
    pub before: DietTotals,
    /// Totals of the pak after it.
    pub after: DietTotals,
    /// Textures a cap applies to, in path order.
    pub textures: Vec<DietTexture>,
    /// Meshes that lose LOD levels, in path order.
    pub lods: Vec<LodDrop>,
    /// Variant groups, in rule then folder order.
    pub variants: Vec<VariantGroup>,
    /// Warnings, each starting with its code (`diet_ceiling_unreached`, `diet_mesh_pinned`,
    /// `diet_variant_referenced`, `diet_texture_over_cap`).
    pub warnings: Vec<String>,
}

/// One cooked asset a profile ships, as [`plan_diet`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DietAsset {
    /// Manifest path (`assets/…`).
    pub path: String,
    /// Asset id.
    pub id: AssetId,
    /// The primary cooked type path.
    pub type_path: String,
}

/// What a diet changes in a profile's pak.
#[derive(Clone, Debug, Default)]
pub struct DietPlan {
    /// Cooked files that replace the cache's (meshes with LODs dropped), by asset.
    pub overrides: BTreeMap<AssetId, Vec<u8>>,
    /// Assets the pak leaves out (variants not picked).
    pub dropped: BTreeSet<AssetId>,
    /// The report.
    pub report: DietReport,
}

fn read_cooked(cooked_dir: &Path, id: AssetId) -> Result<(Vec<u8>, CookedHeader, usize), DietError> {
    let path = cooked_dir.join(cooked_file_name(id));
    let bytes = std::fs::read(&path).map_err(|e| DietError::Cooked(format!("{}: {e}", path.display())))?;
    let (header, payload) =
        decode_cooked_file(&bytes).map_err(|e| DietError::Cooked(format!("{}: {e}", path.display())))?;
    let start = bytes.len() - payload.len();
    Ok((bytes, header, start))
}

fn texture_size(type_path: &str, payload: &[u8]) -> Option<(u32, u32)> {
    match type_path {
        "regia_asset::CookedTexture" => regia_asset::CookedTexture::from_postcard(payload)
            .ok()
            .map(|t| (t.width, t.height)),
        "regia_asset::CookedImage" => regia_asset::CookedImage::from_postcard(payload)
            .ok()
            .map(|i| (i.width, i.height)),
        _ => None,
    }
}

/// Plans `rules` over the cooked assets a profile ships (`assets`, read from `cooked_dir`): the
/// variants it leaves out, the meshes whose finest LODs it drops (rewritten cooked files), and the
/// report.
///
/// # Errors
///
/// A cooked file of `assets` cannot be read, or a rewritten mesh cannot be encoded.
pub fn plan_diet(
    cooked_dir: &Path,
    assets: &[DietAsset],
    rules: &DietRules,
    profile: &str,
    platform: &str,
) -> Result<DietPlan, DietError> {
    let _span = tracing::info_span!("plan_diet", profile, assets = assets.len()).entered();
    let mut sorted: Vec<&DietAsset> = assets.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
    sorted.dedup_by(|a, b| a.id == b.id);

    let mut report = DietReport {
        version: DIET_REPORT_VERSION,
        profile: profile.to_string(),
        platform: platform.to_string(),
        max_triangles: rules.max_triangles,
        seed: rules.seed,
        ..DietReport::default()
    };

    // Every cooked file once: its bytes, header and where its payload starts.
    let mut files: BTreeMap<AssetId, (Vec<u8>, CookedHeader, usize)> = BTreeMap::new();
    for asset in &sorted {
        files.insert(asset.id, read_cooked(cooked_dir, asset.id)?);
    }
    report.before.assets = sorted.len();
    report.before.bytes = files.values().map(|(b, _, _)| b.len() as u64).sum();

    // Variants: the groups, then the left-out members' assets unless a kept asset depends on one.
    let paths: Vec<String> = sorted.iter().map(|a| a.path.clone()).collect();
    let mut groups = variant_groups(rules, &paths);
    let mut dropped: BTreeSet<AssetId> = BTreeSet::new();
    for group in &mut groups {
        let left: BTreeSet<&str> = group.left_out.iter().map(String::as_str).collect();
        let ids: BTreeSet<AssetId> = sorted
            .iter()
            .filter(|a| left.contains(source_path(&a.path)))
            .map(|a| a.id)
            .collect();
        dropped.extend(ids);
    }
    // A left-out member a shipped asset depends on stays (with its whole file).
    loop {
        let mut keep: BTreeSet<String> = BTreeSet::new();
        for asset in &sorted {
            if dropped.contains(&asset.id) {
                continue;
            }
            let (_, header, _) = &files[&asset.id];
            for dep in &header.dependencies {
                if dropped.contains(dep)
                    && let Some(d) = sorted.iter().find(|a| a.id == *dep)
                {
                    keep.insert(source_path(&d.path).to_string());
                }
            }
        }
        if keep.is_empty() {
            break;
        }
        for group in &mut groups {
            let (kept, left): (Vec<String>, Vec<String>) =
                group.left_out.drain(..).partition(|m| keep.contains(m));
            for member in &kept {
                report.warnings.push(format!(
                    "diet_variant_referenced: {member} is not the pick of {} but a shipped asset depends on it; it ships",
                    group.group
                ));
            }
            group.kept_referenced.extend(kept);
            group.left_out = left;
        }
        dropped.retain(|id| {
            sorted
                .iter()
                .find(|a| a.id == *id)
                .is_none_or(|a| !keep.contains(source_path(&a.path)))
        });
    }
    report.variants = groups;

    // The ceiling over the meshes that ship.
    // `before` counts every mesh of the closure at its finest LOD, left-out variants included.
    let mut meshes = Vec::new();
    let mut decoded: BTreeMap<AssetId, CookedMesh> = BTreeMap::new();
    let mut triangles_before = 0u64;
    for asset in &sorted {
        if asset.type_path != COOKED_MESH_TYPE_PATH {
            continue;
        }
        let (bytes, _, start) = &files[&asset.id];
        let mesh = CookedMesh::from_postcard(&bytes[*start..])
            .map_err(|e| DietError::Cooked(format!("{}: {e}", asset.path)))?;
        triangles_before += mesh_lod_triangles(&mesh).first().copied().unwrap_or(0);
        if dropped.contains(&asset.id) {
            continue;
        }
        let mut entry = MeshLods::new(asset.path.clone(), asset.id, mesh_lod_triangles(&mesh));
        if !mesh_may_drop(&mesh) && entry.droppable > 0 {
            entry.droppable = 0;
            report.warnings.push(format!(
                "diet_mesh_pinned: {} has {} submeshes; the ceiling never drops its levels",
                asset.path,
                mesh.submeshes.len()
            ));
        }
        meshes.push(entry);
        decoded.insert(asset.id, mesh);
    }
    let lods = plan_lod_drops(&meshes, rules.max_triangles);
    if !lods.reached {
        report.warnings.push(format!(
            "diet_ceiling_unreached: {} triangles after every droppable level, over the ceiling of {}",
            lods.triangles_after, rules.max_triangles
        ));
    }
    let mut overrides = BTreeMap::new();
    for drop in &lods.drops {
        let Some(id) = meshes.iter().find(|m| m.path == drop.path).map(|m| m.id) else {
            continue;
        };
        let Some(mut mesh) = decoded.remove(&id) else {
            continue;
        };
        drop_finest_lods(&mut mesh, drop.dropped)?;
        let payload = mesh
            .to_postcard()
            .map_err(|e| DietError::Write(format!("{}: {e}", drop.path)))?;
        let (_, header, _) = &files[&id];
        let header = CookedHeader {
            payload_len: payload.len() as u64,
            ..header.clone()
        };
        let file = encode_cooked_file(&header, &payload)
            .map_err(|e| DietError::Write(format!("{}: {e}", drop.path)))?;
        overrides.insert(id, file);
    }
    report.ceiling_reached = lods.reached;
    report.before.triangles = triangles_before;
    report.after.triangles = lods.triangles_after;
    report.lods = lods.drops;

    // Textures a cap applies to (the cook applied it).
    for asset in sorted.iter().filter(|a| !dropped.contains(&a.id)) {
        let Some((edge, rule)) = rules.cap_rule(source_path(&asset.path)) else {
            continue;
        };
        let (bytes, _, start) = &files[&asset.id];
        let Some((width, height)) = texture_size(&asset.type_path, &bytes[*start..]) else {
            continue;
        };
        if width.max(height) > edge {
            report.warnings.push(format!(
                "diet_texture_over_cap: {} is {width}x{height}, over its cap of {edge}; re-cook (regia cook)",
                asset.path
            ));
        }
        report.textures.push(DietTexture {
            path: asset.path.clone(),
            max_edge: edge,
            rule: rule.map(str::to_string),
            width,
            height,
        });
    }

    report.after.assets = sorted.len() - dropped.len();
    report.after.bytes = sorted
        .iter()
        .filter(|a| !dropped.contains(&a.id))
        .map(|a| {
            overrides
                .get(&a.id)
                .map_or_else(|| files[&a.id].0.len() as u64, |o| o.len() as u64)
        })
        .sum();
    Ok(DietPlan {
        overrides,
        dropped,
        report,
    })
}

/// Writes `report` as pretty JSON to `path` (atomically: a temporary file, then a rename).
///
/// # Errors
///
/// The file cannot be written.
pub fn write_report(path: &Path, report: &DietReport) -> Result<(), DietError> {
    let json = serde_json::to_vec_pretty(report).map_err(|e| DietError::Write(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| DietError::Write(format!("{}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| DietError::Write(format!("{}: {e}", path.display())))
}
