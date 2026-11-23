// SPDX-License-Identifier: MPL-2.0
//! The runtime side of region rebuilds (ADR-1934): dirty boxes from destruction, terrain edits and
//! game code wait in a [`NavRegionQueue`], and a fixed-step system re-voxelizes a budgeted slice of
//! them into the [`NavTileCache`].
//!
//! `regia_nav` depends on neither physics nor terrain. `regia_framework::nav_world` turns their
//! messages into boxes and supplies the geometry; any other host calls [`NavRegionQueue::push`] or
//! writes [`NavRegionDirty`], and sets a [`NavGeometrySource`].
//!
//! A box is cut along tile borders and each slice is one unit of work, so a blast across the map
//! costs its share per step. Slices run in arrival order, so a replay rebuilds the same tiles on
//! the same steps. Polygon indices change with each rebuild; crowds replan through
//! [`NavTileCache::revision`].

use std::collections::VecDeque;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec3;

use crate::crowd::NavCrowdStep;
use crate::tile_cache::NavTileCache;

/// Supplies level geometry for a region: appends every triangle touching the XZ box `min..max` and
/// returns `true`, or returns `false` when the geometry is not available yet (the box waits and is
/// tried again next step). Missing triangles read as holes in the navmesh.
pub type NavGeometrySource = Arc<dyn Fn(&World, Vec3, Vec3, &mut Vec<[Vec3; 3]>) -> bool + Send + Sync>;

/// Who dirtied a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirtySource {
    /// Chunks left a destructible structure.
    Destruction,
    /// A runtime terrain height patch.
    Terrain,
    /// A reloaded tile replaying an earlier edit.
    Replay,
    /// The agent protocol.
    Agent,
    /// Game code.
    Game,
}

impl NavDirtySource {
    /// The stable lowercase name (agent protocol).
    pub fn name(self) -> &'static str {
        match self {
            Self::Destruction => "destruction",
            Self::Terrain => "terrain",
            Self::Replay => "replay",
            Self::Agent => "agent",
            Self::Game => "game",
        }
    }
}

/// Asks for the navmesh under a world box to be rebuilt from current geometry. Read by
/// [`NavRegionPlugin`]; corners may come in any order.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct NavRegionDirty {
    /// One corner.
    pub min: Vec3,
    /// The opposite corner.
    pub max: Vec3,
    /// Who asked.
    pub source: NavDirtySource,
}

/// The budgets of [`NavRegionPlugin`]'s step. `NavCrowdPlugin` leaves queued tiles to this step
/// when the app has a queue, so both budgets hold with a crowd too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavRegionConfig {
    /// Box slices (at most one tile's area each) re-voxelized per fixed step.
    pub max_slices_per_step: usize,
    /// Tiles polygonised per fixed step ([`NavTileCache::update_limited`]).
    pub max_tiles_per_step: usize,
    /// Boxes held before new ones merge into the last one. Past it rebuilds cover more area than
    /// they need; no edit is lost.
    pub max_queued: usize,
}

impl Default for NavRegionConfig {
    fn default() -> Self {
        Self {
            max_slices_per_step: 4,
            max_tiles_per_step: 16,
            max_queued: 1024,
        }
    }
}

/// What the queue has done since it was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavRegionStats {
    /// Boxes received.
    pub received: u64,
    /// Boxes dropped because a queued box already covers them.
    pub covered: u64,
    /// Boxes merged into another because the queue was full.
    pub overflowed: u64,
    /// Boxes dropped because they are invalid (non-finite) or miss the grid.
    pub rejected: u64,
    /// Slices re-voxelized.
    pub slices: u64,
    /// Stored cells changed.
    pub cells_changed: u64,
    /// Steps a box waited because the source had no geometry yet.
    pub source_waits: u64,
    /// Rebuilds refused by the cache (non-finite geometry from the source).
    pub failed: u64,
}

/// One queued box.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Dirty {
    lo: Vec3,
    hi: Vec3,
    source: NavDirtySource,
    /// Already cut along tile borders, or kept whole on purpose (a replay must cover its box).
    sliced: bool,
}

