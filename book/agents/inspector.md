# Inspector asset fields and component removal (agents)

The Inspector (spec 19 § 2, ADR-7472) draws a component field that carries `@AssetFilter` as an **asset field**:
the asset's thumbnail, a text box that takes a drop from the Content Browser, and a search picker (🔍) listing the
project assets the field takes, each with its thumbnail. Every removable component has its own **Remove**. All of it
is desktop-editor only. Person-facing page: [Inspector](../user-guide/editor-guide/inspector.md).

## Asset fields

Engine fields tagged with `#[reflect(@regia_ecs::attrs::AssetFilter(..))]` (attributes only; the saved shape of
every component is unchanged):

| Component field | Filter | Takes |
|---|---|---|
| `regia_render::MeshSource.mesh` | `mesh` | mesh files (`.gltf`, `.glb`, ...) and `builtin:cube`, `builtin:plane`, `builtin:sphere` |
| `regia_render::MeshSource.material` | `material` | `.material.ron`, `.matgraph.ron`, `.matinst.ron` |
| `regia_audio::AudioEmitter.sound_name` | `audio` | `.ogg`, `.wav`, `.flac`, `.mp3` |
| `regia_matgraph::MaterialGraphRef.path` | `*.matgraph.ron, *.matinst.ron` | material graphs and instances |
| `regia_framework::cloth::ClothSimSource.asset` | `*.cloth.ron` | cloth assets |
| `regia_framework::cloth::SoftSimSource.asset` | `*.soft.ron` | soft-body assets |
| `regia_physics::destruction::DestructibleSource.asset` | `*.destructible.ron, *#fracture/destructible` | destructible assets and a model's fracture manifest sub-asset (`assets/wall.glb#fracture/destructible`); an asset id is typed |
| `regia_procgen::ProcGenerator.graph` | `*.pgraph.ron, *.pgvar.ron` | procedural graphs and variants |
| `regia_media::VideoPlayer.source` | `*.video.ron, *.webm, *.mkv` | video sources and media files |

A filter part is a kind name (`mesh`, `model`, `level`, `prefab`, `material`, `texture`, `image`, `audio`,
`sound`) or a `*.suffix`; parts are separated by `,` or `;`. Game components use the same attribute.

The field's thumbnail comes from the Content Browser's picture cache. Model and material pictures render on the
GPU a few per frame from the Inspector too, so a mesh field shows its picture without the Content Browser grid
open, and the Inspector keeps repainting while a picture it asked for is on its way.

