# Functional tests: place a test in a level, run it, read the verdict

A **functional test** is an entity with a `FunctionalTest` component placed in a level. Playing the level can run it; `regia test` runs
every test of a level and exits with a code CI understands, so a team tests gameplay with the level data the game ships, without an
outside script holding the connection. The desktop game still opens its window: `regia test` sets `REGIA_SESSION_RENDER=0`, and that
variable does not choose a renderer (spec 41 § 2.8). Everything is counted in fixed-step frames: no limit, wait or order depends on a clock.

Without `regia_framework::functional::FunctionalTestPlugin` (dev and test builds only) every `functest.*` method answers `F0002` with
`data.reason: "no_runner"`. Formats: `FunctionalTest` (`docs/formats/functional-test.md`), `.functest.ron` (`docs/formats/functest-ron.md`),
`<level>.expected.ron` (`docs/formats/expected-ron.md`), `TestReport` (`docs/formats/test-report.md`).

## Lifecycle

| Step | When | Rules |
|---|---|---|
| Setup | once, when the run opens | settings overrides apply first |
| Ready gate | each frame until `ready` returns true | past `ready_limit` the run ends `Aborted` with `ready_timeout` and the body never begins |
| Begin | once per pass | ticks begin next frame |
| Tick | each frame while active | the body may conclude at any time |
| Conclude | once per pass | a verdict and a message; a second conclude in the same pass is ignored |
| Next pass | if passes are queued | labels run in sorted order; at most 32 passes (`too_many_passes`) |
| Teardown | once after the last pass, always | body hook, scoped entities, restored settings, restored viewpoint |

Verdicts: `Pending`, `Active`, `Passed`, `Failed`, `Aborted` (could not run to a decision: ready timeout, panic, bad setting or body, too many
passes), `Undecided` (the pass ended without a verdict). A run's verdict is its worst pass. Assertions never panic: they record a failure
(deterministic text, with the step name in brackets) and the pass ends `Failed` even if the body concluded `Passed`. A panicking body ends `Aborted`
with `panic: <text>` and still tears down.

## The recipe

```json
functest.create { "name": "doomed", "body": "script:tests/doomed.functest.ron", "tags": ["demo"],
                  "script": "(format_version: 1, steps: [(name: \"never\", wait_until: Some((condition: (resource: \"Score\", path: \"n\", op: \"gte\", value: 999), within_frames: 30)))])" }
→ { "entity": 4294967301 }
functest.run { "tags": ["demo"] }         → { "started": 1 }
play.wait_until { "condition": ... }       // or poll functest.status until "finished": true
functest.status {}                         → { "state": "done", "done": 1, "total": 1, "finished": true }
functest.results {}                        → { "tests": [ { "name": "doomed", "verdict": "Failed",
                                                "message": "step `never` failed; [never] wait_until not satisfied within 30 frames: ...",
                                                "steps": [ { "name": "never", "verdict": "Failed" } ] } ], "exit_code": 1 }
```

Make the test fail on purpose and confirm `Failed` with the step name and the failure text before trusting a green run.

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `functest.list` | read | `{ filter?, tags? }` | `{ tests: [{ name, level, entity, tags, enabled, description, body }] }` |
| `functest.run` | play | `{ filter?, tags?, seed?, params?, update_expected?, looped?, level? }` | `{ started }` |
| `functest.status` | read | `{}` | `{ state, test?, pass?, frame?, done, total, finished }` |
| `functest.results` | read | `{ since? }` | the `TestReport` so far plus `exit_code`. The test that is running is not in the report until it closes. While the run is in progress and no finished test has a verdict (the report is empty, or holds only skipped tests), `exit_code` is 3 even though `functest.status` has `state: "running"` and `finished: false`. 3 means no verdicts yet, the same code as a selection that ran nothing. |
| `functest.abort` | play | `{ reason? }` | `{ aborted }` |
| `functest.create` | edit scene | `{ name, body, script?, tags?, frame_limit?, at? }` | `{ entity }` |
| `functest.bless` | edit scene | `{ test, key?, confirm }` | `{ written }` |

