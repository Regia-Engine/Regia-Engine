// SPDX-License-Identifier: MPL-2.0
//! Aircraft in an app (ADR-3374): [`AircraftPlugin`] schedules `regia_physics::aero`'s fixed-step
//! update, maps input actions onto [`AircraftInput`] and runs the optional rate-damping assist.
//!
//! Add it after `PhysicsPlugin`. Spawn planes with `regia_physics::aero::spawn_aircraft`; drive one
//! from the keyboard or a pad by giving its entity an [`AircraftBindings`] (action names per channel,
//! read from `regia_input::Actions`), or write its [`AircraftInput`] yourself (the agent's
//! `physics.aircraft_input` does). An [`AircraftAssist`] on the entity damps the body rates: each
//! fixed step it writes `−gain × rate` into the controller's additive `augment`, so a released stick
//! settles the plane's rotation instead of leaving it to the airframe's own damping.
//!
//! | Schedule | System | Purpose |
//! |---|---|---|
//! | `FixedUpdate` (in `PhysicsStep`, after the velocity sync, before the step) | [`map_aircraft_input`] → [`apply_aircraft_assist`] → `drive_aircraft` | Input, assist, loads |
//! | `FixedUpdate` (after `StreamSet::Rebase`) | `shift_aircraft_origin` | Gear contacts follow a rebase |
//!
//! Without the plugin nothing about the physics schedule changes, so other features' determinism
//! pins do not move. See `book/agents/aircraft.md`.

use regia_app::prelude::*;
use regia_physics::PhysicsWorld;
use regia_physics::aero::{
    AeroEnvironment, AircraftController, AircraftInput, ControlAugment, drive_aircraft, shift_aircraft_origin,
};
use regia_physics::convert::from_rapier_pose;
use regia_physics::{PhysicsStep, step_physics, sync_velocity_and_forces};
use tracing::debug_span;

/// Default action name for the pitch axis (+ nose up).
pub const PITCH_ACTION: &str = "aircraft_pitch";
/// Default action name for the roll axis (+ right).
pub const ROLL_ACTION: &str = "aircraft_roll";
/// Default action name for the yaw axis (+ nose right).
pub const YAW_ACTION: &str = "aircraft_yaw";
/// Default action name for the throttle (0..1).
pub const THROTTLE_ACTION: &str = "aircraft_throttle";
/// Default action name for the flaps (0..1).
pub const FLAPS_ACTION: &str = "aircraft_flaps";
/// Default action name for the wheel brakes (0..1).
pub const BRAKES_ACTION: &str = "aircraft_brakes";

/// Which input actions fly this aircraft: one action per channel, read as its x value for one
/// player. An unknown action reads 0. Every fixed step the mapping overwrites the entity's
/// [`AircraftInput`].
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AircraftBindings {
    /// The player whose actions are read.
    pub player: u8,
    /// Pitch axis action.
    pub pitch: String,
    /// Roll axis action.
    pub roll: String,
    /// Yaw axis action.
    pub yaw: String,
    /// Throttle action (0..1).
    pub throttle: String,
    /// Flaps action (0..1).
    pub flaps: String,
    /// Brakes action (0..1; a button reads 1 while held).
    pub brakes: String,
}

impl Default for AircraftBindings {
    /// Player 0 with the `aircraft_*` action names.
    fn default() -> Self {
        Self {
            player: 0,
            pitch: PITCH_ACTION.into(),
            roll: ROLL_ACTION.into(),
            yaw: YAW_ACTION.into(),
            throttle: THROTTLE_ACTION.into(),
            flaps: FLAPS_ACTION.into(),
            brakes: BRAKES_ACTION.into(),
        }
    }
}

/// Rate damping: the input added per rad/s of body rate (each ≥ 0), so the assist's term is
/// `−gain × rate` on the matching channel, clamped like the input.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AircraftAssist {
    /// Off leaves the controller's augment at zero.
    pub enabled: bool,
    /// Pitch input per rad/s of pitch rate (+ nose up).
    pub pitch_damping: f32,
    /// Roll input per rad/s of roll rate (+ right).
    pub roll_damping: f32,
    /// Yaw input per rad/s of yaw rate (+ nose right).
    pub yaw_damping: f32,
}

impl Default for AircraftAssist {
    /// On, with gains that settle the sample trainer's rotation within about a second.
    fn default() -> Self {
        Self {
            enabled: true,
            pitch_damping: 0.5,
            roll_damping: 0.4,
            yaw_damping: 0.5,
        }
    }
}

impl AircraftAssist {
    /// The augment for body rates `rates` (the world angular velocity in body axes: x pitch + nose up,
    /// y + nose left, z + left wing down).
    #[must_use]
    pub fn augment(&self, rates: regia_core::math::Vec3) -> ControlAugment {
        if !self.enabled {
            return ControlAugment::default();
        }
        let gain = |g: f32| if g.is_finite() { g.max(0.0) } else { 0.0 };
        let clamp = |v: f32| if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
        ControlAugment {
            pitch: clamp(-gain(self.pitch_damping) * rates.x),
            // Right roll is rotation about −Z, nose-right yaw about −Y.
            roll: clamp(gain(self.roll_damping) * rates.z),
            yaw: clamp(gain(self.yaw_damping) * rates.y),
        }
    }
}

/// Copies each bound aircraft's actions into its [`AircraftInput`] (sanitized).
pub fn map_aircraft_input(
    actions: Option<Res<regia_input::Actions>>,
    mut bound: Query<(&AircraftBindings, &mut AircraftInput)>,
) {
    let Some(actions) = actions else {
        return;
    };
    for (bindings, mut input) in &mut bound {
        let player = actions.player(bindings.player);
        let read = |name: &str| player.states.get(name).map_or(0.0, |state| state.value.x);
        let next = AircraftInput {
            pitch: read(&bindings.pitch),
            roll: read(&bindings.roll),
            yaw: read(&bindings.yaw),
            throttle: read(&bindings.throttle),
            flaps: read(&bindings.flaps),
            brakes: read(&bindings.brakes),
        }
        .sanitized();
        if *input != next {
            *input = next;
        }
    }
}

