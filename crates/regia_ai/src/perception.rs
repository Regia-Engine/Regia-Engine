// SPDX-License-Identifier: MPL-2.0
//! Perception helpers (`docs/specs/22-tier2.md` § T2.2): a reflected [`Perception`] component
//! (sight radius and cone, hearing radius, team), line-of-sight raycasts through the physics
//! world, a sight-cone test, and a [`HearingEvent`] message for sound cues.
//! [`crate::perception_system::PerceptionPlugin`] turns these predicates into a per-agent memory.
//! [`draw_perception`] draws the senses as a gizmo debug overlay
//! (`regia_render::gizmos::AiPerceptionGizmos`, off by default; feature `debug_draw`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::attrs::Range;
use regia_physics::{QueryFilter, RaycastHit, WorldQueries};
#[cfg(feature = "debug_draw")]
use regia_render::gizmos::{AiPerceptionGizmos, GizmoBuffer, GizmoConfigGroup, Gizmos};
#[cfg(feature = "debug_draw")]
use regia_scene::GlobalTransform;
#[cfg(feature = "debug_draw")]
use regia_scene::layers::LayerParked;

/// The senses of an AI agent (specs/22 § T2.2: sight cone + line of sight, hearing, team).
///
/// `sight_lose_radius` and `sight_lose_half_angle` are the cone that keeps a target already in
/// memory. Zero on either field means that limit matches the acquire field ([`Self::sight_radius`],
/// [`Self::sight_half_angle`]), so an agent that leaves both at zero matches [`in_sight_cone`].
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct Perception {
    /// How far the agent can acquire a target it does not already remember (metres).
    #[reflect(@Range(0.0..=200.0))]
    pub sight_radius: f32,
    /// Half-angle of the acquire cone (radians; the cone is centred on the facing direction).
    #[reflect(@Range(0.0..=std::f64::consts::PI))]
    pub sight_half_angle: f32,
    /// How far the agent can hear a sound (metres).
    #[reflect(@Range(0.0..=200.0))]
    pub hearing_radius: f32,
    /// Team affiliation, looked up in [`crate::perception_system::Attitudes`].
    pub team: u8,
    /// How far a remembered target stays in sight (metres). Zero means [`Self::sight_radius`].
    /// A positive value is clamped up to [`Self::sight_radius`] on the keep path, so a direct
    /// write can never make the lose cone narrower than the acquire cone.
    #[reflect(@Range(0.0..=400.0))]
    pub sight_lose_radius: f32,
    /// Half-angle of the lose cone (radians). Zero means [`Self::sight_half_angle`]. A positive
    /// value is clamped up to [`Self::sight_half_angle`] on the keep path.
    #[reflect(@Range(0.0..=std::f64::consts::PI))]
    pub sight_lose_half_angle: f32,
    /// Contact range in metres. Zero (the default) disables touch. Touch needs no cone and no
    /// line of sight: a [`crate::perception_system::Perceivable`] inside this radius is felt.
    #[reflect(@Range(0.0..=200.0))]
    pub touch_radius: f32,
    /// Write this agent's stimuli into [`crate::perception_system::TeamKnowledge`] and read what
    /// the team and squad have shared. Off by default, so an agent only knows what it sensed.
    pub share_knowledge: bool,
}

impl Default for Perception {
    fn default() -> Self {
        Self {
            sight_radius: 40.0,
            sight_half_angle: 90f32.to_radians(),
            hearing_radius: 15.0,
            team: 0,
            sight_lose_radius: 0.0,
            sight_lose_half_angle: 0.0,
            touch_radius: 0.0,
            share_knowledge: false,
        }
    }
}

/// A sound the AI layer may hear (specs/22 § T2.2 "hearing events"). Emitted as an ECS message;
/// agents with a [`Perception`] within `radius` of `position` react, as [`can_hear`] says. Which
/// teams count is the [`crate::perception_system::Attitudes`] table's to decide.
#[derive(Message, Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub struct HearingEvent {
    /// The sound's world position.
    pub position: Vec3,
    /// How far the sound carries (metres).
    pub radius: f32,
    /// The team that made the sound (friendly sounds may be ignored).
    pub team: u8,
    /// The entity that produced the sound (or [`Entity::PLACEHOLDER`]).
    pub source: Entity,
}

