// SPDX-License-Identifier: MPL-2.0
//! Resolves level-serialized [`MeshSource`] references into renderable [`MeshRenderer`]s
//! (`docs/specs/19-editor.md` § 3, `specs/14` A2).
//!
//! A level stores `MeshSource { mesh, material }`: a glTF/GLB path or a built-in primitive,
//! plus an optional material override (`.material.ron`, `.matgraph.ron`, `.matinst.ron`).
//! [`resolve_mesh_sources`] loads those source assets through a shared [`MeshLibrary`] cache.
//! Each material becomes one hidden part entity ([`MeshPart`], no guid, never saved) that carries
//! the [`MeshRenderer`]. The owner is not a renderer: it keeps [`ResolvedMesh`] and picking maps
//! parts back to it. The viewport draws those parts through the runtime extraction path
//! ([`regia_render::extract_frame`]).
//!
//! Loading is synchronous and cached by path + modification time: an edited material or a
//! re-exported mesh reloads on the next resolve. Failures are recorded on the entity
//! ([`ResolvedMesh::error`]) and logged once; the entity then has no renderer and the viewport
//! draws its proxy, so a broken reference stays visible and selectable.
//!
//! Greybox [`Primitive`]s (Create ▸ Shape) resolve the same way: the edit world runs no render
//! plugin, so [`resolve_mesh_sources`] meshes them through a [`PrimitiveCache`] onto one hidden
//! part entity each ([`ResolvedPrimitive`]). A world whose render plugin already gave the
//! primitive its own `MeshRenderer` (a game world) is left alone.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use regia_asset::{MaterialAssetRon, TextureSlot};
use regia_core::math::{Mat4, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_render::hard_surface::{AuthoredSurfaceMaps, ResolvedBaseMaps};
use regia_render::primitives::PrimitiveMeshMarker;
use regia_render::{
    CpuMaterial, CpuMesh, LabMaterial, LabTexture, LabVertex, MeshRenderer, MeshSource, Primitive,
    PrimitiveCache,
};
use regia_scene::prelude::{GlobalTransform, Transform};

/// One drawable piece of a loaded model: a mesh with the material its triangles use.
#[derive(Debug, Clone)]
pub struct ModelPart {
    /// Geometry in model space (node transforms baked in).
    pub mesh: Arc<CpuMesh>,
    /// The part's material (the model's own, or the source's override).
    pub material: Arc<CpuMaterial>,
    /// Material name for the asset viewers (`glTF material name`, or the override's path).
    pub material_name: String,
}

/// A loaded mesh asset: its parts and model-space bounds.
#[derive(Debug, Clone)]
pub struct LoadedModel {
    /// Parts in material order (at least one).
    pub parts: Vec<ModelPart>,
    /// The meshes of `collision_*` nodes, merged into one part with the default material, when
    /// the model was loaded with [`GltfLoadOptions::keep_collision_nodes`]; empty otherwise.
    /// Renderers draw `parts` only. A tool draws this layer as an overlay on request (ADR-1658).
    /// Not part of the bounds, [`LoadedModel::triangles`] or [`LoadedModel::vertices`].
    pub collision_parts: Vec<ModelPart>,
    /// Model-space AABB minimum corner.
    pub aabb_min: Vec3,
    /// Model-space AABB maximum corner.
    pub aabb_max: Vec3,
}

impl LoadedModel {
    /// Total triangles over every part.
    #[must_use]
    pub fn triangles(&self) -> usize {
        self.parts.iter().map(|part| part.mesh.indices.len() / 3).sum()
    }

    /// Total vertices over every part.
    #[must_use]
    pub fn vertices(&self) -> usize {
        self.parts.iter().map(|part| part.mesh.vertices.len()).sum()
    }

    fn from_parts(parts: Vec<ModelPart>, collision_parts: Vec<ModelPart>) -> Self {
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);
        for part in &parts {
            for vertex in &part.mesh.vertices {
                let p = Vec3::from_array(vertex.position);
                min = min.min(p);
                max = max.max(p);
            }
        }
        if min.x > max.x {
            min = Vec3::ZERO;
            max = Vec3::ZERO;
        }
        Self {
            parts,
            collision_parts,
            aabb_min: min,
            aabb_max: max,
        }
    }
}

/// One cache slot: the file stamp it was loaded at and the outcome.
struct Cached<T> {
    stamp: Option<FileStamp>,
    value: Result<Arc<T>, String>,
}

#[derive(Default)]
struct LibraryInner {
    models: HashMap<PathBuf, Cached<LoadedModel>>,
    /// Models loaded in the cook's part order ([`load_gltf_model_cooked_order`]).
    cooked_order_models: HashMap<PathBuf, Cached<LoadedModel>>,
    materials: HashMap<PathBuf, Cached<CpuMaterial>>,
    builtins: HashMap<String, Arc<LoadedModel>>,
    default_material: Option<Arc<CpuMaterial>>,
}

/// Shared cache of loaded mesh and material assets. Cloning shares the cache (the play world
/// gets the edit world's library, so play start does not reload anything).
#[derive(Resource, Clone, Default)]
pub struct MeshLibrary {
    inner: Arc<Mutex<LibraryInner>>,
}

impl std::fmt::Debug for MeshLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshLibrary").finish_non_exhaustive()
    }
}

/// A file's modification time and length: a rewrite inside one timestamp quantum that changes
/// the size still invalidates the cache.
type FileStamp = (SystemTime, u64);

fn stamp_of(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl MeshLibrary {
    fn lock(&self) -> std::sync::MutexGuard<'_, LibraryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The default material (built-ins and glTF primitives without a material).
    #[must_use]
    pub fn default_material(&self) -> Arc<CpuMaterial> {
        let mut inner = self.lock();
        Arc::clone(
            inner
                .default_material
                .get_or_insert_with(|| Arc::new(LabMaterial::default())),
        )
    }

    /// [`Self::model`] in the cook's part order and with the cooked vertices
    /// ([`load_gltf_model_cooked_order`]): what a lightmap bake keyed its entries by (ADR-2635). A
    /// built-in or a file that is not glTF loads as [`Self::model`] does.
    ///
    /// # Errors
    ///
    /// The file is missing, does not import, or has no LOD 0 part.
    pub fn model_cooked_order(&self, root: Option<&Path>, mesh: &str) -> Result<Arc<LoadedModel>, String> {
        let path = resolve_asset_path(root, mesh);
        let gltf = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("gltf") || e.eq_ignore_ascii_case("glb"));
        if mesh.starts_with("builtin:") || !gltf {
            return self.model(root, mesh);
        }
        let stamp = stamp_of(&path);
        {
            let inner = self.lock();
            if let Some(cached) = inner.cooked_order_models.get(&path)
                && cached.stamp == stamp
            {
                return cached.value.clone();
            }
        }
        let default_material = self.default_material();
        let value = if stamp.is_none() {
            Err(format!("mesh file not found: {}", path.display()))
        } else {
            load_gltf_model_cooked_order(&path, &default_material).map(Arc::new)
        };
        if let Err(err) = &value {
            tracing::warn!(target: "regia_editor", "mesh {mesh} (cooked order): {err}");
        }
        self.lock().cooked_order_models.insert(
            path,
            Cached {
                stamp,
                value: value.clone(),
            },
        );
        value
    }

    /// Loads (or returns the cached) model for a [`MeshSource::mesh`] string, resolved against
    /// the project `root`.
    ///
    /// # Errors
    ///
    /// The file is missing, not a readable glTF, or has no triangles; or an unknown built-in.
    pub fn model(&self, root: Option<&Path>, mesh: &str) -> Result<Arc<LoadedModel>, String> {
        if let Some(name) = mesh.strip_prefix("builtin:") {
            let material = self.default_material();
            let mut inner = self.lock();
            if let Some(model) = inner.builtins.get(name) {
                return Ok(Arc::clone(model));
            }
            let model = Arc::new(builtin_model(name, material)?);
            inner.builtins.insert(name.to_string(), Arc::clone(&model));
            return Ok(model);
        }
        let path = resolve_asset_path(root, mesh);
        let stamp = stamp_of(&path);
        {
            let inner = self.lock();
            if let Some(cached) = inner.models.get(&path)
                && cached.stamp == stamp
            {
                return cached.value.clone();
            }
        }
        let default_material = self.default_material();
        let value = if stamp.is_none() {
            Err(format!("mesh file not found: {}", path.display()))
        } else {
            load_gltf_model(&path, &default_material).map(Arc::new)
        };
        if let Err(err) = &value {
            tracing::warn!(target: "regia_editor", "mesh {mesh}: {err}");
        }
        self.lock().models.insert(
            path,
            Cached {
                stamp,
                value: value.clone(),
            },
        );
        value
    }

    /// Loads (or returns the cached) material for a [`MeshSource::material`] path.
    ///
    /// # Errors
    ///
    /// The file is missing, does not parse, or (graphs) does not compile.
    pub fn material(&self, root: Option<&Path>, material: &str) -> Result<Arc<CpuMaterial>, String> {
        let path = resolve_asset_path(root, material);
        let stamp = stamp_of(&path);
        {
            let inner = self.lock();
            if let Some(cached) = inner.materials.get(&path)
                && cached.stamp == stamp
            {
                return cached.value.clone();
            }
        }
        let value = if stamp.is_none() {
            Err(format!("material file not found: {}", path.display()))
        } else {
            load_material_file(root, &path).map(Arc::new)
        };
        if let Err(err) = &value {
            tracing::warn!(target: "regia_editor", "material {material}: {err}");
        }
        self.lock().materials.insert(
            path,
            Cached {
                stamp,
                value: value.clone(),
            },
        );
        value
    }
}

