// SPDX-License-Identifier: MPL-2.0
//! Lockstep hover hulls in an engine-native app (plan row mp-hover-ecs-agent): a component, a
//! `FixedUpdate` system and a floor sampler over the level's terrain, around the pure hover step of
//! [`regia_detsim::hover`].
//!
//! Spawn a hull with a [`DetHoverHull`] (its tuning), a [`DetHoverId`] (its sim id: the order hulls
//! step in) and, optionally, a [`Transform`]. [`DetHoverPlugin`] steps every hull once per
//! `FixedUpdate`, in ascending [`DetHoverId`] order (never archetype or spawn order), with
//! [`tick_hull`]: the ground pass ([`ground_attitude`]), the floor sample, the hop and jump commands
//! of [`DetHoverInput`], [`step_with`] and [`integrate`], in that order. That is the order of
//! [`regia_detsim::hover::host::HoverFleet::step_all`]; a world of hulls with no hop or jump steps
//! bit for bit like a fleet of the same hulls (`det_hover_system_matches_the_fleet`).
//!
//! | Schedule | Set | Work |
//! |---|---|---|
//! | `FixedUpdate` | [`DetHoverSet::Step`] | [`step_det_hover_hulls`]: every hull one tick, by sim id |
//! | `FixedUpdate` | [`DetHoverSet::Present`] | [`present_det_hover_hulls`]: [`Transform`] and [`DetHoverLean`] from the state |
//! | `PostUpdate` (client) | | the `det_hover` gizmo group (off by default; its system does not run while off) |
//!
//! The sim state is [`DetHoverState`] only: the system never reads [`Transform`] or
//! [`DetHoverLean`] back, so the visual lean ([`compute_lean`]) is presentation and never feeds the
//! sim. Render interpolation between fixed ticks is the engine's (`PreviousGlobalTransform`).
//!
//! # Determinism
//!
//! The step's time step is [`DetHoverConfig::dt`] (default [`tick_dt`], 1/60 s), never the frame
//! delta, and the system runs once per `FixedUpdate`, so the sim states per fixed tick do not depend
//! on the render frame rate (`det_hover_ignores_frame_rate`). [`tick_hull`] is a pure function of
//! (hull, state, input, floor, world, dt): a client predictor that rolls back to a saved
//! [`DetHoverState`] and replays the same inputs reaches the same bits
//! (`det_hover_rollback_replays_bit_for_bit`).
//!
//! This module is **not** on the lockstep closure (`docs/ci/lockstep-closure.toml`): it runs inside
//! the ECS, and its presentation half uses glam's quaternion trig. A lockstep game steps
//! [`regia_detsim::hover`] (or its `HoverFleet`) directly in its sim; this system is for
//! engine-native games that run the hover on one authority (server-authoritative or single
//! player) or predict it on a client. The sim half ([`tick_hull`]) calls only `regia_detsim` with
//! lockstep arithmetic, and the floor is det-clean when the sampler is ([`DetHoverFloor`]).
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_framework::det_hover::{DetHoverHull, DetHoverId, DetHoverInput, DetHoverPlugin, DetHoverState};
//!
//! let mut app = App::new();
//! app.add_plugins(DetHoverPlugin);
//! enable_fixed_step(&mut app, 60.0);
//! let hull = app
//!     .world_mut()
//!     .spawn((
//!         DetHoverHull::DEFAULT,
//!         DetHoverId(1),
//!         DetHoverState::at_rest([0.0, 0.0, 0.0], 0.0, &DetHoverHull::DEFAULT.params),
//!         DetHoverInput { throttle: 1.0, ..DetHoverInput::default() },
//!     ))
//!     .id();
//! for _ in 0..60 {
//!     app.update();
//! }
//! let state = app.world().get::<DetHoverState>(hull).map(|s| s.0);
//! assert!(state.is_some_and(|b| b.pos[2] > 2.0), "a second of full throttle moved the hull forward");
//! ```

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_detsim::hover::host::{FleetHull, FloorSampler, HoverBody, ground_attitude, integrate};
use regia_detsim::hover::{
    ClimbTuning, FlightMode, HoverLimits, HoverParams, HoverParamsError, HoverParamsErrorKind, HoverState,
    WorldPhysics, apply_hull_hop_with, apply_jump, compute_lean, hop_launch_speed_for, step_with,
};
use regia_detsim::tick::tick_dt;
use regia_scene::Transform;

