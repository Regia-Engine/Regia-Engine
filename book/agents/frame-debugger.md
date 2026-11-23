# Frame Debugger

Open **View → Frame Debugger**, or run `editor.command` with `run` set to
`view.toggle_frame_debugger`. **Capture frame** records the main viewport's active
pane on its next render and holds that picture until **Resume** or another capture.
Closing the inspector keeps the captured picture; Resume releases it. Captures are
transient view state: no asset writes, format changes, or undo entries.

Tab reaches Capture frame, Resume and the pass list. Up/Down moves through passes;
Enter/Space selects a pass. The panel uses the editor's appearance scale and theme.

A pass that records its draws (the depth prepass and the forward pass) lists them below its
counters. Tab reaches **Previous draw**, **Next draw** and the draw list; Up/Down moves
through draws and Enter/Space selects one. Selecting a draw, from the list or with the step
buttons, shows its record and selects its entity in the Outliner.

## Methods

`editor.frame_capture` (MCP `frame_capture`) requires `action`:

- `start`: request the next active main viewport pane frame. Repeating start while
  pending keeps the request. The headless host needs a subsequent viewport render,
  for example `editor.capture_viewport`, before the request completes. Optional
  `passes` (an array of built-in pass names from `render.graph` steps, such as
  `"depth_prepass"` or `"forward"`) limits the pass-boundary copies to the record sites
  that recorded one of them; omitted or empty copies after every pass. Optional
  `budget_bytes` (a nonnegative integer, default 268435456 = 256 MiB) caps the staging
  bytes of those copies. Optional `source` is `viewport` (the default) or `play_window`:
  the separate play window of an in-process play session (Play in a separate window). Its
  next render is captured and the play window keeps showing that frame until Resume; the
  play world keeps running. A play-window capture's draws name the play world's objects, so
  no entity of the edited level is resolved or selected for them. With no play window open
  the start is refused with `capture_no_play_window`. The panel shows **Capture play window**
  while one is open.
