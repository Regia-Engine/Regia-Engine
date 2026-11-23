// SPDX-License-Identifier: MPL-2.0
//! Deterministic mutation fuzzing of the compressed-codec path (ADR-1593 § 4, ADR-1869 § 8): the CI
//! corpus. Each committed real-encoder fixture is mutated with a seeded generator (truncations, bit
//! flips, byte overwrites, splices of another fixture, hostile lengths) and run through `probe`, a full
//! `Session` decode with audio, and the raw decoders packet by packet. Every case must end in a value or
//! an error: no panic, no hang (the loops are bounded by the input), no sample outside [-1, 1], and no
//! picture over the caps. `crates/regia_media/fuzz/` runs the same entry point
//! (`regia_media::fuzzing::decode_untrusted`) under `cargo fuzz`.
#![cfg(feature = "video")]

use std::io::Cursor;

use regia_media::codec;
use regia_media::container::{Demuxer, MkvDemuxer, TrackKind, probe};
use regia_media::fuzzing;
use regia_media::isolate::{DecodeCaps, Guarded, GuardedAudio, GuardedVideo};

const FIXTURES: [&[u8]; 8] = [
    include_bytes!("fixtures/av1_opus_64x48.webm"),
    include_bytes!("fixtures/av1_10bit_65x33.webm"),
    include_bytes!("fixtures/av1_vorbis_32x32.webm"),
    include_bytes!("fixtures/av1_444_48x32.webm"),
    include_bytes!("fixtures/av1_422_10bit_48x32.webm"),
    include_bytes!("fixtures/av1_12bit_48x32.webm"),
    include_bytes!("fixtures/av1_gray_48x32.webm"),
    include_bytes!("fixtures/av1_grain_48x32.webm"),
];

/// Mutations per fixture.
const CASES: usize = 250;
/// Upper bound on packets per decoder run.
const MAX_POLLS: usize = fuzzing::MAX_POLLS;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(base: &[u8], other: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut b = base.to_vec();
    match rng.below(6) {
        0 => b.truncate(rng.below(base.len())),
        1 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
        }
        2 => {
            let i = rng.below(b.len());
            let n = (1 + rng.below(64)).min(b.len() - i);
            for v in &mut b[i..i + n] {
                *v = rng.next() as u8;
            }
        }
        3 => {
            let at = rng.below(b.len());
            let from = rng.below(other.len());
            let n = rng.below(other.len() - from);
            b.splice(at..at, other[from..from + n].iter().copied());
        }
        4 => {
            // A hostile EBML size: 0x01 followed by seven 0xff bytes (an 8-byte vint near 2^56).
            let i = rng.below(b.len().saturating_sub(8));
            b[i..i + 8].copy_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        }
        _ => {
            // Packet-level garbage deep in the clusters (past the headers).
            let start = b.len() / 3;
            for _ in 0..16 {
                let i = start + rng.below(b.len() - start);
                b[i] = rng.next() as u8;
            }
        }
    }
    b
}

/// One case through probe and a whole session (the shared fuzz entry point). Returns the pictures.
fn run_session(bytes: &[u8]) -> usize {
    fuzzing::decode_untrusted(bytes).pictures
}

/// One case through the guarded decoders directly, packet by packet.
fn run_decoders(bytes: &[u8]) {
    let Ok(mut demux) = MkvDemuxer::open(Cursor::new(bytes.to_vec())) else {
        return;
    };
    let tracks = demux.info().tracks.clone();
    let caps = DecodeCaps::default();
    let mut video = tracks.iter().find(|t| t.kind == TrackKind::Video).and_then(|t| {
        codec::video_decoder_with(t, &caps)
            .ok()
            .map(|d| (t.number, GuardedVideo::new(d, caps)))
    });
    let mut audio = tracks.iter().find(|t| t.kind == TrackKind::Audio).and_then(|t| {
        codec::audio_decoder_for(t)
            .ok()
            .map(|d| (t.number, GuardedAudio::new(d, caps)))
    });
    let mut pcm = Vec::new();
    for _ in 0..MAX_POLLS {
        let packet = match demux.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => break,
        };
        if let Some((n, v)) = &mut video
            && packet.track == *n
        {
            match v.decode(&packet) {
                Ok(Guarded::Ok(pics)) => {
                    assert!(pics.iter().all(|p| caps.check_size(p.width, p.height).is_ok()))
                }
                Ok(Guarded::Dropped) => {}
                Err(e) => assert_eq!(e.code(), "decode_failed"),
            }
        }
        if let Some((n, a)) = &mut audio
            && packet.track == *n
        {
            pcm.clear();
            a.decode(&packet, &mut pcm, 960)
                .expect("audio rejections are silence, never fatal");
            assert!(pcm.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        }
    }
    if let Some((_, v)) = &mut video {
        let _ = v.drain();
    }
}