/// The largest valid half length or half width of a hull's footprint, metres.
pub const FOOTPRINT_MAX_M: f32 = 100.0;

/// One hover hull's tuning: its parameters, the model limits, the flight rules and the footprint
/// the ground pass samples.
///
/// Every field is a sim setting: two runs (or two peers) step the same bits only with the same
/// values. A hull read from user content is checked with [`DetHoverHull::validate`].
#[derive(Component, Debug, Clone, Copy, PartialEq)]
#[require(DetHoverState, DetHoverInput, DetHoverLean)]
pub struct DetHoverHull {
    /// The hull's parameters ([`HoverParams::DEFAULT_HULL`] for the engine's generic hull).
    pub params: HoverParams,
    /// The model limits ([`HoverLimits::DEFAULT`] for the engine's model).
    pub limits: HoverLimits,
    /// The flight rules over crests and ramps ([`FlightMode::climb`]).
    pub flight: FlightMode,
    /// Half the hull's length, metres: the ground pass samples the floor this far ahead and behind.
    /// Valid: 0 to [`FOOTPRINT_MAX_M`] (0 reads no pitch from the ground).
    pub half_length: f32,
    /// Half the hull's width, metres: the ground pass samples the floor this far to each side.
    /// Valid: 0 to [`FOOTPRINT_MAX_M`] (0 reads no roll from the ground).
    pub half_width: f32,
}

impl DetHoverHull {
    /// The engine's generic hull ([`HoverParams::DEFAULT_HULL`], [`HoverLimits::DEFAULT`], air-time
    /// flight) on a 4 m x 2 m footprint (the `HoverFleet` default).
    pub const DEFAULT: DetHoverHull = DetHoverHull {
        params: HoverParams::DEFAULT_HULL,
        limits: HoverLimits::DEFAULT,
        flight: FlightMode::AirTime,
        half_length: 2.0,
        half_width: 1.0,
    };

    /// A hull with these numbers on the default 4 m x 2 m footprint.
    pub const fn new(params: HoverParams, limits: HoverLimits, flight: FlightMode) -> Self {
        DetHoverHull {
            params,
            limits,
            flight,
            half_length: 2.0,
            half_width: 1.0,
        }
    }

    /// This hull on another footprint (half length and half width, metres).
    pub const fn with_footprint(mut self, half_length: f32, half_width: f32) -> Self {
        self.half_length = half_length;
        self.half_width = half_width;
        self
    }

    /// The climb tuning the hull steps with (its flight mode's).
    pub const fn climb(&self) -> &'static ClimbTuning {
        self.flight.climb()
    }

    /// The same hull as a `HoverFleet` hull type (the fleet takes the limits and climb per match).
    pub const fn fleet_hull(&self) -> FleetHull {
        FleetHull::new(self.params, self.half_length, self.half_width)
    }

    /// `Ok` when the parameters ([`HoverParams::validate`]), the limits ([`HoverLimits::validate`])
    /// and the footprint are valid; otherwise the first bad field, with its protocol code
    /// ([`HoverParamsErrorKind::code`]). The footprint fields are named `half_length` and
    /// `half_width`. Call it when a hull is loaded, not per tick.
    pub fn validate(&self) -> Result<(), HoverParamsError> {
        self.params.validate()?;
        self.limits.validate()?;
        check_footprint("half_length", self.half_length)?;
        check_footprint("half_width", self.half_width)
    }
}

impl Default for DetHoverHull {
    fn default() -> Self {
        DetHoverHull::DEFAULT
    }
}

fn check_footprint(field: &'static str, value: f32) -> Result<(), HoverParamsError> {
    let kind = if !value.is_finite() {
        Some(HoverParamsErrorKind::NotFinite)
    } else if value < 0.0 {
        Some(HoverParamsErrorKind::Negative)
    } else if value > FOOTPRINT_MAX_M {
        Some(HoverParamsErrorKind::OutOfRange)
    } else {
        None
    };
    match kind {
        Some(kind) => Err(HoverParamsError {
            field,
            kind,
            value,
            range: [0.0, FOOTPRINT_MAX_M],
        }),
        None => Ok(()),
    }
}

