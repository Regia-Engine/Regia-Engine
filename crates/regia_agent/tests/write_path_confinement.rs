// SPDX-License-Identifier: MPL-2.0
//! Headless agent writes must use `AgentSettings::project_dir` when no asset server is
//! installed (ADR-7871, `book/agents/safety.md`). The working directory is the project root
//! only when neither an asset server nor `project_dir` is set.
//!
//! These tests chdir into a private temp directory so a writer that still joins `.` cannot
//! leave a file in the source tree. The directory is restored and both temps removed on drop.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use regia_agent::protocol::RpcError;
use regia_agent::{AgentSettings, LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

/// One process-wide working directory. Each test binary is its own process; this lock keeps
/// the tests in this file from changing it at the same time.
static CWD_LOCK: Mutex<()> = Mutex::new(());

struct Sandbox {
    project: PathBuf,
    cwd: PathBuf,
    prev: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn enter(tag: &str) -> Self {
        let lock = CWD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let project = regia_test_utils::unique_temp_dir(&format!("agent-write-project-{tag}"));
        let cwd = regia_test_utils::unique_temp_dir(&format!("agent-write-cwd-{tag}"));
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&cwd).expect("chdir into the private working directory");
        Self {
            project,
            cwd,
            prev,
            _lock: lock,
        }
    }

    fn world(&self) -> World {
        let mut world = World::new();
        world.insert_resource(AgentSettings {
            project_dir: Some(self.project.clone()),
        });
        // A successful EditScene call records provenance and reads the frame count.
        world.insert_resource(Time::default());
        world
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
        let _ = fs::remove_dir_all(&self.project);
        let _ = fs::remove_dir_all(&self.cwd);
    }
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(world, method, params, None) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} deferred; these methods answer immediately"),
    }
}

/// Files and symlinks under `dir`. A directory symlink is recorded and not walked, so a link
/// that leaves the sandbox cannot pull the listing out with it.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                out.push(path);
            } else if kind.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn name_has(path: &Path, marker: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(marker))
}

fn assert_wrote_inside_project(sandbox: &Sandbox, reply: &Result<Value, RpcError>, marker: &str) {
    let cwd_files = files_under(&sandbox.cwd);
    let project_hit = files_under(&sandbox.project)
        .into_iter()
        .any(|path| name_has(&path, marker));
    assert!(
        cwd_files.is_empty(),
        "{marker}: wrote under the process working directory ({}) while AgentSettings::project_dir is {}: {cwd_files:?}\nreply: {reply:?}",
        sandbox.cwd.display(),
        sandbox.project.display(),
    );
    assert!(
        project_hit,
        "{marker}: nothing was written under the configured project ({}); reply: {reply:?}",
        sandbox.project.display(),
    );
}

const LOOP_RON: &str = r#"(version: 1, name: "wind", length_secs: 0.5, seed: 3,
        nodes: [(id: "n", node: Noise(color: Pink, gain: 0.8)),
                (id: "lp", node: Filter(input: "n", mode: LowPass, cutoff_hz: 400.0))],
        output: "lp")"#;

#[test]
fn ai_bt_edit_keeps_a_new_tree_inside_the_agent_project() {
    let sandbox = Sandbox::enter("bt");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "ai.bt_edit",
        json!({
            "path": "ai/guard.bt.ron",
            "create": true,
            "save": true,
            "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
        }),
    );
    assert_wrote_inside_project(&sandbox, &reply, "guard.bt.ron");
}

#[test]
fn ai_statetree_edit_keeps_a_new_tree_inside_the_agent_project() {
    let sandbox = Sandbox::enter("statetree");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "ai.statetree_edit",
        json!({
            "path": "ai/guard.statetree.ron",
            "create": true,
            "save": true,
            "ops": [
                { "op": "add_state", "name": "Root" },
                { "op": "add_state", "name": "Idle", "parent": "Root" },
                { "op": "add_state", "name": "Alert", "parent": "Root" },
                { "op": "add_transition", "state": "Idle", "trigger": { "Event": "heard" }, "target": { "State": "Alert" } },
                { "op": "add_transition", "state": "Alert", "trigger": "Tick", "priority": 1, "hold": 0.5, "target": { "State": "Idle" } },
                { "op": "set_param", "state": "Alert", "key": "speed", "value": { "Float": 2.5 } },
            ],
        }),
    );
    assert_wrote_inside_project(&sandbox, &reply, "guard.statetree.ron");
}

