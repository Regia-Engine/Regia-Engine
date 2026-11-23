// SPDX-License-Identifier: MPL-2.0
//! The incremental report outbox (plan row NL-08, ADR-8402): uploads the new tail of each log since
//! the last confirmed byte, in capped parts, and resends a crashed session's log once.
//!
//! The ledger (`outbox.ron`, local state, one per install, next to the logs by default) holds one
//! row per source file: its path, its identity (size, modification time and a hash of the first
//! bytes, so a log rotated under the same name starts a new row) and the last offset the server
//! confirmed. A flush reads the bytes from that offset, cuts them into parts of at most
//! [`OutboxSettings::part_cap`] bytes (after scrubbing), and sends each part through an
//! [`UploadTransport`]. The offset moves only after the transport reports a 2xx and the ledger is
//! written again, so an interrupted or refused part is sent again on the next flush and no byte is
//! ever skipped (a part can arrive twice, never not at all).
//!
//! A crash marker (`crash-*.txt`, written by `regia_platform::crash`) sets that session's log back
//! to byte 0 once: the whole log of the session that crashed is sent again, its parts named with a
//! `;crash` suffix. The marker's name is kept in the ledger, so the same crash never resends twice.
//!
//! Uploading follows [`CrashReporter`]'s rules unchanged: nothing is read, sent or written while
//! [`CrashReporter::enabled`] is false (no endpoint, no consent, or a remote `http://` endpoint
//! without `allow_cleartext`), and every part goes through [`scrub_upload`].
//!
//! Parts are cut after the last newline that fits, so a scrubbed pattern (an email, a secret) is not
//! split across two parts; a line longer than the cap is cut on a UTF-8 character boundary. The
//! live session's unfinished last line is held until it ends (see [`OutboxSources::live`]).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::reporter::{CrashReporter, header_safe_name, scrub_upload};
use crate::transport::UploadTransport;

/// Default part cap: 256 KiB of scrubbed body per part.
pub const DEFAULT_PART_CAP: u64 = 256 * 1024;
/// The smallest part cap a setting can ask for; smaller values are raised to it.
pub const MIN_PART_CAP: u64 = 4096;
/// Default number of ledger rows kept.
pub const DEFAULT_MAX_ROWS: usize = 64;
/// Default number of parts one flush sends at most (16 MiB at the default cap).
pub const DEFAULT_MAX_PARTS_PER_FLUSH: usize = 64;
/// The ledger's file name in the logs directory.
pub const LEDGER_FILE: &str = "outbox.ron";
/// The ledger's current format version.
pub const LEDGER_VERSION: u32 = 1;
/// Bytes at the start of a file whose hash identifies it.
const HEADER_BYTES: u64 = 4096;
/// Crash marker names kept in the ledger (the platform keeps far fewer crash reports).
const MAX_CRASH_MARKERS: usize = 256;
/// The longest error message kept, in characters.
const MAX_ERROR_CHARS: usize = 200;
/// The most error messages one [`OutboxFlush`] keeps.
const MAX_FLUSH_ERRORS: usize = 16;

/// Outbox settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxSettings {
    /// Most bytes of scrubbed body in one part ([`DEFAULT_PART_CAP`]; raised to [`MIN_PART_CAP`]).
    pub part_cap: u64,
    /// Ledger rows kept; older rows age out by count ([`DEFAULT_MAX_ROWS`], at least 1).
    pub max_rows: usize,
    /// Parts one flush sends at most; the rest waits for the next flush.
    pub max_parts_per_flush: usize,
    /// The ledger file; `None` is `<logs dir>/outbox.ron`.
    pub ledger: Option<PathBuf>,
}

impl Default for OutboxSettings {
    fn default() -> Self {
        Self {
            part_cap: DEFAULT_PART_CAP,
            max_rows: DEFAULT_MAX_ROWS,
            max_parts_per_flush: DEFAULT_MAX_PARTS_PER_FLUSH,
            ledger: None,
        }
    }
}

