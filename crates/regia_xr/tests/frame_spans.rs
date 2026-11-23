// SPDX-License-Identifier: MPL-2.0
//! Spec 58 § 10 tracing spans on the CPU side of the XR frame: the simulator app runs one focused
//! frame under a recording subscriber, which must see every span the spec names for that path.
//! `xr_render_eye` and `xr_mirror` need a renderer (GPU) and are covered by reading the code; see
//! the lane report. One test per binary on purpose: span interest is process-global, and a sibling
//! test running without this subscriber could cache a callsite as `never`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_input::InputInjection;
use regia_xr::sim::ScriptState;
use regia_xr::{XrBackendKind, XrConfig, XrPlugin, XrState, drive_session_state};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Records the name of every span created while it is the default subscriber.
struct SpanNames {
    names: Arc<Mutex<BTreeSet<&'static str>>>,
    next: std::sync::atomic::AtomicU64,
}

impl Subscriber for SpanNames {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        self.names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(span.metadata().name());
        Id::from_u64(1 + self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn a_focused_simulator_frame_opens_every_cpu_span_spec_58_names() {
    let names = Arc::new(Mutex::new(BTreeSet::new()));
    let subscriber = SpanNames {
        names: Arc::clone(&names),
        next: std::sync::atomic::AtomicU64::new(0),
    };
    tracing::subscriber::with_default(subscriber, || {
        let mut app = App::new();
        enable_fixed_step(&mut app, 60.0);
        app.init_resource::<InputInjection>();
        app.add_plugins(XrPlugin::new(XrConfig {
            backend: XrBackendKind::Simulator,
            ..XrConfig::default()
        }));
        drive_session_state(app.world_mut(), ScriptState::Focused).expect("focused");
        for _ in 0..3 {
            app.update();
        }
        assert!(
            app.world().resource::<XrState>().stats.frames >= 1,
            "the simulator ran no XR frame"
        );
    });
    let seen = names
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for name in [
        "xr_poll_events",
        "xr_wait_frame",
        "xr_begin_frame",
        "xr_sync_actions",
        "xr_locate_views",
        "xr_extract",
        "xr_submit_layers",
        "xr_end_frame",
    ] {
        assert!(seen.contains(name), "span {name} missing; seen {seen:?}");
    }
}
