// SPDX-License-Identifier: MPL-2.0
//! The per-tick CPU work of the frame-generation present path allocates nothing on a steady frame
//! (rev-56 shader-present finding 1). The window's frame keeps the pacer as it was before the tick
//! fed it, so a tick that queued no present can take its frame back (ADR-8930); that copy used to
//! be a fresh clone of the pacer, whose interval history is a `VecDeque`, on every tick.
//!
//! The device work of the path needs a window and a swap chain; this test drives the pacer
//! accounting it runs around that work (`regia_render::feed_frame_gen_tick`), every kind of tick:
//! presented, failed before a present, showed nothing, failed after a present.
//!
//! Its own test binary: it installs the counting global allocator.

use regia_render::frame_gen::FrameGenPacing;
use regia_render::frame_gen::present::FrameGenPresenter;
use regia_render::{FrameGenTick, feed_frame_gen_tick};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

/// 100 fps: above the default floor, so a warm pacer generates.
const FRAME_NS: u64 = 10_000_000;

/// The kinds of tick, as `(presented, failed)`.
const KINDS: [(bool, bool); 4] = [
    // Both presents queued.
    (true, false),
    // The interpolator, a blit or the worker failed before a present: the plain path presents.
    (false, true),
    // The swap chain gave no image: nothing was shown.
    (false, false),
    // A present was queued, then a later step failed.
    (true, true),
];

struct Ticks {
    presenter: FrameGenPresenter,
    pacing: FrameGenPacing,
    frame: u64,
}

impl Ticks {
    fn tick(&mut self, (presented, failed): (bool, bool)) {
        self.frame += 1;
        let _ = feed_frame_gen_tick(
            &mut self.presenter,
            &mut self.pacing,
            FrameGenTick {
                ready_ns: self.frame * FRAME_NS,
                reset: false,
                paused: false,
                min_base_fps: 30.0,
                presented,
                failed,
            },
        );
        // What `observe_presents` does after the window's present.
        if !std::mem::take(&mut self.pacing.fed_this_tick) {
            self.pacing.observe(self.frame * FRAME_NS, false, false, 30.0);
        }
    }
}

#[test]
fn a_steady_frame_gen_tick_allocates_nothing() {
    let _exclusive = alloc_counter::exclusive();
    alloc_counter::quiet_tracing();
    let mut ticks = Ticks {
        presenter: FrameGenPresenter::default(),
        pacing: FrameGenPacing::default(),
        frame: 0,
    };
    // Warm: the pacer's history fills its window, and every kind of tick runs once.
    for _ in 0..40 {
        ticks.tick(KINDS[0]);
    }
    for kind in KINDS {
        for _ in 0..4 {
            ticks.tick(kind);
        }
    }
    let generated_before = ticks.pacing.pacer.stats().generated;
    for kind in KINDS {
        let ((), counted) = alloc_counter::measure(|| {
            for _ in 0..30 {
                ticks.tick(kind);
            }
        });
        assert_eq!(
            counted.allocs, 0,
            "30 steady frame-generation ticks of kind (presented, failed) = {kind:?} allocated \
             {} times ({} bytes)",
            counted.allocs, counted.bytes
        );
    }
    assert!(
        ticks.pacing.pacer.stats().generated > generated_before,
        "the measured ticks never generated: the pacer was not warm"
    );
}
