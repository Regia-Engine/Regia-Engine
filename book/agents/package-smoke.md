# Package smoke

`cargo xtask package-smoke` is the packaging acceptance run (ADR-3968, spec 21 § 6). It packages a game, launches
the package, plays a short scripted input recording, and checks the session log. The user-guide steps are in
[Packaging](../user-guide/packaging.md#smoke-test-a-package-before-shipping).

## What it runs

By default, on Linux:

1. Package `samples/sim_bridge` and a `regia new` third-person project, each at the dev profile and the shipping
   profile (`tar`).
2. Extract the archive and launch the game under `xvfb` with lavapipe.
3. Play `samples/sim_bridge/smoke/playback.frec` or
   `crates/regia_templates/templates/third_person/smoke/playback.frec`.
4. Wait for the first update and all recorded steps to complete, then run for 60 more seconds and exit.
5. Fail if the JSON log contains an `ERROR` record, if playback did not start, or if `summary.json` is missing,
   partial, not a clean exit, or reports a GPU, device-lost, surface, or panic count above zero.

`--android` also builds the sim_bridge APK and AAB for the ABIs in the project manifest (arm64-v8a and x86_64)
and records the sizes. A shipping APK at or above 150,000,000 bytes fails. A dev APK over that size is recorded and does not fail.
`--android-emulator` installs the dev APK on an x86_64 API 35 emulator (swiftshader, headless) when `/dev/kvm`
can be opened read-write, plays for 60 seconds, and fails on a crash, an error line, missing playback, or a missing smoke-exit marker in that process's logcat.
`--require-kvm` makes a missing KVM device a failure. `--dry-run` prints the emulator commands without building packages or requiring KVM.
`--skip-linux` skips the Linux packages (the `android-smoke` CI job).
`--skip-third-person` focuses Linux acceptance on sim_bridge; the default and CI still run both projects.

`--windows` on Windows packages and runs the sim_bridge zip at both profiles. On Linux it prints the commands
for the owner's Windows a test machine and exits 0. The report is `<out>/package-smoke.json` (`format_version` 1).
That file is a CI report, not a content format, and `cargo xtask check-formats` does not hash it.

## Playback switch

The sim_bridge sample and third-person template install `regia_framework::startup_playback::StartupPlaybackPlugin`.
Other games enable the switches by adding that plugin to their app. A game with the plugin plays a `.frec` at startup when asked. Both dev and shipping builds honor it.
A missing file is a warning and the game continues.

| Switch | Meaning |
|---|---|
| `--play-input PATH` or `--play-input=PATH` | Play this file. The flag wins over the environment variable. A flag with no path is ignored, with a warning. |
| `REGIA_PLAY_INPUT` | Same path, when the flag is absent. Empty is ignored. |
| `REGIA_SMOKE_SECONDS` | Exit cleanly this many seconds after startup playback has completed. Empty or `0` does not exit. A non-integer is ignored, with a warning. |
| `REGIA_JSON_LOG` | Write the spec 28 JSON-lines log to this path. Empty is ignored. `summary.json` is written beside it as `<stem>.summary.json`. |

On Android an activity does not inherit those. The emulator smoke copies the recording to
`files/regia-play-input.frec`, touches `files/regia-play-input.on`, and writes the seconds into
`files/regia-smoke-seconds`. A launch without the marker file does not warn.

The log line `playing input recording` (target `regia_framework`) confirms playback started.
`input playback complete; starting smoke timer` confirms all requested steps were consumed before the timed run.
Both markers are required; startup shader compilation does not count toward the 60 seconds. `input recording missing; playback skipped`
and `input recording rejected; playback skipped` are failures for a smoke run.

## Re-record

The committed recordings are 32 steps, seed 1, 60 Hz, with no keyframes. They are produced by the repository,
not captured from a play session. Playback ends and the game stays up until the smoke-seconds limit. From the
workspace root:

```sh
scripts/write-package-smoke-recordings.sh
```

That runs `cargo xtask package-smoke --write-recordings`.

## CI

The `package` job runs `cargo xtask package-smoke` (zig 0.14.1, so the shipping profile can link against glibc
2.31). The `android-smoke` job runs
`cargo xtask package-smoke --skip-linux --android --android-emulator --require-kvm` with KVM enabled and the
system image `system-images;android-35;google_apis;x86_64`.

## Existing agent surface

In a dev build, `demo.inspect` with `{ "path": "smoke/playback.frec" }` reads the recording's header,
actions, and step count when that file exists under the running app's project root. An extracted package need not include the source fixture; its startup playback path can
point outside the agent's allowed roots, so use the log markers to inspect that packaged run. `logs.read` with `{ "target": "regia_framework", "level": "info" }` exposes
`playing input recording` and the smoke-exit marker. `telemetry.get_last_summary` with
`{ "session_summary": true }` exposes the live session summary. These existing methods support inspection;
startup playback itself uses `InputRecorder`, while `demo.play` controls the separate `DemoDriver`.
See [Demo recording](demo.md) and [Telemetry](telemetry.md). Shipping acceptance uses the JSON log and summary.
