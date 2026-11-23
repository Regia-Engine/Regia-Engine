// SPDX-License-Identifier: MPL-2.0
//! Game-code reload surface for the toolbar (`docs/specs/19-editor.md` § 5): a Compile button
//! (Ctrl+Alt+F11) and the idle / compiling / error / reloaded indicator.
//!
//! The [`crate::HotReloadHost`] lives outside the edit world (it owns its own app), so the
//! editor loop publishes its state here each frame and consumes [`CompileRequest`].
//!
//! The agent protocol's `agent.hot_reload` (`docs/specs/23-agent-native.md` § 3.6, ADR-5707)
//! rides the same bridge: [`agent_hot_reload`] validates the request in the edit world (a game
//! crate is hosted, no compile runs, no reload job is pending), asks for a build with
//! [`CompileRequest`] and starts a `hot:<n>` job; the editor loop calls
//! [`service_hot_reload_job`] after polling the host, which finishes the job with the reload's
//! structured outcome once the host publishes a report (recognised by a changed
//! [`crate::HotReloadHost::report_serial`]).
//!
//! One method reloads game code: the protocol's `agent.hot_reload` (ADR-5993: hot reload of game
//! code is not editor-only, so the editor's former `editor.hot_reload` extension method is gone and
//! [`agent_hot_reload`] is that method's editor-side implementation). The extension method
//! `editor.code_status` (`specs/23` § 3.6, ADR-5627) reads [`CodeReloadStatus`], the toolbar's state.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::hot_reload::{CompileState, HotReloadHost};

/// The game-code state the toolbar shows.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct CodeReloadStatus {
    /// The game crate being hosted (`None` without `--game`: the Compile button is disabled).
    pub game: Option<String>,
    /// The host's state.
    pub state: Option<CompileState>,
    /// The host's report serial when this status was published (a changed
    /// [`crate::HotReloadHost::report_serial`] means a build or refusal finished since).
    pub report_serial: u64,
}

impl CodeReloadStatus {
    /// Short toolbar text: `idle`, `compiling 3.2 s`, `error: …`, `reloaded: …`.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.state {
            None | Some(CompileState::Idle) => "code: idle".to_string(),
            Some(CompileState::Compiling(secs)) => format!("code: compiling {secs:.1} s"),
            Some(CompileState::Error(err)) => {
                let first = err.lines().next().unwrap_or_default();
                format!("code: error — {first}")
            }
            Some(CompileState::Reloaded(toast)) => format!("code: {toast}"),
        }
    }
}

/// Set by the Compile button or Ctrl+Alt+F11; the editor loop starts a build and clears it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompileRequest(pub bool);

/// A reason-coded refusal of the agent surface (`data.reason`, specs/23 § 4).
fn reasoned(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(code, "F0002", message, "see book/agents/code.md");
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// The editor's `agent.hot_reload` hook (`regia_agent::hot_reload::HotReloadHook`).
///
/// Starts a `hot:<n>` job and answers its id; the editor loop finishes the job through
/// [`service_hot_reload_job`]. Refusals: `hot_reload_no_game` (no `--game` host),
/// `hot_reload_busy` (a compile runs or a reload job is still pending).
///
/// # Errors
/// `hot_reload_no_game` or `hot_reload_busy` as described.
pub fn agent_hot_reload(world: &mut World, dry_run: bool) -> Result<Value, regia_agent::protocol::RpcError> {
    let status = world
        .get_resource::<CodeReloadStatus>()
        .cloned()
        .unwrap_or_default();
    let Some(game) = status.game else {
        return Err(reasoned(
            regia_agent::protocol::INVALID_PARAMS,
            "hot_reload_no_game",
            "the editor hosts no game crate; restart it with --game <package>",
        ));
    };
    regia_agent::hot_reload::refuse_if_pending(world)?;
    if world.contains_resource::<regia_agent::hot_reload::HotReloadJobPending>() {
        // A pending request the job registry does not list as running (the registry was
        // replaced): still busy until the service finishes it, never a panic.
        return Err(reasoned(
            regia_agent::protocol::INVALID_PARAMS,
            "hot_reload_busy",
            "an earlier agent.hot_reload request is still waiting for the editor's build",
        ));
    }
    if matches!(status.state, Some(CompileState::Compiling(_))) {
        return Err(reasoned(
            regia_agent::protocol::INVALID_PARAMS,
            "hot_reload_busy",
            "the editor is already compiling game code",
        ));
    }
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "game": game,
            "would": "rebuild the game crate and swap the new code in",
        }));
    }
    let (job, handle) = {
        let mut registry =
            world.get_resource_or_insert_with::<regia_agent::jobs::JobRegistry>(Default::default);
        registry.start("hot", format!("hot reload {game}"), true)
    };
    handle.progress(None, "compile requested");
    handle.event(json!({ "kind": "started", "game": game }));
    world
        .get_resource_or_insert_with::<crate::editor_jobs::EditorJobs>(Default::default)
        .register(&job, handle.clone());
    world.insert_resource(regia_agent::hot_reload::HotReloadJobPending {
        handle,
        report_serial: status.report_serial,
        started: std::time::Instant::now(),
    });
    world.insert_resource(CompileRequest(true));
    Ok(json!({ "job": job }))
}

