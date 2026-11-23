// SPDX-License-Identifier: MPL-2.0
// Tests of the path-request queue (ADR-8295).

use std::sync::Arc;

use regia_detsim::golden::scenarios::SplitMix64;

use super::*;
use crate::grid::{GridSettings, NavStatic, Stamp};

/// DO27's canvas (256 x 256 cells of 5 m) with a closed wall that has two gaps and a few discs.
fn canvas() -> NavGrid {
    let stat = Arc::new(NavStatic::build(None, Arc::new(GridSettings::default())).expect("static"));
    let (w, h) = (stat.width, stat.height);
    let mut closed = vec![0u8; w * h];
    for z in 0..h {
        if !(40..46).contains(&z) && !(200..204).contains(&z) {
            closed[z * w + 128] = 1;
        }
    }
    let mut grid = NavGrid::with_closed_cells(stat, closed).expect("grid");
    grid.sync_stamps(vec![
        (1, Stamp::from_world(-200.0, 100.0, 20.0)),
        (2, Stamp::from_world(250.0, -300.0, 35.0)),
        (3, Stamp::from_world(-400.0, -50.0, 15.0)),
    ]);
    grid
}

fn point(rng: &mut SplitMix64) -> [f32; 2] {
    [rng.signed() * 620.0, rng.signed() * 620.0]
}

fn plan_digest(plan: Option<&Plan>) -> Vec<u64> {
    match plan {
        None => vec![u64::MAX],
        Some(p) => {
            let mut v = vec![p.kind as u64, u64::from(p.snapped), u64::from(p.fine_expansions)];
            v.extend(p.cells.iter().map(|c| u64::from(*c)));
            v.extend(
                p.waypoints
                    .iter()
                    .flat_map(|w| [u64::from(w[0].to_bits()), u64::from(w[1].to_bits())]),
            );
            v
        }
    }
}

/// ADR-8295 acceptance: requests are served in `(submit tick, unit id)` order, never by arrival
/// order, and each serve stops once its budget is spent (the last request may overrun it) and
/// serves at least one.
#[test]
fn requests_are_served_in_submit_then_id_order_within_budget() {
    let grid = canvas();
    let budget = 900i64;
    let mut queue = PathRequests::new(RequestSettings {
        tick_budget: Some(budget),
        share_routes: true,
    });
    let mut scratch = Scratch::default();
    let mut rng = SplitMix64::new(0x5eed_0001);
    let mut submitted = 0u32;
    let mut results: Vec<PathResult> = Vec::new();
    for tick in 0..400u64 {
        if tick >= 12 && queue.pending() == 0 {
            break;
        }
        assert_eq!(queue.tick(), tick);
        if tick < 12 {
            // Units arrive out of id order.
            for _ in 0..6 {
                let unit = (rng.next_u64() % 500) as u32;
                let (from, to) = (point(&mut rng), point(&mut rng));
                let t = queue.request(unit, from, to, Profile((rng.next_u64() % 5) as u8));
                assert!(matches!(queue.poll(t), Poll::Pending { submit_tick } if submit_tick == tick));
                submitted += 1;
            }
        }
        let before = queue.pending();
        queue.serve(&grid, &mut scratch);
        let mut now = queue.drain_ready();
        now.sort_by_key(|r| r.seq);
        let last = queue.last_tick();
        assert_eq!(last.served as usize, now.len());
        if before > 0 {
            assert!(
                !now.is_empty(),
                "tick {tick}: a positive budget serves at least one"
            );
        }
        let spent: i64 = now.iter().map(PathResult::work).sum();
        if let Some(final_one) = now.last() {
            // Budget was left before the last request of the tick.
            assert!(
                spent - final_one.work() < budget,
                "tick {tick}: served past the budget"
            );
        }
        if queue.pending() > 0 {
            assert!(spent >= budget, "tick {tick}: carried requests with budget left");
        }
        assert_eq!(last.carried as usize, queue.pending());
        assert_eq!(last.work as i64, spent);
        results.extend(now);
    }
    assert_eq!(queue.pending(), 0);
    assert_eq!(results.len() as u32, submitted);
    for pair in results.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(a.seq < b.seq);
        assert!(
            (a.submit_tick, a.unit, a.ticket) < (b.submit_tick, b.unit, b.ticket),
            "served out of order: {:?} before {:?}",
            (a.submit_tick, a.unit),
            (b.submit_tick, b.unit)
        );
    }
    let c = queue.counters();
    assert_eq!(c.requests, u64::from(submitted));
    assert_eq!(c.served, u64::from(submitted));
    assert!(c.carried > 0, "a 900 budget carries work: {c:?}");
    assert!(results.iter().any(|r| r.waited() > 0));
    assert!(c.expansions > 0 && c.work >= c.expansions);
}

