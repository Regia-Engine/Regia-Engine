# Launch standalone and on an Android device (`editor.launch`)

Play-in-editor runs the game inside the editor. A **launch** runs it the way a player does, as its
own process built from the open project (`docs/specs/19-editor.md` § 4, ADR-8980). The Output Log has
a launch bar at its top: pick **Standalone** or **Android device** (and a device), press **Launch
standalone** / **Launch on device**, and **Stop launch** ends it. While a launch builds or runs the
bar shows a spinner with the launch id, its state and the pid.

Every line the build, adb or the game writes goes to the Output Log under the target
`regia_editor::launch`, prefixed `[launch <id>]` (filter the log with that text to see one launch),
and to the launch's own tail, which `editor.launch_status` returns.

- **Standalone.** The project is cooked for the host platform (incrementally, the same cook
  `regia run` does), the launcher crate is built with
  `cargo build --manifest-path <project>/Cargo.toml -p <launcher> --message-format=json-render-diagnostics`
  (the binary is the executable cargo reports; `--release` when the editor is set to), and the binary
  runs in the project directory. It starts in the open level: a play copy of the level as the editor
  has it, unsaved edits included, passed in `REGIA_START_LEVEL` and removed when the launch ends.
  Live asset reload is off (`REGIA_ASSET_WATCH=0`); use `regia run --dev` for that.
- **Android.** `adb devices -l` lists the devices; with one usable device it is picked, with several
  you name one. `adb -s <serial> shell getprop ro.product.cpu.abi` reads the device's ABI, and
  `regia package --platform android --profile dev --format apk --json` builds the debug APK (only that
  ABI when `[platforms.android] abis` lists it). Then `adb install -r <apk>`,
  `adb shell am start -n <id>/.RegiaActivity`, `adb shell pidof <id>` and
  `adb logcat --pid <pid>`, which streams the app's log until Stop. The app starts in its packaged
  start level. Before logcat streams, the launch **attaches the agent protocol** to the app through
  its device transport (see "Device transport" below, ADR-8472): the app's agent server listens on the
  port the launch handed it, a host port is forwarded to it, and the host endpoint and token are in
  the launch's agent directory, `<project>/.regia/launch/agent-<id>`. `editor.launch_status` reports
  the host URL as `agent` (ADR-8594), and the editor's remote Outliner and Inspector reach the game on
  the device through it. From outside, point an agent client at that directory (`Client::connect`
  with the `endpoint.json` and `token` there).

`adb` is found the way `regia doctor` finds it: `$ANDROID_HOME`, `$ANDROID_SDK_ROOT` or the Android
Studio default SDK's `platform-tools/adb`, else `adb` on `PATH`. The `regia` CLI is `$REGIA_CLI`, the
`regia` next to the editor, or `regia` on `PATH` (as for the Bake runner).

**Stop** (`editor.launch_stop`) kills every child of the launch with its whole process tree and reaps
it before it answers: on Linux and macOS each child leads its own process group and the group is
killed (`killpg`); on Windows the children run in one job object with `KILL_ON_JOB_CLOSE`, and each
child is created suspended and resumed only once it is in the job, so nothing it starts escapes.
Once the launch has sent `am start`, an Android Stop also sends `adb shell am force-stop <id>`, and
logcat ends. A Stop during the build or the install stops cargo, the packager or `adb install` the
same way and force-stops nothing: an app with that id on the device is then an earlier install's,
not this launch's. Closing the editor stops the launch too.

The application id (`[project] id`) is checked against the Android package-name grammar before any
adb call: dot-separated segments of ASCII letters, digits and `_`, each starting with a letter, at
least two, no Java keyword (the rule `regia package` applies). `adb shell` joins its arguments into
one command line for the device's `sh`, so an id with anything else (`;`, `$( )`, quotes, spaces) is
refused with `launch_bad_app_id` and never reaches the device.

One launch runs at a time. A finished launch (stopped, exited or failed) stays readable until the
next one starts.

## `editor.launch`

Mutating, permission `build`. Params: `target` (`"standalone"`, the default, or `"android"`),
`device` (Android: an adb serial from `editor.launch_devices`; default the only usable device),
`wait` (wait up to 1 s for the launch to run or fail; default false), `timeout_ms` (with `wait`:
at most this long; default and maximum 1000, larger values are clamped). Returns the status plus
`pending`.