A steady repaint copies nothing: the Inspector keeps its copy of each shown component while the component's change
tick holds (ADR-7326), and keeps the field tools with it (the baseline a field is compared to, which for a prefab
member is a copy of the prefab's component, and the list of modified fields) while the component, the loaded
prefabs and the entity's prefab link are unchanged (ADR-8593). The test
`regia_editor --test panel_frame_clones` counts one steady frame's allocations against the component's size.

Dropping a Content Browser asset on the field writes its path as one undo step, the same `SetProperty` that
`editor.set_property` records, so both give the same component bytes. While a drag hovers the field, it is
outlined in the selection color when the field takes the asset and in the error color when it does not. A refused
drop writes nothing and shows the reason under the field, for example
`assets/props/crate.gltf is not a material (material)`. Picking an entry in the picker writes the path the same way.

### `editor.asset_field_choices`

What an asset field offers and whether it takes a path, without drawing the Inspector. Read only.

| Param | Type | Meaning |
|---|---|---|
| `entity` | number | The entity whose component holds the field (required). |
| `component` | string | Type path or short name, `MeshSource` (required). |
| `field` | string | The field, `material`; a nested struct field as `a.b` (required). |
| `search` | string | Keep the choices whose path contains this, case-insensitive (the picker's search box). |
| `path` | string | Check this asset as a drop would. |

Answer: `{ filter, current, total, choices: [path], verdict: { accepted, reason } | null }`. `choices` holds at
most 500 paths (`total` counts them all); a mesh field lists the built-in primitives first, then the project's
meshes in Content Browser order. Write the field with `editor.set_property`.

```json
{"method": "editor.asset_field_choices", "params": {"entity": 4294967252, "component": "MeshSource", "field": "material", "path": "assets/props/crate.gltf"}}
```

```json
{"filter": "material", "current": "", "total": 2,
 "choices": ["assets/materials/wet.matgraph.ron", "assets/materials/wet_red.matinst.ron"],
 "verdict": {"accepted": false, "reason": "assets/props/crate.gltf is not a material (material)"}}
```

Errors are `invalid_params` with a lowercase `data.reason`:

| Code | When |
|---|---|
| `asset_field_unknown` | No registered component of that name, or the component has no such field. |
| `asset_field_no_filter` | The field exists but has no `@AssetFilter` (it is not an asset field). |

A missing `entity` is `invalid_params`; one that does not exist (an undo may have removed it) is `not_found`.

## Removing a component

Each removable component's header has a context menu item, and its body a button, **Remove** *component*
(spoken name `Remove PointLight`); with several entities selected the **Remove component** menu still offers every
shared component. `Transform`, `Name`, `EntityGuid`, `LevelMember` and `Persistent` cannot be removed there. The
removal is one undo step; undo puts the component back with the same values. Agent path: `editor.remove_component`
(see [Undo and redo in the editor](undo-redo.md)); agent-parity row `inspector.remove_component`.

## Tests

- `regia_editor --lib inspector::ui_tests`: removal from a header's menu and undo, one Remove per removable
  component, the tagged `MeshSource` fields, picker choices for material and mesh fields, a material-graph drop that
  matches `editor.set_property` byte for byte with one undo step, a refused wrong-kind drop with its visible reason,
  and a level with tagged fields that round-trips byte-identically.
- `regia_editor --lib inspector::asset_field_agent`: the method through the editor's method table.

## Search, categories, Modified only, pins, lock and the add-component picker

Spec 19 (addendum § 7.1-7.3, P-109, P-110, P-113, P-116, P-117, P-118). All of it is view state: none of it is on
the undo stack or marks a document dirty, except adding a component.

- **Search.** The box at the top of the Inspector (**Ctrl+F** while the pointer is over the Inspector; **Esc**
  clears it) keeps the fields whose display name or reflect path fuzzy-matches the query, or whose component name
  followed by the field name does, with at least one query letter in the field (`lgtclr` finds a light's `color`;
  `int` finds `intensity` but not the light's other fields). Matched letters are highlighted. Components with a
  match open; the others are hidden. A struct that holds a matching field stays, and a matching struct shows all its
  fields. Agent path: `editor.inspect_selection { search }`.
- **Categories.** Fields with `@Category("…")` draw under a collapsible sub-header inside their component, in the
  order the categories first appear; fields without a category come first.
- **Modified only.** The toggle beside the search box hides every field equal to its baseline: the prefab's value
  for an entity in a prefab instance, otherwise the component type's reflected `Default` (one helper,
  `regia_editor::inspector::defaults`, which the per-field reset marker shares). A component with no changed field is
  hidden. Agent path: `editor.inspect_selection { modified_only: true }` (a field changed on any of the entities
  counts).
- **Pinned fields.** Right-click a field's label ▸ **Pin to top** (keyboard: Shift+F10 on a focused label opens the same
  menu under it, Tab walks its items): the field shows in a **Pinned** section at the top
  of that component, for every entity with the component (▸ **Unpin from top** there or in place removes it). Pins
  are per user, in `inspector_pins.ron` in the user config directory (`format_version: 1`; a file from a newer
  editor is left untouched). Agent path: `editor.inspector_pin`.
- **Lock.** The 🔒 **Lock** toggle (spoken name `Lock the Inspector to this entity`) keeps the Inspector on its
  entity while the selection changes; the lock drops when the entity is despawned. Agent path:
  `editor.inspector_lock`.
- **Pinned inspectors.** The ⧉ button (`Open pinned inspector`) opens a floating Inspector window locked to the
  inspected entity, titled `Inspector: <name>`. Several may be open; each closes when its entity is despawned. The
  windows float inside the editor window. Agent path: `editor.inspector_open_pinned`.
- **Add component.** **Add component…** opens a search box (focused) over every reflected component type, grouped
  by category (a type-level `@Category`, else the type's crate: `regia_render::…` is **Render**), recent picks first.
  Types already on every selected entity, or without a reflected `Default`, stay listed but grayed with the reason.
  Rows go by category, then best match, so each category heading shows once. **Up**/**Down** move a highlight over the
  rows that can be added; **Enter** adds the highlighted row, or the first row that can be added when none is
  highlighted (one undo step); **Esc** closes the picker. An entity with more than six
  components also gets a **Filter components** box over its component list. Agent paths: `editor.component_types`
  for the list, `editor.add_component` to add.

### Methods

```json
{"method": "editor.inspect_selection", "params": {"search": "lgtclr", "modified_only": false}}
{"method": "editor.inspector_pin", "params": {"type": "PointLight", "path": "intensity", "on": true}}
{"method": "editor.inspector_lock", "params": {"on": true, "entity": 4294967296}}
{"method": "editor.inspector_open_pinned", "params": {"entity": 4294967296}}
{"method": "editor.component_types", "params": {"query": "collid"}}
```

- `editor.inspect_selection` gains `search` (string) and `modified_only` (boolean); with either, the answer carries
  `filter: { search, modified_only }`.
- `editor.inspector_pin` answers `{ type, path, pinned, changed, pins: [path] }` (`type` is the full type path).
- `editor.inspector_lock` answers `{ locked, entity }`; without `entity` it locks on the selection's primary.
- `editor.inspector_open_pinned` answers `{ entity, open, opened, windows: [entity] }`; `close: true` closes the
  entity's window.
- `editor.component_types` answers `{ count, types: [{ type_path, name, category, addable, reason?, recent }] }`;
  `entities` (default: the selection) decides which types are grayed.

Errors carry a lowercase `data.reason`:

| Code | When |
|---|---|
| `type_required` | `editor.inspector_pin` without a `type` string. |
| `path_required` | `editor.inspector_pin` without a `path` naming a field. |
| `type_unknown` | No registered component type of that name (`not_found`). |
| `pins_write_failed` | `inspector_pins.ron` could not be written (the pin stays for the session). |
| `pins_file_unreadable` | `inspector_pins.ron` could not be read this session (malformed, written by a newer editor, or an IO error): no pin is changed and the file is left untouched (`internal`). |
| `on_required` | `editor.inspector_lock` without an `on` boolean. |
| `nothing_selected` | `editor.inspector_lock { on: true }` with no `entity` and an empty selection. |
| `entity_required` | `editor.inspector_open_pinned` without an `entity` id. |
| `entity_gone` | The entity does not exist (`not_found`). |

Agent-parity rows: `inspector.lock`, `inspector.open_pinned`, `inspector.modified_only`, `inspector.pin_field`,
`inspector.add_component`.

### Tests

- `regia_editor --lib inspector::panel_tests`: Ctrl+F then `int` and `lgtclr` through keyboard input, Esc clears;
  category sub-headers in first-seen order after the uncategorized fields; Modified only; Pin to top with the pins
  file and the Pinned section on a second entity; the lock and a pinned window following their entity; the picker's
  search, grayed reason and Enter; the component filter.
- `regia_editor --lib inspector::search`, `inspector::defaults`, `inspector::categories`, `inspector::pinned`: the
  matcher, the path sets, the baseline comparison, the grouping and the pins file.
- `regia_editor --test inspector_panel_agent`: every method through the agent inbox.

## Field tools: copy/paste, reset, conditions, toggles, raw view, lists (ADR-7773)

The Inspector's field rows (spec 19 addendum § 7.4-7.6):

- **Field context menu** (right-click a field's label): Copy value, Paste value, Copy as RON, Copy agent path, Reset
  to default, Add to sequence, Pin to top. With the label focused (Tab): Ctrl+C copies the value, Ctrl+Shift+C the
  agent path, Ctrl+V pastes.
- **Reset arrow** (↺) beside a field that differs from its default: the prefab's value on a prefab instance, else the
  type's `Default`.
- **Tooltip** on every field: its doc (`@Tooltip`, else the doc comment), its default value, the condition that
  greys it and its agent path.
- **Components**: the header's menu and the buttons at the top of the body copy, paste values and reset the
  component; **Paste … as new component** shows under the list while the clipboard holds a component the entity
  lacks.
- **Conditions**: a field with `@EditCondition` greys out (or hides with `@EditConditionHides`) while its condition is
  false.
- **Enable checkboxes**: an `Option` field and a `@Toggle` group have a header checkbox; an off group's body is grey.
- **Raw** (header toggle): `@Hidden` fields marked "(hidden)" and structural components, read-only.
- **Lists**: the item count and Clear (asks to confirm) in the header; per item a drag handle (drag onto another
  item; Alt+Up / Alt+Down while it has focus) and Duplicate.

The clipboard keeps the typed value and its RON for the editor session; a paste of another type is refused.

### Attributes

| Attribute | Effect |
|---|---|
| `@EditCondition("expr")` | Greys the field while `expr` over its sibling fields is false. `expr`: clauses joined by `&&`, each `[!]path [op value]`; `path` names a sibling (dots reach inside it); `op` is `== != < <= > >=`; `value` a number, `true`/`false`, an enum variant name or a `"string"`. A clause without `op` is true for a `true` bool, a `Some`, a non-zero number or a non-empty string. A condition that does not parse never greys the field. |
| `@EditConditionHides` | With `@EditCondition`: hide instead of grey. |
| `@Toggle` | On a struct field with an `enabled: bool`: `enabled` becomes the field's header checkbox; the body greys while off. |

Engine uses: `DirectionalLight.static_shadow` (`"cast_shadows"`), `Collider.contact_force_threshold`
(`"active_events.contact_force"`), `PostVolume` fields (`"enabled"`; `blend_distance`: `"enabled && shape !=
Unbound"`).

### Methods

```json
{"method": "editor.property_copy", "params": {"entity": 4294967296, "component": "PointLight", "path": "intensity"}}
{"method": "editor.property_paste", "params": {"entity": 4294967297, "component": "PointLight", "path": "intensity"}}
{"method": "editor.component_copy", "params": {"entity": 4294967296, "component": "PointLight"}}
{"method": "editor.component_paste", "params": {"entity": 4294967298, "as_new": true}}
{"method": "editor.reset_property", "params": {"entity": 4294967296, "component": "PointLight", "path": "radius"}}
{"method": "editor.reset_property", "params": {"entity": 4294967296, "component": "PointLight", "all": true}}
{"method": "editor.list_move", "params": {"entity": 4294967296, "component": "Route", "path": "waypoints", "from": 2, "to": 0}}
{"method": "editor.list_duplicate", "params": {"entity": 4294967296, "component": "Route", "path": "waypoints", "index": 1}}
{"method": "editor.inspect_selection", "params": {"raw": true}}
```

- `editor.property_copy` answers `{ entity, clipboard: { type, ron, component, path }, agent_call }`; `path` `""` or
  absent copies the whole component. `agent_call` is the `editor.set_property` call that writes the copied value.
- `editor.property_paste`, `editor.component_paste`, `editor.reset_property`: one undo step each; they answer
  `{ entity, component, path?, as_new?, applied, undoable }`.
- `editor.list_move`, `editor.list_duplicate` answer `{ entity, component, path, len, undoable }`.
- `editor.inspect_selection`: a field whose `@EditCondition` is false on the primary entity (or the first holder) has
  `enabled: false` and `condition`; with `raw: true`, `@Hidden` fields have `hidden: true` and structural components
  `structural: true`.

Errors carry a lowercase `data.reason`:

| Code | When |
|---|---|
| `component_required` | No `component` string. |
| `component_unknown` | No registered component type of that name (`not_found`). |
| `component_missing` | The entity has no such component (paste values into a component it lacks). |
| `component_present` | `as_new: true` onto an entity that already has the component. |
| `path_not_found` | No field at `path` (`not_found`). |
| `path_required` | `editor.reset_property` without `path` or `all`; a list method without `path`. |
| `path_and_all` | `editor.reset_property` with both `path` and `all: true`. |
| `clipboard_empty` | Nothing has been copied. |
| `type_mismatch` | The clipboard holds another type than the field (or than `component`). |
| `not_a_component` | `editor.component_paste` while the clipboard holds a field value. |
| `not_serializable` | The value has no serialized form, so it cannot be copied. |
| `no_default` | Reset of a type without `#[reflect(Default)]` and no prefab value. |
| `not_a_list` | A list method on a field that is not a list. |
| `index_required` | A list method without `from`/`to`/`index`. |
| `index_out_of_range` | An index past the end of the list. |

Agent-parity rows: `inspector.copy_value`, `inspector.paste_value`, `inspector.copy_ron`, `inspector.copy_agent_path`,
`inspector.reset_field`, `inspector.copy_component`, `inspector.paste_component`, `inspector.paste_new_component`,
`inspector.reset_component`, `inspector.raw_view`, `inspector.toggle_group`, `inspector.list_duplicate`,
`inspector.list_clear`.

### Tests

- `regia_editor --lib inspector::field_tools_tests`: copy, paste and reset from the keyboard with one undo step each
  and a refused type; conditions and the raw view; the `@Toggle` and `Option` checkboxes; list count, Alt+Up,
  Duplicate and Clear.
- `regia_editor --lib inspector::condition`, `inspector::tooltip`, `inspector::clipboard`: the grammar, the tooltip
  lines, the modified-path test.
- `regia_editor --test inspector_field_tools`: every method through the agent inbox.
- `regia_ecs --lib attrs`: the new attributes read back.

## Number expressions, flags, pickers and buttons (ADR-7785)

### Typed numbers

A number field's text box takes an expression as well as a number. It is evaluated when the edit is committed (Enter
or leaving the field), so a relative edit applies once:

| Form | Example | Means |
|---|---|---|
| Arithmetic | `2*(3+4)`, `7 % 4`, `2^10`, `-2^2` | `+ - * / %`, `^` (power, binds tighter than unary minus), parentheses |
| Constants | `2*pi`, `tau/4`, `e` | `pi`, `tau`, `e` |
| Functions | `sqrt(2)`, `deg(pi)`, `rad(90)`, `clamp(x, 0, 1)` | `sqrt abs sin cos tan asin acos atan floor ceil round ln log exp deg rad` (one argument), `min max pow` (two), `clamp` (three); trigonometry is in radians |
| Relative | `+10`, `-=5`, `*2`, `/=4` | a leading `+`, `*`, `/` or `+= -= *= /=` changes the current value; a leading `-` alone is a negative number |
| Units | `5cm`, `2 ft`, `90deg`, `10ms`, `3s`, `(1+2)km` | lengths `mm cm m km in ft yd mi`, angles `deg ° rad turn`, times `ms s sec min h`, converted to the unit the field shows (`@Units`, or the inferred unit: degrees for a radian angle) |

A field without a unit takes the base unit of the dimension (meters, degrees, seconds); a unit of another dimension,
or any unit on a field whose unit does not convert (`m/s`, `dB`), is refused. Whole-number fields round the result.
A refused expression keeps the old value; the field is outlined in the error color for a moment and its hover text
says why.

While dragging a number, Shift drags a tenth as fast, Alt ten times as fast, and Ctrl (Cmd on macOS) snaps to the
field's step (`@Step`, else 1 for whole numbers and 0.1 otherwise). A drag over the field's label scrubs the number
the same way. From the keyboard, a focused number field takes typed text and the Up and Down arrows.

`editor.set_property` takes `expr` (a string) instead of `value` on a number field of one entity, evaluated the same
way against the current value; `after` in the answer is the number written:

```json
{"method": "editor.set_property", "params": {"entity": 4294967252, "component": "Transform", "path": "translation.y", "expr": "+=50cm"}}
```

### Linked vector components

A `Vec2`, `Vec3` (and their `f64` forms) field has a chain toggle (spoken name `Link components (scale together)`,
or `Unlink components` while linked). While linked, editing one axis scales the others by the same ratio (from zero,
they move by the same amount), which makes `Transform.scale` a uniform scale. The toggle is remembered per field
(`Component.field`) in the user preference `inspector.linked_vectors` ([Editor preferences](editor-preferences.md));
agents set it with `editor.prefs_set`.

### Field attributes

| Attribute | On | Draws |
|---|---|---|
| `@Flags("a, b, c")` | an integer field | one checkbox per named bit (bit 0 first; an empty name skips a bit); spoken names `field: name` |
| `@LayerMask("physics")` | an integer field | the bit-layer grid: a toggle cell per layer, named from the project's collision layers (`config/physics.ron`) for `physics`, numbered (`Layer 3`) for any other domain; the scene's `Layers` component draws it too |
| `@FilePicker("*.png;*.jpg")` | a `String` or `PathBuf` field | a browse button whose picker lists the project's matching files (hidden folders and `target` skipped, links not followed) with a search |
| `@DirPicker` | a `String` or `PathBuf` field | the same for the project's folders |
| `@Button("Bake")` | any field (usually `bake: ()`) | a button instead of the value; it runs the component action named after the field |

In the layer grid, pressing a cell flips it and dragging across other cells paints them to the same state; each
cell is also a focusable toggle (Tab, then Space or Enter). Agents write flags and masks as numbers with
`editor.set_property` and the picked path as a string.

### Component actions

Game or tool code registers a component action in the editor (`ComponentActions::register` on the editor world, or
`ToolExtensionContext::register_component_action` from an extension with `editor_panel`): a component, an id, a
label and a function run on one entity. A `@Button` field runs the action whose id is the field's name; the other
actions of a component are a row of buttons above its fields. A run is one undo step that restores the component's
value; a failing action restores the component and shows its message.

#### `editor.component_actions`

`{ entity }` → `{ entity, actions: [{ component, action, label, source, runnable }] }`: the `@Button` fields
(`source: "button"`, `runnable` when an action is registered for the field) and the other registered actions
(`source: "registered"`). Read only.

#### `editor.run_component_action`

`{ entity, component, action }` → `{ entity, component, action, changed, undoable }`. Runs the action as the button
does; `editor.undo` restores the component. Mutating; permission `edit_scene`.

```json
{"method": "editor.run_component_action", "params": {"entity": 4294967252, "component": "Kiln", "action": "bake"}}
```

| Code | When |
|---|---|
| `component_absent` | The entity has no such component. |
| `action_unknown` | The component has no registered action of that id. |
| `action_failed` | The action returned an error (its message); the component was restored. |
| `value_and_expr` | `editor.set_property` got both `value` and `expr`. |
| `expr_with_entities` | `expr` with `entities` (an expression edits one entity). |
| `expr_not_number` | `expr` on a field that is not a number. |
| `expr_empty`, `expr_too_long`, `expr_too_deep`, `expr_syntax`, `expr_unknown_name`, `expr_unknown_unit`, `expr_unit_mismatch`, `expr_arity`, `expr_not_finite` | The expression's own error (in `data.reason`). |

Tests: `regia_editor --lib inspector::expr` (40-input table, error codes, 10 000 random inputs),
`inspector::flags`, `inspector::values_tests` (typed expressions, drag modifiers, label scrub, linked components and
the toggle from the keyboard, flags and grid painting, path pickers, buttons with undo, the three agent routes), and
`regia_ecs --lib attrs`.

## Asset field buttons, shared assets and make unique (ADR-7786)

Every asset field (`@AssetFilter`) has, after its search picker:

- **Browse** (spoken name `Browse to MeshSource.mesh`): shows the asset in the Content Browser (its folder, the asset
  marked selected). Off for an empty field or a built-in primitive.
- **Use selected** (`Use selected asset for MeshSource.material`): puts the Content Browser's selected asset (the
  last asset clicked there) in the field when the field takes it; otherwise it is off and its tooltip gives the
  reason a drop would give. One undo step.
- **Clear** (`Clear MeshSource.material`): empties the field. One undo step.
- A **shared indicator** (a link icon and a count, spoken `MeshSource.material is shared by 2 files`) when more than
  one project file references the asset; the tooltip lists them. The count is taken once per Content Browser index
  generation (a file change, import, move or delete starts a new one), never per frame: the first frame that shows the
  field asks, the next shows the count.
- **Make unique** (`Make MeshSource.material unique`, next to the indicator): copies the asset file to
  `<name>_unique.<extensions>` (then `_unique2`, ...) next to the open level when it is saved under `assets/`, else
  next to the asset, without the `.meta` (the copy gets its own asset id), and points the field at the copy. One undo
  step; undo points the field back and leaves the copy on disk.

Entity reference fields (`Entity`) show the entity, an eyedropper **Pick** (`Eyedropper for Falloff.target`) and
**Clear**. After **Pick**, the next entity picked in the viewport or the Outliner goes into the field as one undo step
and the selection goes back to what it was; Esc (or **Cancel**) disarms it. Agents write an entity field with
`editor.set_property` (entity bits).

### `editor.asset_field_set`

```json
{"method": "editor.asset_field_set",
 "params": {"entity": 4294967301, "component": "MeshSource", "field": "material", "from_selection": true}}
```

Exactly one of `path` (`""` clears) or `from_selection: true` (the Content Browser's selected asset). The asset is
checked against the field's filter as a drop is. Result `{ filter, before, after, changed, undoable }`.

### `editor.make_unique`

`{ entity, component, path }` (the field's reflect path) → `{ from, to, value, undoable }`; `value` keeps the old
value's form (with or without `assets/`).

### `editor.asset_referencer_count`

`{ path }` (`assets/` optional) → `{ path, count, shared, referencers: [path], generation }` (at most 200 listed).

| Code | When |
|---|---|
| `asset_field_bad_params` | Both or neither of `path` and `from_selection`. |
| `asset_field_no_selection` | `from_selection` with nothing selected in the Content Browser. |
| `asset_field_refused` | The field's filter does not take that asset (the message is the drop's reason). |
| `asset_path_invalid` | `asset_referencer_count` was given a built-in or an empty path. |
| `make_unique_not_asset` | The field does not resolve or holds no asset path. |
| `make_unique_builtin` | The field holds a built-in primitive (no file to copy). |
| `make_unique_no_project` | No project is open. |
| `make_unique_missing` | The asset file is not in the project. |
| `make_unique_io` | The copy could not be written. |

`asset_field_unknown` and `asset_field_no_filter` are as for `editor.asset_field_choices`.

## Curve and gradient fields (ADR-7786)

No engine curve or gradient type is reflected yet, so the Inspector recognizes a key list by its shape: a `Vec` of a
reflected struct with an `f32` time field (`t`, `time`, `x` or `location`) and either an `f32` value field (`v`,
`value` or `y`, optionally `f32` `in_tangent` and `out_tangent`) — a **curve** — or a `color` field (`[f32; 3]`,
`[f32; 4]` linear, or a `#rrggbb[aa]` string) — a **gradient**. Such a field draws a mini graph (spoken
`curve curve`, `ramp gradient`); click it, or Enter when it has focus, to open the editor under it:

- curves: the animation curve canvas (drag keys and tangent handles, double-click to add a key, scroll to zoom, so
  ranges beyond 0-1 fit);
- a key table: time, value (or a color picker per stop) and tangents as number fields, **Remove** per key;
- **Add key** (a quarter of the span after the last key) and presets (curves: Constant, Linear up, Linear down, Ease
  in-out, Bell; gradients: White to black, Black to white, Fade out).

Every control is a focusable widget. A key edit is one undo step (a drag coalesces); adding, removing and presets are
their own steps. Keys are kept sorted by time.

### `editor.curve_field_edit`

```json
{"method": "editor.curve_field_edit",
 "params": {"entity": 4294967301, "component": "Falloff", "path": "curve",
            "keys": [{"t": 0.0, "v": 1.0}, {"t": 2.0, "v": 0.0, "in": -0.5, "out": -0.5}]}}
```

Without `keys` or `preset` it reads. Result `{ kind: "curve" | "gradient", keys, presets: [name], undoable }`; curve
keys are `{ t, v, in?, out? }`, gradient stops `{ t, color: [r, g, b, a], hex }`. A stop's `color` may be given as
`"#rrggbb[aa]"` or `[r, g, b(, a)]` linear.

| Code | When |
|---|---|
| `curve_field_unknown` | The component or path does not resolve. |
| `curve_field_not_curve` | The field is not a curve or gradient key list. |
| `curve_field_bad_params` | Both `keys` and `preset`. |
| `curve_preset_unknown` | No preset of that name for this field (the message lists them). |
| `curve_key_invalid` | A key lacks `t` and `v` (or `t` and `color`), or a number is not finite. |

## Color picker (ADR-7786)

A color field's swatch (spoken `tint color picker`) opens the picker under it: modes **RGB** (linear floats, past 1 for
HDR fields), **0-255**, **HSV** and **OKLCH** (perceptual lightness, chroma, hue; a color outside sRGB keeps its hue and
lightness and loses chroma), an alpha channel for RGBA fields, a hex field (`#rgb`, `#rrggbb`, `#rrggbbaa`; Enter
applies), for `@ColorHdr` fields an **Intensity** in EV (the color is `base x 2^EV`), saved **Swatches** with **Save
swatch** (Delete or the context menu removes a focused swatch) and **Recent** colors (an edit that ends — a drag
released, a typed value, a hex, a swatch — joins them). Swatches and recent colors are the user preferences
`color.swatches` and `color.recent` (`editor_prefs.ron`, no new file). Every control is reachable with Tab.

**Eyedropper** (ADR-8106): the drop button beside the swatch (spoken `tint eyedropper`) arms it; then click any pixel
of the editor window, the viewport's rendered frame included, or move the ring with the arrow keys (Shift: ten points
a step) and press Enter or Space. Esc or a right click cancels; pressing the button again (spoken `Cancel the tint
eyedropper`) does too. While armed a transparent layer covers the window, so nothing under the pointer is hovered or
clicked. The pick asks the window's next present for a copy of the frame (the read-back `seeing.screenshot` uses) and
the frame after writes the pixel's color, decoded to the linear value the field stores (the picker's hex then shows the pixel's bytes), keeping the field's
alpha (and a vector's `w`), on every selected entity that has the component, as one undo step; the color joins the
recent colors. It works on `[f32; 3]`, `[f32; 4]`, `Vec3` and `Vec4` color fields.

### `editor.color_eyedropper`

`{ entities?: [id], component?, path?, pixel?: [x, y], cancel?: true }` → `{ armed, pending, field: { entities,
component, path } | null, last: { pixel, color: "#rrggbbaa" | null, error, undoable } | null }`.

With `pixel` (window pixels, top-left origin, as `seeing.screenshot` returns them) it arms the eyedropper for the color
field `path` of `component` on `entities` (absent: the selection) and asks for the pixel at the next present. Call it
again without `pixel` until `pending` is false: that call also finishes a pick whose read-back arrived, and `last`
holds the color written (or `error`, for a pixel outside the window). `cancel` disarms it. A headless editor answers
with its offscreen window capture.

| Code | When |
|---|---|
| `eyedropper_bad_params` | `pixel` is not `[x, y]`, or `component` or `path` is missing, or no entity is given or selected. |
| `eyedropper_not_color` | The field is not a color (`[f32; 3]`, `[f32; 4]`, `Vec3`, `Vec4`) on any of the entities. |

### `editor.color_swatches`

`{ add?: "#rrggbb[aa]" | [r, g, b(, a)], remove?: index, clear?: true }` → `{ swatches: ["#rrggbbaa"], recent: [...] }`.

| Code | When |
|---|---|
| `color_invalid` | `add` is not a hex color or 3-4 finite numbers. |
| `swatch_index_out_of_range` | `remove` names no saved swatch. |

### Tests

- `regia_editor --lib inspector::picker_tests`: Use selected, Clear and Browse to from the keyboard (one undo step
  each); the shared indicator counted once and Make unique; curve Add key and a gradient preset from the keyboard; the
  entity eyedropper and Esc; the five methods through the editor's method table with their refusals.
- `regia_editor --lib inspector::color`: OKLab reference values, OKLCH and HSV round trips, hex parsing, HDR
  intensity, and the picker driven by Tab, typing and Enter.
- `regia_editor --lib inspector::make_unique`, `inspector::picker_links`: copy names and folders, swatch storage.
- `regia_editor --lib inspector::finish_tests`: the eyedropper armed and picked from the keyboard and Esc, a click for
  every selected light as one undo step, alpha kept, `editor.color_eyedropper` and its refusals; the multi-selection
  search, Modified only, reset arrow, linked vectors and `@Button`; the camera's render-layer grid.
- `regia_editor --lib window_capture::tests::the_eyedropper_picks_a_viewport_pixel`: a pick in the middle of the
  real editor window (lavapipe) writes that viewport pixel's linear color as one undo step.

## Inline sub-asset editing (ADR-8107)

An asset field whose asset has a typed document shows an expand arrow before it (spoken `Expand MeshSource.material`,
then `Collapse MeshSource.material`). Expanded, the asset's fields show under the field and edit in place:

- **Which assets**: data assets (`.data.ron`, the value read through the type registry), standard materials
  (`.material.ron`) and physics materials (`.physmat.ron`). Material graphs and instances, meshes, textures, curve
  tables, levels and prefabs keep the picker only: their own editors edit them.
- **The fields** are the asset's JSON view, the same view `editor.edit_asset` patches: objects are groups (the asset's
  own groups start open), short number lists (colors, vectors) are a row of numbers, `format_version` is shown but not
  editable. Every control is a focusable widget named `<asset field> <field path>` (`MeshSource.material
  base.metallic`); Tab reaches it, arrow keys step a number, typing edits text.
- **Where edits go**: to the referenced file, never a hidden copy. Each edit is one undo step on the editor's history
  (a drag is one step; typing in one text field is one step until it loses focus) and is checked first: the whole
  document goes back through the kind's typed reader (materials and physics materials validate their ranges), and a
  refused value changes nothing (the reason shows in the header). The level stays clean; the asset is listed in File ▸
  **Save all** (kind `inline_asset`), and **Save** in the header (`Save MeshSource.material asset`) writes it alone.
  Every field that names the same file shares one document. Autosave keeps the unsaved bytes like any asset editor's
  (a material or physics material reopens from them; a data asset's autosave reopens from the Data Asset panel).
- An edit is refused while the asset's own panel (the Material panel, the Data Asset panel) holds unsaved edits of the
  same file. Expanding a field again re-reads a document that has no unsaved edits, so a change made elsewhere shows.
- **Make unique** (above) copies the file and repoints the field in one undo step; the copy opens as its own inline
  document. Unsaved inline edits stay with the original file's document (Save all lists it). Every asset field is a path
  (`String` with `@AssetFilter`): no field type holds an inline value yet, so make unique always writes a file.

### `editor.inspect_asset_inline`

```json
{"method": "editor.inspect_asset_inline",
 "params": {"entity": 4294967301, "component": "MeshSource", "field": "material"}}
```

Names the asset by `path`, or by `entity` + `component` + `field` (not both). Opens the document (re-read from disk
when it has no unsaved edits). Result `{ path, kind: "data_asset" | "material" | "physics_material", value, dirty,
save_id }`.

### `editor.asset_inline_set`

`{ path, field, value }` sets one field of the document (dotted path in the JSON view; numeric segments index arrays;
`""` replaces the whole value) as one undo step. Result `{ path, field, changed, before, after, dirty, undoable,
save_id }`. The file is written by `editor.save_all { ids: [save_id] }` (or the inline **Save**), not by this call;
`editor.edit_asset` writes a file at once and is the route when no inline document should hold the edit.

| Code | When |
|---|---|
| `asset_inline_bad_params` | `path` and an asset field were both given or neither; `asset_inline_set` without `path`, `field` or `value`. |
| `asset_inline_bad_path` | The path is empty, a built-in, or leaves the project (`..`, a drive, a backslash). |
| `asset_inline_unsupported` | The asset is not a data asset, material or physics material. |
| `asset_inline_no_project` | No project is open. |
| `asset_inline_not_asset` | The asset field does not resolve or holds no asset path. |
| `asset_inline_not_found` | The asset file is not in the project. |
| `asset_inline_parse` | The file does not parse as its kind. |
| `asset_inline_no_field` | The field path names nothing in the JSON view. |
| `asset_inline_bad_value` | The edited document does not fit the type or fails its validation. |
| `asset_inline_panel_unsaved` | The asset's own panel holds unsaved edits of the same file. |
| `asset_inline_io` | The file could not be written. |

Tests (`regia_editor --lib inspector::inline_asset`): the material field expanded and edited from the keyboard (one
undo step, level clean, Save all row, saved by Save all, undo dirty again, inline Save); make unique repointing in one
undo step with undo and redo; both methods through the editor's method table with every refusal, saved by
`editor.save_all`; path and field-path rules.
