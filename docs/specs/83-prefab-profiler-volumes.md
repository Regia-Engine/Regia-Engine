# 83 — Prefab variants, post-volume remainder, in-editor profiler

Status: v0.3.x (b) lane `sprint-e4-prefab-profiler-volumes` (ADR-2323, proposed). Three small items from
`docs/master-plan.md` § 3.4 b and `docs/versions.md` (v0.3.x (b)): prefab variants, the remainder of post-process
volumes (`30-post-volumes-and-physical-camera.md`), and the in-editor frame profiler. No reviewed public spec exists
for these items; the Regia docs are the source (`14-scene-and-framework.md` A3 for prefabs, `30-…` for volumes,
`10-core.md` § 1.3 for the frame profiler). Algorithms are Regia's own; public sources are cited in
`docs/provenance.md`.

Agent pages: `book/agents/prefab-variants.md`, `book/agents/profiler.md`, and `book/agents/post-volumes.md`
(extended).

## 1. Purpose

- **Prefab variants.** A prefab defined as another prefab plus edits ("a red lamp is the lamp with a red light"). A
  change to the base shows up in every variant; the variant only stores what differs.
- **Post-volume remainder.** Two gaps left by ADR-1464: the physical camera's lens was not saved in scene files
  (set it in the editor, save, and it was gone), and volumes were invisible in the viewport. This lane makes
  `PhysicalCamera` a reflected, versioned scene component and adds a `post_volumes` gizmo overlay.
- **In-editor profiler.** The frame profiler (`regia_core::profiling::FrameProfiler`) already records per-span CPU
  times and, with flame capture on, the span call tree (`FlameGraph`). The GPU timings (`GpuTimings`) already measure
  each render pass. Neither had an agent surface or a view. This lane adds both, plus a capture file.

## 2. Prefab variants

### 2.1 Model

A variant names a **base** (a prefab or another variant) and carries the edit lists an instance record has
(`PrefabInstanceData`): field overrides, added and removed components, added and removed children. Instantiating a
variant:

1. resolves the chain `variant → … → template` (`PrefabLibrary::resolve`);
2. spawns the template's entities (as for a plain prefab);
3. applies each variant step's edits, **base-most first**, as a template-owned record: its added children are
   prefab-internal (`PrefabLink`, derived `EntityGuid`) and addressable by later steps and by the instance;
4. applies the instance's own record last.

So the instance wins over the variant, which wins over its own base. The base's local ids stay valid: an override
written against the base works on an instance of any variant of it. The instance root's `PrefabInstance::prefab` is
the variant's id. Variants never copy base values, so a re-registered base propagates without re-baking variants.
Variants may be nested inside prefabs and used in levels exactly like prefabs.

Limits: a chain longer than `MAX_VARIANT_CHAIN` (16) or a cycle is `SceneSpawnError::Invalid`; a missing step is
`PrefabNotLoaded(id)` (so a level instance waits as a `PendingPrefab`, the existing path); a variant naming itself is
refused at registration. A failed instantiation leaves nothing behind (the existing cleanup).

### 2.2 API (`regia_scene::prefab_variant`)

```rust
pub const PREFAB_VARIANT_FORMAT_VERSION: u32 = 1;
pub const PREFAB_VARIANT_SUFFIX: &str = ".variant.ron";
pub const MAX_VARIANT_CHAIN: usize = 16;
pub struct PrefabVariantFile { pub format_version: u32, pub base: PrefabInstanceData }
impl PrefabVariantFile { fn new(base: Guid); fn from_instance(&PrefabInstanceData); fn from_ron_str; fn to_ron_string; fn migrate; fn validate }
pub struct PrefabVariant { pub asset: Guid, pub base: Guid, pub edits: PrefabInstanceData }
pub struct ResolvedPrefab { pub template: Arc<PrefabTemplate>, pub chain: Vec<Arc<PrefabVariant>> }
impl PrefabLibrary { fn insert_variant; fn variant; fn resolve; fn variant_chain; fn dependent_variants }
```

`PrefabLibrary::contains` is true for variants too; `first_missing` follows variant bases.

### 2.3 File format `.variant.ron` (`prefab-variant` v1)

```ron
PrefabVariant(
    format_version: 1,
    base: (
        prefab: "asset:8b6f2a11-0000-4000-8000-000000000001",
        overrides: [(target: "…", component: "my_game::Lamp", path: ".intensity", value: 800.0)],
        removed_children: ["…"],
    ),
)
```

