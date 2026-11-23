# 98: Two-phase occlusion and the visibility-buffer line (v0.3.x (a), T2.9)

Status: proposed (lane r2-visibility-buffer, 2026-10-30). ADR-1887. Sources: `docs/master-plan.md`
§ 4.1 (rows "GPU-driven culling + MDI" and "Visibility buffer + meshlets") and § 5.5 steps 2 and 4,
`specs/22` T2.9 and T2.12, `specs/27` § 4.2 stage 3. No public spec set was reviewed for this item,
so the Regia docs and the public papers cited in `docs/provenance.md` are the sources.

## 1. Purpose

The GPU-driven line in § 5.5 has two steps this spec covers:

1. **Two-phase occlusion for the depth prepass (F2), built here.** The colour pass already culls
   against a Hi-Z pyramid of this frame's prepass depth (ADR-1238), and the visibility buffer
   already culls its clusters in two phases (`crate::visbuffer`, ADR-353 to ADR-355). The forward
   depth prepass did not cull at all: on a dense scene (scatter behind terrain, interiors) every
   hidden instance paid its vertex work in the prepass. This spec culls that prepass in two phases
   (Haar & Aaltonen 2015), with an exact-image guarantee (§ 4).
2. **The compute software rasterizer for small clusters, built here** (§ 8a), the named stage of
   § 5.5 step 4 and the optional part of T2.9.
3. **Visibility buffer + meshlets as the default for dense static content, recorded here and not
   built.** The path exists and is opt-in (`RenderProfile::visibility_buffer`, `render.set_visbuffer`,
   cooked `.fmlt` meshlets routed from the ECS by `meshlet_bridge`). § 8 says what turning it on by
   default needs and why this lane did not.

Two-phase occlusion for shadow views (the other half of F2) is out of scope (§ 10).

## 2. Gating: a quality tier and a runtime switch

