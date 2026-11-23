// SPDX-License-Identifier: MPL-2.0
//! Ladders (spec § 4): attach at the bottom, the top or the side, climb with the facing locked, leave at
//! the top only with clear space, jump off.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

use super::curve::{self, settle};
use super::{
    Body, CharacterTraversal, Ladder, LadderLine, LadderSettings, Outcome, StepCx, TraversalEvent,
    TraversalKind, TraversalRefusal, clear_body_ground, facing_along, free_result, in_move_state, intent_of,
    queries, refuse, send_event, start_curve,
};
use crate::character::{CharacterMovement, CharacterState, MovementMode};
use crate::nav_world::world_pose;

/// The climb stops this close to the ends of its line (m).
const END_TOLERANCE: f32 = 1.0e-3;
/// A ladder further than this from a request's point is no match (m).
const ORDER_REACH: f32 = 4.0;

/// A ladder in a character's terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderProbe {
    /// The ladder's entity.
    pub entity: Entity,
    /// The ladder's `user_id`.
    pub user_id: u64,
    /// `bottom`, `side` or `top`.
    pub attach: &'static str,
    /// The line a character would climb.
    pub line: LadderLine,
}

/// The horizontal frame of a ladder and where a capsule sits on it.
fn line_of(
    world: &World,
    entity: Entity,
    ladder: &Ladder,
    movement: &CharacterMovement,
    skin: f32,
) -> Option<LadderLine> {
    if ladder.validate().is_err() {
        return None;
    }
    let (_, rotation, origin) = world_pose(world, entity).to_scale_rotation_translation();
    if !(rotation.is_finite() && origin.is_finite()) {
        return None;
    }
    let into = (rotation * Vec3::NEG_Z).with_y(0.0).normalize_or_zero();
    if into == Vec3::ZERO {
        return None;
    }
    let stand = movement.half_height + movement.radius;
    let standoff = ladder.standoff.max(movement.radius + skin);
    let line = origin - into * standoff;
    let top_y = origin.y + ladder.height + stand + skin;
    Some(LadderLine {
        line: Vec3::new(line.x, origin.y + stand + skin, line.z),
        into,
        top_y,
        speed: ladder.climb_speed,
        exit: Vec3::new(0.0, top_y, 0.0) + (origin + into * ladder.exit_forward).with_y(0.0),
        id: ladder.user_id,
    })
}

/// Every enabled, valid ladder, with its line.
fn ladders(world: &mut World, movement: &CharacterMovement, skin: f32) -> Vec<(Entity, Ladder, LadderLine)> {
    let mut query = world.query::<(Entity, &Ladder)>();
    let found: Vec<(Entity, Ladder)> = query
        .iter(world)
        .filter(|(_, ladder)| ladder.enabled)
        .map(|(entity, ladder)| (entity, *ladder))
        .collect();
    found
        .into_iter()
        .filter_map(|(entity, ladder)| {
            line_of(world, entity, &ladder, movement, skin).map(|line| (entity, ladder, line))
        })
        .collect()
}

/// What `order_key` returns: the distance's bits, the `user_id` and the line's position bits.
type LadderKey = (u32, u64, [u32; 3]);

/// A total order over ladders that does not depend on entity order: by distance, then `user_id`, then
/// the line's position bits.
fn order_key(line: &LadderLine, at: Vec3) -> LadderKey {
    let distance = line.line.with_y(at.y).distance(at);
    (
        distance.to_bits(),
        line.id,
        [
            line.line.x.to_bits(),
            line.line.y.to_bits(),
            line.line.z.to_bits(),
        ],
    )
}

/// How a capsule at `at` could attach to the ladder, when it is in the ladder's zone.
fn zone(
    line: &LadderLine,
    ladder: &Ladder,
    settings: &LadderSettings,
    movement: &CharacterMovement,
    at: Vec3,
) -> Option<&'static str> {
    let stand = movement.half_height + movement.radius;
    let right = Vec3::new(-line.into.z, 0.0, line.into.x);
    let offset = at - line.line;
    let lateral = offset.dot(right).abs();
    if lateral > ladder.half_width + settings.attach_margin {
        return None;
    }
    // Positive in front of the climb line, toward the climber's side.
    let front = -offset.dot(line.into);
    let feet = at.y - stand;
    let (base, top) = (line.line.y - stand, line.top_y - stand);
    let reach = settings.attach_reach;
    if (feet - top).abs() <= movement.step_height + 0.05 {
        let beyond = ladder.exit_forward + ladder.standoff.max(movement.radius) + reach;
        return (front >= -beyond && front <= reach).then_some("top");
    }
    if !(front >= -reach && front <= reach) || feet >= top || at.y + stand <= base {
        return None;
    }
    Some(if feet <= base + movement.step_height + 0.05 {
        "bottom"
    } else {
        "side"
    })
}

