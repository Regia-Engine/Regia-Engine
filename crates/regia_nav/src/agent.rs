// SPDX-License-Identifier: MPL-2.0
//! `NavAgent` and the steering systems (`docs/specs/22-tier2.md` § T2.2): the component that
//! declares an agent's movement parameters, and the `FixedUpdate` system that follows waypoints
//! with ORCA local avoidance.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::attrs::Range;
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use regia_scene::streaming::OriginShifted;

use crate::crowd::{MAX_CROWD_NEIGHBOURS, NavStuckAction, ProximityGrid};
use crate::orca::{OrcaScratch, orca_velocity_with_scratch};
use crate::query::NavPath;

/// The movement parameters of a navigation agent (specs/22 § T2.2: radius, height, speed,
/// acceleration, avoidance priority).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct NavAgent {
    /// Collision radius (metres).
    #[reflect(@Range(0.0..=4.0))]
    pub radius: f32,
    /// Standing height (metres). Stored on the component. [`NavPlugin`] steering does not read it:
    /// the step integrates XZ and the game owns vertical placement. A crowd uses
    /// [`crate::CrowdAgentParams::height`] for its own vertical overlap test; that field is separate.
    #[reflect(@Range(0.0..=8.0))]
    pub height: f32,
    /// Maximum speed (metres/second).
    #[reflect(@Range(0.0..=40.0))]
    pub max_speed: f32,
    /// Acceleration toward the preferred velocity (metres/second²).
    #[reflect(@Range(0.0..=200.0))]
    pub acceleration: f32,
    /// Stored avoidance weight. [`NavPlugin`] does not read it: ORCA treats every neighbour the
    /// same. Reserved for a later priority-weighted avoidance.
    #[reflect(@Range(0.0..=1.0))]
    pub avoidance_priority: f32,
    /// The ORCA look-ahead time horizon (seconds).
    #[reflect(@Range(0.1..=10.0))]
    pub time_horizon: f32,
}

impl Default for NavAgent {
    fn default() -> Self {
        Self {
            radius: 0.35,
            height: 1.6,
            max_speed: 3.5,
            acceleration: 12.0,
            avoidance_priority: 0.5,
            time_horizon: 1.5,
        }
    }
}

impl NavAgent {
    /// The parameters one step uses (audit-v035-numerics M1, L1): the `@Range` attributes are
    /// not enforced at runtime, so a non-finite field takes its default and a negative one
    /// floors at 0. Finite in-range parameters come back unchanged (bit-exact steps).
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let at_least_zero = |value: f32, fallback: f32| {
            if value.is_finite() {
                value.max(0.0)
            } else {
                fallback
            }
        };
        Self {
            radius: at_least_zero(self.radius, d.radius),
            height: at_least_zero(self.height, d.height),
            max_speed: at_least_zero(self.max_speed, d.max_speed),
            acceleration: at_least_zero(self.acceleration, d.acceleration),
            avoidance_priority: at_least_zero(self.avoidance_priority, d.avoidance_priority),
            time_horizon: at_least_zero(self.time_horizon, d.time_horizon),
        }
    }
}

/// The per-agent runtime state: current velocity, the waypoint list being followed, and the
/// index of the next waypoint. Written by [`NavAgent`] steering. Reflected so tooling can inspect
/// it, but not persisted (it is transient runtime state).
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct NavAgentState {
    /// Current velocity (metres/second, XZ).
    pub velocity: Vec3,
    /// The destination (the last waypoint), if any.
    pub target: Option<Vec3>,
    /// The waypoint list (world space).
    pub waypoints: Vec<Vec3>,
    /// Index of the next waypoint to reach.
    pub next_waypoint: usize,
    /// Progress toward the next waypoint, for stuck detection (NM-2: only with a
    /// [`NavAgentStuckConfig`] resource). [`NavStuckProgress::is_stuck`] is the owner's signal.
    pub stuck: NavStuckProgress,
}

/// One [`NavAgent`]'s progress record for stuck detection (NM-2), kept in
/// [`NavAgentState::stuck`]. [`NavAgentState::set_path`] and [`NavAgentState::clear_path`] start it
/// over.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default)]
pub struct NavStuckProgress {
    /// The waypoint index being approached (`usize::MAX`: not tracking; `usize::MAX - 1`: the
    /// ladder re-planned, so the next index re-arms the distance without counting as progress).
    waypoint: usize,
    /// The closest the agent has come to it (XZ metres).
    best: f32,
    /// Steps since `best` last improved by the minimum progress.
    steps: u32,
    /// The ladder rung reached: 0 none, 1 re-pathed, 2 nudged, 3 or more reported.
    stage: u32,
    /// Where the agent stood at the last rung.
    since: Vec3,
}

