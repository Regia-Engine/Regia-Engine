// SPDX-License-Identifier: MPL-2.0
//! Window and raw input messages (`docs/specs/11-platform-and-input.md` § 1). The action-mapping layer
//! in `regia_input` (M7) consumes the raw input messages; M1 only delivers them.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regia_app::prelude::*;

/// When an input event reached the engine: time since the process's input epoch (the first
/// timestamp taken), on the monotonic clock.
///
/// The window runner stamps each event as the OS event loop hands it over, before the frame that
/// consumes it, so `frame start - timestamp` is the input's queueing latency and successive touch
/// stamps give gesture velocity independent of the frame rate. winit 0.30 does not expose the OS's
/// own event time, so this is the receipt time (ADR-106). Synthetic events (tests, agent
/// injection) use [`InputTimestamp::now`] or [`InputTimestamp::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct InputTimestamp(pub Duration);

impl InputTimestamp {
    /// The epoch itself; also the stamp of synthetic events that do not care.
    pub const ZERO: Self = Self(Duration::ZERO);

    /// Now, relative to the input epoch.
    pub fn now() -> Self {
        Self::at(Instant::now())
    }

    /// `instant` relative to the input epoch (instants before the epoch clamp to zero).
    pub fn at(instant: Instant) -> Self {
        Self(instant.saturating_duration_since(input_epoch()))
    }

    /// Time since the input epoch.
    pub fn since_epoch(self) -> Duration {
        self.0
    }

    /// Whole microseconds since the input epoch.
    pub fn as_micros(self) -> u64 {
        u64::try_from(self.0.as_micros()).unwrap_or(u64::MAX)
    }
}

/// The instant [`InputTimestamp`]s count from: fixed by the first call (the platform plugin calls it
/// at build time).
pub fn input_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The input timestamps consumed by the current frame, gathered in [`PreUpdate`] from every raw
/// input message (keyboard, mouse buttons, cursor, motion, wheel, touch).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct InputTiming {
    /// When the current frame started ([`First`]), relative to the input epoch.
    pub frame_start: InputTimestamp,
    /// Stamps of this frame's input events, sorted, zero stamps skipped.
    pub events: Vec<InputTimestamp>,
    /// The newest stamp seen so far (any frame).
    pub latest: Option<InputTimestamp>,
}

impl InputTiming {
    /// Oldest input of this frame to frame start: the worst queueing latency this frame.
    pub fn max_latency(&self) -> Option<Duration> {
        self.events
            .first()
            .map(|oldest| self.frame_start.0.saturating_sub(oldest.0))
    }
}

/// The window's surface size changed (physical pixels).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct WindowResized {
    /// Window entity.
    pub window: Entity,
    /// New width in physical pixels.
    pub width: u32,
    /// New height in physical pixels.
    pub height: u32,
}

/// The window's scale factor changed.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct WindowScaleFactorChanged {
    /// Window entity.
    pub window: Entity,
    /// New scale factor.
    pub scale_factor: f64,
}

/// The window gained or lost focus.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFocused {
    /// Window entity.
    pub window: Entity,
    /// True when focused.
    pub focused: bool,
}

/// The OS light/dark appearance: written when the window is created (if the OS says) and whenever
/// it changes. Windows and macOS report live changes through winit; Android reads the night-mode
/// bit of the activity configuration at window creation, on every resume and on every
/// configuration change; Linux reports none yet. Only a change from the last report is written. `regia_ui` follows it with
/// `ThemeMode { follow: System }`.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemThemeChanged {
    /// Window entity.
    pub window: Entity,
    /// True for a dark appearance, false for light.
    pub dark: bool,
}

/// The user asked to close the window. Closing the primary window exits the app unless
/// [`ClosePolicy::exit_on_primary_close`] is turned off (games can show a "quit?" dialog instead).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowCloseRequested {
    /// Window entity.
    pub window: Entity,
}

/// A file was dropped on the window (desktop).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct FileDropped {
    /// Window entity.
    pub window: Entity,
    /// Dropped file.
    pub path: PathBuf,
}

/// Text input from the input method.
#[derive(Message, Debug, Clone, PartialEq)]
pub enum ImeEvent {
    /// IME turned on for the window.
    Enabled(Entity),
    /// Composition in progress.
    Preedit(Entity, String),
    /// The input method's cursor inside the composition just sent with [`ImeEvent::Preedit`], as
    /// a byte range into that string (`None`: hidden, shown at the end). Sent right after the
    /// `Preedit` it belongs to; a new `Preedit` resets it (ADR-7208).
    PreeditCursor(Entity, Option<(usize, usize)>),
    /// Committed text.
    Commit(Entity, String),
    /// IME turned off.
    Disabled(Entity),
}

