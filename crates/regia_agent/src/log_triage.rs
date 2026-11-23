// SPDX-License-Identifier: MPL-2.0
//! Offline log stitching and session triage (ADR-8035).
//!
//! Input bundles are supplied as text, never filesystem paths. Source offsets in `manifest.txt`
//! govern stitching even when scrubbing changed stored lengths. All output strings use Regia's
//! existing report scrubber. No game-specific event vocabulary or simulation math is involved.

mod analysis;
mod time;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Settings for offline session analysis. Defaults are diagnostic policy, not gameplay limits.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Width of event-rate buckets in seconds.
    pub bucket_seconds: f64,
    /// Silence longer than this is reported as a time gap.
    pub gap_seconds: f64,
    /// A nonzero median bucket count must be exceeded by this factor to report a storm.
    pub storm_factor: f64,
    /// Minimum storm count when the median bucket count is zero.
    pub storm_min_count: u64,
    /// Maximum number of normalized message templates returned per session.
    pub top: usize,
    /// Maximum combined input text bytes. The protocol also enforces its transport budget.
    pub max_bytes: usize,
    /// Maximum input file entries (including manifests).
    pub max_files: usize,
    /// Long-frame threshold used for the session verdict.
    pub slow_frame_ms: f64,
    /// Network RTT threshold used for the session verdict.
    pub slow_rtt_ms: f64,
    /// Packet-loss percentage threshold used for the session verdict.
    pub loss_percent: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bucket_seconds: 60.0,
            gap_seconds: 60.0,
            storm_factor: 10.0,
            storm_min_count: 5,
            top: 15,
            max_bytes: 64 * 1024 * 1024,
            max_files: 4096,
            slow_frame_ms: 50.0,
            slow_rtt_ms: 200.0,
            loss_percent: 5.0,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), RpcError> {
        for number in [
            self.bucket_seconds,
            self.gap_seconds,
            self.storm_factor,
            self.slow_frame_ms,
            self.slow_rtt_ms,
            self.loss_percent,
        ] {
            if !number.is_finite() || number <= 0.0 {
                return Err(error(
                    "logs_triage_settings",
                    "thresholds must be finite and positive",
                ));
            }
        }
        if self.bucket_seconds < 0.001
            || self.storm_factor < 1.0
            || self.loss_percent > 100.0
            || self.storm_min_count == 0
            || self.max_bytes == 0
            || self.max_files == 0
        {
            return Err(error(
                "logs_triage_settings",
                "invalid bucket, storm, loss, or resource limit",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    bundles: Vec<Bundle>,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    include_stitched: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    name: String,
    files: BTreeMap<String, FileData>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileData {
    Text(String),
    Bytes { bytes: Vec<u8> },
}

impl FileData {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes { bytes } => bytes,
        }
    }
    fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Text(text) => text.into_bytes(),
            Self::Bytes { bytes } => bytes,
        }
    }
}

#[derive(Default)]
struct Manifest {
    session: String,
    seq: u64,
    ranges: BTreeMap<String, (u64, u64)>,
}

struct Piece {
    seq: u64,
    bundle: String,
    start: u64,
    end: u64,
    text: Vec<u8>,
}

struct Stitched {
    session: String,
    path: String,
    text: String,
    gaps: Vec<Value>,
    sources: Vec<Value>,
    duplicates: usize,
}

pub(super) fn error(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/logs-triage.md")
}

/// Analyze supplied uploads, returning deterministic session summaries and optional stitched text.
///
/// Parameters: `{ bundles: [{ name, files: { relative_path: text } }], settings?,
/// include_stitched? }`. No file is opened, written or executed. Conflicting or partly overlapping
/// source ranges are refused: scrubbed byte lengths cannot safely be used to trim them.
/// Errors have lowercase `logs_triage_*` codes documented in the agent guide.
#[tracing::instrument(skip_all)]
pub fn triage(params: &Value) -> Result<Value, RpcError> {
    let request: Request = serde_json::from_value(params.clone()).map_err(|_| {
        error(
            "logs_triage_input",
            "expected bundles with name/files and optional settings",
        )
    })?;
    request.settings.validate()?;
    let mut bytes = 0usize;
    let mut count = 0usize;
    for bundle in &request.bundles {
        bytes = bytes.saturating_add(bundle.name.len());
        count = count.saturating_add(bundle.files.len());
        for (path, text) in &bundle.files {
            bytes = bytes
                .saturating_add(path.len())
                .saturating_add(text.bytes().len());
            if !safe_path(path) {
                return Err(error(
                    "logs_triage_path",
                    "input member is not a safe relative path",
                ));
            }
        }
    }
    if bytes > request.settings.max_bytes || count > request.settings.max_files {
        return Err(error(
            "logs_triage_limit",
            "input exceeds configured byte or file budget",
        ));
    }
    let files = stitch(request.bundles)?;
    if files.is_empty() {
        return Err(error(
            "logs_triage_empty",
            "no log or session summary files found",
        ));
    }
    let mut sessions: BTreeMap<&str, Vec<&Stitched>> = BTreeMap::new();
    for file in &files {
        sessions.entry(&file.session).or_default().push(file);
    }
    let reports = sessions
        .into_iter()
        .map(|(session, files)| analysis::summarize(session, &files, &request.settings))
        .collect::<Result<Vec<_>, _>>()?;
    let files = files
        .iter()
        .map(|file| {
            let mut value = json!({"session": file.session, "path": file.path,
            "bytes": file.text.len(), "gaps": file.gaps,
            "sources": file.sources, "duplicates": file.duplicates});
            if request.include_stitched {
                value["text"] = json!(file.text);
            }
            value
        })
        .collect::<Vec<_>>();
    let mut result = json!({"schema": 1, "sessions": reports, "files": files});
    scrub_value(&mut result);
    Ok(result)
}

