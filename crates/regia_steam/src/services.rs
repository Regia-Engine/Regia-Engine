// SPDX-License-Identifier: MPL-2.0
//! App and DLC ownership and the game-recording timeline (`docs/specs/24-steam.md` § 2a,
//! ADR-2340). Both are thin, synchronous ISteamApps / ISteamTimeline calls; leaderboards are in
//! [`crate::leaderboards`].
//!
//! ```ignore
//! fn dlc_menu(steam: Res<Steam>) {
//!     let owns_soundtrack = steam.dlc_installed(AppId(1_234_560)).unwrap_or(false);
//! }
//!
//! fn on_boss_down(steam: Res<Steam>) {
//!     steam.timeline_event(&TimelineEvent::new("Boss down").icon("steam_achievement").clip(TimelineClip::Featured)).ok();
//! }
//! ```
//!
//! Every call degrades to [`SteamError::Unavailable`] without Steam. Steam's timeline has no read
//! back, so [`Steam::timeline_log`] keeps what the engine last sent (the agent's `steam.timeline`).

use crate::backend::{AppId, SteamError};
use crate::resource::{Steam, check_name};

/// What Steam reports about one app or DLC for the signed-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppOwnership {
    /// The app asked about.
    pub app: AppId,
    /// The user owns (is subscribed to) the app (`BIsSubscribedApp`). For the running game's own
    /// app id this is true for any licensed launch.
    pub subscribed: bool,
    /// The app is installed (`BIsAppInstalled`).
    pub installed: bool,
    /// The user owns the DLC and it is installed (`BIsDlcInstalled`); false for apps that are
    /// not DLC of this game.
    pub dlc_installed: bool,
    /// Where the app is or would be installed (`GetAppInstallDir`); `None` when Steam returns
    /// nothing.
    pub install_dir: Option<String>,
}

/// Steam's timeline bar color for the current stretch of play (`ETimelineGameMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimelineMode {
    /// In the game, playing.
    Playing,
    /// In a multiplayer lobby or staging area.
    Staging,
    /// In the main menu or a pause menu.
    Menus,
    /// Waiting on a loading screen.
    LoadingScreen,
}

impl TimelineMode {
    /// `playing`, `staging`, `menus` or `loading_screen`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Staging => "staging",
            Self::Menus => "menus",
            Self::LoadingScreen => "loading_screen",
        }
    }

    /// Parses [`TimelineMode::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "playing" => Some(Self::Playing),
            "staging" => Some(Self::Staging),
            "menus" => Some(Self::Menus),
            "loading_screen" => Some(Self::LoadingScreen),
            _ => None,
        }
    }
}

/// How strongly Steam offers a clip around a timeline event (`ETimelineEventClipPriority`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TimelineClip {
    /// Not a clip moment.
    None,
    /// The player may want a clip. The default.
    #[default]
    Standard,
    /// A highlight: promoted above standard clips.
    Featured,
}

impl TimelineClip {
    /// `none`, `standard` or `featured`.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Standard => "standard",
            Self::Featured => "featured",
        }
    }

    /// Parses [`TimelineClip::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "standard" => Some(Self::Standard),
            "featured" => Some(Self::Featured),
            _ => None,
        }
    }
}

/// Most bytes of a timeline icon name or title (a Regia Engine cap).
pub const MAX_TIMELINE_NAME_BYTES: usize = 255;
/// Most bytes of a timeline description or state text (a Regia Engine cap).
pub const MAX_TIMELINE_TEXT_BYTES: usize = 1024;
/// Highest event priority Steam accepts (`k_unMaxTimelinePriority`).
pub const MAX_TIMELINE_PRIORITY: u32 = 1000;
/// Furthest a timeline offset or duration may reach, in seconds (a Regia Engine cap: an hour
/// away is almost always a unit mistake).
pub const MAX_TIMELINE_SECONDS: f32 = 3600.0;

/// One timeline event (`AddRangeTimelineEvent`; a zero duration is an instant marker).
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEvent {
    /// Steam icon name (`steam_marker`, `steam_achievement`, ... or one the game uploaded).
    pub icon: String,
    /// Short title shown on the timeline.
    pub title: String,
    /// Longer text; may be empty.
    pub description: String,
    /// Higher shows first when events overlap; `0..=1000`.
    pub priority: u32,
    /// Seconds from now to the event's start; negative marks the past.
    pub start_offset: f32,
    /// Length in seconds; `0` for an instant.
    pub duration: f32,
    /// Clip offer.
    pub clip: TimelineClip,
}