```json
{ "method": "editor.launch", "params": { "target": "android", "device": "54101FDAP0015H" } }
```

Refusals answer at once. The steps run in the background: the answer is the status with
`pending: true` (state `building`) while the launch has neither run nor ended, and
`editor.launch_status` follows it. The method runs on the editor thread, so `wait` holds the editor
for at most 1 s (ADR-7548): a cook and a cargo build take longer, and then the answer is
`pending: true` as without `wait`. A step that fails within the wait answers its code with the
failed step's last 40 output lines in `data.log_tail` (`editor.launch_status` shows the same error
when it fails later):

```json
{ "code": -32602, "message": "cargo build failed (exit status: 101)",
  "data": { "code": "launch_build_failed", "hint": "fix the build errors in log_tail and launch again",
            "log_tail": ["   Compiling hello v0.1.0", "error[E0425]: cannot find value `speed` in this scope", "..."] } }
```

## `editor.launch_status`

Read-only. Params: `tail` (output lines, default 40, at most 400). Returns
`{ id, target, device, state, running, step, steps, pid, exit_code, artifact, agent, error, lines, log_tail }`:

- `state`: `idle` (no launch yet), `building`, `installing`, `starting`, `running`, `stopped` (by the
  editor), `exited` (the game or logcat ended on its own; `exit_code`), `failed` (`error`).
- `steps`: the steps started, in order: standalone `cook`, `cargo build`, `run`; Android `package`,
  `install`, `start`.
- `pid`: the game's pid, or the app's pid on the device.
- `artifact`: the built binary or APK.
- `agent`: Android, while the device transport's forward is open: the app's agent server's
  forwarded host URL (`ws://127.0.0.1:<host port>`, the same port as `transport.host_port`); `null`
  before the forward opens, after the launch ended, and for standalone launches (the game writes its
  own endpoint into the same directory).
- `error`: `{ code, message, log_tail }` of a failed launch, else `null`.

## `editor.launch_stop`

Mutating, permission `build`. No params. Returns the status plus `stopped` (`false` when nothing was
building or running).

## `editor.launch_devices`

Read-only. No params. Returns `{ adb, devices: [{ serial, state, model, usable }] }` from
`adb devices -l`; only `usable` devices (state `device`) can be launched on, an `unauthorized` one
needs the prompt on the phone accepted.

## Error codes

| Code | When |
|---|---|
| `launch_no_project` | No project is open. |
| `launch_bad_target` | `target` is not `standalone` or `android`. |
| `launch_busy` | A launch is building or running; `editor.launch_stop` first. |
| `launch_no_adb` | adb was not found (no SDK platform-tools, nothing on `PATH`) or did not answer. |
| `launch_no_device` | No usable device is attached, or the named serial is not attached or not authorized. |
| `launch_device_ambiguous` | Several devices are attached and no `device` was named. |
| `launch_no_app_id` | `Regia.toml` has no `[project] id` (the Android application id). |
| `launch_bad_app_id` | `[project] id` is not an Android package name (letters, digits and `_` in dot-separated segments that start with a letter); it would reach the device's shell. |
| `launch_build_failed` | The cook, the cargo build or the APK packaging failed (`log_tail`). |
| `launch_install_failed` | `adb install` failed (`log_tail`). |
| `launch_start_failed` | `am start` refused, the app never showed a pid, or the game binary did not run. |
| `launch_spawn_failed` | A program of the launch (cargo, the CLI, adb) could not be run. |
| `launch_timeout` | Retired (ADR-7548), never answered: `wait` now ends after at most 1 s with `pending: true`. |
| `launch_hook_aborted` | An extension's before-launch hook aborted the launch (ADR-7791); the message names the hook, `log_tail` has its log lines. |

## Testing it without a device

`crates/regia_editor/tests/editor_launch.rs` puts a fake `adb`, a fake `regia` CLI, a fake `cargo` and
a fake game (shell scripts in a unique temp dir) first on the launch's `PATH`
(`Launcher::tools.path_env`) with the SDK lookup off (`SdkSource::None`), so no real device or build is
involved: it checks the order devices → package → install → start → logcat, the logcat and game lines
in the Output Log, that Stop ends logcat and the game's process group (their pids are gone), and
`launch_build_failed` with the log tail. Fakes that hold a step (`hold-<step>` files) check that Stop
during the build or the install kills that step's tree and force-stops nothing, that `wait` never
holds the editor while a build runs, and that ids with shell characters are refused with
`launch_bad_app_id` before any adb call. `android_launch_attaches_the_apps_agent_through_adb_forward`
starts a real loopback agent server with the token the launch handed the device, has the fake `adb
forward` answer its port, and calls it through the endpoint the launch wrote (ADR-8594, ADR-8631).

