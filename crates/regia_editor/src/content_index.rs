// SPDX-License-Identifier: MPL-2.0
//! The Content Browser's asset index (`book/agents/editor-content-index.md`,
//! `book/agents/editor-asset-browser.md`).
//!
//! The panel used to walk the whole `assets/` tree twice per frame (once for the folder list,
//! once for the asset list) and lay out every row. The index walks the tree once and keeps:
//!
//! - every asset `(project-relative path, kind)` sorted, and every folder, exactly as
//!   `ContentBrowserState::assets` and `ContentBrowserState::folders` list them;
//! - each folder's direct assets, so a folder view costs its own size, not the project's;
//! - each asset's lowercase search key ([`crate::asset_search::search_key`]);
//! - the filtered view for the current folder and search, rebuilt only when the folder, the
//!   search or the index changes (the panel clones an `Arc` per frame). A search that only
//!   narrows the previous one (one more letter, one more word) filters the previous matches
//!   instead of every asset ([`crate::asset_search::AssetQuery::refines`]);
//! - the modification time of every directory it walked. Adding, removing or renaming an entry
//!   changes its directory's time, so [`ContentIndex::refresh_stale`] (one `stat` per directory,
//!   at most once a second from the panel) finds outside changes and re-lists only the
//!   directories that changed (ADR-1329). [`ContentIndex::refresh_paths`] does the same for paths
//!   the live asset watcher reports.
//!
//! The editor's own file actions (import, create, delete, reimport) invalidate it directly. The
//! panel draws only the rows in view (`ScrollArea::show_rows`).
//!
//! Searches with `tag:`, `is:`, `meta:` or plain words also read each asset's facts
//! ([`crate::asset_search::AssetFacts`]): its tags (`config/asset_tags.ron` and the automatic
//! tags), its `.meta` fields, whether its source is newer than its cooked file and whether its
//! review is pending. They are read once per asset and kept until the asset's folder is listed
//! again. `is:unreferenced` reads the project's `.ron` assets once per index generation. The
//! editor's unsaved documents and the last validator run arrive through [`WorldStates`]. The
//! Filter menu's chips ([`ContentFilter`]) add their words to the search (ADR-7759).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::asset_search::{AssetFacts, AssetQuery, AssetState, QueryError, search_key};
use crate::content::{ContentBrowserState, slash_path};
use crate::dnd::{ContentKind, kind_of_path};
use crate::viewport_agent::{Handler, str_param};

/// How often the panel checks the directory times for outside changes.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

/// One indexed asset: its project-relative path and kind.
pub type IndexedAsset = (String, ContentKind);

type ViewKey = (String, String, u64, u64);

/// The last search: its query, the index generation and world-state revision it ran on, and its
/// hits (asset indices).
type LastSearch = (AssetQuery, (u64, u64), Arc<[u32]>);

/// The states the index cannot read from disk: the editor's unsaved documents and the last
/// validator run's findings (project-relative paths).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldStates {
    /// Assets with an open document that has unsaved edits.
    pub modified: BTreeSet<String>,
    /// Assets with a finding in the last validator run.
    pub problems: BTreeSet<String>,
}

/// The facts of every asset for one (generation, world revision, references read) key.
type FactsView = ((u64, u64, bool), Arc<[AssetFacts]>);

/// The modification times of the tag rules, the provenance policy and the cooked cache folder.
type FactsInputs = [Option<SystemTime>; 3];

fn facts_inputs(root: &Path) -> FactsInputs {
    let time = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    [
        time(&root.join(regia_asset::registry::TAG_RULES_PATH)),
        time(&root.join(regia_asset_pipeline::provenance_policy::PROVENANCE_POLICY_PATH)),
        time(&regia_asset::cooked_cache_dir(
            root,
            regia_asset_pipeline::host_platform(),
        )),
    ]
}

/// The scanned `assets/` tree (a world resource shared by the panel and the agent).
#[derive(Resource, Debug)]
pub struct ContentIndex {
    root: Option<PathBuf>,
    assets: Arc<[IndexedAsset]>,
    /// Search key and file-name offset per asset (same order as `assets`).
    keys: Vec<(Box<str>, u32)>,
    /// Folder (project-relative, `assets` for the top) → its direct assets' indices, sorted.
    children: HashMap<String, Vec<u32>>,
    folders: Arc<[String]>,
    dirs: BTreeMap<PathBuf, Option<SystemTime>>,
    scanned: bool,
    view: Option<(ViewKey, Arc<[IndexedAsset]>)>,
    last_search: Option<LastSearch>,
    query_error: Option<QueryError>,
    last_check: Option<Instant>,
    /// Full walks of the tree so far.
    pub scans: u64,
    /// Directory-time checks so far.
    pub stamp_checks: u64,
    /// Filtered views built so far (a frame with an unchanged folder and search builds none).
    pub views_built: u64,
    /// Bumped by every scan and refresh; keys the filtered view.
    pub generation: u64,
    /// Directories listed (`read_dir`) by scans and refreshes together.
    pub dirs_read: u64,
    /// Refreshes that re-listed only the changed directories instead of walking the tree.
    pub refreshes: u64,
    /// Assets tested against a search query (every asset for a new search, the previous hits for
    /// a narrowed one).
    pub search_tested: u64,
    /// Searches answered by filtering the previous search's hits.
    pub searches_narrowed: u64,
    /// Disk facts per asset path (kept until the asset's folder changes; a rescan keeps the
    /// facts of folders whose time did not change, review row 16).
    facts_cache: HashMap<String, AssetFacts>,
    /// The times of the files every asset's facts depend on (tag rules, provenance policy, cooked
    /// cache folder) when the cached facts were read; a change drops them all.
    facts_inputs: Option<FactsInputs>,
    /// What each `.ron` asset mentions (kept like `facts_cache`), for `is:unreferenced`.
    mentions_cache: HashMap<String, Arc<[String]>>,
    /// `.ron` assets read for `is:unreferenced` (a rescan with no changed folder reads none).
    pub ron_reads: u64,
    facts: Option<FactsView>,
    /// The `assets/`-relative paths and asset ids the project's `.ron` assets mention, per
    /// generation.
    referenced: Option<(u64, Arc<HashSet<String>>)>,
    world_states: WorldStates,
    world_rev: u64,
    /// `.meta` sidecars read for asset facts.
    pub meta_reads: u64,
    /// Reads of every `.ron` asset for `is:unreferenced`.
    pub reference_scans: u64,
}

impl Default for ContentIndex {
    fn default() -> Self {
        Self {
            root: None,
            assets: Arc::from(Vec::new()),
            keys: Vec::new(),
            children: HashMap::new(),
            folders: Arc::from(Vec::new()),
            dirs: BTreeMap::new(),
            scanned: false,
            view: None,
            last_search: None,
            query_error: None,
            last_check: None,
            scans: 0,
            stamp_checks: 0,
            views_built: 0,
            generation: 0,
            dirs_read: 0,
            refreshes: 0,
            search_tested: 0,
            searches_narrowed: 0,
            facts_cache: HashMap::new(),
            facts_inputs: None,
            mentions_cache: HashMap::new(),
            ron_reads: 0,
            facts: None,
            referenced: None,
            world_states: WorldStates::default(),
            world_rev: 0,
            meta_reads: 0,
            reference_scans: 0,
        }
    }
}

/// What one directory listing found.
struct Listing {
    files: Vec<IndexedAsset>,
    /// `(path, project-relative path, whether its files are assets)`.
    subdirs: Vec<(PathBuf, String, bool)>,
}

/// The modification time of `dir` (`None` when it is gone or the platform has none).
fn dir_stamp(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|meta| meta.modified()).ok()
}

/// The parent folder of a project-relative path as the index stores it (`assets/props` for
/// `assets/props/rock.glb`, `""` for a bare name). Paths are `/`-separated on every OS
/// (`crate::content::slash_path`).
fn parent_str(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Lists one directory with the browser's rules: hidden (`.`) entries are skipped, `.meta`
/// sidecars are not assets, and files under a `*.meta` folder are not assets (`list_files`).
fn list_dir(root: &Path, dir: &Path, list_files: bool) -> Option<Listing> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut listing = Listing {
        files: Vec::new(),
        subdirs: Vec::new(),
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(file_type) if file_type.is_symlink() => path.is_dir(),
            Ok(file_type) => file_type.is_dir(),
            Err(_) => path.is_dir(),
        };
        let rel = slash_path(path.strip_prefix(root).unwrap_or(&path));
        if is_dir {
            let child_lists = list_files && !name.ends_with(".meta");
            listing.subdirs.push((path, rel, child_lists));
        } else if list_files && !name.ends_with(".meta") {
            let kind = kind_of_path(&rel);
            listing.files.push((rel, kind));
        }
    }
    Some(listing)
}

