# UI components (agents)

ADR-3147, `docs/specs/111-ui-components.md`. A **component** is an ordinary `.ui.ron` document (format v8 or later; the current version is 9) that other
documents instantiate by path. A health bar used on three screens is one file and three instances; editing the file
edits every instance, including on hot reload.

## In a `.ui.ron` file

The component declares the parameters and slots an instance may set:

```ron
// assets/ui/health_bar.ui.ron
(
    version: 8,
    component: (
        params: [
            (name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)]),
            (name: "fill",  type: color,  default: "#30C060", targets: [(node: "Fill", property: background)]),
            (name: "shown", type: bool,   default: true, targets: [(property: visible)]),
        ],
        slots: ["badge"],
    ),
    root: (name: "HealthBar", children: [
        (name: "Label", text: (spans: ["HP"]), bindings: [(source: "$.name")]),
        (name: "Fill", widget: progress(1.0), bindings: [(source: "$.hp", target: progress)]),
        (name: "Badge", slot: "badge"),
    ]),
)
```

An instance names the component with `use`, sets declared parameters, gives a view-model root for the component's
`$` sources and fills slots:

```ron
(use: "ui/health_bar.ui.ron", name: "P1Health", bind_root: "Party.p1",
 params: {"label": "P1", "fill": "#D04040"},
 slots: {"badge": [(text: (spans: ["!"]))]})
```

- **Parameters** are typed (`string`, `number`, `bool`, `color`) and write one property of a named node of the
  component: `text`, `command`, `class`, `background`, `border_color`, `tint`, `opacity`, `width`, `height`,
  `progress`, `visible`, `disabled`. Nothing else of a component can be changed from outside.
