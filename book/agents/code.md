# Code: builds and hot reload

The spec 23 § 3.6 code methods drive the project's cargo build from the protocol and ask the
editor to hot-reload game code. Both compile project code with the app's privileges, so both
need the **`build` permission** (specs/23 § 5), which the default profile and every tool scope
lack: grant it in `.regia/agent.toml` only to agents you trust like a developer with a shell.

## `agent.build_game`

`agent.build_game` (MCP tool `build_game`) runs `cargo build --message-format=json` in the
project root and answers a `build:<n>` job (see [Jobs](jobs.md)):

```sh
regia agent call agent.build_game '{"profile": "release", "dry_run": true}'
```

<!-- not-a-request -->
```json
{ "dry_run": true, "command": ["cargo", "build", "--message-format=json", "--release", "--offline"], "cwd": "/path/to/project" }
```

`cwd` is the project's root: run the command with `cwd` as the working directory.

| Param | Meaning |
|---|---|
| `profile` | `debug` (the default), `release`, or a custom cargo profile name. |
| `package` | A workspace package (default: the root package). |
| `offline` | Pass `--offline` (default true: protocol handlers do not use the network). |
| `dry_run` | Answer the command without running it. |

`profile` and `package` are 1..=128 ASCII letters, digits, `_`, `-`, `:` or `.`, never starting
with `-`, so a request cannot add flags. While a build runs, a second `agent.build_game` is
refused with `build_game_busy`; poll the running job with `agent.job_status` or stop it with
`agent.job_cancel` (the cancel kills the `cargo` process group like a test run's).

Events stream as cargo reports them: `diagnostic { level, message, code?, file?, line?,
column?, suggestion? }`, `artifact { name }` per compiled crate, and `build-finished {
success }`. The result carries the structured diagnostics the spec asks for (cargo JSON: file,
line, message, suggestion):

<!-- not-a-request -->
```json
{
  "ok": false, "exit_code": 101, "profile": "release",
  "diagnostics": [
    { "level": "error", "message": "mismatched types", "code": "E0308",
      "file": "src/lib.rs", "line": 3, "column": 9, "suggestion": "1u32" },
    { "level": "warning", "message": "unused variable `x`",
      "file": "src/lib.rs", "line": 2, "column": 5, "code": null, "suggestion": null }
  ],
  "error_count": 1, "warning_count": 1,
  "artifacts": [], "raw_tail": [], "dropped_diagnostics": 0,
  "command": ["cargo", "build", "--message-format=json", "--release", "--offline"]
}
```

The primary span's location is `file`/`line`/`column`; the first machine-applicable replacement
rustc offers is `suggestion`. `message`, `file` and `suggestion` are each cut to 4 KiB. At most 256
diagnostics are kept (`dropped_diagnostics` counts the rest); `error_count` and `warning_count`
count every diagnostic cargo reported, the dropped ones too. Output lines that are not cargo JSON (a wrapper script's own text) are kept in `raw_tail`
(64 lines) so a failed spawn or a broken toolchain is still visible. A host that ships its own
toolchain sets `regia_agent::run_tests::RunTestsProgram`, the same override `agent.run_tests`
uses.

## `agent.hot_reload`

`agent.hot_reload` (MCP tool `hot_reload`) asks the **running editor** to rebuild the game crate
and swap the new code in — the editor's Compile action (Ctrl+Alt+F11), driven from the protocol.
Outside the editor it answers `editor_not_running`; while a reload runs it answers
`hot_reload_busy`. It answers a `hot:<n>` job:

```sh
regia agent call agent.hot_reload '{}'
```

```json
{ "job": "hot:1" }
```

The editor loop picks the request up on its next frame, compiles on its worker thread and swaps
when the build finishes; the job's progress message reports the wait and the result carries the
reload's structured outcome:

```json
{
  "accepted": true, "phase": "running", "generation": 12,
  "detail": "Reloaded in 1.3 s (12 types, 340 instances)",
  "toast": "Reloaded in 1.3 s (12 types, 340 instances)",
  "elapsed_ms": 1300, "types": 12, "instances": 340,
  "scripts_rebuilt": 3, "scripts_dropped": 0
}
```

`scripts_rebuilt` counts the game's script slots (`Scripts` on an entity) that the reload built
again through the new library: their next tick runs the new code, with their reflected fields
carried over. `scripts_dropped` counts slots whose script type the new library no longer
registers; they are removed and the editor log has a warning naming the type and the count. Both
are `0` when the reload did not swap. The same two numbers are in `.regia/hot/status.json` for an
agent that reads the file instead of the job.

`phase` is `running` (the new code is in), `build_failed` (cargo failed; the old code runs),
`refused` (a precondition: no `--game` host, a static `std`, the engine dylib missing, an ABI or
identity check) or `restore_failed` (the snapshot file is intact; retry the reload). `dry_run:
true` answers what would happen without asking for a build.

`agent.job_cancel` on a `hot:<n>` job ends it on the editor's next frame with state `cancelled`
and frees `agent.hot_reload` for the next request. It stops the waiting, not a build the editor
already started: that build still runs, and swaps the new code in when it succeeds (the toolbar
shows its outcome). If the editor stops hosting the game crate while a job waits, the job fails
with `hot_reload_no_game`; if the build ends without publishing a report, it fails with
`hot_reload_no_report`.

The editor answers `hot_reload_no_game` when it hosts no game crate (it was started without
`--game`). While the editor is already compiling (a file change or the Compile button got there
first), the call is refused with `hot_reload_busy` — poll the job you already have, or the
editor's status, instead of queueing a second build.

## Reasons

Errors carry `data.code` `F0002` and a lowercase `data.reason`:

| Reason | When |
|---|---|
| `build_game_bad_name` | `profile` or `package` breaks the name rule. |
| `build_game_busy` | A build is still going; poll it or cancel it. |
| `build_game_no_manifest` | The project root has no `Cargo.toml`. |
| `build_game_spawn_failed` | `cargo` could not be started (the job ends `failed` with this reason). |
| `editor_not_running` | `agent.hot_reload` outside the editor. |
| `hot_reload_busy` | A reload job runs, or the editor is already compiling. |
| `hot_reload_no_game` | The editor hosts no game crate (`--game`); also a pending job's failure when the host went away. |
| `hot_reload_no_report` | The editor's build ended without a reload report (the job ends `failed` with this reason; check the editor log). |
