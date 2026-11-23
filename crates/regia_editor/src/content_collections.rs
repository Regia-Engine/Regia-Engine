// SPDX-License-Identifier: MPL-2.0
//! Content Browser collections (`book/agents/content-browser.md`, ADR-7759): static collections
//! (a named list of assets; adding one never moves a file) and dynamic collections (a saved
//! search), personal or shared.
//!
//! Both files use the `asset-collections` format ([`regia_asset::asset_collections`]): the shared
//! one is the project's `config/asset_collections.ron`, the personal one is the project's
//! user-local `.regia/asset_collections.ron` ([`personal_path`]; `.regia/` is ignored by version
//! control, ADR-7752 rule 3, ADR-7902). A personal file left by an older editor in the user config
//! directory ([`legacy_personal_path`]) moves there once ([`migrate_legacy_personal`]). The
//! personal file also holds the user's favorite folders, folder colors and browser settings
//! (`crate::content_favorites`); folder colors in the shared file apply to everyone, and the
//! personal file's colors win.
//!
//! A static collection stores each member's asset id (from its `.meta`) so a moved asset stays in
//! it, or the project-relative path when the file has no `.meta`. The panel and the agent read the
//! members through the content index ([`crate::content_index::ContentIndex::facts_for`] knows each
//! asset's id). Without [`crate::editor_prefs::EditorConfigPaths`] (tests, tools) the personal file
//! is kept for the session only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::asset_collections::{
    AssetCollection, AssetCollectionsFile, SHARED_COLLECTIONS_PATH, collection_name_problem,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::content::ContentBrowserState;
use crate::content_index::{ContentIndex, IndexedAsset};
use crate::viewport_agent::{Handler, str_param};

/// Where a collection lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionScope {
    /// The user's own file (the project's user-local `.regia/`).
    Personal,
    /// The project's `config/asset_collections.ron`.
    Shared,
}

impl CollectionScope {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Shared => "shared",
        }
    }

    /// The scope a protocol name names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "personal" => Some(Self::Personal),
            "shared" => Some(Self::Shared),
            _ => None,
        }
    }
}

/// A collection operation failed: a protocol code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionError {
    /// `collection_*` code (listed on `book/agents/content-browser.md`).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl CollectionError {
    /// An error with `code` (a protocol code) and `message`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CollectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A 64-bit FNV-1a hash (the personal file's project key; public algorithm, Fowler, Noll, Vo).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The project-relative path of the personal collections file: user-local state under the
/// project's `.regia/`, which version control ignores (ADR-7752 rule 3, ADR-7902).
pub const PERSONAL_COLLECTIONS_PATH: &str = ".regia/asset_collections.ron";

/// The personal collections file of `project_root`.
#[must_use]
pub fn personal_path(project_root: &Path) -> PathBuf {
    project_root.join(PERSONAL_COLLECTIONS_PATH)
}

/// Where editors before ADR-7902 kept `project_root`'s personal collections file: the user config
/// directory, keyed by the canonical project path (so a moved or re-cloned project lost it).
#[must_use]
pub fn legacy_personal_path(config_dir: &Path, project_root: &Path) -> PathBuf {
    let key = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let name = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    config_dir.join("content").join(format!(
        "{safe}-{:016x}.asset_collections.ron",
        fnv1a(key.to_string_lossy().as_bytes())
    ))
}

/// Moves `project_root`'s personal collections file from the user config directory
/// ([`legacy_personal_path`]) to the project's `.regia/` ([`personal_path`]), once: only when the old
/// file exists and the new one does not. The bytes are copied unchanged and the old file is
/// removed only after the new one is written. A file that cannot be read or parsed is left where
/// it is, untouched (the reason is returned so the panel can show it), and nothing is written.
///
/// Returns `Ok(true)` when a file moved, `Ok(false)` when there was nothing to move.
///
/// # Errors
///
/// Why the old file was left in place.
pub fn migrate_legacy_personal(config_dir: &Path, project_root: &Path) -> Result<bool, String> {
    let old = legacy_personal_path(config_dir, project_root);
    let new = personal_path(project_root);
    if !old.is_file() || new.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&old)
        .map_err(|e| format!("cannot read {}: {e}; it was not moved", old.display()))?;
    AssetCollectionsFile::from_ron(&text)
        .map_err(|e| format!("{}: {e}; it was not moved to {}", old.display(), new.display()))?;
    crate::atomic_write::write_atomic(&new, text.as_bytes())?;
    if let Err(e) = std::fs::remove_file(&old) {
        // The new file is in place and wins from now on; the old copy is only left behind.
        tracing::warn!(path = %old.display(), %e, "moved the personal collections file but could not remove the old copy");
    }
    Ok(true)
}

