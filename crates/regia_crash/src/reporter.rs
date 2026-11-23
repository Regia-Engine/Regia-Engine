// SPDX-License-Identifier: MPL-2.0
//! The opt-in [`CrashReporter`]: uploads pending crash reports and a game-provided match log to a
//! configurable endpoint on next start, with retry/backoff, a size cap, a consent flag, and
//! offline safety (`docs/specs/21-tooling-and-packaging.md` § 5).
//!
//! Nothing is sent unless the game opts in: the endpoint must be set (empty = disabled),
//! [`CrashReporter::consent`] must be true, and a remote `http://` endpoint (see
//! [`crate::transport::is_remote_cleartext`]) needs [`CrashReporter::allow_cleartext`]. Each upload
//! is capped by [`CrashReporter::max_bytes`] on the bytes read from disk, before scrubbing.
//! Uploads are best-effort and never delete a report the server did not accept (2xx). A transport
//! failure, HTTP 408, HTTP 429, or a 5xx leaves the file pending. Any other non-2xx renames it to
//! `*.rejected` (it is no longer pending). `https://` endpoints require the crate's `tls` feature
//! (ureq + rustls); without it `https://` is refused.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_app::prelude::*;

use crate::transport::{TransportError, TransportTimeouts, is_retriable, post};

/// Default per-report size cap (2 MiB). Reports larger than this are left on disk.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Most pending crash reports uploaded per launch; the rest wait for the next start.
pub const MAX_REPORTS_PER_LAUNCH: usize = 32;

/// Hard ceiling on retries per report, whatever [`CrashReporter::max_retries`] says, so a huge
/// value cannot wedge the IO worker in backoff sleeps.
pub const MAX_RETRIES_CEILING: u32 = 8;

/// Longest single backoff sleep.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Delay before retry number `attempt` (1-based): `backoff * 2^(attempt-1)`, saturating, capped at
/// `MAX_BACKOFF` (tighter than the transport helper's own cap).
fn retry_delay(backoff: Duration, attempt: u32) -> Duration {
    crate::transport::retry_delay(backoff, attempt).min(MAX_BACKOFF)
}

/// Why [`read_capped`] refused a file.
#[derive(Debug)]
enum ReadError {
    /// Not a regular file (symlink, FIFO, directory, device), or it could not be opened or read.
    Unreadable,
    /// More than the cap.
    TooLarge,
    /// The volume exposes no stable file ids (FAT/exFAT) or the query failed, so the read
    /// could not prove which file the path names.
    IdentityUnproven,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "not a readable regular file",
            Self::TooLarge => "over the size cap",
            Self::IdentityUnproven => "file identity cannot be proven on this volume",
        })
    }
}

/// Reads at most `max_bytes` (0 = no limit) from `reader`; `Ok(None)` when it holds more.
fn read_bounded<R: Read>(mut reader: R, max_bytes: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if max_bytes == 0 {
        reader.read_to_end(&mut bytes)?;
        return Ok(Some(bytes));
    }
    reader.take(max_bytes.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max_bytes).then_some(bytes))
}

/// The platform file id of an open file: device + inode on Unix, volume serial + file index on
/// Windows. Identity that cannot be proven is no identity at all: where the platform exposes no
/// stable file ids, or the query fails, `file_id` returns `None` and the caller fails closed —
/// the read is refused with [`ReadError::IdentityUnproven`], or a later check treats the file
/// as replaced, never deleted or quarantined unsent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileId {
    #[cfg(unix)]
    Unix { dev: u64, ino: u64 },
    #[cfg(windows)]
    Windows { volume: u32, index: u64 },
}

/// The id of the file `file`'s handle describes.
fn file_id(file: &std::fs::File) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().ok()?;
        Some(FileId::Unix {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        let (volume, index) = regia_platform::open_file_identity(file)?;
        Some(FileId::Windows { volume, index })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        None
    }
}

/// The id of the file `path` names right now (opened without following a final symlink, so a
/// swapped-in link describes itself and compares unequal).
fn path_id(path: &Path) -> Option<FileId> {
    file_id(&open_no_follow(path).ok()?)
}

/// Opens `path` without following a final symlink or reparse point on Windows (the handle then
/// describes the link itself, which is not a regular file and is refused); elsewhere `file_id`
/// guards the swap.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        /// `FILE_FLAG_OPEN_REPARSE_POINT`.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)
    }
}

