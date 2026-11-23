// SPDX-License-Identifier: MPL-2.0
//! Overlay asset roots (ADR-8155).
//!
//! Before a project cook, missing glTF buffer and image files under `assets/` are filled from
//! extra directories listed in `Regia.toml` `[cook.overlay_roots]` (or `REGIA_COOK_OVERLAY_ROOTS`,
//! which replaces that list). The earliest configured directory that exists and holds the
//! relative path supplies the bytes. A file already in the project is left alone. Nothing is
//! filled when no roots are configured, and no machine default directories are invented.
//! The live cooker fills the `.gltf` files it cooks with [`fill_gltf_documents`], and the runtime
//! searches the same roots in the same order (`regia_asset::overlay_roots`, ADR-8845).
//!
//! The filled path is the one the cook's sidecar collector already opens: percent-decoded, with
//! `\` read as `/`, and any URI that contains `:` skipped. The copy stays inside `assets/`.
//! A symlink whose target leaves its overlay root is not copied. Directory entries are sorted
//! so the summary order is stable; each filled file's bytes are the source bytes.
//!
//! The behavior is a requirements rewrite of a project-cook payload fill (see `docs/provenance.md`).
//! This module is original Rust.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::cook::join_bounded;
use crate::cook_ledger::write_atomic;
use crate::gltf_import::decode_gltf_uri;

pub use regia_asset::overlay_roots::{OVERLAY_ROOTS_ENV, OverlayRoots};

/// File the cook writes beside the cook ledger when overlay roots are configured.
pub const SUMMARY_FILE: &str = "overlay_fill.json";

/// How deep the asset walk goes. A stack bound, not a project setting.
const MAX_WALK_DEPTH: u32 = 64;

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// One file copied into `assets/`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilledPayload {
    /// Path relative to `assets/`, with `/` separators.
    pub relative_path: String,
    /// Index into the existing overlay directories. The earliest is 0.
    pub root_index: u32,
    /// Bytes copied.
    pub bytes: u64,
}

/// What one overlay fill did. Written as `overlay_fill.json` when the feature is on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayFillSummary {
    /// True when the table or a non-empty environment list was set.
    pub configured: bool,
    /// Existing directories used, earliest first, as displayed paths.
    pub roots: Vec<String>,
    /// Configured paths that were not directories.
    pub missing: Vec<String>,
    /// Files created under `assets/`.
    pub files: u64,
    /// Sum of [`Self::filled`] byte counts.
    pub bytes: u64,
    /// One row per created file, in sorted walk order.
    pub filled: Vec<FilledPayload>,
    /// Problems recorded for one URI. The rest of the fill still runs.
    pub errors: Vec<String>,
}

/// Fills `assets_root` from `roots` and records the summary the cook keeps.
///
/// # Errors
/// As [`OverlayRoots::load`]. The fill itself records IO problems on the summary.
pub fn fill_for_project(project: &Path, assets_root: &Path) -> Result<OverlayFillSummary, String> {
    let roots = OverlayRoots::load(project)?;
    let mut summary = if roots.configured {
        fill_missing_gltf_payloads(assets_root, &roots.paths)
    } else {
        OverlayFillSummary::default()
    };
    summary.configured = roots.configured;
    summary.roots = roots
        .paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    summary.missing = roots.missing;
    Ok(summary)
}

/// Copies missing glTF buffer and image files into `assets_root` from `roots`, earliest first.
///
/// An empty `roots`, or an `assets_root` that is not a directory, does no work and does not
/// walk the tree. A URI the cook would not open, a file already present, and a path that would
/// leave `assets_root` are skipped. The earliest root that has the relative path supplies it.
#[instrument(skip_all, fields(assets = %assets_root.display(), roots = roots.len()))]
pub fn fill_missing_gltf_payloads(assets_root: &Path, roots: &[PathBuf]) -> OverlayFillSummary {
    let mut summary = OverlayFillSummary::default();
    if roots.is_empty() || !assets_root.is_dir() {
        return summary;
    }
    let mut gltfs = Vec::new();
    collect_gltf(assets_root, 0, &mut gltfs);
    for gltf in &gltfs {
        fill_document(&mut summary, assets_root, roots, gltf);
    }
    summary
}

