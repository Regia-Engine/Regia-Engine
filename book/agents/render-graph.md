# Frame graph and host passes

The frame renderer records its passes in a fixed order (`specs/13` § 5): shadows, light clusters,
probes, the depth prepass, culling, ambient occlusion, the forward pass, the screen-space and
outdoor passes, temporal upscaling, translucent layers, the post stack, overlays and gizmos. Between
them sit five named slots (`specs/13` § 4). A game or tool crate adds its own pass at a slot with
`FrameRenderer::register_pass`; the frame records it into its own command encoder when it reaches
that slot, so the frame still submits once (ADR-1552).

A frame with no registered pass does no extra work and is byte-identical to a frame from before the
extension point existed (`passes_that_record_nothing_leave_the_frame_byte_identical`).

## Slots

| Slot | Where | Colour target |
|---|---|---|
| `after_opaque` | after the forward pass, before the screen-space and outdoor passes | the lit opaque HDR image (linear, times the pre-exposure), on frames with a linear HDR target |
| `after_transparent` | after the HDR translucent layers | the HDR image the post stack will read (temporally upscaled when TAAU or TSR ran) |
| `before_post` | just before the post stack, on frames that run it | the same HDR image, as the post stack sees it; when a pass writes this frame (`FramePass::writes_color`), a per-frame copy of it, which the post stack then reads (the image itself may be the temporal history or the held-screen cache, ADR-1863) |
| `after_post` | after the post stack and the display-referred translucent layers | the output image (its own format) |
| `overlay` | last, after scalar overlays and gizmos | the output image |

Reflection-probe captures run no registered pass. Render-to-texture views do; the pass sees the
view's asset id in `FramePassContext::texture_view` (`None` for the main view).

## Add a pass (Rust)

```rust,wrap
use regia_render::{FramePass, FramePassContext, GraphSlot};
# use regia_render::FrameRenderer;
# let mut renderer = FrameRenderer::new();

struct Fade;

impl FramePass for Fade {
    fn name(&self) -> &str { "fade" }            // lowercase ascii, digits and `_`
    fn slot(&self) -> GraphSlot { GraphSlot::Overlay }
    fn writes(&self) -> &[&'static str] { &["output"] }
    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        // Record render or compute passes into ctx.encoder over ctx.color, ctx.depth.
        // Create GPU objects through ctx.device (they count in the hitch gate); never submit.
        Ok(())
    }
}

renderer.register_pass(Box::new(Fade))?;
```

The context holds the tracked device, the frame encoder, the slot's colour view with its format,
size and colour space, the viewport rectangle, the scene depth (and whether it is multisampled),
the unjittered view-projection and projection, the eye, the pre-exposure, the frame index and the
texture view id. `FramePassContext::write_buffer` stages a buffer write that lands before the
frame's submit. `crates/regia_render/examples/custom_pass.rs` is a complete example: it adds an
overlay pass that darkens the image's border and writes the frame to a PNG.

A pass that returns `Err` is skipped for that frame only: the frame goes on, the error is logged
and counted. `FrameRenderer::remove_pass(name)` takes a pass out again.

Registration errors (`PassRegisterError::code`):

| Code | Meaning |
|---|---|
| `pass_bad_name` | The name is empty or has characters other than lowercase ascii letters, digits and `_`. |
| `pass_duplicate_name` | A built-in pass or a registered pass already has the name. |

## Read the graph

`render.graph` (no parameters; read-only):

