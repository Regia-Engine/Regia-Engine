// SPDX-License-Identifier: MPL-2.0
//! Export assets with their dependencies as a package, and import a package (spec addendum § 8.9,
//! ADR-7788, `book/agents/asset-packages.md`).
//!
//! **Export with dependencies…** writes the picked assets and every asset they reach into a folder,
//! at the same project-relative paths and with their `.meta` sidecars (so asset ids stay), plus the
//! manifest `.regia_assetpkg.ron` v1 ([`regia_asset_pipeline::asset_package`]). Another Regia project
//! imports the folder with **Import package…**: every file lands at its listed path. An import that
//! would overwrite a path or reuse an asset id the project already has is refused as a whole, with
//! the list of clashes; nothing is copied.
//!
//! The dependency closure ([`dependency_closure`]) is a breadth-first walk (Cormen et al.,
//! *Introduction to Algorithms*, "Breadth-first search") over the references the project's text
//! assets make: a RON asset (level, prefab, material, ...) references another asset by naming its
//! path (with or without `assets/`) or its asset id from the `.meta`; a `.gltf` references its
//! external buffers and images by relative `uri`. Binary assets reference nothing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::asset_package::{
    ASSET_PACKAGE_FORMAT_VERSION, ASSET_PACKAGE_MANIFEST, AssetPackageEntry, AssetPackageManifest,
    package_hash,
};
use serde_json::{Value, json};

use crate::engine::ecs::prelude::*;
use crate::viewport_agent::Handler;

/// Largest file a text reference scan reads (bytes); larger files are treated as binary.
const MAX_SCAN_BYTES: u64 = 8 << 20;

/// A package failure with its protocol code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageError {
    /// Lowercase code (`pkg_*`), listed in `book/agents/asset-packages.md`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl PackageError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn rpc(&self) -> RpcError {
        let mut error = RpcError::invalid_params(self.message.clone());
        if let Some(data) = error.data.as_mut() {
            data["pkg_error"] = json!(self.code);
        }
        error
    }
}

/// Every asset under `<root>/assets` (project-relative, `/`-separated, sorted), `.meta` sidecars and
/// dot files excluded.
#[must_use]
pub fn project_assets(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && !name.ends_with(".meta")
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(crate::content::slash_path(rel));
            }
        }
    }
    out.sort();
    out
}

/// The asset id in the `.meta` sidecar of `root/rel`, if it has one.
#[must_use]
pub fn asset_id(root: &Path, rel: &str) -> Option<String> {
    let bytes = std::fs::read(regia_asset::meta_path_for(&root.join(rel))).ok()?;
    regia_asset::MetaSidecar::from_ron(&bytes)
        .ok()
        .map(|meta| meta.id.to_string())
}

/// Whether `text` names `needle` as a whole path (not as part of a longer one).
fn names_path(text: &str, needle: &str) -> bool {
    let is_path_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\');
    !needle.is_empty()
        && text.match_indices(needle).any(|(at, _)| {
            let before = text[..at].strip_suffix("assets/").unwrap_or(&text[..at]);
            let left = before.chars().next_back().is_none_or(|c| !is_path_char(c));
            let right = text[at + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !is_path_char(c));
            left && right
        })
}

/// The project's reference index: every asset, its id, and the direct dependencies of each.
#[derive(Debug, Clone, Default)]
pub struct ReferenceIndex {
    /// Asset → its id.
    pub ids: BTreeMap<String, String>,
    /// Asset → the assets it references directly.
    pub deps: BTreeMap<String, BTreeSet<String>>,
}

