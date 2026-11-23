# Cook and package from the protocol

The spec 23 § 3.7 pipeline methods `cook(platform, quality)` and `package(platform, profile)`
run the [`regia` command line](../user-guide/packaging.md) as a child process in the project
root and answer a job (see [Jobs](jobs.md)), like [`agent.build_game`](code.md). They run
project build scripts and the asset pipeline with the app's privileges, so both need the
**`build` permission** (specs/23 § 5), which the default profile lacks. `agent.package` also
needs `confirm: true`: packaging builds and writes a shippable game, so the owner agrees first.

Both are desktop-only: on Android they answer `F0013` with reason `cook_desktop_only` (the
command line and the asset pipeline are not on the device). Cook and package on the desktop,
then install with `regia install-android`.

The program is `regia` from the app's `PATH`, or `$REGIA_CLI`, or the host's
`regia_agent::cook_package::RegiaCliProgram` resource.

## `agent.cook`

`agent.cook` (MCP tool `cook`) runs `regia cook --platform P --quality Q --json` and answers a
`cook:<n>` job:

```sh
regia agent call agent.cook '{"platform": "linux", "quality": "fast", "dry_run": true}'
```

<!-- not-a-request -->
```json
{ "dry_run": true, "command": ["regia", "cook", "--platform", "linux", "--quality", "fast", "--json"], "cwd": "/path/to/project" }
```

`cwd` is the project's root: the command has no `--project` flag, so run it with `cwd` as
the working directory.