/// The nearest ladder `entity`'s capsule could attach to at `at`, with no intent asked for.
pub fn probe_ladder(
    world: &mut World,
    entity: Entity,
    movement: &CharacterMovement,
    settings: &LadderSettings,
    at: Vec3,
    skin: f32,
) -> Result<LadderProbe, TraversalRefusal> {
    if !settings.enabled {
        return Err(TraversalRefusal::TraversalDisabled);
    }
    if queries(world).is_none() {
        return Err(TraversalRefusal::TraversalNoPhysics);
    }
    let mut best: Option<(LadderKey, LadderProbe)> = None;
    for (ladder_entity, ladder, line) in ladders(world, movement, skin) {
        let Some(attach) = zone(&line, &ladder, settings, movement, at) else {
            continue;
        };
        let key = order_key(&line, at);
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((
                key,
                LadderProbe {
                    entity: ladder_entity,
                    user_id: ladder.user_id,
                    attach,
                    line,
                },
            ));
        }
    }
    let probe = best
        .map(|(_, probe)| probe)
        .ok_or(TraversalRefusal::LadderNoneInReach)?;
    let body = Body {
        entity,
        radius: movement.radius,
        half_height: movement.half_height,
    };
    let on_line = attach_point(&probe.line, at);
    if !body.fits(world, on_line) {
        return Err(TraversalRefusal::LadderNoRoom);
    }
    Ok(probe)
}

/// Where a capsule at `at` lands on the climb line.
fn attach_point(line: &LadderLine, at: Vec3) -> Vec3 {
    Vec3::new(line.line.x, at.y.clamp(line.line.y, line.top_y), line.line.z)
}

/// Attaches when the input moves into a ladder in reach (`Walking`, `Falling` or swimming). Silent:
/// a character simply walking near a ladder is no attempt, except when a ladder it moves into has no
/// room on the line, which is recorded.
pub(crate) fn try_attach(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Option<Outcome> {
    if !cx.ladder.enabled || traversal.cooldown_steps > 0 {
        return None;
    }
    let forward = intent_of(cx.input)?;
    let at = transform.translation;
    let mut best: Option<(LadderKey, LadderLine)> = None;
    for (_, ladder, line) in ladders(world, cx.movement, cx.skin) {
        let Some(attach) = zone(&line, &ladder, &cx.ladder, cx.movement, at) else {
            continue;
        };
        let toward = if attach == "top" { -line.into } else { line.into };
        if forward.dot(toward) < cx.ladder.intent {
            continue;
        }
        let key = order_key(&line, at);
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((key, line));
        }
    }
    let (_, line) = best?;
    match enter(world, cx, traversal, state, transform, line, 0) {
        Ok(outcome) => Some(outcome),
        Err(reason) => {
            if traversal.last_refusal != Some(reason) {
                refuse(world, cx.entity, traversal, reason);
            }
            None
        }
    }
}

/// Attaches for a request `(user_id, near, up)`: by `user_id`, or the nearest to `near`.
pub(crate) fn attach_by_order(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    (user_id, near, up): (u64, Vec3, bool),
) -> Result<Outcome, TraversalRefusal> {
    if queries(world).is_none() {
        return Err(TraversalRefusal::TraversalNoPhysics);
    }
    let mut best: Option<(LadderKey, LadderLine)> = None;
    for (_, ladder, line) in ladders(world, cx.movement, cx.skin) {
        if user_id != 0 && ladder.user_id != user_id {
            continue;
        }
        let key = order_key(&line, near);
        let reach = |from: Vec3| line.line.with_y(from.y).distance(from) <= ORDER_REACH;
        if reach(near) && reach(transform.translation) && best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((key, line));
        }
    }
    let (_, line) = best.ok_or(TraversalRefusal::LadderNoneInReach)?;
    enter(
        world,
        cx,
        traversal,
        state,
        transform,
        line,
        if up { 1 } else { -1 },
    )
}

fn enter(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    line: LadderLine,
    auto_climb: i8,
) -> Result<Outcome, TraversalRefusal> {
    if queries(world).is_none() {
        return Err(TraversalRefusal::TraversalNoPhysics);
    }
    let at = transform.translation;
    let on_line = attach_point(&line, at);
    if !cx.body.fits(world, on_line) {
        return Err(TraversalRefusal::LadderNoRoom);
    }
    if cx.body.sweep(world, at, on_line).is_some() {
        return Err(TraversalRefusal::LadderNoRoom);
    }
    traversal.ladder = line;
    traversal.auto_climb = auto_climb;
    traversal.depth = 0.0;
    let speed = at.distance(on_line) / cx.ladder.enter_time.max(cx.dt);
    start_curve(
        world,
        cx,
        traversal,
        TraversalKind::LadderEnter,
        [at, at, on_line, on_line],
        speed.max(1.0e-3),
        Vec::new(),
    );
    Ok(curve::step(world, cx, traversal, state, transform))
}

