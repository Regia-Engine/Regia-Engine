// SPDX-License-Identifier: MPL-2.0
//! The separate play window (`docs/specs/19-editor.md` § 4, ADR-7272): `editor.play { window: true }`
//! (or the toolbar's "▶ Play in window") shows the play world's game camera in a second OS window
//! instead of the viewport.
//!
//! The window is the editor's second winit window. It gets its own `wgpu::Surface` on the
//! editor's shared device, so a resize reconfigures only that surface and a lost surface is
//! recreated on the same window and device; no GPU resource of the editor is rebuilt. Its keyboard,
//! mouse and focus events go to the play world only (as the engine's `regia_platform` input
//! messages a game's `InputPlugin` reads); the editor window's events never reach it. A click in
//! the window captures the mouse and **Shift+F1** releases it, as in the viewport
//! ([`crate::play_input`]). Closing the window ends play; the edit world was never touched. The
//! window's size and position persist in the editor preferences (`play.window_*`).
//!
//! The logic is [`PlayWindow`], a plain state machine over a [`PlayWindowPlatform`]: the windowed
//! editor implements the platform over a real winit window and surface and translates winit events
//! into [`PlayWindowEvent`]s, and tests drive [`PlayWindow::on_event`] and [`PlayWindow::sync`] with
//! a recording platform, never a real window. A host with no window system (the headless editor,
//! CI) never inserts [`PlayWindowSupport`], so `editor.play { window: true }` answers
//! `play_window_unavailable` instead of starting anything.
//!
//! **Chosen resolution** (ADR-7283): the play world can render at a size of its own (a preset such
//! as a 9:16 phone screen, or a custom size) instead of the window's. The picture is scaled to fit
//! the window and centered ([`Letterbox`]); cursor positions are mapped back into the play world's
//! pixels, and a press or wheel in the bars around the picture reaches nothing. The game's
//! primary window reports the chosen size, so its UI lays out as on the target screen. The
//! choice is `play.resolution` (with `play.resolution_width` / `_height` for a custom size) in the
//! editor preferences, or `editor.play { window: true, preset | size }` for one session.
//!
//! **Split-screen** (ADR-7395): with 2 or more `players`, the picture composites one pane per
//! local player, each that player's window view drawn into its viewport rect of the play
//! resolution, exactly as the game's own window draws them ([`crate::render::render_play_window_gpu`]).
//! A play world without window cameras — a level or game that authored none — gets a PIE stand-in
//! camera per player ([`crate::pie::PiePlayerCamera`]); whoever authored a camera owns presentation.
//! Cursor mapping is unchanged: the panes tile the picture, so a window point maps to the same
//! play-pixel either way.
//!
//! The windowed editor's per-frame glue goes through [`PlayWindow::begin_frame`] (a lost surface
//! is recreated and play keeps going) and [`FrameDriver`] (the play window runs the editor frame
//! when the editor window's redraws stop), so tests cover it without a window.

use std::time::{Duration, Instant};

use regia_ecs::ecs::world::WorldId;
use regia_ecs::prelude::*;
use regia_platform::{ButtonState, InputTimestamp, MouseButton, WheelUnit};

use crate::editor_prefs::{EditorPreferences, PrefValue};
use crate::pie::{PieRequest, PieSession};
use crate::play_input::{CaptureEvent, PlayInputCapture};

/// The play window's title.
pub const PLAY_WINDOW_TITLE: &str = "Regia Engine Play";
/// The viewport-instance id of the play window's renderer (far above the ids the viewport-instance
/// registry and the PIE client views use, so the renderers never collide).
pub const PLAY_WINDOW_VIEW_ID: u64 = 0x504C_4159_5749_4E00;
/// The error reason of `editor.play { window: true }` on a host with no window system.
pub const PLAY_WINDOW_UNAVAILABLE: &str = "play_window_unavailable";
/// The message that goes with [`PLAY_WINDOW_UNAVAILABLE`].
pub const NO_WINDOW_SYSTEM: &str = "no window system: this editor host (headless or CI) cannot open a separate play window; play in the viewport";

/// Preference key: the play window's width in physical pixels.
pub const PREF_WIDTH: &str = "play.window_width";
/// Preference key: the play window's height in physical pixels.
pub const PREF_HEIGHT: &str = "play.window_height";
/// Preference key: whether the play window's position is remembered (else the window system places it).
pub const PREF_PLACED: &str = "play.window_placed";
/// Preference key: the play window's left edge on the desktop, physical pixels.
pub const PREF_X: &str = "play.window_x";
/// Preference key: the play window's top edge on the desktop, physical pixels.
pub const PREF_Y: &str = "play.window_y";
/// Preference key: the play resolution, a [`RESOLUTION_PRESETS`] id or `custom` (ADR-7283).
pub const PREF_RESOLUTION: &str = "play.resolution";
/// Preference key: the custom play resolution's width, pixels.
pub const PREF_RESOLUTION_WIDTH: &str = "play.resolution_width";
/// Preference key: the custom play resolution's height, pixels.
pub const PREF_RESOLUTION_HEIGHT: &str = "play.resolution_height";
/// The smallest play resolution side, pixels.
pub const MIN_RESOLUTION: u32 = 16;
/// The largest play resolution side, pixels.
pub const MAX_RESOLUTION: u32 = 16384;
/// The error reason of an `editor.play` `preset` or `size` that is unknown or out of range.
pub const PLAY_RESOLUTION_INVALID: &str = "play_resolution_invalid";
/// The error reason of an `editor.play` `preset` or `size` without `window: true` (the viewport
/// renders at its own size).
pub const PLAY_RESOLUTION_NEEDS_WINDOW: &str = "play_resolution_needs_window";
/// The preset id of a size no preset has.
pub const CUSTOM_RESOLUTION: &str = "custom";
/// How long the editor window may go without a frame before the play window runs it
/// ([`FrameDriver`]).
pub const EDITOR_STALL: Duration = Duration::from_millis(100);

/// The size the play world renders at in the play window (ADR-7283).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlayResolution {
    /// The window's own size (the picture fills the window).
    #[default]
    Window,
    /// A fixed size, scaled to fit the window with bars around it.
    Fixed {
        /// Width, pixels.
        width: u32,
        /// Height, pixels.
        height: u32,
    },
}

/// A named play resolution (the toolbar's size menu and `editor.play`'s `preset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionPreset {
    /// The id (`phone_9_16_portrait`).
    pub id: &'static str,
    /// The menu text.
    pub label: &'static str,
    /// Its size.
    pub resolution: PlayResolution,
}

const fn fixed(width: u32, height: u32) -> PlayResolution {
    PlayResolution::Fixed { width, height }
}