/// [`fill_missing_gltf_payloads`] for the given `.gltf` documents only (the live cooker's
/// targets, ADR-8845): the same lookup, order and refusals, without walking `assets_root`.
/// Paths that are not `.gltf` files under `assets_root` are skipped.
#[instrument(skip_all, fields(assets = %assets_root.display(), roots = roots.len(), docs = gltfs.len()))]
pub fn fill_gltf_documents(assets_root: &Path, roots: &[PathBuf], gltfs: &[PathBuf]) -> OverlayFillSummary {
    let mut summary = OverlayFillSummary::default();
    if roots.is_empty() || !assets_root.is_dir() {
        return summary;
    }
    for gltf in gltfs {
        if is_gltf(gltf) && gltf.starts_with(assets_root) && gltf.is_file() {
            fill_document(&mut summary, assets_root, roots, gltf);
        }
    }
    summary
}

/// Writes `overlay_fill.json` when the fill was configured, and removes a previous summary
/// when it was not, so a later read does not report a fill the last cook did not do.
///
/// # Errors
/// The directory cannot be updated.
pub fn sync_summary(cache_dir: &Path, summary: &OverlayFillSummary) -> io::Result<()> {
    let path = cache_dir.join(SUMMARY_FILE);
    if !summary.configured {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    write_summary(cache_dir, summary)
}

/// Writes [`SUMMARY_FILE`] as pretty JSON plus a trailing newline.
///
/// # Errors
/// The file cannot be written.
pub fn write_summary(cache_dir: &Path, summary: &OverlayFillSummary) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(summary).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(&cache_dir.join(SUMMARY_FILE), &bytes)
}

fn fill_document(summary: &mut OverlayFillSummary, assets_root: &Path, roots: &[PathBuf], gltf: &Path) {
    let Some(dir) = gltf.parent() else {
        return;
    };
    let Ok(bytes) = fs::read(gltf) else {
        return;
    };
    let Some(uris) = payload_uris(&bytes) else {
        return;
    };
    for uri in uris {
        fill_uri(summary, assets_root, roots, dir, &uri);
    }
}

/// Buffer URIs first, then image URIs. `None` when the document is not JSON (that file is skipped).
fn payload_uris(bytes: &[u8]) -> Option<Vec<String>> {
    let document: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut uris = Vec::new();
    for key in ["buffers", "images"] {
        match document.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    if let Some(uri) = item.get("uri").and_then(serde_json::Value::as_str) {
                        uris.push(uri.to_string());
                    }
                }
            }
            Some(_) => {}
        }
    }
    Some(uris)
}

fn fill_uri(
    summary: &mut OverlayFillSummary,
    assets_root: &Path,
    roots: &[PathBuf],
    gltf_dir: &Path,
    uri: &str,
) {
    if uri_closed(uri) {
        return;
    }
    let Some(decoded) = decode_gltf_uri(uri) else {
        return;
    };
    if decoded.is_empty() || uri_closed(&decoded) {
        return;
    }
    let Ok(dest) = join_bounded(gltf_dir, &decoded, Some(assets_root)) else {
        return;
    };
    let Some(key) = lookup_key(assets_root, &dest) else {
        return;
    };
    if fs::symlink_metadata(&dest).is_ok() {
        return;
    }
    if regia_core::project_path::check_under_root(assets_root, &dest).is_err() {
        return;
    }
    copy_from_roots(summary, roots, &dest, &key);
}

/// URIs the cook's sidecar collector does not open, plus an empty string.
fn uri_closed(uri: &str) -> bool {
    uri.is_empty() || uri.starts_with('/') || uri.starts_with("data:") || uri.contains(':')
}

