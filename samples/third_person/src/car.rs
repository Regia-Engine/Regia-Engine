// SPDX-License-Identifier: MIT-0
//! A drivable car (ADR-2134): a `regia_physics::vehicle` ray-cast car parked west of the start,
//! entered and left with `E`. While a player drives, `W`/`S` are throttle and reverse, `A`/`D`
//! steer, `Space` brakes, and the player's camera runs the `vehicle_chase` camera-driver asset
//! (`assets/camera/vehicle.camera.ron`, spec 35). Leaving the car removes the driver, so the
//! on-foot boom rig takes over again. Gameplay tags mark the car (`Vehicle.Car`), the player
//! (`Role.Player`), the coins (`Pickup.Coin`) and a driving player (`State.Driving`).
//!
//! Agent surface: `physics.vehicles` / `physics.vehicle_input` / `physics.vehicle_reset` on the
//! car, `camera.inspect` on the player camera while driving, `tags.get` / `tags.query`, and the
//! reflected [`Driving`] component (`book/agents/gameplay.md` § The third-person car).

use regia::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_framework::camera::director::CameraHooks;
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::player::{Player, PlayerController, PlayerManager};
use regia_framework::tags::{Tag, TagContainer, TagPlugin, TagRegistry, TagTable};
use regia_physics::vehicle::{VehicleConfig, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::vehicle_asset::VehicleAsset;
use regia_render::MeshRenderer;
use regia_scene::Transform;

/// The project tag table (`assets/game.tags.ron`).
pub const TAGS_TEXT: &str = include_str!("../assets/game.tags.ron");
/// The chase camera asset's text.
pub const CAR_CAMERA_TEXT: &str = include_str!("../assets/camera/vehicle.camera.ron");
/// The car's tuning (`assets/car.vehicle.ron`, `docs/formats/vehicle.md`).
pub const CAR_VEHICLE_TEXT: &str = include_str!("../assets/car.vehicle.ron");
/// The chase camera asset's name (what `CameraDriver::asset` names).
pub const CAR_CAMERA: &str = "vehicle_chase";
/// Where the car is parked (chassis centre).
pub const CAR_SPAWN: Vec3 = Vec3::new(-12.0, 1.0, -10.0);
/// A player within this distance of the chassis centre can get in (m).
pub const ENTER_RADIUS: f32 = 3.5;
/// How far to the side of the car a player gets out (m).
const EXIT_OFFSET: f32 = 2.6;

/// Marker on the car's chassis entity.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct Car;

/// On a character while it drives: the car it drives.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Driving {
    /// The car's chassis entity.
    pub car: Entity,
    /// The last throttle, brake and steer the keys wrote. The keys write the car's input only when
    /// they change, so an agent's `physics.vehicle_input` holds until the player touches them.
    pub keys: [f32; 3],
}

/// Installs the car: the vehicle step, tags, the chase camera asset.
pub struct CarPlugin;

impl Plugin for CarPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(VehiclePlugin)
            .register_type::<Car>()
            .register_type::<Driving>();
        if !app.has_plugin(std::any::type_name::<TagPlugin>()) {
            app.add_plugins(TagPlugin);
        }
        match TagTable::from_ron(TAGS_TEXT)
            .map_err(|e| e.to_string())
            .and_then(|table| TagRegistry::from_table(&table).map_err(|e| e.to_string()))
        {
            Ok(registry) => {
                app.insert_resource(registry);
            }
            Err(err) => tracing::error!(target: "third_person", "game.tags.ron: {err}"),
        }
        let world = app.world_mut();
        if world.contains_resource::<CameraAssets>() && world.contains_resource::<CameraHooks>() {
            world.resource_scope(|world, mut assets: Mut<CameraAssets>| {
                let hooks = world.resource::<CameraHooks>();
                if let Err(err) = assets.load_ron(
                    CAR_CAMERA_TEXT,
                    Some("assets/camera/vehicle.camera.ron".into()),
                    hooks,
                ) {
                    tracing::error!(target: "third_person", "vehicle.camera.ron: {err}");
                }
            });
        }
    }
}

fn tag(name: &str) -> Option<Tag> {
    Tag::new(name).ok()
}

