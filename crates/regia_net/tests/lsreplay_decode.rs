// SPDX-License-Identifier: MPL-2.0
//! Acceptance (NL-02, ADR-8513): `lsreplay_decode`. 20,000 seeded mutations of a `.lsreplay` decode
//! without a panic, and a counting allocator shows the reader never sizes a buffer from a length
//! field: the bytes allocated while decoding are bounded by the input's length for every mutation,
//! including ones that write huge counts and lengths into it. Hostile cases adapted as
//! requirements from DO27's hostile replay test (a string-length bomb, a count bomb).
//!
//! Its own test binary: it installs a counting global allocator. Seeded, no wall clock.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_net::desync::ReplayRegistry;
use regia_net::lockstep::replay::{
    LSREPLAY_MAGIC, LSREPLAY_VERSION, LsReplayReader, RECORD_HASHES, RECORD_INPUTS, audit_with, read_whole,
    sample_replay,
};

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

fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut bytes = base.to_vec();
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(bytes.len());
        match rng.below(7) {
            0 => {
                if let Some(b) = bytes.get_mut(at) {
                    *b ^= (rng.next() as u8) | 1;
                }
            }
            1 => {
                if let Some(b) = bytes.get_mut(at) {
                    *b = rng.next() as u8;
                }
            }
            2 => bytes.truncate(at),
            3 => {
                for _ in 0..rng.below(8) {
                    bytes.insert(at, rng.next() as u8);
                }
            }
            4 => {
                // 0xff runs: the largest lengths and counts a field can hold.
                let end = (at + 1 + rng.below(4)).min(bytes.len());
                bytes[at..end].fill(0xff);
            }
            5 => {
                // A huge little-endian u32 where a length may be.
                let word = [u32::MAX, 0x7fff_ffff, 1 << 24, 65_535][rng.below(4)].to_le_bytes();
                let end = (at + 4).min(bytes.len());
                bytes.splice(at..end, word);
            }
            _ => {
                // Repeat a slice: more records or items than any writer made.
                let end = (at + 1 + rng.below(64)).min(bytes.len());
                let slice: Vec<u8> = bytes[at..end].to_vec();
                for _ in 0..1 + rng.below(4) {
                    bytes.splice(at..at, slice.iter().copied());
                }
            }
        }
        if bytes.is_empty() {
            break;
        }
    }
    bytes
}

/// Bytes decoding `len` bytes may allocate, counted over the whole exercise (the reader, the
/// re-encode and the audit each decode the file once): a constant for the reader's and the hook's
/// own state plus a multiple of the input (every list and byte string is a copy of what was read,
/// vectors at most double while they grow). A size taken from a length field would be up to 4 GiB.
fn bound(len: usize) -> usize {
    64 * 1024 + 64 * len
}

/// Decodes, re-encodes and audits `bytes` the way a tool does; never panics.
fn exercise(bytes: &[u8], registry: &ReplayRegistry) -> Result<(), &'static str> {
    let reader = LsReplayReader::new(bytes).map_err(|e| e.code())?;
    reader.read_all().map_err(|e| e.code())?;
    read_whole(bytes).map_err(|e| e.code())?;
    audit_with(bytes, registry).map(|_| ()).map_err(|e| e.code())
}

#[test]
fn lsreplay_decode() {
    let base = sample_replay(3, 48, 8, 4, None);
    let registry = ReplayRegistry::with_builtin();
    assert_eq!(exercise(&base, &registry), Ok(()), "the base decodes and audits");
    assert_eq!(read_whole(&base).as_deref().ok(), Some(&base[..]));

    let mut rng = Rng(0x15e9_1a7d_ec0d_e001);
    let (mut accepted, mut refused, mut worst) = (0u32, 0u32, 0f64);
    for i in 0..20_000u32 {
        let bytes = mutate(&base, &mut rng);
        let before = allocated();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(&bytes, &registry)));
        let used = allocated() - before;
        let Ok(result) = outcome else {
            panic!("mutation {i}: the decoder panicked on {} bytes", bytes.len());
        };
        assert!(
            used <= bound(bytes.len()),
            "mutation {i}: decoding {} bytes allocated {used} bytes (bound {})",
            bytes.len(),
            bound(bytes.len())
        );
        worst = worst.max(used as f64 / bytes.len().max(1) as f64);
        match result {
            Ok(()) => accepted += 1,
            Err(code) => {
                refused += 1;
                assert!(
                    code.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "{code}"
                );
            }
        }
    }
    eprintln!("accepted {accepted}, refused {refused}, worst {worst:.1} bytes allocated per input byte");
    assert_eq!(accepted + refused, 20_000);
    assert!(refused > 10_000, "the mutations reach the refusals: {refused}");
}

