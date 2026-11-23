# Content Browser: move/rename (`editor.rename_asset`)

The content browser's **Rename / Move…** action (spec 12 § 9, M11) moves a source file *and* its
`.meta` sidecar, so the asset id never changes. References that store `asset:<guid>` keep working
with no edit; references that store the source path (scene, prefab, level and material files are
all text) are rewritten to the new path before the rename. Deleting a referenced asset stays
refused with the referencers listed; moving one is always allowed and keeps them valid.

The whole move is one undo step: the editor's undo reverts it by moving everything back (the id
still does not change and the path mentions are rewritten back).

## `editor.rename_asset`

Mutating (`Permission::Assets`). Params:

- `from` (required): project-relative path of the existing asset under `assets/`;
- `to` (required): the new project-relative path under `assets/` (must not already exist).
  A directory moves with everything under it.

```json
{ "method": "editor.rename_asset", "params": { "from": "assets/props/barrel.prefab.ron", "to": "assets/models/barrel.prefab.ron" } }
```

Returns `{ from, to, fixed_referencers }` — `fixed_referencers` lists the project-relative paths
whose text was rewritten (guid-only referencers need no edit and are not listed). The move also
lands on the editor's undo stack as one `Move …` entry, so `editor.undo` reverts it.

Errors (JSON-RPC `data.code`, and `data.reason` where one is listed):

| Case | `data.code` | `data.reason` |
|---|---|---|
| `from` does not exist | `F0003` (`not_found`; hint: check the path with `editor.list_assets`) | — |
| `from` or `to` is absolute, climbs out with `..`, or resolves (through a symlink under `assets/`) outside the project; the check runs on the `assets/<p>` path that is actually moved | `F0002` | `path_outside_project` (`data.path_reason` names the rule) |
| `to`, or the place its `.meta` sidecar would go, already exists | `F0002` | `asset_move_destination_exists` |
| an empty segment (`a//b`), `.`, a backslash, or a folder moved into itself (`props` to `props/sub`) | `F0002` | `asset_move_invalid_path` |
| a filesystem step failed | `F0001` (`internal`); the move was rolled back, and the message says so or names what the rollback could not restore | — |

What is rewritten (ADR-5877): only string literals whose whole text, after an optional
`assets/` prefix, is the moved path, or starts with it and `/` (a file under a moved folder).
`/` is part of a path, so renaming `assets/rock.glb` leaves `"props/rock.glb"` alone; field
names and comments are never edited; a string that is only a bare top-level folder name
(`"props"`) is read as a tag, while `"assets/props"` is rewritten. Symlinked folders under
`assets/` are not scanned. The move is all-or-nothing: every refusal comes before the first
change, and a failed write or rename puts back the rewritten files, the moved source and any
folder the move created. A case-only rename (`Rock.png` to `rock.png`) works on case-insensitive
file systems.

The Content Browser panel offers the same operation as the **Rename / Move…** row action, with a
destination field prefilled with the current path.

## Select by asset (`editor.select_by_asset`)

Spec 19 § 3: the Content Browser's **Select in level** row action selects every level entity that
references an asset. What counts as a reference: any reflected string field of any component equal
to the path with or without its `assets/` prefix — a mesh's `MeshSource.mesh`, its `material`
override, an `AudioEmitter.sound_name` / `cue_name` — plus every `PrefabInstance` whose source is
the asset id in the path's `.meta` sidecar (a saved instance names its prefab by `asset:<guid>`,
never by path). Resources and editor-internal entities are left out. An unknown path answers ok
with `count: 0` (it selects nothing); a path nothing references is not an error.

`editor.select_by_asset` (mutating, `Permission::EditScene`). Params:

- `path` (required): the asset's project-relative path (`assets/…` or under it);
- `mode` (optional): `"set"` (default), `"add"` or `"toggle"`, as `editor.select`.

```json
{ "method": "editor.select_by_asset", "params": { "path": "assets/props/a.glb" } }
```

Returns `{ path, count, entities, primary }`. Read what uses an asset from the files side with
`editor.list_assets` + `asset.*` referencer tools; this method reads the open level's entities.

Errors: `F0002` (`invalid_params`) when `path` is missing or empty, or `mode` is not one of the
three.

## New ▸ Material instance (`editor.create_asset`)

