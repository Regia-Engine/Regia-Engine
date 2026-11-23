// SPDX-License-Identifier: MPL-2.0
//! Ordered upload plan for a release directory. No network.
//! [`crate::upload_with_backend`] sends this plan to a filesystem backend or a test fake.
//!
//! Groups are chunks (`*.tar.zst`), then any other payload, then manifests. Manifests are
//! `manifest-*` JSON and TSV, `release-chunks.json`, `release-chunks.tsv`, and `*.sha256`
//! sidecars. Empty groups are omitted. A later lane drives the hosting CLI from this plan.
//!
//! Retry counts, the publish gate, and the stable-size check are pure. They do not sleep
//! and they do not read a clock. A size change restarts the equal-sample counter.
//!
//! Behavior follows the release-upload requirements at pin `c9513623f` (`regia-absorb-4`).
//! Python was not copied. This module builds the ordered plan only.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ChunkError;

/// JSON `schema` of [`UploadPlan`].
pub const UPLOAD_PLAN_SCHEMA: u32 = 1;

/// How [`upload_plan_with`] labels the plan. Defaults match a retry-safe ordered upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPlanOptions {
    /// Recorded on the plan. This function never contacts a host.
    pub dry_run: bool,
    /// Attempts recorded for a later uploader. Default 3.
    pub retries: u32,
    /// Skip a remote asset whose size already matches. Default true.
    pub skip_same_size: bool,
}

impl Default for UploadPlanOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            retries: 3,
            skip_same_size: true,
        }
    }
}

/// One ordered group. Files are sorted base names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadGroup {
    /// `chunks`, `payload`, or `manifests`.
    pub name: String,
    /// File names in this directory, sorted.
    pub files: Vec<String>,
}

/// Ordered plan. `retry_safe` is always true: groups can be repeated without reordering.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct UploadPlan {
    /// [`UPLOAD_PLAN_SCHEMA`].
    pub schema: u32,
    /// Directory the names are relative to.
    pub dir: String,
    /// Always true.
    pub retry_safe: bool,
    /// From [`UploadPlanOptions::retries`].
    pub retries: u32,
    /// From [`UploadPlanOptions::skip_same_size`].
    pub skip_same_size: bool,
    /// From [`UploadPlanOptions::dry_run`].
    pub dry_run: bool,
    /// Non-empty groups, chunks then payload then manifests.
    pub groups: Vec<UploadGroup>,
}

