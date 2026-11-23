// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the audio cue/patch loaders. See
//! `docs/evidence/m18/robustness.md`.

use regia_audio::{AudioPatchDef, SoundCueDef};
use regia_test_utils::fuzz::fuzz_text_loader;

const N: usize = 3_000;

#[test]
fn cue_ron_never_panics() {
    let fixture = include_str!("fixtures/impact_flesh.cue.ron");
    fuzz_text_loader(fixture, 0xF1, N, SoundCueDef::from_ron);
}

#[test]
fn patch_ron_never_panics() {
    let fixture = include_str!("fixtures/hover_engine.audiopatch.ron");
    fuzz_text_loader(fixture, 0xF2, N, AudioPatchDef::from_ron);
}

/// Finding 30's fuzz seed (ADR-4922): a 0.25 s WAV whose RIFF and data chunk sizes claim about
/// 2 GiB. Every mutation decodes whole through the bounded decoder (here capped at 60 s) or fails
/// with an error; none panics, hangs or allocates what the header claims.
#[test]
fn sound_bytes_decode_whole_within_the_limits() {
    fn decode(bytes: &[u8]) -> Result<f64, regia_audio::AudioError> {
        let limits = regia_audio::SoundDecodeLimits {
            max_secs: 60.0,
            max_bytes: 64 << 20,
        };
        regia_audio::SoundAsset::from_bytes_static_with_limits(bytes.to_vec(), "fuzz", &limits)
            .map(|s| s.duration_secs())
    }
    let fixture = include_bytes!("fixtures/hostile_length.wav");
    regia_test_utils::fuzz::fuzz_tolerant_loader(fixture, 0xF3, 1_000, decode);
}
