// SPDX-License-Identifier: MPL-2.0
//! Navigation (`docs/specs/22-tier2.md` § T2.2): a Recast-style navmesh generator, A* +
//! string-pulling pathfinding, dynamic obstacle carving, and ORCA crowd avoidance.
//!
//! # How the pieces fit
//!
//! A [`NavMeshBuilder`] turns a triangle soup (typically the level's collider geometry) into a
//! [`NavMesh`] of convex polygons: conservative voxelisation into a span heightfield (a cell
//! takes every triangle whose XZ projection overlaps its square, so geometry thinner than a cell
//! is seen wherever it falls on the grid; nav cell v3, ADR-8899), walkable
//! marking by slope, headroom and agent-radius erosion, region flood fill, and polygonisation (a
//! greedy rectangle merge of the walkable grid; the traced contours are kept separately for
//! display and do not feed the polygons).
//! [`NavQuery`] then answers `find_nearest` and `find_path` (A* over the polygon graph + the
//! string-pulling funnel) against the mesh, honouring the dynamic [`NavObstacles`] set.
//! [`NavAgent`]/[`NavAgentState`] plus [`NavPlugin`] steer agents along planned paths with ORCA
//! local avoidance ([`orca`]).
//!
//! A game inserts the built [`NavMesh`] and a [`NavObstacles`] as resources, then adds
//! [`NavPlugin`]. Agents get a [`NavAgent`] + [`NavAgentState`]; a path is planned with
//! [`NavQuery::find_path`] and installed with [`NavAgentState::set_path`].
//!
//! # Determinism and provenance
//!
//! Generation is deterministic for a given triangle soup and parameters, and pathfinding is
//! deterministic given a mesh and obstacle set. Every non-trivial algorithm is cited in
//! `docs/provenance.md` (Recast, the funnel algorithm, ORCA, greedy convex merge).
//!
//! # Getting started
//!
//! Build a mesh from triangles, then ask it for a path:
//!
//! ```
//! # fn main() -> Result<(), regia_nav::NavBuildError> {
//! use regia_core::math::Vec3;
//! use regia_nav::{NavMeshBuilder, NavQuery};
//!
//! // A 12 m by 12 m floor made of two triangles.
//! let h = 6.0;
//! let floor = [
//!     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
//!     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
//! ];
//! let mesh = NavMeshBuilder::default().build(&floor)?;
//! assert!(mesh.poly_count() > 0);
//!
//! let query = NavQuery { mesh: &mesh, obstacles: None };
//! let extents = Vec3::splat(2.0);
//! let path = query
//!     .find_path(Vec3::new(-4.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 0.0), extents)
//!     .expect("both ends are on the floor");
//! // Straight across: about 8 m.
//! assert!((path.length - 8.0).abs() < 0.5);
//! assert!(path.waypoints.len() >= 2);
//! # Ok(())
//! # }
//! ```
//!
//! See `book/user-guide/navigation-ai.md` and `book/agents/navigation.md`.

pub mod agent;
pub mod areas;
pub mod builder;
pub mod config;
pub(crate) mod contours;
pub mod crowd;
/// Deterministic navigation for lockstep simulations: the [`regia_nav_det`] crate, re-exported so the
/// `regia_nav::det::*` paths keep working (ADR-8877). It has no ECS dependency; a headless lockstep sim
/// depends on `regia_nav_det` directly.
pub use regia_nav_det as det;
pub(crate) mod heightfield;
pub mod lanes;
pub mod layered;
pub mod link_file;
pub mod link_gen;
pub mod links;
pub mod mesh;
pub mod obstacle;
pub mod orca;
pub mod origin;
pub mod query;
pub mod region_queue;
pub(crate) mod regions;
pub mod stream_gate;
pub mod tile_cache;
// Plan row S-03 (ADR-8405): the agent-protocol view of a host's deterministic agents.
pub mod det_view;

