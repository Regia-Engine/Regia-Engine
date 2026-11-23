// SPDX-License-Identifier: MPL-2.0
//! A deterministic path-request queue: units ask for routes, the queue serves them in
//! `(submit tick, unit id)` order under a per-tick work budget and carries the rest. Original code
//! (ADR-8295); the shared abstract route follows the hierarchical path-finding idea of Botea,
//! Müller and Schaeffer, "Near Optimal Hierarchical Path-Finding" (Journal of Game Development,
//! 2004).
//!
//! A tick where a thousand units ask for paths does not run a thousand searches at once: the queue
//! spends at most its budget (in [`crate::search::Plan::work`], S-02's unit: cell expansions
//! plus a share for the coarse search) and leaves the rest for the next ticks. Requests that share
//! a start region, a goal region and a profile while the grid's generation is unchanged reuse one
//! coarse route, within a tick and across ticks. Every
//! peer that submits the same requests and serves on the same ticks gets the same results.
//!
//! ```
//! use std::sync::Arc;
//! use regia_nav_det::grid::{GridSettings, NavGrid, NavStatic, Profile};
//! use regia_nav_det::requests::{PathRequests, Poll, RequestSettings};
//! use regia_nav_det::search::{PlanKind, Scratch};
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! let grid = NavGrid::new(Arc::new(NavStatic::build(None, Arc::new(GridSettings::default()))?))?;
//! let mut queue = PathRequests::new(RequestSettings::for_grid(&grid));
//! let mut scratch = Scratch::default();
//! let ticket = queue.request(7, [-100.0, 0.0], [100.0, 20.0], Profile::HOVER);
//! queue.serve(&grid, &mut scratch);
//! match queue.poll(ticket) {
//!     Poll::Ready(result) => assert_eq!(result.kind(), Some(PlanKind::Full)),
//!     other => panic!("served in one tick: {other:?}"),
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::grid::{NavGrid, Profile};
use super::layer::TUNNEL_COST_PCT;
use super::region_graph::RegionGraph;
use super::search::{self, Plan, PlanArgs, PlanKind, RouteCache, Scratch};

/// Work charged for a request that found no free cell near its start or goal (no plan): the
/// plan's fixed overhead.
pub const NO_PLAN_WORK: i64 = 16;

/// Most coarse routes the queue keeps; a full cache is emptied before the next request.
pub const MAX_CACHED_ROUTES: usize = 4096;

/// A request's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ticket(pub u64);

/// How a [`PathRequests`] queue spends its ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSettings {
    /// Work one [`PathRequests::serve`] may spend, in [`Plan::work`] units. `None`: serve every
    /// pending request in one tick. A serve keeps going while any budget is left, so the last
    /// request of a tick may overrun it; a positive budget always serves at least one request.
    pub tick_budget: Option<i64>,
    /// Share one coarse route between requests with the same start region, goal region and
    /// profile while the grid's generation is unchanged (across serves too; a relabel or stamp
    /// empties the cache).
    pub share_routes: bool,
}

impl Default for RequestSettings {
    fn default() -> Self {
        RequestSettings {
            tick_budget: Some(search::PATH_TICK_BUDGET),
            share_routes: true,
        }
    }
}

impl RequestSettings {
    /// The budget S-02 sizes for the grid ([`search::path_tick_budget_for`]) with route sharing.
    pub fn for_grid(grid: &NavGrid) -> RequestSettings {
        RequestSettings {
            tick_budget: Some(search::path_tick_budget_for(grid.width(), grid.height())),
            share_routes: true,
        }
    }
}

/// A served request.
#[derive(Debug, Clone)]
pub struct PathResult {
    /// The request's ticket.
    pub ticket: Ticket,
    /// The unit that asked.
    pub unit: u32,
    /// The tick the request was submitted on.
    pub submit_tick: u64,
    /// The tick that served it.
    pub served_tick: u64,
    /// Serve order over the queue's life (0 is the first request ever served).
    pub seq: u64,
    /// The route, `None` when no free cell lies near the start or the goal
    /// ([`search::plan`]'s `None`).
    pub plan: Option<Plan>,
    /// The coarse route came from an earlier request at the same grid generation.
    pub shared_route: bool,
}

impl PathResult {
    /// How the plan ends ([`PlanKind::Full`], [`PlanKind::Partial`], [`PlanKind::Coarse`]), `None`
    /// without a plan.
    pub fn kind(&self) -> Option<PlanKind> {
        self.plan.as_ref().map(|p| p.kind)
    }

    /// Ticks the request waited (0 when served on its submit tick).
    pub fn waited(&self) -> u64 {
        self.served_tick - self.submit_tick
    }

    /// The work charged to the tick budget.
    pub fn work(&self) -> i64 {
        self.plan.as_ref().map_or(NO_PLAN_WORK, |p| i64::from(p.work))
    }
}

/// What [`PathRequests::poll`] knows about a ticket.
#[derive(Debug, Clone)]
pub enum Poll {
    /// Still queued since `submit_tick`.
    Pending {
        /// The tick it was submitted on.
        submit_tick: u64,
    },
    /// Served; the result is handed over once and the ticket is forgotten.
    Ready(PathResult),
    /// Never issued, cancelled, or already handed over.
    Unknown,
}

