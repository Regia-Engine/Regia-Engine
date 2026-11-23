# 41 — In-level functional tests and the multi-process session harness

Status: proposed (ADR-1581); built (ADR-1673, ADR-1953, ADR-2137, ADR-2755; conformance audit ADR-5637, § 13). Plan item G10, target v0.3.x (b); the multi-client part of the harness waits for `regia_net`
(v0.5.0) but its single-process and server-plus-client-stub forms ship first. Crates: `regia_framework::functional`
(runtime), `regia_agent` (surface), `regia_cli` (`regia test`), `regia_test_utils::functional` (helpers for `cargo test`).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Builders read this file plus the ambient rule docs (`docs/06-testing-and-ci.md`, spec 23, spec 28 for the log markers).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-tests/runtime-tests-functional`,
`runtime-tests/runtime-tests-session-harness` (both exist in the set). Re-derived in Regia terms; no wording,
identifiers, command names or switches carried over, and the names of the lifecycle steps, verdicts, driver hooks and
recovery options below are Regia's own.

## 1. Purpose, scope, baseline

Regia has cargo unit tests, golden images, `play.expect` / `play.wait_until` driven by an outside client, and soak runs. Missing:

1. **Functional tests that live in a level.** A test is an entity placed in a level with a lifecycle (setup, wait for ready, begin,
   conclude with a verdict), a frame limit, assertions, extra passes and automatic teardown. Playing the level runs it; a command runs
   every test of a level and reports pass or fail, so game teams test gameplay with the same level data the game ships, in CI,
   without an outside script holding the connection. `regia test` sets `REGIA_SESSION_RENDER=0`; a desktop build still opens its window (§ 2.8).
2. **A session harness.** One command launches the processes of a whole playthrough (one game, or a server plus clients), gives each its
   own agent endpoint, drives or watches them, gathers verdicts and logs, and says whether the run was clean.

**Baseline (implemented):** the agent protocol with `play.expect` and `play.wait_until` (`regia_agent`, spec 23 § 3.5); the agent server
binds `127.0.0.1` on a random port and advertises itself in `<project>/.regia/agent/endpoint.json` plus a `token` file
(`regia_agent::endpoint::{agent_dir, write, read}`); the soak/replay helpers and `unique_temp_dir` in `regia_test_utils`; the log
contract with panic/assert/fatal markers (spec 28); the settings registry behind the agent's settings methods. Not built: everything
in this spec (no `functional` module, no `regia test`, no session library).

Not in scope: unit-test and golden machinery (spec 06), network simulation (`regia_net`), hardware-farm scheduling, building the game
(the harness takes a finished build or a cargo target), visual scripting of tests, an editor test browser beyond a list panel.
`play.expect` and `play.wait_until` keep their meaning and grammar; they are reused as the **condition language** of script tests
(§ 2.3), so an agent session that works today becomes a saved test by copy.

## 2. Model

### 2.1 Nouns

| Term | Meaning |
|---|---|
| Functional test | An entity with a `FunctionalTest` component (and a script or a Rust body) placed in a level. |
| Run | One execution of one test; one or more passes. |
| Pass | Setup is once per run; the ready gate, begin, ticks and conclude happen per pass. |
| Verdict | `Pending`, `Active`, `Passed`, `Failed`, `Aborted`, `Undecided`. `Aborted`: the test could not run to a decision (ready gate timeout, caught panic, bad setting). `Undecided`: the pass ended without a verdict being stated. |
| Step | A named stretch of a test, for readable reports. |
| Runner | The resource that executes tests of loaded levels one at a time. |
| Session | One harness launch: a set of processes. |
| Role | One kind of process in a session: name, kind, count, arguments, optional driver. |

### 2.2 Component and lifecycle

```ron
"regia_framework::functional::FunctionalTest": (
    format_version: 1,
    enabled: true,
    description: "Door opens when the player is near",
    owner: "gameplay",
    tags: ["doors", "smoke"],
    frame_limit: 1800,                // 0 = none; fixed-step frames, never wall time
    ready_limit: 600,                 // frames the ready gate may take; 0 = none
    on_limit: Failed,                 // verdict applied when frame_limit runs out
    viewpoint: Some("<entity guid>"), // optional: the local camera starts at this entity's pose
    settings: [("render.quality", "low")],   // engine settings overridden for the run (§ 2.5)
    body: Script("tests/door_opens.functest.ron"),   // or Rust("my_game::DoorTest")
),
```

A run's transitions happen at one fixed point of the frame, after gameplay systems, so a test reads a consistent frame.

| Step | When | Rules |
|---|---|---|
| Setup | once, when the run opens | may spawn things, request streaming, connect; need not finish here |
| Ready gate | each frame until `ready` returns true | default ready at once; past `ready_limit` the run ends `Aborted` with `ready_timeout` and the body never begins |
| Begin | once per pass | the pass counts as started; ticks begin next frame |
| Tick | each frame while active | the body may `conclude` at any time |
| Conclude | once per pass | carries a verdict and a message; a second conclude in the same pass is ignored with a log line |
| Next pass | if extra passes are queued | below |
| Teardown | once after the last pass | always runs, also after `Aborted` or a limit |

- **Frame limit.** Counted from Begin, per pass. At the limit the pass concludes with `on_limit` and message "frame limit".
- **Extra passes.** A body may call `add_pass("label")`; labels collect in a sorted set. After a pass concludes, the runner starts a
  pass for the smallest label not yet run (`pass_label()` reads the active label, `None` for the first pass). `wants_pass_now()`
  is asked right after each conclude. The run's verdict is the worst pass (`Aborted` > `Failed` > `Undecided` > `Passed`); every
  pass's label, verdict and message appear in the report. At most 32 passes (`too_many_passes`).
- **No verdict** at conclude is `Undecided`: a test must decide.
- **Panics.** A body runs under `catch_unwind` (with `AssertUnwindSafe` at that one boundary; record the exception in the ADR). The
  shipping profile is `panic = "abort"`, so functional tests are dev/test only and compiled out of shipping (§ 9). A panic ends the run `Aborted` with its message.
- **Disabled** tests are listed and skipped; a skipped test is not a pass in the summary.
- **Teardown order**: the body's `teardown` hook, despawn of scoped entities (§ 2.4), restore of overridden settings, release of the
  viewpoint override. State a body leaves in resources or files is its own teardown duty: the next test must see a clean level.

### 2.3 Two ways to write the body

**Script (data).** `.functest.ron` reuses the `play.expect` / `play.wait_until` condition grammar and the input injection of the agent
protocol:

```ron
FuncTestScript(
    format_version: 1,
    steps: [
        (name: "walk to the door", do: [Inject(action: "Move", value: (0.0, 1.0), frames: 90)]),
        (name: "door opens", wait_until: Some((condition: (entity: (name: "Door"), component: "Door", path: "state", op: "eq", value: "Open"),
                                               within_frames: 120))),
        (name: "player is through", expect: [(name: "x", condition: (/* same grammar */))]),
    ],
)
```

A step runs its `do` actions, waits for `wait_until` (failing the pass with the step name at `within_frames`), then evaluates `expect`
in one frame. Every failing assertion is recorded, and so is a `do` action the input service refuses; the pass fails at the end of the step (ADR-5637). After the last step the pass is `Passed`.
No loops or parallelism in scripts (use a Rust body).

**Rust body.**

```rust
pub trait TestBody: Send + Sync + 'static {
    fn setup(&mut self, _cx: &mut TestContext) {}
    fn ready(&mut self, _cx: &mut TestContext) -> bool { true }
    fn begin(&mut self, cx: &mut TestContext);
    fn tick(&mut self, _cx: &mut TestContext) {}
    fn conclude(&mut self, _cx: &mut TestContext, _verdict: Verdict) {}   // per pass: reset state for the next
    fn teardown(&mut self, _cx: &mut TestContext) {}                     // once after the last pass
    fn wants_pass_now(&mut self, _cx: &TestContext) -> bool { false }
    fn summary(&self, _verdict: Verdict) -> Option<String> { None }      // extra report text
}
```

Bodies register by type path (`app.register_functional_test::<DoorTest>()`); the component's `Rust("my_game::DoorTest")` selects one.
One body can serve many placed tests, so a check is repeated across levels with different data on the placed entity.

### 2.4 Context and assertions

`TestContext`: `world()`, `conclude(verdict, msg)`, `begin_step(name)` / `end_step()`, `current_step()`, `note(text)`, `warn/error(text)`,
`frame()` (frames since Begin), `param(key)`, `rng()` (seeded), `spawn_scoped(bundle) -> Entity` (despawned at teardown),
`despawn_at_teardown(entity)`, `override_setting(key, value)` (reverted at teardown, also after a limit), `add_pass(label)`,
`pass_label()`, `expect_value(key, &impl Reflect)` (§ 2.6), `set_frame_limit(frames, on_limit)`.

Assertions return `bool`, record a failure message (expected, actual, optional label), never panic and never stop the body:

| Group | Checks |
|---|---|
| Tolerance equality | `f32`, `f64`, `Vec2/3/4`, `Quat` (angle in radians, `q` and `-q` equal), `Transform` (translation, rotation angle, scale), `Mat4` (element-wise) |
| Facing equality | two rotations equal when their forward directions agree within tolerance (stable near Euler singularities) |
| Exact equality | `bool`, integers, strings, entities, ordered lists of query hits |
| Relational | `f32`, `f64`, integers through a six-way selector (`Eq, Ne, Lt, Le, Gt, Ge`) |
| Truth and validity | `is_true`, `is_false`, `entity_alive(e)`, `entity_has::<C>(e)` |
| Condition | `condition(&json)` evaluates one `play.expect` condition in the current frame |

Failure text is deterministic (no addresses or timestamps), so reports diff cleanly.

### 2.5 Settings overrides and the viewpoint

`override_setting(key, value)` takes a settings-registry key (the registry behind the agent's settings methods) with a typed value
(string, bool, int, float). The previous value is stored once per key and restored at teardown however the run ended. An unknown key
fails setup with `setting_unknown`. When `viewpoint` is set the local camera moves to that entity's world pose at Begin and is restored at teardown.

### 2.6 Expected-value store

`expect_value(key, value)` compares a reflected value with `<level>.expected.ron` (test name + key -> RON value, `format_version: 1`).
A missing entry fails ("no expected value") unless the run used `--update-expected`, which writes the entry and passes it (marked
`blessed` in the report). The agent's `functest.bless` needs the owner-confirmation flow of spec 23 (same rule as image goldens).
A mismatch shows a field-path diff (reflection walk); floats compare at the test's tolerance (default 1e-5
relative to the absolute expected value; an expected 0 matches only an actual 0). A load error, including a
`format_version` newer than this build, aborts the run (exit 2) with a message that names the file and does
not rewrite it, including when `--update-expected` was set. A `reset_expected` flag on the placed test clears
that test's entries in the process's store and then clears the live component. It does not rewrite the level
file, so a committed `true` clears again on the next process. Files are written atomically (tmp + rename) and sorted.

### 2.7 Runner

- **Discovery.** Tests are the `FunctionalTest` entities of loaded levels, ordered by (level path, first tag, name, guid). `--filter`
  takes name globs, `--tag` tags (all-of). Tests run **one at a time**; the next starts after the previous teardown plus one settle frame.
  The world is not reset between tests; a test needing isolation loads its own level in setup, or tags itself `isolated`, which makes the runner reload the level before it.
- **Report.** `TestReport { build, seed, tests: [{ name, level, tags, verdict, message, passes: [{ label, verdict, message, frames }],
  steps: [{ name, frames, verdict }], frames, blessed }] }` as JSON and optional JUnit XML. No wall-clock values in the body (an optional
  `wall_ms` per test is excluded from equality in tests).
- **Looping.** `loop: true` repeats the set until aborted (soak use).
- **Determinism.** Fixed step, fixed seed (`--seed`, default 0): every limit and wait is a frame count.

### 2.8 Session harness (`regia test session`)

`<name>.session.ron`:

```ron
Session(
    format_version: 1,
    build: Cargo(package: "my_game", profile: "dev"),      // or Packaged(path: "dist/linux/my_game")
    roles: [
        (name: "server", kind: Server, count: 1, args: ["--level", "arena"], driver: None),
        (name: "client", kind: Client, count: 4, args: ["--connect", "{server}"], driver: Some("bot_walker")),
    ],
    tests: (filter: ["smoke*"], tags: []),                  // functional tests every role runs, or per-role lists
    script: [ (role: "client[0]", call: "play.wait_until", params: (/* ... */)) ],   // optional harness-driven agent calls
    limit_frames: 36000,
    max_restarts: 2,                                        // per session; default 2
    allow_errors: [],                                       // log ERROR lines that do not fail the session
    keep: (screenshots_on_failure: true, logs: true),   // a failed test or script call saves failure-<k>.png next to the logs (ADR-2755)
)
```

- **Launch.** For each role instance the harness spawns the build with `REGIA_SESSION_RENDER` set from the role's `render` (`1`/`0`;
  advisory: the harness does not pick the renderer, so a desktop build opens its window either way unless the game reads the variable
  or the role runs a `server`-feature build; CLI2 docs audit 1) and with its own **agent directory**: it sets `REGIA_AGENT_DIR=<session>/<role>-<n>/agent`, and the agent server writes
  `endpoint.json` and `token` there instead of under the project (new env override in `regia_agent::endpoint::agent_dir`;
  Decision ADR-1585: no `--agent-port-file`, since the server already binds a random port and publishes its endpoint). The harness polls
  for the file with a bounded attempt count (yielding, no fixed sleeps in tests), reads it with `endpoint::read`, then connects. Game
  network ports are assigned by the harness from the OS (bind port 0, release, pass the number); `{server}` expands to the server's address.
- **Order.** Roles start in listed order (list servers first when clients require them); the harness waits until a role's agent reports ready (its level loaded)
  before the next. A role exiting before ready fails the session with its exit code and log tail.
- **Driving.** Each process runs the functional tests; `script` lets the harness inject any agent call into any role and wait on conditions.
- **Drivers.** `driver` names a Rust `SessionDriver` registered in game code, for behavior a game does not script itself (a bot that
  walks while the server is observed): hooks `on_launch`, `before_level_load`, `after_level_load` (new level readable), `on_step`
  (each fixed step), and `finish(code)`, whose code becomes that process's result. Different instances may use different drivers.
  Drivers are compiled into the build like any plugin.
- **Verdict.** The session passes only if: every process exited 0 (or was stopped by the harness at the end), every reported functional
  test is `Passed`, no log line of level `ERROR` outside `allow_errors` appears, no panic/assert/fatal marker (spec 28) appears, and no
  crash bundle was written.
- **Artifacts.** `artifacts/<session>/<role>-<n>/`: stdout/stderr logs, the test report, crash bundles, a screenshot on failure (if
  the role renders), and either `session-result.json` (clean run only, so its existence is itself a signal) or `session-failure.json`.
  (Not `summary.json`: spec 28 uses that name for the host's per-session log record.)
- **Recovery.** `--recover` writes `session-progress.json` before starting and updates it after each completed test. When a process
  dies mid-run the harness restarts that role and skips tests recorded as complete; after `max_restarts` restarts the session ends
  `Aborted` with `too_many_restarts`.
- **Parameters.** `--param key=value` (repeatable) reaches tests through `cx.param`; role args may use `{param:key}`.
- **Exit codes** of `regia test` and `regia test session`: 0 all passed; 1 at least one `Failed`; 2 at least one `Aborted` or `Undecided`;
  3 no tests ran (an empty run is not a pass); 4 harness or launch failure.
- **Library.** The lower pieces (launch a process with its own agent directory, wait for ready, read logs) are `regia_cli::session`,
  with no role or test knowledge, reusable by soak and smoke tools.

## 3. API shape in Rust

```rust
// regia_framework::functional  (Gameplay layer)
#[derive(Component, Reflect)] pub struct FunctionalTest { /* § 2.2 fields */ }
pub enum TestBodyRef { Script(String /* project path */), Rust(String) }
pub enum Verdict { Pending, Active, Passed, Failed, Aborted, Undecided }
pub trait TestBody { /* § 2.3 */ }
pub struct TestContext<'w> { /* § 2.4 */ }
pub struct FunctionalTestPlugin;      // Runner + systems in `FunctionalTestSet` (after gameplay, before render extract)
#[derive(Resource)] pub struct Runner { /* queue, current run, report */ }
impl Runner {
    pub fn start(&mut self, selection: Selection, opts: RunOptions);
    pub fn status(&self) -> RunnerStatus;    // Idle, Running { test, pass, frame }, Done
    pub fn report(&self) -> &TestReport;
    pub fn abort(&mut self, reason: &str);
}
pub struct Selection { pub name_globs: Vec<String>, pub tags: Vec<String> }
pub struct RunOptions { pub seed: u64, pub update_expected: bool, pub looped: bool, pub params: BTreeMap<String, String> }
pub trait SessionDriver: Send + Sync { /* § 2.8 hooks */ }
// App extension methods: register_functional_test::<T: TestBody + Default>() and register_session_driver::<T>(name)

