// SPDX-License-Identifier: MPL-2.0
//! Engine-shader hot reload for dev builds (`docs/specs/12-assets.md` § 8, `specs/13` § 7).
//!
//! Every engine shader is composed through `shader_lib`, keyed by its file name
//! (`lab_forward.wgsl`, `pbr_lighting.wgsl`, …). A dev build may replace that text at runtime:
//!
//! 1. [`stage_shader_edit`] takes the edited file, composes it against the library, and validates
//!    with naga every engine entry shader that can see it (all entries for a library module; the
//!    shader itself, with each of its known shader-def sets, for an entry). Nothing changes on
//!    error: the old pipelines stay and the error comes back for the log and the editor.
//! 2. On success the override is committed and [`shader_epoch`] advances.
//! 3. Each [`crate::FrameRenderer`] compares its epoch before a frame and rebuilds its pipelines
//!    from the new text ([`crate::FrameRenderer::sync_shader_epoch`]).
//!
//! A composition that still fails later (a shader-def combination the validation set does not
//! cover) falls back to the embedded text for that build and logs once, so a bad edit never
//! breaks a frame. Shipping builds never stage an edit, so the map stays empty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use naga_oil::compose::ShaderDefValue;
use regia_app::prelude::*;

/// File name → edited WGSL.
pub(crate) type OverrideMap = HashMap<String, Arc<str>>;

type OverrideCell = RwLock<Option<Arc<OverrideMap>>>;

#[cfg(not(test))]
static OVERRIDES: OverrideCell = RwLock::new(None);
#[cfg(not(test))]
static EPOCH: AtomicU64 = AtomicU64::new(0);
#[cfg(not(test))]
static SEQUENCES: SequenceCell = Mutex::new(None);

// This crate's unit tests run in parallel in one process: an edit staged by a `shader_reload`
// test must not make a renderer in another test rebuild mid-frame, so each test thread gets its
// own state. Integration tests and real builds share one process-wide state.
#[cfg(test)]
thread_local! {
    // The initializers are `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static OVERRIDES: OverrideCell = const { RwLock::new(None) };
    #[allow(clippy::missing_const_for_thread_local)]
    static EPOCH: AtomicU64 = const { AtomicU64::new(0) };
    #[allow(clippy::missing_const_for_thread_local)]
    static SEQUENCES: SequenceCell = const { Mutex::new(None) };
}

/// Per file: the sequence number the next staged edit takes, and the newest one committed.
/// Validations run on their own threads and finish in any order: an edit staged before another
/// of the same file must not commit after it (audit-v035-conc M4).
type SequenceCell = Mutex<Option<HashMap<String, (u64, u64)>>>;

#[cfg(not(test))]
fn with_sequences<R>(f: impl FnOnce(&mut HashMap<String, (u64, u64)>) -> R) -> R {
    let mut guard = SEQUENCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(HashMap::new))
}

#[cfg(test)]
fn with_sequences<R>(f: impl FnOnce(&mut HashMap<String, (u64, u64)>) -> R) -> R {
    SEQUENCES.with(|cell| {
        let mut guard = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(guard.get_or_insert_with(HashMap::new))
    })
}

/// Takes the next sequence number for an edit of `file` (call it when the edit's text is read).
fn next_sequence(file: &str) -> u64 {
    with_sequences(|seqs| {
        let entry = seqs.entry(file.to_string()).or_insert((1, 0));
        let seq = entry.0;
        entry.0 += 1;
        seq
    })
}

#[cfg(not(test))]
fn with_overrides<R>(f: impl FnOnce(&OverrideCell) -> R) -> R {
    f(&OVERRIDES)
}

#[cfg(not(test))]
fn with_epoch<R>(f: impl FnOnce(&AtomicU64) -> R) -> R {
    f(&EPOCH)
}

#[cfg(test)]
fn with_overrides<R>(f: impl FnOnce(&OverrideCell) -> R) -> R {
    OVERRIDES.with(f)
}

#[cfg(test)]
fn with_epoch<R>(f: impl FnOnce(&AtomicU64) -> R) -> R {
    EPOCH.with(f)
}

