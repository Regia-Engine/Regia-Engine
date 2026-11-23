// SPDX-License-Identifier: MPL-2.0
//! Diff two release manifests: chunks, files, links, and bytes to download.
//!
//! A path that changes chunk is reported once, as a move, and is not also added or removed.
//! Download bytes count each content identity once. An empty SHA-256 uses `asset:<asset>`
//! (that string is used even when the asset name is empty). Malformed input is
//! `release_input`. The byte formatting is display only; it is not lockstep math.
//!
//! Behavior follows the release-diff requirements at pin `c9513623f` (`regia-absorb-4`).
//! Python was not copied. A consumer that still says `glibc_required` maps onto `libc_required`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ChunkError;

/// Optional platform and tier filter. `None` keeps every manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// Keep manifests whose platform equals this.
    pub platform: Option<String>,
    /// Keep manifests whose tier equals this. Empty string is a real tier.
    pub tier: Option<String>,
}

/// One file path inside a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path as listed.
    pub path: String,
    /// Chunk `name` that lists it.
    pub chunk: String,
    /// Byte size when the entry recorded one.
    pub size: Option<i64>,
}

/// One chunk, reduced to the fields a diff prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkView {
    /// Chunk name.
    pub name: String,
    /// Group label.
    pub group: String,
    /// Archive file name.
    pub asset: String,
    /// Lowercase SHA-256, or empty.
    pub sha256: String,
    /// Compressed size.
    pub size: i64,
    /// Unpacked size.
    pub unpacked: i64,
    /// Member paths, in manifest order.
    pub files: Vec<FileEntry>,
}

impl ChunkView {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "group": self.group,
            "asset": self.asset,
            "sha256": self.sha256,
            "size": self.size,
            "unpacked": self.unpacked,
            "files": self.files.iter().map(|file| &file.path).collect::<Vec<_>>(),
        })
    }
}

/// A chunk whose asset or SHA-256 changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedChunk {
    /// Chunk name.
    pub name: String,
    /// Previous row.
    pub old: ChunkView,
    /// New row.
    pub new: ChunkView,
    /// `new.size - old.size`.
    pub size_delta: i64,
    /// `new.unpacked - old.unpacked`.
    pub unpacked_delta: i64,
}

/// Chunk classification for one platform and tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDiff {
    /// Same name, same asset, same SHA-256.
    pub unchanged: Vec<ChunkView>,
    /// Same name, different asset or SHA-256.
    pub changed: Vec<ChangedChunk>,
    /// Names only on the new side.
    pub added: Vec<ChunkView>,
    /// Names only on the old side.
    pub removed: Vec<ChunkView>,
    /// Bytes to fetch for identities the old release does not already have.
    pub update_download_bytes: i64,
    /// Bytes to fetch for a machine with nothing installed.
    pub fresh_download_bytes: i64,
    /// Sum of old unpacked sizes.
    pub unpacked_before_bytes: i64,
    /// Sum of new unpacked sizes.
    pub unpacked_after_bytes: i64,
}

impl ChunkDiff {
    /// `unpacked_after_bytes - unpacked_before_bytes`.
    #[must_use]
    pub fn unpacked_delta_bytes(&self) -> i64 {
        self.unpacked_after_bytes - self.unpacked_before_bytes
    }
}

/// Counts for one directory group. Byte fields are absent from the meaning of the
/// report when any added or removed file in the group has no size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDiff {
    /// Files added in this group.
    pub added: u64,
    /// Files removed in this group.
    pub removed: u64,
    /// Files that moved between chunks.
    pub moved: u64,
    /// Added bytes, meaningful when [`Self::sizes_known`].
    pub added_bytes: i64,
    /// Removed bytes, meaningful when [`Self::sizes_known`].
    pub removed_bytes: i64,
    /// False when any added or removed file lacked a size.
    pub sizes_known: bool,
}

impl GroupDiff {
    fn new() -> Self {
        Self {
            added: 0,
            removed: 0,
            moved: 0,
            added_bytes: 0,
            removed_bytes: 0,
            sizes_known: true,
        }
    }

