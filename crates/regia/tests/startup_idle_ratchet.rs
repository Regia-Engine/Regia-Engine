// SPDX-License-Identifier: MPL-2.0
//! The start-up and idle-memory ratchet (ADR-2987, `docs/06-testing-and-ci.md` § 18).
//!
//! The per-frame ratchet counts what a frame does. Two costs it cannot see are counted here:
//! the work an app does before its first presented frame (pipelines, live GPU objects, files and
//! bytes read, heap allocations and peak heap) and the heap each engine subsystem still holds
//! after 120 idle frames. Each case below runs in a fresh child process of this binary (so the
//! numbers are those of a cold start, not of a process that already built a renderer), headless
//! on the machine's adapter (lavapipe in CI) with one 320×180 texture camera over a floor quad,
//! the headless stand-in for a window. The app takes its start-up snapshot with
//! `app.startup_work`'s own system (`regia_agent::startup_idle`), at the first rendered texture
//! camera; the idle heap comes from `regia_test_utils::heap_ledger` with the tagging allocator
//! installed in this binary.
//!
//! Every case runs twice. Every compared count (pipelines, live GPU objects, tracked GPU bytes,
//! files) must be equal in both runs; heap figures, which other threads' timing shifts a little,
//! must agree within 5 % and the larger is compared (`is_heap`); figures that follow the host or
//! thread timing are listed in [`EXCLUDED`] with their reason and only printed. Then each figure is
//! compared with its row in `tests/data/startup_idle.ron`: more than 10 % above fails, naming the
//! row and the command that re-records the table; more than 10 % below prints a note asking to
//! lower the row. Only the CPU reference adapter (lavapipe) compares; any other adapter prints
//! the figures and passes, because pipeline and object counts may follow device features.
//!
//! Re-record after a justified change, with the reason in the commit message:
//! `REGIA_STARTUP_IDLE_RECORD=1 cargo test -p regia --test startup_idle_ratchet`
//!
//! `cargo test -p regia --test startup_idle_ratchet -- --nocapture` prints every figure.

#![cfg(all(feature = "client", feature = "agent"))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use regia::prelude::*;
use regia_agent::startup_idle::{StartupCounts, StartupWorkRecord};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, RenderDevice, RenderTarget, TextureViewSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde::{Deserialize, Serialize};

regia_test_utils::install_tagging_allocator!();

/// The child's case name; unset in the parent.
const CASE_ENV: &str = "REGIA_STARTUP_IDLE_CASE";
/// Set to re-record the table instead of comparing.
const RECORD_ENV: &str = "REGIA_STARTUP_IDLE_RECORD";
/// The line a child prints its figures on.
const RESULT_MARK: &str = "STARTUP_IDLE_RESULT ";
/// Diagnostic lines a child prints for the parent to echo (pipeline sites, the idle table).
const NOTE_MARK: &str = "STARTUP_IDLE_NOTE ";
/// The line a child prints when the machine has no adapter.
const SKIP_MARK: &str = "STARTUP_IDLE_SKIP ";
/// The table's format version.
const FORMAT_VERSION: u32 = 1;
/// Idle frames whose heap is sampled at the end; the smallest sample per subsystem is what stayed
/// held.
const IDLE_TAIL: u32 = 60;
/// Idle frames before the heap is read.
const IDLE_FRAMES: u32 = 120;
/// Child processes run at once (each has [`POOL_THREADS`] pool threads and as many lavapipe threads).
const PARALLEL_CHILDREN: usize = 4;
/// Compute pool threads in the child, and lavapipe's rasterizer threads.
const POOL_THREADS: usize = 2;
/// The texture camera's fixed size.
const VIEW_SIZE: [u32; 2] = [320, 180];
/// Allowed growth over a row, in percent.
const SLACK_PERCENT: u64 = 10;
/// The re-record command, printed with every failure.
const RECORD_COMMAND: &str = "REGIA_STARTUP_IDLE_RECORD=1 cargo test -p regia --test startup_idle_ratchet";

