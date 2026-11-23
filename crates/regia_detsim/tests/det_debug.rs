// SPDX-License-Identifier: MPL-2.0
//! Debug draw of the deterministic indexes (ADR-8562): every contact of the last pass is drawn,
//! queries show their shapes and hits, and capture never changes a simulated bit.

use std::ops::ControlFlow;

use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
use regia_detsim::debug::{DebugCapture, DebugKind, DebugPrim, DebugRecorder, DebugSource, DebugWhat};
use regia_detsim::math;
use regia_detsim::spatial::{DetSpatial, SpatialRow};

/// A small deterministic generator (test data only).
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1_u64 << 24) as f32)
    }
}

fn crowd(n: usize, seed: u64, spread: f32) -> Vec<BroadBody> {
    let mut rng = Lcg(seed);
    (0..n)
        .map(|_| {
            let x = rng.unit() * spread;
            let z = rng.unit() * spread;
            let r = 0.4 + rng.unit() * 0.6;
            BroadBody::new([x, 0.0, z], r, [-1.0, 2.0])
        })
        .collect()
}

fn push_apart(bodies: &mut [BroadBody], c: regia_detsim::broadphase::Contact) {
    let d = math::sqrt(c.dist_sq);
    if d > 0.0 {
        let push = (c.min_dist - d) * 0.5 / d;
        bodies[c.a].pos[0] -= c.dx * push;
        bodies[c.a].pos[2] -= c.dz * push;
        bodies[c.b].pos[0] += c.dx * push;
        bodies[c.b].pos[2] += c.dz * push;
    }
}

#[test]
fn debug_emit_lists_every_contact_of_the_last_pass() {
    let mut bodies = crowd(300, 7, 30.0);
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    grid.set_debug_capture(Some(DebugCapture::default()));
    let mut seen = Vec::new();
    for pass in 0..3 {
        seen.clear();
        grid.for_each_contact(&mut bodies, |bodies, c| {
            seen.push((bodies[c.a].pos, bodies[c.b].pos));
            push_apart(bodies, c);
        });
        let stats = grid.stats();
        assert!(stats.contacts > 0, "pass {pass} has contacts");
        let mut out = DebugRecorder::default();
        grid.debug_emit(&mut out, DebugWhat::CONTACTS);
        assert_eq!(
            out.count_kind(DebugKind::Contact) as u64,
            stats.contacts,
            "pass {pass}: one contact line per contact"
        );
        assert_eq!(out.len() as u64, stats.contacts, "CONTACTS draws contacts only");
        // The lines join the two centers as the visitor saw them, in visit order.
        let lines: Vec<_> = out
            .prims()
            .iter()
            .map(|p| match *p {
                DebugPrim::Line { a, b, .. } => (a, b),
                _ => unreachable!("contacts are lines"),
            })
            .collect();
        assert_eq!(lines, seen, "pass {pass}");

        let mut cand = DebugRecorder::default();
        grid.debug_emit(&mut cand, DebugWhat::CANDIDATES);
        assert_eq!(cand.count_kind(DebugKind::Candidate) as u64, stats.candidates);
        let capture = grid.debug_capture_stats();
        assert_eq!(capture.contacts as u64, stats.contacts);
        assert_eq!(capture.contacts_dropped, 0);
    }
}

#[test]
fn contacts_past_the_cap_are_counted_as_dropped() {
    let mut bodies = crowd(300, 7, 30.0);
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    grid.set_debug_capture(Some(DebugCapture {
        contacts: 5,
        candidates: 5,
        ..DebugCapture::default()
    }));
    grid.for_each_contact(&mut bodies, |_, _| {});
    let stats = grid.stats();
    let capture = grid.debug_capture_stats();
    assert_eq!(capture.contacts, 5);
    assert_eq!(capture.contacts as u64 + capture.contacts_dropped, stats.contacts);
    assert_eq!(
        capture.candidates as u64 + capture.candidates_dropped,
        stats.candidates
    );
}

#[test]
fn grid_and_occupancy_count_every_body_once() {
    let mut bodies = crowd(120, 3, 20.0);
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    grid.set_debug_capture(Some(DebugCapture::default()));
    grid.for_each_contact(&mut bodies, |_, _| {});
    let mut out = DebugRecorder::default();
    grid.debug_emit(&mut out, DebugWhat::GRID.union(DebugWhat::OCCUPANCY));
    let total: u64 = out
        .prims()
        .iter()
        .filter(|p| p.kind() == DebugKind::Occupancy)
        .map(|p| out.text_of(p).parse::<u64>().expect("a count"))
        .sum();
    assert_eq!(total, 120, "occupancy texts sum to the body count");
    let cells = out.count_kind(DebugKind::Occupancy);
    assert_eq!(
        out.count_kind(DebugKind::Grid),
        cells + 1,
        "bounds plus one outline per cell in use"
    );

    let mut index = DetSpatial::default();
    index.set_debug_capture(Some(DebugCapture::default()));
    let table: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(slot, body)| SpatialRow {
            slot,
            body: *body,
            group_mask: 1,
            flags: 0,
        })
        .collect();
    index.build(&table).expect("rows");
    let mut out = DebugRecorder::default();
    index.debug_emit(&mut out, DebugWhat::OCCUPANCY);
    let total: u64 = out
        .prims()
        .iter()
        .map(|p| out.text_of(p).parse::<u64>().expect("a count"))
        .sum();
    assert_eq!(total, 120);
}

