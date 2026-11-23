// SPDX-License-Identifier: MPL-2.0
//! Shared helpers for the procgen tests: graph building, running and synthetic terrain.

#![allow(dead_code)] // Each test binary uses a different subset (ADR-1640: shared test helpers).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_core::id::Guid;
use regia_procgen::data::{GroundData, ShapeData, ShapeForm};
use regia_procgen::exec::{SelfInfo, StaticSnapshot};
use regia_procgen::{
    CancelToken, CompiledGraph, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunError, RunInputs,
    RunResult, SettingValue,
};
use regia_terrain::TerrainMap;

/// A node entry with settings given as `(name, RON text)`.
pub fn node(ty: &str, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    for (k, v) in settings {
        n = n.with(k, SettingValue::parse(v).expect("test setting parses"));
    }
    n
}

/// A graph from numbered nodes and `(from, from_pin, to, to_pin)` edges.
pub fn graph(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> ProcGraph {
    ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges
            .iter()
            .map(|(a, ap, b, bp)| Edge::new(*a, ap, *b, bp))
            .collect(),
        ..ProcGraph::default()
    }
}

/// Compiles with the built-in registry.
pub fn compile(g: &ProcGraph) -> CompiledGraph {
    match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    }
}

/// A box shape at the origin.
pub fn box_shape(center: [f32; 3], half: [f32; 3], edge_m: f32) -> ShapeData {
    ShapeData {
        form: ShapeForm::Box { half_extents: half },
        position: center,
        rotation: [0.0, 0.0, 0.0, 1.0],
        edge_m,
        clip_xz: None,
    }
}

/// A snapshot with a generator box and optional ground.
pub fn snapshot(bounds: ShapeData, ground: Option<GroundData>) -> Arc<StaticSnapshot> {
    Arc::new(StaticSnapshot {
        self_info: Some(SelfInfo {
            guid: Guid(0x1234_5678_9abc_def0_1122_3344_5566_7788),
            name: "gen".to_string(),
            bounds,
        }),
        ground: ground.map(Arc::new),
        entities: Vec::new(),
        tables: BTreeMap::new(),
        exports: BTreeMap::new(),
        masks: BTreeMap::new(),
        meshes: BTreeMap::new(),
        rays: BTreeMap::new(),
        colliders: BTreeMap::new(),
        painted: None,
        road_profiles: BTreeMap::new(),
        biome: None,
        self_items: Vec::new(),
        content_hash_memo: Default::default(),
    })
}

/// Runs a compiled graph.
pub fn run_with(c: &CompiledGraph, inputs: &RunInputs) -> Result<RunResult, RunError> {
    Evaluator::new().run(c, inputs, &CancelToken::new())
}

/// Compiles and runs over a snapshot with a seed.
pub fn run(g: &ProcGraph, snap: Arc<StaticSnapshot>, seed: u32) -> RunResult {
    let c = compile(g);
    match run_with(&c, &RunInputs::new(snap, seed)) {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

/// A flat terrain: `n × n` samples over `width` meters at height `h`.
pub fn flat_map(n: u32, width: f32, h: f32) -> TerrainMap {
    TerrainMap::from_heights(n, n, vec![h; (n * n) as usize], width, 1.0, 0.0).expect("flat map")
}

/// The synthetic 256 × 256 heightfield of spec 62 § 10 test 4: a smooth bowl with a tilt, 2 m per
/// sample (510 m wide). Polynomial in f32 only, so it is bit-identical everywhere.
pub fn bowl_heights() -> Vec<f32> {
    let n = 256u32;
    (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32, (i / n) as f32);
            let (dc, dr) = (c - 128.0, r - 128.0);
            0.0012 * dc * dc + 0.0007 * dr * dr + 0.03 * dc
        })
        .collect()
}

/// The bowl as a terrain map.
pub fn bowl_map(heights: Vec<f32>) -> TerrainMap {
    TerrainMap::from_heights(256, 256, heights, 510.0, 1.0, 0.0).expect("bowl map")
}

/// Hex of a hash.
pub fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// The point positions of the first item on graph output `pin`.
pub fn out_points(r: &RunResult, pin: &str) -> regia_procgen::PointSet {
    r.output
        .graph_outputs
        .get(pin)
        .and_then(|b| b.0.first())
        .and_then(|i| i.as_points().cloned())
        .unwrap_or_default()
}
