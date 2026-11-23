// SPDX-License-Identifier: MPL-2.0
//! Cooks extension content into the project's cooked cache (`specs/57` § 4.4).
//!
//! Only what the project references goes in, never a whole mount: this walks `ext://<id>/<rest>`
//! references from the project's text sources, follows them through the extension files they
//! reach, and cooks that closure. The file keeps its `ext://` path in the manifest, so the running
//! game finds it the way it finds a project asset (`AssetServer::id_for_path`). The project's own
//! `assets/<rest>` shadows an extension file (§ 3.3.7): the `ext://` path then names the project's
//! cooked asset. A file type no built-in importer takes is stored as it is, for the extension's
//! own loader.
//!
//! Sources are copied to `.regia/ext/<id>/` first, because a cook writes `.meta` files beside its
//! sources and extension directories are read-only as far as the engine is concerned (§ 3.3.7).
//! The whole root is staged, so a file's relative references resolve; cooking stays closure-only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use regia_asset::{AssetManifest, ManifestEntry, cooked_cache_dir};
use walkdir::WalkDir;

use crate::cook::{CookError, cook_one, read_manifest_map, record_outcome, remove_cached};
use crate::cook_inputs::CookInputs;
use crate::cook_ledger::CookLedger;
use crate::db::AssetDatabase;

/// The manifest prefix of extension content.
pub const EXTENSION_PREFIX: &str = "ext://";

/// One `[[content]]` root of an enabled extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRoot {
    /// The extension id.
    pub id: String,
    /// The directory on disk.
    pub root: PathBuf,
}

/// What [`cook_extension_content`] works on.
#[derive(Clone, Debug)]
pub struct ExtensionCookOptions {
    /// Project root (contains `assets/`).
    pub project_root: PathBuf,
    /// Platform tag.
    pub platform: String,
    /// When true, only recook changed sources.
    pub incremental: bool,
    /// The content roots of the enabled extensions.
    pub roots: Vec<ExtensionRoot>,
    /// `(manifest path, text)` of the cooked extension record, stored in the paks as it is.
    pub record: Option<(String, String)>,
}

/// What [`cook_extension_content`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionCookReport {
    /// `ext://` paths cooked or found current.
    pub cooked: Vec<String>,
    /// `ext://` paths that name a project asset instead (it shadows the extension's file).
    pub shadowed: Vec<String>,
    /// References no enabled extension provides.
    pub missing: Vec<String>,
    /// Failures by `ext://` path.
    pub errors: Vec<(String, String)>,
}

/// The `ext://` references in `bytes`.
///
/// A reference inside a quoted string (`"ext://…"` or `'ext://…'`) runs to the closing quote, so
/// names with spaces, `+`, `%` or non-ASCII letters are kept whole; an unquoted one runs to the
/// first whitespace or delimiter (`,` `;` `(` `)` `[` `]` `{` `}` `<` `>` `\`). A `#sub` part
/// names a sub-asset of the file and is not part of the path.
#[must_use]
pub fn extension_refs(bytes: &[u8]) -> Vec<String> {
    let mark = EXTENSION_PREFIX.as_bytes();
    let unquoted_end = |b: &u8| {
        b.is_ascii_whitespace()
            || matches!(
                *b,
                b'"' | b'\''
                    | b'`'
                    | b','
                    | b';'
                    | b'('
                    | b')'
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'<'
                    | b'>'
                    | b'\\'
            )
    };
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = bytes[at..].windows(mark.len()).position(|w| w == mark) {
        let start = at + found;
        let tail = start + mark.len();
        let quote = start
            .checked_sub(1)
            .map(|i| bytes[i])
            .filter(|b| matches!(*b, b'"' | b'\''));
        let len = match quote {
            Some(q) => bytes[tail..]
                .iter()
                .take_while(|b| **b != q && **b != b'\n' && **b != b'\r')
                .count(),
            None => bytes[tail..].iter().take_while(|b| !unquoted_end(b)).count(),
        };
        let path = String::from_utf8_lossy(&bytes[start..tail + len]);
        let path = path.split('#').next().unwrap_or_default();
        out.push(path.trim_end_matches(['.', '/']).to_owned());
        at = tail + len;
    }
    out
}

