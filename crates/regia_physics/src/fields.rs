// SPDX-License-Identifier: MPL-2.0
//! Gameplay force fields (ADR-3348, `book/agents/physics-fields.md`): jump pads, wind tunnels,
//! gravity wells, slow zones and vortices acting on rigid bodies.
//!
//! A [`ForceField`] entity carries one effect ([`FieldKind`]) and a region ([`FieldVolume`]:
//! the whole world, a sphere, a box or a capsule placed by the entity's transform). Each
//! `FixedUpdate`, [`apply_force_fields`] runs after the property/velocity sync and before the
//! rapier step and folds every field a dynamic body is inside into that body's velocity for
//! the step (`v' = v (1 - drag dt) + a dt + dv`), in a fixed order: bodies sorted by entity,
//! fields latest spawned first (spawn order, ADR-1347). Disabled bodies (frozen by streaming,
//! parked) are left alone. Drag is applied before the step integrates gravity, so a body
//! falling through a linear drag zone settles on `m g / k` exactly in the discrete system.
//!
//! - Effects: directional, radial (attract or repel, with a falloff), vortex (tangential speed
//!   limit plus inward pull), gravity override, linear and angular drag, impulse pad.
//! - [`FieldMode::Force`] divides by the body's mass (newtons, drag in N s/m);
//!   [`FieldMode::Acceleration`] is mass independent.
//! - [`ForceField::mask`] selects which collision layers a field touches: a body is affected
//!   when any collider attached to it (its own or a child entity's, compound bodies included)
//!   sits on a selected layer (a bare [`crate::CollisionLayer`] on the body, else layer 0, is
//!   the fallback when the body has no colliders at all).
//! - Nothing runs while no [`ForceField`] exists: the system is behind a run condition.
//!
//! Fields write the body's velocity, not [`crate::ExternalForce`], so they never fight a
//! gameplay force; a body inside a non-zero field is woken each step, like any body under a
//! steady push.
//!
//! Provenance: original design. The inverse-square law is Newton's `F = k / r²`; linear drag
//! `F = -k v` and quadratic drag `F = -½ ρ Cd A |v| v` are textbook fluid resistance
//! (Wikipedia, "Drag (physics)", "Terminal velocity"); the semi-implicit velocity update is the
//! standard symplectic Euler step (Wikipedia, "Semi-implicit Euler method").

use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_ecs::spawn_order::{SpawnKey, SpawnOrder};
use regia_scene::level::LevelSettings;
use regia_scene::{GlobalTransform, Transform};
use tracing::{debug_span, instrument};

use crate::components::{Collider, CollisionLayer, RigidBodyHandle};
use crate::convert::{from_rapier_vec3, to_rapier_vec3};
use crate::world::{Gravity, PhysicsSettings, PhysicsWorld};

/// The region a [`ForceField`] acts in, in the field entity's local space (scale is ignored).
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default)]
pub enum FieldVolume {
    /// Everywhere (wind over the whole level, global drag). Radial and vortex fields use the
    /// entity's position as their center.
    #[default]
    Global,
    /// A ball around the entity's position.
    Sphere {
        /// Radius in meters.
        radius: f32,
    },
    /// An oriented box centered on the entity.
    Box {
        /// Half extents along the entity's local axes.
        half_extents: Vec3,
    },
    /// A capsule along the entity's local Y axis.
    Capsule {
        /// Half the length of the straight part.
        half_height: f32,
        /// Radius of the caps and the cylinder.
        radius: f32,
    },
}

/// Whether a field's numbers are forces or accelerations.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum FieldMode {
    /// Newtons (drag: N s/m, impulse: kg m/s): the body's mass divides the effect.
    #[default]
    Force,
    /// m/s² (drag: 1/s, impulse: m/s velocity change): the same for every body.
    Acceleration,
}

/// How a radial or vortex field fades with distance `d` from its center.
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default)]
pub enum Falloff {
    /// Constant strength.
    #[default]
    None,
    /// `1 - d / range`, zero past `range`.
    Linear {
        /// Distance where the field reaches zero (meters; zero or less means always zero).
        range: f32,
    },
    /// `1 / max(d, min_distance)²`, so `strength` is the force at one meter.
    InverseSquare {
        /// Distance below which the force stops growing (floor 0.001).
        min_distance: f32,
    },
    /// Piecewise linear `(distance, factor)` points sorted by distance, clamped at both ends.
    Curve(Vec<(f32, f32)>),
}

