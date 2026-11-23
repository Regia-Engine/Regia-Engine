// SPDX-License-Identifier: MPL-2.0
//! Outliner folders (ADR-7757, `docs/specs/19-editor.md` § 2): tree folders that hold top-level
//! entities without being entities. Membership is the saved
//! [`EditorFolder`] component (a `/`-separated path such
//! as `"Lights/Street"`); the Outliner draws one row per folder level. Moving entities in or out,
//! renaming and deleting a folder are one undo step each; deleting a folder keeps its entities
//! (they move up to the parent folder). A folder with no members yet (New Folder) lives in the
//! session's [`OutlinerFolders`] until an entity is moved into it.
//!
//! Folders hold top-level entities only: a child entity follows its parent.

use std::collections::BTreeSet;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{AddComponent, EditorCommand, RemoveComponent, UndoStack};
use crate::hierarchy_ops::CommandBatch;

pub use regia_scene::components::EditorFolder;

/// The registered short type name of [`EditorFolder`] (what the undo commands address).
const COMPONENT: &str = "EditorFolder";

/// The default name of a new folder ("New Folder", then "New Folder 2", ...).
pub const NEW_FOLDER: &str = "New Folder";

/// Folders the user created that hold no entity yet (session state, not saved: a folder is saved
/// through its members' [`EditorFolder`] components).
#[derive(Resource, Debug, Clone, Default)]
pub struct OutlinerFolders {
    /// Empty folder paths.
    pub empty: BTreeSet<String>,
}

/// Why a folder edit was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FolderError {
    /// The path is empty, or a segment is blank.
    #[error("{0:?} is not a folder path (names separated by /)")]
    BadPath(String),
    /// No such folder.
    #[error("no folder {0:?}")]
    Missing(String),
    /// The rename target already exists.
    #[error("a folder {0:?} already exists")]
    Exists(String),
    /// A moved entity has a parent (folders hold top-level entities).
    #[error("entity {0} is not a top-level entity")]
    NotTopLevel(u64),
    /// A moved entity does not exist or is editor-internal.
    #[error("entity {0} is not in the level")]
    EntityMissing(u64),
    /// No entity was given.
    #[error("no entities given")]
    NoEntities,
    /// The undo command could not be built (the component type is not registered).
    #[error("folder component: {0}")]
    Command(String),
}

impl FolderError {
    /// The stable error code (`book/agents/outliner.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPath(_) => "folder_bad_path",
            Self::Missing(_) => "folder_missing",
            Self::Exists(_) => "folder_exists",
            Self::NotTopLevel(_) => "folder_entity_not_top_level",
            Self::EntityMissing(_) => "folder_entity_missing",
            Self::NoEntities => "folder_no_entities",
            Self::Command(_) => "folder_command_failed",
        }
    }
}

/// Normalizes a folder path: trims every segment and drops leading, trailing and doubled `/`.
///
/// # Errors
/// [`FolderError::BadPath`] when nothing is left or a segment between two `/` is blank.
pub fn normalize_path(text: &str) -> Result<String, FolderError> {
    let trimmed = text.trim().trim_matches('/');
    let segments: Vec<&str> = trimmed.split('/').map(str::trim).collect();
    if trimmed.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return Err(FolderError::BadPath(text.to_string()));
    }
    Ok(segments.join("/"))
}

/// The parent folder of `path` (`None` for a top-level folder).
#[must_use]
pub fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

/// The last segment of `path` (the folder row's label).
#[must_use]
pub fn folder_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Whether `path` is `folder` or inside it.
#[must_use]
pub fn is_within(path: &str, folder: &str) -> bool {
    path == folder
        || path
            .strip_prefix(folder)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `entity`'s folder path, if it is a top-level entity in one.
#[must_use]
pub fn folder_of(world: &World, entity: Entity) -> Option<String> {
    if world.get::<ChildOf>(entity).is_some() {
        return None;
    }
    world
        .get::<EditorFolder>(entity)
        .and_then(|f| normalize_path(&f.path).ok())
}

/// Every folder path of the level and session, every parent level included, sorted.
#[must_use]
pub fn all_folders(world: &World) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut add = |path: &str| {
        let mut prefix = String::new();
        for segment in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            out.insert(prefix.clone());
        }
    };
    for entity in world.iter_entities() {
        if let Some(path) = folder_of(world, entity.id()) {
            add(&path);
        }
    }
    if let Some(session) = world.get_resource::<OutlinerFolders>() {
        for path in &session.empty {
            add(path);
        }
    }
    out
}

