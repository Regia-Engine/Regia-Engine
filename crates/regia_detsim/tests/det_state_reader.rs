// SPDX-License-Identifier: MPL-2.0
//! The det-state reader under hostile input (DC-3.7a): every refusal has its code, 20,000 seeded
//! mutations of real snapshots never panic, and a load never allocates past a bound set by the
//! input's length (a forged length prefix sizes no allocation by itself). A counting global
//! allocator tracks this thread's live bytes and their peak; its own test binary, because of the
//! global allocator (the pattern of `hash_alloc.rs`).

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the
// `thread_local!` below whose initializers are already `const { }` (the same false positive
// `hash_alloc.rs` records, ADR-8047). `expect` fails the build once it stops.
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_detsim::arena::{ArenaScenario, ArenaSettings};
use regia_detsim::float::det_max_f64;
use regia_detsim::golden::scenarios::{CollideScenario, SplitMix64, SteerScenario};
use regia_detsim::golden::{DetRun, TickRate};
use regia_detsim::harness::SimHarness;
use regia_detsim::hash::{SystemHashes, SystemRing};
use regia_detsim::ids::DetIdAllocator;
use regia_detsim::state::{
    self, DetReadLimits, DetReader, DetState, DetStateError, DetWriter, HEADER_LEN, SnapshotKind,
};
use serde::{Deserialize, Serialize};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note(delta: i64, counts: bool) {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    if !ARMED.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
    if counts {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: forwards every call to the system allocator unchanged; the only addition is thread-local
// counter updates, which neither allocate nor touch the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as i64, true);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as i64, true);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size as i64 - layout.size() as i64, true);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(-(layout.size() as i64), false);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Peak live bytes and allocation count on this thread while `f` runs (what `f` keeps counts).
fn measured<R>(f: impl FnOnce() -> R) -> (i64, u64, R) {
    LIVE.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (PEAK.with(Cell::get), COUNT.with(Cell::get), out)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Order {
    Hold,
    Move { to: [f32; 2] },
    Attack(u32),
}

/// A serde-derived game state: the codec path a game takes instead of hand-written fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Squad {
    name: String,
    members: Vec<u32>,
    orders: Vec<Order>,
    morale: BTreeMap<u32, f32>,
    leader: Option<u32>,
    payload: Vec<u8>,
}

impl DetState for Squad {
    const STATE_TAG: [u8; 8] = *b"squad\0\0\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.put(self);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        r.take()
    }
}

fn squad() -> Squad {
    Squad {
        name: "north".into(),
        members: (1..=40).collect(),
        orders: vec![Order::Hold, Order::Move { to: [3.0, -0.0] }, Order::Attack(7)],
        morale: (0..12).map(|i| (i, i as f32 * 0.25)).collect(),
        leader: Some(3),
        payload: (0..200).map(|i| i as u8).collect(),
    }
}

/// One loader per snapshot kind; each returns the error code or `"ok"`.
type Loader = fn(&[u8], DetReadLimits) -> &'static str;

fn code<T>(r: Result<T, DetStateError>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(e) => e.code(),
    }
}

fn load_collide(b: &[u8], l: DetReadLimits) -> &'static str {
    code(SimHarness::<CollideScenario>::resume_with(b, l))
}
fn load_steer(b: &[u8], l: DetReadLimits) -> &'static str {
    code(SimHarness::<SteerScenario>::resume_with(b, l))
}
fn load_arena(b: &[u8], l: DetReadLimits) -> &'static str {
    code(SimHarness::<ArenaScenario>::resume_with(b, l))
}
fn load_ids(b: &[u8], l: DetReadLimits) -> &'static str {
    code(state::restore_with::<DetIdAllocator<u16>>(b, l))
}
fn load_ring(b: &[u8], l: DetReadLimits) -> &'static str {
    code(state::restore_with::<SystemRing<4>>(b, l))
}
fn load_squad(b: &[u8], l: DetReadLimits) -> &'static str {
    code(state::restore_with::<Squad>(b, l))
}

