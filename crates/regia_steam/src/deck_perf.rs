// SPDX-License-Identifier: MPL-2.0
//! Steam Deck performance presets (ADR-526): power-friendly render budgets a player picks from,
//! and the profile for gamescope's FSR 1 scaling.
//!
//! SteamOS, not the game, sets the APU's power limit (the quick-access menu's TDP slider). What a
//! game controls is how much work it asks for: the frame cap and the render budget. Each
//! [`DeckPerfPreset`] is a frame cap plus a render budget ([`DeckPerfKnobs`]) sized so that cap is
//! reachable well under the power limit the preset names as a hint. The knobs mirror
//! `regia_render::RenderProfile` fields by name; `regia_steam` does not depend on the renderer,
//! so the host copies them onto its profile (book: user-guide/steam-deck.md).
//!
//! FSR 1 on the Deck is gamescope's: the player picks a lower game resolution in the game's Steam
//! properties and the FSR scaling filter in the quick-access menu, and gamescope upscales the
//! game's whole output, UI included, to the panel. The game then sees a window smaller than the
//! panel. [`DeckDisplay`] records that, and [`DeckPerfKnobs::for_output`] raises the render scale
//! so the frame is not scaled down twice (once by the engine, once by gamescope).
//!
//! The power-limit hints and budgets are starting points, not measurements: the Deck pass in
//! `docs/releases/v0.1.29.md` lists what to measure before they are trusted.

use regia_app::prelude::*;
use regia_platform::WindowResized;

use crate::deck::{DeckFrameCap, DeckFrameCapSetting, DeckPanel, SteamDeck};
use crate::deck_prep::{DeckPowerPolicy, DeckUpscaler, LOW_BATTERY_PERCENT, PowerSource, governed_cap};

/// The environment variable a player (or a Steam launch option) sets to pick a preset by name:
/// `REGIA_DECK_PRESET=battery %command%`.
pub const DECK_PRESET_ENV: &str = "REGIA_DECK_PRESET";

/// The environment variable gamescope sets for the programs it runs.
pub const GAMESCOPE_ENV: &str = "GAMESCOPE_WAYLAND_DISPLAY";

/// The Deck panel's size, physical pixels.
pub const DECK_PANEL: (u32, u32) = (1280, 800);

/// A Deck performance preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DeckPerfPreset {
    /// 30 fps, the smallest render budget: the longest battery life.
    BatterySaver,
    /// The panel's native low refresh (40 fps LCD, 45 fps OLED) at the `Deck` render preset's
    /// budget: the default.
    #[default]
    Balanced,
    /// 60 fps with a lower render scale and fewer screen-space effects.
    Performance,
    /// For gamescope's FSR 1 filter: the game runs at a lower 16:10 resolution (960×600 is the
    /// recommended one) at render scale 1, and gamescope upscales to the panel.
    GamescopeFsr1,
}

impl DeckPerfPreset {
    /// Every preset, in menu order.
    pub const ALL: [Self; 4] = [
        Self::BatterySaver,
        Self::Balanced,
        Self::Performance,
        Self::GamescopeFsr1,
    ];