Spec 80 § 11.5: the Content Browser's New menu lists **Material instance**; picking a parent graph
creates a `.matinst.ron` with no overrides in the shown folder. The file's bytes are exactly what
`MaterialInstance::to_ron` writes for that parent, and the instance loads back (the material graph
editor opens it read-only over the parent).

Over the protocol, `editor.create_asset` takes kind `"material_instance"`:

```json
{ "method": "editor.create_asset", "params": { "kind": "material_instance", "path": "assets/materials/rock_polish.matinst.ron", "parent": "assets/materials/rock.matgraph.ron" } }
```

- `path` (required): must end with `.matinst.ron`;
- `parent` (required for this kind): the parent graph's project-relative `.matgraph.ron` path; it
  must exist, and the created instance stores it verbatim as its `parent` field.

Errors: `F0002` (`invalid_params`) when the parent is missing or is not a `.matgraph.ron` path, the
path has the wrong extension, the destination exists, or the write fails.

## Open an asset (`editor.open_asset`)

Spec 19 § 2, ADR-7725: one dispatcher opens an asset in its editor panel. A double click in the Content Browser, a
click on an asset link in the Output Log (`editor.log_open_link`) and `editor.open_asset` all go through it, so a
file opens the same way from each and answers the same codes. Each kind opens through its panel's own open method
(`editor.bt_open`, `editor.vfx_open`, …) and the panel's tab comes forward.

| `kind` | Suffixes | Panel | Reports the open path |
|---|---|---|---|
| `level` | `.level.ron` | Viewport (the open level) | `editor.headless_status` → `level` |
| `prefab` | `.prefab.ron`, `.variant.ron` | Viewport (prefab edit mode) | `editor.prefab_mode` → `editing` |
| `material` | `.material.ron` | Material | the panel (no state method) |
| `material_graph` | `.matgraph.ron` | Material Graph | `editor.matgraph_state` → `path` |
| `material_function` | `.matfunc.ron` | Material Graph | `editor.matgraph_state` → `path` |
| `material_instance` | `.matinst.ron` | Material Graph | `editor.matgraph_state` → `path` |
| `anim_graph` | `.animgraph.ron` | Anim Graph | `editor.anim_graph` → `path` |
| `procgen_graph` | `.pgraph.ron`, `.pgvar.ron` | Procedural graph | `editor.procgen_state` → `path` |
| `ability` | `.ability.ron`, `.effect.ron`, `.attrs.ron` | Abilities | `editor.abilities_panel_get` → `asset.path` |
| `behavior_tree` | `.bt.ron` | Behavior Tree | `editor.bt_state` → `path` |
| `state_tree` | `.statetree.ron` | State Tree editor | `editor.statetree_state` → `path` |
| `montage` | `.montage.ron`, `.montagevar.ron` | Montage | `editor.montage_panel_get` → `asset` |
| `sequence` | `.seq.ron` | Sequence Curves | `editor.seq_curve_state` → `path` |
| `shot_list` | `.shots.ron` | Shots | `editor.shot_panel_state` → `path` |
| `ui_layout` | `.ui.ron` | UI Designer | `editor.ui_designer_state` → `path` |
| `ui_animation` | `.uianim.ron` | UI Animation | `editor.ui_anim_list` → `path` |
| `sound_cue` | `.cue.ron` | Cue Editor | `editor.cue_edit` → `path` |
| `data_asset` | `.data.ron` | Data Asset window | the `editor.data_asset_open` answer |
| `data_table` | `.table.ron`, `.table.csv` | Tables (Data tab) | `editor.tables_panel_get` → `data.path` |
| `curve_table` | `.curves.ron`, `.curves.csv` | Tables (Curves tab) | `editor.tables_panel_get` → `curves.path` |
| `chooser` | `.chooser.ron` | Tables (Chooser tab) | `editor.tables_panel_get` → `chooser.path` |
| `conversation` | `.conv.ron` | Conversation | `editor.conversation_graph` → `path` |
| `vfx_system` | `.vfx.ron` | VFX editor | `editor.vfx_state` → `path` |
| `video` | `.webm`, `.mkv`, `.video.ron` | Video | `editor.video_state` → `source` |
| `cloth` | `.cloth.ron` | Cloth | `editor.cloth_panel_state` → `path` |
| `soft_body` | `.soft.ron` | Cloth | `editor.cloth_panel_state` → `path` |
| `motion_database` | `.motiondb.ron` | Motion database editor window | `editor.mm_db_state` → `path` |
| `biome` | `.biome.ron` | Biome | `editor.biome_panel_get` → `path` |
| `logic_graph` | `.logic.ron` | Logic Graph | `editor.logic_state` → `path` |
| `viewer` | meshes (`.gltf`, `.glb`), textures, sounds | Asset Viewer | the panel (no state method) |

