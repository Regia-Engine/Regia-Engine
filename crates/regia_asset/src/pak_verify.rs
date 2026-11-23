// SPDX-License-Identifier: MPL-2.0
//! Pak integrity check: the header's `content_hash` (blake3 of everything before the TOC, with
//! the header bytes zeroed, as the writer hashed it) against the file on disk.
//!
//! Loading never waits for it. [`spawn_pak_verify`] hashes in a detached, low-priority thread
//! (nice 19 on Linux, plus the idle IO class where the IO scheduler honours it (BFQ; NVMe's
//! default `none` ignores it); background mode on Windows), throttled to 64 MiB/s on every
//! platform and started 60 s after the mount, and only on the first
//! launch after a pak or the game build changed: a verified `(build, path, size, mtime, hash)`
//! is recorded in the cache directory and skipped next time. A mismatch logs an error telling
//! the player to verify or reinstall the game files and notes a `pak_corrupt` telemetry event;
//! it never stops the game. [`verify_pak_file`] is the synchronous check for tools
//! (`regia doctor --verify-paks`).
//!
//! The thread opens the pak with ordinary reads (not the mapping), so a file truncated
//! underneath it is reported instead of raising SIGBUS, and it holds nothing but a file handle:
//! the process may exit at any point (the record is written by temp file + rename).
//!
//! **Zero-copy gate.** A pak that is not in the record ([`is_recorded`]: the first launch after
//! an update) mounts with its [`ZeroCopyGate`] closed: its loads copy entries through the file
//! handle instead of keeping views of the mapping, so a pak truncated in place cannot raise
//! SIGBUS through a long-lived asset. [`spawn_pak_verify_gated`] opens the gate when the hash
//! matches (or the pak carries none); a mismatch or a read failure keeps it closed for the run.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::pak::{PakError, PakHeader, PakReader, ZeroCopyGate};

/// Environment variable: `0`/`off` disables the background check; `force` re-hashes every pak
/// now (no start delay, the record is ignored).
pub const PAK_VERIFY_ENV: &str = "REGIA_PAK_VERIFY";

/// File in the cache directory that lists the paks verified with this build.
pub const PAK_VERIFY_RECORD: &str = "pak-verified.txt";

/// Bytes of the header the writer zeroed while hashing.
const HEADER_LEN: usize = 60;
/// Read size of the hashing loop.
const CHUNK: usize = 4 << 20;

/// Result of checking one pak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PakIntegrity {
    /// The content matches the header's hash.
    Ok,
    /// The pak carries no hash (all zero: written by a tool that does not hash); nothing to check.
    Unhashed,
    /// The content does not match: the file is corrupt, truncated or was modified.
    Mismatch {
        /// Hash in the header (hex).
        expected: String,
        /// Hash of the bytes on disk (hex), or why none could be computed (truncated).
        actual: String,
    },
}

/// Hashes the pak at `path` and compares it with its header (synchronous; reads the whole
/// content once). For tools; the game uses [`spawn_pak_verify`].
pub fn verify_pak_file(path: &Path) -> Result<PakIntegrity, PakError> {
    check(path, None)
}

/// Options of [`spawn_pak_verify`].
#[derive(Debug, Clone)]
pub struct PakVerifyOptions {
    /// Record of verified paks; `None` re-hashes on every launch.
    pub record: Option<PathBuf>,
    /// Wait this long before the first read, so the initial map load has the disk to itself.
    pub start_delay: Duration,
    /// Read at most this many bytes per second (`0` = unthrottled).
    pub bytes_per_sec: u64,
    /// Ignore the record: hash every pak.
    pub force: bool,
}

impl Default for PakVerifyOptions {
    fn default() -> Self {
        Self {
            record: default_record_path(),
            start_delay: Duration::from_secs(60),
            bytes_per_sec: 64 << 20,
            force: false,
        }
    }
}

impl PakVerifyOptions {
    /// The defaults adjusted by [`PAK_VERIFY_ENV`]; `None` when the check is turned off, or
    /// when there is no place for the record (Android): without one every launch would re-hash
    /// every pak.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let mut options = Self::default();
        options.record.as_ref()?;
        match regia_core::brand::env_var(PAK_VERIFY_ENV)
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0" | "off" | "false") => return None,
            Some("force") => {
                options.force = true;
                options.start_delay = Duration::ZERO;
            }
            _ => {}
        }
        Some(options)
    }
}

