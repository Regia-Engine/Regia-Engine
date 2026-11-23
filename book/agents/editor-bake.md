# Bake runner panel (`editor.bake_gi`, `editor.bake_status`, `editor.bake_gi_cancel`)

The editor's **Bake runner** tab runs `regia bake-gi` ([Baking GI from meshes](baked-gi-bake.md)) on a
built-in scene or one of the project's `.bake.ron` descriptions, shows its progress, can cancel it,
and shows the summary or the error. The bake runs in a child process: the path tracer gets its own
GPU device, the editor keeps drawing, and a crash in the bake leaves the editor running. The panel
and these methods share one state, so an agent sees a bake a user started and the other way round.
One bake runs at a time. To set a probe volume in the viewport and bake it, use the **Bake** tab
instead ([Baking from the editor](baked-gi-editor.md)); its cancel method is `editor.bake_cancel`.

The editor runs the CLI named by `REGIA_CLI`, else `regia` next to the editor executable, else
`regia` on `PATH`. Build it with `cargo build -p regia_cli`.

## `editor.bake_gi`

Mutating, permission `assets`. Starts a bake and returns the status (the `editor.bake_status`
shape below).

| Param | Type | Meaning |
|---|---|---|
| `scene` | string, required | `cornell`, `furnace`, `rolling-hills`, or a `.bake.ron` path (absolute, or relative to the open project). |
| `out` | string | The `.fgiv` to write. Default: beside the description (`maps/bore.bake.ron` → `maps/bore.fgiv`), or `.regia/bake/<name>.fgiv` in the project for a built-in. The folder is created. |
| `dry_run` | bool | Validate the description and lay out the probes without tracing (no GPU, nothing written). |
| `wait` | bool | Block until the bake finishes. Allowed only with `dry_run` (a full bake would freeze the editor). |
| `rays`, `samples`, `bounces`, `seed`, `probes_per_batch` | number | Override the description's trace settings (`--rays` and so on). |
| `no_filter` | bool | Skip the visibility-weighted filter. |

```json
{ "method": "editor.bake_gi", "params": { "scene": "maps/bore.bake.ron", "dry_run": true, "wait": true } }
```

## `editor.bake_status`

Read-only. Param `list` (bool, default false) adds `builtins` (the built-in scene names) and
`descriptions` (the project's `.bake.ron` files, project-relative, sorted; hidden folders and
`target` are skipped). The current or last bake:

- `job`: bakes started this session (the current one's number); `state`: `idle`, `running`,
  `succeeded`, `failed` or `cancelled`; `running`.
- `scene`, `dry_run`, `out`, `args` (the exact `regia` arguments).
- `progress`: `{ stage, done, total, fraction, remaining, eta_seconds }` from the CLI's progress lines, or null
  (`eta_seconds` only from a checkpointed bake, ADR-3095; `checkpoint` reports the job output's checkpoint). It keeps
  the last value after the bake ends.
- `summary` on success: `dry_run`, `out`, `bytes`, `probes`, `valid`, `invalid` (null for a dry
  run), `bricks`, `seconds`, `device`, and the CLI's whole JSON summary under `cli`.
- `error` on failure: `{ code, path, message }`.
- `log`: the last 40 lines of the bake's stderr.

Poll it until `running` is false to follow a bake: the call is cheap, drains the bake's output
and never blocks.

## `editor.bake_gi_cancel`

Mutating, permission `assets`. Kills the running bake; nothing is written. Returns the status
with `cancelled: true`, or `cancelled: false` when no bake was running.

## Error codes

A bake that starts and then fails is **not** an RPC error: the call succeeds and the status has
`state: "failed"` with `error.code`:

- the CLI's description codes, passed through: `bake_scene_unknown`, `bake_source_missing`,
  `bake_desc_invalid` (with `error.path`, the key path at fault) and the other codes on
  [Baking GI from meshes](baked-gi-bake.md);
- `bake_no_summary`: the CLI exited without a summary (no GPU adapter, a panic); `message`
  quotes its last stderr line and the exit status;
- `bake_failed`: the CLI printed `ok: false` without a code.

A bake that cannot start is an RPC error whose `data.bake_error` is:

- `bake_busy`: another bake is running (poll or cancel it first);
- `bake_spawn_failed`: the CLI could not be run (build it, or set `REGIA_CLI`);
- `bake_out_dir`: the output folder could not be created;
- `bake_wait_needs_dry_run`: `wait` was asked for a full bake.

## How to check a result

A dry run of a new description is the cheap check: `state: "succeeded"`, `summary.probes` and
`summary.bricks` match the layout you expect. After a full bake, `summary.invalid` counts the
probes the bake gave up on (inside geometry, under the terrain); open the volume in the level with
[Baked GI from the level](baked-gi-maps.md) and look at it in the viewport.
