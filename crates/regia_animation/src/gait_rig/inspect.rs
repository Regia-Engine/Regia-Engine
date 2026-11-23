// SPDX-License-Identifier: MPL-2.0
//! Read-only views of a [`GaitRig`]'s resolved legs for debug draw and tools (the locomotion
//! gizmos, ADR-8729). The views reflect the last resolve (`step_gait_rigs`); before the first
//! step, or for a leg that did not resolve, they are `None`.

use super::{GaitRig, LegSolver};
use crate::chain_ik::JointLimit;

/// One resolved leg's joint chain, root first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GaitLegChain<'a> {
    /// Three joints: the analytic two-bone solve.
    TwoBone {
        /// Chain root (hip or coxa) joint id.
        root: u32,
        /// Middle (knee) joint id.
        mid: u32,
        /// Tip (foot) joint id.
        tip: u32,
    },
    /// Any other count: FABRIK with per-joint limits.
    Chain {
        /// Joint ids, root to tip.
        joints: &'a [u32],
        /// One limit per joint, or empty for a free chain (the tip's entry is unused).
        limits: &'a [JointLimit],
    },
}

impl GaitLegChain<'_> {
    /// Number of joints in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::TwoBone { .. } => 3,
            Self::Chain { joints, .. } => joints.len(),
        }
    }

    /// Always `false`: a resolved chain has at least two joints.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Joint id `i` of the chain, root first.
    #[must_use]
    pub fn joint(&self, i: usize) -> Option<u32> {
        match *self {
            Self::TwoBone { root, mid, tip } => [root, mid, tip].get(i).copied(),
            Self::Chain { joints, .. } => joints.get(i).copied(),
        }
    }

    /// The limit of chain joint `i` ([`JointLimit::Free`] for two-bone legs and free chains).
    #[must_use]
    pub fn limit(&self, i: usize) -> JointLimit {
        match *self {
            Self::TwoBone { .. } => JointLimit::Free,
            Self::Chain { limits, .. } => limits.get(i).copied().unwrap_or_default(),
        }
    }
}

impl GaitRig {
    /// Leg `leg`'s resolved chain, or `None` before the first resolve or when the leg did not
    /// resolve (`gait_rig_leg_unresolved`).
    #[must_use]
    pub fn leg_chain(&self, leg: usize) -> Option<GaitLegChain<'_>> {
        match self.resolved.get(leg)?.solver.as_ref()? {
            LegSolver::TwoBone(chain) => Some(GaitLegChain::TwoBone {
                root: chain.root,
                mid: chain.mid,
                tip: chain.tip,
            }),
            LegSolver::Chain(chain) => Some(GaitLegChain::Chain {
                joints: &chain.joints,
                limits: &chain.limits,
            }),
        }
    }

    /// Leg `leg`'s bind tip height over the bind floor, skeleton units: the leg's IK target is
    /// its gait foot target raised by this much (times the rig's scale).
    #[must_use]
    pub fn leg_lift(&self, leg: usize) -> Option<f32> {
        let resolved = self.resolved.get(leg)?;
        resolved.solver.as_ref().map(|_| resolved.lift)
    }

    /// The resolved body joint (lifted to the stance height), or `None`.
    #[must_use]
    pub fn body_joint(&self) -> Option<u32> {
        self.body_joint
    }
}