/// Real snapshots of every engine kind, each with its loader and its frame kind.
fn corpus() -> Vec<(&'static str, Vec<u8>, Loader)> {
    let rate = TickRate::DEFAULT;
    let mut collide = SimHarness::new(CollideScenario::new(), DetRun::new(3, 200, rate)).expect("collide");
    collide.step_n(90).expect("step");
    let mut steer = SimHarness::new(SteerScenario::new(), DetRun::new(5, 200, rate)).expect("steer");
    steer.step_n(45).expect("step");
    let arena = ArenaScenario::new(ArenaSettings::default()).expect("settings");
    let mut arena = SimHarness::new(arena, DetRun::new(100, 300, rate)).expect("arena");
    arena.step_n(120).expect("step");
    let mut ids = DetIdAllocator::<u16>::new(300);
    let mut rng = SplitMix64::new(9);
    for _ in 0..400 {
        if rng.next_u64().is_multiple_of(3) {
            let id = (rng.next_u64() % 300) as u16 + 1;
            ids.free(id);
        } else {
            let _ = ids.alloc();
        }
    }
    let mut ring = SystemRing::<4>::new();
    for t in 0..30 {
        ring.push(
            t,
            SystemHashes {
                words: [t, t * 3, t * 5, !t],
            },
        );
    }
    vec![
        ("collide", collide.save().expect("save"), load_collide as Loader),
        ("steer", steer.save().expect("save"), load_steer as Loader),
        ("arena", arena.save().expect("save"), load_arena as Loader),
        ("ids", state::snapshot(&ids, 1).expect("save"), load_ids as Loader),
        (
            "ring",
            state::snapshot(&ring, 2).expect("save"),
            load_ring as Loader,
        ),
        (
            "squad",
            state::snapshot(&squad(), 3).expect("save"),
            load_squad as Loader,
        ),
    ]
}

/// Re-frame a mutated body so the integrity hash passes and the loader itself is exercised.
fn reframe(original: &[u8], body: &[u8]) -> Vec<u8> {
    let header = state::read_header(original, DetReadLimits::DEFAULT).expect("header");
    state::frame_bytes(header.kind, header.tag, header.version, header.tick, body).expect("frame")
}

fn mutate(rng: &mut SplitMix64, bytes: &mut Vec<u8>) {
    let len = bytes.len().max(1) as u64;
    match rng.next_u64() % 6 {
        0 => {
            let i = (rng.next_u64() % len) as usize;
            if let Some(b) = bytes.get_mut(i) {
                *b ^= 1 << (rng.next_u64() % 8);
            }
        }
        1 => {
            let i = (rng.next_u64() % len) as usize;
            if let Some(b) = bytes.get_mut(i) {
                *b = rng.next_u64() as u8;
            }
        }
        2 => {
            let cut = (rng.next_u64() % len) as usize;
            bytes.truncate(cut);
        }
        3 => {
            let i = (rng.next_u64() % len) as usize;
            let n = 1 + (rng.next_u64() % 8) as usize;
            for _ in 0..n {
                bytes.insert(i.min(bytes.len()), rng.next_u64() as u8);
            }
        }
        4 => {
            // A forged length: a large u32 at a random 4-byte position.
            let i = (rng.next_u64() % len) as usize;
            let forged = match rng.next_u64() % 3 {
                0 => u32::MAX,
                1 => 1 << 24,
                _ => (rng.next_u64() % 100_000) as u32,
            };
            for (k, b) in forged.to_le_bytes().into_iter().enumerate() {
                if let Some(slot) = bytes.get_mut(i + k) {
                    *slot = b;
                }
            }
        }
        _ => {
            let i = (rng.next_u64() % len) as usize;
            let j = (rng.next_u64() % len) as usize;
            if i < bytes.len() && j < bytes.len() {
                bytes.swap(i, j);
            }
        }
    }
}