/// The cases, in table order.
///
/// - `hello_window`: `DefaultPlugins` and the sample's clear-color system (the `blank` template is
///   the same set: its game plugin is empty).
/// - `template_first_person`: the engine plugins the `first_person` template adds, without its
///   game code.
/// - `template_character`: the engine plugins of `third_person` and `top_down` (top_down's set is
///   third_person's plus `CharacterControlPlugin`), without game code.
/// - `default_plus_optional`: `DefaultPlugins`, the character set and every optional engine plugin
///   that runs without hardware or a network peer (animation, crowds, motion matching, cloth,
///   vehicles, platformer, destruction).
const CASES: [&str; 4] = [
    "hello_window",
    "template_first_person",
    "template_character",
    "default_plus_optional",
];

/// Figures left out of the comparison, and why. The child still prints them.
const EXCLUDED: &[(&str, &str)] = &[
    (
        "threads_live",
        "the pools and lavapipe are pinned, but the agent server, telemetry and other engine threads \
         start on their own clocks, so the count at the first frame varies by one (15 vs 16 measured)",
    ),
    (
        "heap_peak_bytes",
        "the start-up peak is where transient allocations of several threads overlap (shader \
         composition, encode workers, the agent and telemetry threads): 75.8 vs 80.3 MB in two runs of \
         one case, beyond the heap bound; the allocations and bytes allocated before the first frame \
         are compared instead",
    ),
    (
        "idle_heap_transient_bytes",
        "the transient heap on top of what stays held depends on where a worker is in its task when \
         the tail is sampled (measured: 0 or about 14 MB unscoped in the sets with the character \
         plugins); printed for the report",
    ),
    (
        "tracked_submits",
        "the staging flush submits when a thread's written bytes reach its cap, which depends on \
         which pool thread ran the upload",
    ),
];

/// The committed table (`tests/data/startup_idle.ron`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Table {
    /// [`FORMAT_VERSION`].
    format_version: u32,
    /// Adapter the table was recorded on.
    adapter: String,
    /// Per case: figure name -> value.
    rows: BTreeMap<String, BTreeMap<String, u64>>,
}

/// One child's figures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CaseResult {
    case: String,
    adapter: String,
    software: bool,
    trigger: String,
    frame: u64,
    figures: BTreeMap<String, u64>,
}

/// The rest of `line` after `mark`. With `--nocapture` the harness prints `test child_case ... `
/// without a newline, so the child's first line does not start with its mark.
fn after_mark<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    line.split_once(mark).map(|(_, rest)| rest)
}

fn table_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("startup_idle.ron")
}

fn is_excluded(name: &str) -> bool {
    EXCLUDED.iter().any(|(n, _)| *n == name)
}

/// Heap figures: what other threads allocate and free while the main thread runs (worker pools,
/// the encode workers, the agent and telemetry threads) shifts them a little from run to run
/// (measured on lavapipe: allocation counts by under 0.01 %, live bytes by under 0.1 %; the peak
/// varies more and is excluded). They are not held to equality; the two runs must agree within
/// [`HEAP_RUN_BOUND_PERCENT`], the larger one is compared (and recorded), and the 10 % ratchet
/// applies as to every other figure. Every other compared figure must be equal.
fn is_heap(name: &str) -> bool {
    name.starts_with("heap_") || name.starts_with("idle_heap_")
}

/// How far the two runs' heap figures may differ, in percent of the larger.
const HEAP_RUN_BOUND_PERCENT: u64 = 5;

