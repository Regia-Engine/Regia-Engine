// SPDX-License-Identifier: MPL-2.0
//! Dev-only file watchers (`docs/specs/12-assets.md` § 8, `dev-assets` feature, desktop only).
//!
//! - [`spawn_cooked_watcher`]: the game side of a standalone dev run. `regia run --dev` cooks
//!   changed sources into the cooked cache; this watcher turns each `.fckd` write into
//!   [`ReloadMessage::Reload`].
//! - [`SourceWatcher`]: a debounced watcher over source directories. The editor's live cooker
//!   (`regia_asset_pipeline::live`) and [`spawn_source_forwarder`] build on it.
//!
//! There is no debouncer crate: [`PathDebouncer`] restarts a per-path quiet timer on every event
//! and releases a path once it has been quiet for the configured time. Editors that save by
//! writing a temp file and renaming it produce several events per save; they collapse into one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::instrument;

use crate::id::AssetId;
use crate::live::SourceKind;
use crate::server::ReloadMessage;

/// Default quiet time before a changed asset is reloaded. Short enough that a save shows up well
/// inside a second, long enough to merge the write/rename/chmod burst of one save.
pub const ASSET_DEBOUNCE: Duration = Duration::from_millis(120);

/// Spawn a background thread watching `cache_dir` for `.fckd` writes and sending [`ReloadMessage`].
///
/// Returns the watcher (must be kept alive) and the sender paired with the server's receiver.
#[instrument(skip_all, fields(cache = %cache_dir.display()))]
pub fn spawn_cooked_watcher(
    cache_dir: &Path,
    reload_tx: Sender<ReloadMessage>,
) -> Result<RecommendedWatcher, notify::Error> {
    let cache = cache_dir.to_path_buf();
    let mut watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if !is_content_event(&event.kind) {
                    return;
                }
                for path in event.paths {
                    if path.extension().is_some_and(|e| e == "fckd")
                        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                        && let Ok(id) = stem.parse::<AssetId>()
                    {
                        let _ = reload_tx.send(ReloadMessage::Reload(id));
                    }
                }
            }
        },
        Config::default().with_poll_interval(Duration::from_millis(200)),
    )?;
    watcher.watch(&cache, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Trailing-edge debounce per path: a path is released once no event touched it for `quiet`.
#[derive(Debug, Clone)]
pub struct PathDebouncer {
    quiet: Duration,
    pending: HashMap<PathBuf, Instant>,
    first_seen: HashMap<PathBuf, Instant>,
}

impl PathDebouncer {
    /// A debouncer with the given quiet time.
    #[must_use]
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            pending: HashMap::new(),
            first_seen: HashMap::new(),
        }
    }

    /// The quiet time.
    #[must_use]
    pub fn quiet(&self) -> Duration {
        self.quiet
    }

    /// Records an event on `path` at `now` (restarts its timer).
    pub fn poke(&mut self, path: PathBuf, now: Instant) {
        self.first_seen.entry(path.clone()).or_insert(now);
        self.pending.insert(path, now);
    }

    /// Paths quiet for at least the quiet time, sorted, each with the instant of its first event
    /// (the start of the change-to-reload latency). They are removed.
    pub fn take_ready(&mut self, now: Instant) -> Vec<(PathBuf, Instant)> {
        let mut ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, at)| now.saturating_duration_since(**at) >= self.quiet)
            .map(|(path, _)| path.clone())
            .collect();
        ready.sort();
        ready
            .into_iter()
            .map(|path| {
                self.pending.remove(&path);
                let first = self.first_seen.remove(&path).unwrap_or(now);
                (path, first)
            })
            .collect()
    }

    /// How long until the next path becomes ready (`None` when nothing is pending).
    #[must_use]
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        self.pending
            .values()
            .map(|at| (*at + self.quiet).saturating_duration_since(now))
            .min()
    }

    /// True when no path is waiting.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The platform watcher behind a [`SourceWatcher`] stopped delivering events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("file watcher stopped")]
pub struct WatcherGone;

/// Debounced recursive watcher over one or more source directories.
pub struct SourceWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<Result<notify::Event, notify::Error>>,
    debounce: PathDebouncer,
    roots: Vec<PathBuf>,
    /// `(real path, root as given)` for roots reached through a link (macOS reports events under
    /// the real path: `/var` is `/private/var`), so reported paths stay under the given root.
    aliases: Vec<(PathBuf, PathBuf)>,
}

