# Jobs and test runs

Long operations answer a **job id** at once and run off the protocol thread (specs/23 § 4,
ADR-2346). One method reads any of them: `agent.job_status` (MCP tool `job_status`). A job id is
`<kind>:<n>`.

| Kind | Started by | Read through |
|---|---|---|
| `tests` | `agent.run_tests` | the job registry (events, result) |
| `build` | `agent.build_game` | the job registry (events, result) |
| `hot` | `agent.hot_reload` (the editor's compile) | the job registry (events, result) |
| `render_out` | `render.out_start`, `render.out_job` (their `job` number) | forwards to `render.out_progress`; its answer is in `detail` |
| `dcc_bake` | `dcc_feed.bake` (its `job`) | forwards to `dcc_feed.bake_status`; its answer is in `detail` |

## Polling a job

```sh
regia agent call agent.job_status '{"job": "tests:1", "since": 0}'
```

```json
{
  "job": "tests:1", "kind": "tests", "label": "lib", "state": "running", "finished": false,
  "fraction": null, "message": "running unittests src/lib.rs (target/debug/deps/game-1a2b)",
  "events": [
    { "seq": 0, "kind": "binary", "name": "unittests src/lib.rs (target/debug/deps/game-1a2b)" },
    { "seq": 1, "kind": "running", "tests": 3 },
    { "seq": 2, "kind": "test", "name": "door::opens_near_player", "outcome": "ok" }
  ],
  "next": 3, "more": false, "dropped": 0, "result": null, "error": null
}
```

- `state` is `running`, `done`, `failed` or `cancelled`; `finished` is true for the last three.
- Pass the answer's `next` as `since` on the next poll to get only new events. One answer carries at
  most 512 events; `more: true` means poll again at once. A job keeps its last 4096 events;
  `dropped` counts older ones it let go.
- A finished job carries `result`, or `error: { reason, message }` when it failed.
- `id` is accepted in place of `job` (spec 23 writes the call `job_status(id)`).
- Without `job`, `agent.job_status` lists every in-process job (`jobs: [{ job, kind, label, state,
  fraction, message, started_ms, cancellable }]`) and the provider kinds. Finished jobs are kept for
  the 32 most recent.

`agent.job_cancel { job }` (tool `job_cancel`, permission `play`) asks a job to stop. A test run
kills its `cargo` process together with the compilers and test binaries it started (its process
group on Unix, its process tree on Windows) and ends `cancelled` with what it had, at the latest
5 seconds after the kill even if a stray process still holds its output open. `render_out:<n>`
forwards to `render.out_cancel`. A forwarded call needs the forwarded method's permission and tool
scope too (`render.out_cancel` needs `edit_scene`), so a job id reaches nothing its caller could not
call directly. Calling it on a finished job answers `requested: false` and the state.

## Running a test target: `agent.run_tests`

`agent.run_tests` (tool `run_tests`) runs `cargo test` for one target of the project in the
project root and answers a `tests:<n>` job:

```sh
regia agent call agent.run_tests '{"target": "gameplay", "filter": "door", "dry_run": true}'
```

<!-- not-a-request -->
```json
{ "dry_run": true, "command": ["cargo", "test", "--color", "never", "--test", "gameplay", "--offline", "--", "door"], "cwd": "/path/to/project" }
```

`cwd` is the project's root: run the command with `cwd` as the working directory.

| Param | Meaning |
|---|---|
| `target` | `lib`, `bins`, `doc`, `all` (the default), or a test target name (`tests/<name>.rs`). |
| `package` | A workspace package (default: the root package). |
| `filter` | Only tests whose path contains this. |
| `exact` | Match `filter` exactly. |
| `include_ignored` | Run `#[ignore]` tests too. |
| `offline` | Pass `--offline` (default true: protocol handlers do not use the network). |
| `dry_run` | Answer the command without running it. |

Names (`target`, `package`, `filter`) are 1..=128 ASCII letters, digits, `_`, `-`, `:` or `.`,
never starting with `-`, so a request cannot add flags. The run **compiles and executes project
code**, so the method needs the `build` permission, which the default profile and every tool scope
lack (specs/23 § 5): grant it in `.regia/agent.toml` only to agents you trust like a developer with
a shell. One test run at a time.

Events stream as the output arrives: `binary` (a test binary starts), `running { tests }`,
`test { name, outcome: ok | failed | ignored | bench }`, `summary { ok, passed, failed, ignored,
filtered_out }` per binary, and `error { message }` for cargo and compiler errors. Output that is
not UTF-8 is read with replacement characters, and a line over 64 KiB keeps its first 64 KiB. The result:

```json
{
  "ok": false, "exit_code": 101, "passed": 2, "failed": 1, "ignored": 0, "filtered_out": 4,
  "binaries": ["tests/gameplay.rs (target/debug/deps/gameplay-9f8e)"],
  "failures": [{ "name": "door_opens_near_player", "output": "thread 'door_opens_near_player' panicked at tests/gameplay.rs:41:5:\n..." }],
  "errors": ["error: test failed, to rerun pass `--test gameplay`"],
  "build_failed": false,
  "command": ["cargo", "test", "--color", "never", "--test", "gameplay", "--offline", "--", "door"]
}
```

`build_failed: true` means no test ran and cargo reported errors (read `errors`). A failure's
`output` keeps the first 16 KiB of what the test printed; the output of the first 64 failures is
kept.

A host that ships its own toolchain sets `regia_agent::run_tests::RunTestsProgram`; otherwise the
program is `$CARGO` (set when the app runs under cargo) or `cargo` on the `PATH`.

The run does not inherit the host's libtest harness variables (`RUST_TEST_NOCAPTURE`,
`RUST_TEST_THREADS`, any `RUST_TEST_*`): under `RUST_TEST_NOCAPTURE` libtest prints a failing test's
output as it happens instead of in the failures section, so every failure's `output` would be `null`
(ADR-7611). A host that wants one anyway sets it in `regia_agent::run_tests::RunTestsEnv`.

## Reasons

Errors carry `data.code` `F0002` (or `F0003` for an unknown job) and a lowercase `data.reason`:

| Reason | When |
|---|---|
| `job_id_invalid` | `job` is not `<kind>:<id>` with a lowercase kind. |
| `job_not_found` | No such job (finished jobs are kept for a while, then forgotten). |
| `job_not_cancellable` | The job kind has no cancel (`dcc_bake`). |
| `run_tests_bad_name` | `target`, `package` or `filter` breaks the name rule. |
| `run_tests_busy` | A test run is still going; poll it or cancel it. |
| `run_tests_no_manifest` | The project root has no `Cargo.toml`. |
| `run_tests_spawn_failed` | `cargo` could not be started (the job ends `failed` with this reason). |

`golden:<n>` jobs (`agent.golden_test`, [Golden images](goldens.md)) and `gen:<n>` jobs (`gen.run`, `gen.verify`,
[Generators](generators.md)) answer the same status and cancel. A test run under the protocol never sees
`REGIA_BLESS` or `REGIA_SKIP_GOLDENS`, in either spelling.