```json
{}
→ { "steps": [
      { "kind": "pass", "name": "shadow_occlusion", "module": "shadow_two_phase", "reads": ["casters"], "writes": ["shadow_occlusion"], "gpu_ms": null },
      { "kind": "pass", "name": "shadows", "module": "shadow_passes", "reads": ["casters", "shadow_occlusion"], "writes": ["shadow_atlas"], "gpu_ms": 0.41 },
      …
      { "kind": "pass", "name": "post_stack", "module": "post_stack", "reads": ["color"], "writes": ["output"], "gpu_ms": 0.22 },
      { "kind": "pass", "name": "translucent_layers", "module": "display_passes", "reads": ["output", "depth"], "writes": ["output"], "gpu_ms": null },
      { "kind": "slot", "name": "after_post", "wired": true, "color": "display", "passes": ["outline"] },
      …
    ],
    "extensions": [
      { "name": "outline", "slot": "after_post", "wired": true, "reads": ["output", "depth"], "writes": [],
        "runs": 120, "failures": 0, "last_cpu_ms": 0.02, "last_frame": 121, "last_error": null } ],
    "gpu_timing": true,
    "gpu_scene_ms": 3.9,
    "forward_permutations": { "count": 6, "budget": 256, "over_budget": false, "undeclared": [],
                              "axes": ["ALPHA_TEST", "GLASS", …] } }
```

- `steps` is the frame's recording order. A pass row is listed whether or not this frame's
  settings ran it. `reads` and `writes` name frame resources for orientation; the frame does not
  reorder by them. `module` is the `regia_render` source module that records the pass
  (`src/frame/<module>.rs`); several passes can share one (`outdoor_passes` records the
  atmosphere, clouds and fog). The rows' `reads` and `writes` are checked by a unit test: they
  compile as a `RenderGraph` in recording order with every pass live, so a row does not claim a
  write nothing reads or miss a read.
- `gpu_ms` is the pass's GPU time in the last timed frame; `null` while GPU timing is off
  (`FrameRenderer::set_gpu_timing`) or for a pass without its own timestamp pair.
- `extensions` lists registered passes in registration order. `runs` counts frames the pass
  recorded in, `failures` frames it returned an error in (`last_error` keeps the message).

- `pipelines` on a pass row are the keys of the pipelines play can switch that pass onto after a
  warm frame (`tsr`, `taau_resolve`, `ssgi_half_res`, `depth_match`, `depth_match_msaa`,
  `outdoor`, `subsurface` and `oit` (nothing until a frame has drawn such a material under a profile
  that asks), `sun_shadow_trace` and `forward_sun_rt`, which ask for nothing unless the profile
  traces the sun, see [Hardware ray tracing](hwrt.md), `forward_studio`, which asks for nothing
  unless the renderer has a studio environment, see [Forward studio axis](forward-studio-axis.md), `visbuffer_path`, which asks for nothing
  unless the profile selects the visibility path by density, see [Visibility buffer](visbuffer.md), and `radiance_cache`, which asks for nothing
  while the cache is off, see [Radiance cache](radiance-cache.md), and `restir_di`, which asks for nothing
  unless the profile turns on many-light sampling, see [Many-light sampling](many-light-sampling.md)). The game-frame warm reads them from the compiled graph and builds each once
  (ADR-2018), so a pass that needs a pipeline before play declares it here rather than in a
  hand-kept warm list.
- `graph` is the frame's compiled render graph (below).

- `forward_permutations` counts the distinct define sets the forward shader (`lab_forward.wgsl` and
  its `regia::forward::*` modules) was composed with in this process, against a provisional budget.
  `axes` is the closed list of defines the shader tests (`FORWARD_AXES`); `undeclared` names any define
  outside it and should stay empty.

## The compiled graph

The frame builds one versioned `RenderGraph` from the built-in passes and the passes registered at
the slots (ADR-2018). Every write makes a new version of its resource: the visibility-buffer
resolve writes `color#1`, the subsurface scattering nodes make `color#2` and `color#3`, the forward
pass reads that and writes `color#4`, the transparency composite makes `color#5` (it records in the
middle of the forward pass, listed after it), a pass registered at `after_opaque` that writes `color`
makes `color#6`, and the screen-space passes read that. A read
binds to the latest version written before it, so read-modify-write chains over one target need
no renaming, and the order is the recording order. The graph changes only when a pass is
registered or removed, so a steady frame builds and allocates nothing for it.