fn copy_from_roots(summary: &mut OverlayFillSummary, roots: &[PathBuf], dest: &Path, key: &str) {
    for (index, root) in roots.iter().enumerate() {
        let candidate = root.join(key);
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if regia_core::project_path::check_under_root(root, &candidate).is_err() {
            continue;
        }
        let symlink = meta.file_type().is_symlink();
        let file = if symlink {
            fs::metadata(&candidate).is_ok_and(|followed| followed.is_file())
        } else {
            meta.is_file()
        };
        if !file {
            continue;
        }
        match publish(&candidate, dest, symlink) {
            Ok(Publish::Created(bytes)) => {
                summary.files += 1;
                summary.bytes += bytes;
                summary.filled.push(FilledPayload {
                    relative_path: key.to_string(),
                    root_index: u32::try_from(index).unwrap_or(u32::MAX),
                    bytes,
                });
            }
            Ok(Publish::Already) => {}
            Err(error) => summary.errors.push(format!("{key}: {error}")),
        }
        return;
    }
}

enum Publish {
    Created(u64),
    Already,
}

/// Hard-links a regular file into `dest`. A symlink is copied as bytes so the project does not
/// gain the link. `Already` means `dest` appeared while this call ran.
fn publish(src: &Path, dest: &Path, src_is_symlink: bool) -> io::Result<Publish> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        return Ok(Publish::Already);
    }
    if !src_is_symlink {
        match fs::hard_link(src, dest) {
            Ok(()) => {
                let bytes = fs::symlink_metadata(dest).map(|meta| meta.len()).unwrap_or(0);
                return Ok(Publish::Created(bytes));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(Publish::Already),
            Err(_) => {}
        }
    }
    copy_into_place(src, dest)
}

fn copy_into_place(src: &Path, dest: &Path) -> io::Result<Publish> {
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dest.with_file_name(format!(".{}-{n}.overlay-tmp", std::process::id()));
    if let Err(error) = fs::copy(src, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    let bytes = fs::symlink_metadata(&tmp).map(|meta| meta.len()).unwrap_or(0);
    match fs::hard_link(&tmp, dest) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(Publish::Created(bytes))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            Ok(Publish::Already)
        }
        Err(_) => create_new_from_tmp(&tmp, dest),
    }
}

fn create_new_from_tmp(tmp: &Path, dest: &Path) -> io::Result<Publish> {
    let copied = fs::read(tmp);
    let _ = fs::remove_file(tmp);
    let copied = copied?;
    let bytes = u64::try_from(copied.len()).unwrap_or(u64::MAX);
    match fs::OpenOptions::new().write(true).create_new(true).open(dest) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(&copied) {
                drop(file);
                let _ = fs::remove_file(dest);
                return Err(error);
            }
            Ok(Publish::Created(bytes))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Publish::Already),
        Err(error) => Err(error),
    }
}

fn collect_gltf(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            if fs::metadata(&path).is_ok_and(|meta| meta.is_file()) && is_gltf(&path) {
                out.push(path);
            }
            continue;
        }
        if kind.is_dir() {
            collect_gltf(&path, depth + 1, out);
        } else if kind.is_file() && is_gltf(&path) {
            out.push(path);
        }
    }
}

fn is_gltf(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gltf"))
}