fn split_ref(path: &str) -> Option<(&str, &str)> {
    let (id, rest) = path.strip_prefix(EXTENSION_PREFIX)?.split_once('/')?;
    (!id.is_empty() && regia_core::project_path::check_relative(rest).is_ok()).then_some((id, rest))
}

/// Text sources that can name extension content.
fn is_reference_source(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "ron" | "toml" | "json" | "gltf" | "rs"
        )
    })
}

fn scan(path: &Path, queue: &mut VecDeque<String>) {
    if is_reference_source(path)
        && let Ok(bytes) = std::fs::read(path)
    {
        queue.extend(extension_refs(&bytes));
    }
}

/// Copies every root of one extension (in order; a later root's file replaces an earlier one's)
/// to `staged`, skipping files whose staged copy already holds the same bytes, and removes staged
/// files no root has any more. Only regular files: a symlink cannot lead a cook out of the
/// extension. Content files are compared by bytes, not size and mtime: an extension updated by
/// extracting a release archive keeps the packager's older timestamps (audit-v035-cook M3).
/// `.meta` sidecars keep the size-and-age rule and are never removed: the cook rewrites them in
/// the stage (sub-asset lists, a generated id).
fn stage_roots(roots: &[&Path], staged: &Path) -> Result<(), CookError> {
    let mut present = BTreeSet::new();
    for root in roots {
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            present.insert(rel.to_path_buf());
            let target = staged.join(rel);
            let source = entry.metadata().map_err(std::io::Error::from)?;
            if let Ok(current) = std::fs::metadata(&target)
                && current.is_file()
                && current.len() == source.len()
            {
                let current_enough = if is_meta(rel) {
                    matches!((current.modified(), source.modified()), (Ok(t), Ok(s)) if t >= s)
                } else {
                    same_bytes(entry.path(), &target)
                };
                if current_enough {
                    continue;
                }
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    // A file deleted from the extension must not keep resolving (as a glTF sidecar, say).
    if staged.is_dir() {
        for entry in WalkDir::new(staged).into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file()
                && let Ok(rel) = entry.path().strip_prefix(staged)
                && !is_meta(rel)
                && !present.contains(rel)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Whether `rel` is a `.meta` sidecar.
fn is_meta(rel: &Path) -> bool {
    rel.extension().is_some_and(|e| e.eq_ignore_ascii_case("meta"))
}

/// Whether two files hold the same bytes (read in blocks; any read error counts as different).
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(mut a), Ok(mut b)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut left = vec![0u8; 64 * 1024];
    let mut right = vec![0u8; 64 * 1024];
    loop {
        let Ok(n) = read_full(&mut a, &mut left) else {
            return false;
        };
        let Ok(m) = read_full(&mut b, &mut right) else {
            return false;
        };
        if n != m || left[..n] != right[..m] {
            return false;
        }
        if n == 0 {
            return true;
        }
    }
}

/// Reads until `buf` is full or the end of the file; returns the bytes read.
fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Cooks the extension content the project references, and the extension record, into the cooked
/// cache. Call it after `cook_project` and before the paks are written. Rows for content that is
/// no longer referenced leave the manifest, so a pak never carries it.
///
/// # Errors
///
/// [`CookError`] when the cache cannot be read or written; a file that fails to cook is listed in
/// [`ExtensionCookReport::errors`].
pub fn cook_extension_content(options: &ExtensionCookOptions) -> Result<ExtensionCookReport, CookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "extension_cook", roots = options.roots.len())
            .entered();
    let mut report = ExtensionCookReport::default();
    let project = &options.project_root;
    // The same lock `cook_project` and the live cooker take: this pass reads and rewrites the
    // shared database, ledger and manifest from a snapshot (audit F2).
    let _cook_lock = crate::db::CookLock::acquire(project);
    #[cfg(test)]
    LOCK_HELD_AT_ENTRY.with(|c| c.set(Some(crate::db::CookLock::is_held(project))));
    let cache_dir = cooked_cache_dir(project, &options.platform);
    let mut warnings = Vec::new();
    let mut manifest_map = read_manifest_map(&cache_dir, &mut warnings);
    let had_rows = manifest_map.keys().any(|k| k.starts_with(EXTENSION_PREFIX));
    if options.roots.is_empty() && options.record.is_none() && !had_rows {
        return Ok(report);
    }
    let mut db = AssetDatabase::open(project)?;
    let mut ledger = CookLedger::open(&cache_dir, Some(project), &db);
    let mut inputs = CookInputs::open(project, &options.platform);
    let assets_root = project.join("assets");
    let stage = regia_core::brand::project_state_dir(project).join("ext");
    let roots: BTreeMap<&str, Vec<&Path>> = options.roots.iter().fold(BTreeMap::new(), |mut map, r| {
        map.entry(r.id.as_str()).or_default().push(r.root.as_path());
        map
    });

    let mut queue = VecDeque::new();
    for source in AssetDatabase::discover_sources(&assets_root) {
        scan(&source, &mut queue);
    }
    for entry in WalkDir::new(project.join("src"))
        .into_iter()
        .filter_map(Result::ok)
    {
        scan(entry.path(), &mut queue);
    }

    let mut seen = BTreeSet::new();
    let mut keep = BTreeSet::new();
    let mut staged_ids = BTreeSet::new();
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some((id, rest)) = split_ref(&path) else {
            report.missing.push(path);
            continue;
        };
        // A path into an extension that is not enabled names nothing, shadowed or not.
        let Some(dirs) = roots.get(id) else {
            report.missing.push(path);
            continue;
        };
        let project_row = manifest_map.get(&format!("assets/{rest}")).cloned();
        if let Some(row) = project_row.filter(|_| assets_root.join(rest).is_file()) {
            manifest_map.insert(
                path.clone(),
                ManifestEntry {
                    path: path.clone(),
                    ..row
                },
            );
            keep.insert(path.clone());
            report.shadowed.push(path);
            continue;
        }
        let Some(root) = dirs.iter().find(|root| root.join(rest).is_file()) else {
            report.missing.push(path);
            continue;
        };
        let staged = stage.join(id);
        if staged_ids.insert(id.to_owned()) {
            stage_roots(dirs, &staged)?;
        }
        scan(&root.join(rest), &mut queue);
        let source = staged.join(rest);
        match cook_one(
            &source,
            &path,
            &cache_dir,
            &db,
            &ledger,
            options.incremental,
            &mut inputs,
        ) {
            Ok(outcome) => {
                record_outcome(&mut db, &mut ledger, &mut manifest_map, &path, outcome);
                keep.insert(path.clone());
                report.cooked.push(path);
            }
            Err(error) => {
                // A failed recook keeps the previous manifest row, database row, ledger key and
                // cooked file: the sweep below removes only what this run did not reference
                // (asset_pipeline3 correctness audit, finding 2).
                keep.insert(path.clone());
                report.errors.push((path, error.to_string()));
            }
        }
    }

    if let Some((path, text)) = &options.record {
        let source = stage.join("regia").join("extensions.ron");
        if let Some(parent) = source.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&source).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&source, text)?;
        }
        match cook_one(
            &source,
            path,
            &cache_dir,
            &db,
            &ledger,
            options.incremental,
            &mut inputs,
        ) {
            Ok(outcome) => {
                record_outcome(&mut db, &mut ledger, &mut manifest_map, path, outcome);
                keep.insert(path.clone());
            }
            Err(error) => {
                keep.insert(path.clone());
                report.errors.push((path.clone(), error.to_string()));
            }
        }
    }

    // A sub-asset row (`path#mesh0`) lives and dies with its file.
    let stale =
        |key: &str| key.starts_with(EXTENSION_PREFIX) && !keep.contains(key.split('#').next().unwrap_or(key));
    manifest_map.retain(|key, _| !stale(key));
    let gone: Vec<String> = db.entries.keys().filter(|k| stale(k)).cloned().collect();
    for path in gone {
        if let Some(entry) = db.entries.remove(&path) {
            remove_cached(&cache_dir, &entry.cooked_rel);
        }
        ledger.forget(&path);
    }

    ledger.save()?;
    db.save(project)?;
    AssetManifest::from_map(manifest_map)
        .write_to_cache(&cache_dir)
        .map_err(CookError::Manifest)?;
    // Derived data, as in `cook_project`: the next cook writes it again.
    if let Err(error) = crate::registry_build::write_cache_registry(project, &cache_dir, &db) {
        tracing::warn!(target: "regia_asset_pipeline", "asset registry not written: {error}");
    }
    if let Err(error) = inputs.save() {
        tracing::debug!(target: "regia_asset_pipeline", "cook inputs not saved: {error}");
    }
    Ok(report)
}

