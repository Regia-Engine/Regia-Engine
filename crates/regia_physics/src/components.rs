// SPDX-License-Identifier: MPL-2.0
//! ECS components for physics bodies, colliders, joints and characters (specs/15 § 2).
//!
//! Rapier handles ([`RigidBodyHandle`], [`ColliderHandle`], [`JointHandle`]) are internal,
//! never saved, and inserted only by this crate's sync systems. [`BodyMarker`] is the M14
//! name of [`RigidBodyHandle`] and still works.
//!
//! The user-facing set: [`RigidBody`] (with [`BodyKind::Kinematic`]), [`Mass`], [`Collider`]
//! with every [`ColliderShape`], [`Velocity`], [`ExternalForce`]/[`ExternalImpulse`], [`Joint`]
//! with `break_force`, and [`CharacterBody`] for the character controller (§ 4).

use regia_core::math::{Quat, Vec3};
use regia_ecs::attrs::Range;
use regia_ecs::prelude::*;
use regia_scene::Transform;

// ---------------------------------------------------------------------------
// Rapier handle wrappers (internal, not saved; spec 15 § 1)
// ---------------------------------------------------------------------------

/// Wraps a rapier `RigidBodyHandle` as a component. Internal-only; never saved.
///
/// Rapier handles are (index, generation): a freed index is recycled with a new
/// generation, so both parts are stored (the M14 `from_raw` constructor keeps
/// generation 0).
#[derive(Component, Debug, Clone, Copy)]
pub struct RigidBodyHandle {
    /// Index of the rapier handle (valid only within one `PhysicsWorld` session).
    pub raw: u32,
    /// Generation of the rapier handle (invalidated when the index is recycled).
    pub generation: u32,
}

/// The M14 name of [`RigidBodyHandle`]; kept so cosmetic-debris code keeps compiling.
pub type BodyMarker = RigidBodyHandle;

/// Wraps a rapier `ColliderHandle` as a component. Internal-only; never saved.
#[derive(Component, Debug, Clone, Copy)]
pub struct ColliderHandle {
    /// Index of the rapier handle.
    pub raw: u32,
    /// Generation of the rapier handle (invalidated when the index is recycled).
    pub generation: u32,
}

/// Wraps a rapier `ImpulseJointHandle` as a component. Internal-only; never saved.
#[derive(Component, Debug, Clone, Copy)]
pub struct JointHandle {
    /// Index of the rapier handle.
    pub raw: u32,
    /// Generation of the rapier handle (invalidated when the index is recycled).
    pub generation: u32,
}

impl RigidBodyHandle {
    /// Creates a [`RigidBodyHandle`] from a rapier handle's index (generation 0 —
    /// the M14 debris API).
    pub fn from_raw(raw: u32) -> Self {
        Self { raw, generation: 0 }
    }

    /// Wraps a full rapier handle (index + generation).
    pub fn from_rapier(handle: rapier3d::dynamics::RigidBodyHandle) -> Self {
        let (raw, generation) = handle.into_raw_parts();
        Self { raw, generation }
    }

    /// Converts back to a rapier handle. Only valid within the same `PhysicsWorld` session.
    pub fn to_handle(&self) -> rapier3d::dynamics::RigidBodyHandle {
        rapier3d::dynamics::RigidBodyHandle::from_raw_parts(self.raw, self.generation)
    }
}

impl ColliderHandle {
    /// Creates a [`ColliderHandle`] from a rapier handle's index (generation 0).
    pub fn from_raw(raw: u32) -> Self {
        Self { raw, generation: 0 }
    }

    /// Wraps a full rapier handle (index + generation).
    pub fn from_rapier(handle: rapier3d::geometry::ColliderHandle) -> Self {
        let (raw, generation) = handle.into_raw_parts();
        Self { raw, generation }
    }

    /// Converts back to a rapier handle.
    pub fn to_handle(&self) -> rapier3d::geometry::ColliderHandle {
        rapier3d::geometry::ColliderHandle::from_raw_parts(self.raw, self.generation)
    }
}

impl JointHandle {
    /// Creates a [`JointHandle`] from a rapier handle's index (generation 0).
    pub fn from_raw(raw: u32) -> Self {
        Self { raw, generation: 0 }
    }

