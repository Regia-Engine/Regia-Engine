// SPDX-License-Identifier: MPL-2.0
//! A loaded host on demand (`do27_chase --memory-squeeze`, `SPAWN_SOAK_SQUEEZE` in
//! `samples/arena/tests/spawn_hitch.rs`): a thread that keeps asking the kernel to reclaim this
//! process's memory, so cold pages (rarely run code, rarely read data) are dropped or swapped
//! out as they are on a box short of memory. The next frame that needs one sleeps in a major
//! page fault: the stall the hour soaks on a saturated farm showed at effect spawns
//! (`docs/evidence/soak/2026-10-08-1551/`).
//!
//! Linux with cgroup v2: writes `<MB>M` to the process cgroup's `memory.reclaim` (proactive
//! reclaim, kernel 5.19+) every period. The kernel takes the least recently used pages of the
//! cgroup, as it would under pressure: file-backed and anonymous, or file-backed only
//! (`swappiness=0`, kernel 6.11+). Too much per period evicts the frame's own working set and
//! every frame thrashes; a few tens of MiB per second ages out what has not run for seconds.
//! Elsewhere it does nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How hard the squeeze presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqueezeSettings {
    /// MiB asked of the kernel per period.
    pub mb: u32,
    /// Time between requests.
    pub period: Duration,
    /// Reclaim page cache only (code, mapped files), never swap the heap out.
    pub file_only: bool,
}

impl SqueezeSettings {
    /// Parses `MB[:PERIOD_MS][:file]` (`"32:1000:file"`; the period defaults to 250 ms).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let mb = parts.next()?.trim().parse().ok()?;
        let mut settings = Self {
            mb,
            period: Duration::from_millis(250),
            file_only: false,
        };
        for part in parts {
            match part.trim() {
                "file" => settings.file_only = true,
                ms => settings.period = Duration::from_millis(ms.parse().ok()?),
            }
        }
        Some(settings)
    }
}

/// The running squeeze; stops when dropped.
pub struct MemorySqueeze {
    stop: Arc<AtomicBool>,
    rounds: Arc<AtomicU64>,
    /// The worker, woken by `Drop` so a long period does not keep it parked (ADR-2396).
    thread: std::thread::Thread,
}

impl MemorySqueeze {
    /// Starts reclaiming. `None` when the cgroup has no writable `memory.reclaim` (not Linux,
    /// cgroup v1, or not delegated to this user).
    #[must_use]
    pub fn start(settings: SqueezeSettings) -> Option<Self> {
        let path = reclaim_path()?;
        // A reclaim the kernel cannot complete fails with EAGAIN; only an unwritable file
        // means the squeeze cannot run.
        if std::fs::OpenOptions::new().write(true).open(&path).is_err() {
            return None;
        }
        let request = if settings.file_only {
            format!("{}M swappiness=0", settings.mb)
        } else {
            format!("{}M", settings.mb)
        };
        Self::spawn(settings.period, move || {
            let _ = std::fs::write(&path, &request);
        })
    }

    /// Runs `round` every `period` (at least 10 ms) on a thread until the squeeze is dropped.
    fn spawn(period: Duration, mut round: impl FnMut() + Send + 'static) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicU64::new(0));
        let (stop_t, rounds_t) = (Arc::clone(&stop), Arc::clone(&rounds));
        let period = period.max(Duration::from_millis(10));
        let handle = std::thread::Builder::new()
            .name("memory-squeeze".into())
            .spawn(move || {
                while !stop_t.load(Ordering::Acquire) {
                    round();
                    rounds_t.fetch_add(1, Ordering::Relaxed);
                    std::thread::park_timeout(period);
                }
            })
            .ok()?;
        // Detached: only the `Thread` handle is kept, to wake it.
        let thread = handle.thread().clone();
        Some(Self { stop, rounds, thread })
    }

    /// Reclaim requests made so far.
    #[must_use]
    pub fn rounds(&self) -> u64 {
        self.rounds.load(Ordering::Relaxed)
    }
}

impl Drop for MemorySqueeze {
    /// Asks the thread to stop and returns at once. It is not joined: it may be inside a
    /// `memory.reclaim` write, which blocks until the kernel finishes reclaiming (up to the
    /// configured MiB), and it keeps nothing the caller needs. It ends after that write; a thread
    /// parked between rounds is woken and ends at once.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// `memory.reclaim` of this process's cgroup (v2).
fn reclaim_path() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let path = std::path::Path::new("/sys/fs/cgroup")
        .join(rel.trim().trim_start_matches('/'))
        .join("memory.reclaim");
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the squeeze does not wait for a round that is still inside a blocking reclaim
    /// write; the thread ends once the round returns.
    #[test]
    fn drop_does_not_wait_for_a_blocked_round() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let squeeze = MemorySqueeze::spawn(Duration::from_millis(10), move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("thread");
        entered_rx.recv().expect("round started");
        let rounds = Arc::clone(&squeeze.rounds);
        drop(squeeze); // would hang here if Drop joined
        assert_eq!(rounds.load(Ordering::Relaxed), 0);
        release_tx.send(()).expect("release");
    }

    /// Audit 11-03 core L4 (ADR-2396): dropping the squeeze wakes a thread parked for a long
    /// period, so it ends now instead of after up to one period.
    #[test]
    fn drop_wakes_a_thread_parked_for_a_long_period() {
        let (alive_tx, alive_rx) = std::sync::mpsc::channel::<()>();
        let squeeze = MemorySqueeze::spawn(Duration::from_secs(3600), move || {
            // Held by the round closure: dropped when the thread ends.
            let _ = &alive_tx;
        })
        .expect("thread");
        while squeeze.rounds() == 0 {
            std::thread::yield_now();
        }
        drop(squeeze);
        assert_eq!(
            alive_rx.recv_timeout(Duration::from_secs(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "the parked thread ended after the drop"
        );
    }

    #[test]
    fn settings_parse_mb_period_and_mode() {
        assert_eq!(
            SqueezeSettings::parse("512"),
            Some(SqueezeSettings {
                mb: 512,
                period: Duration::from_millis(250),
                file_only: false
            })
        );
        assert_eq!(
            SqueezeSettings::parse("32:1000:file"),
            Some(SqueezeSettings {
                mb: 32,
                period: Duration::from_millis(1000),
                file_only: true
            })
        );
        assert_eq!(SqueezeSettings::parse("16:file").map(|s| s.file_only), Some(true));
        assert_eq!(SqueezeSettings::parse("x"), None);
        assert_eq!(SqueezeSettings::parse("8:soon"), None);
    }
}
