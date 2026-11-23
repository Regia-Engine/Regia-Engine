// SPDX-License-Identifier: MPL-2.0
//! Path providers and the `MoveTo` leaf (ADR-8293).
//!
//! A [`PathProvider`] answers "how do I get from here to there" for one navigation stack, behind
//! one object-safe trait, so a behavior-tree leaf or a state-tree task does not care which stack a
//! game uses:
//!
//! - [`NavMeshProvider`]: the f32 navmesh ([`regia_nav::NavMesh`] and [`regia_nav::NavQuery`], with
//!   the [`regia_nav::NavObstacles`] carving when that resource exists). The [`MoveTo`] leaf hands its
//!   target to the [`regia_nav::NavCrowd`] when the agent entity carries [`CrowdSteered`].
//! - [`DetGridProvider`]: the deterministic grid ([`regia_nav::det::grid`] and
//!   [`regia_nav::det::search`]), with an optional per-tick work budget that queues requests past it
//!   as [`PathRequestOutcome::Pending`] tickets served in ticket order.
//! - [`DetNavMeshProvider`]: the deterministic navmesh ([`regia_nav::det::navmesh`]).
//!
//! Providers live by name in the [`PathProviders`] resource ([`crate::AiPlugin`] installs it with a
//! `navmesh` provider; a game inserts the others). Positions cross the seam as world meters in a
//! [`Vec3`]: the grid takes `(x, z)` and its waypoints come back at the grid's ground height
//! ([`regia_nav::det::grid::NavGrid::surface_y`]); the deterministic navmesh takes and returns
//! `[x, y, z]` unchanged. Nothing here is imported by `regia_nav::det`, which stays free of ECS
//! types.
//!
//! [`MoveTo`] plans through the named provider, follows the path itself (or lets the crowd walk it),
//! succeeds once the agent is within `acceptance_radius` of the target across the ground (and within
//! its body height vertically), fails
//! on a failed or (by default) a partial plan, and re-plans when the target moves more than
//! `replan_distance` from where it was planned. Its progress goes to the blackboard under
//! `<progress_key>_state`, `_remaining`, `_replans` and `_error`, and to the [`MoveToStatus`]
//! component that `ai.bt_live` reports. The same task runs as a state-tree task: state trees tick
//! the same [`BtTask`] types.
//!
//! The follow step uses IEEE `+ - * /` and `sqrt` (correctly rounded on every target) in a fixed
//! order, so two runs of the same binary from the same inputs move an agent the same way. The
//! deterministic providers' plans are bit-stable across targets; the leaf itself runs inside the
//! ECS tick, which a lockstep sim must drive at its own fixed step.
//!
//! See `book/user-guide/choosing-navigation.md` and `book/agents/behavior-tree-editor.md`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::det::grid::{NavGrid, Profile};
use regia_nav::det::navmesh::{MeshAgent, NavMesh as DetNavMesh};
use regia_nav::det::search::{self, PlanKind, Scratch};
use regia_nav::{
    CrowdAgentId, CrowdAgentState, DEFAULT_PATH_EXTENTS, NavCrowd, NavMesh, NavObstacles, NavQuery,
};

use crate::blackboard::{BbValue, Blackboard};
use crate::task::{BtContext, BtStatus, BtTask};
use crate::tree::BtTaskRegistry;

/// The task path of the blackboard-driven [`MoveTo`] leaf [`crate::AiPlugin`] registers (the BT and
/// state-tree editors list it in their palettes). It reads its target from `move_target` (a vec3 or
/// an entity), its radius from `move_acceptance_radius` (a float, 0.5 m without one) and its provider
/// from `move_provider` (a string, `navmesh` without one).
pub const MOVE_TO_TASK: &str = "regia_ai::MoveTo";

/// The name [`crate::AiPlugin`] gives its [`NavMeshProvider`].
pub const NAVMESH_PROVIDER: &str = "navmesh";

/// The body asking for a path. Each provider reads the fields its stack uses: the navmesh its
/// `extents`, the grid its `profile`, the deterministic navmesh the radius, height and steps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathAgentSpec {
    /// Body radius (m).
    pub radius: f32,
    /// Body height (m).
    pub height: f32,
    /// Highest step the body climbs (m).
    pub step_up: f32,
    /// Deepest step the body drops (m).
    pub step_down: f32,
    /// The grid movement profile ([`regia_nav::det::grid::Profile`]'s index; 0 is `HOVER`).
    pub profile: u8,
    /// The navmesh search box half-extents used to snap endpoints onto the mesh (m).
    pub extents: Vec3,
}

impl Default for PathAgentSpec {
    fn default() -> Self {
        Self {
            radius: regia_nav::det::navmesh::FOOT_RADIUS_M,
            height: regia_nav::det::navmesh::FOOT_HEIGHT_M,
            step_up: regia_nav::det::navmesh::FOOT_STEP_UP_M,
            step_down: regia_nav::det::navmesh::FOOT_STEP_DOWN_M,
            profile: Profile::HOVER.0,
            extents: DEFAULT_PATH_EXTENTS,
        }
    }
}

/// A queued request's handle ([`PathRequestOutcome::Pending`]); poll it with [`PathProvider::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathTicket(pub u64);

/// Why a plan stopped short of the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialReason {
    /// The goal is not reachable; the path ends at the nearest reachable point.
    GoalUnreachable,
    /// The goal stood on blocked ground and was moved to the nearest free point.
    GoalSnapped,
}

impl PartialReason {
    /// The protocol code: `partial_unreachable` or `partial_goal_snapped`.
    pub fn code(self) -> &'static str {
        match self {
            Self::GoalUnreachable => "partial_unreachable",
            Self::GoalSnapped => "partial_goal_snapped",
        }
    }
}

