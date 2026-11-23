# UI animation editor (agents)

ADR-3092. Code: `regia_editor::{ui_anim_doc, ui_anim_agent, ui_anim_panel}`, `regia_ui::anim_pack`,
`regia_asset_pipeline::{data_cook, ui_anim_validate}`. Format: `.uianim.ron` v1 (unchanged,
[UI timeline animations](ui-timeline.md)).

The **UI Animation** panel (and these methods) edit a `.uianim.ron` library next to the
[UI designer](ui-designer.md): list, create, rename and delete animations; tracks and keys (time, value, ease); `once`,
`loop` and `ping_pong` with `loops`; `speed`. Every edit is one undo step and is checked with the format's own
validation first, so the open library is always one the runtime loads. A save writes a temp file and renames it, after
parsing the text back with `UiAnimLibrary::from_ron`.

## Dope sheet

Above the curve view, the **dope sheet** shows every track of the selected animation as a row of key diamonds on one
time axis (the playhead is a line across it). Click a key to select it, Shift-click to add or remove one, and drag
across empty space to box-select keys on several tracks (Shift adds to the selection). Drag a selected key to move
every selected key in time together (10 ms steps); the move is **one undo step** (`editor.ui_anim_edit` op
`move_keys`). From the keyboard the list under the sheet names each key (`Dope sheet: <property> key at <t> s`,
`, selected` when selected): Up / Down move the focus, Enter or Space selects or deselects the key, and Left / Right
(Shift: 0.1 s) move the selection, or the focused key when nothing is selected, one undo step per press. A key moved
onto another key's time on the same track replaces it.

## Event track

