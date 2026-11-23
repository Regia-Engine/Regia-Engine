// SPDX-License-Identifier: MPL-2.0
//! Create prefab from selection (`docs/specs/19-editor.md` § 2 Outliner, ADR-5615 § 2,
//! ADR-7468): Outliner ▸ Create prefab from selection… and `editor.create_prefab`.
//!
//! The selected entities (their top-level members: an entity whose ancestor is also selected
//! travels with that ancestor) and their subtrees become a `.prefab.ron` at the current scene
//! format version, and the selection is replaced by one linked instance of it, every entity at
//! the same world transform. One undo step.
//!
//! **Root rule** (ADR-7468):
//! - One top-level entity: it is the prefab's root. Its record keeps its components with an
//!   identity `Transform`; the instance (the wrapper entity that carries `PrefabInstance`) gets
//!   the entity's local `Transform` and goes under the entity's parent, so the pose is exactly
//!   the old one.
//! - Several: a new root entity named after the file is the prefab's root. The members hang under
//!   it with their transforms relative to the centroid of their world positions; the instance sits
//!   at that centroid, under the members' common parent (the scene root when they differ), like
//!   Outliner ▸ Group.
//!
//! The members' scene guids become the prefab's local ids (a nested prefab instance in the
//! selection stays a nested instance). The written file is noted in
//! [`crate::prefab_link::PrefabWriteEcho`] so the asset watcher's reload of it does not respawn
//! the new instance. Undo despawns the instance and brings the original entities back with their
//! guids, so a level save after the undo writes the same bytes as before; the written file stays
//! on disk (an undo does not delete files).
//!
//! **References** (ADR-8593): a component field that holds an `Entity` and named a member (from
//! outside the selection or from another member) names the instance's entity for that member
//! afterwards; the undo points it at the restored member and the redo at the new instance again
//! (`refs`). A reference to an entity outside the selection is kept. Scene files store `Entity`
//! fields as raw ids, so the written prefab file's references hold only for this session.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::prelude::*;
use regia_scene::prefab::{PrefabLibrary, PrefabLink, PrefabTemplate, PrefabWorldExt as _};
use regia_scene::prelude::{EntityGuid, SceneFile, SpawnOptions, Transform};
use serde_json::{Value, json};

use crate::command::{DespawnEntities, EditorCommand, SpawnedRoots};
use crate::hierarchy_ops::{self, CommandBatch, HierarchyError};
use crate::viewport_agent::Handler;

/// The suffix a prefab file name must carry.
pub const PREFAB_SUFFIX: &str = ".prefab.ron";

/// The scene type path of `Transform` in scene files.
const TRANSFORM_KEY: &str = "regia_scene::transform::Transform";
/// The level record key of the Outliner's folder component (never copied into a prefab).
const EDITOR_FOLDER_KEY: &str = "regia_scene::components::EditorFolder";

/// Why [`create_prefab`] refused. [`code`](Self::code) is the stable protocol reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreatePrefabError {
    /// The selection is not editable as a whole (missing, internal or feed-owned entities, none
    /// given, or a `ChildOf` cycle): the hierarchy edits' own refusal.
    #[error("{0}")]
    Hierarchy(HierarchyError),
    /// A top-level entity lives inside another prefab instance (it has a `PrefabLink`): break that
    /// instance's link first, or select its instance root.
    #[error("entity {0} is inside a prefab instance; select the instance root or break its link first")]
    PrefabInternal(u64),
    /// Prefab edit mode or play-in-editor is running.
    #[error("{0}")]
    Busy(String),
    /// The path does not name a `.prefab.ron` file.
    #[error("{0}")]
    BadPath(String),
    /// A file already exists at the path.
    #[error("{0} already exists; pick another name")]
    Exists(String),
    /// Writing the file or its `.meta` failed.
    #[error("{0}")]
    Io(String),
    /// The selection did not snapshot, bake or instantiate as a prefab.
    #[error("{0}")]
    Scene(String),
}

