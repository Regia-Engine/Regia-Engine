// SPDX-License-Identifier: MPL-2.0
//! Revision control: the porcelain parser, the git driver on temp repositories, the status
//! cache's poll counting, the agent methods (direct and over the dispatcher) and the panel's
//! keyboard path (ADR-7837).

use std::path::Path;
use std::process::Command;

use regia_agent::protocol::RpcError;
use regia_ecs::prelude::World;
use regia_test_utils::TempDir;
use serde_json::{Value, json};

use super::*;

/// Runs git in `dir` (the test's own setup steps; the code under test uses the driver).
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs (the build hosts have git)");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, text: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dirs");
    }
    std::fs::write(path, text).expect("write");
}

/// A repository with `assets/a.txt`, `assets/b.txt` and `assets/c.bin` committed on `main`, its
/// own identity, no signing and no hooks (the user's global settings never reach the fixture).
fn fixture(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    git(&dir, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Fixture Author"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ".git/no-hooks"),
        ("core.autocrlf", "false"),
    ] {
        git(&dir, &["config", key, value]);
    }
    write(&dir, "assets/a.txt", b"one\ntwo\n");
    write(&dir, "assets/b.txt", b"bee\n");
    write(&dir, "assets/c.bin", &[0, 1, 2, 3, 0, 255]);
    // As the project templates do: `.regia/` is per-user editor state.
    write(&dir, ".gitignore", b"build/\n/.regia\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    dir
}

fn world_at(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .expect("method exists");
    handler(world, &params)
}

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn state_of_file(status: &Value, path: &str) -> String {
    status["files"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["path"] == path)
        .map(|f| f["state"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

#[test]
fn porcelain_v2_records_parse() {
    let bytes = b"# branch.oid 1234\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaa bbb assets/a file.txt\0\
1 A. N... 000000 100644 100644 000 ccc assets/new.txt\0\
2 R. N... 100644 100644 100644 ddd ddd R100 assets/moved.txt\0assets/old.txt\0\
u UU N... 100644 100644 100644 100644 e1 e2 e3 assets/conflict.txt\0\
? assets/untracked.txt\0\
! build/\0";
    let status = git::parse_porcelain_v2(bytes);
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.head.as_deref(), Some("1234"));
    let states: Vec<(&str, RevisionState)> = status
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.state()))
        .collect();
    assert_eq!(
        states,
        vec![
            ("assets/a file.txt", RevisionState::Modified),
            ("assets/new.txt", RevisionState::Added),
            ("assets/moved.txt", RevisionState::Renamed),
            ("assets/conflict.txt", RevisionState::Conflicted),
            ("assets/untracked.txt", RevisionState::Untracked),
            ("build/", RevisionState::Ignored),
        ]
    );
    assert_eq!(status.files[2].orig_path.as_deref(), Some("assets/old.txt"));
    let detached = git::parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head (detached)\0");
    assert!(detached.detached && detached.branch.is_none() && detached.head.is_none());
}

#[test]
fn ignored_directories_badge_what_is_inside() {
    let mut status = git::parse_porcelain_v2(b"! build/\0? assets/new.txt\0");
    status.files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(status.state_of("build/out/x.bin"), Some(RevisionState::Ignored));
    assert_eq!(status.state_of("assets/new.txt"), Some(RevisionState::Untracked));
    assert_eq!(status.state_of("assets/other.txt"), None);
}

#[test]
fn project_paths_are_checked() {
    assert_eq!(
        git::clean_project_path("assets\\a.txt").as_deref(),
        Ok("assets/a.txt")
    );
    assert_eq!(
        git::clean_project_path("./assets/a.txt").as_deref(),
        Ok("assets/a.txt")
    );
    for bad in [
        "",
        "../outside",
        "assets/../../x",
        "/etc/passwd",
        "-rf",
        "--force",
    ] {
        assert_eq!(
            git::clean_project_path(bad).map_err(|e| e.code),
            Err("invalid_path"),
            "{bad:?}"
        );
    }
}

/// Acceptance: modify, add and delete files; the lists show them.
#[test]
fn status_lists_modified_added_deleted_and_untracked_files() {
    let repo = fixture("vcs-status-lists");
    write(&repo, "assets/a.txt", b"one\ntwo\nthree\n");
    std::fs::remove_file(repo.join("assets/b.txt")).expect("delete b");
    write(&repo, "assets/new.txt", b"new\n");
    write(&repo, "assets/staged.txt", b"staged\n");
    write(&repo, "build/out.bin", b"ignored");
    git(&repo, &["add", "assets/staged.txt"]);
    let mut world = world_at(&repo);
    let status = call(&mut world, "editor.vcs_status", json!({})).expect("status");
    assert_eq!(status["state"], "ready", "{status}");
    assert_eq!(status["branch"], "main");
    assert_eq!(state_of_file(&status, "assets/a.txt"), "modified");
    assert_eq!(state_of_file(&status, "assets/b.txt"), "deleted");
    assert_eq!(state_of_file(&status, "assets/new.txt"), "untracked");
    assert_eq!(state_of_file(&status, "assets/staged.txt"), "added");
    assert_eq!(state_of_file(&status, "build/"), "ignored");
    assert_eq!(status["staged"], json!(["assets/staged.txt"]));
    assert_eq!(
        status["unstaged"],
        json!(["assets/a.txt", "assets/b.txt", "assets/new.txt"])
    );
    // The browser hook reads the same cache.
    assert_eq!(state_of(&world, "assets/a.txt"), Some(RevisionState::Modified));
    assert_eq!(state_of(&world, "build/out.bin"), Some(RevisionState::Ignored));
    assert_eq!(state_of(&world, "assets/c.bin"), None);
    assert_eq!(
        decorate_row(&world, "assets/new.txt", "new.txt".into()),
        "new.txt   [?]"
    );
}

/// Acceptance: stage and commit through the methods the panel calls; git log has the commit.
#[test]
fn stage_and_commit_make_the_expected_commit() {
    let repo = fixture("vcs-commit");
    write(&repo, "assets/a.txt", b"one\nTWO\n");
    write(&repo, "assets/new.txt", b"new\n");
    std::fs::remove_file(repo.join("assets/b.txt")).expect("delete b");
    let mut world = world_at(&repo);
    let staged = call(
        &mut world,
        "editor.vcs_stage",
        json!({ "paths": ["assets/a.txt", "assets/new.txt", "assets/b.txt"] }),
    )
    .expect("stage");
    assert_eq!(
        staged["staged"],
        json!(["assets/a.txt", "assets/b.txt", "assets/new.txt"])
    );
    let empty = call(&mut world, "editor.vcs_commit", json!({ "message": "  " })).expect_err("empty");
    assert_eq!(code(&empty), "empty_message");
    let done = call(
        &mut world,
        "editor.vcs_commit",
        json!({ "message": "Edit a, add new, drop b" }),
    )
    .expect("commit");
    let id = done["commit"].as_str().expect("commit id").to_string();
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]).trim(), id);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]).trim(),
        "Edit a, add new, drop b"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%an"]).trim(),
        "Fixture Author"
    );
    let names = git(&repo, &["show", "--name-status", "--format=", "HEAD"]);
    let mut rows: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
    rows.sort_unstable();
    assert_eq!(
        rows,
        vec!["A\tassets/new.txt", "D\tassets/b.txt", "M\tassets/a.txt"]
    );
    let after = call(&mut world, "editor.vcs_status", json!({})).expect("status");
    assert_eq!(after["changed"], 0, "{after}");
    let nothing = call(&mut world, "editor.vcs_commit", json!({ "message": "again" })).expect_err("nothing");
    assert_eq!(code(&nothing), "nothing_staged");
}

