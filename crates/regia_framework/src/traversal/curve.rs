// SPDX-License-Identifier: MPL-2.0
//! The curve every scripted move follows (spec § 5): four control points, travelled by arc length
//! with a smoothstep ease over whole fixed steps, swept each step so a moved capsule never ends up
//! in geometry.

use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

use super::{
    CharacterTraversal, Outcome, StepCx, TraversalEvent, TraversalKind, TraversalRefusal, clear_body_ground,
    facing_along, free_result, in_move_state, send_event, start_curve,
};
use crate::character::{CharacterState, MoveResult, MovementMode};

/// How far from a corner the path starts turning (m). At most the mantle's clearance, so a rounded
/// corner stays above the stand height.
pub(crate) const CORNER_ROUND: f32 = 0.04;
/// A segment shorter than this is no segment (m).
const MIN_SEGMENT: f32 = 1.0e-5;
/// A character further than this from where its curve or ladder left it was moved by someone else (m).
/// Above what a position from the wire can differ by.
pub(crate) const OFF_CURVE: f32 = 0.05;

fn lengths(path: &[Vec3; 4]) -> [f32; 3] {
    [
        path[0].distance(path[1]),
        path[1].distance(path[2]),
        path[2].distance(path[3]),
    ]
}

/// The fixed steps a path takes at `speed` (at least one).
pub(crate) fn steps_for(path: &[Vec3; 4], speed: f32, dt: f32) -> u32 {
    let total: f32 = lengths(path).iter().sum();
    let steps = (total / speed.max(1.0e-3) / dt.max(1.0e-6)).ceil();
    if steps.is_finite() && steps >= 1.0 {
        steps.min(u32::MAX as f32) as u32
    } else {
        1
    }
}

/// The point at `s` (0..=1) of the arc length along the polyline `path`, its inner corners rounded by a
/// quadratic blend no larger than `CORNER_ROUND` (half a short segment at most). A pure function
/// of its arguments: a re-run of a tick lands on the same bits.
pub fn path_point(path: &[Vec3; 4], s: f32) -> Vec3 {
    let parts = lengths(path);
    let total: f32 = parts.iter().sum();
    if total <= MIN_SEGMENT {
        return path[3];
    }
    let at = s.clamp(0.0, 1.0) * total;
    // The corners with their rounding: a corner joins the two nearest real segments.
    let mut run = 0.0;
    for corner in 1..=2 {
        run += parts[corner - 1];
        let (before, after) = (parts[corner - 1], parts[corner]);
        if before <= MIN_SEGMENT || after <= MIN_SEGMENT {
            continue;
        }
        let round = CORNER_ROUND.min(before * 0.5).min(after * 0.5);
        if round > MIN_SEGMENT && (at - run).abs() < round {
            let c = path[corner];
            let a = c + (path[corner - 1] - c) / before * round;
            let b = c + (path[corner + 1] - c) / after * round;
            let t = (at - (run - round)) / (2.0 * round);
            let u = 1.0 - t;
            return a * (u * u) + c * (2.0 * t * u) + b * (t * t);
        }
    }
    let mut left = at;
    for (i, part) in parts.iter().enumerate() {
        if left <= *part && *part > MIN_SEGMENT {
            return path[i].lerp(path[i + 1], left / part);
        }
        left -= part;
    }
    path[3]
}

/// Smoothstep: starts and ends at rest.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Starts the jump curve to `to`, a point on the landing floor.
pub(crate) fn start_jump(
    world: &mut regia_app::prelude::World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    to: Vec3,
    arc_height: f32,
) -> Outcome {
    let start = transform.translation;
    let stand = to + Vec3::Y * (cx.movement.half_height + cx.movement.radius + cx.skin);
    let top = start.y.max(stand.y) + arc_height;
    let path = [
        start,
        Vec3::new(start.x, top, start.z),
        Vec3::new(stand.x, top, stand.z),
        stand,
    ];
    start_curve(
        world,
        cx,
        traversal,
        TraversalKind::Jump,
        path,
        cx.mantle.speed,
        Vec::new(),
    );
    step(world, cx, traversal, state, transform)
}

