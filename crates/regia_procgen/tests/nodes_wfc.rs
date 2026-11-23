// SPDX-License-Identifier: MPL-2.0
//! `wfc.solve` and WFC symmetries (spec 63 addendum, ADR-2724): rotation and reflection variants of
//! socket tiles and sample patterns, seeded and deterministic output, node validation and errors.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use regia_procgen::wfc::{
    CellConstraints, GridSize, OverlappingModel, SampleGrid, SocketTile, Symmetry, TiledModel, WfcModel,
    WfcOptions,
};
use regia_procgen::{AttrColumn, PointSet, RunError, RunInputs};

fn empty_snapshot() -> std::sync::Arc<regia_procgen::StaticSnapshot> {
    snapshot(box_shape([0.0; 3], [50.0; 3], 0.0), None)
}

fn tile(name: &str, sockets: [&str; 4]) -> SocketTile {
    SocketTile {
        name: name.to_string(),
        weight: 1.0,
        sockets: sockets.map(str::to_string),
    }
}

fn rev(t: &str) -> String {
    t.chars().rev().collect()
}

/// The sockets of a variant, recomputed here (independently of `wfc.rs`): mirror swaps east and
/// west and reverses every socket; a clockwise quarter turn moves north to east.
fn turned(s: &[String; 4], turns: u8, mirrored: bool) -> [String; 4] {
    let mut v = s.clone();
    if mirrored {
        v = [rev(&s[2]), rev(&s[1]), rev(&s[0]), rev(&s[3])];
    }
    for _ in 0..turns {
        v = [v[3].clone(), v[0].clone(), v[1].clone(), v[2].clone()];
    }
    v
}

fn corner_set() -> Vec<SocketTile> {
    vec![
        tile("grass", ["g", "g", "g", "g"]),
        tile("road", ["r", "g", "r", "g"]),
        // A corner joining east and south, with an asymmetric curb on its north side.
        tile("corner", ["r", "r", "g", "gk"]),
    ]
}

