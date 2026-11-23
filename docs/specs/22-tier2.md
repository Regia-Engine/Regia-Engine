# Spec 22: v0.4.0 feature set

Each item becomes its own spec file (`specs/3x-*.md`) written by the building agent *before*
implementation, following the structure of the v1.0-core specs (model, API, editor, acceptance), and
reviewed against `01-cleanroom.md`. Below: scope, key decisions already made, and acceptance sketches.
Recommended order is the numbering.

> **Revised 2026-10-03.** T2.1 (VFX), T2.5 (terrain), T2.7–T2.9 (GI and high-detail geometry) and the
> GPU-driven parts of T2.12 moved into the v1.0 core for Direct Order 27 — see `specs/26` and milestones M13–M15.
> Their sections below remain as background. Multiplayer (T2.3) is lower priority because DO27 has its
> own netcode. Steam moved from parked into the v1.0 core (`specs/24`).

## T2.1 GPU particles / VFX (`regia_vfx`)
- Emitter assets composed of **modules** in stages: Spawn (rate, bursts, distance-based), Initialize
  (shape: point/sphere/box/cone/mesh surface/skinned mesh; velocity, lifetime, size, color, rotation),
  Update (gravity, drag, curl noise, vortex, attractors, size/color over life curves, depth-buffer
  collision, kill volumes), Render (billboard with flipbook + soft particles, stretched billboard,
  mesh, ribbon/trail, light emission (limited count)).
- Simulation in compute shaders; per-emitter particle buffers; GPU sort for alpha-blended (bitonic)
  on desktop, CPU-ordered emitters on mobile; indirect draws; LOD by distance and budget.
- Editor: module-stack panel per emitter, curve/gradient editors, live preview, stats (particles, GPU ms).
- Events: spawn on collision / death (GPU→GPU via append buffers), CPU-visible events limited.

## T2.2 Navigation and AI (`regia_nav`, `regia_ai`)
*Status (2026-11-01, ADR-2033):* navmesh, queries, crowd avoidance, behavior trees, perception, state trees and smart objects exist; off-mesh links and dynamic obstacles shipped in v0.2.8 (p2-navmesh-offmesh) and the world-scale navigation lane in v0.2.9 (nav-world).
- Navmesh generation: voxelisation → walkable marking → regions → polygon mesh from the walkable grid (contours are traced for display only; no detail mesh) (Recast-style,
  zlib licence — port permitted with attribution, or use the `rerecast` crate if its glam version is
  aligned by then). Tiled navmesh for large levels; runtime tile rebuilds for dynamic obstacles.
  *Multi-level (2026-11-16, ADR-8076):* `NavMeshBuilder::build_layered` (opt-in; the other builds keep the top
  surface only) keeps the walkable spans under the top one as layers (packed from a span graph, vertical walls close
  them, cross-layer portals), so tunnels, the ground under bridges and stacked floors get polygons. The
  tile cache and region rebuild are still layer 0 only. *Layers and climbs (W-19):* `build_with_nav_level_cap` fails when a column has more walkable levels than the setting (`build_layered` still drops); `NavAgentParams::pilot`, `hover` and `for_radius` bake one mesh per hull, a hover bake erodes an extra metre each side, `pilot_layer_links` adds ladder and deck-stair climbs for the pilot only, and `NavAreaCosts::passages` prices a tunnel at 0.75 and a flank at 0.20. The tile cache is still layer 0.
- Path queries: A* over polygons + string pulling (funnel); off-mesh links (jumps, ladders); area costs.
- Crowd/steering: RVO/ORCA-style local avoidance; `NavAgent` component (radius, height, speed,
  acceleration, avoidance priority).
- AI: behaviour trees (`.bt.ron`: selector, sequence, parallel, decorators: condition/cooldown/loop/
  timeout, services; value comparisons and observer aborts, ADR-4543 (a decorator with an abort is a gate
  checked on every entry and re-checked on a key change while it runs; trees at most 256 nodes deep, ADR-7559); a node that runs a state tree and
  a state-tree task that runs a behavior tree, ADR-4543; leaf tasks implemented in Rust via `#[derive(BtTask)]`), blackboard, perception
  (sight cone + line of sight, hearing events, team affiliation), editor BT graph with live debugging.

