# Headless editor

`regia editor --headless` runs the full editor without a window (specs/21 § 1, specs/23 § 2,
ADR-2412): the edit world, the asset database with its live cooker, the hot-reload host (`--game`)
and the agent protocol server. There is no window, no surface and no egui pass. CI jobs and cloud
agents use it to open levels, query and edit the world, run generators, play in the editor and save,
all through the agent protocol.

```sh
regia editor --headless --project path/to/game            # offscreen views on the default adapter
regia editor --headless --no-gpu --project path/to/game   # no device: view methods answer editor_headless
regia-editor --headless --project path/to/game            # the editor binary itself, same flags
regia mcp --headless --project path/to/game               # starts one for an MCP client, see below
```

`regia editor` finds the editor binary in this order: `--editor-bin FILE`, the `REGIA_EDITOR`
variable, `regia-editor` next to the `regia` binary, then `regia-editor` on `PATH`. A missing binary
fails with `editor_not_found` (build it with `cargo build -p regia_editor`). On Unix the command
replaces itself with the editor, so signals and the exit status are the editor's. On Windows the
editor runs as a child in the same console and `regia editor` returns its exit status.

## Ready and exit lines

When the agent server listens, the editor prints one JSON line on stdout and flushes it. Wait for
this line (read stdout line by line); never wait on a timer:

```json
{"event":"ready","pid":4242,"endpoint":"/p/.regia/agent/endpoint.json","url":"ws://127.0.0.1:41234","project":"/p","adapter":{"name":"llvmpipe (LLVM 19.1.7, 256 bits)","backend":"vulkan","device_type":"cpu"}}
```

`adapter` is `null` with `--no-gpu` or when no adapter could be opened. Connect as usual
(`regia agent call`, `regia_agent::client::Client::connect_project`, or the `url` and the token in
`.regia/agent/token`).

Just before it exits, the editor prints the exit line:

```json
{"event":"exit","cause":"agent.shutdown","frames":118,"saved":["assets/maps/arena.level.ron"],"failed":[],"autosaved":null,"status":0}
```

`cause` is `agent.shutdown`, `quit.request`, `signal`, `exit_after_frames` or `parent_exit`. The exit status is 0,
or 1 when a document with edits could not be saved (`failed` says which and why).

## Ending a headless editor

