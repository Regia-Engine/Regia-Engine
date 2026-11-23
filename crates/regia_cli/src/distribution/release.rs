// SPDX-License-Identifier: MPL-2.0
//! The packaged engine release: `regia-engine-<version>/` with `regia-engine.release.ron`
//! (spec 94 § 4.1). The manifest is a pure function of the file bytes (sorted paths, no
//! timestamps), so packaging one tree twice gives the same manifest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::{DistError, Problem, slash_path};

#[cfg(not(target_os = "windows"))]
#[path = "platform/portable.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "platform/windows.rs"]
mod platform;

/// File name of the release manifest at the root of a packaged release.
pub const RELEASE_MANIFEST: &str = "regia-engine.release.ron";

/// The release manifest version this engine writes and reads.
pub const RELEASE_FORMAT_VERSION: u32 = 1;

/// One file of a release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFile {
    /// Path relative to the release root, `/` separators.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// BLAKE3 of the bytes, lowercase hex.
    pub blake3: String,
}

/// `regia-engine.release.ron`, format v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineRelease {
    /// [`RELEASE_FORMAT_VERSION`] when written.
    pub format_version: u32,
    /// Engine version (`[workspace.package] version`).
    pub version: String,
    /// `git rev-parse HEAD` of the packaged checkout; `None` outside git.
    pub commit: Option<String>,
    /// `[workspace.package] rust-version` (empty when the workspace names none).
    pub rust_version: String,
    /// Directories under `crates/` that hold a `Cargo.toml`, sorted.
    pub crates: Vec<String>,
    /// Every file of the release except the manifest, sorted by path.
    pub files: Vec<ReleaseFile>,
}

impl EngineRelease {
    /// Brings a manifest read at `format_version` up to [`RELEASE_FORMAT_VERSION`]. v1 is the first
    /// version: there is nothing to migrate yet; a later shape change adds its step here.
    ///
    /// # Errors
    ///
    /// `release_format_newer` for a manifest written by a newer engine.
    pub fn migrate(self) -> Result<Self, DistError> {
        if self.format_version > RELEASE_FORMAT_VERSION {
            return Err(DistError::new(
                "release_format_newer",
                format!(
                    "{RELEASE_MANIFEST} is format_version {} (this engine reads up to {RELEASE_FORMAT_VERSION}); \
                     use a newer `regia`",
                    self.format_version
                ),
            ));
        }
        Ok(self)
    }
}

/// The serde shape of `regia-engine.release.ron` for `cargo xtask check-formats`
/// (`docs/formats/engine-release-ron.md`, ADR-2316).
#[must_use]
pub fn release_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<EngineRelease>(),
        serde_field_names::<ReleaseFile>(),
    ])
}

/// `regia-engine.release.ron` is at its first version: no migration steps.
#[must_use]
pub fn release_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// What [`package_release`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagedRelease {
    /// `<out>/regia-engine-<version>`.
    pub dir: PathBuf,
    /// The manifest written into it.
    pub release: EngineRelease,
}

/// What [`verify_release`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseReport {
    /// The manifest, when it was readable.
    pub release: Option<EngineRelease>,
    /// Files listed and checked.
    pub checked: usize,
    /// Findings (`release_*` codes).
    pub problems: Vec<Problem>,
}

impl ReleaseReport {
    /// True when no finding is an error.
    pub fn ok(&self) -> bool {
        self.problems
            .iter()
            .all(|p| p.severity == super::Severity::Warning)
    }
}

/// The `[workspace.package]` table of an engine root's `Cargo.toml`.
fn workspace_package(root: &Path) -> Result<toml::Table, DistError> {
    // The workspace manifest is read even if it is not in Git's tracked-file list.
    let path = release_source_file(root, "Cargo.toml").map_err(|err| {
        if err.code == "release_file_link" {
            err
        } else {
            DistError::new("engine_root_invalid", err.message)
        }
    })?;
    let text = std::fs::read_to_string(&path).map_err(|err| {
        DistError::new(
            "engine_root_invalid",
            format!("{} is not an engine checkout: {err}", path.display()),
        )
    })?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|err| DistError::new("engine_root_invalid", format!("{}: {err}", path.display())))?;
    table
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(toml::Value::as_table)
        .cloned()
        .ok_or_else(|| {
            DistError::new(
                "engine_root_invalid",
                format!("{} has no [workspace.package]", path.display()),
            )
        })
}

/// True when `root` is a git work tree (`.git` is a directory or a worktree's file).
fn is_git(root: &Path) -> bool {
    root.join(".git").exists()
}

/// Runs `git -C root args` and returns stdout.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, DistError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| DistError::new("git_failed", format!("git {}: {err}", args.join(" "))))?;
    if !output.status.success() {
        return Err(DistError::new(
            "git_failed",
            format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output.stdout)
}

/// True for a walk entry a release never contains: `.git` (a directory, or a worktree's file),
/// `target/` and the manifest. Other dot files stay: templates embed their `.gitkeep` files.
fn skipped(name: &str, is_dir: bool) -> bool {
    name == ".git" || (is_dir && name == "target") || name == RELEASE_MANIFEST
}

/// The files under `root` (relative, `/` separators, sorted) by a walk that skips `.git`,
/// `target/` and the manifest.
fn walk_files(root: &Path) -> Result<Vec<String>, DistError> {
    let (files, links) = walk_tree(root)?;
    if let Some(link) = links.first() {
        return Err(DistError::new(
            "release_file_link",
            format!("{link} is a symbolic link"),
        ));
    }
    Ok(files)
}

/// [`walk_files`], with the symbolic links the walk met (not followed) as the second list.
fn walk_tree(root: &Path) -> Result<(Vec<String>, Vec<String>), DistError> {
    let mut out = Vec::new();
    let mut links = Vec::new();
    let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|entry| {
        entry.depth() == 0 || !skipped(&entry.file_name().to_string_lossy(), entry.file_type().is_dir())
    });
    for entry in walker {
        let entry = entry.map_err(|err| DistError::new("io_error", format!("{}: {err}", root.display())))?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            out.push(slash_path(rel));
        } else if entry.file_type().is_symlink() {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            links.push(slash_path(rel));
        }
    }
    out.sort();
    links.sort();
    Ok((out, links))
}

