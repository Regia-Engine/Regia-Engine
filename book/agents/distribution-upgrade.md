# Distribution and upgrade (agents)

How a game developer gets a Regia Engine release, pins a project to it and moves the project to the next release
(spec 94, ADR-1852). These are `regia` commands, not protocol methods: each prints one JSON document with `--json`
(schema 1), for an agent or CI that installs an engine, checks a copy or upgrades a project. The running app's
`app.info` reports its engine version as `engine`.

```console
$ regia engine info --project my_game --json          # engine version, line, window; the project's requirement and pin
$ regia engine package --out /srv/releases            # writes /srv/releases/regia-engine-0.2.5/
$ regia engine verify /srv/releases/regia-engine-0.2.5 --json
$ regia new "My Game" --engine-source path:/srv/releases/regia-engine-0.2.5
$ regia upgrade-project --project my_game --engine path:/srv/releases/regia-engine-0.3.0 --json   # dry run
$ regia upgrade-project --project my_game --engine path:/srv/releases/regia-engine-0.3.0 --write
```

Any command whose document has an `error` problem exits non-zero after printing it, so `regia engine verify DIR &&
…` stops.

## Version policy

A version is `MAJOR.MINOR.PATCH`. Its **line** is `0.MINOR` before 1.0 and `MAJOR` after (Cargo's caret rule, so a
git or registry pin resolves the same way). A project names the line it targets in `Regia.toml`:

```toml
[project]
engine = "0.2"      # or "0.2.5": that line, at least that patch
```

| Requirement vs this engine | `status` | `upgrade-project` |
|---|---|---|
| same line, engine at least the required version | `current` | runs the migrators |
| older line, inside the window | `upgrade_needed` | `--write` stamps this engine's line |
| no `engine` key | `unpinned` | as `upgrade_needed` from the oldest line |
| older than the window (`oldest_upgradable_line`, today `0.1`) | `upgrade_unsupported` | refuses; upgrade with an older engine first |
| newer line, or a newer patch of this line | `engine_too_old` | refuses; never downgrades |

Patch releases may add a content-format version with its migrator: run `regia upgrade-project --write` after every
engine update (it is a no-op when nothing changed). Read the release notes of every line in `lines_crossed`
(`release_notes` lists their files): they name every host-visible API change.

Run `upgrade-project` with the `regia` of the engine you upgrade **to**: its migrators are the new ones. Build it from
the release: `cargo install --path regia-engine-<version>/crates/regia_cli`.

## Engine sources

| Form | Written into each engine dependency |
|---|---|
| `path:/abs/regia-engine-0.2.5` (a release or a checkout) | `path = "/abs/regia-engine-0.2.5/crates/<crate>"` |
| `path:vendor/regia-engine-0.2.5` (a copy inside the project) | `path = "../vendor/…/crates/<crate>"`, relative to each manifest |
| `git:https://host/regia.git#v0.2.5` | `git = "…", tag = "v0.2.5"` |
| `registry:studio@0.2.5` (`@version` defaults to this engine) | `version = "=0.2.5", registry = "studio"` |

Engine dependencies are the keys `regia` and `regia_*` (except packages the project defines itself, such as a game
called `regia_arena`) in any `*dependencies` table, including
`[workspace.dependencies]` and `[target.'…'.dependencies]`, written as one-line inline tables. Only the source keys
change (`path`, `git`, `tag`, `branch`, `rev`, `version`, `registry`); `optional`, `features`, `default-features`, the
order of the rest and a trailing comment stay. `regia = { workspace = true }` lines are left alone (the workspace
table carries the source). Directories that hold an engine (a `regia-engine.release.ron` or a
`crates/regia/Cargo.toml`), `target/` and `.git*` are never read or rewritten, and `upgrade-project` does not scan
them for content files either.

## Cargo.lock

A project without a lockfile resolves third-party crates to their newest compatible versions, not the ones the
release was tested with (measured on v0.2.5: 64 of 660 packages of a fresh `first_person` project). `regia new`
therefore copies the engine's `Cargo.lock` into the project (from a `path:` source, or from the checkout `regia` was
built from); Cargo keeps those versions and adds the game's own crates. `upgrade-project --write --engine path:DIR`
seeds a project that has no lock, and reports `engine_lock_differs` with a count when the project's lock has drifted
from `DIR/Cargo.lock`; it never overwrites an existing lock (copy it by hand to take the engine's versions; the game's
own dependencies then re-resolve). Git and registry sources carry no local lockfile.

## Release directory

`regia engine package` copies the tracked files of the checkout (`git ls-files`; a walk that skips `.git` and
`target/` outside git) into `<out>/regia-engine-<version>/` and writes `regia-engine.release.ron`:

```ron
(
    format_version: 1,
    version: "0.2.5",
    commit: Some("28fb09309…"),
    rust_version: "1.98",
    crates: ["regia", "regia_2d", …],
    files: [(path: "Cargo.toml", size: 6144, blake3: "9f…"), …],
)
```

