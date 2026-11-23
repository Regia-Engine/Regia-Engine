// SPDX-License-Identifier: MPL-2.0
//! Robustness of the mixer against bad input, through the offline renderer (ADR-872): NaN and
//! infinite transforms, emitter fields, playback values and occlusion tuning must never reach
//! kira (a NaN sample sums into its bus, and a NaN in a filter's state stays there for good), and
//! loop regions kira cannot wrap must never hang or panic the mixer. Every test renders fixed
//! steps of static (in-memory) sounds and asserts on the samples: no device, no wall clock.

use std::time::Duration;

use regia_audio::{
    Audio, AudioEmitter, AudioOcclusion, AudioSettings, EmitterFrame, EmitterUpdate, ListenerFrame,
    LoopRegion, OcclusionTuning, PlaybackParams, RolloffCurve, SoundAsset, SoundClip,
};
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::EntityIndex;
use regia_ecs::prelude::Entity;

const RATE: u32 = 48_000;
const DT: f64 = 1.0 / 60.0;
/// Stereo frames per 1/60 s tick.
const TICK_FRAMES: usize = 800;

fn headless() -> Audio {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z));
    audio
}

/// A sine of `hz` at amplitude `amp`, `seconds` long (48 kHz mono). 480 Hz and 1200 Hz fit a
/// whole number of cycles into one tick and into one second, so they loop and measure cleanly.
fn tone(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("a sine is a valid clip"))
}

fn entity(index: u32) -> Entity {
    Entity::from_index(EntityIndex::from_raw_u32(index).expect("a valid entity index"))
}

fn channel(frames: &[f32], index: usize) -> Vec<f32> {
    frames.iter().skip(index).step_by(2).copied().collect()
}

/// Goertzel magnitude at `hz` (≈ amplitude / 2 for a sine).
fn goertzel(samples: &[f32], hz: f32) -> f32 {
    let k = std::f32::consts::TAU * hz / RATE as f32;
    let coeff = 2.0 * k.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len().max(1) as f32
}

/// Goertzel level of `hz` on the louder channel.
fn level(frames: &[f32], hz: f32) -> f32 {
    goertzel(&channel(frames, 0), hz).max(goertzel(&channel(frames, 1), hz))
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
}

fn assert_finite(frames: &[f32], what: &str) {
    if let Some(i) = frames.iter().position(|s| !s.is_finite()) {
        panic!("{what}: sample {i} is {}", frames[i]);
    }
}

/// A plain 3D emitter: full volume inside 2 m, inverse rolloff, no occlusion.
fn emitter() -> AudioEmitter {
    AudioEmitter {
        min_distance: 2.0,
        max_distance: 100.0,
        rolloff: RolloffCurve::Inverse,
        occlusion: false,
        ..AudioEmitter::default()
    }
}

fn update(entity: Entity, emitter: &AudioEmitter, position: Vec3, velocity: Vec3) -> EmitterUpdate {
    EmitterUpdate {
        entity,
        emitter: emitter.clone(),
        frame: EmitterFrame {
            position,
            velocity,
            forward: Vec3::ZERO,
        },
    }
}

/// Registers the emitters of `updates` at their first placement and starts `assets` on them.
fn start(audio: &mut Audio, occ: &mut AudioOcclusion, updates: &[EmitterUpdate], assets: &[SoundAsset]) {
    audio.sync_emitter_frames(updates, DT as f32, None, None, Some(occ));
    for (u, asset) in updates.iter().zip(assets) {
        audio
            .start_emitter(u.entity, asset.clone(), &u.emitter, u.emitter.volume_db)
            .expect("the emitter starts");
    }
}

/// One frame: sync `updates`, tick, render.
fn frame(audio: &mut Audio, occ: &mut AudioOcclusion, updates: &[EmitterUpdate]) -> Vec<f32> {
    audio.sync_emitter_frames(updates, DT as f32, None, None, Some(occ));
    audio.tick(DT);
    audio.render_offline(TICK_FRAMES).expect("offline backend")
}

/// The 480 Hz reference emitter alone, `ticks` frames; returns the last 10.
fn reference_level(ticks: usize) -> f32 {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let good = [update(entity(1), &emitter(), Vec3::NEG_Z * 2.0, Vec3::ZERO)];
    start(&mut audio, &mut occ, &good, &[tone(480.0, 1.0, 0.25)]);
    let mut tail = Vec::new();
    for tick in 0..ticks {
        let block = frame(&mut audio, &mut occ, &good);
        if tick + 10 >= ticks {
            tail.extend_from_slice(&block);
        }
    }
    level(&tail, 480.0)
}

// ------------------------------------------------------------------------------ NaN transforms

