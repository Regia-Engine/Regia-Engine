// SPDX-License-Identifier: MPL-2.0
//! Boundary values, non-finite inputs and determinism of the pure-math modules (floating origin,
//! streaming grid, HLOD tree, streaming planner) that the module tests leave out. Everything here
//! is deterministic: fixed inputs, no clocks, no GPU.

use regia_scene::floating_origin::{
    FloatingOrigin, LargePosition, LargeSpace, MAX_ORIGIN_CELL_SIZE, OriginError,
};
use regia_scene::hlod::{CellProxy, HlodConfig, HlodDrawKind, HlodError, HlodTree, MAX_HLOD_LEVELS};
use regia_scene::stream_grid::{CellCoord, MAX_CELL_COORD, StreamError, StreamGrid};
use regia_scene::stream_plan::{CellInfo, StreamConfig, StreamPlanner};

fn space() -> LargeSpace {
    LargeSpace::new(1024.0).expect("space")
}

// ---------------------------------------------------------------- floating origin

/// The cell size bound is inclusive at the top and exclusive at zero.
#[test]
fn large_space_cell_size_bounds() {
    assert!(LargeSpace::new(MAX_ORIGIN_CELL_SIZE).is_ok());
    assert!(matches!(
        LargeSpace::new(MAX_ORIGIN_CELL_SIZE * 1.0000001),
        Err(OriginError::BadCellSize(_))
    ));
    assert!(matches!(LargeSpace::new(0.0), Err(OriginError::BadCellSize(_))));
    assert!(matches!(LargeSpace::new(-0.0), Err(OriginError::BadCellSize(_))));
    assert!(LargeSpace::new(f64::MIN_POSITIVE).is_ok());
}

/// The offset range is `[-half, half)`: exactly `+half` carries into the next cell as `-half`,
/// exactly `-half` stays.
#[test]
fn the_half_open_cell_range_is_respected_at_both_edges() {
    let s = space();
    let up = s.from_world([512.0, 0.0, -512.0]).expect("edge");
    assert_eq!(up.cell, [1, 0, 0], "+half belongs to the next cell");
    assert_eq!(up.local, [-512.0, 0.0, -512.0]);
    let below = s.from_world([511.999, -512.001, 0.0]).expect("edge");
    assert_eq!(below.cell, [0, -1, 0]);
    // The same through carry.
    let base = LargePosition {
        cell: [5, 5, 5],
        local: [0.0; 3],
    };
    let moved = s.offset(&base, [512.0, -512.0, 511.0]).expect("offset");
    assert_eq!(moved.cell, [6, 5, 5]);
    assert_eq!(moved.local, [-512.0, -512.0, 511.0]);
}

/// A non-finite delta or a non-finite stored offset is an error, not a silently wrong cell.
#[test]
fn offset_refuses_non_finite_input() {
    let s = space();
    let p = LargePosition {
        cell: [0; 3],
        local: [0.0; 3],
    };
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            s.offset(&p, [bad, 0.0, 0.0]),
            Err(OriginError::NonFinite("offset"))
        );
        assert_eq!(
            s.offset(&p, [0.0, 0.0, bad]),
            Err(OriginError::NonFinite("offset"))
        );
        let broken = LargePosition {
            cell: [0; 3],
            local: [0.0, bad, 0.0],
        };
        assert_eq!(s.normalize(&broken), Err(OriginError::NonFinite("offset")));
    }
    for bad in [f64::NAN, f64::INFINITY] {
        assert_eq!(s.from_world([0.0, bad, 0.0]), Err(OriginError::NonFinite("y")));
    }
}

