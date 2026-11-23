// SPDX-License-Identifier: MPL-2.0
//! The simulator script (`*.xrsim.ron`, spec 58 § 4.3), its deterministic player and the pose
//! recorder (record mode, § 7).
//!
//! A script is a list of keyframes at fixed-step ticks. Between keyframes the head and controller
//! poses interpolate (position lerp, orientation slerp) in `f32` with a fixed operation order; at a
//! keyframe tick the keyframe's values are returned unchanged, so a recorded stream (one keyframe
//! per tick) replays bit-exactly. Inputs and session states apply at their tick.

use std::collections::VecDeque;

use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{
    Fov, IPD_RANGE_M, NullConfig, Pose, SessionCore, SessionState, Transition, XrError,
};
use serde::{Deserialize, Serialize};

use crate::input::XrActionValue;

/// The `format_version` this build writes and the newest it reads.
pub const SIM_FORMAT_VERSION: u32 = 1;

/// A pose in a script: position in meters and a quaternion `(x, y, z, w)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptPose {
    /// Position, meters.
    pub position: (f32, f32, f32),
    /// Orientation quaternion `(x, y, z, w)`; normalized on use.
    #[serde(default = "identity_quat")]
    pub orientation: (f32, f32, f32, f32),
}

fn identity_quat() -> (f32, f32, f32, f32) {
    (0.0, 0.0, 0.0, 1.0)
}

impl ScriptPose {
    /// The pose. A unit quaternion is taken bit for bit (so a recording replays exactly); any
    /// other is normalized, and a degenerate one becomes the identity, as `Pose::new` does.
    pub fn to_pose(&self) -> Pose {
        let (x, y, z) = self.position;
        let (qx, qy, qz, qw) = self.orientation;
        let q = Quat::from_xyzw(qx, qy, qz, qw);
        if q.is_finite() && (q.length_squared() - 1.0).abs() <= 1e-6 {
            Pose {
                position: Vec3::new(x, y, z),
                orientation: q,
            }
        } else {
            Pose::new(Vec3::new(x, y, z), q)
        }
    }

    /// The script form of `pose`.
    pub fn from_pose(pose: &Pose) -> ScriptPose {
        let p = pose.position;
        let q = pose.orientation;
        ScriptPose {
            position: (p.x, p.y, p.z),
            orientation: (q.x, q.y, q.z, q.w),
        }
    }

    fn finite(&self) -> bool {
        let (x, y, z) = self.position;
        let (qx, qy, qz, qw) = self.orientation;
        [x, y, z, qx, qy, qz, qw].iter().all(|v| v.is_finite())
    }
}

/// A scripted input value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ScriptValue {
    /// A button.
    Bool(bool),
    /// An axis.
    Float(f32),
    /// A stick `(x, y)`.
    Vec2(f32, f32),
}

impl ScriptValue {
    /// The action value.
    pub fn to_action_value(self) -> XrActionValue {
        match self {
            ScriptValue::Bool(b) => XrActionValue::Bool(b),
            ScriptValue::Float(f) => XrActionValue::Float(f),
            ScriptValue::Vec2(x, y) => XrActionValue::Vec2(regia_core::glam::Vec2::new(x, y)),
        }
    }
}

/// A session state a script (or `xr.drive session_state`) moves the runtime to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScriptState {
    /// The runtime is ready (the plugin then begins the session).
    Ready,
    /// Synchronized: frames flow, nothing is shown.
    Synchronized,
    /// Visible, not focused.
    Visible,
    /// Focused: input is live.
    Focused,
    /// The runtime stops the session.
    Stopping,
    /// The runtime is lost.
    LossPending,
    /// The user leaves the app.
    Exiting,
}

impl ScriptState {
    /// Every state, in order.
    pub const ALL: [ScriptState; 7] = [
        ScriptState::Ready,
        ScriptState::Synchronized,
        ScriptState::Visible,
        ScriptState::Focused,
        ScriptState::Stopping,
        ScriptState::LossPending,
        ScriptState::Exiting,
    ];

    /// The session state.
    pub fn session_state(self) -> SessionState {
        match self {
            ScriptState::Ready => SessionState::Ready,
            ScriptState::Synchronized => SessionState::Synchronized,
            ScriptState::Visible => SessionState::Visible,
            ScriptState::Focused => SessionState::Focused,
            ScriptState::Stopping => SessionState::Stopping,
            ScriptState::LossPending => SessionState::LossPending,
            ScriptState::Exiting => SessionState::Exiting,
        }
    }

