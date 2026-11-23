// SPDX-License-Identifier: MPL-2.0
//! The OpenXR seam: a backend-agnostic head-mounted-display interface (ADR-1571).
//!
//! No SDK is linked. [`XrBackend`] is the trait a real runtime binding implements later; this
//! module owns everything that does not depend on one:
//!
//! - [`SessionCore`]: the session state machine (idle → ready → running → stopping → idle) and the
//!   frame-loop ordering (wait, begin, end) as pure, testable logic.
//! - [`Pose`], [`Fov`], [`View`]: tracked poses and the per-eye asymmetric field of view, with the
//!   projection math ([`Fov::projection`]) for standard and reversed-Z depth, finite and infinite
//!   far plane, matching the renderer's right-handed, zero-to-one-depth convention.
//! - [`PoseSample`] and [`LateLatch`]: constant-velocity pose prediction and the late-latch step that
//!   re-predicts the head just before submit and reports the per-eye correction.
//! - [`ControllerState`], [`ControllerAction`]: controller poses and action values.
//! - [`NullXr`]: a scripted, deterministic backend for tests, CI and headless runs.
//! - [`run_frame`] and [`XrRuntime`]: one frame through any backend, and the ECS resource the agent
//!   protocol (`xr.get`, `xr.drive`) reads.
//!
//! Conventions follow the public OpenXR specification's description of spaces and field of view:
//! right-handed, +Y up, the view looks down −Z, field-of-view angles are signed (`left` and `down`
//! are negative for a symmetric view), times are nanoseconds on one monotonic clock.

use std::f32::consts::FRAC_PI_2;

use regia_app::prelude::*;
use regia_core::glam::{Mat4, Quat, Vec2, Vec3, Vec4};

mod layers;

pub use layers::{
    DepthInfo, LayerList, LayerSpace, MAX_QUAD_LAYERS, ProjectionLayer, ProjectionView, QuadLayer, SubImage,
    VisibilityMask, order_quads, split_quads,
};

/// Longest a pose is extrapolated by default (`LateLatch::default`): beyond this a prediction is
/// clamped and flagged, because constant-velocity extrapolation diverges quickly.
pub const DEFAULT_MAX_EXTRAPOLATION_NS: i64 = 50_000_000;

/// Interpupillary distance a [`NullConfig`] defaults to, in meters.
pub const DEFAULT_IPD_M: f32 = 0.063;

/// Smallest and largest interpupillary distance [`stereo_views`] accepts, in meters.
pub const IPD_RANGE_M: (f32, f32) = (0.02, 0.1);

/// Events a [`NullXr`] and [`XrRuntime`] keep for inspection.
pub const MAX_KEPT_EVENTS: usize = 32;

/// Frame submits a [`NullXr`] keeps for [`NullXr::submitted`]. Older submits are dropped.
/// [`SessionCore::frames_ended`] still counts every ended frame.
pub const MAX_KEPT_SUBMITS: usize = 32;

/// Haptic pulses a [`NullXr`] keeps for [`NullXr::haptics`]. Older pulses are dropped.
pub const MAX_KEPT_HAPTICS: usize = 32;

/// Why an XR call failed. [`XrError::code`] is the stable lowercase code the agent protocol uses.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum XrError {
    /// A field of view is not usable (non-finite, inverted or at or past ±90 degrees).
    #[error("invalid field of view: {reason}")]
    InvalidFov {
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A depth range is not usable (`near` must be finite and positive, `far` beyond it or infinite).
    #[error("invalid depth range: near {near}, far {far}")]
    InvalidDepthRange {
        /// The near plane distance given.
        near: f32,
        /// The far plane distance given.
        far: f32,
    },
    /// An interpupillary distance outside [`IPD_RANGE_M`], or not finite.
    #[error("invalid interpupillary distance {ipd}")]
    InvalidIpd {
        /// The distance given, in meters.
        ipd: f32,
    },
    /// A session transition that the state machine does not allow from the current state.
    #[error("transition {transition:?} is not allowed while {from:?}")]
    InvalidTransition {
        /// The state the session was in.
        from: SessionState,
        /// The transition attempted.
        transition: Transition,
    },
    /// A call that needs a running session.
    #[error("{call} needs a running session (state {state:?})")]
    NotRunning {
        /// The call attempted.
        call: &'static str,
        /// The state the session was in.
        state: SessionState,
    },
    /// A frame-loop call out of order (begin before wait, end before begin, wait during a frame).
    #[error("{call} called out of order (frame phase {phase:?})")]
    FrameOutOfOrder {
        /// The call attempted.
        call: &'static str,
        /// The frame phase at the time.
        phase: FramePhase,
    },
    /// `end_frame` was given a display time other than the one `wait_frame` returned.
    #[error("end_frame display time {got} does not match the waited frame {expected}")]
    DisplayTimeMismatch {
        /// The display time `wait_frame` predicted.
        expected: i64,
        /// The display time the submit carried.
        got: i64,
    },
    /// A haptic amplitude outside zero to one, or not finite.
    #[error("invalid haptic amplitude {amplitude}")]
    InvalidHaptic {
        /// The amplitude given.
        amplitude: f32,
    },
    /// The backend does not implement this call.
    #[error("{0} is not supported by this backend")]
    Unsupported(&'static str),
    /// The OpenXR loader library is not installed (spec 58 § 2.3 step 1).
    #[error("the OpenXR loader is not installed")]
    NoLoader,
    /// The loader found no active OpenXR runtime.
    #[error("no OpenXR runtime is active")]
    NoRuntime,
    /// A required instance extension is not advertised; `name` says which.
    #[error("required OpenXR extension {name} is not available")]
    ExtensionMissing {
        /// The extension's name.
        name: String,
    },
    /// Instance creation failed for another reason.
    #[error("OpenXR instance creation failed: {reason}")]
    InstanceFailed {
        /// What the runtime reported.
        reason: String,
    },
    /// No head-mounted display system is available now (not an error state: retried).
    #[error("no head-mounted display system is available")]
    SystemUnavailable,
    /// The primary stereo view configuration reported `views` views instead of two.
    #[error("the primary stereo view configuration has {views} views, not 2")]
    ViewConfigUnsupported {
        /// The view count the runtime reported.
        views: u32,
    },
    /// The renderer's GPU device could not be shared with the runtime (spec 58 § 3.4).
    #[error("GPU interop with the XR runtime is unavailable: {reason}")]
    GpuInteropUnavailable {
        /// Why.
        reason: String,
    },
    /// A second session was requested while one exists (one per process).
    #[error("an XR session already exists")]
    SessionExists,
    /// The runtime offers no swapchain format the renderer supports.
    #[error("no supported swapchain format")]
    NoSwapchainFormat,
    /// Action sets are attached: no action may be created and they may not be attached again.
    #[error("action sets are attached; {call} is not allowed")]
    ActionsFrozen {
        /// The call attempted.
        call: &'static str,
    },
    /// An action name (or sub-path) that the manifest does not declare.
    #[error("unknown XR action {name}")]
    ActionUnknown {
        /// The name given.
        name: String,
    },
    /// A call only a scripted (simulator) backend supports.
    #[error("{call} needs the simulator backend")]
    SimulatorOnly {
        /// The call attempted.
        call: &'static str,
    },
    /// An unknown `xr.set_option` key.
    #[error("unknown XR option {key}")]
    UnknownOption {
        /// The key given.
        key: String,
    },
    /// A value of the wrong type or out of range; `reason` gives the range.
    #[error("invalid value for {key}: {reason}")]
    InvalidValue {
        /// The option or field.
        key: String,
        /// The accepted type or range.
        reason: String,
    },
    /// A file whose `format_version` is newer than this build reads.
    #[error("{what} format_version {found} is newer than {supported}")]
    FormatTooNew {
        /// Which file kind.
        what: &'static str,
        /// The version in the file.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// The runtime or device went away (loss pending); the session was destroyed.
    #[error("the XR runtime was lost")]
    RuntimeLost,
}

impl XrError {
    /// The stable lowercase code (`xr_*`) the agent protocol reports.
    pub fn code(&self) -> &'static str {
        match self {
            XrError::InvalidFov { .. } => "xr_invalid_fov",
            XrError::InvalidDepthRange { .. } => "xr_invalid_depth_range",
            XrError::InvalidIpd { .. } => "xr_invalid_ipd",
            XrError::InvalidTransition { .. } => "xr_invalid_transition",
            XrError::NotRunning { .. } => "xr_not_running",
            XrError::FrameOutOfOrder { .. } => "xr_frame_out_of_order",
            XrError::DisplayTimeMismatch { .. } => "xr_display_time_mismatch",
            XrError::InvalidHaptic { .. } => "xr_invalid_haptic",
            XrError::Unsupported(_) => "xr_unsupported",
            XrError::NoLoader => "xr_no_loader",
            XrError::NoRuntime => "xr_no_runtime",
            XrError::ExtensionMissing { .. } => "xr_extension_missing",
            XrError::InstanceFailed { .. } => "xr_instance_failed",
            XrError::SystemUnavailable => "xr_system_unavailable",
            XrError::ViewConfigUnsupported { .. } => "xr_view_config_unsupported",
            XrError::GpuInteropUnavailable { .. } => "xr_gpu_interop_unavailable",
            XrError::SessionExists => "xr_session_exists",
            XrError::NoSwapchainFormat => "xr_no_swapchain_format",
            XrError::ActionsFrozen { .. } => "xr_actions_frozen",
            XrError::ActionUnknown { .. } => "xr_action_unknown",
            XrError::SimulatorOnly { .. } => "xr_simulator_only",
            XrError::UnknownOption { .. } => "xr_unknown_option",
            XrError::InvalidValue { .. } => "xr_invalid_value",
            XrError::FormatTooNew { .. } => "xr_format_too_new",
            XrError::RuntimeLost => "xr_runtime_lost",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Session state machine
// ---------------------------------------------------------------------------------------------

/// Where an XR session is in its life. The first four are the coarse states every backend reports;
/// `Synchronized`, `Visible`, `Focused`, `LossPending` and `Exiting` are the OpenXR session states a
/// real runtime reports after the session begins (spec 58 § 2.2). `Running` is a begun session that
/// has not reported synchronization yet (or a backend that does not report the finer states).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// No session activity; the runtime has not asked for one.
    #[default]
    Idle,
    /// The runtime is ready for the app to begin the session.
    Ready,
    /// The session is running; frames flow.
    Running,
    /// The session is ending (the app asked, or the runtime did); finish the frame, then end it.
    Stopping,
    /// Frames are synchronized with the display but not shown: submit zero layers, render nothing.
    Synchronized,
    /// Frames are shown; input is not focused on the app.
    Visible,
    /// Frames are shown and input is live.
    Focused,
    /// The runtime or device is going away: end and destroy the session, re-create later.
    LossPending,
    /// The user asked to leave the app: exit normally.
    Exiting,
}

impl SessionState {
    /// Every state, in declaration order.
    pub const ALL: [SessionState; 9] = [
        SessionState::Idle,
        SessionState::Ready,
        SessionState::Running,
        SessionState::Stopping,
        SessionState::Synchronized,
        SessionState::Visible,
        SessionState::Focused,
        SessionState::LossPending,
        SessionState::Exiting,
    ];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Ready => "ready",
            SessionState::Running => "running",
            SessionState::Stopping => "stopping",
            SessionState::Synchronized => "synchronized",
            SessionState::Visible => "visible",
            SessionState::Focused => "focused",
            SessionState::LossPending => "loss_pending",
            SessionState::Exiting => "exiting",
        }
    }

    /// The state named `name` (the inverse of [`SessionState::name`]).
    pub fn from_name(name: &str) -> Option<SessionState> {
        SessionState::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Whether frames flow (wait, begin, end): running, synchronized, visible or focused.
    pub fn frames_flow(self) -> bool {
        matches!(
            self,
            SessionState::Running
                | SessionState::Synchronized
                | SessionState::Visible
                | SessionState::Focused
        )
    }

    /// Whether the runtime shows the app's frames: running (a backend without the finer states),
    /// visible or focused. A synchronized session renders nothing.
    pub fn shows_frames(self) -> bool {
        matches!(
            self,
            SessionState::Running | SessionState::Visible | SessionState::Focused
        )
    }

    /// Whether input is live: focused, or running on a backend that does not report focus.
    pub fn input_live(self) -> bool {
        matches!(self, SessionState::Running | SessionState::Focused)
    }
}

/// A request to move a session between states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    /// The runtime became ready (idle → ready).
    RuntimeReady,
    /// The app begins the session (ready → running).
    Begin,
    /// The app asks to stop (running, synchronized, visible or focused → stopping).
    RequestExit,
    /// The runtime ends the session on its own, a headset removed or a runtime lost (ready → idle;
    /// running, synchronized, visible or focused → stopping).
    RuntimeStop,
    /// The app ends the session after the last frame (stopping → idle), or destroys a lost
    /// session (loss pending → idle).
    End,
    /// The runtime synchronized the frame loop with the display (running → synchronized, or
    /// visible → synchronized when the frames are hidden again).
    RuntimeSynchronized,
    /// The runtime shows the app's frames (synchronized → visible, or focused → visible when input
    /// focus leaves the app).
    RuntimeVisible,
    /// The runtime gave input focus to the app (visible → focused).
    RuntimeFocused,
    /// The runtime or device is going away (any state but exiting and loss pending → loss pending).
    RuntimeLost,
    /// The user asked to leave the app (idle → exiting).
    RuntimeExiting,
}

impl Transition {
    /// Every transition, in declaration order.
    pub const ALL: [Transition; 10] = [
        Transition::RuntimeReady,
        Transition::Begin,
        Transition::RequestExit,
        Transition::RuntimeStop,
        Transition::End,
        Transition::RuntimeSynchronized,
        Transition::RuntimeVisible,
        Transition::RuntimeFocused,
        Transition::RuntimeLost,
        Transition::RuntimeExiting,
    ];
}

/// Which part of a frame the loop is in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FramePhase {
    /// Between frames.
    #[default]
    Idle,
    /// `wait_frame` returned; `begin_frame` is next.
    Waited,
    /// `begin_frame` returned; `end_frame` is next.
    Begun,
}

/// Timing the runtime predicts for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    /// Frames waited since the session began, starting at zero.
    pub frame_index: u64,
    /// When the frame will be displayed, in nanoseconds on the runtime's clock.
    pub predicted_display_time_ns: i64,
    /// Time between two displayed frames, in nanoseconds.
    pub predicted_display_period_ns: i64,
    /// False while the headset is not visible to the user: skip rendering, still submit.
    pub should_render: bool,
}

/// The session state and the frame-loop ordering, shared by every backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCore {
    state: SessionState,
    phase: FramePhase,
    waited: Option<FrameTiming>,
    frames_ended: u64,
}

impl SessionCore {
    /// A session in [`SessionState::Idle`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// The current frame phase.
    pub fn phase(&self) -> FramePhase {
        self.phase
    }

