// SPDX-License-Identifier: MPL-2.0
//! The Tests panel (`docs/specs/41-functional-tests-and-session-harness.md` § 6): the open level's
//! functional tests with their tags, owner and last verdict, a run of a selection in
//! play-in-editor, the report with its step names, and the placed entity a click opens. Placed
//! tests draw a box and a line to their viewpoint.
//!
//! The run itself is the play world's [`Runner`] (the agent's `functest.*` are the primary
//! surface). The panel queues a run in [`TestsPanel`]; [`pump`] starts it in the play session and
//! brings the report back, so a human and an agent read one model (`editor.tests_panel_state`,
//! `editor.tests_panel_run`, `book/agents/functional-tests.md`).

use std::collections::{BTreeMap, BTreeSet};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::functional::{
    FunctionalTest, RunOptions, Runner, RunnerStatus, Selection, TestReport, Verdict, discover,
};
use regia_scene::prelude::{EntityGuid, Name};
use serde_json::{Value, json};

use crate::level_io::OpenLevel;
use crate::pie::{PieRequest, PieRequests, PieSession, PieStatus};
use crate::viewport_agent::Handler;

/// One test of the open level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRow {
    /// Name.
    pub name: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Owner.
    pub owner: String,
    /// What it checks.
    pub description: String,
    /// Disabled tests are listed and skipped.
    pub enabled: bool,
    /// The placed entity.
    pub entity: Entity,
}

/// The panel's model.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct TestsPanel {
    /// The tests, in run order.
    pub rows: Vec<TestRow>,
    /// Names ticked for a run (none: all).
    pub chosen: BTreeSet<String>,
    /// A run waiting for the play session.
    pending: Option<Selection>,
    /// A run is going on in the play world.
    pub running: bool,
    /// The last report.
    pub report: Option<TestReport>,
    /// What the panel last had to say.
    pub status: String,
}

impl TestsPanel {
    /// The last verdict of a test.
    #[must_use]
    pub fn verdict_of(&self, name: &str) -> Option<Verdict> {
        self.report
            .as_ref()?
            .tests
            .iter()
            .find(|test| test.name == name)
            .map(|test| test.verdict)
    }
}

fn level_name(world: &World) -> String {
    world
        .get_resource::<OpenLevel>()
        .and_then(|open| open.path.as_ref())
        .map_or_else(String::new, |path| path.display().to_string())
}

/// Reads the open level's tests into the panel.
pub fn refresh(world: &mut World) {
    let level = level_name(world);
    let rows: Vec<TestRow> = discover(world, &level)
        .into_iter()
        .map(|found| TestRow {
            name: found.name,
            tags: found.test.tags,
            owner: found.test.owner,
            description: found.test.description,
            enabled: found.test.enabled,
            entity: found.entity,
        })
        .collect();
    let mut panel = world.get_resource_or_insert_with(TestsPanel::default);
    panel
        .chosen
        .retain(|name| rows.iter().any(|row| row.name == *name));
    panel.rows = rows;
}

/// Queues a run of the chosen tests (all when none is), starting play when it is stopped.
/// Answers the tests the run takes; zero is an error, a run of nothing is not a pass.
pub fn request_run(world: &mut World) -> Result<usize, RpcError> {
    refresh(world);
    let playing = world
        .get_resource::<PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    let mut panel = world.get_resource_or_insert_with(TestsPanel::default);
    if panel.running {
        return Err(reason("already_running", "a run is in progress"));
    }
    let names: Vec<String> = panel.chosen.iter().cloned().collect();
    let count = panel
        .rows
        .iter()
        .filter(|row| row.enabled && (names.is_empty() || names.contains(&row.name)))
        .count();
    if count == 0 {
        return Err(reason("no_tests", "no enabled functional test is selected"));
    }
    panel.pending = Some(Selection {
        name_globs: names,
        tags: Vec::new(),
    });
    panel.status = format!("{count} test(s) queued");
    if !playing {
        world
            .get_resource_or_insert_with::<PieRequests>(Default::default)
            .push(PieRequest::Play);
    }
    Ok(count)
}

fn reason(code: &str, message: &str) -> RpcError {
    RpcError::new(
        regia_agent::protocol::INVALID_PARAMS,
        code,
        message,
        "editor.tests_panel_state lists the tests; book/agents/functional-tests.md lists the reasons",
    )
}