- `state`: return `state` (`idle`, `pending`, `frozen`, `failed`), `open`, `frame`,
  `passes`, `recorded`, `selected`, `completed`, `copies`, `staging_bytes`,
  `pass_copies`, `skipped`, `request` (`passes`, `budget_bytes`), `draws`,
  `draw_selected`, `source`, `error` and `limitations`. `pane` is the requested pane while pending.
  `draws` lists the captured frame's draw records in recording order (see
  [Draws](#draws)); each pass row's `draws` holds the indices of its own.
- `resume`: cancel a pending request or release the snapshot and resume live rendering.

```json
{"method":"editor.frame_capture","params":{"action":"start"}}
```

```json
{"method":"editor.frame_capture","params":{"action":"start","passes":["depth_prepass","forward"],"budget_bytes":8388608}}
```

```json
{"method":"editor.capture_viewport","params":{"width":320,"height":240}}
```

```json
{"method":"editor.frame_capture","params":{"action":"state"}}
```

`editor.frame_pass` (MCP `frame_pass`) requires `index`, a nonnegative integer into
`passes`. It selects that row and returns its metadata plus `targets` and `skipped`
(the pass's declared outputs that have no image, each with `resource` and `reason`). Each target
has `checkpoint`, `after_pass`, `resource`, `version`, `frame`, `width`, `height`,
`format`, `encoding`, `png_base64` and `unavailable`. Decode `png_base64` as a PNG;
it is null when the target could not be copied. No file is written.

```json
{"method":"editor.frame_pass","params":{"index":0}}
```

`editor.frame_draw` (MCP `frame_draw`) selects one draw record of the capture. Give
`index` (a nonnegative integer into the state's `draws`) or `step` (`next` or `previous`,
moving from the selected draw and holding at the ends; with none selected, `next` takes the
first and `previous` the last). The draw's pass becomes the selected pass row. With `select`
true (the default) the draw's entity becomes the Outliner selection, as selecting the draw in
the panel does; `select: false` only reads the record. It returns the draw's record plus
`selected_entity` and `pass_targets` (the pass's copied output target indices).

```json
{"method":"editor.frame_draw","params":{"index":0}}
```

```json
{"method":"editor.frame_draw","params":{"step":"next"}}
```

```json
{"method":"editor.frame_capture","params":{"action":"resume"}}
```

The three methods require read permission and leave scene content unchanged. Pass image
encoding is a heavy protocol operation; state reads do not encode any image.
The host installs their schemas and dispatch handlers through `AgentExtension`.
`agent.schema` includes their parameter shapes, examples and error contracts.

## What the capture measures

The rows follow `FrameRenderer::graph_report().graph.order`, the same public report
as `render.graph`. Temporary capture observers are removed from that list and from
the renderer before the request completes. Rows retain versioned `inputs`, `outputs`,
`declared_pipelines`, `recorded`, `gpu_ms`, `draw_count`, `pipeline_switches`, and
`bind_group_switches`. Pipeline keys describe permitted pipelines, not the actual
binding of each draw. Null counts mean the renderer does not count that pass;
its existing counters cover mesh draws, not every terrain, sky or extension draw.
Forward counts combine the opaque and translucent counters. GPU timing is from the
captured frame after the offscreen readback; unsupported or untimed rows are null.

Checkpoint copies happen at the public after-opaque, after-transparent, before-post,
after-post and overlay slots (`checkpoint` starts with `frame_capture_`). They are
associated only with the last recorded pass before that checkpoint, never with an
earlier resource writer.

Pass-boundary copies (ADR-7860) happen inside the renderer, on the captured frame only,
right after each built-in record site: for every frame target the site's passes declare
as a write (`color`, `depth` or `output`), the image at that moment is copied into a
capture-owned buffer. The target's `checkpoint` and `after_pass` are the site's last pass
that wrote that resource, and `version` is that pass's declared write (`depth#1`). A site
records several passes at once (the forward pass with its transparency composite, SSGI
with SSR), so the image is the state after the site's last writer; earlier passes of the
same site have no separate image. Declared writes that are not frame targets
(`shadow_atlas`, `clusters`, `ao`, `velocity`, ...) are listed in `skipped` with
`capture_internal`. A depth target can be written again inside the forward pass even
though the graph uses the same resource name. A selected row with neither kind of copy
returns an empty `targets` array and `targets_unavailable` of `pass_outputs_skipped` (it
has `skipped` entries) or `no_checkpoint_for_pass_output`.

The copies are read-only: they never change pass order or any pass result, and the
live targets are never handed out. A frame with a full capture is byte-identical to the
same frame without one, and so is the frame after it.

Display targets preserve their RGBA8 bytes (BGRA is reordered). Float color previews
use `linear_clamp`: undo pre-exposure, apply the checkpoint's exposure when supplied,
clamp to 0–1 and encode with power 1/2.2. They do not reproduce the full post stack.
Depth previews use `depth_ramp`: stored depth 0 is black and 1 is white, without
linearizing distance. These conversions affect only the preview, never the viewport.

The forward renderer has no albedo G-buffer. Its `base_color` debug mode is available
through `editor.capture_viewport { debug_view: "base_color" }`. A capture of that
mode contains its actual output; there is no second debug render inside capture.

## Draws

A capture also records its frame's draws one by one (ADR-8224), in the draw loops of the
depth prepass (`counted: "depth_prepass"`) and the forward pass (`opaque` and `translucent`).
Each record has `index`, `pass` (the graph row's name), `counted`, `pass_index`,
`draw_in_pass`, `draws_in_pass`, `pipeline` (the name the draw loop gives the pipeline it
bound: `standard`, `standard_double_sided`, `material_graph`, `mask_decals`, `lightmap`,
`thin_film`, `glass_multiply`, `transmit_add`, `blend`, ...; `*_instanced` for a GPU-driven
batch), `mesh` and `material` (the renderer's cache keys), `objects` (the object keys drawn:
one for a direct draw, every member of a GPU-driven batch), `entities` and `entity` (the
objects that name an entity of the edited level, which extracts each mesh entity with its
entity id as the object key), `entity_name`, `vertex_count`, `index_count`,
`instance_count` (for an indirect draw, the CPU count before GPU culling), `indirect`,
`target` (`hdr_color`, `msaa_color`, `output` or `depth`), and `partial_image` (always null)
with `partial_image_unavailable`.

A pass row's `draw_details` is `recorded` for those passes and `not_recorded` for the rest,
whose draws are counted only; `partial_images` is `draw_replay_unsupported` for a recording
pass (`no_draw_records` otherwise). No pass replays its draws one at a time, so stepping
through draws shows each draw's record, not the image up to that draw: the pass's copied
output is the image after all its draws. The instanced depth prepass knows its batches' mesh
and material but not their objects (`objects` is empty there). Editor additions (the
backdrop sky, proxy cubes) are draws whose objects name no entity.

Records are made only on the main-view frame a capture copies. Every other frame opens the
renderer's draw log off: one flag test per draw loop, no record and no allocation
(`regia_render::draw_log_counters` proves it).

## Limits and errors

Draw records cover the depth prepass and forward draw loops; shadow, visibility-buffer,
subsurface, transparency (OIT) accumulation, terrain, sky and extension draws are counted
or not, but not recorded one by one. Pass-internal
resources are not copied (`limitations` lists `pass_internal_resources_not_copied`
for a capture with pass copies). Split-screen capture is refused
as a failed capture; independent viewport instances are not capture sources, and a play
window is one only for an in-process play session (an out-of-process launch renders in
another process). The scene renderer is frozen; editor overlays remain
interactive. A GPU-device replacement releases the frozen frame; capture again on the new device.

Each capture permits at most 256 MiB of checkpoint staging buffers, and the pass copies
at most `budget_bytes`. When the pass copies reach the budget, the capture keeps what
fits and lists every later output in `skipped` with `capture_budget`. With capture off
there are no observer passes, target copies, allocations, barriers or readback polls
from this feature: the renderer's per-site hook is one empty check.
The snapshot's `copies` and `staging_bytes` expose the capture cost. Renderer hosts
must collect a session immediately after its submitted frame: collecting before any
main view or after a later frame returns an error and removes the observers.

| Code | Meaning |
|---|---|
| `frame_capture_action` | Missing or invalid action (use start, state or resume), or a `source` other than viewport or play_window. |
| `capture_no_play_window` | `source: "play_window"` with no play window open; nothing was requested. |
| `capture_unknown_pass` | `passes` is not an array of built-in pass names; nothing was requested. |
| `frame_capture_budget` | `budget_bytes` is not a nonnegative integer; nothing was requested. |
| `frame_capture_missing` | No completed capture; render the pending viewport first. |
| `frame_pass_index` | Missing, fractional, negative or out-of-range pass index. |
| `frame_draw_index` | `editor.frame_draw` got no valid `index` or `step` (or both), an out-of-range index, a capture with no draws, or a non-boolean `select`. |
| `frame_image_encode` | The captured target could not be encoded as PNG. |
| `capture_multisampled` | Target is multisampled; no single-sample copy is exposed. |
| `capture_format` | Target format has no preview conversion. |
| `capture_not_copyable` | Target was not created with COPY_SRC. |
| `capture_budget` | Copy would exceed the capture staging limit. |
| `capture_internal` | The declared write is a pass-internal resource, not a frame target. |
| `pass_outputs_skipped` | The pass's outputs are all in `skipped`. |
| `no_checkpoint_for_pass_output` | No copy or public checkpoint observes this pass. |

The last seven codes are target availability reasons, not failed RPCs. A render or
map failure ends the capture in `failed`, with its diagnostic in `error`; observers
are still removed. No stale frame is reported as a successful capture.

## Verification

`regia_editor --lib frame_debugger` covers keyboard activation at three UI scales (the
pass list, and the draw list with Next draw), closed-panel work counts, pending/resume
state, draws listed under their pass with their entities, draw selection and stepping
selecting the entity, and error contracts. `regia_render --lib frame_stats` captures three
meshes and checks three opaque draw records with their object, mesh and material keys, and
that unarmed or armed-then-cancelled frames push no record and match a never-armed
renderer's counted device calls; `regia_editor --test frame_debugger`'s
`frame_debugger_three_meshes_list_three_opaque_draws_and_select_their_entities` does the
same through the dispatcher with three mesh entities.
`regia_editor --test frame_debugger` drives the real dispatcher and renderer: graph
order equals `render.graph`, checkpoint PNGs match the frozen viewport, another pane
cannot overwrite the kept GPU picture, and Resume returns to live rendering.
`regia_render --test frame_capture_checkpoints` compares against control renders and
counts the off path; `--test frame_capture_pass_hooks` checks that a full pass capture
leaves the frame byte-identical, that six frames with no request run no hook, copy or
staging allocation and match a never-requested renderer's counted GPU work, the budget
and the pass filter; `--test pass_timestamps_graph` checks the graph's timing ownership.
No golden image is created or re-blessed.