impl SourceWatcher {
    /// Watches every existing directory in `roots` recursively.
    ///
    /// # Errors
    ///
    /// The platform watcher could not start, or no root could be watched.
    pub fn open(roots: &[PathBuf], quiet: Duration) -> Result<Self, notify::Error> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, Config::default())?;
        let mut watched = Vec::new();
        let mut last_err = None;
        for root in roots {
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => watched.push(root.clone()),
                Err(err) => last_err = Some(err),
            }
        }
        if watched.is_empty()
            && let Some(err) = last_err
        {
            return Err(err);
        }
        let aliases = watched
            .iter()
            .filter_map(|root| {
                let real = std::fs::canonicalize(root).ok()?;
                (real != *root).then(|| (real, root.clone()))
            })
            .collect();
        Ok(Self {
            _watcher: watcher,
            rx,
            debounce: PathDebouncer::new(quiet),
            roots: watched,
            aliases,
        })
    }

    /// The directories actually watched.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The debounce quiet time.
    #[must_use]
    pub fn quiet(&self) -> Duration {
        self.debounce.quiet()
    }

    /// Drains pending events without blocking and returns the paths whose quiet time has passed.
    pub fn poll(&mut self) -> Vec<(PathBuf, Instant)> {
        while let Ok(event) = self.rx.try_recv() {
            self.absorb(event, Instant::now());
        }
        self.debounce.take_ready(Instant::now())
    }

    /// Blocks up to `timeout` for the next batch of debounced paths. Returns an empty batch on
    /// timeout.
    ///
    /// # Errors
    ///
    /// [`WatcherGone`] when the platform watcher has stopped.
    pub fn wait(&mut self, timeout: Duration) -> Result<Vec<(PathBuf, Instant)>, WatcherGone> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            let ready = self.debounce.take_ready(now);
            if !ready.is_empty() {
                return Ok(ready);
            }
            if now >= deadline {
                return Ok(Vec::new());
            }
            let wait = self
                .debounce
                .next_due(now)
                .unwrap_or(deadline - now)
                .min(deadline - now)
                .max(Duration::from_millis(1));
            match self.rx.recv_timeout(wait) {
                Ok(event) => self.absorb(event, Instant::now()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(WatcherGone),
            }
        }
    }

    fn absorb(&mut self, event: Result<notify::Event, notify::Error>, now: Instant) {
        let Ok(event) = event else {
            return;
        };
        if !is_content_event(&event.kind) {
            return;
        }
        for path in event.paths {
            if let Some(path) = relevant_path(&path) {
                self.debounce.poke(self.under_given_root(path), now);
            }
        }
    }

    /// `path` re-based from a root's real path onto the root as it was given.
    fn under_given_root(&self, path: PathBuf) -> PathBuf {
        for (real, given) in &self.aliases {
            if let Ok(rest) = path.strip_prefix(real) {
                return given.join(rest);
            }
        }
        path
    }
}

/// Creations, writes, renames and removals; not reads or metadata-only access.
fn is_content_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && !matches!(
        kind,
        EventKind::Modify(notify::event::ModifyKind::Metadata(
            notify::event::MetadataKind::AccessTime
        ))
    )
}

/// The changed path, or `None` for editor scratch files and directories.
///
/// `.meta` sidecars pass through; the live cooker maps them to their source.
#[must_use]
pub fn relevant_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    // `.<engine>-tmp*` atomic-write scratch files; tools written before the rename to Regia
    // Engine use the legacy name (ADR-1429).
    let engine_scratch = |engine: &str| {
        name.strip_prefix('.')
            .and_then(|rest| rest.strip_prefix(engine))
            .is_some_and(|rest| rest.starts_with("-tmp"))
    };
    let scratch = name.starts_with(".#")
        || name.starts_with('~')
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.ends_with(".part")
        || engine_scratch(regia_core::brand::MAIN_CRATE)
        || engine_scratch(regia_core::brand::LEGACY_MAIN_CRATE);
    if scratch || path.is_dir() {
        return None;
    }
    Some(path.to_path_buf())
}

