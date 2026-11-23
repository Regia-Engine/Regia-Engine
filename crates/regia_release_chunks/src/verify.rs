// SPDX-License-Identifier: MPL-2.0
//! Verify a release directory: manifest bytes, sidecars, and (when asked) archive members.
//!
//! Shallow checks compare each manifest to the files on disk, including SHA-256. Deep checks
//! also decompress every chunk, compare member names, and when a manifest records them, member
//! hashes and `content_key`. Chunk names, the 1.9 GiB cap, and the error codes follow the
//! release-verify requirements at pin `c9513623f` (`regia-absorb-4`). Hashing on a shallow run
//! is the lane brief: the source scripts hashed only with `--deep`.
//!
//! [`verify_tree`](crate::verify_tree) is a different check. It always decompresses one packed
//! tree. This module reads a directory of manifests and does not replace that function.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crate::hash::{hex_encode, sha256_file};
use crate::tar::{self, MemberKind};
use crate::{ChunkCodec, ChunkError, DEFAULT_MAX_CHUNK_BYTES, ZstdCli};

/// One finding. `manifest` is empty when the finding is not owned by one file;
/// JSON renders that as null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyProblem {
    /// Stable check name (`sha256_mismatch`, `tar_members`, and the rest of the set).
    pub check: &'static str,
    /// What failed. Tests match substrings of this text.
    pub message: String,
    /// Manifest file name, or empty when the finding has no owner.
    pub manifest: String,
}

/// One manifest that was checked (tier filters omit the rest; they are listed in `skipped`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedManifest {
    /// File name, for example `manifest-linux-low.json`.
    pub file: String,
    /// Platform label after the JSON value overrides the file name.
    pub platform: String,
    /// Tier label. `None` when the manifest is not tiered.
    pub tier: Option<String>,
    /// Chunk rows.
    pub chunks: usize,
    /// Listed file paths.
    pub files: usize,
    /// Installer links.
    pub links: usize,
}

/// Result of [`verify_release`]. A missing directory is a report, not an error, so a caller
/// can print the problems and exit 1.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ReleaseVerifyReport {
    /// Directory that was asked for.
    pub release_dir: String,
    /// Whether member and content-key checks ran.
    pub deep: bool,
    /// Manifests that were checked.
    pub manifests: Vec<CheckedManifest>,
    /// Manifest file names skipped by the tier filter, in file-name order.
    pub skipped: Vec<String>,
    /// Findings, sorted by manifest, check, then message.
    pub problems: Vec<VerifyProblem>,
}

impl ReleaseVerifyReport {
    /// No findings.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// JSON object: `ok`, `release_dir`, `deep`, `manifests`, `skipped`, `problems`,
    /// `problem_count`. An empty manifest owner is JSON null.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let manifests = self
            .manifests
            .iter()
            .map(|manifest| {
                let own: Vec<Value> = self
                    .problems
                    .iter()
                    .filter(|problem| problem.manifest == manifest.file)
                    .map(|problem| {
                        serde_json::json!({
                            "check": problem.check,
                            "message": problem.message,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "file": manifest.file,
                    "platform": manifest.platform,
                    "tier": manifest.tier,
                    "chunks": manifest.chunks,
                    "files": manifest.files,
                    "links": manifest.links,
                    "ok": own.is_empty(),
                    "problems": own,
                })
            })
            .collect::<Vec<_>>();
        let problems = self
            .problems
            .iter()
            .map(|problem| {
                serde_json::json!({
                    "manifest": if problem.manifest.is_empty() {
                        Value::Null
                    } else {
                        Value::String(problem.manifest.clone())
                    },
                    "check": problem.check,
                    "message": problem.message,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "ok": self.ok(),
            "release_dir": self.release_dir,
            "deep": self.deep,
            "manifests": manifests,
            "skipped": self.skipped,
            "problems": problems,
            "problem_count": self.problems.len(),
        })
    }

    /// Pretty JSON, trailing newline.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut text = serde_json::to_string_pretty(&self.to_value()).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        text
    }

    /// Short human report. The JSON form is the one callers assert on.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut lines = Vec::new();
        for manifest in &self.manifests {
            let own = self
                .problems
                .iter()
                .filter(|problem| problem.manifest == manifest.file)
                .count();
            let tier = manifest.tier.as_deref().unwrap_or("-");
            let status = if own == 0 {
                "OK".to_string()
            } else {
                format!("{own} problem(s)")
            };
            lines.push(format!(
                "{}: platform={} tier={tier} chunks={} files={} links={} {status}",
                manifest.file, manifest.platform, manifest.chunks, manifest.files, manifest.links
            ));
        }
        for name in &self.skipped {
            lines.push(format!("{name}: skipped (tier not selected)"));
        }
        if self.problems.is_empty() {
            lines.push("OK: no problems found".to_string());
        } else {
            lines.push("Problems:".to_string());
            for problem in &self.problems {
                let where_ = if problem.manifest.is_empty() {
                    String::new()
                } else {
                    format!("{}: ", problem.manifest)
                };
                lines.push(format!("  [{}] {where_}{}", problem.check, problem.message));
            }
            lines.push(format!("FAILED: {} problem(s)", self.problems.len()));
        }
        lines.join("\n")
    }
}

/// What [`verify_release`] should check. An empty `tiers` list means every tier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Decompress chunks and check members, member hashes, and `content_key`.
    pub deep: bool,
    /// Tiers to check. Empty checks all. A manifest with no tier is never skipped.
    pub tiers: Vec<String>,
    /// When true, a manifest with no detached signature is a `manifest_signature` problem.
    /// A signature that is present is checked either way.
    pub require_signature: bool,
    /// Public keys (`ed25519:` plus 64 hex digits) allowed to sign. Empty means the key
    /// embedded in the signature is enough to prove the bytes were not modified.
    pub trusted_keys: Vec<String>,
}