    /// Frames ended since this core was created.
    pub fn frames_ended(&self) -> u64 {
        self.frames_ended
    }

    /// The timing of the frame that was waited and not yet ended.
    pub fn pending_frame(&self) -> Option<FrameTiming> {
        self.waited
    }

    /// Applies `transition`. Returns the new state, or [`XrError::InvalidTransition`] (the state is
    /// unchanged). Entering [`SessionState::Idle`] drops any frame in flight.
    pub fn transition(&mut self, transition: Transition) -> Result<SessionState, XrError> {
        use SessionState::{
            Exiting, Focused, Idle, LossPending, Ready, Running, Stopping, Synchronized, Visible,
        };
        use Transition::{
            Begin, End, RequestExit, RuntimeExiting, RuntimeFocused, RuntimeLost, RuntimeReady, RuntimeStop,
            RuntimeSynchronized, RuntimeVisible,
        };
        let next = match (self.state, transition) {
            (Idle, RuntimeReady) => Ready,
            (Ready, Begin) => Running,
            (Running | Synchronized | Visible | Focused, RequestExit | RuntimeStop) => Stopping,
            (Ready, RuntimeStop) => Idle,
            (Stopping | LossPending, End) => Idle,
            (Running | Visible, RuntimeSynchronized) => Synchronized,
            (Synchronized | Focused, RuntimeVisible) => Visible,
            (Visible, RuntimeFocused) => Focused,
            (Idle | Ready | Running | Stopping | Synchronized | Visible | Focused, RuntimeLost) => {
                LossPending
            }
            (Idle, RuntimeExiting) => Exiting,
            (from, transition) => return Err(XrError::InvalidTransition { from, transition }),
        };
        if next == LossPending {
            // The session is going away: nothing in flight survives.
            self.phase = FramePhase::Idle;
            self.waited = None;
        }
        if next == Idle {
            self.phase = FramePhase::Idle;
            self.waited = None;
        }
        tracing::debug!(from = self.state.name(), to = next.name(), "xr session state");
        self.state = next;
        Ok(next)
    }

    /// Records `wait_frame`: frames must flow ([`SessionState::frames_flow`]) and no frame may be
    /// begun. A second wait before `begin_frame` replaces the first (a dropped frame).
    pub fn frame_waited(&mut self, timing: FrameTiming) -> Result<(), XrError> {
        if !self.state.frames_flow() {
            return Err(XrError::NotRunning {
                call: "wait_frame",
                state: self.state,
            });
        }
        if self.phase == FramePhase::Begun {
            return Err(XrError::FrameOutOfOrder {
                call: "wait_frame",
                phase: self.phase,
            });
        }
        self.phase = FramePhase::Waited;
        self.waited = Some(timing);
        Ok(())
    }

    /// Records `begin_frame`: a frame must have been waited.
    pub fn frame_begun(&mut self) -> Result<(), XrError> {
        if !self.state.frames_flow() {
            return Err(XrError::NotRunning {
                call: "begin_frame",
                state: self.state,
            });
        }
        if self.phase != FramePhase::Waited {
            return Err(XrError::FrameOutOfOrder {
                call: "begin_frame",
                phase: self.phase,
            });
        }
        self.phase = FramePhase::Begun;
        Ok(())
    }

    /// Records `end_frame` for `display_time_ns`: a frame must have been begun and the time must be
    /// the waited one. Allowed while stopping, so the last frame can be flushed.
    pub fn frame_ended(&mut self, display_time_ns: i64) -> Result<(), XrError> {
        if !(self.state.frames_flow() || self.state == SessionState::Stopping) {
            return Err(XrError::NotRunning {
                call: "end_frame",
                state: self.state,
            });
        }
        if self.phase != FramePhase::Begun {
            return Err(XrError::FrameOutOfOrder {
                call: "end_frame",
                phase: self.phase,
            });
        }
        let expected = self
            .waited
            .map_or(display_time_ns, |t| t.predicted_display_time_ns);
        if expected != display_time_ns {
            return Err(XrError::DisplayTimeMismatch {
                expected,
                got: display_time_ns,
            });
        }
        self.phase = FramePhase::Idle;
        self.waited = None;
        self.frames_ended += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Poses, field of view, projection
// ---------------------------------------------------------------------------------------------

/// A rigid transform: a position in meters and a unit orientation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Position in the parent space, meters.
    pub position: Vec3,
    /// Orientation in the parent space (unit quaternion).
    pub orientation: Quat,
}

impl Pose {
    /// The identity pose.
    pub const IDENTITY: Pose = Pose {
        position: Vec3::ZERO,
        orientation: Quat::IDENTITY,
    };

    /// A pose from a position and an orientation. A zero or non-finite orientation becomes the
    /// identity; any other is normalized.
    pub fn new(position: Vec3, orientation: Quat) -> Self {
        let orientation = if orientation.is_finite() && orientation.length_squared() > 1e-12 {
            orientation.normalize()
        } else {
            Quat::IDENTITY
        };
        // View matrices are built from this: a bad sample stands at the origin rather than NaN.
        let position = if position.is_finite() {
            position
        } else {
            Vec3::ZERO
        };
        Pose {
            position,
            orientation,
        }
    }

    /// `self` applied after `other`: `other`'s frame expressed in `self`'s parent.
    pub fn compose(&self, other: &Pose) -> Pose {
        Pose::new(
            self.position + self.orientation * other.position,
            self.orientation * other.orientation,
        )
    }

    /// The pose that undoes `self`.
    pub fn inverse(&self) -> Pose {
        let inv = self.orientation.conjugate();
        Pose {
            position: inv * -self.position,
            orientation: inv,
        }
    }

    /// The model matrix (pose space to parent space).
    pub fn to_matrix(&self) -> Mat4 {
        Mat4::from_rotation_translation(self.orientation, self.position)
    }

    /// The view matrix (parent space to pose space) for a camera at this pose.
    pub fn view_matrix(&self) -> Mat4 {
        self.inverse().to_matrix()
    }
}

impl Default for Pose {
    fn default() -> Self {
        Pose::IDENTITY
    }
}

/// How depth is stored: `Reversed` puts the near plane at 1 and the far plane at 0 (the renderer's
/// convention, better precision with a float depth buffer); `Standard` puts near at 0 and far at 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DepthMode {
    /// Near plane at depth 1, far plane at 0.
    #[default]
    Reversed,
    /// Near plane at depth 0, far plane at 1.
    Standard,
}

/// Near and far plane distances, in meters. `far` may be `f32::INFINITY`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthRange {
    /// Distance to the near plane; finite and greater than zero.
    pub near: f32,
    /// Distance to the far plane; beyond `near`, or `f32::INFINITY`.
    pub far: f32,
}

impl DepthRange {
    /// Checks the range.
    pub fn validate(&self) -> Result<(), XrError> {
        let ok = self.near.is_finite() && self.near > 0.0 && self.far > self.near && !self.far.is_nan();
        if ok {
            Ok(())
        } else {
            Err(XrError::InvalidDepthRange {
                near: self.near,
                far: self.far,
            })
        }
    }
}

/// A per-eye field of view as four signed half-angles in radians, the form the OpenXR
/// specification uses: `left` and `down` are negative for a symmetric view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fov {
    /// Angle to the left edge (negative when the edge is left of the view axis).
    pub left: f32,
    /// Angle to the right edge.
    pub right: f32,
    /// Angle to the top edge.
    pub up: f32,
    /// Angle to the bottom edge (negative when below the view axis).
    pub down: f32,
}

impl Fov {
    /// A symmetric field of view with the given half-angles (radians, positive).
    pub fn symmetric(half_horizontal: f32, half_vertical: f32) -> Self {
        Fov {
            left: -half_horizontal,
            right: half_horizontal,
            up: half_vertical,
            down: -half_vertical,
        }
    }

    /// Checks the field of view: finite, `left < right`, `down < up`, each angle within ±90 degrees
    /// (exclusive).
    pub fn validate(&self) -> Result<(), XrError> {
        let angles = [self.left, self.right, self.up, self.down];
        if angles.iter().any(|a| !a.is_finite()) {
            return Err(XrError::InvalidFov {
                reason: "angle is not finite",
            });
        }
        if angles.iter().any(|a| a.abs() >= FRAC_PI_2) {
            return Err(XrError::InvalidFov {
                reason: "angle is at or beyond 90 degrees",
            });
        }
        if self.left >= self.right {
            return Err(XrError::InvalidFov {
                reason: "left is not less than right",
            });
        }
        if self.down >= self.up {
            return Err(XrError::InvalidFov {
                reason: "down is not less than up",
            });
        }
        Ok(())
    }

    /// The tangents `(left, right, up, down)` of the four angles.
    pub fn tangents(&self) -> [f32; 4] {
        [self.left.tan(), self.right.tan(), self.up.tan(), self.down.tan()]
    }

    /// The vertical field of view in radians (`up - down`).
    pub fn vertical(&self) -> f32 {
        self.up - self.down
    }

    /// The horizontal field of view in radians (`right - left`).
    pub fn horizontal(&self) -> f32 {
        self.right - self.left
    }

    /// The asymmetric perspective projection matrix: right-handed view space looking down −Z, clip
    /// space with +Y up and depth in zero to one (the wgpu convention).
    ///
    /// Points on the left, right, top and bottom frustum planes map to NDC x of −1, 1 and y of 1,
    /// −1; the near plane maps to depth 1 ([`DepthMode::Reversed`]) or 0 ([`DepthMode::Standard`])
    /// and the far plane to the other. With `far == f32::INFINITY` the far plane is at infinity
    /// (depth 0 reversed, 1 standard, approached from the near side).
    pub fn projection(&self, range: DepthRange, mode: DepthMode) -> Result<Mat4, XrError> {
        self.validate()?;
        range.validate()?;
        let [tl, tr, tu, td] = self.tangents();
        let width = tr - tl;
        let height = tu - td;
        let (near, far) = (range.near, range.far);
        // Depth row: ndc_z = (a * z_eye + b) / -z_eye.
        let (a, b) = match (mode, far.is_infinite()) {
            (DepthMode::Reversed, true) => (0.0, near),
            (DepthMode::Reversed, false) => (near / (far - near), near * far / (far - near)),
            (DepthMode::Standard, true) => (-1.0, -near),
            (DepthMode::Standard, false) => (-far / (far - near), -near * far / (far - near)),
        };
        Ok(Mat4::from_cols(
            Vec4::new(2.0 / width, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 / height, 0.0, 0.0),
            Vec4::new((tr + tl) / width, (tu + td) / height, a, -1.0),
            Vec4::new(0.0, 0.0, b, 0.0),
        ))
    }
}

/// Which eye.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eye {
    /// The left eye (index 0).
    Left,
    /// The right eye (index 1).
    Right,
}

impl Eye {
    /// Both eyes in index order.
    pub const BOTH: [Eye; 2] = [Eye::Left, Eye::Right];

    /// The eye's index into a `[T; 2]`.
    pub fn index(self) -> usize {
        match self {
            Eye::Left => 0,
            Eye::Right => 1,
        }
    }
}

/// One eye's view: where it is and what it sees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Which eye.
    pub eye: Eye,
    /// The eye's pose in the tracking space.
    pub pose: Pose,
    /// The eye's field of view.
    pub fov: Fov,
}

impl View {
    /// The view matrix for this eye.
    pub fn view_matrix(&self) -> Mat4 {
        self.pose.view_matrix()
    }

    /// The projection matrix for this eye (see [`Fov::projection`]).
    pub fn projection(&self, range: DepthRange, mode: DepthMode) -> Result<Mat4, XrError> {
        self.fov.projection(range, mode)
    }

    /// Projection times view.
    pub fn view_projection(&self, range: DepthRange, mode: DepthMode) -> Result<Mat4, XrError> {
        Ok(self.projection(range, mode)? * self.view_matrix())
    }
}

/// The two eye views for a head at `head` with the eyes `ipd_m` apart along the head's local X.
pub fn stereo_views(head: &Pose, ipd_m: f32, fov: Fov) -> Result<[View; 2], XrError> {
    if !ipd_m.is_finite() || ipd_m < IPD_RANGE_M.0 || ipd_m > IPD_RANGE_M.1 {
        return Err(XrError::InvalidIpd { ipd: ipd_m });
    }
    fov.validate()?;
    let half = ipd_m * 0.5;
    let eye = |eye: Eye, x: f32| View {
        eye,
        pose: head.compose(&Pose::new(Vec3::new(x, 0.0, 0.0), Quat::IDENTITY)),
        fov,
    };
    Ok([eye(Eye::Left, -half), eye(Eye::Right, half)])
}

// ---------------------------------------------------------------------------------------------
// Pose prediction and late latching
// ---------------------------------------------------------------------------------------------

/// A tracked pose with its velocities at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoseSample {
    /// The pose.
    pub pose: Pose,
    /// Linear velocity in the parent space, meters per second.
    pub linear_velocity: Vec3,
    /// Angular velocity in the parent space, radians per second (axis times rate).
    pub angular_velocity: Vec3,
    /// When the sample was taken, nanoseconds on the runtime's clock.
    pub time_ns: i64,
}

impl Default for PoseSample {
    fn default() -> Self {
        PoseSample::at_rest(Pose::IDENTITY, 0)
    }
}

/// The result of [`PoseSample::extrapolate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prediction {
    /// The predicted pose.
    pub pose: Pose,
    /// How far the pose was extrapolated, in nanoseconds (after clamping).
    pub extrapolated_ns: i64,
    /// True when the requested time was outside the allowed window and the step was clamped.
    pub clamped: bool,
}

impl PoseSample {
    /// A sample with zero velocity.
    pub fn at_rest(pose: Pose, time_ns: i64) -> Self {
        PoseSample {
            pose,
            linear_velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            time_ns,
        }
    }

    /// Predicts the pose at `target_ns` with constant linear and angular velocity. The step is
    /// clamped to `[0, max_ns]`: a target before the sample is not extrapolated backwards, and a
    /// far-future one stops at `max_ns`; both set `clamped`. A non-finite velocity counts as zero.
    pub fn extrapolate(&self, target_ns: i64, max_ns: i64) -> Prediction {
        let raw = target_ns.saturating_sub(self.time_ns);
        let step = raw.clamp(0, max_ns.max(0));
        let dt = step as f32 * 1e-9;
        let finite = |v: Vec3| if v.is_finite() { v } else { Vec3::ZERO };
        let v = finite(self.linear_velocity);
        let w = finite(self.angular_velocity);
        let angle = w.length() * dt;
        let orientation = if angle > 1e-9 {
            Quat::from_axis_angle(w.normalize(), angle) * self.pose.orientation
        } else {
            self.pose.orientation
        };
        Prediction {
            pose: Pose::new(self.pose.position + v * dt, orientation),
            extrapolated_ns: step,
            clamped: step != raw,
        }
    }
}