## T2.3 Multiplayer (`regia_net`)
*Status (2026-11-01, ADR-2033):* built across v0.2.5 to v0.2.10 as the `regia_net` crate: transport with reliability channels and encryption, replication with delta and relevance, prediction and reconciliation (b31, net-character), lag compensation (n2), lobbies and matchmaking (n1), sessions (b17), the dedicated-server package (server-package) and the network conditioner. Built after that note (gap audit 2026-11-05): Steam sockets and lobbies (mock-tested only), and PIE with N clients in one process (`pie_worlds`, lane pie-client-worlds, on the v0.2.15 candidate line; `book/agents/pie-net.md`). Not built or not verified: a QUIC transport, PIE as separate processes (lane `pie-separate-processes`, own tests green, unmerged), public-network and two-machine measurements, a real Steam run with two accounts. Book: `book/user-guide/networking.md` and the `net-*` agent pages.

*Status (2026-11-10, ADR-5628):* both alternative transports now exist: QUIC datagrams under the existing reliability layer (`regia_net::quic`, feature `net-quic`, off by default, ADR-3970, `book/agents/net-quic.md`) and Steam sockets (`regia_steam::sockets`, a `regia_net::DatagramSocket` over Steam's peer-to-peer messages, ADR-1877). Still not verified: PIE with N clients beyond `pie_net`, public-network measurements, and a two-account Steam session (owner-run).

- Topologies: dedicated server (headless Linux build: `MinimalPlugins` + sim plugins, no render/audio),
  listen server, client.
- Transport trait; default implementation: UDP with connection handshake (connect tokens), reliability
  & ordering channels (reliable-ordered, reliable-unordered, unreliable, unreliable-sequenced),
  fragmentation, packet acks bitfield, RTT/jitter estimation, encryption (ChaCha20-Poly1305) — design per
  public "Gaffer on Games" articles. Alternative transports (QUIC via `quinn`, Steam sockets) pluggable.
- Replication: `#[derive(Replicate)]` components (reflection-based field diffing), entity relevancy
  (distance/team/always), priority accumulators + per-connection bandwidth budget, delta compression
  against last acked baseline, quantisation hints via attributes, spawn/despawn of replicated prefabs.
- RPCs: server→client, client→server, multicast; reliable/unreliable.
- Client prediction + server reconciliation for `Character` (movement step is a pure function; input
  history buffer; replay on correction), snapshot interpolation for remote entities, lag compensation
  helper (server rewinds hitboxes).
- Tools: network conditions simulator (latency, jitter, loss, duplication), PIE with N clients, bandwidth
  profiler per entity/component.

## T2.4 Material graph editor
*Status (2026-11-01, ADR-2033):* `regia_matgraph` (compiler, agent methods) exists; the node-graph editor panel does not (the `.material.ron` form panel is not a graph canvas); the remainder is v0.4.0 (`docs/versions.md`).
*Pulled forward into the spec 27 programme, stage 5 (`specs/27` § 4.2).*
Node graph (egui-snarl) producing WGSL implementing the `material_surface` contract (`specs/13` § 7):
math, texture sampling, UV ops, time, world/view vectors, Fresnel, noise, parameters (exposed as
material params), functions (reusable sub-graphs), static switches → defines. Live preview + compile
errors mapped back to nodes.

## T2.5 Terrain and foliage
- Heightmap terrain, tiles of 255²–1023² samples, CDLOD (Strugar) quadtree LOD with morphing; GPU
  height sampling in vertex shader; splat-map material with up to 8 layers (triplanar option for
  cliffs), runtime virtual texture parked.
- Physics heightfield collider; holes mask.
- Editor: sculpt (raise/lower/smooth/flatten/noise/ramp), paint layers, import/export 16-bit PNG/RAW.
- Foliage: painted instances (density, scale/rotation jitter, slope/height filters) stored per tile;
  GPU instanced with per-cell culling and distance fade; impostors parked.

## T2.6 Cinematics timeline
`Sequence` asset: tracks (transform, property, animation clip with blending, audio, camera cut, event,
fade, sub-sequence), keyframe curve editor (Bezier tangents), playback API (`play`, `scrub`, `bind
actor slots`), render-to-image-sequence for trailers.

*Status (ADR-1614):* the playback API exists as `regia_framework::sequencer::SequencePlayer` over `.seq.ron` v1
and `.shots.ron`: play, pause, stop, seek (scrub), actor slots as `SequenceBindings` (scene guid or entity),
property tracks through a property-applier registry, events as `MarkerReached` messages, camera cuts and fades from
the shot list with a blended camera takeover; agent method `sequence.player`. Still to build: animation-clip,
audio and sub-sequence tracks (`.seq.ron` v2), the curve editor, and render-out driven by a sequence. *(All of these were built later; see the next status block. Kept as written for history.)*

*Status (ADR-1616, ADR-1880):* render-out from a sequence is `render.out_job` / `regia render --job` (ADR-1616);
`.seq.ron` v2 adds Bezier keys with tangents and weights, animation-clip tracks with blending, audio tracks and
sub-sequence tracks (ADR-1880); the editor's Sequence Curves panel edits float and vec3 tracks with Bezier
handles, undo and project files (`editor.seq_curve_*`); `.render_job.ron` v2 adds a binding table so a job plays
its own sequence. Camera cuts and fades stay in `.shots.ron` (ADR-268). Still open: a dope-sheet view for sections,
spatial audio sections.

*Status (2026-11-01, ADR-2033):* the "Still open" list in the block above is stale: the dope sheet and spatial audio sections were both built (dope sheet ADR-2135, spatial sections ADR-2405). Nothing in T2.6 was left to build except those two at the time. Render-out from a sequence shipped in v0.2.4 (lanes b1-sequencer-player, b2-render-out-sequence-on-b1) and the full sequencer in v0.2.9 (sequencer-full: render-out on the closing path, sequence audio and players client-only, `sequence` and `render-job` formats bumped).

*Status (2026-11-03, ADR-2405, lane sequencer-v3):* `.seq.ron` v3 adds spatial (3D) sound sections (a bound entity or a fixed position, with the section's attenuation, through the audio emitter math), a voice time scale on sound sections (varispeed: pitch follows; no pitch-preserving stretch), and looping sub-sequence sections (a count or until-stop, an exact-remainder wrap, at most 4 looping levels, `seq_loop_too_deep`). The section strip edits all three and renames a binding across the sequence as one undo step; agent methods `sequencer.spatial_voices`, `sequencer.sub_loops`, `sequencer.rename_binding`. The dope sheet for sections shipped with ADR-2135. Not done: occlusion, propagation, binaural rendering and Doppler for sequence voices, and a pitch-preserving time stretch.

*Status (2026-11-06, ADR-3334, lane seq-cutscene-subtitles):* a sound section whose sound belongs to a dialogue line shows that line's subtitle cues on the caption board, timed to the section's playback position (offset, time scale, sub-sequence speed and loops, seek, scrub, pause, skip), with the speaker and the player's caption options; no `.seq.ron` change. Opt-out by track name (`.nosubs` suffix) or settings; render-out requests them only with `burn_in`. Agent `sequencer.subtitles`, `sequencer.subtitle_settings`.

*Status (2026-11-04, ADR-2637, lane seq-take-recorder):* takes are recorded. `regia_framework::sequencer::recorder` records chosen bindings (a guid, an entity or the active camera) and channels (transform, camera with cuts, the dominant clip of a graph animator, named or reflected properties, `TakeCue` audio cues) once per gameplay frame into a `.seq.ron` v3 take with keys reduced by Ramer-Douglas-Peucker within a stated tolerance and checked against the sampler. A still object writes two keys. Each take is `<name>_take<N>` next to its source and joins it as a new sub-sequence section. A playing sequence's own bindings are left out unless asked. The editor's Sequence Curves panel has Record / Stop with a countdown and a channel picker; agent methods `sequence.record_start`, `sequence.record_stop`, `sequence.record_state`. Not done: recording from the mixer, single-clip animator time, Bezier key fitting.

*Status (2026-11-05, ADR-2862, lane seq-video-sections):* video in the sequencer. `.seq.ron` v4 adds `Video` tracks: a section has a source, start, end, offset, target (a fullscreen layer, a bound entity's video player, a bound mesh's albedo or emissive slot, or a bound UI image node), the volume of the source's own sound, and loop. `regia_framework::sequencer::video` (feature `media`) runs one media session per section in effect with its clock slaved to the sequence time, drift correction in real time, seeks that land where linear play is (previous keyframe, then forward, bounded), and frame-exact blocking under a manual clock, so render-out shows the exact source frame on every output frame with no drops. The section strip shows video sections with a first-frame still and edits them with undo; agent method `sequencer.video_sections`. Not done: a video as an arbitrary material graph texture (there is no `FramePassContext::dynamic_texture`; only the video slots exist), a pre-roll before a section's first frame in real time, and a video preview in the editor's own world (it has no `MediaPlugin`).

*Status (2026-11-05, ADR-2948, lane seq-spawnables-flow):* a sequence can create what it animates and runs as a cutscene. `.seq.ron` v5 adds `spawnables` (a binding that names a project `.prefab.ron` or a registered `guid:` template, with a range and `keep_on_stop`) and `restore_on_stop`. `play` pre-rolls until the prefabs are loaded (on the calling thread under manual time, an external clock or `force_sync`, so render-out and repeated runs start on the same frame), each actor exists while the clock is in its range with a deterministic guid and spawn order, and is despawned outside it, on stop and when the player lets go (unless kept); a failed prefab is `seq_spawn_failed` and its tracks are skipped. `SequencePlayer::skip_to` jumps forward delivering the markers in between with a `skipped` flag. `regia_framework::sequencer::cutscene` runs a player as a cutscene: input context lock through `regia_input`, letterbox bars and hidden `HudLayer` UI nodes through the `regia_ui` public API, a skip action with hold and fade, dialogue and subtitles stopped on a skip, and a hand-back through the camera takeover; `NetCutscenes` starts a cutscene at a tick on every peer and a late joiner skips to the tick's time (`NetCutscenePlugin` over `regia_net`). The section strip has a binding kind row with a prefab picker; agent methods `cutscene.start`, `cutscene.skip`, `cutscene.stop`, `cutscene.state` and `sequence.player` `skip`. Not done: spawnables inside sub-sequences (only the top sequence's spawn), `.variant.ron` paths (a registered `guid:` works), a pre-roll budget across several cutscenes.

*Status (2026-11-06, ADR-3197, lane seq-voice-doppler-stretch):* `.seq.ron` v5 closes ADR-2405's "Not done" list except binaural and propagation. Spatial sequence voices get a Doppler shift through the emitter math, driven by the section's motion sampled on the sequence clock (a position track's slope, a central difference), so a seek or a fixed-step render-out has no pitch jump; per-section `doppler` (on by default in v5, off for migrated v4 files), ratio clamped to 0.5..2. `occlusion: true` sends the section's ray through the audio occlusion query (owner-keyed extra rays) and plays it on a low-pass voice with the occlusion tuning, smoothed on the render clock in a render-out. `time_scale_mode: PreservePitch` plays a section from a WSOLA stretch (`regia_audio::stretch_asset`) rendered ahead into a 256 MiB cache, so seeks are sample-exact; rate 1 is bit-identical to the plain voice. Strip fields, `editor.seq_section_edit` op `time_scale_mode`, `sequencer.spatial_voices` reports the new state. Not done: binaural (HRTF) rendering and sound propagation for sequence voices (they need the per-entity emitter voice path).

*Status (2026-11-12, ADR-9710, lane v06-seq-authoring):* the editor authors a sequence's structure. `regia_framework::sequencer::edit::tracks` adds, removes, renames and reorders tracks of every `.seq.ron` v5 kind (transform, float, vec3 and bool property, clip, flat and spatial audio, sub-sequence, video) and markers, writes keys from values (creating the property track), and adds and removes bindings (refused while used unless `cascade`), each on a validated copy so a refusal changes nothing; no format change (a new sequence's length is an `end` marker). The Sequence Curves panel has authoring rows: New sequence, Add track, track reorder/rename/remove, markers, Bind selected (by scene guid) and Remove binding, **Key** and **Auto-key** (joined to the gizmo or inspector edit's undo entry), and **Preview in level** (applies the sequence to the edit world and puts every touched value back bit for bit; level saves never see it). The Inspector's field menu has **Add to sequence**. Agent methods `editor.seq_new`, `editor.seq_track_edit`, `editor.seq_binding_edit`, `editor.seq_key`, `editor.seq_preview`. Fades stay `.shots.ron` data (ADR-268): `fade` is refused. Not done: previewing sounds and video in the edit world. Since ADR-8730: Content Browser New ▸ Sequence and Shot list, the shot list saved and opened as a document, auto key once per editor frame (panel closed too), and clip sections posing their rig in the preview (not a rig with an `Animator`). Since ADR-8940: the preview plays flat and spatial sound sections on the editor's audio output, shows a video section's picture on its bound video player (fullscreen, mesh and UI targets not previewed), and poses `Animator` rigs (animator suspended, restored bit for bit); the shot-list document detaches when another list replaces it (Save then answers `shot_panel_detached` without a path), and a sequence player's own shot list can be adopted as a document (`editor.shot_panel_adopt`). Still not done in the preview: time-stretch of pitch-preserving sounds, Doppler and occlusion.

*Status (2026-11-12, ADR-7322, lane v06-seq-timeline):* the "keyframe curve editor" now edits many tracks at once. The Sequence Curves tab has a **Timeline** section: a dope sheet of every float, vec3 and bool track (click, Shift and box selection across tracks; move with frame snap, scale about the playhead, delete, copy and paste also into another sequence; open a track in the curve editor), a marker row (add, drag, rename, payload, delete, CSV import and export), a cut row of the shot list with its fade curve (add a cut at the playhead from the selected or active camera, move, trim, delete, fades; cuts and fades stay `.shots.ron` data, ADR-268) and a transport (play, reverse, loop, an in/out range that is never saved, timecode or frames, J/K/L, Home/End, frame steps, zoom, pan, follow). The edits are `regia_framework::sequencer::edit::timeline` (validated on a copy, one undo entry per gesture); agent methods `editor.seq_keys`, `editor.seq_marker_edit`, `editor.seq_cut_edit`, `editor.seq_transport`, `editor.seq_timeline_state`. No format change. Not done: section tracks as dope-sheet rows (the section strip keeps them), ripple edits.

*Status (2026-11-13, ADR-8640, lane w6-seq-timeline-z):* the ADR-7322 gaps close. Cut edits ripple: `CutEdit`
add/move/trim take `ripple` (off by default) — a rippling move keeps the second shot's clock so every later cut moves,
and the editor shifts the sequence's keys and markers after the edge's old place by the same delta on one validated
copy, as ONE undo entry (`editor.seq_cut_edit { ripple }`, a **Ripple** toolbar checkbox); a shortening ripple over a
key or marker in the removed span is refused (`seq_ripple_keys_in_span`, amended 2026-11-14). Section tracks (clip, audio, sub-sequence,
video) are dope-sheet rows whose section starts and ends are selectable marks that box-select and drag together with
keys (`editor.seq_keys` accepts `{ track, index, end }` refs; a move that creates an overlap is refused
`seq_section_overlap`). A
shortened Catmull-Rom camera path keeps its keys past the new end as control keys, so the curve before the cut is
bit-identical to the uncut shot's (`.shots.ron` validation now allows path keys past the shot's duration; no format
version change). The keyboard shortcuts and the Alt-drag scale are covered by synthetic egui input tests. Not done: a
judgment of the panel on a screen; a Smooth-eased scalar tail past a trim can still bend locally (its end key is
re-eased Linear, as before).

*As built (2026-11-14, ADR-7726):* render-to-image-sequence from the editor: Sequence ▸ Render movie… builds a
`.render_job.ron` (one take, the Sequence Curves panel's bindings, the dialog's folder, format, frame range, size
and rate) and runs it with `render.out_job` in process, as a cancellable editor job, so the editor and an agent
render the same frames with the same hashes (`editor.render_movie`, `book/agents/render-movie.md`). The editor
also sets a sequence's `restore_on_stop` (Sequence Curves panel) and keys the shot camera's position path and
rotation in the Shots panel (`book/agents/shot-panel.md`). No format changed: every field already existed.

## T2.7 Baked GI
- Lightmap UV generation (if missing) via a chart packer (xatlas-like algorithm from public papers;
  or port permissively-licensed code), GPU or CPU path-traced lightmap baker (irradiance + optional
  directional), denoise (bilateral/A-trous), BC6H/ASTC-friendly encoding.
- Probe volumes (SH L1/L2 grids) for dynamic objects; reflection probe bake integrated.
- Works on mobile (static lighting is the main mobile GI path).
- Lightmap design and phase plan: [`lightmaps.md`](lightmaps.md) (ADR-1100; v0.3.0).

## T2.8 Dynamic GI (desktop)
*Pulled forward into the spec 27 programme, stage 2 (`specs/27` § 4.2); scored against the reference path tracer.*
Screen-space GI + irradiance probes updated by ray tracing against per-mesh SDFs or hardware ray queries
when wgpu exposes them stably; temporal accumulation; quality tiers. Research spike first; ADR on approach.

## T2.9 Virtual geometry (desktop)
*Pulled forward into the spec 27 programme, stage 3, foliage/scatter first (`specs/27` § 4.2).*
Cluster (meshlet) hierarchies built at cook (meshopt meshlets + simplification into a DAG), GPU
culling (frustum + Hi-Z occlusion) and LOD selection per cluster, visibility buffer rendering, software
rasterisation for tiny triangles optional. Requires multi-draw indirect / compute; desktop profile only.
Fallback to regular LOD meshes elsewhere.

## T2.10 2D toolkit
Sprites (atlases, pivot, 9-slice), sprite animation, tilemaps (multiple layers, auto-tiling rules),
2D lights (parked), `rapier2d` integration, orthographic pixel-perfect camera, 2D editor mode.

**Implemented (lane 2d-toolkit, 2026-10-08; ADR "2D toolkit").** Crate `regia_2d` (layer 2,
→ render): cooked sprite atlases from `.atlas.ron` (frozen v1, `docs/formats/atlas-ron.md`;
`texture_packer` at cook time; pivot, trim, extrusion, 9-slice), sprite-sheet animation, tilemaps
(frozen `.tilemap.ron` v1, `docs/formats/tilemap-ron.md`, imported from Tiled `.tmx`/`.tsx`/`.tmj`;
multiple layers, flips, animated tiles, objects, per-tile collision), and an instanced sprite pass
drawn as a `WindowOverlay` layer before the UI. Orthographic pixel-perfect camera:
`regia_framework::camera::ortho` (integer scale, letterbox, snapping, follow). Physics:
`regia_physics::platformer2d` (actors/solids, one-way tiles) and `regia_physics/physics2d`
(rapier2d). Agent: `sprite.*`, `tilemap.*`, `view2d.get` (`book/agents/2d.md`). Still open:
auto-tiling rules, 2D lights (parked) and the 2D editor mode.

**Remainder (lane e7-2d-toolkit, 2026-10-29; ADR-1889):** auto-tiling rules (`.autotile.ron` v1:
Edge16, Blob47 and 3×3 pattern rules with weighted variants) and the 2D editor mode are specified in
`docs/specs/86-2d-toolkit.md`. 2D lights stay parked.

## T2.11 World streaming
*Status (2026-11-01, ADR-2033):* `regia_scene` has `stream_grid`, `hlod` and `floating_origin`, and chunked levels shipped in v0.2.6 (b21-chunks-levels). Large-scene numbers and the cooked HLOD proxy path are not measured; finishing is v0.4.0.
Grid-based partitioning of large levels into cells stored as separate cooked chunks; streaming sources
(players, cameras) with radii; async load/unload with budgets; HLOD proxy meshes for distant cells;
floating origin (rebasing when the player is > 5 km from origin) for precision.

## T2.12 Render performance
Pipelined rendering (render thread), GPU-driven culling & indirect drawing, transient resource
aliasing, parallel command encoding by default, texture mip streaming with residency budget,
FSR1-style upscaler, variable-rate frame pacing on Android (Swappy-like behaviour via
`ANativeWindow_setFrameRate`).

Status (ADR-2074): measured with `regia_render/tests/frame_pipelining_s0.rs`. Pipelining the frame's shadow plan is a
profile switch (`RenderProfile::frame_pipelining`, off in every preset); parallel pass recording was measured and not
built; a render thread exists for hosts that drive `FrameRenderer` (`with_render_thread`), not yet in the engine's frame
loop; transient aliasing exists in the graph pool (`TransientPool::set_aliasing`), and the frame cannot use it until its
built-in passes run as graph nodes.

## T2.13 Runtime UI designer
*Status (2026-11-02, ADR-2312; was ADR-2033):* built in v0.2.9 (`regia_editor/src/ui_designer*.rs`, lane e2-ui-designer, spec 81). The preview is built too (`ui_designer_preview.rs`, ADR-1811): it lays the document out with the runtime layout at a named window size or preset, flags nodes outside the safe-area insets, and fills localized spans from the project's Fluent files or pseudo-localization (`locale`, `pseudo`, `size`, `safe_area`, `samples` on the designer's agent methods, `book/agents/ui-designer.md`).
WYSIWYG editor for `.ui.ron` (hierarchy, layout properties, theme classes, bindings, preview at
multiple resolutions/safe areas, localisation preview).