impl ReferenceIndex {
    /// Builds the index of the project at `root`.
    #[must_use]
    pub fn build(root: &Path) -> Self {
        let _span = tracing::info_span!(target: "regia_editor", "asset_package_index").entered();
        let assets = project_assets(root);
        let ids: BTreeMap<String, String> = assets
            .iter()
            .filter_map(|rel| asset_id(root, rel).map(|id| (rel.clone(), id)))
            .collect();
        let mut deps = BTreeMap::new();
        for rel in &assets {
            let full = root.join(rel);
            let lower = rel.to_ascii_lowercase();
            let mut found = BTreeSet::new();
            let small = std::fs::metadata(&full).is_ok_and(|m| m.len() <= MAX_SCAN_BYTES);
            if small && lower.ends_with(".ron") {
                if let Ok(text) = std::fs::read_to_string(&full) {
                    for other in assets.iter().filter(|other| *other != rel) {
                        let needle = other.strip_prefix("assets/").unwrap_or(other);
                        let by_id = ids.get(other).is_some_and(|id| text.contains(id.as_str()));
                        if by_id || names_path(&text, needle) {
                            found.insert(other.clone());
                        }
                    }
                }
            } else if small && lower.ends_with(".gltf") {
                found.extend(gltf_uris(root, rel));
            }
            deps.insert(rel.clone(), found);
        }
        Self { ids, deps }
    }
}

/// The project assets a `.gltf` names by relative `uri` (buffers and images; `data:` URIs skipped).
fn gltf_uris(root: &Path, rel: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
        return out;
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    let base = Path::new(rel).parent().unwrap_or(Path::new(""));
    for list in ["buffers", "images"] {
        for item in json[list].as_array().into_iter().flatten() {
            let Some(uri) = item["uri"].as_str() else {
                continue;
            };
            if uri.starts_with("data:") || uri.contains("://") {
                continue;
            }
            let joined = crate::content::slash_path(&base.join(uri));
            // Resolve `..` inside the project only.
            let mut parts: Vec<&str> = Vec::new();
            for part in joined.split('/') {
                match part {
                    ".." => {
                        parts.pop();
                    }
                    "." | "" => {}
                    other => parts.push(other),
                }
            }
            let resolved = parts.join("/");
            if resolved.starts_with("assets/") && root.join(&resolved).is_file() {
                out.insert(resolved);
            }
        }
    }
    out
}

/// The assets `roots` reach through references (the roots included), sorted.
#[must_use]
pub fn dependency_closure(index: &ReferenceIndex, roots: &[String]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();
    while let Some(rel) = queue.pop_front() {
        if !seen.insert(rel.clone()) {
            continue;
        }
        for dep in index.deps.get(&rel).into_iter().flatten() {
            if !seen.contains(dep) {
                queue.push_back(dep.clone());
            }
        }
    }
    seen.into_iter().collect()
}

/// What an export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// The package folder.
    pub dest: PathBuf,
    /// The manifest written.
    pub manifest: AssetPackageManifest,
}

fn checked_root_asset(root: &Path, rel: &str) -> Result<String, PackageError> {
    let rel = rel.trim().replace('\\', "/");
    if !regia_asset_pipeline::asset_package::is_package_path(&rel) {
        return Err(PackageError::new(
            "pkg_bad_path",
            format!("{rel:?} is not a path under assets/"),
        ));
    }
    if !root.join(&rel).is_file() {
        return Err(PackageError::new("pkg_missing", format!("{rel} does not exist")));
    }
    Ok(rel)
}

fn copy_file(from: &Path, to: &Path) -> Result<(), PackageError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PackageError::new("pkg_io", format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::copy(from, to).map(|_| ()).map_err(|e| {
        PackageError::new(
            "pkg_io",
            format!("cannot copy {} to {}: {e}", from.display(), to.display()),
        )
    })
}

