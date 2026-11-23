# Game-code hot reload (agents)

The editor started with `--game <crate>` (windowed or `--headless`, an editor built with
`--features hot-dylib`) hosts the game crate as a dylib and swaps in a rebuilt copy without a
restart, keeping reflected state (`docs/specs/20-hot-reload.md`, user guide:
[Hot reload](../user-guide/hot-reload.md)). An agent drives it with the protocol method
`agent.hot_reload` and the editor method `editor.code_status` (specs/23 § 3.6, ADR-5627). There is one
reload method: the editor's former `editor.hot_reload` was folded into `agent.hot_reload` (ADR-5993).

| Method | MCP tool | Permission | What it does |
|---|---|---|---|
| `agent.hot_reload` | `hot_reload` | build | Asks for a rebuild and swap, like the toolbar's **Compile** button (Ctrl+Alt+F11), as a job: answers `{ job }` at once and the job finishes with the reload's outcome ([agent.hot_reload](code.md#agenthot_reload)). |
| `editor.code_status` | `code_status` | read | The toolbar's game-code state: `{ game, state, label, compiling_s, error, toast, requested }`. |

`state` is `idle`, `compiling`, `error` or `reloaded`. `compiling_s` is set while compiling, `error`
holds the compiler output or the refusal reason in the `error` state, and `toast` the reload line
(`Reloaded in 1.3 s (12 types, 340 instances)`) in the `reloaded` state. `requested` is true from the
`agent.hot_reload` call until the editor loop starts the build on its next frame.

## Reload and wait

The build runs in the background, and the swap happens on the editor loop when it finishes. Wait on
the job (`agent.job_status`), or poll `editor.code_status` (or use `wait_until`), never a timer:

```sh
regia agent call agent.hot_reload '{}'
# → {"job":"hot:1"}
regia agent call editor.code_status '{}'
# → {"state":"compiling","compiling_s":3.2,"requested":false,...}
# → {"state":"reloaded","toast":"Reloaded in 1.3 s (12 types, 340 instances)",...}
```

A request made while a build runs is kept: another build starts when the running one finishes,
because the running one read older sources.

## Errors and refusals

| Code / state | When | What to do |
|---|---|---|
| `hot_reload_no_game` (`data.reason`) | The editor hosts no game crate. | Start the editor with `--game <crate>`. |
| `hot_reload_busy` (`data.reason`) | A build runs or an earlier `agent.hot_reload` job is still waiting. | Wait for the job, then call again. |
| `state: error`, `rebuild the editor with --features hot-dylib …` | The editor links the engine statically, so a game dylib would be a second engine copy. | Build the editor with `--features hot-dylib` and `-C prefer-dynamic`. |
| `state: error`, compiler output in `error` | The game crate did not build. The old code keeps running. | Fix the code and call `agent.hot_reload` again. |
| `state: error`, `engine changed — restart the editor` | The engine dylib was relinked since the editor started (by the game build or by any other cargo run on the same target dir). Every later reload is refused too. | Restart the editor (rebuild the editor and the game together). |
| `state: error`, `rebuild the editor`, ABI hash text | The game was built by a different toolchain, version, target or profile. | Rebuild the editor. |
| `state: error`, `identity check refused reload for …` | A rebuilt game type kept its `TypeId` (an unsalted build). The previous plugin is rolled back on. | Rebuild through the editor (it salts every build). |
| `state: error`, `restore panicked …` | Restore panicked after teardown. The world has no game types; the snapshot is kept at `.regia/hot/last_snapshot.bin`. | Fix the code and call `agent.hot_reload` again: the retry restores from that snapshot. |

From the 50th mapped library on, the reload toast suggests a restart (each reload leaks the old
library, about 2.2 MB).

## Files

`.regia/hot/` holds the salted copies (`game-<N>-<pid>-<serial>.<ext>`), `last_snapshot.bin` (RON) and
`status.json` (`{ generation, accepted, old_code_running, phase, toast, elapsed_ms, playing, paused }`,
written atomically after every attempt). Creating an empty file named `trigger` there also requests a
rebuild, for tools that have no protocol connection.
