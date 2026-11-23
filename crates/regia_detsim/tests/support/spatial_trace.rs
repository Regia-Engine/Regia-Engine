// SPDX-License-Identifier: MPL-2.0
//! Every candidate sequence and accepted sequence, nearest visit/result and update
//! of a 900-row, 40-group, 600-tick run. Shared by the det-golden runner test.
#[path = "spatial_fixture.rs"]
mod fixture;
use fixture::*;
use regia_detsim::broadphase::FloorRule;
use regia_detsim::golden::{DetRun, DetTrace, StateHasher};
use regia_detsim::tick::TickRate;
use std::ops::ControlFlow;

pub fn trace() -> DetTrace {
    let mut rows = field();
    let mut index = grid(FloorRule::Bands);
    let mut hashes = Vec::with_capacity(600);
    for tick in 0..600 {
        move_rows(&mut rows, tick);
        index.build(&rows).expect("valid trace rows");
        let mut h = StateHasher::new();
        h.u32("tick", tick as u32);
        for query in 0..rows.len() {
            let q = rows[query].body;
            let mask = !(1 << (query % 40));
            h.u32("query", query as u32);
            let _ = index.within(q, 80.0, mask, |slot| {
                h.u32("candidate", slot as u32);
                h.bool(
                    "accepted",
                    accepted(&q, 80.0, mask, &rows[slot], FloorRule::Bands),
                );
                ControlFlow::Continue(())
            });
            h.u32("range_end", u32::MAX);
            let best = index.nearest(q, mask, 120.0, |slot| {
                // Calls stop at the ring early-out (ADR-8875); the golden moved with it.
                h.u32("nearest_candidate", slot as u32);
                distance(&q, &rows[slot].body)
            });
            h.u32("nearest", best.map_or(u32::MAX, |s| s as u32));
            // Moves during the outer pass: later queries see the changed row.
            let moved = (query + tick) % rows.len();
            rows[moved].body.pos[0] += 0.125;
            let p = rows[moved].body.pos;
            index.update(moved, [p[0], p[2]]).expect("valid trace slot");
            h.u32("moved", moved as u32);
            h.f32("x", p[0]);
            h.f32("z", p[2]);
        }
        hashes.push(h.finish());
    }
    DetTrace {
        scenario: "spatial_queries".into(),
        run: DetRun::new(0x1234_9876, 600, TickRate::DEFAULT),
        tick_hashes: hashes,
        // A clean scenario: it must agree with the reference on every target (ADR-8231).
        planted: false,
    }
}
