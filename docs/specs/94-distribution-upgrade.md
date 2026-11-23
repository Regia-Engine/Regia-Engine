# Spec 94: Developer distribution and upgrade

Status: proposed (ADR-1852). Plan item: `docs/master-plan.md` § 3.7a (v0.3.x (b), proven at v1.0). Source: the Regia
docs only (`docs/master-plan.md` § 3.7a and § 8 Q15, `docs/versions.md`, `docs/specs/12-assets.md` § 4a,
`docs/specs/21-tooling-and-packaging.md` § 1 and § 2); Cargo's public reference for dependency sources and SemVer
compatibility (doc.rust-lang.org/cargo: "Specifying Dependencies", "SemVer Compatibility").

## 1. Purpose

A game developer builds a game against a **released** Regia Engine, not against whatever checkout the engine team has. That
needs three things the engine did not have:

1. **A packaged engine release**: one directory per tag holding the engine source exactly as tagged, plus a manifest
   (`regia-engine.release.ron`) that names the version and the BLAKE3 hash of every file, so a game developer can check that
   a copy is complete and unmodified (`regia engine verify`).
2. **A pinned engine in every project**: `Regia.toml [project] engine` names the release line the project targets,
   and the project's Cargo dependencies on engine crates name one source (a path, a vendored copy, a git tag or a
   private registry). `regia new --engine-source` writes it; `regia engine info` reads it.
3. **An upgrade path**: `regia upgrade-project --engine <source>` moves a project from release vN to vN+1: it runs
   the content-format migrators (unchanged, `specs/12` § 4a), rewrites every engine dependency to the new source and
   stamps the new line in `Regia.toml`, with a backup of every file it rewrites. A dry run lists all of it first.

How the owner sells access (private registry, vendored tag or git tag) is the owner's decision (§ 8 Q15, recommended:
vendored source tag first). All three are supported here so the decision does not block the tooling.

## 2. Version policy

Regia Engine versions are `MAJOR.MINOR.PATCH` (the workspace version, `Cargo.toml [workspace.package] version`).

- **Release line.** Before 1.0 the line is `0.MINOR` (`0.2`, `0.3`); from 1.0 on it is `MAJOR`. This is Cargo's
  own compatibility rule for caret requirements, so a registry or git pin resolves the same way.
- **Within a line** (patch releases): engine APIs stay source compatible for hosts that do not build struct literals
  of engine types or match engine enums exhaustively (the release notes' Upgrade section lists every exception, as
  today). A patch release **may** add a content-format version with its migrator (v0.1.24 did): run
  `regia upgrade-project` after every engine update; it is a no-op when nothing changed.
- **Across lines**: anything may change; every host-visible change is listed in the release notes (already the rule).
  `upgrade-project` crosses any number of lines in one run, because every format's migrators chain from its first
  version; it prints the release-note files of every line it crosses.
- **Support window.** The oldest line `upgrade-project` accepts is `OLDEST_UPGRADABLE_LINE` (today `0.1`: every
  migrator back to each format's v1 is still registered). A future line that retires old migrators raises it; a
  project older than the window gets `upgrade_unsupported` and must first be upgraded with an older engine.
- **The project's requirement** (`Regia.toml [project] engine`): `"0.2"` (the line) or `"0.2.5"` (the line, at
  least that patch). Compared with the running engine:

| Requirement vs engine | Status code | Effect |
|---|---|---|
| same line, engine patch ≥ required patch | `current` | builds; `upgrade-project` only runs migrators |
| older line, inside the support window | `upgrade_needed` | `upgrade-project --write` stamps the engine's line |
| older line, outside the window | `upgrade_unsupported` | `upgrade-project` refuses before writing anything |
| newer line, or same line with a newer patch | `engine_too_old` | `upgrade-project` refuses; never downgrades |
| field missing | `unpinned` | treated as `upgrade_needed` from `OLDEST_UPGRADABLE_LINE`; `--write` stamps the line |
| not one to three numbers (`N`, `N.N`, `N.N.N`; a bare `0` is invalid) | `engine_requirement_invalid` | refuses; fix the field by hand |

Templates wrote a literal `engine = "0.1"` before this spec, also into projects made with v0.2.x. That value is
inside the window and only makes `upgrade-project` stamp the current line; nothing else depends on it.

## 3. API shape

### 3.1 `regia_core::engine_version` (runtime-safe, no dependencies)

