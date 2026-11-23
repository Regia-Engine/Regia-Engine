// SPDX-License-Identifier: MPL-2.0
//! The ECS plugin: messages that spawn effects, decals and shakes, and per-frame CPU systems.
//!
//! The GPU side is host-driven ([`crate::gpu::VfxGpu`]); this plugin keeps every CPU resource
//! ticking so any system (or the sim-bridge event router) can fire
//! `SpawnEffect` / `SpawnDecal` / `CameraShakeRequest` messages and the state updates itself.
//!
//! The camera comes from the [`VfxView`] resource — the host renderer writes it once per frame
//! (LOD distances and shake falloff read it; one frame of latency is fine for both).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_sin_cos};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

use crate::decal::DecalPool;
use crate::decal_format::{DecalLibrary, StaticDecal};
use crate::gpu::{VfxCamera, VfxProfile};
use crate::scene::VfxScene;
use crate::shake::{CameraShake, CameraShakeRequest};
use crate::sim::LodInput;

/// Request a one-shot effect from a `.vfx.ron` system (event router / scripts / animation
/// notifies all send this).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct SpawnEffect {
    /// Effect (system) name.
    pub effect: String,
    /// World position.
    pub position: Vec3,
    /// Local +Z direction (velocity cone axis).
    pub forward: Vec3,
    /// Deterministic seed override; `None` derives one from a counter.
    pub seed: Option<u64>,
}

/// Host-renderer-provided view: written once per frame, read by LOD and shake falloff.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct VfxView {
    /// The camera this frame.
    pub camera: VfxCamera,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
}

impl Default for VfxView {
    fn default() -> Self {
        Self {
            camera: VfxCamera::looking_at(Vec3::new(0.0, 2.0, 6.0), Vec3::ZERO),
            width: 1280,
            height: 720,
        }
    }
}

/// Runtime look switches for the fire layers (agent `vfx.get_look` / `vfx.set_look`).
///
/// Read by [`crate::VfxRuntimePlugin`] every frame; the offscreen harness ignores it.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct VfxLook {
    /// Heat-haze cards bend the scene (any HDR frame with a scene copy).
    pub heat_haze: bool,
    /// Multiplier on every haze card's `heat_haze` pixels.
    pub haze_scale: f32,
    /// Particle lights reach the renderer's local lights and the lit smoke.
    pub fire_light: bool,
    /// Multiplier on every particle light's intensity.
    pub fire_light_scale: f32,
    /// Overrides the profile's soft-particle layer resolution (`None`: the profile's).
    pub soft_particle_res: Option<crate::gpu::SoftParticleRes>,
}

impl Default for VfxLook {
    fn default() -> Self {
        Self {
            heat_haze: true,
            haze_scale: 1.0,
            fire_light: true,
            fire_light_scale: 1.0,
            soft_particle_res: None,
        }
    }
}

/// Global VFX configuration.
#[derive(Resource, Debug, Clone)]
pub struct VfxSettings {
    /// Quality profile (pool sizes, sort on/off, light budget).
    pub profile: VfxProfile,
    /// Directory scanned for `*.vfx.ron` at startup (empty/`None` = no assets).
    pub library_dir: Option<PathBuf>,
    /// Camera-shake tuning: trauma decay per second.
    pub shake_decay: f32,
    /// Camera-shake tuning: positional amplitude at full trauma (metres).
    pub shake_amplitude: f32,
    /// Camera-shake tuning: roll amplitude at full trauma (radians).
    pub shake_roll: f32,
}

impl Default for VfxSettings {
    fn default() -> Self {
        Self {
            profile: VfxProfile::desktop_high(),
            library_dir: None,
            shake_decay: 1.2,
            shake_amplitude: 0.35,
            shake_roll: 0.02,
        }
    }
}

/// Installs the VFX CPU state: resources, messages and per-frame systems.
pub struct VfxPlugin;

impl Plugin for VfxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VfxSettings>()
            .init_resource::<VfxView>()
            .init_resource::<VfxLook>()
            .init_resource::<CameraShake>()
            .init_resource::<crate::lights::VfxFrameLights>()
            .init_resource::<crate::fire_light::FireLights>()
            .init_resource::<VfxSeedCounter>()
            .init_resource::<DecalLibrary>()
            .init_resource::<IngestedStaticDecals>()
            .init_resource::<crate::decal::HiddenStaticDecals>()
            .init_resource::<crate::crash::BlastGusts>()
            .register_type::<StaticDecal>()
            .register_type::<crate::hover::HoverWash>()
            .register_type::<crate::plume::ThrusterPlume>()
            .register_type::<crate::crash::CrashSequence>()
            .add_plugins(crate::glass::GlassEffectsPlugin)
            .add_plugins(crate::weather::WeatherPlugin)
            .add_plugins(crate::ambient::AmbientLifePlugin)
            .add_observer(ingest_added_decal)
            .add_observer(retire_removed_decal)
            .add_observer(retire_parked_decal)
            .add_observer(restore_live_decal)
            .add_message::<regia_scene::streaming::OriginShifted>()
            // In the fixed step of the rebase, before `Update` ticks effects from the moved
            // transforms: a later shift would read the jump as motion (spec 78 § 12.2).
            .add_systems(
                FixedUpdate,
                shift_vfx_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            )
            .add_message::<SpawnEffect>()
            .add_message::<crate::decal::SpawnDecal>()
            .add_message::<CameraShakeRequest>()
            .add_message::<crate::crash::CrashEvent>()
            .add_systems(Startup, (insert_empty_scene, init_vfx_scene).chain())
            .add_systems(
                Startup,
                (load_decal_library, init_decal_pool, ingest_static_decals).chain(),
            )
            .add_systems(
                Update,
                (
                    sync_vfx_medium,
                    apply_effect_requests,
                    crate::hover::drive_hover_washes,
                    crate::plume::drive_thruster_plumes,
                    crate::crash::drive_crash_sequences,
                    crate::crash::publish_blast_gusts,
                    crate::decal::apply_decal_requests,
                    crate::shake::apply_shake_requests,
                )
                    .chain()
                    .before(tick_vfx_scene),
            )
            .add_message::<regia_asset::SourceAssetChanged>()
            .add_systems(Update, reload_vfx_files.before(apply_effect_requests))
            .add_systems(
                Update,
                (
                    tick_vfx_scene,
                    (
                        crate::decal::update_decals,
                        crate::decal::follow_anchored_decals,
                        collect_hidden_static_decals,
                    )
                        .chain(),
                    tick_camera_shake,
                    (gather_vfx_lights, crate::fire_light::drive_fire_lights).chain(),
                ),
            );
        // The ranking measures against this frame's view (ADR-3275).
        app.add_systems(
            Update,
            sync_vfx_significance_view
                .after(apply_effect_requests)
                .before(tick_vfx_scene),
        );
    }
}