impl Default for NavStuckProgress {
    fn default() -> Self {
        Self {
            waypoint: usize::MAX,
            best: 0.0,
            steps: 0,
            stage: 0,
            since: Vec3::ZERO,
        }
    }
}

impl NavStuckProgress {
    /// Whether the ladder ran out ([`NavStuckAction::Reported`]): the agent still follows its
    /// waypoints, and the owner decides what to do (a new target, a teleport, giving up).
    pub fn is_stuck(&self) -> bool {
        self.stage >= 3
    }

    /// The ladder rung reached: 0 none, 1 re-pathed, 2 nudged, 3 or more reported.
    pub fn stage(&self) -> u32 {
        self.stage
    }

    /// Steps since the agent last came `min_progress_m` closer to its next waypoint.
    pub fn steps_without_progress(&self) -> u32 {
        self.steps
    }
}

/// Stuck detection for [`NavPlugin`] steering (NM-2). Insert it as a resource to switch it on: an
/// agent with a plan that has not come `min_progress_m` closer to its next waypoint for
/// `window_steps` fixed steps climbs one rung of the ladder each such window: it re-plans from
/// where it stands, then moves up to one radius toward the center of the polygon under it, then
/// reports ([`NavStuckProgress::is_stuck`] and a [`NavAgentStuck`] message). Without the resource
/// (or with `window_steps` 0) steering is exactly as before.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct NavAgentStuckConfig {
    /// Fixed steps without progress per rung (0: off).
    pub window_steps: u32,
    /// The approach toward the next waypoint (metres, `> 0`) that counts as progress.
    pub min_progress_m: f32,
}

impl Default for NavAgentStuckConfig {
    fn default() -> Self {
        Self {
            window_steps: 60,
            min_progress_m: crate::crowd::DEFAULT_STUCK_MIN_PROGRESS_M,
        }
    }
}

/// A rung of the stuck ladder a [`NavAgent`] took ([`NavAgentStuckConfig`]). [`NavPlugin`]
/// registers it; read it in `FixedUpdate` after [`NavStep`].
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct NavAgentStuck {
    /// The agent's entity.
    pub entity: Entity,
    /// What the step did.
    pub action: NavStuckAction,
    /// The agent's position when it was found stuck (before a nudge).
    pub position: Vec3,
}

impl NavAgentState {
    /// Installs `path` (typically from [`crate::query::NavQuery::find_path`]) as the current plan,
    /// skipping the first waypoint (the agent's own position).
    pub fn set_path(&mut self, path: NavPath) {
        self.target = path.waypoints.last().copied();
        self.waypoints = path.waypoints;
        self.next_waypoint = 1.min(self.waypoints.len());
        self.stuck = NavStuckProgress::default();
    }

    /// Clears the current plan and target.
    pub fn clear_path(&mut self) {
        self.target = None;
        self.waypoints.clear();
        self.next_waypoint = 0;
        self.stuck = NavStuckProgress::default();
    }

    /// Moves the destination and every waypoint by `delta` (the floating-origin step of spec 78
    /// § 12.2); the velocity is a difference and stays.
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        if let Some(target) = &mut self.target {
            *target += delta;
        }
        for waypoint in &mut self.waypoints {
            *waypoint += delta;
        }
    }

    /// Whether the agent has a plan it is still following.
    pub fn has_path(&self) -> bool {
        self.next_waypoint < self.waypoints.len()
    }
}

/// The current waypoint being approached (if any).
impl NavAgentState {
    /// The waypoint the agent is heading toward.
    pub fn current_waypoint(&self) -> Option<Vec3> {
        self.waypoints.get(self.next_waypoint).copied()
    }
}

/// Installs [`NavAgent`] steering: registers the components and steps every agent in `FixedUpdate`
/// (in [`NavStep`]). Requires a [`NavMesh`](crate::NavMesh) resource to be present; steering does not read
/// `NavObstacles` (carving applies to `NavQuery` path planning only).
#[derive(Default)]
pub struct NavPlugin;

impl Plugin for NavPlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        app.register_type::<NavAgent>()
            .register_type::<NavAgentState>()
            .register_type::<NavStuckProgress>()
            .add_message::<NavAgentStuck>()
            .add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
    }
}

/// The fixed-step agent steering of [`NavPlugin`]. Gameplay that plans or replaces paths orders
/// itself `.before(NavStep)` so agents follow the new plan on the same step, and systems that read
/// the moved agents order themselves `.after(NavStep)`. Left unordered, the executor may pick
/// either order, and the order can change with the thread count (ADR-867).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NavStep;

/// Default look-ahead distance used when planning a new path for an agent.
pub const DEFAULT_PATH_EXTENTS: Vec3 = Vec3::new(2.0, 2.0, 2.0);