/// A hull's sim state: its world position and its [`HoverState`]. The only state the system reads
/// and writes; save it to roll a hull back, restore it to replay.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct DetHoverState(pub HoverBody);

impl DetHoverState {
    /// A hull resting on its cushion at `pos` (pass the floor height in `pos[1]`), facing `yaw`
    /// ([`HoverBody::at_rest`]).
    pub fn at_rest(pos: [f32; 3], yaw: f32, params: &HoverParams) -> Self {
        DetHoverState(HoverBody::at_rest(pos, yaw, params))
    }
}

impl Default for DetHoverState {
    fn default() -> Self {
        DetHoverState(HoverBody::new([0.0; 3]))
    }
}

/// A hull's input for the next fixed tick. A game writes it before [`DetHoverSet::Step`] (a
/// player's controls, an AI driver, a replayed input frame); the system reads it and leaves it.
///
/// The axes are clamped to `[-1, 1]` by the step, and a NaN or infinite axis reads as 0. `hop` and
/// `jump` are read every tick they are set: a held hop fires again each time its cooldown
/// ([`HoverLimits::hop_cooldown_s`]) runs out, a held jump again on landing. Clear them after one
/// tick for one press.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct DetHoverInput {
    /// Forward thrust, -1 (brake, then reverse) to 1.
    pub throttle: f32,
    /// Sideways thrust, -1 (left) to 1 (right).
    pub strafe: f32,
    /// Steering, -1 to 1.
    pub steer: f32,
    /// A fan-pulse hop ([`apply_hull_hop_with`]).
    pub hop: bool,
    /// A jump impulse of [`HoverParams::jump_speed`] ([`apply_jump`]).
    pub jump: bool,
}

impl DetHoverInput {
    /// `[throttle, strafe, steer]`, as the step reads them.
    pub const fn axes(&self) -> [f32; 3] {
        [self.throttle, self.strafe, self.steer]
    }

    /// Input of these axes with no hop or jump.
    pub const fn from_axes(axes: [f32; 3]) -> Self {
        DetHoverInput {
            throttle: axes[0],
            strafe: axes[1],
            steer: axes[2],
            hop: false,
            jump: false,
        }
    }
}

/// A hull's sim id: hulls step in ascending id order. Give every hull its own id (the game's
/// entity id, a slot, a det id from `regia_detsim::ids`). A hull without one is not stepped
/// ([`DetHoverStats::unordered`]); hulls that share an id step in entity order, which is not
/// repeatable across runs ([`DetHoverStats::duplicate_ids`]).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DetHoverId(pub u64);

/// Marks a hull another system steps: [`step_det_hover_hulls`] and [`present_det_hover_hulls`]
/// leave it alone (no step, no `Transform`, not counted in [`DetHoverStats`]). The networked hull of
/// `net_hover` (feature `net`, ADR-8819) requires it: `regia_net`'s predicted schedule steps it, and a
/// proxy's `Transform` is the interpolated one.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetHoverSteppedExternally;

/// The visual lean of a hull, radians ([`compute_lean`]): pitch nose up, roll right side down.
/// Presentation only: written after each step, never read by the sim.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct DetHoverLean {
    /// Lean pitch, radians, nose up positive.
    pub pitch: f32,
    /// Lean roll, radians, right side down positive.
    pub roll: f32,
}

/// The floor every hull flies over: a [`FloorSampler`] shared by the system, the debug draw and the
/// agent.
///
/// The bits a hull steps with are only as repeatable as the sampler: a `HeightGrid`, a `FlatFloor`
/// or a `FloorFn` over `regia_detsim::math` are lockstep code; [`DetHoverFloor::from_terrain`]
/// (client builds) reads the terrain's own bilinear sampler, which is plain IEEE arithmetic over the
/// heightfield's `f32` mirror (repeatable on one build and across x86-64 and aarch64 for the
/// cooked `u16` grid or an `f32` grid, but not on the det-scan closure).
#[derive(Resource, Clone)]
pub struct DetHoverFloor(pub Arc<dyn FloorSampler + Send + Sync>);

