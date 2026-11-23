# Runtime focus mutation replay (agents)

The runtime focus graph is updated from the live UI hierarchy each frame. Existing
`ui.nav_graph`, `ui.nav_move` and `ui.a11y_act` calls let an agent inspect and exercise it after
UI changes; this page documents composed histories without adding protocol methods.

```json
{"method":"ui.nav_move","params":{"direction":"next"}}
{"method":"ui.nav_graph","params":{}}
{"method":"ui.a11y_act","params":{"node":42,"action":"focus"}}
```

Use `ui.nav_move` to establish a focus, apply the scene mutation through the existing scene
editing calls, then inspect `ui.nav_graph` and move again. A successful navigation result reports
the resulting `focused` entity and a reason such as `initial`, `recovered`, `geometry`, or `tab`.
An accessibility focus request should update the same focus state used by keyboard and gamepad
navigation. Within a modal layer, the returned focus must remain in that layer; while the layer
has no navigable child, focus may be empty until a child is restored.

The CPU integration history `focus_mutation_sequences` executes keyboard Tab/Shift-Tab, gamepad
D-pad, `nav_move`, and `A11yAction::Focus` around hidden and removed nodes, disabling and
removal of ancestors, parent detachment, and an emptied/restored modal. It checks live focus
markers and a fixed-seed model after each update, including a recycled entity slot with a new
generation. It does not use screenshots or change accessibility adapter behavior.

Guarantees the replay relies on (hardened by this lane, each was a demonstrated failure):

- After every update, focus names a live, eligible node (focusable, laid out, not disabled) or
  the documented empty state — never a despawned, hidden or disabled node.
- A node disabled while focused is repaired in the same update; disabling a non-focusable
  container leaves its children navigable (only the disabled node itself leaves the graph).
- A node hidden in the same frame it was focused (through `ui.nav_move`) repairs on the next
  update; `nav_move` records the focused rect immediately.
- An emptied modal keeps the empty state, and a restored default-focusable child takes focus
  again on the next update without a manual press (navigation mode; `sync_layers` refills the
  emptied top layer's default).
- A recycled entity slot (same index, new generation) never resurrects the old focus: the dead
  entity is never named, and navigation reaches the replacement.

Errors retain the existing protocol codes: `no_focusables` when the active graph is empty,
`ui_not_installed` when the runtime UI is absent, and `unknown_node`, `node_inert`,
`node_disabled`, or `unsupported_action` when an accessibility request cannot target the
current node. See [UI navigation](ui-navigation.md) and [Accessibility](ui-a11y.md) for complete
method schemas and refusal details.