/// `FixedUpdate`: advances each agent toward its next waypoint using ORCA local avoidance, then
/// integrates its position by the fixed step (`FixedTime::step`), so speed is independent of the
/// frame rate. Agents without a mesh or without a plan coast to a stop.
///
/// The agent states and the nav stores move with any pending origin shift first (the shared
/// cursor: exactly one nav system of the tick applies it). Every [`NavAgentState`] moves,
/// including agents this step will not steer, so a parked plan stays in the new frame and no
/// agent steers a step with pre-shift waypoints whatever the executor picks (audit
/// sprint-audit-g-nav-determinism H2).
#[tracing::instrument(skip_all, target = "regia_nav", name = "nav_step")]
#[allow(clippy::type_complexity)]
pub(crate) fn step_nav_agents(
    mut param: ParamSet<(
        Query<(Entity, &NavAgent, &mut NavAgentState, &mut Transform), Without<LayerParked>>,
        Query<(Entity, &NavAgent, &NavAgentState, &Transform), Without<LayerParked>>,
        Query<&mut NavAgentState>,
    )>,
    mut stores: crate::origin::NavStores,
    mut origin_cursor: Option<ResMut<crate::origin::NavOriginShiftCursor>>,
    origin_messages: Option<Res<Messages<OriginShifted>>>,
    fixed: Res<FixedTime>,
    mut orca_scratch: Local<OrcaScratch>,
    stuck: (
        Option<Res<NavAgentStuckConfig>>,
        Option<ResMut<Messages<NavAgentStuck>>>,
    ),
) {
    let (stuck_config, mut stuck_messages) = stuck;
    // H2: apply the floating-origin shift before anything reads world positions. The third query
    // is every agent, parked included: `p0` will not steer those, and this system may be the one
    // that consumes the cursor.
    if let (Some(cursor), Some(messages)) = (origin_cursor.as_deref_mut(), origin_messages.as_deref()) {
        let deltas = crate::origin::apply_origin_shifts(&mut cursor.0, messages, &mut stores);
        let mut states = param.p2();
        crate::origin::shift_agent_states(&deltas, &mut states);
    }
    if stores.mesh.is_none() {
        return;
    }
    // The fixed step, never the frame delta: this runs once per fixed step, so a frame delta
    // would tie the agents' speed to the frame rate and make replays depend on frame timing
    // (ADR-537, ADR-867).
    let dt = fixed_step_secs(&fixed);

    // Snapshot every live agent once. Each agent then keeps at most MAX_CROWD_NEIGHBOURS others
    // inside its time horizon (the crowd's cap). The kept set is sorted by physical bits, which
    // is the order the all-pairs pass used, so a group under the cap stays bit-identical
    // (audit sprint-audit-g-nav-determinism M4). A self entry yields no line and is left out.
    let snaps: Vec<AgentSnap> = param
        .p1()
        .iter()
        .map(|(_, agent, state, transform)| {
            let agent = agent.sanitized();
            AgentSnap {
                pos: transform.translation,
                vel: state.velocity,
                radius: agent.radius,
                max_speed: agent.max_speed,
            }
        })
        .collect();
    let max_radius = snaps.iter().map(|s| s.radius).fold(0.0, f32::max);
    let max_speed = snaps.iter().map(|s| s.max_speed).fold(0.0, f32::max);
    let positions: Vec<Vec3> = snaps.iter().map(|s| s.pos).collect();
    let grid = ProximityGrid::build(&positions, 2.0);
    let mut candidates = Vec::new();
    let mut ranked: Vec<RankedNeighbour> = Vec::new();
    let mut neighbor_buf: Vec<(Vec3, Vec3, f32)> = Vec::new();

    let stuck_config = stuck_config.map(|c| *c).filter(|c| c.window_steps > 0);
    let mesh = stores.mesh.as_deref();
    for (index, (entity, agent, mut state, mut transform)) in param.p0().iter_mut().enumerate() {
        // Sanitized parameters, and a non-finite stored velocity restarts from rest: a NaN or
        // negative acceleration latched a NaN velocity and Transform, or accelerated the agent
        // away from where it wanted to go (audit-v035-numerics M1).
        let agent = &agent.sanitized();
        if !state.velocity.is_finite() {
            state.velocity = Vec3::ZERO;
        }
        // Advance past reached waypoints.
        advance_waypoints(&mut state, transform.translation, agent.radius);

        // Preferred velocity: straight toward the current waypoint at full speed.
        let preferred = state
            .current_waypoint()
            .map(|wp| {
                let to = wp - transform.translation;
                let horiz = Vec3::new(to.x, 0.0, to.z);
                if horiz.length_squared() > 1e-6 {
                    horiz.normalize() * agent.max_speed
                } else {
                    Vec3::ZERO
                }
            })
            .unwrap_or(Vec3::ZERO);

        // ORCA against the nearest neighbours inside the horizon, then in bit order.
        neighbor_buf.clear();
        if let Some(me) = snaps.get(index) {
            let horizon = agent.time_horizon.max(0.0);
            let reach = agent.radius + max_radius + (agent.max_speed + max_speed) * horizon;
            grid.query(me.pos, reach, &mut candidates);
            ranked.clear();
            for &j in &candidates {
                let j = j as usize;
                if j == index {
                    continue;
                }
                let Some(other) = snaps.get(j) else {
                    continue;
                };
                let limit = agent.radius + other.radius + (agent.max_speed + other.max_speed) * horizon;
                let dx = other.pos.x - me.pos.x;
                let dz = other.pos.z - me.pos.z;
                let dist_sq = dx * dx + dz * dz;
                if dist_sq > limit * limit {
                    continue;
                }
                ranked.push(RankedNeighbour {
                    dist: dist_sq.sqrt(),
                    key: avoidance_key(other.pos, other.vel, other.radius),
                    index: j,
                });
            }
            ranked.sort_by(|a, b| {
                a.dist
                    .total_cmp(&b.dist)
                    .then(a.key.cmp(&b.key))
                    .then(a.index.cmp(&b.index))
            });
            ranked.truncate(MAX_CROWD_NEIGHBOURS);
            ranked.sort_by(|a, b| a.key.cmp(&b.key).then(a.index.cmp(&b.index)));
            for entry in &ranked {
                let other = &snaps[entry.index];
                neighbor_buf.push((other.pos, other.vel, other.radius));
            }
        }
        let desired = orca_velocity_with_scratch(
            &mut orca_scratch,
            transform.translation,
            state.velocity,
            preferred,
            agent.radius,
            agent.max_speed,
            agent.time_horizon,
            dt,
            &neighbor_buf,
        );

        // Accelerate toward the desired velocity (simple exponential/linear approach).
        let delta = desired - state.velocity;
        let max_step = agent.acceleration * dt;
        let applied = if delta.length_squared() <= max_step * max_step {
            desired
        } else {
            state.velocity + delta.normalize_or_zero() * max_step
        };
        // A velocity that is still not finite (a non-finite position upstream) stops the agent
        // instead of moving its Transform.
        state.velocity = if applied.is_finite() { applied } else { Vec3::ZERO };

        // Integrate XZ (the game owns vertical placement).
        transform.translation.x += state.velocity.x * dt;
        transform.translation.z += state.velocity.z * dt;

        if let Some(config) = stuck_config
            && let Some(action) = track_stuck(&mut state, &mut transform, agent, mesh, config)
            && let Some(messages) = stuck_messages.as_deref_mut()
        {
            messages.write(NavAgentStuck {
                entity,
                action,
                position: state.stuck.since,
            });
        }
    }
}

