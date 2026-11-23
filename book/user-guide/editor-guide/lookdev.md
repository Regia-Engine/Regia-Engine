# Look-dev

The **Look-dev** tab traces the open level with the reference path tracer and shows the result: the
ground truth a real-time frame can be compared against. The trace takes the same inputs the
real-time path uses (the runtime extraction of the level), with the editor camera or the level's own
game camera as the view. Editor-only helpers (the preview sun, proxy cubes, the backdrop) are not
traced, so a level with no sun of its own traces dark where the viewport shows the preview sun. Each
trace runs on its own thread with its own GPU device, so the editor keeps drawing and a device loss
in the trace does not reach the editor. Depth: [Look-dev panel (agents)](../../agents/editor-lookdev.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
An agent brings it forward with `editor.layout_apply {"open": "lookdev"}`. There is no palette
command for this tab.

## Trace a level

1. Open **Look-dev**. Choose a **Scene**: `viewport` (the open level through the editor camera, the
   default), `game` (through the level's game camera), or `cornell` and `furnace` (the tracer's own
   test scenes, which need no level and show that the tracer runs on this machine).
2. Set the image **Size**, a **Preset** (`none`, `draft`, `preview`, `final`, `reference`), the
   **Samples / pixel**, **Max bounces** and **Seed**. A preset uses its own sample count and path
   length unless you changed those fields.
3. Leave **Output dir** empty for the default under the project's `.regia/lookdev/`, or type a folder.
4. Press **Trace**. A bar counts samples per pixel; **Cancel** stops the trace.
5. When it ends the tab shows the sample count and time, the files it wrote (`exr`, `preview_png`,
   `metadata_json`; click a path to copy it) and the preview image.

A failure shows its code and message in red; one trace runs at a time.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Start a trace | **Trace** | `editor.lookdev_trace` |
| Read progress, files and the result | The progress bar and file list | `editor.lookdev_status` |
| Stop a trace | **Cancel** | `editor.lookdev_cancel` |

## Keyboard

The tab handles no keys of its own; reach its controls with Tab and press them with Enter or Space.

## Known limits

- The trace shows the level as a game would render it, not the editor viewport with its helpers.
- A preset that denoises writes a separate `preview.png`, which is never a reference.
- Trace outputs are the layered `reference.exr`, its AgX preview and a metadata file; compare them
  with a real-time frame with `seeing.compare` ([Renderer excellence](../../agents/renderer-excellence.md)).

See also: [Baked lighting panels](bake.md), [Look-dev harness](../lookdev.md),
[Editor guide](index.md).