#[test]
fn mutated_fixtures_never_panic_or_hang() {
    let mut rng = Rng(0x5eed_c0de_cafe_f00d);
    let mut errors = 0usize;
    let mut decoded = 0usize;
    for (i, base) in FIXTURES.iter().enumerate() {
        let other = FIXTURES[(i + 1) % FIXTURES.len()];
        for _ in 0..CASES {
            let bytes = mutate(base, other, &mut rng);
            match probe(Cursor::new(bytes.clone())) {
                Ok(_) => {}
                Err(e) => {
                    assert!(!e.code().is_empty());
                    errors += 1;
                }
            }
            decoded += run_session(&bytes);
            run_decoders(&bytes);
        }
    }
    // The corpus exercises both outcomes: some cases fail cleanly, many still decode pictures.
    assert!(errors > 0, "no mutation was rejected");
    assert!(
        decoded > CASES,
        "too few mutated cases decoded anything ({decoded})"
    );
}

#[test]
fn every_fixture_prefix_is_safe() {
    // Truncation at every 13th byte of each fixture: the demuxer ends early, decoders see partial packets.
    for base in FIXTURES {
        for len in (0..base.len()).step_by(13) {
            let _ = probe(Cursor::new(base[..len].to_vec()));
            run_session(&base[..len]);
        }
    }
}

/// The Opus packet gate's seed corpus (`fuzz/seeds/opus_packet/`, ADR-2306) and seeded mutations of
/// it: no panic, no sample outside [-1, 1], and every seed's planted lost-frame packet is kept away
/// from libopus's concealment: refused by the gate, or (a packet of one or two bytes, since the
/// conceal guard of a6786bd62) answered with exact silence of its duration (ADR-7635).
#[test]
fn opus_packet_seeds_and_mutations_are_safe() {
    const SEEDS: [(&str, &[u8], usize); 5] = [
        (
            "silk_then_tiny",
            include_bytes!("../fuzz/seeds/opus_packet/silk_then_tiny.bin"),
            2,
        ),
        (
            "silk_celt_silk",
            include_bytes!("../fuzz/seeds/opus_packet/silk_celt_silk.bin"),
            0,
        ),
        (
            "hybrid_codes",
            include_bytes!("../fuzz/seeds/opus_packet/hybrid_codes.bin"),
            0,
        ),
        (
            "code3_padding",
            include_bytes!("../fuzz/seeds/opus_packet/code3_padding.bin"),
            1,
        ),
        (
            "two_streams",
            include_bytes!("../fuzz/seeds/opus_packet/two_streams.bin"),
            1,
        ),
    ];
    for (name, seed, gated) in SEEDS {
        // At least the planted packets never reach libopus (it may refuse more of the synthetic
        // bytes). `opus_packet_run` itself panics if a packet it counts as silenced was not exact
        // silence of its duration.
        let run = fuzzing::opus_packet_run(seed);
        assert!(
            run.refused + run.silenced >= gated,
            "{name}: {} refused, {} silenced, {gated} planted",
            run.refused,
            run.silenced
        );
        assert_eq!(
            fuzzing::opus_packets_untrusted(seed),
            run.refused,
            "{name}: the counting entry agrees with the cargo-fuzz entry"
        );
    }
    // silk_then_tiny plants a TOC-only packet and a one-byte SILK frame after a real SILK packet:
    // both are silenced before the gate, and the gate on its own still refuses them.
    let tiny = fuzzing::opus_packet_run(SEEDS[0].1);
    assert_eq!(tiny.silenced, 2, "silk_then_tiny: {tiny:?}");
    for planted in [&[0x48u8][..], &[0x48, 0x5a]] {
        assert!(
            regia_media::opus::gate_packet(planted, 1).is_err(),
            "the gate refuses {planted:02x?}"
        );
    }
    let mut rng = Rng(0x0b05_5eed_0b05_5eed);
    for (i, (_, seed, _)) in SEEDS.iter().enumerate() {
        let other = SEEDS[(i + 1) % SEEDS.len()].1;
        for _ in 0..CASES {
            let bytes = mutate(seed, other, &mut rng);
            let _ = fuzzing::opus_packets_untrusted(&bytes);
        }
    }
}

/// Findings of the `decode` cargo-fuzz target stay fixed (ADR-2723). Each file is the input as libFuzzer minimized it. An
/// out-of-memory finding replays clean without the fuzzer's allocation limit, so those also have a test that measures
/// the allocation (`tests/vorbis_hostile_headers.rs`).
#[test]
fn committed_decode_findings_stay_fixed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/findings/decode");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no findings in {}", dir.display());
    for file in files {
        let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        run_session(&bytes);
        run_decoders(&bytes);
    }
}