/// Reads a regular, non-symlink file of at most `max_bytes` (0 = no limit). The type is checked with
/// `symlink_metadata` before opening, so a symlink to a secret or a FIFO that would block `open` is
/// refused; the opened handle must then be that same file, so a symlink swapped in between is
/// refused too, and the read itself is bounded (never trusting `st_size`, which is 0 for `/proc`
/// files) so a file that grew since it was listed cannot exhaust memory.
#[cfg(test)]
fn read_capped(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ReadError> {
    read_capped_with_meta(path, max_bytes).map(|(bytes, _)| bytes)
}

/// What a bounded read proves about the file it read: its id, length and modification time. The
/// proof is checked again before the file at the path is deleted or quarantined (see
/// `still_the_file`).
#[derive(Debug)]
struct ReadProof {
    id: FileId,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// `read_capped` that also returns proof of which file the bytes came from, so a later delete or
/// rename can check the path still names that file (see `still_the_file`).
fn read_capped_with_meta(path: &Path, max_bytes: u64) -> Result<(Vec<u8>, ReadProof), ReadError> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| ReadError::Unreadable)?;
    if !meta.file_type().is_file() {
        return Err(ReadError::Unreadable);
    }
    let file = open_no_follow(path).map_err(|_| ReadError::Unreadable)?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(ReadError::Unreadable);
    }
    // The handle must describe the same file the path names right now: a plain file swapped in
    // after the listing (or, on Unix where `open` follows links, a symlink swapped in) is
    // another file and is refused, not uploaded or deleted through the wrong identity.
    let id = file_id(&file).ok_or(ReadError::IdentityUnproven)?;
    if path_id(path) != Some(id) {
        return Err(ReadError::IdentityUnproven);
    }
    let proof = ReadProof {
        id,
        len: meta.len(),
        modified: meta.modified().ok(),
    };
    match read_bounded(file, max_bytes) {
        Ok(Some(bytes)) => Ok((bytes, proof)),
        Ok(None) => Err(ReadError::TooLarge),
        Err(_) => Err(ReadError::Unreadable),
    }
}

/// Whether `path` still names the unchanged regular file `read` described: same file id, length
/// and modification time. An upload can take minutes; a report rewritten under the same name (or a
/// match log the game reopened) meanwhile must not be deleted or quarantined unsent
/// (audit-concurrency-1103 crash M4).
fn still_the_file(path: &Path, read: &ReadProof) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|now| {
        now.file_type().is_file()
            && now.len() == read.len
            && now.modified().ok() == read.modified
            && path_id(path).is_some_and(|id| id == read.id)
    })
}

/// Upload configuration. Empty `endpoint` (the default) disables uploading.
#[derive(Resource, Debug, Clone)]
pub struct CrashReporter {
    /// The endpoint crash reports and the match log are POSTed to. Production endpoints must be
    /// `https://` (needs the `tls` feature). Plain `http://` is sent only when the host string is
    /// `localhost` or a literal loopback IP (names are not resolved; a LAN address of this machine
    /// is refused) unless [`allow_cleartext`](Self::allow_cleartext) is set. Empty = disabled.
    pub endpoint: String,
    /// Sends reports in cleartext to a remote `http://` endpoint. Log tails hold paths and user
    /// names, so this stays off in anything shipped.
    pub allow_cleartext: bool,
    /// Whether the user consented to sending crash reports. Uploads are skipped while `false`.
    pub consent: bool,
    /// Reports larger than this many bytes are skipped (0 = no limit).
    pub max_bytes: u64,
    /// Retries per report after the first attempt (0 = one attempt). Clamped to
    /// `MAX_RETRIES_CEILING`.
    pub max_retries: u32,
    /// Initial retry delay; doubled after each failed attempt, then capped at 60 seconds.
    /// A `backoff` longer than 60 seconds sleeps 60 seconds. This is tighter than
    /// [`crate::transport::retry_delay`]'s five-minute cap.
    pub backoff: Duration,
    /// A game-written match log to upload on next start, if present.
    pub match_log: Option<PathBuf>,
    /// The incremental log outbox (ADR-8402): when set, the startup upload also sends the new
    /// tails of the session logs in the crash directory, and resends a crashed session's log once
    /// ([`crate::outbox`]). `None` (the default) sends no logs.
    pub outbox: Option<crate::outbox::OutboxSettings>,
}

impl Default for CrashReporter {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            allow_cleartext: false,
            consent: false,
            max_bytes: DEFAULT_MAX_BYTES,
            max_retries: 2,
            backoff: Duration::from_secs(1),
            match_log: None,
            outbox: None,
        }
    }
}

