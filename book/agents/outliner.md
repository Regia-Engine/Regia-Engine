# Outliner tree: search queries, folders, rename in place, keyboard (agents)

The Outliner (`docs/specs/19-editor.md` § 2, ADR-7757) lists the level's entities as a tree. This page
covers its search query language, folders, in-place rename, expand and collapse, filter-aware
selection, Paste as child and the keyboard. The tree edits (reparent, group, duplicate, delete) are on
[Outliner hierarchy edits](hierarchy.md); the row window is on
[Editor panels at scale](editor-panel-timings.md).

Every panel action has an agent method and calls the same code. Edits to the level are one undo step
each; expand, collapse and the search are view state and never touch the undo stack.

## Search queries

The search box (Ctrl+F focuses it) and the `filter` parameter of `editor.hierarchy` take the same
query. A row stays listed when it matches or when one of its descendants matches; while searching,
every listed row is open.

| Term | Matches |
|---|---|
| `door`, `"front door"` | the name contains the text (case-insensitive); bare words side by side are one phrase: `crate lid` looks for "crate lid" |
| `name~door`, `Name~door` | the name contains `door` |
| `name=door`, `name!=door` | the name is (is not) `door`, ignoring case |
| `kind:light`, `kind:none`, `kind:?` | the entity's kind; no kind; unclassified (spec 101 § 4.2) |
| `type:PointLight`, `has:PointLight` | the entity has a component of that short type name |
| `tag:loot` | the entity's `Tags` contain `loot` |
| `under:Lights`, `under:Lights/Street`, `under:/Lights` | an ancestor named `Lights` (a path: an ancestor `Street` whose parent is `Lights`; a leading `/` anchors at a root). Folders count as ancestors. |
| `parent:Lights` | the direct parent (or the innermost folder of a top-level entity) is `Lights` |
| `PointLight.intensity>1000`, `Transform.translation.y<=0` | a component field compared with a number (`<`, `<=`, `>`, `>=`, `=`, `!=`); `Light.intensity` matches any component whose short name ends with `light`, such as `PointLight` and `SpotLight` |
| `Visibility=Hidden`, `PointLight.cast_shadows=true` | a field (or a whole component) equal to an enum variant, a boolean or text; `~` compares text containment |
| `a and b`, `a b` | both (side by side means `and`) |
| `a or b` | either |
| `not a` | not `a` |
| `( ... )` | grouping; `and` binds tighter than `or` |

The keywords `and`, `or` and `not` are case-insensitive; quote a word to search for it as text
(`"and"`). A `key:value` with a key the language does not know (`12:30`) is name text. A bare `kind:`,
`type:` or `tag:` is ignored, and a later `kind:` replaces an earlier one, as before the query language.
A property term on an entity without that component or field never matches. Property terms are
evaluated again after each edit on the undo stack.

A query that does not parse lists every row (nothing is filtered) and shows its error under the search
box with the column it starts at. `editor.hierarchy` answers it with `F0002` and `data.query_error`
(the code below) and `data.column` (1-based); `editor.outliner_rows` reports it as
`filter_error: { code, column, message }`.

| Code | Meaning |
|---|---|
| `query_unclosed_paren` | a `(` is never closed (the column is the `(`) |
| `query_unexpected_paren` | a `)` has no opening `(` |
| `query_unclosed_quote` | a `"` is never closed |
| `query_missing_operand` | a term is missing: after `and`, `or` or `not`, at the start, or inside `()` |
| `query_missing_value` | `has:`, `under:`, `parent:`, `name:` or a comparison has no value |
| `query_not_a_number` | `<`, `<=`, `>` or `>=` compares something that is not a number |
| `query_bad_field` | the left side of a comparison is not a `Component.field` path |
| `query_too_deep` | more than 64 nested parentheses or `not`s |

```json
{ "method": "editor.hierarchy", "params": { "filter": "under:Lights and PointLight.intensity>1000" } }
```

## Selection that follows the search