/// One case's compared figures: exact ones from the first run, heap ones the larger of the two.
fn compared_figures(a: &CaseResult, b: &CaseResult) -> BTreeMap<String, u64> {
    a.figures
        .iter()
        .filter(|(name, _)| !is_excluded(name))
        .map(|(name, va)| {
            let v = if is_heap(name) {
                (*va).max(b.figures.get(name).copied().unwrap_or(0))
            } else {
                *va
            };
            (name.clone(), v)
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------
// Parent
// ------------------------------------------------------------------------------------------------

#[test]
fn startup_idle_ratchet() {
    if std::env::var_os(CASE_ENV).is_some() {
        return; // this process is a child: `child_case` does the work
    }
    // Two runs of every case, each in its own process, [`PARALLEL_CHILDREN`] at a time (the
    // figures are counts of one process's own work, so neighbours do not change them).
    let jobs: Vec<&str> = CASES.iter().flat_map(|case| [*case, *case]).collect();
    let mut runs: Vec<Option<CaseResult>> = Vec::new();
    for batch in jobs.chunks(PARALLEL_CHILDREN) {
        let batch_runs: Vec<Option<CaseResult>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|case| scope.spawn(move || run_child(case)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| panic!("a child runner panicked (see above)"))
                })
                .collect()
        });
        runs.extend(batch_runs);
    }
    if runs.iter().any(Option::is_none) {
        return; // no adapter: `run_child` printed why
    }
    let mut runs = runs.into_iter().flatten();
    let mut results: Vec<(CaseResult, CaseResult)> = Vec::new();
    while let (Some(first), Some(second)) = (runs.next(), runs.next()) {
        results.push((first, second));
    }

    // Determinism: both runs of a case agree on every compared figure.
    let mut unstable = Vec::new();
    for (a, b) in &results {
        assert_eq!(
            a.trigger, b.trigger,
            "{}: the start-up window ended differently",
            a.case
        );
        for (name, va) in &a.figures {
            let vb = b.figures.get(name).copied();
            if is_excluded(name) {
                continue;
            }
            let stable = match vb {
                Some(vb) if is_heap(name) => {
                    let (lo, hi) = ((*va).min(vb), (*va).max(vb));
                    u128::from(hi - lo) * 100 <= u128::from(hi) * u128::from(HEAP_RUN_BOUND_PERCENT)
                }
                Some(vb) => *va == vb,
                None => false,
            };
            if !stable {
                unstable.push(format!("{}.{name}: {va} vs {vb:?}", a.case));
            }
        }
    }
    for (a, b) in &results {
        println!(
            "startup_idle: {} ({}, start-up ended by {} at frame {})",
            a.case, a.adapter, a.trigger, a.frame
        );
        for (name, v) in &a.figures {
            let other = b.figures.get(name).copied().unwrap_or(0);
            let note = if is_excluded(name) {
                format!("  (not compared; second run {other})")
            } else if is_heap(name) && other != *v {
                format!("  (second run {other})")
            } else {
                String::new()
            };
            println!("  {name:<34} {v:>14}{note}");
        }
    }
    assert!(
        unstable.is_empty(),
        "figures differ between two runs of the same case (exact figures must be equal, heap figures \
         within {HEAP_RUN_BOUND_PERCENT} %; a new non-deterministic figure must be listed in EXCLUDED with \
         its reason):\n  {}",
        unstable.join("\n  ")
    );

    let software = results.iter().all(|(a, _)| a.software);
    let adapter = results
        .first()
        .map(|(a, _)| a.adapter.clone())
        .unwrap_or_default();
    if std::env::var_os(RECORD_ENV).is_some() {
        record(&results, &adapter);
        return;
    }
    if !software {
        println!(
            "startup_idle: adapter {adapter} is not the CPU reference adapter; figures printed, not \
             compared (the table is recorded on lavapipe)"
        );
        return;
    }
    let table = load_table();
    let mut failures = Vec::new();
    let mut notes = Vec::new();
    for (result, second) in &results {
        let Some(row) = table.rows.get(&result.case) else {
            failures.push(format!(
                "row {} is missing from {}; record it with `{RECORD_COMMAND}`",
                result.case,
                table_path().display()
            ));
            continue;
        };
        let verdicts = compare_row(&result.case, row, &compared_figures(result, second));
        failures.extend(verdicts.failures);
        notes.extend(verdicts.notes);
    }
    for note in &notes {
        println!("startup_idle: note: {note}");
    }
    assert!(
        failures.is_empty(),
        "start-up/idle figures over their rows by more than {SLACK_PERCENT} %:\n  {}\n\
         If the growth is intended, re-record with `{RECORD_COMMAND}` and give the reason in the \
         commit message.",
        failures.join("\n  ")
    );
}

/// Failures (over the slack) and notes (well under the row) of one case.
#[derive(Debug, Default)]
struct Verdicts {
    failures: Vec<String>,
    notes: Vec<String>,
}

