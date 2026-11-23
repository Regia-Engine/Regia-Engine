// SPDX-License-Identifier: MPL-2.0
//! Shared cook cache (ADR-2612): cooked outputs shared by every machine of a team.
//!
//! The ADR-530 key (`cook_inputs`) covers everything an importer reads, but it also holds the
//! running cooker's executable size and modification time, and the outputs live in one machine's
//! `.regia/cooked/<platform>/`. This module stores cooked outputs in a content-addressed cache that
//! many machines share (a local folder or a mounted network share), keyed by the same inputs with
//! the machine-local executable stamp replaced by an [`engine_build_id`] that matches on every
//! machine running the same engine build.
//!
//! **Entry.** One file per key, `<root>/v1/<aa>/<key hex>.rcc`: the magic `RGCC`, the entry
//! version ([`ENTRY_VERSION`]), the header's length and blake3, a postcard header (the key,
//! platform, source path, the cook outcome and every cooked file's name, length and blake3) and
//! the files' bytes. A reader verifies all of
//! it ([`verify_entry`]); a corrupt or partial entry is a warning and a miss, never an output.
//!
//! **Writes** go to a uniquely named temp file beside the entry and are renamed over it, so a
//! reader sees a whole entry or none, and two writers of one key both succeed and leave one entry.
//! A read-only cache never creates, touches or replaces a file.
//!
//! The approach is the one build tools use for remote and shared compiler caches: an action's
//! output stored under the hash of all its inputs (public documentation of content-addressed
//! build caches, e.g. the ccache manual's "cache sharing" and the Bazel remote-caching docs; git's
//! object store for content addressing). Original code.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use regia_asset::{AssetId, ManifestEntry, cooked_file_name};
use serde::{Deserialize, Serialize};

use crate::cook::CookOutcome;
use crate::cook_inputs::{CookInputs, SourceInputs};

mod http;
pub use http::{COOK_CACHE_HTTPS_DISABLED_CODE, HttpBackend, SHARED_CACHE_AUTH_ENV};

/// Version of the shared entry layout (`.rcc`). An entry of another version is a miss.
pub const ENTRY_VERSION: u32 = 1;

/// The first four bytes of every entry.
const MAGIC: &[u8; 4] = b"RGCC";

/// The fixed part of an entry: magic, version, header length, the header's blake3.
const FIXED_LEN: usize = 12 + 32;

/// The directory under the cache root holding version-1 entries.
const LAYOUT_DIR: &str = "v1";

/// The file extension of an entry.
const ENTRY_EXT: &str = "rcc";

/// Temp files older than this are removed by [`prune`] (a writer killed mid-write).
pub const STALE_TEMP_AGE: Duration = Duration::from_secs(3600);

/// The environment variable that replaces the `Regia.toml` path of the shared cache on one machine
/// (an artist's mount point differs from the build host's).
pub const SHARED_CACHE_ENV: &str = "REGIA_SHARED_COOK_CACHE";

/// Whether a cook may write to the shared cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedCacheMode {
    /// Fetch entries; never create, touch or replace a file in the cache.
    #[default]
    Read,
    /// Fetch entries and publish what this machine cooks.
    ReadWrite,
}

impl SharedCacheMode {
    /// The `Regia.toml` and agent label: `read` or `read_write`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }

    /// Parses a label ([`Self::label`]).
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "read" => Some(Self::Read),
            "read_write" => Some(Self::ReadWrite),
            _ => None,
        }
    }
}

/// Where the shared cache is and how a cook uses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedCacheConfig {
    /// The cache root: a local folder or a mounted network share.
    pub path: PathBuf,
    /// Read only, or read and write.
    pub mode: SharedCacheMode,
    /// Replaces [`engine_build_id`] (tests, and tools that cook with a known build); leave `None`.
    pub engine_build_id: Option<String>,
}

impl SharedCacheConfig {
    /// A cache at `path` used in `mode`, keyed by this engine build.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, mode: SharedCacheMode) -> Self {
        Self {
            path: path.into(),
            mode,
            engine_build_id: None,
        }
    }

    /// The project's shared cache: `Regia.toml` `[cook.shared_cache]`, its path replaced by the
    /// [`SHARED_CACHE_ENV`] variable when that is set. `None` when neither names a path (the
    /// default: off).
    ///
    /// # Errors
    /// `Regia.toml` does not parse or `[cook.shared_cache]` holds an unknown key or a bad value.
    pub fn for_project(project_root: &Path) -> Result<Option<Self>, String> {
        let manifest = match std::fs::read_to_string(project_root.join("Regia.toml")) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Regia.toml: {e}")),
        };
        Self::resolve(
            project_root,
            manifest.as_deref(),
            std::env::var_os(SHARED_CACHE_ENV),
        )
    }

    /// [`Self::for_project`] with the manifest text and the environment value given.
    ///
    /// # Errors
    /// As [`Self::for_project`].
    pub fn resolve(
        project_root: &Path,
        manifest: Option<&str>,
        env_path: Option<OsString>,
    ) -> Result<Option<Self>, String> {
        let (mut path, mut mode) = (None, SharedCacheMode::Read);
        if let Some(text) = manifest {
            let root: toml::Table = text.parse().map_err(|e| format!("Regia.toml: {e}"))?;
            if let Some(table) = root
                .get("cook")
                .and_then(toml::Value::as_table)
                .and_then(|cook| cook.get("shared_cache"))
            {
                let table = table
                    .as_table()
                    .ok_or("Regia.toml: [cook.shared_cache] must be a table")?;
                for key in table.keys() {
                    if key != "path" && key != "mode" {
                        return Err(format!(
                            "Regia.toml: [cook.shared_cache] has unknown key `{key}` (known: path, mode)"
                        ));
                    }
                }
                match table.get("path") {
                    None => {}
                    Some(toml::Value::String(p)) if !p.trim().is_empty() => {
                        path = Some(PathBuf::from(p.trim()))
                    }
                    Some(_) => {
                        return Err("Regia.toml: [cook.shared_cache] path must be a non-empty string".into());
                    }
                }
                match table.get("mode") {
                    None => {}
                    Some(toml::Value::String(m)) => {
                        mode = SharedCacheMode::from_label(m).ok_or_else(|| {
                            format!(
                                "Regia.toml: [cook.shared_cache] mode `{m}` is not `read` or `read_write`"
                            )
                        })?;
                    }
                    Some(_) => return Err("Regia.toml: [cook.shared_cache] mode must be a string".into()),
                }
            }
        }
        if let Some(env) = env_path.filter(|v| !v.is_empty()) {
            path = Some(PathBuf::from(env));
        }
        Ok(path.map(|path| {
            let path = if path.is_relative() && !is_url(&path) {
                project_root.join(path)
            } else {
                path
            };
            Self::new(path, mode)
        }))
    }
}

/// Whether a configured cache path is a URL (`http://`, `https://`) rather than a directory.
fn is_url(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("http://") || text.starts_with("https://")
}

/// Which shared cache a cook uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SharedCacheChoice {
    /// The project's (`Regia.toml` `[cook.shared_cache]`, [`SHARED_CACHE_ENV`]); off when unset.
    #[default]
    Project,
    /// None, whatever the project says.
    Off,
    /// This one.
    Use(SharedCacheConfig),
}

