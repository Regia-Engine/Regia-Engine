# Seeing: screenshots, strips and image comparison

Everything an agent needs to *see* results: PNG captures of the window or a mesh asset, multi-frame
strips, orbit contact sheets, and SSIM comparison with a difference image. Every result carries
`png_base64` (MCP clients get an image block automatically).

## Screenshot

```json
seeing.screenshot { "target": "window", "timeout_ms": 30000 }
→ { "width": 1280, "height": 720, "mime": "image/png", "png_base64": "..." }
```

- `target: "window"` (default) captures the primary window (`entity` selects another window).
- `target: "asset"` renders a mesh asset on the neutral preview stage:
  `{"target": "asset", "path": "assets/khronos/DamagedHelmet.glb"}`.
- `target: "offscreen"` renders headless (no window) from the extracted ECS frame
  (`Camera` + `MeshRenderer`). Optional `width` / `height` (default 256×144). Use with
  [`enable_fixed_step`](headless-capture.md) for byte-identical lavapipe goldens.
- `calibration: "applied"` (default) captures what the player sees. `"neutral"` captures the
  frame before the player's [display calibration](display-calibration.md), identical to a
  neutral-calibration capture. Use `"neutral"` for goldens.
- Offscreen captures include the game HUD (`hud`, default true): the runtime UI is laid out for
  the output size, not the window's. `layout` picks the HUD layout: `"auto"` (by orientation),
  `"landscape"` (16:9) or `"portrait"` (native 9:16, e.g. `width: 1080, height: 1920`). Roots
  tagged `UiVariant` show only in their layout; `SafeAreaPadding` roots are inset by the
  render-out safe area (broadcast title-safe 5 % for landscape; 12 % top / 20 % bottom / 5 % left
  / 10 % right for portrait; override with `UiOutput::safe_area`). `hud: false` omits it.
- `window_layers: true` also draws the window overlay layers over the frame, as a visible window
  shows them: the [quick draw](quick-draw.md) 2D layer and the runtime UI.
- The editor viewport (sky, proxies, grid, gizmo, no window chrome) is
  `editor.capture_viewport`, documented on the [editor](editor.md) page. `seeing.screenshot`
  of a running editor is the whole window, chrome included.
- `target: "camera"` renders one camera entity's view offscreen, whatever its render target or
  viewport: `{"target": "camera", "name": "SideCam"}` (the first camera with that `Name`) or
  `{"target": "camera", "entity": 4294967301}`. `width` / `height` (or `size: [w, h]`) default
  to 256×144; the HUD is off unless `hud: true`. The reply adds `camera` (the entity). A camera
  spawned this frame has no view yet: step a frame first (`screenshot_camera_not_rendered`).
- `target: "ui"` renders the runtime UI layer (the window overlay layer `ui`) alone over
  transparent black, laid out for `width` × `height` (default 1280×720) and `layout`, with no
  scene under it: every pixel the UI leaves alone has alpha 0. An app without a UI layer answers
  `screenshot_no_ui_layer`. Safe-area previews are the editor's UI designer
  (`editor.ui_designer_*`).
