# Visibility buffer and cooked meshlets

The visibility buffer is the opt-in geometry path for dense scenes (`docs/specs/27` § 4.2 stage 3):
rigid opaque meshes and eligible masked foliage are split into clusters (meshlets), culled and LOD-cut on the GPU, written into
an ID target, and shaded by a per-material resolve with the forward shader's lighting. This page is
about how a game's cooked meshes get onto it, how its memory behaves under streaming, and how an
agent checks both.

## Turning it on

The path runs when the active render profile has `visibility_buffer: true` (off on every preset; a
game sets it on a custom profile) and the device has the storage buffers the resolve needs. The
frame must also have a depth prepass, a post stack (linear HDR) and no MSAA, and some mesh must have
meshlets. Meshes without meshlets, and every mesh while the path is off, draw on the forward path.

An agent switches it with `render.set_visbuffer { enabled: true }` (below). A preset change
(`render.set_profile`) replaces the whole profile, and with it the switch.

## Dense scenes: selecting the path

`RenderProfile::visibility_path` says which frames take the path and how its resolve shades. The
default is what the switch has always meant: every frame the path can draw, one draw per material.
**No preset changes this.** A game picks the selectable profile, or an agent the same settings:

```rust,wrap
# use regia_render::{RenderProfile, RenderProfilePreset};
// Desktop High or Ultra; `None` on every other preset, none of which has been measured.
let profile = RenderProfile::dense_scenes(RenderProfilePreset::DesktopUltra).ok_or("no dense-scenes profile")?;
```

| `select` | Which frames take the path |
|---|---|
| `always` | Every frame the switch allows (the default) |
| `dense` | Only while the frame's rigid opaque meshes reach `enter_triangles` triangles or `enter_instances` meshes |

The count is the whole submitted list, in view or not, with or without meshlets, so turning the camera
does not flip the path. Entering takes `enter_frames` frames in a row over a threshold (default 4) and
leaving takes `leave_frames` frames in a row under half of both (default 120), so a scene near a threshold
never flaps. A sparse level draws the classic forward frame, byte for byte. A frame with persistent
instance sets (`FrameRenderer::set_instance_set`) always takes the path, because they draw nowhere else.

The default thresholds (2,000,000 triangles, 4096 meshes) are a starting point, not a measured
crossover. The switch itself costs one pop in LOD selection: the first path frame draws chains as
before, and clusters pick the detail from the next one (see "Mesh LOD chains").

