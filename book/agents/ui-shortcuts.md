# UI shortcuts (agents)

Guide topic: `agent.guide {"topic": "ui_shortcuts"}`. ADR-273 lane 7 (gap I02), ADR-374.

The shortcut registry (`regia_ui::shortcuts::UiShortcuts`) binds key chords to named actions. A chord is modifiers
plus one key, written `Ctrl+Shift+S`, `Alt+F4`, `Escape`. A shortcut lives in a scope: `global`, or a named context
the game switches on (`editor`, `pause_menu`). A context shortcut wins over a global one on the same chord. Pressing
the keys writes a `ShortcutTriggered { action, chord, scope }` message; game and menu code react to the action name.

- Letters match case-insensitively; Shift is an explicit modifier (`Shift+1`, not `!`).
- Auto-repeat does not fire. While a text input has focus, only chords with Ctrl, Alt or Meta fire.
- Two actions on one chord in the same scope are a conflict: registering is allowed, the chord triggers nothing, and
  `ui.shortcuts` lists it. An exact duplicate (same chord, scope and action) is refused at registration.
- `UiShortcuts::chord_label(action)` gives the text a menu shows (`Ctrl+S`, or `⌘S` style on macOS).

## Methods

- `ui.shortcuts {}`. Read-only. Returns `{ shortcuts: [{ action, chord, display, scope, label, active }], conflicts:
  [{ chord, kind, actions, scopes }], contexts }`. `active` is whether the action would fire now. Conflict `kind` is
  `same_scope` (nothing fires), `shadow` (a context shortcut hides a global one while its context is on) or `overlap`
  (the same chord in two contexts; ambiguous only when both are on).
- `ui.shortcut_trigger { action }`. Fires the action as if its keys were pressed, with the same scope and conflict
  rules. Returns `{ action, chord, scope }`. Mutating; permission `edit_scene`.
- `ui.shortcut_contexts { contexts }`. Replaces the active contexts (an empty array turns all off). Returns
  `{ contexts }`. Mutating; permission `edit_scene`.

```json
{"method": "ui.shortcuts", "params": {}}
{"method": "ui.shortcut_contexts", "params": {"contexts": ["editor"]}}
{"method": "ui.shortcut_trigger", "params": {"action": "bold"}}
```

Registration is done in game code (`UiShortcuts::register`); there is no agent method for it yet.

## Errors

The code is in `data.code`. The protocol code beside it is `F0003` for `unknown_action`, `F0013` for
`shortcuts_not_installed` and `F0002` for the rest.

| Code | Meaning |
|---|---|
| `unknown_action` | No shortcut has this action name. |
| `shortcut_inactive` | Every shortcut of the action is out of scope (context off) or shadowed by another action. |
| `shortcut_ambiguous` | Another action shares the chord in the same scope. |
| `shortcuts_not_installed` | The app has no `UiShortcuts` resource (no `UiPlugin`). |
| `bad_chord` | A chord string could not be parsed (Rust API only). |
| `duplicate_shortcut` | Same chord, scope and action already registered (Rust API only). |

`ui.shortcuts` takes no parameters. `ui.shortcut_trigger` requires an `action` string from its results. Context
switching requires `contexts`, an array of context name strings (a name no shortcut uses is accepted and matches
nothing); an empty array turns all contexts off. The
`shortcuts_not_installed` protocol error is returned when the app has no shortcut registry.

## Failure example

Captured CLI output from `regia agent call ui.shortcut_trigger '{"action":"missing-action"}'`:

```text
error: method call failed: the shortcut registry is not installed (-32004): {"code":"shortcuts_not_installed","hint":"the app needs regia_ui::UiPlugin"}
```

## Workflow

List shortcuts and their conflicts, enable the context needed by the action, then trigger the action by its registered
name. If it does not fire, inspect `active` and `conflicts` in the list response; same-scope conflicts make a chord
ambiguous.
