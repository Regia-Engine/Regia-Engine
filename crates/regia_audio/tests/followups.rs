// SPDX-License-Identifier: MPL-2.0
//! Audio follow-up tests: bus ducking through the mixer, cue filters end to end, loop seams, and
//! the mixer under voice-stream dropouts.
//!
//! Everything here renders through the offline backend on static (in-memory) sounds, so there is
//! no audio device, no sleeping and no wall-clock assertion: time is the number of frames
//! rendered.

use std::sync::Arc;
use std::time::Duration;

use regia_audio::{
    Audio, AudioSettings, CueEntry, DuckEngine, DuckRule, LoopGenDef, LoopRegion, PlaybackParams, SoundAsset,
    SoundClip, SoundCue,
};

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;
const FRAMES_PER_TICK: usize = (RATE / 60) as usize;

fn headless_with(settings: AudioSettings) -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..settings
    })
}

fn headless() -> Audio {
    headless_with(AudioSettings::default())
}

/// A one-second sine clip (whole cycles for the round frequencies used here).
fn tone(hz: f32, amp: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..RATE as usize)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

/// A one-second clip with a 200 Hz body and an 8 kHz edge.
fn body_and_edge() -> SoundAsset {
    let samples: Vec<f32> = (0..RATE as usize)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            (t * 200.0 * std::f32::consts::TAU).sin() * 0.2
                + (t * 8_000.0 * std::f32::consts::TAU).sin() * 0.2
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("clip"))
}

