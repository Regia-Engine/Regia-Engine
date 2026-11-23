// SPDX-License-Identifier: MPL-2.0
//! A display queue that stays one image full is drained (ADR-5564).
//!
//! The TEST Pixel 9 Pro trace behind it: after pacing `Off` (or a hitch, or a preset change) the
//! swapchain held every frame back about 8 ms, every frame started at once on its missed slot
//! tick (ADR-2320's held start, ADR-4913's late start), and the queue never emptied: frame starts
//! followed the compositor's buffer releases, with a presented p99 of 1.25-1.29x the refresh.

use regia_platform::frame_pacing::PacingModel;

const P60: u64 = 16_666_667;
const MS: u64 = 1_000_000;

/// A 60 Hz model with the runner's `Work`-cost switches; `drain` turns the queue drain on.
struct Phone {
    model: PacingModel,
    fed: u64,
    release: u64,
}

impl Phone {
    fn new(drain: bool) -> Self {
        let mut model = PacingModel::new();
        model.set_late_start(true);
        if drain {
            model.set_queue_drain(true);
        }
        for i in 1..=8 {
            model.on_tick(i * P60);
        }
        model.end_frame(9 * MS, None, None);
        let first = model.plan(8 * P60).expect("a period");
        model.released(first);
        Self {
            model,
            fed: 8 * P60,
            release: first,
        }
    }

    fn feed_to(&mut self, t: u64) {
        while self.fed + P60 <= t {
            self.fed += P60;
            self.model.on_tick(self.fed);
        }
    }

    /// One frame: it ends `end_after` past its release tick, `held` of it blocked in the
    /// swapchain, with `work` of its own. Returns the vsyncs from its release to the next.
    fn frame(&mut self, end_after: u64, held: u64, work: u64) -> u64 {
        let end = self.release + end_after;
        self.feed_to(end);
        self.model.swapchain_held(held);
        self.model.end_frame(work, None, None);
        let bound = self.model.plan(end).expect("a period");
        let tick = bound.div_ceil(P60) * P60;
        self.feed_to(tick);
        self.model.released(tick);
        let vsyncs = (tick - self.release) / P60;
        self.release = tick;
        vsyncs
    }

    /// A frame of the stuck queue: 9 ms of work, 8 ms blocked in the swapchain, so it ends just
    /// after its slot's tick.
    fn stuck(&mut self) -> u64 {
        self.frame(P60 + 300_000, 8 * MS, 9 * MS)
    }

    /// A frame after the queue drained: 9 ms of work, 0.5 ms in the swapchain, done well before
    /// its slot.
    fn free(&mut self) -> u64 {
        self.frame(10 * MS, MS / 2, 9 * MS)
    }
}

#[test]
fn a_new_model_keeps_the_old_held_start() {
    let mut phone = Phone::new(false);
    for _ in 0..200 {
        assert_eq!(phone.stuck(), 1, "every held frame starts at once");
    }
    assert_eq!(phone.model.stats().drains, 0);
}

#[test]
fn a_queue_that_stays_full_is_drained_once() {
    let mut phone = Phone::new(true);
    let gaps: Vec<u64> = (0..8).map(|_| phone.stuck()).collect();
    assert_eq!(
        gaps,
        [1, 1, 1, 1, 1, 1, 1, 2],
        "the eighth held start in a row waits a vsync"
    );
    assert_eq!(phone.model.stats().drains, 1);
    // The queue is one image emptier: frames end before their slot and wait for its tick.
    for _ in 0..300 {
        assert_eq!(phone.free(), 1);
    }
    assert_eq!(
        phone.model.stats().drains,
        1,
        "no drain while frames wait for their slot"
    );
    assert_eq!(phone.model.swap_interval(), 1);
}

#[test]
fn held_frames_that_wait_for_their_slot_never_drain() {
    // Mobile High on the same phone: 3 ms in the swapchain (held), but done before the slot.
    let mut phone = Phone::new(true);
    for _ in 0..500 {
        assert_eq!(phone.frame(14 * MS, 3 * MS, 11 * MS), 1);
    }
    assert_eq!(phone.model.stats().drains, 0);
}

#[test]
fn a_run_broken_by_a_frame_on_its_slot_starts_again() {
    let mut phone = Phone::new(true);
    for _ in 0..50 {
        for _ in 0..7 {
            assert_eq!(phone.stuck(), 1);
        }
        assert_eq!(phone.free(), 1);
    }
    assert_eq!(
        phone.model.stats().drains,
        0,
        "seven held starts and a free frame, never eight"
    );
}

#[test]
fn a_queue_that_refills_at_once_backs_off() {
    // Frames really limited by the display queue: it is full again right after every drain.
    let mut phone = Phone::new(true);
    let mut drains_at = Vec::new();
    for frame in 1..=6000u32 {
        let before = phone.model.stats().drains;
        phone.stuck();
        if phone.model.stats().drains > before {
            drains_at.push(frame);
        }
    }
    let gaps: Vec<u32> = drains_at.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(drains_at.first(), Some(&8));
    // Each drain that did not stick doubles the run the next one needs, up to 1024.
    assert_eq!(&gaps[..8], &[8, 16, 32, 64, 128, 256, 512, 1024]);
    assert!(gaps[8..].iter().all(|&g| g == 1024), "{gaps:?}");
    assert!(drains_at.len() <= 12, "{} drains in 6000 frames", drains_at.len());
}

#[test]
fn a_drain_that_sticks_keeps_the_short_run() {
    let mut phone = Phone::new(true);
    for _ in 0..3 {
        for _ in 0..8 {
            phone.stuck();
        }
        for _ in 0..100 {
            assert_eq!(phone.free(), 1);
        }
    }
    assert_eq!(phone.model.stats().drains, 3, "each stuck run of 8 drains once");
}

#[test]
fn a_restart_forgets_the_run() {
    let mut phone = Phone::new(true);
    for _ in 0..7 {
        phone.stuck();
    }
    phone.model.restart();
    // The ticks start a new timeline after a gap.
    let base = phone.fed + 10 * P60;
    for i in 0..4 {
        phone.model.on_tick(base + i * P60);
    }
    phone.fed = base + 3 * P60;
    phone.model.end_frame(9 * MS, None, None);
    let first = phone.model.plan(phone.fed).expect("a period");
    phone.model.released(first);
    phone.release = first;
    for _ in 0..7 {
        assert_eq!(phone.stuck(), 1, "the run starts from zero after a restart");
    }
    assert_eq!(phone.model.stats().drains, 0);
}
