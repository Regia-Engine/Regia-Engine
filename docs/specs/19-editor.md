# Spec 19 — Editor (`regia_editor`)

Desktop only (Linux, Windows; macOS once that port happens). Built inside each user project (`editor/` crate) so it shares a
compiler and engine build with the game dylib (`specs/20`).

## 1. Architecture
- The editor is an `App` with the full engine plugins **plus** `EditorPlugin`. It owns:
  - the **edit world** (the level being edited; simulation systems disabled: no physics stepping, no
    scripts, animation preview only where requested),
  - zero or one **play world** (PIE; full simulation),
  - the asset database (`regia_asset_pipeline`) running on background threads,
  - the egui context, rendered as an overlay node in the window's render graph; the engine viewport
    renders to an offscreen texture registered with egui-wgpu and displayed inside the dock area.
- Editor-only components (selection, gizmo state, editor camera) live in the edit world and are
  filtered from saving by an `EditorOnly` marker.
- **All edits are commands** (`EditorCommand` trait: `apply(&mut World)`, `revert(&mut World)`,
  `merge(&mut self, next) -> bool`, `description()`), pushed onto the `UndoStack` (transaction groups,
  coalescing of continuous edits like gizmo drags and slider scrubs, cap 500 entries, dirty tracking
  per level/asset). Reflection makes generic commands possible: `SetProperty { entity, component,
  path, old, new }`, `SpawnEntities { serialized }`, `DespawnEntities { serialized }`, `Reparent`,
  `AddComponent`, `RemoveComponent`.

## 2. Layout and panels
Dockable with `egui_dock`; default layout saved in `.regia/editor_layout.ron` per project; "Reset layout".
The user's default layout, the editor preferences and the keymap live in the user config directory
(`editor_layout.ron` at `format_version` 2 since ADR-2771, `editor_prefs.ron` and `editor_keymap.ron` at 1); a project opens with its own
layout, else the user's default, else the built-in one (ADR-2321, `book/agents/editor-preferences.md`).
World Settings (Edit ▸ World settings…) edits the current level's `LevelSettings`, undoable and saved with the level, and
the Output Log's `file:line` and asset paths are links; the code editor command is a preference only the owner sets
(ADR-2696, `book/agents/viewport-tools.md`).

| Panel | Function |
|---|---|
| **Viewport** (1..4) | Perspective/orthographic (top/front/side), view modes (`specs/13` § 11), show flags (grid, gizmos, colliders, lights, cameras, nav — Tier 2), camera speed, realtime toggle, stats overlay, game view (`G`, hides editor helpers), resolution preview for play. |
| **Outliner** | Hierarchy tree with search/filter (name, type, tag), visibility/lock toggles, drag to reparent, multi-select, prefab instances shown with link icon and override marker, context menu (duplicate, delete, create empty child, create prefab, focus). |
| **Inspector** | Reflection-driven properties of selection; multi-select editing (shows mixed values); add/remove components and scripts; per-type custom drawers (colors, curves, asset refs with drag-drop + thumbnail + picker, enums as dropdowns, flags as checkboxes, vectors with x/y/z fields, quaternions as Euler degrees); respects `regia_ecs::attrs`; prefab overrides displayed bold with revert/apply buttons. |
| **Content Browser** | Folder tree + grid/list, thumbnails (rendered by headless render pass, cached in `.regia/thumbs`), search + type filters, create assets (material, prefab, level, anim graph, sound cue, UI layout, data asset, folder), import (drag from OS file manager, or button), rename/move (with references preserved), delete with referencer check, "show in explorer", reimport, asset import-settings editor. |
| **Output Log** | `tracing` events with level/target filters, search, clickable asset paths and `file:line` (opens in configured code editor: `code`, `zed`, `$EDITOR`), compile output from game builds. |
| **Asset editors** | Material editor (parameter inspector + live preview sphere/cube/plane/mesh), Texture viewer (mips/channels), Mesh viewer (LODs, UVs, normals, collision), Anim Graph editor + clip viewer (`specs/17` § 5), Sound/SoundCue editor, Prefab editor (isolated scene), Input config editor, Physics layer matrix editor, Audio bus mixer, Data asset editor (generic reflection inspector). |
| **Project Settings** | `Regia.toml` + `config/*.ron`: general, startup level, render profiles per platform, physics, audio buses, input, localization, platform packaging options (icons, ids, orientation, signing refs). |
| **Editor Preferences** | Theme (dark/light), UI scale, camera controls, code editor command, autosave interval, key bindings. |
| **Profiler** | CPU spans timeline (from `FrameProfiler`), GPU pass timings, frame time graph, entity counts, memory per asset type; works for PIE and for a connected dev build (Tier 2). |
| **World Settings** | `LevelSettings` of the current level. |

Menus: File (new/open/save level, save all, recent, project settings, package for platform, exit),
Edit (undo/redo, cut/copy/paste/duplicate/delete, select all, preferences), View (panels, layouts),
Tools (bake reflection probes, rebuild asset DB, clear caches), Play, Help (docs, about).

As built (ADR-7475): the menu bar has File, Create, Edit, View, Tools, Sequence (ADR-7726: Render movie…), Play and Help; `editor.menu_list` answers the
whole tree with each item's agent method and parameters (`book/agents/editor-menus.md`). File ▸ Package for platform…
opens a dialog (target, profile, output folder, dry run) that runs `regia package` as a cancellable editor job
(`package:<n>`, background task `package`; a cancel removes what the run wrote into the output folder) with progress,
the result and a log link; `editor.package_start`, `editor.package_status`. Tools ▸ Rebuild asset database (in
process, under the cook lock) and Clear caches… (confirm, then `regia clean --cache` as a job). Play mirrors the
toolbar plus Players ▸ 1–4. Help ▸ Documentation and Agent guide open the local book (or the generated manual); Help ▸
About shows the version, git revision, the license (MPL-2.0, ADR-10225) with a link to its text and links to `LICENSE` and `THIRD_PARTY_NOTICES.md`.
Project Settings ▸ Icon copies a PNG to `assets/branding/icon.png`, the file `regia package` ships. Tools ▸ Bake
reflection captures is built (ADR-7592): a cancellable editor job (background task `bake_captures`) that bakes every
placed `LocalCapture` (ADR-2860) with the editor's renderer and writes the level's `.fcap` with the bytes of the
`regia bake-captures` path, through a temporary file and a rename, so a cancel leaves the old file;
`editor.bake_captures`, `editor.bake_captures_status` (`book/agents/reflection-captures.md`).
Amended (ADR-7496): a package run writes into a private staging folder inside the output folder and only a finished
package is moved into place, so a cancel, failure or dry run removes the staging folder and nothing else;
`editor.package_start` needs `confirm: true` unless `dry_run` and its `out` is a request path (ADR-1816). Rebuild asset
database never waits for the cook lock (`tools_busy`). `editor.menu_list` equals the drawn menus (a shell test draws
every menu open and compares), and the Play menu draws Compile.

## 3. Viewport interaction
- Camera: hold RMB + WASD/QE fly (scroll adjusts speed), Alt+LMB orbit, Alt+RMB / scroll dolly,
  MMB pan, `F` frame selection, numeric view presets. Smooth transitions.
