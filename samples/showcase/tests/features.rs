// SPDX-License-Identifier: MIT-0
//! The optional engine features of a scene (grass, valley mist, water, entry streak), without a GPU:
//! a procedural scene with every feature on loads and builds what each needs, a scene with every file
//! absent loads and runs as before (no feature resource, plugin or entity), and a malformed file is
//! refused with its own error code, never a panic.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::prelude::*;
use regia_app::enable_fixed_step;
use regia_render::ValleyMist;
use regia_terrain::{WaterBody, WaterClock, WaterShape};
use regia_test_utils::unique_temp_dir;
use regia_vfx::EntryStreak;
use showcase::STEP_HZ;
use showcase::app::{Options, ShowcaseClock, ShowcasePlugin, ShowcaseStats};
use showcase::example::{write_example_scene, write_feature_scene};
use showcase::features::FeatureOptions;
use showcase::scene::{SceneData, load_scene, load_scene_with};
use showcase::{ErrorCode, ShowcaseError};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

fn feature_scene(tag: &str) -> PathBuf {
    let dir = unique_temp_dir(tag);
    write_feature_scene(&dir).expect("write the feature scene");
    dir
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn load_err(dir: &Path) -> ShowcaseError {
    load_scene(dir).expect_err("the scene must be refused")
}

fn app(scene: SceneData) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, STEP_HZ);
    app.add_plugins(ShowcasePlugin {
        scene: Arc::new(scene),
        options: Options {
            plain: true,
            ..Options::default()
        },
    });
    app
}

fn count<T: Component>(app: &mut App) -> usize {
    let mut query = app.world_mut().query::<&T>();
    query.iter(app.world()).count()
}