/// Dirty boxes waiting for a rebuild. [`NavRegionPlugin`] inserts it; set a source with
/// [`Self::set_source`].
#[derive(Resource, Default)]
pub struct NavRegionQueue {
    queued: VecDeque<Dirty>,
    source: Option<NavGeometrySource>,
    /// The step budgets.
    pub config: NavRegionConfig,
    stats: NavRegionStats,
}

impl std::fmt::Debug for NavRegionQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavRegionQueue")
            .field("queued", &self.queued)
            .field("has_source", &self.source.is_some())
            .field("config", &self.config)
            .field("stats", &self.stats)
            .finish()
    }
}

impl NavRegionQueue {
    /// Moves every waiting box by `delta` (the floating-origin step of spec 78 § 12.2; call it with
    /// [`NavTileCache::shift_origin`]).
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        for d in &mut self.queued {
            d.lo += delta;
            d.hi += delta;
        }
    }

    /// Sets where geometry comes from.
    pub fn set_source(&mut self, source: NavGeometrySource) {
        self.source = Some(source);
    }

    /// Whether a source is set.
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }

    /// Boxes waiting (slices count one each).
    pub fn len(&self) -> usize {
        self.queued.len()
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// The counters.
    pub fn stats(&self) -> NavRegionStats {
        self.stats
    }

    /// The queued boxes in order, as `(min, max, source name)`.
    pub fn boxes(&self) -> impl Iterator<Item = (Vec3, Vec3, &'static str)> + '_ {
        self.queued.iter().map(|d| (d.lo, d.hi, d.source.name()))
    }

    /// Queues a box. A box a queued one covers is dropped, and a queued box this one covers is
    /// replaced in place, so repeated events over one spot cost one rebuild. Non-finite corners
    /// are dropped and counted.
    pub fn push(&mut self, min: Vec3, max: Vec3, source: NavDirtySource) {
        self.stats.received += 1;
        if !min.is_finite() || !max.is_finite() {
            self.stats.rejected += 1;
            return;
        }
        self.insert(Dirty {
            lo: min.min(max),
            hi: min.max(max),
            source,
            sliced: source == NavDirtySource::Replay,
        });
    }

    fn insert(&mut self, new: Dirty) {
        // A replay only counts as done when one rebuild covers its box
        // ([`NavTileCache::take_replay_regions`]), so it never merges, either way.
        let replay = |d: &Dirty| d.source == NavDirtySource::Replay;
        if replay(&new) {
            self.queued.push_back(new);
            return;
        }
        let covers = |a: &Dirty, b: &Dirty| {
            a.lo.x <= b.lo.x && a.lo.z <= b.lo.z && a.hi.x >= b.hi.x && a.hi.z >= b.hi.z
        };
        if self
            .queued
            .iter()
            .any(|d| !replay(d) && !d.sliced && covers(d, &new))
        {
            self.stats.covered += 1;
            return;
        }
        if let Some(i) = self.queued.iter().position(|d| !replay(d) && covers(&new, d)) {
            self.queued[i] = new;
            self.stats.covered += 1;
            return;
        }
        if self.queued.len() >= self.config.max_queued.max(1)
            && let Some(last) = self.queued.iter_mut().rev().find(|d| !replay(d))
        {
            self.stats.overflowed += 1;
            last.lo = last.lo.min(new.lo);
            last.hi = last.hi.max(new.hi);
            last.sliced = false;
            return;
        }
        self.queued.push_back(new);
    }

    /// Cuts `d` along tile borders into the queue's front, in tile order. A box on the grid's edge
    /// is clipped to the tiles it touches; one that misses the grid yields nothing. The tiles are
    /// looked up one cell wider than the box: a box between two cell centers still has a tile,
    /// and [`NavTileCache::rebuild_region`] grows it by that cell anyway.
    fn slice_front(&mut self, d: Dirty, cache: &NavTileCache) {
        let reach = Vec3::splat(cache.params().build.cell_size);
        let tiles = cache.tiles_in(d.lo - reach, d.hi + reach);
        let mut slices = Vec::with_capacity(tiles.len());
        for (tx, tz) in tiles {
            let Some((t_lo, t_hi)) = cache.tile_bounds(tx, tz) else {
                continue;
            };
            let (lo_x, lo_z) = (d.lo.x.max(t_lo.x), d.lo.z.max(t_lo.z));
            let (hi_x, hi_z) = (d.hi.x.min(t_hi.x), d.hi.z.min(t_hi.z));
            // The extra cell can pull in a tile the box does not reach.
            if lo_x > hi_x || lo_z > hi_z {
                continue;
            }
            slices.push(Dirty {
                lo: Vec3::new(lo_x, d.lo.y, lo_z),
                hi: Vec3::new(hi_x, d.hi.y, hi_z),
                source: d.source,
                sliced: true,
            });
        }
        if slices.is_empty() {
            self.stats.rejected += 1;
        }
        for s in slices.into_iter().rev() {
            self.queued.push_front(s);
        }
    }
}

/// The fixed-step region work of [`NavRegionPlugin`]. Runs before [`NavCrowdStep`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NavRegionStep;

/// Re-voxelizes the queue into the [`NavTileCache`] and polygonises the queued tiles. Inert until
/// the app inserts a [`NavTileCache`]. Add it to `FixedUpdate` through [`NavRegionPlugin`]; call it
/// directly to step a world by hand.
pub fn apply_nav_regions(world: &mut World) {
    if !world.contains_resource::<NavTileCache>() || !world.contains_resource::<NavRegionQueue>() {
        return;
    }
    world.resource_scope(|world, mut queue: Mut<NavRegionQueue>| {
        let replays = world.resource_mut::<NavTileCache>().take_replay_regions();
        for (lo, hi) in replays {
            queue.push(lo, hi, NavDirtySource::Replay);
        }
        if !queue.is_empty() {
            let _span = tracing::info_span!("nav_apply_regions", queued = queue.len()).entered();
            rebuild_slices(world, &mut queue);
        }
        let tiles = queue.config.max_tiles_per_step;
        let mut cache = world.resource_mut::<NavTileCache>();
        if cache.pending_tile_count() > 0 {
            cache.update_limited(tiles);
        }
    });
}

/// Runs up to the step's budget of slices. Stops at a box the source cannot serve yet, so the order
/// of edits holds.
fn rebuild_slices(world: &mut World, queue: &mut NavRegionQueue) {
    let Some(source) = queue.source.clone() else {
        queue.stats.source_waits += 1;
        return;
    };
    let mut triangles = Vec::new();
    let mut budget = queue.config.max_slices_per_step;
    while budget > 0 {
        let Some(d) = queue.queued.pop_front() else { break };
        if !d.sliced {
            queue.slice_front(d, world.resource::<NavTileCache>());
            continue;
        }
        let Some((w_lo, w_hi)) = world.resource::<NavTileCache>().region_window(d.lo, d.hi) else {
            queue.stats.rejected += 1;
            continue;
        };
        triangles.clear();
        if !source(world, w_lo, w_hi, &mut triangles) {
            queue.queued.push_front(d);
            queue.stats.source_waits += 1;
            return;
        }
        match world
            .resource_mut::<NavTileCache>()
            .rebuild_region(d.lo, d.hi, &triangles)
        {
            Ok(done) => {
                queue.stats.slices += 1;
                queue.stats.cells_changed += done.cells_changed as u64;
            }
            Err(e) => {
                queue.stats.failed += 1;
                tracing::warn!(target: "regia::nav", error = %e, "nav region rebuild refused");
                if d.source == NavDirtySource::Replay {
                    world.resource_mut::<NavTileCache>().abandon_replay(d.lo, d.hi);
                }
            }
        }
        budget -= 1;
    }
}

/// Reads [`NavRegionDirty`] into the queue.
fn read_nav_region_messages(mut reader: MessageReader<NavRegionDirty>, mut queue: ResMut<NavRegionQueue>) {
    for m in reader.read() {
        queue.push(m.min, m.max, m.source);
    }
}

/// Region rebuilds: the [`NavRegionDirty`] message, the [`NavRegionQueue`] resource and the
/// fixed-step [`apply_nav_regions`]. Keeps a queue the app inserted first.
pub struct NavRegionPlugin;

impl Plugin for NavRegionPlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        app.add_message::<NavRegionDirty>()
            .init_resource::<NavRegionQueue>();
        app.add_systems(
            FixedUpdate,
            (read_nav_region_messages, apply_nav_regions)
                .chain()
                .in_set(NavRegionStep)
                .before(NavCrowdStep),
        );
    }
}