/// What [`sync_vfx_medium`] last applied: the medium, the library size it saw, and the names of
/// the built-in variants it registered (a game's own variant of the same name is never replaced).
#[derive(Default)]
struct MediumSync {
    medium: Option<crate::medium::VfxMedium>,
    library_len: usize,
    builtin: std::collections::HashSet<String>,
}

/// Keeps the scene's medium on the level's atmosphere and gravity
/// ([`crate::medium::VfxMedium::from_level`]), and registers the built-in airless / Mars
/// variants of the showcase explosion and smoke ([`crate::atmos_presets::variants_in`]) for every
/// base effect the library has, so atmosphere-aware effects are the default.
fn sync_vfx_medium(
    level: Option<Res<regia_scene::level::LevelSettings>>,
    scene: Option<ResMut<VfxScene>>,
    mut sync: Local<MediumSync>,
) {
    let _span = tracing::debug_span!("sync_vfx_medium").entered();
    let Some(mut scene) = scene else { return };
    let medium = level.as_deref().map_or_else(
        crate::medium::VfxMedium::earth,
        crate::medium::VfxMedium::from_level,
    );
    let len = scene.library().defs().len();
    if sync.medium == Some(medium) && scene.medium() == medium && sync.library_len == len {
        return;
    }
    scene.set_medium(medium);
    for def in crate::atmos_presets::variants_in(&medium) {
        let base = def.name.split('@').next().unwrap_or_default().to_string();
        let owned_by_game = scene.library().index(&def.name).is_some() && !sync.builtin.contains(&def.name);
        if owned_by_game || scene.library().index(&base).is_none() {
            continue;
        }
        let name = def.name.clone();
        match scene.library_mut().add(def) {
            Ok(_) => {
                sync.builtin.insert(name);
            }
            Err(err) => tracing::warn!(target: "regia_vfx", "medium variant {name} rejected: {err}"),
        }
    }
    sync.medium = Some(medium);
    sync.library_len = scene.library().defs().len();
}

/// The effect stores a rebase moves, each present only when its plugin or the game added it.
type VfxOriginStores<'w> = (
    Option<ResMut<'w, crate::scene::VfxScene>>,
    Option<Res<'w, crate::runtime::VfxGpuHandle>>,
    Option<ResMut<'w, crate::debris_mesh::DebrisBursts>>,
    Option<ResMut<'w, crate::fire_light::FireLights>>,
    Option<ResMut<'w, crate::impact_site::ImpactSites>>,
);

/// Moves decals, effect instances, the GPU particle pool, debris, fire lights and impact sites
/// with the floating origin: they store world positions, not transforms, so a rebase moves them
/// by hand (spec 78 § 12.2).
fn shift_vfx_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    pool: Option<ResMut<DecalPool>>,
    mut stores: VfxOriginStores,
) {
    let mut pool = pool;
    for message in shifted.read() {
        let _span = tracing::info_span!("vfx_shift_origin").entered();
        let shift = Vec3::from_array(message.rebase.shift);
        if let Some(pool) = pool.as_deref_mut() {
            pool.shift_origin(shift);
        }
        let (scene, gpu, debris, fires, sites) = &mut stores;
        if let Some(scene) = scene.as_deref_mut() {
            scene.shift_origin(shift);
        }
        // The live GPU pool moves on the next simulated frame, before that frame's spawns.
        if let Some(mut gpu) = gpu.as_deref().and_then(crate::runtime::VfxGpuHandle::lock) {
            gpu.shift_origin(shift);
        }
        if let Some(debris) = debris.as_deref_mut() {
            debris.shift_origin(shift);
        }
        if let Some(fires) = fires.as_deref_mut() {
            fires.shift_origin(shift);
        }
        if let Some(sites) = sites.as_deref_mut() {
            sites.shift_origin(shift);
        }
    }
}

/// Monotonic seed source so unseeded spawns stay deterministic per run.
#[derive(Resource, Debug, Default)]
pub(crate) struct VfxSeedCounter(pub(crate) u64);

fn init_decal_pool(world: &mut World) {
    if world.contains_resource::<DecalPool>() {
        return;
    }
    let capacity = world
        .get_resource::<VfxSettings>()
        .map(|settings| settings.profile.decal_capacity)
        .unwrap_or(0);
    if capacity == 0 {
        return;
    }
    world.insert_resource(DecalPool::new(capacity));
}

/// Entities whose [`StaticDecal`] has already been copied into the pool, plus ones whose
/// prefab was missing (warned once, retried if the library gains the name).
#[derive(Resource, Default)]
struct IngestedStaticDecals {
    /// Ingested entities and their decal's stable id (a prefab reload rewrites that decal; other
    /// decals retiring or the full pool recycling never redirect it).
    done: HashMap<Entity, crate::decal::DecalId>,
    missing: HashSet<Entity>,
}

/// Loads `*.decal.ron` from the same directory as `.vfx.ron` when one is configured.
fn load_decal_library(mut library: ResMut<DecalLibrary>, settings: Res<VfxSettings>) {
    let Some(path) = settings.library_dir.clone() else {
        return;
    };
    match library.load_dir(&path) {
        Ok(0) => {}
        Ok(n) => tracing::info!(target: "regia_vfx", "loaded {n} decal prefabs from {path:?}"),
        Err(err) => tracing::warn!(target: "regia_vfx", "decal library load failed: {err}"),
    }
}

/// Copies one newly added [`StaticDecal`] into the pool. Runs on the add, not every frame.
fn ingest_added_decal(
    on: On<Add<StaticDecal>>,
    query: Query<(Entity, &StaticDecal, &Transform, Has<LayerParked>)>,
    library: Res<DecalLibrary>,
    mut pool: Option<ResMut<DecalPool>>,
    mut ingested: ResMut<IngestedStaticDecals>,
) {
    let Some(pool) = pool.as_deref_mut() else {
        return;
    };
    let entity = on.event().entity;
    let Ok((_, decal, transform, parked)) = query.get(entity) else {
        return;
    };
    if parked {
        return;
    }
    ingest_one(entity, decal, transform, &library, pool, &mut ingested);
}

/// Retires the pool decal of a [`StaticDecal`] entity that was despawned (or lost the
/// component), so streamed-out chunks do not leave decals behind or crowd the pool.
fn retire_removed_decal(
    on: On<Remove<StaticDecal>>,
    mut pool: Option<ResMut<DecalPool>>,
    mut ingested: ResMut<IngestedStaticDecals>,
) {
    let entity = on.event().entity;
    ingested.missing.remove(&entity);
    if let Some(id) = ingested.done.remove(&entity)
        && let Some(pool) = pool.as_deref_mut()
    {
        pool.remove(id);
    }
}