| Level | Item | Default |
|---|---|---|
| Quality tier | `RenderProfile::two_phase_occlusion: bool` | on in Desktop High and Ultra (and the offline Cinematic preset, which starts from Ultra); off in Desktop Low, the Deck and both Mobile tiers until measured there |
| Player / tool override | `RenderSettings::two_phase_occlusion: Option<bool>`; `RenderSettings::effective_two_phase_occlusion()` | `None` (the preset's column) |
| Renderer | `FrameRenderer::set_two_phase_occlusion(bool)`, set each frame by the runtime from the extracted value (`ExtractedFrame::two_phase_occlusion`) | off for hosts that drive `FrameRenderer` themselves |
| Agent | `render.set_occlusion { two_phase }` | |

A frame takes the cull only when it can (`PrepassOcclusionSkip` names the first reason it cannot,
checked in this order): the switch on (`off`), a depth prepass (`no_prepass`), single-sample
(`multisampled`), Hi-Z occlusion on (`hiz_off`), the GPU-driven storage-object prepass
(`IndirectStrategy::MultiDraw*`, `no_gpu_objects`), and at least 2048 depth-prepass instances
(`HIZ_MIN_RIGID_CASTERS`, the colour pass's threshold: below it the full-frame pyramid costs more than
the draws it rejects; `few_instances`). A frame whose depth prepass draws on the CPU path never reaches
the check, so it reports `no_gpu_objects` even with the switch off. A render-to-texture view returns
before the check and leaves the main view's report as it was, so `texture_view` is kept in the enum
but is not reported today (ADR-5823).

## 3. API shape (Rust)

Crate `regia_render`:

```rust
pub struct PrepassOcclusionStats {      // FrameRenderer::prepass_occlusion_stats()
    pub enabled: bool, pub active: bool, pub skipped: Option<PrepassOcclusionSkip>,
    pub history: bool, pub instances: u32, pub unbounded: u32, pub batches: u32, pub frames: u64,
}
pub enum PrepassOcclusionSkip { Off, NoPrepass, NoGpuObjects, Multisampled, TextureView, FewInstances, HizOff }
impl PrepassOcclusionSkip { pub fn name(self) -> &'static str }   // agent protocol names
pub struct PrepassOcclusionCounters { pub phase1: u32, pub phase2: u32, pub culled: u32 }

impl FrameRenderer {
    pub fn set_two_phase_occlusion(&mut self, enabled: bool);
    pub fn two_phase_occlusion(&self) -> bool;
    pub fn prepass_occlusion_stats(&self) -> PrepassOcclusionStats;
    pub fn prepass_occlusion_counters(&self, device: &RenderDevice)
        -> Result<Option<PrepassOcclusionCounters>, RenderError>;   // blocking: tests, benches
}
```

Files: `src/prepass_occlusion.rs` (GPU side: pipelines, buffers, history, readback),
`shaders/prepass_occlusion.wgsl` (`cs_phase1`, `cs_phase2`), `src/frame/prepass_two_phase.rs` (which
frames take it, the per-instance list, the renderer methods), `src/frame/prepass.rs` (the phase
draws and the phase-2 depth pass), `src/frame/steps/depth_passes.rs` (the ordering: since ADR-4905 the
two phases are the frame graph's optional built-in node `prepass_occlusion`, recorded there through the
graph's dispatch; `src/frame.rs` gains no call for it). `GpuCuller` gains two crate-private
accessors (the pyramid view, the depth instances' source order).

**No file format** is added or changed. The cooked meshlet format (`.fmlt`,
`MESHLET_FORMAT_VERSION = 1`, `specs/12` § 4a) is untouched, so no migrator is needed.
`RenderProfile` is not serialized.

## 4. Algorithm and the exact-image guarantee

Per main-view frame, when the cull runs:

1. **Instance list (CPU).** One 32-byte record per depth-prepass instance, in object-buffer order:
   world bounding sphere, index range, base vertex, and its slot (`first_instance`). The sphere is
   the colour cull's (`gpu_cull::draw_sphere`: a skinned draw's joint-palette sphere, else the mesh
   bound under the model plus `inflate`). A vertex-stage material graph is bounded only when it
   declares its largest offset (`SurfaceGraph::max_vertex_offset` > 0, as a wind sway does; ADR-5389):
   `inflate` then grows its bound by that offset. Draws with no safe bound (vertex-stage graphs that
   declare no offset, compute-skinned twins) get a negative radius and are always drawn in phase 1.
2. **Phase 1 (compute, 64 threads per group).** Frustum test with this frame's unjittered planes.
   If the pyramid holds a previous frame's depth (same pyramid texture, same size), also the
   conservative Hi-Z sphere test of ADR-1238 against it, projected with the view-projection that
   pyramid was built with. The sphere's screen rectangle is widened by one pixel on every side:
   the depth is rasterized with the temporal jitter (up to half a pixel) and the test projects
   without it. The output is one `DrawIndexedIndirect` per instance with an instance
   count of 0 or 1.
3. **Phase 1 depth pass.** The instanced batches draw with one `multi_draw_indexed_indirect` per
   batch over that batch's commands, so instance order within a batch is the direct draw's. The
   shared draw (terrain) also draws here, so it occludes in phase 2.
4. **Pyramid** from phase 1's depth (the existing builder).
5. **Phase 2 (compute).** Instances phase 1 did not draw take the frustum and Hi-Z tests against
   the new pyramid with this frame's matrix.
6. **Phase 2 depth pass** (depth loaded), one multi-draw per batch.

The colour cull then rebuilds the pyramid from the complete depth as before. That pyramid, and the
matrix, become the next frame's phase-1 history. Without history (first frame, resize, new
pyramid), phase 1 draws every instance in the frustum and steps 4 to 6 are skipped.

**Guarantee.** Phase 2's pyramid is built from a subset of this frame's opaque depth writes, so
every texel is at least as far as the full prepass depth. An instance it hides is hidden by the full
prepass too (with the one-pixel margin, also under the temporal jitter: the test of ADR-1238
alone, as the colour cull uses it, can miss a jittered pixel). Under the `Less` depth test, opaque and alpha-tested writes keep the nearest value
in any order. So the depth after both phases equals the unculled prepass depth, and the colour pass,
SSAO, decals and everything downstream are unchanged bit for bit. Phase 1's history only moves
work between the phases; it can never drop an instance.

## 5. Agent-protocol surface and guide page

- `render.get_occlusion` (read): `{ two_phase, setting, preset, visibility_buffer, renderer: { enabled,
  active, skipped, history, instances, unbounded, batches, frames, hiz_occlusion } | null, shadows,
  shadow_setting, shadow_preset }`. `shadows` is the shadow casters' cull (§ 10a; null without a
  renderer); `shadow_setting` and `shadow_preset` are its override and profile column (ADR-2482).
- `render.set_occlusion { two_phase: true | false | null }` (EditScene): `{ before, after, note }`.
- Error codes: `F0003` (no runtime renderer). No new codes.
- Guide topic `occlusion` (`guide.topics`), page `book/agents/occlusion.md`, `book/COVERAGE.md` rows.

## 6. Determinism

The cull is per instance with no atomics, and each command depends only on its own instance and
the pyramid, so the draw set is a pure function of the scene, the view and the history. The image
does not depend on the draw set (§ 4). Lockstep and replay are unaffected: the feature is render
only and reads no simulation state.

## 7. Tests and acceptance

`crates/regia_render/tests/two_phase_occlusion.rs` (headless lavapipe):

1. **Same image, culled work.** A wall in front of 2304 small boxes. Two renderers, one with the
   switch on, draw the same four frames, with TAAU off and again with it on (the jittered
   depth), and every frame must match byte for byte. Frame 1 has no
   history and phase 2 draws nothing. Frame 2 has history, phase 1 skips at least 200 boxes, and
   phase 2 draws nothing. Frame 3 moves the wall out of view: phase 1 still skips the boxes and
   phase 2 draws more than 200 of them. Frame 4 has them back in phase 1: exactly (phase 2 draws
   nothing) without the jitter, and at least four times as many in phase 1 as in phase 2 with it,
   since last frame's jittered pyramid differs from this frame's by up to half a pixel.
2. **Skip reasons.** `off`, `few_instances`, `no_prepass` and `hiz_off` are each reported, and no
   frame is counted.