impl OutboxSettings {
    /// The cap a flush uses: [`part_cap`](Self::part_cap), at least [`MIN_PART_CAP`].
    #[must_use]
    pub fn effective_part_cap(&self) -> u64 {
        self.part_cap.max(MIN_PART_CAP)
    }

    /// The ledger path: [`ledger`](Self::ledger), else [`LEDGER_FILE`] in `logs_dir`.
    #[must_use]
    pub fn ledger_path(&self, logs_dir: Option<&Path>) -> Option<PathBuf> {
        self.ledger
            .clone()
            .or_else(|| logs_dir.map(|dir| dir.join(LEDGER_FILE)))
    }
}

/// What one flush looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxSources {
    /// The log files to upload incrementally. A missing file is skipped.
    pub logs: Vec<PathBuf>,
    /// The directory crash markers (`crash-*.txt`, also quarantined `*.rejected` ones) are
    /// written to; `None` turns the crash resend off.
    pub crash_dir: Option<PathBuf>,
    /// The log the running session is still writing: its unfinished last line is held until it
    /// ends. `None` sends every byte up to the end of each file.
    pub live: Option<PathBuf>,
}

impl OutboxSources {
    /// The session logs (`*.log`, `*.jsonl`) directly in `logs_dir`, sorted by name, with
    /// `logs_dir` as the crash directory (the platform writes crash reports there) and the newest
    /// start-time-named log (`YYYY-MM-DD_HH-MM-SS[_N].log`) as the live one.
    #[must_use]
    pub fn from_logs_dir(logs_dir: &Path) -> Self {
        let mut logs: Vec<PathBuf> = std::fs::read_dir(logs_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        (name.ends_with(".log") || name.ends_with(".jsonl"))
                            && entry.file_type().is_ok_and(|t| t.is_file())
                    })
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        logs.sort();
        // The engine's file sink names each session log by its start time, and the running
        // session's is the newest.
        let live = logs
            .iter()
            .filter_map(|log| Some((start_time(log.file_stem()?.to_str()?)?, log)))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, log)| log.clone());
        Self {
            logs,
            crash_dir: Some(logs_dir.to_path_buf()),
            live,
        }
    }
}

/// Which file a ledger row describes: a log rotated under the same name gets a new row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    /// Size when last looked at, in bytes.
    pub size: u64,
    /// Modification time when last looked at, milliseconds since the Unix epoch (0 unknown).
    pub modified_unix_ms: u64,
    /// How many leading bytes `header_hash` covers (at most 4096).
    pub header_len: u64,
    /// 64-bit FNV-1a of the first `header_len` bytes.
    pub header_hash: u64,
}

/// One source file in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRow {
    /// The file's host path.
    pub path: PathBuf,
    /// The file the row describes.
    pub identity: FileIdentity,
    /// Bytes from the start the server confirmed: the next part starts here.
    pub confirmed: u64,
    /// When the row was last touched, in ledger order (larger is newer; no clock).
    pub seq: u64,
    /// The file at the path was replaced (rotated); a newer row describes it. Kept until it ages
    /// out.
    #[serde(default)]
    pub retired: bool,
    /// The crash marker whose full resend of this log is under way.
    #[serde(default)]
    pub crash_resend: Option<String>,
    /// Why the last part of this file was not confirmed; cleared by the next confirmed part.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// The outbox ledger (`outbox.ron`, version [`LEDGER_VERSION`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Format version.
    pub version: u32,
    /// The next row `seq`.
    pub next_seq: u64,
    /// Rows, oldest first.
    pub rows: Vec<LedgerRow>,
    /// Crash markers already acted on, so each forces one resend only.
    pub crash_markers: Vec<String>,
    /// The most recent error of any flush (a part, a source or the ledger write).
    #[serde(default)]
    pub last_error: Option<String>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            next_seq: 0,
            rows: Vec::new(),
            crash_markers: Vec::new(),
            last_error: None,
        }
    }
}

