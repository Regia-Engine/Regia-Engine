// SPDX-License-Identifier: MPL-2.0
//! Sub-asset ids across re-imports (`docs/specs/12-assets.md` § 2, ADR-2428).
//!
//! A sub-asset id is `hash(parent_id, sub_asset_path)` the first time it is seen, and the `.meta`
//! stores it. Every later import takes the stored id for the same path ([`sub_id`] inside
//! [`with_ids`]), so editing a `.meta` entry's `path` to a new name, keeping its `id`, re-maps a
//! renamed node or material by hand. When a glTF node, material, clip, skeleton, LOD group or
//! variant set is renamed between imports, [`plan`] matches the vanished entries with the new ones
//! by these rules, and the renamed sub-asset keeps its old id:
//!
//! 1. Only entries of one *group* match: same kind (the first path segment), same tail (for a
//!    mesh, the `primitive_<i>` segment) and same cooked type. Index-named and derived sub-assets
//!    (`texture/<i>`, `model`, `prefab/sockets`, collision sets, meshlets, hulls, fracture
//!    chunks) never match: their paths do not carry an authored name.
//! 2. **Content**: a vanished and a new entry whose cooked payloads hash the same (BLAKE3), when no
//!    other entry of the group has that hash.
//! 3. **Sole rename**: after rule 2, a group left with exactly one vanished and one new entry.
//!
//! A vanished entry no rule matches is reported as `asset_subasset_missing` with the project files
//! that still name its id or its `file#path`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;

use regia_asset::{AssetId, SubAssetEntry, cooked_file_name};
use serde::Serialize;

/// The warning code of a sub-asset that vanished from its source.
pub const SUBASSET_MISSING_CODE: &str = "asset_subasset_missing";

/// The info code of a renamed sub-asset that kept its id.
pub const SUBASSET_REMAPPED_CODE: &str = "asset_subasset_remapped";

/// Most referrers listed per missing sub-asset.
pub const MAX_REFERRERS: usize = 50;

/// Largest project file searched for references (bytes).
const MAX_SCANNED_FILE: u64 = 8 << 20;

/// The ids one import gives its sub-assets: `path -> id` for the paths it already knows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubAssetIds {
    parent: AssetId,
    ids: BTreeMap<String, AssetId>,
}

impl SubAssetIds {
    /// The stored ids of `entries` (a `.meta`'s `sub_assets`) under `parent`.
    #[must_use]
    pub fn from_entries(parent: AssetId, entries: &[SubAssetEntry]) -> Self {
        Self {
            parent,
            ids: entries.iter().map(|e| (e.path.clone(), e.id)).collect(),
        }
    }

    /// Give `path` the id `id`.
    pub fn insert(&mut self, path: impl Into<String>, id: AssetId) {
        self.ids.insert(path.into(), id);
    }

    /// The id stored for `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<AssetId> {
        self.ids.get(path).copied()
    }
}

thread_local! {
    static ACTIVE: RefCell<Vec<Rc<SubAssetIds>>> = const { RefCell::new(Vec::new()) };
}

/// Pops the table pushed by [`with_ids`], also when the import panics.
struct Pop;

impl Drop for Pop {
    fn drop(&mut self) {
        ACTIVE.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// Run `f` (one import on this thread) with `ids` as the stored sub-asset ids of its parent.
pub fn with_ids<R>(ids: SubAssetIds, f: impl FnOnce() -> R) -> R {
    ACTIVE.with(|stack| stack.borrow_mut().push(Rc::new(ids)));
    let _pop = Pop;
    f()
}

/// The id of sub-asset `path` of `parent`: the stored one when the running import has it (see
/// [`with_ids`]), else the derived `hash(parent, path)` ([`AssetId::sub_asset`]).
#[must_use]
pub fn sub_id(parent: AssetId, path: &str) -> AssetId {
    ACTIVE
        .with(|stack| {
            stack
                .borrow()
                .iter()
                .rev()
                .find(|t| t.parent == parent)
                .and_then(|t| t.get(path))
        })
        .unwrap_or_else(|| AssetId::sub_asset(parent, path))
}

/// Which rule matched a renamed sub-asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchRule {
    /// Same cooked payload hash.
    Content,
    /// The only vanished and the only new entry of their group.
    SoleRename,
}

impl MatchRule {
    /// Lowercase label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::SoleRename => "sole_rename",
        }
    }
}

