// SPDX-License-Identifier: MPL-2.0
//! Steam Deck readiness that needs no device to test (ADR-368): which upscaler and render scale
//! the Deck starts with, power and thermal hints that step the frame cap down, controller glyph
//! selection, the floating on-screen keyboard hook and the resume reset.
//!
//! Everything here is pure logic or reads sysfs through a root path a test can point at a temp
//! directory. The numeric thresholds are first guesses: `docs/releases/v0.1.29.md` lists
//! what must be measured on a real Deck before they are trusted.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_platform::{AppLifecycle, GamepadButton, GamepadConnected, LifecycleEvent};

use crate::deck::{DeckFrameCap, DeckFrameCapSetting, DeckPanel, SteamDeck};
use crate::resource::Steam;

/// The temporal upscaler the Deck starts with. Mirrors the renderer's `TemporalUpscaler`
/// (`regia_steam` does not depend on `regia_render`; the host maps this by name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeckUpscaler {
    /// The TSR-grade upscaler: the Deck default.
    #[default]
    Tsr,
    /// FSR 3.1 upscaling, the player's alternative.
    Fsr3,
    /// The original TAAU, the cheapest fallback.
    Taau,
}

impl DeckUpscaler {
    /// The name the renderer's settings use for this upscaler.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tsr => "tsr",
            Self::Fsr3 => "fsr3",
            Self::Taau => "taau",
        }
    }
}

/// The render defaults chosen for a Deck: output size, internal render scale, upscaler, frame cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeckRenderDefaults {
    /// Output resolution (the panel's native 1280×800).
    pub resolution: (u16, u16),
    /// Internal render scale (fraction of the output size per axis).
    pub render_scale: f32,
    /// The temporal upscaler.
    pub upscaler: DeckUpscaler,
    /// The starting frame cap.
    pub frame_cap: DeckFrameCap,
}

impl DeckRenderDefaults {
    /// The render scale of the `Deck` render preset.
    pub const BASE_RENDER_SCALE: f32 = 0.7;

    /// The defaults for `panel` when the machine is on `source`. The LCD panel starts at 40 fps
    /// (a native 40 Hz refresh), the OLED panel at 45 fps (its lowest native refresh). On battery
    /// below [`LOW_BATTERY_PERCENT`] the start is 30 fps with the cheaper TAAU.
    pub fn select(panel: DeckPanel, source: PowerSource) -> Self {
        let low = matches!(source, PowerSource::Battery { percent } if percent < LOW_BATTERY_PERCENT);
        let (frame_cap, upscaler, render_scale) = if low {
            (DeckFrameCap::Fps30, DeckUpscaler::Taau, 0.6)
        } else {
            let cap = match panel {
                DeckPanel::Lcd => DeckFrameCap::Fps40,
                DeckPanel::Oled => DeckFrameCap::Fps45,
            };
            (cap, DeckUpscaler::Tsr, Self::BASE_RENDER_SCALE)
        };
        Self {
            resolution: (1280, 800),
            render_scale,
            upscaler,
            frame_cap,
        }
    }
}

/// Battery percentage below which the Deck drops to its lowest tier.
pub const LOW_BATTERY_PERCENT: u8 = 15;

/// Where the machine gets its power.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerSource {
    /// Plugged in, or no battery information (a desktop, a dock).
    #[default]
    Mains,
    /// Running on the battery.
    Battery {
        /// Charge, 0..=100.
        percent: u8,
    },
}

/// How hot the APU is, from the highest hwmon temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ThermalState {
    /// Below the warm threshold.
    #[default]
    Nominal,
    /// At or above [`WARM_C`].
    Warm,
    /// At or above [`HOT_C`].
    Hot,
}

/// Temperature at which the Deck counts as warm, degrees Celsius. A guess; measure on a Deck.
pub const WARM_C: f32 = 80.0;
/// Temperature at which the Deck counts as hot, degrees Celsius. A guess; measure on a Deck.
pub const HOT_C: f32 = 90.0;
/// A level only relaxes after the temperature is this far below its entry threshold.
pub const HYSTERESIS_C: f32 = 5.0;

impl ThermalState {
    /// Classifies a temperature; non-finite values are `Nominal`.
    pub fn from_celsius(c: f32) -> Self {
        if !c.is_finite() {
            Self::Nominal
        } else if c >= HOT_C {
            Self::Hot
        } else if c >= WARM_C {
            Self::Warm
        } else {
            Self::Nominal
        }
    }

    /// The entry threshold of this state, `None` for `Nominal`.
    fn entry_c(self) -> Option<f32> {
        match self {
            Self::Nominal => None,
            Self::Warm => Some(WARM_C),
            Self::Hot => Some(HOT_C),
        }
    }
}

