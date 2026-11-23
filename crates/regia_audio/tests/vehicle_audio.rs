// SPDX-License-Identifier: MPL-2.0
//! Vehicle audio through the real mixer (ADR-3271): the placeholder car's voices on attached
//! emitters, rendered offline at fixed steps (no device, no wall clock, no sleeps).

use regia_audio::{
    Audio, AudioEmitter, AudioSettings, BusLayout, EmitterFrame, EmitterUpdate, ListenerFrame,
    VehicleAudioDef, VehicleAudioInputs, VehicleAudioModel, VehicleEmitterEntities, VehicleEmitterSlot,
    VehicleSoundBank, VehicleVoiceMode, VehicleVoiceSet, measure_loudness, placeholder_sound,
};
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::EntityIndex;
use regia_ecs::prelude::Entity;

const RATE: u32 = 48_000;
const DT: f32 = 1.0 / 60.0;
/// Stereo frames per 1/60 s tick.
const TICK_FRAMES: usize = 800;

fn entity(index: u32) -> Entity {
    Entity::from_index(EntityIndex::from_raw_u32(index).expect("a valid entity index"))
}

fn headless() -> Audio {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout::default_tree(),
        ..AudioSettings::default()
    });
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z));
    audio
}

fn emitter() -> AudioEmitter {
    AudioEmitter {
        min_distance: 2.0,
        max_distance: 200.0,
        ..AudioEmitter::default()
    }
}

struct Car {
    model: VehicleAudioModel,
    bank: VehicleSoundBank,
    voices: VehicleVoiceSet,
    entities: VehicleEmitterEntities,
}

impl Car {
    fn new(def: VehicleAudioDef, first_entity: u32) -> Self {
        let model = VehicleAudioModel::new(def, 4).expect("valid");
        let bank = VehicleSoundBank::resolve(&model, |name| placeholder_sound(name).and_then(Result::ok));
        assert!(bank.missing.is_empty(), "{:?}", bank.missing);
        Self {
            model,
            bank,
            voices: VehicleVoiceSet::default(),
            entities: VehicleEmitterEntities {
                engine: entity(first_entity),
                exhaust: entity(first_entity + 1),
                wheels: (0..4).map(|i| entity(first_entity + 2 + i)).collect(),
            },
        }
    }

    fn updates(&self, at: Vec3) -> Vec<EmitterUpdate> {
        let mut out = vec![
            update(self.entities.engine, at + Vec3::new(0.0, 0.3, -1.4)),
            update(self.entities.exhaust, at + Vec3::new(0.4, -0.25, 2.1)),
        ];
        for (i, wheel) in self.entities.wheels.iter().enumerate() {
            let x = if i.is_multiple_of(2) { -0.8 } else { 0.8 };
            let z = if i < 2 { -1.3 } else { 1.3 };
            out.push(update(*wheel, at + Vec3::new(x, -0.4, z)));
        }
        out
    }
}

fn update(entity: Entity, position: Vec3) -> EmitterUpdate {
    EmitterUpdate {
        entity,
        emitter: emitter(),
        frame: EmitterFrame {
            position,
            ..EmitterFrame::default()
        },
    }
}

fn engine_only(rpm: f32, throttle: f32) -> VehicleAudioInputs {
    VehicleAudioInputs {
        rpm,
        throttle,
        engine_torque: 300.0 * throttle,
        ..VehicleAudioInputs::default()
    }
}