/// The top-level entities in `folder` or its subfolders.
fn members(world: &World, folder: &str) -> Vec<(Entity, String)> {
    let mut out: Vec<(Entity, String)> = world
        .iter_entities()
        .filter_map(|e| folder_of(world, e.id()).map(|path| (e.id(), path)))
        .filter(|(_, path)| is_within(path, folder))
        .collect();
    out.sort_by_key(|(e, _)| *e);
    out
}

/// One command setting (`Some`) or clearing (`None`) an entity's folder.
fn set_command(
    world: &World,
    entity: Entity,
    path: Option<&str>,
) -> Result<Box<dyn EditorCommand>, FolderError> {
    match path {
        Some(path) => AddComponent::capture(
            world,
            entity,
            COMPONENT,
            Box::new(EditorFolder {
                path: path.to_string(),
            }),
        )
        .map(|c| Box::new(c) as Box<dyn EditorCommand>),
        None => {
            RemoveComponent::capture(world, entity, COMPONENT).map(|c| Box::new(c) as Box<dyn EditorCommand>)
        }
    }
    .map_err(|e| FolderError::Command(e.to_string()))
}

/// Applies `commands` and pushes them as one undo entry named `label` (nothing when empty).
fn push_batch(world: &mut World, label: String, mut commands: Vec<Box<dyn EditorCommand>>) {
    if commands.is_empty() {
        return;
    }
    for command in &mut commands {
        command.apply(world);
    }
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(Box::new(CommandBatch { label, commands }));
    stack.seal();
}

/// The first free folder name under `parent` ("New Folder", "New Folder 2", ...).
#[must_use]
pub fn free_folder_path(world: &World, parent: Option<&str>, base: &str) -> String {
    let taken = all_folders(world);
    let join = |name: &str| match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name.to_string(),
    };
    let mut names = crate::hierarchy_ops::CopyNames::default();
    for path in &taken {
        if parent_path(path) == parent {
            names.note(folder_name(path));
        }
    }
    if !taken.contains(&join(base)) {
        return join(base);
    }
    join(&names.next_name(base))
}

/// Creates the folder `path` (session state until an entity moves in). Returns the normalized
/// path; creating an existing folder is not an error.
///
/// # Errors
/// [`FolderError::BadPath`].
pub fn create_folder(world: &mut World, path: &str) -> Result<String, FolderError> {
    let path = normalize_path(path)?;
    if !all_folders(world).contains(&path) {
        world
            .get_resource_or_insert_with(OutlinerFolders::default)
            .empty
            .insert(path.clone());
    }
    Ok(path)
}

/// Moves top-level `entities` into `folder` (`None` = out of every folder) as one undo step.
/// The folders they leave stay listed for the session. Returns the entities that changed.
///
/// # Errors
/// A [`FolderError`] when no entity is given, one is missing, internal or has a parent, or the
/// path is malformed. Nothing is changed then.
pub fn move_to_folder(
    world: &mut World,
    entities: &[Entity],
    folder: Option<&str>,
) -> Result<Vec<Entity>, FolderError> {
    if entities.is_empty() {
        return Err(FolderError::NoEntities);
    }
    let folder = folder.map(normalize_path).transpose()?;
    let mut commands = Vec::new();
    let mut moved = Vec::new();
    let mut left = Vec::new();
    for &entity in entities {
        if world.get_entity(entity).is_err() || crate::visibility::is_hidden(world, entity) {
            return Err(FolderError::EntityMissing(entity.to_bits()));
        }
        if world.get::<ChildOf>(entity).is_some() {
            return Err(FolderError::NotTopLevel(entity.to_bits()));
        }
        let current = folder_of(world, entity);
        if current == folder || moved.contains(&entity) {
            continue;
        }
        commands.push(set_command(world, entity, folder.as_deref())?);
        if let Some(old) = current {
            left.push(old);
        }
        moved.push(entity);
    }
    let label = match (&folder, moved.len()) {
        (Some(path), 1) => format!("Move to folder {path}"),
        (Some(path), n) => format!("Move {n} entities to folder {path}"),
        (None, 1) => "Move out of folder".to_string(),
        (None, n) => format!("Move {n} entities out of folders"),
    };
    push_batch(world, label, commands);
    let mut session = world.get_resource_or_insert_with(OutlinerFolders::default);
    session.empty.extend(left);
    if let Some(path) = &folder {
        session.empty.retain(|p| !is_within(path, p));
    }
    Ok(moved)
}