    /// The state named `name` (the session state names: `ready`, `synchronized`, `visible`,
    /// `focused`, `stopping`, `loss_pending`, `exiting`).
    pub fn from_name(name: &str) -> Option<ScriptState> {
        ScriptState::ALL
            .into_iter()
            .find(|s| s.session_state().name() == name)
    }
}

/// The transitions that move a session from `from` to `to`, shortest first: the runtime-side
/// transitions plus `Begin` and `End` (which the plugin applies on `Ready` and `Stopping` anyway).
/// `None` when `to` is not reachable (for example anything from `Exiting`).
pub fn path_to_state(from: SessionState, to: SessionState) -> Option<Vec<Transition>> {
    let steps = [
        Transition::RuntimeReady,
        Transition::Begin,
        Transition::End,
        Transition::RuntimeSynchronized,
        Transition::RuntimeVisible,
        Transition::RuntimeFocused,
        Transition::RuntimeStop,
        Transition::RuntimeLost,
        Transition::RuntimeExiting,
    ];
    let mut queue = VecDeque::from([(from, Vec::new())]);
    let mut seen = vec![from];
    while let Some((state, path)) = queue.pop_front() {
        if state == to {
            return Some(path);
        }
        for t in steps {
            let mut core = core_at(state);
            if let Ok(next) = core.transition(t)
                && !seen.contains(&next)
            {
                seen.push(next);
                let mut p = path.clone();
                p.push(t);
                queue.push_back((next, p));
            }
        }
    }
    None
}

/// A session core in `state` (reached through a valid path).
fn core_at(state: SessionState) -> SessionCore {
    use Transition::{
        Begin, RequestExit, RuntimeExiting, RuntimeFocused, RuntimeLost, RuntimeReady, RuntimeSynchronized,
        RuntimeVisible,
    };
    let path: &[Transition] = match state {
        SessionState::Idle => &[],
        SessionState::Ready => &[RuntimeReady],
        SessionState::Running => &[RuntimeReady, Begin],
        SessionState::Stopping => &[RuntimeReady, Begin, RequestExit],
        SessionState::Synchronized => &[RuntimeReady, Begin, RuntimeSynchronized],
        SessionState::Visible => &[RuntimeReady, Begin, RuntimeSynchronized, RuntimeVisible],
        SessionState::Focused => &[
            RuntimeReady,
            Begin,
            RuntimeSynchronized,
            RuntimeVisible,
            RuntimeFocused,
        ],
        SessionState::LossPending => &[RuntimeLost],
        SessionState::Exiting => &[RuntimeExiting],
    };
    let mut core = SessionCore::new();
    for t in path {
        // Each path is valid by construction; an error would leave the core earlier on the path.
        let _ = core.transition(*t);
    }
    core
}

/// One keyframe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimTick {
    /// The fixed-step tick.
    pub tick: u64,
    /// The head pose in tracking space.
    pub head: ScriptPose,
    /// The left controller's pose (grip and aim); `None` is untracked.
    #[serde(default)]
    pub left: Option<ScriptPose>,
    /// The right controller's pose.
    #[serde(default)]
    pub right: Option<ScriptPose>,
    /// Inputs applied at this tick: `(action, sub-path, value)`, held until replaced.
    #[serde(default)]
    pub inputs: Vec<(String, String, ScriptValue)>,
    /// A session state the runtime moves to at this tick.
    #[serde(default)]
    pub state: Option<ScriptState>,
}

/// A simulator script (`*.xrsim.ron`, `format_version` 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XrSimScript {
    /// The file's format version ([`SIM_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Full symmetric field of view, degrees (both axes).
    #[serde(default = "default_fov_deg")]
    pub fov_deg: f32,
    /// Interpupillary distance, meters.
    #[serde(default = "default_ipd")]
    pub ipd_m: f32,
    /// Display refresh rate, Hz.
    #[serde(default = "default_refresh")]
    pub refresh_hz: f32,
    /// Keyframes, ticks strictly increasing.
    pub ticks: Vec<SimTick>,
}

fn default_fov_deg() -> f32 {
    100.0
}
fn default_ipd() -> f32 {
    regia_platform::xr::DEFAULT_IPD_M
}
fn default_refresh() -> f32 {
    90.0
}

fn invalid(key: impl Into<String>, reason: impl Into<String>) -> XrError {
    XrError::InvalidValue {
        key: key.into(),
        reason: reason.into(),
    }
}