/// Writes each assisted aircraft's [`AircraftController::augment`] from its body rates, and clears the
/// augment of aircraft without an assist.
pub fn apply_aircraft_assist(
    physics: Option<Res<PhysicsWorld>>,
    mut assisted: Query<(&mut AircraftController, Option<&AircraftAssist>)>,
) {
    let Some(physics) = physics else {
        return;
    };
    let _span = debug_span!("apply_aircraft_assist").entered();
    for (mut controller, assist) in &mut assisted {
        let augment = match assist {
            Some(assist) => physics
                .bodies
                .get(controller.aircraft.chassis().to_handle())
                .map(|body| {
                    let (_, rotation) = from_rapier_pose(body.position());
                    let angvel = regia_physics::convert::from_rapier_vec3(body.angvel());
                    assist.augment(rotation.inverse() * angvel)
                })
                .unwrap_or_default(),
            None => ControlAugment::default(),
        };
        if controller.augment != augment {
            controller.augment = augment;
        }
    }
}

/// Aircraft physics in `FixedUpdate`: input mapping, assist and the aerodynamic update, inside the
/// physics chain before the step. Add it after `PhysicsPlugin`.
pub struct AircraftPlugin;

impl Plugin for AircraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AeroEnvironment>()
            .register_type::<AircraftInput>()
            .add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                (map_aircraft_input, apply_aircraft_assist, drive_aircraft)
                    .chain()
                    .after(sync_velocity_and_forces)
                    .before(step_physics)
                    .in_set(PhysicsStep),
            )
            .add_systems(
                FixedUpdate,
                shift_aircraft_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
        // Aircraft are driven in the order they arrived, not in entity-id order (as vehicles,
        // ADR-2396). Hard-fail like the physics plugins: the fallback sorts by entity ids that
        // parallel spawns reserve in thread-timing order, which diverges between replays.
        must_track_spawn_order::<AircraftController>(app.world_mut());
    }
}

/// Spawn-order tracking is a hard requirement for the aero feed, like
/// [`regia_physics`]' `must_track_spawn_order`: the untracked fallback sorts by
/// entity ids that parallel `Commands::spawn` calls reserve in thread-timing order. Fail at
/// plugin-build time instead of stepping a nondeterministic world.
fn must_track_spawn_order<C: Component>(world: &mut World) {
    if let Err(error) = regia_ecs::spawn_order::try_track_spawn_order::<C>(world) {
        panic!(
            "spawn-order tracking of `{}` failed ({error:?}): insertion order would follow \
             thread-timing entity ids and break deterministic replays. Add the aircraft plugin \
             before anything that inserts `{}` or hooks its on_add/on_remove.",
            std::any::type_name::<C>(),
            std::any::type_name::<C>()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::{Quat, Vec3};
    use regia_physics::aero::{AircraftConfig, spawn_aircraft};

    #[test]
    fn the_assist_opposes_each_rate() {
        let assist = AircraftAssist::default();
        // Nose pitching up: push down.
        assert!(assist.augment(Vec3::new(0.5, 0.0, 0.0)).pitch < 0.0);
        // Rolling right (rotation about −Z, so rates.z < 0): roll left.
        assert!(assist.augment(Vec3::new(0.0, 0.0, -0.5)).roll < 0.0);
        // Yawing right (rotation about −Y, so rates.y < 0): yaw left.
        assert!(assist.augment(Vec3::new(0.0, -0.5, 0.0)).yaw < 0.0);
        let off = AircraftAssist {
            enabled: false,
            ..AircraftAssist::default()
        };
        assert_eq!(off.augment(Vec3::ONE), ControlAugment::default());
        let wild = assist.augment(Vec3::new(f32::NAN, 100.0, -100.0));
        assert_eq!(wild.pitch, 0.0);
        assert_eq!(wild.yaw, 1.0);
        assert_eq!(wild.roll, -1.0);
    }

    #[test]
    fn the_plugin_registers_the_environment() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, AircraftPlugin));
        app.update();
        assert!(app.world().contains_resource::<AeroEnvironment>());
    }

    /// The aero feed sorts aircraft by spawn arrival (ADR-1347); tracking that silently stayed
    /// off used to order them by thread-timing entity ids and diverge replays. That now fails
    /// loudly at plugin-build time, like every physics plugin (muse review of
    /// fix-ecs-determinism, finding 1).
    #[test]
    #[should_panic(expected = "spawn-order tracking of")]
    fn plugin_build_fails_when_a_controller_is_already_in_use() {
        let mut app = App::new();
        app.add_plugins(regia_physics::PhysicsPlugin);
        spawn_aircraft(
            app.world_mut(),
            AircraftConfig::default(),
            Vec3::ZERO,
            Quat::IDENTITY,
            Vec3::ZERO,
        )
        .expect("aircraft");
        app.add_plugins(AircraftPlugin);
    }

    /// The other way tracking used to fail open: another owner holding the controller's
    /// `on_add` hook slot.
    #[test]
    #[should_panic(expected = "spawn-order tracking of")]
    fn plugin_build_fails_when_the_controllers_hooks_are_taken() {
        let mut app = App::new();
        app.world_mut()
            .register_component_hooks::<AircraftController>()
            .on_add(|_world, _context| {});
        app.add_plugins(AircraftPlugin);
    }
}