    /// Wraps a full rapier handle (index + generation).
    pub fn from_rapier(handle: rapier3d::dynamics::ImpulseJointHandle) -> Self {
        let (raw, generation) = handle.into_raw_parts();
        Self { raw, generation }
    }

    /// Converts back to a rapier handle.
    pub fn to_handle(&self) -> rapier3d::dynamics::ImpulseJointHandle {
        rapier3d::dynamics::ImpulseJointHandle::from_raw_parts(self.raw, self.generation)
    }
}

// ---------------------------------------------------------------------------
// RigidBody kind and component (spec 15 § 2)
// ---------------------------------------------------------------------------

/// Whether a body is dynamic, kinematic or static.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect, Default, Component)]
#[reflect(Component, Default)]
pub enum BodyKind {
    /// Moves under forces and collisions.
    #[default]
    Dynamic,
    /// Moved by gameplay writing its `Transform`; pushes dynamic bodies, never falls.
    Kinematic,
    /// Fixed in place, never moved.
    Static,
}

/// Per-axis translation/rotation locks (spec 15 § 2 `locked_axes`).
///
/// Stored as plain bools so it reflects and serializes; converted to rapier's bitflags
/// by the sync.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, Component)]
#[reflect(Component, Default)]
pub struct LockedAxes {
    /// Lock translation along local X.
    pub translation_x: bool,
    /// Lock translation along local Y.
    pub translation_y: bool,
    /// Lock translation along local Z.
    pub translation_z: bool,
    /// Lock rotation around local X.
    pub rotation_x: bool,
    /// Lock rotation around local Y.
    pub rotation_y: bool,
    /// Lock rotation around local Z.
    pub rotation_z: bool,
}

impl LockedAxes {
    /// None locked.
    pub const FREE: Self = Self {
        translation_x: false,
        translation_y: false,
        translation_z: false,
        rotation_x: false,
        rotation_y: false,
        rotation_z: false,
    };

    /// Converts to rapier's bitflags.
    pub fn to_rapier(self) -> rapier3d::dynamics::LockedAxes {
        use rapier3d::dynamics::LockedAxes as RapierLocked;
        let mut axes = RapierLocked::empty();
        axes.set(RapierLocked::TRANSLATION_LOCKED_X, self.translation_x);
        axes.set(RapierLocked::TRANSLATION_LOCKED_Y, self.translation_y);
        axes.set(RapierLocked::TRANSLATION_LOCKED_Z, self.translation_z);
        axes.set(RapierLocked::ROTATION_LOCKED_X, self.rotation_x);
        axes.set(RapierLocked::ROTATION_LOCKED_Y, self.rotation_y);
        axes.set(RapierLocked::ROTATION_LOCKED_Z, self.rotation_z);
        axes
    }
}

/// A physics body (spec 15 § 2). Add with a [`Transform`]; the sync creates the rapier body.
///
/// Dynamic bodies are simulation-driven (their `Transform` is written by physics); Kinematic
/// bodies are gameplay-driven (their `Transform` is pushed to physics as the next pose);
/// Static bodies never move.
///
/// # Example
///
/// ```
/// use regia_physics::{BodyKind, RigidBody};
///
/// let body = RigidBody::dynamic();
/// assert_eq!(body.kind, BodyKind::Dynamic);
/// assert_eq!(body.gravity_scale, 1.0); // full gravity
/// assert_eq!(RigidBody::kinematic().kind, BodyKind::Kinematic);
/// ```
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct RigidBody {
    /// How the body moves.
    pub kind: BodyKind,
    /// Linear damping (0 = none).
    #[reflect(@Range(0.0..=10.0))]
    pub linear_damping: f32,
    /// Angular damping (0 = none).
    #[reflect(@Range(0.0..=10.0))]
    pub angular_damping: f32,
    /// Multiplier on gravity (0 = weightless, negative = floats up).
    pub gravity_scale: f32,
    /// Continuous collision detection: prevents tunnelling at speed (costs time). A CCD body is
    /// swept against static geometry and against bodies without `ccd` at their end-of-step
    /// pose; it is not swept against another CCD body, and two fast bodies closing head-on can
    /// pass through each other (rapier's bullet rule, ADR-1387).
    pub ccd: bool,
    /// Whether the body may sleep when idle.
    pub can_sleep: bool,
    /// Per-axis motion locks.
    pub locked_axes: LockedAxes,
    /// Dominance group: a body never gets pushed by a body in a lower group (rapier dominance).
    pub dominance_group: i8,
}