impl Falloff {
    /// The strength factor at distance `d` (meters).
    pub fn factor(&self, d: f32) -> f32 {
        match self {
            Falloff::None => 1.0,
            Falloff::Linear { range } => {
                if *range > 0.0 {
                    (1.0 - d / range).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            Falloff::InverseSquare { min_distance } => {
                let r = d.max(min_distance.max(0.001));
                1.0 / (r * r)
            }
            Falloff::Curve(points) => {
                let (Some(first), Some(last)) = (points.first(), points.last()) else {
                    return 1.0;
                };
                if d <= first.0 {
                    return first.1;
                }
                if d >= last.0 {
                    return last.1;
                }
                for pair in points.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    if d <= b.0 {
                        let span = b.0 - a.0;
                        if span <= 0.0 {
                            return b.1;
                        }
                        return a.1 + (b.1 - a.1) * ((d - a.0) / span);
                    }
                }
                last.1
            }
        }
    }
}

/// What a [`ForceField`] does to a body inside it.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub enum FieldKind {
    /// A constant push `vector` (field-local axes, so a rotated wind tunnel blows along its axis).
    Directional {
        /// Force (N) or acceleration (m/s²) in the field's local space.
        vector: Vec3,
    },
    /// A pull toward the center (positive `strength`) or push away (negative).
    Radial {
        /// Newtons or m/s² at falloff factor 1; with an inverse-square falloff, N m² or m³/s².
        strength: f32,
        /// How the strength fades with distance.
        falloff: Falloff,
    },
    /// A swirl around the field's local `axis` through its center.
    Vortex {
        /// Push along the swirl direction (counterclockwise about `axis`).
        tangential: f32,
        /// Pull toward the axis.
        inward: f32,
        /// The swirl axis in field-local space.
        axis: Vec3,
        /// The tangential push fades to zero as the body's swirl speed reaches this (m/s) and
        /// reverses above it, so the speed settles here; zero means no limit.
        max_speed: f32,
        /// How both strengths fade with the distance from the axis.
        falloff: Falloff,
    },
    /// Replaces world gravity inside the volume (m/s², the mode is ignored).
    Gravity {
        /// The gravity acceleration in field-local space.
        gravity: Vec3,
    },
    /// Drag on linear velocity: `F = -coefficient v`.
    LinearDrag {
        /// N s/m in force mode, 1/s in acceleration mode.
        coefficient: f32,
    },
    /// Drag on angular velocity: `torque = -coefficient ω`.
    AngularDrag {
        /// N m s in force mode (divided by the mean principal inertia), 1/s in acceleration mode.
        coefficient: f32,
    },
    /// One impulse each time a body enters the volume (field-local axes). Force mode: kg m/s,
    /// the velocity changes by `J / m`; acceleration mode: the velocity changes by the value.
    ImpulsePad {
        /// The impulse.
        impulse: Vec3,
    },
}

impl Default for FieldKind {
    fn default() -> Self {
        FieldKind::Directional { vector: Vec3::ZERO }
    }
}

/// A force field (see the module docs). Add `Transform` to place a volume.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct ForceField {
    /// What it does.
    pub kind: FieldKind,
    /// Where it acts.
    pub volume: FieldVolume,
    /// Force or acceleration.
    pub mode: FieldMode,
    /// Bit `i` set: bodies on collision layer `i` are affected (default: all).
    pub mask: u32,
    /// A disabled field does nothing (an impulse pad forgets who is inside).
    pub enabled: bool,
    /// Among gravity overrides covering a body, the highest priority wins, then the latest
    /// spawned field.
    pub priority: i32,
}

impl Default for ForceField {
    fn default() -> Self {
        Self {
            kind: FieldKind::default(),
            volume: FieldVolume::Global,
            mode: FieldMode::Force,
            mask: u32::MAX,
            enabled: true,
            priority: 0,
        }
    }
}

impl ForceField {
    /// A field of `kind` over `volume`, force mode, all layers.
    pub fn new(kind: FieldKind, volume: FieldVolume) -> Self {
        Self {
            kind,
            volume,
            ..Self::default()
        }
    }

    /// Whether bodies on collision layer `layer` are affected.
    pub fn affects_layer(&self, layer: u16) -> bool {
        layer < 32 && self.mask & (1u32 << layer) != 0
    }
}

/// A field's world pose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldPose {
    /// World position of the field entity.
    pub position: Vec3,
    /// World rotation of the field entity.
    pub rotation: Quat,
}

impl FieldPose {
    /// The identity pose.
    pub const IDENTITY: Self = Self {
        position: Vec3::ZERO,
        rotation: Quat::IDENTITY,
    };
}