/// Verify `dir`. Always returns a report. A missing directory, a bad manifest, and a
/// byte that does not match are findings, not a [`ChunkError`].
pub fn verify_release(dir: &Path, opts: &VerifyOptions) -> ReleaseVerifyReport {
    let mut problems = Vec::new();
    let mut checked = Vec::new();
    let mut skipped_rows = Vec::new();
    let mut skipped = Vec::new();
    let release_dir = dir.display().to_string();
    if !dir.is_dir() {
        problems.push(problem(
            "release_dir",
            format!("release directory {release_dir} does not exist"),
            "",
        ));
        return finish(release_dir, opts.deep, Vec::new(), skipped, problems);
    }
    let paths = match list_manifests(dir) {
        Ok(paths) => paths,
        Err(err) => {
            problems.push(problem("release_dir", err.message, ""));
            return finish(release_dir, opts.deep, Vec::new(), skipped, problems);
        }
    };
    if paths.is_empty() {
        problems.push(problem(
            "release_dir",
            format!("no manifest-*.json files in {release_dir}"),
            "",
        ));
        return finish(release_dir, opts.deep, Vec::new(), skipped, problems);
    }
    let filter: Option<BTreeSet<&str>> = if opts.tiers.is_empty() {
        None
    } else {
        Some(opts.tiers.iter().map(String::as_str).collect())
    };
    for path in &paths {
        let Some(row) = load_json(path, &mut problems) else {
            continue;
        };
        if let Some(filter) = &filter
            && let Some(tier) = &row.tier
            && !filter.contains(tier.as_str())
        {
            skipped.push(row.file_name.clone());
            skipped_rows.push(row);
            continue;
        }
        bind_tsv_and_sidecar(dir, &row, &mut problems);
        check_paths_and_links(&row, &mut problems);
        check_group_gaps(&row, &mut problems);
        checked.push(row);
    }
    let existing = check_assets(dir, &checked, &mut problems);
    check_hashes(&checked, &existing, &mut problems);
    if opts.deep {
        check_deep(&checked, &existing, &mut problems);
    }
    check_cross_manifest(&checked, &mut problems);
    check_directory(dir, &checked, &skipped_rows, &mut problems);
    crate::sign::check_signatures(dir, &paths, &skipped, opts, &mut problems);
    let views = checked.iter().map(CheckedManifest::from).collect();
    finish(release_dir, opts.deep, views, skipped, problems)
}

fn finish(
    release_dir: String,
    deep: bool,
    manifests: Vec<CheckedManifest>,
    skipped: Vec<String>,
    mut problems: Vec<VerifyProblem>,
) -> ReleaseVerifyReport {
    problems.sort_by(|left, right| {
        left.manifest
            .cmp(&right.manifest)
            .then(left.check.cmp(right.check))
            .then(left.message.cmp(&right.message))
    });
    ReleaseVerifyReport {
        release_dir,
        deep,
        manifests,
        skipped,
        problems,
    }
}

fn problem(check: &'static str, message: impl Into<String>, manifest: &str) -> VerifyProblem {
    VerifyProblem {
        check,
        message: message.into(),
        manifest: manifest.to_string(),
    }
}

struct FileRef {
    path: String,
    sha256: Option<String>,
}

struct ChunkRow {
    name: String,
    asset: String,
    sha256: String,
    size: i64,
    unpacked: i64,
    files: Vec<FileRef>,
    content_key: String,
}

struct ManifestRow {
    file_name: String,
    stem: String,
    platform: String,
    tier: Option<String>,
    headers: BTreeMap<String, String>,
    chunks: Vec<ChunkRow>,
    links: Vec<(String, String)>,
}

impl From<&ManifestRow> for CheckedManifest {
    fn from(row: &ManifestRow) -> Self {
        Self {
            file: row.file_name.clone(),
            platform: row.platform.clone(),
            tier: row.tier.clone(),
            chunks: row.chunks.len(),
            files: row.chunks.iter().map(|chunk| chunk.files.len()).sum(),
            links: row.links.len(),
        }
    }
}

pub(crate) fn list_manifests(dir: &Path) -> Result<Vec<PathBuf>, ChunkError> {
    let mut preferred = Vec::new();
    let mut other = Vec::new();
    let entries = fs::read_dir(dir).map_err(|err| ChunkError::io(dir, &err))?;
    for entry in entries {
        let entry = entry.map_err(|err| ChunkError::io(dir, &err))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".json.sha256") {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if name.starts_with("manifest-") {
            preferred.push(path);
        } else {
            other.push(path);
        }
    }
    preferred.sort();
    if !preferred.is_empty() {
        return Ok(preferred);
    }
    other.sort();
    Ok(other)
}