`filter` is a name glob (`*`, `?`) or an array (any-of); `tags` must all be present. Disabled tests are listed and skipped; a skipped test is not a pass.
`level` is a project-relative path (`levels/a.level.ron`); `regia test --level` sends that path. An empty `level` runs the tests already in the world. A `.expected.ron` this build cannot read aborts the run and is not rewritten.
`functest.bless` writes the values a test observed in the last run into its `.expected.ron`; without `confirm: true` (given after the owner reviewed the
diff) it answers `needs_confirmation`.

## Errors

`F0002` with `data.reason`: `no_runner` (plugin absent), `no_tests` (nothing enabled matches), `already_running`, `not_running`, `test_not_found`,
`script_invalid` (the `.functest.ron` does not parse), `needs_confirmation`, `bad_params`, and `not_playing` (reserved for editor hosts whose world is not
in play mode; the in-game agent never raises it). `body_not_found`, `setting_unknown`, `ready_timeout` and `too_many_passes` end a run `Aborted` and appear
as the test's `message` in `functest.results` (the message starts with the reason).

## Writing the body

**Script** (`.functest.ron`): steps with `do` (input injection), `wait_until` (the `play.wait_until` condition and a `within_frames` count) and `expect`
(`play.expect` conditions evaluated in one frame). A failed expectation, or a `do` action the input service refuses, fails the pass at
the end of its step with "step `<name>` failed". See the format page.

**Rust**: implement `regia_framework::functional::TestBody` (`setup`, `ready`, `begin`, `tick`, `conclude`, `teardown`, `wants_pass_now`, `summary`),
register it with `app.register_functional_test::<MyTest>()`, and select it with `body: Rust("my_game::MyTest")`. Through `TestContext`: assertions
(tolerance equality for floats, vectors, quaternions, transforms and matrices; facing equality; exact; the six relations; entity checks;
`condition`), `begin_step`/`end_step`, `spawn_scoped`, `override_setting`, `add_pass`, `param`, a seeded `rng`, and `expect_value` against the
level's expected-value file.

## The command line

```
regia test [PROJECT] --level L [--filter G]... [--tag T]... [--seed N] [--param k=v]... [--report FILE] [--junit FILE] [--update-expected] [--loop] [--exe PATH]
regia test [PROJECT] --platform android [--address attached|127.0.0.1:PORT] [--filter G]... [--tag T]... [--seed N] [--param k=v]... [--report FILE] [--junit FILE]
regia test session FILE [--recover] [--keep-artifacts] [--param k=v]... [--exe PATH]
```

Exit codes of both: **0** all passed, **1** at least one `Failed`, **2** at least one `Aborted` or `Undecided` (or a session aborted),
**3** no tests ran (an empty run is not a pass), **4** harness or launch failure. `regia test` starts the project's game on the level
(`REGIA_START_LEVEL`) with `REGIA_SESSION_RENDER=0`. A desktop build still opens its window: the variable does not select a renderer
(spec 41 § 2.8). The command runs the tests through `functest.*` and writes the JSON report and JUnit XML.

`--platform android` (ADR-2137) starts nothing: it runs the tests of the game already running on the attached device, through the
endpoint `regia install-android --attach` wrote (`--address attached`, the default) or a port you forwarded (`127.0.0.1:<port>`), with
the same report files and exit codes. The device runs the level it was started with, so `--level`, `--exe` and `--loop` are refused there.
Without the agent files the command says to run `regia install-android --attach` first and exits 4.

```sh
regia install-android --attach            # forward the agent port, write .regia/agent/endpoint.json and token
regia test --platform android --filter "smoke*" --junit device.xml
```

## Sessions

