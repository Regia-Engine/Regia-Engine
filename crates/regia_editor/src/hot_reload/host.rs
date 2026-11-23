// SPDX-License-Identifier: MPL-2.0
//! Hot-reload host: build, swap, snapshot, restore (specs/20 §§ 1–5).
//!
//! The success order is: map the new library and call `regia_game_entry` → `on_unload` →
//! snapshot → teardown → `register` → identity check → restore → `on_load` → resume.
//! A build error, an ABI mismatch, or an engine-dylib fingerprint
//! change is refused before `on_unload`, so the old code keeps running. The new library is
//! mapped, and its entry runs, while the old plugin is still current; only `register` runs
//! after teardown. An identity clash is
//! detected after `register` (the spec's position) and rolls the previous plugin back on. A panic
//! during restore does not roll back; the snapshot file is left in place for a retry.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engine::app::App;
use crate::engine::ecs::prelude::{Message, TypeOwner, World};
// `#[derive(Message)]` emits `bevy_ecs::...` paths. The prelude re-exports that crate.
#[allow(unused_imports)]
use crate::engine::ecs::prelude::bevy_ecs;

use super::build_game::{
    BuildCancel, GameBuild, build_salted_dylib, build_salted_dylib_cancellable, engine_fingerprint,
};
use super::load::{
    GameExports, GameLibraryError, host_abi_hash, load_game_exports, process_links_dynamic_std,
};
use super::snapshot::{
    HotReloadSnapshot, component_type_ids, identity_clashes, owned_reflected_types, read_snapshot,
    restore_snapshot, take_snapshot, teardown_owner, with_game_owner, write_snapshot,
};
use super::watch::{Debounce, QUIET, SourceWatch};

/// Suggest a restart after this many mapped libraries (specs/20 § 3, ~2.2 MB leaked each time).
const RESTART_AFTER: u32 = 50;

/// Paths the host builds and watches.
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    /// Manifest passed to `cargo rustc` (workspace or standalone).
    pub manifest_path: PathBuf,
    /// Package name (`-p`).
    pub package: String,
    /// Game crate directory. `src` is what the watcher reads.
    pub crate_dir: PathBuf,
    /// Shared cargo target directory (one copy of each engine rlib).
    pub target_dir: PathBuf,
    /// `.regia/hot`, holding `game-<N>-<pid>-<serial>.<ext>`, the snapshot, the status file and the trigger.
    pub hot_dir: PathBuf,
}

impl HotReloadConfig {
    /// Resolves a game crate directory to a workspace build when one is visible.
    ///
    /// # Errors
    ///
    /// The crate has no `Cargo.toml` package name.
    pub fn discover(crate_dir: &Path) -> Result<Self, String> {
        let crate_dir = crate_dir
            .canonicalize()
            .unwrap_or_else(|_| crate_dir.to_path_buf());
        let package = package_name(&crate_dir.join("Cargo.toml"))?;
        let (manifest_path, target_dir, hot_dir) = if let Some(root) = find_workspace(&crate_dir) {
            (
                root.join("Cargo.toml"),
                root.join("target"),
                regia_core::brand::project_state_dir(&root).join("hot"),
            )
        } else {
            (
                crate_dir.join("Cargo.toml"),
                crate_dir.join("target"),
                regia_core::brand::project_state_dir(&crate_dir).join("hot"),
            )
        };
        Ok(Self {
            manifest_path,
            package,
            crate_dir,
            target_dir,
            hot_dir,
        })
    }

    fn game_build(&self) -> GameBuild {
        GameBuild {
            manifest_path: self.manifest_path.clone(),
            package: self.package.clone(),
            target_dir: self.target_dir.clone(),
            hot_dir: self.hot_dir.clone(),
        }
    }

    fn snapshot_path(&self) -> PathBuf {
        self.hot_dir.join("last_snapshot.bin")
    }

    fn trigger_path(&self) -> PathBuf {
        self.hot_dir.join("trigger")
    }

    fn status_path(&self) -> PathBuf {
        self.hot_dir.join("status.json")
    }
}

/// Where a reload stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadPhase {
    /// New code is registered and play has resumed.
    Running,
    /// `cargo rustc` failed. The world was not touched.
    BuildFailed,
    /// ABI, fingerprint, identity, or static `std` refusal.
    Refused,
    /// Restore panicked. The snapshot file is intact; the world has no game plugin.
    RestoreFailed,
}

impl ReloadPhase {
    /// Stable name for logs and `status.json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::BuildFailed => "build_failed",
            Self::Refused => "refused",
            Self::RestoreFailed => "restore_failed",
        }
    }
}

/// Outcome of one [`HotReloadHost::reload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// The new library is the one running.
    pub accepted: bool,
    /// The previous library is still registered. False when nothing was loaded, or after a restore panic.
    pub old_code_running: bool,
    /// Toast text. Successful reloads use [`toast_line`].
    pub toast: String,
    /// Wall time from the start of the reload through `on_load` (or through the refusal).
    pub elapsed: Duration,
    /// Game-owned reflected types after a successful swap.
    pub types: usize,
    /// Snapshot instances restored (components + resources).
    pub instances: usize,
    /// Game script slots constructed again through the new library (`specs/20` § 3 step 9).
    pub scripts_rebuilt: usize,
    /// Game script slots dropped because the new library no longer registers their type.
    pub scripts_dropped: usize,
    /// Compiler output, refusal reason, or the toast.
    pub detail: String,
    /// Accepted generation. Unchanged when the reload is refused.
    pub generation: u32,
    /// How the attempt ended.
    pub phase: ReloadPhase,
}

/// Sent on the world after a successful swap (specs/20 § 3, step 10).
///
/// The toast text is also [`ReloadReport::toast`]. This message is what gameplay and the agent
/// read. It carries no pointer into the old library.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct HotReloaded {
    /// Metadata salt of the library now running.
    pub generation: u32,
    /// Game-owned reflected types after the swap.
    pub types: usize,
    /// Snapshot instances restored (components + resources).
    pub instances: usize,
    /// Wall time of the reload, in milliseconds.
    pub elapsed_ms: u64,
}

