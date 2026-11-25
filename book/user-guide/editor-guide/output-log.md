# Output Log

Panel id: `output_log`.

The **Output Log** tab shows the engine's log: every record the editor, the engine and a running
game write, with a level filter, a target filter and a text search. The records are the same ring an
agent reads with `logs.read`, so the tab and an agent see the same lines. A `path:line` or an asset
path in a line is a link you can click. Depth: [Editor panels at scale](../../agents/editor-panel-timings.md)
(how the list handles 100 000 lines), [Viewport tools](../../agents/viewport-tools.md) (the link
methods).

## Open it

The tab is in the default layout's bottom group, next to **Content**. From the keyboard press
Ctrl+Shift+P and pick **Open the Output Log tab** (command `window.open_output_log`); bind a key to it
in the keymap if you use it often. An agent brings it forward with
`editor.layout_apply {"open": "output_log"}`.

## Read and filter the log

1. Each row shows the level (colored), the target (the part of the engine that logged it) and the
   first line of the message. A multi-line message ends in `…`.
2. Pick the lowest level to show from the drop-down (**ERROR**, **WARN**, **INFO**, **DEBUG**,
   **TRACE**); it starts at **INFO**.
3. Type a target prefix in the first box (hover says "target prefix") and a text in the second (hover
   says "search"). With a filter that matches nothing the tab says "(no records match the filters)".
4. Click a row's text to show the whole message, wrapped, under the list; **Close** hides it. Clicking
   the row again also closes it.
5. New lines keep the view at the bottom.
6. Click a link in a row. A source file opens in your code editor at that line (the `code.editor_command`
   preference, else the project's `.regia/agent.toml`, else `$VISUAL` or `$EDITOR`); an asset path opens
   the way a double-click in Content would. The result shows beside the filters.

The tab keeps the last 100 000 lines in its own history, polled from the log ring (which itself holds
fewer); only the rows on screen are laid out, so a long log does not slow the editor.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Read log lines with the same filters | Level, target and search boxes | `logs.read` |
| Find the links in a line | Links in a row | `editor.log_links` |
| Open a link | Click it | `editor.log_open_link` |
| Open the tab | Palette command `window.open_output_log` | `editor.layout_apply` |

## Keyboard

The tab handles no keys of its own. The filter boxes and the level drop-down take Tab focus.

## Known limits

- Agents cannot choose the code editor program: it comes from your preference, the project's agent
  settings or the environment, and it is never started through a shell.
- A link opened by an agent stays inside the project; a click in the tab may open any existing file.
- The tab has no clear button; it shows the engine's log, not a copy you can empty.

See also: [Profiler](profiler.md), [Editor guide](index.md).