/// Cell arithmetic at the edge of the addressable range never wraps or panics.
#[test]
fn cell_arithmetic_at_the_extremes_is_an_error_not_a_wrap() {
    let s = space();
    let edge = LargePosition {
        cell: [MAX_CELL_COORD, -MAX_CELL_COORD, 0],
        local: [0.0; 3],
    };
    // One cell further is out of range on either side.
    assert_eq!(s.offset(&edge, [1024.0, 0.0, 0.0]), Err(OriginError::OutOfRange));
    assert_eq!(s.offset(&edge, [0.0, -1024.0, 0.0]), Err(OriginError::OutOfRange));
    // Moving back inside is fine.
    assert!(s.offset(&edge, [-1024.0, 1024.0, 0.0]).is_ok());
    // A stored cell outside i64 arithmetic: checked_add refuses.
    let wild = LargePosition {
        cell: [i64::MAX, i64::MIN, 0],
        local: [0.0; 3],
    };
    assert_eq!(s.offset(&wild, [2048.0, 0.0, 0.0]), Err(OriginError::OutOfRange));
    assert_eq!(s.offset(&wild, [0.0, -2048.0, 0.0]), Err(OriginError::OutOfRange));
    // `relative` uses i128 and survives the full i64 span without overflow.
    let r = s.relative(
        &wild,
        &LargePosition {
            cell: [i64::MIN, i64::MAX, 0],
            local: [0.0; 3],
        },
    );
    assert!(!r[0].is_nan());
    assert!(!r[2].is_nan());
    // Out-of-range world points.
    assert_eq!(s.from_world([1.0e300, 0.0, 0.0]), Err(OriginError::OutOfRange));
    assert_eq!(s.from_world([0.0, 0.0, -1.0e300]), Err(OriginError::OutOfRange));
}

/// `FloatingOrigin::new` accepts exactly half a cell as the threshold and refuses less, NaN, inf
/// and a cell beyond the addressable range.
#[test]
fn floating_origin_constructor_boundaries() {
    let s = space();
    assert!(FloatingOrigin::new(s, [0; 3], 512.0).is_ok());
    assert!(matches!(
        FloatingOrigin::new(s, [0; 3], 511.99),
        Err(OriginError::BadThreshold { .. })
    ));
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(matches!(
            FloatingOrigin::new(s, [0; 3], bad),
            Err(OriginError::BadThreshold { .. })
        ));
    }
    assert!(FloatingOrigin::new(s, [MAX_CELL_COORD, -MAX_CELL_COORD, 0], 512.0).is_ok());
    assert_eq!(
        FloatingOrigin::new(s, [MAX_CELL_COORD + 1, 0, 0], 512.0),
        Err(OriginError::OutOfRange)
    );
    assert_eq!(
        FloatingOrigin::new(s, [0, i64::MIN, 0], 512.0),
        Err(OriginError::OutOfRange)
    );
}

/// A focus exactly at the threshold does not rebase; just past it does, and the rebase plus
/// `Rebase::apply` keeps every world position.
#[test]
fn rebase_triggers_strictly_past_the_threshold_and_preserves_world_positions() {
    let s = space();
    let mut origin = FloatingOrigin::new(s, [0; 3], 600.0).expect("origin");
    let at = origin.from_local([600.0, 0.0, -600.0]).expect("at");
    assert_eq!(origin.update(&at), Ok(None), "exactly at the threshold");
    let past = origin.from_local([600.5, 0.0, 0.0]).expect("past");
    let world_before = s.to_world(&past);
    let mut local = origin.to_local(&past);
    let rebase = origin
        .update(&past)
        .expect("update")
        .expect("past the threshold rebases");
    assert_eq!(rebase.cell_delta, [1, 0, 0]);
    assert_eq!(rebase.shift, [1024.0, 0.0, 0.0]);
    rebase.apply(&mut local);
    assert_eq!(
        origin.to_local(&past),
        local,
        "apply agrees with a fresh to_local"
    );
    assert_eq!(s.to_world(&past), world_before, "the large position is untouched");
    assert_eq!(origin.cell(), [1, 0, 0]);
    // Settled: the same focus does nothing now.
    assert_eq!(origin.update(&past), Ok(None));
}

