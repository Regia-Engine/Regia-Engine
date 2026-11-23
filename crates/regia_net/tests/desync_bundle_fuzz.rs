// SPDX-License-Identifier: MPL-2.0
//! Acceptance (N227, spec 120 § 7.6 and § 15.5; ADR-8455): 20,000 seeded mutations of a desync
//! bundle decode without a panic, and a counting allocator shows the decoder never sizes a buffer
//! from a field of the file: the bytes allocated while decoding are bounded by the text's length
//! for every mutation, including ones that write huge counts and lengths into it.
//!
//! Its own test binary: it installs a counting global allocator. Seeded, no wall clock.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_net::desync::sample::sample_bundle;
use regia_net::desync::{DesyncBundle, DesyncInject, ReplayRegistry};

struct Counting;

thread_local! {
    /// Bytes requested on this thread (allocations and the growth of reallocations).
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = BYTES.try_with(|c| c.set(c.get().saturating_add(layout.size())));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = new_size.saturating_sub(layout.size());
        let _ = BYTES.try_with(|c| c.set(c.get().saturating_add(grown)));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocated() -> usize {
    BYTES.with(Cell::get)
}

/// SplitMix64 (public domain, S. Vigna).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Tokens a mutation writes into the text: huge numbers where counts, ticks and lengths go,
/// structure characters, and long hex runs.
const TOKENS: &[&str] = &[
    "18446744073709551615",
    "4294967295",
    "99999999999999999999999",
    "-1",
    "[",
    "]",
    "(",
    ")",
    ",",
    "\"",
    "Some(",
    "None",
    "seat: 65535",
    "tick: 18446744073709551615",
    "format_version: 2",
    "\"ffffffffffffffffffffffffffffffff\"",
    "\"zz\"",
    "\\u{10ffff}",
];

fn mutate(base: &str, rng: &mut Rng) -> String {
    let mut bytes = base.as_bytes().to_vec();
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(bytes.len());
        match rng.below(6) {
            0 => {
                if let Some(b) = bytes.get_mut(at) {
                    *b ^= (rng.next() as u8) | 1;
                }
            }
            1 => {
                let token = TOKENS[rng.below(TOKENS.len())].as_bytes();
                bytes.splice(at..at, token.iter().copied());
            }
            2 => {
                let end = (at + 1 + rng.below(64)).min(bytes.len());
                bytes.drain(at..end);
            }
            3 => {
                // Repeat a slice: a longer list than any writer made.
                let end = (at + 1 + rng.below(256)).min(bytes.len());
                let slice: Vec<u8> = bytes[at..end].to_vec();
                for _ in 0..1 + rng.below(8) {
                    bytes.splice(at..at, slice.iter().copied());
                }
            }
            4 => bytes.truncate(at),
            _ => {
                // Replace a run of digits with a huge number.
                let token = TOKENS[rng.below(3)].as_bytes();
                let end = bytes[at..]
                    .iter()
                    .position(|b| !b.is_ascii_digit())
                    .map_or(bytes.len(), |p| at + p);
                bytes.splice(at..end, token.iter().copied());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Bytes the decoder may allocate for `len` bytes of text: a constant for the parser's own state
/// plus a multiple of the text (every list and string grows from what it read; a hex string
/// becomes half its length; vectors at most double while they grow).
fn bound(len: usize) -> usize {
    256 * 1024 + 48 * len
}

#[test]
fn twenty_thousand_mutated_bundles_decode_without_a_panic_and_allocate_within_the_text_bound() {
    let base = sample_bundle(3, 16, 1, Some(DesyncInject::new("economy", 9)))
        .to_ron()
        .expect("sample bundle text");
    // The base decodes, and replays, so the mutations start from a real bundle.
    let decoded = DesyncBundle::from_ron(&base).expect("base decodes");
    assert!(ReplayRegistry::with_builtin().replay(&decoded).is_ok());

    let registry = ReplayRegistry::with_builtin();
    let mut rng = Rng(0x00de_5c0d_e5ee_d001);
    let (mut accepted, mut refused, mut worst) = (0u32, 0u32, 0f64);
    for i in 0..20_000u32 {
        let text = mutate(&base, &mut rng);
        let before = allocated();
        let result = DesyncBundle::from_ron(&text);
        let used = allocated() - before;
        assert!(
            used <= bound(text.len()),
            "mutation {i}: decoding {} bytes allocated {used} bytes (bound {})",
            text.len(),
            bound(text.len())
        );
        worst = worst.max(used as f64 / text.len().max(1) as f64);
        match result {
            Ok(bundle) => {
                accepted += 1;
                // A bundle that decodes also replays or is refused: never a panic.
                let _ = registry.replay(&bundle);
            }
            Err(err) => {
                refused += 1;
                let code = err.code();
                assert!(
                    code.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "{code}"
                );
            }
        }
    }
    eprintln!("accepted {accepted}, refused {refused}, worst {worst:.1} bytes allocated per text byte");
    assert_eq!(accepted + refused, 20_000);
    assert!(refused > 10_000, "the mutations reach the refusals: {refused}");
}

/// A count written into the text is not a reservation: a frame list claiming a huge tick, a seat
/// list repeated thousands of times and a hex string as long as the text allows are all bounded
/// by the text itself.
#[test]
fn huge_declared_values_allocate_nothing_beyond_the_text() {
    let base = sample_bundle(2, 4, 1, None).to_ron().expect("text");
    // (text, decodes): a huge checkpoint and a 1 MiB snapshot are legal; the others are refused.
    let cases = [
        (base.replacen("tick: 0", "tick: 18446744073709551615", 1), false),
        (
            base.replacen("checkpoint: 3", "checkpoint: 18446744073709551615", 1),
            true,
        ),
        (
            base.replacen("data: \"\"", &format!("data: \"{}\"", "ab".repeat(1 << 20)), 1),
            true,
        ),
        (
            {
                let open = base.find("inputs: [").expect("inputs") + "inputs: [".len();
                let mut text = base.clone();
                text.insert_str(open, &"(seat: 9, input: None),".repeat(5_000));
                text
            },
            false,
        ),
    ];
    for (i, (text, decodes)) in cases.iter().enumerate() {
        assert_ne!(text, &base, "case {i} changed the text");
        let before = allocated();
        let result = DesyncBundle::from_ron(text);
        let used = allocated() - before;
        assert!(
            used <= bound(text.len()),
            "case {i}: {used} bytes for {} of text",
            text.len()
        );
        assert_eq!(
            result.is_ok(),
            *decodes,
            "case {i}: {:?}",
            result.map(|b| b.checkpoint)
        );
    }
}