/// Specs/20 § 3 toast: `Reloaded in 1.3 s (12 types, 340 instances)`.
#[must_use]
pub fn toast_line(elapsed: Duration, types: usize, instances: usize) -> String {
    format!(
        "Reloaded in {:.1} s ({types} types, {instances} instances)",
        elapsed.as_secs_f64()
    )
}

struct LoadedPlugin {
    owner: TypeOwner,
    exports: GameExports,
    generation: u32,
}

struct Prepared {
    exports: GameExports,
    build_elapsed: Duration,
}

/// Editor-side owner of the game dylib and the world it registers into.
pub struct HotReloadHost {
    config: HotReloadConfig,
    app: App,
    current: Option<LoadedPlugin>,
    /// Metadata salt. Increases on every attempt, including failures, so the loader path is unique.
    attempt: u32,
    /// Game libraries this process has mapped (each `load_game_exports` call; a mapping is never
    /// released, `specs/20` § 1). The restart hint counts these, not build attempts: a failed or
    /// refused build maps nothing (review rc1-rev-editor-k finding 8).
    mapped: u32,
    playing: bool,
    paused: bool,
    /// After a restore panic, the next attempt reuses `last_snapshot.bin` instead of snapshotting an empty world.
    use_saved_snapshot: bool,
    last_report: Option<ReloadReport>,
    /// How many reports [`HotReloadHost::finish`] has published; the agent protocol's
    /// `agent.hot_reload` job recognises its build by a changed serial (ADR-5707).
    report_serial: u64,
    watch: Option<SourceWatch>,
    debounce: Debounce,
    /// A `cargo rustc` running on a worker thread ([`HotReloadHost::request_compile`]).
    in_flight: Option<InFlight>,
    /// A build was asked for (trigger, source change, Compile) while one ran. It read older
    /// sources, so another build starts once it finishes (audit 11-03 dylib M1).
    compile_owed: bool,
    /// The snapshot of a reload whose restore panicked, kept in memory for the retry so a
    /// failed or torn `last_snapshot.bin` cannot lose or wedge it (audit 11-03 dylib M3).
    retry_snapshot: Option<HotReloadSnapshot>,
    /// When the source watch last failed to open; it is retried at most once per
    /// [`WATCH_RETRY`] (audit 11-03 dylib M4).
    watch_retry_at: Option<Instant>,
    /// Engine-dylib fingerprint of the engine this process mapped, taken when the host starts
    /// (or at the first reload that finds the dylib). Every later reload compares against it, so
    /// a relink between two reloads (a terminal `cargo build`, a refused reload whose build
    /// relinked the engine) is refused too, not only a relink by the reload's own build
    /// (`specs/20` § 1, ADR-5627).
    engine_baseline: Option<u64>,
}

/// How often a source watch that failed to open (or lost its watcher) is opened again.
const WATCH_RETRY: Duration = Duration::from_secs(2);

/// A background game build: the swap runs on the main thread once it finishes.
struct InFlight {
    generation: u32,
    started: Instant,
    engine_before: u64,
    /// Stops this build (`specs/19` § 7, [`HotReloadHost::cancel_compile`]).
    cancel: BuildCancel,
    handle: std::thread::JoinHandle<Result<super::build_game::BuiltGame, super::build_game::BuildFailure>>,
}

/// What the toolbar shows for game code (`specs/19` § 5: idle / compiling / error / reloaded).
#[derive(Debug, Clone, PartialEq)]
pub enum CompileState {
    /// No build has run yet.
    Idle,
    /// A build is running; seconds so far.
    Compiling(f32),
    /// The last attempt failed; the old code runs. The text is the reason.
    Error(String),
    /// The last attempt swapped in new code; the text is the toast.
    Reloaded(String),
}

impl HotReloadHost {
    /// Builds an empty app and watches `crate_dir/src`. Does not load the game.
    #[must_use]
    pub fn new(config: HotReloadConfig) -> Self {
        let src = config.crate_dir.join("src");
        let watch = match SourceWatch::open(&src) {
            Ok(watch) => Some(watch),
            Err(error) => {
                tracing::warn!(target: "regia_editor", "not watching {}: {error}", src.display());
                None
            }
        };
        let mut app = App::new();
        app.add_message::<HotReloaded>();
        // Only an editor that can host a dylib fingerprints at start: without `hot-dylib` every
        // reload is refused anyway, and the rlib fallback would hash the whole engine.
        let engine_baseline = if cfg!(feature = "hot-dylib") {
            Some(engine_fingerprint(&config.target_dir)).filter(|fingerprint| *fingerprint != 0)
        } else {
            None
        };
        Self {
            config,
            app,
            current: None,
            attempt: 0,
            mapped: 0,
            playing: true,
            paused: false,
            use_saved_snapshot: false,
            last_report: None,
            report_serial: 0,
            watch,
            debounce: Debounce::new(QUIET),
            in_flight: None,
            compile_owed: false,
            retry_snapshot: None,
            watch_retry_at: None,
            engine_baseline,
        }
    }

    /// The engine dylib's fingerprint now, when it is the engine this process started with.
    ///
    /// Refuses when the dylib is missing, or when its bytes differ from the start-up baseline
    /// ("engine changed — restart the editor"). The first nonzero fingerprint becomes the
    /// baseline when none was taken at start.
    fn check_engine(&mut self) -> Result<u64, (ReloadPhase, String)> {
        let now = engine_fingerprint(&self.config.target_dir);
        if now == 0 {
            return Err((
                ReloadPhase::Refused,
                "engine dylib is not in the target dir; build the editor with --features hot-dylib into that dir and restart"
                    .to_string(),
            ));
        }
        match self.engine_baseline {
            Some(baseline) if baseline != now => Err((
                ReloadPhase::Refused,
                "engine changed — restart the editor".to_string(),
            )),
            Some(_) => Ok(now),
            None => {
                self.engine_baseline = Some(now);
                Ok(now)
            }
        }
    }

    /// The world the game plugin registers into. Engine-owned resources live here too.
    #[must_use]
    pub fn world(&self) -> &World {
        self.app.world()
    }

