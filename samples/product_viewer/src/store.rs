// SPDX-License-Identifier: MIT-0
//! Product packages on the device (`docs/specs/42-product-viewer.md` § 9): folders copied in with
//! `adb push` or downloaded once from a URL, checked before use, and available offline from then on.
//!
//! A package is never half installed: a download lands in a temporary folder beside the store, every
//! file is verified by `regia_viewer_pack`, and only then the folder is renamed into place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_viewer_pack::{MANIFEST_FILE, MAX_MANIFEST_BYTES, Manifest, PackError, Package};
use serde_json::Value;

/// Why a package cannot be stored or opened. `code` is lowercase ascii, digits and `_`:
/// `package_fetch_failed`, `package_store_failed`, `package_invalid_url` or one of `regia_viewer_pack`'s
/// `pack_*` codes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct StoreError {
    /// The protocol's error code (`book/agents/product-viewer.md`).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl StoreError {
    /// A new error.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn fetch(what: &str, detail: impl std::fmt::Display) -> Self {
        Self::new("package_fetch_failed", format!("{what}: {detail}"))
    }

    fn io(what: &str, detail: impl std::fmt::Display) -> Self {
        Self::new("package_store_failed", format!("{what}: {detail}"))
    }
}

impl From<PackError> for StoreError {
    fn from(err: PackError) -> Self {
        Self::new(err.code, err.message)
    }
}

/// The folder name of a package id (`<house>/<design slug>`): `/` becomes `-`, anything else outside
/// `a-z0-9._-` too. `None` when nothing usable is left.
#[must_use]
pub fn folder_name(package_id: &str) -> Option<String> {
    let name: String = package_id
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!name.is_empty() && name.len() <= 128 && !name.starts_with('.') && name.chars().any(|c| c != '-'))
        .then_some(name)
}

/// The id a manifest gives its package.
fn package_id(manifest: &Manifest) -> &str {
    manifest
        .doc
        .get("package_id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Where a package's files come from during an install.
pub trait PackageSource {
    /// The bytes of `path` (relative to the package folder), at most `limit` of them.
    fn get(&mut self, path: &str, limit: u64) -> Result<Vec<u8>, StoreError>;
}

/// A package the store knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    /// The folder name.
    pub slug: String,
    /// The folder.
    pub dir: PathBuf,
    /// The package id from the manifest; empty when the manifest is unusable.
    pub package_id: String,
    /// Why the package cannot be opened, if its manifest already says so. Files are checked at open.
    pub problem: Option<String>,
}

static INSTALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Package folders on this device. `roots` are read in order, the first holds new installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStore {
    roots: Vec<PathBuf>,
}

impl PackageStore {
    /// A store over `roots`; the first takes downloads. Folders are created when missing, so `adb push`
    /// has somewhere to copy to after the first launch.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        for root in &roots {
            if let Err(err) = std::fs::create_dir_all(root) {
                tracing::warn!(target: "product_viewer", "cannot create {}: {err}", root.display());
            }
        }
        let store = Self { roots };
        store.remove_leftovers();
        store
    }

    /// The folders searched.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Temporary folders of an install that a killed process left behind.
    fn remove_leftovers(&self) {
        let Some(root) = self.roots.first() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with(".install-")
                && let Err(err) = std::fs::remove_dir_all(&path)
            {
                tracing::warn!(target: "product_viewer", "cannot remove {}: {err}", path.display());
            }
        }
    }

    /// Every package present, sorted by folder name; the first root wins on a name clash.
    #[must_use]
    pub fn list(&self) -> Vec<PackageEntry> {
        let mut found: Vec<PackageEntry> = Vec::new();
        for root in &self.roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let dir = entry.path();
                if name.starts_with('.') || !dir.is_dir() || found.iter().any(|e| e.slug == name) {
                    continue;
                }
                let manifest = read_limited(&dir.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)
                    .and_then(|bytes| Manifest::parse(&bytes).map_err(StoreError::from));
                let (package_id, problem) = match manifest {
                    Ok(manifest) => (package_id(&manifest).to_owned(), None),
                    Err(err) => (String::new(), Some(err.code.to_owned())),
                };
                found.push(PackageEntry {
                    slug: name,
                    dir,
                    package_id,
                    problem,
                });
            }
        }
        found.sort_by(|a, b| a.slug.cmp(&b.slug));
        found
    }

    /// Reads and verifies a named package.
    pub fn open(&self, slug: &str) -> Result<Package, StoreError> {
        let entry = self
            .list()
            .into_iter()
            .find(|e| e.slug == slug)
            .ok_or_else(|| StoreError::new("pack_missing_file", format!("no package {slug}")))?;
        Ok(regia_viewer_pack::read_dir(&entry.dir)?)
    }

    /// Installs a package from `source`, returning its folder name. `progress` gets
    /// `(bytes so far, bytes total)`. An existing package of the same id stays until the new one is whole.
    pub fn install(
        &self,
        source: &mut dyn PackageSource,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<String, StoreError> {
        let root = self
            .roots
            .first()
            .ok_or_else(|| StoreError::io("package store", "no writable folder"))?;
        let manifest_bytes = source.get(MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        let name = folder_name(package_id(&manifest))
            .ok_or_else(|| StoreError::new("pack_bad_manifest", "package_id names no folder"))?;
        let temp = root.join(format!(
            ".install-{name}-{}-{}",
            std::process::id(),
            INSTALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = fill_and_check(&temp, source, &manifest, &manifest_bytes, progress)
            .and_then(|()| replace_folder(&temp, &root.join(&name)));
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        result.map(|()| name)
    }
}

/// Downloads every listed file into `temp`, then verifies the whole package the way an open does.
fn fill_and_check(
    temp: &Path,
    source: &mut dyn PackageSource,
    manifest: &Manifest,
    manifest_bytes: &[u8],
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), StoreError> {
    let total = manifest.files.iter().map(|f| f.bytes).sum::<u64>();
    let mut done = 0u64;
    progress(done, total);
    for file in &manifest.files {
        let bytes = source.get(&file.path, file.bytes)?;
        // `Manifest::parse` refused any path that could leave the folder.
        let target = temp.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|err| StoreError::io(&file.path, err))?;
        }
        std::fs::write(&target, &bytes).map_err(|err| StoreError::io(&file.path, err))?;
        done += file.bytes;
        progress(done, total);
    }
    std::fs::write(temp.join(MANIFEST_FILE), manifest_bytes)
        .map_err(|err| StoreError::io(MANIFEST_FILE, err))?;
    regia_viewer_pack::read_dir(temp)?;
    Ok(())
}

fn replace_folder(temp: &Path, target: &Path) -> Result<(), StoreError> {
    if target.exists() {
        std::fs::remove_dir_all(target).map_err(|err| StoreError::io("old package", err))?;
    }
    std::fs::rename(temp, target).map_err(|err| StoreError::io("rename", err))
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, StoreError> {
    use std::io::Read;
    let what = path.display().to_string();
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|err| StoreError::fetch(&what, err))?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| StoreError::fetch(&what, err))?;
    if bytes.len() as u64 > limit {
        return Err(StoreError::new(
            "pack_too_large",
            format!("{what} is over {limit} bytes"),
        ));
    }
    Ok(bytes)
}

/// A package folder on this machine, as a source (tests, and copying from a mounted folder).
pub struct DirSource(pub PathBuf);

impl PackageSource for DirSource {
    fn get(&mut self, path: &str, limit: u64) -> Result<Vec<u8>, StoreError> {
        read_limited(&self.0.join(path), limit)
    }
}