#[test]
fn unstage_keeps_the_working_tree_change() {
    let repo = fixture("vcs-unstage");
    write(&repo, "assets/a.txt", b"changed\n");
    let mut world = world_at(&repo);
    call(
        &mut world,
        "editor.vcs_stage",
        json!({ "paths": ["assets/a.txt"] }),
    )
    .expect("stage");
    let out = call(
        &mut world,
        "editor.vcs_unstage",
        json!({ "paths": ["assets/a.txt"] }),
    )
    .expect("unstage");
    assert_eq!(out["staged"], json!([]));
    assert_eq!(out["unstaged"], json!(["assets/a.txt"]));
    assert_eq!(std::fs::read(repo.join("assets/a.txt")).expect("a"), b"changed\n");
}

#[test]
fn discard_needs_confirm_then_restores_and_deletes() {
    let repo = fixture("vcs-discard");
    write(&repo, "assets/a.txt", b"scribble\n");
    write(&repo, "assets/junk.txt", b"junk\n");
    let mut world = world_at(&repo);
    let paths = json!(["assets/a.txt", "assets/junk.txt"]);
    let refused = call(&mut world, "editor.vcs_discard", json!({ "paths": paths })).expect_err("confirm");
    assert_eq!(code(&refused), "confirm_required");
    assert_eq!(
        std::fs::read(repo.join("assets/a.txt")).expect("a"),
        b"scribble\n"
    );
    let out = call(
        &mut world,
        "editor.vcs_discard",
        json!({ "paths": paths, "confirm": true }),
    )
    .expect("discard");
    assert_eq!(out["discarded"], json!(["assets/a.txt", "assets/junk.txt"]));
    assert_eq!(
        std::fs::read(repo.join("assets/a.txt")).expect("a"),
        b"one\ntwo\n"
    );
    assert!(!repo.join("assets/junk.txt").exists());
    let escape = call(
        &mut world,
        "editor.vcs_discard",
        json!({ "paths": ["../x"], "confirm": true }),
    )
    .expect_err("escape");
    assert_eq!(code(&escape), "invalid_path");
}