While searching, Select all (the header button or Ctrl+A over the tree), a Shift range (click or Shift
plus an arrow, Home, End, Page Up or Page Down) and the rubber band (drag on empty space beside the
rows; Ctrl adds) select only the rows the query matches, never the ancestors listed to show where a
match is. Edit ▸ Select all in the menu bar still selects every entity.

## Rename in place

F2, or a click on the only selected row (not a double click), turns the row's name into a text field.
Enter (or a click elsewhere) commits it as one undo step, Esc cancels, and Tab commits and moves to the
next row's name. Folders rename the same way.

`editor.outliner_rename { entity, name }` makes the same edit: it answers
`{ entity, name, undoable }`, and refuses a blank name with `outliner_error: "rename_empty"`, an
entity without a level name with `rename_unnamed`, a missing entity with `rename_entity_missing`
(`F0003`), and an edit that cannot be recorded with `rename_failed`.

## Expand and collapse

The header's Expand all and Collapse all buttons (Ctrl+Shift+Plus, Ctrl+Shift+Minus) open or close
every row, folder and category section. Shift+click on a row's arrow, Shift+Right or Shift+Left, and the
row menu's Expand all below and Collapse all below open or close a whole branch.

`editor.outliner_expand { entity?, folder?, all?, recursive?, open? }` does the same: `all: true` for
every row; else `entity` (with `recursive` for its branch) or a `folder` path (with `recursive` for its
subfolders); `open` defaults to true (false collapses). It answers `{ listed, open }`. Without a target it
answers `outliner_error: "expand_no_target"`; an entity or folder that is not listed answers
`expand_entity_missing` or `expand_folder_missing` (`F0003`).

## Folders

A folder holds top-level entities without being an entity: membership is the saved `EditorFolder
{ path }` component (`"Lights/Street"`), which no runtime system reads. The tree draws one row per folder
level, subfolders first. New folder (header button, Ctrl+Shift+N) makes "New Folder" (then
"New Folder 2", ...) holding the selected top-level entities and opens its name for editing. Drop
entities on a folder row to move them in (a child entity leaves its parent first); drop them on the
space under the tree to move them out. A folder row's menu renames it, makes a folder inside it,
moves the selection into it, or deletes it; deleting keeps its entities (they and its subfolders move
up a level). A folder row is a button: Tab reaches it, Enter opens or closes it, F2 renames it and
Delete deletes it.

| Method | Params | Answers |
|---|---|---|
| `editor.folder_create` | `path` | `{ path, folders }` |
| `editor.folder_rename` | `path`, `to` | `{ moved, folders }` (renames or moves the folder with its subfolders and entities) |
| `editor.folder_delete` | `path` | `{ moved, folders }` (its entities move up a level) |
| `editor.folder_move` | `entities` (absent = the selection), `path` (null = out of folders) | `{ moved, folders }` |

Each edit is one undo step. A folder with no member exists only for the session until an entity moves
in. Refusals carry `data.folder_error`: `folder_bad_path` (empty path or blank segment),
`folder_missing` (`F0003`), `folder_exists`, `folder_entity_not_top_level`, `folder_entity_missing`
(`F0003`), `folder_no_entities`, `folder_command_failed`. `editor.outliner_rows` lists folder rows as
`{ folder, label, depth, count, open }`.

## Paste as child and at the original position

