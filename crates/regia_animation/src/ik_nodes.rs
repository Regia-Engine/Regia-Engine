// SPDX-License-Identifier: MPL-2.0
//! The graph's iterative-IK nodes (`.animgraph.ron` v4, ADR-8475): `ChainIK` and `BodyAim`.
//!
//! Specification: `docs/specs/17-animation.md` § 3. The nodes resolve their joint names once
//! per rig (the cache lives in the rig's [`crate::eval::EvalReuse`], keyed like the two-bone
//! node's by node and skeleton address and checked against the node's names, so an edited node
//! or a reloaded skeleton resolves again) and then solve with
//! [`crate::chain_ik::solve_chain`] and [`crate::aim_ik::solve_body_aim_with`] on kept
//! buffers: a warm rig allocates nothing per frame.
//!
//! Each slot carries the evaluation it was last used in. A slot is never evicted while it is
//! in use: a graph with more IK nodes than `MAX_SLOTS` grows the cache instead of clearing it
//! every frame, and a slot that has not been used for `STALE_AFTER` evaluations (a node of a
//! reloaded or recompiled graph, whose address changed) is the one replaced, so a live node is
//! never resolved again (review-c finding 4, ADR-8735). A chain slot hits only while the parent
//! links of its cached chain are unchanged (a cached failure, while the names still do not form
//! a chain), so an in-place skeleton edit resolves again (ADR-8803).
//!
//! Provenance (`docs/provenance.md`): the solvers are those of `chain_ik` and `aim_ik`; the
//! name-resolution cache is the two-bone node's pattern. No new solve math here.

use regia_core::math::{Mat4, Vec3};

use crate::aim_ik::{AimChain, AimScratch, solve_body_aim_with};
use crate::chain_ik::{ChainError, ChainIkReport, ChainIkScratch, ChainIkSettings, JointChain, solve_chain};
use crate::eval::{EvalError, first_warning};
use crate::graph::{BodyAimNode, ChainIKNode, ChainLimits};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Cached chains (and, separately, aim stacks) one rig keeps before stale slots are dropped.
/// Keys are addresses, so a reloaded graph leaves stale entries; past this count they are
/// replaced or dropped once stale. Slots in use are never dropped, so a graph with more nodes
/// than this keeps one slot per node.
const MAX_SLOTS: usize = 64;

/// Evaluations a slot may go unused before it is stale (replaceable). More than one, so two
/// graphs evaluated in turn on one rig keep each other's slots.
const STALE_AFTER: u64 = 4;

/// A cache entry stamped with the evaluation that last used it (also the two-bone and look-at
/// joint caches of [`crate::eval::EvalReuse`]).
pub(crate) trait Stamped {
    /// Node and skeleton address.
    fn key(&self) -> (usize, usize);
    /// The evaluation that last used the entry.
    fn used(&self) -> u64;
}

/// True when a slot last used in evaluation `used` is stale at evaluation `now`.
fn is_stale(used: u64, now: u64) -> bool {
    now.saturating_sub(used) > STALE_AFTER
}

/// Store `slot` and return its index: over the slot of the same node and skeleton, else over
/// the least recently used stale slot once the list holds `MAX_SLOTS`, else at the end.
/// A live slot is never replaced by another node's.
pub(crate) fn store<S: Stamped>(list: &mut Vec<S>, slot: S, now: u64) -> usize {
    let key = slot.key();
    let at = list.iter().position(|s| s.key() == key).or_else(|| {
        if list.len() < MAX_SLOTS {
            return None;
        }
        list.iter()
            .enumerate()
            .filter(|(_, s)| is_stale(s.used(), now))
            .min_by_key(|(_, s)| s.used())
            .map(|(i, _)| i)
    });
    match at {
        Some(i) => {
            list[i] = slot;
            i
        }
        None => {
            list.push(slot);
            list.len() - 1
        }
    }
}

/// Drop stale slots while the list holds more than `MAX_SLOTS` (after a large graph was
/// replaced). Never allocates.
pub(crate) fn prune<S: Stamped>(list: &mut Vec<S>, now: u64) {
    if list.len() > MAX_SLOTS {
        list.retain(|s| !is_stale(s.used(), now));
    }
}

