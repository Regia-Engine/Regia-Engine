// SPDX-License-Identifier: MPL-2.0
//! Rules-driven local joins instantiate an authored character and possess its root.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{Character, CharacterInput};
use regia_framework::player::{PlayerController, PlayerManager, PlayerPlugin};
use regia_framework::rules::{GameRules, GameRulesResource, RulesCtx, RulesPlugin, SpawnRequest};
use regia_framework::sim_bridge::Team;
use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged, GamepadConnected};
use regia_scene::components::Name;
use regia_scene::prefab::{PrefabInstance, PrefabLink};
use regia_scene::{GlobalTransform, ScenePlugin, Transform};

struct PrefabRules;

impl GameRules for PrefabRules {
    fn on_player_joined(&mut self, _: &mut RulesCtx, _: Entity) -> Option<SpawnRequest> {
        Some(SpawnRequest {
            prefab: Some("actors/hero.prefab.ron".into()),
            transform: Transform::from_translation(Vec3::new(3.0, 4.0, 5.0)),
            team: Some(3),
        })
    }
}

/// A project with the fixture prefab and an app whose rules spawn it, after one pad join.
fn joined_app(name: &str) -> (App, std::path::PathBuf) {
    let root = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(root.join("actors")).expect("fixture directory");
    regia_test_utils::write_atomic(
        &root.join("actors/hero.prefab.ron"),
        include_bytes!("fixtures/spawn_character.prefab.ron"),
    )
    .expect("write fixture");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        ScenePlugin,
        regia_input::InputPlugin::default(),
        RulesPlugin,
        PlayerPlugin,
    ))
    .register_type::<CharacterInput>()
    .register_type::<Team>()
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
    .insert_resource(GameRulesResource {
        rules: Box::new(PrefabRules),
    })
    .insert_resource(
        regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server"),
    );
    app.world_mut().write_message(GamepadConnected {
        device: 7,
        name: "Fixture Pad".into(),
    });
    app.world_mut().write_message(GamepadButtonChanged {
        device: 7,
        button: GamepadButton::South,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.update();
    (app, root)
}

#[test]
fn local_join_possesses_the_authored_prefab_root_and_preserves_its_children() {
    let (mut app, root) = joined_app("spawn-prefab-join");
    let world = app.world_mut();
    assert_eq!(world.resource::<PlayerManager>().count(), 1);
    let controller = world
        .query::<&PlayerController>()
        .single(world)
        .expect("controller");
    let pawn = controller.possessed;
    assert_eq!(controller.view_target, pawn);
    assert_eq!(
        world.get::<Name>(pawn).map(Name::as_str),
        Some("FixtureCharacter")
    );
    assert!(world.get::<Character>(pawn).is_some());
    assert!(world.get::<CharacterInput>(pawn).expect("authored input").crouch);
    assert_eq!(world.get::<Team>(pawn), Some(&Team(3)));
    assert!(world.get::<PrefabLink>(pawn).is_some());
    assert_eq!(world.query::<&PrefabInstance>().iter(world).count(), 1);
    assert_eq!(
        world.get::<Transform>(pawn).expect("transform").translation,
        Vec3::new(3.0, 4.0, 5.0)
    );
    let child = world
        .query::<(Entity, &Name)>()
        .iter(world)
        .find_map(|(entity, name)| (name.as_str() == "FixtureAccessory").then_some(entity))
        .expect("accessory");
    assert_eq!(world.get::<ChildOf>(child).map(ChildOf::parent), Some(pawn));
    assert_eq!(
        Vec3::from(
            world
                .get::<GlobalTransform>(child)
                .expect("propagated child")
                .0
                .translation
        ),
        Vec3::new(3.0, 6.0, 5.0)
    );
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn despawning_the_prefab_pawn_takes_its_wrapper_and_instance() {
    let (mut app, root) = joined_app("spawn-prefab-despawn");
    let world = app.world_mut();
    let pawn = world
        .query::<&PlayerController>()
        .single(world)
        .expect("controller")
        .possessed;
    let wrapper = world.get::<ChildOf>(pawn).map(ChildOf::parent).expect("wrapper");
    assert!(world.get::<PrefabInstance>(wrapper).is_some());
    // A game despawning the possessed entity (a death or a respawn).
    regia_scene::hierarchy::despawn_tree(world, pawn);
    world.flush();
    assert!(
        world.get_entity(wrapper).is_err(),
        "the identity wrapper outlived its pawn"
    );
    assert_eq!(world.query::<&PrefabInstance>().iter(world).count(), 0);
    app.update();
    std::fs::remove_dir_all(root).expect("remove fixture");
}