Edit ▸ Paste as child (and the row menu's Paste as child) pastes the clipboard under the selected
entity; Edit ▸ Paste at original position puts each pasted root where it was in the world when it was
copied. Both are one undo step and select the pasted roots.

`editor.paste { into?, at_original? }` does either or both: `into` is the new parent (absent = the top
level), `at_original` keeps the copied world positions (default false: the copied local transform
applies under the new parent). It answers `{ pasted, parent, undoable }`; an empty clipboard answers
`outliner_error: "paste_clipboard_empty"`, a missing or internal parent the `hierarchy_*` codes of
[Outliner hierarchy edits](hierarchy.md). `editor.menu_invoke` takes `Edit/Paste as child` and
`Edit/Paste at original position` too.

Duplicates (Ctrl+D, `editor.duplicate`) are named after their original with the next free number:
"Lamp", "Lamp 2", "Lamp 3". A name splits into a stem and a trailing number after a space (a bare
name counts as 1); the copy takes one more than the highest number any entity of the level uses with
that stem. Copies made together take consecutive numbers; their children keep their names.

## Keyboard

With the pointer over the tree: Up, Down, Home, End, Page Up and Page Down move the selection (Shift
extends it); Right opens a closed row, then goes to its first child; Left closes an open row, then goes
to its parent; F2 renames; Enter frames the selection in the viewport; Ctrl+A selects the matching
rows; Ctrl+F focuses the search; Ctrl+Shift+N makes a folder; Delete, Ctrl+D, Ctrl+G, Ctrl+Z and
Ctrl+Shift+Z as before.
# Outliner rows: columns, warnings, tags, kinds, unique names

The Outliner's rows carry more than a name (ADR-7772):

- **Columns.** The **Columns** menu above the tree shows or hides the Type (the entity's kind or
  role), Layer (its map layer, inherited from the nearest ancestor), Prefab source (the prefab an
  instance comes from), Tags (chips), Visible / Locked (the eye and lock buttons) and Revision
  columns, sets each shown column's width, and sorts the rows by name or by creation order. The
  Revision column shows the state of the file each entity comes from (the open level, or the prefab being edited): `modified`, `added`, `untracked` and so on, empty when the file is unchanged or the project has no revision control. The revision-control status poll fills it once per answer; drawing the column starts no git process. The settings are per user and
  per tree instance (`main` today), saved in `.regia/outliner_view.ron`; they never touch the level
  or the undo stack. Each project reads and writes its own file: opening another project reloads
  the settings from that project. A view file that exists but does not read or parse is never
  overwritten: the editor logs why, uses the defaults in memory, and saves again once the file is
  fixed or deleted (`file_error` in the agent results says why it is not saved).
- **Creation order.** Rows sort by entity id (the allocator's generation, then index): a freshly
  opened level's entities in the order the loader spawned them, then the entities made in this
  session in the order they were made. A level file lists its entities by guid and stores no
  creation stamp, so the order of entities made in an earlier session is not kept across a save
  and reload.
- **Row colors.** Row color ▸ in the context menu tints the row with one of the theme's palette
  colors: `orange`, `sky`, `green`, `yellow`, `blue`, `red`, `pink`, `gray`, `brown`, `lime`
  (No color clears it). The color is editor-only view state, keyed by the entity's guid.
- **Configuration warnings.** A ⚠ badge marks an entity whose setup is incomplete; its ancestors
  show a dimmed ⚠. Hover (or a screen reader) gives the text, the code and the fix hint. The
  Problems panel lists the same warnings under *Level configuration*; a click selects the entity.
  The checks run when an entity changes (a component added, removed or edited), never on an idle
  frame. Seed checks and their codes:

  | Code | When |
  |---|---|
  | `rigid_body_without_collider` | A rigid body with no collider on itself or on a child without its own body. |
  | `collider_zero_size` | A collider shape with zero size (or a hull with fewer than 4 points, an empty mesh or heightfield). |
  | `light_zero_intensity` | A light with zero intensity (a directional light: zero illuminance). |
  | `light_zero_range` | A point or spot light with zero range (rectangle and tube lights take 0 as automatic). |
  | `audio_emitter_without_sound` | An audio emitter with no sound and no cue. |
  | `camera_near_not_before_far` | A camera whose near plane is not in front of its far plane. |
  | `mesh_without_mesh` | A mesh source that names no mesh. |
  | `prefab_source_missing` | A prefab instance (or a pending one) whose prefab is not loaded. |

  Rust code registers more checks with `ConfigChecks::register::<T>(name, check)` on the
  `ConfigChecks` resource.
- **Badges.** 📜 a Lua script component, ⮕ a logic graph, 🎞 a sequence player or its bindings,
  ① the unique-name marker. Each badge has a spoken name. The sequence badge is a button: it selects
  the entity and brings the Sequence Curves panel forward (Tab to it, Space or Enter; agents:
  `editor.open_sequence`). The logic-graph and script badges are buttons too (agents:
  `editor.open_badge`): the logic-graph badge shows that entity's graph in the Logic Graph panel (a
  panel with unsaved edits is brought forward, not replaced), the script badge opens the declaration
  of the script component (`regia.component("name", ...)`) at its line in the configured code editor
  (the preference `code.editor_command`, then `$VISUAL` / `$EDITOR`). The script's file is found in the
  folder of the declaring mod under the project's `mods/`; when it cannot be found (the mod is not
  loaded, or is not a folder of the project) the badge's tooltip says why.
