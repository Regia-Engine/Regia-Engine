// SPDX-License-Identifier: MPL-2.0
//! Exhaustive reference checks of existing WFC contracts, without propagation,
//! model adjacency queries, production direction arithmetic, or grid validation.
//!
//! The catalog has 256 two-tile and 512 three-tile authored matrices, each on
//! 14 shapes, two boundaries, three constraint histories and four fixed seeds:
//! 64,512 reference solution sets and 258,048 complete-budget solver calls.
//! Every reference set enumerates all assignments (at most 3^6 = 729).

use std::collections::BTreeSet;

use regia_procgen::wfc::{
    AdjacencyRule, Boundary, CellConstraints, Direction, GridSize, TileDef, TiledModel, WfcError, WfcOptions,
};

const SHAPES: [[usize; 2]; 14] = [
    [1, 1],
    [1, 2],
    [2, 1],
    [1, 3],
    [3, 1],
    [1, 4],
    [4, 1],
    [2, 2],
    [1, 5],
    [5, 1],
    [1, 6],
    [6, 1],
    [2, 3],
    [3, 2],
];
const SEEDS: [u64; 4] = [0, 1, 0x5eed, u64::MAX];
// Deliberately independent of Direction::{index,delta,opposite,ALL_2D}.
const STEPS: [(Direction, isize, isize); 4] = [
    (Direction::East, 1, 0),
    (Direction::West, -1, 0),
    (Direction::South, 0, 1),
    (Direction::North, 0, -1),
];

// Keep the authored history, not CellConstraints::get/iter: the last write to a
// cell replaces its subset; different forced cells must all hold simultaneously.
type Forces = Vec<([usize; 2], Vec<usize>)>;
type Solutions = BTreeSet<Vec<usize>>;

fn authored_allows(rules: &[AdjacencyRule], a: usize, dir: Direction, b: usize) -> bool {
    let rows: Vec<_> = rules
        .iter()
        .filter(|r| r.tile == a && r.direction == dir)
        .collect();
    // No row means universal; multiple authored rows form a union, including
    // empty rows. Checking every directed edge below intersects opposite sides.
    rows.is_empty() || rows.iter().any(|r| r.allowed.contains(&b))
}

fn reference_solutions(
    tile_count: usize,
    shape: [usize; 2],
    boundary: Boundary,
    rules: &[AdjacencyRule],
    forces: &Forces,
) -> Solutions {
    let [width, height] = shape;
    let cells = width * height;
    assert!((1..=6).contains(&cells));
    assert!((2..=3).contains(&tile_count));
    let assignments = (0..cells).fold(1, |n, _| n * tile_count);
    let mut solutions = Solutions::new();
    'assignment: for mut number in 0..assignments {
        let mut tiles = Vec::with_capacity(cells);
        for _ in 0..cells {
            tiles.push(number % tile_count);
            number /= tile_count;
        }
        for y in 0..height {
            for x in 0..width {
                let a = tiles[y * width + x];
                if let Some((_, allowed)) = forces.iter().rev().find(|(at, _)| *at == [x, y])
                    && !allowed.contains(&a)
                {
                    continue 'assignment;
                }
                for (dir, dx, dy) in STEPS {
                    let nx = x as isize + dx;
                    let ny = y as isize + dy;
                    let outside = nx < 0 || ny < 0 || nx >= width as isize || ny >= height as isize;
                    if boundary == Boundary::Fixed && outside {
                        continue;
                    }
                    let nx = nx.rem_euclid(width as isize) as usize;
                    let ny = ny.rem_euclid(height as isize) as usize;
                    let b = tiles[ny * width + nx];
                    if !authored_allows(rules, a, dir, b) {
                        continue 'assignment;
                    }
                }
            }
        }
        solutions.insert(tiles);
    }
    solutions
}

fn constraints(forces: &Forces) -> CellConstraints {
    let mut constraints = CellConstraints::new();
    for ([x, y], allowed) in forces {
        constraints.force_2d(*x, *y, allowed);
    }
    constraints
}