#[test]
fn audio_loopgen_render_joins_a_relative_out_dir_to_the_agent_project() {
    let sandbox = Sandbox::enter("loopgen");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "ron": LOOP_RON, "wav": true, "out_dir": "wavs" }),
    );
    assert_wrote_inside_project(&sandbox, &reply, ".wav");
}

#[test]
fn anim_montage_slot_edit_keeps_a_new_table_inside_the_agent_project() {
    let sandbox = Sandbox::enter("slots");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "hero.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    assert_wrote_inside_project(&sandbox, &reply, "hero.slots.ron");
}

/// Lexical escapes are refused before a write. These names never name a file under the
/// configured project or the working directory.
#[test]
fn ai_bt_edit_refuses_paths_that_are_not_project_relative() {
    let sandbox = Sandbox::enter("lexical");
    let mut world = sandbox.world();
    let outside = regia_test_utils::unique_temp_dir("agent-write-lexical-outside");
    let absolute = outside.join("guard.bt.ron");
    let samples = [
        "../guard.bt.ron",
        absolute.to_str().expect("utf8 temp"),
        "C:guard.bt.ron",
        r"\\server\share\guard.bt.ron",
        "",
        "CON.bt.ron",
        "ai/guard.bt.ron ",
        "ai/guard.bt.ron.",
        "ai/nul.bt.ron",
    ];
    for path in samples {
        let reply = call(
            &mut world,
            "ai.bt_edit",
            json!({
                "path": path,
                "create": true,
                "save": true,
                "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
            }),
        );
        assert!(reply.is_err(), "accepted {path:?}: {reply:?}");
        assert!(
            files_under(&sandbox.cwd).is_empty() && files_under(&sandbox.project).is_empty(),
            "{path:?} wrote a file; cwd {:?} project {:?}",
            files_under(&sandbox.cwd),
            files_under(&sandbox.project),
        );
        assert!(!absolute.exists(), "{path:?} created {}", absolute.display());
    }
    let _ = fs::remove_dir_all(&outside);
}

/// A symlink planted inside the configured project must not receive the new tree, and the
/// write must not fall back to the working directory either.
#[cfg(unix)]
#[test]
fn ai_bt_edit_does_not_follow_a_symlink_out_of_the_agent_project() {
    let sandbox = Sandbox::enter("symlink");
    let outside = regia_test_utils::unique_temp_dir("agent-write-symlink-out");
    std::os::unix::fs::symlink(&outside, sandbox.project.join("escape")).expect("symlink");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "ai.bt_edit",
        json!({
            "path": "escape/guard.bt.ron",
            "create": true,
            "save": true,
            "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
        }),
    );
    let outside_files = files_under(&outside);
    let cwd_files = files_under(&sandbox.cwd);
    assert!(
        outside_files.is_empty(),
        "followed the project symlink and wrote {outside_files:?}; reply {reply:?}"
    );
    assert!(
        cwd_files.is_empty(),
        "wrote under the working directory instead of refusing the symlink: {cwd_files:?}; reply {reply:?}"
    );
    assert!(
        reply.is_err(),
        "accepted a symlink that leaves the project: {reply:?}"
    );
    let _ = fs::remove_dir_all(&outside);
}