    fn to_value(&self) -> Value {
        let (added_bytes, removed_bytes, byte_delta) = if self.sizes_known {
            (
                Value::from(self.added_bytes),
                Value::from(self.removed_bytes),
                Value::from(self.added_bytes - self.removed_bytes),
            )
        } else {
            (Value::Null, Value::Null, Value::Null)
        };
        serde_json::json!({
            "added": self.added,
            "removed": self.removed,
            "moved": self.moved,
            "added_bytes": added_bytes,
            "removed_bytes": removed_bytes,
            "byte_delta": byte_delta,
        })
    }
}

/// A file whose chunk changed and which is therefore not added or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedFile {
    /// Path.
    pub path: String,
    /// Chunk name on the old side.
    pub from_chunk: String,
    /// Chunk name on the new side.
    pub to_chunk: String,
}

/// File lists for one pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Paths only on the new side, sorted.
    pub added: Vec<String>,
    /// Paths only on the old side, sorted.
    pub removed: Vec<String>,
    /// Paths whose chunk changed, each once, sorted by path.
    pub moved: Vec<MovedFile>,
    /// First-two-segment groups.
    pub by_directory: BTreeMap<String, GroupDiff>,
}

/// One `(platform, tier)` difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairDiff {
    /// Platform label.
    pub platform: String,
    /// Tier label. Empty when the manifest is not tiered.
    pub tier: String,
    /// Old tag, or empty.
    pub old_tag: String,
    /// New tag, or empty.
    pub new_tag: String,
    /// Old version string (`version`, or `product_version` when `version` is absent).
    pub old_version: String,
    /// New version string.
    pub new_version: String,
    /// Chunk diff.
    pub chunks: ChunkDiff,
    /// File diff.
    pub files: FileDiff,
    /// Links only on the new side, sorted.
    pub links_added: Vec<(String, String)>,
    /// Links only on the old side, sorted.
    pub links_removed: Vec<(String, String)>,
}

impl PairDiff {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "platform": self.platform,
            "tier": self.tier,
            "old_tag": self.old_tag,
            "new_tag": self.new_tag,
            "old_version": self.old_version,
            "new_version": self.new_version,
            "chunks": {
                "unchanged": self.chunks.unchanged.iter().map(ChunkView::to_value).collect::<Vec<_>>(),
                "changed": self.chunks.changed.iter().map(|item| serde_json::json!({
                    "name": item.name,
                    "old": item.old.to_value(),
                    "new": item.new.to_value(),
                    "size_delta": item.size_delta,
                    "unpacked_delta": item.unpacked_delta,
                })).collect::<Vec<_>>(),
                "added": self.chunks.added.iter().map(ChunkView::to_value).collect::<Vec<_>>(),
                "removed": self.chunks.removed.iter().map(ChunkView::to_value).collect::<Vec<_>>(),
                "counts": {
                    "unchanged": self.chunks.unchanged.len(),
                    "changed": self.chunks.changed.len(),
                    "added": self.chunks.added.len(),
                    "removed": self.chunks.removed.len(),
                },
            },
            "update_download_bytes": self.chunks.update_download_bytes,
            "fresh_download_bytes": self.chunks.fresh_download_bytes,
            "unpacked_before_bytes": self.chunks.unpacked_before_bytes,
            "unpacked_after_bytes": self.chunks.unpacked_after_bytes,
            "unpacked_delta_bytes": self.chunks.unpacked_delta_bytes(),
            "downloads": {
                "update_bytes": self.chunks.update_download_bytes,
                "fresh_bytes": self.chunks.fresh_download_bytes,
            },
            "files": {
                "added": self.files.added,
                "removed": self.files.removed,
                "moved": self.files.moved.iter().map(|item| serde_json::json!({
                    "path": item.path,
                    "from_chunk": item.from_chunk,
                    "to_chunk": item.to_chunk,
                })).collect::<Vec<_>>(),
                "counts": {
                    "added": self.files.added.len(),
                    "removed": self.files.removed.len(),
                    "moved": self.files.moved.len(),
                },
                "by_directory": self.files.by_directory.iter().map(|(name, group)| (name.clone(), group.to_value())).collect::<serde_json::Map<String, Value>>(),
            },
            "links": {
                "added": self.links_added,
                "removed": self.links_removed,
            },
        })
    }
}