#[derive(Clone, Debug)]
struct ChainSlot {
    node: usize,
    skeleton: usize,
    /// The evaluation that last used this slot.
    used: u64,
    root: String,
    tip: String,
    limits: Option<ChainLimits>,
    /// The joint ids `root` and `tip` resolved to (kept for a chain that failed too, so a
    /// parent edit that makes it valid is seen without a name lookup).
    ends: (u32, u32),
    /// Empty when the names do not form a chain (warned once; the node passes the pose).
    chain: JointChain,
}

/// True when `JointChain::between(skeleton, root, tip)` would still give `slot`'s chain (or
/// still fail, for an empty one), checked without allocating or looking up names: the cached
/// root and tip ids still carry their names and every parent link between them is unchanged.
/// An in-place parent edit therefore resolves again instead of keeping a list the solver
/// rejects (verification finding 1, ADR-8803). Links are read as `between` reads them (the raw
/// parent list), so a chain `between` accepts and the solver refuses is still a hit and does
/// not resolve every frame.
fn chain_slot_current(skeleton: &Skeleton, slot: &ChainSlot, root: &str, tip: &str) -> bool {
    let (root_id, tip_id) = slot.ends;
    if !joint_named(skeleton, root_id, root) || !joint_named(skeleton, tip_id, tip) {
        return false;
    }
    let joints = &slot.chain.joints;
    if joints.is_empty() {
        // A cached failure stays one only while the names still do not form a chain.
        return !forms_chain(skeleton, root_id, tip_id);
    }
    joints.first() == Some(&root_id)
        && joints.last() == Some(&tip_id)
        && joints
            .windows(2)
            .all(|w| skeleton.parents.get(w[1] as usize).copied().flatten() == Some(w[0]))
}

/// True when `JointChain::between` would succeed for these ids: the parent walk up from `tip`
/// reaches `root` (bounded by the joint count, like `between`, so a parent cycle ends) and the
/// chain has at least two joints. Allocates nothing.
fn forms_chain(skeleton: &Skeleton, root: u32, tip: u32) -> bool {
    let mut len = 1usize;
    let mut current = tip;
    while current != root {
        if len > skeleton.parents.len() {
            return false;
        }
        match skeleton.parents.get(current as usize).copied().flatten() {
            Some(parent) => {
                len += 1;
                current = parent;
            }
            None => return false,
        }
    }
    len >= 2
}

impl Stamped for ChainSlot {
    fn key(&self) -> (usize, usize) {
        (self.node, self.skeleton)
    }
    fn used(&self) -> u64 {
        self.used
    }
}

#[derive(Clone, Debug)]
struct AimSlot {
    node: usize,
    skeleton: usize,
    /// The evaluation that last used this slot.
    used: u64,
    joints: Vec<(String, f32)>,
    aim_joint: String,
    limits: Option<ChainLimits>,
    aim: AimChain,
}

impl Stamped for AimSlot {
    fn key(&self) -> (usize, usize) {
        (self.node, self.skeleton)
    }
    fn used(&self) -> u64 {
        self.used
    }
}

/// Resolved chains and aim stacks of one rig's `ChainIK` and `BodyAim` nodes, plus the solver
/// buffers they share (one node solves at a time).
#[derive(Clone, Debug, Default)]
pub(crate) struct IkNodeCache {
    chains: Vec<ChainSlot>,
    aims: Vec<AimSlot>,
    chain_scratch: ChainIkScratch,
    aim_scratch: AimScratch,
    fk: Vec<Mat4>,
    /// The last chain node's solve report.
    last_chain: ChainIkReport,
    /// The last aim node's residual angle, radians.
    last_aim_angle: f32,
    /// Evaluations begun on this rig ([`IkNodeCache::begin_evaluation`]); slots are stamped
    /// with it.
    now: u64,
    /// Chains resolved by name (cache misses), for the slot tests.
    #[cfg(test)]
    chain_resolves: u64,
    /// Index in `chains` of the last chain node that solved.
    last_chain_slot: usize,
}

