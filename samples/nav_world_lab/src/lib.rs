// SPDX-License-Identifier: MIT-0
//! Headless nav world lab (ADR-2133): the next-tier navigation features in one deterministic run.
//!
//! - **Area costs** come from a level's `NavAreaCostTable` (mud costs 4 per metre) and a painted
//!   mud strip across the middle of a 48 m plaza: paths go round it through the gap.
//! - **Tile streaming**: the plaza is cooked into one cell per tile column and only the west half
//!   is loaded. Crowd agents sent east with `NavCrowd::set_target_streamed` walk to the loaded
//!   border and wait there (`awaiting_tile`); when the east cells load they go on and arrive.
//! - **Lane graphs**: a two-way promenade (two lanes over one strip, paired with `opposite`) whose
//!   walkers keep right of oncoming ones, ending in a split where a weighted lane draws most
//!   wanderers and a flagged lane draws every walker that prefers it.
//! - **Physics asset**: a limited humanoid ragdoll profile with a box torso and a sphere head is drop-tested
//!   (`regia_framework::ragdoll::preview`) until it settles.
//! - **Traffic** ([`traffic`], ADR-3117): four AI drivers (`regia_framework::vehicle_ai`) drive
//!   physics vehicles round a closed oval of lanes; [`traffic::run_traffic`] has its own report.
//!
//! [`run`] returns a [`LabReport`] whose `digest` covers every output: equal runs give equal
//! digests.

pub mod traffic;

use std::fmt;

use regia_animation::demo_rig::{DemoRig, demo_humanoid};
use regia_core::math::Vec3;
use regia_framework::nav_world::{NavAreaCostEntry, NavAreaCostTable};
use regia_framework::ragdoll::preview::{RagdollPreviewOptions, preview_drop};
use regia_framework::ragdoll::{RagdollProfile, RagdollShape};
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, NavAreaVolume, NavCrowd, NavCrowdConfig, NavLaneAgentParams,
    NavLaneCrowd, NavLaneCrowdConfig, NavLaneDef, NavLaneGraph, NavMeshBuildParams, NavObstacleShape,
    NavTileCache, NavTileCacheParams,
};

/// The mud area id.
pub const MUD: u8 = 3;

/// What to run.
#[derive(Clone, Debug, PartialEq)]
pub struct LabConfig {
    /// Crowd agents sent east across the streamed border.
    pub crowd_agents: usize,
    /// Crowd steps before the east cells load.
    pub load_at: u32,
    /// Crowd steps in all.
    pub crowd_steps: u32,
    /// Lane walkers.
    pub lane_agents: u64,
    /// Lane steps.
    pub lane_steps: u32,
    /// Ragdoll drop-test steps.
    pub ragdoll_steps: u32,
}

impl Default for LabConfig {
    fn default() -> Self {
        Self {
            crowd_agents: 12,
            load_at: 1500,
            crowd_steps: 3000,
            lane_agents: 40,
            lane_steps: 2400,
            ragdoll_steps: 900,
        }
    }
}

/// What the lab saw.
#[derive(Clone, Debug, PartialEq)]
pub struct LabReport {
    /// Agents waiting at the border just before the east cells loaded.
    pub waiting_before_load: usize,
    /// Agents that arrived by the end.
    pub arrived: usize,
    /// Waits begun (`CrowdStats::tile_waits`).
    pub tile_waits: u64,
    /// Agents whose final corridor crossed mud.
    pub mud_crossings: usize,
    /// Lane walkers' oncoming yields (`NavLaneCrowdStats::oncoming_yields`).
    pub oncoming_yields: u64,
    /// Lane walkers that took the weighted branch, the plain branch, the flagged branch.
    pub branch_counts: [u64; 3],
    /// The closest two promenade walkers came while passing each other, metres.
    pub closest_pass: f32,
    /// The drop test's settle step.
    pub ragdoll_settled_at: Option<u32>,
    /// Digest over every output.
    pub digest: u64,
}

impl fmt::Display for LabReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "streamed crowd: {} waiting before load, {} arrived, {} waits",
            self.waiting_before_load, self.arrived, self.tile_waits
        )?;
        writeln!(f, "area costs: {} corridors cross mud", self.mud_crossings)?;
        writeln!(
            f,
            "lanes: {} oncoming yields, closest pass {:.3} m, branches weighted/plain/flagged {:?}",
            self.oncoming_yields, self.closest_pass, self.branch_counts
        )?;
        writeln!(f, "ragdoll drop test: settled at {:?}", self.ragdoll_settled_at)?;
        write!(f, "digest {:016x}", self.digest)
    }
}

/// Why the lab could not run.
#[derive(Debug, Clone, PartialEq)]
pub struct LabError(pub String);

impl fmt::Display for LabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LabError {}

fn fail(what: &str) -> impl Fn(String) -> LabError + '_ {
    move |e| LabError(format!("{what}: {e}"))
}

