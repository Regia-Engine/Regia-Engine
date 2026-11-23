// SPDX-License-Identifier: MPL-2.0
//! *Create placed level* (`docs/specs/37-per-entity-levels-layers-instances.md` § 6.5): the selected
//! entities move into a new level asset with their guids, re-expressed relative to a pivot, and one
//! instance takes their place at the pivot, so nothing moves in the world.
//!
//! The move is one undo step. The new level's files stay on disk when it is undone: another level
//! may already place them, and a redo needs them.

use std::path::{Component, Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect};
use regia_ecs::serialize::ReflectCodec;
use regia_framework::level_assets::load_level_source;
use regia_scene::Transform;
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::level::{Level, LevelId, LevelLibrary};
use regia_scene::level_dir::{LEVEL_DIR_SUFFIX, LEVEL_FILE_SUFFIX, level_paths, write_level_dir};
use regia_scene::placed::{self, PlacedError, PlacedLevel, PlacedMember};
use regia_scene::prelude::EntityGuid;
use serde_json::{Value, json};

use crate::command::{
    CommandGroup, DespawnEntities, EditorCommand, SpawnEntities, UndoStack, subtree_snapshot,
};
use crate::hierarchy_ops::{top_level, world_affine};
use crate::level_io::{self, OpenLevel};
use crate::selection::{SelectMode, Selection};
use crate::viewport_agent::Handler;

const TRANSFORM_PATH: &str = "regia_scene::transform::Transform";

/// Where the instance stands. The pivot only translates: the moved entities keep their rotation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacedPivot {
    /// The center of the selection's bounds at their lowest point.
    #[default]
    LowestCenter,
    /// The center of the selection's bounds.
    Center,
    /// The position of a selected entity.
    Member(Guid),
}

/// What [`create_placed_level`] made.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedCreated {
    /// The instance that took the selection's place.
    pub instance: Entity,
    /// The new level.
    pub level: LevelId,
    /// Its source.
    pub path: PathBuf,
    /// Entities moved into it, descendants included.
    pub moved: usize,
    /// The instance's position.
    pub pivot: Vec3,
}

fn refused(code: &str, message: impl Into<String>) -> RpcError {
    crate::layers_panel::reason(code, INVALID_PARAMS, message)
}

/// `path` under the project's `assets/`, in either form, and new. Relative, so a new level is
/// always one the project can find by id.
fn new_level_path(world: &World, path: &str) -> Result<PathBuf, RpcError> {
    let relative = Path::new(path.strip_prefix("assets://").unwrap_or(path));
    let plain = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if !plain || !(path.ends_with(LEVEL_FILE_SUFFIX) || path.ends_with(LEVEL_DIR_SUFFIX)) {
        return Err(refused(
            "level_path_invalid",
            format!(
                "`{path}` must be relative to assets/ and end in {LEVEL_FILE_SUFFIX} or {LEVEL_DIR_SUFFIX}"
            ),
        ));
    }
    let assets = crate::layers_panel::assets_dir(world).ok_or_else(|| {
        refused(
            "level_path_invalid",
            "no project or saved level gives an assets/ directory",
        )
    })?;
    let full = assets.join(relative);
    let (file, dir) = level_paths(&full);
    if file.exists() || dir.exists() {
        return Err(refused(
            "level_exists",
            format!("{} already exists", full.display()),
        ));
    }
    Ok(full)
}

/// `entities` and their descendants.
fn subtrees(world: &World, entities: &[Entity]) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut stack = entities.to_vec();
    while let Some(entity) = stack.pop() {
        if !out.contains(&entity) {
            out.push(entity);
            stack.extend(world.get::<Children>(entity).into_iter().flat_map(|c| c.iter()));
        }
    }
    out
}