/// The record's default location: `cache://pak-verified.txt` (desktop only).
#[must_use]
pub fn default_record_path() -> Option<PathBuf> {
    cache_dir().map(|d| d.join(PAK_VERIFY_RECORD))
}

#[cfg(not(target_os = "android"))]
fn cache_dir() -> Option<PathBuf> {
    regia_platform::paths::cache_dir()
}

#[cfg(target_os = "android")]
fn cache_dir() -> Option<PathBuf> {
    None
}

/// What the background check did with one pak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PakVerifyOutcome {
    /// Hashed now; it matched (and was recorded).
    Verified,
    /// Verified on an earlier launch with this build and unchanged since: not re-hashed.
    AlreadyVerified,
    /// The pak carries no hash.
    Unhashed,
    /// Hashed now; corrupt. Logged as an error and noted for telemetry.
    Mismatch {
        /// Hash in the header (hex).
        expected: String,
        /// Hash on disk (hex) or why none could be computed.
        actual: String,
    },
    /// The pak could not be read.
    Failed(String),
}

/// Results of a background check, filled as it goes.
#[derive(Debug, Default)]
struct Progress {
    results: Vec<(PathBuf, PakVerifyOutcome)>,
    done: bool,
}

/// Handle to a running background check. Dropping it does not stop or join the thread (it is
/// detached and holds no engine resources).
#[derive(Debug, Clone)]
pub struct PakVerifyHandle {
    progress: Arc<(Mutex<Progress>, Condvar)>,
}

impl PakVerifyHandle {
    /// Waits up to `timeout` for the check to finish; the outcomes so far and whether it finished.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> (Vec<(PathBuf, PakVerifyOutcome)>, bool) {
        let (lock, cv) = &*self.progress;
        let deadline = Instant::now() + timeout;
        let mut progress = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !progress.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            progress = cv
                .wait_timeout(progress, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        (progress.results.clone(), progress.done)
    }
}

/// Whether `path` passed the whole-file check with this game build on an earlier launch and is
/// unchanged since (its record line is present). `false` with no record or with `force`.
#[must_use]
pub fn is_recorded(path: &Path, options: &PakVerifyOptions) -> bool {
    if options.force {
        return false;
    }
    let Some(record) = &options.record else {
        return false;
    };
    record_key(path, &build_stamp()).is_ok_and(|key| read_record(record).contains(&key))
}

/// Closes the [`ZeroCopyGate`] of a mapped, hashed pak that has not passed the whole-file check
/// with this build (the first launch after an update) and returns the gate for
/// [`spawn_pak_verify_gated`] to open. `None` when the pak may serve views of its mapping now: it
/// is not mapped, carries no hash, was verified on an earlier launch, or the check is off
/// (`verify` is `None`).
///
/// The asset server applies this when it mounts a pak; any other reader that opens the same paks
/// itself (the terrain tile source) applies it too, so no reader serves views of an unverified
/// pak.
pub fn gate_until_verified(
    reader: &PakReader,
    path: &Path,
    verify: Option<&PakVerifyOptions>,
) -> Option<ZeroCopyGate> {
    let options = verify?;
    if !reader.is_mapped() || reader.header.content_hash == [0u8; 32] || is_recorded(path, options) {
        return None;
    }
    reader.set_zero_copy(false);
    tracing::info!(
        target: "regia_asset",
        pak = %path.display(),
        "pak not yet verified with this build: loads copy its entries until the background check passes"
    );
    Some(reader.zero_copy_gate())
}

/// Starts the background integrity check of `paks` (see the module docs). Returns `None` when
/// there is nothing to check or the thread could not start (the game is unaffected).
pub fn spawn_pak_verify(paks: Vec<PathBuf>, options: PakVerifyOptions) -> Option<PakVerifyHandle> {
    spawn_pak_verify_gated(paks.into_iter().map(|p| (p, None)).collect(), options)
}

