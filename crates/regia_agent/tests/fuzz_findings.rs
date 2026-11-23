// SPDX-License-Identifier: MPL-2.0
//! Every finding of the coverage-guided fuzzer stays fixed (ADR-2723, `docs/specs/97-fuzz-hardening.md` § 8): the files
//! under `fuzz/findings/<target>/` replay through the registry on stable Rust, in the normal gate, with the check the
//! fuzzer itself uses. A finding whose fix belongs to another lane waits under `fuzz/findings/pending/<target>/`, which is
//! only checked for its layout. The targets this crate cannot link replay in `xtask`'s `every_committed_finding_replays_clean`,
//! also in the gate, and through `cargo xtask fuzz replay`.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use regia_agent::fuzz_targets::{self, FailureKind, FuzzInput, FuzzTarget};

#[path = "../../../xtask/src/fuzz/one_input.rs"]
mod one_input;

/// Prefixes of the targets `xtask/src/fuzz/extra_targets.rs` adds from crates this one does not link. `xtask`'s tests check that
/// every extra target has one, and replays them.
const EXTRA_PREFIXES: &[&str] = &["lua.", "import.", "pak.", "cooked.", "save.", "content."];
/// A guard against a hang only: the verdict decides the result, never the clock.
const HANG_GUARD: Duration = Duration::from_secs(300);

fn findings_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/findings")
}

/// The directories or files directly under `dir`, sorted; nothing for a missing directory.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|list| list.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.retain(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'))
    });
    paths.sort();
    paths
}

fn name_of(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
}

fn is_extra(name: &str) -> bool {
    EXTRA_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[test]
fn committed_findings_stay_fixed() {
    // A moved directory reads as no findings at all, which would pass.
    assert!(
        findings_root().join("README.md").is_file(),
        "{} is not the findings directory",
        findings_root().display()
    );
    let mut replayed = 0usize;
    let mut failed = Vec::new();
    for dir in entries(&findings_root()).into_iter().filter(|p| p.is_dir()) {
        let name = name_of(&dir).to_string();
        if name == "pending" {
            continue;
        }
        let Some(target) = fuzz_targets::target(&name).copied() else {
            assert!(
                is_extra(&name),
                "{}: names no fuzz target (a renamed target must move its findings)",
                dir.display()
            );
            continue;
        };
        for file in entries(&dir) {
            let input = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(one_input::check_one(&target, &input));
            });
            match rx.recv_timeout(HANG_GUARD) {
                Ok(Ok(_)) => replayed += 1,
                Ok(Err(finding)) => failed.push(format!("{}: {finding}", file.display())),
                Err(_) => failed.push(format!("{}: no answer in {HANG_GUARD:?}", file.display())),
            }
        }
    }
    eprintln!("replayed {replayed} findings");
    assert!(
        failed.is_empty(),
        "findings that fail again (fix the parser; a fix owned by another lane goes under fuzz/findings/pending/):\n{}",
        failed.join("\n")
    );
}

/// A pending finding is waiting for a fix, so it is not replayed; its directory must still name a real target.
#[test]
fn pending_findings_name_real_targets() {
    for dir in entries(&findings_root().join("pending")) {
        let name = name_of(&dir);
        assert!(
            dir.is_dir() && (fuzz_targets::target(name).is_some() || is_extra(name)),
            "{}: names no fuzz target",
            dir.display()
        );
    }
}

/// The replay would pass on anything if the check let a failure through: each kind of finding must be one.
#[test]
fn the_check_catches_every_kind_of_finding() {
    fn panics(_: &str) -> Result<String, String> {
        panic!("boom")
    }
    fn empty_error(_: &str) -> Result<String, String> {
        Err(String::new())
    }
    fn bug(_: &str) -> Result<String, String> {
        Err(format!("{}bad generated code", fuzz_targets::BUG_PREFIX))
    }
    fn unstable(text: &str) -> Result<String, String> {
        Ok(format!("{text}x"))
    }
    let target = |load| FuzzTarget {
        name: "test",
        format: None,
        format_version: 0,
        round_trip: true,
        input: FuzzInput::Text {
            fixture: String::new,
            load,
        },
    };
    let kind = |load| {
        one_input::check_one(&target(load), b"1")
            .expect_err("a finding")
            .kind
    };
    assert_eq!(kind(panics), FailureKind::Panic);
    assert_eq!(kind(empty_error), FailureKind::EmptyError);
    assert_eq!(kind(bug), FailureKind::Bug);
    assert_eq!(kind(unstable), FailureKind::RoundTrip);
}

/// The fixtures themselves pass the check, so a finding that replays clean means the input is handled, not ignored.
#[test]
fn every_builtin_fixture_passes_the_check() {
    let mut problems = Vec::new();
    for t in fuzz_targets::targets() {
        let bytes = match t.input {
            FuzzInput::Text { fixture, .. } => fixture().into_bytes(),
            FuzzInput::Bytes { fixture, .. } => fixture(),
        };
        if !matches!(one_input::check_one(t, &bytes), Ok(one_input::Verdict::Accepted)) {
            problems.push(t.name);
        }
    }
    assert!(problems.is_empty(), "fixtures not accepted: {problems:?}");
}

/// Spec 97 § 10: a committed finding (pending ones too) is named by the first 128 bits of the BLAKE3 hash of its bytes, in
/// lowercase hex, so the same input is never committed twice under two names and a file edited after the commit shows.
#[test]
fn committed_findings_are_named_by_their_hash() {
    let mut dirs: Vec<PathBuf> = entries(&findings_root())
        .into_iter()
        .filter(|p| p.is_dir() && name_of(p) != "pending")
        .collect();
    dirs.extend(
        entries(&findings_root().join("pending"))
            .into_iter()
            .filter(|p| p.is_dir()),
    );
    let mut checked = 0usize;
    let mut wrong = Vec::new();
    for dir in dirs {
        for file in entries(&dir) {
            let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let hash = blake3::hash(&bytes).to_hex();
            let expected = &hash.as_str()[..32];
            if name_of(&file) != expected {
                wrong.push(format!("{} should be named {expected}", file.display()));
            }
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no committed findings under {}",
        findings_root().display()
    );
    assert!(
        wrong.is_empty(),
        "findings not named by their hash:\n{}",
        wrong.join("\n")
    );
}