/// ADR-8295 acceptance: with 1,000 requests in one tick and later arrivals, no request waits more
/// than `floor(work ahead of it / budget)` ticks, where the work ahead is what the requests served
/// before it spent from its submit tick on; and every request is served.
#[test]
fn no_request_waits_longer_than_the_bound() {
    let grid = canvas();
    let mut queue = PathRequests::new(RequestSettings::for_grid(&grid));
    assert_eq!(queue.settings().tick_budget, Some(search::PATH_TICK_BUDGET));
    let budget = search::PATH_TICK_BUDGET;
    let mut scratch = Scratch::default();
    let mut rng = SplitMix64::new(0x5eed_1000);
    for unit in 0..1000u32 {
        let (from, to) = (point(&mut rng), point(&mut rng));
        queue.request(unit, from, to, Profile::HOVER);
    }
    let mut results: Vec<PathResult> = Vec::new();
    let mut ticks = 0u64;
    while queue.pending() > 0 || ticks < 30 {
        if ticks % 3 == 1 && ticks < 30 {
            // Later arrivals queue behind everything already waiting.
            for k in 0..20u32 {
                let (from, to) = (point(&mut rng), point(&mut rng));
                queue.request(2000 + k, from, to, Profile::FOOT);
            }
        }
        queue.serve(&grid, &mut scratch);
        results.extend(queue.drain_ready());
        ticks += 1;
        assert!(ticks < 10_000, "the queue drains");
    }
    results.sort_by_key(|r| r.seq);
    assert_eq!(results.len(), 1000 + 10 * 20);
    let mut worst = 0u64;
    for (i, r) in results.iter().enumerate() {
        let ahead: i64 = results[..i]
            .iter()
            .filter(|o| o.served_tick >= r.submit_tick)
            .map(PathResult::work)
            .sum();
        let bound = queue.wait_bound(ahead).expect("a positive budget has a bound");
        assert!(
            r.waited() <= bound,
            "request {} waited {} ticks, bound {bound} ({ahead} work ahead, budget {budget})",
            r.unit,
            r.waited()
        );
        worst = worst.max(r.waited());
    }
    assert!(worst > 0, "1,000 requests do not fit one tick");
    println!("1,000 requests on 256 x 256: {ticks} ticks, worst wait {worst}");
}