/// Acceptance: a file history lists two commits (newest first).
#[test]
fn file_history_lists_two_commits() {
    let repo = fixture("vcs-history");
    write(&repo, "assets/a.txt", b"second\n");
    git(&repo, &["commit", "-q", "-am", "second edit of a"]);
    write(&repo, "assets/b.txt", b"other\n");
    git(&repo, &["commit", "-q", "-am", "edit of b"]);
    let mut world = world_at(&repo);
    let log = call(&mut world, "editor.vcs_log", json!({ "path": "assets/a.txt" })).expect("log");
    let summaries: Vec<&str> = log["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|e| e["summary"].as_str())
        .collect();
    assert_eq!(summaries, vec!["second edit of a", "first"]);
    assert_eq!(log["entries"][0]["author"], "Fixture Author");
    assert!(log["entries"][0]["date"].as_str().is_some_and(|d| d.len() >= 10));
    let all = call(&mut world, "editor.vcs_log", json!({ "limit": 2 })).expect("log");
    assert_eq!(all["entries"].as_array().map(Vec::len), Some(2));
    // `open: true` shows it in the window's History tab (the Content Browser's Show history).
    call(
        &mut world,
        "editor.vcs_log",
        json!({ "path": "assets/a.txt", "open": true }),
    )
    .expect("open");
    let panel = world.resource::<panel::VcsPanel>();
    assert!(panel.open && panel.tab == panel::VcsTab::History);
}

#[test]
fn diff_shows_text_lines_and_binary_sizes() {
    let repo = fixture("vcs-diff");
    write(&repo, "assets/a.txt", b"one\nthree\n");
    write(&repo, "assets/c.bin", &[9, 0, 9, 0, 9, 0, 9, 0]);
    write(&repo, "assets/new.txt", b"fresh\n");
    let mut world = world_at(&repo);
    let text = call(&mut world, "editor.vcs_diff", json!({ "path": "assets/a.txt" })).expect("diff");
    assert_eq!(text["kind"], "text");
    let body = text["text"].as_str().expect("text");
    assert!(body.contains("-two") && body.contains("+three"), "{body}");
    let binary = call(&mut world, "editor.vcs_diff", json!({ "path": "assets/c.bin" })).expect("diff");
    assert_eq!(binary["kind"], "binary", "{binary}");
    assert_eq!(binary["old_size"], 6);
    assert_eq!(binary["new_size"], 8);
    let new = call(&mut world, "editor.vcs_diff", json!({ "path": "assets/new.txt" })).expect("diff");
    assert_eq!(new["kind"], "text");
    assert!(new["text"].as_str().is_some_and(|t| t.contains("+fresh")));
    call(
        &mut world,
        "editor.vcs_stage",
        json!({ "paths": ["assets/a.txt"] }),
    )
    .expect("stage");
    let staged = call(
        &mut world,
        "editor.vcs_diff",
        json!({ "path": "assets/a.txt", "staged": true }),
    )
    .expect("diff");
    assert!(staged["text"].as_str().is_some_and(|t| t.contains("+three")));
    let unstaged = call(&mut world, "editor.vcs_diff", json!({ "path": "assets/a.txt" })).expect("diff");
    assert_eq!(unstaged["kind"], "unchanged", "everything is staged");
}

#[test]
fn branches_list_create_and_switch_with_a_dirty_guard() {
    let repo = fixture("vcs-branch");
    let mut world = world_at(&repo);
    let list = call(&mut world, "editor.vcs_branch", json!({ "action": "list" })).expect("list");
    assert_eq!(list["current"], "main");
    let created = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "create", "name": "feature/look" }),
    )
    .expect("create");
    assert_eq!(created["current"], "feature/look");
    assert_eq!(created["branches"], json!(["feature/look", "main"]));
    let again = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "create", "name": "main" }),
    )
    .expect_err("exists");
    assert_eq!(code(&again), "branch_exists");
    let bad = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "create", "name": "-x" }),
    )
    .expect_err("bad name");
    assert_eq!(code(&bad), "invalid_branch");
    // A tracked change blocks the switch; an untracked file does not.
    write(&repo, "assets/a.txt", b"dirty\n");
    let dirty = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "switch", "name": "main" }),
    )
    .expect_err("dirty");
    assert_eq!(code(&dirty), "dirty_tree");
    git(&repo, &["checkout", "-q", "--", "assets/a.txt"]);
    write(&repo, "assets/untracked.txt", b"u\n");
    let switched = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "switch", "name": "main" }),
    )
    .expect("switch");
    assert_eq!(switched["current"], "main");
    let unknown = call(
        &mut world,
        "editor.vcs_branch",
        json!({ "action": "switch", "name": "nope" }),
    )
    .expect_err("unknown");
    assert_eq!(code(&unknown), "unknown_branch");
    let action = call(&mut world, "editor.vcs_branch", json!({ "action": "merge" })).expect_err("action");
    assert_eq!(code(&action), "invalid_action");
}