/// ADR-8798: project-relative reads use the same root. A file that exists only in the working
/// directory is not found; the same name under `AgentSettings::project_dir` is read.
#[test]
fn reads_resolve_against_the_agent_project_not_the_working_directory() {
    const TREE: &str =
        r#"(format_version: 2, root: "Root", states: [(name: "Root", children: ["Idle"]), (name: "Idle")])"#;
    let sandbox = Sandbox::enter("reads");
    let mut world = sandbox.world();
    // Only in the working directory: refused as not found, never read from `.`.
    fs::write(sandbox.cwd.join("cwd_only.loopgen.ron"), LOOP_RON).expect("cwd loop");
    fs::write(sandbox.cwd.join("cwd_only.statetree.ron"), TREE).expect("cwd tree");
    let loop_reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "path": "cwd_only.loopgen.ron" }),
    );
    assert!(loop_reply.is_err(), "read the working directory: {loop_reply:?}");
    let tree_reply = call(
        &mut world,
        "statetree.compile",
        json!({ "path": "cwd_only.statetree.ron" }),
    );
    assert!(tree_reply.is_err(), "read the working directory: {tree_reply:?}");
    // Under the project: read.
    fs::write(sandbox.project.join("wind.loopgen.ron"), LOOP_RON).expect("project loop");
    let loop_reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "path": "wind.loopgen.ron" }),
    );
    assert!(
        loop_reply.is_ok(),
        "did not read the project file: {loop_reply:?}"
    );
    fs::write(sandbox.project.join("guard.statetree.ron"), TREE).expect("project tree");
    let tree_reply = call(
        &mut world,
        "statetree.compile",
        json!({ "path": "guard.statetree.ron" }),
    );
    assert_eq!(
        tree_reply.as_ref().ok().and_then(|v| v["ok"].as_bool()),
        Some(true),
        "did not read and compile the project file: {tree_reply:?}"
    );
}

/// ADR-8798: `ai.statetree_edit` and `anim.montage_slot_edit` refuse a symlink in the project that
/// leads out, and a dangling one, instead of writing through or beside it.
#[cfg(unix)]
#[test]
fn statetree_and_slot_edits_refuse_a_symlink_out_of_the_agent_project() {
    let sandbox = Sandbox::enter("symlink-edits");
    let outside = regia_test_utils::unique_temp_dir("agent-write-symlink-edits-out");
    std::os::unix::fs::symlink(&outside, sandbox.project.join("escape")).expect("dir link");
    std::os::unix::fs::symlink(
        outside.join("gone.statetree.ron"),
        sandbox.project.join("dangling.statetree.ron"),
    )
    .expect("dangling link");
    let mut world = sandbox.world();
    let ops = json!([
        { "op": "add_state", "name": "Root" },
        { "op": "add_state", "name": "Idle", "parent": "Root" },
    ]);
    for path in [
        "escape/guard.statetree.ron",
        "escape/new/guard.statetree.ron",
        "dangling.statetree.ron",
    ] {
        let reply = call(
            &mut world,
            "ai.statetree_edit",
            json!({ "path": path, "create": true, "save": true, "ops": ops }),
        );
        assert!(reply.is_err(), "{path}: accepted a path leading out: {reply:?}");
    }
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "escape/hero.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    assert!(reply.is_err(), "slot table through a symlink: {reply:?}");
    let outside_files = files_under(&outside);
    assert!(outside_files.is_empty(), "wrote outside: {outside_files:?}");
    let cwd_files = files_under(&sandbox.cwd);
    assert!(
        cwd_files.is_empty(),
        "wrote under the working directory: {cwd_files:?}"
    );
    let _ = fs::remove_dir_all(&outside);
}

fn assert_no_write(sandbox: &Sandbox, outside: &Path, what: &str, reply: &Result<Value, RpcError>) {
    let cwd_files = files_under(&sandbox.cwd);
    let outside_files = files_under(outside);
    assert!(
        reply.is_err(),
        "{what} was accepted: {reply:?}; cwd {cwd_files:?}; outside {outside_files:?}"
    );
    assert!(
        cwd_files.is_empty(),
        "{what} wrote under the working directory: {cwd_files:?}; reply {reply:?}"
    );
    assert!(
        outside_files.is_empty(),
        "{what} wrote outside the project: {outside_files:?}; reply {reply:?}"
    );
}