fn looping(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

fn left(frames: &[f32]) -> Vec<f32> {
    frames.iter().step_by(2).copied().collect()
}

/// Goertzel magnitude at `hz` over the left channel (a sine of peak `a` reads about `a / 2`).
fn level(frames: &[f32], hz: f32) -> f32 {
    let samples = left(frames);
    let coeff = 2.0 * (2.0 * std::f32::consts::PI * hz / RATE as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in &samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len().max(1) as f32
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
}

/// Ticks and renders `seconds` in 1/60 s steps, as fast as the CPU allows (static sounds only).
fn run(audio: &mut Audio, seconds: f64) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..(seconds / FRAME).round() as usize {
        audio.tick(FRAME);
        out.extend_from_slice(&audio.render_offline(FRAMES_PER_TICK).expect("offline render"));
    }
    out
}

// ----------------------------------------------------------------------------- bus ducking

fn sfx_duck_rule() -> DuckRule {
    DuckRule {
        when: "Ambience".into(),
        target: "Sfx".into(),
        amount_db: -12.0,
        attack: Duration::from_millis(50),
        release: Duration::from_millis(100),
    }
}

/// A bus-driven rule ducks the target bus by its amount while the source bus has a voice, and
/// releases once the voice is gone; the source bus itself is untouched.
#[test]
fn a_bus_duck_rule_lowers_the_target_bus_and_releases() {
    let mut audio = headless_with(AudioSettings {
        duck_rules: vec![sfx_duck_rule()],
        ..AudioSettings::default()
    });
    audio
        .start_asset(&tone(440.0, 0.4), looping("Sfx"))
        .expect("sfx loop");
    let _ = run(&mut audio, 0.2);
    let baseline = level(&run(&mut audio, 0.3), 440.0);
    assert!(baseline > 0.05, "the Sfx loop is audible: {baseline}");

    let trigger = audio
        .start_asset(&tone(1_000.0, 0.4), looping("Ambience"))
        .expect("ambience loop");
    let _ = run(&mut audio, 0.3); // attack (50 ms) plus settling
    let ducked_run = run(&mut audio, 0.3);
    let ducked = level(&ducked_run, 440.0);
    let ambience = level(&ducked_run, 1_000.0);
    let measured = db(ducked / baseline);
    assert!(
        (measured + 12.0).abs() < 1.5,
        "Sfx is about 12 dB down while Ambience plays: {measured:.2} dB"
    );
    assert!(ambience > 0.05, "the source bus is not ducked: {ambience}");

    audio.stop_voice(trigger, Duration::from_millis(10));
    let _ = run(&mut audio, 0.5); // fade, removal, 100 ms release
    let released = level(&run(&mut audio, 0.3), 440.0);
    assert!(
        db(released / baseline).abs() < 1.0,
        "the duck releases: {:.2} dB from baseline",
        db(released / baseline)
    );
}

/// The envelope is a function of elapsed time, not of the tick size: ten 20 ms ticks land where
/// one 200 ms tick does.
#[test]
fn duck_envelope_does_not_depend_on_the_tick_size() {
    let rules = vec![DuckRule {
        when: "Voice".into(),
        target: "Music".into(),
        amount_db: -8.0,
        attack: Duration::from_millis(400),
        release: Duration::from_millis(800),
    }];
    let active: std::collections::HashMap<String, u32> = [("Voice".to_string(), 1)].into();
    let mut coarse = DuckEngine::new(&rules);
    let mut fine = DuckEngine::new(&rules);
    let coarse_offsets = coarse.tick(0.2, &rules, &active);
    let mut fine_offsets = std::collections::HashMap::new();
    for _ in 0..10 {
        fine_offsets = fine.tick(0.02, &rules, &active);
    }
    assert!(
        (coarse_offsets["Music"] - fine_offsets["Music"]).abs() < 1e-4,
        "{coarse_offsets:?} vs {fine_offsets:?}"
    );
    assert!(
        (coarse.envelope(0) - 0.5).abs() < 1e-4,
        "half the attack: {}",
        coarse.envelope(0)
    );
}

/// A release that starts mid-attack continues from the current envelope, with no jump to full
/// reduction or to zero.
#[test]
fn releasing_mid_attack_continues_from_the_current_envelope() {
    let rules = vec![DuckRule {
        when: "Voice".into(),
        target: "Music".into(),
        amount_db: -10.0,
        attack: Duration::from_millis(200),
        release: Duration::from_millis(400),
    }];
    let mut engine = DuckEngine::new(&rules);
    let mut active: std::collections::HashMap<String, u32> = [("Voice".to_string(), 1)].into();
    let _ = engine.tick(0.1, &rules, &active); // envelope 0.5
    active.clear();
    let offsets = engine.tick(0.1, &rules, &active); // release 0.1 / 0.4 = 0.25
    assert!((engine.envelope(0) - 0.25).abs() < 1e-5, "{}", engine.envelope(0));
    assert!((offsets["Music"] + 2.5).abs() < 1e-4, "{offsets:?}");
}

// ----------------------------------------------------------------------------- cue filters

fn filtered_cue(name: &str, cutoff: Option<f32>, offset: f32) -> SoundCue {
    let mut cue = SoundCue::new(
        name,
        vec![CueEntry {
            asset: body_and_edge(),
            weight: 1.0,
        }],
    );
    cue.filter_cutoff_hz = cutoff;
    cue.filter_offset_octaves = offset;
    cue.bus = Some("Sfx".into());
    cue
}

fn cue_spectrum(cue: SoundCue) -> (f32, f32) {
    let name = cue.name.clone();
    let mut audio = headless();
    audio.register_cue(cue);
    audio
        .play_cue_seeded(&name, 7)
        .expect("cue plays")
        .expect("not refused");
    let _ = run(&mut audio, 0.1);
    let heard = run(&mut audio, 0.3);
    (level(&heard, 200.0), level(&heard, 8_000.0))
}

/// A cue with a 400 Hz low-pass removes the 8 kHz edge and keeps the 200 Hz body; a cue with no
/// filter, and a cue whose offset opens the filter past 20 kHz, keep both.
#[test]
fn a_cue_filter_shapes_the_played_spectrum() {
    let (open_body, open_edge) = cue_spectrum(filtered_cue("open", None, 0.0));
    assert!(open_edge > 0.03, "unfiltered cue keeps the edge: {open_edge}");

    let (body, edge) = cue_spectrum(filtered_cue("dark", Some(400.0), 0.0));
    assert!(
        db(edge / open_edge) < -20.0,
        "a 400 Hz cutoff removes the 8 kHz edge: {:.1} dB",
        db(edge / open_edge)
    );
    assert!(
        db(body / open_body).abs() < 3.0,
        "the body passes: {body} vs {open_body}"
    );

    // 400 Hz opened by +6 octaves is 25.6 kHz, clamped to fully open.
    let (wide_body, wide_edge) = cue_spectrum(filtered_cue("wide", Some(400.0), 6.0));
    assert!(
        db(wide_edge / open_edge).abs() < 3.0,
        "{wide_edge} vs {open_edge}"
    );
    assert!(db(wide_body / open_body).abs() < 3.0);
}

/// An offset alone darkens from fully open: -4 octaves is 1250 Hz, which removes 8 kHz.
#[test]
fn a_cue_filter_offset_alone_starts_from_open() {
    let (_, open_edge) = cue_spectrum(filtered_cue("open", None, 0.0));
    let (body, edge) = cue_spectrum(filtered_cue("offset", None, -4.0));
    assert!(db(edge / open_edge) < -12.0, "{:.1} dB", db(edge / open_edge));
    assert!(body > 0.03, "the 200 Hz body survives: {body}");
}

/// The filter track goes away with its voice: after the one-shot has ended and been reaped, the
/// bus is silent and a second play of the same cue sounds like the first.
#[test]
fn a_cue_filter_does_not_outlive_its_voice() {
    let mut audio = headless();
    audio.register_cue(filtered_cue("dark", Some(400.0), 0.0));
    audio
        .play_cue_seeded("dark", 1)
        .expect("plays")
        .expect("not refused");
    let first = level(&run(&mut audio, 0.4), 200.0);
    let _ = run(&mut audio, 1.0); // the one-second clip ends
    let silent = run(&mut audio, 0.2);
    assert!(level(&silent, 200.0) < 1e-4, "silent after the voice ends");
    assert_eq!(audio.real_voice_count(), 0);

    audio
        .play_cue_seeded("dark", 1)
        .expect("plays")
        .expect("not refused");
    let second = level(&run(&mut audio, 0.4), 200.0);
    assert!(db(second / first).abs() < 0.5, "{first} vs {second}");
}

// ----------------------------------------------------------------------------- loop seams

const SEAM_LOOP: &str = r#"(
    version: 1,
    name: "seam_check",
    sample_rate: 48000,
    length_secs: 0.5,
    channels: 1,
    seed: 5,
    crossfade_secs: 0.05,
    output_gain_db: -6.0,
    nodes: [
        (id: "wind", node: Noise(color: Pink, gain: 0.3)),
        (id: "tone", node: Osc(shape: Sine, hz: 100.5, detune_cents: 0.0, gain: 0.2)),
        (id: "mix", node: Mix(inputs: [(input: "wind"), (input: "tone")])),
    ],
    output: "mix",
)"#;