#[test]
fn socket_variants_follow_the_symmetry_group_and_drop_duplicates() {
    let tiles = corner_set();
    let names = |sym| {
        TiledModel::from_sockets(&tiles, sym)
            .map(|m| m.tiles().iter().map(|t| t.name.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert_eq!(names(Symmetry::None), ["grass", "road", "corner"]);
    assert_eq!(
        names(Symmetry::Rotations),
        [
            "grass",
            "road",
            "road@r1",
            "corner",
            "corner@r1",
            "corner@r2",
            "corner@r3"
        ]
    );
    // The road is mirror-symmetric; the corner's curb is not, so its mirror is new.
    assert_eq!(
        names(Symmetry::Reflections),
        ["grass", "road", "corner", "corner@m"]
    );
    assert_eq!(names(Symmetry::All).len(), 1 + 2 + 8);
}

#[test]
fn every_neighbor_pair_of_a_solved_socket_grid_matches_its_sockets() {
    let tiles = corner_set();
    let model = match TiledModel::from_sockets(&tiles, Symmetry::All) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    let size = GridSize::new_2d(16, 12);
    let grid = match regia_procgen::wfc::solve(
        &WfcModel::Tiled(model.clone()),
        size,
        &CellConstraints::new(),
        77,
        &WfcOptions::default(),
    ) {
        Ok(g) => g,
        Err(e) => panic!("solve: {e}"),
    };
    let sockets_of = |id: usize| {
        let v = model.variant(id).unwrap_or_default();
        turned(&tiles[v.base].sockets, v.rotation, v.mirrored)
    };
    let mut kinds = BTreeSet::new();
    for y in 0..size.height {
        for x in 0..size.width {
            let a = grid.get_2d(x, y).unwrap_or(0);
            kinds.insert(a);
            if x + 1 < size.width {
                let b = grid.get_2d(x + 1, y).unwrap_or(0);
                assert_eq!(sockets_of(a)[0], rev(&sockets_of(b)[2]), "east of ({x}, {y})");
            }
            if y + 1 < size.height {
                let b = grid.get_2d(x, y + 1).unwrap_or(0);
                assert_eq!(sockets_of(a)[1], rev(&sockets_of(b)[3]), "south of ({x}, {y})");
            }
        }
    }
    assert!(kinds.len() > 2, "the grid uses several variants: {kinds:?}");
}

/// `block` (n × n, x-fastest) turned clockwise once, computed here independently.
fn rot90(block: &[u32], n: usize) -> Vec<u32> {
    let mut out = vec![0; n * n];
    for y in 0..n {
        for x in 0..n {
            // Clockwise with y south: what was on the west side is now on the north side.
            out[x + n * y] = block[y + n * (n - 1 - x)];
        }
    }
    out
}

fn mirror(block: &[u32], n: usize) -> Vec<u32> {
    let mut out = vec![0; n * n];
    for y in 0..n {
        for x in 0..n {
            out[x + n * y] = block[(n - 1 - x) + n * y];
        }
    }
    out
}

fn l_sample() -> SampleGrid {
    // An L of 1s on 0s: no rotation or mirror of its 3 × 3 blocks equals the block itself.
    #[rustfmt::skip]
    let cells = vec![
        0, 0, 0, 0, 0,
        0, 1, 0, 0, 0,
        0, 1, 0, 0, 0,
        0, 1, 1, 1, 0,
        0, 0, 0, 0, 0,
    ];
    match SampleGrid::new_2d(5, 5, cells) {
        Ok(s) => s,
        Err(e) => panic!("sample: {e}"),
    }
}

#[test]
fn symmetric_pattern_sets_are_closed_under_their_group() {
    let sample = l_sample();
    let build = |sym| match OverlappingModel::from_sample_symmetric(&sample, 3, true, sym) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    let none = build(Symmetry::None);
    let plain = match OverlappingModel::from_sample_with(&sample, 3, true) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    assert_eq!(
        none.patterns(),
        plain.patterns(),
        "Symmetry::None is the plain model"
    );
    let all = build(Symmetry::All);
    let rot = build(Symmetry::Rotations);
    let refl = build(Symmetry::Reflections);
    assert!(rot.patterns().len() > none.patterns().len());
    assert!(all.patterns().len() >= rot.patterns().len());
    for p in all.patterns() {
        assert!(
            all.contains_pattern(&rot90(&p.cells, 3)),
            "rotation of {:?}",
            p.cells
        );
        assert!(
            all.contains_pattern(&mirror(&p.cells, 3)),
            "mirror of {:?}",
            p.cells
        );
    }
    for p in rot.patterns() {
        assert!(rot.contains_pattern(&rot90(&p.cells, 3)));
    }
    for p in refl.patterns() {
        assert!(refl.contains_pattern(&mirror(&p.cells, 3)));
    }
    // Every original block keeps its place at the front: the symmetric set extends the plain one.
    for p in none.patterns() {
        assert!(all.contains_pattern(&p.cells));
    }
}

fn socket_settings(symmetry: &str, size: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "tiles",
            "[(name: \"grass\", weight: 3.0, sockets: (\"g\", \"g\", \"g\", \"g\")), (name: \"road\", sockets: (\"r\", \"g\", \"r\", \"g\")), (name: \"corner\", sockets: (\"r\", \"r\", \"g\", \"gk\"))]"
                .to_string(),
        ),
        ("symmetry", symmetry.to_string()),
        ("size", size.to_string()),
        ("cell_m", "2.0".to_string()),
        ("origin", "(10.0, 0.0, -4.0)".to_string()),
    ]
}

fn solve_graph(settings: &[(&str, String)]) -> regia_procgen::ProcGraph {
    let s: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (*k, v.as_str())).collect();
    graph(
        vec![(1, node("wfc.solve", &s)), (2, node("graph.output", &[]))],
        &[(1, "out", 2, "in")],
    )
}

fn names(p: &PointSet, attr: &str) -> Vec<String> {
    match p.attrs.get(attr) {
        Some(AttrColumn::Name(v)) => v.to_vec(),
        other => panic!("`{attr}` is not a name column: {other:?}"),
    }
}

#[test]
fn wfc_solve_emits_one_point_per_cell_at_the_cell_centers_with_tile_attributes() {
    let g = solve_graph(&socket_settings("rotate", "(6.0, 4.0, 1.0)"));
    let r = run(&g, empty_snapshot(), 5);
    let p = out_points(&r, "out");
    assert_eq!(p.len(), 24, "6 × 4 cells");
    // Row-major from the min corner: cell (x, y) at origin + ((x + 0.5) · 2, 0, (y + 0.5) · 2).
    for (i, pos) in p.position.iter().enumerate() {
        let (x, y) = ((i % 6) as f32, (i / 6) as f32);
        assert_eq!(
            *pos,
            [10.0 + (x + 0.5) * 2.0, 0.0, -4.0 + (y + 0.5) * 2.0],
            "cell {i}"
        );
    }
    let tile = names(&p, "tile");
    let base = names(&p, "tile_base");
    let Some(AttrColumn::Int(turns)) = p.attrs.get("tile_turns") else {
        panic!("tile_turns")
    };
    for i in 0..p.len() {
        assert!(
            tile[i].starts_with(&base[i]),
            "{} comes from {}",
            tile[i],
            base[i]
        );
        // A turned variant is a yaw of -90° per quarter turn: q = (0, sin(-45° t), 0, cos(-45° t)).
        let half = -std::f32::consts::FRAC_PI_4 * turns[i] as f32;
        let q = p.rotation[i];
        assert!(
            (q[1] - half.sin()).abs() < 1e-5 && (q[3] - half.cos()).abs() < 1e-5,
            "{i}: {q:?}"
        );
    }
}