The `.meta` sidecar kind is `prefab_variant_ron`. Rules: a missing `format_version` loads as 1 (step `0 → 1`); a
newer version is refused before the body is parsed. The edit record types are the scene format's, so the
`prefab-variant` fingerprint (`prefab_variant_schema_fingerprint`) includes them: a scene-format change to
`PrefabInstanceData` also bumps this format. **Migration note:** a v2 adds its step to `PrefabVariantFile::migrate`
and its source to `prefab_variant_migration_sources`, re-records the hash (`cargo xtask check-formats --update`) and
teaches `regia upgrade-project` (suffix `.variant.ron`) to rewrite files.

### 2.4 Editor and agent surface

- `regia_editor::prefab_variant_edit`: `create_prefab_variant(world, base, variant, from_instance)` writes the
  file and its `.meta`, `register_prefab_file(world, path)` bakes a prefab or registers a variant (loading its
  chain's bases from the project by `.meta` id, and refusing a loop or an over-long chain as `chain_broken`), and
  `content::spawn_prefab_asset` accepts `.variant.ron`. A changed base re-instantiates the live instances of every
  dependent variant (`reload_prefab_file`); a base saved in prefab edit mode reaches them when the stashed level
  returns. Prefab edit mode refuses a variant by name (§ 9). Project import parses a `.variant.ron` before it enters
  the project, and asset search types it `prefab`.
- Agent (`book/agents/prefab-variants.md`): `editor.create_prefab_variant { base, path, from_entity? }`,
  `editor.prefab_variant { path }` (read: base, chain, edit counts), `editor.set_variant_override { path, target,
  component, field?, value? }` (edit: add, replace or, with `value` null, clear one override; then
  re-register and respawn). `editor.spawn_prefab` accepts a variant path. Error reasons are listed on the page.
- As built (ADR-7614), the UI: the Outliner's row menu on a prefab instance has *Create variant…* (a dialog for the
  file; `from_instance` is the row) and the Content Browser's menu on a `.prefab.ron` / `.variant.ron` has *Create
  variant…* (an empty variant); both run `create_prefab_variant`, which now records one undo step (the undo removes
  the file and its `.meta`; a redo writes the same bytes and id) for the agent method too. The Inspector
  (`inspector/variant.rs`, declared as `regia_editor::inspector_variant`) lists, under each component of an entity of
  a variant instance, the fields the variant overrides (from the registered variant, no disk read per frame), each with
  a marker and *Clear override*, which is `set_variant_override` with no value.

## 3. Post-volume remainder

### 3.1 `PhysicalCamera` in scene files (`physical-camera` v1)

`PhysicalCamera` (and `CameraExposureMode`, `DepthOfFieldMode`) derive `Reflect`; `PhysicalCamera` is registered as
a component with the pinned type path `regia_render::lens::PhysicalCamera`, so the editor inspector edits it and a
level save keeps it. It gains `format_version: u32` (`PHYSICAL_CAMERA_FORMAT_VERSION = 1`; a hand-written component
without it loads as v1; a newer one is ignored by the renderer with one warning, like `PostVolume`). Fingerprint
`physical_camera_schema_fingerprint`, migration sources `[0]`. A v2 follows the `PostVolume` rule (spec 30 § 6).

### 3.2 `post_volumes` gizmo overlay

A debug gizmo group `PostVolumeGizmos` (`"post_volumes"`, off by default) draws every enabled `PostVolume`: the
shape in a color keyed by priority, and the outer edge of the blend band dimmer. `Unbound` volumes draw nothing.
Turned on with `debug.set_gizmos { group: "post_volumes" }`.

The lines are the segments of `post_volume_outline` (spec 30 § 10, post-volume format v3), the data
`render.post_volume_gizmos` returns: box edges under the full entity transform, three great circles for a sphere,
and the bounded edges of a convex hull; the band is the shape with every face pushed out by the blend distance
(exact for a sphere). So the overlay and the agent's gizmo data are one set of segments (re-cut lane, ADR-2323).
The overlay additionally skips what contributes nothing to the blend (a disabled volume, a newer format, a box with
a zero extent, a sphere with a zero radius, an invalid hull).

### 3.3 Upgrading the embedded components