/// Writes `roots` and their dependency closure, with their `.meta` sidecars and the manifest, into
/// the folder `dest` (created; it must be missing or empty).
///
/// # Errors
///
/// `pkg_bad_path`, `pkg_missing`, `pkg_dest_not_empty` or `pkg_io`.
pub fn export_package(root: &Path, roots: &[String], dest: &Path) -> Result<ExportReport, PackageError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_package_export").entered();
    if roots.is_empty() {
        return Err(PackageError::new(
            "pkg_bad_params",
            "pick at least one asset to export",
        ));
    }
    let roots = roots
        .iter()
        .map(|rel| checked_root_asset(root, rel))
        .collect::<Result<Vec<_>, _>>()?;
    if std::fs::read_dir(dest).is_ok_and(|mut entries| entries.next().is_some()) {
        return Err(PackageError::new(
            "pkg_dest_not_empty",
            format!(
                "{} is not empty: export into a new or empty folder",
                dest.display()
            ),
        ));
    }
    let dest_canonical = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    if let Ok(project) = root.canonicalize()
        && dest_canonical.starts_with(&project)
    {
        return Err(PackageError::new(
            "pkg_bad_dest",
            "export into a folder outside the project (the package would be scanned as assets)",
        ));
    }
    let index = ReferenceIndex::build(root);
    let closure = dependency_closure(&index, &roots);
    let mut assets = Vec::new();
    for rel in &closure {
        let from = root.join(rel);
        let bytes = std::fs::read(&from)
            .map_err(|e| PackageError::new("pkg_io", format!("cannot read {rel}: {e}")))?;
        copy_file(&from, &dest.join(rel))?;
        let meta = regia_asset::meta_path_for(&from);
        if meta.is_file() {
            copy_file(&meta, &regia_asset::meta_path_for(&dest.join(rel)))?;
        }
        assets.push(AssetPackageEntry {
            path: rel.clone(),
            id: index.ids.get(rel).cloned(),
            bytes: bytes.len() as u64,
            hash: package_hash(&bytes),
        });
    }
    let manifest = AssetPackageManifest {
        format_version: ASSET_PACKAGE_FORMAT_VERSION,
        roots,
        assets,
        created_utc: Some(crate::content::rfc3339_utc_now()),
    };
    let text = manifest
        .to_ron()
        .map_err(|e| PackageError::new("pkg_io", e.to_string()))?;
    crate::atomic_write::write_atomic(&dest.join(ASSET_PACKAGE_MANIFEST), text.as_bytes())
        .map_err(|message| PackageError::new("pkg_io", message))?;
    tracing::info!(target: "regia_editor", files = manifest.assets.len(), dest = %dest.display(), "asset package exported");
    Ok(ExportReport {
        dest: dest.to_path_buf(),
        manifest,
    })
}

/// One reason an import was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    /// The packaged asset.
    pub path: String,
    /// `path` (the project already has a file there) or `id` (a project asset already uses the id).
    pub kind: &'static str,
    /// For an id clash, the project asset that has the id.
    pub existing: Option<String>,
}

/// What an import did (or why it refused).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportPackageReport {
    /// Paths copied into the project.
    pub imported: Vec<String>,
    /// Clashes that refused the import (nothing was copied when this is not empty).
    pub clashes: Vec<Clash>,
    /// Packaged files whose bytes do not match the manifest (refused as well).
    pub damaged: Vec<String>,
    /// Assets cooked after the copy.
    pub cooked: usize,
}

impl ImportPackageReport {
    /// One status line.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.clashes.is_empty() || !self.damaged.is_empty() {
            let first = self
                .clashes
                .first()
                .map(|clash| format!("{} ({} clash)", clash.path, clash.kind))
                .or_else(|| self.damaged.first().map(|path| format!("{path} (damaged)")))
                .unwrap_or_default();
            return format!(
                "package refused: {} clash(es), {} damaged file(s), first {first}; nothing was imported",
                self.clashes.len(),
                self.damaged.len()
            );
        }
        format!(
            "imported {} packaged asset(s), cooked {}",
            self.imported.len(),
            self.cooked
        )
    }

    /// The agent reply.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "imported": self.imported,
            "refused": !self.clashes.is_empty() || !self.damaged.is_empty(),
            "clashes": self
                .clashes
                .iter()
                .map(|clash| json!({ "path": clash.path, "kind": clash.kind, "existing": clash.existing }))
                .collect::<Vec<_>>(),
            "damaged": self.damaged,
            "cooked": self.cooked,
        })
    }
}

/// Reads a package folder's manifest.
///
/// # Errors
///
/// `pkg_no_manifest` or the manifest's own code (`pkg_bad_manifest`, `pkg_version`, `pkg_bad_path`,
/// `pkg_too_large`).
pub fn read_manifest(package: &Path) -> Result<AssetPackageManifest, PackageError> {
    let file = package.join(ASSET_PACKAGE_MANIFEST);
    let text = std::fs::read_to_string(&file).map_err(|e| {
        PackageError::new(
            "pkg_no_manifest",
            format!(
                "{} has no readable {ASSET_PACKAGE_MANIFEST}: {e}",
                package.display()
            ),
        )
    })?;
    AssetPackageManifest::from_ron(&text).map_err(|e| PackageError::new(e.code(), e.to_string()))
}

