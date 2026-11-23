// SPDX-License-Identifier: MPL-2.0
//! The M20 cloud-save round trip (`specs/24` § 2): `regia_framework`'s `SaveGame`
//! serialises a world into bytes, `SteamRemoteStorage` stores them in Steam Remote Storage
//! (the mock backend here), and the bytes restore into a fresh world. This is the seam a
//! shipping game uses when it wants Steam-controlled cloud saves instead of Auto-Cloud.

use regia_app::prelude::*;
use regia_framework::save_game::{
    RegisterSaveableExt, SaveGame, SavePlugin, SaveSpec, Saveable, SaveableResource,
};
use regia_scene::Transform;
use regia_steam::{MockSteam, SaveBackend, SteamPlugin, SteamRemoteStorage};

/// A collected-state marker: exactly what `samples/third_person` saves.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Collectible {
    /// Which collectible this is.
    id: u32,
}

/// The saveable counter resource.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
struct CollectedCount(u32);

impl SaveableResource for CollectedCount {}

/// An app whose saves can go through the mock cloud.
fn app(mock: std::sync::Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        SavePlugin::default(),
        SteamPlugin::default().with_backend(mock),
    ))
    .register_type::<Collectible>()
    .register_type::<Transform>()
    .register_saveable_resource::<CollectedCount>();
    app.update(); // let the Steam resource exist
    app
}

#[test]
fn save_game_round_trips_through_steam_remote_storage() {
    let mock = MockSteam::shared();
    let mut app = app(mock.clone());
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    let cloud = SteamRemoteStorage::new(&steam);

    // World state: two collectibles remain, one was collected (counter 1).
    app.world_mut().spawn((
        Saveable,
        Collectible { id: 1 },
        Transform {
            translation: regia_core::math::Vec3::new(1.0, 0.5, 0.0),
            ..Default::default()
        },
    ));
    app.world_mut().spawn((
        Saveable,
        Collectible { id: 2 },
        Transform {
            translation: regia_core::math::Vec3::new(2.0, 0.5, 0.0),
            ..Default::default()
        },
    ));
    app.insert_resource(CollectedCount(1));

    // Encode → cloud.
    let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode save-game bytes");
    cloud.save("Chapter1", &bytes).expect("store in the cloud");
    assert_eq!(
        mock.cloud_file("saves/Chapter1.frts").map(|b| b.len()),
        Some(bytes.len()),
        "the exact save-game container reached the backend"
    );

    // Collect the rest, then change the world further.
    let mut collect_all = app.world_mut().query_filtered::<Entity, With<Collectible>>();
    for entity in collect_all.iter(app.world_mut()).collect::<Vec<_>>() {
        app.world_mut().despawn(entity);
    }
    app.insert_resource(CollectedCount(3));

    // Cloud → decode: the saved state is back, byte-identical route, no filesystem.
    let stored = cloud.load("Chapter1").expect("cloud read").expect("slot exists");
    let restored = SaveGame::decode(app.world_mut(), &stored, &SaveSpec::default()).expect("decode");
    assert_eq!(restored.entity_count, 2, "both collectibles are back");

    let world = app.world_mut();
    let mut q = world.query::<&Collectible>();
    let mut ids: Vec<u32> = q.iter(world).map(|c| c.id).collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(world.resource::<CollectedCount>().0, 1, "counter restored");
}

#[test]
fn unavailable_steam_surfaces_as_unavailable() {
    let mock = MockSteam::shared_unavailable();
    let app = app(mock);
    let steam = app.world().resource::<regia_steam::Steam>().clone();
    let cloud = SteamRemoteStorage::new(&steam);
    assert_eq!(
        cloud.load("Any"),
        Err(regia_steam::SaveError::Unavailable),
        "the game's cue to fall back to the local backend"
    );
}
