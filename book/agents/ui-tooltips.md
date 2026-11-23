# Tooltips and hints (agents)

Tooltips and the controller hint bar are part of the runtime UI (`regia_ui`). An agent reads them through
`ui.state` and drives them with the same input a player uses: move focus with `ui.focus`, press a button with
`ui.click`, wait with `play.wait_until`. Guide topic: `agent.guide {"topic": "ui"}`. See [Runtime UI](ui.md) for the
rest of the `ui.*` methods.

## What `ui.state` reports

- `tooltip` is `null` when no tooltip is open. Otherwise `{ node, text, trigger }`: the node that owns it, the text
  on screen (hint placeholders already expanded for the last device) and `trigger`, `hover` or `focus`.
- `hints` is `{ device, bar }`. `device` is `keyboard_mouse`, `xbox`, `playstation` or `steam_deck`. `bar` is the
  hint-bar lines for that device, such as `"A Select"` and `"B Back"`; it is empty when the screen sets none.
  A hint bar belongs on screen only while `input_mode` is `navigation`.

There are no tooltip-only methods or error codes. `ui.open_popup` with `popup: "tooltip"` keeps a node's tooltip open
until `ui.close_popup` ([popups and menus](ui-menus.md)).

## Agent request details

`ui.state {}` takes no parameters. For the focus workflow, registered `ui.focus` accepts one selector: `node` (the
id from `ui.query`), `name`, `text`, `text_contains`, `class`, `type` or `state`. `ui.click` accepts the same
selectors plus `button` (`left`, `right`, `middle`) and `count` (1 to 8).

`ui.focus` reports `ui_node_required`, `ui_node_not_found`, `ui_node_ambiguous`, `ui_node_disabled`,
`ui_node_blocked_by_layer` or `ui_node_not_focusable`; `ui.click` can also report `ui_node_not_visible`. These use
`F0002` for invalid/ambiguous/refused nodes and `F0003` for `ui_node_not_found`. Both methods return `F0013`
(`ui_not_installed`) when the app has no runtime UI hook. The stable reason is in `error.data.reason`.

## The rules

| Rule | Behavior |
|---|---|
| Candidate | In `pointer` mode the hovered node; in `navigation` mode the focused node. Only one tooltip is open at a time. |
| Delay | The node's `delay` (default 0.5 s) of hover. Focus waits the node's `focus_delay` (0.8 s by default). |
| Warm-up | While a tooltip is open, and for 0.4 s after one closed, the next tooltip opens without waiting. |
| Leave | Moving off the node (or moving focus away) closes it and resets the wait. |
| Dismiss | A press on the node, Enter or A (submit), Escape or B (cancel) and a scroll close it. It stays closed until the pointer or focus leaves the node. |
| Auto hide | Off by default; `TooltipPolicy::auto_hide` closes it after a number of seconds. |

These delays step on `RealTime`. `play.step` still moves them, because a manual step advances `RealTime`
by the step before `Time` is scaled. Pause and `time_scale` do not freeze or stretch a tooltip. A live
run uses the frame's real delta, not a separate wall-clock wait outside the frame.

## Gamepad text and glyphs

A node can carry `TooltipNavText`, which replaces the tooltip text while the UI is in `navigation` mode. Both
texts accept placeholders that expand to the glyph of the last device: `{activate}`, `{back}`, `{secondary}`,
`{menu}`, `{prev_tab}`, `{next_tab}`, `{page_up}` and `{page_down}`. Unknown names stay as written.

| Action | Keyboard | Xbox | PlayStation | Steam Deck |
|---|---|---|---|---|
| activate | Enter | A | Cross | A |
| back | Esc | B | Circle | B |
| secondary | Shift+Enter | Y | Triangle | Y |
| menu | F10 | Menu | Options | Menu |
| prev_tab | Ctrl+PgUp | LB | L1 | L1 |
| next_tab | Ctrl+PgDn | RB | R1 | R1 |
| page_up | PgUp | LT | L2 | L2 |
| page_down | PgDn | RT | R2 | R2 |

The host sets the `HintDevice` resource from the last device used; it defaults to `keyboard_mouse`. A screen sets
the `HintBar` resource (`bar.set([(HintAction::Activate, "Select"), (HintAction::Back, "Back")])`).

## Checking a tooltip

```text
ui.focus { "name": "buy_button" }
play.step { "frames": 60 }          # about one second at 60 fps, past the 0.8 s focus delay
ui.state  ->  { "tooltip": { "node": 4294967301, "text": "Press A to buy", "trigger": "focus" }, ... }
```

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":1,"method":"ui.focus","params":{"text":"__capture_missing_tooltip_target__"}}
```

Response:

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"no UI node matches","data":{"code":"F0003","hint":"call ui.query with fewer filters to see what exists","reason":"ui_node_not_found"}}}
```
