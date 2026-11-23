// SPDX-License-Identifier: MPL-2.0
//! State machine definitions: states (sub-graphs), transitions and conditions.
//!
//! Specification: `docs/specs/17-animation.md` § 3. A state is a sub-graph; a transition is
//! `{ from (or Any), to, conditions, duration, curve, interruptible, blend }`.
//!
//! `blend` picks how the pose moves to the new state: a cross-fade of both states (the
//! default, and the only mode before format v2) or an inertialized transition that evaluates
//! only the target state and decays the pose offset at the switch to zero (see
//! [`crate::inertialize`]).

use serde::{Deserialize, Serialize};

use crate::params::ParameterSet;

/// Numeric comparison for float/int conditions.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CmpOp {
    /// Strictly less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Strictly greater than.
    Gt,
    /// Greater than or equal.
    Ge,
    /// Equal (within 1e-6 for floats).
    Eq,
    /// Not equal.
    Ne,
}

impl CmpOp {
    /// Apply the comparison.
    pub fn test_f32(self, a: f32, b: f32) -> bool {
        match self {
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
            CmpOp::Eq => a == b || (a - b).abs() < 1e-6,
            // The complement of `Eq`, so a NaN operand is "not equal" and `Ne` never holds for
            // equal infinities.
            CmpOp::Ne => !(a == b || (a - b).abs() < 1e-6),
        }
    }

    /// Apply the comparison to integers.
    pub fn test_i32(self, a: i32, b: i32) -> bool {
        match self {
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
        }
    }
}

/// One transition condition.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Condition {
    /// Compare a float parameter.
    Float {
        /// Parameter name.
        param: String,
        /// Comparison.
        op: CmpOp,
        /// Right-hand side.
        value: f32,
    },
    /// Compare an int parameter.
    Int {
        /// Parameter name.
        param: String,
        /// Comparison.
        op: CmpOp,
        /// Right-hand side.
        value: i32,
    },
    /// Require a bool parameter to have a value.
    Bool {
        /// Parameter name.
        param: String,
        /// Required value.
        value: bool,
    },
    /// Require a trigger parameter to be set. Consumed when the transition fires.
    Trigger {
        /// Parameter name.
        param: String,
    },
    /// True when the source state's (non-looping) clip has finished.
    ClipFinished,
    /// True when a notify with this name fired in the source state this frame.
    Notify(String),
}

/// Blend curve of a transition's cross-fade.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransitionCurve {
    /// Linear cross-fade.
    #[default]
    Linear,
    /// Smoothstep cross-fade (ease in and out).
    Smooth,
}

impl TransitionCurve {
    /// Map normalised transition progress `t ∈ 0..=1` to the blend alpha. A NaN progress (a
    /// zero-length blend divided by a zero clock) maps to `0`.
    pub fn alpha(self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match self {
            TransitionCurve::Linear => t,
            TransitionCurve::Smooth => t * t * (3.0 - 2.0 * t),
        }
    }
}

/// How a transition moves the pose from the old state to the new one.
///
/// Format v2 (`docs/specs/17-animation.md` § 3). Files written before v2 have no `blend`
/// field and load as [`TransitionBlend::Crossfade`], which is what they always did.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TransitionBlend {
    /// Evaluate both states for `duration` and cross-fade them with the transition's curve.
    #[default]
    Crossfade,
    /// Inertialize: evaluate only the target state, at full weight, and decay the per-joint
    /// pose offset (and its velocity) at the switch to zero over `duration` with a quintic
    /// polynomial (Bollo, GDC 2018). Overshoot-safe; finishes exactly at `duration`. Costs one
    /// state evaluation instead of two while it runs.
    Inertialize,
    /// Inertialize with a critically damped spring decay of the offset (Holden) instead of the
    /// quintic: keeps the full 3D offset velocity (no overshoot clamp), so velocity is
    /// continuous for every switch; the small remainder left at `duration` (< 0.5 % of the
    /// offset) is dropped.
    InertializeSpring,
}