/// The files a release of `root` holds: the tracked files when `root` is a git work tree (exactly
/// the tag), otherwise a walk. Sorted, relative, `/` separators.
fn release_files(root: &Path) -> Result<Vec<String>, DistError> {
    if !is_git(root) {
        return walk_files(root);
    }
    let raw = git(root, &["ls-files", "-z"])?;
    let mut out: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|p| p.as_str() != RELEASE_MANIFEST)
        .collect();
    out.sort();
    // Check the whole list before build_manifest reads even Cargo.toml.
    for rel in &out {
        release_source_file(root, rel)?;
    }
    Ok(out)
}

/// Resolves a release source file without following links, including links in ancestor folders.
fn release_source_file(root: &Path, rel: &str) -> Result<PathBuf, DistError> {
    if !plain_relative(rel) {
        return Err(DistError::new(
            "release_manifest_invalid",
            format!("{rel:?} is not a plain relative path"),
        ));
    }
    let mut path = root.to_path_buf();
    let mut regular = false;
    for segment in rel.split('/') {
        path.push(segment);
        let meta = path
            .symlink_metadata()
            .map_err(|err| DistError::io(&path, &err))?;
        if platform::is_link(&meta) {
            return Err(DistError::new(
                "release_file_link",
                format!("{rel} leads through a symbolic link or reparse point"),
            ));
        }
        regular = meta.is_file();
    }
    if !regular {
        return Err(DistError::new(
            "io_error",
            format!("{rel} is not a regular release file"),
        ));
    }
    Ok(path)
}

/// Hashes one file.
fn hash_file(root: &Path, rel: &str) -> Result<ReleaseFile, DistError> {
    let path = release_source_file(root, rel)?;
    let mut file = std::fs::File::open(&path).map_err(|err| DistError::io(&path, &err))?;
    let mut hasher = blake3::Hasher::new();
    let size = std::io::copy(&mut file, &mut hasher).map_err(|err| DistError::io(&path, &err))?;
    Ok(ReleaseFile {
        path: rel.to_string(),
        size,
        blake3: hasher.finalize().to_hex().to_string(),
    })
}

/// The release manifest text (pretty RON, deterministic).
fn manifest_text(release: &EngineRelease) -> Result<String, DistError> {
    ron::ser::to_string_pretty(release, regia_core::ron_pretty())
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| DistError::new("io_error", format!("cannot write {RELEASE_MANIFEST}: {err}")))
}

/// Builds the manifest of `root` for `files` (no copy). `dirty` marks a commit whose tree is not
/// what the manifest lists.
fn build_manifest(root: &Path, files: &[String], dirty: bool) -> Result<EngineRelease, DistError> {
    let package = workspace_package(root)?;
    let version = package
        .get("version")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| DistError::new("engine_root_invalid", "[workspace.package] has no version"))?
        .to_string();
    regia_core::engine_version::EngineVersion::parse(&version)
        .map_err(|err| DistError::new("engine_root_invalid", err.to_string()))?;
    let rust_version = package
        .get("rust-version")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let commit = if is_git(root) {
        let head = git(root, &["rev-parse", "HEAD"])?;
        let head = String::from_utf8_lossy(&head);
        let suffix = if dirty { "-dirty" } else { "" };
        Some(format!("{}{suffix}", head.trim()))
    } else {
        None
    };
    let crates: BTreeSet<String> = files
        .iter()
        .filter_map(|f| f.strip_prefix("crates/")?.strip_suffix("/Cargo.toml"))
        .filter(|name| !name.contains('/'))
        .map(str::to_string)
        .collect();
    let files = files
        .iter()
        .map(|rel| hash_file(root, rel))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EngineRelease {
        format_version: RELEASE_FORMAT_VERSION,
        version,
        commit,
        rust_version,
        crates: crates.into_iter().collect(),
        files,
    })
}

/// Copies the release of `engine_root` into `<out>/regia-engine-<version>/` and writes its manifest.
/// The copy is staged in a sibling `.partial` directory and renamed into place at the end.
///
/// # Errors
///
/// `engine_root_invalid` (no workspace version), `release_dirty` (a git checkout with uncommitted
/// changes to tracked files, unless `allow_dirty`), `release_exists` (the destination exists),
/// `release_file_link` (a source symlink or reparse point), `git_failed`, `io_error`.
pub fn package_release(
    engine_root: &Path,
    out: &Path,
    allow_dirty: bool,
) -> Result<PackagedRelease, DistError> {
    let _span = tracing::info_span!("regia_cli::package_release").entered();
    let dirty = is_git(engine_root)
        && !git(engine_root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    if dirty && !allow_dirty {
        return Err(DistError::new(
            "release_dirty",
            format!(
                "{} has uncommitted changes to tracked files; package a clean tag (or pass --allow-dirty)",
                engine_root.display()
            ),
        ));
    }
    let files = release_files(engine_root)?;
    let release = build_manifest(engine_root, &files, dirty)?;
    let dir = out.join(format!("regia-engine-{}", release.version));
    if dir.exists() {
        return Err(DistError::new(
            "release_exists",
            format!(
                "{} already exists; remove it or choose another --out",
                dir.display()
            ),
        ));
    }
    let staged = out.join(format!("regia-engine-{}.partial", release.version));
    if staged.exists() {
        std::fs::remove_dir_all(&staged).map_err(|err| DistError::io(&staged, &err))?;
    }
    for file in &release.files {
        let from = release_source_file(engine_root, &file.path)?;
        let to = staged.join(&file.path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|err| DistError::io(parent, &err))?;
        }
        std::fs::copy(&from, &to).map_err(|err| DistError::io(&from, &err))?;
    }
    std::fs::create_dir_all(&staged).map_err(|err| DistError::io(&staged, &err))?;
    let manifest = staged.join(RELEASE_MANIFEST);
    std::fs::write(&manifest, manifest_text(&release)?).map_err(|err| DistError::io(&manifest, &err))?;
    std::fs::rename(&staged, &dir).map_err(|err| DistError::io(&dir, &err))?;
    Ok(PackagedRelease { dir, release })
}