impl DetHoverFloor {
    /// A floor from any sampler.
    pub fn new(sampler: impl FloorSampler + Send + Sync + 'static) -> Self {
        DetHoverFloor(Arc::new(sampler))
    }

    /// A flat floor at `height` metres.
    pub fn flat(height: f32) -> Self {
        DetHoverFloor::new(regia_detsim::hover::host::FlatFloor(height))
    }

    /// The floor height at world `(x, z)`.
    pub fn floor(&self, x: f32, z: f32) -> f32 {
        self.0.floor(x, z)
    }

    /// The floor of a terrain heightfield: [`TerrainFloor`] over a copy of `map`.
    #[cfg(feature = "client")]
    pub fn from_terrain(map: &regia_terrain::TerrainMap) -> Self {
        DetHoverFloor::new(TerrainFloor(map.clone()))
    }
}

impl Default for DetHoverFloor {
    fn default() -> Self {
        DetHoverFloor::flat(0.0)
    }
}

impl std::fmt::Debug for DetHoverFloor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DetHoverFloor(..)")
    }
}

/// A terrain heightfield as a hover floor: `TerrainMap::sample_bilinear` (border-clamped bilinear
/// over the heightfield's metre mirror, the surface the simulation collides with, spec 26 § 1).
///
/// It is the one terrain adapter. It is repeatable when the heightfield is the cooked `u16` grid or
/// an `f32` grid (the mirror holds exact values and the blend is `+ - *` in a fixed order); it is
/// not checked by det-scan, so a lockstep game copies the heights into a `HeightGrid` instead.
#[cfg(feature = "client")]
#[derive(Debug, Clone)]
pub struct TerrainFloor(pub regia_terrain::TerrainMap);

#[cfg(feature = "client")]
impl FloorSampler for TerrainFloor {
    fn floor(&self, x: f32, z: f32) -> f32 {
        self.0.sample_bilinear(x, z)
    }
}

/// The settings every hull steps with.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct DetHoverConfig {
    /// The world (gravity, air, vacuum): [`WorldPhysics::EARTH`] by default.
    pub world: WorldPhysics,
    /// The fixed time step, seconds ([`tick_dt`], 1/60 s, by default). Set it to the app's fixed
    /// rate (`FixedTime::hz`); it is never read from the frame clock.
    pub dt: f32,
    /// Replace [`DetHoverFloor`] with the level's terrain when a `Terrain` is added or changed
    /// (client builds). Off by default: a game that sets its own floor keeps it.
    pub follow_terrain: bool,
}

impl Default for DetHoverConfig {
    fn default() -> Self {
        DetHoverConfig {
            world: WorldPhysics::EARTH,
            dt: tick_dt(),
            follow_terrain: false,
        }
    }
}

/// Counters of the hover system, for tests, the agent and a HUD.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetHoverStats {
    /// Fixed ticks the system ran.
    pub ticks: u64,
    /// Hulls stepped on the last tick.
    pub hulls: u32,
    /// Hulls reset by the step's bad-state guard, in total.
    pub recovered: u64,
    /// Hops that fired, in total.
    pub hops: u64,
    /// Jumps that fired, in total.
    pub jumps: u64,
    /// Hulls on the last tick that share their [`DetHoverId`] with another hull.
    pub duplicate_ids: u32,
    /// Hulls on the last tick without a [`DetHoverId`] (not stepped).
    pub unordered: u32,
    /// Ticks not stepped because [`DetHoverConfig::dt`] was not finite and above 0.
    pub skipped: u64,
}

/// What one [`tick_hull`] did besides stepping.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DetHoverTick {
    /// The tick ran (`dt` was finite and above 0).
    pub stepped: bool,
    /// The step's bad-state guard reset the hull ([`regia_detsim::hover::StepReport::recovered`]).
    pub recovered: bool,
    /// A hop fired.
    pub hopped: bool,
    /// A jump fired.
    pub jumped: bool,
    /// The floor height the step used (a non-finite sample reads as the previous floor).
    pub floor: f32,
}

