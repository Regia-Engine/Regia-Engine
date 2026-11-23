// SPDX-License-Identifier: MPL-2.0
//! Independent, bounded polygon-corridor optimum checks (spec 22 T2.2; spec 92 §§3, 5–6).
//!
//! Contract transcribed before implementation from query.rs: the start/end snap to allowed
//! polygons within per-axis extents; no endpoint-to-center charge is added. One polygon costs
//! zero. For a portal edge let d be XZ center distance, u/v the distances from those centers to
//! the portal midpoint, and r/s mesh cost times query multiplier. Equal rates cost d*r;
//! unequal rates cost d*(u*r+v*s)/(u+v), falling back to d*max(r,s) if u+v is zero.
//! NavPath::length measures funnel waypoints, not this objective. No links or obstacles here.
//!
//! Original test design: copy public geometry/adjacency, enumerate all simple paths, and price
//! them in f64 without using the query's search, cost helpers, or center helper. Positive rates
//! mean an optimum is simple. Fixtures have at most 12 polygons and 4096 simple paths per pair.

use regia_core::math::Vec3;
use regia_nav::{
    NavAreaCosts, NavAreaVolume, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacleShape, NavPath,
    NavQuery, NavQueryFilter,
};

const AREAS: [u8; 4] = [0, 1, 2, 63];
const EPS: f64 = 2.0e-5;

fn near(a: f64, b: f64) -> bool {
    a.is_finite() && b.is_finite() && (a - b).abs() <= EPS * a.abs().max(b.abs()).max(1.0)
}

fn quad(x0: f32, x1: f32, z0: f32, z1: f32) -> [[Vec3; 3]; 2] {
    let a = Vec3::new(x0, 0.0, z0);
    let b = Vec3::new(x1, 0.0, z0);
    let c = Vec3::new(x1, 0.0, z1);
    let d = Vec3::new(x0, 0.0, z1);
    [[a, b, c], [a, c, d]]
}

fn fixture(branches: usize) -> NavMesh {
    assert!((2..=3).contains(&branches));
    let height = (branches as f32 - 1.0) * 4.0 + 2.0;
    let mut triangles = Vec::from(quad(0.0, 2.0, 0.0, height));
    triangles.extend(quad(10.0, 12.0, 0.0, height));
    // An island exercises unreachable endpoints without changing the connected component.
    triangles.extend(quad(16.0, 18.0, 0.0, 2.0));
    let mut volumes = Vec::new();
    for &area in &AREAS[1..=branches] {
        let branch = volumes.len();
        let z = branch as f32 * 4.0;
        triangles.extend(quad(2.0, 10.0, z, z + 2.0));
        volumes.push(NavAreaVolume::new(
            NavObstacleShape::Box {
                min: Vec3::new(2.0, -1.0, z),
                max: Vec3::new(10.0, 1.0, z + 2.0),
            },
            area,
        ));
    }
    let mut params = NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    params.agent.radius = 0.0;
    let mesh = NavMeshBuilder::new(params)
        .build_with_areas(&triangles, &volumes)
        .expect("tiny public-builder fixture");
    assert!(mesh.polys.len() <= 12);
    assert!(mesh.off_mesh_links.is_empty());
    for &area in &AREAS[1..=branches] {
        assert_eq!(mesh.polys.iter().filter(|p| p.area == area).count(), 1);
        let branch = mesh
            .polys
            .iter()
            .find(|p| p.area == area)
            .expect("painted branch");
        assert_eq!(branch.portals.len(), 2, "a branch must join both trunks");
    }
    let island = mesh
        .polys
        .iter()
        .find(|p| p.min.x >= 16.0)
        .expect("the disconnected component must exist");
    assert!(island.portals.is_empty());
    mesh
}

#[derive(Clone)]
struct Edge {
    to: usize,
    midpoint: [f64; 2],
}

struct Graph {
    centers: Vec<[f64; 2]>,
    areas: Vec<u8>,
    edges: Vec<Vec<Edge>>,
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

impl Graph {
    fn copy(mesh: &NavMesh) -> Self {
        let centers = mesh
            .polys
            .iter()
            .map(|p| {
                let n = p.vertices.len() as f64;
                [
                    p.vertices.iter().map(|v| f64::from(v.x)).sum::<f64>() / n,
                    p.vertices.iter().map(|v| f64::from(v.z)).sum::<f64>() / n,
                ]
            })
            .collect();
        let edges: Vec<Vec<Edge>> = mesh
            .polys
            .iter()
            .map(|p| {
                p.portals
                    .iter()
                    .map(|e| Edge {
                        to: e.neighbor as usize,
                        midpoint: [
                            (f64::from(e.a.x) + f64::from(e.b.x)) / 2.0,
                            (f64::from(e.a.z) + f64::from(e.b.z)) / 2.0,
                        ],
                    })
                    .collect()
            })
            .collect();
        for (a, neighbors) in edges.iter().enumerate() {
            for edge in neighbors {
                assert!(edges[edge.to].iter().any(|back| back.to == a));
            }
        }
        Self {
            centers,
            areas: mesh.polys.iter().map(|p| p.area).collect(),
            edges,
        }
    }