impl CreatePrefabError {
    /// The lowercase protocol reason (documented in `book/agents/prefab-create.md`); a hierarchy
    /// refusal reports its own `hierarchy_*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Hierarchy(err) => err.code(),
            Self::PrefabInternal(_) => "prefab_create_internal",
            Self::Busy(_) => "prefab_create_busy",
            Self::BadPath(_) => "prefab_create_bad_path",
            Self::Exists(_) => "prefab_create_exists",
            Self::Io(_) => "prefab_create_io",
            Self::Scene(_) => "prefab_create_scene",
        }
    }
}

impl From<HierarchyError> for CreatePrefabError {
    fn from(err: HierarchyError) -> Self {
        Self::Hierarchy(err)
    }
}

/// What [`create_prefab`] made.
#[derive(Debug, Clone)]
pub struct CreatedPrefab {
    /// The new instance (the wrapper entity that carries `PrefabInstance`).
    pub instance: Entity,
    /// The prefab's asset id (its `.meta`).
    pub asset: Guid,
    /// The written `.prefab.ron`.
    pub path: PathBuf,
    /// Entity records in the prefab file.
    pub entities: usize,
    /// The top-level entities the instance replaced (input order).
    pub replaced: Vec<Entity>,
    /// Undo entries afterwards.
    pub undo_depth: usize,
    /// Entity references (component fields holding an `Entity`) moved from a member to the
    /// instance's entity for it (ADR-8593).
    pub retargeted: usize,
}

/// The file stem of a `.prefab.ron` path (`crate.prefab.ron` → `crate`).
///
/// # Errors
/// [`CreatePrefabError::BadPath`] when the file name does not end in [`PREFAB_SUFFIX`] or has
/// nothing before it.
pub fn prefab_stem(path: &Path) -> Result<String, CreatePrefabError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CreatePrefabError::BadPath(format!("{} has no file name", path.display())))?;
    match name.strip_suffix(PREFAB_SUFFIX) {
        Some(stem) if !stem.is_empty() && !stem.ends_with(".variant") => Ok(stem.to_string()),
        _ => Err(CreatePrefabError::BadPath(format!(
            "{name} is not a prefab file name (it must end in {PREFAB_SUFFIX}, e.g. crate{PREFAB_SUFFIX})"
        ))),
    }
}

/// The serialized `transform`, as scene files store it.
fn transform_raw(
    world: &World,
    transform: &Transform,
) -> Result<Box<ron::value::RawValue>, CreatePrefabError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
    let text = codec
        .to_ron_typed(transform.as_partial_reflect())
        .map_err(|e| CreatePrefabError::Scene(format!("transform: {e}")))?;
    ron::value::RawValue::from_ron(text.trim_end())
        .map(|raw| raw.to_owned())
        .map_err(|e| CreatePrefabError::Scene(format!("transform: {e}")))
}

/// The prefab's contents and the instance's placement, planned from the live selection.
struct Plan {
    /// The prefab file (one root).
    file: SceneFile,
    /// The instance's local transform under `parent`.
    transform: Transform,
    /// The instance's parent (`None` = the scene root).
    parent: Option<Entity>,
}