/// A parked [`StaticDecal`] leaves the pool (spec 37 § 13: a parked member draws nothing). The
/// pool is the only thing that draws it, so hiding the entity alone would leave it on the ground.
fn retire_parked_decal(
    on: On<Add<LayerParked>>,
    mut pool: Option<ResMut<DecalPool>>,
    mut ingested: ResMut<IngestedStaticDecals>,
) {
    if let Some(id) = ingested.done.remove(&on.event().entity)
        && let Some(pool) = pool.as_deref_mut()
    {
        pool.remove(id);
    }
}

/// A [`StaticDecal`] whose layer went live goes back into the pool.
fn restore_live_decal(
    on: On<Remove<LayerParked>>,
    query: Query<(Entity, &StaticDecal, &Transform)>,
    library: Res<DecalLibrary>,
    mut pool: Option<ResMut<DecalPool>>,
    mut ingested: ResMut<IngestedStaticDecals>,
) {
    let Some(pool) = pool.as_deref_mut() else {
        return;
    };
    let entity = on.event().entity;
    let Ok((_, decal, transform)) = query.get(entity) else {
        return;
    };
    ingest_one(entity, decal, transform, &library, pool, &mut ingested);
}

/// A static decal entity and its own and resolved visibility.
type StaticDecalVisibility = (
    Entity,
    Option<&'static regia_scene::components::Visibility>,
    Option<&'static regia_scene::components::InheritedVisibility>,
);

/// `Update`: the pool decals of hidden (not parked) [`StaticDecal`] entities, for the draw to
/// leave out ([`crate::decal::HiddenStaticDecals`], spec 14 § A1, ADR-7255). Hidden is the
/// entity's own `Visibility::Hidden` (read directly, so the frame that hides it already draws
/// without it) or its resolved `InheritedVisibility(false)` (a hidden ancestor). The pool is not
/// touched. Written only when the set changed; allocation-free in steady state.
fn collect_hidden_static_decals(
    decals: Query<StaticDecalVisibility, With<StaticDecal>>,
    ingested: Res<IngestedStaticDecals>,
    mut hidden: ResMut<crate::decal::HiddenStaticDecals>,
    mut scratch: Local<Vec<crate::decal::DecalId>>,
) {
    scratch.clear();
    for (entity, own, inherited) in &decals {
        let is_hidden = own == Some(&regia_scene::components::Visibility::Hidden)
            || inherited.is_some_and(|visible| !visible.0);
        if is_hidden && let Some(id) = ingested.done.get(&entity) {
            scratch.push(*id);
        }
    }
    scratch.sort_unstable_by_key(crate::decal::DecalId::sort_key);
    scratch.dedup();
    if hidden.ids() != scratch.as_slice() {
        hidden.set(scratch.iter().copied());
    }
}

/// Copies level-placed [`StaticDecal`] entities into the pool once, at startup.
///
/// Identity rotation projects along world +Y (plus the component yaw). Any other rotation is
/// the decal basis. Scale multiplies the prefab half extents. A non-zero `receiver_mask`
/// replaces the prefab mask. This stays on `Startup` so `Update` does not read `Transform`
/// (that read pulls pose sync forward and slows a match that has no static decals).
fn ingest_static_decals(
    query: Query<(Entity, &StaticDecal, &Transform), Without<LayerParked>>,
    library: Res<DecalLibrary>,
    mut pool: ResMut<DecalPool>,
    mut ingested: ResMut<IngestedStaticDecals>,
) {
    for (entity, decal, transform) in &query {
        ingest_one(entity, decal, transform, &library, &mut pool, &mut ingested);
    }
}

fn ingest_one(
    entity: Entity,
    decal: &StaticDecal,
    transform: &Transform,
    library: &DecalLibrary,
    pool: &mut DecalPool,
    ingested: &mut IngestedStaticDecals,
) {
    if ingested.done.contains_key(&entity) {
        return;
    }
    let Some(prefab) = library.get(&decal.prefab) else {
        if ingested.missing.insert(entity) {
            tracing::warn!(target: "regia_vfx", "static decal prefab '{}' is not loaded", decal.prefab);
        }
        return;
    };
    let Some((placed, surface)) = instantiate_static(prefab, decal, transform) else {
        return;
    };
    let slot = pool.spawn_with(placed, surface);
    pool.set_footstep_surface(slot, prefab.surface.as_deref());
    ingested.done.insert(entity, slot);
    ingested.missing.remove(&entity);
}

/// Which [`StaticDecal`] entities are in the pool now, for [`resync_static_decals`].
pub(crate) fn ingested_static_decals(world: &World) -> Option<HashMap<Entity, crate::decal::DecalId>> {
    world
        .get_resource::<IngestedStaticDecals>()
        .map(|ingested| ingested.done.clone())
}

/// Squares the [`StaticDecal`] entities with `pool`, a pool taken earlier (when `captured` was) that
/// is about to go back (ADR-2728): the entities come and go with the world, which is not put back.
/// One that went away since takes its decal out of the pool; one that came since goes in.
pub(crate) fn resync_static_decals(
    world: &mut World,
    captured: HashMap<Entity, crate::decal::DecalId>,
    pool: &mut DecalPool,
) {
    let Some(mut ingested) = world.get_resource_mut::<IngestedStaticDecals>() else {
        return;
    };
    let now = std::mem::take(&mut ingested.done);
    let mut done = HashMap::with_capacity(now.len());
    // HashMap iteration order is per-process random: the removes below push freed slots in
    // that order and the spawns below take them, so two machines restoring the same set would
    // recycle different slots and composite equal-priority decals in different draw orders.
    // Walk both loops in `Entity` (index, generation) order so the result is one fixed order.
    let mut captured: Vec<(Entity, crate::decal::DecalId)> = captured.into_iter().collect();
    captured.sort_by_key(|(entity, _)| entity.to_bits());
    for (entity, id) in captured {
        if now.contains_key(&entity) {
            done.insert(entity, id);
        } else {
            pool.remove(id);
        }
    }
    let mut came: Vec<Entity> = now
        .into_keys()
        .filter(|entity| !done.contains_key(entity))
        .collect();
    // Deterministic ingest order (ADR-4894): the hash map's order differs per process, and ingest
    // order is the pool's serial order, which same-priority decals draw and recycle in. Entity
    // order is the order a plain run streams them in.
    came.sort_unstable_by_key(|entity| entity.to_bits());
    let mut ingested = IngestedStaticDecals {
        done,
        missing: std::mem::take(&mut ingested.missing),
    };
    if let Some(library) = world.get_resource::<DecalLibrary>() {
        for entity in came {
            if let (Some(decal), Some(transform)) =
                (world.get::<StaticDecal>(entity), world.get::<Transform>(entity))
            {
                ingest_one(entity, decal, transform, library, pool, &mut ingested);
            }
        }
    }
    world.insert_resource(ingested);
}