/// One reading of the power and thermal sensors.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DeckSensorSample {
    /// Power source.
    pub source: PowerSource,
    /// Highest hwmon temperature, degrees Celsius, if any sensor was readable.
    pub temperature_c: Option<f32>,
}

impl DeckSensorSample {
    /// Reads `power_supply` and `hwmon` under `sys_root` (`/sys` on a Deck). Unreadable or absent
    /// entries are skipped, so a desktop yields `Mains` and no temperature.
    pub fn read(sys_root: &Path) -> Self {
        Self {
            source: read_power_source(&sys_root.join("class/power_supply")),
            temperature_c: read_max_temperature(&sys_root.join("class/hwmon")),
        }
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    children.sort();
    children
}

fn read_power_source(dir: &Path) -> PowerSource {
    for supply in sorted_children(dir) {
        if read_trimmed(&supply.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let percent = read_trimmed(&supply.join("capacity"))
            .and_then(|s| s.parse::<u32>().ok())
            .map(|p| p.min(100) as u8);
        let status = read_trimmed(&supply.join("status"));
        return match (status.as_deref(), percent) {
            (Some("Discharging"), Some(percent)) => PowerSource::Battery { percent },
            _ => PowerSource::Mains,
        };
    }
    PowerSource::Mains
}

fn read_max_temperature(dir: &Path) -> Option<f32> {
    let mut best: Option<f32> = None;
    for hwmon in sorted_children(dir) {
        for entry in sorted_children(&hwmon) {
            let is_temp = entry
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"));
            if !is_temp {
                continue;
            }
            if let Some(milli) = read_trimmed(&entry).and_then(|s| s.parse::<i64>().ok()) {
                let c = milli as f32 / 1000.0;
                best = Some(best.map_or(c, |b| b.max(c)));
            }
        }
    }
    best
}

/// Why the governor limits the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerReason {
    /// No limit.
    #[default]
    None,
    /// The battery is low.
    LowBattery,
    /// The APU is warm or hot.
    Thermal,
}

impl PowerReason {
    /// Stable name for the agent protocol.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LowBattery => "low_battery",
            Self::Thermal => "thermal",
        }
    }
}

/// What the governor asks of the frame loop and renderer.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct DeckPowerPolicy {
    /// The highest cap allowed right now (the player's cap is applied under it).
    pub cap: DeckFrameCap,
    /// Multiplier on the render scale (1.0 = no change); the host applies it.
    pub render_scale_factor: f32,
    /// Why the policy limits anything.
    pub reason: PowerReason,
}

impl Default for DeckPowerPolicy {
    fn default() -> Self {
        Self {
            cap: DeckFrameCap::Fps60,
            render_scale_factor: 1.0,
            reason: PowerReason::None,
        }
    }
}

/// Consecutive samples at or above a threshold before the governor steps down.
pub const ESCALATE_SAMPLES: u32 = 2;
/// Consecutive samples below a threshold minus [`HYSTERESIS_C`] before it steps back up.
pub const RELAX_SAMPLES: u32 = 6;

/// Steps the cap down when the Deck runs hot or low on battery, and back up with hysteresis.
/// Frame- and sample-counted, never wall-clock, so it is deterministic under test.
#[derive(Resource, Debug, Clone, Default)]
pub struct ThermalGovernor {
    state: ThermalState,
    above: u32,
    below: u32,
    policy: DeckPowerPolicy,
}

impl ThermalGovernor {
    /// The current thermal state the governor holds (after hysteresis).
    pub fn state(&self) -> ThermalState {
        self.state
    }

    /// The current policy.
    pub fn policy(&self) -> DeckPowerPolicy {
        self.policy
    }

    /// Forgets the counters (after a resume the old samples say nothing about the new heat).
    /// The held state stays until fresh samples move it.
    pub fn reset_counters(&mut self) {
        self.above = 0;
        self.below = 0;
    }

    /// Feeds one sensor sample and returns the resulting policy.
    pub fn observe(&mut self, sample: DeckSensorSample) -> DeckPowerPolicy {
        let Some(celsius) = sample.temperature_c else {
            // No reading (a failed hwmon read) says nothing about the heat: hold the state and counters.
            self.policy = Self::policy_for(self.state, sample.source);
            return self.policy;
        };
        let seen = ThermalState::from_celsius(celsius);
        if seen > self.state {
            self.below = 0;
            self.above += 1;
            if self.above >= ESCALATE_SAMPLES {
                self.state = seen;
                self.above = 0;
            }
        } else if seen < self.state {
            self.above = 0;
            // Relaxing needs the temperature below the held state's entry threshold minus the margin.
            let clear = match self.state.entry_c() {
                Some(entry) => celsius < entry - HYSTERESIS_C,
                None => true,
            };
            if clear {
                self.below += 1;
                if self.below >= RELAX_SAMPLES {
                    self.state = seen;
                    self.below = 0;
                }
            } else {
                self.below = 0;
            }
        } else {
            self.above = 0;
            self.below = 0;
        }
        self.policy = Self::policy_for(self.state, sample.source);
        self.policy
    }

