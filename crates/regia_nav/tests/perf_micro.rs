// SPDX-License-Identifier: MPL-2.0
//! Hand-run timing of the crowd step and tile-cache rebuilds (`docs/06-testing-and-ci.md` § 9),
//! plus one counting test that runs in the gate.
//! The timing tests are `#[ignore]`: run them in release on a quiet machine with
//! `cargo test -p regia_nav --release --test perf_micro -- --ignored --nocapture`.
//! They print `perf-micro <name>: n= p50= p99= max=` and assert only that the work happened.

#![allow(unsafe_code)] // A `GlobalAlloc` impl is unsafe by definition (ADR-2704).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Allocations (including reallocations) since the counter was last stored.
static ALLOC_OPS: AtomicU64 = AtomicU64::new(0);

struct CountAlloc;

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_OPS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_OPS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountAlloc = CountAlloc;

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, NavAreaCosts, NavAreaVolume, NavCrowd, NavCrowdConfig, NavLaneAgentParams,
    NavLaneCrowd, NavLaneCrowdConfig, NavLaneDef, NavLaneGraph, NavMeshBuildParams, NavObstacleShape,
    NavQuery, NavTileCache, NavTileCacheParams,
};

fn floor(half: f32) -> Vec<[Vec3; 3]> {
    vec![
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, -half),
            Vec3::new(half, 0.0, half),
        ],
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, half),
            Vec3::new(-half, 0.0, half),
        ],
    ]
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_crowd_step() {
    for (n, half) in [(50usize, 12.0f32), (500, 40.0)] {
        let cache = NavTileCache::build(
            NavTileCacheParams {
                build: NavMeshBuildParams {
                    cell_size: 0.25,
                    ..Default::default()
                },
                tile_size: 32,
            },
            &floor(half),
        )
        .expect("builds");
        let mesh = cache.mesh();
        let mut crowd = NavCrowd::new(NavCrowdConfig {
            max_agents: n,
            ..Default::default()
        })
        .expect("config");
        let ring = half * 0.8;
        let params = CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0);
        let mut ids = Vec::new();
        for k in 0..n {
            // Concentric rings so 500 agents fit; every agent heads for the opposite point.
            let rings = n.div_ceil(100);
            let r = ring * (1.0 - (k % rings) as f32 / rings as f32 * 0.5);
            let a = k as f32 / n as f32 * std::f32::consts::TAU;
            ids.push(
                crowd
                    .add_agent(mesh, Vec3::new(a.cos() * r, 0.0, a.sin() * r), params)
                    .expect("adds"),
            );
        }
        for (k, &id) in ids.iter().enumerate() {
            let p = crowd.agents()[k].position();
            crowd
                .set_target(mesh, id, Vec3::new(-p.x, 0.0, -p.z))
                .expect("target");
        }
        let mut samples = Vec::new();
        for _ in 0..600 {
            let t = Instant::now();
            crowd.step(mesh);
            samples.push(t.elapsed());
        }
        assert_eq!(crowd.stats().steps, 600);
        assert!(crowd.stats().velocity_samples > 0);
        report(&format!("crowd_step_{n}_agents"), samples);
    }
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_tile_rebuild() {
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 32,
        },
        &floor(64.0),
    )
    .expect("builds");
    let mut samples = Vec::new();
    let mut rebuilt = 0;
    for k in 0..200 {
        let x = (k % 20) as f32 * 5.0 - 50.0;
        let z = (k / 20) as f32 * 9.0 - 45.0;
        let t = Instant::now();
        let id = cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(x, 0.0, z),
                radius: 0.8,
                height: 2.0,
            })
            .expect("adds");
        rebuilt += cache.update().rebuilt.len();
        cache.remove_obstacle(id).expect("removes");
        rebuilt += cache.update().rebuilt.len();
        samples.push(t.elapsed());
    }
    assert!(rebuilt >= 400, "{rebuilt}");
    println!(
        "perf-micro tile_cache: {} tiles, {} polys, {rebuilt} tile rebuilds",
        cache.tile_count(),
        cache.mesh().poly_count()
    );
    report("tile_cache_add_update_remove_update", samples);
}