    /// The stable name used by the agent protocol and [`DECK_PRESET_ENV`].
    pub const fn name(self) -> &'static str {
        match self {
            Self::BatterySaver => "battery",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::GamescopeFsr1 => "fsr1",
        }
    }

    /// The preset called `name` (case-insensitive, see [`Self::name`]).
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The starting preset for a machine on `source`: [`Self::BatterySaver`] below
    /// `LOW_BATTERY_PERCENT`, else [`Self::Balanced`].
    pub fn default_for(source: PowerSource) -> Self {
        match source {
            PowerSource::Battery { percent } if percent < LOW_BATTERY_PERCENT => Self::BatterySaver,
            _ => Self::Balanced,
        }
    }

    /// The game resolution the preset expects, physical pixels: the panel, except for
    /// [`Self::GamescopeFsr1`] (960×600, 75 % of the panel per axis).
    pub const fn game_resolution(self) -> (u32, u32) {
        match self {
            Self::GamescopeFsr1 => (960, 600),
            _ => DECK_PANEL,
        }
    }

    /// The preset's render budget on `panel`, before the governor and the output size.
    pub fn knobs(self, panel: DeckPanel) -> DeckPerfKnobs {
        let native_low = match panel {
            DeckPanel::Lcd => DeckFrameCap::Fps40,
            DeckPanel::Oled => DeckFrameCap::Fps45,
        };
        let balanced = DeckPerfKnobs {
            frame_cap: native_low,
            render_scale: crate::deck_prep::DeckRenderDefaults::BASE_RENDER_SCALE,
            upscaler: DeckUpscaler::Tsr,
            csm_cascade_count: 2,
            csm_resolution: 2048,
            ssao: true,
            contact_shadows: true,
            bloom_mip_count: 4,
            local_shadow_max_lights: 8,
            max_visible_lights: 96,
            tdp_hint_watts: Some(12),
        };
        match self {
            Self::Balanced => balanced,
            Self::BatterySaver => DeckPerfKnobs {
                frame_cap: DeckFrameCap::Fps30,
                render_scale: 0.6,
                upscaler: DeckUpscaler::Taau,
                csm_cascade_count: 2,
                csm_resolution: 1024,
                ssao: false,
                contact_shadows: false,
                bloom_mip_count: 3,
                local_shadow_max_lights: 4,
                max_visible_lights: 64,
                tdp_hint_watts: Some(8),
            },
            Self::Performance => DeckPerfKnobs {
                frame_cap: DeckFrameCap::Fps60,
                render_scale: 0.6,
                contact_shadows: false,
                tdp_hint_watts: None,
                ..balanced
            },
            // TAAU at scale 1 is plain TAA: FSR 1 wants anti-aliased input (AMD's FSR 1
            // integration guide), and gamescope does the upscale.
            Self::GamescopeFsr1 => DeckPerfKnobs {
                render_scale: 1.0,
                upscaler: DeckUpscaler::Taau,
                ..balanced
            },
        }
    }
}

/// A preset's render budget. Field names match `regia_render::RenderProfile` where one exists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeckPerfKnobs {
    /// The frame cap.
    pub frame_cap: DeckFrameCap,
    /// Internal render scale per axis, relative to the game's output size.
    pub render_scale: f32,
    /// The temporal upscaler (`RenderProfile::temporal_upscaler`, by [`DeckUpscaler::name`]).
    pub upscaler: DeckUpscaler,
    /// Sun shadow cascades.
    pub csm_cascade_count: u32,
    /// Sun shadow map edge per cascade, texels.
    pub csm_resolution: u32,
    /// Ambient occlusion on (`SsaoMode::HalfRes`) or off.
    pub ssao: bool,
    /// Screen-space contact shadows.
    pub contact_shadows: bool,
    /// Bloom mips (0 = bloom off).
    pub bloom_mip_count: u32,
    /// Local lights with shadows.
    pub local_shadow_max_lights: u32,
    /// Point and spot lights uploaded per view.
    pub max_visible_lights: u32,
    /// The SteamOS power limit (quick-access menu, watts) the preset is meant to hold its cap
    /// under. `None` = the default limit. A hint for the player and the Deck pass, never applied.
    pub tdp_hint_watts: Option<u32>,
}

/// Smallest render scale the knobs ever produce.
pub const MIN_RENDER_SCALE: f32 = 0.5;

impl DeckPerfKnobs {
    /// The knobs with the thermal and battery governor applied: the cap only goes down, the render
    /// scale is multiplied by the policy's factor (never below [`MIN_RENDER_SCALE`]).
    pub fn governed(self, policy: &DeckPowerPolicy) -> Self {
        Self {
            frame_cap: governed_cap(self.frame_cap, policy),
            render_scale: (self.render_scale * policy.render_scale_factor).clamp(MIN_RENDER_SCALE, 1.0),
            ..self
        }
    }

    /// The knobs for a game output of `output` pixels on a panel of `panel` pixels. When gamescope
    /// upscales (the output is smaller than the panel) the render scale rises so the internal
    /// height stays what the preset meant on the panel, and never exceeds the output (scale 1).
    pub fn for_output(self, output: (u32, u32), panel: (u32, u32)) -> Self {
        if output.1 >= panel.1 {
            return self;
        }
        let (out_h, panel_h) = (output.1.max(1) as f32, panel.1 as f32);
        Self {
            render_scale: (self.render_scale * panel_h / out_h).clamp(MIN_RENDER_SCALE, 1.0),
            ..self
        }
    }