/// Android back button (or gesture). Unless [`ClosePolicy::back_closes_primary`] is turned off it
/// becomes a [`WindowCloseRequested`] for the primary window.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackPressed;

/// What the platform layer does with close requests and the back button.
#[derive(Resource, Debug, Clone, Copy)]
pub struct ClosePolicy {
    /// Exit when the primary window is asked to close.
    pub exit_on_primary_close: bool,
    /// Treat the back button as a close request for the primary window.
    pub back_closes_primary: bool,
}

impl Default for ClosePolicy {
    fn default() -> Self {
        Self {
            exit_on_primary_close: true,
            back_closes_primary: true,
        }
    }
}

/// What [`request_quit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuitRequest {
    /// The window a [`WindowCloseRequested`] was sent for: the primary window, else any window
    /// (`None` headless).
    pub window: Option<Entity>,
    /// Whether the app exits at the end of this frame ([`ClosePolicy::exit_on_primary_close`]);
    /// when false the game decides (for example by showing a "quit?" dialog).
    pub will_exit: bool,
}

/// Asks the app to quit the way closing the primary window does, so [`ClosePolicy`] applies: a
/// [`WindowCloseRequested`] is sent for the primary window, and the app exits at the end of the
/// frame only if [`ClosePolicy::exit_on_primary_close`] is on. When no window is marked
/// `PrimaryWindow` but some `Window` exists, the close request goes to that window and the app
/// does not exit by itself (`will_exit` is false: only the primary window's close exits, so the
/// game's close handler decides). With no window at all (headless) it writes
/// [`AppExit::Success`] directly when the policy allows exiting.
pub fn request_quit(world: &mut World) -> QuitRequest {
    let policy = world.get_resource::<ClosePolicy>().copied().unwrap_or_default();
    let primary = world
        .query_filtered::<Entity, With<crate::PrimaryWindow>>()
        .iter(world)
        .next();
    let window = primary.or_else(|| {
        world
            .query_filtered::<Entity, With<crate::Window>>()
            .iter(world)
            .next()
    });
    let will_exit = policy.exit_on_primary_close && (primary.is_some() || window.is_none());
    match window {
        Some(window) => {
            world.write_message(WindowCloseRequested { window });
        }
        None if policy.exit_on_primary_close => {
            world.write_message(AppExit::Success);
        }
        None => {}
    }
    tracing::info!(
        target: "regia_platform",
        "quit requested (exit on close: {})",
        policy.exit_on_primary_close
    );
    QuitRequest { window, will_exit }
}

/// Pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    /// Went down.
    Pressed,
    /// Went up.
    Released,
}

/// A keyboard key event.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct KeyboardInput {
    /// Window entity.
    pub window: Entity,
    /// Layout-independent key name (physical position), e.g. `KeyW`.
    pub physical_key: String,
    /// Layout-dependent key: the text it produces, or a named key such as `Enter`.
    pub logical_key: String,
    /// Down or up.
    pub state: ButtonState,
    /// Auto-repeat.
    pub repeat: bool,
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

/// Mouse buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MouseButton {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Middle.
    Middle,
    /// Back (side).
    Back,
    /// Forward (side).
    Forward,
    /// Any other button.
    Other(u16),
}

/// A mouse button event.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButtonInput {
    /// Window entity.
    pub window: Entity,
    /// Button.
    pub button: MouseButton,
    /// Down or up.
    pub state: ButtonState,
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

/// The cursor moved over a window (physical pixels from the top-left).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct CursorMoved {
    /// Window entity.
    pub window: Entity,
    /// Position.
    pub position: (f64, f64),
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

/// Raw mouse movement, independent of the cursor (mouse-look).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct MouseMotion {
    /// Movement since the last event, in device units.
    pub delta: (f64, f64),
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

/// Units of a wheel event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelUnit {
    /// Lines or notches.
    Line,
    /// Pixels (touchpads).
    Pixel,
}

/// Mouse wheel or touchpad scroll.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct MouseWheel {
    /// Window entity.
    pub window: Entity,
    /// Horizontal and vertical amount.
    pub delta: (f32, f32),
    /// Units of `delta`.
    pub unit: WheelUnit,
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