/// One tick of one hull: the pure function the system, the agent and a client predictor run.
///
/// In order: the ground pass ([`ground_attitude`] on the hull's footprint, into
/// `ground_pitch`/`ground_roll`), the floor sample at the hull, the jump ([`apply_jump`]) and the
/// hop ([`apply_hull_hop_with`] at `hop_speed`) when the input asks, [`step_with`] with the hull's
/// parameters, limits and flight rules, then [`integrate`] with the floor the step used. A `dt` that
/// is not finite and above 0 changes nothing.
///
/// `hop_speed` is [`hop_launch_speed_for`] of the hull's limits, the world and `dt` (a bounded tool:
/// compute it once per hull type and world, [`hop_speed`]); it is read only when `input.hop` is set.
pub fn tick_hull<S: FloorSampler + ?Sized>(
    hull: &DetHoverHull,
    body: &mut HoverBody,
    input: &DetHoverInput,
    floor: &S,
    world: &WorldPhysics,
    hop_speed: f32,
    dt: f32,
) -> DetHoverTick {
    if !(dt.is_finite() && dt > 0.0) {
        return DetHoverTick::default();
    }
    let (pitch, roll) = ground_attitude(floor, body.pos, body.state.yaw, hull.half_length, hull.half_width);
    body.state.ground_pitch = pitch;
    body.state.ground_roll = roll;
    let height = floor.floor(body.pos[0], body.pos[2]);
    let mut jumped = false;
    if input.jump && !body.state.airborne {
        apply_jump(&mut body.state, &hull.params);
        jumped = true;
    }
    let hopped = input.hop && apply_hull_hop_with(&hull.limits, &mut body.state, hop_speed);
    let report = step_with(
        &hull.params,
        &hull.limits,
        hull.climb(),
        world,
        &mut body.state,
        &input.axes(),
        height,
        dt,
    );
    // The floor the step used: a non-finite sample read as the previous floor.
    let used = if height.is_finite() {
        height
    } else {
        body.state.prev_terrain.unwrap_or(0.0)
    };
    integrate(body, dt, used);
    DetHoverTick {
        stepped: true,
        recovered: report.recovered,
        hopped,
        jumped,
        floor: used,
    }
}

/// The hop launch speed of `hull` on `world` at `dt` ([`hop_launch_speed_for`]): a bounded tool (24
/// bisection steps over a whole ascent), so the system caches it per hull limits, world and `dt`.
pub fn hop_speed(hull: &DetHoverHull, world: &WorldPhysics, dt: f32) -> f32 {
    hop_launch_speed_for(&hull.limits, world, dt)
}

/// The height above the floor the hull's hover spring aims at after a step: the lookahead
/// (`ride_height + hover_lead * ground_climb`, clamped as the step clamps it). For the debug draw
/// and the agent; the step computes its own.
pub fn lookahead_aim(hull: &DetHoverHull, state: &HoverState) -> f32 {
    use regia_detsim::float::{det_clamp, det_min};
    let p = &hull.params;
    let climb = hull.climb();
    let flight = if p.ride_height > 0.0 {
        climb.flight
    } else {
        regia_detsim::hover::FlightRules::CLAMPED_1014
    };
    let ceiling = p.ride_height + flight.ride_max_m;
    det_clamp(
        p.ride_height + p.hover_lead * state.ground_climb,
        p.ride_height * 0.4,
        det_min(p.ride_height + climb.lead_max_rise, ceiling),
    )
}

/// The four floor points the ground pass samples for a hull at `pos` facing `yaw`: nose, tail,
/// right, left (X and Z; Y is the floor there). For the debug draw.
pub fn attitude_samples<S: FloorSampler + ?Sized>(
    hull: &DetHoverHull,
    pos: [f32; 3],
    yaw: f32,
    floor: &S,
) -> [[f32; 3]; 4] {
    let (s, c) = (regia_detsim::math::sin(yaw), regia_detsim::math::cos(yaw));
    let (fx, fz) = (s * hull.half_length, c * hull.half_length);
    let (rx, rz) = (c * hull.half_width, -s * hull.half_width);
    let at = |x: f32, z: f32| [x, floor.floor(x, z), z];
    [
        at(pos[0] + fx, pos[2] + fz),
        at(pos[0] - fx, pos[2] - fz),
        at(pos[0] + rx, pos[2] + rz),
        at(pos[0] - rx, pos[2] - rz),
    ]
}