/// Each editor frame: starts a queued run in the play session and collects a finished report.
pub fn pump(world: &mut World, session: Option<&mut PieSession>) {
    let Some(session) = session else {
        // Play stopped under a running test: the run is gone with its world.
        if let Some(mut panel) = world.get_resource_mut::<TestsPanel>()
            && panel.running
        {
            panel.running = false;
            panel.status = "play stopped before the run ended".to_owned();
        }
        return;
    };
    let level = level_name(world);
    let project_root = crate::mesh_source::project_root(world);
    let Some(mut panel) = world.get_resource_mut::<TestsPanel>() else {
        return;
    };
    let play = session.app.world_mut();
    pump_world(&mut panel, play, level, project_root);
}

fn pump_world(
    panel: &mut TestsPanel,
    play: &mut World,
    level: String,
    project_root: Option<std::path::PathBuf>,
) {
    let Some(mut runner) = play.get_resource_mut::<Runner>() else {
        if panel.pending.take().is_some() {
            panel.status = "no_runner: this play session has no functional-test runner".to_owned();
        }
        return;
    };
    if let Some(selection) = panel.pending.take() {
        runner.start(
            selection,
            RunOptions {
                level,
                project_root,
                ..RunOptions::default()
            },
        );
        panel.running = true;
        panel.status = "running".to_owned();
    }
    if panel.running && runner.status() == RunnerStatus::Done {
        let report = runner.report().clone();
        panel.status = summary(&report);
        panel.report = Some(report);
        panel.running = false;
    }
}

/// The counts of a report.
#[must_use]
pub fn summary(report: &TestReport) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for test in &report.tests {
        *counts
            .entry(format!("{:?}", test.verdict).to_lowercase())
            .or_default() += 1;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(verdict, n)| format!("{n} {verdict}"))
        .collect();
    parts.join(", ")
}

/// Selects a test's placed entity.
pub fn open_test(world: &mut World, entity: Entity) {
    world
        .get_resource_or_insert_with(crate::selection::Selection::default)
        .select(entity, crate::selection::SelectMode::Replace);
}

/// The gizmo of placed tests: a box at each test, a line to its viewpoint (spec 41 § 6), with the
/// names and anchors the viewport labels.
#[must_use]
pub fn test_gizmos(world: &World) -> Vec<TestGizmo> {
    let color = egui::Color32::from_rgb(120, 220, 120);
    let mut out = Vec::new();
    for entity_ref in world.iter_entities() {
        let Some(test) = entity_ref.get::<FunctionalTest>() else {
            continue;
        };
        let entity = entity_ref.id();
        let Some(at) = crate::viewport::world_position(world, entity) else {
            continue;
        };
        let mut segments = box_segments(at, 0.5, color);
        if let Some(guid) = test.viewpoint.as_deref()
            && let Some(view) = world
                .iter_entities()
                .find(|e| e.get::<EntityGuid>().is_some_and(|g| g.to_string() == guid))
                .and_then(|e| crate::viewport::world_position(world, e.id()))
        {
            segments.push((at, view, color));
        }
        let name = entity_ref
            .get::<Name>()
            .map_or_else(|| "test".to_owned(), |n| n.as_str().to_owned());
        out.push(TestGizmo {
            name,
            anchor: at + Vec3::Y * 0.5,
            segments,
        });
    }
    out
}

/// What the viewport draws for one placed test.
#[derive(Clone, Debug, PartialEq)]
pub struct TestGizmo {
    /// The label.
    pub name: String,
    /// Where the label goes.
    pub anchor: Vec3,
    /// The lines.
    pub segments: Vec<(Vec3, Vec3, egui::Color32)>,
}