`regia upgrade-project` plans the `PostVolume` and `PhysicalCamera` components inside every `.level.ron` and
`.prefab.ron` as rows of their own (`post-volume`, `physical-camera`; the lowest version in the file, or the highest
when one is newer than the engine). `--write` migrates the file first and then its components (decode under the
registered type, `migrate`, encode, re-save) and keeps one `.bak` with the original bytes. A component newer than
the engine is never touched. Level directories (`.level.d`) are planned the same way, one row per format under the
directory's path (each entity file is parsed on its own, so a directory at mixed scene versions still lists its
components); `--write` upgrades the directory first, then migrates its components and writes the directory back,
which rewrites only the entity files that changed, with one `.bak` copy of the original directory.

## 4. In-editor profiler

### 4.1 Data

- **CPU:** the profiler's span table (`FrameProfiler::span_stats`) and flame graph (`FrameProfiler::flame_graph`, the
  per-frame mean over the newest N frames; capture is opt-in, `set_flame_enabled`).
- **GPU:** `GpuTimings::passes()` of the main view when GPU timing is on (`RenderSettings` GPU timing; lavapipe and
  devices without timestamp queries report none).
- **History:** `FrameProfiler::frame_times` and `frame_spans(index)` (ADR-9740): each retained frame's wall time and
  per-span totals, oldest first; read only when the panel or an agent asks.
- **Play context:** `regia_core::profiling::flame_context(name)` (ADR-9740): while its guard lives, a root span ending
  on a compute task-pool thread is filed under `name` in the flame tree. The editor sets it around each play frame
  (`pie_frame`, `pie_step`, `pie_client_frame`), so play systems on worker threads show under play. No cost with
  flame capture off. As built (ADR-7427): only the compute pool (`regia-compute (n)`, bevy's `Compute Task Pool (n)`)
  counts; roots on the async-compute pool (`regia-async`: editor imports, network polls) and the IO pool are their
  own roots. Contexts are a stack of live guards: guards may drop in any order or on any thread, the context is the
  newest live guard's name, and none once the last guard drops. A process source whose worker panicked while storing
  a poll reads `disconnected` with the reason (`the profiler worker panicked ...`), not `connecting`.
- **Entities and memory:** ECS counts (spawned entities, archetypes, components by entity count, walked over the
  archetype table) and `memory.report` (subsystems, owners, largest GPU allocations, and `assets_by_type`: loaded count
  and bytes per asset type from `AssetServer::memory_by_type`, computed on request; ADR-9740).

### 4.2 Capture file (`profile-capture` v1, JSON, `.rprof.json`)

```json
{ "format_version": 1, "frames": 60, "frame_ms": 8.1,
  "cpu": [ { "name": "frame", "inclusive_ms": 7.9, "self_ms": 0.2, "calls": 1.0, "children": [ … ] } ],
  "gpu": [ { "pass": "shadows", "ms": 0.9 } ], "folded": "frame;render 7.9\n…" }
```

Written atomically (temp file + rename). Readers refuse a newer `format_version`; a missing one is 1. A capture is a
diagnostic artefact, not content: it is not loaded by the engine, so a v2 needs only the reader step in
`ProfileCapture::from_json` (no `upgrade-project` step). Durations are milliseconds; nodes are sorted by inclusive
time then name, so two captures of the same data serialize byte-identically.

### 4.3 Surfaces

- Agent (`book/agents/profiler.md`): `profiler.set_capture { flame }`, `profiler.frame { frames?, depth?, max_nodes? }`
  (read: CPU tree, top spans, GPU passes), `profiler.capture { path, frames?, depth?, max_nodes? }` (writes the capture
  file). `frames` 1–300 (default 60), `depth` 1–32 (default 8), `max_nodes` 1–5000 (default 2000) (ADR-5815).
- Editor: a **Profiler** panel (View menu) with a frame-time line, a flame view (one row per depth, width ∝
  inclusive time, hover shows name/inclusive/self/calls), the GPU pass list, and a **Capture** button writing
  `captures/<timestamp>.rprof.json` under the project. The layout is a pure function (`flame_layout`) tested without a
  GPU.
- Editor, as built by ADR-9740 (`book/agents/profiler.md`): a **Source** picker: `edit` (every span outside play),
  `pie` (the `pie_frame` / `pie_step` / `pie_client_frame` subtrees, and since ADR-8594 the `pie_render` subtree of
  each picture the editor draws of the play world), `process` (a separate-process PIE child, spec 77
  § 10.4, which advertises its endpoint in `build/pie/<session>/agent/<role>` through `REGIA_AGENT_DIR`) and `remote`
  (a dev build at a loopback `ws://` address with its token). Process sources are read with `regia_agent`'s client on a
  worker thread (`profiler.set_capture`, `profiler.frame` with a `since_frame` history cursor, `memory.report` and the
  ranked entity counts every fourth poll, one poll per 250 ms; ADR-7849); a
  dropped connection shows `disconnected` and is retried each second; the editor frame never waits on it. A click on
  the picker's already-selected entry restarts the worker while its link is `disconnected` (ADR-7493) — the gesture
  ADR-7427's panicked-worker reason tells the user to make. Tabs:
  Flame; History (300 frames, hitches over twice the median and at least 4 ms listed with their top five spans, click
  to freeze a frame); Entities (total, archetypes, top 20 components); Memory (subsystems, owners, top GPU, asset
  types); Compare (§ 4.4). **Open**, **Set A** and **Set B** read `.rprof.json` files. The panel reads its source once
  per repaint inside one `profiler_panel` span. Agent twins: `editor.profiler_state`, `editor.profiler_source { kind,
  address?, token? }`, `editor.profiler_capture_slot { slot, path? }`.

### 4.4 Comparing captures (ADR-9740)

`regia_telemetry::profile_compare::compare(a, b)` is a pure function next to `ProfileCapture`: spans are matched by
their path from the root; each row holds the inclusive and self ms of both captures and `b - a`, with `presence`
`both`, `only_a` or `only_b`. Rows sort by the absolute inclusive change, then the absolute self change, then the path;
GPU passes by the absolute change, then the name. Every number is rounded to the microsecond (and `-0` to `0`), so one
pair of captures gives byte-identical JSON. Agent: `profiler.compare { a, b, top? }` (runtime-safe, read-only). The
editor's Compare tab lists the rows and colors the flame view of B by the inclusive change (grey unchanged, red slower,
green faster, scaled by the largest change).