/// What the last `ChainIK` and `BodyAim` nodes of one evaluation left (for reports such as
/// `anim.ik_report`). Zero until a node of that kind solved on the rig.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IkNodeResiduals {
    /// Distance from the last chain node's (unblended) solved tip to its target, meters.
    pub chain_error: f32,
    /// Iterations the last chain node used.
    pub chain_iterations: u32,
    /// Angle between the last body aim node's axis and its target direction, radians.
    pub aim_angle: f32,
}

fn joint_named(skeleton: &Skeleton, joint: u32, name: &str) -> bool {
    skeleton
        .joint_names
        .get(joint as usize)
        .is_some_and(|n| n == name)
}

fn limits_for(
    skeleton: &Skeleton,
    joints: &[u32],
    limits: Option<&ChainLimits>,
    node: &str,
) -> Vec<crate::chain_ik::JointLimit> {
    let Some(limits) = limits else {
        return Vec::new();
    };
    if let ChainLimits::PerJoint(list) = limits {
        for entry in list {
            let on_chain = joints.iter().any(|&j| joint_named(skeleton, j, &entry.joint));
            if !on_chain && first_warning(format!("ik limit {node} {}", entry.joint)) {
                tracing::warn!(
                    target: "regia_animation",
                    "animgraph_limit_joint_not_in_chain: the {node} node limits {:?}, which it does not turn; the limit is ignored",
                    entry.joint
                );
            }
        }
    }
    joints
        .iter()
        .map(|&j| {
            skeleton
                .joint_names
                .get(j as usize)
                .map_or(crate::chain_ik::JointLimit::Free, |name| limits.limit_of(name))
        })
        .collect()
}

impl IkNodeCache {
    /// Start one evaluation of the rig's graph: advance the clock slots are stamped with and
    /// drop stale slots past `MAX_SLOTS`.
    pub(crate) fn begin_evaluation(&mut self) {
        self.now = self.now.wrapping_add(1);
        prune(&mut self.chains, self.now);
        prune(&mut self.aims, self.now);
    }

    /// The current evaluation (what slots used now are stamped with).
    pub(crate) fn now(&self) -> u64 {
        self.now
    }

    /// Cached chain and aim slots (tests: the cache stays bounded).
    #[cfg(test)]
    pub(crate) fn slot_counts(&self) -> (usize, usize) {
        (self.chains.len(), self.aims.len())
    }

    /// The residuals of the last solves.
    pub(crate) fn residuals(&self) -> IkNodeResiduals {
        IkNodeResiduals {
            chain_error: self.last_chain.error,
            chain_iterations: self.last_chain.iterations,
            aim_angle: self.last_aim_angle,
        }
    }

    fn chain_slot(&mut self, def: &ChainIKNode, skeleton: &Skeleton) -> Result<usize, EvalError> {
        let node = std::ptr::from_ref(def) as usize;
        let key = std::ptr::from_ref(skeleton) as usize;
        let now = self.now;
        let found = self.chains.iter().position(|slot| {
            slot.node == node
                && slot.skeleton == key
                && slot.root == def.root
                && slot.tip == def.tip
                && slot.limits == def.limits
                && chain_slot_current(skeleton, slot, &def.root, &def.tip)
        });
        if let Some(index) = found {
            self.chains[index].used = now;
            return Ok(index);
        }
        #[cfg(test)]
        {
            self.chain_resolves += 1;
        }
        let (ends, chain) = match JointChain::between(skeleton, &def.root, &def.tip) {
            Ok(mut chain) => {
                chain.limits = limits_for(skeleton, &chain.joints, def.limits.as_ref(), "chain IK");
                let ends = (
                    chain.joints.first().copied().unwrap_or(u32::MAX),
                    chain.joints.last().copied().unwrap_or(u32::MAX),
                );
                (ends, chain)
            }
            Err(ChainError::MissingJoint(name)) => return Err(EvalError::MissingJoint(name)),
            Err(error) => {
                if first_warning(format!("chain ik {} {}", def.root, def.tip)) {
                    tracing::warn!(
                        target: "regia_animation",
                        "animgraph_chain_invalid: chain IK {:?} to {:?}: {error}; the node leaves the pose unchanged",
                        def.root,
                        def.tip
                    );
                }
                // `between` found both names before it failed; an id it could not find would
                // only make the slot miss and resolve again.
                let ends = (
                    skeleton.joint_index(&def.root).unwrap_or(u32::MAX),
                    skeleton.joint_index(&def.tip).unwrap_or(u32::MAX),
                );
                (ends, JointChain::default())
            }
        };
        Ok(store(
            &mut self.chains,
            ChainSlot {
                node,
                skeleton: key,
                used: now,
                root: def.root.clone(),
                tip: def.tip.clone(),
                limits: def.limits.clone(),
                ends,
                chain,
            },
            now,
        ))
    }

