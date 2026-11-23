// SPDX-License-Identifier: MPL-2.0
//! The app-facing XR state (spec 58 § 2.2): [`XrAppState`], derived from whether an instance
//! exists and the session state, and the [`XrState`] resource `xr.get` reports.

use regia_app::prelude::*;
use regia_core::glam::UVec2;
use regia_platform::xr::SessionState;

use crate::config::{Foveation, Mirror, XrBackendKind};
use crate::hands::HandTrackingSupport;
use crate::space::{PlayArea, TrackingSpace};
use crate::stereo::StereoMode;

/// Regia's coarse app state over the OpenXR session states.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum XrAppState {
    /// XR disabled or no runtime: the desktop game runs normally.
    #[default]
    Off,
    /// An instance exists (or is being retried) but no session: waiting for a headset.
    NoSession,
    /// A session exists; waiting for the runtime to be ready.
    Idle,
    /// The runtime is ready; the session is being begun.
    Starting,
    /// Frames flow (synchronized or visible); input is live (`Running` and `Focused` both count).
    Running,
    /// Frames are shown and input is live.
    Focused,
    /// The session is ending.
    Stopping,
    /// The runtime or device is going away; the session is torn down and re-created later.
    LossPending,
    /// The user asked to leave; the app exits.
    Exiting,
}

impl XrAppState {
    /// The name `xr.get` reports (`"Focused"`, as `wait_until` compares it).
    pub fn name(self) -> &'static str {
        match self {
            XrAppState::Off => "Off",
            XrAppState::NoSession => "NoSession",
            XrAppState::Idle => "Idle",
            XrAppState::Starting => "Starting",
            XrAppState::Running => "Running",
            XrAppState::Focused => "Focused",
            XrAppState::Stopping => "Stopping",
            XrAppState::LossPending => "LossPending",
            XrAppState::Exiting => "Exiting",
        }
    }

    /// Whether eye views are being rendered or could be captured (running or focused).
    pub fn is_running(self) -> bool {
        matches!(self, XrAppState::Running | XrAppState::Focused)
    }
}

/// The app state for a session state, given whether an instance (and so a session) exists and
/// whether XR is enabled at all.
pub fn app_state_for(enabled: bool, has_session: bool, session: SessionState) -> XrAppState {
    if !enabled {
        return XrAppState::Off;
    }
    if !has_session {
        return XrAppState::NoSession;
    }
    match session {
        SessionState::Idle => XrAppState::Idle,
        SessionState::Ready => XrAppState::Starting,
        SessionState::Running | SessionState::Synchronized | SessionState::Visible => XrAppState::Running,
        SessionState::Focused => XrAppState::Focused,
        SessionState::Stopping => XrAppState::Stopping,
        SessionState::LossPending => XrAppState::LossPending,
        SessionState::Exiting => XrAppState::Exiting,
    }
}

/// Counters `xr.get` reports under `stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XrStats {
    /// XR frames run.
    pub frames: u64,
    /// Extractions that found no new frame in the mailbox (the old frame was repeated).
    pub stale_frames: u64,
    /// Poses rejected for a non-finite value (the previous pose was kept).
    pub bad_poses: u64,
    /// Frames that failed after `begin_frame` and were ended without layers.
    pub dropped_frames: u64,
    /// `xr_over_budget` diagnostics raised (spec 58 § 10; see [`crate::budget`]).
    pub over_budget: u64,
}

/// What the renderer needs to pick the XR pass set (spec 58 § 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct XrRenderProfile {
    /// Views per frame (2: stereo).
    pub view_count: u32,
    /// The stereo strategy asked for (`xr.set_option stereo_mode`).
    pub stereo_mode: StereoMode,
    /// Whether one multiview pass renders both eyes (else two passes): [`Self::stereo_mode`]
    /// resolved against the device.
    pub multiview: bool,
    /// The runtime's recommended per-eye size.
    pub recommended_extent: UVec2,
    /// Per-eye target size (recommended size × render scale).
    pub extent: UVec2,
    /// Whether eye-dependent effects keep per-eye history (TAA); false on the Android reduced set.
    pub per_eye_history: bool,
    /// Whether the runtime's visibility mask hides pixels no lens shows, when it offers one.
    pub hidden_area: bool,
    /// Whether each eye's depth goes to the runtime for positional reprojection, when it takes it.
    pub depth_layer: bool,
}