/// Compares one case's figures with its row: over `row × 1.1` fails, under `row × 0.9` is a note.
/// A figure the row does not have, or has and the run lacks, fails too: the table must be
/// re-recorded when the set of figures changes.
fn compare_row(case: &str, row: &BTreeMap<String, u64>, figures: &BTreeMap<String, u64>) -> Verdicts {
    let mut out = Verdicts::default();
    for (name, &value) in figures {
        if is_excluded(name) {
            continue;
        }
        let Some(&limit) = row.get(name) else {
            out.failures.push(format!(
                "{case}.{name} = {value} has no row value; re-record with `{RECORD_COMMAND}`"
            ));
            continue;
        };
        if u128::from(value) * 100 > u128::from(limit) * u128::from(100 + SLACK_PERCENT) {
            out.failures.push(format!(
                "{case}.{name} = {value}, row {limit} (+{:.1} %); re-record with `{RECORD_COMMAND}` \
                 if intended",
                percent_over(value, limit)
            ));
        } else if u128::from(value) * 100 < u128::from(limit) * u128::from(100 - SLACK_PERCENT) {
            out.notes.push(format!(
                "{case}.{name} = {value} is well under its row {limit}: lower the row with \
                 `{RECORD_COMMAND}` so the table keeps the gain"
            ));
        }
    }
    for name in row.keys() {
        if !figures.contains_key(name) && !is_excluded(name) {
            out.failures.push(format!(
                "{case}.{name} is in the row but was not measured; re-record with `{RECORD_COMMAND}`"
            ));
        }
    }
    out
}

fn percent_over(value: u64, limit: u64) -> f64 {
    if limit == 0 {
        return f64::INFINITY;
    }
    (value as f64 / limit as f64 - 1.0) * 100.0
}

fn load_table() -> Table {
    let path = table_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}; record it with `{RECORD_COMMAND}`",
            path.display()
        )
    });
    let table: Table =
        ron::from_str(&text).unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
    assert_eq!(
        table.format_version,
        FORMAT_VERSION,
        "{} has format_version {}, this test reads {FORMAT_VERSION}; re-record with `{RECORD_COMMAND}`",
        path.display(),
        table.format_version
    );
    table
}