/// `path` with `.` and `..` folded by text. A private copy of the cook's cleaner: that function
/// is private to `cook` and this walk only needs the folded text.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The overlay lookup key: the lexically cleaned path of `dest` relative to `assets_root`,
/// with `/` separators. `None` when `dest` is not inside that root.
fn lookup_key(assets_root: &Path, dest: &Path) -> Option<String> {
    let dest_clean = lexical_clean(dest);
    let root_clean = lexical_clean(assets_root);
    let relative = dest_clean.strip_prefix(&root_clean).ok()?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    let mut key = String::new();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(&segment.to_string_lossy());
    }
    if key.is_empty() { None } else { Some(key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> regia_test_utils::TempDir {
        regia_test_utils::TempDir::new(tag)
    }

    fn put(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        regia_test_utils::write_atomic(path, bytes).expect("write");
    }

    fn gltf(root: &Path, relative: &str, json: &str) {
        put(&root.join("assets").join(relative), json.as_bytes());
    }

    #[test]
    fn fill_gltf_documents_fills_only_the_named_documents() {
        let root = scratch("overlay-named-docs");
        gltf(&root, "a/one.gltf", r#"{"buffers":[{"uri":"one.bin"}]}"#);
        gltf(&root, "b/two.gltf", r#"{"buffers":[{"uri":"two.bin"}]}"#);
        let overlay = root.join("overlay");
        put(&overlay.join("a/one.bin"), b"one");
        put(&overlay.join("b/two.bin"), b"two");
        put(
            &root.join("outside.gltf"),
            br#"{"buffers":[{"uri":"a/one.bin"}]}"#,
        );
        let assets = root.join("assets");
        let summary = fill_gltf_documents(
            &assets,
            std::slice::from_ref(&overlay),
            &[
                assets.join("a/one.gltf"),
                assets.join("a/one.bin"),
                root.join("outside.gltf"),
                assets.join("missing.gltf"),
            ],
        );
        assert_eq!(summary.files, 1);
        assert_eq!(summary.filled[0].relative_path, "a/one.bin");
        assert_eq!(fs::read(assets.join("a/one.bin")).expect("filled"), b"one");
        assert!(
            !assets.join("b/two.bin").exists(),
            "a document not named is not filled"
        );
        let none = fill_gltf_documents(&assets, &[], &[assets.join("b/two.gltf")]);
        assert_eq!(none, OverlayFillSummary::default());
        assert!(!assets.join("b/two.bin").exists());
    }

    #[test]
    fn missing_buffer_filled_from_second_root() {
        let root = scratch("overlay-second");
        gltf(&root, "props/crate.gltf", r#"{"buffers":[{"uri":"crate.bin"}]}"#);
        let first = root.join("root0");
        let second = root.join("root1");
        put(&first.join("props/other.bin"), b"decoy");
        let payload = b"from-the-second-root\n";
        put(&second.join("props/crate.bin"), payload);
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[first, second]);
        let dest = root.join("assets/props/crate.bin");
        assert_eq!(fs::read(&dest).expect("filled"), payload);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.bytes, payload.len() as u64);
        assert_eq!(summary.filled.len(), 1);
        assert_eq!(summary.filled[0].root_index, 1);
        assert_eq!(summary.filled[0].relative_path, "props/crate.bin");
        assert!(summary.errors.is_empty());
    }

    #[test]
    fn a_present_file_or_directory_is_not_replaced() {
        let root = scratch("overlay-present");
        gltf(
            &root,
            "props/crate.gltf",
            r#"{"buffers":[{"uri":"crate.bin"},{"uri":"dir.bin"}]}"#,
        );
        put(&root.join("assets/props/crate.bin"), b"project");
        fs::create_dir_all(root.join("assets/props/dir.bin")).expect("dir node");
        let overlay = root.join("overlay");
        put(&overlay.join("props/crate.bin"), b"other");
        put(&overlay.join("props/dir.bin"), b"other-dir");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(
            fs::read(root.join("assets/props/crate.bin")).expect("kept"),
            b"project"
        );
        assert!(root.join("assets/props/dir.bin").is_dir());
        assert_eq!(summary.files, 0);
    }

    #[test]
    fn the_earliest_root_supplies_the_buffer() {
        let root = scratch("overlay-earliest");
        gltf(&root, "props/crate.gltf", r#"{"buffers":[{"uri":"crate.bin"}]}"#);
        let first = root.join("root0");
        let second = root.join("root1");
        put(&first.join("props/crate.bin"), b"earliest");
        put(&second.join("props/crate.bin"), b"later");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[first, second]);
        assert_eq!(
            fs::read(root.join("assets/props/crate.bin")).expect("filled"),
            b"earliest"
        );
        assert_eq!(summary.filled[0].root_index, 0);
        assert_eq!(summary.bytes, 8);
    }

    #[test]
    fn images_fill_after_buffers_and_non_arrays_are_skipped() {
        let root = scratch("overlay-images");
        gltf(
            &root,
            "props/crate.gltf",
            r#"{"buffers":{"uri":"nope.bin"},"images":[null,{"uri":"pic.png"}],"x":1}"#,
        );
        let overlay = root.join("overlay");
        put(&overlay.join("props/nope.bin"), b"no");
        put(&overlay.join("props/pic.png"), b"png");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(
            fs::read(root.join("assets/props/pic.png")).expect("image"),
            b"png"
        );
        assert!(!root.join("assets/props/nope.bin").exists());
        assert_eq!(summary.filled.len(), 1);
        assert_eq!(summary.filled[0].relative_path, "props/pic.png");
    }

    #[test]
    fn closed_uris_are_skipped_and_a_sibling_still_fills() {
        let root = scratch("overlay-closed");
        gltf(
            &root,
            "props/crate.gltf",
            r#"{"buffers":[
                {"uri":"data:application/octet-stream,AA"},
                {"uri":"http://example.test/x.bin"},
                {"uri":"/abs.bin"},
                {"uri":"foo:bar.bin"},
                {"uri":"ok.bin"}
            ]}"#,
        );
        let overlay = root.join("overlay");
        for name in [
            "props/x.bin",
            "props/abs.bin",
            "props/foo:bar.bin",
            "props/ok.bin",
            "abs.bin",
        ] {
            put(&overlay.join(name), b"x");
        }
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(fs::read(root.join("assets/props/ok.bin")).expect("sibling"), b"x");
        assert_eq!(summary.files, 1);
        assert!(!root.join("assets/props/x.bin").exists());
        assert!(!root.join("assets/abs.bin").exists());
    }

    #[test]
    fn a_uri_that_leaves_assets_is_not_written_and_one_that_returns_is() {
        let root = scratch("overlay-escape");
        gltf(
            &root,
            "props/a.gltf",
            r#"{"buffers":[{"uri":"../../outside.bin"},{"uri":"../inside.bin"},{"uri":"%2e%2e/%2e%2e/encoded.bin"}]}"#,
        );
        let overlay = root.join("overlay");
        put(&overlay.join("outside.bin"), b"out");
        put(&overlay.join("inside.bin"), b"in");
        put(&overlay.join("encoded.bin"), b"enc");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(fs::read(root.join("assets/inside.bin")).expect("inside"), b"in");
        assert!(!root.join("outside.bin").exists());
        assert!(!root.join("assets/../outside.bin").exists());
        assert!(!root.join("encoded.bin").exists());
        assert_eq!(summary.files, 1);
        assert_eq!(summary.filled[0].relative_path, "inside.bin");
    }

    #[test]
    fn backslash_and_percent_encoded_uris_use_the_decoded_relative_path() {
        let root = scratch("overlay-decode");
        gltf(
            &root,
            "props/a.gltf",
            r#"{"buffers":[{"uri":"sub\\buf.bin"},{"uri":"my%20file.bin"}]}"#,
        );
        let overlay = root.join("overlay");
        put(&overlay.join("props/sub/buf.bin"), b"sub");
        put(&overlay.join("props/my file.bin"), b"space");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(
            fs::read(root.join("assets/props/sub/buf.bin")).expect("slash"),
            b"sub"
        );
        assert_eq!(
            fs::read(root.join("assets/props/my file.bin")).expect("space"),
            b"space"
        );
        let mut paths: Vec<_> = summary
            .filled
            .iter()
            .map(|row| row.relative_path.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, ["props/my file.bin", "props/sub/buf.bin"]);
    }

    #[test]
    fn invalid_json_is_skipped_and_a_sibling_still_fills() {
        let root = scratch("overlay-json");
        put(&root.join("assets/bad.gltf"), b"{not json");
        gltf(&root, "ok.gltf", r#"{"buffers":[{"uri":"ok.bin"}]}"#);
        let overlay = root.join("overlay");
        put(&overlay.join("ok.bin"), b"ok");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(fs::read(root.join("assets/ok.bin")).expect("ok"), b"ok");
        assert_eq!(summary.files, 1);
        assert!(summary.errors.is_empty());
    }

    #[test]
    fn null_buffers_still_fill_images() {
        let root = scratch("overlay-null");
        gltf(&root, "a.gltf", r#"{"buffers":null,"images":[{"uri":"a.png"}]}"#);
        let overlay = root.join("overlay");
        put(&overlay.join("a.png"), b"png");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(summary.files, 1);
        assert_eq!(fs::read(root.join("assets/a.png")).expect("png"), b"png");
    }

    #[test]
    fn the_table_the_environment_and_missing_directories() {
        let project = scratch("overlay-config");
        fs::create_dir_all(project.join("from-toml")).expect("toml root");
        fs::create_dir_all(project.join("from-env")).expect("env root");
        let text = "[cook.overlay_roots]\npaths = [\"from-toml\", \"missing-dir\"]\n";
        let from_file = OverlayRoots::resolve(&project, Some(text), None).expect("resolve");
        assert!(from_file.configured);
        assert_eq!(from_file.paths, vec![project.join("from-toml")]);
        assert_eq!(from_file.missing, vec!["missing-dir".to_string()]);

        let replaced = OverlayRoots::resolve(&project, Some(text), Some("from-env")).expect("env");
        assert_eq!(replaced.paths, vec![project.join("from-env")]);
        assert!(replaced.missing.is_empty());

        let blank = OverlayRoots::resolve(&project, Some(text), Some("   ")).expect("blank env");
        assert_eq!(blank, from_file);
        let absent = OverlayRoots::resolve(&project, None, None).expect("off");
        assert!(!absent.configured);
        assert!(absent.paths.is_empty());

        let unknown = OverlayRoots::resolve(&project, Some("[cook.overlay_roots]\nextra = 1\n"), None);
        assert!(unknown.expect_err("unknown").contains("unknown key"));
        let empty = OverlayRoots::resolve(&project, Some("[cook.overlay_roots]\npaths = [\"\"]\n"), None);
        assert!(empty.expect_err("empty").contains("non-empty"));
        let typed = OverlayRoots::resolve(&project, Some("[cook.overlay_roots]\npaths = [1]\n"), None);
        assert!(typed.expect_err("type").contains("non-empty"));
        let shape = OverlayRoots::resolve(&project, Some("[cook]\noverlay_roots = \"x\"\n"), None);
        assert!(shape.expect_err("table").contains("must be a table"));
        let array = OverlayRoots::resolve(&project, Some("[cook.overlay_roots]\npaths = \"x\"\n"), None);
        assert!(array.expect_err("array").contains("array of strings"));
        let broken = OverlayRoots::resolve(&project, Some("[\n"), None);
        assert!(broken.expect_err("parse").starts_with("Regia.toml:"));
    }

    #[test]
    fn an_unconfigured_project_does_not_walk_or_keep_a_summary() {
        let project = scratch("overlay-off");
        gltf(
            &project,
            "props/crate.gltf",
            r#"{"buffers":[{"uri":"crate.bin"}]}"#,
        );
        put(&project.join("overlay/props/crate.bin"), b"nope");
        let summary = fill_for_project(&project, &project.join("assets")).expect("load");
        assert!(!summary.configured);
        assert!(!project.join("assets/props/crate.bin").exists());
        let cache = project.join("cache");
        fs::create_dir_all(&cache).expect("cache");
        write_summary(&cache, &summary).expect("seed");
        assert!(cache.join(SUMMARY_FILE).is_file());
        sync_summary(&cache, &summary).expect("clear");
        assert!(!cache.join(SUMMARY_FILE).exists());
        sync_summary(&cache, &summary).expect("clear again");
    }

    #[test]
    fn case_insensitive_gltf_is_walked_and_glb_is_not() {
        let root = scratch("overlay-case");
        gltf(&root, "Model.GLTF", r#"{"buffers":[{"uri":"a.bin"}]}"#);
        put(&root.join("assets/pack.glb"), b"glb");
        let overlay = root.join("overlay");
        put(&overlay.join("a.bin"), b"A");
        put(&overlay.join("pack.bin"), b"P");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(fs::read(root.join("assets/a.bin")).expect("gltf"), b"A");
        assert!(!root.join("assets/pack.bin").exists());
        assert_eq!(summary.files, 1);
    }

    #[test]
    fn a_second_gltf_does_not_count_a_file_the_first_already_filled() {
        let root = scratch("overlay-once");
        gltf(&root, "b.gltf", r#"{"buffers":[{"uri":"same.bin"}]}"#);
        gltf(&root, "a.gltf", r#"{"buffers":[{"uri":"same.bin"}]}"#);
        let overlay = root.join("overlay");
        put(&overlay.join("same.bin"), b"once");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.bytes, 4);
        assert_eq!(fs::read(root.join("assets/same.bin")).expect("once"), b"once");
    }

    #[test]
    fn a_file_where_a_directory_is_needed_records_an_error() {
        let root = scratch("overlay-blocked");
        gltf(
            &root,
            "props/crate.gltf",
            r#"{"buffers":[{"uri":"blocked/buf.bin"}]}"#,
        );
        put(&root.join("assets/props/blocked"), b"not a directory");
        let overlay = root.join("overlay");
        put(&overlay.join("props/blocked/buf.bin"), b"bytes");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(summary.files, 0);
        assert_eq!(summary.errors.len(), 1);
        assert!(summary.errors[0].contains("props/blocked/buf.bin"));
        assert!(!root.join("assets/props/blocked/buf.bin").exists());
    }

    #[test]
    fn a_walk_deeper_than_the_bound_is_not_filled() {
        let root = scratch("overlay-depth");
        let mut deep = root.join("assets");
        for index in 0..65 {
            deep.push(format!("d{index}"));
        }
        put(&deep.join("hidden.gltf"), br#"{"buffers":[{"uri":"no.bin"}]}"#);
        let mut seen = root.join("assets");
        for index in 0..64 {
            seen.push(format!("d{index}"));
        }
        put(&seen.join("seen.gltf"), br#"{"buffers":[{"uri":"yes.bin"}]}"#);
        let overlay = root.join("overlay");
        put(&overlay.join("no.bin"), b"no");
        let mut yes = PathBuf::new();
        for index in 0..64 {
            yes.push(format!("d{index}"));
        }
        yes.push("yes.bin");
        put(&overlay.join(&yes), b"yes");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), &[overlay]);
        assert_eq!(summary.files, 1);
        assert_eq!(
            summary.filled[0].relative_path,
            yes.display().to_string().replace('\\', "/")
        );
        assert!(!deep.join("no.bin").exists());
    }

    #[test]
    fn the_summary_json_round_trips_with_a_trailing_newline() {
        let root = scratch("overlay-json-summary");
        let summary = OverlayFillSummary {
            configured: true,
            roots: vec!["/r0".to_string(), "/r1".to_string()],
            missing: vec!["gone".to_string()],
            files: 1,
            bytes: 4,
            filled: vec![FilledPayload {
                relative_path: "a.bin".to_string(),
                root_index: 1,
                bytes: 4,
            }],
            errors: vec!["a.bin: busy".to_string()],
        };
        fs::create_dir_all(root.path()).expect("root");
        write_summary(&root, &summary).expect("write");
        let bytes = fs::read(root.join(SUMMARY_FILE)).expect("read");
        assert_eq!(*bytes.last().expect("newline"), b'\n');
        let back: OverlayFillSummary = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(back, summary);
        let off = OverlayFillSummary::default();
        sync_summary(&root, &off).expect("remove");
        assert!(!root.join(SUMMARY_FILE).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_same_filesystem_file_is_hard_linked_and_an_in_root_symlink_is_copied_as_bytes() {
        use std::os::unix::fs::MetadataExt;
        let root = scratch("overlay-link");
        gltf(
            &root,
            "props/crate.gltf",
            r#"{"buffers":[{"uri":"crate.bin"},{"uri":"via.bin"}]}"#,
        );
        let overlay = root.join("overlay");
        put(&overlay.join("props/real.bin"), b"bytes");
        put(&overlay.join("props/crate.bin"), b"linked");
        std::os::unix::fs::symlink(overlay.join("props/real.bin"), overlay.join("props/via.bin"))
            .expect("symlink");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), std::slice::from_ref(&overlay));
        let dest = root.join("assets/props/crate.bin");
        assert_eq!(
            fs::metadata(&dest).expect("dest").ino(),
            fs::metadata(overlay.join("props/crate.bin")).expect("src").ino()
        );
        let via = root.join("assets/props/via.bin");
        assert_eq!(fs::read(&via).expect("copied"), b"bytes");
        assert!(!fs::symlink_metadata(&via).expect("meta").file_type().is_symlink());
        assert_eq!(summary.files, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_leaves_its_overlay_root_is_not_copied_and_directory_links_are_not_walked() {
        let root = scratch("overlay-escape-link");
        let outside = scratch("overlay-escape-outside");
        put(&outside.join("secret.bin"), b"secret");
        gltf(&root, "props/crate.gltf", r#"{"buffers":[{"uri":"crate.bin"}]}"#);
        let overlay = root.join("overlay");
        fs::create_dir_all(overlay.join("props")).expect("props");
        std::os::unix::fs::symlink(outside.join("secret.bin"), overlay.join("props/crate.bin"))
            .expect("out link");
        let linked = scratch("overlay-linked-tree");
        put(
            &linked.join("nested/only.gltf"),
            br#"{"buffers":[{"uri":"hidden.bin"}]}"#,
        );
        std::os::unix::fs::symlink(linked.path(), root.join("assets/linked")).expect("dir link");
        put(&overlay.join("linked/nested/hidden.bin"), b"hidden");
        let file_link_target = root.join("assets/props/crate.gltf");
        std::os::unix::fs::symlink(&file_link_target, root.join("assets/alias.gltf")).expect("file link");
        put(&overlay.join("crate.bin"), b"from-alias");
        let summary = fill_missing_gltf_payloads(&root.join("assets"), std::slice::from_ref(&overlay));
        assert!(
            !root.join("assets/props/crate.bin").exists(),
            "outbound symlink is not planted"
        );
        assert!(
            !linked.join("nested/hidden.bin").exists(),
            "a directory symlink is not walked"
        );
        assert_eq!(
            fs::read(root.join("assets/crate.bin")).expect("alias"),
            b"from-alias"
        );
        assert_eq!(summary.files, 1);
        assert_eq!(summary.filled[0].relative_path, "crate.bin");
    }

    #[test]
    fn cook_project_fills_before_import_and_a_bad_table_fails_the_cook() {
        let project = scratch("overlay-cook");
        let second = project.join("root1");
        let payload = b"from-the-second-root\n";
        put(&second.join("props/crate.bin"), payload);
        fs::create_dir_all(project.join("root0/props")).expect("root0");
        gltf(
            &project,
            "props/crate.gltf",
            r#"{"buffers":[{"uri":"crate.bin"}]}"#,
        );
        let manifest = format!(
            "[cook.overlay_roots]\npaths = [{:?}, {:?}]\n",
            project.join("root0").display().to_string(),
            second.display().to_string()
        );
        fs::write(project.join("Regia.toml"), manifest).expect("manifest");
        let options = crate::CookOptions::for_host(project.path().to_path_buf());
        let platform = options.platform.clone();
        let report = crate::cook_project(options).expect("cook");
        assert_eq!(
            fs::read(project.join("assets/props/crate.bin")).expect("cooked fill"),
            payload
        );
        assert_eq!(report.overlay_fill.files, 1);
        assert_eq!(report.overlay_fill.filled[0].root_index, 1);
        assert!(report.overlay_fill.configured);
        let cache = regia_asset::cooked_cache_dir(project.path(), &platform);
        let stored: OverlayFillSummary =
            serde_json::from_slice(&fs::read(cache.join(SUMMARY_FILE)).expect("summary")).expect("json");
        assert_eq!(stored.files, 1);
        assert_eq!(stored.filled[0].root_index, 1);

        fs::write(project.join("Regia.toml"), "[cook.overlay_roots]\nextra = 1\n").expect("bad");
        let _ = fs::remove_file(project.join("assets/props/crate.bin"));
        let again =
            crate::cook_project(crate::CookOptions::for_host(project.path().to_path_buf())).expect("cook");
        assert!(
            again
                .errors
                .iter()
                .any(|(path, error)| path == "Regia.toml" && error.contains("unknown key"))
        );
        assert_eq!(again.overlay_fill.files, 0);
        assert!(!project.join("assets/props/crate.bin").exists());
    }
}