/// The visual lean of a hull ([`compute_lean`] of its parameters, state and input): presentation.
pub fn compute_lean_of(hull: &DetHoverHull, state: &HoverState, input: &DetHoverInput) -> DetHoverLean {
    let (pitch, roll) = compute_lean(&hull.params, state, &input.axes());
    DetHoverLean { pitch, roll }
}

/// The hull's rotation for presentation: yaw about +Y, then pitch (the cushion's plus the lean,
/// nose up), then roll (the hull's plus the lean, right side down). Glam trig: presentation only.
pub fn hull_rotation(state: &HoverState, lean: &DetHoverLean) -> Quat {
    Quat::from_rotation_y(state.yaw)
        * Quat::from_rotation_x(-(state.cushion_pitch + lean.pitch))
        * Quat::from_rotation_z(-(state.roll + lean.roll))
}

/// The system sets of the hover in `FixedUpdate`, run in this order.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetHoverSet {
    /// [`step_det_hover_hulls`]. Write [`DetHoverInput`] before it.
    Step,
    /// [`present_det_hover_hulls`]: [`Transform`] and [`DetHoverLean`].
    Present,
}

/// Steps the hover hulls in `FixedUpdate` ([`DetHoverSet`]), with the `det_hover` gizmo group in
/// client builds.
#[derive(Default)]
pub struct DetHoverPlugin;

impl Plugin for DetHoverPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DetHoverConfig>()
            .init_resource::<DetHoverFloor>()
            .init_resource::<DetHoverStats>();
        app.configure_sets(FixedUpdate, (DetHoverSet::Step, DetHoverSet::Present).chain());
        app.add_systems(FixedUpdate, step_det_hover_hulls.in_set(DetHoverSet::Step));
        app.add_systems(FixedUpdate, present_det_hover_hulls.in_set(DetHoverSet::Present));
        #[cfg(feature = "client")]
        app.add_systems(FixedUpdate, follow_terrain_floor.before(DetHoverSet::Step));
    }

    #[cfg(feature = "client")]
    fn finish(&self, app: &mut App) {
        use regia_render::gizmos::{AppGizmoBuilder, gizmo_group_enabled};
        // The gizmo group hands its lines to the renderer: register it only when the renderer's gizmo
        // plugin is present, so headless apps (servers, tests) run without it.
        if !app.has_plugin(std::any::type_name::<regia_render::gizmos::GizmoPlugin>()) {
            return;
        }
        app.init_gizmo_group::<DetHoverGizmos>();
        app.add_systems(
            PostUpdate,
            draw_det_hover.run_if(gizmo_group_enabled::<DetHoverGizmos>),
        );
    }
}

/// Hop speeds already solved: `(limits, world bytes, dt bits, speed)`. Small: a game has a few hull
/// types and one world.
#[derive(Default)]
pub struct HopSpeedCache(Vec<(HoverLimits, [u8; WorldPhysics::BYTES], u32, f32)>);

impl HopSpeedCache {
    /// Most entries kept; a new one past this clears the cache.
    pub const CAPACITY: usize = 32;

    /// The hop speed of `hull` on `world` at `dt`, solved once per key.
    pub fn get(&mut self, hull: &DetHoverHull, world: &WorldPhysics, dt: f32) -> f32 {
        let bytes = world.to_le_bytes();
        let bits = dt.to_bits();
        if let Some(entry) = self
            .0
            .iter()
            .find(|e| e.0 == hull.limits && e.1 == bytes && e.2 == bits)
        {
            return entry.3;
        }
        let speed = hop_speed(hull, world, dt);
        if self.0.len() >= Self::CAPACITY {
            self.0.clear();
        }
        self.0.push((hull.limits, bytes, bits, speed));
        speed
    }
}

/// A hull this plugin steps: one not marked [`DetHoverSteppedExternally`].
type OwnHull = (With<DetHoverHull>, Without<DetHoverSteppedExternally>);

/// What [`present_det_hover_hulls`] writes per hull.
type PresentItem<'a> = (
    &'a DetHoverHull,
    &'a DetHoverState,
    &'a DetHoverInput,
    &'a mut DetHoverLean,
    Option<&'a mut Transform>,
);

