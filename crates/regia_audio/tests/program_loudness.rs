// SPDX-License-Identifier: MPL-2.0
//! The live program-loudness meter (`docs/specs/audio-loudness.md` § 6, ADR-7227) against the offline
//! loudness function, and the 5.1 channel weights of BS.1770-4.
//!
//! The live meter runs on the audio thread without allocating; these tests drive its tap directly with
//! the reference signals of the lane's acceptance (a 1 kHz sine and pink noise) and compare every
//! figure with [`LoudnessMeter`] fed the same samples.

use kira::Frame;
use regia_audio::program_loudness::ProgramLoudnessTap;
use regia_audio::{LoudnessMeter, measure_loudness};

const RATE: u32 = 48_000;

fn sine_frames(amp: f32, hz: f32, frames: usize) -> Vec<Frame> {
    (0..frames)
        .map(|i| {
            let s = amp * (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin();
            Frame { left: s, right: s }
        })
        .collect()
}

/// Deterministic pink noise: xorshift white noise through P. Kellet's three-pole "economy" pinking
/// filter (musicdsp.org, public domain). Independent generators per channel.
fn pink_frames(amp: f32, frames: usize) -> Vec<Frame> {
    struct Pink {
        state: u64,
        b: [f32; 3],
    }
    impl Pink {
        fn next(&mut self) -> f32 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            let white = (self.state >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0;
            self.b[0] = 0.99765 * self.b[0] + white * 0.099_046;
            self.b[1] = 0.963 * self.b[1] + white * 0.296_516_4;
            self.b[2] = 0.57 * self.b[2] + white * 1.052_691_3;
            self.b[0] + self.b[1] + self.b[2] + white * 0.1848
        }
    }
    let mut l = Pink {
        state: 0x9E37_79B9_7F4A_7C15,
        b: [0.0; 3],
    };
    let mut r = Pink {
        state: 0xD1B5_4A32_D192_ED03,
        b: [0.0; 3],
    };
    (0..frames)
        .map(|_| Frame {
            left: amp * l.next(),
            right: amp * r.next(),
        })
        .collect()
}

fn interleaved(frames: &[Frame]) -> Vec<f32> {
    frames.iter().flat_map(|f| [f.left, f.right]).collect()
}

/// Feeds `frames` to an enabled tap in 512-frame blocks (not a whole number of 100 ms hops) and
/// checks every figure against the offline meter on the same samples.
fn assert_live_matches_offline(frames: &[Frame], what: &str) {
    let (mut tap, handle) = ProgramLoudnessTap::new();
    handle.set_enabled(true);
    let dt = 1.0 / f64::from(RATE);
    for block in frames.chunks(512) {
        tap.process_block(block, dt);
    }
    let live = handle.read();

    let mut offline = LoudnessMeter::new(2, RATE).expect("stereo 48 kHz");
    offline.push(&interleaved(frames));
    let want = offline.finish();

    assert!(live.enabled, "{what}");
    assert!(
        (live.seconds - frames.len() as f64 / f64::from(RATE)).abs() < 1e-9,
        "{what}: {}",
        live.seconds
    );
    let close = |got: f32, want: f32, tol: f32, field: &str| {
        assert!(
            (got - want).abs() <= tol,
            "{what} {field}: live {got} vs offline {want} (tolerance {tol})"
        );
    };
    close(live.momentary_lufs, offline.momentary_lufs(), 0.01, "momentary");
    close(
        live.short_term_lufs,
        offline.short_term_lufs(),
        0.01,
        "short-term",
    );
    close(live.integrated_lufs, want.integrated_lufs, 0.01, "integrated");
    close(
        live.loudness_range_lu,
        want.loudness_range_lu,
        0.1,
        "loudness range",
    );
    close(live.true_peak_db, offline.true_peak_db(), 0.001, "true peak");
}

#[test]
fn the_live_meter_matches_the_offline_function_on_a_1_khz_sine() {
    // −20 dBFS on both channels, 10 s (a whole number of hops, so the windows line up).
    let frames = sine_frames(0.1, 1000.0, 10 * RATE as usize);
    assert_live_matches_offline(&frames, "1 kHz sine");
    let level = measure_loudness(&interleaved(&frames), 2, RATE)
        .expect("valid")
        .integrated_lufs;
    assert!(
        (level - -20.0).abs() < 0.2,
        "a −20 dBFS 1 kHz sine on two channels reads {level} LUFS"
    );
}

#[test]
fn the_live_meter_matches_the_offline_function_on_pink_noise() {
    let frames = pink_frames(0.05, 12 * RATE as usize);
    assert_live_matches_offline(&frames, "pink noise");
}

#[test]
fn a_level_step_matches_the_offline_gates_and_range() {
    // 10 s at −20 dBFS then 10 s at −40 dBFS: the relative gate and the loudness range both act.
    let mut frames = sine_frames(0.1, 1000.0, 10 * RATE as usize);
    frames.extend(sine_frames(0.01, 1000.0, 10 * RATE as usize));
    assert_live_matches_offline(&frames, "level step");
}

#[test]
fn reset_starts_a_new_measurement() {
    let (mut tap, handle) = ProgramLoudnessTap::new();
    handle.set_enabled(true);
    let dt = 1.0 / f64::from(RATE);
    tap.process_block(&sine_frames(0.5, 1000.0, 2 * RATE as usize), dt);
    let loud = handle.read();
    assert!(loud.integrated_lufs > -10.0, "{loud:?}");
    handle.reset();
    let quiet = sine_frames(0.01, 1000.0, 2 * RATE as usize);
    tap.process_block(&quiet, dt);
    let after = handle.read();
    let mut offline = LoudnessMeter::new(2, RATE).expect("meter");
    offline.push(&interleaved(&quiet));
    assert!((after.seconds - 2.0).abs() < 1e-9, "{after:?}");
    assert!(
        (after.integrated_lufs - offline.finish().integrated_lufs).abs() < 0.01,
        "{after:?}"
    );
    assert!(
        (after.true_peak_db - offline.true_peak_db()).abs() < 0.001,
        "{after:?}"
    );
}

#[test]
fn a_disabled_meter_measures_nothing() {
    let (mut tap, handle) = ProgramLoudnessTap::new();
    tap.process_block(&sine_frames(0.5, 1000.0, RATE as usize), 1.0 / f64::from(RATE));
    let r = handle.read();
    assert!(!r.enabled);
    assert_eq!(r.seconds, 0.0);
    assert_eq!(r.integrated_lufs, f32::NEG_INFINITY);
    assert_eq!(r.true_peak_db, -100.0);
}

#[test]
fn non_finite_samples_read_as_silence_and_the_meter_recovers() {
    let (mut tap, handle) = ProgramLoudnessTap::new();
    handle.set_enabled(true);
    let dt = 1.0 / f64::from(RATE);
    let mut frames = sine_frames(0.1, 1000.0, 4 * RATE as usize);
    frames[100] = Frame {
        left: f32::NAN,
        right: f32::INFINITY,
    };
    for block in frames.chunks(480) {
        tap.process_block(block, dt);
    }
    let r = handle.read();
    assert!(r.integrated_lufs.is_finite() && r.short_term_lufs.is_finite() && r.true_peak_db.is_finite());
    assert!((r.short_term_lufs - -20.0).abs() < 0.3, "{r:?}");
}

/// One channel of a 6-channel frame set carries a −20 dBFS 1 kHz tone, the others silence.
fn five_one_tone(channel: usize, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * 6];
    for i in 0..frames {
        out[i * 6 + channel] = 0.1 * (std::f32::consts::TAU * 1000.0 * i as f32 / RATE as f32).sin();
    }
    out
}

