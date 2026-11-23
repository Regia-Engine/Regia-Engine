# Open kit

## `editor.open_kit`

Read-only. Opens a Tincal kit folder in the editor's Kit panel (the same state **File ▸ Open kit…**
uses) and returns what is in it. Params: `path` (the kit folder, absolute or relative to the open
project, required), `clip` (play this clip in the animation preview), `render` (also render the kit,
default false), and for the render `lod`, `width`, `height` (default 320×240, clamped to
64..=2048), `yaw`, `pitch` and `sockets` (paint socket marks, default true).

```json
{ "method": "editor.open_kit",
  "params": { "path": "/home/me/kits/jmc_hauler_v3", "clip": "idle", "render": true } }
```

Returns:

- `kit_id`, `path` (the canonical folder), `manifest` (`kit_id`, `kind`, `class`, `faction`,
  `replaces`, `stats_from`, `contract`, `version`), `character_kit` (the skeleton root's
  `{"tincal_kit": 1}` extras).
- `model` and `cockpit` (or null): `file`, `bytes`, `nodes`, `mesh_nodes`, `skins`,
  `skeleton_root`, `lod_levels`; the cockpit also has `attached_to`: `"socket_cockpit"` or
  `"origin"`.
- `lods[]`: `level`, `authored` (from `_LOD<n>` nodes, else generated), `triangles`.
- `clips[]`: `name`, `duration` (seconds), `source` (`model` or `rig`), `rig_secs`, `joint`.
- `sockets[]`: `name`, `position`, `parent`, `in_model`, `in_sockets_json`.
- `checksums`: `verdict` (`verified`, `failed`, `absent`, `unreadable`), `entries[]` (`file`,
  `expected`, `status`: `ok`, `mismatch` with `actual`, `missing`, `refused` with `reason`),
  `unlisted`, `malformed_lines`, `error`.
- `warnings[]`: `file`, `message`.
- `preview_rig` (the model is the animation preview rig), `clip_played` (null without `clip`).
- `budget`: the kit's budget against its class's default limits, the same object as
  `editor.budget { "source": "kit" }` returns ([budget meters](editor-budget.md)); null when the
  model was not measured.
- With `render`: `render` = `{ width, height, lod, mime, png_base64, socket_marks: [{name, x, y}] }`.

A folder without `kit.json` or its model, or with either unsafe, too large or malformed, is an
error whose `data.kit_error` is one of `not_a_folder`, `missing_manifest`, `bad_manifest`,
`invalid_kit_id`, `missing_model`, `unsafe_path`, `too_large`, `io`, `bad_model`. Everything else
(a bad sidecar, a checksum mismatch, a symlink out of the folder) is a `warnings[]` entry.

`checksums.entries[].file` is the name as listed in `SHA256SUMS`: a flat name or an in-folder
subpath such as `textures/x.png`. A listed name with `..`, a leading `/`, a backslash, a colon, NUL,
an empty component, or a symlink out of the folder is `status: "refused"` and the verdict `failed`.
`manifest.faction` may be `generic` (no warning). With a cockpit glb at `socket_cockpit`, the hull
glb must not also hold the cockpit geometry: the editor draws both and hides no nodes.

How to check a result: a tampered file is a `checksums.entries[]` row with `status: "mismatch"`
and a warning `"<file>: checksum mismatch: ..."`; a cockpit at the wrong place is
`cockpit.attached_to: "origin"` with a warning naming `socket_cockpit`. After `clip`, see the
clip play with `editor.capture_panel { "panel": "clip" }`, or step it with `editor.anim_clip`.
See [Open kit](../user-guide/open-kit.md) for the folder layout and the limits.

`path` is required. `clip` is optional; `render` defaults false. When rendering, `lod` defaults to 0
and must index an available LOD; `width`/`height` default to 320×240 and clamp to 64–2048, `yaw` and
`pitch` default to 0.6 and 0.35 radians, and `sockets` defaults true. Missing folder/manifest/model
uses `F0003`; refused or malformed kit data uses `F0002`, with the specific `data.kit_error` values
listed above. Workflow: open the folder, inspect warnings/checksums and available clips, request a
render at a selected LOD, then capture the clip panel if animation needs review.

Captured failing call (lavapipe; required `path` missing):

```console
$ regia agent call editor.open_kit '{}' --project samples/arena
error: method call failed: `path` is required (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