/// Running totals of a queue. Serializable for logs, tools and agents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestCounters {
    /// Requests submitted.
    pub requests: u64,
    /// Requests served.
    pub served: u64,
    /// Requests left queued at the end of a serve, summed over serves.
    pub carried: u64,
    /// Requests cancelled while queued.
    pub cancelled: u64,
    /// Served requests whose coarse route was shared.
    pub cache_hits: u64,
    /// Cell expansions of the fine searches.
    pub expansions: u64,
    /// Region expansions of the coarse searches.
    pub coarse_expansions: u64,
    /// Work charged to the budgets ([`PathResult::work`]).
    pub work: u64,
    /// Region-graph clusters recomputed before serving.
    pub graph_repairs: u64,
}

impl RequestCounters {
    fn add(&mut self, o: &RequestCounters) {
        self.requests += o.requests;
        self.served += o.served;
        self.carried += o.carried;
        self.cancelled += o.cancelled;
        self.cache_hits += o.cache_hits;
        self.expansions += o.expansions;
        self.coarse_expansions += o.coarse_expansions;
        self.work += o.work;
        self.graph_repairs += o.graph_repairs;
    }
}

/// One queued request.
#[derive(Debug, Clone, Copy)]
struct Queued {
    from: [f32; 2],
    to: [f32; 2],
    profile: Profile,
}

/// The queue. Sim state: pending requests, results and the route cache (which decides shared
/// routes). The region graph inside is rebuilt from the grid and carries no decisions of its own.
#[derive(Debug, Clone)]
pub struct PathRequests {
    settings: RequestSettings,
    /// The tick the next serve runs on (requests submitted now carry it).
    tick: u64,
    next_ticket: u64,
    /// `(submit tick, unit, ticket)` in serve order.
    pending: BTreeMap<(u64, u32, u64), Queued>,
    /// Ticket to its pending key.
    keys: BTreeMap<u64, (u64, u32)>,
    done: BTreeMap<u64, PathResult>,
    graph: RegionGraph,
    /// Coarse routes at `cache_gen`; emptied when the grid generation moves.
    cache: RouteCache,
    cache_gen: u64,
    totals: RequestCounters,
    last: RequestCounters,
}

impl PathRequests {
    /// An empty queue at tick 0.
    pub fn new(settings: RequestSettings) -> PathRequests {
        PathRequests {
            settings,
            tick: 0,
            next_ticket: 1,
            pending: BTreeMap::new(),
            keys: BTreeMap::new(),
            done: BTreeMap::new(),
            graph: RegionGraph::default(),
            cache: RouteCache::new(),
            cache_gen: 0,
            totals: RequestCounters::default(),
            last: RequestCounters::default(),
        }
    }

    /// The settings.
    pub fn settings(&self) -> &RequestSettings {
        &self.settings
    }

    /// Change the settings from the next serve on.
    pub fn set_settings(&mut self, settings: RequestSettings) {
        self.settings = settings;
    }

    /// The tick the next [`Self::serve`] runs on; requests submitted now carry it.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Queue a surface route for `unit` from `from` to `to` (world XZ). Requests of one tick are
    /// served in unit id order (then ticket order for one unit's several requests), after every
    /// request of earlier ticks.
    pub fn request(&mut self, unit_id: u32, from: [f32; 2], to: [f32; 2], profile: Profile) -> Ticket {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.pending
            .insert((self.tick, unit_id, ticket), Queued { from, to, profile });
        self.keys.insert(ticket, (self.tick, unit_id));
        self.totals.requests += 1;
        Ticket(ticket)
    }

    /// Drop a queued request or an unread result. True when the ticket was known.
    pub fn cancel(&mut self, ticket: Ticket) -> bool {
        if let Some((tick, unit)) = self.keys.remove(&ticket.0) {
            self.pending.remove(&(tick, unit, ticket.0));
            self.totals.cancelled += 1;
            return true;
        }
        self.done.remove(&ticket.0).is_some()
    }

    /// Where a ticket stands. A served result is handed over once.
    pub fn poll(&mut self, ticket: Ticket) -> Poll {
        if let Some(result) = self.done.remove(&ticket.0) {
            return Poll::Ready(result);
        }
        match self.keys.get(&ticket.0) {
            Some((tick, _)) => Poll::Pending { submit_tick: *tick },
            None => Poll::Unknown,
        }
    }

    /// Requests still queued.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Served results not yet polled.
    pub fn ready(&self) -> usize {
        self.done.len()
    }

    /// Running totals.
    pub fn counters(&self) -> RequestCounters {
        self.totals
    }

    /// The last serve's counters (`requests` counts the requests submitted for that tick).
    pub fn last_tick(&self) -> RequestCounters {
        self.last
    }

    /// The table the queue's coarse searches read (repaired at the start of every serve).
    pub fn region_graph(&self) -> &RegionGraph {
        &self.graph
    }