/// A NaN transform (a physics blow-up) on one emitter must not reach the bus the others share,
/// and once the emitter has a real place again it is heard again: nothing stays poisoned.
#[test]
fn a_nan_transform_neither_poisons_the_mix_nor_sticks() {
    let reference = reference_level(40);
    assert!(reference > 0.05, "the reference tone is audible: {reference}");

    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let good = update(entity(1), &emitter(), Vec3::NEG_Z * 2.0, Vec3::ZERO);
    let nan = Vec3::new(f32::NAN, 0.0, f32::NAN);
    let broken = |position: Vec3| update(entity(2), &emitter(), position, Vec3::new(f32::NAN, 0.0, 0.0));
    start(
        &mut audio,
        &mut occ,
        &[good.clone(), broken(nan)],
        &[tone(480.0, 1.0, 0.25), tone(1_200.0, 1.0, 0.25)],
    );

    let mut tail = Vec::new();
    for tick in 0..40 {
        let block = frame(&mut audio, &mut occ, &[good.clone(), broken(nan)]);
        assert_finite(&block, &format!("NaN emitter, tick {tick}"));
        if tick >= 30 {
            tail.extend_from_slice(&block);
        }
    }
    let shared = level(&tail, 480.0);
    assert!(
        db(shared / reference).abs() < 0.5,
        "the healthy emitter keeps its level: {shared} vs {reference}"
    );
    assert!(level(&tail, 1_200.0) < 1e-3, "the NaN emitter is silent");

    // The broken emitter gets a real place back: it is heard, and the mix stays finite.
    let mut tail = Vec::new();
    for tick in 0..40 {
        let block = frame(&mut audio, &mut occ, &[good.clone(), broken(Vec3::X * 2.0)]);
        assert_finite(&block, &format!("recovered emitter, tick {tick}"));
        if tick >= 30 {
            tail.extend_from_slice(&block);
        }
    }
    let recovered = level(&tail, 1_200.0);
    assert!(recovered > 0.03, "the emitter recovers: {recovered}");
    assert!(
        db(level(&tail, 480.0) / reference).abs() < 0.5,
        "the healthy emitter is unchanged"
    );
}

/// NaN in every numeric emitter field but the volume: the NaN gain is silence (it used to come
/// out of the propagation path as full volume), the mix stays finite and the emitter beside it
/// keeps its level. (A NaN `volume_db` is covered by the playback-value test.)
#[test]
fn nan_emitter_fields_do_not_reach_the_bus() {
    let reference = reference_level(40);
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let good = update(entity(1), &emitter(), Vec3::NEG_Z * 2.0, Vec3::ZERO);
    let fields = AudioEmitter {
        min_distance: f32::NAN,
        max_distance: f32::NAN,
        rolloff: RolloffCurve::Log {
            db_per_doubling: f32::NAN,
        },
        spatial_blend: f32::NAN,
        cone_inner_degrees: f32::NAN,
        cone_outer_degrees: f32::NAN,
        cone_outer_gain: f32::NAN,
        air_absorption: true,
        doppler_factor: f32::NAN,
        ..emitter()
    };
    let bad = update(entity(2), &fields, Vec3::X * 3.0, Vec3::Z * 20.0);
    start(
        &mut audio,
        &mut occ,
        &[good.clone(), bad.clone()],
        &[tone(480.0, 1.0, 0.25), tone(1_200.0, 1.0, 0.25)],
    );
    let mut tail = Vec::new();
    for tick in 0..40 {
        let block = frame(&mut audio, &mut occ, &[good.clone(), bad.clone()]);
        assert_finite(&block, &format!("NaN fields, tick {tick}"));
        if tick >= 30 {
            tail.extend_from_slice(&block);
        }
    }
    assert!(
        db(level(&tail, 480.0) / reference).abs() < 0.5,
        "the healthy emitter keeps its level"
    );
    assert!(
        level(&tail, 1_200.0) < 1e-3,
        "a NaN gain is silence, not full volume"
    );
}

/// An emitter flying straight through the listener (one frame exactly on it) with full Doppler:
/// no NaN, no gain spike.
#[test]
fn an_emitter_flying_through_the_listener_stays_finite_and_bounded() {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let fly = AudioEmitter {
        doppler_factor: 1.0,
        air_absorption: true,
        min_distance: 1.0,
        ..emitter()
    };
    // 0.5 m per frame along +Z (30 m/s): frame 20 sits exactly on the listener.
    let at = |i: usize| {
        update(
            entity(3),
            &fly,
            Vec3::new(0.0, 0.0, -10.0 + 0.5 * i as f32),
            Vec3::Z * 30.0,
        )
    };
    start(&mut audio, &mut occ, &[at(0)], &[tone(480.0, 1.0, 0.25)]);
    let mut peak = 0.0f32;
    for i in 0..41 {
        let block = frame(&mut audio, &mut occ, &[at(i)]);
        assert_finite(&block, &format!("fly-through frame {i}"));
        peak = block.iter().fold(peak, |p, s| p.max(s.abs()));
    }
    // A 0.25 sine at gain ≤ 1 through kira's equal-power pan (≤ √2 on one side).
    assert!(peak < 0.25 * 1.5, "no gain spike at the crossing: {peak}");
    assert!(peak > 0.05, "the emitter was heard: {peak}");
}

// ----------------------------------------------------------------------------------- occlusion

