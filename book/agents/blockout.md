# Blockout brushes and mesh baking

Create > Blockout places box, cylinder, wedge, stairs and sphere brushes. Choose Add,
Subtract or Intersect before placing a brush. Brushes with the same group name combine
in ascending order; equal orders use entity GUID order. The accumulated solid starts
empty, so the first useful brush normally uses Add. Dimensions are full X/Y/Z sizes
in meters; cylinders run along Y. Ramps and stairs rise in Y toward positive Z.

Select a brush in the Outliner. Create > Blockout exposes its group, operation, dimensions,
steps, order and material slot. Tab reaches each field, arrow keys adjust numeric fields,
and typing replaces a value. With the viewport handle tool enabled, drag a size grip or
the stairs' step-count grip. A completed drag is one undo step. Theme tokens and the
editor's appearance.ui_scale apply to the controls and grips.

The preview waits for two unchanged engine frames and runs as a cancellable editor job.
An old worker result never replaces a newer edit. An idle group starts no jobs.
Only one blockout job runs at a time. A group with no surface has no preview.

## `editor.blockout_add`

Required: `shape` (`box`, `cylinder`, `wedge`, `stairs`, `sphere`), `op` (`add`,
`subtract`, `intersect`). Optional:

| Parameter | Meaning and default |
|---|---|
| `group` | Nonempty group name, at most 128 bytes; selected brush's group, else `Blockout`. |
| `size` | Three positive finite dimensions, greater than 0.00001 and at most 10000; `[2,2,2]`. |
| `steps` | Integer 1..64; default 6. Used by stairs. |
| `order` | Signed 32-bit evaluation order; default one after the group's last brush. |
| `material` | Integer face material slot 0..255; default 0. |
| `translation` | Three finite world coordinates; default the viewport focus point. |

Returns `{ entity, group }`. Creates and selects one entity in one undo step.
A brush's transform, including rotation and scale, remains editable through the ordinary
Inspector and `editor.set_property`. New brushes are unparented.

## `editor.blockout_edit`

Required: `entity` (brush entity bits). Optional: `shape`, `op`, `group`, `size`, `steps`,
`order`, `material`, with the meanings above. Unspecified fields stay unchanged.
Returns `{ entity }`. Invalid edits change nothing; successful edits use the shared undo
stack and invalidate the preview. This is also the numeric fields' and grips' edit path.

## `editor.blockout_bake`

Required: `group`. Optional `out` is a new project-relative `assets/` path ending in `.glb`;
when absent it uses a unique path under `assets/blockout/`. Optional `collider` (default
false) adds a static triangle-mesh collider preserving cavities. Existing files are refused.

Returns `{ job }` immediately. Poll `agent.job_status { "job": "blockout:<n>" }` until
`finished` is true. Success has `state: "done"` and result
`{ entity, out, triangles, collider }`. `editor.cancel_job { id }` or
`agent.job_cancel { job }` cancels it. A changed/deleted group discards its worker result. Detach any child outside the group
before baking; the bake refuses such children, including ones attached while it runs.

The worker evaluates the solid and writes a temporary GLB through the existing exporter.
Completion publishes the file and replaces the brushes by one placed `MeshSource` in one
undo step. `editor.undo` restores the brushes; `editor.redo` restores the placed mesh.
Undo retains the asset so redo and other references continue to work. Material slots
become glTF primitives with named neutral materials; cut faces keep the cutter's slot.
Assign finished materials after baking. Baked meshes use the normal editor/runtime loaders.

```json
{"method":"editor.blockout_add","params":{"shape":"box","op":"add","group":"Room","size":[2,2,2],"translation":[0,0,0]}}
{"method":"editor.blockout_add","params":{"shape":"cylinder","op":"subtract","group":"Room","size":[1,3,1],"translation":[0,0,0],"material":1}}
{"method":"editor.blockout_bake","params":{"group":"Room","collider":true}}
```

Wait for any preview job to finish before starting the bake. Preview jobs and bake jobs
share the `blockout` job kind and background-task row; the bake reply identifies the bake.
Use `seeing.screenshot` or `editor.capture_viewport` after completion to inspect the result.

## Errors and limits

Errors use lowercase protocol codes: `blockout_params` (wrong parameter type or enum),
`blockout_invalid_brush` (dimensions, group, steps or material out of range),
`blockout_missing` (unknown group or entity without a brush), `blockout_busy` (another job),
`blockout_hierarchy` (a brush has children outside the group),
`blockout_path` (output is not an assets/ GLB), `blockout_exists` (output exists),
`blockout_failed` (serialization, worker, geometry or file operation failed).
The shared path checks use `path_outside_project`; entity lookup uses the shared
`F0002` / `F0003` contract. Worker diagnostics include `blockout_empty`,
`blockout_cancelled`, `blockout_complexity_limit`, `blockout_invalid_brush` and
`blockout_spawn_failed` (a prepared entity could not be found after creation).

Groups are limited to 128 brushes, BSP depth to 256, polygon work to two million split
visits, and welded boundary corners to 8192. Cylinders use 64 sides; spheres use 16 sides
and eight latitude intervals. Near-degenerate or overly complex geometry may be refused.
Brushes are editor authoring components; bake before shipping a runtime level. No existing
frozen content layout or version changes (ADR-7809).