/// What the field system needs to know about a body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodySample {
    /// World position.
    pub position: Vec3,
    /// Linear velocity.
    pub velocity: Vec3,
    /// Angular velocity.
    pub angular_velocity: Vec3,
    /// Mass in kg (zero: force-mode effects are skipped).
    pub mass: f32,
    /// Mean principal angular inertia (kg m²) used by angular drag in force mode.
    pub inertia: f32,
}

/// One field's contribution to one body for one step.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FieldEffect {
    /// Linear acceleration (m/s²).
    pub accel: Vec3,
    /// Angular acceleration (rad/s²).
    pub ang_accel: Vec3,
    /// Linear drag rate (1/s).
    pub lin_drag: f32,
    /// Angular drag rate (1/s).
    pub ang_drag: f32,
    /// A gravity override (world m/s²).
    pub gravity: Option<Vec3>,
    /// The velocity change an entry impulse would cause (applied only on entry).
    pub pad_dv: Vec3,
}

impl FieldVolume {
    /// Whether world point `p` is inside the volume placed at `pose`.
    pub fn contains(&self, pose: &FieldPose, p: Vec3) -> bool {
        let local = pose.rotation.inverse() * (p - pose.position);
        match self {
            FieldVolume::Global => true,
            FieldVolume::Sphere { radius } => {
                // A negative radius is an empty volume, like a negative box half extent:
                // the plain `radius * radius` square made `-r` act like `+r`.
                *radius >= 0.0 && local.length_squared() <= radius * radius
            }
            FieldVolume::Box { half_extents } => {
                local.x.abs() <= half_extents.x
                    && local.y.abs() <= half_extents.y
                    && local.z.abs() <= half_extents.z
            }
            FieldVolume::Capsule { half_height, radius } => {
                // `f32::clamp` panics when the bounds are unordered, and `NaN <= NaN` is
                // false. A non-finite capsule is empty, the same as a non-finite body sample.
                if !half_height.is_finite() || !radius.is_finite() {
                    return false;
                }
                if *radius < 0.0 {
                    // As the sphere above: a negative radius is an empty volume.
                    return false;
                }
                let y = local.y.clamp(-half_height.abs(), half_height.abs());
                Vec3::new(local.x, local.y - y, local.z).length_squared() <= radius * radius
            }
        }
    }
}

impl ForceField {
    /// What this field does to `body` this step. A body outside the volume, a disabled field
    /// and non-finite inputs give no effect (`None`).
    pub fn effect(&self, pose: &FieldPose, body: &BodySample) -> Option<FieldEffect> {
        if !self.enabled || !self.volume.contains(pose, body.position) {
            return None;
        }
        if !(body.position.is_finite() && body.velocity.is_finite() && body.angular_velocity.is_finite()) {
            return None;
        }
        // Force mode needs a mass; acceleration mode ignores it.
        let to_accel = match self.mode {
            FieldMode::Force => {
                if body.mass > 0.0 && body.mass.is_finite() {
                    1.0 / body.mass
                } else {
                    0.0
                }
            }
            FieldMode::Acceleration => 1.0,
        };
        let mut e = FieldEffect::default();
        match &self.kind {
            FieldKind::Directional { vector } => {
                e.accel = pose.rotation * *vector * to_accel;
            }
            FieldKind::Radial { strength, falloff } => {
                let to_center = pose.position - body.position;
                let d = to_center.length();
                if d > 1e-6 {
                    e.accel = to_center / d * (strength * falloff.factor(d) * to_accel);
                }
            }
            FieldKind::Vortex {
                tangential,
                inward,
                axis,
                max_speed,
                falloff,
            } => {
                let axis_w = (pose.rotation * *axis).normalize_or_zero();
                let r = body.position - pose.position;
                let r_perp = r - axis_w * r.dot(axis_w);
                let d = r_perp.length();
                if d > 1e-6 && axis_w != Vec3::ZERO {
                    let radial = r_perp / d;
                    let tangent = axis_w.cross(radial);
                    let f = falloff.factor(d);
                    // The swirl speed along the push: a clockwise swirl (negative `tangential`)
                    // measures against the counterclockwise tangent (corr-physics audit: it used
                    // the tangent as is, so a clockwise swirl never reached its limit).
                    let limit = if *max_speed > 0.0 {
                        let along = body.velocity.dot(tangent) * tangential.signum();
                        (1.0 - along / max_speed).clamp(-1.0, 1.0)
                    } else {
                        1.0
                    };
                    e.accel = (tangent * (tangential * f * limit) - radial * (inward * f)) * to_accel;
                }
            }
            FieldKind::Gravity { gravity } => {
                e.gravity = Some(pose.rotation * *gravity);
            }
            FieldKind::LinearDrag { coefficient } => {
                e.lin_drag = coefficient.max(0.0) * to_accel;
            }
            FieldKind::AngularDrag { coefficient } => {
                let inv_inertia = match self.mode {
                    FieldMode::Force => {
                        if body.inertia > 0.0 && body.inertia.is_finite() {
                            1.0 / body.inertia
                        } else {
                            0.0
                        }
                    }
                    FieldMode::Acceleration => 1.0,
                };
                e.ang_drag = coefficient.max(0.0) * inv_inertia;
            }
            FieldKind::ImpulsePad { impulse } => {
                e.pad_dv = pose.rotation * *impulse * to_accel;
            }
        }
        Some(e)
    }