3. **Tier column and override.** Each preset's column, and the override in both directions.
4. **Every depth pipeline.** The same four frames with every third box alpha-tested and every fifth
   double-sided, so the multi-draws run through the masked and cull-none pipelines.
5. **A cut creates nothing.** A level that reserved a dense view (`prewarm` with 2048 or more
   instances) gets the cull's objects on its warm-up frames: `a_cut_onto_a_dense_view_creates_nothing`
   and its `_with_two_phase_occlusion` twin (`frame.rs`) count zero creates on the cut.

Unit tests: the uniform and instance layouts against the shader, and the protocol names of the skip
reasons (`prepass_occlusion.rs`). Agent tests: get/set, null clears, bad parameters, and the
missing renderer (`regia_agent/src/occlusion.rs`).

Acceptance on hardware (the lead runs it; § 9): on a dense DO27 or showcase view, the prepass GPU
time with the switch on is lower than with it off, and the frames are identical.

## 8. Visibility buffer by default: what it needs (not built here)

The visibility buffer is gated the same way: the profile column `visibility_buffer` (off in every
preset) and the runtime switch `render.set_visbuffer`. Turning it on by default in a preset is a
look change. It changes frames by the cluster LOD cut (ADR-041), so it needs re-blessed goldens and
the owner's look sign-off against the reference plates. Before that:

1. the LOD-handoff test that fails on integ (master plan § 4.1) must pass;
2. bindless must be the desktop default (§ 5.5 step 3), so the resolve shades in one pass;
3. there must be a GPU measurement on the 5080 and the 7900 showing that dense scatter at 10× the
   rock count holds the frame time (`specs/27` stage 3 acceptance).

**Built without a card (ADR-2689).** What does not need the cards or the owner is in place:

- *Lightmap receivers per material.* The pass records which material slots hold a receiver
  instance (`visbuffer/gpu/lightmap.rs`), and `resolve_variants` splits the frame's materials so
  only those take the resolve's `LIGHTMAP` pipeline. The frame's resolve loop takes the split when
  the frame owner applies the hookup (ADR-2689 lists the lines); until then a frame with any receiver
  still shades every material through the variant. `VisBufferStats::receiver_materials` counts them.