## Pushing asset changes into a standalone launch

While a standalone launch runs, every asset the editor's live cooker rebuilds (textures, meshes,
materials and the other cooked kinds, UI documents, engine shaders) is pushed into the game with
`assets.push` and swapped in place there; the bar shows **pushed N**. The game runs with its own agent
directory (`REGIA_AGENT_DIR=<project>/.regia/launch/agent-<id>`), removed when the launch ends.
`editor.launch_status` adds `pushed`, `push_failed` and `push_error`. See
[Asset push](asset-push.md) (ADR-7836).

## Device screen (Android)

With **Android device** chosen, the launch bar has a **Device screen** row: **Start mirror** shows the
device's screen under the bar, refreshed from `adb -s <serial> exec-out screencap -p` at most twice a
second (nothing is installed on the device; no new dependency). One screenshot takes about 600 ms on
the TEST Pixel 9 Pro (960x2142, measured 2026-11-15), so the picture refreshes about 1.5 times a second
there. A click on the picture taps the device there
(`adb -s <serial> shell input tap <x> <y>` in device pixels). The keyboard path is the **X** and **Y**
fields and **Send tap**. **Stop mirror** ends it. The mirror uses the device of the Android launch, or
the serial the agent names.

`editor.launch { target: "android", mirror: true }` launches and starts the mirror on the launch's
device (the answer adds `mirror` and `mirror_error`; `mirror_android_only` for another target).

`editor.device_mirror { action, device?, interval_ms?, x?, y? }`:

| `action` | Does |
|---|---|
| `status` (default) | `{ running, device, frames, taps, errors, last_error, width, height, interval_ms }`. |
| `start` | Starts (or restarts) the mirror on `device` or the Android launch's device; `interval_ms` 100..=5000 (default 500). |
| `stop` | Stops it; adds `stopped`. |
| `frame` | The status plus the newest screenshot as `png_base64` and its `seq`. |
| `tap` | Taps device pixel (`x`, `y`), which must lie inside the newest frame. |

| Code | When |
|---|---|
| `mirror_bad_action` | `action` is not start, stop, status, frame or tap. |
| `mirror_no_adb` | adb was not found. |
| `mirror_no_device` | No device was named and no Android launch has one. |
| `mirror_not_running` | `tap` or `frame` while the mirror is not running. |
| `mirror_no_frame` | `tap` or `frame` before the first screenshot arrived. |
| `mirror_bad_tap` | `x` or `y` is missing, not a whole number, or outside the newest frame. |
| `mirror_tap_failed` | `adb shell input tap` could not run or failed. |
| `mirror_android_only` | `editor.launch { mirror: true }` with a target other than `android`. |

The mirror shows the panel under the launch bar rather than in a dock tab of its own (a new tab needs
a panel entry in the editor shell; listed for a later wave in ADR-7836).

## Named launch profiles (P-342, ADR-7843)

The launch bar's **Profile** dropdown lists project recipes. **Quick launch** keeps the
original target/device controls. **Launch Profiles** opens a dialog to create, save and
delete recipes. Window → Launch Profiles opens the same dialog. Controls use the editor's
appearance scale and theme; Tab reaches the fields and buttons and Enter activates them.
A name edit followed by Save creates/replaces that exact name; it does not rename another
saved recipe. Delete removes the selected saved name.

`editor.launch_profiles` takes `op` (`list`, the default; `save`; `delete`), `profile`
(the complete object for save), `name` (exact name for delete), and optional `open`
(boolean, shows/hides the dialog). It requires `assets` permission. The result is
`{ format_version: 1, profiles: [...] }`; save and delete atomically replace
`config/launch_profiles.ron`. An absent file lists no recipes. Malformed or future-version
files are refused and never replaced. Names are case-sensitive, unique and at most
128 bytes; at most 256 profiles are supported. `launch_profiles_invalid` describes
invalid fields. No existing content format changes.

```json
{"method":"editor.launch_profiles","params":{"op":"save","profile":{
  "name":"Desktop review","target":"standalone","build_profile":"dev",
  "cook":"incremental","package":false,"device":null,
  "start_level":"levels/review.level.ron","extra_arguments":["--review","two words"]
}}}
```