/// Why a provider could not plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFailure {
    /// The provider has no navigation data (no navmesh resource, for example).
    NoNavData,
    /// The start is off the navigation data.
    StartOffNav,
    /// The goal is off the navigation data.
    GoalOffNav,
    /// No route joins the start and the goal (or the search budget ran out).
    Unreachable,
    /// A non-finite position or an agent the provider cannot plan for (an unknown grid profile).
    InvalidInput,
    /// The ticket is not one the provider knows (served and taken already, or cancelled).
    UnknownTicket,
}

impl PathFailure {
    /// The protocol code: `no_nav_data`, `start_off_nav`, `goal_off_nav`, `unreachable`,
    /// `invalid_input` or `unknown_ticket`.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoNavData => "no_nav_data",
            Self::StartOffNav => "start_off_nav",
            Self::GoalOffNav => "goal_off_nav",
            Self::Unreachable => "unreachable",
            Self::InvalidInput => "invalid_input",
            Self::UnknownTicket => "unknown_ticket",
        }
    }
}

/// What a plan request answered.
#[derive(Debug, Clone, PartialEq)]
pub enum PathRequestOutcome {
    /// A path to the goal: world points, the first at (or snapped from) the start.
    Ready(Vec<Vec3>),
    /// Queued; poll the ticket on a later tick.
    Pending(PathTicket),
    /// A path that stops short of the goal, and why.
    Partial {
        /// World points, the first at (or snapped from) the start.
        points: Vec<Vec3>,
        /// Why it stops short.
        reason: PartialReason,
    },
    /// No path.
    Failed(PathFailure),
}

/// One navigation stack behind the `MoveTo` seam. Object-safe: [`PathProviders`] stores
/// `Box<dyn PathProvider>`. `world` is the ECS world the request comes from (a provider that keeps
/// its own data ignores it).
pub trait PathProvider: Send + Sync + 'static {
    /// The stack's name: `navmesh`, `det_grid`, `det_navmesh`, or a game's own.
    fn kind(&self) -> &'static str;

    /// Plans from `from` to `to` for `agent`.
    fn plan(&mut self, world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> PathRequestOutcome;

    /// The answer to a queued request: `Pending` while it waits, the result once (the ticket is
    /// then forgotten). The default knows no tickets.
    fn poll(&mut self, _world: &World, _ticket: PathTicket) -> PathRequestOutcome {
        PathRequestOutcome::Failed(PathFailure::UnknownTicket)
    }

    /// Drops a queued request or an answer nobody took. The default does nothing.
    fn cancel(&mut self, _ticket: PathTicket) {}

    /// Whether a full path joins `from` and `to`. The default plans and answers `true` only for
    /// `Ready`; a request it queued is cancelled and answers `false`.
    fn is_reachable(&mut self, world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> bool {
        match self.plan(world, from, to, agent) {
            PathRequestOutcome::Ready(_) => true,
            PathRequestOutcome::Pending(ticket) => {
                self.cancel(ticket);
                false
            }
            _ => false,
        }
    }

    /// Called once per fixed step before any tree ticks ([`crate::AiPlugin`]'s behavior-tree pass):
    /// a budgeted provider refills its budget and serves its queue here. The default does nothing.
    fn begin_tick(&mut self) {}

    /// Whether [`MoveTo`] hands the path to the [`NavCrowd`] for an agent carrying [`CrowdSteered`]
    /// (the navmesh provider) rather than following it itself.
    fn steers_with_crowd(&self) -> bool {
        false
    }
}

/// The path providers by name. [`crate::AiPlugin`] inserts it with a [`NavMeshProvider`] named
/// [`NAVMESH_PROVIDER`]; a game inserts its deterministic providers.
#[derive(Resource, Default)]
pub struct PathProviders {
    providers: BTreeMap<String, Box<dyn PathProvider>>,
}

impl PathProviders {
    /// The set [`crate::AiPlugin`] installs: a [`NavMeshProvider`] named `navmesh`.
    pub fn with_defaults() -> Self {
        let mut providers = Self::default();
        providers.insert(NAVMESH_PROVIDER, NavMeshProvider);
        providers
    }

    /// Adds (or replaces) the provider called `name`; returns the one it replaced.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        provider: impl PathProvider,
    ) -> Option<Box<dyn PathProvider>> {
        self.providers.insert(name.into(), Box::new(provider))
    }

    /// Removes the provider called `name`.
    pub fn remove(&mut self, name: &str) -> Option<Box<dyn PathProvider>> {
        self.providers.remove(name)
    }

    /// Whether a provider is called `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// The provider called `name`.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut (dyn PathProvider + 'static)> {
        self.providers.get_mut(name).map(|p| &mut **p)
    }

    /// The provider names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    /// [`PathProvider::begin_tick`] on every provider, in name order.
    pub fn begin_tick(&mut self) {
        for provider in self.providers.values_mut() {
            provider.begin_tick();
        }
    }
}

/// Runs `f` on the provider called `name` with the rest of the world readable. `None` when there
/// is no [`PathProviders`] resource or no provider of that name.
pub fn with_provider<R>(
    world: &mut World,
    name: &str,
    f: impl FnOnce(&World, &mut dyn PathProvider) -> R,
) -> Option<R> {
    world
        .try_resource_scope(|world, mut providers: Mut<PathProviders>| {
            providers.get_mut(name).map(|provider| f(world, provider))
        })
        .flatten()
}

/// The per-tick refill of [`crate::AiPlugin`]: [`PathProviders::begin_tick`] when the resource exists.
pub(crate) fn begin_path_tick(world: &mut World) {
    if let Some(mut providers) = world.get_resource_mut::<PathProviders>() {
        providers.begin_tick();
    }
}

fn finite(v: Vec3) -> bool {
    v.x.is_finite() && v.y.is_finite() && v.z.is_finite()
}

// ------------------------------------------------------------------------------------- navmesh

/// The f32 navmesh: [`NavQuery`] over the [`NavMesh`] resource (and the [`NavObstacles`] resource
/// when there is one). Endpoints snap within [`PathAgentSpec::extents`]. A plan on the same binary
/// from the same mesh is repeatable; it is not a lockstep stack (see the choosing-navigation page).
#[derive(Debug, Clone, Copy, Default)]
pub struct NavMeshProvider;

