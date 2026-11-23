// SPDX-License-Identifier: MPL-2.0
//! Span audit (AGENTS.md § 3): per-frame systems carry `tracing` spans. A headless app with the
//! scene, physics, animation and audio plugins runs a few frames under a recording subscriber and the
//! test requires the span names listed in `EXPECTED`, so a span that is removed or renamed is noticed.
//! Systems that run only with entities (animation evaluation, sockets) or a GPU frame (renderer) are listed in the audit report instead.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_animation::AnimationPlugin;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_audio::{AudioPlugin, AudioSettings};
use regia_physics::PhysicsPlugin;
use regia_scene::ScenePlugin;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Span names a headless frame must open. Add a name when a per-frame system gets a span.
const EXPECTED: &[&str] = &[
    "apply_property_animators",
    "level_lifecycle",
    "propagate_transforms",
    "resolve_pending_prefabs",
];

/// A subscriber that records the name of every span created.
#[derive(Clone, Default)]
struct Recorder {
    names: Arc<Mutex<BTreeSet<&'static str>>>,
    next: Arc<AtomicU64>,
}

impl Subscriber for Recorder {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        if let Ok(mut names) = self.names.lock() {
            names.insert(attrs.metadata().name());
        }
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn a_headless_frame_opens_the_audited_spans() {
    let recorder = Recorder::default();
    let names = recorder.names.clone();
    tracing::subscriber::with_default(recorder, || {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins((ScenePlugin, PhysicsPlugin, AnimationPlugin, AudioPlugin));
        for _ in 0..5 {
            app.update();
        }
    });
    let seen = names.lock().map(|n| n.clone()).unwrap_or_default();
    let missing: Vec<&str> = EXPECTED.iter().copied().filter(|n| !seen.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "spans no longer opened by a headless frame: {missing:?}\nseen: {seen:?}"
    );
}