`<name>.session.ron` (format `Session`, v2; v1 files read unchanged) names the roles of a playthrough: `build` (`Cargo`, `Packaged` or `Remote`),
`roles` (name, kind `Server`/`Client`/`Game`, count, args, driver, render, env, and since v2 `remote`), the `tests` each role runs, optional harness `script` calls, `limit_frames`,
`max_restarts` (default 2), `allow_errors` and `keep`. The harness starts roles in listed order, each with its own agent directory
(`REGIA_AGENT_DIR=<artifacts>/<session>/<role>-<n>/agent`), waits for the agent, runs the tests, then stops each process (stdin closes; a process that
ignores that is killed). In role arguments `{server}` is the first server's address, `{port}` the instance's own OS-assigned port and `{param:key}` a
`--param`.

The session **passes** only if every process exited 0 (or was stopped by the harness at the end), every reported test is `Passed`, no log `ERROR` line outside
`allow_errors` appears, no panic/assert/fatal marker appears (`panicked at`, `assertion failed`, `FATAL`, `fatal runtime error`) and no crash bundle
(a `crash/` folder with files, or a `.dmp`) was written. Artifacts are `<artifacts>/<session>/<role>-<n>/` (stdout and stderr logs, the agent folder) and either
`session-result.json` (clean run only) or `session-failure.json`. With `--recover` the harness writes `session-progress.json`, restarts a role whose process
died and skips tests recorded as complete; after `max_restarts` the session ends `Aborted` with `too_many_restarts`.