// regia_test_utils::functional (dev-dependency only)
pub fn run_level(app: &mut App, project: &Path, level: &str, selection: &Selection, options: RunOptions, max_frames: u32) -> Result<TestReport, TestError>;   // options.level / project_root are replaced   // headless, in process
// regia_cli::session
pub struct Session { /* parsed file */ }
pub fn launch(session: &Session, opts: &LaunchOptions) -> Result<SessionOutcome, SessionError>;
```

Layering: `regia_framework` and `regia_agent` are the same layer, and the edge `regia_agent -> regia_framework` already exists; the
agent glue reads `Runner` from `regia_agent`. `regia_framework` has no edge to `regia_agent`. `regia_test_utils` is dev-dependency
only; `regia_cli` is a tool. No new dependencies: `std::process` for launching, JUnit XML written by hand (escape `& < > " '`), globs reuse the cook's matcher.
Spans: `functional_tick` (only while a run is active), `functional_report`, `session_launch`.

## 4. Files and formats (all new, `format_version: 1`)

| File | Format | Notes |
|---|---|---|
| `FunctionalTest` component (inside `.level.ron`) | reflected, `format_version` 1 | `check-formats` name `functional-test`. A level with these loads in shipping builds but the component is ignored, and the cook strips the entities in shipping profiles (§ 9). |
| `*.functest.ron` | `FuncTestScript` | `check-formats` name `functest-script`. The condition grammar is the agent's: a change to it needs this format's version considered (record a note in `docs/formats/functest-ron.md`; Decision ADR-1585: pin it, since a saved script is the point). |
| `<level>.expected.ron` | v1 | atomic write, sorted keys. |
| `*.session.ron` | `Session` | `check-formats` name `session`. |
| `TestReport` JSON | v1 | schema `docs/formats/test-report.md`; JUnit is derived. |
| `session-progress.json`, `session-result.json`, `session-failure.json` | v1 | harness output, readable by CI. |