/// Reads a collections file: a missing file is an empty one; a file that cannot be read or parsed
/// is the reason (shown in the panel, and no edit overwrites it until it is fixed).
fn read_file(path: &Path) -> Result<AssetCollectionsFile, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => AssetCollectionsFile::from_ron(&text).map_err(|e| {
            format!(
                "{}: {e}; it is ignored and not written until fixed",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AssetCollectionsFile::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// The loaded collection files (an edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct ContentLibrary {
    root: Option<PathBuf>,
    /// The project's shared file.
    pub shared: AssetCollectionsFile,
    /// The user's personal file.
    pub personal: AssetCollectionsFile,
    personal_file: Option<PathBuf>,
    /// Why a file could not be read (shown in the panel).
    pub errors: Vec<String>,
    /// Bumped by every change (keys cached views).
    pub revision: u64,
    /// Why each scope's file could not be read when it was last read: `[personal, shared]`.
    unreadable: [Option<String>; 2],
}

fn slot(scope: CollectionScope) -> usize {
    match scope {
        CollectionScope::Personal => 0,
        CollectionScope::Shared => 1,
    }
}

impl ContentLibrary {
    /// Loads both files for `root` (once per project; every edit reads its file again first).
    pub fn ensure(&mut self, root: Option<&Path>, config_dir: Option<&Path>) {
        if self.root.as_deref() == root {
            return;
        }
        *self = Self {
            revision: self.revision + 1,
            ..Self::default()
        };
        self.root = root.map(Path::to_path_buf);
        let Some(root) = root else {
            return;
        };
        if let Some(config_dir) = config_dir {
            // ADR-7902: the personal file lives in the project's `.regia/`; an older editor's copy
            // in the user config directory moves there once.
            if let Err(reason) = migrate_legacy_personal(config_dir, root) {
                self.errors.push(reason);
            }
            self.personal_file = Some(personal_path(root));
        }
        for scope in [CollectionScope::Shared, CollectionScope::Personal] {
            // A file that cannot be read is listed as empty plus its error.
            let _ = self.refresh(scope);
        }
    }

    /// The file `scope` is stored in (`None`: a session-only personal file, or no project).
    fn path_of(&self, scope: CollectionScope) -> Option<PathBuf> {
        match scope {
            CollectionScope::Shared => self.root.as_ref().map(|root| root.join(SHARED_COLLECTIONS_PATH)),
            CollectionScope::Personal => self.personal_file.clone(),
        }
    }

    /// Reads `scope`'s file again, so an edit applies to what is on disk now (a teammate's
    /// collections after a pull, a second editor's personal edits) and never overwrites a file this
    /// editor cannot read.
    ///
    /// # Errors
    ///
    /// `collection_file_unreadable` while the file does not parse (a newer version, a merge
    /// conflict); the copy in memory is left as it was.
    pub fn refresh(&mut self, scope: CollectionScope) -> Result<(), CollectionError> {
        let Some(path) = self.path_of(scope) else {
            return Ok(());
        };
        let result = read_file(&path);
        if let Some(old) = self.unreadable[slot(scope)].take() {
            self.errors.retain(|e| *e != old);
        }
        match result {
            Ok(file) => {
                if *self.file(scope) != file {
                    *self.file_mut(scope) = file;
                    self.revision += 1;
                }
                Ok(())
            }
            Err(reason) => {
                self.errors.push(reason.clone());
                self.unreadable[slot(scope)] = Some(reason.clone());
                Err(CollectionError::new("collection_file_unreadable", reason))
            }
        }
    }

    /// Reads both files again (an unreadable one keeps its last copy and reports its error).
    fn refresh_all(&mut self) {
        for scope in [CollectionScope::Personal, CollectionScope::Shared] {
            let _ = self.refresh(scope);
        }
    }

    /// Applies `edit` to `scope`'s file as it is on disk now ([`Self::refresh`]) and writes it when
    /// it changed. The copy in memory changes only after the write succeeds. Returns `edit`'s
    /// result; an error from `edit` writes nothing.
    ///
    /// # Errors
    ///
    /// `edit`'s error, `collection_file_unreadable` (the file is left alone),
    /// `collection_no_project`, `collection_write_failed`.
    pub fn edit<R>(
        &mut self,
        scope: CollectionScope,
        edit: impl FnOnce(&mut AssetCollectionsFile) -> Result<R, CollectionError>,
    ) -> Result<R, CollectionError> {
        self.refresh(scope)?;
        let mut copy = self.file(scope).clone();
        let out = edit(&mut copy)?;
        if copy != *self.file(scope) {
            self.write(scope, &copy)?;
            *self.file_mut(scope) = copy;
            self.revision += 1;
        }
        Ok(out)
    }

    /// The project this library belongs to.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// One scope's file.
    #[must_use]
    pub fn file(&self, scope: CollectionScope) -> &AssetCollectionsFile {
        match scope {
            CollectionScope::Personal => &self.personal,
            CollectionScope::Shared => &self.shared,
        }
    }

    fn file_mut(&mut self, scope: CollectionScope) -> &mut AssetCollectionsFile {
        match scope {
            CollectionScope::Personal => &mut self.personal,
            CollectionScope::Shared => &mut self.shared,
        }
    }

    /// Every collection: `(scope, collection)`, personal first, each in file order.
    #[must_use]
    pub fn all(&self) -> Vec<(CollectionScope, &AssetCollection)> {
        let personal = self
            .personal
            .collections
            .iter()
            .map(|c| (CollectionScope::Personal, c));
        let shared = self
            .shared
            .collections
            .iter()
            .map(|c| (CollectionScope::Shared, c));
        personal.chain(shared).collect()
    }

    /// The collection named `name` (ignoring case) and its scope; `scope` narrows the lookup.
    #[must_use]
    pub fn find(
        &self,
        name: &str,
        scope: Option<CollectionScope>,
    ) -> Option<(CollectionScope, &AssetCollection)> {
        self.all()
            .into_iter()
            .filter(|(s, _)| scope.is_none_or(|want| want == *s))
            .find(|(_, c)| c.name.trim().eq_ignore_ascii_case(name.trim()))
    }

    /// Writes `file` as one scope's file (the shared file under the project's `config/`, the
    /// personal file under its `.regia/`; a session-only personal file is not written).
    fn write(&self, scope: CollectionScope, file: &AssetCollectionsFile) -> Result<(), CollectionError> {
        let path = match scope {
            CollectionScope::Shared => match &self.root {
                Some(root) => root.join(SHARED_COLLECTIONS_PATH),
                None => {
                    return Err(CollectionError::new(
                        "collection_no_project",
                        "no project is open",
                    ));
                }
            },
            CollectionScope::Personal => match &self.personal_file {
                Some(path) => path.clone(),
                None => return Ok(()),
            },
        };
        let text = file
            .to_ron_string()
            .map_err(|e| CollectionError::new("collection_write_failed", e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CollectionError::new(
                    "collection_write_failed",
                    format!("cannot create {}: {e}", parent.display()),
                )
            })?;
        }
        crate::atomic_write::write_atomic(&path, text.as_bytes())
            .map_err(|e| CollectionError::new("collection_write_failed", e))
    }

    /// Creates an empty static collection, or a dynamic one when `query` is set (the query must
    /// parse).
    ///
    /// # Errors
    ///
    /// `collection_bad_name`, `collection_exists`, `collection_bad_query` or an [`Self::edit`]
    /// error.
    pub fn create(
        &mut self,
        scope: CollectionScope,
        name: &str,
        query: Option<&str>,
    ) -> Result<(), CollectionError> {
        if let Some(problem) = collection_name_problem(name) {
            return Err(CollectionError::new(
                "collection_bad_name",
                format!("collection name {name:?} {problem}"),
            ));
        }
        if let Some(query) = query {
            crate::asset_search::AssetQuery::parse(query)
                .map_err(|e| CollectionError::new("collection_bad_query", format!("query {query:?}: {e}")))?;
        }
        self.refresh_all();
        if self.find(name, None).is_some() {
            return Err(CollectionError::new(
                "collection_exists",
                format!("a collection named {:?} already exists", name.trim()),
            ));
        }
        self.edit(scope, |file| {
            if file.find_mut(name).is_some() {
                return Err(CollectionError::new(
                    "collection_exists",
                    format!("a collection named {:?} already exists", name.trim()),
                ));
            }
            file.collections.push(AssetCollection {
                name: name.trim().to_string(),
                assets: Vec::new(),
                query: query.map(str::to_string),
            });
            Ok(())
        })
    }

    /// Deletes a collection (its assets stay where they are).
    ///
    /// # Errors
    ///
    /// `collection_not_found` or an [`Self::edit`] error.
    pub fn delete(&mut self, name: &str) -> Result<CollectionScope, CollectionError> {
        self.refresh_all();
        let (scope, _) = self.find(name, None).ok_or_else(|| not_found(name))?;
        self.edit(scope, |file| {
            let before = file.collections.len();
            file.collections
                .retain(|c| !c.name.trim().eq_ignore_ascii_case(name.trim()));
            if file.collections.len() == before {
                return Err(not_found(name));
            }
            Ok(scope)
        })
    }

    /// Adds members (keys from [`member_key`]) to a static collection; returns how many were new.
    ///
    /// # Errors
    ///
    /// `collection_not_found`, `collection_is_dynamic` or an [`Self::edit`] error.
    pub fn add(&mut self, name: &str, keys: &[String]) -> Result<usize, CollectionError> {
        self.refresh_all();
        let scope = self.static_scope(name)?;
        self.edit(scope, |file| {
            let collection = static_in(file, name)?;
            let mut added = 0;
            for key in keys {
                if !collection.assets.contains(key) {
                    collection.assets.push(key.clone());
                    added += 1;
                }
            }
            Ok(added)
        })
    }

    /// Removes members from a static collection; returns how many were removed.
    ///
    /// # Errors
    ///
    /// `collection_not_found`, `collection_is_dynamic` or an [`Self::edit`] error.
    pub fn remove(&mut self, name: &str, keys: &[String]) -> Result<usize, CollectionError> {
        self.refresh_all();
        let scope = self.static_scope(name)?;
        self.edit(scope, |file| {
            let collection = static_in(file, name)?;
            let before = collection.assets.len();
            collection.assets.retain(|key| !keys.contains(key));
            Ok(before - collection.assets.len())
        })
    }

    fn static_scope(&self, name: &str) -> Result<CollectionScope, CollectionError> {
        let (scope, collection) = self.find(name, None).ok_or_else(|| not_found(name))?;
        if collection.is_dynamic() {
            return Err(dynamic(&collection.name));
        }
        Ok(scope)
    }

    /// The folder color token of `folder` (personal over shared).
    #[must_use]
    pub fn folder_color(&self, folder: &str) -> Option<&str> {
        self.personal
            .folder_colors
            .get(folder)
            .or_else(|| self.shared.folder_colors.get(folder))
            .map(String::as_str)
    }
}

fn not_found(name: &str) -> CollectionError {
    CollectionError::new(
        "collection_not_found",
        format!("no collection named {:?}", name.trim()),
    )
}

/// The static collection `name` in `file` as it is on disk now.
fn static_in<'a>(
    file: &'a mut AssetCollectionsFile,
    name: &str,
) -> Result<&'a mut AssetCollection, CollectionError> {
    let collection = file.find_mut(name).ok_or_else(|| not_found(name))?;
    if collection.is_dynamic() {
        return Err(dynamic(&collection.name));
    }
    Ok(collection)
}

