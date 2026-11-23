# Spec 11 — Platform (`regia_platform`) and Input (`regia_input`)

## 1. Event loop and windows

- `PlatformPlugin` installs the app runner: creates a winit `EventLoop`, implements
  `ApplicationHandler`, and calls `app.update()` once per redraw (`about_to_wait` → `request_redraw`
  → `RedrawRequested`).
- `Window` component on an entity (primary window entity created at startup from `WindowSettings`):
  title, logical size, `WindowMode { Windowed, BorderlessFullscreen }` (borderless uses the current
  monitor; there is no monitor argument and no exclusive fullscreen), `present_mode` (`Vsync`
  default, `VsyncRelaxed`, `Immediate`), resizable, decorations, `cursor_visible`,
  `cursor_grab: None|Confined|Locked` (no cursor icon), `ime_enabled`, and `app_id` (set at
  creation only). There is no position field and no minimum size. `physical_size` and
  `scale_factor` are read back from the OS; a value written on the component is replaced on the
  next create or resize and is not an override (ADR-5070).
  Changes to the component apply to the OS window in `PostUpdate` (`apply_window_changes`), which is
  pinned to the main thread with `NonSendMarker`: winit's window setters post to the event-loop
  thread and block off it, so a task-pool thread would deadlock the frame loop on Windows.
  `CursorGrab::Locked` is not supported by winit on Windows or on X11 (including Xwayland) and falls back to `Confined` there. `CursorGrab::Confined` is not supported on macOS and falls back to `Locked` there. A grab the OS rejects is left unapplied and retried on a later frame.
- `CursorRelease` (resource, ADR-7349): while true, `apply_window_changes` asks the OS for a free,
  visible cursor on every window whatever its `Window` component says — the hold is applied once and
  a game that re-writes its grab (every frame, or on focus churn) makes no OS call while it is on.
  The components keep the game's own `cursor_grab` and `cursor_visible`; when the resource turns
  false, the components' current values are re-applied on the next `PostUpdate`. The Steam overlay
  holds it while open (`SteamSettings::release_cursor_on_overlay`, spec 24 § 2).
- Multiple windows: only the primary window becomes an OS window today (a `Window` entity with no handle is
  skipped by `apply_window_changes`, ADR-2252); the editor's detached panels need the same.
  Runtime games normally use one.
- Window events become ECS events: `WindowResized`, `WindowScaleFactorChanged`, `WindowFocused`,
  `WindowCloseRequested` (default handler exits app when primary window closes; games can intercept
  to show "quit?" dialog), `FileDropped` (desktop), `ImeEvent`.
- Raw window handles exposed to the renderer through the `WindowHandles` resource (entity →
  `Arc<winit::Window>`; ADR-5610: there is no `RawHandleWrapper` type).

### Runner diagnostics (v0.5.1, ADR-7965)

The runner starts unfocused until an OS focus event. Before each update it publishes
`regia_app::RunnerState`: focus, all-window occlusion/zero size, stalled redraw delivery,
repeated surface refusal, and the configured hidden/frozen/idle period. These observations
are distinct from a host's optimistic visibility policy. Winit wait-start timestamps measure
actual event sleep; `App::record_event_wait` attributes it to `FramePhases`' appended
`event_wait` phase rather than `outside_update`. Phase totals retain the wait exactly once.
Hidden deadlines are frame-start-relative, including the update's work; early event wakes
still obey the existing throttle due check. Telemetry and the existing telemetry agent methods
expose these optional diagnostics; see `book/user-guide/telemetry.md`.

## 2. Application lifecycle (critical for Android)

`AppLifecycle` resource + `LifecycleEvent` events: `Started`, `Suspended`, `Resumed`, `WillTerminate`,
`LowMemory`.

| winit callback | Regia Engine behaviour |
|---|---|
| `resumed` (first time) | create windows, emit `Started` + `Resumed`; renderer creates surface |
| `suspended` | emit `Suspended`; **renderer drops surfaces** (keeps device & resources); audio pauses output stream; gameplay `Time` pauses; autosave hook runs (`regia_framework::SavePlugin` saves once on the `Suspended` edge, in the `Lifecycle` schedule; there is no `SaveGameAutosave` event, ADR-5610) |
| `resumed` (again) | recreate surfaces, resume audio, unpause time |
| `memory_warning` | emit `LowMemory`; the renderer signals GPU memory pressure (`gpu_health`, source `os_low_memory`) and world streaming trims (ADR-5610: the asset server keeps no cache of unreferenced assets to drop) |
| app exit (`exiting`) | the clean exit (ADR-188): stop background GPU work, leave fullscreen, wait for GPU idle (bounded), then `Suspended` + `WillTerminate` (surfaces drop; flush logs, save settings), then the world (renderers and every GPU object; the runtime renderer is freed on a helper thread while the world clears, ADR-196) and the device drop, then the pipeline cache is written. Each step is time-boxed and logged |

