// SPDX-License-Identifier: MPL-2.0
//! Cross-project user asset library (ADR-8201).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_asset::{AssetId, MetaSidecar, meta_path_for};
use regia_asset_pipeline::asset_package::{ASSET_PACKAGE_MANIFEST, AssetPackageManifest, package_hash};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::asset_package;
use crate::content::ContentBrowserState;
use crate::viewport_agent::Handler;

fn error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        code,
        message,
        "See editor.asset_library in the Content Browser guide",
    )
}

fn io(e: impl std::fmt::Display) -> RpcError {
    error("library_io", e.to_string())
}

fn package_error(e: asset_package::PackageError) -> RpcError {
    error("library_package", format!("{}: {}", e.code, e.message))
}

// Resolve an absent directory through its nearest existing ancestor, including symlink aliases.
fn resolved(path: &Path) -> Result<PathBuf, RpcError> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().filter(|p| *p != path).ok_or_else(|| io(e))?;
            let name = path
                .file_name()
                .ok_or_else(|| error("library_path", "invalid library directory"))?;
            Ok(resolved(parent)?.join(name))
        }
        Err(e) => Err(io(e)),
    }
}

fn directory(world: &World) -> Result<PathBuf, RpcError> {
    let config = world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|paths| paths.dir.as_ref())
        .ok_or_else(|| error("library_unconfigured", "The editor has no user config directory"))?;
    let config = if config.is_absolute() {
        config.clone()
    } else {
        std::env::current_dir().map_err(io)?.join(config)
    };
    let library = resolved(&config.join("asset_library"))?;
    if let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.as_ref())
    {
        let root = root.canonicalize().map_err(io)?;
        if library.starts_with(&root) || root.starts_with(&library) {
            return Err(error(
                "library_path",
                "The user library must be outside the project",
            ));
        }
    }
    Ok(library)
}

fn project(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| error("library_no_project", "Open a project first"))
}

// Do not follow source, destination, sidecar or package symlinks, including intermediate folders.
fn no_links(root: &Path, relative: &Path) -> Result<PathBuf, RpcError> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(error(
                "library_path",
                "Expected a relative path without parent components",
            ));
        };
        path.push(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(error(
                    "library_path",
                    "Library copies do not follow symbolic links",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
    }
    Ok(path)
}

struct Stage(PathBuf);
impl Stage {
    fn new(parent: &Path) -> Result<Self, RpcError> {
        std::fs::create_dir_all(parent).map_err(io)?;
        let path = parent.join(format!(".stage-{}", AssetId::new_random()));
        std::fs::create_dir(&path).map_err(io)?;
        Ok(Self(path))
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), RpcError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    crate::atomic_write::write_atomic(path, bytes).map_err(io)
}

fn manifest_write(dir: &Path, manifest: &AssetPackageManifest) -> Result<(), RpcError> {
    write(
        &dir.join(ASSET_PACKAGE_MANIFEST),
        manifest.to_ron().map_err(io)?.as_bytes(),
    )
}

fn meta_read(root: &Path, rel: &str) -> Result<Option<MetaSidecar>, RpcError> {
    let path = no_links(root, &meta_path_for(Path::new(rel)))?;
    match std::fs::read(path) {
        Ok(bytes) => MetaSidecar::from_ron(&bytes).map(Some).map_err(io),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e)),
    }
}

fn entry_id(entry: &str) -> Result<(), RpcError> {
    if entry.parse::<AssetId>().is_ok_and(|id| id.to_string() == entry) {
        Ok(())
    } else {
        Err(error(
            "library_path",
            "entry must be an ID returned by list or add",
        ))
    }
}

fn list(dir: &Path) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_library_list").entered();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"entries": [], "errors": []}));
        }
        Err(e) => return Err(io(e)),
    };
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry_id(&name).is_err() {
            continue;
        }
        let read = || {
            no_links(dir, Path::new(&name))?;
            no_links(&entry.path(), Path::new(ASSET_PACKAGE_MANIFEST))?;
            asset_package::read_manifest(&entry.path()).map_err(package_error)
        };
        match read() {
            Ok(manifest) => {
                out.push(json!({"entry": name, "roots": manifest.roots, "assets": manifest.assets.len()}))
            }
            Err(e) => errors.push(format!("{name}: {}", e.message)),
        }
    }
    out.sort_by(|a, b| a["entry"].as_str().cmp(&b["entry"].as_str()));
    Ok(json!({"entries": out, "errors": errors}))
}

