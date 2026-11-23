// SPDX-License-Identifier: MPL-2.0
//! Derived roles: what an entity without a kind looks like from its components (spec 101 § 3).
//!
//! One priority table of engine component short names, shared by the editor's outliner, the agent,
//! the lint and the project map. Rows 1-6 are the viewport's `icon_of` order. Components are matched
//! by their last path segment within the engine crates (`regia_*`, and the pre-rename `ferrite_*`
//! spellings old files carry); game components never match a role.

use regia_app::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, GetPath, ReflectComponent, TypeRegistry};
use serde::Serialize;

/// A derived role, in priority order (the first match is the entity's role).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Directional light.
    Sun,
    /// Spot light.
    Spot,
    /// Point light.
    Point,
    /// Render camera or camera rig.
    Camera,
    /// Placed decal.
    Decal,
    /// Audio emitter or listener.
    Audio,
    /// Character, player or AI controller, navigation agent.
    Character,
    /// Vehicle or hover controller.
    Vehicle,
    /// Sensor collider (trigger volume).
    Trigger,
    /// Rigid body.
    Body,
    /// Mesh.
    Mesh,
    /// Collider (not a sensor).
    Collider,
    /// Terrain.
    Terrain,
    /// Reflection probe.
    Probe,
    /// Spline.
    Spline,
    /// Region shape.
    Volume,
    /// UI root.
    Ui,
}

/// A family of roles: two families on one entity are two jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Lights and probes.
    Light,
    /// Cameras.
    Camera,
    /// Decals.
    Decal,
    /// Audio.
    Audio,
    /// Characters and vehicles.
    Actor,
    /// Trigger volumes.
    Trigger,
    /// Meshes, bodies and colliders: one physical object.
    Physical,
    /// Terrain.
    Terrain,
    /// Splines and region shapes.
    Shape,
    /// UI.
    Ui,
}

impl Role {
    /// Every role in priority order.
    pub const PRIORITY: [Role; 17] = [
        Role::Sun,
        Role::Spot,
        Role::Point,
        Role::Camera,
        Role::Decal,
        Role::Audio,
        Role::Character,
        Role::Vehicle,
        Role::Trigger,
        Role::Body,
        Role::Mesh,
        Role::Collider,
        Role::Terrain,
        Role::Probe,
        Role::Spline,
        Role::Volume,
        Role::Ui,
    ];

    /// The role's name (`sun`, `trigger`, ...), also its icon name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Role::Sun => "sun",
            Role::Spot => "spot",
            Role::Point => "point",
            Role::Camera => "camera",
            Role::Decal => "decal",
            Role::Audio => "audio",
            Role::Character => "character",
            Role::Vehicle => "vehicle",
            Role::Trigger => "trigger",
            Role::Body => "body",
            Role::Mesh => "mesh",
            Role::Collider => "collider",
            Role::Terrain => "terrain",
            Role::Probe => "probe",
            Role::Spline => "spline",
            Role::Volume => "volume",
            Role::Ui => "ui",
        }
    }

    /// The role's family.
    #[must_use]
    pub const fn family(self) -> Family {
        match self {
            Role::Sun | Role::Spot | Role::Point | Role::Probe => Family::Light,
            Role::Camera => Family::Camera,
            Role::Decal => Family::Decal,
            Role::Audio => Family::Audio,
            Role::Character | Role::Vehicle => Family::Actor,
            Role::Trigger => Family::Trigger,
            Role::Body | Role::Mesh | Role::Collider => Family::Physical,
            Role::Terrain => Family::Terrain,
            Role::Spline | Role::Volume => Family::Shape,
            Role::Ui => Family::Ui,
        }
    }
}

impl Family {
    /// The family's name (`light`, `physical`, ...).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Family::Light => "light",
            Family::Camera => "camera",
            Family::Decal => "decal",
            Family::Audio => "audio",
            Family::Actor => "actor",
            Family::Trigger => "trigger",
            Family::Physical => "physical",
            Family::Terrain => "terrain",
            Family::Shape => "shape",
            Family::Ui => "ui",
        }
    }
}

/// One component as the classifier sees it: its type path and, for a collider, whether it is a
/// sensor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentProbe<'a> {
    /// Full type path (`regia_physics::components::Collider`).
    pub type_path: &'a str,
    /// For a `Collider`: its `sensor` flag. Ignored for other components.
    pub sensor: bool,
}

