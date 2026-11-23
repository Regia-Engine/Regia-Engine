// SPDX-License-Identifier: MPL-2.0
//! File sink: one log file per session, named by UTC start time, keeping the newest `max_files`.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing_subscriber::fmt::MakeWriter;

/// A shared, buffered session log file. Cloning shares the same file.
#[derive(Clone)]
pub struct FileSink {
    path: Arc<PathBuf>,
    file: Arc<Mutex<BufWriter<File>>>,
}

impl std::fmt::Debug for FileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSink").field("path", &self.path).finish()
    }
}

impl FileSink {
    /// Creates `dir/<YYYY-MM-DD_HH-MM-SS>.log` and deletes the oldest session logs so that at most
    /// `max_files` remain (including the new one). Only regular files named exactly like the ones
    /// this sink creates (`<YYYY-MM-DD_HH-MM-SS>.log` or `<YYYY-MM-DD_HH-MM-SS>_<N>.log`) are
    /// counted or deleted: another program's `.log`, a symlink or a directory is never touched.
    pub fn create(dir: &Path, max_files: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (y, mo, d, h, mi, s) = civil_date_time(secs);
        let stem = format!("{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}");
        // `create_new` claims the name atomically: a second process starting in the same second
        // takes the next suffix instead of truncating this session's file (ADR-2396).
        let mut path = dir.join(format!("{stem}.log"));
        let mut n = 1u32;
        let file = loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && n < 10_000 => {
                    path = dir.join(format!("{stem}_{n}.log"));
                    n += 1;
                }
                Err(error) => return Err(error),
            }
        };
        prune(dir, max_files.max(1), &path)?;
        Ok(Self {
            path: Arc::new(path),
            file: Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    /// Path of the session file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes buffered output.
    pub fn flush(&self) {
        // A failed flush cannot be logged anywhere useful; the next write will retry.
        let _ = self.file.lock().flush();
    }
}

/// Whether `path` is a session log this sink could have created: a regular file (a symlink is
/// not followed and not owned) whose name is exactly `YYYY-MM-DD_HH-MM-SS.log` or
/// `YYYY-MM-DD_HH-MM-SS_N.log` with `N` of one to four digits (the collision counter of
/// `FileSink::create`). Every other name, `.log` or not, belongs to someone else and is kept.
fn is_owned_log(path: &Path) -> bool {
    const STAMP: &[u8; 19] = b"dddd-dd-dd_dd-dd-dd";
    let Some(stem) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    let Some((stamp, rest)) = stem.as_bytes().split_at_checked(STAMP.len()) else {
        return false;
    };
    let stamp_ok = stamp.iter().zip(STAMP).all(|(&b, &want)| {
        if want == b'd' {
            b.is_ascii_digit()
        } else {
            b == want
        }
    });
    let counter_ok = match rest {
        [] => true,
        [b'_', digits @ ..] => (1..=4).contains(&digits.len()) && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    stamp_ok && counter_ok && fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Chronological sort key of a session log name: the start-time stem, then the same-second
/// collision counter (`<stem>.log` is 0, `<stem>_N.log` is N), so `_10` sorts after `_2`.
fn log_sort_key(path: &Path) -> (String, u32) {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('_') {
        Some((base, n)) if n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() => {
            (base.to_owned(), n.parse().unwrap_or(u32::MAX))
        }
        _ => (stem, 0),
    }
}

/// Removes the oldest `*.log` files in `dir` until at most `keep` remain. Names sort chronologically
/// (see `log_sort_key`). `live`, the file this session just opened, is never removed, even when a
/// clock step backward makes its name the oldest.
fn prune(dir: &Path, keep: usize, live: &Path) -> io::Result<()> {
    let mut logs: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p != live && is_owned_log(p))
        .collect();
    logs.sort_by_key(|p| log_sort_key(p));
    // The live file counts toward `keep`.
    let excess = logs.len().saturating_sub(keep.saturating_sub(1));
    for old in &logs[..excess] {
        fs::remove_file(old)?;
    }
    Ok(())
}

/// Writer handed to `tracing-subscriber` for one event.
pub struct FileWriter<'a>(parking_lot::MutexGuard<'a, BufWriter<File>>);

impl Write for FileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for FileSink {
    type Writer = FileWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        FileWriter(self.file.lock())
    }
}