fn gltf_dependencies(root: &Path, rel: &str) -> Result<Vec<String>, RpcError> {
    let bytes = std::fs::read(no_links(root, Path::new(rel))?).map_err(io)?;
    let gltf = regia_asset_pipeline::parse_gltf_checked(&bytes).map_err(|e| error("library_package", e))?;
    let buffers = gltf.buffers().filter_map(|buffer| match buffer.source() {
        gltf::buffer::Source::Uri(uri) => Some(uri),
        gltf::buffer::Source::Bin => None,
    });
    let images = gltf.images().filter_map(|image| match image.source() {
        gltf::image::Source::Uri { uri, .. } => Some(uri),
        gltf::image::Source::View { .. } => None,
    });
    let mut dependencies = Vec::new();
    for uri in buffers.chain(images) {
        if uri.starts_with("data:") {
            continue;
        }
        let decoded = regia_asset_pipeline::decode_gltf_uri(uri)
            .ok_or_else(|| error("library_path", "A glTF URI is not UTF-8"))?;
        if decoded.starts_with('/') || decoded.contains([':', '\\']) {
            return Err(error(
                "library_path",
                "Library glTF dependencies must be project-local files",
            ));
        }
        let base = rel.rsplit_once('/').map_or("assets", |(base, _)| base);
        let mut parts: Vec<&str> = base.split('/').collect();
        for part in decoded.split('/') {
            match part {
                "" | "." => {}
                ".." if parts.len() > 1 => {
                    parts.pop();
                }
                ".." => return Err(error("library_path", "A glTF dependency leaves assets/")),
                _ => parts.push(part),
            }
        }
        let path = parts.join("/");
        if !no_links(root, Path::new(&path))?.is_file() {
            return Err(error(
                "library_package",
                format!("Missing glTF dependency {path}"),
            ));
        }
        dependencies.push(path);
    }
    Ok(dependencies)
}

fn is_gltf(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".gltf") || path.ends_with(".glb")
}

fn add(root: &Path, dir: &Path, path: &str) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_library_add").entered();
    if !regia_asset_pipeline::asset_package::is_package_path(path) {
        return Err(error("library_path", "path must name an asset under assets/"));
    }
    no_links(root, Path::new(path))?;
    no_links(root, Path::new("assets"))?;
    let mut index = asset_package::ReferenceIndex::build(root);
    // Package discovery knows primary string IDs. Include sub-assets and decimal prefab IDs too.
    let mut owners = BTreeMap::new();
    let mut source_errors = BTreeMap::new();
    for rel in index.deps.keys() {
        let meta = match meta_read(root, rel) {
            Ok(meta) => meta,
            Err(e) => {
                source_errors.insert(rel.clone(), e);
                None
            }
        };
        if let Some(meta) = meta {
            for id in std::iter::once(meta.id).chain(meta.sub_assets.iter().map(|s| s.id)) {
                owners.insert(id.to_string(), rel.clone());
                owners.insert(format!("number:{}", id.0.as_u128()), rel.clone());
            }
        }
    }
    let known: BTreeSet<_> = index.deps.keys().cloned().collect();
    for (rel, deps) in &mut index.deps {
        if rel.ends_with(".ron") {
            let text = match std::fs::read_to_string(no_links(root, Path::new(rel))?) {
                Ok(text) => text,
                Err(e) => {
                    source_errors.insert(rel.clone(), io(e));
                    continue;
                }
            };
            if let Err(e) = map_references(&text, |value, numeric| {
                let value = value.split_once('#').map_or(value, |(base, _)| base);
                let id = if numeric {
                    format!("number:{value}")
                } else {
                    value.strip_prefix("asset:").unwrap_or(value).to_string()
                };
                let path = if value.starts_with("assets/") {
                    value.to_string()
                } else {
                    format!("assets/{value}")
                };
                if let Some(owner) = owners
                    .get(&id)
                    .or_else(|| if numeric { None } else { known.get(&path) })
                    && owner != rel
                {
                    deps.insert(owner.clone());
                }
                None
            }) {
                source_errors.insert(rel.clone(), e);
            }
        } else if is_gltf(rel) {
            match gltf_dependencies(root, rel) {
                Ok(paths) => deps.extend(paths),
                Err(e) => {
                    source_errors.insert(rel.clone(), e);
                }
            }
        }
    }
    let closure = asset_package::dependency_closure(&index, &[path.to_string()]);
    for rel in &closure {
        no_links(root, Path::new(rel))?;
        if let Some(e) = source_errors.remove(rel) {
            return Err(e);
        }
    }
    let stage = Stage::new(dir)?;
    let mut report = asset_package::export_package(root, &closure, &stage.0).map_err(package_error)?;
    report.manifest.roots = vec![path.to_string()];
    manifest_write(&stage.0, &report.manifest)?;
    let entry = AssetId::new_random().to_string();
    std::fs::rename(&stage.0, dir.join(&entry)).map_err(io)?;
    Ok(json!({"entry": entry, "roots": report.manifest.roots, "assets": report.manifest.assets.len()}))
}

fn string_reference(
    text: &str,
    paths: &BTreeMap<String, String>,
    ids: &BTreeMap<String, String>,
) -> Option<String> {
    let (base, fragment) = text
        .split_once('#')
        .map_or((text, "".to_string()), |(base, fragment)| {
            (base, format!("#{fragment}"))
        });
    let mapped = if let Some(id) = ids.get(base) {
        id.clone()
    } else if let Some(id) = base.strip_prefix("asset:").and_then(|id| ids.get(id)) {
        format!("asset:{id}")
    } else if let Some(path) = paths.get(base) {
        path.clone()
    } else {
        let path = paths.get(&format!("assets/{base}"))?;
        path.strip_prefix("assets/").unwrap_or(path).to_string()
    };
    Some(format!("{mapped}{fragment}"))
}

