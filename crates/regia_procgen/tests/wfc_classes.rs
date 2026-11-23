// SPDX-License-Identifier: MPL-2.0
//! WFC tile symmetry classes and 3D tiles (ADR-3027): per-class variant counts under each symmetry
//! group, adjacency that matches sockets recomputed here, class/socket disagreement refused, a 3D
//! tile set with turns about the vertical axis solving a 16 x 4 x 16 volume, and the `wfc.solve`
//! node agreeing with the solver for an explicit `seed` on every schedule.

mod common;

use std::collections::BTreeSet;

use common::*;
use regia_procgen::exec::Parallelism;
use regia_procgen::wfc::{
    CellConstraints, Direction, GridSize, Symmetry, TileClass, TileSpec, TiledModel, WfcModel, WfcOptions,
};
use regia_procgen::{AttrColumn, CancelToken, Evaluator, RunInputs};

fn spec(name: &str, sockets: [&str; 4], class: Option<TileClass>) -> TileSpec {
    TileSpec {
        name: name.to_string(),
        weight: 1.0,
        sockets: sockets.map(str::to_string),
        class,
        vertical: None,
    }
}

fn rev(t: &str) -> String {
    t.chars().rev().collect()
}

/// The sockets of a variant, recomputed here (independently of `wfc.rs`).
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

/// One tile per class, with sockets that have that class's symmetry (orientations as documented on
/// `TileClass`). The `F` tile is a dead end whose sockets are mirror-symmetric: the class, not the
/// sockets, decides that its mirror is a different picture.
fn class_tiles() -> Vec<(TileClass, TileSpec)> {
    vec![
        (TileClass::X, spec("x", ["a", "a", "a", "a"], Some(TileClass::X))),
        (TileClass::I, spec("i", ["g", "r", "g", "r"], Some(TileClass::I))),
        // A diagonal through the north-west and south-east corners: each socket reads the two
        // corners of its side clockwise (X = the line touches that corner).
        (
            TileClass::Slash,
            spec("slash", ["oX", "Xo", "oX", "Xo"], Some(TileClass::Slash)),
        ),
        (TileClass::T, spec("t", ["r", "r", "r", "g"], Some(TileClass::T))),
        (TileClass::L, spec("l", ["r", "g", "g", "r"], Some(TileClass::L))),
        (TileClass::F, spec("f", ["r", "g", "g", "g"], Some(TileClass::F))),
    ]
}

#[test]
fn each_class_gives_the_expected_variant_count_under_each_group() {
    // (class, all, rotations, reflections): |G| / |G ∩ stabilizer|.
    let expected = [
        (TileClass::X, 1, 1, 1),
        (TileClass::I, 2, 2, 1),
        (TileClass::Slash, 2, 2, 2),
        (TileClass::T, 4, 4, 1),
        (TileClass::L, 4, 4, 2),
        (TileClass::F, 8, 4, 2),
    ];
    for (class, tile) in class_tiles() {
        let count = |sym| match TiledModel::from_specs(std::slice::from_ref(&tile), sym) {
            Ok(m) => m.tile_count(),
            Err(e) => panic!("{class:?}: {e}"),
        };
        let row = expected
            .iter()
            .find(|r| r.0 == class)
            .copied()
            .unwrap_or((class, 0, 0, 0));
        assert_eq!(count(Symmetry::All), row.1, "{class:?} under all eight");
        assert_eq!(count(Symmetry::Rotations), row.2, "{class:?} under rotations");
        assert_eq!(count(Symmetry::Reflections), row.3, "{class:?} under the mirror");
        assert_eq!(count(Symmetry::None), 1, "{class:?} alone");
        assert_eq!(class.cardinality(), row.1, "{class:?} cardinality");
        assert_eq!(TileClass::from_name(class.name()), Some(class));
    }
    // Without a class the dead end keeps one variant per distinct socket set: 4, not 8.
    let plain = spec("f", ["r", "g", "g", "g"], None);
    let m = TiledModel::from_specs(&[plain], Symmetry::All).map(|m| m.tile_count());
    assert_eq!(m, Ok(4));
}

#[test]
fn a_class_whose_symmetry_the_sockets_lack_is_refused() {
    // Declared T (symmetric about north-south) but the only road leaves east.
    let bad = spec("t", ["r", "g", "g", "g"], Some(TileClass::T));
    let err = TiledModel::from_specs(&[bad], Symmetry::All).map(|m| m.tile_count());
    match err {
        Err(regia_procgen::wfc::WfcError::InvalidModel(m)) => assert!(m.contains("`t`"), "{m}"),
        other => panic!("expected InvalidModel, got {other:?}"),
    }
}