fn complete_budget(tile_count: usize, cells: usize) -> usize {
    // Each binary decision is x=v versus x!=v. Either branch removes at least
    // one of the initial cells*(tile_count-1) excess domain values. Even with
    // changing variable order the entire binary tree has fewer than 2^(d+1)
    // nodes. Charging every node a backtrack is a conservative upper bound.
    1 << (cells * (tile_count - 1) + 1)
}

fn check_case(
    tile_count: usize,
    model: &TiledModel,
    rules: &[AdjacencyRule],
    shape: [usize; 2],
    boundary: Boundary,
    forces: &Forces,
    seeds: &[u64],
) -> usize {
    let solutions = reference_solutions(tile_count, shape, boundary, rules, forces);
    let options = WfcOptions {
        boundary,
        max_backtracks: complete_budget(tile_count, shape[0] * shape[1]),
    };
    for &seed in seeds {
        let result = model.solve(
            GridSize::new_2d(shape[0], shape[1]),
            &constraints(forces),
            seed,
            &options,
        );
        let context = || {
            format!(
                "tiles={tile_count} shape={shape:?} boundary={boundary:?} forces={forces:?} \
                 seed={seed} budget={} rules={rules:?} reference_count={}",
                options.max_backtracks,
                solutions.len()
            )
        };
        match result {
            Ok(grid) => {
                assert_eq!(grid.size(), GridSize::new_2d(shape[0], shape[1]), "{}", context());
                assert!(
                    solutions.contains(grid.tiles()),
                    "nonmember success {:?}: {}",
                    grid.tiles(),
                    context()
                );
            }
            Err(WfcError::Contradiction) => {
                assert!(
                    solutions.is_empty(),
                    "false unsatisfiability: {} solutions={solutions:?}",
                    context()
                );
            }
            Err(error) => panic!("complete search returned {error:?}: {}", context()),
        }
    }
    solutions.len()
}

fn transpose(mask: usize, count: usize) -> usize {
    let mut transposed = 0;
    for a in 0..count {
        for b in 0..count {
            if mask & (1 << (a * count + b)) != 0 {
                transposed |= 1 << (b * count + a);
            }
        }
    }
    transposed
}

fn append_rows(rules: &mut Vec<AdjacencyRule>, count: usize, dir: Direction, mask: usize, split: bool) {
    for a in 0..count {
        let allowed: Vec<_> = (0..count)
            .filter(|b| mask & (1 << (a * count + b)) != 0)
            .collect();
        if allowed.len() == count {
            // Exercise the documented universal default rather than spelling
            // every universal row. Empty rows, in contrast, must be authored.
            continue;
        }
        if split {
            rules.push(AdjacencyRule::new(a, dir, vec![]));
            for b in allowed {
                // Duplicate ids and repeated rows must still be set union.
                rules.push(AdjacencyRule::new(a, dir, vec![b, b]));
            }
        } else {
            rules.push(AdjacencyRule::new(a, dir, allowed));
        }
    }
}

fn catalog_rules(count: usize, horizontal: usize, vertical: usize) -> Vec<AdjacencyRule> {
    let mut rules = Vec::new();
    let encoding = (horizontal + 3 * vertical) % 4;
    match encoding {
        0 => {
            append_rows(&mut rules, count, Direction::East, horizontal, false);
            append_rows(&mut rules, count, Direction::South, vertical, false);
        }
        1 => {
            append_rows(
                &mut rules,
                count,
                Direction::West,
                transpose(horizontal, count),
                true,
            );
            append_rows(
                &mut rules,
                count,
                Direction::North,
                transpose(vertical, count),
                true,
            );
        }
        _ => {
            append_rows(&mut rules, count, Direction::East, horizontal, true);
            append_rows(&mut rules, count, Direction::South, vertical, true);
            // Encoding 2 deliberately disagrees across opposite sides. The
            // legal edge is the intersection, never a union or auto-mirroring.
            let reverse_h = if encoding == 2 { vertical } else { horizontal };
            let reverse_v = if encoding == 2 { horizontal } else { vertical };
            append_rows(
                &mut rules,
                count,
                Direction::West,
                transpose(reverse_h, count),
                false,
            );
            append_rows(
                &mut rules,
                count,
                Direction::North,
                transpose(reverse_v, count),
                false,
            );
        }
    }
    rules
}