impl TransitionBlend {
    /// True for the two inertialized modes.
    #[must_use]
    pub fn is_inertial(self) -> bool {
        !matches!(self, TransitionBlend::Crossfade)
    }

    /// The decay used by an inertialized mode (`None` for a cross-fade).
    #[must_use]
    pub fn decay(self) -> Option<crate::inertialize::DecayMethod> {
        match self {
            TransitionBlend::Crossfade => None,
            TransitionBlend::Inertialize => Some(crate::inertialize::DecayMethod::Quintic),
            TransitionBlend::InertializeSpring => Some(crate::inertialize::DecayMethod::Spring),
        }
    }

    /// Lower-case name used by the agent protocol (`crossfade`, `inertialize`,
    /// `inertialize_spring`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionBlend::Crossfade => "crossfade",
            TransitionBlend::Inertialize => "inertialize",
            TransitionBlend::InertializeSpring => "inertialize_spring",
        }
    }

    /// All modes, in UI order.
    pub const ALL: [TransitionBlend; 3] = [
        TransitionBlend::Crossfade,
        TransitionBlend::Inertialize,
        TransitionBlend::InertializeSpring,
    ];
}

fn is_crossfade(blend: &TransitionBlend) -> bool {
    *blend == TransitionBlend::Crossfade
}

/// One transition between states.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TransitionDef {
    /// Source state name, or `None` for "from any state".
    pub from: Option<String>,
    /// Target state name.
    pub to: String,
    /// All must hold for the transition to fire.
    pub conditions: Vec<Condition>,
    /// Blend duration in seconds: the cross-fade length, or the inertial decay time.
    pub duration: f32,
    /// Cross-fade shape (unused by inertialized transitions).
    pub curve: TransitionCurve,
    /// Whether another transition may interrupt this one while it is blending.
    pub interruptible: bool,
    /// Cross-fade or inertialize (format v2; absent = cross-fade, and not written when it is
    /// the default, so a saved graph without inertialized transitions differs from v1 only in
    /// its `format_version`).
    #[serde(default, skip_serializing_if = "is_crossfade")]
    pub blend: TransitionBlend,
}

/// A state machine: named states, each a sub-graph, plus transitions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StateMachineDef {
    /// The state the machine starts in.
    pub initial: String,
    /// Named states with their sub-graph roots.
    pub states: Vec<(String, crate::graph::AnimNode)>,
    /// Transitions.
    pub transitions: Vec<TransitionDef>,
}

