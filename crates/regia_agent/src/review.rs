// SPDX-License-Identifier: MPL-2.0
//! Staged review (`docs/specs/23-agent-native.md` § 5, ADR-2346): an agent proposes a change set
//! of text files; nothing touches the project until a human (the editor's Agent Changes panel, or
//! a client holding the `review` permission) accepts it.
//!
//! - `review.propose` stages whole-file texts with the agent's stated reason; each change records
//!   the file's text at proposal time (`before`), so `review.diff` shows a unified diff and
//!   `review.accept` can refuse a change whose file moved on since (`review_conflict`).
//! - `review.accept` writes the accepted files (temp file + rename) and stamps their `.meta`
//!   provenance `reviewer: owner` (specs/23 § 5); it needs the `review` permission, which no
//!   default profile and no tool scope grants, so an agent cannot approve its own work.
//! - `review.reject` drops changes unapplied (the proposing agent may withdraw its own).
//!
//! The queue persists in `.regia/agent/review/queue.json` (format version 2, written atomically;
//! a version 1 file still loads), so a proposal waits across editor restarts. The diff is a line
//! LCS between the common prefix and suffix (Wagner and Fischer 1974; Hunt and McIlroy 1976),
//! bounded by [`MAX_DIFF_CELLS`]; a larger middle is shown as one replaced block.
//!
//! A change with a [`GoldenProposal`] (`agent.bless_golden`, ADR-2821) stays out of this path: its
//! texts are a readable summary, and [`ReviewQueue::decide`] never applies it. Only `regia golden
//! accept`, at the owner's terminal, does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::jobs::reason_error;
use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Version of `.regia/agent/review/queue.json`. Version 2 adds [`StagedChange::golden`]; a version 1
/// file reads unchanged, and a version 1 build refuses a version 2 file instead of applying a
/// golden proposal's summary text over its PNG (ADR-2821).
pub const REVIEW_QUEUE_FORMAT_VERSION: u32 = 2;

/// Most changes one proposal carries.
pub const MAX_CHANGES_PER_PROPOSAL: usize = 64;

/// Most pending changes the queue holds.
pub const MAX_PENDING: usize = 256;

/// Most bytes of one staged file text.
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Decided changes kept for `review.list` (the oldest are forgotten first).
pub const KEEP_DECIDED: usize = 256;

/// Decided changes that keep their texts (the newest ones); older decided changes keep only
/// their summary, so the queue holds at most this many decided texts.
pub const KEEP_DECIDED_TEXTS: usize = 32;

/// Largest LCS table (lines × lines) the diff builds; a larger changed middle is one block.
pub const MAX_DIFF_CELLS: usize = 4_000_000;

/// Context lines around each hunk.
pub const DIFF_CONTEXT: usize = 3;

/// A change's review state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// Waiting for a decision.
    Pending,
    /// Applied.
    Accepted,
    /// Dropped unapplied.
    Rejected,
}

impl ChangeStatus {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// What makes a staged change a golden proposal (`agent.bless_golden`, ADR-2821): the images live in
/// `dir` as `before.png`, `after.png` and `diff.png`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenProposal {
    /// The golden's name as `agent.golden_test` takes it (`<owner>:<stem>`).
    pub name: String,
    /// The `agent.golden_test` job the actual image came from.
    pub job: String,
    /// Project-relative folder holding the three images.
    pub dir: String,
    /// BLAKE3 of the golden when proposed; accepting needs the file to still hash to this.
    pub before_hash: String,
    /// BLAKE3 of `after.png`.
    pub after_hash: String,
    /// The measured stats of the job, as JSON text.
    pub stats: String,
}

/// One staged file change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedChange {
    /// Change id (unique in the queue).
    pub id: u64,
    /// The change set it belongs to (one `review.propose` call).
    pub set: u64,
    /// Project-relative path with `/` separators.
    pub path: String,
    /// The file's text when proposed (`None`: the file did not exist).
    pub before: Option<String>,
    /// The proposed text.
    pub after: String,
    /// The agent's stated reason (the set's).
    pub reason: String,
    /// The set's title.
    pub title: String,
    /// Proposing client, session and model (specs/23 § 5 provenance).
    pub client: String,
    /// Session id.
    pub session: String,
    /// Model, when reported.
    pub model: String,
    /// Project-relative before/after screenshots the agent attached.
    #[serde(default)]
    pub images: BTreeMap<String, String>,
    /// Unix ms of the proposal.
    pub created_ms: u64,
    /// Review state.
    pub status: ChangeStatus,
    /// Unix ms of the decision.
    #[serde(default)]
    pub decided_ms: Option<u64>,
    /// Who decided (`owner` for the editor panel, else the client name).
    #[serde(default)]
    pub decided_by: Option<String>,
    /// The reviewer's note.
    #[serde(default)]
    pub note: Option<String>,
    /// Lines the change adds, counted when it is proposed (0 with `removed` 0: count again).
    #[serde(default)]
    pub added: usize,
    /// Lines the change removes, counted when it is proposed.
    #[serde(default)]
    pub removed: usize,
    /// The texts of an older decided change were dropped to bound the queue
    /// ([`KEEP_DECIDED_TEXTS`]); its diff is no longer available.
    #[serde(default)]
    pub texts_dropped: bool,
    /// Set for a golden proposal: only the owner's terminal accepts it (ADR-2821).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub golden: Option<GoldenProposal>,
}

impl StagedChange {
    /// `create` or `modify`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        if self.before.is_some() { "modify" } else { "create" }
    }

    /// `(added, removed)` line counts: the ones stored at proposal, else counted now (a queue
    /// file written before the counts were stored).
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        if self.added > 0 || self.removed > 0 || self.texts_dropped {
            return (self.added, self.removed);
        }
        let diff = line_diff(self.before.as_deref().unwrap_or(""), &self.after);
        (diff.added, diff.removed)
    }

    /// The change's summary row (no texts).
    #[must_use]
    pub fn summary(&self) -> Value {
        let (added, removed) = self.counts();
        let mut row = json!({
            "id": self.id, "set": self.set, "path": self.path, "kind": self.kind(),
            "status": self.status.name(), "title": self.title, "reason": self.reason,
            "client": self.client, "model": self.model, "created_ms": self.created_ms,
            "added": added, "removed": removed, "images": self.images,
            "decided_by": self.decided_by, "note": self.note,
        });
        if let (Some(golden), Some(object)) = (&self.golden, row.as_object_mut()) {
            object.insert(
                "golden".into(),
                json!({
                    "name": golden.name,
                    "job": golden.job,
                    "before": format!("{}/before.png", golden.dir),
                    "after": format!("{}/after.png", golden.dir),
                    "diff": format!("{}/diff.png", golden.dir),
                    "stats": serde_json::from_str::<Value>(&golden.stats).unwrap_or(Value::Null),
                    "accept": format!("regia golden accept {}", self.id),
                }),
            );
        }
        row
    }
}

/// The on-disk queue file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFile {
    /// [`REVIEW_QUEUE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Next change id.
    pub next_id: u64,
    /// Next set id.
    pub next_set: u64,
    /// Every kept change, oldest first.
    pub changes: Vec<StagedChange>,
}

impl QueueFile {
    /// Parses queue JSON, refusing a newer version.
    ///
    /// # Errors
    /// Parse errors and unsupported versions, as text.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let file: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if file.format_version == 0 || file.format_version > REVIEW_QUEUE_FORMAT_VERSION {
            return Err(format!(
                "review queue format_version {} (this build reads 1..={REVIEW_QUEUE_FORMAT_VERSION})",
                file.format_version
            ));
        }
        Ok(file)
    }

    /// Canonical JSON (pretty, stable field order).
    ///
    /// # Errors
    /// Serialization failure, as text.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }
}

/// The review queue of one project (a resource; loaded from disk on first use).
#[derive(Resource, Debug, Clone, Default)]
pub struct ReviewQueue {
    /// The queue state.
    pub file: QueueFile,
    /// Where it persists (`None`: memory only).
    pub path: Option<PathBuf>,
    /// The project root files resolve against.
    pub root: PathBuf,
    /// The last persistence error, if any.
    pub last_error: Option<String>,
    /// The queue file's modification time and length when last read or written: another process
    /// (`regia golden accept`) writing it makes [`queue`] read it again.
    pub stamp: Option<(std::time::SystemTime, u64)>,
}

/// The modification time and length of `path`.
fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Where the queue of `root` persists.
#[must_use]
pub fn queue_path(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root)
        .join("agent")
        .join("review")
        .join("queue.json")
}

