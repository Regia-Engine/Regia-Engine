// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit: the synthetic WebM writer. Fixtures that every other media test trusts must
//! have monotonic timestamps, round-trip every packet, and be byte-identical on a re-run.

use std::io::Cursor;

use regia_media::container::{Demuxer, MkvDemuxer, Packet};
use regia_media::mux::{
    MuxOptions, MuxPacket, MuxTrack, PatternClip, ToneAudio, pattern_frame, pattern_index,
};

fn read_all(bytes: &[u8]) -> (regia_media::MediaInfo, Vec<Packet>) {
    let mut demux = MkvDemuxer::open(Cursor::new(bytes.to_vec())).expect("fixture demuxes");
    let info = demux.info().clone();
    let mut packets = Vec::new();
    while let Some(p) = demux.next_packet().expect("clean fixture") {
        packets.push(p);
    }
    (info, packets)
}

fn tone_clip(frames: u32) -> PatternClip {
    let mut clip = PatternClip::new(16, 16, frames, 30);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    clip
}

#[test]
fn a_clip_is_byte_identical_on_two_runs() {
    let clip = tone_clip(12);
    assert_eq!(clip.to_webm(), clip.to_webm());
    assert_eq!(
        clip.to_webm_with_cues(&[(0, Some(500_000_000), "a")]),
        clip.to_webm_with_cues(&[(0, Some(500_000_000), "a")])
    );
}

#[test]
fn timestamps_are_monotonic_per_track_across_cluster_boundaries() {
    // Long enough that the relative timestamp leaves the i16 range several times (32.7 s at
    // 1 ms). No keyframes after frame 0, so mux can only open a later cluster through the
    // i16-overflow branch — keyframe-driven clustering is covered by the assertions below.
    let mut clip = tone_clip(3 * 32 * 30);
    clip.keyframe_every = clip.frames + 1;
    let (info, packets) = read_all(&clip.to_webm());
    assert_eq!(info.tracks.len(), 2);
    for track in [1u64, 2] {
        let mine: Vec<&Packet> = packets.iter().filter(|p| p.track == track).collect();
        assert_eq!(
            mine.len(),
            clip.frames as usize,
            "track {track} keeps every packet"
        );
        for pair in mine.windows(2) {
            assert!(
                pair[0].pts_ns < pair[1].pts_ns,
                "track {track}: {} then {}",
                pair[0].pts_ns,
                pair[1].pts_ns
            );
        }
    }
    let video: Vec<&Packet> = packets.iter().filter(|p| p.track == 1).collect();
    for (k, p) in video.iter().enumerate() {
        assert_eq!(
            p.pts_ns,
            clip.frame_pts_ns(k as u32),
            "frame {k} time is the quantized one"
        );
        assert_eq!(pattern_index(&p.data), k as u32, "frame {k} content");
        assert_eq!(p.keyframe, (k as u32).is_multiple_of(clip.keyframe_every));
    }
}

#[test]
fn quantization_never_moves_a_frame_by_more_than_half_a_tick_or_reorders_them() {
    // 24 fps has no whole-millisecond frame time.
    let clip = PatternClip::new(16, 16, 240, 24);
    let mut prev = -1i64;
    for k in 0..clip.frames {
        let exact = u64::from(k) * clip.frame_ns;
        let q = clip.frame_pts_ns(k);
        assert!((q - exact as i64).abs() <= 500_000, "frame {k}");
        assert_eq!(q % 1_000_000, 0);
        assert!(q > prev, "frame {k} is after frame {}", k.wrapping_sub(1));
        prev = q;
    }
    assert_eq!(clip.duration_ns(), 240 * clip.frame_ns);
}

#[test]
fn a_one_frame_and_a_zero_frame_clip_still_demux() {
    let (info, packets) = read_all(&PatternClip::new(16, 16, 1, 30).to_webm());
    assert_eq!(packets.len(), 1);
    assert_eq!(info.tracks.len(), 1);
    assert_eq!(packets[0].pts_ns, 0);
    assert!(packets[0].keyframe);

    let (_, none) = read_all(&PatternClip::new(16, 16, 0, 30).to_webm());
    assert!(none.is_empty());
}

#[test]
fn pattern_frames_are_distinct_per_index_and_sized_as_i420() {
    let (w, h) = (16u32, 8u32);
    // The content index is a 16-bit field (pattern_frame stores the low and high byte), so
    // indices wrap at 65_536 and frames repeat exactly every 65_536 indices.
    for k in [0u32, 1, 255, 256, 65_535] {
        let frame = pattern_frame(w, h, k);
        assert_eq!(frame.len(), (w * h * 3 / 2) as usize);
        assert_eq!(pattern_index(&frame), k);
    }
    assert_eq!(
        pattern_index(&pattern_frame(w, h, 70_000)),
        70_000 & 0xFFFF,
        "the index wraps at 16 bits"
    );
    assert_ne!(pattern_frame(w, h, 3), pattern_frame(w, h, 4));
}

#[test]
fn explicit_packets_round_trip_with_the_declared_options() {
    let track = MuxTrack {
        number: 1,
        kind: regia_media::container::TrackKind::Video,
        codec_id: regia_media::codec::RAW_VIDEO.to_string(),
        default_duration_ns: Some(33_000_000),
        language: "und".into(),
        default: true,
        video: Some((16, 16, Some("I420".into()))),
        audio: None,
    };
    let packets: Vec<MuxPacket> = (0..4)
        .map(|k| MuxPacket {
            track: 1,
            pts_ns: k * 33_000_000,
            keyframe: k == 0,
            data: pattern_frame(16, 16, k as u32),
        })
        .collect();
    let opts = MuxOptions {
        duration_ns: Some(132_000_000),
        ..MuxOptions::default()
    };
    let bytes = regia_media::mux::mux(&opts, &[track], &packets);
    let (info, read) = read_all(&bytes);
    assert_eq!(info.tracks.len(), 1);
    let got: Vec<(i64, bool, Vec<u8>)> = read.into_iter().map(|p| (p.pts_ns, p.keyframe, p.data)).collect();
    let want: Vec<(i64, bool, Vec<u8>)> = packets
        .iter()
        .map(|p| (p.pts_ns, p.keyframe, p.data.clone()))
        .collect();
    assert_eq!(got, want);
}
