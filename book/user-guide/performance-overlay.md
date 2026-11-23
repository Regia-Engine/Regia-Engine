# Performance overlay

Every Regia Engine game has a built-in performance overlay. It is a small panel in the top-left
corner that lets players and alpha testers see how the game runs and report it. It works in any
game that adds `regia::DefaultPlugins` (with the default `ui` feature). There is nothing else
to set up, and it costs nothing while it is hidden.

## What it shows

**Compact**

| Line | Meaning | Source |
|---|---|---|
| `144 FPS   6.9 ms   DesktopHigh` | Average FPS, median frame time of the last 5 s, and the render profile preset (`Custom` when the game changed the profile) | `Diagnostics`, the overlay's 5 s window, `RenderSettings::preset` |
| Graph | Frame time over the last ~5 s, newest on the right. Thin lines mark 16.7 ms (60 fps) and 33.3 ms (30 fps). Bars are green up to 16.7 ms, yellow up to 33.3 ms, orange-red above that, and **red** when a frame took more than twice the median (a stutter). The top of the graph is 50 ms. | `RealTime` delta, the same interval `play.hitch_report` records |
| Render CPU / GPU | The renderer's CPU time for the frame and the GPU frame time. The CPU number covers `FrameRenderer::render_into` only (cull, record, submit): not the game's simulation, extract, the swapchain acquire or the present, so it can be far below the frame time on a CPU-bound game. `GPU-bound` or `CPU-bound` appears when one of them is at least 90 % of the frame. `n/a` means the device has no timestamp queries. | `FrameRenderer::cpu_timings().total_ms`, `GpuTimings::frame_ms` |
| GPU top | The three most expensive GPU passes | `GpuTimings::passes` |
| Textures | Resident material textures against the texture budget. The line turns yellow above 90 % and red above 100 %. | `FrameRenderer::texture_memory_summary` (the same report as `assets.texture_memory`) |
| Stutters | Frames over 2 × median in the last 5 s, and in total since warm-up while the overlay was visible | `HitchReport` over the window |
| Creates | Pipelines and bind groups created after warm-up (the first 300 frames, and 60 frames after each level warm). Anything above 0 is a hitch source, so report it. | frame-path counters |
| Lighting | The lighting option (`Depth` or `Balanced`) and the frame's GI mode | `RenderSettings::effective_gi_look`, `FrameQuality::gi` |
| Resolution | Output size, internal render size, render scale and upscaler (`native`, `TAAU`, `TSR`, `FSR 3`, `SGSR`, `bilinear`) of the last view drawn | `FrameRenderer::last_extent` |

**Detailed** has everything in Compact, plus p50/p99/longest frame time, GPU memory (the
device memory the allocator reserves, and its sub-allocated part; Vulkan and D3D12), the process RSS, the overlay's own CPU time, and the full per-pass
GPU table: shadows, clusters, GI probes, prepass, cull, SSAO, contact shadows, forward, SSGI,
SSR, atmosphere, clouds, fog, TAAU, post, scene and frame.

The GPU memory and RSS lines are refreshed every 2 s by a short-lived worker thread
(`regia-perf-memory`, v0.1.14). The report walks the allocator's block list, which took
milliseconds on the frame thread before; now the frame never waits for it, and the lines show the
new numbers at the next refresh after the report finishes (up to 0.25 s later).

GPU timestamps are switched on while the overlay is visible and switched off again when it is
hidden, unless something else had already switched them on.

## Turning it on

The overlay is a **setting**, `GameSettings::performance_overlay`, saved with the other settings
in `user://settings.ron`:

- `None` (the default) follows the host's default.
- `Some(Off)`, `Some(Compact)` or `Some(Detailed)` is the player's choice. The player's choice
  always wins over the host's default.

**Add the option to your settings menu.** You can drop in the ready-made row, which is a label
and a dropdown that writes the player's choice:

```rust
use regia_app::prelude::*;
use regia_framework::perf_overlay::spawn_settings_entry;

fn build_graphics_page(world: &mut World, page: Entity) {
    spawn_settings_entry(world, Some(page)); // "Performance overlay  [Off | Compact | Detailed]"
}
```

To build your own control instead, use the descriptor `settings_menu_entry()`. It gives the
settings path, the label, the options and the help text. Write the choice with
`set_player_mode(world, mode)`, or set `GameSettings::performance_overlay = Some(mode)` yourself.

**Host default.** The engine's default is `Off`. A game can ship with the overlay on (DO27's
alpha uses `Compact`):

```rust
use regia_app::prelude::*;
use regia_framework::perf_overlay::{PerformanceOverlay, PerformanceOverlayMode};

fn configure(app: &mut App) {
    app.insert_resource(PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact));
}
```

This only sets the default. A player who picks `Off` in the menu keeps `Off`.

**An optional key.** The overlay has no default key binding, because games use the F-keys for
their own controls. To add one, declare the `PERF_OVERLAY_ACTION` action
(`"PerformanceOverlay"`) in your `InputConfig` and bind it. Each press then cycles Off →
Compact → Detailed and saves the result as the player's choice. Players can rebind it like any
other action.

```rust
use regia_input::prelude::*;

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("PerformanceOverlay", ActionKind::Button)
        .binding(Binding::new("PerformanceOverlay", InputSource::key("Backquote")))
}
```

**Agents** call `perf_overlay.toggle` and `perf_overlay.get`. See
[Performance overlay (agents)](../agents/perf-overlay.md).

## Cost

| | CPU | GPU |
|---|---|---|
| Hidden | a few comparisons per frame | none (it draws nothing) |
| Visible | ≤ 0.1 ms per frame (release build) | ≤ 0.1 ms (one instanced UI draw of a few hundred quads) |

The graph redraws at 20 Hz and the text at 4 Hz. Changing the graph redraws only the overlay's
own UI subtree and never runs layout again. The window's boot warm builds the runtime UI's pipeline on a warm worker (job "overlay layers").
It does this for the window format and both display-stage formats, and the UI also compiles it on
the first frame it draws, even with nothing to draw. Showing the overlay later therefore creates
no pipelines and no bind groups. The test `crates/regia_framework/tests/perf_overlay.rs` checks
both bounds, the zero-create toggle, and that the panel is drawn in the corner and nowhere else.

## Hosts with their own renderer

A host that drives its own `FrameRenderer` instead of the runtime's `RuntimeRenderer` (such as the
lookdev chase) feeds the overlay after each frame:

```rust
use regia_app::prelude::*;
use regia_framework::perf_overlay::{PerfOverlayRendererSample, RendererSample};

fn after_frame(world: &mut World, renderer: &regia_render::FrameRenderer) {
    world.insert_resource(PerfOverlayRendererSample(RendererSample::of(renderer)));
}
```

`cargo run --release -p lookdev --features overlay --bin perf_overlay_shot` draws the overlay over
DO27's chase this way.

## Reporting performance (testers)

1. Turn on **Settings → Performance overlay → Detailed**.
2. Play the scene that feels slow until the graph shows it.
3. Take a screenshot that includes the overlay. Note the map, what was on screen, and the
   **Stutters** and **Creates** lines. A non-zero **Creates** count is always a bug.