- **Tags.** *Add tag to selection…* and *Remove tag* act on every selected entity as one undo step.
- **Change kind.** *Change kind* ▸ swaps a light for another light type (point, spot, rectangle,
  tube, directional), a collider's shape for another shape (box, sphere, capsule, cylinder, cone) or a
  camera's projection, keeping the fields both share (same name and type; a point light's `radius`
  is the other lights' `range`) and the children, as one undo step. Components the new light type
  requires (a directional light brings `InheritedVisibility`) arrive with it and leave with it on
  undo, so undo restores the entity's exact component set.
- **Unique name.** The *Unique name* toggle adds the saved `UniqueName` component. Two marked
  entities never share a name: toggling it on, or renaming a marked entity, to a name another marked
  entity holds is refused (`unique_name_taken`), and a new copy (duplicate, paste) of a marked
  entity gets the next free name (`Door` → `Door 2`) in the same undo step as the copy, so one undo
  removes both. Only the entity that changed is renamed: a level that was saved with two marked
  entities of one name opens with both names as they are, and opening a project or level never
  renames anything. An undo or redo is never followed by a rename. Game code finds the entity
  wherever it sits in the tree with `regia_scene::unique_name::find_unique(world, "Door")`.

Keyboard: the Columns menu is a button (Tab to it, Space or Enter opens it) and every entry inside it
is a checkbox, a width field or a radio button. The row context menu opens with a right click; its
actions are also the agent methods below.

## `editor.outliner_columns`

