# Flaky tests, retries and nightly jobs

The merge gate ([The merge gate](merge-gate.md)) retries very little, and only in one place: the test runner. This
page says exactly what is retried, how a flaky test is recorded, and where the jobs that run outside the gate are
defined. Two registries hold the data, and `cargo xtask` checks both against the tree:

| file | command | holds |
|---|---|---|
| `docs/ci/flaky-tests.toml` | `cargo xtask flaky` | every known flaky or load-sensitive test binary, how the gate treats it, who owns the fix |
| `docs/ci/nightly-jobs.toml` | `cargo xtask nightly` | every nightly or periodic job: command, host, schedule, results, owner |

## What the gate retries

| gate step | retried? |
|---|---|
| conflict markers, the git-dependency parse, `fmt --check`, clippy, rustdoc, the test build | never: a failure fails the gate |
| xtask checks (`check-versions`, `notices --check`, `check-formats`, `book-check`, `check-shaders`, `check-unwraps`, `check-layers`) | never |
| doc tests (`cargo test --workspace --doc`, lavapipe pass) | never |
| test binaries, lavapipe pass (12 at a time, longest first, 1200 s limit per binary) | a failing binary runs **once more alone** |
| test binaries, real-GPU pass (4 at a time on the shared 5090, 300 s limit per binary, goldens skipped) | a failing binary runs **once more alone** |

The rerun rule, as the runner applies it:

- **Failed beside others, passed alone:** the gate prints `FLAKY UNDER LOAD <key>` and does not count the failure.
  The gate still passes.
- **Failed alone too:** the gate fails and prints `FAILED <key>` with the `cargo test` command that reruns it.
- **Over the time limit:** the binary is killed, run once more alone, and printed as `HANG <key>` either way. It
  counts by its second run.
- **Exit crash:** a binary killed by SIGSEGV or SIGABRT at process exit, after every one of its tests reported `ok`
  and with no panic in its output, counts as passed and is printed as `EXIT-CRASH`. This covers the NVIDIA
  driver-teardown family (flake `nvidia-exit-sigsegv`). It is loud on purpose; it is not a fix.
- **Solo binaries** run alone after the parallel pool, the way `cargo test` would run them. They are the
  wall-clock-bound binaries listed with `handling = "solo"`.
- **Quarantined binaries** (real-GPU pass only) still run every gate and are printed as `QUARANTINED`, but their
  result is not counted. When one passes, the gate prints `QUARANTINE CLEARED`: remove the quarantine that day.

A `<key>` is `package:target:kind`, for example `regia_scene:formats:test`. The registry uses the same keys.

No test retries itself inside the process. One older test still does: `emissive_hitch` re-measures a wall-time budget
(0240c762), and it is listed in the registry, as a `solo` binary since ADR-5908 (its 1 ms bar is not loosened; a
failure names each attempt's landing-frame cost and the host's load). New tests use counters, fixed steps and deterministic inputs instead of
elapsed-time asserts or sleeps.

## Recording a flake

Every `FLAKY UNDER LOAD`, `HANG` or `EXIT-CRASH` line in a gate summary, and every failure that passes when rerun by
hand, is recorded the same day:

1. Find the binary's entry in `docs/ci/flaky-tests.toml`, or add one (`id`, `tests`, `symptom`, `cause`, `status`,
   `handling`, `owner`).
2. Add the date to `seen`, and set `review_by` (required while the flake is `open` or `mitigated`).
3. Run `cargo xtask flaky`. It prints the table, fails on an entry that names a package, target or test function
   that does not exist, and prints a note for every overdue review.

An open flake seen twice is a P0 ticket (`docs/roadmap-post-alpha.md`, Process); `cargo xtask flaky` says so.

| field | meaning |
|---|---|
| `tests` | runner keys `package:target:kind`; each target must exist |
| `test_fns` | the test functions involved; each must exist in the package |
| `status` | `open`, `mitigated` (a fix landed, not yet proven), `fixed`, `latent` (a load-sensitive shape, not seen failing) |
| `handling` | `none` (only the rerun rule), `solo`, `quarantine`, `exit-crash` |
| `passes` | `lvp` and/or `gpu`: where `solo` or `quarantine` applies |
| `seen`, `review_by` | `YYYY-MM-DD` dates |