    /// The internal render size for an output of `output` pixels, as `RenderProfile::scaled_extent`
    /// computes it.
    pub fn internal_extent(&self, output: (u32, u32)) -> (u32, u32) {
        let scale = |v: u32| ((v as f32) * self.render_scale).round().max(1.0) as u32;
        (scale(output.0), scale(output.1))
    }
}

/// What the game knows about its display on the Deck.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckDisplay {
    /// Running under gamescope (Game Mode, or a `gamescope` launch option).
    pub gamescope: bool,
    /// The game's output size (the primary window), once known.
    pub output: Option<(u32, u32)>,
    /// The panel size (1280×800 on both Deck models).
    pub panel: (u32, u32),
}

impl Default for DeckDisplay {
    fn default() -> Self {
        Self::detect()
    }
}

impl DeckDisplay {
    /// The display for the running process: gamescope from [`GAMESCOPE_ENV`], no output yet.
    pub fn detect() -> Self {
        Self::from_env(|k| regia_core::brand::env_var(k).ok())
    }

    /// [`Self::detect`] over an injectable environment.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            gamescope: get(GAMESCOPE_ENV).is_some_and(|v| !v.is_empty()),
            output: None,
            panel: DECK_PANEL,
        }
    }

    /// Whether gamescope scales the game's output up to the panel: under gamescope with an output
    /// smaller than the panel on both axes (the player chose a lower game resolution).
    pub fn compositor_upscales(&self) -> bool {
        self.gamescope
            && self
                .output
                .is_some_and(|(w, h)| w > 0 && h > 0 && w < self.panel.0 && h < self.panel.1)
    }

    /// Whether the output has the panel's 16:10 shape (within one pixel of rounding).
    pub fn is_16_10(&self) -> bool {
        self.output
            .is_some_and(|(w, h)| h > 0 && (w * 10).abs_diff(h * 16) <= 16)
    }
}

/// The player's preset choice.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckPerfSettings {
    /// The chosen preset.
    pub preset: DeckPerfPreset,
    /// The Deck model's panel. The host sets it from the GPU (`DeckPanel::from_gpu_device_id` on
    /// the adapter's PCI device id); LCD until then.
    pub panel: DeckPanel,
    /// Whether choosing a preset writes its frame cap into `DeckFrameCapSetting`. Starting on
    /// [`DeckPerfPreset::Balanced`] writes nothing (its cap is the engine default).
    pub drive_frame_cap: bool,
}

impl Default for DeckPerfSettings {
    fn default() -> Self {
        Self::from_env(|k| regia_core::brand::env_var(k).ok())
    }
}

impl DeckPerfSettings {
    /// The preset named by [`DECK_PRESET_ENV`], else [`DeckPerfPreset::Balanced`]; the preset
    /// drives the frame cap. An unknown name is logged and ignored.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let preset = match get(DECK_PRESET_ENV) {
            Some(name) => DeckPerfPreset::from_name(&name).unwrap_or_else(|| {
                tracing::warn!(
                    target: "regia_steam",
                    "{DECK_PRESET_ENV}={name:?} is not a preset (battery, balanced, performance, fsr1); using balanced"
                );
                DeckPerfPreset::Balanced
            }),
            None => DeckPerfPreset::Balanced,
        };
        Self {
            preset,
            panel: DeckPanel::Lcd,
            drive_frame_cap: true,
        }
    }
}

/// The knobs in force: the preset's, governed and fitted to the output. The host copies them onto
/// its render profile when this resource changes.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct DeckPerfState {
    /// The preset the knobs come from.
    pub preset: DeckPerfPreset,
    /// The effective knobs.
    pub knobs: DeckPerfKnobs,
    /// Gamescope scales the output up (see [`DeckDisplay::compositor_upscales`]).
    pub compositor_upscales: bool,
}

impl Default for DeckPerfState {
    fn default() -> Self {
        Self::resolve(
            DeckPerfPreset::Balanced,
            DeckPanel::Lcd,
            &DeckPowerPolicy::default(),
            &DeckDisplay::from_env(|_| None),
        )
    }
}

impl DeckPerfState {
    /// The state for `preset` on `panel` under `policy`, fitted to `display`.
    pub fn resolve(
        preset: DeckPerfPreset,
        panel: DeckPanel,
        policy: &DeckPowerPolicy,
        display: &DeckDisplay,
    ) -> Self {
        let upscales = display.compositor_upscales();
        let mut knobs = preset.knobs(panel).governed(policy);
        if upscales && let Some(output) = display.output {
            knobs = knobs.for_output(output, display.panel);
        }
        Self {
            preset,
            knobs,
            compositor_upscales: upscales,
        }
    }
}