/// Imports the package folder `package` into the project at `root`: refuses as a whole on any path
/// or id clash (or a damaged file), else copies every file with its `.meta` and cooks.
///
/// # Errors
///
/// The manifest cannot be read, or a copy fails (`pkg_io`; files copied so far are removed).
pub fn import_package(root: &Path, package: &Path, cook: bool) -> Result<ImportPackageReport, PackageError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_package_import").entered();
    let manifest = read_manifest(package)?;
    let mut report = ImportPackageReport::default();
    let existing_ids: BTreeMap<String, String> = project_assets(root)
        .into_iter()
        .filter_map(|rel| asset_id(root, &rel).map(|id| (id, rel)))
        .collect();
    for entry in &manifest.assets {
        let source = package.join(&entry.path);
        match std::fs::read(&source) {
            Ok(bytes) if bytes.len() as u64 == entry.bytes && package_hash(&bytes) == entry.hash => {}
            _ => report.damaged.push(entry.path.clone()),
        }
        if root.join(&entry.path).exists() {
            report.clashes.push(Clash {
                path: entry.path.clone(),
                kind: "path",
                existing: None,
            });
        }
        if let Some(id) = &entry.id
            && let Some(owner) = existing_ids.get(id)
        {
            report.clashes.push(Clash {
                path: entry.path.clone(),
                kind: "id",
                existing: Some(owner.clone()),
            });
        }
    }
    if !report.clashes.is_empty() || !report.damaged.is_empty() {
        tracing::warn!(target: "regia_editor", "{}", report.summary());
        return Ok(report);
    }
    let mut copied: Vec<PathBuf> = Vec::new();
    for entry in &manifest.assets {
        let from = package.join(&entry.path);
        let to = root.join(&entry.path);
        let mut step = || -> Result<(), PackageError> {
            copy_file(&from, &to)?;
            copied.push(to.clone());
            let meta = regia_asset::meta_path_for(&from);
            if meta.is_file() {
                let meta_to = regia_asset::meta_path_for(&to);
                copy_file(&meta, &meta_to)?;
                copied.push(meta_to);
            }
            Ok(())
        };
        if let Err(err) = step() {
            for path in &copied {
                let _ = std::fs::remove_file(path);
            }
            return Err(err);
        }
        report.imported.push(entry.path.clone());
    }
    if cook {
        match regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(
            root.to_path_buf(),
        )) {
            Ok(cooked) => {
                report.cooked = cooked.cooked;
                for (path, message) in cooked.errors {
                    tracing::warn!(target: "regia_editor", "cook {path}: {message}");
                }
            }
            Err(e) => tracing::warn!(target: "regia_editor", "cook after the package import: {e}"),
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// UI: Export with dependencies… (asset menu) and Import package… (browser toolbar)
// ---------------------------------------------------------------------------------------------

/// A package action asked for by a menu, run when the browser releases the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageRequest {
    /// Export these assets (with dependencies) into the folder.
    Export(Vec<String>, PathBuf),
    /// Import the package folder.
    Import(PathBuf),
}

/// The pending package request.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingPackageRequest(pub Option<PackageRequest>);

/// The egui frame whose package or thumbnail menu click already acted.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuClickFrame(pub Option<u64>);

/// Whether a package or thumbnail click in egui frame `frame` may act (it then stamps the frame):
/// egui's second UI pass of a frame sees the same click, and must not open the folder dialog again
/// or queue a second import of the same package (review sprint-w12-rev-g5 #10).
pub fn first_click_in_frame(world: &mut World, frame: u64) -> bool {
    let mut stamp = world.get_resource_or_insert_with(MenuClickFrame::default);
    if stamp.0 == Some(frame) {
        return false;
    }
    stamp.0 = Some(frame);
    true
}