/// Places one static decal from `prefab` (shared by ingest and live reload).
fn instantiate_static(
    prefab: &crate::decal_format::DecalPrefab,
    decal: &StaticDecal,
    transform: &Transform,
) -> Option<(crate::decal::Decal, crate::decal::DecalSurface)> {
    let (mut placed, mut surface) = prefab
        .instantiate(transform.translation, Vec3::Y, decal.yaw, None)
        .ok()?;
    let identity = transform.rotation.dot(Quat::IDENTITY).abs() > 1.0 - 1e-4;
    if !identity {
        // `Quat::from_rotation_z(yaw)` from the deterministic kernel (x = y = 0,
        // z = sin(yaw/2), w = cos(yaw/2)): the same bits on every platform.
        let (sin, cos) = det_sin_cos(decal.yaw * 0.5);
        placed.rotation = transform.rotation * Quat::from_xyzw(0.0, 0.0, sin, cos);
    }
    placed.half_size *= Vec3::new(
        transform.scale.x.abs(),
        transform.scale.y.abs(),
        transform.scale.z.abs(),
    );
    if decal.receiver_mask != 0 {
        surface.receiver_mask = decal.receiver_mask;
    }
    Some((placed, surface))
}

/// `Update`: a `.vfx.ron` or `.decal.ron` changed on disk (dev builds,
/// `docs/specs/12-assets.md` § 8). A VFX system is replaced in place, so running instances
/// continue with the new parameters; a decal prefab is replaced and every static decal placed
/// from it is re-instantiated in its pool slot. Parse errors keep the old definition.
#[allow(clippy::too_many_arguments)]
fn reload_vfx_files(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    scene: Option<ResMut<VfxScene>>,
    mut library: ResMut<DecalLibrary>,
    mut pool: Option<ResMut<DecalPool>>,
    mut ingested: ResMut<IngestedStaticDecals>,
    statics: Query<(Entity, &StaticDecal, &Transform)>,
    mut status: Option<ResMut<regia_asset::AssetWatchStatus>>,
    assets: Option<Res<regia_asset::AssetServer>>,
) {
    use regia_asset::{SourceKind, WatchEntry, WatchOutcome};
    let mut scene = scene;
    for change in changes.read() {
        if !matches!(change.kind, SourceKind::Vfx | SourceKind::Decal) {
            continue;
        }
        let started = std::time::Instant::now();
        let shown = match assets.as_deref() {
            Some(server) => regia_asset::display_path(server.project_root(), &change.path),
            None => change.path.display().to_string(),
        };
        let text = std::fs::read_to_string(&change.path).map_err(|e| e.to_string());
        let result: Result<String, String> = match change.kind {
            SourceKind::Vfx => text
                .and_then(|t| crate::format::VfxSystemDef::from_ron(&t).map_err(|e| e.to_string()))
                .and_then(|def| {
                    let name = def.name.clone();
                    match scene.as_deref_mut() {
                        Some(scene) => scene
                            .library_mut()
                            .upsert(def)
                            .map(|_| format!("vfx {name}"))
                            .map_err(|e| e.to_string()),
                        None => Err("no VfxScene".to_string()),
                    }
                }),
            _ => text
                .and_then(|t| crate::decal_format::DecalPrefab::from_ron(&t).map_err(|e| e.to_string()))
                .and_then(|prefab| {
                    let name = prefab.name.clone();
                    let parent = change.path.parent().unwrap_or_else(|| std::path::Path::new("."));
                    library.load_prefab(prefab, parent).map_err(|e| e.to_string())?;
                    let mut replaced = 0;
                    if let (Some(pool), Some(prefab)) = (pool.as_deref_mut(), library.get(&name)) {
                        for (entity, decal, transform) in &statics {
                            if decal.prefab != name {
                                continue;
                            }
                            let Some(&id) = ingested.done.get(&entity) else {
                                continue;
                            };
                            if let Some((placed, surface)) = instantiate_static(prefab, decal, transform) {
                                let footstep = prefab.surface.as_deref();
                                if pool.replace(id, placed, surface) {
                                    pool.set_footstep_surface(id, footstep);
                                } else {
                                    // The full pool recycled it: place it again.
                                    let new = pool.spawn_with(placed, surface);
                                    pool.set_footstep_surface(new, footstep);
                                    ingested.done.insert(entity, new);
                                }
                                replaced += 1;
                            }
                        }
                    }
                    Ok(format!("decal {name} ({replaced} placed)"))
                }),
        };
        let outcome = match result {
            Ok(what) => {
                tracing::info!(target: "regia_vfx", "reloaded {what} from {shown}");
                WatchOutcome::Reloaded
            }
            Err(err) => {
                tracing::error!(target: "regia_vfx", "{shown}: {err} (keeping the previous version)");
                WatchOutcome::Error(err)
            }
        };
        if let Some(status) = status.as_deref_mut() {
            status.record(WatchEntry {
                path: shown,
                kind: change.kind,
                millis: started.elapsed().as_millis() as u64,
                outcome,
            });
        }
    }
}

/// Installs an empty [`VfxScene`] unless the host already provided one; [`init_vfx_scene`]
/// (Startup) upgrades it from `library_dir` when set.
fn insert_empty_scene(world: &mut World) {
    if world.get_resource::<VfxScene>().is_none() {
        world.insert_resource(VfxScene::new(crate::scene::VfxLibrary::new()));
    }
}

fn init_vfx_scene(world: &mut World) {
    let dir = world
        .get_resource::<VfxSettings>()
        .and_then(|s| s.library_dir.clone());
    let Some(path) = dir else { return };
    if world.resource::<VfxScene>().library().defs().is_empty() {
        match crate::scene::VfxLibrary::load_dir(&path) {
            Ok(library) if !library.defs().is_empty() => {
                world.insert_resource(VfxScene::new(library));
            }
            Ok(_) => tracing::warn!(target: "regia_vfx", "no *.vfx.ron assets in {path:?}"),
            Err(err) => tracing::error!(target: "regia_vfx", "vfx library load failed: {err}"),
        }
    }
}