impl<'a> ComponentProbe<'a> {
    /// A probe for a component that is not a sensor collider.
    #[must_use]
    pub const fn new(type_path: &'a str) -> Self {
        Self {
            type_path,
            sensor: false,
        }
    }
}

/// The result of classifying an entity's components.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Classification {
    /// The first matching role in priority order.
    pub role: Option<Role>,
    /// Every matching role, in priority order.
    pub roles: Vec<Role>,
    /// The distinct families of those roles, sorted.
    pub families: Vec<Family>,
}

impl Classification {
    /// Whether the components span two or more families (two jobs on one entity).
    #[must_use]
    pub fn is_multi_family(&self) -> bool {
        self.families.len() >= 2
    }
}

/// The crate segment of a type path (`regia_render` of `regia_render::light::PointLight`).
fn crate_of(type_path: &str) -> &str {
    type_path.split("::").next().unwrap_or(type_path)
}

/// The last path segment without generics (`PointLight`).
fn short_of(type_path: &str) -> &str {
    let base = type_path.split('<').next().unwrap_or(type_path);
    base.rsplit("::").next().unwrap_or(base)
}

/// Whether the type comes from an engine crate (`regia*`, `ferrite*`, `bevy_*`) or the standard
/// library and math crates. Components of any other crate are game components.
#[must_use]
pub fn is_engine_type(type_path: &str) -> bool {
    let krate = crate_of(type_path);
    krate == "regia"
        || krate == "ferrite"
        || krate.starts_with("regia_")
        || krate.starts_with("ferrite_")
        || krate.starts_with("bevy_")
        || matches!(krate, "std" | "core" | "alloc" | "glam")
}

/// The role a single component gives, if any.
#[must_use]
pub fn role_of(probe: ComponentProbe<'_>) -> Option<Role> {
    let krate = crate_of(probe.type_path);
    if !(krate.starts_with("regia_") || krate.starts_with("ferrite_")) {
        return None;
    }
    Some(match short_of(probe.type_path) {
        "DirectionalLight" => Role::Sun,
        "SpotLight" => Role::Spot,
        "PointLight" => Role::Point,
        "Camera" | "CameraRig" => Role::Camera,
        "StaticDecal" => Role::Decal,
        "AudioEmitter" | "AudioListener" => Role::Audio,
        "Character" | "CharacterBody" | "PlayerController" | "AiController" | "NetCharacter" | "NavAgent" => {
            Role::Character
        }
        "Vehicle" | "VehicleController" | "HoverController" => Role::Vehicle,
        "Collider" if probe.sensor => Role::Trigger,
        "RigidBody" => Role::Body,
        "MeshSource" | "MeshRenderer" | "SkinnedMesh" => Role::Mesh,
        "Collider" => Role::Collider,
        "Terrain" => Role::Terrain,
        "ReflectionProbe" => Role::Probe,
        "Spline" => Role::Spline,
        "Shape" => Role::Volume,
        "UiRoot" => Role::Ui,
        _ => return None,
    })
}

/// Classifies a set of components by the priority rule.
pub fn classify<'a>(components: impl IntoIterator<Item = ComponentProbe<'a>>) -> Classification {
    let mut roles: Vec<Role> = components.into_iter().filter_map(role_of).collect();
    roles.sort();
    roles.dedup();
    let mut families: Vec<Family> = roles.iter().map(|r| r.family()).collect();
    families.sort();
    families.dedup();
    Classification {
        role: roles.first().copied(),
        roles,
        families,
    }
}

/// Whether any of the components is a game component (outside the engine crates).
pub fn has_game_component<'a>(type_paths: impl IntoIterator<Item = &'a str>) -> bool {
    type_paths.into_iter().any(|p| !is_engine_type(p))
}

/// The component type paths of a live entity, with each collider's `sensor` flag read through
/// reflection. Sorted by type path. Takes the world's `AppTypeRegistry` read lock; a caller that
/// already holds it uses [`live_components_in`].
#[must_use]
pub fn live_components(world: &World, entity: Entity) -> Vec<(String, bool)> {
    match world.get_resource::<AppTypeRegistry>().cloned() {
        Some(registry) => live_components_in(world, Some(&registry.read()), entity),
        None => live_components_in(world, None, entity),
    }
}