impl Default for RigidBody {
    fn default() -> Self {
        Self {
            kind: BodyKind::default(),
            linear_damping: 0.0,
            angular_damping: 0.0,
            gravity_scale: 1.0,
            ccd: false,
            can_sleep: true,
            locked_axes: LockedAxes::FREE,
            dominance_group: 0,
        }
    }
}

impl RigidBody {
    /// A dynamic body with default settings.
    pub fn dynamic() -> Self {
        Self::default()
    }

    /// A kinematic body with default settings.
    pub fn kinematic() -> Self {
        Self {
            kind: BodyKind::Kinematic,
            ..Self::default()
        }
    }

    /// A static body with default settings.
    pub fn static_body() -> Self {
        Self {
            kind: BodyKind::Static,
            ..Self::default()
        }
    }
}

/// How a body's mass is defined (spec 15 § 2 `Mass`).
#[derive(Reflect, Debug, Clone, PartialEq, Default, Component)]
#[reflect(Component, Default)]
pub enum Mass {
    /// Mass computed from the colliders' shapes × `density` (kg/m³). The default.
    #[default]
    Auto,
    /// An explicit mass in kg, center of mass (local) and principal inertia (local, diagonal).
    ///
    /// The mass and each principal moment must be finite and strictly positive. A zero, negative
    /// or non-finite value is not pushed: on spawn the body keeps the collider mass, and a later
    /// edit keeps the mass already in the solver. The drop is counted in
    /// `PhysicsStats::non_finite_inputs`.
    Explicit {
        /// Mass in kilograms. Must be finite and strictly positive.
        mass: f32,
        /// Center of mass in the body's local space.
        center_of_mass: Vec3,
        /// Principal moments of inertia on the local diagonal. Each must be finite and strictly
        /// positive.
        principal_inertia: Vec3,
    },
}

/// Linear and angular velocity of a body (m/s, rad/s). Read/write: an edit is pushed to the
/// simulation on its change tick and applied once — afterwards the simulation owns the value
/// (so impulses from collisions and character pushes are not overwritten) and writes results
/// back after each step without marking the component changed again.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Velocity {
    /// Linear velocity (m/s).
    pub linear: Vec3,
    /// Angular velocity (rad/s).
    pub angular: Vec3,
}

/// A persistent force (N) and torque (τ) applied every step while present.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ExternalForce {
    /// Force in newtons (world space).
    pub force: Vec3,
    /// Torque in newton-metres.
    pub torque: Vec3,
}

/// A one-shot impulse (kg·m/s) applied on the next step, then the component is removed.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ExternalImpulse {
    /// Linear impulse in kg·m/s (world space).
    pub impulse: Vec3,
    /// Angular impulse in kg·m²/s.
    pub torque_impulse: Vec3,
}

/// Marker: teleport the body to its current `Transform` on the next step (bypassing the
/// solver: the jump sweeps nothing, adds no velocity and is not blended by interpolation). The
/// velocity the body already had is kept; [`teleport`](crate::teleport::teleport) with
/// [`TeleportVelocity::Clear`](crate::teleport::TeleportVelocity::Clear) also stops it (ADR-7233).
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct Teleport;

/// Per-body opt-out of transform interpolation (spec 15 § 1).
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct NoInterpolation;

/// The body's pose after the most recent fixed step (internal; never saved).
#[derive(Component, Debug, Clone, Copy)]
pub struct PhysicsPose {
    /// Translation after the last step.
    pub translation: Vec3,
    /// Rotation after the last step.
    pub rotation: Quat,
}

/// The body's pose after the fixed step before the most recent one (internal; never saved).
#[derive(Component, Debug, Clone, Copy)]
pub struct PreviousPhysicsPose {
    /// Translation at the previous step.
    pub translation: Vec3,
    /// Rotation at the previous step.
    pub rotation: Quat,
}

// ---------------------------------------------------------------------------
// Collider (spec 15 § 2)
// ---------------------------------------------------------------------------

