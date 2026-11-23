// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/fast_quit.rs, commit c9513623f), ADR-8012.
//! Fast exit (ADR-8833, absorb plan P-07): the window goes away in the frame the quit arrives,
//! a bounded flush writes what must persist, then the process ends without tearing anything down.
//!
//! The clean exit ([`crate::run_clean_exit`]) drops the world object by object, then the device,
//! with the window still on screen. With part of the game in swap that reads every page back in
//! only to free it; on a loaded box a quit could take minutes, and a low-end laptop's quit hung
//! (KI-RB-003). [`ExitMode::Fast`] takes another path:
//!
//! 1. [`request_fast_exit`] (any quit, when the host chose [`ExitMode::Fast`]): a hard deadline is
//!    armed ([`FastExitSettings::hard_deadline`], exit code
//!    [`FastExitSettings::deadline_exit_code`]); every OS window is hidden now (X11 unmap, Win32
//!    `SW_HIDE`, macOS `orderOut`, Wayland a null buffer commit); then the hosts' hide hooks run
//!    ([`ExitHooks::on_fast_hide`]: windows the platform does not own, stop presenting).
//! 2. At the end of the frame ([`FastExitRequest::flush_frame`]) the runner calls
//!    [`poll_fast_exit`]: each flush job ([`ExitHooks::add_flush`]: the pipeline cache, a save, a
//!    network leave) runs on its own thread, all in parallel, inside
//!    [`FastExitSettings::flush_budget`]; the files registered with [`keep_file_on_fast_exit`] are
//!    synced to disk. A job still running at the budget is left; it ends with the process.
//! 3. Logs and stdio are flushed (bounded) and the process ends with `_exit` (Linux, macOS,
//!    Android) or `TerminateProcess` (Windows): no world, renderer or device is dropped. The
//!    kernel frees the memory (swapped pages without reading them back) and the GPU driver frees
//!    its objects in bulk when the device file closes; no C exit handler runs.
//!
//! Where the host has no platform runner (a headless loop), it calls [`poll_fast_exit`] after each
//! update, as the runner does.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regia_app::prelude::*;

use crate::exit::ExitHooks;
use crate::window::WindowHandles;

/// How the app leaves when it quits. Optional resource; without it the app exits
/// [`ExitMode::Clean`].
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ExitMode {
    /// The ordered teardown of [`crate::run_clean_exit`] (the default).
    #[default]
    Clean,
    /// Hide, bounded flush, `_exit` ([`request_fast_exit`]): every `AppExit` the runner sees
    /// (window close, [`crate::request_quit`], [`crate::request_clean_exit`]) ends this way.
    Fast,
}

impl ExitMode {
    /// Stable lowercase name (`clean` or `fast`; the agent's `quit.request` `mode`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ExitMode::Clean => "clean",
            ExitMode::Fast => "fast",
        }
    }

    /// The mode named `name` (`clean`, `fast`; ASCII case and surrounding spaces ignored).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "clean" => Some(ExitMode::Clean),
            "fast" => Some(ExitMode::Fast),
            _ => None,
        }
    }
}

/// The fast exit's budgets. Optional resource; the defaults apply without it.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct FastExitSettings {
    /// The flush jobs run in parallel inside this budget. Default 1.5 s.
    pub flush_budget: Duration,
    /// From [`request_fast_exit`], the process ends by this time whatever is still running
    /// (exit code [`FastExitSettings::deadline_exit_code`]). Default 3 s; `None` arms no deadline
    /// (in-process tests).
    pub hard_deadline: Option<Duration>,
    /// The hard deadline's exit code, distinct from a crash and from the device-lost code (3).
    /// Default [`FAST_EXIT_DEADLINE_CODE`].
    pub deadline_exit_code: i32,
}

/// [`FastExitSettings::deadline_exit_code`]'s default: a quit that overran its deadline.
pub const FAST_EXIT_DEADLINE_CODE: i32 = 5;

impl Default for FastExitSettings {
    fn default() -> Self {
        Self {
            flush_budget: Duration::from_millis(1500),
            hard_deadline: Some(Duration::from_millis(3000)),
            deadline_exit_code: FAST_EXIT_DEADLINE_CODE,
        }
    }
}