fn dynamic(name: &str) -> CollectionError {
    CollectionError::new(
        "collection_is_dynamic",
        format!("{name:?} is a saved search: change its query instead of adding assets"),
    )
}

/// The key a static collection stores for `rel` (project-relative): the asset id from its `.meta`,
/// or `rel` when it has none.
#[must_use]
pub fn member_key(project_root: &Path, rel: &str) -> String {
    std::fs::read(regia_asset::meta_path_for(&project_root.join(rel)))
        .ok()
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
        .map_or_else(|| rel.to_string(), |meta| meta.id.to_string())
}

/// What a collection lists: its assets (project-relative, sorted for a static collection, best
/// matches first for a dynamic one) and the static members no longer found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Members {
    /// The assets.
    pub paths: Vec<String>,
    /// Static members whose asset is gone.
    pub missing: Vec<String>,
}

/// The members of `collection` through the content index.
pub fn members(index: &mut ContentIndex, collection: &AssetCollection) -> Members {
    if let Some(query) = &collection.query {
        return Members {
            paths: index
                .visible("", query)
                .iter()
                .map(|(rel, _)| rel.clone())
                .collect(),
            missing: Vec::new(),
        };
    }
    let assets = index.assets();
    let facts = if collection.assets.iter().any(|key| !key.contains('/')) {
        Some(index.facts_for(false))
    } else {
        None
    };
    let mut out = Members::default();
    for key in &collection.assets {
        let found = if key.contains('/') {
            assets
                .binary_search_by(|(rel, _)| rel.as_str().cmp(key.as_str()))
                .ok()
        } else {
            facts
                .as_ref()
                .and_then(|facts| facts.iter().position(|f| f.meta.get("id") == Some(key)))
        };
        match found.and_then(|at| assets.get(at)) {
            Some((rel, _)) => out.paths.push(rel.clone()),
            None => out.missing.push(key.clone()),
        }
    }
    out.paths.sort();
    out.paths.dedup();
    out
}

/// Loads the world's library for the open project (the panel and the agent call this first).
pub fn library(world: &mut World) -> Option<PathBuf> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    let config_dir = world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|paths| paths.dir.clone());
    world
        .get_resource_or_insert_with(ContentLibrary::default)
        .ensure(root.as_deref(), config_dir.as_deref());
    root
}