Frozen-format rule (spec 12 § 4a): each starts at 1; a later shape change bumps the version and adds a migrator plus `regia upgrade-project`
support; unknown newer versions are an error naming the file. The lead runs `cargo xtask check-formats --update` and `cargo xtask manual`.

## 5. Agent protocol (`book/agents/functional-tests.md`; `book/COVERAGE.md` row "Functional tests and session harness")

| Method | Class | Params | Result |
|---|---|---|---|
| `functest.list` | read | `{ filter?, tags? }` | `{ tests: [{ name, level, entity, tags, enabled, description, body }] }` |
| `functest.run` | play | `{ filter?, tags?, seed?, params?, update_expected?, looped?, level? }` | `{ started: n }` |
| `functest.status` | read | `{}` | `{ state, test?, pass?, frame?, done, total }` |
| `functest.results` | read | `{ since? }` | the `TestReport` so far |
| `functest.abort` | play | `{ reason? }` | `{ aborted }` |
| `functest.create` | edit | `{ name, level?, body, tags?, frame_limit?, at? }` | `{ entity }` (places a test; with `script` text it also writes the `.functest.ron`) |
| `functest.bless` | edit (owner confirmation) | `{ test, key? }` | `{ written }` |

Errors are `F0002` with `data.reason`, as the neighboring agent methods do: `no_runner` (plugin absent), `no_tests`, `already_running`,
`not_running`, `test_not_found`, `body_not_found`, `script_invalid`, `setting_unknown`, `ready_timeout`, `too_many_passes`,
`needs_confirmation`, `not_playing`, `bad_params`. The harness adds no agent method; each process offers the above. Without the plugin the
methods answer `no_runner`.