/// The shader library's composable modules (`#import regia::…`). An edit to one revalidates
/// every entry shader.
pub(crate) const LIBRARY_MODULES: [&str; 23] = [
    "utils.wgsl",
    "view.wgsl",
    "mesh.wgsl",
    "pbr_surface.wgsl",
    "foliage_sway.wgsl",
    "pbr_lighting.wgsl",
    "pbr_brdf.wgsl",
    "shading_models.wgsl",
    "shadows.wgsl",
    "clustering.wgsl",
    "fog.wgsl",
    "gi.wgsl",
    "aerial.wgsl",
    "sssr_common.wgsl",
    "cloud_shadow.wgsl",
    "cloud_density.wgsl",
    "forward_ltc.wgsl",
    "forward_occluder_geometry.wgsl",
    "forward_occlusion.wgsl",
    "forward_env_ground.wgsl",
    "forward_util.wgsl",
    "forward_surface_weather.wgsl",
    "forward_decals.wgsl",
];

/// Advances on every committed shader edit. Renderers built before the current epoch rebuild.
#[must_use]
pub fn shader_epoch() -> u64 {
    with_epoch(|epoch| epoch.load(Ordering::Acquire))
}

/// The committed overrides (`None` when no edit was ever committed).
pub(crate) fn current_overrides() -> Option<Arc<OverrideMap>> {
    with_overrides(|cell| cell.read().ok().and_then(|guard| guard.clone()))
}

/// The engine's WGSL directory in this build's source checkout. It exists in a dev checkout
/// (the editor and `regia run --dev` watch it); a shipped game never has it.
///
/// `None` without the `dev-assets` feature: the path is the build machine's, and a packaged
/// binary must not embed it (`regia package` rejects binaries that contain the source path).
/// With the feature, a debug build names the checkout it was compiled in; a release build (a
/// downloaded editor) carries no build path and looks the directory up at run time, in
/// `REGIA_ENGINE_DIR` or the checkout around the executable (ADR-10354, ADR-10544).
#[must_use]
pub fn engine_shader_dir() -> Option<PathBuf> {
    #[cfg(all(any(test, feature = "dev-assets"), debug_assertions))]
    {
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders"))
    }
    #[cfg(all(any(test, feature = "dev-assets"), not(debug_assertions)))]
    {
        let exe = std::env::current_exe().ok();
        find_engine_shader_dir(
            regia_core::brand::env_var("REGIA_ENGINE_DIR")
                .ok()
                .map(PathBuf::from),
            exe.as_deref(),
        )
    }
    #[cfg(not(any(test, feature = "dev-assets")))]
    {
        None
    }
}

/// The release-build lookup of [`engine_shader_dir`]: `crates/regia_render/shaders` under the
/// override, else under the nearest ancestor of the executable that has it.
#[cfg(any(test, all(feature = "dev-assets", not(debug_assertions))))]
fn find_engine_shader_dir(override_dir: Option<PathBuf>, exe: Option<&Path>) -> Option<PathBuf> {
    let ancestors = exe
        .into_iter()
        .flat_map(|exe| exe.ancestors().skip(1).map(Path::to_path_buf));
    override_dir
        .into_iter()
        .chain(ancestors)
        .map(|root| root.join("crates/regia_render/shaders"))
        .find(|dir| dir.is_dir())
}

/// True when `path` names one of the engine's shaders (by file name, in [`engine_shader_dir`]).
#[must_use]
pub fn is_engine_shader(path: &Path) -> bool {
    let Some(engine) = engine_shader_dir() else {
        return false;
    };
    path.extension().is_some_and(|e| e == "wgsl")
        && path
            .parent()
            .is_some_and(|dir| dir == engine || dir.canonicalize().ok() == engine.canonicalize().ok())
}

/// What a committed edit revalidated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderSwap {
    /// The edited file name.
    pub file: String,
    /// Entry-shader compositions validated (shader-def sets counted separately).
    pub validated: usize,
    /// The new [`shader_epoch`].
    pub epoch: u64,
    /// `true` when a newer edit of the same file was issued before this one committed (it may
    /// have failed validation): the override map is unchanged and any error it reported stays.
    pub superseded: bool,
}

/// Validates an edit of `file` (a file name such as `pbr_lighting.wgsl`) and commits it.
///
/// # Errors
///
/// The composer or naga's validator rejected a composition; the text is the diagnostic. The
/// previous shaders stay live.
pub fn stage_shader_edit(file: &str, text: &str) -> Result<ShaderSwap, String> {
    stage_sequenced(file, text, next_sequence(file))
}