A game-frame warm of a profile on `dense` builds the path's cull, raster, shadow and resolve pipelines
(warm key `visbuffer_path`), and draws two window-size frames through the path, so its targets, lists
and bind groups exist too. While a profile selects `dense`, the lists keep a reserve from its
thresholds: four times `enter_instances` meshes (1024 to 65,536) and the larger of
`enter_triangles / 32` and 16 per reserved mesh in clusters (at most 2^20). The frames a scene turns dense compile nothing and create no
target, list, buffer or bind group: `creates.last_frame.objects` stays 0, switch after switch, and a
new renderer-wide bind epoch in play does not touch the path's groups. A scene past the reserve grows a list once, on the frame it
passes it, and the count is flat again from the next frame. The dense scene's own meshes still become
resident on their first path frame, and if they overflow the cluster arena it doubles once ("Cluster
memory"), which replaces its buffers and the groups over them. A worker warm that is handed over
(`adopt_warm_pipelines`) moves the pipelines only; that renderer creates its lists and targets on its
first dense frame.

### Choosing the thresholds

Pick the thresholds in density units, not raw counts: the classic path's cost grows with the
triangles it submits per pixel, the path's with the clusters it draws per screen tile, so one
density holds at 1080p and at 4K where a triangle count does not. `DensityMetrics::of_frame`
gives a frame's `triangles_per_pixel` (the gate's triangle count over the frame's pixels),
`instances_per_megapixel` and `clusters_per_tile` (clusters drawn per 16 px tile, from
`FrameRenderer::visbuffer_counters`). The bench prints all three on every row ("Measuring a
machine"). To choose a default from a card's rows:

1. Run the bench at the target resolution. For each scene, compare the classic row's `gpu` time
   with the path's.
2. Take the lowest `tri/px` where the path's time is at or under the classic one, on both cards.
   That density, with a margin, is the triangle threshold. Do the same with `inst/Mpx` for the
   instance threshold (a many-small-props scene).
3. `DenseSceneSelect::at_density(tri_per_px, inst_per_mpx, width, height)` turns them into the
   gate's counts for a frame size. Write them into `RenderProfile::dense_scenes`.
4. Note the rows' `clusters/tile` next to the choice: that is the path's load at the
   crossover, and a scene far above it costs more on the path than the bench measured.

The current defaults (2,000,000 triangles, 4096 meshes) are about 1 triangle per pixel and 1,975
meshes per million pixels at 1080p. Lavapipe rows show the numbers are counted right, not where
the crossover is.

The bench does steps 1 to 3 itself (ADR-3149): after the scenes of each size it prints one
`crossover <resolve>:` line per resolve, from `DenseCrossover::of_rows`. That is the sparsest scene
from which the path wins every denser scene by at least 3 % (`DENSE_CROSSOVER_MARGIN`), with the
gate's counts at 1080p, 1440p and 4K. `bracketed false` means the path won down to the sparsest
scene run, so add sparser scenes until a row loses. `crossover <resolve>: none` means the path lost
the densest scene, and no threshold is safe on that card. For two cards, `DenseCrossover::stricter`
takes the denser of each column, so a routed scene wins on both.

## The dense-scene default

`RenderSettings::visibility_default` is the switch that routes dense scenes to the path by default
(ADR-3149). It is off (`preset`) in every preset, so nothing changes unless a game or an agent turns
it on:

```rust,wrap
# use regia_render::{RenderProfilePreset, RenderSettings, VisibilityDefault, VisibilityDefaultState};
let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
let state = settings.set_visibility_default(VisibilityDefault::Dense);
assert_eq!(state, VisibilityDefaultState::Applied);
assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopHigh)); // the same tier for every other system
```

On, a Desktop High or Ultra profile that still has its preset's path columns (path off, `always`)
gets `VisibilityPath::DENSE_DEFAULT`: the path on, `select` `dense` at `DenseSceneSelect::MEASURED`
(4,000,000 triangles or 4096 meshes), and the per-material resolve. Those thresholds come from the only
card rows recorded (a 5090 at 1440p, `docs/perf-log.md`). The sparsest scene the path won there has
3,999,744 triangles, and the single pass lost two of the four scenes, so it stays off. The 5080 and
7900 crossovers replace these numbers. The switch is kept across `apply_preset` (`render.set_profile`)
and leaves alone:

| `default.state` | Meaning |
|---|---|
| `applied` | The switch is on and the profile holds the default's path |
| `switch_off` | The switch is off (`preset`) |
| `preset_unmeasured` | The profile is not Desktop High or Ultra, or not a preset's row at all: unmeasured, untouched |
| `profile_choice` | The game set its own path (`enabled`, `select`, the single pass): the default neither writes nor reverts it |

Turning it off restores the preset's columns only when the profile still holds exactly what the default
wrote. With the switch on, no golden moves. The lab goldens draw at `FrameQuality::LAB`, which has the
path off whatever the settings say. The default-look goldens (`samples/lookdev/goldens`, drawn from
`RenderSettings`) stay byte-identical because their scenes are far below the thresholds:
`default_look_dense_default` renders each shot both ways with clusters available and fails on one
moved pixel. The same test prints what `select: always` would move instead.

Stage 3's acceptance on the floor tier (1080p, Desktop High, the default on) is
`samples/lookdev/tests/visbuffer_default_plates.rs`. In the gate, `the_floor_tier_routes_ten_times_the_rocks_to_the_path`
checks that 410 rocks stay classic and 4100 take the path. On a card, `stage3_scatter_plates` times
the base scatter, ten times the rocks on the classic path, and ten times with the default, and prints
`stage3 verdict: ... holds true|false` (ten times with the default within 5 % of the base).
`dense_default_owner_plates` writes the classic and default frames with their difference times eight
for the owner:

```sh
REGIA_STAGE3_OUT=/tmp/stage3 REGIA_STAGE3_SIZE=1920x1080 \
cargo test --release -p lookdev --test visbuffer_default_plates -- --ignored --nocapture --test-threads=1
```

## The single-pass resolve

With `single_pass_resolve` on, one full-screen draw shades every material. Group 2 binds the whole
material arena and a map from the frame's material slots to arena records, each pixel finds its record
through its instance, and the textures come from the texture table by per-pixel index. There is no
material-depth pass. The frames equal the per-material resolve byte for byte, on any adapter where both
run.

It needs a device on the texture table ([bindless](bindless.md)), which is decided when the device is
created: `RenderProfile::dense_scenes` and any profile with the knob on ask the first window's device for
it, and `REGIA_BINDLESS_LEVEL=3` does the same for any device. Where it cannot run, the per-material
resolve shades the same pixels and `resolve.fallback` says why:

| Code | Meaning | Fix |
|---|---|---|
| `vis_no_texture_table` | The device is off the texture table (`render.bindless_report` says why) | Set the profile before the first window's device, or `REGIA_BINDLESS_LEVEL=3`; a device that cannot carry the table keeps the per-material resolve |
| `vis_storage_limits` | Fewer than 13 storage buffers per fragment stage (14 with residency feedback) | A device with higher limits |
| `vis_lightmap_receivers` | An instance this frame is a lightmap receiver; the per-material resolve has the variant that reads the lightmap | Nothing to fix: the frame is right, it costs the per-material draws |

To check it: `render.get_visbuffer` `resolve.single_pass` is `true`, `resolve.draws` is 1 and
`resolve.materials` counts the materials; off it, `draws` is 1 + the materials.

## Mesh LOD chains

A cooked mesh usually carries a `MeshLodChain` too (its generated LOD 1..3 index lists). On the
visibility buffer the clusters are the LOD: each cluster of the simplification hierarchy is cut per
pixel at every distance. So while the path draws, the mesh LOD selector leaves a chained mesh at
LOD 0 when that mesh has meshlets (ADR-1148). Without that, its coarser levels would draw on the
forward path from the first switch distance on, which is most of a dense scatter.

- **When.** The profile has the path on, the runtime renderer's last frame took it
  (`FrameRenderer::visbuffer_active`), and the mesh has meshlets that were not declined (cooked for
  other mesh data) and not refused at the arena's cap (`FrameRenderer::visbuffer_cluster_lod`).
  The entity must also be one the path draws: opaque, not transmissive, no material graph, no morph
  targets (skinned meshes never take a chain's selection). Otherwise the chain picks the level as
  before, so a glass or morphing user of a shared mesh keeps its levels. The first visibility frame after the path turns on
  still uses the chain, and the hand-off starts on the next one.
- **Shadows.** With `cluster_shadows` on (the default), the clusters cast with their own per-texel
  cut, so the mesh is one draw. With it off, the chain's shadow LOD still draws the shadow on the
  forward shadow passes.
- **Switching back.** When the path stops drawing, or the meshlets are removed, the chain picks the
  level for the distance at once, with no cross-fade from LOD 0. Turning the path on is likewise
  one switch with no fade.
- **Hosts that drive `FrameRenderer` themselves** (not through `RuntimeRenderer`) keep their own
  LOD selection. A meshlet builder (`set_meshlet_builder`) clusters each LOD's index list as its
  own mesh, as before.

## Software raster

With `software_raster` on (`render.set_visbuffer`, off by default), clusters that span at most
`software_raster_max_px` pixels rasterize in compute instead of through the hardware rasterizer. A
cluster stays in hardware when it reaches the near plane or the software list is full. A
double-sided material's clusters take the compute path too, with their back faces drawn, as the
hardware's cull-none raster draws them; when the software list is full they fall back to that raster,
so their back faces stay. Shadow clusters always rasterize in hardware: a shadow job
writes depth only, with the shadow pipelines' slope bias, and its LOD cut is already per shadow
texel. The pixels merge into the same ID target and depth, so the resolve, motion vectors, SSAO and
decals do not change.

To check it, read `render.get_visbuffer` `settings` after the call, then compare screenshots with
the switch off and on: they match except for a few edge pixels. Tests and benches read how many
clusters took the path with `FrameRenderer::visbuffer_counters` (`software_clusters`), which blocks
on the GPU. Whether it is faster than the hardware rasterizer depends on the GPU, so no preset
turns it on.

## Cooking meshlets

Set `meshlets: true` in a glTF's `.meta` settings. `regia cook` then writes one `.fmlt` sub-asset
per mesh primitive, `meshlets/<node>/primitive_<i>`, next to `mesh/<node>/primitive_<i>`. The
clusters are built over LOD 0 of the cooked mesh exactly as the runtime decodes it, so they index
the vertex buffer the game uploads.

## The framework bridge

`regia_framework::meshlet_bridge::MeshletBridgePlugin` is part of `DefaultPlugins`. It is idle
until the profile turns the path on.

A host marks each entity that draws a cooked primitive with `MeshletSource`, the `.fmlt` sibling's
asset id:

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let (gltf_id, rock): (regia_asset::AssetId, Entity) = todo!();
# let server: regia_asset::AssetServer = todo!();
use regia_framework::meshlet_bridge::MeshletSource;

// From the glTF's asset id and the mesh's sub-asset path:
let source = MeshletSource::for_mesh(gltf_id, "mesh/Rock/primitive_0");
// Or from the manifest path of the cooked mesh:
let source = MeshletSource::for_mesh_path(&server, "assets/rock.gltf#mesh/Rock/primitive_0");
commands.entity(rock).insert(source.expect("a cooked mesh primitive"));
```

Each frame (`PostUpdate`) the bridge:

- **Inserts.** It loads the payload through the asset server (the latest pak that holds it, else
  the cooked cache) and calls `FrameRenderer::insert_meshlets` under the entity's mesh key. Several
  entities that share one mesh share one insert.
- **Registers masked foliage.** Rigid `ALPHA_MASK` meshes without hair or parallax receive their cooked meshlets
  too. The frame uses the alpha-tested cluster raster on texture-table devices; elsewhere they keep their forward
  draws. Material changes to blended, hair, parallax, transmissive or graph shading remove that entity from the bridge.
- **Keeps the forward path where it must.** Skinned meshes (`SkinJoints`), morphing meshes
  (`MeshMorphTargets`), translucent ones (blended, hair drawn blended or masked, transmissive) and
  material-graph meshes are never inserted.
- **Leaves a lightmap receiver alone.** It draws its entry's unwrap, which the source's clusters do not fit, so the bridge gives
  them back while it is one and inserts them again when it gets its own mesh back. Its clusters come from a
  `regia bake-lightmaps --meshlets` bake ([Lightmaps](lightmaps.md)).
- **Removes and inserts again.** This happens when the entity's mesh or `MeshletSource` changes,
  when the entity is despawned or loses either component (the last user of a mesh removes it), and
  when the `.fmlt` is hot-reloaded (`regia run --dev`, the editor's live cook).
- **Removes everything** when the profile turns the path off, and inserts again when it comes back
  on.
- **Fails safe.** A missing, damaged or old payload, or one cooked for other mesh data, logs one
  warning (per payload version) and the mesh draws forward. Nothing panics. Recooking the asset
  retries it.

Hosts that drive the renderer directly call `FrameRenderer::insert_meshlets`,
`FrameRenderer::remove_meshlets` and `FrameRenderer::has_meshlets` themselves.

## Cluster memory

Every resident mesh's cluster records and geometry words (vertices, cluster vertex lists,
triangles) are ranges of one GPU buffer per stream, taken from a best-fit free list that merges
neighbours on free (ADR-353). `remove_meshlets`, a dropped mesh (`release_meshes`) and a changed mesh
return their ranges, and the next mesh reuses them. Once a streaming map is warm, inserting and
removing meshes creates no buffer. A stream that runs out of room doubles, up to a cap that is
logged once when it is hit. A mesh that does not fit past the cap draws forward until room frees up.
A stream that doubles copies its old buffer into the new one on the GPU, then uploads only the ranges
written since (ADR-7462).

## Counted meshes

A counted mesh (`MeshRenderer::counted`, drawn from its GPU upload with no CPU vertices) takes the path
too (ADR-7462). Give it a `MeshletSource`: the bridge checks the `.fmlt` against the upload's vertex and
index counts and inserts it under the stand-in's key. On its first path frame the arena takes the same
ranges as for CPU vertices. One compute pass (`vis_vertex_repack.wgsl`) then copies the upload's vertices
into a GPU-owned range that the arena's CPU copy never writes. `render.stats` `counted.vis` reports
`meshes`, `clusters`, `repacks`, `forward_fallbacks` and `kept_meshlet_bytes` ([Assets](assets.md)).
It still draws forward with cluster residency on, as a LOD or cross-fade variant, without cooked
meshlets, or with a vertex buffer past the device's storage-binding limit (ADR-7542).

## GPU creates

The rest of the path is reused across frames too (ADR-355). Its instance and cull lists and its
shadow-cull lists only grow, by doubling, when a count passes their capacity. Its parameter
blocks are made once. Its ID, material-depth and Hi-Z targets are rebuilt only when the frame
size changes. Its bind groups are rebuilt only when a buffer or target they bind is replaced, and
its pipelines are built once per variant. A warm frame creates no GPU object through the path,
even while meshes stream in and out. It only uploads: instances, parameters and the arena ranges
written since the last frame.

`FrameRenderer::visbuffer_create_stats` (and `creates` below) counts every pipeline, bind group,
buffer and texture the path makes, with their bytes and the upload bytes. It reports a total,
the last frame, and `quiet_frames`: the run of frames, ending with the last one, that created
nothing. The blocking counter readbacks (`visbuffer_counters`) are test diagnostics and are not
counted.

## Agent method

`render.get_visbuffer` (read only) returns:

| Field | Meaning |
|---|---|
| `enabled` | the profile has `visibility_buffer` on |
| `active` | the runtime renderer's last frame drew through the path (all the conditions above held) |
| `settings` | `{ lod_threshold_px, shadow_lod_threshold_px, max_clusters, occlusion, cluster_shadows, software_raster, software_raster_max_px }`, or `null` without a renderer |
| `default` | `{ setting, state }`: the dense-scene default switch (`preset` or `dense`) and why it does or does not hold the path (`applied`, `switch_off`, `preset_unmeasured`, `profile_choice`; "The dense-scene default"), or `null` without `RenderSettings` |
| `select` | `{ mode, dense, gate }`, or `null` without `RenderSettings`: `mode` is `always` or `dense`; `dense` is `{ enter_triangles, enter_instances, enter_frames, leave_frames }` under `dense`, else `null`; `gate` is `{ dense, triangles, instances, entered, left }` from the renderer (`null` without one): the last frame's counts, and how often the scene entered and left |
| `resolve` | `{ requested, single_pass, fallback, draws, materials }` from the last visibility frame's resolve, or `null` without a renderer |
| `lod_handoff` | `{ active, chained, cluster_lod }` from the mesh LOD selector's last run: chained rigid meshes it placed, and how many it left at LOD 0 for their clusters. `null` without the render plugin |
| `bridge` | the framework bridge is installed |
| `meshes` | `{ visbuffer, forward }`: distinct meshes the bridge inserted, and the other meshes drawn by `MeshRenderer` entities |
| `entities` | `{ total, visbuffer, forward, forward_reasons }` over `MeshRenderer` entities |
| `clusters_resident` | cluster records resident in the arena |
| `buffer_creates` | GPU buffers created for the arena since the renderer started |
| `arena` | `{ meshes, pending, clusters, cluster_capacity, geometry_words, geometry_capacity_words, used_bytes, free_bytes, capacity_bytes, free_ranges, growths, buffer_creates, capped }`, or `null` without a renderer |
| `load_errors` | payload loads that failed since start |
| `errors` | `[{ asset, code, message }]`: the payloads failing now |
| `last_frame` | `{ frames, instances, candidate_clusters, materials, resident_meshes, geometry_bytes, declined_meshes, receiver_instances, receiver_materials, masked_instances, masked_materials, masked_tested, sway, lod_switches, lod_history_discards, lod_history_copies, view_histories, view_history_bytes }` from the last visibility frame, or `null` (see "Masked materials", "Swaying plants", "LOD hysteresis in split screen") |
| `creates` | `{ total, last_frame, frames, quiet_frames }`, or `null` without a renderer. `total` and `last_frame` are each `{ objects, pipelines, bind_groups, buffers, buffer_bytes, textures, texture_bytes, upload_bytes }`; `objects` is pipelines + bind groups + buffers + textures |

`pending` counts meshlets inserted whose mesh has not drawn yet: they become resident at its first
visibility frame. `free_ranges` is 2 with no fragmentation (one per stream).

`render.set_visbuffer` switches the path and its knobs. Every parameter is optional, and nothing is
applied unless all of them are valid (an invalid one is an invalid-params error, `F0002`, naming it):

| Parameter | Meaning |
|---|---|
| `enabled` | `RenderProfile.visibility_buffer` of the active profile |
| `lod_threshold_px` | largest projected simplification error of a drawn cluster, pixels (above 0, at most 1024; default 1) |
| `shadow_lod_threshold_px` | the same for shadow clusters, shadow-map texels (above 0, at most 1024; default 1) |
| `max_clusters` | clusters each cull phase may draw. `get` and `set` store and return the integer passed (at least 1, at most 4 294 967 295; default 1 048 576). The cull capacity clamps that stored value to 1024..=16 777 216 and does not rewrite it. `dropped_clusters` counts clusters past that clamp, not past the raw number |
| `occlusion` | two-phase Hi-Z occlusion culling |
| `cluster_shadows` | instances cast as culled, LOD-cut clusters (off: through the forward shadow passes) |
| `software_raster` | clusters at most `software_raster_max_px` across rasterize in compute (off by default; see "Software raster") |
| `software_raster_max_px` | largest projected cluster diameter the software rasterizer takes: an integer from 1 to 64 pixels (default 16) |
| `select` | `always` or `dense`: `RenderProfile.visibility_path.select` (see "Dense scenes") |
| `dense_enter_triangles` | triangles at which a scene is dense, at least 1. Needs `dense` (in this call or already on) |
| `dense_enter_instances` | meshes at which a scene is dense, at least 1. Needs `dense` likewise |
| `single_pass_resolve` | `RenderProfile.visibility_path.single_pass_resolve` (see "The single-pass resolve") |
| `default` | `preset` or `dense`: `RenderSettings::visibility_default` (see "The dense-scene default"). Applied first; the other path parameters in the same call apply over it |

It returns `{ before, after, note }`, where `before` and `after` are `{ enabled, active, settings, select, single_pass_resolve, default, masked }`.
`default` (also in `render.get_visbuffer`) is `{ setting, state }`: the switch and its `default.state` code, or `null` without `RenderSettings`.
`masked` is `{ instances, materials, tested }` from the last visibility frame (see "Masked materials"), or `null` without a renderer.
`active` turns true at the first frame drawn through the path. With no parameters it changes
nothing and returns the current state twice. Without the render plugin, `enabled` is refused
(not found, `F0003`: no `RenderSettings`), and so are the knobs (no `RuntimeRenderer`).

Forward reasons (`entities.forward_reasons`): `no_meshlets` (no `MeshletSource`), `skinned`,
`morphing`, `translucent`, `graph_material`, `thin_film`, `load_error`, `meshlets_mismatch`, `sway_graph_surface`
(a sway on a surface graph of its own, see "Swaying plants"), and `path_off` (the only key while the path is off).
A lightmap receiver counts in `no_meshlets` here; `render.get_lightmaps` `receivers.{clusters, no_clusters}` says how it draws.

Payload error codes (`errors[].code`):

| Code | Meaning |
|---|---|
| `meshlets_not_cooked` | no pak and no cooked-cache file holds the asset (not cooked, or `.meta` lacks `meshlets: true`) |
| `meshlets_unreadable` | a bad cooked wrapper, another asset type, or not a meshlet blob |
| `meshlets_old_version` | cooked by another meshlet format version: run `regia cook` |
| `meshlets_invalid` | truncated, or a cluster range is out of bounds |
| `meshlets_mismatch` | cooked for other mesh data: its vertex or index count differs from the entity's mesh |

## Checking a level

1. The game turns `visibility_buffer` on in its `RenderSettings` profile, or an agent calls
   `render.set_visbuffer { enabled: true }`. Step a few frames (`play.step 3`).
2. `render.get_visbuffer`: `enabled` and `active` are `true`, `entities.visbuffer` counts the rigid
   props you expect, and `errors` is empty. `last_frame.instances` is above 0 once a camera sees
   them. `enabled` true with `active` false means a frame condition failed (no depth prepass, MSAA,
   no post stack, an unsupported device, or no meshlets yet).
3. Back the camera off past the props' first LOD switch distance. `lod_handoff.cluster_lod` equals
   the opaque chained props with meshlets (in view or not), and `last_frame.instances` does not
   drop. If `cluster_lod` is 0 while `active` is true, those props have no meshlets under their
   LOD 0 mesh key.
4. A `forward_reasons.load_error` or `meshlets_mismatch` above 0: read `errors`, recook the named
   asset, and check again (the bridge retries after the hot reload).
5. Stream the level (load and unload cells) and read `buffer_creates` before and after: it stays
   flat once the arena is warm. `capped: true` means a stream is at its cap. The log names it, and
   meshes past it draw forward.
6. While it streams, `creates.last_frame.objects` is 0 and `creates.quiet_frames` keeps rising.
   A frame with objects above 0 after warm-up is a hitch source: a list grew (a one-off, after
   which the count stays flat), or the frame size changed (new targets). If it comes back frame
   after frame, it is a bug. Report the `last_frame` counters.
7. Turn the path off: `entities.forward_reasons.path_off` equals `entities.total` and
   `arena.meshes` drops to 0 at the next frame.
8. On a profile selecting `dense`: `select.gate.triangles` and `instances` count the frame. Below the
   thresholds `active` stays false and the frame is the classic one. Spawn past them: after
   `enter_frames` frames `gate.dense` and `active` are true and `gate.entered` is 1.

## Measuring a machine

The bench prints one row per path (classic, per-material, single-pass, single-pass with the software
raster): frame time with readback, GPU stamps, the resolve's draws, the clusters drawn and the
scene's density (`tri/px`, `inst/Mpx`, `clusters/tile`; "Choosing the thresholds"). It asserts
nothing. Only a real card can decide a default. Lavapipe numbers show the paths run and their draw
counts, not their relative cost on hardware.

```sh
# RTX 5080 or RX 7900, on the machine's own Vulkan driver, 1080p and 4K:
REGIA_VIS_BENCH_SIZES=1920x1080,3840x2160 \
REGIA_VIS_BENCH_SCENES=1024x2048,4096x2048,4096x8192,16384x2048 \
REGIA_VIS_BENCH_FRAMES=60 \
cargo test --release -p regia_render --test visbuffer_dense_bench -- --ignored --nocapture
# To bracket the crossover (ADR-3149): sparse scenes and few-triangle meshes too, three
# interleaved rounds, 1080p (the floor tier), 1440p and 4K.
REGIA_VIS_BENCH_SIZES=1920x1080,2560x1440,3840x2160 \
REGIA_VIS_BENCH_SCENES=64x2048,256x2048,512x2048,1024x2048,4096x128,16384x128,4096x2048,4096x8192,16384x2048 \
REGIA_VIS_BENCH_FRAMES=60 REGIA_VIS_BENCH_ROUNDS=3 \
cargo test --release -p regia_render --test visbuffer_dense_bench -- --ignored --nocapture
```

`REGIA_VIS_BENCH_RESIDENCY=1` runs the visibility rows with cluster residency at the Desktop High budget
(`ClusterResidency::for_preset`), as the dense-scene default would ship (ADR-4906). Every visibility row runs the
LOD band. Each row ends with `lod switches N, resident X MiB (peak Y MiB, budget Z MiB)`.

The bench's `geometry` stamp spans the cull, both rasters and the Hi-Z build. For the cull alone, phase by
phase, run the ignored lib test `cull_cost` (test-only stamps around the cull passes):

```sh
REGIA_CULL_COST_SCENES=1024x2048,4096x2048,4096x8192,16384x2048 REGIA_CULL_COST_FRAMES=128 \
cargo test -p regia_render --lib cull_cost -- --ignored --nocapture
```

## Draw order

The cull lists every cluster in (instance, cluster) order on every run and adapter (ADR-2294): no slot
comes from an atomic counter. A pixel where two triangles meet at exactly the same depth therefore has
the same owner in every run and every frame, and two renderers given the same scene write the same
visibility IDs. When lists overflow the cull capacity (`max_clusters` clamped to 1024..=16 777 216),
the back-face-culled list keeps first; the counters report the rest as `dropped_clusters`.
`visbuffer::tests::cull_order` checks the order by reading
the lists back.

## Lightmap receivers

A rigid lightmap receiver ([lightmaps](lightmaps.md)) can draw on the path: its UV2 sits beside its
clusters, and the resolve's `LIGHTMAP` variant reads its lightmap texel. A voxel-far cluster of that
receiver indexes vertices appended past the chart, so those pixels keep the probe light instead of
reading past the UV2 range. Only the materials that hold a receiver need that variant. `render.get_visbuffer` `last_frame.receiver_instances` counts the
receivers on the path, and `last_frame.receiver_materials` the materials with one: those are the ones
the per-material resolve shades through the `LIGHTMAP` variant (ADR-2689); every other material
keeps the plain pipeline. `resolve.lightmap_materials` is what the last resolve shaded that way. The single-pass
resolve has no `LIGHTMAP` variant, so a frame with a receiver falls back to the per-material resolve
(`resolve.fallback: vis_lightmap_receivers`).

## Masked materials

Alpha-tested (`ALPHA_MASK`) meshes, such as leaf cards, grass and fences, draw on the path on a device that
carries the texture table ([bindless](bindless.md)). Their clusters rasterize with an alpha-tested pipeline that
reads each pixel's base-color alpha through the table and drops what the forward alpha test drops. One pipeline
serves every masked material, so no draw binds a material. The holes are in the depth before the occlusion
pyramid is built, so nothing seen through a leaf is culled. The resolve shades the pixels the test kept.

A masked mesh stays on the forward path, as before, when:

- the device has no texture table (`render.bindless_report` says why);
- the frame may hash its alpha test (TSR or TAAU with hashed alpha): the cluster raster tests the hard cutoff only;
- the material has hair cards or parallax.

For ECS foliage with a separate `MeshLodChain`, the ordinary chain's cluster handoff still accepts only
opaque materials. Use `DisableMeshLod` (or omit that chain) to keep the source mesh submitted while its
cluster DAG picks detail at every distance. Otherwise the ordinary chain can select a coarser forward draw.

A masked mesh casts its shadow through the forward shadow passes, which alpha-test. Masked clusters never take the
software rasterizer. A persistent instance set with a masked material draws alpha-tested on a table device, from its
first visibility frame. It casts no shadow, whatever its policy says: the cluster shadow raster has no alpha test, and a
set has no forward shadow draw. Where the set's test cannot run (no table, hair cards, parallax) it draws nothing, since
it has no forward path to stay on.

To check a level: `render.get_visbuffer` `last_frame.masked_instances` counts the masked instances on the path and
`masked_materials` their materials. `masked_tested: true` means they were alpha-tested this frame. With
`masked_instances` above 0 and `masked_tested` false, the frame did not bind the table, and the masked clusters
wrote no pixels or occluding depth. `render.set_visbuffer` returns the same counts under `before.masked` and `after.masked`. Tests and benches read
`FrameRenderer::visbuffer_counters` `masked_clusters` (a blocking readback).

To price the alpha-tested raster on a card, add `REGIA_VIS_BENCH_MASKED=1` to the `visbuffer_dense_bench` command above:
all 64 materials become masked, and each row prints its masked instances, masked clusters and `tested`.

## Swaying plants

A plant adopted with `FoliageSway::adopt` draws as clusters (spec 98 § 8d, ADR-3916). It moves the way the forward
path moves it: both call one function, `regia::foliage_sway::offset`. Nothing needs turning on beyond the path and the
scene wind (`render.set_foliage_wind`, [Foliage wind](foliage-wind.md)).

- **Cull.** While some sway moves, an instance's bounds grow by its material's offset bound, and its clusters skip the
  normal cone test. The LOD cut does not move with the wind. In calm air the frame is byte for byte the frame of the same
  material without sway.
- **Far levels.** Voxel far levels do not sway. An instance with such levels fades its sway from 70 % of the distance
  where the first one can be selected, and is at rest from 95 %.
- **Shadows.** An opaque plant casts as swaying clusters. A masked one keeps its forward shadow draws, which alpha-test and
  sway. Both redraw their pages through the dynamic-caster class. An opaque swaying persistent set does so while it moves;
  a masked persistent set casts no shadow until the cluster shadow raster supports its alpha test.
- **Software raster.** Opaque swaying clusters take it like any other and do not crack against hardware neighbours.
  Masked clusters stay in hardware.

To check a level, read `render.get_visbuffer` `last_frame.sway`:

| Field | Meaning |
|---|---|
| `instances` | swaying instances on the path |
| `materials` | their distinct materials |
| `moving` | some sway moved this frame, so the cull grew those bounds |
| `forward` | swaying meshes that stayed forward this frame, by reason; only the reasons that happened are listed |

| `forward` reason | Why the mesh stayed forward | What to do |
|---|---|---|
| `graph_surface` | the sway sits on a surface graph of its own, and the resolve shades the standard surface only | adopt a plain material |
| `translucent` | the material blends, transmits, or is hair or parallax | |
| `hashed_alpha` | a masked material on a frame that hashes its alpha test | turn hashed alpha off, or accept forward |
| `no_texture_table` | a masked material on a device without the texture table | |
| `table_full` | the frame holds 256 distinct sways already (tint variants share one) | share sway values |
| `other` | skinned, morphing, a LOD cross-fade, a render override, a thin film, subsurface scattering, or no clusters | |

`entities.forward_reasons` says what the ECS bridge decided before any of this: `sway_graph_surface` is the bridge's
`graph_surface`, for an entity whose meshlets it does not insert. A persistent set has no forward path: past 256 sways it
draws at rest and counts under `table_full`.

Known limits: traced sun shadows, mesh SDFs and baked probes keep the rest pose. An adopted material on the path keeps a
second copy of its RGBA8 maps for its standard twin; cooked and shared images are not doubled.

To price the sway on a card, add `REGIA_VIS_BENCH_SWAY=1` to the `visbuffer_dense_bench` command above, with or without
`REGIA_VIS_BENCH_MASKED=1`: every material is adopted, the wind blows, and each row prints its swaying instances.

## Per-instance level range

Each instance's cull work covers only the cluster levels its distance can select. A mesh's clusters are stored
level by level. For each level the renderer keeps bounds on the level's errors and on how far its spheres reach.
The instance cull drops a level that no cluster in it could pass at the instance's distance. Clusters it drops would
fail the LOD test anyway, so the frame is the same. A near tree spends no cull work on its voxel far levels, and a
far one none on its finest levels. `visbuffer_counters` `work_items` shows the effect.

## LOD hysteresis

A cluster is drawn when its own projected error is at most the threshold and its parent's is above it. When that
error sits on the threshold, a camera move of a fraction of a pixel used to swap the whole cluster. The pass now
remembers both threshold comparisons for each cluster in each instance. While a boundary error stays inside
a band around the threshold, its stored comparison is kept. A comparison changes when the error leaves the band.
This preserves cuts that select several levels simultaneously. The first frame, a missing history and a band of zero use the hard cut.

The symmetric band half-width defaults to `LOD_HYSTERESIS_FRACTION` (one eighth) of `lod_threshold_px`. `VisBufferSettings::lod_hysteresis_px`
overrides it: `None` is that fraction, `Some(0.0)` is the hard cut, and `Some(px)` is a width in pixels. It is a
pass setting, not a field of `RenderProfile`. A still camera draws the same clusters as the hard cut, including
on the threshold, because the stored level is the level the hard cut chose. Instances near the threshold can
spend a few more cull work items once a level is stored; the drawn clusters do not change.

`render.vis_lod_hysteresis` reads and sets the band and reads how many instances changed their selected cut last frame.

| `band_px` | Effect |
|---|---|
| omitted | read the band |
| `null` | restore the fraction of the threshold |
| `0` | the hard cut |
| a positive number | that many pixels |

The result is `{ band_px, band_source, fraction, threshold_px, switches, cleared }`. `band_source` is `fraction`
or `pixels`. `switches` is last frame's count (`VisBufferStats::lod_switches`). `cleared` is false: the call
does not drop stored levels. `render.get_visbuffer` also reports `settings.lod_hysteresis_px`,
`settings.lod_hysteresis` and `last_frame.lod_switches`.

| Error | When |
|---|---|
| `vis_lod_hysteresis_bad_params` | the parameters are not an object, or `band_px` is not a number or null |
| `vis_lod_hysteresis_bad_band` | `band_px` is negative or not finite |
| `vis_lod_hysteresis_no_renderer` | the world has no `RuntimeRenderer` |

A camera cut drops the stored levels. The frame calls `VisBufferPass::clear_lod_history` from `begin_cut_frame`. An eye jump past the camera-cut
distance, an instance layout or identity change, resident geometry replacement, persistent-set changes,
a main-view frame without visibility geometry, and a replaced history buffer do the same. The draw list gives each instance its object key;
mesh/material slots and motion continuity provide additional invalidation checks. Anonymous draws retain history while their mesh/material slots and
previous-model continuity match; stable object keys distinguish replacements at an identical pose. The first stored level
after a clear is not a switch. Wind does not move the cut: the LOD spheres stay at rest.

### LOD hysteresis in split screen

Each window view keeps its own stored levels (ADR-8620). When the renderer draws another view, the
outgoing view's levels are put aside and the next view's come back, together with its cluster
visibility bits: two buffer copies each way per view change, none with one view. A view change is
therefore not a camera cut, even when the views stand far apart, and a camera cut in one view drops
that view's levels only. `render.get_visbuffer` `last_frame` reports:

| Field | Meaning |
|---|---|
| `lod_history_discards` | stored levels dropped since start: camera cuts, eye jumps within a view, changed instance lists, replaced buffers. A fresh view's first frame does not count |
| `lod_history_copies` | buffer copies of stored levels between views since start; stays 0 with one view |
| `view_histories` | views that own buffers for their put-aside bits or levels: 0 with one view, one per view in split screen |
| `view_history_bytes` | the GPU bytes of those buffers |

A view that has not drawn for `OCCLUSION_VIEW_RETIRE_ROUNDS` (16) rounds gives its bits, levels and
Hi-Z pyramid back on its own: `view_histories` and `view_history_bytes` drop, and so does its
`pyramid_bytes` in `render.visible_entities` (`book/agents/visibility.md`). A round ends when a
view draws a second time. Coming back, the view starts as a new one: its first phase draws no
clusters and its levels start from the hard cut.

To check split screen: hold both views still for a few frames, read `last_frame.lod_history_discards`, keep
drawing, and read it again. If it rose without a camera cut, report both readings and
`last_frame.lod_history_copies`.

Swaying persistent sets refresh shadow pages only while the visibility path and cluster shadows are on.
Shadow-off instances and masked sets contribute no dynamic shadow box. A casting set's box follows its
shadow scale and lift before adding the sway bound, so a styled moving shadow stays inside refreshed pages.

## Dense-scene acceptance

`visbuffer::tests::close` (lib tests, ADR-4906) runs one scene with every kind of instance the path takes: an opaque
rock, a masked rock, a swaying rock and a lightmap receiver, all on cooked cluster DAGs. The residency budget holds
the bases and about one mesh's fine bands. The LOD band is on. The camera cuts between two groups 297 m apart,
seven times. On every frame, including the first frame after each cut, the test checks:

- the path holds every instance in its class (`instances`, `masked_instances`, `sway_instances`,
  `receiver_instances`), and the cull and shadow lists drop no cluster;
- each rock prints: most of its body differs from the frame without it, most of its shadow is darker than the frame
  where it casts none, and its body stays close to the reference frame (a wrong material moves it far). The
  reference frames come from fresh renderers on the same device, so the test needs no golden;
- residency stays within its byte and upload budgets;
- after two cycles, a frame creates no path object, and the arena and the path's buffers keep their size.

A residency-managed receiver draws forward with its lightmap: its UV2 cannot follow the managed mesh's
non-contiguous vertices (ADR-3118). The run without residency puts the receiver on the path too. The same checks run
on the fallbacks:

| Device | What draws where |
|---|---|
| Texture table, residency | opaque, masked and swaying rocks on the path; the receiver forward |
| Texture table, no residency | every rock on the path |
| No texture table (A15 fallback) | the masked rock forward; the rest as above |
| Exactly `RESOLVE_STORAGE_BUFFERS` (11) storage buffers per stage | the path runs; the receiver forward (its variant needs 12) |
| One fewer | the classic path draws everything; no path object is created |

The resolve binds the forward view group's seven storage buffers and its own four, so the floor is 11. Before
ADR-4906 it was 10, and a device with exactly 10 failed the resolve's pipeline layout instead of falling back.

The dense-scene default (`RenderSettings::visibility_default`) stays off in every preset. With residency and the LOD
band on (`REGIA_VIS_BENCH_RESIDENCY=1`, "Measuring a machine"), an RTX 5090 and an RTX 5080 both cross over at
1.085 tri/px at 2560x1440 (bracketed), which is 3,999,744 triangles or 1,024 meshes. The path saves 22 to 28 % there.
Turning the default on is the lead's or owner's decision.