/// Reads and migrates the manifest of a release directory.
///
/// # Errors
///
/// `release_manifest_missing`, `release_manifest_invalid` (also a listed path that is absolute or
/// leaves the release), `release_format_newer`, `io_error` (the manifest is there but cannot be
/// read).
pub fn read_release(dir: &Path) -> Result<EngineRelease, DistError> {
    let path = dir.join(RELEASE_MANIFEST);
    let bytes =
        regia_core::project_path::read_regular_capped(&path, MAX_RELEASE_MANIFEST_BYTES).map_err(|err| {
            let code = match err.kind() {
                std::io::ErrorKind::InvalidData => "release_manifest_invalid",
                std::io::ErrorKind::NotFound => "release_manifest_missing",
                // It is there but cannot be read (permissions, a failing disk).
                _ => "io_error",
            };
            DistError::new(code, format!("{}: {err}", path.display()))
        })?;
    let text = String::from_utf8(bytes)
        .map_err(|err| DistError::new("release_manifest_invalid", format!("{}: {err}", path.display())))?;
    // Read the version first so a newer manifest reports `release_format_newer`, not a parse error.
    #[derive(Deserialize)]
    struct Head {
        format_version: u32,
    }
    let head: Head = ron::from_str(&text)
        .map_err(|err| DistError::new("release_manifest_invalid", format!("{}: {err}", path.display())))?;
    if head.format_version > RELEASE_FORMAT_VERSION {
        return EngineRelease {
            format_version: head.format_version,
            version: String::new(),
            commit: None,
            rust_version: String::new(),
            crates: Vec::new(),
            files: Vec::new(),
        }
        .migrate();
    }
    let release: EngineRelease = ron::from_str(&text)
        .map_err(|err| DistError::new("release_manifest_invalid", format!("{}: {err}", path.display())))?;
    let release = release.migrate()?;
    if let Some(file) = release.files.iter().find(|f| !inside_release(&f.path)) {
        return Err(DistError::new(
            "release_manifest_invalid",
            format!("{}: {} leaves the release", path.display(), file.path),
        ));
    }
    Ok(release)
}

/// Largest manifest `verify` reads: a real release lists about ten thousand files in about 2 MiB.
const MAX_RELEASE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// True when `rel` names a file under the release root: `verify` reads what the manifest lists, and
/// a manifest may come from anywhere. The rule is the project path rule (a release written on Linux
/// gets verified on Windows, where `.. /x` is the parent and `NUL` is a device), and a release path
/// has `/` separators only.
fn inside_release(rel: &str) -> bool {
    !rel.contains('\\') && regia_core::project_path::check_relative(rel).is_ok()
}

/// What `verify` found at a listed path.
enum ListedFile {
    Same,
    Missing,
    /// The path or a folder above it exists but cannot be read (permissions, IO): not "missing".
    Unreadable(std::io::ErrorKind),
    /// A symbolic link at the path or in a folder above it.
    Link,
    Changed,
}

/// Compares the file at a listed path with its record. A link is not followed: its target can change
/// after this check, and a listed file is what was packaged, not a pointer to it. A size that differs,
/// or a FIFO or a device in the file's place, needs no read, and the hash reads as a stream, so a huge
/// or endless file costs neither the memory of its size nor a hang.
fn check_listed(dir: &Path, file: &ReleaseFile) -> ListedFile {
    let mut walk = dir.to_path_buf();
    let mut last = None;
    for segment in file.path.split('/') {
        walk.push(segment);
        match walk.symlink_metadata() {
            Err(err) => return gone_or_unreadable(&err),
            Ok(meta) if meta.file_type().is_symlink() => return ListedFile::Link,
            Ok(meta) => last = Some(meta),
        }
    }
    if !last.is_some_and(|meta| meta.is_file() && meta.len() == file.size) {
        return ListedFile::Changed;
    }
    let mut hasher = blake3::Hasher::new();
    let hashed = std::fs::File::open(&walk).and_then(|mut opened| std::io::copy(&mut opened, &mut hasher));
    match hashed {
        Ok(_) if hasher.finalize().to_hex().as_str() == file.blake3 => ListedFile::Same,
        Ok(_) => ListedFile::Changed,
        Err(err) => gone_or_unreadable(&err),
    }
}

/// `NotFound` is a missing file; any other error means something is there that cannot be read.
fn gone_or_unreadable(err: &std::io::Error) -> ListedFile {
    match err.kind() {
        std::io::ErrorKind::NotFound => ListedFile::Missing,
        kind => ListedFile::Unreadable(kind),
    }
}

/// Checks a release directory against its manifest: every listed file present with its size and
/// hash and no link on its way (`release_file_missing`, `release_file_unreadable`,
/// `release_file_changed`, `release_file_link`: errors), no link anywhere outside `target/` and `.git` (`release_file_link`), and no unlisted file
/// there (`release_file_extra`: a warning). A manifest that lists nothing checks nothing and is
/// refused (`release_manifest_invalid`).
///
/// This tells a damaged or partly replaced tree from the release its manifest describes. The manifest
/// travels with the tree and is not signed, so it says nothing about who wrote the pair.
pub fn verify_release(dir: &Path) -> ReleaseReport {
    let _span = tracing::info_span!("regia_cli::verify_release").entered();
    let release = match read_release(dir) {
        Ok(release) => release,
        Err(err) => {
            return ReleaseReport {
                release: None,
                checked: 0,
                problems: vec![err.into()],
            };
        }
    };
    let mut problems = Vec::new();
    if release.files.is_empty() {
        problems.push(Problem::error(
            "release_manifest_invalid",
            format!("{RELEASE_MANIFEST} lists no files, so nothing would be checked"),
        ));
    }
    for file in &release.files {
        match check_listed(dir, file) {
            ListedFile::Same => {}
            ListedFile::Missing => problems.push(Problem::error(
                "release_file_missing",
                format!("{} is listed but missing", file.path),
            )),
            ListedFile::Unreadable(kind) => problems.push(Problem::error(
                "release_file_unreadable",
                format!("{} is listed but cannot be read: {kind}", file.path),
            )),
            ListedFile::Link => problems.push(Problem::error(
                "release_file_link",
                format!("{} leads through a symbolic link", file.path),
            )),
            ListedFile::Changed => problems.push(Problem::error(
                "release_file_changed",
                format!("{} differs from the release", file.path),
            )),
        }
    }
    match walk_tree(dir) {
        Ok((present, links)) => {
            let listed: BTreeSet<&str> = release.files.iter().map(|f| f.path.as_str()).collect();
            for path in links.iter().filter(|p| !listed.contains(p.as_str())) {
                problems.push(Problem::error(
                    "release_file_link",
                    format!("{path} is a symbolic link; a release holds none"),
                ));
            }
            for path in present.iter().filter(|p| !listed.contains(p.as_str())) {
                problems.push(Problem::warning(
                    "release_file_extra",
                    format!("{path} is not part of the release"),
                ));
            }
        }
        Err(err) => problems.push(err.into()),
    }
    ReleaseReport {
        checked: release.files.len(),
        release: Some(release),
        problems,
    }
}

