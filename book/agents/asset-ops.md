# Asset file operations (agents)

The five asset methods of spec 23 § 3.3 that work on project files: patch a text asset, re-cook
one source, move or rename it, delete it, and change its import settings. They mirror the Content
Browser's file operations; everything below is project-relative under `assets/`, and any path that
escapes the project is refused with `path_outside_project` (absolute paths, `..`, symlinks,
Windows drive and UNC forms, device names), on every parameter that names a path —
`move_asset`'s destination too — and on every host. A path under a `goldens` directory is
`golden_path_denied` (only the owner changes goldens). All five need the `assets` permission and are `editor.*` methods:
they exist only on the editor surface (over MCP they are tools of the attached editor; a runtime
answers `editor_not_running`).

## `editor.edit_asset`

`edit_asset { path, patch }` patches a text asset and saves it canonically, as one undo entry
(undo restores the previous file text). Supported documents: `.material.ron`, `.prefab.ron`,
`.level.ron` and `.variant.ron`; anything else is refused with `asset_edit_unsupported_kind`.

The patch is a list of `{ path, value }` entries over the asset's JSON view, applied in order:

| Document | JSON view |
|---|---|
| `.material.ron` | The material's fields: `name`, `base.metallic`, `base.roughness`, `base.base_color`, … |
| `.prefab.ron` / `.level.ron` | The scene document: `entities` (array; numeric segments index it), `entities.0.name`, … Component values are **RON-text strings** (`entities.0.components["regia_scene::transform::Transform"]` is the component's RON, quoted). |
| `.variant.ron` | The variant document: `base.prefab`, … |

A patch path addresses existing fields only: a missing segment is `asset_edit_no_such_path`
(`data.path`, `data.segment` name it). A `value` that does not fit the field is
`asset_edit_bad_value`. The whole patch is checked before anything is written: a refused call
changes no file. The save is canonical — re-parsing and re-saving changes nothing further — and
the asset's `.meta` records agent provenance (spec 23 § 5). When the Material panel has the file
open with unsaved edits the call is refused (`material_unsaved_in_panel`, the same rule
`editor.edit_material` follows); when the panel has it open and clean, the panel adopts the saved
material. The level open in the editor is refused (`asset_level_open`): the editor would reload
it from the patched file, dropping the undo entry (or, with unsaved edits, asking the owner);
edit it with the scene methods (`editor.spawn`, `editor.add_component`, …) and
`editor.save_level`, or open another level first. Example:

```console
$ regia agent call editor.edit_asset '{"path":"assets/mat/crate.material.ron","patch":[
    {"path":"base.metallic","value":0.25},{"path":"base.roughness","value":0.5}]}'
```

## `editor.reimport`

`reimport { path }` re-cooks the source, and the sources that depend on it, with the cook cache
ignored — the Content Browser's "Reimport (forget cache)". The `.meta` stays as it is: the asset
id (saved levels and prefabs reference `asset:<guid>`), the import settings
`set_import_settings` wrote and the provenance record keep working. A kind the cook reads
directly (a level, a script) re-cooks nothing. Returns `{ path, cooked, skipped, errors }` like
`editor.import`. A missing file is `F0003`.

## `editor.move_asset`

`move_asset { from, to }` moves or renames a file (or a folder); its `.meta` sidecar moves with it
(the asset id is unchanged, so id-based references keep working). `to` follows the same path rule
as `from` (see above), and a segment that is empty or starts with `.` is `asset_bad_destination`
(so is a folder moved into itself). Since ADR-7822 the agent moves exactly like the Content
Browser's **Rename / Move…**: every string in another asset that names the old path is rewritten
to the new one (the rewritten files are in `fixed_referencers`), files that reference it only by
id (`asset:<guid>`) need no edit and are listed in `id_referencers`, the whole move is
all-or-nothing, and it is one undo step (`undoable: true`). An existing destination is
`asset_move_destination_exists`; nothing is moved on a refusal. Returns
`{ from, to, fixed_referencers, id_referencers, undoable }`. (Before ADR-7822 a path mention made
the call fail with `asset_move_referenced`; that refusal is gone.)

## `editor.delete_asset`

`delete_asset { path, confirm }` deletes a file (or folder) and its `.meta` — the spec 23 § 3.3
"needs confirmation" rule. The first call never deletes: without `confirm: true` it answers
`needs_confirmation`, or `asset_delete_referenced` with `data.referencers` when other assets
mention the file. Repeating with `confirm: true` deletes anyway (the Content Browser's "Delete
anyway") and reports what referenced the file. The delete itself is not undoable, matching the
Content Browser.

`delete_asset { path, replace_with, confirm }` (ADR-7822, the delete dialog's **Replace references
with…**) first points every reference at `replace_with`, an asset of the same kind: strings naming
the path are rewritten (the move rule), and the asset id too when both files have a `.meta`. Then
the file and its `.meta` are deleted. It still needs `confirm: true` (the first call answers
`needs_confirmation` with `data.referencers`), and it is one undo step that puts the file, its
`.meta` and every rewritten reference back. A folder cannot be replaced (`asset_replace_folder`);
the replacement's refusals are those of `editor.retarget_references` (`retarget_kind_mismatch`,
`retarget_target_missing`, `retarget_same_asset`; `book/agents/content-browser.md`). Returns
`{ path, deleted, referencers, replaced_with, rewritten }` (`replaced_with: null`, `rewritten: []`
without a replacement).

## `editor.set_import_settings`

`set_import_settings { path, settings }` writes the source's importer settings into its `.meta`
(`settings` is a RON settings object string, e.g. `"(max_size: Some(2048))"`); absent or `null`
clears them. The `.meta` settings override every `config/import_rules.ron` folder rule for this
source, and the incremental cook that follows applies them to the dev cache (the cook hashes the
settings). The source must have been cooked once: without a `.meta` the call is
`asset_not_imported`, and a malformed object is `asset_settings_invalid`. For images, glTF/GLB,
OBJ, FBX and sounds the object is also checked against the importer's settings type and ranges
(ADR-7469): a wrong type or an out-of-range value is `asset_settings_invalid` and nothing is
written. The Content Browser's **Import settings…** panel writes through this same path
([Import settings panel](import-settings.md)). Read the effective
settings with `assets.import_rules` ([import rules](import-rules.md)).

The write and the cook run as editor job `import_settings:<n>` (ADR-7664), the panel's job: the
call waits for it and adds `job` to its result, or with `async: true` answers `{ job }` at once
(poll `agent.job_status`; `editor.cancel_job { id }` puts the old `.meta` and cooked output back).
On success the content index is invalidated. One write runs at a time
(`import_settings_apply_running`); a blocking call is refused with `tools_busy` while another
holder has the project cook lock, where an `async` job waits for it.

`"(lossless_clips: true)"` on a glTF, FBX, USD or BVH source cooks its animation clips bit-exact
instead of compressed ([lossless animation clips](lossless-clips.md), ADR-7819).

## Errors

| Code (`data.reason`) | When |
|---|---|
| `path_outside_project` | Any path that escapes the project (`..`, absolute, drive/UNC, device names, symlinked), `move_asset`'s `to` included. `data.path_reason` names the rule. |
| `golden_path_denied` | A path under a `goldens` directory (ADR-2821). |
| `asset_edit_unsupported_kind` | `edit_asset` on anything but `.material.ron`, `.prefab.ron`, `.level.ron`, `.variant.ron`. |
| `asset_edit_no_such_path` | A patch path segment that does not exist. |
| `asset_edit_bad_value` | A patch `value` that does not fit the field. |
| `asset_edit_bad_patch` | A patch `path` that is empty or has an empty segment (`a..b`). |
| `asset_level_open` | `edit_asset` on the level open in the editor (use the scene methods). |
| `asset_bad_destination` | A `move_asset` destination with an empty segment or one starting with `.`. |
| `asset_move_referenced` | Retired by ADR-7822: `move_asset` rewrites path mentions instead of refusing. |
| `asset_move_destination_exists` | `move_asset` onto an existing path. |
| `needs_confirmation` | `delete_asset` without `confirm: true` (nothing deleted). |
| `asset_delete_referenced` | `delete_asset` without `confirm: true` of a referenced file (`data.referencers`). |
| `asset_not_imported` | `set_import_settings` on a source with no `.meta`. |
| `asset_settings_invalid` | `set_import_settings` with a malformed settings object, or a value the importer's settings type or range refuses (ADR-7469). |
| `import_settings_apply_running` | `set_import_settings` while another import-settings write (a panel Apply or another call) runs (ADR-7664). |
| `tools_busy` | `set_import_settings` without `async` while another holder has the project cook lock. |
| `import_settings_cancelled` / `import_settings_cook_failed` / `import_settings_io` | A blocking `set_import_settings` whose job was cancelled, whose cook failed (an internal error carrying this reason) or whose `.meta` could not be written; see [Import settings panel](import-settings.md). |
| `F0002` / `F0003` / `F0001` | Malformed parameters / a missing file or no open project / an IO or cook failure (see `logs.read`). |
