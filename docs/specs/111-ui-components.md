# 111 — UI components: reusable `.ui.ron` documents

Status: proposed with ADR-3147 (T2.13 UI designer, sprint 2026-10-21). Crates: `regia_ui`
(`authoring.rs` format hunks, `ui_component.rs` expander, the `binding.rs` prefix hook),
`regia_asset_pipeline` (`ui_refs.rs`, the reference extraction the dependency graph reads),
`regia_editor` (`ui_designer*.rs`), `regia_agent` (`ui_components.rs`, the `ui.components` read
method). Agent page: `book/agents/ui-components.md`. Format page: `docs/formats/ui-ron.md`. No other
engine's documents or code were used.

## 1. Purpose

Before this spec a `.ui.ron` document had no reuse: a health bar on three screens was three copies of
the same subtree, and `repeat` templates (spec 81 § 2.7) were inline only. A **component** is an
ordinary `.ui.ron` document that other documents instantiate by path. Editing the component edits
every instance.

## 2. Format (`.ui.ron` v8)

v8 adds one optional top-level field and five optional node fields. A v7 file means the same in v8,
so the built-in v7 → v8 step only renumbers (`docs/formats/ui-ron.md`).

### 2.1 Component declaration (top level)

```ron
(
    version: 8,
    component: (
        params: [
            (name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)]),
            (name: "fill",  type: color,  default: "#30C060", targets: [(node: "Fill", property: background)]),
            (name: "value", type: number, default: 1.0, targets: [(node: "Fill", property: progress)]),
        ],
        slots: ["badge"],
    ),
    root: ( name: "HealthBar", children: [ /* … */ ( name: "Badge", slot: "badge" ) ] ),
)
```

- `params`: name, `type` (`string`, `number`, `bool`, `color`), `default` and `targets`. A target names
  a node of this document by `name` (`""` is the root) and one property. The properties are the
  overridable surface; an instance cannot change anything else.

  | property | type | effect on the target node |
  |---|---|---|
  | `text` | string | the node's text becomes one literal span (keeps align, wrap, style) |
  | `command` | string | `command` |
  | `class` | string | appended to `classes` |
  | `background` | color | `background` (solid) |
  | `border_color` | color | the border colour (a 1 px border when the node had none) |
  | `tint` | color | the image tint (no image: no effect, reported by the declaration check) |
  | `opacity` | number | `opacity` |
  | `width`, `height` | number | `style.size` in px |
  | `progress` | number | the progress widget value (clamped to 0..1) |
  | `visible` | bool | `display: none` when false |
  | `disabled` | bool | `disabled` |

- `slots`: outlet names. A node with `slot: "<name>"` is that outlet; instance content for the slot
  replaces the outlet's own children (which are the default content). `default` is the slot plain
  instance `children` fill.

The declaration is checked when the component is loaded (`ui_component_invalid`): every target node
exists, every type fits its property, every default fits its type, names are unique, each slot has
exactly one outlet.

### 2.2 Instance (node fields)

```ron
( use: "ui/health_bar.ui.ron", name: "P1Health", bind_root: "Party.p1",
  params: { "label": "P1", "fill": "#D04040" },
  slots: { "badge": [ ( text: (spans: ["!"]) ) ] } )
```