/// Consumes [`SpawnEffect`] messages into the scene.
fn apply_effect_requests(
    mut requests: MessageReader<SpawnEffect>,
    mut scene: ResMut<VfxScene>,
    mut seeds: ResMut<VfxSeedCounter>,
) {
    let _span = tracing::debug_span!("apply_effect_requests").entered();
    for req in requests.read() {
        seeds.0 = seeds.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let seed = req.seed.unwrap_or(seeds.0 ^ fxhash(&req.effect));
        if scene
            .spawn(&req.effect, req.position, req.forward, seed)
            .is_none()
        {
            if scene.library().index(&scene.resolve(&req.effect)).is_some() {
                // A per-system cap refused it (ADR-3275); counted in the significance stats.
                tracing::debug!(target: "regia_vfx", "effect '{}' refused by its system cap", req.effect);
            } else {
                tracing::warn!(target: "regia_vfx", "unknown effect '{}'", req.effect);
            }
        }
    }
}

/// FNV-1a over a string (stable, cheap, public-domain algorithm).
fn fxhash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Advances spawn scheduling for the frame.
pub(crate) fn tick_vfx_scene(
    time: Res<Time>,
    settings: Res<VfxSettings>,
    view: Res<VfxView>,
    wind: Option<Res<regia_scene::weather::SceneWind>>,
    mut scene: ResMut<VfxScene>,
) {
    let _span = tracing::debug_span!("tick_vfx_scene").entered();
    let lod = LodInput {
        camera_pos: view.camera.eye,
    };
    // `wind_response` emitters read the scene wind (ADR-2642); no resource is calm air.
    let wind_now = wind.as_deref().and_then(regia_scene::weather::SceneWind::active);
    scene.set_wind(wind_now.map_or(Vec3::ZERO, |[x, z]| Vec3::new(x, 0.0, z)));
    scene.update_budgets(
        time.delta_secs(),
        &lod,
        settings.profile.particle_capacity,
        settings.profile.smoke_budget,
    );
}

/// Copies [`VfxView`] into the scene's significance ranking, only while caps are active (without
/// caps the scene is not touched, so nothing changes for a host that never sets them).
fn sync_vfx_significance_view(view: Res<VfxView>, scene: Option<ResMut<VfxScene>>) {
    let Some(mut scene) = scene else { return };
    if !scene.significance().caps().is_active() {
        return;
    }
    let next = crate::significance::SignificanceView::from_camera(&view.camera, view.width, view.height);
    if scene.significance().view() != Some(&next) {
        scene.significance_mut().set_view(Some(next));
    }
}

/// Advances the camera shake.
pub(crate) fn tick_camera_shake(time: Res<Time>, settings: Res<VfxSettings>, mut shake: ResMut<CameraShake>) {
    shake.update(
        time.delta_secs(),
        settings.shake_decay,
        settings.shake_amplitude,
        settings.shake_roll,
    );
}