/// Finishes a pending `agent.hot_reload` job when the host's build and swap completed (or a
/// precondition refused it). The editor loop calls this each frame after polling the host
/// (`host` is `None` when the editor hosts no game crate).
///
/// While the host compiles, the job's progress reports the wait. The job is finished when the
/// host is not compiling and its [`crate::HotReloadHost::report_serial`] differs from the one
/// recorded at request time; the result carries the report's structured fields. A cancel
/// (`agent.job_cancel`, `editor.cancel_job`) ends the job on the next call, state `cancelled`, and
/// stops the build the editor started for it ([`HotReloadHost::cancel_compile`]: cargo is killed,
/// nothing partial reaches the hot dir). Without a host the job fails with `hot_reload_no_game`.
pub fn service_hot_reload_job(world: &mut World, host: Option<&HotReloadHost>) {
    // `editor.cancel_job { id: "build" }`: a build with no job of its own (toolbar, source change).
    if world
        .remove_resource::<crate::editor_jobs::CancelBuildRequest>()
        .is_some()
        && host.is_some_and(HotReloadHost::cancel_compile)
    {
        tracing::info!(target: "regia_editor", "game build cancel requested");
    }
    let Some(pending) = world.get_resource::<regia_agent::hot_reload::HotReloadJobPending>() else {
        return;
    };
    if pending.handle.cancel_requested() {
        // `specs/19` § 7: the cancel stops the build itself (cargo is killed, nothing is copied
        // into the hot dir); the old code keeps running.
        let stopped = host.is_some_and(HotReloadHost::cancel_compile);
        pending
            .handle
            .event(json!({ "kind": "cancelled", "build_stopped": stopped }));
        pending.handle.finish(json!({
            "cancelled": true,
            "build_stopped": stopped,
            "detail": if stopped {
                "the game build was stopped; the old code keeps running"
            } else {
                "the wait was cancelled before a build started; the old code keeps running"
            },
        }));
        world.remove_resource::<regia_agent::hot_reload::HotReloadJobPending>();
        return;
    }
    let Some(host) = host else {
        pending.handle.fail(
            "hot_reload_no_game",
            "the editor no longer hosts a game crate, so the reload cannot finish",
        );
        world.remove_resource::<regia_agent::hot_reload::HotReloadJobPending>();
        return;
    };
    if host.is_compiling() {
        pending.handle.progress(
            None,
            format!(
                "compiling game code ({:.1} s)",
                pending.started.elapsed().as_secs_f64()
            ),
        );
        return;
    }
    if host.report_serial() == pending.report_serial {
        // The loop has not serviced our CompileRequest yet (or the build has not reported).
        return;
    }
    let Some(report) = host.last_report().cloned() else {
        pending.handle.fail(
            "hot_reload_no_report",
            "the build ended without a report; check the editor log",
        );
        world.remove_resource::<regia_agent::hot_reload::HotReloadJobPending>();
        return;
    };
    let Some(pending) = world.remove_resource::<regia_agent::hot_reload::HotReloadJobPending>() else {
        return;
    };
    pending.handle.event(json!({
        "kind": "finished",
        "phase": report.phase.as_str(),
        "accepted": report.accepted,
    }));
    pending.handle.finish(json!({
        "accepted": report.accepted,
        "phase": report.phase.as_str(),
        "generation": report.generation,
        "detail": report.detail,
        "toast": report.toast,
        "elapsed_ms": u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        "types": report.types,
        "instances": report.instances,
        "scripts_rebuilt": report.scripts_rebuilt,
        "scripts_dropped": report.scripts_dropped,
    }));
}