    /// Whether this field is an entry impulse pad.
    pub fn is_pad(&self) -> bool {
        matches!(self.kind, FieldKind::ImpulsePad { .. })
    }
}

/// The force a mass would feel at a point (agent `physics.field_probe`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldProbe {
    /// Total force (N) from directional, radial, vortex and drag fields.
    pub force: Vec3,
    /// Total torque (N m) from angular drag, using the probe's inertia.
    pub torque: Vec3,
    /// The winning gravity override, if any, and the force it adds on top of world gravity
    /// (`m (g_override - g_world)`).
    pub gravity: Option<Vec3>,
    /// The impulse (kg m/s) a body entering here would get from pads.
    pub impulse: Vec3,
    /// Per field `(entity, force)` of the fields that cover the point, in evaluation order.
    pub per_field: Vec<(Entity, Vec3)>,
}

/// Sums the fields in `fields` (already in the step's fold order: latest spawned first) at
/// `sample` for a body on `layer`.
/// Drag contributes `-rate m v`. Pure: used by the agent probe and the tests.
pub fn probe_fields<'a>(
    fields: impl IntoIterator<Item = (Entity, &'a ForceField, FieldPose)>,
    sample: &BodySample,
    layer: u16,
    world_gravity: Vec3,
) -> FieldProbe {
    let mut out = FieldProbe::default();
    let mut best: Option<(i32, Vec3)> = None;
    for (entity, field, pose) in fields {
        if !field.affects_layer(layer) {
            continue;
        }
        let Some(e) = field.effect(&pose, sample) else {
            continue;
        };
        let force = (e.accel - sample.velocity * e.lin_drag) * sample.mass;
        out.force += force;
        out.torque -= sample.angular_velocity * (e.ang_drag * sample.inertia);
        out.impulse += e.pad_dv * sample.mass;
        if let Some(g) = e.gravity
            && best.is_none_or(|(p, _)| field.priority > p)
        {
            best = Some((field.priority, g));
        }
        out.per_field.push((entity, force));
    }
    out.gravity = best.map(|(_, g)| (g - world_gravity) * sample.mass);
    out
}

/// A field with its world pose, collected once per step.
#[derive(Debug, Clone)]
struct FieldSlot {
    entity: Entity,
    pose: FieldPose,
}

/// The sort key of a field: its spawn order when tracked, else the entity id (a world without
/// the physics plugin, e.g. a bare test schedule).
fn field_key(order: Option<&SpawnOrder<ForceField>>, entity: Entity) -> SpawnKey {
    order.map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
        o.key(entity)
    })
}

/// Scratch buffers of [`apply_force_fields`], kept between steps so a steady scene allocates
/// nothing. `inside` is the sorted `(field, body)` set of impulse-pad occupants.
#[derive(Resource, Default)]
pub struct FieldScratch {
    fields: Vec<FieldSlot>,
    /// Each body: entity, handle, field-layer bits, whether it has any collider (own or child).
    bodies: Vec<(Entity, RigidBodyHandle, u32, bool)>,
    body_index: Vec<(u64, usize)>,
    inside_now: Vec<(Entity, Entity)>,
    inside_prev: Vec<(Entity, Entity)>,
    /// Number of impulse-pad entries delivered so far.
    pub pad_entries: u64,
}

impl FieldScratch {
    /// Forgets which (field, body) pairs are inside impulse pads, so a pad removed and re-added
    /// fires its entry impulse for bodies still inside.
    pub fn clear_occupancy(&mut self) {
        self.inside_now.clear();
        self.inside_prev.clear();
    }

    /// Forgets the impulse-pad occupancy of one field only, so that pad fires its entry impulse
    /// again if it comes back, while every other pad keeps its occupants (removing one field
    /// must not re-launch the bodies standing in the others). `retain` keeps the sort order.
    pub fn forget_field(&mut self, field: Entity) {
        self.inside_now.retain(|k| k.0 != field);
        self.inside_prev.retain(|k| k.0 != field);
    }
}