/// A running [`spawn_source_forwarder`] thread. Dropping it stops the thread and waits for it
/// (at most one watcher poll, 250 ms), so no change is forwarded after the drop returns.
pub struct SourceForwarder {
    stop: Arc<AtomicBool>,
    roots: Vec<PathBuf>,
    quiet: Duration,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SourceForwarder {
    /// The watched directories.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The debounce quiet time.
    #[must_use]
    pub fn quiet(&self) -> Duration {
        self.quiet
    }
}

impl Drop for SourceForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Watches `roots` and forwards each debounced change whose kind `accept` admits as
/// [`ReloadMessage::Source`].
///
/// A dev game uses it for the kinds it reads from source (shaders, levels, VFX, UI layouts,
/// audio) while `regia run --dev` cooks the rest. The thread exits when the returned handle is
/// dropped or the receiver is gone.
///
/// # Errors
///
/// The watcher could not start.
pub fn spawn_source_forwarder(
    roots: &[PathBuf],
    quiet: Duration,
    accept: fn(SourceKind) -> bool,
    tx: Sender<ReloadMessage>,
) -> Result<SourceForwarder, notify::Error> {
    let mut watcher = SourceWatcher::open(roots, quiet)?;
    let stop = Arc::new(AtomicBool::new(false));
    let watched = watcher.roots().to_vec();
    let stop_flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("regia-asset-watch".into())
        .spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                let Ok(batch) = watcher.wait(Duration::from_millis(250)) else {
                    return;
                };
                for (path, _) in batch {
                    // A batch that ends a wait after the handle was dropped is not forwarded.
                    if stop_flag.load(Ordering::Acquire) {
                        return;
                    }
                    if !accept(SourceKind::of(&path)) {
                        continue;
                    }
                    if tx.send(ReloadMessage::Source(path)).is_err() {
                        return;
                    }
                }
            }
        })
        .map_err(|err| notify::Error::generic(&err.to_string()))?;
    Ok(SourceForwarder {
        stop,
        roots: watched,
        quiet,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_merges_bursts_per_path() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = PathDebouncer::new(ms(100));
        d.poke("a".into(), t0);
        d.poke("b".into(), t0 + ms(10));
        d.poke("a".into(), t0 + ms(60));
        assert!(d.take_ready(t0 + ms(99)).is_empty());
        assert_eq!(d.next_due(t0 + ms(99)), Some(ms(11)));
        let ready = d.take_ready(t0 + ms(115));
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, PathBuf::from("b"));
        let ready = d.take_ready(t0 + ms(160));
        assert_eq!(ready, vec![(PathBuf::from("a"), t0)]);
        assert!(d.is_idle());
    }

    #[test]
    fn scratch_files_are_ignored() {
        assert_eq!(
            relevant_path(Path::new("/p/assets/a.png.meta")),
            Some(PathBuf::from("/p/assets/a.png.meta"))
        );
        assert_eq!(relevant_path(Path::new("/p/assets/.#a.ron")), None);
        assert_eq!(relevant_path(Path::new("/p/assets/a.ron~")), None);
        assert_eq!(relevant_path(Path::new("/p/assets/.a.ron.swp")), None);
        for engine in [
            regia_core::brand::MAIN_CRATE,
            regia_core::brand::LEGACY_MAIN_CRATE,
        ] {
            let scratch = format!("/p/assets/.{engine}-tmp-42");
            assert_eq!(relevant_path(Path::new(&scratch)), None, "{scratch}");
        }
        assert_eq!(
            relevant_path(Path::new("/p/assets/hud.ui.ron")),
            Some(PathBuf::from("/p/assets/hud.ui.ron"))
        );
    }

    /// Three writes in a burst come back as one change. No wall-clock assert (deep-zai-assets
    /// finding 7: a loaded build host failed the old `elapsed() < 1 s` check); the waits are a
    /// counted hang bound.
    #[test]
    fn source_watcher_reports_a_write_once_after_the_quiet_time() {
        let dir = regia_test_utils::unique_temp_dir("hot-reload-source-watch");
        let mut watcher = SourceWatcher::open(std::slice::from_ref(&dir), ms(80)).expect("watch");
        let file = dir.join("fire.vfx.ron");
        for i in 0..3 {
            std::fs::write(&file, format!("v{i}")).expect("write");
        }
        let mut got = Vec::new();
        for _ in 0..50 {
            got = watcher.wait(Duration::from_millis(200)).expect("alive");
            if !got.is_empty() {
                break;
            }
        }
        assert_eq!(got.len(), 1, "one debounced change: {got:?}");
        assert_eq!(got[0].0, file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The `regia run --dev` source forwarder (same `accept` filter as `server.rs`) sends a
    /// `ReloadMessage::Source` for an edited `.detutility.ron` (det-utility review bug: the file
    /// classified as `Other` and was dropped). The receive waits are a counted hang bound.
    #[test]
    fn dev_forwarder_sends_an_edited_det_utility_catalogue() {
        let dir = regia_test_utils::unique_temp_dir("hot-reload-det-utility");
        let (tx, rx) = std::sync::mpsc::channel();
        let forwarder = spawn_source_forwarder(
            std::slice::from_ref(&dir),
            ms(50),
            SourceKind::is_read_at_runtime,
            tx,
        )
        .expect("forwarder");
        let file = dir.join("skirmish.detutility.ron");
        std::fs::write(&file, "(format_version: 1)").expect("write");
        let mut got = None;
        for _ in 0..100 {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ReloadMessage::Source(path)) if path == file => {
                    got = Some(path);
                    break;
                }
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(forwarder);
        assert_eq!(got.as_ref(), Some(&file), "no source change forwarded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit concurrency-1103 asset L4 (2d L1): dropping the forwarder only set a flag, so its
    /// detached thread could still wake and send a stale batch after the owner stopped watching.
    /// Dropping it now stops and joins the thread: its sender is gone when `drop` returns.
    #[test]
    fn dropping_the_forwarder_stops_its_thread() {
        let dir = regia_test_utils::unique_temp_dir("hot-reload-forwarder-drop");
        let (tx, rx) = std::sync::mpsc::channel();
        let forwarder =
            spawn_source_forwarder(std::slice::from_ref(&dir), ms(50), |_| true, tx).expect("forwarder");
        drop(forwarder);
        loop {
            match rx.try_recv() {
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    panic!("the watch thread is still running after its handle was dropped")
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