Desktop: minimise ≈ throttled frames (no suspend). *(batch v020, ADR-213 Steam Deck)*: on Linux the runner detects system
sleep (a `CLOCK_BOOTTIME` vs `CLOCK_MONOTONIC` gap of 2 s or more) and runs `Suspended` then `Resumed`, so a Deck that sleeps recreates
its surface and restarts audio like Android does. Pausing on focus loss is the project setting
`pause_on_focus_loss` (`[runtime]` of `Regia.toml`, default off, ADR-7070): `regia_platform::PauseOnFocusLoss` sets
`Time::paused` when the primary window loses focus and clears it on focus gain, lifting only a pause it set itself.
Focus loss also releases held keys and buttons and cancels live touches (`regia_input`).

## 3. Android specifics

- Entry: user project's `android/` crate is a `cdylib` named `main` with
  `#[unsafe(no_mangle)] fn android_main(app: AndroidApp) { regia::run_android(app, game::plugin()) }`
  (macro `regia::android_main!(MyGame)` generates it).
- Uses **GameActivity** (`android-activity` feature `game-activity`, winit `android-game-activity`).
- `AndroidApp` stored as a resource (`AndroidAppHandle`, an ordinary `Clone` resource: `AndroidApp` is
  `Send + Sync`, ADR-5610) for JNI/asset access.
- Assets: `assets://` VFS mount reads from the APK via `AAssetManager` (`ndk::asset`). Pak files are
  stored **uncompressed** in the APK (`noCompress` in Gradle) so they can be streamed with seeks.
- Paths: `user://` → `app.internal_data_path()`; `cache://` → cache dir via JNI `getCacheDir`.
- Soft keyboard: `show_soft_input`/`hide_soft_input` via android-activity when a UI text field gains
  focus; text arrives via GameTextInput → `ImeEvent`.
- Display cutouts/safe area: query `WindowInsets` via JNI on resume and on config change; expose
  `SafeArea { left, top, right, bottom }` resource in physical pixels.
- Orientation: from `Regia.toml` (`landscape`, `portrait`, `sensor_landscape`, ...) → manifest.
- Back button: `BackPressed` event; while `ClosePolicy::back_closes_primary` is true (the default) it
  also sends `WindowCloseRequested` for the primary window; a game that handles back itself turns the
  policy off (ADR-5610: there is no per-event "handled" flag).
- Vibration (`Haptics::vibrate(duration, amplitude)`) via JNI `VibratorManager` (Tier 1, simple).
- Thermal status (Tier 2): `PowerManager.getCurrentThermalStatus` → `ThermalState` resource for
  dynamic quality scaling.
- Keep screen on while playing via window flag `FLAG_KEEP_SCREEN_ON`, set from code with the
  `KeepScreenOn` resource, or from the project with `[runtime] keep_screen_on = true` in `Regia.toml` (ADR-7070).

## 3a. Frame pacing (N12, ADR-1942)

A frame starts on a display vsync and stays on screen for a whole number of vsyncs, the swap interval,
so a phone shows even frames instead of bursts. `regia_platform::frame_pacing` holds a pure model over
nanosecond timestamps (`PacingModel`, `simulate`) and the live parts around it.

- **Ticks.** A platform with a tick source feeds `VsyncTicks`; on Android a `regia-vsync` thread with its
  own looper re-posts `AChoreographer_postFrameCallback64` (`postFrameCallback` before API 29) and stops
  posting while the app is suspended. Other platforms have no source and stay on `FramePacing`.
- **Period.** The lower quartile of the last 16 tick intervals. A skipped tick only lengthens an interval.
- **Swap interval.** The smallest that holds the software cap (a cap is never exceeded, so 45 fps on 60 Hz
  is 30). It rises when frames run over, straight to where the frame fits, and falls after 60 frames fit
  a lower interval with 12.5 % to spare, straight to the shortest slot the costliest of them fits (ADR-2320); that wait doubles, up to 960 frames, when a drop bounces. It stops at
  the fewest vsyncs whose slot holds 50 ms (20 fps), on any panel rate; only a forced interval is limited to 8. A refresh-rate change keeps the slot's duration.

