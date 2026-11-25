# Kit

Panel id: `kit`.

The **Kit** tab shows an exported content kit: a folder with a `kit.json` manifest and a model
(`<kit_id>.glb`), optionally with a cockpit model, a rig and sockets list, and a `SHA256SUMS`
file. It lays out the kit's metadata, checksum result, warnings, clips, sockets and budget next to
a render of the model you can orbit and switch between LOD levels. It is a viewer: it never
writes or repackages a kit. The kit folder layout and what the viewer refuses are on [Open
kit](../open-kit.md); the agent call is on [Open kit (agents)](../../agents/open-kit.md).

## Open it

- **File ▸ Open kit…** (also **Open kit…** on the project browser, which needs no project) asks
  for a folder and opens it. The Kit tab comes forward, even if you had closed it.
- Inside the tab, **Open kit…** does the same, and **Reload** (shown once a kit is open) re-reads
  the open folder.
- An agent calls `editor.open_kit {"path": "..."}` or `editor.layout_apply {"open": "kit"}`.
  The command palette has no `open` command for the tab.

## Inspect a kit

1. Pick the kit folder. With nothing open the tab says to open a kit folder with `kit.json`,
   `<kit_id>.glb` and optionally a cockpit, `rig.json`, `sockets.json` and `SHA256SUMS`. If the kit
   cannot open, the tab shows "Kit not opened:" with the reason in red.
2. The left column lists the **manifest** fields (including whether it is a character kit), then
   the `SHA256SUMS` result: green **verified**, red when a file differs, is missing or was refused,
   or gray when there is no list. Expand **Checksums** for each file's result.
3. **Warnings** lists everything that did not stop the kit from opening, one line each.
4. **Budget** measures the model against the limits of a class; the class picker next to it
   changes the class, and **Budget meters** expands one meter per limit.
5. Under **Clips**, a clip from the model plays in the Clip tab when you click it (the model must
   be skinned; the tab then uses it as the preview rig). A clip listed only in `rig.json` is
   disabled and marked "rig.json only".
6. Under **Sockets**, tick **Socket gizmos** to draw the sockets over the render, and click a
   socket to highlight it (click again to clear).
7. The right column is the render. Drag it to orbit the model. The buttons above it switch
   between LOD levels, with each level's triangle count and "generated" when the viewer built
   the level rather than the model supplying it. A line below says where the cockpit is drawn:
   at `socket_cockpit`, or at the origin when the model has no such node.

| Action | Control / key | Agent method |
|---|---|---|
| Open a kit | **File ▸ Open kit…**, **Open kit…** | `editor.open_kit` |
| Re-read the open kit | **Reload** | `editor.open_kit` with the same path |
| Play a clip | click a clip | `editor.open_kit` with `clip` |
| Render a LOD | the LOD buttons | `editor.open_kit` with `render: true` and `lod` |
| Read the budget | **Budget** | `editor.budget` with `source: "kit"` |
| Show the tab | the tab | `editor.layout_apply` with `open: "kit"` |

## Keyboard

The tab has no shortcuts. Its buttons, LOD selectors and list rows take focus with Tab. The
render responds to the mouse only (drag to orbit); an agent gets a rendered frame from
`editor.open_kit` with `render: true`.

## Known limits

- The folder dialog is a native dialog; an agent passes the path instead.
- Without a GPU device the tab says "No GPU device: the render is off." and still shows the
  information columns.
- The viewer does not check a kit against a content contract (scale, budget tables, node and clip
  names) and does not write kits.
- A kit that cannot be read safely (an unsafe path, a file over its size cap, a model without
  embedded data) does not open and the tab says why; every other problem is a warning.

See also: [Content browser](content-browser.md), [Editor budget
(agents)](../../agents/editor-budget.md).
