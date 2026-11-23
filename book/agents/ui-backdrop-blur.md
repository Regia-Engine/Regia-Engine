# UI backdrop blur (agents)

UI parity T11 / master-plan F7, ADR-1836. A node with `BackdropBlur { radius }` shows frosted glass: the frame
already drawn under its rect (the 3D scene and any earlier UI) is blurred with a Gaussian, through the node's
rounded rect, before the node's own background paints on top. Give the node a translucent `Background` for the
glass tint.

## In code

```rust,ignore
commands.spawn((
    UiNode,
    BackdropBlur { radius: 10.0 },          // Gaussian sigma in UI px
    Background { kind: BackgroundKind::Color(UiColor([0.05, 0.07, 0.1, 0.45])) },
    Border { radius: [12.0; 4], ..default() },
));
```

- `radius` is the Gaussian sigma in UI px (scaled by the UI scale), clamped to 0..=64 physical px; 0 or less draws
  nothing.
- The blur follows the node's corner radius (the `Border` radius or the theme radius) and the node's `Opacity`.
- Below the node in paint order is what gets blurred: earlier siblings, the parent's background and the scene.
  Children and later siblings draw sharp over it.
- Lower-level: a `UiQuad` with `texture: TextureRef::Backdrop` and `blur` = sigma does the same; follow it with a
  tint quad.

In a `.ui.ron` file (format v5) the same flag is the node field `backdrop_blur: 12.0`; absent means off.

## In the editor

The viewport's overlay chips (the stats block and the view label) can sit on frosted glass too:
`editor.set_viewport { overlay_blur: 8 }` (Gaussian sigma in points, 0 to 64; 0 is off, the default). The pane's finished
picture is blurred under each chip before egui samples it, on the GPU path with these same quads and on the software
path with `regia_ui::cpu_backdrop_blur`. `editor.viewport_stats` reports `overlay_blur: { chips, shaded_pixels }`. A tool
overlay with its own chip calls `regia_editor::viewport_blur::blur_behind_chips`.

## How it draws

The overlay pass splits at each backdrop quad: the frame region under it (plus the blur reach) is copied into a
scratch texture, blurred by a horizontal pass that also downsamples (factor 1, 2 or 4 for sigma up to 4, 16, 64)
and a vertical pass, and the result is composited through the quad's rounded rect in the next overlay pass. Factor 4
has a box prefilter pass first, so a thin line behind the panel does not flicker as it moves (at factor 2 the bilinear
taps already cover the block under them). All passes are scissored to the quad, so cost follows the panel size, not the
screen. Work is in the target's own space (linear for sRGB targets), so a black and white edge blurs to the linear mid
value. `regia_ui::cpu_backdrop_blur` is the ideal result on the CPU; `tests/backdrop_oracle.rs` compares the GPU passes
with it.

Limits: the target must allow `COPY_SRC` (every window surface the engine configures, offscreen targets and
screenshots do; a surface that cannot be read back skips the blur). At most 192 blur passes per frame (two per quad,
three at factor 4); later quads draw their tint only, and `ui.backdrop_stats` counts them as `skipped`. HDR and display-calibrated frames that go through the intermediate target blur in that
target's space.

## Cost

Measured with `cargo bench -p regia_ui --bench backdrop_blur` (one tinted panel on a 1920x1080 target, the same
draw with and without the blur, median of 12 frames, each frame waited for). `cost_ms` is the blurred draw minus the
plain one. The run below is on the software adapter (llvmpipe, 2026-11-02, the build server), so the milliseconds are CPU
rasterization and only show how cost scales; `shaded_px` and `samples` (also in `ui.backdrop_stats`) do not depend on
the adapter and are the numbers to budget with.

| panel | sigma | factor | passes | shaded_px | samples | cost_ms (llvmpipe) |
|---|---|---|---|---|---|---|
| 320x180 | 4 | 1 | 2 | 124,936 | 3,123,400 | 5.0 |
| 320x180 | 16 | 2 | 2 | 37,584 | 1,841,616 | 8.3 |
| 640x360 | 8 | 2 | 2 | 124,936 | 3,123,400 | 5.2 |
| 640x360 | 32 | 4 | 3 | 66,984 | 1,959,216 | 7.1 |
| 1280x720 | 4 | 1 | 2 | 1,881,976 | 47,049,400 | 28.0 |
| 1280x720 | 16 | 2 | 2 | 495,624 | 24,285,576 | 19.1 |
| 1280x720 | 64 | 4 | 3 | 258,404 | 14,569,208 | 18.7 |
| 1920x1080 | 4 | 1 | 2 | 4,147,200 | 103,680,000 | 54.1 |
| 1920x1080 | 16 | 2 | 2 | 1,036,800 | 50,803,200 | 30.4 |
| 1920x1080 | 32 | 4 | 3 | 388,800 | 13,219,200 | 13.6 |

