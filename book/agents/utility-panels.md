# Utility panels

A utility panel is an editor panel built without code: a UI-designer document (`.ui.ron`) and a driver
(a logic graph or a Lua script), named by a `.editorpanel.ron` sidecar under the project's `assets/`
(ADR-8189, format: `docs/formats/editor-panel-ron.md`). Editor-only: runtime hosts do not register
`editor.utility_panels`.

Open one from **Window → Utility Panels**, or with `editor.utility_panels { "op": "open", "id": ... }`. The
starter sample **Rename selection with prefix** (`starter/rename_with_prefix`) is always listed; its files are
in `crates/regia_editor/starter/utility_panels/` and the starter content installs them as
`assets/starter/utility_panels/`, where they become an ordinary project panel you can copy and change.

## How a panel runs

The document runs in its own runtime UI app (the UI designer's interactive preview), drawn by the runtime UI
renderer into the tab at the sidecar's `size` in points, so it follows the appearance UI scale. Click the
panel or press **Tab** to give it focus; then **Tab** and **Shift+Tab** move through its controls, typing goes
to the focused text field, **Enter** or **Space** presses the focused button, and **Escape** gives focus back
to the editor. The tab's status line shows the last command's result in theme colors.

A node with `command: "<name>"` sends that command to the driver with:

- `values`: every named node's current text (text inputs; a toggle reads `On` or `Off`), keyed by node name;
- `selection`: the selected entities with their level names, in selection order.

The driver answers a list of editor calls. They run in order through the same route as the editor console
(permission classes, parameter checks, audit, client `editor:utility_panel`) inside **one undo step**
labelled `<title>: <command>`. When a call fails, the command's earlier edits are reverted.

### The safe subset

A call runs only when it is

- an `editor.*` method of permission class `read` or `edit_scene` (the classes a Tools scope may hold,
  without `play` and `assets`), or
- `editor.command { run }` or `editor.extension_action { id }` naming a live action of the editor actions
  registry (`bind` and `pin` are refused).

`editor.utility_panels`, `editor.console_run` and `editor.console_complete` never run. Any other call refuses
the whole command with `utility_panel_method_refused` before anything runs.

### Lua driver

`driver: Lua("x.lua")` runs the script in the mod sandbox (no `io`, `os`, `package`, `debug` or `load`;
instruction and memory budgets; `docs/specs/84-lua-mods.md`):

```lua
regia.on("command", function(ctx)
    if ctx.command ~= "rename" then return end
    for _, item in ipairs(ctx.selection) do
        regia.emit("editor.call", { method = "editor.outliner_rename",
            params = { entity = item.entity, name = ctx.values.prefix .. item.name } })
    end
end)
```

`ctx.command` is the command, `ctx.values` the values table, `ctx.selection` a list of `{ entity, name }`
(`entity` is the entity's bits as a number). Each `regia.emit("editor.call", { method, params })` asks for one
call; whole numbers go out as integers. A runtime error fails the command (`utility_panel_driver_failed`) and
the script restarts with fresh state for the next one.

### Logic graph driver

`driver: Logic("x.logic.ron")` compiles the graph with one extra action task, `editor::Call`. A command is an
inbox event: a rule with `event: EntityEvent("rename")` fires on the `rename` command. The values are in the
entity blackboard under their node names, and `selection_count` holds the number of selected entities.

```ron
(
    format_version: 1,
    rules: [(
        name: "rename",
        event: EntityEvent("rename"),
        condition: Compare(key: "selection_count", op: Gt, value: 0.0),
        actions: [(
            task: "editor::Call",
            set: {
                "method": Literal(String("editor.outliner_rename")),
                "for_each_selected": Literal(Bool(true)),
                "name": Literal(String("{prefix}{name}")),
            },
        )],
    )],
)
```

`method` names the call; every other `set` entry is a parameter. With `for_each_selected: Bool(true)` the
action is one call per selected entity with `entity` set to it. In string parameters `{name}` is that entity's
name and `{<node name>}` a panel value; `{{` and `}}` are literal braces. A template naming no value, or a
failed action, fails the command.

## `editor.utility_panels`

Permission `edit_scene` (a command can edit the level), mutating.

| `op` | Parameters | Result |
|---|---|---|
| `list` (default) | — | `{ panels: [{ id, title, source: "starter" \| "project", open, layout?, driver?: { kind, path }, error?: { code, message } }] }` |
| `open` | `id` | `{ id, title, runs, values, status }`; the tab is opened (Window → Utility Panels) |
| `input` | `id`, `input`: `{ action: click \| focus \| type \| set_value \| list \| scroll_to, node: "#name" or a node path, text?, value?, index? }` | the `state` result plus `ran: [{ command, calls, ok, code, message }]` |
| `state` | `id` | `{ id, title, runs, values, status }`; `status` is the last command's `{ command, calls, ok, code, message }` |

A project panel's id is its sidecar path without `.editorpanel.ron` (`assets/tools/rename`). A sidecar that
does not load is listed with its `error` and cannot open.

```json
{"method":"editor.utility_panels","params":{"op":"list"}}
{"method":"editor.utility_panels","params":{"op":"open","id":"starter/rename_with_prefix"}}
{"method":"editor.utility_panels","params":{"op":"input","id":"starter/rename_with_prefix","input":{"action":"type","node":"#prefix","text":"Box_"}}}
{"method":"editor.utility_panels","params":{"op":"input","id":"starter/rename_with_prefix","input":{"action":"click","node":"#rename"}}}
```

After the last call `ran[0]` is `{ "command": "rename", "ok": true, ... }` and every selected entity's name
starts with `Box_`; `editor.undo` reverts all of them at once.

## Errors

Codes are lowercase and listed by `errors.explain`.

| Code | When |
|---|---|
| `utility_panel_not_found` | no panel has the id |
| `utility_panel_not_open` | `input` or `state` before `open` |
| `utility_panel_bad_params` | an unknown `op`, a missing `id` or `input`, or an input the document refused |
| `utility_panel_parse` | the sidecar is not the v1 shape (unknown or missing field, version 0) |
| `utility_panel_newer` | the sidecar was written by a newer editor |
| `utility_panel_invalid` | a sidecar field is out of range (title, paths, size) |
| `utility_panel_read_failed` | the sidecar, layout or driver file could not be read |
| `utility_panel_layout` | the `.ui.ron` did not parse or spawn |
| `utility_panel_driver_load` | the logic graph did not compile, or the Lua script did not load |
| `utility_panel_driver_failed` | the driver failed on a command, or asked for more than 256 calls; nothing ran |
| `utility_panel_method_refused` | a call outside the safe subset; nothing ran |
| `utility_panel_call_failed` | a call failed or answers only in a later frame; the command's edits were reverted |

`utility_panel_driver_failed`, `utility_panel_method_refused` and `utility_panel_call_failed` arrive in the
command result (`ran[].code`, `status.code`), not as the reply's error: the input itself was delivered.

## Known limits

- Utility panel tabs are not saved in layouts; reopen them from Window → Utility Panels.
- A driver cannot read a call's result; Lua and logic drivers see only the command's context.