/// The review's lexical forms, on every writer the fix retargeted. `ai.bt_edit` already
/// covers them; a sibling that still accepts one would write under the working directory
/// or the absolute target.
#[test]
fn every_fixed_writer_refuses_the_review_path_forms() {
    let sandbox = Sandbox::enter("forms");
    let outside = regia_test_utils::unique_temp_dir("agent-write-forms-out");
    let absolute = outside.join("guard.bt.ron");
    let bt_forms = [
        "../guard.bt.ron",
        absolute.to_str().expect("utf8 temp"),
        "C:guard.bt.ron",
        r"\\server\share\guard.bt.ron",
        "",
        "CON.bt.ron",
        "ai/guard.bt.ron ",
        "ai/guard.bt.ron.",
        "ai/nul.bt.ron",
    ];
    let mut world = sandbox.world();
    for path in bt_forms {
        let reply = call(
            &mut world,
            "ai.bt_edit",
            json!({
                "path": path,
                "create": true,
                "save": true,
                "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
            }),
        );
        assert_no_write(&sandbox, &outside, path, &reply);
    }
    let tree_absolute = outside.join("guard.statetree.ron");
    let tree_forms = [
        "../guard.statetree.ron",
        tree_absolute.to_str().expect("utf8"),
        "C:guard.statetree.ron",
        r"\\server\share\guard.statetree.ron",
        "",
        "CON.statetree.ron",
        "ai/guard.statetree.ron ",
        "ai/guard.statetree.ron.",
        "ai/nul.statetree.ron",
    ];
    let ops = json!([
        { "op": "add_state", "name": "Root" },
        { "op": "add_state", "name": "Idle", "parent": "Root" },
    ]);
    for path in tree_forms {
        let path = path.to_string();
        let reply = call(
            &mut world,
            "ai.statetree_edit",
            json!({ "path": path, "create": true, "save": true, "ops": ops }),
        );
        assert_no_write(&sandbox, &outside, &path, &reply);
    }
    let slot_absolute = outside.join("hero.slots.ron");
    let slot_forms = [
        "../hero.slots.ron",
        slot_absolute.to_str().expect("utf8"),
        "C:hero.slots.ron",
        r"\\server\share\hero.slots.ron",
        "",
        "CON.slots.ron",
        "hero.slots.ron ",
        "hero.slots.ron.",
        "nul.slots.ron",
    ];
    for path in slot_forms {
        let path = path.to_string();
        let reply = call(
            &mut world,
            "anim.montage_slot_edit",
            json!({
                "path": path,
                "op": { "op": "add", "name": "upper" },
                "montages": [],
                "graphs": [],
            }),
        );
        assert_no_write(&sandbox, &outside, &path, &reply);
    }
    // `out_dir` uses the output-root rule. An absolute path under the system temp directory
    // is allowed, so the refused absolute path is outside temp, the project and `output_roots`.
    let forbidden = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(format!("regia-v061-not-a-root-{}", std::process::id()));
    let _ = fs::remove_dir_all(&forbidden);
    let wav_forms = [
        "../wavs",
        "wavs/../../outside",
        forbidden.to_str().expect("utf8"),
        "C:wavs",
        r"\\server\share\wavs",
        "",
        "CON",
        "wavs ",
        "wavs.",
        "nul",
    ];
    for dir in wav_forms {
        let reply = call(
            &mut world,
            "audio.loopgen_render",
            json!({ "ron": LOOP_RON, "wav": true, "out_dir": dir }),
        );
        assert_no_write(&sandbox, &outside, dir, &reply);
        assert!(!forbidden.exists(), "{dir} created {}", forbidden.display());
    }
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&forbidden);
}

/// A directory that is not the project, the system temp dir, or an `output_roots` entry.
/// `audio.loopgen_render` may write under the system temp dir, and `unique_temp_dir` lives
/// there, so a symlink proof for `out_dir` has to point somewhere else.
#[cfg(unix)]
fn dir_outside_allowed_roots(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.components().count() > 1);
    let base = home.unwrap_or_else(|| PathBuf::from("/var/tmp"));
    let dir = base.join(format!("regia-v061-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir outside allowed roots");
    dir
}