/// Run condition: at least one [`ForceField`] exists.
pub fn any_force_field(fields: Query<(), With<ForceField>>) -> bool {
    !fields.is_empty()
}

/// Forgets the removed [`ForceField`]'s impulse-pad occupancy (the run condition keeps
/// [`apply_force_fields`] from running while no field exists, so the scratch could not refresh
/// itself). Only that field's pairs go: the other pads keep their occupants, so removing or
/// unloading one field does not re-fire every pad a body still stands in.
pub fn clear_pad_occupancy(on: On<Remove<ForceField>>, mut scratch: ResMut<FieldScratch>) {
    scratch.forget_field(on.event().entity);
}

type FieldQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ForceField,
        Option<&'static GlobalTransform>,
        Option<&'static Transform>,
        Option<&'static ChildOf>,
    ),
>;

/// Colliders on child entities (compound bodies): no body of their own, attached to the nearest
/// ancestor body.
type ChildCollidersQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static Collider), (Without<RigidBodyHandle>, With<ChildOf>)>;

/// The world pose of a field entity. A root entity's `Transform` is its world pose and is current
/// in `FixedUpdate`; a child's pose is composed from its `Transform` chain (see
/// [`world_field_pose`]), which is current in `FixedUpdate`, unlike [`GlobalTransform`], which
/// only propagates in `PostUpdate`. Either falls back to the other, then to the identity.
pub fn field_pose(global: Option<&GlobalTransform>, local: Option<&Transform>, is_child: bool) -> FieldPose {
    let from_global = |g: &GlobalTransform| {
        let (_, rotation, translation) = g.0.to_scale_rotation_translation();
        FieldPose {
            position: translation,
            rotation,
        }
    };
    let from_local = |t: &Transform| FieldPose {
        position: t.translation,
        rotation: t.rotation,
    };
    match (is_child, global, local) {
        (true, Some(g), _) => from_global(g),
        (_, _, Some(t)) => from_local(t),
        (_, Some(g), None) => from_global(g),
        _ => FieldPose::IDENTITY,
    }
}

/// Composes the `Transform` chain starting at `current` and walking up through the `ChildOf`
/// ancestors. A non-finite local transform composes like the identity (ADR-1507); a hierarchy
/// loop stops after 64 nodes.
fn compose_chain(
    mut current: Option<Entity>,
    child_of: &dyn Fn(Entity) -> Option<Entity>,
    transform_at: &dyn Fn(Entity) -> Option<Transform>,
) -> Affine3A {
    let mut world = Affine3A::IDENTITY;
    let mut guard = 0;
    while let Some(node) = current {
        guard += 1;
        if guard > 64 {
            break; // hierarchy loop guard
        }
        if let Some(local) = transform_at(node)
            && local.translation.is_finite()
            && local.rotation.is_finite()
        {
            world = local.compute_affine() * world;
        }
        current = child_of(node);
    }
    world
}

/// The world pose of `entity`, composed from its own `Transform` and its `ChildOf` ancestors'
/// (current within the fixed step, unlike [`GlobalTransform`], which propagates in `PostUpdate`).
/// Entity lookup goes through the two closures so both systems (queries) and tooling (a `World`)
/// can share it.
pub fn world_field_pose(
    entity: Entity,
    child_of: &dyn Fn(Entity) -> Option<Entity>,
    transform_at: &dyn Fn(Entity) -> Option<Transform>,
) -> FieldPose {
    let (_, rotation, translation) =
        compose_chain(Some(entity), child_of, transform_at).to_scale_rotation_translation();
    FieldPose {
        position: translation,
        rotation,
    }
}

/// The world affine of `entity`'s `ChildOf` ancestors (excluding `entity`'s own transform):
/// what a child's local `Transform` is relative to.
pub fn ancestor_world_affine(
    entity: Entity,
    child_of: &dyn Fn(Entity) -> Option<Entity>,
    transform_at: &dyn Fn(Entity) -> Option<Transform>,
) -> Affine3A {
    compose_chain(child_of(entity), child_of, transform_at)
}

/// The local translation and rotation that place world `pose` under a parent whose world affine
/// is `parent` (parent scale is honored, as in `compose_chain`).
pub fn localize_under_parent(pose: FieldPose, parent: Affine3A) -> (Vec3, Quat) {
    let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
    (
        parent.inverse().transform_point3(pose.position),
        parent_rotation.inverse() * pose.rotation,
    )
}

