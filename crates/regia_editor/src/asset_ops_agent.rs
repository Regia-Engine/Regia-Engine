// SPDX-License-Identifier: MPL-2.0
//! Asset file operations as agent calls (`specs/23` § 3.3, ADR-5706):
//! `editor.edit_asset`, `editor.reimport`, `editor.move_asset`, `editor.delete_asset` and
//! `editor.set_import_settings` (`book/agents/asset-ops.md`).
//!
//! These mirror the Content Browser's file operations (import/create aside): reimport re-cooks
//! one source with its cook cache ignored (the `.meta` and its id stay), move renames a file with
//! its `.meta` sidecar after the path-referencer check, delete goes through the referencer check
//! and needs the owner's confirmation (`confirm: true`), and `edit_asset` patches a text asset
//! through its typed model and writes it back canonically, as one undo entry. Paths (`move_asset`'s destination
//! too) are project-relative under `assets/`; everything that escapes is `path_outside_project`
//! (ADR-1816) and a goldens directory is `golden_path_denied` (ADR-2821).

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::viewport_agent::Handler;

/// The five asset operations.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.edit_asset",
                tool: "edit_asset",
                description: "Patch a text asset (`.material.ron`, `.prefab.ron`, `.level.ron`, \
                    `.variant.ron`) and save it canonically, as one undo entry. The patch is a \
                    list of {path, value} entries over the asset's JSON view: dot-separated \
                    fields, numeric segments index arrays; component values in scene files are \
                    RON-text strings. Undo restores the previous file text.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path under assets/.",
                    },
                    ParamSchema {
                        name: "patch",
                        kind: "array",
                        required: true,
                        description: "[{ path: \"base.metallic\", value: 0.5 }, ...].",
                    },
                ],
                result: "{ path, patched, bytes, undoable }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_edit_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.reimport",
                tool: "reimport",
                description: "Re-cook one source (and what depends on it) with its cook cache \
                    ignored — the Content Browser's \"Reimport (forget cache)\". The `.meta` is \
                    kept: the asset id, import settings and provenance survive.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path under assets/.",
                }],
                result: "{ path, cooked, skipped, errors: [{path, error}] }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_reimport as Handler,
        ),
        (
            MethodSchema {
                name: "editor.move_asset",
                tool: "move_asset",
                description: "Move or rename an asset (or a folder) under assets/, its `.meta` \
                    sidecar (and asset id) with it, exactly as the Content Browser's Rename / \
                    Move does (ADR-7822): every string that names the old path is rewritten to \
                    the new one (listed in fixed_referencers), `asset:<guid>` references survive \
                    unchanged (listed in id_referencers), and the move is one undo step. Refuses \
                    when the destination exists.",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "Project-relative source path under assets/.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: true,
                        description: "Project-relative destination under assets/.",
                    },
                ],
                result: "{ from, to, fixed_referencers, id_referencers, undoable }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_move_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.delete_asset",
                tool: "delete_asset",
                description: "Delete an asset (and its `.meta`) from the project. Always needs \
                    the owner's confirmation: the first call answers `needs_confirmation` (or \
                    `asset_delete_referenced` with data.referencers when other assets mention \
                    the file); repeat with `confirm: true` to delete. With `replace_with` (an \
                    asset of the same kind) every reference is first pointed at that asset \
                    (paths rewritten, and the asset id when both have a .meta), as one undo step \
                    that also restores the file (ADR-7822).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path under assets/.",
                    },
                    ParamSchema {
                        name: "confirm",
                        kind: "boolean",
                        required: false,
                        description: "true deletes; absent or false answers the confirmation refusal.",
                    },
                    ParamSchema {
                        name: "replace_with",
                        kind: "string",
                        required: false,
                        description: "An asset of the same kind every reference is pointed at before the delete.",
                    },
                ],
                result: "{ path, deleted, referencers, replaced_with, rewritten }",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_delete_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_import_settings",
                tool: "set_import_settings",
                description: "Set one source's importer settings (a RON settings object like \
                    `(max_size: Some(2048))`, or `(lossless_clips: true)` to cook a glTF, FBX, USD \
                    or BVH source's animation clips bit-exact instead of compressed) in its \
                    `.meta`, replacing folder rules for this source, then re-cook it. Absent or null `settings` clears them. The asset \
                    must have been cooked once (a `.meta` must exist). The write and the cook run \
                    as job import_settings:<n> (agent.job_status; editor.cancel_job cancels it and \
                    puts the old .meta and cooked output back): with async: true the method \
                    answers { job } at once, otherwise it waits for the job and answers its result \
                    plus job. Without async it is refused with `tools_busy` while another holder \
                    has the project cook lock (an async job waits for it); \
                    `import_settings_apply_running` while another write runs.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative source path under assets/.",
                    },
                    ParamSchema {
                        name: "settings",
                        kind: "string",
                        required: false,
                        description: "RON settings object; absent or null clears the override.",
                    },
                    ParamSchema {
                        name: "async",
                        kind: "boolean",
                        required: false,
                        description: "true answers { job } at once (poll agent.job_status); absent or false waits for the job.",
                    },
                ],
                result: "{ path, settings, cooked, skipped, errors: [{path, error}], job } (async: { job })",
                mutating: true,
                permission: Permission::Assets,
            },
            editor_set_import_settings as Handler,
        ),
    ]
}

/// Counts the import-settings writes that recooked (`editor.set_import_settings` and the Import
/// settings panel's Apply): the asset viewer drops its gathered overlay geometry when it moves, so
/// a collision overlay never shows the hulls of the settings before the Apply (rev-claude-57 asset
/// finding 2, ADR-7495).
pub(crate) static IMPORT_SETTINGS_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An `invalid_params` (F0002) refusal carrying a stable machine-readable `data.reason`, like
/// `functest.bless`'s `needs_confirmation`.
pub(crate) fn reasoned(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// The project root from the editor's resources.
fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            )
        })
}

/// A request path naming a file under the project's `assets/` (an optional `assets/` prefix is
/// dropped): absolute, drive, UNC, `..` and symlinked paths are `path_outside_project` (ADR-1816).
pub(crate) fn assets_request_path(root: &Path, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    let rel = path.replace('\\', "/");
    let rel = rel.strip_prefix("assets/").unwrap_or(&rel);
    // The caller's own spelling is held to the rule first: behind the `assets/` prefix an
    // absolute (`/x`), root-relative (`\x`) or UNC (`\\server\share`) form would only gain an
    // empty segment, which the rule drops, and resolve inside `assets/` instead of being refused
    // (review rev-kimi-8). Every lexical refusal is also one of `project_path_in`.
    if let Err(lexical) = regia_core::project_path::check_relative(rel) {
        return Err(match regia_agent::req_path::project_path_in(root, key, rel) {
            Err(refusal) => refusal,
            Ok(_) => RpcError::invalid_params(format!("`{key}` {path:?}: {lexical}")),
        });
    }
    regia_agent::req_path::project_path_in(root, key, &format!("assets/{rel}"))
}

/// A destination path validated like `content::create_asset`: no empty or dot segments. Run
/// after [`assets_request_path`] has confined it, so `..`, absolute, drive, UNC, symlinked and
/// goldens forms are already `path_outside_project` / `golden_path_denied`.
fn destination_rel(to: &str) -> Result<String, RpcError> {
    let rel = to.replace('\\', "/");
    let rel = rel.strip_prefix("assets/").unwrap_or(&rel).to_string();
    if rel.is_empty()
        || rel
            .split('/')
            .any(|segment| segment.is_empty() || segment.starts_with('.'))
    {
        return Err(reasoned(
            "asset_bad_destination",
            format!("invalid destination {to:?}: it must name a file under assets/"),
        ));
    }
    Ok(rel)
}

