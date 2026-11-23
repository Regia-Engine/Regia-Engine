# Import settings panel and mesh viewer overlays (agents)

Two editor surfaces of spec 19 § 2 (ADR-7469): the Content Browser's **Import settings…** panel,
which edits one source's importer settings with typed fields, and the mesh viewer's overlays (UV
layout, vertex normals, the cooked collider). Both are `editor.*` methods on the editor surface.

## The Import settings panel

Right-click an image (`.png`, `.jpg`, `.tga`, `.bmp`, `.hdr`, `.exr`), a mesh (`.glb`, `.gltf`,
`.obj`, `.fbx`) or a sound (`.wav`, `.ogg`, `.flac`, `.mp3`) in the Content Browser and pick
**Import settings…**. The panel shows the fields of that importer's settings type (the types
`regia_asset_pipeline` re-exports: `TextureImporterSettings`, `GltfImporterSettings`,
`ObjImportSettings`, `FbxImportSettings`, `AudioImporterSettings`; OBJ and FBX also show the glTF
mesh keys, which apply to their converted scenes). A field left at **default** is absent from the
`.meta` and the importer's default applies. Keys the form has no widget for are kept as written.

**Apply** writes the form through the same function as `editor.set_import_settings`: the same
values produce the same `.meta` bytes, and one Apply cooks the project once (the cook hashes the
settings, so the source re-cooks; `recooks` counts it). While a package, a Clear caches, an
agent cook or a Content ▸ Import job runs, Apply is disabled and `editor.import_settings_apply`
refuses with `tools_busy`. **Revert** drops the unsaved edits.

