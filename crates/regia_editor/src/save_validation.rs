// SPDX-License-Identifier: MPL-2.0
//! The save trigger of the asset validators (spec 32 § 4.5, ADR-1668, ADR-2755).
//!
//! Every save a user or an agent asks for goes through [`crate::level_io::save_level_and_terrain`],
//! which calls [`note_saved`]. That is a queue insert and nothing else: saving is never blocked or
//! slowed by validation. [`tick_save_validation`] runs once per editor frame (and `editor.save_validation`
//! can drive it): it takes the paths the gate batched (`SaveValidationGate`), starts one run on a worker
//! thread with `Trigger::Save` and `Scope::Changed`, and a later tick hands the finished report to the
//! Problems panel model (`AssetPanels`), where `editor.problems_panel` reads it too.
//!
//! A batch tool holds the gate (`hold`) while it saves many files; the paths keep queuing and run
//! once, on the first tick after the last hold is released.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use regia_asset_pipeline::asset_graph::AssetSet;
use regia_asset_pipeline::dialogue_validator::dialogue_assets_validator;
use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_asset_pipeline::validation_triggers::{SaveHold, SaveValidationGate};
use regia_asset_pipeline::validators::{RunSettings, Scope, Trigger, ValidationReport, ValidatorRegistry};
use regia_ecs::prelude::*;

use crate::asset_panels::{AssetPanels, project_rules, registry_set};

/// A finished (or failed) background run.
type RunResult = Result<ValidationReport, String>;

/// The save trigger's state: the gate, the run in flight and what the last run was.
#[derive(Resource, Debug)]
pub struct SaveValidation {
    /// Batches saved paths and holds.
    pub gate: SaveValidationGate,
    /// Runs finished and handed to the Problems panel.
    pub runs: u32,
    /// The paths of the last batch that ran (sorted).
    pub last_batch: Vec<String>,
    /// Why the last run produced no report (`None` after a good run).
    pub last_error: Option<String>,
    /// The worker's answer (a `Mutex` because `Receiver` is not `Sync`, and a resource must be).
    inflight: Option<Mutex<Receiver<RunResult>>>,
    inflight_batch: Vec<String>,
    hold: Option<SaveHold>,
}

impl SaveValidation {
    /// A trigger; `enabled` is the project's `validate_on_save` setting.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            gate: SaveValidationGate::new(enabled),
            runs: 0,
            last_batch: Vec::new(),
            last_error: None,
            inflight: None,
            inflight_batch: Vec::new(),
            hold: None,
        }
    }

    /// Whether a run is in flight.
    #[must_use]
    pub fn running(&self) -> bool {
        self.inflight.is_some()
    }

    /// Start (`true`) or release (`false`) the hold of the agent's batch tools.
    pub fn set_hold(&mut self, hold: bool) {
        if hold {
            if self.hold.is_none() {
                self.hold = Some(self.gate.hold());
            }
        } else {
            self.hold = None;
        }
    }

    /// Whether the agent's hold is on.
    #[must_use]
    pub fn held(&self) -> bool {
        self.hold.is_some()
    }
}

/// What a run needs, copied out of the world so a worker thread can own it.
struct RunInput {
    set: AssetSet,
    file: ValidationFile,
    project: Option<PathBuf>,
    batch: Vec<String>,
}

fn run(input: RunInput) -> ValidationReport {
    let _span = tracing::info_span!("validate_run", trigger = "save", paths = input.batch.len()).entered();
    let RunInput {
        mut set,
        file,
        project,
        batch,
    } = input;
    let mut registry = ValidatorRegistry::with_rules(&file);
    if let Some(validator) = project.as_deref().and_then(dialogue_assets_validator) {
        validator.augment_set(&mut set);
        if let Err(error) = registry.register(Box::new(validator)) {
            tracing::warn!(%error, "save validation: dialogue_assets not registered");
        }
    }
    let settings = RunSettings {
        scope: Scope::Changed(batch),
        trigger: Trigger::Save,
        exclude: file.exclude.clone(),
        ..RunSettings::default()
    };
    registry.run_with(&set, &settings)
}

/// The path as the asset set spells it: relative to the project root, `/`-separated.
fn set_path(world: &World, path: &Path) -> String {
    spell_for_set(
        crate::mesh_source::project_root(world).as_deref(),
        path,
        crate::path_norm::FOLD_CASE,
    )
}

