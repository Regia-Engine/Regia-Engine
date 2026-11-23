# Frame list growth (`summary.frame_scratch`, v0.1.28)

The renderer builds its per-frame lists (lights, shadow casters, shadow jobs, the GPU cull's
instances, the storage-object upload) in buffers it keeps between frames (ADR-385). A list that
needs more room than it held reallocates once; that is a *growth*. Once a scene and its camera
path settle, every list fits and the growth count stops rising: the steady frame allocates none
of those lists.

Read it from the frame summary (`telemetry.get_last_summary`, see [Telemetry](telemetry.md)):

```json
{ "jsonrpc": "2.0", "id": 1, "method": "telemetry.get_last_summary" }
```

```json
{ "summary": { "…": 0,
               "frame_scratch": { "growths": 0, "growths_after_warmup": 0, "total": 61 } } }
```

- `growths`: growths during the summary window (main view and texture views together).
- `growths_after_warmup`: the part of `growths` after the warm-up frames.
- `total`: growths since the renderer was created.

Typical uses:

- After a scripted run (`play.wait_until`) on a settled level, assert
  `summary.frame_scratch.growths_after_warmup == 0`. A non-zero value on a held camera means a
  list outgrew its buffer every few frames: something the frame builds keeps getting larger.
- A level load, a camera cut onto a denser view or a larger crowd grows the lists once; the next
  window goes back to 0.

What is counted, and what is not:

| Counted (one reusable buffer each) | Not counted |
|---|---|
| uploaded local lights, the light and shadow-tile records, GI probe records, the draw list | a buffer created for a new mesh or material |
| the joint palette, per-mesh skin spans, glass index, changed object uniforms | wgpu's own command recording |
| opaque, blended, shadow and translucent draw indices; cascade splits | caster lists of pages rebuilt this frame (they are kept and shared while the page holds) |
| caster bounds and cascade masks, caster footprints per cascade, page and atlas casters | the emissive light collection, the post stack |
| shadow jobs, GPU-cull instances and slots, storage-object bytes and their draw map | |
| the GPU culler's sort order, cull spheres, batch commands, and the instanced shadow and depth lists | |
| the velocity pass's sort keys, instances, skinned palettes and batched draws | |
| the stream pass's material order and the in-scene material map | |

Hosts and tests read the same numbers from Rust with `FrameRenderer::frame_scratch_stats()`
(`FrameScratchStats { growths, last_frame_growths, frames }`). The renderer's own tests
(`crates/regia_render/tests/frame_scratch.rs`) draw a scene with a held and an orbiting camera
and assert that no steady frame grows a list.