/// The shape of a collider. All shapes that can be described without an asset are here
/// (spec 15 § 2 `ColliderShape`); `TriMesh` is for static/kinematic bodies only.
#[derive(Reflect, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
pub enum ColliderShape {
    /// Axis-aligned box with the given half-extents.
    Box {
        /// Half-extents (width/2, height/2, depth/2).
        half_extents: Vec3,
    },
    /// Sphere with the given radius.
    Sphere {
        /// Radius in metres.
        radius: f32,
    },
    /// Capsule with the given half-height (cylinder half) and radius.
    Capsule {
        /// Half-height of the cylindrical section.
        half_height: f32,
        /// Radius of the end caps.
        radius: f32,
        /// Long axis of the capsule (the shape is rotated into place by the sync).
        axis: ShapeAxis,
    },
    /// Cylinder with the given half-height and radius.
    Cylinder {
        /// Half-height.
        half_height: f32,
        /// Radius.
        radius: f32,
        /// Long axis.
        axis: ShapeAxis,
    },
    /// Cone with the given half-height and base radius.
    Cone {
        /// Half-height.
        half_height: f32,
        /// Base radius.
        radius: f32,
        /// Long axis.
        axis: ShapeAxis,
    },
    /// Convex hull from a set of points.
    ConvexHull {
        /// Points in the body's local space.
        points: Vec<Vec3>,
    },
    /// Triangle mesh (static/kinematic bodies only — never dynamic).
    TriMesh {
        /// Triangle vertices in local space.
        vertices: Vec<Vec3>,
        /// Triangle indices into `vertices`.
        indices: Vec<[u32; 3]>,
    },
    /// Heightfield for terrain ground collision. Build it from a terrain with
    /// `regia_terrain::TerrainMap::collision_heightfield`, whose fields map one to one.
    HeightField {
        /// Rows of the height matrix: the number of samples along **Z**.
        nx: usize,
        /// Columns of the height matrix: the number of samples along **X**.
        nz: usize,
        /// Heights, column-major: sample `(i, j)` (`i` along Z, `j` along X) is
        /// `heights[i + j * nx]`, so Z runs fastest. A terrain map's own heights are row-major
        /// with X fastest; copying them in unchanged mirrors the ground about X = Z. The field
        /// spans `[-0.5, 0.5] × scale` in X and Z around the collider. A NaN height is a hole (a
        /// tunnel mouth cut through the terrain, `regia_terrain::HolesMask::cut_heights`):
        /// every cell touching it is removed and collides with nothing.
        heights: Vec<f32>,
        /// World-space scale of the heightfield.
        scale: Vec3,
    },
    /// Several shapes at local offsets, moved as one collider.
    Compound {
        /// The child shapes with their local transforms.
        shapes: Vec<(Transform, ColliderShape)>,
    },
}

impl ColliderShape {
    /// Builds a static/kinematic trimesh from raw triangle data (tunnel collision meshes).
    ///
    /// `indices` must be a non-empty multiple of three vertex indexes into `vertices`.
    #[must_use]
    pub fn trimesh_from_triangles(vertices: &[[f32; 3]], indices: &[u32]) -> Option<Self> {
        if indices.is_empty() || !indices.len().is_multiple_of(3) || vertices.is_empty() {
            return None;
        }
        if indices.iter().any(|&i| i as usize >= vertices.len()) {
            return None;
        }
        Some(Self::TriMesh {
            vertices: vertices.iter().map(|p| Vec3::new(p[0], p[1], p[2])).collect(),
            indices: indices.as_chunks::<3>().0.to_vec(),
        })
    }
}

impl Default for ColliderShape {
    fn default() -> Self {
        Self::Box {
            half_extents: Vec3::new(0.5, 0.5, 0.5),
        }
    }
}

/// The long axis of a capsule/cylinder/cone.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShapeAxis {
    /// +X.
    X,
    /// +Y (the default).
    #[default]
    Y,
    /// +Z.
    Z,
}

impl ShapeAxis {
    /// Rotation that turns a +Y-aligned shape into this axis.
    ///
    /// Bit constants, not `Quat::from_rotation_*`: those call the platform `f32::sin_cos`, whose
    /// result Rust does not guarantee to be stable across platforms and toolchains, and the
    /// fluid solver hashes the poses these quaternions build (ADR-4753). `sin(pi/4)` and
    /// `cos(pi/4)` are one fixed bit pattern each.
    pub fn from_y_rotation(self) -> Quat {
        /// `sin(pi/4)` == `cos(pi/4)`, the only f32 bit pattern either value can take.
        const S: f32 = f32::from_bits(0x3f35_04f3);
        match self {
            Self::Y => Quat::IDENTITY,
            Self::X => Quat::from_xyzw(0.0, 0.0, -S, S),
            Self::Z => Quat::from_xyzw(S, 0.0, 0.0, S),
        }
    }
}