/// What a cook did with the shared cache (in [`crate::CookReport`], the cook ledger and
/// `cook.cache_stats`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedCacheSummary {
    /// The cache root as configured.
    pub location: String,
    /// `read` or `read_write`.
    pub mode: String,
    /// The engine build id the keys used.
    pub engine_build_id: String,
    /// Sources whose cooked outputs came from the cache (no import).
    pub hits: u64,
    /// Lookups that found no usable entry (the source was imported).
    pub misses: u64,
    /// Of the misses, entries found corrupt or partial.
    pub corrupt: u64,
    /// Entries this cook wrote to the cache.
    pub published: u64,
    /// Cache reads or writes that failed (the cook went on locally).
    pub errors: u64,
    /// Entry bytes read from the cache.
    pub bytes_read: u64,
    /// Entry bytes written to the cache.
    pub bytes_written: u64,
}

impl SharedCacheSummary {
    /// One line for the end of `regia cook`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "shared cache {} ({}): {} hits, {} misses ({} corrupt), {} published, {} errors, {:.1} MiB read, {:.1} MiB written",
            self.location,
            self.mode,
            self.hits,
            self.misses,
            self.corrupt,
            self.published,
            self.errors,
            self.bytes_read as f64 / (1024.0 * 1024.0),
            self.bytes_written as f64 / (1024.0 * 1024.0),
        )
    }
}

/// The identity of the running engine build in shared keys: the pipeline version, the target
/// architecture and OS, and the `REGIA_ENGINE_BUILD_ID` the engine was compiled with (a release or
/// CI build stamps its commit), else the blake3 of the running executable's bytes. `None` when the
/// executable cannot be read (the shared cache is then off: an unknown build must never match).
#[must_use]
pub fn engine_build_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        let stamp = match option_env!("REGIA_ENGINE_BUILD_ID").map(str::trim) {
            Some(id) if !id.is_empty() => format!("build:{id}"),
            _ => {
                let exe = std::env::current_exe().ok()?;
                let mut file = std::fs::File::open(exe).ok()?;
                let mut hasher = blake3::Hasher::new();
                io::copy(&mut file, &mut hasher).ok()?;
                format!("exe:{}", &hasher.finalize().to_hex()[..32])
            }
        };
        Some(format!(
            "regia_asset_pipeline {} {}-{} {stamp}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
            std::env::consts::OS,
        ))
    })
    .as_deref()
}

/// The shared cook cache summary of the last cook of the platform cache `cache_dir` that used
/// one, from its cook ledger (`cook_keys.bin`).
#[must_use]
pub fn last_summary(cache_dir: &Path) -> Option<SharedCacheSummary> {
    crate::cook_ledger::last_shared_summary(cache_dir)
}

/// The JSON copy of the last summary in a platform's cooked cache directory (beside the cook
/// ledger, never packed), for readers without the pipeline (the agent's `cook.cache_stats`).
pub const SUMMARY_FILE: &str = "shared_cache_summary.json";

/// Writes [`SUMMARY_FILE`] (temp file + rename); a failure is a warning.
pub(crate) fn write_summary_json(cache_dir: &Path, summary: &SharedCacheSummary, warnings: &mut Vec<String>) {
    let written = serde_json::to_vec_pretty(summary)
        .map_err(io::Error::other)
        .and_then(|bytes| crate::cook_ledger::write_atomic(&cache_dir.join(SUMMARY_FILE), &bytes));
    if let Err(e) = written {
        warnings.push(format!("shared cook cache summary not written: {e}"));
    }
}

/// One entry of a cache ([`CookCacheBackend::list`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntryInfo {
    /// The shared key.
    pub key: [u8; 32],
    /// Entry size.
    pub bytes: u64,
    /// Last write or hit (read-write caches touch an entry on every hit).
    pub last_used: SystemTime,
}

/// Storage of shared entries. Implementations must make [`Self::put`] atomic (a reader sees the
/// whole entry or none) and safe for concurrent writers of one key.
pub trait CookCacheBackend: Send + Sync + std::fmt::Debug {
    /// Where the cache is, for messages.
    fn location(&self) -> String;
    /// The entry of `key`, or `None` when there is none.
    ///
    /// # Errors
    /// The cache could not be read.
    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>>;
    /// Whether an entry of `key` exists (not verified).
    ///
    /// # Errors
    /// The cache could not be read.
    fn contains(&self, key: &[u8; 32]) -> io::Result<bool>;
    /// Stores `bytes` as the entry of `key`, replacing any.
    ///
    /// # Errors
    /// The cache could not be written.
    fn put(&self, key: &[u8; 32], bytes: &[u8]) -> io::Result<()>;
    /// Marks the entry of `key` used now (for least-recently-used pruning).
    ///
    /// # Errors
    /// The entry could not be touched.
    fn touch(&self, key: &[u8; 32]) -> io::Result<()>;
    /// Removes the entry of `key` (no error when there is none).
    ///
    /// # Errors
    /// The entry could not be removed.
    fn remove(&self, key: &[u8; 32]) -> io::Result<()>;
    /// Every entry.
    ///
    /// # Errors
    /// The cache could not be listed.
    fn list(&self) -> io::Result<Vec<CacheEntryInfo>>;
    /// Removes unfinished writes older than `age`; returns how many files and bytes.
    ///
    /// # Errors
    /// The cache could not be listed.
    fn remove_stale_temps(&self, age: Duration) -> io::Result<(u64, u64)> {
        let _ = age;
        Ok((0, 0))
    }
}

/// A shared cache in a directory: a local folder or a mounted network share.
#[derive(Clone, Debug)]
pub struct DirBackend {
    root: PathBuf,
}

