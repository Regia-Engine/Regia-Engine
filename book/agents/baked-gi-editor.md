# Baking from the editor

The editor's **Bake** panel holds one GI volume, its quality and the state of the running bake. Four agent methods
drive the same state, so what an agent sets is what the panel shows. A bake writes a `.bake.ron` description and
runs `regia bake-gi` on it ([Baking GI from meshes](baked-gi-bake.md)); the result is a `.fgiv` volume
([Baked GI volumes](baked-gi.md)) that the level can name ([Baked GI from the level](baked-gi-maps.md)). The user
side is [The Bake panel](../user-guide/bake-panel.md). Design: ADR-1098.

## `editor.bake_get`

Read-only. Returns the state:

- `volume`: `center`, `size`, `spacing`, `min`, `max`, `counts` (probes per axis), `probes`, `valid`, `error` (an
  error code or null).
- `quality`: `draft`, `normal`, `high`, or null when the counts differ from every preset.
- `settings`: `rays_per_probe`, `samples`, `max_bounces`, `seed`, `filter`.
- `sun`: the sun used when the level has no directional light.
- `show_gizmo`, `assign`, `running`.
- `job`: while a bake runs, `phase`, `dry_run`, `stage`, `done`, `total` (the last progress line), else null.
- `last`: the last finished bake: `phase` (`succeeded`, `failed`, `cancelled`), `dry_run`, `volume` (the file, for
  a real success), `code` and `message` (a failure), `summary` (the JSON `regia bake-gi` printed), `assigned`.
- `desc` and `out`: the files a bake would write (null without an open project).

## `editor.bake_set`

Changes the volume and options. All parameters are optional. A `filter`, `show_gizmo`, `assign` or
`fit_selection` that is not a JSON boolean is skipped, not an error (`fit_selection` runs only when it is
exactly `true`). Number fields are checked as they are read. Every field is read into a copy first: the
volume and the trace settings are validated and `out` is checked before anything is stored, so a call that
returns an error (a bad number, a spacing that fails validation, a rejected `out`) changes nothing,
`sun_toward` and `sun_illuminance` included.

| Param | Meaning |
|---|---|
| `center`, `size` | `[x, y, z]` in metres. |
| `spacing` | Probe spacing, 0.05 to 64 m. |
| `fit_selection`, `margin` | Fit the volume around the selected entities' positions plus `margin` (default 2 m) first. `center`, `size` and `spacing` in the same call then override it. |
| `quality` | `draft`, `normal` or `high`. |
| `rays_per_probe`, `samples`, `max_bounces`, `seed`, `filter` | The trace settings (limits 1..4096, 1..1024, 1..32). |
| `sun_toward`, `sun_illuminance` | The sun for a level without a directional light. |
| `show_gizmo` | Draw the volume in the viewport. |
| `assign` | Name the volume in the level when a bake succeeds. |
| `out` | Volume file, project-relative or absolute; null restores `assets/maps/<level>.fgiv`. |

Returns the state of `editor.bake_get`. It is refused with `bake_busy` while a bake runs.

## `editor.bake_start`

Writes `<project>/bake/<level>.editor.bake.ron` and starts `regia bake-gi`. Params: `dry_run` (validate and lay
out the probes; no GPU, no file), `wait` (block until the bake ends; use it for dry runs and small bakes, not for a
long bake), `assign`. Without `wait` it returns at once with `running: true`; poll `editor.bake_get` until
`running` is false and read `last`. The description holds the level's first `DirectionalLight` as the sun (else the
panel's sun), every `GiMobility::Static` entity with a glTF `MeshSource` as an occluder, and the volume as one
detail brick.

## `editor.bake_cancel`

Kills the running bake. No volume is written. Returns the state; `last.phase` is `cancelled`.

## Errors

Every refusal is an invalid-params error with the code in `data.bake_error`.

| Code | Meaning |
|---|---|
| `bake_volume_invalid` | A non-finite or out-of-range centre, size or spacing, or more than 4,194,304 probes. |
| `bake_settings_invalid` | A zero or too large ray, sample or bounce count. |
| `bake_busy` | A bake runs; cancel it first. |
| `bake_not_running` | `editor.bake_cancel` with nothing running. |
| `bake_no_project` | No project is open. |
| `bake_write_failed` | The description or the output folder cannot be written. |
| `bake_spawn_failed` | The `regia` executable cannot be started. Build it next to the editor (`cargo build -p regia_cli`). |
| `bake_selection_empty` | `fit_selection` with nothing selected. |

A bake that starts and fails reports in `last`: `code` is the command's own code (`bake_desc_invalid`,
`bake_layout`, `bake_mesh_load`, ... in [Baking GI from meshes](baked-gi-bake.md)), or `bake_failed` when it printed
none (the `message` then holds the last lines of its stderr, for example "no GPU adapter for the bake").

## Verify with the agent

```json
{ "method": "editor.bake_set", "params": { "fit_selection": true, "spacing": 1.0, "quality": "draft" } }
{ "method": "editor.bake_start", "params": { "dry_run": true, "wait": true } }
{ "method": "editor.capture_viewport" }
{ "method": "editor.bake_start", "params": { "assign": true } }
{ "method": "editor.bake_get" }
```

The dry run proves the description loads and shows the layout in `last.summary.plan`. The capture shows the green
volume box with its probe lattice. After the real bake succeeds, `level.get_baked_gi` reports the named volume.
