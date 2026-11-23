// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 30 (ADR-3206; fixed under ADR-4922): audio was decoded whole to PCM with no
//! size or duration limit. Every whole-file decode (a static sound, a stream decoded to memory, a
//! loudness measurement) now counts the frames it produced and stops at `SoundDecodeLimits`.

use regia_audio::cooked_sound::CookedSound;
use regia_audio::{
    AudioError, MAX_DECODED_SOUND_BYTES, MAX_DECODED_SOUND_SECS, REFERENCE_LUFS, SoundAsset, SoundAssets,
    SoundDecodeLimits, SoundLoudness,
};

/// A PCM WAV: 16-bit mono at `rate`, `frames` samples of a quiet ramp.
fn wav(rate: u32, frames: u32) -> Vec<u8> {
    let data_len = frames * 2;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        out.extend_from_slice(&((i % 2000) as i16).to_le_bytes());
    }
    out
}

fn limit_of(result: Result<SoundAsset, AudioError>) -> Option<(u64, u32)> {
    match result {
        Err(AudioError::DecodeLimit {
            max_frames,
            sample_rate,
            ..
        }) => Some((max_frames, sample_rate)),
        _ => None,
    }
}

#[test]
fn the_default_limits_are_the_documented_ones() {
    let limits = SoundDecodeLimits::default();
    assert_eq!(limits.max_secs, MAX_DECODED_SOUND_SECS);
    assert_eq!(limits.max_bytes, MAX_DECODED_SOUND_BYTES);
    assert_eq!(
        (MAX_DECODED_SOUND_SECS, MAX_DECODED_SOUND_BYTES),
        (1800.0, 1 << 30)
    );
}

/// Finding 30: a sound longer than the duration cap stops with `DecodeLimit`; one at the cap loads.
#[test]
fn finding_30_a_sound_past_the_duration_cap_is_refused() {
    let one_second = wav(8_000, 8_000);
    let half = SoundDecodeLimits {
        max_secs: 0.5,
        ..SoundDecodeLimits::default()
    };
    let refused = SoundAsset::from_bytes_static_with_limits(one_second.clone(), "a.wav", &half);
    assert_eq!(limit_of(refused), Some((4_000, 8_000)));
    let exact = SoundDecodeLimits {
        max_secs: 1.0,
        ..SoundDecodeLimits::default()
    };
    let sound =
        SoundAsset::from_bytes_static_with_limits(one_second.clone(), "a.wav", &exact).expect("at the cap");
    assert!((sound.duration_secs() - 1.0).abs() < 1e-9);
    let sound = SoundAsset::from_bytes_static_with_limits(one_second, "a.wav", &SoundDecodeLimits::default())
        .expect("the defaults");
    assert_eq!(sound.sample_rate(), 8_000);
}

/// Finding 30: the byte cap binds on its own (8 bytes per decoded frame).
#[test]
fn finding_30_a_sound_past_the_size_cap_is_refused() {
    let bytes = SoundDecodeLimits {
        max_secs: f64::INFINITY,
        max_bytes: 8 * 1_000,
    };
    let refused = SoundAsset::from_bytes_static_with_limits(wav(48_000, 1_001), "b.wav", &bytes);
    assert_eq!(limit_of(refused), Some((1_000, 48_000)));
    assert!(SoundAsset::from_bytes_static_with_limits(wav(48_000, 1_000), "b.wav", &bytes).is_ok());
    let message = SoundAsset::from_bytes_static_with_limits(wav(48_000, 1_001), "b.wav", &bytes)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        message.contains("b.wav") && message.contains("limit"),
        "{message}"
    );
}