/// The seam metric is exactly `|first - last|`, and the sample after the last one (the first
/// again) is no further from it than the loop's own largest step.
#[test]
fn the_reported_seam_jump_is_the_wrap_step() {
    let rendered = LoopGenDef::from_ron(SEAM_LOOP)
        .expect("parses")
        .render()
        .expect("renders");
    let stats = rendered.stats();
    let samples = &rendered.samples;
    assert_eq!(rendered.channels, 1);
    assert_eq!(stats.frames, 24_000);
    assert_eq!(samples.len(), stats.frames);
    let wrap = (samples[0] - samples[samples.len() - 1]).abs();
    assert_eq!(
        stats.seam_jump.to_bits(),
        wrap.to_bits(),
        "sample-exact wrap step"
    );
    assert!(
        wrap <= stats.max_step * 1.1 + 2e-4,
        "{wrap} vs {}",
        stats.max_step
    );
    // Unrolled twice, every step across the join obeys the same bound as the steps inside.
    let mut doubled: Vec<f32> = samples.to_vec();
    doubled.extend_from_slice(samples);
    let worst = doubled
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst <= stats.max_step * 1.1 + 2e-4,
        "{worst} vs {}",
        stats.max_step
    );
}

/// The same definition renders the same bits again, and the WAV carries exactly one 16-bit
/// sample per rendered sample.
#[test]
fn a_rendered_loop_is_bit_exact_and_its_wav_has_one_sample_per_frame() {
    let def = LoopGenDef::from_ron(SEAM_LOOP).expect("parses");
    let a = def.render().expect("renders");
    let b = def.render().expect("renders again");
    assert_eq!(a.samples, b.samples, "bit-identical samples");
    assert_eq!(a.stats().hash, b.stats().hash);
    let wav = a.to_wav_bytes();
    assert_eq!(wav.len(), 44 + a.samples.len() * 2);
}