fn model(count: usize, rules: &[AdjacencyRule]) -> TiledModel {
    TiledModel::new(
        (0..count).map(|id| TileDef::new(format!("tile_{id}"))).collect(),
        rules.to_vec(),
    )
    .expect("catalog rules and weights are well formed")
}

fn catalog(count: usize, matrices: impl Iterator<Item = (usize, usize)>) -> (usize, usize) {
    let mut cases = 0;
    let mut unique = 0;
    for (horizontal, vertical) in matrices {
        let rules = catalog_rules(count, horizontal, vertical);
        let model = model(count, &rules);
        for shape in SHAPES {
            let last = [shape[0] - 1, shape[1] - 1];
            let histories = [
                vec![],
                vec![([0, 0], vec![0])],
                vec![([0, 0], vec![0]), (last, vec![1])],
            ];
            for boundary in [Boundary::Fixed, Boundary::Periodic] {
                for forces in &histories {
                    let solutions = check_case(count, &model, &rules, shape, boundary, forces, &SEEDS);
                    unique += usize::from(solutions == 1);
                    cases += 1;
                }
            }
        }
    }
    (cases, unique)
}

#[test]
fn two_tile_authored_matrix_catalog_matches_the_exhaustive_oracle() {
    let (cases, unique) = catalog(2, (0..16).flat_map(|h| (0..16).map(move |v| (h, v))));
    assert_eq!(cases, 21_504);
    assert!(unique > 0, "catalog must exercise single-solution models");
    eprintln!(
        "two tiles: {cases} reference sets, {} complete solves, {unique} unique sets",
        cases * SEEDS.len()
    );
}

#[test]
fn every_three_tile_horizontal_relation_matches_the_exhaustive_oracle() {
    // All 512 three-by-three horizontal bit matrices. Rotate nine bits by
    // three and xor a constant for a deterministic bijection of vertical
    // matrices. This is a bounded catalog, not all 512^2 matrix pairs.
    let (cases, unique) = catalog(3, (0..512).map(|h| (h, (((h << 3) | (h >> 6)) & 511) ^ 0x12d)));
    assert_eq!(cases, 43_008);
    assert!(unique > 0, "catalog must exercise single-solution models");
    eprintln!(
        "three tiles: {cases} reference sets, {} complete solves, {unique} unique sets",
        cases * SEEDS.len()
    );
}

#[test]
fn reference_sets_match_hand_counted_boundary_and_forcing_cases() {
    // These anchors test the reference itself rather than merely comparing
    // two implementations on a potentially misunderstood contract.
    for boundary in [Boundary::Fixed, Boundary::Periodic] {
        assert_eq!(reference_solutions(2, [2, 3], boundary, &[], &vec![]).len(), 64);
    }
    let mut equal = Vec::new();
    append_rows(&mut equal, 2, Direction::East, 0b1001, false);
    append_rows(&mut equal, 2, Direction::South, 0b1001, false);
    let equal_model = model(2, &equal);
    let forced = vec![([0, 0], vec![1])];
    for boundary in [Boundary::Fixed, Boundary::Periodic] {
        assert_eq!(
            reference_solutions(2, [2, 3], boundary, &equal, &vec![]),
            Solutions::from([vec![0; 6], vec![1; 6]])
        );
        assert_eq!(
            reference_solutions(2, [2, 3], boundary, &equal, &forced),
            Solutions::from([vec![1; 6]])
        );
        assert_eq!(
            check_case(2, &equal_model, &equal, [2, 3], boundary, &forced, &SEEDS),
            1
        );
        let contradiction = vec![([0, 0], vec![0]), ([1, 0], vec![1])];
        assert_eq!(
            check_case(2, &equal_model, &equal, [2, 1], boundary, &contradiction, &SEEDS),
            0
        );
    }
    let mut alternating = Vec::new();
    append_rows(&mut alternating, 2, Direction::East, 0b0110, true);
    for (shape, boundary, expected) in [
        ([1, 1], Boundary::Fixed, 2),
        ([1, 1], Boundary::Periodic, 0),
        ([2, 1], Boundary::Periodic, 2),
        ([3, 1], Boundary::Periodic, 0),
    ] {
        assert_eq!(
            reference_solutions(2, shape, boundary, &alternating, &vec![]).len(),
            expected
        );
        assert_eq!(
            check_case(
                2,
                &model(2, &alternating),
                &alternating,
                shape,
                boundary,
                &vec![],
                &SEEDS
            ),
            expected
        );
    }
}