/// The play resolution presets, in menu order.
pub const RESOLUTION_PRESETS: &[ResolutionPreset] = &[
    ResolutionPreset {
        id: "window",
        label: "Window size",
        resolution: PlayResolution::Window,
    },
    ResolutionPreset {
        id: "hd_720",
        label: "1280 × 720 (16:9)",
        resolution: fixed(1280, 720),
    },
    ResolutionPreset {
        id: "hd_1080",
        label: "1920 × 1080 (16:9, phone 9:16 landscape)",
        resolution: fixed(1920, 1080),
    },
    ResolutionPreset {
        id: "phone_9_16_portrait",
        label: "Phone 9:16 portrait (1080 × 1920)",
        resolution: fixed(1080, 1920),
    },
    ResolutionPreset {
        id: "phone_9_19_5_portrait",
        label: "Phone 9:19.5 portrait (1080 × 2340)",
        resolution: fixed(1080, 2340),
    },
    ResolutionPreset {
        id: "phone_9_19_5_landscape",
        label: "Phone 19.5:9 landscape (2340 × 1080)",
        resolution: fixed(2340, 1080),
    },
    ResolutionPreset {
        id: "tablet_3_4_portrait",
        label: "Tablet 3:4 portrait (1536 × 2048)",
        resolution: fixed(1536, 2048),
    },
    ResolutionPreset {
        id: "tablet_3_4_landscape",
        label: "Tablet 4:3 landscape (2048 × 1536)",
        resolution: fixed(2048, 1536),
    },
];

/// Every value `play.resolution` takes: the preset ids and `custom`.
pub const RESOLUTION_CHOICES: &[&str] = &[
    "window",
    "hd_720",
    "hd_1080",
    "phone_9_16_portrait",
    "phone_9_19_5_portrait",
    "phone_9_19_5_landscape",
    "tablet_3_4_portrait",
    "tablet_3_4_landscape",
    CUSTOM_RESOLUTION,
];

impl PlayResolution {
    /// The preset called `id`.
    #[must_use]
    pub fn preset(id: &str) -> Option<Self> {
        RESOLUTION_PRESETS
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.resolution)
    }

    /// A custom size; `None` when a side is outside `MIN_RESOLUTION..=MAX_RESOLUTION`.
    #[must_use]
    pub fn custom(width: u32, height: u32) -> Option<Self> {
        let range = MIN_RESOLUTION..=MAX_RESOLUTION;
        (range.contains(&width) && range.contains(&height)).then_some(Self::Fixed { width, height })
    }

    /// The id of the first preset with this size, else [`CUSTOM_RESOLUTION`].
    #[must_use]
    pub fn preset_id(self) -> &'static str {
        RESOLUTION_PRESETS
            .iter()
            .find(|p| p.resolution == self)
            .map_or(CUSTOM_RESOLUTION, |p| p.id)
    }

    /// The fixed size, `None` for [`PlayResolution::Window`].
    #[must_use]
    pub fn size(self) -> Option<[u32; 2]> {
        match self {
            Self::Window => None,
            Self::Fixed { width, height } => Some([width, height]),
        }
    }

    /// The size the play world renders at in a window of `window` pixels.
    #[must_use]
    pub fn render_size(self, window: [u32; 2]) -> [u32; 2] {
        self.size().unwrap_or(window)
    }

    /// The resolution the preferences hold ([`PlayResolution::Window`] when unset).
    #[must_use]
    pub fn from_prefs(prefs: &EditorPreferences) -> Self {
        let choice = prefs.text(PREF_RESOLUTION);
        if choice == CUSTOM_RESOLUTION {
            let side = |key: &str| u32::try_from(prefs.int(key)).unwrap_or(0);
            return Self::custom(side(PREF_RESOLUTION_WIDTH), side(PREF_RESOLUTION_HEIGHT))
                .unwrap_or_default();
        }
        Self::preset(&choice).unwrap_or_default()
    }

    /// Writes the resolution into `prefs` (a custom size also writes its sides); whether
    /// anything changed.
    pub fn store(self, prefs: &mut EditorPreferences) -> bool {
        let mut changed = false;
        let mut set = |key: &str, value: PrefValue| {
            if let Ok(did) = prefs.set(key, &value) {
                changed |= did;
            }
        };
        let id = self.preset_id();
        if let (CUSTOM_RESOLUTION, Some([width, height])) = (id, self.size()) {
            set(PREF_RESOLUTION_WIDTH, PrefValue::Int(i64::from(width)));
            set(PREF_RESOLUTION_HEIGHT, PrefValue::Int(i64::from(height)));
        }
        set(PREF_RESOLUTION, PrefValue::Text(id.to_owned()));
        changed
    }
}

/// Where the play world's picture sits in the play window, physical pixels: scaled to fit with
/// its aspect ratio kept and centered, so bars fill the rest (letterbox or pillarbox).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Letterbox {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Letterbox {
    /// `picture` pixels fitted into `window` pixels (the whole window when either is empty).
    #[must_use]
    pub fn fit(picture: [u32; 2], window: [u32; 2]) -> Self {
        let [ww, wh] = window;
        let [pw, ph] = picture;
        if pw == 0 || ph == 0 || ww == 0 || wh == 0 {
            return Self {
                x: 0,
                y: 0,
                width: ww,
                height: wh,
            };
        }
        let scale = (f64::from(ww) / f64::from(pw)).min(f64::from(wh) / f64::from(ph));
        // Rounded to whole pixels; the fitted side is the window's exactly.
        let side = |p: u32, w: u32| ((f64::from(p) * scale).round() as u32).clamp(1, w);
        let width = side(pw, ww);
        let height = side(ph, wh);
        Self {
            x: (ww - width) / 2,
            y: (wh - height) / 2,
            width,
            height,
        }
    }

    /// `[x, y, width, height]`.
    #[must_use]
    pub fn to_array(self) -> [u32; 4] {
        [self.x, self.y, self.width, self.height]
    }

    /// A window point in the `picture`'s pixels; `None` in the bars.
    #[must_use]
    pub fn to_picture(self, x: f64, y: f64, picture: [u32; 2]) -> Option<(f64, f64)> {
        let lx = x - f64::from(self.x);
        let ly = y - f64::from(self.y);
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        if self.width == 0 || self.height == 0 || lx < 0.0 || ly < 0.0 || lx >= w || ly >= h {
            return None;
        }
        Some((lx * f64::from(picture[0]) / w, ly * f64::from(picture[1]) / h))
    }
}

/// What the windowed editor got when it asked the play window's surface for its next texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceAcquire {
    /// A texture to draw into (also a suboptimal one).
    Ready,
    /// The surface is lost or out of date: it has to be made again.
    Lost,
    /// No texture this time (timeout, occluded window): skip the frame.
    Unavailable,
}