/// Finding 30: every whole-file path takes the limits: a file, a stream decoded to memory, and a
/// loudness measurement.
#[test]
fn finding_30_every_whole_file_decode_is_bounded() {
    let dir = regia_test_utils::unique_temp_dir("audio-decode-limits");
    let path = dir.join("tone.wav");
    std::fs::write(&path, wav(8_000, 8_000)).expect("wav");
    let tight = SoundDecodeLimits {
        max_secs: 0.25,
        ..SoundDecodeLimits::default()
    };
    assert_eq!(
        limit_of(SoundAsset::from_file_static_with_limits(&path, &tight)),
        Some((2_000, 8_000))
    );
    assert!(SoundAsset::from_file_static(&path).is_ok());
    let stream =
        SoundAsset::streaming_from_bytes(std::sync::Arc::new(wav(8_000, 8_000)), "s.wav").expect("stream");
    assert_eq!(
        limit_of(stream.decode_to_memory_with_limits(&tight)),
        Some((2_000, 8_000))
    );
    assert!(!stream.decode_to_memory().expect("decodes").is_streaming());
    let measured = regia_audio::measure_sound_bytes_with_limits(wav(8_000, 8_000), "m.wav", &tight);
    assert!(
        matches!(measured, Err(AudioError::DecodeLimit { .. })),
        "{measured:?}"
    );
    assert!(regia_audio::measure_sound_bytes(wav(8_000, 8_000), "m.wav").is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bounded decoder gives the same sound as before for the shipped formats (FLAC, Ogg Vorbis,
/// MP3 fixtures): it decodes them whole within the defaults.
#[test]
fn the_fixture_formats_still_decode() {
    for name in ["tone_1k.flac", "tone_1k.ogg", "tone_1k.mp3"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let sound = SoundAsset::from_file_static(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(sound.duration_secs() > 0.0, "{name}");
    }
}

/// A silent 16-bit mono WAV. Used when `frames` is too large for the per-sample helper.
fn wav_silence(rate: u32, frames: u32) -> Vec<u8> {
    let data_len = frames * 2;
    let mut out = vec![0u8; 44 + data_len as usize];
    out[0..4].copy_from_slice(b"RIFF");
    out[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    out[8..16].copy_from_slice(b"WAVEfmt ");
    out[16..20].copy_from_slice(&16u32.to_le_bytes());
    out[20..22].copy_from_slice(&1u16.to_le_bytes());
    out[22..24].copy_from_slice(&1u16.to_le_bytes());
    out[24..28].copy_from_slice(&rate.to_le_bytes());
    out[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    out[32..34].copy_from_slice(&2u16.to_le_bytes());
    out[34..36].copy_from_slice(&16u16.to_le_bytes());
    out[36..40].copy_from_slice(b"data");
    out[40..44].copy_from_slice(&data_len.to_le_bytes());
    out
}

fn cooked_pcm(rate: u32, frames: u32) -> Vec<u8> {
    let loudness = SoundLoudness {
        integrated_lufs: None,
        loudness_range_lu: 0.0,
        true_peak_db: None,
        short_clip: true,
        dual_mono: true,
        sample_rate: rate,
        frames: u64::from(frames),
        normalize: false,
        target_lufs: REFERENCE_LUFS,
        gain_db: 0.0,
        peak_limited: false,
    };
    CookedSound {
        loudness,
        data: wav_silence(rate, frames),
    }
    .to_bytes()
    .expect("cook")
}

/// Finding 30: a pak entry is a whole-file decode. One frame past the default duration cap is
/// refused; a one-second cooked sound still loads and keeps its loudness record.
#[test]
fn finding_30_a_packed_sound_past_the_duration_cap_is_refused() {
    let over_frames = (MAX_DECODED_SOUND_SECS as u32) * 8_000 + 1;
    let over = cooked_pcm(8_000, over_frames);
    let mut sounds = SoundAssets::default();
    match sounds.add_packed("long", "assets/long.wav", &over) {
        Err(AudioError::DecodeLimit {
            path,
            max_frames,
            sample_rate,
        }) => {
            assert_eq!(path, "assets/long.wav");
            assert_eq!((max_frames, sample_rate), (1800 * 8_000, 8_000));
        }
        other => panic!("expected DecodeLimit, got {other:?}"),
    }
    assert!(sounds.by_name("long").is_none());
    assert!(
        sounds
            .packed_loudness_table()
            .entry_for("assets/long.wav")
            .is_none()
    );

    let under = cooked_pcm(8_000, 8_000);
    let handle = sounds
        .add_packed("short", "assets/short.wav", &under)
        .expect("a short cooked sound loads");
    let sound = sounds.get(&handle).expect("registered");
    assert!((sound.duration_secs() - 1.0).abs() < 1e-6);
    assert_eq!(sound.sample_rate(), 8_000);
    assert!(
        sounds
            .packed_loudness_table()
            .entry_for("assets/short.wav")
            .is_some()
    );
}

/// The fuzz seed: a WAV whose headers claim about 2 GiB decodes what it holds (or fails), and
/// never allocates what the header claims.
#[test]
fn a_header_that_claims_gigabytes_decodes_what_is_there() {
    let seed = include_bytes!("fixtures/hostile_length.wav").to_vec();
    match SoundAsset::from_bytes_static_with_limits(seed, "hostile.wav", &SoundDecodeLimits::default()) {
        Ok(sound) => assert!(sound.duration_secs() <= 0.25 + 1e-9, "{}", sound.duration_secs()),
        Err(e) => assert!(matches!(e, AudioError::LoadFailed { .. }), "{e}"),
    }
}

/// A WAV header may say 0 Hz (the WAV reader does not refuse it). An empty sound at 0 Hz used to
/// reach `Duration::from_secs_f64(0 / 0)` and panic the loader; a sound with frames at 0 Hz was
/// reported as past the decode limit. Both are now a load error for an unknown sample rate.
#[test]
fn a_zero_hz_wav_is_a_load_error_not_a_panic() {
    for frames in [0, 4] {
        let bytes = wav(0, frames);
        let result = SoundAsset::from_bytes_static_with_limits(
            bytes.clone(),
            "zero.wav",
            &SoundDecodeLimits::default(),
        );
        assert!(
            matches!(result, Err(AudioError::LoadFailed { .. })),
            "{frames} frames at 0 Hz: {:?}",
            result.map(|sound| sound.duration_secs())
        );
        let measured =
            regia_audio::measure_sound_bytes_with_limits(bytes, "zero.wav", &SoundDecodeLimits::default());
        assert!(
            matches!(measured, Err(AudioError::LoadFailed { .. })),
            "{frames} frames: {measured:?}"
        );
    }
}