/// Resolves a project-relative asset path: absolute paths stay, `assets/...` joins the root,
/// anything else is tried under the root and then under `root/assets/`.
///
/// The string is normalized first: a level authored on Windows may name a mesh with `\`
/// separators, which on Linux/macOS is one literal filename component and resolves nowhere
/// (audit G finding 3).
#[must_use]
pub fn resolve_asset_path(root: Option<&Path>, path: &str) -> PathBuf {
    let normalized = path.replace('\\', "/");
    let raw = Path::new(&normalized);
    if raw.is_absolute() {
        return raw.to_path_buf();
    }
    let base = root.map_or_else(PathBuf::new, Path::to_path_buf);
    let direct = base.join(raw);
    if direct.exists() || normalized.starts_with("assets/") {
        return direct;
    }
    let under_assets = base.join("assets").join(raw);
    if under_assets.exists() {
        under_assets
    } else {
        direct
    }
}

/// [`resolve_asset_path`] for a path from an agent request (ADR-1816, ADR-7388): an absolute path must lie
/// under the project or the temp dir; a relative one may not hold `..`, a drive or UNC prefix, and
/// the resolved file (symlinks resolved) must stay in the project. Errors are
/// `path_outside_project`.
pub(crate) fn agent_asset_path(
    world: &World,
    key: &str,
    path: &str,
) -> Result<PathBuf, regia_agent::protocol::RpcError> {
    if Path::new(path).is_absolute() {
        return crate::agent_ext::document_request_path(world, key, path);
    }
    let base = project_root(world).unwrap_or_else(|| crate::agent_ext::agent_root(world));
    regia_agent::req_path::project_path_in(&base, key, path)?;
    let full = resolve_asset_path(Some(&base), path);
    regia_agent::req_path::check_derived(&base, key, &full)?;
    Ok(full)
}

/// Bookkeeping on an entity whose [`MeshSource`] was resolved (editor-side, never saved).
///
/// Sparse-set storage, and the renderers live on part entities: resolving must not move the
/// level entity to another ECS table, because scene snapshots (save, play-in-editor) list
/// entities in table order and a re-save has to stay byte-identical.
#[derive(Component, Debug, Clone)]
#[component(storage = "SparseSet", on_despawn = resolved_mesh_despawned)]
pub struct ResolvedMesh {
    /// The source this resolution was made from (a change triggers a re-resolve).
    pub source: MeshSource,
    /// The model, when it loaded.
    pub model: Option<Arc<LoadedModel>>,
    /// The override material, when one is set and loaded.
    pub material: Option<Arc<CpuMaterial>>,
    /// Hidden part entities drawing the model, one per part (material).
    pub parts: Vec<Entity>,
    /// The load error, if the mesh or the material failed.
    pub error: Option<String>,
}

/// A hidden entity drawing one part of its owner's model (never saved, never selected: it has
/// no guid, name or `Transform`; picking maps it to [`Self::owner`]).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshPart {
    /// The entity carrying the [`MeshSource`].
    pub owner: Entity,
}

/// The resolution of an entity's greybox [`Primitive`] (sparse-set, like [`ResolvedMesh`], so
/// the level entity keeps its ECS table).
#[derive(Component, Debug, Clone)]
#[component(storage = "SparseSet", on_despawn = resolved_primitive_despawned)]
pub struct ResolvedPrimitive {
    /// The primitive this resolution was made from (a change triggers a re-mesh).
    pub primitive: Primitive,
    /// The hidden part entity drawing it (`None` when the parameters are invalid).
    pub part: Option<Entity>,
    /// Model-space AABB (minimum, maximum) of the mesh, when it meshed.
    pub aabb: Option<(Vec3, Vec3)>,
    /// Why the primitive did not mesh.
    pub error: Option<String>,
}

/// A despawned owner takes its parts with it (ADR-8847). The sweep in [`resolve_mesh_sources`]
/// runs only when a viewport renders, and the headless editor renders only for a capture, so an
/// undone Create left its part in the edit world, where `scene.serialize` and the agent queries
/// saw it. The despawn runs after the one in progress and skips a part that is gone already or
/// belongs to another owner now.
fn despawn_parts_of(world: &mut regia_ecs::ecs::world::DeferredWorld<'_>, owner: Entity, parts: Vec<Entity>) {
    if parts.is_empty() {
        return;
    }
    world.commands().queue(move |world: &mut World| {
        for part in parts {
            if world.get::<MeshPart>(part).is_some_and(|p| p.owner == owner) {
                let _ = world.despawn(part);
            }
        }
    });
}

fn resolved_mesh_despawned(
    mut world: regia_ecs::ecs::world::DeferredWorld<'_>,
    context: regia_ecs::ecs::lifecycle::HookContext,
) {
    let parts = world
        .get::<ResolvedMesh>(context.entity)
        .map(|resolved| resolved.parts.clone())
        .unwrap_or_default();
    despawn_parts_of(&mut world, context.entity, parts);
}

fn resolved_primitive_despawned(
    mut world: regia_ecs::ecs::world::DeferredWorld<'_>,
    context: regia_ecs::ecs::lifecycle::HookContext,
) {
    let parts = world
        .get::<ResolvedPrimitive>(context.entity)
        .and_then(|resolved| resolved.part)
        .into_iter()
        .collect();
    despawn_parts_of(&mut world, context.entity, parts);
}

/// Whether `entity` draws through resolved part entities (a [`MeshSource`] or a
/// [`Primitive`]), so it needs no proxy cube.
#[must_use]
pub fn has_resolved_parts(world: &World, entity: Entity) -> bool {
    world
        .get::<ResolvedMesh>(entity)
        .is_some_and(|resolved| !resolved.parts.is_empty())
        || world
            .get::<ResolvedPrimitive>(entity)
            .is_some_and(|resolved| resolved.part.is_some())
}

/// The entity a picked (rendered) entity stands for: a part's owner, else itself.
#[must_use]
pub fn pick_owner(world: &World, entity: Entity) -> Entity {
    world.get::<MeshPart>(entity).map_or(entity, |part| part.owner)
}

/// Registers the level components the viewport renders and the editor authors (cameras,
/// lights, [`MeshSource`], placed decals, audio emitters, saved cloth) on a world's type
/// registry, so scene files, `scene.spawn` and the inspector see them. The edit and play worlds
/// call it; games get the same registrations from their plugins.
pub fn register_level_types(world: &mut World) {
    crate::review_notes::install(world);
    use regia_ecs::reflect::AppTypeRegistry;
    let registry = world
        .get_resource_or_insert_with(AppTypeRegistry::default)
        .clone();
    let mut registry = registry.write();
    registry.register::<MeshSource>();
    registry.register::<Primitive>();
    // Colliders are level content: the collider handles edit them (ADR-2696), with no physics plugin.
    registry.register::<regia::physics::Collider>();
    registry.register::<regia_render::Camera>();
    registry.register::<regia_render::RenderTarget>();
    registry.register::<regia_render::ClearMode>();
    registry.register::<regia_render::DirectionalLight>();
    registry.register::<regia_render::PointLight>();
    registry.register::<regia_render::SpotLight>();
    registry.register::<regia_render::LightSourceSize>();
    // IES profiles and authored area lights (ADR-2592).
    registry.register::<regia_render::LightProfile>();
    registry.register::<regia_render::LightSourceLtc>();
    registry.register::<regia_render::RectLight>();
    registry.register::<regia_render::TubeLight>();
    // A baked level keeps its lightmap receivers (ADR-2313).
    registry.register::<regia_render::lightmap::LightmapReceiver>();
    // Local reflection captures are level content (ADR-2860).
    registry.register::<regia_render::LocalCapture>();
    registry.register::<regia_vfx::StaticDecal>();
    registry.register::<regia_audio::AudioEmitter>();
    registry.register::<regia_framework::cloth::ClothSimSource>();
    registry.register::<regia_framework::cloth::SoftSimSource>();
    registry.register::<regia_framework::cloth::ClothBodyProxy>();
    registry.register::<regia_framework::functional::FunctionalTest>();
    registry.register::<regia_framework::functional::TestBodyRef>();
    registry.register::<regia_framework::functional::Verdict>();
    // Smart objects and camera sources are level content (ADR-2030); their systems stay out of the
    // edit world.
    registry.register::<regia_ai::smart_object::SmartObject>();
    registry.register::<regia_ai::smart_object::SmartObjectSlot>();
    registry.register::<regia_framework::camera::load::CameraAssetSource>();
    // A level can author replicated entities (ADR-2623): without these, opening a level would drop
    // them and saving it would lose them. Their systems stay in `NetPlugin`.
    registry.register::<regia_net::Replicated>();
    registry.register::<regia_net::NetGroups>();
    registry.register::<regia_net::NetAsleep>();
    registry.register::<regia_net::replication::NetPredicted>();
    // Hit shapes are prefab and level content (spec 75 § 3, ADR-5650).
    registry.register::<regia_net::HitShapes>();
    registry.register::<regia_net::NoLagCompensation>();
}