impl TimelineEvent {
    /// An instant `steam_marker` event at "now", priority 0, standard clip.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            icon: "steam_marker".to_owned(),
            title: title.into(),
            description: String::new(),
            priority: 0,
            start_offset: 0.0,
            duration: 0.0,
            clip: TimelineClip::Standard,
        }
    }

    /// Sets the icon.
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = icon.into();
        self
    }

    /// Sets the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the priority (`0..=1000`).
    #[must_use]
    pub fn priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    /// Places the event `start_offset` seconds from now and makes it `duration` long.
    #[must_use]
    pub fn span(mut self, start_offset: f32, duration: f32) -> Self {
        self.start_offset = start_offset;
        self.duration = duration;
        self
    }

    /// Sets the clip offer.
    #[must_use]
    pub fn clip(mut self, clip: TimelineClip) -> Self {
        self.clip = clip;
        self
    }

    /// Checks the fields against Steam's and Regia Engine's limits.
    pub fn check(&self) -> Result<(), SteamError> {
        check_timeline_name("timeline icon", &self.icon)?;
        check_timeline_name("timeline title", &self.title)?;
        check_timeline_text("timeline description", &self.description, true)?;
        if self.priority > MAX_TIMELINE_PRIORITY {
            return Err(SteamError::Rejected(format!(
                "timeline priority {} is above {MAX_TIMELINE_PRIORITY}",
                self.priority
            )));
        }
        check_seconds("timeline start offset", self.start_offset, true)?;
        check_seconds("timeline duration", self.duration, false)
    }
}

/// One call to Steam's timeline, as the backend receives it.
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineCall {
    /// `SetTimelineGameMode`.
    Mode(TimelineMode),
    /// `SetTimelineTooltip`: describes the game state from `delta` seconds from now on.
    State {
        /// The text.
        description: String,
        /// Seconds from now (`>= 0`: the bindings take an unsigned duration).
        delta: f32,
    },
    /// `ClearTimelineTooltip` from `delta` seconds from now on.
    ClearState {
        /// Seconds from now (`>= 0`).
        delta: f32,
    },
    /// `AddRangeTimelineEvent`.
    Event(TimelineEvent),
}

/// What the engine last sent to Steam's timeline (Steam has no read back).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelineLog {
    /// The last game mode set.
    pub mode: Option<TimelineMode>,
    /// The state text in force (`None` after a clear).
    pub state: Option<String>,
    /// Events sent.
    pub events: u64,
    /// The last event sent.
    pub last_event: Option<TimelineEvent>,
    /// Calls the backend refused (Steam unavailable).
    pub refused: u64,
}

