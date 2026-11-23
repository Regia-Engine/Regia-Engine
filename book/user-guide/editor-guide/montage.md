# Montage

The **Montage** tab is the timeline of a `.montage.ron` (or a child `.montagevar.ron`): one lane
per slot track with its clip segments, then lanes for sections, notifies and warp windows, a
playhead, and a pose preview of one section chain or the whole timeline. Every control writes the
same validated, undoable edit an agent sends, so the file is saved by each edit. Depth:
[Editor data panels](../../agents/editor-data-panels.md) (the Montage section), and for the runtime
[Montages, sync groups and motion warping](../montages.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
An agent brings it forward with `editor.layout_apply {"open": "montage"}`. There is no palette
command for this tab.

## Edit a montage

1. Type a project-relative path such as `assets/anim/combo.montage.ron` in the path field and press
   **Open**. The lanes appear; a message under the path field shows the code of a refused edit.
2. Press **Preview on selected rig** with a skinned entity selected to see the pose at the
   playhead in the viewport (the status reads `posed` or `bind pose`). Drag the **Playhead**
   slider, or click or drag the ruler. **Whole timeline** previews every section in order instead
   of one chain; the `plays:` line lists the chain.
3. Add a clip: type its name in the **clip** field and a slot in the **slot** field (a slot no
   track plays starts a new track; empty means the first track), then press **Add to** the slot.
   You can also drop a clip from the Content browser on a track lane: it joins that track at its end.
4. Click a segment to edit its **start**, **rate** and **loops**, or use **◀** and **▶** to
   reorder and **Remove** to delete it.
5. **Section at playhead** adds a section; drag a section marker to move it. With a section
   selected, **Preview from here**, **Jump to** (the target typed beside it), **Continue**,
   **Stop at end**, **Set follow** and **Clear follow** edit how it links. Select a notify or warp
   to **Remove** it.
6. Type a name and press **Add notify at playhead** to add a notify. In the warp row, a target,
   a window and the **translation**, **vertical** and **rotation** checkboxes with **Add warp** add a
   motion-warping window.
7. The blend row (**in**, **out**, **lead**, **auto blend-out**, **Apply blend**) sets the blend times.
8. **Slot table** (a collapsing section) loads a skeleton's `.slots.ron` with **Load slots** and
   adds, renames, deletes (**force delete**) and sets slots (group, joints, **with children**).
9. **Undo** (shown once a montage is open) reverts the last montage edit.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open a montage | Path field, **Open** | `editor.montage_panel_open` |
| Read the timeline, selection and preview | The tab | `editor.montage_panel_get` |
| Any edit above | The controls | `editor.montage_panel_edit` (`anim.montage_edit`) |
| Move the playhead, pick the chain or the rig | **Playhead**, **Whole timeline**, **Preview on selected rig** | `editor.montage_panel_preview` |
| Select a segment, section, notify or warp | Click on the timeline | `editor.montage_panel_select` |
| Slot table edits | **Slot table** | `anim.montage_slot_edit`, `anim.montage_slots` |

## Keyboard

The tab handles no keys of its own; its controls are reached with Tab and pressed with Enter or
Space. To undo a montage edit use **Undo** in the tab (or `{ "op": "undo" }` through
`editor.montage_panel_edit`); the tab's code has no Ctrl+Z handler of its own.

## Known limits

- A segment that plays to the end of a clip no loaded library knows is drawn one second long with
  an amber outline.
- The pose preview samples the first track only; other tracks' slot masks are not applied.
- A child `.montagevar.ron` cannot be previewed (the preview answers
  `montage_panel_preview_variant`).

See also: [Animation editor](anim-graph.md), [Editor guide](index.md).