/// `FixedUpdate`: every hull one tick ([`tick_hull`]), in ascending [`DetHoverId`] order.
#[expect(
    clippy::too_many_arguments,
    reason = "a system: one parameter per resource and query; a SystemParam struct would only rename them"
)]
pub fn step_det_hover_hulls(
    config: Res<DetHoverConfig>,
    floor: Res<DetHoverFloor>,
    mut stats: ResMut<DetHoverStats>,
    mut hops: Local<HopSpeedCache>,
    mut order: Local<Vec<(u64, u64, Entity)>>,
    ids: Query<(Entity, &DetHoverId), OwnHull>,
    unordered: Query<(), (OwnHull, Without<DetHoverId>)>,
    mut hulls: Query<(&DetHoverHull, &mut DetHoverState, &DetHoverInput)>,
) {
    let _span = tracing::trace_span!("det_hover_step").entered();
    stats.ticks += 1;
    stats.unordered = u32::try_from(unordered.iter().count()).unwrap_or(u32::MAX);
    let dt = config.dt;
    if !(dt.is_finite() && dt > 0.0) {
        stats.skipped += 1;
        stats.hulls = 0;
        return;
    }
    order.clear();
    order.extend(ids.iter().map(|(e, id)| (id.0, e.to_bits(), e)));
    order.sort_unstable();
    let mut duplicates = 0_u32;
    for (i, entry) in order.iter().enumerate() {
        let same_prev = i > 0 && order[i - 1].0 == entry.0;
        let same_next = order.get(i + 1).is_some_and(|n| n.0 == entry.0);
        if same_prev || same_next {
            duplicates += 1;
        }
    }
    stats.duplicate_ids = duplicates;
    let sampler: &(dyn FloorSampler + Send + Sync) = &*floor.0;
    let mut stepped = 0_u32;
    for &(_, _, entity) in order.iter() {
        let Ok((hull, mut state, input)) = hulls.get_mut(entity) else {
            continue;
        };
        let speed = if input.hop {
            hops.get(hull, &config.world, dt)
        } else {
            0.0
        };
        let tick = tick_hull(hull, &mut state.0, input, sampler, &config.world, speed, dt);
        stepped += 1;
        stats.recovered += u64::from(tick.recovered);
        stats.hops += u64::from(tick.hopped);
        stats.jumps += u64::from(tick.jumped);
    }
    stats.hulls = stepped;
}

/// `FixedUpdate`, after the step: each hull's [`DetHoverLean`] ([`compute_lean`]) and, when it has
/// one, its [`Transform`] (translation = position, rotation = [`hull_rotation`]). Never read back.
pub fn present_det_hover_hulls(mut hulls: Query<PresentItem<'_>, Without<DetHoverSteppedExternally>>) {
    let _span = tracing::trace_span!("det_hover_present").entered();
    for (hull, state, input, mut lean, transform) in &mut hulls {
        *lean = compute_lean_of(hull, &state.0.state, input);
        if let Some(mut transform) = transform {
            let [x, y, z] = state.0.pos;
            transform.translation = Vec3::new(x, y, z);
            transform.rotation = hull_rotation(&state.0.state, &lean);
        }
    }
}

/// Debug overlay of the hover hulls (`debug.set_gizmos`, group `det_hover`, off by default).
#[cfg(feature = "client")]
#[derive(Default, Debug, Clone, Copy)]
pub struct DetHoverGizmos;

#[cfg(feature = "client")]
impl regia_render::gizmos::GizmoConfigGroup for DetHoverGizmos {
    const NAME: &'static str = "det_hover";
    fn default_config() -> regia_render::gizmos::GizmoConfig {
        regia_render::gizmos::GizmoConfig {
            enabled: false,
            ..Default::default()
        }
    }
}