// A small RON token walk: only whole string values and numeric ID tokens are replaced. Comments,
// labels, arbitrary string substrings and binary sources stay untouched. Changes use one map pass.
fn map_references(
    text: &str,
    mut replace: impl FnMut(&str, bool) -> Option<String>,
) -> Result<String, RpcError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut copied = 0;
    let mut out = String::new();
    let mut field = String::new();
    let mut prefab_id = false;
    while i < bytes.len() {
        let start = i;
        let replacement = match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                None
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                let mut depth = 1;
                while i + 1 < bytes.len() && depth > 0 {
                    if &bytes[i..i + 2] == b"/*" {
                        depth += 1;
                        i += 2;
                    } else if &bytes[i..i + 2] == b"*/" {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth != 0 {
                    return Err(error("library_package", "Unterminated RON comment"));
                }
                None
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i = (i + if bytes[i] == b'\\' { 2 } else { 1 }).min(bytes.len());
                }
                i = (i + 1).min(bytes.len());
                let value: String = ron::from_str(&text[start..i]).map_err(io)?;
                field.clone_from(&value);
                replace(&value, false)
                    .map(|value| ron::to_string(&value).map_err(io))
                    .transpose()?
            }
            b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#')) => {
                i += 1;
                while bytes.get(i) == Some(&b'#') {
                    i += 1;
                }
                let hashes = i - start - 1;
                if bytes.get(i) != Some(&b'"') {
                    continue;
                }
                i += 1;
                let end = format!("\"{}", "#".repeat(hashes));
                let offset = text[i..]
                    .find(&end)
                    .ok_or_else(|| error("library_package", "Unterminated RON string"))?;
                let value = &text[i..i + offset];
                field = value.to_string();
                i += offset + end.len();
                replace(value, false)
                    .map(|value| ron::to_string(&value).map_err(io))
                    .transpose()?
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                None
            }
            b'0'..=b'9' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.'))
                {
                    i += 1;
                }
                let token = text[start..i].replace('_', "");
                let token = token.strip_suffix("u128").unwrap_or(&token);
                let number = if let Some(hex) = token.strip_prefix("0x") {
                    u128::from_str_radix(hex, 16).ok()
                } else {
                    token.parse::<u128>().ok()
                };
                let replacement = if prefab_id {
                    number.and_then(|id| replace(&id.to_string(), true))
                } else {
                    None
                };
                prefab_id = false;
                replacement
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                field = text[start..i].to_string();
                None
            }
            b':' => {
                prefab_id = field == "prefab";
                i += 1;
                None
            }
            b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b'-' => {
                prefab_id = false;
                field.clear();
                i += 1;
                None
            }
            _ => {
                i += text[i..].chars().next().map_or(1, char::len_utf8);
                None
            }
        };
        if let Some(replacement) = replacement {
            out.push_str(&text[copied..start]);
            out.push_str(&replacement);
            copied = i;
        }
    }
    out.push_str(&text[copied..]);
    Ok(out)
}

fn remap_text(
    text: &str,
    paths: &BTreeMap<String, String>,
    ids: &BTreeMap<String, String>,
) -> Result<String, RpcError> {
    map_references(text, |value, numeric| {
        if numeric {
            ids.get(&format!("number:{value}")).cloned()
        } else {
            string_reference(value, paths, ids)
        }
    })
}