/// The referencers of `rel` (project-relative) other than the file itself.
fn referencers_of(root: &Path, rel: &str) -> Vec<String> {
    crate::content::find_referencers(root, rel)
        .into_iter()
        .filter(|found| found != rel)
        .collect()
}

/// The files that mention `rel`'s path (not its asset id), other than the file itself.
fn path_referencers_of(root: &Path, rel: &str) -> Vec<String> {
    crate::content::find_path_referencers(root, rel)
        .into_iter()
        .filter(|found| found != rel)
        .collect()
}

/// Re-cooks the source `full` and what depends on it with the cook cache ignored, keeping its
/// `.meta` — the asset id saved instances reference, the import settings and the provenance
/// record (review rev-kimi-8 finding 3). A kind the cook reads directly (a level, a script)
/// re-cooks nothing. The Content Browser's "Reimport (forget cache)" uses this too.
///
/// # Errors
/// The cook's own failure (the database or the cache cannot be opened or written); a source that
/// fails to import is an entry of the report's `errors`.
pub(crate) fn recook_source(
    root: &Path,
    full: &Path,
) -> Result<regia_asset_pipeline::CookReport, regia_asset_pipeline::CookError> {
    let options = regia_asset_pipeline::CookOptions {
        incremental: false,
        ..regia_asset_pipeline::CookOptions::for_host(root.to_path_buf())
    };
    let targets = regia_asset_pipeline::affected_sources(
        &root.join("assets"),
        &options.platform,
        &[full.to_path_buf()],
    );
    let mut report = regia_asset_pipeline::CookReport::default();
    if targets.is_empty() {
        return Ok(report);
    }
    for cooked in regia_asset_pipeline::cook_sources(&options, &targets)? {
        match cooked.error {
            Some(error) => report.errors.push((cooked.rel_path, error)),
            None if cooked.skipped => report.skipped += 1,
            None => report.cooked += 1,
        }
        report.warnings.extend(cooked.warnings);
    }
    Ok(report)
}

/// Runs the project's incremental cook and maps a failure to an internal error.
fn cook(root: &Path) -> Result<regia_asset_pipeline::CookReport, RpcError> {
    regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.to_path_buf()))
        .map_err(|e| RpcError::internal(format!("cook failed: {e}")))
}

/// Cook errors as the result's `errors` array.
fn cook_errors(report: &regia_asset_pipeline::CookReport) -> Vec<Value> {
    report
        .errors
        .iter()
        .map(|(path, message)| json!({ "path": path, "error": message }))
        .collect()
}

// ---------------------------------------------------------------------------
// edit_asset
// ---------------------------------------------------------------------------

/// The text asset kinds `edit_asset` patches, each with its typed reader and canonical writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    /// A `.material.ron` (`regia_asset::MaterialAssetRon`).
    Material,
    /// A `.prefab.ron` or `.level.ron` document (`regia_scene::format::SceneFile`).
    Scene,
    /// A `.variant.ron` (`regia_scene::prefab_variant::PrefabVariantFile`).
    Variant,
}

impl EditKind {
    fn of_path(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".material.ron") {
            Some(Self::Material)
        } else if lower.ends_with(".prefab.ron") || lower.ends_with(".level.ron") {
            Some(Self::Scene)
        } else if lower.ends_with(regia_scene::prefab_variant::PREFAB_VARIANT_SUFFIX) {
            Some(Self::Variant)
        } else {
            None
        }
    }

    /// Parses the document with the kind's typed parser (a malformed file is refused before
    /// anything is written) and returns its JSON view.
    fn parse_json(self, text: &str) -> Result<Value, RpcError> {
        fn json_view<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
            serde_json::to_value(value)
                .map_err(|e| RpcError::internal(format!("JSON view of the asset: {e}")))
        }
        match self {
            Self::Material => {
                let material = regia_asset::MaterialAssetRon::from_ron(text.as_bytes())
                    .map_err(|e| RpcError::invalid_params(format!("{e}")))?;
                json_view(&material)
            }
            Self::Scene => {
                let file = regia_scene::prelude::SceneFile::from_ron_str(text)
                    .map_err(|e| RpcError::invalid_params(format!("{e}")))?;
                json_view(&file)
            }
            Self::Variant => {
                let variant = regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(text)
                    .map_err(|e| RpcError::invalid_params(format!("{e}")))?;
                json_view(&variant)
            }
        }
    }

    /// Deserializes the patched JSON view back into the typed document and writes it canonically.
    fn write_canonical(self, doc: &Value) -> Result<String, RpcError> {
        fn bad_value(context: &str) -> impl Fn(serde_json::Error) -> RpcError + '_ {
            move |e| {
                reasoned(
                    "asset_edit_bad_value",
                    format!("`value` does not fit {context}: {e}"),
                )
            }
        }
        match self {
            Self::Material => {
                let material: regia_asset::MaterialAssetRon =
                    serde_json::from_value(doc.clone()).map_err(bad_value("the material"))?;
                material
                    .to_ron_string()
                    .map_err(|e| RpcError::internal(format!("material serialize: {e}")))
            }
            Self::Scene => {
                let file: regia_scene::prelude::SceneFile =
                    serde_json::from_value(doc.clone()).map_err(bad_value("the scene"))?;
                file.to_ron_string()
                    .map_err(|e| RpcError::internal(format!("scene serialize: {e}")))
            }
            Self::Variant => {
                let variant: regia_scene::prefab_variant::PrefabVariantFile =
                    serde_json::from_value(doc.clone()).map_err(bad_value("the variant"))?;
                variant
                    .to_ron_string()
                    .map_err(|e| RpcError::internal(format!("variant serialize: {e}")))
            }
        }
    }
}

/// Applies one `{path, value}` entry to the JSON view: dot-separated fields, numeric segments
/// index arrays. Only existing fields are addressable; a missing segment is
/// `asset_edit_no_such_path` naming it.
fn apply_patch(doc: &mut Value, path: &str, value: &Value) -> Result<(), RpcError> {
    let mut segments = path.split('.');
    let Some(first) = segments.next().filter(|s| !s.is_empty()) else {
        return Err(reasoned(
            "asset_edit_bad_patch",
            format!("patch path {path:?} is empty"),
        ));
    };
    let mut current = doc.get_mut(first).ok_or_else(|| no_such_path(path, first))?;
    for segment in segments {
        if segment.is_empty() {
            return Err(reasoned(
                "asset_edit_bad_patch",
                format!("patch path {path:?} has an empty segment"),
            ));
        }
        current = match current {
            Value::Object(_) => current
                .get_mut(segment)
                .ok_or_else(|| no_such_path(path, segment))?,
            Value::Array(items) => {
                let index: usize = segment.parse().map_err(|_| no_such_path(path, segment))?;
                items.get_mut(index).ok_or_else(|| no_such_path(path, segment))?
            }
            _ => return Err(no_such_path(path, segment)),
        };
    }
    *current = value.clone();
    Ok(())
}

/// Whether `path` is `open` or lies inside it (a level saved as a folder), compared with
/// symlinks resolved when both exist.
fn same_or_inside(open: &Path, path: &Path) -> bool {
    if path.starts_with(open) {
        return true;
    }
    match (open.canonicalize(), path.canonicalize()) {
        (Ok(open), Ok(path)) => path.starts_with(open),
        _ => false,
    }
}

fn no_such_path(path: &str, segment: &str) -> RpcError {
    let mut error = reasoned(
        "asset_edit_no_such_path",
        format!("`{path}` has no field `{segment}`"),
    );
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("path".into(), json!(path));
        data.insert("segment".into(), json!(segment));
    }
    error
}

/// The undo entry of one asset-text write: reverts to `before`, redoes to `after`. File-only:
/// it must not mark a saved level dirty (the undo entry restores the file, not the scene).
#[derive(Debug, Clone)]
pub struct AssetTextEdit {
    /// The asset file the entry edits.
    path: PathBuf,
    /// The file text before the edit.
    before: String,
    /// The canonical text the edit wrote.
    after: String,
    /// What the entry does, for the undo history.
    label: String,
}

