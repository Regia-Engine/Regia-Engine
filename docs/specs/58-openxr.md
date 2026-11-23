# Spec 58: OpenXR (`regia_xr`)

Status: proposed (ADR-1579). Target rung: after v0.4.0 (owner decision 2026-10-27: OpenXR is in scope for
"complete"). New crate `regia_xr` (the OpenXR binding, ECS plugin, rendering and input glue) on top of the built seam
`regia_platform::xr`. Handheld/camera-passthrough AR is out of scope.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline (integ/ext2, ADR-1571, implemented, spec records gaps): `regia_platform::xr` (§ 1.1).

Source area (set ids only): runtime-xr/runtime-xr-openxr, runtime-xr/runtime-xr-virtual-reality,
runtime-xr/runtime-xr-controllers-and-hand-tracking, runtime-xr/runtime-xr-overview (skim),
runtime-xr/runtime-xr-ar-sessions-and-tracking (skim, session vocabulary only).

Primary public source: the Khronos OpenXR 1.0/1.1 specification (registry.khronos.org/OpenXR/specs/1.1/),
the OpenXR loader design document, and the OpenXR Vulkan graphics binding chapter. Section references
below ("OpenXR § Session lifecycle") are to that specification.

## 1. Purpose and scope

### 1.1 Baseline: what is built (ADR-1571, `regia_platform::xr`, integ/ext2) and what this spec adds

Built, pure Rust, no dependency, tested without a headset (read the module before building; do not duplicate it):

| Built item | Notes |
|---|---|
| `SessionCore`, `SessionState {Idle, Ready, Running, Stopping}`, `Transition`, `FramePhase`, `FrameTiming`; frame-order and display-time checks | shared by every backend |
| `Pose`, `Fov` (signed half-angles, `validate`, `projection(DepthRange, DepthMode)` for reversed or standard depth, finite or infinite far), `Eye`, `View`, `stereo_views(head, ipd, fov)`, `DEFAULT_IPD_M` 0.063 | replaces the `XrView`/`XrFov`/`projection_from_fov` of an earlier draft |
| `PoseSample::extrapolate`, `LateLatch::latch_views` (late latch is implemented; § 2.4 no longer says "not required") | |
| `Hand`, `ControllerAction` (a fixed engine action list), `ActionValue`, `ControllerState`, `HapticPulse` | the action-manifest layer (§ 2.7) maps names onto this |
| `trait XrBackend: Send + Sync` (`name`, `state`, `poll_events`, `begin_session`, `request_exit`, `end_session`, `wait_frame`, `begin_frame`, `locate_views`, `head_sample`, `sync_actions`, `controller`, `apply_haptic`, `end_frame(&FrameSubmit)`, defaulted `simulate(SimEvent)`) | the one backend trait; this spec adds no second one |
| `NullXr` (deterministic scripted backend), `run_frame`, `FrameReport`, resource `XrRuntime` (not installed by any plugin) | the Simulator backend of this spec is `NullXr` plus a script player |
| `XrError` with `code()` (`xr_invalid_fov`, `xr_invalid_depth_range`, `xr_invalid_ipd`, `xr_invalid_transition`, `xr_not_running`, `xr_frame_out_of_order`, `xr_display_time_mismatch`, `xr_invalid_haptic`, `xr_unsupported`) | new errors extend this enum |
| Agent `xr.get` (read) and `xr.drive` (play), errors `F0002` with `data.reason` = `xr_*`, a missing resource answers `installed: false` / `refused: xr_not_installed`; page `book/agents/xr.md` | extended, not replaced (§ 5) |

