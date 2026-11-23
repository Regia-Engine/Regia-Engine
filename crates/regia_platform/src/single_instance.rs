// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/single_instance.rs, commit c9513623f), ADR-8012.
//! One running instance per user and app (ADR-8833, absorb plan P-07).
//!
//! A game started twice by the same user (a launcher retry, a double click, a relaunch that
//! raced the old process's exit) opened a second window, shared the first one's save and cache
//! files, and could be left running unseen. [`SingleInstanceLock::acquire`] takes a per-user,
//! per-app lock when the app starts; a second start finds it held, waits up to its `wait` for
//! the first to leave (a relaunch), and is then refused with [`SingleInstanceError::AlreadyRunning`],
//! which names the holder's pid. The host prints the message and exits with
//! [`EXIT_ALREADY_RUNNING`]; on Windows the running instance's window is raised first.
//!
//! The lock is an OS file lock (`flock` on Linux and macOS, `LockFileEx` on Windows, through
//! [`std::fs::File::try_lock`]) on `<dir>/<app>.instance.lock`, where `<dir>` is per user
//! ([`instance_lock_path`]). The OS releases a file lock when its process ends, crashes
//! included, so a crashed instance never blocks the next start. The holder's pid is kept in a
//! sidecar file (`<app>.instance.lock.pid`, a separate file because Windows refuses reads of a
//! locked range). A clean release removes it, so a pid found there while the lock is free belongs
//! to an instance that died holding it: the new instance takes the lock over and reports that pid
//! ([`SingleInstanceLock::took_over_from`]). Where the file system has no locks (some network
//! shares) the pid file decides alone: a live recorded pid refuses the start, a dead one is taken
//! over.
//!
//! Android: one task per app is the OS's rule (the activity's launch mode), so
//! [`SingleInstanceLock::acquire`] takes no file lock there and returns an OS-managed guard
//! ([`SingleInstanceLock::is_os_managed`]).
//!
//! Test harnesses that start several copies at once set [`ALLOW_MULTI_INSTANCE_ENV`] to `1`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Set to anything but empty or `0` to skip the lock (test runs that start several copies).
pub const ALLOW_MULTI_INSTANCE_ENV: &str = "REGIA_ALLOW_MULTI_INSTANCE";

/// The exit code a refused second start ends with (`EX_TEMPFAIL`: try again later).
pub const EXIT_ALREADY_RUNNING: i32 = 75;

/// How often a waiting start retries the lock.
const RETRY_EVERY: Duration = Duration::from_millis(50);

/// Why [`SingleInstanceLock::acquire`] did not take the lock.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SingleInstanceError {
    /// Another live instance of this app holds the lock for this user.
    #[error("another instance is already running for this user (lock {}, held by pid {})", .path.display(), holder_text(*.holder))]
    AlreadyRunning {
        /// The lock file.
        path: PathBuf,
        /// The holder's pid, when its pid file was readable.
        holder: Option<u32>,
    },
    /// The lock file could not be created or opened.
    #[error("instance lock {}: {message}", .path.display())]
    Io {
        /// The lock file.
        path: PathBuf,
        /// The OS error.
        message: String,
    },
}

fn holder_text(holder: Option<u32>) -> String {
    holder.map_or_else(|| "unknown".to_owned(), |p| p.to_string())
}

impl SingleInstanceError {
    /// The stable lowercase code (agent replies, logs): `already_running` or `instance_lock_io`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SingleInstanceError::AlreadyRunning { .. } => "already_running",
            SingleInstanceError::Io { .. } => "instance_lock_io",
        }
    }
}

/// The held single-instance lock. Keep it alive for the whole run (a `main` local or a
/// resource); dropping it releases the lock.
#[derive(Debug)]
pub struct SingleInstanceLock {
    /// The locked file (`None`: no lock taken, see `kind`).
    file: Option<File>,
    path: Option<PathBuf>,
    took_over_from: Option<u32>,
    kind: LockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockKind {
    /// An OS file lock is held.
    Locked,
    /// The file system has no locks; the pid file alone marks the instance.
    PidOnly,
    /// [`ALLOW_MULTI_INSTANCE_ENV`] is set: no lock.
    Allowed,
    /// Android: the OS keeps one task per app.
    OsManaged,
}

impl SingleInstanceLock {
    /// Takes this user's lock for `app_id` ([`instance_lock_path`]), waiting up to `wait` for a
    /// previous instance to leave. On refusal the running instance's window is raised (Windows,
    /// best effort) and the error names its pid. With [`ALLOW_MULTI_INSTANCE_ENV`] set, or on
    /// Android, no lock is taken and this always succeeds.
    pub fn acquire(app_id: &str, wait: Duration) -> Result<Self, SingleInstanceError> {
        if multi_instance_allowed(
            regia_core::brand::env_var(ALLOW_MULTI_INSTANCE_ENV)
                .ok()
                .as_deref(),
        ) {
            tracing::info!(target: "regia_platform", "single instance: {ALLOW_MULTI_INSTANCE_ENV} is set; no lock");
            return Ok(Self::unlocked(LockKind::Allowed));
        }
        let Some(path) = instance_lock_path(app_id) else {
            return Ok(Self::unlocked(LockKind::OsManaged));
        };
        let result = Self::acquire_at(&path, wait);
        if let Err(SingleInstanceError::AlreadyRunning {
            holder: Some(pid), ..
        }) = &result
        {
            let raised = crate::platform::raise_instance_window(*pid);
            tracing::info!(target: "regia_platform", "single instance: pid {pid} holds the lock; its window raised: {raised}");
        }
        result
    }

