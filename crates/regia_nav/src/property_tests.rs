// SPDX-License-Identifier: MPL-2.0
//! Property tests for navmesh generation and pathfinding (reachability and no-cut-through).
//!
//! Every input comes from a fixed-seed generator, so the tests are deterministic; no test
//! depends on timing.

use std::collections::VecDeque;

use regia_core::math::Vec3;

use crate::builder::NavMeshBuilder;
use crate::config::NavMeshBuildParams;
use crate::heightfield::WalkableGrid;
use crate::mesh::NavMesh;
use crate::obstacle::NavObstacles;
use crate::query::{NavQuery, assert_waypoints_in_corridor};

/// A tiny xorshift generator (fixed seed → fixed test inputs).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A value in `[lo, hi)`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() >> 40) as f32 / (1u64 << 24) as f32 * (hi - lo)
    }
}

const CLIMB: f32 = 0.5;

/// A random walkable grid (1 m cells): ~70 % walkable, heights from a few levels so some
/// neighbours are steppable (0.3 m) and some are not (0.9 m).
fn random_grid(rng: &mut Rng, size: usize) -> WalkableGrid {
    let levels = [0.0f32, 0.0, 0.0, 0.3, 0.9];
    let cells = (0..size * size)
        .map(|_| (rng.below(10) < 7).then(|| levels[rng.below(levels.len())]))
        .collect();
    WalkableGrid {
        origin: Vec3::ZERO,
        cell_size: 1.0,
        width: size,
        height: size,
        cells,
        areas: Vec::new(),
    }
}

/// Connected-component ids over walkable cells (4-neighbour, step ≤ `CLIMB`); `usize::MAX` for
/// blocked cells. This is the reference the mesh is checked against.
fn components(grid: &WalkableGrid) -> Vec<usize> {
    let (w, h) = (grid.width, grid.height);
    let mut comp = vec![usize::MAX; w * h];
    let mut next = 0;
    for start in 0..w * h {
        if grid.cells[start].is_none() || comp[start] != usize::MAX {
            continue;
        }
        comp[start] = next;
        let mut queue = VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            let (x, z) = (cur % w, cur / w);
            let here = grid.cells[cur].unwrap_or(0.0);
            let mut neighbors = Vec::new();
            if x > 0 {
                neighbors.push(cur - 1);
            }
            if x + 1 < w {
                neighbors.push(cur + 1);
            }
            if z > 0 {
                neighbors.push(cur - w);
            }
            if z + 1 < h {
                neighbors.push(cur + w);
            }
            for n in neighbors {
                let Some(there) = grid.cells[n] else {
                    continue;
                };
                if comp[n] == usize::MAX && (here - there).abs() <= CLIMB + 1e-4 {
                    comp[n] = next;
                    queue.push_back(n);
                }
            }
        }
        next += 1;
    }
    comp
}

fn cell_center(grid: &WalkableGrid, idx: usize) -> Vec3 {
    let (x, z) = (idx % grid.width, idx / grid.width);
    Vec3::new(
        grid.origin.x + (x as f32 + 0.5) * grid.cell_size,
        grid.cells[idx].unwrap_or(0.0),
        grid.origin.z + (z as f32 + 0.5) * grid.cell_size,
    )
}

/// Whether `(x, z)` touches a walkable cell (a point on a shared edge or corner counts for
/// every cell around it).
fn touches_walkable(grid: &WalkableGrid, x: f32, z: f32) -> bool {
    let e = 1.0e-3;
    [(-e, -e), (e, -e), (-e, e), (e, e)].iter().any(|&(dx, dz)| {
        let gx = ((x + dx - grid.origin.x) / grid.cell_size).floor();
        let gz = ((z + dz - grid.origin.z) / grid.cell_size).floor();
        gx >= 0.0
            && gz >= 0.0
            && (gx as usize) < grid.width
            && (gz as usize) < grid.height
            && grid.cells[gz as usize * grid.width + gx as usize].is_some()
    })
}

fn walkable_indices(grid: &WalkableGrid) -> Vec<usize> {
    (0..grid.cells.len())
        .filter(|&i| grid.cells[i].is_some())
        .collect()
}

fn extents() -> Vec3 {
    // Narrow in XZ so a cell centre only ever resolves to its own polygon.
    Vec3::new(0.1, 0.1, 0.1)
}

#[test]
fn a_path_exists_exactly_when_the_cells_are_connected() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let none = NavObstacles::default();
    for size in [8usize, 12, 16, 20, 24] {
        let grid = random_grid(&mut rng, size);
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let comp = components(&grid);
        let walkable = walkable_indices(&grid);
        if walkable.len() < 2 {
            continue;
        }
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&none),
        };
        for _ in 0..60 {
            let a = walkable[rng.below(walkable.len())];
            let b = walkable[rng.below(walkable.len())];
            let path = q.find_path(cell_center(&grid, a), cell_center(&grid, b), extents());
            assert_eq!(
                path.is_some(),
                comp[a] == comp[b],
                "size {size}: cells {a} and {b} (components {} / {})",
                comp[a],
                comp[b]
            );
            let rev = q.find_path(cell_center(&grid, b), cell_center(&grid, a), extents());
            assert_eq!(path.is_some(), rev.is_some(), "reachability is symmetric");
        }
    }
}