Policy, enforced by the check:

- **The lavapipe pass never quarantines.** It is the golden reference (ADR-025). `quarantine` takes `passes = ["gpu"]`
  only.
- Only an `open` or `mitigated` flake may be quarantined. A `fixed` entry with a quarantine fails the check.
- `solo` and `quarantine` need `passes`.
- Prefer fixing the test to adding a `solo` or a quarantine. A wall-clock assertion moves to an `#[ignore]`d perf check
  (listed as a nightly job), and the correctness test keeps a counter-based assertion.

## How the gate scripts read the registry

`cargo xtask flaky --gate-args lvp|gpu` prints the runner's arguments, one per line (`--solo`, then a key; or
`--quarantine`, then `key=reason`). It exits 1 when the registry does not validate, so a stale entry fails the gate
instead of being skipped. In `the gate script`, after the xtask build:

```sh
args=$("$T/debug/xtask" flaky --gate-args lvp) || fail "FLAKY REGISTRY FAIL"
SOLO=(); [ -n "$args" ] && mapfile -t SOLO <<< "$args"
python3 gate/runtests.py "$L/spool" "$L/build.json" "$L/tests" "${GATE_JOBS:-12}" --rerun-failures-alone ${SOLO[@]+"${SOLO[@]}"}
```

The real-GPU pass does the same with `--gate-args gpu` in place of its `Q=()` list. Do not use `mapfile` on empty
output: it yields one empty argument. Today the registry gives the lavapipe pass the four `--solo` binaries the
script used to list itself plus `regia_render:emissive_hitch:test` (ADR-5908), and the real-GPU pass nothing (a test
pins this).

## Nightly and periodic jobs

`docs/ci/nightly-jobs.toml` lists every job that runs outside the merge gate: the player sweep, look CI, the path-traced
plates, the loaded exit soak, Android lint, build times, the desktop soak, shuffle checks, the microbenchmarks, the
hot-reload speed check, the alpha smoke list, the device passes and fuzzing. Each job gives:

| field | meaning |
|---|---|
| `command` | the in-repo command, run from the workspace root |
| `wrapper` | the build machines script that schedules it, takes the card locks and copies the results (not in the repo) |
| `host`, `needs` | where it runs and what that host must have (`gpu`, `gpu-exclusive`, `do27-content`, `device`, ...) |
| `schedule` | `daily HH:MM`, `weekly <mon..sun> HH:MM`, `per-tag` or `on-demand` |
| `status` | `scheduled` (a cron or a CI schedule runs it), `manual` (a person runs it at those times), `planned` |
| `blocking` | whether a failure blocks a merge or a tag (none do today) |
| `results`, `owner` | where the output goes, and who reads it |

`cargo xtask nightly` prints the table and fails when a job names an xtask sub-command, a package, a test target or
a test function that does not exist. A `planned` job may name a sub-command that another branch adds; the check
prints a note instead, and another note once the command exists and the status should change.

Rules for every GPU job on the build machines: never Vulkan 6 (nvidia 6 and 7 on the build server); build with no GPU lock held, then
run under the card's lock (`the GPU job wrapper` takes it); skip the night while a merge gate holds `<scratch>`.
A nightly job never takes the gate lock, so a gate never waits for one.

## Writing tests that do not flake

- Temp paths: `regia_test_utils::unique_temp_dir` (process id plus a counter), never a fixed name, never the
  source tree. Write a file another test reads through a temporary file and a rename.
- No wall-clock budgets (`elapsed() < X`) and no sleeps as synchronization: count frames, steps or events.
- Renderer tests share one headless device (`regia_render::device::test_device`).
- Evidence files go behind `REGIA_WRITE_EVIDENCE=1` into `target/evidence/`, never into `docs/`.
- History of the fixed flakes and their root causes: `docs/evidence/flaky-tests/README.md`.