impl DirBackend {
    /// The cache at `root`, which must exist in either mode: a share that is not mounted must
    /// not fill the local disk under its mount point. A writable cache creates its layout
    /// directory inside `root`.
    ///
    /// # Errors
    /// `root` is not a reachable directory, or (read-write) its layout cannot be created.
    pub fn open(root: &Path, mode: SharedCacheMode) -> io::Result<Self> {
        if !std::fs::metadata(root)?.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a directory"));
        }
        if mode == SharedCacheMode::ReadWrite {
            match std::fs::create_dir(root.join(LAYOUT_DIR)) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn layout(&self) -> PathBuf {
        self.root.join(LAYOUT_DIR)
    }

    /// The file of `key`'s entry.
    #[must_use]
    pub fn entry_path(&self, key: &[u8; 32]) -> PathBuf {
        let hex = hex(key);
        self.layout().join(&hex[..2]).join(format!("{hex}.{ENTRY_EXT}"))
    }

    /// Every shard directory.
    fn shards(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        match std::fs::read_dir(self.layout()) {
            Ok(dir) => {
                for entry in dir.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        out.push(entry.path());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        out.sort();
        Ok(out)
    }
}

impl CookCacheBackend for DirBackend {
    fn location(&self) -> String {
        self.root.display().to_string()
    }

    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.entry_path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn contains(&self, key: &[u8; 32]) -> io::Result<bool> {
        match std::fs::metadata(self.entry_path(key)) {
            Ok(meta) => Ok(meta.is_file()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn put(&self, key: &[u8; 32], bytes: &[u8]) -> io::Result<()> {
        let path = self.entry_path(key);
        let shard = path
            .parent()
            .ok_or_else(|| io::Error::other("entry path has no parent"))?;
        std::fs::create_dir_all(shard)?;
        // A uuid, not the pid: machines sharing the directory have overlapping pids.
        let tmp = shard.join(format!(".{}.{}.tmp", hex(key), uuid::Uuid::new_v4().simple()));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                // Another writer's rename won (Windows refuses to replace a file a reader holds
                // open): its entry has the same inputs. Not when the file there is the corrupt
                // entry this put replaces.
                let whole =
                    std::fs::read(&path).is_ok_and(|old| verify_entry(&old).is_ok_and(|e| e.key == *key));
                if whole { Ok(()) } else { Err(e) }
            }
        }
    }

    fn touch(&self, key: &[u8; 32]) -> io::Result<()> {
        std::fs::File::options()
            .write(true)
            .open(self.entry_path(key))?
            .set_modified(SystemTime::now())
    }

    fn remove(&self, key: &[u8; 32]) -> io::Result<()> {
        match std::fs::remove_file(self.entry_path(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn list(&self) -> io::Result<Vec<CacheEntryInfo>> {
        let mut out = Vec::new();
        for shard in self.shards()? {
            let Ok(dir) = std::fs::read_dir(&shard) else {
                continue;
            };
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Some(key) = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(&format!(".{ENTRY_EXT}")))
                    .and_then(parse_hex)
                    // Only a file where `entry_path` puts it: `remove` and `prune` act on that.
                    .filter(|key| self.entry_path(key) == entry.path())
                else {
                    continue;
                };
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                out.push(CacheEntryInfo {
                    key,
                    bytes: meta.len(),
                    last_used: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
        Ok(out)
    }

    fn remove_stale_temps(&self, age: Duration) -> io::Result<(u64, u64)> {
        let now = SystemTime::now();
        let (mut files, mut bytes) = (0, 0);
        for shard in self.shards()? {
            let Ok(dir) = std::fs::read_dir(&shard) else {
                continue;
            };
            for entry in dir.flatten() {
                if !entry.file_name().to_string_lossy().ends_with(".tmp") {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let old = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .is_some_and(|d| d >= age);
                if old && std::fs::remove_file(entry.path()).is_ok() {
                    files += 1;
                    bytes += meta.len();
                }
            }
        }
        Ok((files, bytes))
    }
}

/// The backend of `config`: an [`HttpBackend`] for an `http://` or `https://` URL (ADR-2710:
/// `https://` through ureq + rustls, [`SHARED_CACHE_AUTH_ENV`] as the `Authorization` header),
/// else a [`DirBackend`].
///
/// # Errors
/// The directory is not reachable, or the URL is not usable (the message says why and never holds
/// the auth value). An `https://` URL in a build without the `http_cache` feature starts with
/// [`COOK_CACHE_HTTPS_DISABLED_CODE`] (ADR-7972).
pub fn open_backend(config: &SharedCacheConfig) -> Result<Box<dyn CookCacheBackend>, String> {
    if is_url(&config.path) {
        return HttpBackend::new(&config.path.to_string_lossy())
            .map(|b| Box::new(b) as Box<dyn CookCacheBackend>);
    }
    DirBackend::open(&config.path, config.mode)
        .map(|b| Box::new(b) as Box<dyn CookCacheBackend>)
        .map_err(|e| {
            format!(
                "shared cook cache {} is not reachable: {e}",
                config.path.display()
            )
        })
}

/// Entry count and bytes of a cache.
///
/// # Errors
/// The cache could not be listed.
pub fn cache_totals(backend: &dyn CookCacheBackend) -> io::Result<(u64, u64)> {
    let entries = backend.list()?;
    Ok((entries.len() as u64, entries.iter().map(|e| e.bytes).sum()))
}

/// What [`prune`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    /// Entries before.
    pub entries_before: u64,
    /// Bytes before.
    pub bytes_before: u64,
    /// Entries removed.
    pub removed: u64,
    /// Bytes removed.
    pub bytes_removed: u64,
    /// Unfinished writes removed (older than [`STALE_TEMP_AGE`]).
    pub stale_temps_removed: u64,
    /// Entries that could not be removed (held open by a reader on Windows, permissions).
    pub failed: u64,
}

/// Removes least-recently-used entries until the cache holds at most `max_bytes`, and unfinished
/// writes older than [`STALE_TEMP_AGE`]. Ties in last use go by key, so a prune is deterministic.
///
/// # Errors
/// The cache could not be listed.
pub fn prune(backend: &dyn CookCacheBackend, max_bytes: u64) -> io::Result<PruneReport> {
    let mut entries = backend.list()?;
    let mut report = PruneReport {
        entries_before: entries.len() as u64,
        bytes_before: entries.iter().map(|e| e.bytes).sum(),
        ..PruneReport::default()
    };
    report.stale_temps_removed = backend.remove_stale_temps(STALE_TEMP_AGE)?.0;
    entries.sort_by(|a, b| a.last_used.cmp(&b.last_used).then(a.key.cmp(&b.key)));
    let mut total = report.bytes_before;
    for entry in entries {
        if total <= max_bytes {
            break;
        }
        match backend.remove(&entry.key) {
            Ok(()) => {
                total -= entry.bytes;
                report.removed += 1;
                report.bytes_removed += entry.bytes;
            }
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

/// Parses a size: bytes, or a number with `K`, `M`, `G` or `T` (binary, `KiB`/`KB`/`k` alike).
///
/// # Errors
/// Not a size.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let digits = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(digits);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("`{text}` is not a size (e.g. 500G, 2T, 1048576)"))?;
    let shift = match unit
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches("ib")
        .trim_end_matches('b')
    {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => return Err(format!("`{text}`: unknown size unit (use K, M, G or T)")),
    };
    let bytes = number * (1u64 << shift) as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("`{text}` is out of range"));
    }
    Ok(bytes as u64)
}

// --- Entry format ---------------------------------------------------------------------------

/// One cooked file in an entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EntryFile {
    name: String,
    len: u64,
    hash: [u8; 32],
}

/// The header of an entry (postcard).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EntryHeader {
    key: [u8; 32],
    platform: String,
    rel_path: String,
    primary_id: AssetId,
    type_path: String,
    source_hash: [u8; 32],
    settings_hash: [u8; 32],
    importer: String,
    importer_version: u32,
    cache_key: [u8; 32],
    cooked_rel: String,
    sub_manifest: Vec<ManifestEntry>,
    files: Vec<EntryFile>,
}

/// A verified entry: its header and each file's bytes, in the header's file order.
struct Entry<'a> {
    header: EntryHeader,
    files: Vec<&'a [u8]>,
}

/// What [`verify_entry`] found in a valid entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrySummary {
    /// The shared key the entry says it holds.
    pub key: [u8; 32],
    /// Platform tag.
    pub platform: String,
    /// Project-relative source path.
    pub rel_path: String,
    /// Cooked files.
    pub files: usize,
    /// Cooked bytes.
    pub bytes: u64,
}

/// The cooked file names an outcome owns: the primary and each sub-asset, without repeats.
fn output_names(primary_id: AssetId, sub_manifest: &[ManifestEntry]) -> BTreeSet<String> {
    std::iter::once(primary_id)
        .chain(sub_manifest.iter().map(|s| s.id))
        .map(cooked_file_name)
        .collect()
}

/// Encodes an entry of `outcome` whose files are `files` (name, bytes).
fn encode_entry(
    key: &[u8; 32],
    platform: &str,
    rel_path: &str,
    outcome: &CookOutcome,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let header = EntryHeader {
        key: *key,
        platform: platform.to_string(),
        rel_path: rel_path.to_string(),
        primary_id: outcome.primary_id,
        type_path: outcome.type_path.clone(),
        source_hash: outcome.source_hash,
        settings_hash: outcome.settings_hash,
        importer: outcome.importer.clone(),
        importer_version: outcome.importer_version,
        cache_key: outcome.cache_key,
        cooked_rel: outcome.cooked_rel.clone(),
        sub_manifest: outcome.sub_manifest.clone(),
        files: files
            .iter()
            .map(|(name, bytes)| EntryFile {
                name: name.clone(),
                len: bytes.len() as u64,
                hash: *blake3::hash(bytes).as_bytes(),
            })
            .collect(),
    };
    let head = postcard::to_allocvec(&header).map_err(|e| e.to_string())?;
    let head_len = u32::try_from(head.len()).map_err(|_| "entry header too large".to_string())?;
    let body: usize = files.iter().map(|(_, b)| b.len()).sum();
    let mut out = Vec::with_capacity(FIXED_LEN + head.len() + body);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&ENTRY_VERSION.to_le_bytes());
    out.extend_from_slice(&head_len.to_le_bytes());
    out.extend_from_slice(blake3::hash(&head).as_bytes());
    out.extend_from_slice(&head);
    for (_, bytes) in files {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// Decodes and verifies an entry: magic, version, header, the file names (exactly the outcome's
/// cooked files), lengths and every hash.
fn decode_entry(bytes: &[u8]) -> Result<Entry<'_>, String> {
    let fixed = bytes
        .get(..FIXED_LEN)
        .ok_or("entry shorter than its fixed header")?;
    if &fixed[..4] != MAGIC {
        return Err("not a shared cook cache entry (bad magic)".into());
    }
    let word = |at: usize| u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]]);
    let version = word(4);
    if version != ENTRY_VERSION {
        return Err(format!(
            "entry version {version}, this engine reads {ENTRY_VERSION}"
        ));
    }
    let head_len = word(8) as usize;
    let head_end = FIXED_LEN
        .checked_add(head_len)
        .filter(|end| *end <= bytes.len())
        .ok_or("entry header runs past the end (truncated)")?;
    let head = &bytes[FIXED_LEN..head_end];
    // The header carries the outcome's database fields, which no file hash covers.
    if blake3::hash(head).as_bytes() != &fixed[12..FIXED_LEN] {
        return Err("entry header fails its hash".into());
    }
    let header: EntryHeader =
        postcard::from_bytes(head).map_err(|e| format!("entry header does not decode: {e}"))?;
    let sub_prefix = format!("{}#", header.rel_path);
    if header
        .sub_manifest
        .iter()
        .any(|m| !m.path.starts_with(&sub_prefix))
    {
        return Err("entry sub-asset rows name another source".into());
    }
    if header.cooked_rel != cooked_file_name(header.primary_id) {
        return Err("entry primary file name does not match its id".into());
    }
    let expected = output_names(header.primary_id, &header.sub_manifest);
    let named: BTreeSet<&str> = header.files.iter().map(|f| f.name.as_str()).collect();
    if named.len() != header.files.len() || named != expected.iter().map(String::as_str).collect() {
        return Err("entry files are not exactly the cooked outputs it names".into());
    }
    let mut at = head_end;
    let mut files = Vec::with_capacity(header.files.len());
    for file in &header.files {
        let len = usize::try_from(file.len).map_err(|_| "entry file too large")?;
        let end = at
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or("entry is truncated")?;
        let data = &bytes[at..end];
        if blake3::hash(data).as_bytes() != &file.hash {
            return Err(format!("entry file {} fails its hash", file.name));
        }
        at = end;
        files.push(data);
    }
    if at != bytes.len() {
        return Err("entry has trailing bytes".into());
    }
    Ok(Entry { header, files })
}

/// Verifies an entry's bytes (the fuzz and `regia cook cache` surface): `Ok` with what it holds,
/// else why it is corrupt.
///
/// # Errors
/// The bytes are not a whole, valid entry of [`ENTRY_VERSION`].
pub fn verify_entry(bytes: &[u8]) -> Result<EntrySummary, String> {
    let entry = decode_entry(bytes)?;
    Ok(EntrySummary {
        key: entry.header.key,
        platform: entry.header.platform.clone(),
        rel_path: entry.header.rel_path.clone(),
        files: entry.files.len(),
        bytes: entry.files.iter().map(|d| d.len() as u64).sum(),
    })
}

/// A valid entry (for fuzzing and tests): one 16-byte cooked file.
#[must_use]
pub fn sample_entry() -> Vec<u8> {
    let id = AssetId::sub_asset(AssetId::default(), "sample");
    let outcome = CookOutcome {
        primary_id: id,
        type_path: "regia_asset::Sample".into(),
        source_hash: [1; 32],
        settings_hash: [2; 32],
        importer: "png".into(),
        importer_version: 1,
        cache_key: [3; 32],
        cooked_rel: cooked_file_name(id),
        skipped: false,
        sub_manifest: Vec::new(),
    };
    let files = vec![(cooked_file_name(id), b"sixteen bytes!!!".to_vec())];
    encode_entry(&[4; 32], "linux", "assets/sample.png", &outcome, &files).unwrap_or_default()
}

fn hex(key: &[u8; 32]) -> String {
    use std::fmt::Write;
    key.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn parse_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// --- The cook's handle ----------------------------------------------------------------------

/// Cache reads or writes that may fail before the cook stops using the cache (one warning, then
/// local cooking; a share that went away must not cost a timeout per source).
const MAX_ERRORS: u64 = 3;

/// Revision of the published-keys file (`shared-cook-<platform>.bin`, a cache file).
const KNOWN_REVISION: u32 = 1;

/// The keys this machine knows the cache holds (it published or fetched them), so a read-write
/// cook asks the cache about a locally skipped source once.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownFile {
    revision: u32,
    location: String,
    keys: BTreeSet<[u8; 32]>,
}

/// The shared cache attached to one cook ([`attach`]).
#[derive(Debug)]
pub(crate) struct SharedCookCache {
    backend: Box<dyn CookCacheBackend>,
    read_only: bool,
    build_id: String,
    platform: String,
    summary: SharedCacheSummary,
    warnings: Vec<String>,
    /// Off for the rest of the cook after [`MAX_ERRORS`].
    stopped: bool,
    known_path: PathBuf,
    known: KnownFile,
    known_dirty: bool,
}

impl SharedCookCache {
    /// Whether the cook still uses this cache (it stops after `MAX_ERRORS` IO errors).
    pub(crate) fn active(&self) -> bool {
        !self.stopped
    }

    fn new(
        backend: Box<dyn CookCacheBackend>,
        mode: SharedCacheMode,
        build_id: String,
        project_root: &Path,
        platform: &str,
    ) -> Self {
        let location = backend.location();
        let known_path = crate::cook_inputs::inputs_path(project_root, platform)
            .with_file_name(format!("shared-cook-{platform}.bin"));
        let known = std::fs::read(&known_path)
            .ok()
            .and_then(|b| postcard::from_bytes::<KnownFile>(&b).ok())
            .filter(|k| k.revision == KNOWN_REVISION && k.location == location)
            .unwrap_or_else(|| KnownFile {
                revision: KNOWN_REVISION,
                location: location.clone(),
                keys: BTreeSet::new(),
            });
        Self {
            backend,
            read_only: mode == SharedCacheMode::Read,
            summary: SharedCacheSummary {
                location,
                mode: mode.label().to_string(),
                engine_build_id: build_id.clone(),
                ..SharedCacheSummary::default()
            },
            build_id,
            platform: platform.to_string(),
            warnings: Vec::new(),
            stopped: false,
            known_path,
            known,
            known_dirty: false,
        }
    }

    fn warn(&mut self, message: String) {
        tracing::warn!(target: "regia_asset_pipeline", "{message}");
        self.warnings.push(message);
    }

    fn io_error(&mut self, what: &str, rel_path: &str, error: &io::Error) {
        self.summary.errors += 1;
        self.warn(format!(
            "shared cook cache {}: {what} {rel_path} failed: {error}",
            self.summary.location
        ));
        if self.summary.errors >= MAX_ERRORS && !self.stopped {
            self.stopped = true;
            self.warn(format!(
                "shared cook cache {}: {MAX_ERRORS} errors; cooking the rest without it",
                self.summary.location
            ));
        }
    }

    fn remember(&mut self, key: [u8; 32]) {
        self.known_dirty |= self.known.keys.insert(key);
    }

    /// The outputs of `key` written into `cache_dir`, or `None` (a miss; corrupt entries warn).
    fn fetch(
        &mut self,
        key: &[u8; 32],
        src: &SourceInputs<'_>,
        source_hash: &[u8; 32],
        cache_dir: &Path,
    ) -> Option<CookOutcome> {
        let rel_path = src.rel_path;
        let bytes = match self.backend.get(key) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.summary.misses += 1;
                return None;
            }
            Err(e) => {
                self.summary.misses += 1;
                self.io_error("reading the entry of", rel_path, &e);
                return None;
            }
        };
        self.summary.bytes_read += bytes.len() as u64;
        let entry = match decode_entry(&bytes) {
            Ok(entry)
                if &entry.header.key == key
                    && entry.header.platform == self.platform
                    && entry.header.rel_path == rel_path
                    && &entry.header.source_hash == source_hash
                    && entry.header.importer_version == src.importer_version =>
            {
                entry
            }
            Ok(_) => {
                return self.corrupt(
                    rel_path,
                    "it holds another key, platform, source, source hash or importer version",
                );
            }
            Err(why) => return self.corrupt(rel_path, &why),
        };
        for (file, data) in entry.header.files.iter().zip(&entry.files) {
            if let Err(e) = crate::cook_ledger::write_atomic(&cache_dir.join(&file.name), data) {
                self.summary.misses += 1;
                self.io_error("writing the cooked outputs of", rel_path, &e);
                return None;
            }
        }
        self.summary.hits += 1;
        if !self.read_only {
            // Recency for pruning; a failure only makes the entry look older.
            let _ = self.backend.touch(key);
        }
        self.remember(*key);
        let h = entry.header;
        Some(CookOutcome {
            primary_id: h.primary_id,
            type_path: h.type_path,
            source_hash: h.source_hash,
            settings_hash: h.settings_hash,
            importer: h.importer,
            importer_version: h.importer_version,
            cache_key: h.cache_key,
            cooked_rel: h.cooked_rel,
            skipped: false,
            sub_manifest: h.sub_manifest,
        })
    }

