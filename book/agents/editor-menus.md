# Editor menus, packaging and Help (agents)

The editor's menu bar (spec 19 § 2, ADR-7475) has **File**, **Create**, **Edit**, **View**, **Tools**, **Sequence**,
**Play** and **Help**. Every item is a registered UI action with an agent call that does what clicking it does, and
`editor.menu_list` answers the whole tree, so an agent can find and run any menu item. This page also covers
File ▸ **Package for platform…**, Tools ▸ **Rebuild asset database**, **Clear caches…** and **Bake reflection captures**, Help ▸ **Documentation**,
**Agent guide** and **About**, and the Project Settings **Icon** picker. All of it is desktop-editor only.

Errors are `invalid_params` (or `not_found` / `unsupported` where noted) with a lowercase `data.reason` and a hint.

## The menu tree (`editor.menu_list`)

```json
{"method": "editor.menu_list", "params": {}}
```

Answers `{ menus: [{ name, items: [{ path, label, action, class, method, params, enabled }] }], count }`. `path` is
`Menu/Label` (`File/Package for platform…`, `Play/Players/2 players`); `action` is the parity registry id
(`editor.actions` lists the same ids); `class` is `method`, `command`, `os_dialog` or `confirm`; `method` and
`params` are the call to make. An `os_dialog` item's `params` carry `"path": "<file>"`: pass the file the dialog
would have picked. A `confirm` item's carry `"confirm": true`: the confirm window's choice. `enabled` is whether the
item can run now (Undo with an empty history, Paste with an empty clipboard, Pause while stopped, Compile without game
code, Package without a project, and Package, Clear caches and Rebuild asset database while a package, cache clear or
cook runs are disabled).

The list is what the menus draw: a shell test draws every menu open and checks that the drawn items are exactly the
listed ones, in order (ADR-7496). Create ▸ Shape has one item per primitive kind (`Create/Shape/Cuboid`, params
`{ "run": "create.cuboid" }`); other Create actions call `editor.create_entity { kind }`.

| Menu | Items (agent call) |
|---|---|
| File | New level (`editor.command file.new_level`), Open level… (`editor.open_level`), New script… (`editor.script_wizard`), Open kit… (`editor.open_kit`), Save level / Save level as… (`editor.save_level`), Save all, Recent, Exit (`editor.menu_invoke`), Export selection to glTF… (`asset.export_glb`), Package for platform… (`editor.package_start { confirm: true }`), Create entity (`editor.spawn`) |
| Create | Shape ▸ one item per kind (`editor.command { run: "create.<kind>" }`) |
| Edit | Undo, Redo (`editor.command`), Cut, Copy, Paste, Duplicate, Delete, Select all (`editor.menu_invoke`), Preferences…, Project settings…, World settings… (`editor.command window.*`), Prefab overrides…, Edit prefab in place (`editor.command prefab.*`, [Prefab overrides](prefabs.md)) |
| View | Grid, Selection labels, Realtime viewport, Profiler, Diagnostics, Frame selection, layouts (`editor.command`) |
| Tools | Rebuild asset database (`editor.rebuild_asset_db`), Clear caches… (`editor.clear_caches { confirm: true }`), Open code editor (`editor.open_code_editor`), Bake reflection captures (`editor.bake_captures`) |
| Sequence | Render movie… (`editor.render_movie { sequence }`, ADR-7726; the dialog and its job: [Render movie](render-movie.md)) |
| Play | Play, Simulate (`editor.play { mode: "simulate" }`), Pause, Resume, Stop, Step, Compile (`agent.hot_reload`), Players ▸ 1–4 (`editor.play { players }`) |
| Help | Documentation, Agent guide (`editor.help_open { topic }`), About Regia Engine (`editor.about { open: true }`) |

## Package for platform (`editor.package_start`, `editor.package_status`)

File ▸ Package for platform… opens a window with the **target** (Linux, Windows, Android), the **profile** (dev,
shipping, or a `[profiles.<name>]` entry of `Regia.toml`), the **output folder** (default `dist/<target>`) and **dry
run**. Package runs the same command a terminal user runs, in the project root:

```text
regia package --platform <target> --profile <profile> --out <dir> --project <root> --json [--dry-run]
```

The editor never waits on it. The run is job `package:<n>` (it shows in `agent.job_status` too) and background task
`package`; the window shows a spinner, the last output line, the output tail, then the result with **Open log** and
**Show in folder**. The `regia` program is `$REGIA_CLI`, else `regia` beside the editor executable, else `regia` on
`PATH`.

The run never writes into the output folder directly (ADR-7496). The editor creates a private staging folder inside it,
`<out>/.regia-package-<pid>-<n>`, and passes that as `--out`. When the run succeeds, each entry of the staging folder
is moved into the output folder, replacing the entry of the same name (the CLI rebuilds `<out>/<slug>/` and replaces
its archive the same way), and the staging folder goes. A cancelled, failed or dry run removes the staging folder, and
the folders the editor created for it when they are empty, and nothing else: the previous build in the output folder,
and files other programs write there meanwhile, stay. Cancel also kills the run and its child processes (cargo, the
compilers). A package that finished but could not be moved into place (an old file is locked) fails the job with
`package_failed`; the message names the staging folder that holds it.