## 5. Determinism

- Variant resolution and edit order are fixed (chain base-most first, edits in file order); derived guids come from
  the instance root guid as for every prefab entity, so the same level gives the same guids.
- Volume gizmos read the same data the renderer blends; they change no frame (off by default; gizmos are not in
  render-out unless enabled).
- Profiler data is wall-clock and therefore not deterministic; tests check structure (names, nesting, ordering,
  round trips) and use injected spans, never time budgets.

## 6. Tests and acceptance

- `regia_scene::prefab_variant::tests`: file round trip, migration from a missing version, newer refused, nil base
  and nil targets refused; a variant spawns its base with its edits and follows a base change; chain order (base-most
  first) and the instance wins; variant added children are prefab-internal, addressable, and their guids
  deterministic; cycle, missing base, self base and over-long chains are reported and leave nothing behind;
  dependents and id replacement; a variant nested in a prefab.
- `regia_render`: `PhysicalCamera` reflect round trip through a scene file (spawn → snapshot → respawn equal),
  missing `format_version` loads as v1, the fingerprint names every field; the gizmo draws box/sphere/hull edges and band
  (the `post_volume_outline` segments), nothing for `Unbound`, disabled or degenerate volumes.
- `regia_telemetry` / `regia_agent`: capture JSON round trip and newer refusal; agent methods on a world with
  injected spans; error reasons; book page names every method and reason.
- `regia_editor`: `flame_layout` geometry (children inside parents, widths ∝ time, minimum width culling);
  create/spawn/edit a variant through the content functions and agent methods.

- ADR-9740: `regia_telemetry::profile_compare` (fixture deltas, order, byte-identical JSON, rounding);
  `regia_core::profiling` (`frame_times`/`frame_spans` by index, `flame_context` filing task-pool roots);
  `regia_asset` `tests/type_memory_ledger.rs` (per-type bytes vs the `assets` ledger within 64 KiB + 2 %, tagging
  allocator); `regia_agent` `profiler.compare` and `memory.report` `assets_by_type`; `regia_editor`: the play/edit
  split, history and hitches, entity counts equal `memory.report` `ecs.entities`, capture slots,
  `tests/profiler_sources.rs` (a play-only system span shows under `pie` and not `edit`; a dedicated-server PIE child
  started by `PieProcesses` is profiled over the protocol and the panel's top spans equal the child's
  `profiler.frame`; stopping shows `disconnected`), `tests/profiler_panel_span.rs` (one source read per repaint inside
  the `profiler_panel` span).