    fn corrupt(&mut self, rel_path: &str, why: &str) -> Option<CookOutcome> {
        self.summary.misses += 1;
        self.summary.corrupt += 1;
        let action = if self.read_only {
            "importing it"
        } else {
            "importing it and replacing the entry"
        };
        self.warn(format!(
            "shared cook cache {}: the entry of {rel_path} is corrupt or partial ({why}); {action}",
            self.summary.location
        ));
        None
    }

    /// Publishes the outputs of `outcome` (in `cache_dir`) under `key`.
    fn publish(&mut self, key: &[u8; 32], rel_path: &str, outcome: &CookOutcome, cache_dir: &Path) {
        if outcome.cooked_rel != cooked_file_name(outcome.primary_id) {
            return;
        }
        let mut files = Vec::new();
        for name in output_names(outcome.primary_id, &outcome.sub_manifest) {
            match std::fs::read(cache_dir.join(&name)) {
                Ok(bytes) => files.push((name, bytes)),
                // An output another step removed: nothing whole to publish.
                Err(_) => return,
            }
        }
        let bytes = match encode_entry(key, &self.platform, rel_path, outcome, &files) {
            Ok(bytes) => bytes,
            Err(why) => {
                self.io_error("encoding the entry of", rel_path, &io::Error::other(why));
                return;
            }
        };
        match self.backend.put(key, &bytes) {
            Ok(()) => {
                self.summary.published += 1;
                self.summary.bytes_written += bytes.len() as u64;
                self.remember(*key);
            }
            Err(e) => self.io_error("publishing", rel_path, &e),
        }
    }
}