/// The review's symlink case on the writers whose confine walk the fix did not replace
/// with `check_under_root` (`anim.montage_slot_edit`) and on `audio.loopgen_render`'s
/// `out_dir`. A directory link, a new folder under it, a dangling file link, and a link
/// to a file that already exists outside must not receive bytes.
#[cfg(unix)]
#[test]
fn symlink_forms_do_not_receive_bytes_outside_the_project() {
    let sandbox = Sandbox::enter("symlink-forms");
    let outside = regia_test_utils::unique_temp_dir("agent-write-symlink-forms-out");
    std::os::unix::fs::symlink(&outside, sandbox.project.join("escape")).expect("dir link");
    std::os::unix::fs::symlink(
        outside.join("gone.bt.ron"),
        sandbox.project.join("dangling.bt.ron"),
    )
    .expect("dangling bt");
    // `out_dir` may resolve into the system temp dir. Point the link at a directory that
    // is not an allowed root, or the output-root rule accepts the write.
    let wav_outside = dir_outside_allowed_roots("loopgen-wavs");
    std::os::unix::fs::symlink(&wav_outside, sandbox.project.join("wavs")).expect("wav dir link");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "ai.bt_edit",
        json!({
            "path": "escape/new/guard.bt.ron",
            "create": true,
            "save": true,
            "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
        }),
    );
    assert_no_write(&sandbox, &outside, "bt nested symlink", &reply);
    let reply = call(
        &mut world,
        "ai.bt_edit",
        json!({
            "path": "dangling.bt.ron",
            "create": true,
            "save": true,
            "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
        }),
    );
    assert_no_write(&sandbox, &outside, "bt dangling", &reply);
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "escape/new/hero.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    assert_no_write(&sandbox, &outside, "slots nested symlink", &reply);
    // A link to a file that already exists outside, then a dangling slot path. Bytes must
    // not change the outside file and must not create the dangling link's target (rename
    // replaces a dangling symlink; it must not follow it).
    let existing = outside.join("existing.slots.ron");
    fs::write(&existing, b"keep").expect("existing");
    std::os::unix::fs::symlink(&existing, sandbox.project.join("hero.slots.ron")).expect("file link");
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "hero.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    assert!(reply.is_err(), "slot edit followed a file symlink: {reply:?}");
    assert_eq!(fs::read(&existing).expect("existing"), b"keep");
    let dangling_slots = sandbox.project.join("dangling.slots.ron");
    std::os::unix::fs::symlink(outside.join("dangling-target.slots.ron"), &dangling_slots)
        .expect("dangling slots");
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "dangling.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    let outside_files = files_under(&outside);
    assert_eq!(
        outside_files,
        vec![existing.clone()],
        "dangling slot path wrote outside: {outside_files:?}; reply {reply:?}"
    );
    assert_eq!(fs::read(&existing).expect("existing"), b"keep");
    assert!(
        files_under(&sandbox.cwd).is_empty(),
        "dangling slot path wrote under the working directory: {:?}",
        files_under(&sandbox.cwd)
    );
    let reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "ron": LOOP_RON, "wav": true, "out_dir": "wavs" }),
    );
    assert_no_write(&sandbox, &wav_outside, "loopgen symlink out_dir", &reply);
    let reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "ron": LOOP_RON, "wav": true, "out_dir": "wavs/nested" }),
    );
    assert_no_write(&sandbox, &wav_outside, "loopgen nested symlink out_dir", &reply);
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&wav_outside);
}

const LOGIC_GRAPH: &str = r#"(format_version: 1,
    locals: [(name: "n", default: Int(0))],
    rules: [
      (name: "count", event: EntityEvent("ping"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) }),
      ]),
    ])"#;

