// SPDX-License-Identifier: MPL-2.0
//! The warm discovery cache (`specs/57` § 10, ADR-2844).
//!
//! Cold discovery reads and parses every `regia-plugin.toml`. The cache keeps the parsed result of
//! each manifest in `<project>/.regia/extension-discovery.json`, keyed by the extension directory
//! and **validated by the hash of the manifest's bytes**: the file is still read on every start
//! (a changed manifest is noticed at once), but an unchanged one is not parsed or checked again.
//!
//! The cache is derived data and never authoritative:
//!
//! * a missing, unreadable, oversized, truncated or otherwise corrupt file is a cold start;
//! * a different `format_version` or engine version is a cold start (no migrator: it is rebuilt);
//! * an entry that no longer converts back into a manifest is parsed from the file instead;
//! * a failed write is logged and ignored, and the file is replaced atomically (temp + rename);
//! * manifests that fail to parse are never cached, so their diagnostics are produced every time.
//!
//! [`CacheStats`] counts the manifests parsed and reused, which is how tests and the performance
//! budget measure it (counters, not wall time).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use super::manifest::{CodeEntry, Dependency, ExtensionManifest, Phase, ToolEntry};

/// The cache file name inside `.regia/`.
pub const CACHE_FILE: &str = "extension-discovery.json";
/// The layout this build writes and reads. A different value is discarded and rebuilt.
pub const CACHE_FORMAT_VERSION: u32 = 1;
/// A cache file larger than this is treated as corrupt.
const MAX_CACHE_BYTES: u64 = 16 << 20;

/// How the cache file looked at the start of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheState {
    /// No file (or no cache was asked for).
    #[default]
    Cold,
    /// A valid file of this format and engine version.
    Warm,
    /// A file that could not be read as a cache.
    Corrupt,
    /// A valid file of another format or engine version.
    Outdated,
}

/// What one cached discovery did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// The cache file's state when the run began.
    pub state: CacheState,
    /// Manifests read from disk and parsed.
    pub parsed: usize,
    /// Manifests answered from the cache (hash equal).
    pub reused: usize,
    /// Whether the cache file was written.
    pub written: bool,
}

#[derive(Serialize, Deserialize)]
struct File {
    format_version: u32,
    engine_version: String,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    hash: String,
    manifest: Dto,
}