```json
"graph": {
  "nodes": [
    { "name": "forward", "registered": false, "slot": null, "reads": ["color#3", "depth#1", …],
      "writes": ["color#4"], "pipelines": [], "live": true },
    { "name": "outline", "registered": true, "slot": "after_post", "reads": ["output#2", "depth#1"],
      "writes": [], "pipelines": [], "live": true },
    …
  ],
  "order": ["shadows", "cluster_lights", …, "outline", "scalar_overlays", "gizmos"],
  "error": null,
  "warm_keys": ["blue_noise", "sun_shadow_trace", "restir_di", "visbuffer_path", "subsurface", "forward_sun_rt", "forward_studio", "oit", "ssgi_half_res", "radiance_cache", "outdoor", "tsr", "taau_resolve", "depth_match", "depth_match_msaa"],
  "transients": { "textures": 4, "bytes": 33177600, "in_use": 4, "created": 4, "reused": 1196, "evicted": 0, "aliased": 0 },
  "keyed": { "textures": 2, "bytes": 33177600, "in_use": 1, "created": 2, "reused": 1197, "evicted": 0 },
  "transient_writes": ["outdoor_hdr_scratch", "translucent_hdr_scratch"],
  "pooled": [
    { "label": "frame hdr", "key": 0, "width": 1920, "height": 1080, "format": "Rgba16Float",
      "bytes": 16588800, "generation": 41, "used_this_frame": true },
    …
  ]
}
```

- `reads` and `writes` are resource versions: `name#n` is the `n`-th write of `name` in the frame;
  `#0` is what the frame starts with (the scene's meshes, lights and casters).
- `live` is `false` for a node whose writes nothing downstream of it reads on the way to `output`.
  A registered pass that is not live still records (its writes may be for the host); the flag says
  its declared writes reach nothing. A pass that writes nothing is always live.
- `order` lists the live nodes in execution order.
- `error` is set when the graph does not compile (`code` below); the frame then still records in its
  fixed order.
- `transients` is the main view's transient texture pool. The frame's linear HDR target, the
  opaque-frame copy refracting glass samples, the glass front layer drawn with it (depth and
  transmittance), the two SSAO targets, the `before_post` copy (on frames where a `before_post`
  pass writes) and the output-sized depth copy the translucent layers use on a scaled or MSAA frame
  come from it, keyed by size,
  format, layers, samples and usage. The same requests get the same textures every frame, so a
  steady frame creates none (`created` stays put while `reused` climbs); a size or format left for
  four frames the pool was used in is dropped (`evicted`). `aliased` counts acquires served by a
  texture released earlier in the same pool frame, which needs a graph a host runs with aliasing on
  (see [Frame pipelining](frame-pipelining.md)); in the frame it stays 0. Render-to-texture views have pools of
  their own.
- The two HDR scratch images (`translucent hdr scratch`, `outdoor hdr scratch`: copies of a cached
  composite that the translucent layers and the outdoor composite draw into on a frame whose image a
  later frame reuses) come from the view's keyed pool, a second pool of the same kind, each under its
  own slot key (`key` 1 and 2), so two images of one size and format never share a texture. `keyed`
  has the same counters as `transients` for that pool (`transients` keeps counting the frame
  transients only). The pool keeps the latest of each slot alive on frames that do not draw it,
  because post builds its bind groups for them ahead of the first held frame; an older one (another
  size or format) is dropped after four pool frames like any other entry, and that drop renews the
  bind caches so post stops naming it.
- `transient_writes` lists the resources built-in nodes declare as graph transients rather than as
  imported (`outdoor_hdr_scratch`, written by `atmosphere`, `clouds` and `fog`;
  `translucent_hdr_scratch`, written by `translucent_hdr`; `post_stack` reads both). They appear in
  the nodes' `reads` and `writes` like any resource.
- `pooled` lists every texture of the main view's two pools, the frame pool's first (`key` 0), each in
  creation order: its label, slot `key`, size, format, bytes, `generation` (new for every texture a
  pool creates) and whether this pool frame used it. The sum of `bytes` is `transients.bytes` plus
  `keyed.bytes`.

### How the frame records the built-in passes

The frame records its built-in passes through the compiled graph (ADR-4905). Each frame enters every
built-in node at its record site. A node the graph does not hold is not recorded and creates
nothing. A node the graph holds but lists with `live: false` (a host pass overwrote its output without
reading it) still records, as before. A node whose per-frame switch is off (no post stack on this frame, no depth prepass) is
listed as inactive. A second entry in one frame is refused, so each node records at most once. The
dispatch counts any node that was skipped, repeated or recorded after a node that depends on it:

```json
"dispatch": {
  "frames": 240, "last_frame": 239,
  "recorded": ["shadows", "cluster_lights", "gi_probes", "depth_prepass", "gpu_cull", "ssao", …, "gizmos"],
  "inactive": [], "skipped": [], "late": [],
  "violations": 0, "last_violation": null
}
```

- `recorded` lists the built-in passes of the last main-view frame in recording order, in the
  graph's node order. Several passes that one record function records (the outdoor passes, the
  screen-space passes) are entered together, in order.