/// Converts seconds since the Unix epoch (UTC) to `(year, month, day, hour, minute, second)`.
///
/// Days-to-civil conversion after Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms"
/// (public domain description; see `docs/provenance.md`).
pub fn civil_date_time(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date_time(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_date_time(951_782_400), (2000, 2, 29, 0, 0, 0)); // leap day
        assert_eq!(civil_date_time(1_791_142_496), (2026, 10, 4, 19, 34, 56)); // checked with `date -u -d @…`
        assert_eq!(civil_date_time(4_102_444_799), (2099, 12, 31, 23, 59, 59));
    }

    #[test]
    fn ring_keeps_newest_files() {
        let dir = std::env::temp_dir().join(format!("regia-log-ring-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        for i in 0..12 {
            File::create(dir.join(format!("2026-01-{:02}_00-00-00.log", i + 1))).expect("old log");
        }
        File::create(dir.join("notes.txt")).expect("other file");
        let sink = FileSink::create(&dir, 10).expect("sink");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.iter().filter(|n| n.ends_with(".log")).count(), 10);
        assert!(
            names.contains(&"notes.txt".to_string()),
            "non-log files untouched"
        );
        assert!(
            !names.contains(&"2026-01-01_00-00-00.log".to_string()),
            "oldest removed"
        );
        assert!(sink.path().exists(), "new session file kept");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_reach_disk_after_flush() {
        let dir = std::env::temp_dir().join(format!("regia-log-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sink = FileSink::create(&dir, 10).expect("sink");
        sink.make_writer().write_all(b"hello\n").expect("write");
        sink.flush();
        assert_eq!(fs::read_to_string(sink.path()).expect("read"), "hello\n");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The count cap deletes only session logs this sink owns. A foreign `.log` that another
    /// writer holds open, a near-miss name, a directory and (on Unix) a symlink with a session
    /// name all stay, and the owned logs are still trimmed to the cap.
    #[test]
    fn count_cap_keeps_logs_it_did_not_create() {
        let dir = regia_test_utils::unique_temp_dir("log-owned-count");
        fs::write(dir.join("Cargo.toml"), b"keep").expect("cargo");
        let foreign = dir.join("build.log");
        fs::write(&foreign, b"other tool\n").expect("foreign");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&foreign)
            .expect("open foreign");
        writer.write_all(b"mid-write\n").expect("write");
        writer.flush().expect("flush");
        let keep = [
            "build.log",
            "notes.log",
            "2020-01-01_00-00-00.log.bak",
            "2020-01-01_00-00-00_12345.log",
            "2020-01-01_00-00-0x.log",
            "x2020-01-01_00-00-00.log",
        ];
        for name in &keep[1..] {
            fs::write(dir.join(name), b"keep").expect("near miss");
        }
        fs::create_dir(dir.join("2019-01-01_00-00-00.log")).expect("dir named like a log");
        fs::write(dir.join("2019-01-01_00-00-00.log").join("inner.txt"), b"keep").expect("inner");
        #[cfg(unix)]
        let outside = {
            let outside = regia_test_utils::unique_temp_dir("log-owned-count-target");
            fs::write(outside.join("precious.txt"), b"keep").expect("target");
            std::os::unix::fs::symlink(outside.join("precious.txt"), dir.join("2019-06-01_00-00-00.log"))
                .expect("link");
            outside
        };
        let owned = ["2020-01-02_00-00-00.log", "2020-01-03_00-00-00_1.log"];
        for name in owned {
            fs::write(dir.join(name), b"owned").expect("owned");
        }
        let sink = FileSink::create(&dir, 2).expect("sink");
        for name in keep {
            assert!(dir.join(name).is_file(), "{name} was deleted");
        }
        assert!(dir.join("2019-01-01_00-00-00.log/inner.txt").is_file());
        assert!(
            fs::read(&foreign)
                .expect("foreign bytes")
                .ends_with(b"mid-write\n")
        );
        assert!(!dir.join(owned[0]).exists(), "the oldest owned log stays");
        assert!(dir.join(owned[1]).is_file(), "the newest owned log was deleted");
        assert!(sink.path().is_file());
        #[cfg(unix)]
        {
            let link = dir.join("2019-06-01_00-00-00.log");
            assert!(link.symlink_metadata().is_ok(), "the symlink was unlinked");
            assert_eq!(fs::read(outside.join("precious.txt")).expect("target"), b"keep");
            let _ = fs::remove_dir_all(&outside);
        }
        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-log-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn collision_counter_sorts_numerically() {
        let dir = test_dir("numeric");
        let stem = "2026-01-01_00-00-00";
        File::create(dir.join(format!("{stem}.log"))).expect("log");
        for n in 1..=10 {
            File::create(dir.join(format!("{stem}_{n}.log"))).expect("log");
        }
        let live = dir.join("2026-01-02_00-00-00.log");
        File::create(&live).expect("live");
        prune(&dir, 3, &live).expect("prune");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                format!("{stem}_10.log"),
                format!("{stem}_9.log"),
                "2026-01-02_00-00-00.log".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_file_survives_even_when_oldest() {
        let dir = test_dir("live");
        for i in 5..9 {
            File::create(dir.join(format!("2030-01-{i:02}_00-00-00.log"))).expect("future log");
        }
        // The new session's name sorts before every existing file (clock stepped backward).
        let live = dir.join("2026-01-01_00-00-00.log");
        File::create(&live).expect("live");
        prune(&dir, 2, &live).expect("prune");
        assert!(live.exists(), "the open session file must not be unlinked");
        let logs = fs::read_dir(&dir).expect("read dir").count();
        assert_eq!(logs, 2, "live file plus the newest other file");
        let _ = fs::remove_dir_all(&dir);
    }
}
