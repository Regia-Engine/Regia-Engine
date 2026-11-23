# Performance overlay (agents)

The in-game performance overlay (`regia_framework::perf_overlay`, book page
[Performance overlay](../user-guide/performance-overlay.md)) is a top-left panel. It shows FPS,
a 5 s frame-time graph, renderer CPU against GPU time, GPU passes, texture memory against its
budget, stutters, creates after warm-up, the preset, the GI mode and the resolution. Guide
topic: `agent.guide {"topic": "perf_overlay"}`.

## Methods

- `perf_overlay.get` returns `{ mode, source, host_default, setting, shown, plugin, stats,
  top_passes }`.
  - `mode` is `Off`, `Compact` or `Detailed`.
  - `source` says where the mode comes from: `override` (this protocol), `player`
    (`GameSettings.performance_overlay`, saved) or `host_default`
    (`PerformanceOverlay::default_mode`).
  - `stats` is a fresh snapshot of every value the overlay shows, read from the same engine
    sources the overlay uses: `fps`, `frame_ms`, `frame_avg_ms`, `window_frames`, `p50_ms`,
    `p99_ms`, `longest_ms`, `stutters_window`, `stutters_total`, `cpu_ms`, `gpu_ms`,
    `gpu_timing`, `gpu_passes` (`[name, ms]` pairs), `gpu_scene_ms`, `warmed_up`,
    `pipelines_after_warmup`, `bind_groups_after_warmup`, `texture_bytes`,
    `texture_budget_bytes`, `texture_streaming`, `vram_allocated_bytes`,
    `vram_reserved_bytes`, `rss_bytes`, `preset`, `gi_look`, `gi_mode`, `output`, `internal`,
    `render_scale`, `upscaler` and `overlay_cpu_us`.
  - The window statistics (`p50_ms`, `stutters_window`, …) cover the last ~5 s while the
    overlay is visible, and are 0 while it is hidden.
  - `vram_*` is filled only on the Detailed page, refreshed every 2 s off the frame thread
    (v0.1.14): null until the first report finishes, a refresh or two after Detailed opens.
  - `cpu_ms` is `FrameRenderer::render_into` only, not the frame; compare it with `frame_ms`.
    `telemetry.live` has the frame's bound class and the GPU waits.
- `perf_overlay.toggle {mode?, persist?, placement?}` changes the overlay.
  - `placement` (`{corner, offset: [x, y]}`) moves the overlay to a screen corner; see
    [Overlay placement, default font and pipeline loading](ui-overlay-font-loading.md).
  - With no `mode`, it cycles Off → Compact → Detailed → Off.
  - `mode` is `off`, `compact`, `detailed` or `default`. `default` drops the override.
  - By default the change is a runtime override and is not saved. With `"persist": true` it is
    written as the player's setting.
  - Returns `{ before, after, note }`. Mutating; permission `play`.

```json
{"method": "perf_overlay.toggle", "params": {"mode": "detailed"}}
{"method": "perf_overlay.get"}
```

## Checking a frame

1. `perf_overlay.toggle {"mode": "detailed"}`, then `play.step 30` (or wait 5 s for a full
   window).
2. `seeing.screenshot`. The panel is in the top-left corner and the rest of the frame is
   unchanged.
3. `perf_overlay.get`: compare `stats.gpu_passes` with `play.hitch_report`'s `gpu_passes`, and
   `stats.texture_bytes` with `assets.texture_memory`. They come from the same sources.
4. `perf_overlay.toggle {"mode": "default"}` puts back the player's or host's mode.

Showing the overlay creates no pipelines and no bind groups. If `pipelines_after_warmup` is
above 0 after a toggle, the game created them, not the overlay.