    fn policy_for(state: ThermalState, source: PowerSource) -> DeckPowerPolicy {
        let low_battery = matches!(source, PowerSource::Battery { percent } if percent < LOW_BATTERY_PERCENT);
        let (cap, factor, reason) = match state {
            ThermalState::Hot => (DeckFrameCap::Fps30, 0.85, PowerReason::Thermal),
            ThermalState::Warm => (DeckFrameCap::Fps40, 0.93, PowerReason::Thermal),
            ThermalState::Nominal if low_battery => (DeckFrameCap::Fps30, 0.85, PowerReason::LowBattery),
            ThermalState::Nominal => (DeckFrameCap::Fps60, 1.0, PowerReason::None),
        };
        DeckPowerPolicy {
            cap,
            render_scale_factor: factor,
            reason,
        }
    }
}

/// The lower of the player's cap and the governor's cap.
pub fn governed_cap(player: DeckFrameCap, policy: &DeckPowerPolicy) -> DeckFrameCap {
    if player.fps() <= policy.cap.fps() {
        player
    } else {
        policy.cap
    }
}

/// Where the sensor system reads and how often (frames between reads).
#[derive(Resource, Debug, Clone)]
pub struct DeckSensorConfig {
    /// The sysfs root (`/sys`).
    pub sys_root: PathBuf,
    /// Frames between two sensor reads.
    pub interval_frames: u32,
}

impl Default for DeckSensorConfig {
    fn default() -> Self {
        Self {
            sys_root: PathBuf::from("/sys"),
            interval_frames: 300,
        }
    }
}

/// `PreUpdate` on a Deck: reads the sensors every `interval_frames` frames and updates the
/// governor and [`DeckPowerPolicy`].
pub fn poll_deck_sensors(
    deck: Res<SteamDeck>,
    config: Res<DeckSensorConfig>,
    mut frame: Local<u32>,
    mut governor: ResMut<ThermalGovernor>,
    mut policy: ResMut<DeckPowerPolicy>,
) {
    if !deck.detected {
        return;
    }
    let due = *frame == 0;
    *frame = (*frame + 1) % config.interval_frames.max(1);
    if !due {
        return;
    }
    let sample = DeckSensorSample::read(&config.sys_root);
    let next = governor.observe(sample);
    if *policy != next {
        tracing::info!(target: "regia_steam", "Deck power policy: {next:?} ({sample:?})");
        *policy = next;
    }
}

/// The controller family a game's prompts should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ControllerKind {
    /// Not known yet.
    #[default]
    Unknown,
    /// The Steam Deck's built-in controls.
    SteamDeck,
    /// Steam Controller.
    SteamController,
    /// Xbox 360 / One / Series.
    Xbox,
    /// DualShock 4 / DualSense.
    PlayStation,
    /// Switch Pro Controller or Joy-Con.
    Switch,
    /// Any other gamepad.
    Generic,
}

impl ControllerKind {
    /// Maps Steam Input's `ESteamInputType` number (public Steamworks documentation).
    pub fn from_steam_input_type(t: u32) -> Self {
        match t {
            1 => Self::SteamController,
            2 | 3 => Self::Xbox,
            4 => Self::Generic,
            5 | 12 | 13 => Self::PlayStation,
            8..=10 => Self::Switch,
            14 => Self::SteamDeck,
            _ => Self::Unknown,
        }
    }

    /// Guesses from a USB vendor id: Microsoft, Sony, Nintendo and Valve.
    pub fn from_vendor_id(vendor: u16) -> Self {
        match vendor {
            0x045e => Self::Xbox,
            0x054c => Self::PlayStation,
            0x057e => Self::Switch,
            0x28de => Self::SteamDeck,
            _ => Self::Generic,
        }
    }

