// SPDX-License-Identifier: MPL-2.0
//! Debug overlays drawn with gizmos: physics colliders, audio emitters, the nav mesh and motion
//! matching ([`crate::motion_matching_gizmos`]).
//!
//! Each overlay draws into its own gizmo config group, disabled by default
//! ([`PhysicsGizmos`], [`AudioGizmos`], [`NavMeshGizmos`]); turn one on with
//! `GizmoConfigStore::set_enabled("physics", true)` or the agent method `debug.set_gizmos`.
//! A disabled overlay's system does not run. AI perception is drawn by `regia_ai`.
//!
//! These systems live here because the runtime layer rules keep physics, audio and navigation
//! independent of the renderer (`docs/02-architecture.md` § 1); this crate sees all of them.

use std::f32::consts::{FRAC_PI_2, TAU};

use regia_app::prelude::*;
use regia_audio::AudioEmitter;
use regia_core::math::{Aabb, Color, Isometry, Quat, Vec3};
use regia_nav::NavMesh;
use regia_physics::{Collider, ColliderShape, ShapeAxis};
use regia_render::gizmos::{
    AppGizmoBuilder, AudioGizmos, GizmoBuffer, GizmoConfigGroup, Gizmos, NavMeshGizmos, PhysicsGizmos,
    gizmo_group_enabled,
};
use regia_scene::GlobalTransform;

/// Solid colliders.
pub const COLLIDER_COLOR: Color = Color::rgb(0.2, 1.0, 0.35);
/// Sensor (trigger) colliders.
pub const SENSOR_COLOR: Color = Color::rgb(1.0, 0.85, 0.1);
/// Nav mesh polygon edges.
pub const NAV_EDGE_COLOR: Color = Color::rgb(0.1, 0.75, 1.0);
/// Nav mesh off-mesh links.
pub const NAV_LINK_COLOR: Color = Color::rgb(1.0, 0.4, 0.9);
/// Audio emitter full-volume radius and inner cone.
pub const AUDIO_NEAR_COLOR: Color = Color::rgb(1.0, 0.55, 0.1);
/// Audio emitter silence radius and outer cone.
pub const AUDIO_FAR_COLOR: Color = Color::rgb(0.55, 0.3, 0.08);

/// Most triangles of one trimesh collider drawn as wireframe (larger meshes draw their bounds).
pub const MAX_TRIMESH_TRIANGLES: usize = 20_000;

/// Draws the physics, audio and nav-mesh debug overlays (`PostUpdate`, after transforms).
#[derive(Default)]
pub struct DebugGizmosPlugin;

impl Plugin for DebugGizmosPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DebugColors>();
        app.init_gizmo_group::<crate::path_debug::PathGizmos>();
        // The deterministic-index debug draw, gizmo group `lockstep` (ADR-8562).
        app.add_plugins(crate::det_debug::DetDebugDrawPlugin);
        // The `locomotion` overlay: gait, foot placement and leg IK (ADR-8729).
        app.add_plugins(crate::locomotion_gizmos::LocomotionGizmosPlugin);
        app.add_systems(
            PostUpdate,
            (
                draw_colliders.run_if(gizmo_group_enabled::<PhysicsGizmos>),
                draw_physics_debug
                    .after(regia_physics::debug_draw::collect_physics_debug)
                    .run_if(gizmo_group_enabled::<PhysicsGizmos>)
                    .run_if(regia_physics::debug_draw::physics_debug_on),
                draw_audio_emitters.run_if(gizmo_group_enabled::<AudioGizmos>),
                draw_nav_mesh.run_if(gizmo_group_enabled::<NavMeshGizmos>),
                crate::path_debug::draw_paths.run_if(gizmo_group_enabled::<crate::path_debug::PathGizmos>),
                crate::motion_matching_gizmos::draw_motion_matching
                    .run_if(gizmo_group_enabled::<regia_render::gizmos::MotionMatchingGizmos>),
            )
                .after(PostUpdateSet::TransformPropagate),
        );
    }
}

