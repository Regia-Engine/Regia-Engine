// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests of the spec 23 gap methods (ADR-2346) through the real dispatcher: permissions,
//! contracts, routing, files on disk. `agent.job_status`, `agent.job_cancel`, `agent.run_tests`,
//! `agent.build_game`, `agent.hot_reload`, `agent.open_in_editor`, `review.propose`,
//! `review.list`, `review.diff`, `review.accept`, `review.reject`.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::dispatch::{AgentInbox, RateLimit, process_requests};
use crate::protocol::{Request, Response};
use crate::safety::{AgentConfig, AgentPermissions, AgentSession, AuditLog};
use crate::server::Inbound;

fn app(project: &Path, permissions: &[&str]) -> App {
    let config = AgentConfig {
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        ..AgentConfig::default()
    };
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentPermissions { config, source: None })
        .insert_resource(crate::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        })
        .init_resource::<AuditLog>()
        .insert_resource(AgentSession {
            client: "gaps-test".into(),
            model: "test-model".into(),
            session: "s1".into(),
        })
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    drop(inbox_tx);
    rx.recv_timeout(Duration::from_secs(5)).expect("answered")
}

fn ok(app: &mut App, method: &str, params: Value) -> Value {
    let response = call(app, method, params);
    response
        .result
        .unwrap_or_else(|| panic!("{method}: {:?}", response.error))
}

/// `(data.code, data.reason)` of a refused call.
fn refusal(app: &mut App, method: &str, params: Value) -> (String, String) {
    let response = call(app, method, params);
    let data = response
        .error
        .and_then(|e| e.data)
        .unwrap_or_else(|| panic!("{method} was not refused"));
    (
        data["code"].as_str().unwrap_or_default().to_string(),
        data["reason"].as_str().unwrap_or_default().to_string(),
    )
}

fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("materials")).expect("dir");
    std::fs::write(
        root.join("materials/door.material.ron"),
        "(\n    base_color: (1.0, 1.0, 1.0, 1.0),\n    roughness: 0.5,\n)\n",
    )
    .expect("material");
    root
}

const WARMER: &str = "(\n    base_color: (0.8, 0.3, 0.2, 1.0),\n    roughness: 0.5,\n)\n";