/// ADR-8295 acceptance: identical requests in one tick share one coarse route (one coarse search,
/// the rest counted as cache hits) and get the plan a lone request gets; without sharing every
/// request searches.
#[test]
fn same_tick_identical_requests_share_one_route() {
    let grid = canvas();
    let (from, to) = ([-500.0, -400.0], [500.0, 450.0]);
    let lone = search::plan(&grid, &mut Scratch::default(), Profile::HOVER, from, to).expect("route");
    assert!(lone.coarse_expansions > 0, "the route crosses many regions");
    for share in [true, false] {
        let mut queue = PathRequests::new(RequestSettings {
            tick_budget: None,
            share_routes: share,
        });
        let mut scratch = Scratch::default();
        for unit in (0..20u32).rev() {
            queue.request(unit, from, to, Profile::HOVER);
        }
        // A different region pair does not share.
        queue.request(99, [0.0, -600.0], [-600.0, 600.0], Profile::HOVER);
        queue.serve(&grid, &mut scratch);
        let last = queue.last_tick();
        assert_eq!(last.served, 21);
        let results = queue.drain_ready();
        if share {
            assert_eq!(last.cache_hits, 19, "{last:?}");
            assert_eq!(results.iter().filter(|r| r.shared_route).count(), 19);
        } else {
            assert_eq!(last.cache_hits, 0);
        }
        for r in results.iter().filter(|r| r.unit < 20) {
            let p = r.plan.as_ref().expect("plan");
            assert_eq!(p.waypoints, lone.waypoints, "unit {}", r.unit);
            assert_eq!(p.cells, lone.cells);
            assert_eq!(p.kind, lone.kind);
            let coarse = if r.shared_route { 0 } else { lone.coarse_expansions };
            assert_eq!(p.coarse_expansions, coarse);
        }
        let other = results.iter().find(|r| r.unit == 99).expect("unit 99");
        assert!(!other.shared_route);
        let searches = results.iter().filter(|r| !r.shared_route).count();
        assert_eq!(searches, if share { 2 } else { 21 });
    }
    // The cache outlives a serve while the grid generation holds, and a new generation (a stamp
    // moved) empties it.
    let mut grid = grid;
    let mut queue = PathRequests::new(RequestSettings::default());
    let mut scratch = Scratch::default();
    queue.request(1, from, to, Profile::HOVER);
    queue.serve(&grid, &mut scratch);
    queue.request(2, from, to, Profile::HOVER);
    queue.serve(&grid, &mut scratch);
    assert_eq!(queue.counters().cache_hits, 1, "same generation, next tick");
    let mut stamps = grid.stamps().to_vec();
    stamps.push((77, Stamp::from_world(600.0, 600.0, 5.0)));
    grid.sync_stamps(stamps);
    queue.request(3, from, to, Profile::HOVER);
    queue.serve(&grid, &mut scratch);
    assert_eq!(queue.counters().cache_hits, 1, "a new generation searches again");
    let last = queue
        .drain_ready()
        .into_iter()
        .find(|r| r.unit == 3)
        .expect("unit 3");
    assert!(!last.shared_route);
}

/// Without sharing, a served request is bit for bit the direct [`search::plan`] call, at the
/// default cluster and at 16; with sharing, requests whose key no earlier request used are too.
#[test]
fn served_requests_equal_direct_plans() {
    for k in [8usize, 16] {
        let base = canvas();
        let grid = if k == 8 {
            base
        } else {
            let s = Arc::new(GridSettings {
                cluster_cells: k,
                ..GridSettings::default()
            });
            let stat = Arc::new(NavStatic::build(None, s).expect("static"));
            let mut g = NavGrid::with_closed_cells(stat, base.closed.to_vec()).expect("grid");
            g.sync_stamps(base.stamps().to_vec());
            g
        };
        let mut queue = PathRequests::new(RequestSettings {
            tick_budget: Some(3000),
            share_routes: false,
        });
        let (mut qs, mut ds) = (Scratch::default(), Scratch::default());
        let mut rng = SplitMix64::new(0x5eed_00d1 + k as u64);
        let mut asked = Vec::new();
        for unit in 0..60u32 {
            let (from, to) = (point(&mut rng), point(&mut rng));
            let profile = Profile((rng.next_u64() % 5) as u8);
            let t = queue.request(unit, from, to, profile);
            asked.push((t, from, to, profile));
        }
        while queue.pending() > 0 {
            queue.serve(&grid, &mut qs);
        }
        for (t, from, to, profile) in asked {
            let Poll::Ready(r) = queue.poll(t) else {
                panic!("served");
            };
            let direct = search::plan(&grid, &mut ds, profile, from, to);
            assert_eq!(
                plan_digest(r.plan.as_ref()),
                plan_digest(direct.as_ref()),
                "k {k}"
            );
            assert!(matches!(queue.poll(t), Poll::Unknown), "handed over once");
        }
    }
}