/// Where a walk puts what it finds.
struct WalkOut<'a> {
    assets: &'a mut Vec<IndexedAsset>,
    folders: &'a mut Vec<String>,
    dirs: &'a mut BTreeMap<PathBuf, Option<SystemTime>>,
    dirs_read: &'a mut u64,
}

/// Walks `start` and everything under it (`start` itself is not added to the folder list).
fn walk(root: &Path, start: PathBuf, list_files: bool, out: &mut WalkOut<'_>) {
    let mut stack = vec![(start, list_files)];
    while let Some((dir, list_files)) = stack.pop() {
        out.dirs.insert(dir.clone(), dir_stamp(&dir));
        *out.dirs_read += 1;
        let Some(listing) = list_dir(root, &dir, list_files) else {
            continue;
        };
        out.assets.extend(listing.files);
        for (path, rel, child_lists) in listing.subdirs {
            out.folders.push(rel);
            stack.push((path, child_lists));
        }
    }
}

/// Removes a folder (project-relative) and everything under it from the lists.
fn forget_subtree(
    rel_dir: &str,
    dir: &Path,
    assets: &mut Vec<IndexedAsset>,
    folders: &mut Vec<String>,
    dirs: &mut BTreeMap<PathBuf, Option<SystemTime>>,
) {
    let prefix = format!("{rel_dir}/");
    assets.retain(|(rel, _)| !rel.starts_with(&prefix));
    folders.retain(|folder| folder != rel_dir && !folder.starts_with(&prefix));
    dirs.retain(|known, _| !known.starts_with(dir));
}

/// `rel` without one leading `assets/`.
fn strip_assets(rel: &str) -> &str {
    rel.strip_prefix("assets/").unwrap_or(rel)
}

/// What a `.ron` text can reference: every string literal, `/`-separated, without an `asset:`
/// prefix, a `#sub-asset` part or a leading `assets/` (a path, or an asset id).
fn ron_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.char_indices();
    while let Some((start, c)) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut end = None;
        let mut escaped = false;
        for (at, c) in chars.by_ref() {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    end = Some(at);
                    break;
                }
                _ => escaped = false,
            }
        }
        let Some(end) = end else {
            break;
        };
        let literal = text[start + 1..end].replace("\\\\", "/").replace('\\', "/");
        let literal = literal.trim();
        let literal = literal.strip_prefix("asset:").unwrap_or(literal);
        let literal = literal.split_once('#').map_or(literal, |(source, _)| source);
        let literal = strip_assets(literal);
        if !literal.is_empty() {
            out.push(literal.to_string());
        }
    }
    out
}

/// One asset's facts from disk: its tags, its `.meta` fields, whether its source or `.meta` is
/// newer than its cooked file, and whether its review is pending.
fn disk_facts(
    root: &Path,
    rel: &str,
    rules: &regia_asset::registry::TagRules,
    policy: Option<&regia_asset_pipeline::provenance_policy::ProvenancePolicy>,
    cache_dir: Option<&Path>,
) -> AssetFacts {
    use regia_asset::{MetaSidecar, ProvenanceTool, ReviewerState};
    let mut tags = regia_asset::registry::automatic_tags(rel);
    tags.extend(rules.tags_for(rel));
    let source = root.join(rel);
    let meta_path = regia_asset::meta_path_for(&source);
    let meta = std::fs::read(&meta_path)
        .ok()
        .and_then(|bytes| MetaSidecar::from_ron(&bytes).ok());
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut review = false;
    let mut stale = false;
    if let Some(meta) = &meta {
        fields.push(("id".to_string(), meta.id.to_string()));
        fields.push(("importer".to_string(), meta.importer.clone()));
        if let Some(p) = &meta.provenance {
            let tool = regia_asset_pipeline::provenance_policy::tool_label(&p.tool);
            fields.push(("tool".to_string(), tool.to_string()));
            for (key, value) in [
                ("origin", &p.origin),
                ("generator", &p.generator),
                ("client", &p.client),
                ("model", &p.model),
                ("created", &p.created_utc),
            ] {
                if let Some(value) = value {
                    fields.push((key.to_string(), value.clone()));
                }
            }
            let reviewer = match &p.reviewer {
                ReviewerState::Pending => "pending".to_string(),
                ReviewerState::Owner => "owner".to_string(),
                ReviewerState::Named(name) => name.clone(),
            };
            fields.push(("reviewer".to_string(), reviewer));
            for (key, value) in &p.extensions {
                fields.push((key.clone(), value.clone()));
            }
            let made = matches!(p.tool, ProvenanceTool::Agent | ProvenanceTool::Generator);
            review = matches!(p.reviewer, ReviewerState::Pending)
                && (made || policy.is_some_and(|policy| policy.needs_review(Some(p))));
        }
        if let Some(cache_dir) = cache_dir {
            let cooked = cache_dir.join(regia_asset::cooked_file_name(meta.id));
            let time = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
            if let Some(cooked) = time(&cooked) {
                stale = [time(&source), time(&meta_path)]
                    .into_iter()
                    .flatten()
                    .any(|changed| changed > cooked);
            }
        }
    }
    let mut facts = AssetFacts::new(tags, fields);
    facts.set_state(AssetState::Modified, stale);
    facts.set_state(AssetState::Review, review);
    facts
}

/// The Filter menu's chips (an edit-world resource): each adds a search word, so chips combine
/// with the typed query. Within a group the chips widen the filter (any kind, any tag, any
/// state); the groups narrow it together.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentFilter {
    /// Asset types (`crate::asset_search::ASSET_TYPES` names).
    pub kinds: Vec<String>,
    /// Tags, `key` or `key=value`.
    pub tags: Vec<String>,
    /// States.
    pub states: Vec<AssetState>,
}

impl ContentFilter {
    /// Whether no chip is on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.tags.is_empty() && self.states.is_empty()
    }

    /// The search words of the chips (`type:mesh tag:biome=desert is:modified`).
    #[must_use]
    pub fn words(&self) -> String {
        let mut words: Vec<String> = Vec::new();
        words.extend(self.kinds.iter().map(|kind| format!("type:{kind}")));
        words.extend(self.tags.iter().map(|tag| format!("tag:{tag}")));
        words.extend(self.states.iter().map(|state| format!("is:{}", state.name())));
        words.join(" ")
    }

    /// The text the index searches: the typed query plus the chips' words (blank when both are).
    #[must_use]
    pub fn search_text(&self, typed: &str) -> String {
        if self.is_empty() {
            return typed.to_string();
        }
        format!("{} {}", typed.trim(), self.words()).trim().to_string()
    }

    /// Turns a chip on or off.
    pub fn toggle_state(&mut self, state: AssetState) {
        match self.states.iter().position(|s| *s == state) {
            Some(at) => {
                self.states.remove(at);
            }
            None => {
                self.states.push(state);
                self.states.sort_unstable();
            }
        }
    }

    /// Turns a kind chip on or off.
    pub fn toggle_kind(&mut self, kind: &str) {
        toggle_word(&mut self.kinds, kind);
    }

    /// Turns a tag chip on or off.
    pub fn toggle_tag(&mut self, tag: &str) {
        toggle_word(&mut self.tags, tag);
    }
}

fn toggle_word(list: &mut Vec<String>, word: &str) {
    match list.iter().position(|w| w == word) {
        Some(at) => {
            list.remove(at);
        }
        None => {
            list.push(word.to_string());
            list.sort();
        }
    }
}

/// The editor's unsaved documents and the last validator run's findings as [`WorldStates`]
/// (read only when `search` has an `is:` word, so other frames cost nothing).
#[must_use]
pub fn world_states(world: &World, search: &str) -> Option<WorldStates> {
    if !search.contains("is:") {
        return None;
    }
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    let mut states = WorldStates::default();
    for doc in crate::doc_autosave::dirty_documents(world) {
        if let Some(original) = doc.original {
            let rel = match &root {
                Some(root) => original
                    .strip_prefix(root)
                    .map_or(original.clone(), Path::to_path_buf),
                None => original,
            };
            states.modified.insert(slash_path(&rel));
        }
    }
    if let Some(panels) = world.get_resource::<crate::asset_panels::AssetPanels>() {
        for row in &panels.problems.rows {
            states.problems.insert(row.asset.clone());
        }
    }
    Some(states)
}

/// The tags the Filter menu offers: every `key=value` the assets carry from
/// `config/asset_tags.ron` (the automatic `ext` and `folder` tags are left out), sorted.
#[must_use]
pub fn tag_choices(index: &mut ContentIndex) -> Vec<String> {
    let facts = index.facts_for(false);
    let mut out: BTreeSet<String> = BTreeSet::new();
    for facts in facts.iter() {
        for (key, value) in &facts.tags {
            if key == "ext" || key == "folder" {
                continue;
            }
            out.insert(format!("{key}={value}"));
        }
    }
    out.into_iter().collect()
}