/// What [`PlayWindow::begin_frame`] says to do with this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayFrame {
    /// Render the play world at `size` and draw it into `picture` (the rest of the window is
    /// cleared to black).
    Render {
        /// The play world's render size, pixels.
        size: [u32; 2],
        /// Where it goes in the window.
        picture: Letterbox,
    },
    /// Nothing to draw this frame.
    Skip,
}

/// Decides when the play window's redraw runs the editor frame itself: a compositor stops the
/// redraws of an editor window the play window covers, and play must keep stepping. Times are
/// passed in, so tests drive it with fixed instants.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDriver {
    last_editor_frame: Option<Instant>,
}

impl FrameDriver {
    /// The editor frame ran at `now` (from either window's redraw).
    pub fn editor_frame_ran(&mut self, now: Instant) {
        self.last_editor_frame = Some(now);
    }

    /// Whether the play window's redraw at `now` runs the editor frame: the play window is open
    /// and no editor frame ran for [`EDITOR_STALL`] (never before the editor's first frame).
    #[must_use]
    pub fn play_redraw_runs_frame(&self, now: Instant, play_window_open: bool) -> bool {
        play_window_open
            && self
                .last_editor_frame
                .is_some_and(|last| now.saturating_duration_since(last) >= EDITOR_STALL)
    }
}

/// Present in the edit world when the host has a window system and can open a play window (the
/// windowed editor inserts it once its window exists). Without it `editor.play { window: true }`
/// answers [`PLAY_WINDOW_UNAVAILABLE`].
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayWindowSupport;

/// The play window's size and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayWindowGeometry {
    /// Width, physical pixels.
    pub width: u32,
    /// Height, physical pixels.
    pub height: u32,
    /// The outer top-left corner on the desktop, physical pixels; `None` lets the window system
    /// place it (and on Wayland, where a client cannot read or set it).
    pub position: Option<[i32; 2]>,
}

impl Default for PlayWindowGeometry {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            position: None,
        }
    }
}

impl PlayWindowGeometry {
    /// The geometry the preferences hold (the defaults for unset keys; out-of-range values never
    /// get in, the preference check refuses them).
    #[must_use]
    pub fn from_prefs(prefs: &EditorPreferences) -> Self {
        let size = |key: &str, fallback: u32| {
            u32::try_from(prefs.int(key))
                .ok()
                .filter(|v| *v > 0)
                .unwrap_or(fallback)
        };
        let defaults = Self::default();
        let coordinate = |key: &str| i32::try_from(prefs.int(key)).unwrap_or(0);
        Self {
            width: size(PREF_WIDTH, defaults.width),
            height: size(PREF_HEIGHT, defaults.height),
            position: prefs
                .bool(PREF_PLACED)
                .then(|| [coordinate(PREF_X), coordinate(PREF_Y)]),
        }
    }

    /// Writes the geometry into `prefs`; whether anything changed. A size outside the preference's
    /// range (a tiny or huge window) keeps the stored value.
    pub fn store(&self, prefs: &mut EditorPreferences) -> bool {
        let mut changed = false;
        let mut set = |key: &str, value: PrefValue| {
            if let Ok(did) = prefs.set(key, &value) {
                changed |= did;
            }
        };
        set(PREF_WIDTH, PrefValue::Int(i64::from(self.width)));
        set(PREF_HEIGHT, PrefValue::Int(i64::from(self.height)));
        set(PREF_PLACED, PrefValue::Bool(self.position.is_some()));
        if let Some([x, y]) = self.position {
            set(PREF_X, PrefValue::Int(i64::from(x)));
            set(PREF_Y, PrefValue::Int(i64::from(y)));
        }
        changed
    }
}

/// One event of a window, in the engine's terms (the windowed editor translates winit's).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayWindowEvent {
    /// A key went down or up.
    Key {
        /// Layout-independent key name (`KeyW`, `F1`), as `regia_platform::KeyboardInput` names it.
        physical: String,
        /// Layout-dependent key: its text or a named key.
        logical: String,
        /// Down (`true`) or up.
        pressed: bool,
        /// Auto-repeat.
        repeat: bool,
        /// Shift held.
        shift: bool,
        /// Ctrl held.
        ctrl: bool,
        /// Alt held.
        alt: bool,
    },
    /// A mouse button went down or up.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Down (`true`) or up.
        pressed: bool,
    },
    /// The cursor moved, physical pixels from the window's top-left.
    CursorMoved {
        /// X.
        x: f64,
        /// Y (down).
        y: f64,
    },
    /// Raw mouse motion while the window holds the mouse (a device event; the editor only sends it
    /// while the play window captures).
    MouseMotion {
        /// X delta, device units.
        dx: f64,
        /// Y delta, device units.
        dy: f64,
    },
    /// The wheel turned.
    Wheel {
        /// Horizontal and vertical motion.
        delta: (f32, f32),
        /// Lines or pixels.
        unit: WheelUnit,
    },
    /// The window gained (`true`) or lost focus.
    Focused(bool),
    /// The window's inner size changed, physical pixels (0 x 0 while minimized).
    Resized {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// The window moved on the desktop, physical pixels.
    Moved {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
    },
    /// The window's surface was lost or went out of date (the GPU or the compositor dropped it).
    SurfaceLost,
    /// The user closed the window.
    CloseRequested,
}

/// What the play window needs from the windowing layer. The windowed editor implements it over a
/// winit window and its `wgpu::Surface`; tests record the calls.
pub trait PlayWindowPlatform {
    /// Opens the play window with `geometry`; returns its id (winit's `WindowId` as `u64`).
    ///
    /// # Errors
    ///
    /// Why the window system refused the window.
    fn open(&mut self, geometry: PlayWindowGeometry) -> Result<u64, String>;
    /// Reconfigures the window's surface at a new size (the device and every other GPU resource
    /// stay).
    fn configure_surface(&mut self, width: u32, height: u32);
    /// Drops the lost surface and makes a new one on the same window and the shared device.
    ///
    /// # Errors
    ///
    /// Why a new surface could not be made.
    fn recreate_surface(&mut self) -> Result<(), String>;
    /// Grabs and hides (`true`) or frees and shows the cursor in the play window.
    fn set_capture(&mut self, captured: bool);
    /// Closes the play window (its surface goes first).
    fn close(&mut self);
}

/// How many times the play window did what (`editor.play_state`'s `play_window.counters`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayWindowCounters {
    /// Windows opened.
    pub windows_opened: u64,
    /// Windows closed.
    pub windows_closed: u64,
    /// Surface reconfigurations (resizes).
    pub surface_configures: u64,
    /// Surfaces recreated after a loss.
    pub surface_recreations: u64,
    /// Play-window frames drawn and presented (ADR-7283).
    pub frames_presented: u64,
    /// Play-window frames skipped (lost, occluded or timed-out surface).
    pub frames_skipped: u64,
}

