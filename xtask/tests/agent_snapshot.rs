// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask agent-snapshot` end to end against the real protocol tables and a fixture
//! git repo: a fresh `--update` writes a parseable snapshot, `--check` passes on it, a
//! breaking change fails until a release fragment names the method under `## Upgrade`, and
//! even then fails until `--update` refreshes the snapshot (ADR-3335).

use std::path::Path;
use std::process::Command;

use xtask::agent_snapshot::{self, Args, SNAPSHOT_PATH};

/// git in `dir`, isolated from the host's identity, signing and hooks settings.
fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A fixture repo: `main` holds a base commit, lane work sits in commits on top, so the
/// release-fragment diff has something to compare against.
fn fixture() -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir("regia-agent-snapshot-fixture");
    git_in(&dir, &["init", "-q", "-b", "main"]);
    git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
    // Lane work sits on a branch: the merge-base with `main` stays at the base commit, so
    // fragments a lane commits count as "touched by this branch" (on `main` itself the
    // merge-base diff would be empty).
    git_in(&dir, &["checkout", "-q", "-b", "lane"]);
    dir
}

fn args() -> Args {
    Args {
        base: "main".to_owned(),
        ..Args::default()
    }
}

fn run(dir: &Path, args: &Args) -> bool {
    let code = agent_snapshot::run(dir, args).expect("run");
    code == std::process::ExitCode::SUCCESS
}

#[test]
fn update_writes_a_snapshot_check_accepts_it() {
    let dir = fixture();
    assert!(!run(&dir, &args()), "no snapshot yet: the check fails");
    assert!(
        run(
            &dir,
            &Args {
                update: true,
                ..args()
            },
        ),
        "--update succeeds"
    );
    let text = std::fs::read_to_string(dir.join(SNAPSHOT_PATH)).expect("snapshot written");
    let parsed = agent_snapshot::parse(&text).expect("the written snapshot parses");
    assert_eq!(parsed, agent_snapshot::current(), "it holds the current surface");
    assert!(run(&dir, &args()), "the check passes on the fresh snapshot");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_breaking_change_needs_an_upgrade_line_then_an_update() {
    let dir = fixture();
    run(
        &dir,
        &Args {
            update: true,
            ..args()
        },
    );
    // A lane whose tables lost `agent.hello`: the old snapshot still has it, so the check
    // sees a breaking removal. Simulate the stale side by adding the block to the snapshot.
    let path = dir.join(SNAPSHOT_PATH);
    let text = std::fs::read_to_string(&path).expect("snapshot");
    let mut old = agent_snapshot::parse(&text).expect("parse");
    old.methods.push(agent_snapshot::MethodBlock {
        name: "agent.removed_method".to_owned(),
        permission: "read".to_owned(),
        tool: String::new(),
        mutating: false,
        cost: Some("light".to_owned()),
        limits: Vec::new(),
        params: Vec::new(),
        result: "{}".to_owned(),
        errors: Vec::new(),
    });
    std::fs::write(&path, agent_snapshot::render(&old)).expect("stale snapshot");
    assert!(!run(&dir, &args()), "an undocumented breaking change fails");

    // The fragment names the method under `## Upgrade`, but the snapshot is stale: still failing.
    std::fs::create_dir_all(dir.join("docs/releases")).expect("dir");
    std::fs::write(
        dir.join("docs/releases/v-test.md"),
        "# Test\n\n## Upgrade\n\n- `agent.removed_method` is gone.\n",
    )
    .expect("fragment");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "lane: fragment"]);
    assert!(!run(&dir, &args()), "documented but stale: the check still fails");

    // `--update` refreshes the snapshot from the tables (which never had the method):
    // the check passes again.
    assert!(
        run(
            &dir,
            &Args {
                update: true,
                ..args()
            },
        ),
        "--update succeeds"
    );
    assert!(run(&dir, &args()), "the refreshed snapshot checks clean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_refuses_an_undocumented_change_even_though_it_refreshes_the_snapshot() {
    // The review hole: a lane removes a method, runs `--update`, commits the refreshed snapshot
    // with no release fragment — the ratchet must refuse to write, not bless the bypass.
    let dir = fixture();
    run(
        &dir,
        &Args {
            update: true,
            ..args()
        },
    );
    let path = dir.join(SNAPSHOT_PATH);
    let text = std::fs::read_to_string(&path).expect("snapshot");
    let mut old = agent_snapshot::parse(&text).expect("parse");
    old.methods.push(agent_snapshot::MethodBlock {
        name: "agent.removed_method".to_owned(),
        permission: "read".to_owned(),
        tool: String::new(),
        mutating: false,
        cost: Some("light".to_owned()),
        limits: Vec::new(),
        params: Vec::new(),
        result: "{}".to_owned(),
        errors: Vec::new(),
    });
    let stale = agent_snapshot::render(&old);
    std::fs::write(&path, &stale).expect("stale snapshot");
    assert!(
        !run(
            &dir,
            &Args {
                update: true,
                ..args()
            },
        ),
        "--update refuses: the breaking removal has no fragment line"
    );
    let after = std::fs::read_to_string(&path).expect("snapshot");
    assert_eq!(after, stale, "--update did not write over the refusal");

    // The fragment names the method under `## Upgrade`: `--update` now refreshes the snapshot.
    std::fs::create_dir_all(dir.join("docs/releases")).expect("dir");
    std::fs::write(
        dir.join("docs/releases/v-test.md"),
        "# Test\n\n## Upgrade\n\n- `agent.removed_method` is gone.\n",
    )
    .expect("fragment");
    assert!(
        run(
            &dir,
            &Args {
                update: true,
                ..args()
            },
        ),
        "--update succeeds once every change is documented"
    );
    assert!(run(&dir, &args()), "the refreshed snapshot checks clean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_fragment_line_without_the_snapshot_refresh_is_not_enough() {
    let dir = fixture();
    run(
        &dir,
        &Args {
            update: true,
            ..args()
        },
    );
    // An additive change: the snapshot is missing a method the tables have. (The fragment
    // alone cannot satisfy the check.)
    let path = dir.join(SNAPSHOT_PATH);
    let text = std::fs::read_to_string(&path).expect("snapshot");
    let mut old = agent_snapshot::parse(&text).expect("parse");
    old.methods.retain(|m| m.name != "curves.list");
    std::fs::write(&path, agent_snapshot::render(&old)).expect("stale snapshot");
    std::fs::create_dir_all(dir.join("docs/releases")).expect("dir");
    std::fs::write(
        dir.join("docs/releases/v-test.md"),
        "# Test\n\n## New\n\n- `curves.list` lists curve tables.\n",
    )
    .expect("fragment");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "lane: fragment"]);
    assert!(
        !run(&dir, &args()),
        "a New line names the method but the snapshot is stale"
    );
    assert!(
        run(
            &dir,
            &Args {
                update: true,
                ..args()
            },
        ),
        "--update succeeds"
    );
    assert!(run(&dir, &args()), "clean after the refresh");
    let _ = std::fs::remove_dir_all(&dir);
}