/// The asset types present in the index (the Filter menu's kind chips), in
/// `crate::asset_search::ASSET_TYPES` order.
#[must_use]
pub fn kind_choices(index: &ContentIndex) -> Vec<&'static str> {
    let present: HashSet<&'static str> = index
        .assets
        .iter()
        .map(|(rel, _)| crate::asset_search::asset_type(rel))
        .collect();
    crate::asset_search::ASSET_TYPES
        .into_iter()
        .filter(|kind| present.contains(kind))
        .collect()
}

impl ContentIndex {
    /// An index over a listing instead of a disk walk (benchmarks, tests, and tools that replay a
    /// listing). `paths` are project-relative (`assets/props/rock.glb`); the folder list is every
    /// folder above them under `assets/`. It knows no directory times, so it never goes stale.
    #[must_use]
    pub fn from_listing(root: Option<PathBuf>, paths: impl IntoIterator<Item = String>) -> Self {
        let mut index = Self {
            root,
            scanned: true,
            ..Self::default()
        };
        let mut folders = BTreeSet::new();
        let mut assets = Vec::new();
        for rel in paths {
            let mut parent = parent_str(&rel);
            while parent.contains('/') && folders.insert(parent.to_string()) {
                parent = parent_str(parent);
            }
            let kind = kind_of_path(&rel);
            assets.push((rel, kind));
        }
        index.generation = 1;
        index.install(assets, folders.into_iter().collect());
        index
    }

    /// Forgets the scan: the next [`Self::ensure`] walks the tree again.
    pub fn invalidate(&mut self) {
        self.scanned = false;
    }

    /// Forgets the scan and every asset's facts: the next [`Self::ensure`] walks the tree and the
    /// next search reads every `.meta` again (the panel's Rescan button; a plain rescan keeps the
    /// facts of folders whose time did not change).
    pub fn invalidate_all(&mut self) {
        self.scanned = false;
        self.facts_cache.clear();
        self.mentions_cache.clear();
        self.facts_inputs = None;
    }

    /// Scans `root/assets` when the index is new, invalidated, or for another project.
    pub fn ensure(&mut self, root: Option<&Path>) {
        if self.scanned && self.root.as_deref() == root {
            return;
        }
        self.scan(root);
    }

    fn scan(&mut self, root: Option<&Path>) {
        let _span = tracing::info_span!(target: "regia_editor", "content_index_scan").entered();
        let same_root = self.root.as_deref() == root;
        self.root = root.map(Path::to_path_buf);
        self.scanned = true;
        self.scans += 1;
        self.generation += 1;
        let old_dirs = std::mem::take(&mut self.dirs);
        let mut assets: Vec<IndexedAsset> = Vec::new();
        let mut folders: Vec<String> = Vec::new();
        let mut dirs = BTreeMap::new();
        if let Some(root) = root {
            let assets_dir = root.join("assets");
            if assets_dir.is_dir() {
                let mut out = WalkOut {
                    assets: &mut assets,
                    folders: &mut folders,
                    dirs: &mut dirs,
                    dirs_read: &mut self.dirs_read,
                };
                walk(root, assets_dir, true, &mut out);
            }
        }
        // Keep the facts of assets whose folder time did not change (an edit through a temporary
        // file and a rename, a new or removed `.meta`, changes it) while the files every asset's
        // facts depend on are unchanged too.
        let inputs = root.map(facts_inputs);
        if same_root && inputs.is_some() && inputs == self.facts_inputs {
            if let Some(root) = root {
                let unchanged = |rel: &str| {
                    let dir = root.join(parent_str(rel));
                    old_dirs
                        .get(&dir)
                        .is_some_and(|old| old.is_some() && dirs.get(&dir) == Some(old))
                };
                self.facts_cache.retain(|rel, _| unchanged(rel));
                self.mentions_cache.retain(|rel, _| unchanged(rel));
            }
        } else {
            self.facts_cache.clear();
            self.mentions_cache.clear();
        }
        self.facts_inputs = inputs;
        self.dirs = dirs;
        self.install(assets, folders);
    }

    /// Sorts the lists and rebuilds the per-folder children and the search keys.
    fn install(&mut self, mut assets: Vec<IndexedAsset>, mut folders: Vec<String>) {
        assets.sort();
        assets.dedup();
        folders.sort();
        folders.dedup();
        let mut children: HashMap<String, Vec<u32>> = HashMap::new();
        let mut keys = Vec::with_capacity(assets.len());
        for (index, (rel, _)) in assets.iter().enumerate() {
            let parent = parent_str(rel);
            match children.get_mut(parent) {
                Some(list) => list.push(index as u32),
                None => {
                    children.insert(parent.to_string(), vec![index as u32]);
                }
            }
            let (lower, name_at) = search_key(rel);
            keys.push((lower.into_boxed_str(), name_at as u32));
        }
        self.assets = Arc::from(assets);
        self.folders = Arc::from(folders);
        self.children = children;
        self.keys = keys;
        self.view = None;
        self.last_search = None;
    }

    /// Whether a walked directory changed (or vanished) since the scan; invalidates when so (the
    /// next [`Self::ensure`] walks the whole tree; [`Self::refresh_stale`] re-lists only what
    /// changed).
    pub fn check_stale(&mut self) -> bool {
        if !self.scanned {
            return true;
        }
        self.stamp_checks += 1;
        let stale = self
            .dirs
            .iter()
            .any(|(dir, modified)| dir_stamp(dir) != *modified || modified.is_none());
        if stale {
            self.invalidate();
        }
        stale
    }