/// A failed update leaves the origin where it was.
#[test]
fn a_failed_update_does_not_move_the_origin() {
    let s = space();
    let mut origin = FloatingOrigin::new(s, [3, 4, 5], 600.0).expect("origin");
    let bad = LargePosition {
        cell: [3, 4, 5],
        local: [0.0, f32::NAN, 0.0],
    };
    assert!(origin.update(&bad).is_err());
    let inf = LargePosition {
        cell: [3, 4, 5],
        local: [f32::INFINITY, 0.0, 0.0],
    };
    assert!(origin.update(&inf).is_err());
    assert_eq!(origin.cell(), [3, 4, 5]);
    // A focus at the far end of the addressable range rebases there without wrapping.
    let far = LargePosition {
        cell: [MAX_CELL_COORD, -MAX_CELL_COORD, 0],
        local: [0.0; 3],
    };
    let rebase = origin.update(&far).expect("far focus").expect("moves");
    assert_eq!(rebase.cell_delta, [MAX_CELL_COORD - 3, -MAX_CELL_COORD - 4, -5]);
    assert_eq!(origin.cell(), far.cell);
}

/// The same sequence of focus positions gives the same rebases, run after run.
#[test]
fn the_same_walk_gives_the_same_rebases() {
    let run = || {
        let s = space();
        let mut origin = FloatingOrigin::new(s, [0; 3], 800.0).expect("origin");
        let mut log = Vec::new();
        for i in 0..400i64 {
            let w = [i as f64 * 137.31, (i % 7) as f64 * -311.9, -(i as f64) * 53.7];
            let focus = s.from_world(w).expect("focus");
            if let Some(r) = origin.update(&focus).expect("update") {
                log.push((i, r.cell_delta, r.shift.map(f32::to_bits)));
            }
        }
        (log, origin.cell())
    };
    let first = run();
    assert!(!first.0.is_empty());
    assert_eq!(first, run());
}

// ---------------------------------------------------------------- stream grid

/// The addressable range ends exactly at `MAX_CELL_COORD` cells.
#[test]
fn cell_of_at_the_range_limit() {
    let g = StreamGrid::new(1.0).expect("grid");
    let max = MAX_CELL_COORD as f64;
    assert_eq!(
        g.cell_of(max, -max),
        Ok(CellCoord::new(MAX_CELL_COORD, -MAX_CELL_COORD))
    );
    assert!(matches!(
        g.cell_of(max * 2.0, 0.0),
        Err(StreamError::OutOfRange(_))
    ));
    assert!(matches!(
        g.cell_of(0.0, -max * 2.0),
        Err(StreamError::OutOfRange(_))
    ));
    // A tiny cell size turns a modest coordinate into an out-of-range cell.
    let tiny = StreamGrid::new(f64::MIN_POSITIVE).expect("grid");
    assert!(matches!(tiny.cell_of(1.0, 0.0), Err(StreamError::OutOfRange(_))));
}

/// `cells_within` with absurd radii reports an error instead of looping or overflowing.
#[test]
fn cells_within_with_absurd_radii_errors_cleanly() {
    let g = StreamGrid::new(100.0).expect("grid");
    for radius in [f64::MAX, 1.0e300, 1.0e30] {
        assert!(
            g.cells_within(0.0, 0.0, radius, u64::MAX).is_err(),
            "radius {radius} must not be accepted"
        );
    }
    assert!(g.cells_within(f64::NAN, 0.0, 10.0, 100).is_err());
    assert!(g.cells_within(0.0, f64::INFINITY, 10.0, 100).is_err());
    assert_eq!(
        g.cells_within(0.0, 0.0, f64::INFINITY, 100),
        Err(StreamError::BadRadius {
            what: "radius",
            value: f64::INFINITY
        })
    );
    // A limit of zero refuses even a single-cell answer (the box is at least 3 x 3 with its ring).
    assert!(matches!(
        g.cells_within(50.0, 50.0, 1.0, 0),
        Err(StreamError::TooManyCells { limit: 0, .. })
    ));
}