No golden images are added or changed.

## 7. Platforms

All code is platform-independent Rust. Linux, Windows and Android aarch64 build the runtime parts (`regia_scene`,
`regia_render`, `regia_telemetry`, `regia_agent`); the editor parts are desktop-only as always. The capture path is
project-relative; on Android `profiler.capture` writes under the app's writable data dir passed by the host (a
relative path is refused when no project root is known: `no_project_root`).

## 8. Performance budget

- Variant instantiation: one library lookup per chain step plus the edits; no per-frame cost.
- Gizmo overlay: off by default (its system does not run); on, O(volumes) line segments.
- Profiler: flame capture off by default; on, one record (the span's ancestor names) per span exit, capped at 65,536 per frame. The
  editor panel builds its layout once per repaint from at most 300 frames; `profiler.frame` caps the tree at a depth
  (default 8) and 2,000 nodes. Per-asset-type bytes, entity counts and the history are computed only when the panel
  or an agent asks (no per-frame cost); a process source costs one worker thread and four small requests per second
  on the profiled process.

## 9. Out of scope

- Prefab-edit mode for variants (editing a variant visually and diffing against the base): `enter_prefab_edit`
  refuses a `.variant.ron`; variant edits are authored through `create_prefab_variant` (from an instance) and
  `set_variant_override`.
- Lens-effect overrides by volumes (spec 30 § 8). Per-volume LUT blending and convex-hull volumes landed with
  post-volume v3 (spec 30 § 10).
- GPU timestamps per draw or per material. Capture comparison is built (§ 4.4, ADR-9740).
- **As built (ADR-7325)**: the per-thread timeline that ADR-9740 deferred. `FrameProfiler::set_timeline_enabled` (off
  by default; off, a span exit does no timeline work) records each exit's thread, name, start, end and depth into a
  reused per-thread buffer, at most 8,192 spans a frame (the rest counted as `dropped`), folded into the 300-frame
  history by `end_frame` (which now reuses its buffers: a warmed-up frame allocates nothing). `profile-capture` v2
  carries the window's timeline (`docs/formats/profile-capture-v2.md`; v0 and v1 read as v2 with none;
  `regia upgrade-project` re-writes them). Agent surface: `profiler.set_capture { timeline }`,
  `profiler.timeline { frame?, frames?, threads?, max_spans? }` (`timeline_off` while off), `profiler.capture { timeline? }`,
  `editor.profiler_timeline`. The panel's Timeline tab reads every source over the same paths as the other tabs (a
  process source's worker polls `profiler.timeline` only while the tab is shown); its geometry is the pure
  `thread_timeline_layout`; sub-pixel bars of a row merge into one bar with their count and the layout is cached by
  (data revision, view), shared with the `.regiatrace` view (`timeline_geometry`). **As built (ADR-7409)**: the
  multi-frame strip — a frame count of 1 to 8 (default 1) lays N consecutive retained frames on one time axis
  (`regia_telemetry::profile_capture::timeline_strip`): a boundary line and the frame number at each frame start,
  zoom and pan across the whole strip, ◀ ▶ step by one frame, a hover names the frame, and both timeline methods
  (`profiler.timeline`, `editor.profiler_timeline`) take `frames` and answer each frame with its start offset.
  ECS systems past `MAX_SYSTEMS` (ADR-7409) keep their own interned name in the timeline and flame records and are
  timed by name in the frame table; only the aggregated per-slot counters stay capped, and `profiler.frame` reports
  `systems_over_cap`. **As built (ADR-7849)**: a process or dev-build source's History tab holds every frame of the
  process, not one sample per poll — `profiler.frame { since_frame }` answers every retained frame from a frame-number
  cursor (`frame_times` rows with the wall time and the five costliest spans, plus `newest_frame`; a newest frame
  behind the cursor means the process restarted and the cursor starts again at 0); its Entities tab ranks component
  types like an in-process source (`profiler.frame { entities }`, counted by the same function,
  `regia_agent::profiler::entity_counts`); and freezing a history frame (the click or `editor.profiler_freeze`) stops
  the history's intake until it is unfrozen, so the frozen index and the hitch rows keep naming the same frames.
  `editor.profiler_timeline { frame }` holds the Timeline tab on a frame (the click's twin; ADR-7409). Not built:
  nothing of § 4 remains.