/// A fast exit is under way: inserted by [`request_fast_exit`], read by [`poll_fast_exit`].
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct FastExitRequest {
    /// The process exit code.
    pub code: i32,
    /// Who asked (logged).
    pub reason: String,
    /// The frame ([`Time::frame_count`]) the request arrived in.
    pub requested_frame: u64,
    /// The flush and the process end run at the end of this frame (`requested_frame` plus the
    /// request's delay: an agent reply gets a frame to leave the socket).
    pub flush_frame: u64,
    /// OS windows hidden by the request.
    pub windows_hidden: usize,
    /// Hide hooks run ([`ExitHooks::on_fast_hide`]).
    pub hide_hooks: usize,
}

/// One flush job: runs on its own thread during the fast exit's flush.
pub type FlushJob = Box<dyn FnOnce() + Send>;

/// Prepares a flush job on the frame thread, with the world (`None`: nothing to write this time).
pub type FlushHook = Box<dyn FnMut(&mut World) -> Option<FlushJob> + Send + Sync>;

/// A hide hook: runs in the request frame, right after the OS windows are hidden.
pub type FastHideHook = Box<dyn FnMut(&mut World) + Send + Sync>;

impl ExitHooks {
    /// Adds a flush job builder to the fast exit: `hook` runs on the frame thread with the world
    /// when the flush starts and returns the job that writes (`None` to skip); the job then runs
    /// on its own thread, in parallel with the others, inside [`FastExitSettings::flush_budget`].
    pub fn add_flush(
        &mut self,
        name: &'static str,
        hook: impl FnMut(&mut World) -> Option<FlushJob> + Send + Sync + 'static,
    ) -> &mut Self {
        self.flush.push((name, Box::new(hook)));
        self
    }

    /// Adds a hook run in the fast exit's request frame, right after the OS windows are hidden:
    /// a host hides the windows the platform does not own, a renderer stops presenting.
    pub fn on_fast_hide(
        &mut self,
        name: &'static str,
        hook: impl FnMut(&mut World) + Send + Sync + 'static,
    ) -> &mut Self {
        self.fast_hide.push((name, Box::new(hook)));
        self
    }

    /// Number of fast-exit flush hooks.
    #[must_use]
    pub fn flush_len(&self) -> usize {
        self.flush.len()
    }
}

static KEPT_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A file the player's actions wrote (settings, key map, a save): the fast exit syncs it to disk
/// during its flush, so a hard reset right after quitting cannot lose it. Listed once per path.
pub fn keep_file_on_fast_exit(path: &Path) {
    let mut kept = KEPT_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !kept.iter().any(|p| p == path) {
        kept.push(path.to_path_buf());
    }
}

/// A window the fast exit can hide (the winit window; a mock in tests).
pub trait HideableWindow {
    /// Takes the window off screen now; returns how (`x11: unmapped`, `win32: hidden`, ...).
    fn hide_now(&self) -> &'static str;
}

impl HideableWindow for winit::window::Window {
    fn hide_now(&self) -> &'static str {
        crate::platform::hide_window_now(self)
    }
}

/// Hides every window in `windows` now; returns how each went.
pub fn hide_windows<'a>(windows: impl IntoIterator<Item = &'a dyn HideableWindow>) -> Vec<&'static str> {
    windows.into_iter().map(HideableWindow::hide_now).collect()
}

