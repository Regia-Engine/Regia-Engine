// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for in-level functional tests (`docs/specs/41-functional-tests-and-session-harness.md` § 5,
//! `book/agents/functional-tests.md`): `functest.list`, `.run`, `.status`, `.results`, `.abort`, `.create`, `.bless`.
//!
//! Errors are `F0002` with `data.reason` (lowercase ascii, digits and `_`): `no_runner`, `no_tests`,
//! `already_running`, `not_running`, `test_not_found`, `body_not_found`, `script_invalid`, `setting_unknown`,
//! `ready_timeout`, `too_many_passes`, `needs_confirmation`, `not_playing`, `bad_params`. `body_not_found`,
//! `setting_unknown`, `ready_timeout` and `too_many_passes` are run-level reasons reported in a test's `message` by
//! `functest.results` (`script_invalid` is both); the rest are call errors. `not_playing` is reserved for editor hosts
//! whose world is not in play mode; the in-game agent never raises it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use regia_app::prelude::*;
use regia_framework::functional::{
    FunctionalTest, RunOptions, Runner, RunnerStatus, Selection, TestBodyRef, TestHooks, discover,
    script::FuncTestScript,
};
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Name};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// The `F0002` call error for functional tests, with `data.reason`.
fn reason(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(
        INVALID_PARAMS,
        "F0002",
        message,
        "see book/agents/functional-tests.md for the reasons and the recipe",
    );
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// Writes `target` via `target` with extension `ron.tmp`, replacing a symlink at that
/// temp name instead of following it, then renaming over `target`.
fn write_script_file(target: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = target.with_extension("ron.tmp");
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()
    })();
    let renamed = written.and_then(|()| std::fs::rename(&tmp, target));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// The project directory: the agent's configured one, else the environment/cwd lookup.
fn project_dir(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::AgentSettings>()
        .and_then(|settings| settings.project_dir.clone())
        .or_else(|| crate::endpoint::resolve_project_dir(None))
}

fn bad_params(message: impl Into<String>) -> RpcError {
    reason("bad_params", message)
}

/// The services the runner needs from the agent: condition evaluation, settings, input injection.
pub fn hooks() -> TestHooks {
    fn condition(world: &mut World, condition: &Value) -> Result<(bool, String), String> {
        let finding = crate::playtest::evaluate(world, condition);
        match finding.error {
            Some(error) => Err(error),
            None => Ok((finding.holds, finding.detail)),
        }
    }
    fn get_setting(world: &mut World, key: &str) -> Option<Value> {
        let result = crate::framework::settings_get(world, &json!({ "path": key })).ok()?;
        result.get("value").cloned()
    }
    fn set_setting(world: &mut World, key: &str, value: &Value) -> Result<(), String> {
        crate::framework::settings_set(world, &json!({ "path": key, "value": value }))
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn inject(world: &mut World, action: &str, value: [f32; 2], frames: u32) -> Result<(), String> {
        let params =
            json!({ "actions": [{ "action": action, "value": value, "duration_frames": frames.max(1) }] });
        crate::playtest::inject_input(world, &params)
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn reload_level(world: &mut World) -> Result<(), String> {
        let mut start = world
            .get_resource_mut::<regia_framework::start_level::StartLevel>()
            .ok_or("no start level to reload")?;
        start.dirty = true;
        regia_framework::start_level::open_start_level(world);
        Ok(())
    }
    TestHooks {
        condition: Some(condition),
        get_setting: Some(get_setting),
        set_setting: Some(set_setting),
        inject: Some(inject),
        viewpoint: None,
        reload_level: Some(reload_level),
    }
}

fn runner(world: &mut World) -> Result<Mut<'_, Runner>, RpcError> {
    if world.get_resource::<Runner>().is_none() {
        return Err(reason(
            "no_runner",
            "this app has no functional-test runner (add regia_framework::functional::FunctionalTestPlugin)",
        ));
    }
    let mut runner = world.resource_mut::<Runner>();
    runner.set_hooks(hooks());
    Ok(runner)
}

fn strings(params: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| bad_params(format!("`{key}` must be a string or an array of strings")))
            })
            .collect(),
        Some(_) => Err(bad_params(format!(
            "`{key}` must be a string or an array of strings"
        ))),
    }
}