/// One step of stuck detection for a [`NavAgent`] (NM-2): updates its progress toward the
/// current waypoint and, after a window without progress, takes the next rung (re-plan, nudge,
/// report). Returns the rung taken this step.
fn track_stuck(
    state: &mut NavAgentState,
    transform: &mut Transform,
    agent: &NavAgent,
    mesh: Option<&crate::NavMesh>,
    config: NavAgentStuckConfig,
) -> Option<NavStuckAction> {
    let min_progress = if config.min_progress_m.is_finite() && config.min_progress_m > 0.0 {
        config.min_progress_m
    } else {
        crate::crowd::DEFAULT_STUCK_MIN_PROGRESS_M
    };
    let Some(waypoint) = state.current_waypoint() else {
        state.stuck = NavStuckProgress::default();
        return None;
    };
    let pos = transform.translation;
    let d = horizontal_dist_sq(pos, waypoint).sqrt();
    let index = state.next_waypoint;
    let p = &mut state.stuck;
    if p.waypoint == index {
        if p.best - d >= min_progress {
            p.best = d;
            p.steps = 0;
            p.stage = 0;
        } else {
            p.steps += 1;
        }
    } else if p.waypoint == usize::MAX {
        // Not tracking yet (a new plan from the owner).
        p.waypoint = index;
        p.best = d;
        p.steps = 0;
    } else {
        // A new waypoint. Passed the old one (it came within reach of it): progress. Otherwise
        // the ladder's own re-plan: measure from here but keep the count and the rung.
        let reached = p.best <= agent.radius.max(0.1) + min_progress;
        p.waypoint = index;
        p.best = d;
        if reached {
            p.steps = 0;
            p.stage = 0;
        } else {
            p.steps += 1;
        }
    }
    if p.steps < config.window_steps {
        return None;
    }
    p.steps = 0;
    p.since = pos;
    p.stage = p.stage.saturating_add(1);
    let action = match p.stage {
        1 => NavStuckAction::Repath,
        2 => NavStuckAction::Nudge,
        _ => NavStuckAction::Reported,
    };
    let progress = *p;
    match action {
        NavStuckAction::Repath => {
            let query = mesh.zip(state.target).and_then(|(mesh, target)| {
                crate::NavQuery {
                    mesh,
                    obstacles: None,
                }
                .find_path(pos, target, DEFAULT_PATH_EXTENTS)
            });
            if let Some(path) = query {
                state.set_path(path);
                // The new plan keeps the ladder: a re-plan that does not help climbs on.
                state.stuck = NavStuckProgress {
                    waypoint: usize::MAX - 1,
                    ..progress
                };
            }
        }
        NavStuckAction::Nudge => {
            let center = mesh.and_then(|mesh| {
                crate::query::find_nearest_on(mesh, pos, DEFAULT_PATH_EXTENTS)
                    .and_then(|(poly, _)| mesh.polys.get(poly as usize))
                    .map(|poly| poly.center_xz())
            });
            if let Some(c) = center {
                let to = Vec3::new(c.x - pos.x, 0.0, c.z - pos.z);
                let len = to.length();
                if len > 1e-6 {
                    let step = len.min(agent.radius.max(0.0));
                    transform.translation.x += to.x * (step / len);
                    transform.translation.z += to.z * (step / len);
                }
            }
            state.velocity = Vec3::ZERO;
        }
        NavStuckAction::Reported => {}
    }
    Some(action)
}