/// Renames (or moves) the folder `from` to `to`, its subfolders and members with it, as one undo
/// step. Returns how many entities changed.
///
/// # Errors
/// [`FolderError::Missing`] when `from` does not exist, [`FolderError::Exists`] when `to` does,
/// [`FolderError::BadPath`] for a malformed path.
pub fn rename_folder(world: &mut World, from: &str, to: &str) -> Result<usize, FolderError> {
    let from = normalize_path(from)?;
    let to = normalize_path(to)?;
    let folders = all_folders(world);
    if !folders.contains(&from) {
        return Err(FolderError::Missing(from));
    }
    if from == to {
        return Ok(0);
    }
    if folders.contains(&to) && !is_within(&to, &from) {
        return Err(FolderError::Exists(to));
    }
    let renamed = |path: &str| format!("{to}{}", &path[from.len()..]);
    let mut commands = Vec::new();
    let mut count = 0;
    for (entity, path) in members(world, &from) {
        commands.push(set_command(world, entity, Some(&renamed(&path)))?);
        count += 1;
    }
    push_batch(world, format!("Rename folder {from} to {to}"), commands);
    let mut session = world.get_resource_or_insert_with(OutlinerFolders::default);
    let empty: Vec<String> = session.empty.iter().cloned().collect();
    session.empty = empty
        .into_iter()
        .map(|path| {
            if is_within(&path, &from) {
                renamed(&path)
            } else {
                path
            }
        })
        .collect();
    Ok(count)
}

/// Deletes the folder `path`, keeping its entities: they and its subfolders move up to the
/// parent folder (or out of folders), as one undo step. Returns how many entities moved.
///
/// # Errors
/// [`FolderError::Missing`] or [`FolderError::BadPath`].
pub fn delete_folder(world: &mut World, path: &str) -> Result<usize, FolderError> {
    let path = normalize_path(path)?;
    if !all_folders(world).contains(&path) {
        return Err(FolderError::Missing(path));
    }
    let parent = parent_path(&path).map(str::to_string);
    let lifted = |member: &str| -> Option<String> {
        let rest = member[path.len()..].trim_start_matches('/');
        match (&parent, rest.is_empty()) {
            (Some(parent), true) => Some(parent.clone()),
            (Some(parent), false) => Some(format!("{parent}/{rest}")),
            (None, true) => None,
            (None, false) => Some(rest.to_string()),
        }
    };
    let mut commands = Vec::new();
    let mut count = 0;
    for (entity, member) in members(world, &path) {
        commands.push(set_command(world, entity, lifted(&member).as_deref())?);
        count += 1;
    }
    push_batch(world, format!("Delete folder {path}"), commands);
    let mut session = world.get_resource_or_insert_with(OutlinerFolders::default);
    let empty: Vec<String> = session.empty.iter().cloned().collect();
    session.empty = empty
        .into_iter()
        .filter(|p| *p != path)
        .filter_map(|p| if is_within(&p, &path) { lifted(&p) } else { Some(p) })
        .collect();
    Ok(count)
}

// -------------------------------------------------------------------------------------------
// Agent surface

