# Floated panels and OS windows (agents)

ADR-7821. Code: `regia_editor::os_windows`. Parity rows P-001 (float as an OS window), P-012 (graph editors in their own
windows), P-015 (single-window mode). Panel instances (P-003) are not built yet: every panel has one instance (`instance`
is always 0) until the layout file gets its v3 row.

A floated panel group is its own OS window when the preference `ui.floating_windows` is `os` (the default on the Linux
and Windows desktop) and the editor runs with a window system. With `in_window` (single-window mode), or in a headless
editor, a floated group is a window inside the main editor window. Any panel can float: the Inspector, the Content
Browser, the material graph, the anim graph, the sequence curve editor, the montage timeline and the rest.

Every method queues the same request the controls queue (a tab's context menu: Float in own window / Dock back; the
palette commands `view.float_panel` and `view.dock_floated_panels`; an OS window's close button docks its panels back),
so the agent route and the person's route run one code path. Queued changes apply on the editor's next frame.

## Methods

| Method | Params | Does |
|---|---|---|
| `editor.panel_float` | `panel`, `os_window?`, `dock?` | floats `panel` in a window of its own, or with `dock: true` docks it back into the main window. `os_window: true` (or `false`) requires an OS window (or a window inside the main one) and fails with `float_mode_mismatch` when the mode differs. Reply: the status below plus `queued: { panel, action }` and `os_window` (whether the float is its own OS window) |
| `editor.panel_instances` | none | `{ mode, os_mode, supported, windows: [{ window, tabs, os_window }], pending_requests, panels: [{ panel, instance, floating, os_window, window }] }` for every open panel |

`mode` is the preference (`os` or `in_window`); `os_mode` says whether floated panels are OS windows now (the preference
and a runner that opens windows); `supported` says whether the editor has such a runner (false in the headless editor).
`window` is a floated window's serial, stable while it lives.

```json
{"method": "editor.panel_float", "params": {"panel": "material_graph", "os_window": true}}
{"method": "editor.panel_instances", "params": {}}
{"method": "editor.panel_float", "params": {"panel": "material_graph", "dock": true}}
{"method": "editor.prefs_set", "params": {"values": {"ui.floating_windows": "in_window"}}}
```

## Behavior

- An OS window shares the editor's GPU device, its egui context (theme, `appearance.ui_scale`, fonts) and the edit and
  play worlds. Keyboard focus is per window. One editor frame draws every window, and a panel draws in one window only,
  so a floated panel never renders the 3D viewport twice.
- The layout file keeps an OS window as a `floating` entry (layout version 2, unchanged) with its rectangle relative to
  the main window. A restored window opens where it was; if its monitor is gone, it docks back. Where the window system
  gives no window positions (Wayland), the compositor places the window.
- Dragging an asset from one window onto a field in another works: the drop lands when the button is released over the
  other window (no hover highlight there while dragging).

## Error codes

| Code | When |
|---|---|
| `unknown_panel` | no panel has that id |
| `panel_not_open` | the panel has no tab |
| `float_mode_mismatch` | `os_window` asks for the other kind of window than the current mode gives |

## Tests

`cargo test -p regia_editor --test editor_os_windows` (a floated Inspector is a child viewport egui lists, draws only in
its window's pass, is saved relative to the main window and docks back; the close button and the palette dock panels
back and the palette floats the focused one; single-window mode and a runner without windows keep floats inside; a
restored layout reopens its windows where they were; an OS window never renders the viewport twice, on lavapipe).
`cargo test -p regia_editor --lib os_windows`.