/// Reads the fix retargeted (`logic.compile`, state-tree includes, `fuzz.run`) follow
/// `project_dir`. A file that exists only in the working directory is not the one read.
#[test]
fn retargeted_reads_ignore_a_file_that_exists_only_in_the_working_directory() {
    const TREE: &str =
        r#"(format_version: 2, root: "Root", states: [(name: "Root", children: ["Idle"]), (name: "Idle")])"#;
    let sandbox = Sandbox::enter("more-reads");
    let mut world = sandbox.world();
    fs::write(sandbox.cwd.join("count.logic.ron"), LOGIC_GRAPH).expect("cwd logic");
    fs::write(sandbox.cwd.join("secret.statetree.ron"), TREE).expect("cwd include");
    fs::write(sandbox.cwd.join("fixture.bin"), b"from-cwd").expect("cwd fixture");
    let logic = call(&mut world, "logic.compile", json!({ "path": "count.logic.ron" }));
    assert!(
        logic.is_err(),
        "logic.compile read the working directory: {logic:?}"
    );
    let include = call(
        &mut world,
        "statetree.compile",
        json!({
            "text": "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"secret.statetree.ron\")], states: [(name: \"R\")])",
        }),
    );
    let include_reason = include
        .as_ref()
        .ok()
        .and_then(|v| v["errors"][0]["reason"].as_str())
        .unwrap_or("");
    assert_eq!(
        include_reason, "include_missing",
        "statetree.compile read the include from the working directory: {include:?}"
    );
    let targets = call(&mut world, "fuzz.targets", json!({})).expect("fuzz.targets");
    let target = targets["targets"][0]["name"]
        .as_str()
        .expect("a target name")
        .to_string();
    let fuzz = call(
        &mut world,
        "fuzz.run",
        json!({ "target": target, "path": "fixture.bin", "iterations": 1 }),
    );
    assert!(
        fuzz.is_err(),
        "fuzz.run read the working-directory fixture: {fuzz:?}"
    );
    // `project_file_in` reports the OS error (`No such file or directory`) once the path
    // resolves and the file is absent. `read_fixture`'s own text is `cannot be read`.
    let fuzz_msg = fuzz.as_ref().err().map(|e| e.message.clone()).unwrap_or_default();
    assert!(
        fuzz_msg.contains("cannot be read") || fuzz_msg.contains("No such file"),
        "fuzz.run did not treat the cwd-only fixture as missing: {fuzz:?}"
    );
    assert!(
        !fuzz_msg.contains("from-cwd"),
        "fuzz.run returned the working-directory bytes: {fuzz:?}"
    );
    fs::write(sandbox.project.join("count.logic.ron"), LOGIC_GRAPH).expect("project logic");
    let logic = call(&mut world, "logic.compile", json!({ "path": "count.logic.ron" }));
    assert_eq!(
        logic.as_ref().ok().and_then(|v| v["ok"].as_bool()),
        Some(true),
        "logic.compile did not read the project file: {logic:?}"
    );
    fs::write(sandbox.project.join("secret.statetree.ron"), TREE).expect("project include");
    let include = call(
        &mut world,
        "statetree.compile",
        json!({
            "text": "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"secret.statetree.ron\")], states: [(name: \"R\")])",
        }),
    );
    let include_reason = include
        .as_ref()
        .ok()
        .and_then(|v| v["errors"][0]["reason"].as_str())
        .unwrap_or("");
    assert_ne!(
        include_reason, "include_missing",
        "statetree.compile did not read the include from the project: {include:?}"
    );
    fs::write(sandbox.project.join("fixture.bin"), b"from-project").expect("project fixture");
    let fuzz = call(
        &mut world,
        "fuzz.run",
        json!({ "target": target, "path": "fixture.bin", "iterations": 1 }),
    );
    let fuzz_msg = fuzz.as_ref().err().map(|e| e.message.clone()).unwrap_or_default();
    assert!(
        fuzz.is_ok() || (!fuzz_msg.contains("cannot be read") && !fuzz_msg.contains("No such file")),
        "fuzz.run did not open the project fixture: {fuzz:?}"
    );
}