CLI: `regia test [PROJECT] --level L [--filter G] [--tag T] [--seed N] [--param k=v] [--report FILE] [--junit FILE] [--update-expected] [--loop]`;
`regia test session FILE [--recover] [--keep-artifacts] [--param k=v]`. Recipe for the guide: place a test with `functest.create`,
`functest.run`, `play.wait_until` on `functest.status.done`, read `functest.results`, make the test fail on purpose and confirm `Failed` with the step name and the failure text.

## 6. Editor surface (brief)

A "Tests" panel lists the open level's tests (tags, owner, last verdict), runs a selection in play-in-editor, shows the report with step
names and opens the placed entity. Placed tests draw a gizmo (box with the name) and a line to the viewpoint when set. The agent protocol is primary.

## 7. Determinism

Every limit and wait is a frame count; the runner uses the fixed step and a fixed seed. Two runs with the same level, seed and params
give equal reports (the optional wall time excluded). Test, pass and report order are sorted as stated. Harness runs are not bit-exact
(separate processes), but each process's report is deterministic and the session decides from verdicts, never timings. A test reading the
system clock or OS randomness is the game's responsibility (`cx.rng()` and the simulation clock exist).

## 8. Tests and acceptance (tests first)

No wall-clock asserts; sleeps are never used for synchronization (bounded polling with a yield, fake child processes where possible).
Temp paths from `regia_test_utils::unique_temp_dir`. No golden images (the screenshot-on-failure artifact is checked for existence on a GPU only).