    fn point(&self, poly: usize) -> Vec3 {
        let [x, z] = self.centers[poly];
        Vec3::new(x as f32, 0.0, z as f32)
    }

    fn paths(&self, start: usize, end: usize) -> Vec<Vec<usize>> {
        fn visit(
            g: &Graph,
            end: usize,
            path: &mut Vec<usize>,
            found: &mut Vec<Vec<usize>>,
            visits: &mut usize,
        ) {
            *visits += 1;
            assert!(
                *visits <= 16_384,
                "keep traversal work bounded, including unreachable goals"
            );
            if *path.last().expect("nonempty traversal") == end {
                found.push(path.clone());
                assert!(found.len() <= 4096, "keep enumeration bounded");
                return;
            }
            for edge in &g.edges[*path.last().expect("nonempty traversal")] {
                if !path.contains(&edge.to) {
                    path.push(edge.to);
                    visit(g, end, path, found, visits);
                    path.pop();
                }
            }
        }
        let mut found = Vec::new();
        visit(self, end, &mut vec![start], &mut found, &mut 0);
        found
    }

    fn legal(&self, path: &[usize], mask: u64) -> bool {
        path.iter().all(|&p| mask & (1u64 << self.areas[p]) == 0)
    }

    fn cost(&self, path: &[usize], rates: &[f64; 64]) -> f64 {
        path.windows(2)
            .map(|pair| {
                let (a, b) = (pair[0], pair[1]);
                let portal = self.edges[a].iter().find(|e| e.to == b).expect("adjacent pair");
                let d = distance(self.centers[a], self.centers[b]);
                let r = rates[self.areas[a] as usize];
                let s = rates[self.areas[b] as usize];
                if r == s {
                    return d * r;
                }
                let u = distance(self.centers[a], portal.midpoint);
                let v = distance(self.centers[b], portal.midpoint);
                let rate = if u + v > 0.0 {
                    (u * r + v * s) / (u + v)
                } else {
                    r.max(s)
                };
                d * rate
            })
            .sum()
    }