impl Default for HearingEvent {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            radius: 10.0,
            team: 0,
            source: Entity::PLACEHOLDER,
        }
    }
}

/// Casts a line-of-sight ray from `from` toward `to` through the physics world, returning the
/// first blocker (or `None` when the line is clear). `exclude` are entities to ignore (the
/// observer itself). Degrades to `None` when `PhysicsPlugin` is not installed.
pub fn line_of_sight(world: &World, from: Vec3, to: Vec3, exclude: &[Entity]) -> Option<RaycastHit> {
    let queries = queries(world)?;
    let dir = (to - from).normalize_or_zero();
    if dir.length_squared() == 0.0 {
        return None;
    }
    let max_distance = (to - from).length();
    queries.raycast(
        from,
        dir,
        max_distance,
        QueryFilter::new().exclude_entities(exclude),
    )
}

/// Whether `target` is inside the sight cone rooted at `eye` facing `forward`: within
/// `half_angle` of the facing direction and within `max_distance`. The test is a horizontal fan:
/// the Y component is dropped before both the angle and the distance checks.
pub fn in_sight_cone(eye: Vec3, forward: Vec3, target: Vec3, half_angle: f32, max_distance: f32) -> bool {
    // A non-finite eye, facing, target, radius or angle is never a hit (a NaN would otherwise fall
    // through to the "no facing" 360° branch or pass the range compare).
    if !(eye.is_finite()
        && forward.is_finite()
        && target.is_finite()
        && half_angle.is_finite()
        && max_distance.is_finite())
    {
        return false;
    }
    let half_angle = half_angle.clamp(0.0, std::f32::consts::PI);
    let to = target - eye;
    let horiz = Vec3::new(to.x, 0.0, to.z);
    let dist = horiz.length();
    if !dist.is_finite() || dist > max_distance {
        return false;
    }
    let facing = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    let unrestricted = facing.length_squared() == 0.0 || half_angle >= std::f32::consts::PI;
    if dist < 1e-4 {
        // Straight above or below the eye: it has no horizontal bearing, so only an unrestricted
        // cone (no facing, or a full disk) admits it, and only off the eye's own spot.
        return unrestricted && to.length() >= 1e-4 && to.length() <= max_distance;
    }
    if facing.length_squared() == 0.0 {
        return true; // no facing direction → 360° sight
    }
    let cos = facing.dot(horiz / dist);
    // The deterministic cosine: a libm that differs in the last digit would split lockstep peers on
    // a target exactly at the cone's edge.
    cos >= regia_core::math::det_cos(half_angle)
}

/// The hearing test (specs/22 § T2.2 "hearing events"): whether `listener` at `listener_pos` can
/// hear `event` — the two are within both the listener's [`Perception::hearing_radius`] and the
/// event's [`HearingEvent::radius`]. A sound from any team is audible: enemies are what a listener
/// most needs to hear. Friendly sounds may be ignored by comparing `event.team`, which is what
/// [`crate::perception_system::PerceptionPlugin`] does through its attitude table.
///
/// Systems that read [`HearingEvent`] messages apply this predicate to filter which listeners
/// react. The distance is 3D (a sound directly above still counts).
pub fn can_hear(listener: &Perception, listener_pos: Vec3, event: &HearingEvent) -> bool {
    let dist = (event.position - listener_pos).length();
    if !(dist.is_finite() && listener.hearing_radius.is_finite() && event.radius.is_finite()) {
        return false;
    }
    let within_listener = dist <= listener.hearing_radius;
    let within_event = dist <= event.radius;
    within_listener && within_event
}

/// Builds a [`WorldQueries`] over the app's physics world, or `None` when there is none.
fn queries(world: &World) -> Option<WorldQueries<'_>> {
    world.get_resource::<regia_physics::PhysicsWorld>()?;
    let has_layers = world.get_resource::<regia_physics::PhysicsLayers>().is_some();
    Some(WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: has_layers.then_some(world.resource::<regia_physics::PhysicsLayers>()),
    })
}