/// Phase of a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    /// Finger down.
    Started,
    /// Finger moved.
    Moved,
    /// Finger up.
    Ended,
    /// Touch cancelled by the system.
    Cancelled,
}

/// A touch event.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct TouchInput {
    /// Window entity.
    pub window: Entity,
    /// Finger id, stable for the duration of the touch.
    pub id: u64,
    /// Phase.
    pub phase: TouchPhase,
    /// Position in physical pixels from the top-left.
    pub position: (f64, f64),
    /// When the event reached the engine ([`InputTimestamp`]).
    pub timestamp: InputTimestamp,
}

pub(crate) fn register(app: &mut App) {
    app.add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<WindowFocused>()
        .add_message::<SystemThemeChanged>()
        .add_message::<WindowCloseRequested>()
        .add_message::<FileDropped>()
        .add_message::<ImeEvent>()
        .add_message::<BackPressed>()
        .add_message::<KeyboardInput>()
        .add_message::<MouseButtonInput>()
        .add_message::<CursorMoved>()
        .add_message::<MouseMotion>()
        .add_message::<MouseWheel>()
        .add_message::<TouchInput>()
        .init_resource::<ClosePolicy>()
        .add_plugins(InputTimingPlugin);
}

/// Registers the raw input messages and keeps [`InputTiming`] up to date. Part of
/// [`PlatformPlugin`](crate::PlatformPlugin); add it alone in headless apps and tests that feed
/// input messages without a window.
pub struct InputTimingPlugin;

impl Plugin for InputTimingPlugin {
    fn build(&self, app: &mut App) {
        input_epoch();
        app.add_message::<KeyboardInput>()
            .add_message::<MouseButtonInput>()
            .add_message::<CursorMoved>()
            .add_message::<MouseMotion>()
            .add_message::<MouseWheel>()
            .add_message::<TouchInput>()
            .init_resource::<InputTiming>()
            .add_systems(First, start_input_frame)
            .add_systems(PreUpdate, collect_input_timing);
    }
}

/// `First`: stamps the frame start and forgets the previous frame's input stamps.
fn start_input_frame(mut timing: ResMut<InputTiming>) {
    timing.frame_start = InputTimestamp::now();
    timing.events.clear();
}

/// `PreUpdate`: gathers the stamps of this frame's raw input messages into [`InputTiming`].
fn collect_input_timing(
    mut timing: ResMut<InputTiming>,
    mut keys: MessageReader<KeyboardInput>,
    mut buttons: MessageReader<MouseButtonInput>,
    mut cursor: MessageReader<CursorMoved>,
    mut motion: MessageReader<MouseMotion>,
    mut wheel: MessageReader<MouseWheel>,
    mut touch: MessageReader<TouchInput>,
) {
    let stamps = keys
        .read()
        .map(|e| e.timestamp)
        .chain(buttons.read().map(|e| e.timestamp))
        .chain(cursor.read().map(|e| e.timestamp))
        .chain(motion.read().map(|e| e.timestamp))
        .chain(wheel.read().map(|e| e.timestamp))
        .chain(touch.read().map(|e| e.timestamp))
        .filter(|stamp| *stamp != InputTimestamp::ZERO);
    timing.events.extend(stamps);
    if timing.events.is_empty() {
        return;
    }
    timing.events.sort_unstable();
    let newest = timing.events.last().copied();
    timing.latest = timing.latest.max(newest);
}

/// `Last`: back button → close request → exit, according to [`ClosePolicy`].
pub(crate) fn handle_close_requests(
    policy: Res<ClosePolicy>,
    mut back: MessageReader<BackPressed>,
    mut close: ParamSet<(
        MessageWriter<WindowCloseRequested>,
        MessageReader<WindowCloseRequested>,
    )>,
    primary: Query<Entity, With<crate::PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    let primary = primary.iter().next();
    // Read Back every frame, whatever the policy. A Back written after `First` (an agent's
    // `play.inject_input`, or a game system) is still buffered in the next frame; a reader that
    // skipped it while the policy was off would act on it there, once a screen that held the policy
    // (the first-person template's touch Controls screen) gives it back: the Back that closed the
    // screen would then quit the game.
    let backed = back.read().count() > 0;
    if policy.back_closes_primary
        && backed
        && let Some(window) = primary
    {
        close.p0().write(WindowCloseRequested { window });
    }
    if policy.exit_on_primary_close && close.p1().read().any(|r| Some(r.window) == primary) {
        tracing::info!(target: "regia_platform", "primary window close requested; exiting");
        exit.write(AppExit::Success);
    }
}