/// How friction/restitution of two colliders combine (rapier `CoefficientCombineRule`).
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CombineRule {
    /// (a + b) / 2.
    #[default]
    Average,
    /// min(a, b).
    Min,
    /// a × b.
    Multiply,
    /// max(a, b).
    Max,
}

impl CombineRule {
    /// Converts to rapier's enum.
    pub fn to_rapier(self) -> rapier3d::dynamics::CoefficientCombineRule {
        match self {
            Self::Average => rapier3d::dynamics::CoefficientCombineRule::Average,
            Self::Min => rapier3d::dynamics::CoefficientCombineRule::Min,
            Self::Multiply => rapier3d::dynamics::CoefficientCombineRule::Multiply,
            Self::Max => rapier3d::dynamics::CoefficientCombineRule::Max,
        }
    }
}

/// Which collision layer a collider belongs to (index into `config/physics.ron`'s layer
/// list; spec 15 § 3).
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, Component)]
#[reflect(Component, Default)]
pub struct CollisionLayer {
    /// Index of the layer in `PhysicsLayers`.
    pub index: u16,
}

impl CollisionLayer {
    /// Layer number `index`.
    pub fn new(index: u16) -> Self {
        Self { index }
    }
}

/// Which events a collider raises (spec 15 § 3).
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, Component)]
#[reflect(Component, Default)]
pub struct ActiveEvents {
    /// Raise `CollisionStarted`/`CollisionEnded` (and trigger events for sensors).
    pub collision: bool,
    /// Raise `ContactForce` while a contact pair's total force exceeds the threshold.
    pub contact_force: bool,
}

impl ActiveEvents {
    /// Both event kinds.
    pub const ALL: Self = Self {
        collision: true,
        contact_force: true,
    };
}

/// A collider (spec 15 § 2): a shape at an offset from its body, with a physical material,
/// a collision layer and event flags. On a child entity without a [`RigidBody`] it attaches
/// to the nearest ancestor body (compound, spec 15 § 2).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct Collider {
    /// The shape.
    pub shape: ColliderShape,
    /// Pose of the shape relative to its body (spec 15 § 2 `offset`).
    pub offset: Transform,
    /// Sensor colliders overlap but never push (trigger volumes).
    pub sensor: bool,
    /// Friction coefficient (0 = ice, 1 = rough).
    #[reflect(@Range(0.0..=2.0))]
    pub friction: f32,
    /// Restitution (bounciness, 0 = dead stop, 1 = perfect bounce).
    #[reflect(@Range(0.0..=1.0))]
    pub restitution: f32,
    /// Density (kg/m³) used for [`Mass::Auto`] mass computation. Zero = massless collider.
    #[reflect(@Range(0.0..=100.0))]
    pub density: f32,
    /// How friction combines with the other collider.
    pub friction_combine: CombineRule,
    /// How restitution combines with the other collider.
    pub restitution_combine: CombineRule,
    /// Which named layer this collider lives on.
    pub layer: CollisionLayer,
    /// Which events to raise.
    pub active_events: ActiveEvents,
    /// Total contact force above which `ContactForce` is raised (newtons).
    #[reflect(@regia_ecs::attrs::EditCondition("active_events.contact_force"))]
    pub contact_force_threshold: f32,
}

