// SPDX-License-Identifier: MPL-2.0
//! Material graphs at run time: a [`MaterialGraphRef`] component names a cooked
//! `.matgraph.ron` or `.matinst.ron` asset; [`apply_material_graphs`] loads it through the
//! [`AssetServer`] (paks first, then the dev cache) and sets the entity's
//! [`MeshRenderer::material`], so the frame extraction carries it to the renderer as
//! [`LabMaterial::surface_graph`]. A hot reload of the cooked graph or one of its images (the
//! dev cooked watcher) rebuilds the material and swaps it on every entity that uses it.
//!
//! New graphs are prewarmed on the renderer's workers as soon as they load
//! ([`prewarm_graph_pipelines`]), and a level load waits for them, so a loaded level draws its
//! graphs from the first frame without building a pipeline on the frame thread. Loading a
//! cooked graph never compiles or validates WGSL ([`crate::compile_count`] stays put).
//!
//! Scene files reference a graph with the reflected component:
//!
//! ```text
//! "regia_matgraph::runtime::MaterialGraphRef": (path: "assets/materials/hull.matinst.ron"),
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer, decode_cooked_file};
use regia_render::{
    GraphTarget, LabMaterial, LabTexture, MeshRenderer, RenderDevice, RuntimeRenderer, SurfaceGraph,
};

use crate::cooked::{COOKED_MATGRAPH_TYPE_PATH, CookedMaterialGraph};

/// Draw this entity's [`MeshRenderer`] with a cooked material graph or instance.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct MaterialGraphRef {
    /// Project-relative path of a `.matgraph.ron` or `.matinst.ron` (`assets/...`).
    #[reflect(@regia_ecs::attrs::AssetFilter("*.matgraph.ron, *.matinst.ron"))]
    pub path: String,
}

impl MaterialGraphRef {
    /// A reference to `path`.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

/// A graph asset could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphLoadError {
    /// The manifest has no entry for the path (not cooked).
    #[error("no cooked asset for `{0}` (cook the project)")]
    NotCooked(String),
    /// The cooked file is unreadable or not a material graph.
    #[error("`{path}`: {reason}")]
    Bad {
        /// Asset path.
        path: String,
        /// What failed.
        reason: String,
    },
}

struct GraphEntry {
    id: AssetId,
    /// [`AssetServer::generation`] of the graph and of each image when built.
    generations: Vec<(AssetId, u64)>,
    material: Result<Arc<LabMaterial>, GraphLoadError>,
}

/// Loaded graph materials by asset path. Every entity naming the same path shares one
/// `Arc<LabMaterial>`, so they batch and draw instanced.
#[derive(Resource, Default)]
pub struct GraphMaterials {
    entries: HashMap<String, GraphEntry>,
    /// Bumped whenever an entry is rebuilt (entities re-resolve).
    revision: u64,
    /// Surfaces loaded since the last prewarm.
    to_prewarm: Vec<Arc<SurfaceGraph>>,
    /// Paths asked for before their cooked asset existed; retried once the manifest has them.
    missing: std::collections::BTreeSet<String>,
    /// A level loaded while the renderer was absent; settle once it exists. Messages only
    /// survive two updates, so the reader latches them here.
    settle_pending: bool,
    /// A run found no [`AssetServer`]: the component change flags of that run are consumed, so
    /// the next run with a server re-resolves every entity.
    reload_all: bool,
    /// Distinct paths named by a live [`MaterialGraphRef`] that also has a [`MeshRenderer`].
    /// Rebuilt only when that set can change.
    wanted: BTreeSet<String>,
    /// Entities in the apply query the last time `wanted` was built.
    tracked: HashSet<Entity>,
    /// False after a run that did not see the query, so the next run rebuilds from the entities.
    wanted_valid: bool,
    /// Times [`apply_material_graphs`] rebuilt the set of paths entities name.
    wanted_rebuilds: u64,
    /// Path `String`s copied into that set. A steady frame adds none.
    path_copies: u64,
}

/// A failed (re)load must not replace a graph that loaded: the last good material keeps
/// serving until a later cook succeeds.
fn keep_last_good(
    old: Option<&Result<Arc<LabMaterial>, GraphLoadError>>,
    new: Result<Arc<LabMaterial>, GraphLoadError>,
) -> Result<Arc<LabMaterial>, GraphLoadError> {
    match (old, new) {
        (Some(Ok(good)), Err(_)) => Ok(Arc::clone(good)),
        (_, new) => new,
    }
}

/// On a failed reload the last good material stays, and so do image generations this attempt
/// never reached. Generations the attempt did observe replace the stored ones, so the same bad
/// cook is not reread every frame and a later cook of a missed image still marks the entry stale.
fn overlay_generations(kept: &[(AssetId, u64)], observed: &[(AssetId, u64)]) -> Vec<(AssetId, u64)> {
    let mut out = kept.to_vec();
    for &(id, generation) in observed {
        if let Some(slot) = out.iter_mut().find(|entry| entry.0 == id) {
            slot.1 = generation;
        } else {
            out.push((id, generation));
        }
    }
    out
}