- `use`: the component's path under `assets/` (`/`-separated, no `..`).
- `params`: values for declared parameters only.
- `bind_root`: the view-model path the component's `$`-relative sources resolve against (§ 3).
- `slots`: content per declared slot; plain `children` fill the `default` slot.
- An instance node may also set `name` (replaces the component root's name), `classes` (appended) and
  `style` (replaces the component root's style when set: the instance's placement). Any other field on
  an instance node is an override of an undeclared property: `ui_component_override`.

## 3. Bindings inside a component

A binding source (`bind`, `bindings`, `repeat.source`, and a nested instance's `bind_root`) that starts
with `$` is relative to the instance's `bind_root`: with `bind_root: "Party.p1"`, `$.hp` is
`Party.p1.hp` and `$` is `Party.p1`. A `bind_root` that starts with `.` is item-relative, so an
instance inside a `repeat` template binds to its row: `bind_root: "."` makes `$.hp` the item's `.hp`.
Substitution happens once at expansion (`regia_ui::binding::prefixed_source`, the prefix hook), so the
runtime binding pass is unchanged and costs the same as the inline form. An instance without a
`bind_root` whose component uses `$` sources gets `ui_component_no_bind_root` (a warning; the binding
stays unresolved and shows its usual binding status).

## 4. Loading and expansion

`regia_ui::ui_component::expand` turns a document into the plain tree the spawner builds:

1. Nested instances expand depth first. At most `MAX_COMPONENT_DEPTH` (8) components deep:
   deeper is `ui_component_depth`. A path already on the expansion stack is `ui_component_cycle`
   (the message lists the chain). At most `MAX_COMPONENT_INSTANCES` (4,096) instances expand in one
   document, nesting included, so components that each instantiate several others cannot grow a
   tree exponentially: the rest are `ui_component_too_large`.
2. Slot content is expanded in the *outer* document's context (its `$` means the outer root).
3. Every failure is a diagnostic (code, document, node, message) and the failing instance is replaced
   by a visible placeholder node (`name: "ui_component_error"`, class `ui-component-error`, red fill,
   the code and path as text): never a panic, never a missing subtree.

| code | severity | cause |
|---|---|---|
| `ui_component_missing` | error | the `use` path does not exist or is not readable |
| `ui_component_invalid` | error | the component does not parse, or its declaration fails § 2.1 |
| `ui_component_cycle` | error | the document instantiates itself through the chain |
| `ui_component_depth` | error | more than 8 nested components |
| `ui_component_unknown_param` | error | `params` names an undeclared parameter |
| `ui_component_param_type` | error | a value does not fit its declared type |
| `ui_component_unknown_slot` | error | `slots` names an undeclared slot (or plain children, no `default`) |
| `ui_component_override` | error | an instance sets a field the component did not declare |
| `ui_component_bad_path` | error | `use` is absolute, climbs out with `..`, or is not a `.ui.ron` |
| `ui_component_no_bind_root` | warning | `$` sources without a `bind_root` |
| `ui_component_too_large` | error | more than `MAX_COMPONENT_INSTANCES` (4,096) instances expand in one document |

The `UiLibrary` keeps both forms: `templates` (expanded, what `spawn_ui` and `repeat` use) and the
source documents, plus which components each template uses and its diagnostics.

### 4.1 Hot reload

A changed `.ui.ron` re-expands every library template that uses it, directly or through other
components, and respawns their roots. Before the respawn the focused node and every scroll offset are
recorded by **key**: the node's path of names from the root (`HUD/P1Health/Fill`; an unnamed node is
`#<index>`). After the respawn a node with the same key gets the focus and scroll offset back; keys
that no longer exist are dropped. Rows of a `repeat` spawn on the next repeat pass, so state inside a
row is not kept (spec 81 § 2.7 keeps row state across data changes, not across a template edit).

## 5. Cook and packs

`regia_asset_pipeline::ui_refs::ui_layout_references` lists a layout's `use` paths (and mask assets);
`AssetSet::with_ui_layouts` adds every `.ui.ron` under `assets/` with those references, so the
dependency graph, the missing-reference and orphan validators and the size map see a component as a
dependency of every layout that uses it. A named build profile's closure already follows quoted paths
in raw files, so a pack that ships a layout ships its components.

## 6. Designer (spec 81 § 4)

- **Palette:** the project's components (every `.ui.ron` under `assets/` with a `component:`
  declaration) are listed under "Components"; adding one inserts an instance node.
- **Make component from selection:** writes the selected subtree to a new component document, with
  chosen properties exposed as parameters (their current values become the defaults), and replaces
  the subtree with an instance. One undo step; undo restores the document byte for byte (the new
  component file stays on disk).
- **Enter and return:** opening an instance's component pushes the current document; return pops it
  (and re-expands it, so edits show at once).
- **Inspector:** an instance shows its component's parameters with their types and defaults; editing
  one is a `params.<name>` edit (null resets to the default).
- **Hierarchy:** an instance's expanded nodes are listed under it, dimmed and read only.
- **Previews:** the CPU and GPU previews draw the expanded document, so instances look as they do at
  run time.

## 7. Agent surface

| method | kind | what |
|---|---|---|
| `ui.components` | read | every component known to the `UiLibrary` and project: path, params, slots, users, diagnostics |
| `editor.ui_designer_make_component` | write | make component from a node: `{node, path, params: [{name, node, property}]}` |
| `editor.ui_designer_set_instance_param` | write | `{node, name, value}` (null resets) |
| `editor.ui_designer_enter_component` | write | `{node}`: open the instance's component |
| `editor.ui_designer_return` | write | back to the document that entered |
| `editor.ui_designer_instances` | read | the open document's instances, their expansion and diagnostics |

## 8. Tests and acceptance

`crates/regia_ui/tests/ui_components.rs`: three instances with different parameters and bind roots
give the expected tree and layout; slot content lands in the outlet; a 2-file cycle and a 9-deep chain
give their codes and placeholders; missing file and unknown parameter give placeholders; an edit to a
component reaches every instance on hot reload with focus and scroll kept; a component in a `repeat`
over 1,000 items spawns with the same allocation count as the inline form (counting allocator).
`regia_editor` tests: make-component then undo restores the text byte for byte; a save keeps instances
as references. Fuzz: `ui_ron` target fixtures cover the v8 fields.

## 9. Out of scope

Instance overrides of arbitrary fields (only declared parameters change), field-wise style merging,
string interpolation inside text (`"{label}"`), and runtime-switchable components.