#[test]
fn a_proposal_waits_for_review_and_the_agent_cannot_accept_it() {
    let root = project("review-flow");
    let mut agent = app(&root, &["read", "edit_scene", "assets", "play"]);
    let proposed = ok(
        &mut agent,
        "review.propose",
        json!({
            "changes": [
                { "path": "materials/door.material.ron", "text": WARMER },
                { "path": "materials/new.material.ron", "text": "(roughness: 0.9)\n" },
            ],
            "reason": "warmer door, as asked",
        }),
    );
    assert_eq!(proposed["set"], 1);
    assert_eq!(proposed["changes"][0]["kind"], "modify");
    assert_eq!(proposed["changes"][1]["kind"], "create");
    let diff = proposed["changes"][0]["diff"].as_str().unwrap_or_default();
    assert!(diff.contains("-    base_color: (1.0, 1.0, 1.0, 1.0),"), "{diff}");
    assert!(diff.contains("+    base_color: (0.8, 0.3, 0.2, 1.0),"), "{diff}");
    // Nothing was written.
    let on_disk = std::fs::read_to_string(root.join("materials/door.material.ron")).expect("read");
    assert!(on_disk.contains("1.0, 1.0, 1.0"));
    assert!(!root.join("materials/new.material.ron").exists());

    let listed = ok(&mut agent, "review.list", json!({}));
    assert_eq!(listed["pending"], 2);
    assert_eq!(listed["changes"][0]["client"], "gaps-test");
    let one = ok(&mut agent, "review.diff", json!({ "id": 1, "full": true }));
    assert!(
        one["diff"]
            .as_str()
            .unwrap_or_default()
            .starts_with("--- a/materials/door.material.ron")
    );
    assert_eq!(one["after"], WARMER);

    // The agent's own profile has no `review` permission.
    let (code, _) = refusal(&mut agent, "review.accept", json!({ "all": true }));
    assert_eq!(code, "F0012");

    // The queue persisted: a new app (the editor after a restart) sees it.
    let mut owner = app(&root, &["read", "assets", "review"]);
    let accepted = ok(&mut owner, "review.accept", json!({ "ids": [1], "note": "good" }));
    assert_eq!(accepted["applied"], 1);
    assert_eq!(accepted["results"][0]["meta"], "no_meta");
    assert_eq!(
        std::fs::read_to_string(root.join("materials/door.material.ron")).expect("read"),
        WARMER
    );
    let again = ok(&mut owner, "review.accept", json!({ "ids": [1] }));
    assert_eq!(again["results"][0]["reason"], "review_already_decided");
    let rejected = ok(
        &mut owner,
        "review.reject",
        json!({ "set": 1, "note": "not now" }),
    );
    assert_eq!(rejected["decided"], 1);
    assert!(
        !root.join("materials/new.material.ron").exists(),
        "a rejected change writes nothing"
    );
    let history = ok(&mut owner, "review.list", json!({ "status": "all" }));
    let statuses: Vec<&str> = history["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["status"].as_str())
        .collect();
    assert_eq!(statuses, ["accepted", "rejected"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_that_moved_on_is_a_conflict_and_meta_provenance_is_stamped() {
    let root = project("review-conflict");
    let source = root.join("materials/door.material.ron");
    let meta = regia_asset::MetaSidecar::new("material_ron", regia_asset::AssetId::new_random());
    std::fs::write(
        regia_asset::meta_path_for(&source),
        meta.to_ron_string().expect("meta text"),
    )
    .expect("meta");
    let mut owner = app(&root, &["read", "assets", "review"]);
    ok(
        &mut owner,
        "review.propose",
        json!({ "changes": [{ "path": "materials/door.material.ron", "text": WARMER }], "reason": "r" }),
    );
    std::fs::write(&source, "(roughness: 0.1)\n").expect("someone else edits");
    let conflict = ok(&mut owner, "review.accept", json!({ "all": true }));
    assert_eq!(conflict["results"][0]["reason"], "review_conflict");
    assert_eq!(conflict["pending"], 1, "a conflicting change stays pending");
    ok(&mut owner, "review.reject", json!({ "all": true }));

    std::fs::write(&source, "(roughness: 0.2)\n").expect("reset");
    ok(
        &mut owner,
        "review.propose",
        json!({ "changes": [{ "path": "materials/door.material.ron", "text": WARMER }], "reason": "again" }),
    );
    let accepted = ok(&mut owner, "review.accept", json!({ "set": 2 }));
    assert_eq!(accepted["results"][0]["meta"], "stamped");
    let stamped = regia_asset::MetaSidecar::from_ron(
        &std::fs::read(regia_asset::meta_path_for(&source)).expect("meta back"),
    )
    .expect("parses");
    let provenance = stamped.provenance.expect("provenance");
    assert_eq!(provenance.reviewer, regia_asset::ReviewerState::Owner);
    assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Agent);
    assert_eq!(provenance.client.as_deref(), Some("gaps-test"));
    assert_eq!(provenance.model.as_deref(), Some("test-model"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn proposals_outside_the_project_or_into_agent_state_are_refused() {
    let root = project("review-deny");
    let mut agent = app(&root, &["read", "assets"]);
    let text = |path: &str| json!({ "changes": [{ "path": path, "text": "x" }], "reason": "r" });
    assert_eq!(
        refusal(&mut agent, "review.propose", text("../escape.txt")).0,
        "F0002"
    );
    assert_eq!(
        refusal(&mut agent, "review.propose", text("/etc/hosts")).0,
        "F0002"
    );
    assert_eq!(
        refusal(&mut agent, "review.propose", text(".regia/agent.toml")).1,
        "review_path_denied"
    );
    assert_eq!(
        refusal(&mut agent, "review.propose", text(".git/config")).1,
        "review_path_denied"
    );
    // The same folders in another case (Windows and macOS file systems ignore case).
    assert_eq!(
        refusal(&mut agent, "review.propose", text(".REGIA/agent.toml")).1,
        "review_path_denied"
    );
    assert_eq!(
        refusal(&mut agent, "review.propose", text("sub/.GIT/hooks/pre-commit")).1,
        "review_path_denied"
    );
    // The file a proposal replaces is capped like the proposed text.
    std::fs::write(
        root.join("huge.txt"),
        "x".repeat(crate::review::MAX_TEXT_BYTES + 1),
    )
    .expect("huge");
    assert_eq!(
        refusal(&mut agent, "review.propose", text("huge.txt")).1,
        "review_too_large"
    );
    std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).expect("binary");
    assert_eq!(
        refusal(&mut agent, "review.propose", text("blob.bin")).1,
        "review_not_text"
    );
    assert_eq!(
        refusal(
            &mut agent,
            "review.propose",
            json!({ "changes": [], "reason": "r" })
        )
        .0,
        "F0002"
    );
    assert_eq!(
        refusal(
            &mut agent,
            "review.propose",
            json!({ "changes": [{ "path": "a", "text": "b" }] })
        )
        .0,
        "F0002",
        "reason is required"
    );
    let dry = ok(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": "notes.txt", "text": "hi\n" }], "reason": "r", "dry_run": true }),
    );
    assert_eq!(dry["dry_run"], true);
    assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 0);
    assert_eq!(
        refusal(&mut agent, "review.diff", json!({ "id": 99 })).1,
        "review_change_not_found"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Without the `review` permission an agent withdraws only its own changes.
#[test]
fn an_agent_withdraws_only_its_own_changes() {
    let root = project("review-own");
    let mut agent = app(&root, &["read", "assets"]);
    let mine = ok(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": "materials/door.material.ron", "text": WARMER }], "reason": "mine" }),
    );
    assert_eq!(mine["changes"][0]["added"], 1);
    assert_eq!(mine["changes"][0]["removed"], 1);
    // Another agent (another session) proposes too.
    agent.world_mut().insert_resource(AgentSession {
        client: "other-agent".into(),
        model: "m".into(),
        session: "s2".into(),
    });
    ok(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": "notes.txt", "text": "theirs\n" }], "reason": "theirs" }),
    );
    // The other agent cannot reject the first one's change by id, and `all` selects only its own.
    let by_id = ok(&mut agent, "review.reject", json!({ "ids": [1] }));
    assert_eq!(by_id["results"][0]["reason"], "review_not_yours");
    assert_eq!(by_id["decided"], 0);
    let all = ok(&mut agent, "review.reject", json!({ "all": true }));
    assert_eq!(all["decided"], 1, "{all}");
    assert_eq!(all["results"][0]["id"], 2);
    let listed = ok(&mut agent, "review.list", json!({}));
    assert_eq!(listed["pending"], 1);
    assert_eq!(listed["changes"][0]["id"], 1);
    // Line counts are stored at proposal.
    assert_eq!(listed["changes"][0]["added"], 1);
    // The owner (review permission) rejects anyone's.
    let mut owner = app(&root, &["read", "assets", "review"]);
    assert_eq!(
        ok(&mut owner, "review.reject", json!({ "ids": [1] }))["decided"],
        1
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Decided changes past the newest `KEEP_DECIDED_TEXTS` keep their summary but drop their texts.
#[test]
fn old_decided_changes_drop_their_texts() {
    let root = project("review-prune");
    let mut owner = app(&root, &["read", "assets", "review"]);
    let count = crate::review::KEEP_DECIDED_TEXTS + 8;
    let changes: Vec<Value> = (0..count)
        .map(|n| json!({ "path": format!("note{n}.txt"), "text": format!("note {n}\n") }))
        .collect();
    ok(
        &mut owner,
        "review.propose",
        json!({ "changes": changes, "reason": "notes" }),
    );
    assert_eq!(
        ok(&mut owner, "review.reject", json!({ "all": true }))["decided"],
        count
    );
    let queue = crate::review::queue(owner.world_mut());
    let dropped: Vec<bool> = queue.file.changes.iter().map(|c| c.texts_dropped).collect();
    assert_eq!(dropped.iter().filter(|d| **d).count(), 8, "{dropped:?}");
    assert!(dropped[..8].iter().all(|d| *d), "the oldest drop first");
    let first = &queue.file.changes[0];
    assert!(first.after.is_empty());
    assert_eq!(first.kind(), "create");
    assert_eq!(first.counts(), (1, 0), "the counts survive");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_tests_needs_build_and_names_its_command() {
    let root = regia_test_utils::unique_temp_dir("run-tests-perm");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    let mut agent = app(&root, &["read", "edit_scene", "assets", "play"]);
    assert_eq!(
        refusal(
            &mut agent,
            "agent.run_tests",
            json!({ "target": "lib", "dry_run": true })
        )
        .0,
        "F0012"
    );
    let mut builder = app(&root, &["read", "build"]);
    let dry = ok(
        &mut builder,
        "agent.run_tests",
        json!({ "target": "smoke", "filter": "door", "dry_run": true }),
    );
    let command: Vec<&str> = dry["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        &command[1..],
        [
            "test",
            "--color",
            "never",
            "--test",
            "smoke",
            "--offline",
            "--",
            "door"
        ]
    );
    assert_eq!(
        refusal(&mut builder, "agent.run_tests", json!({ "target": "--help" })).1,
        "run_tests_bad_name"
    );
    let empty = regia_test_utils::unique_temp_dir("run-tests-empty");
    let mut nowhere = app(&empty, &["read", "build"]);
    assert_eq!(
        refusal(&mut nowhere, "agent.run_tests", json!({ "target": "lib" })).1,
        "run_tests_no_manifest"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}

/// A real `cargo test` run of a tiny project, streamed through `agent.job_status`.
#[test]
fn run_tests_streams_a_real_cargo_run() {
    let root = regia_test_utils::unique_temp_dir("run-tests-real");
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"gaps_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("manifest");
    std::fs::write(
        root.join("src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(1 + 1, 2); }\n    \
         #[test]\n    fn breaks() { assert_eq!(1, 2, \"deliberate\"); }\n    \
         #[test]\n    #[ignore]\n    fn skipped() {}\n}\n",
    )
    .expect("lib");
    let mut builder = app(&root, &["read", "build", "play"]);
    builder
        .world_mut()
        .insert_resource(crate::run_tests::RunTestsEnv(vec![(
            "CARGO_TARGET_DIR".into(),
            root.join("target").display().to_string(),
        )]));
    let started = ok(&mut builder, "agent.run_tests", json!({ "target": "lib" }));
    let job = started["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("tests:"), "{job}");
    assert_eq!(
        refusal(&mut builder, "agent.run_tests", json!({ "target": "lib" })).1,
        "run_tests_busy"
    );
    let mut since = 0;
    let mut events = Vec::new();
    let mut last = Value::Null;
    // Bounded wait for the external process (compiling a one-file crate).
    for _ in 0..6000 {
        last = ok(
            &mut builder,
            "agent.job_status",
            json!({ "job": job, "since": since }),
        );
        events.extend(last["events"].as_array().cloned().unwrap_or_default());
        since = last["next"].as_u64().unwrap_or(since);
        if last["finished"] == true && last["more"] != true {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(last["state"], "done", "{last}");
    let result = &last["result"];
    assert_eq!(result["passed"], 1, "{result}");
    assert_eq!(result["failed"], 1, "{result}");
    assert_eq!(result["ignored"], 1, "{result}");
    assert_eq!(result["ok"], false);
    assert_eq!(result["failures"][0]["name"], "tests::breaks");
    assert!(
        result["failures"][0]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("deliberate"),
        "{result}"
    );
    let outcomes: Vec<(String, String)> = events
        .iter()
        .filter(|e| e["kind"] == "test")
        .map(|e| {
            (
                e["name"].as_str().unwrap_or_default().into(),
                e["outcome"].as_str().unwrap_or_default().into(),
            )
        })
        .collect();
    assert!(
        outcomes.contains(&("tests::adds".into(), "ok".into())),
        "{outcomes:?}"
    );
    assert!(
        outcomes.contains(&("tests::breaks".into(), "failed".into())),
        "{outcomes:?}"
    );
    let listed = ok(&mut builder, "agent.job_status", json!({}));
    assert!(
        listed["jobs"]
            .as_array()
            .is_some_and(|j| j.iter().any(|x| x["job"] == job.as_str()))
    );
    let cancel = ok(&mut builder, "agent.job_cancel", json!({ "job": job }));
    assert_eq!(cancel["requested"], false, "a finished job is not cancelled");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn job_ids_route_to_registry_or_provider() {
    let root = regia_test_utils::unique_temp_dir("jobs-route");
    let mut agent = app(&root, &["read", "play"]);
    assert_eq!(
        refusal(&mut agent, "agent.job_status", json!({ "job": "nope" })).1,
        "job_id_invalid"
    );
    assert_eq!(
        refusal(&mut agent, "agent.job_status", json!({ "job": "tests:4" })).1,
        "job_not_found"
    );
    // A provider id reaches the provider's own method, which answers for an unknown render-out.
    let response = call(&mut agent, "agent.job_status", json!({ "job": "render_out:41" }));
    assert!(
        response.error.is_some(),
        "no render-out ran: {:?}",
        response.result
    );
    assert_eq!(
        refusal(&mut agent, "agent.job_cancel", json!({ "job": "dcc_bake:abc" })).1,
        "job_not_cancellable"
    );
    // Spec 23 § 4 names the parameter `id`.
    assert_eq!(
        refusal(&mut agent, "agent.job_status", json!({ "id": "tests:4" })).1,
        "job_not_found"
    );
    // A forwarded call keeps the target method's permission: render.out_cancel needs
    // edit_scene, which this profile lacks.
    assert_eq!(
        refusal(&mut agent, "agent.job_cancel", json!({ "job": "render_out:3" })).0,
        "F0012"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A cancelled run ends even when the "cargo" leaves a child holding its output open: the run's
/// process group is killed, and the worker stops waiting for the pipes after a grace period.
#[cfg(target_os = "linux")]
#[test]
fn cancelling_a_run_ends_the_job_and_its_children() {
    let root = regia_test_utils::unique_temp_dir("run-tests-cancel");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    let pid_file = root.join("child.pid");
    let script = root.join("fake-cargo.sh");
    regia_test_utils::process::write_executable_script(
        &script,
        &format!(
            "#!/bin/sh\nsleep 600 &\necho $! > {}.tmp\nmv {}.tmp {}\necho 'running 1 test'\nwait\n",
            pid_file.display(),
            pid_file.display(),
            pid_file.display()
        ),
    )
    .expect("script");
    let mut builder = app(&root, &["read", "build", "play"]);
    builder
        .world_mut()
        .insert_resource(crate::run_tests::RunTestsProgram(script));
    let started = ok(&mut builder, "agent.run_tests", json!({ "filter": "door" }));
    let job = started["job"].as_str().expect("job id").to_string();
    // Bounded waits on the external process.
    let mut child_pid = None;
    for _ in 0..2000 {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            child_pid = text.trim().parse::<u32>().ok();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let child_pid = child_pid.expect("the fake cargo started its child");
    let cancel = ok(&mut builder, "agent.job_cancel", json!({ "id": job }));
    assert_eq!(cancel["requested"], true, "{cancel}");
    let mut last = Value::Null;
    for _ in 0..2000 {
        last = ok(&mut builder, "agent.job_status", json!({ "job": job }));
        if last["finished"] == true {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(last["state"], "cancelled", "{last}");
    let proc_dir = PathBuf::from(format!("/proc/{child_pid}"));
    let mut gone = false;
    for _ in 0..500 {
        // A killed child may linger as a zombie until its reaper runs; both count as stopped.
        let state = std::fs::read_to_string(proc_dir.join("stat")).unwrap_or_default();
        if state.is_empty() || state.contains(") Z ") {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone, "the run's child {child_pid} survived the cancel");
    // A new run is allowed again.
    let again = call(&mut builder, "agent.run_tests", json!({ "dry_run": true }));
    assert!(again.error.is_none(), "{:?}", again.error);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_in_editor_outside_the_editor_says_so() {
    let root = regia_test_utils::unique_temp_dir("open-runtime");
    let mut agent = app(&root, &["read", "play"]);
    assert_eq!(
        refusal(
            &mut agent,
            "agent.open_in_editor",
            json!({ "asset": "prefabs/door.prefab.ron" })
        )
        .1,
        "editor_not_running"
    );
    assert_eq!(
        refusal(&mut agent, "agent.open_in_editor", json!({ "file": "../x.rs" })).0,
        "F0002"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn no_tool_scope_can_hold_the_review_permission() {
    use crate::schema::Permission;
    let scope = crate::scope::ToolScope::new(
        "t",
        [Permission::Read, Permission::Review, Permission::Assets],
        vec!["review.*".into()],
    );
    assert!(!scope.allows_permission(Permission::Review));
    assert!(scope.allows_permission(Permission::Assets));
    assert_eq!(Permission::from_name("review"), Some(Permission::Review));
    assert!(
        !AgentConfig::default().permissions.contains("review"),
        "not in the default profile"
    );
}

#[test]
fn line_diff_hunks_are_unified_and_bounded() {
    let a = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
    let b = a.replace("line 5\n", "line five\n").replace("line 17\n", "");
    let diff = crate::review::line_diff(&a, &b);
    assert_eq!((diff.added, diff.removed), (1, 2));
    assert!(diff.hunks.contains("@@ -2,7 +2,7 @@\n"), "{}", diff.hunks);
    assert!(diff.hunks.contains("-line 5\n+line five\n"), "{}", diff.hunks);
    assert!(diff.hunks.contains("@@ -14,7 +14,6 @@\n"), "{}", diff.hunks);
    let created = crate::review::line_diff("", "x\ny\n");
    assert!(
        created.hunks.starts_with("@@ -0,0 +1,2 @@\n"),
        "{}",
        created.hunks
    );
    // A middle too large for the table is one replaced block.
    let big_a: String = (0..3000).map(|i| format!("a{i}\n")).collect();
    let big_b: String = (0..3000).map(|i| format!("b{i}\n")).collect();
    let big = crate::review::line_diff(&big_a, &big_b);
    assert_eq!((big.added, big.removed), (3000, 3000));
}

#[test]
fn the_review_queue_file_round_trips_and_refuses_newer_versions() {
    let fixture = crate::review::fuzz_fixture();
    let once = crate::review::fuzz_load(&fixture).expect("loads");
    assert_eq!(crate::review::fuzz_load(&once).expect("reloads"), once);
    let current = crate::review::REVIEW_QUEUE_FORMAT_VERSION;
    let newer = fixture.replace(
        &format!("\"format_version\": {current}"),
        &format!("\"format_version\": {}", current + 1),
    );
    assert_ne!(newer, fixture, "the fixture states its version");
    assert!(crate::review::fuzz_load(&newer).is_err());
    let mut rng = regia_core::fuzz::FuzzRng::new(0x2319_0003);
    for _ in 0..1500 {
        let mutated = regia_core::fuzz::mutate_text(&fixture, &mut rng);
        if let Ok(encoded) = crate::review::fuzz_load(&mutated) {
            assert_eq!(crate::review::fuzz_load(&encoded).as_ref(), Ok(&encoded));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Spec 23 § 3.6 (Code): `agent.build_game` and `agent.hot_reload` — the spec's code methods,
// shipped as jobs (`build:<n>`, `hot:<n>`) with structured, cargo-shaped results.

#[test]
fn build_game_needs_build_permission_and_names_its_command() {
    let root = regia_test_utils::unique_temp_dir("build-game-perm");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    let mut agent = app(&root, &["read", "edit_scene", "assets", "play"]);
    assert_eq!(
        refusal(&mut agent, "agent.build_game", json!({ "dry_run": true })).0,
        "F0012"
    );
    let mut builder = app(&root, &["read", "build"]);
    let dry = ok(
        &mut builder,
        "agent.build_game",
        json!({ "profile": "release", "package": "game", "dry_run": true }),
    );
    assert_eq!(dry["dry_run"], true);
    let command: Vec<&str> = dry["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        &command[1..],
        [
            "build",
            "--message-format=json",
            "--release",
            "--package",
            "game",
            "--offline"
        ]
    );
    assert_eq!(
        refusal(
            &mut builder,
            "agent.build_game",
            json!({ "profile": "--release" })
        )
        .1,
        "build_game_bad_name"
    );
    let online = ok(
        &mut builder,
        "agent.build_game",
        json!({ "profile": "release", "dry_run": true, "offline": false }),
    );
    assert_eq!(online["dry_run"], true, "offline false is accepted");
    let empty = regia_test_utils::unique_temp_dir("build-game-empty");
    let mut nowhere = app(&empty, &["read", "build"]);
    assert_eq!(
        refusal(&mut nowhere, "agent.build_game", json!({})).1,
        "build_game_no_manifest"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}

/// `agent.build_game` streams cargo's JSON diagnostics (file, line, message, suggestion) and
/// refuses a second build while one runs (spec 23 § 4: long operations answer a job id).
#[cfg(unix)]
#[test]
fn build_game_streams_diagnostics_and_refuses_while_a_build_runs() {
    let root = regia_test_utils::unique_temp_dir("build-game-stream");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    let ready = root.join("ready");
    let go = root.join("go");
    let script = root.join("fake-cargo.sh");
    regia_test_utils::process::write_executable_script(
        &script,
        &format!(
            "#!/bin/sh\necho ready > {}\nwhile [ ! -f {} ]; do sleep 0.05; done\n\
             printf '%s\\n' '{{\"reason\":\"compiler-message\",\"message\":{{\"message\":\"mismatched types\",\"level\":\"error\",\"code\":{{\"code\":\"E0308\"}},\"spans\":[{{\"file_name\":\"src/lib.rs\",\"line_start\":3,\"column_start\":9,\"is_primary\":true}}],\"children\":[{{\"message\":\"try this replacement\",\"spans\":[{{\"suggested_replacement\":\"1u32\"}}]}}]}}}}'\n\
             printf '%s\\n' '{{\"reason\":\"compiler-message\",\"message\":{{\"message\":\"unused variable\",\"level\":\"warning\",\"spans\":[{{\"file_name\":\"src/lib.rs\",\"line_start\":2,\"column_start\":5,\"is_primary\":true}}]}}}}'\n\
             printf '%s\\n' '{{\"reason\":\"build-finished\",\"success\":false}}'\nexit 1\n",
            ready.display(),
            go.display()
        ),
    )
    .expect("script");
    let mut builder = app(&root, &["read", "build"]);
    builder
        .world_mut()
        .insert_resource(crate::run_tests::RunTestsProgram(script));
    let started = ok(&mut builder, "agent.build_game", json!({}));
    let job = started["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("build:"), "{job}");
    // The fake cargo signals readiness, then waits for `go`: the build is deterministically
    // still running, so a second build is refused.
    for _ in 0..2000 {
        if ready.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.is_file(), "the fake cargo started");
    assert_eq!(
        refusal(&mut builder, "agent.build_game", json!({})).1,
        "build_game_busy"
    );
    std::fs::write(&go, "go").expect("go");
    let mut since = 0;
    let mut events = Vec::new();
    let mut last = Value::Null;
    for _ in 0..600 {
        last = ok(
            &mut builder,
            "agent.job_status",
            json!({ "job": job, "since": since }),
        );
        events.extend(last["events"].as_array().cloned().unwrap_or_default());
        since = last["next"].as_u64().unwrap_or(since);
        if last["finished"] == true && last["more"] != true {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(last["state"], "done", "{last}");
    let result = &last["result"];
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["exit_code"], 1, "{result}");
    assert_eq!(result["profile"], "debug", "{result}");
    let diagnostics = result["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 2, "{result}");
    assert_eq!(diagnostics[0]["level"], "error");
    assert_eq!(diagnostics[0]["message"], "mismatched types");
    assert_eq!(diagnostics[0]["code"], "E0308");
    assert_eq!(diagnostics[0]["file"], "src/lib.rs");
    assert_eq!(diagnostics[0]["line"], 3);
    assert_eq!(diagnostics[0]["column"], 9);
    assert_eq!(diagnostics[0]["suggestion"], "1u32");
    assert_eq!(diagnostics[1]["level"], "warning");
    assert!(diagnostics[1]["suggestion"].is_null(), "{result}");
    assert_eq!(result["error_count"], 1, "{result}");
    assert_eq!(result["warning_count"], 1, "{result}");
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"diagnostic"),
        "the diagnostics streamed as events: {kinds:?}"
    );
    // The build ended: a dry run is allowed again.
    let again = ok(&mut builder, "agent.build_game", json!({ "dry_run": true }));
    assert_eq!(again["dry_run"], true);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hot_reload_needs_the_editor_and_refuses_while_a_reload_runs() {
    let root = regia_test_utils::unique_temp_dir("hot-reload-runtime");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    let mut agent = app(&root, &["read", "build"]);
    // Outside the editor the method says so (spec 23 § 3.6: the editor rebuilds and hot-reloads).
    assert_eq!(
        refusal(&mut agent, "agent.hot_reload", json!({})).1,
        "editor_not_running"
    );
    assert_eq!(refusal(&mut agent, "agent.hot_reload", json!({})).0, "F0002");
    let mut denied = app(&root, &["read", "edit_scene", "assets", "play"]);
    assert_eq!(
        refusal(&mut denied, "agent.hot_reload", json!({ "dry_run": true })).0,
        "F0012"
    );

    // An editor host installs the hook. The hook stands in for the editor here: it starts a
    // `hot:` job whose worker waits on a gate file, like the editor's in-flight compile.
    let gate = root.join("reload-gate");
    #[derive(Resource)]
    struct Gate(PathBuf);
    fn editor_hook(world: &mut World, dry_run: bool) -> Result<Value, crate::protocol::RpcError> {
        if dry_run {
            return Ok(json!({ "dry_run": true, "would": "rebuild and swap the game dylib" }));
        }
        let gate = world
            .get_resource::<Gate>()
            .map(|g| g.0.clone())
            .expect("the test installs the gate path");
        let mut registry = world.get_resource_or_insert_with::<crate::jobs::JobRegistry>(Default::default);
        let (job, handle) = registry.start("hot", "fake compile", true);
        std::thread::Builder::new()
            .name("fake-hot-compile".into())
            .spawn(move || {
                while !gate.is_file() {
                    std::thread::sleep(Duration::from_millis(25));
                }
                handle.finish(json!({ "accepted": true, "phase": "running" }));
            })
            .expect("worker");
        Ok(json!({ "job": job }))
    }
    let mut editor = app(&root, &["read", "build"]);
    editor
        .world_mut()
        .insert_resource(crate::hot_reload::HotReloadHook(editor_hook));
    editor.world_mut().insert_resource(Gate(gate.clone()));
    let dry = ok(&mut editor, "agent.hot_reload", json!({ "dry_run": true }));
    assert_eq!(dry["dry_run"], true);
    let started = ok(&mut editor, "agent.hot_reload", json!({}));
    let job = started["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("hot:"), "{job}");
    // While the reload job runs, a second call is refused (spec 23 § 3.6).
    assert_eq!(
        refusal(&mut editor, "agent.hot_reload", json!({})).1,
        "hot_reload_busy"
    );
    // The gate lets the fake compile finish; the job then reports its structured result.
    std::fs::write(&gate, "go").expect("gate");
    let mut last = Value::Null;
    for _ in 0..600 {
        last = ok(&mut editor, "agent.job_status", json!({ "job": job }));
        if last["finished"] == true {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(last["state"], "done", "{last}");
    assert_eq!(last["result"]["accepted"], true, "{last}");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Spec 23 § 3.7 (pipeline): `agent.cook` and `agent.package` as jobs (ADR-7217), over the regia
// command line's one-object `--json` output.

/// Polls `job` until it finished; answers the final status and every streamed event.
#[cfg(unix)]
fn finish_job(app: &mut App, job: &str) -> (Value, Vec<Value>) {
    let mut since = 0;
    let mut events = Vec::new();
    let mut last = Value::Null;
    for _ in 0..1500 {
        last = ok(app, "agent.job_status", json!({ "job": job, "since": since }));
        events.extend(last["events"].as_array().cloned().unwrap_or_default());
        since = last["next"].as_u64().unwrap_or(since);
        if last["finished"] == true && last["more"] != true {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (last, events)
}

#[test]
fn cook_and_package_need_build_permission_and_refuse_bad_values() {
    let root = regia_test_utils::unique_temp_dir("agent-cook-perm");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"x\"\n").expect("manifest");
    let mut player = app(&root, &["read", "edit_scene", "assets", "play"]);
    assert_eq!(
        refusal(&mut player, "agent.cook", json!({ "dry_run": true })).0,
        "F0012"
    );
    assert_eq!(
        refusal(&mut player, "agent.package", json!({ "dry_run": true })).0,
        "F0012"
    );
    let mut builder = app(&root, &["read", "build"]);
    let dry = ok(
        &mut builder,
        "agent.cook",
        json!({ "platform": "android", "quality": "fast", "dry_run": true }),
    );
    let command: Vec<&str> = dry["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        &command[1..],
        ["cook", "--platform", "android", "--quality", "fast", "--json"]
    );
    let dry = ok(
        &mut builder,
        "agent.package",
        json!({ "platform": "windows", "profile": "dev", "dry_run": true }),
    );
    let command: Vec<&str> = dry["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        &command[1..],
        ["package", "--platform", "windows", "--profile", "dev", "--json"]
    );
    // The documented reasons for bad values.
    assert_eq!(
        refusal(&mut builder, "agent.cook", json!({ "platform": "playstation" })),
        ("F0002".to_string(), "cook_bad_platform".to_string())
    );
    assert_eq!(
        refusal(
            &mut builder,
            "agent.package",
            json!({ "platform": "ps5", "confirm": true })
        )
        .1,
        "cook_bad_platform"
    );
    assert_eq!(
        refusal(&mut builder, "agent.cook", json!({ "quality": "best" })).1,
        "cook_bad_quality"
    );
    assert_eq!(
        refusal(&mut builder, "agent.package", json!({ "profile": "--release" })).1,
        "package_bad_profile"
    );
    assert_eq!(
        refusal(&mut builder, "agent.package", json!({ "platform": "linux" })).1,
        "needs_confirmation"
    );
    let empty = regia_test_utils::unique_temp_dir("agent-cook-empty");
    let mut nowhere = app(&empty, &["read", "build"]);
    assert_eq!(
        refusal(&mut nowhere, "agent.cook", json!({})).1,
        "cook_no_manifest"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}

/// `agent.cook` and `agent.package` report the command's progress lines and end with the
/// `--json` object's result (the artifact path); a second pipeline job while one runs is
/// refused. A fake `regia` script stands in for the command line.
#[cfg(unix)]
#[test]
fn cook_and_package_jobs_report_progress_and_a_final_result() {
    use std::os::unix::fs::PermissionsExt;
    let root = regia_test_utils::unique_temp_dir("agent-cook-job");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"x\"\n").expect("manifest");
    let ready = root.join("ready");
    let go = root.join("go");
    let script = root.join("fake-regia.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho ready > {ready}\nwhile [ ! -f {go} ]; do sleep 0.05; done\n\
             case \"$1\" in\n\
             cook)\n  echo \"cook: importing assets/props/crate.glb\" >&2\n  echo \"cook: 1 asset cooked\" >&2\n\
             \x20 echo '{{\"ok\":true,\"platform\":\"'$3'\",\"quality\":\"'$5'\",\"pak_dir\":\"build/paks/'$3'\",\"paks\":[\"build/paks/'$3'/base.fpak\"]}}'\n  exit 0;;\n\
             package)\n  echo \"package: building the launcher\" >&2\n\
             \x20 echo '{{\"ok\":true,\"platform\":\"'$3'\",\"profile\":\"'$5'\",\"artifact\":\"dist/'$3'/game.tar.gz\"}}'\n  exit 0;;\n\
             esac\nexit 2\n",
            ready = ready.display(),
            go = go.display()
        ),
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut builder = app(&root, &["read", "build"]);
    builder
        .world_mut()
        .insert_resource(crate::cook_package::RegiaCliProgram(script));
    let started = ok(
        &mut builder,
        "agent.cook",
        json!({ "platform": "linux", "quality": "fast" }),
    );
    let job = started["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("cook:"), "{job}");
    for _ in 0..2000 {
        if ready.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.is_file(), "the fake regia started");
    assert_eq!(
        refusal(&mut builder, "agent.package", json!({ "confirm": true })).1,
        "cook_busy",
        "one pipeline job at a time"
    );
    std::fs::write(&go, "go").expect("go");
    let (last, events) = finish_job(&mut builder, &job);
    assert_eq!(last["state"], "done", "{last}");
    let result = &last["result"];
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["artifact"], "build/paks/linux", "{result}");
    assert_eq!(result["report"]["quality"], "fast", "{result}");
    assert_eq!(result["lines"], 2, "{result}");
    let texts: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "line")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"cook: importing assets/props/crate.glb"),
        "{events:?}"
    );
    assert!(events.iter().any(|e| e["kind"] == "report"), "{events:?}");

    // The package job (the `go` file stays, so it runs straight through).
    let started = ok(
        &mut builder,
        "agent.package",
        json!({ "platform": "linux", "profile": "shipping", "confirm": true }),
    );
    let job = started["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("package:"), "{job}");
    let (last, events) = finish_job(&mut builder, &job);
    assert_eq!(last["state"], "done", "{last}");
    assert_eq!(last["result"]["ok"], true, "{last}");
    assert_eq!(last["result"]["artifact"], "dist/linux/game.tar.gz", "{last}");
    assert_eq!(last["result"]["report"]["profile"], "shipping", "{last}");
    assert!(
        events
            .iter()
            .any(|e| e["text"] == "package: building the launcher"),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `seeing.screenshot` refuses the editor-only overlays with a named reason through the
/// dispatcher, before any renderer is needed (ADR-7217).
#[test]
fn screenshot_names_the_editor_only_overlays() {
    let root = regia_test_utils::unique_temp_dir("agent-seeing-overlays");
    let mut agent = app(&root, &["read"]);
    assert_eq!(
        refusal(&mut agent, "seeing.screenshot", json!({ "overlays": ["labels"] })),
        ("F0013".to_string(), "overlay_needs_editor".to_string())
    );
    assert_eq!(
        refusal(
            &mut agent,
            "seeing.screenshot",
            json!({ "target": "camera", "name": "Nope" })
        )
        .0,
        "F0003"
    );
    let _ = std::fs::remove_dir_all(&root);
}