    /// Solve a `ChainIK` node on `pose` (`target` and `pole` already resolved, `weight` in
    /// `0..=1`). `Ok(false)` when the node's names do not form a chain (the pose is left
    /// alone).
    pub(crate) fn solve_chain_node(
        &mut self,
        def: &ChainIKNode,
        skeleton: &Skeleton,
        pose: &mut Pose,
        target: Vec3,
        pole: Option<Vec3>,
        weight: f32,
    ) -> Result<bool, EvalError> {
        let index = self.chain_slot(def, skeleton)?;
        let IkNodeCache {
            chains,
            chain_scratch,
            last_chain,
            last_chain_slot,
            ..
        } = self;
        let chain = &chains[index].chain;
        if chain.joints.is_empty() {
            return Ok(false);
        }
        *last_chain_slot = index;
        let _span = tracing::trace_span!("anim_chain_ik", joints = chain.joints.len()).entered();
        *last_chain = solve_chain(
            skeleton,
            pose,
            chain,
            target,
            pole,
            def.solver,
            ChainIkSettings {
                iterations: def.iterations.min(crate::graph::MAX_CHAIN_IK_ITERATIONS),
                tolerance: def.tolerance,
                weight,
            },
            chain_scratch,
        );
        Ok(true)
    }

    /// IK debug capture of the chain node that just solved (`anim.ik_report`): its reported
    /// residual and iterations, and whether `target` lies past the chain's total length from
    /// its root on `pose`. Only called while the rig's IK debug flag is on.
    pub(crate) fn chain_record(
        &self,
        skeleton: &Skeleton,
        pose: &Pose,
        target: Vec3,
        weight: f32,
        node: usize,
    ) -> crate::debug::IkRecord {
        let joints = self
            .chains
            .get(self.last_chain_slot)
            .map_or(&[][..], |slot| slot.chain.joints.as_slice());
        let mut length = 0.0_f32;
        let mut previous: Option<Vec3> = None;
        for &joint in joints {
            let point = crate::ik::joint_object_point(skeleton, pose, joint);
            if let Some(p) = previous {
                length += (point - p).length();
            }
            previous = Some(point);
        }
        let root = joints
            .first()
            .map_or(Vec3::ZERO, |&j| crate::ik::joint_object_point(skeleton, pose, j));
        crate::debug::IkRecord {
            node,
            kind: crate::debug::IkNodeKind::ChainIk,
            residual: self.last_chain.error,
            iterations: self.last_chain.iterations,
            clamped: (target - root).length() > length + 1e-5,
            weight,
        }
    }

    /// IK debug capture of the aim node that just solved: its residual angle, the passes it
    /// ran, and whether its joint limits left more than 1 mrad.
    pub(crate) fn aim_record(&self, def: &BodyAimNode, weight: f32, node: usize) -> crate::debug::IkRecord {
        crate::debug::IkRecord {
            node,
            kind: crate::debug::IkNodeKind::BodyAim,
            residual: self.last_aim_angle,
            iterations: def.passes.clamp(1, crate::graph::MAX_BODY_AIM_PASSES),
            clamped: def.limits.is_some() && self.last_aim_angle > 1e-3,
            weight,
        }
    }