/// Adds `paths` (project-relative) to the static collection `name`: what the panel's drop on a
/// collection does. Returns the toast line.
pub fn add_paths(world: &mut World, name: &str, paths: &[String]) -> String {
    let Some(root) = library(world) else {
        return "no project is open".to_string();
    };
    let keys: Vec<String> = paths.iter().map(|rel| member_key(&root, rel)).collect();
    let mut library = world.resource_mut::<ContentLibrary>();
    match library.add(name, &keys) {
        Ok(added) => format!("Added {added} asset(s) to {name}"),
        Err(e) => e.message,
    }
}

/// The members the panel listed last, keyed by the collection, the index generation, the index's
/// world-state revision, library revision and combined typed/chip search (an edit-world resource, so a collection is
/// resolved once, not per frame; review row 17).
#[derive(Resource, Debug, Default)]
pub struct CollectionView {
    view: Option<(CollectionViewKey, Arc<[IndexedAsset]>)>,
    /// Member lists built so far (an idle frame builds none).
    pub builds: u64,
}

/// What a [`CollectionView`] was built for: the collection, the index generation, the world-state
/// revision, library revision and combined typed/chip search.
type CollectionViewKey = (String, String, u64, u64, u64);

/// The folder list and the members of collection `name` for the panel (in place of
/// [`crate::content_index::panel_view`] while a collection is shown). A collection that is gone
/// lists nothing.
pub fn panel_view(
    world: &mut World,
    state: &ContentBrowserState,
    name: &str,
) -> (Arc<[String]>, Arc<[IndexedAsset]>) {
    library(world);
    let library = world.resource::<ContentLibrary>();
    let revision = library.revision;
    // A saved search with `is:` words follows the unsaved documents and the validator findings.
    let query = library.find(name, None).and_then(|(_, c)| c.query.clone());
    let search = world
        .get_resource::<crate::content_index::ContentFilter>()
        .map_or_else(
            || state.search.clone(),
            |filter| filter.search_text(&state.search),
        );
    // Resolve world flags for either predicate without merging their type/tag groups.
    let states = crate::content_index::world_states(
        world,
        &format!("{} {search}", query.as_deref().unwrap_or_default()),
    );
    let mut index = world.get_resource_or_insert_with(ContentIndex::default);
    if let Some(states) = states {
        index.set_world_states(states);
    }
    index.maybe_check(std::time::Instant::now());
    index.ensure(state.project_root.as_deref());
    let folders = index.folders();
    let key = (
        name.to_string(),
        search.clone(),
        index.generation,
        index.world_revision(),
        revision,
    );
    let cached = world
        .get_resource::<CollectionView>()
        .and_then(|view| view.view.as_ref())
        .filter(|(at, _)| *at == key)
        .map(|(_, view)| Arc::clone(view));
    if let Some(view) = cached {
        return (folders, view);
    }
    let collection = world
        .resource::<ContentLibrary>()
        .find(name, None)
        .map(|(_, c)| c.clone());
    let mut index = world.resource_mut::<ContentIndex>();
    let _span = tracing::info_span!(target: "regia_editor", "collection_filter").entered();
    let member_paths = collection
        .as_ref()
        .map(|c| members(&mut index, c).paths)
        .unwrap_or_default();
    let view: Arc<[IndexedAsset]> = if search.trim().is_empty() {
        // Unlike a folder view, blank search includes members in every folder.
        if query.is_none() {
            index.visible("", "");
        }
        member_paths
            .into_iter()
            .map(|rel| {
                let kind = crate::dnd::kind_of_path(&rel);
                (rel, kind)
            })
            .collect()
    } else {
        let members: std::collections::BTreeSet<_> = member_paths.into_iter().collect();
        index
            .visible("", &search)
            .iter()
            .filter(|(rel, _)| members.contains(rel))
            .cloned()
            .collect()
    };
    let mut cache = world.get_resource_or_insert_with(CollectionView::default);
    cache.view = Some((key, Arc::clone(&view)));
    cache.builds += 1;
    (folders, view)
}

fn rpc(error: CollectionError) -> RpcError {
    let mut rpc = match error.code {
        "collection_not_found" => RpcError::not_found(error.message, "list them with editor.collection_list"),
        "collection_write_failed" => RpcError::internal(error.message),
        _ => RpcError::invalid_params(error.message),
    };
    if let Some(data) = rpc.data.as_mut() {
        data["collection_error"] = json!(error.code);
    } else {
        rpc.data = Some(json!({ "collection_error": error.code }));
    }
    rpc
}

fn need_project(world: &mut World) -> Result<PathBuf, RpcError> {
    library(world).ok_or_else(|| {
        rpc(CollectionError::new(
            "collection_no_project",
            "no project is open: start the editor with --project <dir>",
        ))
    })
}

fn name_param(params: &Value) -> Result<String, RpcError> {
    str_param(params, "name").map(str::to_string).ok_or_else(|| {
        rpc(CollectionError::new(
            "collection_bad_params",
            "`name` (a string) is required",
        ))
    })
}

fn paths_param(params: &Value) -> Result<Vec<String>, RpcError> {
    let bad = || {
        rpc(CollectionError::new(
            "collection_bad_params",
            "`paths` must be a non-empty array of project-relative paths",
        ))
    };
    let list = params.get("paths").and_then(Value::as_array).ok_or_else(bad)?;
    if list.is_empty() {
        return Err(bad());
    }
    list.iter()
        .map(|p| p.as_str().map(|p| p.trim().replace('\\', "/")).ok_or_else(bad))
        .collect()
}

fn collection_json(index: &mut ContentIndex, scope: CollectionScope, collection: &AssetCollection) -> Value {
    let members = members(index, collection);
    json!({
        "name": collection.name,
        "scope": scope.name(),
        "kind": if collection.is_dynamic() { "dynamic" } else { "static" },
        "query": collection.query,
        "count": members.paths.len(),
        "paths": members.paths.iter().take(500).collect::<Vec<_>>(),
        "missing": members.missing,
    })
}