**The write is an editor job** (ADR-7664): `import_settings:<n>` in `agent.job_status`, with the
background task `import_settings`. The `.meta` write and the cook run on a worker thread, so the
UI never cooks: the window shows a spinner, the job's step and **Cancel** while it runs (`applying`
in the state names the job), and the next frame after it ends makes the form the saved state
(`recooks` +1) and invalidates the content index (the Content Browser rescans). One write runs at
a time: a second Apply, `editor.import_settings_apply` or `editor.set_import_settings` is refused
with `import_settings_apply_running`, not queued. When another holder has the project cook lock
(the editor's live cooker, `regia cook` in a terminal), the job waits for it on its worker and
can be cancelled meanwhile; a blocking method call (no `async`) is refused with `tools_busy` at
once instead, so it never waits on the lock.

**Cancel** (the window's button, `editor.cancel_job { id }`, `editor.cancel_task { id:
"import_settings" }`) leaves the old settings and the old cooked output. A cancel before the
write writes nothing; a cancel after the write puts the old `.meta` back; a cancel during the
cook (which cannot stop part-way) puts the old `.meta` back and cooks it again. The job ends
`cancelled` (`{ path, cancelled, recooked }`), the form is kept and `error` is
`import_settings_cancelled`. A cook that fails puts the old `.meta` back too.

| Method | What it does |
|---|---|
| `editor.import_settings_open { path }` | Opens the source in the panel (the row menu's **Import settings…**). Returns the state. |
| `editor.import_settings_edit { field, value }` | Sets one field: a bool, a choice string, a number, a string, or RON text for a raw block (`generate_lods`, `limits`, `collision_hulls`, ...). `null` removes it. Not written until apply. |
| `editor.import_settings_apply { async? }` | **Apply** as job `import_settings:<n>`: waits for it and answers `{ result, state, job }` (`result` has `editor.set_import_settings`'s shape: `path`, `settings`, `cooked`, `skipped`, `errors`); with `async: true` answers `{ job, state }` at once. |
| `editor.import_settings_revert {}` | **Revert**. |
| `editor.import_settings_state {}` | The state: `{ open, path, importer, dirty, settings, saved, fields: [{key, label, kind, set, value, default, choices?}], other_fields, problem, error, invalid, can_apply, recooks, last_apply, applying }`. A write that ended since the last frame is applied first. |
| `editor.set_import_settings { path, settings?, async? }` | The same write without the panel (`book/agents/editor.md`), as the same job: waits and answers `{ path, settings, cooked, skipped, errors, job }`, or `{ job }` at once with `async: true`. |

`importer` is `texture`, `gltf`, `obj`, `fbx` or `audio`; a field's `kind` is `opt_bool`, `bool`,
`opt_choice`, `choice`, `opt_u32`, `f32`, `opt_f32`, `opt_string` or `raw`. `settings` is the RON
text Apply would write (`null` when the form is empty, which clears the override). `invalid` lists
the fields whose last edit was refused (Apply is disabled, `can_apply` false, until each is fixed
or **Revert** drops the edits). A successful `editor.import_settings_edit` of a field clears that
field's refused mark and its text buffer, so the agent's value is what the UI shows; it keeps
another still-invalid field's refusal message (`error`) visible.

`editor.set_import_settings` checks values the same way since ADR-7469: a value of the wrong type
or outside the importer's range (`max_size` 1 to 16384, `cube_face_px` 4 to 8192,
`alpha_cutoff` 0 to 1, `scale` above 0, FBX `sample_rate` 1 to 240, `target_lufs` -70 to 0, the
`collision_hulls` decomposition ranges, an unknown enum spelling) is `asset_settings_invalid`, and
nothing is written or cooked.

Example: lower a texture's size cap and turn compression off.

```json
{"method": "editor.import_settings_open", "params": {"path": "assets/props/crate.png"}}
{"method": "editor.import_settings_edit", "params": {"field": "max_size", "value": 512}}
{"method": "editor.import_settings_edit", "params": {"field": "compress", "value": false}}
{"method": "editor.import_settings_apply", "params": {}}
```

## Mesh viewer overlays

The Asset Viewer's mesh view has toggles **UV0**, **UV1**, **Normals** and **Collision** (all off
by default). `editor.view_asset { path, overlay }` draws the same overlays over its render;
`overlay` is one name or a list:

| Overlay | Draws |
|---|---|
| `uv` (`uv0`) | The `TEXCOORD_0` triangles in a centered square over a darkened backdrop (glTF convention: `(0, 0)` top left). |
| `uv1` | The same for `TEXCOORD_1` (the lightmap set). |
| `normals` | A line from every vertex along its normal (6 % of the bounds' diagonal; at most 20 000 lines), colored `n * 0.5 + 0.5`. |
| `collision` | The cooked collider: the silhouettes of the convex hulls the cook wrote for `collision: ConvexDecomposition` (read from the dev cache, mapped back through the importer's axis and scale), else the triangles of the file's `collision_*` nodes. |

The result's `info.overlays` lists what was drawn (`{ overlay, set, triangles }`,
`{ overlay, normals }` or `{ overlay, source, hulls, triangles }`).

## Errors

| Code (`data.reason`) | When |
|---|---|
| `import_settings_unsupported` | `import_settings_open` on a file the panel has no form for. |
| `asset_not_imported` | `import_settings_open` on a source with no `.meta` (cook it once first). |
| `import_settings_not_open` | `import_settings_edit`, `_apply` or `_revert` with no source open. |
| `import_settings_unknown_field` | `import_settings_edit` of a key that is not a field of this importer's form. |
| `asset_settings_invalid` | A value of the wrong type or out of range (`import_settings_edit`, `import_settings_apply`, `editor.set_import_settings`). In the panel, a text field whose typed value was refused keeps the last accepted value in the form; **Apply** is disabled (and `import_settings_apply` answers this code) until the field is fixed or **Revert**ed, so a half-typed value is never written. |
| `tools_busy` | `import_settings_apply` while a package, a Clear caches, an agent cook or a Content ▸ Import job runs (Apply is disabled then), or a blocking (no `async`) `import_settings_apply` or `editor.set_import_settings` while another holder (the live cooker, `regia cook`) has the project cook lock. Retry once it ends, or pass `async: true` (the job waits for the lock); the form is kept. |
| `import_settings_apply_running` | Apply, `import_settings_apply` or `editor.set_import_settings` while another import-settings write runs (one at a time; refused, not queued). |
| `import_settings_cancelled` | A blocking call whose write was cancelled (the panel's `error` after any cancel): the old `.meta` and cooked output are back. |
| `import_settings_cook_failed` | The cook after the write failed (a blocking call answers it as an internal error with this reason); the old `.meta` is back. |
| `import_settings_io` | The `.meta` could not be written (or put back after a cancel). |
| `import_settings_failed` | The write's worker stopped unexpectedly. |
| `job_start_failed` | The write's worker thread could not start. |
| `path_outside_project` | A path that escapes the project. |
| `overlay_unknown` | `view_asset` with an overlay name other than `uv`, `uv0`, `uv1`, `normals`, `collision`. |
| `overlay_not_mesh` | `view_asset` with an overlay on a texture or a sound. |
| `uv_set_missing` | `overlay: "uv1"` (or `uv`) on a mesh with no such texture coordinate set; a `builtin:` mesh has UV0 only. |
| `no_collision` | `overlay: "collision"` on a mesh with no cooked hulls and no `collision_*` nodes. |
| `collision_not_cooked` | `overlay: "collision"` when the `.meta` asks for `ConvexDecomposition` but the dev cache holds no hulls (cook first). |

Tests: `cargo test -p regia_editor --lib import_settings_panel` (the job of ADR-7664:
`the_ui_apply_runs_as_a_job_and_invalidates_the_content_index`, `a_second_apply_while_one_runs_is_refused`,
`a_cancel_during_the_cook_puts_the_old_settings_back`, `a_cancel_while_waiting_for_the_cook_lock_writes_nothing`,
`blocking_methods_name_their_job`, `set_import_settings_async_answers_its_job_at_once`, each with a counted fake
cook gated on a channel) and `asset_viewer` (the dispatcher
test `view_asset_overlays_through_the_dispatcher` needs lavapipe or a GPU).

## Reset and several sources (ADR-7744)

Every field has a **Reset** button: it takes the field back to the importer's default (the key leaves the `.meta`), the
same as `editor.import_settings_edit { field, reset: true }` or `value: null`.

`editor.import_settings_open { path, paths }` opens several sources of one importer at once (`path` first, then `paths`;
a repeated path counts once). The state lists them in `sources`. An edit or a Reset goes to every source. A field whose
values differ across the sources reads `mixed: true` with a null `value` (and `set: false`) until an edit or Reset sets
one value for all. **Revert** drops the edits of every source. **Apply** writes each source whose form changed, cooks the
project once, and puts every old `.meta` back on a cancel or a failed cook; the result carries `paths`. Sources of
different importers are refused with `import_settings_mixed_importers`; the other codes are the single-source ones.

## Mesh helper actions (ADR-7771)

The mesh viewer's **Helpers** row and the Content Browser's mesh menu (right-click a `.glb` or `.gltf`)
carry three actions, all `editor.mesh_helper { path, action }`:

- **Add simple collision** (`action: "add_collision"`, `shape?`) writes the `.meta` collision request through
  the import-settings write (the job and the locks of `editor.set_import_settings`, so `async`, `tools_busy`
  and cancel work the same) and recooks. The source's current settings (its own, else what
  `config/import_rules.ron` gives its path) are kept; only `collision` and, for `convex`, `collision_hulls`
  change. `shape`: `convex` (default; one hull of the whole mesh: `collision: ConvexDecomposition` with
  `collision_hulls: (max_hulls: 1)`), `decomposition` (several hulls, the existing hull settings kept) or `none`
  (`collision: None`, taking the request back). The collision overlay shows the cooked hulls afterwards. The
  importer's `.meta` has no box, sphere or capsule request, so those are not offered here.
- **Generate lightmap UV** (`action: "lightmap_uv"`, `size?`) unwraps the mesh with the generator and default
  settings `regia bake-lightmaps` uses, this mesh alone, and shows the atlas. The viewer draws the charts'
  triangles in the unit square under the render with the figures; the Content Browser item opens the viewer
  on the mesh and does the same. The method answers `{ action, path, atlas: { charts, width, height,
  texels_per_metre, chart_fill, degenerate_triangles, source_triangles, source_vertices, unwrapped_vertices,
  triangles, generator }, width, height, mime, png_base64 }`. Nothing is written.
- **Build nav preview here** (`action: "nav_preview"`, `async?`) builds the viewport's navigation preview for the
  open level, as `editor.nav_preview { action: "build" }`, and answers its state plus the `nav_preview:<n>` job.

`add_collision` answers the `editor.set_import_settings` result plus `action`, `shape` and the `settings`
written (with `async: true`: `{ job, action, shape, settings }`).

```json
{ "method": "editor.mesh_helper", "params": { "path": "builtin:cube", "action": "lightmap_uv" } }
{ "method": "editor.mesh_helper", "params": { "path": "assets/props/rock.glb", "action": "add_collision", "shape": "convex" } }
```

| Code (`data.reason`) | When |
|---|---|
| `mesh_helper_unknown_action` | `action` is not `add_collision`, `lightmap_uv` or `nav_preview`. |
| `mesh_helper_unknown_shape` | `shape` is not `convex`, `decomposition` or `none`. |
| `mesh_helper_not_importable` | `add_collision` on a `builtin:` shape or a file that is not `.glb` / `.gltf`. |
| `mesh_helper_no_project` | `add_collision` with no project open. |
| `mesh_helper_no_triangles` | `lightmap_uv` on a mesh with nothing to unwrap. |
| `mesh_helper_unwrap_failed` | The generator refused the mesh (its `lightmap_uv_*` code is in the message). |
| `asset_not_imported`, `tools_busy`, `import_settings_apply_running`, `asset_settings_invalid` | As for `editor.set_import_settings` (`add_collision`). |
| `nav_preview_busy`, `nav_preview_too_large`, ... | As for `editor.nav_preview` (`nav_preview`). |

The mesh viewer also draws with the shared preview scene (lighting preset, environment intensity, floor,
auto-rotate): `editor.preview_scene { editor: "mesh" }` (book `agents/material-graph-editor.md`).

Tests: `cargo test -p regia_editor --lib asset_viewer` (`add_simple_collision_writes_the_meta_and_the_recook_has_one_hull`,
`generate_lightmap_uv_unwraps_a_cube_into_charts_inside_the_unit_square`,
`the_mesh_helper_buttons_are_keyboard_operable_and_named`, `mesh_helper_and_preview_scene_through_the_dispatcher`).


## Import presets (ADR-7904)

A preset is a named importer settings object for one importer kind (`texture`, `gltf`, `obj`, `fbx`, `audio`), shared
by the project in `config/import_presets.ron` ([format](../../docs/formats/import-presets-ron.md)). A preset never
applies by itself (project-wide defaults are import rules); the user or an agent applies it.

In the Import settings window, the **Preset** row picks a preset of the open importer: **Apply preset** replaces the form
of every source the panel edits with the preset's settings (each source keeps its own `clips`), and **Delete preset**
removes it. **Save as preset** stores the form under the typed name (an existing preset of that name and importer is
replaced, ignoring case). Nothing is written to a `.meta` until **Apply** (one cook for all edited sources).

| Method | What it does |
|---|---|
| `editor.import_presets { action: "list", importer? }` | `{ path, presets: [{ name, importer, settings }] }`, optionally one importer. |
| `editor.import_presets { action: "save", name, settings?, importer? }` | Saves the open panel's form (or `settings` + `importer`) as `name`; answers the list plus `saved` and `replaced`. The per-file `clips` key is never stored. |
| `editor.import_presets { action: "apply", name, paths?, write? }` | Applies the preset to the panel's sources (`paths` opens them first, one importer); `write: true` then runs the panel's Apply (every edited `.meta`, one cook) and answers `{ applied, result, state }`. |
| `editor.import_presets { action: "delete", name, importer? }` | Removes the preset (`importer` defaults to the open panel's). |

## The import dialog (ADR-7904)

Content ▸ Import… with several files, or with a glTF/GLB, FBX or OBJ, opens the import dialog instead of importing at
once; so does **Re-import with options…** on an imported model in the Content Browser. The dialog shows each file in
turn (pick it in the file row; **Skip file** leaves it out). For a model it shows:

- a **preview** (the asset viewer's orbit render at dialog size; drag it, or focus it and use the arrow keys);
- the **scene tree**: meshes (parts, triangles) with **Import mesh**, materials (base color, texture count) with their
  **Use** choice (both glTF/GLB only, see below; read-only for FBX/OBJ), and the **animations**. Each animation becomes a clip: **Import clip** (off skips it), **Clip name**, and ranges (**Add
  range**, **Remove range**) that split one take into several clips, edited in frames at the importer's 30 fps clip
  rate (stored in seconds);
- the **importer settings** form (the Import settings grid) and the project presets of the importer (**Apply preset**).

**Import** writes each new file's `.meta` (a fresh id, the settings) next to its destination before the files are
copied, so the import's one cook already uses them; a re-import writes the existing `.meta` and cooks once. **Apply to
all** copies the shown file's settings (not its clips) to every other file of the same importer, then imports. From the
window the import runs as the Content ▸ Import job (a re-import as the `import_settings:<n>` write job).

The clip options are the `.meta` settings key `clips`, which the cook passes to the clip import (glTF, and the GLB
FBX/OBJ convert to):

```ron
(clips: [(animation: "Take 001", ranges: [(name: "idle", start: 0.0, end: 1.0), (name: "run", start: 1.0, end: 1.8)]),
         (animation: "T-pose", skip: true),
         (animation: "Jump_v2", name: Some("jump"))])
```

An animation no entry names imports as one clip under its own name; a range that cannot cut a clip (end not after start,
not finite) is dropped with a warning at cook. A range with an empty name is named `<clip>_<index>`.

### Meshes and materials (glTF/GLB, ADR-8376)

Each row of **Meshes** is one mesh-bearing node as the importer cooks it, named by its mesh sub-asset key: the node name
(`mesh/<key>/primitive_<i>`), with `~<node index>` for a later node of the same name. **Import mesh** off leaves the node
out: no mesh sub-asset, no part of the cooked `#model`; the other nodes keep their paths and ids. Skipping every mesh is
refused (skip the file instead).

Each material has a **Use** choice:

- **Embedded**: the material cooked from the file (`file#material/<key>`), as before;
- **Extract**: at Import the embedded material is written as `<model folder>/<model stem>_<key>.material.ron` with its own
  `.meta` (its texture slots name the model's texture sub-assets), and the model's parts draw with that asset. The
  project cook cooks it with the model. A re-import keeps the file as it is (edits to it stay);
- **Replace**: the parts draw with a material asset of the project, picked from the list of every `.material.ron` under
  `assets/` with a `.meta`.

The embedded material stays cooked in every mode, so references to `file#material/<key>` keep working; only the model's
parts move. The choices are the `.meta` settings keys `meshes` and `materials` (an importer setting inside the
`settings` string, no format version step), so **Re-import with options…** shows them and a re-import keeps them:

```ron
(meshes: [(mesh: "collision_box", skip: true)],
 materials: [(material: "paint", mode: Extracted(asset: "<id>", path: "assets/models/robot_paint.material.ron")),
             (material: "trim", mode: Replaced(asset: "<id>", path: "assets/materials/blue.material.ron"))])
```

The keys follow the source's names, so they survive reordered nodes and materials; a renamed node or material is a new
key (its old option no longer applies). A skipped mesh is not reported as a vanished sub-asset. FBX and OBJ files list
their meshes and materials read-only (their importer settings do not take these keys yet).

Not yet: the generated read-only prefab of the model's hierarchy (P-166).

| Method | What it does |
|---|---|
| `editor.import_dialog_open { paths, folder? }` / `{ reimport }` / `{ close: true }` | Opens the dialog as Content ▸ Import… / Re-import with options… do; answers the state. `close: true` is the dialog's Cancel: it closes the dialog, imports nothing and answers the state. |
| `editor.import_dialog_state` | `{ open, mode, folder, current, files: [{ source, name, reimport, importer, skipped, settings, invalid, problem, preview, part_options, scene: { meshes: [{ name, key, import, primitives, triangles }], materials: [{ name, key, mode, path, asset, base_color, textures }], clips: [{ animation, duration, frames, import, name, ranges: [{ name, start, end, start_frame, end_frame }] }] } }], error, last_result, material_assets: [{ path, asset }] }`. `mode` is `embedded`, `extract` or `replace`; `part_options` is true for glTF/GLB. |
| `editor.import_with { paths \| path \| reimport, folder?, file?, settings?, preset?, items?, skip?, apply_to_all? }` | What Import does, in one call: `paths`/`reimport` build the dialog model for this call (the window is untouched); without them the open dialog is imported and closes. `items`: `[{ kind: "clip", animation, import?, name?, ranges?: [{ name, start, end } or { name, start_frame, end_frame }] }]`, `{ kind: "mesh", mesh: <key>, import }` and `{ kind: "material", material: <key>, mode: "embedded" \| "extract" \| "replace", path? }` (extract: an optional `assets/….material.ron`, default next to the model; replace: a path from `material_assets`) for the file at `file` (default the shown one). Answers `{ imported, cooked, skipped, errors, cancelled, settings }` (a re-import: `editor.set_import_settings`' result plus `settings`). |

| Code (`data.reason`) | When |
|---|---|
| `import_presets_unreadable` | `config/import_presets.ron` does not read or parse (it is never overwritten then). |
| `import_presets_io` | The presets file could not be written. |
| `import_preset_not_found` | Apply or delete of a preset the importer does not have. |
| `import_preset_name_invalid` | A blank, too long (over 128 bytes) or control-character name. |
| `import_preset_importer_unknown` | `importer` is not `texture`, `gltf`, `obj`, `fbx` or `audio`. |
| `import_dialog_not_open` | `import_with` without `paths`/`reimport` and no open dialog. |
| `import_dialog_nothing` | Every file is skipped. |
| `import_destination_exists` | A file (or its `.meta`) already exists in the destination folder, or an extract path holds a file that is not this extraction (another `.meta` id); nothing is written. |
| `import_item_unknown` | A clip, mesh or material item names an animation, mesh key or material key the file does not have, or `skip` names a file not in the dialog. |
| `import_item_unsupported` | An item kind other than clip, mesh and material, or a mesh/material item on a file that is not glTF/GLB. |
| `import_material_not_found` | A material item's replace `path` is not a material asset of the project (a `.material.ron` under `assets/` with a `.meta`). |
| `import_failed` | The `.meta` could not be written before the copy, the import could not start, or an extracted material could not be read or written. |
| `asset_settings_invalid`, `tools_busy`, `asset_not_imported`, `path_outside_project` | As in the Import settings panel above. |

Tests: `cargo test -p regia_editor --lib import_presets import_dialog` (`save_apply_to_a_selection_and_delete_a_preset`,
`import_with_splits_a_take_and_skips_another_in_the_first_cook`, `apply_to_all_sets_every_file_of_the_importer`,
`reimport_with_options_renames_a_clip`, the keyboard tests `the_presets_row_saves_from_the_keyboard` and
`the_dialog_imports_from_the_keyboard`), `cargo test -p regia_asset_pipeline --lib clip_options gltf_anim`
(`meta_clip_options_split_skip_and_rename_the_cooked_clips`, `per_animation_clip_options_split_rename_and_skip`) and
`cargo test -p regia_asset --lib import_presets`. Meshes and materials (ADR-8376): `cargo test -p regia_editor --lib
import_dialog` (`a_skipped_mesh_is_not_cooked`, `an_extracted_material_is_written_and_referenced`,
`a_replaced_material_points_at_the_chosen_asset`, `choices_survive_a_reimport`, `part_option_refusals_carry_their_codes`,
the keyboard test `mesh_and_material_rows_work_from_the_keyboard`) and `cargo test -p regia_asset_pipeline --lib gltf`
(`gltf_part_keys_match_the_imported_mesh_and_material_paths`, `a_skipped_gltf_mesh_is_not_imported_and_no_other_path_moves`,
`a_replaced_gltf_material_rebinds_the_model_parts`).
