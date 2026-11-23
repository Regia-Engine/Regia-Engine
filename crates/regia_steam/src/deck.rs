// SPDX-License-Identifier: MPL-2.0
//! Steam Deck detection (specs/24 § 2 "Steam Deck"): the [`SteamDeck`] resource carries the
//! detection flag and the default `Deck` render profile the renderer should switch to —
//! 1280×800, 30/40/60 fps caps, an FSR-style upscale (specs/24; the render-profile plumbing
//! itself belongs to `regia_render`/packaging).

use regia_app::prelude::*;

/// The default render profile for the Steam Deck (specs/24 § 2): native 1280×800 with the fps
/// caps the Deck's refresh tiers imply, upscaled FSR-style when the game opts in.
///
/// Values derive from Valve's public Steam Deck specifications (1280 × 800 LCD panel, 60 Hz) and
/// spec 24's Deck profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckProfile {
    /// Native render resolution (width, height).
    pub resolution: (u16, u16),
    /// Frame-rate caps offered to the player (fps), in ascending order.
    pub fps_caps: [u32; 3],
    /// Whether the profile upscales from a lower internal resolution (FSR-style) by default.
    pub upscale: bool,
}

impl DeckProfile {
    /// The default profile from spec 24: 1280×800, caps 30/40/60, upscaling on.
    pub const DECK: DeckProfile = DeckProfile {
        resolution: (1280, 800),
        fps_caps: [30, 40, 60],
        upscale: true,
    };
}

/// Which Steam Deck display. Valve's public specifications: the LCD model refreshes at 40–60 Hz,
/// the OLED model at 45–90 Hz (both 800 px tall; the OLED panel is 1280×800 too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeckPanel {
    /// LCD model (APU "Van Gogh", PCI device 0x163f): 40–60 Hz.
    #[default]
    Lcd,
    /// OLED model (APU "Sephiroth", PCI device 0x1435): 45–90 Hz.
    Oled,
}

impl DeckPanel {
    /// The panel of the GPU with PCI device id `device` (AMD vendor), `None` for other GPUs.
    pub fn from_gpu_device_id(device: u32) -> Option<Self> {
        match device {
            0x163f => Some(Self::Lcd),
            0x1435 => Some(Self::Oled),
            _ => None,
        }
    }

    /// The panel's variable-refresh range, Hz (inclusive).
    pub const fn refresh_range_hz(self) -> (u32, u32) {
        match self {
            Self::Lcd => (40, 60),
            Self::Oled => (45, 90),
        }
    }
}

/// The frame caps the Deck's menu offers (the Deck's own system limiter offers 30/40/45/60 too).
///
/// The engine cap and SteamOS's limiter must agree, or the two beat against each other: an engine
/// cap above the OS limit queues frames in the compositor (latency, uneven pacing), one far below it
/// wastes refresh. [`Self::effective_fps`] therefore never exceeds the refresh rate the display
/// actually runs at, and [`Self::refresh_for`] names the refresh rate a game should expect (and ask
/// gamescope for) at each cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeckFrameCap {
    /// 30 fps: half of a 60 Hz refresh.
    Fps30,
    /// 40 fps: the Deck default (`RenderProfilePreset::Deck`), a native LCD refresh.
    #[default]
    Fps40,
    /// 45 fps: native on both panels (the OLED panel's lowest refresh).
    Fps45,
    /// 60 fps.
    Fps60,
}

impl DeckFrameCap {
    /// Every cap, ascending.
    pub const ALL: [Self; 4] = [Self::Fps30, Self::Fps40, Self::Fps45, Self::Fps60];

    /// The cap in frames per second.
    pub const fn fps(self) -> u32 {
        match self {
            Self::Fps30 => 30,
            Self::Fps40 => 40,
            Self::Fps45 => 45,
            Self::Fps60 => 60,
        }
    }

    /// The refresh rate, Hz, `panel` runs at for this cap: the cap itself when the panel can refresh
    /// that slowly, else the smallest whole multiple inside the panel's range (30 → 60 on both).
    pub fn refresh_for(self, panel: DeckPanel) -> u32 {
        let (min, max) = panel.refresh_range_hz();
        let fps = self.fps();
        (1..=8)
            .map(|k| fps * k)
            .find(|hz| (min..=max).contains(hz))
            .unwrap_or(max)
    }

    /// The caps `panel` can show without a frame-rate mismatch (the cap is a whole divisor of some
    /// refresh the panel supports).
    pub fn supported(panel: DeckPanel) -> Vec<Self> {
        let (min, max) = panel.refresh_range_hz();
        Self::ALL
            .into_iter()
            .filter(|c| (1..=8).any(|k| (min..=max).contains(&(c.fps() * k))))
            .collect()
    }