#[test]
fn the_feature_scene_turns_every_feature_on() {
    let dir = feature_scene("showcase-features-on");
    let scene = load_scene(&dir).expect("the feature scene loads");
    assert_eq!(scene.features.names(), ["grass", "mist", "water", "entry"]);

    let grass = scene.features.grass.as_ref().expect("grass");
    assert_eq!(grass.set.classes.len(), 1);
    assert_eq!(grass.mask.classes(), 1);
    assert_eq!(grass.mask.side(), 512, "a 1024 m map at 2 m texels");

    let mist = scene.features.mist.as_ref().expect("mist");
    let grid = mist
        .grid
        .as_ref()
        .expect("the mist follows the loaded heightfield");
    assert!(
        grid.width() >= 64,
        "a 16 m grid over 1024 m: {} cells",
        grid.width()
    );

    let water = scene.features.water.as_ref().expect("water");
    let WaterShape::Ribbon { points } = &water.shape else {
        panic!("a ribbon");
    };
    assert_eq!(points.len(), 40);
    assert!(
        points[0].position[0] > points[39].position[0],
        "source first, flowing west"
    );

    let entry = scene.features.entry.as_ref().expect("entry");
    assert_eq!(entry.track.points().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn grass_grows_where_the_dead_grass_channel_is_painted() {
    let dir = feature_scene("showcase-features-grass");
    let scene = load_scene(&dir).expect("loads");
    let mask = &scene.features.grass.as_ref().expect("grass").mask;
    let world = scene.map.world_width();
    // The west half is painted `grass` (pack0 R), the east half `rock`, which the grass set does not use.
    let west = mask.mean_density(0, -300.0, 0.0, 40.0, world);
    let east = mask.mean_density(0, 300.0, 0.0, 40.0, world);
    assert!(west > 0.2, "grass on the painted layer: {west}");
    assert!(east < 0.02, "no grass on the other layer: {east}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_grass_bake_is_a_pure_function_of_the_scene() {
    let a = feature_scene("showcase-features-bake-a");
    let b = feature_scene("showcase-features-bake-b");
    let (a_scene, b_scene) = (load_scene(&a).expect("a"), load_scene(&b).expect("b"));
    let image = |s: &SceneData| {
        s.features
            .grass
            .as_ref()
            .and_then(|g| g.mask.image(0).map(<[u8]>::to_vec))
    };
    assert_eq!(image(&a_scene), image(&b_scene));
    let _ = (std::fs::remove_dir_all(a), std::fs::remove_dir_all(b));
}

#[test]
fn the_grass_flag_names_the_missing_layers() {
    // The example scene has no grass file and layers named grass, rock and snow: the default file
    // names dead_grass and shoots, and the error says so.
    let flagged = load_scene_with(&example_dir(), &FeatureOptions { grass: true })
        .expect_err("the default names are not in the example's layers");
    assert_eq!(flagged.code, ErrorCode::GrassMalformed);
    assert!(
        flagged.message.contains("dead_grass") && flagged.message.contains("rock"),
        "{flagged}"
    );
    // Without the flag the same scene loads with no grass.
    assert!(
        load_scene(&example_dir())
            .expect("loads")
            .features
            .grass
            .is_none()
    );
}

#[test]
fn a_scene_with_every_feature_file_absent_is_unchanged() {
    let dir = unique_temp_dir("showcase-features-off");
    write_example_scene(&dir).expect("example");
    let scene = load_scene(&dir).expect("loads");
    assert!(scene.features.names().is_empty(), "{:?}", scene.features.names());
    assert!(scene.sky.mist.is_none());

    let mut app = app(scene);
    for _ in 0..4 {
        app.update();
    }
    assert!(!app.world().contains_resource::<ValleyMist>());
    assert!(!app.world().contains_resource::<WaterClock>(), "no WaterPlugin");
    assert!(
        !app.world()
            .contains_resource::<regia_terrain::TerrainGrassHandle>()
    );
    assert_eq!(count::<WaterBody>(&mut app), 0);
    assert_eq!(count::<EntryStreak>(&mut app), 0);
    let stats = app.world().resource::<ShowcaseStats>().clone();
    assert_eq!(
        (
            stats.water_bodies,
            stats.entry_streaks,
            stats.mist,
            stats.grass_installed,
            stats.contact_installed
        ),
        (0, 0, false, false, false)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_features_run_on_the_frame_clock() {
    let dir = feature_scene("showcase-features-clock");
    let scene = load_scene(&dir).expect("loads");
    let entry = scene.features.entry.clone().expect("entry");
    let mut app = app(scene);
    let mut last_clock = i64::MIN;
    for _ in 0..40 {
        app.update();
        let clock = app.world().resource::<ShowcaseClock>().frame;
        let evaluated = clock - 1;
        let mut query = app.world_mut().query::<&EntryStreak>();
        let streaks: Vec<(u32, bool)> = query.iter(app.world()).map(|s| (s.frame, s.playing)).collect();
        assert_eq!(streaks.len(), 1, "one entry streak entity");
        assert_eq!(
            streaks[0],
            (entry.streak_frame(evaluated), false),
            "the clock sets the frame"
        );
        let mist = app.world().resource::<ValleyMist>();
        assert_eq!(
            mist.time_s,
            ValleyMist::time_for_frame(evaluated.max(0) as u64, 60.0)
        );
        last_clock = clock;
    }
    assert!(last_clock > 0, "the clock moved");
    assert_eq!(count::<WaterBody>(&mut app), 1, "the stream is one ribbon entity");
    assert!(
        app.world().contains_resource::<WaterClock>(),
        "WaterPlugin is in the app"
    );
    let stats = app.world().resource::<ShowcaseStats>().clone();
    assert_eq!(
        (stats.water_bodies, stats.entry_streaks, stats.mist),
        (1, 1, true)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn malformed_feature_files_are_refused_with_their_codes() {
    let cases: [(&str, &str, ErrorCode); 8] = [
        ("terrain/grass.ron", "nope", ErrorCode::GrassMalformed),
        (
            "terrain/grass.ron",
            "Grass(version: 1, dead_grass: \"lava\")",
            ErrorCode::GrassMalformed,
        ),
        (
            "terrain/grass.ron",
            "Grass(version: 3)",
            ErrorCode::GrassMalformed,
        ),
        (
            "terrain/stream.json",
            "{\"points\": 3}",
            ErrorCode::WaterMalformed,
        ),
        (
            "terrain/stream.json",
            "{\"points\": []}",
            ErrorCode::WaterMalformed,
        ),
        (
            "entry.ron",
            "Entry(version: 1, points: [])",
            ErrorCode::EntryMalformed,
        ),
        (
            "entry.ron",
            "Entry(version: 1, points: [(time: 1.0, position: (0.0, 0.0, 0.0)), (time: 1.0, position: (1.0, 0.0, 0.0))])",
            ErrorCode::EntryMalformed,
        ),
        (
            "sky.ron",
            "Sky(version: 1, weather: Preset(\"fair\"), mist: Some((density: -1.0)))",
            ErrorCode::SkyMalformed,
        ),
    ];
    for (index, (rel, text, code)) in cases.into_iter().enumerate() {
        let dir = feature_scene(&format!("showcase-features-bad-{index}"));
        write(&dir, rel, text);
        let err = load_err(&dir);
        assert_eq!(err.code, code, "{rel}: {err}");
        assert!(
            err.path.ends_with(rel),
            "{rel}: the error names {}",
            err.path.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_new_error_codes_are_listed_and_lowercase() {
    for code in [
        ErrorCode::GrassMalformed,
        ErrorCode::WaterMalformed,
        ErrorCode::EntryMalformed,
    ] {
        assert!(ErrorCode::ALL.contains(&code));
        let text = code.as_str();
        assert!(
            text.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }
}