#[test]
fn a_project_outside_git_shows_no_revision_control() {
    let dir = TempDir::new("vcs-no-repo");
    let mut world = world_at(&dir);
    let status = call(&mut world, "editor.vcs_status", json!({})).expect("status");
    assert_eq!(status["state"], "no_revision_control", "{status}");
    let refused = call(&mut world, "editor.vcs_stage", json!({ "paths": ["a.txt"] })).expect_err("no repo");
    assert_eq!(code(&refused), "no_revision_control");
    let mut no_project = World::new();
    let status = call(&mut no_project, "editor.vcs_status", json!({})).expect("status");
    assert_eq!(status["state"], "no_project");
}

#[test]
fn a_missing_git_program_is_reported() {
    let repo = fixture("vcs-no-git");
    let missing = repo.join("no-such-git-program");
    let error = GitDriver::open(&repo, GitProgram(missing)).expect_err("missing");
    assert_eq!(error.code, "git_not_found");
}

/// A project in a subdirectory of the repository sees project-relative paths.
#[test]
fn a_project_inside_a_larger_repository_uses_its_own_paths() {
    let repo = fixture("vcs-subdir");
    write(&repo, "game/assets/x.txt", b"x\n");
    write(&repo, "assets/a.txt", b"outside the project\n");
    let mut world = world_at(&repo.join("game"));
    let status = call(&mut world, "editor.vcs_status", json!({})).expect("status");
    assert_eq!(status["unstaged"], json!(["assets/x.txt"]), "{status}");
    call(
        &mut world,
        "editor.vcs_stage",
        json!({ "paths": ["assets/x.txt"] }),
    )
    .expect("stage");
    let staged = git(&repo, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged.trim(), "game/assets/x.txt");
}

/// The cache polls on change notifications only: idle ticks start nothing.
#[test]
fn the_status_cache_polls_only_when_its_key_moves() {
    let repo = fixture("vcs-poll-count");
    let mut world = world_at(&repo);
    tick_and_wait(&mut world);
    let first = world.resource::<VcsStatus>().polls_started;
    assert_eq!(first, 1);
    for _ in 0..50 {
        tick(&mut world);
    }
    tick_and_wait(&mut world);
    assert_eq!(
        world.resource::<VcsStatus>().polls_started,
        1,
        "idle frames start no git process"
    );
    request_refresh(&mut world);
    tick_and_wait(&mut world);
    assert_eq!(world.resource::<VcsStatus>().polls_started, 2);
    assert_eq!(world.resource::<VcsStatus>().polls_finished, 2);
}

