// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 4.11 and § 2.6 (pure parts): the `.rfdrec` format and the playout buffer.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_agent::dcc_feed::playout::{PlayoutBuffer, Sample, slerp};
use regia_agent::dcc_feed::recording::{FORMAT_VERSION, Recording, RecordingError};
use regia_agent::dcc_feed::wire::*;

fn sample_recording() -> Recording {
    let mut rec = Recording::new("ref", "Blender");
    let mut f = Vec::new();
    encode(&Message::ItemDelete(1), 1, &mut f).unwrap();
    rec.push(0, f.clone());
    rec.push(6, f);
    rec
}

#[test]
fn recording_round_trips_through_a_file() {
    let rec = sample_recording();
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_rec");
    let path = dir.join("a.rfdrec");
    rec.write_file(&path).unwrap();
    assert!(
        !dir.join("a.rfdrec.tmp").exists(),
        "the temporary file is renamed away"
    );
    assert_eq!(Recording::read_file(&path).unwrap(), rec);
    assert_eq!(
        rec.dump_lines(&Limits::default()),
        vec![
            "tick=0 type=0x0011 handle=1 size=16".to_owned(),
            "tick=6 type=0x0011 handle=1 size=16".to_owned(),
        ]
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn recording_header_is_pinned() {
    let bytes = sample_recording().to_bytes().unwrap();
    assert_eq!(&bytes[..4], b"RFDR");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        FORMAT_VERSION
    );
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        PROTOCOL_VERSION
    );
}

#[test]
fn corrupt_recordings_are_refused_not_panicked() {
    let bytes = sample_recording().to_bytes().unwrap();
    for n in 0..bytes.len() {
        assert!(
            matches!(
                Recording::from_bytes(&bytes[..n]),
                Err(RecordingError::Corrupt(_))
            ),
            "prefix {n}"
        );
    }
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(Recording::from_bytes(&bad).unwrap_err().code(), "replay_corrupt");
    let mut newer = bytes.clone();
    newer[4..8].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        Recording::from_bytes(&newer).unwrap_err(),
        RecordingError::TooNew(2)
    );
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Recording::from_bytes(&trailing).is_err());
    // A record count far above the file size is refused before allocating.
    let mut huge = bytes;
    let at = 4 + 4 + 4 + 2 + 3 + 2 + 7;
    huge[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(Recording::from_bytes(&huge).is_err());
    assert_eq!(
        Recording::read_file(std::path::Path::new("/nonexistent/x.rfdrec"))
            .unwrap_err()
            .code(),
        "replay_not_found"
    );
}

fn at(frame_id: u64, tick: f64, x: f32, r: [f32; 4]) -> Sample {
    Sample {
        frame_id,
        time_s: tick,
        t: [x, 0.0, 0.0],
        r,
        s: [1.0; 3],
    }
}

#[test]
fn delayed_playout_midpoint_is_exact() {
    // Frames at ticks 0, 6, 12; delay 3 ticks; evaluated at tick 9 -> time 6... mapped: now 9, playout
    // time = 9 - 3 = 6 is a sample, so use tick 6 + delay: evaluate at 3 -> midpoint of frames 0 and 6.
    let q = std::f32::consts::FRAC_1_SQRT_2;
    let mut b = PlayoutBuffer::new(64, 100.0);
    assert!(b.push(at(1, 0.0, 0.0, [0.0, 0.0, 0.0, 1.0])));
    assert!(b.push(at(2, 6.0, 6.0, [0.0, 0.0, q, q])));
    assert!(b.push(at(3, 12.0, 12.0, [0.0, 0.0, 1.0, 0.0])));
    let now = 6.0;
    let delay = 3.0;
    let s = b.evaluate(now - delay).unwrap();
    assert!((s.t[0] - 3.0).abs() < 1e-6);
    // Midpoint rotation is 22.5 degrees about z.
    let half = std::f32::consts::FRAC_PI_8;
    let want = [0.0, 0.0, half.sin(), half.cos()];
    for i in 0..4 {
        assert!((s.r[i] - want[i]).abs() < 1e-6, "{:?} vs {want:?}", s.r);
    }
    // Before the first / after the last sample clamp.
    assert_eq!(b.evaluate(-5.0).unwrap().t[0], 0.0);
    assert_eq!(b.evaluate(50.0).unwrap().t[0], 12.0);
}

#[test]
fn slerp_takes_the_short_way() {
    let a = [0.0, 0.0, 0.0, 1.0];
    let neg = [0.0, 0.0, 0.0, -1.0];
    let m = slerp(a, neg, 0.5);
    assert!((m[3].abs() - 1.0).abs() < 1e-6);
}

#[test]
fn out_of_order_age_and_cap_are_counted() {
    let id = [0.0, 0.0, 0.0, 1.0];
    let mut b = PlayoutBuffer::new(3, 2.0);
    assert!(b.push(at(5, 0.0, 0.0, id)));
    assert!(!b.push(at(5, 1.0, 0.0, id)), "equal id dropped");
    assert!(!b.push(at(4, 1.0, 0.0, id)), "older id dropped");
    assert_eq!(b.counters().dropped_out_of_order, 2);
    for i in 6..=9u64 {
        b.push(at(i, i as f64, 0.0, id));
    }
    assert_eq!(b.len(), 3);
    assert_eq!(b.counters().evicted_cap, 2);
    b.evict(20.0);
    assert_eq!(b.len(), 1, "keeps the newest sample as bracket start");
    assert!(b.counters().evicted_age >= 2);
    assert_eq!(b.newest().unwrap().frame_id, 9);
}