/// [`spawn_pak_verify`] that also opens each pak's [`ZeroCopyGate`] (when given) once the pak is
/// verified, now or on an earlier launch, or carries no hash. A mismatch or a read failure
/// leaves the gate closed. When the thread cannot start the gates stay closed: loads keep
/// copying, which is safe.
pub fn spawn_pak_verify_gated(
    paks: Vec<(PathBuf, Option<ZeroCopyGate>)>,
    options: PakVerifyOptions,
) -> Option<PakVerifyHandle> {
    if paks.is_empty() {
        return None;
    }
    let progress = Arc::new((Mutex::new(Progress::default()), Condvar::new()));
    let thread_progress = Arc::clone(&progress);
    let spawned = std::thread::Builder::new()
        .name("regia-pak-verify".into())
        .spawn(move || {
            let _ = regia_platform::background::lower_current_thread_priority();
            if !options.start_delay.is_zero() {
                std::thread::sleep(options.start_delay);
            }
            let build = build_stamp();
            for (path, gate) in paks {
                let outcome = verify_one(&path, &options, &build);
                if let Some(gate) = gate
                    && matches!(
                        outcome,
                        PakVerifyOutcome::Verified
                            | PakVerifyOutcome::AlreadyVerified
                            | PakVerifyOutcome::Unhashed
                    )
                {
                    gate.open();
                    tracing::info!(
                        target: "regia_asset",
                        pak = %path.display(),
                        "pak verified: loads from it are zero-copy from now on"
                    );
                }
                let (lock, _) = &*thread_progress;
                lock.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .results
                    .push((path, outcome));
            }
            let (lock, cv) = &*thread_progress;
            lock.lock().unwrap_or_else(PoisonError::into_inner).done = true;
            cv.notify_all();
        });
    match spawned {
        Ok(_detached) => Some(PakVerifyHandle { progress }),
        Err(e) => {
            tracing::warn!(target: "regia_asset", "pak integrity check not started: {e}");
            None
        }
    }
}

fn verify_one(path: &Path, options: &PakVerifyOptions, build: &str) -> PakVerifyOutcome {
    // No tracing span here: `FrameProfiler` books spans from every thread into the frame they
    // end in, and a seconds-long span would read as the frame's top CPU cost. The log line
    // below carries the duration.
    let key = match record_key(path, build) {
        Ok(key) => key,
        Err(e) => return PakVerifyOutcome::Failed(e.to_string()),
    };
    if !options.force
        && let Some(record) = &options.record
        && read_record(record).contains(&key)
    {
        return PakVerifyOutcome::AlreadyVerified;
    }
    let started = Instant::now();
    match check(path, Some(options.bytes_per_sec)) {
        Ok(PakIntegrity::Ok) => {
            tracing::info!(
                target: "regia_asset",
                pak = %path.display(),
                ms = started.elapsed().as_millis() as u64,
                "pak integrity verified"
            );
            if let Some(record) = &options.record
                && let Err(e) = add_to_record(record, &key, build)
            {
                tracing::debug!(target: "regia_asset", "pak verify record not written: {e}");
            }
            PakVerifyOutcome::Verified
        }
        Ok(PakIntegrity::Unhashed) => PakVerifyOutcome::Unhashed,
        Ok(PakIntegrity::Mismatch { expected, actual }) => {
            tracing::error!(
                target: "regia_asset",
                pak = %path.display(),
                expected = %expected,
                actual = %actual,
                "game data file {} is corrupt (content hash mismatch): verify the game files or reinstall the game",
                path.display()
            );
            regia_core::telemetry::note(
                regia_core::telemetry::NoteKind::PakCorrupt,
                &format!("{} whole-file hash", path.display()),
                -1.0,
            );
            PakVerifyOutcome::Mismatch { expected, actual }
        }
        Err(e) => {
            tracing::warn!(
                target: "regia_asset",
                pak = %path.display(),
                error = %e,
                "pak integrity check could not read the file"
            );
            PakVerifyOutcome::Failed(e.to_string())
        }
    }
}

