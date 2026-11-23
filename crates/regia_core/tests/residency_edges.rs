// SPDX-License-Identifier: MPL-2.0
//! `residency` value types: the squeeze setting parser's edge inputs and the process-wide region
//! accounting (empty and zero-sized slices, overlap, drop order). No keeper thread is started.

use std::sync::Mutex;
use std::time::Duration;

use regia_core::residency::{ResidencySettings, ResidencyStats, ResidentRegion, SqueezeSettings};

/// The region registry is process-wide: tests that read its total must not interleave.
static REGISTRY_TESTS: Mutex<()> = Mutex::new(());

fn ms(text: &str) -> Option<Duration> {
    SqueezeSettings::parse(text).map(|s| s.period)
}

#[test]
fn squeeze_parse_accepts_the_documented_forms_in_any_order_after_the_size() {
    assert_eq!(ms("32"), Some(Duration::from_millis(250)));
    assert_eq!(ms("32:1000"), Some(Duration::from_millis(1000)));
    assert_eq!(
        ms("32:0"),
        Some(Duration::ZERO),
        "zero is parsed; the thread clamps it when it starts"
    );
    let both = SqueezeSettings::parse("32:file:500").expect("file before the period");
    assert_eq!(
        (both.mb, both.period, both.file_only),
        (32, Duration::from_millis(500), true)
    );
    let spaced = SqueezeSettings::parse(" 8 : 100 : file ").expect("spaces are trimmed");
    assert_eq!(
        (spaced.mb, spaced.period, spaced.file_only),
        (8, Duration::from_millis(100), true)
    );
    assert_eq!(SqueezeSettings::parse("0").map(|s| s.mb), Some(0));
    assert_eq!(SqueezeSettings::parse("4294967295").map(|s| s.mb), Some(u32::MAX));
}

#[test]
fn squeeze_parse_later_values_win_and_repeats_are_harmless() {
    assert_eq!(ms("32:100:200"), Some(Duration::from_millis(200)));
    let twice = SqueezeSettings::parse("32:file:file").expect("repeated flag");
    assert!(twice.file_only);
}

#[test]
fn squeeze_parse_rejects_malformed_input_without_panicking() {
    for text in [
        "",
        " ",
        ":",
        "::",
        "file",
        "32:",
        "32::file",
        "32:file:",
        "-1",
        "+",
        "1.5",
        "4294967296",
        "32:-5",
        "32:1e3",
        "32:1000:fast",
        "32:18446744073709551616",
        "０",
        "32:file:extra",
        "MB",
    ] {
        assert_eq!(SqueezeSettings::parse(text), None, "{text:?}");
    }
    // A plus sign is accepted by integer parsing: it is part of the number, not a mode.
    assert_eq!(SqueezeSettings::parse("+32").map(|s| s.mb), Some(32));
}

#[test]
fn residency_defaults_do_not_sweep_code_and_stats_start_at_zero() {
    let defaults = ResidencySettings::default();
    assert!(!defaults.code);
    assert_eq!(defaults.period, Duration::from_millis(500));
    assert_eq!(
        ResidencyStats::default(),
        ResidencyStats {
            data_bytes: 0,
            code_bytes: 0,
            pages: 0,
            sweeps: 0,
            restored_pages: 0,
            last_sweep_us: 0,
            max_sweep_us: 0,
        }
    );
}

#[test]
fn regions_register_their_byte_length_and_unregister_on_drop() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let base = ResidentRegion::registered_bytes();
    let a = vec![0u32; 100];
    let b = vec![0u8; 1000];
    let ra = ResidentRegion::of_slice(&a);
    assert_eq!(ResidentRegion::registered_bytes(), base + 400);
    let rb = ResidentRegion::of_slice(&b);
    assert_eq!(ResidentRegion::registered_bytes(), base + 1400);
    // The same memory twice counts twice and each handle removes only its own entry.
    let rb2 = ResidentRegion::of_slice(&b);
    assert_eq!(ResidentRegion::registered_bytes(), base + 2400);
    drop(rb);
    assert_eq!(ResidentRegion::registered_bytes(), base + 1400);
    drop(ra);
    assert_eq!(ResidentRegion::registered_bytes(), base + 1000);
    drop(rb2);
    assert_eq!(ResidentRegion::registered_bytes(), base);
}

#[test]
fn empty_and_zero_sized_slices_register_nothing() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let base = ResidentRegion::registered_bytes();
    let empty: [u64; 0] = [];
    let zst = [(); 16];
    let r1 = ResidentRegion::of_slice(&empty);
    let r2 = ResidentRegion::of_slice(&zst);
    assert_eq!(ResidentRegion::registered_bytes(), base);
    drop((r1, r2));
    assert_eq!(
        ResidentRegion::registered_bytes(),
        base,
        "dropping a null handle removes nothing else"
    );
    let keep = vec![1u16; 50];
    let held = ResidentRegion::of_slice(&keep);
    drop(ResidentRegion::of_slice(&empty));
    assert_eq!(
        ResidentRegion::registered_bytes(),
        base + 100,
        "an unrelated empty handle leaves it alone"
    );
    drop(held);
}

#[test]
fn regions_registered_from_many_threads_balance_out() {
    let _serial = REGISTRY_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let base = ResidentRegion::registered_bytes();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let data = vec![0u8; 64];
                for _ in 0..100 {
                    let region = ResidentRegion::of_slice(&data);
                    drop(region);
                }
            });
        }
    });
    assert_eq!(ResidentRegion::registered_bytes(), base);
}