/// One step of a running curve.
pub(crate) fn step(
    world: &mut regia_app::prelude::World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Outcome {
    let kind = traversal.kind;
    let from = transform.translation;

    // Moved off the curve (a shove, a correction from the server): the move is over. Pulling the
    // capsule back would erase the move in one step, and it would never reach the owning client.
    let expected = path_point(
        &traversal.path,
        ease(traversal.step as f32 / traversal.steps.max(1) as f32),
    );
    if !expected.is_finite() || from.distance(expected) > OFF_CURVE {
        interrupt(world, cx, traversal);
        in_move_state(state, MovementMode::Falling, Vec3::ZERO);
        return Outcome::PlainWith(MovementMode::Falling);
    }

    traversal.step = (traversal.step + 1).min(traversal.steps.max(1));
    let t = traversal.step as f32 / traversal.steps.max(1) as f32;
    let target = path_point(&traversal.path, ease(t));
    let facing = move_facing(traversal, transform);

    // A sweep to the next point: a hit ends the move where the capsule still is free, whatever
    // moved it off the curve (a shove, a correction).
    if let Some(free) = cx.body.sweep(world, from, target) {
        let along = (target - from).normalize_or_zero();
        let stopped = from + along * (free - cx.skin).max(0.0);
        interrupt(world, cx, traversal);
        in_move_state(state, MovementMode::Falling, Vec3::ZERO);
        clear_body_ground(world, cx.entity);
        return Outcome::Moved(free_result(stopped), stopped, facing, MovementMode::Falling);
    }

    if traversal.step < traversal.steps {
        let velocity = (target - from) / cx.dt;
        in_move_state(state, MovementMode::TRAVERSING, velocity);
        clear_body_ground(world, cx.entity);
        return Outcome::Moved(free_result(target), target, facing, MovementMode::TRAVERSING);
    }

    // The end of the curve.
    traversal.step = 0;
    traversal.steps = 0;
    if kind == TraversalKind::LadderEnter {
        send_event(
            world,
            TraversalEvent::Finished {
                entity: cx.entity,
                kind,
            },
        );
        traversal.kind = TraversalKind::Ladder;
        started_ladder(world, cx);
        in_move_state(state, MovementMode::CLIMBING, Vec3::ZERO);
        clear_body_ground(world, cx.entity);
        return Outcome::Moved(free_result(target), target, facing, MovementMode::CLIMBING);
    }
    traversal.kind = TraversalKind::None;
    traversal.auto_climb = 0;
    send_event(
        world,
        TraversalEvent::Finished {
            entity: cx.entity,
            kind,
        },
    );
    let result = settle(world, cx, target);
    let mode = if result.grounded {
        MovementMode::Walking
    } else {
        MovementMode::Falling
    };
    in_move_state(state, mode, Vec3::ZERO);
    state.grounded = result.grounded;
    let position = result.position;
    Outcome::Moved(result, position, facing, mode)
}

/// Tells the host the climb itself began, after the step onto the ladder.
fn started_ladder(world: &mut regia_app::prelude::World, cx: &StepCx<'_>) {
    let slot = world
        .get::<super::TraversalSettings>(cx.entity)
        .map(|s| s.montage_slot.clone())
        .unwrap_or_default();
    send_event(
        world,
        TraversalEvent::Started {
            entity: cx.entity,
            kind: TraversalKind::Ladder,
            slot,
            montage: TraversalKind::Ladder.name(),
            duration: 0.0,
            warp_targets: Vec::new(),
        },
    );
}

/// Ends a running curve early and reports it.
pub(crate) fn interrupt(
    world: &mut regia_app::prelude::World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
) {
    send_event(
        world,
        TraversalEvent::Finished {
            entity: cx.entity,
            kind: traversal.kind,
        },
    );
    traversal.kind = TraversalKind::None;
    traversal.step = 0;
    traversal.steps = 0;
    traversal.auto_climb = 0;
    traversal.last_refusal = Some(TraversalRefusal::TraversalInterrupted);
    send_event(
        world,
        TraversalEvent::Refused {
            entity: cx.entity,
            reason: TraversalRefusal::TraversalInterrupted,
        },
    );
}

/// One sweep with snap: ends a move on the ground, with the ground reported as usual.
pub(crate) fn settle(world: &mut regia_app::prelude::World, cx: &StepCx<'_>, at: Vec3) -> MoveResult {
    cx.collision.move_shape(
        world,
        cx.entity,
        at,
        cx.movement.radius,
        cx.movement.half_height,
        Vec3::ZERO,
        cx.dt,
        cx.dt,
        cx.movement.max_slope_angle,
        cx.movement.step_height,
        cx.movement.snap_to_ground_distance,
    )
}

/// The facing during a curve: along its horizontal travel, or the ladder's when on one.
fn move_facing(traversal: &CharacterTraversal, transform: &Transform) -> Quat {
    if matches!(traversal.kind, TraversalKind::LadderEnter) {
        return facing_along(traversal.ladder.into);
    }
    let travel = traversal.path[3] - traversal.path[0];
    let horizontal = Vec3::new(travel.x, 0.0, travel.z);
    if horizontal.length_squared() > 1.0e-6 {
        facing_along(horizontal.normalize())
    } else {
        transform.rotation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mantle_path() -> [Vec3; 4] {
        [
            Vec3::new(0.0, 1.2, 0.0),
            Vec3::new(0.0, 2.3, 0.0),
            Vec3::new(0.0, 2.3, -0.7),
            Vec3::new(0.0, 2.25, -0.7),
        ]
    }

    #[test]
    fn the_path_runs_from_end_to_end_and_never_cuts_under_the_stand_height() {
        let path = mantle_path();
        assert_eq!(path_point(&path, 0.0), path[0]);
        assert!(path_point(&path, 1.0).distance(path[3]) < 1.0e-5);
        for i in 0..=1000 {
            let p = path_point(&path, i as f32 / 1000.0);
            // Moving forward (z < 0) happens only above the stand height.
            if p.z < -1.0e-5 {
                assert!(p.y >= path[3].y - 1.0e-4, "{p}");
            }
        }
    }

    #[test]
    fn the_path_is_continuous() {
        let path = mantle_path();
        let total: f32 = lengths(&path).iter().sum();
        let mut last = path_point(&path, 0.0);
        for i in 1..=2000 {
            let p = path_point(&path, i as f32 / 2000.0);
            assert!(
                p.distance(last) <= 2.0 * total / 2000.0 + 1.0e-4,
                "{i}: {last} -> {p}"
            );
            last = p;
        }
    }

    #[test]
    fn degenerate_paths_do_not_divide_by_zero() {
        let same = [Vec3::ONE; 4];
        assert_eq!(path_point(&same, 0.5), Vec3::ONE);
        let straight = [Vec3::ZERO, Vec3::ZERO, Vec3::X, Vec3::X];
        assert!(path_point(&straight, 0.5).distance(Vec3::X * 0.5) < 1.0e-5);
        assert_eq!(steps_for(&same, 1.0, 1.0 / 60.0), 1);
        assert_eq!(steps_for(&straight, 1.0, 0.25), 4);
    }
}