    fn aim_slot(&mut self, def: &BodyAimNode, skeleton: &Skeleton) -> Result<usize, EvalError> {
        let node = std::ptr::from_ref(def) as usize;
        let key = std::ptr::from_ref(skeleton) as usize;
        let now = self.now;
        let found = self.aims.iter().position(|slot| {
            slot.node == node
                && slot.skeleton == key
                && slot.joints == def.joints
                && slot.aim_joint == def.aim_joint
                && slot.limits == def.limits
                && joint_named(skeleton, slot.aim.aim_joint, &def.aim_joint)
                && slot
                    .aim
                    .joints
                    .iter()
                    .zip(&def.joints)
                    .all(|(&j, (name, _))| joint_named(skeleton, j, name))
        });
        if let Some(index) = found {
            self.aims[index].used = now;
            return Ok(index);
        }
        let mut joints = Vec::with_capacity(def.joints.len());
        for (name, _) in &def.joints {
            joints.push(
                skeleton
                    .joint_index(name)
                    .ok_or_else(|| EvalError::MissingJoint(name.clone()))?,
            );
        }
        let aim_joint = skeleton
            .joint_index(&def.aim_joint)
            .ok_or_else(|| EvalError::MissingJoint(def.aim_joint.clone()))?;
        let last = joints.last().copied().unwrap_or(aim_joint);
        let mut cursor = Some(aim_joint as usize);
        let mut under = false;
        while let Some(j) = cursor {
            if j == last as usize || joints.contains(&(j as u32)) {
                under = true;
                break;
            }
            cursor = skeleton.parent_of(j);
        }
        if !under && first_warning(format!("body aim {}", def.aim_joint)) {
            tracing::warn!(
                target: "regia_animation",
                "animgraph_aim_joint_not_under_chain: body aim joint {:?} is not one of the turning joints or below the last; the turn will not reach it",
                def.aim_joint
            );
        }
        let limits = limits_for(skeleton, &joints, def.limits.as_ref(), "body aim");
        let aim = AimChain {
            shares: def.joints.iter().map(|(_, share)| *share).collect(),
            joints,
            limits,
            aim_joint,
            aim_axis: def.axis,
        };
        Ok(store(
            &mut self.aims,
            AimSlot {
                node,
                skeleton: key,
                used: now,
                joints: def.joints.clone(),
                aim_joint: def.aim_joint.clone(),
                limits: def.limits.clone(),
                aim,
            },
            now,
        ))
    }