/// What one [`Outbox::flush`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxFlush {
    /// Nothing was read or sent: uploads are off ([`CrashReporter::enabled`] is false).
    pub disabled: bool,
    /// Parts the server confirmed.
    pub parts_sent: u64,
    /// Source bytes those parts covered (before scrubbing).
    pub bytes_confirmed: u64,
    /// Parts that were sent but not confirmed (transport error or non-2xx); they are sent again.
    pub failed: u64,
    /// Sources found rotated (a new ledger row was started).
    pub rotated: u64,
    /// New crash markers that set a session log back to byte 0.
    pub crash_resends: u64,
    /// One message per failure, at most 16 of 200 characters.
    pub errors: Vec<String>,
}

impl OutboxFlush {
    fn push_error(&mut self, message: String) {
        if self.errors.len() < MAX_FLUSH_ERRORS {
            self.errors.push(clip(message));
        }
    }
}

/// One source in [`OutboxStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    /// The file's host path.
    pub path: PathBuf,
    /// Its size now (0 when it is missing).
    pub bytes: u64,
    /// Bytes the server confirmed (0 for a file the ledger has no row for, or a rotated one).
    pub confirmed: u64,
    /// Bytes still to send: `bytes - confirmed`.
    pub pending_bytes: u64,
    /// The ledger has a live row for this file.
    pub tracked: bool,
    /// A crash resend of this file is under way.
    pub crash_resend: bool,
    /// Why its last part was not confirmed.
    pub last_error: Option<String>,
}

/// The outbox's state for a source list, read without sending anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxStatus {
    /// The ledger file.
    pub ledger: PathBuf,
    /// Rows in the ledger.
    pub rows: usize,
    /// Pending bytes over every source.
    pub pending_bytes: u64,
    /// Per source, in the order given.
    pub sources: Vec<SourceStatus>,
    /// The most recent error of any flush.
    pub last_error: Option<String>,
    /// Why the ledger on disk could not be read (it was started over).
    pub load_error: Option<String>,
}

/// The outbox: a ledger file and the settings it is flushed with.
#[derive(Debug, Clone)]
pub struct Outbox {
    path: PathBuf,
    settings: OutboxSettings,
    ledger: Ledger,
    load_error: Option<String>,
}

impl Outbox {
    /// Opens the ledger at `ledger_path`. A missing file is an empty ledger. A file that cannot be
    /// read, does not parse or has a newer version also starts empty (its sources are sent again
    /// from byte 0: duplicates, never gaps) and [`load_error`](Self::load_error) says why; the file
    /// is replaced on the next confirmed part.
    #[must_use]
    pub fn open(ledger_path: impl Into<PathBuf>, settings: OutboxSettings) -> Self {
        let path = ledger_path.into();
        let (ledger, load_error) = match load(&path) {
            Ok(ledger) => (ledger, None),
            Err(error) => {
                tracing::warn!(target: "regia_crash", "outbox ledger started over: {error}");
                (Ledger::default(), Some(error))
            }
        };
        Self {
            path,
            settings,
            ledger,
            load_error,
        }
    }

