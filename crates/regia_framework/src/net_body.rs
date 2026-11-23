// SPDX-License-Identifier: MPL-2.0
//! Networked rigid bodies (spec 71 § 9, spec 72 § 6, spec 74; ADR-2902): crates, barrels and
//! thrown objects that the server simulates and every client shows.
//!
//! Opt in per entity with [`ReplicatedBody`] next to `Replicated` and add [`NetBodyPlugin`] on every
//! process (after `NetPlugin` and `PhysicsPlugin`).
//!
//! - **Server.** After each physics step the body's stepped pose, its linear velocity and its rapier
//!   sleep state drive what the transform fast path sends: a moved pose marks `Transform` changed
//!   (the step writes it without change detection), the velocity is written to `NetVelocity`
//!   (rounded to [`ReplicatedBody::velocity_step`], zero below [`ReplicatedBody::rest_speed`]), and
//!   a body rapier put to sleep gets `NetAsleep`. Spec 71 § 8 then keeps sending until every
//!   connection acknowledged the final pose and sends nothing after that: a sleeping body costs no
//!   bytes. When rapier wakes the body (an impact, an impulse, a character push) the marker goes and
//!   the moved pose is sent in the same tick. The speed raises the body's `NetPriorityScale`, so a
//!   tumbling crate wins the bandwidth budget over a slow one; distance comes from the profile
//!   ([`body_profile`]).
//! - **Client.** A proxy copy is kinematic: rapier takes its pose from the `Transform` that spec 74
//!   interpolation writes (with its capped velocity extrapolation and decaying recovery offset), so
//!   characters stand on and walk into what the player sees. Physics render interpolation is turned
//!   off for the copy (`NoInterpolation`), because a copy that ever stepped as dynamic would
//!   otherwise be pinned to its stale stepped pose. A copy without a collider gets one from
//!   [`ReplicatedBody::shape`].
//!
//! Not in scope (ADR-2902): client prediction of pushed bodies (spec 73 § 14 defers it; proxies
//! follow the server), parented bodies (the stepped pose is a world pose; a body is replicated as a
//! root), and owner-simulated bodies.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_net::profile::{NetProfile, NetProfiles, RelevanceShape};
use regia_net::replication::{NetPriorityScale, NetVelocity, ReplicateOptions, register_replicated};
use regia_net::{NetAsleep, NetId, NetMode, NetRole, NetSet, Replicate};
use regia_physics::{
    BodyKind, Collider, ColliderShape, NoInterpolation, PhysicsPose, PhysicsStep, PhysicsWorld,
    PreviousPhysicsPose, RigidBody, RigidBodyHandle, ShapeAxis, Teleport, Velocity,
};
use regia_scene::Transform;

/// The name [`NetBodyPlugin`] registers [`body_profile`] under in `NetProfiles`.
pub const BODY_PROFILE: &str = "body";

/// The collider a client copy (or a server body spawned without one) is given. `FromEntity`: the
/// entity brings its own `RigidBody` and `Collider` (a prefab), nothing is added.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Default, Replicate)]
pub enum NetBodyShape {
    /// The entity's own collider (a prefab carries it on every process).
    #[default]
    FromEntity,
    /// A box with these half extents (m).
    Cuboid {
        /// Half extents (m).
        half_extents: Vec3,
    },
    /// A sphere (m).
    Ball {
        /// Radius (m).
        radius: f32,
    },
    /// A Y-up capsule (m).
    Capsule {
        /// Half height of the cylinder part (m).
        half_height: f32,
        /// Radius (m).
        radius: f32,
    },
    /// A Y-up cylinder (barrels; m).
    Cylinder {
        /// Half height (m).
        half_height: f32,
        /// Radius (m).
        radius: f32,
    },
}