impl PathProvider for NavMeshProvider {
    fn kind(&self) -> &'static str {
        NAVMESH_PROVIDER
    }

    fn plan(&mut self, world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> PathRequestOutcome {
        if !finite(from) || !finite(to) || !finite(agent.extents) {
            return PathRequestOutcome::Failed(PathFailure::InvalidInput);
        }
        let Some(mesh) = world.get_resource::<NavMesh>() else {
            return PathRequestOutcome::Failed(PathFailure::NoNavData);
        };
        let query = NavQuery {
            mesh,
            obstacles: world.get_resource::<NavObstacles>(),
        };
        if query.find_nearest(from, agent.extents).is_none() {
            return PathRequestOutcome::Failed(PathFailure::StartOffNav);
        }
        if query.find_nearest(to, agent.extents).is_none() {
            return PathRequestOutcome::Failed(PathFailure::GoalOffNav);
        }
        match query.find_path(from, to, agent.extents) {
            Some(path) if !path.waypoints.is_empty() => PathRequestOutcome::Ready(path.waypoints),
            _ => PathRequestOutcome::Failed(PathFailure::Unreachable),
        }
    }

    fn steers_with_crowd(&self) -> bool {
        true
    }
}

// ------------------------------------------------------------------------------------ det grid

/// Counters of a [`DetGridProvider`] (tests and the progress page read them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetGridStats {
    /// Searches run.
    pub searches: u64,
    /// Requests queued past the budget.
    pub queued: u64,
    /// Queued requests served by [`PathProvider::begin_tick`].
    pub served_from_queue: u64,
    /// Work spent ([`regia_nav::det::search::Plan::work`] summed).
    pub work: u64,
}

#[derive(Debug, Clone, Copy)]
struct QueuedPlan {
    ticket: PathTicket,
    from: [f32; 2],
    to: [f32; 2],
    profile: Profile,
}

/// The deterministic grid: [`regia_nav::det::search::plan`] on a [`NavGrid`] the provider owns.
///
/// Positions convert at the boundary: a [`Vec3`] gives the grid its `(x, z)`, and each waypoint
/// comes back as `(x, surface_y(x, z), z)`. The agent's [`PathAgentSpec::profile`] picks the
/// movement profile. Without a budget every request is searched at once. With
/// [`Self::with_tick_budget`] a request is searched at once only while the tick's work budget is
/// positive and nothing waits before it; otherwise it queues as a ticket, and
/// [`PathProvider::begin_tick`] refills the budget and serves the queue in ticket order. Every
/// decision is integer or comes from the grid's own deterministic search, so the same requests in
/// the same order give the same answers on every peer.
#[derive(Debug)]
pub struct DetGridProvider {
    grid: NavGrid,
    scratch: Scratch,
    tick_budget: Option<i64>,
    budget_left: i64,
    next_ticket: u64,
    queue: VecDeque<QueuedPlan>,
    done: BTreeMap<PathTicket, PathRequestOutcome>,
    stats: DetGridStats,
}

impl DetGridProvider {
    /// A provider over `grid` with no budget.
    pub fn new(grid: NavGrid) -> Self {
        Self {
            grid,
            scratch: Scratch::default(),
            tick_budget: None,
            budget_left: 0,
            next_ticket: 0,
            queue: VecDeque::new(),
            done: BTreeMap::new(),
            stats: DetGridStats::default(),
        }
    }

    /// Caps the search work per tick at `work` (in [`regia_nav::det::search::Plan::work`] units; the
    /// grid's default is [`regia_nav::det::search::path_tick_budget_for`]). A search that starts with
    /// budget left runs to the end, so one tick may overspend by one search.
    pub fn with_tick_budget(mut self, work: i64) -> Self {
        self.tick_budget = Some(work);
        self.budget_left = work;
        self
    }

    /// [`Self::with_tick_budget`] at the grid's own default budget.
    pub fn with_default_tick_budget(self) -> Self {
        let work = search::path_tick_budget_for(self.grid.width(), self.grid.height());
        self.with_tick_budget(work)
    }

    /// The grid.
    pub fn grid(&self) -> &NavGrid {
        &self.grid
    }

    /// The grid, to stamp obstacles or set exclusions; answers already queued are not re-planned.
    pub fn grid_mut(&mut self) -> &mut NavGrid {
        &mut self.grid
    }

    /// Requests waiting in the queue.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The counters.
    pub fn stats(&self) -> DetGridStats {
        self.stats
    }

    fn to_world(&self, p: [f32; 2]) -> Vec3 {
        Vec3::new(p[0], self.grid.surface_y(p[0], p[1]), p[1])
    }

    fn search(&mut self, from: [f32; 2], to: [f32; 2], profile: Profile) -> PathRequestOutcome {
        self.stats.searches += 1;
        let Some(plan) = search::plan(&self.grid, &mut self.scratch, profile, from, to) else {
            return PathRequestOutcome::Failed(PathFailure::Unreachable);
        };
        self.stats.work += u64::from(plan.work);
        self.budget_left -= i64::from(plan.work).max(1);
        let points: Vec<Vec3> = plan.waypoints.iter().map(|p| self.to_world(*p)).collect();
        if points.is_empty() {
            return PathRequestOutcome::Failed(PathFailure::Unreachable);
        }
        match plan.kind {
            PlanKind::Partial => PathRequestOutcome::Partial {
                points,
                reason: PartialReason::GoalUnreachable,
            },
            PlanKind::Full | PlanKind::Coarse if plan.snapped => PathRequestOutcome::Partial {
                points,
                reason: PartialReason::GoalSnapped,
            },
            PlanKind::Full | PlanKind::Coarse => PathRequestOutcome::Ready(points),
        }
    }