    /// The ledger as loaded and updated.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The ledger file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &OutboxSettings {
        &self.settings
    }

    /// Why the ledger on disk was not used, if it was not.
    #[must_use]
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Sends what is new in `sources` through `transport`, if `reporter` allows uploads.
    ///
    /// First every crash marker not seen before sets its session's log back to byte 0 (the
    /// session log is the one whose start-time name is the latest at or before the marker's).
    /// Then each log is checked against its row (a rotated file starts a new row) and its bytes
    /// from the confirmed offset go out in capped, scrubbed parts. A part that is not confirmed
    /// stops that source for this flush (later parts would leave a gap) and is sent again next
    /// time. The ledger is written after every confirmed part, then rows age out by count.
    pub fn flush(
        &mut self,
        reporter: &CrashReporter,
        sources: &OutboxSources,
        transport: &mut dyn UploadTransport,
    ) -> OutboxFlush {
        let _span = tracing::debug_span!("report_outbox_flush").entered();
        let mut out = OutboxFlush::default();
        if !reporter.enabled() {
            out.disabled = true;
            return out;
        }
        let mut dirty = self.apply_crash_markers(sources, &mut out);
        let cap = self.settings.effective_part_cap();
        let mut budget = self.settings.max_parts_per_flush;
        for path in &sources.logs {
            let mut observed = match observe(path) {
                Ok(Some(observed)) => observed,
                Ok(None) => continue,
                Err(error) => {
                    out.push_error(format!("{}: {error}", file_label(path)));
                    continue;
                }
            };
            let (index, changed) = self.live_row(path, &observed, &mut out);
            dirty |= changed;
            let hold = sources.live.as_deref() == Some(path.as_path());
            while budget > 0 {
                let row = &self.ledger.rows[index];
                if row.confirmed >= observed.len {
                    break;
                }
                let resend = row.crash_resend.is_some();
                let start = row.confirmed;
                let part = match read_part(&mut observed, start, cap, hold && !resend) {
                    Ok(Some(part)) => part,
                    Ok(None) => break,
                    Err(error) => {
                        let message = format!("{}: {error}", file_label(path));
                        self.note_error(index, &message);
                        out.push_error(message);
                        dirty = true;
                        break;
                    }
                };
                budget -= 1;
                let name = part_name(path, start, part.end, resend);
                match transport.post_part(&name, &part.body) {
                    Ok(status) if (200..300).contains(&status) => {
                        let seq = self.next_seq();
                        let row = &mut self.ledger.rows[index];
                        row.confirmed = part.end;
                        row.seq = seq;
                        row.last_error = None;
                        if part.end >= observed.len {
                            row.crash_resend = None;
                        }
                        out.parts_sent += 1;
                        out.bytes_confirmed += part.end - start;
                        // Written now, so a part confirmed before a later failure (or a kill)
                        // is never sent again from an older offset.
                        if let Err(error) = self.save() {
                            out.push_error(error.clone());
                            self.ledger.last_error = Some(clip(error));
                            return out;
                        }
                        dirty = false;
                    }
                    Ok(status) => {
                        out.failed += 1;
                        let message = format!("{name}: server answered HTTP {status}");
                        self.note_error(index, &message);
                        out.push_error(message);
                        dirty = true;
                        break;
                    }
                    Err(error) => {
                        out.failed += 1;
                        let message = format!("{name}: {error}");
                        self.note_error(index, &message);
                        out.push_error(message);
                        dirty = true;
                        break;
                    }
                }
            }
        }
        dirty |= self.age(sources);
        if dirty && let Err(error) = self.save() {
            out.push_error(error);
        }
        out
    }

    /// The pending bytes and last error per source, from the ledger and the files' sizes now.
    /// Reads no file contents and sends nothing.
    #[must_use]
    pub fn status(&self, sources: &OutboxSources) -> OutboxStatus {
        let mut status = OutboxStatus {
            ledger: self.path.clone(),
            rows: self.ledger.rows.len(),
            pending_bytes: 0,
            sources: Vec::with_capacity(sources.logs.len()),
            last_error: self.ledger.last_error.clone(),
            load_error: self.load_error.clone(),
        };
        for path in &sources.logs {
            let bytes = std::fs::symlink_metadata(path)
                .ok()
                .filter(|meta| meta.file_type().is_file())
                .map_or(0, |meta| meta.len());
            let row = self.ledger.rows.iter().find(|r| r.path == *path && !r.retired);
            // A file shorter than its confirmed offset was rotated: all of it is pending.
            let confirmed = row.map_or(0, |r| if bytes < r.confirmed { 0 } else { r.confirmed });
            let pending_bytes = bytes - confirmed;
            status.pending_bytes += pending_bytes;
            status.sources.push(SourceStatus {
                path: path.clone(),
                bytes,
                confirmed,
                pending_bytes,
                tracked: row.is_some(),
                crash_resend: row.is_some_and(|r| r.crash_resend.is_some()),
                last_error: row.and_then(|r| r.last_error.clone()),
            });
        }
        status
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.ledger.next_seq;
        self.ledger.next_seq += 1;
        seq
    }

    fn note_error(&mut self, index: usize, message: &str) {
        let message = clip(message.to_owned());
        self.ledger.rows[index].last_error = Some(message.clone());
        self.ledger.last_error = Some(message);
    }

    /// The live row for `path`, created when there is none, and replaced (the old one retired)
    /// when the file was rotated. Returns the row index and whether the ledger changed.
    fn live_row(&mut self, path: &Path, observed: &Observed, out: &mut OutboxFlush) -> (usize, bool) {
        let identity = observed.identity();
        if let Some(index) = self.ledger.rows.iter().position(|r| r.path == path && !r.retired) {
            if !rotated(&self.ledger.rows[index], observed) {
                let row = &mut self.ledger.rows[index];
                let changed = row.identity != identity;
                row.identity = identity;
                return (index, changed);
            }
            out.rotated += 1;
            let row = &mut self.ledger.rows[index];
            row.retired = true;
            row.crash_resend = None;
        }
        let seq = self.next_seq();
        self.ledger.rows.push(LedgerRow {
            path: path.to_path_buf(),
            identity,
            confirmed: 0,
            seq,
            retired: false,
            crash_resend: None,
            last_error: None,
        });
        (self.ledger.rows.len() - 1, true)
    }

    /// Records every crash marker in `sources.crash_dir` not seen before and sets its session log
    /// back to byte 0. Returns whether the ledger changed.
    fn apply_crash_markers(&mut self, sources: &OutboxSources, out: &mut OutboxFlush) -> bool {
        let Some(dir) = &sources.crash_dir else {
            return false;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut markers: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let base = name.strip_suffix(".rejected").unwrap_or(&name);
                regia_platform::crash::is_crash_file_name(base).then(|| base.to_owned())
            })
            .collect();
        markers.sort();
        markers.dedup();
        let mut changed = false;
        for marker in markers {
            if self.ledger.crash_markers.contains(&marker) {
                continue;
            }
            changed = true;
            self.ledger.crash_markers.push(marker.clone());
            let Some(log) = session_log_for(&marker, &sources.logs) else {
                continue;
            };
            out.crash_resends += 1;
            let seq = self.next_seq();
            if let Some(row) = self.ledger.rows.iter_mut().find(|r| r.path == *log && !r.retired) {
                row.confirmed = 0;
                row.seq = seq;
                row.crash_resend = Some(marker);
            } else {
                // Not sent yet: it goes out from byte 0 anyway, named as a crash resend.
                self.ledger.rows.push(LedgerRow {
                    path: log.clone(),
                    identity: FileIdentity::default(),
                    confirmed: 0,
                    seq,
                    retired: false,
                    crash_resend: Some(marker),
                    last_error: None,
                });
            }
        }
        if self.ledger.crash_markers.len() > MAX_CRASH_MARKERS {
            self.ledger.crash_markers.sort();
            let excess = self.ledger.crash_markers.len() - MAX_CRASH_MARKERS;
            self.ledger.crash_markers.drain(..excess);
        }
        changed
    }

    /// Drops rows over [`OutboxSettings::max_rows`]: retired rows and rows of files no longer
    /// listed first, then the least recently touched. Returns whether any row was dropped.
    fn age(&mut self, sources: &OutboxSources) -> bool {
        let max = self.settings.max_rows.max(1);
        let len = self.ledger.rows.len();
        if len <= max {
            return false;
        }
        let rows = &self.ledger.rows;
        let mut order: Vec<usize> = (0..len).collect();
        order.sort_by_key(|&i| {
            let row = &rows[i];
            (!row.retired && sources.logs.contains(&row.path), row.seq)
        });
        let mut keep = vec![true; len];
        for &i in &order[..len - max] {
            keep[i] = false;
        }
        let mut index = 0;
        self.ledger.rows.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });
        true
    }

    /// Writes the ledger atomically (a temporary file renamed over the old one).
    fn save(&self) -> Result<(), String> {
        let text = ron::ser::to_string_pretty(&self.ledger, regia_core::ron_pretty())
            .map_err(|error| format!("outbox ledger not encoded: {error}"))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| format!("outbox ledger directory: {error}"))?;
        }
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text.as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|error| {
                let _ = std::fs::remove_file(&tmp);
                format!("outbox ledger not written: {error}")
            })
    }
}

