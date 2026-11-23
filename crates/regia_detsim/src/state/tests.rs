// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the det-state codec, the frame and the checkpoint ring (DC-3.7a).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

/// A small state with every float shape the tests need.
#[derive(Debug, Clone, PartialEq)]
struct Probe {
    f: Vec<f32>,
    d: Vec<f64>,
}

impl DetState for Probe {
    const STATE_TAG: [u8; 8] = *b"probe\0\0\0";
    const STATE_VERSION: u32 = 3;
    fn save(&self, w: &mut DetWriter) {
        w.seq(&self.f, |w, v| w.f32(*v));
        w.seq(&self.d, |w, v| w.f64(*v));
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Probe {
            f: r.seq(|r| r.f32())?,
            d: r.seq(|r| r.f64())?,
        })
    }
}

/// The same tag, a newer version: a build that changed its layout.
#[derive(Debug)]
struct ProbeV4;

impl DetState for ProbeV4 {
    const STATE_TAG: [u8; 8] = *b"probe\0\0\0";
    const STATE_VERSION: u32 = 4;
    fn save(&self, _w: &mut DetWriter) {}
    fn load(_r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(ProbeV4)
    }
}

/// Another type entirely.
#[derive(Debug)]
struct Other;

impl DetState for Other {
    const STATE_TAG: [u8; 8] = *b"other\0\0\0";
    const STATE_VERSION: u32 = 3;
    fn save(&self, _w: &mut DetWriter) {}
    fn load(_r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Other)
    }
}

const F32_BITS: [u32; 12] = [
    0x8000_0000, // -0.0
    0x0000_0000, // +0.0
    0x7fc0_0000, // the canonical quiet NaN
    0x7fc0_0001, // quiet NaN with a payload
    0xffc1_2345, // negative quiet NaN with a payload
    0x7f80_0001, // signaling NaN
    0x7f80_0000, // +inf
    0xff80_0000, // -inf
    0x0000_0001, // smallest subnormal
    0x807f_ffff, // largest negative subnormal
    0x0080_0000, // smallest normal
    0x3fc0_0000, // 1.5
];

const F64_BITS: [u64; 10] = [
    0x8000_0000_0000_0000,
    0x7ff8_0000_0000_0001,
    0xfff8_dead_beef_0001,
    0x7ff0_0000_0000_0001,
    0x7ff0_0000_0000_0000,
    0xfff0_0000_0000_0000,
    0x0000_0000_0000_0001,
    0x800f_ffff_ffff_ffff,
    0x0010_0000_0000_0000,
    0x3ff8_0000_0000_0000,
];