What it shows: cost follows the panel's area and the sigma inside one downsample factor, and drops at each step up in
factor (a full-screen sigma 32 panel costs a quarter of a full-screen sigma 4 panel). Keep blurred panels small, or use
a sigma of 8 or more so the work runs at half or quarter resolution. Numbers on a real card come from the same bench
with `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1`; they are not in this page yet.

## Agent surface

- `ui.set_backdrop_blur { node | name | text | ..., radius }` (permission `edit_scene`) sets or, with `radius: 0`,
  removes the effect on the one node the filters name. Reply: `{ node, radius, previous, changed }`.

| Error `reason` | Code | When |
|---|---|---|
| `ui_radius_required` | `F0002` | `radius` is missing or not a finite number |
| `ui_radius_out_of_range` | `F0002` | `radius` is outside 0..=64 |
| `ui_node_required` | `F0002` | no node filter was given |
| `ui_node_not_found` | `F0003` | no node matches |
| `ui_node_ambiguous` | `F0002` | more than one node matches |

- `ui.backdrop_stats {}` (read) reports what the last window draw did: `{ nodes, quads, blurred, skipped, passes,
  shaded_pixels, samples, copied_pixels, live_targets, live_target_bytes, targets_created, targets_freed }`. The
  per-draw fields are zeros after a draw with no backdrop quads, including a draw where the UI is empty.
  `skipped` above zero means a tint-only panel: no readable target, or the pass limit. The last four are the blur's
  render-target pool right now (ADR-5420), not the last draw: see below.

## Render targets

The blur keeps its scratch textures between frames: one full-size copy of the frame, plus a pair at reduced size for
each downsample factor (1, 2 or 4) a draw has used at the current target size and format. `live_targets` counts them
(at most 7), `live_target_bytes` is their memory, and `targets_created` and `targets_freed` count since the renderer
was made (`live_targets` is always `targets_created - targets_freed`). Closing a frosted panel keeps the pool
for 120 consecutive window draws without a successful backdrop blur (ADR-7869), so a quick reopen creates
nothing. At the 120th idle draw the pool releases all targets and their bind groups. Empty UI lists and
skipped blurs count as idle; a successful blur resets the count. A resize or a format change drops every
texture of the old size on the next blur, including pairs of
factors the new frame does not use. In Rust the same fields are on `UiRenderer::backdrop_stats()`
(`regia_ui::BackdropStats`).

For memory pressure, call `UiRenderer::trim_backdrop()` to release the targets immediately, even
when drawing is paused. Calling it again is harmless. It preserves the warmed pipelines and
uniform storage. `UiRenderer::BACKDROP_IDLE_FRAMES` documents the 120-draw idle limit. The next
blur allocates its frame copy and required downsample pair once, then returns to the warm path.
These counters measure references held by the pool; GPU work already submitted can retain its
resources until the device finishes it. There is no new agent method or error code.

After closing a panel, advance 120 window draws and call `ui.backdrop_stats {}`: expect
`live_targets: 0`, `live_target_bytes: 0`, and `targets_created == targets_freed`. Reopen once and
expect three live targets for one downsample factor; a second draw must not increase
`targets_created`. `tests/backdrop_targets.rs` checks this through the UI extraction/draw and
agent readout paths, as well as empty/skipped draws and restarting the idle count.

The baseline is the count after the first open and close: frame copy plus one pair per factor in use, so 3 for one
panel. If the live count rises after later short opens and closes at a fixed size, that is a leak. `tests/backdrop_targets.rs`
opens and closes a panel 50 times, at UI scale 1.0 and 2.25. After every close the count is back at the baseline,
and after the warm cycle the draws create no texture, pipeline or bind group.

To check a running game (the same 50 cycles on a device), toggle the blur on a panel that is on screen and read the
pool after each clear:

```text
ui.set_backdrop_blur { name: "controls.panel", radius: 12 }   # then let two frames draw
ui.backdrop_stats {}                                           # blurred: 1
ui.set_backdrop_blur { name: "controls.panel", radius: 0 }    # then let two frames draw
ui.backdrop_stats {}                                           # live_targets and targets_created as after cycle 1
```

Verify with `screenshot` before and after, or `ui.query` plus a screenshot crop.
