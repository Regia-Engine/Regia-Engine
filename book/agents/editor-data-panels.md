# Editor gameplay data panels (agents)

Four editor panels for gameplay data (ADR-2031): **Tables** (curve tables, choosers and, ADR-2132, typed data tables), **Abilities** (an entity's
attributes, effects and grants), **Montage** (the montage timeline) and **Ragdoll** (a character's physics asset).
Each one keeps its model in an edit-world resource; its buttons and its `editor.*` methods call the same functions,
so an agent can do whatever a person can, and read back what the panel shows. All of them are desktop-editor only.

Errors are `invalid_params` (or `not_found` for the `*_not_found` codes) with a lowercase `data.reason`. The codes
of each panel are listed in its section. A panel keeps the last failure on its `error` line until the next call
succeeds.

## Tables (`editor.tables_panel_*`, spec 51 § 6)

Opens a `.curves.ron`, `.curves.csv` or `.chooser.ron` under the project's `assets/` (paths as in `curves.*` and
`chooser.*`). Edits change the open table in memory with the same library calls as `curves.set_key` or
`chooser.set_cell`; `editor.tables_panel_save` writes the file atomically (canonical RON, or CSV for a `.curves.csv`).
Each table keeps 32 undo steps.

| Method | What it does |
|---|---|
| `editor.tables_panel_open { path }` | Open a table; a chooser shows on the Chooser tab, a curve table on the Curves tab. Tables a chooser reaches through `Chooser("...")` results are read too (`chooser.nested`). |
| `editor.tables_panel_get` | The panel: `curves` (curves, keys, selection, issues) and `chooser` (columns, rows with each cell's `state`, the test context, the `outcome`, issues). |
| `editor.tables_panel_curve_edit { op, ... }` | `set_key { name, x, y, interp?, in_tangent?, out_tangent? }`, `move_key { name, from_x, x, y }` (a graph drag), `remove_key { name, x }`, `set_tangent { name, x, side?, slope }` (a tangent handle: `side` is `in`, `out` or `both`, the default; `slope: null` returns to the derived tangent), `set_curve { name, pre?, post?, interp?, rename? }` (the **Add curve**, **Rename** and **Delete curve** buttons: creates the curve with one key at `(0, 0)` when absent, renames, deletes), `delete { name }`, `select { name, x? }`, `import_csv { csv, interp? }` (the paste box: replaces the table). Edits that carry the same `gesture` id (the steps of one drag) are one undo step. |
| `editor.tables_panel_chooser_edit { op, ... }` | `set_cell { row, column, value \| text }` (`value` in `chooser.set_cell`'s JSON forms, `text` as RON, `null`/`"Any"` clears), `set_row { row, enabled?, result?, cells?, note?, before? }` (creates when absent; the grid's result box and the **Add row** button; `result` in `chooser.set_row`'s JSON forms, `{ ron: "..." }` parses RON), `move_row { row, before? }`, `remove_row { row }` (the row's **✕**), `set_column { column: id, disabled? }` or `set_column { column: { id, kind, input? \| output?, disabled? }, before? }` (the header toggle and **✕**, the **Add column** bar), `remove_column { column }`, `set_options { mode?, result?, catch_all?, near_best?, recency? }` (the options bar). The `set_row`, `remove_row`, `set_column`, `remove_column` and `set_options` ops run the very edit functions `chooser.set_row`, `chooser.remove_row`, `chooser.set_column`, `chooser.remove_column` and `chooser.set_options` run, so a change through the panel writes the same file bytes the agent method writes. |
| `editor.tables_panel_set_context { context?, seed?, replace? }` | The chooser's **test context** (`{ input: value \| null }`, the JSON forms of `chooser.evaluate`) and the seed of a weighted draw. |
| `editor.tables_panel_undo { tab? }` | Undo the last edit of the curve table, the chooser or the data table (`tab`: `curves`, `chooser` or `data`; default: the shown tab). |
| `editor.tables_panel_save { tab? }` | Write it back. Answers `{ written }`. |
| `editor.tables_panel_import { path, format?, tab? }` | Replace the open data table or curve table with the rows of a file: `format` `csv`, `json` (an array of row objects keyed by `key`) or, for a data table, `ron` (a `.table.ron`); default from the file's extension; `tab` `curves` or `data` (default: the shown tab). One undo step; the table is then unsaved. A data table keeps its row type and its text columns (the cell rules of `regia table to-ron --into`); an import that would add a schema error is refused with that error's code and changes nothing. A curve table is replaced like the CSV paste box. Answers `{ tab, format, file, rows, imported, undo_depth }`. |
| `editor.tables_panel_export { path, format?, tab? }` | Write the open data table or curve table to a file, atomically: `csv` (`regia table to-csv`), `json` (`regia table to-json`), or for a data table `ron` (`regia table to-ron`, the canonical `.table.ron`). A curve table is written in the layout of its CSV form (a curve per row, an x per column). The table is not marked saved. Answers `{ tab, format, file, rows, imported, undo_depth }`. |

The **Add column** bar offers every column kind; `FloatDistance` shows its parameters there (influence, max
distance, **wrap** with its range, **drop beyond**) and adds `set_column { column: { id, input, kind:
"FloatDistance(influence: ..., max_distance: ..., wrap: ..., drop_beyond: ...)" } }`, the kind spelled as
`chooser.get` lists it. With a catch-all and declared outputs, **Catch-all outputs** shows one RON value box per
output (blank = not set); committing a box sends `set_options { catch_all: { result, outputs } }` with the
catch-all's result kept and that output set or removed, as `chooser.set_options` takes it; a value that does not fit
the output is refused with `tables_panel_bad_value` and changes nothing; the box keeps the text that was typed, so it can be
fixed (ADR-7716; it used to show the stored value again).

After every chooser edit or context change the panel evaluates the table again with
`ChooserTable::evaluate_traced` at full detail, a fresh `Rng::new(seed)` and no recent picks (a "what if" run, as
`chooser.evaluate`). Each cell's `state` is `pass`, `fail`, `score` (with `cost`) or `skipped`, its `detail` the
trace's explanation, and `issue` the validator code that badges it; each row's `state` is `selected`, `candidate`,
`rejected`, `disabled`, `not_evaluated` or `nested_empty`. The grid fills cells green (pass), red (fail) or amber
(score) and marks the selected row blue.

**Import and Export** (the **Exchange** row of the Curves and Data tabs, parity row P-282): a format drop-down (`csv`,
`json`, and `ron` on the Data tab), a file field (relative paths are under `assets/`) with a **…** picker, **Import…**
and **Export…**. RON and CSV convert both ways this way (a `.table.ron` exports as CSV; a CSV imports into the open
`.table.ron`), as `regia table to-csv` and `to-ron --into` do, and JSON is the same table as an array of row objects:
`[{"key": "rifle", "damage": 30, "kind": "Rifle"}, ...]`. A number or `true`/`false` cell is a JSON number or boolean, a
string cell its text, any other RON value its RON text; a column a row does not set is left out. Reading follows the
CSV cell rules: a JSON string that reads as a RON value is a value unless its column is a text column of the open table.
The chooser has no tabular form and refuses both (`tables_panel_not_exchangeable`).

Codes: `tables_panel_no_project`, `tables_panel_bad_path`, `tables_panel_file_not_found`, `tables_panel_no_table`,
`tables_panel_bad_op`, `tables_panel_bad_value`, `tables_panel_row_not_found`, `tables_panel_column_not_found`,
`tables_panel_unknown_input`, `tables_panel_bad_context`, `tables_panel_nothing_to_undo`, `tables_panel_io_failed`,
`tables_panel_bad_format` (an unknown `format`, or none and no known extension), `tables_panel_not_exchangeable` (a
chooser, or a curve table as `ron`), the reader's `table_bad_file` and `table_bad_record` for an import that does not
parse, `bad_value`, `row_not_found` and `column_not_found` (the shared `chooser.*` edit functions' own reasons), and the
library's `curve_*` and `chooser_*` codes for a file that does not load or an edit that would
make the table invalid (the table is unchanged).

Check an edit the way the panel's smoke test does: open a chooser, set a context, read a cell's `state`, edit the
cell, and read it again.

```json
{"method": "editor.tables_panel_open", "params": {"path": "tables/gait.chooser.ron"}}
{"method": "editor.tables_panel_set_context", "params": {"context": {"armed": false, "speed": 5.0}}}
{"method": "editor.tables_panel_chooser_edit", "params": {"op": "set_cell", "row": "walk", "column": "speed", "value": [0.0, 6.0]}}
```

### The Data tab (typed data tables, ADR-2132)

`editor.tables_panel_open` also opens a `.table.ron` or `.table.csv` (the data tables of `tables.*`), on the **Data**
tab: a grid with one row per key and one column per field. The **schema** is the row struct. When the edit world holds
the table (a game plugin called `register_data_table`), every check a load runs applies: field types, unknown and
missing columns, duplicate and empty keys, the game's row rules (`schema: "loaded"`). Otherwise, when the type registry
knows the file's `row_type`, its fields give the columns and their types and a cell naming no field is an error
(`"reflected"`). Without either, the checks are structural: keys, and one RON value per cell (`"structural"`).

Schema errors badge their cell (`state: "error"`, `issue`: the code) or the key, and list under `issues`. An edit that
would add an error is refused with that error's code (`table_bad_cell`, `table_duplicate_key`, `table_missing_column`,
`table_unknown_column`, `table_empty_key`, `table_rule`) and changes nothing; errors the file already had stay badged
until an edit fixes them. Each table keeps 32 undo steps (each step stores the edit's inverse, so a large table is
cheap to undo). `editor.tables_panel_save { tab: "data" }` writes canonical `.table.ron`, or CSV for a `.table.csv`
(comments are not kept).

| Method | What it does |
|---|---|
| `editor.tables_panel_data_edit { op, key, ... }` | `set_cell { key, column, text \| ron }` (`text` as typed in the grid, by the CSV cell rules: plain text in a `String` field, any other text that is not one RON value becomes a string; `ron` stores a RON literal as is), `clear_cell { key, column }` (the field's default applies), `insert_row { key, before?, cells? }` (`cells: { field: text }`), `remove_row { key }`, `rename_row { key, to }`, `move_row { key, before? }` (absent: to the end). |

`editor.tables_panel_get` answers `data: { path, format, row_type, schema, generation, dirty, undo_depth, columns: [{
name, type_path, text, doc, known }], rows: [{ key, line, state, cells: [{ column, ron, text, state, issue }] }],
issues, errors, warnings }`. The grid fills an error cell dark red and a warning cell (a row without a column other rows
have, which its default may fill) amber.

Check a fix the way the Data tab's smoke test does: open the table, find the badged cell, clear it, and read `errors`.

```json
{"method": "editor.tables_panel_open", "params": {"path": "tables/weapons.table.ron"}}
{"method": "editor.tables_panel_data_edit", "params": {"op": "set_cell", "key": "rifle", "column": "damage", "text": "40"}}
{"method": "editor.tables_panel_data_edit", "params": {"op": "clear_cell", "key": "odd", "column": "colour"}}
{"method": "editor.tables_panel_get", "params": {}}
{"method": "editor.tables_panel_save", "params": {"tab": "data"}}
```

## Abilities (`editor.abilities_panel_*`, spec 53 § 6)

The inspector of an entity's `Attributes` (base editable, current with its breakdown), `AppliedEffects` (remove) and
`AbilityBook` (grant, revoke, start). While a play session runs it shows the play world (`source: "play"`), else the
edit world. Without a pinned entity it follows the editor selection (edit) or shows the first entity with
attributes or a book (play); play-world ids are not edit-world ids, so a new session drops the pin.

| Method | What it does |
|---|---|
| `editor.abilities_panel_get` | `source`, `available` (the world runs the ability plugin), `entities` (id order, at most 256: `state` ring and live effect count), the inspected `entity` with `attributes` (`breakdown` lines: base, each modifier's contribution per phase, clamping, current), `effects`, `grants` (`state`: `ready`, `running`, `ending`, `cooling`), loaded `abilities`, loaded `effect_assets` (what `apply_effect` takes), and `last` (what the last operation did). |
| `editor.abilities_panel_select { entity?, overlay? }` | Pin an entity (`null` follows the selection again); switch the overlay group. |
| `editor.abilities_panel_call { op, entity?, ... }` | `set_base { attribute, base }`, `remove_effect { id }`, `grant { ability, level? }`, `revoke { grant }`, `activate { grant }` (play only), `add_attributes { asset }`, `apply_effect { effect, level? }`. Without a play session it runs at once and answers `{ queued: false, answer, panel }`; while playing it is queued for the editor's next frame (`{ queued: true, pending }`) and its result lands in `last`. |

| `editor.abilities_panel_asset { path?, text?, field?, value?, undo?, redo?, save? }` | The asset editor: `path` opens an `.attrs.ron`, `.effect.ron` or `.ability.ron` under `assets/`, `text` replaces its text and validates it at once, `field` + `value` is a typed form edit (below), `undo: true` / `redo: true` step the asset editor's history (below), `save: true` writes it atomically (refused with `abilities_panel_invalid_asset` while there is an error). Answers `{ asset: { path, dirty, ok, text, comments_warning, form, diagnostics }, written, comments_dropped, undo_depth, redo_depth }`; each diagnostic has `code`, `severity`, `message`, `span` and `field`. |

The asset editor validates with the checks of `abilities.validate` (`parse_error` and `unknown_field` with a line
and column, `format_too_new`, the § 4.5 validator codes), against the open world's loaded assets, tags and
run-state types. The effects an ability names as cost or cooldown are read from the project when the world has not
loaded them, so an ability validates in the edit world too.

The asset editor opens in a typed form (spec 53 § 6, ADR-7472): one row per top-level field of the asset, nested
records and lists under it, numbers as drag values, booleans as checkboxes, text and unit enum variants as text
boxes. Each validation message is drawn beside the field it names (the diagnostic's `field`, e.g. `chance must be in
0..=1` beside `chance`); messages about the asset as a whole and parse errors (with a line and column) are drawn
under the form. **Advanced** shows the raw RON text instead (also shown while the text does not parse). The answer's
`form` is the asset's value tree in the format's serde form (`null` while the text does not parse).

A typed edit sets one field: `field` is a path of keys and indices (`chance`, `lifetime.Timed.Const`,
`live_mods[0].op`) and `value` the new value in serde form (a number, a string, a unit variant name, a list or a
record). The asset's text becomes the format's canonical text with that field changed, so a typed edit and a text
edit that make the same change save the same bytes. A refused typed edit leaves the text unchanged and is drawn
beside its field.

```json
{"method": "editor.abilities_panel_asset", "params": {"path": "effects/burn.effect.ron"}}
{"method": "editor.abilities_panel_asset", "params": {"field": "chance", "value": 0.5, "save": true}}
```

`add_attributes` and `apply_effect` are the panel's **Add attributes** and **Apply effect** buttons (spec 53 § 6
parity). For a set the world has loaded they leave the same world state as `abilities.add_attributes` and
`abilities.apply_effect`: `asset` is the attribute set's asset key (a path under `assets/`), `effect` a loaded effect,
`level` defaults to 1. A set the world has not loaded is read from the project's `assets/` folder under the key as
given; `attrs/guard.attrs.ron` and `assets/attrs/guard.attrs.ron` name the same file. `abilities.add_attributes` reads
an unloaded set from the project root instead, so it takes only the `assets/...` spelling, and with that spelling both
give the same world state. Answers: `{ attributes }` (how many the entity now has) and `{ outcome, id?, stacks }`.
In the edit world each is **one undo step**: `editor.undo` puts the entity's attributes, live effects, grants and
counted tags back as they were. Play-world calls are not undoable (the play world is thrown away at Stop).

```json
{"method": "editor.abilities_panel_call", "params": {"op": "add_attributes", "asset": "attrs/guard.attrs.ron"}}
{"method": "editor.abilities_panel_call", "params": {"op": "apply_effect", "effect": "effects/burn.effect.ron", "level": 2}}
```

`format_version` is shown in the form but not edited there: a typed edit of it is refused with
`abilities_panel_read_only` (change the version in Advanced, if ever). A non-finite number written by hand (`NaN`,
`inf`, `-inf`) is shown, not dropped: the form draws its spelling with a number widget beside it, and the answer's
`form` holds `{ "non_finite": "NaN" }` (or `"inf"`, `"-inf"`) in its place. A typed edit of another field keeps the
value as written; a number typed over it replaces it. The first typed edit of a hand-formatted file writes the
canonical text, so RON comments in the file are dropped (edit in Advanced to keep them). Before that edit the form
shows a banner naming the loss ("This file has comments: a typed edit rewrites it in its canonical form and drops
them (edit in Advanced to keep them).") and the answer's `asset.comments_warning` carries the same sentence (`null`
for a file without comments); the answer to the typed edit that dropped them has `comments_dropped: true`, and the
form then says the comments were dropped until the file is saved or opened again. It is a warning, not an error:
the edit is applied.

Lists and enums (w10 forms-parity): a list row has **Add entry** (appends an entry) and each entry **Remove
entry**; an enum value has a variant picker. Each is one typed edit of the whole value, so an agent makes the same
change with `field` set to the list or the enum and `value` the new list or variant (`{ "field": "lifetime",
"value": "UntilRemoved" }`, `{ "field": "lifetime.Timed", "value": { "Stat": { ... } } }`) and gets the same text.
A new entry or variant is default-filled from the format's serde types: fields with a serde default take it
(`Magnitude::Stat`'s `weight` is 1.0), the others their zero value (`0`, `false`, `""`, an empty list, `none`), a
nested enum its first variant; the previous variant's values are dropped. A type with no zero value (a tag cannot be
empty) repeats the list's last entry, and an empty list of such a type is refused with `abilities_panel_bad_value`
(type its first entry in Advanced). A click is applied once per editor frame, so a second egui pass of the frame
does not add a second entry.

**Undo and redo** (w11 forms-undo-autosave). The asset editor keeps its own history of the open file's text: every
typed form commit (a field, an added or removed entry, a variant switch) and every agent `text` is one step, and a
run of keystrokes in the raw text editor is one step (a save, an undo or any other edit ends the run). A refused edit
is no step. **Undo** and **Redo** beside **Save** (and `undo: true`, `redo: true`) put the text back and validate it
again; nothing is written until Save, and `asset.dirty` compares with the saved text, so undoing past a save marks
the asset dirty again. A new edit after an undo drops the redo steps; opening a file starts a new history; at most
128 steps are kept (the oldest go first). In one call the order is `path`, `text`, `field`, `undo`, `redo`, `save`.

```json
{"method": "editor.abilities_panel_asset", "params": {"undo": true}}
{"method": "editor.abilities_panel_asset", "params": {"redo": true, "save": true}}
```

The debug overlay group `abilities` is off by default. On, it draws a ring at each listed entity's feet (green
ready, yellow running, blue cooling) and one tick above it per live effect.

Codes: `abilities_panel_bad_op`, `abilities_panel_bad_value`, `abilities_panel_no_entity`,
`abilities_panel_unavailable` (no ability plugin in that world), `abilities_panel_not_playing` (`activate` in the
edit world), `abilities_panel_no_asset`, `abilities_panel_invalid_asset`, `abilities_panel_no_project`,
`abilities_panel_bad_path`, `abilities_panel_file_not_found`, `abilities_panel_io_failed`, `abilities_panel_parse` (a typed edit while the text does not parse: fix it in Advanced), `abilities_panel_no_field` (the asset has no such field or index), `abilities_panel_read_only` (a typed edit of `format_version`), `abilities_panel_nothing_to_undo`, `abilities_panel_nothing_to_redo`, and the reasons of `abilities.*` (`entity_not_found`, `no_attributes`, `unknown_attribute`,
`attribute_derived`, `attribute_not_finite`, `effect_not_found`, `grant_not_found`, `asset_not_found`, ...).

## Montage (`editor.montage_panel_*`, spec 34 M10)

The timeline of a `.montage.ron` (or a child `.montagevar.ron`): one lane per slot track with its clip segments,
then the section, notify and warp lanes. It reads with `anim.montage_describe` and writes with `anim.montage_edit`,
the same functions, so edits are atomic, validated, move their followers (M8) and share that method's undo stack.
Paths are project-relative.

| Method | What it does |
|---|---|
| `editor.montage_panel_open { asset }` | Open a montage. |
| `editor.montage_panel_get` | `montage` (the describe answer, read again from the file), `layout` (each segment's `at` and `length` on the timeline, `known: false` where the clip length is unknown), `picked`, `preview`, `last_edit`. |
| `editor.montage_panel_edit { edit, dry_run? }` | One `anim.montage_edit` edit (`add_segment`, `move_segment`, `remove_segment`, `set_section`, `remove_section`, `link_sections`, `add_notify`, `remove_notify`, `set_follow`, `set_blend`, `add_warp`, `remove_warp`, `undo`; or a child montage's `set_override` / `remove_override`). The answer adds `report` (`moved`, `unlinked`, `saved`, `undo_depth`). |
| `editor.montage_panel_preview { playhead?, section?, whole?, rig? }` | Move the playhead (clamped to the timeline) and pick what the preview plays: the chain from `section` (`null` = the first), or the `whole` timeline order. `rig` (an entity with a `SkinnedMesh`, `null` clears) is the rig the pose preview samples. |
| `editor.montage_panel_select { kind?, ... }` | Select a `segment { track, segment }`, `section { name }`, `notify { name }` or `warp { index }` (no `kind` clears). |
| `editor.montage_panel_slot_edit { path?, op, force? }` | One `anim.montage_slot_edit` op (`add`, `set`, `rename`, `delete`) on the slot-table editor's table (`path` loads a `.slots.ron` first), as one step of the panel's undo journal (below). The answer is the panel state plus `report` (the slot edit's answer). |

`preview.chain` lists the sections the preview plays: from the start section, each section's link is followed
(`continue` to the next one on the timeline, `stop`, `jump`) until it stops or reaches a section again
(`loops: true`). `preview.active` says what each track plays at the playhead: the segment, the clip and the clip time
(rate and loops applied). A clip dropped from the Content Browser on a track lane is added at the end of that track
under its file stem (`kick.clip.ron` → `kick`).

**Pose preview.** With a rig picked, the panel samples the main track (track 0) at the playhead over the rig's
bind pose, with the clips of the rig's `MontagePlayer` and graph `Animator`, and the viewport draws the posed
skeleton (joint to parent lines). `preview.pose` is `{ sampled, joints, positions }` (world space; `sampled: false`
when the track has nothing at the playhead, so the bind pose shows) or `{ error }` (`montage_panel_missing_clip`
when the rig lacks a clip the montage names, `montage_panel_preview_variant` for a child montage). The other tracks'
slot masks are not applied to the preview.

Clip lengths come from the clip libraries of the montage players and animators in the edit world. A segment that
plays to the end of a clip no one has loaded is drawn one second long with an amber outline, and the description
carries `montage_clip_length_unknown`.

**Authoring controls.** Every control below the lanes builds one `anim.montage_edit` edit (or one
`anim.montage_slot_edit` op) and sends it through the same function an agent calls, so a click writes the same
bytes as the agent method for the same change and is one step of that method's undo stack (the panel's Undo is
`{ op: "undo" }`).

| Control | The agent call it makes |
|---|---|
| Slot field + **Add to `<slot>`** | `add_segment { slot, clip }`; a slot no track plays starts a second (third, ...) track; an empty slot field adds to the first track. |
| **Warp**: target, from, to, translation / vertical / rotation, mode, rotation kind, **turn over** (fraction), anchor (root, bone + name, offset + x y z), **Add warp** | `add_warp { target, start, end, translation, vertical, rotation, mode, rotation_kind, rotation_fraction, anchor_bone? \| anchor_offset? }` (spec 34 M2; `mode` is `similarity` or `path_scale`, `rotation_kind` is `auto`, `match_target` or `face_target`). A bone anchor with no bone name sends nothing. The window `tint` has no control: `add_warp` takes none (ADR-7731). |
| **Blend**: in, out, lead, auto blend-out, **Apply blend** | `set_blend` with only the fields that differ from the file. |
| Selected section: **Jump to** `<section>`, **Continue**, **Stop at end** | `link_sections { from, to }`, `{ from }` (on into the next section), `{ from, stop: true }`. |
| Selected section or notify: track, segment, mode, **Set follow**, **Clear follow** | `set_follow { kind, name, follow: { track, segment, mode } }`; Clear sends `follow: null`. Selecting the item (on the timeline or with `editor.montage_panel_select`) loads the form from its own link, the default when it follows nothing, and an edit or Undo loads it again. |
| **Slot table**: path, **Load slots** | `anim.montage_slots { path }` (a missing file is the default table). |
| Slot row: new name + **Rename slot**, **Delete slot** (with **force delete**); **Add slot** (name, group) | `editor.montage_panel_slot_edit { op: rename \| delete \| add, force }` (the panel's `anim.montage_slot_edit`). A rename rewrites the montages and graphs that name the slot, and the open montage is read again. |
| Slot row: group, joints (comma-separated; none = full body), **with children**, **Set slot** | `editor.montage_panel_slot_edit { op: set { name, group?, joints, include_children } }`, or `set { name, group?, full_body: true }` when the joints box is empty (w10). The row's boxes start from the table's group and mask and are read again after every slot edit. |

**Undo journal** (w11 forms-undo-autosave). The panel keeps one journal of its own edits, newest last (at most 64):
a montage edit is undone by `anim.montage_edit`'s `{ op: "undo" }` as before, and a slot-table edit by putting back
every file it wrote (the table, and for a rename each montage and graph it rewrote, read before the edit; a table
the edit created is removed again). **Undo** (and `editor.montage_panel_edit { edit: { op: "undo" } }`) takes back
the newest journal step, so slot and montage edits undo in the order they were made; its `report` for a slot step
is `{ op: "undo", kind: "slots", restored, saved, journal_depth }`. `journal_depth` and `slots` (the loaded
table, `anim.montage_slots`' answer) are in the panel state. A rename that rewrote the open montage drops that
montage's journal steps, because `anim.montage_slot_edit` drops its agent undo steps. Slot edits made with
`anim.montage_slot_edit` directly are not in the journal.

New montages: Content ▸ New ▸ Montage (`editor.create_asset { kind: "montage" }`) writes a montage named after the
file with one track on the default slot playing a clip named `idle`; replace the clip on the timeline.

A refused edit writes nothing and the panel shows the agent's code (`montage_bad_edit`, `montage_invalid`,
`slot_in_use`, `slot_unknown`, `montage_read_only`, ...). `montage_panel_no_slots` means a slot edit was tried with
no table loaded.

Codes: `montage_panel_no_asset`, `montage_panel_section_not_found`, `montage_panel_bad_value`,
`montage_panel_no_rig`, `montage_panel_missing_clip`, `montage_panel_preview_variant`,
`montage_panel_file_not_found`, `montage_panel_bad_path`, `montage_panel_open_failed` (the describe failed without a
code of its own), `montage_panel_no_slots` (a slot-table edit with no table loaded), `montage_panel_io_failed` (an
undo could not put a file back; the step stays, so Undo can try again), `montage_panel_nothing_to_undo`, and the codes of
`anim.montage_describe` / `anim.montage_edit` (`bad_path`, `montage_unknown_asset`, `montage_invalid`,
`montage_bad_edit`, ...).

## Ragdoll (`editor.ragdoll_panel_*`)

A character's physics asset is its ragdoll profile (`.ragdoll.ron`, `format_version` 4, spec 93 § 3): one body per
simulated joint (a capsule, sphere or box: shape, radius, mass, length) and the swing-twist limits of each joint to its nearest simulated
ancestor. A rig picked in the level (an entity with `SkinnedMesh`) gives the joint names to check against, the
humanoid generator and the viewport gizmo.

| Method | What it does |
|---|---|
| `editor.ragdoll_panel_open { path, new? }` | Open a profile (project-relative), or with `new: true` start an empty one that `save` writes there. |
| `editor.ragdoll_panel_get` | `profile` (`forward`, `total_mass`, `bones` with `limits { flex, bend, side, twist }` in radians and `shape { kind, width?, depth? }`), `issues`, `rig`, `selected`, `dirty`, `undo_depth`, `redo_depth`, `preview`, `gizmo_segments`. |
| `editor.ragdoll_panel_edit { op, ... }` | `set_bone { joint \| index, radius?, mass?, length?, limits?, shape?, width?, depth? }` (`length: null` measures from the skeleton; `limits: null` makes an unlimited ball joint; limit fields left out stay; `shape`: `capsule`, `sphere` or `box`), `add_bone { joint, radius?, mass?, length?, limits?, shape?, width?, depth? }`, `remove_bone`, `set_forward { forward: [x, y, z] }`, `humanoid { mass?, limited? }` (from the rig, `RagdollProfile::humanoid_limited`), `select`, `undo`, `redo` (an edit clears the redo steps; in the editor each edit is an entry of the [shared undo history](undo.md), which `undo` and `redo` step). |
| `editor.ragdoll_panel_preview { steps?, drop_height?, push? }` | Drop test (ADR-2133): simulate the open profile on the rig's skeleton falling limp onto a floor, in a private physics world. The level is not touched. Answers the state with `preview`. |
| `editor.ragdoll_panel_set_rig { entity?, show_gizmo? }` | Pick the rig (`null` clears) and switch the gizmo. |
| `editor.ragdoll_panel_save` | Write the profile atomically in the current format. Answers `{ written }`. |

The file's own checks refuse an edit (a radius or mass that is not positive, an empty joint name:
`ragdoll_bad_bone`), as does a limit range with its minimum above its maximum (`ragdoll_bad_limits`). Findings that do
not refuse an edit are `issues`: `ragdoll_unknown_joint` (the rig's skeleton has no such joint),
`ragdoll_duplicate_joint`, `ragdoll_limit_range` (a limit past ±π, warning), `ragdoll_empty` (warning).

### Drop test

`editor.ragdoll_panel_preview` (the panel's `Drop test` button) builds the ragdoll from the open profile on the rig's
skeleton in a private world with a floor. The floor's top is at y = 0. The rig is placed so the lowest joint's
largest shape extent sits `drop_height` above that floor (default 0.3 m): the joint centre is at
`drop_height` plus that extent, after the skeleton's lowest bind-pose Y is subtracted. It then lets the ragdoll go
limp with an optional `push` (N·s on the root body) and runs `steps` fixed steps at 62.5 per second
(1..=4000, default 250):

```json
editor.ragdoll_panel_preview { "steps": 400, "push": [0, 0, 30] }
→ { ..., "preview": { "stale": false, "steps": 400, "settled": true, "settled_at": 212, "face_up": true,
    "max_speed": 3.1, "lowest": 0.08, "finite": true, "digest": "4c0d…",
    "bodies": [ { "joint": "hips", "parent": null, "position": [0.4, 0.12, 0.9] }, ... ] }, ... }
```

`settled` is the ragdoll's own rest test (spec 93 § 2.3). `lowest` is the lowest body centre (below 0 a body sank
through the floor), `max_speed` the fastest any body moved, and `finite: false` means the simulation blew up (look
for tiny masses or radii). The same profile, skeleton and options give the same `digest`. The final pose is drawn
beside the rig in green until the profile changes; then `stale` is true. Codes: `ragdoll_panel_no_rig`,
`ragdoll_panel_no_profile`, `ragdoll_panel_bad_value`, `ragdoll_preview_bad_options` (steps, height or push out of
range), `ragdoll_preview_no_bodies` (no bone of the profile names a joint of the rig).

### Body shapes

Each body is a capsule (the default), a sphere or a box (ADR-2133 § 6). Every shape sits along the bone from its
joint, `length` long: a capsule of the bone's `radius`; a sphere of that radius centred halfway along; a box whose
`width` runs side to side and `depth` front to back (along the profile's `forward`). A leaf bone with no `length` is
four extents long (a sphere: one diameter, touching its joint). Switching a body to `box` starts it at the bone's
diameter on both sides:

```json
editor.ragdoll_panel_edit { "op": "set_bone", "joint": "chest", "shape": "box" }
editor.ragdoll_panel_edit { "op": "set_bone", "joint": "chest", "width": 0.34, "depth": 0.22 }
editor.ragdoll_panel_edit { "op": "set_bone", "joint": "head", "shape": "sphere" }
→ { ..., "profile": { "bones": [ ..., { "joint": "chest", "shape": { "kind": "box", "width": 0.34, "depth": 0.22 }, ... },
    { "joint": "head", "shape": { "kind": "sphere" }, ... } ] }, ... }
```

A box extent that is not positive is `ragdoll_bad_bone`; an unknown shape, or `width`/`depth` on a body that is not a
box, is `ragdoll_panel_bad_value`. Shape edits undo and redo like any other. `save` writes `format_version: 4`;
version 1 files open with every body a capsule (`regia upgrade-project` migrates them).

The gizmo draws, in the rig's bind pose, each body's axis from its joint towards its first child joint (or its
`length`) and its shape: a capsule's ring at each end, a sphere's three rings, a box's 12 edges; and for a limited
joint the two ends of its bend range.

Codes: `ragdoll_panel_no_project`, `ragdoll_panel_bad_path`, `ragdoll_panel_file_not_found`,
`ragdoll_panel_no_profile`, `ragdoll_panel_bad_op`, `ragdoll_panel_bad_value`, `ragdoll_panel_bone_not_found`,
`ragdoll_panel_duplicate_joint`, `ragdoll_panel_no_rig`, `ragdoll_panel_nothing_to_undo`,
`ragdoll_panel_nothing_to_redo`, `ragdoll_panel_io_failed`, `ragdoll_preview_bad_options`, `ragdoll_preview_no_bodies`,
and the file codes `ragdoll_format_version`, `ragdoll_parse`, `ragdoll_bad_bone`, `ragdoll_bad_active`, plus the panel's
own `ragdoll_bad_limits`.

## Not yet

- The montage pose preview samples the main track only (other tracks' slot masks are not applied).
- Curve colors kept in the editor layout file (colors follow the curve's index).
- Golden images of the four panels: the smoke tests are headless (they check the drawn fills, not pixels).