/// [`live_components`] with a registry the caller already holds (no second lock).
#[must_use]
pub fn live_components_in(
    world: &World,
    registry: Option<&TypeRegistry>,
    entity: Entity,
) -> Vec<(String, bool)> {
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    let entity_ref = world.get_entity(entity).ok();
    let mut out: Vec<(String, bool)> = infos
        .filter_map(|(_, info)| {
            let registration = info.type_id().and_then(|t| registry.and_then(|r| r.get(t)));
            let path = match registration {
                Some(registration) => registration.type_info().type_path().to_string(),
                None => {
                    let name = info.name().to_string();
                    if name.starts_with('<') {
                        return None;
                    }
                    name
                }
            };
            let sensor = short_of(&path) == "Collider"
                && registration
                    .and_then(|r| r.data::<ReflectComponent>())
                    .zip(entity_ref)
                    .and_then(|(rc, e)| rc.reflect(e))
                    .and_then(|value| {
                        value
                            .reflect_path("sensor")
                            .ok()
                            .and_then(|v| v.try_downcast_ref::<bool>().copied())
                    })
                    .unwrap_or(false);
            Some((path, sensor))
        })
        .collect();
    out.sort();
    out
}

/// Classifies a live entity ([`live_components`] then [`classify`]).
#[must_use]
pub fn classify_entity(world: &World, entity: Entity) -> Classification {
    classify_components(&live_components(world, entity))
}

/// [`classify_entity`] with a registry the caller already holds (no second lock).
#[must_use]
pub fn classify_entity_in(world: &World, registry: Option<&TypeRegistry>, entity: Entity) -> Classification {
    classify_components(&live_components_in(world, registry, entity))
}

fn classify_components(components: &[(String, bool)]) -> Classification {
    classify(components.iter().map(|(path, sensor)| ComponentProbe {
        type_path: path,
        sensor: *sensor,
    }))
}

