// SPDX-License-Identifier: MIT-0
//! `--ambience` (ADR-2442), without a GPU: the flag installs the forest ambience bed with generated placeholder
//! loops, the bed hears the `--foliage-wind` breeze through its wind override (still trees: calm), and off (the
//! default) installs nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::audio::{AmbienceBed, AudioPlugin, AudioSettings, SoundAssets, TEST_TONE_PREFIX};
use regia::prelude::*;
use regia_app::enable_fixed_step;
use showcase::STEP_HZ;
use showcase::app::{Options, SHOWCASE_WIND, ShowcasePlugin};
use showcase::scene::load_scene;

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

fn app(ambience: bool, foliage_wind: bool) -> App {
    let scene = load_scene(&example_dir()).expect("the example scene loads");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins(AudioPlugin);
    enable_fixed_step(&mut app, STEP_HZ);
    app.add_plugins(ShowcasePlugin {
        scene: Arc::new(scene),
        options: Options {
            plain: true,
            ambience,
            foliage_wind,
            ..Options::default()
        },
    });
    app
}

#[test]
fn the_flag_plays_the_forest_bed_in_the_showcase_wind() {
    let mut app = app(true, true);
    for _ in 0..3 {
        app.update();
    }
    let bed = app.world().resource::<AmbienceBed>();
    assert_eq!(
        bed.wind_override,
        Some(SHOWCASE_WIND),
        "the bed hears the breeze the trees sway in"
    );
    let speed = SHOWCASE_WIND[0].hypot(SHOWCASE_WIND[1]);
    assert!((bed.inputs.wind_m_per_s - speed).abs() < 1e-4, "{:?}", bed.inputs);
    let sounds = app.world().resource::<SoundAssets>();
    for (layer, state) in bed.layers.iter().zip(&bed.state) {
        let name = layer
            .sound
            .as_deref()
            .expect("every forest layer has a test tone");
        assert!(name.starts_with(TEST_TONE_PREFIX), "{name}");
        assert!(sounds.by_name(name).is_some(), "{name} is registered");
        assert!(state.voice.is_some(), "{} loops", layer.name);
    }
    assert!(
        bed.state[bed.layer_index("wind").unwrap()].target_db > -40.0,
        "a 5.4 m/s breeze is heard"
    );
}

#[test]
fn still_trees_leave_the_wind_silent_and_off_installs_nothing() {
    let mut app = app(true, false);
    for _ in 0..3 {
        app.update();
    }
    let bed = app.world().resource::<AmbienceBed>();
    assert_eq!(bed.wind_override, None);
    for name in ["wind", "gust", "rustle"] {
        let i = bed.layer_index(name).unwrap();
        assert!(
            bed.state[i].target_db <= regia::audio::MUTE_DB,
            "{name} is silent with still trees"
        );
    }
    let mut off = app_off();
    for _ in 0..3 {
        off.update();
    }
    assert!(off.world().get_resource::<AmbienceBed>().is_none());
}

fn app_off() -> App {
    app(false, true)
}