/// Full diff. `pairs` and the JSON `tiers` array are the same list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct DiffReport {
    /// One entry per platform and tier, sorted. A single mismatched pair stays one entry.
    pub pairs: Vec<PairDiff>,
}

impl DiffReport {
    /// JSON object with `pairs` and a duplicate `tiers` array.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let pairs: Vec<Value> = self.pairs.iter().map(PairDiff::to_value).collect();
        serde_json::json!({
            "pairs": pairs.clone(),
            "tiers": pairs,
        })
    }
}

struct Manifest {
    platform: String,
    tier: String,
    tag: String,
    version: String,
    chunks: Vec<ChunkView>,
    links: Vec<(String, String)>,
}

/// Diff `old` against `new`. Each path may be a manifest file or a directory of them.
///
/// # Errors
///
/// `release_input` when a path is missing, a directory has no manifest, JSON is malformed,
/// or `chunks` is not a list. A release that differs still returns [`Ok`].
pub fn diff_release(old: &Path, new: &Path, opts: &DiffOptions) -> Result<DiffReport, ChunkError> {
    let old_set = load_manifests(old, opts)?;
    let new_set = load_manifests(new, opts)?;
    Ok(DiffReport {
        pairs: diff_sets(&old_set, &new_set),
    })
}

fn diff_sets(
    old: &BTreeMap<(String, String), Manifest>,
    new: &BTreeMap<(String, String), Manifest>,
) -> Vec<PairDiff> {
    if old.len() == 1
        && new.len() == 1
        && let Some((old_key, old_manifest)) = old.iter().next()
        && let Some((new_key, new_manifest)) = new.iter().next()
        && old_key != new_key
        && let Some(pair) = diff_pair(Some(old_manifest), Some(new_manifest))
    {
        return vec![pair];
    }
    let mut keys: BTreeSet<&(String, String)> = BTreeSet::new();
    keys.extend(old.keys());
    keys.extend(new.keys());
    keys.into_iter()
        .filter_map(|key| diff_pair(old.get(key), new.get(key)))
        .collect()
}

fn diff_pair(old: Option<&Manifest>, new: Option<&Manifest>) -> Option<PairDiff> {
    let anchor = new.or(old)?;
    let old_links: BTreeSet<&(String, String)> = old.map(|m| m.links.iter().collect()).unwrap_or_default();
    let new_links: BTreeSet<&(String, String)> = new.map(|m| m.links.iter().collect()).unwrap_or_default();
    Some(PairDiff {
        platform: anchor.platform.clone(),
        tier: anchor.tier.clone(),
        old_tag: old.map(|m| m.tag.clone()).unwrap_or_default(),
        new_tag: new.map(|m| m.tag.clone()).unwrap_or_default(),
        old_version: old.map(|m| m.version.clone()).unwrap_or_default(),
        new_version: new.map(|m| m.version.clone()).unwrap_or_default(),
        chunks: diff_chunks(old, new),
        files: diff_files(old, new),
        links_added: new_links
            .difference(&old_links)
            .map(|link| (*link).clone())
            .collect(),
        links_removed: old_links
            .difference(&new_links)
            .map(|link| (*link).clone())
            .collect(),
    })
}