| Param | Meaning |
|---|---|
| `platform` | `linux`, `windows` or `android` (default: the host's). |
| `quality` | `final` (default: compressed paks, the review gate) or `fast` (uncompressed). |
| `clean` | Delete the platform's cooked output first and cook everything. |
| `dry_run` | Answer the command without running it. |

## `agent.package`

`agent.package` (MCP tool `package`) runs `regia package --platform P --profile R --json` and
answers a `package:<n>` job:

```sh
regia agent call agent.package '{"platform": "linux", "profile": "shipping", "confirm": true}'
```

| Param | Meaning |
|---|---|
| `platform` | `linux`, `windows` or `android` (default: the host's). |
| `profile` | `shipping` (default), `dev`, or a `[profiles.<name>]` entry of `Regia.toml`. |
| `container` | Linux only: build inside a pinned build container, `steam-sniper` (glibc 2.31) or `debian12` (glibc 2.36); adds `--container` (see [Build containers](#build-containers)). |
| `confirm` | Must be `true`; without it the call answers `needs_confirmation`. |
| `dry_run` | Answer the command without running it (no confirmation needed). |

`profile` is 1..=128 ASCII letters, digits, `_`, `-`, `:` or `.`, never starting with `-`, so
a request cannot add flags.

### Build containers

`container` builds the Linux binary inside a container whose glibc is the oldest the players
have (C-26, ADR-8835), the same as `regia package --container` ([Packaging](../user-guide/packaging.md#build-containers)):

```sh
regia agent call agent.package '{"platform": "linux", "profile": "shipping", "container": "steam-sniper", "dry_run": true}'
```

<!-- not-a-request -->
```json
{ "dry_run": true, "command": ["regia", "package", "--platform", "linux", "--profile", "shipping", "--json", "--container", "steam-sniper"], "cwd": "/path/to/project" }
```

Any other value answers `package_bad_container` before a process starts. A container package
that fails reports one of these lowercase codes in `report.error.code` (the job's result):

| `report.error.code` | When |
|---|---|
| `container_platform_unsupported` | `container` with a platform other than `linux` |
| `container_host_unsupported` | the machine running the job is not Linux |
| `container_tool_missing` | neither podman nor docker (nor `REGIA_CONTAINER_TOOL`) is on `PATH` |
| `container_tool_unknown` | `REGIA_CONTAINER_TOOL` is not `podman` or `docker` |
| `container_image_unpinned` | `--container-image` can move (`:latest`, no tag, no digest) |
| `container_image_missing` | `--container-image` names a local image that is not present |
| `container_glibc_conflict` | `--glibc` disagrees with the profile, or `--steam` with `debian12` |
| `container_lockfile_missing` | the workspace has no `Cargo.lock` (sources are mounted read-only) |
| `container_mount_missing` | a `--container-mount` directory does not exist |
| `container_build_failed` | the build in the container (or the toolchain image build) failed |

The staged binary then passes the usual glibc floor check: `build_info.json` records
`glibc_floor` (the profile's glibc) and `glibc_max` (the newest `GLIBC_` version the binary
needs, read from its ELF version table).

## Progress and result

Only one pipeline job runs at a time: a cook and a package share the cooked cache and the paks,
so a second call while one runs answers `cook_busy`. Poll with `agent.job_status`; stop with
`agent.job_cancel` (the cancel stops the whole process group).

Each human output line of the command (its stderr) is streamed as a `line { text }` event and
becomes the job's progress message; the command's one `--json` result object (its stdout,
specs/21 § 1) is streamed as `report { ok }` and kept. The job's result:

<!-- not-a-request -->
```json
{
  "ok": true,
  "exit_code": 0,
  "cancelled": false,
  "artifact": "build/paks/linux",
  "report": { "ok": true, "platform": "linux", "quality": "fast", "pak_dir": "build/paks/linux", "paks": ["build/paks/linux/base.fpak"] },
  "tail": ["cook: importing assets/props/crate.glb", "pak: wrote 1 pak file(s) to build/paks/linux"],
  "lines": 2,
  "command": ["regia", "cook", "--platform", "linux", "--quality", "fast", "--json"]
}
```

`ok` is true when the command exited 0 and its report did not say `"ok": false`. `artifact` is
the report's `artifact` (the package path of `regia package`) or its `pak_dir` (the pak
directory of `regia cook`). `report` is the command's whole `--json` object (null when it
printed none); `tail` keeps the last 64 output lines. A failed command's report is
`{ "ok": false, "error": { "code", "message" } }` (the CLI's lowercase error code), so read
`report.error` when `ok` is false.

Source diagnostics run before cooked output is published (ADR-8390). A success object also
has `lint`: `status` (`skipped`, `clean`, `warnings` or `failed`) and `report` (the evidence
path). A source-lint stop adds the same `lint` object beside `error`. Read
`report.lint.status` and `report.lint.report` from `agent.job_status`. `content_lint_cancelled`
is one JSON error and a nonzero exit, not a partial success report. The scan never repairs
sources during cook or package. Geometry warnings stop `cook: Fail` and package; material
warnings are reported and do not by themselves stop those steps. `play.expect` and
`play.wait_until` still check session state after the job; they do not open the lint file.
See [Source lint policy](content-lint.md).

## Reasons

| Reason | Code | When |
|---|---|---|
| `cook_bad_platform` | F0002 | `platform` is not `linux`, `windows` or `android` |
| `cook_bad_quality` | F0002 | `quality` is not `fast` or `final` (`agent.cook`) |
| `package_bad_profile` | F0002 | `profile` is outside the name alphabet (`agent.package`) |
| `package_bad_container` | F0002 | `container` is not `steam-sniper` or `debian12` (`agent.package`) |
| `needs_confirmation` | F0002 | `agent.package` without `confirm: true` |
| `cook_busy` | F0002 | a cook or package job is still running |
| `cook_no_manifest` | F0002 | the project root has no `Regia.toml` |
| `cook_desktop_only` | F0013 | called on Android |
| `cook_spawn_failed` | job error | `regia` could not be started (not on `PATH`, no `REGIA_CLI`) |

A call without the `build` permission answers `F0012` before any of these.

## Editor package size reports

After an editor package finishes, **Window → Size Report** reports the published contents
by category and largest file. `editor.package_size_report` uses `op: "status"` to query,
`op: "refresh"` to recount an optional `path` (directory, APK, AAB or ZIP), and `open`
to show/hide the panel. Refresh returns a `package_size_report:<n>` job; poll the same
method or `agent.job_status`, and cancel via `agent.job_cancel`.

Desktop directory totals are installed bytes; archive totals use stored entries and
archive overhead. Desktop pak payloads are broken down by asset kind; archive paks remain
whole entries. Categories add to `content_bytes`. The report does not change packaging
or strip engine features. Plugin execution telemetry is unavailable at this base, so the
unused-feature list is empty with an explicit explanation. See [Build and launch tools](editor-launch.md#size-report-p-344)
for the complete result contract, limits and lowercase error codes (`package_size_no_project`,
`package_size_no_package`, `package_size_busy`, `package_size_bad_params`,
`package_size_failed`, `path_outside_project`).

## Package hygiene

Shipping jobs enforce the project's `[package_hygiene]` policy before archiving. The optional
`agent.package` parameter `audit_dir` audits an existing project-relative extracted package
without building it. The same permission/confirmation rules apply; `dry_run` previews the
command. Read `result.report.hygiene` and `result.report.error` from `agent.job_status`.
`hygiene.complete`, `incomplete_reason`, `limits`, `peak_memory_bytes` and `work` say whether
the audit finished and which ceilings were in force. A policy failure has `complete: true`
and findings; `complete: false` means the audit stopped early.
After archiving, a shipping job also audits the finished artifact and writes its receipt
beside it; `result.report.final_audit` carries that receipt with `verified`. The optional
parameter `audit_artifact` audits an existing project-relative finished package file or
directory in place, with no build and no writes; its receipt is `result.report.final_audit`.
See [Package hygiene](package-hygiene.md) for policy, redaction, limits and failure codes.
Stage versus final evidence, receipt fields and the archive error codes are in
[Final-artifact audit and receipts](package-hygiene.md#final-artifact-audit-and-receipts).