/// A 64 x 64 m two-level test map (ADR-1821 budget): a ground floor and a 2 m high block of
/// 8 x 8 m platforms on a 16 m pitch (ledges on every side).
fn two_levels() -> Vec<[Vec3; 3]> {
    let quad = |x0: f32, z0: f32, x1: f32, z1: f32, y: f32| {
        [
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
        ]
    };
    let mut t: Vec<[Vec3; 3]> = quad(-32.0, -32.0, 32.0, 32.0, 0.0).into();
    for gz in 0..4 {
        for gx in 0..4 {
            let (x0, z0) = (-28.0 + gx as f32 * 16.0, -28.0 + gz as f32 * 16.0);
            t.extend(quad(x0, z0, x0 + 8.0, z0 + 8.0, 2.0));
        }
    }
    t
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_generate_links() {
    let tris = two_levels();
    let mesh = regia_nav::NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("builds");
    let params = regia_nav::NavLinkGenParams::default();
    let mut samples = Vec::new();
    let mut count = 0;
    for _ in 0..20 {
        let t = Instant::now();
        count = regia_nav::generate_links(&mesh, &params, |_| true)
            .expect("valid")
            .len();
        samples.push(t.elapsed());
    }
    let mut clear = regia_nav::TriangleClearance::for_mesh(&tris, &mesh);
    let t = Instant::now();
    let kept = regia_nav::generate_links(&mesh, &params, |c| clear.is_clear(c))
        .expect("valid")
        .len();
    let with_clearance = t.elapsed();
    assert!(count > 0 && kept > 0, "{count} {kept}");
    println!(
        "perf-micro generate_links: {} polys, {count} links ({kept} with clearance in {with_clearance:?}, {} tests)",
        mesh.poly_count(),
        clear.tests()
    );
    report("generate_links_64m_two_levels", samples);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_crowd_step_with_links() {
    let tris = two_levels();
    let mesh = regia_nav::NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("builds");
    let mut links =
        regia_nav::generate_links(&mesh, &regia_nav::NavLinkGenParams::default(), |_| true).expect("valid");
    links.truncate(64);
    let link_count = links.len();
    let mesh = mesh.with_off_mesh_links(links);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let params = CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0);
    for k in 0..100 {
        // Agents start on the platforms and walk to the far ground corner, so they drop.
        let (gx, gz) = (k % 4, (k / 4) % 4);
        let o = (k / 16) as f32 * 0.9;
        let p = Vec3::new(-27.0 + gx as f32 * 16.0 + o, 2.0, -27.0 + gz as f32 * 16.0 + 1.0);
        let Ok(id) = crowd.add_agent(&mesh, p, params) else {
            continue;
        };
        crowd
            .set_target(&mesh, id, Vec3::new(30.0, 0.0, 30.0))
            .expect("target");
    }
    let mut samples = Vec::new();
    for _ in 0..600 {
        let t = Instant::now();
        crowd.step(&mesh);
        samples.push(t.elapsed());
    }
    println!(
        "perf-micro crowd_with_links: {} agents, {link_count} links, {} crossings",
        crowd.len(),
        crowd.stats().link_traversals
    );
    assert!(crowd.stats().link_traversals > 0);
    report("crowd_step_100_agents_64_links", samples);
}

/// A grid of 8 x 8 blocks of lanes: 112 one-way streets, each joined to the next by distance.
fn lane_grid() -> NavLaneGraph {
    let mut defs = Vec::new();
    let block = 50.0;
    for i in 0..8 {
        for j in 0..7 {
            let (a, b) = (j as f32 * block, (j + 1) as f32 * block);
            let line = i as f32 * block;
            let mut along = NavLaneDef::new(&[Vec3::new(a, 0.0, line), Vec3::new(b, 0.0, line)]);
            let mut across = NavLaneDef::new(&[Vec3::new(line, 0.0, a), Vec3::new(line, 0.0, b)]);
            along.width = 4.0;
            across.width = 4.0;
            defs.push(along);
            defs.push(across);
        }
    }
    NavLaneGraph::build(&defs, 0.5).expect("valid lanes")
}

fn lane_crowd_step(agents: u64, name: &str) {
    let graph = lane_grid();
    let config = NavLaneCrowdConfig {
        max_agents: agents as usize,
        ..Default::default()
    };
    let mut crowd = NavLaneCrowd::new(config).expect("config");
    let lanes = graph.len() as u64;
    for i in 0..agents {
        let lane = (i % lanes) as u32;
        let params = NavLaneAgentParams {
            speed: 1.0 + (i % 5) as f32 * 0.15,
            ..Default::default()
        };
        let distance = (i / lanes) as f32 * 49.0 / (agents / lanes + 1) as f32;
        let offset = (i % 7) as f32 * 0.4 - 1.2;
        crowd
            .add_agent(&graph, lane, distance, offset, params, i)
            .expect("valid agent");
    }
    let mut samples = Vec::new();
    for _ in 0..300 {
        let t = Instant::now();
        crowd.step(&graph);
        samples.push(t.elapsed());
    }
    let s = crowd.stats();
    println!(
        "perf-micro {name}: {agents} agents, {} transitions, {} slowed",
        s.transitions, s.slowed
    );
    assert!(s.steps > 0);
    report(name, samples);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_lane_crowd_step() {
    lane_crowd_step(1_000, "lane_crowd_step_1k");
    lane_crowd_step(10_000, "lane_crowd_step_10k");
    lane_crowd_step(100_000, "lane_crowd_step_100k");
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_find_path_with_area_costs() {
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 32,
        },
        &floor(64.0),
    )
    .expect("builds");
    for i in 0..16 {
        let (x, z) = ((i % 4) as f32 * 28.0 - 42.0, (i / 4) as f32 * 28.0 - 42.0);
        let shape = NavObstacleShape::Box {
            min: Vec3::new(x, -1.0, z),
            max: Vec3::new(x + 20.0, 1.0, z + 20.0),
        };
        cache
            .add_area_volume(NavAreaVolume::new(shape, 1 + i % 3))
            .expect("valid volume");
    }
    cache.update();
    let mut costs = NavAreaCosts::default();
    costs.set(1, 3.0).expect("valid cost");
    costs.set(2, 8.0).expect("valid cost");
    costs.set(3, 0.5).expect("valid cost");
    cache.set_area_costs(costs);
    let query: NavQuery<'_> = cache.query();
    let mut samples = Vec::new();
    for i in 0..600 {
        let t = Instant::now();
        let from = Vec3::new(-60.0, 0.0, -60.0 + (i % 100) as f32);
        let to = Vec3::new(60.0, 0.0, 60.0 - (i % 100) as f32);
        let path = query.find_path(from, to, Vec3::splat(2.0));
        samples.push(t.elapsed());
        assert!(path.is_some());
    }
    println!("perf-micro area_path: {} polygons", cache.mesh().poly_count());
    report("find_path_area_costs", samples);
}

/// A floor of 1 m quads, leaving out those within `hole` metres of `center` on both axes.
fn quad_floor(half: i32, center: (f32, f32), hole: f32) -> Vec<[Vec3; 3]> {
    let mut out = Vec::new();
    for z in -half..half {
        for x in -half..half {
            let (fx, fz) = (x as f32, z as f32);
            if (fx + 0.5 - center.0).abs() < hole && (fz + 0.5 - center.1).abs() < hole {
                continue;
            }
            let (a, b) = (Vec3::new(fx, 0.0, fz), Vec3::new(fx + 1.0, 0.0, fz));
            let (c, d) = (Vec3::new(fx + 1.0, 0.0, fz + 1.0), Vec3::new(fx, 0.0, fz + 1.0));
            out.push([a, b, c]);
            out.push([a, c, d]);
        }
    }
    out
}

/// One blast: the geometry under a 6 m box goes and comes back, each edit re-voxelized and its
/// tiles polygonised on a 128 m map (17 x 17 tiles, ADR-1934).
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_region_rebuild() {
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    };
    let mut cache = NavTileCache::build(params, &quad_floor(64, (0.0, 0.0), 0.0)).expect("builds");
    let mut samples = Vec::new();
    let mut changed = 0;
    for k in 0..200 {
        let center = ((k % 20) as f32 * 5.0 - 50.0, (k / 20) as f32 * 9.0 - 45.0);
        let lo = Vec3::new(center.0 - 3.0, 0.0, center.1 - 3.0);
        let hi = Vec3::new(center.0 + 3.0, 2.0, center.1 + 3.0);
        for hole in [3.0, 0.0] {
            let tris = quad_floor(64, center, hole);
            let (w_lo, w_hi) = cache.region_window(lo, hi).expect("on the grid");
            let window: Vec<_> = tris
                .iter()
                .filter(|t| {
                    t.iter()
                        .any(|v| v.x >= w_lo.x && v.x <= w_hi.x && v.z >= w_lo.z && v.z <= w_hi.z)
                })
                .copied()
                .collect();
            let t = Instant::now();
            changed += cache
                .rebuild_region(lo, hi, &window)
                .expect("rebuilds")
                .cells_changed;
            cache.update();
            samples.push(t.elapsed());
        }
    }
    report("region_rebuild", samples);
    assert!(changed > 0);
}