    /// Checks a request; the grid's `(x, z)` endpoints and profile when it can be planned.
    fn check(
        &self,
        from: Vec3,
        to: Vec3,
        agent: &PathAgentSpec,
    ) -> Result<([f32; 2], [f32; 2], Profile), PathFailure> {
        if !finite(from) || !finite(to) {
            return Err(PathFailure::InvalidInput);
        }
        if usize::from(agent.profile) >= self.grid.settings().profiles.len() {
            return Err(PathFailure::InvalidInput);
        }
        if !self.grid.in_bounds_world(from.x, from.z) {
            return Err(PathFailure::StartOffNav);
        }
        if !self.grid.in_bounds_world(to.x, to.z) {
            return Err(PathFailure::GoalOffNav);
        }
        Ok(([from.x, from.z], [to.x, to.z], Profile(agent.profile)))
    }
}

impl PathProvider for DetGridProvider {
    fn kind(&self) -> &'static str {
        "det_grid"
    }

    fn plan(&mut self, _world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> PathRequestOutcome {
        let (from, to, profile) = match self.check(from, to, agent) {
            Ok(request) => request,
            Err(failure) => return PathRequestOutcome::Failed(failure),
        };
        let _span = tracing::trace_span!("det_grid_plan").entered();
        if self.tick_budget.is_some() && (!self.queue.is_empty() || self.budget_left <= 0) {
            let ticket = PathTicket(self.next_ticket);
            self.next_ticket += 1;
            self.queue.push_back(QueuedPlan {
                ticket,
                from,
                to,
                profile,
            });
            self.stats.queued += 1;
            return PathRequestOutcome::Pending(ticket);
        }
        self.search(from, to, profile)
    }

    fn poll(&mut self, _world: &World, ticket: PathTicket) -> PathRequestOutcome {
        if let Some(done) = self.done.remove(&ticket) {
            return done;
        }
        if self.queue.iter().any(|q| q.ticket == ticket) {
            return PathRequestOutcome::Pending(ticket);
        }
        PathRequestOutcome::Failed(PathFailure::UnknownTicket)
    }

    fn cancel(&mut self, ticket: PathTicket) {
        self.queue.retain(|q| q.ticket != ticket);
        self.done.remove(&ticket);
    }

    /// One search outside the budget: `true` for a full (or coarse) route to the goal itself.
    fn is_reachable(&mut self, _world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> bool {
        let Ok((from, to, profile)) = self.check(from, to, agent) else {
            return false;
        };
        let left = self.budget_left;
        let reachable = matches!(self.search(from, to, profile), PathRequestOutcome::Ready(_));
        self.budget_left = left;
        reachable
    }

    fn begin_tick(&mut self) {
        let Some(budget) = self.tick_budget else {
            return;
        };
        self.budget_left = budget;
        while self.budget_left > 0 {
            let Some(next) = self.queue.pop_front() else {
                break;
            };
            let answer = self.search(next.from, next.to, next.profile);
            self.stats.served_from_queue += 1;
            self.done.insert(next.ticket, answer);
        }
    }
}

// --------------------------------------------------------------------------------- det navmesh

/// The deterministic navmesh: [`regia_nav::det::navmesh::NavMesh::find_path_scaled`] on a mesh the
/// provider shares (`Arc`, so a host keeps its own handle). The agent spec becomes a
/// [`MeshAgent`] (radius, height, steps); positions pass through unchanged as `[x, y, z]`.
#[derive(Debug, Clone)]
pub struct DetNavMeshProvider {
    mesh: Arc<DetNavMesh>,
    tunnel_pct: u32,
}

impl DetNavMeshProvider {
    /// A provider over `mesh`, tunnels costing as much as open ground.
    pub fn new(mesh: impl Into<Arc<DetNavMesh>>) -> Self {
        Self {
            mesh: mesh.into(),
            tunnel_pct: 100,
        }
    }

    /// A tunnel meter costs `pct` percent of a meter of open ground (clamped to 10-100 by the search).
    pub fn with_tunnel_pct(mut self, pct: u32) -> Self {
        self.tunnel_pct = pct;
        self
    }

    /// The mesh.
    pub fn mesh(&self) -> &Arc<DetNavMesh> {
        &self.mesh
    }
}

fn mesh_agent(agent: &PathAgentSpec) -> MeshAgent {
    MeshAgent {
        radius: agent.radius,
        height: agent.height,
        step_up: agent.step_up,
        step_down: agent.step_down,
    }
}

impl PathProvider for DetNavMeshProvider {
    fn kind(&self) -> &'static str {
        "det_navmesh"
    }

    fn plan(&mut self, _world: &World, from: Vec3, to: Vec3, agent: &PathAgentSpec) -> PathRequestOutcome {
        if !finite(from) || !finite(to) {
            return PathRequestOutcome::Failed(PathFailure::InvalidInput);
        }
        let _span = tracing::trace_span!("det_navmesh_plan").entered();
        let body = mesh_agent(agent);
        let (a, b) = ([from.x, from.y, from.z], [to.x, to.y, to.z]);
        if let Some(plan) = self.mesh.find_path_scaled(a, b, &body, self.tunnel_pct) {
            let points: Vec<Vec3> = plan
                .waypoints
                .iter()
                .map(|p| Vec3::new(p[0], p[1], p[2]))
                .collect();
            if !points.is_empty() {
                return PathRequestOutcome::Ready(points);
            }
        }
        let located = |p: [f32; 3]| {
            self.mesh
                .locate(p, &body)
                .or_else(|| self.mesh.locate_near(p[0], p[2], &body))
                .is_some()
        };
        if !located(a) {
            PathRequestOutcome::Failed(PathFailure::StartOffNav)
        } else if !located(b) {
            PathRequestOutcome::Failed(PathFailure::GoalOffNav)
        } else {
            PathRequestOutcome::Failed(PathFailure::Unreachable)
        }
    }
}

// --------------------------------------------------------------------------------- the leaf

/// Ties an agent entity to the [`NavCrowd`] agent that walks it: a [`MoveTo`] on a provider that
/// [`PathProvider::steers_with_crowd`] sets that agent's target and copies its position onto the
/// entity each tick, instead of following the path itself.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrowdSteered(pub CrowdAgentId);