- *The path in the warm.* A game-frame warm of a profile that selects by density draws two
  window-size frames through the path, and the lists keep a reserve from the selection's thresholds,
  so the frames the scene turns dense create no path object (the Hi-Z groups follow the targets
  they bind, not the renderer's bind epoch of ADR-184)
  (`visbuffer/gpu/warm.rs`, `tests/visbuffer_warm_creates.rs`).
- *Skinned and outdoor frames in the differential.* `visbuffer_differential.rs` has a skinned scene
  (forward skinned meshes among the path's rigid ones), and `regia_terrain/tests/visbuffer_outdoor.rs`
  an outdoor one (terrain, physical sky, height fog, rocks), both under the strict tolerance.
- *Density units for the default.* `DensityMetrics` gives a frame's triangles per pixel, meshes per
  million pixels and clusters per 16 px tile; the bench prints them per row, and
  `DenseSceneSelect::at_density` turns a chosen density into the gate's counts for a frame size.
  The owner picks the default from the 5080/7900 rows (`book/agents/visbuffer.md`).

**The default behind a switch (ADR-3149).** `RenderSettings::visibility_default` (`preset` | `dense`,
`render.set_visbuffer { default }`) is off in every preset. On, Desktop High and Ultra get
`VisibilityPath::DENSE_DEFAULT`: dense selection at `DenseSceneSelect::MEASURED` (4,000,000 triangles or
4096 meshes, from the 5090 rows) and the per-material resolve (the single pass lost two of the four 5090
rows). `DenseCrossover::of_rows` turns a card's bench rows into the crossover density; the bench prints it.
With the switch on no golden moves (the lab goldens draw at `FrameQuality::LAB`; the default-look goldens
stay byte-identical because the gate never enters on them). A preset turns it on only after items 1-3 above,
the 5080/7900 crossovers and the owner's look decision on the plates (`samples/lookdev/tests/visbuffer_default_plates.rs`).

## 8a. Compute software rasterizer for small clusters

A triangle of a few pixels costs a hardware rasterizer far more per pixel than a large one (Laine &
Karras 2011). With `VisBufferSettings::software_raster` on, the cluster cull routes small clusters
to a compute rasterizer instead of the hardware list.

**Gating.** The switch is `VisBufferSettings::software_raster` (off by default) with
`software_raster_max_px` (default 16, at most 64), set through `FrameRenderer::set_visbuffer_settings`
or `render.set_visbuffer { software_raster, software_raster_max_px }`. It works only inside the
visibility-buffer path, whose tier gate is `RenderProfile::visibility_buffer`.

**Routing** (`vis_cull.wgsl`, `takes_software`). A selected cluster goes to the software list when
its bounding sphere is entirely in front of the near plane (so nothing needs clipping), and that sphere spans at most `software_raster_max_px` pixels at its nearest depth,
counting the `1 + tan²θ` stretch of its image at `θ` off the view axis. Without that, a wide lens
could hand the rasterizer a corner cluster past the size its `i32` edge products hold. The
cluster buffer gains a third range of `min(capacity, 2^24 - 1 - capacity)` entries. When that range
is full, a cluster draws in hardware: nothing is dropped.

**IDs.** A software cluster's ID is `1 << 31 | (capacity + k) << 7 | triangle`, so the resolve's and
the velocity pass's `phase × capacity + slot` lands on list entry `2 × capacity + k`. Neither pass
changes. `specs/12`'s formats are untouched: this is per-frame GPU data.

**Rasterization** (`vis_software.wgsl`). wgpu 30 does not have 64-bit atomics on every adapter, so
depth and ID cannot share one atomic word. Each pixel has two `atomic<u32>` words in storage
buffers, and every cluster is rasterized twice by the same entry point:

1. Depth pass: `atomicMax` of the inverted bits of the float depth (a cleared word means no coverage;
   a nearer depth is a larger word).
2. ID pass: the same computation. Where the triangle's word equals the stored word, `atomicMax` of
   its ID.

Both passes run the same compiled code, so the depth bits they compute are equal. Coverage follows
the hardware's rules: 1/256-pixel snapping, pixel-centre samples and the top-left fill rule. Back
faces are culled unless the camera is double-sided or the cluster's instance is (`Instance.flags.y`,
ADR-2689): a double-sided instance's clusters take the software range like any other, and their back
faces rasterize with the same coverage and fill rule as their front faces (the winding swap), as the
hardware's cull-none raster draws them. Past the software range's room a cluster draws in hardware
with its instance's cull mode: a double-sided one in the double-sided list, so it keeps its back
faces the frame the range fills. To keep that exact, each phase's part of the range holds the
single-sided clusters first and the double-sided ones after (`finalize_phase`, `place`).

**Shadow clusters stay in hardware** (ADR-2689). The software rasterizer exists to beat the
hardware's quad overhead on sub-pixel triangles of the main view, where every pixel also carries an
ID for the resolve. A shadow job writes depth only, into a page or atlas tile with its own viewport
and the forward shadow pipelines' slope-scaled bias; its LOD cut is already per shadow texel
(§ 4.2 stage 3), so its clusters are not sub-texel the way distant main-view clusters are. A compute
path there would need per-job word buffers the size of every page and the hardware bias rebuilt
bit for bit, or the cluster and forward shadows would disagree at the depth test.

**Order.** Each cull phase has one round: its hardware raster, then the rasterizer over what that
phase listed, then a fullscreen merge into the ID target and the scene depth with the hardware's
`LessEqual` test. Phase 1's software pixels are in the depth before the Hi-Z build, so small clusters
occlude like any other.

**Determinism.** Two triangles with exactly equal depth at a pixel resolve to the larger ID. Since
ADR-2294 every slot comes from the cull's prefix sums, not from atomics: the work items and each
phase's lists (back-face-culled, double-sided, software; the software range single-sided first, then
double-sided, each in order) are in (instance, cluster) order on every run and adapter, so the software tie and the hardware raster's `LessEqual` tie (the last fragment in draw
order) resolve the same each time. The cluster shadow cull (`vis_shadow.wgsl`) takes its slots the
same way since ADR-4778: past the work-item cap or `max_clusters` a capacity cut drops a stable
(job, instance, cluster)-order tail at each job's turn, instead of the scheduler-chosen subset the
atomic counters kept. `visbuffer/tests/cull_order.rs` reads the lists back and checks order and
run-to-run equality, over 100k+ clusters too, for the camera lists and the shadow lists at
capacity.

**Files.** `src/visbuffer/software.rs`, `vis_software.wgsl`, `vis_software_merge.wgsl`;
`vis_cull.wgsl` (`takes_software`, `place`, the round in `finalize_phase`); `state` grows from 26 to 32
words and `dispatch_args` from 16 to 32 bytes (ADR-2294: 48 words and 48 bytes, for the scans; ADR-3915:
`state` is 72 words, with the masked counts in `state[52..68)`, § 8b).

**Tests** (`visbuffer/tests.rs`, lavapipe): frames with the switch on match the hardware frames with
and without a hardware occluder and with the near plane through the scene, and under temporal AA;
two renderers draw identical bytes; a warm frame creates nothing, and a size change makes new
words; at the smallest capacity a full software range hands the rest to the hardware lists with
nothing dropped and the frame still matching; under a wide lens the same rock goes to compute on
the view axis and to hardware in the corner. Agent: the knobs and their bad values (`regia_agent/src/visbuffer.rs`).

**Needs a GPU.** Whether the compute path beats the hardware rasterizer, and at what
`software_raster_max_px`, is a real-GPU question (§ 9). No preset turns it on until then.

## 8b. Masked clusters and the per-instance level range (ADR-3915)

**Masked clusters.** An `ALPHA_MASK` material without hair cards or parallax (`visbuffer::gpu::alpha_tests`)
takes the path on a texture-table device when the frame does not hash its alpha test; elsewhere it stays forward.
The framework meshlet bridge registers this foliage too, leaving the device and per-frame gate to the renderer.
Its instance carries `Instance.flags.y` bit 1. The instance scan counts masked instances' work items in a second
channel, so they follow every opaque item and each phase's lists hold the opaque clusters first and the masked
ones after (the double-sided list likewise from its end). `finalize_phase` splits each list at the first masked
item's prefix: opaque counts in the existing DrawIndirects, masked counts in `state[52..68)`, the split in a
two-entry header after the cluster lists. Masked clusters never take the software range. Lists keep a fixed order
(masked, instance, cluster) on every run.