    /// Checks every walked directory's time and re-lists only the directories that changed (new
    /// folders are walked, vanished ones forgotten). Returns whether anything changed. An index
    /// that was never scanned returns `true` and waits for [`Self::ensure`].
    pub fn refresh_stale(&mut self) -> bool {
        if !self.scanned {
            return true;
        }
        self.stamp_checks += 1;
        let changed: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|(dir, modified)| modified.is_none() || dir_stamp(dir) != **modified)
            .map(|(dir, _)| dir.clone())
            .collect();
        if changed.is_empty() {
            return false;
        }
        self.refresh_dirs(changed);
        true
    }

    /// Re-lists the directories that hold `paths` (files or folders that were created, changed,
    /// renamed or removed; absolute or project-relative), whatever their times say. Paths outside
    /// `assets/` are ignored. Returns how many directories were re-listed.
    pub fn refresh_paths(&mut self, paths: &[PathBuf]) -> usize {
        if !self.scanned {
            return 0;
        }
        let Some(root) = self.root.clone() else {
            return 0;
        };
        let assets_dir = root.join("assets");
        let mut changed = BTreeSet::new();
        for path in paths {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            if !path.starts_with(&assets_dir) {
                continue;
            }
            if self.dirs.contains_key(&path) {
                changed.insert(path.clone());
            }
            // The nearest known folder above lists the new, renamed or removed entry.
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if !dir.starts_with(&assets_dir) {
                    break;
                }
                if self.dirs.contains_key(dir) {
                    changed.insert(dir.to_path_buf());
                    break;
                }
                parent = dir.parent();
            }
        }
        let count = changed.len();
        if count > 0 {
            self.refresh_dirs(changed.into_iter().collect());
        }
        count
    }

    /// Re-lists `changed` (known directories) in place.
    fn refresh_dirs(&mut self, mut changed: Vec<PathBuf>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let _span =
            tracing::info_span!(target: "regia_editor", "content_index_refresh", dirs = changed.len())
                .entered();
        // Parents first, so a vanished folder's children are already forgotten.
        changed.sort();
        changed.dedup();
        self.refreshes += 1;
        self.generation += 1;
        let mut assets = self.assets.to_vec();
        let mut folders = self.folders.to_vec();
        let mut dirs = std::mem::take(&mut self.dirs);
        for dir in changed {
            if !dirs.contains_key(&dir) {
                // Forgotten with a vanished parent earlier in this refresh.
                continue;
            }
            let rel_dir = slash_path(dir.strip_prefix(&root).unwrap_or(&dir));
            // The folder's own files are listed again below, and their facts read again.
            assets.retain(|(rel, _)| parent_str(rel) != rel_dir);
            let under = format!("{rel_dir}/");
            self.facts_cache.retain(|rel, _| !rel.starts_with(&under));
            self.mentions_cache.retain(|rel, _| !rel.starts_with(&under));
            if !dir.is_dir() {
                forget_subtree(&rel_dir, &dir, &mut assets, &mut folders, &mut dirs);
                continue;
            }
            let list_files = !Path::new(&rel_dir)
                .components()
                .any(|part| part.as_os_str().to_string_lossy().ends_with(".meta"));
            dirs.insert(dir.clone(), dir_stamp(&dir));
            self.dirs_read += 1;
            let Some(listing) = list_dir(&root, &dir, list_files) else {
                continue;
            };
            assets.extend(listing.files);
            let present: HashSet<&str> = listing.subdirs.iter().map(|(_, rel, _)| rel.as_str()).collect();
            let prefix = format!("{rel_dir}/");
            let gone: Vec<String> = folders
                .iter()
                .filter(|folder| {
                    folder
                        .strip_prefix(&prefix)
                        .is_some_and(|rest| !rest.contains('/'))
                        && !present.contains(folder.as_str())
                })
                .cloned()
                .collect();
            for folder in gone {
                let path = root.join(&folder);
                forget_subtree(&folder, &path, &mut assets, &mut folders, &mut dirs);
            }
            for (path, rel, child_lists) in listing.subdirs {
                if dirs.contains_key(&path) {
                    // A known folder: its own time says whether it changed.
                    continue;
                }
                folders.push(rel);
                let mut out = WalkOut {
                    assets: &mut assets,
                    folders: &mut folders,
                    dirs: &mut dirs,
                    dirs_read: &mut self.dirs_read,
                };
                walk(&root, path, child_lists, &mut out);
            }
        }
        self.dirs = dirs;
        self.install(assets, folders);
    }

    /// [`Self::refresh_stale`] at most once per [`CHECK_EVERY`] (the panel calls this per frame).
    pub fn maybe_check(&mut self, now: Instant) {
        if self
            .last_check
            .is_some_and(|last| now.saturating_duration_since(last) < CHECK_EVERY)
        {
            return;
        }
        self.last_check = Some(now);
        self.refresh_stale();
    }

    /// Every asset, sorted.
    #[must_use]
    pub fn assets(&self) -> Arc<[IndexedAsset]> {
        Arc::clone(&self.assets)
    }

    /// Every folder under `assets/`, sorted.
    #[must_use]
    pub fn folders(&self) -> Arc<[String]> {
        Arc::clone(&self.folders)
    }

    /// Why the last search matched nothing, when its text was not a valid query.
    #[must_use]
    pub fn query_error(&self) -> Option<&QueryError> {
        self.query_error.as_ref()
    }

    /// The assets the panel lists for `folder` (`""` = `assets/`) and `search`: a non-blank
    /// search matches everywhere ([`crate::asset_search`] syntax, best matches first), otherwise
    /// only the folder's direct children are listed (the same rules as
    /// `ContentBrowserState::visible_assets`). Cached until the folder, search or index changes.
    pub fn visible(&mut self, folder: &str, search: &str) -> Arc<[IndexedAsset]> {
        let key = (
            folder.to_string(),
            search.to_string(),
            self.generation,
            self.world_rev,
        );
        if let Some((cached, view)) = &self.view
            && *cached == key
        {
            return Arc::clone(view);
        }
        self.views_built += 1;
        let view: Arc<[IndexedAsset]> = if search.trim().is_empty() {
            self.query_error = None;
            let prefix = if folder.is_empty() { "assets" } else { folder };
            match self.children.get(prefix) {
                Some(ids) => ids.iter().map(|&id| self.assets[id as usize].clone()).collect(),
                None => Arc::from(Vec::new()),
            }
        } else {
            match AssetQuery::parse(search) {
                Ok(query) => {
                    self.query_error = None;
                    let hits = self.search(&query);
                    hits.iter().map(|&id| self.assets[id as usize].clone()).collect()
                }
                Err(err) => {
                    self.query_error = Some(err);
                    Arc::from(Vec::new())
                }
            }
        };
        self.view = Some((key, Arc::clone(&view)));
        view
    }

    /// The hits of `query` as asset indices, best rank first, then by path. Filters the previous
    /// search's hits when `query` refines it on the same index generation.
    fn search(&mut self, query: &AssetQuery) -> Arc<[u32]> {
        let _span = tracing::info_span!(target: "regia_editor", "content_index_search").entered();
        let epoch = (self.generation, self.world_rev);
        let previous = self
            .last_search
            .as_ref()
            .filter(|(older, at, _)| *at == epoch && query.refines(older))
            .map(|(_, _, hits)| Arc::clone(hits));
        let facts = if query.uses_facts() || !query.terms.is_empty() {
            Some(self.facts_for(query.wants(AssetState::Unreferenced)))
        } else {
            None
        };
        let rank = |id: u32| {
            let (lower, name_at) = &self.keys[id as usize];
            match &facts {
                Some(facts) => query.rank_with(lower, *name_at as usize, facts.get(id as usize)),
                None => query.rank(lower, *name_at as usize),
            }
            .map(|rank| (rank, id))
        };
        let mut ranked: Vec<(u8, u32)> = match &previous {
            Some(hits) => {
                self.search_tested += hits.len() as u64;
                hits.iter().filter_map(|&id| rank(id)).collect()
            }
            None => {
                self.search_tested += self.keys.len() as u64;
                (0..self.keys.len() as u32).filter_map(rank).collect()
            }
        };
        if previous.is_some() {
            self.searches_narrowed += 1;
        }
        // Asset indices follow path order, so (rank, index) is (rank, path).
        ranked.sort_unstable();
        let hits: Arc<[u32]> = ranked.into_iter().map(|(_, id)| id).collect();
        self.last_search = Some((query.clone(), epoch, Arc::clone(&hits)));
        hits
    }

    /// Bumped by every change of the states the index cannot read from disk (keys views built
    /// from a search).
    #[must_use]
    pub fn world_revision(&self) -> u64 {
        self.world_rev
    }

    /// Sets the states the index cannot read from disk; a change rebuilds the views that use
    /// them. Returns whether they changed.
    pub fn set_world_states(&mut self, states: WorldStates) -> bool {
        if self.world_states == states {
            return false;
        }
        self.world_states = states;
        self.world_rev += 1;
        true
    }

    /// The facts of every asset (same order as [`Self::assets`]), read on first use per index
    /// generation and world-state revision. `references` also reads the project's `.ron` assets so
    /// [`AssetState::Unreferenced`] is set (otherwise it never is).
    pub fn facts_for(&mut self, references: bool) -> Arc<[AssetFacts]> {
        let references = references
            || self
                .referenced
                .as_ref()
                .is_some_and(|(generation, _)| *generation == self.generation);
        let key = (self.generation, self.world_rev, references);
        if let Some((cached, facts)) = &self.facts
            && *cached == key
        {
            return Arc::clone(facts);
        }
        let _span = tracing::info_span!(target: "regia_editor", "content_index_facts").entered();
        let root = self.root.clone();
        let rules = root
            .as_deref()
            .map(|root| regia_asset::registry::TagRules::load(root).0)
            .unwrap_or_default();
        let policy = root.as_deref().and_then(|root| {
            regia_asset_pipeline::provenance_policy::load_policy(root)
                .ok()
                .flatten()
        });
        let cache_dir = root
            .as_deref()
            .map(|root| regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform()));
        let referenced = if references {
            Some(self.referenced_paths())
        } else {
            None
        };
        let mut out = Vec::with_capacity(self.assets.len());
        for (rel, _) in self.assets.iter() {
            let mut facts = match self.facts_cache.get(rel) {
                Some(facts) => facts.clone(),
                None => {
                    let facts = match root.as_deref() {
                        Some(root) => {
                            self.meta_reads += 1;
                            disk_facts(root, rel, &rules, policy.as_ref(), cache_dir.as_deref())
                        }
                        None => AssetFacts::default(),
                    };
                    self.facts_cache.insert(rel.clone(), facts.clone());
                    facts
                }
            };
            if self.world_states.modified.contains(rel) {
                facts.set_state(AssetState::Modified, true);
            }
            facts.set_state(AssetState::Problem, self.world_states.problems.contains(rel));
            if let Some((ids, paths)) = &referenced {
                let id = facts.meta.get("id");
                let used = paths.contains(strip_assets(rel)) || id.is_some_and(|id| ids.contains(id));
                facts.set_state(AssetState::Unreferenced, !used);
            }
            out.push(facts);
        }
        let facts: Arc<[AssetFacts]> = Arc::from(out);
        self.facts = Some((key, Arc::clone(&facts)));
        facts
    }

    /// `(asset ids, assets/-relative paths)` the project's `.ron` assets mention (each file's
    /// mentions of itself left out), read once per generation.
    fn referenced_paths(&mut self) -> (Arc<HashSet<String>>, Arc<HashSet<String>>) {
        if let Some((generation, paths)) = &self.referenced
            && *generation == self.generation
        {
            let paths = Arc::clone(paths);
            return (Arc::clone(&paths), paths);
        }
        self.reference_scans += 1;
        let mut found = HashSet::new();
        if let Some(root) = self.root.clone() {
            for (rel, _) in self.assets.iter().filter(|(rel, _)| rel.ends_with(".ron")) {
                let mentions = match self.mentions_cache.get(rel) {
                    Some(mentions) => Arc::clone(mentions),
                    None => {
                        self.ron_reads += 1;
                        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
                            continue;
                        };
                        let mentions: Arc<[String]> = ron_mentions(&text).into();
                        self.mentions_cache.insert(rel.clone(), Arc::clone(&mentions));
                        mentions
                    }
                };
                let own = strip_assets(rel);
                for mention in mentions.iter() {
                    if mention != own {
                        found.insert(mention.clone());
                    }
                }
            }
        }
        let found = Arc::new(found);
        self.referenced = Some((self.generation, Arc::clone(&found)));
        (Arc::clone(&found), found)
    }

    /// `{root, assets, folders, scans, stamp_checks, views_built, generation, dirs_read,
    /// refreshes, search_tested, searches_narrowed, meta_reads, reference_scans}`.
    #[must_use]
    pub fn stats_json(&self) -> Value {
        json!({
            "root": self.root.as_ref().map(|root| root.display().to_string()),
            "assets": self.assets.len(),
            "folders": self.folders.len(),
            "scans": self.scans,
            "stamp_checks": self.stamp_checks,
            "views_built": self.views_built,
            "generation": self.generation,
            "dirs_read": self.dirs_read,
            "refreshes": self.refreshes,
            "search_tested": self.search_tested,
            "searches_narrowed": self.searches_narrowed,
            "meta_reads": self.meta_reads,
            "reference_scans": self.reference_scans,
        })
    }
}