fn draw_colliders(
    colliders: Query<(Entity, &Collider, &GlobalTransform)>,
    colors: Res<DebugColors>,
    physics: Option<Res<regia_physics::PhysicsWorld>>,
    mut gizmos: Gizmos<PhysicsGizmos>,
) {
    let _span = tracing::trace_span!("debug_colliders").entered();
    let sleeping = if colors.sleeping_body == colors.collider {
        std::collections::HashSet::new()
    } else {
        sleeping_entities(physics.as_deref())
    };
    for (entity, collider, transform) in &colliders {
        let body = Isometry::from_affine(&transform.0);
        let pose = body * Isometry::new(collider.offset.rotation, collider.offset.translation);
        let color = if collider.sensor {
            colors.trigger
        } else if sleeping.contains(&entity) {
            colors.sleeping_body
        } else {
            colors.collider
        };
        draw_shape(&mut gizmos, &collider.shape, pose, color);
    }
}

/// Draws the lines `regia_physics::debug_draw` collected (contacts, joints, centers of mass; ADR-2424)
/// into the physics gizmo group. The collection runs only while the draw is on, and this system only
/// while the draw is on and the group is enabled, so neither costs anything otherwise.
fn draw_physics_debug(
    draw: Res<regia_physics::debug_draw::PhysicsDebugDraw>,
    colors: Res<DebugColors>,
    mut gizmos: Gizmos<PhysicsGizmos>,
) {
    let _span = tracing::debug_span!(target: "regia_framework", "physics_debug_gizmos").entered();
    for line in draw.lines() {
        let [r, g, b] = line.color;
        let color = if line.color == regia_physics::debug_draw::CONTACT_COLOR {
            colors.contact
        } else {
            Color::rgb(r, g, b)
        };
        gizmos.line(line.from, line.to, color);
    }
}

/// Rotation that takes a shape's +Y axis onto `axis`.
fn axis_rotation(axis: ShapeAxis) -> Quat {
    match axis {
        ShapeAxis::X => Quat::from_rotation_z(-FRAC_PI_2),
        ShapeAxis::Y => Quat::IDENTITY,
        ShapeAxis::Z => Quat::from_rotation_x(FRAC_PI_2),
    }
}

/// Draws `shape` at `pose` as a wireframe.
pub fn draw_shape<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    shape: &ColliderShape,
    pose: Isometry,
    color: Color,
) {
    match shape {
        ColliderShape::Box { half_extents } => gizmos.cuboid(pose, *half_extents, color),
        ColliderShape::Sphere { radius } => {
            gizmos.sphere(pose, *radius, color).resolution(24);
        }
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => {
            let pose = pose * Isometry::from_rotation(axis_rotation(*axis));
            gizmos.capsule(pose, *radius, *half_height, color).resolution(8);
        }
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        } => {
            let pose = pose * Isometry::from_rotation(axis_rotation(*axis));
            gizmos.cylinder(pose, *radius, *half_height, color).resolution(16);
        }
        ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => {
            let pose = pose * Isometry::from_rotation(axis_rotation(*axis));
            gizmos
                .cone(pose, *radius, half_height * 2.0, color)
                .resolution(16);
        }
        ColliderShape::ConvexHull { points } => {
            // The hull's faces are rapier's to compute; its bounds are a faithful outline.
            if let Some(bounds) = Aabb::from_points(points.iter().copied()) {
                gizmos.aabb_3d(bounds, pose, color);
            }
        }
        ColliderShape::TriMesh { vertices, indices } => {
            if indices.len() > MAX_TRIMESH_TRIANGLES {
                if let Some(bounds) = Aabb::from_points(vertices.iter().copied()) {
                    gizmos.aabb_3d(bounds, pose, color);
                }
                return;
            }
            // Three plain lines per triangle: no strip separators or seam joints for big meshes.
            for triangle in indices {
                let corners = triangle.map(|i| vertices.get(i as usize).map(|v| pose.transform_point(*v)));
                if let [Some(a), Some(b), Some(c)] = corners {
                    gizmos.line(a, b, color);
                    gizmos.line(b, c, color);
                    gizmos.line(c, a, color);
                }
            }
        }
        ColliderShape::HeightField { heights, scale, .. } => {
            // Its outline: the sample grid spans [-0.5, 0.5] × scale in X and Z.
            let (lo, hi) = heights
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), h| (lo.min(*h), hi.max(*h)));
            if lo <= hi {
                let bounds = Aabb::new(
                    Vec3::new(-0.5 * scale.x, lo * scale.y, -0.5 * scale.z),
                    Vec3::new(0.5 * scale.x, hi * scale.y, 0.5 * scale.z),
                );
                gizmos.aabb_3d(bounds, pose, color);
            }
        }
        ColliderShape::Compound { shapes } => {
            for (offset, child) in shapes {
                draw_shape(
                    gizmos,
                    child,
                    pose * Isometry::new(offset.rotation, offset.translation),
                    color,
                );
            }
        }
    }
}

