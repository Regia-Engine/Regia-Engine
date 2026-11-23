// SPDX-License-Identifier: MPL-2.0
//! Read-only level queries for the baked-GI export (`regia bake-gi --from-scene`, ADR-310):
//! spawn a level into a bare world (no renderer, no window, no app) and list its entities with
//! their world transforms and [`GiMobility`], sorted by guid.
//!
//! The spawn and the transform propagation are the runtime's own ([`spawn_scene`],
//! [`propagate_transforms`]), so the world matrices are the ones a running game computes. Prefab
//! instances whose template is not loaded stay pending (their subtree is not spawned); the
//! warnings say which.

use regia_core::id::Guid;
use regia_core::math::Mat4;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};

use crate::components::{
    EntityGuid, GiMobility, InheritedVisibility, Layers, MapLayer, Name, Static, Tags, ViewVisibility,
    Visibility, register_primitives,
};
use crate::format::SceneFile;
use crate::prefab::{PendingPrefab, PrefabInstance, PrefabLibrary, PrefabLink};
use crate::propagate::propagate_transforms;
use crate::spawn::{SceneResourceHooks, SceneSpawnError, SpawnOptions, spawn_scene};
use crate::transform::{GlobalTransform, PreviousGlobalTransform, Transform};

/// A level spawned headless by [`spawn_headless`].
#[derive(Debug)]
pub struct HeadlessScene {
    /// The world holding the level's entities, with `GlobalTransform` propagated.
    pub world: World,
    /// Entities spawned, prefab-internal ones included.
    pub entities: usize,
    /// Non-fatal spawn problems (unregistered component types, pending prefabs, lost parents).
    pub warnings: Vec<String>,
}

/// One entity of a level as the bake export sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct GiEntity {
    /// The entity in [`HeadlessScene::world`] (or the live world it was read from).
    pub entity: Entity,
    /// Stable identity (the scene file's `guid`).
    pub guid: Guid,
    /// The entity's name, if it has one.
    pub name: Option<String>,
    /// Its [`GiMobility`]; `Dynamic` when the entity has none (the component's default).
    pub mobility: GiMobility,
    /// World transform (object → world), column-major 4 × 4.
    pub world: [f32; 16],
    /// Its [`Tags`], in order.
    pub tags: Vec<String>,
}

impl GiEntity {
    /// `name (guid)`, or the guid alone: how errors and reports name the entity.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.guid),
            None => self.guid.to_string(),
        }
    }

    /// Whether the entity carries the tag `tag`.
    #[must_use]
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// Registers the scene crate's own component types in `registry` (what [`crate::ScenePlugin`]
/// registers for entities), so a bare world can decode them.
fn register_scene_types(registry: &mut TypeRegistry) {
    register_primitives(registry);
    registry.register::<Transform>();
    registry.register::<GlobalTransform>();
    registry.register::<PreviousGlobalTransform>();
    registry.register::<Name>();
    registry.register::<EntityGuid>();
    registry.register::<Tags>();
    registry.register::<Layers>();
    registry.register::<MapLayer>();
    registry.register::<Static>();
    registry.register::<GiMobility>();
    registry.register::<crate::components::LightmapSurface>();
    registry.register::<Visibility>();
    registry.register::<InheritedVisibility>();
    registry.register::<ViewVisibility>();
    registry.register::<PrefabInstance>();
    registry.register::<PrefabLink>();
    registry.register::<PendingPrefab>();
    crate::level::register_types(registry);
}

/// Spawns `file` into a new bare world and propagates its transforms. `register` adds the
/// component types of other crates the caller reads (a mesh reference, a light); every other
/// unregistered type is skipped with a warning, as at runtime.
///
/// # Errors
///
/// The file's structure is invalid or a component value does not decode.
pub fn spawn_headless(
    file: &SceneFile,
    register: impl FnOnce(&mut TypeRegistry),
) -> Result<HeadlessScene, SceneSpawnError> {
    let _span = tracing::info_span!("scene_spawn_headless", entities = file.entities.len()).entered();
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        register_scene_types(&mut registry);
        register(&mut registry);
    }
    world.insert_resource(PrefabLibrary::default());
    world.insert_resource(SceneResourceHooks::with_engine_hooks());
    let spawned = spawn_scene(&mut world, file, &SpawnOptions::default())?;
    propagate_transforms(&mut world);
    Ok(HeadlessScene {
        world,
        entities: spawned.entities,
        warnings: spawned.warnings,
    })
}

/// Every entity of `world` that has an [`EntityGuid`], sorted by guid, with its world transform
/// (identity without a `GlobalTransform`), mobility and tags. Works on a headless world from
/// [`spawn_headless`] and on a running game's world alike; the world is not changed.
#[must_use]
pub fn gi_entities(world: &mut World) -> Vec<GiEntity> {
    let mut query = world.query::<(
        Entity,
        &EntityGuid,
        Option<&Name>,
        Option<&GiMobility>,
        Option<&GlobalTransform>,
        Option<&Tags>,
    )>();
    let mut out: Vec<GiEntity> = query
        .iter(world)
        .map(|(entity, guid, name, mobility, global, tags)| GiEntity {
            entity,
            guid: guid.0,
            name: name.map(|n| n.as_str().to_string()),
            mobility: mobility.copied().unwrap_or_default(),
            world: global.map_or(Mat4::IDENTITY, |g| Mat4::from(g.0)).to_cols_array(),
            tags: tags
                .map(|t| t.0.iter().map(|n| n.as_str().to_string()).collect())
                .unwrap_or_default(),
        })
        .collect();
    out.sort_by_key(|e| e.guid);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parented static entity gets its parent's transform composed in; a missing
    /// `GiMobility` reads as `Dynamic`; the list is sorted by guid.
    #[test]
    fn a_headless_level_lists_world_transforms_and_mobility() {
        let parent = Guid::from_u128(0x20);
        let child = Guid::from_u128(0x10);
        let text = format!(
            r#"(
    format_version: 10,
    entities: [
        (guid: "{parent}", name: "Group", components: {{
            "regia_scene::transform::Transform": (translation: (10.0, 0.0, 0.0)),
        }}),
        (guid: "{child}", name: "Wall", parent: Some("{parent}"), components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 2.0, 0.0)),
            "regia_scene::components::GiMobility": Static,
            "regia_scene::components::Tags": (["setpiece"]),
            "my_game::Door": (open: true),
        }}),
    ],
)"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let mut scene = spawn_headless(&file, |_| {}).expect("spawn");
        assert_eq!(scene.entities, 2);
        assert!(
            scene.warnings.iter().any(|w| w.contains("my_game::Door")),
            "{:?}",
            scene.warnings
        );
        let list = gi_entities(&mut scene.world);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].guid, child);
        assert_eq!(list[0].mobility, GiMobility::Static);
        assert_eq!(&list[0].world[12..15], &[10.0, 2.0, 0.0]);
        assert!(list[0].has_tag("setpiece"));
        assert_eq!(list[0].label(), format!("Wall ({child})"));
        assert_eq!(list[1].mobility, GiMobility::Dynamic);
    }
}
