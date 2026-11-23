// SPDX-License-Identifier: MPL-2.0
//! Determinism regressions for the SpaceMouse and touch layers (audit findings 3, 5, 7, 8,
//! ADR-4768). Everything here drives the public API with fixed inputs; no wall clock, no GPU.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_input::GestureEvent;
use regia_input::prelude::*;
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
use regia_input::spacemouse::report::DeviceDecoder;
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
use regia_input::spacemouse::{
    FakeSpaceMouse, NavContext, SpaceMouseButton, SpaceMouseNav, SpaceMousePlugin, SpaceMouseState, nav,
};
use regia_platform::TouchPhase;

/// Audit finding 3 (ADR-4768): gesture classification must not depend on the session's absolute
/// gameplay time. The old code subtracted two `Time::elapsed_secs()` values; at elapsed 32820 s
/// the f32 granularity is ~4 ms, and a 299 ms touch reads as ~300.8 ms — just past
/// `TAP_MAX_SECS` — so the same touch stopped being a tap hours into a session (and a replay
/// starting at 0 classified it the other way).
#[test]
fn tap_classification_is_independent_of_session_age() {
    // 1 ms frames: the touch is down for 299 updates, comfortably inside the tap window, but
    // an absolute-time subtraction at elapsed 32820 s rounds the end up by more than the 1 ms
    // margin (f32 ulp at 2^15 is ~3.9 ms).
    let tap_at = |elapsed_secs: u64| {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(1)));
        app.add_plugins(InputPlugin::default());
        app.world_mut().resource_mut::<Time>().set_clocks(
            Duration::from_secs(elapsed_secs),
            Duration::from_secs(elapsed_secs),
        );
        send_touch(&mut app, 1, TouchPhase::Started, (50.0, 50.0));
        app.update();
        for _ in 0..299 {
            app.update();
        }
        send_touch(&mut app, 1, TouchPhase::Ended, (50.0, 50.0));
        app.update();
        app.world_mut()
            .resource_mut::<Messages<GestureEvent>>()
            .drain()
            .collect::<Vec<_>>()
    };
    let fresh = tap_at(0);
    assert_eq!(
        fresh,
        vec![GestureEvent::Tap {
            position: Vec2::new(50.0, 50.0)
        }],
        "a 299 ms still touch is a tap at the start of a session"
    );
    assert_eq!(
        tap_at(32820),
        fresh,
        "the same touch classifies the same deep into a session"
    );
}