fn draw_audio_emitters(emitters: Query<(&AudioEmitter, &GlobalTransform)>, mut gizmos: Gizmos<AudioGizmos>) {
    for (emitter, transform) in &emitters {
        let pose = Isometry::from_affine(&transform.0);
        draw_emitter(&mut gizmos, emitter, pose);
    }
}

/// Draws an emitter: the full-volume and silence spheres and, for directional emitters, the
/// inner and outer cones along its forward axis (−Z).
pub fn draw_emitter<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    emitter: &AudioEmitter,
    pose: Isometry,
) {
    let min = emitter.min_distance.max(0.0);
    let max = emitter.max_distance.max(min);
    gizmos.sphere(pose, min, AUDIO_NEAR_COLOR).resolution(24);
    gizmos.sphere(pose, max, AUDIO_FAR_COLOR).resolution(48);
    let length = min.max(1.0);
    for (degrees, color) in [
        (emitter.cone_inner_degrees, AUDIO_NEAR_COLOR),
        (emitter.cone_outer_degrees, AUDIO_FAR_COLOR),
    ] {
        if !(0.0..180.0).contains(&degrees) {
            continue;
        }
        draw_cone_outline(gizmos, pose, degrees.to_radians(), length, color);
    }
    let forward = pose.transform_vector(Vec3::NEG_Z);
    gizmos.arrow(
        pose.translation,
        pose.translation + forward * length,
        AUDIO_NEAR_COLOR,
    );
}

/// Eight rays and a rim for a cone of `half_angle` and slant `length` along the pose's −Z.
pub fn draw_cone_outline<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    pose: Isometry,
    half_angle: f32,
    length: f32,
    color: Color,
) {
    let (sin, cos) = half_angle.sin_cos();
    let rim: Vec<Vec3> = (0..=24)
        .map(|i| {
            let a = i as f32 * TAU / 24.0;
            pose.transform_point(Vec3::new(a.cos() * sin, a.sin() * sin, -cos) * length)
        })
        .collect();
    for point in rim.iter().step_by(3).take(8) {
        gizmos.line(pose.translation, *point, color);
    }
    gizmos.linestrip(rim, color);
}

fn draw_nav_mesh(mesh: Option<Res<NavMesh>>, colors: Res<DebugColors>, mut gizmos: Gizmos<NavMeshGizmos>) {
    let Some(mesh) = mesh else { return };
    for poly in &mesh.polys {
        // Lifted a few centimetres so the outline sits on, not in, the ground.
        gizmos.lineloop(poly.vertices.iter().map(|v| *v + Vec3::Y * 0.05), colors.nav);
    }
    for link in &mesh.off_mesh_links {
        let arrow = gizmos.arrow(link.from, link.to, colors.nav_links);
        if link.bidirectional {
            arrow.with_double_end();
        }
    }
}

/// Project-selected semantic debug colors, shared by editor and runtime overlays.
/// Values are linear RGB; hosts may replace this resource without depending on the editor.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct DebugColors {
    /// Solid collider outlines.
    pub collider: Color,
    /// Trigger outlines.
    pub trigger: Color,
    /// Navigation polygon outlines.
    pub nav: Color,
    /// Navigation links. A project navigation color sets this and `nav` together;
    /// separate defaults preserve the existing distinction until an override is authored.
    pub nav_links: Color,
    /// Remaining agent navigation paths.
    pub path: Color,
    /// Contact normals.
    pub contact: Color,
    /// Sleeping dynamic-body outlines.
    pub sleeping_body: Color,
}