/// Where [`PlayWindow::on_event`] sent an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// Not the play window's: the editor handles it (its egui and shortcuts).
    Editor,
    /// The play window's: the play world got it (or the play window handled it: resize, Shift+F1).
    Play,
    /// The play window was closed and play ended.
    Closed,
    /// For a play window that is gone (late events after its close): dropped.
    Ignored,
    /// The play window's pointer event in the bars around a fixed-resolution picture: nothing
    /// got it (ADR-7283).
    Letterbox,
}

/// The play window's state for panels and agents (`editor.play_state`'s `play_window`), in the edit
/// world.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayWindowStatus {
    /// A play window is open.
    pub open: bool,
    /// Its size, physical pixels (the last one when closed).
    pub size: [u32; 2],
    /// Its desktop position, when known.
    pub position: Option<[i32; 2]>,
    /// The mouse is captured by it.
    pub captured: bool,
    /// Why the last play window could not open, when it could not.
    pub error: Option<String>,
    /// Its counters.
    pub counters: PlayWindowCounters,
    /// The chosen resolution (the open window's, else the preferences').
    pub resolution: PlayResolution,
    /// The play world's render size, pixels.
    pub render_size: [u32; 2],
    /// Where the picture sits in the window, `[x, y, width, height]` pixels.
    pub picture: [u32; 4],
}

/// The separate play window's state machine (see the module docs). The windowed editor holds one.
#[derive(Debug, Default)]
pub struct PlayWindow {
    /// The open window's id.
    id: Option<u64>,
    /// Every id this host opened (late events of a closed window are dropped, not given to the editor).
    known: Vec<u64>,
    /// The play world the window shows (its `World` id) and its window entity there.
    attached: Option<(WorldId, Entity)>,
    /// The play world whose window failed to open (not retried every frame).
    failed_for: Option<WorldId>,
    geometry: PlayWindowGeometry,
    capture: PlayInputCapture,
    counters: PlayWindowCounters,
    error: Option<String>,
    /// The open window's play resolution.
    resolution: PlayResolution,
    /// The last cursor position over the window, window pixels (to drop presses in the bars).
    cursor: Option<(f64, f64)>,
    /// The preferences' play resolution and the preference generation it was read at (read again
    /// only after a change, so a frame allocates nothing for it).
    prefs_resolution: Option<(u64, PlayResolution)>,
    /// Device simulation (ADR-7829): mouse-to-touch emulation, the virtual pad, Ctrl held, and the
    /// safe area this window gave the play world.
    touch: crate::play_device::TouchEmulator,
    pad: crate::play_device::VirtualPad,
    ctrl: bool,
    safe_area: Option<regia_platform::SafeArea>,
    /// Keyboard focus in the painted pad and its held activation key.
    pad_focus: Option<usize>,
    pad_key: Option<String>,
    pad_consumed_keys: Vec<String>,
    last_device: crate::play_device::DeviceSim,
}

impl PlayWindow {
    /// The open play window's id.
    #[must_use]
    pub fn id(&self) -> Option<u64> {
        self.id
    }

