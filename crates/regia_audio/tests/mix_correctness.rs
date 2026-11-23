// SPDX-License-Identifier: MPL-2.0
//! Mixer correctness through the offline renderer (ADR-544): spatialization (pan law, rolloff),
//! occlusion, bus gains, mutes and sends, clicks and clipping. Every test renders fixed steps of
//! static (in-memory) sounds and asserts on the samples: no device, no wall clock, no sleeps.

use std::time::Duration;

use regia_audio::{
    Audio, AudioEmitter, AudioOcclusion, AudioSettings, BusLayout, DECLICK_FADE, EmitterFrame, EmitterUpdate,
    ListenerFrame, LoopRegion, MasterLimiter, OcclusionTuning, PlaybackParams, RenderStats, RolloffCurve,
    SoundAsset, SoundClip, db_to_gain,
};
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::EntityIndex;
use regia_ecs::prelude::Entity;

const RATE: u32 = 48_000;
const DT: f64 = 1.0 / 60.0;
/// Stereo frames per 1/60 s tick.
const TICK_FRAMES: usize = 800;

fn headless() -> Audio {
    headless_with(BusLayout::default_tree())
}

fn headless_with(buses: BusLayout) -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        buses,
        ..AudioSettings::default()
    })
}

/// A sum of sines (`(hz, amplitude)`), `seconds` long, 48 kHz mono. With whole cycles per
/// partial over the length it loops seamlessly.
fn tones(partials: &[(f32, f32)], seconds: f64) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            partials
                .iter()
                .map(|&(hz, amp)| (t * hz * std::f32::consts::TAU).sin() * amp)
                .sum::<f32>()
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("a sine sum is a valid clip"))
}