/// Renders `ticks` frames of the car at `at`, with `inputs(tick)` stepped twice per tick (120 Hz).
fn render(car: &mut Car, at: Vec3, ticks: usize, inputs: impl Fn(usize) -> VehicleAudioInputs) -> Vec<f32> {
    let mut audio = headless();
    let updates = car.updates(at);
    // One sync first so every emitter has its place before a voice starts.
    audio.sync_emitter_frames(&updates, DT, None, None, None);
    let mut out = Vec::with_capacity(ticks * TICK_FRAMES * 2);
    for tick in 0..ticks {
        let i = inputs(tick);
        car.model.step(&i, DT / 2.0);
        car.model.step(&i, DT / 2.0);
        car.voices.apply(
            &mut audio,
            &car.model,
            &car.bank,
            &car.entities,
            &emitter(),
            VehicleVoiceMode::Full,
        );
        audio.sync_emitter_frames(&updates, DT, None, None, None);
        audio.tick(f64::from(DT));
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    out
}

/// Spectral centroid (Hz) of a mono block by a direct DFT over 20 Hz to 4 kHz bins.
fn centroid(samples: &[f32]) -> f32 {
    let n = samples.len();
    let bin_hz = RATE as f32 / n as f32;
    let (mut weighted, mut total) = (0.0f64, 0.0f64);
    let mut k = (20.0 / bin_hz).ceil() as usize;
    while (k as f32) * bin_hz <= 4000.0 {
        let w = std::f64::consts::TAU * k as f64 / n as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (t, &x) in samples.iter().enumerate() {
            let a = w * t as f64;
            re += f64::from(x) * a.cos();
            im -= f64::from(x) * a.sin();
        }
        let mag = (re * re + im * im).sqrt();
        weighted += mag * k as f64 * f64::from(bin_hz);
        total += mag;
        k += 1;
    }
    (weighted / total.max(1e-12)) as f32
}

fn mono(frames: &[f32]) -> Vec<f32> {
    frames
        .as_chunks::<2>()
        .0
        .iter()
        .map(|f| 0.5 * (f[0] + f[1]))
        .collect()
}

#[test]
fn a_three_second_rpm_ramp_mixes_down_finite_below_full_scale_with_a_rising_centroid() {
    let mut car = Car::new(VehicleAudioDef::placeholder(), 10);
    let ticks = 180;
    let frames = render(&mut car, Vec3::new(0.0, 0.0, -6.0), ticks, |tick| {
        engine_only(900.0 + 6000.0 * tick as f32 / ticks as f32, 1.0)
    });
    assert!(frames.iter().all(|s| s.is_finite()), "finite");
    let loudness = measure_loudness(&frames, 2, RATE).expect("measurable");
    assert!(
        loudness.true_peak_db < 0.0,
        "true peak {} dBTP",
        loudness.true_peak_db
    );
    assert!(frames.iter().any(|s| s.abs() > 1e-3), "audible");
    let samples = mono(&frames);
    // 0.25 s windows early, middle and late in the ramp.
    let window = (RATE / 4) as usize;
    let at = |secs: f32| {
        let start = (secs * RATE as f32) as usize;
        centroid(&samples[start..start + window])
    };
    let (early, middle, late) = (at(0.4), at(1.4), at(2.6));
    assert!(
        early < middle && middle < late,
        "centroid rises: {early} {middle} {late} Hz"
    );
    assert!(car.voices.counters().0 > 0);
}

#[test]
fn a_layer_pitch_reaches_the_mixer_through_the_emitter_pipeline() {
    // One on-load loop recorded at 2,000 rpm: a pure 100 Hz sine (a test tone, so no other line
    // can land on the measured ones). At 4,000 rpm it must play at pitch 2, at 200 Hz: the emitter
    // sync composes the layer pitch with Doppler instead of replacing it.
    let mut def = VehicleAudioDef::placeholder();
    def.engine.on_load.truncate(1);
    def.engine.on_load[0].rpm = 2000.0;
    def.engine.on_load[0].sound = "placeholder:engine_on_2000".into();
    def.engine.off_load.clear();
    def.whine = None;
    def.wind = None;
    def.tires = Default::default();
    let level = |rpm: f32| {
        let mut car = Car::new(def.clone(), 30);
        let sine: Vec<f32> = (0..RATE as usize)
            .map(|i| 0.5 * (std::f32::consts::TAU * 100.0 * i as f32 / RATE as f32).sin())
            .collect();
        let sine = regia_audio::SoundAsset::from_clip(
            &regia_audio::SoundClip::new(sine, RATE).expect("a sine is a valid clip"),
        );
        car.bank = VehicleSoundBank::resolve(&car.model, |_| Some(sine.clone()));
        let frames = render(&mut car, Vec3::new(0.0, 0.0, -4.0), 90, |_| engine_only(rpm, 1.0));
        let samples = mono(&frames[frames.len() - 2 * 24_000..]);
        let goertzel = |hz: f32| {
            let k = std::f32::consts::TAU * hz / RATE as f32;
            let coeff = 2.0 * k.cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in &samples {
                let s0 = x + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len() as f32
        };
        (goertzel(100.0), goertzel(200.0))
    };
    let (at_2000_low, at_2000_high) = level(2000.0);
    let (at_4000_low, at_4000_high) = level(4000.0);
    assert!(
        at_2000_low > 2.0 * at_2000_high,
        "pitch 1: {at_2000_low} vs {at_2000_high}"
    );
    assert!(
        at_4000_high > 2.0 * at_4000_low,
        "pitch 2: {at_4000_high} vs {at_4000_low}"
    );
}

#[test]
fn the_voice_modes_bound_the_live_voices() {
    let mut audio = headless();
    let mut car = Car::new(VehicleAudioDef::placeholder(), 50);
    let updates = car.updates(Vec3::new(0.0, 0.0, -8.0));
    audio.sync_emitter_frames(&updates, DT, None, None, None);
    let mut i = engine_only(3200.0, 0.5);
    i.speed = 20.0;
    i.wheel_count = 4;
    for w in &mut i.wheels[..4] {
        w.grounded = true;
        w.contact_speed = 20.0;
        w.slip_angle = 0.3;
    }
    let mut counts = Vec::new();
    for mode in [
        VehicleVoiceMode::Full,
        VehicleVoiceMode::Reduced,
        VehicleVoiceMode::Silent,
    ] {
        for _ in 0..10 {
            car.model.step(&i, DT);
            car.voices
                .apply(&mut audio, &car.model, &car.bank, &car.entities, &emitter(), mode);
            audio.sync_emitter_frames(&updates, DT, None, None, None);
            audio.tick(f64::from(DT));
            let _ = audio.render_offline(TICK_FRAMES).expect("offline backend");
        }
        counts.push(car.voices.live_count());
    }
    assert!(
        counts[0] >= 8,
        "full: engine, whine, wind, squeal and rolling: {counts:?}"
    );
    assert_eq!(counts[1], 1, "reduced: one engine layer");
    assert_eq!(counts[2], 0, "silent");
    let _ = VehicleEmitterSlot::Engine;
}
