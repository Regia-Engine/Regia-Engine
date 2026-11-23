// SPDX-License-Identifier: MPL-2.0
//! Public-API checks for `regia_core::telemetry`: counters, gauges and the bounded note ring.
//!
//! The telemetry statics are process-wide, so everything that touches the note ring lives in one
//! test (one sequence, no interleaving); counters and gauges use their own slots.

use regia_core::telemetry::{self, Counter, Gauge, NOTE_CAPACITY, NOTE_LABEL_BYTES, NoteKind};

#[test]
fn counter_and_gauge_names_are_unique_snake_case() {
    let counters = [
        Counter::SurfaceOutdated,
        Counter::SurfaceLost,
        Counter::SurfaceTimeout,
        Counter::SurfaceOther,
        Counter::TextureEvictions,
        Counter::AssetLoads,
        Counter::GpuErrors,
        Counter::Panics,
        Counter::DeviceLost,
        Counter::AudioSuspends,
        Counter::AudioThrottleSuspends,
    ];
    assert_eq!(
        counters.len(),
        Counter::COUNT,
        "the list must cover every counter"
    );
    let mut names: Vec<&str> = counters.iter().map(|c| c.name()).collect();
    for name in &names {
        assert!(
            name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{name}"
        );
    }
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), Counter::COUNT, "counter names must be unique");

    let gauges = [Gauge::ScatterEmitted, Gauge::ParticlesLive];
    assert_eq!(gauges.len(), Gauge::COUNT);
    assert_ne!(gauges[0].name(), gauges[1].name());
    assert_eq!(NoteKind::SlowAssetLoad.name(), "slow_asset_load");
    assert_eq!(NoteKind::PakCorrupt.name(), "pak_corrupt");
}

#[test]
fn counters_accumulate_and_gauges_overwrite() {
    let before = telemetry::counter(Counter::AudioThrottleSuspends);
    telemetry::add(Counter::AudioThrottleSuspends, 3);
    telemetry::add(Counter::AudioThrottleSuspends, 4);
    assert_eq!(telemetry::counter(Counter::AudioThrottleSuspends), before + 7);
    telemetry::add(Counter::AudioThrottleSuspends, 0);
    assert_eq!(telemetry::counter(Counter::AudioThrottleSuspends), before + 7);

    telemetry::set_gauge(Gauge::ParticlesLive, 120);
    telemetry::set_gauge(Gauge::ParticlesLive, 5);
    assert_eq!(
        telemetry::gauge(Gauge::ParticlesLive),
        5,
        "a gauge keeps the last value"
    );
}

#[test]
fn note_ring_truncates_labels_drops_when_full_and_drains_oldest_first() {
    let mut buffer = telemetry::note_buffer();
    assert_eq!(buffer.len(), NOTE_CAPACITY);
    assert!(!telemetry::notes_pending());
    assert_eq!(
        telemetry::drain_notes(&mut buffer),
        0,
        "draining an empty ring copies nothing"
    );

    // A label cut inside a multi-byte character backs up to the previous boundary.
    let long = format!("a{}", "é".repeat(100));
    telemetry::note(NoteKind::PakCorrupt, &long, -1.0);
    assert!(telemetry::notes_pending());
    let n = telemetry::drain_notes(&mut buffer);
    assert_eq!(n, 1);
    let first = buffer[0];
    assert_eq!(first.kind, NoteKind::PakCorrupt);
    assert_eq!(first.value, -1.0);
    assert!(first.label().len() <= NOTE_LABEL_BYTES);
    assert_eq!(
        first.label().len(),
        119,
        "byte 120 is inside an `é`, so the cut is at 119"
    );
    assert!(long.starts_with(first.label()));
    assert!(
        !telemetry::notes_pending(),
        "pending clears once the ring is empty"
    );

    // An empty label and a short label survive intact.
    telemetry::note(NoteKind::SlowAssetLoad, "", 1.5);
    telemetry::note(NoteKind::SlowAssetLoad, "mesh.ron", 2.5);
    assert_eq!(telemetry::drain_notes(&mut buffer), 2);
    assert_eq!(buffer[0].label(), "");
    assert_eq!(buffer[1].label(), "mesh.ron");
    assert_eq!(buffer[1].value, 2.5);

    // Overfill: the extras are dropped and counted, the oldest are kept.
    let dropped_before = telemetry::notes_dropped();
    for i in 0..NOTE_CAPACITY + 3 {
        telemetry::note(NoteKind::SlowAssetLoad, &format!("n{i}"), i as f64);
    }
    assert_eq!(telemetry::notes_dropped(), dropped_before + 3);

    // A short destination takes only what fits and leaves the rest pending, order preserved.
    let mut small = [buffer[0]; 10];
    assert_eq!(telemetry::drain_notes(&mut small), 10);
    for (i, note) in small.iter().enumerate() {
        assert_eq!(note.value, i as f64);
        assert_eq!(note.label(), format!("n{i}"));
    }
    assert!(telemetry::notes_pending(), "the remainder is still queued");

    let rest = telemetry::drain_notes(&mut buffer);
    assert_eq!(rest, NOTE_CAPACITY - 10);
    assert_eq!(
        buffer[0].value, 10.0,
        "the remainder continues where the first drain stopped"
    );
    assert_eq!(buffer[rest - 1].value, (NOTE_CAPACITY - 1) as f64);
    assert!(!telemetry::notes_pending());

    // The ring accepts notes again after a drain.
    telemetry::note(NoteKind::PakCorrupt, "again", 0.0);
    assert_eq!(telemetry::drain_notes(&mut buffer), 1);
    assert_eq!(buffer[0].label(), "again");

    // A zero-length destination copies nothing and keeps the note queued.
    telemetry::note(NoteKind::PakCorrupt, "kept", 0.0);
    assert_eq!(telemetry::drain_notes(&mut []), 0);
    assert!(telemetry::notes_pending());
    assert_eq!(telemetry::drain_notes(&mut buffer), 1);
    assert_eq!(buffer[0].label(), "kept");
}