/// Cancel drops a queued request (it is never served) or an unread result; unknown tickets stay
/// unknown.
#[test]
fn cancel_and_poll_follow_the_ticket() {
    let grid = canvas();
    let mut queue = PathRequests::new(RequestSettings::default());
    let mut scratch = Scratch::default();
    let a = queue.request(5, [0.0, 0.0], [100.0, 0.0], Profile::HOVER);
    let b = queue.request(6, [0.0, 0.0], [-100.0, 0.0], Profile::HOVER);
    assert!(queue.cancel(a));
    assert!(!queue.cancel(a));
    assert!(matches!(queue.poll(a), Poll::Unknown));
    assert!(matches!(queue.poll(Ticket(999)), Poll::Unknown));
    queue.serve(&grid, &mut scratch);
    assert_eq!(queue.ready(), 1);
    assert_eq!(queue.counters().cancelled, 1);
    assert_eq!(queue.counters().served, 1);
    let c = queue.request(7, [0.0, 0.0], [0.0, 90.0], Profile::FOOT);
    queue.serve(&grid, &mut scratch);
    assert!(queue.cancel(c), "an unread result can be dropped");
    match queue.poll(b) {
        Poll::Ready(r) => {
            assert_eq!(r.kind(), Some(PlanKind::Full));
            assert_eq!(r.unit, 6);
            assert_eq!(r.waited(), 0);
        }
        other => panic!("{other:?}"),
    }
    // A request with no free cell anywhere near its start has no plan.
    let stat = Arc::new(NavStatic::build(None, Arc::new(GridSettings::default())).expect("static"));
    let shut = NavGrid::with_closed_cells(stat, vec![1; 256 * 256]).expect("grid");
    let d = queue.request(8, [0.0, 0.0], [10.0, 0.0], Profile::HOVER);
    queue.serve(&shut, &mut scratch);
    match queue.poll(d) {
        Poll::Ready(r) => {
            assert_eq!(r.kind(), None);
            assert_eq!(r.work(), NO_PLAN_WORK);
        }
        other => panic!("{other:?}"),
    }
}

/// A seeded queue run (moving obstacles, a request stream, the budget on): the queue's state hash
/// per tick folded into one value, pinned on every architecture (ADR-8012 A-4).
fn queue_run() -> (u64, RequestCounters) {
    let mut grid = canvas();
    // A budget that keeps up with the stream (the backlog drains), so same-tick duplicates meet in
    // one serve.
    let mut queue = PathRequests::new(RequestSettings {
        tick_budget: Some(40_000),
        share_routes: true,
    });
    let mut scratch = Scratch::default();
    let mut rng = SplitMix64::new(0x5eed_8295);
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for tick in 0..120u32 {
        let mut stamps = vec![(1, Stamp::from_world(-200.0, 100.0, 20.0))];
        // The obstacle moves every fourth tick: between moves the grid generation holds and routes are
        // shared across serves.
        stamps.push((9, Stamp::from_world(-300.0 + (tick / 4) as f32 * 20.0, 0.0, 25.0)));
        grid.sync_stamps(stamps);
        for _ in 0..(rng.next_u64() % 4) {
            let unit = (rng.next_u64() % 400) as u32;
            let (from, to) = (point(&mut rng), point(&mut rng));
            queue.request(unit, from, to, Profile((rng.next_u64() % 5) as u8));
        }
        if tick % 7 == 3 {
            // Same-tick duplicates share routes.
            for unit in 500..510 {
                queue.request(unit, [-550.0, -550.0], [550.0, 500.0], Profile::FOOT);
            }
        }
        queue.serve(&grid, &mut scratch);
        fold = (fold ^ queue.state_hash()).wrapping_mul(0x100_0000_01b3);
        if tick % 5 == 0 {
            queue.drain_ready();
        }
    }
    (fold, queue.counters())
}

/// The queue's bits, pinned: the same value on x86-64 and aarch64, and on two runs.
#[test]
fn request_queue_pinned_on_every_arch() {
    let (a, counters) = queue_run();
    let (b, _) = queue_run();
    assert_eq!(a, b, "two runs agree");
    println!("queue fold {a:#018x} counters {counters:?}");
    assert!(counters.cache_hits >= 9, "{counters:?}");
    assert!(counters.carried > 0, "{counters:?}");
    assert!(
        counters.served * 10 >= counters.requests * 9,
        "the backlog drains: {counters:?}"
    );
    assert_eq!(a, PINNED_QUEUE, "the queue's results moved");
}

/// Recorded on x86-64 Windows (Rust 1.99.1) and checked on x86-64 Linux (Rust 1.99.0) on 2026-11-17.
const PINNED_QUEUE: u64 = 0xde82_14d8_ac07_fcc3;
