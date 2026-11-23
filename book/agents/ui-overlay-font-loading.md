# Overlay placement, default font and pipeline loading

These are runtime presentation APIs (P-16, ADR-8051). They never change simulation state.

## Position the performance overlay

`perf_overlay.get {}` returns `placement: {corner, offset: [x, y]}` alongside mode and telemetry.
Set it with:

```json
{"method":"perf_overlay.toggle","params":{"placement":{"corner":"bottom_right","offset":[24,32]}}}
```

Corners are `top_left`, `top_right`, `bottom_left`, `bottom_right`. Offsets are finite,
nonnegative UI pixels measured inward from the corner and follow the overlay's DPI scale.
Placement alone preserves mode. Add `mode: "compact"` to show it. Placement is runtime-only;
`persist: true` still saves only the mode. Invalid placement rejects the whole call before
changing mode or placement (`F0002`, existing invalid-params code).

Rust hosts insert `regia_framework::perf_overlay::PerformanceOverlayPlacement`; the plugin
preserves a resource inserted before it. Existing hosts keep the top-left 10 px default.

## Override the default font

Call `ui.set_default_font {"family":"<loaded family>"}` with `edit_scene` permission.
The result is `{family, changed}`. `family: null` restores embedded Inter. Load project bytes
through `TextSystem::load_font` before selecting their internal family name, or select a bundled
family. `TextSystem::set_default_family` is the same Rust API. Explicit span families, script
fallback and emoji remain active. The override selects the default sans-serif face; it does
not replace script-specific fallback policy.

`ui.text_shape {"text":"Hello 你好"}` reports `default_family`, the actual `fonts` used,
`missing_glyphs` and dimensions. Nodes reshape and relayout on the next frame. Use
`ui.text_shape {"entity":id}` after stepping a frame to inspect the displayed result.
Repeated selection is a no-op. Bad names/types (including unloaded fonts) return `bad_family`
without mutation. A host without `TextSystem` returns `ui_not_installed`.

## Inspect pipeline loading

A host installs `regia_framework::loading_widget::PipelineLoadingPlugin` alongside `UiPlugin`
and its renderer, then calls `spawn_pipeline_loading_widget(world, parent)` under its own UI root.
For a standalone menu notice use `spawn_shader_compile_notice(world, parent)` instead;
its progress bar and immediate status are hidden until an error occurs. The caller controls
placement and styling. A custom renderer loop may call
`apply_pipeline_warm_progress(world, &snapshot, now_ms)` instead; `now_ms` is monotonic.

`ui.inspect {"node":panel_id}` returns `pipeline_loading`, including:

- `state`: `done`, `total`, `deferred_pending`, `fraction`, `busy`, `label`, `error`;
- `notice`: `visible` and `line`;
- `notice_settings`, `hide_when_ready`, `show_progress`, and child ids `label`, `bar`, `notice_node`.

Read `render.pipeline_warm {}` for the source queue; `ui.inspect` reads the last applied panel
snapshot, so it matches what the UI received. Progress is queued **jobs**, not pipeline objects.
The bar is the queued-job fraction. Deferred work is a separate count and keeps the panel visible
even at 100%. An empty queue is complete. Errors remain visible after completion. New work may
lower the fraction. No total or remaining-time estimate is invented.

The delayed notice appears strictly after continuous pending work exceeds its configured delay
(5,000 ms normally, 1,500 ms when the cache reports missing/discarded data). Completion hides it
immediately; subsequent work starts a new delay. A clock regression restarts the wait. It never
captures input or blocks a menu. With no queue the plugin leaves host-fed state alone.

Deterministic checks: `loading_widget_tracks_warm_progress`,
`shader_notice_delays_resets_and_uses_cache_status`,
`loading_widget_updates_real_entities_and_retains_errors`,
`overlay_placement_tracks_four_corners_and_live_resize`, and
`overlay_font_loading_controls_and_inspection_through_dispatch`.
