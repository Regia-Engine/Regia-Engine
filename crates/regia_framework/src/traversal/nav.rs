// SPDX-License-Identifier: MPL-2.0
//! The nav bridge (spec § 7): ladders and jump points publish [`NavLinkAnchor`]s, and a crowd agent
//! taking off on a link becomes a [`TraversalRequest`], so AI uses the moves a player has.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_nav::{CrowdAgentId, NavCrowd, NavCrowdStep, NavLinkAnchor, NavLinkKind, NavLinkPhase};

use regia_scene::{GlobalTransform, Transform};

use super::{Ladder, TraversalJumpPoint, TraversalOrder, TraversalRequest};

/// How far above the higher end a jump passes when no [`TraversalJumpPoint`] says (m).
const DEFAULT_ARC_HEIGHT: f32 = 0.5;

/// Ties a character to the crowd agent that steers it: the character takes the agent's crossings.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraversalCrowdAgent(pub CrowdAgentId);

/// The anchor a ladder publishes, or `None` when it publishes none.
pub fn ladder_anchor(ladder: &Ladder) -> Option<NavLinkAnchor> {
    if !ladder.nav_link || !ladder.enabled || ladder.validate().is_err() {
        return None;
    }
    Some(NavLinkAnchor {
        bidirectional: true,
        user_id: ladder.user_id,
        kind: NavLinkKind::Climb,
        duration: ladder.height / ladder.climb_speed,
        ..NavLinkAnchor::new(Vec3::new(0.0, ladder.height, -ladder.exit_forward))
    })
}

/// The anchor a jump point publishes, or `None` when it publishes none.
pub fn jump_point_anchor(point: &TraversalJumpPoint) -> Option<NavLinkAnchor> {
    if !point.enabled || point.validate().is_err() {
        return None;
    }
    Some(NavLinkAnchor {
        bidirectional: point.bidirectional,
        user_id: point.user_id,
        kind: NavLinkKind::Jump,
        ..NavLinkAnchor::new(point.to)
    })
}

/// The `arc_height` of the jump point a crossing from `start` to `end` with `user_id` belongs to.
///
/// Several points may share a `user_id` (0 is "unnamed"), so the point whose published link
/// lies nearest the crossing wins: the smallest summed squared distance from its take-off and
/// landing to the crossing's ends (either way round for a two-way point). Points that publish no
/// link (disabled or invalid) never count. Ties go to the earlier point of `points`, which the
/// caller orders deterministically. `DEFAULT_ARC_HEIGHT` when no point matches.
pub(crate) fn crossing_arc_height<'a>(
    points: impl IntoIterator<Item = (&'a TraversalJumpPoint, Affine3A)>,
    user_id: u64,
    start: Vec3,
    end: Vec3,
) -> f32 {
    let mut best: Option<(f32, f32)> = None;
    for (point, pose) in points {
        if point.user_id != user_id || jump_point_anchor(point).is_none() {
            continue;
        }
        let from = pose.transform_point3(Vec3::ZERO);
        let to = pose.transform_point3(point.to);
        let mut cost = from.distance_squared(start) + to.distance_squared(end);
        if point.bidirectional {
            cost = cost.min(from.distance_squared(end) + to.distance_squared(start));
        }
        if best.is_none_or(|(best_cost, _)| cost.total_cmp(&best_cost).is_lt()) {
            best = Some((cost, point.arc_height));
        }
    }
    best.map_or(DEFAULT_ARC_HEIGHT, |(_, arc_height)| arc_height)
}

/// The move a request asks for; `None` for a link kind no move answers (a teleport, a game's own).
pub fn order_for(request: &TraversalRequest) -> Option<TraversalOrder> {
    match request.kind {
        NavLinkKind::Climb => Some(TraversalOrder::Ladder {
            user_id: request.user_id,
            near: request.start,
            up: request.end.y >= request.start.y,
        }),
        NavLinkKind::Jump | NavLinkKind::Drop => Some(TraversalOrder::Jump {
            to: request.end,
            arc_height: request.arc_height,
        }),
        NavLinkKind::Teleport | NavLinkKind::Custom(_) => None,
    }
}