/// Sight cone colour of the perception overlay.
#[cfg(feature = "debug_draw")]
pub const SIGHT_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(1.0, 0.25, 0.2);
/// Hearing radius colour of the perception overlay.
#[cfg(feature = "debug_draw")]
pub const HEARING_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.3, 0.6, 1.0);
/// Last-known-position colour. The sphere grows with confidence.
#[cfg(feature = "debug_draw")]
pub const MEMORY_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.95, 0.85, 0.2);
/// A fact the team or squad shared.
#[cfg(feature = "debug_draw")]
pub const TEAM_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.2, 0.9, 0.45);
/// The detection meter, drawn up from the observer.
#[cfg(feature = "debug_draw")]
pub const METER_COLOR: regia_core::math::Color = regia_core::math::Color::rgb(0.95, 0.45, 0.1);

/// `PostUpdate` debug overlay: every [`Perception`]'s sight cone (a horizontal fan, as
/// [`in_sight_cone`] tests it) and hearing radius, drawn into the
/// [`AiPerceptionGizmos`] group. [`crate::AiPlugin`] runs it only while that group is enabled.
#[cfg(feature = "debug_draw")]
pub fn draw_perception(
    agents: Query<(&Perception, &GlobalTransform), Without<LayerParked>>,
    mut gizmos: Gizmos<AiPerceptionGizmos>,
) {
    for (perception, transform) in &agents {
        let forward = transform.0.transform_vector3(Vec3::NEG_Z);
        draw_senses(&mut gizmos, perception, transform.translation(), forward);
    }
}

/// Draws one agent's senses at `eye` facing `forward` (flattened to the ground plane).
#[cfg(feature = "debug_draw")]
pub fn draw_senses<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    perception: &Perception,
    eye: Vec3,
    forward: Vec3,
) {
    if !(eye.is_finite() && forward.is_finite() && perception.sight_half_angle.is_finite()) {
        return;
    }
    let sight_radius = perception.sight_radius.max(0.0);
    let hearing_radius = perception.hearing_radius.max(0.0);
    let facing = Vec3::new(forward.x, 0.0, forward.z)
        .try_normalize()
        .unwrap_or(Vec3::NEG_Z);
    let yaw = facing.x.atan2(-facing.z);
    let point = |angle: f32, radius: f32| {
        let a = yaw + angle;
        eye + Vec3::new(a.sin(), 0.0, -a.cos()) * radius
    };
    let half = perception.sight_half_angle.clamp(0.0, std::f32::consts::PI);
    let steps = ((half * 2.0 / std::f32::consts::TAU) * 48.0).ceil().max(2.0) as u32;
    let rim = (0..=steps).map(|i| point(-half + 2.0 * half * i as f32 / steps as f32, sight_radius));
    if half < std::f32::consts::PI {
        gizmos.linestrip(
            std::iter::once(eye).chain(rim).chain(std::iter::once(eye)),
            SIGHT_COLOR,
        );
    } else {
        gizmos.linestrip(rim, SIGHT_COLOR);
    }
    gizmos.ray(eye, facing * sight_radius.min(2.0), SIGHT_COLOR);
    let hearing = (0..=48).map(|i| point(i as f32 * std::f32::consts::TAU / 48.0, hearing_radius));
    gizmos.linestrip(hearing, HEARING_COLOR);
}

/// A remembered point. The sphere radius is `0.05 + 0.2 * confidence` (confidence clamped to
/// `[0, 1]`). A non-finite position or confidence draws nothing.
#[cfg(feature = "debug_draw")]
pub fn draw_memory_point<G: GizmoConfigGroup>(gizmos: &mut GizmoBuffer<G>, position: Vec3, confidence: f32) {
    if !position.is_finite() || !confidence.is_finite() {
        return;
    }
    let confidence = confidence.clamp(0.0, 1.0);
    gizmos.sphere(position, 0.05 + 0.2 * confidence, MEMORY_COLOR);
}

/// A shared team or squad fact, drawn just above the last known position.
#[cfg(feature = "debug_draw")]
pub fn draw_team_marker<G: GizmoConfigGroup>(gizmos: &mut GizmoBuffer<G>, position: Vec3) {
    if !position.is_finite() {
        return;
    }
    gizmos.sphere(position + Vec3::Y * 0.3, 0.12, TEAM_COLOR);
}