/// The twelve edges of a cube of half-size `half` around `at`.
fn box_segments(at: Vec3, half: f32, color: egui::Color32) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let corner = |x: f32, y: f32, z: f32| at + Vec3::new(x, y, z) * half;
    let mut out = Vec::with_capacity(13);
    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        out.push((corner(x, y, -1.0), corner(x, y, 1.0), color));
    }
    for z in [-1.0, 1.0] {
        for (a, b) in [
            ((-1.0, -1.0), (1.0, -1.0)),
            ((1.0, -1.0), (1.0, 1.0)),
            ((1.0, 1.0), (-1.0, 1.0)),
            ((-1.0, 1.0), (-1.0, -1.0)),
        ] {
            out.push((corner(a.0, a.1, z), corner(b.0, b.1, z), color));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// egui
// ---------------------------------------------------------------------------

/// What the Tests panel shows when it has no tests listed (bug 25, ADR-8586).
pub const TESTS_EMPTY: &str = "No tests listed. Press Refresh to look for the project's tests.";

/// The Tests panel.
pub fn tests_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.get_resource::<TestsPanel>().cloned().unwrap_or_default();
    let mut refresh_now = false;
    let mut run_now = false;
    let mut open = None;
    ui.horizontal(|ui| {
        refresh_now = ui.button("Refresh").clicked();
        let label = if panel.chosen.is_empty() {
            "Run all"
        } else {
            "Run selected"
        };
        run_now = ui.add_enabled(!panel.running, egui::Button::new(label)).clicked();
        ui.label(&panel.status);
    });
    // No tests: one sentence instead of a bare table header (bug 25, ADR-8586).
    if panel.rows.is_empty() {
        ui.weak(TESTS_EMPTY);
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        if panel.rows.is_empty() {
            return;
        }
        egui::Grid::new("tests-grid").num_columns(5).show(ui, |ui| {
            for heading in ["", "test", "tags", "owner", "last"] {
                ui.strong(heading);
            }
            ui.end_row();
            for row in &panel.rows {
                let mut chosen = panel.chosen.contains(&row.name);
                if ui
                    .add_enabled(row.enabled, egui::Checkbox::new(&mut chosen, ""))
                    .changed()
                {
                    if chosen {
                        panel.chosen.insert(row.name.clone());
                    } else {
                        panel.chosen.remove(&row.name);
                    }
                }
                let response = ui.selectable_label(false, &row.name);
                let response = if row.description.is_empty() {
                    response
                } else {
                    response.on_hover_text(&row.description)
                };
                if response.clicked() {
                    open = Some(row.entity);
                }
                ui.label(row.tags.join(", "));
                ui.label(&row.owner);
                let verdict = if row.enabled {
                    panel
                        .verdict_of(&row.name)
                        .map_or_else(String::new, |verdict| format!("{verdict:?}"))
                } else {
                    "disabled".to_owned()
                };
                ui.label(verdict);
                ui.end_row();
            }
        });
        if let Some(report) = &panel.report {
            ui.separator();
            for test in &report.tests {
                ui.collapsing(format!("{} {:?}", test.name, test.verdict), |ui| {
                    if !test.message.is_empty() {
                        ui.label(&test.message);
                    }
                    for step in &test.steps {
                        ui.label(format!(
                            "{} {:?} ({} frames)",
                            step.name, step.verdict, step.frames
                        ));
                    }
                    for pass in &test.passes {
                        if !pass.message.is_empty() {
                            ui.label(format!("{:?}: {}", pass.verdict, pass.message));
                        }
                    }
                });
            }
        }
    });
    world.insert_resource(panel);
    if refresh_now {
        refresh(world);
    }
    if run_now && let Err(err) = request_run(world) {
        world.resource_mut::<TestsPanel>().status = err.message;
    }
    if let Some(entity) = open {
        open_test(world, entity);
    }
}

// ---------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------

fn state_json(world: &World) -> Value {
    let panel = world.get_resource::<TestsPanel>().cloned().unwrap_or_default();
    let rows: Vec<Value> = panel
        .rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "tags": row.tags,
                "owner": row.owner,
                "description": row.description,
                "enabled": row.enabled,
                "entity": row.entity.to_bits(),
                "verdict": panel.verdict_of(&row.name).map(|v| format!("{v:?}")),
                "chosen": panel.chosen.contains(&row.name),
            })
        })
        .collect();
    json!({
        "tests": rows,
        "running": panel.running,
        "queued": panel.pending.is_some(),
        "status": panel.status,
        "report": panel.report,
    })
}

fn panel_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    refresh(world);
    Ok(state_json(world))
}

fn panel_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    refresh(world);
    if let Some(names) = params.get("tests").filter(|v| !v.is_null()) {
        let names: Option<Vec<String>> = names
            .as_array()
            .and_then(|items| items.iter().map(|v| v.as_str().map(str::to_owned)).collect());
        let names = names.ok_or_else(|| reason("bad_params", "`tests` must be an array of names"))?;
        let mut panel = world.resource_mut::<TestsPanel>();
        if let Some(unknown) = names
            .iter()
            .find(|name| !panel.rows.iter().any(|row| row.name == **name))
        {
            return Err(reason("test_not_found", &format!("no test is named `{unknown}`")));
        }
        panel.chosen = names.into_iter().collect();
    }
    let started = request_run(world)?;
    let mut answer = state_json(world);
    if let Some(object) = answer.as_object_mut() {
        object.insert("started".into(), json!(started));
    }
    Ok(answer)
}