#[test]
fn asymmetric_directional_cycles_and_replaced_subsets_have_exact_solutions() {
    // Only East or South is authored. Auto-symmetrizing by union would allow
    // the reversed cycle, while transposing the wrong direction reverses it.
    for (dir, shape, expected) in [
        (Direction::East, [3, 1], vec![1, 2, 0]),
        (Direction::South, [1, 3], vec![1, 2, 0]),
    ] {
        let rules = vec![
            AdjacencyRule::allow_one(0, dir, 1),
            AdjacencyRule::allow_one(1, dir, 2),
            AdjacencyRule::allow_one(2, dir, 0),
        ];
        let forces = vec![([0, 0], vec![0]), ([0, 0], vec![2, 1, 2]), ([0, 0], vec![1])];
        assert_eq!(
            reference_solutions(3, shape, Boundary::Periodic, &rules, &forces),
            Solutions::from([expected])
        );
        assert_eq!(
            check_case(
                3,
                &model(3, &rules),
                &rules,
                shape,
                Boundary::Periodic,
                &forces,
                &SEEDS
            ),
            1
        );
        // With the final write absent both allowed ids must remain possible.
        let subset = forces[..2].to_vec();
        assert_eq!(
            check_case(
                3,
                &model(3, &rules),
                &rules,
                shape,
                Boundary::Periodic,
                &subset,
                &SEEDS
            ),
            2
        );
    }
}

#[test]
fn small_budgets_are_inconclusive_for_both_sat_and_unsat_rings() {
    // Existing inline trap_ring_with_zero_backtrack_budget_hits_the_limit
    // uses four ids; this distinct three-id ring has exactly one solution.
    // The other two ids form an even cycle and cannot fill the odd ring.
    let mut saw_sat_limit = false;
    let mut saw_unsat_limit = false;
    for (count, mask, expected) in [(3, 0b010100001, 1), (2, 0b0110, 0)] {
        let mut rules = Vec::new();
        append_rows(&mut rules, count, Direction::East, mask, false);
        let model = model(count, &rules);
        let solutions = reference_solutions(count, [3, 1], Boundary::Periodic, &rules, &vec![]);
        assert_eq!(solutions.len(), expected);
        let seeds: Vec<_> = (0..32).collect();
        check_case(count, &model, &rules, [3, 1], Boundary::Periodic, &vec![], &seeds);
        for seed in seeds {
            let result = model.solve(
                GridSize::new_2d(3, 1),
                &CellConstraints::new(),
                seed,
                &WfcOptions::periodic().with_max_backtracks(0),
            );
            match result {
                Ok(grid) => assert!(solutions.contains(grid.tiles())),
                Err(WfcError::Contradiction) => assert!(solutions.is_empty()),
                Err(WfcError::BacktrackLimit) => {
                    saw_sat_limit |= !solutions.is_empty();
                    saw_unsat_limit |= solutions.is_empty();
                }
                Err(error) => panic!("well-formed small-budget ring: {error:?}"),
            }
        }
    }
    assert!(
        saw_sat_limit,
        "a budget failure must not be mistaken for unsatisfiability"
    );
    assert!(
        saw_unsat_limit,
        "an exhausted budget is not an unsatisfiability proof"
    );
}