/// Publishes ladders and jump points as nav links and turns crowd take-offs into
/// [`TraversalRequest`]s. Opt-in; add with `regia_nav::NavAuthoredLinksPlugin`,
/// `regia_nav::NavCrowdPlugin` and [`TraversalPlugin`](super::TraversalPlugin).
pub struct TraversalNavPlugin;

impl Plugin for TraversalNavPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<TraversalRequest>().add_systems(
            FixedUpdate,
            (
                publish_link_anchors.before(NavCrowdStep),
                forward_crowd_crossings
                    .after(NavCrowdStep)
                    .before(super::apply_traversal_requests),
            ),
        );
    }
}

/// Keeps each source's anchor equal to what it publishes.
fn publish_link_anchors(
    mut commands: Commands,
    ladders: Query<(Entity, &Ladder, Option<&NavLinkAnchor>)>,
    points: Query<(Entity, &TraversalJumpPoint, Option<&NavLinkAnchor>), Without<Ladder>>,
) {
    let wanted = ladders
        .iter()
        .map(|(entity, ladder, anchor)| (entity, ladder_anchor(ladder), anchor))
        .chain(
            points
                .iter()
                .map(|(entity, point, anchor)| (entity, jump_point_anchor(point), anchor)),
        );
    for (entity, want, have) in wanted {
        match (want, have) {
            (Some(want), have) if have != Some(&want) => {
                commands.entity(entity).insert(want);
            }
            (None, Some(_)) => {
                commands.entity(entity).remove::<NavLinkAnchor>();
            }
            _ => {}
        }
    }
}

/// What [`forward_crowd_crossings`] reads of a jump point's entity: its pose as the authored-links
/// plugin reads it (a root's `Transform`, a child's `GlobalTransform`).
type JumpPointItem = (
    Entity,
    &'static TraversalJumpPoint,
    Option<&'static Transform>,
    Option<&'static GlobalTransform>,
    Has<ChildOf>,
);

