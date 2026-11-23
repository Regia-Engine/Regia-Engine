# Material clock and wind

Wind on foliage, scrolling water, a pulsing light panel: a material graph animates its surface by
reading the `Time` node. The value it reads is the **material clock**, which the runtime renderer
writes into every graph that reads `Time` (`params[0].x`, with the previous frame's value in
`params[0].y` for motion vectors). `regia_matgraph::drive_material_clock` sets it each
`PostUpdate` from gameplay [`Time`](../user-guide/gameplay.md) (ADR-1238).

## Read

`render.material_clock` (no parameters):

```json
{}
→ { "set": true, "now": 12.5, "prev": 12.483333, "delta": 0.016667,
    "period_secs": 3600, "gameplay_secs": 3612.5 }
```

- `now` is the clock this frame, `prev` the clock one frame earlier, `delta` their difference
  (the frame's gameplay delta). `null` until the clock system has run once.
- `period_secs` is the lap length: the clock is gameplay time modulo 3600 s.
- `gameplay_secs` is the unwrapped gameplay time the clock follows (`null` without a `Time`
  resource).

Without a `RuntimeRenderer` the call fails with `F0003` (not found) and a hint. A host that drives
`FrameRenderer` itself sets the clock with `FrameRenderer::set_material_clock(now, prev)`, usually
from `regia_render::material_clock(elapsed, delta)`, and reads it with
`FrameRenderer::material_clock`.

## What the clock guarantees

- **Deterministic.** The clock is a function of gameplay time and the frame's delta, computed in
  integer nanoseconds. Two runs that step the same gameplay time (fixed-step captures, replays,
  `loop.set` stepping) write the same clock and draw the same frames.
- **Smooth in long sessions.** Plain `f32` seconds lose resolution as a session grows: after
  9 hours neighboring values are 3.9 ms apart, after 30 days 0.25 s, so wind would move in
  visible steps and its motion vectors would come out wrong. The wrapped clock never passes 3600,
  so it keeps sub-millisecond resolution whatever the session length.
- **Follows gameplay time.** A paused game holds the wind (`delta` 0, no motion blur or TAA
  smear on still leaves); `Time::time_scale` slows it with everything else.
- **The wrap.** Once an hour the clock jumps back to 0. `prev` is measured on the new lap
  (slightly negative), so motion vectors stay right. A sway whose period divides 3600 s loops
  across the wrap without a seam: 1, 2, 3, 4, 5, 6, 8, 10, 12, 15 s and so on, and 0.5, 0.75,
  1.5, 2.5 s. With `sin(time * k)`, the period is 2π / k: pick `k` so it is one of those (for a
  2 s sway, `k` = π).

## Authoring wind

A per-instance sway uses the instance's origin as the phase, so neighbors do not move in step:

```text
"t": Time,
"op": ObjectPosition,
"opx": Swizzle(input: "op", channels: "x"),
"phase": Add("t", "opx"),
"s": Sin("phase"),
"sway": Mul("gust", "s"),
...
output: (world_position_offset: Some("sway")),
shading: (max_vertex_offset: 0.5),
```

Set `shading.max_vertex_offset` to the largest offset the graph can produce. The renderer grows
every culling bound by it: the frustum test on the CPU, the GPU cull's spheres, and the shadow
casters' boxes. An offset larger than the bound can carry a surface out of its bound and get it
culled at a screen edge.

Swaying instances batch like any other mesh: the GPU-driven path draws a field of them in one
instanced draw per mesh and material, with the same pixels as drawing them one by one
(`crates/regia_matgraph/tests/wind_sway.rs`).

## Occlusion culling of instances

On the GPU-driven path, instances that the depth prepass hides are dropped from the color pass by
a Hi-Z test (`gpu_cull.wgsl`). The test is conservative: it bounds each sphere on screen from the
view-projection, reads every pyramid texel under that bound (up to 3×3), and draws anything that
reaches pixels the pyramid's coarser levels do not cover (the last row or column of an odd-sized
level). A visible instance is never dropped, so there are no holes of clear color at a narrow
field of view (a scope), at the bottom or right edge of the screen, or behind a one-pixel gap.

## Check it

- Step the game a few frames (`loop.set`) and call `render.material_clock` after each:
  `prev` of each frame equals `now` of the frame before, and `delta` equals the step.
- Pause gameplay: `now` holds and `delta` is 0.
- Take two screenshots at the same gameplay time in two runs: they match.
