// SPDX-License-Identifier: MPL-2.0
//! Discovery (`specs/57` § 2.5 step 1): finds `regia-plugin.toml` files under the discovery roots.
//!
//! Roots are scanned in the order given; inside a root, subdirectories in name order, so the
//! result is the same on every machine. A directory holding a manifest is never descended into.
//! Hidden directories and `target` are skipped, and the walk stops [`MAX_DEPTH`] levels down.
//! A manifest that does not parse becomes a diagnostic; it never stops the scan.

use std::path::{Path, PathBuf};

use super::cache::{CacheStats, DiscoveryCache};
use super::manifest::ExtensionManifest;
use super::project::ExtensionsConfig;
use super::resolver::{Diagnostic, DiscoveredManifest, Severity};

/// The manifest file name.
pub const MANIFEST_FILE: &str = "regia-plugin.toml";
/// How deep below a root the walk looks.
pub const MAX_DEPTH: usize = 6;

/// The info diagnostic of an engine extension a project extension of the same id replaced
/// (ADR-9850 (a)).
pub const EXTENSION_OVERRIDDEN: &str = "extension_overridden";

/// What [`discover`] found.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// Parsed manifests in discovery order; `path` is the extension directory.
    pub manifests: Vec<DiscoveredManifest>,
    /// Manifests that failed to parse (`manifest_invalid`, `manifest_too_new`, `path_escape`).
    pub diagnostics: Vec<Diagnostic>,
}

/// The discovery roots of a project (spec § 2.5 step 1): `<project>/extensions/`, the workspace
/// members of `<project>/Cargo.toml` (literal paths and `dir/*` globs), each `extension_paths`
/// entry, then `<engine>/extensions/`.
#[must_use]
pub fn project_roots(project: &Path, config: &ExtensionsConfig, engine: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![project.join("extensions")];
    roots.extend(workspace_members(project));
    roots.extend(config.extension_paths.iter().map(|p| project.join(p)));
    if let Some(engine) = engine {
        roots.push(engine.join("extensions"));
    }
    roots
}

/// Discovers the extensions of `project` (spec § 2.5 step 1 over [`project_roots`]) and applies
/// the override rule of ADR-9850 (a): a project extension (one found under a project root) wins
/// over an engine extension (under `<engine>/extensions/`) with the same id. The engine copy leaves
/// the discovery with an [`EXTENSION_OVERRIDDEN`] info diagnostic, so it is neither loaded nor a
/// `duplicate_id`. With `cache_file`, unchanged manifests come from the warm cache.
#[must_use]
pub fn discover_project(
    project: &Path,
    config: &ExtensionsConfig,
    engine: Option<&Path>,
    cache_file: Option<&Path>,
) -> Discovery {
    let roots = project_roots(project, config, engine);
    let mut discovery = match cache_file {
        Some(cache_file) => discover_cached(&roots, cache_file).0,
        None => discover(&roots),
    };
    if let Some(engine) = engine {
        apply_engine_overrides(&mut discovery, &engine.join("extensions"));
    }
    discovery
}

/// The override rule of ADR-9850 (a) on a finished discovery: every manifest under
/// `engine_extensions` whose id a manifest outside it also declares is removed, with an
/// [`EXTENSION_OVERRIDDEN`] info diagnostic naming both paths. Two copies inside the same side
/// (two project copies, or two engine copies) are left for the resolver's `duplicate_id`.
pub fn apply_engine_overrides(discovery: &mut Discovery, engine_extensions: &Path) {
    let in_engine = |path: &str| Path::new(path).starts_with(engine_extensions);
    let project_paths: std::collections::BTreeMap<String, String> = discovery
        .manifests
        .iter()
        .filter(|m| !in_engine(&m.path))
        .map(|m| (m.manifest.id.clone(), m.path.clone()))
        .rev()
        .collect();
    let mut kept = Vec::with_capacity(discovery.manifests.len());
    for found in std::mem::take(&mut discovery.manifests) {
        match project_paths
            .get(&found.manifest.id)
            .filter(|_| in_engine(&found.path))
        {
            Some(project_path) => discovery.diagnostics.push(Diagnostic {
                id: found.manifest.id.clone(),
                code: EXTENSION_OVERRIDDEN,
                severity: Severity::Info,
                message: format!(
                    "the project's `{project_path}` overrides the engine's `{}` ({} {} replaces {})",
                    found.path,
                    found.manifest.id,
                    discovery_version(&kept, project_path),
                    found.manifest.version,
                ),
            }),
            None => kept.push(found),
        }
    }
    discovery.manifests = kept;
}

fn discovery_version(kept: &[DiscoveredManifest], path: &str) -> String {
    kept.iter()
        .find(|m| m.path == path)
        .map(|m| m.manifest.version.to_string())
        .unwrap_or_default()
}