#[test]
fn wfc_solve_is_seeded_by_the_generator_seed_and_repeatable() {
    let g = solve_graph(&socket_settings("all", "(12.0, 12.0, 1.0)"));
    let a = run(&g, empty_snapshot(), 5);
    let b = run(&g, empty_snapshot(), 5);
    let c = run(&g, empty_snapshot(), 6);
    let tiles = |r: &regia_procgen::RunResult| names(&out_points(r, "out"), "tile");
    assert_eq!(tiles(&a), tiles(&b), "same seed, same grid");
    assert_eq!(a.output_hash, b.output_hash);
    assert_ne!(tiles(&a), tiles(&c), "another generator seed, another grid");
    let used: BTreeSet<String> = tiles(&a).into_iter().collect();
    assert!(
        used.iter().any(|t| t.contains('@')),
        "turned or mirrored variants appear: {used:?}"
    );
}

#[test]
fn wfc_solve_overlapping_output_only_holds_sample_blocks() {
    let rows = "[\"....\", \".##.\", \".##.\", \"....\"]";
    let g = solve_graph(&[
        ("mode", "overlapping".to_string()),
        ("sample", rows.to_string()),
        ("n", "2".to_string()),
        ("size", "(10.0, 10.0, 1.0)".to_string()),
    ]);
    let r = run(&g, empty_snapshot(), 3);
    let p = out_points(&r, "out");
    let tile = names(&p, "tile");
    assert_eq!(tile.len(), 100);
    // Every 2 × 2 block of the output's symbols occurs in the (wrapping) sample.
    let sample: Vec<Vec<char>> = ["....", ".##.", ".##.", "...."]
        .iter()
        .map(|r| r.chars().collect())
        .collect();
    let mut blocks = BTreeSet::new();
    for y in 0..4 {
        for x in 0..4 {
            blocks.insert([
                sample[y][x],
                sample[y][(x + 1) % 4],
                sample[(y + 1) % 4][x],
                sample[(y + 1) % 4][(x + 1) % 4],
            ]);
        }
    }
    let at = |x: usize, y: usize| tile[x + 10 * y].chars().next().unwrap_or('?');
    for y in 0..9 {
        for x in 0..9 {
            let b = [at(x, y), at(x + 1, y), at(x, y + 1), at(x + 1, y + 1)];
            assert!(blocks.contains(&b), "block at ({x}, {y}): {b:?}");
        }
    }
}

#[test]
fn wfc_solve_validation_and_run_errors_have_stable_codes() {
    let reg = regia_procgen::ProcNodeRegistry::with_builtins();
    let codes = |settings: &[(&str, &str)]| -> Vec<String> {
        let g = graph(vec![(1, node("wfc.solve", settings))], &[]);
        match g.compile(&reg) {
            Ok(_) => Vec::new(),
            Err(e) => e.0.iter().map(|x| x.code.clone()).collect(),
        }
    };
    assert!(codes(&[]).is_empty(), "defaults validate: {:?}", codes(&[]));
    assert!(codes(&[("size", "(2.5, 4.0, 1.0)")]).contains(&"setting_out_of_range".to_string()));
    assert!(
        codes(&[("tiles", "[(name: \"a\", sockets: (\"a\", \"a\", \"a\"))]")])
            .contains(&"setting_type".to_string()),
        "three sockets"
    );
    assert!(!codes(&[("symmetry", "spin")]).is_empty(), "unknown symmetry");
    // No tiles at all fails the run.
    let g = graph(
        vec![(1, node("wfc.solve", &[])), (2, node("graph.output", &[]))],
        &[(1, "out", 2, "in")],
    );
    match run_with(&compile(&g), &RunInputs::new(empty_snapshot(), 1)) {
        Err(RunError::Node { code, .. }) => assert_eq!(code, "wfc_invalid_model"),
        other => panic!("expected wfc_invalid_model, got {other:?}"),
    }
    // A tile whose east side matches nothing cannot fill a 2-wide grid.
    let g = solve_graph(&[
        (
            "tiles",
            "[(name: \"a\", sockets: (\"x\", \"a\", \"y\", \"a\"))]".to_string(),
        ),
        ("size", "(2.0, 1.0, 1.0)".to_string()),
    ]);
    match run_with(&compile(&g), &RunInputs::new(empty_snapshot(), 1)) {
        Err(RunError::Node { code, .. }) => {
            assert!(
                code == "wfc_contradiction" || code == "wfc_backtrack_limit",
                "{code}"
            )
        }
        other => panic!("expected a contradiction, got {other:?}"),
    }
    // A border tile pins every edge cell.
    let mut s = socket_settings("rotate", "(8.0, 6.0, 1.0)");
    s.push(("border", "\"grass\"".to_string()));
    let p = out_points(&run(&solve_graph(&s), empty_snapshot(), 2), "out");
    let tile = names(&p, "tile");
    let mut counts: BTreeMap<bool, usize> = BTreeMap::new();
    for (i, t) in tile.iter().enumerate() {
        let (x, y) = (i % 8, i / 8);
        let edge = x == 0 || y == 0 || x == 7 || y == 5;
        if edge {
            assert_eq!(t, "grass", "border cell ({x}, {y})");
        }
        *counts.entry(edge).or_default() += 1;
    }
    assert_eq!(counts.get(&true), Some(&24));
}

