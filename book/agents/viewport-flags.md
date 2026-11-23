# Viewport Show flags and the navigation preview

ADR-7593. Code: `regia_editor::views::ShowFlags`, `viewport_agent` (`editor.set_viewport`), `volume_overlay`,
`nav_preview`. The user's view is [Editor](../user-guide/editor.md); the other helpers of the Show menu are on
[Create menu and viewport helpers](create-menu.md).

## The Show menu

Every entry of the viewport toolbar's **Show** menu is one flag of `editor.set_viewport`, which answers the full flag
set after every call (no params reads it). Game view hides every helper below.

| Show menu entry | `editor.set_viewport` flag | Draws | Default |
|---|---|---|---|
| Navigation | `navigation` | The world's `regia_nav::NavMesh`: polygon outlines and off-mesh links (ADR-7468) | off |
| Colliders | `colliders` | Every collider's outline (ADR-7217) | off |
| Light radii | `light_radii` | Every point light's radius and spot light's cone (ADR-7217) | off |
| Post volumes | `post_volumes` | Every post volume's shape and blend band (below) | off |

Each entry is registered with the agent-parity scan (`viewport.show_navigation`, `viewport.show_colliders`,
`viewport.show_light_radii`, `viewport.show_post_volumes`; `editor.actions { panel: "viewport.toolbar" }` lists them).

```json
{ "method": "editor.set_viewport", "params": { "post_volumes": true, "grid": false } }
```

## Show ▸ Post volumes

`editor.set_viewport { post_volumes: true }` outlines every enabled post volume: the shape in its priority color
(`regia_render::post_volume_gizmos::priority_color`, `priority.rem_euclid(6)`) and the outer edge of its blend band dimmed
by `BAND_DIM`. The segments are `regia_render::post_volume_outline`, the same data `render.post_volume_gizmos` returns and
the runtime's `post_volumes` debug overlay draws (ADR-2323). Unbound, disabled, newer-format and degenerate volumes draw
nothing, as in the runtime overlay; editor-internal entities draw nothing. At most 8,192 segments per pane. The live panel and
`editor.capture_viewport` / `editor.capture_viewport_instance` draw the same lines (a viewport instance uses its own
flags).

## `editor.nav_preview`

Show ▸ Navigation draws a `regia_nav::NavMesh` resource, which games insert at run time; in a stock editor there is none.
**Show ▸ Build navigation preview** (and **Clear navigation preview**) bake one in the editor.

| Param | Type | Meaning |
|---|---|---|
| `action` | string, optional | `build` bakes (replacing an earlier preview) and turns Show ▸ Navigation on; `cancel` stops a running bake; `clear` removes the preview (and cancels a running bake); omitted reads the state |
| `async` | boolean, optional | With `build`: `true` answers at once while the bake runs as a job; absent or `false` waits for it |

Result: `{ active, polys, triangles, settings?, cell_size?, agent_radius?, game_mesh, navigation, building, job? }`.
`building` is the running bake's job id (`null` when none runs); `job` is the id of the bake this call started. `settings` is
`level_streaming` when the level's `LevelStreaming.nav` (the settings the streaming cook bakes with) is set, otherwise
`default` (`regia_nav::NavMeshBuildParams::default()`). `game_mesh` is true when the world holds a `NavMesh` the preview
did not build. `navigation` is the main viewport's Show ▸ Navigation flag.

```json
{ "method": "editor.nav_preview", "params": { "action": "build" } }
```
```json
{ "active": true, "polys": 9, "triangles": 24, "settings": "default", "cell_size": 0.25, "agent_radius": 0.35,
  "game_mesh": false, "navigation": true, "building": null, "job": "nav_preview:4" }
```

What the bake reads: `regia_framework::nav_world::level_triangles_in`, the runtime nav world's and the streaming cook's
geometry source. That is static box, triangle-mesh and heightfield colliders on an entity with a static `RigidBody`,
terrain surfaces and attached destructible chunks. Sensors, dynamic and kinematic bodies, and colliders with no body are
not ground (physics skips a collider with no body in its ancestry, too).

