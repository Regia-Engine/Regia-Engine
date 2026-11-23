# Swapchain, HDR output and host paths (agents)

Each window's swapchain is planned from the surface capabilities, `OutputSettings` (present policy
and HDR request), the window's present mode, the backend and what the display reports about HDR
(ADR-525). `render.swapchain` reports the plan and the reasons behind it. `platform.paths` reports
where the app keeps its files on the host. Guide topic: `agent.guide {"topic": "swapchain"}`.

## `render.swapchain`

No parameters. Read-only. Returns `{ windows, count }`, one entry per live window surface, ordered by
window entity. A headless app has no window surfaces and answers `{ "windows": [], "count": 0 }`.

| Field | Meaning |
|---|---|
| `window` | Window entity bits. |
| `backend` | `vulkan`, `dx12`, `metal` (wgpu backend name). |
| `size` | `[width, height]` of the configured swapchain, in pixels. |
| `format`, `color_space` | wgpu names, for example `Bgra8UnormSrgb` / `Auto` (SDR), `Rgb10a2Unorm` / `Bt2100Pq` (HDR10), `Rgba16Float` / `ExtendedSrgbLinear` (scRGB). |
| `present_mode` | What the engine asked wgpu for (may be `AutoVsync` or `AutoNoVsync`). |
| `present_mode_effective` | The concrete mode after wgpu resolves an `Auto` mode: `AutoVsync` is `FifoRelaxed`, else `Fifo`; `AutoNoVsync` is `Immediate`, else `Mailbox`, else `Fifo`. |
| `alpha_mode` | `Opaque` whenever the surface lists it. |
| `frame_latency` | Frames the CPU may queue ahead of the display (2). |
| `copy_src` | The swapchain can be read back (screenshots). |
| `requested` | `{ present: vsync / uncapped / vrr, window_present: vsync / vsync_relaxed / immediate, hdr: sdr / hdr10 / scrgb }`. |
| `hdr_active` | The configured color space is HDR. |
| `hdr_fallback` | Why an HDR request was configured as SDR: `not_offered` (the surface does not list the format and color space pair) or `display_sdr` (the display is not in an HDR mode, for example Windows HDR is off). `null` otherwise. |
| `offered` | `{ present_modes, hdr }`: what the surface lists. |
| `display` | `{ hdr_mode, peak_nits, full_frame_nits, min_nits, sdr_white_nits, bits_per_color }`. Windows reports these through DXGI for DX12 and Vulkan alike; other platforms report `null` (unknown, not "SDR"). |

```json
{"jsonrpc": "2.0", "id": 1, "method": "render.swapchain"}
```

### The rules (ADR-525)

- **SDR** (the default) is the first sRGB format with the `Auto` color space, as before.
- **HDR on an SDR display.** DX12 lists its HDR color spaces whether or not Windows HDR is on, and
  the desktop compositor would tone-map an HDR swapchain back down to SDR. When the display reports
  `hdr_mode: false`, an HDR request is configured as SDR with `hdr_fallback: "display_sdr"`. While
  HDR is requested, the display is read again every 120 frames, so switching Windows HDR on or off
  while the game runs reconfigures the swapchain.
- **HDR pairs** are looked up in the surface's per-format color spaces, so a format the surface lists
  only with an HDR color space is still found.
- **VRR on DX12** presents with `Immediate`: sync interval 0 with tearing allowed, which is what
  variable refresh needs on a DXGI flip-model swapchain. Without tearing support it takes `Mailbox`.
  Vulkan, on Windows too, keeps `Mailbox` first.
- **Display luminance.** When the platform reports no display luminance (desktop), the display's
  peak and minimum nits from this report fill `DisplayLuminance`, and a calibration still at its
  platform default takes them (see [Display calibration](display-calibration.md)).

### Typical loop

1. `render.set_calibration {"hdr": true}` to request HDR10.
2. `render.swapchain`: check `hdr_active`. If it is `false`, `hdr_fallback` says why.
3. `seeing.screenshot` as usual: HDR frames are captured as their SDR rendition.

## `platform.paths`

No parameters. Read-only. Returns the app's host directories (`null` when not configured):

| Field | Meaning |
|---|---|
| `os` | `std::env::consts::OS` (`linux`, `windows`, `android`, `macos`). |
| `user`, `cache`, `logs` | Host paths of `user://`, `cache://`, `logs://`. On Windows `user://` is `%APPDATA%\RegiaEngine\<exe>` and `cache://` is `%LOCALAPPDATA%\RegiaEngine\<exe>`. |
| `crash_reports` | Where panic text reports go (`logs://`, read them with `diagnostics.crash_reports`). |
| `feedback` | The feedback folder: feedback bundles, crash bundles and native-crash minidumps. On Windows, with the standard `user://`, it is the non-roaming `%LOCALAPPDATA%\RegiaEngine\<exe>\feedback`, so dumps never sync with a roaming profile. |
| `saved_games` | The Windows `Saved Games` folder; `null` elsewhere. |
| `pipeline_cache` | `{ dir, backend, state, skipped }`. `state` is `active`, `backend_unsupported` (DX12 and Metal: wgpu 30 has no pipeline cache there, and the GPU driver keeps its own shader cache), `driver_skipped` (Intel's Windows Vulkan driver, v0.2.15, ADR-3288: the first cold compile of the Desktop High pipelines with the engine cache attached crashed inside the driver on an N150 laptop; the driver's own disk cache keeps warm launches as fast; `REGIA_PIPELINE_CACHE=1` attaches the engine cache to re-test a new driver), `no_device`, `no_directory` or `disabled` (`REGIA_PIPELINE_CACHE=0`). `skipped` is the driver rule's reason in words, `null` otherwise. |

```json
{"jsonrpc": "2.0", "id": 2, "method": "platform.paths"}
```

## Wayland frame callbacks

A focused window does not ask for Wayland's frame callback on any present mode, so its frames overlap as on
X11 and Windows. On `Fifo` or `FifoRelaxed` the swapchain paces the loop; with vsync off (`Immediate`, `Mailbox`)
only the frame cap does, also on a compositor that throttles frame callbacks. Every other window keeps it, because
the runner detects a hidden workspace by the missing callback (`swapchain::wants_frame_callback`, ADR-1750, amended
for the vsync-off modes by ADR-8170). The boot splash's present follows the same rule. So an unfocused Wayland window
can run at a lower rate than the same window focused, once CPU plus GPU time passes a vblank. A focused window under
a blanked panel keeps rendering.