fn looping_on(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

/// `ticks` frames of tick + render, concatenated.
fn run(audio: &mut Audio, ticks: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(ticks * TICK_FRAMES * 2);
    for _ in 0..ticks {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    out
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

fn power(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
}

fn entity(index: u32) -> Entity {
    Entity::from_index(EntityIndex::from_raw_u32(index).expect("a valid entity index"))
}

/// An emitter of `asset` at `position`, synced for `ticks` frames (with `occlusion` reported
/// as `amount` when given); returns the last `measure` ticks.
fn emitter_run(
    emitter: &AudioEmitter,
    asset: SoundAsset,
    position: Vec3,
    occlusion: Option<f32>,
    ticks: usize,
    measure: usize,
) -> Vec<f32> {
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z));
    let e = entity(7);
    let update = EmitterUpdate {
        entity: e,
        emitter: emitter.clone(),
        frame: EmitterFrame {
            position,
            ..EmitterFrame::default()
        },
    };
    let mut occ = AudioOcclusion::default();
    audio.sync_emitter_frames(
        std::slice::from_ref(&update),
        DT as f32,
        None,
        None,
        Some(&mut occ),
    );
    audio
        .start_emitter(e, asset, emitter, emitter.volume_db)
        .expect("the emitter starts");
    let mut out = Vec::new();
    for tick in 0..ticks {
        if let Some(amount) = occlusion {
            occ.report(e, amount);
        }
        audio.sync_emitter_frames(
            std::slice::from_ref(&update),
            DT as f32,
            None,
            None,
            Some(&mut occ),
        );
        audio.tick(DT);
        let block = audio.render_offline(TICK_FRAMES).expect("offline backend");
        if tick + measure >= ticks {
            out.extend_from_slice(&block);
        }
    }
    out
}

fn near_emitter() -> AudioEmitter {
    AudioEmitter {
        min_distance: 2.0,
        max_distance: 100.0,
        rolloff: RolloffCurve::Inverse,
        occlusion: true,
        ..AudioEmitter::default()
    }
}

// --------------------------------------------------------------------------- spatialization

/// kira's pan law is constant power (`L = a√(1−p)`, `R = a√(1+p)`) and the emitter's pan is
/// the sine of its azimuth: walking an emitter around the listener keeps the total power and
/// moves the left share to `(1 − sin θ) / 2`.
#[test]
fn the_pan_law_keeps_power_constant_around_the_listener() {
    let asset = tones(&[(440.0, 0.4)], 0.5);
    let mut reference_power = None;
    for degrees in [0.0f32, 45.0, 90.0, 135.0, 180.0, -45.0, -90.0] {
        let theta = degrees.to_radians();
        // Inside min_distance: distance gain 1, only the direction matters.
        let position = Vec3::new(1.5 * theta.sin(), 0.0, -1.5 * theta.cos());
        let out = emitter_run(&near_emitter(), asset.clone(), position, None, 24, 6);
        let (left, right) = (power(&channel(&out, 0)), power(&channel(&out, 1)));
        let total = left + right;
        let reference = *reference_power.get_or_insert(total);
        assert!(
            db(total.sqrt() / reference.sqrt()).abs() < 0.5,
            "{degrees}°: total power {total} vs front {reference}"
        );
        let share = left / total;
        let expected = (1.0 - theta.sin()) / 2.0;
        assert!(
            (share - expected).abs() < 0.03,
            "{degrees}°: left share {share}, the pan law gives {expected}"
        );
    }
}

/// The inverse rolloff (`g = min_d / d`) loses 6 dB per doubling of distance, measured on the
/// rendered output.
#[test]
fn inverse_rolloff_loses_six_db_per_doubling_through_the_mixer() {
    let asset = tones(&[(440.0, 0.4)], 0.5);
    let levels: Vec<f32> = [4.0f32, 8.0, 16.0]
        .iter()
        .map(|&d| {
            level(
                &emitter_run(&near_emitter(), asset.clone(), Vec3::NEG_Z * d, None, 24, 6),
                440.0,
            )
        })
        .collect();
    for pair in levels.windows(2) {
        let step = db(pair[1] / pair[0]);
        assert!(
            (step + 6.02).abs() < 0.5,
            "a doubling loses {step:.2} dB, not −6.02"
        );
    }
    let at_4 = db(levels[0] / 0.2);
    assert!((at_4 + 6.02).abs() < 0.6, "4 m with min 2 m is −6 dB: {at_4:.2}");
}

/// Beyond `max_distance` an emitter is silent; with `spatial_blend` 0 it is centred and
/// unattenuated wherever it is.
#[test]
fn max_distance_silences_and_a_2d_emitter_ignores_its_place() {
    let asset = tones(&[(440.0, 0.4)], 0.5);
    let far = emitter_run(&near_emitter(), asset.clone(), Vec3::X * 150.0, None, 24, 6);
    assert!(
        RenderStats::measure(&far).peak_db() < -80.0,
        "past max_distance is silent"
    );

    let flat = AudioEmitter {
        spatial_blend: 0.0,
        ..near_emitter()
    };
    let out = emitter_run(&flat, asset, Vec3::X * 40.0, None, 24, 6);
    let (left, right) = (channel(&out, 0), channel(&out, 1));
    assert!(
        db(goertzel(&left, 440.0) / goertzel(&right, 440.0)).abs() < 0.2,
        "a 2D emitter is centred"
    );
    assert!(
        db(goertzel(&left, 440.0) / 0.2).abs() < 0.5,
        "and unattenuated: {}",
        goertzel(&left, 440.0)
    );
}

// --------------------------------------------------------------------------- occlusion

/// A fully occluded emitter is `occluded_gain_db` quieter in its low band and loses much more
/// above the occluded cutoff; half occlusion is half the dB.
#[test]
fn occlusion_lowers_the_level_by_its_tuning_and_darkens_the_sound() {
    let tuning = OcclusionTuning::default();
    let asset = tones(&[(200.0, 0.2), (8_000.0, 0.2)], 0.5);
    let position = Vec3::NEG_Z * 1.5;
    let clear = emitter_run(&near_emitter(), asset.clone(), position, None, 60, 12);
    let full = emitter_run(&near_emitter(), asset.clone(), position, Some(1.0), 60, 12);
    let half = emitter_run(&near_emitter(), asset, position, Some(0.5), 60, 12);

    let low_full = db(level(&full, 200.0) / level(&clear, 200.0));
    assert!(
        (low_full - tuning.occluded_gain_db).abs() < 1.5,
        "full occlusion: 200 Hz moved {low_full:.2} dB, tuning says {}",
        tuning.occluded_gain_db
    );
    let high_full = db(level(&full, 8_000.0) / level(&clear, 8_000.0));
    assert!(
        high_full < tuning.occluded_gain_db - 15.0,
        "the {} Hz low-pass removes 8 kHz: {high_full:.2} dB",
        tuning.occluded_cutoff_hz
    );
    let low_half = db(level(&half, 200.0) / level(&clear, 200.0));
    assert!(
        (low_half - tuning.occluded_gain_db * 0.5).abs() < 1.5,
        "half occlusion is half the dB: {low_half:.2}"
    );
    assert!(RenderStats::measure(&full).is_clean() && RenderStats::measure(&half).is_clean());
}

// --------------------------------------------------------------------------- buses

/// The level of a `hz` tone looping on `bus` after `setup`, measured over the last 0.1 s of
/// 0.4 s.
fn bus_level(bus: &str, hz: f32, setup: impl FnOnce(&mut Audio)) -> (f32, RenderStats) {
    let mut audio = headless();
    audio
        .play_asset(&tones(&[(hz, 0.25)], 0.5), looping_on(bus))
        .expect("enqueue");
    audio.drain();
    setup(&mut audio);
    let _ = run(&mut audio, 18);
    let out = run(&mut audio, 6);
    (level(&out, hz), RenderStats::measure(&out))
}

/// Bus volumes compose down the tree: a voice on `World` hears `World + Sfx + Master`.
#[test]
fn bus_gains_multiply_down_the_tree() {
    let (reference, _) = bus_level("World", 440.0, |_| {});
    let (two, _) = bus_level("World", 440.0, |audio| {
        audio.set_bus_volume("Sfx", -6.0, Duration::ZERO).expect("Sfx");
        audio
            .set_bus_volume("World", -6.0, Duration::ZERO)
            .expect("World");
    });
    assert!(
        (db(two / reference) + 12.0).abs() < 0.3,
        "{:.2}",
        db(two / reference)
    );
    let (three, _) = bus_level("World", 440.0, |audio| {
        audio.set_bus_volume("Sfx", -6.0, Duration::ZERO).expect("Sfx");
        audio
            .set_bus_volume("World", -6.0, Duration::ZERO)
            .expect("World");
        audio
            .set_bus_volume("Master", -6.0, Duration::ZERO)
            .expect("Master");
    });
    assert!(
        (db(three / reference) + 18.0).abs() < 0.3,
        "{:.2}",
        db(three / reference)
    );
    // A sibling is untouched by World's volume.
    let (ui, _) = bus_level("Ui", 440.0, |audio| {
        audio
            .set_bus_volume("World", -20.0, Duration::ZERO)
            .expect("World");
    });
    let (ui_reference, _) = bus_level("Ui", 440.0, |_| {});
    assert!(db(ui / ui_reference).abs() < 0.1);
}

/// Muting a parent silences its children; unmuting restores the same level.
#[test]
fn a_muted_parent_silences_its_children_and_unmutes_to_the_same_level() {
    let (reference, _) = bus_level("World", 440.0, |_| {});
    let (muted, stats) = bus_level("World", 440.0, |audio| {
        audio.set_bus_mute("Sfx", true).expect("Sfx");
    });
    assert!(
        muted < reference * 1e-4,
        "muted Sfx silences World: {muted} vs {reference}"
    );
    assert!(stats.is_clean());

    let mut audio = headless();
    audio
        .play_asset(&tones(&[(440.0, 0.25)], 0.5), looping_on("World"))
        .expect("enqueue");
    audio.drain();
    audio.set_bus_mute("Sfx", true).expect("Sfx");
    let _ = run(&mut audio, 12);
    audio.set_bus_mute("Sfx", false).expect("Sfx");
    let after = run(&mut audio, 24);
    let restored = level(&after[after.len() - 6 * TICK_FRAMES * 2..], 440.0);
    assert!(db(restored / reference).abs() < 0.3, "{restored} vs {reference}");
    assert_eq!(audio.output_meter().nonfinite_samples, 0);
}

/// A bus the layout starts muted stays muted (it used to unmute itself on the first tick, with a
/// volume tween from −∞ dB that made NaN), and unmuting it later is finite all the way.
#[test]
fn a_layout_muted_bus_starts_silent_and_unmutes_without_nan() {
    let (reference, _) = bus_level("World", 440.0, |_| {});
    let mut layout = BusLayout::default_tree();
    if let Some(world) = layout.buses.iter_mut().find(|b| b.name == "World") {
        world.mute = true;
    }
    let mut audio = headless_with(layout);
    assert_eq!(audio.bus_muted("World"), Some(true));
    audio
        .play_asset(&tones(&[(440.0, 0.25)], 0.5), looping_on("World"))
        .expect("enqueue");
    audio.drain();
    let silent = run(&mut audio, 12);
    let stats = RenderStats::measure(&silent);
    assert!(stats.peak_db() < -80.0, "the muted bus is silent: {stats:?}");

    audio.set_bus_mute("World", false).expect("World");
    let after = run(&mut audio, 24);
    let stats = RenderStats::measure(&after);
    assert_eq!(stats.nonfinite_samples, 0, "the unmute fade is finite");
    assert_eq!(
        audio.output_meter().nonfinite_samples,
        0,
        "the limiter had nothing to clean up"
    );
    let restored = level(&after[after.len() - 6 * TICK_FRAMES * 2..], 440.0);
    assert!(db(restored / reference).abs() < 0.3, "{restored} vs {reference}");
}

/// The default reverb send is silent: once a one-shot on `World` ends, the output falls to
/// digital silence (no tail leaks out of the send bus; a −60 dB send is amplitude 0 to kira).
#[test]
fn the_default_reverb_send_leaks_nothing() {
    let mut audio = headless();
    audio
        .play_asset(&tones(&[(440.0, 0.5)], 0.1), PlaybackParams::on_bus("World"))
        .expect("enqueue");
    audio.drain();
    let out = run(&mut audio, 30);
    let tail = &out[out.len() - 12 * TICK_FRAMES * 2..];
    let tail = RenderStats::measure(tail);
    let tail_peak = tail.peak_left.max(tail.peak_right);
    assert!(tail_peak < 1e-6, "a silent send leaves no tail: {tail_peak}");
    assert!(
        RenderStats::measure(&out[..TICK_FRAMES * 2]).peak_left > 0.3,
        "the shot played"
    );
}

// --------------------------------------------------------------------------- clicks

/// The steepest step a 440 Hz sine of amplitude 0.5 makes at 48 kHz, with 50 % headroom: any
/// larger step is a discontinuity the content cannot make.
fn click_bound() -> f32 {
    std::f32::consts::TAU * 440.0 * 0.5 / RATE as f32 * 1.5
}

fn started_tone(audio: &mut Audio) -> regia_audio::SoundInstanceId {
    audio
        .start_asset(&tones(&[(440.0, 0.5)], 0.5), looping_on("Sfx"))
        .expect("starts")
}

/// `stop_voice` with no fade on a playing voice ramps out over `DECLICK_FADE` instead of
/// cutting the waveform.
#[test]
fn stopping_a_playing_voice_without_a_fade_does_not_click() {
    let mut audio = headless();
    let id = started_tone(&mut audio);
    let mut out = run(&mut audio, 6);
    // Stop mid-cycle: 6 ticks of 800 frames end 4 800 samples in, 44 cycles of 440 Hz.
    audio.stop_voice(id, Duration::ZERO);
    out.extend(run(&mut audio, 6));
    let stats = RenderStats::measure(&out);
    assert!(
        stats.max_step < click_bound(),
        "max step {} at frame {} (bound {})",
        stats.max_step,
        stats.max_step_frame,
        click_bound()
    );
    let tail = &out[out.len() - 3 * TICK_FRAMES * 2..];
    assert!(tail.iter().all(|s| *s == 0.0), "and it does stop");
    assert_eq!(audio.real_voice_count(), 0);
    // The ramp is the declick floor, not a long fade: 10 ms in, the output is already silent.
    let declick_frames = (DECLICK_FADE.as_secs_f64() * f64::from(RATE)) as usize + 256;
    let after_stop = &out[6 * TICK_FRAMES * 2..];
    let late = RenderStats::measure(&after_stop[declick_frames * 2..]);
    assert!(
        late.peak_left < 1e-6,
        "silent one block after the declick: {late:?}"
    );
}

/// Pausing and resuming with no fade ramp too.
#[test]
fn pausing_and_resuming_without_a_fade_does_not_click() {
    let mut audio = headless();
    let id = started_tone(&mut audio);
    let mut out = run(&mut audio, 5);
    audio.pause_voice(id, Duration::ZERO);
    out.extend(run(&mut audio, 3));
    audio.resume_voice(id, Duration::ZERO);
    out.extend(run(&mut audio, 5));
    let stats = RenderStats::measure(&out);
    assert!(
        stats.max_step < click_bound(),
        "max step {} at frame {}",
        stats.max_step,
        stats.max_step_frame
    );
    let resumed = &out[out.len() - 2 * TICK_FRAMES * 2..];
    assert!(
        RenderStats::measure(resumed).peak_left > 0.45,
        "the voice resumed"
    );
}

/// A voice stopped before the mixer rendered any of it never sounds: no declick tail (a voice
/// stolen in the frame it started stays silent, as before).
#[test]
fn a_voice_stopped_before_it_renders_is_silent() {
    let mut audio = headless();
    let id = started_tone(&mut audio);
    audio.stop_voice(id, Duration::ZERO);
    let out = run(&mut audio, 3);
    assert!(
        out.iter().all(|s| *s == 0.0),
        "peak {}",
        RenderStats::measure(&out).peak_left
    );
}

/// Bus mutes, unmutes and a 20 dB volume jump with no fade all ramp (the tick spreads every
/// bus change over up to two frames).
#[test]
fn bus_mute_and_volume_jumps_do_not_click() {
    let mut audio = headless();
    let _ = started_tone(&mut audio);
    let mut out = run(&mut audio, 4);
    audio.set_bus_mute("Sfx", true).expect("Sfx");
    out.extend(run(&mut audio, 4));
    audio.set_bus_mute("Sfx", false).expect("Sfx");
    out.extend(run(&mut audio, 4));
    audio.set_bus_volume("Sfx", -20.0, Duration::ZERO).expect("Sfx");
    out.extend(run(&mut audio, 4));
    audio.set_bus_volume("Sfx", 0.0, Duration::ZERO).expect("Sfx");
    out.extend(run(&mut audio, 4));
    let stats = RenderStats::measure(&out);
    assert!(
        stats.max_step < click_bound(),
        "max step {} at frame {}",
        stats.max_step,
        stats.max_step_frame
    );
    assert!(stats.is_clean());
}

// --------------------------------------------------------------------------- clipping

/// Sixteen loud voices (peaks sum to 8× full scale) through each output stage.
fn loud_mix(limiter: Option<MasterLimiter>) -> (RenderStats, regia_audio::OutputMeter) {
    let mut audio = headless();
    audio.set_master_limiter(limiter);
    for i in 0..16 {
        let hz = 110.0 * (i + 2) as f32;
        audio
            .play_asset(&tones(&[(hz, 0.5)], 0.5), looping_on("Sfx"))
            .expect("enqueue");
    }
    audio.drain();
    let out = run(&mut audio, 30);
    (RenderStats::measure(&out), audio.output_meter())
}

/// Without the limiter kira hard-clips a loud mix; with a −1 dBFS ceiling no sample reaches
/// full scale, nothing is non-finite, and the meter reports the gain reduction.
#[test]
fn a_loud_mix_is_limited_below_the_ceiling_instead_of_clipped() {
    let (clipped, _) = loud_mix(None);
    assert!(
        clipped.clipped_samples > clipped.frames / 10,
        "bypassed, the mix hard-clips: {clipped:?}"
    );
    let ceiling = MasterLimiter {
        ceiling_db: -1.0,
        release_secs: 0.08,
    };
    let (limited, meter) = loud_mix(Some(ceiling));
    assert!(limited.is_clean(), "{limited:?}");
    let peak = limited.peak_left.max(limited.peak_right);
    assert!(peak <= db_to_gain(-1.0) * (1.0 + 1e-5), "peak {peak}");
    assert!(meter.peak_in > 2.0, "the mix really was over: {meter:?}");
    assert!(
        meter.max_reduction_db > 6.0 && meter.limited_frames > 0,
        "{meter:?}"
    );
    assert!(meter.peak_out <= db_to_gain(-1.0) * (1.0 + 1e-5));
    assert_eq!(meter.nonfinite_samples, 0);
}

/// A mix that stays below full scale renders bit for bit the same with the default limiter as
/// without it.
#[test]
fn a_mix_below_full_scale_is_bit_identical_with_the_limiter() {
    let render = |limiter: Option<MasterLimiter>| {
        let mut audio = headless();
        audio.set_master_limiter(limiter);
        audio
            .play_asset(&tones(&[(440.0, 0.3), (660.0, 0.3)], 0.5), looping_on("Sfx"))
            .expect("enqueue");
        audio.drain();
        (run(&mut audio, 12), audio.output_meter())
    };
    let (bypassed, _) = render(None);
    let (limited, meter) = render(Some(MasterLimiter::default()));
    assert_eq!(bypassed, limited);
    assert_eq!(meter.limited_frames, 0);
    assert_eq!(meter.frames, 12 * TICK_FRAMES as u64);
}