fn use_entry(root: &Path, dir: &Path, entry: &str) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_library_use").entered();
    entry_id(entry)?;
    let package = no_links(dir, Path::new(entry))?;
    no_links(&package, Path::new(ASSET_PACKAGE_MANIFEST))?;
    let mut manifest = asset_package::read_manifest(&package).map_err(package_error)?;
    let mut metas = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut paths = BTreeMap::new();
    let prefix = format!("assets/library/{}", AssetId::new_random());
    let mut seen_ids = BTreeSet::new();
    for asset in &manifest.assets {
        let bytes = std::fs::read(no_links(&package, Path::new(&asset.path))?).map_err(io)?;
        if bytes.len() as u64 != asset.bytes
            || package_hash(&bytes) != asset.hash
            || paths.contains_key(&asset.path)
        {
            return Err(error("library_package", "Damaged or duplicate packaged asset"));
        }
        paths.insert(
            asset.path.clone(),
            format!("{prefix}/{}", asset.path.trim_start_matches("assets/")),
        );
        if let Some(mut meta) = meta_read(&package, &asset.path)? {
            if asset.id.as_deref() != Some(&meta.id.to_string()) {
                return Err(error("library_package", "Sidecar ID differs from the manifest"));
            }
            let parent = AssetId::new_random();
            for (old, new) in std::iter::once((meta.id, parent)).chain(
                meta.sub_assets
                    .iter()
                    .map(|s| (s.id, AssetId::sub_asset(parent, &s.path))),
            ) {
                if old.is_nil() || !seen_ids.insert(old) {
                    return Err(error("library_package", "Nil or duplicate asset ID in package"));
                }
                ids.insert(old.to_string(), new.to_string());
                ids.insert(format!("number:{}", old.0.as_u128()), new.0.as_u128().to_string());
            }
            meta.id = parent;
            for sub in &mut meta.sub_assets {
                sub.id = AssetId::sub_asset(parent, &sub.path);
            }
            metas.insert(asset.path.clone(), meta);
        } else if asset.id.is_some() {
            return Err(error("library_package", "Packaged sidecar is missing"));
        }
    }
    if manifest.roots.is_empty() || manifest.roots.iter().any(|root| !paths.contains_key(root)) {
        return Err(error("library_package", "The package roots are not present"));
    }
    for asset in &manifest.assets {
        if is_gltf(&asset.path)
            && gltf_dependencies(&package, &asset.path)?
                .iter()
                .any(|p| !paths.contains_key(p))
        {
            return Err(error("library_package", "The package omits a glTF dependency"));
        }
    }
    let stage = Stage::new(dir)?;
    for asset in &mut manifest.assets {
        let old_path = asset.path.clone();
        let mut bytes = std::fs::read(no_links(&package, Path::new(&old_path))?).map_err(io)?;
        if bytes.len() as u64 != asset.bytes || package_hash(&bytes) != asset.hash {
            return Err(error(
                "library_package",
                "Packaged source changed during the copy",
            ));
        }
        if old_path.ends_with(".ron") {
            let text = std::str::from_utf8(&bytes).map_err(io)?;
            bytes = remap_text(text, &paths, &ids)?.into_bytes();
        }
        asset.path = paths[&old_path].clone();
        asset.bytes = bytes.len() as u64;
        asset.hash = package_hash(&bytes);
        write(&stage.0.join(&asset.path), &bytes)?;
        if let Some(meta) = metas.get(&old_path) {
            asset.id = Some(meta.id.to_string());
            write(
                &meta_path_for(&stage.0.join(&asset.path)),
                meta.to_ron_string().map_err(io)?.as_bytes(),
            )?;
        }
    }
    manifest.roots = manifest.roots.iter().map(|path| paths[path].clone()).collect();
    manifest_write(&stage.0, &manifest)?;
    let destination = no_links(root, Path::new(&prefix))?;
    let parent = destination
        .parent()
        .ok_or_else(|| error("library_path", "Missing destination parent"))?;
    std::fs::create_dir_all(parent).map_err(io)?;
    std::fs::create_dir(&destination).map_err(io)?;
    let imported = asset_package::import_package(root, &stage.0, false).map_err(package_error);
    match imported {
        Ok(report) if report.clashes.is_empty() && report.damaged.is_empty() => {
            Ok(json!({"roots": manifest.roots, "paths": paths, "imported": report.imported}))
        }
        result => {
            let _ = std::fs::remove_dir_all(destination);
            match result {
                Err(e) => Err(e),
                Ok(report) => Err(error("library_package", report.summary())),
            }
        }
    }
}

fn required<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("library_params", format!("{key} must be a non-empty string")))
}

#[derive(Resource, Default)]
struct LibraryWorkers(Vec<std::thread::JoinHandle<String>>);

impl Drop for LibraryWorkers {
    fn drop(&mut self) {
        // Closing the edit world waits for its copies, so no worker writes after shutdown.
        for worker in self.0.drain(..) {
            if worker.join().is_err() {
                tracing::error!(target: "regia_editor", "library worker stopped during shutdown");
            }
        }
    }
}

#[derive(Resource)]
struct LibraryNotice(String);

fn reap_copies(world: &mut World, wait: bool) {
    let Some(mut workers) = world.remove_resource::<LibraryWorkers>() else {
        return;
    };
    let mut pending = Vec::new();
    for worker in workers.0.drain(..) {
        if wait || worker.is_finished() {
            let message = worker
                .join()
                .unwrap_or_else(|_| "Library worker stopped unexpectedly".into());
            world.insert_resource(LibraryNotice(message));
            world.remove_resource::<LibraryView>();
            crate::content_index::invalidate(world);
        } else {
            pending.push(worker);
        }
    }
    world.insert_resource(LibraryWorkers(pending));
}

/// Waits for this world's library copies and applies their cache invalidations.
/// Controlled headless sessions can use this synchronization point before inspecting files or
/// closing a project. Interactive panels only poll completed workers and never wait.
pub fn finish_copies(world: &mut World) {
    reap_copies(world, true);
}

fn asset_library(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    reap_copies(world, false);
    let action = required(params, "action")?;
    if !matches!(action, "list" | "add" | "use") {
        return Err(error("library_params", "action is list, add or use"));
    }
    let dir = directory(world)?;
    if action == "list" {
        return list(&dir);
    }
    if world
        .get_resource::<LibraryWorkers>()
        .is_some_and(|workers| !workers.0.is_empty())
    {
        return Err(error(
            "library_busy",
            "A library copy is still running; poll agent.job_status",
        ));
    }
    let root = project(world)?;
    let adding = action == "add";
    let value = required(params, if adding { "path" } else { "entry" })?.to_string();
    if adding {
        if !regia_asset_pipeline::asset_package::is_package_path(&value) {
            return Err(error("library_path", "path must name an asset under assets/"));
        }
    } else {
        entry_id(&value)?;
    }
    let (job, handle) = world
        .get_resource_or_insert_with::<regia_agent::jobs::JobRegistry>(Default::default)
        .start("library", format!("{action} {value}"), false);
    let unstarted = handle.clone();
    let worker = std::thread::Builder::new()
        .name("regia-asset-library".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(|| {
                if adding {
                    add(&root, &dir, &value)
                } else {
                    use_entry(&root, &dir, &value)
                }
            })
            .unwrap_or_else(|_| Err(error("library_io", "Library worker stopped unexpectedly")));
            match result {
                Ok(value) => {
                    let message = if adding {
                        format!("Added {} asset(s) to the library", value["assets"])
                    } else {
                        format!(
                            "Copied {} asset(s) into the project",
                            value["imported"].as_array().map_or(0, Vec::len)
                        )
                    };
                    handle.finish(value);
                    message
                }
                Err(e) => {
                    let code = e
                        .data
                        .as_ref()
                        .and_then(|data| data["code"].as_str())
                        .unwrap_or("library_io");
                    handle.fail(code, e.message.clone());
                    e.message
                }
            }
        });
    match worker {
        Ok(worker) => world
            .get_resource_or_insert_with(LibraryWorkers::default)
            .0
            .push(worker),
        Err(e) => {
            unstarted.fail("library_io", e.to_string());
            return Err(io(e));
        }
    }
    Ok(json!({"job": job}))
}