impl XrSimScript {
    /// An empty script with the defaults (100 degrees, 63 mm, 90 Hz).
    pub fn new() -> Self {
        XrSimScript {
            format_version: SIM_FORMAT_VERSION,
            fov_deg: default_fov_deg(),
            ipd_m: default_ipd(),
            refresh_hz: default_refresh(),
            ticks: Vec::new(),
        }
    }

    /// Parses and validates a script; a newer `format_version` is [`XrError::FormatTooNew`].
    pub fn from_ron(text: &str) -> Result<XrSimScript, XrError> {
        let version = crate::config::read_format_version(text).map_err(|e| invalid("xrsim.ron", e))?;
        if version > SIM_FORMAT_VERSION {
            return Err(XrError::FormatTooNew {
                what: "xrsim.ron",
                found: version,
                supported: SIM_FORMAT_VERSION,
            });
        }
        let script: XrSimScript = ron::from_str(text).map_err(|e| invalid("xrsim.ron", e.to_string()))?;
        script.validate()?;
        Ok(script)
    }

    /// The canonical RON text.
    pub fn to_ron(&self) -> Result<String, XrError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| invalid("xrsim.ron", e.to_string()))
    }

    /// Checks the version, `fov_deg` 10-170, the IPD range, `refresh_hz` 30-240, finite poses,
    /// finite input values and strictly increasing ticks.
    pub fn validate(&self) -> Result<(), XrError> {
        if self.format_version != SIM_FORMAT_VERSION {
            return Err(invalid("format_version", format!("must be {SIM_FORMAT_VERSION}")));
        }
        if !(self.fov_deg.is_finite() && (10.0..=170.0).contains(&self.fov_deg)) {
            return Err(invalid("fov_deg", "must be 10..=170"));
        }
        let (lo, hi) = IPD_RANGE_M;
        if !(self.ipd_m.is_finite() && (lo..=hi).contains(&self.ipd_m)) {
            return Err(invalid("ipd_m", format!("must be {lo}..={hi}")));
        }
        if !(self.refresh_hz.is_finite() && (30.0..=240.0).contains(&self.refresh_hz)) {
            return Err(invalid("refresh_hz", "must be 30..=240"));
        }
        let mut last: Option<u64> = None;
        for t in &self.ticks {
            if last.is_some_and(|l| t.tick <= l) {
                return Err(invalid(
                    format!("tick {}", t.tick),
                    "ticks must be strictly increasing",
                ));
            }
            last = Some(t.tick);
            let poses = [Some(&t.head), t.left.as_ref(), t.right.as_ref()];
            if poses.iter().flatten().any(|p| !p.finite()) {
                return Err(invalid(format!("tick {}", t.tick), "poses must be finite"));
            }
            if t.inputs.iter().any(|(_, _, v)| !v.to_action_value().is_finite()) {
                return Err(invalid(format!("tick {}", t.tick), "inputs must be finite"));
            }
        }
        Ok(())
    }

    /// The `NullXr` configuration the script implies.
    pub fn null_config(&self) -> NullConfig {
        let half = (self.fov_deg * 0.5).to_radians();
        NullConfig {
            ipd_m: self.ipd_m,
            fov: Fov::symmetric(half, half),
            display_period_ns: (1.0e9 / f64::from(self.refresh_hz)).round() as i64,
            start_time_ns: 0,
        }
    }
}

impl Default for XrSimScript {
    fn default() -> Self {
        Self::new()
    }
}

/// What the script says for one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct SimFrame {
    /// The head pose.
    pub head: Pose,
    /// The controllers' poses, `None` when untracked (left, right).
    pub controllers: [Option<Pose>; 2],
    /// Inputs that apply from this tick.
    pub inputs: Vec<(String, String, XrActionValue)>,
    /// A state the runtime moves to at this tick.
    pub state: Option<ScriptState>,
}

/// Plays a script tick by tick.
#[derive(Debug, Clone, PartialEq)]
pub struct SimPlayer {
    script: XrSimScript,
}

fn interpolate(a: &ScriptPose, b: &ScriptPose, s: f32) -> Pose {
    let pa = a.to_pose();
    let pb = b.to_pose();
    Pose::new(
        pa.position.lerp(pb.position, s),
        pa.orientation.slerp(pb.orientation, s),
    )
}

impl SimPlayer {
    /// A player for a (validated) script.
    pub fn new(script: XrSimScript) -> Self {
        SimPlayer { script }
    }