impl GraphMaterials {
    /// The material for `path`, loading (or reloading after a hot reload) through `server`.
    ///
    /// # Errors
    ///
    /// [`GraphLoadError`] when the asset is missing or malformed (also remembered, so a bad
    /// asset is reported once until it is recooked).
    pub fn load(&mut self, server: &mut AssetServer, path: &str) -> Result<Arc<LabMaterial>, GraphLoadError> {
        if let Some(entry) = self.entries.get(path)
            && entry
                .generations
                .iter()
                .all(|&(id, seen)| server.generation(id) == seen)
        {
            return entry.material.clone();
        }
        let _span = tracing::info_span!("matgraph_load", path).entered();
        let Some(id) = server.id_for_path(path) else {
            if self.missing.insert(path.to_string()) {
                tracing::warn!(target: "regia_matgraph", path, "material graph not cooked yet");
            }
            return Err(GraphLoadError::NotCooked(path.to_string()));
        };
        self.missing.remove(path);
        server.track_external(id);
        let mut generations = vec![(id, server.generation(id))];
        let material = Self::read(server, path, id, &mut generations);
        if let Ok(material) = &material
            && let Some(surface) = &material.surface_graph
        {
            self.to_prewarm.push(Arc::clone(surface));
        }
        if let Err(err) = &material {
            tracing::warn!(target: "regia_matgraph", %err, "material graph not loaded");
        }
        let kept_generations = self
            .entries
            .get(path)
            .filter(|entry| entry.material.is_ok())
            .map(|entry| entry.generations.clone());
        let generations = match (&material, kept_generations) {
            (Err(_), Some(previous)) => overlay_generations(&previous, &generations),
            _ => generations,
        };
        let material = keep_last_good(self.entries.get(path).map(|e| &e.material), material);
        self.revision += 1;
        self.entries.insert(
            path.to_string(),
            GraphEntry {
                id,
                generations,
                material: material.clone(),
            },
        );
        material
    }

    fn read(
        server: &mut AssetServer,
        path: &str,
        id: AssetId,
        generations: &mut Vec<(AssetId, u64)>,
    ) -> Result<Arc<LabMaterial>, GraphLoadError> {
        let bad = |reason: String| GraphLoadError::Bad {
            path: path.to_string(),
            reason,
        };
        let (bytes, _) = server.read_cooked(id).map_err(|e| bad(e.to_string()))?;
        let (header, payload) = decode_cooked_file(&bytes).map_err(|e| bad(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(COOKED_MATGRAPH_TYPE_PATH) {
            return Err(bad("not a cooked material graph".into()));
        }
        let cooked = CookedMaterialGraph::from_postcard(payload).map_err(|e| bad(e.to_string()))?;
        let mut images = BTreeMap::new();
        for texture in &cooked.textures {
            let Some(image_id) = texture.image else {
                continue;
            };
            generations.push((image_id, server.generation(image_id)));
            let handle = server
                .load_image(image_id)
                .map_err(|e| bad(format!("texture `{}`: {e}", texture.path)))?;
            let Some(image) = server.images().get(&handle) else {
                return Err(bad(format!("texture `{}` did not load", texture.path)));
            };
            images.insert(
                texture.slot,
                LabTexture {
                    width: image.width,
                    height: image.height,
                    rgba: image.rgba8.clone(),
                },
            );
        }
        Ok(Arc::new(cooked.to_material(&images)))
    }

    /// The loaded material for `path` (no IO).
    #[must_use]
    pub fn get(&self, path: &str) -> Option<Arc<LabMaterial>> {
        self.entries.get(path).and_then(|e| e.material.clone().ok())
    }

    /// The asset id `path` resolved to.
    #[must_use]
    pub fn asset_id(&self, path: &str) -> Option<AssetId> {
        self.entries.get(path).map(|e| e.id)
    }

    /// True when a hot reload touched a loaded graph or one of its images.
    #[must_use]
    pub fn stale(&self, server: &AssetServer) -> bool {
        self.entries.values().any(|entry| {
            entry
                .generations
                .iter()
                .any(|&(id, seen)| server.generation(id) != seen)
        })
    }

    /// True when a path that was not cooked when first asked for now has a cooked asset, so
    /// the entities naming it should resolve again.
    #[must_use]
    pub fn missing_resolved(&self, server: &AssetServer) -> bool {
        self.missing.iter().any(|path| server.id_for_path(path).is_some())
    }

    /// Paths asked for that have no cooked asset yet (sorted).
    pub fn missing_paths(&self) -> impl Iterator<Item = &str> {
        self.missing.iter().map(String::as_str)
    }

    /// Forgets missing paths no entity names any more, so a path cooked later for nobody does
    /// not keep [`GraphMaterials::missing_resolved`] true forever.
    pub fn retain_missing(&mut self, wanted: &std::collections::BTreeSet<String>) {
        self.missing.retain(|path| wanted.contains(path));
    }

    /// Drops loaded graphs no entity names any more (their CPU image copies and materials).
    pub fn retain_loaded(&mut self, wanted: &std::collections::BTreeSet<String>) {
        self.entries.retain(|path, _| wanted.contains(path));
    }

    /// The path cache is stale: the last run did not observe the query.
    fn invalidate_wanted(&mut self) {
        self.wanted_valid = false;
    }

    /// Rebuilds `wanted` from the entities that currently name a graph. Strings already in the
    /// set are moved, not cloned. `path_copies` counts only a path that is new.
    fn rebuild_wanted<'a>(&mut self, named: impl Iterator<Item = (Entity, &'a str)>) {
        self.wanted_rebuilds += 1;
        let mut next = BTreeSet::new();
        self.tracked.clear();
        for (entity, path) in named {
            self.tracked.insert(entity);
            if next.contains(path) {
                continue;
            }
            if let Some(owned) = self.wanted.take(path) {
                next.insert(owned);
            } else {
                self.path_copies += 1;
                next.insert(path.to_string());
            }
        }
        self.wanted = next;
        self.wanted_valid = true;
    }