/// [`stage_shader_edit`] for an edit that took sequence number `seq` when its text was read.
fn stage_sequenced(file: &str, text: &str, seq: u64) -> Result<ShaderSwap, String> {
    let _span = tracing::info_span!("stage_shader_edit", file).entered();
    let mut staged: OverrideMap = current_overrides().map(|m| (*m).clone()).unwrap_or_default();
    staged.insert(file.to_string(), Arc::from(text));
    let validated = validate_with(&staged, file)?;
    let (epoch, applied) = commit_override(file, Arc::from(text), seq)?;
    Ok(ShaderSwap {
        file: file.to_string(),
        validated,
        epoch,
        superseded: !applied,
    })
}

/// Stores one override and advances the epoch, unless a later edit of the same file (a higher
/// issued sequence, committed or not) exists: then the older text is dropped and the epoch stays.
/// A newer edit that *failed* validation still blocks an older one: the file on disk is the
/// rejected text, so committing the older save would hide the failure. The read-modify-write
/// happens under the write lock, so edits to different files staged concurrently both survive
/// (each commit merges its own key into the map as it is now, not into the map it validated
/// against). Returns the epoch and whether this commit was applied.
fn commit_override(file: &str, text: Arc<str>, seq: u64) -> Result<(u64, bool), String> {
    with_overrides(|cell| {
        let mut guard = cell
            .write()
            .map_err(|_| "shader override lock poisoned".to_string())?;
        let superseded = with_sequences(|seqs| {
            let entry = seqs.entry(file.to_string()).or_insert((seq + 1, 0));
            // The newest sequence issued for this file: a failed newer edit never commits, so
            // `entry.1` alone cannot tell an older commit to drop.
            let newest_issued = entry.0.saturating_sub(1);
            if seq < newest_issued {
                return true;
            }
            entry.1 = seq;
            false
        });
        if superseded {
            tracing::debug!(target: "regia_render", file, seq, "shader edit superseded by a newer one; dropped");
            return Ok((with_epoch(|epoch| epoch.load(Ordering::Acquire)), false));
        }
        let mut merged: OverrideMap = guard.as_ref().map(|m| (**m).clone()).unwrap_or_default();
        merged.insert(file.to_string(), text);
        *guard = Some(Arc::new(merged));
        Ok((with_epoch(|epoch| epoch.fetch_add(1, Ordering::AcqRel)) + 1, true))
    })
}

/// Reads `path` and stages it ([`stage_shader_edit`] keyed by its file name).
///
/// # Errors
///
/// The file could not be read, or validation failed.
pub fn stage_shader_file(path: &Path) -> Result<ShaderSwap, String> {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a file name", path.display()))?;
    // The sequence number and the read are taken together, so sequence order is read order.
    let (text, seq) = with_sequences(|seqs| {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let entry = seqs.entry(file.to_string()).or_insert((1, 0));
        let seq = entry.0;
        entry.0 += 1;
        Ok::<_, String>((text, seq))
    })?;
    stage_sequenced(file, &text, seq)
}

/// Drops every override (back to the embedded shaders) and advances the epoch.
pub fn clear_shader_overrides() {
    with_overrides(|cell| {
        if let Ok(mut guard) = cell.write() {
            *guard = None;
            // Inside the lock: a commit cannot interleave between the clear and its epoch.
            with_epoch(|epoch| epoch.fetch_add(1, Ordering::AcqRel));
        }
    });
}

/// Composes and validates the entries `file` affects under `overrides`. Returns how many.
fn validate_with(overrides: &OverrideMap, file: &str) -> Result<usize, String> {
    let library = LIBRARY_MODULES.contains(&file);
    let own_text = || overrides.get(file).map(|t| t.to_string()).unwrap_or_default();
    let mut jobs: Vec<(String, String, HashMap<String, ShaderDefValue>)> =
        crate::shader_check::shader_sources()
            .into_iter()
            .filter(|(name, _, _)| library || *name == file)
            .map(|(name, source, defs)| (name.to_string(), source.to_string(), defs))
            .collect();
    if library || jobs.is_empty() {
        // A library module: composing an entry keeps only the library items it reaches, so an
        // error in a function no entry calls yet would slip through: validate the module's own
        // text as well. An entry outside the frame's validation set: compose it as written.
        jobs.push((file.to_string(), own_text(), HashMap::new()));
    }
    let results = validate_parallel(overrides, &jobs);
    // The first failing entry in list order, so the reported error does not depend on timing.
    results.into_iter().collect::<Result<Vec<()>, String>>()?;
    Ok(jobs.len())
}

