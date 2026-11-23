// SPDX-License-Identifier: MPL-2.0
//! The ambience bed heard through the mixer (ADR-2442): the offline backend renders what the bed
//! plays, and the measured level, not only the computed one, follows the scene wind and the rain.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    AmbienceBed, Audio, AudioPlugin, AudioSettings, BedLayer, SoundAssets, TEST_TONE_PREFIX,
    test_tone_loopgens,
};
use regia_scene::weather::{SceneWind, SurfaceWeather};

const RATE: usize = 48_000;
const FRAMES_PER_UPDATE: usize = RATE / 60;

fn app_with(layers: &[&str]) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins(AudioPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut sounds = SoundAssets::default();
    let forest = AmbienceBed::forest();
    let mut kept: Vec<BedLayer> = Vec::new();
    for (layer, text) in test_tone_loopgens() {
        if !layers.contains(&layer) {
            continue;
        }
        let name = format!("{TEST_TONE_PREFIX}{layer}");
        sounds
            .add_loopgen(name.clone(), &text)
            .expect("the test tone renders");
        let mut def = forest.layers[forest.layer_index(layer).expect("a forest layer")].clone();
        def.sound = Some(name);
        kept.push(def);
    }
    app.insert_resource(sounds);
    app.insert_resource(AmbienceBed::with_layers(kept));
    app
}

/// Runs `updates` frames, rendering the mixer after each; returns the RMS level (dBFS) of the
/// last `measured` frames' output.
fn run_and_measure(app: &mut App, updates: usize, measured: usize) -> f32 {
    let mut tail: Vec<f32> = Vec::new();
    for i in 0..updates {
        app.update();
        let out = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(FRAMES_PER_UPDATE)
            .expect("the offline backend renders");
        if i + measured >= updates {
            tail.extend_from_slice(&out);
        }
    }
    let mean_square =
        tail.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / tail.len().max(1) as f64;
    if mean_square <= 1e-20 {
        -200.0
    } else {
        (10.0 * mean_square.log10()) as f32
    }
}

#[test]
fn a_still_scene_renders_no_wind_and_a_breeze_renders_it() {
    let mut app = app_with(&["wind", "gust", "rustle"]);
    app.insert_resource(SceneWind::CALM);
    let calm = run_and_measure(&mut app, 240, 60);
    assert!(
        calm < -90.0,
        "calm: the wind layers render silence, measured {calm} dBFS"
    );

    app.insert_resource(SceneWind::manual([8.0, 0.0]));
    let breeze = run_and_measure(&mut app, 600, 60);
    assert!(breeze > -45.0, "8 m/s: the wind is heard, measured {breeze} dBFS");

    // The wind drops: after the release the output is silent again.
    app.insert_resource(SceneWind::CALM);
    let after = run_and_measure(&mut app, 1200, 60);
    assert!(after < -90.0, "calm again: silent, measured {after} dBFS");
}

#[test]
fn a_stronger_wind_renders_louder() {
    let mut levels = Vec::new();
    for speed in [1.0f32, 4.0, 12.0] {
        let mut app = app_with(&["wind"]);
        app.insert_resource(SceneWind::manual([speed, 0.0]));
        levels.push(run_and_measure(&mut app, 600, 60));
    }
    assert!(levels[0] < levels[1] && levels[1] < levels[2], "{levels:?}");
    // The curve's 16 dB between 1 and 12 m/s survives the mixer within the loop's own variation.
    assert!(levels[2] - levels[0] > 15.0, "{levels:?}");
}

#[test]
fn rain_renders_while_it_falls_and_the_drips_follow() {
    let mut app = app_with(&["rain", "drips"]);
    app.insert_resource(SurfaceWeather::DRY);
    let dry = run_and_measure(&mut app, 120, 60);
    assert!(dry < -90.0, "dry: silent, measured {dry} dBFS");
    app.insert_resource(SurfaceWeather {
        rain_mm_per_h: 8.0,
        wetness: 1.0,
        ..SurfaceWeather::DRY
    });
    let raining = run_and_measure(&mut app, 600, 60);
    // The placeholder loop sits near −27 dBFS; the curve puts 8 mm/h at −19 dB.
    assert!(
        raining > -60.0 && raining > dry + 40.0,
        "raining {raining} dBFS, dry {dry} dBFS"
    );
    {
        let bed = app.world().resource::<AmbienceBed>();
        assert_eq!(
            bed.level_db("drips"),
            Some(regia_audio::MUTE_DB),
            "no drips while it rains"
        );
    }
    app.insert_resource(SurfaceWeather {
        rain_mm_per_h: 0.0,
        wetness: 1.0,
        ..SurfaceWeather::DRY
    });
    let _ = run_and_measure(&mut app, 1200, 60);
    let bed = app.world().resource::<AmbienceBed>();
    assert_eq!(bed.level_db("rain"), Some(regia_audio::MUTE_DB));
    assert!(bed.level_db("drips").is_some_and(|db| db > -40.0));
}

#[test]
fn removing_a_layer_sound_stops_its_voice() {
    let mut app = app_with(&["air"]);
    let level = run_and_measure(&mut app, 120, 30);
    assert!(level > -80.0, "the air tone plays: {level}");
    app.world_mut().resource_mut::<AmbienceBed>().layers[0].sound = None;
    let silent = run_and_measure(&mut app, 120, 30);
    assert!(silent < -90.0, "{silent}");
    let bed = app.world().resource::<AmbienceBed>();
    assert!(bed.state[0].voice.is_none());
}