/// Plans the prefab of `top` (top-level, validated entities with guids): see the module docs'
/// root rule.
fn plan(world: &mut World, top: &[Entity], stem: &str) -> Result<Plan, CreatePrefabError> {
    // Guids of every entity the prefab holds (prefab-internal members of a nested instance are
    // not snapshotted: their instance root's record carries them).
    let mut guids = std::collections::HashSet::new();
    for &entity in top {
        for member in crate::outliner::subtree_of(world, entity) {
            if let Some(guid) = world.get::<EntityGuid>(member) {
                guids.insert(guid.0);
            }
        }
    }
    let filter = regia_scene::spawn::SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let scene = regia_scene::prelude::snapshot_scene(world, &filter)
        .map_err(|e| CreatePrefabError::Scene(format!("snapshot: {e}")))?;
    let mut file = SceneFile::new();
    file.entities = scene
        .entities
        .into_iter()
        .filter(|record| guids.contains(&record.guid))
        .collect();
    // The Outliner folder is the level's editor-only tree data (ADR-7757): a prefab made from a
    // foldered entity must not put every later placement into that folder (review
    // sprint-w12-rev-early-k row 11).
    for record in &mut file.entities {
        record.components.remove(EDITOR_FOLDER_KEY);
    }
    let top_guids: Vec<Guid> = top
        .iter()
        .filter_map(|e| world.get::<EntityGuid>(*e).map(|g| g.0))
        .collect();
    let identity = transform_raw(world, &Transform::default())?;

    if let [only] = top {
        let only = *only;
        let Some(guid) = top_guids.first().copied() else {
            return Err(HierarchyError::NoGuid(only.to_bits()).into());
        };
        for record in &mut file.entities {
            if record.guid == guid {
                record.parent = None;
                if record.components.contains_key(TRANSFORM_KEY) {
                    record
                        .components
                        .insert(TRANSFORM_KEY.to_string(), identity.clone());
                }
            }
        }
        return Ok(Plan {
            file,
            transform: world.get::<Transform>(only).copied().unwrap_or_default(),
            parent: world.get::<ChildOf>(only).map(ChildOf::parent),
        });
    }

    // Several members: a new root at their centroid, under their common parent.
    let parents: Vec<Option<Entity>> = top
        .iter()
        .map(|e| world.get::<ChildOf>(*e).map(ChildOf::parent))
        .collect();
    let parent = parents
        .first()
        .copied()
        .flatten()
        .filter(|first| parents.iter().all(|p| *p == Some(*first)));
    let centroid = top
        .iter()
        .map(|e| Vec3::from(hierarchy_ops::world_affine(world, *e).translation))
        .fold(Vec3::ZERO, |sum, p| sum + p)
        / top.len() as f32;
    let (local_translation, instance_world) = match parent {
        Some(parent) => {
            let affine = hierarchy_ops::world_affine(world, parent);
            if affine.matrix3.determinant().abs() > 1e-12 {
                let local = affine.inverse().transform_point3(centroid);
                (local, affine * Affine3A::from_translation(local))
            } else {
                (Vec3::ZERO, affine)
            }
        }
        None => (centroid, Affine3A::from_translation(centroid)),
    };
    // Members are stored relative to where the instance really sits (its parent may be turned or
    // scaled), so instance × member is each member's old world matrix.
    if instance_world.matrix3.determinant().abs() <= 1e-12 {
        return Err(CreatePrefabError::Scene(
            "the members' common parent has a zero scale: the instance cannot keep their poses".to_string(),
        ));
    }
    let to_root = instance_world.inverse();
    for record in &mut file.entities {
        let Some(index) = top_guids.iter().position(|g| *g == record.guid) else {
            continue;
        };
        let entity = top[index];
        if world.get::<Transform>(entity).is_some() {
            let local = Transform::from_affine(to_root * hierarchy_ops::world_affine(world, entity));
            record
                .components
                .insert(TRANSFORM_KEY.to_string(), transform_raw(world, &local)?);
        }
    }
    let root_guid = Guid::new_random();
    for record in &mut file.entities {
        if top_guids.contains(&record.guid) {
            record.parent = Some(root_guid);
        }
    }
    file.entities.insert(
        0,
        regia_scene::format::SceneEntity {
            guid: root_guid,
            name: Some(stem.to_string()),
            parent: None,
            components: std::collections::BTreeMap::from([(TRANSFORM_KEY.to_string(), identity)]),
            prefab_instance: None,
        },
    );
    Ok(Plan {
        file,
        transform: Transform::from_translation(local_translation),
        parent,
    })
}

/// Writes `text` to `path` through a temporary file in the same directory.
fn write_new(path: &Path, text: &str) -> Result<(), CreatePrefabError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CreatePrefabError::Io(format!("cannot create {}: {e}", dir.display())))?;
    }
    crate::level_io::write_atomic(path, text.as_bytes()).map_err(CreatePrefabError::Io)
}