impl UploadPlan {
    /// JSON object a caller can print. Field order is not significant.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "schema": self.schema,
            "dir": self.dir,
            "retry_safe": self.retry_safe,
            "retries": self.retries,
            "skip_same_size": self.skip_same_size,
            "dry_run": self.dry_run,
            "groups": self.groups.iter().map(|group| serde_json::json!({
                "name": group.name,
                "files": group.files,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Plan `dir` with [`UploadPlanOptions::default`].
///
/// # Errors
///
/// `release_dir` when `dir` is missing or not a directory, or when the directory cannot be read.
pub fn upload_plan(dir: &Path) -> Result<UploadPlan, ChunkError> {
    upload_plan_with(dir, &UploadPlanOptions::default())
}

/// Plan `dir`. Top-level files only.
///
/// # Errors
///
/// `release_dir` when `dir` is missing or not a directory, or when the directory cannot be read.
pub fn upload_plan_with(dir: &Path, opts: &UploadPlanOptions) -> Result<UploadPlan, ChunkError> {
    if !dir.is_dir() {
        return Err(ChunkError::new(
            "release_dir",
            format!("release directory {} does not exist", dir.display()),
        ));
    }
    let mut chunks = Vec::new();
    let mut payload = Vec::new();
    let mut manifests = Vec::new();
    let entries = fs::read_dir(dir).map_err(|err| ChunkError::io(dir, &err))?;
    for entry in entries {
        let entry = entry.map_err(|err| ChunkError::io(dir, &err))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !entry.path().is_file() {
            continue;
        }
        if name.ends_with(".tar.zst") {
            chunks.push(name.to_string());
        } else if is_manifest_name(name) {
            manifests.push(name.to_string());
        } else {
            payload.push(name.to_string());
        }
    }
    chunks.sort();
    payload.sort();
    manifests.sort();
    let mut groups = Vec::new();
    push_group(&mut groups, "chunks", chunks);
    push_group(&mut groups, "payload", payload);
    push_group(&mut groups, "manifests", manifests);
    Ok(UploadPlan {
        schema: UPLOAD_PLAN_SCHEMA,
        dir: dir.display().to_string(),
        retry_safe: true,
        retries: opts.retries,
        skip_same_size: opts.skip_same_size,
        dry_run: opts.dry_run,
        groups,
    })
}

fn push_group(groups: &mut Vec<UploadGroup>, name: &str, files: Vec<String>) {
    if files.is_empty() {
        return;
    }
    groups.push(UploadGroup {
        name: name.to_string(),
        files,
    });
}

/// Manifest and sidecar names that upload last.
#[must_use]
pub fn is_manifest_name(name: &str) -> bool {
    if name == "release-chunks.json" || name == "release-chunks.tsv" || name == "release-chunks.json.sha256" {
        return true;
    }
    if name.ends_with(".sha256") {
        return true;
    }
    name.strip_prefix("manifest-")
        .is_some_and(|rest| rest.ends_with(".json") || rest.ends_with(".tsv"))
}

/// True when some line, after trimming, is exactly `OK`. `OKAY` and `BOOKED` are false.
/// A missing file is false.
#[must_use]
pub fn publish_gate_ok(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| line.trim() == "OK")
}

/// Asset names from `chunks`, `assets`, or `files`. Objects contribute `asset`, `name`,
/// `file`, or `path`. Duplicates are dropped. Order is kept.
///
/// # Errors
///
/// `plan_invalid` when the file cannot be read, the JSON is malformed, or no asset is listed.
pub fn load_manifest_assets(path: &Path) -> Result<Vec<String>, ChunkError> {
    let text = fs::read_to_string(path).map_err(|err| {
        ChunkError::new(
            "plan_invalid",
            format!("cannot read manifest {}: {err}", path.display()),
        )
    })?;
    let data: Value = serde_json::from_str(&text).map_err(|err| {
        ChunkError::new(
            "plan_invalid",
            format!("invalid JSON in manifest {}: {err}", path.display()),
        )
    })?;
    let mut names = Vec::new();
    match &data {
        Value::Object(obj) => {
            if let Some(chunks) = obj
                .get("chunks")
                .or_else(|| obj.get("assets"))
                .or_else(|| obj.get("files"))
            {
                add_assets(chunks, &mut names);
            }
        }
        Value::Array(_) => add_assets(&data, &mut names),
        _ => {}
    }
    if names.is_empty() {
        return Err(ChunkError::new(
            "plan_invalid",
            format!("manifest {} lists no chunk assets", path.display()),
        ));
    }
    Ok(names)
}

fn add_assets(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                add_one(item, names);
            }
        }
        other => add_one(other, names),
    }
}

fn add_one(value: &Value, names: &mut Vec<String>) {
    let asset = match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(obj) => ["asset", "name", "file", "path"].iter().find_map(|key| {
            obj.get(*key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        }),
        _ => None,
    };
    if let Some(asset) = asset
        && !names.iter().any(|have| have == &asset)
    {
        names.push(asset);
    }
}

/// `owner/name`: one slash, both sides non-empty, no whitespace. `a/b/c` is false.
#[must_use]
pub fn repo_ok(repo: &str) -> bool {
    let Some((left, right)) = repo.split_once('/') else {
        return false;
    };
    !left.is_empty() && !right.is_empty() && !right.contains('/') && !repo.chars().any(char::is_whitespace)
}

/// Non-empty and no whitespace.
#[must_use]
pub fn tag_ok(tag: &str) -> bool {
    !tag.is_empty() && !tag.chars().any(char::is_whitespace)
}

/// How many tries [`attempts_until_success`] counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptCount {
    /// Tries used. The budget is `max(1, retries)`.
    pub attempts: u32,
    /// True when a success fit in the budget.
    pub succeeded: bool,
}

/// `failures_before_ok` failures, then one success, inside a budget of `max(1, retries)`.
/// Two failures and `retries` 3 succeed on attempt 3. Ten failures and `retries` 2 stop after 2.
#[must_use]
pub fn attempts_until_success(failures_before_ok: u32, retries: u32) -> AttemptCount {
    let budget = retries.max(1);
    if failures_before_ok < budget {
        AttemptCount {
            attempts: failures_before_ok.saturating_add(1),
            succeeded: true,
        }
    } else {
        AttemptCount {
            attempts: budget,
            succeeded: false,
        }
    }
}

/// First size that stays equal for `stable_polls` samples. A change restarts the counter.
/// `stable_size(&[3, 3, 3], 3)` is `Some(3)`. `&[3, 3, 6, 6]` with 3 polls is `None`.
/// `&[3, 3, 6, 6, 6]` with 3 polls is `Some(6)`.
#[must_use]
pub fn stable_size(samples: &[u64], stable_polls: u32) -> Option<u64> {
    if stable_polls == 0 {
        return samples.last().copied();
    }
    let mut samples = samples.iter().copied();
    let mut last = samples.next()?;
    let mut run = 1u32;
    if run >= stable_polls {
        return Some(last);
    }
    for size in samples {
        if size == last {
            run += 1;
        } else {
            last = size;
            run = 1;
        }
        if run >= stable_polls {
            return Some(last);
        }
    }
    None
}