/// The integration step of one `FixedUpdate` run: the fixed step length, not `Time::delta_secs`
/// (which stays the frame delta on every substep).
fn fixed_step_secs(fixed: &FixedTime) -> f32 {
    fixed.step().as_secs_f32().max(1e-4)
}

/// Advances [`NavAgentState::next_waypoint`] past any waypoint the agent has reached.
fn advance_waypoints(state: &mut NavAgentState, position: Vec3, radius: f32) {
    let reach = radius.max(0.1);
    while state
        .current_waypoint()
        .is_some_and(|wp| horizontal_dist_sq(position, wp) <= reach * reach)
    {
        state.next_waypoint += 1;
    }
}

/// One agent in the avoidance snapshot, in query order.
struct AgentSnap {
    pos: Vec3,
    vel: Vec3,
    radius: f32,
    max_speed: f32,
}

/// A neighbour that survived the horizon test, before the cap.
struct RankedNeighbour {
    dist: f32,
    key: (u32, u32, u32, u32, u32, u32, u32),
    index: usize,
}

/// The bit key the all-pairs pass sorted by (position, velocity, radius).
fn avoidance_key(p: Vec3, v: Vec3, r: f32) -> (u32, u32, u32, u32, u32, u32, u32) {
    (
        p.x.to_bits(),
        p.z.to_bits(),
        p.y.to_bits(),
        v.x.to_bits(),
        v.z.to_bits(),
        v.y.to_bits(),
        r.to_bits(),
    )
}

