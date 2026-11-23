// SPDX-License-Identifier: MPL-2.0
//! Swimming (spec § 3): entering at the chest, a float line, surface swim and held dive, leaving by
//! depth.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

use super::{
    CharacterTraversal, Outcome, StepCx, SwimPhase, SwimSettings, TraversalEvent, TraversalKind, send_event,
};
use crate::character::{CharacterState, MovementMode, turned_rotation};

/// Answers the water surface for traversal. Install with [`TraversalWater`] where the terrain
/// crate's water bodies are not the answer: a dedicated server, a test, a game's own water.
pub trait WaterSource: Send + Sync + 'static {
    /// The surface height over world `(x, z)`, if there is water there.
    fn surface_height(&self, world: &World, x: f32, z: f32) -> Option<f32>;
}

/// Overrides where [`water_surface`] looks.
#[derive(Resource)]
pub struct TraversalWater(pub Box<dyn WaterSource>);

/// A flat water level inside a box in plan: enough for a server that loads no terrain crate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatWater {
    /// The surface height.
    pub height: f32,
    /// The smallest corner (`x`, `z`).
    pub min: [f32; 2],
    /// The largest corner (`x`, `z`).
    pub max: [f32; 2],
}

impl WaterSource for FlatWater {
    fn surface_height(&self, _world: &World, x: f32, z: f32) -> Option<f32> {
        let inside = x >= self.min[0] && x <= self.max[0] && z >= self.min[1] && z <= self.max[1];
        (inside && self.height.is_finite()).then_some(self.height)
    }
}

/// The water surface over `(x, z)`: the [`TraversalWater`] source when installed, else the terrain
/// crate's water bodies on client builds (the rule of `WaterQuery`: the highest wet body, the shore
/// switch, the floating origin). A plain build has no terrain crate and answers `None`.
pub fn water_surface(world: &mut World, x: f32, z: f32) -> Option<f32> {
    let surface = match world.get_resource::<TraversalWater>() {
        Some(source) => source.0.surface_height(world, x, z),
        None => terrain_surface(world, x, z),
    };
    surface.filter(|height| height.is_finite())
}

#[cfg(feature = "client")]
fn terrain_surface(world: &mut World, x: f32, z: f32) -> Option<f32> {
    use regia_terrain::water_body::plugin::WaterSettings;
    use regia_terrain::water_body::query::{BodyRef, surface_at};
    use regia_terrain::water_body::shore::WaterShore;
    use regia_terrain::water_body::{WaterBody, WaterClock};

    let mut query = world.query::<(Entity, &WaterBody, Option<&WaterShore>)>();
    let world = &*world;
    let clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
    let shore = world.get_resource::<WaterSettings>().is_none_or(|s| s.shore);
    let origin = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or([0.0; 3], |o| o.0.map(|axis| axis as f32));
    let bodies = query.iter(world).map(|(entity, body, field)| BodyRef {
        entity,
        body,
        shore: field.map(|s| s.field.as_ref()),
    });
    surface_at(bodies, x + origin[0], z + origin[2], &clock, shore)
        .map(|(_, surface)| surface.height - origin[1])
}

#[cfg(not(feature = "client"))]
fn terrain_surface(_world: &mut World, _x: f32, _z: f32) -> Option<f32> {
    None
}