#[derive(Clone, Serialize, Deserialize)]
struct DepDto {
    id: String,
    version: String,
    optional: bool,
    platforms: Option<Vec<String>>,
    profiles: Option<Vec<String>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CodeDto {
    crate_name: Option<String>,
    audience: Vec<String>,
    phase: String,
    library: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolDto {
    name: String,
    command: String,
    capabilities: Vec<String>,
    timeout_seconds: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct Dto {
    id: String,
    name: String,
    version: String,
    publisher: String,
    licence: String,
    description: String,
    auto_enable: bool,
    provides: Vec<String>,
    replaces: Vec<String>,
    conflicts: Vec<String>,
    platforms: Option<Vec<String>>,
    profiles: Option<Vec<String>>,
    engine_min: Option<String>,
    engine_max: Option<String>,
    dependencies: Vec<DepDto>,
    code: Vec<CodeDto>,
    capabilities: Vec<String>,
    load_capabilities: Vec<String>,
    content: Vec<String>,
    tools: Vec<ToolDto>,
    warnings: Vec<String>,
}

fn phase_of(name: &str) -> Option<Phase> {
    [Phase::Foundation, Phase::Early, Phase::Default, Phase::Late]
        .into_iter()
        .find(|p| p.name() == name)
}

impl Dto {
    fn of(m: &ExtensionManifest) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.to_string(),
            publisher: m.publisher.clone(),
            licence: m.licence.clone(),
            description: m.description.clone(),
            auto_enable: m.auto_enable,
            provides: m.provides.clone(),
            replaces: m.replaces.clone(),
            conflicts: m.conflicts.clone(),
            platforms: m.platforms.clone(),
            profiles: m.profiles.clone(),
            engine_min: m.engine_min.as_ref().map(ToString::to_string),
            engine_max: m.engine_max.as_ref().map(ToString::to_string),
            dependencies: m
                .dependencies
                .iter()
                .map(|d| DepDto {
                    id: d.id.clone(),
                    version: d.version.to_string(),
                    optional: d.optional,
                    platforms: d.platforms.clone(),
                    profiles: d.profiles.clone(),
                })
                .collect(),
            code: m
                .code
                .iter()
                .map(|c| CodeDto {
                    crate_name: c.crate_name.clone(),
                    audience: c.audience.clone(),
                    phase: c.phase.name().to_owned(),
                    library: c.library.clone(),
                })
                .collect(),
            capabilities: m.capabilities.clone(),
            load_capabilities: m.load_capabilities.clone(),
            content: m.content.clone(),
            tools: m
                .tools
                .iter()
                .map(|t| ToolDto {
                    name: t.name.clone(),
                    command: t.command.clone(),
                    capabilities: t.capabilities.clone(),
                    timeout_seconds: t.timeout_seconds,
                })
                .collect(),
            warnings: m.warnings.clone(),
        }
    }

    /// `None` when a stored version, requirement or phase no longer parses.
    fn into_manifest(self) -> Option<ExtensionManifest> {
        let version = |text: &str| Version::parse(text).ok();
        let mut dependencies = Vec::with_capacity(self.dependencies.len());
        for d in self.dependencies {
            dependencies.push(Dependency {
                id: d.id,
                version: VersionReq::parse(&d.version).ok()?,
                optional: d.optional,
                platforms: d.platforms,
                profiles: d.profiles,
            });
        }
        let mut code = Vec::with_capacity(self.code.len());
        for c in self.code {
            code.push(CodeEntry {
                crate_name: c.crate_name,
                audience: c.audience,
                phase: phase_of(&c.phase)?,
                library: c.library,
            });
        }
        Some(ExtensionManifest {
            version: version(&self.version)?,
            engine_min: match self.engine_min {
                Some(text) => Some(version(&text)?),
                None => None,
            },
            engine_max: match self.engine_max {
                Some(text) => Some(version(&text)?),
                None => None,
            },
            id: self.id,
            name: self.name,
            publisher: self.publisher,
            licence: self.licence,
            description: self.description,
            auto_enable: self.auto_enable,
            provides: self.provides,
            replaces: self.replaces,
            conflicts: self.conflicts,
            platforms: self.platforms,
            profiles: self.profiles,
            dependencies,
            code,
            capabilities: self.capabilities,
            load_capabilities: self.load_capabilities,
            content: self.content,
            tools: self
                .tools
                .into_iter()
                .map(|t| ToolEntry {
                    name: t.name,
                    command: t.command,
                    capabilities: t.capabilities,
                    timeout_seconds: t.timeout_seconds,
                })
                .collect(),
            warnings: self.warnings,
        })
    }
}

/// The entries of a cache file, by extension directory.
#[derive(Default)]
pub(super) struct DiscoveryCache {
    entries: BTreeMap<String, (String, Dto)>,
    /// The paths the file listed, to tell whether a run changed the set.
    loaded: Vec<String>,
    /// Entries kept or added by this run, in the order met.
    seen: Vec<(String, String, Dto)>,
    pub(super) stats: CacheStats,
}

impl DiscoveryCache {
    /// Reads `path`; every failure is a cold start with a state saying why.
    pub(super) fn load(path: &Path) -> Self {
        let mut cache = Self::default();
        let Ok(meta) = std::fs::metadata(path) else {
            return cache;
        };
        if !meta.is_file() || meta.len() > MAX_CACHE_BYTES {
            cache.stats.state = CacheState::Corrupt;
            return cache;
        }
        let Ok(bytes) = std::fs::read(path) else {
            cache.stats.state = CacheState::Corrupt;
            return cache;
        };
        let Ok(file) = serde_json::from_slice::<File>(&bytes) else {
            cache.stats.state = CacheState::Corrupt;
            return cache;
        };
        if file.format_version != CACHE_FORMAT_VERSION || file.engine_version != super::stamp::ENGINE_VERSION
        {
            cache.stats.state = CacheState::Outdated;
            return cache;
        }
        cache.stats.state = CacheState::Warm;
        for entry in file.entries {
            cache.loaded.push(entry.path.clone());
            cache.entries.insert(entry.path, (entry.hash, entry.manifest));
        }
        cache
    }

    /// The cached manifest for `path` when its recorded hash is `hash`.
    pub(super) fn reuse(&mut self, path: &str, hash: &str) -> Option<ExtensionManifest> {
        let (cached_hash, dto) = self.entries.get(path)?;
        if cached_hash != hash {
            return None;
        }
        let manifest = dto.clone().into_manifest()?;
        self.stats.reused += 1;
        self.seen
            .push((path.to_owned(), hash.to_owned(), Dto::of(&manifest)));
        Some(manifest)
    }

    /// Records a manifest parsed from disk.
    pub(super) fn parsed(&mut self, path: &str, hash: &str, manifest: &ExtensionManifest) {
        self.stats.parsed += 1;
        self.seen
            .push((path.to_owned(), hash.to_owned(), Dto::of(manifest)));
    }

    /// Writes the file when the set of entries changed. Returns the run's counters.
    pub(super) fn finish(mut self, path: &Path) -> CacheStats {
        let same_set = {
            let mut now: Vec<&str> = self.seen.iter().map(|(p, _, _)| p.as_str()).collect();
            let mut before: Vec<&str> = self.loaded.iter().map(String::as_str).collect();
            now.sort_unstable();
            before.sort_unstable();
            now == before
        };
        if self.stats.parsed == 0 && same_set && self.stats.state == CacheState::Warm {
            return self.stats;
        }
        let file = File {
            format_version: CACHE_FORMAT_VERSION,
            engine_version: super::stamp::ENGINE_VERSION.to_owned(),
            entries: std::mem::take(&mut self.seen)
                .into_iter()
                .map(|(path, hash, manifest)| Entry { path, hash, manifest })
                .collect(),
        };
        match write_atomic(path, &file) {
            Ok(()) => self.stats.written = true,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "extension discovery cache not written")
            }
        }
        self.stats
    }
}

fn write_atomic(path: &Path, file: &File) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let bytes = serde_json::to_vec(file).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}