    /// Insert engine resources (the sim handle, specs/25 § 3) before the first reload.
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Play-in-editor. [`HotReloadHost::frame`] runs an update only while playing and not paused.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    /// Pauses play-in-editor without unloading the game.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Last reload, if one has been attempted.
    #[must_use]
    pub fn last_report(&self) -> Option<&ReloadReport> {
        self.last_report.as_ref()
    }

    /// How many reload reports the host has published; changes when a build or refusal finishes
    /// (`agent.hot_reload`'s job matches its request by a changed serial, ADR-5707).
    #[must_use]
    pub fn report_serial(&self) -> u64 {
        self.report_serial
    }

    /// The currently loaded game's export table and generation, when one is running. PIE
    /// registers this into the play world (`specs/19` § 4).
    #[must_use]
    pub fn current_exports(&self) -> Option<(GameExports, u32)> {
        self.current
            .as_ref()
            .map(|plugin| (plugin.exports, plugin.generation))
    }

    /// The game crate's package name (`-p` argument of the build).
    #[must_use]
    pub fn package_name(&self) -> &str {
        &self.config.package
    }

    /// One frame of play-in-editor.
    #[tracing::instrument(skip_all, name = "hot_reload_frame")]
    pub fn frame(&mut self) {
        if self.playing && !self.paused {
            self.app.update();
        }
    }

    /// File trigger (`.regia/hot/trigger`), a debounced source change (500 ms, `specs/19` § 5)
    /// or a finished background build.
    ///
    /// Triggers and source changes start a background build ([`HotReloadHost::request_compile`]);
    /// the editor keeps running while cargo works, and the swap happens on the call that sees the
    /// build finished. The agent protocol's `agent.hot_reload` rides the same path: its handler
    /// sets `CompileRequest` (`crate::code_reload`) and the editor loop finishes the job it
    /// started once a report lands (`crate::code_reload::service_hot_reload_job`);
    /// [`HotReloadHost::status_json`] remains the state an out-of-process agent can read.
    #[tracing::instrument(skip_all, name = "hot_reload_poll")]
    pub fn poll(&mut self) -> Option<ReloadReport> {
        let trigger = self.config.trigger_path();
        let mut wanted = false;
        if trigger.is_file() {
            if let Err(error) = std::fs::remove_file(&trigger) {
                tracing::warn!(target: "regia_editor", "cannot remove trigger {}: {error}", trigger.display());
            }
            wanted = true;
        }
        self.reopen_watch();
        if self.watch.as_ref().is_some_and(SourceWatch::drain_rust_changes) {
            self.debounce.poke(Instant::now());
        }
        if self.debounce.take_ready(Instant::now()) {
            wanted = true;
        }
        if wanted && let Some(refused) = self.request_compile() {
            return Some(refused);
        }
        if let Some(finished) = self.finish_compile() {
            // A request owed by this build starts on the next call, so this report is not
            // overwritten before the caller sees it.
            return Some(finished);
        }
        if self.compile_owed && self.in_flight.is_none() {
            self.compile_owed = false;
            return self.request_compile();
        }
        None
    }

    /// Opens the source watch again when it failed to open (the crate's `src` did not exist
    /// yet, or the watcher could not start), at most once per [`WATCH_RETRY`].
    fn reopen_watch(&mut self) {
        if self.watch.is_some() {
            return;
        }
        let now = Instant::now();
        if self
            .watch_retry_at
            .is_some_and(|at| now.saturating_duration_since(at) < WATCH_RETRY)
        {
            return;
        }
        let src = self.config.crate_dir.join("src");
        match SourceWatch::open(&src) {
            Ok(watch) => {
                tracing::info!(target: "regia_editor", "watching {}", src.display());
                self.watch = Some(watch);
                self.watch_retry_at = None;
            }
            Err(_) => self.watch_retry_at = Some(now),
        }
    }