    /// Takes the lock on `path` itself (tests, hosts with their own lock location), waiting up
    /// to `wait`. Creates the parent directory.
    pub fn acquire_at(path: &Path, wait: Duration) -> Result<Self, SingleInstanceError> {
        let io = |e: std::io::Error| SingleInstanceError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(io)?;
        let pid_path = pid_path(path);
        let me = std::process::id();
        let start = Instant::now();
        loop {
            let attempt = match file.try_lock() {
                Ok(()) => Attempt::Locked,
                Err(std::fs::TryLockError::WouldBlock) => Attempt::Held,
                Err(std::fs::TryLockError::Error(err)) => {
                    tracing::warn!(
                        target: "regia_platform",
                        "single instance: {} cannot be locked ({err}); the pid file decides",
                        path.display()
                    );
                    Attempt::NoLocks
                }
            };
            let recorded = read_pid(&pid_path).filter(|pid| *pid != me);
            let alive = recorded.and_then(crate::platform::process_alive);
            match decide(attempt, recorded, alive) {
                Decision::Take { took_over_from } => {
                    write_pid(&pid_path, me).map_err(io)?;
                    if let Some(old) = took_over_from {
                        tracing::warn!(
                            target: "regia_platform",
                            "single instance: took over the lock {} from pid {old}, which ended without releasing it",
                            path.display()
                        );
                    }
                    let kind = if attempt == Attempt::NoLocks {
                        LockKind::PidOnly
                    } else {
                        LockKind::Locked
                    };
                    return Ok(Self {
                        file: Some(file),
                        path: Some(path.to_path_buf()),
                        took_over_from,
                        kind,
                    });
                }
                Decision::Refuse => {
                    if start.elapsed() >= wait {
                        return Err(SingleInstanceError::AlreadyRunning {
                            path: path.to_path_buf(),
                            holder: recorded,
                        });
                    }
                    std::thread::sleep(RETRY_EVERY);
                }
            }
        }
    }

    fn unlocked(kind: LockKind) -> Self {
        Self {
            file: None,
            path: None,
            took_over_from: None,
            kind,
        }
    }

    /// The lock file (`None` when no lock was taken).
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The pid of an earlier instance that ended (crashed, was killed) while holding the lock,
    /// whose stale record this instance took over.
    #[must_use]
    pub fn took_over_from(&self) -> Option<u32> {
        self.took_over_from
    }

    /// Android: no lock was taken because the OS keeps one task per app.
    #[must_use]
    pub fn is_os_managed(&self) -> bool {
        self.kind == LockKind::OsManaged
    }

    /// An OS file lock (or, where the file system has none, the pid record) guards this instance.
    #[must_use]
    pub fn is_held(&self) -> bool {
        matches!(self.kind, LockKind::Locked | LockKind::PidOnly)
    }

    /// The guard's state as a stable lowercase name: `locked`, `pid_only`, `allowed_multi` or
    /// `os_managed`.
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self.kind {
            LockKind::Locked => "locked",
            LockKind::PidOnly => "pid_only",
            LockKind::Allowed => "allowed_multi",
            LockKind::OsManaged => "os_managed",
        }
    }
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        // The pid record goes first, while the lock is still held: a pid file next to a free
        // lock then always means an instance that died holding it.
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(pid_path(path));
        }
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

/// One look at the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// The OS lock is ours now.
    Locked,
    /// Another open file holds the OS lock.
    Held,
    /// The file system does not support locks.
    NoLocks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Take { took_over_from: Option<u32> },
    Refuse,
}

/// The lock decision from one attempt, the pid recorded by another process (if any) and
/// whether that process is alive (`None`: unknown).
fn decide(attempt: Attempt, recorded: Option<u32>, alive: Option<bool>) -> Decision {
    match attempt {
        // A free lock with another pid on record: that instance died holding it.
        Attempt::Locked => Decision::Take {
            took_over_from: recorded,
        },
        Attempt::Held => Decision::Refuse,
        Attempt::NoLocks => match (recorded, alive) {
            (None, _) => Decision::Take { took_over_from: None },
            (Some(pid), Some(false)) => Decision::Take {
                took_over_from: Some(pid),
            },
            // Alive, or unknown: refusing a start is safer than two instances on one save.
            (Some(_), _) => Decision::Refuse,
        },
    }
}

