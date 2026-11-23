// SPDX-License-Identifier: MPL-2.0
//! Device thermal state: Android's `PowerManager` thermal status and headroom, polled every 2
//! seconds (`getThermalHeadroom` is documented to be called at most once a second) so hosts and
//! the framework's quality governor can react before the OS throttles the device. A status that
//! changes and changes back inside that gap is not stored. Desktop reports [`ThermalStatus::None`]
//! with no headroom.

use std::time::{Duration, Instant};

use regia_app::prelude::*;

/// Android `PowerManager.THERMAL_STATUS_*` (API 29+), in the OS's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ThermalStatus {
    /// Not throttling (also what desktop reports).
    #[default]
    None,
    /// Light throttling; the user does not notice.
    Light,
    /// Moderate: performance is reduced.
    Moderate,
    /// Severe: heavy throttling, the frame rate suffers.
    Severe,
    /// Critical: the device may shut features down.
    Critical,
    /// Emergency: key components are being shut down.
    Emergency,
    /// Shutdown is imminent.
    Shutdown,
}

impl ThermalStatus {
    /// Maps the OS integer (`THERMAL_STATUS_NONE` = 0 … `THERMAL_STATUS_SHUTDOWN` = 6). Unknown
    /// values clamp to the nearest end.
    pub fn from_android(raw: i32) -> Self {
        match raw {
            i32::MIN..=0 => Self::None,
            1 => Self::Light,
            2 => Self::Moderate,
            3 => Self::Severe,
            4 => Self::Critical,
            5 => Self::Emergency,
            _ => Self::Shutdown,
        }
    }

    /// The agent-protocol token: `none`, `light`, `moderate`, `severe`, `critical`, `emergency`,
    /// `shutdown`.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Light => "light",
            Self::Moderate => "moderate",
            Self::Severe => "severe",
            Self::Critical => "critical",
            Self::Emergency => "emergency",
            Self::Shutdown => "shutdown",
        }
    }
}

/// The latest thermal reading. `headroom` is `PowerManager.getThermalHeadroom` (API 30+): a
/// forecast 10 s ahead where `1.0` means the severe-throttling threshold; `None` when the device
/// does not report it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct ThermalState {
    /// Current OS thermal status.
    pub status: ThermalStatus,
    /// Forecast headroom to the severe threshold, `0.0..` (above `1.0` = past it).
    pub headroom: Option<f32>,
}

/// A forced thermal reading (ADR-527): while `Some`, [`ThermalState`] takes this value every frame
/// instead of the OS reading, so the governor and game hooks can be exercised on a desktop or a
/// cool phone (`thermal.set_override`). Present by default with `None`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct ThermalOverride(pub Option<ThermalState>);

/// Seconds between polls: `getThermalHeadroom` is documented to be called at most once a second.
const POLL_SECONDS: f32 = 2.0;

/// Turns the raw headroom float into `Some` only when it is a usable forecast (the OS returns NaN
/// for "unsupported" and a negative value on some builds).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn sane_headroom(raw: f32) -> Option<f32> {
    (raw.is_finite() && raw >= 0.0).then_some(raw)
}

/// `PreUpdate`: poll the OS at `POLL_SECONDS` intervals and publish [`ThermalState`] when it
/// changes. Paused (not polled) while the app is suspended because frames are not running then.
/// A [`ThermalOverride`] wins over the OS at once.
pub(crate) fn poll_thermal(
    mut state: ResMut<ThermalState>,
    forced: Option<Res<ThermalOverride>>,
    mut last: Local<Option<Instant>>,
) {
    if let Some(forced) = forced.and_then(|f| f.0) {
        if *state != forced {
            tracing::info!(
                target: "regia_platform",
                "thermal {:?} headroom {:?} (override)",
                forced.status,
                forced.headroom
            );
            *state = forced;
        }
        // The next OS reading replaces the forced one as soon as the override is cleared.
        *last = None;
        return;
    }
    let now = Instant::now();
    if last.is_some_and(|t| now.duration_since(t) < Duration::from_secs_f32(POLL_SECONDS)) {
        return;
    }
    *last = Some(now);
    let Some(new) = crate::platform::thermal_state() else {
        return;
    };
    if *state != new {
        tracing::info!(target: "regia_platform", "thermal {:?} headroom {:?}", new.status, new.headroom);
        *state = new;
    }
}

/// The Android activity's own callbacks, which winit does not forward: `onPause` (audio and
/// other background work should stop now, not when the window is torn down a moment later) and
/// `onDestroy` (the app is going away: the platform plugin requests the clean exit). Always
/// default on desktop, where window events carry both.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActivityState {
    /// `Activity.onPause` ran and `onResume` has not run since.
    pub paused: bool,
    /// `Activity.onDestroy` ran.
    pub destroyed: bool,
}

/// `PreUpdate`: reads [`ActivityState`] about ten times a second; the first `destroyed` writes
/// [`AppExit`] so the runner runs the clean exit (ADR-188) and the final `summary.json` is written
/// into app storage instead of only the partial.
pub(crate) fn poll_activity(
    mut state: ResMut<ActivityState>,
    mut exit: MessageWriter<AppExit>,
    mut last: Local<Option<Instant>>,
) {
    let now = Instant::now();
    if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(100)) {
        return;
    }
    *last = Some(now);
    let Some((paused, destroyed)) = crate::platform::activity_state() else {
        return;
    };
    let new = ActivityState { paused, destroyed };
    if *state == new {
        return;
    }
    tracing::info!(target: "regia_platform", "regia activity: paused {paused} destroyed {destroyed}");
    if destroyed && !state.destroyed {
        tracing::info!(target: "regia_platform", "regia exit: clean exit requested (activity destroyed)");
        exit.write(AppExit::Success);
    }
    *state = new;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_android_constants() {
        assert_eq!(ThermalStatus::from_android(0), ThermalStatus::None);
        assert_eq!(ThermalStatus::from_android(3), ThermalStatus::Severe);
        assert_eq!(ThermalStatus::from_android(6), ThermalStatus::Shutdown);
        assert_eq!(ThermalStatus::from_android(99), ThermalStatus::Shutdown);
        assert_eq!(ThermalStatus::from_android(-1), ThermalStatus::None);
        assert!(ThermalStatus::Severe > ThermalStatus::Moderate);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn an_override_wins_at_once_and_clearing_it_returns_to_the_os() {
        let mut app = App::new();
        app.init_resource::<ThermalState>()
            .init_resource::<ThermalOverride>()
            .add_systems(Update, poll_thermal);
        let hot = ThermalState {
            status: ThermalStatus::Severe,
            headroom: Some(0.95),
        };
        app.world_mut().resource_mut::<ThermalOverride>().0 = Some(hot);
        app.update();
        assert_eq!(*app.world().resource::<ThermalState>(), hot);
        app.world_mut().resource_mut::<ThermalOverride>().0 = None;
        app.update();
        // Desktop has no OS reading: the last value stays until something else writes one.
        assert_eq!(*app.world().resource::<ThermalState>(), hot);
        *app.world_mut().resource_mut::<ThermalState>() = ThermalState::default();
        app.update();
        assert_eq!(
            *app.world().resource::<ThermalState>(),
            ThermalState::default(),
            "a cleared override no longer writes"
        );
    }

    #[test]
    fn headroom_rejects_nan_and_negative() {
        assert_eq!(sane_headroom(f32::NAN), None);
        assert_eq!(sane_headroom(-1.0), None);
        assert_eq!(sane_headroom(0.8), Some(0.8));
    }
}