fn probe() -> Probe {
    Probe {
        f: F32_BITS.iter().map(|b| f32::from_bits(*b)).collect(),
        d: F64_BITS.iter().map(|b| f64::from_bits(*b)).collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Shape {
    Empty,
    Circle(f32),
    Rect { w: f32, h: f32 },
    Pair(u8, i64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Derived {
    name: String,
    pos: [f32; 3],
    tags: Vec<u16>,
    shapes: Vec<Shape>,
    parent: Option<u32>,
    by_id: BTreeMap<u32, f64>,
    flag: bool,
    big: u128,
    neg: i128,
    letter: char,
    unit: (),
}

fn derived() -> Derived {
    Derived {
        name: "crate_7".into(),
        pos: [-0.0, f32::from_bits(0x7fc0_0042), 1.0e-40],
        tags: vec![1, 2, 65_535],
        shapes: vec![
            Shape::Empty,
            Shape::Circle(f32::from_bits(0xffc0_0007)),
            Shape::Rect { w: 2.0, h: -0.0 },
            Shape::Pair(9, -3),
        ],
        parent: Some(4),
        by_id: BTreeMap::from([(3, -0.0), (1, f64::from_bits(0x7ff8_0000_0000_0009))]),
        flag: true,
        big: u128::MAX - 7,
        neg: i128::MIN + 1,
        letter: 'é',
        unit: (),
    }
}

fn f32_bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn f64_bits(v: &[f64]) -> Vec<u64> {
    v.iter().map(|x| x.to_bits()).collect()
}

#[test]
fn raw_float_bits_survive() {
    // Through the hand-written codec, framed.
    let state = probe();
    let bytes = snapshot(&state, 77).expect("snapshot");
    let (tick, back): (u64, Probe) = restore(&bytes).expect("restore");
    assert_eq!(tick, 77);
    assert_eq!(f32_bits(&back.f), F32_BITS.to_vec());
    assert_eq!(f64_bits(&back.d), F64_BITS.to_vec());

    // The bytes are the raw bits, little-endian (not the hash's canonical rule).
    let body = to_bytes(&state).expect("body");
    assert_eq!(&body[4..8], &0x8000_0000u32.to_le_bytes());
    assert_eq!(&body[16..20], &0x7fc0_0001u32.to_le_bytes());

    // Through serde: a derived struct with -0.0, NaN payloads and a subnormal.
    let value = derived();
    let mut w = DetWriter::new();
    w.put(&value);
    let bytes = w.into_bytes().expect("serde write");
    let mut r = DetReader::new(&bytes, DetReadLimits::DEFAULT).expect("reader");
    let back: Derived = r.take().expect("serde read");
    r.finish().expect("all read");
    assert_eq!(f32_bits(&back.pos), f32_bits(&value.pos));
    assert_eq!(back.shapes[0], Shape::Empty);
    assert_eq!(back.shapes[3], Shape::Pair(9, -3));
    let Shape::Circle(c) = back.shapes[1] else {
        panic!("variant order changed");
    };
    assert_eq!(c.to_bits(), 0xffc0_0007);
    let Shape::Rect { h, .. } = back.shapes[2] else {
        panic!("variant order changed");
    };
    assert_eq!(h.to_bits(), 0x8000_0000);
    let map_bits: Vec<(u32, u64)> = back.by_id.iter().map(|(k, v)| (*k, v.to_bits())).collect();
    assert_eq!(
        map_bits,
        vec![(1, 0x7ff8_0000_0000_0009), (3, 0x8000_0000_0000_0000)]
    );
    // Every non-float field round trips as is.
    assert_eq!(back.name, value.name);
    assert_eq!(back.tags, value.tags);
    assert_eq!(back.parent, value.parent);
    assert_eq!(
        (back.flag, back.big, back.neg, back.letter),
        (true, value.big, value.neg, 'é')
    );
}

#[test]
fn reader_refuses_bad_input() {
    let state = probe();
    let good = snapshot(&state, 5).expect("snapshot");

    // Truncated: every strict prefix is refused, never a panic.
    for cut in 0..good.len() {
        let err = restore::<Probe>(&good[..cut]).expect_err("prefix");
        assert_eq!(err.code(), "truncated", "cut at {cut}: {err}");
    }

    // Trailing bytes after the frame.
    let mut long = good.clone();
    long.push(0);
    assert_eq!(
        restore::<Probe>(&long).expect_err("trailing"),
        DetStateError::TrailingBytes { left: 1 }
    );

    // Trailing bytes inside a well-framed body (load leaves some unread).
    let mut body = to_bytes(&state).expect("body");
    body.extend_from_slice(&[1, 2, 3]);
    let framed = frame_bytes(
        SnapshotKind::State,
        Probe::STATE_TAG,
        Probe::STATE_VERSION,
        5,
        &body,
    )
    .expect("frame");
    assert_eq!(
        restore::<Probe>(&framed).expect_err("inner trailing"),
        DetStateError::TrailingBytes { left: 3 }
    );

    // Wrong tag.
    let err = restore::<Other>(&good).expect_err("tag");
    assert_eq!(err.code(), "bad_tag");
    assert!(
        err.to_string().contains("probe") && err.to_string().contains("other"),
        "{err}"
    );

    // Wrong magic is bad_tag too.
    let mut magic = good.clone();
    magic[0] = b'X';
    assert_eq!(restore::<Probe>(&magic).expect_err("magic").code(), "bad_tag");

    // Wrong version: an old snapshot read by a newer build names both versions.
    let err = restore::<ProbeV4>(&good).expect_err("version");
    assert_eq!(
        err,
        DetStateError::BadVersion {
            found: 3,
            expected: 4
        }
    );
    let text = err.to_string();
    assert!(
        text.starts_with("bad_version") && text.contains('3') && text.contains('4'),
        "{text}"
    );

    // A newer frame layout is bad_version too.
    let mut frame_v = good.clone();
    frame_v[4] = 2;
    assert_eq!(
        restore::<Probe>(&frame_v).expect_err("frame version"),
        DetStateError::BadVersion {
            found: 2,
            expected: 1
        }
    );

    // A flipped body bit fails the integrity hash.
    let mut flipped = good.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x10;
    assert_eq!(restore::<Probe>(&flipped).expect_err("hash").code(), "bad_hash");

    // Oversized length prefix (over max_len), inside a valid frame.
    let mut w = DetWriter::new();
    w.u32(1 << 30);
    let framed = frame_bytes(
        SnapshotKind::State,
        Probe::STATE_TAG,
        Probe::STATE_VERSION,
        5,
        &w.into_bytes().expect("body"),
    )
    .expect("frame");
    assert_eq!(
        restore::<Probe>(&framed).expect_err("length"),
        DetStateError::LengthLimit {
            len: 1 << 30,
            limit: u64::from(DetReadLimits::DEFAULT.max_len)
        }
    );

    // Oversized input (over max_bytes).
    let tight = DetReadLimits {
        max_bytes: good.len() - 1,
        max_len: 1024,
    };
    assert_eq!(
        restore_with::<Probe>(&good, tight).expect_err("max bytes").code(),
        "length_limit"
    );

    // A harness frame is not a state frame.
    let harness = frame_bytes(
        SnapshotKind::Harness,
        Probe::STATE_TAG,
        Probe::STATE_VERSION,
        5,
        &to_bytes(&state).expect("body"),
    )
    .expect("frame");
    assert_eq!(restore::<Probe>(&harness).expect_err("kind").code(), "bad_value");

    // Out-of-domain values.
    let mut r = DetReader::new(&[2], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.bool().expect_err("bool").code(), "bad_value");
    let mut w = DetWriter::new();
    w.length(3);
    w.u8(0b1111_1000); // padding bits set past the third
    let bytes = w.into_bytes().expect("bits");
    let mut r = DetReader::new(&bytes, DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.bits().expect_err("padding").code(), "bad_value");
    let mut r = DetReader::new(&[3], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.take::<Option<u8>>().expect_err("option").code(), "bad_value");
    let mut r = DetReader::new(&[0x00, 0xd8, 0, 0], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.take::<char>().expect_err("char").code(), "bad_value");
    let mut r = DetReader::new(&[1, 0, 0, 0, 0xff], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.take::<String>().expect_err("utf8").code(), "bad_value");
    let mut r = DetReader::new(&[9, 0, 0, 0], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.take::<Shape>().expect_err("variant").code(), "bad_value");
}

#[test]
fn every_error_code_is_lowercase_ascii() {
    let all = [
        DetStateError::Truncated { offset: 0, needed: 1 },
        DetStateError::LengthLimit { len: 2, limit: 1 },
        DetStateError::BadVersion {
            found: 1,
            expected: 2,
        },
        DetStateError::BadTag {
            found: [0; 8],
            expected: [1; 8],
        },
        DetStateError::TrailingBytes { left: 1 },
        DetStateError::bad_value("x"),
        DetStateError::BadHash {
            found: 1,
            expected: 2,
        },
    ];
    let codes: Vec<&str> = all.iter().map(DetStateError::code).collect();
    assert_eq!(
        codes,
        [
            "truncated",
            "length_limit",
            "bad_version",
            "bad_tag",
            "trailing_bytes",
            "bad_value",
            "bad_hash"
        ]
    );
    for (error, code) in all.iter().zip(codes) {
        assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        assert!(error.to_string().starts_with(code), "{error}");
    }
    // A long serde message is cut on a char boundary.
    let long = DetStateError::bad_value("é".repeat(200));
    let DetStateError::BadValue { detail } = long else {
        panic!("bad_value");
    };
    assert!(detail.len() <= 160);
}

#[test]
fn writer_refuses_what_the_reader_could_not_read() {
    #[derive(Serialize)]
    struct Skips {
        #[serde(skip_serializing_if = "Option::is_none")]
        a: Option<u8>,
    }
    let mut w = DetWriter::new();
    w.put(&Skips { a: None });
    assert_eq!(w.into_bytes().expect_err("skip").code(), "bad_value");

    struct Unsized;
    impl Serialize for Unsized {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let seq = s.serialize_seq(None)?;
            seq.end()
        }
    }
    let mut w = DetWriter::new();
    w.put(&Unsized);
    assert_eq!(w.into_bytes().expect_err("no length").code(), "bad_value");

    // The reader does not guess at self-describing input.
    let mut r = DetReader::new(&[0; 8], DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(
        r.take::<serde::de::IgnoredAny>().expect_err("any").code(),
        "bad_value"
    );
}

#[test]
fn saving_does_not_change_the_bytes_or_the_state() {
    let state = probe();
    let a = snapshot(&state, 9).expect("first");
    let b = snapshot(&state, 9).expect("second");
    assert_eq!(a, b);
    assert_eq!(f32_bits(&state.f), F32_BITS.to_vec());
    let header = read_header(&a, DetReadLimits::DEFAULT).expect("header");
    assert_eq!(header.kind, SnapshotKind::State);
    assert_eq!(header.tag, Probe::STATE_TAG);
    assert_eq!(header.version, 3);
    assert_eq!(header.tick, 9);
    assert_eq!(header.body_len as usize, a.len() - HEADER_LEN);
    // A different tick changes the integrity hash, so a relabeled body is refused.
    let c = snapshot(&state, 10).expect("other tick");
    assert_ne!(a[32..40], c[32..40]);
    let mut relabeled = a.clone();
    relabeled[20..28].copy_from_slice(&10u64.to_le_bytes());
    assert_eq!(
        restore::<Probe>(&relabeled).expect_err("relabel").code(),
        "bad_hash"
    );
}

#[test]
fn snapshot_ring_is_bounded_in_bytes() {
    assert_eq!(
        SnapshotRing::new(SnapshotRingSettings {
            capacity_ticks: 0,
            every: 1,
            max_bytes: 1,
        })
        .expect_err("zero capacity")
        .code(),
        "bad_value"
    );

    let entry = 100;
    let mut ring = SnapshotRing::new(SnapshotRingSettings {
        capacity_ticks: 1_000,
        every: 5,
        max_bytes: 3 * entry + 50,
    })
    .expect("ring");
    // Off-cadence ticks are not kept.
    assert!(!ring.push(3, vec![0; entry]).expect("push"));
    for tick in (0..200).step_by(5) {
        assert!(ring.push(tick, vec![tick as u8; entry]).expect("push"));
        assert!(ring.bytes_used() <= ring.settings().max_bytes);
        assert!(ring.len() <= 3);
    }
    assert_eq!(ring.ticks().collect::<Vec<_>>(), vec![185, 190, 195]);
    assert_eq!(ring.bytes_used(), 3 * entry);
    assert_eq!(ring.at(190).map(|b| b[0]), Some(190));
    assert_eq!(ring.at(191), None);
    assert_eq!(ring.latest_at_or_before(193).map(|(t, _)| t), Some(190));
    assert_eq!(ring.latest_at_or_before(10_000).map(|(t, _)| t), Some(195));
    assert_eq!(ring.latest_at_or_before(184), None);

    // One checkpoint larger than the whole budget is refused, and the ring is unchanged.
    assert_eq!(
        ring.push(200, vec![0; 4 * entry]).expect_err("too big").code(),
        "length_limit"
    );
    assert_eq!(ring.len(), 3);

    // Mixed sizes: the oldest go first until the sum fits.
    ring.push(200, vec![1; 3 * entry]).expect("big");
    assert_eq!(ring.ticks().collect::<Vec<_>>(), vec![200]);
    assert!(ring.bytes_used() <= ring.settings().max_bytes);

    // A rewind replaces the checkpoints from there on.
    ring.clear();
    for tick in [10, 15, 20] {
        ring.push(tick, vec![tick as u8; 10]).expect("push");
    }
    ring.push(15, vec![99; 10]).expect("rewind");
    assert_eq!(ring.ticks().collect::<Vec<_>>(), vec![10, 15]);
    assert_eq!(ring.at(15).map(|b| b[0]), Some(99));
    assert_eq!(ring.bytes_used(), 20);

    // The tick window: checkpoints older than capacity_ticks drop.
    let mut ring = SnapshotRing::new(SnapshotRingSettings {
        capacity_ticks: 30,
        every: 10,
        max_bytes: 1 << 20,
    })
    .expect("ring");
    for tick in (0..=100).step_by(10) {
        ring.record(tick, &probe()).expect("record");
    }
    assert_eq!(ring.ticks().collect::<Vec<_>>(), vec![80, 90, 100]);
    let (tick, bytes) = ring.latest_at_or_before(95).expect("checkpoint");
    let (at, back): (u64, Probe) = restore(bytes).expect("restore");
    assert_eq!((tick, at), (90, 90));
    assert_eq!(f32_bits(&back.f), F32_BITS.to_vec());
    // `record` off-cadence saves nothing.
    assert!(!ring.record(101, &probe()).expect("off cadence"));
}

#[test]
fn writer_lengths_and_bits_round_trip() {
    let bits: Vec<bool> = (0..37).map(|i| i % 3 == 0 || i == 36).collect();
    let mut w = DetWriter::new();
    w.bits(&bits);
    w.bits(&[]);
    w.str("lockstep");
    w.bytes(&[1, 2, 3]);
    w.i8(-1);
    w.i16(-2);
    w.i32(-3);
    w.i64(-4);
    w.raw(&[7; 3]);
    let bytes = w.into_bytes().expect("write");
    let mut r = DetReader::new(&bytes, DetReadLimits::DEFAULT).expect("reader");
    assert_eq!(r.bits().expect("bits"), bits);
    assert!(r.bits().expect("empty").is_empty());
    assert_eq!(r.str().expect("str"), "lockstep");
    assert_eq!(r.bytes().expect("bytes"), &[1, 2, 3]);
    assert_eq!(
        (
            r.i8().expect("i8"),
            r.i16().expect("i16"),
            r.i32().expect("i32"),
            r.i64().expect("i64")
        ),
        (-1, -2, -3, -4)
    );
    assert_eq!(r.raw(3).expect("raw"), &[7; 3]);
    r.finish().expect("all read");
    assert_eq!(r.position(), bytes.len());
}

#[test]
fn hash_container_names_are_the_outer_type_only() {
    // H1 (review det-core-review-c): the names serde's map and set impls hand to `collect_map` /
    // `collect_seq` (`&Container`), as `std::any::type_name` spells them.
    for sorted in [
        "&std::collections::hash::map::HashMap<u32, u32, std::hash::random::RandomState>",
        "&std::collections::hash::set::HashSet<alloc::string::String>",
        "&hashbrown::map::HashMap<u32, f32, foldhash::fast::RandomState>",
        "&hashbrown::set::HashSet<u64>",
        "&bevy_platform::collections::hash_map::HashMap<u32, u32>",
        "&ahash::hash_map::AHashMap<u32, u32>",
        "&mut std::collections::hash::map::HashMap<u8, u8>",
    ] {
        assert!(hash_container_name(sorted), "{sorted}");
    }
    for kept in [
        "&alloc::collections::btree::map::BTreeMap<u32, std::collections::hash::map::HashMap<u8, u8>>",
        "&alloc::collections::btree::set::BTreeSet<u32>",
        "&alloc::vec::Vec<std::collections::hash::set::HashSet<u8>>",
        "&indexmap::map::IndexMap<u32, u32>",
        "&[u32]",
        "std::collections::hash::map::Keys<'_, u32, u32>",
        "core::slice::iter::Iter<'_, u8>",
        "&regia_game::HashGrid",
    ] {
        assert!(!hash_container_name(kept), "{kept}");
    }
    // The type serde passes for a real map: proves the detection on this toolchain's spelling.
    // det-scan: allow(hash_iteration): test input, the type serde passes for a hash map (H1)
    assert!(is_hash_container::<&std::collections::HashMap<u32, u32>>());
    // det-scan: allow(hash_iteration): test input, the type serde passes for a hash set (H1)
    assert!(is_hash_container::<&std::collections::HashSet<u32>>());
    assert!(!is_hash_container::<&BTreeMap<u32, u32>>());
    assert!(!is_hash_container::<&Vec<u32>>());
}

#[test]
fn sorted_entries_keep_the_btree_bytes_and_read_back() {
    // An ordered map is written in its own order: the bytes are the ones before H1 (count, then
    // key/value pairs in key order), so nothing recorded from a BTreeMap moves.
    let ordered = BTreeMap::from([(256u32, 1u8), (1u32, 2u8)]);
    let mut w = DetWriter::new();
    w.put(&ordered);
    let bytes = w.into_bytes().expect("write");
    assert_eq!(bytes, [2, 0, 0, 0, 1, 0, 0, 0, 2, 0, 1, 0, 0, 1]);
    // The same entries in a hash map: sorted by encoded key bytes (256 is 00 01 00 00, 1 is 01 00 00
    // 00), and the reader takes them back in either order.
    // det-scan: allow(hash_iteration): test input, built from the ordered map and never iterated
    let hashed: std::collections::HashMap<u32, u8> = ordered.clone().into_iter().collect();
    let mut w = DetWriter::new();
    w.put(&hashed);
    let bytes = w.into_bytes().expect("write");
    assert_eq!(bytes, [2, 0, 0, 0, 0, 1, 0, 0, 1, 1, 0, 0, 0, 2]);
    let mut r = DetReader::new(&bytes, DetReadLimits::DEFAULT).expect("reader");
    // det-scan: allow(hash_iteration): test output, compared with `==` only
    let back: std::collections::HashMap<u32, u8> = r.take().expect("take");
    r.finish().expect("all read");
    assert_eq!(back, hashed);
    let mut r = DetReader::new(&bytes, DetReadLimits::DEFAULT).expect("reader");
    let as_btree: BTreeMap<u32, u8> = r.take().expect("take");
    assert_eq!(as_btree, ordered);
}