fn selection(params: &Value) -> Result<Selection, RpcError> {
    Ok(Selection {
        name_globs: strings(params, "filter")?,
        tags: strings(params, "tags")?,
    })
}

fn body_text(body: &TestBodyRef) -> String {
    match body {
        TestBodyRef::Script(path) => format!("script:{path}"),
        TestBodyRef::Rust(path) => format!("rust:{path}"),
    }
}

/// `functest.list`: the placed tests that match the optional `filter` globs and `tags`.
pub fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    runner(world)?;
    let selection = selection(params)?;
    let level = crate::params::opt_str(params, "level")?.unwrap_or("");
    let tests: Vec<Value> = discover(world, level)
        .into_iter()
        .filter(|t| selection.matches(&t.name, &t.test.tags))
        .map(|t| {
            json!({
                "name": t.name,
                "level": t.level,
                "entity": t.entity.to_bits(),
                "tags": t.test.tags,
                "enabled": t.test.enabled,
                "description": t.test.description,
                "body": body_text(&t.test.body),
            })
        })
        .collect();
    Ok(json!({ "tests": tests }))
}

/// `functest.run`: starts the selected tests; they run one at a time, one transition per frame.
pub fn run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let selection = selection(params)?;
    let mut opts = RunOptions {
        seed: crate::params::opt_u64(params, "seed")?.unwrap_or(0),
        update_expected: crate::params::opt_bool(params, "update_expected")?.unwrap_or(false),
        looped: crate::params::opt_bool(params, "looped")?.unwrap_or(false),
        level: crate::params::opt_str(params, "level")?.unwrap_or("").to_owned(),
        project_root: project_dir(world),
        ..RunOptions::default()
    };
    // The level names the `.expected.ron` file written beside it (`update_expected`, `bless`), so
    // it must be project-relative and stay in the project (ADR-1816).
    if !opts.level.is_empty() {
        match &opts.project_root {
            Some(root) => {
                crate::req_path::project_path_in(root, "level", &opts.level)?;
            }
            None => {
                regia_core::project_path::check_relative(&opts.level).map_err(|e| {
                    bad_params(format!("`level` must be project-relative ({}): {e}", e.code()))
                })?;
            }
        }
    }
    if let Some(map) = params.get("params") {
        let Some(object) = map.as_object() else {
            return Err(bad_params("`params` must be an object of strings"));
        };
        let mut out = BTreeMap::new();
        for (k, v) in object {
            let Some(text) = v.as_str() else {
                return Err(bad_params(format!("`params.{k}` must be a string")));
            };
            out.insert(k.clone(), text.to_owned());
        }
        opts.params = out;
    }
    if runner(world)?.is_active() {
        return Err(reason(
            "already_running",
            "a functional-test run is already in progress",
        ));
    }
    let selected = discover(world, &opts.level)
        .into_iter()
        .filter(|t| t.test.enabled && selection.matches(&t.name, &t.test.tags))
        .count();
    if selected == 0 {
        return Err(reason(
            "no_tests",
            "no enabled functional test matches the selection",
        ));
    }
    runner(world)?.start(selection, opts);
    Ok(json!({ "started": selected }))
}

fn state_name(status: &RunnerStatus) -> &'static str {
    match status {
        RunnerStatus::Idle => "idle",
        RunnerStatus::Running { .. } => "running",
        RunnerStatus::Done => "done",
    }
}

/// `functest.status`: `{ state, test?, pass?, frame?, done, total }` (`done` counts finished tests).
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let runner = runner(world)?;
    let status = runner.status();
    let (finished, total) = runner.progress();
    let mut out = json!({
        "state": state_name(&status),
        "done": finished,
        "total": total,
        "finished": status == RunnerStatus::Done,
    });
    if let RunnerStatus::Running { test, pass, frame } = status {
        out["test"] = json!(test);
        out["pass"] = json!(pass);
        out["frame"] = json!(frame);
    }
    Ok(out)
}