/// The outcome of one [`CrashReporter::upload_pending`] call.
#[derive(Debug, Clone, Default)]
pub struct UploadSummary {
    /// Nothing was sent: no endpoint, no consent, or a remote `http://` endpoint while
    /// `allow_cleartext` is false (see [`crate::transport::is_remote_cleartext`]).
    pub disabled: bool,
    /// Reports an upload was attempted for.
    pub attempted: u64,
    /// Reports whose sent bytes the server accepted (HTTP 2xx). The file is removed when it is
    /// still the file that was read. A path replaced during the POST is still counted here and
    /// left on disk: the bytes that were sent were accepted, and the file now at the path is a
    /// different report. A 2xx whose remove fails is counted in [`failed`](Self::failed), not here.
    pub uploaded: u64,
    /// Reports the server rejected or that could not be sent; they stay on disk (a final 4xx
    /// answer renames the file with a `.rejected` suffix so later starts skip it).
    pub failed: u64,
    /// Reports skipped because they exceeded [`CrashReporter::max_bytes`].
    pub skipped_too_large: u64,
    /// One message per failed report, at most 16 messages of 200 characters.
    pub errors: Vec<String>,
}

/// The most error messages an [`UploadSummary`] keeps.
const MAX_SUMMARY_ERRORS: usize = 16;
/// The longest kept error message, in characters.
const MAX_ERROR_CHARS: usize = 200;
/// File-name suffix appended to a report the server rejected for good (it is no longer listed).
const REJECTED_SUFFIX: &str = ".rejected";

impl UploadSummary {
    /// Records an error message, keeping at most `MAX_SUMMARY_ERRORS` messages of at most
    /// `MAX_ERROR_CHARS` characters each.
    fn push_error(&mut self, message: String) {
        if self.errors.len() >= MAX_SUMMARY_ERRORS {
            return;
        }
        let message = match message.char_indices().nth(MAX_ERROR_CHARS) {
            Some((end, _)) => format!("{}...", &message[..end]),
            None => message,
        };
        self.errors.push(message);
    }
}

/// Whether a non-2xx status is a final rejection: anything but a 2xx, a transient 408/429, or a
/// 5xx (3xx too, since redirects are never followed, and out-of-range codes). Resending the same
/// bytes on every later launch would never succeed.
fn is_final_rejection(status: u16) -> bool {
    !(200..300).contains(&status) && status != 408 && status != 429 && !(500..600).contains(&status)
}