impl Default for DebugColors {
    fn default() -> Self {
        let [r, g, b] = regia_physics::debug_draw::CONTACT_COLOR;
        Self {
            collider: COLLIDER_COLOR,
            trigger: SENSOR_COLOR,
            nav: NAV_EDGE_COLOR,
            nav_links: NAV_LINK_COLOR,
            path: NAV_LINK_COLOR,
            contact: Color::rgb(r, g, b),
            sleeping_body: COLLIDER_COLOR,
        }
    }
}

/// Sleeping dynamic bodies and their attached collider entities (empty without physics).
#[must_use]
pub fn sleeping_entities(physics: Option<&regia_physics::PhysicsWorld>) -> std::collections::HashSet<Entity> {
    let _span = tracing::trace_span!("debug_sleeping_entities").entered();
    let mut entities = std::collections::HashSet::new();
    if let Some(physics) = physics {
        for (handle, body) in physics.bodies.iter() {
            if body.is_dynamic() && body.is_sleeping() {
                entities.extend(physics.entity_of_body(handle));
                entities.extend(
                    body.colliders()
                        .iter()
                        .filter_map(|handle| physics.entity_of_collider(*handle)),
                );
            }
        }
    }
    entities
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::gizmos::{DefaultGizmoConfigGroup, GizmoConfigStore, GizmoPlugin, GizmoStorage};

    fn segments<G: GizmoConfigGroup>(b: &GizmoBuffer<G>) -> usize {
        let strips: usize = b
            .strip_positions
            .split(|p| p.is_nan())
            .map(|r| r.len().saturating_sub(1))
            .sum();
        b.list_positions.len() / 2 + strips
    }

    #[test]
    fn every_collider_shape_draws_something() {
        let shapes = [
            ColliderShape::default(),
            ColliderShape::Sphere { radius: 1.0 },
            ColliderShape::Capsule {
                half_height: 1.0,
                radius: 0.5,
                axis: ShapeAxis::X,
            },
            ColliderShape::Cylinder {
                half_height: 1.0,
                radius: 0.5,
                axis: ShapeAxis::Z,
            },
            ColliderShape::Cone {
                half_height: 1.0,
                radius: 0.5,
                axis: ShapeAxis::Y,
            },
            ColliderShape::ConvexHull {
                points: vec![Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z],
            },
            ColliderShape::TriMesh {
                vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Z],
                indices: vec![[0, 1, 2], [0, 1, 9]],
            },
            ColliderShape::HeightField {
                nx: 2,
                nz: 2,
                heights: vec![0.0, 1.0, 0.5, 0.2],
                scale: Vec3::new(10.0, 2.0, 10.0),
            },
            ColliderShape::Compound {
                shapes: vec![(
                    regia_scene::Transform::IDENTITY,
                    ColliderShape::Sphere { radius: 0.2 },
                )],
            },
        ];
        for shape in &shapes {
            let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
            draw_shape(&mut b, shape, Isometry::IDENTITY, COLLIDER_COLOR);
            assert!(segments(&b) > 0, "{shape:?} drew nothing");
        }
        // The capsule along X spans ±1.5 on X.
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_shape(&mut b, &shapes[2], Isometry::IDENTITY, COLLIDER_COLOR);
        let max_x = b
            .strip_positions
            .iter()
            .chain(&b.list_positions)
            .filter(|p| p.is_finite())
            .map(|p| p.x)
            .fold(f32::MIN, f32::max);
        assert!((max_x - 1.5).abs() < 1e-3, "capsule along X reaches {max_x}");
        // An out-of-range trimesh index is skipped, not a panic: one triangle drawn.
        let mut t = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_shape(&mut t, &shapes[6], Isometry::IDENTITY, COLLIDER_COLOR);
        assert_eq!(segments(&t), 3);
    }

    #[test]
    fn directional_emitters_draw_cones() {
        let mut omni = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        draw_emitter(&mut omni, &AudioEmitter::default(), Isometry::IDENTITY);
        let mut cone = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        let emitter = AudioEmitter {
            cone_inner_degrees: 30.0,
            cone_outer_degrees: 60.0,
            ..AudioEmitter::default()
        };
        draw_emitter(&mut cone, &emitter, Isometry::IDENTITY);
        assert_eq!(segments(&cone), segments(&omni) + 2 * (8 + 24));
    }

    #[test]
    fn overlays_only_draw_when_enabled() {
        let mut app = App::new();
        app.add_plugins(GizmoPlugin).add_plugins(DebugGizmosPlugin);
        app.world_mut().spawn((
            Collider::default(),
            GlobalTransform::IDENTITY,
            AudioEmitter::default(),
        ));
        app.run_schedule(PostUpdate);
        assert!(app.world().resource::<GizmoStorage<PhysicsGizmos>>().is_empty());
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("physics", true);
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("audio", true);
        app.run_schedule(PostUpdate);
        assert_eq!(
            app.world()
                .resource::<GizmoStorage<PhysicsGizmos>>()
                .view()
                .list_positions
                .len(),
            24
        );
        assert!(!app.world().resource::<GizmoStorage<AudioGizmos>>().is_empty());
        assert!(app.world().resource::<GizmoStorage<NavMeshGizmos>>().is_empty());
    }

    #[test]
    fn physics_debug_lines_reach_the_physics_group_only_while_on() {
        use regia_physics::debug_draw::PhysicsDebugDraw;
        let mut app = App::new();
        app.add_plugins(GizmoPlugin)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins(DebugGizmosPlugin);
        app.world_mut().spawn((
            regia_scene::Transform::default(),
            regia_physics::RigidBody::dynamic(),
            Collider::default(),
        ));
        app.run_schedule(FixedUpdate);
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("physics", true);
        // Off: nothing is collected or drawn although the group is on (the collider outline
        // overlay draws its own lines, so compare against the list before turning the draw on).
        app.run_schedule(PostUpdate);
        assert_eq!(app.world().resource::<PhysicsDebugDraw>().stats().collections, 0);
        let outline = app
            .world()
            .resource::<GizmoStorage<PhysicsGizmos>>()
            .view()
            .list_positions
            .len();
        // On: the dynamic body's center-of-mass cross (three lines) joins the outline.
        app.world_mut().resource_mut::<PhysicsDebugDraw>().set_on(true);
        app.run_schedule(PostUpdate);
        let drawn = app
            .world()
            .resource::<GizmoStorage<PhysicsGizmos>>()
            .view()
            .list_positions
            .len();
        assert_eq!(app.world().resource::<PhysicsDebugDraw>().lines().len(), 3);
        // The storage keeps the first frame's outline (gizmos are cleared by the render extract, not
        // by this schedule), so the second frame adds another outline and the three physics lines.
        assert_eq!(drawn, 2 * outline + 6);
    }
}

