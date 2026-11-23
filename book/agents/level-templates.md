# Level templates (File ▸ New level)

File ▸ **New level** opens a dialog that starts a level from a template. The new level is always a copy: it is unsaved
(it has no path, so Save asks for one), and a template is never changed. ADR-7856.

| Id | What the level holds |
|---|---|
| `empty` | Nothing. |
| `basic_lit` | A sun (directional light), the default sky (the level's atmosphere setting), a 40 m floor and a player start. |
| `interior` | An enclosed 8 m room (floor, ceiling, four walls, all with colliders), two point lights and a player start. |
| `open_world` | A sun and a player start, on the project's terrain when it has a `*.terrain.layers.ron` under `assets/` (the level names it), else on a 400 m floor. |
| `project:<path>` | A copy of `templates/levels/<path>.level.ron` in the project (sub-folders up to four deep; `project:arena/small`). |

A project template is any level the editor wrote: build one, save it under `templates/levels/`, and it is in the list.
A copy gets a fresh level id, so two levels made from one template do not collide.

## `editor.level_templates`

```json
{"method": "editor.level_templates", "params": {}}
```

Answers `{ templates: [{ id, label, description, builtin, path, kinds }] }`. `kinds` is the entity kinds a level from
the template holds (`directional_light`, `point_light`, `player_start`, `plane`, `cuboid`); empty for `empty` and for a
project template (it holds whatever its file holds).

## `editor.new_level { template?, discard? }`

Starts the new level. `template` defaults to `empty`, the old behavior; `discard: true` drops unsaved edits. Answers
`{ template, level, entities, terrain }`. The palette command `file.new_level` (and `file.new_level_discard`) is
unchanged: an empty level.

Error codes (lowercase `data.reason`): `level_dirty` (unsaved edits, no `discard`), `prefab_open` (close the prefab
first), `level_template_unknown`, `level_template_unreadable` and `level_template_invalid` (a project template's
file, or a level from it that does not open), `level_template_failed`. A refused call leaves the open level as it
was: the new level is built and checked before the open one is unloaded. When a project template passes the checks
but fails to spawn (a prefab that nests itself), the open level is spawned back as it was a moment before, unsaved
edits included, under the same level id; its undo history starts over and its unsaved-changes flag is kept.

## The dialog and the keyboard

Up and Down move the highlighted template, Enter creates the level, Escape closes the dialog; **Create level** and
**Cancel** are buttons, and every row is a focusable widget (Tab). When the open level has unsaved changes, the dialog
shows **Discard unsaved changes in the open level**; Create is refused until it is ticked.