/// [`set_path`] without the world. The root is stripped with the host's case rule (ADR-4824):
/// on Windows a save through a differently cased spelling of the project root (`c:\dev\game`
/// for `C:\Dev\Game`) is still the project's file, and an exact strip would queue the absolute
/// path, which `Scope::Changed` never matches, so the save would validate nothing.
fn spell_for_set(root: Option<&Path>, path: &Path, fold: bool) -> String {
    let relative = root
        .and_then(|root| crate::path_norm::strip_prefix_fold(root, path, fold))
        .unwrap_or_else(|| path.to_path_buf());
    relative.to_string_lossy().replace('\\', "/")
}

/// A save of `path` landed: queue it for the next validation run. Cheap by design; the first save
/// reads `validate_on_save` from the project's `config/validation.ron`.
pub fn note_saved(world: &mut World, path: &Path) {
    let spelled = set_path(world, path);
    crate::content_lint_jobs::note_path(world, &spelled);
    if world.get_resource::<SaveValidation>().is_none() {
        let enabled = project_rules(world).map_or(true, |file| file.validate_on_save);
        world.insert_resource(SaveValidation::new(enabled));
    }
    if let Some(mut state) = world.get_resource_mut::<SaveValidation>() {
        state.gate.note_saved(spelled);
    }
}

/// One editor tick: hand a finished run's report to the Problems panel, and start a run for the
/// batch the gate releases. Returns whether the panel received a report this tick.
pub fn tick_save_validation(world: &mut World) -> bool {
    tick(world, false)
}

/// Ticks and waits for the run it starts (`editor.save_validation { wait: true }` and tests: the
/// worker's receive is the wait, no sleeps). A hold keeps the batch queued, so a held gate returns
/// at once with nothing run.
pub fn finish_save_validation(world: &mut World) -> bool {
    tick(world, true)
}

/// The `Last` system of the editor: [`tick_save_validation`] once per frame.
pub fn tick_save_validation_system(world: &mut World) {
    tick_save_validation(world);
}

fn tick(world: &mut World, block: bool) -> bool {
    // Nothing queued and nothing in flight: leave the resource alone (no change tick per frame).
    if world
        .get_resource::<SaveValidation>()
        .is_none_or(|s| s.inflight.is_none() && s.gate.pending() == 0)
    {
        return false;
    }
    let Some(mut state) = world.remove_resource::<SaveValidation>() else {
        return false;
    };
    let mut delivered = poll(world, &mut state, block);
    if state.inflight.is_none()
        && let Some(batch) = state.gate.tick()
    {
        let input = RunInput {
            set: registry_set(world).unwrap_or_default(),
            file: project_rules(world).unwrap_or_default(),
            project: crate::mesh_source::project_root(world),
            batch: batch.clone(),
        };
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("save-validation".into())
            .spawn(move || {
                let _ = tx.send(Ok(run(input)));
            });
        match spawned {
            Ok(_) => {
                state.inflight = Some(Mutex::new(rx));
                state.inflight_batch = batch;
                delivered |= poll(world, &mut state, block);
            }
            Err(error) => {
                state.last_error = Some(format!("cannot start the validation thread: {error}"));
                state.last_batch = batch;
            }
        }
    }
    world.insert_resource(state);
    delivered
}