#[cfg(test)]
mod w12_tests {
    use super::*;
    use regia_render::gizmos::{GizmoConfigStore, GizmoPlugin, GizmoStorage};

    #[test]
    fn w12_runtime_paths_are_opt_in_and_use_the_project_color() {
        use crate::path_debug::PathGizmos;
        let mut app = App::new();
        app.add_plugins(GizmoPlugin).add_plugins(DebugGizmosPlugin);
        let color = Color::rgb(0.25, 0.5, 0.75);
        app.world_mut().resource_mut::<DebugColors>().path = color;
        app.world_mut().spawn((
            GlobalTransform::IDENTITY,
            regia_nav::NavAgentState {
                waypoints: vec![Vec3::X, Vec3::Y],
                ..Default::default()
            },
        ));
        app.run_schedule(PostUpdate);
        assert!(app.world().resource::<GizmoStorage<PathGizmos>>().is_empty());
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("paths", true);
        app.run_schedule(PostUpdate);
        let storage = app.world().resource::<GizmoStorage<PathGizmos>>();
        assert_eq!(storage.view().list_positions.len(), 4);
        assert!(storage.view().list_colors.iter().all(|c| *c == color));
        app.world_mut().resource_mut::<GizmoStorage<PathGizmos>>().clear();
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("paths", false);
        app.run_schedule(PostUpdate);
        assert!(app.world().resource::<GizmoStorage<PathGizmos>>().is_empty());
    }