    /// Drops missing and loaded graphs no current entity names.
    fn retain_to_wanted(&mut self) {
        let GraphMaterials {
            missing,
            entries,
            wanted,
            ..
        } = self;
        missing.retain(|path| wanted.contains(path));
        entries.retain(|path, _| wanted.contains(path));
    }

    /// Latches that a run had no asset server, so every entity re-resolves on the next one.
    pub fn note_server_absent(&mut self) {
        self.reload_all = true;
    }

    /// True (once) when a run without an asset server is waiting to be repeated.
    pub fn take_reload_all(&mut self) -> bool {
        std::mem::take(&mut self.reload_all)
    }

    /// Latches that a level finished loading and the renderer still has to settle.
    pub fn note_level_loaded(&mut self) {
        self.settle_pending = true;
    }

    /// True (once) when a latched level load is waiting for the renderer to settle.
    pub fn take_settle_pending(&mut self) -> bool {
        std::mem::take(&mut self.settle_pending)
    }

    /// Bumped when any entry was (re)built.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Puts surfaces back after a failed prewarm (and the level latch with them).
    pub fn requeue_prewarm(&mut self, surfaces: Vec<Arc<SurfaceGraph>>, level_loaded: bool) {
        self.to_prewarm.extend(surfaces);
        self.settle_pending |= level_loaded;
    }

    /// Surfaces loaded since the last call (for [`prewarm_graph_pipelines`]).
    pub fn take_prewarm(&mut self) -> Vec<Arc<SurfaceGraph>> {
        std::mem::take(&mut self.to_prewarm)
    }
}

/// `PostUpdate`: resolves every new or changed [`MaterialGraphRef`] (and every one after a
/// hot reload) and sets the entity's [`MeshRenderer::material`].
///
/// The set of paths those entities name is cached on [`GraphMaterials`]. A frame that does not
/// add, remove, or edit a [`MaterialGraphRef`], and does not add or remove the paired
/// [`MeshRenderer`], copies no path and does not walk the loaded set. A renderer edit still
/// reapplies that entity's material.
#[tracing::instrument(skip_all)]
pub fn apply_material_graphs(
    mut graphs: ResMut<GraphMaterials>,
    server: Option<ResMut<AssetServer>>,
    mut entities: Query<(Entity, Ref<MaterialGraphRef>, &mut MeshRenderer)>,
    mut removed_refs: RemovedComponents<MaterialGraphRef>,
    mut removed_meshes: RemovedComponents<MeshRenderer>,
) {
    let Some(mut server) = server else {
        // The change flags of this run are consumed even though nothing was read. An edit in
        // the same gap is not `is_changed` on the next run, so the path cache has to be rebuilt
        // from the entities rather than from the change ticks.
        graphs.note_server_absent();
        graphs.invalidate_wanted();
        removed_refs.clear();
        removed_meshes.clear();
        return;
    };

    // Drain every removal. Stopping at the first hit would leave the rest for the next frame
    // and rebuild the set again for an entity that is already gone.
    let mut removed_tracked = false;
    for entity in removed_refs.read() {
        removed_tracked |= graphs.tracked.contains(&entity);
    }
    for entity in removed_meshes.read() {
        removed_tracked |= graphs.tracked.contains(&entity);
    }

    let mut matched = 0usize;
    let mut ref_changed = false;
    let mut renderer_changed = false;
    for (_, graph_ref, renderer) in &mut entities {
        matched += 1;
        ref_changed |= graph_ref.is_changed();
        renderer_changed |= renderer.is_changed();
    }
    let membership_changed =
        !graphs.wanted_valid || removed_tracked || ref_changed || matched != graphs.tracked.len();
    let reload = graphs.take_reload_all() || graphs.stale(&server) || graphs.missing_resolved(&server);
    if !membership_changed && !reload && !renderer_changed {
        return;
    }

    if membership_changed {
        // `Ref`'s `Deref` borrows the iterator item, so the closure cannot return
        // `path`. `into_inner` yields a reference that lives as long as the query.
        graphs.rebuild_wanted(entities.iter().map(|(entity, graph_ref, _)| {
            let graph_ref = graph_ref.into_inner();
            (entity, graph_ref.path.as_str())
        }));
    }
    for (_, graph_ref, mut renderer) in &mut entities {
        // Membership covers a mesh that joined an older graph ref: that ref's change tick can
        // already be older than this system, and the new entity still needs its material.
        if !(membership_changed || reload || graph_ref.is_changed() || renderer.is_changed()) {
            continue;
        }
        let Ok(material) = graphs.load(&mut server, &graph_ref.path) else {
            continue;
        };
        if !Arc::ptr_eq(&renderer.material, &material) {
            renderer.material = material;
        }
    }
    if membership_changed {
        graphs.retain_to_wanted();
    }
}

