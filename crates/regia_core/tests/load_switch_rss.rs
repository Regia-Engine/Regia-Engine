// SPDX-License-Identifier: MPL-2.0
//! A level switch mid-load holds one map, not two (ADR-8027; DO27's MAP_SWITCH_MEM).
//!
//! DO27's rehearsal: a map switch dropped only the receiving end of the preload worker's channel,
//! so the old load ran to its end beside the new one and RSS went 4.7 -> 11.3 GB in 3 s. This test
//! plays the same switch twice with 512 MiB fake maps (64 MiB chunks, each written so its pages are
//! resident; above glibc's largest mmap threshold, so a freed chunk leaves the process):
//!
//! - the *control* switches the old way (drop the receiver, start the next load beside it): two
//!   maps are alive at once and the process peak grows by about two maps;
//! - [`LoadSwitch`] cancels the old load and starts the next one only after the old one is freed:
//!   one map at a time, and the peak grows by one map.
//!
//! The steps are paced by the test (no sleeps), so both runs do the same work in the same order.
//! The exact count of live chunks is checked on every OS; the process peak (`VmHWM`, reset through
//! `/proc/self/clear_refs`) on Linux and Android. This file holds one test, so no other test
//! allocates in the process while it measures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use regia_core::load_switch::{CancelToken, Cancelled, LoadSwitch, LoadSwitchStats, SwitchEvent};

const MIB: u64 = 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024 * 1024;
const MAP_CHUNKS: u64 = 8;
const MAP_BYTES: u64 = MAP_CHUNKS * CHUNK_BYTES as u64;
/// Steps the old load takes before the switch.
const BEFORE_SWITCH: u64 = 4;
const DEADLINE: Duration = Duration::from_secs(120);

/// Live chunks across both loads, and the most at once.
#[derive(Clone, Default)]
struct Live {
    now: Arc<AtomicU64>,
    peak: Arc<AtomicU64>,
}