New in this spec: the real OpenXR binding (`OpenXrBackend`); the remaining OpenXR session states (synchronized, visible,
focused, loss pending, exiting; § 2.2 adds them to `SessionState`/`Transition`, an additive-variant change listed under
Upgrade in the builder's release fragment); swapchains and the wgpu hand-off (§ 3.4); `XrPlugin` and the ECS components;
the action manifest; hand tracking; comfort; `config/xr.ron`; script player and eye capture; `xr.set_option`,
`xr.recenter`, `xr.capture_eye`.

**As built for v0.5 (ADR-1676, ADR-1918, ADR-2457, ADR-4900, ADR-5642).** XR is Simulator-only: the real binding is not
built (no `openxr` cargo feature, no `openxr_backend` module, no § 3.4 spike), so `Auto` and `OpenXr` start `Off` with
`xr_no_loader` and the message that the binding is not built (`crates/regia_xr/tests/no_loader.rs`). Everything that only
the binding can do is therefore absent: the loader, instance, system and graphics-requirement steps of § 2.3, the
swapchain format choice of § 2.6, the view-count check of § 2.11, the foveation, refresh-rate, depth-info and debug
extensions (their options are stored and reported), and Android loader bundling. The `XrError` variants and reason codes
for those steps (`xr_no_runtime`, `xr_extension_missing`, `xr_instance_failed`, `xr_system_unavailable`,
`xr_view_config_unsupported`, `xr_gpu_interop_unavailable` outside the renderer, `xr_session_exists`,
`xr_no_swapchain_format`) exist and are documented, but no built path raises them yet. The frame loop runs on the main
thread (`Last` without a renderer, `Render` with `XrRenderPlugin`); the dedicated XR frame thread of § 2.4 comes with the
binding, and the mailbox plus `XrSession::frame_paused` model it for acceptance 8.4. The extra errors live in
`regia_platform::xr::XrError` (no `regia_xr::error` module), and the crate also has `state`, `mailbox`, `stereo`,
`render`, `walk`, `perf`, `formats` and `budget` modules beyond § 3.1's list. The § 10 over-budget diagnostic
`xr_over_budget` and `auto_scale` are built (post-v0.5, `regia_xr::budget`; see the § 10 as-built note).

**Problem.** Head-mounted displays are reached through one open standard. Regia needs a single, optional
backend that opens an XR runtime, negotiates the graphics device with it, runs the frame loop, renders two
views per frame, reads head and controller poses, and maps action-based input onto the existing Regia input
layer, on Linux desktop, Windows and Android aarch64 (standalone headsets), without forking the renderer or
the sim.

**In scope.** Instance and session lifecycle; runtime selection; wgpu/Vulkan interop requirement and a
feasibility spike; stereo rendering; frame timing and pose prediction; tracking spaces; depth/projection layer
submission; action-based input, haptics, optional hand tracking; comfort settings; a desktop **simulator
backend** so CI runs with no hardware and no GPU; agent surface; budgets.

**Not.**
- Not AR (passthrough, planes, anchors, image/face/body tracking) and not alpha-blend/additive blend modes
  beyond reading what the runtime reports (§ 11).
- Not a VR game template. Locomotion, grabbing and menus are game code; this spec ships comfort *primitives*
  (snap turn, vignette) and a spatial UI layer hook, not a gameplay kit.
- Not multiplayer or avatar replication.
- Not a vendor SDK path. Only the open standard is used; vendor-specific features arrive as optional,
  runtime-probed extensions (§ 2.9) and never change the core path.
- Not a promise that wgpu can host an XR device today. § 3.4 states the requirement; feasibility is **open**.

## 2. Regia terms and model

### 2.1 Nouns

| Term | Meaning |
|---|---|
| **XrBackend** | The built trait of `regia_platform::xr` (§ 1.1). Implementations: `OpenXrBackend` (real runtime, this spec) and `NullXr` (the Simulator). Exactly one is active per run. Swapchain images are not part of the trait (layer 0b cannot name wgpu): they come through `EyeTargets` (§ 3.2). |
| **XrRuntime** | The installed OpenXR runtime the loader selects (not a Regia concept beyond selection and reporting). |
| **XrSystem** | One head-mounted display system reported by the runtime (form factor: head-mounted only). |
| **XrSession** | The live connection that owns swapchains and the frame loop. |
| **View** | The built `regia_platform::xr::View`: one eye's pose and `Fov` for a predicted display time. Stereo = 2 views. The recommended per-eye image size is reported separately (`EyeTargets::extent`). |
| **TrackingSpace** | The reference frame poses are expressed in: `Local` (seated, origin at start head position), `Stage` (room-scale, origin on floor in the play area), `LocalFloor` (origin on the floor under the start position, if the runtime offers it; else emulated from `Local` plus a configured eye height). |
| **XrRig** | A component marking an entity that is the tracking-space origin in the world. The head, hands and views are child entities in tracking space. Moving the rig moves the player (locomotion = moving the rig). |
| **XrActionManifest** | Project asset declaring action sets, actions and per-interaction-profile suggested bindings (§ 4.2). |
| **ComfortSettings** | Snap turn, vignette, and height/handedness options (§ 2.8). |

### 2.2 App states and OpenXR session states

The session state is driven by runtime events; Regia maps it to a small app state exposed as the resource
`XrState` (and `xr.get`). The OpenXR states are the standard's (OpenXR § Session lifecycle); the app
state is Regia's.

| OpenXR session state | Frame loop | Regia `XrAppState` | What the engine does |
|---|---|---|---|
| (no instance) | no | `Off` | XR disabled or no runtime; desktop game runs normally. |
| instance created, session not created | no | `NoSession` | System queried; waiting for headset (system may be "unavailable"; retry on a slow timer driven by frame count, § 2.3). |
| `IDLE` | no | `Idle` | Session exists; wait for `READY`. |
| `READY` | begin session | `Starting` | Call begin-session with the primary stereo view configuration; on success the loop starts. |
| `SYNCHRONIZED` | yes (wait/begin/end) | `Running` (hidden) | Submit frames with **zero layers** is allowed; do not render eye views. Sim continues. |
| `VISIBLE` | yes | `Running` (visible) | Render and submit layers. Input still reads inactive. |
| `FOCUSED` | yes | `Focused` | Render, submit, and input is live. Controllers and hand poses valid. |
| (backend stays in `running`) | yes | `Running` | No synchronized, visible or focused report. Input is live, the same as `Focused`. |
| `STOPPING` | stop | `Stopping` | Finish or drop the current frame, call end-session, destroy swapchains' use; return to `Idle`. |
| `LOSS_PENDING` | stop | `LossPending` | Runtime or device going away: end the session, destroy the session and instance, return to `NoSession` and re-create after the retry delay. Never crash. |
| `EXITING` | no | `Exiting` | The user asked to leave: request a normal app exit (same path as a window close), destroy everything. |

Mapping to the built types (§ 1.1): the table's `Idle`, `Ready`(`Starting`), `Running` and `Stopping` rows are the built
`SessionState` values; `Synchronized`, `Visible`, `Focused`, `LossPending` and `Exiting` are added to
`SessionState` (and matching `Transition` variants) in `regia_platform::xr`, with `SessionCore` allowing
exactly the transitions the table lists. `XrAppState` is Regia's coarser app-facing state derived from them and
from "is there an instance" (`Off`, `NoSession`). `XrEvent` gains `FocusChanged { focused }`.

Rules:
1. Every state change arrives as an event drained at the start of the frame by `XrPlugin` (poll until the
   runtime reports none). The app state changes only from events; nothing guesses.
2. `Focused` ↔ `Running (visible)` changes **pause nothing in the sim**; they only gate input validity and
   a `XrFocusChanged { focused }` event. Game code decides whether to pause (a game may pause on focus loss).
3. A transition into `Starting` resets frame-order bookkeeping (§ 2.4).
4. `LossPending` and `Stopping` both end by returning to a state from which re-creation is legal; the
   instance is rebuilt only from `LossPending` or from user action (`xr.set_option enabled`).
5. A session for a system unsupported or unavailable at create time yields `NoSession` plus the reason in
   `xr.get`; it is **never** a startup error unless `config `required: true`.

### 2.3 Instance and runtime selection

Order of operations (each failure maps to an error code, § 5.3):
1. **Load**. Load the OpenXR loader dynamically (`libopenxr_loader.so` / `openxr_loader.dll`); if it is not
   present, `XrAppState::Off` with reason `xr_no_loader`. Never link the loader statically on desktop (the game
   must start without it installed). On Android the loader is bundled (§ 9).
2. **Enumerate** API layers and extensions the loader reports. Layers are never required; extensions are
   split into **required** and **optional** lists (§ 2.9). The advertised lists can change between
   enumerations; enumerate once per instance create and trust only that result.
3. **Create the instance** with the application name (non-empty), the Regia engine name/version, and the
   union of required extensions plus whichever optional extensions were advertised. Creation is atomic:
   one missing required extension fails the whole call and no instance exists. Retry without optional
   extensions is unnecessary because only advertised optionals are requested.
4. **Select the runtime**. Regia does not ship a runtime and does not implement runtime discovery; the
   loader resolves the active runtime. Regia supports overrides only through the loader's documented
   mechanisms: the runtime-manifest environment override, the platform's active-runtime setting (Linux
   config file, Windows registry), and on Android the system broker. `xr.get` reports the runtime name and
   version string and the OpenXR API version it implements; `xr.set_option runtime_json` documents (but does
   not itself set) the override: it is applied by setting the environment before the next instance create
   (desktop only, dev builds only).
5. **Get the system** for head-mounted-display form factor. "Form factor unavailable" is not an error: stay in
   `NoSession`, re-try every `retry_frames` (default 120 sim frames, counter-driven, no wall clock).
6. **Graphics requirements.** Query the Vulkan graphics requirements for the instance and system *before*
   creating a session (the session create fails otherwise). They give the minimum/maximum Vulkan API
   version and the physical device the runtime requires (§ 3.4).
7. **Create the session** with the graphics binding; expect `IDLE` first.
8. Check API version: require OpenXR 1.0; features of 1.1 are used only if the reported version is ≥ 1.1.

### 2.4 Frame loop contract

Each XR frame, in this fixed order (OpenXR § Frame synchronization):
`wait_frame` → `begin_frame` → (acquire per-eye swapchain image, wait image, render, release) → `end_frame`
with the layer list and the **predicted display time** that `wait_frame` returned.

- `wait_frame` paces the app; it returns the predicted display time, predicted display period and whether
  the app should render (`should_render`). When `should_render` is false (hidden), begin/end are still
  called; `end_frame` is called with an empty layer list (an empty list clears the display).
- `end_frame` without a successful `begin_frame` is an error in the runtime; the wrapper tracks the order and
  never emits an out-of-order call (it returns `XrError::FrameOutOfOrder` to the *caller* instead).
- A dropped frame (renderer missed the budget) still calls `end_frame` for that display time with the last
  valid layers or empty layers; it never skips `end_frame` after a successful `begin_frame`.
- **The renderer must not block the sim.** `wait_frame` runs on a dedicated **XR frame thread**, never on the
  app-update thread. The sim keeps its fixed step (`specs/10`). Per frame the thread publishes the latest
  `XrFrameState { predicted_display_time, period, should_render }` into a lock-free single-slot mailbox; the
  render world extraction reads the freshest slot without waiting. If no new slot arrived since the last
  extraction the renderer repeats the old frame's layers (the runtime reprojects) and increments
  `xr.stale_frames`.
- Pose queries use the *predicted display time*, not "now". The head and controller transforms the renderer
  uses are located at that time per frame; game-logic reads (what the sim sees) use the pose **sampled at
  the start of the fixed step** and held for the step so logic is deterministic within a step.
- Late latching of the *CPU-side view poses just before submit* is built (`LateLatch`); the render path calls
  `latch_views` once per frame. Updating poses inside the GPU command buffer after submission stays out of scope.

### 2.5 Tracking-space and origin model

- `XrRig` entity world transform = tracking-space origin in the world (Y up, −Z forward, metres: the engine's
  standard conventions, `04-conventions`). OpenXR is also right-handed, Y-up, −Z forward, metres; no basis
  change is needed.
- `TrackingSpace` is chosen at config time and can change at runtime (`xr.set_option tracking_space`).
  Changing it re-creates the reference space, keeps the rig entity, and sends `XrOriginChanged`. Fallback
  order on request `Stage`: Stage → LocalFloor → Local; the actual space is reported in `xr.get`.
- `recenter()` sets the rig-local offset so the user's current head yaw/position lines up with the rig's
  forward; it sends `XrOriginChanged` and also fires when the runtime reports a reference-space change
  event (user recenter from the system menu), carrying the new pose offset.
- Play area bounds (stage): width × depth in metres when the runtime provides them; absent bounds are
  reported as `None` (some runtimes do not implement them).
- Local floor height for `Local` is `eye_height_m` (default 1.65) when the user wants a standing floor
  emulation; applied only to the rig offset, never to views.

### 2.6 Stereo rendering

- **Two views** per frame from the located view-configuration (left, right). Each carries pose in tracking
  space and a field of view as four angles (left, right, up, down; may be asymmetric). Per-eye projection is
  `DepthMode::Standard` with near from config (default 0.05 m) and far `INFINITE_FAR_STANDIN` (100_000), or
  twice the near plane when that is larger (ADR-869, ADR-1676). The renderer never assumes a symmetric
  frustum or a fixed IPD; the eye offset comes from the view poses.
- **Rendering strategies**: (a) **multiview**: one render pass over a 2-layer array target, per-view
  matrices in a uniform array indexed by the view index; (b) **two-pass**: render the whole scene twice
  into two single-layer targets or two sub-rects of one wide target. Multiview is preferred when the wgpu
  device exposes it (spike § 3.4); two-pass is mandatory as a fallback and as the Simulator's baseline.
  Both must produce the same image (acceptance 8.12). As built (ADR-1918): two-pass is the whole frame per eye; multiview is
  a reduced forward set that also runs one pass per layer, and the parity holds between those two strategies. Shared across both: culling is done **once** with a
  conservative frustum that is the union of both eye frusta (plus a small epsilon); eye-dependent work
  (view-dependent shading, TAA history, screen-space effects) runs per eye with per-eye history buffers.
- **Unsupported-in-XR effects** are disabled in XR mode with a diagnostic, listed per feature (e.g. SSR
  that assumes one camera, single-view temporal accumulation tied to a mono history): the renderer reads
  `XrRenderProfile` (§ 3.3) to pick the XR-safe pass set. Post-processing runs per eye after the shared
  scene passes. The intent is a fixed **XR quality profile** rather than a full-feature parity promise.
- **Render scale**: a multiplier on the runtime-recommended per-eye image size (default 1.0, range
  0.5-2.0), changed live; swapchains are re-created at a frame boundary (never mid-frame). Dynamic scale
  (a render-scale controller driven by frame time) is an optional follow-up; the hook is `render_scale`.
- **Foveation hints**: if a foveation extension is advertised (optional, § 2.9), `foveation: Off|Low|Medium|High`
  applies the runtime's fixed foveation profile on swapchains. Eye-tracked foveation is an optional extra.
  Hints are best-effort and never required for correctness; absent extension means `Off` and a diagnostic.
- **Colour space and format**: swapchain format chosen from the runtime's list in preference order
  `Rgba8Srgb`, `Bgra8Srgb`, `Rgba16Float`; the first the renderer supports wins. The renderer writes
  display-referred sRGB values for 8-bit sRGB formats (the format does the gamma encode). HDR formats
  are accepted only if the tonemap path is configured for linear output.
- **Depth layer submission**: the renderer also submits the depth buffer through the depth-info layer
  extension (optional) so the runtime can do positional reprojection; near/far values match the
  projection. Not advertised → colour only and a diagnostic.
- **Layers**: the frame submits a single projection layer (two views, each with its sub-image rect and pose
  used for rendering, so the runtime reprojects from the pose rendered, not the pose now). Optionally also:
  quad layers for HUD/menu panels (§ 2.10). A hidden-area mesh (the region not visible in the lens) is
  used to stencil-mask pixels when advertised.
- Mirror/spectator window: the desktop window shows eye 0 (left), eye 1, or a cropped-to-fill combination
  (`mirror: Off|Left|Right|Both`); Android has no mirror. Mirror cost counts against the frame budget.

### 2.7 Action-based input

OpenXR input is action-based (OpenXR § Input and haptics): the app declares *actions*, suggests *bindings*
per *interaction profile*, attaches the sets once, and the runtime/user can remap.

- **XrActionManifest** declares action sets (`gameplay`, `ui`), each with actions of types: `Bool`, `Float`,
  `Vec2`, `Pose` (a space, e.g. grip, aim), `Haptic` (output). Each action has a stable name (lowercase ASCII
  with `_`), a localized label key, and optional sub-action paths (`left`, `right`).
- **Bindings** list for each interaction profile (simple controller, a generic two-hand controller profile,
  per-vendor profiles) which input path feeds which action. Regia ships a default manifest covering: move
  (Vec2), turn (Vec2), select/trigger (Float), grab/squeeze (Float), primary/secondary button (Bool), menu
  (Bool), grip pose, aim pose, haptic. Projects override the manifest; vendor profile extensions are
  listed in the manifest as optional.
- **Freeze rule.** Action sets are created and bindings suggested first; they are attached **exactly once**
  per session; creating an action in an attached set or attaching twice is an error (`XrError::ActionsFrozen` (`xr_actions_frozen`)).
  Changing the manifest at runtime therefore re-creates the session's action state (not the instance).
- **Sync.** Once per sim frame (at the start of the fixed step) the plugin calls sync for the active sets and
  reads all states. Input is live in `Focused`, and in `Running` when the backend has no finer focus
  state. `Visible` stays inactive: every action reads inactive and its value is the default. Two reads
  between syncs return the same value (the plugin never re-reads mid-frame).
- **Mapping to `regia_input`.** Each action state is published to `regia_input` as an `InputAction`
  (existing input-map layer, `specs/11`): Bool→button, Float→axis, Vec2→2D axis, Pose→an `ActionPoses`
  resource entry keyed by action and sub-path (the built `Pose`). Existing input-binding code works unchanged for XR actions.
- **Haptics.** `xr_haptic(action, sub_path, amplitude 0..1, duration_ns, frequency_hz)` emits a vibration;
  `xr_haptic_stop`. Amplitude clamps to [0,1]. A runtime without haptics on that device returns a soft
  no-op with a diagnostic, not an error.
- **Hand tracking (optional).** If the hand-tracking extension is advertised, 26 joints per hand are
  located each frame at the predicted display time: pose, radius, validity flags (position valid,
  orientation valid, tracked vs inferred). Exposed as `XrHands` resource. Never required; an absent
  extension yields `hand_tracking: unsupported` in `xr.get`. A hand-mesh/skin extension and gesture
  interpretation (pinch strength etc.) are follow-ups.
- Eye tracking is out of scope except that foveation may use it if the runtime does it itself.
- Controller records latch: a pose that has never arrived reads invalid; after the first valid pose it
  stays "ever valid" while the current-frame flag is separate.

### 2.8 Comfort settings

`ComfortSettings` (resource and part of `xr.ron`):
- **Snap turn**: `snap_turn_deg` (default 30, 0 = smooth turn), applied by rotating the **rig** about the
  head's vertical axis at the head position so the head does not translate; one snap per stick deflection
  past a threshold (0.6) and a re-arm below 0.3; cooldown `snap_cooldown_frames` (default 8).
- **Vignette**: a post-process that darkens the periphery proportional to the rig's artificial
  angular and linear speed (`vignette_strength` 0..1, `vignette_inner`, `vignette_outer` in radians of FOV),
  eased by a time constant in sim frames; it never rises when the user moves their own head (only rig
  motion drives it). It is per-eye, resolved after tonemap.
- **Height**: `eye_height_m` as in § 2.5; **dominant hand**: swaps `left`/`right` default bindings.
- **Seated mode**: tracking space `Local` + eye-height offset.
- Teleport/comfort locomotion helpers are not in this spec; `rig_move`, `rig_turn` utilities are.

### 2.9 Extension policy

| Extension capability | Class | Used for |
|---|---|---|
| Vulkan graphics binding (`XR_KHR_vulkan_enable2`) | required (desktop and Android) | device negotiation (§ 3.4) |
| Android instance creation / loader init | required on Android | bring-up (§ 9) |
| Depth layer submission | optional | positional reprojection |
| Composition-layer colour scale/bias | optional | not used v1 |
| Foveation | optional | § 2.6 |
| Hand tracking | optional | § 2.7 |
| Display-refresh-rate control | optional | request 72/90/120 Hz |
| Visibility mask (hidden area) | optional | stencil mask |
| Interaction-profile extensions (vendor controllers) | optional | bindings |
| Debug utils messenger | optional (dev builds) | forwards runtime messages to `tracing` |

Behavior is always probed from the *enabled* list, never assumed. An optional capability that is advertised
but later errors at use is disabled for the rest of the session with a diagnostic.

### 2.10 Spatial UI

`regia_ui` world-space panels render to a texture and are submitted as **quad layers** (compositor
re-projects them; they skip post-processing and stay sharp). A panel is placed head-locked, rig-locked or
world-locked; with equal priority, order is unspecified, so panels carry an explicit `priority`. Panels over the
layer budget are drawn into the eye images, under the composed ones and without a depth test (ADR-1918). Details
are a follow-up (§ 11).

### 2.11 Limits and edge cases

- Max 1 `XrSession` per process (a second create returns `xr_session_exists`). Max 2 views (stereo); quad-view
  (4 views) is out of scope.
- `should_render == false`: no eye render, still end_frame.
- Headset removed while running: runtime sends `LOSS_PENDING` or goes to `IDLE`/`STOPPING`; handled by the table.
- User takes headset off (proximity): usually `VISIBLE`/`SYNCHRONIZED` without focus; input reads inactive.
- Non-finite pose from a runtime is rejected and the previous pose kept; counted in `xr.bad_poses`.
- Runtime reports a different view count than 2 for the primary stereo configuration: `xr_view_config_unsupported`.
- Swapchain image format list empty or none supported: `xr_no_swapchain_format`.
- Instance creation succeeded but GPU interop failed (§ 3.4): `xr_gpu_interop_unavailable`, state `NoSession`;
  the desktop game keeps running.

## 3. API shape in Rust

### 3.1 Crate and layering

New crate **`regia_xr`**, Layer 2 (systems). Depends on `regia_core`, `regia_ecs`, `regia_app`, `regia_platform`
(which owns the built `xr` module, layer 0b), `regia_input` (layer 1) and `regia_render` (the one allowed
system-to-system edge, as `regia_ui` and `regia_vfx`). `regia_agent` (layer 3) already carries the built `xr.get` /
`xr.drive` handlers over `regia_platform::xr` only; the new methods (§ 5) read `regia_xr` types behind its optional
`xr` dependency (a downward edge; add `regia_agent -> regia_xr` to the `xtask` layer table). Nothing in layers 0-1
depends on `regia_xr`. It sits behind the cargo feature `xr` on the `regia` umbrella; off by default; compiled out of
shipping builds that do not opt in. The Simulator needs no GPU, loader or dependency. The OpenXR binding sits behind a
second feature, `openxr` (off by default, desktop and Android), so the plugin, manifest, comfort and simulator code build
and test everywhere without the dependency.

`regia_xr` modules: `plugin`, `openxr_backend` (feature `openxr`), `eye_targets`, `space`, `input` (action manifest,
mapping to `regia_input`), `hands`, `comfort`, `config`, `sim` (script player over `NullXr`), `error` (extra variants).

### 3.2 Types (only what is not built; the built types are in § 1.1)

```rust
pub struct XrPlugin { pub config: XrConfig }              // impl Plugin; installs XrRuntime from the chosen backend
pub enum XrBackendKind { Auto, OpenXr, Simulator, Off }

#[derive(Resource, Reflect, Serialize, Deserialize)]
pub struct XrConfig {
    pub format_version: u32,
    pub backend: XrBackendKind,
    pub required: bool,            // true = app refuses to start without a session
    pub tracking_space: TrackingSpace,
    pub render_scale: f32,
    pub foveation: Foveation,
    pub mirror: Mirror,
    pub comfort: ComfortSettings,
    pub preferred_refresh_hz: Option<f32>,
    pub retry_frames: u32,
}

#[derive(Resource)] pub struct XrState { pub app: XrAppState, pub session: SessionState, /* ... */ }
pub enum XrAppState { Off, NoSession, Idle, Starting, Running, Focused, Stopping, LossPending, Exiting }

#[derive(Component)] pub struct XrRig;
#[derive(Component)] pub struct XrHead;                   // child, local pose = rig-local head (offset composed with the tracking pose)
#[derive(Component)] pub struct XrController { pub hand: Hand }
#[derive(Component)] pub struct XrEye { pub index: u8 }   // camera child per eye

/// Where eye images are rendered. Lives in `regia_xr` because it names wgpu (layer 2).
pub trait EyeTargets: Send {
    fn extent(&self) -> UVec2;                                         // recommended per-eye size x render scale
    fn acquire(&mut self, eye: Eye) -> Result<wgpu::TextureView, XrError>;   // wait-image included
    fn release(&mut self, eye: Eye) -> Result<(), XrError>;
}
```

`OpenXrBackend` implements the built `XrBackend` and also yields its `EyeTargets` (swapchain images wrapped per
§ 3.4). The Simulator pairs `NullXr` with `SimEyeTargets` (ordinary wgpu textures). Extra errors are new
`XrError` variants with `xr_`-prefixed codes (§ 5.3). Poses and the `ControllerAction` list stay the built ones; a
manifest action (§ 2.7) whose name is not a built `ControllerAction` is published to `regia_input` only. The free
functions of the earlier draft (`projection_from_fov`, `xr_haptic`, `recenter`) are the built `Fov::projection`,
`XrBackend::apply_haptic` and a `regia_xr::recenter(world)` helper.

All fallible calls return `Result`; no `unwrap`/`expect` on runtime-dependent paths.

### 3.3 Events, resources, systems

Events (ECS messages): `XrStateChanged { from, to }`, `XrFocusChanged { focused }`, `XrOriginChanged
{ space, offset }`, `XrRuntimeLost`, `XrInteractionProfileChanged { hand, profile }`, `XrRefreshRateChanged
{ hz }`. Resources: `XrState`, `XrFrameMailbox`, `ActionPoses`/`XrHands` snapshots, `XrRenderProfile` (view count,
multiview or not, per-eye extent). System sets in order: `XrSet::Poll` (events) → `XrSet::SyncInput`
(fixed-step start) → `XrSet::UpdateRig` (head/controller transforms from held poses) → game systems →
`XrSet::Extract` (frame mailbox read, view matrices into the render world) → render → `XrSet::Submit`.
All items `Reflect` where data-only; `XrConfig` and the manifest derive `Serialize`/`Deserialize`.

### 3.4 wgpu/Vulkan interop: the requirement and the open question

**Requirement (from the OpenXR Vulkan binding).** OpenXR Vulkan sessions require that: (1) the runtime be
asked which `VkPhysicalDevice` it wants for the system, (2) the `VkInstance` and `VkDevice` be created **with
the runtime's participation** (the runtime may need extra instance/device extensions and may create the
instance/device itself through a runtime-provided create-instance call), (3) the session be created with
the resulting handles plus the queue family and queue index, and (4) swapchain images come back as raw
`VkImage` handles the app must wrap. A Regia renderer that creates its wgpu instance/adapter first, as
it does today, cannot satisfy (1)/(2).

**Candidate approach (unproven).** wgpu's Vulkan hal layer allows (a) creating a wgpu instance *from* an
existing raw Vulkan instance, (b) opening a logical device on an adapter *from* a raw device with a
caller-chosen extension list, and (c) wrapping a raw `VkImage` as a wgpu texture. The XR backend would
therefore: create the Vulkan instance and device through the runtime's negotiated calls, hand those
raw handles to wgpu (hal), build `wgpu::Device`/`Queue` over them, wrap each swapchain image as a texture
view, and render with ordinary wgpu passes. The same device is also used by the desktop mirror window's
surface, which requires the window's surface to be creatable on that instance (display-surface extensions
included in the negotiated list).

**What the pinned release exposes (checked against the published `wgpu` 30.0.1 and `wgpu-hal` 30.0.1 sources, MIT OR
Apache-2.0, public registry crates).** All three entry points exist without patches: `wgpu::Instance::from_hal` (wraps a
`wgpu_hal::vulkan::Instance`, itself built with `vulkan::Instance::from_raw(entry, raw_instance, api_version, ...)`),
`wgpu::Adapter::create_device_from_hal` (with `vulkan::Adapter::device_from_raw`, which takes the enabled extension
list and requires it to be a superset of `required_device_extensions(features)`), and
`wgpu::Device::create_texture_from_hal` (with `vulkan::Device::texture_from_raw`). So the expected spike result is GO;
what remains unproven is below.

**Open questions the spike must answer (feasibility: likely GO, unproven).**
- Can the extension list the XR runtime demands coexist with the features Regia already requires on its Vulkan device
  (multiview, timestamp queries, push constants, bindless-style arrays)? `required_device_extensions` is the union to build.
- Does importing a raw device keep wgpu's internal queue-family assumptions valid (one graphics queue)?
- On Android, does the surface/swapchain path for the non-XR game still work when the device is created by the XR runtime?
- Decision (ADR-1585): device creation is **deferred until after the XR runtime is queried**; `regia_render` gets an init
  option that accepts a pre-made `(wgpu::Instance, Adapter, Device, Queue)` (an injection point, not a fork). The default
  path (the engine picks its own adapter) is unchanged when XR is off.

**Spike (gate for the whole spec), deliverable before any other XR work, headless-capable, 1 builder-day-class:**
1. A test binary in `regia_xr/spikes/` creates an OpenXR instance (with the `xr` feature) against **a
   software/mock runtime** (a conformance or null runtime installed by the CI image, or the Simulator
   with a raw-Vulkan device) and queries Vulkan graphics requirements.
2. Create the Vulkan instance/device by the negotiated path; import into wgpu; clear a 2-layer target,
   read back; compare bytes.
3. Wrap an externally allocated `VkImage` as a texture and render to it.
4. Record in `docs/decisions.md` an ADR: GO (hal import works as-is), GO-WITH-PATCH (a small upstream or
   local patch; license/PR path stated), or NO-GO (fallback: the raw-Vulkan XR present path, i.e.
   render into wgpu-owned textures and **copy** into runtime swapchain images through a thin raw Vulkan
   shim; costs one extra copy per eye and is accepted only if it meets § 10).
Acceptance: either GO/GO-WITH-PATCH with step 2 and 3 passing under lavapipe or a real GPU, or NO-GO with
the copy-path prototype passing step 2. **Until this ADR exists nothing else in this spec is marked ready
to build** beyond the Simulator backend and the pure-logic items (§ 8.1-8.10).

**Fallback for tests.** The Simulator backend allocates ordinary wgpu textures as "swapchain images", so
the renderer's two-eye path is tested with no XR device at all.

## 4. Files and formats

### 4.1 Project config: `config/xr.ron` (`format_version: 1`)

```ron
(format_version: 1, backend: Auto, required: false, tracking_space: Stage,
 render_scale: 1.0, foveation: Off, mirror: Left, preferred_refresh_hz: Some(90.0),
 retry_frames: 120,
 comfort: (snap_turn_deg: 30.0, vignette_strength: 0.5, eye_height_m: 1.65, dominant_hand: Right))
```
Absent file = `backend: Auto, required: false`. Unknown fields are a load error naming the field.

### 4.2 Action manifest: `assets/xr/actions.xr.ron` (`format_version: 1`)

```ron
(format_version: 1,
 sets: [(name: "gameplay", priority: 0, actions: [
   (name: "move", ty: Vec2, label: "xr-action-move", subs: ["left"]),
   (name: "grab", ty: Float, label: "xr-action-grab", subs: ["left","right"]),
   (name: "grip_pose", ty: Pose, subs: ["left","right"]), (name: "buzz", ty: Haptic, subs: ["left","right"])])],
 bindings: [(profile: "simple_controller", map: [("grab", "left", "select/click")])])
```
Input paths are the standard's strings (e.g. `/user/hand/left/input/trigger/value`) stored as text; the
profile names are the standard's interaction profile paths. Validation at load: names are
`[a-z][a-z0-9_]*`, unique per set; every binding names an existing action and sub-path.

### 4.3 Simulator script: `*.xrsim.ron` (`format_version: 1`)

A deterministic recording/script: `ticks: [(tick: 0, head: pose, left: pose?, right: pose?, inputs:
[(action, sub, value)], state: Option<SessionState>)]` plus `fov_deg`, `ipd_m` (default 0.063), `refresh_hz`.
Poses are position (m) + quaternion; between ticks poses interpolate (lerp/slerp) at the sim step.

### 4.4 Migration
`format_version` is read first. A v2 of any of these files ships with a migrator registered in the asset
migration table (`specs/12` § 4a) and is applied by `regia upgrade-project`; a loader seeing a newer
version than it knows refuses with `xr_format_too_new`. No cook step: the files are copied to the pak as
data. No change to frozen content formats.

## 5. Agent-protocol surface

Guide page: `book/agents/xr.md`. Methods (JSON-RPC, dotted), error codes lowercase ascii/digits/`_`.
All methods degrade gracefully (the built `installed: false` / `refused: xr_not_installed` answer) without the `xr` feature or plugin.

### 5.1 Methods

Built (ADR-1571, implemented): `xr.get` (read; the status snapshot) and `xr.drive` (play; scripted actions on the
backend). Extended here, not renamed: `xr.get` gains `backend`, `app_state`, `runtime`, `system`, `tracking_space`,
`actual_tracking_space`, `refresh_hz`, `render_scale`, `extensions_enabled`, `hand_tracking`, `play_area`, `stats`
(`frames`, `stale_frames`, `bad_poses`, `dropped_frames`) and `last_error` as additional fields; `xr.drive` gains the
actions `pose` (`head?`, `left?`, `right?`, `fov_deg?`, `ipd_m?`; becomes `SimEvent::HeadPose` plus controller state),
`input` (`action`, `sub?`, `value`) and `session_state` (`ready|synchronized|visible|focused|stopping|loss_pending|exiting`)
on a scripted backend; on a real backend they answer `refused: xr_simulator_only`. New methods:

| Method | Access | Params | Result |
|---|---|---|---|
| `xr.set_option` | play | `{ key, value }`; keys: `enabled`, `tracking_space`, `render_scale`, `foveation`, `mirror`, `snap_turn_deg`, `vignette_strength`, `eye_height_m`, `refresh_hz`, `runtime_json` (dev, desktop) | `{ key, old, new, applied_at: "now"/"next_frame"/"next_session" }` |
| `xr.recenter` | play | `{}` | `{ offset }` |
| `xr.capture_eye` | read | `{ eye: 0\|1\|"both", source?: "eye"\|"display"\|"mirror", width?, height? }` | image result (same shape as `capture.screenshot`): per-eye PNG path/base64 + the view's pose/fov; works with both backends |
| `xr.layers` | read | `{}` | the last frame's layers and the runtime's budget (ADR-1918) |
| `xr.set_quad` | play | `{ id, remove?, space?, position?, yaw?, size_m?, priority?, color? }` | `{ id, count }` or `{ id, removed }` (ADR-1918) |

`xr.set_option` rules: unknown key -> `xr_unknown_option`; wrong type or out of range -> `xr_invalid_value` (range in the
message); `enabled: false` tears the session down, `true` begins the instance sequence. `xr.capture_eye` reads back the
*eye render target* (not the mirror), at the rendered size or an explicit size (resized by a box filter). A capture while
`app_state` is `Off|NoSession|Idle` returns `xr_not_running`.

### 5.2 Verification by an agent
`wait_until { expr: "xr.app_state == 'Focused'" }` then `xr.capture_eye`; `expect` on `xr.get` fields and
head/controller transforms through the existing world-query methods (`XrHead`, `XrController` are plain
components). A typical test: start Simulator, `xr.drive session_state focused`, `xr.drive pose`, step N
fixed frames, `xr.capture_eye both`, assert left and right images differ by the expected horizontal parallax
(feature-position delta ≈ `ipd * focal / depth` within tolerance).

### 5.3 Error codes
Agent errors follow the built module: code `F0002` with `data.reason` set to one of these lowercase values; the
built `XrError::code()` values (§ 1.1) are unchanged. New `reason`s (and matching `XrError` variants): `xr_no_loader`,
`xr_no_runtime`, `xr_extension_missing` (names which), `xr_instance_failed`, `xr_system_unavailable`,
`xr_view_config_unsupported`, `xr_gpu_interop_unavailable`, `xr_session_exists`, `xr_no_swapchain_format`,
`xr_actions_frozen`, `xr_action_unknown`, `xr_simulator_only`, `xr_unknown_option`, `xr_invalid_value`,
`xr_format_too_new`, `xr_runtime_lost`. Earlier-draft names map to built ones: `frame_order` is
`xr_frame_out_of_order`, `session_not_running` / `not_running` is `xr_not_running`, `xr_unavailable` is the built
`installed: false` / `refused: xr_not_installed` answer. Each reason has a long form in `book/agents/xr.md`
(extend the built page; do not create a second page).

### 5.4 Coverage
`book/COVERAGE.md` already has rows for `xr.get` and `xr.drive` (built). Add one row each, in the built format
`| xr.set_option | xr_set_option | [xr](agents/xr.md) |`, for `xr.set_option`, `xr.recenter` and `xr.capture_eye`. Each
needs a schema row and contract per `cargo xtask agent-coverage`.

## 6. Editor surface

Editor is desktop-only and optional: a **Project settings → XR** page (edits `config/xr.ron`), a small
**XR** status strip in the play toolbar (backend, app state, runtime name), and a **Simulator** toggle that
runs PIE with the Simulator backend: the viewport shows both eye previews side by side and an inspector
for head/hand poses with mouse-and-key drag (WASD + right-drag for head, held key for each hand). The
editor never needs a headset for PIE, and `regia_editor` is not a dependency of `regia_xr`. As built (ADR-1940): the
previews and the inspector are one floating `XR simulator` window over the dock, held keys Z and X pick a hand, and
`editor.xr_simulator` is its agent surface (the `xr.*` methods answer on the edit world, which holds no XR).

## 7. Determinism notes

- The Simulator is **bit-exact**: poses from the script at fixed ticks, interpolation in `f32` with fixed
  operation order, no wall-clock input; same script + seed = same `XrHead/XrController` transforms every run.
- The fixed step samples the held pose once; logic that reads poses is deterministic given the pose stream.
- Real runtimes are inherently non-deterministic (prediction, timing); a **record mode** writes the sampled
  pose stream per fixed tick to a `.xrsim.ron` so the session can be replayed in the Simulator.
- `wait_frame` timing never enters the sim; the sim sees only the mailbox (counter `xr.stale_frames`).
- Rendered eye images are tolerance-based on GPU (per the renderer's golden-image tolerance); view matrices,
  projection matrices and action mapping are bit-exact (CPU math).
- Ordering is fixed by `XrSet`; event handling order = runtime event order.

## 8. Tests and acceptance

"Sim" = Simulator backend, no GPU, no hardware. "GPU" = needs a GPU (software Vulkan is fine unless noted).
"HW" = needs a real headset: **the owner has no confirmed headset; all HW items are unverified until one is
available and must not be claimed as passing.**

1. (Sim) Lifecycle table: for each row of § 2.2, `xr.drive session_state` produces the listed `XrAppState`
   and `XrStateChanged`; `Starting` resets frame-order bookkeeping; `LossPending` returns to `NoSession`
   then re-creates after `retry_frames` fixed steps (a counter, not time).
2. (Sim) `Focused` gating: with state `Visible`, every action reads inactive/default; after `Focused`,
   injected input appears; two reads between syncs are identical.
3. (Sim, no GPU) Frame-order wrapper: `end_frame` without `begin_frame` returns `xr_frame_out_of_order` and emits no
   backend call; a missed render still ends the frame; empty layer list is accepted; `should_render=false`
   produces no eye render.
4. (Sim) Mailbox: with the frame thread paused, extraction repeats the old frame and increments
   `xr.stale_frames` each frame; the sim fixed-step count is unchanged by the pause (asserts the renderer cannot block
   the sim).
5. (Pure) Projection: the built `Fov::projection` tests stay; a symmetric eye uses `DepthMode::Standard` and far
   `INFINITE_FAR_STANDIN` (at least twice the near plane), the same matrix as the engine camera, and the eye
   matrices fed to the renderer are that projection times the latched view.
6. (Pure) Tracking spaces: `Stage` request falls back Stage→LocalFloor→Local when unsupported and
   reports `actual_tracking_space`; recenter aligns head yaw to rig forward and emits `XrOriginChanged`.
7. (Pure) Action manifest: load/validate/round-trip canonical RON; duplicate names, unknown sub-paths and a
   bad `format_version` are rejected with the names; attaching twice → `xr_actions_frozen`; creating in an
   attached set → `xr_actions_frozen`.
8. (Sim) Snap turn: stick deflection past 0.6 rotates the rig by `snap_turn_deg` once, re-arms below 0.3,
   honors cooldown, and the head world position is unchanged (rotation about head); vignette strength rises
   with rig speed and stays 0 when only the head moves.
9. (Sim) Haptic clamping: amplitude <0 or >1 clamps; a device without haptics is a diagnostic, not an error.
10. (Sim) Non-finite pose is rejected, previous pose kept, `xr.bad_poses` increments.
11. (Sim) Agent protocol: every § 5.1 method round-trips; wrong backend gives `xr_simulator_only`; unknown
    option gives `xr_unknown_option`; `xr.get` reflects `xr.set_option`.
12. (GPU) Two-eye render parity: the same scene rendered by multiview and by two-pass yields images equal within
    the renderer's tolerance per eye (skipped when multiview is unsupported, recorded as skipped).
13. (GPU) `xr.capture_eye both` of a scene with one near and one far object: horizontal disparity of the
    near object exceeds the far object's; mono eyes (`xr.drive mono`, since a real IPD is never 0) give identical
    eyes.
14. (GPU or CI image with null runtime, feature `openxr`) The § 3.4 spike items 1-3.
15. (CI) `cargo check` of `regia_xr` with and without `xr` for Linux, Windows, Android aarch64
    (`cargo ndk -t arm64-v8a check -p regia`); `cargo xtask check-layers`; `cargo deny check`.
16. (HW) A real runtime on Linux desktop (monado-class), Windows, and a standalone Android headset each reach
    `Focused`, render a rotating cube, and track head and controllers at the headset refresh rate; session
    survives remove/re-wear (loss and restart handled); Android pause/resume cycle recreates the session
    and keeps the GPU device. **Not verifiable until a headset is available.**
17. (HW) Motion-to-photon and judder checks with the runtime's compositor timing tools; comfort review of snap
    turn and vignette by a human. **Not verifiable until a headset is available.**

Golden images for the Simulator scene are a TODO for the lead; builders never create them. No wall-clock
assertions anywhere; use counters and fixed steps.

## 9. Platform notes

| Platform | Notes |
|---|---|
| Linux | Dynamic loader (`libopenxr_loader.so.1`); runtime chosen by the loader's active-runtime config or manifest override; a monado-compatible open runtime is the reference; Vulkan only; X11 and Wayland mirror window both fine. |
| Windows | Dynamic `openxr_loader.dll`; active runtime from the registry via the loader; Vulkan binding (D3D is not used since the renderer is Vulkan-first on this target when XR is on). |
| Android aarch64 | Standalone headsets: the loader ships in the APK/AAB; needs the loader-init call with the activity/JVM before instance creation and the Android instance-create struct; Vulkan binding; pause/resume: end the session on pause, **drop the wgpu Surface but keep `Device`**, recreate the surface on resume, and create a new session on `READY`. The Gradle shim (`specs/21`) gains the headset manifest category and the OpenXR permission/feature entries. No mirror. Frame budget is the tightest target (§ 10). |
| macOS | Testing-only: Simulator backend and pure tests only; no runtime exists, no Vulkan; `xr` feature must still type-check. |

Dependencies (Decision ADR-1585): `openxr` crate, behind the `openxr` cargo feature of `regia_xr`, off by default.
Licence MIT OR Apache-2.0 (on the `docs/03` § 4 allow list). It is Rust bindings (`openxr-sys` is generated FFI
declarations, no C compiled) that load the Khronos loader at run time with its `loaded` feature, so there is no
cmake, no system library at build time and Android cross-compiles with the NDK only; the Android loader library is
bundled in the package (Apache-2.0, notice added to `THIRD_PARTY_NOTICES`). Needs a `docs/03` row and a passing
`cargo deny check` with all features (the builder verifies; the lane cannot). `ash` is already in the graph through
`wgpu-hal`; use the same version. No GPL/LGPL code.

`unsafe`: `regia_xr` needs raw Vulkan handle wrapping and `openxr` FFI calls; add `regia_xr` to the `docs/04` § 5
unsafe table (reason "raw Vulkan handles for the XR graphics binding") in the same change, with an ADR; every block has
a `// SAFETY:` comment. The Simulator and manifest code is safe Rust and can be split into a module without the opt-out
if the builder prefers a crate-level `#![deny(unsafe_code)]` on those modules.

## 10. Performance budget

Min-spec for XR is a **separate class from the engine min-spec**: XR is supported only on hardware that
sustains the headset refresh rate. Targets: **90 fps** (11.1 ms per frame) on a desktop-class GPU tier
(a recent mid-range discrete card) and the headset's native rate (72 or 90 Hz) on a standalone
Android aarch64 SoC at reduced quality profile. The runtime reserves part of the GPU for its own
reprojection, so the app's usable GPU time is about **9-10 ms at 90 Hz**.

| Item | Budget |
|---|---|
| Total frame (CPU, one thread) | ≤ 8 ms at 90 Hz; sim must not stall the XR frame thread |
| GPU, both eyes (desktop) | ≤ 9 ms; per eye ≤ 4.5 ms (two-pass) or one multiview pass ≤ 7 ms |
| GPU, standalone Android | ≤ 11 ms at 72 Hz at render scale 0.8-1.0, reduced pass set |
| XR bookkeeping CPU (poll, sync, locate, extraction, submit) | ≤ 0.3 ms |
| Mirror window (desktop) | ≤ 0.4 ms; disabled by default on Android |
| Memory | swapchain targets + depth ≤ 400 MiB at the recommended size for desktop; ≤ 250 MiB on Android |
| Latency | the frame thread never waits on game work; at most one frame of queue between sim and render |

Tracing spans: `xr_poll_events`, `xr_wait_frame`, `xr_begin_frame`, `xr_sync_actions`, `xr_locate_views`,
`xr_extract`, `xr_render_eye` (field `eye`), `xr_submit_layers`, `xr_end_frame`, `xr_mirror`. Counters:
`xr.stale_frames`, `xr.dropped_frames`, `xr.bad_poses`. A frame-time overrun beyond budget for N
consecutive frames raises a diagnostic `xr_over_budget` and, if `auto_scale` is on, lowers render scale one step.
As built (ADR-5642): every span above exists (`xr_wait_frame`, `xr_begin_frame`, `xr_locate_views`, `xr_submit_layers` and
`xr_end_frame` in `regia_platform::xr::run_frame_layers`; `xr_render_eye` per eye in the stereo pass), plus `xr_frame`,
`xr_late_latch`, `xr_render_frame`, `xr_multiview`, `xr_capture_eye` and `xr_walk`; the counters are `xr.get` `stats`.
As built (post-v0.5, `regia_xr::budget`): the budget is 0.9 of the display period (10 ms at 90 Hz, 12.5 ms at 72 Hz),
N is 10 frames (`OVER_BUDGET_FRAMES`), a frame on the budget is inside it, a frame inside it resets the run, and the run
starts over after each raise (hysteresis: the next raise needs N more overruns, measured at the new scale). The diagnostic
is a `tracing` warning with `code = "xr_over_budget"`, the `XrOverBudget` message, `xr.get` `stats.over_budget` and
`budget.last`. `auto_scale` is a runtime option (`xr.set_option auto_scale`, default off, `XrFrameBudget::auto_scale`),
not a `config/xr.ron` field, so the file stays v1; a raise lowers `render_scale` by 0.1 on the hundredths grid down to
the 0.5 floor (at the floor the diagnostic still fires), applied at the next frame like `xr.set_option render_scale`;
nothing raises it again automatically. Frame time: the CPU time of the stereo frame the renderer drew
(`XrRenderStats::last_cpu_ms`) on a real backend; on the simulator only the scripted `xr.drive frame_time { ms }`
(never the wall clock, so § 7's bit-exact runs hold). Observed in the `Last` frame loop (`xr_frame`), no `FixedUpdate`
system. GPU time is not measured (no timestamp queries on the eye passes yet).

## 11. Out of scope / follow-ups

- AR (passthrough, planes, anchors, scene understanding), alpha/additive blend modes.
- Quad-view / foveated multi-viewport, eye-tracked foveation, hand mesh and gesture recognition, body
  tracking, face tracking, spatial anchors, custom-hand-model retargeting.
- Late latching, application space warp / motion-vector reprojection submission.
- Spatial UI layer details (cylinder/equirect layers, depth-tested layers), loading-veil layers during
  level transitions (reserved in the layer API), teleport/grab gameplay kit, onlooker camera rig.
- DirectX/Metal bindings, dynamic resolution controller, passthrough capture for QA.
- A conformance-test integration with the Khronos conformance suite.

## 12. Provenance and open questions

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Session lifecycle | The state set (idle, ready, synchronized, visible, focused, stopping, loss pending, exiting), begin/end rules and event-driven transitions | Khronos OpenXR 1.1 Specification, "Session lifecycle" chapter, registry.khronos.org/OpenXR/specs/1.1/ |
| Frame loop and layers | wait/begin/end frame ordering, predicted display time, projection layers with sub-images and depth info, empty-layer clear | OpenXR 1.1 Specification, "Frame synchronization" and "Composition" chapters |
| Vulkan interop | Graphics-requirements-before-session, runtime-negotiated instance/device creation (`XR_KHR_vulkan_enable2`), swapchain images as raw handles | OpenXR 1.1 Specification, Vulkan graphics binding chapter; OpenXR Loader design document |
| Action input | Action sets, interaction profile suggested bindings, attach-once, sync-then-read | OpenXR 1.1 Specification, "Input and haptics" chapter |
| Spaces | Reference spaces local/stage/(local floor), reference-space change events, view/pose location at a display time | OpenXR 1.1 Specification, "Spaces" chapter |
| Off-axis projection | Asymmetric-FOV projection from four angles, standard depth (near 0, far 1) and far plane `INFINITE_FAR_STANDIN` | Standard graphics derivation (Real-Time Rendering, 4th ed., projection chapter). The frame's depth is ADR-869 and ADR-1676. |

Cleanroom note: no source from any engine's XR plug-ins was consulted; names used are the OpenXR
standard's names and Regia's own.

### Decisions (ADR-1585)

- Build on the built seam (`regia_platform::xr`, ADR-1571); no second backend trait; extend `SessionState` with the
  five missing OpenXR states (additive; the ext2 module is unreleased on main).
- Agent surface extends `xr.get` / `xr.drive`; only `xr.set_option`, `xr.recenter`, `xr.capture_eye` are new; errors are
  `F0002` with `data.reason` `xr_*`.
- `regia_xr` is layer 2 with a `regia_render` edge (as `regia_ui`); `regia_agent` depends on it optionally.
- Per-eye TAA history is kept in the XR profile but is the first thing the Android reduced pass set drops
  (`XrRenderProfile.per_eye_history: bool`, false on Android by default).
- Budget numbers (90 Hz desktop, 72 Hz standalone) are targets, not measurements. Default action manifest and comfort
  numbers (30 degrees, 0.6 and 0.3 thresholds) are Regia's own proposals, not sourced values; tune with a headset.
- wgpu 30.0.1 exposes the raw import hooks (§ 3.4); the spike still gates the render hand-off.

### Owner decisions

- Hardware: the owner has no confirmed headset, so the HW tests (§ 8.16-8.17) stay unverified until the owner provides
  one (a standalone Android headset and/or a desktop PC headset). Nothing else needs the owner.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <https://registry.khronos.org/OpenXR/>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrCreateInstance.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrGetSystem.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrCreateSession.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrBeginSession.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrCreateActionSet.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrCreateAction.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrAttachSessionActionSets.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrSyncActions.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrEndFrame.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrEnumerateInstanceExtensionProperties.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrEnumerateEnvironmentBlendModes.html>
- <https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrEndSession.html>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <https://learn.microsoft.com/en-us/windows/mixed-reality/develop/the legacy engine/the legacy engine-hand-tracking>
- <https://learn.microsoft.com/en-us/windows/mixed-reality/develop/the legacy engine/the legacy engine-gaze-input>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <https://registry.khronos.org/OpenXR/specs/1.0/man/html/XrHapticVibration.html>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