/// Acceptance: the Outliner's level row shows modified after an edit is saved. The save makes the
/// level clean again, which moves the cache's key; the poll that follows sees the new bytes.
#[test]
fn the_level_row_shows_modified_after_a_saved_edit() {
    let repo = fixture("vcs-level-row");
    write(&repo, "assets/main.level.ron", b"(entities: [])\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "level"]);
    let mut world = world_at(&repo);
    world.insert_resource(crate::level_io::OpenLevel {
        id: regia_scene::prelude::LevelId::default(),
        path: Some(repo.join("assets/main.level.ron")),
    });
    world.insert_resource(crate::command::UndoStack::new());
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    tick_and_wait(&mut world);
    assert_eq!(level_path(&world).as_deref(), Some("assets/main.level.ron"));
    assert_eq!(level_state(&world), None, "clean after the commit");
    // An edit: the level is dirty (one poll; the file on disk is unchanged).
    world.resource_mut::<crate::command::UndoStack>().mark_dirty();
    tick_and_wait(&mut world);
    assert_eq!(level_state(&world), None);
    // The save writes the file and makes the level clean.
    write(
        &repo,
        "assets/main.level.ron",
        b"(entities: [(name: \"cube\")])\n",
    );
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    tick_and_wait(&mut world);
    assert_eq!(level_state(&world), Some(RevisionState::Modified));
    assert_eq!(world.resource::<VcsStatus>().polls_started, 3);
}

/// A world with the repository's project and its committed level open.
fn world_with_level(repo: &Path) -> World {
    write(repo, "assets/main.level.ron", b"(entities: [])\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "level"]);
    let mut world = world_at(repo);
    world.insert_resource(crate::level_io::OpenLevel {
        id: regia_scene::prelude::LevelId::default(),
        path: Some(repo.join("assets/main.level.ron")),
    });
    world.insert_resource(crate::command::UndoStack::new());
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    world
}

/// Acceptance: the Outliner's Revision column shows a modified level file. The poll fills the
/// hook once; reading cells afterwards starts no git process.
#[test]
fn the_revision_column_shows_a_modified_level_file() {
    use crate::outliner_columns::{Column, cell_text};
    let repo = fixture("vcs-revision-column");
    let mut world = world_with_level(&repo);
    let entity = world.spawn(regia_scene::prelude::Name::new("crate")).id();
    tick_and_wait(&mut world);
    assert_eq!(
        cell_text(&world, entity, Column::Revision),
        "",
        "clean level file"
    );
    write(
        &repo,
        "assets/main.level.ron",
        b"(entities: [(name: \"cube\")])\n",
    );
    request_refresh(&mut world);
    tick_and_wait(&mut world);
    assert_eq!(cell_text(&world, entity, Column::Revision), "modified");
    let polls = world.resource::<VcsStatus>().polls_started;
    for _ in 0..200 {
        let _ = cell_text(&world, entity, Column::Revision);
        tick(&mut world);
    }
    assert_eq!(
        world.resource::<VcsStatus>().polls_started,
        polls,
        "cells read the published state: no git call per frame"
    );
    // A level that git does not know yet is untracked.
    write(&repo, "assets/new.level.ron", b"(entities: [])\n");
    world.insert_resource(crate::level_io::OpenLevel {
        id: regia_scene::prelude::LevelId::default(),
        path: Some(repo.join("assets/new.level.ron")),
    });
    request_refresh(&mut world);
    tick_and_wait(&mut world);
    assert_eq!(cell_text(&world, entity, Column::Revision), "untracked");
}

/// Acceptance: the asset tooltip's revision line comes from git (temp repository fixture): modified,
/// untracked and unchanged files read their state; without a repository it says not tracked.
#[test]
fn the_asset_tooltip_shows_the_revision_state_from_git() {
    let repo = fixture("vcs-asset-tooltip");
    let mut world = world_at(&repo);
    tick_and_wait(&mut world);
    let tip = |world: &mut World, rel: &str| crate::content_actions::tooltip_text(world, &repo, rel, None);
    assert!(
        tip(&mut world, "assets/a.txt").contains("Revision: unchanged"),
        "{}",
        tip(&mut world, "assets/a.txt")
    );
    write(&repo, "assets/a.txt", b"one\ntwo\nthree\n");
    write(&repo, "assets/fresh.txt", b"new\n");
    request_refresh(&mut world);
    tick_and_wait(&mut world);
    assert!(tip(&mut world, "assets/a.txt").contains("Revision: modified"));
    assert!(tip(&mut world, "assets/fresh.txt").contains("Revision: untracked"));
    assert!(tip(&mut world, "assets/b.txt").contains("Revision: unchanged"));
    // Outside a repository the line stays "not tracked".
    let plain = TempDir::new("vcs-asset-tooltip-plain");
    write(&plain, "assets/a.txt", b"x\n");
    let mut loose = world_at(&plain);
    tick_and_wait(&mut loose);
    let text = crate::content_actions::tooltip_text(&mut loose, &plain, "assets/a.txt", None);
    assert!(text.contains("Revision: not tracked"), "{text}");
}

/// Acceptance: `editor.status_bar` reports the branch and the changed count the poll found.
#[test]
fn editor_status_bar_reports_the_branch() {
    let repo = fixture("vcs-status-bar");
    let mut world = world_at(&repo);
    assert!(
        crate::shell_status::values(&world).revision.is_none(),
        "nothing before the first poll"
    );
    tick_and_wait(&mut world);
    let status_bar = |world: &mut World| {
        let (_, handler) = crate::shell_status::methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.status_bar")
            .expect("method exists");
        handler(world, &json!({})).expect("status bar")
    };
    let reply = status_bar(&mut world);
    assert_eq!(reply["revision"]["label"], "main \u{b7} clean", "{reply}");
    write(&repo, "assets/a.txt", b"changed\n");
    request_refresh(&mut world);
    tick_and_wait(&mut world);
    let reply = status_bar(&mut world);
    assert_eq!(reply["revision"]["label"], "main \u{b7} 1 changed", "{reply}");
    assert!(
        reply["revision"]["tooltip"]
            .as_str()
            .unwrap_or_default()
            .contains("branch main"),
        "{reply}"
    );
    // A project outside git reports its state, not a branch.
    let plain = TempDir::new("vcs-status-bar-plain");
    let mut loose = world_at(&plain);
    tick_and_wait(&mut loose);
    let reply = status_bar(&mut loose);
    assert_eq!(reply["revision"]["label"], "No revision control", "{reply}");
}

/// The panel's keyboard path: Tab to a row's checkbox, Space checks it, Tab to Stage, Enter
/// stages it. Every control is named.
#[test]
fn the_panel_stages_a_file_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let repo = fixture("vcs-panel-keys");
    write(&repo, "assets/a.txt", b"keyboard edit\n");
    let mut world = world_at(&repo);
    refresh_now(&mut world);
    let mut opened = panel::VcsPanel::default();
    opened.open = true;
    world.insert_resource(opened);
    let ctx = egui::Context::default();
    let size = [700.0, 1000.0];
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| panel::draw_and_apply(ui, &mut world));
    };
    frame(&ctx, size, vec![], &mut draw);
    let update = frame(&ctx, size, vec![], &mut draw);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    tab_to(&ctx, size, "assets/a.txt", false, 40, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Stage", false, 40, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let status = cached(&world).expect("status");
    let a = status.file("assets/a.txt").expect("row");
    assert!(a.is_staged() && !a.is_unstaged(), "{a:?}");
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]).trim(),
        "assets/a.txt"
    );
    let panel = world.resource::<panel::VcsPanel>();
    assert_eq!(panel.notice, Some((false, "Staged 1 file(s)".to_string())));
}

