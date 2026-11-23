// SPDX-License-Identifier: MPL-2.0
//! The fuzzing entry point shared by `tests/fuzz.rs` (the CI corpus) and the `cargo fuzz` target in
//! `crates/regia_media/fuzz/` (ADR-1593 § 4, ADR-1869 § 8): untrusted bytes through probe, a full decode
//! session with audio, and the caps checks on every picture.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::container::probe;
use crate::session::{AudioChoice, AudioSink, FeedPoll, Session, SessionOptions};

/// Upper bound on pictures pulled from one input (fixtures hold a few dozen at most).
pub const MAX_POLLS: usize = 4096;

/// What one input produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuzzOutcome {
    /// `probe` accepted the file.
    pub probed: bool,
    /// A session opened.
    pub opened: bool,
    /// Pictures decoded.
    pub pictures: usize,
    /// Audio samples delivered to the sink.
    pub samples: usize,
}

/// Counts delivered samples into a shared counter and checks their range.
struct CountSink(Arc<AtomicUsize>);

impl AudioSink for CountSink {
    fn push(&mut self, _pts_ns: i64, samples: &[f32]) {
        assert!(
            samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0),
            "decoded audio left [-1, 1]"
        );
        self.0.fetch_add(samples.len(), Ordering::Relaxed);
    }

    fn flush(&mut self) {}
}

/// Runs untrusted bytes through probe and a whole session. Panics only on a broken invariant (a picture
/// over the caps, audio outside [-1, 1], or a session that never ends), which is what a fuzzer reports.
pub fn decode_untrusted(bytes: &[u8]) -> FuzzOutcome {
    let mut out = FuzzOutcome {
        probed: probe(Cursor::new(bytes.to_vec())).is_ok(),
        ..FuzzOutcome::default()
    };
    let opts = SessionOptions {
        audio: AudioChoice::Default,
        ..SessionOptions::default()
    };
    let samples = Arc::new(AtomicUsize::new(0));
    let sink = CountSink(samples.clone());
    let Ok(mut session) = Session::open(Cursor::new(bytes.to_vec()), &opts, Some(Box::new(sink))) else {
        return out;
    };
    out.opened = true;
    for _ in 0..MAX_POLLS {
        match session.next_picture() {
            FeedPoll::Frame(p) => {
                assert!(
                    opts.caps.check_size(p.width, p.height).is_ok(),
                    "picture over the size cap"
                );
                assert!(
                    p.byte_size() <= opts.caps.max_output_bytes,
                    "picture over the output cap"
                );
                out.pictures += 1;
            }
            FeedPoll::End | FeedPoll::Failed(_) | FeedPoll::Pending => {
                out.samples = samples.load(Ordering::Relaxed);
                return out;
            }
        }
    }
    panic!("session did not end within {MAX_POLLS} pictures");
}

/// Untrusted bytes as a run of Opus packets into one decoder, through the packet gate (ADR-1911,
/// ADR-2306). The first byte picks one to three coded streams (family 0 mono for one, family 1 with
/// one channel per stream otherwise); then each packet is a length byte and that many bytes. Returns
/// how many packets the decoder refused. Panics when decoded audio leaves [-1, 1], which is what a
/// fuzzer reports. `crates/regia_media/fuzz/seeds/opus_packet/` holds the seed corpus.
/// [`opus_packet_run`] also counts the packets the decoder turned into silence.
#[cfg(feature = "opus")]
pub fn opus_packets_untrusted(bytes: &[u8]) -> usize {
    opus_packet_run(bytes).refused
}

/// What [`opus_packet_run`] did with each packet that libopus never decoded (ADR-7635).
#[cfg(feature = "opus")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpusPacketRun {
    /// Packets the decoder refused (the packet gate, or a duration no Opus packet can have).
    pub refused: usize,
    /// Packets of one or two bytes, or with a zero-length frame, that the decoder answered with
    /// exact silence of their duration instead of running libopus's packet-loss concealment.
    pub silenced: usize,
}

/// [`opus_packets_untrusted`] with both counts. Panics when decoded audio leaves [-1, 1], and when a
/// packet the decoder must answer with silence (`silenced`) produced anything but zeros of its
/// duration, which would mean libopus concealed it after all.
#[cfg(feature = "opus")]
pub fn opus_packet_run(bytes: &[u8]) -> OpusPacketRun {
    use crate::codec::AudioDecoder;
    let mut run = OpusPacketRun::default();
    let Some((&selector, mut rest)) = bytes.split_first() else {
        return run;
    };
    let streams = 1 + selector % 3;
    let mut head = b"OpusHead".to_vec();
    if streams == 1 {
        head.extend_from_slice(&[1, 1, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
    } else {
        head.extend_from_slice(&[1, streams, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 1, streams, 0]);
        head.extend(0..streams);
    }
    let Ok(parsed) = crate::opus::OpusHead::parse(&head) else {
        panic!("the fuzz entry's own OpusHead must parse");
    };
    // One channel per stream and no pre-skip, so a silenced packet is `samples * streams` zeros.
    let channels = usize::from(parsed.channels);
    let Ok(mut decoder) = crate::opus::OpusDecoder::from_head(parsed) else {
        return run;
    };
    let mut out = Vec::new();
    for index in 0..MAX_POLLS {
        let Some((&len, tail)) = rest.split_first() else {
            break;
        };
        let take = usize::from(len).min(tail.len());
        let packet = crate::container::Packet {
            track: 1,
            pts_ns: index as i64 * 20_000_000,
            duration_ns: None,
            keyframe: true,
            data: tail[..take].to_vec(),
        };
        rest = &tail[take..];
        out.clear();
        let silent = crate::opus::concealed_samples(&packet.data, streams)
            .ok()
            .flatten();
        let decoded = decoder.decode(&packet, &mut out);
        assert!(
            out.iter().all(|v| v.is_finite() && v.abs() <= 1.0),
            "decoded audio left [-1, 1]"
        );
        match (decoded, silent) {
            (Err(_), _) => run.refused += 1,
            (Ok(()), Some(samples)) => {
                assert!(
                    out.len() == samples * channels && out.iter().all(|v| *v == 0.0),
                    "packet {index} ({take} bytes) must decode as {samples} samples of silence"
                );
                run.silenced += 1;
            }
            (Ok(()), None) => {}
        }
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{PatternClip, ToneAudio};

    #[test]
    fn clean_and_garbage_inputs() {
        let mut clip = PatternClip::new(16, 8, 5, 25);
        clip.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 1,
            freq_hz: 440.0,
            amplitude: 0.5,
        });
        let ok = decode_untrusted(&clip.to_webm());
        assert!(ok.probed && ok.opened);
        assert_eq!(ok.pictures, 5);
        assert!(ok.samples > 0);
        let junk = decode_untrusted(b"\x1a\x45\xdf\xa3 not a webm");
        assert!(!junk.opened);
        assert_eq!(junk.pictures, 0);
    }
}