/// Agent handler signature (the same as `regia_editor::headless`'s).
pub type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The code-reload extension methods with their handlers (`editor.code_status`; the reload itself
/// is `agent.hot_reload`, served by [`agent_hot_reload`], ADR-5993).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.code_status",
            tool: "code_status",
            description: "The game-code state the toolbar shows: the hosted game crate (null without --game), \
                    state idle | compiling | error | reloaded, the compile seconds so far, the compiler error or \
                    refusal text, the reload toast, and whether a requested build has not started yet.",
            params: &[],
            result: "{ game, state, label, compiling_s, error, toast, requested }",
            mutating: false,
            permission: Permission::Read,
        },
        code_status as Handler,
    )]
}

fn state_fields(status: Option<&CodeReloadStatus>) -> (&'static str, Value, Value, Value) {
    match status.and_then(|status| status.state.as_ref()) {
        None | Some(CompileState::Idle) => ("idle", Value::Null, Value::Null, Value::Null),
        Some(CompileState::Compiling(secs)) => ("compiling", json!(secs), Value::Null, Value::Null),
        Some(CompileState::Error(error)) => ("error", Value::Null, json!(error), Value::Null),
        Some(CompileState::Reloaded(toast)) => ("reloaded", Value::Null, Value::Null, json!(toast)),
    }
}