    /// Starts building the game library on a worker thread (the toolbar's Compile button,
    /// Ctrl+Alt+F11). While a build runs, the request is remembered and a fresh build starts
    /// when the running one finishes (that one read older sources). A precondition refusal (the
    /// editor cannot host a dylib, the engine dylib is missing) is reported at once.
    pub fn request_compile(&mut self) -> Option<ReloadReport> {
        if self.in_flight.is_some() {
            self.compile_owed = true;
            return None;
        }
        let started = Instant::now();
        if !cfg!(feature = "hot-dylib") {
            return Some(self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                "rebuild the editor with --features hot-dylib and -C prefer-dynamic so the game shares one engine copy"
                    .to_string(),
            )));
        }
        if !process_links_dynamic_std() {
            return Some(self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                GameLibraryError::StaticStd.to_string(),
            )));
        }
        let engine_before = match self.check_engine() {
            Ok(fingerprint) => fingerprint,
            Err((phase, detail)) => return Some(self.finish(self.keep_old(started, phase, detail))),
        };
        self.attempt = self.attempt.saturating_add(1);
        let generation = self.attempt;
        let build = self.config.game_build();
        let cancel = BuildCancel::default();
        let token = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("regia-game-build".into())
            .spawn(move || build_salted_dylib_cancellable(&build, generation, host_abi_hash(), &token));
        match spawned {
            Ok(handle) => {
                tracing::info!(target: "regia_editor", generation, "compiling game code");
                self.in_flight = Some(InFlight {
                    generation,
                    started,
                    engine_before,
                    cancel,
                    handle,
                });
                None
            }
            Err(error) => Some(self.finish(self.keep_old(
                started,
                ReloadPhase::BuildFailed,
                format!("cannot start the build thread: {error}"),
            ))),
        }
    }

    /// Stops the running background build (`specs/19` § 7): cargo is killed, nothing is copied
    /// into the hot dir, the old code keeps running and a build owed by a later request is dropped.
    /// The report lands on the next [`HotReloadHost::poll`]. Returns whether a build was running.
    pub fn cancel_compile(&self) -> bool {
        match &self.in_flight {
            Some(flight) => {
                flight.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// The running build's cancel token and seconds so far (the toolbar's Cancel and
    /// `editor.cancel_task { id: "game_build" }`, ADR-9770; the same token as
    /// [`HotReloadHost::cancel_compile`], ADR-7381).
    #[must_use]
    pub fn compile_task(&self) -> Option<(BuildCancel, f32)> {
        self.in_flight
            .as_ref()
            .map(|flight| (flight.cancel.clone(), flight.started.elapsed().as_secs_f32()))
    }

    /// True while a background build runs.
    #[must_use]
    pub fn is_compiling(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The toolbar state.
    #[must_use]
    pub fn compile_state(&self) -> CompileState {
        if let Some(flight) = &self.in_flight {
            return CompileState::Compiling(flight.started.elapsed().as_secs_f32());
        }
        match &self.last_report {
            None => CompileState::Idle,
            Some(report) if report.accepted => CompileState::Reloaded(report.toast.clone()),
            Some(report) => CompileState::Error(report.detail.clone()),
        }
    }

    /// Swaps in a finished background build (the fixed sequence of `specs/20` § 3).
    fn finish_compile(&mut self) -> Option<ReloadReport> {
        if !self.in_flight.as_ref().is_some_and(|f| f.handle.is_finished()) {
            return None;
        }
        let flight = self.in_flight.take()?;
        if self
            .current
            .as_ref()
            .is_some_and(|plugin| plugin.generation > flight.generation)
        {
            // A newer generation was accepted while this build ran: never swap older code in.
            tracing::info!(target: "regia_editor", generation = flight.generation, "dropping a superseded game build");
            let _ = flight.handle.join();
            return None;
        }
        let built = flight
            .handle
            .join()
            .unwrap_or_else(|_| Err(super::build_game::BuildFailure::panicked()));
        let built = match built {
            Ok(built) => built,
            Err(failure) => {
                let detail = failure.to_string();
                if failure.is_cancelled() || flight.cancel.is_cancelled() {
                    // A cancel also drops a build owed by a request made while this one ran.
                    self.compile_owed = false;
                    tracing::info!(target: "regia_editor", generation = flight.generation, "game build cancelled");
                } else {
                    tracing::error!(target: "regia_editor", "{detail}");
                }
                return Some(self.finish(self.keep_old(flight.started, ReloadPhase::BuildFailed, detail)));
            }
        };
        if engine_fingerprint(&self.config.target_dir) != flight.engine_before {
            return Some(self.finish(self.keep_old(
                flight.started,
                ReloadPhase::Refused,
                "engine changed — restart the editor".to_string(),
            )));
        }
        for warning in &built.warnings {
            tracing::warn!(target: "regia_editor", file = warning.file.as_deref().unwrap_or(""), "{}", warning.message);
        }
        self.note_mapped();
        match load_game_exports(&built.copied, host_abi_hash()) {
            Ok(exports) => Some(self.swap_in(
                flight.started,
                flight.generation,
                Prepared {
                    exports,
                    build_elapsed: built.elapsed,
                },
            )),
            Err(error) => {
                Some(self.finish(self.keep_old(flight.started, ReloadPhase::Refused, error.to_string())))
            }
        }
    }

    /// Build, then the fixed swap sequence.
    #[tracing::instrument(skip_all, name = "hot_reload")]
    pub fn reload(&mut self) -> ReloadReport {
        let started = Instant::now();
        // A background build started earlier read older sources than this one will: wait for
        // it (two cargo runs share the target dir anyway) and discard it, so the next `poll`
        // cannot swap that older generation in over this one (audit 11-03 dylib M2). A request
        // owed to it is served by this build.
        if let Some(flight) = self.in_flight.take() {
            tracing::info!(target: "regia_editor", generation = flight.generation, "a synchronous reload supersedes a background build");
            let _ = flight.handle.join();
            self.compile_owed = false;
        }
        // The default editor links engine rlibs. A game dylib would then be a second copy of
        // `TypeId` and the allocator (specs/20 § 1). Refuse that build instead of swapping.
        if !cfg!(feature = "hot-dylib") {
            return self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                "rebuild the editor with --features hot-dylib and -C prefer-dynamic so the game shares one engine copy"
                    .to_string(),
            ));
        }
        if !process_links_dynamic_std() {
            return self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                GameLibraryError::StaticStd.to_string(),
            ));
        }
        let before = match self.check_engine() {
            Ok(fingerprint) => fingerprint,
            Err((phase, detail)) => return self.finish(self.keep_old(started, phase, detail)),
        };
        self.attempt = self.attempt.saturating_add(1);
        let generation = self.attempt;
        match self.build_and_load(generation, before) {
            Ok(prepared) => self.swap_in(started, generation, prepared),
            Err((phase, detail)) => self.finish(self.keep_old(started, phase, detail)),
        }
    }

    /// JSON object written to `.regia/hot/status.json` as well as returned.
    #[must_use]
    pub fn status_json(&self) -> String {
        let (accepted, old_code_running, phase, generation, toast, elapsed_ms, playing, paused) =
            if let Some(report) = &self.last_report {
                (
                    report.accepted,
                    report.old_code_running,
                    report.phase.as_str(),
                    report.generation,
                    report.toast.as_str(),
                    u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
                    self.playing,
                    self.paused,
                )
            } else {
                (false, false, "idle", 0, "", 0, self.playing, self.paused)
            };
        let (scripts_rebuilt, scripts_dropped) = self
            .last_report
            .as_ref()
            .map_or((0, 0), |report| (report.scripts_rebuilt, report.scripts_dropped));
        format!(
            "{{\"generation\":{generation},\"accepted\":{accepted},\"old_code_running\":{old_code_running},\"phase\":\"{phase}\",\"toast\":\"{}\",\"elapsed_ms\":{elapsed_ms},\"playing\":{playing},\"paused\":{paused},\"scripts_rebuilt\":{scripts_rebuilt},\"scripts_dropped\":{scripts_dropped}}}",
            json_escape(toast)
        )
    }

    /// `before` is the engine fingerprint [`HotReloadHost::check_engine`] accepted.
    fn build_and_load(&mut self, generation: u32, before: u64) -> Result<Prepared, (ReloadPhase, String)> {
        let built = build_salted_dylib(&self.config.game_build(), generation, host_abi_hash()).map_err(
            |failure| {
                let detail = failure.to_string();
                tracing::error!(target: "regia_editor", "{detail}");
                (ReloadPhase::BuildFailed, detail)
            },
        )?;
        let after = engine_fingerprint(&self.config.target_dir);
        if after != before {
            return Err((
                ReloadPhase::Refused,
                "engine changed — restart the editor".to_string(),
            ));
        }
        for warning in &built.warnings {
            tracing::warn!(target: "regia_editor", file = warning.file.as_deref().unwrap_or(""), "{}", warning.message);
        }
        self.note_mapped();
        match load_game_exports(&built.copied, host_abi_hash()) {
            Ok(exports) => Ok(Prepared {
                exports,
                build_elapsed: built.elapsed,
            }),
            Err(error) => Err((ReloadPhase::Refused, error.to_string())),
        }
    }

    /// Counts a library about to be mapped (`load_game_exports` maps it even when its ABI check then
    /// refuses it) and logs the restart hint every [`RESTART_AFTER`] mappings.
    fn note_mapped(&mut self) {
        self.mapped = self.mapped.saturating_add(1);
        suggest_restart(self.mapped);
    }

    fn swap_in(&mut self, started: Instant, generation: u32, prepared: Prepared) -> ReloadReport {
        let paused_before = self.paused;
        self.paused = true;
        if let Some(current) = &self.current {
            let on_unload = current.exports.on_unload;
            let unloaded = std::panic::catch_unwind(AssertUnwindSafe(|| on_unload(self.app.world_mut())));
            if let Err(payload) = unloaded {
                self.paused = paused_before;
                return self.finish(self.keep_old(
                    started,
                    ReloadPhase::Refused,
                    format!("on_unload panicked: {}", panic_message(&payload)),
                ));
            }
        }

        let snapshot = match self.capture_snapshot() {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                self.paused = paused_before;
                return self.finish(self.keep_old(started, ReloadPhase::Refused, detail));
            }
        };

        let old = self.current.take();
        if let Some(previous) = &old {
            teardown_owner(self.app.world_mut(), previous.owner);
            self.app.forget_owner_plugins(previous.owner);
        }

        let owner = TypeOwner::Game(generation);
        let before = component_type_ids(self.app.world());
        let register = prepared.exports.register;
        let registered = std::panic::catch_unwind(AssertUnwindSafe(|| {
            with_game_owner(&mut self.app, owner, register);
        }));
        if let Err(payload) = registered {
            teardown_owner(self.app.world_mut(), owner);
            self.app.forget_owner_plugins(owner);
            self.rollback(old, &snapshot);
            self.paused = paused_before;
            return self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                format!("register panicked: {}", panic_message(&payload)),
            ));
        }

        let clashes = identity_clashes(self.app.world(), owner, &before);
        if !clashes.is_empty() {
            let names = clashes
                .iter()
                .map(|clash| clash.type_path.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            teardown_owner(self.app.world_mut(), owner);
            self.app.forget_owner_plugins(owner);
            self.rollback(old, &snapshot);
            self.paused = paused_before;
            return self.finish(self.keep_old(
                started,
                ReloadPhase::Refused,
                format!("identity check refused reload for {names}"),
            ));
        }

        let restored = std::panic::catch_unwind(AssertUnwindSafe(|| {
            restore_snapshot(self.app.world_mut(), &snapshot)
        }));
        let restored = match restored {
            Ok(report) => report,
            Err(payload) => {
                self.use_saved_snapshot = true;
                self.retry_snapshot = Some(snapshot);
                teardown_owner(self.app.world_mut(), owner);
                self.app.forget_owner_plugins(owner);
                self.current = None;
                self.paused = paused_before;
                let detail = format!(
                    "restore panicked: {}; snapshot kept at {}, retry reload",
                    panic_message(&payload),
                    self.config.snapshot_path().display()
                );
                tracing::error!(target: "regia_editor", "{detail}");
                return self.finish(ReloadReport {
                    accepted: false,
                    old_code_running: false,
                    toast: detail.clone(),
                    elapsed: started.elapsed(),
                    types: 0,
                    instances: 0,
                    scripts_rebuilt: 0,
                    scripts_dropped: 0,
                    detail,
                    generation: old.as_ref().map(|plugin| plugin.generation).unwrap_or(0),
                    phase: ReloadPhase::RestoreFailed,
                });
            }
        };

        let on_load = prepared.exports.on_load;
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| on_load(self.app.world_mut()))) {
            tracing::error!(target: "regia_editor", "on_load panicked: {}", panic_message(&payload));
        }

        let types = owned_reflected_types(self.app.world(), owner);
        let instances = snapshot
            .instance_count()
            .max(restored.components + restored.resources);
        self.current = Some(LoadedPlugin {
            owner,
            exports: prepared.exports,
            generation,
        });
        self.use_saved_snapshot = false;
        self.retry_snapshot = None;
        self.paused = paused_before;
        let elapsed = started.elapsed();
        let toast = toast_with_restart_hint(toast_line(elapsed, types, instances), self.mapped);
        tracing::info!(
            target: "regia_editor",
            build_ms = prepared.build_elapsed.as_millis(),
            total_ms = elapsed.as_millis(),
            "{toast}"
        );
        self.app.world_mut().write_message(HotReloaded {
            generation,
            types,
            instances,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        });
        self.finish(ReloadReport {
            accepted: true,
            old_code_running: false,
            toast: toast.clone(),
            elapsed,
            types,
            instances,
            scripts_rebuilt: restored.scripts_rebuilt,
            scripts_dropped: restored.scripts_dropped,
            detail: toast,
            generation,
            phase: ReloadPhase::Running,
        })
    }

    fn capture_snapshot(&mut self) -> Result<HotReloadSnapshot, String> {
        if self.use_saved_snapshot {
            if let Some(saved) = self.retry_snapshot.clone() {
                return Ok(saved);
            }
            let saved_path = self.config.snapshot_path();
            match read_snapshot(&saved_path) {
                Ok(saved) => return Ok(saved),
                Err(error) => {
                    // Refusing here refused every later reload too (audit 11-03 dylib M3).
                    let shown = saved_path.display();
                    tracing::warn!(
                        target: "regia_editor",
                        "cannot read the saved snapshot {shown}: {error}; reloading from the current world"
                    );
                    self.use_saved_snapshot = false;
                }
            }
        }
        let snapshot = if let Some(current) = &self.current {
            take_snapshot(self.app.world(), current.owner)
        } else {
            HotReloadSnapshot::default()
        };
        if let Err(error) = write_snapshot(&self.config.snapshot_path(), &snapshot) {
            tracing::error!(target: "regia_editor", "cannot write snapshot: {error}");
        }
        Ok(snapshot)
    }

    fn rollback(&mut self, old: Option<LoadedPlugin>, snapshot: &HotReloadSnapshot) {
        let Some(old) = old else {
            self.current = None;
            return;
        };
        let owner = old.owner;
        let register = old.exports.register;
        let on_load = old.exports.on_load;
        let registered = std::panic::catch_unwind(AssertUnwindSafe(|| {
            with_game_owner(&mut self.app, owner, register);
        }));
        if registered.is_err() {
            tracing::error!(target: "regia_editor", "rollback register panicked");
            self.current = None;
            return;
        }
        if std::panic::catch_unwind(AssertUnwindSafe(|| {
            restore_snapshot(self.app.world_mut(), snapshot)
        }))
        .is_err()
        {
            tracing::error!(target: "regia_editor", "rollback restore panicked");
        }
        if std::panic::catch_unwind(AssertUnwindSafe(|| on_load(self.app.world_mut()))).is_err() {
            tracing::error!(target: "regia_editor", "rollback on_load panicked");
        }
        self.current = Some(old);
    }

    fn keep_old(&self, started: Instant, phase: ReloadPhase, detail: String) -> ReloadReport {
        tracing::warn!(target: "regia_editor", phase = phase.as_str(), "{detail}");
        let generation = self.current.as_ref().map(|plugin| plugin.generation).unwrap_or(0);
        ReloadReport {
            accepted: false,
            old_code_running: self.current.is_some(),
            toast: format!("Reload failed; old code still running ({detail})"),
            elapsed: started.elapsed(),
            types: 0,
            instances: 0,
            scripts_rebuilt: 0,
            scripts_dropped: 0,
            detail,
            generation,
            phase,
        }
    }

    fn finish(&mut self, report: ReloadReport) -> ReloadReport {
        self.last_report = Some(report.clone());
        self.report_serial = self.report_serial.saturating_add(1);
        let path = self.config.status_path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::error!(target: "regia_editor", "cannot create {}: {error}", parent.display());
        }
        if let Err(error) = publish_status(&path, &self.status_json()) {
            tracing::error!(target: "regia_editor", "cannot write {}: {error}", path.display());
        }
        report
    }
}