impl NetBodyShape {
    /// The physics shape (`None` for `FromEntity` or a shape with a non-positive size).
    pub fn collider_shape(self) -> Option<ColliderShape> {
        let ok = |v: f32| v.is_finite() && v > 0.0;
        match self {
            Self::FromEntity => None,
            Self::Cuboid { half_extents } => (ok(half_extents.x) && ok(half_extents.y) && ok(half_extents.z))
                .then_some(ColliderShape::Box { half_extents }),
            Self::Ball { radius } => ok(radius).then_some(ColliderShape::Sphere { radius }),
            Self::Capsule { half_height, radius } => {
                (ok(half_height) && ok(radius)).then_some(ColliderShape::Capsule {
                    half_height,
                    radius,
                    axis: ShapeAxis::Y,
                })
            }
            Self::Cylinder { half_height, radius } => {
                (ok(half_height) && ok(radius)).then_some(ColliderShape::Cylinder {
                    half_height,
                    radius,
                    axis: ShapeAxis::Y,
                })
            }
        }
    }
}

/// Opts a rigid body into network physics (ADR-2902). Put it on a replicated root entity next to
/// `Replicated` (transform `Movement`) and, on the server, a dynamic `RigidBody` (or a
/// [`NetBodyShape`] that is not `FromEntity`). Replicated: client copies know they are bodies.
#[derive(Component, Reflect, Clone, Debug, PartialEq, Replicate)]
#[reflect(Component, Default)]
pub struct ReplicatedBody {
    /// The collider for copies that have none (and for a server body spawned without one).
    pub shape: NetBodyShape,
    /// Linear speeds below this are sent as zero, m/s (a proxy then holds instead of drifting).
    pub rest_speed: f32,
    /// The replicated linear velocity is rounded to this step, m/s, so velocity noise makes no new
    /// state version.
    pub velocity_step: f32,
    /// Priority gained per m/s of speed: `NetPriorityScale = 1 + speed × this`.
    pub speed_priority: f32,
    /// The largest priority scale speed gives.
    pub max_speed_priority: f32,
}

impl Default for ReplicatedBody {
    fn default() -> Self {
        Self {
            shape: NetBodyShape::FromEntity,
            rest_speed: 0.02,
            velocity_step: 0.01,
            speed_priority: 0.5,
            max_speed_priority: 4.0,
        }
    }
}

impl ReplicatedBody {
    /// A body with this collider shape and default settings.
    pub fn with_shape(shape: NetBodyShape) -> Self {
        Self {
            shape,
            ..Self::default()
        }
    }

    /// The velocity the server replicates for a simulated linear velocity.
    pub fn wire_velocity(&self, linear: Vec3, asleep: bool) -> Vec3 {
        if asleep || !linear.is_finite() || linear.length() < self.rest_speed.max(0.0) {
            return Vec3::ZERO;
        }
        let step = self.velocity_step;
        if !(step.is_finite() && step > 0.0) {
            return linear;
        }
        (linear / step).round() * step
    }

    /// The priority scale a body moving at `speed` m/s gets.
    pub fn priority_for_speed(&self, speed: f32) -> f32 {
        let gain = if self.speed_priority.is_finite() {
            self.speed_priority.max(0.0)
        } else {
            0.0
        };
        let cap = if self.max_speed_priority.is_finite() {
            self.max_speed_priority.max(1.0)
        } else {
            1.0
        };
        if !speed.is_finite() {
            return cap;
        }
        (1.0 + speed.max(0.0) * gain).min(cap)
    }
}

/// Server bookkeeping of one driven body (not replicated, not saved).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct NetBodyState {
    /// The stepped pose last seen.
    pub translation: Vec3,
    /// The stepped rotation last seen.
    pub rotation: Quat,
    /// Rapier had the body asleep at the last step.
    pub asleep: bool,
    /// This bridge inserted the body's `NetAsleep` (only those are removed on wake).
    pub marked: bool,
}

/// Counters of the body bridge (agent `net.bodies`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetBodyStats {
    /// Bodies rapier put to sleep (server; each fall asleep counts once).
    pub fell_asleep: u64,
    /// Bodies rapier woke after sleeping (server).
    pub woke: u64,
    /// Steps on which a driven body moved (server).
    pub moved_steps: u64,
    /// Client copies turned kinematic (or given a kinematic body).
    pub proxies_followed: u64,
    /// Teleports through [`net_body_teleport`].
    pub teleports: u64,
}