impl Live {
    fn reset(&self) {
        self.peak.store(self.now.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

/// One chunk of a map's bank: every byte written, so every page is resident.
struct Chunk {
    _bytes: Vec<u8>,
    live: Live,
}

impl Chunk {
    fn new(live: &Live) -> Self {
        let bytes = vec![0x5a_u8; CHUNK_BYTES];
        let now = live.now.fetch_add(1, Ordering::SeqCst) + 1;
        live.peak.fetch_max(now, Ordering::SeqCst);
        Self {
            _bytes: bytes,
            live: live.clone(),
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        self.live.now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A fake map: a bank of chunks.
struct Map {
    _bank: Vec<Chunk>,
}

/// Grants load steps; `poke` wakes a waiting load without a grant so it re-reads its token.
#[derive(Clone, Default)]
struct Pacer {
    state: Arc<(Mutex<u64>, Condvar)>,
    done: Arc<(Mutex<u64>, Condvar)>,
}

impl Pacer {
    fn grant(&self, steps: u64) {
        let (lock, cv) = &*self.state;
        *lock.lock().expect("pacer") += steps;
        cv.notify_all();
    }

    fn poke(&self) {
        let (_, cv) = &*self.state;
        let _held = self.state.0.lock().expect("pacer");
        cv.notify_all();
    }

    /// Waits for step `index`: false once `token` reads cancelled.
    fn step(&self, index: u64, token: &CancelToken) -> bool {
        let (lock, cv) = &*self.state;
        let mut granted = lock.lock().expect("pacer");
        loop {
            if token.is_cancelled() {
                return false;
            }
            if *granted > index {
                return true;
            }
            granted = cv.wait(granted).expect("pacer");
        }
    }

    fn mark_done(&self) {
        let (lock, cv) = &*self.done;
        *lock.lock().expect("progress") += 1;
        cv.notify_all();
    }

    fn wait_done(&self, steps: u64) {
        let (lock, cv) = &*self.done;
        let guard = lock.lock().expect("progress");
        let (guard, timeout) = cv
            .wait_timeout_while(guard, DEADLINE, |done| *done < steps)
            .expect("progress");
        assert!(!timeout.timed_out(), "load stuck at {} steps", *guard);
    }

    /// A load of [`MAP_CHUNKS`] steps, one chunk each, that checks its token before every step.
    fn load(&self, live: &Live) -> impl FnOnce(&CancelToken) -> Result<Map, String> + Send + 'static {
        let pacer = self.clone();
        let live = live.clone();
        move |token: &CancelToken| {
            let mut bank = Vec::new();
            for index in 0..MAP_CHUNKS {
                if !pacer.step(index, token) {
                    return Err(Cancelled.into());
                }
                bank.push(Chunk::new(&live));
                pacer.mark_done();
            }
            Ok(Map { _bank: bank })
        }
    }
}

/// The process's resident bytes now, with the peak reset to it. `None` where the OS cannot.
fn reset_peak_rss() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // "5" resets the peak resident set (VmHWM) to the current one (proc(5), Linux 4.0+).
        std::fs::write("/proc/self/clear_refs", "5").ok()?;
        status_bytes("VmRSS:")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// The process's peak resident bytes since the last [`reset_peak_rss`].
fn peak_rss() -> Option<u64> {
    status_bytes("VmHWM:")
}

fn status_bytes(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix(key))?;
    let kib: u64 = line.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kib * 1024)
}

struct Outcome {
    peak_chunks: u64,
    rss_growth: Option<u64>,
    /// The switch's counters (`None` for the control).
    stats: Option<LoadSwitchStats>,
    /// Chunks still live after the switch freed everything.
    left: u64,
}

/// The switch the old way: the old load's receiver is dropped and the next load starts beside it.
fn control_switch() -> Outcome {
    let live = Live::default();
    let start = reset_peak_rss();
    live.reset();
    let (old, new) = (Pacer::default(), Pacer::default());
    let (tx, rx) = mpsc::channel();
    let load = old.load(&live);
    let old_thread = std::thread::spawn(move || {
        let _ = tx.send(load(&CancelToken::never()));
    });
    old.grant(BEFORE_SWITCH);
    old.wait_done(BEFORE_SWITCH);
    // The switch: forget the old load, start the new one.
    drop(rx);
    let load = new.load(&live);
    let new_thread = std::thread::spawn(move || load(&CancelToken::never()));
    old.poke();
    new.grant(MAP_CHUNKS);
    let current = new_thread.join().expect("new load").expect("new map");
    // The old load runs to its end beside the new map, then drops its own.
    old.grant(MAP_CHUNKS);
    old_thread.join().expect("old load");
    let rss_growth = start
        .zip(peak_rss())
        .map(|(start, peak)| peak.saturating_sub(start));
    drop(current);
    Outcome {
        peak_chunks: live.peak.load(Ordering::SeqCst),
        rss_growth,
        stats: None,
        left: live.now.load(Ordering::SeqCst),
    }
}

/// The same switch through [`LoadSwitch`].
fn switch_with_cancel() -> Outcome {
    let live = Live::default();
    let start = reset_peak_rss();
    live.reset();
    let (old, new) = (Pacer::default(), Pacer::default());
    let mut maps: LoadSwitch<Map> = LoadSwitch::new();
    maps.switch_to("old", old.load(&live));
    old.grant(BEFORE_SWITCH);
    old.wait_done(BEFORE_SWITCH);
    maps.switch_to("new", new.load(&live));
    old.poke();
    new.grant(MAP_CHUNKS);
    match maps.wait(DEADLINE) {
        Some(SwitchEvent::Loaded { name }) => assert_eq!(name, "new"),
        other => panic!("the new map did not land: {other:?}"),
    }
    // The same late grant as the control: the old load is gone, so it takes nothing.
    old.grant(MAP_CHUNKS);
    assert!(maps.wait_retired(DEADLINE), "the old load was never freed");
    let rss_growth = start
        .zip(peak_rss())
        .map(|(start, peak)| peak.saturating_sub(start));
    let stats = maps.stats();
    maps.unload();
    let freed = maps.wait_retired(DEADLINE);
    assert!(freed, "the last map was never freed");
    Outcome {
        peak_chunks: live.peak.load(Ordering::SeqCst),
        rss_growth,
        stats: Some(stats),
        left: live.now.load(Ordering::SeqCst),
    }
}

#[test]
fn switch_cancels_old_load_rss_bounded() {
    // One map, plus one chunk and 64 MiB for thread stacks and allocator slack.
    let bound = MAP_BYTES + CHUNK_BYTES as u64 + 64 * MIB;
    let control = control_switch();
    let switched = switch_with_cancel();
    eprintln!(
        "map {} MiB; control: {} chunks live at peak, RSS growth {:?} MiB; switch: {} chunks, {:?} MiB; bound {} MiB",
        MAP_BYTES / MIB,
        control.peak_chunks,
        control.rss_growth.map(|b| b / MIB),
        switched.peak_chunks,
        switched.rss_growth.map(|b| b / MIB),
        bound / MIB,
    );
    // The control shows what the test catches: both maps whole at once.
    assert_eq!(
        control.peak_chunks,
        2 * MAP_CHUNKS,
        "the control did not overlap the loads"
    );
    if let Some(growth) = control.rss_growth {
        assert!(
            growth > bound,
            "the RSS probe cannot see two maps: {} MiB",
            growth / MIB
        );
    }
    assert_eq!(
        switched.peak_chunks, MAP_CHUNKS,
        "the switch held more than one map at once"
    );
    if let Some(growth) = switched.rss_growth {
        assert!(
            growth <= bound,
            "RSS grew {} MiB over a switch, more than one map ({} MiB)",
            growth / MIB,
            bound / MIB
        );
    }
    let stats = switched.stats.unwrap_or_default();
    assert_eq!(stats.loads_cancelled, 1, "{stats:?}");
    assert_eq!(stats.loads_landed, 1, "{stats:?}");
    assert_eq!(switched.left, 0, "a map was never freed");
}
