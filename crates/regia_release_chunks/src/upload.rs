// SPDX-License-Identifier: MPL-2.0
//! Upload a release directory through an [`UploadBackend`] (ADR-8687).
//!
//! The order is the plan from [`crate::upload_plan_with`]: chunks, then other payload, then
//! manifests. [`FsBackend`] writes that directory on a local disk. [`FakeBackend`] records the
//! calls for tests. [`crate::CliBackend`] runs a hosting CLI (ADR-8846). This module itself
//! opens no network connection.
//!
//! Requirements at pin `c9513623f` (`regia-absorb-4`) describe a hosted upload. This module
//! follows the ordered plan only. Python was not copied.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{ChunkError, UploadPlanOptions, upload_plan_with};

/// One object a backend already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// Base name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
}

/// Where a release is published. `list`, then `put` for each planned file, then `publish`.
pub trait UploadBackend {
    /// Objects already stored.
    ///
    /// # Errors
    ///
    /// `upload_backend` or `io_error` when the listing cannot be read.
    fn list(&self) -> Result<Vec<RemoteFile>, ChunkError>;

    /// Store `bytes` under `name`, replacing a previous object of that name.
    ///
    /// # Errors
    ///
    /// `upload_backend` or `io_error` when the object cannot be stored.
    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<(), ChunkError>;

    /// Store the local file `path` under `name`. The default reads the file and calls `put`.
    /// A backend that hands a path to another program overrides it (ADR-8846).
    ///
    /// # Errors
    ///
    /// `io_error` when the file cannot be read, or an error of `put`.
    fn put_file(&mut self, name: &str, path: &Path) -> Result<(), ChunkError> {
        let bytes = fs::read(path).map_err(|err| ChunkError::io(path, &err))?;
        self.put(name, &bytes)
    }

    /// Mark the upload complete. Called after every planned `put`.
    ///
    /// # Errors
    ///
    /// `upload_backend` or `io_error` when the publish marker cannot be written.
    fn publish(&mut self) -> Result<(), ChunkError>;
}

/// What [`upload_with_backend`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadReceipt {
    /// File names passed to `put`, in call order.
    pub puts: Vec<String>,
    /// Planned files skipped because the remote size already matched, or every planned file
    /// when `dry_run` is set.
    pub skipped: Vec<String>,
    /// Whether `publish` ran.
    pub published: bool,
}

/// Filesystem backend. Objects are files in `root`. `publish` writes `PUBLISHED` containing `OK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsBackend {
    root: PathBuf,
}

impl FsBackend {
    /// Create `root` if needed.
    ///
    /// # Errors
    ///
    /// `io_error` when the directory cannot be created.
    pub fn create(root: &Path) -> Result<Self, ChunkError> {
        fs::create_dir_all(root).map_err(|err| ChunkError::io(root, &err))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// The directory this backend writes.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl UploadBackend for FsBackend {
    fn list(&self) -> Result<Vec<RemoteFile>, ChunkError> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.root).map_err(|err| ChunkError::io(&self.root, &err))?;
        for entry in entries {
            let entry = entry.map_err(|err| ChunkError::io(&self.root, &err))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let size = entry.metadata().map_err(|err| ChunkError::io(&path, &err))?.len();
            out.push(RemoteFile { name, size });
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(out)
    }

    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<(), ChunkError> {
        let path = self.root.join(safe_name(name)?);
        write_atomic(&path, bytes)
    }

    fn publish(&mut self) -> Result<(), ChunkError> {
        write_atomic(&self.root.join("PUBLISHED"), b"OK\n")
    }
}

/// In-memory backend. `calls` records `put:<name>` and `publish` in order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FakeBackend {
    /// Stored objects.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Calls, in order.
    pub calls: Vec<String>,
    /// Whether `publish` has run.
    pub published: bool,
}

impl UploadBackend for FakeBackend {
    fn list(&self) -> Result<Vec<RemoteFile>, ChunkError> {
        Ok(self
            .files
            .iter()
            .map(|(name, bytes)| RemoteFile {
                name: name.clone(),
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            })
            .collect())
    }

    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<(), ChunkError> {
        let name = safe_name(name)?.to_string();
        self.calls.push(format!("put:{name}"));
        self.files.insert(name, bytes.to_vec());
        Ok(())
    }

