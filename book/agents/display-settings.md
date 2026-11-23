# Display settings and split-screen viewports

`GameSettings.display` (`regia_framework::settings`) is applied live to the primary window
(ADR-7880). A game's options menu writes the fields and the window follows on the same frame. No
restart is needed and the render device is not recreated: a vsync change reconfigures the swapchain,
and a size change goes through the resize path the renderer already has.

| Field | Applied to | Notes |
|---|---|---|
| `display.resolution` | the primary `Window::logical_size` | `null` keeps the window's own size, and going back to `null` restores the size it had before the setting applied. |
| `display.fullscreen` | the primary `Window::mode` | `true` is borderless fullscreen, `false` windowed, `null` (the default) leaves the window's mode alone. |
| `display.vsync` | the primary `Window::present_mode` | `true` keeps the window's own vsync mode (or `Vsync`), `false` is `Immediate`. |
| `display.frame_cap` | `FramePacing` | Unchanged. |

A window's own size, mode and present mode stand while the settings are at their defaults. When a
setting differs from its default, a window created later gets it too. This covers the first window on
resume and an Android window re-created on resume.

## Vsync

Vsync off asks the window for `Immediate`. The renderer takes the first mode the surface offers
from `Immediate`, `Mailbox` and FIFO, so a surface with only FIFO keeps vsync on, and the report
says so. Vsync on uses `AutoVsync` (FIFO relaxed, else FIFO). If the host's
`regia_render::OutputSettings::present` is `Uncapped`, turning vsync on sets it to `Vsync`. A `Vrr`
policy is never changed.

- **Android.** Vsync stays on. The frame pacer releases frames on the display's vsync grid
  (ADR-5946), and a present mode without the vsync wait would take that grid away. Vsync off is
  recorded (`vsync_note: "android_display_paced"`) and not applied. To lower the rate, use
  `display.frame_cap`.
- **Steam Deck.** Applied as on desktop. The frame cap, the Deck preset's pacing and the Wayland
  frame-callback rule (ADR-1750) are unchanged. With vsync off the window asks for the compositor's
  frame callback again, because FIFO no longer paces the loop.

## Resolution and fullscreen

The platform window layer (`regia_platform`, `apply_window_changes`) clamps each axis of a size
request to the monitor the window is on, when the OS reports one. Some platforms report a rotated
panel unrotated: the Steam Deck's 800 × 1280 panel, shown landscape, reads as 800 × 1280. When the
window's current size does not fit the report but fits it rotated, the rotated extent is used
(`orient_monitor`). When the window fits neither, the report is not trusted and nothing is clamped.
Before the OS has reported the window's own size the orientation is unknown, so the first request
goes out unclamped and is applied again, clamped, once the size arrives (ADR-7979).
`monitor_logical_size` in the report below is the OS's value as reported. Some requests are refused. The
window then keeps its old size and mode, `Window::logical_size` keeps the request as written, and
`regia_platform::WindowStatus::refused` holds the code:

| Code | Meaning |
|---|---|
| `window_size_invalid` | The size is not finite, or not positive. |
| `window_size_too_small` | The size is below `MIN_WINDOW_LOGICAL_SIZE` (320 × 180 logical pixels) on either axis. |
| `window_size_fixed` | The platform's window is the whole screen (Android). |
| `window_mode_fixed` | The platform's window is always fullscreen, so windowed mode cannot apply (Android). |

The next request that applies clears the refusal.

## Agent surface

`settings.get` answers `applied.display` beside the settings: in the whole-object answer, and in the
answer for a `display.*` path. The values come from `regia_framework::settings::DisplayApplied`, which
is refreshed in `Last`. They change one or two frames after a `settings.set`: the window layer applies
the change in `PostUpdate`, and the renderer reconfigures the swapchain in the next `PreUpdate`.

```json
settings.set { "path": "display.vsync", "value": false }
settings.get { "path": "display.vsync" }
→ { "path": "display.vsync", "value": false,
    "applied": { "display": {
      "window": 4294967296, "logical_size": [1600.0, 900.0], "applied_logical_size": [1600.0, 900.0],
      "physical_size": [1600, 900], "monitor_logical_size": [1920.0, 1080.0], "mode": "windowed",
      "refused": null, "present_mode": "immediate",
      "swapchain": { "planned": "AutoNoVsync", "effective": "Immediate", "vsync": false,
                     "offers_immediate": true, "offers_mailbox": true },
      "vsync_note": null } } }
```

| Field | Meaning |
|---|---|
| `window` | The primary window entity; `null` without one (headless, before the first resume). |
| `logical_size`, `applied_logical_size` | What the window asks for, and what the window layer passed to the OS after the monitor clamp. |
| `physical_size`, `monitor_logical_size` | What the platform reports. |
| `mode` | `windowed` or `borderless_fullscreen`. |
| `refused` | `{ code, requested_size }` with a code from the table above, or `null`. |
| `present_mode` | The window's mode: `vsync`, `vsync_relaxed` or `immediate`. |
| `swapchain` | The live swapchain: `planned` (the mode handed to wgpu), `effective` (what wgpu runs), `vsync` (the effective mode waits for vertical blank) and the modes offered. `null` without a surface. |
| `vsync_note` | Why vsync differs from the setting: `android_display_paced`, `no_unsynced_mode_offered` (vsync is off but the surface has only FIFO) or `host_vrr_policy` (the host's VRR policy picks its own mode). `null` otherwise. |

`render.swapchain` ([swapchain](swapchain.md)) still lists every window's full plan.

## Split-screen viewports

`regia_framework::player::SplitScreenCamera { player }` marks a camera as a local player's view. In
the client build the player plugin writes that player's `SplitScreenLayout` rect onto the camera's
`Camera::viewport` in `PostUpdate`, after the layout is recomputed. It writes only when the two
differ, which happens on a join or a leave. A tagged camera whose player has no rect gets the full
target. Draw order, target and every other camera field stay the game's, and untagged cameras are
not touched.

`players.list` gives each player's `viewport` (`[x, y, w, h]`, or `null` for a player without a
local view) and `cameras`, the tagged cameras with the viewport each one has now:

```json
players.list {}
→ { "players": [ { "index": 0, …, "viewport": [0.0, 0.0, 0.5, 1.0],
                   "cameras": [ { "entity": 8589934601, "viewport": [0.0, 0.0, 0.5, 1.0] } ] },
                 { "index": 1, …, "viewport": [0.5, 0.0, 0.5, 1.0], "cameras": [] } ],
    "count": 2, "max_local": 4 }
```

## Checks

- `cargo test -p regia_framework --lib settings::tests::display` and `settings::tests::vsync`: the
  window's present mode, size and mode follow the settings and come back to the host's values, a
  window created later or re-created gets them, and the report carries the platform's clamp and
  refusal.
- `cargo test -p regia_framework --lib player::tests::split_screen`: two and four players, a leave,
  and an untagged camera.
- `cargo test -p regia_platform --lib window::tests`: the clamp and the refusal codes.
- `cargo test -p regia_render --lib swapchain::tests::a_vsync_toggle`: the swapchain's effective
  mode and the FIFO fallback.
- `cargo test -p regia_agent --lib framework::tests::settings_get_reports` and
  `players_list_reports`.