/// The version field alone, read before the whole ledger so a newer format is named as such.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

fn load(path: &Path) -> Result<Ledger, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(error) => return Err(format!("cannot read the outbox ledger: {error}")),
    };
    let probe: VersionProbe =
        ron::from_str(&text).map_err(|error| format!("outbox ledger does not parse: {error}"))?;
    if probe.version != LEDGER_VERSION {
        return Err(format!(
            "outbox ledger version {} is not {LEDGER_VERSION}",
            probe.version
        ));
    }
    ron::from_str(&text).map_err(|error| format!("outbox ledger does not parse: {error}"))
}

/// An open source file as one flush sees it.
struct Observed {
    file: File,
    len: u64,
    modified_unix_ms: u64,
    prefix: Vec<u8>,
}

impl Observed {
    fn identity(&self) -> FileIdentity {
        FileIdentity {
            size: self.len,
            modified_unix_ms: self.modified_unix_ms,
            header_len: self.prefix.len() as u64,
            header_hash: fnv1a64(&self.prefix),
        }
    }
}

/// Opens `path` if it is a regular file (never through a symlink) and reads its first bytes.
/// `Ok(None)` when it does not exist.
fn observe(path: &Path) -> Result<Option<Observed>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !meta.file_type().is_file() {
        return Err("not a regular file".into());
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let meta = file.metadata().map_err(|error| error.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let len = meta.len();
    let modified_unix_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64);
    let mut prefix = Vec::new();
    (&mut file)
        .take(len.min(HEADER_BYTES))
        .read_to_end(&mut prefix)
        .map_err(|error| error.to_string())?;
    Ok(Some(Observed {
        file,
        len,
        modified_unix_ms,
        prefix,
    }))
}