/// The asset menu's "Export with dependencies…" item (an OS folder dialog picks the destination).
pub fn export_menu_item(ui: &mut egui::Ui, world: &mut World, rel: &str) {
    if ui
        .button("Export with dependencies…")
        .on_hover_text("Write this asset, everything it uses and their .meta files into a package folder")
        .clicked()
        && first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
    {
        ui.close();
        if let Some(dest) = crate::file_dialog::FileDialog::new()
            .set_title("Export package into")
            .pick_folder()
        {
            world.insert_resource(PendingPackageRequest(Some(PackageRequest::Export(
                vec![rel.to_string()],
                dest,
            ))));
        }
    }
}

/// Queues one export of every path in `paths` into `dest` (one package, one manifest whose roots
/// are the paths in order, duplicates dropped): what the selection's **Export selection with
/// dependencies…** does once the folder is picked (P-145, ADR-8147). [`run_pending`] runs it.
pub fn request_export(world: &mut World, paths: &[String], dest: PathBuf) {
    let mut roots: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        if !roots.contains(path) {
            roots.push(path.clone());
        }
    }
    world.insert_resource(PendingPackageRequest(Some(PackageRequest::Export(roots, dest))));
}

/// The Content Browser selection's "Export selection with dependencies…" (menu item and selection
/// bar button): an OS folder dialog picks the destination, then [`request_export`] queues one
/// package of the whole selection. Agent route: `editor.export_package { paths, dest }`.
pub fn export_selection_item(ui: &mut egui::Ui, world: &mut World, paths: &[String]) {
    let action = &crate::agent_parity::CONTENT_BULK_EXPORT;
    if crate::agent_parity::action_menu_item(ui, action)
        .on_hover_text(
            "Write the selected assets, everything they use and their .meta files into one package folder",
        )
        .clicked()
        && first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
    {
        ui.close();
        if let Some(dest) = crate::file_dialog::FileDialog::new()
            .set_title("Export package into")
            .pick_folder()
        {
            request_export(world, paths, dest);
        }
    }
}

/// The browser toolbar's "Import package…" button (an OS folder dialog picks the package).
pub fn import_button(ui: &mut egui::Ui, world: &mut World) {
    if ui
        .button("Import package…")
        .on_hover_text("Import a package folder exported from another Regia project")
        .clicked()
        && first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
        && let Some(package) = crate::file_dialog::FileDialog::new()
            .set_title("Import package")
            .pick_folder()
    {
        world.insert_resource(PendingPackageRequest(Some(PackageRequest::Import(package))));
    }
}

/// Runs a pending package request as a background `package:<n>` job (listed in Asset Jobs); returns a
/// status line.
pub fn run_pending(world: &mut World) -> Option<String> {
    let request = world.get_resource_mut::<PendingPackageRequest>()?.0.take()?;
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())?;
    let label = match &request {
        PackageRequest::Export(roots, dest) => {
            format!("export {} asset(s) to {}", roots.len(), dest.display())
        }
        PackageRequest::Import(package) => format!("import package {}", package.display()),
    };
    let (id, handle) = world
        .get_resource_or_insert_with::<regia_agent::jobs::JobRegistry>(Default::default)
        .start("package", label, false);
    handle.progress(None, "working");
    // The worker owns `handle`; this copy ends the job when the worker never starts.
    let unstarted = handle.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-asset-package".into())
        .spawn(move || {
            let outcome = match request {
                PackageRequest::Export(roots, dest) => export_package(&root, &roots, &dest)
                    .map(|report| json!({ "dest": report.dest.display().to_string(), "assets": report.manifest.assets.len() })),
                PackageRequest::Import(package) => import_package(&root, &package, true).and_then(|report| {
                    if report.clashes.is_empty() && report.damaged.is_empty() {
                        Ok(report.to_json())
                    } else {
                        Err(PackageError::new("pkg_clash", report.summary()))
                    }
                }),
            };
            match outcome {
                Ok(value) => handle.finish(value),
                Err(err) => {
                    tracing::error!(target: "regia_editor", "asset package: {}", err.message);
                    handle.fail(err.code, err.message);
                }
            }
        });
    crate::content_index::invalidate(world);
    Some(match spawned {
        Ok(_) => format!("{id} started (View → Asset Jobs shows it)"),
        Err(e) => spawn_failed(&unstarted, &e),
    })
}

