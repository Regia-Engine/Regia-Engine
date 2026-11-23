// SPDX-License-Identifier: MPL-2.0
//! Deterministic edge cases for the hot-reload debouncer and the scratch-file filter
//! (`dev-assets` feature). Time is a fixed base `Instant` plus offsets; nothing sleeps.
#![cfg(all(feature = "dev-assets", not(target_os = "android")))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_asset::hot_reload::{PathDebouncer, relevant_path};
use regia_test_utils::unique_temp_dir;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A path is ready exactly when `quiet` has passed since its last event (boundary included), not
/// one tick earlier.
#[test]
fn debouncer_boundary_is_inclusive() {
    let t0 = Instant::now();
    let mut d = PathDebouncer::new(ms(100));
    d.poke(PathBuf::from("a"), t0);
    assert!(d.take_ready(t0 + ms(99)).is_empty());
    assert!(!d.is_idle());
    let ready = d.take_ready(t0 + ms(100));
    assert_eq!(ready, vec![(PathBuf::from("a"), t0)]);
    assert!(d.is_idle());
    assert!(d.take_ready(t0 + ms(1000)).is_empty(), "a released path is gone");
}

/// A zero quiet time releases on the same instant; an empty debouncer has nothing due.
#[test]
fn debouncer_zero_quiet_and_empty() {
    let t0 = Instant::now();
    let mut d = PathDebouncer::new(Duration::ZERO);
    assert_eq!(d.next_due(t0), None);
    assert!(d.take_ready(t0).is_empty());
    d.poke(PathBuf::from("a"), t0);
    assert_eq!(d.next_due(t0), Some(Duration::ZERO));
    assert_eq!(d.take_ready(t0).len(), 1);
}

/// Each new event restarts the timer, the released entry reports the *first* event's instant, and
/// the next burst on the same path starts a fresh first-seen.
#[test]
fn debouncer_restarts_and_reports_first_event() {
    let t0 = Instant::now();
    let mut d = PathDebouncer::new(ms(100));
    d.poke(PathBuf::from("a"), t0);
    d.poke(PathBuf::from("a"), t0 + ms(60));
    d.poke(PathBuf::from("a"), t0 + ms(120));
    assert!(d.take_ready(t0 + ms(200)).is_empty(), "restarted at 120");
    assert_eq!(d.next_due(t0 + ms(150)), Some(ms(70)));
    let ready = d.take_ready(t0 + ms(220));
    assert_eq!(ready, vec![(PathBuf::from("a"), t0)]);
    d.poke(PathBuf::from("a"), t0 + ms(500));
    let ready = d.take_ready(t0 + ms(600));
    assert_eq!(ready, vec![(PathBuf::from("a"), t0 + ms(500))]);
}

/// Several ready paths come back sorted; paths still inside their quiet time stay pending, and
/// `next_due` is the soonest of them.
#[test]
fn debouncer_sorts_ready_paths_and_keeps_the_rest() {
    let t0 = Instant::now();
    let mut d = PathDebouncer::new(ms(50));
    d.poke(PathBuf::from("c"), t0);
    d.poke(PathBuf::from("a"), t0 + ms(10));
    d.poke(PathBuf::from("b"), t0 + ms(80));
    let ready = d.take_ready(t0 + ms(70));
    let names: Vec<_> = ready.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(names, vec![PathBuf::from("a"), PathBuf::from("c")]);
    assert_eq!(d.next_due(t0 + ms(70)), Some(ms(60)));
    assert!(!d.is_idle());
}

/// `take_ready` with a clock behind a poke (a stale `now`) saturates instead of panicking.
#[test]
fn debouncer_tolerates_a_clock_behind_the_event() {
    let t0 = Instant::now();
    let mut d = PathDebouncer::new(ms(10));
    d.poke(PathBuf::from("a"), t0 + ms(500));
    assert!(d.take_ready(t0).is_empty());
    assert_eq!(d.next_due(t0), Some(ms(510)));
}

#[test]
fn scratch_and_odd_names_are_filtered() {
    let dir = unique_temp_dir("tq-relevant");
    let file = dir.join("a.glb");
    std::fs::write(&file, b"x").expect("write");
    assert_eq!(relevant_path(&file), Some(file.clone()));
    // Directories are never content.
    assert_eq!(relevant_path(&dir), None);
    // `.meta` sidecars pass through (the live cooker maps them to their source).
    let meta = dir.join("a.glb.meta");
    assert_eq!(relevant_path(&meta), Some(meta.clone()));
    for name in [
        ".#a.glb",
        "~a.glb",
        "a.glb~",
        "a.glb.swp",
        "a.glb.swx",
        "a.glb.tmp",
        "a.glb.part",
        ".regia-tmp-1",
        ".ferrite-tmp-1",
    ] {
        assert_eq!(relevant_path(&dir.join(name)), None, "{name}");
    }
    // Names that only resemble scratch files are kept.
    for name in ["regia-tmp", ".regia", ".regiatmp", "tmp", "a.tmpl", "swp"] {
        assert!(relevant_path(&dir.join(name)).is_some(), "{name}");
    }
    // No file name at all (a root or a trailing `..`).
    assert_eq!(relevant_path(Path::new("/")), None);
    assert_eq!(relevant_path(Path::new("..")), None);
    let _ = std::fs::remove_dir_all(&dir);
}
