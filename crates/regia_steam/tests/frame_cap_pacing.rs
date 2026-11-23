// SPDX-License-Identifier: MPL-2.0
//! The Deck frame caps pace a loop at the right rate (lane v020-steam-deck): a frame of 4 ms of
//! work, paced at each cap, averages the cap's period with no early frames, and a long hitch is not
//! "caught up" with a burst of unpaced frames.
//!
//! Real-time on a shared machine: a preempted wake can make one frame late (and, within a period,
//! the next one short, as the pacer keeps its deadlines). So the checks use the median frame and
//! refuse only a real burst (a frame under a tenth of the period), not one short frame
//! (audit-v035-flaky H2: a mean within 6 % and "no frame under 0.7 periods" failed under load).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use regia_app::FramePacing;
use regia_steam::DeckFrameCap;

fn run(cap: DeckFrameCap, frames: u32) -> Vec<Duration> {
    let mut pacing = FramePacing::default();
    cap.apply(&mut pacing, None);
    let mut stamps = Vec::new();
    for _ in 0..frames {
        let work = Instant::now();
        while work.elapsed() < Duration::from_millis(4) {
            std::hint::spin_loop();
        }
        pacing.wait();
        stamps.push(Instant::now());
    }
    stamps.windows(2).map(|w| w[1] - w[0]).collect()
}

#[test]
fn every_cap_paces_to_its_period() {
    for cap in DeckFrameCap::ALL {
        let period = 1.0 / f64::from(cap.fps());
        let deltas = run(cap, 24);
        let mut sorted: Vec<f64> = deltas.iter().map(Duration::as_secs_f64).collect();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        // 10 % tells every pair of caps apart (40 and 45 fps differ by 12.5 %).
        assert!(
            (median / period - 1.0).abs() < 0.10,
            "{} fps: median frame {:.2} ms vs {:.2} ms",
            cap.fps(),
            median * 1000.0,
            period * 1000.0
        );
        let earliest = sorted[0];
        assert!(
            earliest > period * 0.1,
            "{} fps: a burst frame came {:.2} ms after the last",
            cap.fps(),
            earliest * 1000.0
        );
    }
}

#[test]
fn a_hitch_is_not_caught_up_with_a_burst() {
    let mut pacing = FramePacing::default();
    DeckFrameCap::Fps40.apply(&mut pacing, None);
    pacing.wait();
    std::thread::sleep(Duration::from_millis(200)); // a 200 ms hitch = 8 missed frames
    let t = Instant::now();
    pacing.wait(); // late: returns at once and resynchronizes
    pacing.wait();
    pacing.wait();
    // The two following frames still take a full period each (50 ms), not zero.
    assert!(t.elapsed() >= Duration::from_millis(40), "{:?}", t.elapsed());
}