/// Deletes the uploaded report at `path` (the file that was read and sent, not a lookup by name).
fn remove_report(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// Moves a rejected report out of the pending list by appending `.rejected` to its name, so the
/// next start does not dial again. The bytes stay on disk for the owner; the platform crash writer
/// bounds the directory.
fn quarantine(path: &Path) {
    let mut target = path.as_os_str().to_owned();
    target.push(REJECTED_SUFFIX);
    // Pending reports can arrive through a synced logs folder together with an earlier
    // `.rejected` twin, so the destination can already exist. std's `rename` replaces an
    // existing file destination atomically on every platform (on Windows it maps to
    // `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`), so a single rename both keeps the newest
    // quarantine and never leaves the report without one: a pre-delete plus rename could lose
    // the old quarantine to a crash (or a sharing violation on Windows) before the new one
    // lands. The earlier premise that "rename never replaces on Windows" was wrong
    // (audit fixrev-06-k H2).
    // The destination may already exist (see above); note the deliberate replacement.
    let replaced = std::fs::symlink_metadata(&target).is_ok();
    if let Err(error) = std::fs::rename(path, &target) {
        tracing::warn!(target: "regia_crash", "cannot quarantine a rejected report: {error}");
    } else if replaced {
        tracing::info!(
            target: "regia_crash",
            "replaced an existing quarantined report: {}",
            Path::new(&target).display()
        );
    }
}

impl CrashReporter {
    /// Whether uploads are enabled (endpoint set and consent given, and the endpoint is not a
    /// remote `http://` one this reporter refuses).
    pub fn enabled(&self) -> bool {
        !self.endpoint.is_empty() && self.consent && !self.refuses_cleartext()
    }

    /// Whether the endpoint is a remote `http://` one and cleartext was not allowed.
    fn refuses_cleartext(&self) -> bool {
        !self.allow_cleartext && crate::transport::is_remote_cleartext(&self.endpoint)
    }

    /// Uploads every pending crash report (`crash-*.txt`).
    ///
    /// A 2xx removes the file when it is still the file that was read. A transport failure, HTTP
    /// 408, HTTP 429, or a 5xx leaves that `crash-*.txt` for the next start. Any other status
    /// (including 301, 302, and 404; redirects are not followed) renames it to `*.rejected`.
    /// `pending()` and `diagnostics.crash_reports` list only the bare `crash-*.txt` names, so a
    /// quarantined report is not listed or sent again. The `.rejected` bytes stay on disk.
    pub fn upload_pending(&self) -> UploadSummary {
        if !self.enabled() {
            return UploadSummary {
                disabled: true,
                ..UploadSummary::default()
            };
        }
        // Entries that are not regular files (symlinks, say from a synced logs folder) are refused
        // at read time; they must not use up the per-launch cap, or 32 of them would keep every
        // real report behind them from ever being attempted (audit-concurrency-1103 crash L3).
        let (regular, refused): (Vec<_>, Vec<_>) =
            regia_platform::crash::pending().into_iter().partition(|report| {
                std::fs::symlink_metadata(&report.path).is_ok_and(|meta| meta.file_type().is_file())
            });
        self.upload_files(
            refused
                .into_iter()
                .chain(regular.into_iter().take(MAX_REPORTS_PER_LAUNCH))
                .collect(),
            remove_report,
        )
    }

    /// The body of [`CrashReporter::upload_pending`] with the file list and the deleter injected.
    fn upload_files(
        &self,
        files: Vec<regia_platform::crash::CrashFile>,
        remove: impl Fn(&Path) -> std::io::Result<()>,
    ) -> UploadSummary {
        let mut summary = UploadSummary::default();
        for report in files {
            if self.max_bytes > 0 && report.bytes > self.max_bytes {
                summary.skipped_too_large += 1;
                continue;
            }
            // A symlink (or anything else that is not a plain file) could point outside the log
            // directory; never upload through one. The read is bounded by the cap, so a report
            // that grew since it was listed is skipped before it is buffered.
            let (bytes, read) = match read_capped_with_meta(&report.path, self.max_bytes) {
                Ok(read) => read,
                Err(ReadError::TooLarge) => {
                    summary.skipped_too_large += 1;
                    continue;
                }
                Err(error @ (ReadError::Unreadable | ReadError::IdentityUnproven)) => {
                    // Nothing was sent, so this is a failure but not an attempt. The identity
                    // failure names its cause (FAT/exFAT volume, failed query) so the user can
                    // move the crash directory instead of guessing.
                    summary.failed += 1;
                    summary.push_error(format!("{}: {error}", report.name));
                    continue;
                }
            };
            summary.attempted += 1;
            // A file name is whatever the directory holds; one with CR or LF would fail as a
            // header on every launch and never be quarantined.
            match self.upload_with_retry(&header_safe_name(&report.name), &bytes) {
                // Delete the exact file that was read and sent: the process-global crash
                // directory may have been reconfigured while the upload ran.
                Ok(status) if (200..300).contains(&status) && !still_the_file(&report.path, &read) => {
                    // What was sent is accepted; the file now at the path is another report.
                    summary.uploaded += 1;
                    tracing::info!(target: "regia_crash", "{}: replaced during its upload, kept for the next start", report.name);
                }
                Ok(status) if (200..300).contains(&status) => match remove(&report.path) {
                    Ok(()) => summary.uploaded += 1,
                    Err(error) => {
                        summary.failed += 1;
                        summary.push_error(format!(
                            "{}: accepted (HTTP {status}) but not removed: {error}",
                            report.name
                        ));
                    }
                },
                Ok(status) => {
                    summary.failed += 1;
                    if is_final_rejection(status) && still_the_file(&report.path, &read) {
                        quarantine(&report.path);
                    }
                    summary.push_error(format!("{}: server answered HTTP {status}", report.name));
                }
                Err(error) => {
                    summary.failed += 1;
                    summary.push_error(format!("{}: {error}", report.name));
                }
            }
        }
        summary
    }

    /// Uploads a game-provided match log (identified by `name`). Returns the server status.
    ///
    /// Enforces the same gates as [`CrashReporter::upload_pending`]: nothing is sent when the
    /// endpoint is empty or consent is off ([`TransportError::Disabled`]), when the endpoint is
    /// a remote `http://` URL and cleartext was not allowed ([`TransportError::RemoteCleartext`]),
    /// or when `bytes` exceeds [`CrashReporter::max_bytes`].
    pub fn upload_match_log(&self, name: &str, bytes: &[u8]) -> Result<u16, TransportError> {
        if self.endpoint.is_empty() || !self.consent {
            return Err(TransportError::Disabled);
        }
        if self.refuses_cleartext() {
            return Err(TransportError::RemoteCleartext);
        }
        if self.max_bytes > 0 && bytes.len() as u64 > self.max_bytes {
            return Err(TransportError::TooLarge {
                size: bytes.len() as u64,
                max_bytes: self.max_bytes,
            });
        }
        let name = header_safe_name(name);
        let name = if name.is_empty() {
            "match.log"
        } else {
            name.as_str()
        };
        self.upload_with_retry(name, bytes)
    }

    /// POSTs `bytes` under `name`, retrying transient failures with exponential backoff. Retries
    /// transport errors and HTTP 5xx (server errors) up to `max_retries` additional attempts; 2xx
    /// and 4xx are returned immediately (a 4xx is a client error retrying will not fix).
    ///
    /// This is the one door every upload leaves through, so the name and the body are scrubbed
    /// here ([`scrub_upload`]): a report written by an older build, or a game's match log, is
    /// sent with the same redaction as a fresh report's fields.
    fn upload_with_retry(&self, name: &str, bytes: &[u8]) -> Result<u16, TransportError> {
        let name = header_safe_name(&regia_platform::crash::scrub_field(name));
        let body = scrub_upload(bytes);
        let mut attempt = 0u32;
        loop {
            let result = post(
                &self.endpoint,
                &body,
                "text/plain",
                Some(name.as_str()),
                TransportTimeouts::default(),
            );
            let retriable = match &result {
                Ok(status) => (500..600).contains(&i32::from(*status)),
                Err(error) => is_retriable(error),
            };
            if !retriable || attempt >= self.max_retries.min(MAX_RETRIES_CEILING) {
                return result;
            }
            attempt += 1;
            std::thread::sleep(retry_delay(self.backoff, attempt));
        }
    }
}

/// What leaves the machine of an uploaded report or match log: its text (invalid UTF-8 becomes
/// U+FFFD; the upload is `text/plain`) through `regia_platform::crash::scrub_field`, the function a
/// report's fields are written through. The home folder becomes `~`, secrets `[redacted]` and email
/// addresses `[email]` (threat model finding 16, ADR-3285).
pub fn scrub_upload(bytes: &[u8]) -> Vec<u8> {
    regia_platform::crash::scrub_field(&String::from_utf8_lossy(bytes)).into_bytes()
}

/// The `Startup` system set holding the next-start upload. A game that loads its endpoint and
/// consent from saved settings in a `Startup` system must order that system `.before(CrashUploadSet)`
/// so the upload sees the loaded values.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrashUploadSet;

/// Turns a match-log file name into a value safe for the `X-Regia-Report-Name` header: every
/// character outside visible ASCII becomes `_` (so `partida-mañana.log` still uploads).
pub(crate) fn header_safe_name(name: &str) -> String {
    name.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '_' })
        .collect()
}