/// The replicated-body profile: spatial relevance (150 m), position grid 1 mm (the spec 72 § 8
/// quantization; rotation is smallest-three, 10 bits per component, on the transform fast path),
/// a forced send every 15 ticks for a moving body, linear interpolation with the spec 74 defaults
/// (snap 5 m, extrapolation capped at 100 ms).
pub fn body_profile() -> NetProfile {
    NetProfile {
        priority: 1.0,
        relevance: RelevanceShape::Spatial {
            radius: 150.0,
            hysteresis: 0.15,
        },
        min_interval_ticks: 1,
        max_interval_ticks: Some(15),
        linger_ticks: 30,
        position_step_m: Some(0.001),
        ..NetProfile::default()
    }
}

/// The largest position error between a settled proxy and its server body that the profile
/// allows, m: half a grid step on each axis (`position_step_m`, √3 / 2 of a step in all) plus f32
/// rounding. Rotation adds [`BODY_ROTATION_TOLERANCE_RAD`].
pub fn body_position_tolerance(profile: &NetProfile) -> f32 {
    profile
        .position_step_m
        .map_or(1e-4, |s| s * 0.5 * 3.0f32.sqrt() + 1e-4)
}

/// The largest rotation error of a settled proxy, radians: the smallest-three wire rotation with
/// 10 bits per component (each kept component off by at most half of 1 / 1023 × √2).
pub const BODY_ROTATION_TOLERANCE_RAD: f32 = 0.005;

/// Moves a server body to `pose` at once: no blend on clients (the spec 71 § 9 teleport bit) and no
/// swept motion in rapier (`Teleport`). Returns false when `entity` is not a replicated body.
pub fn net_body_teleport(world: &mut World, entity: Entity, pose: Transform) -> bool {
    let Ok(mut ent) = world.get_entity_mut(entity) else {
        return false;
    };
    if !ent.contains::<ReplicatedBody>() {
        return false;
    }
    ent.insert((pose, Teleport));
    if let Some(mut v) = ent.get_mut::<Velocity>() {
        *v = Velocity::default();
    }
    let sent = regia_net::net_teleport(world, entity);
    if sent && let Some(mut stats) = world.get_resource_mut::<NetBodyStats>() {
        stats.teleports += 1;
    }
    sent
}

/// Registers the body component and profile on a world (what [`NetBodyPlugin`] does; call on every
/// process, in the same order as the other registrations).
pub fn install_net_body(world: &mut World) {
    register_replicated::<ReplicatedBody>(world, ReplicateOptions::default());
    world.init_resource::<NetBodyStats>();
    world.init_resource::<NetProfiles>();
    if let Some(mut profiles) = world.get_resource_mut::<NetProfiles>()
        && profiles.get(BODY_PROFILE).is_none()
        && let Err(error) = profiles.insert(BODY_PROFILE, body_profile())
    {
        tracing::error!(target: "net", %error, "net.body_profile_refused");
    }
}

/// Network physics for [`ReplicatedBody`] entities. Add after `NetPlugin` and `PhysicsPlugin` on
/// the server and on every client.
pub struct NetBodyPlugin;

impl Plugin for NetBodyPlugin {
    fn build(&self, app: &mut App) {
        install_net_body(app.world_mut());
        app.register_type::<ReplicatedBody>()
            .register_type::<NetBodyShape>()
            .add_systems(
                FixedUpdate,
                (
                    equip_bodies.after(NetSet::Apply).before(PhysicsStep),
                    drive_server_bodies.after(PhysicsStep).before(NetSet::Assign),
                ),
            );
    }
}

fn is_server(mode: Option<&NetMode>) -> bool {
    mode.is_some_and(|m| m.is_server())
}

type EquipQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ReplicatedBody,
        Option<&'static NetRole>,
        Option<&'static RigidBody>,
        Has<Collider>,
        Has<NoInterpolation>,
        Has<Velocity>,
    ),
    Or<(
        Added<ReplicatedBody>,
        Changed<ReplicatedBody>,
        Added<NetRole>,
        Changed<NetRole>,
        Changed<RigidBody>,
    )>,
>;

/// `FixedUpdate`, before the physics step: a server body spawned with a shape only gets a dynamic
/// body; a client proxy turns kinematic (or gets a kinematic body) and stops render interpolation.
fn equip_bodies(
    mode: Option<Res<NetMode>>,
    mut stats: ResMut<NetBodyStats>,
    bodies: EquipQuery,
    mut commands: Commands,
) {
    let server = is_server(mode.as_deref());
    for (e, rb, role, body, has_collider, no_interp, has_velocity) in &bodies {
        let mut ec = commands.entity(e);
        if !has_collider && let Some(shape) = rb.shape.collider_shape() {
            ec.insert(Collider {
                shape,
                ..Collider::default()
            });
        }
        let proxy = !server && role.is_none_or(|r| *r == NetRole::Proxy);
        if !proxy {
            if server && body.is_none() && rb.shape != NetBodyShape::FromEntity {
                ec.insert(RigidBody::dynamic());
                if !has_velocity {
                    ec.insert(Velocity::default());
                }
            }
            continue;
        }
        match body {
            Some(b) if b.kind == BodyKind::Dynamic => {
                let mut k = b.clone();
                k.kind = BodyKind::Kinematic;
                ec.insert(k);
                stats.proxies_followed += 1;
            }
            Some(_) => {}
            None => {
                ec.insert(RigidBody::kinematic());
                stats.proxies_followed += 1;
            }
        }
        if !no_interp {
            ec.insert(NoInterpolation);
        }
        ec.remove::<(PhysicsPose, PreviousPhysicsPose)>();
    }
}

type DriveQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ReplicatedBody,
        &'static RigidBodyHandle,
        &'static RigidBody,
        &'static mut Transform,
        Option<&'static PhysicsPose>,
        Option<&'static mut NetVelocity>,
        Option<&'static mut NetPriorityScale>,
        Has<NetAsleep>,
        Option<&'static mut NetBodyState>,
    ),
    Without<ChildOf>,
>;

/// `FixedUpdate`, after the physics step and before the gather (server): the stepped pose,
/// velocity and sleep state of every dynamic [`ReplicatedBody`] drive its replicated transform,
/// `NetVelocity`, `NetPriorityScale` and `NetAsleep` (span `net_body_drive`).
fn drive_server_bodies(
    mode: Option<Res<NetMode>>,
    physics: Option<Res<PhysicsWorld>>,
    mut stats: ResMut<NetBodyStats>,
    mut bodies: DriveQuery,
    mut commands: Commands,
) {
    if !is_server(mode.as_deref()) {
        return;
    }
    let Some(physics) = physics else {
        return;
    };
    let _span = tracing::trace_span!("net_body_drive").entered();
    for (e, rb, handle, body, mut transform, pose, velocity, scale, has_asleep, state) in &mut bodies {
        if body.kind != BodyKind::Dynamic {
            continue;
        }
        let Some(simulated) = physics.bodies.get(handle.to_handle()) else {
            continue;
        };
        let asleep = simulated.is_sleeping();
        let linear = regia_physics::convert::from_rapier_vec3(simulated.linvel());
        let (translation, rotation) = pose.map_or((transform.translation, transform.rotation), |p| {
            (p.translation, p.rotation)
        });
        let mut ec = commands.entity(e);
        let marked = match state {
            Some(mut st) => {
                if st.translation != translation || st.rotation != rotation {
                    // The step wrote the pose without change detection: tell the gather.
                    transform.set_changed();
                    st.translation = translation;
                    st.rotation = rotation;
                    stats.moved_steps += 1;
                }
                if asleep && !st.asleep {
                    stats.fell_asleep += 1;
                } else if !asleep && st.asleep {
                    stats.woke += 1;
                }
                st.asleep = asleep;
                let marked = st.marked;
                if asleep && !has_asleep {
                    st.marked = true;
                } else if !asleep && has_asleep && marked {
                    st.marked = false;
                }
                marked
            }
            None => {
                transform.set_changed();
                ec.insert(NetBodyState {
                    translation,
                    rotation,
                    asleep,
                    marked: asleep && !has_asleep,
                });
                false
            }
        };
        let wire = rb.wire_velocity(linear, asleep);
        match velocity {
            Some(mut v) => {
                v.set_if_neq(NetVelocity(wire));
            }
            None => {
                ec.insert(NetVelocity(wire));
            }
        }
        let wanted = NetPriorityScale(rb.priority_for_speed(if asleep { 0.0 } else { linear.length() }));
        match scale {
            Some(mut s) => {
                s.set_if_neq(wanted);
            }
            None => {
                ec.insert(wanted);
            }
        }
        if asleep && !has_asleep {
            ec.insert(NetAsleep);
        } else if !asleep && has_asleep && marked {
            ec.remove::<NetAsleep>();
        }
    }
}