/// Takes a finished run, if any, into the Problems panel; `block` waits for an unfinished one.
fn poll(world: &mut World, state: &mut SaveValidation, block: bool) -> bool {
    let Some(rx) = &state.inflight else {
        return false;
    };
    let received = match rx.lock() {
        Ok(rx) if block => rx.recv().map_err(|_| TryRecvError::Disconnected),
        Ok(rx) => rx.try_recv(),
        Err(_) => Err(TryRecvError::Disconnected),
    };
    let result = match received {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("the validation thread ended without a report".to_owned()),
    };
    state.inflight = None;
    state.last_batch = std::mem::take(&mut state.inflight_batch);
    match result {
        Ok(report) => {
            state.runs += 1;
            state.last_error = None;
            let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
            panels.problems.set_report(report);
            panels.problems.last_scope = Some(Scope::Changed(state.last_batch.clone()));
            true
        }
        Err(error) => {
            state.last_error = Some(error);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_ext::EditorProject;

    /// A project root with a dialogue whose speaker has no voice file.
    fn project() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("save-validation");
        let dialogue = root.join("assets/dialogue");
        std::fs::create_dir_all(&dialogue).expect("mkdir");
        std::fs::write(
            dialogue.join("intro.dialogue.ron"),
            r#"(format_version: 1, name: "intro", lines: [(id: "hello", speaker: "ghost")])"#,
        )
        .expect("dialogue");
        root
    }

    fn world(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.to_path_buf()),
        });
        world
    }

    fn state(world: &World) -> &SaveValidation {
        world.get_resource::<SaveValidation>().expect("state")
    }

    #[test]
    fn a_save_only_queues_and_the_report_arrives_on_a_later_tick() {
        let root = project();
        let mut world = world(&root);
        note_saved(&mut world, &root.join("assets/dialogue/intro.dialogue.ron"));
        assert_eq!(state(&world).gate.pending(), 1);
        assert_eq!(state(&world).runs, 0, "the save did not run validation");
        assert!(world.get_resource::<AssetPanels>().is_none());
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 1);
        assert_eq!(state(&world).last_batch, ["assets/dialogue/intro.dialogue.ron"]);
        let panels = world.resource::<AssetPanels>();
        let codes: Vec<&str> = panels.problems.rows.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(codes, ["dialogue_unknown_voice"], "{:?}", panels.problems.rows);
        let report = panels.problems.report.as_ref().expect("report");
        assert_eq!(report.summary.failed, 1);
        assert_eq!(
            panels.problems.last_scope,
            Some(Scope::Changed(vec!["assets/dialogue/intro.dialogue.ron".into()]))
        );
    }

    #[test]
    fn a_case_variant_root_still_queues_the_project_relative_path() {
        // fold = true is the Windows rule, proven on every host. Before the fix the exact
        // `strip_prefix` queued the absolute path, which no asset-set path ever matches.
        let root = Path::new("C:/Dev/Game");
        let saved = Path::new("c:/dev/game/assets/maps/arena.level.ron");
        assert_eq!(
            spell_for_set(Some(root), saved, true),
            "assets/maps/arena.level.ron"
        );
        assert_eq!(
            spell_for_set(Some(root), saved, false),
            "c:/dev/game/assets/maps/arena.level.ron",
            "case-sensitive hosts keep the exact compare"
        );
        assert_eq!(
            spell_for_set(Some(root), Path::new("C:/Dev/Game/assets/a.level.ron"), false),
            "assets/a.level.ron"
        );
        assert_eq!(
            spell_for_set(None, Path::new("assets/a.level.ron"), true),
            "assets/a.level.ron"
        );
    }

    #[test]
    fn three_saves_make_one_run() {
        let root = project();
        let mut world = world(&root);
        for name in ["c.level.ron", "a.level.ron", "b.level.ron"] {
            note_saved(&mut world, &root.join("assets").join(name));
        }
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 1);
        assert_eq!(
            state(&world).last_batch,
            ["assets/a.level.ron", "assets/b.level.ron", "assets/c.level.ron"]
        );
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 1, "an empty queue runs nothing");
    }

    #[test]
    fn a_hold_defers_the_run_to_one_run_after_the_release() {
        let root = project();
        let mut world = world(&root);
        note_saved(&mut world, &root.join("assets/a.level.ron"));
        world.resource_mut::<SaveValidation>().set_hold(true);
        note_saved(&mut world, &root.join("assets/b.level.ron"));
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 0, "held");
        assert_eq!(state(&world).gate.pending(), 2);
        world.resource_mut::<SaveValidation>().set_hold(false);
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 1, "one run for the whole batch");
        assert_eq!(state(&world).last_batch.len(), 2);
    }

    #[test]
    fn a_disabled_trigger_queues_nothing() {
        let root = project();
        let mut world = world(&root);
        std::fs::create_dir_all(root.join("config")).expect("mkdir");
        std::fs::write(root.join("config/validation.ron"), "(validate_on_save: false)").expect("config");
        note_saved(&mut world, &root.join("assets/a.level.ron"));
        assert!(!state(&world).gate.enabled());
        assert_eq!(state(&world).gate.pending(), 0);
        finish_save_validation(&mut world);
        assert_eq!(state(&world).runs, 0);
    }

    #[test]
    fn saving_a_level_through_the_editor_path_queues_the_save() {
        let root = project();
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        world.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        let open = crate::level_io::new_level(world);
        let path = root.join("assets/new.level.ron");
        crate::level_io::save_level_and_terrain(world, &open, &path).expect("saves");
        assert_eq!(state(world).gate.pending(), 1);
        assert_eq!(state(world).runs, 0);
        finish_save_validation(world);
        assert_eq!(state(world).last_batch, ["assets/new.level.ron"]);
    }
}