/// A package job whose worker thread did not start: the job fails (`pkg_io`) so Asset Jobs does
/// not list it as running for the rest of the session (review sprint-w12-rev-g5 #11). Returns the
/// status line.
fn spawn_failed(handle: &regia_agent::jobs::JobHandle, err: &std::io::Error) -> String {
    let message = format!("cannot start the package job: {err}");
    tracing::error!(target: "regia_editor", "asset package: {message}");
    handle.fail("pkg_io", message.clone());
    message
}

// ---------------------------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------------------------

fn string_list(params: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    params
        .get(key)
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .and_then(|list| {
            list.iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| {
            PackageError::new(
                "pkg_bad_params",
                format!("`{key}` must be a non-empty array of strings"),
            )
            .rpc()
        })
}

fn path_param(world: &World, params: &Value, key: &str) -> Result<PathBuf, RpcError> {
    let raw = params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            PackageError::new("pkg_bad_params", format!("`{key}` (a folder path) is required")).rpc()
        })?;
    crate::agent_ext::allowed_request_path(world, key, raw)
}

/// `editor.export_package { paths, dest }`.
fn export_package_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::agent_ext::project_root(world)?;
    let paths = string_list(params, "paths")?;
    let dest = path_param(world, params, "dest")?;
    let report = export_package(&root, &paths, &dest).map_err(|e| e.rpc())?;
    Ok(json!({
        "dest": report.dest.display().to_string(),
        "manifest": ASSET_PACKAGE_MANIFEST,
        "roots": report.manifest.roots,
        "assets": report
            .manifest
            .assets
            .iter()
            .map(|entry| json!({ "path": entry.path, "id": entry.id, "bytes": entry.bytes }))
            .collect::<Vec<_>>(),
    }))
}

/// `editor.import_package { package, cook? }`.
fn import_package_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::agent_ext::project_root(world)?;
    let package = path_param(world, params, "package")?;
    let cook = params.get("cook").and_then(Value::as_bool).unwrap_or(true);
    let report = import_package(&root, &package, cook).map_err(|e| e.rpc())?;
    crate::content_index::invalidate(world);
    Ok(report.to_json())
}

/// The asset-package agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.export_package",
                tool: "export_package",
                description: "Export with dependencies (spec addendum § 8.9): write `paths` and every asset they \
                    reference (by path or asset id, and a .gltf's buffers and images) with their .meta sidecars into \
                    the folder `dest`, at the same project-relative paths, plus the manifest .regia_assetpkg.ron v1. \
                    `dest` must be missing or empty and outside the project.",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: true,
                        description: "Project-relative assets under assets/ to export.",
                    },
                    ParamSchema {
                        name: "dest",
                        kind: "string",
                        required: true,
                        description: "The package folder to create (an allowed request path).",
                    },
                ],
                result: "{ dest, manifest, roots, assets: [{ path, id, bytes }] }",
                mutating: true,
                permission: Permission::Assets,
            },
            export_package_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_package",
                tool: "import_package",
                description: "Import package (spec addendum § 8.9): copy a package folder's assets and .meta sidecars \
                    into the project at their listed paths and cook them. Refused as a whole, with the clash list, \
                    when a path exists or an asset id is already used in the project, or a file does not match the \
                    manifest; nothing is copied then.",
                params: &[
                    ParamSchema {
                        name: "package",
                        kind: "string",
                        required: true,
                        description: "The package folder (holds .regia_assetpkg.ron).",
                    },
                    ParamSchema {
                        name: "cook",
                        kind: "boolean",
                        required: false,
                        description: "Cook after the copy (default true).",
                    },
                ],
                result: "{ imported: [paths], refused, clashes: [{ path, kind: path|id, existing }], damaged: [paths], \
                    cooked }",
                mutating: true,
                permission: Permission::Assets,
            },
            import_package_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod first_import_tests;