/// Squared horizontal distance between two points.
fn horizontal_dist_sq(a: Vec3, b: Vec3) -> f32 {
    let dx = a.x - b.x;
    let dz = a.z - b.z;
    dx * dx + dz * dz
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce as _;

    #[test]
    fn a_parked_agent_does_not_step() {
        use regia_scene::layers::LayerParked;
        use regia_scene::prelude::Visibility;

        let half = 6.0;
        let floor = [
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
        let mut state = NavAgentState::default();
        state.set_path(NavPath {
            polys: vec![0],
            waypoints: vec![Vec3::ZERO, Vec3::new(5.0, 0.0, 0.0)],
            length: 5.0,
        });
        let parked = world
            .spawn((
                NavAgent::default(),
                state.clone(),
                Transform::default(),
                LayerParked {
                    restore: Visibility::Inherited,
                    inherited: false,
                },
            ))
            .id();
        let live = world
            .spawn((NavAgent::default(), state, Transform::default()))
            .id();
        for _ in 0..4 {
            world.run_system_once(step_nav_agents).expect("the system runs");
        }
        assert_eq!(world.get::<Transform>(parked).expect("t").translation, Vec3::ZERO);
        assert!(world.get::<Transform>(live).expect("t").translation.x > 0.0);
    }

    /// `height` and `avoidance_priority` are stored. Steering does not read them: a neighbour ten
    /// metres up still contributes an ORCA plane, and priority 0 and 1 take the same step.
    #[test]
    fn height_and_avoidance_priority_do_not_change_steering() {
        let floor = [
            [
                Vec3::new(-6.0, 0.0, -6.0),
                Vec3::new(6.0, 0.0, -6.0),
                Vec3::new(6.0, 0.0, 6.0),
            ],
            [
                Vec3::new(-6.0, 0.0, -6.0),
                Vec3::new(6.0, 0.0, 6.0),
                Vec3::new(-6.0, 0.0, 6.0),
            ],
        ];
        let step_of = |other_y: f32, priority_a: f32, priority_b: f32| {
            let mut world = World::new();
            world.insert_resource(FixedTime::default());
            world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
            let plan = |from: Vec3, to: Vec3| {
                let mut state = NavAgentState::default();
                state.set_path(NavPath {
                    polys: vec![0],
                    waypoints: vec![from, to],
                    length: (to - from).length(),
                });
                state
            };
            let a = NavAgent {
                avoidance_priority: priority_a,
                height: 1.6,
                ..NavAgent::default()
            };
            let b = NavAgent {
                avoidance_priority: priority_b,
                height: 1.6,
                ..NavAgent::default()
            };
            let ea = world
                .spawn((
                    a,
                    plan(Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0)),
                    Transform::from_translation(Vec3::ZERO),
                ))
                .id();
            world.spawn((
                b,
                plan(Vec3::new(0.4, other_y, 0.0), Vec3::new(-4.0, other_y, 0.0)),
                Transform::from_translation(Vec3::new(0.4, other_y, 0.0)),
            ));
            world.run_system_once(step_nav_agents).expect("the system runs");
            world.get::<NavAgentState>(ea).expect("state").velocity
        };
        let ground = step_of(0.0, 0.0, 1.0);
        let stacked = step_of(10.0, 1.0, 0.0);
        assert_eq!(ground, stacked, "height and priority are not read");
        assert!(ground.length() > 0.0, "the agent did step");
    }

    #[test]
    fn velocities_do_not_depend_on_spawn_or_archetype_order() {
        // M4 (audit sprint-audit-g-nav-determinism): an infeasible ORCA set is repaired from the
        // first failing line, so the result follows the neighbour (line) order. Query order is
        // archetype order; the velocities must not change when the spawn order does.
        let half = 8.0;
        let floor = [
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        // The infeasible fixture of orca.rs: an agent at the origin with four neighbours closing
        // in has no velocity inside the disc that satisfies every half-plane.
        let homes = [
            (Vec3::new(1.2, 0.0, 0.0), Vec3::new(-3.0, 0.0, 0.0)),
            (Vec3::new(-1.2, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)),
            (Vec3::new(0.0, 0.0, 1.2), Vec3::new(0.0, 0.0, -3.0)),
            (Vec3::new(0.0, 0.0, -1.2), Vec3::new(0.0, 0.0, 3.0)),
        ];
        let build = |reverse: bool| {
            let mut world = World::new();
            world.insert_resource(FixedTime::default());
            world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
            let center = {
                let mut state = NavAgentState {
                    velocity: Vec3::ZERO,
                    ..Default::default()
                };
                state.set_path(NavPath {
                    polys: vec![0],
                    waypoints: vec![Vec3::new(4.0, 0.0, 0.0)],
                    length: 4.0,
                });
                world
                    .spawn((NavAgent::default(), state, Transform::default()))
                    .id()
            };
            let order: Vec<_> = if reverse {
                homes.iter().rev().collect()
            } else {
                homes.iter().collect()
            };
            let mut spawned = Vec::new();
            for &(home, vel) in order {
                let mut state = NavAgentState {
                    velocity: vel,
                    ..Default::default()
                };
                state.set_path(NavPath {
                    polys: vec![0],
                    waypoints: vec![home + vel.normalize_or_zero() * 8.0],
                    length: 8.0,
                });
                spawned.push(
                    world
                        .spawn((NavAgent::default(), state, Transform::from_translation(home)))
                        .id(),
                );
            }
            world.run_system_once(step_nav_agents).expect("the system runs");
            let vel = |e: Entity| world.get::<NavAgentState>(e).expect("state").velocity;
            let pos = |e: Entity| world.get::<Transform>(e).expect("t").translation;
            let mut outer = spawned.iter().map(|&e| (pos(e), vel(e))).collect::<Vec<_>>();
            // The spawn order differs between the two worlds; compare per position.
            outer.sort_by_key(|(p, _)| (p.x.to_bits(), p.z.to_bits()));
            (vel(center), pos(center), outer)
        };
        let (vc, pc, va) = build(false);
        let (vc2, pc2, va2) = build(true);
        assert_eq!(
            vc, vc2,
            "the centered agent's velocity follows the physics, not the spawn order"
        );
        assert_eq!(pc, pc2);
        for (a, b) in va.iter().zip(&va2) {
            assert_eq!(a, b);
        }
    }

    #[test]
    fn step_uses_fixed_step_not_frame_delta() {
        let mut fixed = FixedTime::default();
        assert!((fixed_step_secs(&fixed) - 1.0 / 60.0).abs() < 1e-6);
        fixed.hz = 30.0;
        assert!((fixed_step_secs(&fixed) - 1.0 / 30.0).abs() < 1e-6);
    }

    #[test]
    fn defaults_are_sensible() {
        let a = NavAgent::default();
        assert!(a.radius > 0.0 && a.height > a.radius);
        assert!(a.max_speed > 0.0 && a.acceleration > 0.0);
        assert!(a.time_horizon > 0.0);
    }

    #[test]
    fn set_path_and_clear() {
        let mut state = NavAgentState::default();
        state.set_path(NavPath {
            polys: vec![0, 1],
            waypoints: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 0.0, 0.0)],
            length: 2.0,
        });
        assert!(state.has_path());
        assert_eq!(state.current_waypoint(), Some(Vec3::new(1.0, 0.0, 0.0)));
        state.clear_path();
        assert!(!state.has_path());
        assert!(state.target.is_none());
    }

    #[test]
    fn advance_waypoints_skips_reached() {
        let mut state = NavAgentState {
            waypoints: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 0.0),
            ],
            next_waypoint: 1,
            ..Default::default()
        };
        // The agent is at the first waypoint and close to the second.
        advance_waypoints(&mut state, Vec3::new(0.9, 0.0, 0.0), 0.35);
        assert_eq!(state.next_waypoint, 2, "reached waypoint 1 is skipped");
        assert_eq!(state.current_waypoint(), Some(Vec3::new(2.0, 0.0, 0.0)));
    }

    #[test]
    fn advance_waypoints_uses_horizontal_distance() {
        let make = || NavAgentState {
            waypoints: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 0.0),
            ],
            next_waypoint: 1,
            ..Default::default()
        };
        // 1 m from waypoint 1, radius 0.35: not reached.
        let mut far = make();
        advance_waypoints(&mut far, Vec3::new(0.0, 0.0, 0.0), 0.35);
        assert_eq!(far.next_waypoint, 1, "an unreached waypoint is kept");
        // Directly above waypoint 1 (50 m up): reached by horizontal distance, waypoint 2 is not.
        let mut above = make();
        advance_waypoints(&mut above, Vec3::new(1.0, 50.0, 0.0), 0.35);
        assert_eq!(
            above.next_waypoint, 2,
            "height is ignored, only waypoint 1 is skipped"
        );
    }
}