impl Default for XrRenderProfile {
    fn default() -> Self {
        XrRenderProfile {
            view_count: 2,
            stereo_mode: StereoMode::TwoPass,
            multiview: false,
            recommended_extent: UVec2::new(1440, 1584),
            extent: UVec2::new(1440, 1584),
            per_eye_history: !cfg!(target_os = "android"),
            hidden_area: true,
            depth_layer: true,
        }
    }
}

/// The XR status: app state, session state, what the runtime offered and the counters.
#[derive(Debug, Clone, PartialEq, Resource)]
pub struct XrState {
    /// The app state.
    pub app: XrAppState,
    /// The session state.
    pub session: SessionState,
    /// The configured backend.
    pub backend_kind: XrBackendKind,
    /// The backend in use (`"null"` for the simulator, `"none"` while off).
    pub backend: String,
    /// The runtime's name and version, when one is open.
    pub runtime: Option<String>,
    /// The system (headset) name, when one is open.
    pub system: Option<String>,
    /// The requested tracking space.
    pub tracking_space: TrackingSpace,
    /// The space actually in use after fallback.
    pub actual_tracking_space: TrackingSpace,
    /// Display refresh rate, Hz.
    pub refresh_hz: f32,
    /// Render scale.
    pub render_scale: f32,
    /// Foveation level applied.
    pub foveation: Foveation,
    /// Mirror mode.
    pub mirror: Mirror,
    /// Extensions enabled on the instance.
    pub extensions_enabled: Vec<String>,
    /// Hand tracking availability.
    pub hand_tracking: HandTrackingSupport,
    /// Stage bounds, when reported.
    pub play_area: Option<PlayArea>,
    /// Counters.
    pub stats: XrStats,
    /// The last error code (`xr_*`) and message, if any.
    pub last_error: Option<(String, String)>,
    /// Fixed steps until the next attempt to reach a headset or re-create a lost session.
    pub retry_in: Option<u32>,
    /// Sessions created (the first and every re-creation).
    pub sessions_created: u64,
}

impl Default for XrState {
    fn default() -> Self {
        XrState {
            app: XrAppState::Off,
            session: SessionState::Idle,
            backend_kind: XrBackendKind::Auto,
            backend: "none".into(),
            runtime: None,
            system: None,
            tracking_space: TrackingSpace::Stage,
            actual_tracking_space: TrackingSpace::Local,
            refresh_hz: 0.0,
            render_scale: 1.0,
            foveation: Foveation::Off,
            mirror: Mirror::Left,
            extensions_enabled: Vec::new(),
            hand_tracking: HandTrackingSupport::Unsupported,
            play_area: None,
            stats: XrStats::default(),
            last_error: None,
            retry_in: None,
            sessions_created: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_state_maps_to_the_table_row() {
        use SessionState as S;
        use XrAppState as A;
        let rows = [
            (S::Idle, A::Idle),
            (S::Ready, A::Starting),
            (S::Running, A::Running),
            (S::Synchronized, A::Running),
            (S::Visible, A::Running),
            (S::Focused, A::Focused),
            (S::Stopping, A::Stopping),
            (S::LossPending, A::LossPending),
            (S::Exiting, A::Exiting),
        ];
        for (session, app) in rows {
            assert_eq!(app_state_for(true, true, session), app, "{session:?}");
            assert_eq!(app_state_for(true, false, session), A::NoSession);
            assert_eq!(app_state_for(false, true, session), A::Off);
        }
    }
}