#[test]
fn classed_adjacency_matches_independently_turned_sockets_and_solved_grids_obey_it() {
    let tiles: Vec<TileSpec> = class_tiles()
        .into_iter()
        .map(|(_, t)| t)
        .filter(|t| t.name != "slash" && t.name != "x")
        .chain([spec("grass", ["g", "g", "g", "g"], Some(TileClass::X))])
        .collect();
    let model = match TiledModel::from_specs(&tiles, Symmetry::All) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    // 2 + 4 + 4 + 8 + 1.
    assert_eq!(model.tile_count(), 19);
    let sockets_of = |id: usize| {
        let v = model.variant(id).unwrap_or_default();
        turned(&tiles[v.base].sockets, v.rotation, v.mirrored)
    };
    let n = model.tile_count();
    for a in 0..n {
        for b in 0..n {
            let (sa, sb) = (sockets_of(a), sockets_of(b));
            assert_eq!(
                model.allows(a, Direction::East, b),
                sa[0] == rev(&sb[2]),
                "{a} east {b}"
            );
            assert_eq!(
                model.allows(a, Direction::South, b),
                sa[1] == rev(&sb[3]),
                "{a} south {b}"
            );
            assert_eq!(
                model.allows(a, Direction::West, b),
                sa[2] == rev(&sb[0]),
                "{a} west {b}"
            );
            assert_eq!(
                model.allows(a, Direction::North, b),
                sa[3] == rev(&sb[1]),
                "{a} north {b}"
            );
        }
    }
    let size = GridSize::new_2d(20, 14);
    let grid = match regia_procgen::wfc::solve(
        &WfcModel::Tiled(model.clone()),
        size,
        &CellConstraints::new(),
        41,
        &WfcOptions::default(),
    ) {
        Ok(g) => g,
        Err(e) => panic!("solve: {e}"),
    };
    let mut used = BTreeSet::new();
    for y in 0..size.height {
        for x in 0..size.width {
            let a = grid.get_2d(x, y).unwrap_or(0);
            used.insert(a);
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
    assert!(used.len() > 4, "several variants are used: {used:?}");
}

/// A road network on stacked layers: roads, corners and junctions turn about the vertical axis; air
/// and roads stack freely (`a`), and a beam only stacks on a beam turned the same way (`^b`).
fn volume_tiles() -> Vec<TileSpec> {
    let v = |t: TileSpec, up: &str, down: &str| TileSpec {
        vertical: Some([up.to_string(), down.to_string()]),
        ..t
    };
    vec![
        v(spec("air", ["e", "e", "e", "e"], Some(TileClass::X)), "a", "a"),
        v(spec("grass", ["g", "g", "g", "g"], Some(TileClass::X)), "a", "a"),
        v(spec("road", ["g", "r", "g", "r"], Some(TileClass::I)), "a", "a"),
        v(spec("corner", ["r", "g", "g", "r"], Some(TileClass::L)), "a", "a"),
        v(spec("tee", ["r", "r", "r", "g"], Some(TileClass::T)), "a", "a"),
        v(spec("beam", ["e", "e", "e", "e"], Some(TileClass::I)), "^b", "^b"),
    ]
}

#[test]
fn a_3d_tile_set_with_vertical_turns_solves_a_16_by_4_by_16_volume() {
    let tiles = volume_tiles();
    let model = match TiledModel::from_specs(&tiles, Symmetry::Rotations) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    // air 1, grass 1, road 2, corner 4, tee 4, beam 2.
    assert_eq!(model.tile_count(), 14);
    let id = |name: &str| model.tile_id(name).unwrap_or(usize::MAX);
    assert!(model.allows(id("beam"), Direction::Up, id("beam")));
    assert!(model.allows(id("beam@r1"), Direction::Up, id("beam@r1")));
    assert!(
        !model.allows(id("beam"), Direction::Up, id("beam@r1")),
        "a turn-sensitive socket"
    );
    assert!(
        !model.allows(id("beam"), Direction::Up, id("air")),
        "`^b` meets only `^b`"
    );
    assert!(
        model.allows(id("road@r1"), Direction::Up, id("air")),
        "plain sockets ignore turns"
    );

    // 16 cells along x, 16 rows along z, 4 layers along y.
    let size = GridSize::new_3d(16, 16, 4);
    let grid = match regia_procgen::wfc::solve(
        &WfcModel::Tiled(model.clone()),
        size,
        &CellConstraints::new(),
        2912,
        &WfcOptions::default(),
    ) {
        Ok(g) => g,
        Err(e) => panic!("solve: {e}"),
    };
    assert_eq!(grid.len(), 16 * 16 * 4);
    let mut turns = BTreeSet::new();
    for (i, &a) in grid.tiles().iter().enumerate() {
        let Some([x, y, z]) = size.coords(i) else {
            panic!("cell {i} outside the grid");
        };
        turns.insert(model.variant(a).map_or(0, |v| v.rotation));
        for dir in Direction::ALL_3D {
            let d = dir.delta();
            let (nx, ny, nz) = (
                x as i64 + i64::from(d[0]),
                y as i64 + i64::from(d[1]),
                z as i64 + i64::from(d[2]),
            );
            if nx < 0 || ny < 0 || nz < 0 {
                continue;
            }
            let Some(j) = size.index(nx as usize, ny as usize, nz as usize) else {
                continue;
            };
            let b = grid.tiles()[j];
            assert!(
                model.allows(a, dir, b),
                "({x}, {y}, {z}) {dir:?}: {a} next to {b}"
            );
        }
    }
    assert!(turns.len() > 1, "turned variants appear: {turns:?}");
}

fn volume_settings(seed: &str) -> Vec<(&'static str, String)> {
    let tiles = "[\
        (name: \"air\", sockets: (\"e\", \"e\", \"e\", \"e\"), class: \"X\", up: \"a\", down: \"a\"),\
        (name: \"grass\", sockets: (\"g\", \"g\", \"g\", \"g\"), class: \"X\", up: \"a\", down: \"a\"),\
        (name: \"road\", sockets: (\"g\", \"r\", \"g\", \"r\"), class: \"I\", up: \"a\", down: \"a\"),\
        (name: \"corner\", sockets: (\"r\", \"g\", \"g\", \"r\"), class: \"L\", up: \"a\", down: \"a\"),\
        (name: \"tee\", sockets: (\"r\", \"r\", \"r\", \"g\"), class: \"T\", up: \"a\", down: \"a\"),\
        (name: \"beam\", sockets: (\"e\", \"e\", \"e\", \"e\"), class: \"I\", up: \"^b\", down: \"^b\"),\
    ]";
    vec![
        ("mode", "tiled".to_string()),
        ("tiles", tiles.to_string()),
        ("symmetry", "rotate".to_string()),
        ("size", "(16.0, 16.0, 4.0)".to_string()),
        ("seed", seed.to_string()),
    ]
}

fn solve_graph(settings: &[(&str, String)]) -> regia_procgen::ProcGraph {
    let s: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (*k, v.as_str())).collect();
    graph(
        vec![
            (1, node("wfc.solve", &s)),
            (2, node("out.points", &[("name", "\"cells\"")])),
        ],
        &[(1, "out", 2, "in")],
    )
}

fn ints(p: &regia_procgen::PointSet, name: &str) -> Vec<i64> {
    match p.attrs.get(name) {
        Some(AttrColumn::Int(v)) => v.to_vec(),
        _ => Vec::new(),
    }
}

#[test]
fn the_node_with_a_seed_equals_the_solver_on_every_schedule_and_run() {
    let c = compile(&solve_graph(&volume_settings("2912")));
    let snap = snapshot(box_shape([0.0; 3], [50.0; 3], 0.0), None);
    let model = match TiledModel::from_specs(&volume_tiles(), Symmetry::Rotations) {
        Ok(m) => m,
        Err(e) => panic!("model: {e}"),
    };
    let direct = match regia_procgen::wfc::solve(
        &WfcModel::Tiled(model),
        GridSize::new_3d(16, 16, 4),
        &CellConstraints::new(),
        2912,
        &WfcOptions::default(),
    ) {
        Ok(g) => g.tiles().iter().map(|&t| t as i64).collect::<Vec<_>>(),
        Err(e) => panic!("solve: {e}"),
    };
    // Per generator seed: one output hash for threads None, 1 and 8 over two runs. The grid is the
    // solver's for both generator seeds (an explicit `seed` picks it); the hash also covers the
    // per-point random seeds, which follow the generator seed.
    let mut hashes: [BTreeSet<[u8; 32]>; 2] = [BTreeSet::new(), BTreeSet::new()];
    let schedules = [None, Some(1), Some(8), None, Some(1), Some(8)];
    for (g, gen_seed) in [5u32, 6].into_iter().enumerate() {
        for threads in schedules {
            let ev = match threads {
                None => Evaluator::new(),
                Some(t) => Evaluator::new().with_parallelism(Parallelism::threads(t)),
            };
            let r = match ev.run(&c, &RunInputs::new(snap.clone(), gen_seed), &CancelToken::new()) {
                Ok(r) => r,
                Err(e) => panic!("run {threads:?}: {e}"),
            };
            let p = exported(&r, "cells");
            assert_eq!(
                ints(&p, "tile_id"),
                direct,
                "{threads:?}: the node's grid is the solver's"
            );
            hashes[g].insert(r.output_hash);
        }
    }
    for (g, h) in hashes.iter().enumerate() {
        assert_eq!(
            h.len(),
            1,
            "generator seed {g}: one output hash for every schedule and run"
        );
    }

    // The default `-1` takes the generator seed, so two generator seeds differ.
    let c = compile(&solve_graph(&volume_settings("-1")));
    let tiles = |seed| {
        let r = match Evaluator::new().run(&c, &RunInputs::new(snap.clone(), seed), &CancelToken::new()) {
            Ok(r) => r,
            Err(e) => panic!("run: {e}"),
        };
        ints(&exported(&r, "cells"), "tile_id")
    };
    assert_ne!(tiles(5), tiles(6));
}

#[test]
fn a_bounds_input_sets_the_grid_origin_and_cell_counts() {
    // A 6 x 4 m box of points (1 m cells) from points.grid, at an offset.
    let settings = [
        ("mode", "tiled".to_string()),
        (
            "tiles",
            "[(name: \"g\", sockets: (\"g\", \"g\", \"g\", \"g\"), class: \"X\")]".to_string(),
        ),
        ("size", "(99.0, 99.0, 1.0)".to_string()),
        ("cell_m", "2.0".to_string()),
    ];
    let s: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (2, node("wfc.solve", &s)),
            (3, node("out.points", &[("name", "\"cells\"")])),
        ],
        &[(1, "out", 2, "bounds"), (2, "out", 3, "in")],
    );
    let snap = snapshot(box_shape([10.0, 0.0, 20.0], [6.0, 1.0, 4.0], 0.0), None);
    let r = run(&g, snap, 1);
    let p = exported(&r, "cells");
    // 12 m by 8 m at 2 m cells: 6 x 4 cells, origin at the box's min corner (4, -1, 16).
    assert_eq!(p.len(), 24, "the input's box sets the cell counts, not `size`");
    let first = p.point(0);
    assert_eq!(first.position, [5.0, -1.0, 17.0], "first cell center");
    let last = p.point(23);
    assert_eq!(last.position, [15.0, -1.0, 23.0], "last cell center");
}