/// `PreUpdate`: follows the primary window's size into [`DeckDisplay`].
pub fn track_deck_display(mut resized: MessageReader<WindowResized>, mut display: ResMut<DeckDisplay>) {
    if let Some(last) = resized.read().last() {
        let output = Some((last.width, last.height));
        if display.output != output {
            display.output = output;
        }
    }
}

/// `PreUpdate`, on a Deck: recomputes [`DeckPerfState`] from the preset, the panel, the power
/// policy and the display, and writes the frame cap of a newly chosen preset into
/// [`DeckFrameCapSetting`] (the player can still pick another cap afterwards).
pub fn update_deck_perf(
    deck: Res<SteamDeck>,
    settings: Res<DeckPerfSettings>,
    policy: Res<DeckPowerPolicy>,
    display: Res<DeckDisplay>,
    mut applied: Local<Option<DeckPerfPreset>>,
    mut state: ResMut<DeckPerfState>,
    mut cap: ResMut<DeckFrameCapSetting>,
) {
    if !deck.detected {
        return;
    }
    let panel = settings.panel;
    let next = DeckPerfState::resolve(settings.preset, panel, &policy, &display);
    // The engine's default cap is Balanced's, so the first applied preset counts as Balanced: a
    // startup on Balanced keeps a cap the game or player already set, any other preset (from
    // `REGIA_DECK_PRESET` or the settings) and every later switch writes its own cap.
    let last = applied.get_or_insert(DeckPerfPreset::Balanced);
    if *last != settings.preset {
        *last = settings.preset;
        let preset_cap = settings.preset.knobs(panel).frame_cap;
        if settings.drive_frame_cap && cap.cap != preset_cap {
            cap.cap = preset_cap;
        }
    }
    if *state != next {
        tracing::info!(target: "regia_steam", "Deck perf: {} {:?}", next.preset.name(), next.knobs);
        *state = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck_prep::{DeckRenderDefaults, PowerReason};

    #[test]
    fn names_round_trip_and_unknown_names_fail() {
        for preset in DeckPerfPreset::ALL {
            assert_eq!(DeckPerfPreset::from_name(preset.name()), Some(preset));
            assert!(
                preset
                    .name()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
        }
        assert_eq!(
            DeckPerfPreset::from_name(" Battery "),
            Some(DeckPerfPreset::BatterySaver)
        );
        assert_eq!(DeckPerfPreset::from_name("turbo"), None);
    }

    #[test]
    fn balanced_matches_the_deck_starting_defaults() {
        for panel in [DeckPanel::Lcd, DeckPanel::Oled] {
            let defaults = DeckRenderDefaults::select(panel, PowerSource::Mains);
            let knobs = DeckPerfPreset::Balanced.knobs(panel);
            assert_eq!(knobs.frame_cap, defaults.frame_cap);
            assert_eq!(knobs.render_scale, defaults.render_scale);
            assert_eq!(knobs.upscaler, defaults.upscaler);
        }
        assert_eq!(
            DeckPerfPreset::default_for(PowerSource::Battery { percent: 9 }),
            DeckPerfPreset::BatterySaver
        );
        assert_eq!(
            DeckPerfPreset::default_for(PowerSource::Mains),
            DeckPerfPreset::Balanced
        );
    }

    #[test]
    fn presets_are_ordered_by_cost() {
        let lcd = |p: DeckPerfPreset| p.knobs(DeckPanel::Lcd);
        let saver = lcd(DeckPerfPreset::BatterySaver);
        let balanced = lcd(DeckPerfPreset::Balanced);
        let perf = lcd(DeckPerfPreset::Performance);
        // Pixels per second the preset asks for at the panel size.
        let rate = |k: DeckPerfKnobs| {
            let (w, h) = k.internal_extent(DECK_PANEL);
            u64::from(w * h) * u64::from(k.frame_cap.fps())
        };
        assert!(rate(saver) < rate(balanced), "battery saver is the cheapest");
        assert!(
            rate(balanced) < rate(perf),
            "performance asks for the most frames"
        );
        assert!(saver.csm_resolution < balanced.csm_resolution);
        assert!(!saver.ssao && balanced.ssao);
        assert!(saver.tdp_hint_watts < balanced.tdp_hint_watts);
        assert_eq!(perf.tdp_hint_watts, None);
    }

    #[test]
    fn the_governor_only_lowers_the_budget() {
        let hot = DeckPowerPolicy {
            cap: DeckFrameCap::Fps30,
            render_scale_factor: 0.85,
            reason: PowerReason::Thermal,
        };
        let knobs = DeckPerfPreset::Performance.knobs(DeckPanel::Lcd).governed(&hot);
        assert_eq!(knobs.frame_cap, DeckFrameCap::Fps30);
        assert!((knobs.render_scale - 0.51).abs() < 1e-6);
        let floor = DeckPerfKnobs {
            render_scale: 0.4,
            ..knobs
        }
        .governed(&hot);
        assert_eq!(floor.render_scale, MIN_RENDER_SCALE);
        let calm = DeckPerfPreset::BatterySaver.knobs(DeckPanel::Lcd);
        assert_eq!(calm.governed(&DeckPowerPolicy::default()), calm);
    }

    #[test]
    fn gamescope_scaling_is_detected_from_the_env_and_the_output() {
        let env = |k: &str| (k == GAMESCOPE_ENV).then(|| "gamescope-0".to_owned());
        let mut display = DeckDisplay::from_env(env);
        assert!(display.gamescope);
        assert!(!display.compositor_upscales(), "no output yet");
        display.output = Some(DECK_PANEL);
        assert!(!display.compositor_upscales(), "native size");
        assert!(display.is_16_10());
        display.output = Some((960, 600));
        assert!(display.compositor_upscales());
        assert!(display.is_16_10());
        display.output = Some((1280, 720));
        assert!(
            !display.compositor_upscales(),
            "same width: letterboxed, not scaled"
        );
        assert!(!display.is_16_10());
        let desktop = DeckDisplay {
            output: Some((960, 600)),
            ..DeckDisplay::from_env(|_| None)
        };
        assert!(
            !desktop.compositor_upscales(),
            "a small desktop window is not gamescope"
        );
    }

    #[test]
    fn the_fsr1_profile_renders_the_lower_resolution_natively() {
        let env = |k: &str| (k == GAMESCOPE_ENV).then(|| "gamescope-0".to_owned());
        let display = DeckDisplay {
            output: Some(DeckPerfPreset::GamescopeFsr1.game_resolution()),
            ..DeckDisplay::from_env(env)
        };
        let policy = DeckPowerPolicy::default();
        let fsr1 = DeckPerfState::resolve(DeckPerfPreset::GamescopeFsr1, DeckPanel::Lcd, &policy, &display);
        assert!(fsr1.compositor_upscales);
        assert_eq!(fsr1.knobs.render_scale, 1.0);
        assert_eq!(fsr1.knobs.internal_extent((960, 600)), (960, 600));
        // Balanced at 960×600 would render 672×420 and gamescope would upscale that again; the
        // fitted scale keeps about the panel-preset's 560 internal lines instead.
        let balanced = DeckPerfState::resolve(DeckPerfPreset::Balanced, DeckPanel::Lcd, &policy, &display);
        let (_, h) = balanced.knobs.internal_extent((960, 600));
        assert_eq!(h, 560);
        // A 640×400 output (FSR 1 "performance") clamps at scale 1, never supersamples.
        let tiny = DeckPerfPreset::Balanced
            .knobs(DeckPanel::Lcd)
            .for_output((640, 400), DECK_PANEL);
        assert_eq!(tiny.render_scale, 1.0);
        // At the panel size nothing changes.
        let native = DeckPerfPreset::Balanced.knobs(DeckPanel::Lcd);
        assert_eq!(native.for_output(DECK_PANEL, DECK_PANEL), native);
    }

    #[test]
    fn settings_come_from_the_env() {
        let pick = |v: &'static str| move |k: &str| (k == DECK_PRESET_ENV).then(|| v.to_owned());
        assert_eq!(
            DeckPerfSettings::from_env(pick("fsr1")).preset,
            DeckPerfPreset::GamescopeFsr1
        );
        assert_eq!(
            DeckPerfSettings::from_env(pick("nope")).preset,
            DeckPerfPreset::Balanced
        );
        assert_eq!(
            DeckPerfSettings::from_env(|_| None).preset,
            DeckPerfPreset::Balanced
        );
        assert!(DeckPerfSettings::from_env(|_| None).drive_frame_cap);
    }
}