/// File name of a plain checksum list (`CHECKSUMS.b3`, the `b3sum` text format).
pub const CHECKSUMS_FILE: &str = "CHECKSUMS.b3";

/// Hashes `files` (paths relative to `root`, `/` separators) into the entries of a checksum list,
/// sorted by path. Used by `cargo xtask demo-package` for the plain archives it publishes next to the
/// Steam depot. A symlink is not followed, and a directory, FIFO or device is not opened.
///
/// # Errors
///
/// `io_error` when a file cannot be read, is a symbolic link, or is not a regular file.
/// `checksum_path_invalid` for a path that is absolute, empty, or not a plain relative path under
/// the project path rule (Windows parents such as `...` and `.. `, and device names such as `nul.txt`).
pub fn checksum_files(root: &Path, files: &[String]) -> Result<Vec<ReleaseFile>, DistError> {
    let _span = tracing::info_span!("regia_cli::checksum_files").entered();
    let mut out = Vec::with_capacity(files.len());
    // A repeated name is listed once (`verify_checksums` refuses a list that repeats a path).
    let mut seen = BTreeSet::new();
    for rel in files {
        if !seen.insert(rel.as_str()) {
            continue;
        }
        if !plain_relative(rel) {
            return Err(DistError::new(
                "checksum_path_invalid",
                format!("{rel:?} is not a plain relative path"),
            ));
        }
        match hash_listed(root, rel) {
            ListedHash::Ready { size, blake3 } => out.push(ReleaseFile {
                path: rel.clone(),
                size,
                blake3,
            }),
            ListedHash::Missing => {
                let path = root.join(rel);
                return Err(DistError::io(
                    &path,
                    &std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
                ));
            }
            ListedHash::Link => {
                return Err(DistError::new(
                    "io_error",
                    format!("{} is a symbolic link", root.join(rel).display()),
                ));
            }
            ListedHash::NotRegular => {
                return Err(DistError::new(
                    "io_error",
                    format!("{} is not a regular file", root.join(rel).display()),
                ));
            }
            ListedHash::Unreadable(kind) => {
                return Err(DistError::new(
                    "io_error",
                    format!("{}: {kind}", root.join(rel).display()),
                ));
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// True for a non-empty `/`-separated path the project path rule accepts, with no `.`, `..` or
/// empty part. `check_relative` alone would drop `.` and empty parts; a checksum row is the path
/// as listed, and `.. `, `...` and `nul.txt` are parents and devices on Windows.
fn plain_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && regia_core::project_path::check_relative(rel).is_ok()
}

/// A listed path hashed without following a link or reading a non-file.
enum ListedHash {
    Missing,
    Link,
    NotRegular,
    Unreadable(std::io::ErrorKind),
    Ready { size: u64, blake3: String },
}

/// Walks `rel` under `dir`. A symlink on the way is [`ListedHash::Link`] and is not opened. A
/// directory, FIFO or device is [`ListedHash::NotRegular`] and is not opened. A regular file is
/// hashed as a stream.
fn hash_listed(dir: &Path, rel: &str) -> ListedHash {
    let mut walk = dir.to_path_buf();
    let mut last_is_file = false;
    let mut size = 0u64;
    for segment in rel.split('/') {
        walk.push(segment);
        match walk.symlink_metadata() {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return ListedHash::Missing,
            Err(err) => return ListedHash::Unreadable(err.kind()),
            Ok(meta) if meta.file_type().is_symlink() => return ListedHash::Link,
            Ok(meta) => {
                last_is_file = meta.is_file();
                size = meta.len();
            }
        }
    }
    if !last_is_file {
        return ListedHash::NotRegular;
    }
    let mut hasher = blake3::Hasher::new();
    match std::fs::File::open(&walk).and_then(|mut opened| std::io::copy(&mut opened, &mut hasher)) {
        Ok(_) => ListedHash::Ready {
            size,
            blake3: hasher.finalize().to_hex().to_string(),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ListedHash::Missing,
        Err(err) => ListedHash::Unreadable(err.kind()),
    }
}

/// The text of a checksum list: one `<blake3 hex>  <path>` line per file (two spaces, the
/// `b3sum` format), in the order given.
pub fn checksums_text(files: &[ReleaseFile]) -> String {
    let mut text = String::new();
    for file in files {
        text.push_str(&file.blake3);
        text.push_str("  ");
        text.push_str(&file.path);
        text.push('\n');
    }
    text
}

/// Checks `dir` against the checksum list in `dir/CHECKSUMS.b3`: every listed file exists with the
/// listed hash. A listed path is not followed and a non-file is not read. Findings:
/// `checksums_missing`, `checksums_invalid` (a line that is not `<64 hex>  <path>`, an unsafe path,
/// an empty list, a duplicate row, or a list file that is not a regular file of text),
/// `checksum_file_missing`, `checksum_file_link`, `checksum_file_unreadable`, `checksum_mismatch`.
pub fn verify_checksums(dir: &Path) -> Vec<Problem> {
    let _span = tracing::info_span!("regia_cli::verify_checksums").entered();
    let list = dir.join(CHECKSUMS_FILE);
    let bytes = match regia_core::project_path::read_regular_capped(&list, MAX_RELEASE_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return vec![Problem::error(
                "checksums_missing",
                format!("{}: {err}", list.display()),
            )];
        }
        Err(err) => {
            return vec![Problem::error(
                "checksums_invalid",
                format!("{}: {err}", list.display()),
            )];
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            return vec![Problem::error(
                "checksums_invalid",
                format!("{}: {err}", list.display()),
            )];
        }
    };
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (number, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let Some((hash, rel)) = line.split_once("  ") else {
            problems.push(Problem::error(
                "checksums_invalid",
                format!("{CHECKSUMS_FILE} line {}: expected `<hash>  <path>`", number + 1),
            ));
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || !plain_relative(rel) {
            problems.push(Problem::error(
                "checksums_invalid",
                format!("{CHECKSUMS_FILE} line {}: bad hash or path", number + 1),
            ));
            continue;
        }
        if !seen.insert(rel) {
            problems.push(Problem::error(
                "checksums_invalid",
                format!("{rel} is listed twice"),
            ));
            continue;
        }
        match hash_listed(dir, rel) {
            ListedHash::Missing => problems.push(Problem::error(
                "checksum_file_missing",
                format!("{rel} is listed but missing"),
            )),
            ListedHash::Link => problems.push(Problem::error(
                "checksum_file_link",
                format!("{rel} leads through a symbolic link"),
            )),
            ListedHash::NotRegular => problems.push(Problem::error(
                "checksum_mismatch",
                format!("{rel} is not a regular file (not read)"),
            )),
            ListedHash::Unreadable(kind) => problems.push(Problem::error(
                "checksum_file_unreadable",
                format!("{rel} is listed but cannot be read: {kind}"),
            )),
            ListedHash::Ready { blake3, .. } => {
                if !blake3.eq_ignore_ascii_case(hash) {
                    problems.push(Problem::error(
                        "checksum_mismatch",
                        format!("{rel} differs from {CHECKSUMS_FILE}"),
                    ));
                }
            }
        }
    }
    if seen.is_empty() && problems.is_empty() {
        problems.push(Problem::error(
            "checksums_invalid",
            "the checksum list names no files",
        ));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small engine tree without git: a workspace manifest, two crates and a doc.
    fn engine_tree() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("dist_release_src");
        for (rel, text) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n[workspace.package]\nversion = \"0.2.5\"\nrust-version = \"1.98\"\n",
            ),
            ("crates/regia/Cargo.toml", "[package]\nname = \"regia\"\n"),
            ("crates/regia/src/lib.rs", "//! regia\n"),
            (
                "crates/regia_core/Cargo.toml",
                "[package]\nname = \"regia_core\"\n",
            ),
            ("crates/regia_core/src/lib.rs", "//! core\n"),
            ("crates/regia_core/tests/data/nested/Cargo.toml", "[package]\n"),
            ("docs/a.md", "# a\n"),
            ("crates/regia/templates/.gitkeep", ""),
            ("target/debug/junk", "x"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        }
        root
    }

    #[cfg(unix)]
    #[test]
    fn package_refuses_tracked_symlinks_before_copying() {
        let root = engine_tree();
        let outside = regia_test_utils::unique_temp_dir("release-link-outside");
        let out = regia_test_utils::unique_temp_dir("release-link-out");
        std::fs::write(outside.join("private.txt"), "must not ship").expect("fixture");
        git(&root, &["init", "-q"]).expect("git init");
        git(&root, &["add", "Cargo.toml", "docs/a.md"]).expect("git add");
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .expect("git commit");
        for target in [
            outside.join("private.txt"),
            root.join("docs/a.md"),
            outside.join("missing"),
        ] {
            let link = root.join("linked.txt");
            std::os::unix::fs::symlink(&target, &link).expect("symlink");
            git(&root, &["add", "linked.txt"]).expect("track link");
            let err = package_release(&root, &out, true).expect_err("links are not release files");
            assert_eq!(err.code, "release_file_link");
            assert!(!out.join("regia-engine-0.2.5").exists());
            assert!(!out.join("regia-engine-0.2.5.partial").exists());
            std::fs::remove_file(link).expect("remove link");
        }
        for dir in [root, outside, out] {
            std::fs::remove_dir_all(dir).expect("cleanup");
        }
    }

    #[test]
    fn package_copies_the_tree_and_writes_a_manifest_that_verifies() {
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_out");
        let packaged = package_release(&root, &out, false).expect("package");
        assert_eq!(packaged.dir, out.join("regia-engine-0.2.5"));
        let release = &packaged.release;
        assert_eq!(release.format_version, 1);
        assert_eq!(release.version, "0.2.5");
        assert_eq!(release.rust_version, "1.98");
        assert_eq!(release.commit, None);
        assert_eq!(
            release.crates,
            vec!["regia".to_string(), "regia_core".to_string()]
        );
        let paths: Vec<_> = release.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.windows(2).all(|w| w[0] < w[1]), "sorted");
        assert!(!paths.iter().any(|p| p.starts_with("target/")), "{paths:?}");
        assert!(paths.contains(&"docs/a.md"));
        assert!(
            paths.contains(&"crates/regia/templates/.gitkeep"),
            "embedded dot files ship"
        );
        assert!(packaged.dir.join("crates/regia_core/src/lib.rs").is_file());
        assert!(!out.join("regia-engine-0.2.5.partial").exists());
        assert_eq!(read_release(&packaged.dir).expect("read"), *release);
        let report = verify_release(&packaged.dir);
        assert!(report.ok(), "{:?}", report.problems);
        assert!(report.problems.is_empty());
        assert_eq!(report.checked, release.files.len());
        assert_eq!(
            crate::distribution::engine_dir_version(&packaged.dir),
            Some(regia_core::engine_version::EngineVersion::new(0, 2, 5))
        );
        // A second package into the same place is refused.
        assert_eq!(
            package_release(&root, &out, false).expect_err("exists").code,
            "release_exists"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn the_manifest_is_byte_identical_across_packages_of_one_tree() {
        let root = engine_tree();
        let a = regia_test_utils::unique_temp_dir("dist_release_a");
        let b = regia_test_utils::unique_temp_dir("dist_release_b");
        let pa = package_release(&root, &a, false).expect("a");
        let pb = package_release(&root, &b, false).expect("b");
        let ta = std::fs::read(pa.dir.join(RELEASE_MANIFEST)).expect("a manifest");
        let tb = std::fs::read(pb.dir.join(RELEASE_MANIFEST)).expect("b manifest");
        assert_eq!(ta, tb);
        for dir in [root, a, b] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn verify_reports_missing_changed_and_extra_files() {
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_verify");
        let dir = package_release(&root, &out, false).expect("package").dir;
        std::fs::remove_file(dir.join("docs/a.md")).expect("remove");
        std::fs::write(dir.join("crates/regia/src/lib.rs"), "//! patched\n").expect("patch");
        std::fs::write(dir.join("extra.txt"), "x").expect("extra");
        std::fs::create_dir_all(dir.join("target")).expect("target");
        std::fs::write(dir.join("target/build.log"), "x").expect("target file");
        let report = verify_release(&dir);
        let codes: Vec<_> = report.problems.iter().map(|p| p.code).collect();
        assert_eq!(
            codes,
            vec![
                "release_file_changed",
                "release_file_missing",
                "release_file_extra"
            ]
        );
        assert!(!report.ok());
        for dir in [root, out] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// CLI2 audit 11: a listed file that exists but cannot be read is not "missing".
    #[cfg(unix)]
    #[test]
    fn verify_names_a_listed_file_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_unreadable");
        let dir = package_release(&root, &out, false).expect("package").dir;
        let file = dir.join("docs/a.md");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod file");
        let docs = dir.join("crates/regia/src");
        let readable = std::fs::File::open(&file).is_ok();
        let report = verify_release(&dir);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("restore file");
        // A folder the walk cannot search hides the files under it the same way.
        std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o000)).expect("chmod dir");
        let searchable = docs.join("lib.rs").symlink_metadata().is_ok();
        let folder = verify_release(&dir);
        std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o755)).expect("restore dir");
        for d in [root, out] {
            std::fs::remove_dir_all(d).ok();
        }
        // A superuser reads through any mode; there is nothing to prove then.
        if readable || searchable {
            return;
        }
        let codes: Vec<_> = report.problems.iter().map(|p| p.code).collect();
        assert_eq!(codes, vec!["release_file_unreadable"], "{:?}", report.problems);
        assert!(report.problems[0].message.contains("docs/a.md"));
        assert!(!report.ok());
        let codes: Vec<_> = folder.problems.iter().map(|p| p.code).collect();
        assert!(
            codes.contains(&"release_file_unreadable"),
            "{:?}",
            folder.problems
        );
        assert!(!codes.contains(&"release_file_missing"), "{:?}", folder.problems);
    }

    #[test]
    fn newer_and_broken_manifests_are_named() {
        let dir = regia_test_utils::unique_temp_dir("dist_release_bad");
        assert_eq!(verify_release(&dir).problems[0].code, "release_manifest_missing");
        std::fs::write(dir.join(RELEASE_MANIFEST), "(format_version: 2, new_field: [])").expect("write");
        assert_eq!(
            read_release(&dir).expect_err("newer").code,
            "release_format_newer"
        );
        std::fs::write(dir.join(RELEASE_MANIFEST), "(format_version: 1)").expect("write");
        assert_eq!(
            read_release(&dir).expect_err("short").code,
            "release_manifest_invalid"
        );
        std::fs::write(dir.join(RELEASE_MANIFEST), "not ron").expect("write");
        assert_eq!(
            read_release(&dir).expect_err("junk").code,
            "release_manifest_invalid"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_manifest_cannot_list_a_path_outside_the_release() {
        let dir = regia_test_utils::unique_temp_dir("dist_release_escape");
        for rel in ["../secret", "crates/../../secret", "/etc/hostname", ""] {
            std::fs::write(
                dir.join(RELEASE_MANIFEST),
                format!(
                    "(format_version: 1, version: \"0.2.5\", commit: None, rust_version: \"\", crates: [], \
                     files: [(path: \"{rel}\", size: 0, blake3: \"\")])"
                ),
            )
            .expect("write");
            let report = verify_release(&dir);
            assert_eq!(report.problems[0].code, "release_manifest_invalid", "{rel:?}");
            assert_eq!(report.checked, 0, "{rel:?}: nothing was read");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The project path rule, because a release written on Linux gets verified on Windows.
    #[test]
    fn a_manifest_cannot_list_a_path_windows_reads_as_another_file() {
        let dir = regia_test_utils::unique_temp_dir("dist_release_windows");
        for rel in [
            "..\\\\secret",
            "a\\\\b",
            "C:\\\\x",
            "C:x",
            "\\\\\\\\server\\\\share",
            "a/.. /x",
            "a/NUL",
            "con.txt",
            "COM1",
            "file::$DATA",
            "name.",
            "name ",
        ] {
            std::fs::write(
                dir.join(RELEASE_MANIFEST),
                format!(
                    "(format_version: 1, version: \"0.2.5\", commit: None, rust_version: \"\", crates: [], \
                     files: [(path: \"{rel}\", size: 0, blake3: \"\")])"
                ),
            )
            .expect("write");
            let report = verify_release(&dir);
            assert_eq!(report.problems[0].code, "release_manifest_invalid", "{rel:?}");
            assert_eq!(report.checked, 0, "{rel:?}: nothing was read");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Audit ADR-3206 finding 7: a link replaced a listed file, or sat unlisted anywhere in the
    /// tree, and `verify` followed the first and did not see the second.
    #[cfg(unix)]
    #[test]
    fn links_in_a_release_are_errors_and_are_not_followed() {
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_links");
        let dir = package_release(&root, &out, false).expect("package").dir;
        assert!(verify_release(&dir).ok());

        // A listed file replaced by a link to identical bytes: the hash would match.
        let doc = dir.join("docs/a.md");
        let copy = out.join("a.md.outside");
        std::fs::copy(&doc, &copy).expect("copy");
        std::fs::remove_file(&doc).expect("rm");
        std::os::unix::fs::symlink(&copy, &doc).expect("link");
        let report = verify_release(&dir);
        assert_eq!(
            report.problems[0].code, "release_file_link",
            "{:?}",
            report.problems
        );
        assert!(!report.ok());
        std::fs::remove_file(&doc).expect("rm");
        std::fs::copy(&copy, &doc).expect("restore");
        assert!(verify_release(&dir).ok());

        // A listed folder replaced by a link to a copy of it.
        let docs = dir.join("docs");
        let moved = out.join("docs.outside");
        std::fs::rename(&docs, &moved).expect("move");
        std::os::unix::fs::symlink(&moved, &docs).expect("folder link");
        let report = verify_release(&dir);
        assert_eq!(
            report.problems[0].code, "release_file_link",
            "{:?}",
            report.problems
        );
        std::fs::remove_file(&docs).expect("rm");
        std::fs::rename(&moved, &docs).expect("restore");

        // An unlisted link: nothing else about the tree changed.
        std::os::unix::fs::symlink("/etc/hostname", dir.join("crates/regia/planted")).expect("planted");
        let report = verify_release(&dir);
        let codes: Vec<_> = report.problems.iter().map(|p| p.code).collect();
        assert_eq!(codes, vec!["release_file_link"]);
        assert!(!report.ok());
        for dir in [root, out] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// A listed path holding a FIFO (would hang), a folder, or a file of another size is `changed`
    /// without reading it.
    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_folder_in_a_files_place_is_changed_without_a_read() {
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_fifo");
        let dir = package_release(&root, &out, false).expect("package").dir;
        let doc = dir.join("docs/a.md");
        std::fs::remove_file(&doc).expect("rm");
        let made = Command::new("mkfifo")
            .arg(&doc)
            .status()
            .is_ok_and(|s| s.success());
        if made {
            assert_eq!(verify_release(&dir).problems[0].code, "release_file_changed");
            std::fs::remove_file(&doc).expect("rm");
        }
        std::fs::create_dir_all(&doc).expect("a folder at the name");
        assert_eq!(verify_release(&dir).problems[0].code, "release_file_changed");
        for dir in [root, out] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn a_manifest_that_lists_nothing_or_is_too_big_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("dist_release_empty");
        std::fs::write(
            dir.join(RELEASE_MANIFEST),
            "(format_version: 1, version: \"0.2.5\", commit: None, rust_version: \"\", crates: [], files: [])",
        )
        .expect("write");
        assert_eq!(verify_release(&dir).problems[0].code, "release_manifest_invalid");
        let big = std::fs::File::create(dir.join(RELEASE_MANIFEST)).expect("create");
        big.set_len(MAX_RELEASE_MANIFEST_BYTES + 1).expect("sparse");
        let report = verify_release(&dir);
        assert_eq!(report.problems[0].code, "release_manifest_invalid");
        assert!(
            report.problems[0].message.contains("larger than"),
            "{:?}",
            report.problems
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Runs git in `dir` with a fixed identity (the machine may have none).
    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(status.status.success(), "git {args:?}: {status:?}");
    }

    #[test]
    fn a_git_checkout_ships_its_tracked_files_and_names_its_commit() {
        let root = engine_tree();
        let out = regia_test_utils::unique_temp_dir("dist_release_git_out");
        git_in(&root, &["init", "-q"]);
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-q", "-m", "tag"]);
        std::fs::write(root.join("local_notes.txt"), "not tracked").expect("untracked");
        let head = git(&root, &["rev-parse", "HEAD"]).expect("head");
        let head = String::from_utf8_lossy(&head).trim().to_string();

        let packaged = package_release(&root, &out, false).expect("an untracked file is not dirty");
        assert_eq!(packaged.release.commit.as_deref(), Some(head.as_str()));
        assert!(!packaged.dir.join("local_notes.txt").exists());
        assert!(!packaged.release.files.iter().any(|f| f.path.starts_with(".git")));
        assert!(verify_release(&packaged.dir).ok());
        std::fs::remove_dir_all(&packaged.dir).expect("remove");

        std::fs::write(root.join("docs/a.md"), "# changed\n").expect("edit");
        assert_eq!(
            package_release(&root, &out, false).expect_err("dirty").code,
            "release_dirty"
        );
        assert!(!out.join("regia-engine-0.2.5").exists(), "nothing written");

        // Forced: the manifest must not claim the tree is that commit.
        let forced = package_release(&root, &out, true).expect("allow dirty");
        assert_eq!(forced.release.commit, Some(format!("{head}-dirty")));
        assert_eq!(
            std::fs::read_to_string(forced.dir.join("docs/a.md")).expect("doc"),
            "# changed\n"
        );
        assert!(verify_release(&forced.dir).ok());
        for dir in [root, out] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn a_tree_without_a_workspace_version_is_not_an_engine() {
        let dir = regia_test_utils::unique_temp_dir("dist_release_noroot");
        let out = regia_test_utils::unique_temp_dir("dist_release_noroot_out");
        assert_eq!(
            package_release(&dir, &out, false).expect_err("none").code,
            "engine_root_invalid"
        );
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n[workspace.package]\nversion = \"x\"\n",
        )
        .expect("write");
        assert_eq!(
            package_release(&dir, &out, false).expect_err("bad").code,
            "engine_root_invalid"
        );
        for d in [dir, out] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn checksum_lists_cannot_be_empty_or_repeat_a_file() {
        let dir = regia_test_utils::unique_temp_dir("dist_checksums_inventory");
        std::fs::write(dir.join(CHECKSUMS_FILE), "\n").expect("empty list");
        assert!(
            !verify_checksums(&dir).is_empty(),
            "empty checksum lists fail closed"
        );
        std::fs::write(dir.join("a.bin"), b"a").expect("file");
        let files = checksum_files(&dir, &["a.bin".into()]).expect("hash");
        let row = checksums_text(&files);
        std::fs::write(dir.join(CHECKSUMS_FILE), format!("{row}{row}")).expect("duplicate list");
        assert!(
            !verify_checksums(&dir).is_empty(),
            "duplicate checksum rows are invalid"
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// Corr-low (cli LOW): `checksum_files` wrote a repeated input name twice, a list that
    /// `verify_checksums` then rejects as `checksums_invalid`. A repeat is listed once.
    #[test]
    fn corr_low_a_repeated_input_is_listed_once_and_verifies() {
        let dir = regia_test_utils::unique_temp_dir("dist_checksums_repeat");
        std::fs::write(dir.join("a.bin"), b"a").expect("file");
        let files = checksum_files(&dir, &["a.bin".into(), "a.bin".into()]).expect("hash");
        assert_eq!(files.len(), 1, "{files:?}");
        std::fs::write(dir.join(CHECKSUMS_FILE), checksums_text(&files)).expect("list");
        assert!(verify_checksums(&dir).is_empty(), "{:?}", verify_checksums(&dir));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// Corr-low (cli LOW): a release manifest that exists but cannot be read (permission
    /// denied) was reported as `release_manifest_missing`; it is an `io_error` now.
    #[cfg(unix)]
    #[test]
    fn corr_low_an_unreadable_release_manifest_is_not_missing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = regia_test_utils::unique_temp_dir("dist_release_unreadable");
        let manifest = dir.join(RELEASE_MANIFEST);
        std::fs::write(&manifest, "(format_version: 1)").expect("manifest");
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // A runner with CAP_DAC_OVERRIDE (root) reads it anyway: nothing to check there.
        if std::fs::read(&manifest).is_err() {
            let err = read_release(&dir).expect_err("not readable");
            assert_eq!(err.code, "io_error", "{err:?}");
        }
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).expect("chmod back");
        let missing = regia_test_utils::unique_temp_dir("dist_release_missing");
        assert_eq!(
            read_release(&missing).expect_err("absent").code,
            "release_manifest_missing"
        );
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(missing).ok();
    }

    #[test]
    fn checksums_round_trip_and_catch_a_changed_file() {
        let dir = regia_test_utils::unique_temp_dir("dist_checksums");
        std::fs::create_dir_all(dir.join("sub")).expect("dir");
        std::fs::write(dir.join("a.bin"), b"alpha").expect("a");
        std::fs::write(dir.join("sub/b.bin"), b"beta").expect("b");
        let files = checksum_files(&dir, &["sub/b.bin".to_string(), "a.bin".to_string()]).expect("hash");
        assert_eq!(files[0].path, "a.bin", "sorted by path");
        let text = checksums_text(&files);
        assert!(text.starts_with(&format!("{}  a.bin\n", blake3::hash(b"alpha").to_hex())));
        std::fs::write(dir.join(CHECKSUMS_FILE), &text).expect("list");
        assert!(verify_checksums(&dir).is_empty());
        std::fs::write(dir.join("a.bin"), b"alpHa").expect("change");
        std::fs::remove_file(dir.join("sub/b.bin")).expect("remove");
        let codes: Vec<&str> = verify_checksums(&dir).iter().map(|p| p.code).collect();
        assert_eq!(codes, ["checksum_mismatch", "checksum_file_missing"]);
    }

    #[test]
    fn checksums_refuse_paths_outside_the_root_and_malformed_lists() {
        let dir = regia_test_utils::unique_temp_dir("dist_checksums_bad");
        for bad in ["", "../x", "/etc/passwd", "a//b", "./a", "C:/x", "a\\b"] {
            let err = checksum_files(&dir, &[bad.to_string()]).expect_err(bad);
            assert_eq!(err.code, "checksum_path_invalid", "{bad:?}");
        }
        assert_eq!(verify_checksums(&dir)[0].code, "checksums_missing");
        std::fs::write(
            dir.join(CHECKSUMS_FILE),
            format!("nothex  a\n{}  ../escape\n", "0".repeat(64)),
        )
        .expect("list");
        let codes: Vec<&str> = verify_checksums(&dir).iter().map(|p| p.code).collect();
        assert_eq!(codes, ["checksums_invalid", "checksums_invalid"]);
    }

    /// A checksum list is checked with the same path rule as a release manifest, and a listed
    /// path is never followed: `.../secret` and `nul.txt` are parents and devices on Windows,
    /// and a symlink whose target happens to hash correctly is not the file the list names.
    #[cfg(unix)]
    #[test]
    fn checksum_verify_refuses_unsafe_paths_links_and_special_files() {
        let dir = regia_test_utils::unique_temp_dir("dist_checksums_guard");
        let hash = "ab".repeat(32);
        std::fs::write(
            dir.join(CHECKSUMS_FILE),
            format!("{hash}  .../secret\n{hash}  nul.txt\n{hash}  .. /x\n"),
        )
        .expect("list");
        let codes: Vec<&str> = verify_checksums(&dir).iter().map(|p| p.code).collect();
        assert_eq!(
            codes,
            ["checksums_invalid", "checksums_invalid", "checksums_invalid"],
            "windows-stripped parents and device names are unsafe paths, not missing files: {codes:?}"
        );
        for bad in [".../secret", "nul.txt", ".. /x", "a/. ./x", "CON", "file.txt "] {
            let err = checksum_files(&dir, &[bad.to_string()]).expect_err(bad);
            assert_eq!(err.code, "checksum_path_invalid", "{bad}: {err}");
        }

        let outside = regia_test_utils::unique_temp_dir("dist_checksums_outside");
        std::fs::write(outside.join("secret"), b"secret-bytes").expect("outside");
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("a.bin")).expect("link");
        let digest = blake3::hash(b"secret-bytes").to_hex().to_string();
        std::fs::write(dir.join(CHECKSUMS_FILE), format!("{digest}  a.bin\n")).expect("list");
        let problems = verify_checksums(&dir);
        assert!(
            problems.iter().any(|p| p.code == "checksum_file_link"),
            "a symlink whose target matches the listed hash must not pass: {problems:?}"
        );
        let err = checksum_files(&dir, &["a.bin".into()]).expect_err("link");
        assert_eq!(err.code, "io_error", "{err}");
        assert!(err.message.contains("symbolic link"), "{err}");

        std::fs::remove_file(dir.join("a.bin")).expect("unlink");
        std::fs::create_dir(dir.join("a.bin")).expect("dir in a file's place");
        let problems = verify_checksums(&dir);
        assert!(
            problems
                .iter()
                .any(|p| p.code == "checksum_mismatch" && p.message.contains("not a regular file")),
            "{problems:?}"
        );
        let err = checksum_files(&dir, &["a.bin".into()]).expect_err("directory");
        assert_eq!(err.code, "io_error", "{err}");

        std::fs::remove_dir(dir.join("a.bin")).expect("rmdir");
        let fifo = dir.join("a.bin");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success(),
            "mkfifo"
        );
        let problems = verify_checksums(&dir);
        assert!(
            problems
                .iter()
                .any(|p| p.code == "checksum_mismatch" && p.message.contains("not a regular file")),
            "a fifo must be refused without opening it: {problems:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