#[test]
fn a_tileset_table_gives_the_same_grid_as_the_inline_tiles() {
    let snap = snapshot(box_shape([0.0; 3], [50.0; 3], 0.0), None);
    let inline = run(&solve_graph(&volume_settings("77")), snap.clone(), 1);
    let columns = "[\
        (\"name\", Name, [\"air\", \"grass\", \"road\", \"corner\", \"tee\", \"beam\"]),\
        (\"east\", Name, [\"e\", \"g\", \"g\", \"r\", \"r\", \"e\"]),\
        (\"south\", Name, [\"e\", \"g\", \"r\", \"g\", \"r\", \"e\"]),\
        (\"west\", Name, [\"e\", \"g\", \"g\", \"g\", \"r\", \"e\"]),\
        (\"north\", Name, [\"e\", \"g\", \"r\", \"r\", \"g\", \"e\"]),\
        (\"class\", Name, [\"X\", \"X\", \"I\", \"L\", \"T\", \"I\"]),\
        (\"up\", Name, [\"a\", \"a\", \"a\", \"a\", \"a\", \"^b\"]),\
        (\"down\", Name, [\"a\", \"a\", \"a\", \"a\", \"a\", \"^b\"]),\
    ]";
    let settings: Vec<(&str, String)> = volume_settings("77")
        .into_iter()
        .map(|(k, v)| {
            if k == "tiles" {
                (k, "[]".to_string())
            } else {
                (k, v)
            }
        })
        .collect();
    let s: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let g = graph(
        vec![
            (1, node("table.make", &[("columns", columns)])),
            (2, node("wfc.solve", &s)),
            (3, node("out.points", &[("name", "\"cells\"")])),
        ],
        &[(1, "out", 2, "tileset"), (2, "out", 3, "in")],
    );
    let from_table = run(&g, snap, 1);
    let ids = |r: &regia_procgen::RunResult| ints(&exported(r, "cells"), "tile_id");
    assert_eq!(ids(&from_table).len(), 16 * 16 * 4);
    assert_eq!(ids(&from_table), ids(&inline));
    assert_eq!(from_table.output_hash, inline.output_hash);
}

/// The points `out.points { name }` exported.
fn exported(r: &regia_procgen::RunResult, name: &str) -> regia_procgen::PointSet {
    r.output.points.get(name).cloned().unwrap_or_default()
}
