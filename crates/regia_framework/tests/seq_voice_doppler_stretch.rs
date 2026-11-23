// SPDX-License-Identifier: MPL-2.0
//! Sequence voices in v5 (ADR-3197): Doppler from the sampled section motion, occlusion through
//! the occlusion query and its low-pass, and pitch-preserving sound sections, checked offline over
//! render-out mixdown buffers rendered with a fixed 60 Hz step (800 frames at 48 kHz per frame).
//!
//! Tolerances (stated once, used below):
//! - Doppler: the measured frequency is within 0.5 % of `440 × c / (c − v_radial)`; with the field
//!   off, within 0.2 % of 440 Hz.
//! - Occlusion: a fully occluded voice's volume is the tuning's `occluded_gain_db` below the open
//!   one within 0.05 dB once settled, its cutoff the tuning's within 1 %, and a 6 kHz tone loses
//!   more than 30 dB in the rendered mix.
//! - Stretch: a 440 Hz tone stays within 0.5 % of 440 Hz and lasts `1 s / rate` within 25 ms; no
//!   sample-to-sample step of a speech-like fixture exceeds 1.5 times the unstretched render's.
//! - Seek: from 10 ms after a seek into a stretched section, the mix matches playing through to
//!   that point within 1e-3 (the signal peaks at 0.5).
//! - Hash: a render-out mix is bit-identical on repeat, and a v4 file (every new field off after
//!   the migration) renders the hash the v4 build rendered (x86_64).
#![cfg(feature = "client")]

use std::time::Duration;

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioOcclusion, AudioSettings, ListenerFrame, MixdownSettings, SoundAsset, SoundAssets, SoundClip,
};
use regia_core::math::Vec3;
use regia_framework::sequencer::{
    Binding, Sequence, SequenceBindings, SequencePlayer, SequenceVoice, SequenceVoices, SequencerPlugin,
};

const STEP: Duration = Duration::from_nanos(16_666_667);
const BLOCK: usize = 800;
const RATE: f32 = 48_000.0;

fn sine(hz: f32, secs: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..(secs * RATE) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE).sin() * 0.5)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"))
}

/// A speech-like fixture: 8 harmonics at 1/k over a fundamental gliding 110 → 170 Hz, under a
/// 4 Hz syllable envelope.
fn speech(secs: f32) -> SoundAsset {
    let n = (secs * RATE) as usize;
    let mut phase = 0.0f32;
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / RATE;
            phase += std::f32::consts::TAU * (110.0 + 60.0 * t / secs) / RATE;
            let voice: f32 = (1..=8).map(|k| (phase * k as f32).sin() / k as f32).sum();
            voice * (0.5 - 0.5 * (std::f32::consts::TAU * 4.0 * t).cos()) * 0.25
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"))
}

/// An app with the sequencer, a headless mixer in a running mixdown, a listener at the origin
/// facing −Z, and the fixture sounds.
fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z));
    audio.begin_mixdown(&MixdownSettings::default()).expect("mixdown");
    app.insert_resource(audio);
    let mut sounds = SoundAssets::default();
    sounds.add("tone", sine(440.0, 12.0));
    sounds.add("short", sine(440.0, 1.0));
    sounds.add("bright", sine(6_000.0, 3.0));
    sounds.add("speech", speech(2.0));
    app.insert_resource(sounds);
    app
}

/// Spawns a playing player of `ron` with `car` bound to a fresh entity; returns the player.
fn spawn(app: &mut App, ron: &str) -> Entity {
    let sequence = Sequence::from_ron(ron).expect("sequence");
    let car = app.world_mut().spawn(regia_scene::Transform::default()).id();
    let mut player = SequencePlayer::new(sequence);
    player.play();
    app.world_mut()
        .spawn((
            player,
            SequenceBindings::default().with("car", Binding::Entity(car)),
        ))
        .id()
}

/// One frame: the update, the mixer's tick and one block of the mix (interleaved stereo).
fn frame(app: &mut App) -> Vec<f32> {
    app.update();
    let mut audio = app.world_mut().resource_mut::<Audio>();
    audio.tick(STEP.as_secs_f64());
    audio.render_mixdown(BLOCK).expect("block").mix
}

fn render(app: &mut App, frames: usize) -> Vec<f32> {
    (0..frames).flat_map(|_| frame(app)).collect()
}

