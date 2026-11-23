# Create prefab from selection

The Outliner's row menu **Create prefab from selection…** turns entities of the open level into a
prefab: their subtrees are written to a new `.prefab.ron` at the current scene format version, and
they are replaced by one linked instance of it, every entity at the same world transform. An agent
does the same with `editor.create_prefab`. Both paths call `regia_editor::prefab_create::create_prefab`,
so the edit is **one entry** on the editor's undo stack (`editor.undo` / `editor.redo`). Spec 19 § 2
Outliner, ADR-5615 § 2, ADR-7468.

## `editor.create_prefab`

Params: `entities` (entity ids; absent = the editor selection), `path` (required: the new file,
project-relative under `assets/`, the `assets/` prefix optional, ending in `.prefab.ron`).

Returns `{ path, asset, instance, entities, replaced, undo_depth, retargeted }`: the project-relative file, the
prefab's asset id (its new `.meta`), the instance (the entity that carries `PrefabInstance`; it becomes
the selection), how many entity records the file holds, the top-level entities it replaced, the
undo depth afterwards, and how many entity references moved to the instance (below).

```json
{ "method": "editor.create_prefab", "params": { "entities": [4294967301], "path": "props/crate.prefab.ron" } }
```

### Which entity is the prefab's root

- **One top-level entity** (an entity whose ancestor is also given travels with that ancestor): it is
  the prefab's root. Its record keeps its components with an identity `Transform`; the instance takes
  the entity's local `Transform` and its place under the entity's parent.
- **Several**: a new root entity named after the file (`crate.prefab.ron` → `crate`) is the prefab's
  root, the members hang under it with their transforms relative to the centroid of their world
  positions, and the instance sits at that centroid under the members' common parent (the scene root
  when they differ), like Outliner ▸ Group.

The members' scene guids become the prefab's local ids. A prefab instance inside the selection (its
root selected) stays a nested instance in the new file. Instances of the new prefab respawn from the
file like any other (`editor.open_prefab` edits it, live reload updates them).

### Entity references

A component field that holds an entity (a joint's `connected`, a chase camera's `target`, a field set with the
Inspector's entity eyedropper) and named one of the members, from an entity outside the selection or from another
member, names the instance's entity for that member afterwards (ADR-8593). A member's reference to an entity outside
the selection is kept. Undo points the references back at the restored members and redo at the new instance again.
Scene files store entity fields as raw ids, so the references inside the written `.prefab.ron` hold for this
session only (as in level files); a reference by guid held by another file (a sequence binding) is not rewritten.

### Undo

Undo despawns the instance and brings the original entities back with their guids, parents, level and
place among their siblings, so a level saved after the undo is byte-identical to one saved before the
create. Redo places the same instance again. The written `.prefab.ron` and its `.meta` stay on disk
(an undo does not delete files).

### Refusals

Nothing is written or replaced. `data.reason` names the cause:

| `data.reason` | Meaning |
|---|---|
| `path_outside_project` | `path` is absolute, has `..`, a drive or UNC prefix, or leads out of the project through a symlink (ADR-1816). |
| `prefab_create_bad_path` | The file name does not end in `.prefab.ron`, has nothing before it, or names a `.variant` file. |
| `prefab_create_exists` | A file (or its `.meta`) already exists at `path`. |
| `prefab_create_internal` | A given entity lives inside another prefab instance (it has `PrefabLink`): select that instance's root, or call `editor.prefab_break_link` first. |
| `prefab_create_busy` | Prefab edit mode or play-in-editor is running. |
| `prefab_create_io` | Writing the file or its `.meta` failed. |
| `prefab_create_scene` | The selection did not snapshot, bake or instantiate as a prefab. |
| `hierarchy_no_entities`, `hierarchy_entity_missing`, `hierarchy_entity_internal`, `hierarchy_feed_owned`, `hierarchy_cycle`, `hierarchy_no_guid` | The [hierarchy edits](hierarchy.md)' own refusals (also in `data.hierarchy_error`). |

## In the editor

Right-click a row (the selection when the row is part of it) ▸ **Create prefab from selection…**. The
dialog offers `assets/prefabs/<name>.prefab.ron` after the first entity's name; edit it and press
**Create prefab**. A refusal shows under the field with its reason; **Cancel** closes the dialog.
`editor.actions { panel: "outliner" }` lists both controls with `editor.create_prefab` as their agent
path.

Tests: `cargo test -p regia_editor --lib prefab_create` (`created_prefab_loads_back_through_the_runtime_loader`,
`level_resaves_with_the_link_and_undo_restores_its_bytes`, `several_entities_get_a_new_root_at_their_centroid`,
`refusals_write_nothing`, `create_prefab_through_the_agent_method`,
`entity_references_follow_the_members_through_create_undo_and_redo`).