## T2.14 Importers and pipeline extras
*Status (2026-11-02, ADR-2312; was ADR-2033):* FBX (pipeline-only, v0.2.4), OBJ, convex decomposition (`regia_physics::convex_decomposition`) and EBU R128 loudness (`regia_audio::loudness`, with cook wiring in v0.2.9) are built. Texture atlasing: the `.atlas.ron` sprite-atlas cook (`regia_asset_pipeline::atlas_cook`, since v0.2.4) feeds UI icon sheets (`regia_ui::icons::IconAtlas`), and UI text uses its own dynamic glyph atlas (`regia_ui::atlas`). Android asset packs / Play Asset Delivery are built since v0.2.8 (`regia_platform::asset_packs`, spec 40 § 2.6, ADR-1876); a store-side run needs a Play Console upload (owner). Texture mip streaming metadata is stamped into cooked KTX2 textures by the cook (`regia_asset_pipeline::mip_stream`).
FBX via `ufbx`, OBJ, texture mip streaming metadata, convex decomposition (V-HACD-like, public paper),
audio loudness normalisation (EBU R128), texture atlasing for UI, Android asset packs / Play Asset Delivery.
*MaterialX (ADR-3341, proposed):* `.mtlx` surface materials (`standard_surface`, `open_pbr_surface`) import onto material graphs and cook like `.matgraph.ron` (`regia_asset_pipeline::materialx_import`, `import.materialx_report`, `book/agents/materialx-import.md`).