fn send_touch(app: &mut App, id: u64, phase: TouchPhase, position: (f64, f64)) {
    app.world_mut().write_message(regia_platform::TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// A minimal split-report descriptor (the classic wired shape): report 1 `X, Y, Z`, report 2
/// `Rx, Ry, Rz` (16-bit), report 3 two buttons + 6 constant padding bits. Same shape as the
/// in-module `SPLIT_DESCRIPTOR`, kept inline so this suite stands alone.
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
const SPLIT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x08, // Usage (Multi-axis Controller)
    0xA1, 0x01, // Collection (Application)
    0xA1, 0x00, //   Collection (Physical)
    0x85, 0x01, //     Report ID 1
    0x16, 0xA2, 0xFE, // Logical Minimum (-350)
    0x26, 0x5E, 0x01, // Logical Maximum (350)
    0x09, 0x30, 0x09, 0x31, 0x09, 0x32, // Usage X, Y, Z
    0x75, 0x10, 0x95, 0x03, // Report Size 16, Count 3
    0x81, 0x02, // Input (Data, Var, Abs)
    0xC0, //   End Collection
    0xA1, 0x00, //   Collection (Physical)
    0x85, 0x02, //     Report ID 2
    0x09, 0x33, 0x09, 0x34, 0x09, 0x35, // Usage Rx, Ry, Rz
    0x75, 0x10, 0x95, 0x03, 0x81, 0x02, // Input 3 x 16 bit
    0xC0, //   End Collection
    0xA1, 0x02, //   Collection (Logical)
    0x85, 0x03, //     Report ID 3
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, 0x29, 0x02, // Usage Minimum 1, Maximum 2
    0x15, 0x00, 0x25, 0x01, // Logical 0..1
    0x75, 0x01, 0x95, 0x02, 0x81, 0x02, // Input 2 x 1 bit
    0x95, 0x06, 0x81, 0x03, // Input 6 x 1 bit constant (padding)
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// Audit finding 5 (ADR-4768): on a host without a report descriptor the known layout must
/// agree with the descriptor's decode — constant padding bits are not buttons, so `[3, 0x04]`
/// (only bit 2 set) leaves `buttons == 0` while `[3, 0x01]` sets button 1. Before the fix the
/// descriptor-less layout mapped all 32 bits of report 3 as buttons, and a set padding bit
/// read as "button 3" — which the plugin toggles dominant-axis mode with.
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
#[test]
fn known_layout_ignores_padding_bits_in_the_button_byte() {
    // The descriptor-less case runs as the Compact, a two-button split product whose padding
    // bits the known layout must keep rejecting.
    for (descriptor, product) in [(Some(SPLIT_DESCRIPTOR), 0), (None, 0xC635)] {
        // Only bit 2 set: constant padding on this layout, so the report changes nothing and
        // no button reads as pressed.
        let mut decoder = DeviceDecoder::new(descriptor, product);
        decoder.feed(&[3, 0x04]);
        assert_eq!(
            decoder.state().buttons,
            0,
            "{descriptor:?}: bit 2 is constant padding, not button 3"
        );
        let mut decoder = DeviceDecoder::new(descriptor, product);
        assert!(decoder.feed(&[3, 0x01]));
        assert_eq!(
            decoder.state().buttons,
            0b01,
            "{descriptor:?}: button 1 still decodes"
        );
        // A padding bit set alongside a real button must not add a button.
        decoder.feed(&[3, 0x05]);
        assert_eq!(
            decoder.state().buttons,
            0b01,
            "{descriptor:?}: bit 2 stays padding next to a live button 1"
        );
    }
}

/// Audit finding 7 (ADR-4768): the shaping and the zoom multiplier are bit-identical across
/// targets — the quadratic curve is one multiply and the zoom uses the deterministic exp,
/// never the platform libm (`powf`/`expf` are not required to agree in the last bits between
/// glibc, UCRT and bionic).
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
#[test]
fn nav_shaping_and_zoom_are_bit_identical() {
    let n = 0.52_f32;
    let t = ((n.abs() - 0.04) / (1.0 - 0.04)).max(0.0);
    assert_eq!(nav::shape_axis(n), t * t, "the curve is sign · t · t exactly");
    assert_eq!(nav::shape_axis(-n), -(t * t));
    assert_eq!(nav::shape_axis(1.0), 1.0);

    // Full +Z push for one 60 Hz frame in an orbit viewport: the zoom multiplier is a pinned
    // bit pattern (change the literal only if the deterministic exp changes deliberately),
    // tracked against the true value.
    let pushed = SpaceMouseState {
        translation: [0.0, 0.0, 1.0],
        ..SpaceMouseState::default()
    };
    let delta = nav::step(&pushed, 1.0 / 60.0, NavContext::orbit(10.0));
    assert_eq!(delta.zoom.to_bits(), 0x3f833d87);
    let truth = (1.5_f64 / 60.0).exp() as f32;
    assert!(
        (delta.zoom - truth).abs() < 1e-6,
        "zoom tracks the true value: {} vs {truth}",
        delta.zoom
    );
}

/// Audit finding 8 (ADR-4768), refined by the fixrev review (finding 5): with two devices,
/// the merged button edges and toggles must not depend on which device's samples the backend
/// delivers first. Both orders press button 3 on both devices inside one poll: the merged
/// press edge and the single merged rising edge are identical either way, so dominant-axis
/// mode toggles once and ends on in both.
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
#[test]
fn merged_button_toggles_do_not_depend_on_delivery_order() {
    let run = |b_first: bool| {
        let fake = FakeSpaceMouse::default();
        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
        app.add_plugins(InputPlugin::default());
        fake.connect(1, "SpaceMouse A");
        fake.connect(2, "SpaceMouse B");
        app.update();
        let _ = app
            .world_mut()
            .resource_mut::<Messages<SpaceMouseButton>>()
            .drain()
            .collect::<Vec<_>>();
        let held = SpaceMouseState {
            buttons: 0b100,
            ..SpaceMouseState::default()
        };
        let released = SpaceMouseState::default();
        if b_first {
            fake.sample(2, held);
            fake.sample(2, released);
            fake.sample(1, held);
        } else {
            fake.sample(1, held);
            fake.sample(2, held);
            fake.sample(2, released);
        }
        app.update();
        let presses = app
            .world_mut()
            .resource_mut::<Messages<SpaceMouseButton>>()
            .drain()
            .filter(|b: &SpaceMouseButton| b.pressed)
            .count();
        (presses, app.world().resource::<SpaceMouseNav>().dominant)
    };
    let (a_presses, a_dominant) = run(false);
    let (b_presses, b_dominant) = run(true);
    assert_eq!(
        a_presses, 1,
        "both devices pressed button 3 in one poll: one merged press edge"
    );
    assert_eq!(
        b_presses, a_presses,
        "the press count does not depend on delivery order"
    );
    assert!(
        a_dominant && b_dominant,
        "one merged rising edge toggles dominant mode on either way"
    );
}

/// Fixrev review finding 5: `SpaceMouseButton` edges describe the merged
/// [`SpaceMouseState::buttons`]. One device holding a button while another taps it inside one
/// poll must send the tap's press but no release — the merged button never went up — and the
/// toggle must not flip for a merged rising edge that never occurred.
#[cfg(all(feature = "devices", not(target_os = "android")))] // the SpaceMouse module (spec 29 § 1)
#[test]
fn a_tap_on_another_devices_button_never_contradicts_the_merge() {
    let fake = FakeSpaceMouse::default();
    let mut app = App::new();
    app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
    app.add_plugins(InputPlugin::default());
    fake.connect(1, "SpaceMouse A");
    fake.connect(2, "SpaceMouse B");
    app.update();
    let drain_buttons = |app: &mut App| -> Vec<(u8, bool)> {
        app.world_mut()
            .resource_mut::<Messages<SpaceMouseButton>>()
            .drain()
            .map(|b| (b.button, b.pressed))
            .collect()
    };
    let _ = drain_buttons(&mut app);

    // Device A presses and holds button 3 (the dominant-axis toggle).
    let held = SpaceMouseState {
        buttons: 0b100,
        ..SpaceMouseState::default()
    };
    fake.sample(1, held);
    app.update();
    assert_eq!(
        drain_buttons(&mut app),
        vec![(3, true)],
        "A's press is the merged rising edge"
    );
    assert!(app.world().resource::<SpaceMouseNav>().dominant, "toggled on");

    // Device B taps the same button inside one poll while A keeps holding it.
    fake.sample(2, held);
    fake.sample(2, SpaceMouseState::default());
    app.update();
    assert_eq!(
        drain_buttons(&mut app),
        vec![(3, true)],
        "the tap's press arrives, but no release: the merged button never went up"
    );
    assert_eq!(
        app.world().resource::<SpaceMouseState>().buttons,
        0b100,
        "the merged state still holds the button"
    );
    assert!(
        app.world().resource::<SpaceMouseNav>().dominant,
        "no toggle without a merged rising edge"
    );

    // Only when the last holder releases does the release edge fire.
    fake.sample(1, SpaceMouseState::default());
    app.update();
    assert_eq!(drain_buttons(&mut app), vec![(3, false)]);
    assert!(
        app.world().resource::<SpaceMouseNav>().dominant,
        "the release does not toggle either"
    );
}