```rust
pub struct EngineVersion { pub major: u32, pub minor: u32, pub patch: u32 }
impl EngineVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self;
    pub fn parse(text: &str) -> Result<Self, EngineVersionError>;   // "0.2.5"
    pub fn current() -> Self;                                        // this build (CARGO_PKG_VERSION)
    pub fn line(&self) -> ReleaseLine;
}
pub struct ReleaseLine { pub major: u32, pub minor: Option<u32> }  // "0.2" or "1"; Display/parse
pub struct EngineRequirement { pub minimum: EngineVersion }  // Regia.toml form; parse accepts "1", "0.2", "0.2.5" (missing parts are 0; a bare "0" is invalid)
pub enum Compatibility { Current, UpgradeNeeded { from: ReleaseLine, to: ReleaseLine },
                         Unpinned { to: ReleaseLine },
                         UpgradeUnsupported { from: ReleaseLine, oldest: ReleaseLine },
                         EngineTooOld { required: EngineVersion } }
pub fn compatibility(requirement: &EngineRequirement, engine: EngineVersion) -> Compatibility;
pub fn assess(requirement: Option<&EngineRequirement>, engine: EngineVersion) -> Compatibility;  // None => Unpinned
pub const OLDEST_UPGRADABLE_LINE: ReleaseLine;
pub const RELEASE_LINES: &[ReleaseLine];              // every line ever released, oldest first
pub fn lines_crossed(from: ReleaseLine, to: ReleaseLine) -> Vec<ReleaseLine>;
impl Compatibility { pub fn code(&self) -> &'static str; }   // the § 2 status codes
impl Compatibility { pub fn upgradable(&self) -> bool; pub fn stamp(&self) -> Option<ReleaseLine>; }
pub const CURRENT: EngineVersion;                     // EngineVersion::current(), a const
```

As built (ADR-5821): `EngineTooOld` carries the required version for the message, `upgradable()` is true for
`Current`, `UpgradeNeeded` and `Unpinned`, `stamp()` is the line `--write` writes (`UpgradeNeeded`, `Unpinned`),
and `EngineVersion::parse` also accepts a leading `v` (a tag name).

### 3.2 `regia_cli::distribution` (desktop tool library)

- `EngineSource`: `Path(PathBuf)` (absolute: an engine checkout or an installed release; relative: a copy inside the
  project, written relative to each manifest that names it), `Git { url, tag }`, `Registry { name, version }`.
  `EngineSource::parse("path:<dir>" | "git:<url>#<tag>" | "registry:<name>[@<version>]")`; a registry source without a
  version takes the running engine's version. `Display` writes the same form back.
- `EnginePin`: the engine dependency sites of a project: for each `Cargo.toml` under the project (not under `target/`,
  `.git*`, or a packaged release), each line in a `*dependencies` table (including `[workspace.dependencies]` and
  `[target.'…'.dependencies]`) whose key is `regia` or starts with `regia_`, unless a manifest of the project defines
  that package (a game called `regia_arena`), as `DepSite { file, line, krate, source }`. `read_pin(project) -> Result<EnginePin, DistError>`
  (`DistError { code, message }`: every distribution failure carries its stable code, § 5).
- `rewrite_pin(project, &EngineSource) -> Result<Vec<PinEdit>, DistError>`: the new text of every manifest. Only the
  source keys change (`path`, `git`, `tag`, `branch`, `rev`, `version`, `registry`); `optional`, `features`,
  `default-features`, `package` and the key order of the rest stay as written. A dependency written as a multi-line
  `[dependencies.regia_x]` table is refused with `engine_dep_layout` (the line is named; edit it by hand).
- `stamp_engine_line(manifest_text, ReleaseLine) -> Option<String>`: sets or inserts `engine = "…"` in `[project]`,
  leaving every other line untouched, including multiline user strings. Returns `None` when
  the manifest has no explicit `[project]` table; no table is invented.
