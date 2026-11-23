// SPDX-License-Identifier: MPL-2.0
//! The ledge test (spec § 5): is there a wall, where is its top, can the capsule stand there and get
//! there. A pure read of the world: the same world and capsule give the same answer.

use regia_animation::WarpTarget;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{QueryFilter, WorldQueries};
use regia_scene::Transform;

use super::{
    Body, CharacterTraversal, MantleSettings, Outcome, StepCx, TraversalKind, TraversalRefusal,
    TraversalWarpTarget, curve, facing_along, queries, start_curve,
};
use crate::character::{CharacterMovement, CharacterState};

/// The wall cast clears a step by this much more (m).
const STEP_MARGIN: f32 = 0.02;
/// Rows of the scan for a wall's top (m).
const SCAN_STEP: f32 = 0.1;
/// The scan ray starts this far in front of the contact (m).
const SCAN_BACK: f32 = 0.05;
/// Columns of the search for a thin wall's far face (m).
const THICKNESS_STEP: f32 = 0.05;
/// The smallest cosine between the intent and the wall's inward direction.
const MIN_FACING: f32 = 0.3;

/// A ledge a capsule can use: what the probe found and the path it would follow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LedgeProbe {
    /// [`TraversalKind::Mantle`] or [`TraversalKind::Vault`].
    pub kind: TraversalKind,
    /// The ledge's height above the feet (m).
    pub height: f32,
    /// The point on the top edge.
    pub edge: Vec3,
    /// The capsule center where the move ends.
    pub stand: Vec3,
    /// The top surface's normal.
    pub normal: Vec3,
    /// The horizontal direction of the move.
    pub into: Vec3,
    /// The curve's control points.
    pub path: [Vec3; 4],
}

fn solid_filter<'a>(not_own: &'a dyn Fn(Entity) -> bool) -> QueryFilter<'a> {
    QueryFilter::new().exclude_sensors().predicate(not_own)
}

/// The angle between a surface normal and world up.
fn slope_of(normal: Vec3) -> f32 {
    // Deterministic `acos`: the result decides whether a ledge is usable (sim state).
    regia_core::math::det_acos(normal.y.clamp(-1.0, 1.0))
}

/// Whether a surface at `y` lies within `reach` under the point, and its normal.
fn floor_within(
    q: &WorldQueries<'_>,
    not_own: &dyn Fn(Entity) -> bool,
    from: Vec3,
    reach: f32,
) -> Option<(f32, Vec3)> {
    q.raycast(from, Vec3::NEG_Y, reach, solid_filter(not_own))
        .map(|hit| (hit.point.y, hit.normal))
}