What it never does: the preview is editor-only. `NavMesh` is not reflected, so no level save, prefab or snapshot carries
it (the level file's bytes are the same before and after a build), and the build is no undo step. A `NavMesh` a game
inserted is never replaced or removed.

The bake is an editor job (ADR-7664), `nav_preview:<n>` in `agent.job_status`, with the background task `nav_preview`.
The editor thread only runs the size check and copies the level's nav sources (terrain heights, static mesh collider
vertices, destructible chunk hulls: memory copies; ADR-8594). The geometry gather (the triangles, millions on a big
terrain) and the voxel build run on the job's worker thread, so the editor stays live, and the next frame after it ends
puts the mesh in place and turns Show ▸ Navigation on (the overlay draws it). The job's progress reads `reading the
level's collision (k of n)`, then `building the navigation mesh from N triangles`. One bake runs at a time: another `build` is refused with `nav_preview_busy`, not queued. While a bake
runs, the Show menu has **Cancel navigation preview**. A cancel (`action: "cancel"`, `editor.cancel_job { id }`,
`editor.cancel_task { id: "nav_preview" }`, `clear`, File ▸ Open or New level) ends the job `cancelled` at once and keeps
the preview that was there. The worker checks for the cancel before each source of the gather and before the build and
stops there; a build already running cannot stop part-way, so its worker finishes it in the background and its mesh is
dropped. Sources that give no triangle at all end the job `failed` with `nav_preview_no_geometry`. A bake whose level is no longer open when it ends is dropped too (the job ends `cancelled`). Without `async`
the method waits for the job and answers the state with `job`; a bake the builder refuses is `nav_preview_build_failed`
(with `async`, the job ends `failed` with that reason). The method needs only
the read permission, but it counts as a mutating call against the per-second rate limit
(`rate_limit_per_second` in `.regia/agent.toml`), reads included: a read-scoped session cannot queue back-to-back
whole-level bakes.