/// `audio.loopgen_render` unlinks a planted temp name and opens it with `create_new`
/// (ADR-3206). A symlink at that name must not receive the WAV.
#[cfg(unix)]
#[test]
fn audio_loopgen_render_does_not_follow_a_planted_temp_symlink() {
    let sandbox = Sandbox::enter("loopgen-temp");
    let outside = regia_test_utils::unique_temp_dir("agent-write-loopgen-temp-out");
    let victim = outside.join("pwned.wav");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "ron": LOOP_RON, "wav": true, "out_dir": "wavs" }),
    );
    let wav = reply
        .as_ref()
        .ok()
        .and_then(|v| v["wav"].as_str())
        .expect("wav path");
    let name = Path::new(wav).file_name().expect("wav file name");
    fs::remove_file(wav).expect("remove the first wav");
    let tmp =
        sandbox
            .project
            .join("wavs")
            .join(format!("{}.{}.tmp", name.to_string_lossy(), std::process::id()));
    std::os::unix::fs::symlink(&victim, &tmp).expect("temp link");
    let reply = call(
        &mut world,
        "audio.loopgen_render",
        json!({ "ron": LOOP_RON, "wav": true, "out_dir": "wavs" }),
    );
    assert!(!victim.exists(), "loopgen followed the temp symlink: {reply:?}");
    assert!(
        reply.is_ok(),
        "loopgen did not write after the temp link was planted: {reply:?}"
    );
    let _ = fs::remove_dir_all(&outside);
}

/// ADR-3206: `std::fs::write` follows a symlink. `ai.bt_edit`, `ai.statetree_edit` and
/// `anim.montage_slot_edit` still write a predictable temp name beside the target
/// (`*.ron.new`, `.{name}.{pid}.tmp`) with `std::fs::write`. A symlink planted at that
/// name, which a cloned project can carry, receives the bytes outside the project.
/// `audio.loopgen_render` unlinks the temp and uses `create_new`; that case is the test
/// above and must stay green.
#[cfg(unix)]
#[test]
fn planted_temp_symlinks_are_not_write_targets() {
    let sandbox = Sandbox::enter("temp-link");
    let outside = regia_test_utils::unique_temp_dir("agent-write-temp-link-out");
    let mut leaked: Vec<String> = Vec::new();

    let victim = outside.join("bt.ron");
    fs::create_dir_all(sandbox.project.join("ai")).expect("ai");
    std::os::unix::fs::symlink(&victim, sandbox.project.join("ai/guard.bt.ron.new")).expect("bt link");
    let mut world = sandbox.world();
    let reply = call(
        &mut world,
        "ai.bt_edit",
        json!({
            "path": "ai/guard.bt.ron",
            "create": true,
            "save": true,
            "ops": [{ "op": "add_node", "kind": "selector", "name": "root" }],
        }),
    );
    if victim.exists() {
        leaked.push(format!(
            "ai.bt_edit wrote {} via ai/guard.bt.ron.new ({reply:?})",
            victim.display()
        ));
    }

    let victim = outside.join("tree.ron");
    std::os::unix::fs::symlink(&victim, sandbox.project.join("ai/guard.statetree.ron.new"))
        .expect("tree link");
    let reply = call(
        &mut world,
        "ai.statetree_edit",
        json!({
            "path": "ai/guard.statetree.ron",
            "create": true,
            "save": true,
            "ops": [
                { "op": "add_state", "name": "Root" },
                { "op": "add_state", "name": "Idle", "parent": "Root" },
            ],
        }),
    );
    if victim.exists() {
        leaked.push(format!(
            "ai.statetree_edit wrote {} via ai/guard.statetree.ron.new ({reply:?})",
            victim.display()
        ));
    }

    let victim = outside.join("slots.ron");
    let tmp_name = format!(".hero.slots.ron.{}.tmp", std::process::id());
    std::os::unix::fs::symlink(&victim, sandbox.project.join(&tmp_name)).expect("slot link");
    let reply = call(
        &mut world,
        "anim.montage_slot_edit",
        json!({
            "path": "hero.slots.ron",
            "op": { "op": "add", "name": "upper" },
            "montages": [],
            "graphs": [],
        }),
    );
    if victim.exists() {
        leaked.push(format!(
            "anim.montage_slot_edit wrote {} via {tmp_name} ({reply:?})",
            victim.display()
        ));
    }

    assert!(
        leaked.is_empty(),
        "a planted temp symlink received the write:\n{}",
        leaked.join("\n")
    );
    let _ = fs::remove_dir_all(&outside);
}