/// Left plus right of an interleaved stereo buffer.
fn mono(mix: &[f32]) -> Vec<f32> {
    mix.as_chunks::<2>().0.iter().map(|[l, r]| l + r).collect()
}

/// Frequency of `x` by rising zero crossings (linear interpolation) over `from..to` frames.
fn frequency(x: &[f32], from: usize, to: usize) -> f32 {
    let mut crossings = Vec::new();
    for i in from.max(1)..to.min(x.len()) {
        let (a, b) = (x[i - 1], x[i]);
        if a < 0.0 && b >= 0.0 {
            crossings.push((i - 1) as f32 + a / (a - b));
        }
    }
    assert!(crossings.len() > 2, "no tone in {from}..{to}");
    let span = crossings[crossings.len() - 1] - crossings[0];
    (crossings.len() - 1) as f32 * RATE / span
}

fn voices(app: &App, player: Entity) -> Vec<(Vec<usize>, SequenceVoice)> {
    app.world().resource::<SequenceVoices>().voices(player)
}

fn fnv(mix: &[f32]) -> u64 {
    mix.iter().fold(0xcbf2_9ce4_8422_2325, |h, s| {
        s.to_bits()
            .to_le_bytes()
            .iter()
            .fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
    })
}

/// A car passes 2 m in front of the listener along +X at 30 m/s (x = 30 t − 150), carrying the
/// tone; a flat second section plays under it.
fn passing(version: u32, extra_spatial: &str) -> String {
    format!(
        r#"(version: {version}, name: "pass", tracks: [
        (name: "move", target: "car", property: "transform.position",
            keys: Vec3([(time: 0.0, value: (-150.0, 0.0, -2.0)), (time: 10.0, value: (150.0, 0.0, -2.0))])),
        (name: "engine", keys: Audio([
            (sound: "tone", start: 0.0, end: 6.0,
                spatial: Some((source: Binding("car"), min_distance: 500.0, max_distance: 1000.0{extra_spatial}))),
        ])),
        (name: "bed", keys: Audio([(sound: "speech", start: 0.5, end: 2.0, offset: 0.1, volume_db: -12.0, time_scale: 1.25)])),
    ], markers: [(name: "end", time: 10.0)])"#
    )
}

/// The render-out hash of the v4 fixture, from the v4 build (lane base, `integ/v037c` plus
/// `lane/sprint-integ-v037b`, `seq-video-sections` and `audio-convolution-ambience`). Only an
/// x86_64 build checks it (the hash is that build's), so only x86_64 compiles it: on aarch64
/// (macOS, Android) it was dead code and warned.
#[cfg(target_arch = "x86_64")]
const V4_HASH: u64 = 0x8ee8_b62f_3681_8314;

/// A render-out mix repeats bit for bit; a v4 file (every new field off once migrated) renders
/// what the v4 build rendered; the v5 fields change it.
#[test]
fn render_out_hash_repeats_and_a_v4_file_matches_the_v4_build() {
    let run = |ron: &str| {
        let mut app = app();
        spawn(&mut app, ron);
        fnv(&render(&mut app, 120))
    };
    let v4 = run(&passing(4, ""));
    println!("v4 fixture hash: {v4:#018x}");
    assert_eq!(v4, run(&passing(4, "")), "repeat");
    #[cfg(target_arch = "x86_64")]
    assert_eq!(v4, V4_HASH, "the v4 build's hash: {v4:#018x}");
    let off = run(&passing(5, ", doppler: 0.0"));
    assert_eq!(off, v4, "v5 with every new field off is the v4 render");
    let on = run(&passing(5, ""));
    assert_eq!(on, run(&passing(5, "")), "Doppler on repeats");
    assert_ne!(on, v4, "Doppler changes the mix");
}