/// A renamed sub-asset that keeps its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Remap {
    /// The path the previous import gave it.
    pub old_path: String,
    /// The path this import gives it.
    pub new_path: String,
    /// The id it keeps.
    pub id: AssetId,
    /// The id the first import gave the new path (its cooked file is removed after the re-run).
    pub derived: AssetId,
    /// Which rule matched.
    pub rule: MatchRule,
}

/// What [`plan`] decided for one source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RemapPlan {
    /// Renamed sub-assets matched to their old ids.
    pub remapped: Vec<Remap>,
    /// Vanished sub-assets no rule matched (only those that carry an authored name or index).
    pub missing: Vec<SubAssetEntry>,
}

/// `(kind, tail)` of a path whose name may be renamed, or `None` for index-named and derived
/// sub-assets.
fn group_of(path: &str) -> Option<(&str, &str)> {
    let mut parts = path.split('/');
    let kind = parts.next()?;
    let _name = parts.next().filter(|n| !n.is_empty())?;
    let rest: Vec<&str> = parts.collect();
    match (kind, rest.as_slice()) {
        ("mesh", [tail]) if tail.starts_with("primitive_") => Some((kind, tail)),
        ("material" | "skeleton" | "clip" | "nodeclip" | "lod_group" | "material_variants", []) => {
            Some((kind, ""))
        }
        _ => None,
    }
}

/// Whether a vanished `path` is worth a warning: a re-mappable one, or a texture (an image
/// removed from the file).
fn warns(path: &str) -> bool {
    group_of(path).is_some() || (path.starts_with("texture/") && !path.starts_with("texture/orm/"))
}

/// Match the vanished entries of `old` (the `.meta` before the import) with the new entries of
/// `new` (the first import's result) by the module rules. `fingerprint` hashes an entry's cooked
/// payload, `None` when it is not on disk.
pub fn plan(
    old: &[SubAssetEntry],
    new: &[SubAssetEntry],
    mut fingerprint: impl FnMut(&SubAssetEntry) -> Option<[u8; 32]>,
) -> RemapPlan {
    let new_paths: BTreeSet<&str> = new.iter().map(|e| e.path.as_str()).collect();
    let old_paths: BTreeSet<&str> = old.iter().map(|e| e.path.as_str()).collect();
    let kept_ids: BTreeSet<AssetId> = new.iter().map(|e| e.id).collect();
    let vanished: Vec<&SubAssetEntry> = old
        .iter()
        .filter(|e| !new_paths.contains(e.path.as_str()) && !kept_ids.contains(&e.id))
        .collect();
    let appeared: Vec<&SubAssetEntry> = new
        .iter()
        .filter(|e| !old_paths.contains(e.path.as_str()))
        .collect();
    // A `.meta` written before the crate rename names the legacy type path (ADR-1429).
    type Key<'a> = (&'a str, &'a str, String);
    let mut groups: BTreeMap<Key<'_>, (Vec<&SubAssetEntry>, Vec<&SubAssetEntry>)> = BTreeMap::new();
    for v in vanished.iter().copied() {
        if let Some((kind, tail)) = group_of(&v.path) {
            let ty = regia_core::brand::current_type_path(&v.type_path).into_owned();
            groups.entry((kind, tail, ty)).or_default().0.push(v);
        }
    }
    for a in appeared.iter().copied() {
        if let Some((kind, tail)) = group_of(&a.path) {
            let ty = regia_core::brand::current_type_path(&a.type_path).into_owned();
            groups.entry((kind, tail, ty)).or_default().1.push(a);
        }
    }
    let mut out = RemapPlan::default();
    let mut matched_old: BTreeSet<&str> = BTreeSet::new();
    for (gone, came) in groups.values() {
        if gone.is_empty() || came.is_empty() {
            continue;
        }
        let gone_fp: Vec<Option<[u8; 32]>> = gone.iter().map(|e| fingerprint(e)).collect();
        let came_fp: Vec<Option<[u8; 32]>> = came.iter().map(|e| fingerprint(e)).collect();
        let mut used_old = vec![false; gone.len()];
        let mut used_new = vec![false; came.len()];
        // Rule 2: unique content matches.
        for (i, fp) in gone_fp.iter().enumerate() {
            let Some(fp) = fp else { continue };
            let olds = gone_fp.iter().filter(|f| f.as_ref() == Some(fp)).count();
            let news: Vec<usize> = came_fp
                .iter()
                .enumerate()
                .filter(|(_, f)| f.as_ref() == Some(fp))
                .map(|(j, _)| j)
                .collect();
            if olds == 1
                && let [j] = news.as_slice()
            {
                used_old[i] = true;
                used_new[*j] = true;
                out.remapped.push(remap(gone[i], came[*j], MatchRule::Content));
            }
        }
        // Rule 3: the sole remaining pair.
        let left_old: Vec<usize> = (0..gone.len()).filter(|i| !used_old[*i]).collect();
        let left_new: Vec<usize> = (0..came.len()).filter(|j| !used_new[*j]).collect();
        if let ([i], [j]) = (left_old.as_slice(), left_new.as_slice()) {
            out.remapped
                .push(remap(gone[*i], came[*j], MatchRule::SoleRename));
        }
    }
    for r in &out.remapped {
        matched_old.insert(r.old_path.as_str());
    }
    let matched_old: BTreeSet<String> = matched_old.into_iter().map(str::to_owned).collect();
    out.missing = vanished
        .into_iter()
        .filter(|v| !matched_old.contains(&v.path) && warns(&v.path))
        .cloned()
        .collect();
    out.remapped.sort_by(|a, b| a.new_path.cmp(&b.new_path));
    out
}