    /// Guesses from the device name the OS or the SDL mapping database reports (case-insensitive):
    /// `Xbox Wireless Controller`, `PS5 Controller`, `Nintendo Switch Pro Controller`, … An empty
    /// name is `Unknown`.
    pub fn from_device_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.trim().is_empty() {
            Self::Unknown
        } else if n.contains("steam deck") || n.contains("neptune") {
            Self::SteamDeck
        } else if n.contains("steam controller") {
            Self::SteamController
        } else if n.contains("xbox") || n.contains("x-box") || n.contains("xinput") {
            Self::Xbox
        } else if n.contains("dualsense")
            || n.contains("dualshock")
            || n.contains("playstation")
            || n.contains("wireless controller")
            || ["ps3", "ps4", "ps5"].iter().any(|model| n.contains(model))
        {
            Self::PlayStation
        } else if n.contains("nintendo") || n.contains("pro controller") || n.contains("joy-con") {
            Self::Switch
        } else {
            Self::Generic
        }
    }

    /// The kind whose glyphs to show when a pad named `name` connects; `on_deck` when the game
    /// runs on a Steam Deck. On a Deck, Steam presents the built-in controls to the game as a
    /// virtual pad named like an Xbox pad (or `Steam Virtual Gamepad`, which reads as
    /// [`Self::Generic`]), so both of those keep the Deck's glyphs there (ADR-1332). Off a Deck the
    /// name decides alone.
    pub fn for_connected_pad(name: &str, on_deck: bool) -> Self {
        match Self::from_device_name(name) {
            Self::Xbox | Self::Generic if on_deck => Self::SteamDeck,
            kind => kind,
        }
    }

    /// The glyph family for this kind. The Steam Controller shares the Deck's layout.
    pub fn glyphs(self) -> GlyphFamily {
        match self {
            Self::SteamDeck | Self::SteamController => GlyphFamily::SteamDeck,
            Self::PlayStation => GlyphFamily::PlayStation,
            Self::Switch => GlyphFamily::Switch,
            Self::Xbox | Self::Generic | Self::Unknown => GlyphFamily::Xbox,
        }
    }
}

/// A set of button labels and glyph names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GlyphFamily {
    /// A / B / X / Y, LB / RB.
    #[default]
    Xbox,
    /// Cross / Circle / Square / Triangle, L1 / R1.
    PlayStation,
    /// Nintendo layout: A is the east button, B the south one.
    Switch,
    /// The Deck: Xbox-style face buttons, L4 / R4 grips elsewhere, View / Menu in the middle.
    SteamDeck,
}

impl GlyphFamily {
    /// Directory name under which a game keeps this family's glyph textures.
    pub const fn dir(self) -> &'static str {
        match self {
            Self::Xbox => "xbox",
            Self::PlayStation => "playstation",
            Self::Switch => "switch",
            Self::SteamDeck => "deck",
        }
    }

    /// The label printed on `button` for this family.
    pub fn label(self, button: GamepadButton) -> &'static str {
        use GamepadButton::*;
        match (self, button) {
            (Self::PlayStation, South) => "Cross",
            (Self::PlayStation, East) => "Circle",
            (Self::PlayStation, West) => "Square",
            (Self::PlayStation, North) => "Triangle",
            (Self::PlayStation, LeftShoulder) => "L1",
            (Self::PlayStation, RightShoulder) => "R1",
            (Self::PlayStation, LeftTrigger) => "L2",
            (Self::PlayStation, RightTrigger) => "R2",
            (Self::PlayStation, LeftStick) => "L3",
            (Self::PlayStation, RightStick) => "R3",
            (Self::PlayStation, Start) => "Options",
            (Self::PlayStation, Select) => "Create",
            (Self::Switch, South) => "B",
            (Self::Switch, East) => "A",
            (Self::Switch, West) => "Y",
            (Self::Switch, North) => "X",
            (Self::Switch, LeftShoulder) => "L",
            (Self::Switch, RightShoulder) => "R",
            (Self::Switch, LeftTrigger) => "ZL",
            (Self::Switch, RightTrigger) => "ZR",
            (Self::Switch, LeftStick) => "L Stick Press",
            (Self::Switch, RightStick) => "R Stick Press",
            (Self::Switch, Start) => "+",
            (Self::Switch, Select) => "-",
            (Self::Xbox | Self::SteamDeck, South) => "A",
            (Self::Xbox | Self::SteamDeck, East) => "B",
            (Self::Xbox | Self::SteamDeck, West) => "X",
            (Self::Xbox | Self::SteamDeck, North) => "Y",
            (Self::Xbox | Self::SteamDeck, LeftShoulder) => "LB",
            (Self::Xbox | Self::SteamDeck, RightShoulder) => "RB",
            (Self::Xbox | Self::SteamDeck, LeftTrigger) => "LT",
            (Self::Xbox | Self::SteamDeck, RightTrigger) => "RT",
            (Self::Xbox | Self::SteamDeck, LeftStick) => "LS",
            (Self::Xbox | Self::SteamDeck, RightStick) => "RS",
            (Self::Xbox | Self::SteamDeck, Start) => "Menu",
            (Self::Xbox | Self::SteamDeck, Select) => "View",
            (Self::Xbox, Mode) => "Guide",
            (Self::SteamDeck, Mode) => "Steam",
            (Self::PlayStation, Mode) => "PS",
            (Self::Switch, Mode) => "Home",
            (_, DPadUp) => "D-pad Up",
            (_, DPadDown) => "D-pad Down",
            (_, DPadLeft) => "D-pad Left",
            (_, DPadRight) => "D-pad Right",
        }
    }

    /// The texture key a game loads for `button`: `glyphs/<family>/<label lowercased, spaces as _>`.
    pub fn texture_key(self, button: GamepadButton) -> String {
        let label = match self.label(button) {
            "+" => "plus".to_string(),
            "-" => "minus".to_string(),
            other => other.to_ascii_lowercase().replace(' ', "_"),
        };
        format!("glyphs/{}/{}", self.dir(), label)
    }
}

