# XR (OpenXR seam)

<!-- index: OpenXR -->
<!-- index: xr_no_loader -->
<!-- index: XR simulator -->

`regia_platform::xr` is the engine's side of a head-mounted display: a backend-agnostic trait
(`XrBackend`), the session state machine, per-eye poses and fields of view with the projection
math, late-latch pose prediction, controller poses and actions, and a scripted `NullXr` backend for
tests and headless runs. No OpenXR SDK is linked in this version (ADR-1571): a real runtime
binding implements `XrBackend` later, and everything here stays the same.

## What runs in this version

`regia_xr` runs the configuration, the action manifest, comfort options, the scripted simulator, and
stereo rendering on that simulator. `XrBackendKind::Simulator` is the backend that creates a session.

`XrBackendKind::OpenXr` and `Auto` do not. They start in app state `Off` with `last_error.code`
`xr_no_loader`. No loader, instance, or swapchain is linked (ADR-1571). `xr.set_option` with
`runtime_json` stores a path. This version has no binding that reads it, so the option does not open
a headset. The error text says the binding is not built. It does not mean the player's install is
missing a file you can add.

Sentences below that say "a real runtime" describe the seam. That runtime is not in this tree. Use
the simulator.

| Method | Permission | What it does |
|---|---|---|
| `xr.get` | read | Backend, session state, frames run, the last frame, the late latch, head, controllers, recent events; with the `regia_xr` plugin also the app state, options, stats and the last error. |
| `xr.drive` | play | Move the scripted backend through the session states, run frames, move the head, play a haptic pulse; on the simulator also `pose`, `input` and `session_state`. |
| `xr.set_option` | play | Change one XR option (`regia_xr` plugin). |
| `xr.recenter` | play | Recenter the rig on the head (`regia_xr` plugin). |
| `xr.capture_eye` | read | Read back the eye render targets, what the headset shows or the mirror, with each eye's pose, field of view and view-projection (`regia_xr` plugin). |
| `xr.layers` | read | The composition layers the last frame submitted, and the runtime's layer budget. |
| `xr.set_quad` | play | Add, replace or remove a panel drawn as a quad layer (`XrRenderPlugin`). |

Both need an `XrRuntime` resource (`XrRuntime::null()` wraps a `NullXr`). Nothing installs one by
default. Without it `xr.get` answers `{ "installed": false }` and `xr.drive` answers
`{ "refused": "xr_not_installed" }`.

## Session states

`idle` → `ready` → `running` → `stopping` → `idle`. A real runtime also reports the OpenXR states
after the session begins: `running` → `synchronized` (frames flow, nothing is shown, zero layers are
submitted) → `visible` (shown, input not focused) ↔ `focused` (input live); `visible` → `synchronized`
when the frames are hidden again; `loss_pending` from any state but `exiting` (end the session with
`end`, which goes to `idle`, then re-create it); `exiting` from `idle` (the user leaves the app).
`running` is also what a backend that does not report the finer states stays in; it counts as
focused for input. A change into or out of `focused` adds a `focus_changed { focused }` event. `xr.drive` actions: `ready` (the runtime becomes
ready), `begin` (ready → running), `exit` (the app asks to stop), `stop` (the runtime stops the
session: running → stopping, ready → idle), `end` (stopping → idle). Any other order is refused with
`xr_invalid_transition` and the state is unchanged.

```json
{ "action": "ready" }
→ { "ok": true, "action": "ready", "state": "ready", "frames": 0 }
```

## Frames

`xr.drive { "action": "frame", "count": 3 }` runs wait, begin, locate views, sync actions, render,
late latch, end, three times. Frame times are whole display periods from a fixed start, so a run is
deterministic. Calls out of order are `xr_frame_out_of_order`, a frame before `begin` is
`xr_not_running`, an `end_frame` for another display time is `xr_display_time_mismatch`.

`xr.get` then shows `last_frame` (index, display time, rendered, the two views with pose and field
of view as signed half-angles in radians), and `latch.latched` / `latch.clamped`. A late-latch
prediction is clamped to `max_extrapolation_ns` (50 ms by default) and counted in `clamped`.