/// The layer-select bit for a collision layer index (indices ≥ 32 can never match a mask,
/// like [`ForceField::affects_layer`]).
fn layer_bit(index: u16) -> u32 {
    if index < 32 { 1u32 << index } else { 0 }
}

/// Applies every [`ForceField`] to the dynamic bodies it covers (see the module docs).
#[instrument(skip_all, target = "regia_physics")]
#[allow(clippy::too_many_arguments)]
pub fn apply_force_fields(
    settings: Res<PhysicsSettings>,
    level: Option<Res<LevelSettings>>,
    gravity: Res<Gravity>,
    fixed: Res<regia_app::FixedTime>,
    mut physics: ResMut<PhysicsWorld>,
    mut scratch: ResMut<FieldScratch>,
    order: Option<Res<SpawnOrder<ForceField>>>,
    fields: FieldQuery,
    bodies: Query<(
        Entity,
        &'static RigidBodyHandle,
        Option<&'static Collider>,
        Option<&'static CollisionLayer>,
    )>,
    body_handles: Query<&'static RigidBodyHandle>,
    parents: Query<&'static ChildOf>,
    transforms: Query<&'static Transform>,
    child_colliders: ChildCollidersQuery,
) {
    let _span = debug_span!("apply_force_fields").entered();
    let dt = fixed.step().as_secs_f32();
    let requested = level.map(|l| l.gravity).unwrap_or(settings.gravity);
    // Same non-finite fallback as the step (ADR-1507): the last finite gravity is kept, so a
    // gravity override below cancels exactly what the step integrates.
    let world_gravity = if requested.is_finite() {
        requested
    } else {
        let last = gravity.0;
        let last = if last.x.is_finite() && last.y.is_finite() && last.z.is_finite() {
            last
        } else {
            Gravity::default().0
        };
        from_rapier_vec3(last)
    };

    let scratch = &mut *scratch;
    scratch.fields.clear();
    for (entity, field, global, local, child) in &fields {
        if field.enabled {
            // A child entity's GlobalTransform only propagates in PostUpdate (one frame stale
            // here in FixedUpdate): compose the Transform chain instead, like collider offsets.
            let pose = if child.is_some() {
                world_field_pose(entity, &|e| parents.get(e).ok().map(|p| p.parent()), &|e| {
                    transforms.get(e).copied().ok()
                })
            } else {
                field_pose(global, local, false)
            };
            scratch.fields.push(FieldSlot { entity, pose });
        }
    }
    // Spawn order, not entity ids: parallel `Commands::spawn` reserves ids in thread-timing order
    // (ADR-1347), and the fold order decides float sums and the gravity tie-break. Latest spawned
    // first, which is the order fresh entity ids gave before (they are handed out downward), so
    // existing scenes fold as they did. Unstable sort: the keys are unique, and it allocates
    // nothing.
    let order = order.as_deref();
    scratch
        .fields
        .sort_unstable_by_key(|f| std::cmp::Reverse(field_key(order, f.entity)));
    scratch.bodies.clear();
    for (entity, handle, collider, _) in &bodies {
        // The body's own collider's layer is the base; child colliders add theirs below. The
        // bare-`CollisionLayer`/layer-0 fallback applies only when the body has no colliders at
        // all (own or child).
        scratch.bodies.push((
            entity,
            *handle,
            collider.map_or(0, |c| layer_bit(c.layer.index)),
            collider.is_some(),
        ));
    }
    scratch.bodies.sort_unstable_by_key(|b| b.0.to_bits());
    scratch.body_index.clear();
    for (i, (entity, _, _, _)) in scratch.bodies.iter().enumerate() {
        scratch.body_index.push((entity.to_bits(), i));
    }
    // Layers of child-entity colliders count too (compound bodies): a body is on every layer
    // any of its attached colliders is on.
    for (collider_entity, collider) in &child_colliders {
        let mut current = parents.get(collider_entity).ok().map(|p| p.parent());
        let mut owner = None;
        let mut guard = 0;
        while let Some(ancestor) = current {
            guard += 1;
            if guard > 64 {
                break; // hierarchy loop guard
            }
            if body_handles.get(ancestor).is_ok() {
                owner = Some(ancestor);
                break;
            }
            current = parents.get(ancestor).ok().map(|p| p.parent());
        }
        if let Some(owner) = owner
            && let Ok(i) = scratch.body_index.binary_search_by_key(&owner.to_bits(), |b| b.0)
        {
            scratch.bodies[i].2 |= layer_bit(collider.layer.index);
            scratch.bodies[i].3 = true;
        }
    }
    // A body with no colliders at all falls back to its bare `CollisionLayer`, else layer 0. A
    // body whose colliders are all on layers past 31 has no bits and stays on no field layer
    // (like `ForceField::affects_layer`), instead of being taken for one without colliders.
    for (entity, _, layers, has_collider) in scratch.bodies.iter_mut() {
        if !*has_collider {
            let fallback = bodies
                .get(*entity)
                .ok()
                .map_or(0, |(_, _, _, l)| l.map_or(0, |l| l.index));
            *layers = layer_bit(fallback);
        }
    }
    scratch.inside_now.clear();

    let FieldScratch {
        fields: field_slots,
        bodies,
        body_index: _,
        inside_now,
        inside_prev,
        pad_entries,
    } = scratch;

    // Occupancy of non-dynamic bodies is only kept when a pad exists (static levels pay nothing).
    let any_pad = field_slots.iter().any(|slot| {
        fields
            .get(slot.entity)
            .is_ok_and(|(_, field, _, _, _)| field.is_pad())
    });
    for (body_entity, handle, layers, _) in bodies.iter() {
        let Some(rb) = physics.bodies.get_mut(handle.to_handle()) else {
            continue;
        };
        if !rb.is_dynamic() && !any_pad {
            continue;
        }
        if !rb.is_dynamic() {
            // Fields push only dynamic bodies, but a kinematic or fixed body standing in a pad
            // still occupies it: switched to dynamic there, it has not entered (rc1 physics
            // review, ADR-7417). A disabled one keeps what it had, as below.
            let position = from_rapier_vec3(rb.translation());
            let enabled = rb.is_enabled();
            for slot in field_slots.iter() {
                let Ok((_, field, _, _, _)) = fields.get(slot.entity) else {
                    continue;
                };
                if !field.is_pad() || field.mask & *layers == 0 {
                    continue;
                }
                let key = (slot.entity, *body_entity);
                let inside = if enabled {
                    field.enabled && field.volume.contains(&slot.pose, position)
                } else {
                    inside_prev
                        .binary_search_by_key(&(key.0.to_bits(), key.1.to_bits()), |k| {
                            (k.0.to_bits(), k.1.to_bits())
                        })
                        .is_ok()
                };
                if inside {
                    inside_now.push(key);
                }
            }
            continue;
        }
        // A disabled body (frozen by streaming, a parked chassis) does not move, so fields must
        // not pile velocity onto it for its return (corr-physics audit). It keeps the pads it
        // was inside, so coming back does not count as entering them.
        if !rb.is_enabled() {
            inside_now.extend(
                inside_prev
                    .iter()
                    .filter(|k| k.1 == *body_entity && field_slots.iter().any(|f| f.entity == k.0))
                    .copied(),
            );
            continue;
        }
        let inertia = {
            let p = rb.mass_properties().local_mprops.principal_inertia();
            (p.x + p.y + p.z) / 3.0
        };
        let sample = BodySample {
            position: from_rapier_vec3(rb.translation()),
            velocity: from_rapier_vec3(rb.linvel()),
            angular_velocity: from_rapier_vec3(rb.angvel()),
            mass: rb.mass(),
            inertia,
        };
        let mut accel = Vec3::ZERO;
        let mut ang_accel = Vec3::ZERO;
        let mut lin_drag = 0.0f32;
        let mut ang_drag = 0.0f32;
        let mut best_gravity: Option<(i32, Vec3)> = None;
        let mut pad_dv = Vec3::ZERO;
        for slot in field_slots.iter() {
            let Ok((_, field, _, _, _)) = fields.get(slot.entity) else {
                continue;
            };
            if field.mask & *layers == 0 {
                continue;
            }
            let Some(e) = field.effect(&slot.pose, &sample) else {
                continue;
            };
            accel += e.accel;
            ang_accel += e.ang_accel;
            lin_drag += e.lin_drag;
            ang_drag += e.ang_drag;
            if let Some(g) = e.gravity
                && best_gravity.is_none_or(|(p, _)| field.priority > p)
            {
                best_gravity = Some((field.priority, g));
            }
            if field.is_pad() {
                let key = (slot.entity, *body_entity);
                inside_now.push(key);
                if inside_prev
                    .binary_search_by_key(&(key.0.to_bits(), key.1.to_bits()), |k| {
                        (k.0.to_bits(), k.1.to_bits())
                    })
                    .is_err()
                {
                    pad_dv += e.pad_dv;
                    *pad_entries += 1;
                }
            }
        }
        if let Some((_, g)) = best_gravity {
            accel += (g - world_gravity) * rb.gravity_scale();
        }
        let lin_factor = (1.0 - lin_drag * dt).max(0.0);
        let ang_factor = (1.0 - ang_drag * dt).max(0.0);
        let new_lin = sample.velocity * lin_factor + accel * dt + pad_dv;
        let new_ang = sample.angular_velocity * ang_factor + ang_accel * dt;
        if new_lin != sample.velocity && new_lin.is_finite() {
            rb.set_linvel(to_rapier_vec3(new_lin), true);
        }
        if new_ang != sample.angular_velocity && new_ang.is_finite() {
            rb.set_angvel(to_rapier_vec3(new_ang), true);
        }
    }

    // `inside_now` was built in (body, field) order; occupancy lookups need (field, body) order.
    inside_now.sort_unstable_by_key(|k| (k.0.to_bits(), k.1.to_bits()));
    std::mem::swap(inside_now, inside_prev);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corr-low P11: `radius * radius` made a negative sphere radius act like the positive one.
    /// A negative radius is an empty volume, like a negative box half extent. The capsule's
    /// radius had the same square.
    #[test]
    fn a_negative_radius_contains_nothing() {
        let pose = FieldPose::IDENTITY;
        let sphere = FieldVolume::Sphere { radius: -2.0 };
        assert!(!sphere.contains(&pose, Vec3::ZERO));
        assert!(!sphere.contains(&pose, Vec3::new(1.0, 0.0, 0.0)));
        let capsule = FieldVolume::Capsule {
            half_height: 1.0,
            radius: -1.0,
        };
        assert!(!capsule.contains(&pose, Vec3::ZERO));
        // A positive radius still holds its center, and a zero one its exact center.
        assert!(FieldVolume::Sphere { radius: 2.0 }.contains(&pose, Vec3::ZERO));
        assert!(FieldVolume::Sphere { radius: 0.0 }.contains(&pose, Vec3::ZERO));
    }

    /// One dynamic 1 kg body inside a global gravity override of (0, -2, 0), stepped `warm`
    /// updates; then, when `break_gravity`, the world gravity turns NaN for `after` more updates.
    /// Returns the body's linear velocity bits.
    fn override_run(break_gravity: bool, warm: u32, after: u32) -> [u32; 3] {
        use crate::components::{ColliderShape, Mass, RigidBody, Velocity};
        use regia_app::{App, TimeUpdateStrategy};

        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        app.insert_resource(PhysicsSettings {
            gravity: Vec3::new(0.0, -9.81, 0.0),
            ..PhysicsSettings::default()
        });
        app.add_plugins(crate::PhysicsPlugin);
        let body = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 100.0, 0.0)),
                RigidBody::dynamic(),
                Mass::Explicit {
                    mass: 1.0,
                    center_of_mass: Vec3::ZERO,
                    principal_inertia: Vec3::splat(0.5),
                },
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.05 },
                    sensor: true,
                    density: 0.0,
                    ..Collider::default()
                },
                Velocity::default(),
            ))
            .id();
        app.world_mut().spawn((
            ForceField::new(
                FieldKind::Gravity {
                    gravity: Vec3::new(0.0, -2.0, 0.0),
                },
                FieldVolume::Global,
            ),
            Transform::default(),
        ));
        for _ in 0..warm {
            app.update();
        }
        if break_gravity {
            app.world_mut().resource_mut::<PhysicsSettings>().gravity = Vec3::new(0.0, f32::NAN, 0.0);
        }
        for _ in 0..after {
            app.update();
        }
        let handle = app.world().get::<RigidBodyHandle>(body).map(|h| h.to_handle());
        let physics = app.world().resource::<PhysicsWorld>();
        let v = handle
            .and_then(|h| physics.bodies.get(h))
            .map_or(Vec3::splat(f32::NAN), |rb| from_rapier_vec3(rb.linvel()));
        v.to_array().map(f32::to_bits)
    }

    /// corr-low P10: on a non-finite world gravity the step keeps the last finite gravity, but
    /// the fields measured overrides against zero, so an override added the old gravity on top
    /// (the body fell at -11.81 m/s² instead of -2). The fields now use the gravity the step
    /// will use: the run matches one whose gravity never broke, bit for bit.
    #[test]
    fn a_non_finite_world_gravity_keeps_the_override_exact() {
        let steady = override_run(false, 10, 20);
        let broken = override_run(true, 10, 20);
        assert!(
            f32::from_bits(steady[1]) < -0.5,
            "the override pulled: {}",
            f32::from_bits(steady[1])
        );
        assert_eq!(
            broken,
            steady,
            "velocity {:?} vs {:?}",
            broken.map(f32::from_bits),
            steady.map(f32::from_bits)
        );
    }
}