/// Results are the same on every call and equal the brute-force distance filter.
#[test]
fn cells_within_matches_brute_force_on_a_fixed_set_of_points() {
    let g = StreamGrid::new(37.5).expect("grid");
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % 20_000) as f64 / 10.0 - 1000.0
    };
    for _ in 0..40 {
        let (x, z) = (next(), next());
        let radius = (next().abs() % 300.0) + 0.0;
        let got = g.cells_within(x, z, radius, 1_000_000).expect("area");
        assert_eq!(got, g.cells_within(x, z, radius, 1_000_000).expect("again"));
        let c = g.cell_of(x, z).expect("cell");
        let reach = (radius / 37.5).ceil() as i64 + 2;
        let mut want = Vec::new();
        for cx in c.x - reach..=c.x + reach {
            for cz in c.z - reach..=c.z + reach {
                let cell = CellCoord::new(cx, cz);
                if g.distance_to_cell(x, z, cell) <= radius {
                    want.push(cell);
                }
            }
        }
        assert_eq!(got, want, "point ({x}, {z}) radius {radius}");
    }
}

/// Labels the parser accepts must name the cell they say; signs are part of the number.
#[test]
fn cell_labels_with_signs() {
    assert_eq!(CellCoord::from_label("-3_-4"), Ok(CellCoord::new(-3, -4)));
    assert_eq!(CellCoord::from_label("0_-0"), Ok(CellCoord::new(0, 0)));
    for bad in [
        "--1_2",
        "1_--2",
        "1__2",
        "9223372036854775808_0",
        "0_-9223372036854775809",
        "1_2 ",
    ] {
        assert!(
            matches!(CellCoord::from_label(bad), Err(StreamError::BadLabel(_))),
            "{bad:?}"
        );
    }
}

// ---------------------------------------------------------------- hlod

fn proxy(x: i64, z: i64, bytes: u64) -> CellProxy {
    CellProxy {
        cell: CellCoord::new(x, z),
        min_y: 0.0,
        max_y: 20.0,
        bytes,
    }
}

/// Each config bound accepts its edge value and refuses the value just past it.
#[test]
fn hlod_config_edges() {
    let ok = |f: &dyn Fn(&mut HlodConfig)| {
        let mut c = HlodConfig::default();
        f(&mut c);
        c.validate()
    };
    assert_eq!(ok(&|c| c.screen_size_threshold = 1.0), Ok(()));
    assert_eq!(ok(&|c| c.merge_factor = 2), Ok(()));
    assert_eq!(ok(&|c| c.merge_factor = 16), Ok(()));
    assert_eq!(ok(&|c| c.max_levels = MAX_HLOD_LEVELS), Ok(()));
    assert_eq!(ok(&|c| c.max_levels = 0), Ok(()));
    assert_eq!(ok(&|c| c.proxy_byte_ratio = 1.0), Ok(()));
    assert_eq!(ok(&|c| c.merge_factor = 1), Err(HlodError::BadMergeFactor(1)));
    assert_eq!(ok(&|c| c.merge_factor = 0), Err(HlodError::BadMergeFactor(0)));
    assert_eq!(ok(&|c| c.merge_factor = 17), Err(HlodError::BadMergeFactor(17)));
    assert_eq!(
        ok(&|c| c.max_levels = MAX_HLOD_LEVELS + 1),
        Err(HlodError::TooManyLevels(MAX_HLOD_LEVELS + 1))
    );
    assert!(matches!(
        ok(&|c| c.fov_y_rad = std::f64::consts::PI),
        Err(HlodError::BadFov(_))
    ));
    assert!(matches!(ok(&|c| c.fov_y_rad = 0.0), Err(HlodError::BadFov(_))));
    assert!(matches!(
        ok(&|c| c.fov_y_rad = f64::NAN),
        Err(HlodError::BadFov(_))
    ));
    assert!(matches!(
        ok(&|c| c.screen_size_threshold = 1.0000001),
        Err(HlodError::BadThreshold(_))
    ));
    assert!(matches!(
        ok(&|c| c.screen_size_threshold = f64::NAN),
        Err(HlodError::BadThreshold(_))
    ));
    assert!(matches!(
        ok(&|c| c.proxy_byte_ratio = 0.0),
        Err(HlodError::BadByteRatio(_))
    ));
    assert!(matches!(
        ok(&|c| c.proxy_byte_ratio = f64::INFINITY),
        Err(HlodError::BadByteRatio(_))
    ));
    assert!(matches!(
        ok(&|c| c.proxy_byte_ratio = 1.5),
        Err(HlodError::BadByteRatio(_))
    ));
}