// What the last `cook_extension_content` call on this thread saw: was the cook lock held?
#[cfg(test)]
thread_local! {
    static LOCK_HELD_AT_ENTRY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit F2: the extension pass rewrites the shared database, so it holds the cook lock.
    #[test]
    fn extension_cook_holds_the_cook_lock() {
        let project = regia_test_utils::unique_temp_dir("ext-cook-lock");
        std::fs::create_dir_all(project.join("assets")).expect("assets");
        LOCK_HELD_AT_ENTRY.with(|c| c.set(None));
        let options = ExtensionCookOptions {
            project_root: project.clone(),
            platform: "linux".into(),
            incremental: true,
            roots: Vec::new(),
            record: None,
        };
        cook_extension_content(&options).expect("cooks");
        assert_eq!(LOCK_HELD_AT_ENTRY.with(std::cell::Cell::get), Some(true));
        assert!(!crate::db::CookLock::is_held(&project), "released on return");
    }

    #[test]
    fn references_end_at_the_first_byte_outside_a_path() {
        assert_eq!(
            extension_refs(b"a: \"ext://dlg/talk/one.dlg\", b: 'ext://x/y.png'. ext://z/w.txt."),
            ["ext://dlg/talk/one.dlg", "ext://x/y.png", "ext://z/w.txt"]
        );
        assert!(extension_refs(b"no references, not even ext:/").is_empty());
    }

    /// audit-v035-cook M3: a same-sized file replaced with older timestamps (an extension updated
    /// from a release archive) kept its stale staged copy forever, and files deleted from the
    /// extension stayed staged.
    #[test]
    fn staging_follows_the_bytes_not_the_timestamps() {
        let base = regia_test_utils::unique_temp_dir("ext-stage-bytes");
        let (root, staged) = (base.join("root"), base.join("staged"));
        std::fs::create_dir_all(root.join("tex")).expect("root");
        std::fs::write(root.join("tex/a.png"), b"AAAA").expect("a");
        std::fs::write(root.join("tex/gone.png"), b"GONE").expect("gone");
        stage_roots(&[root.as_path()], &staged).expect("first stage");
        assert_eq!(
            std::fs::read(staged.join("tex/a.png")).expect("staged a"),
            b"AAAA"
        );
        // Same size, different bytes, a timestamp older than the staged copy.
        std::fs::write(root.join("tex/a.png"), b"BBBB").expect("a again");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("tex/a.png"))
            .and_then(|f| f.set_modified(old))
            .expect("age the source");
        std::fs::remove_file(root.join("tex/gone.png")).expect("delete gone");
        stage_roots(&[root.as_path()], &staged).expect("second stage");
        assert_eq!(
            std::fs::read(staged.join("tex/a.png")).expect("staged a"),
            b"BBBB"
        );
        assert!(
            !staged.join("tex/gone.png").exists(),
            "a deleted file leaves the stage"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// audit-v035-cook L7: a quoted reference whose name has a non-ASCII letter, a space, `+` or
    /// `%` was cut at that byte (`ext://dlg/` → `ext://dlg`), so the real file never cooked.
    #[test]
    fn a_quoted_reference_keeps_every_byte_to_its_closing_quote() {
        assert_eq!(
            extension_refs("image: \"ext://dlg/über.png\", b: 'ext://dlg/my file+1%.png'".as_bytes()),
            ["ext://dlg/über.png", "ext://dlg/my file+1%.png"]
        );
        // A `#sub` part names a sub-asset of the file, not a path.
        assert_eq!(
            extension_refs(b"mesh: \"ext://kit/hero.glb#mesh/Body/primitive_0\""),
            ["ext://kit/hero.glb"]
        );
        // Unquoted references still stop at a delimiter.
        assert_eq!(
            extension_refs(b"(ext://kit/a.png) [ext://kit/b.png]"),
            ["ext://kit/a.png", "ext://kit/b.png"]
        );
    }

    #[test]
    fn a_reference_that_climbs_out_is_refused() {
        assert_eq!(split_ref("ext://a/b/c.png"), Some(("a", "b/c.png")));
        assert_eq!(split_ref("ext://a/../../etc/passwd"), None);
        assert_eq!(split_ref("ext:///b"), None);
        assert_eq!(split_ref("ext://a"), None);
    }
}