    /// Whether the play window holds the mouse.
    #[must_use]
    pub fn captured(&self) -> bool {
        self.capture.captured
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> PlayWindowCounters {
        self.counters
    }

    /// The window's current size and position.
    #[must_use]
    pub fn geometry(&self) -> PlayWindowGeometry {
        self.geometry
    }

    /// The open window's play resolution.
    #[must_use]
    pub fn resolution(&self) -> PlayResolution {
        self.resolution
    }

    /// The size the play world renders at: the chosen resolution, or the window's size.
    #[must_use]
    pub fn render_size(&self) -> [u32; 2] {
        self.resolution
            .render_size([self.geometry.width, self.geometry.height])
    }

    /// Where the picture sits in the window.
    #[must_use]
    pub fn picture(&self) -> Letterbox {
        Letterbox::fit(self.render_size(), [self.geometry.width, self.geometry.height])
    }

    /// Once per play-window frame, with what the surface gave: a ready surface renders (at
    /// [`PlayWindow::render_size`] into [`PlayWindow::picture`]); a lost one is made again on the
    /// same window and device and the frame is skipped (play keeps stepping; a failed
    /// recreation is in the status and is tried again on the next lost frame); an unavailable
    /// one is skipped. Call [`PlayWindow::presented`] after drawing.
    pub fn begin_frame(
        &mut self,
        acquire: SurfaceAcquire,
        edit: &mut World,
        session: &mut Option<PieSession>,
        platform: &mut dyn PlayWindowPlatform,
    ) -> PlayFrame {
        let Some(id) = self.id else {
            return PlayFrame::Skip;
        };
        let frame = match acquire {
            SurfaceAcquire::Ready if session.is_some() => PlayFrame::Render {
                size: self.render_size(),
                picture: self.picture(),
            },
            SurfaceAcquire::Lost => {
                self.on_event(id, PlayWindowEvent::SurfaceLost, edit, session, platform);
                PlayFrame::Skip
            }
            SurfaceAcquire::Ready | SurfaceAcquire::Unavailable => PlayFrame::Skip,
        };
        if frame == PlayFrame::Skip {
            self.counters.frames_skipped += 1;
            self.publish(edit);
        } else if let Some(session) = session.as_mut() {
            // A Frame Debugger capture of the play window (ADR-8224): its request goes to the play
            // world this frame renders, and a finished capture comes back to the editor.
            crate::frame_debugger::sync_play_window(edit, session.app.world_mut());
        }
        frame
    }

    /// A frame [`PlayWindow::begin_frame`] asked for was drawn and presented.
    pub fn presented(&mut self, edit: &mut World) {
        self.counters.frames_presented += 1;
        self.publish(edit);
    }

    /// Whether the cursor is over the bars around the picture.
    fn cursor_in_bars(&self) -> bool {
        self.cursor
            .is_some_and(|(x, y)| self.picture().to_picture(x, y, self.render_size()).is_none())
    }

    /// Once per editor frame, after the play requests were applied: opens the window for a session
    /// that asked for one (on a host with [`PlayWindowSupport`]), moves it to a restarted session's
    /// world, and closes it when play ended or the new session plays in the viewport. Publishes
    /// [`PlayWindowStatus`] into `edit`.
    pub fn sync(
        &mut self,
        edit: &mut World,
        session: Option<&mut PieSession>,
        platform: &mut dyn PlayWindowPlatform,
    ) {
        let supported = edit.contains_resource::<PlayWindowSupport>();
        match session {
            Some(session) if supported && session.options.window => {
                let world_id = session.app.world().id();
                if self.id.is_none() {
                    if self.failed_for != Some(world_id) {
                        self.open(edit, session, platform);
                    }
                } else if self.attached.map(|(id, _)| id) != Some(world_id) {
                    // Play restarted under the open window: show the new world.
                    self.resolution = self.chosen_resolution(edit, session);
                    self.attach(session);
                } else {
                    // A resolution chosen while the window is open (the toolbar's size menu)
                    // applies now: the game's window takes the new size and hears about it.
                    let wanted = self.chosen_resolution(edit, session);
                    if wanted != self.resolution {
                        self.resolution = wanted;
                        let [width, height] = self.render_size();
                        if let Some(entity) = self.entity() {
                            size_game_window(session.app.world_mut(), entity, width, height);
                        }
                    }
                }
                self.sync_device(edit, session);
                self.sync_pad_controls(edit, session);
            }
            _ => {
                if self.id.is_some() {
                    self.close(edit, platform);
                }
                self.failed_for = None;
            }
        }
        self.publish(edit);
    }

    /// Routes one event of window `window` (see [`Routed`]). Events of the open play window go to
    /// the play world; any other window's go to the editor.
    pub fn on_event(
        &mut self,
        window: u64,
        event: PlayWindowEvent,
        edit: &mut World,
        session: &mut Option<PieSession>,
        platform: &mut dyn PlayWindowPlatform,
    ) -> Routed {
        if self.id != Some(window) {
            return if self.known.contains(&window) {
                Routed::Ignored
            } else {
                Routed::Editor
            };
        }
        if self.device_key(&event, edit, session) {
            self.publish(edit);
            return Routed::Play;
        }
        let routed = match event {
            PlayWindowEvent::CloseRequested => {
                // Play ends first (the session's `on_unload` runs), then the window goes.
                let _ = crate::pie::apply_request(edit, session, PieRequest::Stop, None);
                crate::pie::sync_status(edit, session.as_ref());
                self.close(edit, platform);
                Routed::Closed
            }
            PlayWindowEvent::Resized { width, height } => {
                if width > 0 && height > 0 {
                    self.geometry.width = width;
                    self.geometry.height = height;
                    platform.configure_surface(width, height);
                    self.counters.surface_configures += 1;
                    // A fixed resolution keeps the game's window size; only the bars move.
                    let window_sized = self.resolution == PlayResolution::Window;
                    if let (Some(entity), Some(world)) =
                        (self.entity(), self.play_world(session).filter(|_| window_sized))
                    {
                        size_game_window(world, entity, width, height);
                    }
                }
                Routed::Play
            }
            PlayWindowEvent::Moved { x, y } => {
                self.geometry.position = Some([x, y]);
                Routed::Play
            }
            PlayWindowEvent::SurfaceLost => {
                match platform.recreate_surface() {
                    Ok(()) => {
                        self.counters.surface_recreations += 1;
                        self.error = None;
                    }
                    Err(err) => {
                        tracing::warn!(target: "regia_editor", "play window: the surface could not be recreated: {err}");
                        self.error = Some(err);
                    }
                }
                Routed::Play
            }
            PlayWindowEvent::Key {
                ref physical,
                pressed,
                shift,
                ctrl,
                alt,
                ..
            } if pressed && crate::play_input::is_release_key(physical == "F1", shift, ctrl, alt) => {
                // Shift+F1 is the editor's: it frees the mouse and never reaches the game.
                self.apply_capture(CaptureEvent::ReleaseKey, true, platform);
                Routed::Play
            }
            PlayWindowEvent::CursorMoved { x, y } if self.device(edit).touch => {
                // Touch emulation (ADR-7829): the cursor is a finger only while it is down.
                self.cursor = Some((x, y));
                let size = self.render_size();
                let at = self.picture().to_picture(x, y, size);
                match (at, self.entity(), self.play_world(session)) {
                    (Some(at), Some(entity), Some(world)) => {
                        let touches = self.touch.moved(at);
                        deliver_touches(world, entity, &touches);
                        Routed::Play
                    }
                    (None, ..) => Routed::Letterbox,
                    _ => Routed::Ignored,
                }
            }
            PlayWindowEvent::MouseButton {
                button: MouseButton::Left,
                pressed,
            } if self.device_input(edit, pressed) => self.device_press(pressed, edit, session),
            PlayWindowEvent::CursorMoved { x, y } => {
                self.cursor = Some((x, y));
                let size = self.render_size();
                match self.picture().to_picture(x, y, size) {
                    // In the picture: the play world gets the point in its own pixels.
                    Some((px, py)) => match (self.entity(), self.play_world(session)) {
                        (Some(entity), Some(world)) => {
                            deliver(world, entity, PlayWindowEvent::CursorMoved { x: px, y: py });
                            Routed::Play
                        }
                        _ => Routed::Ignored,
                    },
                    None => Routed::Letterbox,
                }
            }
            PlayWindowEvent::MouseButton { pressed: true, .. } | PlayWindowEvent::Wheel { .. }
                if self.cursor_in_bars() =>
            {
                // A press or the wheel over the bars reaches nothing (and captures nothing).
                Routed::Letterbox
            }
            event => {
                let active = session.is_some();
                match &event {
                    PlayWindowEvent::MouseButton { pressed: true, .. } => {
                        self.apply_capture(CaptureEvent::ViewportClicked, active, platform);
                    }
                    PlayWindowEvent::Focused(false) => {
                        self.apply_capture(CaptureEvent::Lost, active, platform);
                        // A finger or pad button down when the window loses focus lets go.
                        self.device_release_all(session);
                    }
                    PlayWindowEvent::Key {
                        physical,
                        pressed,
                        ctrl,
                        ..
                    } => {
                        self.ctrl = if physical.starts_with("Control") {
                            *pressed
                        } else {
                            *ctrl
                        };
                    }
                    _ => {}
                }
                match (self.entity(), self.play_world(session)) {
                    (Some(entity), Some(world)) => {
                        deliver(world, entity, event);
                        Routed::Play
                    }
                    _ => Routed::Ignored,
                }
            }
        };
        self.publish(edit);
        routed
    }

    /// The play world the window shows, when the session still is the attached one.
    fn play_world<'a>(&self, session: &'a mut Option<PieSession>) -> Option<&'a mut World> {
        let (attached, _) = self.attached?;
        session
            .as_mut()
            .map(|s| s.app.world_mut())
            .filter(|world| world.id() == attached)
    }

    fn entity(&self) -> Option<Entity> {
        self.attached.map(|(_, entity)| entity)
    }

    fn apply_capture(
        &mut self,
        event: CaptureEvent,
        session_active: bool,
        platform: &mut dyn PlayWindowPlatform,
    ) {
        if let Some(grab) = crate::play_input::on_event(&mut self.capture, session_active, event) {
            platform.set_capture(grab);
        }
    }

    fn open(&mut self, edit: &mut World, session: &mut PieSession, platform: &mut dyn PlayWindowPlatform) {
        let geometry = edit
            .get_resource::<EditorPreferences>()
            .map(PlayWindowGeometry::from_prefs)
            .unwrap_or_default();
        match platform.open(geometry) {
            Ok(id) => {
                self.resolution = self.chosen_resolution(edit, session);
                let [rw, rh] = self.resolution.render_size([geometry.width, geometry.height]);
                tracing::info!(target: "regia_editor", width = geometry.width, height = geometry.height, render_width = rw, render_height = rh, "play window opened");
                self.id = Some(id);
                self.cursor = None;
                self.known.push(id);
                self.geometry = geometry;
                self.capture = PlayInputCapture::default();
                self.counters.windows_opened += 1;
                self.error = None;
                self.attach(session);
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "play window refused, playing in the viewport: {err}");
                self.failed_for = Some(session.app.world().id());
                self.error = Some(err);
            }
        }
    }

    /// Gives the session's play world a primary window entity sized like the play window and the
    /// input messages the window writes (a game without `InputPlugin` has none registered).
    fn attach(&mut self, session: &mut PieSession) {
        let app = &mut session.app;
        app.add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseMotion>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::WindowFocused>()
            .add_message::<regia_platform::WindowResized>()
            // Device simulation (ADR-7829): emulated touches and the virtual pad.
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::GamepadConnected>()
            .add_message::<regia_platform::GamepadButtonChanged>();
        // A new play world has not met the virtual pad, and holds no touch.
        self.pad.reset();
        self.pad_focus = None;
        self.pad_key = None;
        self.pad_consumed_keys.clear();
        self.touch = crate::play_device::TouchEmulator::default();
        self.safe_area = None;
        let world = app.world_mut();
        let [width, height] = self.render_size();
        let size = (width, height);
        let existing = world
            .query_filtered::<Entity, With<regia_platform::PrimaryWindow>>()
            .iter(world)
            .next();
        let entity = match existing {
            Some(entity) => entity,
            None => world
                .spawn((
                    regia_platform::PrimaryWindow,
                    regia_platform::Window {
                        title: PLAY_WINDOW_TITLE.to_owned(),
                        ..regia_platform::Window::default()
                    },
                ))
                .id(),
        };
        if let Some(mut window) = world.get_mut::<regia_platform::Window>(entity) {
            window.physical_size = size;
        }
        self.attached = Some((world.id(), entity));
    }

    fn close(&mut self, edit: &mut World, platform: &mut dyn PlayWindowPlatform) {
        if let Some(prefs) = edit.get_resource_mut::<EditorPreferences>().as_deref_mut() {
            self.geometry.store(prefs);
        }
        if self.capture.captured {
            platform.set_capture(false);
        }
        platform.close();
        crate::render::release_instance_renderers(PLAY_WINDOW_VIEW_ID);
        tracing::info!(target: "regia_editor", "play window closed");
        edit.remove_resource::<crate::play_device::DeviceControlQueue>();
        edit.remove_resource::<crate::play_device::DeviceInputState>();
        self.pad.reset();
        self.pad_focus = None;
        self.pad_key = None;
        self.id = None;
        self.attached = None;
        self.cursor = None;
        self.capture = PlayInputCapture::default();
        self.counters.windows_closed += 1;
    }

    /// The device simulation settings (ADR-7829).
    fn device(&self, edit: &World) -> crate::play_device::DeviceSim {
        edit.get_resource::<crate::play_device::DeviceSim>()
            .copied()
            .unwrap_or_default()
    }

    /// Draws device markings onto the composed play surface, after its picture blit.
    /// This never writes into the game's render target. `native_scale` is the OS
    /// window's physical pixels per logical point, in addition to the UI preference.
    pub fn draw_device_overlay(
        &self,
        overlay: &mut crate::play_device::DeviceOverlay,
        device: &regia_render::RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        edit: &World,
        native_scale: f32,
    ) {
        let scale = crate::play_device::ui_scale(edit);
        overlay.draw(
            device,
            encoder,
            target,
            crate::play_device::OverlayFrame {
                sim: self.device(edit),
                picture: self.picture(),
                size: [self.geometry.width, self.geometry.height],
                scale,
                native_scale,
                held: self.pad.held(),
                focused: self.pad_focus,
                tokens: crate::theme::tokens(edit),
            },
        );
    }

    /// One state transition path for pointer, keyboard and agent pad input.
    fn set_pad(&mut self, world: &mut World, button: Option<regia_platform::GamepadButton>) {
        use regia_platform::{GamepadButtonChanged, GamepadConnected};
        if button == self.pad.held() {
            return;
        }
        if let Some(button) = self.pad.release() {
            world.write_message(GamepadButtonChanged {
                device: crate::play_device::VIRTUAL_PAD_DEVICE,
                button,
                state: ButtonState::Released,
                value: 0.0,
            });
        }
        if let Some(button) = button {
            let (_, announce) = self.pad.press_button(button);
            if announce {
                world.write_message(GamepadConnected {
                    device: crate::play_device::VIRTUAL_PAD_DEVICE,
                    name: "Regia virtual gamepad".to_owned(),
                });
            }
            world.write_message(GamepadButtonChanged {
                device: crate::play_device::VIRTUAL_PAD_DEVICE,
                button,
                state: ButtonState::Pressed,
                value: 1.0,
            });
        }
    }

    fn device_key(
        &mut self,
        event: &PlayWindowEvent,
        edit: &World,
        session: &mut Option<PieSession>,
    ) -> bool {
        use crate::canvas_keys::PlayDeviceKey;
        let PlayWindowEvent::Key {
            physical,
            pressed,
            repeat,
            shift,
            ctrl,
            alt,
            ..
        } = event
        else {
            return false;
        };
        // Consume every matching release, even after focus or modifier changes.
        if !pressed && let Some(i) = self.pad_consumed_keys.iter().position(|key| key == physical) {
            self.pad_consumed_keys.swap_remove(i);
            if self.pad_key.as_ref() == Some(physical) {
                self.pad_key = None;
                if let Some(world) = self.play_world(session) {
                    self.set_pad(world, None);
                }
            }
            return true;
        }
        if self.captured() || !self.device(edit).gamepad || *ctrl || *alt {
            return false;
        }
        let Some(key) = crate::canvas_keys::play_device_key(physical, self.pad_focus.is_some()) else {
            return false;
        };
        if !pressed {
            return false;
        }
        if !self.pad_consumed_keys.contains(physical) {
            self.pad_consumed_keys.push(physical.clone());
        }
        if *repeat {
            return true;
        }
        let Some(world) = self.play_world(session) else {
            return true;
        };
        match key {
            PlayDeviceKey::Traverse => {
                self.set_pad(world, None);
                let len = crate::play_device::PAD_BUTTONS.len();
                self.pad_focus = Some(match (self.pad_focus, shift) {
                    (None, false) => 0,
                    (None, true) => len - 1,
                    (Some(i), false) => (i + 1) % len,
                    (Some(i), true) => (i + len - 1) % len,
                });
            }
            PlayDeviceKey::Activate => {
                if let Some(i) = self.pad_focus {
                    self.set_pad(world, Some(crate::play_device::PAD_BUTTONS[i].0));
                    self.pad_key = Some(physical.clone());
                }
            }
            PlayDeviceKey::Leave => {
                self.set_pad(world, None);
                self.pad_focus = None;
            }
        }
        true
    }

    fn sync_pad_controls(&mut self, edit: &mut World, session: &mut PieSession) {
        let sim = self.device(edit);
        let world = session.app.world_mut();
        let old = self.last_device;
        if (sim.preset, sim.landscape, sim.touch, sim.gamepad)
            != (old.preset, old.landscape, old.touch, old.gamepad)
        {
            self.set_pad(world, None);
            if let Some(entity) = self.entity() {
                deliver_touches(world, entity, &self.touch.cancel());
            }
            self.pad_focus = None;
            self.pad_key = None;
        }
        self.last_device = sim;
        let mut queue = edit.get_resource_or_insert_with(crate::play_device::DeviceControlQueue::default);
        let current = world.id();
        let buttons = if queue.world == Some(current) {
            std::mem::take(&mut queue.buttons)
        } else {
            queue.buttons.clear();
            Vec::new()
        };
        queue.world = Some(current);
        if sim.gamepad {
            for (button, pressed) in buttons {
                if pressed {
                    self.set_pad(world, Some(button));
                } else if self.pad.held() == Some(button) {
                    self.set_pad(world, None);
                }
            }
        }
    }

    /// Whether a left press (or release) is the device simulation's: a press on a pad button or any
    /// press with touch emulation on, and the release of what such a press started.
    fn device_input(&self, edit: &World, pressed: bool) -> bool {
        let device = self.device(edit);
        if !pressed {
            return self.pad.holding() || self.touch.active();
        }
        let at = self
            .cursor
            .and_then(|(x, y)| self.picture().to_picture(x, y, self.render_size()));
        let on_pad = device.gamepad
            && at.is_some_and(|at| {
                crate::play_device::pad_button_at_scaled(
                    at,
                    self.render_size(),
                    crate::play_device::ui_scale(edit),
                )
                .is_some()
            });
        on_pad || (device.touch && at.is_some())
    }

    /// A left press or release [`PlayWindow::device_input`] claimed: a pad button, else a touch.
    fn device_press(&mut self, pressed: bool, edit: &World, session: &mut Option<PieSession>) -> Routed {
        let size = self.render_size();
        let at = self
            .cursor
            .and_then(|(x, y)| self.picture().to_picture(x, y, size));
        let device = self.device(edit);
        let Some(entity) = self.entity() else {
            return Routed::Ignored;
        };
        let Some(world) = self.play_world(session) else {
            return Routed::Ignored;
        };
        let button = at.filter(|_| device.gamepad).and_then(|at| {
            crate::play_device::pad_button_at_scaled(at, size, crate::play_device::ui_scale(edit))
        });
        if pressed && button.is_some() {
            deliver_touches(world, entity, &self.touch.cancel());
            self.pad_key = None;
            self.set_pad(world, button);
            return Routed::Play;
        }
        if !pressed && self.pad.holding() {
            self.set_pad(world, None);
            return Routed::Play;
        }
        let touches = match (pressed, at) {
            (true, Some(at)) => self.touch.press(at, self.ctrl),
            (false, _) => self.touch.release(),
            (true, None) => Vec::new(),
        };
        deliver_touches(world, entity, &touches);
        Routed::Play
    }

    /// Lets go of a held pad button and cancels a touch (focus lost, play restarted).
    fn device_release_all(&mut self, session: &mut Option<PieSession>) {
        self.pad_focus = None;
        self.pad_key = None;
        let button = self.pad.release();
        let touches = self.touch.cancel();
        let (Some(entity), Some(world)) = (self.entity(), self.play_world(session)) else {
            return;
        };
        if let Some(button) = button {
            world.write_message(regia_platform::GamepadButtonChanged {
                device: crate::play_device::VIRTUAL_PAD_DEVICE,
                button,
                state: ButtonState::Released,
                value: 0.0,
            });
        }
        deliver_touches(world, entity, &touches);
    }

    /// Keeps the play world's safe area at the simulated device's (ADR-7829); a device turned off puts
    /// back an empty safe area.
    fn sync_device(&mut self, edit: &World, session: &mut PieSession) {
        let wanted = self.device(edit).layout().map(|layout| layout.safe_area());
        let world = session.app.world_mut();
        match wanted {
            Some(safe) => {
                if world.get_resource::<regia_platform::SafeArea>() != Some(&safe) {
                    world.insert_resource(safe);
                }
                self.safe_area = Some(safe);
            }
            None => {
                if self.safe_area.take().is_some() {
                    world.insert_resource(regia_platform::SafeArea::default());
                }
            }
        }
    }

    /// The preferences' play resolution (cached by preference generation).
    fn prefs_resolution(&mut self, edit: &World) -> PlayResolution {
        let Some(prefs) = edit.get_resource::<EditorPreferences>() else {
            return PlayResolution::default();
        };
        let generation = prefs.generation();
        match self.prefs_resolution {
            Some((seen, resolution)) if seen == generation => resolution,
            _ => {
                let resolution = PlayResolution::from_prefs(prefs);
                self.prefs_resolution = Some((generation, resolution));
                resolution
            }
        }
    }

    /// The session's play resolution: its own (`editor.play`'s `preset` / `size`), else the
    /// preferences'.
    fn chosen_resolution(&mut self, edit: &World, session: &PieSession) -> PlayResolution {
        // A simulated device's screen wins (ADR-7829): the picture is that device's.
        if let Some(layout) = edit
            .get_resource::<crate::play_device::DeviceSim>()
            .and_then(crate::play_device::DeviceSim::layout)
        {
            return fixed(layout.size[0], layout.size[1]);
        }
        match session.options.resolution {
            Some(resolution) => resolution,
            None => self.prefs_resolution(edit),
        }
    }

    fn publish(&mut self, edit: &mut World) {
        edit.insert_resource(crate::play_device::DeviceInputState {
            held: self.pad.held(),
            focused: self.pad_focus,
        });
        // A closed window reports the resolution the next one opens with.
        let resolution = if self.id.is_some() {
            self.resolution
        } else {
            self.prefs_resolution(edit)
        };
        let window = [self.geometry.width, self.geometry.height];
        let render_size = resolution.render_size(window);
        let status = PlayWindowStatus {
            resolution,
            render_size,
            picture: Letterbox::fit(render_size, window).to_array(),
            open: self.id.is_some(),
            size: [self.geometry.width, self.geometry.height],
            position: self.geometry.position,
            captured: self.capture.captured,
            error: self.error.clone(),
            counters: self.counters,
        };
        if edit.get_resource::<PlayWindowStatus>() != Some(&status) {
            edit.insert_resource(status);
        }
    }
}