    /// The fps to pace at when the display is known to run at `refresh_hz`: the cap, but never above
    /// the refresh (a lower OS limiter wins; the engine follows it instead of queueing frames).
    pub fn effective_fps(self, refresh_hz: Option<f32>) -> f32 {
        let cap = self.fps() as f32;
        match refresh_hz {
            Some(hz) if hz.is_finite() && hz > 1.0 && hz < cap => hz,
            _ => cap,
        }
    }

    /// Applies the cap to the app's [`FramePacing`].
    pub fn apply(self, pacing: &mut FramePacing, refresh_hz: Option<f32>) {
        pacing.target_fps = Some(self.effective_fps(refresh_hz));
    }
}

/// The player's frame cap on the Deck, applied to [`FramePacing`] by [`apply_deck_frame_cap`]
/// whenever it or the detection result changes. A game's settings menu writes `cap`
/// ([`DeckFrameCap::ALL`] are the choices); `display_refresh_hz` is the refresh rate the host reads
/// from the monitor (winit's `refresh_rate_millihertz`) so the engine follows a lower SteamOS limiter.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct DeckFrameCapSetting {
    /// The chosen cap.
    pub cap: DeckFrameCap,
    /// The display's current refresh rate, if the host knows it.
    pub display_refresh_hz: Option<f32>,
}

/// Run condition of [`apply_deck_frame_cap`]: the setting or the detection changed.
pub fn deck_cap_dirty(
    deck: Res<SteamDeck>,
    setting: Res<DeckFrameCapSetting>,
    policy: Option<Res<crate::deck_prep::DeckPowerPolicy>>,
) -> bool {
    deck.is_changed() || setting.is_changed() || policy.is_some_and(|p| p.is_changed())
}

/// `PreUpdate` (when the setting or the detection changed): paces the frame loop at the cap on a
/// Deck; leaves [`FramePacing`] alone elsewhere.
pub fn apply_deck_frame_cap(
    deck: Res<SteamDeck>,
    setting: Res<DeckFrameCapSetting>,
    policy: Option<Res<crate::deck_prep::DeckPowerPolicy>>,
    mut pacing: ResMut<FramePacing>,
) {
    if !deck.detected {
        return;
    }
    let cap = policy.map_or(setting.cap, |p| crate::deck_prep::governed_cap(setting.cap, &p));
    cap.apply(&mut pacing, setting.display_refresh_hz);
    tracing::info!(
        target: "regia_steam",
        "Deck frame cap {} fps (display {:?} Hz)",
        pacing.target_fps.unwrap_or(0.0),
        setting.display_refresh_hz
    );
}

/// Steam Deck detection result, written by [`crate::SteamPlugin`] at init. The renderer and
/// packaging lanes read this to select the `Deck` render profile (`Regia.toml`
/// `render_profile = "Deck"`) and to apply Deck-specific defaults (controller-first navigation,
/// `ShowFloatingGamepadTextInput` for text fields, minimum text sizes in `regia_ui`).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamDeck {
    /// Whether the game is running on a Steam Deck.
    pub detected: bool,
    /// The render profile to use on the Deck (only meaningful when `detected`).
    pub profile: DeckProfile,
}

impl SteamDeck {
    /// A detection result for a non-Deck machine.
    pub const NOT_DECK: SteamDeck = SteamDeck {
        detected: false,
        profile: DeckProfile::DECK,
    };
}

impl Default for SteamDeck {
    /// A non-Deck machine until the backend says otherwise (`init_resource` needs this; the
    /// plugin overwrites the resource at init).
    fn default() -> Self {
        Self::NOT_DECK
    }
}

/// Raised once at init when the game is running on a Steam Deck, so UI and settings can apply
/// controller-first defaults before the first frame.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamDeckDetected;