Recipe fields:

| Field | Meaning |
|---|---|
| `name` | Display name and exact lookup key. |
| `target` | `standalone` or `android`. |
| `build_profile` | `dev`, `shipping`, or a project `[profiles.<name>]` build profile. Standalone builds use its base and feature flags; packaged builds use the existing CLI profile resolver. |
| `cook` | `incremental`, `full` or `skip`. Full forces an explicit recook. Skip omits the explicit cook; packaging still cooks required content through the existing CLI. |
| `package` | Standalone: build a directory package before running its binary. False cooks/builds/runs directly. Android currently requires true. |
| `device` | Android adb serial, or null to use the only usable device. |
| `start_level` | Path under `assets/`, or null to use the current launch behavior. Explicit source levels are retained after Stop. |
| `extra_arguments` | Array of application argument tokens; spaces and shell punctuation remain literal. No shell is used. |

`editor.launch { "profile": "Desktop review" }` re-reads the saved recipe and uses it.
Do not combine `profile` with `target` or `device`; that is `launch_profile_invalid`.
The original call without `profile` remains supported. A desktop package runs from the
published bundle directory. Android now passes `build_profile` to packaging instead of
always using `dev`.

**Android options (ADR-8472):** `start_level` and `extra_arguments` reach the device game in the
launch intent (see "Device transport" below): the game opens that level (read out of the APK's
assets) instead of its packaged startup level, and reads its arguments with
`regia_framework::dev_launch::launch_arguments(world)` (the command line on desktop). Package-off
Android is still refused: starting whatever APK the device has may run content older than the
project, and the fast-iterate dev APK that would define what is reused is not built. Such a recipe
can be stored, but launching it returns `launch_profile_unsupported` **before any adb call**; use
`package: true`.

## Device transport (P-301, P-302/P-303 device half, ADR-8472)

An Android launch starts the app with one intent string extra, `regia.dev_launch`, and a value of
only `0-9`, `a-z` and `.` (one plain word on the device shell): `v1`, then `l<hex>` (start level under
`assets/`, UTF-8 as hex), one `a<hex>` per argument, `p<port>` and `t<token>`. The token is made for
this launch only (64 hex digits) and the port is `47800 + id % 100` on the device's loopback. A **dev**
build's agent server binds that port with that token; a shipping build ignores the extra entirely.
When the app's pid is known the launch runs `adb -s <serial> forward tcp:0 tcp:<port>` and writes
`ws://127.0.0.1:<host port>` and the token into the launch's agent directory
(`<project>/.regia/launch/agent-<id>`). Through it:

- asset edits are pushed into the device game, cooked for `android` first (`book/agents/asset-push.md`);
- the remote Outliner and Inspector list the device game as source `device` (`book/agents/live-edit.md`).

`editor.launch_status` answers `transport: { host_port }` while the forward is open (else `null`).
Stop, or the app ending, removes the forward and the directory. A forward that fails is logged
("agent transport not opened") and the launch goes on without push or remote panels. The runtime
refuses a malformed extra and logs one of these codes (the launch then runs with no options):
`dev_launch_too_long`, `dev_launch_bad_version`, `dev_launch_bad_field`, `dev_launch_bad_hex`,
`dev_launch_bad_level`, `dev_launch_bad_arguments`, `dev_launch_bad_port`, `dev_launch_bad_token`,
`dev_launch_agent_incomplete`.

## Platforms (P-343)

Window → Platforms shows the selected platform's toolchain, SDK and other doctor checks.
**Re-check** starts a cancellable editor job; fixes are displayed as text and are never
executed. A failed check for the launch target shows a warning on the launch bar. Switching
projects invalidates cached results. macOS checks are for testing only.

`editor.platform_check` takes `op` (`status`, default; `recheck`), `platform` (for recheck:
`standalone` resolves to the host, or `linux`, `windows`, `android`, `macos`), and `open`
(show/hide the window). It requires `build` permission. The editor invokes
`regia doctor --json --platform <platform> --project <root>` with argument vectors.