/// NM-2: stuck detection for `NavAgent` steering.
#[cfg(test)]
mod stuck_tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce as _;

    /// An agent heading for (5, 0, 0) with a still neighbour standing exactly there: ORCA stops it
    /// a radius sum short of the waypoint, which it never reaches.
    fn blocked_world(config: Option<NavAgentStuckConfig>) -> (World, Entity) {
        let half = 6.0;
        let floor = [
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
        world.insert_resource(Messages::<NavAgentStuck>::default());
        if let Some(config) = config {
            world.insert_resource(config);
        }
        let mut state = NavAgentState::default();
        state.set_path(NavPath {
            polys: vec![0],
            waypoints: vec![Vec3::ZERO, Vec3::new(5.0, 0.0, 0.0)],
            length: 5.0,
        });
        let walker = world
            .spawn((NavAgent::default(), state, Transform::default()))
            .id();
        world.spawn((
            NavAgent {
                max_speed: 0.0,
                ..NavAgent::default()
            },
            NavAgentState::default(),
            Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        ));
        (world, walker)
    }

    #[test]
    fn a_nav_agent_blocked_short_of_its_waypoint_climbs_the_ladder() {
        // Off: it stands short of the waypoint for good and nothing is reported.
        let (mut world, walker) = blocked_world(None);
        for _ in 0..600 {
            world.run_system_once(step_nav_agents).expect("the system runs");
        }
        let state = world.get::<NavAgentState>(walker).expect("state");
        assert!(state.has_path(), "it never reaches the waypoint");
        assert_eq!(state.stuck, NavStuckProgress::default());
        assert!(
            world
                .resource::<Messages<NavAgentStuck>>()
                .iter_current_update_messages()
                .next()
                .is_none()
        );

        // On: re-plan, nudge, report, one window each (plus the approach).
        let config = NavAgentStuckConfig {
            window_steps: 30,
            min_progress_m: 0.5,
        };
        let (mut world, walker) = blocked_world(Some(config));
        let mut reported_at = None;
        for step in 0..600 {
            world.run_system_once(step_nav_agents).expect("the system runs");
            let state = world.get::<NavAgentState>(walker).expect("state");
            if state.stuck.is_stuck() {
                reported_at = Some(step);
                break;
            }
        }
        let reported_at = reported_at.expect("reported within 600 steps");
        // The approach takes about 1.5 s at 3.5 m/s; then three windows of 30 steps.
        assert!(reported_at < 300, "reported at step {reported_at}");
        let actions: Vec<NavStuckAction> = world
            .resource::<Messages<NavAgentStuck>>()
            .iter_current_update_messages()
            .filter(|m| m.entity == walker)
            .map(|m| m.action)
            .collect();
        assert_eq!(
            actions,
            vec![
                NavStuckAction::Repath,
                NavStuckAction::Nudge,
                NavStuckAction::Reported
            ]
        );
        // A new plan from the owner starts the record over.
        let mut state = world.get_mut::<NavAgentState>(walker).expect("state");
        state.set_path(NavPath {
            polys: vec![0],
            waypoints: vec![Vec3::ZERO, Vec3::new(-5.0, 0.0, 0.0)],
            length: 5.0,
        });
        assert!(!state.stuck.is_stuck());
    }
}