- **Waiting.** After `App::update` the runner waits for the planned tick, for at most its slot plus a
  vsync (1 s at most). Three timeouts in a row (the display off) hand frames back to the software cap until a tick
  arrives. A hidden window or `LoopMode::Bench` does not wait for ticks.
- **The compositor.** While the slot is longer than one vsync of the panel's own rate,
  `ANativeWindow_setFrameRate` (API 30) suggests the slot's rate, so a 120 Hz panel can settle on a
  multiple of it. The panel may follow: the ticks slow, the interval falls to 1 and the hint stays, because
  clearing it would send the panel straight back. The model remembers the panel's own period. A hint
  clears after frames fit the next shorter slot on the panel's grid under the same rule as a lower
  interval (the cap permitting), and the slot is kept across the change. A forced `Interval(n)` counts
  vsyncs of the panel's own rate.
- **Frame cost (ADR-2320).** What `Auto` compares with a slot is the frame's work: its time from start to
  end less the time it sat blocked in the swapchain (acquire and present), which the renderer reports with
  `note_swapchain_wait`. That blocking is the display queue draining at the rate pacing chose; counting it
  made every slow slot look full, and with a compositor that follows the rate hint the interval rose until
  frames took two slots of the 20 fps limit (both test phones, 10-15 fps). A frame blocked for more than an
  eighth of a vsync is *held*: if it missed its slot's tick it starts at once on that tick, and it counts in
  `PacingStats::held`, not `late`. `FramePacingSettings::cost = PacingCost::Frame` restores the v0.2.10 model.
  `simulate` can put a stand-in swapchain (`SwapchainSim`) in front of the model.
- **Hint (ADR-2339).** `FramePacingSettings::hint` picks the rate sent to the compositor. `Double`, the
  default, asks for the slowest whole divisor of the panel's rate that still gives each frame two vsyncs or
  more on the panel's grid (a slot of `n` panel vsyncs as `n / d` vsyncs of `d`, `d` the largest divisor of
  `n` up to `n / 2`; nothing when `d` is 1). A phone's display queue holds two of the app's buffers, so a
  frame needs about two latches, and the wait for them lands inside the frame (the buffer comes back from
  acquire with a fence): the Fairphone 5 ran at half the panel's rate under every setting, so the
  ADR-1942 hint, the slot's own rate, halved the rate it chose. `Slot` keeps that hint; `Off` sends none.
