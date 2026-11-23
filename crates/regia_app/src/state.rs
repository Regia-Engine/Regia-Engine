// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit 343a023 = v0.20.0-rc.2,
// `crates/bevy_state/src/app.rs`), Copyright (c) Bevy contributors, MIT OR Apache-2.0, used under
// the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or
// substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
// BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Game states on [`App`]: `init_state`, `insert_state`, `add_sub_state`, `add_computed_state`.
//!
//! `bevy_state`'s own `AppExtStates` needs `bevy_app`, which Regia Engine does not link; these are the
//! same registrations against Regia Engine's [`App`]. The app creates the `StateTransition` schedule in
//! [`App::new`] and runs it once before [`crate::Startup`] and every frame after
//! [`crate::PreUpdate`] (`App::update`).
//!
//! Unlike `bevy_state`, every state type must be reflected: registration also records the
//! `ReflectState` (and for settable states `ReflectFreelyMutableState`) type data, so the agent
//! protocol's `state.list` / `state.get` / `state.set` see every state of every app.

use regia_ecs::prelude::*;
use regia_ecs::reflect::{FromReflect, TypePath};
use regia_ecs::state::{
    ComputedStates, FreelyMutableState, NextState, PreviousState, ReflectFreelyMutableState, ReflectState,
    State, StateTransition, StateTransitionEvent, StateTransitionSystems, States, SubStates,
    despawn_entities_on_enter_state, despawn_entities_on_exit_state, despawn_entities_when_state,
    disable_entities_on_enter_state, disable_entities_on_exit_state, disable_entities_when_state,
    enable_entities_on_enter_state, enable_entities_on_exit_state, enable_entities_when_state,
};

use crate::App;

/// Bounds every state type registered through [`App`] meets: reflected, so the agent protocol
/// and the inspector can read it. `#[derive(States, Reflect, ...)]` satisfies it.
pub trait ReflectedState: States + Reflect + FromReflect + TypePath + GetTypeRegistration + Typed {}

impl<S: States + Reflect + FromReflect + TypePath + GetTypeRegistration + Typed> ReflectedState for S {}

impl App {
    /// Registers a settable state starting at its [`FromWorld`] (usually `Default`) value: the
    /// [`State<S>`] and [`NextState<S>`] resources, its transition systems, its
    /// [`StateTransitionEvent<S>`] message and `DespawnOnExit<S>`-style entity scoping. The
    /// initial value's `OnEnter` runs on the next `StateTransition` (before `Startup` on the
    /// first frame). Registering the same state twice logs a warning and keeps the first.
    pub fn init_state<S: FreelyMutableState + ReflectedState + FromWorld>(&mut self) -> &mut Self {
        if self.world().contains_resource::<State<S>>() {
            tracing::warn!(target: "regia_app", "state {} is already initialized", S::type_path());
            return self;
        }
        self.init_resource::<State<S>>();
        self.register_settable_state::<S>();
        self
    }

    /// Registers a settable state starting at `state`. Calling it again for a registered state
    /// replaces the current value (and re-runs its `OnEnter` on the next transition) without
    /// running `OnExit` of the old one, as in `bevy_state`.
    pub fn insert_state<S: FreelyMutableState + ReflectedState>(&mut self, state: S) -> &mut Self {
        if self.world().contains_resource::<State<S>>() {
            self.insert_resource(State::new(state.clone()));
            // A pending request would overwrite the inserted value and exit a state never entered.
            if let Some(mut next) = self.world_mut().get_resource_mut::<NextState<S>>() {
                next.reset();
            }
            if let Some(mut messages) = self
                .world_mut()
                .get_resource_mut::<Messages<StateTransitionEvent<S>>>()
            {
                messages.clear();
            }
            self.world_mut().write_message(StateTransitionEvent {
                exited: None,
                entered: Some(state),
                allow_same_state_transitions: true,
            });
            return self;
        }
        self.insert_resource(State::new(state));
        self.register_settable_state::<S>();
        self
    }

    /// Registers a computed state: derived from its source states each transition, never set
    /// directly. It exists (as a [`State<S>`] resource) only while `compute` returns `Some`.
    pub fn add_computed_state<S: ComputedStates + ReflectedState>(&mut self) -> &mut Self {
        if self
            .world()
            .contains_resource::<Messages<StateTransitionEvent<S>>>()
        {
            tracing::warn!(target: "regia_app", "computed state {} is already initialized", S::type_path());
            return self;
        }
        self.add_message::<StateTransitionEvent<S>>();
        self.defer_transition_systems(S::register_computed_state_systems);
        let current = self.world().get_resource::<State<S>>().map(|s| s.get().clone());
        self.world_mut().write_message(StateTransitionEvent {
            exited: None,
            entered: current,
            allow_same_state_transitions: S::ALLOW_SAME_STATE_TRANSITIONS,
        });
        self.enable_state_scoped_entities::<S>();
        self.register_type::<S>()
            .register_type::<State<S>>()
            .register_type::<PreviousState<S>>();
        self.register_state_type_data::<S>();
        self
    }