// Path syntax is deliberately portable, including when archives are inspected on Unix for Windows.
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = regia_core::privacy::scrub_report(text),
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        Value::Object(map) => {
            let old = std::mem::take(map);
            for (key, mut value) in old {
                scrub_value(&mut value);
                map.insert(regia_core::privacy::scrub_report(&key), value);
            }
        }
        _ => {}
    }
}

fn manifest(bundle: &Bundle) -> Result<Manifest, RpcError> {
    let mut result = Manifest::default();
    let mut table = false;
    let manifest_text = bundle
        .files
        .get("manifest.txt")
        .map(|file| std::str::from_utf8(file.bytes()))
        .transpose()
        .map_err(|_| error("logs_triage_input", "manifest must be UTF-8 text"))?
        .unwrap_or("");
    for line in manifest_text.lines() {
        if line.starts_with("entry\t") {
            table = true;
        } else if table {
            let cols: Vec<_> = line.split('\t').collect();
            if cols.len() >= 3 && cols[2].contains("..") {
                let range = cols[2]
                    .split_once("..")
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
                let Some((start, end)) = range else {
                    return Err(error("logs_triage_range", "invalid manifest source range"));
                };
                if end < start || result.ranges.insert(cols[0].to_owned(), (start, end)).is_some() {
                    return Err(error(
                        "logs_triage_range",
                        "reversed or duplicate manifest source range",
                    ));
                }
            }
        } else if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "session" => result.session = value.trim().to_owned(),
                "seq" => {
                    result.seq = value
                        .trim()
                        .parse()
                        .map_err(|_| error("logs_triage_range", "invalid manifest sequence"))?
                }
                _ => {}
            }
        }
    }
    if result.seq == 0 {
        let name = bundle.name.strip_suffix(".zip").unwrap_or(&bundle.name);
        if let Some((_, tail)) = name.rsplit_once("_s") {
            result.seq = tail.parse().unwrap_or(0);
        }
    }
    Ok(result)
}

fn log_stem(name: &str) -> Option<&str> {
    if name == "summary.json" || name == "summary.partial.json" {
        return Some("");
    }
    [
        ".summary.partial.json",
        ".summary.json",
        "-summary.json",
        "-system.json",
        ".jsonl",
        ".log",
    ]
    .iter()
    .find_map(|suffix| name.strip_suffix(suffix))
}

/// A canonical `summary.json` at the bundle root joins unstamped siblings under the bundle name.
fn canonical_root(files: &BTreeMap<String, FileData>) -> bool {
    files.keys().any(|path| {
        let base = path.rsplit_once(".from-").map_or(path.as_str(), |(base, _)| base);
        let rel = base.strip_prefix("logs/").unwrap_or(base);
        !rel.contains('/') && (rel == "summary.json" || rel == "summary.partial.json")
    })
}