fn pivot_position(world: &World, roots: &[Entity], pivot: PlacedPivot) -> Result<Vec3, RpcError> {
    let members = subtrees(world, roots);
    if let PlacedPivot::Member(guid) = pivot {
        return members
            .iter()
            .find(|entity| world.get::<EntityGuid>(**entity).is_some_and(|g| g.0 == guid))
            .map(|entity| Vec3::from(world_affine(world, *entity).translation))
            .ok_or_else(|| refused("pivot_not_selected", format!("{guid} is not in the selection")));
    }
    let mut bounds: Option<(Vec3, Vec3)> = None;
    for entity in members {
        let position = Vec3::from(world_affine(world, entity).translation);
        let (min, max) = crate::overlays::world_aabb(world, entity).unwrap_or((position, position));
        bounds = Some(bounds.map_or((min, max), |(a, b)| (a.min(min), b.max(max))));
    }
    let (min, max) = bounds.unwrap_or_default();
    let center = (min + max) * 0.5;
    Ok(match pivot {
        PlacedPivot::LowestCenter => Vec3::new(center.x, min.y, center.z),
        _ => center,
    })
}

/// Writes `transform` into a record the way a scene file stores it.
fn store_transform(world: &World, record: &mut SceneEntity, transform: Transform) -> Result<(), RpcError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let text = ReflectCodec::new(&registry)
        .to_ron_typed(transform.as_partial_reflect())
        .map_err(|e| RpcError::internal(e.to_string()))?;
    let raw =
        ron::value::RawValue::from_ron(text.trim_end()).map_err(|e| RpcError::internal(e.to_string()))?;
    record
        .components
        .insert(TRANSFORM_PATH.to_string(), raw.to_owned());
    Ok(())
}

/// The selection as a level: the roots, re-rooted, relative to `pivot`.
fn level_scene(world: &mut World, roots: &[Entity], pivot: Vec3) -> Result<SceneFile, RpcError> {
    let bits: Vec<u64> = roots.iter().map(|entity| entity.to_bits()).collect();
    let mut scene = subtree_snapshot(world, &bits);
    for root in roots {
        let Some(guid) = world.get::<EntityGuid>(*root).map(|g| g.0) else {
            continue;
        };
        let mut local = Transform::from_affine(world_affine(world, *root));
        local.translation -= pivot;
        if let Some(record) = scene.entities.iter_mut().find(|record| record.guid == guid) {
            store_transform(world, record, local)?;
            record.parent = None;
        }
    }
    Ok(scene)
}

fn write_level(path: &Path, id: LevelId, scene: &SceneFile) -> Result<(), RpcError> {
    let internal = |e: String| RpcError::internal(e);
    if level_io::is_level_dir(path) {
        write_level_dir(path, scene).map_err(|e| internal(e.to_string()))?;
    } else {
        let text = scene.to_ron_string().map_err(|e| internal(e.to_string()))?;
        level_io::write_atomic(path, text.as_bytes()).map_err(internal)?;
    }
    let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(id.0))
        .to_ron_string()
        .map_err(|e| internal(e.to_string()))?;
    let meta = regia_asset::meta_path_for(path);
    level_io::write_atomic(&meta, sidecar.as_bytes()).map_err(internal)
}