impl AssetTextEdit {
    fn write(&self, text: &str) {
        if let Err(e) = crate::atomic_write::write_atomic(&self.path, text.as_bytes()) {
            tracing::warn!(target: "regia_editor", "{}: {e}", self.path.display());
        }
    }
}

impl crate::command::EditorCommand for AssetTextEdit {
    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), vec![self.path.to_string_lossy().into_owned()])
    }

    fn apply(&mut self, _world: &mut World) {
        self.write(&self.after.clone());
    }
    fn revert(&mut self, _world: &mut World) {
        self.write(&self.before.clone());
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        self.label.clone()
    }
}

/// `editor.edit_asset`.
fn editor_edit_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let patch = params
        .get("patch")
        .and_then(Value::as_array)
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params("`patch` (a non-empty array of {path, value}) is required")
        })?;
    let Some(kind) = EditKind::of_path(path) else {
        return Err(reasoned(
            "asset_edit_unsupported_kind",
            format!(
                "edit_asset does not support {path:?}: patch .material.ron, .prefab.ron, \
                 .level.ron or .variant.ron"
            ),
        ));
    };
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(&root).unwrap_or(&full));
    let text = std::fs::read_to_string(&full).map_err(|_| {
        RpcError::not_found(
            format!("{rel} does not exist"),
            "editor.list_assets shows the project's files",
        )
    })?;
    // The level open in the editor: the asset watcher would reload it from the patched file,
    // clearing the undo history (the entry this call returns) or, with unsaved edits, raising the
    // owner's reload prompt (review rev-kimi-8 finding 5). Its scene methods edit it instead.
    let open_level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone());
    if open_level
        .as_deref()
        .is_some_and(|open| same_or_inside(open, &full))
    {
        return Err(reasoned(
            "asset_level_open",
            format!(
                "{rel} is the level open in the editor: edit it with the editor's scene methods \
                 (editor.spawn, editor.add_component, ...) and editor.save_level, or open another \
                 level with editor.open_level first"
            ),
        ));
    }
    // The Material panel keeps unsaved edits: writing the file would be overwritten by its next
    // save (the same refusal editor.edit_material makes, audit v033b).
    let panel = world
        .get_resource::<crate::material_editor::MaterialPanelStatus>()
        .cloned()
        .unwrap_or_default();
    let panel_has_it = panel
        .path
        .as_deref()
        .is_some_and(|open| crate::material_editor::same_material_file(open, &full));
    if panel_has_it && panel.dirty {
        let mut error = RpcError::invalid_params(format!(
            "{rel} has unsaved edits in the Material panel: save or undo them there first"
        ));
        if let Some(data) = error.data.as_mut() {
            data["material_error"] = json!("material_unsaved_in_panel");
        }
        return Err(error);
    }
    let mut doc = kind.parse_json(&text)?;
    for entry in patch {
        let entry_path = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("every patch entry needs a string `path`"))?;
        let value = entry
            .get("value")
            .ok_or_else(|| RpcError::invalid_params("every patch entry needs a `value`"))?;
        apply_patch(&mut doc, entry_path, value)?;
    }
    let new_text = kind.write_canonical(&doc)?;
    crate::atomic_write::write_atomic(&full, new_text.as_bytes())
        .map_err(|e| RpcError::internal(format!("cannot write {rel}: {e}")))?;
    let undoable = crate::undo_agent::push_discrete(
        world,
        Box::new(AssetTextEdit {
            path: full.clone(),
            before: text,
            after: new_text.clone(),
            label: format!("edit_asset {rel} ({} patch(es))", patch.len()),
        }),
    );
    if panel_has_it && let Ok(material) = regia_asset::MaterialAssetRon::from_ron(new_text.as_bytes()) {
        world.insert_resource(crate::material_editor::MaterialPanelReload {
            path: full.clone(),
            material,
        });
    }
    // Provenance on the touched asset (specs/23 § 5), best effort like the import path.
    let provenance = regia_asset::Provenance {
        origin: None,
        tool: regia_asset::ProvenanceTool::Agent,
        generator: None,
        seed: None,
        client: params.get("client").and_then(Value::as_str).map(str::to_string),
        session: None,
        model: None,
        created_utc: Some(crate::content::rfc3339_utc_now()),
        reviewer: regia_asset::ReviewerState::default(),
        extensions: std::collections::BTreeMap::new(),
    };
    if let Err(e) = crate::content::stamp_provenance(&full, &provenance) {
        tracing::warn!(target: "regia_editor", "provenance for {rel}: {e}");
    }
    crate::content_index::invalidate(world);
    Ok(json!({
        "path": rel,
        "patched": patch.len(),
        "bytes": new_text.len(),
        "undoable": undoable,
    }))
}

// ---------------------------------------------------------------------------
// reimport
// ---------------------------------------------------------------------------

/// `editor.reimport`.
fn editor_reimport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(&root).unwrap_or(&full));
    if !full.is_file() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist or is not a file"),
            "editor.list_assets shows the project's files",
        ));
    }
    let report = recook_source(&root, &full).map_err(|e| RpcError::internal(format!("cook failed: {e}")))?;
    crate::content_index::invalidate(world);
    Ok(json!({
        "path": rel,
        "cooked": report.cooked,
        "skipped": report.skipped,
        "errors": cook_errors(&report),
    }))
}

// ---------------------------------------------------------------------------
// move_asset
// ---------------------------------------------------------------------------

/// `editor.move_asset`.
fn editor_move_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = params
        .get("from")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`from` (project-relative, under assets/) is required"))?;
    let to = params
        .get("to")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`to` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    let src = assets_request_path(&root, "from", from)?;
    let from_rel = crate::content::slash_path(src.strip_prefix(&root).unwrap_or(&src));
    if !src.is_file() && !src.is_dir() {
        return Err(RpcError::not_found(
            format!("{from_rel} does not exist"),
            "editor.list_assets shows the project's files",
        ));
    }
    // The destination is a request path like `from` (review rev-kimi-8 findings 1-2): confined to
    // the project (no `..`, absolute, drive, UNC or symlinked escape) and never under goldens.
    let dst = assets_request_path(&root, "to", to)?;
    let dst_rel = destination_rel(to)?;
    if dst.symlink_metadata().is_ok() {
        return Err(reasoned(
            "asset_move_destination_exists",
            format!("assets/{dst_rel} already exists"),
        ));
    }
    // One move behavior for the Content Browser and the agent (ADR-7822): path mentions are
    // rewritten by `regia_asset::project_ops::move_asset` (all-or-nothing, rolled back on a
    // failure) and the move is one undo step. An `asset:<guid>` reference survives unchanged (the
    // `.meta`, and its id, move with the file): those referencers are reported.
    let path_referencers = path_referencers_of(&root, &from_rel);
    let id_referencers: Vec<String> = referencers_of(&root, &from_rel)
        .into_iter()
        .filter(|found| !path_referencers.contains(found))
        .collect();
    let to_rel = format!("assets/{dst_rel}");
    let outcome = regia_asset::project_ops::move_asset(&root, &from_rel, &to_rel).map_err(|e| match e {
        regia_asset::project_ops::MoveError::DestinationExists(path) => {
            reasoned("asset_move_destination_exists", format!("{path} already exists"))
        }
        regia_asset::project_ops::MoveError::InvalidPath(path) => reasoned(
            "asset_bad_destination",
            format!("invalid destination {path:?}: {to} cannot hold {from_rel}"),
        ),
        other => RpcError::internal(format!("cannot move {from_rel}: {other}")),
    })?;
    crate::content::push_move_undo(world, root.clone(), from_rel.clone(), to_rel.clone());
    world
        .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
        .rename(&from_rel, &to_rel);
    crate::content_index::invalidate(world);
    Ok(json!({
        "from": from_rel,
        "to": to_rel,
        "fixed_referencers": outcome.fixed_referencers,
        "id_referencers": id_referencers,
        "undoable": true,
    }))
}