/// audit-v035-numerics M1 (fix-audit-wave12, ADR-2395).
#[cfg(test)]
mod audit_v035_tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce as _;

    fn world_with_agent(agent: NavAgent) -> (World, Entity) {
        let half = 6.0;
        let floor = [
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
        let mut state = NavAgentState::default();
        state.set_path(NavPath {
            polys: vec![0],
            waypoints: vec![Vec3::ZERO, Vec3::new(5.0, 0.0, 0.0)],
            length: 5.0,
        });
        let e = world.spawn((agent, state, Transform::default())).id();
        (world, e)
    }

    /// A NaN acceleration latched a NaN velocity and Transform for good; restoring the
    /// parameter never recovered the agent.
    #[test]
    fn a_nan_acceleration_does_not_latch_a_nan_agent() {
        let (mut world, e) = world_with_agent(NavAgent {
            acceleration: f32::NAN,
            ..NavAgent::default()
        });
        for _ in 0..3 {
            world.run_system_once(step_nav_agents).expect("the system runs");
        }
        world.get_mut::<NavAgent>(e).expect("agent").acceleration = NavAgent::default().acceleration;
        for _ in 0..30 {
            world.run_system_once(step_nav_agents).expect("the system runs");
        }
        let t = world.get::<Transform>(e).expect("t").translation;
        let v = world.get::<NavAgentState>(e).expect("state").velocity;
        assert!(t.is_finite() && v.is_finite(), "{t:?} {v:?}");
        assert!(t.x > 0.1, "walks to its waypoint again: {t:?}");
    }

    /// A negative acceleration pushed the agent away from its desired velocity every step
    /// until the speed overflowed; it now reads as no acceleration (the agent stays at rest).
    #[test]
    fn a_negative_acceleration_does_not_run_away() {
        let (mut world, e) = world_with_agent(NavAgent {
            acceleration: -12.0,
            ..NavAgent::default()
        });
        for _ in 0..120 {
            world.run_system_once(step_nav_agents).expect("the system runs");
        }
        let v = world.get::<NavAgentState>(e).expect("state").velocity;
        let max = NavAgent::default().max_speed;
        assert!(
            v.is_finite() && v.length() <= max + 1e-3,
            "speed {} above {max}",
            v.length()
        );
    }
}

/// audit sprint-audit-g-nav-perf H1: one step must not build an ORCA line for every other agent.
#[cfg(test)]
mod neighbour_cap_tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce as _;

    #[test]
    fn nav_agent_step_neighbour_work() {
        let half = 20.0;
        let floor = [
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        world.insert_resource(crate::NavMeshBuilder::default().build(&floor).expect("floor"));
        let mut state = NavAgentState::default();
        state.set_path(NavPath {
            polys: vec![0],
            waypoints: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0)],
            length: 1.0,
        });
        // 0.4 m spacing with radius 0.35: the row overlaps, and a middle agent can see about
        // twenty-seven others inside the time horizon, well above the neighbour cap.
        let n = 64usize;
        for i in 0..n {
            world.spawn((
                NavAgent {
                    radius: 0.35,
                    ..NavAgent::default()
                },
                state.clone(),
                Transform::from_translation(Vec3::new(i as f32 * 0.4, 0.0, 0.0)),
            ));
        }
        crate::orca::reset_lines_built();
        world.run_system_once(step_nav_agents).expect("the system runs");
        let lines = crate::orca::lines_built();
        let cap = (n * crate::crowd::MAX_CROWD_NEIGHBOURS) as u64;
        assert!(lines > 0, "the packed row builds ORCA lines, got {lines}");
        assert!(
            lines <= cap,
            "ORCA lines {lines} exceed {n} agents * {} neighbours = {cap}",
            crate::crowd::MAX_CROWD_NEIGHBOURS
        );
    }
}