fn classify(
    path: &str,
    header: &str,
    bundle: &str,
    canonical: bool,
) -> Result<Option<(String, String, u64, bool)>, RpcError> {
    let (base, offset) = if let Some((base, offset)) = path.rsplit_once(".from-") {
        (
            base,
            offset
                .parse()
                .map_err(|_| error("logs_triage_range", "invalid source offset"))?,
        )
    } else {
        (path, 0)
    };
    let rel = base.strip_prefix("logs/").unwrap_or(base);
    // More than one remaining directory is a per-match trace and would double-count events.
    // One parent directory is the session folder (`session-a/summary.json`).
    if rel.matches('/').count() > 1 {
        return Ok(None);
    }
    let (dir, name) = rel
        .split_once('/')
        .map(|(dir, name)| (Some(dir), name))
        .unwrap_or((None, rel));
    let Some(stem) = log_stem(name) else {
        return Ok(None);
    };
    let bundle_name = bundle.strip_suffix(".zip").unwrap_or(bundle);
    let session = if let Some(stamped) = stamp(stem) {
        stamped
    } else if let Some(dir) = dir {
        dir.to_owned()
    } else if !header.is_empty() {
        header.to_owned()
    } else if canonical || stem.is_empty() {
        bundle_name.to_owned()
    } else {
        stem.to_owned()
    };
    if !safe_path(&session) || session.contains('/') {
        return Err(error(
            "logs_triage_path",
            "session must be one safe path component",
        ));
    }
    Ok(Some((session, rel.to_owned(), offset, name.ends_with(".json"))))
}

fn stamp(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    for at in 0..bytes.len().saturating_sub(16) {
        let tail = &bytes[at..];
        if tail.len() >= 17
            && tail[..8].iter().all(u8::is_ascii_digit)
            && tail[8] == b'-'
            && tail[9..15].iter().all(u8::is_ascii_digit)
            && tail[15] == b'-'
            && tail[16..].iter().all(u8::is_ascii_digit)
        {
            return Some(stem[at..].to_owned());
        }
    }
    None
}

fn stitch(bundles: Vec<Bundle>) -> Result<Vec<Stitched>, RpcError> {
    let mut groups: BTreeMap<(String, String, bool), Vec<Piece>> = BTreeMap::new();
    for bundle in bundles {
        let manifest = manifest(&bundle)?;
        let canonical = canonical_root(&bundle.files);
        for (path, text) in bundle.files {
            let text = text.into_bytes();
            let Some((session, path_out, off, whole)) =
                classify(&path, &manifest.session, &bundle.name, canonical)?
            else {
                continue;
            };
            let (start, end) = manifest.ranges.get(&path).copied().unwrap_or((
                off,
                off.checked_add(text.len() as u64)
                    .ok_or_else(|| error("logs_triage_range", "source offset overflow"))?,
            ));
            if start == end && !text.is_empty() {
                return Err(error(
                    "logs_triage_range",
                    "nonempty data has an empty source range",
                ));
            }
            groups.entry((session, path_out, whole)).or_default().push(Piece {
                seq: manifest.seq,
                bundle: bundle.name.clone(),
                start,
                end,
                text,
            });
        }
    }
    let mut files = Vec::new();
    for ((session, path, whole), mut pieces) in groups {
        pieces.sort_by(|a, b| {
            (a.start, a.end, a.seq, &a.bundle, &a.text).cmp(&(b.start, b.end, b.seq, &b.bundle, &b.text))
        });
        let mut file = Stitched {
            session,
            path,
            text: String::new(),
            gaps: Vec::new(),
            sources: Vec::new(),
            duplicates: 0,
        };
        let mut joined = Vec::new();
        if whole {
            pieces.sort_by(|a, b| (a.seq, &a.bundle, &a.text).cmp(&(b.seq, &b.bundle, &b.text)));
            if let Some(newest) = pieces.pop() {
                file.sources
                    .push(json!({"bundle": newest.bundle, "seq": newest.seq}));
                joined = newest.text;
            }
        } else {
            let mut pos = 0;
            let mut previous: Option<(u64, u64, Vec<u8>)> = None;
            for piece in pieces {
                if let Some((start, end, text)) = &previous
                    && *start == piece.start
                    && *end == piece.end
                    && *text == piece.text
                {
                    file.duplicates += 1;
                    continue;
                }
                if piece.start < pos {
                    return Err(error(
                        "logs_triage_overlap",
                        "overlapping source ranges cannot be trimmed after scrubbing",
                    ));
                }
                if piece.start > pos {
                    file.gaps
                        .push(json!({"start": pos, "end": piece.start, "output_offset": joined.len()}));
                    // Do not turn two broken records across a missing range into one valid record.
                    if !joined.is_empty() && !joined.ends_with(b"\n") {
                        joined.push(b'\n');
                    }
                }
                file.sources.push(json!({"bundle": piece.bundle, "seq": piece.seq,
                    "start": piece.start, "end": piece.end}));
                pos = piece.end;
                joined.extend_from_slice(&piece.text);
                previous = Some((piece.start, piece.end, piece.text));
            }
        }
        file.text = String::from_utf8_lossy(&joined).into_owned();
        files.push(file);
    }
    Ok(files)
}

#[cfg(test)]
mod tests;