/// Moves the selection into a new level at `path` (relative to `assets/`; the suffix chooses the
/// form) and puts an instance of it in their place, as one undo step.
///
/// # Errors
/// `selection_empty`, `placed_member` (a member of another instance is selected),
/// `pivot_not_selected`, `level_path_invalid`, `level_exists`, `placed_cycle` and `placed_limit`
/// change nothing; a failed write leaves the selection where it is.
pub fn create_placed_level(
    world: &mut World,
    path: &str,
    pivot: PlacedPivot,
) -> Result<PlacedCreated, RpcError> {
    let selected: Vec<Entity> = world
        .get_resource::<Selection>()
        .map(|selection| selection.entities().to_vec())
        .unwrap_or_default();
    let roots: Vec<Entity> = top_level(world, &selected)
        .into_iter()
        .filter(|entity| !crate::visibility::is_hidden(world, *entity))
        .collect();
    if roots.is_empty() {
        return Err(refused(
            "selection_empty",
            "select the entities to move into the level",
        ));
    }
    if roots
        .iter()
        .any(|entity| world.get::<PlacedMember>(*entity).is_some())
    {
        return Err(refused(
            "placed_member",
            "a member of a placed level moves with its instance: select the instance",
        ));
    }
    let full = new_level_path(world, path)?;
    let position = pivot_position(world, &roots, pivot)?;
    let scene = level_scene(world, &roots, position)?;
    let moved = scene.entities.len();

    // The selection may hold an instance whose level places this host.
    let level = LevelId(Guid::new_random());
    let mut library = world.get_resource::<LevelLibrary>().cloned().unwrap_or_default();
    library.insert(Level::from_scene_file(level, scene.clone()));
    let host = world.get_resource::<OpenLevel>().map(|open| open.id);
    placed::check_placement(&library, level, &host.into_iter().collect::<Vec<_>>())
        .map_err(|e| refused(e.reason(), e.to_string()))?;

    write_level(&full, level, &scene)?;
    load_level_source(world, &full).map_err(|e| refused(e.reason(), e.to_string()))?;

    let name = full
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split('.').next())
        .unwrap_or("Placed level")
        .to_string();
    let guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(world, guid, &name, Transform::from_translation(position))
        .and_then(|command| command.with_component(world, PlacedLevel::inline(level).as_partial_reflect()))
        .map_err(|e| RpcError::internal(e.to_string()))?;
    let mut despawn = DespawnEntities {
        entities: roots.iter().map(|entity| entity.to_bits()).collect(),
        snapshot: None,
    };
    despawn.apply(world);
    spawn.apply(world);
    placed::spawn_placed_levels(world);
    let mut group = CommandGroup::new("Create placed level");
    group.push(Box::new(despawn));
    group.push(Box::new(spawn));
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(group));

    let instance = crate::layers_panel::entity_of(world, guid)
        .ok_or_else(|| RpcError::internal("the instance did not spawn"))?;
    if let Some(error) = world.get::<PlacedError>(instance) {
        return Err(refused(error.reason, error.detail.clone()));
    }
    if let Some(mut selection) = world.get_resource_mut::<Selection>() {
        selection.select(instance, SelectMode::Replace);
    }
    Ok(PlacedCreated {
        instance,
        level,
        path: full,
        moved,
        pivot: position,
    })
}