/// The per-eye result of [`LateLatch::latch_views`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatchedViews {
    /// The views re-predicted with the freshest head sample (submit these).
    pub views: [View; 2],
    /// Per eye, the world-space transform that takes the rendered eye pose to the latched one
    /// (`latched = correction.compose(rendered)`), for timewarp-style reprojection.
    pub corrections: [Pose; 2],
    /// True when the head prediction was clamped.
    pub clamped: bool,
}

/// Late latching: after the app rendered with a head pose predicted early, read the tracker again
/// just before submit, re-predict to the same display time, and move both eyes by the difference.
#[derive(Debug, Clone, PartialEq)]
pub struct LateLatch {
    /// Longest extrapolation allowed, nanoseconds.
    pub max_extrapolation_ns: i64,
    latched: u64,
    clamped: u64,
}

impl Default for LateLatch {
    fn default() -> Self {
        LateLatch::new(DEFAULT_MAX_EXTRAPOLATION_NS)
    }
}

impl LateLatch {
    /// A latch that extrapolates at most `max_extrapolation_ns`.
    pub fn new(max_extrapolation_ns: i64) -> Self {
        LateLatch {
            max_extrapolation_ns,
            latched: 0,
            clamped: 0,
        }
    }

    /// Frames latched so far.
    pub fn latched(&self) -> u64 {
        self.latched
    }

    /// Frames whose prediction was clamped.
    pub fn clamped(&self) -> u64 {
        self.clamped
    }

    /// Predicts `sample` to `display_time_ns` within this latch's extrapolation window.
    pub fn predict(&self, sample: &PoseSample, display_time_ns: i64) -> Prediction {
        sample.extrapolate(display_time_ns, self.max_extrapolation_ns)
    }

    /// Re-predicts the head from `fresh` for `display_time_ns` and moves `rendered` by the change.
    /// `rendered_head` is the head pose the views were built from; each eye keeps its offset from
    /// the head.
    pub fn latch_views(
        &mut self,
        rendered_head: &Pose,
        rendered: &[View; 2],
        fresh: &PoseSample,
        display_time_ns: i64,
    ) -> LatchedViews {
        let prediction = self.predict(fresh, display_time_ns);
        let head_inv = rendered_head.inverse();
        let mut views = *rendered;
        let mut corrections = [Pose::IDENTITY; 2];
        for (i, view) in rendered.iter().enumerate() {
            let offset = head_inv.compose(&view.pose);
            let latched = prediction.pose.compose(&offset);
            corrections[i] = latched.compose(&view.pose.inverse());
            views[i].pose = latched;
        }
        self.latched += 1;
        if prediction.clamped {
            self.clamped += 1;
        }
        LatchedViews {
            views,
            corrections,
            clamped: prediction.clamped,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Controllers and actions
// ---------------------------------------------------------------------------------------------

/// Which hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hand {
    /// The left hand (index 0).
    Left,
    /// The right hand (index 1).
    Right,
}

impl Hand {
    /// Both hands in index order.
    pub const BOTH: [Hand; 2] = [Hand::Left, Hand::Right];

    /// The hand's index into a `[T; 2]`.
    pub fn index(self) -> usize {
        match self {
            Hand::Left => 0,
            Hand::Right => 1,
        }
    }

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            Hand::Left => "left",
            Hand::Right => "right",
        }
    }
}

/// The controller inputs the seam exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControllerAction {
    /// Analog trigger, zero to one.
    Trigger,
    /// Analog grip, zero to one.
    Squeeze,
    /// Thumbstick, each axis −1 to 1.
    Thumbstick,
    /// The primary face button (A or X).
    PrimaryButton,
    /// The secondary face button (B or Y).
    SecondaryButton,
    /// Pressing the thumbstick in.
    ThumbstickClick,
    /// The menu button.
    Menu,
}

/// An action's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionValue {
    /// A button.
    Bool(bool),
    /// An analog axis.
    Float(f32),
    /// A two-axis stick.
    Vec2(Vec2),
}

/// One controller's pose and inputs at the last `sync_actions`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControllerState {
    /// Which hand.
    pub hand: Hand,
    /// False while the controller is not tracked or not connected; its pose and inputs are stale.
    pub active: bool,
    /// The grip pose (where the hand holds it).
    pub grip: PoseSample,
    /// The aim pose (where it points).
    pub aim: PoseSample,
    /// Trigger, zero to one.
    pub trigger: f32,
    /// Grip squeeze, zero to one.
    pub squeeze: f32,
    /// Thumbstick, each axis −1 to 1.
    pub thumbstick: Vec2,
    /// Primary face button.
    pub primary: bool,
    /// Secondary face button.
    pub secondary: bool,
    /// Thumbstick pressed in.
    pub thumbstick_click: bool,
    /// Menu button.
    pub menu: bool,
}

/// A trigger or squeeze value as gameplay may read it: `0..=1`, with NaN read as released.
fn unit_range(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// A thumbstick value as gameplay may read it: each axis `-1..=1`, with NaN read as centered.
fn stick_range(v: Vec2) -> Vec2 {
    let axis = |x: f32| if x.is_nan() { 0.0 } else { x.clamp(-1.0, 1.0) };
    Vec2::new(axis(v.x), axis(v.y))
}

impl ControllerState {
    /// An inactive controller at the origin with every input released.
    pub fn inactive(hand: Hand) -> Self {
        ControllerState {
            hand,
            active: false,
            grip: PoseSample::default(),
            aim: PoseSample::default(),
            trigger: 0.0,
            squeeze: 0.0,
            thumbstick: Vec2::ZERO,
            primary: false,
            secondary: false,
            thumbstick_click: false,
            menu: false,
        }
    }

    /// The value of `action`. An inactive controller reports released (false, zero) values.
    pub fn action(&self, action: ControllerAction) -> ActionValue {
        if !self.active {
            return match action {
                ControllerAction::Trigger | ControllerAction::Squeeze => ActionValue::Float(0.0),
                ControllerAction::Thumbstick => ActionValue::Vec2(Vec2::ZERO),
                _ => ActionValue::Bool(false),
            };
        }
        match action {
            ControllerAction::Trigger => ActionValue::Float(unit_range(self.trigger)),
            ControllerAction::Squeeze => ActionValue::Float(unit_range(self.squeeze)),
            ControllerAction::Thumbstick => ActionValue::Vec2(stick_range(self.thumbstick)),
            ControllerAction::PrimaryButton => ActionValue::Bool(self.primary),
            ControllerAction::SecondaryButton => ActionValue::Bool(self.secondary),
            ControllerAction::ThumbstickClick => ActionValue::Bool(self.thumbstick_click),
            ControllerAction::Menu => ActionValue::Bool(self.menu),
        }
    }
}

/// A haptic pulse a backend was asked to play.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HapticPulse {
    /// Which controller.
    pub hand: Hand,
    /// Strength, zero to one.
    pub amplitude: f32,
    /// Length in nanoseconds.
    pub duration_ns: i64,
}

// ---------------------------------------------------------------------------------------------
// The backend trait
// ---------------------------------------------------------------------------------------------

/// Something that happened in the runtime, read with [`XrBackend::poll_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XrEvent {
    /// The session moved to `state`.
    StateChanged {
        /// The new state.
        state: SessionState,
    },
    /// A controller became tracked or lost tracking.
    ControllerActive {
        /// Which hand.
        hand: Hand,
        /// Whether it is tracked now.
        active: bool,
    },
    /// Input focus moved to (`true`) or away from (`false`) the app: raised on entering or leaving
    /// [`SessionState::Focused`], after the matching `StateChanged`.
    FocusChanged {
        /// Whether the app has input focus now.
        focused: bool,
    },
    /// The runtime's visibility mask changed: read [`XrBackend::visibility_mask`] again.
    VisibilityMaskChanged,
}

/// What the app hands to `end_frame`.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSubmit {
    /// The display time `wait_frame` returned for this frame.
    pub display_time_ns: i64,
    /// The late-latched views, the pose the next prediction starts from. A projection layer carries
    /// the poses each eye was rendered with (the views passed to `render`, `xr/layers.rs`), not these.
    pub views: [View; 2],
    /// False when nothing was rendered (the headset is not visible).
    pub rendered: bool,
    /// The composition layers. Empty on a frame that did not render, and on a frame whose host
    /// composes nothing ([`run_frame`]).
    pub layers: LayerList,
}

/// A simulated runtime event for backends that can script them ([`XrBackend::simulate`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SimEvent {
    /// The runtime becomes ready.
    RuntimeReady,
    /// The runtime ends the session.
    RuntimeStop,
    /// The head moves to this pose (zero velocity). A pose with a non-finite position is ignored:
    /// the head keeps its last pose, so no NaN reaches the eye views (ADR-1507, ADR-1660).
    HeadPose(Pose),
    /// The runtime applies a session transition (synchronized, visible, focused, lost, exiting, or
    /// any other runtime-side [`Transition`]).
    Runtime(Transition),
    /// Both eyes at the head (`true`), or back at the configured IPD: the zero-disparity check of
    /// spec 58 § 8.13. A real IPD is always inside [`IPD_RANGE_M`].
    MonoEyes(bool),
    /// The scripted lens: `radius` as in [`VisibilityMask::lens`] over the current field of view,
    /// 0 for no mask. Raises [`XrEvent::VisibilityMaskChanged`].
    LensMask {
        /// The lens radius, 0 for none.
        radius: f32,
    },
    /// A controller's live state (pose, tracking and inputs); read at the next `sync_actions`.
    Controller(ControllerState),
    /// The eyes' interpupillary distance (meters) and field of view, validated as `stereo_views`
    /// does; applies from the next located views.
    ViewConfig {
        /// Interpupillary distance, meters.
        ipd_m: f32,
        /// Field of view of both eyes.
        fov: Fov,
    },
}

/// A head-mounted-display runtime. A real binding (OpenXR, linked later) and [`NullXr`] both
/// implement it. All calls come from the frame thread.
pub trait XrBackend: Send + Sync {
    /// A short name for logs and the agent protocol (`"null"`, `"openxr"`).
    fn name(&self) -> &'static str;

    /// The current session state.
    fn state(&self) -> SessionState;

    /// Drains the events since the last call.
    fn poll_events(&mut self) -> Vec<XrEvent>;

    /// Begins the session (ready → running).
    fn begin_session(&mut self) -> Result<(), XrError>;

    /// Asks the session to stop (running → stopping).
    fn request_exit(&mut self) -> Result<(), XrError>;

    /// Ends the session (stopping → idle).
    fn end_session(&mut self) -> Result<(), XrError>;

    /// Waits for the next frame and returns its predicted timing.
    fn wait_frame(&mut self) -> Result<FrameTiming, XrError>;

    /// Marks the start of rendering for the waited frame.
    fn begin_frame(&mut self) -> Result<(), XrError>;

    /// The two eye views predicted for `display_time_ns`.
    fn locate_views(&mut self, display_time_ns: i64) -> Result<[View; 2], XrError>;

    /// The newest head sample (pose and velocities), for late latching.
    fn head_sample(&self) -> PoseSample;

    /// Reads the controllers' poses and inputs.
    fn sync_actions(&mut self) -> Result<(), XrError>;

    /// The controller state from the last `sync_actions`.
    fn controller(&self, hand: Hand) -> ControllerState;

    /// Plays a haptic pulse (`amplitude` zero to one).
    fn apply_haptic(&mut self, hand: Hand, amplitude: f32, duration_ns: i64) -> Result<(), XrError>;

    /// Submits the frame.
    fn end_frame(&mut self, submit: &FrameSubmit) -> Result<(), XrError>;

    /// The hidden triangle mesh of `eye`, when the runtime offers the visibility mask extension.
    fn visibility_mask(&self, _eye: Eye) -> Option<VisibilityMask> {
        None
    }

    /// Whether the runtime takes depth images beside the projection layer (the depth-info layer
    /// extension). A runtime without it gets colour only.
    fn supports_depth_layer(&self) -> bool {
        false
    }

    /// The composition layers one frame may submit (the projection layer included).
    fn max_layers(&self) -> usize {
        MAX_QUAD_LAYERS + 1
    }

    /// Scripts a runtime event. Only test backends support it.
    fn simulate(&mut self, _event: SimEvent) -> Result<(), XrError> {
        Err(XrError::Unsupported("simulate"))
    }

    /// The interpupillary distance (meters) and field of view the backend currently locates
    /// views with, when it can report them. Only simulator backends do; the default is `None`.
    fn view_config(&self) -> Option<(f32, Fov)> {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// The null backend
// ---------------------------------------------------------------------------------------------

/// Segments of [`NullXr`]'s scripted lens mask.
const LENS_SEGMENTS: u32 = 48;

/// Configuration of a [`NullXr`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NullConfig {
    /// Interpupillary distance, meters.
    pub ipd_m: f32,
    /// The field of view of both eyes.
    pub fov: Fov,
    /// Display period in nanoseconds (11.1 ms is 90 Hz). Must be positive: [`NullXr::new`]
    /// replaces a zero or negative period with the default.
    pub display_period_ns: i64,
    /// The clock value frame zero's period starts from.
    pub start_time_ns: i64,
}

impl Default for NullConfig {
    fn default() -> Self {
        NullConfig {
            ipd_m: DEFAULT_IPD_M,
            fov: Fov::symmetric(0.9, 0.9),
            display_period_ns: 11_111_111,
            start_time_ns: 0,
        }
    }
}

/// A deterministic, scripted backend for tests, CI and headless runs. It never talks to a device:
/// time is `start_time_ns` plus whole display periods, the head and controllers are set by the
/// test, and the session moves only through [`NullXr::simulate`] and the trait calls.
#[derive(Debug, Clone)]
pub struct NullXr {
    config: NullConfig,
    core: SessionCore,
    events: Vec<XrEvent>,
    head: PoseSample,
    controllers: [ControllerState; 2],
    synced: [ControllerState; 2],
    haptics: Vec<HapticPulse>,
    submitted: Vec<FrameSubmit>,
    next_frame: u64,
    lens_radius: f32,
    max_layers: usize,
    mono_eyes: bool,
}

impl NullXr {
    /// A null backend, idle, head at the origin, controllers inactive.
    pub fn new(mut config: NullConfig) -> Self {
        // Frame times are whole periods: a zero or negative period would stand time still or run
        // it backwards while frame indices advance (audit ext-b Low).
        if config.display_period_ns <= 0 {
            tracing::warn!(
                period_ns = config.display_period_ns,
                "xr null backend: the display period must be positive; using the default"
            );
            config.display_period_ns = NullConfig::default().display_period_ns;
        }
        let controllers = [
            ControllerState::inactive(Hand::Left),
            ControllerState::inactive(Hand::Right),
        ];
        NullXr {
            config,
            core: SessionCore::new(),
            events: Vec::new(),
            head: PoseSample::at_rest(Pose::IDENTITY, config.start_time_ns),
            controllers,
            synced: controllers,
            haptics: Vec::with_capacity(MAX_KEPT_HAPTICS),
            submitted: Vec::with_capacity(MAX_KEPT_SUBMITS),
            next_frame: 0,
            lens_radius: 0.0,
            max_layers: MAX_QUAD_LAYERS + 1,
            mono_eyes: false,
        }
    }

