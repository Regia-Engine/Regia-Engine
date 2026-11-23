// SPDX-License-Identifier: MPL-2.0
//! Spec 65 § 12 tests: biome format, claims, weights, filters, spacing, children, resolve,
//! exclusion, determinism and grass multipliers (no GPU; synthetic terrain).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, hex};
use regia_core::id::Guid;
use regia_procgen::biome::def::{
    BiomeChannel, BiomeGrass, ChannelRule, Footprint, GrassOutside, KeepClear, Placement, ScaleRule, Sharing,
    Yaw,
};
use regia_procgen::biome::{
    AreaOutline, BiomeAreaSnap, BiomeDef, BiomeEntry, BiomeLayer, BiomeWorld, CandidateSource, ChildRule,
    EntryFilter, EntrySpawn, ExclusionRegion, STARTER_BIOMES_GRAPH, WaterField, WaterLevel, WaterSnap,
    claims_at,
};
use regia_procgen::data::{AttrColumn, GroundData, ShapeData};
use regia_procgen::exec::{SelfInfo, StaticSnapshot};
use regia_procgen::{CancelToken, Evaluator, ProcGraph, ProcNodeRegistry, RunInputs, RunResult};
use regia_terrain::TerrainMap;

/// The spec's § 3 example, verbatim.
const SPEC_EXAMPLE: &str = r#"BiomeDef(
    format_version: 1,
    name: "Pine forest",
    color: (0.15, 0.45, 0.2),           // debug display; also the default map key
    map_key: None,                      // Some((r, g, b)) 8-bit key in biome map textures; default from `color`
    priority: 20,                       // higher wins where areas overlap
    layers: [
        (
            name: "canopy",
            rank: 30,                   // higher-rank layers keep their spots in the cross-layer resolve (§ 6.4)
            sharing: Exclusive,         // Exclusive | Shared (Shared: neither removes nor is removed in § 6.4)
            source: Ground(spacing_m: 6.0, jitter: 0.9, noise: Some((scale_m: 60.0, octaves: 3, min: 0.2, max: 1.0))),
            channels: [ (name: "steep", rule: Slope(min_deg: 25.0, max_deg: 90.0)), (name: "flat", rule: Rest) ],
            entries: [
                (
                    name: "pine_tall",
                    spawn: Mesh("trees/pine_a.glb"),
                    weight: 3.0, channels: ["flat"], enabled: true,
                    filters: [ Altitude(min: 0.0, max: 1400.0, feather: 50.0), Slope(min_deg: 0.0, max_deg: 30.0, feather: 4.0) ],
                    footprint: (core_m: Some(0.6), spread_m: Some(3.5)),
                    placement: (upright: 1.0, scale: Range(0.8, 1.25), yaw: Random, sink_m: 0.15, offset: None),
                    render: (cast_shadows: true, draw_distance_m: 1500.0, fade_m: 100.0, collision: Simple),
                    children: [ (entry: "fern", count: (2, 5), radius_m: (1.0, 3.0), rate: 0.6, keep_clear_of_parent: Core) ],
                ),
                ( name: "fern", spawn: Mesh("plants/fern.glb"), weight: 0.0, channels: [],   // child-only (weight 0)
                  footprint: (core_m: Some(0.2), spread_m: Some(0.5)), placement: (upright: 0.3, scale: Range(0.7, 1.1), yaw: Random) ),
            ],
            max_child_depth: 2,
        ),
    ],
    grass: [ (class: "forest_floor", density: 0.6, outside: Keep) ],   // optional grass multipliers (§ 8)
)"#;

const GEN_GUID: Guid = Guid(0x1234_5678_9abc_def0_1122_3344_5566_7788);

/// The synthetic 512 m test terrain (spec 65 § 12): 257 × 257 samples, 2 m apart, a hill at
/// (120, 120), an east-west valley at z = -150 whose floor lies below the water level 0.
fn test_heights() -> Vec<f32> {
    let n = 257u32;
    (0..n * n)
        .map(|i| {
            let x = (i % n) as f32 * 2.0 - 256.0;
            let z = (i / n) as f32 * 2.0 - 256.0;
            let (u, v) = ((x - 120.0) / 100.0, (z - 120.0) / 100.0);
            let hill = (1.0 - u * u - v * v).max(0.0);
            let w = (z + 150.0) / 40.0;
            let valley = (1.0 - w * w).max(0.0);
            2.0 + 40.0 * hill * hill - 8.0 * valley
        })
        .collect()
}

fn test_map() -> TerrainMap {
    TerrainMap::from_heights(257, 257, test_heights(), 512.0, 1.0, 0.0).expect("test map")
}

/// A ramp `h = slope * x` (for the altitude filter).
fn ramp_map(slope: f32) -> TerrainMap {
    let n = 257u32;
    let h = (0..n * n)
        .map(|i| ((i % n) as f32 * 2.0 - 256.0) * slope)
        .collect();
    TerrainMap::from_heights(n, n, h, 512.0, 1.0, 0.0).expect("ramp map")
}

fn box_area(guid: u128, center: [f32; 2], half: [f32; 2], blend: f32, def: BiomeDef) -> BiomeAreaSnap {
    BiomeAreaSnap {
        guid: Guid(guid),
        name: format!("area{guid}"),
        outline: AreaOutline::Shape(box_shape(
            [center[0], 0.0, center[1]],
            [half[0], 100.0, half[1]],
            0.0,
        )),
        blend_m: blend,
        blend_noise: None,
        priority_offset: 0,
        preview_alone: false,
        biomes: vec![(format!("{}.biome.ron", def.name), Arc::new(def))],
    }
}

fn snapshot_with(map: &TerrainMap, bounds: ShapeData, world: BiomeWorld) -> Arc<StaticSnapshot> {
    let (lo, hi) = bounds.world_aabb();
    let g = GroundData::capture(map, [lo[0], lo[2], hi[0], hi[2]], 2);
    let mut world = world;
    world.sort();
    for a in &world.areas {
        for (p, d) in &a.biomes {
            world.defs.insert(p.clone(), d.clone());
        }
    }
    Arc::new(StaticSnapshot {
        self_info: Some(SelfInfo {
            guid: GEN_GUID,
            name: "gen".to_string(),
            bounds,
        }),
        ground: Some(Arc::new(g)),
        biome: Some(Arc::new(world)),
        ..StaticSnapshot::default()
    })
}

fn starter() -> ProcGraph {
    ProcGraph::from_ron(STARTER_BIOMES_GRAPH).expect("starter graph parses")
}