impl Default for Collider {
    fn default() -> Self {
        Self {
            shape: ColliderShape::default(),
            offset: Transform::IDENTITY,
            sensor: false,
            friction: 0.5,
            restitution: 0.0,
            density: 1.0,
            friction_combine: CombineRule::default(),
            restitution_combine: CombineRule::default(),
            layer: CollisionLayer::default(),
            active_events: ActiveEvents::default(),
            contact_force_threshold: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Joints (spec 15 § 2)
// ---------------------------------------------------------------------------

/// A joint's motor: drives the joint axis towards a position/velocity like a spring-damper.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct JointMotorConfig {
    /// Target angle (rad, revolute/spherical) or offset (m, prismatic).
    pub target_position: f32,
    /// Target velocity (rad/s or m/s).
    pub target_velocity: f32,
    /// Spring stiffness.
    pub stiffness: f32,
    /// Spring damping.
    pub damping: f32,
    /// Maximum motor force (0 = unlimited).
    pub max_force: f32,
}

/// The kind of joint connecting two bodies (spec 15 § 2 `Joint`).
#[derive(Reflect, Debug, Clone, Default, PartialEq)]
pub enum JointKind {
    /// Welds the two bodies together.
    #[default]
    Fixed,
    /// Hinge around `axis` with optional angle limits (rad) and motor.
    Revolute {
        /// Hinge axis in the first body's local space.
        axis: Vec3,
        /// Min/max angle around the axis (rad).
        limits: Option<(f32, f32)>,
        /// Motor driving the axis.
        motor: Option<JointMotorConfig>,
    },
    /// Slider along `axis` with optional distance limits (m) and motor.
    Prismatic {
        /// Slide axis in the first body's local space.
        axis: Vec3,
        /// Min/max offset along the axis (m).
        limits: Option<(f32, f32)>,
        /// Motor driving the axis.
        motor: Option<JointMotorConfig>,
    },
    /// Ball joint with an optional cone limit around Y (rad).
    Spherical {
        /// Half-angle of the allowed cone around the first body's Y axis (rad): the swing
        /// away from Y is bounded, the twist about Y stays free. The cone is applied as
        /// per-axis swing limits about X and Z (exact along those axes, up to about √2 ×
        /// the half-angle on the diagonals).
        cone_limit: Option<f32>,
    },
    /// Keeps the anchors' distance within `[min, max]` metres. `min == max` for a rigid
    /// rope of exact length; `min == 0` behaves like a rope.
    Distance {
        /// Minimum distance between the anchors.
        min: f32,
        /// Maximum distance between the anchors.
        max: f32,
    },
    /// Ball joint with per-axis angle ranges in a joint frame (ragdoll limbs, spec 93 § 2.1).
    ///
    /// The angles are the rotation of this entity's body relative to the connected body,
    /// measured from `rest` and positive by the right-hand rule about each axis. The swing
    /// limit is the bounding box of `bend` and `side` (exact along each axis, wider on the
    /// diagonals).
    SwingTwist {
        /// Twist axis in this body's local space (a limb's bone direction).
        twist_axis: Vec3,
        /// Bend axis in this body's local space; made perpendicular to `twist_axis`.
        bend_axis: Vec3,
        /// Min/max rotation about the bend axis (rad).
        bend: (f32, f32),
        /// Min/max rotation about `twist_axis × bend_axis` (rad).
        side: (f32, f32),
        /// Min/max rotation about the twist axis (rad).
        twist: (f32, f32),
        /// The rotation of this body relative to the connected body (`connected⁻¹ · this`)
        /// at which every angle is zero; `None` takes the relative rotation the bodies have
        /// when the joint is created.
        rest: Option<Quat>,
    },
}

/// Connects this entity's body to `connected`'s body with a joint (spec 15 § 2).
///
/// Anchors are in each body's local space. When the linear constraint force on the joint
/// (angular reactions are not counted) exceeds `break_force` (and it is non-zero), the joint is removed and a
/// [`JointBroken`](crate::events::JointBroken) message is raised (spec 15 § 2 `break_force`).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct Joint {
    /// What the joint locks/allows.
    pub kind: JointKind,
    /// The other body.
    pub connected: Entity,
    /// Anchor on this entity's body (local space).
    pub anchor: Vec3,
    /// Anchor on the connected body (local space).
    pub connected_anchor: Vec3,
    /// Linear constraint force (N, angular reactions not counted) above which the joint breaks; 0 = unbreakable.
    pub break_force: f32,
}

impl Default for Joint {
    fn default() -> Self {
        Self {
            kind: JointKind::default(),
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        }
    }
}

/// Next to a [`Joint`]: the joint also breaks when the estimated constraint torque (N·m)
/// exceeds this on a step; 0 or absent = no torque limit. The estimate adds the locked angular
/// axes and the angular limit reactions to the moment of the linear reaction about the joint
/// point (`(center of mass − anchor) × F` on the connected body). A load hanging to the side of
/// the anchor counts even when the angular rows are quiet. `Joint::break_force` (linear) keeps
/// working beside it, and either one breaks the joint. Like `break_force`, a broken joint stays
/// broken until its [`Joint`] changes.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct JointBreakTorque(pub f32);

/// Marker next to a [`Joint`]: the two jointed bodies do not collide with each other
/// (ragdoll limbs, chains of overlapping links). Read when the joint is created.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct JointNoContacts;

// ---------------------------------------------------------------------------
// Character body (spec 15 § 2/§ 4)
// ---------------------------------------------------------------------------

/// Ground/contact state of a character, written by the character-controller move.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct CharacterGround {
    /// Whether the character rests on ground this step.
    pub grounded: bool,
    /// World-space normal of the ground surface (up-ish when grounded).
    pub ground_normal: Vec3,
    /// The entity whose collider is the ground (placeholder if none).
    pub ground_entity: Entity,
    /// World-space velocity of the ground body (m/s). Informational: the move's translation
    /// already includes the platform's motion, so do not add `ground_velocity * dt` again.
    pub ground_velocity: Vec3,
    /// Yaw rate of the ground body (platform rotation, rad/s).
    pub ground_angular_velocity: f32,
    /// Whether the character's head touched a ceiling this step.
    pub ceiling_hit: bool,
    /// Whether the character is sliding down a slope too steep to stand on.
    pub sliding: bool,
}