// ---- reports (agent `net.bodies`) ---------------------------------------------------------------

/// One replicated body as the agent reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct NetBodyRow {
    /// The local entity.
    pub entity: Entity,
    /// Its network id.
    pub net_id: Option<NetId>,
    /// `authority`, `proxy`, `predicted` or `local` (not replicated yet).
    pub role: &'static str,
    /// Asleep: rapier sleeping on the server; held at zero velocity on a client.
    pub asleep: bool,
    /// Its kinematic or dynamic body kind (`none` without a body).
    pub kind: &'static str,
    /// Position.
    pub position: Vec3,
    /// Replicated linear velocity.
    pub velocity: Vec3,
    /// State bytes per second sent for it, summed over connections (server with the spec 72
    /// pipeline; `None` otherwise).
    pub bytes_per_second: Option<f64>,
}

/// What the agent's `net.bodies` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct NetBodyReport {
    /// Whether this process is the server.
    pub server: bool,
    /// Replicated bodies on this process.
    pub bodies: usize,
    /// Of those, asleep.
    pub asleep: usize,
    /// Of those, kinematic proxies (client).
    pub proxies: usize,
    /// State bytes per second for all bodies, summed over connections (server with the spec 72
    /// pipeline; `None` otherwise).
    pub bytes_per_second: Option<f64>,
    /// The bridge counters.
    pub stats: NetBodyStats,
    /// The rows (at most `limit`, by entity order).
    pub rows: Vec<NetBodyRow>,
}

/// Whether [`NetBodyPlugin`] (or [`install_net_body`]) ran on this world.
pub fn net_bodies_installed(world: &World) -> bool {
    world.get_resource::<NetBodyStats>().is_some()
}

fn role_name(role: Option<&NetRole>) -> &'static str {
    match role {
        Some(NetRole::Authority) => "authority",
        Some(NetRole::Proxy) => "proxy",
        Some(NetRole::Predicted) => "predicted",
        Some(NetRole::Local) | None => "local",
    }
}

fn kind_name(body: Option<&RigidBody>) -> &'static str {
    match body.map(|b| b.kind) {
        Some(BodyKind::Dynamic) => "dynamic",
        Some(BodyKind::Kinematic) => "kinematic",
        Some(BodyKind::Static) => "static",
        None => "none",
    }
}