/// Creates a prefab from `entities` at `path` (an absolute file path; the caller confines it to
/// the project) and replaces them by a linked instance: see the module docs. The instance becomes
/// the selection. One undo step.
///
/// # Errors
/// See [`CreatePrefabError`]; nothing is written or changed on an error, except that a failure
/// after the file was written leaves the file.
pub fn create_prefab(
    world: &mut World,
    entities: &[Entity],
    path: &Path,
) -> Result<CreatedPrefab, CreatePrefabError> {
    if crate::prefab_edit::in_prefab_mode(world) {
        return Err(CreatePrefabError::Busy(
            "a prefab is open in prefab edit mode: close it first".to_string(),
        ));
    }
    if world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some())
    {
        return Err(CreatePrefabError::Busy(
            "stop play-in-editor first: the selection lives in the edit world".to_string(),
        ));
    }
    let stem = prefab_stem(path)?;
    if path.exists() || regia_asset::meta_path_for(path).exists() {
        return Err(CreatePrefabError::Exists(path.display().to_string()));
    }
    hierarchy_ops::validate_entities(world, entities)?;
    hierarchy_ops::refuse_parent_cycles(world, entities)?;
    let top = hierarchy_ops::top_level(world, entities);
    if top.is_empty() {
        return Err(HierarchyError::NoEntities.into());
    }
    for &entity in &top {
        if world.get::<PrefabLink>(entity).is_some() {
            return Err(CreatePrefabError::PrefabInternal(entity.to_bits()));
        }
        if world.get::<EntityGuid>(entity).is_none_or(|g| g.0.is_nil()) {
            return Err(HierarchyError::NoGuid(entity.to_bits()).into());
        }
    }

    // Plan and bake before anything is written: a selection that does not bake writes nothing.
    let plan = plan(world, &top, &stem)?;
    plan.file
        .validate()
        .map_err(|e| CreatePrefabError::Scene(format!("the selection is not a prefab: {e}")))?;
    let text = plan
        .file
        .to_ron_string()
        .map_err(|e| CreatePrefabError::Scene(format!("serialize: {e}")))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    // A placeholder id checks the bake; the real one comes from the `.meta` written below.
    PrefabTemplate::from_scene_file(Guid::from_u128(1), &plan.file, &registry.read())
        .map_err(|e| CreatePrefabError::Scene(format!("bake: {e}")))?;

    write_new(path, &text)?;
    let asset = crate::content::prefab_asset_id(path)
        .map_err(CreatePrefabError::Io)?
        .0;
    world
        .get_resource_or_insert_with(crate::prefab_link::PrefabWriteEcho::default)
        .note(path, &text);
    let template = PrefabTemplate::from_scene_file(asset, &plan.file, &registry.read())
        .map_err(|e| CreatePrefabError::Scene(format!("bake: {e}")))?;
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);

    // The instance joins the first member's level (else the active one, P-097) and keeps
    // `Persistent`.
    let first = top[0];
    let level = world
        .get::<regia_scene::prelude::LevelMember>(first)
        .map(regia_scene::prelude::LevelMember::level)
        .or_else(|| crate::level_io::spawn_target_level(world));
    let opts = SpawnOptions {
        level,
        persistent: world.get::<regia_scene::level::Persistent>(first).is_some(),
        unknown_types: Default::default(),
    };

    // Every member's guid: the references to members follow them into the instance (ADR-8593).
    let members: Vec<Guid> = top
        .iter()
        .flat_map(|entity| crate::outliner::subtree_of(world, *entity))
        .filter(|entity| world.get::<PrefabLink>(*entity).is_none())
        .filter_map(|entity| world.get::<EntityGuid>(entity).map(|g| g.0))
        .collect();
    let mut ids = refs::MemberIds::default();
    ids.note_plain(world, &members);

    // Replace: the members go (one snapshot for the undo), the instance comes.
    let mut despawn = DespawnEntities {
        entities: top.iter().map(|e| e.to_bits()).collect(),
        snapshot: None,
    };
    despawn.apply(world);
    let instance = match world.spawn_prefab(asset, Some(plan.transform), &opts, Guid::new_random()) {
        Ok(instance) => instance,
        Err(err) => {
            despawn.revert(world);
            return Err(CreatePrefabError::Scene(format!("instantiate: {err}")));
        }
    };
    if let Ok(mut entry) = world.get_entity_mut(instance) {
        entry.insert(regia_scene::prelude::Name::new(stem.as_str()));
        if let Some(parent) = plan.parent {
            entry.insert(ChildOf(parent));
        }
    }
    let mut spawned = SpawnedRoots {
        entities: vec![instance.to_bits()],
        snapshot: None,
    };
    spawned.apply(world);
    let retargeted = ids.retarget_to_instance(world, instance, &members);
    let instance_guid = world.get::<EntityGuid>(instance).map(|g| g.0);
    let undo_depth = crate::undo_agent::push_discrete(
        world,
        Box::new(CreatePrefabStep {
            batch: CommandBatch {
                label: format!("Create prefab {stem}"),
                commands: vec![Box::new(despawn), Box::new(spawned)],
            },
            instance: instance_guid,
            members,
            ids,
        }),
    );
    let mut selection = world
        .get_resource_or_insert_with::<crate::selection::Selection>(crate::selection::Selection::default)
        .clone();
    selection.clear();
    selection.select(instance, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    Ok(CreatedPrefab {
        instance,
        asset,
        path: path.to_path_buf(),
        entities: plan.file.entities.len(),
        replaced: top,
        undo_depth,
        retargeted,
    })
}