/// Where a [`MoveTo`] goes.
#[derive(Debug, Clone, PartialEq)]
pub enum MoveTarget {
    /// A blackboard key holding a vec3, or an entity whose position is the target.
    Key(String),
    /// A fixed point.
    Point(Vec3),
}

/// Where a [`MoveTo`] run is.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MoveToState {
    /// Not started (or reset).
    #[default]
    Idle,
    /// Waiting for a queued plan.
    Pending,
    /// Following its path.
    Following,
    /// The crowd walks it.
    Crowd,
    /// Inside the acceptance radius.
    Arrived,
    /// Failed; `<key>_error` names why.
    Failed,
}

impl MoveToState {
    /// The lowercase name the blackboard and `ai.bt_live` use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Following => "following",
            Self::Crowd => "crowd",
            Self::Arrived => "arrived",
            Self::Failed => "failed",
        }
    }
}

/// One [`MoveTo`]'s latest progress (an entry of [`MoveToStatus`]).
#[derive(Reflect, Debug, Clone, Default, PartialEq)]
pub struct MoveToReport {
    /// The leaf's progress key.
    pub key: String,
    /// The provider name it plans through.
    pub provider: String,
    /// Where the run is.
    pub state: MoveToState,
    /// Whether the crowd walks it.
    pub crowd: bool,
    /// The target it planned for.
    pub target: Vec3,
    /// Distance left along the path (m; straight to the target while the crowd walks).
    pub remaining: f32,
    /// The next waypoint's index.
    pub waypoint: u32,
    /// Waypoints in the path.
    pub waypoints: u32,
    /// Re-plans since the run started.
    pub replans: u32,
    /// Ticks since the run started.
    pub ticks: u64,
    /// The failure code, while failed.
    pub error: Option<String>,
}

/// The [`MoveTo`] runs of an agent, by progress key (sorted): written by the leaf each tick it
/// runs, read by `ai.bt_live`. Never read back by the simulation.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct MoveToStatus {
    /// One entry per progress key, sorted by key.
    pub entries: Vec<MoveToReport>,
}

impl MoveToStatus {
    /// The entry for `key`.
    pub fn entry(&self, key: &str) -> Option<&MoveToReport> {
        self.entries.iter().find(|e| e.key == key)
    }

    fn entry_mut(&mut self, key: &str) -> &mut MoveToReport {
        let at = match self.entries.binary_search_by(|e| e.key.as_str().cmp(key)) {
            Ok(i) => i,
            Err(i) => {
                self.entries.insert(
                    i,
                    MoveToReport {
                        key: key.to_string(),
                        ..MoveToReport::default()
                    },
                );
                i
            }
        };
        &mut self.entries[at]
    }
}

#[derive(Debug, Clone, Default)]
enum Phase {
    #[default]
    Idle,
    Pending {
        ticket: PathTicket,
        provider: String,
        goal: Vec3,
        crowd: bool,
    },
    Following {
        points: Vec<Vec3>,
        next: usize,
        goal: Vec3,
    },
    Crowd {
        id: CrowdAgentId,
        goal: Vec3,
    },
}

impl Phase {
    fn goal(&self) -> Option<Vec3> {
        match self {
            Phase::Idle => None,
            Phase::Pending { goal, .. } | Phase::Following { goal, .. } | Phase::Crowd { goal, .. } => {
                Some(*goal)
            }
        }
    }
}

/// Blackboard keys derived from the progress key, built once per key (no allocation per tick).
#[derive(Debug, Clone, Default)]
struct Keys {
    base: String,
    state: String,
    remaining: String,
    replans: String,
    error: String,
}

impl Keys {
    fn of(base: &str) -> Self {
        Self {
            base: base.to_string(),
            state: format!("{base}_state"),
            remaining: format!("{base}_remaining"),
            replans: format!("{base}_replans"),
            error: format!("{base}_error"),
        }
    }
}

/// A leaf (and state-tree task) that moves its agent to a target through a named [`PathProvider`].
///
/// Each tick: the target is resolved ([`MoveTarget`]); inside `acceptance_radius` (across the ground,
/// with the vertical gap within the agent's height) the run succeeds.
/// Otherwise, with no plan yet (or the target moved more than `replan_distance` from where it was
/// planned, which counts a re-plan) it plans through the provider: `Ready` starts following,
/// `Pending` waits and polls, `Failed` fails with the failure's code, and `Partial` follows only when
/// its end lies inside the radius or `allow_partial` is set (else it fails with `partial_path`). A
/// path is followed at `speed` m/s by the tick's `dt`, waypoint by waypoint; a path that ends outside
/// the radius fails with `path_ended`. On a provider that steers with the crowd, an agent carrying
/// [`CrowdSteered`] gets the target as its crowd target instead (`crowd_refused` when the crowd
/// refuses it, `crowd_agent_missing` when the agent is gone, `unreachable` when the crowd finds no
/// path). `no_target`, `no_position` and `unknown_provider` fail before planning.
///
/// Register a configured leaf with [`MoveTo::register`]; [`crate::AiPlugin`] registers the
/// blackboard-driven one as [`MOVE_TO_TASK`].
#[derive(Debug, Clone)]
pub struct MoveTo {
    /// Where to go.
    pub target: MoveTarget,
    /// Success distance from the target across the ground (m, XZ); the vertical gap must also be
    /// within `agent.height`.
    pub acceptance_radius: f32,
    /// The provider name in [`PathProviders`].
    pub provider: String,
    /// A blackboard string key that overrides `provider` when it holds a string.
    pub provider_key: Option<String>,
    /// A blackboard float key that overrides `acceptance_radius` when it holds a float.
    pub radius_key: Option<String>,
    /// Follow speed (m/s) when the leaf follows the path itself.
    pub speed: f32,
    /// Re-plan when the target is farther than this from where it was planned (m).
    pub replan_distance: f32,
    /// Follow a partial path even when it ends outside the radius (it then fails with
    /// `path_ended` at its end).
    pub allow_partial: bool,
    /// The body handed to the provider.
    pub agent: PathAgentSpec,
    /// The blackboard key prefix and [`MoveToStatus`] key of this leaf's progress.
    pub progress_key: String,
    phase: Phase,
    replans: u32,
    ticks: u64,
    keys: Keys,
    /// The provider name of the current run (reused buffer).
    run_provider: String,
}