impl StateMachineDef {
    /// Index of a state by name.
    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|(n, _)| n == name)
    }

    /// Whether all conditions hold. `clip_finished` and `fired_notifies` describe the source
    /// state's most recent evaluation. Trigger conditions are *not* consumed here; call
    /// `consume_condition_triggers` when a transition actually fires.
    pub fn conditions_hold(
        &self,
        conditions: &[Condition],
        params: &ParameterSet,
        clip_finished: bool,
        fired_notifies: &[String],
    ) -> bool {
        conditions.iter().all(|c| match c {
            Condition::Float { param, op, value } => op.test_f32(params.float(param), *value),
            Condition::Int { param, op, value } => op.test_i32(params.int(param), *value),
            Condition::Bool { param, value } => params.bool(param) == *value,
            Condition::Trigger { param } => params.trigger(param),
            Condition::ClipFinished => clip_finished,
            Condition::Notify(name) => fired_notifies.iter().any(|n| n == name),
        })
    }

    /// Consume the trigger parameters used by these conditions (called when a transition has
    /// fired: triggers auto-reset after being consumed).
    pub fn consume_condition_triggers(conditions: &[Condition], params: &mut ParameterSet) {
        for c in conditions {
            if let Condition::Trigger { param } = c {
                params.consume_trigger(param);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_shapes() {
        assert_eq!(TransitionCurve::Linear.alpha(0.25), 0.25);
        assert_eq!(TransitionCurve::Smooth.alpha(0.0), 0.0);
        assert_eq!(TransitionCurve::Smooth.alpha(1.0), 1.0);
        assert_eq!(TransitionCurve::Smooth.alpha(0.5), 0.5);
        assert!(TransitionCurve::Smooth.alpha(0.25) < 0.25);
        assert!(TransitionCurve::Smooth.alpha(0.75) > 0.75);
        assert_eq!(TransitionCurve::Smooth.alpha(2.0), 1.0, "clamped");
    }

    #[test]
    fn conditions_over_params() {
        let mut params = ParameterSet::new();
        params.set_float("Speed", 2.0);
        params.set_bool("Grounded", true);
        params.set_trigger("Jump");
        let def = StateMachineDef {
            initial: "A".into(),
            states: vec![],
            transitions: vec![],
        };
        let conds = vec![
            Condition::Float {
                param: "Speed".into(),
                op: CmpOp::Ge,
                value: 1.5,
            },
            Condition::Bool {
                param: "Grounded".into(),
                value: true,
            },
            Condition::Trigger { param: "Jump".into() },
        ];
        assert!(def.conditions_hold(&conds, &params, false, &[]));
        assert!(!def.conditions_hold(&[Condition::ClipFinished], &params, false, &[]));
        assert!(def.conditions_hold(&[Condition::ClipFinished], &params, true, &[]));
        assert!(def.conditions_hold(
            &[Condition::Notify("Land".into())],
            &params,
            false,
            &["Land".to_string()]
        ));
        assert!(!def.conditions_hold(
            &[Condition::Notify("Land".into())],
            &params,
            false,
            &["Step".to_string()]
        ));

        // Consume the trigger, and the condition no longer holds.
        StateMachineDef::consume_condition_triggers(&conds, &mut params);
        assert!(!def.conditions_hold(&conds, &params, false, &[]));
    }

    #[test]
    fn every_comparison_operator_over_float_and_int_conditions() {
        let def = StateMachineDef {
            initial: "A".into(),
            states: vec![],
            transitions: vec![],
        };
        // (op, lhs < rhs, lhs == rhs, lhs > rhs) expected results.
        let table = [
            (CmpOp::Lt, true, false, false),
            (CmpOp::Le, true, true, false),
            (CmpOp::Gt, false, false, true),
            (CmpOp::Ge, false, true, true),
            (CmpOp::Eq, false, true, false),
            (CmpOp::Ne, true, false, true),
        ];
        for (op, lt, eq, gt) in table {
            let mut params = ParameterSet::new();
            for (lhs, expect) in [(1.0f32, lt), (2.0, eq), (3.0, gt)] {
                params.set_float("F", lhs);
                params.set_int("I", lhs as i32);
                let float = Condition::Float {
                    param: "F".into(),
                    op,
                    value: 2.0,
                };
                let int = Condition::Int {
                    param: "I".into(),
                    op,
                    value: 2,
                };
                assert_eq!(
                    def.conditions_hold(&[float], &params, false, &[]),
                    expect,
                    "{op:?} f32 {lhs}"
                );
                assert_eq!(
                    def.conditions_hold(&[int], &params, false, &[]),
                    expect,
                    "{op:?} i32 {lhs}"
                );
                assert_eq!(op.test_f32(lhs, 2.0), expect, "{op:?} test_f32 {lhs}");
                assert_eq!(op.test_i32(lhs as i32, 2), expect, "{op:?} test_i32 {lhs}");
            }
        }
        // Floats compare equal within 1e-6: a pair 1e-7 apart is Eq, not Ne; 1e-5 apart is Ne.
        assert!(CmpOp::Eq.test_f32(1.0, 1.0 + 1.0e-7));
        assert!(!CmpOp::Ne.test_f32(1.0, 1.0 + 1.0e-7));
        assert!(!CmpOp::Eq.test_f32(1.0, 1.0 + 1.0e-5));
        assert!(CmpOp::Ne.test_f32(1.0, 1.0 + 1.0e-5));
    }
}
