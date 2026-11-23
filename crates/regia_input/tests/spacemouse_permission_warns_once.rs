// SPDX-License-Identifier: MPL-2.0
//! `docs/specs/29-spacemouse.md` § 2.6: a SpaceMouse the user may not read (Linux hidraw without the
//! udev rule) is a single `warn` that carries the rule, then silence, and a device that later opens
//! is no longer listed as unavailable. This binary installs a process-wide subscriber that counts
//! the warning, so it holds this one test only.
#![cfg(all(feature = "devices", not(target_os = "android")))]

use std::sync::atomic::{AtomicUsize, Ordering};

use regia_app::prelude::*;
use regia_input::InputPlugin;
use regia_input::spacemouse::{
    FakeSpaceMouse, LINUX_UDEV_RULE, SpaceMouseDeviceStatus, SpaceMouseDevices, SpaceMousePlugin,
    SpaceMouseState,
};

static PERMISSION_WARNINGS: AtomicUsize = AtomicUsize::new(0);
static WARNINGS_WITH_THE_RULE: AtomicUsize = AtomicUsize::new(0);

/// Counts `regia_input` warnings about unreadable SpaceMice; ignores everything else.
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
        if message.0.contains("SpaceMouse") && message.0.contains("not readable") {
            PERMISSION_WARNINGS.fetch_add(1, Ordering::Relaxed);
            // The message field is `fmt::Arguments`, whose `Debug` is its `Display`; accept an
            // escaped copy too in case a subscriber layer quotes it.
            let first_rule = LINUX_UDEV_RULE.lines().nth(1).unwrap_or_default();
            let has_rule =
                message.0.contains(first_rule) || message.0.contains(&first_rule.replace('"', "\\\""));
            if message.0.contains("udevadm") && has_rule {
                WARNINGS_WITH_THE_RULE.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn unavailable(devices: &SpaceMouseDevices) -> Vec<String> {
    devices
        .devices
        .iter()
        .filter(|d| matches!(d.status, SpaceMouseDeviceStatus::Unavailable(_)))
        .map(|d| d.info.name.clone())
        .collect()
}

#[test]
fn an_unreadable_device_warns_once_with_the_udev_rule_and_opening_it_clears_the_entry() {
    tracing::subscriber::set_global_default(Counter).expect("the only subscriber of this binary");
    let fake = FakeSpaceMouse::default();
    let mut app = App::new();
    app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
    app.add_plugins(InputPlugin::default());
    app.update();

    // Two devices refused for permissions, reported again on every rescan for many frames.
    for _ in 0..20 {
        fake.unavailable(
            "SpaceMouse Wireless",
            "permission denied opening /dev/hidraw3",
            true,
        );
        fake.unavailable(
            "SpaceMouse Compact",
            "permission denied opening /dev/hidraw5",
            true,
        );
        app.update();
    }
    assert_eq!(
        PERMISSION_WARNINGS.load(Ordering::Relaxed),
        1,
        "one warning, then silence"
    );
    assert_eq!(
        WARNINGS_WITH_THE_RULE.load(Ordering::Relaxed),
        1,
        "the warning carries the rule to install"
    );
    let devices = app.world().resource::<SpaceMouseDevices>();
    assert_eq!(
        unavailable(devices),
        ["SpaceMouse Wireless", "SpaceMouse Compact"],
        "each refused device listed once, however often it is reported"
    );
    assert_eq!(devices.connected(), 0);
    assert!(
        app.world().resource::<SpaceMouseState>().is_idle(),
        "a refused device moves nothing"
    );

    // The rule is installed and the device replugged: it opens and stops being unavailable.
    fake.connect(9, "SpaceMouse Wireless");
    app.update();
    let devices = app.world().resource::<SpaceMouseDevices>();
    assert_eq!(devices.connected(), 1);
    assert_eq!(unavailable(devices), ["SpaceMouse Compact"]);
    assert_eq!(
        devices.devices[0].status,
        SpaceMouseDeviceStatus::Connected,
        "connected devices are listed first"
    );
    assert_eq!(
        PERMISSION_WARNINGS.load(Ordering::Relaxed),
        1,
        "still one warning"
    );
}
