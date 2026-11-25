# World Settings

Panel id: `world_settings`.

The **World Settings** tab edits the open level's `LevelSettings`: gravity, the environment asset,
the game rules, the ambient audio bed, streaming, the atmosphere and sky, the look (exposure, color
grade), the audio medium, the terrain and the baked light a level names. The settings are saved with
the level, so an edit marks the level as changed. The fields are drawn by the same reflection
inspector as the [Inspector](inspector.md). Depth: [Viewport tools](../../agents/viewport-tools.md)
(the `editor.world_settings_*` methods).

## Open it

The tab is not in the default layout. **Edit ▸ World settings…** opens it as a tab; so does the
command palette entry **World settings** (Ctrl+Shift+P, then type `world`). An agent opens it with
`editor.world_settings_panel`, or `editor.layout_apply {"open": "world_settings"}`.

## Change a setting

1. Open a level. With no level the tab says "Open a level to edit its settings."
2. Edit a field. The header shows the level's path (or `(unsaved level)`). A change takes effect at
   once for what reads the settings every frame (gravity in play, the look and the atmosphere in the
   renderer); what a level load applies (the game rules, the baked GI volume, the lightmap) takes
   effect the next time the level opens.
3. The viewport's `editor` quality preset keeps its own sky and exposure, so a look change may not
   show there.
4. If a value is refused (gravity that is not finite, cloud coverage outside 0 to 1, an hour outside
   0 to 24, a day outside 1 to 365, a latitude outside -90 to 90, an exposure bias that is not
   finite) the message appears next to the **Reset to defaults** button and nothing changes.
5. **Reset to defaults** puts every setting back to the engine default.
6. Save the level to keep the change.

Each edit is one undo step; scrubbing a slider on one field merges into one step.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open the tab | **Edit ▸ World settings…**, palette command `window.world_settings` | `editor.world_settings_panel` |
| Read the settings | The tab | `editor.world_settings_get` |
| Change settings | The fields | `editor.world_settings_set` (`settings`, or `reset: true`) |
| Reset to defaults | **Reset to defaults** | `editor.world_settings_set` with `reset: true` |
| Undo, redo | Ctrl+Z, Ctrl+Y | `editor.undo`, `editor.redo` |

## Keyboard

The tab has no keys of its own beyond the editor-wide ones: Ctrl+Z and Ctrl+Y undo and redo, and the
fields take Tab focus like the Inspector's.

## Known limits

- It needs an open level.
- The tab is not part of the default or reset layout; reopen it from the Edit menu.

See also: [Viewport](viewport.md), [Baked lighting panels](bake.md), [Editor guide](index.md).