/// Hashes `path`'s content like the writer did; throttled to `bytes_per_sec` when given.
fn check(path: &Path, bytes_per_sec: Option<u64>) -> Result<PakIntegrity, PakError> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; HEADER_LEN];
    file.read_exact(&mut head)?;
    let header = PakHeader::from_bytes(&head)?;
    if header.content_hash == [0u8; 32] {
        return Ok(PakIntegrity::Unhashed);
    }
    let expected = hex(&header.content_hash);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[0u8; HEADER_LEN]);
    let mut left = header.toc_offset.saturating_sub(HEADER_LEN as u64);
    let mut buf = vec![0u8; CHUNK];
    let started = Instant::now();
    let mut read_total = 0u64;
    while left > 0 {
        let want = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
        let n = match file.read(&mut buf[..want]) {
            Ok(0) => {
                return Ok(PakIntegrity::Mismatch {
                    expected,
                    actual: format!(
                        "truncated: content ends at byte {} of {}",
                        HEADER_LEN as u64 + read_total,
                        header.toc_offset
                    ),
                });
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        hasher.update(&buf[..n]);
        left -= n as u64;
        read_total += n as u64;
        if let Some(rate) = bytes_per_sec.filter(|&r| r > 0) {
            let due = Duration::from_secs_f64(read_total as f64 / rate as f64);
            let elapsed = started.elapsed();
            if due > elapsed {
                std::thread::sleep(due - elapsed);
            }
        }
    }
    let actual = hex(hasher.finalize().as_bytes());
    if actual == expected {
        Ok(PakIntegrity::Ok)
    } else {
        Ok(PakIntegrity::Mismatch { expected, actual })
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Identity of this game build: engine version plus the executable's size and mtime (changes
/// with every rebuilt or updated binary).
fn build_stamp() -> String {
    let exe = std::env::current_exe()
        .and_then(|p| p.metadata())
        .map(|m| format!("{}:{}", m.len(), mtime_ns(&m)))
        .unwrap_or_else(|_| "unknown-exe".into());
    format!("{}:{exe}", env!("CARGO_PKG_VERSION"))
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// One record line: build, size, mtime, header hash, path (tab separated; the path last).
fn record_key(path: &Path, build: &str) -> io::Result<String> {
    let meta = std::fs::metadata(path)?;
    let mut head = [0u8; HEADER_LEN];
    std::fs::File::open(path)?.read_exact(&mut head)?;
    let hash = PakHeader::from_bytes(&head)
        .map(|h| hex(&h.content_hash))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(format!(
        "{build}\t{}\t{}\t{hash}\t{}",
        meta.len(),
        mtime_ns(&meta),
        path.display()
    ))
}

fn read_record(record: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(record)
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Adds `key` to the record (atomically: temp file + rename) and forgets lines of other builds.
///
/// The read-modify-write is serialized within the process and the temp name carries the pid and
/// a counter (audit concurrency-1103 L1: two verify threads shared `tmp-<pid>` and lost lines).
/// Two processes sharing one record still race last-writer-wins; a lost line only costs that pak
/// one more hash on the next launch.
fn add_to_record(record: &Path, key: &str, build: &str) -> io::Result<()> {
    static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let _serial = RECORD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prefix = format!("{build}\t");
    let mut lines = read_record(record);
    // One line per pak path: a rewritten pak replaces its older line instead of adding one.
    let path_of = |line: &str| line.splitn(5, '\t').nth(4).map(str::to_owned);
    let new_path = path_of(key);
    lines.retain(|l| l.starts_with(&prefix) && (new_path.is_none() || path_of(l) != new_path));
    lines.insert(key.to_owned());
    if let Some(dir) = record.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    let n = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = record.with_extension(format!("tmp-{}-{n}", std::process::id()));
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, record).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pak::{FPAK_VERSION, PakToc, encode_pak_header};

    /// A minimal pak written like `regia_asset_pipeline::pak_writer` (zeroed header while
    /// hashing, content up to the TOC).
    fn write_pak(path: &Path) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_LEN];
        data.resize(4096, 0);
        data.extend((0..20_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
        let toc_offset = data.len() as u64;
        let toc = PakToc {
            entries: Vec::new(),
            manifest: Vec::new(),
            dependencies: Vec::new(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
        data.extend_from_slice(&toc_bytes);
        let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
        let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
        data[..HEADER_LEN].copy_from_slice(&header);
        std::fs::write(path, &data).expect("write pak");
        data
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-pak-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn options(record: &Path) -> PakVerifyOptions {
        PakVerifyOptions {
            record: Some(record.to_path_buf()),
            start_delay: Duration::ZERO,
            bytes_per_sec: 0,
            force: false,
        }
    }

    fn run(paks: Vec<PathBuf>, options: PakVerifyOptions) -> Vec<(PathBuf, PakVerifyOutcome)> {
        let handle = spawn_pak_verify(paks, options).expect("thread");
        let (results, done) = handle.wait(Duration::from_secs(30));
        assert!(done, "check finished");
        results
    }

    #[test]
    fn intact_pak_verifies_and_corruption_is_detected() {
        let dir = temp_dir("sync");
        let path = dir.join("base.fpak");
        let mut data = write_pak(&path);
        assert_eq!(verify_pak_file(&path).expect("verify"), PakIntegrity::Ok);
        // Flip one content byte.
        data[5000] ^= 0x40;
        std::fs::write(&path, &data).expect("corrupt");
        assert!(matches!(
            verify_pak_file(&path).expect("verify"),
            PakIntegrity::Mismatch { .. }
        ));
        // Truncated inside the content.
        std::fs::write(&path, &data[..8000]).expect("truncate");
        match verify_pak_file(&path).expect("verify") {
            PakIntegrity::Mismatch { actual, .. } => assert!(actual.contains("truncated"), "{actual}"),
            other => panic!("expected a mismatch, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_hash_is_unhashed() {
        let dir = temp_dir("unhashed");
        let path = dir.join("tool.fpak");
        let mut data = write_pak(&path);
        data[28..60].fill(0);
        std::fs::write(&path, &data).expect("write");
        assert_eq!(verify_pak_file(&path).expect("verify"), PakIntegrity::Unhashed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The background check hashes a pak once, records it, skips it on the next launch, and
    /// detects a pak that changed (corrupted) after it was recorded.
    #[test]
    fn unchanged_pak_is_not_rehashed_and_a_corrupted_one_is_detected() {
        let dir = temp_dir("background");
        let path = dir.join("base.fpak");
        let record = dir.join("cache").join(PAK_VERIFY_RECORD);
        let mut data = write_pak(&path);

        let first = run(vec![path.clone()], options(&record));
        assert_eq!(first, vec![(path.clone(), PakVerifyOutcome::Verified)]);
        assert!(record.exists(), "record written");

        let second = run(vec![path.clone()], options(&record));
        assert_eq!(second, vec![(path.clone(), PakVerifyOutcome::AlreadyVerified)]);

        // Same size, one byte flipped, a different mtime (a patcher or bad sector rewrite).
        data[6000] ^= 0x01;
        std::fs::write(&path, &data).expect("corrupt");
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .expect("mtime");
        drop(file);
        let third = run(vec![path.clone()], options(&record));
        assert!(
            matches!(third.as_slice(), [(_, PakVerifyOutcome::Mismatch { .. })]),
            "{third:?}"
        );
        // A mismatch is never recorded: the next launch checks again.
        let fourth = run(vec![path.clone()], options(&record));
        assert!(
            matches!(fourth.as_slice(), [(_, PakVerifyOutcome::Mismatch { .. })]),
            "{fourth:?}"
        );
        // `force` ignores the record.
        write_pak(&path);
        let forced = run(
            vec![path.clone()],
            PakVerifyOptions {
                force: true,
                ..options(&record)
            },
        );
        assert_eq!(forced, vec![(path.clone(), PakVerifyOutcome::Verified)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gated check opens a pak's zero-copy gate when its hash matches and records it; a pak
    /// that changed since is not recorded any more, and its gate stays closed on a mismatch.
    #[test]
    fn the_gated_check_opens_the_gate_only_for_a_matching_pak() {
        let dir = temp_dir("gated");
        let path = dir.join("base.fpak");
        let record = dir.join("cache").join(PAK_VERIFY_RECORD);
        let mut data = write_pak(&path);
        assert!(!is_recorded(&path, &options(&record)));

        let gate = ZeroCopyGate::new(false);
        let handle = spawn_pak_verify_gated(vec![(path.clone(), Some(gate.clone()))], options(&record))
            .expect("thread");
        assert!(handle.wait(Duration::from_secs(30)).1);
        assert!(gate.is_open());
        assert!(is_recorded(&path, &options(&record)));
        assert!(
            !is_recorded(
                &path,
                &PakVerifyOptions {
                    force: true,
                    ..options(&record)
                }
            ),
            "force re-checks"
        );

        data[6000] ^= 0x01;
        std::fs::write(&path, &data).expect("corrupt");
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .expect("mtime");
        drop(file);
        assert!(
            !is_recorded(&path, &options(&record)),
            "a changed pak is not recorded"
        );
        let gate = ZeroCopyGate::new(false);
        let handle = spawn_pak_verify_gated(vec![(path.clone(), Some(gate.clone()))], options(&record))
            .expect("thread");
        let (results, done) = handle.wait(Duration::from_secs(30));
        assert!(done);
        assert!(matches!(results[0].1, PakVerifyOutcome::Mismatch { .. }));
        assert!(!gate.is_open(), "a corrupt pak keeps serving copies");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn throttle_limits_the_read_rate() {
        let dir = temp_dir("throttle");
        let path = dir.join("base.fpak");
        let data = write_pak(&path);
        let started = Instant::now();
        // ~24 KB of content at 100 KB/s: at least ~0.2 s.
        assert_eq!(check(&path, Some(100_000)).expect("check"), PakIntegrity::Ok);
        let content = data.len() as f64 / 100_000.0;
        assert!(
            started.elapsed().as_secs_f64() >= content * 0.5,
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_record_keeps_one_line_per_pak_path() {
        let dir = regia_test_utils::unique_temp_dir("pak-verify-record");
        let record = dir.join("pak-verified.txt");
        let build = "0.1:1:2";
        let key = |size: u32, path: &str| format!("{build}\t{size}\t5\tabcd\t{path}");
        add_to_record(&record, &key(10, "/p/a.fpak"), build).expect("first");
        add_to_record(&record, &key(11, "/p/a.fpak"), build).expect("rewritten pak");
        add_to_record(&record, &key(7, "/p/b.fpak"), build).expect("other pak");
        let lines = read_record(&record);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.contains(&key(11, "/p/a.fpak")));
        assert!(lines.contains(&key(7, "/p/b.fpak")));
        // A failed rename (the record path is a directory) leaves no temp file behind.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(&blocked).expect("dir");
        assert!(add_to_record(&blocked, &key(1, "/p/c.fpak"), build).is_err());
        let temps = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(temps, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit concurrency-1103 asset L1: two verify threads finishing at once shared the temp
    /// name `tmp-<pid>` and an unguarded read-modify-write, so one pak's line was lost (or a
    /// rename failed). Both lines survive.
    #[test]
    fn concurrent_record_writes_keep_every_line() {
        let dir = regia_test_utils::unique_temp_dir("pak-verify-record-race");
        let record = dir.join("pak-verified.txt");
        let build = "0.1:1:2";
        let mut lost = Vec::new();
        for round in 0..100 {
            let _ = std::fs::remove_file(&record);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = ["/p/a.fpak", "/p/b.fpak"]
                .into_iter()
                .map(|path| {
                    let barrier = barrier.clone();
                    let record = record.clone();
                    std::thread::spawn(move || {
                        let key = format!("{build}\t{round}\t5\tabcd\t{path}");
                        barrier.wait();
                        add_to_record(&record, &key, build).map(|()| key)
                    })
                })
                .collect();
            let keys: Vec<_> = handles.into_iter().map(|h| h.join().expect("thread")).collect();
            let lines = read_record(&record);
            for key in keys {
                match key {
                    Ok(key) if lines.contains(&key) => {}
                    other => lost.push(format!("round {round}: {other:?}")),
                }
            }
        }
        assert!(lost.is_empty(), "{lost:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
