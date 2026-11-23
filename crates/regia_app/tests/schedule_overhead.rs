// SPDX-License-Identifier: MPL-2.0
//! Quick schedule-overhead measurement (the criterion bench is `benches/schedule.rs`). Prints the
//! time per frame for 200 empty systems in `Update`; does not assert on timing, because CI machines
//! vary. The reference number is recorded in `docs/progress.md` by the lead.

use std::time::{Duration, Instant};

use regia_app::prelude::*;

fn empty_system() {}

#[test]
fn measure_200_empty_systems() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    for _ in 0..200 {
        app.add_systems(Update, empty_system);
    }
    app.finish_plugins();
    for _ in 0..100 {
        app.update();
    }
    let frames = 2_000;
    let start = Instant::now();
    for _ in 0..frames {
        app.update();
    }
    let per_frame = start.elapsed() / frames;
    println!(
        "schedule overhead: {:.4} ms/frame with 200 empty systems (target < 0.15 ms on the reference machine; debug builds are slower)",
        per_frame.as_secs_f64() * 1000.0
    );
    assert_eq!(app.world().resource::<Time>().frame_count(), 2_100);
}