#[test]
fn paths_never_cut_through_blocked_cells() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let none = NavObstacles::default();
    let mut checked = 0;
    for size in [10usize, 14, 18, 22] {
        let grid = random_grid(&mut rng, size);
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let comp = components(&grid);
        let walkable = walkable_indices(&grid);
        if walkable.len() < 2 {
            continue;
        }
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&none),
        };
        for _ in 0..60 {
            let a = walkable[rng.below(walkable.len())];
            let b = walkable[rng.below(walkable.len())];
            if comp[a] != comp[b] {
                continue;
            }
            let (start, end) = (cell_center(&grid, a), cell_center(&grid, b));
            let path = q.find_path(start, end, extents()).expect("connected cells");
            checked += 1;

            assert_eq!(path.waypoints.first(), Some(&start));
            assert_eq!(path.waypoints.last(), Some(&end));
            // Every sample of every segment is over walkable ground.
            for pair in path.waypoints.windows(2) {
                let steps = ((pair[1] - pair[0]).length() / 0.05).ceil().max(1.0) as usize;
                for s in 0..=steps {
                    let p = pair[0].lerp(pair[1], s as f32 / steps as f32);
                    assert!(
                        touches_walkable(&grid, p.x, p.z),
                        "size {size}: ({:.3}, {:.3}) is over a blocked cell; path {:?}",
                        p.x,
                        p.z,
                        path.waypoints
                    );
                }
            }
            // …and inside the polygon corridor the path reports.
            assert_waypoints_in_corridor(&mesh, &path.polys, &path.waypoints);
            // Consecutive corridor polygons are linked by a portal.
            for w in path.polys.windows(2) {
                assert!(
                    mesh.polys[w[0] as usize]
                        .portals
                        .iter()
                        .any(|p| p.neighbor == w[1]),
                    "corridor polygons {} and {} share no portal",
                    w[0],
                    w[1]
                );
            }
            // The reported length is the waypoint length, and no shorter than the straight line.
            let summed: f32 = path.waypoints.windows(2).map(|p| (p[1] - p[0]).length()).sum();
            assert!((summed - path.length).abs() < 1e-3);
            let straight = ((end.x - start.x).powi(2) + (end.z - start.z).powi(2)).sqrt();
            assert!(path.length + 1e-3 >= straight);
        }
    }
    assert!(
        checked > 50,
        "the generator produced too few connected pairs ({checked})"
    );
}

#[test]
fn polygons_tile_the_walkable_cells_without_overlap() {
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    for size in [9usize, 15, 21] {
        let grid = random_grid(&mut rng, size);
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let mut covered = vec![0u32; size * size];
        for poly in &mesh.polys {
            for z in 0..size {
                for x in 0..size {
                    if poly.contains_xz(x as f32 + 0.5, z as f32 + 0.5) {
                        covered[z * size + x] += 1;
                        assert_eq!(grid.cells[z * size + x], Some(poly.height()));
                    }
                }
            }
        }
        for (i, &n) in covered.iter().enumerate() {
            assert_eq!(n, u32::from(grid.cells[i].is_some()), "cell {i} (size {size})");
        }
    }
}

#[test]
fn find_nearest_honours_wide_extents_and_clamps_onto_the_mesh() {
    // One walkable cell in the far corner of a 12 x 12 grid, queried from the opposite corner
    // with an extent wider than a spatial bucket.
    let size = 12;
    let mut cells = vec![None; size * size];
    cells[(size - 1) * size + (size - 1)] = Some(0.0);
    let grid = WalkableGrid {
        origin: Vec3::ZERO,
        cell_size: 1.0,
        width: size,
        height: size,
        cells,
        areas: Vec::new(),
    };
    let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
    let none = NavObstacles::default();
    let q = NavQuery {
        mesh: &mesh,
        obstacles: Some(&none),
    };
    let point = Vec3::new(8.5, 0.0, 11.5);
    assert!(
        q.find_nearest(point, Vec3::splat(2.0)).is_none(),
        "2.5 m away, 2 m extent"
    );
    let (poly, snapped) = q
        .find_nearest(point, Vec3::new(4.0, 1.0, 4.0))
        .expect("within a 4 m extent, past the first bucket");
    assert!(
        mesh.polys[poly as usize].contains_xz(snapped.x, snapped.z),
        "the returned point lies on the polygon, got {snapped:?}"
    );
    assert!((snapped.x - 11.0).abs() < 1e-4 && (snapped.z - 11.5).abs() < 1e-4);
}