/// The protocol error of a refused folder edit: `F0002` (bad parameters) or `F0003` (not found)
/// with [`FolderError::code`] in `data.folder_error`.
#[must_use]
pub fn folder_rpc_error(err: &FolderError) -> RpcError {
    use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND};
    let (code, regia_code) = match err {
        FolderError::Missing(_) | FolderError::EntityMissing(_) => (NOT_FOUND, "F0003"),
        _ => (INVALID_PARAMS, "F0002"),
    };
    let mut error = RpcError::new(
        code,
        regia_code,
        err.to_string(),
        "list folders with editor.outliner_rows; folders hold top-level entities",
    );
    if let Some(data) = error.data.as_mut() {
        data["folder_error"] = json!(err.code());
    }
    error
}

fn string_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (a folder path string) is required")))
}

fn folders_json(world: &World) -> Value {
    json!(all_folders(world).into_iter().collect::<Vec<_>>())
}

fn folder_create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = create_folder(world, string_param(params, "path")?).map_err(|e| folder_rpc_error(&e))?;
    Ok(json!({ "path": path, "folders": folders_json(world) }))
}

fn folder_rename(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = string_param(params, "path")?;
    let to = string_param(params, "to")?;
    let moved = rename_folder(world, from, to).map_err(|e| folder_rpc_error(&e))?;
    Ok(json!({ "moved": moved, "folders": folders_json(world) }))
}

fn folder_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let moved = delete_folder(world, string_param(params, "path")?).map_err(|e| folder_rpc_error(&e))?;
    Ok(json!({ "moved": moved, "folders": folders_json(world) }))
}

fn folder_move(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = crate::hierarchy_agent::entities_param(world, params)?;
    let path = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) => Some(path.as_str()),
        Some(_) => return Err(RpcError::wrong_type("path", "a folder path string or null")),
    };
    let moved = move_to_folder(world, &entities, path).map_err(|e| folder_rpc_error(&e))?;
    Ok(json!({
        "moved": moved.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "folders": folders_json(world),
    }))
}

const P_PATH: ParamSchema = ParamSchema {
    name: "path",
    kind: "string",
    required: true,
    description: "A folder path, folder names separated by / (\"Lights/Street\").",
};