/// The detection meter: a vertical line at `origin` whose length is the evidence, capped at 8 m.
/// Zero or non-finite evidence draws nothing.
#[cfg(feature = "debug_draw")]
pub fn draw_meter_bar<G: GizmoConfigGroup>(gizmos: &mut GizmoBuffer<G>, origin: Vec3, evidence: f32) {
    if !origin.is_finite() || !evidence.is_finite() || evidence <= 0.0 {
        return;
    }
    let height = evidence.clamp(0.0, 8.0);
    gizmos.line(origin, origin + Vec3::Y * height, METER_COLOR);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sight_cone_rejects_non_finite_inputs() {
        let eye = Vec3::ZERO;
        let target = Vec3::new(0.0, 0.0, 5.0);
        let fwd = Vec3::Z;
        assert!(in_sight_cone(eye, fwd, target, 1.0, 10.0));
        assert!(!in_sight_cone(
            eye,
            Vec3::new(f32::NAN, 0.0, 1.0),
            target,
            1.0,
            10.0
        ));
        assert!(!in_sight_cone(eye, fwd, target, 1.0, f32::NAN));
        assert!(!in_sight_cone(eye, fwd, target, f32::NAN, 10.0));
        assert!(!in_sight_cone(eye, fwd, Vec3::new(f32::NAN, 0.0, 1.0), 1.0, 10.0));
    }

    #[cfg(feature = "debug_draw")]
    #[test]
    fn senses_draw_a_closed_fan_and_a_hearing_ring() {
        let mut b = GizmoBuffer::<regia_render::gizmos::DefaultGizmoConfigGroup>::new();
        let perception = Perception {
            sight_radius: 10.0,
            sight_half_angle: 0.5,
            hearing_radius: 4.0,
            team: 0,
            ..Perception::default()
        };
        draw_senses(&mut b, &perception, Vec3::ZERO, Vec3::NEG_Z);
        let strips: Vec<&[Vec3]> = b
            .strip_positions
            .split(|p| p.is_nan())
            .filter(|r| !r.is_empty())
            .collect();
        assert_eq!(strips.len(), 2);
        // The fan starts and ends at the eye; its rim is at the sight radius, centred on −Z.
        let fan = strips[0];
        assert_eq!(fan[0], Vec3::ZERO);
        assert_eq!(*fan.last().expect("fan"), Vec3::ZERO);
        let mid = fan[fan.len() / 2];
        assert!(
            (mid.length() - 10.0).abs() < 1e-3 && mid.z < -9.0,
            "rim center {mid}"
        );
        assert!(strips[1].iter().all(|p| (p.length() - 4.0).abs() < 1e-3));
        // Plus the facing ray.
        assert_eq!(b.list_positions.len(), 2);
    }

    #[test]
    fn sight_cone_accepts_ahead_and_rejects_behind() {
        let eye = Vec3::ZERO;
        let forward = Vec3::NEG_Z;
        assert!(in_sight_cone(
            eye,
            forward,
            Vec3::new(0.0, 0.0, -5.0),
            45f32.to_radians(),
            10.0
        ));
        assert!(!in_sight_cone(
            eye,
            forward,
            Vec3::new(0.0, 0.0, 5.0),
            45f32.to_radians(),
            10.0
        ));
        assert!(!in_sight_cone(
            eye,
            forward,
            Vec3::new(100.0, 0.0, 0.0),
            45f32.to_radians(),
            10.0
        ));
    }

    #[test]
    fn a_wall_blocks_line_of_sight() {
        use regia_physics::{Collider, ColliderShape, RigidBody};
        use regia_scene::Transform;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 2.0, 0.2),
                },
                ..Collider::default()
            },
        ));
        for _ in 0..6 {
            app.update();
        }
        // The wall at z=0 blocks a ray from z=-5 to z=+5.
        let hit = line_of_sight(
            app.world(),
            Vec3::new(0.0, 1.0, -5.0),
            Vec3::new(0.0, 1.0, 5.0),
            &[],
        );
        assert!(hit.is_some(), "the wall blocks the line");
    }

    #[test]
    fn can_hear_applies_both_radii_for_any_team() {
        let listener = Perception::default(); // hearing_radius 15, team 0
        let near = HearingEvent {
            position: Vec3::new(5.0, 0.0, 0.0),
            radius: 10.0,
            team: 0,
            ..Default::default()
        };
        assert!(can_hear(&listener, Vec3::ZERO, &near), "in range");

        let far = HearingEvent {
            position: Vec3::new(20.0, 0.0, 0.0),
            radius: 10.0,
            team: 0,
            ..Default::default()
        };
        assert!(
            !can_hear(&listener, Vec3::ZERO, &far),
            "beyond the hearing radius"
        );

        let other_team = HearingEvent {
            position: Vec3::new(5.0, 0.0, 0.0),
            radius: 10.0,
            team: 1,
            ..Default::default()
        };
        assert!(
            can_hear(&listener, Vec3::ZERO, &other_team),
            "another team is audible: its doc says friendly sounds may be ignored, not enemy ones"
        );

        let quiet = HearingEvent {
            position: Vec3::new(8.0, 0.0, 0.0),
            radius: 2.0,
            team: 0,
            ..Default::default()
        };
        assert!(
            !can_hear(&listener, Vec3::ZERO, &quiet),
            "beyond the event's own carry radius"
        );
    }

    #[test]
    fn perception_defaults() {
        let p = Perception::default();
        assert!(p.sight_radius > 0.0 && p.hearing_radius > 0.0);
        assert!(p.sight_half_angle > 0.0);
        assert_eq!(p.sight_lose_radius, 0.0);
        assert_eq!(p.sight_lose_half_angle, 0.0);
    }

    #[test]
    fn sight_cone_clamps_the_half_angle_and_rejects_non_finite_input() {
        let (eye, fwd) = (Vec3::ZERO, Vec3::NEG_Z);
        let behind = Vec3::new(0.0, 0.0, 5.0);
        let ahead = Vec3::new(0.0, 0.0, -5.0);
        // 2 pi used to give cos = 1 (a ray); it now clamps to a full disk.
        assert!(in_sight_cone(eye, fwd, behind, std::f32::consts::TAU, 10.0));
        assert!(in_sight_cone(eye, fwd, ahead, std::f32::consts::TAU, 10.0));
        // A negative half-angle clamps to 0: only dead ahead.
        assert!(in_sight_cone(eye, fwd, ahead, -1.0, 10.0));
        assert!(!in_sight_cone(eye, fwd, Vec3::new(5.0, 0.0, -5.0), -1.0, 10.0));
        assert!(!in_sight_cone(Vec3::NAN, fwd, ahead, 1.0, 10.0));
        assert!(!in_sight_cone(eye, fwd, ahead, f32::NAN, 10.0));
    }

    #[test]
    fn hearing_requires_finite_radii() {
        let event = HearingEvent {
            position: Vec3::new(1.0, 0.0, 0.0),
            radius: f32::INFINITY,
            team: 0,
            source: Entity::PLACEHOLDER,
        };
        assert!(!can_hear(&Perception::default(), Vec3::ZERO, &event));
        let nan_pos = HearingEvent {
            position: Vec3::NAN,
            radius: 10.0,
            ..event
        };
        assert!(!can_hear(&Perception::default(), Vec3::ZERO, &nan_pos));
        let ok = HearingEvent {
            radius: 10.0,
            ..event
        };
        assert!(can_hear(&Perception::default(), Vec3::ZERO, &ok));
    }

    #[cfg(feature = "debug_draw")]
    #[test]
    fn drawing_non_finite_senses_emits_nothing() {
        let mut b = GizmoBuffer::<regia_render::gizmos::DefaultGizmoConfigGroup>::new();
        let perception = Perception {
            sight_half_angle: f32::NAN,
            ..Perception::default()
        };
        draw_senses(&mut b, &perception, Vec3::ZERO, Vec3::NEG_Z);
        assert_eq!(b.list_positions.len() + b.strip_positions.len(), 0);
    }

    #[cfg(feature = "debug_draw")]
    #[test]
    fn memory_points_team_markers_and_the_meter_draw() {
        let mut buffer = GizmoBuffer::<regia_render::gizmos::DefaultGizmoConfigGroup>::new();
        let before = buffer.list_positions.len() + buffer.strip_positions.len();
        draw_memory_point(&mut buffer, Vec3::X, 1.0);
        draw_team_marker(&mut buffer, Vec3::Z);
        draw_meter_bar(&mut buffer, Vec3::ZERO, 0.5);
        draw_memory_point(&mut buffer, Vec3::NAN, 1.0);
        draw_meter_bar(&mut buffer, Vec3::ZERO, 0.0);
        assert!(buffer.list_positions.len() + buffer.strip_positions.len() > before);
    }
}