The masked draws follow the opaque ones in each phase's raster pass, with one pipeline per cull mode
(`vs_vis_masked[_reversed]`, `fs_vis_masked` in `vis_raster_masked.wgsl`): the material record comes from the
cluster's instance through the single-pass resolve's slot map, the base color from the texture table by index,
and the fragment drops what the forward color pass drops. Phase 1's Hi-Z pyramid is built from that depth, so a
hole never occludes. The resolve shades every visibility pixel as before. The frame binds the arena, the map and
the table once per frame (`VisBufferPass::bind_masked`); without it every masked fragment is discarded,
so untested cards contribute neither visibility IDs nor occluding depth. A masked
mesh-list instance keeps its forward shadow draws (alpha-tested); a masked persistent set casts no shadow (the
cluster shadow raster has no alpha test) and draws nothing where its test cannot run. The cull's `Instance.scale.x` is an
upper bound on the model's stretch (`max_stretch`), so the level range and the spheres hold for sheared models.

**Per-instance level range.** Each mesh's clusters are stored level by level. Per run of equal level a small table
keeps the smallest own error and the farthest reach of its LOD spheres from the mesh center, and the largest parent
error with the farthest reach of its parent spheres (`visbuffer/gpu/levels.rs`, cull binding 9). `instance_range`
drops a run no cluster of which can pass `lod_selected` at the instance's distance (1e-3 relative margin), so the
work items cover only what can be selected and the frame does not change.

**Coverage against the level.** `tests/visbuffer_masked_coverage.rs` rasterizes every DAG cut of a leaf-card canopy
on the CPU with the masked test: both DAG cuts keep 100 % of level 0's coverage (no fix or cap needed); the voxel
far levels keep 68.3 %, 75.3 % and 160.6 % (ADR-3915 (5)).

**Tests.** `visbuffer::tests::masked` (list order and determinism with masked sets, no masked cluster in the software
range, the level range byte for byte), `visbuffer::gpu::levels` (run bounds), `tests/visbuffer_voxel_far.rs`
(near canopies' work items equal the plain DAG's), `tests/visbuffer_masked_coverage.rs`; with the frame hookup,
`tests/visbuffer_masked.rs` (forward and visibility frames of a masked leaf-card scene within the strict floors,
switch-off byte for byte, no creates when warm).

## 8c. Cluster residency (ADR-3118)

**Problem.** The cluster arena (ADR-353) holds every resident mesh's whole DAG. VRAM grows with the loaded content.

**Contract.** With `ClusterResidency::enabled` (from `RenderProfile::cluster_residency` or the runtime override
`VisBufferSettings::residency`), the cluster data of meshes made resident from then on stays within
`budget_bytes`, and the residency layer uploads at most `upload_bytes_per_frame` per frame:

1. **Bands.** A mesh's clusters split into at most 16 bands by their parent error against a ladder of thresholds
   (the running maximum of each cooker level's largest `lod_error`). Step `k` keeps bands `k..n` resident: exactly the
   clusters whose parent error exceeds `T_k`, a set closed under "parent of". Band `n-1` is the base: always resident
   while the mesh is.
2. **Records.** All cluster records stay resident. Absent clusters have `lod_error = f32::MAX` and no vertices or
   triangles. Resident clusters with `lod_error ≤ T_k` have `lod_error = 0`: they are the leaves of the resident DAG.
   The cut the cull selects is the full DAG's cut clamped to the step's cut (no hole, no overlap); `vis_cull.wgsl` is
   unchanged. Level runs (§ 8b) are recomputed from the rewritten records.
3. **Geometry.** Each band's block holds the vertices it owns (the coarsest band using a vertex owns it), its clusters'
   vertex lists and triangles. Bands load coarse to fine, so every vertex a resident cluster names is resident.
4. **Demand.** Per frame and mesh, each instance inside the last main view's frustum asks for the finest band its cut
   can select (conservative per-band bounds, the § 8b test). No GPU readback.
5. **Scheduling.** One band per mesh in flight, the next finer one, largest projected size first. Over budget, the
   finest resident band of a mesh that does not need it is evicted, least recently needed first. A needed band and a
   base are never evicted. A band's records flip only after its whole block is written, so no frame draws a hole.
   A flip redraws the cached shadow maps and misses the screen reuse from the next frame (the geometry changed).
6. **Failure.** A base over the budget: the mesh is refused and draws forward. A mesh whose next band never fits:
   degraded, it draws its coarsest resident level. Both are counted.
7. **Off.** No mesh is managed; the arena behaves as in ADR-353, frames are bit-identical.

**Surface.** `VisBufferArenaStats::residency` (`VisBufferResidencyStats`), `render.get_visbuffer_residency`,
`render.set_visbuffer_residency` (`book/agents/visbuffer-residency.md`).

**Tests.** `visbuffer::tests::residency` (settled image equals the all-resident image byte for byte on a fixed camera:
`the_settled_frame_equals_the_all_resident_frame`) and `visbuffer::gpu::resident::tests` (byte budget, per-frame
upload bound and eviction safety over a fixed camera path: `budget_and_upload_bound_hold_over_a_camera_path`; no
buffer created by churn and nothing created by a warm frame: `churn_grows_the_arena_only_while_warming`; a mesh larger
than the budget stays at its base and is reported: `a_mesh_larger_than_the_budget_degrades_and_a_base_too_large_is_refused`;
two runs give the same counters: `residency_is_deterministic`).

**Next.** Bands streamed from the pak instead of held in RAM; shadow-job demand; UV2 for managed receivers.

## 8d. Foliage sway (ADR-3916)

**What takes the path.** A material `FoliageSway::adopt` made from a plain one: stage `FOLIAGE_SWAY_VERTEX`, surface
`FOLIAGE_SURFACE` (`visbuffer::sway::sway_gate`). The instance draws under a standard twin (`FoliageSway::plain`,
`material_key ^ SWAY_TWIN_TAG`), which the resolves and the masked raster read like any material. A sway on another surface
graph stays forward (`SwaySkip`, `render.get_visbuffer` `last_frame.sway.forward`), since the resolve has no graph surfaces.

**Data.** `regia::foliage_sway::SwayTable`, one uniform buffer: `frame` (main camera eye; pixels per unit over the LOD
threshold) and up to 256 records (graph slots 15, 14 and 0, with the renderer's clock and wind written over slot 0, as the
forward passes see it). One record per distinct sway. A record unclaimed this frame and not referenced by a persistent set
can be reused after its previous key is removed; current-frame plans reserve their records immediately, so material edits
cannot exhaust the table over the renderer's lifetime. `Instance.flags.y` from bit 8 = record + 1; `scale.y` = reach (the graph's
`max_vertex_offset`, metres); `scale.z` = the first voxel level's world error (0: none); `scale.w` = what its LOD sphere adds to the
distance from the instance's centre. Bindings: raster 5, software 8, shadow raster 6, velocity 7, resolve 14. Uniform, so
`RESOLVE_STORAGE_BUFFERS` is unchanged. It is one buffer, written in place, so a warm frame creates nothing.