/// A queued op runs once per frame even when the UI runs twice in that frame.
/// The panel's draw closure over `world` (one borrow per call).
fn panel_draw(world: &mut World) -> impl FnMut(&mut egui::Ui) + '_ {
    move |ui| {
        egui::CentralPanel::default().show(ui, |ui| panel::draw_and_apply(ui, world));
    }
}

/// Acceptance: a RON asset's diff in the Changes panel is structural (side by side fields), with
/// a toggle back to the text, all from the keyboard; a file that is not RON shows its text only.
#[test]
fn a_ron_asset_diffs_structurally_in_the_changes_panel() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let repo = fixture("vcs-panel-structural");
    write(&repo, "assets/thing.ron", b"(a: 1, b: \"x\")\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "thing"]);
    write(&repo, "assets/thing.ron", b"(a: 2, b: \"x\", c: true)\n");
    write(&repo, "assets/a.txt", b"edited\n");
    let mut world = world_at(&repo);
    refresh_now(&mut world);
    let mut opened = panel::VcsPanel::default();
    opened.open = true;
    world.insert_resource(opened);
    let ctx = egui::Context::default();
    let size = [700.0, 1100.0];
    let enter = key(egui::Key::Enter, egui::Modifiers::NONE);
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    // The Changes list is sorted: assets/a.txt first (not RON), assets/thing.ron second. Every row
    // has a Diff toggle; Tab reaches the first, then on to the second.
    tab_to(&ctx, size, "Diff", false, 60, &mut panel_draw(&mut world));
    frame(&ctx, size, vec![enter.clone()], &mut panel_draw(&mut world));
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    {
        let panel = world.resource::<panel::VcsPanel>();
        assert_eq!(panel.diff.as_ref().map(|d| d.path.as_str()), Some("assets/a.txt"));
        assert!(panel.structural.is_none(), "a text file has no structural view");
    }
    tab_to(&ctx, size, "Diff", false, 60, &mut panel_draw(&mut world));
    frame(&ctx, size, vec![enter.clone()], &mut panel_draw(&mut world));
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    let update = frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    {
        let panel = world.resource::<panel::VcsPanel>();
        let window = panel
            .structural
            .as_ref()
            .expect("a RON asset has a structural diff");
        let diff = window.diff().expect("the diff");
        assert_eq!(diff.old.label(), "index");
        assert_eq!(diff.new.label(), "working copy");
        let changed: Vec<&str> = diff.body.fields().iter().map(|c| c.path.as_str()).collect();
        assert_eq!(changed.len(), 2, "a changed and c added: {changed:?}");
        assert!(!panel.text_diff, "the structural view is the default");
    }
    // The toggle back to the text, and again to the structural view (keyboard).
    tab_to(&ctx, size, "Text", false, 60, &mut panel_draw(&mut world));
    frame(&ctx, size, vec![enter.clone()], &mut panel_draw(&mut world));
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    assert!(world.resource::<panel::VcsPanel>().text_diff);
    tab_to(&ctx, size, "Structural", true, 60, &mut panel_draw(&mut world));
    frame(&ctx, size, vec![enter], &mut panel_draw(&mut world));
    frame(&ctx, size, vec![], &mut panel_draw(&mut world));
    assert!(!world.resource::<panel::VcsPanel>().text_diff);
}