/// Non-finite and inverted height ranges are refused, a flat range (min == max) is accepted.
#[test]
fn hlod_height_ranges() {
    let g = StreamGrid::new(100.0).expect("grid");
    let mk = |min_y: f64, max_y: f64| CellProxy {
        cell: CellCoord::new(0, 0),
        min_y,
        max_y,
        bytes: 1,
    };
    let build = |p: CellProxy| HlodTree::build(&g, HlodConfig::default(), &[p]);
    assert!(build(mk(5.0, 5.0)).is_ok());
    for (lo, hi) in [
        (1.0, 0.0),
        (f64::NAN, 1.0),
        (0.0, f64::NAN),
        (f64::NEG_INFINITY, 1.0),
        (0.0, f64::INFINITY),
    ] {
        assert_eq!(
            build(mk(lo, hi)).err(),
            Some(HlodError::BadBounds(CellCoord::new(0, 0))),
            "({lo}, {hi})"
        );
    }
}

/// An empty tree, a stale node index and a default tree answer without panicking.
#[test]
fn hlod_empty_tree_and_bad_indices() {
    let g = StreamGrid::new(100.0).expect("grid");
    let empty = HlodTree::build(&g, HlodConfig::default(), &[]).expect("empty");
    assert_eq!(empty.select(&[[0.0; 3]]), Ok(Vec::new()));
    assert_eq!(empty.select(&[]), Err(HlodError::NoViewers));
    assert_eq!(
        empty.select(&[[f64::NAN, 0.0, 0.0]]),
        Err(HlodError::NonFiniteViewer)
    );
    assert_eq!(empty.max_level(), 0);
    assert!(empty.leaf_for(CellCoord::new(0, 0)).is_none());
    assert!(empty.config().is_some(), "a built tree keeps its config");
    assert!(HlodTree::default().config().is_none());
    let tree = HlodTree::build(&g, HlodConfig::default(), &[proxy(0, 0, 10), proxy(1, 0, 10)]).expect("tree");
    assert!(tree.node(u32::MAX).is_none());
    assert!(tree.leaves_under(u32::MAX).is_empty());
    assert!(tree.leaf_for(CellCoord::new(5, 5)).is_none());
    assert_eq!(tree.leaves_under(tree.roots()[0]).len(), 2);
}

/// A far viewer sees the proxy bytes of the whole-tree root; the sum saturates instead of wrapping.
#[test]
fn hlod_proxy_bytes_saturate() {
    let g = StreamGrid::new(100.0).expect("grid");
    let tree = HlodTree::build(
        &g,
        HlodConfig {
            proxy_byte_ratio: 1.0,
            ..HlodConfig::default()
        },
        &[proxy(0, 0, u64::MAX), proxy(5, 5, u64::MAX)],
    )
    .expect("tree");
    let draws = tree.select(&[[1.0e9, 0.0, 1.0e9]]).expect("select");
    assert!(draws.iter().all(|d| d.kind == HlodDrawKind::Proxy));
    assert_eq!(tree.proxy_bytes_of(&draws), u64::MAX);
    let near = tree.select(&[[50.0, 10.0, 50.0]]).expect("select");
    assert!(near.iter().any(|d| d.kind == HlodDrawKind::Full));
}