/// Occlusion tuning full of NaN must neither poison the emitter track's low-pass nor its gain:
/// the emitter plays unoccluded, and with the tuning repaired the same track occludes normally.
#[test]
fn nan_occlusion_tuning_leaves_the_track_usable() {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    occ.tuning = OcclusionTuning {
        occluded_gain_db: f32::NAN,
        occluded_cutoff_hz: f32::NAN,
        smoothing_secs: f32::NAN,
        per_obstacle: f32::NAN,
    };
    let occluding = AudioEmitter {
        occlusion: true,
        ..emitter()
    };
    let e = entity(4);
    let u = [update(e, &occluding, Vec3::NEG_Z * 2.0, Vec3::ZERO)];
    start(&mut audio, &mut occ, &u, &[tone(480.0, 1.0, 0.25)]);
    let mut open = Vec::new();
    for tick in 0..40 {
        occ.report(e, 1.0);
        let block = frame(&mut audio, &mut occ, &u);
        assert_finite(&block, &format!("NaN tuning, tick {tick}"));
        if tick >= 30 {
            open.extend_from_slice(&block);
        }
    }
    let open_level = level(&open, 480.0);
    assert!(open_level > 0.05, "NaN tuning plays unoccluded: {open_level}");

    occ.tuning = OcclusionTuning::default();
    let mut closed = Vec::new();
    for tick in 0..90 {
        occ.report(e, 1.0);
        let block = frame(&mut audio, &mut occ, &u);
        assert_finite(&block, &format!("repaired tuning, tick {tick}"));
        if tick >= 80 {
            closed.extend_from_slice(&block);
        }
    }
    // −15 dB of gain, plus a little of the 700 Hz low-pass on a 480 Hz tone.
    let drop = db(level(&closed, 480.0) / open_level);
    assert!(
        (-20.0..-12.0).contains(&drop),
        "the track still occludes: {drop} dB"
    );
}

/// Occlusion arriving all at once is smoothed: no tick-to-tick level step above 3 dB, and the
/// level settles at the tuned −15 dB plus the low-pass's share.
#[test]
fn sudden_occlusion_fades_without_steps() {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let occluding = AudioEmitter {
        occlusion: true,
        ..emitter()
    };
    let e = entity(5);
    let u = [update(e, &occluding, Vec3::NEG_Z * 2.0, Vec3::ZERO)];
    start(&mut audio, &mut occ, &u, &[tone(480.0, 1.0, 0.25)]);
    for _ in 0..20 {
        let _ = frame(&mut audio, &mut occ, &u);
    }
    let mut levels = Vec::new();
    for _ in 0..90 {
        occ.report(e, 1.0);
        // 800 frames hold exactly 8 cycles of 480 Hz: the block RMS is phase-independent.
        levels.push(rms(&channel(&frame(&mut audio, &mut occ, &u), 0)));
    }
    let worst = levels
        .windows(2)
        .map(|w| db(w[1] / w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 3.0, "largest tick-to-tick step {worst} dB");
    // −15 dB of gain plus about −3 dB of the 700 Hz low-pass (kira's filter at resonance 0 has
    // Q = 0.5) on a 480 Hz tone; the first block already carries part of the first step.
    let settled = db(levels[89] / levels[0]);
    assert!(
        (-21.0..-13.0).contains(&settled),
        "settles near −18 dB: {settled}"
    );
}

// ----------------------------------------------------------------------- playback values

/// NaN, infinite and out-of-range playback values from a host or tool: no panic, no NaN.
#[test]
fn nan_and_extreme_playback_values_play_safely() {
    let mut audio = headless();
    let asset = tone(480.0, 1.0, 0.25);
    let looping = Some(LoopRegion {
        start_secs: 0.0,
        end_secs: None,
    });
    let silent = audio
        .start_asset(
            &asset,
            PlaybackParams {
                volume_db: f32::NAN,
                pitch: f32::NAN,
                panning: Some(f32::NAN),
                loop_region: looping,
                ..PlaybackParams::default()
            },
        )
        .expect("NaN params still start");
    let fast = audio
        .start_asset(
            &asset,
            PlaybackParams {
                volume_db: f32::INFINITY,
                pitch: 1e9,
                panning: Some(5.0),
                loop_region: looping,
                ..PlaybackParams::default()
            },
        )
        .expect("extreme params still start");
    // Tweens from a tool with nonsense lengths (these used to panic in `Duration`).
    audio.set_voice_volume_db(silent, f32::NAN, -1.0);
    audio.set_voice_pitch(silent, f32::INFINITY, f32::NAN);
    audio.set_voice_pan(silent, f32::NAN, f32::INFINITY);
    audio.set_voice_volume_db(fast, -6.0, -0.5);
    for tick in 0..20 {
        audio.tick(DT);
        let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
        assert_finite(&block, &format!("extreme params, tick {tick}"));
    }
    // A NaN tick is ignored, not a panic.
    audio.tick(f64::NAN);
    let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
    assert_finite(&block, "after a NaN tick");
}

/// Loop regions kira cannot wrap (zero length, inverted, past the end): kira's transport would
/// spin forever or underflow. They loop the whole sound instead and play normally.
#[test]
fn degenerate_loop_regions_neither_hang_nor_panic() {
    let asset = tone(480.0, 1.0, 0.25);
    for (start_secs, end_secs) in [(0.5, Some(0.5)), (0.8, Some(0.2)), (1.0, None), (5.0, None)] {
        let mut audio = headless();
        audio
            .start_asset(
                &asset,
                PlaybackParams {
                    loop_region: Some(LoopRegion { start_secs, end_secs }),
                    ..PlaybackParams::default()
                },
            )
            .expect("the voice starts");
        let mut tail = Vec::new();
        // 2.5 s: past the end of the 1 s sound, so it must have wrapped to still be playing.
        for tick in 0..150 {
            audio.tick(DT);
            let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
            if tick >= 140 {
                tail.extend_from_slice(&block);
            }
        }
        assert_finite(&tail, "degenerate loop");
        let heard = level(&tail, 480.0);
        assert!(
            heard > 0.05,
            "region {start_secs}..{end_secs:?} loops the whole sound: {heard}"
        );
    }
}

/// A one-shot started and stopped with a fade of NaN length from a tool does not panic.
#[test]
fn stopping_with_a_zero_fade_after_nan_values_is_clean() {
    let mut audio = headless();
    let id = audio
        .start_asset(&tone(480.0, 1.0, 0.25), PlaybackParams::default())
        .expect("the voice starts");
    audio.set_voice_pitch(id, f32::NAN, 0.0);
    audio.tick(DT);
    let _ = audio.render_offline(TICK_FRAMES);
    audio.stop_voice(id, Duration::ZERO);
    for _ in 0..5 {
        audio.tick(DT);
        let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
        assert_finite(&block, "after stop");
    }
}

// ------------------------------------------------------------------------------------- streams

/// PCM16 mono WAV bytes of `samples` (already in −1..1) at 48 kHz.
fn wav(samples: &[f32]) -> std::sync::Arc<Vec<u8>> {
    let mut data = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        data.extend_from_slice(&((s * 32_767.0).round() as i16).to_le_bytes());
    }
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::sync::Arc::new(out)
}