/// Validates `jobs` on up to `available_parallelism` scoped threads (a library module reaches
/// every engine entry; one thread took over a second on lavapipe hosts). Results keep job order.
fn validate_parallel(
    overrides: &OverrideMap,
    jobs: &[(String, String, HashMap<String, ShaderDefValue>)],
) -> Vec<Result<(), String>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(jobs.len())
        .max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<Result<(), String>> = Vec::with_capacity(jobs.len());
    results.resize_with(jobs.len(), || Ok(()));
    let done: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((name, source, defs)) = jobs.get(i) else {
                            break out;
                        };
                        out.push((i, validate_one(overrides, name, source, defs.clone())));
                    }
                })
            })
            .collect();
        // `Err(())` marks a worker that panicked: its finished results are lost with it.
        workers.into_iter().map(|w| w.join().map_err(|_| ())).collect()
    });
    let mut panicked_workers = 0usize;
    for worker in done {
        match worker {
            Ok(finished) => {
                for (i, result) in finished {
                    if let Some(slot) = results.get_mut(i)
                        && slot.is_ok()
                    {
                        *slot = result;
                    }
                }
            }
            Err(()) => panicked_workers += 1,
        }
    }
    if panicked_workers > 0 {
        // A panicked worker's finished results died with it: every job still unmarked was
        // claimed by it (or never ran), so fail it rather than silently passing the edit —
        // and never attach the panic to job 0 specifically.
        let err = "shader validation worker panicked".to_string();
        for slot in results.iter_mut().filter(|slot| slot.is_ok()) {
            *slot = Err(err.clone());
        }
    }
    results
}

fn validate_one(
    overrides: &OverrideMap,
    name: &str,
    embedded: &str,
    defs: HashMap<String, ShaderDefValue>,
) -> Result<(), String> {
    let module = crate::shader_lib::compose_with_overrides(embedded, name, defs, &[], Some(overrides))
        .map_err(|e| format!("{name}: {e}"))?;
    naga_validate(&module).map_err(|e| format!("{name}: {e}"))
}

/// naga's validator with every capability a desktop device may expose (the renderer gates
/// features at pipeline creation; this catches type, binding and control-flow errors).
pub(crate) fn naga_validate(module: &naga::Module) -> Result<(), String> {
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .map(|_| ())
    .map_err(|e| e.emit_to_string(""))
}

/// One shader edit validating on a worker thread.
type ShaderJob = std::thread::JoinHandle<Result<ShaderSwap, String>>;

/// Shader edits being validated on worker threads, polled each frame by
/// [`reload_engine_shaders`].
#[derive(Resource)]
pub struct ShaderReloadJobs {
    pending: Vec<(PathBuf, std::time::Instant, ShaderJob)>,
    /// The last error, for the editor's status line (`None` after a successful swap).
    pub last_error: Option<String>,
    /// Directories whose `.wgsl` files are engine shaders (keyed by file name). Defaults to
    /// [`engine_shader_dir`] (empty without `dev-assets`); tests point it at a scratch copy.
    pub engine_dirs: Vec<PathBuf>,
}

impl Default for ShaderReloadJobs {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            last_error: None,
            engine_dirs: engine_shader_dir().into_iter().collect(),
        }
    }
}

impl ShaderReloadJobs {
    /// True when `path` is a `.wgsl` file directly inside one of [`Self::engine_dirs`].
    #[must_use]
    pub fn is_engine_path(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "wgsl")
            && path.parent().is_some_and(|dir| {
                self.engine_dirs.iter().any(|engine| {
                    dir == engine
                        || (dir.canonicalize().ok().is_some()
                            && dir.canonicalize().ok() == engine.canonicalize().ok())
                })
            })
    }

    /// Edits still validating.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// `Update`: an engine `.wgsl` changed on disk ([`regia_asset::SourceAssetChanged`]) → validate
