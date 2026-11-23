# Two-phase occlusion of the depth prepass

The forward renderer draws opaque meshes into a depth prepass first, and the colour pass then
shades only what that depth keeps. Until this feature, the prepass itself drew every opaque
instance in the view, even the ones hidden behind a wall or a hill. Only the colour pass was culled,
against a Hi-Z pyramid of that depth. With two-phase occlusion on, the prepass is culled too
(`docs/specs/98-visibility-buffer.md`, ADR-1887):

1. **Phase 1.** Each depth-prepass instance is tested on the GPU against the frustum and against
   *last frame's* depth pyramid. Instances that pyramid does not hide are drawn.
2. The pyramid is rebuilt from that depth.
3. **Phase 2.** The instances phase 1 skipped are tested against the new pyramid. The ones it does
   not hide are drawn in a second depth pass.

Phase 2's pyramid only holds depth that this frame really has, so it never hides anything the
full prepass would show. **The frame is the same with the switch on or off**: the depth buffer the
two phases write is the one the unculled prepass writes. Only the prepass's GPU work changes.
After a camera cut, phase 1 guesses badly and phase 2 draws the rest, so that frame costs about
what it did before.

The visibility buffer (`render.get_visbuffer`, [visbuffer.md](visbuffer.md)) already culls its
clusters this way. This switch covers the forward meshes that are not on it.

## When it runs

It runs on the main view when all of these hold:

- the switch is on (preset Desktop High or Ultra, or the override below);
- the frame has a depth prepass and is single-sample (no MSAA);
- the prepass is GPU-driven: the device has multi-draw indirect with a first instance, and
  the quality has `gpu_driven` on;