fn workspace_members(project: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(project.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let members = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for member in members.iter().filter_map(toml::Value::as_str) {
        if let Some(dir) = member.strip_suffix("/*") {
            out.extend(sorted_subdirs(&project.join(dir)));
        } else if !member.contains('*') {
            let dir = project.join(member);
            if dir.join(MANIFEST_FILE).is_file() {
                out.push(dir);
            }
        }
    }
    out.retain(|d| d.join(MANIFEST_FILE).is_file());
    out
}

fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "target"
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Hex BLAKE3 of manifest bytes, the hash the resolver folds into `resolved_hash`.
#[must_use]
pub fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Reads and checks one extension directory's manifest.
///
/// # Errors
///
/// A diagnostic with the manifest's error code (the directory name stands in for the id).
pub fn read_extension_dir(dir: &Path) -> Result<DiscoveredManifest, Diagnostic> {
    read_extension_dir_via(dir, None)
}

/// [`read_extension_dir`] that answers an unchanged manifest from `cache` instead of parsing it.
fn read_extension_dir_via(
    dir: &Path,
    mut cache: Option<&mut DiscoveryCache>,
) -> Result<DiscoveredManifest, Diagnostic> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |code: &'static str, message: String| Diagnostic {
        id: name.clone(),
        code,
        severity: Severity::Error,
        message,
    };
    let path = dir.join(MANIFEST_FILE);
    let bytes =
        std::fs::read(&path).map_err(|e| fail("manifest_invalid", format!("{}: {e}", path.display())))?;
    let text = String::from_utf8(bytes.clone())
        .map_err(|_| fail("manifest_invalid", format!("{}: not UTF-8", path.display())))?;
    let shown = dir.display().to_string();
    let hash = hash_hex(&bytes);
    let cached = cache.as_deref_mut().and_then(|c| c.reuse(&shown, &hash));
    let manifest = match cached {
        Some(manifest) => manifest,
        None => {
            let manifest = ExtensionManifest::parse(&text)
                .map_err(|e| fail(e.code(), format!("{}: {e}", path.display())))?;
            if let Some(c) = cache {
                c.parsed(&shown, &hash, &manifest);
            }
            manifest
        }
    };
    if manifest.code.is_empty() && manifest.id != name {
        return Err(fail(
            "manifest_invalid",
            format!(
                "content-only extension `{}` must live in a directory of the same name (found `{name}`)",
                manifest.id
            ),
        ));
    }
    // A crate-published extension: the crate's `[package.metadata.regia] id` must name the same
    // extension (spec § 4.1). A crate without the table is not checked (ADR-7237).
    if let Some(crate_id) = crate_metadata_id(dir)
        && crate_id != manifest.id
    {
        return Err(fail(
            "manifest_invalid",
            format!(
                "extension `{}`: {} says `[package.metadata.regia] id = \"{crate_id}\"`; the two ids must match",
                manifest.id,
                dir.join("Cargo.toml").display()
            ),
        ));
    }
    Ok(DiscoveredManifest {
        manifest,
        path: shown,
        hash,
    })
}

/// `[package.metadata.regia] id` of the crate in `dir`, when `dir/Cargo.toml` has one.
fn crate_metadata_id(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    table
        .get("package")?
        .get("metadata")?
        .get("regia")?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

/// Scans `roots` in order (spec § 2.5 step 1).
#[must_use]
pub fn discover(roots: &[PathBuf]) -> Discovery {
    let _span = tracing::info_span!("extension_discover", roots = roots.len()).entered();
    let mut out = Discovery::default();
    for root in roots {
        walk(root, 0, &mut out, &mut None);
    }
    out
}

/// [`discover`] with the warm cache at `cache_file` (`<project>/.regia/extension-discovery.json`,
/// spec § 10): a manifest whose bytes hash as recorded is not parsed again. The result equals
/// [`discover`]'s; the counters say how many manifests were parsed and how many reused.
#[must_use]
pub fn discover_cached(roots: &[PathBuf], cache_file: &Path) -> (Discovery, CacheStats) {
    let _span = tracing::info_span!("extension_discover", roots = roots.len(), cached = true).entered();
    let mut out = Discovery::default();
    let mut cache = Some(DiscoveryCache::load(cache_file));
    for root in roots {
        walk(root, 0, &mut out, &mut cache);
    }
    let stats = cache.map(|c| c.finish(cache_file)).unwrap_or_default();
    (out, stats)
}

fn walk(dir: &Path, depth: usize, out: &mut Discovery, cache: &mut Option<DiscoveryCache>) {
    if dir.join(MANIFEST_FILE).is_file() {
        match read_extension_dir_via(dir, cache.as_mut()) {
            Ok(found) => {
                // The same directory reached twice (a workspace member under `extensions/`) is
                // one extension, not a duplicate.
                if !out.manifests.iter().any(|m| m.path == found.path) {
                    out.manifests.push(found);
                }
            }
            Err(diag) => out.diagnostics.push(diag),
        }
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    for sub in sorted_subdirs(dir) {
        walk(&sub, depth + 1, out, cache);
    }
}