/// The passing car shifts the tone by `c / (c − v_radial)` approaching and receding, measured in
/// the rendered mix; with `doppler: 0.0` the tone stays at 440 Hz.
#[test]
fn a_passing_emitter_shifts_by_the_doppler_formula_and_not_with_the_field_off() {
    let c = regia_audio::SPEED_OF_SOUND;
    let only_engine = |extra: &str| {
        format!(
            r#"(version: 5, name: "pass", tracks: [
            (name: "move", target: "car", property: "transform.position",
                keys: Vec3([(time: 0.0, value: (-150.0, 0.0, -2.0)), (time: 10.0, value: (150.0, 0.0, -2.0))])),
            (name: "engine", keys: Audio([
                (sound: "tone", start: 0.0, end: 10.0,
                    spatial: Some((source: Binding("car"), min_distance: 500.0, max_distance: 1000.0{extra}))),
            ])),
        ], markers: [(name: "end", time: 10.0)])"#
        )
    };
    let mut pass = app();
    let player = spawn(&mut pass, &only_engine(""));
    let mix = mono(&render(&mut pass, 360));
    // Approaching, t = 1..2 s: x = −120..−90, radial speed 30 |x| / √(x² + 4) ≈ 29.99 m/s.
    let radial = |x: f32| 30.0 * x.abs() / (x * x + 4.0).sqrt();
    let want_in = 440.0 * c / (c - radial(-105.0));
    let got_in = frequency(&mix, 48_000, 96_000);
    assert!(
        (got_in / want_in - 1.0).abs() < 0.005,
        "approaching: {got_in} Hz, want {want_in}"
    );
    // Past the listener, t = 5.625..6 s is x = 18.75..30: receding.
    let want_out = 440.0 * c / (c + radial(22.5));
    let got_out = frequency(&mix, 270_000, 288_000);
    assert!(
        (got_out / want_out - 1.0).abs() < 0.005,
        "receding: {got_out} Hz, want {want_out}"
    );
    let v = voices(&pass, player);
    let velocity = v[0].1.velocity.expect("Doppler velocity");
    assert!(
        (velocity[0] - 30.0).abs() < 0.01 && velocity[1].abs() < 1e-4,
        "{velocity:?}"
    );
    // Off: no shift.
    let mut off = app();
    let player = spawn(&mut off, &only_engine(", doppler: 0.0"));
    let mix = mono(&render(&mut off, 120));
    let got = frequency(&mix, 48_000, 96_000);
    assert!((got / 440.0 - 1.0).abs() < 0.002, "off: {got} Hz");
    assert_eq!(voices(&off, player)[0].1.doppler, 1.0);
    assert_eq!(voices(&off, player)[0].1.velocity, None);
}

/// A seek mid-pass gives the Doppler ratio playing through gave at that clock: the velocity is
/// the track's slope, not the jump.
#[test]
fn a_seek_does_not_jump_the_doppler_pitch() {
    let ron = passing(5, "");
    let mut through = app();
    let a = spawn(&mut through, &ron);
    render(&mut through, 120); // to t = 2.0
    let mut seeked = app();
    let b = spawn(&mut seeked, &ron);
    render(&mut seeked, 3);
    seeked
        .world_mut()
        .get_mut::<SequencePlayer>(b)
        .expect("player")
        .seek(2.0 - STEP.as_secs_f64());
    frame(&mut seeked); // the clock lands on 2.0
    let (pa, pb) = (voices(&through, a), voices(&seeked, b));
    let engine = |v: &[(Vec<usize>, SequenceVoice)]| {
        v.iter()
            .find(|(p, _)| p[0] == 1)
            .map(|(_, v)| *v)
            .expect("engine")
    };
    assert!(
        (engine(&pa).doppler - engine(&pb).doppler).abs() < 1e-4,
        "{} vs {}",
        engine(&pa).doppler,
        engine(&pb).doppler
    );
    assert!(engine(&pb).doppler > 1.05, "the seeked voice is shifted at once");
}

