// SPDX-License-Identifier: MPL-2.0
//! A Vorbis header's numbers must not size an allocation its packet cannot back (ADR-2723, found by the `decode` cargo-fuzz
//! target: a 7 KB WebM asked for 16 GB through the comment count). The cap is checked by measuring: every hostile header is
//! refused and the largest allocation request stays small. Bytes and counts, not clocks.
#![cfg(feature = "vorbis")]

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_media::codec;
use regia_media::container::{MkvDemuxer, TrackInfo, TrackKind};

use regia_test_utils::alloc_counter::{self, AllocEvent, AllocHook, CountingAlloc};

/// Requests of exactly `WATCHED_SIZE` bytes: the budget across books is cumulative, so no single request shows it.
static WATCHED_SIZE: AtomicUsize = AtomicUsize::new(0);
static WATCHED_HITS: AtomicUsize = AtomicUsize::new(0);

/// Counts the requests of exactly `WATCHED_SIZE` bytes; the shared counting allocator (ADR-3291)
/// records the largest request since `open_measured` last reset it.
struct Watch;

impl AllocHook for Watch {
    fn on_request(&self, event: AllocEvent) {
        if event.size == WATCHED_SIZE.load(Ordering::Relaxed) {
            WATCHED_HITS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc<Watch> = CountingAlloc::with_hook(Watch);

/// One measurement at a time: the counter is global to the process.
static MEASURING: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASURING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// No request while refusing a header should come near this. The fixture's own setup header stays far below it.
const SMALL: usize = 8 * 1024 * 1024;

const AV1_VORBIS: &[u8] = include_bytes!("fixtures/av1_vorbis_32x32.webm");

fn vorbis_track() -> TrackInfo {
    let demux = MkvDemuxer::open(Cursor::new(AV1_VORBIS.to_vec())).expect("the fixture opens");
    demux
        .info()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .cloned()
        .expect("the fixture has an audio track")
}

/// The Xiph-laced parts of a real `CodecPrivate`: identification, comment, setup.
fn split_headers(private: &[u8]) -> [Vec<u8>; 3] {
    assert_eq!(private[0], 2);
    let mut at = 1;
    let mut read_len = || {
        let mut len = 0usize;
        loop {
            let byte = private[at];
            at += 1;
            len += usize::from(byte);
            if byte != 255 {
                return len;
            }
        }
    };
    let ident_len = read_len();
    let comment_len = read_len();
    let body = &private[at..];
    [
        body[..ident_len].to_vec(),
        body[ident_len..ident_len + comment_len].to_vec(),
        body[ident_len + comment_len..].to_vec(),
    ]
}

/// A `CodecPrivate` of the three headers.
fn lace_headers(ident: &[u8], comment: &[u8], setup: &[u8]) -> Vec<u8> {
    let mut out = vec![2u8];
    for part in [ident, comment] {
        out.extend(std::iter::repeat_n(255u8, part.len() / 255));
        out.push(u8::try_from(part.len() % 255).expect("below 255"));
    }
    out.extend_from_slice(ident);
    out.extend_from_slice(comment);
    out.extend_from_slice(setup);
    out
}

/// Opens a decoder for `track` with these headers; the error code and the largest request made.
fn open_measured(ident: &[u8], comment: &[u8], setup: &[u8]) -> (Result<(), String>, usize) {
    let mut track = vorbis_track();
    track.codec_private = lace_headers(ident, comment, setup);
    alloc_counter::reset_largest_request();
    let result = codec::audio_decoder_for(&track)
        .map(|_| ())
        .map_err(|e| e.code().to_string());
    (result, alloc_counter::largest_request())
}

/// A comment header with these fields.
fn comment_header(vendor_len: u32, vendor: &[u8], count: u32, comment_len: Option<u32>) -> Vec<u8> {
    let mut out = vec![3u8];
    out.extend_from_slice(b"vorbis");
    out.extend_from_slice(&vendor_len.to_le_bytes());
    out.extend_from_slice(vendor);
    out.extend_from_slice(&count.to_le_bytes());
    if let Some(len) = comment_len {
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.push(1);
    out
}

/// Vorbis packs bits from the low end of each byte.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: u8,
}

impl Bits {
    fn push(&mut self, value: u64, bits: u32) {
        for i in 0..bits {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = u8::try_from((value >> i) & 1).expect("one bit");
            *self.bytes.last_mut().expect("just pushed") |= bit << self.used;
            self.used = (self.used + 1) % 8;
        }
    }
}

fn setup_start(books: u64) -> Bits {
    let mut bits = Bits::default();
    bits.push(5, 8);
    for byte in b"vorbis" {
        bits.push(u64::from(*byte), 8);
    }
    bits.push(books - 1, 8);
    bits
}

/// The start of a codebook whose codewords all have one length, in the fewest bits the format allows: the ordered form.
fn ordered_book(bits: &mut Bits, dimensions: u64, entries: u64, codeword_length: u64) {
    bits.push(0x56_4342, 24);
    bits.push(dimensions, 16);
    bits.push(entries, 24);
    bits.push(1, 1); // ordered
    bits.push(codeword_length - 1, 5);
    bits.push(entries, 64 - entries.leading_zeros()); // a run of every entry
}

/// A lookup section: `kind` 1 or 2, the two floats and the value width (`bits` is the width minus one).
fn lookup(bits: &mut Bits, kind: u64, value_bits_minus_one: u64) {
    bits.push(kind, 4);
    bits.push(0, 32);
    bits.push(0, 32);
    bits.push(value_bits_minus_one, 4);
    bits.push(0, 1);
}

#[test]
fn the_fixtures_headers_still_open() {
    let _guard = measuring();
    let track = vorbis_track();
    let [ident, comment, setup] = split_headers(&track.codec_private);
    let (result, largest) = open_measured(&ident, &comment, &setup);
    assert_eq!(result, Ok(()));
    assert!(largest < SMALL, "the fixture asked for {largest} bytes");
}

#[test]
fn comment_lengths_past_the_packet_are_refused_without_allocating() {
    let _guard = measuring();
    let track = vorbis_track();
    let [ident, _, setup] = split_headers(&track.codec_private);
    for (what, comment) in [
        ("vendor length", comment_header(u32::MAX, b"", 0, None)),
        ("comment count", comment_header(0, b"", u32::MAX, None)),
        (
            "the count from the finding",
            comment_header(0, b"", 352_321_536, None),
        ),
        ("comment length", comment_header(0, b"", 1, Some(u32::MAX))),
    ] {
        let (result, largest) = open_measured(&ident, &comment, &setup);
        assert_eq!(result, Err("container_invalid".to_string()), "{what}");
        assert!(largest < SMALL, "{what}: asked for {largest} bytes");
    }
}

#[test]
fn codebooks_a_few_bits_describe_are_held_to_the_budget() {
    let _guard = measuring();
    let track = vorbis_track();
    let [ident, comment, _] = split_headers(&track.codec_private);
    // Most entries of 24 bits with the most dimensions: terabytes of vector scalars from one multiplicand.
    let mut vectors = setup_start(1);
    ordered_book(&mut vectors, 65_535, (1 << 24) - 1, 24);
    lookup(&mut vectors, 1, 0);
    vectors.push(0, 1);
    // Within the entry budget, but the lookup's scalars are not.
    let mut within_entries = setup_start(1);
    ordered_book(&mut within_entries, 65_535, 1 << 20, 24);
    lookup(&mut within_entries, 1, 0);
    within_entries.push(0, 1);
    // Within the whole budget, but the packet cannot hold the multiplicands: 2^22 values of four bytes is over `SMALL`.
    let mut multiplicands = setup_start(1);
    ordered_book(&mut multiplicands, 4, 1 << 20, 24);
    lookup(&mut multiplicands, 2, 31);
    for (what, setup) in [
        ("vectors", vectors),
        ("vectors within the entry budget", within_entries),
        ("multiplicands", multiplicands),
    ] {
        let (result, largest) = open_measured(&ident, &comment, &setup.bytes);
        assert_eq!(result, Err("container_invalid".to_string()), "{what}");
        assert!(largest < SMALL, "{what}: asked for {largest} bytes");
    }
}

/// Books under the budget one at a time, over it together. Length 20 over 2^20 entries is a complete code, so each book
/// builds and the next one is read; each starts with a request of exactly 2^20 bytes for its lengths.
#[test]
fn the_budget_spans_the_books_of_a_setup_header() {
    let _guard = measuring();
    let track = vorbis_track();
    let [ident, comment, _] = split_headers(&track.codec_private);
    let mut many = setup_start(64);
    for _ in 0..64 {
        ordered_book(&mut many, 1, 1 << 20, 20);
        many.push(0, 4); // no lookup
    }
    WATCHED_HITS.store(0, Ordering::Relaxed);
    WATCHED_SIZE.store(1 << 20, Ordering::Relaxed);
    let (result, _) = open_measured(&ident, &comment, &many.bytes);
    WATCHED_SIZE.store(0, Ordering::Relaxed);
    assert_eq!(result, Err("container_invalid".to_string()));
    // 2^23 of budget at 2^20 a book: eight books start, and the ninth is refused before it allocates.
    assert_eq!(WATCHED_HITS.load(Ordering::Relaxed), 8);
}

/// An ordered book raises its codeword length once per run, and a run may be empty: a few hundred zero bits used to carry
/// the `u8` length past its end, a panic in a build with overflow checks. Codewords have at most 32 bits.
#[test]
fn ordered_codeword_lengths_stop_at_32() {
    let _guard = measuring();
    let track = vorbis_track();
    let [ident, comment, _] = split_headers(&track.codec_private);
    let mut setup = setup_start(1);
    setup.push(0x56_4342, 24);
    setup.push(1, 16); // dimensions
    setup.push(1, 24); // entries
    setup.push(1, 1); // ordered
    setup.push(0, 5); // the first length, minus one
    for _ in 0..300 {
        setup.push(0, 1); // an empty run
    }
    setup.push(0, 64);
    let (result, _) = open_measured(&ident, &comment, &setup.bytes);
    assert_eq!(result, Err("container_invalid".to_string()));
}