fn spot_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins(AudioPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut sounds = SoundAssets::default();
    // Placeholder loops: the rain tone stands in for water, short clips for the calls.
    let (_, water) = test_tone_loopgens()
        .into_iter()
        .find(|(layer, _)| *layer == "rain")
        .expect("a rain tone");
    sounds.add_loopgen("water", &water).expect("renders");
    for i in 0..3 {
        let clip = regia_audio::SoundClip::new(vec![0.2; 4_800], 48_000).expect("a clip");
        sounds.add(format!("call_{i}"), regia_audio::SoundAsset::from_clip(&clip));
    }
    app.insert_resource(sounds);
    app
}

fn place(position: regia_core::math::Vec3) -> (regia_scene::Transform, regia_scene::GlobalTransform) {
    let t = regia_scene::Transform::from_translation(position);
    (t, regia_scene::GlobalTransform(t.compute_affine()))
}

#[test]
fn an_occluded_water_spot_renders_quieter_and_duller() {
    let mut app = spot_app();
    let spot = app
        .world_mut()
        .spawn((
            regia_audio::AmbientSpot::water("water"),
            place(regia_core::math::Vec3::new(10.0, 0.0, 0.0)),
        ))
        .id();
    let open = run_and_measure(&mut app, 120, 60);
    assert!(open > -60.0, "the stream is heard: {open}");
    let status = |app: &App| {
        app.world()
            .resource::<Audio>()
            .emitter_status()
            .into_iter()
            .find(|s| s.entity == spot)
            .expect("the spot plays through an emitter")
    };
    assert!(status(&app).occlusion < 0.01);
    // A provider stand-in: a hill between the listener and the stream, reported every frame.
    let mut hidden_tail = Vec::new();
    for i in 0..240 {
        app.world_mut()
            .resource_mut::<regia_audio::AudioOcclusion>()
            .report(spot, 1.0);
        app.update();
        let out = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(FRAMES_PER_UPDATE)
            .expect("renders");
        if i >= 180 {
            hidden_tail.extend_from_slice(&out);
        }
    }
    let ms = hidden_tail.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / hidden_tail.len() as f64;
    let hidden = (10.0 * ms.max(1e-20).log10()) as f32;
    assert!(status(&app).occlusion > 0.95, "{}", status(&app).occlusion);
    // −15 dB of occlusion gain plus the 700 Hz low-pass on white-ish noise.
    assert!(hidden < open - 15.0, "open {open} dBFS, hidden {hidden} dBFS");
}

#[test]
fn bird_calls_are_emitters_that_take_occlusion_rays_and_go_away() {
    let mut app = spot_app();
    let mut birds = regia_audio::AmbientSpot::birds(vec!["call_0".into(), "call_1".into(), "call_2".into()]);
    birds.kind = regia_audio::AmbientSpotKind::Scatter {
        min_interval_secs: 0.5,
        max_interval_secs: 1.0,
        radius_m: 5.0,
        height_m: 3.0,
    };
    birds.seed = 11;
    app.world_mut()
        .spawn((birds, place(regia_core::math::Vec3::new(0.0, 0.0, -20.0))));
    let mut rays_to_calls = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut variants = Vec::new();
    for _ in 0..600 {
        app.update();
        let _ = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(FRAMES_PER_UPDATE);
        let pending: Vec<_> = app
            .world()
            .resource::<regia_audio::AudioOcclusion>()
            .pending()
            .iter()
            .map(|r| r.emitter)
            .collect();
        let mut query = app
            .world_mut()
            .query::<(regia_ecs::prelude::Entity, &regia_audio::AmbientCall)>();
        let mut calls: Vec<_> = query.iter(app.world()).map(|(e, c)| (e, c.variant)).collect();
        calls.sort();
        for (entity, variant) in &calls {
            if seen.insert(*entity) {
                variants.push(*variant);
            }
        }
        rays_to_calls += pending
            .iter()
            .filter(|e| calls.iter().any(|(c, _)| c == *e))
            .count();
    }
    assert!(
        variants.windows(2).all(|w| w[0] != w[1]),
        "a call repeated the previous variant: {variants:?}"
    );
    let state = app.world().resource::<regia_audio::AmbienceSpotState>();
    assert!((6..=21).contains(&state.calls_spawned), "{}", state.calls_spawned);
    assert!(rays_to_calls > 0, "calls are occluding emitters");
    assert!(
        state.live_calls <= 2,
        "ended calls are despawned: {}",
        state.live_calls
    );
    let schedule = state.schedules.values().next().expect("one spot");
    assert_eq!(schedule.calls, state.calls_spawned);
}

#[test]
fn removing_the_bed_resource_silences_it() {
    let mut app = app_with(&["air"]);
    let level = run_and_measure(&mut app, 120, 30);
    assert!(level > -80.0, "the air tone plays: {level}");
    app.world_mut().remove_resource::<AmbienceBed>();
    let silent = run_and_measure(&mut app, 120, 30);
    assert!(silent < -90.0, "a removed bed falls silent: {silent}");
}