/// A fixed-position occluding section queues its ray under the sequencer owner; a full report
/// takes it down by the tuning's gain and closes its low-pass to the tuning's cutoff, smoothed on
/// the render clock; the rendered 6 kHz tone loses more than 30 dB. Off by default: no ray.
#[test]
fn an_occluded_voice_drops_by_the_tuning_and_its_filter_applies() {
    let ron = |occlusion: &str| {
        format!(
            r#"(version: 5, name: "wall", tracks: [
            (name: "hum", keys: Audio([(sound: "bright", start: 0.0, end: 3.0,
                spatial: Some((source: Position((0.0, 0.0, -5.0)), min_distance: 50.0, max_distance: 100.0{occlusion}))),
            ])),
        ], markers: [(name: "end", time: 3.0)])"#
        )
    };
    let run = |occlusion: &str, report: bool| {
        let mut app = app();
        app.insert_resource(AudioOcclusion::default());
        let player = spawn(&mut app, &ron(occlusion));
        let mut tail = Vec::new();
        for i in 0..150 {
            let block = frame(&mut app);
            if i == 0 {
                let key = app
                    .world()
                    .resource::<SequenceVoices>()
                    .occlusion_key(player, &[0, 0]);
                let pending = app.world().resource::<AudioOcclusion>().pending().to_vec();
                assert_eq!(key.is_some(), !occlusion.is_empty());
                // The audio sync is not running: queue as it would, then read the rays.
                let mut occ = app.world_mut().resource_mut::<AudioOcclusion>();
                occ.queue(&[]);
                let rays = occ.take_pending();
                assert!(pending.is_empty());
                assert_eq!(rays.iter().any(|r| Some(r.emitter) == key), key.is_some());
                if let (Some(key), true) = (key, report) {
                    occ.report(key, 1.0);
                }
            }
            if i >= 120 {
                tail.extend(block);
            }
        }
        (voices(&app, player)[0].1, tail)
    };
    let tuning = AudioOcclusion::default().tuning;
    let (plain, plain_mix) = run("", false);
    let (open, _) = run(", occlusion: true", false);
    let (shut, shut_mix) = run(", occlusion: true", true);
    assert_eq!(plain.cutoff_hz, None, "off by default: no filter");
    assert_eq!(open.occlusion, 0.0);
    assert!(
        (open.volume_db - plain.volume_db).abs() < 1e-4,
        "unoccluded: same level"
    );
    assert_eq!(shut.occlusion, 1.0, "settled on the report");
    assert!(
        (shut.volume_db - plain.volume_db - tuning.occluded_gain_db).abs() < 0.05,
        "{} vs {}",
        shut.volume_db,
        plain.volume_db
    );
    let cutoff = shut.cutoff_hz.expect("cutoff");
    assert!(
        (cutoff / tuning.occluded_cutoff_hz - 1.0).abs() < 0.01,
        "{cutoff} Hz"
    );
    let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
    let drop_db = 20.0 * (rms(&shut_mix) / rms(&plain_mix)).log10();
    assert!(drop_db < -30.0, "the mix drops {drop_db} dB");
}

/// The smoothing runs on the render clock: after `n` fixed steps the amount is the tuning's
/// smoothing iterated `n` times with the step.
#[test]
fn occlusion_smooths_on_the_render_clock() {
    let ron = r#"(version: 5, name: "wall", tracks: [
        (name: "hum", keys: Audio([(sound: "bright", start: 0.0, end: 3.0,
            spatial: Some((source: Position((0.0, 0.0, -5.0)), occlusion: true))),
        ])),
    ], markers: [(name: "end", time: 3.0)])"#;
    let mut app = app();
    app.insert_resource(AudioOcclusion::default());
    let player = spawn(&mut app, ron);
    frame(&mut app);
    let key = app
        .world()
        .resource::<SequenceVoices>()
        .occlusion_key(player, &[0, 0])
        .expect("key");
    app.world_mut().resource_mut::<AudioOcclusion>().report(key, 1.0);
    let tuning = AudioOcclusion::default().tuning;
    let mut want = 0.0f32;
    for _ in 0..5 {
        frame(&mut app);
        want = tuning.smooth(want, 1.0, STEP.as_secs_f32());
        assert_eq!(voices(&app, player)[0].1.occlusion, want);
    }
}

/// A flat pitch-preserving section of a 1 s, 440 Hz tone at `rate`, from 0.1 s, longer than the
/// stretched sound.
fn stretched(rate: f32, mode: &str) -> String {
    let end = 0.1 + 1.5 / rate;
    format!(
        r#"(version: 5, name: "stretch", tracks: [
        (name: "line", keys: Audio([(sound: "short", start: 0.1, end: {end}, time_scale: {rate}, time_scale_mode: {mode})])),
    ], markers: [(name: "end", time: 3.5)])"#
    )
}