/// Adds (or removes) `name` on `entity`'s tag container, creating the container when needed.
fn set_tag(world: &mut World, entity: Entity, name: &str, held: bool) {
    let Some(tag) = tag(name) else {
        return;
    };
    let Ok(mut entity) = world.get_entity_mut(entity) else {
        return;
    };
    if let Some(mut container) = entity.get_mut::<TagContainer>() {
        if held {
            container.add(tag);
        } else {
            container.remove(tag);
        }
    } else if held {
        let mut container = TagContainer::default();
        container.add(tag);
        entity.insert(container);
    }
}

/// Tags `entity` as a player character (`Role.Player`).
pub fn tag_player(world: &mut World, entity: Entity) {
    set_tag(world, entity, "Role.Player", true);
}

/// Tags `entity` as a coin (`Pickup.Coin`).
pub fn tag_coin(world: &mut World, entity: Entity) {
    set_tag(world, entity, "Pickup.Coin", true);
}

/// `Startup`: parks the car. Logs and returns `None` without a physics world.
pub fn spawn_car(world: &mut World) -> Option<Entity> {
    let config = car_config();
    let half = config.chassis_half_extents;
    let car = match spawn_vehicle(world, config, CAR_SPAWN, Quat::IDENTITY) {
        Ok(car) => car,
        Err(err) => {
            tracing::warn!(target: "third_person", "the car was not spawned: {err}");
            return None;
        }
    };
    world.entity_mut(car).insert((
        Car,
        regia_scene::components::Name::new("car"),
        Name::new("car"),
        MeshRenderer::new(
            crate::level::box_mesh(half),
            crate::level::solid_material([0.78, 0.14, 0.10, 1.0]),
        ),
    ));
    set_tag(world, car, "Vehicle.Car", true);
    Some(car)
}

/// The car's configuration from `assets/car.vehicle.ron`; the default car (and a warning) when
/// the asset does not load.
pub fn car_config() -> VehicleConfig {
    match VehicleAsset::from_ron(CAR_VEHICLE_TEXT) {
        Ok(asset) => asset.into_config(),
        Err(err) => {
            tracing::warn!(target: "third_person", "car.vehicle.ron did not load ({}): {err}", err.code());
            VehicleConfig::default()
        }
    }
}

/// The character player `index` possesses.
fn possessed(world: &World, index: u8) -> Option<Entity> {
    let player = world
        .get_resource::<PlayerManager>()?
        .iter()
        .find(|(i, _)| *i == index)?
        .1;
    let controller = world.get::<Player>(player)?.controller;
    let character = world.get::<PlayerController>(controller)?.possessed;
    (character != Entity::PLACEHOLDER).then_some(character)
}

/// Player `index`'s camera.
fn player_camera(world: &mut World, index: u8) -> Option<Entity> {
    let mut query = world.query::<(Entity, &crate::camera::PlayerCamera)>();
    query
        .iter(world)
        .find(|(_, camera)| camera.player == index)
        .map(|(entity, _)| entity)
}

/// The nearest car within [`ENTER_RADIUS`] of `position`.
fn car_near(world: &mut World, position: Vec3) -> Option<Entity> {
    let mut query = world.query_filtered::<(Entity, &Transform), With<Car>>();
    let mut best: Option<(Entity, f32)> = None;
    for (entity, transform) in query.iter(world) {
        let dist = (transform.translation - position).length();
        if dist <= ENTER_RADIUS
            && best.is_none_or(|(e, d)| dist < d || (dist == d && entity.index() < e.index()))
        {
            best = Some((entity, dist));
        }
    }
    best.map(|(entity, _)| entity)
}

/// Gets player `index` into the nearest car in reach, or out of the car it drives. Returns whether
/// the player drives afterwards.
pub fn toggle(world: &mut World, index: u8) -> bool {
    let Some(character) = possessed(world, index) else {
        return false;
    };
    if let Some(driving) = world.get::<Driving>(character).copied() {
        exit(world, index, character, driving.car);
        return false;
    }
    let Some(position) = world.get::<Transform>(character).map(|t| t.translation) else {
        return false;
    };
    let Some(car) = car_near(world, position) else {
        return false;
    };
    world
        .entity_mut(character)
        .insert(Driving { car, keys: [0.0; 3] });
    set_tag(world, character, "State.Driving", true);
    if let Some(mut input) = world.get_mut::<regia_framework::character::CharacterInput>(character) {
        input.move_dir = Vec3::ZERO;
        input.jump = false;
        input.sprint = false;
    }
    if let Some(camera) = player_camera(world, index) {
        world
            .entity_mut(camera)
            .insert(CameraDriver::new(CAR_CAMERA).with_target(car));
    }
    true
}