/// The project root the resolver loads from (`EditorProject`, else the working directory).
#[must_use]
pub fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
}

/// The world matrix of `entity`: its `GlobalTransform` when propagated, else its local
/// `Transform` (unparented entities spawned this frame).
#[must_use]
pub fn world_matrix(world: &World, entity: Entity) -> Option<Mat4> {
    if let Some(global) = world.get::<GlobalTransform>(entity) {
        return Some(Mat4::from(global.0));
    }
    world
        .get::<Transform>(entity)
        .map(|transform| Mat4::from(transform.compute_affine()))
}

/// Resolves every [`MeshSource`] in `world` into [`MeshRenderer`]s on hidden part entities
/// (one per model part), removes them when the source is removed, keeps part transforms on
/// their owners (run it after transform propagation), and gives transform-only entities a
/// `GlobalTransform` so the runtime extraction sees them. Returns how many entities were
/// (re)resolved.
#[tracing::instrument(target = "regia_editor", skip_all, name = "resolve_mesh_sources")]
pub fn resolve_mesh_sources(world: &mut World) -> usize {
    let library = world.get_resource_or_insert_with(MeshLibrary::default).clone();
    let root = project_root(world);

    // Transform-only entities (bare test worlds, spawns before propagation) need a world
    // transform for the extraction query.
    let missing_global: Vec<(Entity, GlobalTransform)> = world
        .iter_entities()
        .filter(|entity| entity.get::<GlobalTransform>().is_none())
        .filter_map(|entity| {
            entity
                .get::<Transform>()
                .map(|transform| (entity.id(), GlobalTransform(transform.compute_affine())))
        })
        .collect();
    for (entity, global) in missing_global {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert(global);
        }
    }

    let sources: Vec<(Entity, MeshSource)> = world
        .iter_entities()
        .filter_map(|entity| {
            entity
                .get::<MeshSource>()
                .map(|source| (entity.id(), source.clone()))
        })
        .collect();
    // Entities the installed lightmap has keyed entries for load in the cook's part order, which
    // the bake keyed them by (ADR-2635); every other entity keeps the editor's material grouping.
    let keyed = lightmap_keyed_entities(world);
    let mut changed = 0usize;
    for (entity, source) in &sources {
        let cooked_order = !keyed.is_empty()
            && world
                .get::<regia_scene::prelude::EntityGuid>(*entity)
                .is_some_and(|guid| keyed.contains(&guid.0));
        let model = if cooked_order {
            library.model_cooked_order(root.as_deref(), &source.mesh)
        } else {
            library.model(root.as_deref(), &source.mesh)
        };
        let material = if source.material.is_empty() {
            None
        } else {
            Some(library.material(root.as_deref(), &source.material))
        };
        let model_ok = model.as_ref().ok().cloned();
        let material_ok = material.as_ref().and_then(|m| m.as_ref().ok().cloned());
        let unchanged = world.get::<ResolvedMesh>(*entity).is_some_and(|resolved| {
            resolved.source == *source
                && ptr_eq_opt(&resolved.model, &model_ok)
                && ptr_eq_opt(&resolved.material, &material_ok)
        });
        if unchanged {
            continue;
        }
        changed += 1;
        let error = match (&model, &material) {
            (Err(err), _) | (_, Some(Err(err))) => Some(err.clone()),
            _ => None,
        };
        let old_parts = world
            .get::<ResolvedMesh>(*entity)
            .map(|resolved| resolved.parts.clone())
            .unwrap_or_default();
        // A layer set on the drawn parts survives the re-resolve.
        let layer = old_parts
            .first()
            .and_then(|part| world.get::<MeshRenderer>(*part))
            .map_or(0, |r| r.layer);
        for part in old_parts {
            let _ = world.despawn(part);
        }
        let mut parts = Vec::new();
        let Ok(entry) = world.get_entity(*entity) else {
            continue;
        };
        let global = entry.get::<GlobalTransform>().copied();
        if let Some(loaded) = &model_ok {
            // A failed override falls back to the neutral material (stays visible as grey).
            let override_material = match &material {
                Some(Ok(m)) => Some(Arc::clone(m)),
                Some(Err(_)) => Some(Arc::new(LabMaterial::neutral_fallback())),
                None => None,
            };
            let pick = |part: &ModelPart| {
                override_material
                    .as_ref()
                    .map_or_else(|| Arc::clone(&part.material), Arc::clone)
            };
            for (index, part) in loaded.parts.iter().enumerate() {
                let mut renderer = MeshRenderer::new(Arc::clone(&part.mesh), pick(part));
                renderer.layer = layer;
                let mut spawned = world.spawn((
                    renderer,
                    MeshPart { owner: *entity },
                    // The part's index keys its lightmap entry (ADR-2313).
                    regia_render::lightmap::MeshPartOf {
                        owner: *entity,
                        part: u32::try_from(index).unwrap_or(u32::MAX),
                    },
                    crate::visibility::EditorHidden,
                    regia_render::MaterialLabel(if override_material.is_some() {
                        "material override".to_string()
                    } else {
                        part.material_name.clone()
                    }),
                ));
                if let Some(global) = global {
                    spawned.insert(global);
                }
                parts.push(spawned.id());
            }
        }
        if let Ok(mut entry) = world.get_entity_mut(*entity) {
            entry.insert(ResolvedMesh {
                source: source.clone(),
                model: model_ok,
                material: material_ok,
                parts,
                error,
            });
        }
    }

    // Sources removed since the last resolve: drop the renderer and the parts.
    let stale: Vec<(Entity, Vec<Entity>)> = world
        .iter_entities()
        .filter(|entity| entity.get::<MeshSource>().is_none())
        .filter_map(|entity| {
            entity
                .get::<ResolvedMesh>()
                .map(|resolved| (entity.id(), resolved.parts.clone()))
        })
        .collect();
    for (entity, parts) in stale {
        for part in parts {
            let _ = world.despawn(part);
        }
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.remove::<ResolvedMesh>();
        }
        changed += 1;
    }

    changed += resolve_primitives(world);

    // Parts follow their owner (an owner that was despawned takes its parts with it).
    let parts: Vec<(Entity, Entity)> = world
        .iter_entities()
        .filter_map(|entity| entity.get::<MeshPart>().map(|part| (entity.id(), part.owner)))
        .collect();
    for (part, owner) in parts {
        let global = world
            .get_entity(owner)
            .ok()
            .and_then(|owner| owner.get::<GlobalTransform>().copied());
        match global {
            Some(global) => {
                if let Ok(mut entry) = world.get_entity_mut(part) {
                    entry.insert(global);
                }
            }
            None => {
                let _ = world.despawn(part);
            }
        }
    }
    changed
}

/// Meshes every [`Primitive`] that has no renderer of its own onto a hidden part entity, and
/// drops the parts of primitives that were removed (or got a renderer from a render plugin).
/// Returns how many entities were (re)resolved.
fn resolve_primitives(world: &mut World) -> usize {
    let wanted: Vec<(Entity, Primitive)> = world
        .iter_entities()
        .filter(|entity| {
            entity.get::<MeshSource>().is_none()
                && entity.get::<MeshRenderer>().is_none()
                && entity.get::<PrimitiveMeshMarker>().is_none()
        })
        .filter_map(|entity| entity.get::<Primitive>().map(|p| (entity.id(), p.clone())))
        .collect();
    let mut changed = 0usize;
    if !wanted.is_empty() {
        let mut cache = world
            .remove_resource::<EditorPrimitiveCache>()
            .unwrap_or_default();
        for (entity, primitive) in wanted {
            let resolved = world.get::<ResolvedPrimitive>(entity);
            if resolved.is_some_and(|r| r.primitive == primitive) {
                continue;
            }
            changed += 1;
            let (old_part, layer) = resolved.and_then(|r| r.part).map_or((None, 0), |part| {
                (Some(part), world.get::<MeshRenderer>(part).map_or(0, |r| r.layer))
            });
            if let Some(part) = old_part {
                let _ = world.despawn(part);
            }
            let built = primitive
                .clone()
                .migrate()
                .and_then(|migrated| cache.0.mesh(&migrated.shape).map(|mesh| (migrated, mesh)));
            let global = world.get::<GlobalTransform>(entity).copied();
            let resolution = match built {
                Ok((migrated, mesh)) => {
                    let aabb = mesh_aabb(&mesh);
                    let mut renderer = MeshRenderer::new(mesh, cache.0.material(&migrated));
                    renderer.layer = layer;
                    let mut spawned = world.spawn((
                        renderer,
                        MeshPart { owner: entity },
                        crate::visibility::EditorHidden,
                    ));
                    if let Some(global) = global {
                        spawned.insert(global);
                    }
                    ResolvedPrimitive {
                        primitive,
                        part: Some(spawned.id()),
                        aabb,
                        error: None,
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "primitive on {entity:?} not meshed: {err}");
                    ResolvedPrimitive {
                        primitive,
                        part: None,
                        aabb: None,
                        error: Some(err.to_string()),
                    }
                }
            };
            if let Ok(mut entry) = world.get_entity_mut(entity) {
                entry.insert(resolution);
            }
        }
        world.insert_resource(cache);
    }

    // Primitives removed (or now drawn by a render plugin's own renderer).
    let stale: Vec<(Entity, Option<Entity>)> = world
        .iter_entities()
        .filter(|entity| {
            entity.get::<Primitive>().is_none()
                || entity.get::<MeshSource>().is_some()
                || entity.get::<PrimitiveMeshMarker>().is_some()
        })
        .filter_map(|entity| {
            entity
                .get::<ResolvedPrimitive>()
                .map(|resolved| (entity.id(), resolved.part))
        })
        .collect();
    for (entity, part) in stale {
        if let Some(part) = part {
            let _ = world.despawn(part);
        }
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.remove::<ResolvedPrimitive>();
        }
        changed += 1;
    }
    changed
}