fn diff_chunks(old: Option<&Manifest>, new: Option<&Manifest>) -> ChunkDiff {
    let old_chunks = old.map(|m| m.chunks.as_slice()).unwrap_or(&[]);
    let new_chunks = new.map(|m| m.chunks.as_slice()).unwrap_or(&[]);
    let old_by: BTreeMap<&str, &ChunkView> = old_chunks
        .iter()
        .map(|chunk| (chunk.name.as_str(), chunk))
        .collect();
    let new_by: BTreeMap<&str, &ChunkView> = new_chunks
        .iter()
        .map(|chunk| (chunk.name.as_str(), chunk))
        .collect();
    let mut result = ChunkDiff {
        unchanged: Vec::new(),
        changed: Vec::new(),
        added: Vec::new(),
        removed: Vec::new(),
        update_download_bytes: 0,
        fresh_download_bytes: 0,
        unpacked_before_bytes: old_chunks.iter().map(|chunk| chunk.unpacked).sum(),
        unpacked_after_bytes: new_chunks.iter().map(|chunk| chunk.unpacked).sum(),
    };
    for (name, chunk) in &new_by {
        match old_by.get(name) {
            None => result.added.push((*chunk).clone()),
            Some(previous) if previous.asset == chunk.asset && previous.sha256 == chunk.sha256 => {
                result.unchanged.push((*chunk).clone());
            }
            Some(previous) => result.changed.push(ChangedChunk {
                name: (*name).to_string(),
                size_delta: chunk.size - previous.size,
                unpacked_delta: chunk.unpacked - previous.unpacked,
                old: (*previous).clone(),
                new: (*chunk).clone(),
            }),
        }
    }
    for (name, chunk) in &old_by {
        if !new_by.contains_key(name) {
            result.removed.push((*chunk).clone());
        }
    }
    result.update_download_bytes = download_bytes(new_chunks, &identity_set(old_chunks));
    result.fresh_download_bytes = download_bytes(new_chunks, &BTreeSet::new());
    result
}

fn identity_of(chunk: &ChunkView) -> String {
    if chunk.sha256.is_empty() {
        format!("asset:{}", chunk.asset)
    } else {
        chunk.sha256.clone()
    }
}

fn identity_set(chunks: &[ChunkView]) -> BTreeSet<String> {
    chunks.iter().map(identity_of).collect()
}

fn download_bytes(chunks: &[ChunkView], have: &BTreeSet<String>) -> i64 {
    let mut total = 0i64;
    let mut counted = BTreeSet::new();
    for chunk in chunks {
        let identity = identity_of(chunk);
        if have.contains(&identity) || !counted.insert(identity) {
            continue;
        }
        total += chunk.size;
    }
    total
}

fn diff_files(old: Option<&Manifest>, new: Option<&Manifest>) -> FileDiff {
    let old_map = file_map(old);
    let new_map = file_map(new);
    let mut result = FileDiff {
        added: Vec::new(),
        removed: Vec::new(),
        moved: Vec::new(),
        by_directory: BTreeMap::new(),
    };
    for (path, entry) in &new_map {
        match old_map.get(path) {
            None => {
                result.added.push((*path).to_string());
                let group = result
                    .by_directory
                    .entry(directory_group(path))
                    .or_insert_with(GroupDiff::new);
                group.added += 1;
                match entry.size {
                    Some(size) => group.added_bytes += size,
                    None => group.sizes_known = false,
                }
            }
            Some(previous) if previous.chunk != entry.chunk => {
                result.moved.push(MovedFile {
                    path: (*path).to_string(),
                    from_chunk: previous.chunk.clone(),
                    to_chunk: entry.chunk.clone(),
                });
                result
                    .by_directory
                    .entry(directory_group(path))
                    .or_insert_with(GroupDiff::new)
                    .moved += 1;
            }
            Some(_) => {}
        }
    }
    for (path, entry) in &old_map {
        if new_map.contains_key(path) {
            continue;
        }
        result.removed.push((*path).to_string());
        let group = result
            .by_directory
            .entry(directory_group(path))
            .or_insert_with(GroupDiff::new);
        group.removed += 1;
        match entry.size {
            Some(size) => group.removed_bytes += size,
            None => group.sizes_known = false,
        }
    }
    result
}