fn load_json(path: &Path, problems: &mut Vec<VerifyProblem>) -> Option<ManifestRow> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let stem = file_name.strip_suffix(".json").unwrap_or(&file_name).to_string();
    let (file_platform, file_tier) = filename_platform_tier(&file_name);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            problems.push(problem(
                "manifest_parse",
                format!("{file_name}: cannot parse JSON: {err}"),
                &file_name,
            ));
            return None;
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            problems.push(problem(
                "manifest_parse",
                format!("{file_name}: cannot parse JSON: {err}"),
                &file_name,
            ));
            return None;
        }
    };
    let Some(obj) = parsed.as_object() else {
        problems.push(problem(
            "manifest_parse",
            format!("{file_name}: top-level JSON is not an object"),
            &file_name,
        ));
        return None;
    };
    if let Some(raw) = obj.get("format").filter(|value| !value.is_null())
        && as_i64(raw) != Some(1)
    {
        problems.push(problem(
            "manifest_parse",
            format!("{file_name}: unsupported format value {raw} (expected 1)"),
            &file_name,
        ));
    }
    if let Some(raw) = obj.get("format_version").filter(|value| !value.is_null())
        && as_i64(raw) != Some(1)
    {
        problems.push(problem(
            "manifest_parse",
            format!("{file_name}: unsupported format value {raw} (expected 1)"),
            &file_name,
        ));
    }
    let mut headers = BTreeMap::new();
    for (canonical, aliases) in HEADER_ALIASES {
        if let Some(value) = first_header(obj, aliases) {
            headers.insert((*canonical).to_string(), value);
        }
    }
    let platform = headers
        .get("platform")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or(file_platform);
    let tier = headers
        .get("tier")
        .filter(|value| !value.is_empty())
        .cloned()
        .or(file_tier);
    let mut chunks = Vec::new();
    match obj.get("chunks") {
        Some(Value::Array(entries)) => {
            for entry in entries {
                let Some(entry) = entry.as_object() else {
                    problems.push(problem(
                        "manifest_parse",
                        format!("{file_name}: chunk entry is not an object"),
                        &file_name,
                    ));
                    continue;
                };
                chunks.push(load_chunk(entry, &file_name, problems));
            }
        }
        Some(_) => {
            problems.push(problem(
                "manifest_parse",
                format!("{file_name}: 'chunks' is missing or not a list"),
                &file_name,
            ));
        }
        None => {
            problems.push(problem(
                "manifest_parse",
                format!("{file_name}: 'chunks' is missing or not a list"),
                &file_name,
            ));
        }
    }
    let mut links = Vec::new();
    match obj.get("links") {
        Some(Value::Array(entries)) => {
            for entry in entries {
                match parse_link(entry) {
                    Some(pair) => links.push(pair),
                    None => problems.push(problem(
                        "manifest_parse",
                        format!("{file_name}: link entry is not a [path, target] pair: {entry}"),
                        &file_name,
                    )),
                }
            }
        }
        Some(_) => problems.push(problem(
            "manifest_parse",
            format!("{file_name}: 'links' is not a list"),
            &file_name,
        )),
        None => {}
    }
    Some(ManifestRow {
        file_name,
        stem,
        platform,
        tier,
        headers,
        chunks,
        links,
    })
}

const HEADER_ALIASES: &[(&str, &[&str])] = &[
    ("format", &["format", "format_version"]),
    ("tag", &["tag"]),
    ("version", &["version", "product_version"]),
    ("git_sha", &["git_sha", "source_commit"]),
    ("platform", &["platform"]),
    ("tier", &["tier"]),
    ("glibc_required", &["glibc_required", "libc_required"]),
];

fn first_header(obj: &Map<String, Value>, aliases: &[&str]) -> Option<String> {
    for key in aliases {
        if let Some(value) = obj.get(*key).filter(|value| !value.is_null()) {
            return Some(header_string(value));
        }
    }
    None
}

fn header_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|wide| i64::try_from(wide).ok())),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn load_chunk(entry: &Map<String, Value>, file_name: &str, problems: &mut Vec<VerifyProblem>) -> ChunkRow {
    let name = entry.get("name").map(header_string).unwrap_or_default();
    let size = as_i64(entry.get("size").unwrap_or(&Value::Null));
    let unpacked = as_i64(entry.get("unpacked").unwrap_or(&Value::Null));
    if size.is_none() || unpacked.is_none() {
        problems.push(problem(
            "manifest_parse",
            format!("{file_name}: chunk {name:?} has a non-integer size/unpacked"),
            file_name,
        ));
    }
    let files = match entry.get("files") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| parse_file_ref(item, file_name, problems))
            .collect(),
        _ => Vec::new(),
    };
    ChunkRow {
        name,
        asset: entry.get("asset").map(header_string).unwrap_or_default(),
        sha256: entry
            .get("sha256")
            .map(header_string)
            .unwrap_or_default()
            .to_ascii_lowercase(),
        size: size.unwrap_or(-1),
        unpacked: unpacked.unwrap_or(-1),
        files,
        content_key: entry
            .get("content_key")
            .map(header_string)
            .unwrap_or_default()
            .to_ascii_lowercase(),
    }
}

fn parse_file_ref(value: &Value, file_name: &str, problems: &mut Vec<VerifyProblem>) -> Option<FileRef> {
    match value {
        Value::String(path) => Some(FileRef {
            path: path.clone(),
            sha256: None,
        }),
        Value::Object(obj) => {
            let path = ["path", "name", "file"]
                .iter()
                .find_map(|key| obj.get(*key).and_then(Value::as_str));
            let Some(path) = path else {
                problems.push(problem(
                    "manifest_parse",
                    format!("{file_name}: file entry has no path"),
                    file_name,
                ));
                return None;
            };
            let sha256 = obj
                .get("sha256")
                .and_then(Value::as_str)
                .map(|hash| hash.to_ascii_lowercase());
            Some(FileRef {
                path: path.to_string(),
                sha256,
            })
        }
        _ => {
            problems.push(problem(
                "manifest_parse",
                format!("{file_name}: file entry is not a path"),
                file_name,
            ));
            None
        }
    }
}

fn parse_link(value: &Value) -> Option<(String, String)> {
    if let Some(pair) = value.as_array()
        && pair.len() == 2
        && let (Some(path), Some(target)) = (pair[0].as_str(), pair[1].as_str())
    {
        return Some((path.to_string(), target.to_string()));
    }
    if let Some(obj) = value.as_object() {
        let path = ["path", "link", "name"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_str));
        let target = ["target", "source", "to"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_str));
        if let (Some(path), Some(target)) = (path, target) {
            return Some((path.to_string(), target.to_string()));
        }
    }
    None
}

/// `manifest-<platform>-<tier>.json` (hyphen, first tier segment only) and
/// `manifest-<platform>.<tier>.json` (dot). Other names contribute no platform.
fn filename_platform_tier(file_name: &str) -> (String, Option<String>) {
    let stem = file_name.strip_suffix(".json").unwrap_or(file_name);
    let Some(rest) = stem.strip_prefix("manifest-") else {
        return ("unknown".to_string(), None);
    };
    if rest.is_empty() {
        return ("unknown".to_string(), None);
    }
    if let Some((platform, tier)) = rest.split_once('.') {
        let platform = if platform.is_empty() { "unknown" } else { platform };
        let tier = if tier.is_empty() {
            None
        } else {
            Some(tier.to_string())
        };
        return (platform.to_string(), tier);
    }
    let mut parts = rest.split('-');
    let platform = parts.next().filter(|part| !part.is_empty()).unwrap_or("unknown");
    let tier = parts.next().filter(|part| !part.is_empty()).map(str::to_string);
    (platform.to_string(), tier)
}