Runtime:
1. Lifecycle order: a recording body proves setup once, ready polls until true, begin once per pass, teardown after the last pass.
2. Ready gate: `ready` false for 5 frames then true -> Begin on frame 6; with `ready_limit = 3` -> `Aborted` "ready_timeout", body never began, teardown ran.
3. Frame limit expires at frame N with `on_limit`; `0` never expires.
4. Verdicts: conclude with none -> `Undecided`; second conclude ignored; panic -> `Aborted` (test builds unwind).
5. Extra passes: two labels run in sorted order each reporting its label; `wants_pass_now` works; worst verdict wins; the 33rd pass -> `too_many_passes`.
6. Assertions: tolerance edges (inside/on/outside), `q`/`-q`, facing equality at a gimbal singularity, all six relational ops, deterministic failure text.
7. Teardown: scoped entities despawned, settings restored (also after limit and after panic), viewpoint restored, a failing test does not leak into the next (entity count).
8. Scripts: `wait_until` success and `within_frames` failure naming the step; two failing assertions both recorded; three-step script passes.
9. Expected store: missing entry fails; `--update-expected` writes and marks blessed; mismatch shows a field-path diff; tolerance; sorted atomic file; `reset_expected` clears once.
10. Runner: ordering, filter/tag selection, disabled tests listed as skipped (not passed), `isolated` reloads the level, an empty selection exits 3, exit-code mapping.
11. Report: JSON round trip; JUnit well-formed with special characters escaped; two runs equal.