/// The editor frame of the last toolbar "Play in window" request (egui runs the toolbar twice in
/// some frames; one click queues one request).
#[derive(Resource, Debug, Clone, Copy, Default)]
struct PlayInWindowQueued(Option<u64>);

/// Queues Play in a separate window from the toolbar, once per editor frame `frame` (egui's
/// `cumulative_frame_nr`). Returns whether it queued.
pub fn queue_play_in_window(world: &mut World, frame: u64) -> bool {
    let mut queued = world.get_resource_or_insert_with(PlayInWindowQueued::default);
    if queued.0 == Some(frame) {
        return false;
    }
    queued.0 = Some(frame);
    world
        .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
        .push(PieRequest::PlayWith(crate::pie::PlayOptions {
            window: true,
            ..crate::pie::PlayOptions::default()
        }));
    true
}

/// Sets the play world's primary window `entity` to `width` x `height` and tells the game
/// (`WindowResized`).
fn size_game_window(world: &mut World, entity: Entity, width: u32, height: u32) {
    if let Some(mut window) = world.get_mut::<regia_platform::Window>(entity) {
        window.physical_size = (width, height);
    }
    world.write_message(regia_platform::WindowResized {
        window: entity,
        width,
        height,
    });
}

/// Stores the play resolution in the editor preferences (the toolbar's size menu); the next play
/// window renders at it. Whether the preferences changed (none in `world`: they are made).
pub fn set_play_resolution(world: &mut World, resolution: PlayResolution) -> bool {
    let mut prefs = world.get_resource_or_insert_with(EditorPreferences::default);
    resolution.store(&mut prefs)
}