#[test]
fn the_query_ring_keeps_the_last_queries_with_their_hits() {
    let bodies = crowd(80, 11, 20.0);
    let table: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(slot, body)| SpatialRow {
            slot,
            body: *body,
            group_mask: 1,
            flags: 0,
        })
        .collect();
    let mut index = DetSpatial::default();
    index.set_debug_capture(Some(DebugCapture {
        queries: 4,
        ..DebugCapture::default()
    }));
    index.build(&table).expect("rows");
    let center = |i: usize| BroadBody::new([i as f32 * 2.0, 0.0, 10.0], 0.0, [-1.0, 2.0]);
    // Six queries: the ring keeps the last four.
    for i in 0..6 {
        let _ = index.within(center(i), 3.0, 1, |_| ControlFlow::Continue(()));
    }
    let mut out = DebugRecorder::default();
    index.debug_emit(&mut out, DebugWhat::QUERIES);
    let circles: Vec<_> = out
        .prims()
        .iter()
        .filter_map(|p| match *p {
            DebugPrim::Circle { center, radius, .. } => Some((center[0], radius)),
            _ => None,
        })
        .collect();
    assert_eq!(
        circles,
        [(4.0, 3.0), (6.0, 3.0), (8.0, 3.0), (10.0, 3.0)],
        "oldest first"
    );
    assert_eq!(index.debug_capture_stats().queries, 4);

    // `nearest` draws exactly its result.
    let mut tiny = DetSpatial::default();
    tiny.set_debug_capture(Some(DebugCapture::default()));
    tiny.build(&table).expect("rows");
    let point = center(3);
    let best = tiny
        .nearest(point, 1, 50.0, |slot| {
            let dx = table[slot].body.pos[0] - point.pos[0];
            let dz = table[slot].body.pos[2] - point.pos[2];
            dx * dx + dz * dz
        })
        .expect("a nearest row");
    let mut out = DebugRecorder::default();
    tiny.debug_emit(&mut out, DebugWhat::QUERIES);
    let hits: Vec<_> = out
        .prims()
        .iter()
        .filter_map(|p| match *p {
            DebugPrim::Line { b, .. } => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(hits, [table[best].body.pos]);
}

/// FNV-1a over the bits of every position (test-only state hash).
fn state_hash(bodies: &[BroadBody], h: &mut u64) {
    for b in bodies {
        for v in b.pos {
            for byte in v.to_bits().to_le_bytes() {
                *h ^= u64::from(byte);
                *h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
}

/// Steps a crowd with the broadphase and the spatial index for `ticks` ticks and returns one hash
/// per tick. With `capture`, both indexes capture and are drawn every tick.
fn run(capture: bool, ticks: usize) -> Vec<u64> {
    let mut bodies = crowd(200, 42, 25.0);
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    let mut index = DetSpatial::new(BroadphaseConfig {
        cell_size: 8.0,
        ..BroadphaseConfig::default()
    });
    if capture {
        let sources: [&mut dyn DebugSource; 2] = [&mut grid, &mut index];
        for source in sources {
            source.set_debug_capture(Some(DebugCapture::default()));
        }
    }
    let mut table = Vec::with_capacity(bodies.len());
    let mut picks = Vec::with_capacity(4);
    let mut out = DebugRecorder::default();
    let mut hashes = Vec::with_capacity(ticks);
    for _ in 0..ticks {
        grid.for_each_contact(&mut bodies, push_apart);
        table.clear();
        table.extend(bodies.iter().enumerate().map(|(slot, body)| SpatialRow {
            slot,
            body: *body,
            group_mask: 1 << (slot % 2),
            flags: 0,
        }));
        index.build(&table).expect("rows");
        for (i, body) in bodies.iter_mut().enumerate() {
            let me = *body;
            let enemy = 1_u64 << ((i + 1) % 2);
            let target = index.nearest(me, enemy, 12.0, |slot| {
                let dx = table[slot].body.pos[0] - me.pos[0];
                let dz = table[slot].body.pos[2] - me.pos[2];
                dx * dx + dz * dz
            });
            index.first_k_within_by(me, 4.0, enemy, 3, &mut picks, |slot| slot % 3 != 0);
            if let Some(t) = target {
                let dx = table[t].body.pos[0] - me.pos[0];
                let dz = table[t].body.pos[2] - me.pos[2];
                let d = math::sqrt(dx * dx + dz * dz);
                if d > 0.0 {
                    let step = 0.05 * (1.0 + picks.len() as f32) / d;
                    body.pos[0] += dx * step;
                    body.pos[2] += dz * step;
                }
            }
        }
        if capture {
            out.clear();
            grid.debug_emit(&mut out, DebugWhat::ALL);
            index.debug_emit(&mut out, DebugWhat::ALL);
            assert!(!out.is_empty());
        }
        let mut h = 0xcbf2_9ce4_8422_2325;
        state_hash(&bodies, &mut h);
        hashes.push(h);
    }
    hashes
}

#[test]
fn capture_on_and_off_step_the_same_bits() {
    let off = run(false, 40);
    let on = run(true, 40);
    assert_eq!(off.len(), 40);
    assert_eq!(
        off, on,
        "debug capture reads state only: every tick hashes the same"
    );
    assert!(off.windows(2).any(|w| w[0] != w[1]), "the crowd moves");
}