/// Forgets the world's index scan (after the editor changed files under `assets/`).
pub fn invalidate(world: &mut World) {
    if let Some(mut index) = world.get_resource_mut::<ContentIndex>() {
        index.invalidate();
    }
}

/// Forgets the world's index scan and every asset's facts ([`ContentIndex::invalidate_all`]).
pub fn invalidate_all(world: &mut World) {
    if let Some(mut index) = world.get_resource_mut::<ContentIndex>() {
        index.invalidate_all();
    }
}

/// Re-lists the folders that hold `paths` in the world's index (the live asset watcher's changed
/// sources; see [`ContentIndex::refresh_paths`]). Returns how many folders were re-listed.
pub fn note_changed(world: &mut World, paths: &[PathBuf]) -> usize {
    match world.get_resource_mut::<ContentIndex>() {
        Some(mut index) => index.refresh_paths(paths),
        None => 0,
    }
}

/// The folder list and the visible assets for the panel this frame: scans when needed and
/// checks the directory times at most once per [`CHECK_EVERY`].
pub fn panel_view(world: &mut World, state: &ContentBrowserState) -> (Arc<[String]>, Arc<[IndexedAsset]>) {
    // The Filter menu's chips add their words to the typed search (ADR-7759).
    let search = world.get_resource::<ContentFilter>().map_or_else(
        || state.search.clone(),
        |filter| filter.search_text(&state.search),
    );
    let states = world_states(world, &search);
    let mut index = world.get_resource_or_insert_with(ContentIndex::default);
    if let Some(states) = states {
        index.set_world_states(states);
    }
    index.maybe_check(Instant::now());
    index.ensure(state.project_root.as_deref());
    let visible = index.visible(&state.folder, &search);
    (index.folders(), visible)
}

/// `editor.content_index`.
fn content_index_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(text) = params.get("complete") {
        let text = text.as_str().ok_or_else(|| {
            RpcError::new(
                regia_agent::protocol::INVALID_PARAMS,
                "bad_query",
                "complete must be a string",
                "see book/agents/editor-content-index.md#token-completion",
            )
        })?;
        let vocab = crate::asset_search::CompletionVocab::content_browser();
        return Ok(json!({
            "completion": crate::asset_search::complete_token(text, &vocab),
            "candidates": crate::asset_search::completion_candidates(text, &vocab),
        }));
    }
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| {
            let mut error = RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            );
            if let Some(data) = error.data.as_mut() {
                data["index_error"] = json!("index_no_project");
            }
            error
        })?;
    let refresh = params.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    let folder = str_param(params, "folder").unwrap_or("").replace('\\', "/");
    let search = str_param(params, "search").unwrap_or("");
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(200, |limit| limit.min(5_000) as usize);
    let states = world_states(world, search);
    let mut index = world.get_resource_or_insert_with(ContentIndex::default);
    if let Some(states) = states {
        index.set_world_states(states);
    }
    // Only the folders whose times changed are listed again (no full walk).
    let stale = if refresh { index.refresh_stale() } else { false };
    index.ensure(Some(&root));
    let visible = index.visible(&folder, search);
    if let Some(err) = index.query_error() {
        let mut error = RpcError::invalid_params(format!("search {search:?}: {err}"));
        if let Some(data) = error.data.as_mut() {
            data["index_error"] = json!(err.code);
        }
        return Err(error);
    }
    let mut out = index.stats_json();
    out["stale"] = json!(stale);
    out["visible"] = json!(visible.len());
    out["paths"] = json!(
        visible
            .iter()
            .take(limit)
            .map(|(rel, kind)| json!({ "path": rel, "kind": crate::dnd::kind_name(*kind) }))
            .collect::<Vec<_>>()
    );
    Ok(out)
}

/// A coded `editor.content_filter` error (`filter_error` in the data).
fn filter_error(message: String, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["filter_error"] = json!(code);
    }
    error
}