/// Logs the match-log upload `result` and, like a crash report, removes the file after a 2xx and
/// quarantines it after a final rejection, so later starts do not POST the same bytes again.
fn settle_match_log(path: &Path, result: &Result<u16, TransportError>) {
    match result {
        Ok(status) if (200..300).contains(status) => {
            tracing::info!(target: "regia_crash", "match-log upload: HTTP {status}");
            if let Err(error) = remove_report(path) {
                tracing::warn!(target: "regia_crash", "match log uploaded but not removed: {error}");
            }
        }
        Ok(status) => {
            tracing::warn!(target: "regia_crash", "match-log upload rejected: HTTP {status}");
            if is_final_rejection(*status) {
                quarantine(path);
            }
        }
        Err(error) => {
            tracing::warn!(target: "regia_crash", "match-log upload failed: {error}")
        }
    }
}

/// Installs the [`CrashReporter`] resource and queues a next-start upload. The game inserts its own
/// [`CrashReporter`] (from its settings) before adding this plugin to configure the endpoint and
/// consent.
#[derive(Default)]
pub struct CrashReporterPlugin;

impl Plugin for CrashReporterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CrashReporter>()
            .add_systems(Startup, queue_startup_upload.in_set(CrashUploadSet));
    }
}

/// `Startup`: if uploads are enabled, upload pending reports and the match log on a background
/// thread so the first frame is never blocked by the network.
fn queue_startup_upload(reporter: Res<CrashReporter>) {
    if reporter.consent && reporter.refuses_cleartext() {
        tracing::warn!(target: "regia_crash", endpoint = %reporter.endpoint, "crash upload is off: this endpoint would send reports in cleartext (use https://)");
    }
    if !reporter.enabled() {
        return;
    }
    let reporter = reporter.clone();
    // Its own thread, not the shared IO pool: the upload blocks on the network and sleeps between
    // retries (minutes with a slow collector), which would starve every other `spawn_io` user
    // (one IO thread on small machines; audit-concurrency-1103 crash M1).
    let spawned = std::thread::Builder::new()
        .name(UPLOAD_THREAD.into())
        .spawn(move || upload_at_startup(&reporter));
    if let Err(error) = spawned {
        tracing::warn!(target: "regia_crash", "crash upload not started: {error}");
    }
}

/// Name of the startup upload thread (`regia-crash-up` is what Linux keeps of it).
const UPLOAD_THREAD: &str = "regia-crash-upload";