fn check_timeline_name(what: &str, name: &str) -> Result<(), SteamError> {
    check_name(what, name)?;
    if name.len() > MAX_TIMELINE_NAME_BYTES {
        return Err(SteamError::Rejected(format!(
            "{what} must be at most {MAX_TIMELINE_NAME_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_timeline_text(what: &str, text: &str, empty_ok: bool) -> Result<(), SteamError> {
    if (!empty_ok && text.is_empty()) || text.contains('\0') || text.len() > MAX_TIMELINE_TEXT_BYTES {
        return Err(SteamError::Rejected(format!(
            "{what} must be {}..={MAX_TIMELINE_TEXT_BYTES} bytes without NUL",
            u8::from(!empty_ok)
        )));
    }
    Ok(())
}

fn check_seconds(what: &str, seconds: f32, negative_ok: bool) -> Result<(), SteamError> {
    let low = if negative_ok { -MAX_TIMELINE_SECONDS } else { 0.0 };
    if !seconds.is_finite() || seconds < low || seconds > MAX_TIMELINE_SECONDS {
        return Err(SteamError::Rejected(format!(
            "{what} must be finite and within {low}..={MAX_TIMELINE_SECONDS} seconds, got {seconds}"
        )));
    }
    Ok(())
}

fn check_app(app: AppId) -> Result<(), SteamError> {
    if app.0 == 0 {
        return Err(SteamError::Rejected("app id 0 is not an app".into()));
    }
    Ok(())
}

impl Steam {
    /// Whether the signed-in user owns `app` (`BIsSubscribedApp`): use it for a related game or
    /// a demo, and [`Steam::dlc_installed`] for this game's DLC.
    pub fn owns_app(&self, app: AppId) -> Result<bool, SteamError> {
        check_app(app)?;
        self.live()?;
        self.backend.is_subscribed_app(app)
    }

    /// Whether the user owns DLC `app` and it is installed (`BIsDlcInstalled`).
    pub fn dlc_installed(&self, app: AppId) -> Result<bool, SteamError> {
        check_app(app)?;
        self.live()?;
        self.backend.is_dlc_installed(app)
    }

    /// Whether `app` is installed (`BIsAppInstalled`; not proof of ownership).
    pub fn app_installed(&self, app: AppId) -> Result<bool, SteamError> {
        check_app(app)?;
        self.live()?;
        self.backend.is_app_installed(app)
    }

    /// Where `app` is (or would be) installed; `None` when Steam returns nothing.
    pub fn app_install_dir(&self, app: AppId) -> Result<Option<String>, SteamError> {
        check_app(app)?;
        self.live()?;
        self.backend.app_install_dir(app)
    }

    /// Every ownership fact about `app` in one call.
    pub fn app_ownership(&self, app: AppId) -> Result<AppOwnership, SteamError> {
        Ok(AppOwnership {
            app,
            subscribed: self.owns_app(app)?,
            installed: self.app_installed(app)?,
            dlc_installed: self.dlc_installed(app)?,
            install_dir: self.app_install_dir(app)?,
        })
    }

    /// Sends one timeline call and records it in [`Steam::timeline_log`].
    fn send_timeline(&self, call: TimelineCall) -> Result<(), SteamError> {
        let sent = self.live().and_then(|()| self.backend.timeline(&call));
        let mut log = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
        match &sent {
            Ok(()) => match call {
                TimelineCall::Mode(mode) => log.mode = Some(mode),
                TimelineCall::State { description, .. } => log.state = Some(description),
                TimelineCall::ClearState { .. } => log.state = None,
                TimelineCall::Event(event) => {
                    log.events = log.events.saturating_add(1);
                    log.last_event = Some(event);
                }
            },
            Err(_) => log.refused = log.refused.saturating_add(1),
        }
        sent
    }

    /// Sets the timeline's game mode (the bar color: playing, staging, menus, loading).
    pub fn timeline_mode(&self, mode: TimelineMode) -> Result<(), SteamError> {
        self.send_timeline(TimelineCall::Mode(mode))
    }

    /// Describes the game state on the timeline from `delta` seconds from now (`0` = now).
    pub fn timeline_state(&self, description: &str, delta: f32) -> Result<(), SteamError> {
        check_timeline_text("timeline state", description, false)?;
        check_seconds("timeline state delta", delta, false)?;
        self.send_timeline(TimelineCall::State {
            description: description.to_owned(),
            delta,
        })
    }

    /// Clears the timeline's state description from `delta` seconds from now.
    pub fn timeline_clear_state(&self, delta: f32) -> Result<(), SteamError> {
        check_seconds("timeline state delta", delta, false)?;
        self.send_timeline(TimelineCall::ClearState { delta })
    }

    /// Marks an event (an instant or a range) on the timeline.
    pub fn timeline_event(&self, event: &TimelineEvent) -> Result<(), SteamError> {
        event.check()?;
        self.send_timeline(TimelineCall::Event(event.clone()))
    }

    /// What the engine last sent to the timeline.
    pub fn timeline_log(&self) -> TimelineLog {
        self.timeline.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_event_limits() {
        assert!(TimelineEvent::new("Boss down").check().is_ok());
        assert!(TimelineEvent::new("").check().is_err());
        assert!(TimelineEvent::new("a\0b").check().is_err());
        assert!(TimelineEvent::new("x").icon("").check().is_err());
        assert!(TimelineEvent::new("x").priority(1000).check().is_ok());
        assert!(TimelineEvent::new("x").priority(1001).check().is_err());
        assert!(TimelineEvent::new("x").span(-10.0, 5.0).check().is_ok());
        assert!(TimelineEvent::new("x").span(0.0, -1.0).check().is_err());
        assert!(TimelineEvent::new("x").span(f32::NAN, 0.0).check().is_err());
        assert!(TimelineEvent::new("x").span(0.0, 3601.0).check().is_err());
        assert!(
            TimelineEvent::new("x")
                .description("d".repeat(MAX_TIMELINE_TEXT_BYTES + 1))
                .check()
                .is_err()
        );
    }

    #[test]
    fn names_round_trip() {
        for mode in [
            TimelineMode::Playing,
            TimelineMode::Staging,
            TimelineMode::Menus,
            TimelineMode::LoadingScreen,
        ] {
            assert_eq!(TimelineMode::from_name(mode.name()), Some(mode));
        }
        for clip in [TimelineClip::None, TimelineClip::Standard, TimelineClip::Featured] {
            assert_eq!(TimelineClip::from_name(clip.name()), Some(clip));
        }
    }
}