/// `editor.collection_list`.
fn collection_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = need_project(world)?;
    let only = str_param(params, "name").map(str::to_string);
    let library = world.resource::<ContentLibrary>().clone();
    let mut index = world.get_resource_or_insert_with(ContentIndex::default);
    index.ensure(Some(&root));
    let mut out = Vec::new();
    for (scope, collection) in library.all() {
        if only
            .as_ref()
            .is_some_and(|name| !collection.name.eq_ignore_ascii_case(name.trim()))
        {
            continue;
        }
        out.push(collection_json(&mut index, scope, collection));
    }
    if let Some(name) = only
        && out.is_empty()
    {
        return Err(rpc(not_found(&name)));
    }
    Ok(json!({ "collections": out, "errors": library.errors }))
}

/// `editor.collection_create`.
fn collection_create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    need_project(world)?;
    let name = name_param(params)?;
    let scope = match str_param(params, "scope") {
        None => CollectionScope::Personal,
        Some(text) => CollectionScope::from_name(text).ok_or_else(|| {
            rpc(CollectionError::new(
                "collection_bad_params",
                "`scope` is personal or shared",
            ))
        })?,
    };
    let query = str_param(params, "query").map(str::to_string);
    world
        .resource_mut::<ContentLibrary>()
        .create(scope, &name, query.as_deref())
        .map_err(rpc)?;
    collection_list(world, &json!({ "name": name }))
}

/// `editor.collection_add` and `editor.collection_remove`.
fn collection_members(world: &mut World, params: &Value, add: bool) -> Result<Value, RpcError> {
    let root = need_project(world)?;
    let name = name_param(params)?;
    let paths = paths_param(params)?;
    let mut keys = Vec::new();
    for rel in &paths {
        if add && !root.join(rel).is_file() {
            return Err(rpc(CollectionError::new(
                "collection_asset_not_found",
                format!("{rel} is not a file in the project"),
            )));
        }
        keys.push(member_key(&root, rel));
        // A path stored before the asset had a `.meta` is removed by its path too.
        if !add {
            keys.push(rel.clone());
        }
    }
    let changed = {
        let mut library = world.resource_mut::<ContentLibrary>();
        if add {
            library.add(&name, &keys)
        } else {
            library.remove(&name, &keys)
        }
        .map_err(rpc)?
    };
    let mut out = collection_list(world, &json!({ "name": name }))?;
    out[if add { "added" } else { "removed" }] = json!(changed);
    Ok(out)
}

fn collection_add(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    collection_members(world, params, true)
}

fn collection_remove(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    collection_members(world, params, false)
}

/// `editor.collection_delete`.
fn collection_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    need_project(world)?;
    let name = name_param(params)?;
    let scope = world
        .resource_mut::<ContentLibrary>()
        .delete(&name)
        .map_err(rpc)?;
    Ok(json!({ "deleted": name, "scope": scope.name() }))
}

const NAME: ParamSchema = ParamSchema {
    name: "name",
    kind: "string",
    required: true,
    description: "The collection's name (any case).",
};

const PATHS: ParamSchema = ParamSchema {
    name: "paths",
    kind: "array",
    required: true,
    description: "Project-relative asset paths (assets/...).",
};

const ONE: &str = "{ collections: [{ name, scope, kind, query, count, paths, missing }], errors }";