impl Default for CharacterGround {
    fn default() -> Self {
        Self {
            grounded: false,
            ground_normal: Vec3::Y,
            ground_entity: Entity::PLACEHOLDER,
            ground_velocity: Vec3::ZERO,
            ground_angular_velocity: 0.0,
            ceiling_hit: false,
            sliding: false,
        }
    }
}

/// Marks a character entity and configures the kinematic character controller
/// (spec 15 § 4). The movement state machine lives in `regia_framework::Character`;
/// this component carries the physics-side settings and the resulting ground state.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct CharacterBody {
    /// Skin width: the small gap kept between the capsule and the world (spec 15 § 4 `offset`).
    pub offset: f32,
    /// Minimum free width required to autostep onto a stair (metres).
    pub autostep_min_width: f32,
    /// Whether autostepping may climb dynamic bodies (spec 15 § 4).
    pub autostep_include_dynamic: bool,
    /// Slope angle at which a walkable slope starts sliding (rad; spec 15 § 4).
    pub min_slope_slide_angle: f32,
    /// Whether to slide along walls (spec 15 § 4 `slide`).
    pub slide: bool,
    /// Mass used when pushing dynamic bodies (kg; spec 15 § 4).
    pub mass: f32,
    /// Latest ground state (written by the controller move; read by gameplay).
    pub ground: CharacterGround,
}