fn remap(old: &SubAssetEntry, new: &SubAssetEntry, rule: MatchRule) -> Remap {
    Remap {
        old_path: old.path.clone(),
        new_path: new.path.clone(),
        id: old.id,
        derived: new.id,
        rule,
    }
}

/// BLAKE3 of the cooked payload of `id` in `cache_dir` (the header, which names the id, is left
/// out), or `None` when there is no readable cooked file.
#[must_use]
pub fn cooked_fingerprint(cache_dir: &Path, id: AssetId) -> Option<[u8; 32]> {
    let bytes = std::fs::read(cache_dir.join(cooked_file_name(id))).ok()?;
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).ok()?;
    Some(*blake3::hash(payload).as_bytes())
}

/// The project files (project-relative, sorted, at most [`MAX_REFERRERS`]) whose text names any
/// of `needles`: the RON, JSON, TOML, CSV, Lua and text files under `assets/` and `config/`,
/// `.meta` sidecars left out.
#[must_use]
pub fn referrers(project_root: &Path, needles: &[String]) -> Vec<String> {
    let _span = tracing::info_span!("subasset_referrers").entered();
    let mut out = Vec::new();
    if needles.is_empty() {
        return out;
    }
    for dir in ["assets", "config"] {
        let root = project_root.join(dir);
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let text_like = [".ron", ".json", ".toml", ".csv", ".lua", ".txt"]
                .iter()
                .any(|ext| name.ends_with(ext));
            if !text_like || name.ends_with(".meta") {
                continue;
            }
            if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_SCANNED_FILE {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if needles.iter().any(|n| text.contains(n.as_str())) {
                out.push(
                    entry
                        .path()
                        .strip_prefix(project_root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if out.len() >= MAX_REFERRERS {
                    return out;
                }
            }
        }
    }
    out
}

/// One event of a re-import, as the cook reports it and `editor.asset_subasset_changes` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubAssetChange {
    /// `asset_subasset_remapped` or `asset_subasset_missing`.
    pub code: &'static str,
    /// The source, project-relative.
    pub source: String,
    /// The sub-asset path (the new one for a re-map).
    pub path: String,
    /// The old path of a re-map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// The id kept (re-map) or lost (missing).
    pub id: AssetId,
    /// The rule of a re-map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<MatchRule>,
    /// Files that still name a missing sub-asset.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referrers: Vec<String>,
}

impl SubAssetChange {
    /// The cook warning line.
    #[must_use]
    pub fn line(&self) -> String {
        match (&self.old_path, self.rule) {
            (Some(old), Some(rule)) => format!(
                "{} {}#{} keeps the id {} of #{old} (rule {})",
                self.code,
                self.source,
                self.path,
                self.id,
                rule.label()
            ),
            _ => {
                let refs = if self.referrers.is_empty() {
                    "no project file names it".to_string()
                } else {
                    format!("referenced by: {}", self.referrers.join(", "))
                };
                format!(
                    "{} {}#{} (id {}) is gone after re-import; {refs}. Rename it back, or edit the \
                     .meta entry's path to the new name keeping its id",
                    self.code, self.source, self.path, self.id
                )
            }
        }
    }
}

/// The events of `plan` for `source` (project-relative), with the referrers of each missing entry.
#[must_use]
pub fn changes(project_root: &Path, source: &str, plan: &RemapPlan) -> Vec<SubAssetChange> {
    let mut out: Vec<SubAssetChange> = plan
        .remapped
        .iter()
        .map(|r| SubAssetChange {
            code: SUBASSET_REMAPPED_CODE,
            source: source.to_owned(),
            path: r.new_path.clone(),
            old_path: Some(r.old_path.clone()),
            id: r.id,
            rule: Some(r.rule),
            referrers: Vec::new(),
        })
        .collect();
    for m in &plan.missing {
        let needles = vec![m.id.to_string(), format!("{source}#{}", m.path)];
        out.push(SubAssetChange {
            code: SUBASSET_MISSING_CODE,
            source: source.to_owned(),
            path: m.path.clone(),
            old_path: None,
            id: m.id,
            rule: None,
            referrers: referrers(project_root, &needles),
        });
    }
    out
}

/// Where the cook keeps the latest sub-asset events (a cache next to the asset database, JSON).
#[must_use]
pub fn changes_log_path(project_root: &Path) -> std::path::PathBuf {
    project_root
        .join(".regia")
        .join("assetdb")
        .join("subasset_changes.json")
}

/// Most events kept in the log.
pub const MAX_LOGGED_CHANGES: usize = 500;

/// Append `events` to the log (newest last, the oldest dropped past [`MAX_LOGGED_CHANGES`]).
///
/// # Errors
///
/// The log cannot be written.
pub fn log_changes(project_root: &Path, events: &[SubAssetChange]) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let path = changes_log_path(project_root);
    let mut all = read_changes_log(project_root);
    all.extend(events.iter().filter_map(|e| serde_json::to_value(e).ok()));
    let excess = all.len().saturating_sub(MAX_LOGGED_CHANGES);
    all.drain(..excess);
    let text = serde_json::to_string_pretty(&all).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::cook_ledger::write_atomic(&path, text.as_bytes())
}