- `inactive` lists held nodes that the frame's switches left out. Only some switches reach the
  dispatch today (`shadow_occlusion`, `depth_prepass`, `gpu_cull`, `prepass_occlusion`, `many_light_sampling`,
  `oit_accum`, `oit_composite`, `translucent_hdr`, `post_stack`, `translucent_layers`). The other nodes are listed as recorded even on a frame where they record no
  commands.
- `skipped` and `late` should stay empty, and `violations` (all frames and views, including texture
  views) should stay 0. `last_violation` is `{ code, node, frame }`:

| Code | Meaning |
|---|---|
| `builtin_skipped` | The graph holds the node, but the frame never recorded it. |
| `builtin_repeated` | The frame reached the node a second time; the second recording was refused. |
| `builtin_out_of_order` | The node recorded after a node ranked after it in the graph. |

The graph also decides the recording order (ADR-4945). It folds its nodes and slots, in node order,
into record sites, one per record function (the outdoor passes are one site, each slot is one), and
the frame records every pass and every slot in one loop over those sites. So `recorded` is the graph's
order by construction, not by the order of the renderer's statements. If a site ever ran before the
site whose result it needs, the frame would fail with `frame_site_missing_input` (an encode error that
names the missing result) instead of recording a wrong image. A graph whose nodes of one site are not
next to each other is refused when the graph is built: the frame then records no built-in pass and
`skipped` lists them all.

Two-phase occlusion is an optional node: `prepass_occlusion` (phase 1 of the depth prepass's
two-phase cull, see [Occlusion](occlusion.md)) is in `graph.nodes` and `graph.order` only while
`FrameRenderer::set_two_phase_occlusion` is on, and comes right before `depth_prepass`. The `steps`
row is always listed. An agent check after switching it:

```json
{ "method": "render.graph" }
→ graph.order contains "prepass_occlusion" iff two-phase occlusion is on;
  dispatch.recorded contains it on such a frame; dispatch.violations == 0
```

The shadow casters' half is the same kind of optional node (ADR-5178): `shadow_occlusion` (phase 1 of
the shadow casters' two-phase cull) is in `graph.nodes` and `graph.order` only while
`FrameRenderer::set_shadow_occlusion` (agent `render.set_shadow_occlusion`) is on, and comes right
before `shadows`, which reads what it writes. Its `steps` row (module `shadow_two_phase`) is always
listed, first. Its dispatches record inside the shadows' record function, before any shadow pass
draws. While the node is held, `dispatch.recorded` lists it only on a frame where phase 1 can cull:
a main view with shadow jobs whose shadow lists are GPU-driven (ADR-5626). Otherwise
`dispatch.inactive` lists it (`render.get_occlusion` `shadows.skipped` says why: `no_jobs`,
`no_gpu_objects`). A recorded frame can still skip the cull for too few casters (`few_casters`).
With it off, the frame records and creates nothing for it:

```json
{ "method": "render.set_shadow_occlusion", "params": { "on": true } }
{ "method": "seeing.screenshot", "params": { "target": "offscreen" } }
{ "method": "render.graph" }
→ graph.order starts ["shadow_occlusion", "shadows", ...];
  on a GPU-driven frame with shadow jobs, dispatch.recorded starts ["shadow_occlusion", "shadows", ...],
  otherwise dispatch.inactive contains "shadow_occlusion"; dispatch.violations == 0
```

Neither node needs a change to the renderer's frame code to be added or removed: the graph holds it
or not, and its record site asks the dispatch. `crates/regia_render/examples/custom_pass.rs` switches
both on next to its host pass and exits 1 if the dispatch counted a violation.

Graph compile codes (`GraphCompileError::code`):

| Code | Meaning |
|---|---|
| `graph_duplicate_output` | Two nodes of an unversioned graph (`RenderGraph::new`) write one resource. |
| `graph_cycle` | Live nodes of an unversioned graph depend on each other in a cycle. |
| `graph_desc_mismatch` | Writers of one transient texture of a versioned graph declare different descriptions. |

### Build your own graph (Rust)

`RenderGraph::versioned()` gives a tool or a game the same model for its own passes: each node lists
what it reads and writes, a `GraphResource::Texture(TextureDesc)` output is a transient the graph
takes from a `TransientPool` when its first writer runs, and `GraphResource::Imported` names a
target the caller owns. `RenderGraph::execute` records the live nodes in order; each node's
`record` gets a `GraphContext` with the encoder, the tracked device, the frame index and
`texture(name)` for the views bound to this run. `RenderGraph::pipeline_keys` lists the live nodes'
pipeline keys for a prewarm. A `record` that returns `Err` is skipped for that run and listed in
`GraphRun::failed`.

Errors: `F0003` when the app has no `RuntimeRenderer` (a host driving `FrameRenderer` itself calls
`FrameRenderer::graph_report`).

## Persistent buffer ownership (ADR-8065)

`render.graph` appends `graph.buffers` and each graph node's `buffer_reads` and
`buffer_writes`. Existing fields, including the original `reads`/`writes`, keep their
meaning and order. Buffer dependencies participate in the same compiled graph.

Each inventory row has `name`, `owner`, `memory` (`cpu` or `gpu`), `bytes`, `generation`,
`imported`, `history`, `writers` and `readers`. Bytes are actual allocation sizes, zero
before allocation. The CPU draw-list row measures retained vector capacity; it does not
count GPU objects referenced by the draws. Sum CPU and GPU bytes separately. This inventory
covers the audited buffers, not every allocation in the renderer. It is queried on demand.

- `lights_buf`: per-view upload, consumed by clustering and shading.
- `counts_buf` and `indices_buf`: shared cluster output, rewritten for each view.
- `cpu_draw_list`: frame preparation's CPU list; `gpu_cull` updates its GPU-object fields.
- `di_reservoir_current`, `di_reservoir_history`, `di_reservoir_temporal`: the many-light
  sampler's two parity slots and its temporal scratch buffer.
- `gi_reservoir_current`, `gi_reservoir_history`, `gi_reservoir_temporal`: the GI sampler's
  two parity slots and temporal scratch, recorded at the end of `sun_shadow_trace`'s site.

Current and history are logical roles of distinct parity allocations, not extra copies.
History is ignored after a reset; allocation generation changes only on replacement.
Readers/writers describe possible accesses, including disabled features and intra-node
compute stages. `frame_upload`, `frame_prepare` and `previous_frame:<node>` are external
producers, not GPU nodes. Intra-node scratch reads are listed in the inventory; node
`buffer_reads` lists dependencies crossing node boundaries. Optional paths can have zero
bytes while still declaring their dependencies. ReSTIR GI needs ray-query support.

`graph_read_before_write` means a declared buffer has neither an earlier writer nor an
explicitly initialized owner import. It appears in `graph.error` with the buffer, reader
and owner in the message. Check the producing node's registration order. The public
`RenderGraph::declare_buffer(name, owner, initialized)` declares this contract; it leaves
allocation/binding with the owner. Unknown legacy resources retain their implicit-import
behavior. Declared buffers are outside transient texture aliasing.