#[test]
fn the_cancel_check_stops_the_solver_at_that_decision_and_otherwise_changes_nothing() {
    let model = match TiledModel::from_sockets(&corner_set(), Symmetry::All) {
        Ok(m) => WfcModel::Tiled(m),
        Err(e) => panic!("model: {e}"),
    };
    let size = GridSize::new_2d(16, 16);
    let opts = WfcOptions::default();
    let constraints = CellConstraints::new();
    let solve = |checks: &std::cell::Cell<usize>, stop_at: usize| {
        regia_procgen::wfc::solve_cancellable(&model, size, &constraints, 7, &opts, &|| {
            checks.set(checks.get() + 1);
            checks.get() >= stop_at
        })
    };

    // Never tripped: the same grid as the plain solve, and it was asked at every decision.
    let checks = std::cell::Cell::new(0);
    let free = solve(&checks, usize::MAX);
    let plain = regia_procgen::wfc::solve(&model, size, &constraints, 7, &opts);
    assert_eq!(free, plain);
    let asked = checks.get();
    assert!(asked > 1, "the check runs during the solve ({asked} checks)");

    // Tripped at the fifth check: it stops there, with its own error.
    let checks = std::cell::Cell::new(0);
    assert_eq!(solve(&checks, 5), Err(regia_procgen::wfc::WfcError::Cancelled));
    assert_eq!(checks.get(), 5, "no further check after the one that tripped");
}

/// Trips the run's token from inside a wave, so `wfc.solve` is already scheduled and only the
/// solver's own check can stop it.
struct TripNode;

impl regia_procgen::nodes::NodeKind for TripNode {
    fn id(&self) -> &str {
        "test.trip"
    }
    fn category(&self) -> &str {
        "test"
    }
    fn doc(&self) -> &str {
        "Cancels the run"
    }
    fn settings(&self) -> &[regia_procgen::nodes::SettingDesc] {
        &[]
    }
    fn pins(&self, _: &regia_procgen::nodes::Settings) -> regia_procgen::nodes::PinSpec {
        regia_procgen::nodes::PinSpec::new(
            Vec::new(),
            vec![regia_procgen::nodes::PinDesc {
                name: "out".into(),
                kinds: regia_procgen::KindSet::ANY,
                multi: false,
                required: false,
            }],
        )
    }
    fn eval(
        &self,
        ctx: &mut regia_procgen::exec::NodeCtx<'_>,
        _: &[regia_procgen::Bundle],
        _: &regia_procgen::nodes::Settings,
    ) -> Result<Vec<regia_procgen::Bundle>, regia_procgen::nodes::NodeError> {
        ctx.cancel.cancel();
        Ok(vec![regia_procgen::Bundle::empty()])
    }
}

#[test]
fn a_cancel_during_the_solve_stops_a_grid_that_would_otherwise_run_for_ages() {
    // 512 x 512 cells scan every cell per decision (about 7e10 cell visits): without the solver's
    // check this test does not finish in any reasonable time.
    let mut registry = regia_procgen::ProcNodeRegistry::with_builtins();
    registry.register(std::sync::Arc::new(TripNode));
    let g = graph(
        vec![
            (1, node("test.trip", &[])),
            (
                2,
                node(
                    "wfc.solve",
                    &[
                        (
                            "tiles",
                            "[(name: \"a\", sockets: (\"x\", \"x\", \"x\", \"x\")), (name: \"b\", sockets: (\"x\", \"x\", \"x\", \"x\"))]",
                        ),
                        ("size", "(512.0, 512.0, 1.0)"),
                    ],
                ),
            ),
            (3, node("graph.output", &[("pin", "\"tripped\"")])),
            (4, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 4, "in")],
    );
    let compiled = match g.compile(&registry) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let evaluator =
        regia_procgen::Evaluator::new().with_parallelism(regia_procgen::exec::Parallelism::threads(2));
    let run = evaluator.run(
        &compiled,
        &RunInputs::new(empty_snapshot(), 3),
        &regia_procgen::CancelToken::new(),
    );
    assert_eq!(run.err(), Some(RunError::Cancelled));
}