fn run_graph(g: &ProcGraph, snap: Arc<StaticSnapshot>, seed: u32) -> RunResult {
    let c = match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let mut inputs = RunInputs::new(snap, seed);
    inputs.keep_debug = true;
    match Evaluator::new().run(&c, &inputs, &CancelToken::new()) {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

/// Points on output 0 of node `id`, flattened over items: `(entry, position, attrs)`.
struct Pt {
    entry: String,
    pos: [f32; 3],
    depth: i64,
    core: f64,
    spread: f64,
}

fn node_points(r: &RunResult, id: u32) -> Vec<Pt> {
    let mut out = Vec::new();
    let Some(b) = r.debug.as_ref().and_then(|d| d.get(&id)).and_then(|v| v.first()) else {
        return out;
    };
    for item in &b.0 {
        let Some(p) = item.as_points() else { continue };
        let names = match p.attrs.get("entry") {
            Some(AttrColumn::Name(v)) => v.to_vec(),
            _ => vec![String::new(); p.len()],
        };
        let ints = |n: &str| match p.attrs.get(n) {
            Some(AttrColumn::Int(v)) => v.to_vec(),
            _ => vec![0; p.len()],
        };
        let floats = |n: &str| match p.attrs.get(n) {
            Some(AttrColumn::Float(v)) => v.to_vec(),
            _ => vec![0.0; p.len()],
        };
        let (depth, core, spread) = (ints("depth"), floats("core_m"), floats("spread_m"));
        for i in 0..p.len() {
            out.push(Pt {
                entry: names[i].clone(),
                pos: p.position[i],
                depth: depth[i],
                core: core[i],
                spread: spread[i],
            });
        }
    }
    out
}

/// The sum of a counter over every node.
fn counter(r: &RunResult, name: &str) -> u64 {
    r.report.nodes.iter().filter_map(|n| n.counters.get(name)).sum()
}

fn layer(name: &str, source: CandidateSource, entries: Vec<BiomeEntry>) -> BiomeLayer {
    BiomeLayer {
        source,
        entries,
        ..BiomeLayer::new(name)
    }
}

fn grid(spacing: f64) -> CandidateSource {
    CandidateSource::Ground {
        spacing_m: spacing,
        jitter: 0.0,
        noise: None,
    }
}

fn mesh_entry(name: &str, weight: f64, core: f32, spread: f32) -> BiomeEntry {
    BiomeEntry {
        weight,
        footprint: Footprint {
            core_m: Some(core),
            spread_m: Some(spread),
        },
        placement: Placement {
            yaw: Yaw::Fixed(0.0),
            ..Placement::default()
        },
        ..BiomeEntry::new(name, EntrySpawn::Mesh(format!("{name}.glb")))
    }
}

fn biome(name: &str, priority: i32, layers: Vec<BiomeLayer>) -> BiomeDef {
    BiomeDef {
        name: name.to_string(),
        priority,
        layers,
        ..BiomeDef::default()
    }
}

// ---------------------------------------------------------------------------------------------
// 1. Format
// ---------------------------------------------------------------------------------------------

#[test]
fn t1_the_spec_example_loads_round_trips_and_fills_defaults() {
    let def = BiomeDef::from_ron(SPEC_EXAMPLE).expect("spec example loads");
    assert_eq!(def.name, "Pine forest");
    assert_eq!(def.priority, 20);
    let l = &def.layers[0];
    assert_eq!(l.rank, 30);
    assert_eq!(l.max_child_depth, 2);
    let fern = &l.entries[1];
    assert!(fern.child_only());
    assert!(fern.enabled, "enabled defaults to true");
    assert_eq!(fern.render.draw_distance_m, 300.0, "render defaults");
    assert_eq!(fern.placement.sink_m, 0.0);
    assert_eq!(l.entries[0].children[0].keep_clear_of_parent, KeepClear::Core);
    assert!(!l.entries[0].children[0].scale_with_parent);
    // The default map key comes from the color.
    assert_eq!(def.key(), [38, 115, 51]);
    // Canonical text round-trips byte-identically.
    let text = def.to_ron();
    let again = BiomeDef::from_ron(&text).expect("canonical text loads");
    assert_eq!(again, def);
    assert_eq!(again.to_ron(), text);
    // A file naming only a name and one empty layer gets every default.
    let min = BiomeDef::from_ron("(name: \"m\", layers: [(name: \"a\")])").expect("minimal");
    assert_eq!(min.layers[0].source, CandidateSource::default());
    assert_eq!(min.layers[0].max_child_depth, 1);
}

#[test]
fn t1_a_child_naming_a_missing_entry_fails_with_biome_entry_unknown() {
    let text = SPEC_EXAMPLE.replace("(entry: \"fern\"", "(entry: \"fernn\"");
    let err = BiomeDef::from_ron(&text).expect_err("unknown child entry");
    assert_eq!(err.code(), "biome_entry_unknown");
    // Other faults are biome_invalid, all listed.
    let bad = SPEC_EXAMPLE
        .replace("weight: 3.0", "weight: -1.0")
        .replace("channels: [\"flat\"]", "channels: [\"nope\"]");
    match BiomeDef::from_ron(&bad) {
        Err(regia_procgen::biome::BiomeDefError::Invalid(list)) => {
            assert_eq!(list.len(), 2, "{list:?}");
            assert!(list.iter().all(|i| i.code == "biome_invalid"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert_eq!(
        BiomeDef::from_ron("(format_version: 9)")
            .expect_err("newer")
            .code(),
        "biome_invalid"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Claims
// ---------------------------------------------------------------------------------------------

#[test]
fn t2_claims_overlap_priority_blend_and_guid_ties() {
    let low = box_area(5, [0.0, 0.0], [100.0, 100.0], 8.0, biome("low", 10, vec![]));
    let high = box_area(9, [60.0, 0.0], [50.0, 50.0], 8.0, biome("high", 20, vec![]));
    let mut areas = vec![low.clone(), high.clone()];
    areas.sort_by_key(|a| a.guid);
    // Deep inside the overlap: the priority-20 area owns with weight 1.
    let c = claims_at(&areas, 60.0, 0.0);
    let owner = c.owner.expect("owner");
    assert_eq!(areas[owner.area].guid, Guid(9));
    assert_eq!(owner.weight, 1.0);
    assert!(c.second.is_none());
    // Across its band (x from 102 down to 110 is outside the low area; use z band inside both).
    for k in 0..=16 {
        let z = 42.0 + k as f32 * 0.5; // high's edge at z = 50, band 42..50
        let c = claims_at(&areas, 60.0, z);
        let o = c.owner.expect("owner");
        let total = o.weight + c.second.map_or(0.0, |s| s.weight);
        assert!((total - 1.0).abs() < 1e-6, "z {z}: {total}");
        if z < 50.0 {
            assert_eq!(areas[o.area].guid, Guid(9));
        }
    }
    // Equal priorities: the lower guid wins, every run and in any input order.
    let a = box_area(7, [0.0, 0.0], [50.0, 50.0], 0.0, biome("a", 10, vec![]));
    let b = box_area(3, [0.0, 0.0], [50.0, 50.0], 0.0, biome("b", 10, vec![]));
    for order in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
        let mut w = BiomeWorld {
            areas: order,
            ..BiomeWorld::default()
        };
        w.sort();
        let o = w.claims_at(1.0, 1.0).owner.expect("owner");
        assert_eq!(w.areas[o.area].guid, Guid(3));
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Weights
// ---------------------------------------------------------------------------------------------

#[test]
fn t3_entry_weights_three_to_one_over_20000_candidates() {
    let map = TerrainMap::from_heights(257, 257, vec![1.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let def = biome(
        "w",
        0,
        vec![layer(
            "l",
            grid(1.0),
            vec![mesh_entry("a", 3.0, 0.0, 0.0), mesh_entry("b", 1.0, 0.0, 0.0)],
        )],
    );
    let area = box_area(1, [100.0, 50.0], [100.0, 50.0], 0.0, def);
    let snap = snapshot_with(
        &map,
        box_shape([100.0, 0.0, 50.0], [100.0, 50.0, 50.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 7);
    assert_eq!(counter(&r, "w/l|candidates"), 20_000);
    let a = counter(&r, "w/l/a|assigned");
    let b = counter(&r, "w/l/b|assigned");
    eprintln!("t3: a {a}, b {b}");
    assert!(a.abs_diff(15_000) <= 300, "a = {a}");
    assert!(b.abs_diff(5_000) <= 300, "b = {b}");
    assert_eq!(counter(&r, "w/l/a|placed"), a, "nothing else removes them");
    assert_eq!(r.output.instance_count() as u64, a + b);
}

// ---------------------------------------------------------------------------------------------
// 4. Filters
// ---------------------------------------------------------------------------------------------

#[test]
fn t4_altitude_feather_thins_linearly_and_keeps_the_window() {
    // h = x; window max 50 m, feather 50 m: all kept below 50, none above 100, linear between.
    let map = ramp_map(1.0);
    let mut e = mesh_entry("e", 1.0, 0.0, 0.0);
    e.filters = vec![EntryFilter::Altitude {
        min: -1.0e9,
        max: 50.0,
        feather: 50.0,
    }];
    let def = biome("f", 0, vec![layer("l", grid(0.5), vec![e])]);
    let area = box_area(1, [60.0, 0.0], [60.0, 50.0], 0.0, def);
    let snap = snapshot_with(
        &map,
        box_shape([60.0, 0.0, 0.0], [60.0, 200.0, 50.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 11);
    let pts = node_points(&r, 4);
    // Candidates per 10 m bin: 20 columns × 200 rows.
    let per_bin = 20.0 * 200.0;
    for bin in 0..12 {
        let lo = bin as f32 * 10.0;
        let n = pts
            .iter()
            .filter(|p| p.pos[1] >= lo && p.pos[1] < lo + 10.0)
            .count() as f64;
        let mid = f64::from(lo) + 5.0;
        let expect = if mid <= 50.0 {
            1.0
        } else if mid >= 100.0 {
            0.0
        } else {
            1.0 - (mid - 50.0) / 50.0
        };
        let frac = n / per_bin;
        assert!((frac - expect).abs() <= 0.03, "bin {bin}: {frac} vs {expect}");
        if mid <= 50.0 {
            assert_eq!(n, per_bin, "inside the window nothing is removed");
        }
        if mid > 100.0 {
            assert_eq!(n, 0.0, "beyond the feather nothing is left");
        }
    }
    assert!(counter(&r, "f/l/e|filter:0:Altitude") > 0);
}

#[test]
fn t4_water_distance_is_negative_inside_the_lake() {
    let map = test_map();
    let g = GroundData::capture(&map, [-256.0, -256.0, 256.0, 256.0], 2);
    let water = WaterSnap {
        levels: vec![WaterLevel {
            level: 0.0,
            rect: None,
        }],
        ribbons: Vec::new(),
    };
    let f = WaterField::build(&g, &water).expect("field");
    // The valley floor at z = -150 is 6 m under water; its shore (h = 0) is at |z + 150| = 34.6 m.
    assert!(f.at(0.0, -150.0) < -30.0, "{}", f.at(0.0, -150.0));
    let shore = f.at(0.0, -100.0);
    assert!((13.0..=18.0).contains(&shore), "{shore}");
    assert!(f.at(0.0, 100.0) > 200.0);
    // A filter keeps only dry land within 30 m of the water.
    let mut e = mesh_entry("reed", 1.0, 0.0, 0.0);
    e.filters = vec![EntryFilter::WaterDistance {
        min_m: 0.0,
        max_m: 30.0,
        feather: 0.0,
    }];
    let def = biome("shore", 0, vec![layer("l", grid(2.0), vec![e])]);
    let area = box_area(1, [0.0, -150.0], [100.0, 100.0], 0.0, def);
    let snap = snapshot_with(
        &map,
        box_shape([0.0, 0.0, -150.0], [100.0, 100.0, 100.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            water,
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 3);
    let pts = node_points(&r, 4);
    assert!(!pts.is_empty());
    for p in &pts {
        let d = f.at(p.pos[0], p.pos[2]);
        assert!(
            (0.0..=30.0).contains(&d),
            "point at {:?} is {d} m from water",
            p.pos
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 5. Self spacing
// ---------------------------------------------------------------------------------------------

#[test]
fn t5_no_two_instances_of_an_entry_closer_than_their_spreads() {
    let map = test_map();
    let mut e = mesh_entry("tree", 1.0, 0.3, 1.5);
    e.placement.scale = ScaleRule::Range(0.6, 1.4);
    let def = biome(
        "s",
        0,
        vec![layer(
            "l",
            CandidateSource::Ground {
                spacing_m: 1.0,
                jitter: 0.9,
                noise: None,
            },
            vec![e],
        )],
    );
    let area = box_area(1, [0.0, 0.0], [60.0, 60.0], 0.0, def);
    let snap = snapshot_with(
        &map,
        box_shape([0.0; 3], [60.0, 100.0, 60.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 5);
    let pts = node_points(&r, 4);
    assert!(pts.len() > 300, "{}", pts.len());
    for (i, a) in pts.iter().enumerate() {
        for b in &pts[i + 1..] {
            let (dx, dz) = (a.pos[0] - b.pos[0], a.pos[2] - b.pos[2]);
            let d = f64::from((dx * dx + dz * dz).sqrt());
            assert!(
                d >= a.spread + b.spread - 1e-4,
                "{d} < {} + {}",
                a.spread,
                b.spread
            );
        }
    }
    assert!(counter(&r, "s/l/tree|spaced_out") > 0);
}

// ---------------------------------------------------------------------------------------------
// 6. Children
// ---------------------------------------------------------------------------------------------

/// One `parent` entry (core 1 m) on a grid, with one child rule to `kid` or to itself.
fn children_world(depth: u32, spacing: f64, rule: ChildRule) -> BiomeWorld {
    let mut parent = mesh_entry("parent", 1.0, 1.0, 0.0);
    let kid = mesh_entry("kid", 0.0, 0.0, 0.0);
    parent.children = vec![rule];
    let mut l = layer("l", grid(spacing), vec![parent, kid]);
    l.max_child_depth = depth;
    let def = biome("c", 0, vec![l]);
    BiomeWorld {
        areas: vec![box_area(1, [0.0, 0.0], [240.0, 240.0], 0.0, def)],
        ..BiomeWorld::default()
    }
}

fn rule(entry: &str, count: u32, radius: (f32, f32), rate: f64, keep: KeepClear) -> ChildRule {
    ChildRule {
        entry: entry.into(),
        count: (count, count),
        radius_m: radius,
        rate,
        keep_clear_of_parent: keep,
        scale_with_parent: false,
        graph: None,
    }
}

fn children_run(w: BiomeWorld) -> (RunResult, Vec<Pt>) {
    let map = TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let snap = snapshot_with(&map, box_shape([0.0; 3], [240.0, 50.0, 240.0], 0.0), w);
    let r = run_graph(&starter(), snap, 21);
    let pts = node_points(&r, 4);
    (r, pts)
}

#[test]
fn t6_depth_cap_zero_gives_no_children() {
    let (_, pts) = children_run(children_world(
        0,
        20.0,
        rule("kid", 2, (1.0, 2.0), 1.0, KeepClear::None),
    ));
    assert!(!pts.is_empty());
    assert!(pts.iter().all(|p| p.depth == 0 && p.entry.ends_with("/parent")));
}

#[test]
fn t6_a_self_referencing_child_with_depth_3_stops_after_3_generations() {
    // Roots 30 m apart; one child 2 m away per generation; nothing spaces them out.
    let (_, pts) = children_run(children_world(
        3,
        30.0,
        rule("parent", 1, (2.0, 2.0), 1.0, KeepClear::None),
    ));
    let roots = pts.iter().filter(|p| p.depth == 0).count();
    assert!(roots > 0);
    for d in 1..=3 {
        assert_eq!(
            pts.iter().filter(|p| p.depth == d).count(),
            roots,
            "generation {d}"
        );
    }
    assert_eq!(pts.iter().map(|p| p.depth).max(), Some(3));
}

#[test]
fn t6_rate_half_gives_children_to_half_the_parents() {
    let (r, pts) = children_run(children_world(
        1,
        5.0,
        rule("kid", 1, (1.0, 2.0), 0.5, KeepClear::None),
    ));
    let parents = pts.iter().filter(|p| p.depth == 0).count() as f64;
    let kids = pts.iter().filter(|p| p.depth == 1).count() as f64;
    eprintln!("t6 rate: {kids} children of {parents} parents");
    assert!(parents >= 9000.0);
    assert!((kids / parents - 0.5).abs() <= 0.02, "{}", kids / parents);
    assert_eq!(counter(&r, "c/l/kid|children") as f64, kids);
}

#[test]
fn t6_keep_clear_core_leaves_no_child_inside_a_core() {
    // Four children per parent at radii 0.71, 1.22, 1.58, 1.87 m: the first falls inside the core.
    let (r, pts) = children_run(children_world(
        1,
        20.0,
        rule("kid", 4, (0.0, 2.0), 1.0, KeepClear::Core),
    ));
    let parents: Vec<&Pt> = pts.iter().filter(|p| p.depth == 0).collect();
    let kids: Vec<&Pt> = pts.iter().filter(|p| p.depth == 1).collect();
    assert!(!kids.is_empty());
    for k in &kids {
        for p in &parents {
            let (dx, dz) = (k.pos[0] - p.pos[0], k.pos[2] - p.pos[2]);
            assert!(f64::from(dx * dx + dz * dz).sqrt() >= p.core - 1e-5);
        }
    }
    assert!(
        counter(&r, "c/l/kid|spaced_out") > 0,
        "some children fell inside the 1 m core"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. Resolve
// ---------------------------------------------------------------------------------------------

fn resolve_run(rock_rank: i32, bush_rank: i32, bush_sharing: Sharing) -> (RunResult, Vec<Pt>, Vec<Pt>) {
    let map = TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let mut rocks = layer("rocks", grid(3.0), vec![mesh_entry("rock", 1.0, 1.0, 0.0)]);
    rocks.rank = rock_rank;
    let mut bushes = layer(
        "bushes",
        CandidateSource::Ground {
            spacing_m: 1.3,
            jitter: 0.5,
            noise: None,
        },
        vec![mesh_entry("bush", 1.0, 0.5, 0.0)],
    );
    bushes.rank = bush_rank;
    bushes.sharing = bush_sharing;
    let def = biome("r", 0, vec![rocks, bushes]);
    let snap = snapshot_with(
        &map,
        box_shape([0.0; 3], [40.0, 50.0, 40.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [40.0, 40.0], 0.0, def)],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 9);
    let before = node_points(&r, 4);
    let after = node_points(&r, 5);
    (r, before, after)
}

#[test]
fn t7_rocks_remove_overlapping_bush_cores_but_not_vice_versa() {
    let (r, before, after) = resolve_run(40, 10, Sharing::Exclusive);
    let rocks_before = before.iter().filter(|p| p.entry.ends_with("/rock")).count();
    let rocks_after: Vec<&Pt> = after.iter().filter(|p| p.entry.ends_with("/rock")).collect();
    assert_eq!(rocks_after.len(), rocks_before, "no rock is removed");
    let bushes_after: Vec<&Pt> = after.iter().filter(|p| p.entry.ends_with("/bush")).collect();
    assert!(bushes_after.len() < before.iter().filter(|p| p.entry.ends_with("/bush")).count());
    for b in &bushes_after {
        for k in &rocks_after {
            let (dx, dz) = (b.pos[0] - k.pos[0], b.pos[2] - k.pos[2]);
            assert!(f64::from(dx * dx + dz * dz).sqrt() >= b.core + k.core - 1e-5);
        }
    }
    assert!(counter(&r, "r/bushes/bush|resolved_out") > 0);
    assert_eq!(counter(&r, "r/rocks/rock|resolved_out"), 0);
}

#[test]
fn t7_shared_layers_and_equal_ranks_keep_both() {
    for (rank, sharing) in [(10, Sharing::Shared), (40, Sharing::Exclusive)] {
        let (_, before, after) = resolve_run(40, rank, sharing);
        assert_eq!(before.len(), after.len(), "rank {rank} {sharing:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// 8. Exclusion
// ---------------------------------------------------------------------------------------------

#[test]
fn t8_a_tagged_box_removes_every_point_inside_it() {
    let map = test_map();
    let def = biome(
        "x",
        0,
        vec![layer("l", grid(1.0), vec![mesh_entry("e", 1.0, 0.0, 0.0)])],
    );
    let zone = box_shape([10.0, 0.0, 5.0], [8.0, 100.0, 6.0], 0.0);
    let snap = snapshot_with(
        &map,
        box_shape([0.0; 3], [40.0, 100.0, 40.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [40.0, 40.0], 0.0, def)],
            exclusions: vec![ExclusionRegion::Shape(zone)],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 1);
    let before = node_points(&r, 5);
    let after = node_points(&r, 6);
    let inside = |p: &Pt| (p.pos[0] - 10.0).abs() < 8.0 && (p.pos[2] - 5.0).abs() < 6.0;
    assert!(before.iter().any(inside));
    assert!(!after.iter().any(inside));
    assert_eq!(
        before.iter().filter(|p| !inside(p)).count(),
        after.len(),
        "nothing outside is removed"
    );
    assert_eq!(counter(&r, "x/l/e|excluded") as usize, before.len() - after.len());
}

// ---------------------------------------------------------------------------------------------
// 9. Determinism
// ---------------------------------------------------------------------------------------------

/// Three biomes on the test map, every stage in use.
fn full_world() -> BiomeWorld {
    let forest = BiomeDef::from_ron(SPEC_EXAMPLE).expect("forest");
    let mut meadow_flowers = mesh_entry("flower", 2.0, 0.05, 0.3);
    meadow_flowers.placement = Placement {
        upright: 0.0,
        yaw: Yaw::AlongSlope,
        scale: ScaleRule::Range(0.8, 1.2),
        scale_from_source: false,
        sink_m: 0.02,
        offset: Some(((0.0, 0.1, 0.0), (0.0, 90.0, 0.0), (1.0, 1.0, 1.0))),
    };
    let mut meadow_rocks = mesh_entry("rock", 1.0, 0.8, 1.0);
    meadow_rocks.channels = vec!["steep".into()];
    let mut ml = layer(
        "ground",
        CandidateSource::Poisson { min_dist_m: 1.5 },
        vec![
            meadow_flowers,
            meadow_rocks,
            BiomeEntry::new("gap", EntrySpawn::None),
        ],
    );
    ml.channels = vec![
        BiomeChannel {
            name: "steep".into(),
            rule: ChannelRule::Slope {
                min_deg: 12.0,
                max_deg: 90.0,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    ml.rank = 10;
    let mut meadow = biome("Meadow", 10, vec![ml]);
    meadow.grass = vec![BiomeGrass {
        class: "meadow".into(),
        density: 0.8,
        outside: GrassOutside::Keep,
    }];
    let mut reeds = mesh_entry("reed", 1.0, 0.1, 0.4);
    reeds.filters = vec![EntryFilter::WaterDistance {
        min_m: -2.0,
        max_m: 15.0,
        feather: 5.0,
    }];
    let river = biome("Riverbed", 30, vec![layer("reeds", grid(2.0), vec![reeds])]);
    let mut fa = box_area(0x10, [150.0, 150.0], [80.0, 80.0], 8.0, forest);
    fa.blend_noise = Some((30.0, 0.5));
    BiomeWorld {
        areas: vec![
            fa,
            box_area(0x20, [0.0, 0.0], [250.0, 250.0], 8.0, meadow),
            box_area(0x30, [0.0, -150.0], [250.0, 45.0], 6.0, river),
        ],
        exclusions: vec![ExclusionRegion::Corridor {
            points: vec![[-250.0, 40.0], [250.0, 60.0]],
            half_width: 4.0,
        }],
        water: WaterSnap {
            levels: vec![WaterLevel {
                level: 0.0,
                rect: None,
            }],
            ribbons: Vec::new(),
        },
        ..BiomeWorld::default()
    }
}

fn full_run(seed: u32) -> RunResult {
    let snap = snapshot_with(
        &test_map(),
        box_shape([0.0; 3], [250.0, 100.0, 250.0], 0.0),
        full_world(),
    );
    run_graph(&starter(), snap, seed)
}

/// The recorded output hash of the starter graph over the three-biome test map (Linux x86_64,
/// Rust 1.99). Other platforms must reproduce it (lead: Windows, Android aarch64).
const RECORDED_HASH: &str = "RECORD_ME";

#[test]
fn t9_full_starter_graph_hash_is_recorded_and_repeatable() {
    let a = full_run(42);
    let b = full_run(42);
    assert_eq!(a.output_hash, b.output_hash);
    let h = hex(&a.output_hash);
    eprintln!("t9 hash {h}; instances {}", a.output.instance_count());
    let mut entries: BTreeMap<String, u64> = BTreeMap::new();
    for n in &a.report.nodes {
        for (k, v) in &n.counters {
            if k.ends_with("|placed") {
                *entries.entry(k.clone()).or_default() += v;
            }
        }
    }
    eprintln!("t9 placed {entries:?}");
    for name in [
        "Pine forest/canopy/pine_tall|placed",
        "Pine forest/canopy/fern|placed",
        "Meadow/ground/flower|placed",
        "Meadow/ground/rock|placed",
        "Riverbed/reeds/reed|placed",
    ] {
        assert!(entries.get(name).copied().unwrap_or(0) > 0, "{name}: {entries:?}");
    }
    assert!(a.output.masks.contains_key("grass_multiplier/meadow"));
    assert!(a.output.masks.contains_key("grass_multiplier/forest_floor"));
    assert_ne!(hex(&full_run(43).output_hash), h, "the seed changes the output");
    if RECORDED_HASH != "RECORD_ME" {
        assert_eq!(h, RECORDED_HASH);
    }
}

// ---------------------------------------------------------------------------------------------
// 11. Grass multipliers
// ---------------------------------------------------------------------------------------------

#[test]
fn t11_meadow_multiplier_inside_and_outside() {
    for (outside, expect_out) in [(GrassOutside::Keep, 1.0f32), (GrassOutside::Clear, 0.0)] {
        let mut meadow = biome("meadow", 0, vec![]);
        meadow.grass = vec![BiomeGrass {
            class: "meadow".into(),
            density: 0.8,
            outside,
        }];
        // The meadow covers the half x < 0 of a 512 m map.
        let mut w = BiomeWorld {
            areas: vec![box_area(1, [-128.0, 0.0], [128.0, 256.0], 8.0, meadow)],
            ..BiomeWorld::default()
        };
        w.sort();
        let masks = regia_procgen::biome::grass::grass_masks(&w, [-256.0, -256.0, 256.0, 256.0], 4.0);
        assert_eq!(masks.len(), 1);
        let (class, m) = &masks[0];
        assert_eq!(class, "meadow");
        assert!((m.sample(-128.0, 0.0) - 0.8).abs() < 1e-6);
        assert!((m.sample(-30.0, 100.0) - 0.8).abs() < 1e-6, "beyond the 8 m band");
        assert_eq!(m.sample(100.0, 0.0), expect_out);
        assert_eq!(m.sample(10.0, -50.0), expect_out);
    }
}

// ---------------------------------------------------------------------------------------------
// Spec 65 § 13 budget (hand-run): a 2 km × 2 km map, 3 biomes, 6 layers, 20 entries.
// cargo test -p regia_procgen --release --test biomes -- --ignored --nocapture perf_
// ---------------------------------------------------------------------------------------------

/// The § 13 budget world: 3 biomes, 6 layers, 20 entries over 2 km.
fn perf_2km_snapshot() -> Arc<StaticSnapshot> {
    // 1025 × 1025 samples over 2048 m: rolling hills, a valley under water at z = 600.
    let n = 1025u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let x = (i % n) as f32 * 2.0 - 1024.0;
            let z = (i / n) as f32 * 2.0 - 1024.0;
            let (u, v) = (x / 300.0, z / 260.0);
            let hills = 25.0 * (u - u.floor() - 0.5).abs() * (v - v.floor() - 0.5).abs() * 4.0;
            let w = (z - 600.0) / 80.0;
            hills - 10.0 * (1.0 - w * w).max(0.0) + 3.0
        })
        .collect();
    let map = TerrainMap::from_heights(n, n, heights, 2048.0, 1.0, 0.0).expect("map");
    let e = |name: &str, w: f64, core: f32, spread: f32| {
        let mut x = mesh_entry(name, w, core, spread);
        x.placement.yaw = Yaw::Random;
        x.placement.scale = ScaleRule::Range(0.8, 1.2);
        x
    };
    let mut canopy = layer(
        "canopy",
        CandidateSource::Ground {
            spacing_m: 5.0,
            jitter: 0.9,
            noise: None,
        },
        vec![
            e("pine", 3.0, 0.5, 2.0),
            e("fir", 2.0, 0.5, 2.2),
            e("birch", 1.0, 0.3, 1.6),
        ],
    );
    canopy.rank = 30;
    canopy.entries[0].children = vec![ChildRule {
        entry: "fern".into(),
        count: (2, 4),
        radius_m: (1.0, 3.0),
        rate: 0.5,
        keep_clear_of_parent: KeepClear::Core,
        scale_with_parent: false,
        graph: None,
    }];
    canopy.entries.push(e("fern", 0.0, 0.1, 0.4));
    canopy.max_child_depth = 1;
    let mut under = layer(
        "understory",
        CandidateSource::Poisson { min_dist_m: 2.5 },
        vec![
            e("bush", 2.0, 0.3, 0.8),
            e("log", 0.5, 0.4, 1.0),
            e("stump", 0.5, 0.2, 0.4),
        ],
    );
    under.rank = 10;
    let forest = biome("Forest", 20, vec![canopy, under]);
    let mut flowers = layer(
        "flowers",
        CandidateSource::Ground {
            spacing_m: 2.2,
            jitter: 0.8,
            noise: Some(regia_procgen::biome::def::SourceNoise {
                scale_m: 80.0,
                octaves: 3,
                min: 0.3,
                max: 1.0,
            }),
        },
        vec![
            e("daisy", 3.0, 0.05, 0.3),
            e("poppy", 2.0, 0.05, 0.3),
            e("clover", 2.0, 0.05, 0.3),
            e("tuft", 4.0, 0.05, 0.4),
        ],
    );
    flowers.sharing = Sharing::Shared;
    let mut rocks = layer(
        "rocks",
        grid(9.0),
        vec![
            e("boulder", 1.0, 1.0, 2.0),
            e("rock", 3.0, 0.5, 1.0),
            e("pebble", 4.0, 0.2, 0.4),
        ],
    );
    rocks.rank = 40;
    let mut meadow = biome("Meadow", 10, vec![flowers, rocks]);
    meadow.grass = vec![BiomeGrass {
        class: "meadow".into(),
        density: 0.8,
        outside: GrassOutside::Keep,
    }];
    let mut reed = e("reed", 3.0, 0.1, 0.5);
    reed.filters = vec![EntryFilter::WaterDistance {
        min_m: -3.0,
        max_m: 20.0,
        feather: 6.0,
    }];
    let mut sedge = e("sedge", 1.0, 0.1, 0.5);
    sedge.filters = reed.filters.clone();
    let mut shore = layer("shore", grid(1.6), vec![reed, sedge]);
    shore.rank = 20;
    let mut gravel = layer(
        "gravel",
        grid(6.0),
        vec![e("gravel", 1.0, 0.3, 0.8), e("drift", 0.3, 0.4, 1.2)],
    );
    gravel.rank = 25;
    let river = biome("Riverbed", 30, vec![shore, gravel]);
    let world = BiomeWorld {
        areas: vec![
            box_area(1, [0.0, -500.0], [1000.0, 500.0], 16.0, forest),
            box_area(2, [0.0, 0.0], [1020.0, 1020.0], 8.0, meadow),
            box_area(3, [0.0, 600.0], [1020.0, 140.0], 10.0, river),
        ],
        water: WaterSnap {
            levels: vec![WaterLevel {
                level: 0.0,
                rect: None,
            }],
            ribbons: Vec::new(),
        },
        ..BiomeWorld::default()
    };
    snapshot_with(&map, box_shape([0.0; 3], [1020.0, 200.0, 1020.0], 0.0), world)
}

/// The § 13 budget run's inputs (limits raised for 600 000 instances).
fn perf_2km_inputs(snap: Arc<StaticSnapshot>) -> RunInputs {
    let mut inputs = RunInputs::new(snap, 5);
    inputs.limits.points_per_item = 50_000_000;
    inputs.limits.total_elements = 200_000_000;
    inputs.limits.memory_bytes = 64 << 30;
    inputs
}

#[test]
#[ignore = "perf: hand-run in release"]
fn perf_2km_three_biomes_full_bake() {
    let snap = perf_2km_snapshot();
    let c = starter()
        .compile(&ProcNodeRegistry::with_builtins())
        .expect("compile");
    let inputs = perf_2km_inputs(snap);
    let t = std::time::Instant::now();
    let r = Evaluator::new()
        .run(&c, &inputs, &CancelToken::new())
        .expect("run");
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "perf: {} instances, {ms:.0} ms total, output hash {}",
        r.output.instance_count(),
        hex(&r.output_hash)
    );
    for n in &r.report.nodes {
        eprintln!(
            "  node {} {}: {:.0} ms, {} elements",
            n.id, n.ty, n.ms, n.elements
        );
    }
}

/// The § 13 budget world one layer at a time on a serial schedule: what each `biome.layer` job costs
/// alone (the slowest bounds the node on a pool; ADR-7252).
/// `cargo test -p regia_procgen --release --test biomes -- --ignored --nocapture perf_2km_layer`
#[test]
#[ignore = "perf: hand-run in release"]
fn perf_2km_layer_jobs_one_at_a_time() {
    let snap = perf_2km_snapshot();
    for name in ["canopy", "understory", "flowers", "rocks", "shore", "gravel"] {
        let text = STARTER_BIOMES_GRAPH.replace(
            "4: (ty: \"biome.layer\", settings: {}",
            &format!("4: (ty: \"biome.layer\", settings: {{ \"layers\": [\"{name}\"] }}"),
        );
        assert_ne!(
            text, STARTER_BIOMES_GRAPH,
            "the starter graph names node 4 biome.layer"
        );
        let c = ProcGraph::from_ron(&text)
            .expect("graph")
            .compile(&ProcNodeRegistry::with_builtins())
            .expect("compile");
        let r = Evaluator::new()
            .with_parallelism(regia_procgen::exec::Parallelism::Serial)
            .run(&c, &perf_2km_inputs(snap.clone()), &CancelToken::new())
            .expect("run");
        let line = r.report.nodes.iter().find(|n| n.id == 4).expect("layer line");
        eprintln!("perf layer {name}: {:.0} ms, {} elements", line.ms, line.elements);
    }
}

// ---------------------------------------------------------------------------------------------
// Graph-driven stages, random channels and map areas
// ---------------------------------------------------------------------------------------------

fn sub(
    nodes: Vec<(u32, regia_procgen::NodeEntry)>,
    edges: &[(u32, &str, u32, &str)],
    ins: &[(&str, regia_procgen::KindSet)],
    outs: &[(&str, regia_procgen::KindSet)],
) -> String {
    let pin = |(n, k): &(&str, regia_procgen::KindSet)| regia_procgen::graph::GraphPin {
        name: (*n).to_string(),
        kinds: *k,
    };
    let mut g = common::graph(nodes, edges);
    g.inputs = ins.iter().map(pin).collect();
    g.outputs = outs.iter().map(pin).collect();
    g.to_ron()
}

#[test]
fn graph_sources_filters_children_and_random_channels() {
    use common::node;
    use regia_procgen::KindSet;
    let mut graphs = BTreeMap::new();
    graphs.insert(
        "src.pgraph.ron".to_string(),
        sub(
            vec![
                (1, node("graph.input", &[("pin", "\"region\"")])),
                (2, node("sample.region", &[("spacing_m", "4.0")])),
                (3, node("graph.output", &[("pin", "\"out\"")])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
            &[("region", KindSet::SPATIAL)],
            &[("out", KindSet::POINTS)],
        ),
    );
    graphs.insert(
        "mark.pgraph.ron".to_string(),
        sub(
            vec![
                (1, node("graph.input", &[("pin", "\"in\"")])),
                (
                    2,
                    node("attr.set", &[("target", "\"px\""), ("from", "\"position:x\"")]),
                ),
                (3, node("graph.output", &[("pin", "\"out\"")])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
            &[("in", KindSet::POINTS)],
            &[("out", KindSet::POINTS)],
        ),
    );
    graphs.insert(
        "kids.pgraph.ron".to_string(),
        sub(
            vec![
                (
                    1,
                    node(
                        "gen.points",
                        &[(
                            "points",
                            "[(position: (1.0, 0.0, 0.0)), (position: (-1.0, 0.0, 0.0))]",
                        )],
                    ),
                ),
                (2, node("graph.output", &[("pin", "\"out\"")])),
            ],
            &[(1, "out", 2, "in")],
            &[("parent", KindSet::POINTS)],
            &[("out", KindSet::POINTS)],
        ),
    );
    let mut a = mesh_entry("a", 1.0, 0.0, 0.0);
    a.channels = vec!["lucky".into()];
    a.filters = vec![EntryFilter::Graph {
        graph: "mark.pgraph.ron".into(),
        attr: "px".into(),
        min: -1.0e9,
        max: 0.0,
        feather: 0.0,
    }];
    a.children = vec![ChildRule {
        entry: "b".into(),
        count: (1, 1),
        radius_m: (0.0, 0.0),
        rate: 1.0,
        keep_clear_of_parent: KeepClear::None,
        scale_with_parent: false,
        graph: Some("kids.pgraph.ron".into()),
    }];
    let mut b = mesh_entry("b", 1.0, 0.0, 0.0);
    b.channels = vec!["rest".into()];
    let mut l = layer(
        "l",
        CandidateSource::Graph {
            graph: "src.pgraph.ron".into(),
            params: BTreeMap::new(),
        },
        vec![a, b],
    );
    l.channels = vec![
        BiomeChannel {
            name: "lucky".into(),
            rule: ChannelRule::Random { fraction: 0.25 },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("g", 0, vec![l]);
    let map = TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let snap = snapshot_with(
        &map,
        box_shape([0.0; 3], [40.0, 50.0, 40.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [40.0, 40.0], 0.0, def)],
            ..BiomeWorld::default()
        },
    );
    let c = starter()
        .compile(&ProcNodeRegistry::with_builtins())
        .expect("compile");
    let mut inputs = RunInputs::new(snap, 8).with_graphs(graphs);
    inputs.keep_debug = true;
    let r = Evaluator::new()
        .run(&c, &inputs, &CancelToken::new())
        .expect("run");
    assert_eq!(counter(&r, "g/l|candidates"), 400);
    let assigned_a = counter(&r, "g/l/a|assigned");
    assert!(assigned_a.abs_diff(100) <= 30, "{assigned_a}");
    assert_eq!(counter(&r, "g/l/b|assigned"), 400 - assigned_a);
    let pts = node_points(&r, 4);
    let roots_a: Vec<&Pt> = pts
        .iter()
        .filter(|p| p.depth == 0 && p.entry.ends_with("/a"))
        .collect();
    assert!(!roots_a.is_empty());
    assert!(
        roots_a.iter().all(|p| p.pos[0] <= 0.0),
        "the graph filter keeps x <= 0"
    );
    assert_eq!(
        counter(&r, "g/l/a|filter:0:Graph") + roots_a.len() as u64,
        assigned_a
    );
    let kids: Vec<&Pt> = pts.iter().filter(|p| p.depth == 1).collect();
    assert_eq!(
        kids.len(),
        2 * roots_a.len(),
        "two children per parent from the child graph"
    );
    for k in &kids {
        let near = roots_a.iter().any(|p| {
            let (dx, dz) = (k.pos[0] - p.pos[0], k.pos[2] - p.pos[2]);
            ((dx * dx + dz * dz).sqrt() - 1.0).abs() < 1e-3
        });
        assert!(near, "each child sits 1 m from a parent");
    }
}

#[test]
fn map_areas_place_each_biome_on_its_key() {
    use regia_procgen::biome::{KeyImage, KeyTables};
    let left = biome(
        "left",
        0,
        vec![layer("l", grid(2.0), vec![mesh_entry("x", 1.0, 0.0, 0.0)])],
    );
    let right = BiomeDef {
        map_key: Some((0, 0, 255)),
        ..biome(
            "right",
            0,
            vec![layer("l", grid(2.0), vec![mesh_entry("y", 1.0, 0.0, 0.0)])],
        )
    };
    // 32 × 32 texels over [-64, 64]²: left half the left biome's key, right half the right biome's.
    let (w, h) = (32u32, 32u32);
    let keys = (0..w * h)
        .map(|i| if i % w < w / 2 { left.key() } else { [0, 0, 255] })
        .collect();
    let image = Arc::new(KeyImage {
        width: w,
        height: h,
        keys,
    });
    let tables = Arc::new(KeyTables::build(&image, &[left.key(), right.key()]));
    let area = BiomeAreaSnap {
        guid: Guid(7),
        name: "map".into(),
        outline: AreaOutline::Map {
            rect: [-64.0, -64.0, 64.0, 64.0],
            image,
            tables,
        },
        blend_m: 0.0,
        blend_noise: None,
        priority_offset: 0,
        preview_alone: false,
        biomes: vec![
            ("left.biome.ron".into(), Arc::new(left)),
            ("right.biome.ron".into(), Arc::new(right)),
        ],
    };
    let map = TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let snap = snapshot_with(
        &map,
        box_shape([0.0; 3], [64.0, 50.0, 64.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 2);
    let pts = node_points(&r, 7);
    let xs: Vec<&Pt> = pts.iter().filter(|p| p.entry == "left/l/x").collect();
    let ys: Vec<&Pt> = pts.iter().filter(|p| p.entry == "right/l/y").collect();
    assert_eq!(xs.len(), 32 * 64, "every left cell");
    assert_eq!(ys.len(), 32 * 64, "every right cell");
    assert!(xs.iter().all(|p| p.pos[0] < 0.0));
    assert!(ys.iter().all(|p| p.pos[0] > 0.0));
}

/// Painted weights replace procedural ones in proportion to their coverage, as the terrain's material
/// does: an area painted `high` over ground the bands call `low` takes the `high` channel.
#[test]
fn terrain_layer_rules_read_painted_weights() {
    use regia_procgen::biome::PaintedLayers;
    use regia_terrain::{LayerDesc, LayerWeightMap, TerrainLayers};
    let band = |name: &str, lo: f32, hi: f32| LayerDesc {
        name: name.into(),
        slope_max: 1.6,
        altitude_min: lo,
        altitude_max: hi,
        ..LayerDesc::ground()
    };
    let layers = TerrainLayers {
        layers: vec![band("low", -1000.0, 1000.0), band("high", 2000.0, 3000.0)],
        ..TerrainLayers::default()
    };
    let map = ramp_map(0.0);
    // Paint `high` over z > 0 (half the rows).
    let (w, h) = (map.width(), map.height());
    let mut painted = LayerWeightMap::new(w, h, 2).expect("map");
    for r in h / 2 + 1..h {
        for c in 0..w {
            painted.set(1, c, r, 255);
        }
    }
    let mut hi = mesh_entry("h", 1.0, 0.0, 0.0);
    hi.channels = vec!["high".into()];
    let mut lo = mesh_entry("l", 1.0, 0.0, 0.0);
    lo.channels = vec!["rest".into()];
    let mut lay = layer("l", grid(1.0), vec![hi, lo]);
    lay.channels = vec![
        BiomeChannel {
            name: "high".into(),
            rule: ChannelRule::TerrainLayer {
                name: "high".into(),
                min_weight: 0.5,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("t", 0, vec![lay]);
    let bounds = box_shape([0.0, 0.0, 0.0], [20.0, 200.0, 20.0], 0.0);
    let run = |painted_layers: Option<PaintedLayers>| {
        let snap = snapshot_with(
            &map,
            bounds,
            BiomeWorld {
                areas: vec![box_area(1, [0.0, 0.0], [20.0, 20.0], 0.0, def.clone())],
                terrain_layers: Some(Arc::new(layers.clone())),
                painted_layers,
                ..BiomeWorld::default()
            },
        );
        let r = run_graph(&starter(), snap, 4);
        node_points(&r, 4)
    };
    let unpainted = run(None);
    assert!(unpainted.iter().all(|p| p.entry == "t/l/l"), "the bands say low");
    let pts = run(Some(PaintedLayers::new(Arc::new(painted), map.world_width())));
    let (hs, ls): (Vec<&Pt>, Vec<&Pt>) = pts.iter().partition(|p| p.entry == "t/l/h");
    assert!(!hs.is_empty() && !ls.is_empty());
    assert!(hs.iter().all(|p| p.pos[2] > -2.0), "painted side only");
    assert!(ls.iter().all(|p| p.pos[2] < 2.0), "unpainted side keeps the band");
}

#[test]
fn terrain_layer_channels_and_filters_read_the_layer_stack() {
    use regia_terrain::{LayerDesc, TerrainLayers};
    let band = |name: &str, lo: f32, hi: f32| LayerDesc {
        name: name.into(),
        slope_max: 1.6,
        altitude_min: lo,
        altitude_max: hi,
        ..LayerDesc::ground()
    };
    let layers = TerrainLayers {
        // Overlapping bands: both layers weigh in between 40 and 60 m.
        layers: vec![band("low", -1000.0, 60.0), band("high", 40.0, 1000.0)],
        ..TerrainLayers::default()
    };
    let mut h = mesh_entry("h", 1.0, 0.0, 0.0);
    h.channels = vec!["high".into()];
    h.filters = vec![EntryFilter::TerrainLayer {
        name: "high".into(),
        min: 0.9,
        max: 1.0,
        feather: 0.0,
    }];
    let mut l = mesh_entry("l", 1.0, 0.0, 0.0);
    l.channels = vec!["rest".into()];
    let mut lay = layer("l", grid(1.0), vec![h, l]);
    lay.channels = vec![
        BiomeChannel {
            name: "high".into(),
            rule: ChannelRule::TerrainLayer {
                name: "high".into(),
                min_weight: 0.5,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("t", 0, vec![lay]);
    let snap = snapshot_with(
        &ramp_map(1.0),
        box_shape([60.0, 0.0, 0.0], [60.0, 200.0, 20.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [60.0, 0.0], [60.0, 20.0], 0.0, def)],
            terrain_layers: Some(Arc::new(layers)),
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap, 4);
    let pts = node_points(&r, 4);
    let hs: Vec<&Pt> = pts.iter().filter(|p| p.entry == "t/l/h").collect();
    let ls: Vec<&Pt> = pts.iter().filter(|p| p.entry == "t/l/l").collect();
    assert!(!hs.is_empty() && !ls.is_empty());
    assert!(
        hs.iter().all(|p| p.pos[1] > 45.0),
        "high layer only above its band edge"
    );
    assert!(ls.iter().all(|p| p.pos[1] < 55.0), "the rest below it");
    assert!(pts.iter().filter(|p| p.pos[1] > 65.0).all(|p| p.entry == "t/l/h"));
    assert!(
        counter(&r, "t/l/h|filter:0:TerrainLayer") > 0,
        "the 0.9 window trims the blend band"
    );
    assert!(
        !r.report
            .warning_codes()
            .contains(&"biome_terrain_layer_unavailable".to_string())
    );
}

fn perf_area(guid: u128, defs: Vec<BiomeDef>) -> BiomeAreaSnap {
    let first = defs[0].clone();
    let mut area = box_area(guid, [0.0, 0.0], [8.0, 8.0], 0.0, first);
    area.biomes = defs
        .into_iter()
        .enumerate()
        .map(|(i, def)| (format!("{}-{i}.biome.ron", def.name), Arc::new(def)))
        .collect();
    area
}

fn areas_graph(with_layer: bool) -> ProcGraph {
    let mut nodes = vec![
        (1, common::node("in.self", &[])),
        (2, common::node("biome.areas", &[("cell_m", "8.0")])),
    ];
    let mut edges = vec![(1, "out", 2, "bounds")];
    if with_layer {
        nodes.push((3, common::node("in.ground", &[])));
        nodes.push((4, common::node("biome.layer", &[])));
        nodes.push((5, common::node("graph.output", &[])));
        edges.extend([
            (2, "out", 4, "claims"),
            (3, "out", 4, "ground"),
            (4, "out", 5, "in"),
        ]);
    } else {
        nodes.push((5, common::node("graph.output", &[])));
        edges.push((2, "out", 5, "in"));
    }
    common::graph(nodes, &edges)
}

/// `biome.areas` resolves each texel once per overlapping area, not once per mask (audit finding 2).
#[test]
fn areas_raster_resolves_each_texel_once() {
    let defs = |tag: &str| {
        vec![
            biome(&format!("{tag}-a"), 1, vec![]),
            biome(&format!("{tag}-b"), 2, vec![]),
        ]
    };
    let world = BiomeWorld {
        areas: vec![
            perf_area(1, defs("p")),
            perf_area(2, defs("q")),
            perf_area(3, defs("r")),
        ],
        ..BiomeWorld::default()
    };
    let snap = snapshot_with(
        &common::flat_map(5, 32.0, 0.0),
        box_shape([0.0, 0.0, 0.0], [8.0, 20.0, 8.0], 0.0),
        world,
    );
    // 16 m box, cell 8 m → 2×2 texels. 3 areas × 2 biomes.
    let (t, a, b) = (4u64, 3u64, 2u64);
    regia_procgen::probe::arm();
    common::run(&areas_graph(false), snap, 1);
    let values = regia_procgen::probe::value_calls();
    let claims = regia_procgen::probe::claims_calls();
    assert!(
        values <= t * a * b * 2,
        "value() {values} exceeds T*A*B*2 = {} (T×(A×B)² would be {})",
        t * a * b * 2,
        t * (a * b) * (a * b)
    );
    assert!(
        claims <= t * 2,
        "claims_at {claims} exceeds T*2 = {} (one call per mask texel would be {})",
        t * 2,
        t * a * b
    );
}

/// `biome.layer` samples the claim mask instead of resolving claims again (audit finding 3).
#[test]
fn layer_candidates_sample_the_claim_mask() {
    let def = biome(
        "meadow",
        1,
        vec![layer("canopy", grid(4.0), vec![mesh_entry("oak", 1.0, 0.0, 0.0)])],
    );
    let world = BiomeWorld {
        areas: vec![box_area(1, [0.0, 0.0], [8.0, 8.0], 0.0, def)],
        ..BiomeWorld::default()
    };
    let snap = snapshot_with(
        &common::flat_map(17, 64.0, 1.0),
        box_shape([0.0, 0.0, 0.0], [8.0, 20.0, 8.0], 0.0),
        world,
    );
    let claims_of = |with_layer: bool| {
        regia_procgen::probe::arm();
        common::run(&areas_graph(with_layer), Arc::clone(&snap), 1);
        regia_procgen::probe::claims_calls()
    };
    let areas = claims_of(false);
    let layered = claims_of(true);
    assert!(
        layered <= areas,
        "biome.layer added {} claims_at calls (areas {areas}, with layer {layered})",
        layered.saturating_sub(areas)
    );
}

/// Grass masks share one claim resolve per texel (audit finding 4).
#[test]
fn grass_masks_resolve_each_texel_once() {
    let mut defs = Vec::new();
    for (i, name) in ["north", "south"].into_iter().enumerate() {
        let mut def = biome(name, i as i32, vec![]);
        def.grass = ["short", "tall", "reed"]
            .into_iter()
            .map(|class| BiomeGrass {
                class: class.to_string(),
                density: 0.5,
                outside: GrassOutside::Keep,
            })
            .collect();
        defs.push(def);
    }
    let mut second = defs.clone();
    second.iter_mut().for_each(|d| d.name = format!("{}-2", d.name));
    let world = BiomeWorld {
        areas: vec![perf_area(1, defs), perf_area(2, second)],
        ..BiomeWorld::default()
    };
    let world = {
        let mut world = world;
        world.sort();
        world
    };
    // 16 m rect, 8 m texels → 4 texels. 3 classes, 2 areas, 2 biomes.
    let (c, t, a) = (3u64, 4u64, 2u64);
    regia_procgen::probe::arm();
    let masks = regia_procgen::biome::grass::grass_masks(&world, [-8.0, -8.0, 8.0, 8.0], 8.0);
    assert_eq!(masks.len(), 3);
    let values = regia_procgen::probe::value_calls();
    assert!(
        values <= c * t * a,
        "value() {values} exceeds C*T*A = {} (per class per biome would be {})",
        c * t * a,
        c * t * a * 2
    );
}

/// Far corridors are rejected by the exclusion grid, not by walking every region (audit finding 8).
#[test]
fn exclude_uses_a_spatial_index() {
    let exclusions = (0..6)
        .map(|i| {
            let z = i as f32 * 30.0;
            ExclusionRegion::Corridor {
                points: vec![[10_000.0, z], [10_020.0, z]],
                half_width: 2.0,
            }
        })
        .collect();
    let world = BiomeWorld {
        exclusions,
        ..BiomeWorld::default()
    };
    let snap = snapshot_with(
        &common::flat_map(5, 64.0, 0.0),
        box_shape([0.0; 3], [16.0, 10.0, 16.0], 0.0),
        world,
    );
    let g = common::graph(
        vec![
            (1, common::node("gen.grid", &[("count", "(8.0, 1.0, 8.0)")])),
            (2, common::node("biome.exclude", &[])),
            (3, common::node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    regia_procgen::probe::arm();
    let points = common::out_points(&common::run(&g, snap, 1), "out").len();
    let (p, e) = (64u64, 6u64);
    assert_eq!(points, p as usize, "nothing near the origin is excluded");
    let calls = regia_procgen::probe::contains_calls();
    assert!(
        calls < p && calls < p * e,
        "contains() {calls} is not below P={p} (a full scan is P*E={})",
        p * e
    );
}

/// A terrain-layer rule does not build a weight vector per candidate, and warns once (audit finding 9).
#[test]
fn terrain_layer_filter_does_not_allocate_per_point() {
    use regia_terrain::{LayerDesc, TerrainLayers};
    let mut entry = mesh_entry("oak", 1.0, 0.0, 0.0);
    entry.filters = vec![EntryFilter::TerrainLayer {
        name: "nope".into(),
        min: 0.0,
        max: 1.0,
        feather: 0.0,
    }];
    let mut lay = layer("canopy", grid(4.0), vec![entry]);
    lay.channels = vec![
        BiomeChannel {
            name: "grounded".into(),
            rule: ChannelRule::TerrainLayer {
                name: "ground".into(),
                min_weight: 0.0,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("meadow", 1, vec![lay]);
    let snap = snapshot_with(
        &common::flat_map(17, 64.0, 1.0),
        box_shape([0.0, 0.0, 0.0], [8.0, 20.0, 8.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [8.0, 8.0], 0.0, def)],
            terrain_layers: Some(Arc::new(TerrainLayers {
                layers: vec![LayerDesc::ground()],
                ..TerrainLayers::default()
            })),
            ..BiomeWorld::default()
        },
    );
    regia_procgen::probe::arm();
    common::run(&areas_graph(true), snap, 1);
    assert_eq!(
        regia_procgen::probe::weight_vecs(),
        0,
        "layer_weight_at built a Vec per candidate"
    );
    assert!(
        regia_procgen::probe::layer_warns() <= 1,
        "missing layer warned {} times",
        regia_procgen::probe::layer_warns()
    );
}

/// The mask lookup allocates its cache key on the first sample only (audit finding 10).
#[test]
fn mask_lookup_allocates_the_key_once() {
    let mut entry = mesh_entry("oak", 1.0, 0.0, 0.0);
    entry.channels = vec!["painted".into()];
    let mut lay = layer("canopy", grid(8.0), vec![entry]);
    lay.channels = vec![
        BiomeChannel {
            name: "painted".into(),
            rule: ChannelRule::Mask {
                asset: "paint.png".into(),
                channel: "r".into(),
                min: 2.0,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("meadow", 1, vec![lay]);
    let mut snap = snapshot_with(
        &common::flat_map(17, 64.0, 1.0),
        box_shape([0.0, 0.0, 0.0], [8.0, 20.0, 8.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [8.0, 8.0], 0.0, def)],
            ..BiomeWorld::default()
        },
    );
    Arc::get_mut(&mut snap).expect("snapshot").masks.insert(
        "paint.png".into(),
        regia_procgen::exec::MaskImage {
            width: 1,
            height: 1,
            rgba: vec![[1.0, 0.0, 0.0, 1.0]],
        },
    );
    regia_procgen::probe::arm();
    common::run(&areas_graph(true), snap, 1);
    assert_eq!(
        regia_procgen::probe::mask_keys(),
        1,
        "each candidate allocated a fresh mask-cache key"
    );
}

/// `biome.spawn` tallies one entry name and counts it once (audit finding 11).
#[test]
fn spawn_counts_once_per_entry_name() {
    let def = biome(
        "oakb",
        1,
        vec![layer("canopy", grid(4.0), vec![mesh_entry("oak", 1.0, 0.0, 0.0)])],
    );
    let snap = snapshot_with(
        &common::flat_map(17, 64.0, 1.0),
        box_shape([0.0, 0.0, 0.0], [8.0, 20.0, 8.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [8.0, 8.0], 0.0, def)],
            ..BiomeWorld::default()
        },
    );
    regia_procgen::probe::arm();
    let r = run_graph(&starter(), snap, 1);
    let placed = counter(&r, "oakb/canopy/oak|placed");
    assert!(placed > 1, "expected several placements, got {placed}");
    assert_eq!(
        regia_procgen::probe::count_flushes(),
        1,
        "spawn flushed the placed counter {placed} times"
    );
}

// ---------------------------------------------------------------------------------------------
// Spec conformance additions (lane spec-26-64-splines-polygons-road, 2026-11-10): behavior the
// spec requires that had no test yet.
// ---------------------------------------------------------------------------------------------

fn flat_map() -> TerrainMap {
    TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat")
}

/// One biome `b` over a 40 m box on `map`, run through the starter graph.
fn one_biome_run(map: &TerrainMap, layers: Vec<BiomeLayer>, seed: u32) -> RunResult {
    let def = biome("b", 0, layers);
    let snap = snapshot_with(
        map,
        box_shape([0.0; 3], [40.0, 100.0, 40.0], 0.0),
        BiomeWorld {
            areas: vec![box_area(1, [0.0, 0.0], [40.0, 40.0], 0.0, def)],
            ..BiomeWorld::default()
        },
    );
    run_graph(&starter(), snap, seed)
}

/// Spec 65 § 3 and ADR-1665 (5): a spacer (`spawn: None`) places nothing and is not counted as
/// placed, but it still claims space: its cores remove lower-rank points in the resolve.
#[test]
fn spacers_claim_space_but_are_not_counted_as_placed() {
    let mut gap = BiomeEntry::new("gap", EntrySpawn::None);
    gap.footprint = Footprint {
        core_m: Some(2.0),
        spread_m: Some(0.0),
    };
    let mut clearings = layer("clearings", grid(8.0), vec![gap]);
    clearings.rank = 40;
    let mut trees = layer("trees", grid(1.0), vec![mesh_entry("tree", 1.0, 0.25, 0.0)]);
    trees.rank = 10;
    let r = one_biome_run(&flat_map(), vec![clearings, trees], 3);
    assert!(
        counter(&r, "b/clearings/gap|assigned") > 0,
        "the spacer takes candidates"
    );
    assert_eq!(
        counter(&r, "b/clearings/gap|placed"),
        0,
        "spacers are not counted as placed (ADR-1665 (5))"
    );
    assert!(
        counter(&r, "b/trees/tree|resolved_out") > 0,
        "spacer cores clear trees"
    );
    assert_eq!(
        counter(&r, "b/trees/tree|placed") as usize,
        r.output.instance_count(),
        "the placed counters equal the spawned instances"
    );
}

/// Spec 65 § 3: `enabled: false` never places and is skipped as a child.
#[test]
fn disabled_entries_never_place_and_are_skipped_as_children() {
    let mut on = mesh_entry("on", 1.0, 0.0, 0.0);
    on.children = vec![rule("off", 2, (1.0, 2.0), 1.0, KeepClear::None)];
    let mut off = mesh_entry("off", 5.0, 0.0, 0.0);
    off.enabled = false;
    let r = one_biome_run(&flat_map(), vec![layer("l", grid(4.0), vec![on, off])], 4);
    let pts = node_points(&r, 4);
    assert!(pts.iter().any(|p| p.entry.ends_with("/on")));
    assert!(
        pts.iter().all(|p| p.entry.ends_with("/on") && p.depth == 0),
        "no disabled root or child"
    );
    assert_eq!(counter(&r, "b/l/off|assigned"), 0);
}

/// Spec 65 § 3.3: `filters_off: true` skips every filter of the entry.
#[test]
fn filters_off_skips_every_filter() {
    let run = |off: bool| {
        let mut e = mesh_entry("e", 1.0, 0.0, 0.0);
        e.filters = vec![EntryFilter::Altitude {
            min: 100.0,
            max: 200.0,
            feather: 0.0,
        }];
        e.filters_off = off;
        node_points(
            &one_biome_run(&flat_map(), vec![layer("l", grid(2.0), vec![e])], 5),
            4,
        )
        .len()
    };
    assert_eq!(
        run(false),
        0,
        "the altitude window removes every point at height 0"
    );
    assert!(run(true) > 300, "with filters_off nothing is removed");
}

/// Spec 65 § 3.3: the `Slope` and `Density` filters keep points inside their window and remove
/// points beyond it (no feather).
#[test]
fn slope_and_density_filters_keep_the_window() {
    let ramp = ramp_map(1.0); // 45° everywhere
    let count = |map: &TerrainMap, f: EntryFilter| {
        let mut e = mesh_entry("e", 1.0, 0.0, 0.0);
        e.filters = vec![f];
        node_points(&one_biome_run(map, vec![layer("l", grid(2.0), vec![e])], 6), 4).len()
    };
    let all = count(
        &ramp,
        EntryFilter::Altitude {
            min: -1.0e6,
            max: 1.0e6,
            feather: 0.0,
        },
    );
    assert!(all > 300, "{all}");
    let slope = |lo: f64, hi: f64| EntryFilter::Slope {
        min_deg: lo,
        max_deg: hi,
        feather: 0.0,
    };
    assert_eq!(count(&ramp, slope(40.0, 50.0)), all, "45° is inside 40-50°");
    assert_eq!(count(&ramp, slope(0.0, 30.0)), 0, "45° is beyond 0-30°");
    let density = |lo: f64, hi: f64| EntryFilter::Density {
        min: lo,
        max: hi,
        feather: 0.0,
    };
    let flat = flat_map();
    assert_eq!(count(&flat, density(0.5, 1.0)), count(&flat, slope(0.0, 90.0)));
    assert_eq!(
        count(&flat, density(1.5, 2.0)),
        0,
        "candidate density 1 is below 1.5"
    );
}

/// Groups depth-1 points with their nearest root (roots are far apart).
fn kids_by_parent(pts: &[Pt]) -> Vec<(&Pt, Vec<&Pt>)> {
    let roots: Vec<&Pt> = pts.iter().filter(|p| p.depth == 0).collect();
    let mut out: Vec<(&Pt, Vec<&Pt>)> = roots.iter().map(|r| (*r, Vec::new())).collect();
    for k in pts.iter().filter(|p| p.depth == 1) {
        let d2 = |r: &Pt| (k.pos[0] - r.pos[0]).powi(2) + (k.pos[2] - r.pos[2]).powi(2);
        let best = (0..roots.len())
            .min_by(|&a, &b| d2(roots[a]).total_cmp(&d2(roots[b])))
            .expect("a root");
        out[best].1.push(k);
    }
    out
}

/// Spec 65 § 3.6: child `k` of `n` sits at `θ0 + k · 137.508°` and radius
/// `lo + (hi − lo) · sqrt((k + 0.5) / n)`.
#[test]
fn children_follow_the_golden_angle_spiral() {
    let (_, pts) = children_run(children_world(
        1,
        20.0,
        rule("kid", 4, (0.0, 2.0), 1.0, KeepClear::None),
    ));
    let groups = kids_by_parent(&pts);
    assert!(groups.len() > 100);
    let want: Vec<f32> = (0..4).map(|k| 2.0 * ((k as f32 + 0.5) / 4.0).sqrt()).collect();
    for (p, kids) in &groups {
        assert_eq!(kids.len(), 4);
        let mut polar: Vec<(f32, f32)> = kids
            .iter()
            .map(|k| {
                let (dx, dz) = (k.pos[0] - p.pos[0], k.pos[2] - p.pos[2]);
                ((dx * dx + dz * dz).sqrt(), dz.atan2(dx).to_degrees())
            })
            .collect();
        polar.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (k, (r, _)) in polar.iter().enumerate() {
            assert!((r - want[k]).abs() < 1e-3, "child {k}: radius {r} vs {}", want[k]);
        }
        for w in polar.windows(2) {
            // The step is ±137.508° depending on handedness; its size is fixed.
            let step = (w[1].1 - w[0].1).rem_euclid(360.0);
            let size = step.min(360.0 - step);
            assert!((size - 137.508).abs() < 0.02, "step {step}");
        }
    }
}

/// Spec 65 § 3.5: `keep_clear_of_parent: Spread` removes children inside the parent's spread.
#[test]
fn keep_clear_spread_removes_children_inside_the_spread() {
    let mut parent = mesh_entry("parent", 1.0, 0.25, 1.4);
    parent.children = vec![rule("kid", 4, (0.0, 2.0), 1.0, KeepClear::Spread)];
    let kid = mesh_entry("kid", 0.0, 0.0, 0.0);
    let mut l = layer("l", grid(20.0), vec![parent, kid]);
    l.max_child_depth = 1;
    let w = BiomeWorld {
        areas: vec![box_area(
            1,
            [0.0, 0.0],
            [240.0, 240.0],
            0.0,
            biome("c", 0, vec![l]),
        )],
        ..BiomeWorld::default()
    };
    let (r, pts) = children_run(w);
    let groups = kids_by_parent(&pts);
    assert!(!groups.is_empty());
    for (p, kids) in &groups {
        // Radii 0.71, 1.22, 1.58, 1.87: the two inside the 1.4 m spread go.
        assert_eq!(kids.len(), 2, "children kept per parent");
        for k in kids {
            let d = ((k.pos[0] - p.pos[0]).powi(2) + (k.pos[2] - p.pos[2]).powi(2)).sqrt();
            assert!(f64::from(d) >= p.spread - 1e-5, "{d} inside spread {}", p.spread);
        }
    }
    assert_eq!(
        counter(&r, "c/l/kid|spaced_out") as usize,
        groups.len() * 2,
        "spaced_out counts the keep-clear removals"
    );
}

/// Spec 65 § 3.6 and ADR-1665 (4): `scale_with_parent` scales the ring radii by the parent's
/// horizontal scale.
#[test]
fn scale_with_parent_scales_the_ring() {
    let dist = |with: bool| {
        let mut parent = mesh_entry("parent", 1.0, 0.0, 0.0);
        parent.placement.scale = ScaleRule::Fixed(2.0);
        let mut ch = rule("kid", 1, (1.0, 1.0), 1.0, KeepClear::None);
        ch.scale_with_parent = with;
        parent.children = vec![ch];
        let kid = mesh_entry("kid", 0.0, 0.0, 0.0);
        let mut l = layer("l", grid(20.0), vec![parent, kid]);
        l.max_child_depth = 1;
        let w = BiomeWorld {
            areas: vec![box_area(
                1,
                [0.0, 0.0],
                [240.0, 240.0],
                0.0,
                biome("c", 0, vec![l]),
            )],
            ..BiomeWorld::default()
        };
        let (_, pts) = children_run(w);
        let groups = kids_by_parent(&pts);
        assert!(!groups.is_empty());
        let mut ds = Vec::new();
        for (p, kids) in &groups {
            assert_eq!(kids.len(), 1);
            let k = kids[0];
            ds.push(((k.pos[0] - p.pos[0]).powi(2) + (k.pos[2] - p.pos[2]).powi(2)).sqrt());
        }
        ds
    };
    assert!(dist(false).iter().all(|d| (d - 1.0).abs() < 1e-3));
    assert!(dist(true).iter().all(|d| (d - 2.0).abs() < 1e-3));
}

/// Spec 65 § 3: layer, channel and entry names are unique within their scope (`biome_invalid`).
#[test]
fn duplicate_names_are_biome_invalid() {
    let e = || mesh_entry("e", 1.0, 0.0, 0.0);
    let mut l = layer("a", grid(1.0), vec![e(), e()]);
    l.channels = vec![
        BiomeChannel {
            name: "c".into(),
            rule: ChannelRule::Rest,
        },
        BiomeChannel {
            name: "c".into(),
            rule: ChannelRule::Rest,
        },
    ];
    let def = biome("d", 0, vec![l, layer("a", grid(1.0), vec![e()])]);
    let issues = def.validate();
    assert_eq!(issues.len(), 3, "{issues:?}");
    assert!(issues.iter().all(|i| i.code == "biome_invalid"));
    assert!(BiomeDef::from_ron(&def.to_ron()).is_err());
}

/// Spec 65 § 4: `BiomeArea` defaults (`blend_m` 8, offset 0, no preview, version 1), and
/// `priority_offset` adds to the biome priority for that area only.
#[test]
fn area_defaults_and_priority_offset() {
    let a = regia_procgen::biome::ecs::BiomeArea::default();
    assert_eq!(a.blend_m, 8.0);
    assert_eq!(a.priority_offset, 0);
    assert!(!a.preview_alone && a.blend_noise.is_none());
    assert_eq!(a.format_version, 1);
    let low = box_area(5, [0.0, 0.0], [50.0, 50.0], 0.0, biome("low", 10, vec![]));
    let high = box_area(9, [0.0, 0.0], [50.0, 50.0], 0.0, biome("high", 20, vec![]));
    let owner = |areas: &[BiomeAreaSnap]| {
        let c = claims_at(areas, 1.0, 1.0);
        areas[c.owner.expect("owner").area].guid
    };
    assert_eq!(owner(&[low.clone(), high.clone()]), Guid(9));
    let mut boosted = low;
    boosted.priority_offset = 11;
    assert_eq!(owner(&[boosted, high]), Guid(5), "10 + 11 beats 20");
}

/// Spec 65 § 12 test 9 (thread part): the full starter graph over the three-biome map gives the
/// same output hash serially and with 1 and 8 worker threads.
#[test]
fn t9_full_starter_graph_hash_is_the_same_at_1_and_8_threads() {
    use regia_procgen::exec::Parallelism;
    let snap = snapshot_with(
        &test_map(),
        box_shape([0.0; 3], [250.0, 100.0, 250.0], 0.0),
        full_world(),
    );
    let c = match starter().compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let inputs = RunInputs::new(snap, 42);
    let hash =
        |p: Parallelism| match Evaluator::new()
            .with_parallelism(p)
            .run(&c, &inputs, &CancelToken::new())
        {
            Ok(r) => {
                assert!(r.output.instance_count() > 0);
                r.output_hash
            }
            Err(e) => panic!("run failed: {e}"),
        };
    let serial = hash(Parallelism::Serial);
    assert_eq!(hash(Parallelism::threads(1)), serial, "1 thread");
    assert_eq!(hash(Parallelism::threads(8)), serial, "8 threads");
}

/// Spec 65 § 3.4 and ADR-1665 (4): `yaw: Fixed`, `yaw: AlongSlope` (faces downhill:
/// `atan2(n.x, n.z)` about Y, so local +Z points down the slope), `scale: Fixed` and `sink_m`
/// (lowered along the instance's up axis) reach the spawned instances.
#[test]
fn placement_yaw_scale_and_sink_reach_the_instances() {
    use regia_core::math::{Quat, Vec3};
    let instances = |map: &TerrainMap, yaw: Yaw| {
        let mut e = mesh_entry("e", 1.0, 0.0, 0.0);
        e.placement = Placement {
            upright: 1.0,
            yaw,
            scale: ScaleRule::Fixed(1.5),
            scale_from_source: false,
            sink_m: 0.5,
            offset: None,
        };
        let r = one_biome_run(map, vec![layer("l", grid(4.0), vec![e])], 8);
        let all: Vec<_> = r
            .output
            .instance_sets
            .iter()
            .flat_map(|s| s.instances.iter().cloned())
            .collect();
        assert!(all.len() > 50, "{}", all.len());
        all
    };
    let fwd = |q: [f32; 4]| Quat::from_array(q) * Vec3::Z;
    // Flat ground at 0: Fixed(90) turns +Z a quarter turn about Y; sink lowers by 0.5 m.
    for i in instances(&flat_map(), Yaw::Fixed(90.0)) {
        let f = fwd(i.rotation);
        assert!(
            f.y.abs() < 1e-4 && f.z.abs() < 1e-4 && (f.x.abs() - 1.0).abs() < 1e-4,
            "{f:?}"
        );
        assert!((i.translation[1] + 0.5).abs() < 1e-4, "{:?}", i.translation);
        assert_eq!(i.scale, [1.5; 3]);
    }
    // A ramp h = x (45°): AlongSlope faces downhill, -X; upright 1 keeps the instance level.
    for i in instances(&ramp_map(1.0), Yaw::AlongSlope) {
        let f = fwd(i.rotation);
        assert!(f.abs_diff_eq(Vec3::new(-1.0, 0.0, 0.0), 1e-3), "{f:?}");
        assert!(
            (i.translation[1] - (i.translation[0] - 0.5)).abs() < 1e-3,
            "{:?}",
            i.translation
        );
    }
}

/// Spec 65 § 7 filter view (lane post05-spline-biome-perf): a preview run (`keep_debug`) records
/// every candidate an entry filter removed on `biome.layer`'s second pin `filtered`, with the entry
/// and the filter (`<index>:<kind>`, the report counter's suffix), as many as the counter says. The
/// generator output is the same with and without it.
#[test]
fn filter_view_records_the_removed_points_by_filter_in_debug_runs() {
    let map = ramp_map(1.0);
    let mut e = mesh_entry("e", 1.0, 0.0, 0.0);
    e.filters = vec![EntryFilter::Altitude {
        min: -1.0e9,
        max: 50.0,
        feather: 0.0,
    }];
    let def = biome("f", 0, vec![layer("l", grid(1.0), vec![e])]);
    let area = box_area(1, [60.0, 0.0], [60.0, 20.0], 0.0, def);
    let snap = snapshot_with(
        &map,
        box_shape([60.0, 0.0, 0.0], [60.0, 200.0, 20.0], 0.0),
        BiomeWorld {
            areas: vec![area],
            ..BiomeWorld::default()
        },
    );
    let r = run_graph(&starter(), snap.clone(), 11);
    let removed = counter(&r, "f/l/e|filter:0:Altitude");
    assert!(removed > 0, "the ramp rises past the 50 m window");
    let layer_id = r
        .report
        .nodes
        .iter()
        .find(|n| n.ty == "biome.layer")
        .map(|n| n.id)
        .expect("the starter graph has a biome.layer node");
    let pins = r
        .debug
        .as_ref()
        .and_then(|d| d.get(&layer_id))
        .expect("debug outputs of the layer");
    assert_eq!(pins.len(), 2, "biome.layer outputs `out` and `filtered`");
    let mut marks = Vec::new();
    for item in &pins[1].0 {
        let p = item.as_points().expect("filtered points");
        let col = |n: &str| match p.attrs.get(n) {
            Some(AttrColumn::Name(v)) => v.to_vec(),
            other => panic!("`{n}` is a name column, got {other:?}"),
        };
        let (entries, filters) = (col("entry"), col("filter"));
        for i in 0..p.len() {
            marks.push((p.position[i], entries[i].clone(), filters[i].clone()));
        }
    }
    assert_eq!(marks.len() as u64, removed, "one mark per removed candidate");
    for (pos, entry, filter) in &marks {
        assert_eq!((entry.as_str(), filter.as_str()), ("e", "0:Altitude"));
        assert!(
            pos[1] > 50.0,
            "only candidates above the window are removed: {pos:?}"
        );
    }
    // Without debug data nothing is recorded and the output is unchanged.
    let c = starter()
        .compile(&ProcNodeRegistry::with_builtins())
        .expect("compile");
    let plain = Evaluator::new()
        .run(&c, &RunInputs::new(snap, 11), &CancelToken::new())
        .expect("run");
    assert_eq!(plain.output_hash, r.output_hash);
}

// ---------------------------------------------------------------------------------------------
// ADR-7252 (lane post05-biome-bake-parallel): `biome.layer` jobs on the pool
// ---------------------------------------------------------------------------------------------

/// The full three-biome run on a schedule, with debug outputs (both `biome.layer` pins).
fn full_run_on(
    parallelism: regia_procgen::exec::Parallelism,
    snap: Arc<dyn regia_procgen::exec::WorldSnapshot>,
) -> RunResult {
    let c = starter()
        .compile(&ProcNodeRegistry::with_builtins())
        .expect("compile");
    let mut inputs = RunInputs::new(snap, 42);
    inputs.keep_debug = true;
    match Evaluator::new()
        .with_parallelism(parallelism)
        .run(&c, &inputs, &CancelToken::new())
    {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

/// `biome.layer` runs one job per (claim, layer) on the pool; a serial run and 1-, 2- and 8-worker
/// pools give the same bytes: the output hash, the run output, every node's debug outputs (the
/// layer's `out` and `filtered` pins included), and per node the counters and the warnings with
/// their first messages (the warnings name entries, so their order shows the job order).
#[test]
fn layer_jobs_give_the_same_bytes_on_one_and_many_threads() {
    use regia_procgen::exec::Parallelism;
    let snap = snapshot_with(
        &test_map(),
        box_shape([0.0; 3], [250.0, 100.0, 250.0], 0.0),
        full_world(),
    );
    let reference = full_run_on(Parallelism::Serial, snap.clone());
    let layer = reference
        .report
        .nodes
        .iter()
        .find(|n| n.ty == "biome.layer")
        .expect("a biome.layer line");
    assert!(
        layer.warnings.len() > 1 || !layer.counters.is_empty(),
        "the reference run exercises the layer: {layer:?}"
    );
    assert!(reference.output.instance_count() > 1000);
    for threads in [1, 2, 8] {
        let r = full_run_on(Parallelism::threads(threads), snap.clone());
        assert_eq!(
            hex(&r.output_hash),
            hex(&reference.output_hash),
            "{threads} threads: output hash"
        );
        assert!(r.output == reference.output, "{threads} threads: run output");
        assert!(
            r.debug == reference.debug,
            "{threads} threads: node debug outputs"
        );
        assert_eq!(r.report.nodes.len(), reference.report.nodes.len());
        for (a, b) in r.report.nodes.iter().zip(&reference.report.nodes) {
            assert_eq!((a.id, &a.ty), (b.id, &b.ty));
            assert_eq!(
                a.counters, b.counters,
                "{threads} threads: node {} counters",
                a.id
            );
            assert_eq!(
                a.warnings, b.warnings,
                "{threads} threads: node {} warnings",
                a.id
            );
            assert_eq!(
                a.elements, b.elements,
                "{threads} threads: node {} elements",
                a.id
            );
        }
    }
}

/// Two callers of `mesh` meet here: the first waits until a second arrives (or gives up after a
/// generous bound so a serial build fails instead of hanging). Later calls pass straight through.
#[derive(Default)]
struct Rendezvous {
    state: std::sync::Mutex<(usize, Option<bool>)>,
    cv: std::sync::Condvar,
}

impl Rendezvous {
    fn meet(&self) {
        let Ok(mut g) = self.state.lock() else {
            return;
        };
        if g.1.is_some() {
            return;
        }
        g.0 += 1;
        if g.0 >= 2 {
            g.1 = Some(true);
            self.cv.notify_all();
            return;
        }
        // Only reached by a serial schedule, where no second job can ever arrive: the bound turns
        // that hang into a failure; it is not a performance budget.
        let Ok((mut g, _)) = self
            .cv
            .wait_timeout_while(g, std::time::Duration::from_secs(120), |s| s.1.is_none())
        else {
            return;
        };
        if g.1.is_none() {
            g.1 = Some(false);
        }
    }

    fn met(&self) -> Option<bool> {
        self.state.lock().ok().and_then(|g| g.1)
    }
}

/// A snapshot that makes the first two `mesh` lookups (one per layer job's footprint) meet.
struct MeetingSnapshot {
    inner: Arc<regia_procgen::exec::StaticSnapshot>,
    meet: Arc<Rendezvous>,
}

impl regia_procgen::exec::WorldSnapshot for MeetingSnapshot {
    fn self_info(&self) -> Option<&SelfInfo> {
        self.inner.self_info()
    }
    fn ground(&self) -> Option<Arc<regia_procgen::data::GroundData>> {
        self.inner.ground()
    }
    fn entities(&self) -> &[regia_procgen::exec::SnapEntity] {
        self.inner.entities()
    }
    fn table(&self, path: &str) -> Option<&regia_procgen::data::Table> {
        self.inner.table(path)
    }
    fn mesh(&self, path: &str) -> Option<Arc<regia_procgen::data::MeshData>> {
        self.meet.meet();
        self.inner.mesh(path)
    }
    fn biome(&self) -> Option<Arc<BiomeWorld>> {
        self.inner.biome()
    }
}

/// On a pool, two `biome.layer` jobs run at the same time: the forest job's first footprint lookup
/// waits for another job's, which only a parallel schedule can supply. The output is the serial
/// run's.
#[test]
fn layer_jobs_run_at_the_same_time_on_a_pool() {
    use regia_procgen::exec::Parallelism;
    let inner = snapshot_with(
        &test_map(),
        box_shape([0.0; 3], [250.0, 100.0, 250.0], 0.0),
        full_world(),
    );
    let serial = full_run_on(Parallelism::Serial, inner.clone());
    let meet = Arc::new(Rendezvous::default());
    let snap = Arc::new(MeetingSnapshot {
        inner,
        meet: meet.clone(),
    });
    let r = full_run_on(Parallelism::threads(4), snap);
    assert_eq!(
        meet.met(),
        Some(true),
        "no two biome.layer jobs looked up a mesh at the same time"
    );
    assert_eq!(hex(&r.output_hash), hex(&serial.output_hash));
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 (deep-zai-procgen-runtime finding 2): child counts against the point budget
// ---------------------------------------------------------------------------------------------

/// A child rule whose `count.hi` alone passes the largest per-item point budget fails validation
/// (`biome_invalid` naming `count`), instead of loading and allocating that many children per parent.
#[test]
fn v051_a_child_count_above_the_point_budget_is_invalid() {
    // `MAX_CHILDREN_PER_PARENT`: the desktop per-item point budget.
    const MAX_CHILDREN_PER_PARENT: usize = 4 << 20;
    let text = SPEC_EXAMPLE.replace("count: (2, 5)", "count: (2, 4000000000)");
    match BiomeDef::from_ron(&text) {
        Err(regia_procgen::biome::BiomeDefError::Invalid(list)) => {
            assert!(
                list.iter()
                    .any(|i| i.code == "biome_invalid" && i.path.ends_with("children/0/count")),
                "{list:?}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    let at_cap = SPEC_EXAMPLE.replace("count: (2, 5)", &format!("count: (2, {MAX_CHILDREN_PER_PARENT})"));
    assert!(
        BiomeDef::from_ron(&at_cap).is_ok(),
        "the budget itself is allowed"
    );
}

/// A def built in code skips the file validation: the run checks each parent's child count against
/// the run's point budget before it allocates the positions, and fails with `limit_exceeded`. A
/// `(0, u32::MAX)` count used to overflow `hi - lo + 1`.
#[test]
fn v051_children_are_checked_against_the_budget_before_they_are_allocated() {
    let map = TerrainMap::from_heights(257, 257, vec![0.0; 257 * 257], 512.0, 1.0, 0.0).expect("flat");
    let run = |count: (u32, u32), budget: usize| {
        let mut r = rule("kid", 0, (1.0, 2.0), 1.0, KeepClear::None);
        r.count = count;
        let snap = snapshot_with(
            &map,
            box_shape([0.0; 3], [240.0, 50.0, 240.0], 0.0),
            children_world(1, 20.0, r),
        );
        let c = match starter().compile(&ProcNodeRegistry::with_builtins()) {
            Ok(c) => c,
            Err(e) => panic!("compile failed: {:?}", e.0),
        };
        let mut inputs = RunInputs::new(snap, 21);
        inputs.limits.points_per_item = budget;
        Evaluator::new().run(&c, &inputs, &CancelToken::new())
    };
    // 300 000 fits the starter graph's other nodes (its area sampling holds 230 400 points here) and
    // the 576 parents (a 24 x 24 grid), not 1 000 children per parent.
    const BUDGET: usize = 300_000;
    for count in [(0, u32::MAX), (3_000_000_000, 3_000_000_000), (1_000, 1_000)] {
        match run(count, BUDGET) {
            Err(regia_procgen::RunError::Node { code, message, .. }) => {
                assert_eq!(code, "limit_exceeded", "{count:?}: {message}");
                assert!(message.contains("points_per_item"), "{count:?}: {message}");
            }
            other => panic!(
                "{count:?}: expected limit_exceeded, got {:?}",
                other.map(|r| r.output_hash)
            ),
        }
    }
    // Within the budget the same rule runs: 576 parents (24 x 24 grid) with 4 children each.
    let r = run((4, 4), BUDGET).expect("within the budget");
    assert!(counter(&r, "c/l/kid|children") > 0);
}
