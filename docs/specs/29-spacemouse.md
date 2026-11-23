# 29. SpaceMouse (6-DoF navigation device) support

Status: **accepted defaults, 2026-10-15** (owner: "all apps are the same across the board out of the box"). Applies to Regia Engine's
editor, Tincal, Red-Rouge and any other Regia Engine UI app. Lane: `v020b-spacemouse`. Implementation: `regia_input::spacemouse`.

## 1. Scope
- Navigation **while in a UI**: 3D viewports (orbit, pan, zoom, fly, turntable), 2D panes (pan/zoom views, timelines, list stepping), fit, home.
  Gameplay use is out of scope for the defaults; a game may read the raw device ([§ 3](#3-api)).
- Desktop only (Linux, Windows; macOS when it enters the platform matrix). Android and consoles: no backend, the module compiles to nothing.
- Devices: 3Dconnexion SpaceMouse family (vendor id `0x256F`; legacy Logitech-branded units `0x046D` with the known 3Dconnexion product ids).
  Any number of devices; all feed one merged state.

## 2. The defaults (the same in every app; apps do NOT change these)
All numbers are the out-of-the-box behaviour. An app may expose only the three user overrides in § 2.7; it must not ship different defaults.

### 2.1 Input normalisation
- Raw axis range of a wired SpaceMouse is about ±350. Normalised value `n = clamp(raw / 350, -1, 1)`.
- Frame: right-handed, **+X right, +Y up, +Z toward the viewer**, for both translation (push/pull/lift the cap) and rotation (tilt/twist).
  Rotation axes: `rx` pitch (tilt the cap away from you = positive), `ry` yaw (twist counter-clockwise seen from above = positive),
  `rz` roll (tilt the cap to the left = positive). The backend maps each device's report to this frame; apps never see device axes.

### 2.2 Dead zone and response curve
- Per axis: `t = max(0, (|n| − 0.04) / (1 − 0.04))`, then `v = sign(n) · t²` (quadratic: fine control near the centre, full speed at the stops).
- Dominant-axis mode (isolate the largest axis): **off**; toggled by Button 3 where the device has it (§ 2.5).
- No extra smoothing: the device is already filtered. Use the **latest sample per frame** (never sum events), scaled by `dt`, so motion is
  frame-rate independent.

### 2.3 Navigation modes
- **Orbit (default for object/asset viewports: Tincal, Red-Rouge, the editor):** the scene follows the cap ("object mode").
  - Pan: `Δtarget = (v.x, v.y) · distance · 1.0 · dt` in view space, per second at full deflection (scales with the orbit distance).
  - Zoom: `distance ← distance · exp(v.z · 1.5 · dt)`. Z is the push/pull axis: pushing the cap away from you (−Z) zooms in, pulling it
    toward you (+Z) zooms out. Lifting the cap (+Y) pans up.
  - Orbit: `yaw += v.ry · 1.5 rad/s · dt`, `pitch += v.rx · 1.5 rad/s · dt` around the orbit target. `rz` (roll): **ignored** while horizon lock is
    on (default on).
- **Fly (default for walk-through / free cameras):** camera moves with the cap: translate `v.xyz · speed · dt`, `speed = 1.0 × scene_scale` m/s
  at full deflection (apps pass `scene_scale`, e.g. the bounding-sphere radius), rotate `v.rx, v.ry` at 1.5 rad/s, roll only when horizon lock is off.
- Horizon lock: **on** by default; toggled by Button 4 where the device has it, and by the same action name in the UI.
- Pitch is clamped to ±89° when horizon lock is on.
- **Turntable** (review lightboxes that spin an object instead of moving a camera): apply `orbit.yaw` and `orbit.pitch` of the Orbit delta to the
  object's rotation (same speeds, pitch clamp and horizon lock); pan and zoom are the Orbit ones.
- **View2D** (2D canvases: crop windows, image and video panes): pan `Δ = (v.x, v.y) · visible_extent · 1.0 · dt`; zoom
  `extent ← extent · exp(v.z · 1.5 · dt)` (push away = zoom in, as in 3D). `rx, ry, rz` unused.
- **Timeline** (a 1D time axis: players, sequencers, curve editors): `playhead += v.ry · visible_span · 1.0 · dt` (twist scrubs); zoom on Z as View2D.
- **Step** (discrete lists: scene cards, cuts, items): one step when the **shaped** value `|v.rz|` (after dead zone and curve, § 2.2) crosses 0.5 (sign gives direction: tilting the cap right, `rz` < 0, is +1 = next; that is about 71 % of the raw deflection), repeating every 0.35 s while held past 0.5. A tilt that flips from one side to the other between two frames is a new crossing (ADR-265). Sensitivity (§ 2.7) is a rate multiplier and moves neither the threshold nor the repeat interval.
- In 2D modes `view.fit_selection` fits the pane's content to the selection (the crop to the frame, the timeline to the selection) and
  `view.home` resets the 2D view.
- Active only while the pointer is over a viewport pane, or the viewport has keyboard focus, and no modal or text field has focus. Never
  consumed by gameplay unless the game opts in.

### 2.3b Object tool (opt-in, any app)
An app may let the user switch on an **Object tool** where the cap moves or rotates the selected object, part or joint pivot instead of the
camera. Rules: off by default; switched by the action `spacemouse.object_tool_toggle` (no device button by default); it uses the same
`nav::step` delta (pan becomes a translation in view space, orbit becomes a rotation about the selection pivot, zoom is unused or scales along
the view axis); one gesture (from first deflection until 300 ms of rest) is **one undo entry** through the app's command layer
(`regia_editor::command::EditorCommand` in Regia Engine apps); camera navigation is suspended while it is on.

### 2.4 Camera deltas: one shared function
Apps do not implement § 2.2-2.3 themselves. They call the shared pure function and apply the result to their own camera:

```rust
let delta: NavDelta = nav.step(&SpaceMouseState, dt, NavContext { mode, distance, scene_scale, horizon_lock });
// NavDelta { pan: Vec3 /* view space, world units */, orbit: Vec2 /* yaw, pitch rad */, roll: f32, zoom: f32 /* distance, extent or span multiplier */,
//            translate: Vec3 /* fly, view space */, scrub: f32 /* Timeline: fraction of visible_span */, step: i8 /* Step: -1, 0, +1 this frame */ }
// NavContext { mode: NavMode /* Orbit | Fly | Turntable | View2D | Timeline | Step */, distance, scene_scale, visible_extent, visible_span, horizon_lock }
```
This is what keeps Tincal, Red-Rouge and the editor identical.

*Implementation note (ADR-219):* `NavContext` also carries `pitch` (the camera's current pitch, for the ±89° clamp),
`dominant`, `sensitivity` and `invert` (set with `with_pitch`, `with_toggles`, `with_config`), and `nav::step` caps `dt` at
`NavigationDefaults::MAX_DT` = 0.1 s. `NavDelta` is in camera terms: `orbit.x` positive turns the view counter-clockwise seen
from above (an orbit eye moves counter-clockwise around the target), `orbit.y` positive tips the view down (the orbit eye
rises), `pan` and `translate` are view space (x right, y up, z toward the viewer). Step's memory is passed in and handed
back to keep the function pure: `NavContext::step_held` / `NavDelta::step_held` (seconds held past the threshold) and
`NavContext::step_dir` / `NavDelta::step_dir` (the last tilt direction; ADR-265); build the context with
`NavContext::list_dir(held, dir)`.

### 2.5 Buttons (action names are stable; the key/button binding is data)
| Device button | Action (default) | Notes |
|---|---|---|
| 1 (left) | `view.fit_selection` | frame the selection, or everything when nothing is selected |
| 2 (right) | `view.home` | reset to the app's home view |
| 3 | `spacemouse.dominant_toggle` | on devices that have it |
| 4 | `spacemouse.horizon_lock_toggle` | on devices that have it |
| 5+ | `spacemouse.button.N` | exposed as actions and agent events, unassigned |
Apps implement `view.fit_selection` and `view.home` once (they already have fit and home); the rest is generic.

### 2.6 Hot-plug, devices, errors
- Devices may appear and disappear while the app runs; the app never fails because a device is missing, unreadable or in use by another
  driver. A missing permission (Linux hidraw udev rule) is a single `warn` with the rule to install, then silence.
- The device LED/beep and 3Dconnexion's own driver are never touched. If the vendor driver already owns the device and the backend cannot
  open it, the module reports `Unavailable(reason)` and the app shows nothing.

### 2.7 What a user may override (and nothing else)
Per app, in the app's input settings (RON `spacemouse` section, `regia_input::config`): `enabled: bool` (default true), `sensitivity: f32`
(a multiplier on every rate: translation, rotation, zoom and timeline scrub, default 1.0, range 0.1 to 4.0), `invert: [bool; 6]` (default all false). Dead zone, curve,
modes, button actions and speeds are **not** configurable per app.

## 3. API
Crate `regia_input`, module `spacemouse` (cfg: non-Android and the crate's default `devices` feature, which a server build
turns off; ADR-1886):
- `SpaceMousePlugin` (added by `InputPlugin` on desktop, no-op without a device): resources `SpaceMouseState` (latest merged state:
  `translation [f32; 3]`, `rotation [f32; 3]`, `buttons u32`, all normalised per § 2.1, before dead zone), `SpaceMouseDevices` (list, status).
- Messages: `SpaceMouseButton { button: u8, pressed: bool }`, `SpaceMouseAction { action, button }` (the § 2.5 action a
  press fired), `SpaceMouseConnected` / `SpaceMouseDisconnected`. Resources besides the state: `SpaceMouseNav` (the runtime
  toggles: horizon lock, dominant axis, object tool) and `SpaceMouseInjection` (§ 4). `ObjectGesture` groups Object-tool
  frames into gestures (§ 2.3b).
- `SpaceMouseConfig` (§ 2.7) and `NavigationDefaults` (§ 2.2-2.3 constants, `pub const` so tests and docs quote them).
- `nav::step` (§ 2.4), pure and deterministic.
- Linux backends, tried in this order: (1) the **spacenavd** socket (`/run/spnav.sock`, also `/var/run/spnav.sock`), which needs no permissions
  and shares the device with other apps (Blender): a client written from the public protocol notes, no libspnav code; (2) direct HID. spacenavd
  reports already-scaled motion events and button events; the backend normalises them to the § 2.1 frame. Windows: HID. macOS later.
  `SpaceMouseDevices` names which source is active: each device's `info.source` is `spacenavd`, `hid` or `fake`
  (`SpaceMouseDevices::backend` names the backend object, `hid` for the OS backend whichever source it reads).
- Backend (private, HID): HID reports read through the OS HID layer on a reader thread (no vendor SDK, no vendor driver headers). Report layout
  from public documentation (the open-source spacenav project's protocol notes and the HID descriptor the device itself publishes): report 1
  translation `x, y, z` as little-endian `i16`, report 2 rotation, report 3 buttons bitmask; some models send translation and rotation in one
  report. The backend parses by the device's own descriptor where it can and by the known layouts otherwise.

## 4. Agent surface (spec 23)
- `input.spacemouse_status`: devices, connection state, last sample, config. `input.spacemouse_inject { translation, rotation, buttons, frames }`
  (test/automation; injected state replaces device state for the frame, or `frames` frames) and `input.spacemouse_nav_defaults` (returns § 2
  constants). (Method names use one dot, the protocol's `area.name` rule; ADR-219.) Button actions and hot-plug are `events.read` records of
  kind `spacemouse`.
- Agent-guide page `book/agents/spacemouse.md`; user-guide page `book/user-guide/spacemouse.md` with the udev rule and the defaults table.

## 5. Tests (no hardware in CI)
- Recorded report bytes for each known layout, parsed to the expected normalised state.
- `nav::step` golden table: inputs → `NavDelta` for orbit and fly, dead zone edge, curve, frame-rate independence (same motion at 30 and 144 fps within 1 %), horizon lock, pitch clamp, sensitivity and invert overrides.
- Plugin test with `input.spacemouse_inject`; hot-plug via a fake backend.
- Modes: golden tables for Turntable, View2D, Timeline and Step (threshold, repeat interval, direction).
- Object tool: one gesture produces exactly one undo entry (with a fake command layer).
- Each app (editor, Tincal, Red-Rouge) has one test that feeds an injected state and asserts its camera moved by `nav::step`'s delta, which pins "same across the board".
  In this repository that is the editor's `viewport::tests::spacemouse_injection_moves_the_camera_by_nav_step`; Tincal and
  Red-Rouge live in their own repositories.

## 5b. Implementation status (audit 2026-11-10, ADR-5631)
Where each requirement is implemented and tested is listed in the lane report `sprint-spec-12-29-spacemouse`. Open items:
- **Editor 2D panes and fly cameras.** The editor drives only its 3D viewports (Orbit). Its 2D panes (sequencer timeline,
  curve editors, image panes) and its fly camera do not read the SpaceMouse yet; `NavMode::View2D`, `Timeline`, `Step`,
  `Fly` and `Turntable` are implemented and tested in `nav::step` and await their app integrations (v0.5.x).
- **Editor camera limits (refinement).** The editor's orbit camera has no roll and keeps its pitch within ±1.45 rad (about
  83°, shared with mouse orbit and the agent's `pitch`), inside the ±89° clamp of § 2.3. Horizon lock therefore has no
  visible effect in the editor, and the editor shows no horizon-lock toggle; Button 4 still flips `SpaceMouseNav`.
  An app with a rolling camera offers the UI toggle by the action name of § 2.5.
- **Hardware confirmation.** The device-frame and spacenavd axis signs (§ 2.1, § 3) come from public protocol notes and
  still need a check on a real SpaceMouse (ADR-219); the user `invert` override covers a wrong sign meanwhile.
- **Modal focus.** The editor navigates only in the active pane of the panel that has the hotkeys, and not while egui
  wants keyboard input (a text field, or a modal that takes keyboard focus). It has no separate modal check, and no test
  drives the focus rule (it needs a real egui frame).

## 6. Cleanroom and licence
Public protocol knowledge only. Do not read, copy or link the 3Dconnexion SDK (`3DxWare`), driver sources or headers. Any new dependency (an HID
library) gets an entry in `docs/03-workspace-and-dependencies.md`, a passing `cargo deny check`, and a `docs/provenance.md` entry.
