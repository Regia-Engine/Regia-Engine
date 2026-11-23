// SPDX-License-Identifier: MPL-2.0
//! Behaviour-tree leaf tasks and the tick context (`docs/specs/22-tier2.md` § T2.2).
//!
//! A leaf task is a [`BtTask`] implementation — the small piece of Rust that does one concrete
//! thing (move to the target, check line of sight, fire). Trees are composed in `.bt.ron` and
//! resolve a leaf by its type path (registered with [`BtTaskRegistration`], derived via
//! `#[derive(BtTask)]`). The tick context hands the task the world, its agent entity and the
//! tick delta.

use regia_app::prelude::*;

use crate::blackboard::Blackboard;

/// The result of ticking a behaviour-tree node.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BtStatus {
    /// The node succeeded.
    Success,
    /// The node failed.
    Failure,
    /// The node is still working; tick it again next frame.
    #[default]
    Running,
}

/// A leaf task: the Rust unit of behaviour in a tree.
///
/// Implement `tick` by hand and derive [`BtTaskRegistration`] with `#[derive(BtTask)]` (which
/// also requires `Default`). The task is looked up in a tree definition by its type path.
pub trait BtTask: Send + Sync + 'static {
    /// Runs one tick of the task.
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus;

    /// Clears the task's own state so the next `tick` starts a fresh run. The tree calls this when
    /// a `Repeat` starts another iteration and when a `Timeout` or a finished `Parallel` aborts the
    /// subtree. The default does nothing. Blackboard keys a task wrote are not cleared by the
    /// tree; a task that latches state there removes it in its own `reset`.
    fn reset(&mut self, _ctx: &mut BtContext<'_>) {}

    /// The resolved `set` map of a logic-graph action, in key order, handed over before each tick
    /// right after the same values were written into the entity blackboard (spec 102 § 4.3). Trees
    /// never call it. The default ignores it; a configured task that must know which parameters it
    /// got keeps them (the Lua call action passes them to its function as a table).
    fn logic_params(&mut self, _params: &[(String, crate::blackboard::BbValue)]) {}
}

/// The registration the derive macro generates so a tree can construct a leaf from its type path.
pub trait BtTaskRegistration {
    /// Constructs a default instance, boxed as a leaf task.
    fn construct() -> Box<dyn BtTask>
    where
        Self: Sized;
    /// The full type path used to look the task up in a tree definition.
    fn task_type_path() -> &'static str
    where
        Self: Sized;
}

/// The per-tick context handed to a leaf task: the world, the agent entity and the frame delta.
pub struct BtContext<'a> {
    /// The world (exclusive access — trees tick in one system).
    pub world: &'a mut World,
    /// The agent entity the tree is attached to.
    pub entity: Entity,
    /// The simulated time of this tick in seconds (one fixed step when driven by
    /// [`crate::AiPlugin`]).
    pub dt: f32,
}

impl<'a> BtContext<'a> {
    /// Reads the agent's [`Blackboard`].
    pub fn blackboard(&self) -> Option<&Blackboard> {
        self.world.get::<Blackboard>(self.entity)
    }

    /// Mutably reads the agent's [`Blackboard`].
    pub fn blackboard_mut(&mut self) -> Option<Mut<'_, Blackboard>> {
        self.world.get_mut::<Blackboard>(self.entity)
    }

    /// Reads the agent's world position (from its `GlobalTransform`, falling back to the local
    /// `Transform` for an entity that has not been propagated yet). The global transform is
    /// refreshed once per frame; `set_position` updates its translation at once.
    pub fn position(&self) -> Option<regia_core::math::Vec3> {
        if let Some(global) = self.world.get::<regia_scene::GlobalTransform>(self.entity) {
            return Some(global.translation());
        }
        self.world
            .get::<regia_scene::Transform>(self.entity)
            .map(|t| t.translation)
    }

    /// Moves the agent to the world position `position`: the `Transform.translation` is set
    /// through the parent's `GlobalTransform` when the agent has a parent. Returns `false` and
    /// leaves the transform unchanged for a non-finite position or an agent without a `Transform`.
    pub fn set_position(&mut self, position: regia_core::math::Vec3) -> bool {
        if !position.is_finite() {
            return false;
        }
        let parent_global = self
            .world
            .get::<ChildOf>(self.entity)
            .and_then(|p| self.world.get::<regia_scene::GlobalTransform>(p.parent()))
            .map(|g| g.0);
        let local = match parent_global {
            Some(parent) => parent.inverse().transform_point3(position),
            None => position,
        };
        let moved = self
            .world
            .get_mut::<regia_scene::Transform>(self.entity)
            .map(|mut t| t.translation = local)
            .is_some();
        // Keep the global translation in step, so `position` reads the new value before the next
        // transform propagation (rotation and scale are left to propagation).
        if moved && let Some(mut global) = self.world.get_mut::<regia_scene::GlobalTransform>(self.entity) {
            global.0.translation = position.into();
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_scene::{GlobalTransform, Transform};

    #[test]
    fn position_is_world_space_under_a_parent() {
        let mut world = World::new();
        let parent = world
            .spawn((
                Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
                GlobalTransform::IDENTITY * Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            ))
            .id();
        let child = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                GlobalTransform::IDENTITY * Transform::from_translation(Vec3::new(11.0, 0.0, 0.0)),
                ChildOf(parent),
            ))
            .id();
        let mut ctx = BtContext {
            world: &mut world,
            entity: child,
            dt: 0.0,
        };
        assert_eq!(ctx.position(), Some(Vec3::new(11.0, 0.0, 0.0)));
        assert!(ctx.set_position(Vec3::new(12.0, 0.0, 0.0)));
        let local = world.get::<Transform>(child).expect("transform").translation;
        assert!(
            (local - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-5,
            "local {local}"
        );
    }

    #[test]
    fn set_position_without_a_parent_writes_the_translation() {
        let mut world = World::new();
        let e = world.spawn(Transform::from_translation(Vec3::ZERO)).id();
        let mut ctx = BtContext {
            world: &mut world,
            entity: e,
            dt: 0.0,
        };
        assert_eq!(ctx.position(), Some(Vec3::ZERO));
        assert!(ctx.set_position(Vec3::new(1.0, 2.0, 3.0)));
        assert_eq!(ctx.position(), Some(Vec3::new(1.0, 2.0, 3.0)));
    }
}