/// Writes `event` into the play world as the engine's input message for window `entity`.
fn deliver(world: &mut World, window: Entity, event: PlayWindowEvent) {
    let timestamp = InputTimestamp::now();
    let state = |pressed: bool| {
        if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        }
    };
    match event {
        PlayWindowEvent::Key {
            physical,
            logical,
            pressed,
            repeat,
            ..
        } => {
            world.write_message(regia_platform::KeyboardInput {
                window,
                physical_key: physical,
                logical_key: logical,
                state: state(pressed),
                repeat,
                timestamp,
            });
        }
        PlayWindowEvent::MouseButton { button, pressed } => {
            world.write_message(regia_platform::MouseButtonInput {
                window,
                button,
                state: state(pressed),
                timestamp,
            });
        }
        PlayWindowEvent::CursorMoved { x, y } => {
            world.write_message(regia_platform::CursorMoved {
                window,
                position: (x, y),
                timestamp,
            });
        }
        PlayWindowEvent::MouseMotion { dx, dy } => {
            world.write_message(regia_platform::MouseMotion {
                delta: (dx, dy),
                timestamp,
            });
        }
        PlayWindowEvent::Wheel { delta, unit } => {
            world.write_message(regia_platform::MouseWheel {
                window,
                delta,
                unit,
                timestamp,
            });
        }
        PlayWindowEvent::Focused(focused) => {
            world.write_message(regia_platform::WindowFocused { window, focused });
        }
        // Handled by `PlayWindow::on_event` before delivery.
        PlayWindowEvent::Resized { .. }
        | PlayWindowEvent::Moved { .. }
        | PlayWindowEvent::SurfaceLost
        | PlayWindowEvent::CloseRequested => {}
    }
}

