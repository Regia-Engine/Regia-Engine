# Editor tools (`editor.tool_*`, the Tools panel)

An **editor tool** is a separate program the editor launches from a manifest: a review queue, a
publishing calendar, an asset checker, a settings page. Each launch gets its own **scoped token**,
which can call only the methods and permissions its manifest names; the editor's own token is
never given to a tool. The tool shows up in the editor's **Tools** tab (`Panel::Tool`) with its
status, progress bars, a small panel view it describes, its log and the methods it provides.
Agents call those methods through `editor.tool_call`. Tools can be written in any language that
speaks WebSocket and JSON-RPC 2.0. The design record is ADR-1523.

The panel and these methods share one state: an agent sees a tool a person launched, and a person
sees progress an agent's call caused.

## Manifest: `<id>.tool.toml`

One file per tool, named after its id. The editor looks in these folders, in order, and the first
manifest of an id wins (later ones are listed as duplicates):

1. `<project>/.regia/tools/`
2. `<user dir>/tools/` (the editor's `user://` folder: `~/.local/share/RegiaEngine/regia-editor/tools/`
   on Linux, `%APPDATA%\RegiaEngine\regia-editor\tools\` on Windows)
3. each folder in `REGIA_TOOL_PATH` (separated like `PATH`)

```toml
format = 1
id = "red-rouge"                 # a-z, 0-9, _ and -, 1-64 bytes; the file is red-rouge.tool.toml
name = "Red-Rouge"
version = "0.4.0"
description = "Pick page, review queue and publish calendar."

[launch]
program = "uv"                   # absolute, relative to this file's folder (has a /), or on PATH
args = ["run", "red-rouge", "editor-tool"]
cwd = "."                        # relative to this file's folder (the default)
env = { RED_ROUGE_PANEL = "1" }  # never REGIA_TOOL_* or REGIA_PROJECT_DIR
autostart = true                 # launch when a project opens; ignored for project manifests

[scope]                          # what the tool's token may call
permissions = ["read", "play"]   # read, edit_scene, play, assets (never tools)
methods = ["replay.*", "camera.*", "render.out_*", "scene.query_entities"]

[[provides]]                     # methods the tool answers (editor.tool_call)
name = "pick.list"
description = "The pick page's scenes, newest first."
permission = "read"              # what a caller needs (default read)
params = [{ name = "limit", kind = "number", required = false, description = "At most this many." }]

[panel]
title = "Red-Rouge"
url = "http://127.0.0.1:8765/?panel=1"   # http/https only: the panel's "Open in browser" link
```

Unknown keys are refused, so a typo is an error, not a silently ignored setting. A manifest
that does not parse or validate is listed by `editor.tool_list` with its error; it never stops
the editor. `methods` patterns are exact names, prefixes ending in `*`, or `*`.

`autostart` is honored only for manifests in the user folder or `REGIA_TOOL_PATH`: opening a
project never runs a program by itself.

## What a launch does

The editor starts a second loopback listener (once), mints a fresh token bound to the manifest's
scope, and starts the program with these environment variables:

| Variable | Value |
|---|---|
| `REGIA_TOOL_ENDPOINT` | `ws://127.0.0.1:<port>`: the tool listener (not the agent endpoint) |
| `REGIA_TOOL_TOKEN` | the scoped token; send `Authorization: Bearer <token>` |
| `REGIA_TOOL_ID` | the manifest id |
| `REGIA_TOOL_PROTOCOL` | the agent protocol version |
| `REGIA_PROJECT_DIR` | the open project's root (absent without one) |

The token is never written to disk. It is revoked when the tool exits, is stopped, or the editor
closes; the next request on a revoked token answers `tool_token_revoked` and the connection
closes. Every token may call `agent.hello`, `agent.schema`, `agent.guide`, `errors.explain` and the
tool-side methods below, plus its manifest scope; the project's `.regia/agent.toml` still applies
on top. Audit and provenance records name the caller `tool:<id>`. Each token has its own
per-second budgets, so a busy tool does not spend an agent's.

The tool's stdout and stderr (prefixed `stderr: `) fill the last 200 lines of its log. When the
process ends with a non-zero code, or is killed, the editor logs a warning and adds an
`[editor] tool stopped: exit code 1` (or `killed by signal 9`) line to that log; `editor.tool_status`
then reports `status: "exited"` and the `exit_code` (null when a signal ended it). The editor itself
is unaffected, and the tool can be launched again. Only the
program the editor started is stopped or killed: a tool that starts its own children should exit
them when its connection drops or on `shutdown`.

## A tool's loop

```text
connect(REGIA_TOOL_ENDPOINT, REGIA_TOOL_TOKEN)
editor.tool_set_view { items: [...] }            # optional: what the panel shows
loop:
    events = editor.tool_poll { timeout_ms: 20000 }.events
    for event in events:
        call     -> editor.tool_reply { call, result } (or { call, error: { message, code } })
        click    -> handle the button `item`
        shutdown -> exit
    editor.tool_progress { task, fraction, state, message }   # whenever there is news
```

A request waits on its connection, so a tool that posts progress while a long poll is open uses a
second connection with the same token.

A Rust tool can connect with `regia_agent::client::Client::connect_tool_from_env()`, which reads
`REGIA_TOOL_ENDPOINT` and `REGIA_TOOL_TOKEN` and returns the client and `REGIA_TOOL_ID` (the names are
`regia_agent::scope::ENDPOINT_ENV`, `TOKEN_ENV` and `ID_ENV`).

## Agent-side methods

### `editor.tool_list`

Read-only. Param `refresh` (bool): re-read the folders first. Returns `tools` (one row per
manifest file: `id`, `name`, `version`, `description`, `source` = `project`/`user`/`tool_path`,
`path`, `valid`, `error`, `status`, `provides` (method names), `panel` {`title`, `url`},
`autostart`) and `dirs` (the folders searched).

### `editor.tool_status`

Read-only. Params `tool` (required) and `log_lines` (default 50, at most 200). Returns `status`
(`stopped`, `running`, `stopping`, `exited`, `failed`), `exit_code`, `failure`, `connected` (the
tool's process is live, `running` or `stopping`, and it has made a request on its current token;
`false` once it exited), `requests`, `launches`, `calls_in_flight`,
`queued`, `tasks` (each `task`, `label`, `fraction`, `state`, `message`), `view` (the items as set),
`log` and `selected` (the panel shows it).

### `editor.tool_methods`

Read-only. Param `tool` (optional). Returns `methods`: `tool`, `name`, `qualified`,
`description`, `permission`, `params` for every provided method. Provided methods are not in
`agent.schema` (they belong to a tool, not to the editor); this list is their schema. The
`editor.tool_*` methods themselves are in `agent.schema` with full param schemas (ADR-1582). `qualified` is the method's namespaced
name, `tool.<tool>.<name>` (ADR-1554): no engine or editor method uses the `tool.` prefix, and a
provided method is only ever reached through `editor.tool_call` with its tool id, so it can never
shadow an engine method.

### `editor.tool_call`

Calls a provided method and answers with the tool's result unchanged. Params `tool`, `method`
(required), `params` (object, default `{}`), `timeout_ms` (default 25000, 1 to 600000). The
caller needs the method's declared `permission` in the project profile; a tool calling another
tool needs `editor.tool_call` in its scope and the permission in its own scope too.

```json
{ "method": "editor.tool_call", "params": { "tool": "red-rouge", "method": "pick.list", "params": { "limit": 5 } } }
```

### `editor.tool_launch`, `editor.tool_stop`, `editor.tool_click`

Mutating, permission **`tools`**, which the default profile does not grant: add `"tools"` to the
`permissions` list in `.regia/agent.toml`. A person in the panel needs no permission.

- `editor.tool_launch { tool }` → `{ tool, status, endpoint }`.
- `editor.tool_stop { tool, force }` sends `shutdown` and kills the tool after 5 s (`force`: at
  once) → `{ tool, status, force }`.
- `editor.tool_click { tool, item }` presses a button of the tool's view (the tool gets a `click`
  event) → `{ tool, item, queued }`.