/// `PostUpdate`, after [`apply_material_graphs`]: queues the pipelines of newly loaded graphs
/// on the renderer's workers for every target it draws (and the HDR runtime target); a level
/// load waits for them, so the level's first frame draws every graph.
#[tracing::instrument(skip_all)]
pub fn prewarm_graph_pipelines(
    mut graphs: ResMut<GraphMaterials>,
    device: Option<Res<RenderDevice>>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    mut loaded: MessageReader<regia_scene::level::LevelLoaded>,
) {
    // The message is latched first: it lives only two updates, and the device may be absent
    // longer. The queued surfaces wait for a frame that can prewarm.
    if loaded.read().count() > 0 {
        graphs.note_level_loaded();
    }
    let (Some(device), Some(mut renderer)) = (device, renderer) else {
        return;
    };
    let level_loaded = graphs.take_settle_pending();
    let surfaces = graphs.take_prewarm();
    if !surfaces.is_empty() {
        let mut targets = renderer.0.material_graph_targets();
        if !targets.contains(&GraphTarget::HDR) {
            targets.push(GraphTarget::HDR);
        }
        let refs: Vec<&SurfaceGraph> = surfaces.iter().map(Arc::as_ref).collect();
        if let Err(err) = renderer.0.prewarm_material_graphs(&device, &refs, &targets) {
            tracing::warn!(target: "regia_matgraph", %err, "material graph prewarm failed");
            // Queue them again for the next frame, with the level latch.
            graphs.requeue_prewarm(surfaces, level_loaded);
            return;
        }
    }
    if level_loaded {
        renderer.0.settle_material_graphs();
    }
}