/// Renders `asset` alone (2D, centred, on `Master`) for `ticks` fixed steps.
fn render_alone(asset: &SoundAsset, ticks: usize) -> Vec<f32> {
    let mut audio = headless();
    audio
        .start_asset(asset, PlaybackParams::default())
        .expect("the sound starts");
    let mut out = Vec::new();
    for _ in 0..ticks {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    out
}

/// A stream decoded to memory renders the same samples on every run without pacing the renderer
/// at real time, and they are the file's samples from the first frame (no decoder stall, no
/// shift).
#[test]
fn a_stream_decoded_to_memory_renders_reproducibly_at_any_speed() {
    let source: Vec<f32> = (0..RATE as usize)
        .map(|i| ((i as f32 / RATE as f32) * 480.0 * std::f32::consts::TAU).sin() * 0.25)
        .collect();
    let stream = SoundAsset::streaming_from_bytes(wav(&source), "tone.wav").expect("the WAV parses");
    assert!(stream.is_streaming());
    let decoded = stream.decode_to_memory().expect("the WAV decodes");
    assert!(!decoded.is_streaming());
    assert_eq!(decoded.label(), "tone.wav");
    assert!(
        (decoded.duration_secs() - 1.0).abs() < 1e-3,
        "{}",
        decoded.duration_secs()
    );
    assert_eq!(decoded.sample_rate(), RATE);

    // 40 ticks = 0.67 s, faster than real time: two runs agree bit for bit.
    let first = render_alone(&decoded, 40);
    let second = render_alone(&decoded, 40);
    assert_eq!(
        first.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        second.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    // ...and match the same PCM played from a clip, frame for frame (PCM16 rounding apart).
    let quantized: Vec<f32> = source
        .iter()
        .map(|s| f32::from((s * 32_767.0).round() as i16) / 32_768.0)
        .collect();
    let clip = SoundAsset::from_clip(&SoundClip::new(quantized, RATE).expect("clip"));
    let reference = render_alone(&clip, 40);
    let worst = first
        .iter()
        .zip(&reference)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-3, "decoded stream vs clip: {worst}");
    assert!(level(&first, 480.0) > 0.1, "the tone was rendered");

    // A sound already in memory is returned as is.
    assert_eq!(clip.decode_to_memory().expect("static"), clip);
}

/// A zero-length loop on a stream used to spin kira's decoder thread forever; the region is
/// replaced before the decoder sees it, so the stream starts and stops normally.
#[test]
fn a_degenerate_loop_on_a_stream_is_replaced_before_the_decoder_sees_it() {
    let source: Vec<f32> = (0..RATE as usize / 10)
        .map(|i| (i as f32 * 0.01).sin() * 0.2)
        .collect();
    let stream = SoundAsset::streaming_from_bytes(wav(&source), "short.wav").expect("the WAV parses");
    let mut audio = headless();
    let id = audio
        .start_asset(
            &stream,
            PlaybackParams {
                loop_region: Some(LoopRegion {
                    start_secs: 0.05,
                    end_secs: Some(0.05),
                }),
                ..PlaybackParams::default()
            },
        )
        .expect("the stream starts");
    for _ in 0..5 {
        audio.tick(DT);
        let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
        assert_finite(&block, "degenerate stream loop");
    }
    audio.stop_voice(id, Duration::ZERO);
    for _ in 0..3 {
        audio.tick(DT);
        let _ = audio.render_offline(TICK_FRAMES);
    }
}

// --------------------------------------------------------------------------- stream underruns

/// Frames per decoded chunk of the gated test stream (0.1 s).
const GATE_CHUNK: usize = 4_800;

/// How far a gated decoder may go, shared between the test and kira's decoder thread.
#[derive(Default)]
struct GateState {
    /// Chunks the decoder may hand out.
    allowed: usize,
    /// No limit any more (the decoder never waits again).
    open: bool,
    /// The chunk the decoder asked for last.
    requested: usize,
}

#[derive(Default)]
struct Gate {
    state: std::sync::Mutex<GateState>,
    changed: std::sync::Condvar,
}

impl Gate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn allow(&self, chunks: usize) {
        self.lock().allowed = chunks;
        self.changed.notify_all();
    }

    fn open(&self) {
        self.lock().open = true;
        self.changed.notify_all();
    }

    /// Waits (a handshake, not a timing assumption) until the decoder asked for `chunk`: kira
    /// pushes every frame of a chunk into its ring buffer before it asks for the next, so all
    /// frames before `chunk` are then buffered. The deadline only turns a hang into a failure.
    fn wait_requested(&self, chunk: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut state = self.lock();
        while state.requested < chunk {
            let now = std::time::Instant::now();
            assert!(now < deadline, "the decoder thread never asked for chunk {chunk}");
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// A kira decoder over in-memory frames that hands out only the chunks its gate allows: a
/// decoder that falls behind on demand.
struct GatedDecoder {
    frames: std::sync::Arc<[kira::Frame]>,
    next_chunk: usize,
    gate: std::sync::Arc<Gate>,
}

impl kira::sound::streaming::Decoder for GatedDecoder {
    type Error = kira::sound::FromFileError;

    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn num_frames(&self) -> usize {
        self.frames.len()
    }

    fn decode(&mut self) -> Result<Vec<kira::Frame>, Self::Error> {
        let chunk = self.next_chunk;
        {
            let mut state = self.gate.lock();
            state.requested = state.requested.max(chunk);
            self.gate.changed.notify_all();
            while chunk >= state.allowed && !state.open {
                state = self
                    .gate
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        self.next_chunk += 1;
        let start = (chunk * GATE_CHUNK).min(self.frames.len());
        let end = (start + GATE_CHUNK).min(self.frames.len());
        Ok(self.frames[start..end].to_vec())
    }

    fn seek(&mut self, index: usize) -> Result<usize, Self::Error> {
        self.next_chunk = index / GATE_CHUNK;
        Ok(self.next_chunk * GATE_CHUNK)
    }
}

/// A 1 s 480 Hz stream whose decoder only hands out what `gate` allows.
fn gated_stream(gate: &std::sync::Arc<Gate>) -> SoundAsset {
    let frames: std::sync::Arc<[kira::Frame]> = (0..RATE as usize)
        .map(|i| {
            kira::Frame::from_mono(((i as f32 / RATE as f32) * 480.0 * std::f32::consts::TAU).sin() * 0.25)
        })
        .collect();
    let gate = std::sync::Arc::clone(gate);
    SoundAsset::from_streaming_factory(
        move || {
            Ok(kira::sound::streaming::StreamingSoundData::from_decoder(
                GatedDecoder {
                    frames: std::sync::Arc::clone(&frames),
                    next_chunk: 0,
                    gate: std::sync::Arc::clone(&gate),
                },
            ))
        },
        1.0,
        "gated",
    )
}

/// A stream whose decoder falls behind is held silent by the mixer; the watch counts the
/// underrun once, reports it starved while it lasts, and clears it when the decoder catches up.
/// Deterministic: the decoder is blocked at a known frame before any audio is rendered.
#[test]
fn a_starved_stream_is_counted_once_and_recovers() {
    let gate = std::sync::Arc::new(Gate::default());
    gate.allow(1);
    let asset = gated_stream(&gate);
    assert!(asset.is_streaming());
    assert!(
        asset.decode_to_memory().is_err(),
        "a caller's decoder has no in-memory form"
    );
    let mut audio = headless();
    let id = audio
        .start_asset(&asset, PlaybackParams::default())
        .expect("the stream starts");
    // Exactly the first 0.1 s is buffered; the decoder now waits for the second chunk.
    gate.wait_requested(1);

    let mut out = Vec::new();
    for _ in 0..20 {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&out, "starved stream");
    let (head, tail) = out.split_at(4 * TICK_FRAMES * 2);
    assert!(level(head, 480.0) > 0.05, "the buffered 0.1 s played");
    assert!(
        rms(&tail[tail.len() - 6 * TICK_FRAMES * 2..]) < 1e-6,
        "then the stream is silent"
    );
    let starved = audio.stream_stats();
    assert_eq!(starved.streaming_voices, 1, "{starved:?}");
    assert_eq!(starved.starved_voices, 1, "{starved:?}");
    assert_eq!(starved.underruns, 1, "{starved:?}");
    assert!(starved.starved_secs >= 0.1, "{starved:?}");

    // The decoder catches up: at least two more chunks are buffered before rendering resumes.
    gate.open();
    gate.wait_requested(3);
    let mut resumed = Vec::new();
    for _ in 0..8 {
        audio.tick(DT);
        resumed.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert!(level(&resumed, 480.0) > 0.05, "the stream plays again");
    let after = audio.stream_stats();
    assert_eq!(after.underruns, 1, "one stretch, one underrun: {after:?}");
    assert_eq!(after.starved_voices, 0, "{after:?}");

    audio.stop_voice(id, Duration::ZERO);
    for _ in 0..3 {
        audio.tick(DT);
        let _ = audio.render_offline(TICK_FRAMES);
    }
}

/// Ticking without rendering (a headless run that never renders) is not starvation, and a
/// stream that is paused is not either.
#[test]
fn a_stream_that_is_not_rendered_or_is_paused_is_not_starved() {
    let gate = std::sync::Arc::new(Gate::default());
    gate.open();
    let mut audio = headless();
    let id = audio
        .start_asset(&gated_stream(&gate), PlaybackParams::default())
        .expect("the stream starts");
    gate.wait_requested(2);
    for _ in 0..4 {
        audio.tick(DT);
        let _ = audio.render_offline(TICK_FRAMES);
    }
    // Thirty ticks, nothing rendered.
    for _ in 0..30 {
        audio.tick(DT);
    }
    assert_eq!(audio.stream_stats().underruns, 0, "{:?}", audio.stream_stats());
    // Paused and rendered: the position stands still on purpose.
    audio.pause_voice(id, Duration::ZERO);
    for _ in 0..20 {
        audio.tick(DT);
        let _ = audio.render_offline(TICK_FRAMES);
    }
    let stats = audio.stream_stats();
    assert_eq!(stats.underruns, 0, "{stats:?}");
    assert_eq!(stats.starved_voices, 0, "{stats:?}");
    audio.stop_voice(id, Duration::ZERO);
}

// ---------------------------------------------------------------------------- bus layouts

/// Bus effect times from an authored layout that `Duration` or kira cannot take (zero, NaN,
/// negative, absurdly long): the tree builds and renders instead of panicking (a zero-frame kira
/// delay line panics on the mixer thread; a negative or NaN time panicked in `Duration`).
#[test]
fn degenerate_bus_effect_times_build_and_render() {
    use regia_audio::{BusEffect, BusLayout};
    let mut layout = BusLayout::default_tree();
    let bus = |layout: &BusLayout, name: &str| -> usize {
        layout
            .buses
            .iter()
            .position(|b| b.name == name)
            .unwrap_or_else(|| panic!("the default tree has a `{name}` bus"))
    };
    let sfx = bus(&layout, "Sfx");
    for delay_secs in [0.0, f32::NAN, -1.0, 1e9] {
        layout.buses[sfx].effects.push(BusEffect::Delay {
            delay_secs,
            feedback_db: -12.0,
            mix: 0.3,
        });
    }
    let voice = bus(&layout, "Voice");
    layout.buses[voice].effects.push(BusEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_secs: f32::NAN,
        release_secs: -0.5,
        makeup_db: 0.0,
    });
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: layout,
        ..AudioSettings::default()
    });
    // 1 kHz on Sfx (its 1 ms delay lines then add in phase) and 480 Hz on Voice: each bus is
    // checked on its own tone.
    for (bus, hz) in [("Sfx", 1_000.0), ("Voice", 480.0)] {
        audio
            .start_asset(
                &tone(hz, 1.0, 0.25),
                PlaybackParams {
                    bus: Some(bus.into()),
                    ..PlaybackParams::default()
                },
            )
            .expect("the bus exists");
    }
    let mut out = Vec::new();
    for _ in 0..10 {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&out, "degenerate bus effects");
    assert!(level(&out, 1_000.0) > 0.02, "the Sfx bus (delays) is heard");
    assert!(level(&out, 480.0) > 0.05, "the Voice bus (compressor) is heard");
}

// ----------------------------------------------------------------------------------- voice

/// Decoded remote voice is untrusted: NaN, infinite and out-of-range samples from a broken or
/// hostile peer neither poison the Voice bus nor blast the listener, and good frames after them
/// still play.
#[test]
fn bad_voice_frames_are_scrubbed_before_the_voice_bus() {
    let mut audio = headless();
    audio.start_voice_loopback(10).expect("loopback starts");
    let hostile: std::sync::Arc<[f32]> = (0..960)
        .map(|i| match i % 4 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => 1e30,
            _ => -1e30,
        })
        .collect();
    audio.push_voice_frames(hostile, None);
    let mut out = Vec::new();
    for _ in 0..6 {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&out, "hostile voice frames");
    // Kira's equal-power pan keeps a centred ±1 at ±1; allow the bus chain a little headroom.
    let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    assert!(peak <= 1.5, "clamped, not a blast: {peak}");

    let tone: std::sync::Arc<[f32]> = (0..4_800)
        .map(|i| ((i as f32 / RATE as f32) * 480.0 * std::f32::consts::TAU).sin() * 0.25)
        .collect();
    audio.push_voice_frames(tone, None);
    let mut after = Vec::new();
    for _ in 0..6 {
        audio.tick(DT);
        after.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&after, "voice after the hostile frames");
    assert!(rms(&after) > 0.02, "good frames still play: {}", rms(&after));
    audio.stop_voice_loopback();
}

/// A positional one-shot queued at a NaN or infinite position (`play_asset_at`, the drain path)
/// is silent and the one-shot beside it plays normally.
#[test]
fn a_one_shot_at_a_nan_position_is_silent_and_harmless() {
    let mut audio = headless();
    let bad = tone(1_200.0, 1.0, 0.25);
    for position in [Vec3::NAN, Vec3::INFINITY, Vec3::new(0.0, f32::NEG_INFINITY, 0.0)] {
        audio
            .play_asset_at(&bad, PlaybackParams::default(), position, &emitter())
            .expect("queued");
    }
    audio
        .play_asset_at(
            &tone(480.0, 1.0, 0.25),
            PlaybackParams::default(),
            Vec3::NEG_Z * 2.0,
            &emitter(),
        )
        .expect("queued");
    audio.drain();
    let mut out = Vec::new();
    for _ in 0..12 {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&out, "NaN-placed one-shots");
    assert!(level(&out, 480.0) > 0.05, "the healthy one-shot plays");
    assert!(level(&out, 1_200.0) < 1e-3, "the NaN-placed ones are silent");
}

/// A NaN listener (a broken camera transform: NaN position or rotation), for speakers and for
/// headphones (binaural): the mix stays finite, and the emitter is heard at its level again once
/// the listener is valid.
#[test]
fn a_nan_listener_is_survivable_for_speakers_and_headphones() {
    let reference = reference_level(40);
    for hrtf in [false, true] {
        let mut audio = headless();
        let mut occ = AudioOcclusion::default();
        let u = [update(entity(6), &emitter(), Vec3::NEG_Z * 2.0, Vec3::ZERO)];
        let listener = |position: Vec3, rotation: regia_core::math::Quat| {
            let mut frame = ListenerFrame::from_rotation(position, rotation);
            frame.hrtf = hrtf;
            frame
        };
        let nan_rotation = regia_core::math::Quat::from_xyzw(f32::NAN, 0.0, 0.0, f32::NAN);
        audio.set_listener(listener(Vec3::ZERO, regia_core::math::Quat::IDENTITY));
        start(&mut audio, &mut occ, &u, &[tone(480.0, 1.0, 0.25)]);
        for (tick, (position, rotation)) in [
            (Vec3::NAN, regia_core::math::Quat::IDENTITY),
            (Vec3::ZERO, nan_rotation),
        ]
        .into_iter()
        .cycle()
        .take(20)
        .enumerate()
        {
            audio.set_listener(listener(position, rotation));
            let block = frame(&mut audio, &mut occ, &u);
            assert_finite(&block, &format!("NaN listener (hrtf {hrtf}), tick {tick}"));
        }
        audio.set_listener(listener(Vec3::ZERO, regia_core::math::Quat::IDENTITY));
        let mut tail = Vec::new();
        for tick in 0..40 {
            let block = frame(&mut audio, &mut occ, &u);
            assert_finite(&block, &format!("restored listener (hrtf {hrtf}), tick {tick}"));
            if tick >= 30 {
                tail.extend_from_slice(&block);
            }
        }
        let heard = level(&tail, 480.0);
        if hrtf {
            // Binaural: the head model filters the tone, so only "clearly heard" is asserted.
            assert!(heard > 0.02, "binaural emitter heard again: {heard}");
        } else {
            assert!(
                db(heard / reference).abs() < 0.5,
                "speaker emitter back at its level: {heard} vs {reference}"
            );
        }
    }
}

// ------------------------------------------------------------------------------------ reverb

/// A reverb-zone mix with NaN room values (a NaN in a scene file) must not reach the shared
/// reverb send: its feedback state would keep the NaN and feed it into `Master` for good.
#[test]
fn a_nan_reverb_zone_value_does_not_poison_the_shared_reverb() {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let u = [update(entity(8), &emitter(), Vec3::NEG_Z * 2.0, Vec3::ZERO)];
    start(&mut audio, &mut occ, &u, &[tone(480.0, 1.0, 0.25)]);
    audio.set_reverb_mix(Some(regia_audio::ReverbMix {
        weight: 1.0,
        send_db: 0.0,
        feedback: f32::NAN,
        damping: f32::INFINITY,
        stereo_width: f32::NAN,
    }));
    let mut tail = Vec::new();
    for tick in 0..40 {
        let block = frame(&mut audio, &mut occ, &u);
        assert_finite(&block, &format!("NaN reverb room, tick {tick}"));
        if tick >= 30 {
            tail.extend_from_slice(&block);
        }
    }
    // The emitter is still heard (the reverb's own phase may add to or take from the dry tone, so
    // only "clearly audible" is asserted).
    assert!(level(&tail, 480.0) > 0.03, "the emitter is heard");
    // A NaN weight or send is refused too, and the zone can be cleared afterwards.
    audio.set_reverb_mix(Some(regia_audio::ReverbMix {
        weight: f32::NAN,
        send_db: f32::NAN,
        ..regia_audio::ReverbMix::DRY
    }));
    audio.set_reverb_mix(None);
    for tick in 0..10 {
        let block = frame(&mut audio, &mut occ, &u);
        assert_finite(&block, &format!("cleared reverb, tick {tick}"));
    }
}

// ------------------------------------------------------------------------------- determinism

/// One run of a scripted scene: an orbiting Doppler emitter, an emitter whose occlusion ramps,
/// positional one-shots every ten ticks, and a listener that turns. Fixed steps only.
fn scripted_scene() -> Vec<f32> {
    let mut audio = headless();
    let mut occ = AudioOcclusion::default();
    let orbit = AudioEmitter {
        doppler_factor: 1.0,
        air_absorption: true,
        ..emitter()
    };
    let walled = AudioEmitter {
        occlusion: true,
        ..emitter()
    };
    let at = |tick: usize| {
        let angle = tick as f32 * 0.05;
        let position = Vec3::new(angle.sin() * 6.0, 0.0, -angle.cos() * 6.0);
        let velocity = Vec3::new(angle.cos(), 0.0, angle.sin()) * 18.0;
        [
            update(entity(10), &orbit, position, velocity),
            update(entity(11), &walled, Vec3::new(3.0, 0.0, -4.0), Vec3::ZERO),
        ]
    };
    start(
        &mut audio,
        &mut occ,
        &at(0),
        &[tone(480.0, 1.0, 0.2), tone(1_200.0, 1.0, 0.2)],
    );
    let click = tone(3_000.0, 0.05, 0.3);
    let mut out = Vec::new();
    for tick in 0..90usize {
        let yaw = regia_core::math::Quat::from_rotation_y(tick as f32 * 0.01);
        audio.set_listener(ListenerFrame::from_rotation(Vec3::ZERO, yaw));
        occ.report(entity(11), (tick as f32 / 60.0).min(1.0));
        if tick.is_multiple_of(10) {
            audio
                .play_asset_at(
                    &click,
                    PlaybackParams::default(),
                    Vec3::new(-2.0, 0.0, -2.0),
                    &emitter(),
                )
                .expect("queued");
        }
        audio.drain();
        out.extend_from_slice(&frame(&mut audio, &mut occ, &at(tick)));
    }
    out
}

/// The whole spatial pipeline (spatialization, Doppler, occlusion smoothing, propagation paths,
/// one-shots) renders the same scene the same way twice: no wall clock, no thread timing, no
/// hash-map order reaches the samples. (A tolerance, not bit equality: kira's mixer does not
/// promise a summation order across instances.)
#[test]
fn a_scripted_scene_renders_the_same_twice() {
    let first = scripted_scene();
    let second = scripted_scene();
    assert_eq!(first.len(), second.len());
    assert_finite(&first, "scripted scene");
    let worst = first
        .iter()
        .zip(&second)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-5, "two runs differ by {worst}");
    assert!(rms(&first) > 0.01, "the scene is audible");
}

/// NaN parameters in an authored bus layout (a low-pass, an EQ, a distortion and the reverb send's
/// room): every bus still builds, renders finite samples and is heard (kira's filter, EQ and
/// reverb would keep a NaN in their state for the life of the bus).
#[test]
fn nan_bus_effect_values_are_neutralised() {
    use regia_audio::{BusEffect, BusLayout};
    let mut layout = BusLayout::default_tree();
    let index = |layout: &BusLayout, name: &str| -> usize {
        layout
            .buses
            .iter()
            .position(|b| b.name == name)
            .unwrap_or_else(|| panic!("the default tree has a `{name}` bus"))
    };
    let sfx = index(&layout, "Sfx");
    layout.buses[sfx].effects.push(BusEffect::LowPass {
        cutoff_hz: f32::NAN,
        resonance: f32::NAN,
    });
    layout.buses[sfx]
        .effects
        .push(BusEffect::Distortion { drive_db: f32::NAN });
    let world = index(&layout, "World");
    layout.buses[world].effects.push(BusEffect::Eq {
        low_hz: f32::NAN,
        low_db: f32::INFINITY,
        mid_hz: 1_000.0,
        mid_db: f32::NAN,
        high_hz: f32::NAN,
        high_db: 0.0,
        quality: f32::NAN,
    });
    for send in &mut layout.send_buses {
        send.effects.push(BusEffect::Reverb {
            feedback: f32::NAN,
            damping: f32::NAN,
            stereo_width: f32::INFINITY,
            mix: f32::NAN,
        });
    }
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: layout,
        ..AudioSettings::default()
    });
    for (bus, hz) in [("Sfx", 1_200.0), ("World", 480.0)] {
        audio
            .start_asset(
                &tone(hz, 1.0, 0.25),
                PlaybackParams {
                    bus: Some(bus.into()),
                    ..PlaybackParams::default()
                },
            )
            .expect("the bus exists");
    }
    let mut out = Vec::new();
    for _ in 0..12 {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    assert_finite(&out, "NaN bus effect values");
    assert!(level(&out, 1_200.0) > 0.03, "the Sfx bus is heard");
    assert!(level(&out, 480.0) > 0.03, "the World bus is heard");
}