/// The structural diff of a staged row compares `HEAD` with the index, and a RON asset missing on
/// one side says why there is no structural view.
#[test]
fn the_structural_diff_follows_the_staged_side() {
    let repo = fixture("vcs-structural-sides");
    write(&repo, "assets/thing.ron", b"(a: 1)\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "thing"]);
    write(&repo, "assets/thing.ron", b"(a: 2)\n");
    git(&repo, &["add", "assets/thing.ron"]);
    write(&repo, "assets/thing.ron", b"(a: 3)\n");
    write(&repo, "assets/fresh.ron", b"(z: 1)\n");
    let world = world_at(&repo);
    let staged = panel::structural_diff(&world, "assets/thing.ron", true)
        .expect("ok")
        .expect("RON");
    let diff = staged.diff().expect("diff");
    assert!(diff.old.label().starts_with("HEAD ("), "{}", diff.old.label());
    assert_eq!(diff.new.label(), "index");
    let unstaged = panel::structural_diff(&world, "assets/thing.ron", false)
        .expect("ok")
        .expect("RON");
    let diff = unstaged.diff().expect("diff");
    assert_eq!(diff.new.label(), "working copy");
    assert!(
        panel::structural_diff(&world, "assets/a.txt", false)
            .expect("ok")
            .is_none()
    );
    let note = panel::structural_diff(&world, "assets/fresh.ron", false).expect_err("untracked");
    assert!(note.contains("missing on one side"), "{note}");
}

#[test]
fn a_second_ui_pass_in_one_frame_does_not_repeat_an_op() {
    let repo = fixture("vcs-panel-two-pass");
    let mut world = world_at(&repo);
    refresh_now(&mut world);
    world.insert_resource(panel::VcsPanel::default());
    let ctx = egui::Context::default();
    let mut passes = 0;
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        for _ in 0..2 {
            world
                .resource_mut::<panel::VcsPanel>()
                .pending
                .push(panel::Op::Refresh);
            panel::draw_and_apply(ui, &mut world);
            passes += 1;
        }
    });
    out.textures_delta.clear();
    assert_eq!(passes, 2);
    // The first pass ran its load refresh and the queued one; the second pass ran nothing.
    assert_eq!(world.resource::<panel::VcsPanel>().ops_applied, 2);
}