/// True when the remote size is known and equals `local`.
#[must_use]
pub fn skip_same_size(local: u64, remote: Option<u64>) -> bool {
    remote == Some(local)
}

/// Text that contains `size mismatch`, for a remote size that does not match.
#[must_use]
pub fn size_mismatch_message(name: &str, local: u64, remote: u64) -> String {
    format!("size mismatch for {name} after upload: local {local}, remote {remote}")
}

/// One planned group and the asset names that must already be present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateGroup {
    /// Files this group would upload, in order.
    pub files: Vec<String>,
    /// Asset names required before this group. Empty means no gate.
    pub after: Vec<String>,
}

/// What an ordered upload got through before a manifest gate stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    /// Files uploaded before the gate failed, in group order.
    pub uploaded: Vec<String>,
    /// True when a later group was not uploaded.
    pub blocked: bool,
}

/// Walk groups in order. A group whose `after` assets are not all present (already, or
/// uploaded by an earlier group) stops the walk. Nothing after that group is uploaded.
#[must_use]
pub fn gated_uploads(groups: &[GateGroup], already_present: &[String]) -> GateOutcome {
    let mut present: Vec<&str> = already_present.iter().map(String::as_str).collect();
    let mut uploaded = Vec::new();
    for group in groups {
        if group
            .after
            .iter()
            .any(|asset| !present.iter().any(|have| *have == asset))
        {
            return GateOutcome {
                uploaded,
                blocked: true,
            };
        }
        for file in &group.files {
            uploaded.push(file.clone());
            present.push(file.as_str());
        }
    }
    GateOutcome {
        uploaded,
        blocked: false,
    }
}

/// One group from a TOML plan. Patterns are not expanded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanGroup {
    /// Group name.
    pub name: String,
    /// Glob patterns, relative to the asset directory.
    pub patterns: Vec<String>,
    /// `after` paths. Not resolved here.
    pub after: Vec<String>,
}

/// The groups of a TOML plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPlan {
    /// Groups in file order.
    pub groups: Vec<PlanGroup>,
    /// `retries`, `max_retries`, or `attempts`. Default 3.
    pub retries: u32,
}

/// Parse a TOML plan from text. This does not expand globs and does not upload.
///
/// # Errors
///
/// `plan_invalid` when the TOML is malformed, a group has no patterns, or there are no groups.
pub fn load_plan_str(text: &str) -> Result<LoadedPlan, ChunkError> {
    let data: toml::Value = toml::from_str(text)
        .map_err(|err| ChunkError::new("plan_invalid", format!("invalid TOML in plan: {err}")))?;
    let Some(table) = data.as_table() else {
        return Err(ChunkError::new("plan_invalid", "plan must be a TOML table"));
    };
    let retries = toml_int(table, &["retries", "max_retries", "attempts"])
        .or_else(|| {
            table
                .get("settings")
                .and_then(toml::Value::as_table)
                .and_then(|settings| toml_int(settings, &["retries", "max_retries", "attempts"]))
        })
        .unwrap_or(3);
    if retries < 0 {
        return Err(ChunkError::new("plan_invalid", "retries must be >= 0"));
    }
    let raw_groups = if let Some(groups) = table.get("groups").and_then(toml::Value::as_array) {
        groups.clone()
    } else if let Some(groups) = table.get("group").and_then(toml::Value::as_array) {
        groups.clone()
    } else {
        return Err(ChunkError::new("plan_invalid", "plan has no [[groups]] entries"));
    };
    if raw_groups.is_empty() {
        return Err(ChunkError::new("plan_invalid", "plan has no groups"));
    }
    let mut groups = Vec::new();
    for (index, raw) in raw_groups.iter().enumerate() {
        groups.push(parse_plan_group(raw, index)?);
    }
    let retries =
        u32::try_from(retries).map_err(|_| ChunkError::new("plan_invalid", "retries do not fit"))?;
    Ok(LoadedPlan { groups, retries })
}

fn toml_int(table: &toml::map::Map<String, toml::Value>, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(toml::Value::Integer(value)) = table.get(*key) {
            return Some(*value);
        }
    }
    None
}