**Motion.** `cluster_offset` runs `offset` on the rest world position, the unit model normal, vertex colour red and the instance's
origin, as `lab_forward.wgsl`'s vertex stage does. The resolve moves the three corners before its barycentrics and keeps the rest
normal, as the forward path does. The velocity pass moves each corner at the previous clock and model, with this frame's wind and
fade (`velocity.wgsl`'s `graph_offset`), adding the offsets to the rest point it computes without sway, so calm air stays bit-identical. The shadow raster moves the shadow model's rest point about its origin
(`shadow_scale`, `shadow_lift`), as the forward shadow pass does.

**Cull.** While some named record moves (not calm, a limit above zero, a height above zero), `CullParams.scan.w` and
`JobParams.caps.z` are 1 and the frustum, Hi-Z, software-size and shadow side-plane tests take the sphere grown by `scale.y`. The cone
test is skipped for those instances. The LOD test keeps the rest spheres. In calm air the lists and the frame are those of the same
material without sway, byte for byte.

**Voxel far levels.** Their uploaded records have `pad` = 1 and stay at rest. An instance's offset is multiplied by `fade(d, s)`, `d` its
distance from the main camera, `s = scale.z · frame.w + scale.w` the distance where the first voxel level can be selected: 1 up to
`SWAY_FADE_START` (0.7) · `s`, 0 from `SWAY_FADE_END` (0.95) · `s`, linear between. Forward has no fade and no voxel levels. A shadow job with a
coarser cut can select a voxel cluster nearer than `s`; it stays rigid beside swaying neighbours.

**Shadows.** Opaque: swaying clusters. Masked mesh-list instances: forward shadow draws through the graph (§ 8b).
Masked persistent sets: no cluster shadow until the shadow raster supports their alpha test (§ 8b). Swaying draws are
`animated` in the dynamic-caster class (spec 88 § 2.3); a swaying persistent set's box joins it while it moves, the path
is active and cluster shadows are enabled. Only instances whose flags say they cast contribute a box; masked and
shadow-off sets refresh no pages. The box follows the shadow scale about the object origin and the shadow lift, then
grows by the world-space sway bound. Its grown sphere can touch every redrawn page, so a frame with swaying instances
reserves shadow work items for every page job (4 × 64 lists, at most 2^20 items) and job records for as many jobs:
the first gust grows no buffer.

**Tests.** `visbuffer::tests::wind` (persistent sets, no hookup): zero wind byte for byte with the same lists; forward against the
path at fixed wind times (strict floor, a calm control); velocity against the CPU mirror's displacement per vertex; clusters with a
displaced triangle in the frustum against the lists at the sway limit, zero lost, with a reach-off control; a plant swaying out from
behind an occluder (Hi-Z), with a reach-off control; the cluster shadow against the forward caster; software against hardware neighbours, no
crack pixel; a warm frame creates nothing; the fade's endpoints against the lists. `visbuffer::sway`, `visbuffer::gpu::sway`: the gate, the
numbers, the table. With the hookup, `tests/visbuffer_wind.rs` (a masked tree; zero wind; the page class; skip reasons), the differential's
`swaying_masked_meshes_agree_with_the_classic_path_at_fixed_wind_times` and `tests/visbuffer_warm_creates.rs`. Without the hookup a set
keeps the clock its material carries.

## 8e. Band streaming from the cooked pak (ADR-3708)

**Problem.** ADR-3118 saves VRAM, but a managed mesh still holds its cooked cluster lists and triangles (and a packed
copy of its vertices) in RAM, and a shadow cut draws whatever bands the main view made resident.

**Contract.** With `ClusterResidency { enabled, ram_budget_bytes, io_bytes_per_frame, .. }` a residency-managed mesh
whose payload came from a pak keeps only its cluster records, its band plan and its packed vertices in RAM; band
payloads (cluster vertex lists and triangles) are read from the cooked payload on demand and evicted back to the
on-disk state under the RAM budget, least recently arrived first, never a payload mid-upload.

1. **Source.** `regia_framework::meshlet_stream::open_stream` parses the cooked `.fmlt` header, counts, bounds and
   cluster records through `AssetServer::payload_reader` (a zero-copy view of the mapped pak), and
   `MeshletMesh::from_head` keeps the bytes behind a `BandDisk` handle (`vertices`/`triangles` stay empty until
   `materialize()`). (The function lives in `regia_framework` because `regia_render` does not read paks; ADR-5823.) The handle rides the existing `insert_meshlets` API; no
   frame file changes. The payload format is unchanged — the band plan is the index (offsets and sizes derive from the
   cluster records' ranges).
2. **Reads.** One band per mesh in flight, the next finer one, ordered by projected screen error. A worker on the
   existing IO path (`PayloadReader` + `regia_core::tasks::spawn_io`, the `regia_world` cell-source pair) slices the
   band's cluster ranges, validates them, and posts the concatenated words; the frame accepts deliveries within
   `io_bytes_per_frame` (the read is charged when the request is issued, from the band's words). A failed read
   (`band_unreadable`, `band_corrupt`, `band_not_found`) leaves the band on disk, draws the coarse level, and is
   counted and reported — never a panic. The base band is read synchronously at admission (it must be resident before
   the first draw).
3. **Uploads** keep to `upload_bytes_per_frame`: a delivered payload gets its GPU range only when its upload starts;
   its records flip once the whole block is written (ADR-3118). RAM eviction of a delivered-but-not-uploading payload
   frees it back to the on-disk state; a GPU eviction (ADR-3118) leaves the payload on disk, so a reload is a disk read.
4. **Shadow demand.** `record_shadow_cull` records, per managed mesh, the finest band any GPU-masked caster inside a
   job's region can select under the job's texel cut — the § 8c bounds with the job's texels per unit and
   `shadow_lod_threshold_px`. It feeds the next frame's request pass at its own priority, so a shadow-only caster
   refines. The ask is conservative; the GPU cut clamps to resident records, so a shadow map never opens a hole.
5. **Fallbacks.** The base band is always resident while the mesh is; while a finer band loads, the frame draws the
   coarsest resident level and counts a fallback (`VisBufferResidencyStats::fallbacks`).

**Surface.** `render.get_visbuffer_residency` / `render.set_visbuffer_residency` gain `ram_budget_bytes`,
`io_bytes_per_frame` and the disk stats (`bands_on_disk/loading/in_ram`, `io_bytes`, `io_bytes_total`,
`read_queue_depth`, `ram_bytes`, `peak_ram_bytes`, `fallbacks`, `read_errors`, `last_read_error`).
`book/agents/visbuffer-residency.md` § "Disk streaming".

**Tests.** `visbuffer::gpu::resident::tests` (streaming request/delivery/upload flow on a memory disk: IO bound, RAM
budget LRU, fallback counting, corrupt/missing band error codes, shadow-only caster demand, settle-then-compare
records against an all-resident run) and the lavapipe streaming test (settled frames byte-identical to the
all-resident frame; warm frames create nothing).

## 8f. Voxel proxies of terrain scatter in HLOD (ADR-3253)

The world-streaming cook reuses the voxel far cooker for whole forests: each HLOD node's placed scatter instances are
voxelized with `voxel_far_cook::voxelize_with_coverage` into one `VoxelFarProxy` per node (spec 78 § 11.6). These
proxies are not bridged into a cluster DAG: the runtime draws a node's finest level as sub-voxel boxes on the ordinary
mesh path where the HLOD cut draws the node, so they need neither the visibility buffer nor the frame. Bridging them
into the visibility path (a per-view level pick, the opacity dropout, GI input through `mesh_sdf`) is a follow-up for a
visibility lane.

## 9. Platforms and performance budget

- **Linux, Windows (Vulkan, DX12):** runs where `IndirectStrategy` is `MultiDraw` or
  `MultiDrawCount` (`multi_draw_indirect` and `indirect_first_instance`). wgpu 30 needs only
  `INDIRECT_EXECUTION` for `multi_draw_indexed_indirect` and emulates it as a loop where the
  backend lacks the native call.
- **Android aarch64:** compiles. The Mobile presets leave it off. Where a device lacks
  `indirect_first_instance`, the storage-object prepass, and with it this cull, does not run
  (`skipped: no_gpu_objects`).
- **macOS (testing only):** the same code path, untested there.
- **Budget:** two compute dispatches of `ceil(n / 64)` groups, one extra pyramid build, and one
  multi-draw per batch per phase. Memory is 32 + 2 × 20 bytes per reserved instance, with the
  capacity rounded up to a power of two so growth reallocates rarely (16384 slots, about 1.2 MB, at
  10k instances), plus a 272-byte parameter uniform. Target at 1440p on the 5080: phase cull plus the extra pyramid under 0.1 ms.
  On a dense occluded view, the net prepass saving is positive. These numbers need a GPU run; lavapipe
  shows correctness only.
- **Software raster:** two storage buffers of 4 bytes per pixel (29 MB at 1440p), a third cluster
  range of 8 bytes per entry (8 MB at the default capacity), one cleared per frame, two indirect
  dispatches and one fullscreen merge per phase. Memory is allocated only once the switch is on. Per
  cluster each triangle transforms three vertices in each pass (no shared-vertex cache: ADR-026
  rules out the local array); a cheaper cache is a follow-up if a GPU profile asks for it.

## 10. Out of scope

- Two-phase occlusion for shadow views: built, see § 10a (ADR-2072).
- Double-sided clusters in the software rasterizer: built, see § 8a (ADR-2689).
- Software shadow clusters (§ 8a says why they stay in hardware).
- Visibility buffer as a preset default (§ 8).

## 10a. Two-phase occlusion of the shadow casters (ADR-2072)

The rest of F2. Off by default: `FrameRenderer::set_shadow_occlusion(bool)`, agent
`render.set_shadow_occlusion { on, min_casters? }`. Since ADR-2482 it is also a profile column,
`RenderProfile::shadow_occlusion` (off in every preset), with the override
`RenderSettings::shadow_occlusion` / `effective_shadow_occlusion()` that the runtime hands the renderer
every frame; `render.set_shadow_occlusion` writes that override too.

- **Graph node (ADR-5178).** Phase 1 is the frame graph's optional built-in node `shadow_occlusion`
  (reads `casters`, writes `shadow_occlusion`, which `shadows` reads), held only while the switch is
  on and recorded through the graph's dispatch in the `shadows` record site, before any shadow pass
  draws; with the switch off the graph does not hold it and phase 1 records and creates nothing. The
  depth prepass's half is `prepass_occlusion` (ADR-4905).

- **Unit: the shadow-map kind.** Each sun cascade (its own texture) and the local-light atlas has one
  coarse farthest-depth pyramid (`R32Float`, base texel = 8×8 map texels, edge rounded up to a power
  of two, full mip chain): about 11 MB for four 4096² cascades and an 8192² atlas, against 170 MB at
  full resolution. A sun page or an atlas tile is a rectangle in its kind's pyramid.
- **History** is the pyramid as the kind's last redraw left it and the matrix that redraw used (per
  tile rectangle for the atlas). Retained cascades draw nothing and are not culled; their pyramid
  stays valid for their next redraw.
- **Order.** Phase 1 of every culled kind dispatches before the shadow passes. Per kind: pass A (page
  clears, phase 1's survivors, visibility-buffer clusters, the shared terrain draw), the pyramid rebuilt
  from the map, phase 2's cull, pass B (load) with phase 2's survivors in each job's viewport and
  scissor. Commands are indexed by the instance's slot in the shadow object buffer, so each instanced
  batch is one multi-draw per phase.
- **Exactness.** The footprint is the projection of the eight corners of the bounding sphere's box,
  grown by one texel and clipped to the job's scissor; the test reads the level where it covers at most
  2×2 texels and culls only when the nearest corner depth is beyond the farthest pyramid depth by 1e-5.
  The shadow pipelines have no hardware depth bias, so the margin is for rounding only. Pass B's
  pyramid is never nearer than the unculled map (outside a cleared page the loaded depth is final), so
  the maps are the unculled maps texel for texel; `tests/shadow_occlusion.rs` checks every map and the
  frame with paging off and with page redraws, on a uniform field under a held view and on mixed casters (two meshes, double-sided, masked, shadow scale and lift) under a camera, sun and light set that move every frame.
- **Scope.** GPU-driven shadow path, main view, kinds with at least `min_casters` (512) caster
  instances in their jobs. Unbounded casters (vertex-stage graphs that declare no largest offset or draw
  their shadow at a shadow scale or lift, morphs, compute-skinned twins) draw in phase 1; a graph that
  declares one (a wind sway) is culled on its mesh bound grown by it (ADR-5389). Reports: `FrameRenderer::shadow_occlusion_stats` / `shadow_occlusion_counters`, agent
  `render.get_occlusion` `shadows` (with each phase's GPU counts when the last frame culled: a blocking readback, only then); skip reasons `off`, `no_gpu_objects`, `no_jobs`, `few_casters`, `failed` (the cull could not make its resources; the casters drew directly).