/// One cell's blob loaded and its tiles polygonised, then unloaded again, for one tile and for a
/// 3 x 3 block of tiles (ADR-1934).
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_nav_cell_load() {
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    };
    let whole = NavTileCache::build(params.clone(), &floor(64.0)).expect("builds");
    for (name, side) in [("nav_cell_load_1_tile", 1u32), ("nav_cell_load_9_tiles", 3)] {
        let tiles: Vec<(u32, u32)> = (0..side)
            .flat_map(|z| (0..side).map(move |x| (x + 2, z + 2)))
            .collect();
        let blob = whole.cook_cell(&tiles).expect("cooks");
        let mut cache = NavTileCache::streamed(params.clone(), whole.layout()).expect("a streamed cache");
        let (mut load, mut unload) = (Vec::new(), Vec::new());
        for _ in 0..100 {
            let t = Instant::now();
            cache.load_cell(1, &blob).expect("loads");
            cache.update();
            load.push(t.elapsed());
            assert!(cache.cell_ready(1));
            let t = Instant::now();
            cache.unload_cell(1).expect("unloads");
            cache.update();
            unload.push(t.elapsed());
        }
        println!("perf-micro {name}: {} bytes", blob.len());
        report(name, load);
        report(&format!("{name}_unload"), unload);
    }
}

