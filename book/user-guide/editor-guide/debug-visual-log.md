# Selected Object Debug and Visual Log

Two debugging tabs for gameplay. **Selected Object Debug** shows what the selected object's gameplay systems are doing
right now: perception, the state-tree path, smart-object claims, navigation waypoints, abilities and effects, as text
in the tab and as a label in the viewport. **Visual Log** replays the lines and text labels a game recorded per
entity, one frame at a time, so you can scrub back to the frame where something went wrong. Both only change what is
drawn: they never edit the level and add nothing to the undo history. Depth: [Selected-object debug overlay and visual
log (agents)](../../agents/visual-log.md).

## Open them

Press Ctrl+Shift+P and pick **Open the Selected Object Debug tab** or **Open the Visual Log tab** (commands
`window.open_selected_debug`, `window.open_visual_log`; bind a key to either in the keymap). An agent brings a tab
forward with `editor.layout_apply {"open": "selected_debug"}` or `{"open": "visual_log"}`.

## Selected Object Debug

1. Check **Show selected object debug data**. Unchecked, the overlay does no gameplay queries at all.
2. Select an object in the viewport or the Outliner; the tab follows the selection the viewport and Outliner share.
   With nothing selected it says "Select an object in the viewport or Outliner."
3. The tab names the world it reads (the edit world, or the play world while playing) and the entity, then one
   line per piece of data: perception cones and hearing radius, perceived targets, the active state-tree path,
   smart-object claims, the remaining navigation waypoints, running abilities and applied effects. Each list is
   capped at 128 rows. The same text is drawn as a label next to the object in the viewport.
4. If the selected entity is gone (for example, despawned during play), the tab says why instead of showing data.

## Visual Log

A game records shapes with the `debug_log!` macro of `regia_framework` (the `visual-log` feature);
play-in-editor installs the recorder for you. The recorder keeps a ring of recent frames.

1. The first line names the world the recording comes from. With nothing recorded the tab says "No recorded frames.
   Start play with visual logging enabled."
2. Drag **Recorded frame** to pick a frame. The viewport draws exactly that frame's shapes, also while play is paused;
   scrubbing does not rewind the game. A frame that has fallen out of the ring draws nothing ("This frame is not
   retained").
3. Click an **Entity N** row to show or hide that entity's shapes. Entities that are gone keep their recorded rows.
4. Under the list, each shape of the frame is listed: a text label as "entity: text", a line with its two ends.

The tab keeps its last recording after play stops. A closed tab does no work.

## Actions and agent methods

| Action | Control / key | Agent method |
|---|---|---|
| Turn the overlay on or off, pick the entity | **Show selected object debug data** (Tab to it, Space) | `editor.debug_overlay {"on", "entity"}` |
| Read the overlay's text | The tab | `editor.debug_overlay {}` |
| Scrub to a frame | **Recorded frame** (Tab to it, Left and Right) | `editor.visual_log {"frame"}` |
| Show an entity's shapes | **Entity N** (Tab to it, Space) | `editor.visual_log {"entities"}` |
| Read the recorded shapes | The tab | `editor.visual_log {}` |