#[test]
fn carving_boxes_removes_exactly_the_covered_cells() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let size = 16;
    for _ in 0..6 {
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells: vec![Some(0.0); size * size],
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let x0 = rng.range(1.0, 10.0);
        let z0 = rng.range(1.0, 10.0);
        let bmin = Vec3::new(x0, -1.0, z0);
        let bmax = Vec3::new(x0 + rng.range(0.5, 4.0), 1.0, z0 + rng.range(0.5, 4.0));
        let carved = mesh.carved(&[(bmin, bmax)]);
        for z in 0..size {
            for x in 0..size {
                let overlaps = bmax.x > x as f32
                    && bmin.x < x as f32 + 1.0
                    && bmax.z > z as f32
                    && bmin.z < z as f32 + 1.0;
                let on_mesh = carved
                    .polys
                    .iter()
                    .any(|p| p.contains_xz(x as f32 + 0.5, z as f32 + 0.5));
                assert_eq!(on_mesh, !overlaps, "cell ({x}, {z}) vs box {bmin:?}..{bmax:?}");
            }
        }
        // A path between two points that survive the carve routes around the box.
        let none = NavObstacles::default();
        let q = NavQuery {
            mesh: &carved,
            obstacles: Some(&none),
        };
        if let Some(path) = q.find_path(Vec3::new(0.5, 0.0, 0.5), Vec3::new(15.5, 0.0, 15.5), extents()) {
            for pair in path.waypoints.windows(2) {
                for s in 0..=40 {
                    let p = pair[0].lerp(pair[1], s as f32 / 40.0);
                    let inside = p.x > bmin.x + 1e-3
                        && p.x < bmax.x - 1e-3
                        && p.z > bmin.z + 1e-3
                        && p.z < bmax.z - 1e-3;
                    assert!(!inside, "{p:?} is inside the carved box");
                }
            }
        }
    }
}

/// A floor quad plus raised box tops (2 m up, well above the climb limit).
fn floor_with_blocks(rng: &mut Rng) -> (Vec<[Vec3; 3]>, Vec<(Vec3, Vec3)>) {
    let c = 8.0;
    let quad = |x0: f32, z0: f32, x1: f32, z1: f32, y: f32| {
        [
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
        ]
    };
    let mut tris = quad(-c, -c, c, c, 0.0).to_vec();
    let mut blocks = Vec::new();
    for _ in 0..4 {
        let x0 = (rng.range(-6.0, 4.0) * 2.0).round() / 2.0;
        let z0 = (rng.range(-6.0, 4.0) * 2.0).round() / 2.0;
        let x1 = x0 + (rng.range(1.0, 4.0) * 2.0).round() / 2.0;
        let z1 = z0 + (rng.range(1.0, 4.0) * 2.0).round() / 2.0;
        tris.extend(quad(x0, z0, x1, z1, 2.0));
        blocks.push((Vec3::new(x0, 2.0, z0), Vec3::new(x1, 2.0, z1)));
    }
    (tris, blocks)
}

#[test]
fn built_meshes_route_around_raised_blocks() {
    let mut rng = Rng(0xC2B2_AE3D_27D4_EB4F);
    let none = NavObstacles::default();
    let mut checked = 0;
    for _ in 0..4 {
        let (tris, blocks) = floor_with_blocks(&mut rng);
        let mesh = NavMeshBuilder::default().build(&tris).expect("builds");
        // Determinism: a second build of the same soup is identical.
        let again = NavMeshBuilder::default().build(&tris).expect("builds");
        assert_eq!(mesh.polys, again.polys);
        assert_eq!(mesh.region_count, again.region_count);

        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&none),
        };
        for _ in 0..12 {
            let pick = |rng: &mut Rng| loop {
                let p = Vec3::new(rng.range(-7.0, 7.0), 0.0, rng.range(-7.0, 7.0));
                let clear = blocks.iter().all(|(lo, hi)| {
                    p.x < lo.x - 0.8 || p.x > hi.x + 0.8 || p.z < lo.z - 0.8 || p.z > hi.z + 0.8
                });
                if clear {
                    return p;
                }
            };
            let (start, end) = (pick(&mut rng), pick(&mut rng));
            let Some(path) = q.find_path(start, end, Vec3::new(0.5, 0.5, 0.5)) else {
                continue;
            };
            checked += 1;
            // The corridor stays on the floor level, off every block footprint.
            for &poly in &path.polys {
                let p = &mesh.polys[poly as usize];
                assert!(p.height() < 1.0, "a floor path stepped onto a 2 m block");
            }
            assert_waypoints_in_corridor(&mesh, &path.polys, &path.waypoints);
            for pair in path.waypoints.windows(2) {
                for s in 0..=60 {
                    let p = pair[0].lerp(pair[1], s as f32 / 60.0);
                    for (lo, hi) in &blocks {
                        assert!(
                            !(p.x > lo.x + 1e-3
                                && p.x < hi.x - 1e-3
                                && p.z > lo.z + 1e-3
                                && p.z < hi.z - 1e-3),
                            "{p:?} crosses block {lo:?}..{hi:?}"
                        );
                    }
                }
            }
        }
    }
    assert!(checked > 0, "no connected pair was generated");
}
