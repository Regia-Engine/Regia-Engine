// SPDX-License-Identifier: MPL-2.0
//! Exact-tick joint poses for lag compensation (ADR-2551; spec 75 § 4, spec 90 § 4 (h)).
//!
//! With [`LagCompAnimSource`] installed, the recorder keeps, for each `PoseSource::Joints` entity,
//! the animation *state* of the tick (playback clocks, states, parameters, the step's time; no
//! pose) instead of posed joint shapes. A rewound query that reaches the entity in the broad phase
//! evaluates the joints from that state through the host's side-effect-free pose evaluation, so a
//! bone-attached hit shape sits where the animation had the bone on that exact server tick, not
//! where the last rendered frame left it. `regia_net` has no animation edge: the state is an
//! opaque [`RecordedAnim`] the host builds (`regia::lagcomp_bridge` does, from the `Animator`).

use std::any::Any;
use std::sync::Arc;

use glam::Affine3A;
use regia_ecs::prelude::*;

/// One entity's animation state on one recorded tick, built by the host's [`AnimCaptureFn`].
/// Self-contained (it holds what evaluation needs), so a despawned target still poses.
pub trait RecordedAnim: Send + Sync + std::fmt::Debug + 'static {
    /// The world poses of `joints` for a root at world pose `root` on the recorded tick, in
    /// order (`None` for a joint the rig lacks). `false` when the evaluation failed; the caller
    /// then poses the shapes on the root.
    fn joint_poses(&self, root: &Affine3A, joints: &[&str], out: &mut Vec<Option<Affine3A>>) -> bool;

    /// Whether the rig has a joint of this name.
    fn has_joint(&self, joint: &str) -> bool;

    /// An upper bound, in metres at unit root scale, of any joint's distance from the root: the
    /// recorded box is the root's position grown by it (no pose is evaluated at record time).
    fn reach(&self) -> f32;

    /// Heap bytes this tick's record holds that no other tick shares (the history's memory
    /// budget, spec 75 § 14).
    fn heap_bytes(&self) -> usize;

    /// For the host's next capture: reuse what does not change between ticks.
    fn as_any(&self) -> &dyn Any;
}

/// Captures the animation state of `root` (or the rig under it) for the tick being recorded.
/// `seconds` is one fixed step; `previous` is the entity's record of the previous tick (shared
/// parts may be reused). `Err(code)` when the entity's animation cannot be recorded as state (no
/// rig, a state a snapshot cannot reproduce): the recorder falls back to `LagCompJointSource`
/// and counts the code.
pub type AnimCaptureFn =
    fn(&World, Entity, f32, Option<&Arc<dyn RecordedAnim>>) -> Result<Arc<dyn RecordedAnim>, &'static str>;

/// Host hook for exact-tick joint poses. `None` (the default) keeps the posed-shape history.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LagCompAnimSource(pub Option<AnimCaptureFn>);