/// The edit world's primitive meshes and materials, shared by equal parameters.
#[derive(Resource, Default)]
struct EditorPrimitiveCache(PrimitiveCache);

fn mesh_aabb(mesh: &CpuMesh) -> Option<(Vec3, Vec3)> {
    let mut points = mesh.vertices.iter().map(|v| Vec3::from(v.position));
    let first = points.next()?;
    Some(points.fold((first, first), |(min, max), p| (min.min(p), max.max(p))))
}

/// The entity guids the installed lightmap ([`regia_render::lightmap::InstalledLightmap`]) has
/// keyed entries for; empty without one.
fn lightmap_keyed_entities(world: &World) -> std::collections::HashSet<regia_core::id::Guid> {
    world
        .get_resource::<regia_render::lightmap::InstalledLightmap>()
        .and_then(|installed| installed.lightmap.as_ref())
        .map(|map| {
            map.entries
                .iter()
                .filter_map(|e| e.key.map(|k| k.entity))
                .collect()
        })
        .unwrap_or_default()
}

fn ptr_eq_opt<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// Model-space bounds of an entity's resolved mesh (`None` without one).
#[must_use]
pub fn model_bounds(world: &World, entity: Entity) -> Option<(Vec3, Vec3)> {
    world
        .get::<ResolvedMesh>(entity)
        .and_then(|resolved| resolved.model.as_ref())
        .map(|model| (model.aabb_min, model.aabb_max))
        .or_else(|| {
            world
                .get::<ResolvedPrimitive>(entity)
                .and_then(|resolved| resolved.aabb)
        })
}

// ---------------------------------------------------------------------------------------------
// Built-in primitives
// ---------------------------------------------------------------------------------------------

/// The built-in primitive models (`builtin:cube`, `builtin:plane`, `builtin:sphere`).
fn builtin_model(name: &str, material: Arc<CpuMaterial>) -> Result<LoadedModel, String> {
    let (vertices, indices) = match name {
        "cube" => regia_render::box_mesh([0.5, 0.5, 0.5]),
        "sphere" => regia_render::uv_sphere(0.5, 32, 16),
        "plane" => plane_mesh(),
        other => {
            return Err(format!(
                "unknown built-in mesh `builtin:{other}` (cube, plane, sphere)"
            ));
        }
    };
    Ok(LoadedModel::from_parts(
        vec![ModelPart {
            mesh: Arc::new(CpuMesh { vertices, indices }),
            material,
            material_name: "default".to_string(),
        }],
        Vec::new(),
    ))
}

/// A 1 m plane on XZ facing +Y, UVs 0..1.
fn plane_mesh() -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [
        ([-0.5, 0.0, -0.5], [0.0, 0.0]),
        ([-0.5, 0.0, 0.5], [0.0, 1.0]),
        ([0.5, 0.0, 0.5], [1.0, 1.0]),
        ([0.5, 0.0, -0.5], [1.0, 0.0]),
    ];
    let vertices = corners
        .iter()
        .map(|(position, uv)| LabVertex {
            position: *position,
            normal: [0.0, 1.0, 0.0],
            uv: *uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

// ---------------------------------------------------------------------------------------------
// glTF
// ---------------------------------------------------------------------------------------------

/// One triangle primitive's vertex streams, normals and tangents generated when absent.
pub(crate) struct PrimitiveData {
    pub(crate) positions: Vec<[f32; 3]>,
    pub(crate) normals: Vec<[f32; 3]>,
    pub(crate) uvs: Vec<[f32; 2]>,
    pub(crate) colors: Vec<[f32; 4]>,
    pub(crate) tangents: Vec<[f32; 4]>,
    pub(crate) indices: Vec<u32>,
    /// `JOINTS_0` (empty unless skin data was asked for and present).
    pub(crate) joints: Vec<[u16; 4]>,
    /// `WEIGHTS_0` (empty unless skin data was asked for and present).
    pub(crate) weights: Vec<[f32; 4]>,
}

/// Whether the accessor's element type matches what `semantic` is read as and its `byteStride`
/// (when set) holds one element. The `gltf` readers hit `unreachable!()` on an unexpected
/// component type and `assert!` on a stride below the element size, and both abort a shipping
/// build, so a primitive that fails this is skipped instead of read.
fn accessor_readable(accessor: &gltf::Accessor<'_>, semantic: Option<&gltf::Semantic>) -> bool {
    use gltf::accessor::{DataType, Dimensions};
    if accessor
        .view()
        .and_then(|v| v.stride())
        .is_some_and(|s| s < accessor.size())
    {
        return false;
    }
    let (ty, dim) = (accessor.data_type(), accessor.dimensions());
    let normalized = |t: DataType| matches!(t, DataType::U8 | DataType::U16 | DataType::F32);
    match semantic {
        Some(gltf::Semantic::Positions | gltf::Semantic::Normals) => {
            ty == DataType::F32 && dim == Dimensions::Vec3
        }
        Some(gltf::Semantic::Tangents) => ty == DataType::F32 && dim == Dimensions::Vec4,
        Some(gltf::Semantic::Colors(0)) => {
            normalized(ty) && matches!(dim, Dimensions::Vec3 | Dimensions::Vec4)
        }
        Some(gltf::Semantic::TexCoords(0)) => normalized(ty) && dim == Dimensions::Vec2,
        Some(gltf::Semantic::Joints(0)) => {
            matches!(ty, DataType::U8 | DataType::U16) && dim == Dimensions::Vec4
        }
        Some(gltf::Semantic::Weights(0)) => normalized(ty) && dim == Dimensions::Vec4,
        // Indices.
        None => matches!(ty, DataType::U8 | DataType::U16 | DataType::U32) && dim == Dimensions::Scalar,
        Some(_) => true,
    }
}

/// Whether every accessor the readers touch (attributes, indices, morph targets) is safe to
/// read; see [`accessor_readable`].
fn primitive_accessors_readable(primitive: &gltf::Primitive<'_>) -> bool {
    use gltf::accessor::{DataType, Dimensions};
    let attributes_ok = primitive
        .attributes()
        .all(|(semantic, accessor)| accessor_readable(&accessor, Some(&semantic)));
    let indices_ok = primitive.indices().is_none_or(|a| accessor_readable(&a, None));
    let morphs_ok = primitive.morph_targets().all(|target| {
        [target.positions(), target.normals(), target.tangents()]
            .into_iter()
            .flatten()
            .all(|a| {
                a.data_type() == DataType::F32
                    && a.dimensions() == Dimensions::Vec3
                    && accessor_readable(&a, Some(&gltf::Semantic::Positions))
            })
    });
    attributes_ok && indices_ok && morphs_ok
}

/// Reads a triangle-list primitive; `None` when it has no positions, mismatched streams or
/// fewer than three indices (the primitive is skipped).
pub(crate) fn read_primitive(
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
    with_skin: bool,
) -> Option<PrimitiveData> {
    if !primitive_accessors_readable(primitive) {
        return None;
    }
    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));
    let positions: Vec<[f32; 3]> = reader.read_positions()?.collect();
    let count = positions.len();
    let mut normals: Vec<[f32; 3]> = reader
        .read_normals()
        .map(Iterator::collect)
        .unwrap_or_else(|| vec![[0.0; 3]; count]);
    let uvs: Vec<[f32; 2]> = reader
        .read_tex_coords(0)
        .map(|uv| uv.into_f32().collect())
        .unwrap_or_else(|| vec![[0.0; 2]; count]);
    let colors: Vec<[f32; 4]> = reader
        .read_colors(0)
        .map(|c| c.into_rgba_f32().collect())
        .unwrap_or_else(|| vec![[1.0; 4]; count]);
    let indices: Vec<u32> = reader
        .read_indices()
        .map(|i| i.into_u32().collect())
        .unwrap_or_else(|| (0..count as u32).collect());
    if normals.len() != count || uvs.len() != count || indices.len() < 3 {
        return None;
    }
    if reader.read_normals().is_none() {
        regia_render::generate_vertex_normals(&positions, &indices, &mut normals);
    }
    // A tangent stream shorter or longer than the positions is treated as absent (the callers
    // index it by vertex).
    let mut tangents: Vec<[f32; 4]> = reader
        .read_tangents()
        .map(Iterator::collect)
        .filter(|t: &Vec<[f32; 4]>| t.len() == count)
        .unwrap_or_default();
    if tangents.is_empty() {
        tangents = vec![[1.0, 0.0, 0.0, 1.0]; count];
        regia_render::generate_vertex_tangents(&positions, &normals, &uvs, &indices, &mut tangents);
    }
    let (joints, weights) = if with_skin {
        let joints: Vec<[u16; 4]> = reader
            .read_joints(0)
            .map(|j| j.into_u16().collect())
            .unwrap_or_default();
        let weights: Vec<[f32; 4]> = reader
            .read_weights(0)
            .map(|w| w.into_f32().collect())
            .unwrap_or_default();
        if joints.len() == count && weights.len() == count {
            (joints, weights)
        } else {
            (Vec::new(), Vec::new())
        }
    } else {
        (Vec::new(), Vec::new())
    };
    Some(PrimitiveData {
        positions,
        normals,
        uvs,
        colors,
        tangents,
        indices,
        joints,
        weights,
    })
}

