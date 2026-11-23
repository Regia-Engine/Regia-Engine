// SPDX-License-Identifier: MPL-2.0
//! Determinism matrix (ADR-1347, `book/agents/determinism-matrix.md`): the headless scenes of
//! `regia_test_utils::det_matrix` give the same trace
//!
//! - on the single-threaded and the multi-threaded executor (this process);
//! - with a compute pool of 1, 2, 4 and 8 threads (one child process each: the pool is
//!   process-global and sized once);
//! - in a debug and a release build, and from commit to commit: each build compares with the
//!   trace pinned for its target in `tests/fixtures/determinism/` (write it with
//!   `REGIA_BLESS=1`, like a golden image, and say why in the commit message).
//!
//! `REGIA_DET_FRAMES` (default 240) and `REGIA_DET_THREADS` (default `1,2,4,8`) size the run.

use std::path::PathBuf;

use regia_test_utils::det_matrix::{
    self, Executor, PinnedOutcome, Trace, check_pinned, child_threads, diff, init_child_pool,
    print_child_traces, record, run_children,
};

/// Checkpoint spacing.
const EVERY: u32 = 20;

fn frames() -> u32 {
    std::env::var("REGIA_DET_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240)
}

fn thread_counts() -> Vec<usize> {
    std::env::var("REGIA_DET_THREADS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect::<Vec<usize>>()
        })
        .filter(|counts| !counts.is_empty())
        .unwrap_or_else(|| vec![1, 2, 4, 8])
}

fn record_all(executor: Executor) -> Vec<Trace> {
    det_matrix::scenes()
        .into_iter()
        .map(|(name, build)| record(name, build, frames(), EVERY, executor))
        .collect()
}

fn pinned_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("determinism")
}

/// The child half of `the_compute_pool_size_does_not_change_any_scene`: a no-op in a normal run.
#[test]
fn det_matrix_child() {
    let Some(threads) = child_threads() else {
        return;
    };
    let pool = init_child_pool(threads);
    print_child_traces(pool, &record_all(Executor::MultiThreaded));
}

#[test]
fn every_scene_moves_and_the_race_reuses_ids() {
    if child_threads().is_some() {
        return;
    }
    for (name, build) in det_matrix::scenes() {
        let trace = record(name, build, frames(), EVERY, Executor::SingleThreaded);
        let distinct: std::collections::BTreeSet<u64> = trace.checkpoints.iter().map(|c| c.1).collect();
        // Piles settle and bodies sleep, so only the first checkpoints must differ.
        assert!(
            distinct.len() >= 4,
            "{name}: the state changes between checkpoints: {trace:?}"
        );
        assert!(
            trace.keys.len() > 20,
            "{name} has state to compare: {} keys",
            trace.keys.len()
        );
    }
    // The race scene spawns more boxes than stay alive: the reaper frees ids that the lanes
    // reuse (boxes live 75 steps; spawning stops at step 180), and part of the pile is left.
    let race = record(
        "race",
        det_matrix::race_app,
        frames(),
        EVERY,
        Executor::SingleThreaded,
    );
    let live_boxes: std::collections::BTreeSet<&str> = race
        .keys
        .keys()
        .filter_map(|k| k.split('/').next())
        .filter(|entity| entity.starts_with("race"))
        .collect();
    assert!(
        live_boxes.len() >= 8,
        "boxes alive at the end: {}",
        live_boxes.len()
    );
    if frames() >= 160 {
        let born: Vec<u32> = live_boxes
            .iter()
            .filter_map(|entity| entity.rsplit('-').next()?.parse().ok())
            .collect();
        assert!(
            born.iter().all(|step| step + 80 >= frames()),
            "boxes older than their lifetime were reaped: {born:?}"
        );
    }
}

#[test]
fn a_scene_is_repeatable_in_one_process() {
    if child_threads().is_some() {
        return;
    }
    let first = record_all(Executor::MultiThreaded);
    let second = record_all(Executor::MultiThreaded);
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(diff(a, b, 12), None, "second run of {}", a.scene);
    }
}

#[test]
fn the_executor_does_not_change_any_scene() {
    if child_threads().is_some() {
        return;
    }
    let single = record_all(Executor::SingleThreaded);
    let multi = record_all(Executor::MultiThreaded);
    let mismatches: Vec<String> = single
        .iter()
        .zip(&multi)
        .filter_map(|(a, b)| diff(a, b, 12))
        .map(|m| m.to_string())
        .collect();
    assert!(
        mismatches.is_empty(),
        "single vs multi-threaded executor:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn the_compute_pool_size_does_not_change_any_scene() {
    if child_threads().is_some() {
        return;
    }
    let counts = thread_counts();
    let runs = run_children("det_matrix_child", &counts).unwrap_or_else(|e| panic!("{e}"));
    let scenes = det_matrix::scenes().len();
    for run in &runs {
        // Evidence that the race scene raced (timing-dependent, so only printed).
        println!(
            "{} compute threads: race spawners ran out of lane order on {:?} steps",
            run.threads, run.race_out_of_order
        );
        assert_eq!(
            run.pool_threads, run.threads,
            "the child sized its compute pool before anything else did"
        );
        assert_eq!(
            run.traces.len(),
            scenes,
            "{}-thread child printed every scene",
            run.threads
        );
    }
    let Some((reference, rest)) = runs.split_first() else {
        panic!("no thread counts to compare: {counts:?}");
    };
    let mut mismatches = Vec::new();
    for run in rest {
        for (a, b) in reference.traces.iter().zip(&run.traces) {
            if let Some(m) = diff(a, b, 12) {
                mismatches.push(format!("{} vs {} threads: {m}", reference.threads, run.threads));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn every_scene_matches_its_pinned_trace_in_this_build() {
    if child_threads().is_some() {
        return;
    }
    if frames() != 240 {
        // The pinned traces are 240 frames long.
        return;
    }
    let mut problems = Vec::new();
    for trace in record_all(Executor::MultiThreaded) {
        match check_pinned(&pinned_dir(), &trace) {
            Ok(PinnedOutcome::Matches) => {}
            Ok(PinnedOutcome::Skipped) => println!(
                "pinned trace of {} not compared (REGIA_SKIP_GOLDENS=1)",
                trace.scene
            ),
            Ok(PinnedOutcome::Blessed(path)) => println!("blessed {}", path.display()),
            Ok(PinnedOutcome::Missing(path)) => {
                // Pinned on the reference target (the gate's Linux x86_64); other targets
                // report it and compare once someone blesses theirs.
                let reference = cfg!(all(target_os = "linux", target_arch = "x86_64"));
                let message = format!(
                    "no pinned trace at {}: bless it with REGIA_BLESS=1 cargo test -p regia_test_utils \
                     --test determinism_matrix",
                    path.display()
                );
                if reference {
                    problems.push(message);
                } else {
                    println!("{message}");
                }
            }
            Ok(PinnedOutcome::Differs(m)) => problems.push(format!(
                "{} build differs from the pinned trace: {m}",
                det_matrix::build_profile()
            )),
            Err(e) => problems.push(format!("reading the pinned trace of {}: {e}", trace.scene)),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