The preview belongs to the level it was baked from. After File ▸ Open, New level or a project switch it no longer
draws, the method reports `active: false` (and drops it), and Clear has nothing to clear. A level whose terrains and
static colliders together need more nav grid cells than one build allocates (64 Mi columns at the level's `cell_size`,
with the border) is refused with `nav_preview_too_large` before any geometry is read; raise `LevelStreaming.nav.cell_size`.
The estimate takes the XZ rectangle of every terrain and of every static box, triangle-mesh and heightfield collider
that feeds the bake (a box's corners, a mesh's vertices, a heightfield's height range; ADR-7664); destructible chunks
are left to the builder's own guard.

### Error codes

| Code | Method | Meaning |
|---|---|---|
| `nav_preview_no_geometry` | `editor.nav_preview` | `build`: the level has no static collision to bake |
| `nav_preview_game_mesh` | `editor.nav_preview` | `build` or `clear`: the world's `NavMesh` is a game's, not the preview's |
| `nav_preview_build_failed` | `editor.nav_preview` | `build`: the nav builder refused the geometry or the settings (no walkable surface, a grid too large) |
| `nav_preview_unknown_action` | `editor.nav_preview` | `action` is not `build`, `cancel` or `clear` |
| `nav_preview_too_large` | `editor.nav_preview` | `build`: the level's terrains and static colliders need more nav grid cells than one build allocates; raise the nav `cell_size` |
| `nav_preview_busy` | `editor.nav_preview` | `build` while a bake job runs (refused, not queued); wait for it or `cancel` it |
| `nav_preview_cancelled` | `editor.nav_preview` | a blocking `build` whose job was cancelled before it ended (the job's result carries it as `reason`) |
| `nav_preview_level_changed` | `editor.nav_preview` | a blocking `build` whose level was replaced before the bake ended (the job ends `cancelled` with this `reason`) |

## Gizmo and pane drags

Each press in a pane has one owner, decided at press time (ADR-7674, which replaces ADR-7593's see-through rule): a
primary press on a transform gizmo handle is the gizmo's; any other press is the pane's and reaches its tools (collider
and capture grips, spline control points, the procedural Path/Area/Paint/Box tools, terrain sculpt, 2D strokes in the
front pane, the Fracture click modes) and then box select, the camera and picking, with an entity selected or not and in
every pane of the quad layout. The viewport drives the gizmo crate without the 1x1 cursor widget that used to take every
press. Click picking reads the pointer click (ADR-8870). The tool drags keep their agent routes
(`editor.collider_handle_drag`, `editor.procgen_tool`, `editor.set_property` for a spline's points, `terrain.apply_stroke`,
`editor.tile_stroke`, `editor.fracture_panel_set` / `editor.fracture_test_hit`).

## Tests

- `cargo test -p regia_editor --lib nav_preview volume_overlay viewport` (`the_floor_bakes_and_clear_removes_only_the_preview`,
  `the_level_streaming_nav_settings_drive_the_bake`, `the_method_is_rate_limited_but_read_scoped`,
  `a_preview_of_another_level_is_dropped`, `reset_edit_state_drops_the_preview_but_never_a_game_mesh`,
  `a_terrain_too_large_for_the_grid_is_refused_before_the_gather`, `a_box_is_twelve_edges_and_its_band_twelve_more`,
  `a_box_select_that_starts_off_the_gizmo_handles_still_selects`, `a_camera_drag_over_the_gizmo_cursor_still_moves_the_camera`,
  `the_wheel_zooms_with_the_gizmo_shown`; the bake job of ADR-7664:
  `a_mesh_too_large_for_the_grid_is_refused_before_the_gather`, `nav_preview_build_names_its_job`,
  `the_bake_runs_as_a_job_and_the_overlay_updates_when_it_ends`, `a_cancel_keeps_the_old_preview`,
  `a_level_change_drops_the_running_bake`).
- `cargo test -p regia_editor --test pane_tool_press`: a press on each pane tool's handle moves only that handle and a
  press beside it reaches box select or the camera, with the transform gizmo shown (ADR-7674).
- `cargo test -p regia_editor --test viewport_flags_nav`: the nav preview's bake, refusals and unchanged level bytes,
  the toggles' round trips and, on a Vulkan adapter (lavapipe), the Navigation and Post volumes flags against a
  flag-off control capture (no golden).

## Project debug colors and remaining paths (W12)

`editor.set_viewport { "paths": true }` enables **Show ▸ Paths**. It draws each navigation
agent's current position through its remaining waypoints, omitting visited points and invalid
edges. Completed paths draw nothing. Work is capped per frame. The flag defaults off and follows
the other Show flags into play. Colliders, navigation and paths can be shown while the viewport
is possessed; Game view still suppresses editor debug overlays.

Project Settings ▸ Debug colors edits linear RGB channels for `collider`, `trigger`, `nav`,
`path`, `contact` and `sleeping_body`. Tab reaches each named channel; Up/Down change it; editing
works at the appearance UI scale. These colors are project preferences in the existing
`Regia.toml` `[editor.debug_colors]` table. Setting one key preserves unmentioned keys and other
manifest tables. An edit writes atomically and participates in `editor.undo`/`editor.redo`.
Runtime overlays read `regia_framework::debug_gizmos::DebugColors`, with no editor dependency.
Contact lines require the physics contact debug collector to be enabled.

- `editor.debug_colors { "op": "get" }` returns the palette (get is the default).
- `editor.debug_colors { "op": "set", "colors": { "path": [0.25, 0.5, 0.75] } }` saves an edit.

The result is `{ colors: { collider, trigger, nav, path, contact, sleeping_body },
space: "linear_rgb", path: "Regia.toml#editor.debug_colors", changed }`. Each color is three
finite numbers in 0..=1. This combined get/set method requires `edit_scene` permission.
No project is needed to read defaults; setting requires a readable project manifest.

Error reasons: `debug_colors_no_project` (open a project), `debug_colors_read` (repair the
manifest/table), `debug_colors_write` (check file access), `debug_colors_bad_value` (supply three
finite channels in range), `debug_colors_unknown_color` (use a listed key),
`debug_colors_unknown_op` (use get or set). Errors carry this guide as their hint.

Unset palette keys preserve existing editor overlay colors; the defaults returned by the method
are the runtime palette. Explicitly setting a key applies it to both views, even if it equals
that default. While play is running, agent changes to collider/navigation/path flags and Game
view reach the play world on the next editor frame (windowed or headless); later GUI changes in play remain effective.
The separate play window uses the same flags and palette.

## Show ▸ AI layers

ADR-8463. Seven more Show rows draw what each agent is doing in the world. They are
`editor.set_viewport` booleans, default false: `ai_bt`, `ai_state_tree`, `ai_goap`, `ai_squad`,
`ai_formation`, `ai_move_to`, and `ai_blackboard`. The menu ids are `viewport.show_ai_bt`,
`viewport.show_ai_state_tree`, `viewport.show_ai_goap`, `viewport.show_ai_squad`,
`viewport.show_ai_formation`, `viewport.show_ai_move_to`, and `viewport.show_ai_blackboard`.

Game view does not clear these seven. The other helpers in the table above are hidden in game
view; the AI rows stay as they were set, so a running game still shows the overlay. Turning any
row on enables the `ai_debug` gizmo group. Turning the last one off disables that group. Opening
the viewport with every row off does not change layers an agent already set with
`ai.debug_layers_set`. While the toolbar runs, the editor selection is the overlay's selected
entity. See [Seeing AI in play](ai.md).
