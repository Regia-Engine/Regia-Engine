// SPDX-License-Identifier: MPL-2.0
//! Procedural content graphs (`docs/specs/62-procedural-content-graph.md`,
//! `docs/specs/63-procedural-node-library.md`, `docs/specs/65-biomes-and-vegetation-scatter.md`).
//!
//! A [`ProcGraph`] (`.pgraph.ron`) is a directed graph of nodes that read the world (terrain,
//! entities, tables), produce and refine [`PointSet`]s carrying attributes, and spawn mesh
//! instances, prefabs or data. A [`world::ProcGenerator`] component binds a graph, its parameters
//! and a seed to a level entity and runs it over its bounds. Output is deterministic: the same
//! graph, parameters, seed, inputs and run limits give the same bytes on every platform (spec 62
//! § 6). The default limits differ by platform (`exec::RunLimits::for_platform`: Android's are
//! lower), so a graph near a desktop limit can fail `limit_exceeded` on Android instead of giving
//! different bytes; bake with explicit limits (`ProcGenerator::limits`) to rule that out.
//!
//! Layout:
//! - [`data`]: attribute values and columns, point sets, tables, items, bundles, selectors.
//! - [`graph`]: the graph and variant formats, parameters, compile (validation, order, hash).
//! - [`exec`]: the evaluator, run inputs, limits, cancellation, cache, outputs and reports.
//! - [`nodes`]: the node registry and the built-in node library.
//! - [`detmath`], [`seeds`], [`rot`]: platform-independent math and the seed rules.
//! - [`pgout`]: the baked output file format.
//! - [`polygon`]: 2D polygons with integer-grid booleans and offsets (spec 64 § 3).
//! - [`road`]: the road profile asset (`.road.ron`, spec 64 § 7.1).
//! - [`plant`]: plant-domain data (skeletons, plant meshes, export jobs; spec 68).
//! - [`world`]: the generator component, run lifecycle, events, and the ECS plugin.
//! - [`tiles`]: tiled, multi-scale and anchored runtime generation (spec 66).
//! - [`terrain_edits`]: writing and restoring a published output's terrain edits (spec 64 § 7.3).
//! - [`draw`]: instance sets on the frame renderer (meshes by asset path in [`draw::ProcMeshLibrary`]).
//! - [`debug_view`]: the runtime tile and region overlays (`procgen.set_debug`, spec 66 § 7).
//! - [`painted`]: points painted by the editor's Paint tool (spec 67), read by `in.self`.
//! - [`biome`]: biome definitions, areas and claims, and the `biome.*` pipeline (spec 65).
//!
//! The evaluator (`data`, `graph`, `exec`, `nodes`) uses no ECS types, so it runs in tools and tests
//! without an app.
//!
//! # Getting started
//!
//! Build a graph (here `gen.grid` into `graph.output`), compile it against the built-in node
//! registry and run it over a snapshot of the world. The generator's bounds come from the snapshot;
//! the result is the same bytes for the same inputs and seed:
//!
//! ```
//! use std::sync::Arc;
//!
//! use regia_core::id::Guid;
//! use regia_procgen::data::{ShapeData, ShapeForm};
//! use regia_procgen::exec::SelfInfo;
//! use regia_procgen::{
//!     CancelToken, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs, StaticSnapshot,
//! };
//!
//! let mut graph = ProcGraph::default();
//! graph.nodes.insert(1, NodeEntry::new("gen.grid"));
//! graph.nodes.insert(2, NodeEntry::new("graph.output"));
//! graph.edges.push(Edge::new(1, "out", 2, "in"));
//! let compiled = graph.compile(&ProcNodeRegistry::with_builtins())?;
//!
//! // A 20 m by 20 m generator box at the origin.
//! let bounds = ShapeData {
//!     form: ShapeForm::Box { half_extents: [10.0; 3] },
//!     position: [0.0; 3],
//!     rotation: [0.0, 0.0, 0.0, 1.0],
//!     edge_m: 0.0,
//!     clip_xz: None,
//! };
//! let snapshot = Arc::new(StaticSnapshot {
//!     self_info: Some(SelfInfo { guid: Guid::from_u128(1), name: "gen".into(), bounds }),
//!     ..StaticSnapshot::default()
//! });
//!
//! let inputs = RunInputs::new(snapshot, 42);
//! let first = Evaluator::new().run(&compiled, &inputs, &CancelToken::new())?;
//! let again = Evaluator::new().run(&compiled, &inputs, &CancelToken::new())?;
//! assert_eq!(first.output_hash, again.output_hash);
//!
//! let points = first.output.graph_outputs["out"].0[0].as_points().map_or(0, |p| p.len());
//! assert_eq!(points, 16); // a 4 by 4 grid at the default spacing
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod biome;
pub mod cook;
pub mod data;
pub mod debug_view;
pub mod detmath;
pub mod draw;
pub mod exec;
pub mod fingerprint;
pub mod graph;
pub mod nodes;
pub mod painted;
pub mod pgout;
pub mod plant;
pub mod polygon;
pub mod prefab_overrides;
pub mod probe;
pub mod profile;
pub mod road;
pub mod rot;
pub mod seeds;
pub mod terrain_edits;
pub mod tiles;
pub mod value;
pub mod wfc;
pub mod world;

pub use data::{
    AttrColumn, AttrType, AttrValue, Bundle, DataItem, DataKind, KindSet, Payload, Point, PointSet, Table,
};
pub use exec::{
    CancelToken, Evaluator, GeneratedMesh, ProcCache, RunError, RunInputs, RunLimits, RunOutput, RunReport,
    RunResult, StaticSnapshot, WorldSnapshot,
};
pub use graph::{
    CompiledGraph, Edge, GraphError, GraphErrors, NodeEntry, ProcGraph, ProcGraphAsset, ProcGraphVariant,
};
pub use nodes::{NodeKind, ProcNodeRegistry};
pub use value::SettingValue;
pub use wfc::{
    AdjacencyRule, Boundary, CellConstraints, Direction, GridSize, OverlappingModel, Pattern, SampleGrid,
    TileDef, TileId, TiledModel, WfcError, WfcGrid, WfcModel, WfcOptions, WfcSpec,
};
pub use world::{ProcGenPlugin, ProcGenerator};