pub use agent::{DEFAULT_PATH_EXTENTS, NavAgent, NavAgentState, NavPlugin, NavStep};
pub use agent::{NavAgentStuck, NavAgentStuckConfig, NavStuckProgress};
pub use areas::paint_triangle_areas;
pub use areas::{FLANK_AREA_COST, NAV_AREA_FLANK, NAV_AREA_TUNNEL, TUNNEL_AREA_COST};
pub use areas::{
    MAX_NAV_AREA_VOLUMES, MAX_NAV_AREAS, NAV_AREA_DEFAULT, NavAreaCosts, NavAreaError, NavAreaVolume,
};
pub use builder::{NavBuildError, NavMeshBuilder};
pub use config::{
    HOVER_HEIGHT_EXTRA, HULL_SIDE_CLEARANCE, HULL_STEP, MESH_LOCATE_DROPS, PILOT_HEIGHT, PILOT_RADIUS,
    PILOT_STEP,
};
pub use config::{NavAgentParams, NavMeshBuildParams};
pub use contours::Contour;
pub use crowd::NAV_LINK_GONE;
pub use crowd::{
    AvoidanceParams, CrowdAgent, CrowdAgentId, CrowdAgentParams, CrowdAgentState, CrowdGroup, CrowdStats,
    FORMATION_CATCH_UP, FORMATION_HEADING_SPEED, FORMATION_OFF_MESH, MAX_CROWD_AGENTS, MAX_CROWD_NEIGHBOURS,
    MAX_LINK_TRAVERSAL_STEPS, MAX_STEPS_PER_ADVANCE, NavCrowd, NavCrowdConfig, NavCrowdError, NavCrowdPlugin,
    NavCrowdStep, NavLinkEvent, NavLinkPhase, NavLinkTraversal, NavLinkTraversalMode, formation_heading,
    formation_offset_world,
};
pub use crowd::{DEFAULT_STUCK_MIN_PROGRESS_M, NavStuckAction, NavStuckEvent};
pub use crowd::{NavLinkQueueConfig, NavLinkWait};
pub use lanes::{
    MAX_LANE_AGENTS, MAX_LANE_POINTS, MAX_NAV_LANES, NAV_LANE_WALK, NAV_LANES_FORMAT_VERSION, NavLane,
    NavLaneAgent, NavLaneAgentId, NavLaneAgentParams, NavLaneAgentState, NavLaneCrowd, NavLaneCrowdConfig,
    NavLaneCrowdStats, NavLaneDef, NavLaneError, NavLaneGap, NavLaneGraph, NavLanePlugin, NavLaneRoute,
    NavLaneSet, NavLaneStep, NavLaneTransition, migrate_nav_lanes, nav_lanes_migration_sources,
    nav_lanes_schema_fingerprint,
};
pub use lanes::{MAX_ROAD_LANES_EACH_WAY, NavRoadLaneParams, NavRoadNetwork};
pub use link_file::{
    MAX_NAV_FILE_BYTES, NAV_LINK_ANCHOR_FORMAT_VERSION, NavAuthoredLinksPlugin, NavAuthoredLinksStatus,
    NavFileError, NavLinkAnchor, NavLinkFiles, nav_link_anchor_migration_sources,
    nav_link_anchor_schema_fingerprint,
};
pub use link_file::{
    NAV_LINKS_FORMAT_VERSION, NavLinkDef, NavLinkSet, nav_links_migration_sources,
    nav_links_schema_fingerprint,
};
pub use link_gen::generate_clear_links;
pub use link_gen::{NavLinkCandidate, NavLinkGenParams, TriangleClearance, generate_links};
pub use links::{
    LAYER_LINK_SNAP_RADIUS, NAV_DECK_STAIRS_USER_ID, NAV_LADDER_USER_ID, NavLayerLink, NavLayerLinkKind,
    pilot_layer_links,
};
pub use links::{
    MAX_NAV_LINKS, NavLinkError, NavLinkFlags, NavLinkKind, NavLinkState, NavLinkStatus, NavQueryFilter,
    validate_links,
};
pub use links::{NavLinkArc, NavLinkArcConfig};
pub use mesh::{DEFAULT_LINK_RADIUS, NavMesh, NavPoly, OffMeshLink, Portal};
pub use obstacle::NavObstacles;
pub use origin::NavOriginPlugin;
pub use query::{NavPath, NavQuery};
pub use query::{NavPathResult, NavQueryError, NavRaycastHit};
pub use region_queue::{
    NavDirtySource, NavGeometrySource, NavRegionConfig, NavRegionDirty, NavRegionPlugin, NavRegionQueue,
    NavRegionStats, NavRegionStep, apply_nav_regions,
};
pub use stream_gate::{NAV_GATE, NavStreamPlugin, NavStreamState, nav_cell_id};
pub use tile_cache::NavTileRevision;
pub use tile_cache::{
    MAX_STREAMED_GRID_CELLS, MAX_TILE_CACHE_OBSTACLES, MAX_TILE_SIZE, NAV_CELL_FORMAT_VERSION,
    NavAreaVolumeId, NavCellLoad, NavObstacleId, NavObstacleShape, NavPathOutcome, NavRegionRebuild,
    NavStreamedPath, NavTileCache, NavTileCacheError, NavTileCacheParams, NavTileLayout, NavTileStreamStatus,
    NavTileUpdate,
};
pub use tile_cache::{NavGridGrowth, NavTileLinks};

#[cfg(test)]
mod property_tests;

pub mod walk;
pub use walk::{
    CapsuleWalkSettings, StairDirection, StairFlag, StairProbe, StairVerdict, WalkCheckError, check_stair,
};