- `overlays` draws debug outlines over the picture of any target, after the display calibration
  and with no depth test (an outline inside a wall still shows): `"colliders"` (every
  `Collider`: box edges, sphere / capsule / cylinder / cone rings, trimesh edges, heightfield
  grid, compound children; convex hulls as their bounding box; solid green, sensors cyan),
  `"light_radii"` (point-light radius circles, spot-light cones; yellow), `"bounds"` (every
  `MeshRenderer`'s mesh box; orange) and `"grid"` (1 m, ±10 m on Y = 0). The reply adds
  `overlay_pixels`, the pixels they drew. `"selection"` and `"labels"` draw editor state: they
  answer `F0013` with reason `overlay_needs_editor`; use `editor.set_viewport { colliders,
  light_radii, bounds, labels }` and `editor.capture_viewport` in the editor. Overlays need a
  camera view to project through (`screenshot_no_view` before the first frame).
- `view_mode` needs the editor renderer's debug passes — this build answers `F0013` instead of
  pretending.

- A window the OS will not present (minimized, occluded, a locked console or a sleeping display)
  still answers `target: "window"`: a game renders the capture offscreen at the window's size, and
  the editor draws its frame into a stand-in target, so the capture shows what the window would
  show. While the OS refuses the window's surface the app steps at its hidden rate
  (`LoopControl::throttled_fps`), so a capture may take up to one hidden-rate frame longer
  (ADR-7335).
- Repeated window captures do not grow the app's memory: the readback buffer, the pixels, the PNG
  and the reply text are reused across shots of the same size (ADR-8848). The first capture's
  buffers stay until the app suspends or exits.

| Reason (`data.reason`) | Code | When |
|---|---|---|
| `overlay_needs_editor` | F0013 | `overlays` names `selection` or `labels` |
| `screenshot_no_view` | F0003 | overlays before any camera was extracted |
| `screenshot_not_a_camera` | F0002 | target `camera`: `entity` has no `Camera` |
| `screenshot_camera_not_rendered` | F0003 | target `camera`: the camera has no view in the frame yet |
| `screenshot_no_ui_layer` | F0013 | target `ui`: the app draws no `ui` layer |

## Sequences and orbit sheets

```json
seeing.capture_sequence { "frames": 4, "every_n": 5, "timeout_ms": 60000 }
→ one strip image: N window screenshots taken `every_n` frames apart
seeing.contact_sheet { "path": "assets/khronos/DamagedHelmet.glb", "angles": 4 }
→ one strip image: the mesh from N orbit positions
```

Use the sequence for motion/VFX review and the contact sheet to show a model from all sides.
At most 8 sequence and sheet jobs run at once; a ninth call answers `too_many_pending`
(JSON-RPC -32600) until one finishes.

## Comparing images

```json
seeing.compare_images { "a": "<base64 png>", "b": "<base64 png>" }
→ { "ssim": 0.98, "max_channel_diff": 3, "same_size": true, "diff_png_base64": "..." }
```

SSIM per Wang et al. (IEEE TIP 2004): 1.0 = identical; flat-but-different images score low but not
zero. The classic workflow: screenshot before your edit, apply it, screenshot after, compare — the
`diff_png_base64` shows *where* pixels changed.

The images come from earlier `seeing.*` calls (take the `png_base64` values) — the comparison
itself needs no running window, so it works headless.

## Picking

`seeing.pick_entity {"x": 320, "y": 240}` returns the entity under a window pixel — once the
renderer exposes its entity-id buffer. Until then it answers `F0013`; find entities spatially with
`scene.query_entities` (`near`/`box`) instead.

## Memory

```json
seeing.memory_report { "top": 5 }
→ { "available": true, "backend": "Vulkan", "allocated_bytes": 972..., "reserved_bytes": 1476...,
    "rss_bytes": 1434..., "peak_rss_bytes": 1980..., "device_allocated_bytes": 972...,
    "staging": { "cap_bytes": 134217728, "flushes": 0, "flushed_bytes": 0, "waits": 0, "peak_pending_bytes": 2310... },
    "categories": { "terrain_height": { "bytes": 89489408, "count": 2 }, "meshes": {...}, ... },
    "labels": [ { "label": "material texture", "category": "meshes", "bytes": 201392128, "count": 36 }, ... ] }
```

Every live GPU allocation of the render device, from the backend allocator (Vulkan, D3D12), sorted
into categories by its wgpu label: `terrain_height`, `terrain_layers`, `terrain_macro`,
`terrain_masks`, `terrain_geometry`, `scatter`, `meshes` (vertex/index buffers and material
textures), `shadows` (shadow maps and their atlases; the UI glyph and VFX atlases count as `other`, a
lightmap atlas as `probes_luts`, ADR-2952), `render_targets`, `probes_luts`, `frame_data`, `other`, `unlabeled`. A
growing `unlabeled` or `other` means a new allocation needs a label (the rules are in
`regia_render::memory`). `rss_bytes` is the process resident set and `peak_rss_bytes` its peak:
Linux and Android report `VmRSS` and `VmHWM`, Windows reports the working set and peak working
set, macOS the task's resident size and `ru_maxrss`, and both are `null` on other targets and when the OS query fails. `staging` is the upload staging flush that bounds a level
load's write staging (ADR-1346, [Load memory](load-memory.md)). On GL and Metal `available` is `false`. `device_allocated_bytes` is the whole device's
allocated size, the one figure to hold against a GPU memory cap on every backend (ADR-5860): the
allocator's live total (`allocated_bytes`) on Vulkan and D3D12, `MTLDevice.currentAllocatedSize` on Metal
(driver-internal resources included, no categories or labels), `null` on GL and WebGPU. It answers `F0011` before a render device
exists.

Budgets and the DO27 measurements: `docs/evidence/perf/memory-20261007.md`; the CLI version is
`lookdev`'s `mem_report` (load time, bytes read, RSS, GPU by category, streaming soak).

## Photosensitivity check

```json
seeing.flash_check { "dir": "captures/trailer", "fps": 30 }
seeing.flash_check { "frames": 120, "fps": 60, "every_n": 1 }
→ { "passed": false, "max_flashes": 5, "window_start_s": 1.25,
    "region": { "x": 0, "y": 0, "width": 341, "height": 256 },
    "area_fraction": 1.0, "kind": "general",
    "general_flashes": 5, "red_flashes": 0, "frames": 120 }
```

`seeing.flash_check` counts general flashes and saturated-red flashes in a sequence of sRGB frames
(WCAG 2.2 success criterion 2.3.1). Pass one of two sources, plus `fps`:

- `dir`: a folder of PNG frames, read in file-name order. Subfolders are skipped. Relative paths
  stay inside the project (or the temp directory, or an `output_roots` entry).
- `frames`: capture the primary window now, `every_n` engine frames apart (default 1), and analyze
  those frames. `frames` is 2 to 600. The pixels are not kept. At most 8 sequence, sheet and flash
  jobs run at once; a ninth answers `too_many_pending`.

`fps` is the rate the sequence represents, from 1 to 480. Frame `i` is at time `i / fps`, so the
same square wave captured at 30 fps and at 60 fps gets the same verdict. A missing or out-of-range
`fps` is `fps_out_of_range`. Passing both `dir` and `frames` is `F0002`. Passing neither is
`no_frames`. An empty folder is also `no_frames`. `frames` outside 2 to 600 is
`frames_out_of_range`. More than 18000 PNGs is `too_many_frames`.

A general flash is a pair of opposing changes in relative luminance of 10% or more where the
darker image is below 0.80. A red flash is a pair of opposing transitions involving a saturated
red, using WCAG 2.2 definition note 3: at least one transition is to or from a state with
`R / (R + G + B) ≥ 0.8` on the linear channels, and each transition differs by more than 0.2
in CIE 1976 UCS chromaticity. Black has undefined chromaticity; this aid conservatively uses
`(u′, v′) = (0, 0)`. More than three flashes in any one-second period fails
when the flashing cells cover more than a quarter of any 10 degree field. The field is 341×256 px
on a 1024×768 screen; pass `reference_width`, `reference_height`, `ten_degree_width_px` and
`ten_degree_height_px` to document a different screen or viewing distance. `cell_px` (default 8,
1 to 4096) is the square the analyzer averages before it counts. A zero or oversized geometry
field is `geometry_out_of_range`. A frame side above 8192, an area above 4096×4096, or more than
2,500,000 cells is `frame_too_large`.

`passed` is false when either kind fails. `kind` is `general` or `red`. The other counts are
always in the report. `region` is the cell-aligned 10 degree window, and `area_fraction` is how
much of it was flashing in the reported second. `window_start_s` is the start of that second.

The same check offline, for a render-out or a trailer:

```text
cargo xtask flash-check captures/trailer --fps 30
cargo xtask flash-check captures/trailer --fps 30 --json
```

Exit 0 passes, exit 2 fails the thresholds, exit 1 is a usage or I/O error.

Error codes: `no_frames`, `empty_frame`, `frame_size_mismatch`, `time_not_monotonic`,
`geometry_out_of_range`, `frame_too_large`, `bad_png`, `dir_not_found`, `fps_out_of_range`,
`frames_out_of_range`, `too_many_frames`, `too_many_pending`. A capture with no renderer is
`F0011`, no primary window is `F0003`, and a capture that fails is `F0015`. `every_n` outside
1 to 3600, a wrong JSON type, or both `dir` and `frames` at once is `F0002`.

This is an aid for a release check. It is not a certification and it is not a guarantee that a
sequence is safe to watch. The balanced-pattern exception for squares under 0.1 degree is not
applied. There is no runtime limiter on engine effects yet.

