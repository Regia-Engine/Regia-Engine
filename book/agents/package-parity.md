# Package parity

`package.parity` compares one dev access trace with one package access trace.
Call it with read permission. Paths may sit outside the project: the method
only reads the four paths you pass. It does not launch a process. `regia package
parity` is the command that can launch both sides.

```json
{"method":"package.parity","params":{"dev_trace":"dev.log","dev_root":".","package_trace":"pkg.log","package_root":"out/pkg"}}
```

## Parameters

- `dev_trace`: access-trace file from the dev run.
- `dev_root`: directory those dev paths are under.
- `package_trace`: access-trace file from the package run.
- `package_root`: extracted package directory.
- `dry_run`: when true, the result is the plan and no file is read. The four
  paths may be omitted.

A trace line is `ok open <path>` or the runtime's debug-quoted form
`ok open "<path>"`. A file is needed only when the dev run opened it under
`dev_root`. The package accounts for it when the package run opened or stat'd
that relative path, or when a cooked entry stands in for the source.

## Result

`ok` is false and `exit_code` is 1 when `missing` is not empty. That is a
successful RPC: the comparison ran, and the gate failed. `missing` lists dev
opens the package did not account for. `drift`, `dieted` and `cooked` are
reported and do not fail the gate. `never_touched` lists package files neither
run touched; that list does not fail the gate. `lines` is the classifier text.
`markdown` is the same report.

A cooked `.gltf` or `.bin` counts as present when the package run read the
cooked entry the bake index names, or when the source file is in the package
and the cook index covers its folder. A `.bin` whose sibling `.gltf` the
package run touched, and which is itself in the package, is cooked. Drift is a
dev file newer than `staged_at` in the package `build-info.toml`. Pass
`--strict-drift` on the command to fail on drift as well. The agent method
does not take that flag.

## Errors

- `parity_bad_params`: a required path is missing and `dry_run` is not true.
- `parity_not_a_directory`: `dev_root` or `package_root` is not a directory.
- `parity_trace_missing`: `dev_trace` or `package_trace` is not an existing file.

A wrong JSON type, including a string `dry_run`, is the protocol `invalid_params`
error. These three codes are the method's own.

## Command

`regia package parity` accepts the same four paths for an offline diff, or
`--dev-command` and `--package-command` with `--script`. The command sets
`REGIA_ASSET_TRACE`, `REGIA_PARITY_ROOT` and `REGIA_PARITY_SCRIPT` for each
child. `--report-md` and `--report-json` write the report. `--json` prints one
JSON object. `--classify` reads strace text instead (`--trace`, `--root`,
`--manifest`).

Two command exits are not agent codes: `parity_script_missing` when `--script`
is not a file, and `parity_run_failed` when a child exits non-zero. Both use
process exit 2. Exit 1 is a missing dev open. Exit 0 is a clean gate.

The command does not capture stills or compare pixels. The scripted path is
the agent script you pass; a separate play-gate track is not required.

Validation: `cargo test -p regia_asset --lib package_parity` and
`cargo test -p regia_agent --test package_parity`.