fn file_map(manifest: Option<&Manifest>) -> BTreeMap<&str, &FileEntry> {
    let mut map = BTreeMap::new();
    let Some(manifest) = manifest else {
        return map;
    };
    for chunk in &manifest.chunks {
        for entry in &chunk.files {
            map.entry(entry.path.as_str()).or_insert(entry);
        }
    }
    map
}

/// First two non-empty segments after `\` is rewritten to `/`.
/// `data/maps/a.bin` is `data/maps`. `bin/app` and `README` stay whole. Empty is empty.
#[must_use]
pub fn directory_group(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|part| !part.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second, ..] => format!("{first}/{second}"),
    }
}

/// Short byte size. `0 B`, `512 B`, `2.00 KiB`. Negative values keep the sign.
/// Display only.
#[must_use]
pub fn format_size(num_bytes: i64) -> String {
    let sign = if num_bytes < 0 { "-" } else { "" };
    let mut value = num_bytes.unsigned_abs() as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            if unit == "B" {
                return format!("{sign}{} B", num_bytes.unsigned_abs());
            }
            return format!("{sign}{value:.2} {unit}");
        }
        value /= 1024.0;
    }
    format!("{sign}{value:.2} TiB")
}

/// [`format_size`] plus the exact count: `150 B (150 bytes)`.
#[must_use]
pub fn format_size_detailed(num_bytes: i64) -> String {
    format!("{} ({num_bytes} bytes)", format_size(num_bytes))
}

fn signed_size(num_bytes: i64) -> String {
    let prefix = if num_bytes > 0 { "+" } else { "" };
    format!("{prefix}{}", format_size_detailed(num_bytes))
}