/// Enters the water when the chest is under and the character is not rising.
pub(crate) fn try_enter(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Option<Outcome> {
    if !cx.swim.enabled || state.velocity.y > 0.0 {
        return None;
    }
    let at = transform.translation;
    let surface = water_surface(world, at.x, at.z)?;
    if at.y + cx.swim.chest_offset > surface {
        return None;
    }
    traversal.kind = TraversalKind::Swim;
    traversal.last_refusal = None;
    let slot = world
        .get::<super::TraversalSettings>(cx.entity)
        .map(|s| s.montage_slot.clone())
        .unwrap_or_default();
    send_event(
        world,
        TraversalEvent::Started {
            entity: cx.entity,
            kind: TraversalKind::Swim,
            slot,
            montage: TraversalKind::Swim.name(),
            duration: 0.0,
            warp_targets: Vec::new(),
        },
    );
    Some(swim_step(world, cx, traversal, state, transform, surface))
}

/// The target vertical speed: dive, 3D input, or back to the float line (spec § 3).
fn vertical_target(s: &SwimSettings, crouch: bool, input_y: f32, depth: f32) -> f32 {
    let target = if crouch {
        -s.dive_speed
    } else if input_y.abs() > 1.0e-3 {
        input_y * s.dive_speed
    } else {
        (depth * s.float_rate).min(s.rise_speed)
    };
    if depth >= s.max_dive_depth && target < 0.0 {
        0.0
    } else {
        target
    }
}

/// One step of a running swim. Ladders and ledges come first; no water ends the swim.
pub(crate) fn step(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Outcome {
    if let Some(order) = traversal.pending.take()
        && let Some(outcome) = super::run_order(world, cx, traversal, state, transform, order)
    {
        return outcome;
    }
    if let Some(outcome) = super::ladder::try_attach(world, cx, traversal, state, transform) {
        return outcome;
    }
    let at = transform.translation;
    let Some(surface) = water_surface(world, at.x, at.z).filter(|_| cx.swim.enabled) else {
        finish(world, cx, traversal);
        return Outcome::PlainWith(MovementMode::Falling);
    };
    let chest = at.y + cx.swim.chest_offset;
    let near_surface = surface - chest <= cx.swim.surface_band;
    if cx.input.jump && near_surface {
        if let Some(outcome) = super::try_ledge(world, cx, traversal, state, transform, false) {
            return outcome;
        }
        // A hop: plain movement carries it, and it cannot re-enter while rising.
        finish(world, cx, traversal);
        state.velocity.y = cx.swim.jump_velocity;
        state.grounded = false;
        state.jumps_used = 1;
        return Outcome::PlainWith(MovementMode::Falling);
    }
    swim_step(world, cx, traversal, state, transform, surface)
}

fn finish(world: &mut World, cx: &StepCx<'_>, traversal: &mut CharacterTraversal) {
    traversal.kind = TraversalKind::None;
    traversal.depth = 0.0;
    traversal.phase = SwimPhase::Surface;
    send_event(
        world,
        TraversalEvent::Finished {
            entity: cx.entity,
            kind: TraversalKind::Swim,
        },
    );
}

fn swim_step(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    surface: f32,
) -> Outcome {
    let s = &cx.swim;
    let at = transform.translation;
    let float_y = surface - s.chest_offset;
    let depth = float_y - at.y;
    let input = cx.input.move_dir.clamp_length_max(1.0);
    let speed = if cx.input.sprint {
        s.speed * s.sprint_multiplier
    } else {
        s.speed
    };
    let target = Vec3::new(
        input.x * speed,
        vertical_target(s, cx.input.crouch, input.y, depth),
        input.z * speed,
    );
    let mut velocity = if state.velocity.is_finite() {
        state.velocity
    } else {
        Vec3::ZERO
    };
    // Deterministic `exp` (no platform libm): the velocity feeds the position, which is sim state.
    velocity += (target - velocity) * (1.0 - regia_core::math::det_exp(-s.drag * cx.dt));
    let mut delta = velocity * cx.dt;
    // A dive does not pass its limit, and the float rate does not overshoot the line.
    if delta.y < 0.0 && at.y + delta.y < float_y - s.max_dive_depth {
        delta.y = ((float_y - s.max_dive_depth) - at.y).min(0.0);
        velocity.y = delta.y / cx.dt;
    }
    let movement = cx.movement;
    let result = cx.collision.move_shape(
        world,
        cx.entity,
        at,
        movement.radius,
        movement.half_height,
        delta,
        cx.dt,
        cx.dt,
        movement.max_slope_angle,
        movement.step_height,
        0.0,
    );
    // Velocity reads the motion achieved when the move was blocked.
    let achieved = result.position - at;
    if achieved.is_finite() && delta.length_squared() > 1.0e-10 && achieved.length() < 0.9 * delta.length() {
        velocity = achieved / cx.dt;
    }
    // A backend that answers with a non-finite position keeps the last pose, like the plain step.
    let position = if result.position.is_finite() {
        result.position
    } else {
        velocity = Vec3::ZERO;
        at
    };
    let chest = position.y + s.chest_offset;
    let facing = turned_rotation(cx.movement, transform, input.with_y(0.0), None, cx.dt);

    // Leaving by depth: the gap to the enter line keeps a character from flipping on one spot.
    if result.grounded && chest >= surface + s.exit_margin {
        finish(world, cx, traversal);
        velocity.y = 0.0;
        super::in_move_state(state, MovementMode::Walking, velocity);
        state.grounded = true;
        return Outcome::Moved(result, position, facing, MovementMode::Walking);
    }
    if !result.grounded && chest > surface + s.leave_margin {
        finish(world, cx, traversal);
        super::in_move_state(state, MovementMode::Falling, velocity);
        return Outcome::Moved(result, position, facing, MovementMode::Falling);
    }

    let new_depth = float_y - position.y;
    traversal.depth = new_depth;
    traversal.phase = if new_depth.abs() <= s.surface_band {
        SwimPhase::Surface
    } else {
        SwimPhase::Underwater
    };
    super::in_move_state(state, MovementMode::Swimming, velocity);
    state.grounded = result.grounded;
    Outcome::Moved(result, position, facing, MovementMode::Swimming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vertical_target_prefers_the_dive_then_input_then_the_float_line() {
        let s = SwimSettings::default();
        assert_eq!(vertical_target(&s, true, 0.5, 1.0), -s.dive_speed);
        assert_eq!(vertical_target(&s, false, 0.5, 1.0), 0.5 * s.dive_speed);
        assert_eq!(vertical_target(&s, false, 0.0, 0.25), 0.25 * s.float_rate);
        assert_eq!(vertical_target(&s, false, 0.0, 10.0), s.rise_speed);
        // Above the line it settles down at the same rate.
        assert_eq!(vertical_target(&s, false, 0.0, -0.25), -0.25 * s.float_rate);
        // At the limit a dive holds and a rise is free.
        assert_eq!(vertical_target(&s, true, 0.0, s.max_dive_depth), 0.0);
        assert_eq!(vertical_target(&s, false, 0.0, s.max_dive_depth), s.rise_speed);
    }

    #[test]
    fn a_flat_water_source_answers_inside_its_box_only() {
        let water = FlatWater {
            height: 2.0,
            min: [0.0, 0.0],
            max: [4.0, 4.0],
        };
        let world = World::new();
        assert_eq!(water.surface_height(&world, 1.0, 1.0), Some(2.0));
        assert_eq!(water.surface_height(&world, 5.0, 1.0), None);
    }
}