- **Probes (ADR-2339).** A frame's cost can grow with the slot it is given: the Fairphone 5's frames cost
  29 ms unpaced and 51 ms at a 15 Hz slot, outside acquire and present (clocks that fall when paced frames
  leave the phone idle, or GPU waits tied to the display's rate). Such a cost never fits a shorter slot on
  paper, so the slot cannot come down after a start-up stall. With `PacingCost::Work`, when the slot was
  raised by a frame at least a third costlier than the current frames and those still do not fit the next
  shorter slot, the model tries that slot after a hold anyway: one step down the interval or the panel's grid.
  For up to 120 frames, or until 8 in a row fit, a frame that misses its tick starts at once on it (an idle
  wait would keep the clocks down) and over-budget frames do not count towards a raise. If the frames then
  fit, the slot stays; otherwise the raise goes back to the slot the probe left (asking the compositor
  again when that is off the current ticks' grid), counts as a bounce (the hold doubles) and the raising
  frame becomes the new reference, so a cost that does not grow is probed once. `PacingStats::probes`
  counts them; `PacingCost::Frame` never probes. `PacingSim::stretch` makes a simulated frame's work grow
  into that fraction of the idle time of recent frames.
- **Start (ADR-5946).** A paced frame is released when its tick is due on the grid of the real ticks (the tick's
  time plus a slowly following delivery delay), whether the tick's callback has run or not, and never before
  (`FrameSteadyStart::grid`, default on; off is ADR-5461's release when the callback is seen). A tick is predicted
  only one slot past the newest real tick. A tick that is not a whole number of periods after the last one moves the
  frame's slot onto the new grid (`PacingModel::set_grid_follow`: a 120 Hz panel that gives the app 60 Hz ticks moves
  them by half a period). The runner drains a full display queue after 2 held late starts in a row
  (`FrameSteadyStart::drain_run`; ADR-5564's 8 for a new model). `FrameSteadyStart::start_offset_ns` (default 0)
  moves the start against the tick's due time. A change of refresh rate keeps the slot; where that would round it
  up to a longer one (25 ms of 120 Hz ticks is 33 ms at 60 Hz), the model takes the shorter slot as an ADR-2339 probe
  and goes back only if the frames do not fit.
- **Control.** `FramePacingSettings { mode, cost }` (`Auto`, `Off`, `Interval(n)`; `Work`, `Frame`),
  `pacing.get`, `pacing.set`, `pacing.simulate`, `pacing.trace` (the last 1024 frames as the pacer saw them);
  `FramePacingReport` holds the numbers.

## 4. Virtual file system

`Vfs` resource with mount points. The table is where content lives; cooked assets are read by
`regia_asset::AssetServer` (dev cooked cache and shipping paks), not through `Vfs`. The `Vfs`
`assets://` mount on desktop is `$REGIA_ASSETS`, else `<exe dir>/assets`, else
`$CARGO_MANIFEST_DIR/assets`; `config://` is its `config` folder. `Vfs::mount_pak(point, pak)` mounts a `.fpak`
read-only: each manifest path of the pak is a file (its stored bytes, decompressed; a leading `assets/` segment is
dropped, so a game pak mounted at `assets://` serves `assets://textures/rock.ktx2`), path prefixes are directories.
The reader lives in `regia_asset`, which installs it as `regia_platform`'s pak opener (`AssetPlugin`, or
`regia_asset::install_vfs_pak_mounts`); without it `mount_pak` returns `Unsupported`, and a file that is not a pak is
`VfsError::PakOpen` (ADR-7232). Agent methods: `window.get`/`window.set` and `vfs.mounts`/`exists`/`list`/`read`/
`mount_pak` (`book/agents/window-and-vfs.md`).

| Mount | Desktop dev | Desktop shipping | Android |
|---|---|---|---|
| `assets://` | `<project>/.regia/cooked/<platform>/` | `<exe dir>/content/*.fpak` | APK `assets/content/*.fpak` |
| `config://` | `<project>/config/` (cooked copy) | inside pak | inside pak |
| `user://` | `dirs::data_dir()/RegiaEngine/<exe stem>/` (ADR-2252) | same | internal storage |
| `cache://` | `dirs::cache_dir()/...` | same | cache dir |
| `logs://` | `user://logs` | same | same |

API: `Vfs::read(path) -> impl Future<Output = Result<Arc<[u8]>>>`, `read_sync`, `open_stream` (seekable
reader), `write_atomic` (temp file + rename; `user://` and `cache://` only), `exists`, `list_dir`.
`Vfs::read` calls `read_sync` before it returns, so the future is already complete and the read
runs on the caller, including a frame that only constructed the future. `open_stream`,
`write_atomic`, `exists` and `list_dir` run on the caller as well. This crate does not schedule
VFS IO on an `IoPool` (ADR-5070). Path type `VfsPath` validates mount + normalised relative path (no `..`).
Symbolic links (audit v0.2.12): `user://`, `cache://` and `logs://` refuse any path through a symbolic link
(on Windows, any reparse point) below their root with `PermissionDenied`, and `write_atomic` creates its temp
file exclusively, so a link planted in a save or cache folder cannot move reads or writes outside it. The root
itself may be a link. Packaged `assets://` and `config://` follow links (`DirMount::without_symlinks` opts a
read-only mount in).

## 5. Misc platform services

`Clipboard` (desktop: arboard; Android: JNI ClipboardManager, Tier 2), `open_url`, `system_locale()`,
`OsInfo` (os, version, device model, total RAM, CPU count), `PowerState` (Tier 2, not implemented), the
title (`Window::title`, applied by `apply_window_changes`; there is no separate `set_title`) and
`request_user_attention` (a `WindowHandles` method) (ADR-5610).

## 6. Input — raw devices (`regia_input`)

Resources updated in `PreUpdate`:
- `Keyboard`: three sets of physical `KeyCode` names (`held`, `just_pressed`, `just_released`), filled
  from `KeyboardInput.physical_key`. Logical keys stay on that message (`logical_key`) and are not stored
  on the resource. `pressed`, `just_pressed` and `just_released` take the physical name.
- `Mouse`: buttons, position (window, physical px as the platform reports; divide by the window scale for logical), delta (raw device motion, accumulated whether or not the cursor is locked),
  wheel (lines/pixels normalised).
- `Touches`: active touches with id, start/current/previous position, phase, pressure (always 1.0 for now: the platform event carries none); gesture
  recogniser (tap, double tap, long press, swipe, pinch, two-finger pan) emitting `GestureEvent`.
- `Gamepads`: per-pad `GamepadId`, name, standard layout buttons/axes (`South/East/West/North, L1/R1,
  L2/R2 analog, sticks, dpad, Start/Select/Mode`), connection events, rumble.
  - Desktop backend: gilrs.
  - Android backend (`poll_android_gamepads`, winit 0.30): device discovery polls `InputManager`.
    Buttons arrive as `KeyboardInput` whose physical key is `Unidentified(Android(<akeycode>))` and are
    applied to the first connected pad, because winit does not expose the Android device id. The left
    stick is `AXIS_X`/`AXIS_Y` from joystick-shaped touch reports, with Y flipped up-positive. The right
    stick, hat and analog L2/R2 are not reachable through winit 0.30. Triggers are digital buttons
    (akeycode 104/105 map to `LeftTrigger`/`RightTrigger`), not analog axes. The D-pad is not mapped to
    pad buttons (it arrives as arrow keys). Rumble uses `InputDevice.getVibrator()`
    (`regia_platform::gamepad_rumble`). Desktop gilrs still has the full layout.
- Mouse emulation from touch is **off** by default (games opt in).

## 7. Input — action mapping

Data-driven, edited in the editor (Project Settings → Input), stored in `config/input.ron`.

```ron
InputConfig(
    actions: {
        "Move": Axis2D,
        "Look": Axis2D,
        "Jump": Button,
        "Fire": Button,
    },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Move", input: Keys2D(up: "KeyW", down: "KeyS", left: "KeyA", right: "KeyD")),
                (action: "Move", input: GamepadStick(Left), modifiers: [DeadZone((mode: Radial, lo: 0.15, hi: 0.95))]),
                (action: "Look", input: MouseDelta, modifiers: [Scale((0.1, 0.1, 0.0))]),
                (action: "Jump", input: Key("Space")),
                (action: "Fire", input: MouseButton(Left), triggers: [Pressed]),
            ],
        ),
        "Menu": (priority: 10, consume_lower: true, bindings: []),
    },
)
```

`InputConfig::from_ron` accepts this text. Action values are unit variants. Keys are quoted strings.
`DeadZone` is a struct and `Scale` is a `Vec3`.

- Action value types: `Button(bool)`, `Axis1D(f32)`, `Axis2D(Vec2)`, `Axis3D(Vec3)`.
- Modifiers (applied in order): `DeadZone` (`mode`, `lo`, `hi`), `Scale` (a `Vec3`), `Negate { x, y, z }`,
  `Swizzle`, `InvertY(bool)`, `Smooth(t)`, `ResponseCurve`, `ScaleByDelta` (for per-second inputs).
  The fenced sample above is the RON shape `InputConfig::from_ron` accepts.
- Triggers are per binding (ADR-5517, recording the input-correctness fix 28ce61ef7 that ADR-5456 kept).
  Each binding runs its own trigger list against its own post-modifier value, with its own previous
  actuation as the edge; an empty list is `Down`. The action's phase is the strongest of its bindings'
  phases (`Triggered`, then `Started`, `Ongoing`, `Completed`, `Canceled`, `Idle`), so one binding ending
  does not end a sibling that is still held; an action that was already active reports `Ongoing`, not a
  second `Started`. The action's value still combines the bindings (a button keeps
  the last non-zero value, axes add; a zero value contributes nothing). A `Pressed` on one binding and an
  empty list on a sibling means the sibling is a `Down` binding: holding it holds the action, and a press
  of the `Pressed` binding still fires while the sibling is down. A `Hold` or `Tap` on one binding sees only
  that binding's source and times that binding's own press (ADR-5599). An injected value, or an assist
  that changes whether the action is down, is one value for the whole action, and every binding's
  triggers see it.
  The kinds are `Down`, `Pressed`, `Released`, `Hold(secs)`, `Tap(max_secs)`, `Pulse(interval)` and
  `Chord(other_action)`. Each action reports
  `ActionState { value, phase: Started|Ongoing|Triggered|Completed|Canceled|Idle, elapsed }`.
  `Chord` reads the partner's previous-frame `actuated` flag, which is set only when that phase was
  `Started`, `Ongoing` or `Triggered` and the value was non-zero. A partner bound with `Pressed` is
  actuated for one frame, so a chord on a later frame stays `Idle`. Losing the partner ends the chord
  as `Idle`, not `Completed`.
- Contexts: a stack of enabled contexts per player; higher priority evaluates first;
  `consume_lower` blocks lower contexts from seeing inputs it bound.
- Per-player device assignment: the `PlayerInputs` resource maps a player index (`u8`) to a
  `DeviceAssignment` (`Any` (player 0's default) | `KeyboardMouse` | `Gamepad(id)` | `Touch`; touch
  sources arrive through injection) (ADR-5610).
- API: `actions.pressed("Jump")`, `actions.axis2d("Move")`, typed alternative with
  a typed `#[derive(InputAction)]` enum (ADR-7070): a fieldless enum, one action per variant, `#[action(name = "..",
  kind = Axis2D)]` to rename or change the kind; `InputConfig::declare::<A>()` declares them and
  `actions.pressed_action(A::Jump)` reads them.
- Evaluation cadence (ADR-4768): actions are evaluated in `FixedUpdate`, once per fixed step with that step's
  delta, so triggers, holds, taps, pulses, smoothing and assists advance in simulation time and an edge is true
  for exactly one fixed step. `FixedUpdate` systems reading actions order `after(InputSet::Evaluation)`. On a
  frame whose fixed loop runs no step (a fast display banking less than one step, a paused app), a `PreUpdate`
  fallback evaluates once with a zero delta, so `Update` readers and one-frame injections still see the frame.
  Edge views: `just_pressed` is the per-fixed-step view (simulation); `just_pressed_this_frame` is the
  frame-level view for once-per-frame readers (UI, quicksave, agents) — on a multi-step frame the step-level
  phase is consumed by the first step, but the frame-level flag survives until the end of the frame.
- Rebinding: `InputRebinder` helper (listen for next input, conflict detection), persisted to
  `user://input_overrides.ron`, merged over the project config. A capture can be limited to one
  device slot, axes take a dead zone and invert, and named profiles live in
  `user://input_profiles/` (ADR-2129).
- `LastUsedDevice` resource for showing correct button prompts. The `InputGlyph` table asset
  (`assets://input/project.glyphs.ron`, `docs/formats/input-glyphs-ron.md`, ADR-7070) gives the glyph per action per
  device family; lookups fall back to the built-in `regia_ui::hints::HintDevice::glyph` families (keyboard/mouse,
  Xbox, PlayStation, Steam Deck) for the hint actions, then to the action's binding named per family. Agent:
  `input.glyph {action, device?}`.
- UI routing (ADR-7070): while the runtime UI holds the pointer, `UiPointerGate` hides mouse-button bindings from
  every context but `Menu`; the `Menu` context (`InputConfig::with_menu_context`) drives UI navigation while enabled.
- Touch controls: virtual joystick and buttons are `regia_ui` widgets that write to actions
  (`VirtualStick { action: "Move" }`, `VirtualButton { action: "Jump" }`): while a finger is on the
  stick it holds its action at the deflection (x right, y up), and a pressed button holds its action
  at `1.0`, through `InputInjection::hold`; lifting the finger releases it (ADR-5610).

## 8. Acceptance (M1 platform, M7 input)
- M1: `samples/hello_window` clears the screen with a color driven by elapsed time on all 3 platforms
  (Linux, Windows, Android);
  logs every input event; Android survives 50 home/resume cycles and 10 rotation changes (if sensor
  orientation) with no crash or leak (`adb shell dumpsys meminfo` stable within 5%).
- M7: `samples/input_lab` shows live action values for keyboard/mouse, two gamepads, and touch;
  rebinding UI persists across restarts; Android gamepad (any Bluetooth controller) works.

## 9. Keyboard layout presets (v0.6 P-15, ADR-8034)

`KeyboardLayout` provides explicit QWERTY, original Colemak and classic French
AZERTY labels for the 30 physical letter-block keys. `physical_key` resolves a
label and `key_label` supplies its inverse; unknown values return `None`. `keys`
enumerates rows in physical order. `movement_keys` returns the same physical
W/S/A/D up/down/left/right source on every layout (labels W/R/A/S on Colemak,
Z/S/Q/D on AZERTY). Hosts choose actions and preserve user overrides as before.
No event translation, auto-detection, global setting or serialized format is added.
Backspace/CapsLock remaps, modifiers, digit rows, shifted characters and national
layout variants remain explicit host bindings. `input.keyboard_layout {layout}`
exposes the presets read-only; see `book/agents/keyboard-layout-presets.md`.

Acceptance: Colemak and AZERTY label-to-physical mappings; bijection over each
letter block; invalid labels refused; physical press/release drives actions even
when logical labels change; agent dispatcher parity and invalid-parameter errors.