/// Cells at the far end of the addressable range build without a panic and still cover each leaf once.
#[test]
fn hlod_cells_far_from_the_origin() {
    let g = StreamGrid::new(100.0).expect("grid");
    let far = MAX_CELL_COORD - 2;
    let proxies = [proxy(far, far, 5), proxy(far + 1, far, 5), proxy(-far, -far, 5)];
    let tree = HlodTree::build(&g, HlodConfig::default(), &proxies).expect("tree");
    assert_eq!(tree.leaf_count(), 3);
    let draws = tree.select(&[[0.0; 3]]).expect("select");
    let mut covered: Vec<CellCoord> = draws.iter().flat_map(|d| tree.leaves_under(d.node)).collect();
    covered.sort();
    covered.dedup();
    assert_eq!(covered.len(), 3);
}

// ---------------------------------------------------------------- stream plan

fn planner(cfg: StreamConfig) -> StreamPlanner {
    StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), cfg).expect("planner")
}

fn cfg() -> StreamConfig {
    StreamConfig {
        load_radius: 150.0,
        unload_radius: 250.0,
        memory_budget_bytes: 10_000,
        max_loads_per_step: 0,
    }
}

/// Config edges: equal radii are allowed, zero radii too; NaN and infinity are refused.
#[test]
fn stream_config_edges() {
    let mut c = cfg();
    c.unload_radius = c.load_radius;
    assert!(c.validate().is_ok());
    c.load_radius = 0.0;
    c.unload_radius = 0.0;
    assert!(c.validate().is_ok());
    for bad in [f64::NAN, f64::INFINITY, -0.001] {
        let mut c = cfg();
        c.load_radius = bad;
        assert!(matches!(
            c.validate(),
            Err(StreamError::BadRadius {
                what: "load_radius",
                ..
            })
        ));
        let mut c = cfg();
        c.unload_radius = bad;
        assert!(matches!(
            c.validate(),
            Err(StreamError::BadRadius {
                what: "unload_radius",
                ..
            })
        ));
    }
    let mut p = planner(cfg());
    let before = *p.config();
    let mut worse = cfg();
    worse.unload_radius = 1.0;
    assert!(p.set_config(worse).is_err());
    assert_eq!(*p.config(), before, "a refused config changes nothing");
}

/// With nothing registered a plan is empty; non-finite viewers are errors; the byte count is zero.
#[test]
fn an_empty_planner_plans_nothing() {
    let p = planner(cfg());
    assert!(p.plan(&[[0.0, 0.0]]).expect("plan").is_empty());
    assert_eq!(p.plan(&[]), Err(StreamError::NoViewers));
    assert!(p.plan(&[[f64::NAN, 0.0]]).is_err());
    assert_eq!(p.loaded_bytes(), 0);
    assert_eq!(p.cell_count(), 0);
}

/// Bytes at the `u64` limit do not overflow the budget arithmetic.
#[test]
fn huge_cell_sizes_do_not_overflow_the_budget() {
    let mut p = planner(StreamConfig {
        memory_budget_bytes: u64::MAX,
        ..cfg()
    });
    for x in 0..3 {
        p.register_cell(
            CellCoord::new(x, 0),
            CellInfo {
                bytes: u64::MAX / 2,
                priority: 0,
            },
        )
        .expect("register");
    }
    let plan = p.plan(&[[150.0, 50.0]]).expect("plan");
    p.apply(&plan).expect("apply stays within the budget");
    assert!(p.loaded_bytes() > 0, "something loaded");
    assert!(p.loaded_cells().count() <= 2, "two such cells fill the budget");
}