impl Default for CharacterBody {
    fn default() -> Self {
        Self {
            offset: 0.02,
            autostep_min_width: 0.15,
            autostep_include_dynamic: false,
            min_slope_slide_angle: std::f32::consts::FRAC_PI_4,
            slide: true,
            mass: 70.0,
            ground: CharacterGround::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Debris tag (M14, unchanged)
// ---------------------------------------------------------------------------

/// Marks an entity as cosmetic debris — driven by rapier, never feeds back to the sim.
///
/// Debris entities are managed by the [`crate::DebrisSpawner`] budget system: oldest
/// debris is despawned (optionally with a fade) when the budget is exceeded.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct DebrisTag {
    /// Physics-clock time ([`crate::PhysicsWorld::fixed_elapsed_secs`]) the chunk's lifetime
    /// counts from: the end of the first fixed step that simulates it. `None` until that step or
    /// the `PostUpdate` init-debris-spawn-times system records it.
    pub spawn_time_secs: Option<f64>,
    /// Lifetime in simulated (fixed-step) seconds after which the chunk automatically despawns.
    pub lifetime_secs: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_kind_default_is_dynamic() {
        assert_eq!(BodyKind::default(), BodyKind::Dynamic);
    }

    #[test]
    fn collider_shape_box() {
        let s = ColliderShape::Box {
            half_extents: Vec3::new(1.0, 2.0, 3.0),
        };
        match &s {
            ColliderShape::Box { half_extents } => {
                assert!((*half_extents - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-6);
            }
            _ => panic!("expected Box"),
        }
    }

    #[test]
    fn debris_tag_defaults() {
        let tag = DebrisTag::default();
        assert_eq!(tag.spawn_time_secs, None);
        assert_eq!(tag.lifetime_secs, 0.0);
    }

    #[test]
    fn handle_round_trip() {
        let h = rapier3d::dynamics::RigidBodyHandle::from_raw_parts(42, 7);
        let marker = BodyMarker::from_rapier(h);
        let back = marker.to_handle();
        assert_eq!(back, h, "index and generation survive the round trip");
    }

    #[test]
    fn from_raw_keeps_the_m14_generation_zero_shape() {
        let marker = BodyMarker::from_raw(42);
        assert_eq!(marker.generation, 0);
        assert_eq!(marker.to_handle().into_raw_parts().0, 42);
    }

    #[test]
    fn rigid_body_helpers() {
        assert_eq!(RigidBody::dynamic().kind, BodyKind::Dynamic);
        assert_eq!(RigidBody::kinematic().kind, BodyKind::Kinematic);
        assert_eq!(RigidBody::static_body().kind, BodyKind::Static);
        assert_eq!(RigidBody::default().gravity_scale, 1.0);
        assert!(RigidBody::default().can_sleep);
    }

    #[test]
    fn locked_axes_convert() {
        let locked = LockedAxes {
            translation_y: true,
            rotation_x: true,
            ..LockedAxes::FREE
        };
        let rapier = locked.to_rapier();
        use rapier3d::dynamics::LockedAxes as L;
        assert!(rapier.contains(L::TRANSLATION_LOCKED_Y));
        assert!(rapier.contains(L::ROTATION_LOCKED_X));
        assert!(!rapier.contains(L::TRANSLATION_LOCKED_X));
        assert!(!rapier.contains(L::ROTATION_LOCKED_Z));
        assert!(LockedAxes::FREE.to_rapier().is_empty());
    }

    #[test]
    fn mass_default_is_auto() {
        assert_eq!(Mass::default(), Mass::Auto);
    }

    #[test]
    fn shape_axis_rotations_move_y_into_place() {
        // Rotating +Y by the axis rotation must land on the target axis.
        for (axis, target) in [
            (ShapeAxis::X, Vec3::X),
            (ShapeAxis::Y, Vec3::Y),
            (ShapeAxis::Z, Vec3::Z),
        ] {
            let rotated = axis.from_y_rotation() * Vec3::Y;
            assert!(
                (rotated - target).length() < 1e-5,
                "{axis:?}: {rotated} != {target}"
            );
        }
    }

    #[test]
    fn shape_axis_rotations_are_bit_constants() {
        // ADR-4753: the X and Z turns must be fixed bit patterns (no platform sin_cos), the
        // same on every target the engine ships. 0x3f3504f3 is sin(pi/4) == cos(pi/4) in f32.
        const S: u32 = 0x3f35_04f3;
        assert_eq!(ShapeAxis::Y.from_y_rotation(), Quat::IDENTITY);
        assert_eq!(
            ShapeAxis::X.from_y_rotation().to_array().map(f32::to_bits),
            [0, 0, 0xbf35_04f3, S]
        );
        assert_eq!(
            ShapeAxis::Z.from_y_rotation().to_array().map(f32::to_bits),
            [S, 0, 0, S]
        );
    }

    #[test]
    fn collider_defaults_are_physical() {
        let c = Collider::default();
        assert!(!c.sensor);
        assert!((c.friction - 0.5).abs() < 1e-6);
        assert_eq!(c.restitution, 0.0);
        assert!((c.density - 1.0).abs() < 1e-6);
        assert_eq!(c.layer, CollisionLayer::default());
        assert_eq!(c.active_events, ActiveEvents::default());
    }

    #[test]
    fn joint_defaults() {
        let j = Joint::default();
        assert_eq!(j.kind, JointKind::Fixed);
        assert_eq!(j.connected, Entity::PLACEHOLDER);
        assert_eq!(j.break_force, 0.0);
    }

    #[test]
    fn character_body_defaults() {
        let cb = CharacterBody::default();
        assert!((cb.offset - 0.02).abs() < 1e-6);
        assert!((cb.mass - 70.0).abs() < 1e-6);
        assert!(!cb.ground.grounded);
    }

    #[test]
    fn body_marker_alias_matches_new_handle() {
        // M14 code stores BodyMarker; it must be the same type as RigidBodyHandle.
        let marker: BodyMarker = BodyMarker::from_raw(7);
        let handle: RigidBodyHandle = marker;
        assert_eq!(handle.raw, 7);
    }
}