### T2.14b Pak update diffs (v0.1.28, ADR-3322)

*(Numbered T2.14a until ADR-5628: the texture-set section below also used T2.14a, and ADR-3320 cites that one.)*

What shipping a new build's paks over an old one's costs a player. Steam delivers updates as deltas
over fixed-size chunks of about 1 MiB (SteamPipe's public documentation), and a Regia pak is one
large file, so the pak writer's order and alignment stability decide patch size. The writer keeps
entries sorted by asset id at 4096-byte alignment, which pins a deterministic prefix: editing one
asset moves only the chunks from its bytes through the end of the pak.

- `regia_asset::pak_diff` (`diff_paks`, `diff_sides`): per asset by TOC asset id — added, removed,
  changed (blake3 over the entry's stored bytes), moved (same bytes at a new offset), unchanged —
  plus a fixed-size chunk comparison (default 1 MiB) and an estimated patch download (the new-side
  bytes of every changed or added chunk), labelled an estimate that cites Steam's public SteamPipe
  page. Each side is a `.fpak` file or a packaged build directory (paks pair up by relative path).
  A truncated or unreadable pak is a structured error, never a panic.
- CLI: `regia assets pak-diff <old> <new> [--json] [--chunk-size BYTES]`. Agent: `assets.pak_diff
  { old, new, chunk_size? }`, `tools` permission (desktop tools class). Codes:
  `pak_diff_unreadable`, `pak_diff_no_paks`, `pak_diff_invalid_chunk_size` (also for a `chunk_size`
  above 64 MiB, `MAX_CHUNK_SIZE`), and `pak_diff_report` when the diff cannot be serialized
  (refinement recorded by ADR-5628).
- Stability is test-pinned in `regia_asset_pipeline::pak_writer`: a reordered source list gives
  byte-identical paks; after one asset is edited, every other asset keeps its bytes and TOC order
  (measured: a same-length edit diffs to exactly that asset's chunks plus header and TOC).

## T2.15 Apple platforms: macOS, then iOS
- **macOS** (runtime + editor) — deferred out of the v1.0 core until the owner has a Mac to test on (ADR-009); bring-up for testing only is in ADR-264.
  The design is already in the v1.0-core specs: Metal backend (`specs/13` § 2), `.app` packaging and
  signing (`specs/21` § 4), hot reload, Steam overlay on Metal. The port is mostly verification: build,
  run the full test and golden suites, package, then add macOS to CI and to `AGENTS.md` § 1.10.
- **iOS** — Metal path shared with macOS; touch input and lifecycle handling shared with Android; Xcode
  project generation and signing in `regia package`. Own spec before implementation.

## Parked (owner to place, listed for architecture awareness)
Consoles, hardware-RT-only paths, Google Play services (accessibility via AccessKit left this list: the bridge shipped, `regia_ui::a11y_platform`), web, modding SDK
(load user paks + sandboxed logic is hard in Rust-only — would need WASM), visual graph → Rust codegen.
(Steam is in the v1.0 core, `specs/24`; macOS and iOS are v0.4.0, above.)

## T2.3 addendum: connectionless server bots (ADR-3346)

`regia_framework::net_bots` owns policy and player lifecycle; `regia_ai::net_bots` owns the behaviour-tree
adapter because `regia_ai` already depends on the framework. A `NetBot` marker, rather than a new
`PlayerKind` variant, keeps exhaustive host matches source-compatible. Bots join through `GameRules`, use a
`Player` plus `PlayerController`/`AiController`, and possess replicated `NetCharacter` pawns without a
`NetOwner`. They have no peer, connection, input command queue or per-bot send allocation. Their view target
is the possessed pawn; no fake `NetPlayerId` is minted. Bot pawns and scores replicate to real clients like
any other server-owned entity.

Policy has `min_players`, `max_bots`, fixed-tick join/leave hysteresis and deterministic per-bot difficulty.
Initial fill is immediate. A dropped player's pawn keeps its entity, `NetOwner`, transform, movement state
and game-owned score while `NetBotDriven` redirects the server character step to direct `CharacterInput`.
On resume the marker and brain are removed and the original owner's command path continues. After grace
expires the takeover becomes a filler. `net_session_players` and `session.state` expose `is_bot`; `net.bots`
administers list/add/remove/set_difficulty with the server play permission. `[bots]` is additive to
`server.toml` v1, and net_game's hosted server accepts `--bots N`.

Acceptance uses fixed steps and seeds: four nav-following bots with no clients, hysteretic replacement,
state-intact reconnect handback, the character movement clamp, identical state hashes for repeated seeds,
and no per-bot transport bytes. `net_bot_policy` and `net_bot_tick` carry tracing spans.

## T2.14a Texture-set materials (2026-11-06, ADR-3320)

A folder of PBR maps named `<base>_<suffix>.<ext>` becomes one `.material.ron` per base name
without hand-written RON. Built in `regia_asset::texture_sets` (re-exported as
`regia_asset_pipeline::texture_set`); `regia assets texture-sets <folder> [--write]
[--recursive] [--json]` (dry run by default) and the agent method `assets.texture_sets
{ folder, write?, recursive? }` share it.

- **One shared suffix table** (`TEXTURE_SUFFIX_TABLE` in `regia_asset::texture_sets`): the
  cook's usage inference (`usage_from_file_name`) and the set detector derive from it; defaults
  cover BaseColor/Albedo/Diffuse/Color, Normal/N/Nrm/NormalGL/NormalDX, Roughness, Metallic,
  AO, ORM/ARM/RMA/MRA (channel orders documented per suffix), Height/Displacement, Emissive/
  Emission, Opacity/Alpha/Mask. Ambiguous packings (`_RM`, `_MR`, `_MetallicRoughness`) are
  cook-only: the detector reports them as leftovers instead of guessing.
- **Detection** groups rasters per folder by base name; two files claiming one slot are a
  reported conflict and neither is used; a set never spans folders.
- **Generation** writes the members' `.meta` settings (usage; `normal_green` for a
  DirectX-convention normal; `orm_channel_order` for a non-ORM packed map — both applied at
  cook, never on disk), then `<base>.material.ron` naming the members' asset ids; an existing
  material is never overwritten. An opacity map switches the material to alpha cutout; an
  emissive map ships with a white emissive factor. A set's separate roughness/metallic maps
  and its opacity map have no slot in the standard material and are reported unreferenced
  until the packed-ORM packer (lane texture-cook-coverage-pack) lands.
- **Configuration** in `Regia.toml [import.texture_sets]`: `normal_green = "opengl"|"directx"`
  (project default) and `[import.texture_sets.suffixes]` extra words per slot.
- The module lives in `regia_asset`, not the pipeline: the agent protocol is a runtime crate
  and may not depend on the desktop-only pipeline (`docs/06` § 7); the established alternative
  (`editor.*` extension methods) would hide the method from game-hosted agents.

Importer version notes: PNG/JPEG 1.1.0 → 1.2.0 (`_NormalDX` maps to the normal usage;
meta-driven channel transforms); `material_ron` 1.4.0 → 1.5.0 (the cooked header lists the
texture-slot asset ids so chunking pulls the maps into the pak). No frozen content format
changed: `.material.ron` stays v7, `.meta` stays v1.