/// Starts a fast exit with `code`: arms the hard deadline, hides every OS window and runs the
/// hide hooks now, and leaves a [`FastExitRequest`] for [`poll_fast_exit`], which flushes and ends
/// the process at the end of this frame (`delay_frames` frames later, for an agent reply to
/// leave). It also writes `AppExit`, so a host loop that does not call [`poll_fast_exit`] still
/// exits (cleanly). A second request keeps the first one. Call it from the frame thread (an exclusive
/// system or a `&mut World` handler): Win32 hides a window on its own thread.
pub fn request_fast_exit(world: &mut World, code: i32, reason: &str, delay_frames: u64) -> FastExitRequest {
    if let Some(existing) = world.get_resource::<FastExitRequest>() {
        return existing.clone();
    }
    let started = Instant::now();
    let settings = world
        .get_resource::<FastExitSettings>()
        .cloned()
        .unwrap_or_default();
    if let Some(deadline) = settings.hard_deadline {
        // Never disarmed: it also covers a flush job or an exit that blocks.
        let _ = crate::exit::arm_exit_watchdog(deadline, settings.deadline_exit_code);
    }
    let hidden = match world.get_resource::<WindowHandles>() {
        Some(handles) => hide_windows(handles.iter().map(|(_, w)| &**w as &dyn HideableWindow)),
        None => Vec::new(),
    };
    // The renderer presents nothing more (a Wayland surface given a null buffer must not get a
    // new one; the runner keeps it set).
    if let Some(mut flag) = world.get_resource_mut::<crate::SurfacesHidden>() {
        flag.0 = true;
    }
    let mut hooks = world.remove_resource::<ExitHooks>().unwrap_or_default();
    for (name, hook) in &mut hooks.fast_hide {
        hook(world);
        tracing::debug!(target: "regia_platform", "regia fast exit: hide hook {name}");
    }
    let hide_hooks = hooks.fast_hide.len();
    world.insert_resource(hooks);
    let frame = world.get_resource::<Time>().map_or(0, Time::frame_count);
    let request = FastExitRequest {
        code,
        reason: reason.to_owned(),
        requested_frame: frame,
        flush_frame: frame.saturating_add(delay_frames),
        windows_hidden: hidden.len(),
        hide_hooks,
    };
    tracing::info!(
        target: "regia_platform",
        "regia fast exit: requested by {reason} (code {code}) at frame {frame}; {} window(s) hidden [{}] in {:.1} ms; deadline {:?}",
        hidden.len(),
        hidden.join(", "),
        started.elapsed().as_secs_f64() * 1e3,
        settings.hard_deadline
    );
    world.insert_resource(request.clone());
    // A host loop that never calls `poll_fast_exit` (a headless loop) still exits, cleanly, on
    // this `AppExit`; the platform runner ignores it while the fast exit waits for its frame.
    let exit = match u8::try_from(code).ok().and_then(std::num::NonZeroU8::new) {
        Some(n) => AppExit::Error(n),
        None if code == 0 => AppExit::Success,
        None => AppExit::Error(std::num::NonZeroU8::MIN),
    };
    world.write_message(exit);
    request
}

/// A fast exit is under way in this world.
#[must_use]
pub fn fast_exit_requested(world: &World) -> bool {
    world.contains_resource::<FastExitRequest>()
}

/// The world's [`ExitMode`] (default [`ExitMode::Clean`]).
#[must_use]
pub fn exit_mode(world: &World) -> ExitMode {
    world.get_resource::<ExitMode>().copied().unwrap_or_default()
}

/// One flush job's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushOutcome {
    /// The job's name.
    pub name: &'static str,
    /// It finished inside the budget.
    pub done: bool,
    /// When it finished, from the start of the flush (the budget when it did not).
    pub elapsed: Duration,
}

/// What the fast exit's flush did ([`run_fast_exit_flush`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastExitReport {
    /// The request.
    pub request: FastExitRequest,
    /// Every job, in the order added.
    pub flush: Vec<FlushOutcome>,
    /// The whole flush.
    pub flush_total: Duration,
}

impl FastExitReport {
    /// The one-line summary the exit logs and prints: `regia fast exit: code=... steps=name:ok:ms,...`.
    #[must_use]
    pub fn line(&self) -> String {
        let steps: Vec<String> = self
            .flush
            .iter()
            .map(|o| {
                format!(
                    "{}:{}:{}",
                    o.name,
                    if o.done { "ok" } else { "timeout" },
                    o.elapsed.as_millis()
                )
            })
            .collect();
        format!(
            "regia fast exit: code={} reason=\"{}\" frame={} windows_hidden={} flush_ms={} steps={}",
            self.request.code,
            self.request.reason,
            self.request.requested_frame,
            self.request.windows_hidden,
            self.flush_total.as_millis(),
            if steps.is_empty() {
                "-".to_owned()
            } else {
                steps.join(",")
            }
        )
    }
}