/// it off the main thread → commit (renderers rebuild on their next frame) or keep the old
/// pipelines and report the error to the log and [`regia_asset::AssetWatchStatus`].
pub fn reload_engine_shaders(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    mut jobs: ResMut<ShaderReloadJobs>,
    mut status: Option<ResMut<regia_asset::AssetWatchStatus>>,
    assets: Option<Res<regia_asset::AssetServer>>,
) {
    for change in changes.read() {
        if change.kind != regia_asset::SourceKind::Shader {
            continue;
        }
        if !jobs.is_engine_path(&change.path) {
            tracing::debug!(target: "regia_render", path = %change.path.display(), "not an engine shader; ignored");
            continue;
        }
        let path = change.path.clone();
        // Registered (ADR-287) as CPU work (naga validation): listed at exit, never waited for.
        let spawned = regia_platform::GpuWorkerBuilder::new("regia-shader-reload")
            .driver_calls_marked()
            .spawn(move |_| stage_shader_file(&path));
        if let Ok(handle) = spawned {
            jobs.pending
                .push((change.path.clone(), std::time::Instant::now(), handle));
        }
    }
    if jobs.pending.is_empty() {
        return;
    }
    let mut still = Vec::new();
    let pending = std::mem::take(&mut jobs.pending);
    for (path, started, handle) in pending {
        if !handle.is_finished() {
            still.push((path, started, handle));
            continue;
        }
        let result = handle
            .join()
            .unwrap_or_else(|_| Err("shader validation thread panicked".to_string()));
        let shown = match assets.as_deref() {
            Some(server) => regia_asset::display_path(server.project_root(), &path),
            None => path.display().to_string(),
        };
        let outcome = match result {
            Ok(swap) => {
                tracing::info!(target: "regia_render", file = %swap.file, validated = swap.validated, epoch = swap.epoch, "shader edit committed");
                // A superseded commit (a newer edit of the same file was issued, and may have
                // failed) must not clear the error that newer edit reported.
                if !swap.superseded {
                    jobs.last_error = None;
                }
                regia_asset::WatchOutcome::Reloaded
            }
            Err(err) => {
                tracing::error!(target: "regia_render", "shader error in {shown} (old pipelines kept): {err}");
                jobs.last_error = Some(format!("{shown}: {err}"));
                regia_asset::WatchOutcome::Error(err)
            }
        };
        if let Some(status) = status.as_deref_mut() {
            status.record(regia_asset::WatchEntry {
                path: shown,
                kind: regia_asset::SourceKind::Shader,
                millis: started.elapsed().as_millis() as u64,
                outcome,
            });
        }
    }
    jobs.pending = still;
}

