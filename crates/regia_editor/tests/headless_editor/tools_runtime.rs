// SPDX-License-Identifier: MPL-2.0
//! Runtime-only consumer for the project exported by `tools_author_save_reopen_undo_cook`.
//! The generated Cargo project uses existing runtime crates; the editor is never a dependency.

use regia_app::prelude::*;
use regia_scene::components::Name;
use regia_scene::prefab::PrefabTemplate;
use regia_scene::prelude::*;
use regia_ui::ViewModelAppExt;
use std::path::Path;

#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct Score {
    value: u32,
}

fn read(root: &Path, path: &str) -> Result<String, Box<dyn std::error::Error>> {
    Ok(std::fs::read_to_string(root.join("assets").join(path))?)
}

#[cfg(not(test))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let root = executable.parent().ok_or("package directory missing")?;
    verify(root)
}

/// Loads the package with runtime crates and checks the exact authored state.
pub fn verify(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pak = regia_asset::PakReader::open(&root.join("content/base.fpak"))?;
    let tile = pak
        .manifest()
        .iter()
        .find(|entry| entry.path == "assets/maps/room.tilemap.ron")
        .ok_or("authored tilemap missing from package")?;
    let bytes = pak.read_decompressed(tile.id)?;
    let (_, payload) = regia_asset::decode_cooked_file(&bytes)?;
    let map = regia_2d::tilemap::CookedTileMap::from_postcard(payload)?;
    assert_eq!(map.map.version, regia_2d::tilemap::TILEMAP_FORMAT_VERSION);
    assert_eq!(
        &map.map.tile_layer(0).ok_or("tile layer missing")?.data[6..12],
        &[3, 11, 11, 11, 9, 0]
    );
    for (time, local) in [(0, 2), (99, 2), (100, 3), (199, 3), (200, 2)] {
        assert_eq!(
            regia_2d::batch::animated_tile(&map.map.tilesets[0], 2, time),
            local
        );
    }

    let heightfield = pak
        .manifest()
        .iter()
        .find(|entry| entry.type_path == regia_terrain::stream::TILED_HEIGHTFIELD_TYPE_PATH)
        .ok_or("authored terrain missing from package")?;
    let heights =
        regia_terrain::stream::load_tiled_heightfield_bytes(&pak.read_decompressed(heightfield.id)?)?;
    assert_eq!(
        regia_terrain::height_io::encode_r16(&heights, regia_terrain::height_io::RawEndian::Little),
        std::fs::read(root.join("assets/terrain-expected.r16"))?
    );
    let ui = regia_ui::parse_ui_ron(
        &read(root, "ui/hud.ui.ron")?,
        "ui/hud.ui.ron",
        &regia_ui::UiMigrators::default(),
    )?;
    assert_eq!(ui.version, regia_ui::authoring::UI_RON_VERSION);
    assert_eq!(ui.root.children[1].bindings[0].source, "Score.value");
    let mut localization = load_localization(root)?;
    assert!(
        localization
            .source_text("en")
            .ok_or("English bundle missing")?
            .contains("Workshop")
    );
    assert!(
        localization
            .source_text("fr")
            .ok_or("French bundle missing")?
            .contains("Atelier")
    );
    localization.set_locale("en");
    assert_eq!(localization.locale(), "en");
    let sequence = regia_framework::sequencer::Sequence::from_ron(&read(root, "cine/intro.seq.ron")?)?;
    assert_eq!(
        sequence.version,
        regia_framework::sequencer::SEQUENCE_FORMAT_VERSION
    );
    let frame = sequence.sample(1.0);
    assert_eq!(frame.sounds.len(), 1);
    assert_eq!(frame.sounds[0].sound, "chime");
    assert_eq!(frame.sounds[0].volume_db, -6.0);
    assert_eq!(frame.sounds[0].position, 1.0);
    assert_eq!(sequence.tracks[0].keys.sample_component(1.0, 0), Some(2.0));

    // Instantiate the packaged UI and resolve the same binding and locales as the designer.
    let mut ui_app = App::new();
    ui_app.add_plugins(MinimalPlugins).register_view_model::<Score>();
    ui_app.world_mut().resource_mut::<Score>().value = 7;
    let ui_root = ui_app.world_mut().spawn_empty().id();
    let title = regia_ui::authoring::spawn_node(ui_app.world_mut(), ui_root, &ui.root.children[0]);
    let score = regia_ui::authoring::spawn_node(ui_app.world_mut(), ui_root, &ui.root.children[1]);
    regia_ui::binding::update_bindings(ui_app.world_mut());
    let title_text = ui_app
        .world()
        .get::<regia_ui::Text>(title)
        .ok_or("title text missing")?;
    let score_text = ui_app
        .world()
        .get::<regia_ui::Text>(score)
        .ok_or("score text missing")?;
    assert_eq!(
        regia_ui::text::shaped_string(title_text, &localization),
        "Workshop"
    );
    assert_eq!(regia_ui::text::shaped_string(score_text, &localization), "007");
    assert!(localization.set_locale("fr"));
    assert_eq!(
        regia_ui::text::shaped_string(title_text, &localization),
        "Atelier"
    );

    // Resolve the same prefab chain and load the authored level into a fresh runtime world.
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin)
        .register_type::<regia_render::Camera>();
    let level =
        regia_scene::level::Level::from_ron_str(LevelId::default(), &read(root, "maps/tools.level.ron")?)?;
    let variant_id = level
        .scene
        .entities
        .iter()
        .find_map(|entity| entity.prefab_instance.as_ref())
        .ok_or("level has no variant instance")?
        .prefab
        .0;
    let variant_file =
        regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(&read(root, "props/tall.variant.ron")?)?;
    assert_eq!(variant_file.format_version, 1);
    let file = SceneFile::from_ron_str(&read(root, "props/lamp.prefab.ron")?)?;
    let template = {
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .read();
        PrefabTemplate::from_scene_file(variant_file.base.prefab.0, &file, &registry)?
    };
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    let variant = regia_scene::prefab_variant::PrefabVariant::from_file(variant_id, &variant_file)?;
    app.world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert_variant(variant);
    spawn_scene(app.world_mut(), &level.scene, &SpawnOptions::default())?;
    let mut names = app.world_mut().query::<(&Name, &Transform)>();
    let child = names
        .iter(app.world())
        .find(|(name, _)| name.as_str() == "LampChild")
        .ok_or("variant child missing")?;
    assert_eq!(child.1.translation.y, 3.0);

    // Drive the packaged camera track at an exact time; the app uses no renderer or device.
    let camera = {
        let mut cameras = app
            .world_mut()
            .query_filtered::<Entity, With<regia_render::Camera>>();
        cameras
            .iter(app.world())
            .next()
            .ok_or("authored camera missing")?
    };
    app.add_plugins(regia_framework::sequencer::SequencerPlugin);
    app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
        headless: true,
        ..Default::default()
    }));
    let mut sounds = regia_audio::SoundAssets::default();
    sounds.add_file("chime", root.join("assets/audio/chime.wav"))?;
    app.insert_resource(sounds);
    let mut player = regia_framework::sequencer::SequencePlayer::new(sequence);
    player.clock = regia_framework::sequencer::ClockSource::External;
    player.play();
    player.set_external_time(1.0);
    let player_entity = app
        .world_mut()
        .spawn((
            player,
            regia_framework::sequencer::SequenceBindings::default()
                .with("camera", regia_framework::sequencer::Binding::Entity(camera)),
        ))
        .id();
    app.finish_plugins();
    app.update();
    assert_eq!(
        app.world()
            .get::<Transform>(camera)
            .ok_or("camera transform missing")?
            .translation
            .x,
        2.0
    );
    let voices = app
        .world()
        .resource::<regia_framework::sequencer::audio::SequenceVoices>()
        .voices(player_entity);
    assert_eq!(voices.len(), 1);
    assert_eq!(voices[0].1.volume_db, -6.0);
    let mixed = app
        .world_mut()
        .resource_mut::<regia_audio::Audio>()
        .render_offline(2_048)?;
    assert!(mixed.iter().all(|sample| sample.is_finite()));
    assert!(mixed.iter().any(|sample| *sample != 0.0));
    app.world_mut()
        .get_mut::<regia_framework::sequencer::SequencePlayer>(player_entity)
        .ok_or("sequence player missing")?
        .stop();
    app.update();
    assert!(
        app.world()
            .resource::<regia_framework::sequencer::audio::SequenceVoices>()
            .is_empty()
    );
    println!("tools_runtime_ok: prefab, UI, localization, animated tilemap, camera and audio track data");
    Ok(())
}

// Source-project assertions and packaged binaries intentionally use their actual layouts.
// The package must contain a valid hash-checked bundle index; it never falls back to raw FTL.
#[cfg(not(test))]
fn load_localization(root: &Path) -> Result<regia_ui::Localization, Box<dyn std::error::Error>> {
    let index = regia_ui::loc_bundle::BundleIndex::from_ron(&read(root, "loc/index.ron")?)?;
    let localization = regia_ui::Localization::load_bundles(&index, |file| {
        std::fs::read(root.join("assets").join(file)).map_err(|error| error.to_string())
    })?;
    assert_eq!(
        localization.origin(),
        regia_ui::localization::LocalizationOrigin::Bundle
    );
    Ok(localization)
}

#[cfg(test)]
fn load_localization(root: &Path) -> Result<regia_ui::Localization, Box<dyn std::error::Error>> {
    Ok(regia_ui::Localization::new(
        vec![
            ("en".into(), read(root, "localization/en/main.ftl")?),
            ("fr".into(), read(root, "localization/fr/main.ftl")?),
        ],
        Some("en".into()),
    )?)
}