/// Loads a glTF/GLB: every primitive of the default scene with node transforms baked (meshes of
/// `collision_*` nodes left out), grouped into one part per material; materials through the engine's glTF mapping
/// ([`regia_asset_pipeline::gltf_materials`]) and the runtime material bridge
/// ([`LabMaterial::from_authored_resolved`]).
///
/// # Errors
///
/// Not a readable glTF, or no triangles.
pub fn load_gltf_model(path: &Path, default_material: &Arc<CpuMaterial>) -> Result<LoadedModel, String> {
    load_gltf_model_filtered(path, default_material, &|_| true)
}

/// Options for [`load_gltf_model_with_options`]. The default is the behavior of
/// [`load_gltf_model_filtered`]: `collision_*` nodes are dropped (ADR-1622).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GltfLoadOptions {
    /// Keep the meshes of `collision_*` nodes ([`regia_asset::is_collision_node_name`]) as a
    /// marked layer in [`LoadedModel::collision_parts`] instead of dropping them. They never
    /// join [`LoadedModel::parts`], so a renderer that draws `parts` is unchanged. Default false.
    pub keep_collision_nodes: bool,
}

/// [`load_gltf_model`] keeping only the meshes of nodes whose name passes `keep_mesh_node`
/// (`node_<index>` for an unnamed node). Children of a skipped node are still visited, so a
/// filter on `_LOD<n>` suffixes picks one level of an authored LOD chain. Meshes of
/// `collision_*` nodes ([`regia_asset::is_collision_node_name`]) are never drawn, whatever the
/// filter says (ADR-1622): they are collision hulls, as in the cook.
///
/// # Errors
///
/// Not a readable glTF, or no triangles left after the filter.
pub fn load_gltf_model_filtered(
    path: &Path,
    default_material: &Arc<CpuMaterial>,
    keep_mesh_node: &dyn Fn(&str) -> bool,
) -> Result<LoadedModel, String> {
    load_gltf_model_with_options(path, default_material, keep_mesh_node, GltfLoadOptions::default())
}