The answer is `{ state, job, reports, error, open }`. `state` is `idle`, `running`, `done`
or `failed`; `reports` maps platforms to doctor schema 1 documents containing `checks`
(`section`, `name`, `status`, `code`, `detail`, `fix`). A doctor exit code of 1 still yields
a completed report with `ok: false`; a malformed document or tool failure is a failed job.
`job` is `platform_check:<n>`. Poll `editor.platform_check { op: "status" }` or
`agent.job_status { job }`; cancel with `agent.job_cancel { job }` / `editor.cancel_task
{ id: "platform_check" }`. A re-check replaces the selected platform's last result.
The worker bounds captured JSON to 8 MiB and commands to 60 seconds, and stops child
process trees on cancellation. No device deployment is performed by these checks.

## Size Report (P-344)

Window → Size Report shows actual stored byte totals by category and the largest files.
A successful editor package automatically schedules a report; packaged launches also
feed the report. Desktop directories list engine binaries, shaders, audio and asset kinds;
`.fpak` payloads use the pak manifest/type and stored entry size, with header/index bytes
accounted separately. The largest pak assets are listed separately from containing files.
The directory measurement is installed contents, **not compressed tar/AppImage size**.
For archives, the existing `apk-size` command supplies entry sizes and packaging warnings;
embedded paks remain one category (per-kind archive pak inspection is deferred).

`editor.package_size_report` takes `op` (`status`, default; `refresh`), `path` (optional
package directory, APK, AAB or ZIP for refresh; defaults to the previous input), and `open`
(show/hide the window). It returns `{ state, job, report, error, open, unused_features,
feature_telemetry }`. The report includes `path`, `measurement`, `content_bytes`,
`categories`, `largest`, `largest_assets` and `warnings`. Each largest entry has `path`,
`category` and `bytes`; category totals sum exactly to `content_bytes`. Ties sort by path.
No symlinks are followed in desktop bundles; more than 100,000 files/payloads are refused.
Refresh is an editor job (`package_size_report:<n>`), queried/cancelled through the same
job methods as platform checks. It needs `read` permission and obeys request path rules
(`path_outside_project` on an escaping path). Open/refresh are mutating panel operations,
so a read-only agent session refuses them along with the combined status method.

**Unused engine features:** `feature_telemetry` is currently `unavailable` and
`unused_features` is empty. The registered-plugin API has no complete feature/system
execution observations for recent play sessions. The panel explicitly says this; it never
mistakes missing data for inactivity or changes Cargo flags. Connecting this hint requires
the telemetry design recorded as deferred in ADR-7843.

## New method contracts and errors

All new error names below are stable lowercase codes in `error.data.code`; jobs expose
`platform_check_failed` / `package_size_failed` with the failure detail. Standard dispatcher
permission/type errors still apply before handlers. Profile writes are atomic, invalid
requests do not replace the file, and status calls do not start a process. Launch profile
execution retains the existing launch stop/process-tree contract.

| Code | Meaning |
|---|---|
| `launch_profiles_no_project` | No project is open. |
| `launch_profiles_invalid` | Bad profile fields, malformed RON, unsupported version or invalid parameter type. |
| `launch_profiles_io` | Atomic save/delete failed. |
| `launch_profiles_bad_op` | `op` is not list/save/delete. |
| `launch_profile_not_found` | No recipe has the exact requested name. |
| `launch_profile_invalid` | Profile execution could not resolve its build/start-level settings, or overrides conflict. |
| `launch_profile_unsupported` | A package-off Android profile (no artifact-reuse policy; ADR-8472). |
| `platform_no_project` | Re-check needs an open project. |
| `platform_bad_target` | Unknown platform. |
| `platform_bad_op` | `op` is not status/recheck. |
| `platform_bad_params` | Wrong parameter type. |
| `platform_busy` | A check is already running. |
| `platform_check_failed` | Worker/tool failure, timeout, malformed or unsupported doctor output. |
| `package_size_no_project` | Refresh needs an open project. |
| `package_size_no_package` | No previous input and no path was supplied. |
| `package_size_busy` | A report is already running. |
| `package_size_bad_params` | Invalid operation or parameter type. |
| `package_size_failed` | Cannot read/count the package, unsupported input, symlink, limit or tool failure. |
| `path_outside_project` | Request path leaves the allowed project/output roots. |

`tests/build_launch_tools.rs` drives all three routes through the dispatcher.
`tests/editor_launch.rs` records fake child arguments and proves package/build selection,
argument boundaries, retained source levels and early Android refusals. Unit tests in each
panel press Tab and Enter at normal and enlarged scale without a window or golden.