/// Whether the file now at a row's path is another file than the row describes.
fn rotated(row: &LedgerRow, observed: &Observed) -> bool {
    if observed.len < row.confirmed {
        return true;
    }
    let header_len = row.identity.header_len;
    if header_len == 0 {
        // Nothing was hashed (the file was empty): only a step back in time tells.
        return observed.modified_unix_ms < row.identity.modified_unix_ms;
    }
    let Ok(header_len) = usize::try_from(header_len) else {
        return true;
    };
    observed
        .prefix
        .get(..header_len)
        .is_none_or(|head| fnv1a64(head) != row.identity.header_hash)
}

/// 64-bit FNV-1a (public domain, Fowler/Noll/Vo): a cheap, stable identity hash of a file's first
/// bytes. Not a security measure.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One part ready to send.
struct Part {
    /// The source offset after the part's last byte.
    end: u64,
    /// The scrubbed body (at most the cap).
    body: Vec<u8>,
}

/// Reads the part starting at `start`: at most `cap` source bytes, cut after the last newline
/// that fits (else on a UTF-8 boundary), shrunk until its scrubbed body fits the cap. `hold` keeps
/// an unfinished last line back; `Ok(None)` when nothing can be sent yet.
fn read_part(observed: &mut Observed, start: u64, cap: u64, hold: bool) -> std::io::Result<Option<Part>> {
    let want = (observed.len - start).min(cap);
    let mut window = Vec::new();
    observed.file.seek(SeekFrom::Start(start))?;
    (&mut observed.file).take(want).read_to_end(&mut window)?;
    if window.is_empty() {
        return Ok(None);
    }
    let reaches_end = start + window.len() as u64 >= observed.len;
    let mut cut = if reaches_end && (!hold || window.ends_with(b"\n")) {
        window.len()
    } else if let Some(newline) = window.iter().rposition(|&b| b == b'\n') {
        newline + 1
    } else if reaches_end {
        return Ok(None);
    } else {
        utf8_cut(&window)
    };
    // The scrubber redacts a private-key block from its BEGIN line on, so a part must never end
    // inside one: the next part would start with key lines and no BEGIN.
    if let Some(block) = open_key_block(&window[..cut]) {
        if block > 0 {
            cut = block;
        } else {
            let limit = observed.len.min(start.saturating_add(MAX_KEY_BLOCK_BYTES));
            let more = limit.saturating_sub(start + window.len() as u64);
            (&mut observed.file).take(more).read_to_end(&mut window)?;
            cut = match key_block_end(&window) {
                Some(end) => end,
                // Still being written: wait for the END line.
                None if hold && (window.len() as u64) < MAX_KEY_BLOCK_BYTES => return Ok(None),
                // No END before the file ends or within the limit: all of it is redacted.
                None => window.len(),
            };
        }
    }
    let mut body = scrub_upload(&window[..cut]);
    while body.len() as u64 > cap && cut > 1 {
        let half = cut / 2;
        cut = match window[..half].iter().rposition(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            None => utf8_cut(&window[..half]),
        };
        body = scrub_upload(&window[..cut]);
    }
    Ok(Some(Part {
        end: start + cut as u64,
        body,
    }))
}