    /// Registers a sub-state: it exists only while its `#[source(...)]` state matches, starts at
    /// its default value each time it comes into existence, and can be set while it exists.
    pub fn add_sub_state<S: SubStates + ReflectedState>(&mut self) -> &mut Self {
        if self
            .world()
            .contains_resource::<Messages<StateTransitionEvent<S>>>()
        {
            tracing::warn!(target: "regia_app", "sub-state {} is already initialized", S::type_path());
            return self;
        }
        self.init_resource::<NextState<S>>();
        self.add_message::<StateTransitionEvent<S>>();
        self.defer_transition_systems(S::register_sub_state_systems);
        let current = self.world().get_resource::<State<S>>().map(|s| s.get().clone());
        self.world_mut().write_message(StateTransitionEvent {
            exited: None,
            entered: current,
            allow_same_state_transitions: true,
        });
        self.enable_state_scoped_entities::<S>();
        self.register_mutable_state_types::<S>();
        self
    }

    /// Runs the `StateTransition` schedule now, outside the frame. Tests and tools use it to apply
    /// a pending [`NextState`] without stepping a whole frame.
    pub fn apply_state_transitions(&mut self) {
        self.run_schedule(StateTransition);
    }

    /// Shared tail of [`App::init_state`] / [`App::insert_state`] once `State<S>` exists.
    fn register_settable_state<S: FreelyMutableState + ReflectedState>(&mut self) {
        self.init_resource::<NextState<S>>()
            .add_message::<StateTransitionEvent<S>>();
        self.with_transition_schedule(S::register_state);
        let current = self.world().resource::<State<S>>().get().clone();
        self.world_mut().write_message(StateTransitionEvent {
            exited: None,
            entered: Some(current),
            allow_same_state_transitions: true,
        });
        self.enable_state_scoped_entities::<S>();
        self.register_mutable_state_types::<S>();
    }

    fn register_mutable_state_types<S: FreelyMutableState + ReflectedState>(&mut self) {
        self.register_type::<S>()
            .register_type::<State<S>>()
            .register_type::<NextState<S>>()
            .register_type::<PreviousState<S>>();
        self.register_state_type_data::<S>();
        self.type_registry()
            .write()
            .register_type_data::<S, ReflectFreelyMutableState>();
    }

    fn register_state_type_data<S: ReflectedState>(&mut self) {
        self.type_registry()
            .write()
            .register_type_data::<S, ReflectState>();
    }

    /// Queues a sub/computed state's transition systems. They read the source state's message
    /// buffer, which exists only once the source is registered; registering them at the first
    /// `StateTransition` run makes `add_sub_state` before `init_state` work instead of aborting.
    fn defer_transition_systems(&mut self, register: impl FnOnce(&mut Schedule) + Send + Sync + 'static) {
        self.pending_state_systems.push(Box::new(register));
    }

    /// Registers the queued transition systems (a no-op when none are queued).
    pub(crate) fn flush_pending_state_systems(&mut self) {
        for register in std::mem::take(&mut self.pending_state_systems) {
            self.with_transition_schedule(register);
        }
    }

    /// Calls `register` on the `StateTransition` schedule (created by [`App::new`]).
    fn with_transition_schedule(&mut self, register: impl FnOnce(&mut Schedule)) {
        let mut schedules = self.world_mut().resource_mut::<Schedules>();
        match schedules.get_mut(StateTransition) {
            Some(schedule) => register(schedule),
            None => {
                tracing::error!(target: "regia_app", "the StateTransition schedule is missing");
            }
        }
    }

    /// Despawn/disable/enable-on-exit/enter scoping for entities tagged with a value of `S`.
    fn enable_state_scoped_entities<S: States>(&mut self) {
        self.add_systems(
            StateTransition,
            (
                despawn_entities_on_exit_state::<S>,
                disable_entities_on_exit_state::<S>,
                enable_entities_on_exit_state::<S>,
            )
                .in_set(StateTransitionSystems::ExitSchedules),
        )
        .add_systems(
            StateTransition,
            (
                despawn_entities_on_enter_state::<S>,
                disable_entities_on_enter_state::<S>,
                enable_entities_on_enter_state::<S>,
            )
                .in_set(StateTransitionSystems::EnterSchedules),
        )
        .add_systems(
            StateTransition,
            (
                despawn_entities_when_state::<S>,
                disable_entities_when_state::<S>,
                enable_entities_when_state::<S>,
            )
                .in_set(StateTransitionSystems::TransitionSchedules),
        );
    }
}
