# Editor status bar, drawers and console line (agents)

ADR-7754. Code: `regia_editor::shell_status`, `regia_editor::drawers`, `regia_editor::console`.

## Status bar: `editor.status_bar`

The status bar's indicators are buttons (Tab reaches them, Enter or Space presses them):

| Indicator | Shows | Click |
|---|---|---|
| Content, Log | the drawer buttons (▾ while open) | toggle the content or log drawer (`view.content_drawer`, `view.log_drawer`) |
| Unsaved: N | documents with edits not on disk (hover lists them) | File ▸ Save all: opens the Save all dialog (every unsaved document in one list with checkboxes) |
| Jobs: N | running background jobs: the game build, package, cache clear, reflection capture bake, Import settings apply, navigation preview, the Content Browser import and the GI bake runner | a list with each job's progress and a Cancel that uses the job's own cancel path |
| Trace / ● Recording trace | whether the trace recorder writes | start or stop recording through `editor.trace_record` (the file opens in Diagnostics ▸ Timeline when it stops) |
| Problems: N | assets that failed or warned in the last validation run | open the Problems tab (`window.open_problems`) |
| revision slot | the branch and the changed-path count, from the revision-control status poll (`shell_status::RevisionSlot { label, tooltip }`, filled once per poll); the slot is drawn by the revision-control status bar button, not by the indicators | opens the Revision Control window |

`editor.status_bar {}` returns the same values (read only):

```json
{ "unsaved": { "count": 1, "documents": ["the level"] },
  "jobs": { "count": 2, "rows": [{ "id": "package", "label": "packaging linux", "progress": 0.25 },
                                 { "id": "capture_bake", "label": "baking reflection captures", "progress": null }] },
  "trace": { "recording": false },
  "problems": { "count": 3, "failed": 1, "warnings": 2 },
  "revision": null,
  "drawer": "log" }
```

Cancel a job with `editor.cancel_task { id }` (the row's `id`); the GI bake runner's row `bake_gi` is
`editor.bake_gi_cancel`. `problems` is `null` before the first validation run.

## Drawers: `editor.drawer`

A drawer slides the Content Browser or the Output Log up from the bottom edge over the dock area (about
40 % of its height) without changing the dock layout. It closes on Esc, on its Close button, on a press
outside it (the status bar excepted) or when the keyboard focus moves to a widget outside it. While a
drawer is open, Esc only closes the drawer: it does not also stop play or ask to close the editor. Its Dock
button closes it and opens the panel as a normal tab. The content drawer is the Content Browser itself
(same folder and filters as its tab); while it is open, a docked Content tab shows a one-line note instead
of a second copy of the browser (ADR-8702). Ctrl+Shift+Space toggles the content drawer (not during play or
while a text field has the focus, like the palette's shortcuts); rebind
`view.content_drawer` in the command palette (F2) to move it. Opening the content drawer focuses its
Dock button; opening the log drawer focuses the console line. `view.dock_drawer` docks the open drawer
from the keyboard.

`editor.drawer { drawer, open?, dock? }`:

| Param | Type | Meaning |
|---|---|---|
| `drawer` | string | `content` or `log` (required) |
| `open` | boolean | open or close it (opening one closes the other) |
| `dock` | boolean | `true` closes it and opens its panel as a tab (next frame) |

Result: `{ drawer, open, docked, open_drawer: "content" | "log" | null }`. Without `open` or `dock` it
only reports.

## Console line: `editor.console_run`, `editor.console_complete`

The log drawer and the Output Log tab end with a console line. It runs:

- an editor command id: `edit.undo` (runs `editor.command { run: "edit.undo" }`; the log prints the
  palette's status line);
- an agent method with JSON parameters: `editor.focus {"entity": 12}` (no parameters: `{}`);
- a project console variable, when the project registers any (`console::ConsoleVars::register`):
  `name` prints it, `name value` sets it.

Commands and methods go through the same dispatcher as an agent call (`regia_agent::call_local`): the
permission profile of `.regia/agent.toml`, the parameter checks, the rate limits and the audit log apply,
so a line answers exactly what the agent call answers. A line the person types is audited and stamped in
`agent.changes` as client `editor:console` (not as the connected agent). A line an agent sends through
`editor.console_run` stays that agent's: a scoped tool token (`tools.*` launches) keeps its scope, so the
inner command or method is refused with `tool_scope_denied` when the scope does not list it, exactly as a
direct call would be. The line and its result are printed to the Output Log (target
`regia_editor::console`).

Keys: Enter runs, Tab completes (a single match completes and adds a space; several extend to their
common prefix and are listed under the line with their parameters from the schema), Up and Down walk the
history, Shift+Tab moves the focus back out of the line. The history is kept per project in `.regia/console_history.txt` (one line per entry, newest
last, 200 lines) and survives a restart.

`editor.console_run { line }` runs a line as typed there (it joins the history). Result:
`{ line, reply, printed, pending }`: `reply` is the called method's own reply (`{ var, value, previous? }`
for a variable), `printed` the log line; `pending: true` when the method answers in a later frame (the
log prints it then). An error of the called method is returned as that error.

`editor.console_complete { prefix }` is Tab: `{ prefix, line, count, completions: [{ text, kind, hint }] }`
with `kind` `command`, `method` or `var` and `hint` the title, the parameters (`entity: integer,
frame?: boolean`, `?` = optional) or the value.

## Errors

| Code | When |
|---|---|
| `console_empty_line` | `line` is missing or empty |
| `console_unknown_name` | the first word is not a command id, a method or a variable (the hint names close ones) |
| `console_bad_params` | the text after a method is not a JSON object; `prefix` is not a string |
| `console_command_args` | a command id was given arguments |
| `console_nested` | the line calls `editor.console_run` or `editor.console_complete` |
| `console_no_reply` | a deferred method ended without answering |
| `bad_drawer` | `drawer` is not `content` or `log` |
| `bad_param` | `open` or `dock` is not a boolean |

## Quick settings

The main toolbar's Quick settings menu gathers: Snap and its move, rotate and scale steps (the viewport
toolbar's Snap, `editor.set_snap`), Realtime viewport (`view.toggle_realtime`), the main viewport's Grid,
Icons, Bounds, Colliders and Stats flags and Preview sun and sky (its Show menu, `editor.set_viewport`),
and Autosave on or off (Preferences ▸ Autosave interval: off writes 0, on restores the default;
`editor.prefs_set`). Each entry changes the same state as its home control.