fn bind_tsv_and_sidecar(dir: &Path, row: &ManifestRow, problems: &mut Vec<VerifyProblem>) {
    let own = dir.join(format!("{}.sha256", row.file_name));
    let canonical = canonical_sidecar(dir, &own, row);
    let (tsv_path, sidecar_names) = if canonical.is_some() {
        (dir.join("release-chunks.tsv"), None)
    } else {
        (dir.join(format!("{}.tsv", row.stem)), Some(own))
    };
    let tsv_required = row.platform == "linux" || tsv_path.is_file();
    if tsv_path.is_file() {
        if let Some(tsv) = load_tsv(&tsv_path, &row.file_name, problems) {
            check_agreement(
                row,
                &tsv,
                tsv_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("manifest.tsv"),
                problems,
            );
        }
    } else if tsv_required {
        let name = tsv_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("manifest.tsv");
        problems.push(problem(
            "missing_manifest_file",
            format!("{name}: TSV manifest is missing"),
            &row.file_name,
        ));
    }
    if let Some(seen) = canonical {
        check_fallback_sidecar(dir, &seen, tsv_required, &row.file_name, problems);
    } else if let Some(path) = sidecar_names {
        check_own_sidecar(dir, row, &path, &tsv_path, tsv_required, problems);
    }
}

/// C-15 writes one sidecar for `release-chunks.json` and copies the same JSON to
/// `manifest-<platform>[.<tier>].json` without a second sidecar. Accept that sidecar
/// when this file's bytes hash to the `release-chunks.json` line.
fn canonical_sidecar(dir: &Path, own: &Path, row: &ManifestRow) -> Option<BTreeMap<String, String>> {
    if own.is_file() || row.file_name == "release-chunks.json" {
        return None;
    }
    let path = dir.join("release-chunks.json.sha256");
    if !path.is_file() {
        return None;
    }
    let actual = hash_file(&dir.join(&row.file_name)).ok()?;
    let text = fs::read_to_string(&path).ok()?;
    let seen = sidecar_map(&text).ok()?;
    if seen
        .get("release-chunks.json")
        .is_some_and(|listed| listed == &actual)
    {
        Some(seen)
    } else {
        None
    }
}

fn check_fallback_sidecar(
    dir: &Path,
    seen: &BTreeMap<String, String>,
    tsv_required: bool,
    owner: &str,
    problems: &mut Vec<VerifyProblem>,
) {
    if !tsv_required {
        return;
    }
    let name = "release-chunks.tsv";
    let Some(listed) = seen.get(name) else {
        problems.push(problem(
            "sidecar_sha256",
            format!("release-chunks.json.sha256: no entry for {name}"),
            owner,
        ));
        return;
    };
    let target = dir.join(name);
    if !target.is_file() {
        return;
    }
    match hash_file(&target) {
        Ok(actual) if actual != *listed => problems.push(problem(
            "sidecar_sha256",
            format!(
                "{name}: sha256 mismatch with release-chunks.json.sha256 (sidecar {listed}, actual {actual})"
            ),
            owner,
        )),
        Ok(_) => {}
        Err(err) => problems.push(problem("io_error", err.message, owner)),
    }
}

fn check_own_sidecar(
    dir: &Path,
    row: &ManifestRow,
    sidecar: &Path,
    tsv_path: &Path,
    tsv_required: bool,
    problems: &mut Vec<VerifyProblem>,
) {
    let owner = &row.file_name;
    let sidecar_name = sidecar
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sidecar.sha256");
    if !sidecar.is_file() {
        problems.push(problem(
            "missing_manifest_file",
            format!("{sidecar_name}: sidecar file is missing"),
            owner,
        ));
        return;
    }
    let text = match fs::read_to_string(sidecar) {
        Ok(text) => text,
        Err(err) => {
            problems.push(problem(
                "sidecar_sha256",
                format!("{sidecar_name}: cannot read sidecar: {err}"),
                owner,
            ));
            return;
        }
    };
    let mut seen = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((hash, rest)) = split_ws(line) else {
            problems.push(problem(
                "sidecar_sha256",
                format!("{sidecar_name}: malformed sha256sum line: {line:?}"),
                owner,
            ));
            continue;
        };
        let name = sidecar_name_of(rest);
        seen.insert(name, hash.to_ascii_lowercase());
    }
    let mut expected = BTreeSet::new();
    expected.insert(row.file_name.clone());
    if tsv_required && let Some(name) = tsv_path.file_name().and_then(|n| n.to_str()) {
        expected.insert(name.to_string());
    }
    let seen_names: BTreeSet<String> = seen.keys().cloned().collect();
    for name in expected.difference(&seen_names) {
        problems.push(problem(
            "sidecar_sha256",
            format!("{sidecar_name}: no entry for {name}"),
            owner,
        ));
    }
    for name in seen.keys() {
        if !expected.contains(name) {
            problems.push(problem(
                "sidecar_sha256",
                format!("{sidecar_name}: unexpected entry for {name}"),
                owner,
            ));
        }
    }
    for name in &expected {
        let Some(listed) = seen.get(name) else {
            continue;
        };
        let target = dir.join(name);
        if !target.is_file() {
            continue;
        }
        match hash_file(&target) {
            Ok(actual) if &actual != listed => problems.push(problem(
                "sidecar_sha256",
                format!("{name}: sha256 mismatch with {sidecar_name} (sidecar {listed}, actual {actual})"),
                owner,
            )),
            Ok(_) => {}
            Err(err) => problems.push(problem("io_error", err.message, owner)),
        }
    }
}

fn sidecar_map(text: &str) -> Result<BTreeMap<String, String>, ()> {
    let mut seen = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((hash, rest)) = split_ws(line) else {
            return Err(());
        };
        seen.insert(sidecar_name_of(rest), hash.to_ascii_lowercase());
    }
    Ok(seen)
}