impl ReviewQueue {
    /// Loads the queue of `root` (an absent file is an empty queue; an unreadable one starts
    /// empty and keeps the error, never overwriting the bad file until the next change).
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let path = queue_path(root);
        let (file, last_error) = match std::fs::read_to_string(&path) {
            Ok(text) => match QueueFile::from_json(&text) {
                Ok(file) => (file, None),
                Err(e) => (QueueFile::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (QueueFile::default(), None),
            Err(e) => (QueueFile::default(), Some(format!("{}: {e}", path.display()))),
        };
        Self {
            file: QueueFile {
                format_version: REVIEW_QUEUE_FORMAT_VERSION,
                ..file
            },
            stamp: file_stamp(&path),
            path: Some(path),
            root: root.to_path_buf(),
            last_error,
        }
    }

    /// Writes the queue (temp file + rename). Errors are kept in `last_error` and logged.
    pub fn save(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let result = (|| -> Result<(), String> {
            let text = self.file.to_json()?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            write_atomic(&path, text.as_bytes())
        })();
        if let Err(e) = &result {
            tracing::warn!(target: "regia_agent", "review queue {}: {e}", path.display());
        }
        self.last_error = result.err();
        self.stamp = file_stamp(&path);
    }

    /// Pending changes.
    pub fn pending(&self) -> impl Iterator<Item = &StagedChange> {
        self.file
            .changes
            .iter()
            .filter(|c| c.status == ChangeStatus::Pending)
    }

    /// Forgets the oldest decided changes beyond [`KEEP_DECIDED`], and drops the texts of
    /// decided changes beyond the newest [`KEEP_DECIDED_TEXTS`].
    fn prune(&mut self) {
        let mut keep_texts = KEEP_DECIDED_TEXTS;
        for change in self.file.changes.iter_mut().rev() {
            if change.status == ChangeStatus::Pending || change.texts_dropped {
                continue;
            }
            if keep_texts > 0 {
                keep_texts -= 1;
                continue;
            }
            let (added, removed) = change.counts();
            change.added = added;
            change.removed = removed;
            // `Some("")` keeps the change's kind (modify) readable.
            change.before = change.before.as_ref().map(|_| String::new());
            change.after = String::new();
            change.texts_dropped = true;
        }
        let decided = self
            .file
            .changes
            .iter()
            .filter(|c| c.status != ChangeStatus::Pending)
            .count();
        let mut excess = decided.saturating_sub(KEEP_DECIDED);
        let root = self.root.clone();
        self.file.changes.retain(|c| {
            if excess > 0 && c.status != ChangeStatus::Pending {
                excess -= 1;
                if let Some(golden) = &c.golden {
                    remove_staged_images(&root, golden);
                }
                false
            } else {
                true
            }
        });
    }

    /// Decides one pending change: `accept` writes it (after the conflict check) and stamps its
    /// `.meta`; otherwise it is rejected. Answers the per-change outcome.
    pub fn decide(&mut self, id: u64, accept: bool, by: &str, note: Option<&str>) -> Value {
        let root = self.root.clone();
        let Some(change) = self.file.changes.iter_mut().find(|c| c.id == id) else {
            return json!({ "id": id, "applied": false, "reason": "review_change_not_found" });
        };
        if change.status != ChangeStatus::Pending {
            return json!({ "id": id, "applied": false, "reason": "review_already_decided", "status": change.status.name() });
        }
        // ADR-2821: whoever calls this (`review.accept`, the editor panel), a golden proposal is
        // not applied here. It stays pending for the owner's terminal.
        if accept && change.golden.is_some() {
            return json!({
                "id": id, "path": change.path, "applied": false, "reason": "golden_owner_only",
                "message": format!("a golden changes only by `regia golden accept {id}`, typed by the owner at a terminal"),
            });
        }
        if !accept && let Some(golden) = &change.golden {
            remove_staged_images(&root, golden);
        }
        if accept && let Err(reason) = apply(&root, change) {
            return json!({ "id": id, "path": change.path, "applied": false, "reason": reason.0, "message": reason.1 });
        }
        change.status = if accept {
            ChangeStatus::Accepted
        } else {
            ChangeStatus::Rejected
        };
        change.decided_ms = Some(crate::safety::unix_ms_now());
        change.decided_by = Some(by.to_string());
        change.note = note.map(str::to_string);
        let meta = if accept {
            stamp_reviewed(&root, change)
        } else {
            "skipped"
        };
        json!({ "id": id, "path": change.path, "applied": accept, "status": change.status.name(), "meta": meta })
    }
}

/// Removes a decided golden proposal's staged images. Best effort: a leftover folder under
/// `.regia/` costs disk, nothing else.
fn remove_staged_images(root: &Path, golden: &GoldenProposal) {
    // A hand-written queue names any folder: only ever one under the staging folder.
    if let Ok(dir) = regia_core::project_path::resolve_write(root, &golden.dir)
        && dir.starts_with(crate::golden_tools::staging_root(root))
    {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Writes `bytes` to `path` through a temp file in the same directory and a rename. The temp
/// file is created new (a leftover or a planted link at its name is removed first, never
/// followed).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()));
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    renamed.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Applies an accepted change: the file must still hold `before` (or still be absent).
fn apply(root: &Path, change: &StagedChange) -> Result<(), (&'static str, String)> {
    // `review.propose` cannot stage this (the dispatcher's guard); a hand-written queue can.
    if crate::golden_tools::names_golden(&change.path) {
        return Err((
            "review_path_denied",
            format!("{} is a golden; goldens are the owner's (ADR-2821)", change.path),
        ));
    }
    let path = regia_core::project_path::resolve_write(root, &change.path)
        .map_err(|e| ("review_path_denied", format!("{}: {e}", change.path)))?;
    let current = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(("review_write_failed", format!("{}: {e}", change.path))),
    };
    if current.as_deref() != change.before.as_deref().map(str::as_bytes) {
        return Err((
            "review_conflict",
            format!(
                "{} changed since the proposal; ask the agent to propose again",
                change.path
            ),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ("review_write_failed", e.to_string()))?;
        regia_core::project_path::check_under_root(root, dir)
            .map_err(|e| ("review_path_denied", format!("{}: {e}", change.path)))?;
    }
    write_atomic(&path, change.after.as_bytes()).map_err(|e| ("review_write_failed", e))
}

/// Stamps the accepted file's `.meta` provenance as reviewed by the owner (specs/23 § 5); answers
/// `stamped`, `no_meta` or `failed`.
fn stamp_reviewed(root: &Path, change: &StagedChange) -> &'static str {
    let Ok(source) = regia_core::project_path::resolve_write(root, &change.path) else {
        return "failed";
    };
    let meta_path = regia_asset::meta_path_for(&source);
    let Ok(bytes) = std::fs::read(&meta_path) else {
        return "no_meta";
    };
    let Ok(mut meta) = regia_asset::MetaSidecar::from_ron(&bytes) else {
        return "failed";
    };
    let mut provenance = meta.provenance.take().unwrap_or_default();
    if provenance.tool == regia_asset::ProvenanceTool::Unknown {
        provenance.tool = regia_asset::ProvenanceTool::Agent;
    }
    let fill = |slot: &mut Option<String>, value: &str| {
        if slot.is_none() && !value.is_empty() {
            *slot = Some(value.to_string());
        }
    };
    fill(&mut provenance.client, &change.client);
    fill(&mut provenance.session, &change.session);
    fill(&mut provenance.model, &change.model);
    fill(
        &mut provenance.created_utc,
        &rfc3339_from_unix_ms(change.created_ms),
    );
    provenance.reviewer = regia_asset::ReviewerState::Owner;
    meta.provenance = Some(provenance);
    match meta.to_ron_string() {
        Ok(text) if write_atomic(&meta_path, text.as_bytes()).is_ok() => "stamped",
        _ => "failed",
    }
}

/// RFC 3339 UTC of a unix-ms time (Hinnant's `civil_from_days`, public algorithm).
#[must_use]
pub fn rfc3339_from_unix_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let tod = secs.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// A line diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineDiff {
    /// Unified diff text (no file header).
    pub hunks: String,
    /// Added lines.
    pub added: usize,
    /// Removed lines.
    pub removed: usize,
}

/// One diff operation over lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep(usize, usize),
    Del(usize),
    Add(usize),
}