/// Human text. Includes `Update download:` and the exact byte count.
#[must_use]
pub fn format_text(report: &DiffReport) -> String {
    let mut lines = Vec::new();
    for pair in &report.pairs {
        let chunks = &pair.chunks;
        let mut heading = format!("Platform: {}", pair.platform);
        if !pair.tier.is_empty() {
            heading.push_str(&format!("  Tier: {}", pair.tier));
        }
        lines.push(heading);
        if !pair.old_tag.is_empty() || !pair.new_tag.is_empty() {
            let old = if pair.old_tag.is_empty() {
                "?"
            } else {
                pair.old_tag.as_str()
            };
            let new = if pair.new_tag.is_empty() {
                "?"
            } else {
                pair.new_tag.as_str()
            };
            lines.push(format!("Release: {old} -> {new}"));
        }
        lines.push(format!("Chunks unchanged: {}", chunk_names(&chunks.unchanged)));
        let changed = chunks
            .changed
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "Chunks changed: {}",
            if changed.is_empty() {
                "none".to_string()
            } else {
                changed
            }
        ));
        lines.push(format!("Chunks added: {}", chunk_names(&chunks.added)));
        lines.push(format!("Chunks removed: {}", chunk_names(&chunks.removed)));
        lines.push(format!(
            "Update download: {}",
            format_size_detailed(chunks.update_download_bytes)
        ));
        lines.push(format!(
            "Fresh download: {}",
            format_size_detailed(chunks.fresh_download_bytes)
        ));
        lines.push(format!(
            "Unpacked before: {}",
            format_size_detailed(chunks.unpacked_before_bytes)
        ));
        lines.push(format!(
            "Unpacked after: {}",
            format_size_detailed(chunks.unpacked_after_bytes)
        ));
        lines.push(format!(
            "Unpacked delta: {}",
            signed_size(chunks.unpacked_delta_bytes())
        ));
        lines.push(format!(
            "Installed size: {}",
            format_size_detailed(chunks.unpacked_after_bytes)
        ));
        lines.push(format!(
            "Files: {} added, {} removed, {} moved",
            pair.files.added.len(),
            pair.files.removed.len(),
            pair.files.moved.len()
        ));
        for path in &pair.files.added {
            lines.push(format!("  added: {path}"));
        }
        for path in &pair.files.removed {
            lines.push(format!("  removed: {path}"));
        }
        for item in &pair.files.moved {
            lines.push(format!(
                "  moved: {} ({} -> {})",
                item.path, item.from_chunk, item.to_chunk
            ));
        }
        if !pair.files.by_directory.is_empty() {
            lines.push("Files by directory:".to_string());
            for (name, group) in &pair.files.by_directory {
                let mut entry = format!(
                    "  {name}: {} added, {} removed, {} moved",
                    group.added, group.removed, group.moved
                );
                if group.sizes_known {
                    entry.push_str(&format!(
                        ", byte delta {}",
                        signed_size(group.added_bytes - group.removed_bytes)
                    ));
                }
                lines.push(entry);
            }
        }
        lines.push(format!(
            "Links: {} added, {} removed",
            pair.links_added.len(),
            pair.links_removed.len()
        ));
        for (link, target) in &pair.links_added {
            lines.push(format!("  link added: {link} -> {target}"));
        }
        for (link, target) in &pair.links_removed {
            lines.push(format!("  link removed: {link} -> {target}"));
        }
        lines.push(String::new());
    }
    let mut text = lines.join("\n");
    if text.ends_with('\n') {
        while text.ends_with("\n\n") {
            text.pop();
        }
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn chunk_names(chunks: &[ChunkView]) -> String {
    if chunks.is_empty() {
        "none".to_string()
    } else {
        chunks
            .iter()
            .map(|chunk| chunk.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Markdown table. The header row is
/// `| Platform | Tier | Update download | Fresh download | Installed size | Delta |`.
/// An empty tier prints as `-`.
#[must_use]
pub fn format_markdown(report: &DiffReport) -> String {
    let mut lines = Vec::new();
    let old_tag = report
        .pairs
        .first()
        .map(|pair| pair.old_tag.as_str())
        .unwrap_or("");
    let new_tag = report
        .pairs
        .first()
        .map(|pair| pair.new_tag.as_str())
        .unwrap_or("");
    if !old_tag.is_empty() || !new_tag.is_empty() {
        lines.push(format!("## Release {old_tag} -> {new_tag}").trim().to_string());
        lines.push(String::new());
    }
    lines.push("| Platform | Tier | Update download | Fresh download | Installed size | Delta |".to_string());
    lines.push("| --- | --- | ---: | ---: | ---: | ---: |".to_string());
    for pair in &report.pairs {
        let tier = if pair.tier.is_empty() {
            "-"
        } else {
            pair.tier.as_str()
        };
        lines.push(format!(
            "| {} | {tier} | {} | {} | {} | {} |",
            pair.platform,
            format_size_detailed(pair.chunks.update_download_bytes),
            format_size_detailed(pair.chunks.fresh_download_bytes),
            format_size_detailed(pair.chunks.unpacked_after_bytes),
            signed_size(pair.chunks.unpacked_delta_bytes())
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn load_manifests(
    source: &Path,
    opts: &DiffOptions,
) -> Result<BTreeMap<(String, String), Manifest>, ChunkError> {
    let candidates = manifest_files(source)?;
    let mut manifests = BTreeMap::new();
    for candidate in candidates {
        let manifest = load_manifest_file(&candidate)?;
        if opts
            .platform
            .as_ref()
            .is_some_and(|platform| platform != &manifest.platform)
        {
            continue;
        }
        if opts.tier.as_ref().is_some_and(|tier| tier != &manifest.tier) {
            continue;
        }
        manifests.insert((manifest.platform.clone(), manifest.tier.clone()), manifest);
    }
    if manifests.is_empty() {
        return Err(ChunkError::new(
            "release_input",
            format!(
                "no manifests match the requested platform/tier in {}",
                source.display()
            ),
        ));
    }
    Ok(manifests)
}

fn manifest_files(path: &Path) -> Result<Vec<PathBuf>, ChunkError> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(ChunkError::new(
            "release_input",
            format!("input not found: {}", path.display()),
        ));
    }
    let mut preferred = Vec::new();
    let mut other = Vec::new();
    let entries = fs::read_dir(path).map_err(|err| {
        ChunkError::new(
            "release_input",
            format!("cannot read manifest {}: {err}", path.display()),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            ChunkError::new(
                "release_input",
                format!("cannot read manifest {}: {err}", path.display()),
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".sha256") {
            continue;
        }
        if !entry.path().is_file() {
            continue;
        }
        if name.starts_with("manifest-") {
            preferred.push(entry.path());
        } else {
            other.push(entry.path());
        }
    }
    preferred.sort();
    if !preferred.is_empty() {
        return Ok(preferred);
    }
    other.sort();
    if other.is_empty() {
        return Err(ChunkError::new(
            "release_input",
            format!("no manifests found in directory {}", path.display()),
        ));
    }
    Ok(other)
}

fn load_manifest_file(path: &Path) -> Result<Manifest, ChunkError> {
    let text = fs::read_to_string(path).map_err(|err| {
        ChunkError::new(
            "release_input",
            format!("cannot read manifest {}: {err}", path.display()),
        )
    })?;
    let data: Value = serde_json::from_str(&text).map_err(|err| {
        ChunkError::new(
            "release_input",
            format!("cannot parse manifest {}: {err}", path.display()),
        )
    })?;
    parse_manifest(&data, path)
}

fn parse_manifest(data: &Value, source: &Path) -> Result<Manifest, ChunkError> {
    let Some(obj) = data.as_object() else {
        return Err(ChunkError::new(
            "release_input",
            format!("{}: manifest must be a JSON object", source.display()),
        ));
    };
    let (file_platform, file_tier) = filename_key(source);
    let tier = match obj.get("tier") {
        Some(Value::String(tier)) => tier.clone(),
        Some(_) => file_tier,
        None => file_tier,
    };
    let platform = match obj.get("platform").and_then(Value::as_str) {
        Some(platform) if !platform.is_empty() => platform.to_string(),
        _ => {
            if file_platform.is_empty() {
                "unknown".to_string()
            } else {
                file_platform
            }
        }
    };
    let raw_chunks = obj
        .get("chunks")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let Some(raw_chunks) = raw_chunks.as_array() else {
        return Err(ChunkError::new(
            "release_input",
            format!("{}: field 'chunks' must be a list", source.display()),
        ));
    };
    let raw_links = obj
        .get("links")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let Some(raw_links) = raw_links.as_array() else {
        return Err(ChunkError::new(
            "release_input",
            format!("{}: field 'links' must be a list", source.display()),
        ));
    };
    let mut chunks = Vec::new();
    for item in raw_chunks {
        chunks.push(parse_chunk(item, source)?);
    }
    let mut links = Vec::new();
    for item in raw_links {
        links.push(parse_link(item, source)?);
    }
    Ok(Manifest {
        platform,
        tier,
        tag: json_str(obj.get("tag")),
        version: version_field(obj),
        chunks,
        links,
    })
}

fn version_field(obj: &Map<String, Value>) -> String {
    if obj.contains_key("version") {
        json_str(obj.get("version"))
    } else {
        json_str(obj.get("product_version"))
    }
}

fn json_str(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_string()
}

fn filename_key(path: &Path) -> (String, String) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return (String::new(), String::new());
    };
    let Some(rest) = name
        .strip_prefix("manifest-")
        .and_then(|stem| stem.strip_suffix(".json"))
    else {
        return (String::new(), String::new());
    };
    if rest.is_empty() {
        return (String::new(), String::new());
    }
    if let Some((platform, tier)) = rest.split_once('.') {
        return (platform.to_string(), tier.to_string());
    }
    match rest.split_once('-') {
        Some((platform, tier)) if !platform.is_empty() => (platform.to_string(), tier.to_string()),
        Some(_) => (String::new(), String::new()),
        None => (rest.to_string(), String::new()),
    }
}

fn parse_chunk(raw: &Value, source: &Path) -> Result<ChunkView, ChunkError> {
    let Some(obj) = raw.as_object() else {
        return Err(ChunkError::new(
            "release_input",
            format!("{}: each chunk must be an object", source.display()),
        ));
    };
    let asset = json_str(obj.get("asset"));
    let name = {
        let named = json_str(obj.get("name"));
        if !named.is_empty() {
            named
        } else if !asset.is_empty() {
            asset.clone()
        } else {
            json_str(obj.get("group"))
        }
    };
    if name.is_empty() {
        return Err(ChunkError::new(
            "release_input",
            format!("{}: chunk is missing a name", source.display()),
        ));
    }
    let raw_files = obj
        .get("files")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let Some(raw_files) = raw_files.as_array() else {
        return Err(ChunkError::new(
            "release_input",
            format!(
                "{}: chunk {name:?} field 'files' must be a list",
                source.display()
            ),
        ));
    };
    let mut files = Vec::new();
    for item in raw_files {
        files.push(parse_file_entry(item, &name, source)?);
    }
    Ok(ChunkView {
        name,
        group: json_str(obj.get("group")),
        asset,
        sha256: json_str(obj.get("sha256")).to_ascii_lowercase(),
        size: optional_i64(obj, "size", source)?,
        unpacked: optional_i64(obj, "unpacked", source)?,
        files,
    })
}

fn optional_i64(obj: &Map<String, Value>, key: &str, source: &Path) -> Result<i64, ChunkError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Bool(_)) => Err(int_error(source, key)),
        Some(Value::Number(number)) => number.as_i64().ok_or_else(|| int_error(source, key)),
        Some(_) => Err(int_error(source, key)),
    }
}

fn int_error(source: &Path, field: &str) -> ChunkError {
    ChunkError::new(
        "release_input",
        format!("{}: field {field:?} must be an integer", source.display()),
    )
}

fn parse_file_entry(raw: &Value, chunk_name: &str, source: &Path) -> Result<FileEntry, ChunkError> {
    match raw {
        Value::String(path) => Ok(FileEntry {
            path: path.clone(),
            chunk: chunk_name.to_string(),
            size: None,
        }),
        Value::Object(obj) => {
            let path = ["path", "name", "file"]
                .iter()
                .find_map(|key| obj.get(*key).and_then(Value::as_str));
            let Some(path) = path.filter(|path| !path.is_empty()) else {
                return Err(ChunkError::new(
                    "release_input",
                    format!(
                        "{}: file entry in chunk {chunk_name:?} has no path",
                        source.display()
                    ),
                ));
            };
            let size = ["size", "size_bytes", "bytes", "unpacked"]
                .iter()
                .find_map(|key| match obj.get(*key) {
                    Some(Value::Number(number)) => number.as_i64(),
                    _ => None,
                });
            Ok(FileEntry {
                path: path.to_string(),
                chunk: chunk_name.to_string(),
                size,
            })
        }
        Value::Array(pair) if pair.first().and_then(Value::as_str).is_some() => {
            let path = pair[0].as_str().unwrap_or("").to_string();
            let size = pair.get(1).and_then(|value| match value {
                Value::Number(number) => number.as_i64(),
                _ => None,
            });
            Ok(FileEntry {
                path,
                chunk: chunk_name.to_string(),
                size,
            })
        }
        _ => Err(ChunkError::new(
            "release_input",
            format!(
                "{}: malformed file entry in chunk {chunk_name:?}",
                source.display()
            ),
        )),
    }
}

fn parse_link(raw: &Value, source: &Path) -> Result<(String, String), ChunkError> {
    if let Some(obj) = raw.as_object() {
        let link = ["link", "path", "name"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_str));
        let target = ["target", "source", "to"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_str));
        if let (Some(link), Some(target)) = (link, target)
            && !link.is_empty()
            && !target.is_empty()
        {
            return Ok((link.to_string(), target.to_string()));
        }
    }
    if let Some(pair) = raw.as_array()
        && pair.len() == 2
        && let (Some(link), Some(target)) = (pair[0].as_str(), pair[1].as_str())
    {
        return Ok((link.to_string(), target.to_string()));
    }
    Err(ChunkError::new(
        "release_input",
        format!("{}: malformed link entry {raw}", source.display()),
    ))
}