    /// Sets the layer budget this runtime reports, at least 1 (the projection layer).
    pub fn set_max_layers(&mut self, max_layers: usize) {
        self.max_layers = max_layers.max(1);
    }

    /// The session core (state, frame phase, frames ended).
    pub fn core(&self) -> &SessionCore {
        &self.core
    }

    /// Sets the tracked head sample.
    pub fn set_head(&mut self, head: PoseSample) {
        self.head = head;
    }

    /// Sets a controller's live state; `sync_actions` copies it to what `controller` reports.
    /// Raises [`XrEvent::ControllerActive`] when the tracked flag changes.
    pub fn set_controller(&mut self, state: ControllerState) {
        let index = state.hand.index();
        if self.controllers[index].active != state.active {
            self.push_event(XrEvent::ControllerActive {
                hand: state.hand,
                active: state.active,
            });
        }
        self.controllers[index] = ControllerState {
            trigger: unit_range(state.trigger),
            squeeze: unit_range(state.squeeze),
            thumbstick: stick_range(state.thumbstick),
            ..state
        };
    }

    /// The most recent haptic pulses (at most [`MAX_KEPT_HAPTICS`]), oldest kept first.
    pub fn haptics(&self) -> &[HapticPulse] {
        &self.haptics
    }

    /// The most recent frame submits (at most [`MAX_KEPT_SUBMITS`]), oldest kept first.
    ///
    /// [`SessionCore::frames_ended`] counts every ended frame, including ones that have
    /// fallen out of this window.
    pub fn submitted(&self) -> &[FrameSubmit] {
        &self.submitted
    }

    fn push_event(&mut self, event: XrEvent) {
        if self.events.len() >= MAX_KEPT_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    fn apply(&mut self, transition: Transition) -> Result<(), XrError> {
        let from = self.core.state();
        let state = self.core.transition(transition)?;
        self.push_event(XrEvent::StateChanged { state });
        if (from == SessionState::Focused) != (state == SessionState::Focused) {
            self.push_event(XrEvent::FocusChanged {
                focused: state == SessionState::Focused,
            });
        }
        Ok(())
    }
}

impl Default for NullXr {
    fn default() -> Self {
        NullXr::new(NullConfig::default())
    }
}

/// Appends `value`, dropping the oldest entries so `buf` stays at most `cap` long.
fn push_kept<T>(buf: &mut Vec<T>, value: T, cap: usize) {
    if buf.len() >= cap {
        buf.drain(..buf.len() + 1 - cap);
    }
    buf.push(value);
}

impl XrBackend for NullXr {
    fn name(&self) -> &'static str {
        "null"
    }

    fn view_config(&self) -> Option<(f32, Fov)> {
        Some((self.config.ipd_m, self.config.fov))
    }

    fn state(&self) -> SessionState {
        self.core.state()
    }

    fn poll_events(&mut self) -> Vec<XrEvent> {
        std::mem::take(&mut self.events)
    }

    fn begin_session(&mut self) -> Result<(), XrError> {
        // Frame indices restart only for a session that does begin: a refused `begin` (already
        // running) must not reuse frame index 0.
        self.apply(Transition::Begin)?;
        self.next_frame = 0;
        Ok(())
    }

    fn request_exit(&mut self) -> Result<(), XrError> {
        self.apply(Transition::RequestExit)
    }

    fn end_session(&mut self) -> Result<(), XrError> {
        self.apply(Transition::End)
    }

    fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
        let period = self.config.display_period_ns;
        let timing = FrameTiming {
            frame_index: self.next_frame,
            predicted_display_time_ns: self
                .config
                .start_time_ns
                .saturating_add(period.saturating_mul(self.next_frame as i64 + 1)),
            predicted_display_period_ns: period,
            should_render: self.core.state().shows_frames(),
        };
        self.core.frame_waited(timing)?;
        self.next_frame += 1;
        Ok(timing)
    }

    fn begin_frame(&mut self) -> Result<(), XrError> {
        self.core.frame_begun()
    }

    fn locate_views(&mut self, display_time_ns: i64) -> Result<[View; 2], XrError> {
        if !self.core.state().frames_flow() {
            return Err(XrError::NotRunning {
                call: "locate_views",
                state: self.core.state(),
            });
        }
        let head = self
            .head
            .extrapolate(display_time_ns, DEFAULT_MAX_EXTRAPOLATION_NS);
        let mut views = stereo_views(&head.pose, self.config.ipd_m, self.config.fov)?;
        if self.mono_eyes {
            for view in &mut views {
                view.pose = head.pose;
            }
        }
        Ok(views)
    }

    fn head_sample(&self) -> PoseSample {
        self.head
    }

    fn sync_actions(&mut self) -> Result<(), XrError> {
        if !self.core.state().frames_flow() {
            return Err(XrError::NotRunning {
                call: "sync_actions",
                state: self.core.state(),
            });
        }
        self.synced = self.controllers;
        Ok(())
    }

    fn controller(&self, hand: Hand) -> ControllerState {
        self.synced[hand.index()]
    }