- Selection: click (GPU picking: render entity ids to an R32Uint target on demand, read back 1 px; fallback
  CPU ray vs mesh BVH), Ctrl/Shift add/remove, box select (drag in empty space), select children,
  select by asset. Surface picks (Tincal F7, ADR-227): the id buffer names the entity, a CPU ray cast
  (Möller-Trumbore, brute force on the candidate's triangles, skinned meshes with their palette) returns
  the triangle, barycentric weights, world point and normal (`IdBuffer::hit`, `editor.pick_hit`).
- Batch thumbnails (Tincal F8, ADR-236): `regia_editor::thumbnails::ThumbnailRenderer` renders many small
  deterministic pictures of models or entities on one warm frame renderer, grouped by size
  (`editor.thumbnails`).
- Per-entity render overrides (Tincal F5, ADR-227): `RenderOverride` (tint, emissive, ghost alpha, x-ray,
  unlit) on the instance's object data, no material clone (`editor.set_render_override`).
- Gizmos (transform-gizmo-egui): `W` translate, `E` rotate, `R` scale, local/world toggle, pivot
  (center/individual origins), snapping (grid size, angle step, scale step; hold Ctrl to toggle), vertex
  snap (V), surface placement (drop-to-floor `End`, align to surface normal option).
  Built (ADR-2696): `V` held while translating moves the pivot to the nearest vertex of the triangle under the pointer,
  with the moving entity left out of the ray; `End` drops each selected entity onto the first drawn mesh or terrain under
  its footprint as one undo step (a collider with no mesh is not a surface: the edit world steps no physics).
- Per-vertex scalar overlay (Tincal F10, ADR-232): `ScalarOverlay` colors a mesh entity by one value per vertex through a
  ramp, unlit over the lit viewport, as its own pass (skin-weight and deformation heatmaps, damage brushes;
  `editor.set_scalar_overlay`).
- Drag and drop from content browser: a mesh spawns an entity with `MeshSource` at the drop hit (the
  ray hit on a mesh or the terrain, otherwise the point at the orbit-target depth) and a hidden part
  entity carries the `MeshRenderer`. A collider is added only when the glTF `.meta` import settings
  request collision, as a triangle mesh of the loaded model. A prefab spawns an instance. A material
  dropped on a mesh sets that owner's `MeshSource.material` (one override for every part). A sound
  spawns an `AudioEmitter` whose `sound_name` is the asset path. A level opens. Any other asset is
  not placed.
- Duplicate (Ctrl+D) offsets new entities; Alt+drag gizmo duplicates.
- Component-specific gizmos: light ranges/cones, camera frustums, collider shapes (editable handles
  for box/sphere/capsule sizes), audio ranges, reflection probe bounds, player starts, trigger volumes.
  Collider handles are built (ADR-2696): a viewport toggle (`C`) replaces the selection's gizmo with grips for box, sphere
  and capsule sizes and the offset of any collider; one undo step per drag.
- Several independent viewports per world (batch v020, ADR-230): a `ViewportInstance` carries its own
  perspective camera, layout (panes, quality preset, show flags, game view) and settings, drawn by
  `viewport_panel_with`; the main viewport is the same panel over the world singletons. A
  `CameraLinkGroup` keeps panes orbiting together (last mover leads). The selection, gizmo mode,
  bookmarks and undo stack stay shared. Agent surface: `editor.viewport_instances`,
  `editor.viewport_instance_set`, `editor.viewport_link`, `editor.capture_viewport_instance`,
  `editor.viewport_instance_remove` (`book/agents/viewports.md`).

## 4. Play in editor
- **Play** (Alt+P / toolbar): serialize edit world (in-memory postcard via reflection) → build play world
  → run `Startup` + `LevelLoaded` → simulate. Edit world untouched; editing during play edits the play
  world (changes discarded on stop unless "keep simulation changes" is used on selected entities).
- **Simulate** (no player spawned, camera stays free), **Pause**, **Step one frame**, **Stop** (Esc).
  Built (ADR-5615): Alt+P plays only while no session runs (a stray press never restarts one); Esc stops a running or
  paused session and only asks to close the editor (ADR-2179) when none runs.
- Play in viewport or in a separate window (with chosen resolution, e.g. phone aspect ratios).
  Multiple local players for split-screen testing.
- "Possess/eject" toggle (`F8`): detach editor camera during play to inspect.
- Input focus: the play viewport captures input when clicked; `Shift+F1` releases cursor.
- Launch standalone (desktop dev build with current cooked assets) and **Launch on Android device**
  (build + install via adb + logcat streamed into Output Log) — the latter in M16.
  As built (ADR-8980, `book/agents/editor-launch.md`): the Output Log's launch bar and `editor.launch { target:
  "standalone" | "android", device? }`. Standalone cooks for the host (incremental), builds the launcher with cargo and
  runs it in a play copy of the open level; Android lists devices with `adb devices -l` (the SDK lookup of `regia
  doctor`), packages the debug APK with `regia package`, installs it, starts the activity and streams `adb logcat
  --pid`. Lines reach the Output Log as `[launch <id>]`; Stop force-stops the app and kills every child's process tree
  (Unix process group, Windows job object). `editor.launch_status`, `editor.launch_stop`, `editor.launch_devices`.
  Not built: the agent protocol attached to a device launch (use `regia install-android --attach`), a start level for
  the Android launch. ADR-7548: the app id must be an Android package name (`launch_bad_app_id`, checked before any
  adb call); Stop force-stops only an app the launch started; `wait` holds the editor at most 1 s and answers
  `pending: true` after that; Windows children start suspended until they are in the job.

## 5. Game code integration
- Watches `game/src/**`; on change (debounced 500 ms, or manual "Compile" button with hotkey Ctrl+Alt+F11)
  runs `cargo rustc --manifest-path <m> -p game --lib --crate-type dylib [--release] --features hot-reload --message-format=json
  --target-dir <t> -- -C prefer-dynamic -C metadata=hotgen<N>` (`specs/20` § 1; `--release` only in a release editor build,
  `prefer-dynamic` so the dylib shares the process's `std`; ADR-5615); diagnostics parsed and shown in
  Output Log with clickable locations; success triggers hot reload (`specs/20`). Status indicator in
  the toolbar (idle / compiling / error / reloaded).
- "Open code editor" action; "New script" wizard creates `game/src/scripts/<name>.rs` from a template
  and registers it in `game/src/lib.rs` (simple text insertion at a marker comment).

## 6. Project browser
On launch without a project: recent projects, "New project" (templates: Blank, Third Person,
First Person, Top Down — Tier 1 ships Blank in M11, Third Person + First Person in M20), open existing.
Calls the same code as `regia new`.

## 7. Robustness
- Autosave (every 5 min default) to `.regia/autosave/`; crash recovery prompt on next launch.
- Panics inside game code during PIE: caught at the script/system boundary with `catch_unwind` where
  possible (scripts, game systems run in `catch_unwind` wrappers in editor builds only), stop PIE, log the
  panic with backtrace; editor stays alive. Panics that cannot be caught (abort, segfault in unsafe
  game code) → crash recovery.
- Long operations (import, cook, bake, build) run in background with progress notifications and cancel.

## 8. Acceptance
- M10: open/save a level; create entities; inspect/edit every built-in component; undo/redo of every
  edit type (property test: random command sequences then full undo returns byte-identical scene);
  viewport navigation and gizmos; picking precision 1 px; editor idles at < 5% CPU when nothing changes
  (egui repaint on demand; viewport realtime toggle).
- M11: build the level of `samples/sim_bridge` (meshes, materials, lights, prefabs, kind prefabs)
  entirely in the editor (documented step-by-step in progress.md); PIE start < 1 s for the sample; hot reload of a script change < 10 s end-to-end
  (incremental build) with state preserved; import → place → play loop works without restarting.

## 9. Editor tools (ADR-1523)
- An **editor tool** is an out-of-process program described by `<id>.tool.toml` (format 1): how to
  launch it, the scope of its token (permission names and method patterns), the methods it provides
  and its panel title and link. Manifests are read from `<project>/.regia/tools/`, `<user dir>/tools/`
  and `REGIA_TOOL_PATH`, first id wins; invalid ones are listed with their error. `autostart` is
  honored only outside the project.
- **Launch** mints a scoped token per launch on a second loopback listener (the master token is never
  given out) and passes it in the child's environment only; exit, stop and editor close revoke it.
- **`Panel::Tool`** (the Tools tab) lists the tools with launch/stop, and for the selected tool its
  tasks (progress events), its declarative view (heading, label, value, progress, button, link,
  separator), its log and its provided methods.
- **Proxy:** agents call provided methods with `editor.tool_call`; the tool takes work with
  `editor.tool_poll` and answers with `editor.tool_reply`. Progress is `editor.tool_progress`, also an
  `events.read` record. The full surface is `book/agents/editor-tools.md`.
- Acceptance: a real child launched from a manifest connects with its token, is refused outside its
  scope, sets its view, reports progress, answers a proxied call and an error, takes a click, and exits
  on stop with its token revoked (`crates/regia_editor/tests/tools_e2e.rs`).

### Status (ADR-2771, editor diagnostics and settings)

- Input config editor: modifiers, triggers and context priorities are editable in Project Settings and by
  `editor.project_input_edit`, with conflict reporting; each write is one undo step. Saving rewrites `config/input.ron` in
  canonical RON, so comments are lost.
- Floating dock windows are saved in `editor_layout.ron` (`format_version` 2) and restored, clamped onto the screen.
- Diagnostics window (GPU tab, `.regiatrace` timeline): `docs/specs/54` § 6.

### Status (ADR-5615, conformance audit 2026-11-10)

Not built yet; each is a v0.5.x gap, not a change of intent:
- § 1: dirty tracking is one level stack plus a dirty flag per asset panel. (The `EditorOnly` save filter is built,
  ADR-7205: `EditorOnly` and `GltfEditorOnly` entities and their descendants are left out of saves and exports.)
- Built (ADR-7468, ADR-7475, ADR-7744): § 2's **Create prefab from selection** in the Outliner
  (`prefab_create.rs`, `editor.create_prefab`); the File ▸ Package for platform, Tools, Play and Help menus
  (`menus.rs:69` `MENUS`, `editor.menu_list`, `package_job.rs`); and the viewport resolution preview (toolbar
  **Resolution**: the play window's letterbox frame at a chosen `play.resolution` preset or custom size, with
  action-safe and title-safe guides; `resolution_preview.rs`, `editor.viewport_resolution_preview`). Content
  Browser creation, the data asset editor and the Project Settings pages are built too: ADR-7233, ADR-7234 below.
- Built since (ADR-9700, v0.6.0 Stage 2): the material preview's cube, plane and project mesh with orbit and a light
  control (Material panel and material graph editor), and Content Browser creation of material graphs and material
  functions.
- Built since (w8-editor-smalls-z): the Outliner's `type:<Component>` and `tag:<name>` search terms (beside `kind:`;
  `filter_tree`), New ▸ Material instance, and select by asset (§ 3 above).
- Built (ADR-7468, ADR-7592): § 3's player start and trigger volume gizmos (`overlays.rs:3` and `:36`,
  `player_start_helper`, `trigger_volume_segments`; the Icons flag) and the reflection probe bounds
  (`capture_helper.rs:2`, under Show ▸ Bounds). (Select by asset is built,
  w8-editor-smalls-z: the Content Browser's **Select in level** and `editor.select_by_asset`
  select every level entity whose reflected string fields reference the asset path — mesh, material
  override, sound — plus prefab instances of the path's asset id; `book/agents/content-browser.md`.)

Built since (ADR-7204, `book/agents/viewport-tools.md`): the CPU picking fallback casts the mesh BVH; bare 1-4 are the
perspective/top/front/side presets and Focus, presets and bookmark recall (now Shift+digit; Ctrl+digit stores) ease over
0.25 s; a pivot mode (median, individual origins, active element) and a Snap toggle that Ctrl inverts; Ctrl+D in the
viewport and Alt+drag gizmo duplicate, each one undo step; File ▸ Save all, Recent (this session) and Exit; Edit ▸ Cut,
Copy, Paste, Duplicate, Delete and Select all; the Outliner eye (undoable), lock (not saved), prefab link icon and
override marker, and Focus. Since ADR-7275: File ▸ Recent and, per level, the perspective camera, the camera bookmarks
and the Outliner locks (by `EntityGuid`) are kept per project in `.regia/editor_view.ron` and restored when the level
opens (`editor.view_state`).
- § 4: launch standalone and launch on an Android device. (Built
  by ADR-7205: Simulate, keep simulation changes, Shift+F1, split-screen players through `PlayerManager` with
  their rects reported. Built by ADR-7272: the separate play window, `editor.play { window: true }`: its own
  surface on the shared device, input routed to the play world only, Shift+F1, close stops play, size and
  position in the editor preferences; a host with no window system answers `play_window_unavailable`. Built
  by ADR-7283: the chosen play resolution: presets (window size, 720p, 1080p, phone 9:16 and 9:19.5, tablet
  3:4) and a custom size in the `play.resolution*` preferences or `editor.play { preset | size }`; the picture is
  letterboxed into the window and cursor positions are mapped back to play pixels (a press in the bars reaches
  nothing); the viewport shows a placeholder while the window is open, so play renders once per frame. Built
  by ADR-7395: split-screen composites — the play window and a possessed viewport pane draw one pane per local
  player into the player manager's rects (`PiePlayerCamera` stands in per player when the play world has no
  window cameras), and `editor.play_state.play_window` reports `players` and `panes`. ADR-7466: the stand-ins
  are decided every frame after the game's `Startup`/`Update` (a game camera at any time replaces them), only
  PIE-joined controllers are stood apart, and the panes are the drawn `pixel_viewport` rects.)
- § 7: built by ADR-7205: a game panic logs its backtrace and `editor.play_state` keeps it; the content import
  (`editor.import { async }`) and the game build cancel through `editor.cancel_job`, leaving no partial output. The
  Content Browser's Import runs the same background job with a Cancel button.
  Since ADR-9770 (merged by ADR-7381): document autosave of the asset editors, the recovery list and a toolbar
  Cancel for the game build (`editor.cancel_task`, the same token as `editor.cancel_job`). Follow-ups since ADR-7340
  (merged by ADR-7422): the several-client PIE worlds run game code through the same panic boundary (backtrace + `pie`
  crash report), a cancelled game build kills its whole process tree (Unix process group; Windows job object with
  `KILL_ON_JOB_CLOSE`), the UI animation editor autosaves (kind `ui_anim`), and the VFX autosave keeps the
  disabled-module stash in its editor-only envelope. Since ADR-7382: sequences (kind `sequence`, each `.seq.ron` the
  Sequence Curves panel opened has a dirty flag of its own: the player's sequence against its saved copy) and prefab
  edit mode (kind `prefab`, the mode's own undo history is the prefab's dirty flag) autosave and restore too; prefab
  mode clears every scene entity when it switches scenes (a closed prefab no longer leaks into the next one), and a
  level save or autosave adopts runtime spawns (`scene.spawn`, `scene.spawn_primitive`) into the open level. Since
  ADR-8730: the Shots panel's shot list is a document (path, saved copy, dirty; Open, Save, Ctrl+S over the panel)
  and autosaves too (kind `shots`; only a list with a file, since Restore puts it back at its path). Since ADR-7479:
  restoring a level recovery row while a prefab is open is refused (`autosave::apply_restore`;
  `editor.recovery_restore` answers `prefab_open`) and changes nothing. Since ADR-8940: a list another tool loads into the `ShotPlayer` detaches the document (path cleared, unsaved; Save without a path answers `shot_panel_detached`), and **Edit player's shots** (`editor.shot_panel_adopt`) takes a sequence player's own shot list as an unsaved document. Since ADR-7662: the preview's voices stop when the transport stops (Pause, Stop, the end of a non-looping play) and start again at the playhead on Play; Play-in-editor snapshots the level with the preview lifted; the shot-list autosave writes the current format version and restores into the `ShotPlayer`; the close prompt lists a shot list with no file without saving it.
  Since ADR-7726: Sequence ▸ Render movie… renders a saved `.seq.ron` as a cancellable editor job that runs `render.out_job` in process on the editor's renderer (the same frames and hashes; output folder confined to the project; Cancel keeps whole frames only; `editor.render_movie`, `editor.render_movie_status`, `render_movie` records in `events.read`); the Sequence Curves panel has a Restore on stop row (`.seq.ron` `restore_on_stop`, `editor.seq_restore_on_stop`); and the Shots panel draws and edits the shot camera's position path and rotation keys (`editor.shot_panel_edit` `track: position | rotation`), each edit one undo entry.

Built since the audit (ADR-9740, `docs/specs/83-prefab-profiler-volumes.md` § 4): the Profiler's entity counts and
memory per asset type, its frame-time history with hitches, and its sources: PIE (in process and separate processes)
and a connected dev build by address and token. Built (ADR-7325, which lifted ADR-9740's deferral): the Profiler's
per-thread **Timeline** tab (`thread_timeline.rs`, `profiler_panel.rs`; profile-capture v2 carries the spans) and its
multi-frame strip, `editor.profiler_freeze` and names for systems past `MAX_SYSTEMS` (ADR-7409); a process or
dev-build source's history holds every frame (`profiler.frame { since_frame }`) and its Entities tab ranks components
like an in-process source (`profiler.frame { entities }`), and freezing a history frame stops the history's intake
until unfrozen (ADR-7849).

### Status (ADR-7234, editor creators)

Built: Content Browser New ▸ anim graph, sound cue, UI layout and data asset (and `editor.create_asset` kinds
`anim_graph`, `sound_cue`, `ui_layout`, `data_asset`); data assets (`.data.ron` v1, `docs/formats/data-asset-ron.md`)
and the Data Asset panel (generic reflection inspector); the list and `editor.create_asset` accept only types
registered as data assets (`ReflectAsset`: a `#[derive(Asset)]` type registered with `register_data_asset` or
`#[reflect(Asset)]`; the derive alone registers nothing), so every file the editor
authors loads in a game (ADR-7490); the Project Settings Physics layers page (collision matrix),
Localization, Packaging (signing passwords only as `env:VAR`) and Audio buses pages; the New script wizard (File ▸ New
script…, templates `empty` and `component`); the project browser's template picker (Blank, First Person, Third Person,
Top Down). Since ADR-8730: New ▸ Sequence and Shot list (`editor.create_asset` kinds `sequence`, `shot_list`: a
`.seq.ron` with no tracks and a `.shots.ron` of one still shot, both 5 s at 30 fps, at the current versions). Agent
surface and codes: `book/agents/editor-creators.md`. The packaging icon picker is built (ADR-7475,
Project Settings ▸ Icon writes `assets/branding/icon.png`; no `Regia.toml` key). Still not built: a runtime loader for data assets
(`ReflectAsset`, spec 10 § 2.4). Since ADR-7725: New ▸ Procedural graph, VFX system, Conversation, Curve table,
Chooser and Data table (`editor.create_asset` kinds `procgen_graph`, `vfx_system`, `conversation`, `curve_table`,
`chooser`, `data_table`, each at its current version, the same bytes as the menu; `editor.asset_kinds` lists them);
one asset-open dispatcher (`asset_open`, `editor.open_asset`) behind the Content Browser's double click and the
Output Log's asset links, opening every authoring kind with a panel (`book/agents/content-browser.md`; an audio
patch has no panel); and a folder row's Validate, Reference graph, Size map and Audit actions scoped to the folder
(spec 32 § 8, the panel methods' `folder` param).

### As built (ADR-7468, create prefab, Create menu actors, helpers, Show ▸ Navigation)

§ 2 Outliner: **Create prefab from selection…** (row menu) and `editor.create_prefab { entities?, path }` write the
selection's subtrees to a new `.prefab.ron` at the current format version and replace them by one linked instance at
the same world transforms (one top-level entity is the prefab's root; several get a new root at their centroid under
their common parent), one undo step; undo restores the level's saved bytes exactly (`book/agents/prefab-create.md`).
§ 2 Create menu: **Player start**, **Trigger volume** (a 2 m sensor box collider), **Point light**, **Spot light**
(aimed down), **Directional light** and **Camera** beside Shape ▸, each at the viewport's focus point, selected, one
undo step; `editor.create_entity { kind }` makes every entry. § 3: the Icons flag draws a player start's flag and floor
arrow and every trigger volume (sensor collider) dashed; **Show ▸ Navigation** (`editor.set_viewport { navigation }`)
draws the baked nav mesh's polygons and off-mesh links (`book/agents/create-menu.md`). Everything this list once named
as not built exists now: the File ▸ Package for platform, Tools, Play and Help menus (ADR-7475, `menus.rs:69`), the
viewport resolution preview (ADR-7744) and select by asset (w8-editor-smalls-z), as the Status section above records.
Amended (ADR-7592): **Create ▸ Reflection capture** places a `LocalCapture` (the reflected capture component of
ADR-2860; an unused id, a 4 m box) at the focus point in one undo step (`editor.create_entity { kind:
"reflection_capture" }`), and under Show ▸ Bounds a selected capture draws its influence box and dashed blend
margin, the "reflection probe bounds" gizmo of § 3 (read-only: edit the box in the inspector).
Amended (ADR-7741): the Create menu also places an **Audio emitter**, **Rect light**, **Tube light**, **Decal** (a
`StaticDecal` of the built-in scorch prefab), **Post-process volume**, **Spline** (two points), **Nav link**
(`NavLinkAnchor`) and **Smart object** (no definition), each with its runtime component's default, at the focus
point in one undo step (`editor.create_entity { kind }`). Not built: a water plane and a fog volume, which are not
saved entity components (ADR-7741 deferred them).

### As built (ADR-7306, large projects)

§ 2 panels at scale: the Outliner caches its tree (rebuilt on a spawn, despawn, reparent, rename or kind change) and
draws only the rows on screen; Up / Down move the selection (Shift extends), Left / Right close and open the selected
row, and a selection made outside the panel opens its parents and scrolls it into view. The Output Log keeps the last
100 000 lines, filters them incrementally and draws one-line rows (a click shows the whole message under the list).
The Inspector pages lists, arrays, maps and sets longer than 200 elements (100 per page, jump to index).
`editor.panel_timings` returns the last draw times per panel for soak scripts (`book/agents/editor-panel-timings.md`).

ADR-7326 follow-ups: the Inspector keeps its copy of each shown component until the component's change tick moves
or the drawer edits it (an unchanged repaint clones nothing); the Layers panel validates its findings once per level
or definitions change; the shell's per-frame identity walks (hidden bookkeeping entities, guids and names for
agent-spawned entities) decide per archetype and visit no entity on a steady frame; the Smart Object blocks and the
UI designer hierarchy draw only the rows in view through `virtual_list::variable_rows` (measured row heights, prefix
sums).

### As built (ADR-7469, import settings and mesh viewer overlays)

§ 2 Content Browser: the row menu of an image, glTF/GLB, OBJ, FBX or sound has **Import settings…**, a typed panel over
the importer's settings type (the fields of `TextureImporterSettings`, `GltfImporterSettings`, `ObjImportSettings`,
`FbxImportSettings`, `AudioImporterSettings`; nested blocks such as `limits` and `collision_hulls` are RON text fields;
keys without a field are kept). **Apply** writes the `.meta` through `editor.set_import_settings`'s own writer (the same
values give the same bytes) and cooks once; **Revert** drops the edits. `editor.set_import_settings` now refuses a
value of the wrong type or out of the importer's range (`asset_settings_invalid`). § 2 Asset editors: the Mesh viewer
draws UV0 and UV1 layouts, vertex normals and the cooked collider (hull silhouettes from the dev cache, else the
`collision_*` nodes) over its render; `editor.view_asset { overlay }`. Agent surface and codes:
`book/agents/import-settings.md`. Built since (ADR-7744): every field has a **Reset** button (the importer default,
`editor.import_settings_edit { reset: true }`), and several sources of one importer edit together
(`editor.import_settings_open { paths }`): an edit or Reset goes to every source, a field whose values differ shows
`mixed` until one is set, and Apply writes each changed `.meta` and cooks once (`ImportSettingsPanel::open_paths`).

### As built (ADR-7472, inspector asset fields and removal)

§ 2 Inspector: asset refs now have drag-drop, thumbnail and picker. The runtime asset-path fields carry
`@AssetFilter` (`MeshSource.mesh` / `.material`, `AudioEmitter.sound_name`, `MaterialGraphRef.path`,
`ClothSimSource.asset`, `SoftSimSource.asset`, `DestructibleSource.asset`, `ProcGenerator.graph`,
`VideoPlayer.source`; attributes only, no format change). An asset field draws its thumbnail, takes a Content Browser
drop (one undo step, the same `SetProperty` as `editor.set_property`; a wrong-kind drop is refused with its reason
under the field) and has a search picker with thumbnails; the `material` filter takes `.material.ron`,
`.matgraph.ron` and `.matinst.ron`. Agent read: `editor.asset_field_choices`. With one entity selected every removable
component has its own Remove (header context menu and a button in its body). Content Browser New gains Attribute set,
Effect and Ability. Agent page: `book/agents/inspector.md`.

### As built (ADR-7593, viewport Show flags, navigation preview, gizmo drags)

§ 2 Viewport show flags: the toolbar's **Show** menu now offers **Colliders**, **Light radii** and **Post volumes**
beside Navigation, each one `editor.set_viewport` flag (`colliders`, `light_radii`, `post_volumes`). **Post volumes**
outlines every post volume's shape and blend band in its priority color from `regia_render::post_volume_outline`, the data
`render.post_volume_gizmos` returns (ADR-2323), in the live panel and in captures. **Show ▸ Build navigation preview**
and `editor.nav_preview { action: "build" | "clear" }` bake an editor-only nav mesh from the level's static collision
with the level's `LevelStreaming.nav` settings (else the `regia_nav` defaults) into the `NavMesh` resource Show ▸
Navigation draws, and turn that flag on; it is never saved and is no undo step, and a game's own `NavMesh` is never
replaced. Box select and camera drags that start away from the transform gizmo's handles now work with the gizmo shown.
Agent page: `book/agents/viewport-flags.md`. The preview belongs to the level it was baked from: once another level
opens it no longer draws and the method drops it, and a level whose terrain alone is too big for one nav build is refused
before any geometry is read (`nav_preview_too_large`, ADR-7630). Not built: a background (non-blocking) bake for big
levels.

### As built (ADR-7664, editor worker jobs: import settings Apply, navigation preview)

§ 7 Long operations: Content ▸ Import settings ▸ Apply (and `editor.import_settings_apply`, `editor.set_import_settings`)
and Show ▸ Build navigation preview (`editor.nav_preview { action: "build" }`) run as cancellable editor jobs
(`import_settings:<n>`, `nav_preview:<n>`) on worker threads, with progress in `agent.job_status`, a background-task row
(`import_settings`, `nav_preview`) and a Cancel in the window or the Show menu; the editor thread applies each job's end
(`regia_editor::editor_jobs::tick`, every frame). Cancel leaves the old `.meta` and cooked output, or the old preview. A
second Apply or bake while one runs is refused (`import_settings_apply_running`, `nav_preview_busy`). Both import-settings
paths invalidate the content index on success. The methods take `async: true` (answer `{ job }` at once, like
`editor.import`) and otherwise wait and name the job. The navigation preview's early size check covers the static
colliders that feed the bake as well as terrains. Agent pages: `book/agents/import-settings.md`,
`book/agents/viewport-flags.md`.

Amended (ADR-7663, reflection capture follow-ups): every copy point (Edit ▸ Duplicate, Copy/Paste, the gizmo's
Alt-drag duplicate, the Outliner's Duplicate) gives a copied `LocalCapture` an id no other capture uses (a Cut then
Paste keeps its own); a capture moved, resized, deleted or added while Tools ▸ Bake reflection captures runs on the
same level cancels the run with `bake_capture_changed` and writes nothing; the "reflection probe bounds" gizmo of § 3
is editable: under the viewport's handle tool (the collider tool's toggle) the influence box faces and the blend
margin have drag handles, one undo step per drag (`editor.capture_handles`, `editor.capture_handle_drag`); and the
editor viewport draws the level's baked captures as a game does (the run's maps, or the `.fcap` the level names, placed
where their bake matches, with the renderer's capture selection run from the editor camera). See
`book/agents/reflection-captures.md`.

Press routing (ADR-7674): the viewport drives the transform gizmo through the crate's `update` and `draw` without its
cursor widget, so each press has one owner decided at press time (a gizmo handle under the pointer, else the pane and its
tools); every pane tool gets its press with the gizmo shown in any pane.

### As built (ADR-7677, UI designer preview size, theme and slots; Localization exchange)

§ UI designer: the preview's toolbar has **size** (width, height, 1..=8192 px) and **safe area** (left, top, right,
bottom) fields beside the size presets; the insets go through the runtime's `SafeArea` limit. With **Themed** on, a
theme picker offers the active theme and every loaded theme (built-ins and `assets/ui/themes`). Size, insets and
theme are the open document's view state (`PreviewSettings`): never in the `.ui.ron`, not undo steps, the same
settings `editor.ui_designer_preview { size, safe_area, theme }` sets. An instance's inspector lists the component's
slots with what fills each one: **Set…** (one palette node), a palette chip dropped on the row (appends) and
**Clear**, one undo step each, the same function as `editor.ui_designer_set_instance_slot`.
§ Localization panel: an **Exchange** row (locale, `po`/`xliff`, `all`/`untranslated`/`stale`, the file with a picker)
with **Export** and **Import…**; Import shows the keys it adds, changes and still misses and the skipped entries
before **Apply** writes them as one undo step. Both call `regia_asset_pipeline::loc_exchange`, the code `regia loc
export|import` runs, so the bytes match the CLI's. Agent: `editor.loc_export`, `editor.loc_import { dry_run? }`.
Built since (ADR-7744): the designer's preview size, safe-area insets and theme are kept across editor sessions in
`.regia/editor_view.ron` (`ui_designer`, `view_state.rs`; editor state, not a content format) and a document opened
later starts with them.

## 10. As planned (ADR-7752, UI parity)

Planned for v0.6.0 ("editor complete"); nothing here is built at the time of writing. Each item names the w12 job that
builds it (`3202` is job `w12-layouts-workspaces`, and so on; the list is in ADR-7752). Every item ships with an
agent method, a `book/agents` page, a `book/COVERAGE.md` row and keyboard access. A method written `editor.name` that
is not in the editor-method snapshot yet is a planned name; the building job sets the final name and records it on its
agent page. Rules for all of it:

- One undo stack: an edit to a saved document is one or more `EditorCommand`s on the shared `UndoStack` (§ 1).
  View-only state (camera, filters, hidden-in-editor, locks, layouts) is never on it and never dirties a document.
- One code path: a panel control, its palette command, its menu item, the console and the agent method call the same op.
- Bounded per-frame work: a closed or idle panel does nothing per frame (counted tests, `idle_repaint`).
- New per-user files live in the user config directory, new per-project shared files under `config/`, and per-user
  per-project view state under `.regia/`.
- Accessibility: reachable and operable from the keyboard, scales with `appearance.ui_scale`, colors from theme tokens
  only, an accessible name on every icon-only button, US English; a test per new panel proves the keyboard path.

### 10.1 Shell: layouts, workspaces, windows, status

- **Named layouts** (3202). Save the dock arrangement under a name; list, switch, rename and delete saved layouts; set
  one as the personal default; reset to the built-in layout. Each is a file in the existing layout shape.
  `editor.layout_save`, `editor.layout_apply`, `editor.layout_reset` grow a `name`; `editor.layout_list` (planned).
- **Workspaces** (3202). A workspace is a named layout plus the panels it opens and an optional editor mode. A switcher
  in the main toolbar, View ▸ Workspace and one palette command per workspace change it without closing documents.
  Built in: Level, Animation, Materials, UI, World, Cinematics, Debug. User workspaces are layouts flagged as
  workspaces. `editor.workspace_set` (planned).
- **Maximize and full-screen viewport** (3202). `view.maximize_panel` maximizes the focused tab to the whole dock area
  and restores the exact previous tree; `view.fullscreen_viewport` shows the active viewport alone. Esc stops a running
  play session before it leaves full screen. `editor.command` runs both.
- **Pane management** (3202). Each dock leaf has an add-tab button and a "Replace with" menu; a tab bar whose tabs
  overflow offers a list of all its tabs; dragging an item over a tab activates it after a dwell counted in frames. The
  main window's monitor, size, position and full-screen state, the open level and the open asset-editor tabs are
  restored per project. **Editor feature profiles** hide feature areas, panels or entity kinds from menus and Create; a
  profile is a shareable `.editor_profile.ron`, active per project. `editor.pane_add`, `editor.pane_replace`,
  `editor.profile_set` (planned).
- **OS windows and panel instances** (3203). A floated panel can be its own OS window (pref `ui.floating_windows = os |
  in_window`; `in_window` is single-window mode). Floated windows share the device, the egui context and the world; a
  window whose monitor is gone re-docks. Outliner, Content Browser and Inspector can have several instances, each with
  its own filter, folder and lock state (`editor-layout` v3, ADR-8105 (a), ADR-8145: tab ids `<panel>#<n>`, n 1 to 7,
  and an `instances` map of each instance's state). `editor.layout_get` reports windows and instances;
  `editor.panel_new_instance { panel }` opens one (Window ▸ New Outliner / Content Browser / Inspector).
- **Status bar** (3204). Clickable indicators: unsaved documents (opens Save all), background jobs (a list with
  progress and Cancel), trace recording, problems count, revision-control state (filled by 3257). `editor.status_get`
  (planned).
- **Drawers** (3204). A content drawer and a log drawer slide over the dock area from the bottom edge, close on focus
  loss or Esc, and can be docked as normal tabs. `editor.drawer` (planned).
- **Console and quick settings** (3204). The log drawer has a console line that runs command ids, agent methods with
  JSON parameters and project console variables, with history and Tab completion from the agent schema. A quick-settings
  menu in the main toolbar gathers snapping, realtime, preview light, autosave and the default play mode.
  `editor.console_run`, `editor.console_complete` (planned).

### 10.2 Search, commands, keys, settings, help

- **Universal search** (3205). One window (Ctrl+T) searches assets, entities, commands, menu items, settings and agent
  methods; results are grouped by provider with counts; prefix tokens (`a:`, `e:`, `c:`, `s:`, `m:`) limit providers;
  moving through results previews (select-and-frame without undo, an asset thumbnail, a setting highlight); Enter
  commits; Esc restores. `editor.search` (planned), next to `editor.content_index` and `editor.command`.
- **Saved searches and completion** (3205). Queries are saved per user or shared per project
  (`config/saved_searches.ron`); Tab completes prefixes, kinds, tags and component types. `editor.search_save`
  (planned).
- **Palette and history** (3205). Palette entries show their menu path and shortcut; menu items appear in the palette
  automatically; entries can be pinned to a favorites popup. Alt+Left and Alt+Right walk back and forward through
  recent selections. `editor.selection_history` (planned).
- **Keymap** (3206). Search by pressing a chord; a keyboard map view; named presets (Default, Laptop) and user presets;
  import and export are kept. `editor.keymap_get`, `editor.keymap_bind`, `editor.keymap_import`,
  `editor.keymap_export` take a `preset`.
- **Scoped shortcuts** (3206). A binding has a scope (global, viewport, outliner, content, graph canvas, timeline,
  text); the focused panel's scope wins over global; the safety keys (stop play, save, undo, redo) are global and
  cannot be shadowed. `editor.keymap_bind` takes `scope`.
- **Tooltips and help** (3206). Menu items, palette entries and Inspector fields show their description, default
  value, shortcut and agent method or property path. F1 opens the book page for the focused panel or field.
  `editor.help_open` takes a `topic` or the focus.
- **Settings** (3206). One Settings window with Preferences and Project Settings tabs sharing a search box; changed
  values are marked and have Reset per key and per category; Preferences export and import. `editor.prefs_get`,
  `editor.prefs_set`, `editor.prefs_reset` (per key and per category), `editor.prefs_export`, `editor.prefs_import`.

### 10.3 Viewport camera and navigation

- Invert options for orbit, pan, fly look and wheel; a speed slider tied to wheel-while-flying; speed scaled by
  distance (pref) (3207). `editor.set_viewport`, `editor.prefs_set`.
- Orbit around the selection without framing it (pref) (3207).
- Six orthographic views; zoom toward the cursor (3207). `editor.view_preset`.
- Navigation presets: default, three-button-mouse emulation, number-row-as-numpad (3207).
- Camera to selection and selection to camera; per-viewport field of view, near and far planes; 1, 2, 3 and 4-pane
  layouts (3207). `editor.viewport_instance_set`, `editor.viewport_layout` (planned).
- **Pilot** (3208): lock the editor camera to a selected camera, light or entity; moving the view moves it (one undo
  step per session); Eject ends it; cameras use their own projection. `editor.set_viewport` `possessed`, `editor.pilot`
  (planned).
- **Camera picker and picture-in-picture preview** of the selected camera, rendered only while a camera is selected
  (3208). `editor.pip_preview` (planned).
- **High-resolution screenshots** (a multiplier or a size, tiled past the device limit, helpers on or off) and
  **composition guides** (thirds, center, safe areas, letterbox) (3208). `editor.capture_viewport` takes `scale` and
  `helpers`; `editor.set_viewport` takes `guides`.

### 10.4 Gizmos, snapping, selection, placement

- **Reference space** World, Local, Parent or View, with a toolbar toggle and a key (3209). The gizmo is world space
  only before this; `editor.set_gizmo` takes `space`.
- **Pivots**: median, individual, active, bounds center and a temporary pivot offset; the rotate gizmo's view-axis
  ring; a select-only tool (Q) (3209). `editor.set_pivot_mode`.
- **Snap step menus** for grid, angle and scale; surface snapping during a translate drag with optional normal
  alignment and offset; a fixed-step rotate key (3209). `editor.set_snap`, `editor.placement_options`.
- **Lasso selection**; add, subtract and toggle modifiers for box and lasso (3210). `editor.box_select` takes `mode`;
  `editor.lasso_select` (planned).
- **Select similar** by kind, components, asset or layer (3210). `editor.select_similar` (planned).
- **Pick from the stack** under the cursor (Alt+click list); a **selection-root** marker so a click on a child selects
  the marked ancestor; hover pre-highlight from the existing id buffer (3210). `editor.pick_stack`,
  `editor.set_selection_root` (planned).
- **Live ghost preview** while dragging an asset into the viewport; a **measure tool** for distances and angles (3210).
  `editor.measure` (planned).

### 10.5 Viewport display and debug

- A per-pane **view-mode menu** (lit, unlit, wireframe, lit plus wireframe, lighting only, light complexity, overdraw,
  buffer views) using the renderer's existing debug views (3211). `editor.set_viewport` `view_mode`,
  `editor.capture_viewport` `debug_view`.
- Per-kind **icon filters** with size and opacity; a realtime-off badge; warning badges on non-default pane settings
  with reset; fixed exposure; screen percentage (3211). `editor.set_viewport`.
- A **Rendering Debug** panel (view modes, per-entity render overrides, pass timings); **hide-in-editor** (H), unhide
  all, isolate selection (Shift+H): editor-only, per user, never dirtying; the Outliner eye keeps saving visibility
  (3211). `editor.set_render_override`, `editor.hide_in_editor` (planned).
- A shared **preview scene** for asset editors (lighting presets, environment intensity, floor, auto-rotate) and mesh
  helper actions: add simple collision through import settings, generate lightmap UVs, navigation preview (3212).
  `editor.preview_scene_set`, `editor.nav_preview`.

### 10.6 Outliner

- Rename in place (F2); expand and collapse all; recursive expand (3213). `editor.outliner_rows`, `editor.rename_entity` (planned).
- A **query language**: name contains, `kind:`, `type:`, `tag:`, `has:`, property comparisons, `under:` and `parent:`,
  `and`, `or`, `not`, parentheses; today's tokens keep working; errors are shown inline (3213). `editor.hierarchy`
  takes the query.
- Select all, range and rubber-band select act on matching rows only; **folders** are a saved `EditorFolder { path }`
  component drawn as virtual rows (3213).
- Paste as child, paste at the original position, duplicate with auto-incremented names; full keyboard navigation
  (3213).
- Configurable **columns**: kind, layer, prefab source, tags, revision state (3214). `editor.outliner_set`.
- **Configuration warnings**: a registry of checks per component type with a fix hint, row and ancestor badges and
  Problems entries; checks run on change only (3214). `editor.config_warnings` (planned).
- Script, logic-graph and sequence-binding badges that open their editors; tag the selection; row colors; change an
  entity's kind in place keeping matching fields (3214). `editor.change_kind`, `editor.tag_selection` (planned).
- A **unique-name** marker with a runtime lookup by name (3214).

### 10.7 Inspector

- **Property search** (fuzzy) and a modified-only filter (3215). `editor.inspect_selection` takes `query`, `modified`.
- Field **categories** from `@Category`; **pinned fields** per component type (a user file) (3215).
- **Lock** to an entity; pinned inspector windows; a searchable, categorized add-component picker; a component filter
  (3215). `editor.inspector_lock` (planned), `editor.add_component`.
- **Field context menu**: copy and paste value, copy as RON, copy agent path, reset to default; component copy and
  paste and paste as new component (3216). `editor.reset_property` and `editor.copy_value` (planned),
  `editor.set_property`.
- `@EditCondition` (grey or hide by other fields), an enable checkbox for optional groups (`@Toggle`), a raw view
  (3216).
- Lists with drag reorder, duplicate, count and clear (3216). `editor.set_property` on list paths.
- Number fields accept **expressions and units**; Ctrl snaps, Alt speeds up, labels scrub (3217).
- Linked vector components; flags as checkboxes and named bit-layer grids with drag paint; file and folder picker hints
  (3217). `editor.physics_layers_edit`.
- **Inspector buttons** (`@Button` or registered component actions) that run with undo (3217).
  `editor.inspector_action` (planned).
- Asset fields: browse to, use selected, clear; entity fields: an eyedropper from the viewport or the Outliner (3218).
  `editor.asset_field_choices`.
- Inline sub-asset editing and make unique; a shared-asset indicator with the referencer count (3218).
- Inline **curve and gradient** fields with a popover editor; a full **color picker** (RGB, HSV, perceptual, hex,
  eyedropper, swatches, recent, HDR intensity) (3218).
- **Bulk Edit**: a grid of selected entities or assets by component fields with sort, filter, pinned columns, TSV copy
  and paste, fill down, one undo step per commit (3219). `editor.bulk_edit_get`, `editor.bulk_edit_apply` (planned).

### 10.8 Content browser, import, problems

As built (ADR-8201): the folder tree also has a read-only **Library** root in the user
editor config directory, outside projects. **Add to library** copies an asset and its
dependencies using asset packages; **Use in project** copies with fresh paths and IDs
and remapped references, never links. `editor.asset_library {action: list|add|use}`
exposes those actions. Typed search and filter chips narrow a shown collection.

- A collapsible **folder tree**, breadcrumbs, back and forward, favorite folders (3220). `editor.content_view`.
- **Filter chips** for kinds, tags and states (modified, unreferenced, has problems, review pending) and matching
  tokens; metadata search (3220). `editor.content_index` takes the filters.
- **Collections**: static lists and saved queries, personal or shared (`config/asset_collections.ron`) (3220).
  `editor.collection_edit` (planned).
- **Multi-select and bulk actions** (validate, re-import, save, move, delete, add to collection) (3221).
  `editor.asset_bulk` (planned).
- A **Save all** dialog listing every dirty document, used on close and on level switch (3221).
- **Delete with "replace references with"**; retarget missing references; one move behavior for the UI and the agent;
  a rich tooltip (3221). `editor.delete_asset` takes `replace_with`; `editor.move_asset` and `editor.rename_asset`
  share the path-rewrite rule.
- **OS file drops** import into the browser and import-and-place in the viewport, always copying into the project
  (3222). `editor.import_files` (planned).
- Thumbnail refresh and capture from the viewport; animation previews (3222). `editor.asset_thumbnails`.
- **Export assets with their dependencies as a package** and import a package, refusing id clashes; an Asset Jobs
  panel; a fresh-clone first import with no false errors (3222). `editor.package_export`, `editor.package_import`
  (planned), `editor.cancel_job`.
- **Import presets**, apply-to-all for multi-file imports, a 3D import dialog with preview and per-mesh, material and
  clip options (extract materials, split clips by frame range) and an optional generated read-only prefab (3223).
  `editor.import_settings_*`.
- **Validation before commit**: `regia assets validate --staged` and `regia hooks install` (3224).
  `editor.asset_validate`.
- Log and Problems **grouped by source** with duplicate collapse and entity links; a **Project Health** panel ranking
  the existing checks with recommended actions (3224). `editor.problems_panel`, `editor.project_health` (planned).

### 10.9 Prefabs, level variants, several levels

- An **Overrides window** per instance: prefab and instance values side by side, revert and apply per row or all, an
  explicit apply target (nested, outer, variant, base) (3225). `editor.prefab_override`,
  `editor.prefab_apply_override` take `target`.
- **In-context prefab editing** with the level visible, dimmed and not pickable (3225). `editor.prefab_mode`. Variant
  edit mode stays out of scope (spec 83 § 9).
- **Level variants** (3226): a level that names a base level and stores only differences; base changes flow through;
  chains with loop detection; runtime and cook support (a new format, `.levelvariant.ron`). `editor.level_variant`
  (planned).
- Layer locks; a layer stored in its own file; several levels open at once with their own dirty state, undo scope and
  an active level for new entities (3267). `editor.layers_panel_set`, `editor.open_level`, `editor.active_level`
  (planned).

### 10.10 Graph editors

- A material **parameters panel** with types, ranges, categories and order (a `.matgraph.ron` row in the format-bump
  plan) (3227). `editor.matgraph_edit`.
- Material **instance reparenting** and an inheritance tree (3227). `editor.matgraph_instance_open`,
  `editor.matgraph_instance_reparent` (planned).
- Per-profile **shader statistics** (instructions, samples, bindings), a quality-tier preview switch, a node context
  menu with find usages (3227). `editor.matgraph_compile`.
- A **WGSL code node** with typed inputs and outputs, validated before any pipeline, with a code view that shows errors
  at their line (3228). `editor.matgraph_edit` `code` node. This is validation feedback, not code editing (ADR-7752
  scope decision (a)).
- **Logic graph**: fuzzy palette search filtered by the dragged wire; a Variables panel (3229). `editor.logic_edit` (on the logic-graph branches that land with the v0.6 integration).
- **Logic graph debugging** in the panel: rule and event breakpoints with a list (per user, never in the asset or a
  shipping build), step to the next rule evaluation, hover values, a watch list, firing wires pulse (3229).
  `logic.debug`, `logic.control`.
- Evaluate a condition expression while paused; graph issues in the editor-wide Problems; open a prefab's logic graph
  from prefab edit mode (3229). `editor.logic_validate` (same branches).
- **Structural diff** of assets and graphs against a git revision, side by side and read-only (3230).
  `editor.asset_diff` (planned).
- A shared **graph toolkit** for search popups, comments, reroutes, badges and previews (3231); the panels' behavior and
  tests do not change.

### 10.11 Animation, sequences, effects, audio

- Live parameter writes to the watched animator in play; a record-and-scrub animation debugger (3232).
  `editor.anim_state`, `editor.anim_record` (planned).
- Clip preview speed, loop and ping-pong, a trimmed preview range, root-motion floor travel, onion skinning (3232).
- Multi-curve views; clip search and sort (3232).
- A **retargeting panel**: auto chain mapping, an editable map, rest-pose alignment, a side-by-side preview and batch
  retarget (3233). `editor.retarget_edit`, `editor.retarget_batch` (planned).
- **Event tracks** that fire named events with payloads (a `.seq.ron` row in the format-bump plan); track lock, mute and
  solo; track context menus (3234). `editor.seq_track_edit`.
- A **dope sheet** for UI animations (3234).
- An effect system overview and timeline; a bounds readout (3235). `editor.vfx_edit`.
- Mixer effect chains, bypass, sends and snapshots (3235). `editor.audio_mixer_set`; built (ADR-8354):
  `editor.audio_mixer_chain`, `editor.audio_mixer_send`, `editor.audio_mixer_snapshot`, `editor.audio_mixer_save`
  (post-fader sends only).
- Project debug-draw colors; a navigation path debug view; debug flags work in the play view (3235).

### 10.12 World tools

- Terrain brush size, strength, falloff and a falloff curve (3236). `editor.terrain_brush_set` (planned).
- Terrain region copy, paste, export and import; kit painting on a grid by floor with line fill (3236).
- **Non-destructive terrain edit layers** (toggle, order, lock, opacity, flatten) baked flat at cook; the cooked map
  does not change (3237). `editor.terrain_layers` (planned).
- A **world streaming map** to load and unload cells in the editor, with save prompts (3238). `editor.world_cells`
  (planned).
- **Tile tools**: line, rectangle, bucket, pick, random brush, saved patterns, autotile connect and path modes, layers
  as tree rows, editable object layers, 2D rulers and guides with smart alignment (3239). `editor.tile_stroke`, `editor.tile_stamp`.
- **Blockout brushes** combined by constructive solid geometry and baked to a mesh (3240). `editor.blockout_edit`
  (planned).

### 10.13 Play, devices, live editing

- Play from the player start, the editor camera or a chosen player start; time scale and mute (3247). `editor.play`.
- Play-mode tint and role colors; a server and client view switch that the Outliner, Inspector and logs follow (3247).
- **Device simulation** in the play window: frame and notch, safe area, touch emulation, a virtual gamepad,
  orientation (3247).
- **Separate-process play** shown in dock tabs with forwarded input, pause, step, speed and mute (3248).
  `editor.play_process` (planned).
- The Outliner and Inspector can show a launched or device game through the runtime scene methods; a live link mirrors
  edit-world edits into a running session by entity id (3249). `editor.remote_source` (planned).
- Asset push to a launched game; a fast-iterate Android launch that reads assets from the editor host (dev builds,
  opt-in); device screen mirroring with tap forwarding (3250). `editor.asset_push` (planned). Device steps run on the
  test Android device only.

### 10.14 Profiler and debug tools

- Percent-of-frame display; per-subsystem modules (audio, physics, UI, rendering) (3251). `editor.profiler_state`.
- Monitors for any named counter; per-asset load times; event marks on the history graph and the timeline (3251).
- **Memory snapshots** with diff (categories with current and peak bytes, entities per archetype and component, assets
  per kind; JSON export); a **GPU resource browser** (textures, render targets and the allocator's other allocations,
  largest first) with previews of a chosen mip and channel (3620, ADR-8142). `editor.memory_snapshot`,
  `editor.gpu_resources`.
- A **frame debugger** listing passes and draws with output previews, through public renderer APIs (3253).
  `editor.frame_debug` (planned).
- A debug overlay for the selected entity; a visual log recorded per entity with a scrubbable timeline (3254; a
  feature-gated runtime API).

### 10.15 Undo, history, recovery

- An **Undo History** panel with jump-to and per-entry details (3255). `editor.undo_history` (planned).
- **Transactions**: `editor.transaction_begin`, `editor.transaction_commit`, `editor.transaction_abort` (planned) and a Rust
  transaction API; scene edits from the hosted agent go on the editor stack; per-panel stacks merge into the shared one
  (3255).
- **Safe mode**: start without extensions, Lua mods, recovery restore or tab restore (3255).
- **Kept versions** of levels and documents with preview and restore; a project backup before an upgrade; project
  duplicate (3256). `editor.versions` (planned).

### 10.16 Revision control and collaboration

- **Git through its command-line tool** (argument vectors only, no credentials handled, no new dependency): status
  badges on assets, the level row and the status bar (3257). `editor.vcs_status` (planned).
- A **Revision Control** panel: stage, unstage, discard, diff (structural for assets), commit with validation,
  history, file history, branches (3257). `editor.vcs_*` (planned).
- **Lock on edit** for projects that use git LFS locking; advisory when unavailable (3258). `editor.vcs_lock`
  (planned).
- **Review notes** pinned in the world (saved with the level, stripped from cooked builds) and an **Activity**
  timeline of agent calls and edits (3259). `editor.review_note`, `editor.activity` (planned).
- **LAN presence** between editors (cameras, selections, jump to a participant), off by default, each joiner accepted;
  edit replication is designed in ADR-7752 for a later wave (3260). `editor.presence` (planned).

### 10.17 Build, launch, extensions

- Named **launch profiles**; a **Platforms** panel showing the platform checks with fixes; a package size report for
  every target and unused-feature hints (3261). `editor.launch_profile`, `editor.platforms_check` (planned).
- Extension commands with default shortcuts in the palette and keymap; typed extension settings in the settings window
  (3262).
- Menu and context-menu entries with selection predicates; viewport tools and overlays registered by extensions
  (3262).
- Before-cook, after-package and before-launch hooks that can add files, log or abort (3262).
- **Utility panels**: a UI document plus a logic graph or Lua driver shown as a dock panel, calling a safe subset of
  editor actions (built by w13 3621, ADR-8189: `.editorpanel.ron` sidecars, Window ▸ Utility Panels, the starter
  sample "Rename selection with prefix"). `editor.utility_panels`.

### 10.18 Accessibility and theming

- Saved custom editor themes with token and accent editing, import and export; a high-contrast theme; a separate text
  scale; diagnostic color preferences; color-vision simulation for the editor chrome and the viewport; an authoring
  guide for accessible editor panels (3264). `editor.theme_edit`, `editor.prefs_set` (planned names). The OS
  screen-reader bridge for the editor is w11 job 3120.

### 10.19 Project browser, samples, agents

- Project thumbnails, tags and search; level templates (empty, basic lit, interior, open world, project templates)
  (3265). `editor.project_browser` (planned).
- A **samples browser** that copies samples, templates and extension examples into a new folder (3622, ADR-8190).
  Help ▸ Samples…; `editor.samples { action: list | create }`. The project's close and Save all save a viewport
  thumbnail that the project browser rows show.
- An **Agent panel** that starts a user-configured agent command connected over the agent protocol, shows its
  transcript, and runs it in Ask-only, Propose (staged review) or Act mode, with plan approval; the editor contacts no
  service by itself (3266). `editor.agent_panel` (planned).

### 10.20 Scope decisions

See ADR-7752: code is edited in the user's IDE (§ 5); the editor is desktop only; play starts from an in-memory copy
(§ 4); there is no asset marketplace; revision control is git; and in-editor polygon modeling, rig posing in the level
viewport, a node-based procedural audio graph and editor UI localization are deferred, each needing its own spec first.

### Frame Debugger checkpoint slice (ADR-7824, proposed; P-310)

View → Frame Debugger captures the main viewport's active pane on its next render,
freezes its scene image and lists the public graph in order with inputs, outputs,
declared pipeline keys, counted mesh draws and GPU timings. Tab and the pass list's
Up/Down + Enter/Space path operate it; Resume releases the capture. The public
FramePass slots supply color and depth previews labeled by their actual checkpoint
and last recorded pass. Unsupported targets remain explicitly unavailable. The
forward renderer has a base-color debug mode, no albedo G-buffer. Per-draw identities
and targets between the public slots remain deferred pending renderer hooks reviewed
by the lead. No frame/graph core edits or persistent format changes are part of this
slice. Agent: `editor.frame_capture`, `editor.frame_pass`; book: `agents/frame-debugger.md`.

Draw records (ADR-8224, amends ADR-7824): a capture with pass copies also records its frame's
draws one by one in the depth prepass and forward draw loops (pass, named pipeline,
mesh/material/object keys, index/vertex/instance counts, indirect, target). The inspector
lists them under their pass with Previous draw / Next draw and a keyboard draw list;
selecting a draw shows its record and selects its entity in the Outliner. No pass replays
its draws, so a draw has no partial image (`draw_replay_unsupported`). Frames without a
capture record nothing. The in-process play window is a capture source (`source:
"play_window"`; its draws name play-world objects, not edited entities). Agent:
`editor.frame_draw`, `editor.frame_capture` `source` and state `draws`.

### Build and launch parity (w12 § 19, ADR-7843)

The launch bar selects named project recipes managed by Launch Profiles; v1 data is in
`config/launch_profiles.ron`. Window → Platforms runs doctor checks as an editor job with
fix text and a target warning on the launch bar. Window → Size Report displays measured
published contents and largest files, including desktop pak asset kinds. The panels use
keyboard-focusable egui controls and inherited appearance scale/theme. Agent routes and
execution limits are in `book/agents/editor-launch.md`. Android override/reuse policy and
plugin execution telemetry are deferred explicitly (ADR-7843); absent telemetry never
produces unused-feature claims or changes cargo flags.

### Review notes and Activity (ADR-7841, v0.6 parity P-337/P-335)

Window → Notes lists saved review markers (open/resolved/mine), replies and resolve/reopen actions.
N is rebindable and opens a draft at the viewport surface or attached to the selected entity. Pins
use theme tokens and count the note plus replies. Each note is a dedicated reflected marker, saved
through the existing component map and stripped with its subtree from cooked/package output, not
from source levels. Author: git user.name, then review.author. Window → Activity combines observed
audit calls and session undo edits in sequence order; source/method filters and live target selection
have keyboard routes. Older audit rows have unknown targets. Contracts: book/agents/review-notes.md.

## LAN collaboration presence (v0.6.0, ADR-7842; UI parity § 18.5)

The Collaboration panel opens from the command palette. Host a session on an explicit numeric
LAN address and port; join by that address; leave to close the socket and remove local indicators.
Hosting and joining are off by default. The host accepts or rejects each pending join separately.
Pending users cannot see or publish presence. No discovery, internet relay, account or automatic
reconnect is provided. Use a trusted LAN: display names are user supplied, and traffic is not encrypted.

Accepted participants share their name, assigned theme color, the main viewport's active camera
pose/view kind and selected scene GUIDs. The panel lists participants and pending requests;
the viewport draws camera gizmos and selection silhouettes, and Outliner rows show named color
dots for matching GUIDs. Jump to moves the main active viewport to the chosen participant's view
without changing the local selection, content or undo stack. Names supplement color throughout.
The existing viewport Outline switch and game-view suppression also control collaboration overlays.
Peers should open the same saved level: presence does not copy levels, assets or edits, and unknown
GUIDs simply have no local indicator. Graph editing stays single-user (spec 67 § 10).

Snapshots run at most every 100 ms, without catch-up bursts. Limits: eight participants including
the host, seven pending joins, names 1–64 UTF-8 bytes without controls, 64 selected GUIDs,
60 KiB/datagram, at most 32 incoming datagrams per update. Larger selections publish the first 64.
Join nonces and host session epochs separate reconnects; strictly increasing sequences reject
stale snapshots. Acceptance binds a name and id to an endpoint and nonce. Leave updates the roster;
five seconds of silence expires a peer, and host silence clears the client's roster. A rejected
or expired client must explicitly join again. No pending peer gets camera/selection snapshots.
Each update reads queued datagrams before it judges silence, so a local hitch does not expire a
peer whose newer packets are waiting; stale sequences never refresh a peer. A join with a new
nonce from the address of a pending or rejected request replaces that request (new id, explicit
acceptance again); an accepted participant's address ignores other nonces.

All controls support Tab and Enter/Space, use theme tokens and follow `appearance.ui_scale`.
UI effects are guarded by `cumulative_frame_nr`. Headless and GUI editors use the same session
pump; a deterministic loopback test advances two edit worlds and sees both camera/selection states
within three eligible updates, checks removal and jump, and refuses a third user's presence until
accepted. Agent surface and errors: `book/agents/collaboration.md`.

Later-wave edit conflict contract: ADR-7842 specifies the host sequence, atomic per-entity soft
lock acquisition, revision preconditions, deduplication, refusal/rollback, undo compensation and
host-loss behavior. Edits travel through host agent dispatch and are replicated only after commit.
This wave implements none of those edit operations, and a presence packet cannot invoke dispatch.
Remote selection colors occupy an outer silhouette ring; co-selected entities alternate their
remote participants' colors, while the existing local selection outline stays visible inside it.

As built (ADR-7860, proposed): Capture frame also asks the renderer for read-only copies at
every built-in pass boundary of the captured frame only, so each pass row shows the frame
targets (`color`, `depth`, `output`) its record site wrote, labeled by the site's last writer.
Pass-internal resources and copies over the capture's byte budget are listed as skipped with a
reason. Agent: `editor.frame_capture { action: "start", passes?, budget_bytes? }`.

### Help and settings follow-up (ADR-8196)

Registered menu items use the same rich-tooltip builder as the palette. Inspector label hit
targets supply hover or keyboard-focus context before F1 dispatch; rendered book links preserve
the section anchor. Content Browser search retains and displays token candidates for repeated
Tab cycling; Shift+Tab leaves the field. `editor.content_index { complete }` exposes the same
vocabulary without requiring a project scan.

Settings compares modeled project keys with the defaults in ADR-8196 and offers per-key Revert
and category Reset through `editor.project_settings_set { reset_keys | reset_category }`.
Saved calls create one shared undo step and preserve unknown keys. Project identity and keys
without a documented generic default report null default/changed metadata and remain excluded
from reset until their owners define defaults; this limitation is explicit on the settings page.


### Workspace mode and area-control follow-up (ADR-8197, proposed)

Workspaces select an optional World, Terrain, 2D tiles, Animation or UI design mode, with
session mode overrides and existing tool palettes plus a related-panel toolbar. Terrain and
2D tiles have dedicated built-in workspaces; World keeps the general World mode. The existing
layout document shape stays unchanged. User mode overrides last for the session only.
Area menus support atomic join of neighboring leaves and swap of complete tab stacks.
There is no layout undo history in the current shell; these remain view-only operations.
Extension workspaces register a name, layout and mode through the editor context, with owner
lifecycle tracking and an extension mark. Contracts: book/agents/editor-layouts.md and
book/agents/editor-extensions.md.