/// A length or count written into the file is not a reservation: a header-string bomb, a record
/// length past the end, an input count, a subsystem count and an input length each claiming the
/// most their field holds are refused, allocating no more than the input.
#[test]
fn declared_lengths_allocate_nothing_beyond_the_input() {
    let base = sample_replay(2, 6, 4, 2, None);
    let head_end = LsReplayReader::new(&base).expect("header").position();
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();

    // The header length and the game id's length at their largest.
    let mut header_bomb = LSREPLAY_MAGIC.to_vec();
    header_bomb.extend_from_slice(&LSREPLAY_VERSION.to_le_bytes());
    header_bomb.extend_from_slice(&u32::MAX.to_le_bytes());
    cases.push(("header length", header_bomb));
    let mut id_bomb = LSREPLAY_MAGIC.to_vec();
    id_bomb.extend_from_slice(&LSREPLAY_VERSION.to_le_bytes());
    id_bomb.extend_from_slice(&64u32.to_le_bytes());
    id_bomb.extend_from_slice(&u16::MAX.to_le_bytes());
    id_bomb.resize(id_bomb.len() + 62, 0x61);
    cases.push(("game id length", id_bomb));

    // A record whose length runs past the end of the file.
    let mut record_bomb = base[..head_end].to_vec();
    record_bomb.push(RECORD_INPUTS);
    record_bomb.extend_from_slice(&(4u32 << 20).to_le_bytes());
    record_bomb.extend_from_slice(&[0; 32]);
    cases.push(("record length", record_bomb));

    // An Inputs record claiming 65,535 seats, and one claiming a 65,535-byte input, in 40 bytes.
    let mut seats = Vec::new();
    seats.extend_from_slice(&7u64.to_le_bytes());
    seats.extend_from_slice(&4u16.to_le_bytes());
    seats.extend_from_slice(&0u64.to_le_bytes());
    seats.extend_from_slice(&u16::MAX.to_le_bytes());
    seats.extend_from_slice(&[0; 20]);
    let mut seat_bomb = base[..head_end].to_vec();
    seat_bomb.push(RECORD_INPUTS);
    seat_bomb.extend_from_slice(&(seats.len() as u32).to_le_bytes());
    seat_bomb.extend_from_slice(&seats);
    cases.push(("seat count", seat_bomb));
    let mut input = seats[..18].to_vec();
    input.extend_from_slice(&1u16.to_le_bytes());
    input.extend_from_slice(&0u16.to_le_bytes());
    input.push(1);
    input.extend_from_slice(&u16::MAX.to_le_bytes());
    input.extend_from_slice(&[0; 16]);
    let mut input_bomb = base[..head_end].to_vec();
    input_bomb.push(RECORD_INPUTS);
    input_bomb.extend_from_slice(&(input.len() as u32).to_le_bytes());
    input_bomb.extend_from_slice(&input);
    cases.push(("input length", input_bomb));

    // A Hashes record claiming 255 subsystems in 20 bytes.
    let mut hashes = Vec::new();
    hashes.extend_from_slice(&0u64.to_le_bytes());
    hashes.extend_from_slice(&0u64.to_le_bytes());
    hashes.push(255);
    hashes.extend_from_slice(&[3, b'a', b'b', b'c']);
    let mut hash_bomb = base[..head_end].to_vec();
    hash_bomb.push(RECORD_HASHES);
    hash_bomb.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
    hash_bomb.extend_from_slice(&hashes);
    cases.push(("subsystem count", hash_bomb));

    let registry = ReplayRegistry::with_builtin();
    for (what, bytes) in &cases {
        let before = allocated();
        let result = exercise(bytes, &registry);
        let used = allocated() - before;
        assert!(result.is_err(), "{what}: refused");
        assert!(
            used <= bound(bytes.len()),
            "{what}: {} bytes allocated {used} (bound {})",
            bytes.len(),
            bound(bytes.len())
        );
    }
}
