// SPDX-License-Identifier: MPL-2.0
//! Deterministic navigation for lockstep simulations (ADR-8012 A-4, ADR-8049).
//!
//! Everything in this crate computes the same bits on every peer, on every supported OS and CPU
//! architecture, from the same inputs: integers for every decision (costs, counts, labels,
//! obstacle discs in centimeters), and floats built only from IEEE `+ - * /` in a fixed order plus
//! [`regia_detsim::math`]. No hash-container iteration, no SIMD vector types, no threads.
//!
//! This crate was `regia_nav`'s `det` module; ADR-8877 made it its own crate so a headless lockstep sim
//! links it without the ECS, the scene or the app (no GPU either). `regia_nav` re-exports it as its
//! `det` module, so the old `det::grid::NavGrid` path there and `regia_nav_det::grid::NavGrid` name
//! the same type. A game's sim links it and drives it from its own tick. `cargo xtask det-scan` holds it to the lockstep rules as a closure crate
//! (`docs/ci/lockstep-closure.toml`, `[[crate]]` row): the line rules, its `clippy.toml` and its
//! workspace dependencies.
//!
//! - [`grid`]: the navigation grid. One cell per ground sample, a cost per movement class, obstacle
//!   discs stamped per size class, and per-cluster regions for a coarse search.
//! - [`search`]: the two-level A* on the grid (regions per 40 m cluster, then cells inside the
//!   corridor), an expansion budget, `(f, h, index)` ties and string pulling; [`search::plan_layered`]
//!   adds a tunnel layer.
//! - [`layer`]: the tunnel layer, a graph on a map's sampled tunnel centerlines joined to the grid at
//!   the openings, with a closed set a host replaces whole. It reads the tunnel description of
//!   `regia_world_geom::compat` (this crate depends on regia_world_geom, never the other way).
//! - [`navmesh`]: the navigation mesh. Convex polygons baked from a ground raster, corridors, rooms,
//!   stairs and doors, linked by shared edges and overlaps, with a byte-stable cooked form (ADR-8174).
//! - [`tunnel_bake`]: the tunnel navmesh bake (`TunnelNavBake` for [`navmesh::NavMesh`]): tunnel corridors
//!   from a map's sampled layout on a ground raster from its terrain, so `TunnelsState<NavMesh>` arms it.
//! - [`layers`]: the runtime path query over the layered bake (integer millimetres, a layer per polygon,
//!   passage lanes, closed sections and dirty tiles; ADR-8486). It replaces the tunnel layer's centerline
//!   graph for maps with a layered bake.
//!
//! - [`region_graph`]: the coarse search's region graph as a table, repaired per dirty cluster
//!   (ADR-8295).
//! - [`requests`]: a path-request queue served in `(submit tick, unit id)` order under a per-tick
//!   work budget, with shared coarse routes and counters (ADR-8295).
//!
//! - [`flow`]: flow fields. An integration field and a direction per cell toward a goal set (one
//!   Dijkstra for any number of movers), a clustered flow for large maps (an abstract route over
//!   the cluster regions, local fields on demand), and a cache keyed by goal set, profile and grid
//!   generation.
//! - [`fog_los`]: terrain line of sight for fog of war over the grid's heights.
//!
//! - [`crowd`]: formation slots, choke lanes and slots that bend back to reachable ground. Tunnel
//!   mouths and junctions are registered from the map's tunnel layer through
//!   [`crowd::Crowd::register_tunnel_chokes`] (KI-WD-030); other narrows through
//!   [`crowd::Crowd::register_choke`].
//! - [`walls`]: the blocked-cell boundary around a point as wall segments for the LP avoidance
//!   solver (`regia_detsim::avoid::OrcaSolver::Lp`, ADR-8555).
//!
//! The crate imports only std, serde, thiserror, tracing, the lockstep kit and `regia_world_geom::compat`.
//!
//! See the book page "Deterministic navigation grid" (`book/agents/det-grid.md`).

pub mod agent;
pub mod crowd;
pub mod flow;
pub mod fog_los;
pub mod grid;
pub mod layer;
pub mod layers;
pub mod navmesh;
pub mod region_graph;
pub mod requests;
pub mod search;
pub mod tunnel_bake;
pub mod walls;