    /// The script.
    pub fn script(&self) -> &XrSimScript {
        &self.script
    }

    /// The last keyframe's tick, or `None` for an empty script.
    pub fn last_tick(&self) -> Option<u64> {
        self.script.ticks.last().map(|t| t.tick)
    }

    /// The frame at `tick`: interpolated poses between the surrounding keyframes, the keyframe's
    /// own values at a keyframe tick, the last keyframe held after the end, the first before it.
    /// A controller interpolates only when both surrounding keyframes track it; otherwise the
    /// earlier keyframe's value holds. `None` for an empty script.
    pub fn frame(&self, tick: u64) -> Option<SimFrame> {
        let ticks = &self.script.ticks;
        let next = ticks.partition_point(|t| t.tick <= tick);
        let (a, b) = match next {
            0 => (ticks.first()?, None),
            n => (&ticks[n - 1], ticks.get(n)),
        };
        let exact = a.tick == tick;
        let pose_at = |pa: &ScriptPose, pb: Option<&ScriptPose>| -> Pose {
            match (exact, b, pb) {
                (false, Some(kb), Some(pb)) if a.tick < tick => {
                    let s = (tick - a.tick) as f32 / (kb.tick - a.tick) as f32;
                    interpolate(pa, pb, s)
                }
                _ => pa.to_pose(),
            }
        };
        let head = pose_at(&a.head, b.map(|k| &k.head));
        let left = a
            .left
            .as_ref()
            .map(|p| pose_at(p, b.and_then(|k| k.left.as_ref())));
        let right = a
            .right
            .as_ref()
            .map(|p| pose_at(p, b.and_then(|k| k.right.as_ref())));
        let (inputs, state) = if exact {
            (
                a.inputs
                    .iter()
                    .map(|(n, s, v)| (n.clone(), s.clone(), v.to_action_value()))
                    .collect(),
                a.state,
            )
        } else {
            (Vec::new(), None)
        };
        Some(SimFrame {
            head,
            controllers: [left, right],
            inputs,
            state,
        })
    }
}

/// Record mode: writes the sampled pose stream, one keyframe per fixed tick, so a session (real
/// runtime or simulator) can be replayed in the simulator.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimRecorder {
    script: XrSimScript,
}

impl SimRecorder {
    /// A recorder with the given view parameters.
    pub fn new(fov_deg: f32, ipd_m: f32, refresh_hz: f32) -> Self {
        SimRecorder {
            script: XrSimScript {
                fov_deg,
                ipd_m,
                refresh_hz,
                ..XrSimScript::new()
            },
        }
    }

    /// Records one tick. A tick not after the last recorded one replaces nothing and is ignored.
    pub fn record(
        &mut self,
        tick: u64,
        head: &Pose,
        controllers: [Option<Pose>; 2],
        state: Option<ScriptState>,
    ) {
        if self.script.ticks.last().is_some_and(|t| t.tick >= tick) {
            return;
        }
        self.script.ticks.push(SimTick {
            tick,
            head: ScriptPose::from_pose(head),
            left: controllers[0].as_ref().map(ScriptPose::from_pose),
            right: controllers[1].as_ref().map(ScriptPose::from_pose),
            inputs: Vec::new(),
            state,
        });
    }

    /// The recorded script.
    pub fn script(&self) -> &XrSimScript {
        &self.script
    }

    /// Ticks recorded.
    pub fn len(&self) -> usize {
        self.script.ticks.len()
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.script.ticks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"(format_version: 1, fov_deg: 90.0, ipd_m: 0.064, refresh_hz: 72.0,
 ticks: [
   (tick: 0, head: (position: (0.0, 1.6, 0.0)), state: Some(Ready)),
   (tick: 10, head: (position: (1.0, 1.6, 0.0), orientation: (0.0, 0.7071068, 0.0, 0.7071068)),
    right: Some((position: (0.2, 1.2, -0.3))), inputs: [("select", "right", Float(0.75))], state: Some(Focused)),
   (tick: 20, head: (position: (1.0, 1.6, -2.0)), right: Some((position: (0.4, 1.2, -0.3)))),
 ])"#;

    #[test]
    fn a_script_loads_and_round_trips() {
        let s = XrSimScript::from_ron(SCRIPT).expect("loads");
        assert_eq!(s.ticks.len(), 3);
        assert_eq!(s.null_config().display_period_ns, 13_888_889);
        let again = XrSimScript::from_ron(&s.to_ron().expect("ser")).expect("reload");
        assert_eq!(again, s);
    }

