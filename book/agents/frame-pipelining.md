# Frame pipelining

Where the frame thread's time goes, and what moves some of it off that thread (ADR-2074,
`specs/22` T2.12).

## Reading it

`render.frame_pipelining` takes no parameters and returns:

| Field | Meaning |
|---|---|
| `profile` | The active profile's switch, or `null` in an app without `RenderSettings`. |
| `applied.parallel_shadow_plan` | The switch the last frame with sun pages applied. `REGIA_FRAME_PIPELINING=1` turns it on whatever the profile says. |
| `applied.on_tasks` | The last frame's page update really ran on tasks: the switch was on, with at least 256 casters and more than one cascade. Read this field to know what ran. |
| `encode_threads` | Workers that finish command buffers (`FrameRenderer::set_encode_threads`). |
| `render_thread.in_frame_loop` | Always `false`: the engine's frame loop renders on the thread that simulates. |
| `render_thread.note` | Why, and what a host that drives `FrameRenderer` itself can use. |
| `last_frame_ms` | The last frame's CPU time: `total`, `encode`, `submit`, `readback`, `shadow_plan`. |

An app without `RuntimeRenderer` gets the protocol's not-found error.

## Switching it

`render.set_frame_pipelining {"parallel_shadow_plan": true}` sets the active profile's switch from
the next frame and returns `before`, `after` and `note`. A value that is not a boolean is an
invalid-params error. `render.set_profile` replaces the profile, which turns the switch off.

`parallel_shadow_plan` projects the casters into each sun cascade and updates each cascade of the
sun page cache on its own compute-pool task, instead of all of them on the frame thread. It is off in every preset. The frames are byte-identical
either way: the update decides the same pages, and only the thread differs. Below 256 casters the
update stays on the frame thread, because spawning the tasks would cost more than they save.

A game sets it with `RenderProfile::frame_pipelining`, a host that drives the renderer itself with
`FrameQuality::frame_pipelining`.

## Measuring it

`crates/regia_render/tests/frame_pipelining_s0.rs` is the measurement behind these choices. It
prints where the frame thread's time goes from the renderer's own spans, the render targets by size,
and the frame interval with and without a render thread. The file's header has the commands. Its
numbers belong to the machine and scene they ran on. `docs/perf-log.md` holds the recorded rounds.

```text
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo test -p regia_render --release --test frame_pipelining_s0 -- --ignored --nocapture
```

Add `REGIA_FRAME_PIPELINING=1` to measure with the switch on.

`frame_pipelining_s0_window` in the same file runs the same scene in a presenting window, inline and
on a render thread, and prints per frame the simulation thread's interval, the wait for
`RedrawRequested`, the acquire, record and present times, and the latency from the start of the
simulation to the frame's present (ADR-2798). The offscreen bench cannot see what a swapchain or a
compositor adds; this one can. On a Linux host without a display, run it under `xvfb-run -a`:

```text
xvfb-run -a -s "-screen 0 1920x1080x24" env VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo test -p regia_render --release --test frame_pipelining_s0 -- --ignored --nocapture frame_pipelining_s0_window
```

`REGIA_S0_PRESENT` (`fifo`, `mailbox`, `immediate`), `REGIA_S0_LATENCY` (the swapchain's frame
latency), `REGIA_S0_LOOP=poll` (frames from `about_to_wait`, not gated on `RedrawRequested`) and
`REGIA_S0_ENCODE` (encode threads) vary the setup. Lavapipe numbers say little about the render
thread, because its rasterizer runs inside `queue.submit`. Use a card on a real display for numbers
that decide anything.

## A render thread for a host

`regia_render::with_render_thread` runs `FrameRenderer` on a thread named `regia-render` for as long
as a closure runs, for a host that drives the renderer itself. The closure hands frames over with
`submit`, which blocks while an earlier frame waits (a channel of capacity one), and reaches the
renderer with `with_renderer`, which runs on the render thread after every frame handed over so far.
`post` queues a call the same way but does not wait for it. A host that presents to a swapchain posts
its whole frame this way (acquire, `render_into`, present), so the acquire's wait for the display falls
on the render thread. The frames are byte-identical to inline frames.

`RenderThreadSettings::queued_frames` is how many frames the channel holds while the render thread
works on another. The default 1 lets the host run furthest ahead, but a frame then reaches the display
about two frames later than inline when the render thread is the slower side. 0 makes the hand-over a
rendezvous: the next frame's simulation still overlaps this frame's recording, and the added latency is
at most one frame (measured in a window, `docs/perf-log.md`, ADR-2798).

| Error code | Meaning |
|---|---|
| `render_thread_stopped` | The render thread panicked. The panic is raised again when the scope ends. |
| `render_thread_spawn_failed` | The OS refused the thread. The closure did not run. |

`RenderPlugin` does not use it: engine systems take the renderer during the simulation, which is when
this thread would hold it. ADR-2074 lists what the frame loop needs first.

## Transient aliasing

With `TransientPool::set_aliasing` on, `RenderGraph::execute` releases a transient after its last
reader, and the next transient of the same `TextureDesc` reuses its texture. `RenderGraph::alias_plan`
shows which transient gets which texture and the bytes saved, and the pool's `aliased` counter (in the
`transients` block of `render.graph`) counts the reuses.

Contents are undefined at a transient's first writer then, so that writer clears it or overwrites all
of it. The graph cannot see a writer that loads what is there or returns `Ok` without writing, and its
reader would see the earlier resource's pixels. A writer that needs last frame's contents declares its
own resource as an input. A transient read before any earlier node wrote it, or kept after the run,
keeps a texture of its own. When a node fails, its readers find no texture for what it was the first
to write, instead of another resource's pixels.

The frame itself does not run through `RenderGraph::execute` yet, so this applies to graphs a host
runs. In the frame, `aliased` stays 0.