/// The undo entry of a Create prefab: the members' despawn and the instance's spawn, with the
/// references to members moved to the entities that stand for them after each step.
struct CreatePrefabStep {
    batch: CommandBatch,
    /// The instance's guid.
    instance: Option<Guid>,
    /// The members' guids.
    members: Vec<Guid>,
    /// Every id each member has had.
    ids: refs::MemberIds,
}

impl CreatePrefabStep {
    fn instance(&self, world: &World) -> Option<Entity> {
        self.instance
            .and_then(|guid| hierarchy_ops::guid_entity(world, guid))
    }
}

impl EditorCommand for CreatePrefabStep {
    fn apply(&mut self, world: &mut World) {
        self.ids.note_plain(world, &self.members);
        self.batch.apply(world);
        if let Some(instance) = self.instance(world) {
            self.ids.retarget_to_instance(world, instance, &self.members);
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Some(instance) = self.instance(world) {
            self.ids.note_internal(world, instance, &self.members);
        }
        self.batch.revert(world);
        self.ids.retarget_to_plain(world, &self.members);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        self.batch.changes_level_state()
    }

    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        self.batch.touched()
    }

    fn description(&self) -> String {
        self.batch.description()
    }

    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        self.batch.activity_targets()
    }
}

/// The default project path the Outliner's dialog offers for `entities`: `assets/prefabs/<name>`
/// after the first entity's name (lowercase, `_` for anything but letters and digits).
#[must_use]
pub fn default_prefab_path(world: &World, entities: &[Entity]) -> String {
    let name = entities
        .first()
        .map(|e| crate::outliner::display_name(world, *e))
        .unwrap_or_default();
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        slug = "new_prefab".to_string();
    }
    format!("assets/prefabs/{slug}{PREFAB_SUFFIX}")
}

/// The protocol error for a refusal: `invalid_params` (`F0002`) with the reason in
/// `data.reason`, a hierarchy refusal through [`crate::hierarchy_agent::hierarchy_rpc_error`]
/// (its code in `data.hierarchy_error` and `data.reason`), IO and scene failures as internal.
#[must_use]
pub fn create_prefab_rpc_error(err: &CreatePrefabError) -> RpcError {
    let mut error = match err {
        CreatePrefabError::Hierarchy(inner) => crate::hierarchy_agent::hierarchy_rpc_error(inner),
        CreatePrefabError::Io(_) | CreatePrefabError::Scene(_) => RpcError::new(
            INTERNAL_ERROR,
            "F0001",
            err.to_string(),
            "see logs.read for details",
        ),
        CreatePrefabError::PrefabInternal(_) => RpcError::new(
            INVALID_PARAMS,
            "F0002",
            err.to_string(),
            "select the instance root (scene.query_entities shows PrefabInstance) or call editor.prefab_break_link",
        ),
        CreatePrefabError::Busy(_) => RpcError::new(
            INVALID_PARAMS,
            "F0002",
            err.to_string(),
            "editor.close_prefab or editor.stop first",
        ),
        CreatePrefabError::BadPath(_) | CreatePrefabError::Exists(_) => RpcError::new(
            INVALID_PARAMS,
            "F0002",
            err.to_string(),
            "pass a new project path under assets/ ending in .prefab.ron",
        ),
    };
    let data = error.data.get_or_insert_with(|| json!({}));
    if let Value::Object(data) = data {
        data.insert("reason".into(), Value::String(err.code().to_string()));
    }
    error
}