## Projection

`Fov::projection(DepthRange, DepthMode)` builds the asymmetric frustum: right-handed, view looks
down −Z, clip +Y up, depth zero to one. `DepthMode::Reversed` puts near at 1 and far at 0 (the
renderer's convention); `Standard` is near 0, far 1. `far: f32::INFINITY` is an infinite far plane.
A bad field of view (not finite, `left >= right`, `down >= up`, any angle at or beyond 90 degrees),
`near <= 0`, or `far <= near` is an error (`xr_invalid_fov`, `xr_invalid_depth_range`).

## Head, controllers, haptics

`xr.drive { "action": "head", "position": [0, 1.7, 0], "yaw": 0.5 }` moves the head; the eyes sit
half the interpupillary distance (63 mm by default) either side along the head's X axis.
`xr.get.controllers` lists both hands: `active`, grip and aim poses, trigger, squeeze, thumbstick,
the face buttons and the menu button. An inactive controller reports released inputs.
`{ "action": "haptic", "hand": "left", "amplitude": 0.5, "duration_ms": 50 }` plays a pulse;
an amplitude outside 0..=1 is `xr_invalid_haptic`.

## The XR plugin (spec 58)

`regia_xr::XrPlugin` (`regia::xr` with the umbrella's `xr` feature, off by default) opens the backend
named in `config/xr.ron` and drives the session from runtime events: it begins the session when the
runtime is ready, ends it when it stops, tears it down on `loss_pending` and re-creates it after
`retry_frames` fixed steps (a counter, never the wall clock), and asks the app to exit on `exiting`.
Backends: `Simulator` (the scripted `NullXr` plus an optional `*.xrsim.ron` script), `OpenXr` and
`Auto`. The OpenXR binding is not built yet (spec 58 § 3.4 gates it on a feasibility spike that needs
an XR runtime), so `OpenXr` and `Auto` start in app state `Off` with `last_error.code` `xr_no_loader`
and the desktop game keeps running. With `required: true` the app exits instead.

No hardware session is ever reported in that state: `xr.get` has no `state`, `frames` or `last_frame`
block, `backend` is `"none"`, `runtime` and `system` are null, `sessions_created` is 0 and `stats` are
zero. The `last_error.message` says the OpenXR binding is not built in this engine version (it does
not blame the player's install). The missing loader is not retried; `xr.set_option enabled true`
answers `xr_no_loader` and creates nothing, and the simulator drives answer `xr_not_running`. To
check it, call `xr.get` after the first frame and read `app_state` (`Off`), `last_error.code`
(`xr_no_loader`), `sessions_created` (0) and the absence of `frames` (`XrState` is not one of the
`play.wait_until` resources). The first real binding slice and the owner's prerequisites are in
`docs/notes/xr-openxr-loader-spike.md`.

`xr.get` then also carries:

| Field | Meaning |
|---|---|
| `app_state` | `Off`, `NoSession`, `Idle`, `Starting`, `Running` (synchronized or visible), `Focused`, `Stopping`, `LossPending` (a runtime loss passes through it and rests at `NoSession` with `retry_in` and `last_error` `xr_runtime_lost`; do not `wait_until` it), `Exiting` (compare with `wait_until { expr: "xr.app_state == 'Focused'" }`) |
| `backend_kind`, `session_state` | the configured backend (`auto`, `openxr`, `simulator`, `off`) and the session state name |
| `runtime`, `system` | runtime and headset names (null on the simulator) |
| `tracking_space`, `actual_tracking_space` | requested and used space (`stage` falls back to `local_floor`, then `local`) |
| `refresh_hz`, `render_scale`, `foveation`, `mirror`, `comfort` | the live session values (`refresh_hz` is the session rate, not a pending `xr.set_option`) |
| `extensions_enabled`, `hand_tracking` (`supported` / `unsupported`), `play_area` (`{ width_m, depth_m }` or null) | what the runtime offered |
| `stats` | `frames`, `stale_frames` (extractions that found no new frame: the old one was repeated), `bad_poses` (non-finite poses rejected, the previous kept), `dropped_frames`, `over_budget` (`xr_over_budget` diagnostics raised, see "Frame budget") |
| `budget` | the frame budget: `budget_ms`, `frames` (N), `auto_scale`, `over_run` (consecutive overruns so far), `last_frame_ms`, `scripted_ms` (the simulator's `frame_time`, or null) and `last` (`{ code: "xr_over_budget", frame_ms, budget_ms, frames, scale_from, scale_to }` or null) |
| `last_error` | `{ code, message }` or null |
| `retry_in`, `sessions_created` | fixed steps until the next re-creation; sessions created so far |
| `offset`, `held_head`, `vignette`, `snaps`, `ticks`, `actions_attached` | rig-local offset (recenter, emulated floor), the head pose held for the fixed step, comfort state, fixed steps run |
| `actions` | every manifest action at the last sync: `{ name, sub, active, value }`; all inactive at their defaults while the session is not running or focused |

While the plugin has no session (off, no headset, a lost runtime) only these fields are present.

### Simulator drives

On the simulator `xr.drive` adds six actions; on a real runtime they answer `xr_simulator_only`.

- `{ "action": "pose", "head": { "position": [0, 1.7, 0], "yaw": 0.3 }, "right": { "position": [0.2, 1.2, -0.3] }, "fov_deg": 100, "ipd_m": 0.063 }`
  sets the head and controller poses (`orientation: [x, y, z, w]` instead of `yaw` is accepted) and the
  eyes' field of view and IPD. A non-finite pose is rejected, the previous pose kept and
  `stats.bad_poses` counted.
- `{ "action": "input", "name": "select", "sub": "right", "value": 0.8 }` scripts a manifest action
  (a bool, a number or `[x, y]`); it is read at the next fixed-step sync and held until replaced. An
  undeclared action is `xr_action_unknown`. A non-finite number or pose is `xr_invalid_value` and is not held.
- `{ "action": "session_state", "state": "focused" }` moves the runtime to `ready`, `synchronized`,
  `visible`, `focused`, `stopping`, `loss_pending` or `exiting` through the shortest valid path, and
  the plugin applies its reactions at once. The answer carries `app_state`.
- `{ "action": "lens", "radius": 0.9 }` scripts a lens: everything outside the ellipse inscribed in the
  field of view, shrunk by `radius`, is the visibility mask. 0 removes it; outside 0.1..=2 is
  `xr_invalid_value`.
- `{ "action": "mono", "on": true }` puts both eyes at the head: the zero-disparity check. A real IPD
  stays inside 20 to 100 mm.
- `{ "action": "frame_time", "ms": 20 }` sets the frame time the frame budget sees for every following
  XR frame (finite, 0 or more; `null` stops it). The simulator never reads the wall clock for the
  budget, so without it simulator frames are not measured. A negative, non-finite or missing `ms` is
  `invalid_params`.

### Action input

The action manifest (`assets/xr/actions.xr.ron`, `format_version` 1) declares action sets and typed
actions (`Bool`, `Float`, `Vec2`, `Pose`, `Haptic`) with sub-paths `left` / `right`, and suggested
bindings per interaction profile. The sets are attached once per session; creating an action or
attaching again afterwards is `xr_actions_frozen`. Each fixed step the plugin syncs once and
publishes every active button, axis or stick to `regia_input` (as a held injection on the input
action of the same name, or `<name>_<sub>` for an action with two sub-paths, so existing bindings
work unchanged); an inactive one is released so keyboard or pad bindings of the same action still
work. Pose actions go to the `ActionPoses` resource. Haptic amplitudes clamp to 0..=1; a device with
no haptics is a soft no-op.

### Options: `xr.set_option`

```json
{ "key": "render_scale", "value": 1.25 }
→ { "key": "render_scale", "old": 1.0, "new": 1.25, "applied_at": "next_frame" }
```

| Key | Value | Applied |
|---|---|---|
| `enabled` | bool: `false` tears the session down (`Off`), `true` opens the backend again | now |
| `tracking_space` | `local`, `stage`, `local_floor` (fallback applies; sends an origin change) | now |
| `render_scale` | 0.5..=2.0 | next_frame |
| `foveation` | `off`, `low`, `medium`, `high` (stored and reported; the simulator applies no foveation) | next_frame |
| `mirror` | `off`, `left`, `right`, `both` (Android: `off` only) | now |
| `snap_turn_deg` | 0..=180 (0 = smooth turn) | now |
| `vignette_strength` | 0..=1 | now |
| `eye_height_m` | 0.5..=2.5 (the emulated floor when `stage` falls back to `local`) | now |
| `refresh_hz` | 30..=240 (stored as `preferred_refresh_hz`; the reply's new value is that pending rate; `xr.get`'s `refresh_hz` stays the live session rate until the next session) | next_session |
| `runtime_json` | a runtime manifest path for the loader (dev, desktop only); stored only until the OpenXR binding reads it | next_session |
| `stereo_mode` | `two_pass`, `multiview` | next_frame |
| `hidden_area` | bool: use the runtime's visibility mask when it offers one | next_frame |
| `depth_layer` | bool: send each eye's depth when the runtime takes it | next_frame |
| `auto_scale` | bool (default `false`): lower `render_scale` one step on each `xr_over_budget` (see "Frame budget"); a runtime option, not a `config/xr.ron` field | now |

An unknown key is `xr_unknown_option`; a wrong type or a value out of range is `xr_invalid_value` with
the range in the message. `xr.get` reflects a change that applies now or next frame. `refresh_hz` and
`runtime_json` apply at the next session: `xr.get`'s `refresh_hz` stays the live rate until then.

### Frame budget

The budget of an XR frame is 0.9 of the display period (10 ms at 90 Hz, 12.5 ms at 72 Hz; the runtime
keeps the rest for its own reprojection). When 10 consecutive frames (`budget.frames`) run over it, the
plugin raises the diagnostic `xr_over_budget`: a warning in the log, `stats.over_budget` counts it,
`budget.last` describes it and the `XrOverBudget` message reaches game code. With `auto_scale` on,
each raise also lowers `render_scale` by 0.1 (0.5 is the floor: at the floor the diagnostic still
fires and the scale stays); the eye images follow at the next frame. The run then starts over, so the
next step needs 10 more overruns at the new scale. A frame inside the budget resets the run. Nothing
raises the scale again: do that with `xr.set_option render_scale`.

The frame time is the CPU time of the stereo frame the renderer drew on a real runtime. On the
simulator it is only what `xr.drive frame_time` scripts, so runs stay exact:

```json
{ "method": "xr.set_option", "params": { "key": "auto_scale", "value": true } }
{ "method": "xr.drive", "params": { "action": "frame_time", "ms": 20 } }
{ "method": "play.step", "params": { "frames": 10 } }
{ "method": "xr.get", "params": {} }
```

After the tenth frame `stats.over_budget` is 1, `render_scale` 0.9 and `budget.last.scale_to` 0.9.

### `xr.recenter`

`{}` → `{ "offset": pose }`: the head's current yaw and floor position become the rig's forward and
origin (height is kept). The game sees an `XrOriginChanged` message.

### `xr.capture_eye`

`{ "eye": 0 | 1 | "both", "source"?, "width"?, "height"? }` → `{ "eyes": [{ eye, pose, fov, view_projection, image }], "image_source" }`.
`view_projection` is the 16 column-major floats the renderer is fed for that eye (the asymmetric
frustum with the frame's standard depth and the renderer's far plane). The image is PNG base64,
optionally resized to `width` × `height` with a box filter. `source` picks what is read:

| `source` | Image | `image_source` |
|---|---|---|
| `eye` (default) | the eye's render target, before any quad layer | `eye_targets` |
| `display` | what the simulator's compositor shows: the eye with its quad layers | `display` (`eye_targets` on a real runtime, which keeps that image, and on a simulator frame that composed no quad layer, where the eye image is what it shows) |
| `mirror` | the desktop mirror at `width` × `height` (640 × 360 by default), in a `mirror` field beside an empty `eyes` | `mirror` |

The app state must be `Running` or `Focused`. `Off`, `NoSession`, `Idle`, `Starting`, `Stopping`,
`LossPending` and `Exiting` return `xr_not_running`. `Running` or `Focused` with no frame report yet
is also `xr_not_running` (`capture_eye (no frame yet)`). When a frame exists and `SimEyeTargets` is
absent, `image` is null and `image_source` is `"none"`.

### Stereo rendering (`XrRenderPlugin`)

Add `regia_xr::XrRenderPlugin` after `XrPlugin` and the renderer's plugin. The XR frame then runs in
the `Render` schedule: the eyes draw this frame's scene, the frame's layers go to `end_frame`, and the
desktop view gives way to the mirror. While the runtime hides the frames (`synchronized`), no eye
draws, an empty layer list is submitted and the desktop view stays. The eye images are the runtime's
recommended size (1440×1584 on the simulator, which has no runtime query) times `render_scale`, re-created between frames.

`stereo_mode` is the render-profile switch (`XrRenderProfile`):

| Mode | What runs | Runs on |
|---|---|---|
| `two_pass` (default) | The whole frame once per eye, each eye with its own targets, temporal history, shadow maps and post chain. | Every device. |
| `multiview` | One pass over a two-layer target: the reduced forward set (one directional light, ambient, material factors, no shadows, no post stack, skinned meshes in their rest pose). For a standalone headset's budget. | A device with multiview. Without it the frame falls back to `two_pass`; `render.mode` shows which ran. |

An eye has an asymmetric frustum and takes its pose from the runtime, never a fixed IPD. Both eyes
cull once with one frustum around both, so both draw the same set.

`xr.get.render` reports:

| Field | Meaning |
|---|---|
| `stereo_mode`, `mode`, `multiview_supported`, `multiview` | what was asked, what ran, what the device can do |
| `extent`, `per_eye_history`, `hidden_area`, `depth_layer` | the profile |
| `frames`, `scene_passes`, `drawn`, `last_cpu_ms` | stereo frames, scene passes of the last frame (2 or 1), objects drawn per eye, CPU time |
| `hidden_triangles`, `depth_layers`, `quads_composed`, `quads_overflow` | triangles masked per eye, eyes whose depth reached the runtime, panels composed and panels over the budget |
| `disabled_effects` | `{ effect, reason }` for each effect the XR profile turns off |

**The XR quality profile** is fixed: SSR, SSGI, probe updates, volumetric fog and clouds, refraction,
the visibility buffer, paged sun shadows, motion blur and depth of field stay off, and exposure does
not meter, so the eyes can never disagree. Temporal AA stays on with per-eye history
(`per_eye_history`, false on Android, where FXAA stands in). VFX draws in the main view only, so it
is not in the eyes yet.

**Comfort vignette.** After the tonemap each eye darkens by the angle off its view axis:
`vignette_strength` scales it, `vignette_inner` and `vignette_outer` are radians of field of view, and
it rises only with the rig's artificial motion (`xr.get.vignette`), never with the user's own head.

**Hidden-area mask.** When the runtime offers a visibility mask, its triangles fill the depth buffer at
the near plane before the prepass (those pixels never shade) and are painted black after the tonemap,
so a mirror or capture never shows them. In `multiview` mode only the black paint runs. The mesh
reloads when the runtime reports a change or the field of view moves.

**Depth layer.** When the runtime takes depth, each eye's depth goes beside its color image with the
near and far planes the projection used, so the runtime can reproject positionally. A runtime without
it gets color only. A projection layer always carries the pose each eye was rendered with, not the
late-latched one.

**Quad layers.** `xr.set_quad` or the `XrPanels` resource declare panels. A panel is locked to the
`head`, the `rig` or the `world`, carries an explicit `priority` (equal priorities draw in id order) and
skips post-processing. Panels beyond the runtime's budget (16 layers with the projection layer) are
drawn into the eye images under the composed ones; the top panels stay layers.

```json
{ "id": 2, "space": "head", "position": [0, 0, -1], "size_m": [0.5, 0.3], "priority": 1, "color": [255, 0, 0, 255] }
→ { "id": 2, "count": 1 }
{ "id": 2, "remove": true }
→ { "id": 2, "removed": true }
```

**Mirror.** The desktop window shows `left`, `right` or `both` eyes side by side, each cropped to fill,
through a window overlay layer under the UI (`capture.screenshot` sees it too). `off` leaves the window
to its UI. Android has no mirror.

`xr.layers` → `{ installed, count, max_layers, depth_layer_supported, projection: { views: [{ pose, fov, image: { array_index, rect }, depth }] } | null, quads: [{ id, priority, space, pose, size_m, image }] }`.

### A typical check

Start the simulator, `xr.drive { action: "session_state", state: "focused" }`, `xr.drive { action: "pose", ... }`,
step N fixed frames, `xr.get` and `expect` on `app_state`, `held_head`, `actions`, or on the `XrHead`
and `XrController` transforms through the world-query methods (they are plain components).

## Error codes

Errors are `F0002` with `data.reason` set to one of: `xr_invalid_transition`, `xr_not_running`,
`xr_frame_out_of_order`, `xr_display_time_mismatch`, `xr_invalid_haptic`, `xr_invalid_fov`,
`xr_invalid_depth_range`, `xr_invalid_ipd`, `xr_unsupported`. `xr.drive` parameter mistakes (unknown
action, `count` outside 1..=1000, a missing or malformed `position` or `hand`, a `yaw` that is not a
number, a negative `duration_ms`, a number too large for 32-bit floats) are plain `F0002`.

Spec 58 adds these reasons:

| Reason | Meaning |
|---|---|
| `xr_no_loader` | The OpenXR loader is not installed (or the binding is not built, as in this version; the message then says so): XR is off, the game runs on the desktop, no session exists. |
| `xr_no_runtime` | The loader found no active OpenXR runtime. |
| `xr_extension_missing` | A required instance extension is not offered; the message names it. |
| `xr_instance_failed` | Instance creation failed for another reason (the runtime's message). |
| `xr_system_unavailable` | No headset system is available now; retried every `retry_frames` fixed steps. |
| `xr_view_config_unsupported` | The primary stereo view configuration does not report two views. |
| `xr_gpu_interop_unavailable` | The renderer's GPU device cannot be shared with the runtime (or a GPU readback failed). |
| `xr_session_exists` | A second session was requested (one per process). |
| `xr_no_swapchain_format` | The runtime offers no swapchain format the renderer supports. |
| `xr_actions_frozen` | The action sets are attached: no new action, no second attach. |
| `xr_action_unknown` | An action (or sub-path) the manifest does not declare. |
| `xr_simulator_only` | `pose`, `input`, `session_state`, `lens`, `mono` or `frame_time` on a real runtime. |
| `xr_unknown_option` | `xr.set_option` with an unknown key. |
| `xr_invalid_value` | A value of the wrong type or out of range (also config and manifest load errors, naming the field). |
| `xr_format_too_new` | A `config/xr.ron`, `actions.xr.ron` or `.xrsim.ron` newer than this build reads. |
| `xr_runtime_lost` | The runtime or device went away; the session was destroyed and is re-created after `retry_frames` (reported in `last_error`). |
| `xr_over_budget` | A diagnostic, not an error reply: 10 consecutive XR frames ran over 0.9 of the display period. Reported in `stats.over_budget` and `budget.last` of `xr.get` (and with `auto_scale` the render scale went down one step); see "Frame budget". |

Without the plugin, `xr.set_option`, `xr.recenter`, `xr.capture_eye`, `xr.layers` and `xr.set_quad`
answer `{ "refused": "xr_not_installed", "installed": false }`, like `xr.drive` without a runtime.

## Files

- `config/xr.ron` (`format_version` 1): `backend`, `required`, `tracking_space`, `render_scale`,
  `foveation`, `mirror`, `preferred_refresh_hz`, `retry_frames`, `near_m` and `comfort` (`snap_turn_deg`,
  `snap_threshold`, `snap_rearm`, `snap_cooldown_frames`, `smooth_turn_deg_per_s`, `vignette_strength`,
  `vignette_inner`, `vignette_outer`, `vignette_ease_frames`, `eye_height_m`, `dominant_hand`, `seated`).
  Every field is optional; an absent file is `backend: Auto, required: false`; an unknown field is a
  load error naming it.
- `assets/xr/actions.xr.ron` (`format_version` 1): the action manifest (above).
- `*.xrsim.ron` (`format_version` 1): `fov_deg`, `ipd_m`, `refresh_hz` and `ticks: [(tick, head, left?,
  right?, inputs: [(action, sub, Bool(..) | Float(..) | Vec2(x, y))], state?)]`; poses interpolate
  between ticks (lerp, slerp) and are exact at a tick, so a recording (`XrSession::start_recording`)
  replays bit for bit.

A newer `format_version` than this build reads is refused with `xr_format_too_new`; a v2 of any of
them will ship with a migrator in the asset migration table.

## In the editor

Project Settings → XR edits `config/xr.ron`: backend, tracking space, render scale, foveation, mirror
and comfort. The play toolbar shows the backend, the app state and the runtime name while play runs XR.
`editor.xr_settings {}` reads the file (absent: the defaults); `{ "set": { "mirror": "both" } }` writes the
named values, and a bad one is `F0002` with `data.reason` `xr_invalid_value` or `xr_unknown_option`, with
nothing written. The keys are every setting of the page: `backend`, `required`, `tracking_space`,
`render_scale`, `foveation`, `mirror`, `snap_turn_deg`, `vignette_strength`, `eye_height_m`, `seated`,
`dominant_hand` (`left` or `right`) and `retry_frames` (fixed steps, 1 to 7200); the last two since ADR-7747.

### The simulator in play

The `Simulator in play` toggle on that page, or `backend: Simulator` in the file, runs play with the
scripted runtime: no headset. The rest of `config/xr.ron` applies; `required` does not, and a broken file
leaves play running. A game that installs `XrPlugin` itself keeps its own. Play adds a rig under the
editor camera and a camera when the level has none, and takes the session to `Focused`.

The `XR simulator` window shows both eyes side by side, with the panels composed. With the pointer over
the eyes, WASD, Q and E move the head along its yaw and a right drag turns it; holding Z or X moves the left
or the right hand instead. The fields below edit the same poses. The eyes refresh every 8 play frames while
the window is open, read back to the CPU: judge the picture, not the frame rate, here.

`editor.xr_simulator` is the same window for an agent. `xr.*` methods answer on the edit world, which never
holds XR, so during play use this one:

```json
{ "enabled": true }
→ { "enabled": true, "active": false, "app_state": null, "rendered_frames": 0, "poses": null, "queued": 0, "preview": null }
```

After `editor.play`:

```json
{ "head": { "position": [0, 1.6, 2], "yaw": 0.5 }, "right": [0.3, 1.0, -0.2] }
→ { "enabled": true, "active": true, "app_state": "Focused", "rendered_frames": 12, "poses": { "head": [0, 1.7, 0], "yaw": 0, "pitch": 0, "left": [-0.2, 1.2, -0.3], "right": [0.2, 1.2, -0.3] }, "queued": 2, "preview": null }
```

`poses` is the state before the queued edits, which apply at the next frame. `{ "capture": true }` asks for
both eyes: `preview` stays null until a frame has read them, so repeat the call until it is set. It then
carries `{ frame, width, height, format: "png", eyes: [left, right] }`, base64. Bad parameters are `F0002`
with `data.reason` `xr_invalid_value`; `enabled` takes effect at the next play.

## In Rust

```rust,ignore
let mut xr = NullXr::default();
xr.simulate(SimEvent::RuntimeReady)?;
xr.begin_session()?;
let mut latch = LateLatch::default();
let report = run_frame(&mut xr, &mut latch, |timing, views| {
    // draw both eyes with views[i].view_matrix() and views[i].projection(range, DepthMode::Reversed)
})?;
```