    fn apply_haptic(&mut self, hand: Hand, amplitude: f32, duration_ns: i64) -> Result<(), XrError> {
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(XrError::InvalidHaptic { amplitude });
        }
        if duration_ns < 0 {
            return Err(XrError::InvalidValue {
                key: "duration_ns".into(),
                reason: "a haptic duration cannot be negative".into(),
            });
        }
        push_kept(
            &mut self.haptics,
            HapticPulse {
                hand,
                amplitude,
                duration_ns,
            },
            MAX_KEPT_HAPTICS,
        );
        Ok(())
    }

    fn end_frame(&mut self, submit: &FrameSubmit) -> Result<(), XrError> {
        for view in &submit.views {
            view.fov.validate()?;
        }
        if let Err(err) = submit.layers.validate(self.max_layers) {
            // The frame is over either way: leave the session able to wait for the next one.
            let _ = self.core.frame_ended(submit.display_time_ns);
            return Err(err);
        }
        self.core.frame_ended(submit.display_time_ns)?;
        push_kept(&mut self.submitted, submit.clone(), MAX_KEPT_SUBMITS);
        Ok(())
    }

    fn visibility_mask(&self, _eye: Eye) -> Option<VisibilityMask> {
        VisibilityMask::lens(self.config.fov, self.lens_radius, LENS_SEGMENTS)
    }

    fn supports_depth_layer(&self) -> bool {
        true
    }

    fn max_layers(&self) -> usize {
        self.max_layers
    }

    fn simulate(&mut self, event: SimEvent) -> Result<(), XrError> {
        match event {
            SimEvent::RuntimeReady => self.apply(Transition::RuntimeReady),
            SimEvent::RuntimeStop => self.apply(Transition::RuntimeStop),
            SimEvent::HeadPose(pose) => {
                if pose.position.is_finite() && pose.orientation.is_finite() {
                    self.head = PoseSample::at_rest(pose, self.head.time_ns);
                }
                Ok(())
            }
            SimEvent::Runtime(transition) => match transition {
                // App-side calls go through the trait methods, never through `simulate`.
                Transition::Begin | Transition::RequestExit | Transition::End => {
                    Err(XrError::InvalidTransition {
                        from: self.core.state(),
                        transition,
                    })
                }
                _ => self.apply(transition),
            },
            SimEvent::Controller(state) => {
                self.set_controller(state);
                Ok(())
            }
            SimEvent::MonoEyes(mono) => {
                self.mono_eyes = mono;
                Ok(())
            }
            SimEvent::LensMask { radius } => {
                if radius != 0.0 && VisibilityMask::lens(self.config.fov, radius, LENS_SEGMENTS).is_none() {
                    return Err(XrError::InvalidValue {
                        key: "lens radius".to_string(),
                        reason: "0 (no mask) or 0.1..=2.0".to_string(),
                    });
                }
                self.lens_radius = radius;
                self.push_event(XrEvent::VisibilityMaskChanged);
                Ok(())
            }
            SimEvent::ViewConfig { ipd_m, fov } => {
                stereo_views(&Pose::IDENTITY, ipd_m, fov)?;
                self.config.ipd_m = ipd_m;
                self.config.fov = fov;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// One frame, and the ECS resource
// ---------------------------------------------------------------------------------------------

/// What [`run_frame`] did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameReport {
    /// The frame's timing.
    pub timing: FrameTiming,
    /// The views submitted (late-latched); equal to the located views when nothing was rendered.
    pub views: [View; 2],
    /// Per eye, the late-latch correction applied (identity when nothing was rendered).
    pub corrections: [Pose; 2],
    /// Whether the app's render closure ran.
    pub rendered: bool,
    /// Whether the head prediction was clamped.
    pub clamped: bool,
}

/// Runs one frame on `backend`: wait, begin, locate views, sync actions, call `render` with the
/// timing and the views, late-latch the head, end. `render` is skipped when the runtime says not to
/// render. If a step fails after `begin_frame`, the frame is still ended (with the located views
/// when there are any) so the runtime is not left mid-frame, and the first error is returned.
pub fn run_frame<B: XrBackend + ?Sized>(
    backend: &mut B,
    latch: &mut LateLatch,
    render: impl FnOnce(&FrameTiming, &[View; 2]),
) -> Result<FrameReport, XrError> {
    run_frame_layers(backend, latch, |timing, views| {
        render(timing, views);
        LayerList::default()
    })
}

/// [`run_frame`] for a host that composes: `render` returns the frame's layers, which go to
/// `end_frame`. A frame that did not render submits an empty list, as does one that failed after
/// `begin_frame`.
pub fn run_frame_layers<B: XrBackend + ?Sized>(
    backend: &mut B,
    latch: &mut LateLatch,
    render: impl FnOnce(&FrameTiming, &[View; 2]) -> LayerList,
) -> Result<FrameReport, XrError> {
    let _span = tracing::trace_span!("xr_frame").entered();
    let timing = {
        let _wait = tracing::trace_span!("xr_wait_frame").entered();
        backend.wait_frame()?
    };
    {
        let _begin = tracing::trace_span!("xr_begin_frame").entered();
        backend.begin_frame()?;
    }
    let early_head = latch.predict(&backend.head_sample(), timing.predicted_display_time_ns);
    let located = {
        let _locate = tracing::trace_span!("xr_locate_views").entered();
        backend.locate_views(timing.predicted_display_time_ns)
    }
    .and_then(|views| backend.sync_actions().map(|()| views));
    let views = match located {
        Ok(views) => views,
        Err(err) => {
            // Leave the runtime in a clean state: end the begun frame, report the original error.
            let fallback = stereo_views(&early_head.pose, DEFAULT_IPD_M, Fov::symmetric(0.9, 0.9));
            if let Ok(views) = fallback {
                let _ = backend.end_frame(&FrameSubmit {
                    display_time_ns: timing.predicted_display_time_ns,
                    views,
                    rendered: false,
                    layers: LayerList::default(),
                });
            }
            return Err(err);
        }
    };
    let (submit_views, corrections, clamped, layers) = if timing.should_render {
        let layers = render(&timing, &views);
        let _latch_span = tracing::trace_span!("xr_late_latch").entered();
        let latched = latch.latch_views(
            &early_head.pose,
            &views,
            &backend.head_sample(),
            timing.predicted_display_time_ns,
        );
        (latched.views, latched.corrections, latched.clamped, layers)
    } else {
        (
            views,
            [Pose::IDENTITY; 2],
            early_head.clamped,
            LayerList::default(),
        )
    };
    let end_span = tracing::trace_span!("xr_submit_layers", layers = layers.len()).entered();
    let end_frame_span = tracing::trace_span!("xr_end_frame").entered();
    backend.end_frame(&FrameSubmit {
        display_time_ns: timing.predicted_display_time_ns,
        views: submit_views,
        rendered: timing.should_render,
        layers,
    })?;
    drop(end_frame_span);
    drop(end_span);
    Ok(FrameReport {
        timing,
        views: submit_views,
        corrections,
        rendered: timing.should_render,
        clamped,
    })
}

/// The XR backend as an ECS resource, with the late latch and the last frame report. Insert one to
/// make `xr.get` and `xr.drive` available; nothing installs it by default.
#[derive(Resource)]
pub struct XrRuntime {
    backend: Box<dyn XrBackend>,
    latch: LateLatch,
    last_report: Option<FrameReport>,
    last_layers: LayerList,
    /// Scratch copy of `last_layers` while a composed frame is in flight (restored on error).
    prev_layers: LayerList,
    frames: u64,
    events: Vec<XrEvent>,
}

impl XrRuntime {
    /// Wraps `backend`.
    pub fn new(backend: Box<dyn XrBackend>) -> Self {
        XrRuntime {
            backend,
            latch: LateLatch::default(),
            last_report: None,
            last_layers: LayerList::default(),
            prev_layers: LayerList::default(),
            frames: 0,
            events: Vec::new(),
        }
    }

    /// A runtime over a default [`NullXr`].
    pub fn null() -> Self {
        XrRuntime::new(Box::new(NullXr::default()))
    }

    /// The backend.
    pub fn backend(&self) -> &dyn XrBackend {
        self.backend.as_ref()
    }

    /// The backend, mutably.
    pub fn backend_mut(&mut self) -> &mut dyn XrBackend {
        self.backend.as_mut()
    }

    /// The late latch (counters and the extrapolation window).
    pub fn latch(&self) -> &LateLatch {
        &self.latch
    }

    /// The late latch, mutably.
    pub fn latch_mut(&mut self) -> &mut LateLatch {
        &mut self.latch
    }

    /// The last frame [`XrRuntime::frame`] ran.
    pub fn last_report(&self) -> Option<&FrameReport> {
        self.last_report.as_ref()
    }

    /// Frames [`XrRuntime::frame`] ran successfully.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The most recent events (up to [`MAX_KEPT_EVENTS`]).
    pub fn events(&self) -> &[XrEvent] {
        &self.events
    }

    /// Drains the backend's events into the kept list.
    pub fn pump(&mut self) {
        self.pump_new();
    }

    /// Drains the backend's events into the kept list and returns them, in runtime order.
    pub fn pump_new(&mut self) -> Vec<XrEvent> {
        let events = self.backend.poll_events();
        for event in &events {
            if self.events.len() >= MAX_KEPT_EVENTS {
                self.events.remove(0);
            }
            self.events.push(*event);
        }
        events
    }

    /// Runs one frame (see [`run_frame`]) and records its report.
    pub fn frame(&mut self, render: impl FnOnce(&FrameTiming, &[View; 2])) -> Result<FrameReport, XrError> {
        let report = run_frame(self.backend.as_mut(), &mut self.latch, render)?;
        self.frames += 1;
        self.last_report = Some(report);
        Ok(report)
    }

    /// Runs one frame whose host composes (see [`run_frame_layers`]) and records its report.
    pub fn frame_layers(
        &mut self,
        render: impl FnOnce(&FrameTiming, &[View; 2]) -> LayerList,
    ) -> Result<FrameReport, XrError> {
        // Keep the previous quad allocation. `Vec::clone_from` reuses that buffer when the
        // panel count does not grow; `LayerList`'s derived `clone_from` would drop it and
        // allocate again. A refused list does not replace the last accepted one.
        // A backend that fails `end_frame` after the list validated (a lost session) submitted
        // nothing either, so the previous list is copied aside before it is overwritten and put
        // back on error (`prev_layers` keeps its own buffer, so this does not allocate per frame).
        let max_layers = self.backend.max_layers();
        let mut staged = std::mem::take(&mut self.last_layers);
        let mut prev = std::mem::take(&mut self.prev_layers);
        let mut overwritten = false;
        let report = run_frame_layers(self.backend.as_mut(), &mut self.latch, |timing, views| {
            let layers = render(timing, views);
            if layers.validate(max_layers).is_ok() {
                prev.projection = staged.projection;
                prev.quads.clone_from(&staged.quads);
                overwritten = true;
                staged.projection = layers.projection;
                staged.quads.clone_from(&layers.quads);
            }
            layers
        });
        match report {
            Ok(report) => {
                if !report.rendered {
                    staged.projection = None;
                    staged.quads.clear();
                }
                self.frames += 1;
                self.last_report = Some(report);
                self.last_layers = staged;
                self.prev_layers = prev;
                Ok(report)
            }
            Err(err) => {
                if overwritten {
                    staged.projection = prev.projection;
                    staged.quads.clone_from(&prev.quads);
                }
                self.last_layers = staged;
                self.prev_layers = prev;
                Err(err)
            }
        }
    }

    /// The layers of the last composed frame ([`Self::frame_layers`]); empty when it drew nothing.
    pub fn last_layers(&self) -> &LayerList {
        &self.last_layers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() <= EPS * (1.0 + a.abs().max(b.abs()))
    }

    fn same_rot(a: Quat, b: Quat) -> bool {
        a.dot(b).abs() > 1.0 - 1e-6
    }

    fn ndc(m: &Mat4, p: Vec3) -> Vec3 {
        let c = *m * p.extend(1.0);
        c.truncate() / c.w
    }

    #[test]
    fn a_refused_begin_keeps_the_frame_counter() {
        // A second `begin` while running fails and used to reset the next frame index (ADR-1660).
        let mut xr = running();
        let first = xr.wait_frame().expect("wait").frame_index;
        assert_eq!(
            xr.begin_session().expect_err("already running").code(),
            "xr_invalid_transition"
        );
        // A second wait replaces the dropped frame and takes the next index.
        let next = xr.wait_frame().expect("wait").frame_index;
        assert_eq!(next, first + 1, "frame indices keep counting");
    }

    fn running() -> NullXr {
        let mut xr = NullXr::default();
        xr.simulate(SimEvent::RuntimeReady).expect("ready");
        xr.begin_session().expect("begin");
        xr
    }

    fn asym() -> Fov {
        Fov {
            left: -0.8,
            right: 0.6,
            up: 0.7,
            down: -0.9,
        }
    }

    // -- projection ------------------------------------------------------------------------

    #[test]
    fn frustum_edges_map_to_ndc_extremes() {
        let fov = asym();
        let range = DepthRange { near: 0.1, far: 50.0 };
        let m = fov.projection(range, DepthMode::Reversed).expect("projection");
        let [tl, tr, tu, td] = fov.tangents();
        let d = 5.0;
        let z = -d;
        // The four side planes at depth d.
        assert!(approx(ndc(&m, Vec3::new(tl * d, 0.0, z)).x, -1.0));
        assert!(approx(ndc(&m, Vec3::new(tr * d, 0.0, z)).x, 1.0));
        assert!(approx(ndc(&m, Vec3::new(0.0, tu * d, z)).y, 1.0));
        assert!(approx(ndc(&m, Vec3::new(0.0, td * d, z)).y, -1.0));
        // The view axis is off-centre for an asymmetric view.
        assert!(ndc(&m, Vec3::new(0.0, 0.0, z)).x > 0.0);
        assert!(ndc(&m, Vec3::new(0.0, 0.0, z)).y > 0.0);
    }

    #[test]
    fn symmetric_fov_matches_the_standard_perspective_matrix() {
        let fov = Fov::symmetric(0.5, 0.4);
        let range = DepthRange { near: 0.2, far: 80.0 };
        let m = fov.projection(range, DepthMode::Standard).expect("projection");
        // The textbook right-handed, zero-to-one perspective matrix for a symmetric frustum.
        let (n, f) = (0.2f32, 80.0f32);
        let cols = m.to_cols_array();
        assert!(approx(cols[0], 1.0 / 0.5f32.tan()));
        assert!(approx(cols[5], 1.0 / 0.4f32.tan()));
        assert!(approx(cols[8], 0.0) && approx(cols[9], 0.0));
        assert!(approx(cols[10], f / (n - f)));
        assert!(approx(cols[11], -1.0));
        assert!(approx(cols[14], n * f / (n - f)));
        assert!(approx(cols[15], 0.0));
    }

    #[test]
    fn reversed_z_puts_near_at_one_and_far_at_zero() {
        let fov = Fov::symmetric(0.7, 0.7);
        let range = DepthRange {
            near: 0.1,
            far: 100.0,
        };
        let m = fov.projection(range, DepthMode::Reversed).expect("projection");
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -0.1)).z, 1.0));
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -100.0)).z, 0.0));
        let mid = ndc(&m, Vec3::new(0.0, 0.0, -1.0)).z;
        assert!(mid > 0.0 && mid < 1.0);
        // Depth falls monotonically with distance.
        let near = ndc(&m, Vec3::new(0.0, 0.0, -2.0)).z;
        let far = ndc(&m, Vec3::new(0.0, 0.0, -20.0)).z;
        assert!(near > far);
    }

    #[test]
    fn standard_z_puts_near_at_zero_and_far_at_one() {
        let fov = Fov::symmetric(0.7, 0.7);
        let range = DepthRange {
            near: 0.1,
            far: 100.0,
        };
        let m = fov.projection(range, DepthMode::Standard).expect("projection");
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -0.1)).z, 0.0));
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -100.0)).z, 1.0));
    }

    #[test]
    fn infinite_far_reversed_z_approaches_zero() {
        let fov = Fov::symmetric(0.7, 0.7);
        let range = DepthRange {
            near: 0.25,
            far: f32::INFINITY,
        };
        let m = fov.projection(range, DepthMode::Reversed).expect("projection");
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -0.25)).z, 1.0));
        // ndc z = near / distance.
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -25.0)).z, 0.01));
        assert!(ndc(&m, Vec3::new(0.0, 0.0, -1.0e6)).z >= 0.0);
        assert!(m.to_cols_array().iter().all(|v| v.is_finite()));
    }

    #[test]
    fn infinite_far_standard_z_approaches_one() {
        let fov = Fov::symmetric(0.7, 0.7);
        let range = DepthRange {
            near: 0.25,
            far: f32::INFINITY,
        };
        let m = fov.projection(range, DepthMode::Standard).expect("projection");
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -0.25)).z, 0.0));
        assert!(approx(ndc(&m, Vec3::new(0.0, 0.0, -25.0)).z, 0.99));
        assert!(ndc(&m, Vec3::new(0.0, 0.0, -1.0e6)).z <= 1.0);
    }

    #[test]
    fn projection_rejects_bad_fov_and_depth() {
        let range = DepthRange { near: 0.1, far: 10.0 };
        let bad_fovs = [
            Fov {
                left: 0.2,
                right: -0.2,
                up: 0.5,
                down: -0.5,
            },
            Fov {
                left: -0.5,
                right: 0.5,
                up: -0.1,
                down: 0.1,
            },
            Fov {
                left: -FRAC_PI_2,
                right: 0.5,
                up: 0.5,
                down: -0.5,
            },
            Fov {
                left: -0.5,
                right: f32::NAN,
                up: 0.5,
                down: -0.5,
            },
            Fov {
                left: -0.5,
                right: 0.5,
                up: f32::INFINITY,
                down: -0.5,
            },
            Fov::symmetric(0.0, 0.5),
        ];
        for fov in bad_fovs {
            let err = fov.projection(range, DepthMode::Reversed).expect_err("bad fov");
            assert_eq!(err.code(), "xr_invalid_fov", "{fov:?}");
        }
        let good = Fov::symmetric(0.5, 0.5);
        for (near, far) in [
            (0.0, 10.0),
            (-1.0, 10.0),
            (f32::NAN, 10.0),
            (f32::INFINITY, f32::INFINITY),
            (1.0, 1.0),
            (1.0, 0.5),
            (1.0, f32::NAN),
        ] {
            let err = good
                .projection(DepthRange { near, far }, DepthMode::Reversed)
                .expect_err("bad range");
            assert_eq!(err.code(), "xr_invalid_depth_range", "{near} {far}");
        }
    }

    #[test]
    fn fov_extents() {
        let fov = asym();
        assert!(approx(fov.horizontal(), 1.4));
        assert!(approx(fov.vertical(), 1.6));
        fov.validate().expect("valid");
    }

    // -- poses -----------------------------------------------------------------------------

    #[test]
    fn pose_inverse_and_compose_round_trip() {
        let pose = Pose::new(Vec3::new(1.0, 2.0, 3.0), Quat::from_rotation_y(0.7));
        let id = pose.compose(&pose.inverse());
        assert!(id.position.length() < EPS);
        assert!(same_rot(id.orientation, Quat::IDENTITY));
        let p = Vec3::new(0.5, -0.2, 0.9);
        let via_view = pose
            .view_matrix()
            .transform_point3(pose.to_matrix().transform_point3(p));
        assert!((via_view - p).length() < EPS);
    }

    #[test]
    fn pose_new_repairs_bad_orientation() {
        assert_eq!(
            Pose::new(Vec3::ZERO, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)).orientation,
            Quat::IDENTITY
        );
        assert_eq!(
            Pose::new(Vec3::ZERO, Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0)).orientation,
            Quat::IDENTITY
        );
        let scaled = Pose::new(Vec3::ZERO, Quat::from_xyzw(0.0, 0.0, 0.0, 2.0));
        assert!(approx(scaled.orientation.length(), 1.0));
    }

    #[test]
    fn stereo_views_split_the_eyes_along_head_x() {
        let head = Pose::new(Vec3::new(0.0, 1.6, 0.0), Quat::IDENTITY);
        let [l, r] = stereo_views(&head, 0.064, Fov::symmetric(0.8, 0.8)).expect("views");
        assert_eq!((l.eye, r.eye), (Eye::Left, Eye::Right));
        assert!(approx(l.pose.position.x, -0.032));
        assert!(approx(r.pose.position.x, 0.032));
        assert!(approx(l.pose.position.y, 1.6));
        // Turn the head 90 degrees left: the right eye ends up behind (+Z) the left.
        let turned = Pose::new(Vec3::ZERO, Quat::from_rotation_y(FRAC_PI_2));
        let [l, r] = stereo_views(&turned, 0.064, Fov::symmetric(0.8, 0.8)).expect("views");
        assert!(r.pose.position.z < l.pose.position.z);
        assert!(l.pose.position.x.abs() < EPS);
    }

    #[test]
    fn stereo_views_reject_bad_ipd_and_fov() {
        let head = Pose::IDENTITY;
        let fov = Fov::symmetric(0.8, 0.8);
        for ipd in [0.0, 0.01, 0.2, f32::NAN, f32::INFINITY, -0.06] {
            assert_eq!(
                stereo_views(&head, ipd, fov).expect_err("ipd").code(),
                "xr_invalid_ipd"
            );
        }
        assert_eq!(
            stereo_views(&head, 0.06, Fov::symmetric(2.0, 0.5))
                .expect_err("fov")
                .code(),
            "xr_invalid_fov"
        );
    }

    #[test]
    fn view_projection_puts_a_point_ahead_of_the_eye_in_front() {
        let head = Pose::new(Vec3::new(0.0, 0.0, 2.0), Quat::IDENTITY);
        let [l, _] = stereo_views(&head, 0.06, Fov::symmetric(0.8, 0.8)).expect("views");
        let range = DepthRange {
            near: 0.1,
            far: 100.0,
        };
        let vp = l.view_projection(range, DepthMode::Reversed).expect("vp");
        let p = ndc(&vp, Vec3::new(0.0, 0.0, 0.0));
        assert!(p.z > 0.0 && p.z < 1.0, "in front of the eye: {p:?}");
        // The left eye sees an on-axis point right of centre.
        assert!(p.x > 0.0);
    }

    // -- prediction and late latch ---------------------------------------------------------

    #[test]
    fn extrapolation_is_linear_and_deterministic() {
        let sample = PoseSample {
            pose: Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY),
            linear_velocity: Vec3::new(2.0, 0.0, -1.0),
            angular_velocity: Vec3::new(0.0, FRAC_PI_2, 0.0),
            time_ns: 1_000_000_000,
        };
        let p = sample.extrapolate(1_500_000_000, 1_000_000_000);
        assert!(!p.clamped);
        assert_eq!(p.extrapolated_ns, 500_000_000);
        assert!(approx(p.pose.position.x, 2.0));
        assert!(approx(p.pose.position.z, -0.5));
        // A quarter turn about Y in half a second at pi/2 per second would be pi/4.
        assert!(same_rot(
            p.pose.orientation,
            Quat::from_rotation_y(FRAC_PI_2 * 0.5)
        ));
        assert_eq!(p, sample.extrapolate(1_500_000_000, 1_000_000_000));
    }

    #[test]
    fn extrapolation_clamps_future_and_never_runs_backwards() {
        let sample = PoseSample {
            linear_velocity: Vec3::X,
            ..PoseSample::at_rest(Pose::IDENTITY, 100)
        };
        let far = sample.extrapolate(100 + 10_000_000_000, 50_000_000);
        assert!(far.clamped);
        assert_eq!(far.extrapolated_ns, 50_000_000);
        assert!(approx(far.pose.position.x, 0.05));
        let past = sample.extrapolate(0, 50_000_000);
        assert!(past.clamped);
        assert_eq!(past.extrapolated_ns, 0);
        assert_eq!(past.pose, Pose::IDENTITY);
        let now = sample.extrapolate(100, 50_000_000);
        assert!(!now.clamped);
        // A negative window counts as zero.
        assert_eq!(sample.extrapolate(200, -5).extrapolated_ns, 0);
    }

    #[test]
    fn extrapolation_ignores_non_finite_velocity() {
        let sample = PoseSample {
            linear_velocity: Vec3::new(f32::NAN, 0.0, 0.0),
            angular_velocity: Vec3::new(0.0, f32::INFINITY, 0.0),
            ..PoseSample::at_rest(Pose::IDENTITY, 0)
        };
        let p = sample.extrapolate(10_000_000, 50_000_000);
        assert_eq!(p.pose, Pose::IDENTITY);
    }

    #[test]
    fn late_latch_moves_both_eyes_by_the_head_change() {
        let fov = Fov::symmetric(0.8, 0.8);
        let early = PoseSample::at_rest(Pose::new(Vec3::new(0.0, 1.6, 0.0), Quat::IDENTITY), 0);
        let rendered = stereo_views(&early.pose, 0.064, fov).expect("views");
        // The head moved 0.1 m to +X and turned a little before submit.
        let moved = Pose::new(Vec3::new(0.1, 1.6, 0.0), Quat::from_rotation_y(0.2));
        let fresh = PoseSample::at_rest(moved, 5_000_000);
        let mut latch = LateLatch::default();
        let out = latch.latch_views(&early.pose, &rendered, &fresh, 5_000_000);
        assert!(!out.clamped);
        let expected = stereo_views(&moved, 0.064, fov).expect("views");
        for (i, view) in out.views.iter().enumerate() {
            assert!((view.pose.position - expected[i].pose.position).length() < EPS);
            assert!(same_rot(view.pose.orientation, expected[i].pose.orientation));
            // correction * rendered == latched.
            let again = out.corrections[i].compose(&rendered[i].pose);
            assert!((again.position - view.pose.position).length() < EPS);
        }
        assert_eq!((latch.latched(), latch.clamped()), (1, 0));
    }

    #[test]
    fn late_latch_with_an_unchanged_head_is_the_identity_correction() {
        let fov = Fov::symmetric(0.8, 0.8);
        let head = Pose::new(Vec3::new(1.0, 1.0, 1.0), Quat::from_rotation_x(0.3));
        let rendered = stereo_views(&head, 0.064, fov).expect("views");
        let mut latch = LateLatch::default();
        let out = latch.latch_views(&head, &rendered, &PoseSample::at_rest(head, 0), 0);
        for c in out.corrections {
            assert!(c.position.length() < EPS);
            assert!(same_rot(c.orientation, Quat::IDENTITY));
        }
    }

    #[test]
    fn late_latch_counts_clamped_predictions() {
        let fov = Fov::symmetric(0.8, 0.8);
        let rendered = stereo_views(&Pose::IDENTITY, 0.064, fov).expect("views");
        let mut latch = LateLatch::new(1_000_000);
        let fresh = PoseSample {
            linear_velocity: Vec3::X,
            ..PoseSample::at_rest(Pose::IDENTITY, 0)
        };
        let out = latch.latch_views(&Pose::IDENTITY, &rendered, &fresh, 20_000_000);
        assert!(out.clamped);
        assert!(approx(out.views[0].pose.position.x, -0.032 + 0.001));
        assert_eq!((latch.latched(), latch.clamped()), (1, 1));
    }

    // -- session state machine -------------------------------------------------------------

    #[test]
    fn session_walks_the_whole_cycle() {
        let mut core = SessionCore::new();
        assert_eq!(core.state(), SessionState::Idle);
        assert_eq!(core.transition(Transition::RuntimeReady), Ok(SessionState::Ready));
        assert_eq!(core.transition(Transition::Begin), Ok(SessionState::Running));
        assert_eq!(
            core.transition(Transition::RequestExit),
            Ok(SessionState::Stopping)
        );
        assert_eq!(core.transition(Transition::End), Ok(SessionState::Idle));
        // And again: the cycle repeats.
        assert_eq!(core.transition(Transition::RuntimeReady), Ok(SessionState::Ready));
    }

    #[test]
    fn runtime_stop_ends_ready_and_running_sessions() {
        let mut core = SessionCore::new();
        core.transition(Transition::RuntimeReady).expect("ready");
        assert_eq!(core.transition(Transition::RuntimeStop), Ok(SessionState::Idle));
        core.transition(Transition::RuntimeReady).expect("ready");
        core.transition(Transition::Begin).expect("begin");
        assert_eq!(
            core.transition(Transition::RuntimeStop),
            Ok(SessionState::Stopping)
        );
    }

    #[test]
    fn every_other_transition_is_rejected_and_leaves_the_state() {
        use SessionState::{
            Exiting, Focused, Idle, LossPending, Ready, Running, Stopping, Synchronized, Visible,
        };
        use Transition::{
            Begin, End, RequestExit, RuntimeExiting, RuntimeFocused, RuntimeLost, RuntimeReady, RuntimeStop,
            RuntimeSynchronized, RuntimeVisible,
        };
        let mut allowed = vec![
            (Idle, RuntimeReady),
            (Ready, Begin),
            (Ready, RuntimeStop),
            (Stopping, End),
            (LossPending, End),
            (Running, RuntimeSynchronized),
            (Visible, RuntimeSynchronized),
            (Synchronized, RuntimeVisible),
            (Focused, RuntimeVisible),
            (Visible, RuntimeFocused),
            (Idle, RuntimeExiting),
        ];
        for state in [Running, Synchronized, Visible, Focused] {
            allowed.push((state, RequestExit));
            allowed.push((state, RuntimeStop));
        }
        for state in [Idle, Ready, Running, Stopping, Synchronized, Visible, Focused] {
            allowed.push((state, RuntimeLost));
        }
        let reach = |state: SessionState| {
            let mut core = SessionCore::new();
            let path: &[Transition] = match state {
                Idle => &[],
                Ready => &[RuntimeReady],
                Running => &[RuntimeReady, Begin],
                Stopping => &[RuntimeReady, Begin, RequestExit],
                Synchronized => &[RuntimeReady, Begin, RuntimeSynchronized],
                Visible => &[RuntimeReady, Begin, RuntimeSynchronized, RuntimeVisible],
                Focused => &[
                    RuntimeReady,
                    Begin,
                    RuntimeSynchronized,
                    RuntimeVisible,
                    RuntimeFocused,
                ],
                LossPending => &[RuntimeLost],
                Exiting => &[RuntimeExiting],
            };
            for t in path {
                core.transition(*t).expect("path");
            }
            assert_eq!(core.state(), state);
            core
        };
        for state in SessionState::ALL {
            for t in Transition::ALL {
                let mut core = reach(state);
                let result = core.transition(t);
                if allowed.contains(&(state, t)) {
                    assert!(result.is_ok(), "{state:?} {t:?}");
                } else {
                    assert_eq!(
                        result,
                        Err(XrError::InvalidTransition {
                            from: state,
                            transition: t
                        })
                    );
                    assert_eq!(core.state(), state, "unchanged after {t:?}");
                }
            }
        }
    }

    #[test]
    fn frames_flow_in_every_begun_state_and_a_loss_drops_the_frame() {
        use Transition::{
            Begin, RuntimeFocused, RuntimeLost, RuntimeReady, RuntimeSynchronized, RuntimeVisible,
        };
        let mut core = SessionCore::new();
        for t in [RuntimeReady, Begin] {
            core.transition(t).expect("path");
        }
        for (i, t) in [RuntimeSynchronized, RuntimeVisible, RuntimeFocused]
            .into_iter()
            .enumerate()
        {
            core.transition(t).expect("sub-state");
            core.frame_waited(timing(i as u64)).expect("wait");
            core.frame_begun().expect("begin");
            core.frame_ended(timing(i as u64).predicted_display_time_ns)
                .expect("end");
        }
        assert_eq!(core.frames_ended(), 3);
        core.frame_waited(timing(3)).expect("wait");
        core.transition(RuntimeLost).expect("lost");
        assert_eq!(core.phase(), FramePhase::Idle);
        assert_eq!(core.pending_frame(), None);
        assert!(matches!(core.frame_begun(), Err(XrError::NotRunning { .. })));
    }

    #[test]
    fn state_names_round_trip_and_classify() {
        for state in SessionState::ALL {
            assert_eq!(SessionState::from_name(state.name()), Some(state));
        }
        assert_eq!(SessionState::from_name("nope"), None);
        assert_eq!(SessionState::LossPending.name(), "loss_pending");
        assert!(!SessionState::Synchronized.shows_frames());
        assert!(SessionState::Synchronized.frames_flow());
        assert!(!SessionState::Visible.input_live());
        assert!(SessionState::Focused.input_live());
        assert!(!SessionState::Stopping.frames_flow());
    }

    #[test]
    fn error_codes_are_unique_lowercase_xr_codes() {
        let errors = [
            XrError::NoLoader,
            XrError::NoRuntime,
            XrError::ExtensionMissing { name: "x".into() },
            XrError::InstanceFailed { reason: "x".into() },
            XrError::SystemUnavailable,
            XrError::ViewConfigUnsupported { views: 4 },
            XrError::GpuInteropUnavailable { reason: "x".into() },
            XrError::SessionExists,
            XrError::NoSwapchainFormat,
            XrError::ActionsFrozen { call: "attach" },
            XrError::ActionUnknown { name: "x".into() },
            XrError::SimulatorOnly { call: "x" },
            XrError::UnknownOption { key: "x".into() },
            XrError::InvalidValue {
                key: "x".into(),
                reason: "y".into(),
            },
            XrError::FormatTooNew {
                what: "xr.ron",
                found: 2,
                supported: 1,
            },
            XrError::RuntimeLost,
            XrError::Unsupported("x"),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for e in &errors {
            let code = e.code();
            assert!(code.starts_with("xr_"), "{code}");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(seen.insert(code), "duplicate {code}");
        }
    }

    #[test]
    fn null_view_config_is_validated_and_moves_the_eyes() {
        let mut xr = running();
        assert_eq!(
            xr.simulate(SimEvent::ViewConfig {
                ipd_m: 0.5,
                fov: Fov::symmetric(0.9, 0.9)
            })
            .expect_err("ipd")
            .code(),
            "xr_invalid_ipd"
        );
        xr.simulate(SimEvent::ViewConfig {
            ipd_m: 0.07,
            fov: Fov::symmetric(0.5, 0.6),
        })
        .expect("valid");
        let t = xr.wait_frame().expect("wait");
        let views = xr.locate_views(t.predicted_display_time_ns).expect("views");
        let gap = views[1].pose.position.x - views[0].pose.position.x;
        assert!((gap - 0.07).abs() < 1e-6, "{gap}");
        assert_eq!(views[0].fov, Fov::symmetric(0.5, 0.6));
    }

    #[test]
    fn null_backend_reports_focus_and_hides_synchronized_frames() {
        let mut xr = running();
        xr.poll_events();
        xr.simulate(SimEvent::Runtime(Transition::RuntimeSynchronized))
            .expect("sync");
        let t = xr.wait_frame().expect("wait");
        assert!(!t.should_render, "synchronized renders nothing");
        xr.begin_frame().expect("begin");
        let views = xr.locate_views(t.predicted_display_time_ns).expect("views");
        xr.end_frame(&FrameSubmit {
            display_time_ns: t.predicted_display_time_ns,
            views,
            rendered: false,
            layers: LayerList::default(),
        })
        .expect("end");
        xr.simulate(SimEvent::Runtime(Transition::RuntimeVisible))
            .expect("visible");
        xr.simulate(SimEvent::Runtime(Transition::RuntimeFocused))
            .expect("focused");
        assert!(xr.wait_frame().expect("wait").should_render);
        xr.simulate(SimEvent::Runtime(Transition::RuntimeVisible))
            .expect("unfocus");
        let events = xr.poll_events();
        assert!(events.contains(&XrEvent::FocusChanged { focused: true }));
        assert_eq!(events.last(), Some(&XrEvent::FocusChanged { focused: false }));
        assert_eq!(
            xr.simulate(SimEvent::Runtime(Transition::Begin)),
            Err(XrError::InvalidTransition {
                from: SessionState::Visible,
                transition: Transition::Begin
            }),
            "app-side transitions are not scripted"
        );
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(SessionState::Idle.name(), "idle");
        assert_eq!(SessionState::Ready.name(), "ready");
        assert_eq!(SessionState::Running.name(), "running");
        assert_eq!(SessionState::Stopping.name(), "stopping");
    }

    // -- frame loop ordering ---------------------------------------------------------------

    fn timing(i: u64) -> FrameTiming {
        FrameTiming {
            frame_index: i,
            predicted_display_time_ns: 1000 * (i as i64 + 1),
            predicted_display_period_ns: 1000,
            should_render: true,
        }
    }

    #[test]
    fn frame_calls_must_follow_wait_begin_end() {
        let mut core = SessionCore::new();
        assert_eq!(
            core.frame_waited(timing(0)).expect_err("idle").code(),
            "xr_not_running"
        );
        core.transition(Transition::RuntimeReady).expect("ready");
        core.transition(Transition::Begin).expect("begin");
        assert_eq!(
            core.frame_begun().expect_err("no wait").code(),
            "xr_frame_out_of_order"
        );
        assert_eq!(
            core.frame_ended(1000).expect_err("no begin").code(),
            "xr_frame_out_of_order"
        );
        core.frame_waited(timing(0)).expect("wait");
        assert_eq!(core.phase(), FramePhase::Waited);
        assert_eq!(
            core.frame_ended(1000).expect_err("not begun").code(),
            "xr_frame_out_of_order"
        );
        core.frame_begun().expect("begin");
        assert_eq!(core.phase(), FramePhase::Begun);
        assert_eq!(
            core.frame_waited(timing(1)).expect_err("mid-frame").code(),
            "xr_frame_out_of_order"
        );
        assert_eq!(
            core.frame_begun().expect_err("twice").code(),
            "xr_frame_out_of_order"
        );
        assert_eq!(
            core.frame_ended(999),
            Err(XrError::DisplayTimeMismatch {
                expected: 1000,
                got: 999
            })
        );
        assert_eq!(
            core.phase(),
            FramePhase::Begun,
            "a mismatch leaves the frame open"
        );
        core.frame_ended(1000).expect("end");
        assert_eq!((core.phase(), core.frames_ended()), (FramePhase::Idle, 1));
        assert!(core.pending_frame().is_none());
    }

    #[test]
    fn a_second_wait_replaces_a_dropped_frame() {
        let mut core = SessionCore::new();
        core.transition(Transition::RuntimeReady).expect("ready");
        core.transition(Transition::Begin).expect("begin");
        core.frame_waited(timing(0)).expect("wait");
        core.frame_waited(timing(1)).expect("re-wait");
        assert_eq!(core.pending_frame(), Some(timing(1)));
    }

    #[test]
    fn the_last_frame_can_end_while_stopping_and_idle_drops_it() {
        let mut core = SessionCore::new();
        core.transition(Transition::RuntimeReady).expect("ready");
        core.transition(Transition::Begin).expect("begin");
        core.frame_waited(timing(0)).expect("wait");
        core.frame_begun().expect("begin frame");
        core.transition(Transition::RequestExit).expect("exit");
        assert_eq!(
            core.frame_waited(timing(1)).expect_err("stopping").code(),
            "xr_not_running"
        );
        core.frame_ended(1000).expect("flush");
        // Ending the session mid-frame drops the frame.
        let mut core = SessionCore::new();
        core.transition(Transition::RuntimeReady).expect("ready");
        core.transition(Transition::Begin).expect("begin");
        core.frame_waited(timing(0)).expect("wait");
        core.transition(Transition::RuntimeStop).expect("stop");
        core.transition(Transition::End).expect("end");
        assert_eq!(core.phase(), FramePhase::Idle);
        assert!(core.pending_frame().is_none());
    }

    // -- null backend ----------------------------------------------------------------------

    #[test]
    fn null_backend_session_events_follow_the_transitions() {
        let mut xr = NullXr::default();
        assert_eq!(xr.name(), "null");
        assert_eq!(
            xr.begin_session().expect_err("idle").code(),
            "xr_invalid_transition"
        );
        xr.simulate(SimEvent::RuntimeReady).expect("ready");
        xr.begin_session().expect("begin");
        xr.request_exit().expect("exit");
        xr.end_session().expect("end");
        assert_eq!(
            xr.poll_events(),
            vec![
                XrEvent::StateChanged {
                    state: SessionState::Ready
                },
                XrEvent::StateChanged {
                    state: SessionState::Running
                },
                XrEvent::StateChanged {
                    state: SessionState::Stopping
                },
                XrEvent::StateChanged {
                    state: SessionState::Idle
                },
            ]
        );
        assert!(xr.poll_events().is_empty(), "drained");
    }

    #[test]
    fn null_frame_times_are_whole_periods() {
        let mut xr = NullXr::new(NullConfig {
            display_period_ns: 10_000_000,
            start_time_ns: 5,
            ..NullConfig::default()
        });
        assert_eq!(xr.wait_frame().expect_err("idle").code(), "xr_not_running");
        xr.simulate(SimEvent::RuntimeReady).expect("ready");
        xr.begin_session().expect("begin");
        for i in 0..3u64 {
            let t = xr.wait_frame().expect("wait");
            assert_eq!(t.frame_index, i);
            assert_eq!(t.predicted_display_time_ns, 5 + 10_000_000 * (i as i64 + 1));
            xr.begin_frame().expect("begin");
            let views = xr.locate_views(t.predicted_display_time_ns).expect("views");
            xr.end_frame(&FrameSubmit {
                display_time_ns: t.predicted_display_time_ns,
                views,
                rendered: true,
                layers: LayerList::default(),
            })
            .expect("end");
        }
        assert_eq!(xr.core().frames_ended(), 3);
        assert_eq!(xr.submitted().len(), 3);
    }

    /// A running session used to retain every submit and haptic pulse. The recorder keeps a
    /// fixed window; [`SessionCore::frames_ended`] still counts every ended frame.
    #[test]
    fn null_recorder_keeps_a_fixed_window_of_submits_and_pulses() {
        let mut xr = running();
        let mut latch = LateLatch::default();
        let frames = (MAX_KEPT_SUBMITS + 8) as u64;
        for i in 0..frames {
            run_frame(&mut xr, &mut latch, |_, _| {}).expect("frame");
            xr.apply_haptic(Hand::Right, 0.25, i as i64).expect("pulse");
        }
        assert_eq!(xr.core().frames_ended(), frames, "every frame still ends");
        assert_eq!(
            xr.submitted().len(),
            MAX_KEPT_SUBMITS,
            "submit history grew to {} (cap {MAX_KEPT_SUBMITS})",
            xr.submitted().len()
        );
        assert_eq!(
            xr.haptics().len(),
            MAX_KEPT_HAPTICS,
            "haptic history grew to {} (cap {MAX_KEPT_HAPTICS})",
            xr.haptics().len()
        );
        let period = NullConfig::default().display_period_ns;
        let oldest = frames - MAX_KEPT_SUBMITS as u64;
        assert_eq!(
            xr.submitted()[0].display_time_ns,
            period * (oldest as i64 + 1),
            "the oldest kept submit is the newest window, not frame 0"
        );
        assert_eq!(xr.haptics()[0].duration_ns, oldest as i64);
        assert_eq!(
            xr.haptics().last().map(|p| p.duration_ns),
            Some((frames - 1) as i64)
        );
    }

    /// Audit ext-b Low: a zero or negative display period (the field is public) must not make
    /// frame times stand still or run backwards while frame indices advance.
    #[test]
    fn null_frame_times_move_forward_for_a_zero_or_negative_period() {
        for period in [0i64, -5_000_000] {
            let mut xr = NullXr::new(NullConfig {
                display_period_ns: period,
                ..NullConfig::default()
            });
            xr.simulate(SimEvent::RuntimeReady).expect("ready");
            xr.begin_session().expect("begin");
            let mut last = i64::MIN;
            for _ in 0..3 {
                let t = xr.wait_frame().expect("wait");
                assert!(t.predicted_display_period_ns > 0, "period {period}");
                assert!(
                    t.predicted_display_time_ns > last,
                    "period {period}: time goes forward"
                );
                last = t.predicted_display_time_ns;
                xr.begin_frame().expect("begin");
                let views = xr.locate_views(t.predicted_display_time_ns).expect("views");
                xr.end_frame(&FrameSubmit {
                    display_time_ns: t.predicted_display_time_ns,
                    views,
                    rendered: true,
                    layers: LayerList::default(),
                })
                .expect("end");
            }
        }
    }

    #[test]
    fn null_end_frame_rejects_a_bad_fov_and_a_wrong_time() {
        let mut xr = running();
        let t = xr.wait_frame().expect("wait");
        xr.begin_frame().expect("begin");
        let mut views = xr.locate_views(t.predicted_display_time_ns).expect("views");
        views[1].fov = Fov::symmetric(3.0, 0.5);
        let bad_fov = FrameSubmit {
            display_time_ns: t.predicted_display_time_ns,
            views,
            rendered: true,
            layers: LayerList::default(),
        };
        assert_eq!(xr.end_frame(&bad_fov).expect_err("fov").code(), "xr_invalid_fov");
        views[1].fov = views[0].fov;
        let wrong_time = FrameSubmit {
            display_time_ns: 1,
            views,
            rendered: true,
            layers: LayerList::default(),
        };
        assert_eq!(
            xr.end_frame(&wrong_time).expect_err("time").code(),
            "xr_display_time_mismatch"
        );
        assert!(xr.submitted().is_empty());
    }

    #[test]
    fn null_locate_and_sync_need_a_running_session() {
        let mut xr = NullXr::default();
        assert_eq!(xr.locate_views(0).expect_err("idle").code(), "xr_not_running");
        assert_eq!(xr.sync_actions().expect_err("idle").code(), "xr_not_running");
    }

    #[test]
    fn null_controllers_update_on_sync_only() {
        let mut xr = running();
        let mut right = ControllerState::inactive(Hand::Right);
        right.active = true;
        right.trigger = 0.75;
        right.primary = true;
        right.thumbstick = Vec2::new(0.5, -0.25);
        xr.set_controller(right);
        assert!(!xr.controller(Hand::Right).active, "not synced yet");
        xr.sync_actions().expect("sync");
        let got = xr.controller(Hand::Right);
        assert!(got.active);
        assert_eq!(got.action(ControllerAction::Trigger), ActionValue::Float(0.75));
        assert_eq!(
            got.action(ControllerAction::PrimaryButton),
            ActionValue::Bool(true)
        );
        assert_eq!(
            got.action(ControllerAction::SecondaryButton),
            ActionValue::Bool(false)
        );
        assert_eq!(
            got.action(ControllerAction::Thumbstick),
            ActionValue::Vec2(Vec2::new(0.5, -0.25))
        );
        assert!(!xr.controller(Hand::Left).active);
        assert_eq!(
            xr.poll_events().last(),
            Some(&XrEvent::ControllerActive {
                hand: Hand::Right,
                active: true
            })
        );
    }

    #[test]
    fn an_inactive_controller_reports_released_inputs() {
        let mut c = ControllerState::inactive(Hand::Left);
        c.trigger = 1.0;
        c.primary = true;
        c.thumbstick = Vec2::ONE;
        for (action, expect) in [
            (ControllerAction::Trigger, ActionValue::Float(0.0)),
            (ControllerAction::Squeeze, ActionValue::Float(0.0)),
            (ControllerAction::Thumbstick, ActionValue::Vec2(Vec2::ZERO)),
            (ControllerAction::PrimaryButton, ActionValue::Bool(false)),
            (ControllerAction::Menu, ActionValue::Bool(false)),
            (ControllerAction::ThumbstickClick, ActionValue::Bool(false)),
        ] {
            assert_eq!(c.action(action), expect);
        }
    }

    /// Trigger and squeeze are documented `0..=1`, the stick `-1..=1`: gameplay's `> 0.5` must never
    /// see a NaN that stays "pressed" or a 2.0, whichever way the value arrived.
    #[test]
    fn controller_inputs_stay_in_range_on_every_path() {
        let mut xr = running();
        let mut bad = ControllerState::inactive(Hand::Left);
        bad.active = true;
        bad.trigger = f32::NAN;
        bad.squeeze = 2.0;
        bad.thumbstick = Vec2::new(f32::INFINITY, f32::NAN);
        for got in [
            {
                xr.set_controller(bad);
                xr.sync_actions().expect("sync");
                xr.controller(Hand::Left)
            },
            // A backend that fills the public fields itself goes through `action` too.
            bad,
        ] {
            assert_eq!(got.action(ControllerAction::Trigger), ActionValue::Float(0.0));
            assert_eq!(got.action(ControllerAction::Squeeze), ActionValue::Float(1.0));
            assert_eq!(
                got.action(ControllerAction::Thumbstick),
                ActionValue::Vec2(Vec2::new(1.0, 0.0))
            );
        }
    }

    #[test]
    fn pose_new_keeps_a_bad_position_out_of_view_matrices() {
        for bad in [Vec3::NAN, Vec3::new(f32::INFINITY, 0.0, 0.0)] {
            assert_eq!(Pose::new(bad, Quat::IDENTITY).position, Vec3::ZERO);
        }
        let good = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(Pose::new(good, Quat::IDENTITY).position, good);
    }

    #[test]
    fn haptics_validate_amplitude() {
        let mut xr = running();
        xr.apply_haptic(Hand::Left, 0.5, 1000).expect("haptic");
        for bad in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert_eq!(
                xr.apply_haptic(Hand::Left, bad, 1000).expect_err("amp").code(),
                "xr_invalid_haptic"
            );
        }
        assert_eq!(xr.haptics().len(), 1);
        assert_eq!(xr.haptics()[0].hand, Hand::Left);
    }

    #[test]
    fn a_non_finite_simulated_head_pose_is_ignored() {
        let mut xr = running();
        let before = xr.locate_views(0).expect("views");
        for position in [Vec3::new(f32::NAN, 1.7, 0.0), Vec3::new(0.0, f32::INFINITY, 0.0)] {
            xr.simulate(SimEvent::HeadPose(Pose::new(position, Quat::IDENTITY)))
                .expect("head");
            let views = xr.locate_views(0).expect("views");
            assert!(views.iter().all(|v| v.pose.position.is_finite()));
            assert_eq!(views[0].pose.position, before[0].pose.position);
        }
    }

    #[test]
    fn simulate_head_pose_moves_the_views() {
        let mut xr = running();
        let before = xr.locate_views(0).expect("views")[0].pose.position;
        xr.simulate(SimEvent::HeadPose(Pose::new(
            Vec3::new(0.0, 2.0, 0.0),
            Quat::IDENTITY,
        )))
        .expect("head");
        let after = xr.locate_views(0).expect("views")[0].pose.position;
        assert!(approx(after.y - before.y, 2.0));
    }

    // -- run_frame and the resource --------------------------------------------------------

    #[test]
    fn run_frame_renders_late_latches_and_submits() {
        let mut xr = running();
        let mut latch = LateLatch::default();
        let mut seen = None;
        let report = run_frame(&mut xr, &mut latch, |timing, views| {
            seen = Some((*timing, *views));
        })
        .expect("frame");
        let (timing, views) = seen.expect("render ran");
        assert_eq!(timing, report.timing);
        assert!(report.rendered);
        // The head did not move, so the submitted views equal the rendered ones.
        for (submitted, rendered) in report.views.iter().zip(views.iter()) {
            assert!((submitted.pose.position - rendered.pose.position).length() < EPS);
        }
        assert_eq!(xr.submitted().len(), 1);
        assert_eq!(
            xr.submitted()[0].display_time_ns,
            timing.predicted_display_time_ns
        );
        assert_eq!(latch.latched(), 1);
    }

    #[test]
    fn run_frame_skips_render_when_asked_not_to_but_still_submits() {
        // A backend wrapper that reports should_render = false.
        struct Hidden(NullXr);
        impl XrBackend for Hidden {
            fn name(&self) -> &'static str {
                "hidden"
            }
            fn state(&self) -> SessionState {
                self.0.state()
            }
            fn poll_events(&mut self) -> Vec<XrEvent> {
                self.0.poll_events()
            }
            fn begin_session(&mut self) -> Result<(), XrError> {
                self.0.begin_session()
            }
            fn request_exit(&mut self) -> Result<(), XrError> {
                self.0.request_exit()
            }
            fn end_session(&mut self) -> Result<(), XrError> {
                self.0.end_session()
            }
            fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
                self.0.wait_frame().map(|t| FrameTiming {
                    should_render: false,
                    ..t
                })
            }
            fn begin_frame(&mut self) -> Result<(), XrError> {
                self.0.begin_frame()
            }
            fn locate_views(&mut self, t: i64) -> Result<[View; 2], XrError> {
                self.0.locate_views(t)
            }
            fn head_sample(&self) -> PoseSample {
                self.0.head_sample()
            }
            fn sync_actions(&mut self) -> Result<(), XrError> {
                self.0.sync_actions()
            }
            fn controller(&self, hand: Hand) -> ControllerState {
                self.0.controller(hand)
            }
            fn apply_haptic(&mut self, h: Hand, a: f32, d: i64) -> Result<(), XrError> {
                self.0.apply_haptic(h, a, d)
            }
            fn end_frame(&mut self, s: &FrameSubmit) -> Result<(), XrError> {
                self.0.end_frame(s)
            }
        }
        let mut hidden = Hidden(running());
        let mut latch = LateLatch::default();
        let mut ran = false;
        let report = run_frame(&mut hidden, &mut latch, |_, _| ran = true).expect("frame");
        assert!(!ran);
        assert!(!report.rendered);
        assert_eq!(latch.latched(), 0);
        assert_eq!(hidden.0.submitted().len(), 1);
        assert!(!hidden.0.submitted()[0].rendered);
        assert_eq!(
            hidden
                .simulate(SimEvent::RuntimeReady)
                .expect_err("default")
                .code(),
            "xr_unsupported"
        );
    }

    #[test]
    fn run_frame_late_latch_follows_a_head_that_moves_during_render() {
        // Move the head inside the render closure through a shared cell: the closure cannot borrow
        // the backend, so a second frame carries the motion instead.
        let mut xr = running();
        let mut latch = LateLatch::default();
        run_frame(&mut xr, &mut latch, |_, _| {}).expect("first");
        xr.set_head(PoseSample::at_rest(
            Pose::new(Vec3::new(0.0, 0.0, -1.0), Quat::IDENTITY),
            0,
        ));
        let report = run_frame(&mut xr, &mut latch, |_, _| {}).expect("second");
        assert!(approx(report.views[0].pose.position.z, -1.0));
        assert_eq!(latch.latched(), 2);
    }

    #[test]
    fn run_frame_on_an_idle_session_fails_cleanly() {
        let mut xr = NullXr::default();
        let mut latch = LateLatch::default();
        let err = run_frame(&mut xr, &mut latch, |_, _| panic!("must not render")).expect_err("idle");
        assert_eq!(err.code(), "xr_not_running");
        assert_eq!(xr.core().phase(), FramePhase::Idle);
    }

    #[test]
    fn run_frame_ends_a_begun_frame_when_locating_fails() {
        // A degenerate fov makes locate_views fail after begin_frame; the frame must not stay open.
        let mut xr = NullXr::new(NullConfig {
            fov: Fov::symmetric(2.0, 2.0),
            ..NullConfig::default()
        });
        xr.simulate(SimEvent::RuntimeReady).expect("ready");
        xr.begin_session().expect("begin");
        let mut latch = LateLatch::default();
        let err = run_frame(&mut xr, &mut latch, |_, _| {}).expect_err("fov");
        assert_eq!(err.code(), "xr_invalid_fov");
        assert_eq!(xr.core().phase(), FramePhase::Idle, "frame was ended");
        assert_eq!(xr.core().frames_ended(), 1);
    }

    #[test]
    fn runtime_resource_counts_frames_and_keeps_events() {
        let mut rt = XrRuntime::null();
        assert!(rt.last_report().is_none());
        assert_eq!(rt.frame(|_, _| {}).expect_err("idle").code(), "xr_not_running");
        assert_eq!(rt.frames(), 0);
        rt.backend_mut().simulate(SimEvent::RuntimeReady).expect("ready");
        rt.backend_mut().begin_session().expect("begin");
        rt.frame(|_, _| {}).expect("frame");
        rt.frame(|_, _| {}).expect("frame");
        assert_eq!(rt.frames(), 2);
        assert_eq!(rt.last_report().map(|r| r.timing.frame_index), Some(1));
        rt.pump();
        assert_eq!(rt.events().len(), 2);
        rt.pump();
        assert_eq!(rt.events().len(), 2, "drained once");
        assert_eq!(rt.latch().latched(), 2);
    }

    #[test]
    fn kept_events_are_capped() {
        let mut xr = NullXr::default();
        for i in 0..(MAX_KEPT_EVENTS + 8) {
            xr.set_controller(ControllerState {
                active: i % 2 == 0,
                ..ControllerState::inactive(Hand::Left)
            });
        }
        // The first set raises nothing (inactive -> inactive); every later one flips.
        assert_eq!(xr.poll_events().len(), MAX_KEPT_EVENTS);
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        let errors = [
            XrError::InvalidFov { reason: "x" },
            XrError::InvalidDepthRange { near: 0.0, far: 0.0 },
            XrError::InvalidIpd { ipd: 0.0 },
            XrError::InvalidTransition {
                from: SessionState::Idle,
                transition: Transition::End,
            },
            XrError::NotRunning {
                call: "x",
                state: SessionState::Idle,
            },
            XrError::FrameOutOfOrder {
                call: "x",
                phase: FramePhase::Idle,
            },
            XrError::DisplayTimeMismatch { expected: 0, got: 1 },
            XrError::InvalidHaptic { amplitude: 2.0 },
            XrError::Unsupported("x"),
        ];
        let mut seen = std::collections::HashSet::new();
        for e in errors {
            let code = e.code();
            assert!(code.starts_with("xr_"));
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
            assert!(seen.insert(code), "duplicate {code}");
            assert!(!e.to_string().is_empty());
        }
    }

    // -- composition layers ----------------------------------------------------------------

    fn layers_for(views: &[View; 2]) -> LayerList {
        let view = |v: &View| ProjectionView {
            pose: v.pose,
            fov: v.fov,
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 32, 32],
            },
            depth: None,
        };
        LayerList {
            projection: Some(ProjectionLayer {
                views: [view(&views[0]), view(&views[1])],
            }),
            quads: Vec::new(),
        }
    }

    #[test]
    fn a_host_that_composes_submits_the_pose_it_rendered_with() {
        let mut xr = running();
        let mut latch = LateLatch::default();
        let report = run_frame_layers(&mut xr, &mut latch, |_, views| layers_for(views)).expect("frame");
        let located = xr.submitted()[0].layers.projection.expect("projection");
        assert_eq!(located.views[0].pose, report.views[0].pose);
        let mut rendered = None;
        let second = run_frame_layers(&mut xr, &mut latch, |_, views| {
            rendered = Some(views[0].pose);
            layers_for(views)
        })
        .expect("frame");
        let layer = xr.submitted()[1].layers.projection.expect("projection");
        assert_eq!(Some(layer.views[0].pose), rendered);
        assert_eq!(xr.submitted()[1].views[0], second.views[0]);
    }

    #[test]
    fn run_frame_submits_no_layers_and_a_bad_layer_is_refused_with_its_name() {
        let mut xr = running();
        let mut latch = LateLatch::default();
        run_frame(&mut xr, &mut latch, |_, _| {}).expect("frame");
        assert!(xr.submitted()[0].layers.is_empty());
        let err = run_frame_layers(&mut xr, &mut latch, |_, views| {
            let mut list = layers_for(views);
            if let Some(p) = list.projection.as_mut() {
                p.views[1].image.rect[3] = 0;
            }
            list
        })
        .expect_err("empty rect");
        assert_eq!(err.code(), "xr_invalid_value");
        assert!(err.to_string().contains("projection view 1"), "{err}");
    }

    #[test]
    fn a_negative_haptic_duration_is_refused() {
        let mut xr = NullXr::default();
        let err = xr.apply_haptic(Hand::Left, 0.5, -1).expect_err("negative");
        assert_eq!(err.code(), "xr_invalid_value");
    }

    #[test]
    fn a_refused_layer_list_still_ends_the_frame() {
        let mut xr = running();
        let mut latch = LateLatch::default();
        run_frame_layers(&mut xr, &mut latch, |_, views| {
            let mut list = layers_for(views);
            if let Some(p) = list.projection.as_mut() {
                p.views[0].image.rect[2] = 0;
            }
            list
        })
        .expect_err("refused");
        run_frame(&mut xr, &mut latch, |_, _| {}).expect("the next frame is accepted");
    }

    /// `frame_layers` used to `clone_from` the derived [`LayerList`] clone, which allocates a
    /// fresh quad `Vec` every frame. A shorter panel list on the next frame must keep the
    /// previous allocation (its capacity), which a fresh `Vec` does not.
    #[test]
    fn composed_frames_reuse_the_quad_list_allocation() {
        let mut rt = XrRuntime::null();
        rt.backend_mut().simulate(SimEvent::RuntimeReady).expect("ready");
        rt.backend_mut().begin_session().expect("begin");
        let quad = |id| QuadLayer {
            id,
            priority: 0,
            space: LayerSpace::Rig,
            pose: Pose::IDENTITY,
            size_m: [0.2, 0.1],
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 8, 8],
            },
        };
        let many = 8u32;
        rt.frame_layers(|_, views| {
            let mut list = layers_for(views);
            list.quads = (0..many).map(quad).collect();
            list
        })
        .expect("many");
        let cap_many = rt.last_layers().quads.capacity();
        assert_eq!(cap_many, 8, "measured quad capacity after eight panels");
        rt.frame_layers(|_, views| {
            let mut list = layers_for(views);
            list.quads = vec![quad(99)];
            list
        })
        .expect("one");
        let cap_one = rt.last_layers().quads.capacity();
        assert_eq!(rt.last_layers().quads.len(), 1);
        assert_eq!(rt.last_layers().quads[0].id, 99);
        assert!(
            rt.last_layers().projection.is_some(),
            "the projection is recorded"
        );
        assert_eq!(
            cap_one, cap_many,
            "quad buffer capacity dropped from {cap_many} to {cap_one} (a new Vec was allocated)"
        );
    }

    /// A layer list the runtime refuses must leave the last accepted panels in place,
    /// buffer included. The copy into that buffer happens only after `validate` succeeds.
    #[test]
    fn a_refused_composed_frame_keeps_the_previous_quads() {
        let mut rt = XrRuntime::null();
        rt.backend_mut().simulate(SimEvent::RuntimeReady).expect("ready");
        rt.backend_mut().begin_session().expect("begin");
        let quad = |id| QuadLayer {
            id,
            priority: 0,
            space: LayerSpace::Rig,
            pose: Pose::IDENTITY,
            size_m: [0.2, 0.1],
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 8, 8],
            },
        };
        rt.frame_layers(|_, views| {
            let mut list = layers_for(views);
            list.quads = vec![quad(1), quad(2)];
            list
        })
        .expect("accepted");
        let cap = rt.last_layers().quads.capacity();
        let err = rt
            .frame_layers(|_, views| {
                let mut list = layers_for(views);
                list.quads = vec![quad(9)];
                if let Some(projection) = list.projection.as_mut() {
                    projection.views[0].image.rect[2] = 0;
                }
                list
            })
            .expect_err("empty rect");
        assert_eq!(err.code(), "xr_invalid_value");
        assert_eq!(rt.last_layers().quads.len(), 2);
        assert_eq!(rt.last_layers().quads[0].id, 1);
        assert!(rt.last_layers().quads.capacity() >= cap);
    }

    /// A frame whose valid list the backend then fails to submit (`end_frame` errors, e.g. a lost
    /// session) must not replace the last accepted panels: `xr.layers` would report layers that
    /// were never shown.
    #[test]
    fn a_failed_submit_keeps_the_previous_quads() {
        struct FailEnd(NullXr, std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl XrBackend for FailEnd {
            fn name(&self) -> &'static str {
                "fail-end"
            }
            fn state(&self) -> SessionState {
                self.0.state()
            }
            fn poll_events(&mut self) -> Vec<XrEvent> {
                self.0.poll_events()
            }
            fn begin_session(&mut self) -> Result<(), XrError> {
                self.0.begin_session()
            }
            fn request_exit(&mut self) -> Result<(), XrError> {
                self.0.request_exit()
            }
            fn end_session(&mut self) -> Result<(), XrError> {
                self.0.end_session()
            }
            fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
                self.0.wait_frame()
            }
            fn begin_frame(&mut self) -> Result<(), XrError> {
                self.0.begin_frame()
            }
            fn locate_views(&mut self, t: i64) -> Result<[View; 2], XrError> {
                self.0.locate_views(t)
            }
            fn head_sample(&self) -> PoseSample {
                self.0.head_sample()
            }
            fn sync_actions(&mut self) -> Result<(), XrError> {
                self.0.sync_actions()
            }
            fn controller(&self, hand: Hand) -> ControllerState {
                self.0.controller(hand)
            }
            fn apply_haptic(&mut self, h: Hand, a: f32, d: i64) -> Result<(), XrError> {
                self.0.apply_haptic(h, a, d)
            }
            fn max_layers(&self) -> usize {
                self.0.max_layers()
            }
            fn end_frame(&mut self, s: &FrameSubmit) -> Result<(), XrError> {
                if self.1.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(XrError::NotRunning {
                        call: "end_frame",
                        state: SessionState::Stopping,
                    });
                }
                self.0.end_frame(s)
            }
        }
        let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut rt = XrRuntime::new(Box::new(FailEnd(running(), fail.clone())));
        let quad = |id| QuadLayer {
            id,
            priority: 0,
            space: LayerSpace::Rig,
            pose: Pose::IDENTITY,
            size_m: [0.2, 0.1],
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 8, 8],
            },
        };
        rt.frame_layers(|_, views| {
            let mut list = layers_for(views);
            list.quads = vec![quad(1), quad(2)];
            list
        })
        .expect("accepted");
        fail.store(true, std::sync::atomic::Ordering::Relaxed);
        rt.frame_layers(|_, views| {
            let mut list = layers_for(views);
            list.quads = vec![quad(9)];
            list
        })
        .expect_err("the backend failed the submit");
        assert_eq!(rt.last_layers().quads.len(), 2);
        assert_eq!(rt.last_layers().quads[0].id, 1);
    }

    #[test]
    fn the_layer_budget_is_the_runtimes() {
        let mut xr = running();
        xr.set_max_layers(0);
        assert_eq!(xr.max_layers(), 1, "the projection layer always fits");
        let quad = |id| QuadLayer {
            id,
            priority: 0,
            space: LayerSpace::Head,
            pose: Pose::IDENTITY,
            size_m: [1.0, 1.0],
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 8, 8],
            },
        };
        let mut latch = LateLatch::default();
        let err = run_frame_layers(&mut xr, &mut latch, |_, views| LayerList {
            quads: vec![quad(1)],
            ..layers_for(views)
        })
        .expect_err("over budget");
        assert_eq!(err.code(), "xr_invalid_value");
    }

    #[test]
    fn the_scripted_lens_raises_an_event_and_follows_the_field_of_view() {
        let mut xr = NullXr::default();
        assert!(xr.visibility_mask(Eye::Left).is_none(), "no mask until scripted");
        assert!(xr.supports_depth_layer());
        xr.simulate(SimEvent::LensMask { radius: 0.9 }).expect("lens");
        assert_eq!(xr.poll_events(), [XrEvent::VisibilityMaskChanged]);
        let mask = xr.visibility_mask(Eye::Right).expect("mask");
        assert!(mask.is_valid());
        let before = mask.vertices[0];
        xr.simulate(SimEvent::ViewConfig {
            ipd_m: DEFAULT_IPD_M,
            fov: Fov::symmetric(0.6, 0.6),
        })
        .expect("view config");
        let after = xr.visibility_mask(Eye::Right).expect("mask");
        assert_ne!(after.vertices[0], before);
        let err = xr
            .simulate(SimEvent::LensMask { radius: 5.0 })
            .expect_err("radius");
        assert_eq!(err.code(), "xr_invalid_value");
        xr.simulate(SimEvent::LensMask { radius: 0.0 }).expect("off");
        assert!(xr.visibility_mask(Eye::Left).is_none());
    }

    #[test]
    fn mono_eyes_sit_at_the_head_and_a_real_ipd_stays_in_range() {
        let mut xr = running();
        let time = xr.wait_frame().expect("wait").predicted_display_time_ns;
        let spread = xr.locate_views(time).expect("views");
        assert_ne!(spread[0].pose, spread[1].pose);
        xr.simulate(SimEvent::MonoEyes(true)).expect("mono");
        let mono = xr.locate_views(time).expect("views");
        assert_eq!(mono[0].pose, mono[1].pose);
        assert_eq!(mono[0].eye, Eye::Left, "the eyes keep their names");
        xr.simulate(SimEvent::MonoEyes(false)).expect("back");
        assert_eq!(xr.locate_views(time).expect("views")[0].pose, spread[0].pose);
    }
}
