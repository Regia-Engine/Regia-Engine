# Rendering Debug window (agents)

View ▸ Rendering Debug (ADR-7770) opens a floating window over the dock for the active viewport pane. It has no
method of its own: every control is an existing agent route, so an agent does what a user does in the window
with the calls below.

| In the window | Agent route |
|---|---|
| View mode (the keyed modes and the buffer views) | `editor.set_viewport { pane, view_mode }`; `editor.capture_viewport { debug_view }` draws the same picture |
| Light-complexity legend (shown in the light complexity mode) | the heat ramp of `debug_view: "light_complexity"`: lights per cluster / 16, blue (none) through green to red (16 or more) |
| Selection overrides: Unlit, X-ray, Ghost, Tint, Clear overrides | `editor.set_render_override { entity, unlit, xray, alpha, tint, clear }` per selected entity |
| Measure GPU passes (Stats) | `editor.set_viewport { stats: true }` |
| Pass timings, CPU time, internal size | `editor.viewport_stats`, and `editor.capture_viewport`'s `info` (`gpu_ms`, `cpu_ms`, `render_size`, `screen_percentage`) |
| Opening and closing the window | the command `view.toggle_rendering_debug` (`editor.command { run: "view.toggle_rendering_debug" }`) |

Ghost draws the selection at 35 % coverage; X-ray draws it through what is in front. The overrides are editor
state on the entity (`RenderOverride`), not saved with the level and not undo steps.

Every control is a focusable widget: Tab reaches the view-mode choices, the override switches and the Stats
switch, and Space or Enter operates them, with no pointer.

```
editor.set_viewport { "view_mode": "light_complexity" }
editor.capture_viewport { "width": 640, "height": 360 }
→ { "png_base64": "…", "info": { "view_mode": "light_complexity", "render_size": [640, 360], … } }
editor.set_render_override { "entity": 4294967301, "xray": true, "alpha": 0.35 }
```

Check it worked: `editor.capture_viewport {}` differs from the Lit capture of the same scene, and
`editor.viewport_badges {}` lists `view_mode` as changed.

## Error codes

The window adds none; see the codes of `editor.set_viewport` (invalid params for an unknown `view_mode`) and
`editor.set_render_override` (invalid params for an unknown entity) in
[Viewport tools](viewport-tools.md) and [Editor surface](editor.md).