- **Instance fields**: `name` replaces the component root's name, `classes` are appended, `style` places the
  instance (replaces the component root's style). Any other field is an error (`ui_component_override`).
- **Bindings**: a source starting with `$` is relative to `bind_root` (`$.hp` with `bind_root: "Party.p1"` reads
  `Party.p1.hp`). Inside a `repeat` template, `bind_root: "."` binds the instance to its row's item. The rewrite
  happens once at load, so a component costs the same at run time as the same nodes written inline.
- **Slots**: `slots: {"badge": [...]}` replaces the outlet node's children; plain `children` fill the `default` slot.
- **Nesting**: components may use components, at most 8 deep and 4,096 instances per document.

## Errors

Expansion never panics and never drops a subtree. A failing instance becomes a visible placeholder node (named
`ui_component_error`, class `ui-component-error`, red, showing the code and path) and a diagnostic that
`ui.components` lists and the log reports.

| code | severity | cause |
|---|---|---|
| `ui_component_missing` | error | the `use` path does not exist or is not readable |
| `ui_component_invalid` | error | the component does not parse, or its declaration is wrong (a target node that does not exist, a type that does not fit its property, a default that does not fit its type, a duplicate name, a slot without exactly one outlet) |
| `ui_component_cycle` | error | the document instantiates itself through the chain (the message lists it) |
| `ui_component_depth` | error | more than 8 nested components |
| `ui_component_unknown_param` | error | `params` names a parameter the component does not declare |
| `ui_component_param_type` | error | a parameter value does not fit its declared type |
| `ui_component_unknown_slot` | error | `slots` names an undeclared slot, or plain children without a `default` slot |
| `ui_component_override` | error | the instance sets a field other than name, classes, style, params, bind_root, slots and children |
| `ui_component_bad_path` | error | `use` is empty, absolute, climbs out with `..`, or is not a `.ui.ron` |
| `ui_component_too_large` | error | more than 4,096 instances expand in one document |
| `ui_component_no_bind_root` | warning | the component binds `$` sources but the instance gives no `bind_root` |

## Hot reload

Saving a component re-expands every template that uses it (directly or through other components) and respawns
their roots. The focused node and scroll offsets are kept by key, the path of names from the root
(`HUD/P1Health/Fill`, `#2` for an unnamed node); keys that no longer exist are dropped.

## Packs

The asset pipeline's reference extraction (`regia_asset_pipeline::ui_refs`) lists each layout's `use` paths, so the
dependency graph, the missing-reference and orphan validators and the size map see a component as a dependency of
every layout that uses it, and a pack that ships a layout ships its components.

## Methods

| method | kind | what |
|---|---|---|
| `ui.components` | read | every component in the UI library (and under the project's `assets/` when an asset server is present): path, template, params, slots, users; every template's expansion diagnostics; the codes and the depth limit |
| `editor.ui_designer_make_component` | write | `{node, path, params?: [{name, property, node?}]}`: write the node's subtree to a new component (never overwriting) with those properties as parameters (current values become defaults), and replace the node with an instance. One undo step; undo restores the document exactly |
| `editor.ui_designer_set_instance_param` | write | `{node, name, value}`: set an instance parameter, checked against the declaration; null resets to the default |
| `editor.ui_designer_enter_component` | write | `{node}`: open the instance's component; the current document waits |
| `editor.ui_designer_return` | write | back to the document that entered (save the component first) |
| `editor.ui_designer_instances` | read | the open document's instances (parameters with types, defaults and values, slots, the inherited read-only nodes, diagnostics) and the project's components |
| `editor.ui_designer_set_instance_slot` | write | `{node, slot, content?}`: set the content of a slot the component declares (`content`: nodes in the form `editor.ui_designer_edit` adds); null or absent clears it so the component's default content shows; `[]` stores an empty slot, which hides the default (the inspector row says `empty (hides the default)`, and Clear removes it like any other content). One undo step; the inspector's slot rows call the same function (ADR-7677) |

The designer's own error codes for these methods: `ui_designer_no_assets` (the document is not under a project's
`assets/`), `ui_designer_exists` (the component path is taken), `ui_designer_not_instance`,
`ui_designer_component_missing`, `ui_designer_component_invalid`, `ui_designer_unknown_param`,
`ui_designer_param_type`, `ui_designer_not_entered` (return without enter), `ui_designer_dirty` (return with unsaved
component edits) and `ui_designer_unknown_slot` (the component declares no such slot). The shared `editor.ui_designer_*` codes (`ui_designer_bad_node`, `ui_designer_bad_value`, ...) are on
the [UI designer](ui-designer.md) page.

## In the designer

- The hierarchy's **Components** row lists the project's components; drag one onto a row (or click it) to add an
  instance. **Rescan** reads `assets/` again.
- An instance's own nodes are listed under it, dimmed and marked `(inherited)`: they come from the component and are
  edited there.
- The inspector of an instance shows its component (with **Enter**), `bind_root` and each parameter with its type and
  default; an empty field resets the parameter. **Return** in the toolbar comes back.
- Under **Slots** the inspector lists each slot the component declares and what fills it (`component default` when the
  instance sets nothing, `empty (hides the default)` when it stores an empty list). **Set…** replaces the slot's content
  with one palette node, a palette chip dropped on the slot row adds that node to the slot, and **Clear** (enabled for
  any stored content, an empty list included) shows the component's default again. Each is one undo step and writes
  the same bytes as `editor.ui_designer_set_instance_slot`.
- **Make component** (inspector, any node but the root): a path under `assets/`, the properties to expose, and the
  button.
- Both previews (the layout preview and the themed GPU preview) draw instances expanded, as the game does.

## Example session

```json
{"method": "editor.ui_designer_open", "params": {"path": "assets/ui/hud.ui.ron"}}
{"method": "editor.ui_designer_make_component", "params": {"node": "#P1Health", "path": "ui/health_bar.ui.ron",
  "params": [{"name": "label", "property": "text", "node": "#Label"}, {"name": "fill", "property": "background"}]}}
{"method": "editor.ui_designer_edit", "params": {"op": "add", "parent": "/", "node": {"use": "ui/health_bar.ui.ron", "name": "P2Health", "bind_root": "Party.p2"}}}
{"method": "editor.ui_designer_set_instance_param", "params": {"node": "#P2Health", "name": "label", "value": "P2"}}
{"method": "editor.ui_designer_set_instance_slot", "params": {"node": "#P2Health", "slot": "badge", "content": [{"name": "Crown", "text": {"spans": ["*"]}}]}}
{"method": "editor.ui_designer_save", "params": {}}
{"method": "ui.components", "params": {}}
```
