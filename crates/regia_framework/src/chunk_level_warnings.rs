// SPDX-License-Identifier: MPL-2.0
//! One warning per unmounted chunk per level load (`docs/specs/40-content-chunks-dlc-asset-packs.md`
//! § 2.5). A level that references assets of a chunk the game has not mounted spawns the rest; the
//! [`AssetServer`] logs each such chunk once and this plugin starts a new window at every level load.

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_scene::level::LevelLoaded;

/// Resets the asset server's per-chunk warnings when a level loads.
#[derive(Debug, Default)]
pub struct ChunkLevelWarningsPlugin;

impl Plugin for ChunkLevelWarningsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<LevelLoaded>()
            .add_systems(PreUpdate, reset_on_level_load);
    }
}

fn reset_on_level_load(mut loaded: MessageReader<LevelLoaded>, server: Option<ResMut<AssetServer>>) {
    if loaded.read().count() > 0
        && let Some(mut server) = server
    {
        server.reset_unavailable_warnings();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetServerSettings;
    use regia_asset::content::ChunkMap;
    use regia_scene::level::LevelId;
    use regia_test_utils::unique_temp_dir;
    use std::collections::BTreeMap;

    #[test]
    fn each_level_load_warns_once_per_unavailable_chunk() {
        let root = unique_temp_dir("chunk-level-warnings");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("mkdir");
        regia_asset::AssetManifest::default()
            .write_to_cache(&cache)
            .expect("manifest");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        let (first, second, third) = (
            regia_asset::AssetId::new_random(),
            regia_asset::AssetId::new_random(),
            regia_asset::AssetId::new_random(),
        );
        let assignment = BTreeMap::from([
            (first, "arena".to_owned()),
            (second, "arena".to_owned()),
            (third, "forest".to_owned()),
        ]);
        server.set_chunk_map(Some(ChunkMap::from_assignment(&assignment)));
        let mut app = App::new();
        app.add_plugins(ChunkLevelWarningsPlugin);
        app.insert_resource(server);

        let load = |app: &mut App, ids: &[regia_asset::AssetId]| {
            let mut server = app.world_mut().resource_mut::<AssetServer>();
            for id in ids {
                assert!(server.load_mesh(*id).is_err(), "unavailable assets fail the load");
            }
        };
        load(&mut app, &[first, second, third]);
        assert_eq!(
            app.world().resource::<AssetServer>().warned_unavailable_chunks(),
            ["arena", "forest"],
            "two assets of one chunk, one warning"
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<AssetServer>()
                .warned_unavailable_chunks()
                .len(),
            2,
            "no level loaded: the window stays open"
        );

        app.world_mut().write_message(LevelLoaded {
            level: LevelId(regia_core::id::Guid::new_random()),
            entities: 1,
        });
        app.update();
        assert!(
            app.world()
                .resource::<AssetServer>()
                .warned_unavailable_chunks()
                .is_empty(),
            "a level load starts a new window"
        );
        load(&mut app, &[second]);
        assert_eq!(
            app.world().resource::<AssetServer>().warned_unavailable_chunks(),
            ["arena"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