/// [`ALLOW_MULTI_INSTANCE_ENV`]'s value turns the lock off: anything but unset, empty or `0`.
#[must_use]
pub fn multi_instance_allowed(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.trim().is_empty() && v.trim() != "0")
}

/// This user's lock file for `app_id`: `<dir>/<app>.instance.lock`, with `<dir>` from the
/// platform seam: `$XDG_RUNTIME_DIR` on Linux (else the temp dir, with the uid in the
/// name), the per-user `$TMPDIR` on macOS, `%LOCALAPPDATA%\RegiaEngine\instance` on Windows.
/// `None` on Android (the OS keeps one task per app). `app_id` is reduced to ASCII letters,
/// digits, `.`, `_` and `-` (others become `_`, at most 64 characters).
#[must_use]
pub fn instance_lock_path(app_id: &str) -> Option<PathBuf> {
    let (dir, prefix) = crate::platform::instance_lock_dir()?;
    Some(dir.join(format!("{prefix}{}.instance.lock", sanitize_app_id(app_id))))
}

fn sanitize_app_id(app_id: &str) -> String {
    let name: String = app_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    // A name of dots only would be `.` / `..` path parts.
    if name.chars().all(|c| c == '.') {
        "app".to_owned()
    } else {
        name
    }
}

fn pid_path(lock: &Path) -> PathBuf {
    let mut name = lock.as_os_str().to_owned();
    name.push(".pid");
    PathBuf::from(name)
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|p| *p != 0)
}

/// Writes the pid record atomically (temp file + rename), so a reader never sees half a number.
fn write_pid(path: &Path, pid: u32) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{pid}.tmp"));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, pid.to_string())?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_lock_with_a_stale_pid_is_taken_over() {
        assert_eq!(
            decide(Attempt::Locked, Some(41), None),
            Decision::Take {
                took_over_from: Some(41)
            }
        );
        assert_eq!(
            decide(Attempt::Locked, None, None),
            Decision::Take { took_over_from: None }
        );
        assert_eq!(decide(Attempt::Held, Some(41), Some(true)), Decision::Refuse);
        assert_eq!(decide(Attempt::Held, None, None), Decision::Refuse);
    }

    #[test]
    fn without_file_locks_the_pid_record_decides() {
        assert_eq!(
            decide(Attempt::NoLocks, None, None),
            Decision::Take { took_over_from: None }
        );
        assert_eq!(
            decide(Attempt::NoLocks, Some(7), Some(false)),
            Decision::Take {
                took_over_from: Some(7)
            }
        );
        assert_eq!(decide(Attempt::NoLocks, Some(7), Some(true)), Decision::Refuse);
        assert_eq!(
            decide(Attempt::NoLocks, Some(7), None),
            Decision::Refuse,
            "an unknown liveness refuses"
        );
    }

    #[test]
    fn allow_multi_values() {
        assert!(!multi_instance_allowed(None));
        assert!(!multi_instance_allowed(Some("")));
        assert!(!multi_instance_allowed(Some(" 0 ")));
        assert!(multi_instance_allowed(Some("1")));
        assert!(multi_instance_allowed(Some("yes")));
    }

    #[test]
    fn app_ids_become_safe_file_names() {
        assert_eq!(sanitize_app_id("my-game.v2_x"), "my-game.v2_x");
        assert_eq!(sanitize_app_id("a/b\\c:d e"), "a_b_c_d_e");
        assert_eq!(sanitize_app_id(".."), "app");
        assert_eq!(sanitize_app_id(""), "app");
        assert_eq!(sanitize_app_id(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn the_lock_path_is_per_user_and_per_app() {
        let (a, b) = (instance_lock_path("game-a"), instance_lock_path("game-b"));
        if cfg!(target_os = "android") {
            assert!(a.is_none());
            return;
        }
        let (a, b) = (a.expect("desktop path"), b.expect("desktop path"));
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with("game-a.instance.lock"), "{a:?}");
        assert_eq!(pid_path(&a).extension().and_then(|e| e.to_str()), Some("pid"));
    }

    #[test]
    fn the_error_codes_are_lowercase_and_stable() {
        let refused = SingleInstanceError::AlreadyRunning {
            path: PathBuf::from("x.lock"),
            holder: Some(12),
        };
        assert_eq!(refused.code(), "already_running");
        assert!(refused.to_string().contains("pid 12"), "{refused}");
        let io = SingleInstanceError::Io {
            path: PathBuf::from("x.lock"),
            message: "denied".into(),
        };
        assert_eq!(io.code(), "instance_lock_io");
    }
}
