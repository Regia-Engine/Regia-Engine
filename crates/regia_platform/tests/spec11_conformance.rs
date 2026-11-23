// SPDX-License-Identifier: MPL-2.0
//! Spec 11 conformance (`docs/specs/11-platform-and-input.md`, ADR-5610): the window and lifecycle
//! vocabulary the spec lists, and the Linux sleep threshold at its exact boundary. Exhaustive
//! matches make an added or removed variant a compile error here, so the spec and the enums cannot
//! drift apart silently.

use std::time::Duration;

use regia_platform::sleep::{ClockReading, MIN_REPORTED_SLEEP, SleepDetector};
use regia_platform::{CursorGrab, LifecycleEvent, PresentMode, Window, WindowMode};

/// § 1: `WindowMode { Windowed, BorderlessFullscreen }` (no exclusive fullscreen), `present_mode`
/// `Vsync` (the default), `VsyncRelaxed`, `Immediate`, and `cursor_grab: None|Confined|Locked`.
#[test]
fn window_enums_are_the_spec_sets_with_the_spec_defaults() {
    for mode in [WindowMode::Windowed, WindowMode::BorderlessFullscreen] {
        match mode {
            WindowMode::Windowed | WindowMode::BorderlessFullscreen => {}
        }
    }
    for present in [
        PresentMode::Vsync,
        PresentMode::VsyncRelaxed,
        PresentMode::Immediate,
    ] {
        match present {
            PresentMode::Vsync | PresentMode::VsyncRelaxed | PresentMode::Immediate => {}
        }
    }
    for grab in [CursorGrab::None, CursorGrab::Confined, CursorGrab::Locked] {
        match grab {
            CursorGrab::None | CursorGrab::Confined | CursorGrab::Locked => {}
        }
    }
    assert_eq!(PresentMode::default(), PresentMode::Vsync);
    assert_eq!(WindowMode::default(), WindowMode::Windowed);
    assert_eq!(CursorGrab::default(), CursorGrab::None);
}

/// § 1: the `Window` component's fields, and the defaults a fresh window starts from. The struct
/// literal names every field, so a new field (a position, a minimum size) fails to compile here.
#[test]
fn window_fields_are_the_spec_list() {
    let w = Window::default();
    let Window {
        title,
        logical_size,
        mode,
        present_mode,
        resizable,
        decorations,
        cursor_visible,
        cursor_grab,
        ime_enabled,
        app_id,
        physical_size,
        scale_factor,
    } = w;
    assert!(!title.is_empty());
    assert!(logical_size.0 > 0.0 && logical_size.1 > 0.0);
    assert_eq!(mode, WindowMode::Windowed);
    assert_eq!(present_mode, PresentMode::Vsync);
    assert!(resizable && decorations && cursor_visible);
    assert_eq!(cursor_grab, CursorGrab::None);
    assert!(!ime_enabled);
    assert_eq!(app_id, None, "None means regia.<executable name>");
    assert_eq!(
        physical_size,
        (0, 0),
        "read back from the OS on create, not chosen"
    );
    assert_eq!(scale_factor, 1.0);
}

/// § 2: `LifecycleEvent` is exactly `Started`, `Suspended`, `Resumed`, `WillTerminate`, `LowMemory`.
#[test]
fn lifecycle_events_are_the_spec_set() {
    let all = [
        LifecycleEvent::Started,
        LifecycleEvent::Suspended,
        LifecycleEvent::Resumed,
        LifecycleEvent::WillTerminate,
        LifecycleEvent::LowMemory,
    ];
    for e in all {
        match e {
            LifecycleEvent::Started
            | LifecycleEvent::Suspended
            | LifecycleEvent::Resumed
            | LifecycleEvent::WillTerminate
            | LifecycleEvent::LowMemory => {}
        }
    }
}

fn reading(awake_ms: u64, boot_ms: u64) -> ClockReading {
    ClockReading {
        awake: Duration::from_millis(awake_ms),
        boot: Duration::from_millis(boot_ms),
    }
}

/// § 2 (ADR-213): a `CLOCK_BOOTTIME` vs `CLOCK_MONOTONIC` gap of 2 s **or more** is a sleep; one
/// millisecond less is not.
#[test]
fn a_sleep_gap_of_exactly_two_seconds_is_reported_and_one_ms_less_is_not() {
    assert_eq!(MIN_REPORTED_SLEEP, Duration::from_secs(2));

    let mut d = SleepDetector::default();
    assert_eq!(d.observe(reading(1_000, 1_000)), None, "first reading");
    // 16 ms awake, 2,016 ms of boot time: exactly 2 s slept.
    assert_eq!(d.observe(reading(1_016, 3_016)), Some(Duration::from_secs(2)));
    assert_eq!(d.sleeps, 1);

    let mut d = SleepDetector::default();
    d.observe(reading(1_000, 1_000));
    assert_eq!(d.observe(reading(1_016, 3_015)), None, "1,999 ms is jitter");
    assert_eq!(d.sleeps, 0);
    assert_eq!(d.total_slept, Duration::ZERO);
}