/// `functest.results`: the `TestReport` so far; `since` skips that many leading tests.
pub fn results(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let runner = runner(world)?;
    let since = crate::params::opt_u64(params, "since")?.unwrap_or(0) as usize;
    let mut report = runner.report().clone();
    report.tests = report.tests.into_iter().skip(since).collect();
    let mut value = serde_json::to_value(&report).map_err(|e| RpcError::internal(e.to_string()))?;
    value["exit_code"] = json!(runner.report().exit_code());
    Ok(value)
}

/// `functest.abort`: stops the run; the current test ends `Aborted` after its teardown.
pub fn abort(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = crate::params::opt_str(params, "reason")?
        .unwrap_or("aborted by agent")
        .to_owned();
    let mut runner = runner(world)?;
    if !runner.is_active() {
        return Err(reason("not_running", "no functional-test run is in progress"));
    }
    runner.abort(&text);
    Ok(json!({ "aborted": true }))
}

fn vec3(params: &Value, key: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(v) = params.get(key) else {
        return Ok(None);
    };
    let items = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| bad_params(format!("`{key}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item
            .as_f64()
            .ok_or_else(|| bad_params(format!("`{key}` must be numbers")))?;
        let n = n as f32;
        if !n.is_finite() {
            return Err(bad_params(format!("`{key}` must be finite")));
        }
        *slot = n;
    }
    Ok(Some(out))
}

/// `functest.create`: places a test entity. `body` is `{ "script": path }` or `{ "rust": type_path }`; with a
/// `script` text the `.functest.ron` is validated and written (atomically) under the project directory.
pub fn create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    runner(world)?;
    let name = crate::params::opt_str(params, "name")?
        .filter(|n| !n.is_empty())
        .ok_or_else(|| bad_params("`name` is required"))?;
    let body = match params.get("body") {
        Some(Value::String(s)) => match s.split_once(':') {
            Some(("script", p)) => TestBodyRef::Script(p.to_owned()),
            Some(("rust", p)) => TestBodyRef::Rust(p.to_owned()),
            _ => {
                return Err(bad_params(
                    "`body` string must be `script:<path>` or `rust:<type path>`",
                ));
            }
        },
        Some(Value::Object(o)) => match (
            crate::params::opt_str(o, "script")?,
            crate::params::opt_str(o, "rust")?,
        ) {
            (Some(p), None) => TestBodyRef::Script(p.to_owned()),
            (None, Some(p)) => TestBodyRef::Rust(p.to_owned()),
            _ => {
                return Err(bad_params(
                    "`body` object needs exactly one of `script` or `rust`",
                ));
            }
        },
        _ => return Err(bad_params("`body` is required")),
    };
    // A script body is read (and, with `script` text, written) under the project: absolute, drive,
    // UNC, `..` and symlinked paths are refused (ADR-1816; `C:x` used to replace the root on Windows).
    if let TestBodyRef::Script(path) = &body {
        match project_dir(world) {
            Some(root) => {
                crate::req_path::project_path_in(&root, "body.script", path)?;
            }
            None => {
                regia_core::project_path::check_relative(path).map_err(|e| {
                    bad_params(format!(
                        "script path must be project-relative ({}): {e}",
                        e.code()
                    ))
                })?;
            }
        }
    }
    let tags = strings(params, "tags")?;
    let frame_limit =
        crate::params::opt_u64(params, "frame_limit")?.map_or(1800, |n| n.min(u64::from(u32::MAX)) as u32);
    let at = vec3(params, "at")?.unwrap_or([0.0; 3]);
    if let Some(text) = crate::params::opt_str(params, "script")? {
        let TestBodyRef::Script(path) = &body else {
            return Err(bad_params("`script` text needs a script body"));
        };
        FuncTestScript::parse(text)
            .map_err(|e| reason("script_invalid", format!("script does not parse: {e}")))?;
        let root: PathBuf =
            project_dir(world).ok_or_else(|| bad_params("no project directory to write the script into"))?;
        let target = crate::req_path::project_path_in(&root, "body.script", path)?;
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| RpcError::internal(format!("cannot create {}: {e}", dir.display())))?;
        }
        // Unlink a planted `*.ron.tmp` symlink before creating the file. `std::fs::write`
        // opens with truncate and would follow that link into whatever it names.
        write_script_file(&target, text)
            .map_err(|e| RpcError::internal(format!("cannot write {}: {e}", target.display())))?;
    }
    let test = FunctionalTest {
        tags,
        frame_limit,
        body,
        ..FunctionalTest::default()
    };
    let entity = world
        .spawn((
            Name::new(name),
            EntityGuid::new_random(),
            test,
            Transform::from_translation(at.into()),
        ))
        .id();
    Ok(json!({ "entity": entity.to_bits() }))
}