/// Whether a raw RON collider value has `sensor: true` (files: the lint and the scanner).
#[must_use]
pub fn ron_says_sensor(raw: &str) -> bool {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("sensor:true")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIGHT: &str = "regia_render::light::PointLight";
    const SUN: &str = "regia_render::light::DirectionalLight";
    const COLLIDER: &str = "regia_physics::components::Collider";
    const MESH: &str = "regia_render::runtime::MeshSource";
    const BODY: &str = "regia_physics::components::RigidBody";

    #[test]
    fn the_first_six_rows_are_the_viewport_icon_order() {
        let six: Vec<&str> = Role::PRIORITY[..6].iter().map(|r| r.name()).collect();
        assert_eq!(six, ["sun", "spot", "point", "camera", "decal", "audio"]);
    }

    #[test]
    fn priority_picks_the_first_matching_role() {
        let c = classify([ComponentProbe::new(LIGHT), ComponentProbe::new(SUN)]);
        assert_eq!(c.role, Some(Role::Sun));
        assert_eq!(c.roles, [Role::Sun, Role::Point]);
        assert_eq!(c.families, [Family::Light]);
        assert!(!c.is_multi_family());
    }

    #[test]
    fn a_sensor_collider_is_a_trigger_and_a_plain_one_a_collider() {
        let trigger = classify([ComponentProbe {
            type_path: COLLIDER,
            sensor: true,
        }]);
        assert_eq!(trigger.role, Some(Role::Trigger));
        assert_eq!(
            classify([ComponentProbe::new(COLLIDER)]).role,
            Some(Role::Collider)
        );
    }

    #[test]
    fn mesh_body_and_collider_are_one_physical_job() {
        let c = classify([
            ComponentProbe::new(MESH),
            ComponentProbe::new(COLLIDER),
            ComponentProbe::new(BODY),
        ]);
        assert_eq!(c.role, Some(Role::Body));
        assert_eq!(c.families, [Family::Physical]);
        assert!(!c.is_multi_family());
    }

    #[test]
    fn a_light_with_a_trigger_spans_two_families() {
        let c = classify([
            ComponentProbe::new(LIGHT),
            ComponentProbe {
                type_path: COLLIDER,
                sensor: true,
            },
        ]);
        assert_eq!(c.role, Some(Role::Point));
        assert_eq!(c.families, [Family::Light, Family::Trigger]);
        assert!(c.is_multi_family());
    }

    #[test]
    fn game_and_legacy_crates() {
        assert_eq!(role_of(ComponentProbe::new("my_game::PointLight")), None);
        assert_eq!(
            role_of(ComponentProbe::new("ferrite_render::light::PointLight")),
            Some(Role::Point)
        );
        assert!(is_engine_type("regia_scene::transform::Transform"));
        assert!(is_engine_type("bevy_ecs::name::Name"));
        assert!(!is_engine_type("my_game::Health"));
        assert!(has_game_component([
            "regia_scene::transform::Transform",
            "my_game::Health"
        ]));
        assert!(!has_game_component(["regia_scene::transform::Transform"]));
    }

    /// Every row of the spec 101 § 3 table: each listed component alone gives that row's role and
    /// family, and the rows are in table order.
    #[test]
    fn every_row_of_the_role_table_maps_its_components() {
        let table: [(Role, &[&str], Family); 17] = [
            (Role::Sun, &["DirectionalLight"], Family::Light),
            (Role::Spot, &["SpotLight"], Family::Light),
            (Role::Point, &["PointLight"], Family::Light),
            (Role::Camera, &["Camera", "CameraRig"], Family::Camera),
            (Role::Decal, &["StaticDecal"], Family::Decal),
            (Role::Audio, &["AudioEmitter", "AudioListener"], Family::Audio),
            (
                Role::Character,
                &[
                    "Character",
                    "CharacterBody",
                    "PlayerController",
                    "AiController",
                    "NetCharacter",
                    "NavAgent",
                ],
                Family::Actor,
            ),
            (
                Role::Vehicle,
                &["Vehicle", "VehicleController", "HoverController"],
                Family::Actor,
            ),
            (Role::Trigger, &[], Family::Trigger),
            (Role::Body, &["RigidBody"], Family::Physical),
            (
                Role::Mesh,
                &["MeshSource", "MeshRenderer", "SkinnedMesh"],
                Family::Physical,
            ),
            (Role::Collider, &["Collider"], Family::Physical),
            (Role::Terrain, &["Terrain"], Family::Terrain),
            (Role::Probe, &["ReflectionProbe"], Family::Light),
            (Role::Spline, &["Spline"], Family::Shape),
            (Role::Volume, &["Shape"], Family::Shape),
            (Role::Ui, &["UiRoot"], Family::Ui),
        ];
        let order: Vec<Role> = table.iter().map(|(role, _, _)| *role).collect();
        assert_eq!(order, Role::PRIORITY, "rows in table order");
        for (role, components, family) in table {
            assert_eq!(role.family(), family, "{}", role.name());
            for short in components {
                for krate in ["regia_somewhere", "ferrite_somewhere"] {
                    let path = format!("{krate}::module::{short}");
                    assert_eq!(role_of(ComponentProbe::new(&path)), Some(role), "{path}");
                }
                let game = format!("my_game::{short}");
                assert_eq!(role_of(ComponentProbe::new(&game)), None, "{game}");
            }
        }
        // Row 9: a collider is a trigger only with `sensor: true`.
        let sensor = ComponentProbe {
            type_path: COLLIDER,
            sensor: true,
        };
        assert_eq!(role_of(sensor), Some(Role::Trigger));
        // The sensor flag means nothing on other components.
        let flagged_mesh = ComponentProbe {
            type_path: MESH,
            sensor: true,
        };
        assert_eq!(role_of(flagged_mesh), Some(Role::Mesh));
        // Generic arguments do not hide the short name.
        assert_eq!(
            role_of(ComponentProbe::new("regia_scene::spline::Spline<f32>")),
            Some(Role::Spline)
        );
    }

    #[test]
    fn no_role_for_plain_transforms() {
        let c = classify([ComponentProbe::new("regia_scene::transform::Transform")]);
        assert_eq!(c, Classification::default());
    }

    #[test]
    fn sensor_in_raw_ron() {
        assert!(ron_says_sensor("(shape: Ball(0.5), sensor: true, friction: 0.5)"));
        assert!(ron_says_sensor("(sensor :\n true)"));
        assert!(!ron_says_sensor("(sensor: false)"));
    }
}