fn split_ws(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let end = trimmed.find(char::is_whitespace)?;
    let (hash, rest) = trimmed.split_at(end);
    Some((hash, rest.trim()))
}

fn sidecar_name_of(rest: &str) -> String {
    let name = rest.trim().trim_start_matches('*').trim();
    Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name)
        .to_string()
}

struct TsvBody {
    headers: BTreeMap<String, String>,
    chunks: BTreeMap<String, BTreeMap<String, String>>,
    files: BTreeMap<String, Vec<String>>,
    links: Vec<(String, String)>,
}

fn load_tsv(path: &Path, owner: &str, problems: &mut Vec<VerifyProblem>) -> Option<TsvBody> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("manifest.tsv");
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            problems.push(problem(
                "manifest_parse",
                format!("{name}: cannot read TSV: {err}"),
                owner,
            ));
            return None;
        }
    };
    let mut body = TsvBody {
        headers: BTreeMap::new(),
        chunks: BTreeMap::new(),
        files: BTreeMap::new(),
        links: Vec::new(),
    };
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let kind = cols.first().copied().unwrap_or("");
        let line_no = index + 1;
        if let Some(canonical) = canonical_tsv_key(kind)
            && cols.len() == 2
        {
            body.headers.insert(canonical.to_string(), cols[1].to_string());
        } else if kind == "chunk" && cols.len() == 6 {
            if body.chunks.contains_key(cols[1]) {
                problems.push(problem(
                    "json_tsv_mismatch",
                    format!("{name}:{line_no}: duplicate chunk row for {:?}", cols[1]),
                    owner,
                ));
            }
            let mut record = BTreeMap::new();
            record.insert("asset".to_string(), cols[2].to_string());
            record.insert("sha256".to_string(), cols[3].to_ascii_lowercase());
            record.insert("size".to_string(), cols[4].to_string());
            record.insert("unpacked".to_string(), cols[5].to_string());
            body.chunks.insert(cols[1].to_string(), record);
        } else if kind == "file" && cols.len() == 3 {
            body.files
                .entry(cols[1].to_string())
                .or_default()
                .push(cols[2].to_string());
        } else if kind == "link" && cols.len() == 3 {
            body.links.push((cols[1].to_string(), cols[2].to_string()));
        } else {
            problems.push(problem(
                "json_tsv_mismatch",
                format!("{name}:{line_no}: unrecognized or malformed row: {line:?}"),
                owner,
            ));
        }
    }
    Some(body)
}

fn canonical_tsv_key(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "format" | "format_version" => "format",
        "tag" => "tag",
        "version" | "product_version" => "version",
        "git_sha" | "source_commit" => "git_sha",
        "platform" => "platform",
        "tier" => "tier",
        "glibc_required" | "libc_required" => "glibc_required",
        _ => return None,
    })
}

fn check_agreement(row: &ManifestRow, tsv: &TsvBody, tsv_name: &str, problems: &mut Vec<VerifyProblem>) {
    let owner = &row.file_name;
    let mut keys = BTreeSet::new();
    keys.extend(row.headers.keys().cloned());
    keys.extend(tsv.headers.keys().cloned());
    for key in keys {
        if (key == "git_sha" || key == "platform") && !tsv.headers.contains_key(&key) {
            continue;
        }
        let json_value = row.headers.get(&key);
        let tsv_value = tsv.headers.get(&key);
        if json_value != tsv_value {
            let json_shown = json_value.map_or("missing", String::as_str);
            let tsv_shown = tsv_value.map_or("missing", String::as_str);
            problems.push(problem(
                "json_tsv_mismatch",
                format!(
                    "{owner} vs {tsv_name}: header {key:?} differs (JSON {json_shown:?}, TSV {tsv_shown:?})"
                ),
                owner,
            ));
        }
    }
    let json_names: BTreeSet<&str> = row.chunks.iter().map(|chunk| chunk.name.as_str()).collect();
    let tsv_names: BTreeSet<&str> = tsv.chunks.keys().map(String::as_str).collect();
    if json_names != tsv_names {
        let only_json: Vec<&str> = json_names.difference(&tsv_names).copied().collect();
        let only_tsv: Vec<&str> = tsv_names.difference(&json_names).copied().collect();
        problems.push(problem(
            "json_tsv_mismatch",
            format!("{owner} vs {tsv_name}: chunk name sets differ (JSON only {only_json:?}, TSV only {only_tsv:?})"),
            owner,
        ));
    }
    for chunk in &row.chunks {
        let Some(record) = tsv.chunks.get(&chunk.name) else {
            continue;
        };
        if let Some(asset) = record.get("asset")
            && asset != &chunk.asset
        {
            problems.push(problem(
                "json_tsv_mismatch",
                format!(
                    "{owner} vs {tsv_name}: chunk {:?} asset differs (JSON {:?}, TSV {asset:?})",
                    chunk.name, chunk.asset
                ),
                owner,
            ));
        }
        if record.get("sha256").is_some_and(|sha| sha != &chunk.sha256) {
            problems.push(problem(
                "json_tsv_mismatch",
                format!(
                    "{owner} vs {tsv_name}: chunk {:?} sha256 differs between JSON and TSV",
                    chunk.name
                ),
                owner,
            ));
        }
        if record
            .get("size")
            .is_some_and(|size| size != &chunk.size.to_string())
        {
            problems.push(problem(
                "json_tsv_mismatch",
                format!(
                    "{owner} vs {tsv_name}: chunk {:?} size differs (JSON {}, TSV {})",
                    chunk.name,
                    chunk.size,
                    record.get("size").map(String::as_str).unwrap_or("")
                ),
                owner,
            ));
        }
        if record
            .get("unpacked")
            .is_some_and(|size| size != &chunk.unpacked.to_string())
        {
            problems.push(problem(
                "json_tsv_mismatch",
                format!(
                    "{owner} vs {tsv_name}: chunk {:?} unpacked differs (JSON {}, TSV {})",
                    chunk.name,
                    chunk.unpacked,
                    record.get("unpacked").map(String::as_str).unwrap_or("")
                ),
                owner,
            ));
        }
        let tsv_files = tsv.files.get(&chunk.name).map(Vec::as_slice).unwrap_or(&[]);
        let json_files: Vec<&str> = chunk.files.iter().map(|file| file.path.as_str()).collect();
        if json_files != tsv_files.iter().map(String::as_str).collect::<Vec<_>>() {
            problems.push(problem(
                "json_tsv_mismatch",
                format!("{owner} vs {tsv_name}: chunk {:?} file lists differ", chunk.name),
                owner,
            ));
        }
    }
    for name in tsv.files.keys() {
        if !json_names.contains(name.as_str()) {
            problems.push(problem(
                "json_tsv_mismatch",
                format!("{owner} vs {tsv_name}: TSV lists files for unknown chunk {name:?}"),
                owner,
            ));
        }
    }
    if row.links != tsv.links {
        problems.push(problem(
            "json_tsv_mismatch",
            format!("{owner} vs {tsv_name}: link lists differ"),
            owner,
        ));
    }
}