Files are sorted by path and carry no timestamps: one tree always gives the same manifest. It refuses a git
checkout with uncommitted changes to tracked files (`--allow-dirty` overrides and writes `commit` as `<sha>-dirty`) and
an existing destination.

## JSON documents (schema 1)

Every document has `schema`, `kind`, `ok` and `problems` (`[{ code, severity, message }]`, severity `error` or
`warning`).

- `regia_engine_info`: `engine { version, line, oldest_upgradable_line, release_lines, release }` (`release` is the
  manifest's `{ version, commit }` when this `regia` was built from a release), `project` (`null` outside a project,
  else `{ dir, engine, pin }`; `engine` is `null` for a quick project without `Regia.toml`).
  `project.engine`: `{ requirement, line, status, lines_crossed, release_notes }`. `project.pin`: `{ sources, sites:
  [{ file, line, crate, source }] }`.
- `regia_engine_package`: `dir`, `version`, `commit`, `files`, `bytes`, `crates`.
- `regia_engine_verify`: `dir`, `version`, `commit`, `checked`.
- `regia_upgrade_project`: `write`, `engine { version, line }`, `project` (as in info), `pin` (as in info plus
  `target`, the `--engine` source, `edits: [{ file, changed_lines }]` and, for a `path:` source, `lock_drift`: the
  count of drifted packages, `null` without a project or engine lock), `stamp` (the line `--write` writes, or
  `null`), `files: [{ path, format, found, current, status }]` (`migrate`, `current`, `newer`), `renames`,
  `build_scripts: [{ path, manifest, status }]`: each game executable crate (`launcher`, `client`, the net game's `server` (ADR-5617) or the quick crate)
  and its Windows build script (ADR-2645): `add` (no `build.rs`; `--write` writes the template's and appends the
  `[build-dependencies]` to `manifest`, with a `.bak`), `dependencies` (the template's script, some build-dependencies
  missing), `update` (an unchanged copy of an earlier engine's script: `--write` replaces it with the current one,
  which also sets the exe's main-thread stack on Windows, ADR-5181), `current`, `user_script` (a `build.rs` of the
  user's or a `build =` key elsewhere: never touched), `manual`
  (the table cannot be extended by a text edit; add `winresource`, `png` and `toml` by hand). A dry
  run describes the plan; `--write` prints the same document after writing. A refusal prints it with `ok: false`
  and writes nothing.

## Codes

Lowercase ASCII, digits and `_`; stable.

| Code | Severity | Meaning |
|---|---|---|
| `current`, `upgrade_needed`, `unpinned`, `upgrade_unsupported`, `engine_too_old` | (status) | The policy table above. |
| `engine_too_old` | error | The project needs a newer engine. |
| `upgrade_unsupported` | error | The project is older than the support window. |
| `engine_requirement_invalid` | error | `[project] engine` is not `N.N`, `N.N.N` or `N` (or not a string). |
| `manifest_missing`, `manifest_invalid` | error | No readable `Regia.toml` where one was needed. |
| `engine_source_invalid` | error | `--engine` / `--engine-source` is not one of the forms above. |
| `engine_source_version_mismatch` | error | The `--engine` path, registry version or git tag names another engine than this `regia`. |
| `engine_source_missing` | error | The `--engine` path holds no engine (copy the release there first). |
| `engine_tag_unverified` | warning | The git tag names no engine version; it cannot be fetched to check. A tag that names another version is `engine_source_version_mismatch`. |
| `engine_crate_missing` | error | A path source lacks `crates/<crate>` for a pinned dependency. |
| `engine_dep_layout` | error (warning without `--engine`) | An engine dependency is a `[dependencies.x]` table, a dotted key or spans lines; the message names `file:line`. |
| `engine_pin_stale` | warning | The pinned source names another engine version than this `regia`. |
| `engine_pin_empty` | warning | `--engine` was given, but the project has no engine dependency. |
| `engine_lock_differs` | warning | The project's `Cargo.lock` locks third-party packages to versions the `--engine` path's lock does not have. |
| `engine_sources_mixed` | warning | Engine crates come from more than one source. |
| `engine_root_invalid` | error | `--engine` for `package` has no `[workspace.package] version`. |
| `release_dirty` | error | The checkout has uncommitted changes to tracked files. |
| `release_exists` | error | The release directory already exists. |
| `release_manifest_missing`, `release_manifest_invalid` | error | No `regia-engine.release.ron`, or it is not a regular file, or it lists a path that is absolute or leaves the release. One that is there but cannot be read (permissions) is `io_error`. |
| `release_format_newer` | error | The manifest's `format_version` is newer than this `regia` reads. |
| `release_file_missing`, `release_file_changed` | error | A listed file is gone, or its size or BLAKE3 differs. |
| `release_file_unreadable` | error | A listed file, or a folder above it, exists but cannot be read (permissions or IO); the message names the error kind. |
| `release_file_link` | error | A symbolic link stands at a listed path, in a folder above one, or anywhere outside `target/` and `.git`. It is not followed. |
| `release_file_extra` | warning | A file outside `target/` and `.git` is not in the manifest. |
| `git_failed`, `io_error` | error | `git` or a file operation failed; the message says which. |
| `checksums_missing` | error | `CHECKSUMS.b3` is not there (`verify_checksums`). |
| `checksums_invalid` | error | A checksum line is not `<64 hex>  <path>`, the path is unsafe, the list is empty or has a duplicate row, or the list file is not regular text. |
| `checksum_path_invalid` | error | `checksum_files` was asked for a path the project path rule rejects. |
| `checksum_file_missing` | error | A listed file is not there. |
| `checksum_mismatch` | error | The hash differs, or the path is not a regular file (a directory, FIFO or device is not read). |
| `checksum_file_link` | error | A symbolic link stands at the listed path or a parent. It is not followed. |
| `checksum_file_unreadable` | error | A listed file, or a folder above it, exists but cannot be read; the message names the error kind. |

## Checking an upgrade across two tags

`cargo xtask two-tag-upgrade --from vA --to vB [--templates all|blank,quick] [--work-dir DIR] [--keep]` runs the
check of spec 94 § 9 for you (ADR-2610). It adds a git worktree per tag under `target/two-tag/`, builds each tag's
`regia` there, packages each tag's engine tree with `regia engine package`, and then, for every template:

1. `regia(vA) new tt_<template> --template <template> --engine-source path:<release A>`;
2. `regia(vB) upgrade-project --project <project> --write --engine path:<release B>`;
3. `cargo check --manifest-path <project>/Cargo.toml` (and the project's `server/` crate when it has one), against
   the new release's crates.

The report has one row per template (`ok` or `FAILED at new|upgrade|check`, with seconds per stage) and the last lines
of the failing command's output. Exit code 1 when any template fails. Both tags must carry spec 94 (`engine package`
exists); it needs no network beyond cargo's registry cache. `--keep` leaves the worktrees and projects for inspection;
without it they are removed. CI runs it weekly over the two newest `v0.*` tags (`nightly.yml`, job `two-tag-upgrade`).

By hand, the same steps are: package both releases, `regia(vA) new T --engine-source path:<release A>`,
`regia(vB) upgrade-project --project T --write --engine path:<release B>`, `cargo check` the project.

Related: [Content formats](../reference/formats.md), [Toolchain doctor (agents)](doctor.md),
[Project structure](../user-guide/project-structure.md).

### Template acceptance and preservation

The distribution compile and upgrade checks use `regia_templates::TEMPLATE_IDS`,
the same six IDs as the existing templates CI job: `blank`, `first_person`,
`third_person`, `top_down`, `quick`, `net_game`. `net-game` is an alias.
A generated blank or quick app accepts `--exit-after-frames N` for a bounded
smoke. The character templates already accept this flag. For `net_game`, build
the dedicated server separately from the default client members.

Run a dry upgrade before `--write`; compare unrelated binary assets/user files
byte for byte and require originals in `.bak` for each changed content file and
manifest. The regression suite includes a cue with a commented version and a
spaced root key, a migrated cue load, and multiline user text in `Regia.toml`.
Reapplying the same pin must leave the project unchanged. `engine_dep_layout`
is a warning for read-only `engine info`; requesting a rewrite still refuses
unsupported dependency tables. Lock drift counts package/version rows, so two
unmatched versions of one crate count twice.

Related CLI documentation correction: `regia bake-gi <scene> --check-volume FILE`
compares a volume. `--check` is a boolean for scene-description export/check;
it does not take a volume path.

A level directory that cannot be inspected (including a mixture of old and
future scene versions) refuses the upgrade before any content or pin writes.
The error starts with `level_dir_unreadable` and names the directory. Use an
engine that supports its newest version; do not delete the directory to make
an upgrade pass. This refusal also leaves unrelated old assets untouched.

Server distribution fails before filtering if a level or prefab in its closure cannot be read. A raw animation graph or montage is retained only when the gameplay pose closure reaches that particular path. `regia import --out` uses exclusively created sibling staging files, so an existing staging symlink cannot redirect the converted bytes into a user file.

### Package source links (v0.5.1)

`regia engine package` refuses source symbolic links with `release_file_link` before hashing
or copying them, including tracked links to files outside the checkout, internal links and
dangling links. Links in a listed file's parent directories and Windows reparse points are refused
too. A source tree without Git is checked by a walk that also refuses links. Replace a link with
the intended regular file before packaging. Source names must follow the verifier's portable
relative-path rule too (`release_manifest_invalid`). This matches the release verifier's regular-file
contract; no link representation or release format version was added (ADR-7871).