/// Most source bytes one part reads ahead to find the end of a private-key block.
const MAX_KEY_BLOCK_BYTES: u64 = 1 << 20;

/// Where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The offset just past the END marker of the private-key block `bytes` starts with, by the
/// scrubber's own rule (`-----END `, then the next `-----`).
fn key_block_end(bytes: &[u8]) -> Option<usize> {
    let end = find(bytes, b"-----END ")?;
    let close = find(&bytes[end + 9..], b"-----")?;
    Some(end + 9 + close + 5)
}

/// The start of a private-key block in `bytes` (a `-----BEGIN ` line naming `PRIVATE KEY`) whose
/// END is not in `bytes`.
fn open_key_block(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    while let Some(found) = find(&bytes[at..], b"-----BEGIN ") {
        let begin = at + found;
        let rest = &bytes[begin..];
        let header = &rest[..rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len())];
        if find(header, b"PRIVATE KEY").is_some() {
            match key_block_end(rest) {
                Some(end) => at = begin + end,
                None => return Some(begin),
            }
        } else {
            at = begin + 11;
        }
    }
    None
}

/// The length of `bytes` without a UTF-8 sequence cut short at its end (the whole length when
/// the end is not inside a sequence, or when nothing would be left).
fn utf8_cut(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for i in (len.saturating_sub(3)..len).rev() {
        let byte = bytes[i];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let need = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 1,
        };
        return if i + need > len && i > 0 { i } else { len };
    }
    len
}

/// The `X-Regia-Report-Name` of a part: `<file>;<start>-<end>`, plus `;crash` in a crash resend.
fn part_name(path: &Path, start: u64, end: u64, resend: bool) -> String {
    let suffix = if resend { ";crash" } else { "" };
    format!("{};{start}-{end}{suffix}", file_label(path))
}

/// A source's file name, scrubbed and header-safe (never its directory).
fn file_label(path: &Path) -> String {
    let name = path
        .file_name()
        .map_or_else(|| "log".to_owned(), |name| name.to_string_lossy().into_owned());
    header_safe_name(&regia_platform::crash::scrub_field(&name))
}