fn unsafe_reason(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        Some("path is empty")
    } else if path.contains('\\') {
        Some("path contains a backslash")
    } else if path.starts_with('/') {
        Some("path is absolute")
    } else if path.split('/').any(|part| part == "..") {
        Some("path contains a '..' component")
    } else {
        None
    }
}

fn check_paths_and_links(row: &ManifestRow, problems: &mut Vec<VerifyProblem>) {
    let mut owner_of: BTreeMap<&str, &str> = BTreeMap::new();
    for chunk in &row.chunks {
        for file in &chunk.files {
            if let Some(reason) = unsafe_reason(&file.path) {
                problems.push(problem(
                    "unsafe_path",
                    format!("chunk {:?} file {:?}: {reason}", chunk.name, file.path),
                    &row.file_name,
                ));
            }
            if let Some(previous) = owner_of.get(file.path.as_str()) {
                problems.push(problem(
                    "duplicate_path",
                    format!(
                        "path {:?} is listed in both chunk {previous:?} and chunk {:?}",
                        file.path, chunk.name
                    ),
                    &row.file_name,
                ));
            } else {
                owner_of.insert(file.path.as_str(), chunk.name.as_str());
            }
        }
    }
    for (link_path, target) in &row.links {
        for (label, value) in [("link path", link_path), ("link target", target)] {
            if let Some(reason) = unsafe_reason(value) {
                problems.push(problem(
                    "unsafe_path",
                    format!("{label} {value:?}: {reason}"),
                    &row.file_name,
                ));
            }
        }
        if !owner_of.contains_key(target.as_str()) {
            problems.push(problem(
                "link_target",
                format!("link {link_path:?} target {target:?} is not a listed file"),
                &row.file_name,
            ));
        }
        if owner_of.contains_key(link_path.as_str()) {
            problems.push(problem(
                "link_path_is_file",
                format!("link path {link_path:?} is also listed as a file"),
                &row.file_name,
            ));
        }
    }
}

fn check_assets(
    dir: &Path,
    rows: &[ManifestRow],
    problems: &mut Vec<VerifyProblem>,
) -> BTreeMap<String, PathBuf> {
    let mut existing = BTreeMap::new();
    for row in rows {
        for chunk in &row.chunks {
            let plain =
                Path::new(&chunk.asset).file_name().and_then(|n| n.to_str()) == Some(chunk.asset.as_str());
            if !plain || unsafe_reason(&chunk.asset).is_some() {
                problems.push(problem(
                    "unsafe_path",
                    format!(
                        "chunk {:?} asset {:?} is not a plain file name",
                        chunk.name, chunk.asset
                    ),
                    &row.file_name,
                ));
                continue;
            }
            let asset_path = dir.join(&chunk.asset);
            if !asset_path.is_file() {
                problems.push(problem(
                    "missing_asset",
                    format!("chunk {:?} asset {} is missing", chunk.name, chunk.asset),
                    &row.file_name,
                ));
                continue;
            }
            let actual = match fs::metadata(&asset_path) {
                Ok(meta) => meta.len(),
                Err(err) => {
                    problems.push(problem(
                        "io_error",
                        format!("{}: {err}", asset_path.display()),
                        &row.file_name,
                    ));
                    continue;
                }
            };
            existing.insert(chunk.asset.clone(), asset_path);
            let differs = i64::try_from(actual).ok() != Some(chunk.size);
            if differs {
                let shown = i64::try_from(actual).map_or_else(|_| actual.to_string(), |n| n.to_string());
                problems.push(problem(
                    "size_mismatch",
                    format!(
                        "asset {} size is {shown}, manifest lists {}",
                        chunk.asset, chunk.size
                    ),
                    &row.file_name,
                ));
            }
        }
    }
    existing
}

fn check_hashes(
    rows: &[ManifestRow],
    existing: &BTreeMap<String, PathBuf>,
    problems: &mut Vec<VerifyProblem>,
) {
    let mut cache: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        for chunk in &row.chunks {
            let Some(path) = existing.get(&chunk.asset) else {
                continue;
            };
            let Ok(meta) = fs::metadata(path) else {
                continue;
            };
            if meta.len() > DEFAULT_MAX_CHUNK_BYTES {
                continue;
            }
            let actual = if let Some(hit) = cache.get(&chunk.asset) {
                hit.clone()
            } else {
                match hash_file(path) {
                    Ok(hex) => {
                        cache.insert(chunk.asset.clone(), hex.clone());
                        hex
                    }
                    Err(err) => {
                        problems.push(problem("io_error", err.message, &row.file_name));
                        continue;
                    }
                }
            };
            if actual != chunk.sha256 {
                problems.push(problem(
                    "sha256_mismatch",
                    format!(
                        "asset {} sha256 is {actual}, manifest lists {}",
                        chunk.asset, chunk.sha256
                    ),
                    &row.file_name,
                ));
            }
        }
    }
}