/// `FixedUpdate`, after the crowd step: one request per take-off of a link a move can use.
fn forward_crowd_crossings(
    crowd: Option<Res<NavCrowd>>,
    agents: Query<(Entity, &TraversalCrowdAgent)>,
    points: Query<JumpPointItem>,
    mut requests: MessageWriter<TraversalRequest>,
) {
    let Some(crowd) = crowd else {
        return;
    };
    if crowd.link_events().is_empty() {
        return;
    }
    let by_agent: BTreeMap<u64, Entity> = agents.iter().map(|(entity, agent)| (agent.0.0, entity)).collect();
    // Poses as the authored-links plugin reads them (a root's `Transform`, a child's
    // `GlobalTransform`), in entity order so a tie never depends on query order.
    let mut poses: Vec<(Entity, &TraversalJumpPoint, Affine3A)> = points
        .iter()
        .map(|(entity, point, transform, global, child)| {
            let pose = match (child, global) {
                (true, Some(global)) => global.0,
                _ => transform.map_or(Affine3A::IDENTITY, Transform::compute_affine),
            };
            (entity, point, pose)
        })
        .collect();
    poses.sort_by_key(|(entity, ..)| *entity);
    for event in crowd.link_events() {
        if event.phase != NavLinkPhase::Started
            || matches!(event.kind, NavLinkKind::Teleport | NavLinkKind::Custom(_))
        {
            continue;
        }
        let (start, end) = crowd
            .agent(event.agent)
            .and_then(|agent| agent.link_traversal())
            .map_or((event.position, event.position), |t| (t.start, t.end));
        requests.write(TraversalRequest {
            agent: Some(event.agent.0),
            entity: by_agent.get(&event.agent.0).copied(),
            kind: event.kind,
            user_id: event.user_id,
            start,
            end,
            arc_height: if event.kind == NavLinkKind::Jump {
                crossing_arc_height(
                    poses.iter().map(|(_, point, pose)| (*point, *pose)),
                    event.user_id,
                    start,
                    end,
                )
            } else {
                DEFAULT_ARC_HEIGHT
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ladder_publishes_a_climb_link_unless_it_opts_out_or_is_invalid() {
        let ladder = Ladder {
            user_id: 9,
            height: 3.0,
            climb_speed: 1.5,
            exit_forward: 0.6,
            ..Ladder::default()
        };
        let anchor = ladder_anchor(&ladder).expect("publishes");
        assert_eq!(anchor.kind, NavLinkKind::Climb);
        assert_eq!(anchor.user_id, 9);
        assert!(anchor.bidirectional);
        assert_eq!(anchor.to, Vec3::new(0.0, 3.0, -0.6));
        assert_eq!(anchor.duration, 2.0);
        for quiet in [
            Ladder {
                nav_link: false,
                ..ladder
            },
            Ladder {
                enabled: false,
                ..ladder
            },
            Ladder {
                height: f32::NAN,
                ..ladder
            },
        ] {
            assert!(ladder_anchor(&quiet).is_none(), "{quiet:?}");
        }
    }

    #[test]
    fn a_crossing_takes_the_arc_of_its_own_jump_point() {
        // Two unnamed points (user_id 0) with different arcs, plus a disabled one right on the
        // crossing: each crossing must take the arc of the point whose link it follows.
        let low = TraversalJumpPoint {
            to: Vec3::new(0.0, 0.0, -3.0),
            arc_height: 0.3,
            ..TraversalJumpPoint::default()
        };
        let high = TraversalJumpPoint {
            arc_height: 2.0,
            ..low
        };
        let off = TraversalJumpPoint {
            arc_height: 9.0,
            enabled: false,
            ..low
        };
        let at = |x: f32| Affine3A::from_translation(Vec3::new(x, 0.0, 0.0));
        let points = [(&off, at(10.0)), (&low, at(0.0)), (&high, at(10.0))];
        let crossing =
            |x: f32| crossing_arc_height(points, 0, Vec3::new(x, 0.0, 0.0), Vec3::new(x, 0.0, -3.0));
        assert_eq!(crossing(0.0), 0.3);
        assert_eq!(crossing(10.0), 2.0);
        // Another id, or none at all: the default.
        assert_eq!(
            crossing_arc_height(points, 7, Vec3::ZERO, Vec3::Z),
            DEFAULT_ARC_HEIGHT
        );
    }

    #[test]
    fn a_jump_point_publishes_a_jump_link() {
        let point = TraversalJumpPoint {
            to: Vec3::new(0.0, 1.0, -4.0),
            user_id: 5,
            ..TraversalJumpPoint::default()
        };
        let anchor = jump_point_anchor(&point).expect("publishes");
        assert_eq!((anchor.kind, anchor.user_id), (NavLinkKind::Jump, 5));
        assert!(!anchor.bidirectional);
        let bad = TraversalJumpPoint {
            arc_height: f32::INFINITY,
            ..point
        };
        assert!(jump_point_anchor(&bad).is_none());
    }

    #[test]
    fn requests_map_to_orders() {
        let request = TraversalRequest {
            agent: Some(1),
            entity: None,
            kind: NavLinkKind::Climb,
            user_id: 4,
            start: Vec3::ZERO,
            end: Vec3::Y * 3.0,
            arc_height: 0.5,
        };
        assert!(matches!(
            order_for(&request),
            Some(TraversalOrder::Ladder {
                user_id: 4,
                up: true,
                ..
            })
        ));
        let down = TraversalRequest {
            start: Vec3::Y * 3.0,
            end: Vec3::ZERO,
            ..request
        };
        assert!(matches!(
            order_for(&down),
            Some(TraversalOrder::Ladder { up: false, .. })
        ));
        let jump = TraversalRequest {
            kind: NavLinkKind::Jump,
            ..request
        };
        assert!(matches!(order_for(&jump), Some(TraversalOrder::Jump { .. })));
        let teleport = TraversalRequest {
            kind: NavLinkKind::Teleport,
            ..request
        };
        assert_eq!(order_for(&teleport), None);
    }
}