/// True from the [`RESTART_AFTER`]-th mapped library on, when the editor suggests a restart
/// (`specs/20` § 3 step 11): the toast of every accepted reload carries the hint, and the log
/// repeats it every [`RESTART_AFTER`] libraries. Both the background compile and the synchronous
/// reload ask.
fn restart_suggested(mapped: u32) -> bool {
    mapped >= RESTART_AFTER
}

/// The toast with the restart hint appended once [`restart_suggested`] holds.
fn toast_with_restart_hint(toast: String, mapped: u32) -> String {
    if restart_suggested(mapped) {
        format!("{toast}; {mapped} libraries mapped, restart the editor to reclaim them")
    } else {
        toast
    }
}

fn suggest_restart(mapped: u32) {
    if restart_suggested(mapped) && mapped.is_multiple_of(RESTART_AFTER) {
        tracing::info!(
            target: "regia_editor",
            mapped,
            "hot reload has mapped {mapped} libraries; restart the editor to reclaim them"
        );
    }
}

/// Publishes `text` to `path` atomically: a pid-unique temp file next to `path`, then a rename
/// over it. `status.json` is polled by out-of-process agents, which must never see the empty
/// or torn document an in-place truncate-and-write shows them. A failed publish removes the
/// temp and leaves the previous status in place (same pattern as `snapshot::write_snapshot`).
/// Limitation: on Windows a rename-replace still fails when a reader holds the file without
/// delete sharing (the in-place write failure this replaces moves to the rename), so a
/// non-sharing reader still gets a silently stale status; the torn/empty read is what the
/// atomic publish fixes.
fn publish_status(path: &Path, text: &str) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(".{}-{serial}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    match std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn package_name(manifest: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(manifest).map_err(|error| error.to_string())?;
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package && let Some(rest) = trimmed.strip_prefix("name") {
            let name = rest.trim().trim_start_matches('=').trim().trim_matches('"');
            if !name.is_empty() && !name.contains(' ') {
                return Ok(name.to_string());
            }
        }
    }
    Err(format!("no package name in {}", manifest.display()))
}

