// SPDX-License-Identifier: MPL-2.0
//! Real wgpu callbacks: a validation error becomes `regia::gpu_error` (with the span that made
//! the call) and destroying the device becomes `regia::device_lost`. Skips without an adapter.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(String, String)>>>);

struct Data(String);

impl Visit for Data {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "data" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut d = Data(String::new());
        event.record(&mut d);
        self.0
            .lock()
            .expect("capture lock")
            .push((event.metadata().target().to_owned(), d.0));
    }
}

#[test]
fn validation_error_and_device_lost_are_reported() {
    let capture = Capture::default();
    let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()));
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipped: no adapter ({e})");
            return;
        }
    };
    regia_telemetry::install_device_hooks(&device.device, false);
    {
        let _pass = tracing::info_span!("telemetry_test_pass").entered();
        // MAP_READ may only be combined with COPY_DST: a validation error.
        let _ = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("telemetry invalid buffer"),
            size: 256,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
    }
    device.device.destroy();
    let _ = device.device.poll(wgpu::PollType::Poll);
    let events = capture.0.lock().expect("lock").clone();
    let error = events
        .iter()
        .find(|(t, _)| t == "regia::gpu_error")
        .unwrap_or_else(|| panic!("no gpu_error event: {events:?}"));
    let data: serde_json::Value = serde_json::from_str(&error.1).expect("data json");
    assert_eq!(data["kind"], "validation");
    assert_eq!(data["pass"], "telemetry_test_pass");
    let lost = events
        .iter()
        .find(|(t, _)| t == "regia::device_lost")
        .unwrap_or_else(|| panic!("no device_lost event: {events:?}"));
    let data: serde_json::Value = serde_json::from_str(&lost.1).expect("data json");
    assert_eq!(data["reason"], "destroyed");
}