/// Runs `jobs` in parallel, each on its own thread, and returns when all are done or `budget` has
/// passed. A job still running is left detached (it ends with the process).
pub fn run_bounded(jobs: Vec<(&'static str, FlushJob)>, budget: Duration) -> Vec<FlushOutcome> {
    let start = Instant::now();
    let (tx, rx) = mpsc::channel::<(usize, Duration)>();
    let mut outcomes = Vec::with_capacity(jobs.len());
    let mut running = 0usize;
    for (index, (name, job)) in jobs.into_iter().enumerate() {
        outcomes.push(FlushOutcome {
            name,
            done: false,
            elapsed: budget,
        });
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("regia-flush-{name}"))
            .spawn(move || {
                job();
                let _ = tx.send((index, start.elapsed()));
            });
        match spawned {
            Ok(_) => running += 1,
            Err(err) => {
                tracing::warn!(target: "regia_platform", "regia fast exit: flush job {name} not started: {err}")
            }
        }
    }
    drop(tx);
    while running > 0 {
        let left = budget.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((index, elapsed)) => {
                outcomes[index].done = true;
                outcomes[index].elapsed = elapsed;
                running -= 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    outcomes
}

/// The flush of a requested fast exit: builds every flush job on this thread
/// ([`ExitHooks::add_flush`]), adds the sync of the kept files, and runs them in parallel inside
/// the budget. Does not end the process ([`finish_fast_exit`] does); a custom host or a test calls
/// it directly. `None` when no fast exit was requested.
pub fn run_fast_exit_flush(world: &mut World) -> Option<FastExitReport> {
    let request = world.get_resource::<FastExitRequest>()?.clone();
    let settings = world
        .get_resource::<FastExitSettings>()
        .cloned()
        .unwrap_or_default();
    let _span = tracing::info_span!("regia_fast_exit_flush").entered();
    let start = Instant::now();
    let mut hooks = world.remove_resource::<ExitHooks>().unwrap_or_default();
    let mut jobs: Vec<(&'static str, FlushJob)> = Vec::new();
    for (name, hook) in &mut hooks.flush {
        if let Some(job) = hook(world) {
            jobs.push((*name, job));
        }
    }
    world.insert_resource(hooks);
    let kept = KEPT_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if !kept.is_empty() {
        jobs.push((
            "fsync",
            Box::new(move || {
                for path in kept {
                    if let Ok(file) = std::fs::File::open(&path)
                        && let Err(err) = file.sync_all()
                    {
                        tracing::warn!(target: "regia_platform", "regia fast exit: sync {}: {err}", path.display());
                    }
                }
            }),
        ));
    }
    let flush = run_bounded(jobs, settings.flush_budget);
    let report = FastExitReport {
        request,
        flush,
        flush_total: start.elapsed(),
    };
    tracing::info!(target: "regia_platform", "{}", report.line());
    Some(report)
}

/// Ends the process after a fast exit's flush: the report line goes to the log and stdout, logs
/// and stdio are flushed (`flush` is the host's log guard; stdio is bounded), and the process ends
/// with the request's code without process-exit handlers (`_exit` / `TerminateProcess`).
pub fn finish_fast_exit(report: &FastExitReport, flush: impl FnOnce()) -> ! {
    let code = report.request.code;
    {
        use std::io::Write as _;
        let line = report.line();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
    }
    flush();
    crate::deliberate_exit::flush_stdio();
    crate::platform::exit_immediately(code)
}

/// What the runner (or a headless host) calls after each update: when a fast exit was requested
/// and its flush frame has come, it flushes and ends the process ([`run_fast_exit_flush`], then
/// [`finish_fast_exit`]). Returns while there is nothing to do yet.
///
/// An app in [`ExitMode::Fast`] whose update wrote `AppExit` is turned into a fast exit here too
/// (`AppExit::Error(n)` exits with `n`).
pub fn poll_fast_exit(app: &mut App) {
    if !fast_exit_due(app) {
        return;
    }
    if let Some(code) = app.world().get_resource::<FastExitRequest>().map(|r| r.code) {
        // A fault from here on ends the process with `code`, without a crash bundle.
        crate::begin_deliberate_exit(code);
    }
    if let Some(report) = run_fast_exit_flush(app.world_mut()) {
        finish_fast_exit(&report, || {});
    }
}

/// [`poll_fast_exit`]'s decision, without the flush: requests the fast exit when the app is in
/// [`ExitMode::Fast`] and wrote `AppExit`, then reports whether the flush frame has come.
#[must_use]
pub fn fast_exit_due(app: &mut App) -> bool {
    if !fast_exit_requested(app.world())
        && exit_mode(app.world()) == ExitMode::Fast
        && let Some(exit) = app.should_exit()
    {
        let code = match exit {
            AppExit::Success => 0,
            AppExit::Error(n) => i32::from(n.get()),
        };
        request_fast_exit(app.world_mut(), code, "AppExit (ExitMode::Fast)", 0);
    }
    let world = app.world();
    let Some(request) = world.get_resource::<FastExitRequest>() else {
        return false;
    };
    let frame = world.get_resource::<Time>().map_or(0, Time::frame_count);
    frame >= request.flush_frame
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct MockWindow {
        visible: Cell<bool>,
    }

    impl HideableWindow for MockWindow {
        fn hide_now(&self) -> &'static str {
            self.visible.set(false);
            "mock: hidden"
        }
    }

    #[test]
    fn hide_windows_hides_every_window() {
        let a = MockWindow {
            visible: Cell::new(true),
        };
        let b = MockWindow {
            visible: Cell::new(true),
        };
        let how = hide_windows([&a as &dyn HideableWindow, &b as &dyn HideableWindow]);
        assert_eq!(how, ["mock: hidden", "mock: hidden"]);
        assert!(!a.visible.get() && !b.visible.get());
    }

    #[test]
    fn exit_modes_round_trip_and_default_to_clean() {
        assert_eq!(ExitMode::default(), ExitMode::Clean);
        for mode in [ExitMode::Clean, ExitMode::Fast] {
            assert_eq!(ExitMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(ExitMode::from_name(" FAST "), Some(ExitMode::Fast));
        assert_eq!(ExitMode::from_name("teardown"), None);
    }

    #[test]
    fn a_hung_job_never_holds_the_flush_past_its_budget() {
        let (release, hold) = mpsc::channel::<()>();
        let quick_ran = Arc::new(AtomicUsize::new(0));
        let ran = quick_ran.clone();
        let outcomes = run_bounded(
            vec![
                (
                    "quick",
                    Box::new(move || {
                        ran.fetch_add(1, Ordering::SeqCst);
                    }),
                ),
                (
                    "hung",
                    Box::new(move || {
                        // Blocks until the test ends (the sender drops): longer than the budget.
                        let _ = hold.recv();
                    }),
                ),
            ],
            Duration::from_millis(100),
        );
        drop(release);
        assert_eq!(
            outcomes.iter().map(|o| (o.name, o.done)).collect::<Vec<_>>(),
            [("quick", true), ("hung", false)]
        );
        assert_eq!(
            outcomes[1].elapsed,
            Duration::from_millis(100),
            "a cut job reports the budget"
        );
        assert_eq!(quick_ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn kept_files_are_listed_once() {
        let path = std::env::temp_dir().join(format!("regia-fast-exit-keep-{}", std::process::id()));
        keep_file_on_fast_exit(&path);
        keep_file_on_fast_exit(&path);
        let kept = KEPT_FILES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(kept.iter().filter(|p| **p == path).count(), 1);
    }

    #[test]
    fn the_report_line_names_every_step() {
        let report = FastExitReport {
            request: FastExitRequest {
                code: 7,
                reason: "test".into(),
                requested_frame: 3,
                flush_frame: 3,
                windows_hidden: 1,
                hide_hooks: 0,
            },
            flush: vec![
                FlushOutcome {
                    name: "cache",
                    done: true,
                    elapsed: Duration::from_millis(4),
                },
                FlushOutcome {
                    name: "save",
                    done: false,
                    elapsed: Duration::from_millis(1500),
                },
            ],
            flush_total: Duration::from_millis(1500),
        };
        let line = report.line();
        assert!(line.starts_with("regia fast exit: code=7"), "{line}");
        assert!(line.contains("steps=cache:ok:4,save:timeout:1500"), "{line}");
    }
}