Agent and CLI:
12. Every `functest.*` method round trip with its reasons, plus the § 5 recipe end to end (`regia_agent` integration test); `agent-coverage` and `check-formats` pass.
13. `regia test` on a sample level with two passing and one deliberately failing test exits 1 naming the failing step; only passing tests exits 0.

Harness:
14. Library: launching a fake child (a tiny test binary that writes `endpoint.json` into `REGIA_AGENT_DIR` and waits for stdin to close) gives a distinct
    agent directory per instance; four concurrent sessions in threads never collide; a child exiting before ready fails the session with its exit code and log tail.
15. Aggregation: a clean run writes `session-result.json` and no `session-failure.json`; a nonzero exit, a panic marker, an unallowed `ERROR` line and a crash bundle each fail the session.
16. Recovery: a fake child that dies once is restarted and completed tests are skipped; exceeding `max_restarts` ends `too_many_restarts`.
17. Drivers: hooks fire in order (`on_launch`, `before_level_load`, `after_level_load` with the new level readable, `on_step`, `finish` code becomes the result); two instances use different drivers.
18. Agent directory override: `REGIA_AGENT_DIR` makes `endpoint::agent_dir` return that path; unset keeps today's behavior (unit test in `regia_agent`).
19. Real game (lead's rigs): the arena sample's smoke session on Linux and Windows. *Status (ADR-2755):* `samples/arena/smoke.session.ron` starts `arena --smoke`. `render` is unset, so the harness sets `REGIA_SESSION_RENDER=0`, and the desktop binary still opens a window (§ 2.8). The in-process test `samples/arena/tests/smoke_test.rs` uses `MinimalPlugins` and opens no window. The Linux command is `regia test session samples/arena/smoke.session.ron`; the Windows run is the same command on the Windows rig (lead).
20. Android (needs a device): `regia test --platform android` over `adb` (agent port forwarded) runs the smoke level; commands recorded in the book page after the lead verifies.
21. Server plus clients: testable once `regia_net` exists; until then `count: 4` of a non-networked role proves launch, directories and ordering only.

Acceptance: a team places a test in a level, runs `regia test` in CI with exit codes and a JUnit report, repeats a body across levels, and runs a
multi-process session where one failing process fails the whole session with its logs kept.

## 9. Platform notes

- Linux, Windows: `std::process::Command`; on Windows children are stopped with `Child::kill` after a graceful quit request through the agent; no POSIX signals assumed; arguments with spaces and non-ASCII pass as separate strings.
- Android aarch64: no launching other roles on device; a device role is one game instance reached over `adb` port forwarding (`Remote(address)` in the session file, `address` being `127.0.0.1:<port>` or `attached`, the endpoint `regia install-android --attach` wrote; ADR-1953); functional tests run in-process on device as on desktop. The runner is dev-build only. As built (ADR-2137): one role of a session with a local build may carry `remote: Some(address)` (format v2), so a desktop server and a device client run as one session; `regia test --platform android [--address A]` runs the tests of the game already on the device without a session file.
- Shipping: `FunctionalTestPlugin` and `functest.*` exist only with cargo feature `functional-tests` (on through `regia`'s defaults and `agent`, off in a shipping profile with `no_default_features`, as for `agent`; ADR-1953); `regia package` strips `FunctionalTest` entities in the `shipping` profile (warning count, not an error) and leaves out `.functest.ron`, `.expected.ron` and `.session.ron`.
- macOS: testing-only, as Linux.

## 10. Budgets (informational; tests assert counts, not time)

Runner with a run active and an idle body: at most 0.05 ms per frame (`functional_tick`); no run and no tests: no systems scheduled (run condition);
an assertion call allocates nothing on success; building a 1,000-test report at most 20 ms; launching 5 processes to ready (excluding game load) at most 1 s.

Counting tests (built post-v0.5.0, lane post05-procgen-math-tests-k): `a_passing_assertion_allocates_nothing`
(the crate's `test_alloc` counter around passing `near_f32`/`is_true`/`equal` calls) and
`a_thousand_test_report_counts_every_test` (the summary, exit code, JSON round trip and the JUnit `<testcase>`
count all agree on 1,000). Wall-time budgets stay informational, per this section's rule.

## 11. Out of scope and follow-ups

Rendering-comparison assertions (use goldens), network fault injection (`regia_net`), a long-lived test server for fleets, parallel tests in one
process, a visual graph for test bodies, a fuller editor UI. Follow-ups: `repeat`/`parallel` in scripts, per-test counters in the report, sharding.

## 12. Provenance

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Framework / functional tests | Placed test entity with setup, ready gate, begin, conclude, frame limit, extra passes, tolerance assertions, teardown, expected-value store | Common xUnit lifecycle (setup/teardown fixtures) and tolerance comparison practice (Goldberg, "What every computer scientist should know about floating-point arithmetic", 1991); behavior-only read of the public-sources set area runtime-tests (ADR-1526). Names, verdicts, frame limits, script form and report are Regia's own. |
| Tools / session harness | Launching a process set, per-process drivers, artifact gathering, restart-and-continue with a cap | Process orchestration is standard (`std::process`); behavior-only read of the same set area. Agent-directory handshake, role file and exit codes are Regia's own. |
| Tools / test report | JUnit XML result layout | The de-facto JUnit XML result format (Apache Ant `junit` task report description). |

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. As built (ADR-5637)

The conformance audit of 2026-11-10 found the code refined § 5 in these places (the book page
`book/agents/functional-tests.md` already documents them):

- `functest.create` takes `{ name, body, script?, tags?, frame_limit?, at? }`: `body` is `script:<path>` / `rust:<type path>`
  or `{ script }` / `{ rust }`, and `script` text writes the `.functest.ron`. There is no `level` (the world has no per-entity
  level path, ADR-1673 (5)).
- `functest.status` adds `finished` (true once the run is done); `functest.results` adds `exit_code`.
- `functest.bless` takes `confirm: true` as the owner confirmation (ADR-1673 (6)); without it the answer is `needs_confirmation`.
- `.session.ron` is v2 for the optional per-role `remote` (ADR-2137) and roles take an optional `env` list (ADR-1673 (7)).