/// The user asset library agent method; the runtime schema is shared with the editor.
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, Handler)> {
    vec![(
        regia_agent::schema::ASSET_LIBRARY_METHOD,
        asset_library as Handler,
    )]
}

#[derive(Resource, Default)]
struct LibraryView {
    dir: Option<PathBuf>,
    listing: Option<std::sync::Arc<Value>>,
    error: Option<String>,
}

#[derive(Resource, Default)]
struct ClickFrame(Option<u64>);

fn first_click(world: &mut World, frame: u64) -> bool {
    let mut stamp = world.get_resource_or_insert_with(ClickFrame::default);
    if stamp.0 == Some(frame) {
        return false;
    }
    stamp.0 = Some(frame);
    true
}

fn ui_action(ui: &egui::Ui, world: &mut World, state: &mut ContentBrowserState, params: Value) {
    if !first_click(world, ui.ctx().cumulative_frame_nr()) {
        return;
    }
    state.toast = Some(match asset_library(world, &params) {
        Ok(reply) if reply["job"].is_string() => format!("Library copy started: {}", reply["job"]),
        Ok(_) => {
            world.remove_resource::<LibraryView>();
            "Library refreshed".into()
        }
        Err(e) => e.message,
    });
}

/// Read-only Library root and copy actions in the Content Browser's folder pane.
/// Ordinary egui text buttons inherit the appearance scale, theme and keyboard navigation.
pub fn folder_root(ui: &mut egui::Ui, world: &mut World, state: &mut ContentBrowserState) {
    let _span = tracing::info_span!(target: "regia_editor", "asset_library_panel").entered();
    use crate::agent_parity::{LIBRARY_ADD, LIBRARY_LIST, LIBRARY_USE, action_button, action_enabled};
    reap_copies(world, false);
    if let Some(notice) = world.remove_resource::<LibraryNotice>() {
        state.toast = Some(notice.0);
    }
    egui::CollapsingHeader::new("Library")
        .id_salt("user-asset-library")
        .show(ui, |ui| {
            ui.weak("Read-only · copies with dependencies");
            if action_enabled(ui, state.selected.is_some(), &LIBRARY_ADD, None).clicked()
                && let Some(path) = state.selected.clone()
            {
                ui_action(ui, world, state, json!({"action": "add", "path": path}));
            }
            if action_button(ui, &LIBRARY_LIST).clicked() {
                ui_action(ui, world, state, json!({"action": "list"}));
            }
            let dir = directory(world);
            let mut view = world.remove_resource::<LibraryView>().unwrap_or_default();
            if view.dir.as_ref() != dir.as_ref().ok() || view.listing.is_none() && view.error.is_none() {
                view.dir = dir.as_ref().ok().cloned();
                match dir.and_then(|dir| list(&dir)) {
                    Ok(value) => {
                        view.listing = Some(std::sync::Arc::new(value));
                        view.error = None;
                    }
                    Err(e) => {
                        view.listing = None;
                        view.error = Some(e.message);
                    }
                }
            }
            if let Some(error) = &view.error {
                ui.colored_label(ui.visuals().warn_fg_color, error);
            }
            let listing = view.listing.clone().unwrap_or_default();
            world.insert_resource(view);
            for error in listing["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                ui.colored_label(ui.visuals().warn_fg_color, error);
            }
            for entry in listing["entries"].as_array().into_iter().flatten() {
                let Some(id) = entry["entry"].as_str() else {
                    continue;
                };
                ui.push_id(id, |ui| {
                    for root in entry["roots"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        ui.label(root);
                    }
                    if action_button(ui, &LIBRARY_USE).clicked() {
                        ui_action(ui, world, state, json!({"action": "use", "entry": id}));
                    }
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use regia_ecs::prelude::*;
    use serde_json::{Value, json};

    fn call(world: &mut World, params: Value) -> Value {
        let handler = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.asset_library")
            .expect("editor.asset_library must be registered")
            .1;
        let reply = handler(world, &params).expect("library operation");
        if let Some(job) = reply["job"].as_str() {
            super::finish_copies(world);
            let status = regia_agent::jobs::job_status(world, &json!({"job": job}), &mut |_, _, _| {
                unreachable!("in-process job")
            })
            .expect("job status");
            assert_eq!(status["state"], "done", "{status}");
            status["result"].clone()
        } else {
            reply
        }
    }

    #[test]
    fn an_asset_added_to_the_library_is_usable_in_another_project() {
        let temp = regia_test_utils::unique_temp_dir("user_asset_library");
        let a = temp.join("a");
        let b = temp.join("b");
        let config = temp.join("config");
        std::fs::create_dir_all(a.join("assets")).expect("a");
        std::fs::create_dir_all(b.join("assets")).expect("b");
        let id = regia_asset::AssetId::new_random();
        let meta = regia_asset::MetaSidecar::new("texture", id);
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode source texture");
        let texture_bytes = encoded.into_inner();
        regia_test_utils::write_atomic(&a.join("assets/paint.png"), &texture_bytes).expect("texture");
        regia_test_utils::write_atomic(
            &a.join("assets/paint.png.meta"),
            meta.to_ron_string().expect("meta").as_bytes(),
        )
        .expect("sidecar");
        let text = format!("(texture: \"assets/paint.png\", handle: \"asset:{id}\")");
        regia_test_utils::write_atomic(&a.join("assets/paint.material.ron"), text.as_bytes())
            .expect("material");
        let mut world = World::new();
        world.insert_resource(crate::editor_prefs::EditorConfigPaths::at(&config));
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(a.clone()),
        });
        let added = call(
            &mut world,
            json!({"action": "add", "path": "assets/paint.material.ron"}),
        );
        let entry = added["entry"].as_str().expect("entry");
        std::fs::remove_dir_all(&a).expect("source is no longer available");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(b.clone()),
        });
        let listed = call(&mut world, json!({"action": "list"}));
        assert_eq!(listed["entries"][0]["entry"], entry);
        let used = call(&mut world, json!({"action": "use", "entry": entry}));
        let root = used["roots"][0].as_str().expect("root");
        let copied = std::fs::read_to_string(b.join(root)).expect("material copied");
        let texture = used["paths"]["assets/paint.png"]
            .as_str()
            .expect("remapped texture");
        let new_id = crate::asset_package::asset_id(&b, texture).expect("new sidecar");
        assert_ne!(new_id, id.to_string());
        assert!(copied.contains(texture), "{copied}");
        assert!(copied.contains(&format!("asset:{new_id}")), "{copied}");
        assert!(!copied.contains(&id.to_string()));
        let copied_texture = std::fs::read(b.join(texture)).expect("dependency");
        assert_eq!(copied_texture, texture_bytes);
        let decoded = image::load_from_memory(&copied_texture)
            .expect("copied texture is usable")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (1, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [12, 34, 56, 255]);
        let again = call(&mut world, json!({"action": "use", "entry": entry}));
        assert_ne!(again["roots"], used["roots"], "copies never overwrite");
        std::fs::remove_dir_all(config).expect("library can be removed");
        assert!(b.join(root).is_file(), "project never links to the library");
        assert_eq!(
            std::fs::read(b.join(texture)).expect("dependency survives library removal"),
            texture_bytes
        );
        std::fs::remove_dir_all(temp).expect("cleanup");
    }

    #[test]
    fn library_controls_work_from_the_keyboard_at_each_scale() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        use egui::{Key, Modifiers};
        for scale in [1.0, 1.5, 2.0] {
            let temp = regia_test_utils::unique_temp_dir("library_keyboard");
            let root = temp.join("project");
            let config = temp.join("config");
            std::fs::create_dir_all(root.join("assets")).expect("assets");
            regia_test_utils::write_atomic(&root.join("assets/icon.png"), b"image").expect("asset");
            let mut world = World::new();
            world.insert_resource(crate::agent_ext::EditorProject {
                root: Some(root.clone()),
            });
            world.insert_resource(crate::editor_prefs::EditorConfigPaths::at(&config));
            let mut state = crate::content::ContentBrowserState {
                project_root: Some(root.clone()),
                selected: Some("assets/icon.png".into()),
                ..Default::default()
            };
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            let size = [720.0, 900.0];
            {
                let mut draw = |ui: &mut egui::Ui| super::folder_root(ui, &mut world, &mut state);
                frame(&ctx, size, vec![], &mut draw);
                tab_to(&ctx, size, "Library", false, 12, &mut draw);
                frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], &mut draw);
                tab_to(&ctx, size, "Add to library", false, 12, &mut draw);
                frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], &mut draw);
            }
            super::finish_copies(&mut world);
            {
                let mut draw = |ui: &mut egui::Ui| super::folder_root(ui, &mut world, &mut state);
                tab_to(&ctx, size, "Refresh library", false, 12, &mut draw);
                frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], &mut draw);
                tab_to(&ctx, size, "Use in project", false, 12, &mut draw);
                frame(&ctx, size, vec![key(Key::Space, Modifiers::NONE)], &mut draw);
            }
            super::finish_copies(&mut world);
            frame(&ctx, size, vec![], |ui| {
                super::folder_root(ui, &mut world, &mut state)
            });
            let cached = world
                .resource::<super::LibraryView>()
                .listing
                .clone()
                .expect("cached");
            for _ in 0..4 {
                frame(&ctx, size, vec![], |ui| {
                    super::folder_root(ui, &mut world, &mut state)
                });
            }
            assert!(std::sync::Arc::ptr_eq(
                &cached,
                world
                    .resource::<super::LibraryView>()
                    .listing
                    .as_ref()
                    .expect("cached")
            ));
            assert_eq!(
                call(&mut world, json!({"action": "list"}))["entries"]
                    .as_array()
                    .expect("entries")
                    .len(),
                1
            );
            assert_eq!(
                crate::asset_package::project_assets(&root).len(),
                2,
                "keyboard copied exactly once at {scale}"
            );
            assert!(
                state.toast.as_deref().is_some_and(|s| s.starts_with("Copied")),
                "{:?}",
                state.toast
            );
            assert!(super::first_click(&mut world, 10000));
            assert!(
                !super::first_click(&mut world, 10000),
                "second UI pass cannot copy again"
            );
            std::fs::remove_dir_all(temp).expect("cleanup");
        }
    }

    #[test]
    fn remapping_changes_whole_references_and_preserves_comments_and_other_values() {
        let paths = std::collections::BTreeMap::from([(
            "assets/rock.png".into(),
            "assets/library/copy/rock.png".into(),
        )]);
        let ids = std::collections::BTreeMap::from([
            ("number:42".into(), "99".into()),
            ("old-id".into(), "new-id".into()),
        ]);
        let input = r###"(
            path: "assets/rock.png", short: r#"rock.png"#, escaped: "assets/rock\u{2e}png",
            handle: "asset:old-id", id: "old-id", prefab: 42, nested: (prefab: 0x2a), number: 42,
            other: "notes assets/rock.png", longer: "rock.png.backup", field42: 42.0,
            sub: "assets/rock.png#layer/0",
            character: '"', unicode: "é", number_name: "42",
            // "assets/rock.png" and 42
            /* 42 /* "old-id" */ */
        )"###;
        let text = super::remap_text(input, &paths, &ids).expect("remap");
        assert!(text.contains("assets/library/copy/rock.png"));
        assert!(text.contains("\"library/copy/rock.png\""));
        assert!(text.contains("prefab: 99, nested: (prefab: 99), number: 42"));
        assert!(text.contains("field42: 42.0"));
        assert!(text.contains("number_name: \"42\""));
        assert!(text.contains("// \"assets/rock.png\" and 42"));
        assert!(text.contains("/* 42 /* \"old-id\" */ */"));
        assert!(text.contains("\"notes assets/rock.png\""));
        assert!(text.contains("\"rock.png.backup\""));
        assert!(text.contains("asset:new-id"));
        assert!(text.contains("assets/library/copy/rock.png#layer/0"));
        assert!(super::remap_text("/* é", &paths, &ids).is_err());
    }

    #[test]
    fn a_library_inside_the_project_is_refused_without_writing() {
        let root = regia_test_utils::unique_temp_dir("library_inside");
        std::fs::create_dir_all(&root).expect("root");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(crate::editor_prefs::EditorConfigPaths::at(root.join("config")));
        let err = super::asset_library(&mut world, &json!({"action": "list"})).expect_err("outside only");
        assert_eq!(err.data.expect("data")["code"], "library_path");
        assert!(!root.join("config").exists());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn source_and_destination_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = regia_test_utils::unique_temp_dir("library_symlinks");
        let root = temp.join("project");
        let library = temp.join("library");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::create_dir_all(temp.join("outside")).expect("outside");
        symlink(temp.join("outside"), root.join("assets/linked")).expect("link");
        regia_test_utils::write_atomic(&temp.join("outside/image.png"), b"outside").expect("outside");
        assert!(super::add(&root, &library, "assets/linked/image.png").is_err());
        assert!(!library.exists());
        regia_test_utils::write_atomic(&root.join("assets/image.png"), b"inside").expect("inside");
        let added = super::add(&root, &library, "assets/image.png").expect("add");
        symlink(temp.join("outside"), root.join("assets/library")).expect("destination link");
        assert!(super::use_entry(&root, &library, added["entry"].as_str().expect("id")).is_err());
        assert_eq!(
            std::fs::read_dir(temp.join("outside")).expect("outside").count(),
            1
        );
        std::fs::remove_dir_all(temp).expect("cleanup");
    }

    #[test]
    fn copied_material_instances_load_their_copied_parent() {
        let temp = regia_test_utils::unique_temp_dir("library_material");
        let source = temp.join("source");
        let dest = temp.join("dest");
        let library = temp.join("library");
        std::fs::create_dir_all(source.join("assets")).expect("source");
        std::fs::create_dir_all(dest.join("assets")).expect("dest");
        let graph = regia_matgraph::MaterialGraph::new("Ground");
        let instance = regia_matgraph::MaterialInstance::new("assets/ground.matgraph.ron");
        regia_test_utils::write_atomic(
            &source.join("assets/ground.matgraph.ron"),
            graph.to_ron().expect("graph").as_bytes(),
        )
        .expect("graph file");
        regia_test_utils::write_atomic(
            &source.join("assets/ground.matinst.ron"),
            instance.to_ron().expect("instance").as_bytes(),
        )
        .expect("instance file");
        let added = super::add(&source, &library, "assets/ground.matinst.ron").expect("add");
        assert_eq!(added["assets"], 2);
        std::fs::remove_dir_all(source).expect("source gone");
        let used = super::use_entry(&dest, &library, added["entry"].as_str().expect("entry")).expect("use");
        let text =
            std::fs::read_to_string(dest.join(used["roots"][0].as_str().expect("root"))).expect("instance");
        let copied = regia_matgraph::MaterialInstance::from_ron(&text).expect("valid instance");
        let parent = std::fs::read_to_string(dest.join(&copied.parent)).expect("copied parent resolves");
        let parent = regia_matgraph::MaterialGraph::from_ron(&parent).expect("valid graph");
        copied.validate(&parent).expect("usable instance");
        std::fs::remove_dir_all(temp).expect("cleanup");
    }

    #[test]
    fn sub_asset_ids_and_gltf_buffers_survive_a_copy_and_damage_copies_nothing() {
        use regia_asset::{AssetId, MetaSidecar, SubAssetEntry};
        let temp = regia_test_utils::unique_temp_dir("library_sub_assets");
        let source = temp.join("source");
        let dest = temp.join("dest");
        let library = temp.join("library");
        std::fs::create_dir_all(source.join("assets/models")).expect("source");
        std::fs::create_dir_all(dest.join("assets")).expect("dest");
        regia_test_utils::write_atomic(
            &source.join("assets/models/body.gltf"),
            br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"body%20mesh.bin","byteLength":4}]}"#,
        )
        .expect("gltf");
        regia_test_utils::write_atomic(&source.join("assets/models/body mesh.bin"), &[1, 2, 3, 4])
            .expect("buffer");
        let parent = AssetId::new_random();
        let sub = AssetId::sub_asset(parent, "mesh/Body");
        let mut meta = MetaSidecar::new("gltf", parent);
        meta.sub_assets.push(SubAssetEntry {
            path: "mesh/Body".into(),
            id: sub,
            type_path: "Mesh".into(),
        });
        regia_test_utils::write_atomic(
            &source.join("assets/models/body.gltf.meta"),
            meta.to_ron_string().expect("meta").as_bytes(),
        )
        .expect("sidecar");
        let text = format!("(id: \"asset:{sub}\", prefab: {})", sub.0.as_u128());
        regia_test_utils::write_atomic(&source.join("assets/root.prefab.ron"), text.as_bytes())
            .expect("root");
        let added = super::add(&source, &library, "assets/root.prefab.ron").expect("add by sub-asset ID");
        assert_eq!(added["assets"], 3);
        let entry = added["entry"].as_str().expect("entry");
        let used = super::use_entry(&dest, &library, entry).expect("use");
        let model_path = used["paths"]["assets/models/body.gltf"].as_str().expect("model");
        let copied_meta = super::meta_read(&dest, model_path).expect("read").expect("meta");
        let copied_sub = copied_meta.sub_assets[0].id;
        assert_eq!(copied_sub, AssetId::sub_asset(copied_meta.id, "mesh/Body"));
        assert_ne!(copied_sub, sub);
        let root_text = std::fs::read_to_string(dest.join(used["roots"][0].as_str().expect("root")))
            .expect("copied root");
        assert!(root_text.contains(&format!("asset:{copied_sub}")));
        assert!(root_text.contains(&copied_sub.0.as_u128().to_string()));
        let model = dest.join(model_path);
        let gltf: Value = serde_json::from_slice(&std::fs::read(&model).expect("gltf")).expect("json");
        let buffer = model.parent().expect("parent").join("body mesh.bin");
        assert_eq!(gltf["buffers"][0]["uri"], "body%20mesh.bin");
        assert_eq!(std::fs::read(buffer).expect("local buffer"), [1, 2, 3, 4]);
        // Use never rewrites the library. Corruption is detected before any destination write.
        assert_eq!(
            std::fs::read_to_string(library.join(entry).join("assets/root.prefab.ron")).expect("original"),
            text
        );
        let before = crate::asset_package::project_assets(&dest);
        regia_test_utils::write_atomic(
            &library.join(entry).join("assets/models/body mesh.bin"),
            b"damaged",
        )
        .expect("damage");
        let err = super::use_entry(&dest, &library, entry).expect_err("damaged package refused");
        assert_eq!(err.data.expect("data")["code"], "library_package");
        assert_eq!(crate::asset_package::project_assets(&dest), before);
        assert!(std::fs::read_dir(&library).expect("entries").all(|e| {
            !e.expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".stage-")
        }));
        std::fs::remove_dir_all(temp).expect("cleanup");
    }

    #[test]
    fn gltf_dependencies_cannot_link_outside_the_project() {
        let temp = regia_test_utils::unique_temp_dir("library_external_uri");
        let root = temp.join("project");
        let library = temp.join("library");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        for uri in [
            "../../other-project/data.bin",
            "/outside/data.bin",
            "file:///outside/data.bin",
            "%2e%2e/outside.bin",
        ] {
            let source =
                json!({"asset": {"version": "2.0"}, "buffers": [{"uri": uri, "byteLength": 4}]}).to_string();
            regia_test_utils::write_atomic(&root.join("assets/model.gltf"), source.as_bytes())
                .expect("model");
            let err =
                super::add(&root, &library, "assets/model.gltf").expect_err("external reference refused");
            assert_eq!(err.data.expect("data")["code"], "library_path", "{uri}");
            assert!(!library.exists(), "nothing published");
        }
        std::fs::remove_dir_all(temp).expect("cleanup");
    }
}