- Lockfile (`distribution::lock`): `seed_lockfile(project, engine_dir)` copies the engine's `Cargo.lock` into a
  project that has none; `lock_drift(project, engine_dir)` counts unmatched third-party package/version rows among names shared by both
  lockfiles (two unmatched versions of one name count twice). Measured on v0.2.5 (the build server): a fresh `first_person` project without a
  lockfile resolved 64 of 660 packages to versions the engine's lock does not have (`bevy_*` helper crates 0.20.1
  beside the pinned `bevy_ecs =0.20.0`, `cc`, `arboard`, …); seeded with the engine's lock, 0 of 670. `regia new`
  seeds from a path source or the build checkout (the editor's New Project from the build checkout); `upgrade-project --write --engine path:` seeds a project without
  a lock and warns `engine_lock_differs` (with the count) when an existing lock drifted; it never overwrites a
  project's lock, because the game's own dependencies would re-resolve.
- Release manifest: `EngineRelease { format_version, version, commit, rust_version, crates, files:
  Vec<ReleaseFile { path, size, blake3 }> }`, `package_release(engine_root, out_dir, allow_dirty) -> Result<PackagedRelease,
  DistError>`, `read_release(dir)`, `verify_release(dir) -> ReleaseReport`. ADR-2827 adds the `CHECKSUMS.b3` helpers
  (`checksum_files`, `checksums_text`, `verify_checksums`) the demo archives use; their codes are on the guide page.

### 3.3 Commands

```text
regia engine info [--project DIR] [--json]          # this engine, the project's requirement and pin
regia engine package --out DIR [--engine DIR] [--allow-dirty] [--json]   # copy the tagged source + write the manifest
regia engine verify DIR [--json]                    # check a release copy against its manifest
regia new NAME [--template T] [--engine-source SRC] # pin a new project to SRC (default: this checkout)
regia upgrade-project [--write] [--engine SRC] [--json]  # migrators + pin rewrite + line stamp
```

`regia engine package` takes the file list from `git ls-files` when the engine root is a git checkout (exactly the
tracked files of the tag), otherwise from a walk that skips `target/` and `.git*`. Source links
(including ancestor links and Windows reparse points) are refused before hashing or copying with
`release_file_link`; source names obey the verifier's portable relative-path rule
(`release_manifest_invalid`, ADR-7871). It writes
`<out>/regia-engine-<version>/` and refuses an existing destination (`release_exists`) and a git checkout with
uncommitted changes to tracked files (`release_dirty`; `--allow-dirty` packages it and writes `commit` as
`<sha>-dirty`). A game developer then builds the
CLI from the release (`cargo install --path regia-engine-<version>/crates/regia_cli`) and creates projects with
`regia new NAME --engine-source path:<release dir>`, or copies the release into the project and pins
`path:vendor/regia-engine-<version>`.

`upgrade-project` must be run by the **new** engine's `regia` (its migrators are the new ones). With `--engine
path:<dir>` the directory's version (its release manifest, else its `Cargo.toml [workspace.package] version`) must
equal the running engine's version (`engine_source_version_mismatch`; a directory with neither is
`engine_source_missing`). A git tag that names another version (`v0.2.5`, `0.2.5`) is the same
`engine_source_version_mismatch` error, so a tag cannot downgrade a project (ADR-1957 § 5); a tag that names no version
is a warning (`engine_tag_unverified`): the command cannot fetch it. Without `--engine`
the pin is left as it is and reported; a pin naming another version is a warning (`engine_pin_stale`: a git tag
or registry version that parses as one, or a path whose release manifest or workspace names one; ADR-5821), as are
engine crates from more than one source (`engine_sources_mixed`) and `--engine` on a project with no engine
dependency (`engine_pin_empty`).

## 4. Files and formats

### 4.1 `regia-engine.release.ron` (new, format_version 1)

```ron
(
    format_version: 1,
    version: "0.2.5",
    commit: Some("28fb09309…"),        // `git rev-parse HEAD`, `<sha>-dirty` with --allow-dirty; None outside git
    rust_version: "1.98",               // [workspace.package] rust-version
    crates: ["regia", "regia_2d", …],   // directories under crates/ with a Cargo.toml, sorted
    files: [ (path: "Cargo.toml", size: 6144, blake3: "9f…"), … ],   // sorted by path, '/' separators
)
```

The manifest itself is not in `files`. A reader rejects `format_version` above 1 (`release_format_newer`) and a listed
path that is absolute or leaves the release (`release_manifest_invalid`).
**Migration note:** v1 is the first version. A later shape change bumps `format_version` and adds a step to
`EngineRelease::migrate` that `regia engine verify` runs on read; the file is engine distribution metadata, not project
content. Its frozen metadata format is registered in `check-formats` and `upgrade-project`
(`docs/formats/engine-release-ron.md`). `upgrade-project` and `read_pin` skip any
directory that holds this file (a vendored engine is never migrated as project content).

### 4.2 Changed: `Regia.toml [project] engine`

Already in `specs/21` § 2; now read (`regia_cli::distribution::read_requirement`; `Manifest` is unchanged) and
written by `regia new` and the editor's New Project (the engine's line, the `{{engine_line}}` template placeholder,
instead of the literal `"0.1"`) and by `upgrade-project --write`. No shape change; no version bump.

## 5. Agent-protocol surface

These commands run without an app, so their agent surface is the JSON document of each command (the pattern of
`regia doctor --json`, ADR-1379), documented in `book/agents/distribution-upgrade.md`:
`{ "schema": 1, "kind": "regia_engine_info" | "regia_engine_package" | "regia_engine_verify" |
"regia_upgrade_project", "ok": bool, "problems": [{ "code", "severity", "message" }], … }`. Codes are lowercase
ASCII, digits and `_`, stable, and listed on the page. A failed check exits non-zero after printing the document.
The running app's `app.info` already reports `engine` (the version); no new protocol method is needed.

## 6. Guide pages

`book/agents/distribution-upgrade.md` (commands, JSON documents, codes, the policy table) and a row per command in
`book/COVERAGE.md` § 3.

## 7. Determinism

The release manifest is a pure function of the file bytes: files sorted by path, no timestamps, hashes in lowercase
hex; packaging the same tree twice gives byte-identical manifests. Pin rewrites are pure text functions (same input,
same output); a second `upgrade-project --write` with the same `--engine` changes nothing.

## 8. Tests and acceptance

- `engine_version`: parse and display round trip; lines before and after 1.0; every row of the § 2 table; the
  window; `lines_crossed`; `RELEASE_LINES` ends with the current line.
- `distribution`: source parse/display round trip; pin read on every template; rewrite keeps `optional` /
  `features` / order and is idempotent; relative paths are written per manifest depth; multi-line tables refused;
  `stamp_engine_line` inserts or replaces; release package + verify on a synthetic engine tree (clean, missing file,
  changed byte, extra file, newer format); manifest bytes identical across two packages of one tree.
- `upgrade-project`: on every template, a project pinned to a previous release directory and line `0.1` upgrades to
  this engine's release directory: pins rewritten, line stamped, backups written, a second run is a no-op;
  `engine_too_old` and `upgrade_unsupported` refuse without writing.
- Every template compiles against this engine: `regia new`, then `cargo check` (the `--ignored` test
  `every_template_compiles_against_this_engine`, nightly). Its first run found the First Person template broken on
  v0.2.5 (`Vec3` not imported), a defect no job caught because `docs/06`'s `templates` row had no job behind it.
- **Acceptance at v1.0** (§ 3.7a item 3, not runnable in a lane): CI runs, for two consecutive tags `vA` and `vB` and
  every template, `regia new` with `vA`'s CLI and release, `regia upgrade-project --write --engine path:<vB release>`
  with `vB`'s CLI, then `cargo check` of the project. § 9 lists the command.

## 9. Platform notes

All code is desktop tooling (`regia_cli`) except `regia_core::engine_version`, which is pure and builds on Linux,
Windows and Android aarch64 (no OS calls). Paths in manifests and pins are written with `/` on every host (Cargo
accepts them on Windows); a Windows drive path `C:/…` counts as absolute. `git` is optional: without it the release
walks the tree and `commit` is `None`. Android: a release is consumed by `regia package --platform android` like a
checkout; nothing Android-specific.

Two-tag check (lead or CI; needs two tags with this spec): `git worktree add /tmp/vA vA && git worktree add /tmp/vB vB`;
`regia engine package --out /tmp/rel` from each; for each template `regia(vA) new T --engine-source
path:/tmp/rel/regia-engine-A`, `regia(vB) upgrade-project --project T --write --engine path:/tmp/rel/regia-engine-B`,
`cargo check --manifest-path T/Cargo.toml`.

## 10. Performance budget

No per-frame work. `engine package` hashes the tree once (BLAKE3, I/O bound: the repo's tracked files, seconds);
`verify` the same. `read_pin` and `upgrade-project` read only manifests and the existing format scan.

## 11. Out of scope

- Running a private registry, publishing crates, licensing or access control (owner, § 8 Q15).
- Prebuilt engine binaries in the release (the `cargo xtask dist` test bundles stay separate) and a release archive
  (`.tar`/`.zip`); the directory is the unit, archived with standard tools.
- Rewriting host source code for API changes (the release notes list them).
- The CI job across two real tags (§ 9 gives the command; it needs two tags that carry this spec).
- Build budgets (§ 3.7a item 2: `cargo xtask build-times` exists) and the support channel (item 4, owner).