/// The 48 m plaza.
fn plaza() -> Vec<[Vec3; 3]> {
    let c = 24.0;
    vec![
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, -c),
            Vec3::new(c, 0.0, c),
        ],
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, c),
            Vec3::new(-c, 0.0, c),
        ],
    ]
}

fn params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    }
}

struct Eater(u64);

impl Eater {
    fn eat(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Runs the lab.
///
/// # Errors
/// A build, cook or agent call the lab's fixed inputs should never fail.
pub fn run(config: &LabConfig) -> Result<LabReport, LabError> {
    run_traced(config, &mut |_, _, _| {})
}

/// A per-step digest sink: `(section, step, digest)`.
///
/// Sections, in run order: `cook` (one FNV-1a digest per cooked tile column blob, step = column),
/// `crowd` and `lanes` (the state digest after each step), `ragdoll` (the drop test's digest,
/// step 0). Two hosts that print the same final digest print the same trace; where they differ,
/// the first differing line names the section and step that diverged first.
pub type DigestTrace<'a> = dyn FnMut(&str, u32, u64) + 'a;

/// FNV-1a over `bytes`.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = Eater(0xcbf2_9ce4_8422_2325);
    h.eat(bytes);
    h.0
}

/// [`run`], reporting every intermediate digest to `trace` (see [`DigestTrace`]).
///
/// # Errors
/// As [`run`].
pub fn run_traced(config: &LabConfig, trace: &mut DigestTrace<'_>) -> Result<LabReport, LabError> {
    let mut h = Eater(0xcbf2_9ce4_8422_2325);

    // -- Area costs: the level's table and a mud strip with a gap at the north end. --------------
    let table = NavAreaCostTable {
        areas: vec![NavAreaCostEntry {
            area: MUD,
            cost: 4.0,
            name: "mud".to_owned(),
        }],
    };
    let costs = table.costs().map_err(|e| fail("area table")(e.to_string()))?;
    let mut whole = NavTileCache::build(params(), &plaza()).map_err(|e| fail("build")(e.to_string()))?;
    let strip = NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(-2.0, -1.0, -24.0),
            max: Vec3::new(2.0, 1.0, 14.0),
        },
        MUD,
    );
    whole
        .add_area_volume(strip)
        .map_err(|e| fail("mud")(e.to_string()))?;
    whole.update();

    // -- Streaming: one cell per tile column, the west half loaded. -------------------------------
    let (tx, tz) = whole.tile_grid();
    let mut cache =
        NavTileCache::streamed(params(), whole.layout()).map_err(|e| fail("streamed")(e.to_string()))?;
    cache.set_area_costs(costs);
    let mut east = Vec::new();
    for x in 0..tx {
        let column: Vec<(u32, u32)> = (0..tz).map(|z| (x, z)).collect();
        let blob = whole
            .cook_cell(&column)
            .map_err(|e| fail("cook")(e.to_string()))?;
        trace("cook", x, fnv(&blob));
        if x < tx / 2 {
            cache
                .load_cell(u64::from(x), &blob)
                .map_err(|e| fail("load")(e.to_string()))?;
        } else {
            east.push((u64::from(x), blob));
        }
    }
    cache.update();

    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).map_err(|e| fail("crowd")(e.to_string()))?;
    let mut ids = Vec::new();
    for k in 0..config.crowd_agents {
        let z = -18.0 + 3.0 * (k % 12) as f32;
        let id = crowd
            .add_agent(
                cache.mesh(),
                Vec3::new(-20.0, 0.0, z),
                CrowdAgentParams::default(),
            )
            .map_err(|e| fail("agent")(e.to_string()))?;
        crowd
            .set_target_streamed(&cache, id, Vec3::new(20.0, 0.0, -z))
            .map_err(|e| fail("target")(e.to_string()))?;
        ids.push(id);
    }
    let mut waiting_before_load = 0;
    for step in 0..config.crowd_steps {
        if step == config.load_at {
            waiting_before_load = crowd
                .agents()
                .iter()
                .filter(|a| a.state() == CrowdAgentState::AwaitingTile)
                .count();
            for (id, blob) in &east {
                cache
                    .load_cell(*id, blob)
                    .map_err(|e| fail("load")(e.to_string()))?;
            }
        }
        cache.update();
        crowd.sync_mesh_revision(cache.revision());
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
        trace("crowd", step, crowd.digest());
        h.eat(&crowd.digest().to_le_bytes());
    }
    let arrived = crowd
        .agents()
        .iter()
        .filter(|a| a.state() == CrowdAgentState::Arrived)
        .count();
    let tile_waits = crowd.stats().tile_waits;
    // Where each agent crossed: a fresh path over the whole plaza with the table's costs.
    let mut mud_crossings = 0;
    for k in 0..config.crowd_agents {
        let z = -18.0 + 3.0 * (k % 12) as f32;
        let path = cache.query().find_path(
            Vec3::new(-20.0, 0.0, z),
            Vec3::new(20.0, 0.0, -z),
            Vec3::splat(1.0),
        );
        if let Some(path) = path
            && path
                .polys
                .iter()
                .any(|&p| cache.mesh().polys[p as usize].area == MUD)
        {
            mud_crossings += 1;
        }
    }

    // -- Lanes: a paired promenade and a split. ---------------------------------------------------
    let lane = |points: &[(f32, f32)]| {
        let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
        NavLaneDef::new(&p)
    };
    // 60 m long; the two flows start apart (west end eastbound, east end westbound) and meet.
    let mut east_way = lane(&[(-60.0, 30.0), (0.0, 30.0)]);
    east_way.width = 3.0;
    east_way.next = vec![2, 3, 4];
    let mut west_way = lane(&[(0.0, 30.0), (-60.0, 30.0)]);
    west_way.width = 3.0;
    west_way.opposite = Some(0);
    let mut weighted = lane(&[(0.0, 30.0), (10.0, 40.0)]);
    weighted.weight = 3.0;
    let plain = lane(&[(0.0, 30.0), (10.0, 30.0)]);
    let mut flagged = lane(&[(0.0, 30.0), (10.0, 20.0)]);
    flagged.flags = 1 | 4;
    let graph = NavLaneGraph::build(&[east_way, west_way, weighted, plain, flagged], 0.0)
        .map_err(|e| fail("lanes")(e.to_string()))?;
    let mut lanes =
        NavLaneCrowd::new(NavLaneCrowdConfig::default()).map_err(|e| fail("lane crowd")(e.to_string()))?;
    let mut walkers = Vec::new();
    for i in 0..config.lane_agents {
        let params = NavLaneAgentParams {
            speed: 1.1 + (i % 4) as f32 * 0.15,
            prefer_flags: if i % 5 == 0 { 4 } else { 0 },
            ..Default::default()
        };
        let on = (i % 2) as u32;
        let id = lanes
            .add_agent(&graph, on, (i / 2) as f32 * 1.2, 0.0, params, i)
            .map_err(|e| fail("walker")(e.to_string()))?;
        walkers.push((id, on));
    }
    let mut closest_pass = f32::INFINITY;
    for step in 0..config.lane_steps {
        lanes.step(&graph);
        trace("lanes", step, lanes.digest());
        h.eat(&lanes.digest().to_le_bytes());
        let positions: Vec<(Vec3, u32)> = walkers
            .iter()
            .filter_map(|&(id, _)| {
                let a = lanes.agent(id)?;
                let lane = a.lane();
                // Walkers passing each other on the promenade (not those stopped at its far end).
                if lane >= 2 || a.state() != regia_nav::NavLaneAgentState::Walking {
                    return None;
                }
                lanes.position(&graph, id).map(|p| (p, lane))
            })
            .collect();
        for (i, (p, l)) in positions.iter().enumerate() {
            for (q, m) in &positions[i + 1..] {
                if l != m {
                    closest_pass = closest_pass.min((*p - *q).length());
                }
            }
        }
    }
    let mut branch_counts = [0u64; 3];
    for a in lanes.agents() {
        if let Some(slot) = (a.lane() as usize)
            .checked_sub(2)
            .and_then(|i| branch_counts.get_mut(i))
        {
            *slot += 1;
        }
    }
    let oncoming_yields = lanes.stats().oncoming_yields;

    // -- Physics asset: a ragdoll drop test. ------------------------------------------------------
    let skeleton = demo_humanoid(&DemoRig::default()).ok_or_else(|| LabError("demo rig".to_owned()))?;
    let mut profile = RagdollProfile::humanoid_limited(&skeleton, 75.0, Vec3::NEG_Z);
    // Mixed shapes (`.ragdoll.ron` v2): a box torso and a sphere head. The humanoid lists hips, chest, head first.
    if let [_, chest, head, ..] = profile.bones.as_mut_slice() {
        chest.shape = RagdollShape::Box {
            width: 0.34,
            depth: 0.22,
        };
        head.shape = RagdollShape::Sphere;
    }
    let drop = preview_drop(
        std::sync::Arc::new(skeleton),
        &profile,
        &RagdollPreviewOptions {
            steps: config.ragdoll_steps,
            push: Vec3::new(0.0, 0.0, 30.0),
            ..Default::default()
        },
    )
    .map_err(|e| fail("drop test")(e.to_string()))?;
    trace("ragdoll", 0, drop.digest);
    h.eat(&drop.digest.to_le_bytes());

    for v in [
        waiting_before_load as u64,
        arrived as u64,
        tile_waits,
        mud_crossings as u64,
        oncoming_yields,
    ] {
        h.eat(&v.to_le_bytes());
    }
    for v in branch_counts {
        h.eat(&v.to_le_bytes());
    }
    h.eat(&closest_pass.to_bits().to_le_bytes());
    Ok(LabReport {
        waiting_before_load,
        arrived,
        tile_waits,
        mud_crossings,
        oncoming_yields,
        branch_counts,
        closest_pass,
        ragdoll_settled_at: drop.settled_at,
        digest: h.0,
    })
}
