// SPDX-License-Identifier: MPL-2.0
//! Local report artifacts from the engine's configured paths (ADR-8058).
//!
//! Discovery never uploads, deletes, or flushes files. Results are a best-effort snapshot:
//! a writer may change a file after its metadata was read. Only pipeline-cache prefixes
//! are read; a parsed key does not validate or authenticate the cache payload.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use regia_ecs::prelude::World;
use regia_platform::{MountPoint, Vfs, VfsPath};
use regia_render::pipeline_cache::{self, PipelineCacheKey};

/// Configured artifact directories. `None` means that source is unavailable.
/// Custom hosts may supply these explicitly instead of using [`Self::from_world`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportSourceDirs {
    /// The mounted `logs://` directory (session logs and telemetry summaries).
    pub logs: Option<PathBuf>,
    /// The process panic hook's configured directory (may equal `logs`).
    pub crashes: Option<PathBuf>,
    /// The feedback folder, including crash bundles and preserved native minidumps.
    pub feedback: Option<PathBuf>,
    /// The renderer's local cache directory after its environment overrides.
    pub pipeline_cache: Option<PathBuf>,
}

/// Limits for an on-demand scan; no file-size or upload policy is imposed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportSourceOptions {
    /// Directory entries considered per configured root, including unrelated files.
    /// One lookahead entry detects truncation. Zero considers none. A capped scan sets [`ReportSources::truncated`]; which entries
    /// fit depends on filesystem enumeration order. Uncapped results are path-sorted.
    pub max_entries_per_directory: usize,
    /// Maximum prefix bytes read from each pipeline cache (default 64 KiB).
    /// A key that does not fit is reported as `invalid_header`, never read unboundedly.
    pub max_header_bytes: usize,
}

impl Default for ReportSourceOptions {
    fn default() -> Self {
        Self {
            max_entries_per_directory: 4096,
            max_header_bytes: 64 * 1024,
        }
    }
}

/// Artifact class; classification describes the name, not the validity of its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReportSourceKind {
    /// A `.log`, `.jsonl` or named session summary in the log directory.
    Log,
    /// A `crash-*.txt` report, native `.crash.bin` record or preserved `native-crash-*.dmp`.
    Crash,
    /// A `feedback-*.zip` bundle, including crash feedback.
    Feedback,
    /// A `pipelines-*.fpc` file in the configured local pipeline-cache directory.
    PipelineCache,
}

impl ReportSourceKind {
    /// Stable lowercase spelling used by the agent protocol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Crash => "crash",
            Self::Feedback => "feedback",
            Self::PipelineCache => "pipeline_cache",
        }
    }
}

/// One regular file. Symlinks, Windows reparse points and subdirectories are skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportArtifact {
    /// Artifact class.
    pub kind: ReportSourceKind,
    /// Exact host path, without lossy filename conversion.
    pub path: PathBuf,
    /// File size observed during the scan.
    pub bytes: u64,
    /// Modification time in milliseconds since the Unix epoch; zero when unavailable.
    pub modified_unix_ms: u64,
    /// Parsed pipeline-cache key; `None` for other artifacts or a malformed header.
    /// Payload length/checksum and driver compatibility are not checked.
    pub pipeline_key: Option<PipelineCacheKey>,
}

/// A scan error retained alongside successfully discovered artifacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportSourceIssue {
    /// Directory or artifact that could not be inspected.
    pub path: PathBuf,
    /// Stable code: `read_directory`, `read_entry`, `read_metadata`, `read_header`,
    /// or `invalid_header`.
    pub code: &'static str,
    /// Human-readable detail; not a stable protocol value.
    pub message: String,
}

/// A sorted, deduplicated snapshot of local artifacts and any incomplete reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportSources {
    /// Roots used by this scan, including directories that do not exist yet.
    pub directories: ReportSourceDirs,
    /// Recognized regular files, sorted and deduplicated by exact host path.
    pub artifacts: Vec<ReportArtifact>,
    /// Failures sorted by path and code. A missing directory is empty, not an error.
    pub issues: Vec<ReportSourceIssue>,
    /// At least one directory exceeded the entry limit; the inventory is incomplete.
    pub truncated: bool,
}