/// The controller family the UI is currently showing. Starts as the Deck's own controls on a Deck
/// and follows the most recently connected pad.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActiveGlyphs {
    /// The controller kind.
    pub kind: ControllerKind,
}

impl ActiveGlyphs {
    /// The glyph family to draw.
    pub fn family(&self) -> GlyphFamily {
        self.kind.glyphs()
    }

    /// Updates from a connected pad's name. A name that identifies nothing (`Unknown`) is ignored.
    pub fn on_connected(&mut self, name: &str) {
        let kind = ControllerKind::from_device_name(name);
        if kind != ControllerKind::Unknown {
            self.kind = kind;
        }
    }
}

/// `PreUpdate`: follows connected pads. On a Deck, Steam's virtual controller is named like an
/// Xbox pad (`Microsoft X-Box 360 pad 0` through evdev) or `Steam Virtual Gamepad` (the name the
/// gilrs backend reports, measured on a Deck OLED on 2026-10-29), so a pad that reads `Xbox` or
/// `Generic` while on a Deck stays on the Deck's glyphs (ADR-1750).
pub fn track_active_glyphs(
    deck: Res<SteamDeck>,
    mut connected: MessageReader<GamepadConnected>,
    mut glyphs: ResMut<ActiveGlyphs>,
) {
    for event in connected.read() {
        let next = ControllerKind::for_connected_pad(&event.name, deck.detected);
        if next != ControllerKind::Unknown && glyphs.kind != next {
            glyphs.kind = next;
        }
    }
}

/// `PreUpdate`, after the Steam Input bridge and [`track_active_glyphs`]: while Steam Input is
/// active and reports the first controller's type, that type picks the glyphs (ADR-1332). Steam
/// Input knows the physical pad behind its virtual one, so a DualSense used through Steam Input
/// on a Deck shows PlayStation glyphs, not the Deck's. An unknown type changes nothing.
pub fn follow_steam_input_glyphs(
    steam: Res<Steam>,
    status: Res<crate::input::SteamInputStatus>,
    mut glyphs: ResMut<ActiveGlyphs>,
) {
    if !status.active || status.controllers == 0 {
        return;
    }
    let Some(input_type) = steam.backend.input_controller_type() else {
        return;
    };
    let kind = ControllerKind::from_steam_input_type(input_type);
    if kind != ControllerKind::Unknown && glyphs.kind != kind {
        glyphs.kind = kind;
    }
}

/// What kind of text a field takes, for the floating keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardMode {
    /// One line of text.
    #[default]
    SingleLine,
    /// Several lines.
    MultiLine,
    /// An email address.
    Email,
    /// Digits only.
    Numeric,
}

/// A request to show the floating keyboard over a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardRequest {
    /// Input type.
    pub mode: KeyboardMode,
    /// The field's rectangle in window pixels: x, y, width, height. The keyboard avoids it.
    pub field: (i32, i32, i32, i32),
}

/// Sent by UI text widgets when a text field gains or loses focus.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextFieldFocus {
    /// `Some` when a field gained focus, `None` when focus left all text fields.
    pub request: Option<KeyboardRequest>,
}

/// Whether the floating keyboard is up.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardState {
    /// Shown by this engine and not yet dismissed.
    pub open: bool,
    /// Times a show request was refused or failed.
    pub failures: u32,
}