impl Default for MoveTo {
    fn default() -> Self {
        Self::new(MoveTarget::Key("move_target".into()), 0.5)
    }
}

impl MoveTo {
    /// A leaf to `target` that succeeds within `acceptance_radius`, on the `navmesh` provider at
    /// 3.5 m/s, re-planning past 1 m, progress under `move_to`.
    pub fn new(target: MoveTarget, acceptance_radius: f32) -> Self {
        Self {
            target,
            acceptance_radius,
            provider: NAVMESH_PROVIDER.to_string(),
            provider_key: None,
            radius_key: None,
            speed: 3.5,
            replan_distance: 1.0,
            allow_partial: false,
            agent: PathAgentSpec::default(),
            progress_key: "move_to".to_string(),
            phase: Phase::Idle,
            replans: 0,
            ticks: 0,
            keys: Keys::default(),
            run_provider: String::new(),
        }
    }

    /// The leaf [`MOVE_TO_TASK`] names: target `move_target`, radius `move_acceptance_radius` (0.5 m
    /// without one), provider `move_provider` (`navmesh` without one).
    pub fn from_blackboard() -> Self {
        let mut leaf = Self::new(MoveTarget::Key("move_target".into()), 0.5);
        leaf.provider_key = Some("move_provider".into());
        leaf.radius_key = Some("move_acceptance_radius".into());
        leaf
    }

    /// Plans through the provider called `name`.
    pub fn with_provider(mut self, name: impl Into<String>) -> Self {
        self.provider = name.into();
        self
    }

    /// Follows at `speed` m/s.
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    /// Re-plans when the target moves more than `distance` m.
    pub fn with_replan_distance(mut self, distance: f32) -> Self {
        self.replan_distance = distance;
        self
    }

    /// Follows partial paths.
    pub fn with_allow_partial(mut self, allow: bool) -> Self {
        self.allow_partial = allow;
        self
    }

    /// Plans for `agent`.
    pub fn with_agent(mut self, agent: PathAgentSpec) -> Self {
        self.agent = agent;
        self
    }