// ---------------------------------------------------------------------------
// delete_asset
// ---------------------------------------------------------------------------

/// `editor.delete_asset` (specs/23 § 3.3: needs confirmation, § 5).
fn editor_delete_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    let full = assets_request_path(&root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(&root).unwrap_or(&full));
    if !full.exists() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist"),
            "editor.list_assets shows the project's files",
        ));
    }
    let referencers = referencers_of(&root, &rel);
    let confirmed = params.get("confirm").and_then(Value::as_bool) == Some(true);
    let replace_with = params
        .get("replace_with")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    if let Some(replacement) = replace_with {
        // The replacement is a request path too (confined, never under goldens).
        let replacement_full = assets_request_path(&root, "replace_with", replacement)?;
        let replacement_rel =
            crate::content::slash_path(replacement_full.strip_prefix(&root).unwrap_or(&replacement_full));
        if !confirmed {
            let mut error = reasoned(
                "needs_confirmation",
                format!(
                    "deleting {rel} and pointing its {} referencer(s) at {replacement_rel} needs the owner's \
                     confirmation: repeat with `confirm: true`",
                    referencers.len()
                ),
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("referencers".into(), json!(referencers));
            }
            return Err(error);
        }
        let outcome = crate::content_actions::delete_with_replace(world, &root, &rel, &replacement_rel)
            .map_err(|e| e.to_rpc())?;
        world
            .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
            .remove(std::slice::from_ref(&rel));
        return Ok(json!({
            "path": rel,
            "deleted": true,
            "referencers": referencers,
            "replaced_with": outcome.to,
            "rewritten": outcome.rewritten,
        }));
    }
    if !confirmed {
        if !referencers.is_empty() {
            let mut error = reasoned(
                "asset_delete_referenced",
                format!(
                    "{rel} is referenced by {} file(s); pass `confirm: true` to delete it anyway",
                    referencers.len()
                ),
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("referencers".into(), json!(referencers));
            }
            return Err(error);
        }
        return Err(reasoned(
            "needs_confirmation",
            format!("deleting {rel} needs the owner's confirmation: repeat with `confirm: true`"),
        ));
    }
    crate::content::delete_asset_force(&root, &rel)
        .map_err(|e| RpcError::internal(format!("cannot delete {rel}: {e}")))?;
    crate::content_index::invalidate(world);
    world
        .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
        .remove(std::slice::from_ref(&rel));
    Ok(
        json!({ "path": rel, "deleted": true, "referencers": referencers, "replaced_with": null, "rewritten": [] }),
    )
}

// ---------------------------------------------------------------------------
// set_import_settings
// ---------------------------------------------------------------------------

/// `editor.set_import_settings`.
fn editor_set_import_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let root = project_root(world)?;
    let settings = match params.get("settings") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`settings` must be a RON settings object string, or null to clear it",
            ));
        }
    };
    let asynchronous = params.get("async").and_then(Value::as_bool) == Some(true);
    // The write and the cook are job `import_settings:<n>` (ADR-7664); its end invalidates the
    // content index.
    crate::import_settings_panel::tick_apply_job(world);
    crate::import_settings_panel::set_import_settings_job(world, &root, path, settings, asynchronous)
}

/// What `editor.set_import_settings` does after its parameters are read, shared with the Import
/// settings panel's Apply (ADR-7469) so both write the same `.meta` bytes for the same values:
/// the settings object is parsed and written canonically ([`SettingsObject::to_ron`]), checked
/// against the source's importer type ([`crate::import_settings_panel::validate_settings`]),
/// written atomically into the `.meta`, and the project is cooked once (incremental: the cook
/// hashes the settings, so this source re-cooks).
///
/// `settings` `None` clears the override.
///
/// [`SettingsObject::to_ron`]: regia_asset::import_rules::SettingsObject::to_ron
///
/// # Errors
///
/// `path_outside_project`, a missing file (`F0003`), `asset_not_imported` (no `.meta`),
/// `asset_settings_invalid` (not a settings object, or a value the importer refuses), or an IO
/// or cook failure (`F0001`).
pub(crate) fn set_import_settings_at(
    root: &Path,
    path: &str,
    settings: Option<&str>,
) -> Result<Value, RpcError> {
    let plan = plan_import_settings(root, path, settings)?;
    crate::atomic_write::write_atomic(&plan.meta_path, &plan.new_meta)
        .map_err(|e| RpcError::internal(format!("cannot write {}: {e}", plan.meta_path.display())))?;
    // The cook hashes the settings: the incremental cook re-cooks this source so the new
    // settings take effect in the dev cache.
    let report = cook(root)?;
    IMPORT_SETTINGS_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(plan.result(&report))
}

/// An import-settings write checked and ready (ADR-7664): what [`set_import_settings_at`] writes,
/// split so the Import settings Apply job can write and cook on a worker thread and put the old
/// `.meta` back on a cancel.
#[derive(Debug, Clone)]
pub(crate) struct SettingsPlan {
    /// The project root.
    pub root: PathBuf,
    /// The source, project-relative (`assets/...`).
    pub rel: String,
    /// The source's `.meta`.
    pub meta_path: PathBuf,
    /// The `.meta` bytes before the write (a cancel writes them back).
    pub old_meta: Vec<u8>,
    /// The `.meta` bytes the write puts in place.
    pub new_meta: Vec<u8>,
    /// The canonical settings object (`None`: the override is cleared).
    pub settings: Option<String>,
}

impl SettingsPlan {
    /// The `editor.set_import_settings` result for a cook `report`.
    pub fn result(&self, report: &regia_asset_pipeline::CookReport) -> Value {
        json!({
            "path": self.rel,
            "settings": self.settings,
            "cooked": report.cooked,
            "skipped": report.skipped,
            "errors": cook_errors(report),
        })
    }
}

/// Checks an import-settings write without touching a file: the path, the `.meta`, the settings
/// object and its values ([`set_import_settings_at`]'s checks, in its order and with its codes).
///
/// # Errors
///
/// As [`set_import_settings_at`], except the write and the cook.
pub(crate) fn plan_import_settings(
    root: &Path,
    path: &str,
    settings: Option<&str>,
) -> Result<SettingsPlan, RpcError> {
    let full = assets_request_path(root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(root).unwrap_or(&full));
    if !full.is_file() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist or is not a file"),
            "editor.list_assets shows the project's files",
        ));
    }
    let meta_path = regia_asset::meta_path_for(&full);
    let meta_bytes = std::fs::read(&meta_path).map_err(|_| {
        reasoned(
            "asset_not_imported",
            format!(
                "{rel} has no .meta: cook it once first (import it, or run editor.reimport) \
                 before setting import settings"
            ),
        )
    })?;
    let mut meta = regia_asset::MetaSidecar::from_ron(&meta_bytes)
        .map_err(|e| RpcError::invalid_params(format!("{} does not parse: {e}", meta_path.display())))?;
    let settings = match settings {
        None => None,
        Some(text) => {
            let object = regia_asset::import_rules::SettingsObject::parse(text).map_err(|e| {
                reasoned(
                    "asset_settings_invalid",
                    format!("`settings` is not a settings object: {e}"),
                )
            })?;
            let canonical = object.to_ron();
            crate::import_settings_panel::validate_settings(&rel, &canonical)
                .map_err(|message| reasoned("asset_settings_invalid", format!("{rel}: {message}")))?;
            Some(canonical)
        }
    };
    meta.settings = settings.clone();
    let text = meta
        .to_ron_string()
        .map_err(|e| RpcError::internal(format!("cannot serialize {}: {e}", meta_path.display())))?;
    Ok(SettingsPlan {
        root: root.to_path_buf(),
        rel,
        meta_path,
        old_meta: meta_bytes,
        new_meta: text.into_bytes(),
        settings,
    })
}

