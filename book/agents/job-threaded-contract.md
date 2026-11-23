# Threaded job contract

The existing [job API](jobs.md) is exercised by committed tests in
`crates/regia_agent/src/jobs.rs`, under `jobs::tests::threaded`. The protocol thread
owns the registry and its world; worker threads receive cloned `JobHandle`s.
These tests add evidence for spec 23 § 4, with no new methods or job kinds.

## Poll while workers report

The test `workers_stream_while_protocol_polls_and_old_cursors_cross_eviction`
starts two registry jobs and 16 workers. Eight workers per job each emit 64 events
in each of 12 barrier-coordinated rounds, interleaving progress and event writes.
The protocol thread makes a fixed number of status calls during every round.
Assertions cover per-job payloads, matching progress/message snapshots, consecutive
sequence numbers, distinct events, per-worker continuity, bounded pages and cursors.

The `agent.job_status` handler receives these actual parameter shapes in the tests:

```json
{ "job": "tests:1", "since": 0 }
```

```json
{ "job": "tests:2", "since": 1 }
```

Both jobs have 6144 events after the workers join. Each retains 4096, reports
`dropped: 2048`, and returns at most 512 per call. A client polling from `since: 0`
or stale `since: 1` receives the oldest retained event (`seq: 2048`),
`next: 2560` and `more: true`. The sequence jump exposes the client's gap;
`dropped` counts all evicted events, rather than that client's unread events.
Continue with `since` equal to the answer's `next`. Eight pages drain the retained
ring without repeats; the final cursor is 6144. Polling at 6144 returns no events
and `more: false`. A cursor ahead of the producer (6161 in the test) stays 6161
with no events; the tests also check `u64::MAX`. Clients should use the server's
returned cursor. Cancelling the first job and then finishing both checks that
cancellation and results stay isolated, too.

## Finish, fail and cancel

The test `finish_fail_cancel_all_six_orders_preserve_the_first_terminal_state`
uses a worker command channel and acknowledgements to enumerate these orders:

| Completed call order | Terminal state |
|---|---|
| finish, fail, cancel | `done` |
| finish, cancel, fail | `done` |
| fail, finish, cancel | `failed` |
| fail, cancel, finish | `failed` |
| cancel, finish, fail | `cancelled` |
| cancel, fail, finish | `failed` |

The protocol thread calls `agent.job_cancel` with `{ "job": "tests:1" }`.
A running cancellable job answers `requested: true`, `state: "running"`:
this requests cancellation without completing the worker. A subsequent worker
finish observes the request and ends `cancelled`; a worker failure may end
`failed`. Cancelling a terminal job answers `requested: false` and its current
state. Worker failure in this fixture carries `error.reason: "threaded_failure"`
and message `worker failed`; it is a test fixture reason, not a new API error.

The test `finite_terminal_races_allow_any_winner_and_never_resurrect` runs 32
barrier-started races among finish, fail, progress/events and protocol cancel.
Any of `done`, `failed` or `cancelled` is permitted. Each snapshot must have a
consistent result/error, and a terminal snapshot must remain unchanged. It does
not require any particular thread to win. Late progress, events, finish and fail
calls cannot mutate or resurrect a terminal job. No new test uses sleeps or
elapsed-time assertions.

## Retention and errors

The test `pruning_keeps_running_jobs_and_evicted_handles_cannot_reinsert_them`
finishes 36 jobs in reverse creation order, then starts another job while a
worker reports on the oldest still-running job. Starting a job prunes excess
finished entries; the 32 retained finished jobs are selected by registry id.
Running jobs survive. The test lists through `agent.job_status {}` and checks
the exact retained set. A worker retaining an evicted handle cannot reinsert it.

Both `agent.job_status { "job": "tests:2", "since": 0 }` and
`agent.job_cancel { "job": "tests:2" }` then fail with JSON-RPC code `-32001`,
`data.code: "F0003"`, `data.reason: "job_not_found"`. This is job eviction,
separate from event-ring eviction, which returns a successful page with a gap.
Cancelling the noncancellable running fixture returns `data.reason:
"job_not_cancellable"` (`F0002`) and does not set its worker's cancellation flag.
Existing malformed-id coverage checks `job_id_invalid`; see [Jobs](jobs.md#reasons).

## Run the checks

```sh
cargo test -p regia_agent --lib jobs
cargo test -p regia_agent --lib contract::tests
cargo test -p regia_agent --lib gaps_tests
```

Test sensitivity was checked with a temporary local defect: removing the guard
that ignores events after termination made the controlled-order test fail. The
terminal snapshot gained a late event and `next` advanced from 1 to 2. The guard
was restored; this is evidence for the assertion, not a claimed runtime fix.

The existing `gaps_tests` exercise the real dispatcher, job routing, permissions,
a real `agent.run_tests` child, and cancellation of its process group. The new
unit tests call the existing status/cancel handlers directly to control every
worker step without transport timing.

For the lead's TSan recipe, use filter `jobs::tests::threaded`:

```sh
RUSTFLAGS="-Zsanitizer=thread --cfg sanitizer_compat" \
  cargo +nightly test -Zbuild-std -p regia_agent --lib \
  --target x86_64-unknown-linux-gnu jobs::tests::threaded -- --test-threads=1
```

Use a separate target directory and a private nightly with `rust-src`, following
the build-host rules. `sanitizer_compat` lets the sanitizer intercept worker
creation through the crash-handler dependency (see the sanitizer audit report).
TSan observes executed schedules; it does not prove all interleavings safe.

Linux verification on 2026-11-08 passed all four threaded tests under Rust 1.99.0
and under private nightly 1.102.0 (f5cedb83a) with the rebuilt standard library.
The TSan run reported no sanitizer findings. Newer-nightly future-compatibility
warnings were preserved without suppressions. The release fragment records the
separate complete-suite blockers in unchanged modules.