Under the dope sheet, the **event track** lists the animation's frame events (`.uianim.ron` v2, ADR-8900); they
also show as flags on the sheet's last row (`events`). **Add event** puts one named `event` at the playhead and
selects it; each row (`Event <i>: <name> at <t> s`, plus the payload when there is one) selects an event for its
fields: time (0 to the animation's length), name and payload (committed when the field loses focus), and **Remove
event**. Each commit is one undo step (`editor.ui_anim_edit` ops `set_event` and `remove_event`). At run time each
event fires `UiAnimEventFired` exactly once per crossing ([UI timeline animations](ui-timeline.md)); the scrub here
samples values and fires nothing.

## Binding an animation to a node

The format has no node field on a track, so the binding is the animation's **name**: `"hud_title::pop"` animates the
node named `hud_title` in the designer's document; a name without `::` is generic and plays on any node. The designer
panel's *Animations* button opens the panel; creating an animation while a node is picked prefixes its name.

## Scrub and preview

`editor.ui_anim_scrub` compiles the animation (`UiAnimDef::compile`) and samples the plan (`TweenPlan::sample`): the same
numbers `ui.play_anim {action: sample}` returns. It changes neither the library nor the designer's document. With
`preview: true` the target node of the open `.ui.ron` is laid out on a **copy** with the sampled `opacity`, `background`,
`border_color`, `width`, `height` and `inset_*`; `translate_x`, `translate_y`, `scale` and `text_color` are runtime-only
and listed under `not_in_layout`.

## Validation (spec 32 codes)

`editor.ui_anim_validate` (and the panel's Problems list) check the library against the nodes of the designer's open
`.ui.ron`; without one only `ui_anim_invalid` can appear.

| Code | Severity | Meaning |
|---|---|---|
| `ui_anim_missing_node` | error | the animation's `node::` target is not in the document |
| `ui_anim_ambiguous_node` | warning | several nodes carry that name; the first is previewed |
| `ui_anim_property_unsupported` | error | a track animates a property the node cannot show (`text_color` on a node without text) |
| `ui_anim_invalid` | error | the animation fails the format's own validation |

The registry validators of spec 32 never read source (§ 4.6), so this check is a function,
`regia_asset_pipeline::ui_anim_validate::validate_ui_anim`, returning the same `Finding` rows.

## Cook, pack and run time

`.uianim.ron` is a cooked data asset (importer `uianim`, registry tag `kind = ui_anim`): the cook validates the library
with `UiAnimLibrary::from_ron` (a malformed one fails the cook with that error) and stores its canonical text under the
type path `regia_ui::anim_timeline::UiAnimLibrary`. No `.ui.ron` field names an animation library, so a game references one
by path: `regia_ui::anim_pack::load_uianim_file(world, "ui/hud.uianim.ron")` reads the source file when there is one,
else the pak entry, and merges the animations into the `UiAnimLibrary` resource. `ui.play_anim {action: play, name,
entity}` then plays them.

## Methods

- `editor.ui_anim_open { path?, new? }`: open a library or start one. Mutating; permission `assets`.
- `editor.ui_anim_list {}`: animations with tracks and keys, undo state, findings. Read.
- `editor.ui_anim_edit { op, ... }`: `add_animation { name, property? }`, `rename_animation { animation?, to }`,
  `remove_animation`, `set_playback { mode?, loops?, speed? }`, `add_track { property }`, `remove_track { property }`,
  `move_keys { animation?, keys: [{ property, time }], by }` (the dope sheet's move: the keys of several tracks
  move `by` seconds in one undo step, each new time snapped to 10 ms and at least 0; a key listed twice is
  `ui_anim_bad_value`, a missing one `ui_anim_unknown_key`, nothing changed), `set_event { animation?, index?, time,
  event, payload? }` (the event track: replace event `index`, or add one when `index` is absent; an event past the
  animation's length or with an empty name is `ui_anim_invalid`), `remove_event { animation?, index }` (a missing
  index is `ui_anim_unknown_event`), `select { animation }`, `undo`, `redo`. Mutating. `editor.ui_anim_list` lists
  each animation's `events: [{ time, name, payload }]`.
- `editor.ui_anim_set_key { animation?, property, time, value, ease? }`: replace or insert the key at `time`. Mutating.
- `editor.ui_anim_remove_key { animation?, property, time }`. Mutating.
- `editor.ui_anim_scrub { animation?, time, reverse?, speed?, preview?, node? }`: `{ values, duration, preview? }`. Read.
- `editor.ui_anim_validate {}`: `{ ok, errors, findings }`. Read.
- `editor.ui_anim_save { path? }`: atomic write. Mutating. Paths follow the document rule of
  [Request paths](safety.md#request-paths) (ADR-7388); a save path (and a `new` library's `path`) must end in
  `.uianim.ron` (`ui_anim_bad_value`).

```json
{"method": "editor.ui_anim_edit", "params": {"op": "add_animation", "name": "title::pop"}}
{"method": "editor.ui_anim_set_key", "params": {"property": "opacity", "time": 0.5, "value": 1.0, "ease": "quad_out"}}
{"method": "editor.ui_anim_scrub", "params": {"time": 0.25, "preview": true}}
```

## Errors

| Code | Meaning |
|---|---|
| `ui_anim_no_document` | no library is open (`editor.ui_anim_open` first) |
| `ui_anim_unknown_animation`, `ui_anim_unknown_track`, `ui_anim_unknown_key`, `ui_anim_unknown_event` | the name, track, key time or event index does not exist |
| `ui_anim_duplicate_animation` | the name is taken |
| `ui_anim_bad_name` | a name is 1 to 128 characters without control characters |
| `ui_anim_bad_value` | a parameter has the wrong type or range (`time`, `ease`, `mode`) |
| `ui_anim_invalid` | the edit would break the format's rules (the `AnimError` text says which); nothing changed |
| `ui_anim_parse` | the file is malformed or newer than this build reads (the `AnimError` text) |
| `ui_anim_io` | the file could not be read or written |
| `ui_anim_nothing_to_undo`, `ui_anim_nothing_to_redo` | the stack is empty |
| `ui_anim_unknown_node` | `scrub` with `preview` named a node the document does not have |