fn parse_plan_group(raw: &toml::Value, index: usize) -> Result<PlanGroup, ChunkError> {
    if let Some(pattern) = raw.as_str() {
        return Ok(PlanGroup {
            name: format!("group-{}", index + 1),
            patterns: vec![pattern.to_string()],
            after: Vec::new(),
        });
    }
    let Some(table) = raw.as_table() else {
        return Err(ChunkError::new(
            "plan_invalid",
            format!("group {} must be a table", index + 1),
        ));
    };
    let name = table
        .get("name")
        .or_else(|| table.get("id"))
        .and_then(toml::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("group-{}", index + 1));
    let patterns_raw = ["patterns", "globs", "glob", "pattern", "files", "file"]
        .iter()
        .find_map(|key| table.get(*key));
    let Some(patterns_raw) = patterns_raw else {
        return Err(ChunkError::new(
            "plan_invalid",
            format!("group {name:?} has no 'patterns'"),
        ));
    };
    let patterns = match patterns_raw {
        toml::Value::String(pattern) => vec![pattern.clone()],
        toml::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    ChunkError::new(
                        "plan_invalid",
                        format!("group {name:?}: 'patterns' must be a list of strings"),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(ChunkError::new(
                "plan_invalid",
                format!("group {name:?}: 'patterns' must be a list of strings"),
            ));
        }
    };
    if patterns.is_empty() || patterns.iter().any(String::is_empty) {
        return Err(ChunkError::new(
            "plan_invalid",
            format!("group {name:?} has empty patterns"),
        ));
    }
    let after = match table
        .get("after")
        .or_else(|| table.get("manifest"))
        .or_else(|| table.get("depends_on"))
    {
        None => Vec::new(),
        Some(toml::Value::String(one)) => vec![one.clone()],
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().unwrap_or("").to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(_) => {
            return Err(ChunkError::new(
                "plan_invalid",
                format!("group {name:?}: 'after' must be a string or list"),
            ));
        }
    };
    Ok(PlanGroup {
        name,
        patterns,
        after,
    })
}

/// Expand glob patterns under `base`. `*` does not cross `/`. `**` does. `?` is one
/// non-slash character. Matches are files only, sorted within each pattern, and de-duplicated.
///
/// # Errors
///
/// `pattern_unmatched` when a pattern matches nothing. `plan_invalid` for an absolute pattern.
pub fn expand_patterns(
    base: &Path,
    group_name: &str,
    patterns: &[String],
) -> Result<Vec<PathBuf>, ChunkError> {
    let mut files = Vec::new();
    let mut seen = Vec::new();
    for pattern in patterns {
        if Path::new(pattern).is_absolute() {
            return Err(ChunkError::new(
                "plan_invalid",
                format!("group {group_name:?}: pattern {pattern:?} is absolute"),
            ));
        }
        let mut matched = Vec::new();
        for entry in walkdir::WalkDir::new(base).follow_links(false) {
            let entry = entry.map_err(|err| {
                ChunkError::new("plan_invalid", format!("cannot walk {}: {err}", base.display()))
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(base).unwrap_or(entry.path());
            let rel = rel.to_string_lossy().replace('\\', "/");
            if glob_match(pattern, &rel) {
                matched.push(entry.path().to_path_buf());
            }
        }
        matched.sort();
        if matched.is_empty() {
            return Err(ChunkError::new(
                "pattern_unmatched",
                format!(
                    "group {group_name:?}: pattern {pattern:?} matched no files in {}",
                    base.display()
                ),
            ));
        }
        for path in matched {
            if !seen.iter().any(|have: &PathBuf| have == &path) {
                seen.push(path.clone());
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern.starts_with(b"**") {
            let rest = if pattern.len() > 2 && pattern[2] == b'/' {
                &pattern[3..]
            } else {
                &pattern[2..]
            };
            if rec(rest, text) {
                return true;
            }
            for (index, _) in text.iter().enumerate() {
                if rec(pattern, &text[index + 1..]) {
                    return true;
                }
            }
            return false;
        }
        if pattern[0] == b'*' {
            let rest = &pattern[1..];
            if rec(rest, text) {
                return true;
            }
            for (index, byte) in text.iter().enumerate() {
                if *byte == b'/' {
                    break;
                }
                if rec(rest, &text[index + 1..]) {
                    return true;
                }
            }
            return false;
        }
        if text.is_empty() {
            return false;
        }
        if pattern[0] == b'?' {
            if text[0] == b'/' {
                return false;
            }
            return rec(&pattern[1..], &text[1..]);
        }
        if pattern[0] == text[0] {
            return rec(&pattern[1..], &text[1..]);
        }
        false
    }
    rec(pattern.as_bytes(), path.as_bytes())
}