/// Played through the mixer as a loop, the output repeats with the loop's period: the wrap adds
/// no click, no gap and no drift. Compared against one period later within float tolerance
/// (the mixer's gain stages are not bit-exact against a hand computation).
#[test]
fn a_looping_voice_repeats_with_the_loop_period_through_the_mixer() {
    let rendered = LoopGenDef::from_ron(SEAM_LOOP)
        .expect("parses")
        .render()
        .expect("renders");
    let period = rendered.frames();
    let mut audio = headless();
    audio
        .start_asset(&rendered.to_sound_asset(), looping("Sfx"))
        .expect("loop starts");
    audio.tick(FRAME);
    let out = audio.render_offline(period * 3).expect("offline render");
    let mono = left(&out);
    assert_eq!(mono.len(), period * 3);
    // Skip the first period (fade-in and filter settling), compare periods two and three.
    let (second, third) = (&mono[period..period * 2], &mono[period * 2..]);
    let worst = second
        .iter()
        .zip(third)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 2e-3, "period two differs from period three by {worst}");
    // And there is no step at the join larger than the loop's own.
    let step = |a: f32, b: f32| (a - b).abs();
    let join = step(mono[period * 2 - 1], mono[period * 2]);
    assert!(join <= rendered.stats().max_step * 1.5 + 2e-3, "join step {join}");
}

// ----------------------------------------------------------------------------- dropouts

fn all_finite_and_bounded(frames: &[f32]) {
    assert!(
        frames.iter().all(|s| s.is_finite()),
        "no NaN or infinity in the mix"
    );
    assert!(frames.iter().all(|s| s.abs() <= 4.0), "no runaway sample");
}

/// A voice stream that starves (no chunks for several frames) and then resumes leaves the rest
/// of the mix untouched, never emits a non-finite sample, and drops nothing.
#[test]
fn a_starved_voice_stream_does_not_disturb_the_other_buses() {
    let mut audio = headless();
    audio
        .start_asset(&tone(440.0, 0.4), looping("Sfx"))
        .expect("sfx loop");
    audio.start_voice_loopback(10).expect("loopback starts");
    let _ = run(&mut audio, 0.2);
    let before = level(&run(&mut audio, 0.3), 440.0);

    let chunk: Arc<[f32]> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    audio.push_voice_frames(chunk.clone(), None);
    let during_gap = run(&mut audio, 0.5); // the stream has run dry long ago
    all_finite_and_bounded(&during_gap);
    audio.push_voice_frames(chunk, None);
    let after_resume = run(&mut audio, 0.3);
    all_finite_and_bounded(&after_resume);

    let after = level(&after_resume, 440.0);
    assert!(
        db(after / before).abs() < 1.0,
        "Sfx unchanged: {before} vs {after}"
    );
    assert!(
        level(&during_gap, 440.0) > 0.05,
        "Sfx kept playing through the gap"
    );
    let stats = audio.voice_stats().expect("stats");
    assert_eq!(stats.chunks_pushed, 2);
    assert_eq!(stats.chunks_dropped, 0);
    assert!(stats.running);
}

/// A flood of chunks (a transport burst after a stall) overflows the jitter buffer: the surplus is
/// counted as dropped, the mix stays finite and the stream keeps running and playing.
#[test]
fn a_burst_of_voice_chunks_is_dropped_and_counted_not_buffered_forever() {
    let mut audio = headless();
    audio.start_voice_loopback(10).expect("loopback starts");
    let chunk: Arc<[f32]> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    for _ in 0..400 {
        audio.push_voice_frames(chunk.clone(), None);
    }
    let stats = audio.voice_stats().expect("stats");
    assert_eq!(stats.chunks_pushed, 400);
    assert!(
        stats.chunks_dropped > 0,
        "the bounded queue overflowed: {stats:?}"
    );
    assert!(stats.chunks_dropped < 400, "some chunks were accepted: {stats:?}");

    let out = run(&mut audio, 0.3);
    all_finite_and_bounded(&out);
    assert!(
        out.iter().any(|s| s.abs() > 0.01),
        "the accepted chunks are heard"
    );
    assert!(audio.voice_stats().expect("stats").running);
}

/// Rendering in irregular block sizes (as a real device callback does, including a zero-length
/// request) produces the same samples as one big render of the same total.
#[test]
fn irregular_render_blocks_equal_one_big_render() {
    let play = || {
        let mut audio = headless();
        audio
            .start_asset(&tone(440.0, 0.4), looping("Sfx"))
            .expect("loop starts");
        audio.tick(FRAME);
        audio
    };
    let total = 4_800;
    let whole = play().render_offline(total).expect("one render");

    let mut audio = play();
    let mut pieces = Vec::new();
    for block in [1, 0, 7, 64, 333, 1_000, 2_000, 1_395] {
        pieces.extend_from_slice(&audio.render_offline(block).expect("block render"));
    }
    assert_eq!(pieces.len(), whole.len());
    let worst = whole
        .iter()
        .zip(&pieces)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-4, "block boundaries add no discontinuity: {worst}");
}