    #[test]
    fn w12_sleeping_dynamic_body_uses_its_color_instead_of_the_collider_color() {
        let mut app = App::new();
        app.add_plugins(GizmoPlugin)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins(DebugGizmosPlugin);
        let entity = app
            .world_mut()
            .spawn((
                regia_scene::Transform::default(),
                GlobalTransform::IDENTITY,
                regia_physics::RigidBody::dynamic(),
                Collider::default(),
            ))
            .id();
        let child = app
            .world_mut()
            .spawn((
                regia_scene::Transform::default(),
                GlobalTransform::IDENTITY,
                regia_ecs::prelude::ChildOf(entity),
                Collider::default(),
            ))
            .id();
        app.run_schedule(FixedUpdate);
        for (_, body) in app
            .world_mut()
            .resource_mut::<regia_physics::PhysicsWorld>()
            .bodies
            .iter_mut()
        {
            body.sleep();
        }
        assert!(
            sleeping_entities(app.world().get_resource::<regia_physics::PhysicsWorld>()).contains(&entity)
        );
        assert!(
            sleeping_entities(app.world().get_resource::<regia_physics::PhysicsWorld>()).contains(&child),
            "colliders attached through the scene hierarchy inherit the sleeping body color"
        );
        let color = Color::rgb(0.25, 0.5, 0.75);
        app.world_mut().resource_mut::<DebugColors>().sleeping_body = color;
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("physics", true);
        app.run_schedule(PostUpdate);
        let storage = app.world().resource::<GizmoStorage<PhysicsGizmos>>();
        assert!(!storage.is_empty());
        assert!(storage.view().list_colors.iter().all(|c| *c == color));
    }

    #[test]
    fn w12_path_work_is_bounded_even_with_invalid_points() {
        let mut world = World::new();
        let limit = crate::path_debug::MAX_PATH_SEGMENTS;
        let mut points = vec![Vec3::splat(f32::NAN); limit];
        points.extend([Vec3::X, Vec3::Y]);
        world.spawn((
            GlobalTransform::IDENTITY,
            regia_nav::NavAgentState {
                waypoints: points,
                ..Default::default()
            },
        ));
        assert!(
            crate::path_debug::path_segments(&world).is_empty(),
            "invalid edges consume the work budget too"
        );
    }

    #[test]
    fn w12_paths_follow_only_remaining_waypoints_and_skip_invalid_edges() {
        let mut world = World::new();
        world.spawn((
            GlobalTransform::IDENTITY,
            regia_nav::NavAgentState {
                waypoints: vec![
                    Vec3::splat(-10.0),
                    Vec3::X,
                    Vec3::new(f32::NAN, 0.0, 0.0),
                    Vec3::Y,
                    Vec3::Z,
                ],
                next_waypoint: 1,
                ..Default::default()
            },
        ));
        let lines = super::super::path_debug::path_segments(&world);
        assert_eq!(lines, vec![(Vec3::ZERO, Vec3::X), (Vec3::Y, Vec3::Z)]);
    }

    #[test]
    fn w12_runtime_collider_color_is_resource_driven() {
        let mut app = App::new();
        app.add_plugins(GizmoPlugin).add_plugins(DebugGizmosPlugin);
        let color = Color::rgb(0.7, 0.2, 0.6);
        app.world_mut().resource_mut::<DebugColors>().collider = color;
        app.world_mut()
            .spawn((Collider::default(), GlobalTransform::IDENTITY));
        app.world_mut()
            .resource_mut::<GizmoConfigStore>()
            .set_enabled("physics", true);
        app.run_schedule(PostUpdate);
        let storage = app.world().resource::<GizmoStorage<PhysicsGizmos>>();
        assert!(!storage.is_empty());
        assert!(storage.view().list_colors.iter().all(|c| *c == color));
    }
}