fn placed_create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let pivot = match params.get("pivot").and_then(Value::as_str) {
        None | Some("lowest_center") => PlacedPivot::LowestCenter,
        Some("center") => PlacedPivot::Center,
        Some(text) => PlacedPivot::Member(text.parse().map_err(|_| {
            RpcError::invalid_params("`pivot` is lowest_center, center or the guid of a selected entity")
        })?),
    };
    let created = create_placed_level(world, path, pivot)?;
    Ok(json!({
        "entity": created.instance.to_bits(),
        "level": format!("asset:{}", created.level.0),
        "path": created.path.display().to_string(),
        "moved": created.moved,
        "pivot": created.pivot.to_array(),
    }))
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.placed_create",
            tool: "placed_create",
            description: "Create placed level: move the selected entities into a new level asset (guids kept, positions relative to the pivot) and put an instance in their place. One undo step; the files stay on disk when it is undone.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "The new level under assets/; .level.ron or .level.d chooses the storage form.",
                },
                ParamSchema {
                    name: "pivot",
                    kind: "string",
                    required: false,
                    description: "lowest_center (default), center, or the guid of a selected entity.",
                },
            ],
            result: "{ entity, level, path, moved, pivot }",
            mutating: true,
            permission: Permission::EditScene,
        },
        placed_create as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::*;
    use regia_scene::level::{LoadMode, open_level};
    use regia_scene::placed::PlacedSpawned;
    use regia_test_utils::unique_temp_dir;

    use super::*;

    const HOST: LevelId = LevelId(Guid::from_u128(0xa0));
    const LEFT: Guid = Guid::from_u128(1);
    const RIGHT: Guid = Guid::from_u128(2);
    const CHILD: Guid = Guid::from_u128(3);

    fn record(guid: Guid, parent: Option<Guid>, position: [f32; 3]) -> String {
        let parent = parent.map_or("None".to_string(), |p| format!(r#"Some("{p}")"#));
        format!(
            r#"(guid: "{guid}", parent: {parent}, components: {{ "{TRANSFORM_PATH}": (translation: ({:?}, {:?}, {:?}), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)) }}),"#,
            position[0], position[1], position[2]
        )
    }

    fn app(root: &Path) -> App {
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        app.world_mut().insert_resource(Selection::default());
        app.world_mut().insert_resource(UndoStack::new());
        let text = format!(
            "SceneFile(format_version: {}, entities: [{}{}{}], resources: {{}})",
            regia_scene::format::SCENE_FORMAT_VERSION,
            record(LEFT, None, [2.0, 1.0, 0.0]),
            record(RIGHT, None, [4.0, 3.0, 0.0]),
            record(CHILD, Some(LEFT), [0.0, 1.0, 0.0]),
        );
        let level = Level::from_ron_str(HOST, &text).expect("level");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), HOST, LoadMode::Replace).expect("open");
        app.world_mut().insert_resource(OpenLevel {
            id: HOST,
            path: Some(root.join("assets/host.level.ron")),
        });
        app.update();
        app
    }

    fn entity(app: &mut App, guid: Guid) -> Option<Entity> {
        crate::layers_panel::entity_of(app.world_mut(), guid)
    }

    fn select(app: &mut App, guids: &[Guid]) {
        for guid in guids {
            let entity = entity(app, *guid).expect("entity");
            app.world_mut()
                .resource_mut::<Selection>()
                .select(entity, SelectMode::Add);
        }
    }

    fn call(app: &mut App, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.placed_create")
            .expect("registered");
        handler(app.world_mut(), params)
    }

    fn code(err: RpcError) -> String {
        err.data.expect("data")["code"]
            .as_str()
            .expect("code")
            .to_string()
    }

    fn translation(scene: &SceneFile, guid: Guid) -> Vec3 {
        let record = scene.entities.iter().find(|e| e.guid == guid).expect("record");
        let raw = record.components.get(TRANSFORM_PATH).expect("transform");
        let text = raw.get_ron();
        let start = text.find("translation").expect("translation");
        let open = start + text[start..].find('(').expect("open") + 1;
        let close = open + text[open..].find(')').expect("close");
        let values: Vec<f32> = text[open..close]
            .split(',')
            .filter_map(|v| v.rsplit(':').next()?.trim().parse().ok())
            .collect();
        Vec3::new(values[0], values[1], values[2])
    }

    #[test]
    fn the_selection_becomes_an_instance_at_the_pivot_and_one_undo_brings_it_back() {
        let root = unique_temp_dir("placed_create");
        let mut app = app(&root);
        select(&mut app, &[LEFT, RIGHT, CHILD]);
        let out = call(
            &mut app,
            &json!({ "path": "rooms/kitchen.level.ron", "pivot": LEFT.to_string() }),
        )
        .expect("created");
        assert_eq!(out["moved"], 3, "the child moves with its parent");
        assert_eq!(out["pivot"], json!([2.0, 1.0, 0.0]));

        let file = root.join("assets/rooms/kitchen.level.ron");
        let scene = SceneFile::from_ron_str(&std::fs::read_to_string(&file).expect("file")).expect("scene");
        let guids: Vec<Guid> = scene.entities.iter().map(|e| e.guid).collect();
        assert!(guids.contains(&LEFT) && guids.contains(&RIGHT), "guids are kept");
        assert_eq!(translation(&scene, LEFT), Vec3::ZERO, "the pivot is the origin");
        assert_eq!(translation(&scene, RIGHT), Vec3::new(2.0, 2.0, 0.0));
        assert_eq!(
            translation(&scene, CHILD),
            Vec3::new(0.0, 1.0, 0.0),
            "a child stays local"
        );

        let instance = Entity::try_from_bits(out["entity"].as_u64().expect("bits")).expect("entity");
        app.update();
        let filled = app.world().get::<PlacedSpawned>(instance).expect("filled");
        assert_eq!(filled.entities, 3);
        assert_eq!(app.world().resource::<Selection>().entities(), [instance]);
        assert!(entity(&mut app, LEFT).is_none(), "the originals left the host");
        let id = regia_framework::level_assets::level_id_of(&file);
        assert_eq!(out["level"], format!("asset:{}", id.0), "the sidecar names it");

        let host = root.join("assets/host.level.ron");
        let open = app.world().resource::<OpenLevel>().clone();
        level_io::save_level_file(app.world_mut(), &open, &host).expect("save");
        let saved = SceneFile::from_ron_str(&std::fs::read_to_string(&host).expect("host")).expect("scene");
        assert_eq!(saved.entities.len(), 1, "the host holds the instance alone");
        assert!(PlacedLevel::from_record(&saved.entities[0]).is_some());

        let stack = app.world_mut().remove_resource::<UndoStack>().expect("stack");
        assert_eq!(stack.undo_depth(), 1, "one undo step");
        app.world_mut().insert_resource(stack);
        assert!(crate::command::undo_world(app.world_mut()));
        app.update();
        assert!(entity(&mut app, LEFT).is_some() && entity(&mut app, RIGHT).is_some());
        assert!(app.world().get_entity(instance).is_err(), "the instance is gone");
        assert!(file.is_file(), "the asset stays");

        assert!(crate::command::redo_world(app.world_mut()));
        app.update();
        assert!(entity(&mut app, LEFT).is_none(), "a redo moves them again");
        let mut query = app.world_mut().query::<&PlacedSpawned>();
        let total: usize = query.iter(app.world()).map(|s| s.entities).sum();
        assert_eq!(total, 3, "and refills the instance");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn the_directory_form_and_the_default_pivot() {
        let root = unique_temp_dir("placed_create_dir");
        let mut app = app(&root);
        select(&mut app, &[RIGHT]);
        let out = call(&mut app, &json!({ "path": "rooms/hall.level.d" })).expect("created");
        assert!(root.join("assets/rooms/hall.level.d").is_dir());
        assert!(regia_asset::meta_path_for(&root.join("assets/rooms/hall.level.d")).is_file());
        let pivot = out["pivot"].as_array().expect("pivot");
        assert!(
            pivot[1].as_f64().expect("y") < 3.0,
            "the default pivot is the lowest point: {out}"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn refusals_change_nothing() {
        let root = unique_temp_dir("placed_create_refusals");
        let mut app = app(&root);
        let err = call(&mut app, &json!({ "path": "a.level.ron" })).expect_err("empty");
        assert_eq!(code(err), "selection_empty");
        select(&mut app, &[LEFT]);
        for bad in ["../a.level.ron", "/tmp/a.level.ron", "a.ron"] {
            let err = call(&mut app, &json!({ "path": bad })).expect_err("path");
            assert_eq!(code(err), "level_path_invalid", "{bad}");
        }
        std::fs::write(root.join("assets/taken.level.ron"), "x").expect("taken");
        let err = call(&mut app, &json!({ "path": "taken.level.d" })).expect_err("exists");
        assert_eq!(code(err), "level_exists", "either form of the name is taken");
        let err = call(
            &mut app,
            &json!({ "path": "a.level.ron", "pivot": RIGHT.to_string() }),
        )
        .expect_err("pivot");
        assert_eq!(code(err), "pivot_not_selected");
        assert!(entity(&mut app, LEFT).is_some());
        assert!(!root.join("assets/a.level.ron").exists());
        assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 0);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