/// Finds a ledge for `entity`'s capsule at `position`, looking along `forward` (only its horizontal
/// part counts). `Err` says why not.
///
/// # Errors
///
/// The [`TraversalRefusal`] of spec § 5, in the order the spec lists the tests.
pub fn probe_ledge(
    world: &World,
    entity: Entity,
    movement: &CharacterMovement,
    mantle: &MantleSettings,
    position: Vec3,
    forward: Vec3,
    skin: f32,
) -> Result<LedgeProbe, TraversalRefusal> {
    if !mantle.enabled {
        return Err(TraversalRefusal::TraversalDisabled);
    }
    let Some(q) = queries(world) else {
        return Err(TraversalRefusal::TraversalNoPhysics);
    };
    let forward = forward.with_y(0.0);
    if !position.is_finite() || !forward.is_finite() || forward.length_squared() < 1.0e-6 {
        return Err(TraversalRefusal::TraversalBadRequest);
    }
    let forward = forward.normalize();
    let (radius, half_height) = (movement.radius, movement.half_height);
    let body = Body {
        entity,
        radius,
        half_height,
    };
    let stand_height = half_height + radius;
    let feet = position.y - stand_height;
    let not_own = |other: Entity| !super::is_own(world, entity, other);

    // 1. The wall: a capsule cast ahead, over anything the character could step on.
    // Lifted by shortening it from below: its top stays where the head is, so a low ceiling over the
    // character reads as no headroom later, not as a wall here.
    let lift = movement.step_height + STEP_MARGIN;
    let cast_from = position + Vec3::Y * (lift * 0.5);
    let hit = q
        .shapecast(
            &super::sweep_shape(radius, half_height - lift * 0.5),
            Transform::from_translation(cast_from),
            forward,
            mantle.reach,
            solid_filter(&not_own),
        )
        .ok_or(TraversalRefusal::LedgeNoWall)?;
    if slope_of(hit.normal) <= movement.max_slope_angle {
        return Err(TraversalRefusal::LedgeNoWall);
    }
    let into = Vec3::new(-hit.normal.x, 0.0, -hit.normal.z).normalize_or_zero();
    if into == Vec3::ZERO || into.dot(forward) < MIN_FACING {
        return Err(TraversalRefusal::LedgeNoWall);
    }

    // 2. The top: the first height where the column at the contact is free, then a ray down from
    // there. Scanning from below finds the ledge under an overhang, which the headroom test judges.
    let column = hit.point.with_y(0.0) + into * mantle.edge_inset;
    let scan_back = into * (SCAN_BACK + mantle.edge_inset);
    let scan_length = SCAN_BACK + mantle.edge_inset + STEP_MARGIN;
    let scan_top = feet + mantle.max_height + SCAN_STEP;
    let mut y = hit.point.y;
    let mut gap = None;
    while y <= scan_top {
        let origin = column - scan_back + Vec3::Y * y;
        if q.raycast(origin, into, scan_length, solid_filter(&not_own))
            .is_none()
        {
            gap = Some(y);
            break;
        }
        y += SCAN_STEP;
    }
    let gap = gap.ok_or(TraversalRefusal::LedgeTooHigh)?;
    let (top_y, normal) = floor_within(
        &q,
        &not_own,
        column + Vec3::Y * (gap + STEP_MARGIN),
        gap - feet + 2.0 * STEP_MARGIN,
    )
    .ok_or(TraversalRefusal::LedgeTooLow)?;
    // 3. Slope comes before the range: on a steep face the "top" is only where the column met the
    // face, so its height says nothing.
    if slope_of(normal) > mantle.max_slope {
        return Err(TraversalRefusal::LedgeTooSteep);
    }
    let height = top_y - feet;
    if height > mantle.max_height {
        return Err(TraversalRefusal::LedgeTooHigh);
    }
    if height < mantle.min_height {
        return Err(TraversalRefusal::LedgeTooLow);
    }

    let edge = column.with_y(top_y) - into * mantle.edge_inset;
    let stand_y = top_y + stand_height + skin;
    let pass_y = stand_y + mantle.clearance;
    let rise = Vec3::new(position.x, pass_y.max(position.y), position.z);

    // 4. Depth: the top goes on, or this is a thin wall.
    let depth_from =
        column + into * mantle.min_depth + Vec3::Y * (top_y + movement.step_height + STEP_MARGIN);
    let depth_reach = 2.0 * movement.step_height + STEP_MARGIN;
    if floor_within(&q, &not_own, depth_from, depth_reach).is_none() {
        let top = FoundTop {
            position,
            rise,
            column,
            edge,
            into,
            height,
            top_y,
            normal,
        };
        return vault(world, &q, &body, movement, mantle, &top, skin);
    }

    // 5. Room: the capsule fits at the stand point and on the way there.
    let stand = (edge + into * (radius + mantle.stand_inset + skin)).with_y(stand_y);
    let over = stand.with_y(rise.y);
    if !body.fits(world, stand) {
        return Err(TraversalRefusal::LedgeNoHeadroom);
    }
    if body.sweep(world, position, rise).is_some()
        || body.sweep(world, rise, over).is_some()
        || body.sweep(world, over, stand).is_some()
    {
        return Err(TraversalRefusal::LedgeNoHeadroom);
    }
    Ok(LedgeProbe {
        kind: TraversalKind::Mantle,
        height,
        edge,
        stand,
        normal,
        into,
        path: [position, rise, over, stand],
    })
}

