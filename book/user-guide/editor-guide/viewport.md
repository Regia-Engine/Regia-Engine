# Viewport

Panel id: `viewport`.

The **Viewport** tab shows the level as the game renders it, from an editor camera you fly or
orbit, in one pane or four (perspective plus top, front and side). You select, move, rotate and
scale entities here, frame them, keep camera bookmarks, and during play possess the game camera.
Depth: [Viewport tools (agents)](../../agents/viewport-tools.md), [Viewports
(agents)](../../agents/viewports.md), [Independent viewports](../viewports.md),
[Gizmos](../gizmos.md).

The keys below act on the active pane (the one last clicked) while no text field has focus. A
user shortcut bound to the same command (Preferences ▸ Keymap) replaces the built-in key.

## Camera

| Action | Mouse / key | Agent method |
|---|---|---|
| Orbit | Alt+left-drag; right-drag orbits at half speed | none (a mouse gesture; `editor.focus` and `editor.bookmark` place the camera) |
| Pan | Middle-drag (right- or middle-drag in an orthographic pane) | none |
| Zoom | Scroll, or Alt+right-drag | none |
| Fly | Hold the right button and press W / S / A / D (forward, back, left, right), E / Q (up, down); right button + scroll changes the fly speed | none (a held-key gesture) |
| Frame the selection | F (also **View ▸ Frame selection (F)**) | `editor.focus` |
| Store a camera bookmark | Ctrl+0 … Ctrl+9 | `editor.bookmark` |
| Recall a bookmark | 0 … 9 | `editor.bookmark` |

## Selection and gizmos

| Action | Mouse / key | Agent method |
|---|---|---|
| Select | Click an entity; Shift+click adds, Ctrl+click toggles | `editor.select`, `editor.pick` |
| Box select | Left-drag on empty space; Shift adds, Ctrl+Shift removes, Ctrl toggles | `editor.box_select` |
| Lasso select | Hold L while left-dragging (or run **Lasso select** from the palette to make every drag a lasso); same modifiers as the box | `editor.lasso_select` |
| Select similar | **Edit ▸ Select ▸ All of the same kind / with the same components / using the same asset / in the same layer**, or the Outliner row menu's **Select similar** | `editor.select_similar` |
| Pick from overlapping objects | Alt+click lists everything under the cursor, nearest first; Up / Down / Enter pick, Escape closes (palette: **List the objects under the cursor**) | `editor.pick_stack` |
| Select a group by clicking a part | Outliner row menu ▸ **Selection root**; Ctrl+click still picks the part | `editor.selection_root` |
| Hover highlight | Rest the pointer on an entity: a thin outline shows what a click selects | none (pointer only) |
| Place an asset | Drag it from the Content Browser: a wire box shows where it lands; release places it, Escape cancels | `editor.drop_preview` |
| Measure | M, then drag for a distance, click again for the angle; hold V to snap to vertices; Escape leaves | `editor.measure` |
| Move gizmo | W | `editor.set_gizmo {"mode":"translate"}` |
| Rotate gizmo | E | `editor.set_gizmo {"mode":"rotate"}` |
| Scale gizmo | R | `editor.set_gizmo {"mode":"scale"}` |
| Snap while dragging | Hold Ctrl | `editor.set_gizmo {"snap":true}` |
| Snap to a vertex | Hold V while dragging | `editor.vertex_snap` |
| Drop the selection to the floor | End | `editor.drop_to_floor` |
| Collider handles | C (bare C; Ctrl+C belongs to copy), or **Tools ▸ Collider handles (C)** | `editor.collider_handles` (reads or sets the tool), `editor.collider_handle_drag` |
| Align new placements to the surface | **Tools ▸ Align to surface** | `editor.placement_options` |
| Move an entity exactly | the gizmo, or type in the Inspector | `editor.set_transform` |

## Toolbar and overlays

| Action | Control / key | Agent method |
|---|---|---|
| Four views | **4 views** | `editor.set_viewport` (layout) |
| Game view (hide editor overlays) | **Game view (G)**, G | `editor.set_viewport` (`game_view`) |
| Render quality of the pane | **Quality: …** | `editor.set_viewport` (quality) |
| Show grid, icons, bounds, outline, proxies, audio ranges, stats | **Show** menu; **View ▸ Grid**, **Selection labels**, **Realtime viewport** | `editor.set_viewport` (show flags), palette commands `view.toggle_grid`, `view.toggle_labels`, `view.toggle_realtime`, `view.toggle_audio_gizmos` |
| Draw the baked navigation mesh | **Show ▸ Navigation** | `editor.set_viewport` (`navigation`) ([Create menu and helpers](../../agents/create-menu.md)) |
| Place a player start, trigger volume, light, camera, audio emitter, decal, post-process volume, spline, nav link or smart object at the focus point | **Create ▸ Player start**, **Trigger volume**, **Point light**, **Spot light**, **Directional light**, **Camera**, **Reflection capture**, **Audio emitter**, **Rect light**, **Tube light**, **Decal**, **Post-process volume**, **Spline**, **Nav link**, **Smart object** (and **Create ▸ Shape**) | `editor.create_entity` ([Create menu and helpers](../../agents/create-menu.md)) |
| Possess the game camera during play | **Possess (F8)**, F8 | `editor.set_viewport` (`possessed`) |
| Debug views and overlays | none in the toolbar | `editor.set_render_override`, `editor.set_scalar_overlay` |
| Look at a capture | none | `editor.capture_viewport`, `editor.viewport_stats` |

The **Icons** flag also draws a player start's flag and a facing arrow on the floor, and every
trigger volume (a sensor collider) as a dashed pink outline (the **Colliders** overlay draws
sensors solid instead).

## Terrain and 2D modes

With the terrain tool on, the viewport paints the heightfield:

| Action | Key | Agent method |
|---|---|---|
| Terrain tool on or off | T (pointer over the viewport) | `terrain.stroke` paints without the tool |
| Brush size smaller / larger | `[` / `]` | `terrain.stroke` `radius` |
| Strength down / up | `-` / `=` | `terrain.stroke` `strength` |
| Brush mode | 0 … 9 | `terrain.stroke` `mode` |
| Previous / next paint layer | `,` / `.` | `terrain.stroke` `layer` |
| Stroke; inverted stroke | left-drag; Shift+left-drag | `terrain.stroke`, `editor.terrain_scatter_brush` |

2D mode ([2D](../2d.md), [Editor 2D mode](../../agents/editor-2d.md)): B turns it on and off;
`,` / `.` step the tile layer and `[` / `]` the terrain set; Shift erases while painting. Agent
methods `editor.mode2d_set`, `editor.mode2d_get`, `editor.tile_stroke`, `editor.tile_stamp`,
`editor.tilemap_save`.

## Keyboard and accessibility notes

Every viewport action with a key above is reachable without the mouse except picking a single
entity, box select and the camera's mouse gestures: select from the [Outliner](outliner.md)
instead (its rows take keyboard focus), and frame with F. Findings and follow-ups are in
[Editor accessibility](accessibility.md).