    #[test]
    fn bad_scripts_are_rejected() {
        let e = XrSimScript::from_ron(&SCRIPT.replace("tick: 20", "tick: 5")).expect_err("order");
        assert!(e.to_string().contains("strictly"), "{e}");
        let e = XrSimScript::from_ron(&SCRIPT.replace("ipd_m: 0.064", "ipd_m: 0.5")).expect_err("ipd");
        assert!(e.to_string().contains("ipd_m"), "{e}");
        let e =
            XrSimScript::from_ron(&SCRIPT.replace("format_version: 1", "format_version: 2")).expect_err("v2");
        assert_eq!(e.code(), "xr_format_too_new");
        let e = XrSimScript::from_ron(&SCRIPT.replace("tick: 0,", "tick: 0, extra: 1,")).expect_err("field");
        assert!(e.to_string().contains("extra"), "{e}");
    }

    #[test]
    fn poses_interpolate_between_keyframes_and_hold_at_the_ends() {
        let p = SimPlayer::new(XrSimScript::from_ron(SCRIPT).expect("loads"));
        let mid = p.frame(5).expect("frame");
        assert!((mid.head.position - Vec3::new(0.5, 1.6, 0.0)).length() < 1e-6);
        let yaw = crate::space::yaw_of(mid.head.orientation);
        assert!((yaw - 45f32.to_radians()).abs() < 1e-4, "{yaw}");
        assert_eq!(
            mid.controllers[1], None,
            "the right controller appears at tick 10"
        );
        assert!(mid.inputs.is_empty() && mid.state.is_none());
        let key = p.frame(10).expect("frame");
        assert_eq!(key.state, Some(ScriptState::Focused));
        assert_eq!(
            key.inputs,
            vec![("select".into(), "right".into(), XrActionValue::Float(0.75))]
        );
        let r = p.frame(15).expect("frame").controllers[1].expect("tracked");
        assert!((r.position - Vec3::new(0.3, 1.2, -0.3)).length() < 1e-6);
        let after = p.frame(500).expect("frame");
        assert_eq!(after.head.position, Vec3::new(1.0, 1.6, -2.0));
        assert!(after.state.is_none(), "states fire at their tick only");
    }

    #[test]
    fn playback_is_bit_exact_and_a_recording_replays_exactly() {
        let p = SimPlayer::new(XrSimScript::from_ron(SCRIPT).expect("loads"));
        let mut rec = SimRecorder::new(90.0, 0.064, 72.0);
        let mut first = Vec::new();
        for tick in 0..=25 {
            let f = p.frame(tick).expect("frame");
            rec.record(tick, &f.head, f.controllers, None);
            first.push(f);
        }
        for (tick, f) in first.iter().enumerate() {
            let again = p.frame(tick as u64).expect("frame");
            assert_eq!(
                again.head.position.to_array(),
                f.head.position.to_array(),
                "same run, same bits"
            );
        }
        let text = rec.script().to_ron().expect("ser");
        let replay = SimPlayer::new(XrSimScript::from_ron(&text).expect("reload"));
        for (tick, f) in first.iter().enumerate() {
            let r = replay.frame(tick as u64).expect("frame");
            assert_eq!(
                r.head.position.to_array(),
                f.head.position.to_array(),
                "tick {tick}"
            );
            assert_eq!(
                r.head.orientation.to_array(),
                f.head.orientation.to_array(),
                "tick {tick}"
            );
            assert_eq!(
                r.controllers.map(|c| c.map(|p| p.position.to_array())),
                f.controllers.map(|c| c.map(|p| p.position.to_array()))
            );
        }
        assert_eq!(rec.len(), 26);
    }

    #[test]
    fn paths_reach_every_scripted_state() {
        for target in ScriptState::ALL {
            let path = path_to_state(SessionState::Idle, target.session_state()).expect("reachable");
            let mut core = SessionCore::new();
            for t in &path {
                core.transition(*t).expect("valid");
            }
            assert_eq!(core.state(), target.session_state());
            assert_eq!(
                ScriptState::from_name(target.session_state().name()),
                Some(target)
            );
        }
        assert_eq!(
            path_to_state(SessionState::Visible, SessionState::Focused),
            Some(vec![Transition::RuntimeFocused])
        );
        assert_eq!(path_to_state(SessionState::Exiting, SessionState::Focused), None);
    }
}