/// Attaches the shared cache `choice` names to a cook. A cache that cannot be reached, or an
/// unknown engine build, is a warning and the cook goes on without it.
///
/// # Errors
/// The project's `[cook.shared_cache]` table is invalid.
pub(crate) fn attach(
    inputs: &mut CookInputs,
    choice: &SharedCacheChoice,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let config = match choice {
        SharedCacheChoice::Off => return Ok(()),
        SharedCacheChoice::Use(config) => config.clone(),
        SharedCacheChoice::Project => match SharedCacheConfig::for_project(inputs.project_root())? {
            Some(config) => config,
            None => return Ok(()),
        },
    };
    let Some(build_id) = config
        .engine_build_id
        .clone()
        .or_else(|| engine_build_id().map(str::to_string))
    else {
        warnings.push(
            "shared cook cache off: the engine build id is unknown (the running executable could not be read)".into(),
        );
        return Ok(());
    };
    match open_backend(&config) {
        Ok(backend) => {
            let root = inputs.project_root().to_path_buf();
            let platform = inputs.platform().to_string();
            inputs.shared = Some(SharedCookCache::new(
                backend,
                config.mode,
                build_id,
                &root,
                &platform,
            ));
        }
        Err(why) => warnings.push(format!("{why}; cooking without it")),
    }
    Ok(())
}

/// Detaches the shared cache: its summary, with its warnings moved into `warnings`, and the
/// published-keys file saved. `None` when the cook had none.
pub(crate) fn finish(inputs: &mut CookInputs, warnings: &mut Vec<String>) -> Option<SharedCacheSummary> {
    let mut shared = inputs.shared.take()?;
    warnings.append(&mut shared.warnings);
    if shared.known_dirty && !shared.read_only {
        // A cache file: when it cannot be written the next cook asks the cache again.
        if let Ok(bytes) = postcard::to_allocvec(&shared.known) {
            let _ = crate::cook_ledger::write_atomic(&shared.known_path, &bytes);
        }
    }
    Some(shared.summary)
}

/// The sidecars a shared key covers for `src`, or `None` when its glTF reads a file the key
/// cannot follow (`cook::gltf_uris_covered`).
fn shareable_sidecars(src: &SourceInputs<'_>) -> Option<Vec<PathBuf>> {
    match src.ext {
        "gltf" | "glb" if !crate::cook::gltf_uris_covered(src.source, src.ext) => None,
        _ => Some(crate::cook_inputs::sidecars_for(src.source, src.ext)),
    }
}

/// The shared-cache step of `cook_one` after a local miss: the source's outputs from the shared
/// cache, written into `cache_dir` and recorded so the next cook skips locally. `None` on a miss,
/// for a source without a complete key, or without a shared cache.
pub(crate) fn fetch_for(
    inputs: &mut CookInputs,
    src: &SourceInputs<'_>,
    source_hash: &[u8; 32],
    cache_dir: &Path,
) -> Option<CookOutcome> {
    let shared = inputs.shared.as_ref().filter(|s| !s.stopped)?;
    let build = shared.build_id.clone();
    let sidecars = shareable_sidecars(src)?;
    // `cook_one` hashed the source fresh after its local miss; the side files are read here.
    let key = inputs.shared_key(&build, src, source_hash, &sidecars, true)?;
    let outcome = inputs.shared.as_mut()?.fetch(&key, src, source_hash, cache_dir)?;
    let local = inputs.input_key(src, source_hash, &sidecars);
    inputs.record(src.rel_path, local, &outcome, sidecars);
    Some(outcome)
}

