# Editor surface (M10 shell, M11 workflows)

The M10 editor shell is agent-controllable through the same protocol as the runtime: the
`editor.*` methods below are registered by the editor as an extension of the shared method
table, so they go through the same permission checks, rate limit, audit log and provenance
records as `scene.*` tools. Everything a human does in the panels, an agent can do (and undo)
through calls.

## Connection

Start the editor with a project directory:

```
regia-editor --project <project-dir>
```

The endpoint and token land in `<project>/.regia/agent/` as with any Regia Engine app; connect
with `regia agent call <method> [JSON] --project <project-dir>`.

## Methods

| Method | What it does |
|---|---|
| `editor.select { entities: [ids], mode: "set"\|"add"\|"toggle" }` | Replaces or extends the editor selection (the same state the panels show). Every element must be an entity id (`F0002` otherwise); `[]` with `set` clears. |
| `editor.get_selection` | The current selection with entity names and the primary entity. |
| `editor.set_gizmo { mode: "translate"\|"rotate"\|"scale", snap?: bool }` | Drives the viewport gizmo mode and snapping. |
| `editor.set_viewport { grid?, labels?, realtime?, icons?, bounds?, outline?, proxies?, stats?, preview_light?, colliders?, light_radii?, navigation?, post_volumes?, audio?, game_view?, possessed?, quality?, layout?, overlay_blur?, guides? }` | Viewport display: show flags (`colliders` and `light_radii` are the debug overlays of [Seeing](seeing.md); `navigation` draws the baked nav mesh, [Create menu and helpers](create-menu.md); `post_volumes` outlines every post volume, [Viewport Show flags](viewport-flags.md)), `overlay_blur` (frosted backing under the overlay chips: Gaussian sigma in points, 0 to 64, 0 = off, the default), the quality preset (`editor`, `game`, `mobile_low`, `mobile_high`, `desktop_low`, `desktop_high`, `desktop_ultra`, `deck`), `layout` `single`/`quad` (perspective + top/front/side), game view (hides every helper), `audio` (emitter markers; the selection's ranges, cones and reverb zones) and play-mode possession. `guides` (`{ thirds, center, safe_areas, letterbox }`) sets the composition guides over the perspective pane ([Viewport tools](viewport-tools.md#pilot-look-through-camera-preview-screenshots-and-guides)). `realtime: false` idles the editor at low CPU. Camera navigation fields (ADR-7755): `invert`, `speed`, `speed_step`, `speed_with_distance`, `orbit_selection`, `zoom_to_cursor`, `navigation_preset`, `fov`, `near`, `far`, and the `layout` names `side_by_side`, `stacked`, `one_large_two`, see [Viewport tools: camera navigation](viewport-tools.md). |
| `editor.capture_viewport { width?, height?, view?, quality?, game_view?, warm_frames?, debug_view?, tile_m?, target_px_per_m?, tolerance?, client? }` | `client` (a 0-based play-in-editor client index, `editor.pie_net_clients`) captures that client's world. Renders a pane headless through the runtime frame path (default 320×240, clamped to 64..=16384; a size past the device's texture limit answers `screenshot_needs_tiles`, tiled capture is deferred (ADR-7768); a width or height that is not an integer in range falls back to the default; `view` = `perspective` / `top` / `front` / `side`; `quality` for this call only; `tile_m` = `uv_checker` cell edge in meters of surface, default 0.25, 0 or less lays it out in UV space; `target_px_per_m` (default 512) and `tolerance` (default 0.1) tune `texel_density`) with the overlays painted in, and returns `png_base64`, the measurements (sky luminance and blue bias, lit peak, shading range, `selection_highlight` = the selection outline is on screen, grid pixels, gizmo axes), `taau`, `profile`, `quality`, and `info` (meshes, proxies, lights, preview sun, possessed, draws: drawn/culled and opaque meshes drawn `instanced` vs `per_object`, with `double_sided_per_object` (double-sided meshes off the instanced path: should be 0 on a GPU-driven device), GPU ms per pass, CPU ms, frame-path counters). |
| `editor.set_reference { view?, png_base64?, path?, mode?, opacity?, rect_uv? }` / `editor.get_reference { view? }` / `editor.clear_reference { view? }` | A reference image (concept ghost) in a pane, drawn behind / over / as a difference / as an outline in the panel and in captures; `capture_viewport` echoes it in `reference`. See [Reference image](reference-image.md). |
| `editor.pick { x, y, width?, height?, view?, select?, cpu? }` | The entity under a capture pixel from the GPU entity-id buffer (pixel-exact on rendered meshes; mesh parts map to their entity) or the terrain heightfield; without a GPU (a headless editor) or with `cpu: true`, a ray against each drawn mesh's BVH and the terrain (ADR-7204). Locked entities are never returned. `{ entity, name, position, terrain, method }` (`method`: `gpu` or `cpu`) or `{ entity: null, method }`. |
| `editor.thumbnails { jobs, size? }` | Batch thumbnails: up to 256 small square renders of models (`path`) or entities (`entity`) on one warm renderer, each with `view` / `yaw` / `pitch`, `size` and `debug` (`lit` / `wireframe`); returns `images[]` (PNG) and `stats` (pipelines created: 0 once warm). See [Batch thumbnails](thumbnails.md). |
| `editor.pick_hit { x, y, width?, height?, view?, cull? }` | The surface under a capture pixel: mesh `entity` (and the `owner` a click selects), `triangle`, `indices`, `barycentric`, world `position`, `normal` (front side), `smooth_normal`, `distance`, `front_face`; skinned meshes where they are drawn; no terrain hit, but `null` where terrain or a proxy cube hides the mesh. See [Render overrides and surface picks](render-override-pick.md). |
| `editor.set_render_override { entity, tint?, emissive?, alpha?, xray?, unlit?, clear? }` | Per-entity tint, glow, ghost (alpha, drawn blended), x-ray (through occluders) and unlit, inherited by children and model parts; no material clone; not undoable. |
| `editor.box_select { x0, y0, x1, y1, width?, height?, view?, mode? }` | Selects every entity with a visible pixel inside the rectangle (`mode` `set` / `add` / `subtract` / `toggle`; the freehand lasso is `editor.lasso_select`, see [Viewport tools](viewport-tools.md#selection-and-placement-tools)). |
| `editor.set_scalar_overlay { entity, values? \| constant? \| axis?, paint?, ramp?, range?, opacity?, clear? }` | Colors a mesh entity by one number per vertex through a ramp, unlit over the lit viewport (skin-weight and deformation heatmaps, damage brushes). A values count that differs from the vertex count draws nothing and reports `fit`. See [Scalar overlay](scalar-overlay.md). |
| `editor.focus { entities? }` | Frames entities (default: the selection) by their bounds in the perspective camera. |
| `editor.bookmark { action?, slot? }` | Camera bookmarks 0–9: `set`, `recall`, `list` (the panel: Ctrl+digit stores, Shift+digit recalls; bare 1-4 are the view presets, ADR-7204). |
| `editor.viewport_stats {}` | fps, median frame time, hitch count (frames > 2× the median and > 1/30 s, or pipeline creation after warm-up), the last pane's draws, GPU ms per pass and counters, and a `pane` block: `setting` (`gpu`, `cpu`, or `none` when no pane textures are installed), `path` (what the last picture used), `pictures_per_second` (all panes together), `pictures`, and `textures` (`live`, `registered`, `freed`, `rebound`, `last_size`). The on-screen stats overlay shows the same path and rate. |
| `editor.viewport_pane_path { path? }` | How the live panes reach egui: `gpu` (default in the windowed editor; the renderer's texture is registered as a native egui texture, only the id buffer is read back for the grid, outline and picking) or `cpu` (read back and uploaded every frame; also `REGIA_EDITOR_PANE_PATH=cpu` at startup). Without `path` it reports `{ path, live }`. Fails with `not_found` (message `pane_textures_unavailable`) in a headless world: captures, `editor.capture_viewport` and screenshots always read back and are unaffected. A pane with a reference image reads back too. |
| `editor.viewport_instances` / `editor.viewport_instance_set` / `editor.viewport_link` / `editor.capture_viewport_instance` / `editor.viewport_instance_remove` | Independent viewports (a lightbox, before/after, compare panes): list, edit, link cameras, capture and remove `ViewportInstance`s. See [Independent viewports](viewports.md). |
| `editor.asset_validate { set?, source?, validators?, config?, list?, include_ron?, … }` / `editor.asset_graph { set?, source?, asset?, … }` | Run the asset validators over an asset set given as RON (pass / fail / not_validated rows with messages) and answer dependency, reverse-lookup and cooked-size questions about it. `set` is required unless `list` is true or `source` is `"registry"` (the editor's loaded asset registry plus every `.ui.ron` under the open project's `assets/`; `registry_not_loaded` when none is loaded). An inline `set` reads nothing from disk. See [Asset validators](asset-validators.md). |
| `editor.view_asset { path, width?, height?, lod?, yaw?, pitch?, mip?, channel?, overlay? }` | The asset viewer as a PNG + `info`: a mesh (parts, materials, LOD chain with triangle counts), a texture (mip chain, channel `rgba`/`r`/`g`/`b`/`a`) or a WAV (waveform, rate, channels, seconds). `builtin:cube` / `plane` / `sphere` work as paths. Mesh overlays: `overlay: "uv" \| "uv1" \| "normals" \| "collision"` (or a list; [Import settings panel and mesh viewer overlays](import-settings.md), ADR-7469). |
| `editor.open_kit { path, clip?, render?, lod?, width?, height?, yaw?, pitch?, sockets? }` | Opens a Tincal kit folder in the Kit panel and returns its `kit.json` metadata, LOD chain, clips, sockets, `SHA256SUMS` result and warnings; `render` adds a PNG with the cockpit at `socket_cockpit`. See [Open kit](open-kit.md). |
| `editor.play_sound { path }` | Plays a WAV through the editor's audio output (offline mixer without a device). |
| `editor.set_transform { entity, translation?, rotation?, scale? }` | The gizmo's edit as a call — one undo entry per call (a gizmo drag is one entry until the mouse button is released). A number outside the `f32` range (`1e39`) is refused with `F0002` and nothing changes. |
| `editor.set_property { entity, component, path?, value, dry_run? }` | The Inspector's edit as a call: one undoable `SetProperty`. `path` is reflect syntax into nested structs, lists, arrays, tuples and enum variants (`"waypoints[1].speed"`, `"mode.0.radius"` for a field inside `Some(..)`); a whole list/map/set value replaces it exactly. A struct or variant value must give every field (no default filling): a missing one is an `invalid_params` error naming its path. Returns `before`/`after` JSON. |
| `editor.undo` / `editor.redo` | Step the editor's undo stack (every panel edit and every undoable `editor.*` edit lands there; `editor.set_render_override` does not); returns what was undone or redone and whether the level is dirty. |
| `editor.save_level { path? }` | Saves the open level (canonical `.level.ron`, atomic write), plus the 2D mode's map when it has unsaved edits (`tilemap_saved`). Refused while a prefab is open in place (reason `prefab_in_place`) or on its own (reason `prefab_open`): close the prefab first; nothing is written and the prefab stays dirty. |
| `editor.play { separate_processes? }` / `editor.pause` / `editor.resume` / `editor.stop` / `editor.step { frames? }` | Queue play-in-editor (`separate_processes` first sets the Net tab's option: with a listen or dedicated mode the server and clients run as processes, see `editor.pie_processes`). The editor applies the queue on its next frame; poll `editor.play_state`. Stop leaves the edit world untouched. |
| `editor.play_state` | `{ mode, frames, entities, start_elapsed_ms, game, panic }`. |
| `editor.import { sources, folder?, async? }` | Import glTF/PNG/audio through the pipeline into the dev cache. `async: true` runs it as a background job that `editor.cancel_job` stops ([Play-in-editor modes and editor jobs](pie-modes.md)). |
| `editor.create_asset { kind, path }` | Create an asset under `assets/` as the Content Browser's New menu does: every kind is listed in [Editor creators](editor-creators.md) and by `editor.asset_kinds`. |
| `editor.open_asset { path }` | Open an asset in its editor panel as a double click does ([Content Browser](content-browser.md), ADR-7725). |
| `editor.list_assets { folder?, search? }` | Content-browser listing. |
| `editor.edit_material { path, base_color?, metallic?, roughness?, emissive?, alpha_mode? }` | Write `.material.ron` base parameters (the material editor's save). Each value must be finite and in `0..=1` (`F0002`); `emissive` is linear RGB (up to 3 channels, each `0..=64`: the panel's emissive color times its brightness boost) and `alpha_mode` is `"opaque"`, `"mask"` or `"blend"` (the panel's Render state; ADR-7747). The result also reports `emissive` and `alpha_mode`. While the Material panel has the same file open with unsaved edits the call is refused, `data.material_error: "material_unsaved_in_panel"`: save or undo them in the panel first. When the panel has it open and clean, it shows the new values as one panel undo step. |
| `editor.spawn_prefab { path, translation? }` | Spawn a prefab instance into the open level (undoable). |
| `editor.open_prefab { path }` / `editor.save_prefab` / `editor.close_prefab { save? }` / `editor.prefab_mode` | Prefab edit mode. Save re-bakes the template and updates live instances; `instances_updated` counts the level-authored instance roots rebuilt (an instance nested in another prefab's instance rebuilds that root, counted once). When the instances cannot spawn from the saved template (it nests itself), the file is saved, the instances keep the previous template, and the call returns an error that says so. `editor.prefab_mode` returns `{ editing, dirty, in_context }`: `in_context` while the prefab was opened in place with the level drawn around it ([Prefab overrides and Edit in place](prefabs.md)); `dirty` while the open prefab has edits a save has not written (they are autosaved, kind `prefab`, see [Editor autosave and recovery](editor-autosave.md)). An isolated open (`editor.open_prefab`) replaces the whole scene (a level save is refused with reason `prefab_open` until the close); an open in place leaves the level in the world, drawn as dimmed read-only context (not pickable, not in the Outliner, a level save refused with reason `prefab_in_place`). Closing restores the stashed level either way: nothing of a closed prefab stays in the next one. |
| `editor.new_project { name, path?, template? }` | Create a project and open it. `template` accepts `blank` (default; `empty` is the same template), `first_person`, `third_person`, or `top_down`. `name` follows the `regia new` rule: a name that starts with a digit, is a Rust or Java reserved word, or is a Windows device name (`con`, `nul`, `com1`...) is refused. |
| `editor.open_project { path }` | Open a folder that contains `Regia.toml`, and its startup level when that file exists. |
| `editor.recent_projects` | Recent project directories, newest first. |
| `editor.create_script { name }` | Write `game/src/scripts/<name>.rs` and register it at `// REGIA:SCRIPTS`. |
| `editor.history` / `editor.spawn` / `editor.delete` / `editor.reparent` / `editor.duplicate` / `editor.add_component` / `editor.remove_component` | The undo stack and the outliner's and inspector's structural edits, one undo entry per call. See [Undo and redo](undo-redo.md). |
| `editor.hierarchy` / `editor.group` / `editor.select_hierarchy` (and the multi-entity forms of `editor.reparent` / `editor.duplicate` / `editor.delete`) | The Outliner's tree and its multi-entity edits (drag-and-drop reparent keeping world poses, group, duplicate, delete, select subtree/children/parent), one undo entry each. See [Outliner hierarchy edits](hierarchy.md). |

## Play in editor

`editor.play` does not block the protocol thread. It queues a request; the editor loop copies the edit world (a `regia_scene` snapshot) into a play world, runs one settled frame, and publishes `editor.play_state`. When a game crate is open, that play world gets the hot-reloaded game plugin. Otherwise play uses the engine's built-in systems. `editor.stop` drops the play world. The edit world is not written during play.

In the editor window, Alt+P plays when no session runs; it does nothing while one runs, so a stray press never restarts a session. Escape stops a running or paused session. It asks to close the editor only when no session runs and no panel has the keyboard (ADR-5615).

`editor.play { mode: "simulate" }` runs the world with no player, `players` joins local split-screen players (the play window and a possessed viewport composite [one pane per player](pie-modes.md#split-screen-players)), and `editor.keep_simulation_changes` copies play state back as one undo step; a click on a possessed play view captures the mouse and Shift+F1 releases it. `editor.play { window: true }` plays in a separate window (closing it stops play; a host with no window system answers `play_window_unavailable`); `preset` or `size` picks the resolution the play world renders at there, letterboxed with clicks mapped back ([Play resolution](pie-modes.md#play-resolution)). See [Play-in-editor modes and editor jobs](pie-modes.md) (ADR-7205, ADR-7272, ADR-7283, ADR-7395). `start` (`player_start`, `camera`, `entity` with `entity`), `time_scale` (0.1..=4) and `mute` choose where the player starts, the play speed and whether the game's audio is muted; `editor.play_controls { time_scale, mute }` changes a running session; `device` simulates a phone or handheld in the play window ([Start options, speed and mute](pie-modes.md#start-options-speed-and-mute), [Device simulation](pie-modes.md#device-simulation), ADR-7829).

## Projects

Launching without `--project` shows the project browser (recent projects, Blank, Open folder). `editor.new_project` accepts `blank` (default), `first_person`, `third_person`, or `top_down` (the browser's quick-create stays Blank). A generated project path-depends on the engine checkout that built `regia` (`{{engine}}` in the template). Open it with `regia-editor --project <root>`; the startup level in `Regia.toml` loads, and Play runs it.

## Close and autosave

While the level is dirty the editor writes `.regia/autosave/<stem>.level.ron` (`<stem>~<8 hex digits>.level.ron` when another level under `assets/` shares the stem; default every 5 minutes). The next launch offers Restore when that file is newer than the level. Restore makes the autosave the open level with a fresh undo history (the old one belonged to the level it replaced), marked unsaved. Closing asks Save and exit, Discard and exit, or Cancel when any document has unsaved edits: the level, the Material panel's material, Project Settings (`Regia.toml`), the UI designer's document, or an open material graph, function or instance. The prompt lists them, and Save and exit saves each. A material-graph edit does not mark the level dirty, and saving the level does not clear it. An empty new graph with no nodes is not listed. A successful save deletes the autosave. An exit keeps the crash-recovery autosaves the recovery window still offers (neither restored nor discarded) and clears the others. `quit.request` never prompts: it writes the level's autosave first when the level has unsaved edits and keeps it for the next launch; the other documents' unsaved edits are lost and logged as a warning.

## The undo stack is shared

Agent edits and human edits share one stack. `scene.set_property` and friends change the
world directly (runtime semantics); the editor-side edits go through it undoably. To make a
scene change you want the user to be able to undo, use the `editor.*` edits (`editor.set_transform`,
`editor.set_property`, `editor.spawn`, `editor.delete`, … on [Undo and redo](undo-redo.md)) and the
panel actions — `scene.*` edits are not undoable in the editor.

## Seeing the result

`seeing.screenshot` captures the whole editor window (docking layout, panels, viewport) —
served from the editor's own present, so the image is exactly what the human sees.

`editor.capture_viewport` is the headless viewport: the same frame the game renders —
meshes from `MeshSource` (glTF or `builtin:` primitives with `.material.ron` / material-graph
materials), lights, environment, post, and whatever the project installed on the world's
`RuntimeRenderer` (terrain, VFX) — plus the editor overlays: grid, icons (lights, cameras,
decals, audio emitters), bounds and light ranges of the selection, the selection outline and
the gizmo. The grid is hidden behind geometry (depth-tested against the entity-id buffer). Entities with nothing to draw show as gray proxy cubes; a level without a sun gets a
neutral preview sun (`info.preview_sun`). The default `editor` preset is native resolution
without TAAU and without the physical sky (a backdrop sky stands in), so an editing frame does
not jitter; `quality: "game"` renders with the world's `RenderSettings`. In play, a possessed
viewport looks through the level's camera and shows no overlays (`possessed: false` ejects).

Placing a mesh in a level: add `MeshSource { mesh, material }` (`scene.add_component`
`"MeshSource"`); the level file stores the paths, the editor resolves them.

## Example: place a rock, check the view, undo

```console
$ regia agent call scene.spawn '{"name":"Rock","components":{"Transform":{"translation":[2.0,0.5,-1.0],"rotation":[0,0,0,1],"scale":[1,1,1]}}}'
$ ROCK=$(regia agent call scene.query_entities '{"name":"Rock"}' | jq '.entities[0].entity')
$ regia agent call editor.select "{\"entities\":[$ROCK]}"
$ regia agent call editor.set_transform "{\"entity\":$ROCK,\"translation\":[3.0,0.5,-2.0]}"
$ regia agent call seeing.screenshot --out shot.png
$ regia agent call editor.undo
```

## Command palette

`editor.command` lists and runs editor commands (Ctrl+Shift+P in the window). See [Editor command palette](editor-command.md).

## Measuring the pane path and choosing the card

A live pane renders at the pixels it covers on screen: on a Retina or HiDPI window (more than one pixel per point) that is
its size in points times the window's scale, in whole pixels, at most 2040 a side (aspect kept). `pane.textures.last_size`
in `editor.viewport_stats` is that pixel size (a 421x282-point pane on a 2x Mac reports `[842, 564]`).
`editor.viewport_stats` reports the live pane rate (`pane.pictures_per_second`). For a repeatable number outside a running
editor, `cargo run -p regia_editor --example pane_bench --release -- 1280x720 120` renders a pane for 120 frames on the GPU
texture path and on the CPU readback path and prints median and p95 milliseconds, fps and whether the median holds 60 fps.
The editor window and that example both honor `REGIA_GPU_INDEX`, `REGIA_GPU_NAME`, `REGIA_GPU_PCI`, `REGIA_GPU_UUID` and
`REGIA_GPU_STRICT` (with `STRICT`, a variable that matches no card makes the editor exit with the reason instead of falling
back). On lavapipe the numbers only show that both paths run; the readback saving needs a real card.

## Multi-select editing

With several entities selected, the Inspector shows one value where a field agrees and an em dash where it does not. Editing a field writes it onto every selected entity that has the component, and one undo restores all of them. A vector lane, a color channel and an Euler axis (`rotation.rx`, `rotation.ry`, `rotation.rz`, degrees) are separate: editing one leaves each entity's other lanes as they were. A component present on only some of the selection is labeled "on k of n" and stays editable on the entities that have it. Add component and Remove component apply to the whole selection.

`editor.set_property` and `editor.set_transform` take either `entity` (one id, unchanged) or `entities` (an array of ids). Passing both is `entity_and_entities` (`F0002`). `entities: []` is `entities_empty` (`F0002`). A value that is not an array is `entities_not_array` (`F0002`). An element that is not an id is invalid params with no reason. A dead id is not found with no reason (`F0003`), and the message says the entity does not exist. No selected entity has the component: `component_on_none` (`F0003`). An unknown field is `path_unknown` (`F0003`). A value of the wrong type is `value_mismatch` (`F0002`). A feed-owned entity is `feed_owned` (`F0002`). An omitted transform channel keeps that entity's own value. The `entities` form marks a prefab override on each instance; the single `entity` form does not. The multi response adds `entities`, `skipped` and, for `editor.set_property`, `befores` (`[{entity, before}]`). `dry_run: true` validates and writes nothing.

`editor.inspect_selection` returns the merged values. Omit `entities` to read the editor selection; `entities: []` is an empty selection (`count: 0`), not an error. The result is `{ count, entities, primary, components }`. Each component is `{ component, type_path, present, selected, editable, fields }`. A field is `{ path, mixed, value }` and, for a vector, a quaternion or a color, `axes: [{ axis, mixed, value }]`. `mixed: true` has `value: null`. Color axes are `r`, `g`, `b`, `a`. Euler axes are `rx`, `ry`, `rz`. `editable` is true when `present` is above zero.

`search` (a fuzzy query, as the Inspector's search box) keeps only the matching fields, and `modified_only: true` only the fields that differ from their default (the prefab's value for a prefab instance) on at least one entity; a component left with no field is dropped and the answer carries `filter: { search, modified_only }`. See [Inspector asset fields and component removal (agents)](inspector.md). `raw: true` (the Inspector's raw view) marks `@Hidden` fields `hidden: true` and structural components `structural: true`; a field whose `@EditCondition` is false always has `enabled: false` and its `condition`.

Prefab overrides appear as bold rows beneath the component, with the entity id and field path. Revert and Apply target that instance. Apply changes the loaded template; one undo restores both the template and the override marker. Apply does not write the source prefab file. Applying to a template that nests another prefab is refused (`prefab_apply_failed`).

`editor.prefab_override { entity, component, path, action }` does the same from an agent (ADR-7307):
`action` is `"revert"` or `"apply"`, `entity` the instance entity, `component` and `path` the override as
the instance lists it. Returns `{ action, entity, component, path, edited, undo_depth }`. Errors:
`bad_entity`, `bad_params`, `bad_action` (`F0002`), and the edit's own `not_prefab_instance`,
`override_absent`, `prefab_template_missing`, `path_unknown`, `feed_owned`, `prefab_apply_failed`.

A field absent on another selected enum variant returns `path_unknown`; a different field type returns `value_mismatch`. Both refuse the whole edit before any entity or undo history changes. Components missing entirely are still skipped.

Synthetic Euler paths (`rotation.rx`, `rotation.ry`, `rotation.rz`) and named color-channel aliases use the `entities` form. For one target, pass an array containing that id; the legacy `entity` form keeps its existing reflection paths.

## Monkey test: replaying a failure

`crates/regia_editor/tests/monkey.rs` (ADR-3290) drives every editor panel with seeded random input through the same path a
user's window takes: pointer moves, clicks and drags in the focused panel, keys and shortcuts, text, scrolls, tab drags (dock
and undock), window resizes down to 160x120, undo, redo, and Play/Stop on the toolbar. After every step it checks that nothing
panicked, every transform in the edit and play worlds is finite, the undo stack stays within its cap and egui drew no ID clash
warning. Every 60 steps it sends input only to the panels whose edits are all undoable (Outliner, Inspector, Viewport, World
Settings), undoes back to where that started, and checks the level is byte for byte what it was.

The gate runs four fixed seeds of 300 steps (`cargo test -p regia_editor --test monkey`). A failure prints the seed, the step,
the last 40 actions and the command that replays it:

```text
monkey: seed 0x5eed0002 FAILED at step 117 of 300: panic: ...
  last actions:
    focus Montage
    click Primary 812,340 NONE on "Add section" in Montage
    ...
replay: cargo test -p regia_editor --test monkey -- --seed 0x5eed0002 --steps 300
```

Replay with `-- --seed <seed> --steps <n>` or `REGIA_MONKEY_SEED=<seed> REGIA_MONKEY_STEPS=<n>`; the same seed gives the same
input on every platform, so the run stops at the same step. `-- --seeds 50 --steps 5000` is the nightly soak (`--first-seed`
moves the range), `REGIA_MONKEY_THREADS` caps the parallel seeds, and `--no-isolate` keeps the caller's environment (normally the
test re-runs itself with an empty `PATH`, no display and a temp `HOME`, so no click can open a file dialog or start a program on
your machine). After fixing a crash, add `(seed, steps)` to `REGRESSION_SEEDS` in the test so the gate keeps replaying it.

W12 debug overlays: `editor.set_viewport` also accepts optional `paths` (boolean, default false)
for remaining navigation-agent paths. See [Viewport debug colors and paths](viewport-flags.md)
for `editor.debug_colors` and play-view behavior.

Fog of war debug view (audit gap FW-1): `editor.set_viewport` also accepts optional `fog` (boolean, default false;
Show ▸ Fog of war) and returns it: the watchers' sight circles, the grid outline, the fog texture and the viewing
team. See [Fog of war](fog-of-war.md#showing-the-fog).

Lockstep overlay (ADR-8562): `editor.set_viewport` also accepts optional `lockstep` (boolean,
default false; Show ▸ Lockstep) and returns it. It draws play's deterministic-index debug frame
(grid, cells, candidate pairs, contacts, queries, and the occupancy labels as stroke text at 0.4 m cap
height) over the viewport; what the frame holds is the `sim.det_debug_draw` switch. Contacts, candidate
pairs and queries are recorded while the pass runs, so they appear from the tick after the switch turns
on (KI-ED-019, ADR-10070). See [Seeing the index](det-ids-broadphase.md#seeing-the-index).

In-world AI debug (ADR-8463): `editor.set_viewport` also accepts optional `ai_bt`, `ai_state_tree`,
`ai_goap`, `ai_squad`, `ai_formation`, `ai_move_to` and `ai_blackboard` (booleans, default false).
Any one of them turns on the `ai_debug` gizmo group; clearing the last one turns that group off.
Game view leaves these flags as they are. See [Seeing AI in play](ai.md).