/// Gathers this frame's particle lights (budgeted).
fn gather_vfx_lights(
    settings: Res<VfxSettings>,
    scene: ResMut<VfxScene>,
    mut lights: ResMut<crate::lights::VfxFrameLights>,
) {
    scene.gather_lights(&mut lights, settings.profile.light_budget);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fxhash_is_stable() {
        assert_eq!(fxhash("explosion"), fxhash("explosion"));
        assert_ne!(fxhash("explosion"), fxhash("sparks"));
    }

    fn medium_app(preset: regia_scene::level::AtmospherePreset, gravity: f32) -> App {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        let mut level = regia_scene::level::LevelSettings::default();
        level.atmosphere.preset = preset;
        level.gravity = Vec3::new(0.0, -gravity, 0.0);
        app.insert_resource(level);
        let mut lib = crate::scene::VfxLibrary::new();
        for def in crate::showcase::showcase_effects() {
            lib.add(def).expect("showcase");
        }
        app.insert_resource(VfxScene::new(lib));
        app.update();
        app
    }

    fn live_names(app: &App) -> Vec<String> {
        let scene = app.world().resource::<VfxScene>();
        scene
            .instances()
            .map(|i| scene.library().def(i.def_index()).name.clone())
            .collect()
    }

    fn parked() -> LayerParked {
        LayerParked {
            restore: regia_scene::components::Visibility::default(),
            inherited: false,
        }
    }

    #[test]
    fn a_parked_static_decal_leaves_the_pool_and_comes_back() {
        let mut app = medium_app(regia_scene::level::AtmospherePreset::Earth, 9.81);
        let prefab = crate::decal_format::DecalPrefab::from_ron(include_str!(
            "../../../docs/formats/decal.example.ron"
        ))
        .expect("example prefab");
        let name = prefab.name.clone();
        app.world_mut().resource_mut::<DecalLibrary>().add(prefab);
        let decal = StaticDecal {
            prefab: name,
            yaw: 0.0,
            receiver_mask: 0,
        };
        let count = |app: &App| app.world().resource::<DecalPool>().len();
        let before = count(&app);

        let entity = app.world_mut().spawn((decal.clone(), Transform::default())).id();
        assert_eq!(count(&app), before + 1, "a live decal is ingested on add");

        app.world_mut().entity_mut(entity).insert(parked());
        assert_eq!(count(&app), before, "parking takes it out of the pool");

        app.world_mut().entity_mut(entity).remove::<LayerParked>();
        assert_eq!(count(&app), before + 1, "resuming puts it back");

        let born_parked = app
            .world_mut()
            .spawn((decal, Transform::default(), parked()))
            .id();
        assert_eq!(count(&app), before + 1, "a member spawned parked is not ingested");
        app.world_mut().entity_mut(born_parked).remove::<LayerParked>();
        assert_eq!(count(&app), before + 2, "it arrives when its layer goes live");
    }

    #[test]
    fn effects_follow_the_level_atmosphere_by_default() {
        use regia_scene::level::AtmospherePreset;
        for (preset, g, expect) in [
            (AtmospherePreset::Airless, 1.315, "explosion@airless"),
            (AtmospherePreset::Mars, 3.71, "explosion@mars"),
            (AtmospherePreset::Earth, 9.81, "explosion"),
        ] {
            let mut app = medium_app(preset, g);
            app.world_mut().write_message(SpawnEffect {
                effect: "explosion".into(),
                position: Vec3::ZERO,
                forward: Vec3::Z,
                seed: Some(1),
            });
            app.update();
            assert_eq!(live_names(&app), vec![expect.to_string()], "{preset:?}");
            let scene = app.world().resource::<VfxScene>();
            assert!((scene.medium().gravity - g).abs() < 1e-5);
            if preset == AtmospherePreset::Airless {
                // The registered variant carries this level's gravity.
                let def = scene
                    .library()
                    .def(scene.library().index(expect).expect("variant"));
                let debris = def.emitters.iter().find(|e| e.name == "debris").expect("debris");
                assert!((debris.update.gravity.1 + g).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn a_games_own_variant_wins_over_the_builtin() {
        use regia_scene::level::AtmospherePreset;
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        let mut level = regia_scene::level::LevelSettings::default();
        level.atmosphere.preset = AtmospherePreset::Airless;
        app.insert_resource(level);
        let mut lib = crate::scene::VfxLibrary::new();
        lib.add(crate::showcase::explosion()).expect("base");
        let mut own = crate::showcase::sparks();
        own.name = "explosion@airless".into();
        lib.add(own).expect("own variant");
        app.insert_resource(VfxScene::new(lib));
        app.update();
        let scene = app.world().resource::<VfxScene>();
        let def = scene
            .library()
            .def(scene.library().index("explosion@airless").expect("variant"));
        assert!(
            def.emitters.iter().any(|e| e.name == "sparks"),
            "the game's variant was replaced"
        );
    }

    #[test]
    fn an_airless_explosion_and_a_glass_breach_play_together() {
        use regia_scene::level::AtmospherePreset;
        let mut app = medium_app(AtmospherePreset::Airless, 1.315);
        app.world_mut().write_message(SpawnEffect {
            effect: "explosion".into(),
            position: Vec3::new(0.0, 0.0, 0.0),
            forward: Vec3::Z,
            seed: Some(3),
        });
        app.world_mut().write_message(crate::glass::GlassBreach {
            position: Vec3::new(2.0, 3.0, 0.0),
            normal: Vec3::Z,
            atmosphere: crate::glass::BreachAtmosphere::Airless,
            pressure: 101.3,
        });
        for _ in 0..4 {
            app.update();
        }
        let names = live_names(&app);
        assert!(names.iter().any(|n| n == "explosion@airless"), "{names:?}");
        assert!(names.iter().any(|n| n == "glass_breach_airless"), "{names:?}");
        let scene = app.world().resource::<VfxScene>();
        // Both schedule particles in the same frame: neither starves the other.
        assert!(scene.estimated_live() > 0);
        let budget: u32 = ["explosion@airless", "glass_breach_airless"]
            .iter()
            .map(|n| {
                scene
                    .library()
                    .def(scene.library().index(n).expect("def"))
                    .system
                    .budget
            })
            .sum();
        assert!(budget <= crate::gpu::VfxProfile::mobile_high().particle_capacity);
    }

    #[test]
    fn plugin_registers_messages() {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        assert!(app.world().contains_resource::<VfxSettings>());
        assert!(app.world().contains_resource::<VfxView>());
        app.update(); // Startup inserts the scene
        assert!(app.world().contains_resource::<VfxScene>());
        assert!(app.world().contains_resource::<DecalPool>());
        assert!(app.world().contains_resource::<DecalLibrary>());
    }

    /// A prefab reload rewrites the static decal placed from it even after an anchored decal in
    /// front of it in the pool has retired (which shifts the dense list).
    #[test]
    fn reload_after_an_anchored_decal_retires_changes_the_right_decal() {
        use crate::decal::{Decal, DecalAnchor, DecalSurface};
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        app.update();
        // An anchored hit decal first, on a hull.
        let hull = app.world_mut().spawn(regia_scene::GlobalTransform::IDENTITY).id();
        let hit = Decal {
            position: Vec3::new(-5.0, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            half_size: Vec3::splat(0.2),
            color: [1.0; 4],
            age: 0.0,
            lifetime: None,
        };
        let hit_id = app.world_mut().resource_mut::<DecalPool>().spawn_anchored(
            hit,
            DecalSurface::from_decal(&hit),
            DecalAnchor {
                entity: hull,
                local_position: hit.position,
                local_rotation: Quat::IDENTITY,
            },
        );
        // Then a static decal placed from the scorch prefab, and a second world decal.
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(4.0, 0.0, 0.0)),
            StaticDecal {
                prefab: "scorch".into(),
                yaw: 0.0,
                receiver_mask: 0,
            },
        ));
        app.update();
        let other = Decal {
            position: Vec3::new(9.0, 0.0, 0.0),
            ..hit
        };
        let other_id = app
            .world_mut()
            .resource_mut::<DecalPool>()
            .spawn_with(other, DecalSurface::from_decal(&other));
        // The hull goes: its decal retires and the dense list shifts.
        app.world_mut().despawn(hull);
        app.update();
        {
            let pool = app.world().resource::<DecalPool>();
            assert!(!pool.contains(hit_id));
            assert_eq!(pool.len(), 2);
        }
        // Reload the scorch prefab with a doubled half size.
        let dir = std::env::temp_dir().join(format!("regia-vfx-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("scorch.decal.ron");
        let text = include_str!("../../../docs/formats/decal.example.ron")
            .replace("half_size: (1.4, 1.4, 0.35)", "half_size: (2.8, 2.8, 0.35)");
        std::fs::write(&path, text).expect("write");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(&path));
        app.update();
        let pool = app.world().resource::<DecalPool>();
        let scorch = pool
            .decals()
            .iter()
            .find(|d| (d.position - Vec3::new(4.0, 0.0, 0.0)).length() < 1e-4)
            .expect("static decal");
        assert!(
            (scorch.half_size.x - 2.8).abs() < 1e-4,
            "the static decal was reloaded"
        );
        let (other_now, _) = pool.get(other_id).expect("other decal");
        assert!(
            (other_now.position - Vec3::new(9.0, 0.0, 0.0)).length() < 1e-4
                && (other_now.half_size.x - 0.2).abs() < 1e-4,
            "the decal after it is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2728: the world is not put back with the live effects, so a static decal whose entity
    /// went away during the stretch does not return as a ghost, and one whose entity came during
    /// it stays in the pool and stays owned.
    #[test]
    fn static_decals_keep_following_their_entities_through_a_live_effects_restore() {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        app.update();
        let spawn = |app: &mut App, x: f32| {
            app.world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                    StaticDecal {
                        prefab: "pad_marking".into(),
                        yaw: 0.0,
                        receiver_mask: 0,
                    },
                ))
                .id()
        };
        let xs = |app: &App| {
            let mut xs: Vec<i32> = app
                .world()
                .resource::<DecalPool>()
                .decals()
                .iter()
                .map(|decal| decal.position.x.round() as i32)
                .collect();
            xs.sort_unstable();
            xs
        };
        let gone = spawn(&mut app, 1.0);
        let stays = spawn(&mut app, 5.0);
        app.update();
        assert_eq!(xs(&app), [1, 5]);
        let capture = crate::live_state::LiveEffectsCapture::capture(app.world_mut());
        app.world_mut().despawn(gone);
        let came = spawn(&mut app, 9.0);
        app.update();
        assert_eq!(xs(&app), [5, 9]);
        capture.restore(app.world_mut());
        assert_eq!(xs(&app), [5, 9], "no ghost at 1, and the decal at 9 stays");
        let done = &app.world().resource::<IngestedStaticDecals>().done;
        assert_eq!(done.len(), 2);
        assert!(done.contains_key(&stays) && done.contains_key(&came));
        // The pool and the entities agree: despawning the newcomer takes its decal out.
        app.world_mut().despawn(came);
        app.update();
        assert_eq!(xs(&app), [5]);
    }

    /// ADR-2728 determinism: a restore walks the captured map and the live map in `Entity` order
    /// (their `HashMap` iteration order is per-process random), so the pool's dense order — the
    /// draw order for equal priorities — and the free-list reuse land the same on every machine.
    /// Eight fresh apps give the live map eight different hash seeds; every run must land the
    /// same dense order.

    #[test]
    fn resync_static_decals_is_independent_of_hash_order() {
        let mut expected: Option<Vec<f32>> = None;
        for _ in 0..8 {
            let mut app = App::new();
            app.add_plugins(VfxPlugin);
            app.update();
            let spawn = |app: &mut App, x: f32| {
                app.world_mut()
                    .spawn((
                        Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                        StaticDecal {
                            prefab: "pad_marking".into(),
                            yaw: 0.0,
                            receiver_mask: 0,
                        },
                    ))
                    .id()
            };
            let old_a = spawn(&mut app, 1.0);
            spawn(&mut app, 2.0);
            app.update();
            // Held render-out job: the live effects are captured (the pool is cloned, the world
            // keeps ticking), then the capture is restored over the world.
            let captured = ingested_static_decals(app.world()).expect("captured");
            let mut pool = app.world().resource::<DecalPool>().clone();
            app.world_mut().despawn(old_a);
            let new_a = spawn(&mut app, 3.0);
            let new_b = spawn(&mut app, 4.0);
            app.update();
            resync_static_decals(app.world_mut(), captured, &mut pool);
            app.world_mut().insert_resource(pool);
            let xs: Vec<f32> = app
                .world()
                .resource::<DecalPool>()
                .decals()
                .iter()
                .map(|decal| decal.position.x)
                .collect();
            // The newcomers spawn in `Entity` sort-key order (the key's direction is bevy's
            // encoding detail; the contract is one fixed order, never the hash map's).
            let mut came = [new_a, new_b];
            came.sort_by_key(|entity| entity.to_bits());
            let want: Vec<f32> = std::iter::once(2.0f32)
                .chain(came.iter().map(|entity| if *entity == new_a { 3.0 } else { 4.0 }))
                .collect();
            assert_eq!(
                xs, want,
                "dense order follows the Entity sort key, not hash order"
            );
            let done = &app.world().resource::<IngestedStaticDecals>().done;
            assert!(done.contains_key(&new_a) && done.contains_key(&new_b));
            assert!(!done.contains_key(&old_a), "the removed entity stays removed");
            if let Some(want) = &expected {
                assert_eq!(&xs, want, "every hash seed lands the same dense order");
            }
            expected = Some(xs);
        }
    }

    /// ADR-4894 (the lost-work review's Med finding): several static decals whose entities come
    /// during a held job are re-ingested at the restore in a deterministic order — ingest order
    /// is the pool's serial order, which same-priority decals draw and recycle in, and the
    /// ingested-entity map's own order differs per process. Eight came decals leave one chance
    /// in forty thousand that a per-process order matches entity order by accident.
    #[test]
    fn static_decals_that_came_during_a_job_ingest_in_entity_order() {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        app.update();
        let spawn = |app: &mut App, x: f32| {
            app.world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                    StaticDecal {
                        prefab: "pad_marking".into(),
                        yaw: 0.0,
                        receiver_mask: 0,
                    },
                ))
                .id()
        };
        let stays = spawn(&mut app, 5.0);
        app.update();
        let capture = crate::live_state::LiveEffectsCapture::capture(app.world_mut());
        // Eight decals stream in during the held job, in this spawn order.
        let came: Vec<_> = (0..8).map(|i| spawn(&mut app, 10.0 + i as f32)).collect();
        app.update();
        capture.restore(app.world_mut());
        // The restore puts the captured pool back first, then the came decals in ingest order:
        // ascending entity bits, whatever the hash map's per-process order was.
        let pool = app.world().resource::<DecalPool>();
        let done = &app.world().resource::<IngestedStaticDecals>().done;
        let ids = pool.ids();
        assert_eq!(ids.len(), 9, "the captured decal and the eight came decals");
        assert_eq!(Some(&done[&stays]), ids.first(), "the captured decal stays first");
        let mut came_by_bits = came.clone();
        came_by_bits.sort_unstable_by_key(|entity| entity.to_bits());
        let expected_tail: Vec<_> = came_by_bits.iter().map(|entity| done[entity]).collect();
        assert_eq!(
            &ids[1..],
            &expected_tail[..],
            "the came decals ingest in ascending entity order"
        );
    }

    #[test]
    fn despawning_a_static_decal_entity_retires_its_pool_decal() {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        app.update();
        let spawn = |app: &mut App, x: f32| {
            app.world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                    StaticDecal {
                        prefab: "pad_marking".into(),
                        yaw: 0.0,
                        receiver_mask: 0,
                    },
                ))
                .id()
        };
        let a = spawn(&mut app, 1.0);
        let b = spawn(&mut app, 5.0);
        app.update();
        assert_eq!(app.world().resource::<DecalPool>().len(), 2);
        app.world_mut().despawn(a);
        app.update();
        let pool = app.world().resource::<DecalPool>();
        assert_eq!(pool.len(), 1, "only the despawned entity's decal retires");
        assert!((pool.decals()[0].position.x - 5.0).abs() < 1e-4);
        assert!(
            app.world()
                .resource::<IngestedStaticDecals>()
                .done
                .contains_key(&b)
        );
        assert!(
            !app.world()
                .resource::<IngestedStaticDecals>()
                .done
                .contains_key(&a)
        );
    }

    #[test]
    fn place_and_static_decal_enter_the_pool_once() {
        use crate::decal::{PlaceDecal, place_decal};
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        app.update();
        let slot = place_decal(
            app.world_mut(),
            &PlaceDecal {
                position: Vec3::new(3.0, 1.0, 0.0),
                normal: Vec3::Y,
                yaw: 0.0,
                prefab: "scorch".into(),
                lifetime: None,
            },
        );
        assert!(slot.is_some());
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(1.0, 0.0, 2.0)),
            StaticDecal {
                prefab: "pad_marking".into(),
                yaw: 0.0,
                receiver_mask: 0,
            },
        ));
        app.update();
        {
            let pool = app.world().resource::<DecalPool>();
            assert_eq!(pool.len(), 2, "place message and static decal both spawn");
            assert!(
                pool.decals()
                    .iter()
                    .any(|decal| (decal.position - Vec3::new(1.0, 0.0, 2.0)).length() < 1e-4),
                "static decal keeps the entity translation"
            );
        }
        app.update();
        assert_eq!(
            app.world().resource::<DecalPool>().len(),
            2,
            "a static decal is ingested once"
        );
    }

    /// ADR-3275 end to end: with caps set, the plugin hands the frame's `VfxView` to the ranking
    /// before the tick, so an effect behind the camera pauses and spawns nothing, and one in front
    /// plays; without caps the view is never copied.
    #[test]
    fn the_plugin_feeds_the_view_to_the_significance_ranking() {
        let mut app = medium_app(regia_scene::level::AtmospherePreset::Earth, 9.81);
        // Fixed 1/32 s frames: no wall clock.
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 32.0),
        ));
        assert!(
            app.world().resource::<VfxScene>().significance().view().is_none(),
            "no caps: the view is not copied"
        );
        app.insert_resource(VfxView {
            camera: VfxCamera::looking_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0)),
            width: 1280,
            height: 720,
        });
        app.world_mut()
            .resource_mut::<VfxScene>()
            .significance_mut()
            .set_caps(crate::quality::significance_caps(regia_render::QualityTier::Low));
        for (z, seed) in [(-20.0, 1), (60.0, 2)] {
            app.world_mut().write_message(SpawnEffect {
                effect: "smoke".into(),
                position: Vec3::new(0.0, 0.0, z),
                forward: Vec3::Y,
                seed: Some(seed),
            });
        }
        for _ in 0..8 {
            app.update();
        }
        let scene = app.world().resource::<VfxScene>();
        assert!(
            scene.significance().view().is_some(),
            "the plugin copied the view"
        );
        let state = |z: f32| {
            let instance = scene
                .instances()
                .find(|i| (i.origin().z - z).abs() < 1e-3)
                .expect("instance");
            (
                scene.significance().state_of(instance.id()),
                instance.live_particles(),
            )
        };
        let (front, front_live) = state(-20.0);
        let (behind, behind_live) = state(60.0);
        assert_eq!(front, Some(crate::significance::SignificanceState::Full));
        assert_eq!(behind, Some(crate::significance::SignificanceState::Paused));
        assert_eq!(behind_live, 0, "the effect behind the camera spawned");
        assert!(front_live > 0, "the effect in front did not play");
    }

    /// Spec 14 § A1 (ADR-7200): a hidden (not parked) [`StaticDecal`] stops drawing, but its decal
    /// stays in the pool, which is rollback state with ordering rules (ADR-2728, ADR-4894). The
    /// filter is at draw time: the pool of a world that hid a decal is byte for byte the pool of a
    /// world that never did, and the draw list leaves the hidden decal out until it shows again.
    #[test]
    fn a_hidden_static_decal_leaves_the_draw_list_but_not_the_pool() {
        let build = || {
            let mut app = App::new();
            // A fixed step: both worlds age their decals by the same amount each frame, so the
            // pools can be compared byte for byte (a wall-clock step would differ per app).
            app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
            app.add_plugins(VfxPlugin);
            app.update();
            let ids: Vec<Entity> = [1.0, 5.0, 9.0]
                .into_iter()
                .map(|x| {
                    app.world_mut()
                        .spawn((
                            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                            StaticDecal {
                                prefab: "pad_marking".into(),
                                yaw: 0.0,
                                receiver_mask: 0,
                            },
                        ))
                        .id()
                })
                .collect();
            app.update();
            (app, ids)
        };
        let drawn_xs = |app: &App| {
            let pool = app.world().resource::<DecalPool>();
            let hidden = app.world().resource::<crate::decal::HiddenStaticDecals>();
            let mut scratch = crate::decal::DecalDrawScratch::default();
            let (decals, surfaces) = pool.visible_in_spawn_order(hidden.ids(), &mut scratch);
            assert_eq!(decals.len(), surfaces.len());
            decals
                .iter()
                .map(|decal| decal.position.x.round() as i32)
                .collect::<Vec<_>>()
        };
        let snapshot = |app: &App| format!("{:?}", app.world().resource::<DecalPool>());
        let (mut control, _) = build();
        let (mut app, ids) = build();
        assert_eq!(drawn_xs(&app), [1, 5, 9]);

        app.world_mut()
            .entity_mut(ids[1])
            .insert(regia_scene::components::Visibility::Hidden);
        app.update();
        control.update();
        assert_eq!(drawn_xs(&app), [1, 9], "the hidden decal is not drawn");
        assert_eq!(drawn_xs(&control), [1, 5, 9]);
        assert_eq!(
            snapshot(&app),
            snapshot(&control),
            "hiding left the pool (rollback state) untouched"
        );

        // A decal under a hidden ancestor is hidden too (its resolved `InheritedVisibility`).
        app.world_mut()
            .entity_mut(ids[0])
            .insert(regia_scene::components::InheritedVisibility(false));
        app.update();
        control.update();
        assert_eq!(drawn_xs(&app), [9]);
        assert_eq!(snapshot(&app), snapshot(&control));

        // Shown again, both draw again in their old (spawn) order, from the same pool.
        app.world_mut()
            .entity_mut(ids[1])
            .insert(regia_scene::components::Visibility::Visible);
        app.world_mut()
            .entity_mut(ids[0])
            .insert(regia_scene::components::InheritedVisibility(true));
        app.update();
        control.update();
        assert_eq!(drawn_xs(&app), [1, 5, 9]);
        assert_eq!(snapshot(&app), snapshot(&control));
    }

    /// The draw-time filter allocates nothing once its scratch has grown, and with nothing hidden
    /// it borrows the pool's own arrays.
    #[test]
    fn the_visible_decal_list_borrows_when_nothing_is_hidden() {
        let mut pool = DecalPool::new(8);
        let decal = crate::decal::Decal {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_size: Vec3::ONE,
            color: [1.0; 4],
            age: 0.0,
            lifetime: None,
        };
        let a = pool.spawn_with(decal, crate::decal::DecalSurface::from_decal(&decal));
        let _b = pool.spawn_with(decal, crate::decal::DecalSurface::from_decal(&decal));
        let mut scratch = crate::decal::DecalDrawScratch::default();
        {
            let (decals, _) = pool.visible_in_spawn_order(&[], &mut scratch);
            assert!(matches!(decals, std::borrow::Cow::Borrowed(_)));
            assert_eq!(decals.len(), 2);
        }
        let (decals, surfaces) = pool.visible_in_spawn_order(&[a], &mut scratch);
        assert_eq!((decals.len(), surfaces.len()), (1, 1));
    }
}
