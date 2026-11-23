// SPDX-License-Identifier: MPL-2.0
//! A cooked level keeps its `LevelSettings` (found by the spec 10 § 2.3 registry round trip,
//! ADR-5608). The cook stores a scene resource as reflected postcard; `LevelSettings` uses its own
//! serde, which skips `terrain`, `look.lightmap` and `look.captures` when they are `None`, so the
//! positional postcard read ran past the end and the cooked level loaded with default settings
//! and a "failed to load" warning.
//!
//! `cargo test -p regia_scene --test cooked_level_settings`

use regia_app::prelude::*;
use regia_scene::ScenePlugin;
use regia_scene::cooked::round_trip_cooked;
use regia_scene::format::SceneFile;
use regia_scene::level::{LEVEL_SETTINGS_PATH, LevelSettings};
use regia_scene::spawn::SpawnOptions;

fn level(settings: &str) -> SceneFile {
    let text = format!(
        r#"(
    format_version: 1,
    entities: [],
    resources: {{
        "{LEVEL_SETTINGS_PATH}": {settings},
    }},
)"#
    );
    SceneFile::from_ron_str(&text).expect("level text parses")
}

fn cooked_settings(settings: &str) -> (LevelSettings, Vec<String>) {
    let mut app = App::new();
    app.add_plugins(ScenePlugin);
    let file = level(settings);
    let (_, spawned) =
        round_trip_cooked(app.world_mut(), &file, &SpawnOptions::default()).expect("cook and spawn");
    let settings = app.world().resource::<LevelSettings>().clone();
    (settings, spawned.warnings)
}

#[test]
fn a_cooked_level_without_terrain_keeps_its_settings() {
    let (settings, warnings) = cooked_settings("(gravity: (0.0, -20.0, 0.0))");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(settings.gravity.y, -20.0, "the cooked level lost its gravity");
}

#[test]
fn a_cooked_level_with_every_optional_part_keeps_its_settings() {
    // With nothing skipped by serde the postcard read always worked; this pins it stays so.
    let mut full = LevelSettings {
        gravity: regia_core::math::Vec3::new(0.0, -3.7, 0.0),
        ..LevelSettings::default()
    };
    full.terrain = Some(Default::default());
    full.look.lightmap = Some(Default::default());
    full.look.captures = Some(Default::default());
    let text = full.to_ron().expect("settings text");
    let (settings, warnings) = cooked_settings(text.trim_end());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(settings, full);
}

/// ADR-7232: `LevelSettings` round-trips through reflected postcard on its own (not only through
/// the cook's RON fallback), with and without the optional `terrain`, `look.lightmap` and
/// `look.captures`, while level text still omits them when they are `None`.
#[test]
fn level_settings_round_trip_through_reflected_postcard_and_text_still_omits_none() {
    use regia_ecs::serialize::{from_postcard, to_postcard};
    use regia_scene::level::{LevelCaptures, LevelLightmap, LevelTerrain};

    let mut app = App::new();
    app.add_plugins(ScenePlugin);
    let registry = app.type_registry().read();

    let bare = LevelSettings::default();
    let mut full = LevelSettings {
        terrain: Some(LevelTerrain {
            layers: "maps/arena/terrain.layers.ron".into(),
            map: "maps/arena".into(),
            ..LevelTerrain::default()
        }),
        ..LevelSettings::default()
    };
    full.look.lightmap = Some(LevelLightmap::default());
    full.look.captures = Some(LevelCaptures::default());
    full.game_rules = Some("deathmatch".into());

    for value in [&bare, &full] {
        let bytes = to_postcard(value, &registry).unwrap_or_else(|e| panic!("to_postcard: {e}"));
        let back = from_postcard(&bytes, &registry).unwrap_or_else(|e| panic!("from_postcard: {e}"));
        let back = back
            .downcast_ref::<LevelSettings>()
            .unwrap_or_else(|| panic!("from_postcard gave another type: {back:?}"));
        assert_eq!(back, value, "reflected postcard round trip");
    }

    let text = bare.to_ron().unwrap_or_else(|e| panic!("to_ron: {e}"));
    for absent in ["terrain", "lightmap", "captures"] {
        assert!(
            !text.contains(absent),
            "`{absent}` is not written when None:\n{text}"
        );
    }
    assert_eq!(
        LevelSettings::from_ron(&text).unwrap_or_else(|e| panic!("from_ron: {e}")),
        bare
    );
    let text = full.to_ron().unwrap_or_else(|e| panic!("to_ron: {e}"));
    for present in ["terrain", "lightmap", "captures"] {
        assert!(text.contains(present), "`{present}` is written when set:\n{text}");
    }
    assert_eq!(
        LevelSettings::from_ron(&text).unwrap_or_else(|e| panic!("from_ron: {e}")),
        full
    );
}