/// The logged events (oldest first); empty when there is no log or it does not parse.
#[must_use]
pub fn read_changes_log(project_root: &Path) -> Vec<serde_json::Value> {
    std::fs::read(changes_log_path(project_root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<serde_json::Value>>(&b).ok())
        .unwrap_or_default()
}

/// Run one import of `source` (project-relative `rel_path`) with stable sub-asset ids: the stored
/// ids of its `.meta`, then, when [`plan`] finds renamed sub-assets, once more with those re-mapped
/// to their old ids (the first run's cooked files of the derived ids are removed). The events are
/// queued as cook warnings ([`take_warnings`]) and appended to the log ([`log_changes`]). A source
/// without a `.meta` (a first import) runs once, unchanged.
pub(crate) fn with_stable_sub_ids<T, E>(
    source: &Path,
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    mut cook: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let meta_path = regia_asset::meta_path_for(source);
    let read_meta = || {
        std::fs::read(&meta_path)
            .ok()
            .and_then(|b| regia_asset::MetaSidecar::from_ron(&b).ok())
    };
    let Some(before) = read_meta() else {
        return cook();
    };
    let before_bytes = std::fs::read(&meta_path).unwrap_or_default();
    let parent = before.id;
    let table = SubAssetIds::from_entries(parent, &before.sub_assets);
    let first = with_ids(table.clone(), &mut cook)?;
    let Some(after) = read_meta().filter(|m| m.id == parent) else {
        return Ok(first);
    };
    // A mesh the `.meta` `meshes` option skips (ADR-8376) did not vanish from the source: it is
    // neither reported missing nor matched as the old name of another new mesh.
    let skipped = crate::gltf_settings::skipped_mesh_keys(after.settings.as_deref());
    let planned: Vec<SubAssetEntry> = before
        .sub_assets
        .iter()
        .filter(|e| !crate::gltf_settings::mesh_key_of(&e.path).is_some_and(|key| skipped.contains(key)))
        .cloned()
        .collect();
    let plan = plan(&planned, &after.sub_assets, |e| {
        cooked_fingerprint(cache_dir, e.id)
    });
    if plan.remapped.is_empty() && plan.missing.is_empty() {
        return Ok(first);
    }
    let _span =
        tracing::info_span!("subasset_remap", path = %rel_path, remapped = plan.remapped.len()).entered();
    let result = if plan.remapped.is_empty() {
        first
    } else {
        let mut remapped = table;
        for r in &plan.remapped {
            remapped.insert(r.new_path.clone(), r.id);
        }
        drop(first);
        let second = match with_ids(remapped, &mut cook) {
            Ok(second) => second,
            Err(e) => {
                // The first pass wrote the derived ids: put the old `.meta` back so the old ids are
                // not lost, and the next cook plans again.
                if let Err(io) = crate::cook_ledger::write_atomic(&meta_path, &before_bytes) {
                    tracing::warn!(target: "regia_asset_pipeline", path = %rel_path, "cannot restore the .meta: {io}");
                }
                return Err(e);
            }
        };
        for r in &plan.remapped {
            if r.derived != r.id {
                let _ = std::fs::remove_file(cache_dir.join(cooked_file_name(r.derived)));
            }
        }
        second
    };
    let events = changes(project_root, rel_path, &plan);
    for event in &events {
        if event.code == SUBASSET_MISSING_CODE {
            tracing::warn!(target: "regia_asset_pipeline", "{}", event.line());
        } else {
            tracing::info!(target: "regia_asset_pipeline", "{}", event.line());
        }
    }
    push_warnings(events.iter().map(SubAssetChange::line));
    if let Err(e) = log_changes(project_root, &events) {
        tracing::warn!(target: "regia_asset_pipeline", "cannot write the sub-asset log: {e}");
    }
    Ok(result)
}

thread_local! {
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Queue cook warning lines for the cook running on this thread ([`take_warnings`] drains them).
pub fn push_warnings(lines: impl IntoIterator<Item = String>) {
    PENDING.with(|p| p.borrow_mut().extend(lines));
}

/// The warning lines queued since the last call.
#[must_use]
pub fn take_warnings() -> Vec<String> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, id: AssetId, ty: &str) -> SubAssetEntry {
        SubAssetEntry {
            path: path.into(),
            id,
            type_path: ty.into(),
        }
    }

    const MESH: &str = "regia_asset::CookedMesh";
    const MAT: &str = "regia_asset::CookedStandardMaterial";

    #[test]
    fn stored_ids_win_inside_an_import_and_derived_ids_outside() {
        let parent = AssetId::new_random();
        let kept = AssetId::new_random();
        let mut ids = SubAssetIds::from_entries(parent, &[e("material/Skin", kept, MAT)]);
        ids.insert("material/Hair", kept);
        assert_eq!(
            sub_id(parent, "material/Skin"),
            AssetId::sub_asset(parent, "material/Skin")
        );
        with_ids(ids, || {
            assert_eq!(sub_id(parent, "material/Skin"), kept);
            assert_eq!(sub_id(parent, "material/Hair"), kept);
            assert_eq!(
                sub_id(parent, "material/New"),
                AssetId::sub_asset(parent, "material/New")
            );
            // Another parent's import nested on this thread keeps its own table.
            let other = AssetId::new_random();
            with_ids(SubAssetIds::from_entries(other, &[]), || {
                assert_eq!(sub_id(parent, "material/Skin"), kept);
                assert_eq!(
                    sub_id(other, "material/Skin"),
                    AssetId::sub_asset(other, "material/Skin")
                );
            });
        });
        assert_eq!(
            sub_id(parent, "material/Skin"),
            AssetId::sub_asset(parent, "material/Skin")
        );
    }

    #[test]
    fn content_then_sole_rename_rules_match_within_a_group() {
        let parent = AssetId::new_random();
        let ids: Vec<AssetId> = (0..8).map(|_| AssetId::new_random()).collect();
        let old = vec![
            e("mesh/Body/primitive_0", ids[0], MESH),
            e("mesh/Body/primitive_1", ids[1], MESH),
            e("material/Skin", ids[2], MAT),
            e("material/Cloth", ids[3], MAT),
            e("material/Metal", ids[4], MAT),
            e("texture/0", ids[5], "regia_asset::CookedTexture"),
            e("model", ids[6], "regia_asset::CookedModel"),
        ];
        let d = |p: &str| AssetId::sub_asset(parent, p);
        let new = vec![
            // The node was renamed: both primitives follow (one per group, rule 3).
            e("mesh/Hero/primitive_0", d("mesh/Hero/primitive_0"), MESH),
            e("mesh/Hero/primitive_1", d("mesh/Hero/primitive_1"), MESH),
            // Two materials renamed at once: told apart by content (rule 2).
            e("material/SkinB", d("material/SkinB"), MAT),
            e("material/ClothB", d("material/ClothB"), MAT),
            // Metal was deleted and nothing else is new in its group.
            e("model", ids[6], "regia_asset::CookedModel"),
        ];
        let fp = |entry: &SubAssetEntry| -> Option<[u8; 32]> {
            let tag = match entry.path.as_str() {
                "material/Skin" | "material/SkinB" => 1,
                "material/Cloth" | "material/ClothB" => 2,
                "material/Metal" => 3,
                _ => return None,
            };
            Some([tag; 32])
        };
        let plan = plan(&old, &new, fp);
        let got: Vec<(&str, &str, AssetId, MatchRule)> = plan
            .remapped
            .iter()
            .map(|r| (r.old_path.as_str(), r.new_path.as_str(), r.id, r.rule))
            .collect();
        assert_eq!(
            got,
            vec![
                ("material/Cloth", "material/ClothB", ids[3], MatchRule::Content),
                ("material/Skin", "material/SkinB", ids[2], MatchRule::Content),
                (
                    "mesh/Body/primitive_0",
                    "mesh/Hero/primitive_0",
                    ids[0],
                    MatchRule::SoleRename
                ),
                (
                    "mesh/Body/primitive_1",
                    "mesh/Hero/primitive_1",
                    ids[1],
                    MatchRule::SoleRename
                ),
            ]
        );
        let missing: Vec<&str> = plan.missing.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(missing, vec!["material/Metal", "texture/0"]);
    }

    #[test]
    fn ambiguous_groups_and_other_types_do_not_match() {
        let parent = AssetId::new_random();
        let a = AssetId::new_random();
        let b = AssetId::new_random();
        let old = vec![e("material/A", a, MAT), e("material/B", b, MAT)];
        let d = |p: &str| AssetId::sub_asset(parent, p);
        // Two renamed with no content to tell them apart: no guess.
        let new = vec![
            e("material/C", d("material/C"), MAT),
            e("material/D", d("material/D"), MAT),
        ];
        let plan_two = plan(&old, &new, |_| None);
        assert!(plan_two.remapped.is_empty());
        assert_eq!(plan_two.missing.len(), 2);
        // Same hash on both new entries: content cannot decide, and two remain: no guess.
        let plan_dup = plan(&old[..1], &new, |_| Some([7; 32]));
        assert!(plan_dup.remapped.is_empty());
        // A mesh never matches a material.
        let plan_type = plan(
            &old[..1],
            &[e("mesh/A/primitive_0", d("mesh/A/primitive_0"), MESH)],
            |_| None,
        );
        assert!(plan_type.remapped.is_empty());
        assert_eq!(plan_type.missing.len(), 1);
        // Nothing renamed: an empty plan.
        assert_eq!(plan(&old, &old, |_| None), RemapPlan::default());
    }

    #[test]
    fn missing_sub_assets_list_their_referrers() {
        let root = regia_test_utils::unique_temp_dir("subasset-referrers");
        std::fs::create_dir_all(root.join("assets/levels")).expect("dir");
        let id = AssetId::new_random();
        std::fs::write(
            root.join("assets/levels/intro.level.ron"),
            format!("(mesh: \"{id}\")"),
        )
        .expect("level");
        std::fs::write(
            root.join("assets/levels/menu.prefab.ron"),
            "(mesh: \"assets/hero.glb#material/Metal\")",
        )
        .expect("prefab");
        std::fs::write(root.join("assets/levels/other.level.ron"), "()").expect("other");
        std::fs::write(root.join("assets/hero.glb.meta"), format!("{id}")).expect("meta is skipped");
        let plan = RemapPlan {
            remapped: Vec::new(),
            missing: vec![e("material/Metal", id, MAT)],
        };
        let events = changes(&root, "assets/hero.glb", &plan);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, SUBASSET_MISSING_CODE);
        assert_eq!(
            events[0].referrers,
            vec!["assets/levels/intro.level.ron", "assets/levels/menu.prefab.ron"]
        );
        let line = events[0].line();
        assert!(
            line.starts_with("asset_subasset_missing assets/hero.glb#material/Metal"),
            "{line}"
        );
        assert!(line.contains("intro.level.ron"), "{line}");
        log_changes(&root, &events).expect("log");
        log_changes(&root, &events).expect("log again");
        assert_eq!(read_changes_log(&root).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