/// Colors of the `det_hover` overlay.
#[cfg(feature = "client")]
pub mod colors {
    use regia_core::math::Color;
    /// The ride-height line, floor to `floor + ride_height`, of a hull on its cushion.
    pub const ON_CUSHION: Color = Color::rgb(0.2, 1.0, 0.4);
    /// The same line of a hull in the air ([`regia_detsim::hover::HoverLimits::hull_in_air`]).
    pub const IN_AIR: Color = Color::rgb(1.0, 0.5, 0.1);
    /// The lookahead aim height ([`super::lookahead_aim`]).
    pub const AIM: Color = Color::rgb(0.2, 0.7, 1.0);
    /// The four ground-pass samples.
    pub const SAMPLE: Color = Color::rgb(1.0, 1.0, 0.2);
    /// The cushion pitch and roll arrows.
    pub const CUSHION: Color = Color::rgb(1.0, 0.3, 0.8);
}

/// `PostUpdate` while the `det_hover` group is on: per hull, the ride-height line (green on the
/// cushion, orange in the air), a cross at the lookahead aim height, the four ground-pass samples,
/// and two arrows from the hull showing the cushion pitch (along the nose) and the roll (along the
/// right side).
#[cfg(feature = "client")]
fn draw_det_hover(
    floor: Res<DetHoverFloor>,
    hulls: Query<(&DetHoverHull, &DetHoverState)>,
    mut gizmos: regia_render::gizmos::Gizmos<DetHoverGizmos>,
) {
    let _span = tracing::trace_span!("det_hover_debug_draw").entered();
    let sampler: &(dyn FloorSampler + Send + Sync) = &*floor.0;
    for (hull, state) in &hulls {
        let body = &state.0;
        let s = &body.state;
        let [x, _, z] = body.pos;
        let ground = sampler.floor(x, z);
        let ground = if ground.is_finite() { ground } else { 0.0 };
        let base = Vec3::new(x, ground, z);
        let in_air = hull.limits.hull_in_air(&hull.params, s);
        let color = if in_air {
            colors::IN_AIR
        } else {
            colors::ON_CUSHION
        };
        gizmos.line(base, base + Vec3::Y * hull.params.ride_height, color);
        let aim = base + Vec3::Y * lookahead_aim(hull, s);
        gizmos.line(aim - Vec3::X * 0.3, aim + Vec3::X * 0.3, colors::AIM);
        gizmos.line(aim - Vec3::Z * 0.3, aim + Vec3::Z * 0.3, colors::AIM);
        for [px, py, pz] in attitude_samples(hull, body.pos, s.yaw, sampler) {
            let py = if py.is_finite() { py } else { ground };
            let p = Vec3::new(px, py, pz);
            gizmos.line(p, p + Vec3::Y * 0.4, colors::SAMPLE);
        }
        let at = Vec3::new(body.pos[0], body.pos[1], body.pos[2]);
        let yaw = Quat::from_rotation_y(s.yaw);
        let nose = yaw * Quat::from_rotation_x(-s.cushion_pitch) * Vec3::Z;
        let right = yaw * Quat::from_rotation_z(-s.roll) * Vec3::X;
        gizmos.arrow(at, at + nose * hull.half_length.max(0.5), colors::CUSHION);
        gizmos.arrow(at, at + right * hull.half_width.max(0.5), colors::CUSHION);
    }
}

/// `FixedUpdate`, before the step, with [`DetHoverConfig::follow_terrain`] on: the first `Terrain`
/// that was added or changed becomes the floor ([`DetHoverFloor::from_terrain`]).
#[cfg(feature = "client")]
fn follow_terrain_floor(
    config: Res<DetHoverConfig>,
    terrains: Query<Ref<regia_terrain::Terrain>>,
    mut floor: ResMut<DetHoverFloor>,
) {
    if !config.follow_terrain {
        return;
    }
    if let Some(terrain) = terrains.iter().find(|t| t.is_changed()) {
        *floor = DetHoverFloor::from_terrain(&terrain.map);
    }
}

pub mod probe;

// `.hoverhull.ron` v1, a named hover hull file (ADR-8512). Its items are re-exported here, so
// `det_hover::HoverHullAsset` is the same type (ADR-8637).
pub mod hull;
pub use hull::{
    HOVERHULL_FORMAT_VERSION, HOVERHULL_VERSION_TABLE, HoverHullAsset, HoverHullError, HoverHullParts,
    hoverhull_migration_sources, hoverhull_schema_fingerprint, migrate_hoverhull,
};

#[cfg(test)]
mod tests;
