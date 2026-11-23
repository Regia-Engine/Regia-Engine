// SPDX-License-Identifier: MPL-2.0
//! A binding for an undeclared action is reported once, not every frame (mutation testing, ADR-3389). This
//! binary installs a process-wide subscriber that counts the warning, so it holds this one test only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;

static UNDECLARED_WARNINGS: AtomicUsize = AtomicUsize::new(0);

/// Counts `regia_input` warnings about undeclared actions; ignores everything else.
struct Counter;

struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Counter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        if *meta.level() != tracing::Level::WARN || meta.target() != "regia_input" {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.contains("undeclared action") {
            UNDECLARED_WARNINGS.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn an_undeclared_binding_warns_once_across_frames() {
    tracing::subscriber::set_global_default(Counter).expect("the only subscriber of this binary");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.125)));
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new("Ghost", InputSource::key("KeyG"))),
    ));
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        UNDECLARED_WARNINGS.load(Ordering::Relaxed),
        1,
        "one warning for `Ghost`, not one per frame"
    );
}