Mutating, needs `edit_scene`: `set`, `widths` and `sort` write the per-user view file
`.regia/outliner_view.ron` (never the level, never the undo stack). To read without writing, use
`editor.outliner_view`. Params: `set` (an object of column name → shown), `widths` (column name →
points, 24 to 400), `sort` (`name` or `creation`), `tree` (default `main`), `entity` (return that
row's cells and badges).

Result: `{ tree, shown, widths, sort, cells?, badges?, file, file_error }`. Column names: `type`,
`layer`, `prefab_source`, `tags`, `visible_locked`, `revision`. `file_error` is `null`, or why the
view file is not saved (it exists but does not read or parse, and is left as it is).

Errors: `unknown_column`, `unknown_sort`, `no_such_entity`, `F0002` (a wrong parameter type).

```json
{"method": "editor.outliner_columns", "params": {"set": {"tags": true, "layer": true}, "widths": {"tags": 140}, "entity": 4294967301}}
```

## `editor.outliner_view`

Read-only. The columns of a tree instance, their widths and the row sort, and with `entity` that
row's cells and badges. Params: `tree` (default `main`), `entity`. Writes nothing.

Result: `{ tree, shown, widths, sort, cells?, badges?, file, file_error }` (as
`editor.outliner_columns`).

Errors: `view_is_read_only` (the call passed `set`, `widths` or `sort`; use
`editor.outliner_columns`), `no_such_entity`.

```json
{"method": "editor.outliner_view", "params": {"entity": 4294967301}}
```

## `editor.open_sequence`

Mutating (selection and layout), needs `edit_scene`. What the row's sequence badge does: selects the
entity (replacing the selection) and brings the Sequence Curves panel forward on the next frame.
Params: `entity` (an entity with a sequence player or sequence bindings).

Result: `{ entity, selected, panel }` (`panel` is `seq_curves`).

Errors: `no_such_entity`, `no_sequence` (the entity has no sequence badge), `F0002` (no `entity`).

## `editor.open_badge`

Mutating (selection and layout), needs `edit_scene`. What clicking a row's badge does, for any badge
that opens something. It selects the entity (replacing the selection), then: `sequence` brings the
Sequence Curves panel forward (as `editor.open_sequence`); `logic_graph` shows the entity's graph in the
Logic Graph panel (the graph has no file: Save asks for a path; the panel comes forward next frame);
`script` opens the declaration of the entity's script component in the configured code editor. Params:
`entity`, `badge` (`script`, `logic_graph` or `sequence`), `dry_run` (for a script: resolve the file,
line and command without starting the editor).

Result: `{ entity, badge, selected, dry_run, opened, panel?, path?, line?, command? }`. `opened` is
`panel` (with `panel`: `seq_curves` or `logic_graph`) or `code_editor` (with the Lua `path`, the 1-based
`line` and the `command` words, never run through a shell).

Errors: `no_such_entity`, `unknown_badge`, `badge_not_openable` (`unique_name` only marks the row),
`no_script`, `no_logic_graph`, `no_sequence` (the entity lacks that badge), `logic_graph_unsaved` (the
Logic Graph panel holds unsaved edits; it was brought forward and left as it is), `script_path_unknown`
(the declaring mod is not loaded, is not a folder under the project's `mods/`, or no file declares the
component), `no_code_editor` (no `code.editor_command`, project code editor or `$VISUAL` / `$EDITOR`),
`code_editor_failed` (the program did not start), `F0002` (a wrong `entity` or `badge`).

```json
{"method": "editor.open_badge", "params": {"entity": 4294967301, "badge": "script", "dry_run": true}}
```

## `editor.config_warnings`

Read-only. Re-runs the checks of the entities that changed since the last update, then lists the
warnings. Params: `entity` (one entity; absent = every warned entity).

Result: `{ entities: [{ entity, name, warnings: [{ code, text, fix_hint }], descendant_warned }],
warned, evaluated, evaluations, checks }`. `evaluated` is how many entities this call re-checked
(0 when nothing changed); `evaluations` is the running total.

Errors: `F0002` (a wrong `entity`, or an entity that does not exist).

## `editor.tag_selection`

Mutating, one undo step. Params: exactly one of `add` or `remove` (the tag), and `entities`
(absent = the editor selection). Entities that already have (or lack) the tag are left alone.

Result: `{ tag, added, changed, entities: [{ entity, tags }], undo_depth }`.

Errors: `tag_op_required` (neither or both of `add` / `remove`), `empty_tag`, `no_entities`,
`no_such_entity`.

## `editor.change_kind`

Mutating, one undo step. Params: `entity`, `kind` (absent = list the entity's current and compatible
kinds). Kinds: `point_light`, `spot_light`, `rect_light`, `tube_light`, `directional_light`,
`collider_box`, `collider_sphere`, `collider_capsule`, `collider_cylinder`, `collider_cone`,
`camera_perspective`, `camera_orthographic`.

Result: `{ entity, from?, to?, kept?, current, compatible }`; `kept` names the carried fields.

Errors: `no_such_entity`, `unknown_kind`, `kind_not_compatible` (the entity has nothing that kind can
replace), `already_that_kind`, `kind_reflect_failed` (the component type is not registered).

```json
{"method": "editor.change_kind", "params": {"entity": 4294967301, "kind": "spot_light"}}
```

## `editor.outliner_set` (row color and unique name)

`editor.outliner_set` (see [Viewport tools](viewport-tools.md)) also takes `color` (a palette name;
`""` or `null` clears it) and `unique` (add or remove the `UniqueName` marker, one undo step), and
its result adds `color`, `unique`, `badges` and `warnings` (the entity's warning codes).

Errors added: `unknown_color`, `no_guid` (a color needs an entity guid), `unique_name_taken`,
`no_name`, `unique_name_unregistered`.
