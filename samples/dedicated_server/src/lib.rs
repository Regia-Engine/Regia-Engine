// SPDX-License-Identifier: MIT-0
//! The smallest game with a client build and a dedicated-server build (`docs/specs/76-dedicated-server-package.md`).
//!
//! The same plugin runs in both. `cargo run -p dedicated_server` opens a window;
//! `regia server --project samples/dedicated_server` builds it with the `server` feature, where
//! `regia::run` hosts it headless, and `regia package --server --platform linux` ships that build.
//! Anything that names a render or audio type goes behind `#[cfg(not(feature = "server"))]`.

use regia::prelude::*;
use regia_scene::kind::KindAppExt as _;

/// Where a player spawns: a code entity kind (spec 101). Zero-size; its contract is a `Transform`.
/// `assets/kinds/spawn_point.kind.ron` binds the same kind to this marker for tools that do not run
/// the game (`regia preflight`, the cook).
#[derive(Component, Reflect, Default, Debug)]
#[reflect(Component, Default)]
#[require(regia_scene::Transform)]
pub struct SpawnPoint;

regia_scene::entity_kind!(
    SpawnPoint,
    "SpawnPoint",
    icon = "spawn",
    color = [0x3c, 0xb4, 0x4b],
    description = "Where a player spawns",
    category = "Gameplay"
);

/// The fixed steps the simulation has run: gameplay state both builds have.
#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub struct Ticks(pub u64);

fn count_ticks(mut ticks: ResMut<Ticks>) {
    ticks.0 += 1;
}

/// The game: simulation for both builds, presentation for the client.
pub struct DedicatedServerGame;

impl Plugin for DedicatedServerGame {
    fn build(&self, app: &mut App) {
        app.init_resource::<Ticks>().add_systems(FixedUpdate, count_ticks);
        app.register_kind::<SpawnPoint>();
        #[cfg(not(feature = "server"))]
        app.insert_resource(ClearColor([0.05, 0.08, 0.1, 1.0]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simulation_counts_fixed_steps() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, DedicatedServerGame));
        regia::app::enable_fixed_step(&mut app, 60.0);
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().resource::<Ticks>().0, 3);
    }

    #[test]
    fn the_spawn_point_kind_matches_its_declaration() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, DedicatedServerGame));
        let text = include_str!("../assets/kinds/spawn_point.kind.ron");
        let file = regia_scene::kind::file::KindFile::from_ron_str(text).expect("valid .kind.ron");
        let info = file.to_info("assets/kinds/spawn_point.kind.ron").expect("info");
        let mut registry = app.world().resource::<regia_scene::kind::KindRegistry>().clone();
        registry.register_data(info);
        assert!(
            registry.notes().is_empty(),
            "code and data agree: {:?}",
            registry.notes()
        );
        let code = registry.get("SpawnPoint").expect("registered");
        assert_eq!(code.source.marker(), file.marker.as_deref());
        assert_eq!(std::mem::size_of::<SpawnPoint>(), 0);
    }
}