/// [`load_gltf_model_filtered`] with [`GltfLoadOptions`]. With `keep_collision_nodes`, the meshes
/// of `collision_*` nodes (node transforms baked, the filter not applied to them) come back
/// merged in [`LoadedModel::collision_parts`] with `default_material`; `parts`, the bounds and
/// the counts stay exactly what they are without the option (ADR-1658). A tool draws
/// `collision_parts` as an overlay when the user asks for it.
///
/// # Errors
///
/// Not a readable glTF, or no triangles left in `parts` after the filter (a model with only
/// collision hulls is an error even with the option on).
pub fn load_gltf_model_with_options(
    path: &Path,
    default_material: &Arc<CpuMaterial>,
    keep_mesh_node: &dyn Fn(&str) -> bool,
    options: GltfLoadOptions,
) -> Result<LoadedModel, String> {
    let regia_asset_pipeline::GltfOpened {
        document,
        buffers,
        images,
        load_data,
    } = regia_asset_pipeline::open_gltf(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let authored = regia_asset_pipeline::gltf_materials(path).map_err(|err| err.to_string())?;

    // Gather triangles per material index (None = no material).
    let mut groups: BTreeMap<Option<usize>, (Vec<LabVertex>, Vec<u32>)> = BTreeMap::new();
    let mut collision: (Vec<LabVertex>, Vec<u32>) = (Vec::new(), Vec::new());
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .ok_or_else(|| format!("{}: no scene", path.display()))?;
    let mut stack: Vec<(gltf::Node<'_>, Mat4)> = scene.nodes().map(|node| (node, Mat4::IDENTITY)).collect();
    while let Some((node, parent)) = stack.pop() {
        let local = Mat4::from_cols_array_2d(&node.transform().matrix());
        let world = parent * local;
        let is_collision = node.name().is_some_and(regia_asset::is_collision_node_name);
        let kept = match node.name() {
            Some(_) if is_collision => options.keep_collision_nodes,
            Some(name) => keep_mesh_node(name),
            None => keep_mesh_node(&format!("node_{}", node.index())),
        };
        if let Some(mesh) = node.mesh().filter(|_| kept) {
            // `EXT_mesh_gpu_instancing`: this path merges a model into one mesh per material, so each
            // instance is baked like a node of its own (ADR-3016).
            let (copies, dropped) = load_data.baked_worlds(node.index(), world);
            if dropped > 0 {
                tracing::warn!(
                    target: "regia_editor",
                    node = node.index(),
                    dropped,
                    "glTF node has more instances than the merged model bakes"
                );
            }
            for primitive in mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let Some(PrimitiveData {
                    positions,
                    normals,
                    uvs,
                    colors,
                    tangents,
                    indices,
                    ..
                }) = read_primitive(&primitive, &buffers, false)
                else {
                    continue;
                };
                let count = positions.len();
                for world in &copies {
                    let world = *world;
                    // Bake the node transform; a mirroring transform flips the winding back.
                    let normal_matrix = world.inverse().transpose();
                    let flip = world.determinant() < 0.0;
                    let (vertices, out_indices) = if is_collision {
                        &mut collision
                    } else {
                        groups.entry(primitive.material().index()).or_default()
                    };
                    let base = vertices.len() as u32;
                    for i in 0..count {
                        let p = world.transform_point3(Vec3::from_array(positions[i]));
                        let n = normal_matrix
                            .transform_vector3(Vec3::from_array(normals[i]))
                            .normalize_or_zero();
                        let t = world
                            .transform_vector3(Vec3::new(tangents[i][0], tangents[i][1], tangents[i][2]))
                            .normalize_or_zero();
                        vertices.push(LabVertex {
                            position: p.to_array(),
                            normal: n.to_array(),
                            uv: uvs[i],
                            tangent: [t.x, t.y, t.z, tangents[i][3]],
                            color: colors.get(i).copied().unwrap_or([1.0; 4]),
                            joints: [0; 4],
                            weights: [0.0; 4],
                        });
                    }
                    for tri in indices.as_chunks::<3>().0 {
                        if tri.iter().any(|&i| i as usize >= count) {
                            continue;
                        }
                        if flip {
                            out_indices.extend_from_slice(&[base + tri[0], base + tri[2], base + tri[1]]);
                        } else {
                            out_indices.extend_from_slice(&[base + tri[0], base + tri[1], base + tri[2]]);
                        }
                    }
                }
            }
        }
        for child in node.children() {
            stack.push((child, world));
        }
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mesh")
        .to_string();
    let mut parts = Vec::new();
    for (material_index, (vertices, indices)) in groups {
        if indices.is_empty() {
            continue;
        }
        let (material, material_name) = match material_index.and_then(|i| authored.get(i).map(|m| (i, m))) {
            Some((index, asset)) => {
                let gltf_name = document
                    .materials()
                    .nth(index)
                    .and_then(|m| m.name().map(str::to_string))
                    .unwrap_or_else(|| format!("material {index}"));
                let material =
                    gltf_lab_material(&format!("{name}#{gltf_name}"), asset, &document, index, &images);
                (Arc::new(material), gltf_name)
            }
            None => (Arc::clone(default_material), "default".to_string()),
        };
        parts.push(ModelPart {
            mesh: Arc::new(CpuMesh { vertices, indices }),
            material,
            material_name,
        });
    }
    if parts.is_empty() {
        return Err(format!("{}: no triangles in the default scene", path.display()));
    }
    let mut collision_parts = Vec::new();
    if !collision.1.is_empty() {
        collision_parts.push(ModelPart {
            mesh: Arc::new(CpuMesh {
                vertices: collision.0,
                indices: collision.1,
            }),
            material: Arc::clone(default_material),
            material_name: "collision".to_string(),
        });
    }
    Ok(LoadedModel::from_parts(parts, collision_parts))
}

/// Loads a glTF/GLB the way the cook and `regia bake-lightmaps` see it (ADR-2635): the asset
/// pipeline's import (no axis conversion, the bake's arguments), one part per cooked model part in
/// the cook's order, LOD 0 and non-LOD nodes only, each with the cooked mesh's vertices (welded and
/// reordered by the cook) and LOD 0's triangles. A lightmap entry keyed `(entity, part)` was baked
/// from exactly this part, so its unwrap's vertex references hold here; [`load_gltf_model`]
/// groups primitives by material instead, which the bake's keys do not follow. Materials map
/// through the same runtime bridge as [`load_gltf_model`].
///
/// Slower than [`load_gltf_model`] (the import also cooks the textures, at low quality): the
/// editor uses it only for entities the installed lightmap has keyed entries for.
///
/// # Errors
///
/// Not a readable glTF, an import error, or no LOD 0 part.
pub fn load_gltf_model_cooked_order(
    path: &Path,
    default_material: &Arc<CpuMaterial>,
) -> Result<LoadedModel, String> {
    let _span = tracing::info_span!(target: "regia_editor", "load_gltf_model_cooked_order").entered();
    let import = regia_asset_pipeline::import_gltf_path(
        path,
        regia_asset::AssetId::new_random(),
        regia_asset_pipeline::Forward::Keep,
        regia_asset_pipeline::host_platform(),
        None,
        regia_asset_pipeline::TextureCookQuality::Low,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let (document, _buffers, images) = regia_asset_pipeline::import_gltf_checked(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let authored = regia_asset_pipeline::gltf_materials(path).map_err(|err| err.to_string())?;
    let mut meshes: HashMap<regia_asset::AssetId, regia_asset::CookedMesh> = HashMap::new();
    // The cook writes one material sub-asset per glTF material, in the document's order.
    let mut material_ids: Vec<regia_asset::AssetId> = Vec::new();
    let mut model: Option<regia_asset::CookedModel> = None;
    for sub in &import.sub_assets {
        match sub.type_path.as_str() {
            "regia_asset::CookedMesh" => {
                let mesh = regia_asset::CookedMesh::from_postcard(&sub.payload)
                    .map_err(|e| format!("{}: {e}", sub.path))?;
                meshes.insert(sub.id, mesh);
            }
            "regia_asset::CookedStandardMaterial" => material_ids.push(sub.id),
            "regia_asset::CookedModel" => {
                model = Some(
                    regia_asset::CookedModel::from_postcard(&sub.payload)
                        .map_err(|e| format!("{}: {e}", sub.path))?,
                );
            }
            _ => {}
        }
    }
    let model = model.ok_or_else(|| format!("{}: the import has no model", path.display()))?;
    let by_order = material_ids.len() == document.materials().len();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mesh")
        .to_string();
    let mut materials: HashMap<usize, (Arc<CpuMaterial>, String)> = HashMap::new();
    let mut parts = Vec::new();
    for part in &model.parts {
        // The bake's rule (`regia bake-gi`'s `gltf_bake_source`): authored LODs above 0 are not
        // parts, and a part whose mesh did not cook is skipped.
        if regia_asset_pipeline::parse_lod_name(&part.node).is_some_and(|(_, level)| level > 0) {
            continue;
        }
        let Some(mesh) = meshes.get(&part.mesh) else {
            continue;
        };
        let cpu =
            regia_render::visbuffer::cooked_mesh_lod0(mesh).map_err(|e| format!("{}: {e}", part.node))?;
        // The cooked stream keeps only the bitangent sign, so `cpu.tangents` are placeholders: generate
        // them as the runtime's cooked models do, or a normal map shades wrongly on this part.
        let n = cpu.positions.len();
        let normals: Vec<[f32; 3]> = (0..n)
            .map(|i| cpu.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]))
            .collect();
        let uvs: Vec<[f32; 2]> = (0..n)
            .map(|i| cpu.uvs.get(i).copied().unwrap_or([0.0, 0.0]))
            .collect();
        let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; n];
        regia_render::generate_vertex_tangents(&cpu.positions, &normals, &uvs, &cpu.indices, &mut tangents);
        let vertices = (0..n)
            .map(|i| LabVertex {
                position: cpu.positions[i],
                normal: normals[i],
                uv: uvs[i],
                tangent: tangents[i],
                color: cpu.colors.get(i).copied().unwrap_or([1.0; 4]),
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect();
        let index = part
            .material
            .filter(|_| by_order)
            .and_then(|id| material_ids.iter().position(|m| *m == id));
        let (material, material_name) = match index.and_then(|i| authored.get(i).map(|a| (i, a))) {
            Some((i, asset)) => materials
                .entry(i)
                .or_insert_with(|| {
                    let gltf_name = document
                        .materials()
                        .nth(i)
                        .and_then(|m| m.name().map(str::to_string))
                        .unwrap_or_else(|| format!("material {i}"));
                    let material =
                        gltf_lab_material(&format!("{name}#{gltf_name}"), asset, &document, i, &images);
                    (Arc::new(material), gltf_name)
                })
                .clone(),
            None => (Arc::clone(default_material), "default".to_string()),
        };
        parts.push(ModelPart {
            mesh: Arc::new(CpuMesh {
                vertices,
                indices: cpu.indices,
            }),
            material,
            material_name,
        });
    }
    if parts.is_empty() {
        return Err(format!("{}: no LOD 0 part in the cooked model", path.display()));
    }
    Ok(LoadedModel::from_parts(parts, Vec::new()))
}

/// One glTF material through the runtime bridge, with its images resolved from the document.
pub(crate) fn gltf_lab_material(
    name: &str,
    asset: &MaterialAssetRon,
    document: &gltf::Document,
    index: usize,
    images: &[gltf::image::Data],
) -> LabMaterial {
    let image = |slot: &Option<TextureSlot>| -> Option<LabTexture> {
        let Some(TextureSlot::Path(path)) = slot else {
            return None;
        };
        let index: usize = path.strip_prefix("image/")?.parse().ok()?;
        images.get(index).and_then(gltf_image_rgba)
    };
    let mut asset = asset.clone();
    // ORM: glTF metallic-roughness carries G roughness, B metallic; occlusion (R) may be a
    // separate image. Pack the two the way the cook does (`docs/formats/gltf-conventions.md`).
    let gltf_material = document.materials().nth(index);
    let mr_index = gltf_material.as_ref().and_then(|m| {
        m.pbr_metallic_roughness()
            .metallic_roughness_texture()
            .map(|t| t.texture().source().index())
    });
    let ao_index = gltf_material
        .as_ref()
        .and_then(|m| m.occlusion_texture().map(|t| t.texture().source().index()));
    let occlusion = match (mr_index, ao_index) {
        (Some(mr), ao) => images.get(mr).and_then(gltf_image_rgba).map(|mut packed| {
            let ao_image = ao
                .filter(|ao| *ao != mr)
                .and_then(|ao| images.get(ao))
                .and_then(gltf_image_rgba);
            for (i, px) in packed.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                px[0] = match &ao_image {
                    Some(ao) if ao.width == packed.width && ao.height == packed.height => ao.rgba[i * 4],
                    _ if ao == Some(mr) => px[0],
                    _ => 255,
                };
                px[3] = 255;
            }
            asset.base.orm_packed = true;
            packed
        }),
        (None, Some(ao)) => {
            asset.base.orm_packed = false;
            images.get(ao).and_then(gltf_image_rgba)
        }
        (None, None) => None,
    };
    let base = ResolvedBaseMaps {
        albedo: image(&asset.base.base_color_map),
        normal: image(&asset.base.normal_map),
        occlusion,
        emissive: image(&asset.base.emissive_map),
    };
    let film = asset.thin_film.as_ref();
    let maps = AuthoredSurfaceMaps {
        thin_film_coverage: film.and_then(|f| image(&f.factor_map)),
        thin_film_thickness: film.and_then(|f| image(&f.thickness_map)),
        ..AuthoredSurfaceMaps::default()
    };
    LabMaterial::from_authored_resolved(name, &asset, base, maps)
}

/// A decoded glTF image as RGBA8 (8-bit formats only; 16/32-bit images are skipped).
pub(crate) fn gltf_image_rgba(data: &gltf::image::Data) -> Option<LabTexture> {
    use gltf::image::Format;
    let (width, height) = (data.width, data.height);
    let pixels = (width as usize) * (height as usize);
    let src = &data.pixels;
    let rgba: Vec<u8> = match data.format {
        Format::R8G8B8A8 => src.clone(),
        Format::R8G8B8 => src
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        Format::R8G8 => src
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], 0, 255])
            .collect(),
        Format::R8 => src.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        _ => return None,
    };
    (rgba.len() == pixels * 4).then_some(LabTexture { width, height, rgba })
}

// ---------------------------------------------------------------------------------------------
// Material files
// ---------------------------------------------------------------------------------------------