    /// Writes progress under `key`.
    pub fn with_progress_key(mut self, key: impl Into<String>) -> Self {
        self.progress_key = key.into();
        self
    }

    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf (or a
    /// state's task) uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }

    fn resolve_target(&self, ctx: &BtContext<'_>) -> Option<Vec3> {
        let point = match &self.target {
            MoveTarget::Point(p) => *p,
            MoveTarget::Key(key) => match ctx.blackboard()?.get(key)? {
                BbValue::Vec3(v) => *v,
                BbValue::Entity(bits) => {
                    let entity = Entity::try_from_bits(*bits)?;
                    if let Some(global) = ctx.world.get::<regia_scene::GlobalTransform>(entity) {
                        global.translation()
                    } else {
                        ctx.world.get::<regia_scene::Transform>(entity)?.translation
                    }
                }
                _ => return None,
            },
        };
        finite(point).then_some(point)
    }

    /// Puts the provider name of this run (the blackboard override, else `provider`) into
    /// `run_provider`, reusing its buffer.
    fn refresh_provider(&mut self, ctx: &BtContext<'_>) {
        if let Some(key) = &self.provider_key
            && let Some(BbValue::String(name)) = ctx.blackboard().and_then(|b| b.get(key))
        {
            if self.run_provider != *name {
                self.run_provider.clone_from(name);
            }
            return;
        }
        if self.run_provider != self.provider {
            self.run_provider.clone_from(&self.provider);
        }
    }

    fn radius(&self, ctx: &BtContext<'_>) -> f32 {
        if let Some(key) = &self.radius_key
            && let Some(BbValue::Float(r)) = ctx.blackboard().and_then(|b| b.get(key))
            && r.is_finite()
        {
            return r.max(0.0);
        }
        if self.acceptance_radius.is_finite() {
            self.acceptance_radius.max(0.0)
        } else {
            0.0
        }
    }

    /// Writes the progress to the blackboard and the [`MoveToStatus`] component.
    fn report(&mut self, ctx: &mut BtContext<'_>, state: MoveToState, target: Vec3, error: Option<&str>) {
        if self.keys.base != self.progress_key {
            self.keys = Keys::of(&self.progress_key);
        }
        let (remaining, waypoint, waypoints, crowd) = match &self.phase {
            Phase::Following { points, next, .. } => {
                let pos = ctx.position().unwrap_or(target);
                (
                    path_remaining(pos, points, *next),
                    *next as u32,
                    points.len() as u32,
                    false,
                )
            }
            Phase::Crowd { .. } => (ctx.position().map_or(0.0, |p| distance(p, target)), 0, 0, true),
            Phase::Pending { crowd, .. } => (0.0, 0, 0, *crowd),
            Phase::Idle => (ctx.position().map_or(0.0, |p| distance(p, target)), 0, 0, false),
        };
        let replans = self.replans;
        let ticks = self.ticks;
        if let Some(mut board) = ctx.blackboard_mut() {
            assign_str(&mut board, &self.keys.state, state.name());
            board.assign(&self.keys.remaining, &BbValue::Float(remaining));
            board.assign(&self.keys.replans, &BbValue::Int(i64::from(replans)));
            match error {
                Some(code) => assign_str(&mut board, &self.keys.error, code),
                None => {
                    if board.get(&self.keys.error).is_some() {
                        board.remove(&self.keys.error);
                    }
                }
            }
        }
        let entity = ctx.entity;
        let Ok(mut agent) = ctx.world.get_entity_mut(entity) else {
            return;
        };
        if !agent.contains::<MoveToStatus>() {
            agent.insert(MoveToStatus::default());
        }
        let Some(mut status) = agent.get_mut::<MoveToStatus>() else {
            return;
        };
        let entry = status.entry_mut(&self.progress_key);
        if entry.provider != self.run_provider {
            entry.provider.clone_from(&self.run_provider);
        }
        entry.state = state;
        entry.crowd = crowd || (state == MoveToState::Arrived && entry.crowd);
        entry.target = target;
        entry.remaining = remaining;
        entry.waypoint = waypoint;
        entry.waypoints = waypoints;
        entry.replans = replans;
        entry.ticks = ticks;
        match error {
            Some(code) if entry.error.as_deref() != Some(code) => entry.error = Some(code.to_string()),
            Some(_) => {}
            None => entry.error = None,
        }
    }

    fn fail(&mut self, ctx: &mut BtContext<'_>, target: Vec3, code: &str) -> BtStatus {
        self.stop(ctx);
        self.report(ctx, MoveToState::Failed, target, Some(code));
        self.end_run();
        BtStatus::Failure
    }

    /// A finished run: the next tick starts a fresh one (counters from zero).
    fn end_run(&mut self) {
        self.replans = 0;
        self.ticks = 0;
    }

    fn arrive(&mut self, ctx: &mut BtContext<'_>, target: Vec3) -> BtStatus {
        let was_crowd = matches!(self.phase, Phase::Crowd { .. });
        self.stop(ctx);
        self.report(ctx, MoveToState::Arrived, target, None);
        if was_crowd && let Some(mut status) = ctx.world.get_mut::<MoveToStatus>(ctx.entity) {
            status.entry_mut(&self.progress_key).crowd = true;
        }
        self.end_run();
        BtStatus::Success
    }

    /// Drops whatever the run holds: a queued ticket, a crowd target.
    fn stop(&mut self, ctx: &mut BtContext<'_>) {
        match std::mem::take(&mut self.phase) {
            Phase::Pending { ticket, provider, .. } => {
                let _ = with_provider(ctx.world, &provider, |_, p| p.cancel(ticket));
            }
            Phase::Crowd { id, .. } => {
                if let Some(mut crowd) = ctx.world.get_resource_mut::<NavCrowd>() {
                    let _ = crowd.clear_target(id);
                }
            }
            Phase::Idle | Phase::Following { .. } => {}
        }
    }

    /// Takes a plan answer: starts following (or the crowd), waits, or fails.
    fn accept(
        &mut self,
        ctx: &mut BtContext<'_>,
        outcome: PathRequestOutcome,
        provider: String,
        crowd: bool,
        target: Vec3,
        radius: f32,
    ) -> Result<(), &'static str> {
        let points = match outcome {
            PathRequestOutcome::Ready(points) => points,
            PathRequestOutcome::Partial { points, .. }
                if self.allow_partial
                    || points
                        .last()
                        .is_some_and(|end| arrived(*end, target, radius, self.agent.height)) =>
            {
                points
            }
            PathRequestOutcome::Partial { .. } => return Err("partial_path"),
            PathRequestOutcome::Failed(failure) => return Err(failure.code()),
            PathRequestOutcome::Pending(ticket) => {
                self.phase = Phase::Pending {
                    ticket,
                    provider,
                    goal: target,
                    crowd,
                };
                return Ok(());
            }
        };
        let steered = if crowd {
            ctx.world.get::<CrowdSteered>(ctx.entity).map(|c| c.0)
        } else {
            None
        };
        if let Some(id) = steered {
            let set = ctx
                .world
                .try_resource_scope(|world, mut crowd: Mut<NavCrowd>| {
                    world
                        .get_resource::<NavMesh>()
                        .map(|mesh| crowd.set_target(mesh, id, target))
                })
                .flatten();
            return match set {
                Some(Ok(())) => {
                    self.phase = Phase::Crowd { id, goal: target };
                    Ok(())
                }
                _ => Err("crowd_refused"),
            };
        }
        self.phase = Phase::Following {
            next: 1.min(points.len()),
            points,
            goal: target,
        };
        Ok(())
    }

    fn start(
        &mut self,
        ctx: &mut BtContext<'_>,
        from: Vec3,
        target: Vec3,
        radius: f32,
    ) -> Result<(), &'static str> {
        self.refresh_provider(ctx);
        let provider = self.run_provider.clone();
        let agent = self.agent;
        let answer = with_provider(ctx.world, &provider, |world, p| {
            (p.plan(world, from, target, &agent), p.steers_with_crowd())
        });
        let Some((outcome, crowd)) = answer else {
            return Err("unknown_provider");
        };
        self.accept(ctx, outcome, provider, crowd, target, radius)
    }
}

/// Writes `text` under `key` unless it is already there (no allocation for an unchanged string).
fn assign_str(board: &mut Blackboard, key: &str, text: &str) {
    if let Some(BbValue::String(old)) = board.get(key)
        && old == text
    {
        return;
    }
    board.assign(key, &BbValue::String(text.to_string()));
}

/// Whether `pos` is inside the acceptance radius of `target`: within `radius` across the ground
/// (XZ, as the crowd measures arrival) and within the body `height` vertically (another floor of a
/// building is not "there").
fn arrived(pos: Vec3, target: Vec3, radius: f32, height: f32) -> bool {
    let (dx, dz) = (pos.x - target.x, pos.z - target.z);
    let dy = pos.y - target.y;
    let height = if height.is_finite() { height.max(0.0) } else { 0.0 };
    (dx * dx + dz * dz).sqrt() <= radius && dy * dy <= height * height
}