/// Serializes tests that touch the process-wide override map.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    const LIGHTING: &str = include_str!("../shaders/pbr_lighting.wgsl");

    /// ADR-10544: a release editor finds the engine shaders from the override or the checkout
    /// around its executable, and none when it runs from a download.
    #[test]
    fn release_builds_find_the_engine_shaders_at_run_time() {
        let root = regia_test_utils::unique_temp_dir("engine-shader-dir");
        let shaders = root.join("engine/crates/regia_render/shaders");
        std::fs::create_dir_all(&shaders).expect("shader dir");
        let inside = root.join("engine/target/release/regia-editor");
        let download = root.join("download/regia-editor");
        assert_eq!(find_engine_shader_dir(None, Some(&inside)), Some(shaders.clone()));
        assert_eq!(find_engine_shader_dir(None, Some(&download)), None);
        assert_eq!(find_engine_shader_dir(None, None), None);
        assert_eq!(
            find_engine_shader_dir(Some(root.join("engine")), Some(&download)),
            Some(shaders)
        );
        assert_eq!(find_engine_shader_dir(Some(root.join("download")), None), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_valid_library_edit_commits_and_advances_the_epoch() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_shader_overrides();
        let before = shader_epoch();
        let edited = format!("{LIGHTING}\nfn hot_reload_probe() -> f32 {{ return 1.0; }}\n");
        let swap = stage_shader_edit("pbr_lighting.wgsl", &edited).expect("valid edit");
        assert!(swap.validated >= 5, "every entry revalidated: {swap:?}");
        assert_eq!(shader_epoch(), before + 1);
        assert!(current_overrides().is_some_and(|m| m.contains_key("pbr_lighting.wgsl")));
        clear_shader_overrides();
    }

    #[test]
    fn a_broken_edit_is_rejected_and_the_old_text_stays() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_shader_overrides();
        let before = shader_epoch();
        let broken = LIGHTING.replacen("fn ", "fn fn ", 1);
        let err = stage_shader_edit("pbr_lighting.wgsl", &broken).expect_err("must not commit");
        assert!(!err.is_empty());
        assert_eq!(shader_epoch(), before, "no swap on error");
        assert!(current_overrides().is_none());
        // A type error the parser accepts but the validator rejects.
        let typed = format!("{LIGHTING}\nfn bad_probe() -> f32 {{ return vec3<f32>(1.0); }}\n");
        assert!(stage_shader_edit("pbr_lighting.wgsl", &typed).is_err());
        assert_eq!(shader_epoch(), before);
    }

    #[test]
    fn commits_staged_from_the_same_base_both_survive() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_shader_overrides();
        let before = shader_epoch();
        // Two edits validated against the same (empty) base, committed one after the other:
        // the second must merge into the map as it is now, not replace it.
        commit_override("a.wgsl", Arc::from("fn a() {}"), next_sequence("a.wgsl")).expect("commit a");
        commit_override("b.wgsl", Arc::from("fn b() {}"), next_sequence("b.wgsl")).expect("commit b");
        let map = current_overrides().expect("overrides present");
        assert!(map.contains_key("a.wgsl") && map.contains_key("b.wgsl"));
        assert_eq!(shader_epoch(), before + 2);
        clear_shader_overrides();
    }

    /// audit-v035-conc M4: two edits of one file validate on their own threads; when the older
    /// one finishes last it committed over the newer one, leaving the old text live with no
    /// event to correct it. The older edit's commit is now dropped.
    #[test]
    fn an_older_edit_finishing_last_does_not_replace_a_newer_one() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_shader_overrides();
        let first = next_sequence("m4.wgsl");
        let second = next_sequence("m4.wgsl");
        assert!(second > first);
        let before = shader_epoch();
        // The second edit's validation finishes first.
        commit_override("m4.wgsl", Arc::from("fn v2() {}"), second).expect("commit v2");
        commit_override("m4.wgsl", Arc::from("fn v1() {}"), first).expect("late v1");
        let map = current_overrides().expect("overrides present");
        assert_eq!(
            map.get("m4.wgsl").map(|t| t.to_string()).as_deref(),
            Some("fn v2() {}")
        );
        assert_eq!(
            shader_epoch(),
            before + 1,
            "the dropped edit does not advance the epoch"
        );
        // A later edit still commits.
        let third = next_sequence("m4.wgsl");
        commit_override("m4.wgsl", Arc::from("fn v3() {}"), third).expect("commit v3");
        let map = current_overrides().expect("overrides present");
        assert_eq!(
            map.get("m4.wgsl").map(|t| t.to_string()).as_deref(),
            Some("fn v3() {}")
        );
        clear_shader_overrides();
    }

    /// audit-render9: a newer edit that fails validation still blocks an older edit of the same
    /// file from committing afterwards: the file on disk is the rejected text, so the older
    /// save must not land and clear the error.
    #[test]
    fn a_failed_newer_edit_blocks_an_older_commit() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_shader_overrides();
        let first = next_sequence("pbr_lighting.wgsl");
        let second = next_sequence("pbr_lighting.wgsl");
        assert!(second > first);
        let before = shader_epoch();
        // The newer edit fails validation (never reaches `commit_override`).
        let broken = LIGHTING.replacen("fn ", "fn fn ", 1);
        assert!(stage_sequenced("pbr_lighting.wgsl", &broken, second).is_err());
        // The older edit validates and commits late: dropped, the map unchanged.
        let edited = format!("{LIGHTING}\nfn render9_probe() -> f32 {{ return 1.0; }}\n");
        let swap = stage_sequenced("pbr_lighting.wgsl", &edited, first).expect("valid edit");
        assert!(
            swap.superseded,
            "a newer edit was issued: the late commit is dropped"
        );
        assert_eq!(
            shader_epoch(),
            before,
            "the dropped edit does not advance the epoch"
        );
        assert!(
            current_overrides().is_none(),
            "the override map stays unchanged while the newer edit is rejected"
        );
        clear_shader_overrides();
    }

    #[test]
    fn engine_shader_paths_are_recognised() {
        let dir = engine_shader_dir().expect("unit tests see the source checkout");
        assert!(is_engine_shader(&dir.join("lab_forward.wgsl")));
        assert!(!is_engine_shader(Path::new("/tmp/game/assets/water.wgsl")));
    }
}
