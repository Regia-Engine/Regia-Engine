# Tools

The **Tools** tab runs **editor tools**: separate programs the editor launches from a small
manifest file, such as a review queue, a publishing calendar or an asset checker. Each tool shows
its status, progress bars, a small view it describes itself, its log and the methods it provides.
A tool runs in its own process, in any language, and talks to the editor over a local WebSocket
with a token that can call only what its manifest allows. Depth: [Editor tools
(agents)](../../agents/editor-tools.md), [Tools acceptance](../../agents/tools-acceptance.md).

The **Tools** menu in the menu bar is a different thing: it holds editor commands such as
rebuilding the asset database. This page is about the **Tools** tab.

## Open it

The tab is part of the default layout (the group under the viewport); **View ▸ Reset layout**
brings it back when closed. An agent uses `editor.layout_apply {"open": "tools"}` or
`editor.tool_panel`, which also brings the tab forward when it was closed and can select a tool.
The command palette has no `open` command for it.

## Add and run a tool

1. Put a manifest named `<id>.tool.toml` (the tool's id, then `.tool.toml`) in `<project>/.regia/tools/`, in the editor's user
   `tools` folder, or in a folder listed in `REGIA_TOOL_PATH`. The first manifest of an id wins. The
   manifest names the program to launch, the permissions and methods its token may use, the methods
   it provides, and an optional web panel address.
2. Open the tab and press **Refresh** if the tool was added while the editor ran. The header says
   how many manifests were found. A manifest that does not parse shows as `<id> (invalid)` and
   its error appears when selected; it never stops the editor.
3. Click a tool in the left list. The dot is green while it runs, yellow while it stops, red when
   it failed and gray otherwise, followed by the status name.
4. Press **Launch**. The editor starts the program with a fresh scoped token. **Stop** asks it to
   shut down; if it does not, **Kill** appears. **Open in browser** shows when the manifest has a
   panel address.
5. While it runs the right column shows the tool's tasks as progress bars, then the view the tool
   set (headings, labels, values, progress bars, links and buttons). Buttons are enabled while
   the tool runs; pressing one sends a click event to the tool.
6. Open **Log** to read the tool's last output lines and **Methods** to see the methods it provides,
   each with its permission.
7. A tool whose manifest sets `autostart` launches when a project opens only if its manifest is in
   the user folder or a `REGIA_TOOL_PATH` folder: opening a project never runs a program by
   itself.

| Action | Control / key | Agent method |
|---|---|---|
| List manifests and refresh | **Refresh** | `editor.tool_list` (with `refresh: true`) |
| Read a tool's status, tasks, view and log | the right column | `editor.tool_status` |
| Read the methods a tool provides | **Methods** | `editor.tool_methods` |
| Launch a tool | **Launch** | `editor.tool_launch` |
| Stop or kill a tool | **Stop**, **Kill** | `editor.tool_stop` |
| Press a view button | the view's buttons | `editor.tool_click` |
| Call a method a tool provides | none | `editor.tool_call` |
| Bring the tab forward, select a tool | the tab | `editor.tool_panel` |

## Keyboard

The tab has no shortcuts. The tool list, buttons and hyperlinks take focus with Tab.

## Known limits

- A tool's own token never has the `tools` permission and is revoked when the tool exits.
- Unknown keys in a manifest are errors, not ignored settings.
- The tab shows what a tool describes with its own view items; it does not embed a web page, only
  the **Open in browser** link.
- Only the program the editor started is stopped; a tool that starts children should end them when
  its connection drops.

See also: [Editor guide](index.md).