fn code_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let status = world.get_resource::<CodeReloadStatus>();
    let (state, compiling_s, error, toast) = state_fields(status);
    Ok(json!({
        "game": status.and_then(|status| status.game.clone()),
        "state": state,
        "label": status.map(CodeReloadStatus::label).unwrap_or_else(|| CodeReloadStatus::default().label()),
        "compiling_s": compiling_s,
        "error": error,
        "toast": toast,
        "requested": world.get_resource::<CompileRequest>().is_some_and(|request| request.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_at(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    fn status(game: Option<&str>, state: Option<CompileState>, serial: u64) -> CodeReloadStatus {
        CodeReloadStatus {
            game: game.map(str::to_string),
            state,
            report_serial: serial,
        }
    }

    #[test]
    fn labels_cover_every_state() {
        let mut status = CodeReloadStatus::default();
        assert_eq!(status.label(), "code: idle");
        status.state = Some(CompileState::Compiling(3.24));
        assert_eq!(status.label(), "code: compiling 3.2 s");
        status.state = Some(CompileState::Error(
            "error[E0308]: mismatched types\n --> src/lib.rs".into(),
        ));
        assert_eq!(status.label(), "code: error — error[E0308]: mismatched types");
        status.state = Some(CompileState::Reloaded(
            "Reloaded in 1.3 s (12 types, 340 instances)".into(),
        ));
        assert_eq!(
            status.label(),
            "code: Reloaded in 1.3 s (12 types, 340 instances)"
        );
    }

    #[test]
    fn without_a_game_the_hook_refuses() {
        let root = regia_test_utils::unique_temp_dir("code-reload-no-game");
        let mut world = world_at(&root);
        world.insert_resource(status(None, None, 0));
        let error = agent_hot_reload(&mut world, false).expect_err("no game");
        assert_eq!(
            error.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "hot_reload_no_game"
        );
        let dry = agent_hot_reload(&mut world, true).expect_err("dry run too");
        assert_eq!(
            dry.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "hot_reload_no_game"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn while_compiling_the_hook_refuses() {
        let root = regia_test_utils::unique_temp_dir("code-reload-compiling");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), Some(CompileState::Compiling(1.0)), 0));
        let error = agent_hot_reload(&mut world, false).expect_err("compiling");
        assert_eq!(
            error.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "hot_reload_busy"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_reports_without_a_request_or_job() {
        let root = regia_test_utils::unique_temp_dir("code-reload-dry");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        let answer = agent_hot_reload(&mut world, true).expect("dry run");
        assert_eq!(answer["dry_run"], true);
        assert_eq!(answer["game"], "demo");
        assert!(!world.contains_resource::<CompileRequest>());
        assert!(!world.contains_resource::<regia_agent::hot_reload::HotReloadJobPending>());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_request_starts_a_job_and_asks_the_loop_for_a_build() {
        let root = regia_test_utils::unique_temp_dir("code-reload-start");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 7));
        let answer = agent_hot_reload(&mut world, false).expect("started");
        assert_eq!(answer["job"], "hot:1");
        let pending = world
            .get_resource::<regia_agent::hot_reload::HotReloadJobPending>()
            .expect("pending");
        assert_eq!(pending.report_serial, 7);
        assert!(world.get_resource::<CompileRequest>().is_some_and(|r| r.0));
        // A second request while the job is pending is refused.
        let error = agent_hot_reload(&mut world, false).expect_err("pending");
        assert_eq!(
            error.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "hot_reload_busy"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The job is finished by a changed report serial, with the report's structured fields.
    #[test]
    fn the_service_finishes_the_job_on_a_fresh_report() {
        let root = regia_test_utils::unique_temp_dir("code-reload-service");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        let answer = agent_hot_reload(&mut world, false).expect("started");
        let job = answer["job"].as_str().expect("job id").to_string();
        let host = crate::hot_reload::HotReloadHost::new(crate::hot_reload::HotReloadConfig {
            manifest_path: root.join("Cargo.toml"),
            package: "demo".into(),
            crate_dir: root.join("game"),
            target_dir: root.join("target"),
            hot_dir: root.join("hot"),
        });
        // No report yet: the service leaves the job running.
        service_hot_reload_job(&mut world, Some(&host));
        let listed =
            regia_agent::jobs::job_status(&mut world, &json!({ "job": job }), &mut no_call).expect("status");
        assert_eq!(listed["state"], "running");
        // A refusal report (the host publishes one for a build that cannot run) finishes it.
        let host = host_refusal(&root);
        service_hot_reload_job(&mut world, Some(&host));
        let listed =
            regia_agent::jobs::job_status(&mut world, &json!({ "job": job }), &mut no_call).expect("status");
        assert_eq!(listed["state"], "done", "{listed}");
        assert_eq!(listed["result"]["accepted"], false);
        assert_eq!(listed["result"]["phase"], "refused");
        // A refused reload rebuilt and dropped no script slots (spec 20 § 3.9 counts).
        assert_eq!(listed["result"]["scripts_rebuilt"], 0, "{listed}");
        assert_eq!(listed["result"]["scripts_dropped"], 0, "{listed}");
        assert!(
            listed["result"]["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "{listed}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn reason_of(error: regia_agent::protocol::RpcError) -> Value {
        error.data.map(|d| d["reason"].clone()).unwrap_or_default()
    }

    fn test_host(root: &std::path::Path) -> HotReloadHost {
        HotReloadHost::new(crate::hot_reload::HotReloadConfig {
            manifest_path: root.join("Cargo.toml"),
            package: "demo".into(),
            crate_dir: root.join("game"),
            target_dir: root.join("target"),
            hot_dir: root.join("hot"),
        })
    }

    /// A pending reload the job registry no longer lists (the registry resource was replaced)
    /// is refused with the reasoned `hot_reload_busy`, never a panic (review rk9 finding 1).
    #[test]
    fn a_pending_job_the_registry_does_not_list_is_refused_not_a_panic() {
        let root = regia_test_utils::unique_temp_dir("code-reload-orphan");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        agent_hot_reload(&mut world, false).expect("started");
        world.insert_resource(regia_agent::jobs::JobRegistry::default());
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            agent_hot_reload(&mut world, false)
        }));
        let Ok(answer) = refused else {
            panic!("the hook panicked on a pending job the registry does not list");
        };
        let error = answer.expect_err("still pending");
        assert_eq!(reason_of(error), "hot_reload_busy");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `agent.job_cancel` ends a pending `hot` job (state `cancelled`) and frees the next
    /// `agent.hot_reload`, even when the host never publishes a report (review rk9 finding 2).
    #[test]
    fn a_cancelled_job_ends_and_frees_the_next_request() {
        let root = regia_test_utils::unique_temp_dir("code-reload-cancel");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        let answer = agent_hot_reload(&mut world, false).expect("started");
        let job = answer["job"].as_str().expect("job id").to_string();
        world
            .get_resource::<regia_agent::hot_reload::HotReloadJobPending>()
            .expect("pending")
            .handle
            .request_cancel();
        // The host never reports: no build ran, the serial is unchanged.
        let host = test_host(&root);
        service_hot_reload_job(&mut world, Some(&host));
        let listed =
            regia_agent::jobs::job_status(&mut world, &json!({ "job": job }), &mut no_call).expect("status");
        assert_eq!(listed["state"], "cancelled", "{listed}");
        assert!(!world.contains_resource::<regia_agent::hot_reload::HotReloadJobPending>());
        let next = agent_hot_reload(&mut world, false).expect("the next request starts");
        assert_eq!(next["job"], "hot:2");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `specs/19` § 7: `editor.cancel_job` on the build's job ends it `cancelled` (and asks the
    /// host to stop the build; this host runs none, so `build_stopped` is false).
    #[test]
    fn editor_cancel_job_ends_the_build_job_cancelled() {
        let root = regia_test_utils::unique_temp_dir("code-reload-editor-cancel");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        let answer = agent_hot_reload(&mut world, false).expect("started");
        let job = answer["job"].as_str().expect("job id").to_string();
        let cancel = crate::editor_jobs::cancel_job(&mut world, &json!({ "id": job })).expect("cancel");
        assert_eq!(cancel["requested"], true, "{cancel}");
        let host = test_host(&root);
        service_hot_reload_job(&mut world, Some(&host));
        let listed =
            regia_agent::jobs::job_status(&mut world, &json!({ "job": job }), &mut no_call).expect("status");
        assert_eq!(listed["state"], "cancelled", "{listed}");
        assert_eq!(listed["result"]["build_stopped"], false, "{listed}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pending job whose host went away fails with `hot_reload_no_game` instead of staying
    /// `running` (and the editor `hot_reload_busy`) forever.
    #[test]
    fn without_a_host_the_service_fails_the_pending_job() {
        let root = regia_test_utils::unique_temp_dir("code-reload-no-host");
        let mut world = world_at(&root);
        world.insert_resource(status(Some("demo"), None, 0));
        let answer = agent_hot_reload(&mut world, false).expect("started");
        let job = answer["job"].as_str().expect("job id").to_string();
        service_hot_reload_job(&mut world, None);
        let listed =
            regia_agent::jobs::job_status(&mut world, &json!({ "job": job }), &mut no_call).expect("status");
        assert_eq!(listed["state"], "failed", "{listed}");
        assert_eq!(listed["error"]["reason"], "hot_reload_no_game", "{listed}");
        assert!(!world.contains_resource::<regia_agent::hot_reload::HotReloadJobPending>());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn no_call(_: &mut World, method: &str, _: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
        Err(regia_agent::protocol::RpcError::internal(format!(
            "unexpected call to {method}"
        )))
    }

    /// A host that has published one refusal: the precondition refusal of `request_compile`
    /// without a hot-dylib build or an engine dylib in the target dir.
    fn host_refusal(root: &std::path::Path) -> HotReloadHost {
        let mut host = HotReloadHost::new(crate::hot_reload::HotReloadConfig {
            manifest_path: root.join("Cargo.toml"),
            package: "demo".into(),
            crate_dir: root.join("game"),
            target_dir: root.join("target"),
            hot_dir: root.join("hot"),
        });
        let _ = host.request_compile();
        host
    }

    /// `agent.hot_reload` (the editor's hook, which replaced `editor.hot_reload`, ADR-5993) without
    /// `--game` refuses and requests nothing.
    #[test]
    fn hot_reload_without_a_game_answers_no_game() {
        let mut world = World::new();
        let Err(error) = agent_hot_reload(&mut world, false) else {
            panic!("hot_reload without a game must refuse");
        };
        assert_eq!(
            error.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "hot_reload_no_game"
        );
        assert!(world.get_resource::<CompileRequest>().is_none());
        world.insert_resource(CodeReloadStatus::default());
        assert!(agent_hot_reload(&mut world, false).is_err());
        assert!(world.get_resource::<CompileRequest>().is_none());
    }

    /// `agent.hot_reload` sets the same request the Compile button sets, and
    /// `editor.code_status` reports it until the editor loop takes it.
    #[test]
    fn hot_reload_sets_the_compile_request_and_code_status_reports_it() {
        let root = regia_test_utils::unique_temp_dir("code-reload-request");
        let mut world = world_at(&root);
        world.insert_resource(CodeReloadStatus {
            game: Some("my_game".into()),
            state: Some(CompileState::Idle),
            report_serial: 0,
        });
        let answer = agent_hot_reload(&mut world, false).expect("requested");
        assert!(
            answer["job"].as_str().is_some_and(|job| job.starts_with("hot:")),
            "{answer}"
        );
        assert!(world.resource::<CompileRequest>().0);
        let _ = std::fs::remove_dir_all(&root);
        let status = code_status(&mut world, &json!({})).expect("status");
        assert_eq!(status["requested"], true);
        assert_eq!(status["game"], "my_game");
        // The editor loop takes the request (`std::mem::take`, as `headless.rs` does).
        world.resource_mut::<CompileRequest>().0 = false;
        let status = code_status(&mut world, &json!({})).expect("status");
        assert_eq!(status["requested"], false);
    }

    /// `editor.code_status` maps every toolbar state to the documented fields.
    #[test]
    fn code_status_covers_every_state() {
        let mut world = World::new();
        let status = code_status(&mut world, &json!({})).expect("no status yet");
        assert_eq!(status["state"], "idle");
        assert_eq!(status["game"], Value::Null);
        assert_eq!(status["label"], "code: idle");
        let cases = [
            (CompileState::Compiling(2.5), "compiling", "compiling_s"),
            (CompileState::Error("error[E0308]".into()), "error", "error"),
            (
                CompileState::Reloaded("Reloaded in 1.3 s (1 types, 2 instances)".into()),
                "reloaded",
                "toast",
            ),
        ];
        for (state, name, field) in cases {
            world.insert_resource(CodeReloadStatus {
                game: Some("g".into()),
                state: Some(state),
                report_serial: 0,
            });
            let status = code_status(&mut world, &json!({})).expect("status");
            assert_eq!(status["state"], name, "{status}");
            assert!(!status[field].is_null(), "{field} missing: {status}");
        }
    }

    /// The registered names reach these handlers: an agent calling `editor.code_status` by name
    /// gets the answer the direct tests above check, and `editor.hot_reload` is no longer an
    /// extension method (one method, `agent.hot_reload`, ADR-5993).
    #[test]
    fn the_registered_method_names_reach_their_handlers() {
        let by_name = |name: &str| {
            methods()
                .into_iter()
                .find(|(schema, _)| schema.name == name)
                .map(|(_, handler)| handler)
                .unwrap_or_else(|| panic!("{name} is not registered"))
        };
        let mut world = World::new();
        let status = by_name("editor.code_status")(&mut world, &json!({})).expect("status");
        assert_eq!(status["state"], "idle");
        assert!(
            methods()
                .iter()
                .all(|(schema, _)| schema.name != "editor.hot_reload" && schema.tool != "hot_reload"),
            "the MCP tool name hot_reload belongs to agent.hot_reload alone"
        );
    }
}
