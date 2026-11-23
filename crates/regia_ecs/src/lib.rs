// SPDX-License-Identifier: MPL-2.0
//! ECS + reflection facade over bevy_ecs/bevy_reflect, plus reflected (de)serialization.
//!
//! Specification: `docs/specs/10-core.md` § 2. Game and engine code import from [`prelude`] and
//! never name `bevy_*` directly.
//!
//! - [`prelude`]: ECS and reflection items game code normally needs.
//! - [`reflect`]: the full reflection API (type registry, type info, dynamic values).
//! - [`attrs`]: editor attributes used as `#[reflect(@Range(0.0..=1.0))]`.
//! - [`describe`]: plain accessors over registered types (fields, docs, attributes) for the
//!   inspector and the agent protocol.
//! - [`serialize`]: canonical RON and postcard (de)serialization of reflected values.
//! - [`owner`]: who registered what, so hot reload can remove a game plugin's registrations.
//! - [`state`]: finite game states (`bevy_state`): `States`, sub-states, computed states,
//!   `OnEnter`/`OnExit` schedules, `in_state` run conditions and state-scoped entities. The app
//!   side (running the `StateTransition` schedule, `App::init_state`) lives in `regia_app`.
//! - [`spawn_order`]: the order a component arrived in the world, independent of entity ids, for
//!   code that must process new entities in a repeatable order on any thread count.
//!
//! # Getting started
//!
//! Components are plain structs; a `World` holds the entities that carry them.
//!
//! ```
//! use regia_ecs::prelude::*;
//!
//! #[derive(Component)]
//! struct Health(i32);
//!
//! let mut world = World::new();
//! world.spawn(Health(10));
//! world.spawn(Health(3));
//!
//! let mut query = world.query::<&Health>();
//! assert_eq!(query.iter(&world).map(|h| h.0).sum::<i32>(), 13);
//! ```

pub mod attrs;
pub mod describe;
pub mod owner;
pub mod serialize;
pub mod spawn_order;

/// The ECS. Prefer [`prelude`]; this is for the rare item the prelude does not carry.
pub use bevy_ecs as ecs;

/// Finite game states, re-exported from `bevy_state` (version-locked with `bevy_ecs`).
///
/// A state type is an enum (or struct) deriving [`States`](state::States); the current value is
/// the [`State<S>`](state::State) resource and a change is requested by writing
/// [`NextState<S>`](state::NextState). The `StateTransition` schedule (run by `regia_app`'s
/// `App` after `PreUpdate` and once before `Startup`) applies the request and runs, in order,
/// [`OnExit`](state::OnExit) of the old value, [`OnTransition`](state::OnTransition), then
/// [`OnEnter`](state::OnEnter) of the new one. [`SubStates`](state::SubStates) exist only while
/// their source state matches; [`ComputedStates`](state::ComputedStates) are derived from other
/// states and never set directly. Entities tagged [`DespawnOnExit`](state::DespawnOnExit) are
/// despawned when their state is left. Register states through `regia_app::App::init_state`
/// and friends, which also make them visible to the agent protocol (`state.*`).
pub mod state {
    pub use bevy_state::commands::CommandsStatesExt;
    pub use bevy_state::condition::*;
    pub use bevy_state::reflect::{
        ReflectFreelyMutableState, ReflectFreelyMutableStateFns, ReflectState, ReflectStateFns,
    };
    pub use bevy_state::state::*;
    pub use bevy_state::state_scoped::*;
}

/// Reflection: the type registry, type info, dynamic values, custom attributes and the low-level
/// reflect serializers. Prefer [`prelude`] for derives and the common traits.
pub mod reflect {
    pub use bevy_reflect::*;

    pub use bevy_ecs::reflect::{
        AppTypeRegistry, ReflectBundle, ReflectCommandExt, ReflectComponent, ReflectEvent, ReflectFromWorld,
        ReflectMapEntities, ReflectMessage, ReflectResource,
    };
}

/// Everything game code normally needs from the ECS and reflection.
pub mod prelude {
    pub use bevy_ecs::prelude::*;
    pub use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleLabel};
    // Not in `bevy_ecs::prelude`: a `!Send` marker system param that pins a system to the main
    // thread of the schedule executor (needed by anything that touches an OS window / winit).
    pub use bevy_ecs::system::NonSendMarker;
    pub use bevy_reflect::prelude::*;
    pub use bevy_reflect::{GetTypeRegistration, TypeRegistry, Typed};
    pub use bevy_state::prelude::*;

    pub use crate::owner::{OwnerSet, TypeOwner, TypeOwnership};

    // Derive macros emit `bevy_ecs::...` / `bevy_reflect::...` paths. Re-exporting the crates under
    // those names lets `#[derive(Component, Reflect)]` work in crates that depend only on Regia Engine.
    #[doc(hidden)]
    pub use bevy_ecs;
    #[doc(hidden)]
    pub use bevy_reflect;
    #[doc(hidden)]
    pub use bevy_state;
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;

    /// Doc comment captured by `reflect_documentation`.
    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Health {
        current: f32,
    }

    #[derive(Resource, Reflect, Default)]
    #[reflect(Resource, Default)]
    struct Score(u32);

    #[test]
    fn prelude_covers_derives_and_type_data() {
        let mut registry = TypeRegistry::default();
        registry.register::<Health>();
        registry.register::<Score>();
        let health = registry.get(std::any::TypeId::of::<Health>());
        assert!(health.is_some_and(|r| r.data::<ReflectComponent>().is_some()));
        assert!(health.is_some_and(|r| r.data::<ReflectDefault>().is_some()));
        let score = registry.get(std::any::TypeId::of::<Score>());
        assert!(score.is_some_and(|r| r.data::<ReflectResource>().is_some()));
        assert_eq!(
            Health::type_info().docs(),
            Some(" Doc comment captured by `reflect_documentation`.")
        );
    }
}