/// The session log a crash marker belongs to: the log whose start-time name
/// (`YYYY-MM-DD_HH-MM-SS[_N]`, the engine's session file names) is the latest at or before the
/// marker's (`crash-YYYY-MM-DD_HH-MM-SS[_N].txt`). `None` when no log name is a start time.
fn session_log_for<'a>(marker: &str, logs: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let stem = marker
        .strip_prefix(regia_platform::crash::CRASH_PREFIX)?
        .strip_suffix(regia_platform::crash::CRASH_SUFFIX)?;
    let (crash_time, _) = start_time(stem)?;
    logs.iter()
        .filter_map(|log| {
            let key = start_time(log.file_stem()?.to_str()?)?;
            (key.0 <= crash_time).then_some((key, log))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, log)| log)
}

/// `YYYY-MM-DD_HH-MM-SS` and the same-second counter of a session or crash file stem.
fn start_time(stem: &str) -> Option<(&str, u32)> {
    let time = stem.get(..19)?;
    let shape = time.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 | 13 | 16 => b == b'-',
        10 => b == b'_',
        _ => b.is_ascii_digit(),
    });
    if !shape {
        return None;
    }
    match &stem[19..] {
        "" => Some((time, 0)),
        rest => {
            let n = rest.strip_prefix('_')?;
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .then(|| (time, n.parse().unwrap_or(u32::MAX)))
        }
    }
}

/// Keeps at most `MAX_ERROR_CHARS` characters of `message`.
fn clip(message: String) -> String {
    match message.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => format!("{}...", &message[..end]),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_cut_never_splits_a_character() {
        assert_eq!(utf8_cut(b"abc"), 3);
        let euro = "a€".as_bytes(); // 61 e2 82 ac
        assert_eq!(utf8_cut(euro), 4);
        assert_eq!(utf8_cut(&euro[..3]), 1);
        assert_eq!(utf8_cut(&euro[..2]), 1);
        // Invalid bytes are not sequences: nothing to hold back.
        assert_eq!(utf8_cut(&[0xff, 0xff]), 2);
        // A lone lead byte cannot shrink to nothing.
        assert_eq!(utf8_cut(&[0xe2]), 1);
    }

    #[test]
    fn session_logs_match_crash_markers_by_start_time() {
        let logs: Vec<PathBuf> = [
            "2026-11-17_09-00-00.log",
            "2026-11-17_10-00-00.log",
            "2026-11-17_10-00-00_1.log",
            "2026-11-17_11-00-00.log",
            "notes.log",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let pick = |marker: &str| session_log_for(marker, &logs).map(|p| p.display().to_string());
        assert_eq!(
            pick("crash-2026-11-17_10-30-00.txt").as_deref(),
            Some("2026-11-17_10-00-00_1.log")
        );
        assert_eq!(
            pick("crash-2026-11-17_11-00-00_2.txt").as_deref(),
            Some("2026-11-17_11-00-00.log")
        );
        assert_eq!(pick("crash-2026-11-17_08-00-00.txt"), None);
        assert_eq!(pick("crash-garbage.txt"), None);
        assert_eq!(start_time("2026-11-17_10-00-00_x"), None);
    }

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn the_ledger_round_trips_through_ron() {
        let ledger = Ledger {
            next_seq: 3,
            rows: vec![LedgerRow {
                path: PathBuf::from("/logs/2026-11-17_10-00-00.log"),
                identity: FileIdentity {
                    size: 10,
                    modified_unix_ms: 5,
                    header_len: 10,
                    header_hash: 7,
                },
                confirmed: 10,
                seq: 2,
                retired: false,
                crash_resend: Some("crash-2026-11-17_10-30-00.txt".into()),
                last_error: None,
            }],
            crash_markers: vec!["crash-2026-11-17_10-30-00.txt".into()],
            ..Ledger::default()
        };
        let text = ron::ser::to_string_pretty(&ledger, regia_core::ron_pretty()).expect("encode");
        let back: Ledger = ron::from_str(&text).expect("decode");
        assert_eq!(back, ledger);
    }
}
