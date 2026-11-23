# Staging the demo for a Steam upload

`cargo xtask steam-demo-upload` takes a finished demo package (`cargo xtask demo-package`, see
[the demo package page](../user-guide/demo-package.md)) and stages it into the folder layout steamcmd reads. It
re-checks every depot file's size and BLAKE3, writes the app and depot build scripts from the templates in
`steamworks/`, renders the store page text, and prints the exact steamcmd commands (ADR-4899). It never runs
steamcmd, never logs in, and never reads or stores a credential: the upload is the owner's step
(steamworks/STEAM-DEMO-KIT.md).

```sh
cargo xtask steam-demo-upload --app 4100000 --depot-win 4100001 --depot-linux 4100002 --dry-run
cargo xtask steam-demo-upload --app 4100000 --depot-win 4100001 --depot-linux 4100002 --dry-run --json
```

| Option | Meaning |
|---|---|
| `--app ID` | The Steam app id from the partner site (required). |
| `--depot-win ID` | The Windows x64 depot id (required). |
| `--depot-linux ID` | The Linux x64 depot id, also what the Steam Deck installs (required). |
| `--package DIR` | The demo package (default `target/demo-package`). |
| `--out DIR` | The staging folder (default `target/steam-demo-upload`). |
| `--dry-run` | Required: stage and print, never upload. Without it the command refuses. |
| `--json` | Print the result or the problems as one JSON object on stdout. |

Exit 0: staged. Exit 1: a problem; nothing is staged and an earlier staging folder is left as it was unless the
problem was found after the old folder was cleared (only `steam_stage_hash_mismatch`).

## What it checks

1. The ids: decimal numbers above 0, three different ones.
2. `demo-package.json`: kind `regia_demo_package`, profile `shipping`, `panic` `abort`, targets exactly `windows`
   and `linux`, none twice.
3. Each depot folder `depot/content/<platform>/`: no link or special file (checked without following links), no
   path naming Direct Order 27 content, a `dist-manifest.json` for the right target (`windows`, or `deck` for the
   Linux build) built with the `shipping` profile, and every file against it, as `cargo xtask dist --verify` does
   (sizes, BLAKE3, no unlisted file, licenses present, the binary checks).
4. Both depots are one engine version, and each depot names its build: its manifest's `git` is known (not
   `unknown`) and equals the id `demo-package.json` records for that target. The two ids may differ (the
   v0.2.20 Windows and Linux binaries were built from two commits of one engine version, ADR-5002); a package
   that records no per-target ids needs one id for both depots.
5. After copying, each staged depot is verified against its manifest again.

## What it writes

```text
<out>/
  content/windows/, content/linux/      the depot content roots
  scripts/app_build_<app>.vdf           the upload build ("Preview" "0")
  scripts/app_build_<app>_preview.vdf   steamcmd's preview of the same build ("Preview" "1"): checks, uploads nothing
  scripts/depot_build_<depot>.vdf       one per depot; LocalPath windows/* or linux/*; *.pdb / *.debug excluded
  output/                               steamcmd's build logs and cache
  store/short-description.txt, about.bbcode, system-requirements.txt, graphics.txt
  UPLOAD-COMMAND.txt                    the two commands in order
  steam-upload-plan.json                kind regia_steam_demo_upload, schema 1
```

Every build script has `"SetLive" ""`: an uploaded build is not live anywhere until the owner sets a branch on the
partner site. The build description is `<name> <engine version> (<git>)`, or
`<name> <engine version> (windows <rev>, linux <rev>)` when the depots' ids differ; the name is the single line of
`steamworks/DEMO_NAME.txt`, so renaming the demo is one edit there. The printed commands are

```text
steamcmd +login <your-steam-build-account> +run_app_build <out>/scripts/app_build_<app>_preview.vdf +quit
steamcmd +login <your-steam-build-account> +run_app_build <out>/scripts/app_build_<app>.vdf +quit
```

with the path shell-quoted when it needs it. steamcmd asks for the password and the Steam Guard code itself.

`steam-upload-plan.json`:

```json,ignore
{
  "kind": "regia_steam_demo_upload", "schema": 1,
  "demo_name": "Regia Forest Demo", "engine_version": "0.5.0", "git": "b394124ea",
  "app_id": 4100000,
  "depots": [
    { "platform": "windows", "depot_id": 4100001, "script": "scripts/depot_build_4100001.vdf",
      "content": "content/windows", "files": 9, "bytes": 151234567, "manifest_blake3": "...",
      "git": "b394124ea" },
    { "platform": "linux", "depot_id": 4100002, "...": "..." }
  ],
  "install_bytes": 151234567, "install_mb": { "windows": 152, "linux": 277 },
  "commands": { "preview": "steamcmd ...", "upload": "steamcmd ..." },
  "steamcmd_run": false, "credentials_stored": false
}
```

`--json` prints `{ "ok": true, "out": "...", "plan": { ... } }` or `{ "ok": false, "problems": [{ "code", "message" }] }`.

## Error codes

| Code | Meaning |
|---|---|
| `steam_upload_dry_run_required` | `--dry-run` was not passed; the command only stages. |
| `steam_id_invalid` | An id is not a decimal number above 0. |
| `steam_id_duplicate` | Two of the three ids are equal. |
| `steam_package_missing` | The package folder does not exist. |
| `steam_package_invalid` | `demo-package.json` or a `dist-manifest.json` is missing, not JSON, of another kind, not `panic = abort`, or names an unknown or repeated target. |
| `steam_package_target_missing` | The Windows or Linux build is not in the package. |
| `steam_package_not_shipping` | A depot was built with another profile than `shipping`. |
| `steam_package_special_file` | A depot holds a link or a special file. |
| `steam_package_do27_content` | A depot path names Direct Order 27 content. |
| `steam_package_hash_mismatch` | A depot file is missing, unlisted, or differs in size or BLAKE3 from its manifest. |
| `steam_package_mismatch` | The two depots are different engine versions, a depot's build id differs from the one `demo-package.json` records for it, or (no per-target ids recorded) the depots' ids differ. |
| `steam_package_git_unknown` | A depot's manifest records no build id (`unknown`): rebuild the package with `--git-windows` / `--git-linux`. |
| `steam_stage_out_not_empty` | `--out` holds other files and is not an earlier staging folder; it is left alone. |
| `steam_stage_out_overlaps` | `--out` is inside the package, or the package is inside `--out`. |
| `steam_stage_hash_mismatch` | A staged copy does not match its manifest (disk or copy fault). |

## Tests

`xtask/src/steam_demo_upload.rs` (`cargo test -p xtask --lib steam_demo_upload`): the VDF templates (both depots,
empty `SetLive`, preview flag, escaping), id checks, `--dry-run` required, staging a two-platform package built by
`dist` from header-only binaries (copies equal, re-verified, the Linux binary still executable, plan and commands),
tampered and unlisted files, missing targets and wrong profiles, Direct Order 27 paths, mixed builds, links, foreign
and overlapping output folders, the store texts (length, no comparisons, no other engine named, each platform's
own storage line, no claim the build does not show: night, rain, snow, the Steam Deck, full controller support),
per-depot build ids (`each_depot_carries_its_recorded_build_id`), the kit page's graphics table against the
code's list, and that the module never starts a process.

`store/system-requirements.txt` fills `{{INSTALL_MB_WINDOWS}}` and `{{INSTALL_MB_LINUX}}` from each depot's bytes
(decimal MB, rounded up), so each requirement block names its own install size.