- Hi-Z occlusion is on (`FrameRenderer::set_hiz_occlusion`, on by default);
- the prepass has at least 2048 instances. Below that, building the pyramid costs more than the
  draws it saves (the same threshold as the colour pass's Hi-Z test).

A render-to-texture view (a mirror or a security camera) never takes it.

The two phases are the frame graph's optional node `prepass_occlusion` (ADR-4905): the graph holds
it only while the switch is on, so `render.graph` lists it in `graph.order` exactly then, and with
the switch off nothing is recorded or created ([Render graph](render-graph.md)).

## Agent methods

`render.get_occlusion` (read only) returns:

| Field | Meaning |
|---|---|
| `two_phase` | the switch in effect: the override, else the preset's column |
| `setting` | the override (`true`, `false`), or `null` for the preset |
| `preset` | the active profile's `two_phase_occlusion` column |
| `visibility_buffer` | the profile has the visibility buffer on (its clusters always cull in two phases) |
| `renderer` | the last main-view frame, or `null` without a runtime renderer |
| `renderer.enabled` | the renderer's switch |
| `renderer.active` | the last frame culled its prepass in two phases |
| `renderer.skipped` | why the last main view did not: `off`, `no_prepass`, `no_gpu_objects`, `multisampled`, `few_instances`, `hiz_off` (`null` when that frame ran it). A texture view does not update this report, so `texture_view` is not written |
| `renderer.history` | phase 1 tested last frame's pyramid. `false` on the first frame after the switch turns on, after a size change, and when no pyramid was built the frame before |
| `renderer.instances` | depth-prepass instances tested |
| `renderer.unbounded` | instances with no safe bound (compute-skinned twins, vertex-stage material graphs that declare no largest offset): always drawn in phase 1. A wind sway declares one and is culled on its grown bound (ADR-5389) |
| `renderer.batches` | instanced batches: one indirect multi-draw per batch and phase |
| `renderer.frames` | frames that culled in two phases since the renderer was made |
| `renderer.hiz_occlusion` | the renderer's Hi-Z occlusion switch |
| `shadows` | the shadow casters' cull, see [Shadow casters](#shadow-casters-two-phase-off-by-default) (`null` without a runtime renderer) |
| `shadow_setting`, `shadow_preset` | the shadow casters' override and profile column, see [Profile column](#profile-column) |

`render.set_occlusion { two_phase: true | false | null }` sets the override (`null` returns to the
preset). It applies from the next extracted frame and returns `{ before, after, note }`. Errors:
`F0003` when the app has no runtime renderer (no `RenderSettings`); invalid parameters otherwise.

### Checking it

```json
{"method": "render.set_occlusion", "params": {"two_phase": true}}
{"method": "render.get_occlusion"}
```

Let at least two frames render between the calls. Then expect `renderer.active: true` and
`renderer.history: true` on a dense view. If `active` is false,
`skipped` says why. A screenshot with the switch on and off must be identical. Any difference is a
bug: report it with both captures.

## In code

- `RenderProfile::two_phase_occlusion`: the preset column (on in Desktop High and Ultra).
- `RenderSettings::two_phase_occlusion: Option<bool>`: the override;
  `RenderSettings::effective_two_phase_occlusion()` gives the switch in effect.
- `FrameRenderer::set_two_phase_occlusion(bool)` / `two_phase_occlusion()`: hosts that drive the
  renderer themselves. The runtime calls it every frame from the settings.
- `FrameRenderer::prepass_occlusion_stats()`: the report above.
- `FrameRenderer::prepass_occlusion_counters(&device)`: reads back how many instances each phase
  drew. This blocks on the GPU, so use it in tests and benches only.

## Cost

The cull is one 64-wide compute dispatch per phase over the instance list (32 bytes per instance
and 20 bytes per phase command). The extra pyramid build from phase 1's depth is the main cost; on
lavapipe at 96×96 it is noise. Phase 2's depth pass is one indirect multi-draw per batch. The gain is
the hidden instances' vertex work in the prepass, so it grows with dense, occluded scenes (scatter
behind terrain, interiors). Card numbers are in [Which scenes gain](#which-scenes-gain).

## Shadow casters (two-phase, off by default)

The shadow passes can cull their casters the same way (ADR-2072, `specs/98` § 10a). Each shadow-map
*kind* (each sun cascade, and the local-light atlas) keeps a coarse pyramid of its map's farthest
depth. When a kind is redrawn:

1. **Phase 1.** Each caster of the kind's jobs is tested against the pyramid as the kind's last
   redraw left it, with the light matrix that redraw used. The ones it does not hide draw, with the
   page clears, the visibility-buffer clusters and the terrain.
2. The pyramid is rebuilt from that depth.
3. **Phase 2.** The casters phase 1 skipped are tested against the new pyramid with this frame's
   matrix; the ones it does not hide draw in a second pass over the same map.

**The shadow maps are the same with the switch on or off, texel for texel**, so every frame is the
same too. Only the shadow passes' vertex work changes. Cascades that keep their depth (nothing moved)
draw nothing either way and are not culled.

### When it runs

On the main view, for each kind redrawn this frame, when the shadow casters are GPU-driven (the
same device and quality conditions as above) and the kind's jobs hold at least `min_casters` caster
instances (default 512). A sun page redraw holds only the casters over its pages, so it often stays
under the threshold; lower it with `min_casters` to cull those too. Render-to-texture views never
take it.

### Agent methods

`render.set_shadow_occlusion { on: true | false, min_casters?: 1.. | null }` switches it on the
renderer (no preset turns it on) and returns `{ before, after, note }` with the `shadows` block below.
While it is on, the frame graph holds the optional node `shadow_occlusion` right before `shadows`
(ADR-5178); `render.graph` lists it in `graph.order` and in `dispatch.recorded`
([Render graph](render-graph.md)).
Errors: `F0003` when the app has no runtime renderer; invalid parameters otherwise (`on` missing or
not a boolean, `min_casters` below 1 or not a whole number).

`render.get_occlusion` gains `shadows` (`null` without a runtime renderer):

| Field | Meaning |
|---|---|
| `shadows.enabled` | the renderer's switch |
| `shadows.active` | at least one kind culled in two phases in the last main-view frame |
| `shadows.skipped` | why none did: `off`, `no_gpu_objects`, `no_jobs` (no map was redrawn), `few_casters` (every redrawn kind is under `min_casters`), `failed` (the cull could not make its resources this frame; logged, the casters drew directly); `null` when active |
| `shadows.min_casters` | the threshold in effect |
| `shadows.instances` | caster instances tested, all kinds |
| `shadows.phase1`, `.phase2`, `.culled` | casters drawn by each phase and by neither, all kinds (`null` unless the last frame culled and the app has a render device) |
| `shadows.pyramid_bytes` | GPU memory the pyramids hold (0 while off) |
| `shadows.frames` | frames that culled at least one kind since the renderer was made |
| `shadows.kinds[]` | per kind, `kind` = `cascade0` … `cascade3`, `atlas` |
| `kinds[].active` | the kind culled this frame |
| `kinds[].jobs` | its shadow jobs (a whole cascade, its dirty pages, or atlas tiles) |
| `kinds[].history_jobs` | jobs whose phase 1 had last redraw's pyramid (the rest draw every caster in phase 1) |
| `kinds[].instances` | caster instances tested |
| `kinds[].unbounded` | instances with no safe bound (vertex-stage graphs that declare no largest offset or cast at a shadow scale or lift, morphs, compute-skinned twins): always drawn in phase 1. A wind sway declares one and is culled on its grown bound (ADR-5389) |
| `kinds[].phase1`, `.phase2`, `.culled` | that kind's casters drawn by each phase and by neither; the three add up to `instances` (`null` for a kind that did not cull) |
| `shadow_setting` | `RenderSettings::shadow_occlusion`: `true` / `false` when forced (`render.set_shadow_occlusion` sets it), `null` for the profile's column (ADR-2482) |
| `shadow_preset` | the profile's `shadow_occlusion` column (off in every preset) |

The counts come from a GPU readback, so `render.get_occlusion` waits for the GPU when the last frame
culled, and makes a small staging buffer each time. Call it from a test or a check, not every frame.
A frame that culled nothing reads nothing back. The `before` and `after` of
`render.set_shadow_occlusion` never read back, so their counts are `null`.

### Checking it

```json
{"method": "render.set_shadow_occlusion", "params": {"on": true}}
{"method": "render.get_occlusion"}
```

Let two frames render with the camera moving (so cascades redraw). Expect `shadows.active: true`,
`history_jobs` equal to `jobs` and `culled` above 0 on a dense view. `phase2` is near 0 once the view
settles; it rises for a frame when a large caster goes away. Screenshots with the switch on and off
must be identical; any difference is a bug.

### In code

- `FrameRenderer::set_shadow_occlusion(bool)` / `shadow_occlusion()`; off frees the pyramids.
- `FrameRenderer::set_shadow_occlusion_min_casters(Option<u32>)` (`None` = 512).
- `FrameRenderer::shadow_occlusion_stats()`: the report above.
- `FrameRenderer::shadow_occlusion_counters(&device)`: per kind, how many casters each phase drew and
  how many neither did. Blocks on the GPU: tests, benches and `render.get_occlusion`.
- `FrameRenderer::shadow_map_depth(&device, kind)`: a map's depth, for differential tests. Blocks.

### Cost

Per culled kind: two 64-wide dispatches over its casters (48 bytes per caster, 20 bytes per phase
command), one pyramid build (64 reads per base texel, then the mips), and one more render pass.
Each caster is also one indirect command in each phase's multi-draw, where the unculled pass has one
draw per mesh. wgpu validates indirect commands in release builds too: it records a small entry per
command on the CPU and runs its own validation dispatch before each pass. So the cull trades vertex
work for per-caster CPU and a few dispatches, and it needs a scene where it hides many casters to pay
off. `shadow_occlusion_cost` in `tests/shadow_occlusion.rs` measures the whole shadow stage on a card.
The gain is the hidden casters' vertex work, so it grows with dense scatter under roofs and
overhangs. Card numbers are in [Which scenes gain](#which-scenes-gain).

Since ADR-2482 every culled kind's phase 1 shares one compute pass, and each kind's pyramid shares one with
its phase 2: a culled frame records one compute pass plus one per culled kind
(`regia_render::shadow_occlusion_work`, checked by `tests/shadow_occlusion_cost.rs`). The commands are
still one per caster; one command per batch is the next step (ADR-2482 § 2).

### Profile column

`RenderProfile::shadow_occlusion` turns the cull on for a profile (off in every preset), and
`RenderSettings::shadow_occlusion` (`Some(true)` or `Some(false)`) overrides it at runtime; the render
systems hand `RenderSettings::effective_shadow_occlusion()` to the renderer every frame.
`render.set_shadow_occlusion` sets that override too, so the next frame keeps the agent's choice.
`RenderSettings::preset()` ignores the column: a shipped preset with the cull on still reads as that preset.

## Which scenes gain

Both culls trade a few dispatches, a pyramid build and per-instance commands for the vertex work of
the instances they hide. They pay off where **many heavy casters are hidden behind or under an
opaque occluder**: thousands of instances with thousands of triangles each, such as a forest or a
scatter field behind a ridge, wall or building (the prepass cull), or foliage and props under a
roof, overhang, deck or cliff as the sun and the lights see them (the shadow cull). Open fields with
no occluder gain little or nothing. Cheap casters, like the 12-triangle boxes of the test fields,
lose.

`heavy_caster_occlusion_cost` in `crates/regia_render/tests/occlusion_gain_cost.rs` measures this
(ADR-5389). It uses the visibility-buffer bench's `4096x2048` layout: 4 096 instances of a
~2 048-triangle sphere, 64 materials, alpha-tested shadows, 2560x1440, four 2048² cascades redrawn
every frame and a 2048² atlas. Each class runs both culls off, the prepass cull on, and the shadow
cull on, in 3 rotated rounds of 24 timed frames each. The table gives GPU ms (median over the
rounds) on an RTX 5090 and an RTX 5080. "Hidden" means a wall covers the camera's view and a roof
covers the field from the sun and a spot light:

| Card | Class | Frame off | Frame, prepass cull | Frame, shadow cull | Prepass + colour cull | Shadows |
|---|---|---|---|---|---|---|
| 5090 | masked, hidden | 5.81 | 5.31 (-0.50) | 3.41 (-2.40) | 0.79 -> 0.16 | 2.90 -> 0.50 |
| 5090 | masked, open | 10.50 | 10.61 (+0.11) | 10.31 (-0.19) | 0.94 -> 1.05 | 2.91 -> 2.71 |
| 5090 | swaying, hidden | 6.32 | 5.77 (-0.55) | 4.20 (-2.13) | 0.68 -> 0.16 | 2.86 -> 0.68 |
| 5090 | swaying, no declared offset, hidden | 6.32 | 6.36 (+0.04) | 6.76 (+0.44) | 0.68 -> 0.76 | 2.87 -> 3.29 |
| 5090 | opaque, hidden | 5.53 | 5.12 (-0.41) | 3.23 (-2.31) | 0.61 -> 0.16 | 2.84 -> 0.50 |
| 5080 | masked, hidden | 8.79 | 7.83 (-0.96) | 5.54 (-3.26) | 1.13 -> 0.18 | 4.06 -> 0.73 |
| 5080 | masked, open | 17.67 | 17.18 (-0.49) | 16.95 (-0.72) | 1.41 -> 1.48 | 4.12 -> 4.07 |
| 5080 | swaying, hidden | 9.53 | 8.80 (-0.72) | 6.38 (-3.14) | 0.93 -> 0.21 | 4.04 -> 0.97 |
| 5080 | swaying, no declared offset, hidden | 9.37 | 9.50 (+0.13) | 10.12 (+0.75) | 0.92 -> 1.01 | 4.00 -> 4.67 |
| 5080 | opaque, hidden | 6.99 | 6.60 (-0.39) | 4.94 (-2.05) | 0.62 -> 0.19 | 2.78 -> 0.71 |

In the open rows, the frame differences are run-to-run noise in the forward pass. Read the stage
columns there: the prepass cull costs a little, and the shadow cull saves a little. Every culled
frame equals the unculled frame byte for byte (the test checks this).

With paged sun shadows (`REGIA_OCC_GAIN_PAGED=1`, the Desktop High default), on the RTX 5090:

| Class | Frame off | Frame, prepass cull | Frame, shadow cull | Prepass + colour cull | Shadows |
|---|---|---|---|---|---|
| masked, hidden | 2.88 | 2.24 (-0.64) | 2.88 (0.00) | 0.79 -> 0.15 | 0.003 -> 0.003 |
| swaying, hidden | 5.55 | 5.00 (-0.55) | 4.05 (-1.50) | 0.69 -> 0.15 | 2.07 -> 0.53 |
| swaying, no declared offset, hidden | 5.29 | 5.32 (+0.03) | 5.49 (+0.20) | 0.69 -> 0.76 | 1.82 -> 2.07 |

A static field keeps its pages, so its shadows cost nothing and there is nothing to cull. Swaying
casters redraw the pages they cover every frame, and that redraw is what the shadow cull removes.

**Swaying casters** take part when their material declares its largest vertex offset
(`SurfaceGraph::max_vertex_offset`). Every `FoliageSway` material declares one. Both culls then
test the mesh bound grown by that offset, as the colour pass's cull does (ADR-5389). A vertex graph
that declares no offset has no safe bound: it is counted in `unbounded`, draws in phase 1, and costs
the cull's overhead for nothing (the "no declared offset" row). If `unbounded` is high on a forest,
check that its wind materials declare their offset.

**What to switch on.** The prepass cull is on in Desktop High and Ultra. Leave it on. The shadow cull
is off in every preset (ADR-2482). Turn it on for a level that has this scene class
(`render.set_shadow_occlusion {"on": true}` or `RenderSettings::shadow_occlusion`). Then read
`render.get_occlusion`: on a hidden heavy field, `shadows.culled` should be most of
`shadows.instances`. With paged sun shadows the cull pays where casters move (swaying foliage, moving
props) under an occluder.

Run the case on a card:

```sh
REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 cargo test --release -p regia_render \
    --test occlusion_gain_cost -- --ignored --nocapture heavy_caster_occlusion_cost
```

`REGIA_OCC_GAIN_SCENES`, `REGIA_OCC_GAIN_SIZE`, `REGIA_OCC_GAIN_ROUNDS`, `REGIA_OCC_GAIN_FRAMES`,
`REGIA_OCC_GAIN_INSTANCES` and `REGIA_OCC_GAIN_PAGED=1` change the classes, the size, the rounds,
the frames, the field and the shadow paging.

## Masked occlusion of the draw list (CPU, Low and Deck)

The tiers without the GPU two-phase prepass cull hidden draws on the CPU instead (ADR-10072).
Each main-view frame, after the frustum cull, the renderer rasterizes its largest opaque rigid
meshes in view into a 256×128 software depth buffer
([`masked_occlusion`](../user-guide/masked-occlusion.md)) and tests every visible opaque rigid
draw's bounds against it. Hidden draws leave the colour, depth and velocity passes; the shadow
passes keep them (an occluded mesh can still cast a visible shadow).

**The frame is the same with it on or off.** The test is conservative: a draw is culled only when
every pixel its bounds could touch is covered by occluder surface nearer than the bounds' nearest
point, so the depth test would have rejected all of it.

### Tiers

| Preset | GPU two-phase prepass | CPU masked cull |
|---|---|---|
| Mobile Low, Mobile High | off | off (not measured on phones) |
| Desktop Low (and the ladder's Very Low) | off | **on** |
| Deck | off | **on** |
| Desktop High, Ultra | on | off |

### What it culls and what occludes

- **Occluders**: visible rigid meshes with an opaque standard material (no blend, mask, glass,
  hair, material graph, render override, LOD dither or morph), with CPU vertices, whose bounding
  sphere covers at least 8 % of the view's half-height. Largest first, at most 32 meshes and 8192
  triangles a frame. A single-sided material occludes with its front faces only (the GPU culls the
  back faces), a double-sided one with both.
- **Culled**: visible rigid opaque or alpha-tested draws with a rest bound, tested with the box
  around their bounding sphere (grown by a declared vertex offset). Blended, transmissive, hair,
  overridden (ghost, x-ray), skinned, morphed and visibility-buffer draws are never culled.
- **Not covered**: the terrain heightfield and other extension passes draw on the GPU and are not
  occluders; a ridge does not hide scatter behind it on this path yet.
- Main view only: render-to-texture views and stereo eyes skip it.
- **Pacing.** A frame with fewer than 32 cullable draws skips it (`few_draws`): the occluder raster
  would cost more than it saves. After 8 culling frames in a row that hide nothing, the cull backs off
  (`idle`) and builds the buffer once every 16 frames; the first frame that hides something ends the
  back-off. Skipping draws everything, so pacing changes cost, never the image.

### Agent methods

`render.get_occlusion` gains `masked`:

| Field | Meaning |
|---|---|
| `masked.enabled` | the profile's column in effect |
| `masked.preset` | the column of the preset the profile matches, `null` for a custom profile |
| `masked.renderer` | the last main-view frame, `null` without a runtime renderer |
| `masked.renderer.active` | the last frame rasterized occluders and tested its draws |
| `masked.renderer.skipped` | why not: `off`, `stereo`, `no_occluders`, `bad_matrix`, `few_draws` (fewer than 32 draws could be hidden), `idle` (backed off, below) (`null` when it ran) |
| `masked.renderer.occluders`, `occluder_triangles` | meshes and triangles rasterized |
| `masked.renderer.tested`, `culled` | draws tested, and the hidden ones dropped |
| `masked.renderer.raster_us`, `test_us` | CPU time of the rasterization and of the tests (render thread, microseconds) |
| `masked.renderer.frames` | frames that culled since the renderer was made |

`render.set_occlusion { masked: true | false }` sets the profile's column (with or without
`two_phase`; give at least one). It applies from the next extracted frame and leaves the preset
match (`RenderSettings::preset()`) alone. Errors: `F0003` without `RenderSettings`; invalid
parameters when neither key is given or `masked` is not a boolean.

```json
{"method": "render.set_occlusion", "params": {"masked": true}}
{"method": "render.get_occlusion"}
```

A screenshot with `masked` on and off must be identical. Any difference is a bug: report it with
both captures and the `masked.renderer` block.

### In code

- `RenderProfile::masked_occlusion`: the column above.
- `FrameRenderer::set_masked_occlusion(bool)` / `masked_occlusion()`: hosts that drive the renderer
  themselves (off on a bare `FrameRenderer`); the runtime sets it from the profile every frame.
- `FrameRenderer::masked_occlusion_report()` → `MaskedCullReport` (the block above);
  `MaskedCullSkip::as_str` gives the `skipped` names.
- `MASKED_CULL_WIDTH`, `MASKED_CULL_HEIGHT`, `MASKED_MAX_OCCLUDERS`, `MASKED_OCCLUDER_TRIANGLE_BUDGET`,
  `MASKED_OCCLUDER_MIN_SCREEN`, `MASKED_MIN_CULLABLE`, `MASKED_IDLE_FRAMES`, `MASKED_PROBE_INTERVAL`: the
  fixed limits.

Tests: `cargo test -p regia_render --test masked_occlusion_frame` (lavapipe: the counted wall, the
byte-identical frame, 16 camera steps, a back-facing and a blended wall).