/// The startup upload: the log outbox (before the crash reports, whose files are its crash
/// markers), pending crash reports, then the match log.
fn upload_at_startup(reporter: &CrashReporter) {
    if let Some(settings) = &reporter.outbox {
        flush_outbox(reporter, settings);
    }
    let summary = reporter.upload_pending();
    if let Some(path) = &reporter.match_log {
        match read_capped_with_meta(path, reporter.max_bytes) {
            Ok((bytes, read)) => {
                let name = path
                    .file_name()
                    .map(|n| header_safe_name(&n.to_string_lossy()))
                    .unwrap_or_else(|| "match.log".to_owned());
                let result = reporter.upload_match_log(&name, &bytes);
                if still_the_file(path, &read) {
                    settle_match_log(path, &result);
                } else {
                    // The game reused the path (a new match started) while this one uploaded.
                    tracing::info!(target: "regia_crash", "match log changed during its upload; kept");
                }
            }
            Err(error) => {
                tracing::warn!(target: "regia_crash", "match log skipped: {error}")
            }
        }
    }
    // Counters only: the per-report errors can be long and the log ring is copied into the
    // next crash report.
    tracing::info!(
        target: "regia_crash",
        "crash upload finished: attempted {} uploaded {} failed {} skipped_too_large {}",
        summary.attempted,
        summary.uploaded,
        summary.failed,
        summary.skipped_too_large
    );
}