    fn optima(&self, paths: &[Vec<usize>], rates: &[f64; 64], mask: u64) -> Vec<Vec<usize>> {
        let best = paths
            .iter()
            .filter(|p| self.legal(p, mask))
            .map(|p| self.cost(p, rates))
            .fold(f64::INFINITY, f64::min);
        paths
            .iter()
            .filter(|p| self.legal(p, mask) && near(self.cost(p, rates), best))
            .cloned()
            .collect()
    }
}

fn tables(
    mesh_rates: [f32; 4],
    filter_rates: [f32; 4],
    scale: f32,
) -> (NavAreaCosts, NavQueryFilter, [f64; 64]) {
    let mut table = NavAreaCosts::default();
    let mut filter = NavQueryFilter::default();
    let mut rates = [0.0; 64];
    // Scale ALL ids, including default/unused ones, so the heuristic's minimum scales too.
    for (area, rate) in rates.iter_mut().enumerate() {
        let index = AREAS.iter().position(|&a| usize::from(a) == area);
        let m = index.map_or(1.0, |i| mesh_rates[i]);
        let f = index.map_or(1.0, |i| filter_rates[i]) * scale;
        table.set(area as u8, m).expect("valid mesh rate");
        filter.area_costs.set(area as u8, f).expect("valid multiplier");
        *rate = f64::from(m) * f64::from(f);
    }
    (table, filter, rates)
}

fn verify(g: &Graph, result: Option<NavPath>, optima: &[Vec<usize>], rates: &[f64; 64], mask: u64) {
    if optima.is_empty() {
        assert!(result.is_none(), "refusal must return no previous corridor");
        return;
    }
    let path: Vec<usize> = result
        .expect("reachable allowed endpoints")
        .polys
        .into_iter()
        .map(|p| p as usize)
        .collect();
    assert!(g.legal(&path, mask), "excluded polygon in {path:?}");
    assert_eq!(path.first(), optima[0].first());
    assert_eq!(path.last(), optima[0].last());
    // cost() independently proves public adjacency for every returned consecutive pair.
    let actual = g.cost(&path, rates);
    let best = g.cost(&optima[0], rates);
    assert!(
        near(actual, best),
        "suboptimal {path:?}: {actual}, optimum {optima:?}: {best}, mask={mask:#x}, rates={rates:?}"
    );
    assert!(optima.contains(&path), "accept any valid optimum, including ties");
}

#[test]
fn bounded_corridor_sweep_matches_all_simple_path_optima_and_scales_the_optimal_set() {
    // Extends areas.rs single examples, rather than repeating their crossing/length assertions.
    let values = [0.125, 1.0, 4.0];
    let mut queries = 0;
    let mut ties = 0;
    let mut refusals = 0;
    for branches in [2, 3] {
        let mut mesh = fixture(branches);
        let g = Graph::copy(&mesh);
        let left = g.centers.iter().position(|c| c[0] < 2.0).expect("left trunk");
        let right = g
            .centers
            .iter()
            .position(|c| (10.0..12.0).contains(&c[0]))
            .expect("right trunk");
        assert_eq!(
            g.paths(left, right).len(),
            branches,
            "the fixture must offer exactly the stated competing corridors"
        );
        eprintln!("fixture: {branches} corridors, {} polygons", g.areas.len());
        let pairs: Vec<_> = (0..g.areas.len())
            .flat_map(|a| (0..g.areas.len()).map(move |b| (a, b)))
            .collect();
        for (start, end) in pairs {
            let paths = g.paths(start, end);
            for &a in &values {
                for &b in &values {
                    for &c in &values {
                        let mut unscaled = Vec::new();
                        for scale in [1.0, 0.25, 8.0] {
                            // Nontrivial products distinguish the mesh table from the filter table.
                            let (table, mut filter, rates) =
                                tables([2.0, 0.5, 4.0, 1.0], [0.5, a, b, c], scale);
                            mesh.set_area_costs(table);
                            // Every subset of the fixture's present area ids, including bit 63.
                            for subset in 0..(1usize << (branches + 1)) {
                                let mask = AREAS[..=branches]
                                    .iter()
                                    .enumerate()
                                    .filter(|(i, _)| subset & (1 << i) != 0)
                                    .fold(0, |m, (_, &area)| m | (1u64 << area));
                                filter.excluded_areas = mask;
                                let optima = g.optima(&paths, &rates, mask);
                                if scale == 1.0 {
                                    unscaled.push(optima.clone());
                                } else {
                                    assert_eq!(
                                        optima, unscaled[subset],
                                        "uniform scaling must preserve the whole optimal set"
                                    );
                                }
                                ties += usize::from(optima.len() > 1);
                                refusals += usize::from(optima.is_empty());
                                let result = NavQuery {
                                    mesh: &mesh,
                                    obstacles: None,
                                }
                                .find_path_filtered(
                                    g.point(start),
                                    g.point(end),
                                    Vec3::ZERO,
                                    &filter,
                                );
                                verify(&g, result, &optima, &rates, mask);
                                queries += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(queries > 10_000);
    assert!(ties > 0, "the sweep must actually exercise ties");
    assert!(refusals > 0);
    eprintln!("oracle sweep: {queries} queries, {ties} tied optima, {refusals} refusals");
}

#[test]
fn reference_prices_portal_fractions_and_equal_rates_as_documented() {
    // Unequal widths: one meter in area 0, three in area 1, centers four meters apart.
    let g = Graph {
        centers: vec![[0.0, 0.0], [4.0, 0.0]],
        areas: vec![0, 1],
        edges: vec![
            vec![Edge {
                to: 1,
                midpoint: [1.0, 0.0],
            }],
            vec![Edge {
                to: 0,
                midpoint: [1.0, 0.0],
            }],
        ],
    };
    let mut rates = [2.0; 64];
    rates[1] = 6.0;
    assert_eq!(g.cost(&[0, 1], &rates), 20.0);
    assert_eq!(g.cost(&[1, 0], &rates), 20.0);
    rates[1] = 2.0;
    assert_eq!(g.cost(&[0, 1], &rates), 8.0);
    assert_eq!(g.cost(&[0], &rates), 0.0);
    // An off-axis portal does not charge the sum of the two legs.
    let mut offset = g;
    offset.edges[0][0].midpoint = [1.0, 3.0];
    assert_eq!(offset.cost(&[0, 1], &rates), 8.0);
    rates[1] = 6.0;
    let expected = 4.0 * (10.0f64.sqrt() * 2.0 + 18.0f64.sqrt() * 6.0) / (10.0f64.sqrt() + 18.0f64.sqrt());
    assert!(near(offset.cost(&[0, 1], &rates), expected));
}

#[test]
fn symmetric_corridors_have_two_optima_independent_of_positions_within_endpoint_polygons() {
    let mut mesh = fixture(2);
    let g = Graph::copy(&mesh);
    for scale in [0.125, 1.0, 8.0] {
        let (table, filter, rates) = tables([1.0; 4], [1.0; 4], scale);
        mesh.set_area_costs(table);
        let q = NavQuery {
            mesh: &mesh,
            obstacles: None,
        };
        for (from, to) in [
            (Vec3::new(1.0, 0.0, 3.0), Vec3::new(11.0, 0.0, 3.0)),
            (Vec3::new(0.75, 0.0, 0.75), Vec3::new(11.25, 0.0, 5.25)),
        ] {
            let start = q.find_nearest(from, Vec3::ZERO).expect("left trunk").0 as usize;
            let end = q.find_nearest(to, Vec3::ZERO).expect("right trunk").0 as usize;
            let optima = g.optima(&g.paths(start, end), &rates, 0);
            assert_eq!(optima.len(), 2, "symmetric branches must give a real tie");
            verify(
                &g,
                q.find_path_filtered(from, to, Vec3::ZERO, &filter),
                &optima,
                &rates,
                0,
            );
        }
    }
}

#[test]
fn snapping_and_refusal_transitions_do_not_reuse_a_previous_corridor() {
    // Extends areas.rs::an_end_in_a_left_out_area_snaps_off_it with reuse, both endpoints,
    // tight extents, non-finite/negative inputs, recovery and optimum verification after snap.
    let mut mesh = fixture(3);
    let g = Graph::copy(&mesh);
    let (table, open, rates) = tables([1.0; 4], [1.0, 0.125, 4.0, 2.0], 1.0);
    mesh.set_area_costs(table);
    let closed = open.without_area(1).expect("area 1");
    let q = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let left = Vec3::new(1.0, 0.0, 1.0);
    let right = Vec3::new(11.0, 0.0, 1.0);
    let excluded = Vec3::new(2.25, 0.0, 1.0);
    let start = q.find_nearest(left, Vec3::ZERO).expect("left trunk").0 as usize;
    let end = q.find_nearest(right, Vec3::ZERO).expect("right trunk").0 as usize;
    let paths = g.paths(start, end);
    let expected = g.optima(&paths, &rates, 0);
    verify(
        &g,
        q.find_path_filtered(left, right, Vec3::ZERO, &open),
        &expected,
        &rates,
        0,
    );
    let reach = Vec3::new(0.5, 0.0, 0.0);
    assert_eq!(
        q.find_nearest_filtered(excluded, reach, &closed),
        Some((start as u32, Vec3::new(2.0, 0.0, 1.0)))
    );
    let snapped = g.optima(&paths, &rates, 1 << 1);
    verify(
        &g,
        q.find_path_filtered(excluded, right, reach, &closed),
        &snapped,
        &rates,
        1 << 1,
    );
    let reverse = g.paths(end, start);
    verify(
        &g,
        q.find_path_filtered(right, excluded, reach, &closed),
        &g.optima(&reverse, &rates, 1 << 1),
        &rates,
        1 << 1,
    );
    // Different positions within one polygon change waypoints, never the graph cost.
    let same = q
        .find_path_filtered(left, Vec3::new(1.5, 0.0, 1.25), Vec3::ZERO, &open)
        .expect("same polygon");
    assert_eq!(same.polys, vec![start as u32]);
    assert_eq!(same.waypoints, vec![left, Vec3::new(1.5, 0.0, 1.25)]);
    assert!(same.length > 0.0);
    assert_eq!(g.cost(&[start], &rates), 0.0);
    for (from, to, extents, filter) in [
        (left, excluded, Vec3::ZERO, &closed),
        (excluded, right, Vec3::ZERO, &closed),
        (left, Vec3::new(17.0, 0.0, 1.0), Vec3::ZERO, &open),
        (left, right, Vec3::splat(-0.1), &open),
        (Vec3::splat(f32::NAN), right, reach, &open),
        (left, Vec3::splat(f32::INFINITY), reach, &open),
        (left, right, Vec3::splat(f32::INFINITY), &open),
    ] {
        assert!(
            q.find_path_filtered(from, to, extents, filter).is_none(),
            "refusal must not retain the preceding successful result"
        );
        verify(
            &g,
            q.find_path_filtered(left, right, Vec3::ZERO, &open),
            &expected,
            &rates,
            0,
        );
    }
}