    fn publish(&mut self) -> Result<(), ChunkError> {
        self.calls.push("publish".to_string());
        self.published = true;
        self.files.insert("PUBLISHED".to_string(), b"OK\n".to_vec());
        Ok(())
    }
}

/// Upload `dir` through `backend` in plan order.
///
/// `dry_run` lists the plan and does not call `put` or `publish`. When `skip_same_size` is
/// set, a remote object whose size already matches is not put again.
///
/// # Errors
///
/// `release_dir` when `dir` cannot be planned, `unsafe_path` for a name that is not a single
/// path component, or a backend error.
pub fn upload_with_backend(
    dir: &Path,
    backend: &mut dyn UploadBackend,
    opts: &UploadPlanOptions,
) -> Result<UploadReceipt, ChunkError> {
    let plan = upload_plan_with(dir, opts)?;
    let mut planned = Vec::new();
    for group in &plan.groups {
        for name in &group.files {
            safe_name(name)?;
            planned.push(name.clone());
        }
    }
    if opts.dry_run {
        return Ok(UploadReceipt {
            puts: Vec::new(),
            skipped: planned,
            published: false,
        });
    }
    let listed = backend.list()?;
    let sizes: BTreeMap<String, u64> = listed.into_iter().map(|file| (file.name, file.size)).collect();
    let mut puts = Vec::new();
    let mut skipped = Vec::new();
    for name in &planned {
        let path = dir.join(name);
        let meta = fs::metadata(&path).map_err(|err| ChunkError::io(&path, &err))?;
        if opts.skip_same_size && sizes.get(name).copied() == Some(meta.len()) {
            skipped.push(name.clone());
            continue;
        }
        backend.put_file(name, &path)?;
        puts.push(name.clone());
    }
    backend.publish()?;
    Ok(UploadReceipt {
        puts,
        skipped,
        published: true,
    })
}

/// A `--backend` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpec {
    /// `fs:<directory>`: copy into a local directory.
    Fs(PathBuf),
    /// `cli:<config.toml>`: run the hosting CLI the config names (ADR-8846).
    Cli(PathBuf),
}

/// `fs:<directory>` or `cli:<config.toml>`.
///
/// # Errors
///
/// `upload_backend` for another scheme or an empty path.
pub fn parse_backend(text: &str) -> Result<BackendSpec, ChunkError> {
    if let Some(path) = text.strip_prefix("cli:") {
        if path.is_empty() {
            return Err(ChunkError::new("upload_backend", "cli: needs a config file"));
        }
        return Ok(BackendSpec::Cli(PathBuf::from(path)));
    }
    if text.starts_with("fs:") {
        return parse_fs_backend(text).map(BackendSpec::Fs);
    }
    Err(ChunkError::new(
        "upload_backend",
        format!("backend {text:?} is not fs:<dir> or cli:<config.toml>"),
    ))
}

/// `fs:<directory>`. Any other scheme is `upload_backend`.
///
/// # Errors
///
/// `upload_backend` when the text is not `fs:` plus a non-empty path.
pub fn parse_fs_backend(text: &str) -> Result<PathBuf, ChunkError> {
    let Some(path) = text.strip_prefix("fs:") else {
        return Err(ChunkError::new(
            "upload_backend",
            format!("backend {text:?} is not fs:<dir>"),
        ));
    };
    if path.is_empty() {
        return Err(ChunkError::new("upload_backend", "fs: needs a directory"));
    }
    Ok(PathBuf::from(path))
}

pub(crate) fn safe_name(name: &str) -> Result<&str, ChunkError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(ChunkError::new(
            "unsafe_path",
            format!("upload name {name:?} is not a single file name"),
        ));
    }
    Ok(name)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ChunkError> {
    let tmp_name = format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("part"),
        std::process::id()
    );
    let tmp = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.write_all(bytes).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.sync_all().map_err(|err| ChunkError::io(&tmp, &err))?;
    }
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        ChunkError::io(path, &err)
    })
}