/// Detects the Deck through the backend. The renderer lane consumes [`SteamDeck`] (M5/M16); this
/// milestone delivers the flag, the profile values and the message.
///
/// `steam_deck_env` is the `SteamDeck` environment variable (Steam sets `SteamDeck=1` in the game's
/// environment on a Deck): `regia_ui` and the renderer's calibration already key off it, so it
/// counts here too, even when the Steam API did not run (feature off, client not running).
pub(crate) fn detect(backend: &dyn crate::backend::SteamBackend, steam_deck_env: Option<&str>) -> SteamDeck {
    if backend.is_steam_deck() || steam_deck_env == Some("1") {
        SteamDeck {
            detected: true,
            profile: DeckProfile::DECK,
        }
    } else {
        SteamDeck::NOT_DECK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panels_by_gpu_and_refresh_range() {
        assert_eq!(DeckPanel::from_gpu_device_id(0x163f), Some(DeckPanel::Lcd));
        assert_eq!(DeckPanel::from_gpu_device_id(0x1435), Some(DeckPanel::Oled));
        assert_eq!(DeckPanel::from_gpu_device_id(0x744c), None);
        assert_eq!(DeckPanel::Lcd.refresh_range_hz(), (40, 60));
        assert_eq!(DeckPanel::Oled.refresh_range_hz(), (45, 90));
    }

    #[test]
    fn refresh_for_each_cap() {
        use DeckFrameCap::*;
        // 30 fps is no native refresh on either panel: it runs at 60 Hz, every other vblank.
        assert_eq!(Fps30.refresh_for(DeckPanel::Lcd), 60);
        assert_eq!(Fps30.refresh_for(DeckPanel::Oled), 60);
        assert_eq!(Fps40.refresh_for(DeckPanel::Lcd), 40);
        // The OLED panel cannot refresh at 40 Hz: it shows 40 fps at 80 Hz.
        assert_eq!(Fps40.refresh_for(DeckPanel::Oled), 80);
        assert_eq!(Fps45.refresh_for(DeckPanel::Oled), 45);
        assert_eq!(Fps60.refresh_for(DeckPanel::Lcd), 60);
        assert_eq!(DeckFrameCap::supported(DeckPanel::Lcd), DeckFrameCap::ALL);
        assert_eq!(DeckFrameCap::supported(DeckPanel::Oled), DeckFrameCap::ALL);
    }

    #[test]
    fn engine_cap_follows_a_lower_os_limiter() {
        let cap = DeckFrameCap::Fps60;
        assert_eq!(cap.effective_fps(None), 60.0);
        assert_eq!(cap.effective_fps(Some(60.0)), 60.0);
        assert_eq!(cap.effective_fps(Some(144.0)), 60.0);
        assert_eq!(cap.effective_fps(Some(40.0)), 40.0);
        assert_eq!(cap.effective_fps(Some(f32::NAN)), 60.0);
        let mut pacing = FramePacing::default();
        DeckFrameCap::Fps45.apply(&mut pacing, Some(45.0));
        assert_eq!(pacing.target_fps, Some(45.0));
        assert_eq!(DeckFrameCap::default().fps(), DeckProfile::DECK.fps_caps[1]);
    }

    #[test]
    fn effective_fps_never_exceeds_the_reported_refresh() {
        // winit reports millihertz: 59.97 Hz must win over the 60 fps cap.
        assert_eq!(DeckFrameCap::Fps60.effective_fps(Some(59.97)), 59.97);
        assert_eq!(DeckFrameCap::Fps60.effective_fps(Some(60.0)), 60.0);
    }

    #[test]
    fn the_steam_deck_env_counts_as_detected_without_the_api() {
        let backend = crate::backend::UnavailableBackend;
        assert!(detect(&backend, Some("1")).detected);
        assert!(!detect(&backend, Some("0")).detected);
        assert!(!detect(&backend, None).detected);
    }

    #[test]
    fn the_cap_system_paces_only_on_a_deck_and_follows_changes() {
        let mut app = App::new();
        app.init_resource::<SteamDeck>()
            .init_resource::<DeckFrameCapSetting>()
            .add_systems(PreUpdate, apply_deck_frame_cap.run_if(deck_cap_dirty));
        app.update();
        assert_eq!(
            app.world().resource::<FramePacing>().target_fps,
            None,
            "not a Deck: untouched"
        );
        app.insert_resource(SteamDeck {
            detected: true,
            profile: DeckProfile::DECK,
        });
        app.update();
        assert_eq!(
            app.world().resource::<FramePacing>().target_fps,
            Some(40.0),
            "the default cap"
        );
        app.world_mut().resource_mut::<DeckFrameCapSetting>().cap = DeckFrameCap::Fps60;
        app.world_mut()
            .resource_mut::<DeckFrameCapSetting>()
            .display_refresh_hz = Some(45.0);
        app.update();
        assert_eq!(
            app.world().resource::<FramePacing>().target_fps,
            Some(45.0),
            "follows the lower limiter"
        );
    }

    #[test]
    fn deck_profile_matches_spec_24() {
        let deck = DeckProfile::DECK;
        assert_eq!(deck.resolution, (1280, 800));
        assert_eq!(deck.fps_caps, [30, 40, 60]);
        assert!(deck.upscale);
        let not_deck = SteamDeck::NOT_DECK;
        assert!(!not_deck.detected);
    }
}