/// A stretched 440 Hz tone stays at 440 Hz at 0.5, 0.75, 1.5 and 2 and lasts `1 s / rate`.
#[test]
fn a_stretched_tone_keeps_its_pitch_and_lasts_the_scaled_time() {
    for rate in [0.5f32, 0.75, 1.5, 2.0] {
        let mut app = app();
        let player = spawn(&mut app, &stretched(rate, "PreservePitch"));
        let frames = ((0.1 + 1.0 / rate + 0.3) * 60.0) as usize;
        let mix = mono(&render(&mut app, 12));
        let v = voices(&app, player);
        assert!(v[0].1.stretched, "rate {rate}: the voice plays a stretch");
        assert!(
            (v[0].1.pitch - 1.0).abs() < 1e-6,
            "rate {rate}: pitch {}",
            v[0].1.pitch
        );
        let rest = mono(&render(&mut app, frames));
        let all: Vec<f32> = mix.into_iter().chain(rest).collect();
        let loud: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, s)| s.abs() > 0.05)
            .map(|(i, _)| i)
            .collect();
        let (first, last) = (loud[0], loud[loud.len() - 1]);
        let lasted = (last - first) as f32 / RATE;
        assert!(
            (lasted - 1.0 / rate).abs() < 0.025,
            "rate {rate}: lasted {lasted} s"
        );
        let hz = frequency(&all, first + 4_800, last - 4_800);
        assert!((hz / 440.0 - 1.0).abs() < 0.005, "rate {rate}: {hz} Hz");
    }
}

/// Rate 1 in pitch-preserving mode is the varispeed render bit for bit.
#[test]
fn a_pitch_preserving_section_at_rate_one_is_bit_identical() {
    let run = |mode: &str| {
        let mut app = app();
        spawn(&mut app, &stretched(1.0, mode));
        render(&mut app, 90)
    };
    let a = run("PreservePitch");
    let b = run("Varispeed");
    assert!(a.iter().any(|s| s.abs() > 0.1), "heard");
    assert_eq!(
        a.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
        b.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
    );
}

/// No clicks: a speech-like line stretched to 0.75 and 1.5 has no sample-to-sample step above
/// 1.5 times the largest of its unstretched render.
#[test]
fn a_stretched_speech_line_has_no_clicks() {
    let line = |rate: f32, mode: &str| {
        let end = 0.1 + 2.0 / rate;
        format!(
            r#"(version: 5, name: "speech", tracks: [
            (name: "line", keys: Audio([(sound: "speech", start: 0.1, end: {end}, time_scale: {rate}, time_scale_mode: {mode})])),
        ], markers: [(name: "end", time: 3.0)])"#
        )
    };
    let max_step = |mix: &[f32]| {
        mix.as_chunks::<2>()
            .0
            .iter()
            .map(|[l, _]| *l)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max)
    };
    let render_line = |ron: String, frames: usize| {
        let mut app = app();
        spawn(&mut app, &ron);
        render(&mut app, frames)
    };
    let bound = 1.5 * max_step(&render_line(line(1.0, "Varispeed"), 140));
    for rate in [0.75f32, 1.5] {
        let step = max_step(&render_line(line(rate, "PreservePitch"), 200));
        assert!(step <= bound, "rate {rate}: step {step} over {bound}");
    }
}

/// A seek into a stretched section gives the samples playing through to that point gave.
#[test]
fn a_seek_into_a_stretched_section_matches_playing_through() {
    let ron = stretched(0.75, "PreservePitch");
    let mut through = app();
    spawn(&mut through, &ron);
    let all = render(&mut through, 90);
    // Frame `i` playing through ends on clock `(i + 1)` steps. Seek from before the section (no
    // voice to fade out) to 59 steps; the next frame lands on 60 steps, frame 59's clock.
    let mut seeked = app();
    let player = spawn(&mut seeked, &ron);
    render(&mut seeked, 3);
    seeked
        .world_mut()
        .get_mut::<SequencePlayer>(player)
        .expect("player")
        .seek(59.0 * STEP.as_secs_f64());
    let after: Vec<f32> = render(&mut seeked, 20);
    let want = &all[59 * BLOCK * 2..79 * BLOCK * 2];
    let skip = 480 * 2; // 10 ms
    let worst = after[skip..]
        .iter()
        .zip(&want[skip..])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(want.iter().any(|s| s.abs() > 0.1), "heard");
    assert!(worst < 1e-3, "seek vs play-through: {worst}");
}