/// One step on the climb line.
pub(crate) fn step(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Outcome {
    let line = traversal.ladder;
    let at = transform.translation;
    let facing = facing_along(line.into);

    // Moved off the line (a respawn, a correction from the server): off the ladder, as with a curve.
    // Pulling the character back would erase the move.
    let off_line = at.with_y(0.0).distance(line.line.with_y(0.0)) > curve::OFF_CURVE;
    if off_line || at.y < line.line.y - curve::OFF_CURVE || at.y > line.top_y + curve::OFF_CURVE {
        curve::interrupt(world, cx, traversal);
        in_move_state(state, MovementMode::Falling, Vec3::ZERO);
        return Outcome::PlainWith(MovementMode::Falling);
    }

    if cx.input.jump {
        traversal.kind = TraversalKind::None;
        traversal.auto_climb = 0;
        traversal.cooldown_steps = (cx.ladder.cooldown / cx.dt).ceil() as u32;
        finished(world, cx);
        let velocity = -line.into * cx.ladder.jump_off_speed + Vec3::Y * cx.ladder.jump_off_up;
        in_move_state(state, MovementMode::Falling, velocity);
        state.jumps_used = 1;
        return Outcome::PlainWith(MovementMode::Falling);
    }

    let climb = if traversal.auto_climb != 0 {
        f32::from(traversal.auto_climb)
    } else if cx.input.crouch {
        -1.0
    } else {
        (cx.input.move_dir.dot(line.into) + cx.input.move_dir.y).clamp(-1.0, 1.0)
    };
    let wanted = (at.y + climb * line.speed * cx.dt).clamp(line.line.y, line.top_y);
    let target = Vec3::new(line.line.x, wanted, line.line.z);
    let blocked = cx.body.sweep(world, at, target);
    let moved = match blocked {
        Some(free) => at + (target - at).normalize_or_zero() * (free - cx.skin).max(0.0),
        None => target,
    };

    // Down at the base, or stopped by the ground: walk away.
    if climb < 0.0 && (blocked.is_some() || moved.y <= line.line.y + END_TOLERANCE) {
        traversal.kind = TraversalKind::None;
        traversal.auto_climb = 0;
        finished(world, cx);
        let result = settle(world, cx, moved);
        let mode = if result.grounded {
            MovementMode::Walking
        } else {
            MovementMode::Falling
        };
        in_move_state(state, mode, Vec3::ZERO);
        state.grounded = result.grounded;
        let position = result.position;
        return Outcome::Moved(result, position, facing, mode);
    }

    // Up at the top: the exit needs room, a clear way and a floor.
    if climb > 0.0 && moved.y >= line.top_y - END_TOLERANCE {
        match exit_check(world, cx, moved, line.exit) {
            Ok(()) => {
                traversal.last_refusal = None;
                start_curve(
                    world,
                    cx,
                    traversal,
                    TraversalKind::LadderExit,
                    [moved, moved, line.exit, line.exit],
                    cx.ladder.exit_speed,
                    Vec::new(),
                );
                let there = Transform {
                    translation: moved,
                    ..*transform
                };
                return curve::step(world, cx, traversal, state, &there);
            }
            Err(reason) => {
                if traversal.last_refusal != Some(reason) {
                    refuse(world, cx.entity, traversal, reason);
                }
            }
        }
    }

    // Held under something on the way up: say so, a self-driven climb has no player to notice.
    if climb > 0.0 && blocked.is_some() && traversal.last_refusal != Some(TraversalRefusal::LadderExitBlocked)
    {
        refuse(world, cx.entity, traversal, TraversalRefusal::LadderExitBlocked);
    }

    in_move_state(state, MovementMode::CLIMBING, (moved - at) / cx.dt);
    clear_body_ground(world, cx.entity);
    Outcome::Moved(free_result(moved), moved, facing, MovementMode::CLIMBING)
}

fn finished(world: &mut World, cx: &StepCx<'_>) {
    send_event(
        world,
        TraversalEvent::Finished {
            entity: cx.entity,
            kind: TraversalKind::Ladder,
        },
    );
}

/// Clear space, a clear way and a floor at the top exit.
fn exit_check(world: &World, cx: &StepCx<'_>, from: Vec3, exit: Vec3) -> Result<(), TraversalRefusal> {
    let Some(queries) = queries(world) else {
        return Err(TraversalRefusal::TraversalNoPhysics);
    };
    if !cx.body.fits(world, exit) || cx.body.sweep(world, from, exit).is_some() {
        return Err(TraversalRefusal::LadderExitBlocked);
    }
    let exclude = [cx.entity];
    let filter = regia_physics::QueryFilter::new()
        .exclude_sensors()
        .exclude_entities(&exclude);
    let stand = cx.movement.half_height + cx.movement.radius;
    let reach = stand + cx.skin + cx.movement.step_height;
    match queries.raycast(exit, Vec3::NEG_Y, reach, filter) {
        Some(_) => Ok(()),
        None => Err(TraversalRefusal::LadderExitNoFloor),
    }
}