/// The body report of this process (`net.bodies`), with at most `limit` rows.
pub fn net_body_report(world: &mut World, limit: usize) -> NetBodyReport {
    let mut q = world.query_filtered::<(
        Entity,
        Option<&NetId>,
        Option<&NetRole>,
        Option<&RigidBody>,
        &Transform,
        Option<&NetVelocity>,
        Has<NetAsleep>,
        Option<&NetBodyState>,
    ), With<ReplicatedBody>>();
    let world: &World = world;
    let server = is_server(world.get_resource::<NetMode>());
    let tick_second = world
        .get_resource::<regia_net::runtime::NetNode>()
        .map_or(0, |n| {
            (f64::from(n.tick.0) / regia_net::send::DEFAULT_TICK_HZ) as u64
        });
    let interest = world
        .get_resource::<regia_net::relevance::NetInterest>()
        .filter(|_| server);
    let rate_of = |id: Option<NetId>| -> Option<f64> {
        let interest = interest?;
        let id = id?;
        Some(
            interest
                .peers
                .values()
                .filter_map(|pi| pi.send.bandwidth.by_entity.get(&id))
                .map(|w| w.per_second(tick_second))
                .sum(),
        )
    };
    let mut found: Vec<NetBodyRow> = q
        .iter(world)
        .map(|(entity, id, role, body, t, v, has_asleep, st)| {
            let velocity = v.map_or(Vec3::ZERO, |v| v.0);
            let asleep = if server {
                st.is_some_and(|s| s.asleep) || has_asleep
            } else {
                velocity == Vec3::ZERO
            };
            NetBodyRow {
                entity,
                net_id: id.copied(),
                role: role_name(role),
                asleep,
                kind: kind_name(body),
                position: t.translation,
                velocity,
                bytes_per_second: rate_of(id.copied()),
            }
        })
        .collect();
    found.sort_by_key(|r| r.entity);
    let bodies = found.len();
    let asleep = found.iter().filter(|r| r.asleep).count();
    let proxies = found.iter().filter(|r| r.role == "proxy").count();
    let bytes_per_second = interest.map(|_| found.iter().filter_map(|r| r.bytes_per_second).sum());
    found.truncate(limit);
    NetBodyReport {
        server,
        bodies,
        asleep,
        proxies,
        bytes_per_second,
        stats: world.get_resource::<NetBodyStats>().copied().unwrap_or_default(),
        rows: found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_velocity_rounds_and_rests() {
        let rb = ReplicatedBody::default();
        assert_eq!(rb.wire_velocity(Vec3::new(0.011, 0.0, 0.0), false), Vec3::ZERO);
        let v = rb.wire_velocity(Vec3::new(1.234, -0.006, 0.0), false);
        assert!((v.x - 1.23).abs() < 1e-6 && (v.y + 0.01).abs() < 1e-6, "{v}");
        assert_eq!(rb.wire_velocity(Vec3::new(5.0, 0.0, 0.0), true), Vec3::ZERO);
        assert_eq!(rb.wire_velocity(Vec3::NAN, false), Vec3::ZERO);
    }

    #[test]
    fn priority_grows_with_speed_up_to_the_cap() {
        let rb = ReplicatedBody::default();
        assert_eq!(rb.priority_for_speed(0.0), 1.0);
        assert!((rb.priority_for_speed(2.0) - 2.0).abs() < 1e-6);
        assert_eq!(rb.priority_for_speed(100.0), 4.0);
        assert_eq!(rb.priority_for_speed(f32::NAN), 4.0);
    }

    #[test]
    fn shapes_refuse_degenerate_sizes() {
        assert!(NetBodyShape::FromEntity.collider_shape().is_none());
        assert!(NetBodyShape::Ball { radius: 0.0 }.collider_shape().is_none());
        assert!(
            NetBodyShape::Cuboid {
                half_extents: Vec3::new(0.5, f32::NAN, 0.5)
            }
            .collider_shape()
            .is_none()
        );
        assert!(
            NetBodyShape::Cylinder {
                half_height: 0.4,
                radius: 0.3
            }
            .collider_shape()
            .is_some()
        );
    }

    #[test]
    fn the_body_profile_is_valid_and_registered() {
        let p = body_profile();
        assert!(p.validate().is_ok());
        let mut world = World::new();
        install_net_body(&mut world);
        assert!(world.resource::<NetProfiles>().get(BODY_PROFILE).is_some());
        assert!(net_bodies_installed(&world));
        assert!((body_position_tolerance(&p) - (0.0005 * 3.0f32.sqrt() + 1e-4)).abs() < 1e-7);
    }
}