struct MemberExpect {
    paths: Vec<String>,
    hashes: BTreeMap<String, String>,
    content_key: String,
}

fn check_deep(rows: &[ManifestRow], existing: &BTreeMap<String, PathBuf>, problems: &mut Vec<VerifyProblem>) {
    let mut expects: BTreeMap<(String, Vec<String>), MemberExpect> = BTreeMap::new();
    for row in rows {
        for chunk in &row.chunks {
            if !existing.contains_key(&chunk.asset) {
                continue;
            }
            let paths: Vec<String> = chunk.files.iter().map(|file| file.path.clone()).collect();
            let entry = expects
                .entry((chunk.asset.clone(), paths.clone()))
                .or_insert_with(|| MemberExpect {
                    paths,
                    hashes: BTreeMap::new(),
                    content_key: String::new(),
                });
            for file in &chunk.files {
                if let Some(sha) = &file.sha256 {
                    entry
                        .hashes
                        .entry(file.path.clone())
                        .or_insert_with(|| sha.clone());
                }
            }
            if entry.content_key.is_empty() && !chunk.content_key.is_empty() {
                entry.content_key.clone_from(&chunk.content_key);
            }
        }
    }
    let mut inspected: BTreeMap<String, Result<Inspected, ChunkError>> = BTreeMap::new();
    let assets: BTreeSet<String> = expects.keys().map(|(asset, _)| asset.clone()).collect();
    for asset in assets {
        let Some(path) = existing.get(&asset) else {
            continue;
        };
        inspected.insert(asset, inspect_archive(path));
    }
    for ((asset, _), expect) in &expects {
        let Some(outcome) = inspected.get(asset) else {
            continue;
        };
        match outcome {
            Ok(inspected) => {
                compare_members(asset, expect, inspected, problems);
                if !expect.content_key.is_empty() && expect.content_key != inspected.content_key {
                    problems.push(problem(
                        "content_key_mismatch",
                        format!(
                            "asset {asset} content_key is {}, manifest lists {}",
                            inspected.content_key, expect.content_key
                        ),
                        "",
                    ));
                }
            }
            Err(err) if err.code == "zstd_missing" => {
                problems.push(problem(
                    "zstd_missing",
                    format!("asset {asset}: cannot list members: {}", err.message),
                    "",
                ));
            }
            Err(err) => {
                problems.push(problem(
                    "tar_members",
                    format!("asset {asset}: cannot list members: {}", err.message),
                    "",
                ));
            }
        }
    }
}

struct Inspected {
    names: Vec<String>,
    file_sha256: BTreeMap<String, String>,
    content_key: String,
}

fn inspect_archive(asset: &Path) -> Result<Inspected, ChunkError> {
    let temp = temp_tar_path(asset);
    let _guard = TempFile(temp.clone());
    ZstdCli::standard().decompress_file(asset, &temp)?;
    let members = tar::read_members(&temp)?;
    let content_key = hex_encode(&sha256_file(&temp)?);
    let mut names = Vec::new();
    let mut file_sha256 = BTreeMap::new();
    for member in members {
        let name = normalize_member(&member.name).to_string();
        if member.kind == MemberKind::File
            && let Some(hash) = member.file_sha256
        {
            file_sha256.insert(name.clone(), hash);
        }
        names.push(name);
    }
    Ok(Inspected {
        names,
        file_sha256,
        content_key,
    })
}

fn compare_members(
    asset: &str,
    expect: &MemberExpect,
    inspected: &Inspected,
    problems: &mut Vec<VerifyProblem>,
) {
    let expected: BTreeSet<&str> = expect.paths.iter().map(String::as_str).collect();
    let present: BTreeSet<&str> = inspected.names.iter().map(String::as_str).collect();
    let missing: Vec<String> = expected
        .difference(&present)
        .copied()
        .map(str::to_string)
        .collect();
    let extra: Vec<String> = present
        .difference(&expected)
        .copied()
        .map(str::to_string)
        .collect();
    let unique = present.len();
    if !missing.is_empty() || !extra.is_empty() || inspected.names.len() != unique {
        problems.push(problem(
            "tar_members",
            format!(
                "asset {asset}: member list differs from manifest (missing {}, extra {})",
                summarize(&missing),
                summarize(&extra)
            ),
            "",
        ));
    }
    for (path, listed) in &expect.hashes {
        let Some(actual) = inspected.file_sha256.get(path) else {
            continue;
        };
        if actual != listed {
            problems.push(problem(
                "member_sha256",
                format!("asset {asset} member {path} sha256 is {actual}, manifest lists {listed}"),
                "",
            ));
        }
    }
}

fn summarize(names: &[String]) -> String {
    const LIMIT: usize = 25;
    if names.len() <= LIMIT {
        format!("{names:?}")
    } else {
        format!("{:?}, ... and {} more", &names[..LIMIT], names.len() - LIMIT)
    }
}

fn normalize_member(name: &str) -> &str {
    let mut rest = name;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    rest
}

fn temp_tar_path(asset: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let stem = asset
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("chunk");
    std::env::temp_dir().join(format!(
        "regia-release-verify-{}-{n}-{stem}.tar",
        std::process::id()
    ))
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn hash_file(path: &Path) -> Result<String, ChunkError> {
    Ok(hex_encode(&sha256_file(path)?))
}

fn check_group_gaps(row: &ManifestRow, problems: &mut Vec<VerifyProblem>) {
    let mut groups: BTreeMap<(String, u32), BTreeSet<u32>> = BTreeMap::new();
    let mut totals: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for chunk in &row.chunks {
        let Some(part) = parse_part(&chunk.name) else {
            continue;
        };
        groups
            .entry((part.base.clone(), part.total))
            .or_default()
            .insert(part.index);
        totals.entry(part.base).or_default().insert(part.total);
    }
    for (base, totals) in &totals {
        if totals.len() > 1 {
            let list: Vec<u32> = totals.iter().copied().collect();
            problems.push(problem(
                "chunk_group_gap",
                format!("chunk group {base:?} has inconsistent totals {list:?}"),
                &row.file_name,
            ));
        }
    }
    for ((base, total), indices) in &groups {
        if *total > 100_000 {
            problems.push(problem(
                "chunk_group_gap",
                format!("chunk group {base:?} total {total} is above 100000"),
                &row.file_name,
            ));
            continue;
        }
        let expected: BTreeSet<u32> = if indices.contains(&0) {
            (0..*total).collect()
        } else {
            (1..=*total).collect()
        };
        let missing: Vec<u32> = expected.difference(indices).copied().collect();
        let unexpected: Vec<u32> = indices.difference(&expected).copied().collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            problems.push(problem(
                "chunk_group_gap",
                format!("chunk group {base:?} of {total}: missing indices {missing:?}, unexpected indices {unexpected:?}"),
                &row.file_name,
            ));
        }
    }
}