A crash mid-package leaves its staging folder behind (ADR-7744). Each run holds an exclusive lock on a file beside its
folder, `<out>/.regia-package-<pid>-<n>.lock`, for as long as it owns the folder; the operating system drops the lock when
the run dies. The lock file also names the run's `regia package` process, which outlives a crashed editor and keeps
writing. The editor removes the staging folders whose lock nobody holds and whose `regia package` process is gone (and
orphaned lock files) at start, for every `<project>/dist/<target>`, and again before each new package in that run's
output folder; the sweep runs on a worker thread. A live run's folder, this process's or another editor's, is never
removed (ADR-7794). A folder with no lock file (left by an older editor) goes only when the editor process its name
carries is gone on this machine and the folder is more than 24 hours old; when the platform cannot tell, or the name
carries no process, it stays. Nothing else in the output folder is touched.

```json
{"method": "editor.package_start", "params": {"target": "linux", "profile": "shipping", "out": "dist/linux", "confirm": true}}
```

| Param | Meaning |
|---|---|
| `target` | `linux`, `windows` or `android` (default: the host's). |
| `profile` | `dev` (default), `shipping` or a `[profiles.<name>]` entry. |
| `out` | Output folder: project-relative, or absolute under the project, the temp directory, the working directory or an `output_roots` entry, without `..` or a symlink leading out (a request path, ADR-1816; default `dist/<target>`). |
| `dry_run` | Cook if needed and report the shipped sizes; no build, no output. |
| `confirm` | Must be `true` unless `dry_run`: packaging writes a shippable game, so like `agent.package` it needs the owner's agreement. |

Answers `{ job, task: "package", command, out, staging, log }` (`out` is `null` for a dry run). Poll:

```json
{"method": "editor.package_status", "params": {}}
```

`{ job, task, state, message, lines, line_count, out, staging, log, command, result }`: `state` is `running`, `done`,
`failed` or `cancelled`; `lines` is the last 40 output lines; `log` is the file that gets the whole output when the run
ends (`.regia/logs/package-<n>.log`). Once the run ended `result` is
`{ ok, code?, exit_code, artifact?, report?, removed?, out?, published?, log }`: `report` is the CLI's `--json` object
and `artifact` the package path, both naming the output folder (not the staging folder); `published` lists the entries
moved into the output folder; `removed` the staging folder (and empty folders) a cancel deleted. With `job` it reports a given job (also a `clean:<n>`
job); without one ever started it answers `{ job: null, state: "idle" }`.

Stop a run with `editor.cancel_task { id: "package" }` (the window's Cancel) or `editor.cancel_job { id: "package:<n>" }`
(`agent.job_cancel` works too).

| Code | When |
|---|---|
| `package_no_project` (`not_found`) | The editor has no project, or its root has no `Regia.toml`. |
| `package_bad_target` | `target` is not linux, windows or android. |
| `package_bad_profile` | `profile` is not a profile name (lowercase letters, digits, `_`, `-`). |
| `path_outside_project` | `out` leads outside the allowed roots (ADR-1816). |
| `needs_confirmation` | A package (not a dry run) without `confirm: true`. |
| `package_busy` | A package, a cache clear, or an agent cook/package is running. |
| `package_out_unwritable` (`unsupported`) | The output folder or the run's staging folder in it cannot be created. |
| `package_spawn_failed` (`unsupported`) | The `regia` program could not start; the message names the path and `REGIA_CLI`. |
| `package_not_found` (`not_found`) | `editor.package_status { job }` names no package or clear-caches job of this editor. |
| `package_failed` | Result code of a run that exited non-zero (the job fails with this reason). |
| `package_cancelled` | Result code of a cancelled run (the job ends `cancelled`). |
| `package_hook_aborted` | An extension hook aborted the run (ADR-7791): a before-cook hook refuses the start; an after-package hook turns the finished run's result into `ok: false` with this code. The message names the hook. |

## Tools

**Rebuild asset database** (`editor.rebuild_asset_db`, no params) runs in the editor under the project's cook lock: it
re-pairs moved `.meta` files (the cook's repair), drops database rows whose source no longer exists, saves the
database and re-lists the Content Browser. New and changed sources are imported by the next cook. Answers
`{ sources, kept, dropped, metas_repaired, metas_orphaned }`. It never waits for the lock: the item is disabled while a
package, a cache clear or an agent cook, package or clean job runs, and the method answers `tools_busy` then and when
another process (a terminal `regia cook`) holds the cook lock. Codes: `tools_no_project` (`not_found`), `tools_busy`,
`tools_failed`.

**Clear caches…** asks first, then runs `regia clean --cache --json --project <root>` as job `clean:<n>` (background
task `clear_caches`): it deletes the cook cache, the asset database and the shader and pipeline caches, never sources
or `assets/`. The agent states the choice:

```json
{"method": "editor.clear_caches", "params": {"confirm": true}}
```

Without `confirm: true` it answers `needs_confirmation`. It shares the package codes (`package_no_project`,
`package_busy`, `package_spawn_failed`); a failed run's result code is `clean_failed`, a cancelled one's
`clean_cancelled`. Poll with `editor.package_status { job: "clean:<n>" }`; its `result.report` is the CLI's
`regia clean --json` object `{ dry_run, level, removed, paths, bytes }`.

**Open code editor** (`editor.open_code_editor`, no params, spec 19 § 5) runs the project's configured code editor on
the game crate's manifest (`Regia.toml` when the project has no game crate), spawned with the project root as the
working directory. The editor command is resolved exactly as a log link's click: the `code.editor_command` preference,
else `[editor] code_editor = ["<program>", "{file}:{line}"]` of `.regia/agent.toml`, else the environment's `$VISUAL`
or `$EDITOR`; it never goes through a shell. It starts an external process, so it is declared like `editor.launch` and
`editor.package_start`: mutating (it spends the mutating-call budget) and needing the `build` permission, which is not
in the default profile. The item is enabled with a project open. Answers
`{ opened: "code_editor", root, file, command }`. Codes: `open_in_editor_no_code_editor` when nothing names an editor
(the same code `editor.log_open_link` answers).

**Bake reflection captures** (`editor.bake_captures { file? }`, ADR-7592) bakes every placed `LocalCapture` of the
level with the editor's renderer, one per frame, as background task `bake_captures`, and writes the level's `.fcap`
through a temporary file and a rename; a cancel (`editor.cancel_task { id: "bake_captures" }` or the progress
window's Cancel) writes nothing. Poll `editor.bake_captures_status`. Codes and details:
[Reflection captures](reflection-captures.md).

## Help (`editor.help_open`, `editor.about`)

Documentation and Agent guide open a local file in the OS viewer: the rendered book (`book/build/index.html`,
`book/build/agents/protocol.html`), else the generated manual (`docs/manual/manual.html`, Documentation only), else the
book's Markdown (`book/introduction.md`, `book/agents/protocol.md`). The search tries `$REGIA_BOOK` (the book, or a
folder holding `book/`), then the editor executable's folder and the working directory and up to eight of their parents.

```json
{"method": "editor.help_open", "params": {"topic": "agent_guide", "dry_run": true}}
```

`topic` is `documentation` or `agent_guide`; `dry_run` finds the file and opens nothing. The method starts an OS
viewer, so it is declared mutating and needs the `edit_scene` permission (as does `editor.about`, whose `open: true`
opens the About window). Answers
`{ topic, path, opened }`. Codes: `help_docs_missing` (`not_found`, no local copy; set `REGIA_BOOK`),
`help_open_failed` (the viewer could not start).

About shows the editor version, the git revision baked at build time, the engine's license (Mozilla Public License
2.0, `MPL-2.0`, with a link to https://mozilla.org/MPL/2.0/; ADR-10225) and buttons that open `LICENSE` and
`THIRD_PARTY_NOTICES.md`. `editor.about { open? }` answers
`{ name, version, git_revision, license_title, license_spdx, license_url, license_path, notices_path, open }`
(`license_title` is the first line of `LICENSE`; paths are `null` when not found);
`open: true` opens the window too.

## Project icon (`editor.project_icon`, `editor.project_icon_info`)

Project Settings ▸ Icon ▸ **Choose icon…** copies the chosen PNG, byte for byte, to `assets/branding/icon.png`: the
file `regia package` ships as the Linux `.desktop` icon and the Windows and Android launcher icon. There is no
`Regia.toml` key; the file is the setting. When an icon already exists, the editor asks first (**Replace icon** /
**Keep current**): the overwrite has no undo.

```json
{"method": "editor.project_icon", "params": {"path": "art/icon-512.png"}}
```

`path` is a request path (ADR-1816): project-relative, or absolute under the project, the temp directory, the working
directory or an `output_roots` entry; without it the method reads the current icon. Answers
`{ icon, path, bytes, width, height, set, source? }`. Codes: `icon_no_project` and `icon_source_missing` (`not_found`),
`icon_not_png` (the file does not start with a PNG header; the old icon stays), `icon_write_failed`,
`path_outside_project`. The Project Settings row reads only the icon's header, and only when its size or modified time
changed.

`editor.project_icon_info {}` reads the icon for read-only agent profiles: `{ icon, path, bytes, width, height, set }`.
Here `set` says whether the project has an icon (`false`: packages use the default); in `editor.project_icon` it says
whether that call wrote one, so a read through it always answers `set: false`. It needs no Assets permission; the
write half, `editor.project_icon { path }`, does.