fn find_workspace(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && text.lines().any(|line| line.trim() == "[workspace]")
        {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn json_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ecs::prelude::Messages;

    #[test]
    fn toast_matches_the_spec_shape() {
        let text = toast_line(Duration::from_millis(1300), 12, 340);
        assert_eq!(text, "Reloaded in 1.3 s (12 types, 340 instances)");
    }

    /// Audit G editor2-platform L3 (sprint-fix-editor2-platform): `status.json` is published
    /// through a process-unique temp file and a rename, so a polling agent never sees a torn
    /// document and two publishes in a row leave exactly one file, no temp litter. (The
    /// concurrent read-during-write property itself cannot be tested deterministically — a
    /// reader thread would need a sleep or a wall-clock budget, both banned in lane tests.)
    #[test]
    fn status_json_is_published_atomically() {
        let dir = regia_test_utils::unique_temp_dir("hot_status_publish");
        let path = dir.join("status.json");
        publish_status(&path, "{\"phase\":\"idle\"}").expect("first publish");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"phase\":\"idle\"}"
        );
        publish_status(&path, "{\"phase\":\"building\"}").expect("second publish");
        let text = std::fs::read_to_string(&path).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("status.json parses");
        assert_eq!(parsed["phase"], "building");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "exactly status.json, got {entries:?}");
        assert!(
            !entries[0].file_name().to_string_lossy().ends_with(".tmp"),
            "no temp file left: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_json_is_idle_before_a_reload() {
        let dir = std::env::temp_dir().join(format!("regia-hot-status-{}", std::process::id()));
        let config = HotReloadConfig {
            manifest_path: dir.join("Cargo.toml"),
            package: "hot_fixture".to_string(),
            crate_dir: dir.clone(),
            target_dir: dir.join("target"),
            hot_dir: dir.join("hot"),
        };
        let host = HotReloadHost::new(config);
        let json = host.status_json();
        assert!(json.contains("\"phase\":\"idle\""), "{json}");
        assert!(json.contains("\"generation\":0"), "{json}");
        assert!(json.contains("\"scripts_rebuilt\":0"), "{json}");
        assert!(json.contains("\"scripts_dropped\":0"), "{json}");
        assert!(host.world().contains_resource::<Messages<HotReloaded>>());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn temp_config(tag: &str) -> (PathBuf, HotReloadConfig) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let config = HotReloadConfig {
            manifest_path: dir.join("Cargo.toml"),
            package: "hot_fixture".to_string(),
            crate_dir: dir.clone(),
            target_dir: dir.join("target"),
            hot_dir: dir.join("hot"),
        };
        (dir, config)
    }

    /// Stands in for a running `cargo rustc`: the build ends (as a failure) when the sender is
    /// used or dropped.
    fn fake_flight(host: &mut HotReloadHost) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = rx.recv();
            Err(super::super::build_game::BuildFailure::panicked())
        });
        host.attempt = host.attempt.saturating_add(1);
        host.in_flight = Some(InFlight {
            generation: host.attempt,
            started: Instant::now(),
            engine_before: 0,
            cancel: BuildCancel::default(),
            handle,
        });
        tx
    }

    /// Spins (no sleep) until the fake build thread has returned.
    fn wait_built(host: &HotReloadHost) {
        while !host
            .in_flight
            .as_ref()
            .is_none_or(|flight| flight.handle.is_finished())
        {
            std::thread::yield_now();
        }
    }

    /// The request owed by a finished build is started by the next poll. Without `hot-dylib` it is
    /// refused at once, which makes the started request visible. With it the request either answers
    /// at once (a refusal: a test binary links std statically) or starts a build; a dropped request
    /// does neither (review rc1-rev-editor-k finding 7: these tests returned early under `hot-dylib`).
    fn assert_owed_request_started(host: &mut HotReloadHost, what: &str) {
        assert!(host.compile_owed, "the {what} made during the build was dropped");
        let next = host.poll();
        if cfg!(feature = "hot-dylib") {
            assert!(
                next.is_some() || host.is_compiling(),
                "the {what} made during the build was not started: {next:?}"
            );
        } else {
            assert!(
                next.as_ref()
                    .is_some_and(|report| report.phase == ReloadPhase::Refused),
                "the {what} made during the build was dropped: {next:?}"
            );
        }
        assert!(!host.compile_owed, "the owed {what} started once");
    }

    /// Audit 11-03 editor M3 / dylib M1: a trigger seen while a build runs was consumed and lost.
    #[test]
    fn a_trigger_during_a_build_starts_another_build_after_it() {
        let (dir, config) = temp_config("hot_pending_trigger");
        std::fs::create_dir_all(&config.hot_dir).expect("hot dir");
        let trigger = config.trigger_path();
        let mut host = HotReloadHost::new(config);
        let release = fake_flight(&mut host);
        std::fs::write(&trigger, b"").expect("trigger");
        assert_eq!(host.poll(), None, "the build is still running");
        drop(release);
        wait_built(&host);
        let finished = host.poll().expect("the running build finishes");
        assert_eq!(finished.phase, ReloadPhase::BuildFailed);
        // The edit made during that build is owed a build of its own.
        assert_owed_request_started(&mut host, "request");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `specs/19` § 7: cancelling the running game build stops it (the build thread sees its
    /// token), reports the cancel with the old code kept, and drops a build owed by a request made
    /// meanwhile.
    #[test]
    fn a_cancelled_compile_ends_reports_cancelled_and_owes_nothing() {
        if cfg!(feature = "hot-dylib") {
            return;
        }
        let (dir, config) = temp_config("hot_cancel");
        let mut host = HotReloadHost::new(config);
        assert!(!host.cancel_compile(), "nothing runs yet");
        host.attempt = host.attempt.saturating_add(1);
        let cancel = BuildCancel::default();
        let token = cancel.clone();
        let handle = std::thread::spawn(move || {
            // Stands in for a cargo that only stops when killed.
            while !token.is_cancelled() {
                std::thread::yield_now();
            }
            Err(super::super::build_game::BuildFailure::cancelled())
        });
        host.in_flight = Some(InFlight {
            generation: host.attempt,
            started: Instant::now(),
            engine_before: 0,
            cancel,
            handle,
        });
        assert_eq!(host.request_compile(), None, "a request during the build is owed");
        assert!(host.cancel_compile(), "a build was running");
        wait_built(&host);
        let report = host.poll().expect("the cancelled build reports");
        assert_eq!(report.phase, ReloadPhase::BuildFailed);
        assert!(!report.accepted);
        assert!(
            report.detail.contains(super::super::build_game::BUILD_CANCELLED),
            "{}",
            report.detail
        );
        assert_eq!(host.poll(), None, "the owed build was dropped with the cancel");
        assert!(!host.is_compiling());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit 11-03 dylib M1: the toolbar's Compile request during a build was dropped too.
    #[test]
    fn a_compile_request_during_a_build_is_kept() {
        let (dir, config) = temp_config("hot_pending_button");
        let mut host = HotReloadHost::new(config);
        let release = fake_flight(&mut host);
        assert_eq!(host.request_compile(), None, "busy: nothing to report yet");
        drop(release);
        wait_built(&host);
        assert!(
            host.poll()
                .is_some_and(|report| report.phase == ReloadPhase::BuildFailed)
        );
        assert_owed_request_started(&mut host, "Compile request");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit 11-03 dylib M2: a synchronous reload left the older background build in flight, and
    /// the next poll swapped it in over the newer generation.
    #[test]
    fn a_synchronous_reload_supersedes_a_background_build() {
        let (dir, config) = temp_config("hot_supersede");
        let mut host = HotReloadHost::new(config);
        let release = fake_flight(&mut host);
        drop(release);
        let _ = host.reload();
        assert!(
            !host.is_compiling(),
            "the older background build is still in flight"
        );
        assert_eq!(host.poll(), None, "the superseded build must not report or swap");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit 11-03 dylib M3: after a restore panic a missing or unreadable snapshot file refused
    /// every later reload for the rest of the session.
    #[test]
    fn an_unreadable_saved_snapshot_does_not_wedge_reloads() {
        let (dir, config) = temp_config("hot_saved_snapshot");
        let mut host = HotReloadHost::new(config);
        host.use_saved_snapshot = true;
        let snapshot = host.capture_snapshot();
        assert!(
            snapshot.is_ok(),
            "a missing saved snapshot refused the reload: {snapshot:?}"
        );
        assert!(!host.use_saved_snapshot, "the retry flag must not stay set");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit 11-03 dylib M4: a watch that failed to open (no `src` yet) was never retried.
    #[test]
    fn a_source_dir_created_after_start_is_watched() {
        let (dir, config) = temp_config("hot_watch_retry");
        let src = config.crate_dir.join("src");
        let mut host = HotReloadHost::new(config);
        assert!(host.watch.is_none(), "precondition: no src dir, no watch");
        std::fs::create_dir_all(&src).expect("src");
        let _ = host.poll();
        assert!(
            host.watch.is_some(),
            "the watch was not reopened once src appeared"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Writes a stand-in engine dylib where `engine_fingerprint` looks for it.
    fn write_engine_dylib(config: &HotReloadConfig, bytes: &[u8]) {
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let deps = config.target_dir.join(profile).join("deps");
        std::fs::create_dir_all(&deps).expect("deps");
        let name = format!("libregia_dylib-cafe.{}", super::super::load::dylib_extension());
        std::fs::write(deps.join(name), bytes).expect("engine dylib");
    }

    /// Specs/20 § 1 (ADR-5627): the editor fingerprints the engine dylib at start and refuses a
    /// game library once the dylib was relinked. Before the fix the host compared only the
    /// fingerprints before and after its own build, so a relink between two reloads (a terminal
    /// `cargo build`, or a refused reload whose build relinked the engine) let the next reload
    /// load a game linked against an engine this process never mapped.
    #[test]
    fn an_engine_relinked_between_reloads_is_refused() {
        let (dir, config) = temp_config("hot_engine_baseline");
        write_engine_dylib(&config, b"engine mapped at start");
        let check = config.clone();
        let mut host = HotReloadHost::new(config);
        assert!(host.check_engine().is_ok(), "the start-up engine is accepted");
        assert!(host.check_engine().is_ok(), "and stays accepted while unchanged");
        write_engine_dylib(&check, b"engine relinked by another build");
        let refused = host.check_engine();
        assert!(
            matches!(&refused, Err((ReloadPhase::Refused, detail)) if detail.contains("engine changed")),
            "a relink between reloads must be refused: {refused:?}"
        );
        assert!(
            host.check_engine().is_err(),
            "every later reload is refused too, until a restart"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// No engine dylib at start: the first one found becomes the baseline, and a missing one is
    /// refused with the build hint.
    #[test]
    fn a_missing_engine_dylib_is_refused_and_the_first_found_is_the_baseline() {
        let (dir, config) = temp_config("hot_engine_missing");
        let check = config.clone();
        let mut host = HotReloadHost::new(config);
        let missing = host.check_engine();
        assert!(
            matches!(&missing, Err((ReloadPhase::Refused, detail)) if detail.contains("not in the target dir")),
            "{missing:?}"
        );
        write_engine_dylib(&check, b"first engine");
        assert!(host.check_engine().is_ok());
        write_engine_dylib(&check, b"second engine");
        assert!(
            host.check_engine().is_err(),
            "the first engine found is the baseline"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Specs/20 § 3 step 11: a restart is suggested after about 50 reloads, on both the
    /// background-compile path and the synchronous one (the background path, which the editor
    /// uses, never suggested it before ADR-5627).
    #[test]
    fn a_restart_is_suggested_after_fifty_libraries() {
        assert!(!restart_suggested(1));
        assert!(!restart_suggested(RESTART_AFTER - 1));
        assert!(restart_suggested(RESTART_AFTER));
        assert!(restart_suggested(RESTART_AFTER + 7));
        let toast = toast_line(Duration::from_millis(1300), 12, 340);
        assert_eq!(toast_with_restart_hint(toast.clone(), 3), toast);
        let hinted = toast_with_restart_hint(toast.clone(), RESTART_AFTER);
        assert!(hinted.starts_with(&toast), "{hinted}");
        assert!(hinted.contains("restart the editor"), "{hinted}");
    }

    /// Review rc1-rev-editor-k finding 8: the hint counted build attempts, so failed and refused
    /// builds (which map nothing) pushed it toward "50 libraries mapped". It counts mappings now.
    #[test]
    fn failed_builds_do_not_count_toward_the_restart_hint() {
        let (dir, config) = temp_config("hot_mapped_count");
        let mut host = HotReloadHost::new(config);
        for _ in 0..RESTART_AFTER + 2 {
            drop(fake_flight(&mut host));
            wait_built(&host);
            assert!(
                host.poll()
                    .is_some_and(|report| report.phase == ReloadPhase::BuildFailed)
            );
        }
        assert_eq!(host.attempt, RESTART_AFTER + 2, "every attempt salts a new path");
        assert_eq!(host.mapped, 0, "a failed build maps no library");
        assert!(!restart_suggested(host.mapped));
        host.note_mapped();
        assert_eq!(host.mapped, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_without_the_dylib_feature_does_not_touch_the_world() {
        if cfg!(feature = "hot-dylib") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("regia-hot-refuse-{}", std::process::id()));
        let config = HotReloadConfig {
            manifest_path: dir.join("Cargo.toml"),
            package: "hot_fixture".to_string(),
            crate_dir: dir.clone(),
            target_dir: dir.join("target"),
            hot_dir: dir.join("hot"),
        };
        let mut host = HotReloadHost::new(config);
        let report = host.reload();
        assert!(!report.accepted);
        assert!(!report.old_code_running);
        assert_eq!(report.phase, ReloadPhase::Refused);
        assert!(report.detail.contains("hot-dylib"), "{report:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
