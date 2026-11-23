// SPDX-License-Identifier: MPL-2.0
//! Watch the game crate's sources and debounce reload requests (specs/20 § 1).
//!
//! `notify` is already an editor dependency. There is no debouncer crate: a source change arms a
//! 500 ms quiet timer (`specs/19` § 5), and the next [`Debounce::take_ready`] after that quiet
//! period fires once.

use std::path::Path;
use std::time::{Duration, Instant};

use notify::Watcher;

/// Quiet time after the last `.rs` change before a reload.
pub const QUIET: Duration = Duration::from_millis(500);

/// Trailing-edge debounce. [`Debounce::poke`] restarts the timer.
#[derive(Debug, Clone)]
pub struct Debounce {
    quiet: Duration,
    dirty_at: Option<Instant>,
}

impl Debounce {
    /// A timer that fires once `quiet` has passed since the last [`Debounce::poke`].
    #[must_use]
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            dirty_at: None,
        }
    }

    /// Arms or restarts the timer.
    pub fn poke(&mut self, now: Instant) {
        self.dirty_at = Some(now);
    }

    /// True once, when `now` is at least `quiet` after the last poke.
    pub fn take_ready(&mut self, now: Instant) -> bool {
        let Some(at) = self.dirty_at else {
            return false;
        };
        if now.saturating_duration_since(at) >= self.quiet {
            self.dirty_at = None;
            true
        } else {
            false
        }
    }
}

/// `notify` watcher for `src/**/*.rs`.
pub struct SourceWatch {
    _watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

impl SourceWatch {
    /// Watches `dir` recursively.
    ///
    /// # Errors
    ///
    /// Returns the watcher error. Callers log it and keep explicit reloads working.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default())
            .map_err(|error| error.to_string())?;
        watcher
            .watch(dir, notify::RecursiveMode::Recursive)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// True when a `.rs` path changed since the last drain, or the watcher reported an error
    /// (an overflowed queue or a lost watch may hide a change, so it counts as one).
    pub fn drain_rust_changes(&self) -> bool {
        drain_events(&self.rx)
    }
}

fn drain_events(rx: &std::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>) -> bool {
    let mut dirty = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            Ok(event) => {
                if event.paths.iter().any(|path| is_rust_source(path)) {
                    dirty = true;
                }
            }
            Err(error) => {
                // Dropped silently before (audit 11-03 dylib M4): events may be lost.
                tracing::warn!(target: "regia_editor", "source watch error: {error}; treating the sources as changed");
                dirty = true;
            }
        }
    }
    dirty
}

fn is_rust_source(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("rs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit 11-03 dylib M4: a watcher error (queue overflow, lost watch) counts as a change.
    #[test]
    fn a_watcher_error_counts_as_a_change() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(notify::Error::generic("queue overflow")))
            .expect("send");
        assert!(drain_events(&rx));
        tx.send(Ok(
            notify::Event::new(notify::EventKind::Any).add_path("src/lib.txt".into())
        ))
        .expect("send");
        assert!(!drain_events(&rx), "a non-Rust path is not a change");
    }

    #[test]
    fn debounce_waits_for_quiet_and_restarts() {
        let start = Instant::now();
        let mut timer = Debounce::new(Duration::from_millis(200));
        assert!(!timer.take_ready(start));
        timer.poke(start);
        assert!(!timer.take_ready(start + Duration::from_millis(199)));
        timer.poke(start + Duration::from_millis(150));
        assert!(!timer.take_ready(start + Duration::from_millis(300)));
        assert!(timer.take_ready(start + Duration::from_millis(350)));
        assert!(!timer.take_ready(start + Duration::from_millis(400)));
    }

    /// `specs/19` § 5: a source change is built after 500 ms without another change.
    #[test]
    fn the_editor_watch_debounces_for_the_spec_500_ms() {
        assert_eq!(QUIET, Duration::from_millis(500));
        let start = Instant::now();
        let mut timer = Debounce::new(QUIET);
        timer.poke(start);
        assert!(!timer.take_ready(start + Duration::from_millis(499)));
        assert!(timer.take_ready(start + Duration::from_millis(500)));
    }
}
