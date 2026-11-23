# Renaming to Regia Engine

<!-- rename-keep-begin: this page names the old names on purpose; the markers also protect its copy in the generated manual -->

The engine was named Ferrite until ADR-1388; it is now **Regia Engine**. This page says what the
rename changes for a game project, a player and a host script, and what keeps working. It applies
from the release that ships the rename; until then nothing has changed. (The maintainers' runbook
is `docs/rename-regia.md`; the tool is `cargo xtask rename-brand`.)

## What changes

| Before | After |
|---|---|
| crates `ferrite`, `ferrite_*` | `regia`, `regia_*` |
| CLI `ferrite new`, `ferrite run`, ... | `regia new`, `regia run`, ... |
| environment variables `FERRITE_*` | `REGIA_*` |
| project manifest `Ferrite.toml` | `Regia.toml` |
| project state directory `.ferrite/` (cook cache, asset database, agent endpoint, autosaves) | `.regia/` |
| per-user data `<data dir>/Ferrite/<game>` (saves, logs, feedback, caches) | `<data dir>/RegiaEngine/<game>` |
| Android ids of new projects and samples `com.ferrite.*` | `com.regiaengine.*` |
| editor script marker `// FERRITE:SCRIPTS` | `// REGIA:SCRIPTS` |

File formats do not change: no magic, version or field name carries the engine's name, and every
file written before the rename loads unchanged (levels, prefabs and cooked scenes name components
and level settings by type paths such as `ferrite_scene::transform::Transform`; those are read as
the renamed crates, and a level saved again names its settings by the new path).

## What keeps working for one release

- **Environment variables.** `REGIA_X` is read first; when it is not set, `FERRITE_X` is read and
  the log warns once that `REGIA_X` replaces it. A manifest value `env:FERRITE_X` also finds
  `REGIA_X`.
- **`Ferrite.toml`.** A project with only the old manifest opens in the editor, runs, cooks and
  packages; the log warns once. Rename the file to `Regia.toml` when convenient.
- **`.ferrite/`.** When `.regia/` does not exist, the old directory is used in place (warned once).
  Move it (`mv .ferrite .regia`, and add `/.regia` to `.gitignore`) or run `regia upgrade-project --write`
  (which also adds the `.gitignore` line) to stop the warning; the cook cache and asset database carry over.
  A project that still has only `Ferrite.toml` keeps using `.ferrite/` even when it does not exist yet (a fresh
  clone), because its `.gitignore` ignores that name and the directory holds the agent's token.
- **Cooked files and paks.** Everything cooked before the rename loads: type tags are computed from the old crate
  names, so the bytes did not change. Cooked material graphs written earlier compose against the renamed shader
  modules.
- **Player data.** On first launch a renamed build moves `<data dir>/Ferrite/<game>` to
  `<data dir>/RegiaEngine/<game>` (one rename on the same disk). If the move fails, the old
  directory is used in place with a warning. Steam Auto-Cloud paths that name `Ferrite/` need the
  new name in the Steamworks settings.
- **Script marker.** "New script" also finds the old `// FERRITE:SCRIPTS` marker.
- **MCP resource URIs.** `regia mcp` also serves `ferrite://scene/current` and the other resources
  under the old scheme; `resources/list` names only `regia://`.
- **The CLI's log filter.** `regia` reads `REGIA_LOG`, then `FERRITE_LOG`.
- **Log filters and log targets.** A filter that names an engine crate by its old name (`ferrite_render=debug`,
  `ferrite::stream=debug`) keeps matching: the same directive is applied to the new name. `logs.read` with
  `target: "ferrite_agent"` also returns `regia_agent` records.
- **Type paths in agent calls.** Full paths with the old crate names (`ferrite_scene::transform::Transform`) work
  in scene, state, tween and `types.describe` calls; short names (`Transform`) always did.