An audio patch (`.audiopatch.ron`) has no editor panel: it answers `asset_kind_unknown`.

`editor.open_asset` (mutating, `Permission::EditScene`). Params:

- `path` (required): the asset's project-relative path (`assets/…`; a path without the prefix is read under
  `assets/`).

```json
{ "method": "editor.open_asset", "params": { "path": "assets/ai/guard.bt.ron" } }
```

Returns `{ path, kind, panel, queued, reply, state }`: `reply` is the panel open method's own answer, and `state`
names the method and field that report the open path (`null` for the material editor, the Asset Viewer and data
assets). A material, a viewer kind and a level open on the shell's next frame (`queued: true`, `reply: null`); a
level then waits like a log link: it is refused while a prefab is being edited, play-in-editor runs or the level has
unsaved edits (the shell's status line says which).

Errors (`data.code`):

| Case | `data.code` |
|---|---|
| no editor panel opens the path's kind (also an audio patch) | `asset_kind_unknown` (the hint lists the kinds) |
| the file does not exist | `F0003` |
| the path leaves the project | `F0002` (`data.reason` `path_outside_project`) |
| `path` missing or empty | `F0002` (`invalid_params`) |
| the panel refused the file | that panel's own code (`bt_*`, `vfx_*`, `tables_*`, `data_asset_unsaved`, …) |
| a prefab could not enter prefab edit mode | `asset_open_failed` |

A queued level answers in the shell's status line instead: `asset_open_prefab_editing`, `asset_open_play_running`,
`asset_open_level_unsaved` or `asset_open_failed` are the codes of `asset_open::open_level_guarded`.

`editor.asset_kinds` (read-only) answers `{ open: [{ kind, suffixes, panel, state }], create: [{ kind, label,
extension }] }`: this table, and the New menu's kinds in its order (`book/agents/editor-creators.md`).

## Folder actions (Validate, Reference graph, Size map, Audit)

Spec 32 § 8, ADR-7725: a folder row's context menu in the Content Browser runs the asset audits on what is under the
folder, recursively. The results land in the same panels as a file's actions; nothing outside the folder is listed.

| Menu item | Agent route |
|---|---|
| **Validate folder** | `editor.problems_panel { run: true, folder: "assets/props" }` |
| **Reference graph** | `editor.reference_graph_panel { action: "open", folder: "assets/props" }` (the folder's assets are the roots; the cap is the folder cap, 5,000) |
| **Size map** | `editor.size_map_panel { folder: "assets/props" }` |
| **Audit to CSV** | `editor.asset_audit { source: "registry", folder: "assets/props" }` |

A folder matches the set's paths with or without the `assets/` prefix. A Reference graph or Size map of a folder that
holds no asset of the set answers `asset_folder_empty`.

## Navigation, filters, metadata search and collections (ADR-7759)

The Content Browser's folder pane is a collapsible tree with a **Favorites** and a **Collections** section above it,
and a breadcrumb bar sits above the asset list.

- **Folder tree.** Click a folder to show it; the arrow button (or a double-click) opens and closes it. Keyboard:
  Tab reaches the rows; Up/Down move to the previous or next row, Left closes a folder or moves to its parent, Right
  opens a folder or moves to its first subfolder, Home/End jump to the first or last row. Drop an asset dragged from
  the list or grid on a folder to move it there: the same move as **Rename / Move…** (its `.meta` moves too, path
  references are rewritten, one undo step). Only a key press in the tree moves the keyboard focus to a row; any other
  navigation (an agent's `folder`, a reveal from another panel, a crumb or favorite click) scrolls the row into view
  and leaves the focus where it was.
- **Breadcrumbs.** Each crumb shows that folder; the `›` after a crumb lists the folders inside it. **Back** and
  **Forward** (Alt+Left/Alt+Right while one of the browser's rows has focus, or while the pointer is over the browser
  and no other panel's widget has the keyboard focus; never while a text field has it, where Alt+Left moves the
  cursor by a word; mouse buttons 4/5 while the pointer is over the browser) walk the folder history (64 steps). The star button adds the folder to Favorites or removes it.
- **Favorites.** Click to open; drag a favorite onto another to reorder, or Alt+Up/Alt+Down on a focused favorite.
- **Filter menu.** Chips for kinds (the kinds the project has), tags (`config/asset_tags.ron`) and states:
  *Modified* (an open document with unsaved edits, or a source or `.meta` newer than its cooked file), *Unreferenced*
  (no `.ron` asset of the project names it by path or asset id: the delete check's rule), *Has problems* (a finding
  in the last validator run) and *Review pending* (agent- or generator-made and not reviewed, or the provenance
  policy asks for a review). Chips widen within a group and narrow across groups; they combine with the typed
  search, and a chip row under the toolbar removes one with a click.
- **Search words.** `tag:<key>` or `tag:<key>=<value>`, `is:modified|unreferenced|problem|review`,
  `meta:<key>=<value>` (the `.meta` field contains the value; `meta:<key>` alone: the field is set). `.meta` fields:
  `id`, `importer`, `tool`, `origin`, `generator`, `client`, `model`, `created`, `reviewer` and every provenance
  extension field. A plain word also matches tag and `.meta` values (after every path match). The index reads each
  asset's facts once and again only when its folder changes (a rescan keeps the facts of every folder whose time did
  not change; a change of `config/asset_tags.ron`, `config/provenance.ron` or the cooked cache folder reads them all
  again, and so does the toolbar's Rescan button); `is:unreferenced` reads each `.ron` asset on the same rule
  (`meta_reads`, `reference_scans` in `editor.content_index`). The first search with a plain word reads every
  asset's `.meta` once.
- **Collections.** A static collection is a list of assets (by asset id, so a moved asset stays in it; adding never
  moves a file); drop an asset on it to add it. A dynamic collection is a saved search ("Save the search" when
  creating). Personal collections (and favorites, folder colors, hide-empty) live in the project's user-local
  `.regia/asset_collections.ron` (ignored by version control; an older editor's copy in the user config directory
  moves there once, and one that does not parse is left untouched and named in `errors`), shared ones in `config/asset_collections.ron`
  (`docs/formats/asset-collections-ron.md`). Delete (or the row's menu) on a focused collection asks first (a shared
  one says it is deleted for everyone); the **Delete** button of the question takes the focus, Enter confirms, and
  the assets stay. Every change reads the file again first and applies to what is on disk now, so a teammate's
  collections pulled meanwhile (or a second editor's personal edits) are kept; a file this editor cannot read (a
  newer version, a merge conflict) is listed with its error and never written until it is fixed. "Save the search"
  saves the typed text and the Filter chips.
- **View menu.** *Hide empty folders* and the color of the current folder (a theme token, so a theme change recolors
  it). Showing engine or starter content is not offered: engine-shipped `engine://` assets are not listed by the
  browser yet, and a read-only engine section (mount, preview, no move or delete) needs a design.

### `editor.content_view` (navigation params)

Besides `grid` and `tile`: `folder` (show it; the current one goes on the back history), `back`, `forward` (booleans),
`expand` and `collapse` (folder arrays), `collection` (a name, or `null` for the folder view). The reply adds
`folder`, `collection`, `breadcrumbs`, `can_back`, `can_forward` and `tree { expanded, root_open, rows: [{ path, depth,
has_children, expanded, favorite, color }] }` (first 500 rows). The panel shows the agent's folder on its next frame.

```json
{ "method": "editor.content_view", "params": { "folder": "assets/props/rocks", "expand": ["assets/tex"] } }
```

Errors (`data.view_error`): `view_bad_params`, `view_bad_folder` (not a folder of the project), `view_no_project`,
`view_bad_collection`.

### `editor.content_filter`

Params `kinds`, `tags`, `states` (each list replaces its group), `clear` (boolean), `search` (typed words for the
reply's list). Returns `{ kinds, tags, states, words, kind_choices, tag_choices, visible, paths }` (the last four with
a project open). Errors (`data.filter_error`): `filter_bad_params`, `filter_bad_kind`, `filter_bad_tag`,
`filter_bad_state`, and the search codes `search_bad_type`, `search_bad_state`, `search_empty_filter`.

```json
{ "method": "editor.content_filter", "params": { "kinds": ["mesh"], "states": ["review"] } }
```

`editor.content_index` accepts the same words in `search`; a bad `is:` name answers `index_error: search_bad_state`.

### `editor.content_favorites`, `editor.content_favorites_get`

`editor.content_favorites` changes the personal file (permission `assets`): params `add`, `remove` (folders),
`order` (every favorite, new order), `color` (`{folder, token}`; token `signal`, `warn`, `error`, `strong`, `muted`
or `null`), `hide_empty_folders` (boolean). The change applies to the file as it is on disk now; without params it
reports and writes nothing. `editor.content_favorites_get` (read-only, no params) reports. Both return
`{ favorites, folder_colors, hide_empty_folders, color_tokens }`. Errors (`data.favorites_error`):
`favorites_no_project`, `favorites_bad_params`, `favorites_bad_folder`, `favorites_not_found`, `favorites_bad_color`,
`favorites_file_unreadable` (the personal file does not parse: it is left alone), `favorites_write_failed`.

```json
{ "method": "editor.content_favorites_get", "params": {} }
```

### `editor.collection_list`, `editor.collection_create`, `editor.collection_add`, `editor.collection_remove`, `editor.collection_delete`

- `collection_list { name? }` → `{ collections: [{ name, scope, kind, query, count, paths, missing }], errors }`
  (`missing`: static members whose asset is gone; `errors`: a collections file that could not be read).
- `collection_create { name, scope?: "personal"|"shared", query? }` (a `query` makes it dynamic).
- `collection_add { name, paths }`, `collection_remove { name, paths }` (static collections only; files never move).
- `collection_delete { name }` → `{ deleted, scope }` (the assets stay).

```json
{ "method": "editor.collection_create", "params": { "name": "Needs review", "scope": "shared", "query": "is:review" } }
```

Errors (`data.collection_error`): `collection_no_project`, `collection_bad_params`, `collection_bad_name`,
`collection_exists`, `collection_bad_query`, `collection_not_found`, `collection_is_dynamic`,
`collection_asset_not_found`, `collection_file_unreadable` (the collection's file does not parse, for example a newer
version or a merge conflict: it is not written until fixed), `collection_write_failed` (nothing changed in memory
either).

## OS file drops (`editor.import_files`)

Files dropped from the operating system onto the editor window import through the normal import path
(spec addendum § 8.7, ADR-7788). Over the Content Browser they go into the folder row under the pointer,
else the shown folder; over a viewport pane they import into the shown folder and every mesh, prefab and
sound is placed at the drop point, all placements as ONE undo step. Files from outside the project are
always copied in (never referenced in place); a file already under the project's `assets/` is used where
it is. Files no importer reads are listed in one toast. winit gives no position with a dropped file, so
the target is the pointer's last known position. The panel's import runs as the background job
`import:<n>` (View ▸ Asset Jobs lists it); placement waits for it.

`editor.import_files` (mutating, `Permission::Assets`) does the same synchronously:

- `paths` (required): absolute paths of the dropped files;
- `folder`: destination under `assets/` (default: the browser's shown folder);
- `place`: `true` places the results at the viewport's drop aim (the origin without one), one undo step.

```json
{ "method": "editor.import_files", "params": { "paths": ["/home/me/Downloads/crate.glb", "/home/me/notes.txt"], "folder": "props", "place": true } }
```

Returns `{ imported, placed, unsupported, errors: [{ path, error }], folder }`; `unsupported` holds the file
names no importer reads (`["notes.txt"]`). Errors carry `data.drop_error`:

| Code | When |
|---|---|
| `drop_bad_params` | `paths` is missing, empty or not all strings; `folder` is not a string |
| `drop_bad_folder` | `folder` leaves `assets/` (`..`, absolute, a symlink out) |
| `drop_import_failed` | the import could not start (the message says why) |

No project open answers `F0003` (`not_found`).

## Thumbnails: refresh and capture from the viewport

An asset's context menu ends with **Refresh thumbnail** and **Capture thumbnail from viewport** (spec
addendum § 8.8). Refresh forgets the picture and removes its automatic copy in `.regia/thumbs`, so it is
made again from the source. Capture renders the active viewport pane's current view without overlays,
crops the centered square and stores it as the asset's custom thumbnail in `.regia/thumbs/custom/`
(any asset kind, levels and prefabs included). Being in `custom/` is the marker: pruning never removes
it and a refresh keeps it; `clear_custom` removes it.

- `editor.thumbnail_refresh { paths, clear_custom? }` → `{ refreshed: [{ path, custom }] }` (`custom`: a
  custom thumbnail is still shown).
- `editor.thumbnail_capture { path }` → `{ path, custom: true, file }`; needs a GPU device.

Errors carry `data.thumb_error`: `thumb_no_project`, `thumb_bad_params`, `thumb_bad_path` (not a relative
path under `assets/`), `thumb_missing` (no such file), `thumb_no_device` (no Vulkan adapter),
`thumb_capture_failed` (the render or the write failed), `thumb_io` (a custom thumbnail could not be removed).
`editor.asset_thumbnails` reports a captured picture like any other (its key changes with each capture).

## Animation previews

A `.bvh` motion, or a skinned glTF with clips, loops its first clip as a posed skeleton over its grid tile
while the tile is hovered or has keyboard focus (one tile, at most one file load per frame), and the asset
viewer (double-click, or `editor.view_asset`) shows it with a clip picker and Play/Pause (Space).
`editor.view_asset { path, clip?, time? }` on an animation answers the pose of `clip` (index, default 0) at
`time` seconds (looping) as the image, with `info: { kind: "animation", joints, clips: [{ name, duration }],
clip, time }`. `kind_of` names `.bvh` files `animation`.

## Asset Jobs (`editor.asset_jobs`)

View ▸ Asset Jobs (palette: `window.asset_jobs`) lists the asset processing the editor runs in the
background: imports and OS drops (`import:<n>`), Import settings ▸ Apply (`import_settings:<n>`), cooks
(`cook:<n>`), package export and import (`package:<n>`), and one `thumbnails` row while pictures are queued
or being made. Each row shows its state (`queued`, `running`, `done`, `failed`, `cancelled`), progress and
message; a failed row shows its error and activating it filters the Output Log to it; a running row that
can be cancelled has **Cancel**. Keyboard: click a row (or Tab to it), then Up/Down move, Delete cancels,
Enter shows the error. The window does nothing while closed.

`editor.asset_jobs { open?, cancel? }` → `{ open, jobs: [{ id, kind, label, state, fraction, message, error,
cancellable }], counts: { queued, running, done, failed, active } }`. `counts.active` (queued + running) is
the number the status bar shows (`asset_jobs_panel::job_counts`). Errors carry `data.jobs_error`:
`jobs_bad_params` (`open` not a boolean, `cancel` not a string) and `jobs_not_found` (`cancel` names no
asset job).

## First import of a fresh clone

A new checkout has sources and `.meta` sidecars but no `.regia/`. Opening it cooks everything once; the
test `asset_package::first_import_tests` copies the fixture project `crates/regia_editor/tests/fixtures/first_import`
without `.regia/`, cooks it and checks that the cook reports no missing reference, that every reference in
the cooked registry resolves, that a material's normal map named by id is its registry dependency, that its
color map named by path (resolved through the cook manifest at load time, not a registry dependency,
ADR-3320) names a file that exists and is in the cooked registry, that the committed ids are kept, and that
a second open cooks nothing.

See also [Asset packages](asset-packages.md) for Export with dependencies and Import package.

## Selection, bulk actions, retarget, Save all, details (ADR-7822)

The browser's selection is a list. Click selects one asset; Ctrl (Cmd) click toggles one; Shift click selects the
range from the last plain or Ctrl click, in the order the list or grid shows the assets (Ctrl+Shift adds the range).
A drag that starts on the empty background draws a box and selects every row or tile it touches (Ctrl adds); a click
on the background clears. From the keyboard (pointer over the assets, no text field focused): Up/Down move the cursor
one row (one grid row of tiles in the grid), Left/Right one tile in the grid, Shift extends the range, Ctrl+A selects
everything listed, Escape clears. A right click on an unselected asset selects it first.

With more than one asset selected, a row's menu starts with the bulk actions on all of them: **Validate selection**
(one validator run; the Problems panel shows it), **Re-import selection**, **Save selection** (the open documents among
them that have unsaved edits), **Move to…** (a folder row: references are rewritten, one undo step per asset),
**Delete selection…** (one confirmation listing every file that references any of them) and **Add to collection**.

The delete confirmation of a referenced asset offers **Replace references with…**: an asset picker of the same kind;
Apply points every reference at the chosen asset (paths, and the asset id when both have a `.meta`), then deletes, as
one undo step. In the Reference graph's missing list and on a Problems row with `validate_reference_missing`,
**Retarget…** and **Find similar** open the same picker for the missing path (similar assets, same file name in
another folder, come first and the first is preselected). The picker's filter has the keyboard: Up/Down choose, Enter
applies, Escape cancels.

The hover tooltip of a row or tile shows the path, kind, file and cooked size, triangles (glTF meshes), dimensions
(textures), length (WAV sounds), last modified time, how many files reference it, the revision state (the
revision-control status poll fills `content_actions::AssetRevisionStates` once per answer: `modified`,
`untracked`, ... or `unchanged`; "not tracked" without revision control) and the provenance badge.

File ▸ Save all opens the **Save all** dialog: every document with unsaved edits (the level, the prefab in edit mode,
each asset editor's dirty document, the Material panel and project settings), each with a checkbox. **Save selected**
saves the checked rows (an untitled level is saved as `assets/unsaved*.level.ron`, as the close always did);
**Don't save** and **Cancel** save nothing. The editor's close shows the same dialog (Don't save exits, Cancel stays;
while a listed document cannot be saved from the dialog, such as a material with no file yet, Save selected saves the
rest and keeps the editor open with the dialog naming what is left, so only Don't save drops it),
and so do File ▸ New level, Open level… and Recent over unsaved level edits (they switch after Save selected or Don't
save).
Every asset editor's document saves through its editor's save hook (P-147, ADR-8147): material graphs, UI
designs, procedural graphs, behavior trees, state trees, tables, sound cues, effects, conversations, animation graphs,
UI animations, sequences, the prefab, shot lists, cloth, ragdolls, vehicles, ability assets, the Localization panel's
edit box, camera assets and smart object definitions. A document with no file yet is listed with "untitled: save it
from its editor" (its editor asks for the name). A save the editor refuses (a state tree or animation graph that does
not validate) is a failed row with the editor's reason. An agent's `quit.request` is unchanged.

A Content Browser row or tile whose file is open in an editor with unsaved edits (any row of the Save all dialog)
shows a dot in the theme's unsaved color, and its accessible name ends with ", unsaved changes".
The File ▸ Save all menu item's call is `editor.save_all { dialog: true }` (`editor.menu_list` names it), which opens
the same dialog; `editor.save_all {}` is its Save selected with every row checked. `editor.menu_invoke { path:
"File/Save all" }` saves without the dialog: the level when it has a path, the tile map, and every other document the
dialog can save (`{ level, saved, failed, skipped }`).

### `editor.content_select`

`content_select { paths, mode? }` → `{ selected }`. `mode`: `set` (default, replaces), `add`, `remove`. Paths are
request paths under `assets/` (`path_outside_project` otherwise).

```json
{ "method": "editor.content_select", "params": { "paths": ["assets/props/a.glb", "assets/props/b.glb"] } }
```

### `editor.bulk_asset_action`

With several assets selected, a selected row's menu and the selection bar above the list (Tab reaches its buttons)
also offer **Edit selection in grid** (`editor.bulk_edit`, [Bulk Edit](bulk-edit.md): the selected data assets as
grid rows) and **Export selection with dependencies…** (`editor.export_package { paths }`, [Asset
packages](asset-packages.md): one package of the selection and everything it uses).

`bulk_asset_action { paths, action, to?, collection?, confirm? }` → `{ action, summary, referencers, results: [{ path,
ok, note }] }`. `action`: `validate`, `reimport`, `save`, `move` (needs `to`, a folder), `delete` (the first call
answers `needs_confirmation` with `data.referencers`; repeat with `confirm: true`), `add_to_collection` (needs
`collection`, a static collection).

```json
{ "method": "editor.bulk_asset_action", "params": { "paths": ["assets/a.png", "assets/b.png"], "action": "move", "to": "assets/textures" } }
```

### `editor.retarget_references`

`retarget_references { from, to }` points every reference to `from` (a missing path or any asset) at `to`, an existing
asset of the same kind, as one undo step, and answers `{ from, to, rewritten, deleted: false }`. `retarget_references
{ from, similar: true }` answers `{ from, similar }` (assets with `from`'s file name) and changes nothing.

```json
{ "method": "editor.retarget_references", "params": { "from": "assets/old/rock.png", "to": "assets/rocks/rock.png" } }
```

### `editor.asset_details`

`asset_details { path }` (read-only) → `{ path, kind, file_bytes, cooked_bytes, triangles, dimensions, duration_ms,
modified, referencers, revision }`: what the tooltip shows.

### `editor.save_all`

`save_all { list? }` → `{ documents: [{ id, kind, label, path, can_save, note }] }`; `save_all { dialog: true }` opens
the dialog in the editor; `save_all { ids }` saves those documents; `save_all {}` saves every document the dialog can
save. The save answers `{ saved, failed: [{ id, error }], skipped, queued, documents, shell_results }` (`documents`:
what is still dirty). `save_all { cancel: true }` closes the open dialog the way its **Cancel** button (or Esc) does:
nothing is saved, and the editor's close or a level switch waiting on it does not go ahead; it answers `{ cancelled,
dialog: false }` (`cancelled: false` when no dialog was open). With nothing unsaved, File ▸ Save all's dialog says
"Every document is saved." with a single **Close** button (the same `cancel: true`) instead of the three choices. The shell's own rows, `shell:material` (the Material panel's material) and `shell:settings`
(the Project Settings' `Regia.toml`), are listed while the editor window runs; saving them is `queued` and the
editor's next frame writes them (`shell_results: [{ id, error }]` in the next answer reports how that went).

```json
{ "method": "editor.save_all", "params": { "ids": ["level"] } }
{ "method": "editor.save_all", "params": { "dialog": true } }
{ "method": "editor.save_all", "params": { "cancel": true } }
```

Errors (`data.reason`): `path_outside_project`, `content_select_bad_mode`, `bulk_empty`, `bulk_unknown_action`,
`bulk_missing_param`, `needs_confirmation`, `retarget_same_asset`, `retarget_target_missing`,
`retarget_kind_mismatch`, `retarget_nothing` (no file references `from`), `retarget_write_failed` (rolled back),
`asset_replace_folder`, `asset_missing`, `asset_delete_failed` (the references are put back).

## User asset library

The folder tree's **Library** root is read-only. Select a project asset, expand Library,
and choose **Add to library** to copy it and its dependencies. Entries live under
`<editor config directory>/asset_library/`, outside the project. **Refresh library**
reads entries added by another editor. **Use in project** copies an entry under a fresh
`assets/library/<copy id>/` folder, with new asset and sub-asset IDs and remapped RON
path/ID references. Relative glTF buffer/image paths keep their layout. Project copies
remain independent when the original project or library is removed. Tab reaches the
root and each action; Enter or Space activates it. Controls use the editor's scale and theme.

`editor.asset_library` (MCP `asset_library`) requires `assets` permission, including
list, because the single method also exposes copy operations:

```json
{"action":"list"}
{"action":"add","path":"assets/props/crate.prefab.ron"}
{"action":"use","entry":"<entry id returned by add or list>"}
```

- `action` is required: `list`, `add` or `use`.
- `path` is required for add: a project-relative source under `assets/`.
- `entry` is required for use: an entry ID, never an arbitrary filesystem path.
- List returns `{entries: [{entry, roots, assets}], errors}`; damaged entry manifests
  are reported in `errors` while valid entries remain listed. No project is needed to list.
- Add and Use return `{job}` immediately. Poll `agent.job_status {job}` until `state`
  is `done` or `failed`; copy work runs off the editor and protocol threads. One copy
  runs at a time per editor. These jobs are not cancellable.
- Add's completed job `result` is `{entry, roots, assets}` after publishing a complete package.
- Use's completed job `result` is `{roots, paths, imported}`. `paths` maps original project-relative paths
  to the new copies; `roots` names the copied selection and `imported` includes dependencies.
  Repeating Use creates a separate copy. Imported sources follow the normal project cook workflow.

No library format was added: entries reuse the asset-package v1 manifest. Copying is
for source assets, with the package subsystem's RON and glTF dependency discovery;
it does not import foreign file formats. glTF dependencies outside assets/ (including
external URLs), symlinks and missing buffer/image files are refused.
The library has no editing, deletion or cross-project linking operation.

| Error code | Meaning |
|---|---|
| `editor_not_running` | Connect to the editor; the runtime cannot copy library entries. |
| `library_unconfigured` | The editor has no user config directory. |
| `library_no_project` | Add or Use needs an open project. |
| `library_params` | Supply the action and its required path or entry. |
| `library_path` | Invalid relative path or entry ID, symlink, or a library overlapping the project. |
| `library_package` | Missing, damaged, duplicate or inconsistent packaged content, or a package operation failed. |
| `library_io` | A file could not be read, parsed or written. |
| `library_busy` | A copy is running; poll its job before starting another. |

Typed search and the Filter menu's kind, tag and state chips also narrow the shown
collection. Blank search with no chips shows all members in all folders. A saved
collection's query defines membership independently of the current browser filters.