`limit_frames` reaches every role as `REGIA_SESSION_LIMIT_FRAMES` (only when above 0; a role's own `env` entry wins); the harness itself does not enforce it.

**`keep.screenshots_on_failure`** (ADR-2755). When true, each failed step of a rendering role (`render: true`) saves a screenshot through `seeing.screenshot` while the process still runs: a test whose verdict is not `Passed` (`test:<name>`) or a failed `script` call (`script:<method>`). The PNG is `<artifacts>/<session>/<role>-<n>/failure-<k>.png` next to the logs, and the outcome (`session-failure.json`, the `processes[].screenshots` list) names it: `{ step, file, reason }`. A role that does not render, no display, or a failed screenshot call saves nothing and records the `reason`; it never fails the run by itself (the session's `reasons` stay the test's own). `false` (the default) asks for no screenshot. A passing session takes none.

**The arena smoke session** (spec 41 test 19, ADR-2755): `samples/arena/smoke.session.ron` starts `arena --smoke --bots 3 --seed 7` (the binary installs the functional-test runner and one placed test, `arena_smoke`, tagged `smoke`: the match is in warm-up or the round, its clock advances and pawns exist after 90 frames). `render` is unset, so the harness sets `REGIA_SESSION_RENDER=0`, and the arena binary still opens a window (`regia::run_with`). The in-process test `samples/arena/tests/smoke_test.rs` uses `MinimalPlugins` and opens none:

```sh
regia test session samples/arena/smoke.session.ron        # Linux (lavapipe or no GPU) and Windows alike
```

The device half is spec 41 test 20 (`regia test --platform android`, above).

A game picks up its driver from `REGIA_SESSION_DRIVER` (`regia_framework::functional::SessionDriverPlugin`; hooks `on_launch`, `before_level_load`,
`after_level_load`, `on_step`, `finish(code)`).

## The viewpoint

A test's `viewpoint` is the guid of an entity. At the first Begin the local camera takes that entity's world pose; teardown
puts it back however the run ended. The local camera is the window camera with the lowest `order`, else the lowest-order
camera of any target. A camera driven by a rig or a driver stays at the viewpoint during the run; another camera stays where
the test moves it. A guid nothing resolves, a world without a camera or a target without a transform ends the test `Aborted`
with `bad_params`, and the body never begins. A project may replace the service with `TestHooks::viewpoint`.

## Shipping builds

`FunctionalTestPlugin`, `functest.*` and `regia test` exist with the cargo feature `functional-tests` of `regia`. It is on by
default and with `agent`, so a named shipping profile with `no_default_features` leaves it out, and `ship_check` refuses a
game that turns it on there unless the profile says `allow_agent`. A body's panic cannot be caught under `panic = "abort"`.

`regia package --profile shipping` leaves the `FunctionalTest` entities and their descendants out of every level, with a
warning count, and the `.functest.ron`, `.expected.ron` and `.session.ron` files out of the package. Other profiles keep them.

## The Tests panel

The editor's Tests panel lists the open level's tests with tags, owner and last verdict, ticks a selection, runs it in
play-in-editor (starting play when stopped) and shows the report with its step names. A click selects the placed entity.
Placed tests draw a box with the name and a line to the viewpoint. The `functest.*` methods stay the primary surface; these
read and drive the panel a human sees:

| Method | Class | Params | Result |
|---|---|---|---|
| `editor.tests_panel_state` | read | none | `{ tests: [{ name, tags, owner, description, enabled, entity, verdict, chosen }], running, queued, status, report }` |
| `editor.tests_panel_run` | play, mutating | `tests?` | the state, plus `started` |
| `editor.tests_panel_open` | edit_scene, mutating | `test` | `{ entity }` |

`no_tests` when nothing enabled is selected, `already_running` during a run, `test_not_found` for an unknown name, and a
`status` starting `no_runner` when the play session has none. A run ends with the play session: the status says so.

## Device instances

`build: Remote(address: "attached")` drives the one game already running on a device. First run
`regia install-android --attach`: it forwards a port and writes `.regia/agent/endpoint.json` and `token`. `attached` uses that
endpoint; `127.0.0.1:<port>` connects to a port you forwarded yourself, with the same token. Any other address is refused, because
the token only goes to the loopback address. The session lists one role with `count: 1`.

The harness runs the session's script calls and tests over that connection. The process appears in the outcome with `remote: true`
and `exit_code: null`. A lost connection fails the session; there is no restart.

**A device role in a local session** (format v2, ADR-2137). With a local build (`Cargo` or `Packaged`), one role may carry
`remote: Some("attached")` (or `Some("127.0.0.1:<port>")`): that role is the game on the device, the others start on this machine as
usual. Roles still start in listed order, so a desktop server comes up before the device client is reached. The device role has
`count: 1`, gets no arguments and no agent directory (it is already running), is never restarted, and appears in the outcome with
`remote: true`; script calls address it by name like any role. A session file that uses `remote` must say `format_version: 2`; a v1
harness refuses v2 files, so it can never start the device role as a local process.

```ron
Session(
    format_version: 2,
    build: Cargo(package: "my_game", profile: "dev"),
    roles: [
        (name: "server", kind: Server, args: ["--server", "--port", "{port}"]),
        (name: "phone", kind: Client, remote: Some("attached")),
    ],
    tests: (filter: ["smoke*"], tags: []),
    script: [ (role: "phone", call: "play.pause") ],
)
```

The device does not learn the desktop server's address from the harness: connect it from the game (a menu, a saved address) or with a
script call to a method of your game. With `adb reverse tcp:<port> tcp:<port>` the phone reaches a desktop port as its own loopback.

## Limits of this build

A `Remote` build or role starts, stops and restarts nothing and reads no device logs. At most one role per session is a device (the token
comes from the one attached device). Server-plus-client tests wait for `regia_net`; until then `count: 4` of a non-networked role proves launch, folders and order only.

Cargo builds must identify one distinct binary executable of the exact package. Build-script artifacts and package-name substrings in unrelated paths never qualify; multiple binary paths are refused. A reported `Aborted` or `Undecided` test makes the session status `aborted` and exits 2, including when other results failed.

A nonzero session `limit_frames` is forwarded as `REGIA_SESSION_LIMIT_FRAMES` for local roles and as the string parameter `regia_session_limit_frames` in `functest.run` for local and remote roles. This is an informational hint, not an enforced cap; an explicit role environment entry or functional-test parameter can override its copy.

With `keep.screenshots_on_failure: true`, the harness asks live roles with `render: true` for `seeing.screenshot` before stopping them and atomically writes `<artifacts>/<session>/<role>-<index>/failure.png`. Remote roles are captured without being stopped. A capture failure logs `session_screenshot_failed` and preserves the original verdict and exit code.