    /// Hand over every served result in ticket order.
    pub fn drain_ready(&mut self) -> Vec<PathResult> {
        std::mem::take(&mut self.done).into_values().collect()
    }

    /// The longest a request can wait, in ticks after its submit tick, with `work_ahead` work
    /// queued before it: `floor(work_ahead / budget)`. Every tick it is not served, the requests
    /// ahead spend the whole budget. `Some(0)` without a budget (every request is served on its
    /// submit tick); `None` for a budget of 0 or less (nothing is served).
    pub fn wait_bound(&self, work_ahead: i64) -> Option<u64> {
        match self.settings.tick_budget {
            None => Some(0),
            Some(b) if b > 0 => Some((work_ahead.max(0) / b) as u64),
            Some(_) => None,
        }
    }

    /// Serve this tick's share of the queue on `grid`, then advance the tick. Requests are taken
    /// in `(submit tick, unit id, ticket)` order while budget is left; each charges its
    /// [`PathResult::work`]. The region graph is synced to the grid first, so call this after the
    /// tick's [`NavGrid::sync_stamps`].
    pub fn serve(&mut self, grid: &NavGrid, scratch: &mut Scratch) {
        let _span = tracing::trace_span!("det_path_requests_serve", pending = self.pending.len()).entered();
        let tick = self.tick;
        let mut now = RequestCounters {
            requests: self.keys.values().filter(|(t, _)| *t == tick).count() as u64,
            ..RequestCounters::default()
        };
        now.graph_repairs = self.graph.sync(grid) as u64;
        if self.cache_gen != grid.generation() || !self.settings.share_routes {
            self.cache.clear();
            self.cache_gen = grid.generation();
        }
        let mut left = self.settings.tick_budget.unwrap_or(i64::MAX);
        while left > 0 {
            let Some(((submit_tick, unit, ticket), q)) = self.pending.pop_first() else {
                break;
            };
            self.keys.remove(&ticket);
            let args = PlanArgs {
                layer: None,
                profile: q.profile,
                from: q.from,
                from_tunnel: None,
                goal: q.to,
                goal_tunnel: None,
                tunnel_pct: TUNNEL_COST_PCT,
            };
            if self.cache.len() >= MAX_CACHED_ROUTES {
                self.cache.clear();
            }
            let share = if self.settings.share_routes {
                Some(&mut self.cache)
            } else {
                None
            };
            let found = search::plan_core(grid, Some(&self.graph), scratch, &args, share);
            let (plan, shared_route) = match found {
                Some((p, s)) => (Some(p), s),
                None => (None, false),
            };
            let result = PathResult {
                ticket: Ticket(ticket),
                unit,
                submit_tick,
                served_tick: tick,
                seq: self.totals.served + now.served,
                plan,
                shared_route,
            };
            let work = result.work();
            left = left.saturating_sub(work);
            now.served += 1;
            now.work += work as u64;
            if shared_route {
                now.cache_hits += 1;
            }
            if let Some(p) = &result.plan {
                now.expansions += u64::from(p.fine_expansions);
                now.coarse_expansions += u64::from(p.coarse_expansions);
            }
            self.done.insert(ticket, result);
        }
        now.carried = self.pending.len() as u64;
        // `requests` and `cancelled` are counted as they happen.
        let mut add = now;
        add.requests = 0;
        self.totals.add(&add);
        self.last = now;
        self.tick += 1;
    }

    /// FNV-1a 64 over the queue's sim state (tick, tickets, pending requests, unread results with
    /// their waypoints, the cached route keys), for desync checks and goldens.
    pub fn state_hash(&self) -> u64 {
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        h.u64(self.tick);
        h.u64(self.next_ticket);
        h.u64(self.cache_gen);
        for (from, to, pi, generation) in self.cache.keys() {
            h.u64(u64::from(*from));
            h.u64(u64::from(*to));
            h.u64(*pi as u64);
            h.u64(*generation);
        }
        for ((tick, unit, ticket), q) in &self.pending {
            h.u64(*tick);
            h.u64(u64::from(*unit));
            h.u64(*ticket);
            for v in q.from.iter().chain(q.to.iter()) {
                h.u64(u64::from(v.to_bits()));
            }
            h.u64(u64::from(q.profile.0));
        }
        for (ticket, r) in &self.done {
            h.u64(*ticket);
            h.u64(r.served_tick);
            h.u64(u64::from(r.shared_route));
            match &r.plan {
                None => h.u64(u64::MAX),
                Some(p) => {
                    h.u64(p.kind as u64);
                    h.u64(u64::from(p.snapped));
                    h.u64(u64::from(p.work));
                    for w in &p.waypoints {
                        h.u64(u64::from(w[0].to_bits()));
                        h.u64(u64::from(w[1].to_bits()));
                    }
                    for c in &p.cells {
                        h.u64(u64::from(*c));
                    }
                }
            }
        }
        h.0
    }
}

/// FNV-1a 64 (public domain, http://www.isthe.com/chongo/tech/comp/fnv/).
struct Fnv(u64);

impl Fnv {
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests;