/// Writes emulated touches (ADR-7829) to the play world as `regia_platform::TouchInput`.
fn deliver_touches(world: &mut World, window: Entity, touches: &[crate::play_device::TouchEvent]) {
    let timestamp = InputTimestamp::now();
    for (id, phase, position) in touches {
        world.write_message(regia_platform::TouchInput {
            window,
            id: *id,
            phase: *phase,
            position: *position,
            timestamp,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_defaults_with_no_preferences_and_stores_only_what_differs() {
        let mut prefs = EditorPreferences::default();
        assert_eq!(
            PlayWindowGeometry::from_prefs(&prefs),
            PlayWindowGeometry::default()
        );
        assert!(
            !PlayWindowGeometry::default().store(&mut prefs),
            "defaults store nothing"
        );
        assert!(prefs.is_default(PREF_WIDTH) && prefs.is_default(PREF_PLACED));
        let placed = PlayWindowGeometry {
            width: 640,
            height: 480,
            position: Some([-5, 30]),
        };
        assert!(placed.store(&mut prefs));
        assert_eq!(PlayWindowGeometry::from_prefs(&prefs), placed);
        // A window the window system placed (Wayland) forgets the old position.
        let unplaced = PlayWindowGeometry {
            position: None,
            ..placed
        };
        assert!(unplaced.store(&mut prefs));
        assert_eq!(PlayWindowGeometry::from_prefs(&prefs), unplaced);
        // A size below the preference's range keeps the stored one.
        let tiny = PlayWindowGeometry { width: 4, ..unplaced };
        assert!(!tiny.store(&mut prefs));
        assert_eq!(PlayWindowGeometry::from_prefs(&prefs).width, 640);
    }

    #[test]
    fn the_toolbar_queues_one_play_in_window_request_per_frame() {
        let mut world = World::new();
        assert!(queue_play_in_window(&mut world, 7));
        assert!(
            !queue_play_in_window(&mut world, 7),
            "the second UI pass of frame 7"
        );
        assert!(queue_play_in_window(&mut world, 8));
        let queue = &world.resource::<crate::pie::PieRequests>().queue;
        assert_eq!(queue.len(), 2);
        assert!(matches!(queue[0], PieRequest::PlayWith(options) if options.window));
    }

    #[test]
    fn events_of_other_windows_go_to_the_editor_until_a_play_window_opened() {
        struct Never;
        impl PlayWindowPlatform for Never {
            fn open(&mut self, _: PlayWindowGeometry) -> Result<u64, String> {
                Err("never".into())
            }
            fn configure_surface(&mut self, _: u32, _: u32) {
                unreachable!("no window")
            }
            fn recreate_surface(&mut self) -> Result<(), String> {
                unreachable!("no window")
            }
            fn set_capture(&mut self, _: bool) {
                unreachable!("no window")
            }
            fn close(&mut self) {
                unreachable!("no window")
            }
        }
        let mut window = PlayWindow::default();
        let mut edit = World::new();
        let mut session = None;
        for id in [0, 1, PLAY_WINDOW_VIEW_ID] {
            assert_eq!(
                window.on_event(
                    id,
                    PlayWindowEvent::CloseRequested,
                    &mut edit,
                    &mut session,
                    &mut Never
                ),
                Routed::Editor
            );
        }
        // No session: sync opens nothing and publishes a closed status.
        window.sync(&mut edit, None, &mut Never);
        assert!(!edit.resource::<PlayWindowStatus>().open);
    }
}