fn panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    refresh(world);
    let name = params
        .get("test")
        .and_then(Value::as_str)
        .ok_or_else(|| reason("bad_params", "`test` is required"))?;
    let row = world
        .resource::<TestsPanel>()
        .rows
        .iter()
        .find(|row| row.name == name)
        .cloned()
        .ok_or_else(|| reason("test_not_found", &format!("no test is named `{name}`")))?;
    open_test(world, row.entity);
    Ok(json!({ "entity": row.entity.to_bits() }))
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.tests_panel_state",
                tool: "tests_panel_state",
                description: "The Tests panel: the open level's functional tests with tags, owner, last verdict and what is ticked, whether a run is queued or going on, and the last report.",
                params: &[],
                result: "{ tests: [{ name, tags, owner, description, enabled, entity, verdict, chosen }], running, queued, status, report }",
                mutating: false,
                permission: Permission::Read,
            },
            panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tests_panel_run",
                tool: "tests_panel_run",
                description: "Run the panel's selection in play-in-editor, starting play when stopped; poll editor.tests_panel_state for the report. no_tests when nothing enabled is selected, already_running during a run.",
                params: &[ParamSchema {
                    name: "tests",
                    kind: "array",
                    required: false,
                    description: "Tick these tests first (default: what is ticked, else all).",
                }],
                result: "{ tests, running, queued, status, report, started }",
                mutating: true,
                permission: Permission::Play,
            },
            panel_run as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tests_panel_open",
                tool: "tests_panel_open",
                description: "Select a test's placed entity, as a click on its row does.",
                params: &[ParamSchema {
                    name: "test",
                    kind: "string",
                    required: true,
                    description: "The test's name.",
                }],
                result: "{ entity }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_open as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::MinimalPlugins;
    use regia_app::prelude::App;
    use regia_framework::functional::{
        FunctionalAppExt, FunctionalTestPlugin, TestBody, TestBodyRef, TestContext,
    };
    use regia_scene::Transform;

    #[derive(Default)]
    struct Passes;
    impl TestBody for Passes {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.begin_step("the only step");
            cx.end_step();
            cx.conclude(Verdict::Passed, "fine");
        }
    }

    #[derive(Default)]
    struct Fails;
    impl TestBody for Fails {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.conclude(Verdict::Failed, "on purpose");
        }
    }

    fn place<T>(app: &mut App, name: &str, tweak: impl FnOnce(&mut FunctionalTest)) -> Entity {
        let mut test = FunctionalTest {
            body: TestBodyRef::Rust(std::any::type_name::<T>().to_owned()),
            ..FunctionalTest::default()
        };
        tweak(&mut test);
        app.world_mut()
            .spawn((Name::new(name), test, Transform::default()))
            .id()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, FunctionalTestPlugin));
        app.register_functional_test::<Passes>()
            .register_functional_test::<Fails>();
        place::<Passes>(&mut app, "good", |t| t.tags = vec!["smoke".into()]);
        place::<Fails>(&mut app, "bad", |t| t.owner = "gameplay".into());
        place::<Passes>(&mut app, "off", |t| t.enabled = false);
        app
    }

    fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("registered");
        handler(world, params)
    }

    /// Runs what the panel queued in `play`, as the editor loop does.
    fn run_in(edit: &mut App, play: &mut App) {
        for _ in 0..50 {
            let mut panel = edit.world_mut().resource_mut::<TestsPanel>();
            pump_world(&mut panel, play.world_mut(), String::new(), None);
            if !panel.running && panel.pending.is_none() {
                return;
            }
            play.update();
        }
        panic!("the run did not finish");
    }

    #[test]
    fn the_rows_list_the_tests_with_their_data() {
        let mut app = app();
        let out = call(app.world_mut(), "editor.tests_panel_state", &json!({})).expect("state");
        let tests = out["tests"].as_array().expect("tests");
        assert_eq!(tests.len(), 3);
        let bad = tests.iter().find(|t| t["name"] == "bad").expect("bad");
        assert_eq!(bad["owner"], "gameplay");
        assert_eq!(tests.iter().filter(|t| t["enabled"] == false).count(), 1);
    }

    #[test]
    fn a_run_starts_play_and_reports_verdicts_and_steps() {
        let mut edit = app();
        let mut play = app();
        edit.world_mut().init_resource::<PieRequests>();
        let out = call(edit.world_mut(), "editor.tests_panel_run", &json!({})).expect("run");
        assert_eq!(out["started"], 2);
        assert_eq!(out["queued"], true);
        assert_eq!(
            edit.world().resource::<PieRequests>().queue,
            [PieRequest::Play],
            "stopped: play starts"
        );
        run_in(&mut edit, &mut play);
        let panel = edit.world().resource::<TestsPanel>();
        assert_eq!(panel.verdict_of("good"), Some(Verdict::Passed));
        assert_eq!(panel.verdict_of("bad"), Some(Verdict::Failed));
        let report = panel.report.as_ref().expect("report");
        let good = report.tests.iter().find(|t| t.name == "good").expect("good");
        assert_eq!(good.steps[0].name, "the only step");
        assert!(panel.status.contains("1 failed"), "{}", panel.status);
    }

    #[test]
    fn a_selection_runs_only_what_is_ticked() {
        let mut edit = app();
        let mut play = app();
        let out = call(
            edit.world_mut(),
            "editor.tests_panel_run",
            &json!({ "tests": ["good"] }),
        )
        .expect("run");
        assert_eq!(out["started"], 1);
        run_in(&mut edit, &mut play);
        let panel = edit.world().resource::<TestsPanel>();
        assert_eq!(panel.verdict_of("good"), Some(Verdict::Passed));
        assert_eq!(panel.verdict_of("bad"), None);
    }

    #[test]
    fn the_reasons_of_a_run() {
        let mut edit = app();
        let code = |err: RpcError| err.data.expect("data")["code"].as_str().expect("code").to_owned();
        let err = call(
            edit.world_mut(),
            "editor.tests_panel_run",
            &json!({ "tests": ["ghost"] }),
        )
        .expect_err("unknown");
        assert_eq!(code(err), "test_not_found");
        let err = call(
            edit.world_mut(),
            "editor.tests_panel_run",
            &json!({ "tests": ["off"] }),
        )
        .expect_err("disabled");
        assert_eq!(code(err), "no_tests");
        let err = call(
            edit.world_mut(),
            "editor.tests_panel_open",
            &json!({ "test": "ghost" }),
        )
        .expect_err("unknown");
        assert_eq!(code(err), "test_not_found");

        call(
            edit.world_mut(),
            "editor.tests_panel_run",
            &json!({ "tests": ["good"] }),
        )
        .expect("run");
        edit.world_mut().resource_mut::<TestsPanel>().running = true;
        let err = call(edit.world_mut(), "editor.tests_panel_run", &json!({})).expect_err("busy");
        assert_eq!(code(err), "already_running");
    }

    #[test]
    fn a_play_world_without_a_runner_says_so() {
        let mut edit = app();
        let mut play = App::new();
        call(edit.world_mut(), "editor.tests_panel_run", &json!({})).expect("run");
        let mut panel = edit.world_mut().resource_mut::<TestsPanel>();
        pump_world(&mut panel, play.world_mut(), String::new(), None);
        assert!(panel.status.starts_with("no_runner"), "{}", panel.status);
    }

    #[test]
    fn a_stopped_play_session_ends_the_run_in_the_panel() {
        let mut edit = app();
        edit.world_mut().insert_resource(TestsPanel {
            running: true,
            ..TestsPanel::default()
        });
        pump(edit.world_mut(), None);
        assert!(!edit.world().resource::<TestsPanel>().running);
    }

    #[test]
    fn a_click_selects_the_placed_entity() {
        let mut edit = app();
        let out = call(
            edit.world_mut(),
            "editor.tests_panel_open",
            &json!({ "test": "bad" }),
        )
        .expect("open");
        let selected = edit.world().resource::<crate::selection::Selection>();
        assert_eq!(selected.primary().map(Entity::to_bits), out["entity"].as_u64());
    }

    #[test]
    fn the_gizmo_is_a_box_and_a_line_to_the_viewpoint() {
        let mut edit = app();
        let guid = regia_core::id::Guid::from_u128(0x77);
        edit.world_mut().spawn((
            EntityGuid(guid),
            Transform {
                translation: Vec3::new(4.0, 0.0, 0.0),
                ..Transform::default()
            },
        ));
        place::<Passes>(&mut edit, "view", |t| t.viewpoint = Some(guid.to_string()));
        let gizmos = test_gizmos(edit.world());
        assert_eq!(gizmos.len(), 4);
        let with_view = gizmos.iter().find(|g| g.name == "view").expect("gizmo");
        assert_eq!(with_view.segments.len(), 13);
        let plain = gizmos.iter().find(|g| g.name == "good").expect("gizmo");
        assert_eq!(plain.segments.len(), 12);
    }

    #[test]
    fn the_panel_draws() {
        let mut edit = app();
        refresh(edit.world_mut());
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            tests_panel(ui, edit.world_mut());
        });
        output.textures_delta.clear();
        assert_eq!(edit.world().resource::<TestsPanel>().rows.len(), 3);
    }
}