- **UI themes.** The built-in themes are `regia-dark`, `regia-light`, `regia-high-contrast` and
  `regia-light-high-contrast`. A `.theme.ron` that `extends` `ferrite-dark`, and calls or settings that name the old
  built-ins, find the renamed ones; new files should use the new names.
- **Save files.** Saves written before the rename load, including resources whose type path names an engine crate.
- **Editor recent projects.** The list kept in `<config>/ferrite/` is read until the editor writes its own in
  `<config>/regia/`.
- **A renamed dependency.** With `ferrite = { package = "regia", ... }` in `launcher/Cargo.toml`, `regia run --dev`
  enables `ferrite/dev-assets`, and the shipping check still sees the engine's features.
- **Telemetry and alpha logs.** `cargo xtask alpha-triage` reads logs and session summaries written before the
  rename (`ferrite::*` targets, the `"ferrite"` section).

## What does not fall back

- **Build-time variables** read with `option_env!` when the game is compiled: `REGIA_GIT_HASH`,
  `REGIA_BUILD_VERSION`, `REGIA_BUILD_PROFILE` (crash reports and telemetry) and `REGIA_ABI_HASH`
  (hot reload). `regia package` and the editor set them; a build script of your own that set the
  `FERRITE_` names must set the `REGIA_` names, or crash reports say `unknown`.
- **Variables read by the engine's own tests, samples and `cargo xtask`** (for example
  `REGIA_DO27_CONTENT`, `REGIA_WRITE_EVIDENCE`): only the new names.
- **Telemetry in new logs**: the engine-build section of `summary.json` is the key `regia` and new log lines carry
  the target `regia::*`. Files written before the rename still read in the engine's own tools; an external dashboard
  must look up both spellings.
- **Window app ids.** Windows are `regia.<exe>` (Wayland `app_id`, X11 `WM_CLASS`); window-manager rules that match
  `ferrite.` need the new prefix.
- **Steam Auto-Cloud.** Player data moves from `Ferrite/<exe>/` to `RegiaEngine/<exe>/` on first launch. Add the new
  roots on the Steamworks partner site before a renamed build ships, and keep the old ones for one release.
- **Caches.** Pipeline caches and the incremental GI bake state rebuild once (the shader text and the engine version
  changed), as after any shader change.

## Updating a game project

- Cargo: depend on the renamed crates. The smallest change keeps the code as it is by renaming the
  dependency: `ferrite = { package = "regia", path = "<engine>/crates/regia" }` keeps
  `use ferrite::prelude::*` compiling. Derive macros from `regia_ai` and `regia_framework` name
  those crates by their new names, so a game that uses them depends on them under the new names.
- Log filters that name crate targets (`ferrite_render=debug`) keep matching for one release; change them to
  `regia_render=debug`.
- Scripts that call the CLI call `regia` (there is no `ferrite` alias binary).

## Names in files and on the wire (v0.6.0, ADR-8670)

v0.6.0 also writes the new name where earlier releases kept the old one inside files. Every reader in the
engine accepts both spellings, the new one first:

| Before | After | Old files |
|---|---|---|
| EXR header keys `ferrite.seed`, `ferrite.spp`, `ferrite.denoiser`, ... (path tracer), `ferrite.resource` (resource inspector) | `regia.seed`, `regia.spp`, `regia.denoiser`, ..., `regia.resource` | read |
| bake stamp `"format": "ferrite-bake-stamp/1"` | `regia-bake-stamp/1` | read, and still current (the input hash did not change) |
| crash upload header `X-Ferrite-Report-Name` | `X-Regia-Report-Name` | both headers are sent in v0.6.0; the old one goes in the next release |
| GPU card lock `/tmp/ferrite-gpu-<n>.lock` | `/tmp/regia-gpu-<n>.lock`, taken before the old lock | the old lock is still taken, second |

A tool of your own that reads these names must accept the new ones.

<!-- rename-keep-end -->