| How | What is saved | Exit |
|---|---|---|
| `agent.shutdown {}` | every document with edits (the level, its terrain and tilemaps, an open UI document); an untitled level goes to the first free `assets/unsaved*.level.ron` | at the end of the frame |
| `agent.shutdown { "save": false }` | nothing; edits are dropped | at the end of the frame |
| SIGTERM, SIGINT or SIGHUP (Unix); Ctrl+C, Ctrl+Break or closing the console (Windows) | as `agent.shutdown {}` | after the save; a second signal while saving exits at once |
| `quit.request {}` | as in the windowed editor: a dirty level is autosaved for the next launch to recover | at the end of the frame |
| `--exit-after-frames N` | as `agent.shutdown {}` | after frame N |
| `--exit-on-stdin-eof`, when stdin reaches its end (the editor binary's flag) | as `agent.shutdown {}` | after the save; `cause` is `parent_exit` |

A failed save in `agent.shutdown` answers `shutdown_save_failed` and the editor keeps running; fix
the cause and call again, or pass `save: false`. The editor removes `.regia/agent/endpoint.json` when
it exits. The windowed editor answers `agent.shutdown` too: it saves the level, then the panels' own
documents (the Material panel, Project Settings), then closes like a clean quit; if a panel document
cannot be saved, it stays open and says why in the status bar.

## Views without a window

The headless editor opens a GPU device at start (any Vulkan, DX12 or Metal adapter; lavapipe
works) and shares it with every view method: `editor.capture_viewport` and
the other viewport and panel captures render offscreen exactly as in the windowed editor. A
`seeing.screenshot` of the editor window answers the perspective viewport from the editor camera
(1280 x 720; the play world's, while playing).

Without an adapter (`--no-gpu`, or none found) those methods answer:

```json
{"code":-32004,"message":"the headless editor has no GPU adapter (started with --no-gpu)","data":{"code":"editor_headless","hint":"..."}}
```

and a window `seeing.screenshot` fails with an `F0001` error whose message names `editor_headless`.
Everything that does not draw keeps working.

## Methods

| Method | Permission | What it does |
|---|---|---|
| `editor.headless_status {}` | `read` | `{ headless, adapter, no_adapter_reason, frames, project, level, dirty, asset_watch, game }`. `headless` is `false` in the windowed editor. |
| `editor.open_level { path, discard? }` | `edit_scene` | Opens a project level file (`.level.ron` or `.level.d`), replacing the open level; undo history and selection reset. Answers `{ path, level, entities }`. |
| `agent.shutdown { save? }` | `play` | Saves the documents with edits (unless `save: false`), then exits. Answers `{ exiting: true, saved, headless }` before the editor ends. |

Errors (the `data.code` of the answer):

| Code | When |
|---|---|
| `editor_headless` | a view method on a headless editor without an adapter; also an offscreen `seeing.screenshot`, `render.out_start` / `render.out_job` (product jobs included), `seeing.reference_trace`, `looks.render_board`, `viewer.render_still`, `ui.theme_preview` and `editor.ui_designer_render` there, which open no device of their own (ADR-5180) |
| `no_project` | `editor.open_level` (or a `gen.*` method) with no project open |
| `level_not_found` | `editor.open_level`: no such file under the project |
| `level_dirty` | `editor.open_level`: the open level has unsaved edits and `discard` is not true |
| `prefab_open` | `editor.open_level`: a prefab edit session is open; close it with `editor.close_prefab` first |
| `level_unreadable` | `editor.open_level`: the file does not load as a level |
| `shutdown_save_failed` | `agent.shutdown`: a document could not be saved; the editor keeps running |
| `editor_not_found` | `regia editor`: the editor binary could not start (CLI error text, not a protocol code) |

## A CI job

```sh
regia editor --headless --project game > editor.out &
until grep -q '"event":"ready"' editor.out; do sleep 1; done      # or read the pipe line by line
regia agent call editor.headless_status
regia agent call editor.open_level '{"path":"assets/maps/arena.level.ron"}'
regia agent call scene.query_entities '{"name":"Spawn*"}'
regia agent call gen.verify                                       # see the generators page
regia agent call agent.shutdown
wait                                                              # exit status 0: saved and clean
```

Generators (`gen.list`, `gen.run`, `gen.verify`) are on [Generators](generators.md).

## Started by `regia mcp`

```sh
regia mcp --headless --project game                       # MCP over stdio
regia mcp --headless --http --project game                # MCP over Streamable HTTP
claude mcp add my-game -- regia mcp --headless --project /path/to/game
```

When no app of the project answers, the adapter starts `regia-editor --headless --exit-on-stdin-eof` (found as
`regia editor` finds it, or `--editor-bin FILE`), waits for its ready line, then serves MCP. `--no-gpu` passes
through. If an app already answers it attaches, and that app stays up when the adapter ends. `--attach`, like
no flag at all, attaches and never starts anything.

Startup failures are CLI errors, exit status 1, and leave no editor running:

| Code | When |
|---|---|
| `mcp_editor_start_failed` | The editor binary could not start. |
| `mcp_editor_exited` | The editor ended before its ready line; the error gives its status. |
| `mcp_editor_ready_timeout` | No ready line in `--editor-timeout` seconds (default 120). |

**No orphan.** The adapter holds the write end of the editor's stdin. When that pipe closes, the editor saves its
documents with edits and exits (`cause: "parent_exit"`). The kernel closes the pipe however the adapter ends: the
MCP client went away, the adapter was killed, SIGKILL included. On a normal end the adapter also waits for the
editor, up to `--editor-timeout`, and kills it after that. An HTTP adapter has no client to lose, so it runs until
it is stopped. The editor's own output goes to the adapter's stderr with the prefix `regia mcp: editor:`.

An agent that calls `agent.shutdown` ends the editor while the adapter stays up: later tool calls answer that no
app is running.

## Tests

```sh
cargo test -p regia_editor --lib headless                # methods, save on shutdown, open_level, gen.* handlers
cargo test -p regia_editor --test headless_editor        # the real binary: ready line, calls, shutdown, SIGTERM
cargo test -p regia_cli --bin regia editor                # binary lookup, flags passed to the editor
cargo test -p regia_cli --test mcp_headless               # failure paths with a fake editor; no orphan; the real editor
```

The process test starts `regia-editor --headless --no-gpu` on a fixture project, waits for the
ready line on stdout, opens a level, queries it, edits and saves it through the protocol, checks the
file on disk, and ends with `agent.shutdown` (and, on Unix, with SIGTERM after an unsaved edit) with
exit status 0.