/// `functest.bless`: writes the values the named test observed into its `.expected.ron`. Needs
/// `"confirm": true` (the owner-confirmation rule of spec 23); without it the call answers `needs_confirmation`.
pub fn bless(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let test = params
        .get("test")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_params("`test` is required"))?
        .to_owned();
    let key = crate::params::opt_str(params, "key")?.map(String::from);
    let mut runner = runner(world)?;
    if crate::params::opt_bool(params, "confirm")? != Some(true) {
        return Err(reason(
            "needs_confirmation",
            "blessing expected values needs the owner's confirmation: repeat with `confirm: true` after review",
        ));
    }
    if runner.report().tests.iter().all(|t| t.name != test) {
        return Err(reason(
            "test_not_found",
            format!("no test `{test}` in the last run"),
        ));
    }
    let written = runner.bless(&test, key.as_deref()).map_err(bad_params)?;
    Ok(json!({ "written": written }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::functional::{
        FunctionalAppExt, FunctionalTestPlugin, TestBody, TestContext, Verdict as V,
    };

    #[derive(Default)]
    struct Steps;

    impl TestBody for Steps {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.begin_step("check the answer");
            cx.equal(&6, &7, "answer");
            cx.end_step();
            cx.conclude(V::Passed, "done");
        }
    }

    #[derive(Default)]
    struct Fine;
    impl TestBody for Fine {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.conclude(V::Passed, "fine");
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, FunctionalTestPlugin));
        app.register_functional_test::<Steps>()
            .register_functional_test::<Fine>();
        app
    }

    fn reason_of(error: &RpcError) -> String {
        assert_eq!(error.data.as_ref().expect("data")["code"], "F0002");
        error.data.as_ref().expect("data")["reason"]
            .as_str()
            .expect("reason")
            .to_owned()
    }

    fn frames(app: &mut App, n: usize) {
        for _ in 0..n {
            app.update();
        }
    }

    #[test]
    fn create_rejects_a_non_finite_position_and_spawns_nothing() {
        let mut app = app();
        let err = create(
            app.world_mut(),
            &json!({ "name": "wide", "body": "rust:missing", "at": [1e308, 0.0, 0.0] }),
        )
        .expect_err("non-finite at");
        assert_eq!(reason_of(&err), "bad_params");
        assert!(err.message.contains("finite"), "{}", err.message);
        // MinimalPlugins already spawns entities. The refused call must add no test and no
        // non-finite translation.
        assert!(discover(app.world_mut(), "").is_empty(), "no test spawned");
        let mut transforms = app.world_mut().query::<&regia_scene::Transform>();
        assert!(
            transforms
                .iter(app.world())
                .all(|transform| transform.translation.is_finite()),
            "no non-finite translation"
        );
    }

    #[test]
    fn without_the_plugin_every_method_answers_no_runner() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        for result in [
            list(app.world_mut(), &json!({})),
            run(app.world_mut(), &json!({})),
            status(app.world_mut(), &json!({})),
            results(app.world_mut(), &json!({})),
            abort(app.world_mut(), &json!({})),
            create(app.world_mut(), &json!({ "name": "x", "body": "rust:a" })),
            bless(app.world_mut(), &json!({ "test": "x" })),
        ] {
            assert_eq!(reason_of(&result.expect_err("no runner")), "no_runner");
        }
    }

    /// The § 5 recipe: place a test, run it, poll status, read results, see `Failed` with the step name.
    #[test]
    fn audit_create_validates_all_params_before_writing_script() {
        let root = regia_test_utils::unique_temp_dir("agent-functest-atomic");
        let target = root.join("tests/check.functest.ron");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("tests dir");
        let mut app = app();
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let script = "(format_version: 1, steps: [(name: \"s\")])";
        FuncTestScript::parse(script).expect("valid script fixture");
        for existing in [false, true] {
            for (key, invalid) in [
                ("at", json!([1, 2])),
                ("frame_limit", json!("nope")),
                ("tags", json!([1])),
            ] {
                if existing {
                    let tmp = target.with_extension("fixture.tmp");
                    std::fs::write(&tmp, "previous script").expect("fixture");
                    std::fs::rename(tmp, &target).expect("atomic fixture");
                }
                let mut params = json!({
                    "name": "check", "body": "script:tests/check.functest.ron", "script": script,
                });
                params[key] = invalid;
                let err = create(app.world_mut(), &params).expect_err("invalid late parameter");
                assert_eq!(err.code, INVALID_PARAMS);
                assert!(discover(app.world_mut(), "").is_empty(), "no entity on failure");
                if existing {
                    assert_eq!(
                        std::fs::read_to_string(&target).expect("preserved"),
                        "previous script"
                    );
                } else {
                    assert!(!target.exists(), "no file on failure: {key}");
                }
                assert_eq!(
                    std::fs::read_dir(target.parent().expect("parent"))
                        .expect("dir")
                        .count(),
                    usize::from(existing)
                );
            }
        }
        let created = create(
            app.world_mut(),
            &json!({
                "name": "check", "body": "script:tests/check.functest.ron", "script": script,
                "at": [1, 2, 3], "frame_limit": 42, "tags": ["smoke"],
            }),
        )
        .expect("corrected retry");
        assert!(created["entity"].is_u64());
        assert_eq!(std::fs::read_to_string(&target).expect("written"), script);
        let tests = discover(app.world_mut(), "");
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].test.frame_limit, 42);
        assert_eq!(tests[0].test.tags, ["smoke"]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn audit_create_removes_temp_when_script_rename_fails() {
        let root = regia_test_utils::unique_temp_dir("agent-functest-rename");
        let target = root.join("tests/blocked.functest.ron");
        std::fs::create_dir_all(&target).expect("directory blocks rename");
        let mut app = app();
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let err = create(
            app.world_mut(),
            &json!({
                "name": "blocked", "body": "script:tests/blocked.functest.ron",
                "script": "(format_version: 1, steps: [(name: \"s\")])",
            }),
        )
        .expect_err("rename over directory fails");
        assert_eq!(err.code, crate::protocol::INTERNAL_ERROR);
        assert!(target.is_dir(), "existing target preserved");
        assert!(
            !target.with_extension("ron.tmp").exists(),
            "temporary write removed"
        );
        assert!(discover(app.world_mut(), "").is_empty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    /// A symlink planted at the predictable `*.ron.tmp` name must not be followed.
    /// `std::fs::write` truncates the link target; the script has to replace the link.
    #[cfg(unix)]
    #[test]
    fn create_does_not_follow_a_planted_script_temp_symlink() {
        let root = regia_test_utils::unique_temp_dir("agent-functest-tmp-link");
        let victim = root.join("secret.txt");
        std::fs::write(&victim, "keep-me").expect("victim");
        let target = root.join("tests/check.functest.ron");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("tests dir");
        std::os::unix::fs::symlink(&victim, target.with_extension("ron.tmp")).expect("plant");
        let mut app = app();
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let script = "(format_version: 1, steps: [(name: \"s\")])";
        let created = create(
            app.world_mut(),
            &json!({
                "name": "check",
                "body": "script:tests/check.functest.ron",
                "script": script,
            }),
        )
        .expect("write");
        assert!(created["entity"].is_u64());
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep-me");
        let meta = std::fs::symlink_metadata(&target).expect("script");
        assert!(meta.file_type().is_file(), "script is a regular file");
        assert_eq!(std::fs::read_to_string(&target).expect("script"), script);
        assert!(!target.with_extension("ron.tmp").exists(), "temp name cleared");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recipe_create_run_status_results_with_a_deliberate_failure() {
        let mut app = app();
        let created = create(
            app.world_mut(),
            &json!({ "name": "doomed", "body": { "rust": std::any::type_name::<Steps>() }, "tags": ["demo"] }),
        )
        .expect("creates");
        assert!(created["entity"].is_u64());
        create(
            app.world_mut(),
            &json!({ "name": "fine", "body": format!("rust:{}", std::any::type_name::<Fine>()) }),
        )
        .expect("creates");

        let listed = list(app.world_mut(), &json!({ "tags": ["demo"] })).expect("lists");
        assert_eq!(listed["tests"].as_array().expect("tests").len(), 1);
        assert_eq!(listed["tests"][0]["name"], "doomed");
        assert_eq!(
            status(app.world_mut(), &json!({})).expect("status")["state"],
            "idle"
        );

        assert_eq!(run(app.world_mut(), &json!({})).expect("starts")["started"], 2);
        let busy = run(app.world_mut(), &json!({})).expect_err("busy");
        assert_eq!(reason_of(&busy), "already_running");
        for _ in 0..20 {
            app.update();
            if status(app.world_mut(), &json!({})).expect("status")["finished"] == true {
                break;
            }
        }
        let status = status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(
            (
                status["state"].as_str(),
                status["done"].as_u64(),
                status["total"].as_u64()
            ),
            (Some("done"), Some(2), Some(2))
        );

        let report = results(app.world_mut(), &json!({})).expect("results");
        let doomed = report["tests"]
            .as_array()
            .expect("tests")
            .iter()
            .find(|t| t["name"] == "doomed")
            .expect("doomed");
        assert_eq!(doomed["verdict"], "Failed");
        assert_eq!(doomed["steps"][0]["name"], "check the answer");
        assert_eq!(doomed["steps"][0]["verdict"], "Failed");
        assert!(
            doomed["message"]
                .as_str()
                .expect("message")
                .contains("answer: expected 7, got 6"),
            "{doomed}"
        );
        assert_eq!(report["exit_code"], 1);
        let since = results(app.world_mut(), &json!({ "since": 1 })).expect("since");
        assert_eq!(since["tests"].as_array().expect("tests").len(), 1);
    }

    #[test]
    fn error_reasons() {
        let mut app = app();
        assert_eq!(
            reason_of(&run(app.world_mut(), &json!({})).expect_err("none")),
            "no_tests"
        );
        assert_eq!(
            reason_of(&abort(app.world_mut(), &json!({})).expect_err("idle")),
            "not_running"
        );
        assert_eq!(
            reason_of(&run(app.world_mut(), &json!({ "tags": 5 })).expect_err("bad")),
            "bad_params"
        );
        assert_eq!(
            reason_of(&run(app.world_mut(), &json!({ "params": { "a": 1 } })).expect_err("bad")),
            "bad_params"
        );
        assert_eq!(
            reason_of(&create(app.world_mut(), &json!({ "body": "rust:a" })).expect_err("bad")),
            "bad_params"
        );
        assert_eq!(
            reason_of(
                &create(app.world_mut(), &json!({ "name": "a", "body": "nonsense" })).expect_err("bad")
            ),
            "bad_params"
        );
        let bad_script = create(
            app.world_mut(),
            &json!({ "name": "a", "body": "script:t.functest.ron", "script": "nope" }),
        )
        .expect_err("bad script");
        assert_eq!(reason_of(&bad_script), "script_invalid");
        let escape = create(app.world_mut(), &json!({ "name": "a", "body": "script:../t.functest.ron", "script": "(format_version: 1, steps: [(name: \"s\")])" }))
            .expect_err("escape");
        assert_eq!(reason_of(&escape), "bad_params");
        assert_eq!(
            reason_of(&bless(app.world_mut(), &json!({ "test": "x" })).expect_err("unconfirmed")),
            "needs_confirmation"
        );
        assert_eq!(
            reason_of(
                &bless(app.world_mut(), &json!({ "test": "x", "confirm": true })).expect_err("unknown")
            ),
            "test_not_found"
        );
    }

    #[test]
    fn abort_stops_a_looped_run() {
        let mut app = app();
        create(
            app.world_mut(),
            &json!({ "name": "fine", "body": format!("rust:{}", std::any::type_name::<Fine>()) }),
        )
        .expect("creates");
        run(app.world_mut(), &json!({ "looped": true })).expect("starts");
        frames(&mut app, 12);
        assert_eq!(
            abort(app.world_mut(), &json!({ "reason": "enough" })).expect("aborts")["aborted"],
            true
        );
        frames(&mut app, 6);
        assert_eq!(
            status(app.world_mut(), &json!({})).expect("status")["state"],
            "done"
        );
    }
}