/// One outbox flush over the session logs in the crash directory, through the HTTP transport.
fn flush_outbox(reporter: &CrashReporter, settings: &crate::outbox::OutboxSettings) {
    let Some(logs_dir) = regia_platform::crash::crash_dir() else {
        return;
    };
    let Some(ledger) = settings.ledger_path(Some(&logs_dir)) else {
        return;
    };
    let sources = crate::outbox::OutboxSources::from_logs_dir(&logs_dir);
    let mut outbox = crate::outbox::Outbox::open(ledger, settings.clone());
    let mut transport = crate::transport::HttpTransport::new(reporter.endpoint.clone());
    let flush = outbox.flush(reporter, &sources, &mut transport);
    tracing::info!(
        target: "regia_crash",
        "log outbox: parts {} bytes {} failed {} rotated {} crash resends {}",
        flush.parts_sent,
        flush.bytes_confirmed,
        flush.failed,
        flush.rotated,
        flush.crash_resends
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_platform::crash::CrashFile;

    #[test]
    fn a_remote_http_endpoint_is_off_unless_cleartext_is_allowed() {
        let remote = CrashReporter {
            endpoint: "http://collector.example.com/r".into(),
            consent: true,
            ..CrashReporter::default()
        };
        assert!(!remote.enabled());
        let error = remote.upload_match_log("m.log", b"x").expect_err("cleartext");
        assert!(matches!(error, TransportError::RemoteCleartext), "{error:?}");
        let text = error.to_string();
        assert!(text.contains("http") || text.contains("cleartext"), "{text}");
        assert!(!text.contains("no consent"), "{text}");
        assert!(remote.upload_pending().disabled);
        let allowed = CrashReporter {
            allow_cleartext: true,
            ..remote.clone()
        };
        assert!(allowed.enabled());
        let local = CrashReporter {
            endpoint: "http://127.0.0.1:9/r".into(),
            ..remote
        };
        assert!(local.enabled());
    }

    // Windows cannot create a file name with a newline, so the case cannot occur there.
    #[cfg(unix)]
    #[test]
    fn a_crash_file_name_with_a_newline_reaches_the_transport_as_a_safe_header() {
        let d = dir("crash_header_name");
        let report = file(&d, "crash-a\nb.txt", "body");
        let reporter = CrashReporter {
            endpoint: "http://127.0.0.1:1/r".into(),
            consent: true,
            max_retries: 0,
            ..CrashReporter::default()
        };
        let summary = reporter.upload_files(vec![report], |_| Ok(()));
        assert_eq!(summary.failed, 1);
        // Connection refused, not an invalid header that would repeat on every launch.
        assert!(
            summary.errors.iter().all(|e| !e.contains("header")),
            "{:?}",
            summary.errors
        );
        std::fs::remove_dir_all(&d).ok();
    }

    // Windows arm of the check the audit found stubbed (`same_file` returned `true` for any two
    // metadata records, so a same-length, same-mtime replacement passed `still_the_file` and
    // was deleted or quarantined unsent); runs on Unix too. Equal lengths, so the assertion
    // exercises the file-id check, not any length or mtime fallback.
    #[cfg(any(unix, windows))]
    #[test]
    fn two_distinct_files_are_not_the_same_file() {
        let d = dir("crash-same-file");
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        std::fs::write(&a, "0123456789ab").expect("write a");
        std::fs::write(&b, "fedcba987654").expect("write b");
        let id_a = file_id(&std::fs::File::open(&a).expect("open a")).expect("id a");
        let id_b = file_id(&std::fs::File::open(&b).expect("open b")).expect("id b");
        assert_ne!(id_a, id_b);
        assert_eq!(
            id_a,
            path_id(&a).expect("path id"),
            "the path names the opened file"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    fn dir(tag: &str) -> PathBuf {
        let d = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn file(dir: &std::path::Path, name: &str, body: &str) -> CrashFile {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write");
        CrashFile {
            name: name.to_owned(),
            bytes: body.len() as u64,
            path,
            modified_unix_ms: 0,
        }
    }

    fn server(status: u16) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim_end().is_empty() {
                        break;
                    }
                    if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let _ = reader
                    .into_inner()
                    .write_all(format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n").as_bytes());
            }
        });
        format!("http://{addr}/u")
    }

    fn reporter(endpoint: String) -> CrashReporter {
        CrashReporter {
            endpoint,
            consent: true,
            max_retries: 0,
            backoff: Duration::from_millis(1),
            ..CrashReporter::default()
        }
    }

    #[test]
    fn a_failed_remove_counts_as_failed_with_the_error() {
        let d = dir("crash-remove-fail");
        let f = file(&d, "crash-a.txt", "body");
        let summary = reporter(server(200)).upload_files(vec![f], |_| Err(std::io::Error::other("locked")));
        assert_eq!((summary.uploaded, summary.failed), (0, 1));
        assert!(summary.errors[0].contains("locked"), "{:?}", summary.errors);
    }

    #[test]
    fn an_unreadable_report_counts_as_failed() {
        let d = dir("crash-unreadable");
        let mut f = file(&d, "crash-a.txt", "body");
        f.path = d.join("gone.txt");
        let summary = reporter(server(200)).upload_files(vec![f], |_| Ok(()));
        // The missing file is not a regular file, so it fails before the read (and before any
        // upload attempt).
        assert_eq!((summary.attempted, summary.uploaded, summary.failed), (0, 0, 1));
        assert_eq!(summary.errors.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_report_is_never_uploaded() {
        let d = dir("crash-symlink");
        let target = d.join("secret.txt");
        std::fs::write(&target, "secret").expect("write");
        let link = d.join("crash-link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let f = CrashFile {
            name: "crash-link.txt".into(),
            path: link,
            bytes: 6,
            modified_unix_ms: 0,
        };
        let removed = std::cell::Cell::new(false);
        let summary = reporter(server(200)).upload_files(vec![f], |_| {
            removed.set(true);
            Ok(())
        });
        assert_eq!((summary.uploaded, summary.failed), (0, 1));
        assert!(!removed.get());
        assert!(target.exists());
    }

    #[test]
    fn read_capped_bounds_the_read_and_rejects_non_files() {
        let d = dir("crash-read-capped");
        let p = d.join("m.log");
        std::fs::write(&p, "12345").expect("write");
        assert_eq!(read_capped(&p, 5).expect("at cap"), b"12345");
        assert!(read_capped(&p, 4).is_err());
        assert_eq!(read_capped(&p, 0).expect("unlimited"), b"12345");
        assert!(read_capped(&d, 10).is_err());
        assert!(read_capped(&d.join("missing"), 10).is_err());
    }

    #[test]
    fn header_names_replace_non_ascii_and_control_characters() {
        assert_eq!(header_safe_name("match-1.log"), "match-1.log");
        assert_eq!(header_safe_name("partida-mañana.log"), "partida-ma_ana.log");
        assert_eq!(header_safe_name("a\r\nb"), "a__b");
    }

    #[test]
    fn sanitized_names_pass_header_validation() {
        let name = header_safe_name("日本語.log");
        let result = crate::transport::post(
            "ftp://x",
            b"",
            "text/plain",
            Some(&name),
            crate::transport::TransportTimeouts::default(),
        );
        // The scheme is rejected, but not as an invalid header.
        assert!(!matches!(
            result,
            Err(crate::transport::TransportError::InvalidHeader { .. })
        ));
    }

    #[test]
    fn retry_delay_saturates_and_caps() {
        assert_eq!(retry_delay(Duration::from_secs(1), 1), Duration::from_secs(1));
        assert_eq!(retry_delay(Duration::from_secs(1), 3), Duration::from_secs(4));
        assert_eq!(retry_delay(Duration::MAX, 40), MAX_BACKOFF);
        assert_eq!(retry_delay(Duration::from_secs(1), 40), MAX_BACKOFF);
        assert_eq!(retry_delay(Duration::ZERO, 40), Duration::ZERO);
    }

    #[test]
    fn reads_are_capped_while_reading() {
        assert_eq!(
            read_bounded(&b"abcd"[..], 4).expect("read"),
            Some(b"abcd".to_vec())
        );
        assert_eq!(read_bounded(&b"abcde"[..], 4).expect("read"), None);
        assert_eq!(
            read_bounded(&b"abcde"[..], 0).expect("read"),
            Some(b"abcde".to_vec())
        );
    }

    #[test]
    fn retry_delay_doubles_saturates_and_caps() {
        let base = Duration::from_secs(1);
        assert_eq!(retry_delay(base, 1), Duration::from_secs(1));
        assert_eq!(retry_delay(base, 3), Duration::from_secs(4));
        assert_eq!(retry_delay(base, 10), MAX_BACKOFF);
        // No shift/multiply overflow panic for absurd attempt numbers or backoffs.
        assert_eq!(retry_delay(base, u32::MAX), MAX_BACKOFF);
        assert_eq!(retry_delay(Duration::MAX, 40), MAX_BACKOFF);
    }

    #[test]
    fn permanent_errors_are_not_retried() {
        let reporter = CrashReporter {
            endpoint: "ftp://example.invalid/x".into(),
            consent: true,
            max_retries: 1000,
            backoff: Duration::from_secs(3600),
            ..CrashReporter::default()
        };
        // Would sleep for hours if InvalidUrl were retried.
        let result = reporter.upload_match_log("m.log", b"x");
        assert!(
            matches!(result, Err(TransportError::InvalidUrl { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn non_regular_files_are_refused() {
        let dir = regia_test_utils::unique_temp_dir("crash-capped-read");
        assert!(matches!(read_capped(&dir, 10), Err(ReadError::Unreadable)));
        let file = dir.join("a.txt");
        std::fs::write(&file, b"hello").expect("write");
        assert_eq!(read_capped(&file, 10).expect("read"), b"hello".to_vec());
        assert!(matches!(read_capped(&file, 3), Err(ReadError::TooLarge)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_capped_enforces_the_cap_and_refuses_non_regular_files() {
        let dir = regia_test_utils::unique_temp_dir("crash-read-capped");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"12345").expect("write");
        assert_eq!(read_capped(&file, 5).ok(), Some(b"12345".to_vec()));
        assert_eq!(read_capped(&file, 0).ok(), Some(b"12345".to_vec()));
        assert!(matches!(read_capped(&file, 4), Err(ReadError::TooLarge)));
        assert!(matches!(read_capped(&dir, 100), Err(ReadError::Unreadable)));
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&file, &link).expect("symlink");
            assert!(matches!(read_capped(&link, 100), Err(ReadError::Unreadable)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_rejections_cover_redirects_and_odd_codes() {
        for status in [301, 302, 400, 404, 410, 600, 999] {
            assert!(is_final_rejection(status), "{status}");
        }
        for status in [200, 204, 408, 429, 500, 503] {
            assert!(!is_final_rejection(status), "{status}");
        }
    }

    #[test]
    fn match_log_is_removed_after_2xx_and_quarantined_after_a_final_4xx() {
        let dir = regia_test_utils::unique_temp_dir("crash-match-settle");
        let ok = dir.join("ok.log");
        std::fs::write(&ok, b"x").expect("write");
        settle_match_log(&ok, &Ok(204));
        assert!(!ok.exists());

        let bad = dir.join("bad.log");
        std::fs::write(&bad, b"x").expect("write");
        settle_match_log(&bad, &Ok(404));
        assert!(!bad.exists());
        assert!(dir.join("bad.log.rejected").exists());

        let keep = dir.join("keep.log");
        std::fs::write(&keep, b"x").expect("write");
        settle_match_log(&keep, &Ok(503));
        settle_match_log(&keep, &Ok(429));
        settle_match_log(&keep, &Err(TransportError::Disabled));
        assert!(keep.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn match_log_names_are_sanitized_before_the_header_check() {
        // Disabled reporter returns before sending, so use a closed local port: the name must
        // reach the connect stage (a Connect error), not fail as InvalidHeader.
        let reporter = CrashReporter {
            endpoint: "http://127.0.0.1:1/r".into(),
            consent: true,
            max_retries: 0,
            ..CrashReporter::default()
        };
        for name in ["partida-mañana.log", "a\nb", ""] {
            let error = reporter.upload_match_log(name, b"x").expect_err("closed port");
            assert!(
                !matches!(error, TransportError::InvalidHeader { .. }),
                "{name:?}: {error}"
            );
        }
    }
}