### `editor.tool_panel`

Read-only. Param `tool` (optional) selects it. Focuses the Tools tab (re-adding it when it was
closed) → `{ selected, focused, tools }`. A client that wants to know whether this engine offers
editor tools looks for `editor.tool_call` or `editor.tool_panel` in `agent.hello`'s `methods`.

## Tool-side methods

Called with the tool's own token. With the agent's token (or any token that is not a launched
tool's) every one of them answers `tool_caller_required` (-32600), whatever the params: an agent
that wants to see what a tool shows reads `editor.tool_status`, and one that wants to drive a tool
uses `editor.tool_call` and `editor.tool_click`. `agent.schema` lists these methods too, with the
same param schemas as the agent-side ones, so a tool author can read them from the editor.

**Call ids** are whole numbers (JSON integers, never strings): the `call` of a poll event is the
`call` of the `editor.tool_reply` that answers it. An id that is not open answers
`tool_unknown_call`.

### `editor.tool_poll`

Param `timeout_ms` (default 0, at most 60000). Answers `{ events }` at once when work is queued,
else when work arrives or at the deadline (empty). Events (at most 64 per answer):

- `{ kind: "call", call, method, params }`: answer with `editor.tool_reply`.
- `{ kind: "click", item }`: a button of the view was pressed.
- `{ kind: "shutdown" }`: exit now (killed after 5 s).

A second poll answers the first one empty.

### `editor.tool_reply`

Params `call` (integer) and exactly one of `result` (any JSON, passed through) or `error` (a
message string, or `{ message, code }`; the caller gets `tool_error` with `data.tool_code` = your
`code`). In `agent.schema`, `result` is the empty schema (any JSON value) and `error` is a string
or an object.

### `editor.tool_progress`

Params `task` (1-64 bytes; a new id adds a task), `label`, `fraction` (0 to 1; absent or `null`
is indeterminate), `state` (`running` default, `done`, `failed`, `cancelled`), `message`. The
panel shows at most 32 tasks; above that the oldest finished task drops. Each call is also an
`events.read` record of kind `tool_progress` whose `data` is the task plus `tool`.

### `editor.tool_set_view`

Param `items` (at most 256): the panel view, replaced whole. Each item is one of:

| `kind` | Fields |
|---|---|
| `heading` | `text` |
| `label` | `text` |
| `value` | `label`, `value` |
| `progress` | `label`, `fraction` (clamped to 0-1) |
| `button` | `id` (1-64 bytes, the click's `item`), `label` (default `id`) |
| `link` | `label`, `url` (`http`/`https`; opens in the browser) |
| `separator` | none |

An example of each, as `editor.tool_set_view` takes and `editor.tool_status` returns them:

```json
{ "items": [
  { "kind": "heading", "text": "Review queue" },
  { "kind": "label", "text": "3 scenes waiting" },
  { "kind": "value", "label": "Daily cap", "value": "40" },
  { "kind": "progress", "label": "Upload", "fraction": 0.25 },
  { "kind": "button", "id": "approve", "label": "Approve" },
  { "kind": "link", "label": "Pick page", "url": "http://127.0.0.1:8765/" },
  { "kind": "separator" }
] }
```

Text over 4096 bytes is cut. A bad item answers `F0002` naming its index (`items[2]: ...`).
`agent.schema` gives `items` as an array (at most 256) of a `oneOf` with one alternative per
`kind`, each with its fields, limits and `required` list.

## Error codes

| Code | JSON-RPC | Meaning |
|---|---|---|
| `tool_scope_denied` | -32005 | The method or its permission is outside the calling tool's scope. |
| `tool_token_revoked` | -32005 | The token's tool was stopped, exited or relaunched. |
| `tool_unknown` | -32001 | No manifest has this id. |
| `tool_manifest_invalid` | -32602 | The manifest of this id did not parse or validate (`editor.tool_list` shows why). |
| `tool_already_running` | -32600 | The tool is running; stop it first. |
| `tool_not_running` | -32600 | The tool is not running (or is stopping). |
| `tool_spawn_failed` | -32603 | The program could not be started, or the tool listener could not bind. |
| `tool_unknown_method` | -32001 | The tool does not provide this method. |
| `tool_call_timeout` | -32002 | The tool did not answer the call by `timeout_ms`. |
| `tool_exited` | -32603 | The tool exited before answering. |
| `tool_error` | -32603 | The tool answered with an error (`data.tool_code`), or it has 1024 undelivered events. |
| `tool_caller_required` | -32600 | A tool-side method was called without a tool token. |
| `tool_unknown_call` | -32001 | `editor.tool_reply` names a call that is not open. |

Bad parameters answer `F0002`; a missing permission `F0012`.
