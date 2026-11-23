// SPDX-License-Identifier: MPL-2.0
//! Schedule overhead (`docs/specs/10-core.md` § 4): one frame of an app with 200 empty systems in
//! `Update`. Target: < 0.15 ms/frame on the desktop reference machine.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_app::prelude::*;

fn empty_system() {}

fn app_with_empty_systems(count: usize) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    for _ in 0..count {
        app.add_systems(Update, empty_system);
    }
    app.finish_plugins();
    // Build the schedules before measuring.
    app.update();
    app
}

fn schedule_overhead(c: &mut Criterion) {
    let mut app = app_with_empty_systems(200);
    c.bench_function("frame_200_empty_systems", |b| {
        b.iter(|| {
            app.update();
            black_box(app.world().resource::<Time>().frame_count());
        });
    });
    let mut bare = app_with_empty_systems(0);
    c.bench_function("frame_no_user_systems", |b| {
        b.iter(|| {
            bare.update();
            black_box(bare.world().resource::<Time>().frame_count());
        });
    });
}

criterion_group!(benches, schedule_overhead);
criterion_main!(benches);