/// The file a project-relative `path` (under `assets/`, the prefix optional) names, confined to
/// the project: absolute, drive, `..` and symlinked paths are `path_outside_project` (ADR-1816).
/// The Outliner dialog and `editor.create_prefab` both resolve through it.
///
/// # Errors
/// `path_outside_project`.
pub(crate) fn confined_prefab_path(root: &Path, path: &str) -> Result<PathBuf, RpcError> {
    // `assets_request_path` prefixes `assets/`, which would turn an absolute path into a relative
    // one: an absolute or drive path is judged as given.
    let normalized = path.trim().replace('\\', "/");
    if normalized.starts_with('/') || normalized.get(1..2) == Some(":") {
        regia_agent::req_path::project_path_in(root, "path", &normalized)?;
        return Err(with_reason_outside(path));
    }
    crate::agent_ext::assets_request_path(root, "path", &normalized)
}

/// The refusal of an absolute `path` that `project_path_in` let through (one inside the project):
/// the method takes project-relative paths only.
fn with_reason_outside(path: &str) -> RpcError {
    let mut err = RpcError::invalid_params(format!(
        "`path` must be project-relative under assets/, not absolute: {path}"
    ));
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("reason".into(), Value::String("path_outside_project".into()));
    }
    err
}

/// `editor.create_prefab { entities?, path }`.
fn editor_create_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params("`path` (project-relative .prefab.ron under assets/) is required")
        })?;
    let entities = crate::hierarchy_agent::entities_param(world, params)?;
    let root = crate::agent_ext::project_root(world)?;
    let full = confined_prefab_path(&root, path)?;
    match create_prefab(world, &entities, &full) {
        Ok(created) => {
            let rel = created
                .path
                .strip_prefix(&root)
                .unwrap_or(&created.path)
                .to_string_lossy()
                .replace('\\', "/");
            Ok(json!({
                "path": rel,
                "asset": created.asset.to_string(),
                "instance": created.instance.to_bits(),
                "entities": created.entities,
                "replaced": created.replaced.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
                "undo_depth": created.undo_depth,
                "retargeted": created.retargeted,
            }))
        }
        Err(err) => Err(create_prefab_rpc_error(&err)),
    }
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.create_prefab",
            tool: "create_prefab",
            description: "Create a prefab from entities (Outliner ▸ Create prefab from selection…): their \
                subtrees are written to a new .prefab.ron at the current format version and replaced by one \
                linked instance at the same world transforms, which becomes the selection. One top-level \
                entity is the prefab's root; several get a new root at their centroid under their common \
                parent. Component fields that hold an entity and named a member name the instance's entity \
                for it afterwards (retargeted counts them; undo and redo move them back and forth). One undo \
                step (the file stays). Refusals carry data.reason: prefab_create_bad_path, \
                prefab_create_exists, prefab_create_internal, prefab_create_busy, prefab_create_io, \
                prefab_create_scene, a hierarchy_* code, or path_outside_project.",
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
                    required: true,
                    description: "Project-relative path of the new file under assets/ (the prefix is optional), ending in .prefab.ron.",
                },
            ],
            result: "{ path, asset, instance, entities, replaced, undo_depth, retargeted }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_create_prefab as Handler,
    )]
}

mod refs;

#[cfg(test)]
mod tests;