/// `PreUpdate`: shows or dismisses the Deck's floating keyboard when a text field takes or loses
/// focus. Does nothing off a Deck or without Steam; a failure is counted, never fatal.
pub fn drive_keyboard(
    deck: Res<SteamDeck>,
    steam: Res<Steam>,
    mut focus: MessageReader<TextFieldFocus>,
    mut state: ResMut<KeyboardState>,
) {
    for event in focus.read() {
        if !deck.detected {
            continue;
        }
        match event.request {
            Some(request) => match steam.show_floating_keyboard(&request) {
                Ok(()) => state.open = true,
                Err(error) => {
                    state.failures += 1;
                    tracing::debug!(target: "regia_steam", "floating keyboard refused: {error}");
                }
            },
            None if state.open => {
                steam.dismiss_floating_keyboard();
                state.open = false;
            }
            None => {}
        }
    }
}

/// `Lifecycle`: after a resume (a real one, or the synthetic pair the runner raises when the Deck
/// wakes from sleep) drop the keyboard state, restart the governor's counters and re-apply the frame
/// cap, so a stale cap or keyboard from before the sleep does not survive it.
pub fn reset_on_resume(
    lifecycle: Res<AppLifecycle>,
    steam: Res<Steam>,
    mut keyboard: ResMut<KeyboardState>,
    mut governor: ResMut<ThermalGovernor>,
    mut cap: ResMut<DeckFrameCapSetting>,
) {
    if lifecycle.last_event != Some(LifecycleEvent::Resumed) {
        return;
    }
    if keyboard.open {
        steam.dismiss_floating_keyboard();
        keyboard.open = false;
    }
    governor.reset_counters();
    // Mark the setting changed so `apply_deck_frame_cap` runs again against the fresh pacing.
    cap.set_changed();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hot(c: f32) -> DeckSensorSample {
        DeckSensorSample {
            source: PowerSource::Mains,
            temperature_c: Some(c),
        }
    }

    #[test]
    fn defaults_follow_the_panel_and_the_battery() {
        let lcd = DeckRenderDefaults::select(DeckPanel::Lcd, PowerSource::Mains);
        assert_eq!(lcd.resolution, (1280, 800));
        assert_eq!(lcd.frame_cap, DeckFrameCap::Fps40);
        assert_eq!(lcd.upscaler, DeckUpscaler::Tsr);
        assert_eq!(lcd.render_scale, 0.7);
        let oled = DeckRenderDefaults::select(DeckPanel::Oled, PowerSource::Battery { percent: 80 });
        assert_eq!(oled.frame_cap, DeckFrameCap::Fps45);
        let low = DeckRenderDefaults::select(DeckPanel::Lcd, PowerSource::Battery { percent: 10 });
        assert_eq!(low.frame_cap, DeckFrameCap::Fps30);
        assert_eq!(low.upscaler, DeckUpscaler::Taau);
        assert_eq!(DeckUpscaler::Fsr3.name(), "fsr3");
    }

    #[test]
    fn thermal_classification() {
        assert_eq!(ThermalState::from_celsius(60.0), ThermalState::Nominal);
        assert_eq!(ThermalState::from_celsius(80.0), ThermalState::Warm);
        assert_eq!(ThermalState::from_celsius(95.0), ThermalState::Hot);
        assert_eq!(ThermalState::from_celsius(f32::NAN), ThermalState::Nominal);
    }

    #[test]
    fn governor_escalates_after_two_samples_and_relaxes_after_six_with_hysteresis() {
        let mut g = ThermalGovernor::default();
        assert_eq!(
            g.observe(hot(95.0)).cap,
            DeckFrameCap::Fps60,
            "one hot sample is noise"
        );
        let p = g.observe(hot(95.0));
        assert_eq!(p.cap, DeckFrameCap::Fps30);
        assert_eq!(p.reason, PowerReason::Thermal);
        // 87 C is below Hot (90) but not 5 C below it: the held state stays.
        for _ in 0..20 {
            g.observe(hot(87.0));
        }
        assert_eq!(g.state(), ThermalState::Hot);
        // 84 C is clear of the margin, but Warm is the state seen: relax Hot -> Warm after six.
        for _ in 0..RELAX_SAMPLES - 1 {
            g.observe(hot(84.0));
            assert_eq!(g.state(), ThermalState::Hot);
        }
        let p = g.observe(hot(84.0));
        assert_eq!(g.state(), ThermalState::Warm);
        assert_eq!(p.cap, DeckFrameCap::Fps40);
        for _ in 0..RELAX_SAMPLES {
            g.observe(hot(50.0));
        }
        assert_eq!(g.policy(), DeckPowerPolicy::default());
    }

    #[test]
    fn low_battery_limits_the_cap_but_heat_wins() {
        let mut g = ThermalGovernor::default();
        let low = DeckSensorSample {
            source: PowerSource::Battery { percent: 9 },
            temperature_c: None,
        };
        let p = g.observe(low);
        assert_eq!(p.cap, DeckFrameCap::Fps30);
        assert_eq!(p.reason, PowerReason::LowBattery);
        let charged = DeckSensorSample {
            source: PowerSource::Battery { percent: 90 },
            temperature_c: None,
        };
        assert_eq!(g.observe(charged).reason, PowerReason::None);
    }

    #[test]
    fn a_missing_temperature_holds_the_thermal_state() {
        let mut g = ThermalGovernor::default();
        let hot = DeckSensorSample {
            source: PowerSource::Mains,
            temperature_c: Some(95.0),
        };
        for _ in 0..ESCALATE_SAMPLES {
            g.observe(hot);
        }
        assert_eq!(g.state(), ThermalState::Hot);
        let blind = DeckSensorSample {
            source: PowerSource::Mains,
            temperature_c: None,
        };
        for _ in 0..RELAX_SAMPLES * 3 {
            let p = g.observe(blind);
            assert_eq!(g.state(), ThermalState::Hot);
            assert_eq!(p.cap, DeckFrameCap::Fps30);
        }
    }

    #[test]
    fn the_player_cap_is_only_ever_lowered() {
        let policy = DeckPowerPolicy {
            cap: DeckFrameCap::Fps40,
            ..DeckPowerPolicy::default()
        };
        assert_eq!(governed_cap(DeckFrameCap::Fps60, &policy), DeckFrameCap::Fps40);
        assert_eq!(governed_cap(DeckFrameCap::Fps30, &policy), DeckFrameCap::Fps30);
    }

    #[test]
    fn sensors_read_from_a_fake_sysfs() {
        let root = regia_test_utils::unique_temp_dir("deck_sysfs");
        let put = |path: &std::path::Path, text: &str| {
            let dir = path.parent().expect("fixture file has a parent");
            std::fs::create_dir_all(dir).expect("create the fake sysfs folder");
            std::fs::write(path, text).expect("write the fake sysfs file");
        };
        let bat = root.join("class/power_supply/BAT1");
        put(&bat.join("type"), "Battery\n");
        put(&bat.join("capacity"), "42\n");
        put(&bat.join("status"), "Discharging\n");
        let mains = root.join("class/power_supply/ADP1");
        put(&mains.join("type"), "Mains\n");
        for (hw, milli) in [("hwmon0", "51000"), ("hwmon3", "83500")] {
            let dir = root.join("class/hwmon").join(hw);
            put(&dir.join("temp1_input"), &format!("{milli}\n"));
            put(&dir.join("name"), "x\n");
        }
        let sample = DeckSensorSample::read(&root);
        assert_eq!(sample.source, PowerSource::Battery { percent: 42 });
        assert_eq!(sample.temperature_c, Some(83.5));
        put(&bat.join("status"), "Charging\n");
        assert_eq!(DeckSensorSample::read(&root).source, PowerSource::Mains);
        let empty = regia_test_utils::unique_temp_dir("deck_sysfs_empty");
        assert_eq!(DeckSensorSample::read(&empty), DeckSensorSample::default());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn controller_kind_from_every_source() {
        assert_eq!(
            ControllerKind::from_steam_input_type(14),
            ControllerKind::SteamDeck
        );
        assert_eq!(
            ControllerKind::from_steam_input_type(13),
            ControllerKind::PlayStation
        );
        assert_eq!(ControllerKind::from_steam_input_type(10), ControllerKind::Switch);
        assert_eq!(ControllerKind::from_steam_input_type(3), ControllerKind::Xbox);
        assert_eq!(ControllerKind::from_steam_input_type(0), ControllerKind::Unknown);
        assert_eq!(
            ControllerKind::from_vendor_id(0x054c),
            ControllerKind::PlayStation
        );
        assert_eq!(ControllerKind::from_vendor_id(0x1234), ControllerKind::Generic);
        assert_eq!(
            ControllerKind::from_device_name("Sony Interactive Entertainment DualSense Wireless Controller"),
            ControllerKind::PlayStation
        );
        assert_eq!(
            ControllerKind::from_device_name("Nintendo Switch Pro Controller"),
            ControllerKind::Switch
        );
        assert_eq!(
            ControllerKind::from_device_name("Microsoft X-Box One pad"),
            ControllerKind::Xbox
        );
        assert_eq!(
            ControllerKind::from_device_name("Valve Steam Deck Controller"),
            ControllerKind::SteamDeck
        );
        assert_eq!(ControllerKind::from_device_name("   "), ControllerKind::Unknown);
        assert_eq!(
            ControllerKind::from_device_name("Acme Pad"),
            ControllerKind::Generic
        );
    }

    #[test]
    fn glyph_labels_cover_every_button_and_differ_by_family() {
        let all = [
            GamepadButton::South,
            GamepadButton::East,
            GamepadButton::West,
            GamepadButton::North,
            GamepadButton::LeftShoulder,
            GamepadButton::RightShoulder,
            GamepadButton::LeftTrigger,
            GamepadButton::RightTrigger,
            GamepadButton::LeftStick,
            GamepadButton::RightStick,
            GamepadButton::DPadUp,
            GamepadButton::DPadDown,
            GamepadButton::DPadLeft,
            GamepadButton::DPadRight,
            GamepadButton::Start,
            GamepadButton::Select,
            GamepadButton::Mode,
        ];
        for family in [
            GlyphFamily::Xbox,
            GlyphFamily::PlayStation,
            GlyphFamily::Switch,
            GlyphFamily::SteamDeck,
        ] {
            for b in all {
                assert!(!family.label(b).is_empty());
            }
        }
        assert_eq!(GlyphFamily::Xbox.label(GamepadButton::South), "A");
        assert_eq!(GlyphFamily::PlayStation.label(GamepadButton::South), "Cross");
        assert_eq!(GlyphFamily::Switch.label(GamepadButton::South), "B");
        assert_eq!(GlyphFamily::SteamDeck.label(GamepadButton::Mode), "Steam");
        assert_eq!(
            GlyphFamily::PlayStation.texture_key(GamepadButton::LeftStick),
            "glyphs/playstation/l3"
        );
        assert_eq!(
            GlyphFamily::Switch.texture_key(GamepadButton::Select),
            "glyphs/switch/minus"
        );
        assert_eq!(
            GlyphFamily::Switch.texture_key(GamepadButton::Start),
            "glyphs/switch/plus"
        );
    }

    #[test]
    fn active_glyphs_follow_connected_pads_and_keep_deck_glyphs_for_the_virtual_pad() {
        let mut app = App::new();
        app.init_resource::<SteamDeck>()
            .init_resource::<ActiveGlyphs>()
            .add_message::<GamepadConnected>()
            .add_systems(PreUpdate, track_active_glyphs);
        let connect = |app: &mut App, name: &str| {
            app.world_mut().write_message(GamepadConnected {
                device: 0,
                name: name.to_string(),
            });
            app.update();
        };
        connect(&mut app, "DualSense Wireless Controller");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().family(),
            GlyphFamily::PlayStation
        );
        app.insert_resource(SteamDeck {
            detected: true,
            profile: crate::deck::DeckProfile::DECK,
        });
        connect(&mut app, "Microsoft X-Box 360 pad 0");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().family(),
            GlyphFamily::SteamDeck
        );
        connect(&mut app, "");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().kind,
            ControllerKind::SteamDeck
        );
        let mut glyphs = ActiveGlyphs::default();
        glyphs.on_connected("Pro Controller");
        assert_eq!(glyphs.family(), GlyphFamily::Switch);
    }

    #[test]
    fn the_steam_virtual_gamepad_keeps_deck_glyphs_on_a_deck_only() {
        // `Steam Virtual Gamepad` is the name gilrs reported for the Deck's built-in controls on a
        // Deck OLED (2026-10-29, `input_lab --list-gamepads`). It reads as Generic.
        assert_eq!(
            ControllerKind::from_device_name("Steam Virtual Gamepad"),
            ControllerKind::Generic
        );
        let mut app = App::new();
        app.init_resource::<SteamDeck>()
            .init_resource::<ActiveGlyphs>()
            .add_message::<GamepadConnected>()
            .add_systems(PreUpdate, track_active_glyphs);
        let connect = |app: &mut App, name: &str| {
            app.world_mut().write_message(GamepadConnected {
                device: 0,
                name: name.to_string(),
            });
            app.update();
        };
        // Off a Deck the virtual pad can be any controller behind Steam Input: generic, Xbox glyphs.
        connect(&mut app, "Steam Virtual Gamepad");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().kind,
            ControllerKind::Generic
        );
        assert_eq!(app.world().resource::<ActiveGlyphs>().family(), GlyphFamily::Xbox);
        app.insert_resource(SteamDeck {
            detected: true,
            profile: crate::deck::DeckProfile::DECK,
        });
        connect(&mut app, "Steam Virtual Gamepad");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().family(),
            GlyphFamily::SteamDeck
        );
        // A recognised pad on a Deck still switches the glyphs.
        connect(&mut app, "DualSense Wireless Controller");
        assert_eq!(
            app.world().resource::<ActiveGlyphs>().family(),
            GlyphFamily::PlayStation
        );
    }
}