/// Writes the table (temporary file, then rename) from the first run of each case.
fn record(results: &[(CaseResult, CaseResult)], adapter: &str) {
    let table = Table {
        format_version: FORMAT_VERSION,
        adapter: adapter.to_owned(),
        rows: results
            .iter()
            .map(|(a, b)| (a.case.clone(), compared_figures(a, b)))
            .collect(),
    };
    let pretty = regia_core::ron_pretty().depth_limit(3);
    let text =
        ron::ser::to_string_pretty(&table, pretty).unwrap_or_else(|e| panic!("serialize the table: {e}"));
    let header = "// Start-up and idle-memory figures per case (ADR-2987), compared by\n\
                  // crates/regia/tests/startup_idle_ratchet.rs. Generated: re-record with\n\
                  // REGIA_STARTUP_IDLE_RECORD=1 cargo test -p regia --test startup_idle_ratchet\n\
                  // and give the reason in the commit message.\n";
    let path = table_path();
    let tmp = path.with_extension(format!("ron.tmp{}", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap_or(&path))
        .unwrap_or_else(|e| panic!("create the data dir: {e}"));
    std::fs::write(&tmp, format!("{header}{text}\n"))
        .unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
    std::fs::rename(&tmp, &path).unwrap_or_else(|e| panic!("rename to {}: {e}", path.display()));
    println!(
        "startup_idle: recorded {} rows on {adapter} into {}",
        table.rows.len(),
        path.display()
    );
}

/// Runs `case` in a child process; `None` when the machine has no adapter (the test then passes
/// with a note, as the other GPU tests do).
fn run_child(case: &str) -> Option<CaseResult> {
    let exe = std::env::current_exe().unwrap_or_else(|e| panic!("no test executable: {e}"));
    let root = regia_test_utils::unique_temp_dir("startup-idle");
    let mut command = Command::new(exe);
    command
        .args(["--exact", "child_case", "--nocapture", "--test-threads=1"])
        .env(CASE_ENV, case)
        .env("REGIA_PROJECT_DIR", root.join("project"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("HOME", &root)
        .env("REGIA_AUDIO", "off")
        // lavapipe sizes its rasterizer threads by the core count unless told.
        .env("LP_NUM_THREADS", POOL_THREADS.to_string())
        .env_remove("REGIA_PIPELINE_CACHE_DIR")
        .env_remove("REGIA_TELEMETRY")
        .env_remove(RECORD_ENV);
    // A bound on a hang only: a cold start on lavapipe takes well under a minute.
    let out = regia_test_utils::process::output_within(&mut command, Duration::from_secs(900))
        .unwrap_or_else(|e| panic!("{case}: cannot run the child: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for note in stdout.lines().filter_map(|l| after_mark(l, NOTE_MARK)) {
        println!("startup_idle: {note}");
    }
    let _ = std::fs::remove_dir_all(&root);
    if let Some(line) = stdout.lines().find_map(|l| after_mark(l, SKIP_MARK)) {
        println!("startup_idle: skipped: {line}");
        return None;
    }
    let Some(line) = stdout.lines().find_map(|l| after_mark(l, RESULT_MARK)) else {
        panic!(
            "{case}: the child printed no result (status {:?})\nstdout:\n{stdout}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let result: CaseResult =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{case}: unreadable result {line}: {e}"));
    assert!(
        out.status.success(),
        "{case}: the child failed: {:?}\n{stdout}",
        out.status
    );
    Some(result)
}

// ------------------------------------------------------------------------------------------------
// Child
// ------------------------------------------------------------------------------------------------

#[test]
fn child_case() {
    let Some(case) = std::env::var_os(CASE_ENV) else {
        return; // the parent test runs the cases
    };
    let case = case.to_string_lossy().into_owned();
    // Fixed pool sizes (the first `init` wins; `DefaultPlugins` asks later), so the thread count
    // and the work split are the same on every host.
    regia_core::tasks::init(regia_core::tasks::TaskPoolOptions {
        compute_threads: Some(POOL_THREADS),
        async_compute_threads: Some(1),
        io_threads: Some(1),
        ..regia_core::tasks::TaskPoolOptions::default()
    });
    // Everything before this point (the test harness, the pools) is not the app's.
    let baseline = StartupCounts::now(None);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            println!("{SKIP_MARK}no GPU adapter ({err})");
            return;
        }
    };
    let info = device.adapter.get_info();
    // The CPU reference adapter (lavapipe) is the one the table is recorded on.
    let software = format!("{:?}", info.device_type) == "Cpu";
    let after_device = StartupCounts::now(None);
    let mut app = build_app(&case, device);
    spawn_view(app.world_mut());
    let after_build = StartupCounts::now(None);

    // Start-up: frames until `app.startup_work`'s system took its snapshot.
    let mut frames = 0u64;
    while app.world().resource::<StartupWorkRecord>().capture.is_none() {
        assert!(
            frames <= regia_agent::startup_idle::STARTUP_FRAME_LIMIT,
            "{case}: no start-up snapshot"
        );
        app.update();
        frames += 1;
    }
    // Where the start-up heap went, by phase (printed for the report, not compared).
    let after_frame = StartupCounts::now(None);
    let phase = |name: &str, from: &StartupCounts, to: &StartupCounts| {
        let sum = |c: &StartupCounts, f: fn(&regia_core::memory::SubsystemMemory) -> u64| {
            c.heap.subsystems.iter().map(|(_, m)| f(m)).sum::<u64>()
        };
        println!(
            "{NOTE_MARK}{case}: phase {name}: {} allocations, {} B allocated, {} B live after",
            sum(to, |m| m.total_allocations).saturating_sub(sum(from, |m| m.total_allocations)),
            sum(to, |m| m.total_bytes).saturating_sub(sum(from, |m| m.total_bytes)),
            to.heap.live_bytes,
        );
    };
    phase("device", &baseline, &after_device);
    phase("plugins_build_finish_spawn", &after_device, &after_build);
    phase("first_frame", &after_build, &after_frame);
    let capture = app
        .world()
        .resource::<StartupWorkRecord>()
        .capture
        .clone()
        .unwrap_or_else(|| panic!("{case}: snapshot vanished"));
    // The agent method answers from the same snapshot.
    let answer = regia_agent::startup_idle::startup_work(app.world_mut(), &serde_json::json!({}))
        .unwrap_or_else(|e| panic!("{case}: app.startup_work failed: {e:?}"));
    assert_eq!(answer["captured"], true, "{case}: {answer}");
    assert_eq!(answer["trigger"], capture.trigger.name());
    // Where the start-up pipelines came from (printed for the report, not compared).
    if let Some(warm) = app.world().get_resource::<regia_render::PipelineWarmQueue>() {
        let progress = warm.progress();
        let mut sites = progress.outside_sites.clone();
        sites.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        println!(
            "{NOTE_MARK}{case}: {} pipelines outside the warm queue; top creation sites:",
            progress.pipelines_outside
        );
        for (site, count) in sites.iter().take(12) {
            println!("{NOTE_MARK}  {count:>4}  {site}");
        }
    }

    // The largest GPU allocations after start-up, by label (printed for the report, not compared).
    if let Some(device) = app.world().get_resource::<RenderDevice>() {
        let report = regia_render::memory_report(device);
        if report.available {
            for label in report.labels.iter().take(24) {
                println!(
                    "{NOTE_MARK}{case}: gpu {:>10} B x{:<3} {:<10} {}",
                    label.bytes,
                    label.count,
                    label.category.name(),
                    label.label
                );
            }
        }
    }
    let mut figures = BTreeMap::new();
    let before: BTreeMap<&str, Option<u64>> = baseline.figures().into_iter().collect();
    for (name, value) in capture.counts.figures() {
        let Some(value) = value else { continue };
        assert_eq!(
            answer["figures"][name].as_u64(),
            Some(value),
            "{case}: app.startup_work disagrees on {name}"
        );
        // Counters and gauges alike: the app's share is the change since the baseline. The
        // peak heap becomes the peak above the heap the harness held before the app.
        let base = match name {
            "heap_peak_bytes" => before.get("heap_live_bytes").copied().flatten(),
            _ => before.get(name).copied().flatten(),
        };
        figures.insert(name.to_owned(), value.saturating_sub(base.unwrap_or(0)));
    }

    // Idle: the heap each subsystem holds after `IDLE_FRAMES` untouched frames. Each frame waits for
    // the GPU, as a display-paced app does, so the same number of frames is in flight at every
    // sample. Each idle frame also advances the clocks by exactly one fixed step, as a display-paced
    // app at the fixed rate does (ADR-7640): on the wall clock a slow or loaded host runs up to
    // `max_substeps` `FixedUpdate`s per frame (physics, character, timers), so the idle allocation
    // count followed the host's load (template_character 153,740 vs 162,542 allocations in two runs
    // of one binary on a loaded the build server, the v0.5.0 rc4 gate). Start-up keeps the wall clock.
    let step = app.world().resource::<FixedTime>().step();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    let steps_before = app.world().resource::<FixedSteps>().0;
    let device = app.world().resource::<RenderDevice>().clone();
    let idle = regia_test_utils::heap_ledger::idle_heap_settled(IDLE_FRAMES, IDLE_TAIL, || {
        app.update();
        if let Err(err) = device.wait_idle() {
            panic!("{case}: waiting for the GPU failed: {err}");
        }
    });
    let idle_steps = app.world().resource::<FixedSteps>().0 - steps_before;
    assert_eq!(
        idle_steps,
        u64::from(IDLE_FRAMES),
        "{case}: the idle frames must run one fixed step each, whatever the host's load (ADR-7640)"
    );
    // What stayed held through the last `IDLE_TAIL` frames: a buffer kept while idle is in every
    // sample, a worker's transient allocation is not.
    for subsystem in regia_test_utils::heap_ledger::Subsystem::ALL {
        figures.insert(
            format!("idle_heap_live_{}", subsystem.name()),
            idle.floor_bytes(subsystem),
        );
    }
    // The renderer's named heap owners (ADR-3351): what holds the render heap, by name. Every
    // owner gets a figure (0 when it never allocated) so the rows match across cases.
    for owner in regia_render::heap_owners::ALL {
        figures.insert(
            format!("idle_heap_owner_{}", owner.name()),
            idle.owner_floor_bytes(owner.name()),
        );
    }
    figures.insert("idle_heap_live_total".to_owned(), idle.floor.live_bytes);
    figures.insert("idle_heap_allocations".to_owned(), idle.total_allocations());
    figures.insert("idle_heap_transient_bytes".to_owned(), idle.transient_bytes);
    for line in idle.table().lines() {
        println!("{NOTE_MARK}{case}: {line}");
    }
    println!(
        "{NOTE_MARK}{case}: idle transient {} B over the tail",
        idle.transient_bytes
    );

    let result = CaseResult {
        case: case.clone(),
        adapter: info.name.clone(),
        software,
        trigger: capture.trigger.name().to_owned(),
        frame: capture.frame,
        figures,
    };
    let line = serde_json::to_string(&result).unwrap_or_else(|e| panic!("{case}: serialize: {e}"));
    println!("{RESULT_MARK}{line}");
}

/// The case's app: `DefaultPlugins` (which carries the agent plugin and so `app.startup_work`'s
/// system) and the case's plugins. Each optional plugin is added only when nothing added it yet.
fn build_app(case: &str, device: RenderDevice) -> App {
    use regia_framework::camera::CameraPlugin;
    use regia_framework::character::{CharacterPlugin, use_rapier_character_controller};
    use regia_framework::character_control::CharacterControlPlugin;
    use regia_framework::save_flow::SaveFlowPlugin;
    use regia_framework::save_game::SavePlugin;
    use regia_framework::script::ScriptPlugin;

    let mut app = App::new();
    app.insert_resource(device);
    app.add_plugins(DefaultPlugins);
    app.init_resource::<FixedSteps>();
    app.add_systems(FixedUpdate, count_fixed_steps);
    macro_rules! add_once {
        ($($plugin:expr),+ $(,)?) => {
            $( add_unique(&mut app, $plugin); )+
        };
    }
    match case {
        "hello_window" => {
            app.add_systems(Update, animate_clear_color);
        }
        "template_first_person" => {
            add_once!(
                regia_input::InputPlugin::new(regia_input::InputConfig::default()),
                regia_scene::ScenePlugin,
                CameraPlugin,
            );
        }
        "template_character" | "default_plus_optional" => {
            add_once!(
                regia_scene::ScenePlugin,
                regia::physics::PhysicsPlugin,
                CharacterPlugin,
                CharacterControlPlugin,
                CameraPlugin,
                ScriptPlugin,
                regia_input::InputPlugin::new(regia_input::InputConfig::default()),
                SavePlugin::default(),
                SaveFlowPlugin,
            );
            use_rapier_character_controller(&mut app);
            if case == "default_plus_optional" {
                add_once!(
                    regia_animation::AnimationPlugin,
                    regia_animation::CrowdAnimationPlugin,
                    regia_animation::motion_matching::MotionMatchPlugin,
                    regia::physics::cloth::ClothPlugin,
                    regia::physics::vehicle::VehiclePlugin,
                    regia::physics::platformer2d::PlatformerPlugin,
                    regia::physics::destruction::DestructionPlugin::default(),
                );
            }
        }
        other => panic!("unknown case {other}"),
    }
    app.finish_plugins();
    app
}

/// Adds `plugin` unless a plugin of its type is already in the app.
fn add_unique<P: Plugin>(app: &mut App, plugin: P) {
    if !app.has_plugin(std::any::type_name::<P>()) {
        app.add_plugins(plugin);
    }
}

/// `FixedUpdate` steps the child's app has run; the idle window must add exactly one per frame
/// (ADR-7640).
#[derive(Resource, Default)]
struct FixedSteps(u64);

/// Counts [`FixedSteps`].
fn count_fixed_steps(mut steps: ResMut<FixedSteps>) {
    steps.0 += 1;
}

/// The sample's system: cycles the clear color with time.
fn animate_clear_color(time: Res<Time>, mut clear: ResMut<regia_render::ClearColor>) {
    let t = f64::from(time.elapsed_secs());
    clear.0 = [0.5 + 0.3 * t.sin(), 0.4, 0.6, 1.0];
}

/// A floor quad and one texture camera at [`VIEW_SIZE`] looking at it: the headless stand-in for
/// a window frame.
fn spawn_view(world: &mut World) {
    let vertex = |x: f32, z: f32| LabVertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv: [x / 20.0 + 0.5, z / 20.0 + 0.5],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    };
    let mesh = std::sync::Arc::new(CpuMesh {
        vertices: vec![
            vertex(-10.0, -10.0),
            vertex(10.0, -10.0),
            vertex(10.0, 10.0),
            vertex(-10.0, 10.0),
        ],
        indices: vec![0, 1, 2, 2, 3, 0],
    });
    let material = std::sync::Arc::new(CpuMaterial {
        base_color: [0.35, 0.38, 0.42, 1.0],
        roughness: 0.9,
        ..CpuMaterial::default()
    });
    let floor = Transform::default();
    world.spawn((
        floor,
        GlobalTransform(floor.compute_affine()),
        MeshRenderer::new(mesh, material),
    ));
    let eye = Transform::from_translation(regia_core::math::Vec3::new(0.0, 3.0, 12.0));
    world.spawn((
        eye,
        GlobalTransform(eye.compute_affine()),
        Camera {
            target: RenderTarget::Texture { asset_id: 1 },
            ..Camera::default()
        },
        TextureViewSettings {
            size: VIEW_SIZE,
            ..TextureViewSettings::default()
        },
    ));
}

#[test]
fn an_extra_figure_or_growth_fails_and_names_its_row() {
    let row: BTreeMap<String, u64> = [
        ("pipelines_created", 40u64),
        ("vfs_opens", 0),
        ("heap_peak_bytes", 1000),
    ]
    .into_iter()
    .map(|(n, v)| (n.to_owned(), v))
    .collect();
    let mut figures = row.clone();
    // 40 more pipelines on a cold start, one extra file, 5 % more peak heap (inside the slack).
    figures.insert("pipelines_created".to_owned(), 80);
    figures.insert("vfs_opens".to_owned(), 1);
    figures.insert("heap_peak_bytes".to_owned(), 1050);
    let verdicts = compare_row("hello_window", &row, &figures);
    assert_eq!(verdicts.failures.len(), 2, "{verdicts:?}");
    assert!(verdicts.failures[0].contains("hello_window.pipelines_created = 80, row 40"));
    assert!(verdicts.failures.iter().all(|f| f.contains(RECORD_COMMAND)));
    // A 20 MB scratch buffer kept after load: the idle row fails.
    let idle_row: BTreeMap<String, u64> = [("idle_heap_live_render".to_owned(), 300_000u64)].into();
    let idle: BTreeMap<String, u64> = [("idle_heap_live_render".to_owned(), 300_000 + (20 << 20))].into();
    assert_eq!(
        compare_row("template_character", &idle_row, &idle).failures.len(),
        1
    );
    // Well under: a note, not a failure.
    let lower: BTreeMap<String, u64> = [("idle_heap_live_render".to_owned(), 100_000u64)].into();
    let verdicts = compare_row("template_character", &idle_row, &lower);
    assert!(verdicts.failures.is_empty());
    assert_eq!(verdicts.notes.len(), 1);
    // A figure the row lacks, or a row figure no longer measured, fails.
    let extra: BTreeMap<String, u64> = [("new_figure".to_owned(), 1u64)].into();
    assert_eq!(compare_row("x", &BTreeMap::new(), &extra).failures.len(), 1);
    assert_eq!(compare_row("x", &idle_row, &BTreeMap::new()).failures.len(), 1);
    // Excluded figures are never compared.
    let threads: BTreeMap<String, u64> = [("threads_live".to_owned(), 99u64)].into();
    assert!(compare_row("x", &BTreeMap::new(), &threads).failures.is_empty());
}

#[test]
fn the_committed_table_parses_and_has_every_case() {
    if std::env::var_os(CASE_ENV).is_some() {
        return;
    }
    let table = load_table();
    for case in CASES {
        assert!(
            table.rows.contains_key(case),
            "row {case} missing; record with `{RECORD_COMMAND}`"
        );
    }
}

#[test]
fn a_mark_after_the_harness_prefix_is_found() {
    // `--nocapture`: the child's first line follows `test child_case ... ` on the same line.
    let first = format!("test child_case ... {SKIP_MARK}no GPU adapter");
    assert_eq!(after_mark(&first, SKIP_MARK), Some("no GPU adapter"));
    assert_eq!(after_mark(&format!("{RESULT_MARK}{{}}"), RESULT_MARK), Some("{}"));
    assert_eq!(after_mark("unrelated", NOTE_MARK), None);
}
