// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 8 test 17: the bridges. A `montage` pick plays on the picking entity's
//! `MontagePlayer` (with the `rate` output) and does nothing for an entity without one; a `cue`
//! pick plays the registered sound cue once. Headless audio stands in for a device.

use regia_animation::{AnimationClip, ClipLibrary, Montage, MontagePlayer, SlotDef};
use regia_app::prelude::*;
use regia_audio::{Audio, AudioSettings, CueEntry, SoundAsset, SoundClip, SoundCue};
use regia_framework::chooser::{
    ChooserAppExt, ChooserBinding, ChooserBridgePlugin, ChooserContext, ChooserPlugin, ChooserStats,
    ChooserTable, ChooserValue, Choosers, asset_stem,
};

#[derive(Component)]
struct Armed(bool);

fn armed(world: &World, entity: Entity, ctx: &mut ChooserContext) {
    if let Some(armed) = world.get::<Armed>(entity) {
        ctx.set("armed", ChooserValue::Bool(armed.0));
    }
}

fn hits() -> ChooserTable {
    ChooserTable::from_ron_str(
        r#"(format_version: 1, result: Asset("montage"), inputs: { "armed": Bool }, outputs: { "rate": Float },
            columns: [(id: "armed", input: "armed", kind: Bool), (id: "rate", output: "rate", kind: SetFloat)],
            rows: [
                (id: "armed", cells: { "armed": True, "rate": 1.5 }, result: Asset("anim/hit_armed.montage.ron")),
                (id: "bare", result: Asset("anim/hit_bare.montage.ron")),
            ])"#,
    )
    .expect("hits")
}

fn cues() -> ChooserTable {
    ChooserTable::from_ron_str(
        r#"(format_version: 1, result: Asset("cue"), rows: [(id: "grunt", result: Asset("audio/grunt.cue.ron"))])"#,
    )
    .expect("cues")
}

fn player() -> MontagePlayer {
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("hit", 1.0));
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    for name in ["hit_armed", "hit_bare"] {
        player.add_montage(Montage::single(name, "full_body", "hit"));
    }
    player
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ChooserPlugin, ChooserBridgePlugin));
    enable_fixed_step(&mut app, 60.0);
    app.register_chooser_provider("armed", armed);
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let clip = SoundClip::new(vec![0.2; 8], 48_000).expect("clip");
    audio.register_cue(SoundCue::new(
        "grunt",
        vec![CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight: 1.0,
        }],
    ));
    app.insert_resource(audio);
    {
        let mut choosers = app.world_mut().resource_mut::<Choosers>();
        choosers.insert("anim/hits.chooser.ron", hits());
        choosers.insert("audio/grunts.chooser.ron", cues());
    }
    app
}

#[test]
fn a_montage_pick_plays_on_the_entity_player_and_skips_entities_without_one() {
    let mut app = app();
    let with_player = app
        .world_mut()
        .spawn((
            Armed(true),
            player(),
            ChooserBinding::new("anim/hits.chooser.ron", "armed", "hit"),
        ))
        .id();
    app.world_mut().spawn((
        Armed(false),
        ChooserBinding::new("anim/hits.chooser.ron", "armed", "hit"),
    ));
    app.update();
    let player = app.world().get::<MontagePlayer>(with_player).expect("player");
    let playing = player.instances();
    assert_eq!(playing.len(), 1, "one montage started");
    assert_eq!(
        app.world().resource::<ChooserStats>().montage_plays,
        1,
        "nothing for the entity without a player"
    );
    assert_eq!(asset_stem("anim/hit_armed.montage.ron"), "hit_armed");
}

#[test]
fn a_clip_pick_does_not_start_a_montage() {
    let mut app = app();
    let table = ChooserTable::from_ron_str(
        r#"(format_version: 1, result: Asset("clip"), rows: [(id: "walk", result: Asset("anim/walk.clip.ron"))])"#,
    )
    .expect("clip table");
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("anim/walk.chooser.ron", table);
    let entity = app
        .world_mut()
        .spawn((
            Armed(true),
            player(),
            ChooserBinding::new("anim/walk.chooser.ron", "armed", "walk"),
        ))
        .id();
    app.update();
    let playing = app.world().get::<MontagePlayer>(entity).expect("player");
    assert!(playing.instances().is_empty(), "a clip pick starts nothing");
    assert_eq!(app.world().resource::<ChooserStats>().montage_plays, 0);
}

#[test]
fn a_cue_pick_plays_the_cue_once() {
    let mut app = app();
    app.world_mut().spawn((
        Armed(true),
        ChooserBinding::new("audio/grunts.chooser.ron", "armed", "grunt"),
    ));
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(app.world().resource::<ChooserStats>().cue_plays, 1);
    let mut audio = app.world_mut().resource_mut::<Audio>();
    audio.drain();
    assert_eq!(audio.played_count(), 1);
}

/// `volume_db`, `bus` and a world position are not applied: `play_cue` records no position.
/// `PlayedOneShot` does not store the cue's level, so this asserts the play path, not a dB number.
#[test]
fn a_cue_pick_ignores_volume_bus_and_position_outputs() {
    let mut app = app();
    let clip = SoundClip::new(vec![0.2; 8], 48_000).expect("clip");
    let mut cue = SoundCue::new(
        "quiet",
        vec![CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight: 1.0,
        }],
    );
    cue.volume_db = -6.0;
    // A bus of the default tree, other than the table's `bus` output: an unknown bus refuses the
    // play (`UnknownBus`), which is not what this test is about (ADR-5667).
    cue.bus = Some("Sfx".to_string());
    app.world_mut().resource_mut::<Audio>().register_cue(cue);
    let table = ChooserTable::from_ron_str(
        r#"(format_version: 1, result: Asset("cue"),
            outputs: { "volume_db": Float, "bus": Name },
            columns: [
                (id: "volume_db", output: "volume_db", kind: SetFloat),
                (id: "bus", output: "bus", kind: SetName),
            ],
            rows: [(id: "quiet", cells: { "volume_db": -12.0, "bus": "Music" },
                    result: Asset("audio/quiet.cue.ron"))])"#,
    )
    .expect("cue table");
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("audio/quiet.chooser.ron", table);
    app.world_mut().spawn((
        Armed(true),
        ChooserBinding::new("audio/quiet.chooser.ron", "armed", "quiet"),
    ));
    app.update();
    assert_eq!(app.world().resource::<ChooserStats>().cue_plays, 1);
    let audio = app.world().resource::<Audio>();
    assert_eq!(audio.played_count(), 1);
    assert_eq!(
        audio.played().first().map(|shot| shot.position),
        Some(None),
        "play_cue is not positional"
    );
}
