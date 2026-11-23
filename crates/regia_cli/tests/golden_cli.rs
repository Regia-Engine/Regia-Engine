// SPDX-License-Identifier: MPL-2.0
//! `regia golden` (ADR-2821): the real binary on a project with a staged proposal. Without a terminal
//! `accept` refuses and changes nothing, whatever the environment says; on a real pseudo-terminal, a
//! person's typed answer installs the image and records the reason. Temp projects only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regia_agent::review::{
    ChangeStatus, GoldenProposal, QueueFile, REVIEW_QUEUE_FORMAT_VERSION, ReviewQueue, StagedChange,
    queue_path,
};
use regia_test_utils::unique_temp_dir;

const GOLDEN: &str = "crates/demo/goldens/sponza.png";
const STAGED: &str = ".regia/agent/review/golden_proposals/1";

fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A project with a golden and one pending proposal to replace it.
fn project(tag: &str) -> PathBuf {
    let root = unique_temp_dir(tag);
    let (before, after) = (b"old image bytes".to_vec(), b"new image bytes".to_vec());
    std::fs::create_dir_all(root.join("crates/demo/goldens")).expect("goldens");
    std::fs::write(root.join(GOLDEN), &before).expect("golden");
    std::fs::create_dir_all(root.join(STAGED)).expect("staged");
    std::fs::write(root.join(STAGED).join("before.png"), &before).expect("before");
    std::fs::write(root.join(STAGED).join("after.png"), &after).expect("after");
    std::fs::write(root.join(STAGED).join("diff.png"), b"diff").expect("diff");
    let file = QueueFile {
        format_version: REVIEW_QUEUE_FORMAT_VERSION,
        next_id: 1,
        next_set: 1,
        changes: vec![StagedChange {
            id: 1,
            set: 1,
            path: GOLDEN.into(),
            before: Some("golden: demo:sponza\n".into()),
            after: "golden: demo:sponza\nssim: 0.9\n".into(),
            reason: "the new sun angle is intended".into(),
            title: "bless golden demo:sponza".into(),
            client: "claude".into(),
            session: "s1".into(),
            model: "test-model".into(),
            images: Default::default(),
            created_ms: 0,
            status: ChangeStatus::Pending,
            decided_ms: None,
            decided_by: None,
            note: None,
            added: 1,
            removed: 0,
            texts_dropped: false,
            golden: Some(GoldenProposal {
                name: "demo:sponza".into(),
                job: "golden:1".into(),
                dir: STAGED.into(),
                before_hash: hash(&before),
                after_hash: hash(&after),
                stats: "{\"ssim\":0.9}".into(),
            }),
        }],
    };
    let queue = queue_path(&root);
    std::fs::create_dir_all(queue.parent().expect("parent")).expect("dir");
    std::fs::write(queue, file.to_json().expect("json")).expect("queue");
    root
}

fn regia(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_regia"));
    command.arg("golden").args(args).arg("--project").arg(root);
    command
}

fn status_of(root: &Path) -> ChangeStatus {
    ReviewQueue::load(root).file.changes[0].status
}

#[test]
fn accept_without_a_terminal_refuses_and_changes_nothing() {
    let root = project("golden-cli-no-tty");
    let before = std::fs::read(root.join(GOLDEN)).expect("golden");
    // A pipe that even answers correctly: a program cannot type its way in.
    let mut child = regia(&root, &["accept", "1"])
        // The bless variables, in both spellings, give no way around the question.
        .env("REGIA_BLESS", "1")
        .env("FERRITE_BLESS", "1") // rename-keep
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start regia");
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("stdin");
        let _ = stdin.write_all(b"sponza\n");
    }
    let output = child.wait_with_output().expect("output");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("golden_needs_terminal"), "{stderr}");
    assert_eq!(std::fs::read(root.join(GOLDEN)).expect("golden"), before);
    assert_eq!(status_of(&root), ChangeStatus::Pending);
    assert!(
        !root
            .join(".regia/agent/review/golden_commit_message.txt")
            .exists()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_shows_the_proposal_and_reject_drops_it() {
    let root = project("golden-cli-list");
    let before = std::fs::read(root.join(GOLDEN)).expect("golden");
    let output = regia(&root, &["list"]).output().expect("list");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(GOLDEN), "{text}");
    assert!(text.contains("the new sun angle is intended"), "{text}");
    assert!(text.contains("ssim 0.9"), "{text}");
    let output = regia(&root, &["list", "--json"]).output().expect("list json");
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(rows["pending"][0]["golden"]["name"], "demo:sponza");
    assert_eq!(rows["pending"][0]["golden"]["accept"], "regia golden accept 1");

    let output = regia(&root, &["reject", "1", "--note", "wrong shadow"])
        .output()
        .expect("reject");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(status_of(&root), ChangeStatus::Rejected);
    assert_eq!(std::fs::read(root.join(GOLDEN)).expect("golden"), before);
    assert!(!root.join(STAGED).exists(), "the staged images go with it");
    // A decided or unknown id is a named refusal.
    let output = regia(&root, &["reject", "1"]).output().expect("again");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("review_already_decided"));
    let output = regia(&root, &["reject", "9"]).output().expect("unknown");
    assert!(String::from_utf8_lossy(&output.stderr).contains("review_change_not_found"));
    let _ = std::fs::remove_dir_all(&root);
}

/// `script` runs a command on a pseudo-terminal; when a machine has none, the library test
/// `the_owner_accepts_with_the_reason_in_the_commit_message` still covers the accept itself.
#[cfg(target_os = "linux")]
fn on_a_terminal(root: &Path, typed: &str) -> Option<std::process::Output> {
    use std::io::Write as _;
    let line = format!(
        "{} golden accept 1 --project {}",
        env!("CARGO_BIN_EXE_regia"),
        root.display()
    );
    let mut child = Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(typed.as_bytes()).ok()?;
    child.wait_with_output().ok()
}

#[cfg(target_os = "linux")]
#[test]
fn a_person_typing_the_name_at_a_terminal_blesses_it() {
    let root = project("golden-cli-tty");
    let Some(wrong) = on_a_terminal(&root, "yes\n") else {
        eprintln!("no `script` here: the terminal path was not exercised");
        return;
    };
    assert!(!wrong.status.success(), "{wrong:?}");
    let shown = String::from_utf8_lossy(&wrong.stdout);
    assert!(shown.contains("golden_confirmation_mismatch"), "{shown}");
    assert!(
        shown.contains("the new sun angle is intended"),
        "the prompt shows the reason: {shown}"
    );
    assert_eq!(status_of(&root), ChangeStatus::Pending);
    assert_eq!(
        std::fs::read(root.join(GOLDEN)).expect("golden"),
        b"old image bytes"
    );

    let right = on_a_terminal(&root, "sponza\n").expect("second run");
    assert!(right.status.success(), "{right:?}");
    assert_eq!(
        std::fs::read(root.join(GOLDEN)).expect("golden"),
        b"new image bytes"
    );
    assert_eq!(status_of(&root), ChangeStatus::Accepted);
    let message = std::fs::read_to_string(root.join(".regia/agent/review/golden_commit_message.txt"))
        .expect("the commit message file");
    assert!(
        message.contains("Reason: the new sun angle is intended"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