/// `PostUpdate`: the renderer's material clock follows [`Time`] (graphs that read `Time`, such
/// as wind sway). The clock is [`regia_render::material_clock()`] of the gameplay time and
/// delta: wrapped every hour in exact integer nanoseconds, so offsets stay smooth in a long
/// session and two runs with the same gameplay time draw the same frames (ADR-1238).
pub fn drive_material_clock(time: Option<Res<Time>>, renderer: Option<ResMut<RuntimeRenderer>>) {
    if let (Some(time), Some(mut renderer)) = (time, renderer) {
        let (now, prev) = regia_render::material_clock(time.elapsed(), time.delta());
        renderer.0.set_material_clock(now, prev);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::World;

    use super::*;

    /// Steps `world`'s gameplay clock by `steps` frames of `dt` and runs the clock system after
    /// each; returns the renderer's clock after every step.
    fn drive(world: &mut World, steps: u32, dt: Duration) -> Vec<(f32, f32)> {
        (0..steps)
            .map(|_| {
                world.resource_mut::<Time>().advance(dt);
                world.run_system_once(drive_material_clock).expect("clock system");
                world
                    .resource::<RuntimeRenderer>()
                    .0
                    .material_clock()
                    .expect("the clock is set")
            })
            .collect()
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(Time::default());
        world.insert_resource(RuntimeRenderer::default());
        world
    }

    /// Two runs with the same gameplay steps set the same clock, bit for bit, and each frame's
    /// `prev` is the frame before's `now` (the motion vectors of a wind offset).
    #[test]
    fn the_material_clock_is_a_function_of_gameplay_time() {
        let dt = Duration::from_nanos(16_666_667);
        let a = drive(&mut world(), 120, dt);
        let b = drive(&mut world(), 120, dt);
        assert_eq!(a, b);
        for pair in a.windows(2) {
            assert_eq!(
                pair[1].1.to_bits(),
                pair[0].0.to_bits(),
                "prev is the last frame's now"
            );
        }
        assert_eq!(a[0].1, 0.0, "the first frame starts from zero");
    }

    /// Pausing gameplay freezes the clock and zeroes its motion; time scale slows it.
    #[test]
    fn a_paused_or_slowed_game_holds_or_slows_the_wind() {
        let dt = Duration::from_millis(20);
        let mut world = world();
        let before = drive(&mut world, 10, dt);
        world.resource_mut::<Time>().paused = true;
        let paused = drive(&mut world, 5, dt);
        let last = before[before.len() - 1].0;
        assert!(paused.iter().all(|&(now, prev)| now == last && prev == last));
        world.resource_mut::<Time>().paused = false;
        world.resource_mut::<Time>().time_scale = 0.5;
        let slow = drive(&mut world, 1, dt);
        assert!(
            (slow[0].0 - slow[0].1 - 0.01).abs() < 1.0e-6,
            "half speed: {:?}",
            slow[0]
        );
    }
}

#[cfg(test)]
mod uncooked_path_tests {
    use super::*;
    use regia_asset::AssetServerSettings;
    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::World;

    #[test]
    fn uncooked_path_is_remembered_and_not_resolved() {
        let root = regia_test_utils::unique_temp_dir("matgraph_missing");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let mut graphs = GraphMaterials::default();
        let path = "assets/materials/late.matgraph.ron";
        assert!(matches!(
            graphs.load(&mut server, path),
            Err(GraphLoadError::NotCooked(_))
        ));
        assert_eq!(graphs.missing_paths().collect::<Vec<_>>(), vec![path]);
        // Still absent from the manifest: nothing to retry yet.
        assert!(!graphs.missing_resolved(&server));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_paths_nobody_names_are_forgotten() {
        let root = regia_test_utils::unique_temp_dir("matgraph_retain");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let mut graphs = GraphMaterials::default();
        let _ = graphs.load(&mut server, "a.matgraph.ron");
        let _ = graphs.load(&mut server, "b.matgraph.ron");
        let wanted: std::collections::BTreeSet<String> = ["b.matgraph.ron".to_string()].into_iter().collect();
        graphs.retain_missing(&wanted);
        assert_eq!(graphs.missing_paths().collect::<Vec<_>>(), vec!["b.matgraph.ron"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_reload_keeps_the_last_good_material() {
        let good = Arc::new(LabMaterial::EMPTY);
        let bad = || Err(GraphLoadError::NotCooked("x".into()));
        let kept = keep_last_good(Some(&Ok(Arc::clone(&good))), bad()).expect("keeps the good one");
        assert!(Arc::ptr_eq(&kept, &good));
        assert!(keep_last_good(None, bad()).is_err());
        assert!(keep_last_good(Some(&bad()), bad()).is_err());
        let newer = Arc::new(LabMaterial::EMPTY);
        let got = keep_last_good(Some(&Ok(good)), Ok(Arc::clone(&newer))).expect("new wins");
        assert!(Arc::ptr_eq(&got, &newer));
    }

    #[test]
    fn a_run_without_an_asset_server_latches_a_reload() {
        let mut world = World::new();
        world.insert_resource(GraphMaterials::default());
        world.spawn((
            MaterialGraphRef::new("a.matgraph.ron"),
            MeshRenderer::new(
                Arc::new(regia_render::CpuMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                }),
                Arc::new(regia_render::CpuMaterial::default()),
            ),
        ));
        world.run_system_once(apply_material_graphs).expect("system");
        assert!(world.resource_mut::<GraphMaterials>().take_reload_all());
        assert!(!world.resource_mut::<GraphMaterials>().take_reload_all());
    }

    #[test]
    fn a_failed_prewarm_requeues_surfaces_and_the_level_latch() {
        let mut graphs = GraphMaterials::default();
        let surface = Arc::new(SurfaceGraph::from_code(
            &regia_render::surface_graph::GraphCode::surface("fn f() {}"),
        ));
        graphs.requeue_prewarm(vec![surface], true);
        assert!(graphs.take_settle_pending());
        assert_eq!(graphs.take_prewarm().len(), 1);
    }

    #[test]
    fn graphs_no_entity_names_are_dropped() {
        let mut graphs = GraphMaterials::default();
        for p in ["a", "b"] {
            graphs.entries.insert(
                p.to_string(),
                GraphEntry {
                    id: AssetId::new_random(),
                    generations: Vec::new(),
                    material: Ok(Arc::new(LabMaterial::EMPTY)),
                },
            );
        }
        let wanted: std::collections::BTreeSet<String> = ["b".to_string()].into_iter().collect();
        graphs.retain_loaded(&wanted);
        assert!(graphs.get("a").is_none() && graphs.get("b").is_some());
    }

    #[test]
    fn level_loaded_latch_survives_until_taken() {
        let mut graphs = GraphMaterials::default();
        assert!(!graphs.take_settle_pending());
        graphs.note_level_loaded();
        assert!(graphs.take_settle_pending());
        assert!(!graphs.take_settle_pending());
    }

    use std::path::Path;

    use regia_asset::{
        AssetManifest, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedImage, ManifestEntry,
        cooked_cache_dir, cooked_file_name, encode_cooked_file, pack_importer_version,
    };
    use regia_core::id::TypePathHash;

    use crate::cooked::{COOKED_MATGRAPH_VERSION, CookedGraphTexture};
    use crate::format::Shading;

    fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("cache dir");
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, bytes).unwrap_or_else(|err| panic!("write {}: {err}", tmp.display()));
        if let Err(err) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            panic!("rename {}: {err}", path.display());
        }
    }

    fn cooked_file(id: AssetId, tag: TypePathHash, payload: &[u8]) -> Vec<u8> {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: id,
            importer_version: pack_importer_version(0, 2, 23),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("encode cooked file")
    }

    fn graph_bytes(textures: &[CookedGraphTexture]) -> Vec<u8> {
        CookedMaterialGraph {
            version: COOKED_MATGRAPH_VERSION,
            name: "g".into(),
            surface_wgsl: "fn material_surface() {}".into(),
            vertex_wgsl: None,
            uses_uv1: false,
            uses_time: false,
            params: vec![[0.0; 4]; 16],
            textures: textures.to_vec(),
            shading: Shading::default(),
            switches: BTreeMap::new(),
        }
        .to_postcard()
        .expect("graph postcard")
    }

    fn image_bytes() -> Vec<u8> {
        CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![8, 16, 24, 255],
        }
        .to_postcard()
        .expect("image postcard")
    }

    fn texture(slot: u8, id: AssetId, path: &str) -> CookedGraphTexture {
        CookedGraphTexture {
            slot,
            srgb: true,
            image: Some(id),
            path: path.to_string(),
        }
    }

    /// A graph whose payload no longer decodes must keep the last good material and the image
    /// generations that attempt never reached. A later cook of the image then still marks the
    /// entry stale. The graph's own generation is refreshed, so the same bad cook is not reread
    /// on every frame.
    #[test]
    fn a_failed_reload_keeps_image_generations_so_a_later_image_cook_is_stale() {
        let root = regia_test_utils::unique_temp_dir("matgraph_reload_gen");
        let cache = cooked_cache_dir(&root, "linux");
        let graph_id = AssetId::new_random();
        let image_id = AssetId::new_random();
        let graph_tag = TypePathHash::of(COOKED_MATGRAPH_TYPE_PATH);
        let image_tag = AssetTypeTag::image().0;
        write_bytes(
            &cache.join(cooked_file_name(image_id)),
            &cooked_file(image_id, image_tag, &image_bytes()),
        );
        write_bytes(
            &cache.join(cooked_file_name(graph_id)),
            &cooked_file(
                graph_id,
                graph_tag,
                &graph_bytes(&[texture(0, image_id, "assets/a.png")]),
            ),
        );
        AssetManifest {
            entries: vec![
                ManifestEntry {
                    path: "assets/g.matgraph.ron".into(),
                    id: graph_id,
                    type_path: COOKED_MATGRAPH_TYPE_PATH.into(),
                },
                ManifestEntry {
                    path: "assets/a.png".into(),
                    id: image_id,
                    type_path: "regia_asset::CookedImage".into(),
                },
            ],
        }
        .write_to_cache(&cache)
        .expect("manifest");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        let mut graphs = GraphMaterials::default();
        let path = "assets/g.matgraph.ron";
        let first = graphs.load(&mut server, path).expect("first load");
        assert!(!graphs.stale(&server));

        write_bytes(
            &cache.join(cooked_file_name(graph_id)),
            &cooked_file(graph_id, graph_tag, b"nope"),
        );
        server
            .reload_id(graph_id)
            .expect("external reload bumps the generation");
        let kept = graphs
            .load(&mut server, path)
            .expect("the last good material stays");
        assert!(Arc::ptr_eq(&first, &kept));
        assert!(
            !graphs.stale(&server),
            "the failed graph's generation is recorded, so it is not reread every frame"
        );

        server.reload_id(image_id).expect("the image file is still valid");
        assert!(
            graphs.stale(&server),
            "a later image cook must mark the entry stale"
        );

        // A cook that succeeds replaces the generation list: the image is gone, so a further
        // bump of it does not reload the graph.
        write_bytes(
            &cache.join(cooked_file_name(graph_id)),
            &cooked_file(graph_id, graph_tag, &graph_bytes(&[])),
        );
        server.reload_id(graph_id).expect("graph generation");
        let replaced = graphs.load(&mut server, path).expect("the good cook loads");
        assert!(!Arc::ptr_eq(&first, &replaced));
        server.reload_id(image_id).expect("image generation");
        assert!(!graphs.stale(&server), "a removed image is no longer tracked");
        let _ = std::fs::remove_dir_all(root);
    }

    /// An image that fails in the middle of a reload must not drop the images the attempt never
    /// reached. A later cook of one of those still marks the entry stale.
    #[test]
    fn a_failed_image_reload_keeps_generations_it_never_reached() {
        let root = regia_test_utils::unique_temp_dir("matgraph_reload_images");
        let cache = cooked_cache_dir(&root, "linux");
        let graph_id = AssetId::new_random();
        let ids = [
            AssetId::new_random(),
            AssetId::new_random(),
            AssetId::new_random(),
        ];
        let graph_tag = TypePathHash::of(COOKED_MATGRAPH_TYPE_PATH);
        let image_tag = AssetTypeTag::image().0;
        let textures: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(slot, id)| texture(u8::try_from(slot).expect("slot"), *id, "assets/t.png"))
            .collect();
        for id in ids {
            write_bytes(
                &cache.join(cooked_file_name(id)),
                &cooked_file(id, image_tag, &image_bytes()),
            );
        }
        write_bytes(
            &cache.join(cooked_file_name(graph_id)),
            &cooked_file(graph_id, graph_tag, &graph_bytes(&textures)),
        );
        let mut entries = vec![ManifestEntry {
            path: "assets/g.matgraph.ron".into(),
            id: graph_id,
            type_path: COOKED_MATGRAPH_TYPE_PATH.into(),
        }];
        for (index, id) in ids.iter().enumerate() {
            entries.push(ManifestEntry {
                path: format!("assets/t{index}.png"),
                id: *id,
                type_path: "regia_asset::CookedImage".into(),
            });
        }
        AssetManifest { entries }
            .write_to_cache(&cache)
            .expect("manifest");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        let mut graphs = GraphMaterials::default();
        let path = "assets/g.matgraph.ron";
        let first = graphs.load(&mut server, path).expect("first load");

        write_bytes(
            &cache.join(cooked_file_name(ids[1])),
            &cooked_file(ids[1], image_tag, b"nope"),
        );
        let _ = server.reload_id(ids[1]);
        let kept = graphs
            .load(&mut server, path)
            .expect("the last good material stays");
        assert!(Arc::ptr_eq(&first, &kept));
        assert!(
            !graphs.stale(&server),
            "the failed image's generation is recorded"
        );

        server.reload_id(ids[2]).expect("the third image is still valid");
        assert!(
            graphs.stale(&server),
            "an image this attempt never reached must stay tracked"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod wanted_path_tests {
    use std::collections::BTreeSet;

    use super::*;
    use regia_asset::AssetServerSettings;
    use regia_ecs::prelude::World;

    const DISTINCT: u64 = 4;
    const ENTITIES: usize = 64;

    fn empty_mesh() -> MeshRenderer {
        MeshRenderer::new(
            Arc::new(regia_render::CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            }),
            Arc::new(regia_render::CpuMaterial::default()),
        )
    }

    fn open(label: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let mut world = World::new();
        world.insert_resource(server);
        world.insert_resource(GraphMaterials::default());
        (world, root)
    }

    fn run(world: &mut World) {
        world.run_system_cached(apply_material_graphs).expect("apply");
    }

    fn counters(world: &World) -> (u64, u64) {
        let graphs = world.resource::<GraphMaterials>();
        (graphs.wanted_rebuilds, graphs.path_copies)
    }

    /// 64 entities share 4 paths. The first update records 4 strings and one rebuild. The next
    /// update, with no component changes, records nothing more.
    #[test]
    fn a_steady_frame_does_not_copy_graph_paths() {
        let (mut world, root) = open("matgraph_steady");
        for i in 0..ENTITIES {
            world.spawn((
                MaterialGraphRef::new(format!(
                    "assets/materials/g{}.matgraph.ron",
                    i % DISTINCT as usize
                )),
                empty_mesh(),
            ));
        }
        run(&mut world);
        let (rebuilds, copies) = counters(&world);
        run(&mut world);
        let (rebuilds_steady, copies_steady) = counters(&world);
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(
            (rebuilds, copies, rebuilds_steady, copies_steady),
            (1, DISTINCT, 1, DISTINCT),
            "first update rebuilds once and copies one string per distinct path ({ENTITIES} entities, \
             {DISTINCT} paths); a steady update does neither"
        );
    }

    fn missing(world: &World) -> Vec<String> {
        world
            .resource::<GraphMaterials>()
            .missing_paths()
            .map(str::to_string)
            .collect()
    }

    fn spawn_graph(world: &mut World, path: &str, with_mesh: bool) -> Entity {
        if with_mesh {
            world.spawn((MaterialGraphRef::new(path), empty_mesh())).id()
        } else {
            world.spawn(MaterialGraphRef::new(path)).id()
        }
    }

    /// A component edit between cached runs has to land on a tick newer than the system's last
    /// run, or `is_changed` stays false.
    fn edit_path(world: &mut World, entity: Entity, path: &str) {
        world.increment_change_tick();
        world
            .entity_mut(entity)
            .get_mut::<MaterialGraphRef>()
            .expect("ref")
            .path = path.to_string();
    }

    #[test]
    fn a_despawned_graph_ref_drops_its_path() {
        let (mut world, root) = open("matgraph_despawn");
        spawn_graph(&mut world, "assets/materials/keep.matgraph.ron", true);
        let drop_path = spawn_graph(&mut world, "assets/materials/drop.matgraph.ron", true);
        run(&mut world);
        assert_eq!(
            missing(&world),
            vec![
                "assets/materials/drop.matgraph.ron",
                "assets/materials/keep.matgraph.ron",
            ]
        );
        world.despawn(drop_path);
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/keep.matgraph.ron"]);
        let (rebuilds, copies) = counters(&world);
        run(&mut world);
        assert_eq!(
            counters(&world),
            (rebuilds, copies),
            "steady after a despawn copies nothing"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_path_edit_replaces_the_cached_path() {
        let (mut world, root) = open("matgraph_edit");
        let entity = spawn_graph(&mut world, "assets/materials/a.matgraph.ron", true);
        run(&mut world);
        let (rebuilds, copies) = counters(&world);
        assert_eq!(copies, 1);
        edit_path(&mut world, entity, "assets/materials/c.matgraph.ron");
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/c.matgraph.ron"]);
        let (rebuilds_edit, copies_edit) = counters(&world);
        assert_eq!(rebuilds_edit, rebuilds + 1, "an edit rebuilds the path set");
        assert_eq!(copies_edit, copies + 1, "an edit copies only the new path");
        run(&mut world);
        assert_eq!(counters(&world), (rebuilds_edit, copies_edit));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removing_the_mesh_drops_the_graph_path() {
        let (mut world, root) = open("matgraph_unmesh");
        let entity = spawn_graph(&mut world, "assets/materials/a.matgraph.ron", true);
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/a.matgraph.ron"]);
        world.entity_mut(entity).remove::<MeshRenderer>();
        run(&mut world);
        assert!(
            missing(&world).is_empty(),
            "no mesh names the graph: {:?}",
            missing(&world)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_mesh_added_later_picks_up_the_graph() {
        let (mut world, root) = open("matgraph_mesh_later");
        let entity = spawn_graph(&mut world, "assets/materials/a.matgraph.ron", false);
        run(&mut world);
        assert!(missing(&world).is_empty());
        assert_eq!(counters(&world).1, 0, "a ref with no mesh copies no path");
        world.increment_change_tick();
        world.entity_mut(entity).insert(empty_mesh());
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/a.matgraph.ron"]);
        assert_eq!(counters(&world).1, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// One graph mesh leaves and another entity's mesh appears. The query length stays 1, so
    /// the removal itself has to retarget the cached set.
    #[test]
    fn a_mesh_swap_retargets_the_wanted_set() {
        let (mut world, root) = open("matgraph_swap");
        let left = spawn_graph(&mut world, "assets/materials/left.matgraph.ron", true);
        let right = spawn_graph(&mut world, "assets/materials/right.matgraph.ron", false);
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/left.matgraph.ron"]);
        world.entity_mut(left).remove::<MeshRenderer>();
        world.increment_change_tick();
        world.entity_mut(right).insert(empty_mesh());
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/right.matgraph.ron"]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The run with no asset server consumes change ticks. The path edited in that gap has to
    /// be the one remembered once the server is back, not the path from before the gap.
    #[test]
    fn a_server_gap_rebuilds_from_the_entities() {
        let (mut world, root) = open("matgraph_gap");
        let entity = spawn_graph(&mut world, "assets/materials/before.matgraph.ron", true);
        run(&mut world);
        edit_path(&mut world, entity, "assets/materials/after.matgraph.ron");
        let server = world.remove_resource::<AssetServer>().expect("server");
        run(&mut world);
        world.insert_resource(server);
        run(&mut world);
        assert_eq!(missing(&world), vec!["assets/materials/after.matgraph.ron"]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Editing the mesh, not the graph path, reapplies the load and does not copy paths.
    #[test]
    fn a_renderer_edit_does_not_copy_graph_paths() {
        let (mut world, root) = open("matgraph_renderer");
        let entity = spawn_graph(&mut world, "assets/materials/a.matgraph.ron", true);
        run(&mut world);
        run(&mut world);
        let frozen = counters(&world);
        {
            let mut graphs = world.resource_mut::<GraphMaterials>();
            let empty = BTreeSet::new();
            graphs.retain_missing(&empty);
        }
        assert!(missing(&world).is_empty());
        world.increment_change_tick();
        world
            .entity_mut(entity)
            .get_mut::<MeshRenderer>()
            .expect("mesh")
            .layer = 3;
        run(&mut world);
        assert_eq!(counters(&world), frozen, "a renderer edit copies no graph path");
        assert_eq!(missing(&world), vec!["assets/materials/a.matgraph.ron"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_loaded_graph_nobody_still_names_is_dropped() {
        let (mut world, root) = open("matgraph_drop_loaded");
        spawn_graph(&mut world, "assets/materials/kept.matgraph.ron", true);
        world.resource_mut::<GraphMaterials>().entries.insert(
            "assets/materials/gone.matgraph.ron".to_string(),
            GraphEntry {
                id: regia_asset::AssetId::new_random(),
                generations: Vec::new(),
                material: Ok(Arc::new(regia_render::LabMaterial::EMPTY)),
            },
        );
        run(&mut world);
        assert!(
            world
                .resource::<GraphMaterials>()
                .get("assets/materials/gone.matgraph.ron")
                .is_none()
        );
        assert!(
            world
                .resource::<GraphMaterials>()
                .get("assets/materials/kept.matgraph.ron")
                .is_none()
        );
        assert_eq!(missing(&world), vec!["assets/materials/kept.matgraph.ron"]);
        let _ = std::fs::remove_dir_all(root);
    }
}