/// The folder methods (`editor.folder_create`, `_rename`, `_delete`, `_move`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.folder_create",
                tool: "folder_create",
                description: "Create an Outliner folder (it holds no entity until one is moved in, and is saved \
                    through its members). Creating an existing folder is not an error.",
                params: &[P_PATH],
                result: "{ path, folders }",
                mutating: true,
                permission: Permission::EditScene,
            },
            folder_create as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.folder_rename",
                tool: "folder_rename",
                description: "Rename or move an Outliner folder with its subfolders and entities (one undo step).",
                params: &[
                    P_PATH,
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: true,
                        description: "The new folder path.",
                    },
                ],
                result: "{ moved, folders }",
                mutating: true,
                permission: Permission::EditScene,
            },
            folder_rename as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.folder_delete",
                tool: "folder_delete",
                description: "Delete an Outliner folder, keeping its entities: they and its subfolders move up \
                    to the parent folder (one undo step).",
                params: &[P_PATH],
                result: "{ moved, folders }",
                mutating: true,
                permission: Permission::EditScene,
            },
            folder_delete as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.folder_move",
                tool: "folder_move",
                description: "Move top-level entities into an Outliner folder, or out of every folder with \
                    path null (one undo step).",
                params: &[
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids; absent = the editor selection.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The folder path; null or absent = out of folders.",
                    },
                ],
                result: "{ moved, folders }",
                mutating: true,
                permission: Permission::EditScene,
            },
            folder_move as crate::viewport_agent::Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::{EntityGuid, Name, Transform};

    fn app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    fn spawn(world: &mut World, name: &str) -> Entity {
        world
            .spawn((
                Name::new(name.to_string()),
                Transform::default(),
                EntityGuid::new_random(),
            ))
            .id()
    }

    #[test]
    fn paths_normalize_and_bad_ones_are_refused() {
        assert_eq!(
            normalize_path(" Lights / Street/ ").as_deref(),
            Ok("Lights/Street")
        );
        assert_eq!(normalize_path("/A//").as_deref(), Ok("A"));
        assert_eq!(
            normalize_path("A//B").map_err(|e| e.code()),
            Err("folder_bad_path")
        );
        assert_eq!(normalize_path("  ").map_err(|e| e.code()), Err("folder_bad_path"));
        assert!(is_within("A/B", "A") && is_within("A", "A") && !is_within("AB", "A"));
        assert_eq!(folder_name("A/B/C"), "C");
        assert_eq!(parent_path("A/B/C"), Some("A/B"));
        assert_eq!(parent_path("A"), None);
    }

    #[test]
    fn move_rename_delete_are_one_undo_step_each_and_keep_entities() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = spawn(world, "Lamp");
        let post = spawn(world, "Post");
        let kid = spawn(world, "Kid");
        world.entity_mut(kid).insert(ChildOf(lamp));
        assert_eq!(
            move_to_folder(world, &[kid], Some("Lights")).map_err(|e| e.code()),
            Err("folder_entity_not_top_level")
        );
        move_to_folder(world, &[lamp, post], Some("Lights/Street")).expect("move");
        assert_eq!(folder_of(world, lamp).as_deref(), Some("Lights/Street"));
        assert_eq!(
            all_folders(world).into_iter().collect::<Vec<_>>(),
            ["Lights", "Lights/Street"]
        );
        let depth = world.resource::<UndoStack>().undo_depth();
        assert_eq!(depth, 1);

        assert_eq!(rename_folder(world, "Lights", "Lamps").expect("rename"), 2);
        assert_eq!(folder_of(world, post).as_deref(), Some("Lamps/Street"));
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        assert_eq!(
            rename_folder(world, "Nope", "X").map_err(|e| e.code()),
            Err("folder_missing")
        );

        create_folder(world, "Other").expect("create");
        assert_eq!(
            rename_folder(world, "Lamps", "Other").map_err(|e| e.code()),
            Err("folder_exists")
        );

        assert_eq!(delete_folder(world, "Lamps").expect("delete"), 2);
        assert_eq!(
            folder_of(world, lamp).as_deref(),
            Some("Street"),
            "the subfolder moves up a level"
        );
        assert!(
            world.get_entity(lamp).is_ok(),
            "deleting a folder keeps its entities"
        );
        assert_eq!(delete_folder(world, "Street").expect("delete"), 2);
        assert_eq!(
            folder_of(world, lamp),
            None,
            "a top-level folder's entities leave folders"
        );

        assert!(crate::command::undo_world(world));
        assert_eq!(
            folder_of(world, lamp).as_deref(),
            Some("Street"),
            "one undo per delete"
        );
        assert!(crate::command::undo_world(world));
        assert!(crate::command::undo_world(world));
        assert_eq!(folder_of(world, lamp).as_deref(), Some("Lights/Street"));
        assert!(crate::command::undo_world(world));
        assert_eq!(folder_of(world, lamp), None);
    }

    #[test]
    fn new_folder_names_count_up_and_empty_folders_are_listed() {
        let mut app = app();
        let world = app.world_mut();
        assert_eq!(free_folder_path(world, None, NEW_FOLDER), "New Folder");
        create_folder(world, "New Folder").expect("create");
        assert_eq!(free_folder_path(world, None, NEW_FOLDER), "New Folder 2");
        assert_eq!(
            free_folder_path(world, Some("New Folder"), NEW_FOLDER),
            "New Folder/New Folder"
        );
        assert!(all_folders(world).contains("New Folder"));
    }

    #[test]
    fn the_folder_component_saves_with_the_level() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = spawn(world, "Lamp");
        move_to_folder(world, &[lamp], Some("Lights")).expect("move");
        let file =
            regia_scene::prelude::snapshot_scene(world, &regia_scene::spawn::SnapshotFilter::default())
                .expect("snapshot");
        let text = file.to_ron_string().expect("ron");
        assert!(text.contains("EditorFolder"), "{text}");
        assert!(text.contains("\"Lights\""), "{text}");
    }
}