/// audit sprint-audit-g-nav-perf M4: the second step of a clustered crowd allocates for the
/// proximity grids, not a fresh vector per agent. The first step is a warmup so vector growth
/// is not counted. No wall-clock bound.
#[test]
fn crowd_step_allocations_stay_with_the_grids() {
    let n = 64usize;
    let cache = NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 32,
        },
        &floor(12.0),
    )
    .expect("floor");
    let mesh = cache.mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        max_agents: n,
        ..Default::default()
    })
    .expect("config");
    let params = CrowdAgentParams::new(0.3, 1.6, 1.5, 8.0);
    let mut ids = Vec::new();
    for k in 0..n {
        // A tight block: one proximity bucket, and nobody arrives during the measured step.
        let x = (k % 8) as f32 * 0.05;
        let z = (k / 8) as f32 * 0.05;
        ids.push(
            crowd
                .add_agent(mesh, Vec3::new(x, 0.0, z), params)
                .expect("on the floor"),
        );
    }
    for &id in &ids {
        let p = crowd.agent(id).expect("agent").position();
        crowd
            .set_target(mesh, id, Vec3::new(p.x + 8.0, 0.0, p.z))
            .expect("target");
    }
    crowd.step(mesh);
    // The counter must notice a real allocation. The measured step itself may allocate nothing
    // once the grids' flat buffers have grown during the warmup.
    let probe = vec![1u8];
    assert!(
        ALLOC_OPS.load(Ordering::Relaxed) > 0,
        "the counter recorded an allocation"
    );
    drop(probe);
    let replans_before = crowd.stats().replans;
    ALLOC_OPS.store(0, Ordering::Relaxed);
    crowd.step(mesh);
    let allocs = ALLOC_OPS.load(Ordering::Relaxed);
    let replans = crowd.stats().replans - replans_before;
    // the build server, rustc 1.99.0, 2026-11-10: 5 allocations and 0 replans. The same step
    // allocated 2376 times before the scratch, wall and flat-grid reuse.
    assert!(
        allocs < 48,
        "second step allocated {allocs} times for {n} clustered agents ({replans} replans); the grids may allocate, a vector per agent may not"
    );
}