#[test]
fn reader_refuses_bad_input() {
    let limits = DetReadLimits::DEFAULT;
    let corpus = corpus();
    for (name, bytes, load) in &corpus {
        assert_eq!(load(bytes, limits), "ok", "{name}: the clean snapshot loads");
        // Truncated anywhere: refused.
        for cut in [0, 3, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 1, bytes.len() - 1] {
            assert_eq!(load(&bytes[..cut], limits), "truncated", "{name}: cut at {cut}");
        }
        // Trailing bytes.
        let mut long = bytes.clone();
        long.extend_from_slice(&[0, 0]);
        assert_eq!(load(&long, limits), "trailing_bytes", "{name}");
        // Wrong tag (a corrupted tag byte, re-hashed so only the tag is wrong).
        let header = state::read_header(bytes, limits).expect("header");
        let mut tag = header.tag;
        tag[0] ^= 0x20;
        let other = state::frame_bytes(
            header.kind,
            tag,
            header.version,
            header.tick,
            &bytes[HEADER_LEN..],
        )
        .expect("frame");
        assert_eq!(load(&other, limits), "bad_tag", "{name}");
        // Wrong version.
        let old = state::frame_bytes(
            header.kind,
            header.tag,
            header.version + 1,
            header.tick,
            &bytes[HEADER_LEN..],
        )
        .expect("frame");
        assert_eq!(load(&old, limits), "bad_version", "{name}");
        // Oversized input.
        let tight = DetReadLimits {
            max_bytes: bytes.len() - 1,
            ..limits
        };
        assert_eq!(load(bytes, tight), "length_limit", "{name}");
        // A length over max_len inside the body (the first length prefix is past the fixed head of
        // each body; a tight max_len catches whichever comes first).
        let short = DetReadLimits { max_len: 2, ..limits };
        assert_eq!(load(bytes, short), "length_limit", "{name}: max_len 2");
    }

    // Input over the limit is refused before anything is allocated.
    let (_, count, result) = measured(|| {
        state::restore_with::<Squad>(
            &corpus[5].1,
            DetReadLimits {
                max_bytes: 8,
                max_len: 8,
            },
        )
    });
    assert_eq!(result.err().map(|e| e.code()), Some("length_limit"));
    assert_eq!(count, 0, "a refused oversized input allocates nothing");

    // 20,000 seeded mutations: never a panic; every load's peak allocation stays under
    // 32 bytes per input byte plus 64 KiB (sequences preallocate at most 4 KiB before their
    // elements arrive; a bit table is at most 8 bytes per packed input byte).
    let mut rng = SplitMix64::new(0xde75_7a7e);
    let mut outcomes: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut worst = 0f64;
    for i in 0..20_000u32 {
        let (name, original, load) = &corpus[i as usize % corpus.len()];
        let mut bytes = original.clone();
        let reframed = rng.next_u64().is_multiple_of(2);
        if reframed {
            let mut body = original[HEADER_LEN..].to_vec();
            let rounds = 1 + rng.next_u64() % 3;
            for _ in 0..rounds {
                mutate(&mut rng, &mut body);
            }
            bytes = reframe(original, &body);
        } else {
            mutate(&mut rng, &mut bytes);
        }
        let (peak, _, result) = measured(|| catch_unwind(AssertUnwindSafe(|| load(&bytes, limits))));
        let Ok(outcome) = result else {
            panic!("{name}: mutation {i} panicked (reframed {reframed})");
        };
        let bound = 32 * bytes.len() as i64 + 64 * 1024;
        assert!(
            peak <= bound,
            "{name}: mutation {i} peaked at {peak} bytes for {} input bytes",
            bytes.len()
        );
        worst = regia_detsim::float::det_max_f64(worst, peak as f64 / bytes.len().max(1) as f64);
        worst = det_max_f64(worst, peak as f64 / bytes.len().max(1) as f64);
        *outcomes.entry(outcome).or_default() += 1;
    }
    // The mutations reach the loaders, not only the frame check.
    for expected in ["bad_hash", "truncated", "bad_value", "ok"] {
        assert!(
            outcomes.get(expected).copied().unwrap_or(0) > 0,
            "no {expected} outcome: {outcomes:?}"
        );
    }
    println!("det-state mutation outcomes: {outcomes:?}; worst peak {worst:.1} bytes per input byte");
}

#[test]
fn a_frame_of_unknown_kind_is_refused() {
    let squad_bytes = state::snapshot(&squad(), 0).expect("save");
    let mut bad = squad_bytes.clone();
    bad[6] = 7;
    assert_eq!(
        state::read_header(&bad, DetReadLimits::DEFAULT)
            .err()
            .map(|e| e.code()),
        Some("bad_value")
    );
    let header = state::read_header(&squad_bytes, DetReadLimits::DEFAULT).expect("header");
    assert_eq!(header.kind, SnapshotKind::State);
    let (_, back): (u64, Squad) = state::restore(&squad_bytes).expect("restore");
    assert_eq!(back.members, squad().members);
    assert_eq!(back.orders, squad().orders);
}