fn exit(world: &mut World, index: u8, character: Entity, car: Entity) {
    world.entity_mut(character).remove::<Driving>();
    set_tag(world, character, "State.Driving", false);
    if let Some(mut input) = world.get_mut::<VehicleInput>(car) {
        *input = VehicleInput {
            throttle: 0.0,
            brake: 1.0,
            steer: 0.0,
        };
    }
    // Out on the driver's side (the car's local −X), a little above the ground.
    if let Some(car_transform) = world.get::<Transform>(car).copied() {
        let left = car_transform.rotation * Vec3::NEG_X;
        let out =
            car_transform.translation + Vec3::new(left.x, 0.0, left.z).normalize_or_zero() * EXIT_OFFSET;
        if let Some(mut transform) = world.get_mut::<Transform>(character) {
            transform.translation = Vec3::new(out.x, car_transform.translation.y + 0.6, out.z);
        }
    }
    if let Some(camera) = player_camera(world, index) {
        world.entity_mut(camera).remove::<CameraDriver>();
    }
}

/// Per frame, from `input::drive_input`: when `character` drives, writes the car's input from the
/// move axes and brake key (when they changed since the last frame), feeds the chase camera's
/// `look` slot, and returns `true` (the character itself stands still).
pub fn drive(world: &mut World, index: u8, character: Entity, axes: Vec2, brake: bool, look: Vec2) -> bool {
    let Some(driving) = world.get::<Driving>(character).copied() else {
        return false;
    };
    let keys = [
        axes.y.clamp(-1.0, 1.0),
        if brake { 1.0 } else { 0.0 },
        axes.x.clamp(-1.0, 1.0),
    ];
    if keys != driving.keys {
        if let Some(mut input) = world.get_mut::<VehicleInput>(driving.car) {
            *input = VehicleInput {
                throttle: keys[0],
                brake: keys[1],
                steer: keys[2],
            }
            .sanitized();
        }
        if let Some(mut state) = world.get_mut::<Driving>(character) {
            state.keys = keys;
        }
    }
    if let Some(mut char_input) = world.get_mut::<regia_framework::character::CharacterInput>(character) {
        char_input.move_dir = Vec3::ZERO;
        char_input.jump = false;
        char_input.sprint = false;
    }
    if let Some(camera) = player_camera(world, index)
        && let Some(mut driver) = world.get_mut::<CameraDriver>(camera)
        && let Some(core) = driver.core_mut()
    {
        // The slot integrates value · speed · dt: a mouse delta in radians becomes a rate.
        let value = [(look.x * 30.0).clamp(-1.0, 1.0), (look.y * 30.0).clamp(-1.0, 1.0)];
        let _ = core.set_input("look", value);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_table_and_the_chase_camera_asset_load() {
        let table = TagTable::from_ron(TAGS_TEXT).expect("tag table parses");
        let registry = TagRegistry::from_table(&table).expect("tag table is valid");
        for name in ["Role.Player", "Pickup.Coin", "Vehicle.Car", "State.Driving"] {
            assert!(registry.contains(Tag::new(name).expect("valid")), "{name}");
        }
        let mut assets = CameraAssets::default();
        let status = assets
            .load_ron(CAR_CAMERA_TEXT, None, &CameraHooks::default())
            .expect("parses");
        assert_eq!(
            status,
            regia_framework::camera::asset::CameraBuildStatus::Ok,
            "{:?}",
            assets.issues(CAR_CAMERA)
        );
    }

    #[test]
    fn the_car_asset_loads_and_is_the_default_car() {
        let asset = VehicleAsset::from_ron(CAR_VEHICLE_TEXT).expect("car.vehicle.ron loads");
        assert_eq!(asset.name, "third-person car");
        assert_eq!(car_config(), VehicleConfig::default());
    }
}