/// Validation before commit: a staged UI layout that references a missing asset refuses the
/// commit, and the report reaches the Problems panel model; without a loaded registry nothing is
/// validated and the commit goes through.
#[test]
fn commit_validates_the_staged_assets_first() {
    let repo = fixture("vcs-commit-validation");
    write(
        &repo,
        "assets/ui/hud.ui.ron",
        br#"(use: "ui/health.ui.ron", mask: (asset: "textures/btn.png"))"#,
    );
    let cache = repo.join("cache");
    std::fs::create_dir_all(&cache).expect("cache");
    regia_asset::registry::AssetRegistry::default()
        .write_to(&regia_asset::registry::cache_registry_path(&cache))
        .expect("registry");
    let loaded = regia_asset::registry::LoadedAssetRegistry::load(&cache, None)
        .expect("load")
        .expect("registry file");
    let mut world = world_at(&repo);
    world.insert_resource(loaded);
    call(
        &mut world,
        "editor.vcs_stage",
        json!({ "paths": ["assets/ui/hud.ui.ron"] }),
    )
    .expect("stage");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let refused = call(&mut world, "editor.vcs_commit", json!({ "message": "hud" })).expect_err("refused");
    let why = code(&refused);
    assert!(
        why == "validation_failed" || why == "validation_warnings",
        "{why}: {}",
        refused.message
    );
    assert!(refused.message.contains("hud.ui.ron"), "{}", refused.message);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head, "no commit was made");
    assert!(
        world.get_resource::<crate::asset_panels::AssetPanels>().is_some(),
        "the report went to the Problems panel model"
    );
    // Without a registry nothing is validated.
    world.remove_resource::<regia_asset::registry::LoadedAssetRegistry>();
    let done = call(&mut world, "editor.vcs_commit", json!({ "message": "hud" })).expect("commit");
    assert_eq!(done["validated"], json!([]));
    assert_ne!(git(&repo, &["rev-parse", "HEAD"]), head);
}

/// The methods are reachable over the real dispatcher (schema rows, dispatch arms).
#[test]
fn the_methods_answer_through_the_agent_dispatcher() {
    let repo = fixture("vcs-dispatch");
    write(&repo, "assets/a.txt", b"dispatched\n");
    let mut app = crate::engine::app::App::new();
    app.add_plugins(crate::engine::app::MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(repo.to_path_buf()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    crate::agent_ext::install(app.world_mut());
    app.insert_resource(crate::agent_ext::EditorProject {
        root: Some(repo.to_path_buf()),
    });
    app.finish_plugins();
    let (sender, inbox) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let mut call = |method: &str, params: Value| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        app.update();
        let response = answer.try_recv().expect("dispatch answers in one frame");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("agent result")
    };
    let status = call("editor.vcs_status", json!({}));
    assert_eq!(status["unstaged"], json!(["assets/a.txt"]), "{status}");
    call("editor.vcs_stage", json!({ "paths": ["assets/a.txt"] }));
    let done = call("editor.vcs_commit", json!({ "message": "over the dispatcher" }));
    assert!(done["commit"].as_str().is_some_and(|c| c.len() >= 40), "{done}");
    let log = call("editor.vcs_log", json!({ "path": "assets/a.txt" }));
    assert_eq!(log["entries"][0]["summary"], "over the dispatcher");
}

#[test]
fn every_error_code_is_protocol_shaped() {
    for code in agent::ERROR_CODES {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    let names: Vec<&str> = agent::methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        vec![
            "editor.vcs_status",
            "editor.vcs_stage",
            "editor.vcs_unstage",
            "editor.vcs_discard",
            "editor.vcs_commit",
            "editor.vcs_diff",
            "editor.vcs_log",
            "editor.vcs_branch",
        ]
    );
}