    /// Solve a `BodyAim` node on `pose`.
    pub(crate) fn solve_aim_node(
        &mut self,
        def: &BodyAimNode,
        skeleton: &Skeleton,
        pose: &mut Pose,
        target: Vec3,
        weight: f32,
    ) -> Result<(), EvalError> {
        let index = self.aim_slot(def, skeleton)?;
        let IkNodeCache {
            aims,
            aim_scratch,
            fk,
            last_aim_angle,
            ..
        } = self;
        let aim = &mut aims[index].aim;
        aim.aim_axis = def.axis;
        let _span = tracing::trace_span!("anim_body_aim", joints = aim.joints.len()).entered();
        let report = solve_body_aim_with(
            skeleton,
            pose,
            aim,
            target,
            &[],
            weight,
            def.passes.clamp(1, crate::graph::MAX_BODY_AIM_PASSES),
            fk,
            aim_scratch,
        );
        *last_aim_angle = report.angle_error;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{IkNodeCache, MAX_SLOTS, STALE_AFTER};
    use crate::chain_ik::ChainSolver;
    use crate::graph::{AnimNode, BodyAimNode, ChainIKNode, ClipNode, IkTarget};
    use crate::pose::translated;
    use crate::skeleton::Skeleton;
    use regia_core::math::Vec3;

    // These drive the cache directly, not through `evaluate_graph`: its tracing span would add
    // to the lib binary's parallel-subscriber races (`eval::tests::evaluate_graph_enters_a_span`).

    /// A column of four joints.
    fn column() -> Skeleton {
        let names = (0..4).map(|i| format!("j{i}")).collect();
        let parents = (0..4u32).map(|i| i.checked_sub(1)).collect();
        let binds = (0..4)
            .map(|i| translated(if i == 0 { Vec3::ZERO } else { Vec3::Y * 0.3 }))
            .collect();
        Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
    }

    fn clip() -> Box<AnimNode> {
        Box::new(AnimNode::Clip(ClipNode {
            clip: "idle".into(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        }))
    }

    /// One graph's IK nodes: `n` chain nodes and `n` aim nodes, each at its own address.
    struct Nodes {
        chains: Vec<ChainIKNode>,
        aims: Vec<BodyAimNode>,
    }

    fn nodes(n: usize) -> Nodes {
        Nodes {
            chains: (0..n)
                .map(|_| ChainIKNode {
                    inner: clip(),
                    root: "j0".into(),
                    tip: "j3".into(),
                    solver: ChainSolver::Ccd,
                    target: IkTarget::Param("T".into()),
                    pole: None,
                    limits: None,
                    iterations: 2,
                    tolerance: 1e-3,
                    weight_param: None,
                })
                .collect(),
            aims: (0..n)
                .map(|_| BodyAimNode {
                    inner: clip(),
                    joints: vec![("j1".into(), 1.0), ("j2".into(), 1.0)],
                    aim_joint: "j3".into(),
                    axis: Vec3::Y,
                    target: IkTarget::Param("T".into()),
                    limits: None,
                    passes: 1,
                    weight_param: None,
                })
                .collect(),
        }
    }

    /// `evaluations` evaluations that resolve every node of `graph`.
    fn evaluate(cache: &mut IkNodeCache, skeleton: &Skeleton, graph: &Nodes, evaluations: usize) {
        for _ in 0..evaluations {
            cache.begin_evaluation();
            for def in &graph.chains {
                cache.chain_slot(def, skeleton).expect("chain");
            }
            for def in &graph.aims {
                cache.aim_slot(def, skeleton).expect("aim");
            }
        }
    }

    /// Reloads leave stale slots; the cache never holds more than `MAX_SLOTS` of each kind for
    /// graphs that fit, however many reloads (every copy is kept alive, so no address repeats).
    #[test]
    fn reloads_keep_the_cache_bounded() {
        let skeleton = column();
        let mut cache = IkNodeCache::default();
        let copies: Vec<Nodes> = (0..10).map(|_| nodes(40)).collect();
        for graph in &copies {
            evaluate(&mut cache, &skeleton, graph, STALE_AFTER as usize + 2);
            let (chains, aims) = cache.slot_counts();
            assert!(
                chains <= MAX_SLOTS && aims <= MAX_SLOTS,
                "{chains} chains, {aims} aims"
            );
            assert!(chains >= 40 && aims >= 40, "the live graph's slots are cached");
        }
    }

    /// A graph larger than `MAX_SLOTS` keeps one slot per node and resolves nothing once warm;
    /// once it is replaced by a small graph, its slots go stale and are dropped back to
    /// `MAX_SLOTS`.
    #[test]
    fn a_large_graph_grows_the_cache_and_its_replacement_shrinks_it() {
        let skeleton = column();
        let mut cache = IkNodeCache::default();
        let large = nodes(MAX_SLOTS + 1);
        evaluate(&mut cache, &skeleton, &large, 1);
        assert_eq!(cache.slot_counts(), (MAX_SLOTS + 1, MAX_SLOTS + 1));
        // Warm: the same slots serve every node again, nothing is replaced or added.
        let warm: Vec<usize> = large
            .chains
            .iter()
            .map(|def| cache.chain_slot(def, &skeleton).expect("chain"))
            .collect();
        evaluate(&mut cache, &skeleton, &large, 2);
        let again: Vec<usize> = large
            .chains
            .iter()
            .map(|def| cache.chain_slot(def, &skeleton).expect("chain"))
            .collect();
        assert_eq!(warm, again, "live slots keep their places");
        assert_eq!(cache.slot_counts(), (MAX_SLOTS + 1, MAX_SLOTS + 1));
        let small = nodes(1);
        evaluate(&mut cache, &skeleton, &small, STALE_AFTER as usize + 2);
        let (chains, aims) = cache.slot_counts();
        assert!(
            chains <= MAX_SLOTS && aims <= MAX_SLOTS,
            "{chains} chains, {aims} aims"
        );
    }

    /// A chain node from `root` to `tip`.
    fn chain_def(root: &str, tip: &str) -> ChainIKNode {
        let mut def = nodes(1).chains.remove(0);
        def.root = root.into();
        def.tip = tip.into();
        def
    }

    /// Chains `chain_slot` resolved for this call: 0 on a hit, 1 when it resolved again (a
    /// per-cache count, so tests running in parallel do not disturb it).
    fn resolves(cache: &mut IkNodeCache, def: &ChainIKNode, skeleton: &Skeleton) -> u64 {
        let before = cache.chain_resolves;
        cache.chain_slot(def, skeleton).expect("chain");
        cache.chain_resolves - before
    }

    /// An in-place parent edit makes the warm slot resolve again, to the new chain, and the
    /// slot after that is warm again (verification finding 1, ADR-8803).
    #[test]
    fn a_parent_edit_resolves_the_chain_again() {
        let mut skeleton = column();
        let def = chain_def("j0", "j3");
        let mut cache = IkNodeCache::default();
        cache.begin_evaluation();
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert_eq!(cache.chains[index].chain.joints, [0, 1, 2, 3]);
        assert_eq!(resolves(&mut cache, &def, &skeleton), 0, "warm slot");
        skeleton.parents[3] = Some(1);
        assert_eq!(
            resolves(&mut cache, &def, &skeleton),
            1,
            "a parent edit resolves again"
        );
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert_eq!(cache.chains[index].chain.joints, [0, 1, 3]);
        assert_eq!(resolves(&mut cache, &def, &skeleton), 0, "warm again");
        assert_eq!(cache.slot_counts().0, 1, "the node keeps one slot");
    }

    /// Names that do not form a chain stay a cached failure while they still fail (no lookup per
    /// evaluation), and resolve once a parent edit joins them.
    #[test]
    fn a_failed_chain_stays_cached_until_a_parent_edit_joins_it() {
        let mut skeleton = column();
        skeleton.parents[2] = None;
        let def = chain_def("j0", "j3");
        let mut cache = IkNodeCache::default();
        cache.begin_evaluation();
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert!(cache.chains[index].chain.joints.is_empty(), "j3 is not under j0");
        assert_eq!(resolves(&mut cache, &def, &skeleton), 0, "cached failure");
        // A one-joint chain (root and tip the same joint) is cached the same way.
        let single = chain_def("j1", "j1");
        cache.chain_slot(&single, &skeleton).expect("chain");
        assert_eq!(
            resolves(&mut cache, &single, &skeleton),
            0,
            "cached too-short chain"
        );
        skeleton.parents[2] = Some(1);
        assert_eq!(
            resolves(&mut cache, &def, &skeleton),
            1,
            "the edit joins the chain"
        );
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert_eq!(cache.chains[index].chain.joints, [0, 1, 2, 3]);
        assert_eq!(resolves(&mut cache, &single, &skeleton), 0, "still too short");
    }

    /// A parent cycle (a skeleton that skipped validation) is a cached failure, not a hang.
    #[test]
    fn a_parent_cycle_stays_a_cached_failure() {
        let mut skeleton = column();
        let def = chain_def("j0", "j3");
        let mut cache = IkNodeCache::default();
        cache.begin_evaluation();
        cache.chain_slot(&def, &skeleton).expect("chain");
        skeleton.parents[1] = Some(3);
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert!(cache.chains[index].chain.joints.is_empty(), "j0 is off the cycle");
        assert_eq!(resolves(&mut cache, &def, &skeleton), 0, "cached failure");
    }

    /// A chain `JointChain::between` builds but the solver refuses (a parent listed after its
    /// child) is still a hit: the node does not resolve every evaluation.
    #[test]
    fn a_chain_the_solver_refuses_is_still_warm() {
        let mut skeleton = column();
        skeleton.parents[0] = Some(1);
        skeleton.parents[1] = None;
        let def = chain_def("j1", "j0");
        let mut cache = IkNodeCache::default();
        cache.begin_evaluation();
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert_eq!(cache.chains[index].chain.joints, [1, 0]);
        assert_eq!(resolves(&mut cache, &def, &skeleton), 0, "warm slot");
    }

    /// A rename that moves the root's name to another joint resolves again.
    #[test]
    fn a_renamed_root_resolves_the_chain_again() {
        let mut skeleton = column();
        let def = chain_def("j0", "j3");
        let mut cache = IkNodeCache::default();
        cache.begin_evaluation();
        cache.chain_slot(&def, &skeleton).expect("chain");
        skeleton.joint_names.swap(0, 1);
        assert_eq!(resolves(&mut cache, &def, &skeleton), 1, "the root moved");
        let index = cache.chain_slot(&def, &skeleton).expect("chain");
        assert_eq!(cache.chains[index].chain.joints, [1, 2, 3]);
    }
}