struct Part {
    base: String,
    index: u32,
    total: u32,
}

fn parse_part(name: &str) -> Option<Part> {
    let (base, rest) = name.rsplit_once('-')?;
    if base.is_empty() {
        return None;
    }
    let (idx_s, total_s) = rest.split_once("of")?;
    if !is_digits(idx_s) || !is_digits(total_s) {
        return None;
    }
    Some(Part {
        base: base.to_string(),
        index: idx_s.parse().ok()?,
        total: total_s.parse().ok()?,
    })
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn tex_tier(name: &str) -> Option<&str> {
    let (head, rest) = name.rsplit_once('-')?;
    let (idx_s, total_s) = rest.split_once("of")?;
    if !is_digits(idx_s) || !is_digits(total_s) {
        return None;
    }
    let (base_and_tex, tier) = head.rsplit_once('-')?;
    base_and_tex
        .strip_suffix("-tex")
        .filter(|base| !base.is_empty())?;
    if tier.is_empty() || !tier.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    Some(tier)
}

fn check_cross_manifest(rows: &[ManifestRow], problems: &mut Vec<VerifyProblem>) {
    for key in ["tag", "version", "git_sha"] {
        let mut values: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for row in rows {
            if let Some(value) = row.headers.get(key) {
                values
                    .entry(value.as_str())
                    .or_default()
                    .push(row.file_name.as_str());
            }
        }
        if values.len() > 1 {
            let detail = values
                .iter()
                .map(|(value, names)| format!("{value:?} in {names:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            problems.push(problem(
                "metadata_mismatch",
                format!("header {key:?} differs across manifests: {detail}"),
                "",
            ));
        }
    }
    let mut by_platform: BTreeMap<&str, Vec<&ManifestRow>> = BTreeMap::new();
    for row in rows {
        by_platform.entry(row.platform.as_str()).or_default().push(row);
    }
    for (platform, group) in &by_platform {
        let mut shared: BTreeMap<&str, BTreeMap<&str, (String, String, i64)>> = BTreeMap::new();
        let mut universe: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for row in group {
            for chunk in &row.chunks {
                if let Some(tier) = tex_tier(&chunk.name) {
                    universe.entry(tier).or_default().insert(chunk.name.as_str());
                    continue;
                }
                shared.entry(chunk.name.as_str()).or_default().insert(
                    row.file_name.as_str(),
                    (chunk.asset.clone(), chunk.sha256.clone(), chunk.size),
                );
            }
        }
        for (name, variants) in &shared {
            let distinct: BTreeSet<&(String, String, i64)> = variants.values().collect();
            if distinct.len() > 1 {
                let detail = variants
                    .iter()
                    .map(|(file, (asset, sha, size))| {
                        format!("{file} lists asset={asset} sha256={sha} size={size}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(problem(
                    "shared_chunk_mismatch",
                    format!("platform {platform} shared chunk {name:?} differs across tiers: {detail}"),
                    "",
                ));
            }
        }
        for row in group {
            let Some(tier) = &row.tier else {
                continue;
            };
            let mut own = BTreeSet::new();
            for chunk in &row.chunks {
                let Some(chunk_tier) = tex_tier(&chunk.name) else {
                    continue;
                };
                if chunk_tier != tier {
                    problems.push(problem(
                        "tex_tier_mismatch",
                        format!(
                            "chunk {:?} belongs to tier {chunk_tier:?}, not {tier:?}",
                            chunk.name
                        ),
                        &row.file_name,
                    ));
                } else {
                    own.insert(chunk.name.as_str());
                }
            }
            let expected = universe.get(tier.as_str()).cloned().unwrap_or_default();
            if own != expected {
                let missing: Vec<&str> = expected.difference(&own).copied().collect();
                let unexpected: Vec<&str> = own.difference(&expected).copied().collect();
                let listed: Vec<&str> = expected.iter().copied().collect();
                problems.push(problem(
                    "tex_tier_mismatch",
                    format!(
                        "tier {tier:?} must list exactly tex chunks {listed:?} (missing {missing:?}, unexpected {unexpected:?})"
                    ),
                    &row.file_name,
                ));
            }
        }
    }
}

fn check_directory(
    dir: &Path,
    checked: &[ManifestRow],
    skipped: &[ManifestRow],
    problems: &mut Vec<VerifyProblem>,
) {
    let mut listed = BTreeSet::new();
    for row in checked.iter().chain(skipped) {
        for chunk in &row.chunks {
            listed.insert(chunk.asset.as_str());
        }
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            problems.push(problem(
                "release_dir",
                format!("cannot list release directory: {err}"),
                "",
            ));
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".tar.zst") {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() > DEFAULT_MAX_CHUNK_BYTES {
            problems.push(problem(
                "archive_too_large",
                format!(
                    "archive {name} is {} bytes, over the {DEFAULT_MAX_CHUNK_BYTES} byte limit",
                    meta.len()
                ),
                "",
            ));
        }
        if !listed.contains(name) {
            problems.push(problem(
                "orphan_archive",
                format!("archive {name} is not listed by any manifest"),
                "",
            ));
        }
    }
}