/// The collections' agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.collection_list",
                tool: "collection_list",
                description: "The Content Browser's collections, personal then shared: each one's kind (static list \
                    or dynamic saved search), query, members (project-relative paths, up to 500) and the static \
                    members whose asset is gone. `errors` names a collections file that could not be read.",
                params: &[ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "Only this collection.",
                }],
                result: ONE,
                mutating: false,
                permission: Permission::Read,
            },
            collection_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collection_create",
                tool: "collection_create",
                description: "Creates an empty static collection, or a dynamic one (a saved search) when `query` is \
                    set. Personal collections live in the user's config, shared ones in \
                    config/asset_collections.ron.",
                params: &[
                    NAME,
                    ParamSchema {
                        name: "scope",
                        kind: "string",
                        required: false,
                        description: "personal (default) or shared.",
                    },
                    ParamSchema {
                        name: "query",
                        kind: "string",
                        required: false,
                        description: "A Content Browser search (words, type:, tag:, is:, meta:): makes the \
                            collection dynamic.",
                    },
                ],
                result: ONE,
                mutating: true,
                permission: Permission::Assets,
            },
            collection_create as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collection_add",
                tool: "collection_add",
                description: "Adds assets to a static collection (by asset id when the asset has a .meta, so a \
                    moved asset stays in it). Files are never moved.",
                params: &[NAME, PATHS],
                result: "{ collections: [one], errors, added }",
                mutating: true,
                permission: Permission::Assets,
            },
            collection_add as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collection_remove",
                tool: "collection_remove",
                description: "Removes assets from a static collection (the files stay).",
                params: &[NAME, PATHS],
                result: "{ collections: [one], errors, removed }",
                mutating: true,
                permission: Permission::Assets,
            },
            collection_remove as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collection_delete",
                tool: "collection_delete",
                description: "Deletes a collection. Its assets stay where they are.",
                params: &[NAME],
                result: "{ deleted, scope }",
                mutating: true,
                permission: Permission::Assets,
            },
            collection_delete as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        for rel in [
            "assets/props/rock.glb",
            "assets/props/crate.glb",
            "assets/tex/moss.png",
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            regia_test_utils::write_atomic(&path, b"x").expect("write");
        }
        let meta = regia_asset::MetaSidecar::new("gltf", regia_asset::AssetId::new_random());
        regia_test_utils::write_atomic(
            &regia_asset::meta_path_for(&root.join("assets/props/rock.glb")),
            meta.to_ron_string().expect("ron").as_bytes(),
        )
        .expect("meta");
        root
    }

    fn world_for(root: &Path, config: Option<&Path>) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        if let Some(config) = config {
            world.insert_resource(crate::editor_prefs::EditorConfigPaths {
                dir: Some(config.to_path_buf()),
            });
        }
        world
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("collection_error")?.as_str()
    }

    #[test]
    fn the_search_filters_a_shown_collection() {
        let root = project("collection_search");
        let mut world = world_for(&root, None);
        collection_create(&mut world, &json!({"name": "Mixed"})).expect("collection");
        collection_add(
            &mut world,
            &json!({"name": "Mixed", "paths": [
                "assets/props/rock.glb", "assets/tex/moss.png"
            ]}),
        )
        .expect("members");
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            search: "rock type:mesh".into(),
            ..Default::default()
        };
        let paths = |world: &mut World, state: &ContentBrowserState, name| {
            panel_view(world, state, name)
                .1
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&mut world, &state, "Mixed"), ["assets/props/rock.glb"]);
        state.search.clear();
        world.insert_resource(crate::content_index::ContentFilter {
            kinds: vec!["texture".into()],
            ..Default::default()
        });
        assert_eq!(paths(&mut world, &state, "Mixed"), ["assets/tex/moss.png"]);
        world.remove_resource::<crate::content_index::ContentFilter>();
        assert_eq!(paths(&mut world, &state, "Mixed").len(), 2);
        let builds = world.resource::<CollectionView>().builds;
        assert_eq!(paths(&mut world, &state, "Mixed").len(), 2);
        assert_eq!(world.resource::<CollectionView>().builds, builds);
        collection_create(&mut world, &json!({"name": "Meshes", "query": "type:mesh"})).expect("dynamic");
        state.search = "rock".into();
        assert_eq!(paths(&mut world, &state, "Meshes"), ["assets/props/rock.glb"]);
        state.search = "moss".into();
        assert!(paths(&mut world, &state, "Meshes").is_empty());
        state.search = "type:unknown".into();
        assert!(paths(&mut world, &state, "Mixed").is_empty());
        assert!(world.resource::<ContentIndex>().query_error().is_some());
        state.search.clear();
        assert_eq!(paths(&mut world, &state, "Mixed").len(), 2);
        assert!(world.resource::<ContentIndex>().query_error().is_none());
        std::fs::create_dir_all(root.join("config")).expect("config");
        regia_test_utils::write_atomic(
            &root.join("config/asset_tags.ron"),
            br#"(format_version: 1, rules: [(paths: ["assets/props/**"], tags: {"biome": "Desert"})])"#,
        )
        .expect("tags");
        crate::content_index::invalidate(&mut world);
        world.insert_resource(crate::content_index::ContentFilter {
            tags: vec!["biome=desert".into()],
            ..Default::default()
        });
        assert_eq!(paths(&mut world, &state, "Mixed"), ["assets/props/rock.glb"]);
        let mut panels = crate::asset_panels::AssetPanels::default();
        panels.problems.rows.push(crate::asset_panels::ProblemRow {
            severity: regia_asset_pipeline::validators::Severity::Warning,
            code: "fixture".into(),
            validator: "fixture".into(),
            asset: "assets/tex/moss.png".into(),
            text: "Fixture finding".into(),
            hint: None,
        });
        world.insert_resource(panels);
        world.insert_resource(crate::content_index::ContentFilter {
            states: vec![crate::asset_search::AssetState::Problem],
            ..Default::default()
        });
        assert_eq!(paths(&mut world, &state, "Mixed"), ["assets/tex/moss.png"]);
        world
            .resource_mut::<crate::asset_panels::AssetPanels>()
            .problems
            .rows
            .clear();
        assert!(
            paths(&mut world, &state, "Mixed").is_empty(),
            "world-state changes invalidate the filtered collection"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn static_collections_keep_ids_and_never_move_files() {
        let root = project("collections_static");
        let config = regia_test_utils::unique_temp_dir("collections_static_cfg");
        let mut world = world_for(&root, Some(&config));
        collection_create(&mut world, &json!({ "name": "Hero" })).expect("create");
        let reply = collection_add(
            &mut world,
            &json!({ "name": "hero", "paths": ["assets/props/rock.glb", "assets/tex/moss.png"] }),
        )
        .expect("add");
        assert_eq!(reply["added"], 2);
        assert_eq!(
            reply["collections"][0]["paths"],
            json!(["assets/props/rock.glb", "assets/tex/moss.png"])
        );
        assert!(
            root.join("assets/props/rock.glb").is_file(),
            "adding never moves a file"
        );
        // rock is stored by id: after a move it is still a member; moss (no .meta) by path.
        let personal = personal_path(&root);
        let text = std::fs::read_to_string(&personal).expect("personal file written");
        assert!(
            text.contains("assets/tex/moss.png") && !text.contains("rock.glb"),
            "{text}"
        );
        regia_asset::project_ops::move_asset(&root, "assets/props/rock.glb", "assets/props/boulder.glb")
            .expect("move");
        crate::content_index::invalidate(&mut world);
        let after = collection_list(&mut world, &json!({ "name": "HERO" })).expect("list");
        assert_eq!(
            after["collections"][0]["paths"],
            json!(["assets/props/boulder.glb", "assets/tex/moss.png"])
        );
        // A fresh editor reads the personal file back.
        let mut again = world_for(&root, Some(&config));
        let listed = collection_list(&mut again, &json!({})).expect("list");
        assert_eq!(listed["collections"][0]["count"], 2, "{listed}");
        let removed = collection_remove(
            &mut again,
            &json!({ "name": "Hero", "paths": ["assets/tex/moss.png"] }),
        )
        .expect("remove");
        assert_eq!(removed["removed"], 1);
        assert!(root.join("assets/tex/moss.png").is_file());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn dynamic_collections_are_saved_searches_in_the_shared_file() {
        let root = project("collections_dynamic");
        let mut world = world_for(&root, None);
        let reply = collection_create(
            &mut world,
            &json!({ "name": "Meshes", "scope": "shared", "query": "type:mesh" }),
        )
        .expect("create");
        assert_eq!(reply["collections"][0]["kind"], "dynamic");
        assert_eq!(
            reply["collections"][0]["paths"],
            json!(["assets/props/crate.glb", "assets/props/rock.glb"])
        );
        let shared = std::fs::read_to_string(root.join(SHARED_COLLECTIONS_PATH)).expect("shared file");
        let parsed = AssetCollectionsFile::from_ron(&shared).expect("valid file");
        assert_eq!(parsed.collections[0].query.as_deref(), Some("type:mesh"));
        let err = collection_add(
            &mut world,
            &json!({ "name": "meshes", "paths": ["assets/tex/moss.png"] }),
        )
        .expect_err("dynamic");
        assert_eq!(code(&err), Some("collection_is_dynamic"));
        let err = collection_create(&mut world, &json!({ "name": "MESHES" })).expect_err("dup");
        assert_eq!(code(&err), Some("collection_exists"));
        let err = collection_create(&mut world, &json!({ "name": "x", "query": "type:sprite" }))
            .expect_err("query");
        assert_eq!(code(&err), Some("collection_bad_query"));
        let err = collection_create(&mut world, &json!({ "name": "  " })).expect_err("name");
        assert_eq!(code(&err), Some("collection_bad_name"));
        let err = collection_create(&mut world, &json!({ "name": "y", "scope": "team" })).expect_err("scope");
        assert_eq!(code(&err), Some("collection_bad_params"));
        let err = collection_add(
            &mut world,
            &json!({ "name": "nope", "paths": ["assets/tex/moss.png"] }),
        )
        .expect_err("missing");
        assert_eq!(code(&err), Some("collection_not_found"));
        collection_create(&mut world, &json!({ "name": "Loose" })).expect("create");
        let err = collection_add(
            &mut world,
            &json!({ "name": "Loose", "paths": ["assets/none.png"] }),
        )
        .expect_err("no file");
        assert_eq!(code(&err), Some("collection_asset_not_found"));
        let deleted = collection_delete(&mut world, &json!({ "name": "meshes" })).expect("delete");
        assert_eq!(deleted["scope"], "shared");
        assert!(
            root.join("assets/props/crate.glb").is_file(),
            "deleting a collection keeps its assets"
        );
        let mut empty = World::new();
        let err = collection_list(&mut empty, &json!({})).expect_err("no project");
        assert_eq!(code(&err), Some("collection_no_project"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bad_shared_file_is_reported_and_ignored() {
        let root = project("collections_bad_file");
        std::fs::create_dir_all(root.join("config")).expect("config");
        regia_test_utils::write_atomic(&root.join(SHARED_COLLECTIONS_PATH), b"(format_version: 9)")
            .expect("write");
        let mut world = world_for(&root, None);
        let listed = collection_list(&mut world, &json!({})).expect("list");
        assert_eq!(listed["collections"], json!([]));
        assert!(
            listed["errors"][0]
                .as_str()
                .is_some_and(|e| e.contains("not supported")),
            "{listed}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 17: a shown saved search is built once while nothing changes, and an `is:`
    /// search follows the world states.
    #[test]
    fn a_shown_saved_search_is_built_once_while_idle() {
        let root = project("collections_idle");
        let mut world = world_for(&root, None);
        collection_create(&mut world, &json!({ "name": "Meshes", "query": "type:mesh" })).expect("create");
        let state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        for _ in 0..10 {
            let (_, view) = panel_view(&mut world, &state, "Meshes");
            assert_eq!(view.len(), 2);
        }
        assert_eq!(
            world.resource::<CollectionView>().builds,
            1,
            "idle frames reuse it"
        );
        collection_create(&mut world, &json!({ "name": "Touched", "query": "is:modified" })).expect("create");
        for _ in 0..3 {
            panel_view(&mut world, &state, "Touched");
        }
        assert_eq!(world.resource::<CollectionView>().builds, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 13 (a): an unreadable shared file (a newer editor's version, a merge conflict)
    /// is never overwritten by an edit; the edit is refused until the file parses again.
    #[test]
    fn an_unreadable_shared_file_is_never_overwritten() {
        let root = project("collections_unreadable");
        std::fs::create_dir_all(root.join("config")).expect("config");
        let shared = root.join(SHARED_COLLECTIONS_PATH);
        let bad = b"<<<<<<< HEAD\n(format_version: 1, collections: [(name: \"Team\")])\n=======\n";
        regia_test_utils::write_atomic(&shared, bad).expect("write");
        let mut world = world_for(&root, None);
        for params in [
            json!({ "name": "Mine", "scope": "shared" }),
            json!({ "name": "Mine", "scope": "shared", "query": "type:mesh" }),
        ] {
            let err = collection_create(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), Some("collection_file_unreadable"), "{err:?}");
            assert_eq!(
                std::fs::read(&shared).expect("still there"),
                bad,
                "the team's file is byte-identical"
            );
        }
        // Fixed on disk: the next edit goes through and keeps what the fixed file holds.
        let fixed = "(format_version: 1, collections: [(name: \"Team\")])";
        regia_test_utils::write_atomic(&shared, fixed.as_bytes()).expect("write");
        collection_create(&mut world, &json!({ "name": "Mine", "scope": "shared" })).expect("create");
        let parsed =
            AssetCollectionsFile::from_ron(&std::fs::read_to_string(&shared).expect("read")).expect("valid");
        let names: Vec<&str> = parsed.collections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Team", "Mine"]);
        let listed = collection_list(&mut world, &json!({})).expect("list");
        assert_eq!(listed["errors"], json!([]), "the old error is gone: {listed}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7902 (lead decision on ADR-7798 (7)): the personal file is the project's user-local
    /// `.regia/asset_collections.ron`, never a file in the user config directory.
    #[test]
    fn the_personal_file_lives_in_the_projects_regia_folder() {
        let root = project("collections_personal_regia");
        let config = regia_test_utils::unique_temp_dir("collections_personal_regia_cfg");
        let mut world = world_for(&root, Some(&config));
        collection_create(&mut world, &json!({ "name": "Mine" })).expect("create");
        let personal = root.join(".regia/asset_collections.ron");
        assert_eq!(personal_path(&root), personal);
        let text = std::fs::read_to_string(&personal).expect("personal file in .regia");
        assert!(text.contains("Mine"), "{text}");
        assert!(
            !legacy_personal_path(&config, &root).exists(),
            "nothing is written to the user config directory"
        );
        // A project copied (or cloned) elsewhere keeps its personal collections.
        let moved = regia_test_utils::unique_temp_dir("collections_personal_regia_moved");
        std::fs::create_dir_all(moved.join(".regia")).expect("dirs");
        std::fs::copy(&personal, moved.join(".regia/asset_collections.ron")).expect("copy");
        let mut again = world_for(&moved, Some(&config));
        let listed = collection_list(&mut again, &json!({})).expect("list");
        assert_eq!(listed["collections"][0]["name"], "Mine", "{listed}");
        for dir in [&root, &config, &moved] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// ADR-7902: an older editor's personal file in the user config directory moves to `.regia/`
    /// once, byte for byte; afterwards the `.regia/` file wins and the old location is not read.
    #[test]
    fn an_old_personal_file_moves_to_regia_once() {
        let root = project("collections_personal_migrate");
        let config = regia_test_utils::unique_temp_dir("collections_personal_migrate_cfg");
        let old = legacy_personal_path(&config, &root);
        std::fs::create_dir_all(old.parent().expect("parent")).expect("dirs");
        let hand =
            "// mine\n(format_version: 1, collections: [(name: \"Old\")], favorites: [\"assets/tex\"])\n";
        regia_test_utils::write_atomic(&old, hand.as_bytes()).expect("old file");
        let mut world = world_for(&root, Some(&config));
        let listed = collection_list(&mut world, &json!({})).expect("list");
        assert_eq!(listed["collections"][0]["name"], "Old", "{listed}");
        let new = personal_path(&root);
        assert_eq!(
            std::fs::read_to_string(&new).expect("moved"),
            hand,
            "the bytes move unchanged"
        );
        assert!(!old.exists(), "the old copy is removed after the move");
        // An old file written again later (an editor still on the old version) never replaces it.
        let stale = "(format_version: 1, collections: [(name: \"Stale\")])";
        regia_test_utils::write_atomic(&old, stale.as_bytes()).expect("stale");
        assert_eq!(migrate_legacy_personal(&config, &root), Ok(false));
        assert_eq!(std::fs::read_to_string(&new).expect("read"), hand);
        assert_eq!(
            std::fs::read_to_string(&old).expect("read"),
            stale,
            "and is left alone"
        );
        let mut again = world_for(&root, Some(&config));
        let listed = collection_list(&mut again, &json!({})).expect("list");
        assert_eq!(
            listed["collections"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );
        assert_eq!(listed["collections"][0]["name"], "Old");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }

    /// ADR-7902: an old personal file that does not parse (a newer editor's, a broken edit) is
    /// never moved, rewritten or removed, and the browser says why.
    #[test]
    fn an_unreadable_old_personal_file_is_left_untouched() {
        let root = project("collections_personal_unreadable");
        let config = regia_test_utils::unique_temp_dir("collections_personal_unreadable_cfg");
        let old = legacy_personal_path(&config, &root);
        std::fs::create_dir_all(old.parent().expect("parent")).expect("dirs");
        let newer = b"(format_version: 2, collections: [(name: \"Future\")], unknown_field: 3)";
        regia_test_utils::write_atomic(&old, newer).expect("old file");
        let err = migrate_legacy_personal(&config, &root).expect_err("not moved");
        assert!(err.contains("not moved"), "{err}");
        let mut world = world_for(&root, Some(&config));
        let _ = library(&mut world);
        let errors = world.resource::<ContentLibrary>().errors.clone();
        assert!(errors.iter().any(|e| e.contains("not moved")), "{errors:?}");
        assert_eq!(std::fs::read(&old).expect("still there"), newer, "byte-identical");
        assert!(!personal_path(&root).exists(), "nothing written to .regia");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }

    /// Review row 13 (b): a teammate's collections pulled after the editor read the file (or a
    /// second editor's personal edits) survive the next edit: each edit is applied to the file as
    /// it is on disk now.
    #[test]
    fn an_edit_keeps_collections_written_by_someone_else() {
        let root = project("collections_external");
        let config = regia_test_utils::unique_temp_dir("collections_external_cfg");
        let mut world = world_for(&root, Some(&config));
        collection_create(&mut world, &json!({ "name": "Mine", "scope": "shared" })).expect("create");
        collection_create(&mut world, &json!({ "name": "Own" })).expect("create");
        // A `git pull` brings a teammate's collection; another editor adds a personal one.
        let shared = root.join(SHARED_COLLECTIONS_PATH);
        let pulled = "(format_version: 1, collections: [(name: \"Mine\"), (name: \"Theirs\", query: Some(\"type:mesh\"))])";
        regia_test_utils::write_atomic(&shared, pulled.as_bytes()).expect("pull");
        let personal = personal_path(&root);
        let other = "(format_version: 1, collections: [(name: \"Own\"), (name: \"Other window\")])";
        regia_test_utils::write_atomic(&personal, other.as_bytes()).expect("other editor");
        collection_add(
            &mut world,
            &json!({ "name": "Mine", "paths": ["assets/tex/moss.png"] }),
        )
        .expect("add");
        collection_create(&mut world, &json!({ "name": "Late" })).expect("create");
        let read = |path: &Path| {
            AssetCollectionsFile::from_ron(&std::fs::read_to_string(path).expect("read"))
                .expect("valid")
                .collections
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(read(&shared), ["Mine", "Theirs"], "the pulled collection stays");
        assert_eq!(read(&personal), ["Own", "Other window", "Late"]);
        let listed = collection_list(&mut world, &json!({})).expect("list");
        assert_eq!(
            listed["collections"].as_array().map(Vec::len),
            Some(5),
            "{listed}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }

    /// Review row 32: a failed write leaves nothing on screen that is not on disk.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let root = project("collections_write_fails");
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).expect("config");
        regia_test_utils::write_atomic(
            &root.join(SHARED_COLLECTIONS_PATH),
            b"(format_version: 1, collections: [(name: \"Team\")])",
        )
        .expect("write");
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let writable = std::fs::write(config_dir.join("probe"), b"x").is_ok();
        let mut world = world_for(&root, None);
        let result = collection_create(&mut world, &json!({ "name": "Mine", "scope": "shared" }));
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        if writable {
            // Running as root: the directory cannot be made read-only; nothing to prove.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let err = result.expect_err("the write fails");
        assert_eq!(code(&err), Some("collection_write_failed"), "{err:?}");
        let listed = collection_list(&mut world, &json!({})).expect("list");
        let names: Vec<&str> = listed["collections"]
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(names, ["Team"], "the failed collection is not listed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