#[test]
fn five_one_channels_carry_the_bs_1770_weights() {
    // BS.1770-4 Table 3, in the order L, R, C, LFE, Ls, Rs: 1.0, 1.0, 1.0, excluded, 1.41, 1.41.
    let meter = LoudnessMeter::new(6, RATE).expect("5.1 is supported");
    assert_eq!(meter.channels(), 6);
    assert_eq!(
        regia_audio::channel_weights(6),
        Some(&[1.0, 1.0, 1.0, 0.0, 1.41, 1.41][..])
    );

    let frames = 5 * RATE as usize;
    let level = |channel: usize| -> f32 {
        measure_loudness(&five_one_tone(channel, frames), 6, RATE)
            .expect("valid")
            .integrated_lufs
    };
    let front = level(0);
    let mono = measure_loudness(
        &five_one_tone(0, frames)
            .iter()
            .step_by(6)
            .copied()
            .collect::<Vec<_>>(),
        1,
        RATE,
    )
    .expect("valid")
    .integrated_lufs;
    assert!(
        (front - mono).abs() < 1e-3,
        "a front channel weighs 1.0 ({front} vs mono {mono})"
    );
    assert!((level(1) - front).abs() < 1e-3, "R weighs 1.0");
    assert!((level(2) - front).abs() < 1e-3, "C weighs 1.0");
    assert_eq!(level(3), f32::NEG_INFINITY, "the LFE channel is excluded");
    let surround = 10.0 * 1.41f32.log10();
    assert!((level(4) - front - surround).abs() < 1e-3, "Ls is +{surround} LU");
    assert!((level(5) - front - surround).abs() < 1e-3, "Rs is +{surround} LU");

    // The LFE still counts toward the true peak (Annex 2 measures every channel).
    let lfe = measure_loudness(&five_one_tone(3, frames), 6, RATE).expect("valid");
    assert!(lfe.true_peak_db > -21.0 && lfe.true_peak_db < -19.0, "{lfe:?}");
}

#[test]
fn other_channel_counts_are_still_refused() {
    for channels in [0u8, 3, 4, 5, 7, 8] {
        assert_eq!(
            LoudnessMeter::new(channels, RATE).err(),
            Some(regia_audio::LoudnessError::UnsupportedChannels(channels)),
            "{channels} channels"
        );
    }
}