/// A string array parameter (`None` when absent).
fn string_list(params: &Value, name: &str) -> Result<Option<Vec<String>>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let list = value.as_array().ok_or_else(|| {
        filter_error(
            format!("`{name}` must be an array of strings"),
            "filter_bad_params",
        )
    })?;
    list.iter()
        .map(|item| {
            item.as_str()
                .map(|text| text.trim().to_lowercase())
                .ok_or_else(|| {
                    filter_error(
                        format!("`{name}` must be an array of strings"),
                        "filter_bad_params",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `editor.content_filter`.
fn content_filter_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut filter = world.get_resource::<ContentFilter>().cloned().unwrap_or_default();
    if params.get("clear").and_then(Value::as_bool) == Some(true) {
        filter = ContentFilter::default();
    }
    if let Some(mut kinds) = string_list(params, "kinds")? {
        if let Some(bad) = kinds
            .iter()
            .find(|kind| !crate::asset_search::ASSET_TYPES.contains(&kind.as_str()))
        {
            return Err(filter_error(
                format!(
                    "unknown kind `{bad}` (use {})",
                    crate::asset_search::ASSET_TYPES.join(", ")
                ),
                "filter_bad_kind",
            ));
        }
        kinds.sort();
        kinds.dedup();
        filter.kinds = kinds;
    }
    if let Some(mut tags) = string_list(params, "tags")? {
        if let Some(bad) = tags
            .iter()
            .find(|tag| tag.is_empty() || tag.starts_with('=') || tag.contains(char::is_whitespace))
        {
            return Err(filter_error(
                format!("bad tag `{bad}`: use `key` or `key=value` without spaces"),
                "filter_bad_tag",
            ));
        }
        tags.sort();
        tags.dedup();
        filter.tags = tags;
    }
    if let Some(names) = string_list(params, "states")? {
        let mut states = Vec::new();
        for name in names {
            let state = AssetState::from_name(&name).ok_or_else(|| {
                let all: Vec<&str> = AssetState::ALL.iter().map(|s| s.name()).collect();
                filter_error(
                    format!("unknown state `{name}` (use {})", all.join(", ")),
                    "filter_bad_state",
                )
            })?;
            states.push(state);
        }
        states.sort_unstable();
        states.dedup();
        filter.states = states;
    }
    let search = str_param(params, "search").unwrap_or("").to_string();
    let text = filter.search_text(&search);
    let mut out = json!({
        "kinds": filter.kinds,
        "tags": filter.tags,
        "states": filter.states.iter().map(|s| s.name()).collect::<Vec<_>>(),
        "words": filter.words(),
    });
    world.insert_resource(filter);
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    if let Some(root) = root {
        let states = world_states(world, &text);
        let mut index = world.get_resource_or_insert_with(ContentIndex::default);
        if let Some(states) = states {
            index.set_world_states(states);
        }
        index.ensure(Some(&root));
        out["kind_choices"] = json!(kind_choices(&index));
        out["tag_choices"] = json!(tag_choices(&mut index));
        let visible = index.visible("", &text);
        if let Some(err) = index.query_error() {
            return Err(filter_error(format!("search {text:?}: {err}"), err.code));
        }
        out["visible"] = json!(visible.len());
        out["paths"] = json!(
            visible
                .iter()
                .take(200)
                .map(|(rel, _)| rel.as_str())
                .collect::<Vec<_>>()
        );
    }
    Ok(out)
}

/// The content index's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.content_index",
                tool: "content_index",
                description: "The Content Browser's cached asset index: asset and folder counts, the view for a \
                folder/search (the first `limit` paths, best matches first), and its counters (scans, \
                stamp_checks, views_built, generation, dirs_read, refreshes, search_tested, \
                searches_narrowed). `refresh: true` first checks the directory times and re-lists the \
                folders that changed on disk.",
                params: &[
                    ParamSchema {
                        name: "complete",
                        kind: "string",
                        required: false,
                        description: "Complete a Content Browser search token without running a search; returns completion and sorted candidates, including for a partial filter value. No project is needed.",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "A folder from the folder list (\"assets/props\"); default assets/.",
                    },
                    ParamSchema {
                        name: "search",
                        kind: "string",
                        required: false,
                        description: "A search query (words, -word, type:<name>, ext:<suffix>, tag:<key>[=<value>], \
                        is:modified|unreferenced|problem|review, meta:<key>[=<value>]; any case); matches \
                        everywhere when set. Plain words also match tag and .meta values.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "How many paths to return (default 200, at most 5000).",
                    },
                    ParamSchema {
                        name: "refresh",
                        kind: "boolean",
                        required: false,
                        description: "Check the directory times first (one stat per folder) and re-list the \
                        changed folders. Default false.",
                    },
                ],
                result: "{ completion, candidates } when complete is given; otherwise { root, assets, folders, scans, stamp_checks, views_built, generation, dirs_read, refreshes, \
                search_tested, searches_narrowed, meta_reads, reference_scans, stale, visible, paths: [{path, kind}] }",
                mutating: false,
                permission: Permission::Read,
            },
            content_index_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_filter",
                tool: "content_filter",
                description: "The Content Browser's Filter menu chips: asset kinds, tags and states (modified, \
                unreferenced, problem, review). Each given list replaces that group; chips widen within a group \
                and narrow across groups, and combine with the typed search. Without params it reports the \
                chips. With a project open it also answers the choices and the assets the chips (plus \
                `search`) list.",
                params: &[
                    ParamSchema {
                        name: "kinds",
                        kind: "array",
                        required: false,
                        description: "Asset kinds: level, prefab, mesh, texture, material, audio, other.",
                    },
                    ParamSchema {
                        name: "tags",
                        kind: "array",
                        required: false,
                        description: "Tags, `key` or `key=value` (config/asset_tags.ron and the automatic ext/folder \
                        tags).",
                    },
                    ParamSchema {
                        name: "states",
                        kind: "array",
                        required: false,
                        description: "States: modified, unreferenced, problem, review.",
                    },
                    ParamSchema {
                        name: "clear",
                        kind: "boolean",
                        required: false,
                        description: "Turn every chip off first. Default false.",
                    },
                    ParamSchema {
                        name: "search",
                        kind: "string",
                        required: false,
                        description: "Typed words to combine with the chips for the reply's list.",
                    },
                ],
                result: "{ kinds, tags, states, words, kind_choices?, tag_choices?, visible?, paths? }",
                mutating: false,
                permission: Permission::Read,
            },
            content_filter_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str, files: &[&str]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        for rel in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            regia_test_utils::write_atomic(&path, b"x").expect("write");
        }
        root
    }

    const FILES: &[&str] = &[
        "assets/a.level.ron",
        "assets/props/rock.glb",
        "assets/props/rock.glb.meta",
        "assets/props/crate.prefab.ron",
        "assets/props/small/pebble.glb",
        "assets/.hidden/secret.png",
        "assets/cache.meta/inner.png",
        "assets/textures/Rock_albedo.png",
    ];

    fn state(root: &Path, folder: &str, search: &str) -> ContentBrowserState {
        ContentBrowserState {
            project_root: Some(root.to_path_buf()),
            folder: folder.to_string(),
            search: search.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_index_lists_what_the_browser_scan_lists() {
        let root = project("content_index_same", FILES);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        let full = state(&root, "", "");
        assert_eq!(index.assets().to_vec(), full.assets(), "every asset");
        assert_eq!(index.folders().to_vec(), full.folders(), "every folder");
        for (folder, search) in [
            ("", ""),
            ("assets/props", ""),
            ("assets/props/small", ""),
            ("assets/textures", ""),
            ("assets/props", "rock"),
            ("", "Rock"),
            ("", "no-such"),
        ] {
            assert_eq!(
                index.visible(folder, search).to_vec(),
                state(&root, folder, search).visible_assets(),
                "folder {folder:?}, search {search:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frames_reuse_the_scan_and_the_view() {
        let root = project("content_index_reuse", FILES);
        let mut world = World::new();
        let browser = state(&root, "assets/props", "");
        for _ in 0..10 {
            let (_folders, visible) = panel_view(&mut world, &browser);
            assert_eq!(visible.len(), 2, "rock.glb and crate.prefab.ron");
        }
        let index = world.resource::<ContentIndex>();
        assert_eq!(index.scans, 1, "ten frames, one walk");
        assert_eq!(
            index.views_built, 1,
            "an unchanged folder and search reuse the view"
        );
        let searching = state(&root, "assets/props", "pebble");
        let (_, visible) = panel_view(&mut world, &searching);
        assert_eq!(visible.len(), 1);
        assert_eq!(world.resource::<ContentIndex>().views_built, 2);
        assert_eq!(
            world.resource::<ContentIndex>().scans,
            1,
            "a new search does not rescan"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalidation_and_another_project_rescan() {
        let root = project("content_index_invalidate", FILES);
        let other = project("content_index_other", &["assets/only.glb"]);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        index.ensure(Some(&root));
        assert_eq!(index.scans, 1);
        regia_test_utils::write_atomic(&root.join("assets/new.glb"), b"x").expect("write");
        index.invalidate();
        index.ensure(Some(&root));
        assert_eq!(index.scans, 2);
        assert!(index.assets().iter().any(|(rel, _)| rel.ends_with("new.glb")));
        index.ensure(Some(&other));
        assert_eq!(index.scans, 3, "another project is another scan");
        assert_eq!(index.assets().len(), 1);
        index.ensure(None);
        assert!(index.assets().is_empty(), "no project, no assets");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_vanished_directory_is_stale() {
        let root = project("content_index_stale", FILES);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        assert!(!index.check_stale(), "nothing changed");
        std::fs::remove_dir_all(root.join("assets/props/small")).expect("remove");
        assert!(index.check_stale(), "a walked folder is gone");
        index.ensure(Some(&root));
        assert_eq!(index.scans, 2);
        assert!(!index.folders().iter().any(|folder| folder.ends_with("small")));
        assert!(!index.check_stale());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_throttle_checks_once_per_interval() {
        let root = project("content_index_throttle", FILES);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        let start = Instant::now();
        index.maybe_check(start);
        index.maybe_check(start + Duration::from_millis(10));
        assert_eq!(index.stamp_checks, 1, "inside the interval: no check");
        index.maybe_check(start + CHECK_EVERY);
        assert_eq!(index.stamp_checks, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_agent_method_reports_counts_and_needs_a_project() {
        let root = project("content_index_agent", FILES);
        let mut world = World::new();
        let err = content_index_method(&mut world, &json!({})).expect_err("no project");
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("index_error")),
            Some(&json!("index_no_project"))
        );
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let reply = content_index_method(&mut world, &json!({ "search": "rock" })).expect("index");
        // Search ignores case since ADR-1329: rock.glb and Rock_albedo.png, never the sidecar.
        assert_eq!(
            reply["visible"], 2,
            "rock.glb and Rock_albedo.png (the .meta sidecar is not an asset)"
        );
        assert!(
            reply["paths"].as_array().is_some_and(|paths| paths
                .iter()
                .all(|p| !p["path"].as_str().unwrap_or("").ends_with(".meta"))),
            "{reply}"
        );
        assert_eq!(reply["paths"][0]["kind"], "mesh");
        assert_eq!(reply["scans"], 1);
        let again =
            content_index_method(&mut world, &json!({ "search": "rock", "refresh": true })).expect("refresh");
        assert_eq!(again["stale"], false);
        assert_eq!(again["scans"], 1, "an unchanged tree is not walked again");
        let _ = std::fs::remove_dir_all(&root);
    }
    /// `assets/set_FFF/prop_IIIII.ext` (the index's `/` form on every OS): 500 folders of 100
    /// assets, every tenth a texture.
    fn listing(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| {
                let ext = if i.is_multiple_of(10) { "png" } else { "glb" };
                format!("assets/set_{:03}/prop_{i:05}.{ext}", i / 100)
            })
            .collect()
    }

    #[test]
    fn fifty_thousand_assets_cost_their_folder_and_their_narrowing() {
        let paths = listing(50_000);
        let mut index = ContentIndex::from_listing(None, paths.clone());
        assert_eq!(index.assets().len(), 50_000);
        assert_eq!(index.folders().len(), 500);

        // A folder view is the folder's own 100 entries, built once for any number of frames.
        let folder = "assets/set_042".to_string();
        for _ in 0..30 {
            let view = index.visible(&folder, "");
            assert_eq!(view.len(), 100);
            assert!(view.iter().all(|(rel, _)| rel.starts_with(&folder)));
        }
        assert_eq!(index.views_built, 1, "thirty frames, one view");
        assert_eq!(index.search_tested, 0, "a folder view tests no search keys");

        // Typing "prop_1", "prop_12", then a type filter: each key tests the previous hits only.
        let reference = |text: &str| -> Vec<String> {
            let query = AssetQuery::parse(text).expect("query");
            crate::asset_search::search_paths(&query, paths.iter().map(String::as_str))
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        let steps = [
            ("PROP_1", 50_000, 10_000, 0),
            ("prop_12", 10_000, 1_000, 1),
            ("prop_12 type:texture", 1_000, 100, 2),
            ("prop_12 type:texture -prop_129", 100, 90, 3),
            // Not a narrowing of the last query: back to every asset.
            ("prop_3", 50_000, 10_000, 3),
        ];
        let mut tested = 0;
        for (text, cost, hits, narrowed) in steps {
            let view = index.visible("", text);
            tested += cost;
            assert_eq!(index.search_tested, tested, "{text:?} tests {cost} keys");
            assert_eq!(index.searches_narrowed, narrowed, "{text:?}");
            assert_eq!(view.len(), hits, "{text:?}");
            let got: Vec<String> = view.iter().map(|(rel, _)| rel.clone()).collect();
            assert_eq!(got, reference(text), "{text:?} agrees with the reference matcher");
        }
        // Redrawing the same search costs nothing.
        let before = (index.views_built, index.search_tested);
        let _ = index.visible("", "prop_3");
        assert_eq!((index.views_built, index.search_tested), before);
    }

    #[test]
    fn a_bad_query_lists_nothing_and_says_why() {
        let mut index = ContentIndex::from_listing(None, listing(20));
        assert!(index.visible("", "type:sprite").is_empty());
        assert_eq!(index.query_error().map(|err| err.code), Some("search_bad_type"));
        assert_eq!(index.visible("", "prop").len(), 20);
        assert!(index.query_error().is_none(), "a good query clears the error");
        assert_eq!(
            index.visible("", "   ").len(),
            0,
            "a blank search is the folder view (assets/ holds only folders here)"
        );
    }

    /// 30 folders of 4 assets each, plus a `.meta` folder and a nested folder.
    fn tree(tag: &str) -> PathBuf {
        let mut files: Vec<String> = Vec::new();
        for folder in 0..30 {
            for item in 0..4 {
                files.push(format!("assets/f{folder:02}/item_{item}.glb"));
            }
        }
        files.push("assets/f00/nested/deep.png".to_string());
        files.push("assets/cache.meta/inner.png".to_string());
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        project(tag, &refs)
    }

    fn assert_matches_a_fresh_scan(index: &ContentIndex, root: &Path) {
        let mut fresh = ContentIndex::default();
        fresh.ensure(Some(root));
        assert_eq!(index.assets().to_vec(), fresh.assets().to_vec(), "assets");
        assert_eq!(index.folders().to_vec(), fresh.folders().to_vec(), "folders");
        assert_eq!(
            index.dirs.keys().collect::<Vec<_>>(),
            fresh.dirs.keys().collect::<Vec<_>>(),
            "walked folders"
        );
    }

    #[test]
    fn a_changed_folder_time_relists_only_that_folder() {
        let root = tree("content_index_refresh_stale");
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        assert_eq!(index.dirs_read, 33, "assets/, 30 folders, nested, cache.meta");
        assert!(!index.refresh_stale(), "nothing changed");
        assert_eq!(index.dirs_read, 33, "a clean check lists nothing");

        regia_test_utils::write_atomic(&root.join("assets/f07/new.glb"), b"x").expect("write");
        std::fs::remove_file(root.join("assets/f03/item_0.glb")).expect("remove");
        // Directory times can tick coarser than the test runs; mark the two folders as changed
        // the way a later clock would.
        for dir in ["assets/f07", "assets/f03"] {
            index.dirs.insert(root.join(dir), Some(SystemTime::UNIX_EPOCH));
        }
        assert!(index.refresh_stale());
        assert_eq!(index.dirs_read, 35, "two folders listed again, not 33");
        assert_eq!(index.scans, 1, "no full walk");
        assert_eq!(index.refreshes, 1);
        assert_matches_a_fresh_scan(&index, &root);
        assert!(index.assets().iter().any(|(rel, _)| rel.ends_with("new.glb")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn watcher_paths_relist_new_and_vanished_folders() {
        let root = tree("content_index_refresh_paths");
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        let generation = index.generation;
        let before = index.dirs_read;

        // A new nested folder with a file, a whole folder removed, a file in a `.meta` folder.
        let new_file = root.join("assets/f05/sub/deeper/x.png");
        std::fs::create_dir_all(new_file.parent().expect("parent")).expect("dirs");
        regia_test_utils::write_atomic(&new_file, b"x").expect("write");
        std::fs::remove_dir_all(root.join("assets/f09")).expect("remove folder");
        let hidden = root.join("assets/cache.meta/more.png");
        regia_test_utils::write_atomic(&hidden, b"x").expect("write");

        let relisted = index.refresh_paths(&[
            new_file.clone(),
            root.join("assets/f09"),
            PathBuf::from("assets/cache.meta/more.png"),
            root.join("elsewhere/ignored.png"),
        ]);
        // assets/ (f09's parent), f05 (the nearest known folder above the new one), f09 (gone)
        // and cache.meta.
        assert_eq!(relisted, 4);
        assert_eq!(
            index.dirs_read - before,
            5,
            "assets/, f05, cache.meta, and the two new folders walked"
        );
        assert_eq!(index.scans, 1);
        assert!(index.generation > generation, "views rebuild after a refresh");
        assert_matches_a_fresh_scan(&index, &root);
        assert!(!index.folders().iter().any(|folder| folder.ends_with("f09")));
        assert!(
            !index.assets().iter().any(|(rel, _)| rel.contains("cache.meta")),
            "files under a .meta folder stay out"
        );
        let deeper = "assets/f05/sub/deeper";
        assert_eq!(index.visible(deeper, "").len(), 1, "the new folder's view");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_agent_method_reports_bad_queries_and_limits() {
        let root = project("content_index_agent_query", FILES);
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let err = content_index_method(&mut world, &json!({ "search": "t:sprite" })).expect_err("bad type");
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("index_error")),
            Some(&json!("search_bad_type"))
        );
        let reply =
            content_index_method(&mut world, &json!({ "search": "type:mesh", "limit": 1 })).expect("index");
        assert_eq!(reply["visible"], 2, "rock.glb and pebble.glb");
        assert_eq!(reply["paths"].as_array().map(Vec::len), Some(1), "limit 1");
        assert_eq!(reply["search_tested"], 5, "every asset once");
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn filter_queries_agree_with_the_browser_scan() {
        let root = project("content_index_query_parity", FILES);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        for search in [
            "rock",
            "ROCK -glb",
            "type:mesh",
            "t:texture t:level",
            "ext:ron",
            "props rock",
            "-props",
            "type:sprite",
            "ext:",
        ] {
            assert_eq!(
                index.visible("", search).to_vec(),
                state(&root, "", search).visible_assets(),
                "search {search:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_inspector_catalog_shares_the_index_list() {
        let root = project("content_index_catalog", FILES);
        let mut world = World::new();
        let browser = state(&root, "", "");
        for _ in 0..5 {
            crate::inspector::refresh_asset_catalog(&mut world, &browser);
        }
        let index = world.resource::<ContentIndex>();
        assert_eq!(index.scans, 1, "five refreshes, one walk");
        let catalog = world.resource::<crate::inspector::AssetCatalog>();
        assert!(
            Arc::ptr_eq(&catalog.entries, &index.assets()),
            "the catalog is the index's list, not a copy"
        );
        assert_eq!(catalog.entries.len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A small deterministic generator (64-bit LCG, Knuth's MMIX constants).
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    #[test]
    fn random_edits_refreshed_by_path_match_a_fresh_scan() {
        let root = project("content_index_random", &["assets/a/x.glb", "assets/b/y.png"]);
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        let mut rng = Lcg(0x5eed);
        let names = ["a", "b", "c", "d.meta", "e"];
        for step in 0..60 {
            // A random folder up to three deep, under assets/.
            let depth = 1 + rng.below(3);
            let mut dir = root.join("assets");
            for _ in 0..depth {
                dir = dir.join(names[rng.below(names.len())]);
            }
            let touched = match rng.below(4) {
                0 | 1 => {
                    let file = dir.join(format!("f{step}.glb"));
                    std::fs::create_dir_all(&dir).expect("dirs");
                    regia_test_utils::write_atomic(&file, b"x").expect("write");
                    file
                }
                2 => {
                    let _ = std::fs::remove_dir_all(&dir);
                    dir
                }
                _ => {
                    // Remove one file of the folder, when it has any.
                    let victim = std::fs::read_dir(&dir).ok().and_then(|entries| {
                        entries
                            .filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .find(|path| path.is_file())
                    });
                    match victim {
                        Some(file) => {
                            std::fs::remove_file(&file).expect("remove");
                            file
                        }
                        None => dir,
                    }
                }
            };
            index.refresh_paths(&[touched]);
            assert_matches_a_fresh_scan(&index, &root);
        }
        assert_eq!(index.scans, 1, "sixty edits, no full walk");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_without_a_time_is_listed_on_every_check() {
        let root = tree("content_index_no_time");
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        let before = index.dirs_read;
        index.dirs.insert(root.join("assets/f11"), None);
        assert!(index.refresh_stale(), "an unknown time counts as changed");
        assert_eq!(index.dirs_read, before + 1);
        assert_matches_a_fresh_scan(&index, &root);
        let mut empty = ContentIndex::from_listing(None, Vec::new());
        assert_eq!(
            empty.refresh_paths(&[root.join("assets/f11")]),
            0,
            "no root, nothing to list"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn asset_id(n: u128) -> regia_asset::AssetId {
        regia_asset::AssetId(regia_core::id::Guid::from_u128(n))
    }

    /// A `.meta` with provenance, as the importer writes it.
    fn meta_text(id: u128, tool: &str, reviewer: &str, model: &str) -> String {
        use regia_asset::{MetaSidecar, Provenance, ProvenanceTool, ReviewerState};
        let mut meta = MetaSidecar::new("gltf", asset_id(id));
        meta.provenance = Some(Provenance {
            tool: if tool == "agent" {
                ProvenanceTool::Agent
            } else {
                ProvenanceTool::Hand
            },
            model: Some(model.to_string()),
            reviewer: if reviewer == "Owner" {
                ReviewerState::Owner
            } else {
                ReviewerState::Pending
            },
            ..Provenance::default()
        });
        meta.to_ron_string().expect("meta ron")
    }

    fn facts_project(tag: &str) -> PathBuf {
        let root = project(
            tag,
            &[
                "assets/props/rock.glb",
                "assets/props/crate.glb",
                "assets/props/unused.png",
                "assets/maps/arena.level.ron",
                "assets/mat/self.material.ron",
            ],
        );
        let write = |rel: &str, text: &str| {
            regia_test_utils::write_atomic(&root.join(rel), text.as_bytes()).expect("write");
        };
        write(
            "assets/props/rock.glb.meta",
            &meta_text(1, "agent", "Pending", "Sketch-2"),
        );
        write(
            "assets/props/crate.glb.meta",
            &meta_text(2, "hand", "Owner", "none"),
        );
        // The level names rock by path, crate by id; the material names only itself.
        write(
            "assets/maps/arena.level.ron",
            &format!(
                "(entities: [(mesh: \"assets/props/rock.glb#mesh/0\"), (mesh: \"asset:{}\")])",
                asset_id(2)
            ),
        );
        write(
            "assets/mat/self.material.ron",
            "(name: \"mat/self.material.ron\")",
        );
        std::fs::create_dir_all(root.join("config")).expect("config");
        write(
            "config/asset_tags.ron",
            "(format_version: 1, rules: [(paths: [\"assets/props/**\"], tags: {\"biome\": \"Desert\"})])",
        );
        root
    }

    fn hits(index: &mut ContentIndex, search: &str) -> Vec<String> {
        index
            .visible("", search)
            .iter()
            .map(|(rel, _)| rel.clone())
            .collect()
    }

    #[test]
    fn tag_state_and_meta_searches_read_each_meta_once() {
        let root = facts_project("content_index_facts");
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        assert_eq!(
            hits(&mut index, "tag:biome=desert"),
            [
                "assets/props/crate.glb",
                "assets/props/rock.glb",
                "assets/props/unused.png"
            ]
        );
        assert_eq!(index.meta_reads, 5, "every asset's facts read once");
        assert_eq!(
            hits(&mut index, "is:review"),
            ["assets/props/rock.glb"],
            "agent-made, pending"
        );
        assert_eq!(hits(&mut index, "meta:model=sketch"), ["assets/props/rock.glb"]);
        assert_eq!(hits(&mut index, "meta:tool=hand"), ["assets/props/crate.glb"]);
        assert_eq!(
            hits(&mut index, "sketch-2"),
            ["assets/props/rock.glb"],
            "free text over .meta"
        );
        assert_eq!(
            hits(&mut index, "desert type:texture"),
            ["assets/props/unused.png"]
        );
        assert_eq!(index.meta_reads, 5, "later searches reuse the facts");
        assert_eq!(index.reference_scans, 0, "no is:unreferenced yet");
        assert_eq!(
            hits(&mut index, "is:unreferenced"),
            [
                "assets/maps/arena.level.ron",
                "assets/mat/self.material.ron",
                "assets/props/unused.png"
            ],
            "rock by path, crate by id; a self mention does not count"
        );
        assert_eq!(index.reference_scans, 1);
        // The delete check's referencer rule agrees on every asset.
        for (rel, _) in index.assets().iter() {
            let referencers: Vec<String> = regia_asset::project_ops::find_referencers(&root, rel)
                .into_iter()
                .filter(|file| file != rel)
                .collect();
            let unreferenced = hits(&mut index, "is:unreferenced").contains(rel);
            assert_eq!(unreferenced, referencers.is_empty(), "{rel}: {referencers:?}");
        }
        assert_eq!(index.reference_scans, 1, "once per generation");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn modified_means_newer_than_the_cook_or_an_unsaved_document() {
        let root = facts_project("content_index_modified");
        let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
        std::fs::create_dir_all(&cache).expect("cache");
        // rock's cooked file is older than its source, crate's is newer.
        let now = std::time::SystemTime::now();
        for (id, when) in [
            (1u128, now - Duration::from_secs(7_200)),
            (2, now + Duration::from_secs(3_600)),
        ] {
            let path = cache.join(regia_asset::cooked_file_name(asset_id(id)));
            regia_test_utils::write_atomic(&path, b"cooked").expect("cooked");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(when))
                .expect("set time");
        }
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        assert_eq!(hits(&mut index, "is:modified"), ["assets/props/rock.glb"]);
        let before = index.views_built;
        // An unsaved document joins through the world states and rebuilds the view.
        let mut states = WorldStates::default();
        states.modified.insert("assets/maps/arena.level.ron".to_string());
        states.problems.insert("assets/props/crate.glb".to_string());
        assert!(index.set_world_states(states.clone()));
        assert!(!index.set_world_states(states), "the same states change nothing");
        assert_eq!(
            hits(&mut index, "is:modified"),
            ["assets/maps/arena.level.ron", "assets/props/rock.glb"]
        );
        assert_eq!(hits(&mut index, "is:problem"), ["assets/props/crate.glb"]);
        assert!(index.views_built > before);
        assert_eq!(index.meta_reads, 5, "world states reuse the disk facts");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn chips_add_words_and_a_relisted_folder_reads_its_facts_again() {
        let mut filter = ContentFilter::default();
        assert_eq!(filter.search_text(" rock "), " rock ", "no chips: the typed text");
        filter.toggle_kind("mesh");
        filter.toggle_tag("biome=desert");
        filter.toggle_state(AssetState::Review);
        filter.toggle_state(AssetState::Modified);
        assert_eq!(filter.words(), "type:mesh tag:biome=desert is:modified is:review");
        assert_eq!(
            filter.search_text("rock"),
            "rock type:mesh tag:biome=desert is:modified is:review"
        );
        filter.toggle_state(AssetState::Modified);
        assert_eq!(filter.states, [AssetState::Review]);

        let root = facts_project("content_index_chips");
        let mut index = ContentIndex::default();
        index.ensure(Some(&root));
        assert_eq!(kind_choices(&index), ["level", "mesh", "texture", "material"]);
        assert_eq!(
            tag_choices(&mut index),
            ["biome=desert".replace("desert", "Desert")]
        );
        assert_eq!(
            hits(&mut index, &filter.search_text("")),
            ["assets/props/rock.glb"]
        );
        let reads = index.meta_reads;
        regia_test_utils::write_atomic(
            &root.join("assets/props/crate.glb.meta"),
            meta_text(2, "agent", "Pending", "none").as_bytes(),
        )
        .expect("write");
        index.refresh_paths(&[root.join("assets/props/crate.glb.meta")]);
        assert_eq!(
            hits(&mut index, &filter.search_text("")),
            ["assets/props/crate.glb", "assets/props/rock.glb"]
        );
        assert_eq!(index.meta_reads - reads, 3, "only assets/props is read again");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod completion_dispatch_tests {
    use super::*;

    #[test]
    fn content_index_completion_uses_the_browser_vocabulary() {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension.method("editor.content_index").expect("registered");
        let reply = handler(&mut world, &json!({"complete": "t"})).expect("completion");
        assert_eq!(reply["candidates"], json!(["t:", "type:"]));
        let reply = handler(&mut world, &json!({"complete": "type:te"})).expect("partial value");
        assert_eq!(reply["completion"], "type:texture");
        let error = handler(&mut world, &json!({"complete": 1})).expect_err("string required");
        assert_eq!(error.data.expect("coded error")["code"], "bad_query");
    }
}