/// Lines for [`line_diff`].
///
/// A final `\n` does not add an empty line, matching `str::lines` on LF text, so the
/// existing hunk tests stay exact. A `\r` before that `\n` stays on the line, so CRLF
/// versus LF is a visible edit. A string that is only a newline is one empty line:
/// `str::lines` drops it and the diff would be silent.
fn diff_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            // Keep a `\r` that belongs to this line. `str::lines` strips it, which hid a
            // CRLF versus LF edit.
            lines.push(&text[start..i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// A diff the line split would hide: the texts differ only by a final newline. The hunk
/// names that newline so a staged review is not an empty patch. It replaces the last line, at
/// its own position, with the standard `\ No newline at end of file` marker after the side
/// that lacks the newline (ADR-5597: a fixed `@@ -1,1 +0,0 @@` hunk named line 1 and a
/// removed line for any file).
fn newline_only_diff(a: &str, b: &str) -> LineDiff {
    let lines = diff_lines(a);
    if let Some(last) = lines.last() {
        let n = lines.len();
        let marker = "\\ No newline at end of file\n";
        let hunks = if a.ends_with('\n') && !b.ends_with('\n') {
            Some(format!("@@ -{n},1 +{n},1 @@\n-{last}\n+{last}\n{marker}"))
        } else if !a.ends_with('\n') && b.ends_with('\n') {
            Some(format!("@@ -{n},1 +{n},1 @@\n-{last}\n{marker}+{last}\n"))
        } else {
            None
        };
        if let Some(hunks) = hunks {
            return LineDiff {
                hunks,
                added: 1,
                removed: 1,
            };
        }
    }
    let (added, removed, hunks) = if a.ends_with('\n') && !b.ends_with('\n') {
        (0, 1, "@@ -1,1 +0,0 @@\n-\\ No newline at end of file\n")
    } else if !a.ends_with('\n') && b.ends_with('\n') {
        (1, 0, "@@ -0,0 +1,1 @@\n+\\ No newline at end of file\n")
    } else {
        (
            1,
            1,
            "@@ -1,1 +1,1 @@\n-\\ No newline at end of file\n+\\ No newline at end of file\n",
        )
    };
    LineDiff {
        hunks: hunks.to_string(),
        added,
        removed,
    }
}

/// The line diff of `a` → `b` with [`DIFF_CONTEXT`] lines of context.
#[must_use]
pub fn line_diff(a: &str, b: &str) -> LineDiff {
    let (raw_a, raw_b) = (a, b);
    let a = diff_lines(a);
    let b = diff_lines(b);
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Keep(i, i)).collect();
    let cells = (a_mid.len() + 1).saturating_mul(b_mid.len() + 1);
    if cells <= MAX_DIFF_CELLS {
        // LCS lengths from the back: table[i][j] = LCS of a_mid[i..], b_mid[j..].
        let width = b_mid.len() + 1;
        let mut table = vec![0u32; cells];
        for i in (0..a_mid.len()).rev() {
            for j in (0..b_mid.len()).rev() {
                table[i * width + j] = if a_mid[i] == b_mid[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a_mid.len() || j < b_mid.len() {
            if i < a_mid.len() && j < b_mid.len() && a_mid[i] == b_mid[j] {
                ops.push(Op::Keep(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if i < a_mid.len()
                && (j == b_mid.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                // Removals before additions, as unified diffs show them.
                ops.push(Op::Del(prefix + i));
                i += 1;
            } else {
                ops.push(Op::Add(prefix + j));
                j += 1;
            }
        }
    } else {
        ops.extend((0..a_mid.len()).map(|i| Op::Del(prefix + i)));
        ops.extend((0..b_mid.len()).map(|j| Op::Add(prefix + j)));
    }
    ops.extend((0..suffix).map(|k| Op::Keep(a.len() - suffix + k, b.len() - suffix + k)));
    let diff = render_hunks(&ops, &a, &b);
    if diff.added == 0 && diff.removed == 0 && raw_a != raw_b {
        newline_only_diff(raw_a, raw_b)
    } else {
        diff
    }
}

/// Groups operations into unified-diff hunks.
fn render_hunks(ops: &[Op], a: &[&str], b: &[&str]) -> LineDiff {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Keep(..)))
        .map(|(k, _)| k)
        .collect();
    let mut out = String::new();
    let (mut added, mut removed) = (0, 0);
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(DIFF_CONTEXT);
        let mut end = changed[k];
        while k + 1 < changed.len() && changed[k + 1] <= end + 2 * DIFF_CONTEXT + 1 {
            k += 1;
            end = changed[k];
        }
        let end = (end + DIFF_CONTEXT + 1).min(ops.len());
        k += 1;
        let hunk = &ops[start..end];
        // Line numbers (1-based) where the hunk starts in a and b.
        let (mut a_at, mut b_at) = (None, None);
        let (mut a_len, mut b_len) = (0, 0);
        for op in hunk {
            match *op {
                Op::Keep(i, j) => {
                    a_at.get_or_insert(i);
                    b_at.get_or_insert(j);
                    a_len += 1;
                    b_len += 1;
                }
                Op::Del(i) => {
                    a_at.get_or_insert(i);
                    a_len += 1;
                }
                Op::Add(j) => {
                    b_at.get_or_insert(j);
                    b_len += 1;
                }
            }
        }
        let a_start = if a_len == 0 {
            a_at.unwrap_or(0)
        } else {
            a_at.unwrap_or(0) + 1
        };
        let b_start = if b_len == 0 {
            b_at.unwrap_or(0)
        } else {
            b_at.unwrap_or(0) + 1
        };
        out.push_str(&format!("@@ -{a_start},{a_len} +{b_start},{b_len} @@\n"));
        for op in hunk {
            match *op {
                Op::Keep(i, _) => {
                    out.push(' ');
                    out.push_str(a[i]);
                }
                Op::Del(i) => {
                    removed += 1;
                    out.push('-');
                    out.push_str(a[i]);
                }
                Op::Add(j) => {
                    added += 1;
                    out.push('+');
                    out.push_str(b[j]);
                }
            }
            out.push('\n');
        }
    }
    LineDiff {
        hunks: out,
        added,
        removed,
    }
}

/// The queue of the world's project, loaded on first use (the editor's Agent Changes panel reads
/// it through this).
///
/// The queue persists only when the app knows its project (an asset server, a configured
/// `project_dir`, or an advertised endpoint); otherwise it lives in memory, so an app without a
/// project never writes `.regia/` into its working directory.
pub fn queue(world: &mut World) -> Mut<'_, ReviewQueue> {
    // Another process accepted or rejected a change (`regia golden accept`): read it again, so a
    // later save here does not undo that decision.
    if let Some(mut existing) = world.get_resource_mut::<ReviewQueue>()
        && let Some(path) = existing.path.clone()
        && file_stamp(&path) != existing.stamp
    {
        let root = existing.root.clone();
        let reloaded = ReviewQueue::load(&root);
        if reloaded.last_error.is_none() {
            *existing = reloaded;
        } else {
            existing.stamp = reloaded.stamp;
            existing.last_error = reloaded.last_error;
        }
    }
    if !world.contains_resource::<ReviewQueue>() {
        let root = crate::req_path::project_root(world);
        let known = world.contains_resource::<regia_asset::AssetServer>()
            || world.contains_resource::<crate::AgentEndpoint>()
            || world
                .get_resource::<crate::AgentSettings>()
                .is_some_and(|s| s.project_dir.is_some());
        let queue = if known {
            ReviewQueue::load(&root)
        } else {
            ReviewQueue {
                file: QueueFile {
                    format_version: REVIEW_QUEUE_FORMAT_VERSION,
                    ..QueueFile::default()
                },
                path: None,
                root,
                last_error: None,
                stamp: None,
            }
        };
        world.insert_resource(queue);
    }
    world.resource_mut::<ReviewQueue>()
}

fn denied(reason: &'static str, message: impl Into<String>) -> RpcError {
    let mut error = reason_error(INVALID_PARAMS, "F0002", reason, message);
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("hint".into(), json!("see book/agents/staged-review.md"));
    }
    error
}

/// `review.propose`: stages a change set.
pub(crate) fn propose(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(changes) = crate::params::opt_array(params, "changes")? else {
        return Err(RpcError::invalid_params(
            "`changes` (an array of { path, text }) is required",
        ));
    };
    if changes.is_empty() || changes.len() > MAX_CHANGES_PER_PROPOSAL {
        return Err(RpcError::invalid_params(format!(
            "`changes` holds 1..={MAX_CHANGES_PER_PROPOSAL} items"
        )));
    }
    let Some(reason) = crate::params::opt_str(params, "reason")?.filter(|r| !r.trim().is_empty()) else {
        return Err(RpcError::invalid_params(
            "`reason` (why the agent wants this change) is required",
        ));
    };
    let title = crate::params::opt_str(params, "title")?
        .unwrap_or(reason)
        .to_string();
    let root = crate::req_path::project_root(world);
    let session = identity(world);
    let mut images = BTreeMap::new();
    if let Some(given) = crate::params::opt_object(params, "images")? {
        for (key, value) in given {
            if key != "before" && key != "after" {
                return Err(RpcError::invalid_params(
                    "`images` takes `before` and `after` only",
                ));
            }
            let Some(rel) = value.as_str() else {
                return Err(RpcError::invalid_params(
                    "`images` values are project-relative paths",
                ));
            };
            crate::req_path::project_path_in(&root, "images", rel)?;
            images.insert(key.clone(), rel.to_string());
        }
    }
    // Validate every change before staging any.
    let mut staged = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for change in changes {
        let (Some(rel), Some(text)) = (
            crate::params::opt_str(change, "path")?,
            crate::params::opt_str(change, "text")?,
        ) else {
            return Err(RpcError::invalid_params("each change is { path, text }"));
        };
        if text.len() > MAX_TEXT_BYTES {
            return Err(denied(
                "review_too_large",
                format!(
                    "{rel}: the text is {} bytes; at most {MAX_TEXT_BYTES}",
                    text.len()
                ),
            ));
        }
        let path = crate::req_path::project_path_in(&root, "path", rel)?;
        let normalized = rel.replace('\\', "/");
        let state_dir = regia_core::brand::project_state_dir(&root);
        let state_name = state_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(".regia")
            .to_ascii_lowercase();
        // Case-insensitive: `.REGIA` and `.GIT` are the same folders on Windows and macOS.
        let lowered = normalized.to_ascii_lowercase();
        let first = lowered
            .split('/')
            .find(|part| !part.is_empty() && *part != ".")
            .unwrap_or_default();
        if path.starts_with(&state_dir)
            || first == state_name
            || lowered.split('/').any(|part| part == ".git")
        {
            return Err(denied(
                "review_path_denied",
                format!("{rel}: the agent state and version-control folders are not reviewable files"),
            ));
        }
        if !seen.insert(normalized.clone()) {
            return Err(RpcError::invalid_params(format!(
                "{rel} appears twice in one proposal"
            )));
        }
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > MAX_TEXT_BYTES as u64
        {
            return Err(denied(
                "review_too_large",
                format!(
                    "{rel} is {} bytes; staged review takes files of at most {MAX_TEXT_BYTES}",
                    meta.len()
                ),
            ));
        }
        let before = match std::fs::read(&path) {
            Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
                denied(
                    "review_not_text",
                    format!("{rel} is not UTF-8 text; staged review takes text files"),
                )
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(RpcError::internal(format!("cannot read {rel}: {e}"))),
        };
        staged.push((normalized, before, text.to_string()));
    }
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let mut queue = queue(world);
    let pending = queue.pending().count();
    let new = staged
        .iter()
        .filter(|(_, before, after)| before.as_deref() != Some(after.as_str()))
        .count();
    if pending + new > MAX_PENDING {
        return Err(denied(
            "review_queue_full",
            format!("{pending} changes are pending (at most {MAX_PENDING}); wait for the owner's review"),
        ));
    }
    let (Some(set), Some(_)) = (
        queue.file.next_set.checked_add(1),
        queue
            .file
            .next_id
            .checked_add(1 + MAX_CHANGES_PER_PROPOSAL as u64),
    ) else {
        return Err(denied(
            "review_queue_full",
            "the queue's ids are exhausted; clear .regia/agent/review/queue.json",
        ));
    };
    let now = crate::safety::unix_ms_now();
    let mut answers = Vec::new();
    let mut to_add = Vec::new();
    for (k, (path, before, after)) in staged.into_iter().enumerate() {
        if before.as_deref() == Some(after.as_str()) {
            answers.push(json!({ "path": path, "kind": "unchanged", "staged": false }));
            continue;
        }
        let change = StagedChange {
            id: queue.file.next_id + 1 + k as u64,
            set,
            path,
            before,
            after,
            reason: reason.to_string(),
            title: title.clone(),
            client: session.client.clone(),
            session: session.session.clone(),
            model: session.model.clone(),
            images: images.clone(),
            created_ms: now,
            status: ChangeStatus::Pending,
            decided_ms: None,
            decided_by: None,
            note: None,
            added: 0,
            removed: 0,
            texts_dropped: false,
            golden: None,
        };
        let diff = line_diff(change.before.as_deref().unwrap_or(""), &change.after);
        let change = StagedChange {
            added: diff.added,
            removed: diff.removed,
            ..change
        };
        answers.push(json!({
            "id": change.id, "path": change.path, "kind": change.kind(), "staged": !dry_run,
            "added": diff.added, "removed": diff.removed, "diff": diff.hunks,
        }));
        to_add.push(change);
    }
    if dry_run {
        return Ok(json!({ "dry_run": true, "set": null, "changes": answers }));
    }
    if to_add.is_empty() {
        return Ok(json!({ "set": null, "changes": answers }));
    }
    queue.file.next_set = set;
    queue.file.next_id = to_add.iter().map(|c| c.id).max().unwrap_or(queue.file.next_id);
    queue.file.changes.extend(to_add);
    queue.save();
    let persisted = queue.last_error.is_none();
    Ok(json!({ "set": set, "changes": answers, "persisted": persisted }))
}

/// `review.list`: changes by status (default pending), optionally of one set.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let status = crate::params::opt_str(params, "status")?.unwrap_or("pending");
    if !matches!(status, "pending" | "accepted" | "rejected" | "all") {
        return Err(RpcError::invalid_params(
            "`status` is pending, accepted, rejected or all",
        ));
    }
    let set = crate::params::opt_u64(params, "set")?;
    let queue = queue(world);
    let rows: Vec<Value> = queue
        .file
        .changes
        .iter()
        .filter(|c| status == "all" || c.status.name() == status)
        .filter(|c| set.is_none_or(|s| c.set == s))
        .map(StagedChange::summary)
        .collect();
    Ok(json!({
        "changes": rows,
        "pending": queue.pending().count(),
        "queue_file": queue.path.as_ref().map(|_| ".regia/agent/review/queue.json"),
        "last_error": queue.last_error,
    }))
}

/// A change's unified diff with its `---`/`+++` header.
#[must_use]
pub fn change_diff(change: &StagedChange) -> String {
    let diff = line_diff(change.before.as_deref().unwrap_or(""), &change.after);
    let from = if change.before.is_some() {
        format!("a/{}", change.path)
    } else {
        "/dev/null".into()
    };
    format!("--- {from}\n+++ b/{}\n{}", change.path, diff.hunks)
}

/// `review.diff`: one change's unified diff (and, with `full`, both texts).
pub(crate) fn diff(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(id) = crate::params::opt_u64(params, "id")? else {
        return Err(RpcError::invalid_params("`id` (a change id) is required"));
    };
    let full = crate::params::opt_bool(params, "full")?.unwrap_or(false);
    let queue = queue(world);
    let Some(change) = queue.file.changes.iter().find(|c| c.id == id) else {
        return Err(reason_error(
            NOT_FOUND,
            "F0003",
            "review_change_not_found",
            format!("no staged change {id}"),
        ));
    };
    let mut answer = change.summary();
    if let Some(object) = answer.as_object_mut() {
        object.insert("diff".into(), json!(change_diff(change)));
        if full {
            object.insert("before".into(), json!(change.before));
            object.insert("after".into(), json!(change.after));
        }
    }
    Ok(answer)
}

/// The change ids a decision names: `ids`, a `set`, or `all` pending.
fn selected(queue: &ReviewQueue, params: &Value) -> Result<Vec<u64>, RpcError> {
    let ids: Option<Vec<u64>> =
        crate::params::opt_array(params, "ids")?.map(|a| a.iter().filter_map(Value::as_u64).collect());
    let set = crate::params::opt_u64(params, "set")?;
    let all = crate::params::opt_bool(params, "all")?.unwrap_or(false);
    match (ids, set, all) {
        (Some(ids), None, false) if !ids.is_empty() => Ok(ids),
        (None, Some(set), false) => Ok(queue.pending().filter(|c| c.set == set).map(|c| c.id).collect()),
        (None, None, true) => Ok(queue.pending().map(|c| c.id).collect()),
        _ => Err(RpcError::invalid_params(
            "name the changes with exactly one of `ids`, `set` or `all: true`",
        )),
    }
}

/// Decides the selected changes (`review.accept` / `review.reject`); also the editor panel's path.
///
/// # Errors
/// `F0002` when the selection is malformed.
pub fn decide_selected(world: &mut World, params: &Value, accept: bool, by: &str) -> Result<Value, RpcError> {
    let note = crate::params::opt_str(params, "note")?.map(str::to_string);
    let mut queue = queue(world);
    let ids = selected(&queue, params)?;
    let results: Vec<Value> = ids
        .iter()
        .map(|id| queue.decide(*id, accept, by, note.as_deref()))
        .collect();
    let applied = results.iter().filter(|r| r["applied"] == true).count();
    let decided = results.iter().filter(|r| r.get("reason").is_none()).count();
    // Nothing decided (unknown or already decided ids): the queue file stays untouched.
    if decided > 0 {
        queue.prune();
        queue.save();
    }
    Ok(
        json!({ "results": results, "applied": applied, "decided": decided, "pending": queue.pending().count() }),
    )
}

/// The decider name of a protocol call: the session's client.
fn caller(world: &World) -> String {
    identity(world).client
}

/// Who a call is for provenance and ownership: a scoped tool is `tool:<id>` with its own session
/// (never the master agent's, or the tool could withdraw the agent's changes and the agent's
/// changes would carry the tool's work); else the agent session.
fn identity(world: &World) -> crate::safety::AgentSession {
    if let Some(scope) = crate::scope::current_caller(world) {
        return crate::safety::AgentSession {
            client: scope.client_name(),
            model: String::new(),
            session: format!("scope-{}", scope.id),
        };
    }
    world
        .get_resource::<crate::safety::AgentSession>()
        .cloned()
        .unwrap_or_else(|| crate::safety::AgentSession {
            client: "unknown".into(),
            ..Default::default()
        })
}

/// `review.accept`.
pub(crate) fn accept(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let by = caller(world);
    decide_selected(world, params, true, &by)
}

/// Whether the caller holds the `review` permission (its profile, and its tool scope when the
/// call came through a scoped token).
fn may_review(world: &World) -> bool {
    let profile = world
        .get_resource::<crate::safety::AgentPermissions>()
        .is_some_and(|p| p.allows(crate::schema::Permission::Review));
    let scope = crate::scope::current_caller(world)
        .is_none_or(|s| s.allows_permission(crate::schema::Permission::Review));
    profile && scope
}

/// `review.reject`. Without the `review` permission an agent withdraws only its own changes
/// (same client and session); another proposer's change answers `review_not_yours`.
pub(crate) fn reject(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let by = caller(world);
    if may_review(world) {
        return decide_selected(world, params, false, &by);
    }
    let session = identity(world);
    let mine = |c: &StagedChange| c.client == session.client && c.session == session.session;
    let note = crate::params::opt_str(params, "note")?.map(str::to_string);
    let mut queue = queue(world);
    let all_ids = selected(&queue, params)?;
    // `set` and `all` select the caller's own pending changes; named ids answer one by one.
    let named = params.get("ids").is_some();
    let mut results = Vec::new();
    let mut decided = 0;
    for id in all_ids {
        let owned = queue.file.changes.iter().find(|c| c.id == id).map(mine);
        match owned {
            Some(false) if named => results.push(json!({
                "id": id, "applied": false, "reason": "review_not_yours",
                "message": "another agent proposed this change; only the owner rejects it",
            })),
            Some(false) => {}
            _ => {
                let result = queue.decide(id, false, &by, note.as_deref());
                if result.get("reason").is_none() {
                    decided += 1;
                }
                results.push(result);
            }
        }
    }
    if decided > 0 {
        queue.prune();
        queue.save();
    }
    Ok(json!({ "results": results, "applied": 0, "decided": decided, "pending": queue.pending().count() }))
}

/// Fuzz loader (`agent.review_queue`): parse, re-encode canonically.
///
/// # Errors
/// The parse error.
pub fn fuzz_load(text: &str) -> Result<String, String> {
    QueueFile::from_json(text)?.to_json()
}

/// Fuzz fixture: a queue with one pending and one accepted change.
#[must_use]
pub fn fuzz_fixture() -> String {
    let change = |id: u64, status: ChangeStatus| StagedChange {
        id,
        set: 1,
        path: format!("materials/m{id}.material.ron"),
        before: (id == 1).then(|| "(base_color: (1.0, 1.0, 1.0, 1.0))\n".to_string()),
        after: "(base_color: (0.8, 0.2, 0.2, 1.0))\n".into(),
        reason: "warmer".into(),
        title: "warmer".into(),
        client: "claude".into(),
        session: "pid-1".into(),
        model: String::new(),
        images: BTreeMap::new(),
        created_ms: 1_700_000_000_000,
        status,
        decided_ms: None,
        decided_by: None,
        note: None,
        added: 0,
        removed: 0,
        texts_dropped: false,
        golden: (id == 2).then(|| GoldenProposal {
            name: "regia_test_utils:sponza".into(),
            job: "golden:1".into(),
            dir: ".regia/agent/review/goldens/2".into(),
            before_hash: "00".into(),
            after_hash: "11".into(),
            stats: "{\"ssim\":0.9}".into(),
        }),
    };
    QueueFile {
        format_version: REVIEW_QUEUE_FORMAT_VERSION,
        next_id: 2,
        next_set: 1,
        changes: vec![
            change(1, ChangeStatus::Pending),
            change(2, ChangeStatus::Accepted),
        ],
    }
    .to_json()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! Staged-review trust decisions: the diff, the queue file, every state transition, the stale
    //! base, concurrent stages of one path, the audit rows, scoped callers and a restart with
    //! pending stages on disk. Requests go through the real dispatcher over in-process channels.
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::dispatch::{AgentInbox, RateLimit, process_requests, process_scoped_requests};
    use crate::protocol::{Request, Response};
    use crate::safety::{AgentConfig, AgentPermissions, AgentSession, AuditLog};
    use crate::schema::Permission;
    use crate::scope::{ScopedInbound, ScopedInbox, TOOL_SCOPE_DENIED, ToolScope, ToolTokens};
    use crate::server::Inbound;

    const ORIGINAL: &str = "one\ntwo\n";
    const CHANGED: &str = "one\n2\n";

    fn app(root: &Path, permissions: &[&str], client: &str, session: &str) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
                    ..AgentConfig::default()
                },
                source: None,
            })
            .insert_resource(crate::AgentSettings {
                project_dir: Some(root.to_path_buf()),
            })
            .init_resource::<AuditLog>()
            .insert_resource(AgentSession {
                client: client.into(),
                model: "m".into(),
                session: session.into(),
            })
            .init_resource::<crate::playtest::AgentTimeControl>()
            .init_resource::<RateLimit>();
        app
    }

    fn request(method: &str, params: Value) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }

    fn call(app: &mut App, method: &str, params: Value) -> Response {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: request(method, params),
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        process_requests(app.world_mut());
        rx.recv_timeout(Duration::from_secs(5)).expect("answered")
    }

    fn ok(app: &mut App, method: &str, params: Value) -> Value {
        let response = call(app, method, params);
        response
            .result
            .unwrap_or_else(|| panic!("{method}: {:?}", response.error))
    }

    fn reason(response: &Response) -> String {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn error_code(response: &Response) -> String {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn scoped(
        app: &mut App,
        tokens: &ToolTokens,
        scope: &Arc<ToolScope>,
        method: &str,
        params: Value,
    ) -> Response {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(ScopedInbound {
                request: request(method, params),
                reply: tx,
                scope: Arc::clone(scope),
            })
            .expect("queued");
        app.world_mut().insert_resource(ScopedInbox {
            inbox: inbox_rx,
            tokens: tokens.clone(),
        });
        process_scoped_requests(app.world_mut());
        rx.recv_timeout(Duration::from_secs(5)).expect("answered")
    }

    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::write(root.join("a.txt"), ORIGINAL).expect("file");
        root
    }

    fn propose_one(app: &mut App, path: &str, text: &str) -> Value {
        ok(
            app,
            "review.propose",
            json!({ "changes": [{ "path": path, "text": text }], "reason": "r" }),
        )
    }

    fn set_permissions(app: &mut App, permissions: &[&str]) {
        app.world_mut().insert_resource(AgentPermissions {
            config: AgentConfig {
                permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
                ..AgentConfig::default()
            },
            source: None,
        });
    }

    // ----- the diff -------------------------------------------------------------------------

    #[test]
    fn identical_texts_have_an_empty_diff() {
        let diff = line_diff("a\nb\n", "a\nb\n");
        assert_eq!(diff.hunks, "");
        assert_eq!((diff.added, diff.removed), (0, 0));
    }

    /// `str::lines` treats a final newline as optional and strips `\r`, so a review
    /// whose only edit is the line ending used to stage a change with an empty diff.
    #[test]
    fn a_final_newline_or_crlf_change_is_visible() {
        let dropped = line_diff("hello\n", "hello");
        assert!(
            dropped.hunks.contains("No newline"),
            "dropping the final newline must show in the diff: {dropped:?}"
        );
        assert!(dropped.added + dropped.removed > 0, "{dropped:?}");

        let gained = line_diff("hello", "hello\n");
        assert!(
            gained.hunks.contains("No newline"),
            "gaining the final newline must show in the diff: {gained:?}"
        );
        assert!(gained.added + gained.removed > 0, "{gained:?}");

        // corr-agent (ADR-5597): the hunk sits at the last line and replaces it; it named line 1
        // and a removed line whatever the file.
        let dropped = line_diff("one\ntwo\n", "one\ntwo");
        assert_eq!(
            dropped.hunks, "@@ -2,1 +2,1 @@\n-two\n+two\n\\ No newline at end of file\n",
            "{dropped:?}"
        );
        assert_eq!((dropped.added, dropped.removed), (1, 1));
        let gained = line_diff("one\ntwo", "one\ntwo\n");
        assert_eq!(
            gained.hunks, "@@ -2,1 +2,1 @@\n-two\n\\ No newline at end of file\n+two\n",
            "{gained:?}"
        );

        let crlf = line_diff("hello\r\n", "hello\n");
        assert!(
            crlf.hunks.contains('\r'),
            "CRLF versus LF must show in the diff: {crlf:?}"
        );
        assert_eq!((crlf.added, crlf.removed), (1, 1), "{crlf:?}");

        let blank = line_diff("\n", "");
        assert_ne!(
            blank.hunks, "",
            "a lone newline versus empty must diff: {blank:?}"
        );
        assert!(blank.added + blank.removed > 0, "{blank:?}");
    }

    #[test]
    fn a_new_file_is_all_additions_and_a_cleared_file_all_removals() {
        let created = line_diff("", "x\ny\n");
        assert_eq!(created.hunks, "@@ -0,0 +1,2 @@\n+x\n+y\n");
        assert_eq!((created.added, created.removed), (2, 0));
        let cleared = line_diff("x\ny\n", "");
        assert_eq!(cleared.hunks, "@@ -1,2 +0,0 @@\n-x\n-y\n");
        assert_eq!((cleared.added, cleared.removed), (0, 2));
    }

    #[test]
    fn a_middle_change_shows_three_lines_of_context_and_exact_ranges() {
        let a: String = (1..=10).map(|n| format!("l{n}\n")).collect();
        let b = a.replace("l5\n", "L5\n");
        let diff = line_diff(&a, &b);
        assert_eq!(
            diff.hunks,
            "@@ -2,7 +2,7 @@\n l2\n l3\n l4\n-l5\n+L5\n l6\n l7\n l8\n"
        );
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    #[test]
    fn near_changes_share_a_hunk_and_far_changes_split() {
        let a: String = (1..=30).map(|n| format!("l{n}\n")).collect();
        let near = a.replace("l2\n", "X\n").replace("l6\n", "Y\n");
        assert_eq!(line_diff(&a, &near).hunks.matches("@@ -").count(), 1);
        let far = a.replace("l2\n", "X\n").replace("l29\n", "Y\n");
        let far = line_diff(&a, &far);
        assert_eq!(far.hunks.matches("@@ -").count(), 2, "{}", far.hunks);
        assert_eq!((far.added, far.removed), (2, 2));
    }

    #[test]
    fn a_middle_beyond_the_cell_bound_is_one_replaced_block() {
        let n = 2100; // (n + 1)^2 > MAX_DIFF_CELLS
        let a: String = (0..n).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..n).map(|i| format!("b{i}\n")).collect();
        let diff = line_diff(&a, &b);
        assert_eq!((diff.added, diff.removed), (n, n));
        assert_eq!(diff.hunks.matches("@@ -").count(), 1);
        assert!(diff.hunks.starts_with(&format!("@@ -1,{n} +1,{n} @@\n-a0\n")));
    }

    #[test]
    fn change_diff_names_a_created_file_dev_null() {
        let mut change = StagedChange {
            id: 1,
            set: 1,
            path: "n/x.txt".into(),
            before: None,
            after: "hi\n".into(),
            reason: String::new(),
            title: String::new(),
            client: String::new(),
            session: String::new(),
            model: String::new(),
            images: BTreeMap::new(),
            created_ms: 0,
            status: ChangeStatus::Pending,
            decided_ms: None,
            decided_by: None,
            note: None,
            added: 0,
            removed: 0,
            texts_dropped: false,
            golden: None,
        };
        assert_eq!(
            change_diff(&change),
            "--- /dev/null\n+++ b/n/x.txt\n@@ -0,0 +1,1 @@\n+hi\n"
        );
        assert_eq!(change.kind(), "create");
        change.before = Some(String::new());
        assert_eq!(change.kind(), "modify");
        assert!(change_diff(&change).starts_with("--- a/n/x.txt\n"));
    }

    #[test]
    fn unix_ms_become_rfc3339_across_leap_days_and_eras() {
        assert_eq!(rfc3339_from_unix_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix_ms(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix_ms(1_700_000_000_999), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_from_unix_ms(4_102_444_799_000), "2099-12-31T23:59:59Z");
    }

    // ----- the queue file -------------------------------------------------------------------

    #[test]
    fn the_queue_file_refuses_version_zero_newer_versions_and_garbage() {
        let file = |v: u32| format!(r#"{{"format_version":{v},"next_id":0,"next_set":0,"changes":[]}}"#);
        assert!(QueueFile::from_json(&file(1)).is_ok());
        assert!(QueueFile::from_json(&file(0)).is_err());
        let newer = REVIEW_QUEUE_FORMAT_VERSION + 1;
        assert!(
            QueueFile::from_json(&file(newer))
                .expect_err("newer")
                .contains(&format!("format_version {newer}"))
        );
        assert!(QueueFile::from_json("not json").is_err());
        let fixture = QueueFile::from_json(&fuzz_fixture()).expect("fixture parses");
        assert_eq!(
            QueueFile::from_json(&fixture.to_json().expect("json")),
            Ok(fixture)
        );
    }

    #[test]
    fn loading_an_absent_queue_is_empty_and_a_bad_one_is_kept_on_disk() {
        let root = project("review-load");
        let queue = ReviewQueue::load(&root);
        assert!(queue.last_error.is_none());
        assert_eq!(queue.pending().count(), 0);
        assert_eq!(queue.file.format_version, REVIEW_QUEUE_FORMAT_VERSION);
        let path = queue_path(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "{ not json").expect("bad queue");
        let queue = ReviewQueue::load(&root);
        assert!(queue.last_error.is_some());
        assert_eq!(queue.pending().count(), 0);
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "{ not json",
            "loading never rewrites the bad file"
        );
        // A newer format is reported, not silently reset.
        std::fs::write(
            &path,
            r#"{"format_version":9,"next_id":0,"next_set":0,"changes":[]}"#,
        )
        .expect("newer");
        assert!(
            ReviewQueue::load(&root)
                .last_error
                .expect("error")
                .contains("format_version 9")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ----- transitions ------------------------------------------------------------------------

    #[test]
    fn pending_stages_survive_a_restart_and_ids_are_never_reused() {
        let root = project("review-restart");
        let mut first = app(&root, &["read", "assets"], "agent", "s1");
        propose_one(&mut first, "a.txt", CHANGED);
        propose_one(&mut first, "new.txt", "n\n");
        let before: Value = ok(&mut first, "review.list", json!({}));
        drop(first);

        let mut second = app(&root, &["read", "assets", "review"], "owner-ui", "s2");
        let after = ok(&mut second, "review.list", json!({}));
        assert_eq!(after["pending"], 2);
        assert_eq!(
            after["changes"], before["changes"],
            "the same rows after a restart"
        );
        // The diff of a staged change is available after the restart.
        let diff = ok(&mut second, "review.diff", json!({ "id": 1, "full": true }));
        assert_eq!(diff["before"], ORIGINAL);
        assert_eq!(diff["after"], CHANGED);
        // Ids and sets continue; the old rows keep theirs.
        let third = propose_one(&mut second, "c.txt", "c\n");
        assert_eq!(third["changes"][0]["id"], 3);
        assert_eq!(third["set"], 3);
        let accepted = ok(&mut second, "review.accept", json!({ "ids": [1] }));
        assert_eq!(accepted["applied"], 1);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), CHANGED);
        // A decided row stays decided across another restart; a decided id is not reusable.
        drop(second);
        let mut third_app = app(&root, &["read", "assets", "review"], "owner-ui", "s3");
        let all = ok(&mut third_app, "review.list", json!({ "status": "all" }));
        let rows = all["changes"].as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["status"], "accepted");
        assert_eq!(rows[0]["decided_by"], "owner-ui");
        let again = ok(&mut third_app, "review.accept", json!({ "ids": [1] }));
        assert_eq!(again["results"][0]["reason"], "review_already_decided");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_transition_of_a_staged_change() {
        let root = project("review-transitions");
        let mut app = app(&root, &["read", "assets", "review"], "owner", "s1");
        propose_one(&mut app, "a.txt", CHANGED); // 1
        propose_one(&mut app, "sub/dir/b.txt", "b\n"); // 2
        propose_one(&mut app, "a2.txt", "x\n"); // 3
        // reject: the file is untouched, the row records who and why.
        let rejected = ok(&mut app, "review.reject", json!({ "ids": [1], "note": "no" }));
        assert_eq!(rejected["results"][0]["status"], "rejected");
        assert_eq!(rejected["results"][0]["applied"], false);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), ORIGINAL);
        // accept: creates parent folders; there is no .meta, so nothing to stamp.
        let accepted = ok(&mut app, "review.accept", json!({ "ids": [2] }));
        assert_eq!(accepted["results"][0]["meta"], "no_meta");
        assert_eq!(
            std::fs::read_to_string(root.join("sub/dir/b.txt")).expect("b"),
            "b\n"
        );
        // decided rows cannot be decided again, either way.
        for (method, id) in [("review.accept", 1), ("review.reject", 2), ("review.accept", 2)] {
            let again = ok(&mut app, method, json!({ "ids": [id] }));
            assert_eq!(
                again["results"][0]["reason"], "review_already_decided",
                "{method} {id}"
            );
            assert_eq!(again["decided"], 0);
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("a"),
            ORIGINAL,
            "a rejected change is never applied by a later accept"
        );
        // unknown id.
        let missing = ok(&mut app, "review.accept", json!({ "ids": [99] }));
        assert_eq!(missing["results"][0]["reason"], "review_change_not_found");
        // the rows.
        let all = ok(&mut app, "review.list", json!({ "status": "all" }));
        let statuses: Vec<_> = all["changes"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|c| c["status"].clone())
            .collect();
        assert_eq!(statuses, [json!("rejected"), json!("accepted"), json!("pending")]);
        assert_eq!(all["pending"], 1);
        assert_eq!(all["changes"][0]["note"], "no");
        for filter in ["pending", "accepted", "rejected"] {
            let one = ok(&mut app, "review.list", json!({ "status": filter }));
            assert_eq!(one["changes"].as_array().expect("rows").len(), 1, "{filter}");
        }
        assert_eq!(
            ok(&mut app, "review.list", json!({ "set": 3, "status": "all" }))["changes"][0]["id"],
            3
        );
        let bad = call(&mut app, "review.list", json!({ "status": "bogus" }));
        assert!(bad.error.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stale_base_conflicts_and_leaves_the_change_pending_and_the_file_alone() {
        let root = project("review-stale");
        let mut app = app(&root, &["read", "assets", "review"], "owner", "s1");
        propose_one(&mut app, "a.txt", CHANGED);
        propose_one(&mut app, "fresh.txt", "new\n");
        // The file moved on after staging; a file the proposal meant to create appeared.
        std::fs::write(root.join("a.txt"), "someone else\n").expect("edit");
        std::fs::write(root.join("fresh.txt"), "already\n").expect("create");
        let result = ok(&mut app, "review.accept", json!({ "all": true }));
        assert_eq!(result["applied"], 0);
        assert_eq!(result["decided"], 0);
        for row in result["results"].as_array().expect("rows") {
            assert_eq!(row["reason"], "review_conflict", "{row}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("a"),
            "someone else\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).expect("f"),
            "already\n"
        );
        assert_eq!(ok(&mut app, "review.list", json!({}))["pending"], 2);
        // A file removed after staging also conflicts (the text is no longer what was proposed over).
        std::fs::remove_file(root.join("a.txt")).expect("rm");
        let gone = ok(&mut app, "review.accept", json!({ "ids": [1] }));
        assert_eq!(gone["results"][0]["reason"], "review_conflict");
        assert!(!root.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_stages_of_one_path_apply_once_and_the_second_conflicts() {
        let root = project("review-concurrent");
        let mut agent = app(&root, &["read", "assets"], "agent", "s1");
        propose_one(&mut agent, "a.txt", "first\n");
        agent.world_mut().insert_resource(AgentSession {
            client: "other".into(),
            model: String::new(),
            session: "s2".into(),
        });
        propose_one(&mut agent, "a.txt", "second\n");
        let mut owner = app(&root, &["read", "assets", "review"], "owner", "o");
        let result = ok(&mut owner, "review.accept", json!({ "all": true }));
        assert_eq!(result["applied"], 1, "{result}");
        assert_eq!(result["results"][0]["applied"], true);
        assert_eq!(result["results"][1]["reason"], "review_conflict");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), "first\n");
        // The loser is still pending and can be rejected.
        assert_eq!(ok(&mut owner, "review.list", json!({}))["pending"], 1);
        assert_eq!(
            ok(&mut owner, "review.reject", json!({ "ids": [2] }))["decided"],
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_proposal_dry_run_unchanged_text_and_duplicates() {
        let root = project("review-propose");
        let mut agent = app(&root, &["read", "assets"], "agent", "s1");
        let dry = ok(
            &mut agent,
            "review.propose",
            json!({ "changes": [{ "path": "a.txt", "text": CHANGED }], "reason": "r", "dry_run": true }),
        );
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["changes"][0]["staged"], false);
        assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 0);
        assert!(!queue_path(&root).exists(), "a dry run writes nothing");
        // Unchanged text is not staged and takes no set id.
        let same = propose_one(&mut agent, "a.txt", ORIGINAL);
        assert_eq!(same["changes"][0]["kind"], "unchanged");
        assert!(same["set"].is_null());
        assert_eq!(propose_one(&mut agent, "a.txt", CHANGED)["set"], 1);
        // One path twice in a proposal, even spelled with a backslash, is refused whole.
        let twice = call(
            &mut agent,
            "review.propose",
            json!({ "changes": [{ "path": "d/x.txt", "text": "1" }, { "path": "d\\x.txt", "text": "2" }], "reason": "r" }),
        );
        assert!(twice.error.is_some());
        assert_eq!(
            ok(&mut agent, "review.list", json!({}))["pending"],
            1,
            "nothing of the refused set staged"
        );
        // A blank reason, no changes, and too many changes.
        for params in [
            json!({ "changes": [{ "path": "x", "text": "y" }], "reason": "   " }),
            json!({ "changes": [], "reason": "r" }),
            json!({ "reason": "r" }),
            json!({ "changes": [{ "path": "x" }], "reason": "r" }),
            json!({ "changes": (0..=MAX_CHANGES_PER_PROPOSAL).map(|n| json!({ "path": format!("f{n}"), "text": "t" })).collect::<Vec<_>>(), "reason": "r" }),
            json!({ "changes": [{ "path": "x", "text": "y" }], "reason": "r", "images": { "sideways": "a.png" } }),
        ] {
            assert!(
                call(&mut agent, "review.propose", params.clone()).error.is_some(),
                "{params}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_pending_limit_refuses_a_proposal_that_would_pass_it() {
        let root = project("review-full");
        let mut agent = app(&root, &["read", "assets"], "agent", "s1");
        for batch in 0..MAX_PENDING / MAX_CHANGES_PER_PROPOSAL {
            let changes: Vec<Value> = (0..MAX_CHANGES_PER_PROPOSAL)
                .map(|n| json!({ "path": format!("f{batch}_{n}.txt"), "text": "t" }))
                .collect();
            ok(
                &mut agent,
                "review.propose",
                json!({ "changes": changes, "reason": "r" }),
            );
        }
        assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], MAX_PENDING);
        let over = call(
            &mut agent,
            "review.propose",
            json!({ "changes": [{ "path": "one-more.txt", "text": "t" }], "reason": "r" }),
        );
        assert_eq!(reason(&over), "review_queue_full");
        // A no-op proposal (unchanged text) still passes: it stages nothing.
        let same = propose_one(&mut agent, "a.txt", ORIGINAL);
        assert_eq!(same["changes"][0]["kind"], "unchanged");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_selection_names_exactly_one_of_ids_set_or_all() {
        let root = project("review-select");
        let mut owner = app(&root, &["read", "assets", "review"], "owner", "s1");
        propose_one(&mut owner, "a.txt", CHANGED);
        for params in [
            json!({}),
            json!({ "ids": [] }),
            json!({ "ids": ["1"] }),
            json!({ "ids": [1], "all": true }),
            json!({ "ids": [1], "set": 1 }),
            json!({ "set": 1, "all": true }),
            json!({ "all": false }),
        ] {
            for method in ["review.accept", "review.reject"] {
                assert!(
                    call(&mut owner, method, params.clone()).error.is_some(),
                    "{method} {params}"
                );
            }
        }
        assert_eq!(
            ok(&mut owner, "review.list", json!({}))["pending"],
            1,
            "nothing was decided"
        );
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), ORIGINAL);
        // `set` with no pending changes decides nothing and is not an error.
        assert_eq!(
            ok(&mut owner, "review.accept", json!({ "set": 77 }))["decided"],
            0
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_planted_queue_row_cannot_write_outside_the_project() {
        // A hand-edited queue file (the agent can write other project files, not this one, but
        // the queue is data) must not turn `review.accept` into an arbitrary write.
        let root = project("review-planted");
        let outside = regia_test_utils::unique_temp_dir("review-planted-outside");
        let mut owner = app(&root, &["read", "assets", "review"], "owner", "s1");
        propose_one(&mut owner, "a.txt", CHANGED);
        let victim = outside.join("victim.txt");
        {
            let mut queue = queue(owner.world_mut());
            let mut row = queue.file.changes[0].clone();
            row.id = 50;
            row.path = format!(
                "../{}/victim.txt",
                outside.file_name().and_then(|n| n.to_str()).unwrap_or("x")
            );
            row.before = None;
            queue.file.changes.push(row);
            let mut row = queue.file.changes[0].clone();
            row.id = 51;
            row.path = "/etc/regia-planted.txt".into();
            row.before = None;
            queue.file.changes.push(row);
        }
        let result = ok(&mut owner, "review.accept", json!({ "ids": [50, 51] }));
        assert_eq!(result["applied"], 0, "{result}");
        assert_eq!(result["results"][0]["reason"], "review_path_denied");
        assert_eq!(result["results"][1]["reason"], "review_path_denied");
        assert!(!victim.exists());
        assert!(!Path::new("/etc/regia-planted.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // ----- who may decide ---------------------------------------------------------------------

    #[test]
    fn the_proposing_client_cannot_approve_its_own_change_without_the_review_permission() {
        let root = project("review-self");
        let mut agent = app(
            &root,
            &["read", "assets", "edit_scene", "play", "tools"],
            "agent",
            "s1",
        );
        propose_one(&mut agent, "a.txt", CHANGED);
        for params in [json!({ "ids": [1] }), json!({ "set": 1 }), json!({ "all": true })] {
            let response = call(&mut agent, "review.accept", params);
            assert_eq!(error_code(&response), "F0012");
        }
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), ORIGINAL);
        assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 1);
        // `review.reject` is a withdrawal, never an application.
        let own = ok(&mut agent, "review.reject", json!({ "ids": [1] }));
        assert_eq!(own["decided"], 1);
        assert_eq!(own["applied"], 0);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), ORIGINAL);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_same_client_new_session_is_a_different_proposer() {
        let root = project("review-session");
        let mut agent = app(&root, &["read", "assets"], "agent", "s1");
        propose_one(&mut agent, "a.txt", CHANGED);
        agent.world_mut().insert_resource(AgentSession {
            client: "agent".into(),
            model: "m".into(),
            session: "s2".into(),
        });
        let by_id = ok(&mut agent, "review.reject", json!({ "ids": [1] }));
        assert_eq!(by_id["results"][0]["reason"], "review_not_yours");
        // And a different client in the same session id is a different proposer too.
        agent.world_mut().insert_resource(AgentSession {
            client: "intruder".into(),
            model: "m".into(),
            session: "s1".into(),
        });
        assert_eq!(
            ok(&mut agent, "review.reject", json!({ "ids": [1] }))["decided"],
            0
        );
        assert_eq!(ok(&mut agent, "review.reject", json!({ "set": 1 }))["decided"], 0);
        assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_scoped_tool_never_approves_and_never_withdraws_anothers_change() {
        let root = project("review-scoped");
        // The project profile holds `review`; the tool's token still must not use it.
        let mut app = app(&root, &["read", "assets", "review"], "agent", "s1");
        propose_one(&mut app, "a.txt", CHANGED);
        let tokens = ToolTokens::new();
        let scope = tokens
            .issue(ToolScope::new(
                "fixture",
                [Permission::Read, Permission::Assets, Permission::Review],
                vec!["review.*".to_string()],
            ))
            .1;
        let accept = scoped(&mut app, &tokens, &scope, "review.accept", json!({ "all": true }));
        assert_eq!(error_code(&accept), TOOL_SCOPE_DENIED);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), ORIGINAL);
        // It can read the queue, but a withdrawal of the master agent's change is refused.
        let listed = scoped(&mut app, &tokens, &scope, "review.list", json!({}));
        assert_eq!(
            listed.result.as_ref().map(|r| r["pending"].clone()),
            Some(json!(1))
        );
        let reject = scoped(&mut app, &tokens, &scope, "review.reject", json!({ "ids": [1] }));
        let result = reject.result.expect("answered");
        assert_eq!(result["decided"], 0, "{result}");
        assert_eq!(result["results"][0]["reason"], "review_not_yours");
        assert_eq!(ok(&mut app, "review.list", json!({}))["pending"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_scoped_tools_stage_is_attributed_to_the_tool_not_the_agent_session() {
        let root = project("review-scoped-who");
        let mut app = app(&root, &["read", "assets", "review"], "agent", "s1");
        let tokens = ToolTokens::new();
        let scope = tokens
            .issue(ToolScope::new(
                "fixture",
                [Permission::Read, Permission::Assets],
                vec!["review.*".to_string()],
            ))
            .1;
        let staged = scoped(
            &mut app,
            &tokens,
            &scope,
            "review.propose",
            json!({ "changes": [{ "path": "a.txt", "text": CHANGED }], "reason": "tool edit" }),
        );
        assert!(staged.error.is_none(), "{:?}", staged.error);
        let diff = ok(&mut app, "review.diff", json!({ "id": 1 }));
        assert_eq!(diff["client"], "tool:fixture", "provenance names the tool");
        // So the master agent session is not "the proposer" and the tool cannot pose as it.
        let tool_reject = scoped(&mut app, &tokens, &scope, "review.reject", json!({ "ids": [1] }));
        assert_eq!(
            tool_reject.result.expect("answered")["decided"],
            1,
            "its own change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ----- audit ------------------------------------------------------------------------------

    #[test]
    fn each_review_step_writes_one_audit_row() {
        let root = project("review-audit");
        let log = root.join("audit.jsonl");
        let mut app = app(&root, &["read", "assets"], "agent", "s1");
        app.world_mut().insert_resource(AuditLog::open(log.clone()));
        propose_one(&mut app, "a.txt", CHANGED);
        ok(&mut app, "review.list", json!({}));
        ok(&mut app, "review.diff", json!({ "id": 1 }));
        assert!(
            call(&mut app, "review.accept", json!({ "ids": [1] }))
                .error
                .is_some()
        );
        set_permissions(&mut app, &["read", "assets", "review"]);
        app.world_mut().insert_resource(AgentSession {
            client: "owner".into(),
            model: String::new(),
            session: "o".into(),
        });
        ok(&mut app, "review.accept", json!({ "ids": [1] }));
        ok(&mut app, "review.reject", json!({ "ids": [1], "note": "late" }));
        let text = std::fs::read_to_string(&log).expect("audit log");
        let rows: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("row"))
            .collect();
        let view: Vec<(String, String, Option<String>, String)> = rows
            .iter()
            .map(|r| {
                (
                    r["method"].as_str().unwrap_or_default().to_string(),
                    r["outcome"].as_str().unwrap_or_default().to_string(),
                    r["error_code"].as_str().map(str::to_string),
                    r["client"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let row = |m: &str, o: &str, c: Option<&str>, who: &str| {
            (
                m.to_string(),
                o.to_string(),
                c.map(str::to_string),
                who.to_string(),
            )
        };
        assert_eq!(
            view,
            [
                row("review.propose", "ok", None, "agent"),
                row("review.list", "ok", None, "agent"),
                row("review.diff", "ok", None, "agent"),
                row("review.accept", "error", Some("F0012"), "agent"),
                row("review.accept", "ok", None, "owner"),
                row("review.reject", "ok", None, "owner"),
            ]
        );
        let seqs: Vec<u64> = rows.iter().filter_map(|r| r["seq"].as_u64()).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
        // The refused and the applied accept carry the same params, so the same hash.
        assert_eq!(rows[3]["params_hash"], rows[4]["params_hash"]);
        assert_ne!(
            rows[4]["params_hash"],
            rows[5]["params_hash"].clone(),
            "reject has other params"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