impl ReportSourceDirs {
    /// Resolves the existing engine configuration without creating directories or guessing
    /// OS paths. A non-filesystem VFS mount has no host artifacts. The crash and pipeline
    /// directories are process-global, as are their corresponding engine writers.
    #[must_use]
    pub fn from_world(world: &World) -> Self {
        Self {
            logs: world
                .get_resource::<Vfs>()
                .and_then(|vfs| vfs.host_path(&VfsPath::root(MountPoint::Logs))),
            crashes: regia_platform::crash::crash_dir(),
            feedback: crate::feedback::feedback_folder(world),
            pipeline_cache: pipeline_cache::directory(),
        }
    }

    /// Lists immediate children only, with bounded directory and cache-header reads.
    /// Files with unrecognized names, temporary files and rejected reports are skipped.
    /// Does not require an installed renderer or feedback plugin.
    #[must_use]
    pub fn list(&self, options: &ReportSourceOptions) -> ReportSources {
        let _span = tracing::debug_span!("report_sources").entered();
        let mut result = ReportSources {
            directories: self.clone(),
            ..ReportSources::default()
        };
        let mut artifacts = BTreeMap::new();
        for (dir, source) in [
            (&self.logs, ReportSourceKind::Log),
            (&self.crashes, ReportSourceKind::Crash),
            (&self.feedback, ReportSourceKind::Feedback),
            (&self.pipeline_cache, ReportSourceKind::PipelineCache),
        ] {
            let Some(dir) = dir else { continue };
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    result.issue(dir.clone(), "read_directory", error);
                    continue;
                }
            };
            for (index, entry) in entries.enumerate() {
                if index >= options.max_entries_per_directory {
                    result.truncated = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        result.issue(dir.clone(), "read_entry", error);
                        continue;
                    }
                };
                let name = entry.file_name();
                let Some(kind) = classify(source, &name.to_string_lossy()) else {
                    continue;
                };
                let path = entry.path();
                if artifacts.contains_key(&path) {
                    continue;
                }
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        result.issue(path, "read_metadata", error);
                        continue;
                    }
                };
                if !regular_file(&metadata) {
                    continue;
                }
                let pipeline_key = if kind == ReportSourceKind::PipelineCache {
                    match read_key(&path, options.max_header_bytes) {
                        Ok(key) => Some(key),
                        Err((code, message)) => {
                            result.issue(path.clone(), code, message);
                            None
                        }
                    }
                } else {
                    None
                };
                let modified_unix_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |duration| {
                        duration.as_millis().min(u128::from(u64::MAX)) as u64
                    });
                artifacts.insert(
                    path.clone(),
                    ReportArtifact {
                        kind,
                        path,
                        bytes: metadata.len(),
                        modified_unix_ms,
                        pipeline_key,
                    },
                );
            }
        }
        result.artifacts = artifacts.into_values().collect();
        result
            .issues
            .sort_by(|a, b| (&a.path, a.code).cmp(&(&b.path, b.code)));
        result
    }
}

impl ReportSources {
    fn issue(&mut self, path: PathBuf, code: &'static str, message: impl ToString) {
        self.issues.push(ReportSourceIssue {
            path,
            code,
            message: message.to_string(),
        });
    }
}

fn classify(source: ReportSourceKind, name: &str) -> Option<ReportSourceKind> {
    let crash = regia_platform::crash::is_crash_file_name(name)
        || name.ends_with(".crash.bin")
        || (name.starts_with("native-crash-") && name.ends_with(".dmp"));
    match source {
        ReportSourceKind::Log | ReportSourceKind::Crash | ReportSourceKind::Feedback if crash => {
            Some(ReportSourceKind::Crash)
        }
        ReportSourceKind::Log
            if name.ends_with(".log")
                || name.ends_with(".jsonl")
                || name.ends_with(".summary.json")
                || name.ends_with(".summary.partial.json")
                || name == "summary.json"
                || name == "summary.partial.json" =>
        {
            Some(source)
        }
        ReportSourceKind::Feedback if crate::feedback::bundle::is_bundle_name(name) => Some(source),
        ReportSourceKind::PipelineCache if name.starts_with("pipelines-") && name.ends_with(".fpc") => {
            Some(source)
        }
        _ => None,
    }
}

fn regular_file(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    metadata.is_file() && !metadata.file_type().is_symlink()
}

fn read_key(path: &std::path::Path, limit: usize) -> Result<PipelineCacheKey, (&'static str, String)> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(limit as u64).read_to_end(&mut bytes))
        .map_err(|error| ("read_header", error.to_string()))?;
    pipeline_cache::read_key(&bytes).map_err(|error| ("invalid_header", error.to_string()))
}