fn distance(a: Vec3, b: Vec3) -> f32 {
    let (dx, dy, dz) = (a.x - b.x, a.y - b.y, a.z - b.z);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn path_remaining(pos: Vec3, points: &[Vec3], next: usize) -> f32 {
    let Some(first) = points.get(next) else {
        return 0.0;
    };
    let mut total = distance(pos, *first);
    for pair in points[next..].windows(2) {
        total += distance(pair[0], pair[1]);
    }
    total
}

/// Moves `pos` up to `step` meters along `points` from `next`; returns the new position and next
/// index. IEEE `+ - * /` and `sqrt` only, in a fixed order.
fn follow(mut pos: Vec3, points: &[Vec3], mut next: usize, mut step: f32) -> (Vec3, usize) {
    while step > 0.0 {
        let Some(wp) = points.get(next).copied() else {
            break;
        };
        let d = distance(pos, wp);
        if d <= step {
            pos = wp;
            step -= d;
            next += 1;
        } else {
            let t = step / d;
            pos = Vec3::new(
                pos.x + (wp.x - pos.x) * t,
                pos.y + (wp.y - pos.y) * t,
                pos.z + (wp.z - pos.z) * t,
            );
            step = 0.0;
        }
    }
    (pos, next)
}

impl BtTask for MoveTo {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let _span = tracing::trace_span!("move_to").entered();
        self.ticks += 1;
        let Some(target) = self.resolve_target(ctx) else {
            let at = ctx.position().unwrap_or(Vec3::ZERO);
            return self.fail(ctx, at, "no_target");
        };
        let Some(mut pos) = ctx.position() else {
            return self.fail(ctx, target, "no_position");
        };
        let radius = self.radius(ctx);
        // The crowd moved the agent during its own step: take its position first.
        if let Phase::Crowd { id, .. } = self.phase {
            let state = ctx
                .world
                .get_resource::<NavCrowd>()
                .and_then(|crowd| crowd.agent(id).map(|a| (a.position(), a.state())));
            let Some((at, state)) = state else {
                return self.fail(ctx, target, "crowd_agent_missing");
            };
            ctx.set_position(at);
            pos = at;
            if !arrived(pos, target, radius, self.agent.height) {
                match state {
                    CrowdAgentState::NoPath => return self.fail(ctx, target, "unreachable"),
                    // The crowd's goal is the target snapped onto the mesh; arriving there outside
                    // the radius is the end of the path.
                    CrowdAgentState::Arrived => return self.fail(ctx, target, "path_ended"),
                    _ => {}
                }
            }
        }
        if arrived(pos, target, radius, self.agent.height) {
            return self.arrive(ctx, target);
        }
        if let Some(goal) = self.phase.goal()
            && distance(goal, target) > self.replan_distance
        {
            self.stop(ctx);
            self.replans += 1;
        }
        if matches!(self.phase, Phase::Idle)
            && let Err(code) = self.start(ctx, pos, target, radius)
        {
            return self.fail(ctx, target, code);
        }
        if let Phase::Pending {
            ticket,
            ref provider,
            crowd,
            ..
        } = self.phase
        {
            let provider = provider.clone();
            let Some(outcome) = with_provider(ctx.world, &provider, |world, p| p.poll(world, ticket)) else {
                return self.fail(ctx, target, "unknown_provider");
            };
            if outcome == PathRequestOutcome::Pending(ticket) {
                self.report(ctx, MoveToState::Pending, target, None);
                return BtStatus::Running;
            }
            self.phase = Phase::Idle;
            if let Err(code) = self.accept(ctx, outcome, provider, crowd, target, radius) {
                return self.fail(ctx, target, code);
            }
        }
        match &mut self.phase {
            Phase::Following { points, next, .. } => {
                let speed = if self.speed.is_finite() {
                    self.speed.max(0.0)
                } else {
                    0.0
                };
                let step = speed * ctx.dt;
                let (at, to) = follow(pos, points, *next, step);
                *next = to;
                let ended = to >= points.len();
                ctx.set_position(at);
                if arrived(at, target, radius, self.agent.height) {
                    return self.arrive(ctx, target);
                }
                if ended {
                    return self.fail(ctx, target, "path_ended");
                }
                self.report(ctx, MoveToState::Following, target, None);
            }
            Phase::Crowd { .. } => self.report(ctx, MoveToState::Crowd, target, None),
            Phase::Pending { .. } => self.report(ctx, MoveToState::Pending, target, None),
            Phase::Idle => return self.fail(ctx, target, "unknown_provider"),
        }
        BtStatus::Running
    }

    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        self.stop(ctx);
        self.end_run();
    }
}

/// Registers the blackboard-driven leaf under [`MOVE_TO_TASK`] (what [`crate::AiPlugin`] does).
pub fn register_move_to(registry: &mut BtTaskRegistry) {
    MoveTo::from_blackboard().register(registry, MOVE_TO_TASK);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_crosses_waypoints_inside_one_step() {
        let points = [Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 1.0)];
        let (at, next) = follow(Vec3::ZERO, &points, 1, 1.5);
        assert_eq!(next, 2);
        assert_eq!(at, Vec3::new(1.0, 0.0, 0.5));
        let (at, next) = follow(at, &points, next, 10.0);
        assert_eq!((at, next), (Vec3::new(1.0, 0.0, 1.0), 3));
        assert_eq!(path_remaining(Vec3::ZERO, &points, 1), 2.0);
    }

    #[test]
    fn codes_are_lowercase_protocol_names() {
        let failures = [
            PathFailure::NoNavData,
            PathFailure::StartOffNav,
            PathFailure::GoalOffNav,
            PathFailure::Unreachable,
            PathFailure::InvalidInput,
            PathFailure::UnknownTicket,
        ];
        let partials = [PartialReason::GoalUnreachable, PartialReason::GoalSnapped];
        let codes = failures
            .iter()
            .map(|f| f.code())
            .chain(partials.iter().map(|p| p.code()));
        for code in codes {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn the_provider_set_is_sorted_and_replaceable() {
        let mut providers = PathProviders::with_defaults();
        assert!(providers.contains(NAVMESH_PROVIDER));
        assert!(providers.insert(NAVMESH_PROVIDER, NavMeshProvider).is_some());
        assert_eq!(providers.names().collect::<Vec<_>>(), vec![NAVMESH_PROVIDER]);
        assert!(providers.remove(NAVMESH_PROVIDER).is_some());
        assert!(providers.get_mut(NAVMESH_PROVIDER).is_none());
    }
}