/// Decodes an image file to RGBA8.
///
/// # Errors
///
/// The file cannot be read or decoded.
pub fn load_texture_file(path: &Path) -> Result<LabTexture, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let decoded = image::load_from_memory(&bytes)
        .map_err(|err| format!("{}: {err}", path.display()))?
        .to_rgba8();
    Ok(LabTexture {
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}

/// A `.material.ron` texture slot: `Path` under `assets/` (or project-relative); asset ids
/// need the cooked manifest and are left unresolved here.
fn slot_texture(root: Option<&Path>, slot: &Option<TextureSlot>) -> Option<LabTexture> {
    let Some(TextureSlot::Path(path)) = slot else {
        return None;
    };
    load_texture_file(&resolve_asset_path(root, path))
        .inspect_err(|err| tracing::warn!(target: "regia_editor", "texture {path}: {err}"))
        .ok()
}

/// Loads a material file by extension: `.material.ron` (standard PBR), `.matgraph.ron`
/// (a material graph) or `.matinst.ron` (an instance of a graph).
///
/// # Errors
///
/// Unknown extension, unreadable/invalid file, or a graph that does not compile.
pub fn load_material_file(root: Option<&Path>, path: &Path) -> Result<CpuMaterial, String> {
    let name = path.to_string_lossy().to_string();
    // Dispatch on the lowered name: a file saved as `rock.MATERIAL.RON` opens fine on a
    // case-insensitive filesystem and is still a material (audit G finding 7).
    let lower = name.to_ascii_lowercase();
    let bytes = std::fs::read(path).map_err(|err| format!("{name}: {err}"))?;
    if lower.ends_with(".material.ron") {
        let asset = MaterialAssetRon::from_ron(&bytes).map_err(|err| format!("{name}: {err}"))?;
        let base = ResolvedBaseMaps {
            albedo: slot_texture(root, &asset.base.base_color_map),
            normal: slot_texture(root, &asset.base.normal_map),
            occlusion: slot_texture(root, &asset.base.orm_map),
            emissive: slot_texture(root, &asset.base.emissive_map),
        };
        let film = asset.thin_film.as_ref();
        let maps = AuthoredSurfaceMaps {
            thin_film_coverage: film.and_then(|f| slot_texture(root, &f.factor_map)),
            thin_film_thickness: film.and_then(|f| slot_texture(root, &f.thickness_map)),
            ..AuthoredSurfaceMaps::default()
        };
        return Ok(LabMaterial::from_authored_resolved(&name, &asset, base, maps));
    }
    let text = String::from_utf8(bytes).map_err(|err| format!("{name}: {err}"))?;
    if lower.ends_with(".matgraph.ron") {
        let graph = regia_matgraph::MaterialGraph::from_ron(&text).map_err(|err| format!("{name}: {err}"))?;
        return graph_material(root, path, &graph, None);
    }
    if lower.ends_with(".matinst.ron") {
        let instance =
            regia_matgraph::MaterialInstance::from_ron(&text).map_err(|err| format!("{name}: {err}"))?;
        let parent_path = resolve_relative(root, path, &instance.parent);
        let parent_text = std::fs::read_to_string(&parent_path)
            .map_err(|err| format!("{}: {err}", parent_path.display()))?;
        let graph = regia_matgraph::MaterialGraph::from_ron(&parent_text)
            .map_err(|err| format!("{}: {err}", parent_path.display()))?;
        return graph_material(root, path, &graph, Some(&instance));
    }
    Err(format!(
        "{name}: not a material (.material.ron, .matgraph.ron, .matinst.ron)"
    ))
}

/// A path referenced from inside an asset: relative to the referencing file first, then the
/// project.
fn resolve_relative(root: Option<&Path>, from: &Path, path: &str) -> PathBuf {
    let sibling = from.parent().map(|dir| dir.join(path));
    match sibling {
        Some(sibling) if sibling.exists() => sibling,
        _ => resolve_asset_path(root, path),
    }
}

/// Compiles a graph (or an instance of it) into a material with its textures loaded.
fn graph_material(
    root: Option<&Path>,
    path: &Path,
    graph: &regia_matgraph::MaterialGraph,
    instance: Option<&regia_matgraph::MaterialInstance>,
) -> Result<CpuMaterial, String> {
    let name = path.display();
    let no_switches = BTreeMap::new();
    let switches = instance.map_or(&no_switches, |i| &i.switches);
    let compiled = regia_matgraph::compile_with(graph, switches).map_err(|err| format!("{name}: {err}"))?;
    let mut images = BTreeMap::new();
    for decl in &graph.textures {
        let source = instance
            .and_then(|i| i.textures.get(&decl.name))
            .unwrap_or(&decl.path);
        let texture = load_texture_file(&resolve_relative(root, path, source))
            .map_err(|err| format!("{name}: texture `{}`: {err}", decl.name))?;
        images.insert(decl.name.clone(), texture);
    }
    match instance {
        Some(instance) => regia_matgraph::instance_material(graph, &compiled, instance, &images, 0.0),
        None => regia_matgraph::lab_material(
            graph,
            &compiled,
            regia_matgraph::default_params(graph, 0.0),
            &images,
        ),
    }
    .map_err(|err| format!("{name}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sec-review-assets-agent (ADR-5869): the editor's model loader parsed with `gltf::import`
    /// inside `catch_unwind`, which reads buffer URIs from any absolute path and cannot stop the
    /// loader's own node walk, which looped (and grew its vertex list for ever) on a node listed
    /// twice under itself. It now uses the importer's checked open.
    #[test]
    fn a_hostile_gltf_is_refused_by_the_model_loader() {
        let dir = regia_test_utils::unique_temp_dir("editor-hostile-gltf");
        let secret = dir.join("secret.bin");
        let mut bin = Vec::new();
        for v in [0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&secret, &bin).expect("secret");
        let gltf = |uri: &str, node: &str| {
            format!(
                r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{node}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
"buffers":[{{"uri":"{uri}","byteLength":36}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}}]}}"#
            )
        };
        let material = Arc::new(regia_render::LabMaterial::default());
        let absolute = secret.to_string_lossy().replace('\\', "/");
        let absolute = if absolute.starts_with('/') {
            absolute
        } else {
            format!("/{absolute}")
        };
        let path = dir.join("abs.gltf");
        std::fs::write(&path, gltf(&absolute, r#"{"mesh":0}"#)).expect("gltf");
        let err = load_gltf_model(&path, &material).expect_err("an absolute buffer URI is refused");
        assert!(
            err.contains("absolute") || err.contains("not a relative path"),
            "{err}"
        );
        let path = dir.join("cycle.gltf");
        std::fs::write(&path, gltf("secret.bin", r#"{"mesh":0,"children":[0,0]}"#)).expect("gltf");
        let err = load_gltf_model(&path, &material).expect_err("a node cycle is refused");
        assert!(err.contains("own child"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_cloth_is_a_level_type() {
        let mut world = World::new();
        register_level_types(&mut world);
        let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().read();
        for path in [
            "regia_framework::cloth::ClothSimSource",
            "regia_framework::cloth::SoftSimSource",
            "regia_framework::cloth::ClothBodyProxy",
            "regia_framework::cloth::ProxyShape",
        ] {
            assert!(registry.get_with_type_path(path).is_some(), "{path}");
        }
    }

    /// Spec 75 § 3 (ADR-5650): hit shapes are authored in the editor and saved in levels.
    #[test]
    fn hit_shapes_are_level_types() {
        let mut world = World::new();
        register_level_types(&mut world);
        let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().read();
        assert!(
            registry
                .get(std::any::TypeId::of::<regia_net::HitShapes>())
                .is_some()
        );
        assert!(
            registry
                .get(std::any::TypeId::of::<regia_net::NoLagCompensation>())
                .is_some()
        );
    }

    #[test]
    fn builtins_load_once_and_share_their_arc() {
        let library = MeshLibrary::default();
        let a = library.model(None, MeshSource::BUILTIN_CUBE).expect("cube");
        let b = library.model(None, MeshSource::BUILTIN_CUBE).expect("cube again");
        assert!(Arc::ptr_eq(&a, &b), "cached");
        assert_eq!(a.triangles(), 12);
        assert_eq!(a.aabb_min, Vec3::splat(-0.5));
        assert_eq!(a.aabb_max, Vec3::splat(0.5));
        let plane = library.model(None, MeshSource::BUILTIN_PLANE).expect("plane");
        assert_eq!(plane.triangles(), 2);
        assert!(library.model(None, "builtin:teapot").is_err());
    }

    #[test]
    fn resolve_attaches_renderers_and_drops_them_with_the_source() {
        let mut world = World::new();
        let entity = world
            .spawn((
                Transform::default(),
                MeshSource::new(MeshSource::BUILTIN_SPHERE, ""),
            ))
            .id();
        assert_eq!(resolve_mesh_sources(&mut world), 1);
        let parts = world.get::<ResolvedMesh>(entity).expect("resolved").parts.clone();
        assert_eq!(parts.len(), 1, "one part per material");
        assert!(world.get::<MeshRenderer>(parts[0]).is_some());
        assert_eq!(pick_owner(&world, parts[0]), entity);
        // The part's index keys its lightmap entry (ADR-2313).
        assert_eq!(
            world.get::<regia_render::lightmap::MeshPartOf>(parts[0]).copied(),
            Some(regia_render::lightmap::MeshPartOf {
                owner: entity,
                part: 0
            })
        );
        assert!(
            world.get::<MeshRenderer>(entity).is_none(),
            "the level entity keeps its ECS table (snapshot order)"
        );
        assert!(
            world.get::<GlobalTransform>(parts[0]).is_some(),
            "extraction needs it"
        );
        // Second pass: nothing changed.
        assert_eq!(resolve_mesh_sources(&mut world), 0);
        world.entity_mut(entity).remove::<MeshSource>();
        assert_eq!(resolve_mesh_sources(&mut world), 1);
        assert!(world.get_entity(parts[0]).is_err(), "parts go with the source");
        assert!(world.get::<ResolvedMesh>(entity).is_none());
    }

    /// Lane v07-editor-smoke-residue (ADR-8847, plan E-08 item 2): parts were swept only by the
    /// next resolve, which runs when a viewport renders. The headless editor renders only for a
    /// screenshot, so undoing a Create left the shape's hidden part in the edit world, and
    /// `scene.serialize` listed it (the smoke's Create/Shape/Cuboid undo "did not restore the
    /// level", and the next Create without a primitive, Player start, swept it: the fingerprint
    /// pair 51b7e0d6 / 476bcd01). A despawned owner now takes its parts with it at once.
    #[test]
    fn a_despawned_owner_takes_its_parts_without_another_resolve() {
        let mut world = World::new();
        let cuboid = regia_render::primitives::PrimitiveShape::default_of("cuboid").expect("cuboid");
        let shape = world
            .spawn((
                Transform::default(),
                GlobalTransform::default(),
                Primitive::new(cuboid),
            ))
            .id();
        let model = world
            .spawn((
                Transform::default(),
                MeshSource::new(MeshSource::BUILTIN_SPHERE, ""),
            ))
            .id();
        resolve_mesh_sources(&mut world);
        let mut parts = world.query::<&MeshPart>();
        assert_eq!(parts.iter(&world).count(), 2, "one part each");
        world.despawn(shape);
        world.despawn(model);
        // No resolve in between: the headless editor's frames after an undo, before a render.
        assert_eq!(
            parts.iter(&world).count(),
            0,
            "the despawned owners took their parts"
        );
        // A part someone else despawned first is no error, and the next resolve has nothing to do.
        assert_eq!(resolve_mesh_sources(&mut world), 0);
    }

    #[test]
    fn a_missing_mesh_records_an_error_and_no_renderer() {
        let mut world = World::new();
        let entity = world
            .spawn((Transform::default(), MeshSource::new("assets/nope.gltf", "")))
            .id();
        resolve_mesh_sources(&mut world);
        let resolved = world.get::<ResolvedMesh>(entity).expect("bookkeeping");
        assert!(resolved.parts.is_empty(), "nothing to draw");
        assert!(resolved.error.as_deref().is_some_and(|e| e.contains("not found")));
    }

    #[test]
    fn a_material_ron_override_is_loaded_and_follows_edits() {
        let dir = std::env::temp_dir().join(format!("regia-mesh-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let mut asset = MaterialAssetRon::default_standard();
        asset.base.base_color = [0.9, 0.1, 0.1, 1.0];
        std::fs::write(
            dir.join("assets/red.material.ron"),
            asset.to_ron_string().expect("ron"),
        )
        .expect("write");
        let library = MeshLibrary::default();
        let red = library
            .material(Some(&dir), "assets/red.material.ron")
            .expect("material");
        assert!((red.base_color[0] - 0.9).abs() < 1e-6);
        let again = library
            .material(Some(&dir), "assets/red.material.ron")
            .expect("material");
        assert!(Arc::ptr_eq(&red, &again), "cached until the file changes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit G finding 3: a level authored on Windows names a mesh with `\` separators. On Linux
    /// the raw string is one literal filename component and every probe misses; normalized once,
    /// it resolves under `assets/` on every host.
    #[test]
    fn windows_separators_resolve_under_assets() {
        let dir = regia_test_utils::unique_temp_dir("mesh_source_windows_separators");
        std::fs::create_dir_all(dir.join("assets/models")).expect("dir");
        regia_test_utils::write_atomic(&dir.join("assets/models/rock.gltf"), b"glTF").expect("write");
        let resolved = resolve_asset_path(Some(&dir), "assets\\models\\rock.gltf");
        assert_eq!(
            resolved,
            dir.join("assets/models/rock.gltf"),
            "a Windows-authored path resolves to the same file on every host"
        );
        let direct = resolve_asset_path(Some(&dir), "models\\rock.gltf");
        assert_eq!(
            direct,
            dir.join("assets/models/rock.gltf"),
            "a bare relative path with Windows separators tries assets/ too"
        );
    }

    /// Audit G finding 7: a material saved with an uppercase extension (external tool or rename)
    /// opens fine on a case-insensitive filesystem and must dispatch on the lowered name.
    #[test]
    fn load_material_file_accepts_uppercase_extension() {
        let dir = regia_test_utils::unique_temp_dir("mesh_source_uppercase_ext");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let mut asset = MaterialAssetRon::default_standard();
        asset.base.base_color = [0.2, 0.4, 0.6, 1.0];
        regia_test_utils::write_atomic(
            &dir.join("assets/brick.MATERIAL.RON"),
            asset.to_ron_string().expect("ron").as_bytes(),
        )
        .expect("write");
        let material = load_material_file(Some(&dir), &dir.join("assets/brick.MATERIAL.RON"))
            .expect("an uppercase extension is still a material");
        assert!(
            (material.base_color[1] - 0.4).abs() < 1e-6,
            "parsed, not the not-a-material error"
        );
    }

    #[test]
    fn file_stamp_changes_when_the_length_changes() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-stamp-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("m.gltf");
        assert_eq!(stamp_of(&path), None, "a missing file has no stamp");
        std::fs::write(&path, "{}").expect("write");
        let before = stamp_of(&path).expect("stamp");
        std::fs::write(&path, "{ }").expect("rewrite");
        let after = stamp_of(&path).expect("stamp");
        assert_ne!(before, after, "a longer rewrite never compares equal");
        assert_eq!(after.1, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parses a one-triangle glTF whose accessor/view JSON is supplied by the caller, over a
    /// zeroed 256-byte buffer, and runs `read_primitive` on it.
    fn read_crafted(attributes: &str, views: &str, accessors: &str) -> Option<PrimitiveData> {
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":256}}],
            "bufferViews":[{views}],"accessors":[{accessors}],
            "meshes":[{{"primitives":[{{"attributes":{{{attributes}}}}}]}}]}}"#
        );
        let gltf = gltf::Gltf::from_slice(json.as_bytes()).expect("crafted glTF parses");
        let buffers = vec![gltf::buffer::Data(vec![0u8; 256])];
        let mesh = gltf.document.meshes().next().expect("mesh");
        let primitive = mesh.primitives().next().expect("primitive");
        read_primitive(&primitive, &buffers, true)
    }

    const POS: &str =
        r#"{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,1]}"#;
    const VIEW: &str = r#"{"buffer":0,"byteLength":128}"#;

    #[test]
    fn a_well_formed_primitive_still_reads() {
        let p = read_crafted(r#""POSITION":0"#, VIEW, POS).expect("reads");
        assert_eq!((p.positions.len(), p.tangents.len()), (3, 3));
    }

    #[test]
    fn a_short_tangent_stream_is_regenerated_not_indexed() {
        let tan = r#"{"bufferView":0,"componentType":5126,"count":1,"type":"VEC4"}"#;
        let p = read_crafted(r#""POSITION":0,"TANGENT":1"#, VIEW, &format!("{POS},{tan}")).expect("reads");
        assert_eq!(p.tangents.len(), p.positions.len());
    }

    #[test]
    fn unsupported_component_types_skip_the_primitive() {
        // COLOR_0 as FLOAT SCALAR, TEXCOORD_0 as BYTE VEC2: the gltf readers would abort.
        for bad in [
            r#"{"bufferView":0,"componentType":5126,"count":3,"type":"SCALAR"}"#,
            r#"{"bufferView":0,"componentType":5120,"count":3,"type":"VEC2"}"#,
        ] {
            let attrs = if bad.contains("5120") {
                r#""POSITION":0,"TEXCOORD_0":1"#
            } else {
                r#""POSITION":0,"COLOR_0":1"#
            };
            assert!(
                read_crafted(attrs, VIEW, &format!("{POS},{bad}")).is_none(),
                "{bad}"
            );
        }
        // JOINTS_0 as FLOAT on the skinned path.
        let joints = r#"{"bufferView":0,"componentType":5126,"count":3,"type":"VEC4"}"#;
        assert!(read_crafted(r#""POSITION":0,"JOINTS_0":1"#, VIEW, &format!("{POS},{joints}")).is_none());
    }

    #[test]
    fn a_stride_below_the_element_size_skips_the_primitive() {
        let views = r#"{"buffer":0,"byteLength":128,"byteStride":4}"#;
        assert!(read_crafted(r#""POSITION":0"#, views, POS).is_none());
    }
}