/// The project's incremental cook as the Import settings Apply job runs it (a `String` error).
pub(crate) fn cook_for_job(root: &Path) -> Result<regia_asset_pipeline::CookReport, String> {
    cook(root).map_err(|e| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch project (pid + counter, never the source tree) with the editor resources.
    fn edit_world(label: &str) -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    /// Calls a method through the registered extension, as dispatch does (spec 96).
    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .cloned()
            .unwrap_or_default();
        for (schema, handler) in methods() {
            extension.register(schema, handler);
        }
        world.insert_resource(extension);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    /// The `data.reason` of a refused call.
    fn reason(result: Result<Value, RpcError>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value}"),
            Err(error) => error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// A minimal valid PNG (1×1), encoded with the image crate (the pipeline's own dependency).
    fn tiny_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
            encoder
                .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .expect("png encode");
        }
        bytes
    }

    /// Imports one PNG through the content pipeline so it is cooked and has a `.meta`.
    fn import_png(root: &Path) {
        let source = root.join("incoming.png");
        std::fs::write(&source, tiny_png()).expect("png");
        crate::content::import_files(root, "", std::slice::from_ref(&source), Some("test-agent"))
            .expect("import");
    }

    #[test]
    fn edit_asset_patches_canonically_and_undo_restores_the_text() {
        // Spec 23 § 3.3 `edit_asset`: the patch applies over the typed document, the save is
        // canonical (re-parsing and re-saving changes nothing further), and one undo entry
        // restores the previous file text.
        let (mut world, root) = edit_world("edit-asset");
        let created = crate::content::create_asset(
            &root,
            crate::content::NewAssetKind::Material,
            "assets/mat/crate.material.ron",
        )
        .expect("material");
        let original_text = std::fs::read_to_string(root.join(&created)).expect("original text");
        // Cook once so the `.meta` sidecar exists for the provenance stamp.
        call(&mut world, "editor.reimport", json!({ "path": &created })).expect("reimport");
        let out = call(
            &mut world,
            "editor.edit_asset",
            json!({
                "path": created,
                "patch": [
                    { "path": "base.metallic", "value": 0.25 },
                    { "path": "base.roughness", "value": 0.5 },
                ],
                "client": "test-agent",
            }),
        )
        .expect("edit_asset");
        assert_eq!(out["patched"], 2);
        assert!(out["undoable"].as_u64().is_some_and(|depth| depth >= 1));

        let text = std::fs::read_to_string(root.join(&created)).expect("material text");
        let material = regia_asset::MaterialAssetRon::from_ron(text.as_bytes()).expect("parses");
        assert_eq!(material.base.metallic, 0.25);
        assert_eq!(material.base.roughness, 0.5);
        // Canonical: the typed re-save is byte-identical.
        let resaved = material.to_ron_string().expect("resave");
        assert_eq!(resaved, text, "the save is canonical");

        // Provenance records the agent (specs/23 § 5).
        let meta_bytes = std::fs::read(root.join(format!("{created}.meta"))).expect("meta");
        let meta = regia_asset::MetaSidecar::from_ron(&meta_bytes).expect("meta parses");
        let provenance = meta.provenance.expect("provenance");
        assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Agent);
        assert_eq!(provenance.client.as_deref(), Some("test-agent"));

        // One undo restores the previous text.
        assert!(crate::agent_ext::test_support::undo(&mut world));
        let restored = std::fs::read_to_string(root.join(&created)).expect("restored");
        assert_eq!(restored, original_text, "undo restores the file text");
    }

    #[test]
    fn edit_asset_refuses_bad_paths_kinds_and_patch_paths() {
        // Spec 23 § 3.3 `edit_asset` refusals: escaping paths, unsupported kinds and patch
        // paths that do not exist, each with its stable reason.
        let (mut world, root) = edit_world("edit-asset-refusals");
        let created = crate::content::create_asset(
            &root,
            crate::content::NewAssetKind::Material,
            "assets/mat/crate.material.ron",
        )
        .expect("material");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": "../outside.material.ron", "patch": [{ "path": "a", "value": 1 }] })
            )),
            "path_outside_project"
        );
        std::fs::write(root.join("assets/raw.png"), tiny_png()).expect("png");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": "assets/raw.png", "patch": [{ "path": "a", "value": 1 }] })
            )),
            "asset_edit_unsupported_kind"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": created, "patch": [{ "path": "base.no_such_field", "value": 1 }] })
            )),
            "asset_edit_no_such_path"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": created, "patch": [{ "path": "base.metallic", "value": "shiny" }] })
            )),
            "asset_edit_bad_value"
        );
        // Nothing was written by the refused calls.
        let material = regia_asset::MaterialAssetRon::from_ron(
            std::fs::read(root.join(&created)).expect("bytes").as_slice(),
        )
        .expect("parses");
        assert_eq!(material.base.metallic, 0.0, "the default");
    }

    #[test]
    fn edit_asset_patches_a_prefab_document() {
        // Spec 23 § 3.3 `edit_asset` on a scene document: the JSON view addresses entities by
        // index and component values are RON-text strings; the save stays canonical.
        let (mut world, root) = edit_world("edit-asset-prefab");
        let created = crate::content::create_asset(
            &root,
            crate::content::NewAssetKind::Prefab,
            "assets/props/barrel.prefab.ron",
        )
        .expect("prefab");
        let out = call(
            &mut world,
            "editor.edit_asset",
            json!({
                "path": created,
                "patch": [{ "path": "entities.0.name", "value": "barrel_large" }],
            }),
        )
        .expect("edit_asset");
        assert_eq!(out["patched"], 1);
        let text = std::fs::read_to_string(root.join(&created)).expect("prefab text");
        let file = regia_scene::prelude::SceneFile::from_ron_str(&text).expect("parses");
        assert_eq!(file.entities[0].name.as_deref(), Some("barrel_large"));
        // Undo restores the original name.
        assert!(crate::agent_ext::test_support::undo(&mut world));
        let text = std::fs::read_to_string(root.join(&created)).expect("restored");
        let file = regia_scene::prelude::SceneFile::from_ron_str(&text).expect("parses");
        assert_eq!(file.entities[0].name.as_deref(), Some("barrel"));
    }

    #[test]
    fn reimport_forgets_the_cache_and_recooks() {
        // Spec 23 § 3.3 `reimport`: the cook cache entry is forgotten and the file re-cooks.
        let (mut world, root) = edit_world("reimport");
        import_png(&root);
        let meta = root.join("assets/incoming.png.meta");
        assert!(meta.is_file(), "the import cooked a meta");
        let out = call(
            &mut world,
            "editor.reimport",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("reimport");
        assert_eq!(out["path"], "assets/incoming.png");
        assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
        assert!(meta.is_file(), "the cook wrote a fresh meta");
        // Refusals: a missing file and an escaping path.
        let missing = call(
            &mut world,
            "editor.reimport",
            json!({ "path": "assets/ghost.png" }),
        );
        assert_eq!(missing.expect_err("missing").code, -32001);
        assert_eq!(
            reason(call(
                &mut world,
                "editor.reimport",
                json!({ "path": "../incoming.png" })
            )),
            "path_outside_project"
        );
    }

    #[test]
    fn move_asset_renames_file_and_meta_and_rewrites_path_mentions() {
        // Spec 23 § 3.3 `move_asset`: an asset moves with its `.meta`. Since ADR-7822 the agent
        // moves like the Content Browser: a file that mentions the path is rewritten (it used to
        // refuse with `asset_move_referenced`), and the move is one undo step.
        let (mut world, root) = edit_world("move-asset");
        import_png(&root);
        let out = call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/incoming.png", "to": "assets/textures/incoming.png" }),
        )
        .expect("move");
        assert_eq!(out["from"], "assets/incoming.png");
        assert_eq!(out["to"], "assets/textures/incoming.png");
        assert_eq!(out["undoable"], true);
        assert!(root.join("assets/textures/incoming.png").is_file());
        assert!(root.join("assets/textures/incoming.png.meta").is_file());
        assert!(!root.join("assets/incoming.png").exists());

        // A file mentioning the mover is rewritten, and undo puts both back.
        std::fs::write(root.join("assets/notes.ron"), "(note: \"assets/mover.png\")").expect("note");
        std::fs::write(root.join("assets/mover.png"), tiny_png()).expect("png");
        let out = call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/mover.png", "to": "assets/moved.png" }),
        )
        .expect("a referenced asset moves");
        assert_eq!(out["fixed_referencers"], json!(["assets/notes.ron"]), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("assets/notes.ron")).expect("note"),
            "(note: \"assets/moved.png\")"
        );
        assert!(root.join("assets/moved.png").is_file());
        let mut stack = world
            .remove_resource::<crate::command::UndoStack>()
            .expect("the move is on the undo stack");
        assert!(stack.undo(&mut world));
        assert!(root.join("assets/mover.png").is_file(), "undo moves it back");
        assert_eq!(
            std::fs::read_to_string(root.join("assets/notes.ron")).expect("note"),
            "(note: \"assets/mover.png\")",
            "undo restores the mention"
        );
        world.insert_resource(stack);

        // An existing destination refuses without touching the source.
        let err = call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/mover.png", "to": "assets/textures/incoming.png" }),
        )
        .expect_err("destination exists");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("asset_move_destination_exists")
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.move_asset",
                json!({ "from": "../mover.png", "to": "assets/x.png" })
            )),
            "path_outside_project"
        );
    }

    #[test]
    fn delete_asset_needs_confirmation_then_deletes_with_referencers_reported() {
        // Spec 23 § 3.3 `delete_asset` (needs confirmation, § 5): the first call answers the
        // confirmation refusal (with the referencers when any), the confirmed call deletes the
        // file and its `.meta` and reports what referenced it.
        let (mut world, root) = edit_world("delete-asset");
        import_png(&root);
        // Unreferenced: the plain confirmation refusal, then the delete.
        let err = call(
            &mut world,
            "editor.delete_asset",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect_err("unconfirmed");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("needs_confirmation")
        );
        assert!(root.join("assets/incoming.png").is_file(), "not yet deleted");
        let out = call(
            &mut world,
            "editor.delete_asset",
            json!({ "path": "assets/incoming.png", "confirm": true }),
        )
        .expect("confirmed delete");
        assert_eq!(out["deleted"], true);
        assert!(!root.join("assets/incoming.png").exists());
        assert!(!root.join("assets/incoming.png.meta").exists());

        // Referenced: the refusal names the referencers and `confirm: true` deletes anyway
        // (the Content Browser's "Delete anyway").
        std::fs::write(root.join("assets/other.png"), tiny_png()).expect("png");
        std::fs::write(root.join("assets/refs.ron"), "(uses: \"assets/other.png\")").expect("refs");
        let err = call(
            &mut world,
            "editor.delete_asset",
            json!({ "path": "assets/other.png" }),
        )
        .expect_err("referenced, unconfirmed");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("asset_delete_referenced")
        );
        let out = call(
            &mut world,
            "editor.delete_asset",
            json!({ "path": "assets/other.png", "confirm": true }),
        )
        .expect("delete anyway");
        assert_eq!(out["referencers"][0], "assets/refs.ron");
        assert!(!root.join("assets/other.png").exists());

        assert_eq!(
            reason(call(
                &mut world,
                "editor.delete_asset",
                json!({ "path": "../other.png", "confirm": true })
            )),
            "path_outside_project"
        );
    }

    /// ADR-7819 through the protocol: `editor.set_import_settings` with `lossless_clips: true` on a
    /// cooked `.bvh` recooks its clip as a lossless (`Raw`) `cooked-clip` v3 body, bit-exact to the
    /// imported clip; clearing the settings compresses it again.
    #[test]
    fn set_import_settings_keeps_a_clip_lossless() {
        use regia_animation::AnimationClip;
        use regia_asset_pipeline::bvh_import::{BvhImportSettings, FIXTURE_BVH, convert_bvh};

        let (mut world, root) = edit_world("set-import-settings-lossless-clip");
        std::fs::write(root.join("assets/walk.bvh"), FIXTURE_BVH).expect("bvh");
        let report =
            regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.clone()))
                .expect("first cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let payload = |root: &std::path::Path| {
            let cache = regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform());
            let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
                .expect("manifest")
                .expect("present");
            let id = manifest
                .id_for_path("assets/walk.bvh#clip/walk")
                .expect("clip id");
            let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked");
            regia_asset::decode_cooked_file(&bytes)
                .expect("decodes")
                .1
                .to_vec()
        };
        assert!(AnimationClip::compressed_cooked_payload(&payload(&root)).is_some());

        let out = call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/walk.bvh", "settings": "(lossless_clips: true)" }),
        )
        .expect("set_import_settings");
        assert!(out["errors"].as_array().is_some_and(Vec::is_empty), "{out}");
        assert_eq!(out["settings"], "(lossless_clips: true)");
        let source = convert_bvh(FIXTURE_BVH.as_bytes(), "walk", &BvhImportSettings::default())
            .expect("import")
            .clip;
        let lossless = payload(&root);
        assert!(AnimationClip::compressed_cooked_payload(&lossless).is_none());
        assert_eq!(lossless, source.to_cooked_payload().expect("payload"));

        call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/walk.bvh" }),
        )
        .expect("clear");
        assert!(AnimationClip::compressed_cooked_payload(&payload(&root)).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_import_settings_writes_the_meta_and_recooks() {
        // Spec 23 § 3.3 `set_import_settings`: the settings object lands in the source's
        // `.meta` (over folder rules), the invalid form is refused, and the cook applies them.
        let (mut world, root) = edit_world("set-import-settings");
        import_png(&root);
        let out = call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(64))" }),
        )
        .expect("set_import_settings");
        assert_eq!(out["path"], "assets/incoming.png");
        assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
        let meta_bytes = std::fs::read(root.join("assets/incoming.png.meta")).expect("meta");
        let meta = regia_asset::MetaSidecar::from_ron(&meta_bytes).expect("meta parses");
        let settings = meta.settings.expect("settings written");
        // The effective settings of the desktop pipeline see the override.
        let effective = regia_asset_pipeline::import_rules::effective_settings(
            &root,
            "assets/incoming.png",
            "png",
            Some(&settings),
        )
        .expect("effective settings");
        let merged = regia_asset::import_rules::SettingsObject::parse(&effective.settings)
            .expect("the merged settings parse");
        assert_eq!(
            merged.get("max_size"),
            Some("Some(64)"),
            "the meta beats the folder rules: {effective:?}"
        );

        // Clearing: absent `settings` removes the override.
        let out = call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("clear");
        assert_eq!(out["settings"], Value::Null);
        let meta = regia_asset::MetaSidecar::from_ron(
            std::fs::read(root.join("assets/incoming.png.meta"))
                .expect("meta")
                .as_slice(),
        )
        .expect("meta parses");
        assert_eq!(meta.settings, None);

        // Refusals: a malformed object and a source without a `.meta`.
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "assets/incoming.png", "settings": "(max_size: [1, 2)" })
            )),
            "asset_settings_invalid"
        );
        std::fs::write(root.join("assets/raw.png"), tiny_png()).expect("png");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "assets/raw.png", "settings": "(max_size: Some(64))" })
            )),
            "asset_not_imported"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "../incoming.png", "settings": "(max_size: Some(64))" })
            )),
            "path_outside_project"
        );
    }

    /// The `data.reason` of a refused call, or `ok` when it succeeded (the fixrev3 tests collect
    /// every case before asserting, so one run shows every form that slipped through).
    fn outcome(result: Result<Value, RpcError>) -> String {
        match result {
            Ok(_) => "ok".to_string(),
            Err(error) => error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("<no reason>")
                .to_string(),
        }
    }

    /// Request path spellings that leave the project (or never name a file in it): `..`, absolute,
    /// root-relative, Windows drive, drive-relative and UNC forms in both separator styles, and a
    /// device name. ADR-1816 refuses each with `path_outside_project` on every host.
    const ESCAPING_FORMS: &[&str] = &[
        "../x.material.ron",
        "assets/../../x.material.ron",
        "sub/../../../x.material.ron",
        "..\\x.material.ron",
        "/tmp/regia-fixrev3/x.material.ron",
        "assets//tmp/regia-fixrev3/x.material.ron",
        "\\tmp\\x.material.ron",
        "C:/Users/x/x.material.ron",
        "C:\\Users\\x\\x.material.ron",
        "C:x.material.ron",
        "\\\\server\\share\\x.material.ron",
        "//server/share/x.material.ron",
        "\\\\?\\C:\\x.material.ron",
        "sub/nul.material.ron",
    ];

    #[test]
    fn move_asset_destination_stays_in_the_project() {
        // Review rev-kimi-8 findings 1-2 (ADR-1816, ADR-2821, spec 23 § 5): `to` goes through
        // the same request-path rule as `from`, so a move cannot leave the project (`..`,
        // absolute, drive, UNC, a symlinked folder) or land under a goldens directory. Each case
        // moves its own source, so a case that slipped through cannot hide the next one.
        let (mut world, root) = edit_world("move-asset-dest");
        let mut cases: Vec<(String, &str)> = ESCAPING_FORMS
            .iter()
            .map(|form| (form.replace(".material.ron", ".png"), "path_outside_project"))
            .collect();
        cases.push(("goldens/x.png".into(), "golden_path_denied"));
        cases.push(("assets/sub/Goldens/x.png".into(), "golden_path_denied"));
        let mut wrong = Vec::new();
        for (i, (to, want)) in cases.iter().enumerate() {
            let from = format!("assets/src{i}.png");
            std::fs::write(root.join(&from), tiny_png()).expect("png");
            let got = outcome(call(
                &mut world,
                "editor.move_asset",
                json!({ "from": &from, "to": to }),
            ));
            if got != *want || !root.join(&from).is_file() {
                wrong.push(format!(
                    "{to:?}: {got} (source kept: {})",
                    root.join(&from).is_file()
                ));
            }
        }
        // A folder symlinked out of the project: the destination resolves outside.
        #[cfg(unix)]
        let outside = {
            let outside = regia_test_utils::unique_temp_dir("move-asset-dest-outside");
            std::fs::create_dir_all(&outside).expect("outside");
            std::os::unix::fs::symlink(&outside, root.join("assets/link")).expect("symlink");
            for to in ["link/x.png", "assets/link/deeper/x.png"] {
                std::fs::write(root.join("assets/linked.png"), tiny_png()).expect("png");
                let got = outcome(call(
                    &mut world,
                    "editor.move_asset",
                    json!({ "from": "assets/linked.png", "to": to }),
                ));
                if got != "path_outside_project" {
                    wrong.push(format!("{to:?} through a symlink: {got}"));
                }
            }
            assert!(
                !outside.join("x.png").exists(),
                "a file left the project through a symlink"
            );
            outside
        };
        // Dot segments stay the destination rule's own refusal.
        std::fs::write(root.join("assets/dot.png"), tiny_png()).expect("png");
        let got = outcome(call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/dot.png", "to": "textures/.hidden.png" }),
        ));
        if got != "asset_bad_destination" {
            wrong.push(format!("\"textures/.hidden.png\": {got}"));
        }
        assert!(
            wrong.is_empty(),
            "destinations not refused as expected:\n{}",
            wrong.join("\n")
        );
        // A plain move still works.
        call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/dot.png", "to": "textures/dot.png" }),
        )
        .expect("an in-project move");
        assert!(root.join("assets/textures/dot.png").is_file());
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(outside);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn request_paths_refuse_escaping_forms_on_every_method() {
        // Brief fixrev3: every path parameter of the five methods refuses `..`, absolute, drive,
        // UNC and device forms with `path_outside_project`, as book/agents/asset-ops.md promises.
        // Before the fix, the `assets/` prefix the handlers add turned `/x` and `\\\\server\\x`
        // into `assets/x` (empty segments dropped), answering not-found instead.
        let (mut world, root) = edit_world("request-path-forms");
        let mut wrong = Vec::new();
        for form in ESCAPING_FORMS {
            let calls = [
                (
                    "editor.edit_asset",
                    json!({ "path": form, "patch": [{ "path": "name", "value": "x" }] }),
                ),
                ("editor.reimport", json!({ "path": form })),
                (
                    "editor.move_asset",
                    json!({ "from": form, "to": "moved.material.ron" }),
                ),
                ("editor.delete_asset", json!({ "path": form, "confirm": true })),
                (
                    "editor.set_import_settings",
                    json!({ "path": form, "settings": "(max_size: Some(64))" }),
                ),
            ];
            for (method, params) in calls {
                let got = outcome(call(&mut world, method, params));
                if got != "path_outside_project" {
                    wrong.push(format!("{method} {form:?}: {got}"));
                }
            }
        }
        // A symlinked folder whose target holds a real material: reading or writing through it
        // would edit a file outside the project.
        #[cfg(unix)]
        let outside = {
            let outside = regia_test_utils::unique_temp_dir("request-path-forms-outside");
            std::fs::create_dir_all(&outside).expect("outside");
            crate::content::create_asset(
                &root,
                crate::content::NewAssetKind::Material,
                "assets/real.material.ron",
            )
            .expect("material");
            std::fs::copy(
                root.join("assets/real.material.ron"),
                outside.join("x.material.ron"),
            )
            .expect("copy");
            std::os::unix::fs::symlink(&outside, root.join("assets/link")).expect("symlink");
            let form = "link/x.material.ron";
            let calls = [
                (
                    "editor.edit_asset",
                    json!({ "path": form, "patch": [{ "path": "name", "value": "x" }] }),
                ),
                ("editor.reimport", json!({ "path": form })),
                (
                    "editor.move_asset",
                    json!({ "from": form, "to": "moved.material.ron" }),
                ),
                ("editor.delete_asset", json!({ "path": form, "confirm": true })),
                (
                    "editor.set_import_settings",
                    json!({ "path": form, "settings": "(max_size: Some(64))" }),
                ),
            ];
            for (method, params) in calls {
                let got = outcome(call(&mut world, method, params));
                if got != "path_outside_project" {
                    wrong.push(format!("{method} {form:?} (symlink): {got}"));
                }
            }
            assert!(
                outside.join("x.material.ron").is_file(),
                "the outside file is untouched"
            );
            outside
        };
        assert!(
            wrong.is_empty(),
            "request paths not refused:\n{}",
            wrong.join("\n")
        );
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(outside);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_assets_folder_itself_is_not_a_request_path() {
        // `assets/`, `assets/.` and `.` name the assets folder, not a file under it: refused as
        // an empty path (`path_outside_project`, `path_reason` `path_empty`) by every method that
        // takes one, so no call moves, deletes or re-cooks the whole folder.
        let (mut world, root) = edit_world("assets-folder-itself");
        import_png(&root);
        let mut wrong = Vec::new();
        for form in ["assets/", "assets/.", ".", "./", "assets\\."] {
            let calls = [
                ("editor.reimport", json!({ "path": form })),
                ("editor.move_asset", json!({ "from": form, "to": "moved" })),
                (
                    "editor.move_asset",
                    json!({ "from": "assets/incoming.png", "to": form }),
                ),
                ("editor.delete_asset", json!({ "path": form, "confirm": true })),
                ("editor.set_import_settings", json!({ "path": form })),
            ];
            for (method, params) in calls {
                let result = call(&mut world, method, params);
                let path_reason = result
                    .as_ref()
                    .err()
                    .and_then(|e| e.data.as_ref())
                    .and_then(|d| d.get("path_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let got = outcome(result);
                if got != "path_outside_project" || path_reason != "path_empty" {
                    wrong.push(format!("{method} {form:?}: {got} {path_reason}"));
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "the assets folder was accepted:\n{}",
            wrong.join("\n")
        );
        assert!(
            root.join("assets/incoming.png").is_file(),
            "nothing moved or deleted"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The asset's `.meta` sidecar.
    fn meta_of(root: &Path, rel: &str) -> regia_asset::MetaSidecar {
        regia_asset::MetaSidecar::from_ron(&std::fs::read(root.join(format!("{rel}.meta"))).expect("meta"))
            .expect("meta parses")
    }

    #[test]
    fn reimport_keeps_the_asset_id_settings_and_provenance() {
        // Review rev-kimi-8 finding 3: reimport re-cooks the source but keeps its `.meta` — the
        // asset id (levels store `asset:<guid>`), the import settings set_import_settings wrote
        // and the agent provenance record the review policy reads (spec 23 § 5).
        let (mut world, root) = edit_world("reimport-keeps-meta");
        import_png(&root);
        call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(64))" }),
        )
        .expect("settings");
        let before = meta_of(&root, "assets/incoming.png");
        assert!(before.provenance.is_some(), "the import stamped provenance");
        let out = call(
            &mut world,
            "editor.reimport",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("reimport");
        let after = meta_of(&root, "assets/incoming.png");
        assert_eq!(
            (before.id, before.settings.clone(), before.provenance.is_some()),
            (after.id, after.settings.clone(), after.provenance.is_some()),
            "reimport kept id, settings and provenance"
        );
        assert_eq!(after.provenance, before.provenance);
        // The cache was really forgotten: the source re-cooked rather than matching its key.
        assert_eq!(out["cooked"], 1, "{out}");
        assert_eq!(out["skipped"], 0, "{out}");
        assert_eq!(out["errors"], json!([]), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn edit_asset_refuses_the_level_open_in_the_editor() {
        // Review rev-kimi-8 finding 5: patching the open level's file would make the asset
        // watcher reload it (dropping the undo entry the call just returned) or, with unsaved
        // edits, raise the owner's reload prompt. Refused with `asset_level_open`; another level
        // still patches.
        let (mut world, root) = edit_world("edit-asset-open-level");
        let open = crate::content::create_asset(
            &root,
            crate::content::NewAssetKind::Level,
            "assets/maps/open.level.ron",
        )
        .expect("level");
        let other = crate::content::create_asset(
            &root,
            crate::content::NewAssetKind::Level,
            "assets/maps/other.level.ron",
        )
        .expect("level");
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::level::LevelId::default(),
            path: Some(root.join(&open)),
        });
        let text = std::fs::read_to_string(root.join(&open)).expect("text");
        let patch = json!([{ "path": "entities", "value": [] }]);
        assert_eq!(
            outcome(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": &open, "patch": patch }),
            )),
            "asset_level_open"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(&open)).expect("text"),
            text,
            "nothing written"
        );
        assert_eq!(
            outcome(call(
                &mut world,
                "editor.edit_asset",
                json!({ "path": &other, "patch": patch }),
            )),
            "ok"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn move_asset_keeps_id_references_and_reports_them() {
        // Review rev-kimi-8 finding 8: an `asset:<guid>` reference survives a move (the `.meta`
        // and its id move with the file), so only a path mention blocks it; the id referencers
        // are reported in `id_referencers`. The moved source cooks under the same id.
        let (mut world, root) = edit_world("move-asset-id-refs");
        import_png(&root);
        let id = meta_of(&root, "assets/incoming.png").id;
        std::fs::write(root.join("assets/refs.ron"), format!("(texture: \"asset:{id}\")"))
            .expect("id referencer");
        let out = call(
            &mut world,
            "editor.move_asset",
            json!({ "from": "assets/incoming.png", "to": "textures/incoming.png" }),
        )
        .expect("an id reference does not block a move");
        assert_eq!(out["id_referencers"], json!(["assets/refs.ron"]), "{out}");
        assert_eq!(meta_of(&root, "assets/textures/incoming.png").id, id);
        let report = cook(&root).expect("cook");
        assert!(
            report
                .errors
                .iter()
                .all(|(path, _)| !path.contains("incoming.png")),
            "{:?}",
            report.errors
        );
        assert_eq!(
            meta_of(&root, "assets/textures/incoming.png").id,
            id,
            "same id after the cook"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_agent_and_the_content_browser_move_the_same_way() {
        // ADR-7822 (P-143): one move behavior. The same project moved through the Content
        // Browser's path (`project_ops::move_asset` + `push_move_undo`, content.rs Rename / Move)
        // and through `editor.move_asset` ends with the same files and the same texts.
        let setup = |label: &str| {
            let (world, root) = edit_world(label);
            std::fs::create_dir_all(root.join("assets/props")).expect("props");
            std::fs::write(root.join("assets/props/rock.png"), tiny_png()).expect("png");
            std::fs::write(
                root.join("assets/arena.level.ron"),
                "(a: \"assets/props/rock.png\", b: \"props/rock.png\", c: \"props/rock.png.bak\")",
            )
            .expect("level");
            (world, root)
        };
        let (mut ui_world, ui_root) = setup("move-parity-ui");
        let outcome =
            regia_asset::project_ops::move_asset(&ui_root, "assets/props/rock.png", "assets/stone/rock.png")
                .expect("ui move");
        crate::content::push_move_undo(
            &mut ui_world,
            ui_root.clone(),
            "assets/props/rock.png".into(),
            "assets/stone/rock.png".into(),
        );
        let (mut agent_world, agent_root) = setup("move-parity-agent");
        let out = call(
            &mut agent_world,
            "editor.move_asset",
            json!({ "from": "assets/props/rock.png", "to": "assets/stone/rock.png" }),
        )
        .expect("agent move");
        assert_eq!(out["fixed_referencers"], json!(outcome.fixed_referencers));
        let level =
            |root: &Path| std::fs::read_to_string(root.join("assets/arena.level.ron")).expect("level");
        assert_eq!(level(&ui_root), level(&agent_root));
        assert_eq!(
            level(&agent_root),
            "(a: \"assets/stone/rock.png\", b: \"stone/rock.png\", c: \"props/rock.png.bak\")"
        );
        for world in [&ui_world, &agent_world] {
            assert_eq!(
                world.resource::<crate::command::UndoStack>().undo_depth(),
                1,
                "one undo step each"
            );
        }
        let _ = std::fs::remove_dir_all(&ui_root);
        let _ = std::fs::remove_dir_all(&agent_root);
    }

    #[test]
    fn every_refusal_reason_is_documented_in_the_book_page() {
        // Review rev-kimi-8 finding 4 (COMMON RULES: agent error codes are documented on the
        // book page): every `reasoned("...")` code of this module has an Errors-table row.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(manifest.join("src/asset_ops_agent.rs")).expect("source");
        let book =
            std::fs::read_to_string(manifest.join("../../book/agents/asset-ops.md")).expect("book page");
        let mut missing = Vec::new();
        let mut rest = source.as_str();
        while let Some(at) = rest.find("reasoned(") {
            rest = &rest[at + "reasoned(".len()..];
            // A call site passes the code as its first argument, a string literal.
            let Some(after) = rest.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(close) = after.find('"') else { break };
            let code = &after[..close];
            if !code.is_empty()
                && code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                && !book.contains(&format!("| `{code}` |"))
                && !missing.contains(&code)
            {
                missing.push(code);
            }
        }
        assert!(
            missing.is_empty(),
            "reasons missing from the Errors table: {missing:?}"
        );
    }
}