/// What the probe knows when the wall turns out thin.
struct FoundTop {
    position: Vec3,
    rise: Vec3,
    column: Vec3,
    edge: Vec3,
    into: Vec3,
    height: f32,
    top_y: f32,
    normal: Vec3,
}

/// The thin-wall branch of the probe: a vault when the rules allow it, else the reason.
fn vault(
    world: &World,
    q: &WorldQueries<'_>,
    body: &Body,
    movement: &CharacterMovement,
    mantle: &MantleSettings,
    top: &FoundTop,
    skin: f32,
) -> Result<LedgeProbe, TraversalRefusal> {
    let FoundTop {
        position,
        rise,
        column,
        edge,
        into,
        height,
        top_y,
        normal,
    } = *top;
    if !mantle.vault_enabled || height > mantle.vault_max_height {
        return Err(TraversalRefusal::LedgeTooThin);
    }
    let not_own = |other: Entity| !super::is_own(world, body.entity, other);
    // The far face: the first column where the top is gone.
    let mut thickness = None;
    let mut across = THICKNESS_STEP;
    while across <= mantle.vault_max_thickness + THICKNESS_STEP * 0.5 {
        let from = (column + into * across).with_y(top_y + STEP_MARGIN);
        if floor_within(q, &not_own, from, 2.0 * STEP_MARGIN).is_none() {
            thickness = Some(across);
            break;
        }
        across += THICKNESS_STEP;
    }
    let thickness = thickness.ok_or(TraversalRefusal::LedgeTooThin)?;

    let stand_height = movement.half_height + movement.radius;
    let feet = position.y - stand_height;
    let land_at = column + into * (thickness + movement.radius + mantle.stand_inset + skin);
    let reach = (top_y - feet) + mantle.vault_max_drop;
    let (land_y, land_normal) =
        floor_within(q, &not_own, land_at.with_y(top_y), reach).ok_or(TraversalRefusal::VaultNoLanding)?;
    if slope_of(land_normal) > movement.max_slope_angle {
        return Err(TraversalRefusal::VaultNoLanding);
    }
    let land = land_at.with_y(land_y + stand_height + skin);
    if !body.fits(world, land) {
        return Err(TraversalRefusal::VaultNoLanding);
    }
    let over = land.with_y(rise.y);
    if body.sweep(world, position, rise).is_some()
        || body.sweep(world, rise, over).is_some()
        || body.sweep(world, over, land).is_some()
    {
        return Err(TraversalRefusal::LedgeNoHeadroom);
    }
    Ok(LedgeProbe {
        kind: TraversalKind::Vault,
        height,
        edge,
        stand: land,
        normal,
        into,
        path: [position, rise, over, land],
    })
}

/// The probe with one step's character.
pub(crate) fn probe_for_step(
    world: &World,
    cx: &StepCx<'_>,
    position: Vec3,
    forward: Vec3,
) -> Result<LedgeProbe, TraversalRefusal> {
    probe_ledge(
        world,
        cx.entity,
        cx.movement,
        &cx.mantle,
        position,
        forward,
        cx.skin,
    )
}

/// Starts the move `probe` found.
pub(crate) fn start(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    probe: &LedgeProbe,
) -> Outcome {
    let facing = Some(facing_along(probe.into));
    let (first, second, speed) = match probe.kind {
        TraversalKind::Vault => ("vault_over", "vault_land", cx.mantle.vault_speed),
        _ => ("ledge_edge", "ledge_stand", cx.mantle.speed),
    };
    let targets = vec![
        TraversalWarpTarget {
            name: first,
            target: WarpTarget {
                position: probe.edge,
                facing,
            },
        },
        TraversalWarpTarget {
            name: second,
            target: WarpTarget {
                position: probe.stand,
                facing,
            },
        },
    ];
    traversal.depth = 0.0;
    start_curve(world, cx, traversal, probe.kind, probe.path, speed, targets);
    curve::step(world, cx, traversal, state, transform)
}