/// The shared-cache step of `cook_one` after a cook whose inputs were recorded (an import, or a
/// local skip when `seed`): publishes the outputs in a read-write cache. A seed publishes only
/// when the cache lacks the entry (a machine with a warm local cache fills the shared one).
pub(crate) fn publish_for(
    inputs: &mut CookInputs,
    src: &SourceInputs<'_>,
    source_hash: &[u8; 32],
    outcome: &CookOutcome,
    cache_dir: &Path,
    seed: bool,
) {
    let Some(shared) = inputs.shared.as_ref().filter(|s| !s.stopped && !s.read_only) else {
        return;
    };
    let build = shared.build_id.clone();
    if !inputs.has_record(src.rel_path) {
        return;
    }
    let sidecars = inputs.recorded_sidecars(src.rel_path);
    // After an import `cook_one` hashed the source fresh; a seed's hash is the local skip's, from
    // stamps, and is checked by a fresh read below before anything is published.
    let Some(key) = inputs.shared_key(&build, src, source_hash, &sidecars, !seed) else {
        return;
    };
    if seed {
        let Some(shared) = inputs.shared.as_mut() else {
            return;
        };
        if shared.known.keys.contains(&key) {
            return;
        }
        match shared.backend.contains(&key) {
            Ok(true) => {
                shared.remember(key);
                return;
            }
            Ok(false) => {}
            Err(e) => {
                shared.io_error("looking up", src.rel_path, &e);
                return;
            }
        }
        // A stamp fooled by a file rewritten with its size and mtime kept made the local skip
        // stale (the ADR-530 blind spot): never spread that to other machines. The source cooks
        // again next time.
        let fresh = inputs
            .hash_file_fresh(src.source)
            .ok()
            .and_then(|(hash, _)| inputs.shared_key(&build, src, &hash, &sidecars, true));
        if fresh != Some(key) {
            inputs.forget(src.rel_path);
            if let Some(shared) = inputs.shared.as_mut() {
                shared.warn(format!(
                    "shared cook cache {}: {} changed without a new size or modification time; not publishing it, it cooks again next time",
                    shared.summary.location, src.rel_path
                ));
            }
            return;
        }
    }
    let Some(shared) = inputs.shared.as_mut() else {
        return;
    };
    shared.publish(&key, src.rel_path, outcome, cache_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Small two-output action, encoded by the production entry writer. The expected output
    /// bytes live separately from the entry decoder so a bad restore cannot vouch for itself.
    struct RestoreFixture {
        root: PathBuf,
        source: PathBuf,
        share: PathBuf,
        local: PathBuf,
        key: [u8; 32],
        outcome: CookOutcome,
        files: Vec<(String, Vec<u8>)>,
        entry: Vec<u8>,
    }

    impl RestoreFixture {
        fn new() -> Self {
            let root = regia_test_utils::unique_temp_dir("cook-cache-restore");
            let source = root.join("assets/crate.gltf");
            let share = root.join("share");
            let local = root.join("local");
            for dir in [source.parent().expect("assets"), &share, &local] {
                std::fs::create_dir_all(dir).expect("fixture directory");
            }
            let source_bytes = br#"{"asset":{"version":"2.0"}}"#;
            crate::cook_ledger::write_atomic(&source, source_bytes).expect("source");
            let (mut outcome, mut files) = outcome_with_sub();
            outcome.source_hash = *blake3::hash(source_bytes).as_bytes();
            crate::cook_ledger::write_atomic(
                &regia_asset::meta_path_for(&source),
                ron::to_string(&regia_asset::MetaSidecar::new("gltf", outcome.primary_id))
                    .expect("meta encode")
                    .as_bytes(),
            )
            .expect("meta");
            // The production publisher installs files in name order, which need not put the
            // primary first. The conflict below always targets the second actual install.
            files.sort_by(|a, b| a.0.cmp(&b.0));
            let mut fixture = Self {
                root,
                source,
                share,
                local,
                key: [0; 32],
                outcome,
                files,
                entry: Vec::new(),
            };
            let mut inputs = CookInputs::open(&fixture.root, "linux");
            fixture.key = inputs
                .shared_key(
                    "restore-test",
                    &fixture.src(),
                    &fixture.outcome.source_hash,
                    &[],
                    true,
                )
                .expect("complete shared key");
            fixture.entry = encode_entry(
                &fixture.key,
                "linux",
                fixture.src().rel_path,
                &fixture.outcome,
                &fixture.files,
            )
            .expect("encode two-output entry");
            assert_eq!(verify_entry(&fixture.entry).expect("valid entry").files, 2);
            fixture.store(&fixture.entry);
            fixture
        }

        fn src(&self) -> SourceInputs<'_> {
            SourceInputs {
                source: &self.source,
                rel_path: "assets/crate.gltf",
                project_root: &self.root,
                platform: "linux",
                ext: "gltf",
                importer_version: self.outcome.importer_version,
                revision: "",
            }
        }

        fn store(&self, bytes: &[u8]) {
            DirBackend::open(&self.share, SharedCacheMode::ReadWrite)
                .expect("seed backend")
                .put(&self.key, bytes)
                .expect("atomic entry write");
        }

        fn reader(&self) -> CookInputs {
            let mut inputs = CookInputs::open(&self.root, "linux");
            inputs.shared = Some(SharedCookCache::new(
                Box::new(DirBackend::open(&self.share, SharedCacheMode::Read).expect("read backend")),
                SharedCacheMode::Read,
                "restore-test".into(),
                &self.root,
                "linux",
            ));
            inputs
        }

        fn old_outputs(&self) {
            for (name, _) in &self.files {
                crate::cook_ledger::write_atomic(&self.local.join(name), b"old local output")
                    .expect("old output");
            }
            crate::cook_ledger::write_atomic(&self.local.join("unrelated.txt"), b"keep me")
                .expect("unrelated file");
        }

        fn restore(&self, inputs: &mut CookInputs) -> Option<CookOutcome> {
            fetch_for(inputs, &self.src(), &self.outcome.source_hash, &self.local)
        }

        fn assert_healthy(&self, inputs: &mut CookInputs) {
            let restored = self.restore(inputs).expect("complete restore");
            assert_eq!(restored.primary_id, self.outcome.primary_id);
            assert_eq!(restored.sub_manifest, self.outcome.sub_manifest);
            assert!(!restored.skipped);
            assert!(inputs.has_record(self.src().rel_path));
            let mut expected: BTreeMap<_, _> = self
                .files
                .iter()
                .map(|(name, bytes)| (PathBuf::from(name), Some(bytes.clone())))
                .collect();
            expected.insert(PathBuf::from("unrelated.txt"), Some(b"keep me".to_vec()));
            assert_eq!(restore_tree(&self.local), expected, "exact verified output set");
        }
    }

    impl Drop for RestoreFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Snapshot names and bytes, including directories, unrelated files and leaked temporaries.
    fn restore_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, dir: &Path, tree: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(dir).expect("snapshot directory") {
                let entry = entry.expect("snapshot entry");
                let path = entry.path();
                let name = path.strip_prefix(root).expect("relative name").to_path_buf();
                if entry.file_type().expect("file type").is_dir() {
                    tree.insert(name, None);
                    visit(root, &path, tree);
                } else {
                    tree.insert(name, Some(std::fs::read(path).expect("snapshot bytes")));
                }
            }
        }
        let mut tree = BTreeMap::new();
        visit(root, root, &mut tree);
        tree
    }

    #[test]
    fn restore_second_install_failure_is_a_miss_and_a_healthy_retry_replaces_the_set() {
        let fixture = RestoreFixture::new();
        fixture.old_outputs();
        let share_before = restore_tree(&fixture.share);
        let mut inputs = fixture.reader();
        let second = fixture.local.join(&fixture.files[1].0);
        // A nonempty directory is a portable rename conflict even when tests run as root.
        // Keep the displaced old bytes inside it so the old set and unrelated data are checked.
        let old = std::fs::read(&second).expect("old second output");
        std::fs::remove_file(&second).expect("replace second with conflict");
        std::fs::create_dir(&second).expect("conflicting directory");
        crate::cook_ledger::write_atomic(&second.join("old-output"), &old).expect("old bytes");
        crate::cook_ledger::write_atomic(&second.join("unrelated"), b"directory sentinel").expect("sentinel");
        let mut expected = restore_tree(&fixture.local);
        expected.insert(
            PathBuf::from(&fixture.files[0].0),
            Some(fixture.files[0].1.clone()),
        );

        assert!(fixture.restore(&mut inputs).is_none(), "incomplete set is a miss");
        assert!(!inputs.has_record(fixture.src().rel_path), "no local skip record");
        let shared = inputs.shared.as_ref().expect("attached");
        assert_eq!(
            (shared.summary.hits, shared.summary.misses, shared.summary.errors),
            (0, 1, 1)
        );
        assert_eq!(shared.summary.corrupt, 0, "install IO is not entry corruption");
        assert!(
            shared.known.keys.is_empty(),
            "incomplete restore is not remembered"
        );
        assert!(
            shared
                .warnings
                .iter()
                .any(|w| w.contains("writing the cooked outputs of"))
        );
        assert_eq!(
            restore_tree(&fixture.local),
            expected,
            "no rollback, deletion or temp leak"
        );
        assert_eq!(
            restore_tree(&fixture.share),
            share_before,
            "read-only cache unchanged"
        );

        std::fs::remove_dir_all(&second).expect("remove only test-created conflict");
        crate::cook_ledger::write_atomic(&second, &old).expect("replace old second output");
        fixture.assert_healthy(&mut inputs);
        let shared = inputs.shared.as_ref().expect("attached");
        assert_eq!(
            (shared.summary.hits, shared.summary.misses, shared.summary.errors),
            (1, 1, 1)
        );
        assert_eq!(shared.known.keys, BTreeSet::from([fixture.key]));
        assert_eq!(
            restore_tree(&fixture.share),
            share_before,
            "retry never writes the cache"
        );
    }

    #[test]
    fn restore_verifies_the_final_payload_before_exposing_any_output() {
        let fixture = RestoreFixture::new();
        fixture.old_outputs();
        let local_before = restore_tree(&fixture.local);
        let mut corrupt = fixture.entry.clone();
        *corrupt.last_mut().expect("final payload byte") ^= 1;
        fixture.store(&corrupt);
        let share_before = restore_tree(&fixture.share);
        let mut inputs = fixture.reader();

        assert!(fixture.restore(&mut inputs).is_none(), "invalid entry is a miss");
        assert_eq!(
            restore_tree(&fixture.local),
            local_before,
            "even the valid first payload stays hidden"
        );
        assert!(!inputs.has_record(fixture.src().rel_path));
        let shared = inputs.shared.as_ref().expect("attached");
        assert_eq!(
            (
                shared.summary.hits,
                shared.summary.misses,
                shared.summary.corrupt,
                shared.summary.errors
            ),
            (0, 1, 1, 0)
        );
        assert_eq!(shared.summary.bytes_read, corrupt.len() as u64);
        assert!(shared.known.keys.is_empty());
        assert!(
            shared
                .warnings
                .iter()
                .any(|w| w.contains(&fixture.files[1].0) && w.contains("fails its hash"))
        );
        assert_eq!(
            restore_tree(&fixture.share),
            share_before,
            "corrupt read-only entry retained"
        );

        // Only the fixture writer repairs the entry. A new healthy reader must restore every
        // byte, including the earlier payload that was valid in the rejected entry.
        fixture.store(&fixture.entry);
        let repaired_share = restore_tree(&fixture.share);
        let mut healthy = fixture.reader();
        fixture.assert_healthy(&mut healthy);
        let shared = healthy.shared.as_ref().expect("attached");
        assert_eq!((shared.summary.hits, shared.summary.misses), (1, 0));
        assert_eq!(restore_tree(&fixture.share), repaired_share);
    }

    fn outcome_with_sub() -> (CookOutcome, Vec<(String, Vec<u8>)>) {
        let id = AssetId::sub_asset(AssetId::default(), "crate.gltf");
        let sub = AssetId::sub_asset(id, "mesh0");
        let outcome = CookOutcome {
            primary_id: id,
            type_path: "regia_asset::Model".into(),
            source_hash: [1; 32],
            settings_hash: [2; 32],
            importer: "gltf".into(),
            importer_version: 7,
            cache_key: [3; 32],
            cooked_rel: cooked_file_name(id),
            skipped: false,
            sub_manifest: vec![ManifestEntry {
                path: "assets/crate.gltf#mesh0".into(),
                id: sub,
                type_path: "regia_asset::CpuMesh".into(),
            }],
        };
        let files = vec![
            (cooked_file_name(id), b"primary bytes".to_vec()),
            (cooked_file_name(sub), vec![7u8; 300]),
        ];
        (outcome, files)
    }

    #[test]
    fn an_entry_round_trips_and_every_damage_is_rejected() {
        let (outcome, files) = outcome_with_sub();
        let bytes = encode_entry(&[9; 32], "linux", "assets/crate.gltf", &outcome, &files).expect("encode");
        let summary = verify_entry(&bytes).expect("valid");
        assert_eq!(summary.key, [9; 32]);
        assert_eq!((summary.files, summary.bytes), (2, 313));
        let entry = decode_entry(&bytes).expect("decode");
        assert_eq!(entry.files, vec![&b"primary bytes"[..], &[7u8; 300][..]]);

        // Truncated anywhere, one byte flipped in a file, a trailing byte, another version.
        for cut in [0, 5, 11, 12, 40, bytes.len() - 1] {
            assert!(verify_entry(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(verify_entry(&flipped).expect_err("an error").contains("hash"));
        // Any one bit flipped anywhere, the header's database fields included (no file hash
        // covers them; the header hash does).
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x10;
            assert!(
                verify_entry(&flipped).is_err(),
                "a flip at byte {at} was accepted"
            );
        }
        // Sub-asset rows of another source are refused.
        let (mut other, files) = outcome_with_sub();
        other.sub_manifest[0].path = "assets/other.gltf#mesh0".into();
        let bytes = encode_entry(&[9; 32], "linux", "assets/crate.gltf", &other, &files).expect("encode");
        assert!(
            verify_entry(&bytes)
                .expect_err("an error")
                .contains("another source")
        );
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(verify_entry(&longer).is_err());
        let mut version = bytes.clone();
        version[4] = 2;
        assert!(verify_entry(&version).expect_err("an error").contains("version"));
        assert!(verify_entry(&sample_entry()).is_ok());
    }

    #[test]
    fn an_entry_can_only_name_its_own_cooked_files() {
        let (outcome, mut files) = outcome_with_sub();
        // A file outside the outputs (a path escaping the cache) is refused.
        files[1].0 = "../../escape.fckd".into();
        let bytes = encode_entry(&[1; 32], "linux", "assets/crate.gltf", &outcome, &files).expect("encode");
        assert!(verify_entry(&bytes).expect_err("an error").contains("exactly"));
        // A missing output too.
        let (outcome, files) = outcome_with_sub();
        let bytes =
            encode_entry(&[1; 32], "linux", "assets/crate.gltf", &outcome, &files[..1]).expect("encode");
        assert!(verify_entry(&bytes).is_err());
    }

    #[test]
    fn the_directory_backend_stores_lists_and_survives_concurrent_writers() {
        let root = regia_test_utils::unique_temp_dir("cook-cache-dir");
        // Both modes need the directory (an unmounted share must not fill the local disk);
        // read-write creates its layout inside it.
        assert!(DirBackend::open(&root.join("absent"), SharedCacheMode::Read).is_err());
        assert!(DirBackend::open(&root.join("absent"), SharedCacheMode::ReadWrite).is_err());
        assert!(!root.join("absent").exists());
        let backend = DirBackend::open(&root, SharedCacheMode::ReadWrite).expect("open");
        let key = [0xab; 32];
        assert_eq!(backend.get(&key).expect("get"), None);
        assert!(!backend.contains(&key).expect("contains"));

        let entry = sample_entry();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let backend = backend.clone();
                let entry = entry.clone();
                std::thread::spawn(move || backend.put(&key, &entry))
            })
            .collect();
        for t in threads {
            t.join().expect("thread").expect("every writer succeeds");
        }
        assert_eq!(backend.get(&key).expect("get").as_deref(), Some(&entry[..]));
        let listed = backend.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].key, listed[0].bytes), (key, entry.len() as u64));
        let shard = backend.entry_path(&key).parent().expect("shard").to_path_buf();
        let names: Vec<_> = std::fs::read_dir(&shard).expect("shard").flatten().collect();
        assert_eq!(names.len(), 1, "no temp file is left");
        // A file in the wrong shard is not an entry: `remove` and `prune` could not act on it.
        let other = [0x01; 32];
        std::fs::write(shard.join(format!("{}.{ENTRY_EXT}", hex(&other))), b"x").expect("write");
        assert_eq!(backend.list().expect("list").len(), 1);
        backend.remove(&key).expect("remove");
        backend.remove(&key).expect("removing nothing is fine");
        assert!(backend.list().expect("list").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_least_recently_used_entries_and_stale_temps() {
        let root = regia_test_utils::unique_temp_dir("cook-cache-prune");
        let backend = DirBackend::open(&root, SharedCacheMode::ReadWrite).expect("open");
        let now = SystemTime::now();
        for i in 0..4u8 {
            let key = [i; 32];
            backend.put(&key, &[i; 1000]).expect("put");
            // Entry 0 used longest ago, entry 3 most recently.
            std::fs::File::options()
                .write(true)
                .open(backend.entry_path(&key))
                .expect("open")
                .set_modified(now - Duration::from_secs(100 * (4 - u64::from(i))))
                .expect("mtime");
        }
        let shard = backend
            .entry_path(&[0; 32])
            .parent()
            .expect("shard")
            .to_path_buf();
        let stale = shard.join(".dead.tmp");
        let fresh = shard.join(".live.tmp");
        std::fs::write(&stale, b"half").expect("write");
        std::fs::write(&fresh, b"half").expect("write");
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open")
            .set_modified(now - 2 * STALE_TEMP_AGE)
            .expect("mtime");

        let report = prune(&backend, 2500).expect("prune");
        assert_eq!((report.entries_before, report.bytes_before), (4, 4000));
        assert_eq!((report.removed, report.bytes_removed), (2, 2000));
        assert_eq!(report.stale_temps_removed, 1);
        let left: BTreeSet<[u8; 32]> = backend.list().expect("list").iter().map(|e| e.key).collect();
        assert_eq!(left, BTreeSet::from([[2; 32], [3; 32]]));
        assert!(!stale.exists() && fresh.exists(), "a write in progress is kept");
        assert_eq!(prune(&backend, 10_000).expect("prune").removed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("500M"), Ok(500 << 20));
        assert_eq!(parse_size("2G"), Ok(2 << 30));
        assert_eq!(parse_size("1.5GiB"), Ok(3 << 29));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("64kb"), Ok(64 << 10));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5X").is_err());
    }

    #[test]
    fn the_project_config_is_off_by_default_and_strict() {
        let root = Path::new("/proj");
        assert_eq!(SharedCacheConfig::resolve(root, None, None), Ok(None));
        assert_eq!(
            SharedCacheConfig::resolve(root, Some("[cook]\nchunks = {}\n"), None),
            Ok(None)
        );
        let cfg = SharedCacheConfig::resolve(
            root,
            Some("[cook.shared_cache]\npath = \"cache\"\nmode = \"read_write\"\n"),
            None,
        )
        .expect("valid")
        .expect("on");
        assert_eq!(cfg.path, root.join("cache"));
        assert_eq!(cfg.mode, SharedCacheMode::ReadWrite);
        // Inline table form, default mode read, the environment replacing the path.
        let cfg = SharedCacheConfig::resolve(
            root,
            Some("[cook]\nshared_cache = { path = \"/mnt/share\" }\n"),
            Some("/net/cache".into()),
        )
        .expect("valid")
        .expect("on");
        assert_eq!(
            (cfg.path.as_path(), cfg.mode),
            (Path::new("/net/cache"), SharedCacheMode::Read)
        );
        assert!(
            SharedCacheConfig::resolve(root, None, Some("/env/only".into()))
                .expect("valid")
                .is_some()
        );
        for bad in [
            "[cook.shared_cache]\npath = \"c\"\nmode = \"write\"\n",
            "[cook.shared_cache]\npath = 5\n",
            "[cook.shared_cache]\npath = \"c\"\nsize = 1\n",
            "[cook]\nshared_cache = \"c\"\n",
        ] {
            assert!(
                SharedCacheConfig::resolve(root, Some(bad), None).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn urls_pick_the_http_backend_for_both_schemes() {
        let config = SharedCacheConfig::new("http://cache.local:8080/regia", SharedCacheMode::Read);
        assert_eq!(
            open_backend(&config).expect("http").location(),
            "http://cache.local:8080/regia"
        );
        // ADR-2710: https:// is a TLS backend now (ADR-2612 deferred it); the `http_cache`
        // feature carries it (ADR-7972, the off side is the test below).
        let config = SharedCacheConfig::new("https://cache.local/regia", SharedCacheMode::Read);
        #[cfg(feature = "http_cache")]
        assert_eq!(
            open_backend(&config).expect("https").location(),
            "https://cache.local/regia"
        );
        #[cfg(not(feature = "http_cache"))]
        let _ = config;
        // A URL in Regia.toml is not joined to the project directory.
        let cfg = SharedCacheConfig::resolve(
            Path::new("/proj"),
            Some("[cook.shared_cache]\npath = \"http://cache.lan/c\"\n"),
            None,
        )
        .expect("valid")
        .expect("on");
        assert_eq!(cfg.path, Path::new("http://cache.lan/c"));
    }

    /// ADR-7972: without the `http_cache` feature an `https://` cache is refused when it is
    /// opened, with the documented code and never by falling back to plain `http://`; `http://`
    /// and directory caches still open. With the feature the same URL opens a TLS backend.
    #[test]
    fn https_cache_answers_its_disabled_code_only_without_http_cache() {
        let config = SharedCacheConfig::new("https://cache.local/regia", SharedCacheMode::Read);
        let opened = open_backend(&config);
        let direct = HttpBackend::new("https://cache.local/regia");
        if cfg!(feature = "http_cache") {
            assert_eq!(opened.expect("https").location(), "https://cache.local/regia");
            assert!(direct.is_ok());
        } else {
            for message in [opened.err(), direct.err()] {
                let message = message.expect("https must be refused without http_cache");
                assert!(message.starts_with(COOK_CACHE_HTTPS_DISABLED_CODE), "{message}");
                assert!(message.contains("http_cache"), "{message}");
            }
        }
        let plain = SharedCacheConfig::new("http://cache.local:8080/regia", SharedCacheMode::Read);
        assert!(open_backend(&plain).is_ok());
        let dir = regia_test_utils::unique_temp_dir("https-disabled-dir");
        let local = SharedCacheConfig::new(dir.clone(), SharedCacheMode::ReadWrite);
        assert!(open_backend(&local).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_engine_build_id_names_this_build() {
        let id = engine_build_id().expect("the test executable is readable");
        assert!(id.starts_with(&format!("regia_asset_pipeline {}", env!("CARGO_PKG_VERSION"))));
        assert!(id.contains(std::env::consts::ARCH));
        assert_eq!(engine_build_id(), Some(id), "computed once");
    }
}
