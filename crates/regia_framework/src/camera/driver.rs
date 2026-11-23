// SPDX-License-Identifier: MPL-2.0
//! The camera driver: the component that evaluates one [`CameraAsset`]
//! for one camera entity, the asset registry, and the system (`docs/specs/35-camera-framework.md`
//! §§ 8, 11).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::components::Name;

use super::CinematicCamera;
#[cfg(test)]
use super::asset::VarValue;
use super::asset::{CameraAsset, CameraBuildStatus, CameraError, CameraIssue, TargetRef};
use super::director::CameraHooks;
use super::nodes::{CameraPose, CameraWorld};
use super::runtime::{DriverCore, LayerEventKind, LayerId};

/// A layer event written as a message in evaluation order (§ 7).
#[derive(Message, Clone, Debug, PartialEq)]
pub struct CameraLayerEvent {
    /// The camera entity of the driver.
    pub driver: Entity,
    /// The layer.
    pub id: LayerId,
    /// The layer's rig name.
    pub rig: String,
    /// What happened.
    pub kind: LayerEventKind,
}

/// One name in the per-frame camera target index. `count` is how many entities share the name;
/// the pose is the one on the lowest entity id.
struct NamedTarget {
    id: u64,
    position: Vec3,
    rotation: Quat,
    count: u32,
}

struct AssetEntry {
    path: Option<String>,
    working: CameraAsset,
    active: Option<Arc<CameraAsset>>,
    issues: Vec<CameraIssue>,
    status: CameraBuildStatus,
    stale: bool,
    revision: u32,
}

/// The loaded camera assets by name, with their build status (§ 8). A driver names an asset;
/// an asset with validation errors never replaces the previous good version.
#[derive(Resource, Default)]
pub struct CameraAssets {
    entries: BTreeMap<String, AssetEntry>,
}

impl CameraAssets {
    /// Validates and stores an asset under its `name`. Without errors it becomes the active
    /// version drivers run; with errors the previous good version (if any) keeps running.
    pub fn insert(
        &mut self,
        asset: CameraAsset,
        path: Option<String>,
        hooks: &CameraHooks,
    ) -> CameraBuildStatus {
        let issues = asset.validate(&|name| hooks.contains(name));
        let status = CameraBuildStatus::of(&issues);
        let name = asset.name.clone();
        let entry = self.entries.entry(name).or_insert_with(|| AssetEntry {
            path: None,
            working: asset.clone(),
            active: None,
            issues: Vec::new(),
            status: CameraBuildStatus::Ok,
            stale: false,
            revision: 0,
        });
        entry.path = path.or_else(|| entry.path.take());
        entry.working = asset;
        entry.issues = issues;
        entry.status = status;
        entry.stale = false;
        if status != CameraBuildStatus::Errors {
            entry.active = Some(Arc::new(entry.working.clone()));
            entry.revision += 1;
        }
        status
    }

    /// Parses `.camera.ron` text and stores it (see [`insert`](Self::insert)).
    pub fn load_ron(
        &mut self,
        text: &str,
        path: Option<String>,
        hooks: &CameraHooks,
    ) -> Result<CameraBuildStatus, CameraError> {
        let asset = CameraAsset::from_ron(text)?;
        Ok(self.insert(asset, path, hooks))
    }

    /// Edits the working copy of an asset; its status becomes `Stale` until
    /// [`revalidate`](Self::revalidate). Returns false for an unknown name.
    pub fn edit(&mut self, name: &str, edit: impl FnOnce(&mut CameraAsset)) -> bool {
        let Some(entry) = self.entries.get_mut(name) else {
            return false;
        };
        edit(&mut entry.working);
        entry.stale = true;
        true
    }

    /// Validates the working copy again and activates it when it has no errors.
    pub fn revalidate(&mut self, name: &str, hooks: &CameraHooks) -> Option<CameraBuildStatus> {
        let working = self.entries.get(name)?.working.clone();
        let path = self.entries.get(name)?.path.clone();
        let old_name = name.to_owned();
        let status = self.insert(working, path, hooks);
        // A rename inside an edit keeps the entry under the old key too.
        if let Some(entry) = self.entries.get_mut(&old_name) {
            entry.stale = false;
        }
        Some(status)
    }

    /// The status of an asset (`Stale` after an edit).
    pub fn status(&self, name: &str) -> Option<CameraBuildStatus> {
        self.entries.get(name).map(|e| {
            if e.stale {
                CameraBuildStatus::Stale
            } else {
                e.status
            }
        })
    }

    /// The issues of the last validation.
    pub fn issues(&self, name: &str) -> Option<&[CameraIssue]> {
        self.entries.get(name).map(|e| e.issues.as_slice())
    }

    /// The asset drivers run (the last good version).
    pub fn active(&self, name: &str) -> Option<&Arc<CameraAsset>> {
        self.entries.get(name).and_then(|e| e.active.as_ref())
    }

    /// The working copy.
    pub fn working(&self, name: &str) -> Option<&CameraAsset> {
        self.entries.get(name).map(|e| &e.working)
    }

    /// Bumps each time a new good version becomes active.
    pub fn revision(&self, name: &str) -> u32 {
        self.entries.get(name).map_or(0, |e| e.revision)
    }

    /// The path an asset was loaded from.
    pub fn path(&self, name: &str) -> Option<&str> {
        self.entries.get(name).and_then(|e| e.path.as_deref())
    }

    /// Asset names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Evaluates one camera asset for the camera entity it sits on (§ 8). A driver on an entity
/// disables the legacy [`CameraRig`](super::CameraRig) update for that entity.
///
/// ```
/// use regia_framework::camera::driver::CameraDriver;
///
/// let driver = CameraDriver::new("third_person").with_seed(7);
/// assert!(driver.enabled);
/// assert!(driver.core().is_none(), "bound by the system once the asset is loaded");
/// ```
#[derive(Component)]
pub struct CameraDriver {
    /// The asset's name in [`CameraAssets`].
    pub asset: String,
    /// The entity the `DriveTarget` resolves to.
    pub target: Option<Entity>,
    /// Which local player's input and target this driver serves.
    pub local_player: u8,
    /// Seed of the deterministic noise (shake nodes).
    pub seed: u64,
    /// A driver evaluates as soon as it is enabled and its asset is valid.
    pub enabled: bool,
    core: Option<DriverCore>,
    bound_revision: u32,
}

impl CameraDriver {
    /// A driver for the asset called `asset`, enabled, with no target.
    pub fn new(asset: impl Into<String>) -> Self {
        Self {
            asset: asset.into(),
            target: None,
            local_player: 0,
            seed: 0,
            enabled: true,
            core: None,
            bound_revision: 0,
        }
    }

    /// Sets the target entity.
    pub fn with_target(mut self, target: Entity) -> Self {
        self.target = Some(target);
        self
    }

    /// Sets the noise seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// The evaluation state, once bound.
    pub fn core(&self) -> Option<&DriverCore> {
        self.core.as_ref()
    }

    /// The evaluation state, once bound (mutable: set variables, inputs, layers).
    pub fn core_mut(&mut self) -> Option<&mut DriverCore> {
        self.core.as_mut()
    }

    /// The evaluation state or `camera_asset_invalid`: the asset is not loaded or has errors.
    pub fn bound_mut(&mut self) -> Result<&mut DriverCore, CameraError> {
        let name = self.asset.clone();
        self.core.as_mut().ok_or(CameraError::Invalid(name))
    }

    /// Drops the evaluation state; the system binds the (new) asset on the next frame.
    pub fn unbind(&mut self) {
        self.core = None;
        self.bound_revision = 0;
    }

    /// Binds (or rebinds, when a new good version of the asset became active) the core. Variables
    /// and overrides carry over by name and type.
    pub fn bind(&mut self, assets: &CameraAssets) {
        let revision = assets.revision(&self.asset);
        if revision == 0 || revision == self.bound_revision {
            return;
        }
        let Some(asset) = assets.active(&self.asset) else {
            return;
        };
        let mut core = DriverCore::new(Arc::clone(asset), self.seed, self.local_player);
        if let Some(old) = self.core.take() {
            for (i, name) in old.vars.names.iter().enumerate() {
                let _ = core.vars.set(name, old.vars.values[i]);
            }
            core.vars.end_frame();
            for (name, value) in old.overrides() {
                let _ = core.set_param(name, value.clone());
            }
        }
        self.core = Some(core);
        self.bound_revision = revision;
    }
}

/// Resolves targets and physics for one driver for one frame.
struct EcsWorld<'a> {
    drive: Option<(Vec3, Quat)>,
    named: &'a dyn Fn(&str) -> Option<(Vec3, Quat)>,
    physics: Option<&'a regia_physics::PhysicsWorld>,
    layers: Option<&'a regia_physics::PhysicsLayers>,
    exclude: Option<Entity>,
    /// Builds the world-space curve of the named spline entity (rail cameras, ADR-3231).
    rail: &'a dyn Fn(&str) -> Option<Arc<regia_scene::spline::SplineCurve>>,
}

impl CameraWorld for EcsWorld<'_> {
    fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
        match target {
            TargetRef::DriveTarget => self.drive,
            TargetRef::Entity(name) => (self.named)(name),
            TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
        }
    }

    fn rail(&self, name: &str) -> Option<Arc<regia_scene::spline::SplineCurve>> {
        (self.rail)(name)
    }

    fn cast_sphere(
        &self,
        from: Vec3,
        dir: Vec3,
        max: f32,
        radius: f32,
        layer_names: &[String],
    ) -> Option<f32> {
        let world = self.physics?;
        if dir.length_squared() < 1.0e-8 {
            return None;
        }
        let queries = regia_physics::queries::WorldQueries {
            world,
            layers: self.layers,
        };
        let excluded: [Entity; 1] = [self.exclude.unwrap_or(Entity::PLACEHOLDER)];
        let mut filter = regia_physics::queries::QueryFilter::new()
            .exclude_sensors()
            .exclude_entities(&excluded);
        if let Some(layers) = self.layers {
            match layer_mask(layers, layer_names) {
                LayerMask::All => {}
                LayerMask::Some(mask) => filter = filter.layers(mask),
                LayerMask::Nothing => {
                    tracing::debug!(target: "regia_framework", ?layer_names, "camera collision layers resolve to nothing");
                    return None;
                }
            }
        }
        queries
            .shapecast(
                &regia_physics::ColliderShape::Sphere { radius },
                Transform {
                    translation: from,
                    ..Transform::default()
                },
                dir,
                max,
                filter,
            )
            .map(|hit| hit.time_of_impact)
    }
}

/// How a collision layer-name list resolves against the physics layer table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerMask {
    /// No names listed: every layer.
    All,
    /// The mask of the names that resolved.
    Some(u32),
    /// Names were listed and none resolved: the cast hits nothing rather than everything.
    Nothing,
}

/// Resolves `names` against `layers`; unknown names are dropped.
fn layer_mask(layers: &regia_physics::PhysicsLayers, names: &[String]) -> LayerMask {
    if names.is_empty() {
        return LayerMask::All;
    }
    let mask = names
        .iter()
        .filter_map(|n| layers.index_of(n))
        .fold(0u32, |m, i| m | (1u32 << (i % 32)));
    if mask == 0 {
        LayerMask::Nothing
    } else {
        LayerMask::Some(mask)
    }
}

/// Evaluates every enabled [`CameraDriver`] and writes its camera's [`Transform`] and field of
/// view (§ 8 steps 1-6). Director shakes apply afterwards in [`CameraSet::Director`](super::CameraSet).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
#[tracing::instrument(skip_all, target = "regia_framework", name = "cameras_drive")]
pub(super) fn drive_cameras(
    time: Res<Time>,
    assets: Res<CameraAssets>,
    hooks: Res<CameraHooks>,
    physics: Option<Res<regia_physics::PhysicsWorld>>,
    physics_layers: Option<Res<regia_physics::PhysicsLayers>>,
    mut commands: Commands,
    mut events: MessageWriter<CameraLayerEvent>,
    mut drivers: Query<(
        Entity,
        &mut CameraDriver,
        &mut Transform,
        Option<&mut regia_render::Camera>,
        Has<CinematicCamera>,
    )>,
    targets: Query<&Transform, Without<CameraDriver>>,
    names: Query<(Entity, &Name, &Transform), Without<CameraDriver>>,
    splines: Query<(
        Entity,
        &Name,
        &regia_scene::spline::Spline,
        &regia_scene::GlobalTransform,
    )>,
    motion: Option<Res<crate::settings::SceneMotion>>,
    // Duplicate-name warnings already emitted, per (driver, name): the lookup runs per Pivot /
    // LookAt node per frame, and a shipped scene with two "hero" entities must not log 60+
    // warnings per second (Muse read-only review of fix-framework1-determinism, medium finding 4).
    mut warned_names: Local<Vec<(Entity, Box<str>)>>,
) {
    let dt = time.delta_secs();
    // No enabled driver: nothing to evaluate, so no per-frame name index either (spec 35 § 8, no
    // steady-state allocation in a game that has the plugin but no driven camera; ADR-5778). The
    // warning latch is taken only after this return, so a disabled frame keeps it.
    if !drivers.iter().any(|(_, driver, ..)| driver.enabled) {
        return;
    }
    let warned = std::cell::RefCell::new(std::mem::take(&mut *warned_names));
    // One walk of the name query per frame. A driver that looks a name up (a pivot and a frame
    // can both name the same target) reads this index. Several entities may share a name; the
    // stable key is the entity id, never the archetype row order (spec 35 § 12.13).
    #[cfg(test)]
    name_scans::inc();
    let mut name_index: BTreeMap<&str, NamedTarget> = BTreeMap::new();
    for (entity, name, transform) in names.iter() {
        let key = entity.to_bits();
        match name_index.get_mut(name.as_str()) {
            Some(best) => {
                best.count = best.count.saturating_add(1);
                if key < best.id {
                    best.id = key;
                    best.position = transform.translation;
                    best.rotation = transform.rotation;
                }
            }
            None => {
                name_index.insert(
                    name.as_str(),
                    NamedTarget {
                        id: key,
                        position: transform.translation,
                        rotation: transform.rotation,
                        count: 1,
                    },
                );
            }
        }
    }
    for (entity, mut driver, mut transform, camera, shakeable) in &mut drivers {
        if !driver.enabled {
            continue;
        }
        driver.bind(&assets);
        let target = driver.target;
        let Some(core) = driver.core.as_mut() else {
            continue;
        };
        let drive = target
            .and_then(|t| targets.get(t).ok())
            .map(|t| (t.translation, t.rotation));
        let named = |wanted: &str| {
            let hit = name_index.get(wanted)?;
            if hit.count > 1 {
                // Once per (driver, name), not per lookup per frame (fixrev-10).
                let mut warned = warned.borrow_mut();
                if !warned.iter().any(|(e, n)| *e == entity && &**n == wanted) {
                    warned.push((entity, wanted.into()));
                    tracing::warn!(target: "regia_framework", name = wanted, count = hit.count, "camera name target matches more than one entity; using the lowest entity id");
                }
            }
            Some((hit.position, hit.rotation))
        };
        let rails: RefCell<BTreeMap<String, Option<Arc<regia_scene::spline::SplineCurve>>>> =
            RefCell::default();
        let rail = |wanted: &str| {
            rails
                .borrow_mut()
                .entry(wanted.to_owned())
                .or_insert_with(|| {
                    // Several splines may share a name. The stable key is the entity id, the
                    // same rule as a named target above (spec 35 § 12.13), never the archetype
                    // row order, so a despawn cannot change which rail the camera rides.
                    let mut best: Option<(u64, &regia_scene::spline::Spline, &regia_scene::GlobalTransform)> =
                        None;
                    let mut count = 0u32;
                    for (entity, name, spline, global) in splines.iter() {
                        if name.as_str() != wanted {
                            continue;
                        }
                        count += 1;
                        let key = entity.to_bits();
                        match best {
                            Some((k, _, _)) if k <= key => {}
                            _ => best = Some((key, spline, global)),
                        }
                    }
                    if count > 1 {
                        tracing::warn!(
                            target: "regia_framework",
                            name = wanted,
                            count,
                            "camera rail matches more than one spline; using the lowest entity id"
                        );
                    }
                    let (_, spline, global) = best?;
                    if spline.validate().is_err() {
                        return None;
                    }
                    Some(Arc::new(regia_scene::spline::SplineCurve::new(
                        spline,
                        Some(&global.0),
                    )))
                })
                .clone()
        };
        let world = EcsWorld {
            drive,
            named: &named,
            rail: &rail,
            physics: physics.as_deref(),
            layers: physics_layers.as_deref(),
            exclude: target,
        };
        let mut seed = CameraPose {
            position: transform.translation,
            fov_deg: camera
                .as_ref()
                .map_or(super::nodes::DEFAULT_FOV_DEG, |c| c.fov_y.to_degrees()),
            ..CameraPose::default()
        };
        seed.set_rotation(transform.rotation);
        core.set_entity_bits(entity.to_bits());
        let pose = core.step(dt, &world, &hooks, seed);
        // The core never returns a non-finite pose; the check keeps a NaN out of the camera's
        // Transform even if that changes (audit-v035-numerics M5).
        if pose.is_finite() {
            transform.translation = pose.position;
            transform.rotation = pose.rotation();
            if let Some(mut camera) = camera {
                let fov_deg = motion
                    .as_ref()
                    .map(|motion| motion.fov_deg(pose.fov_deg))
                    .unwrap_or(pose.fov_deg);
                camera.fov_y = fov_deg.to_radians();
            }
        }
        for event in core.events.drain(..) {
            events.write(CameraLayerEvent {
                driver: entity,
                id: event.id,
                rig: event.rig,
                kind: event.kind,
            });
        }
        if !shakeable {
            // The driven camera feels the director's shakes like a cinematic camera (ADR-1637).
            commands.entity(entity).try_insert(CinematicCamera);
        }
    }
    *warned_names = warned.into_inner();
}

/// Full walks of the named-entity query inside one `drive_cameras` (one per frame).
#[cfg(test)]
pub(crate) mod name_scans {
    use std::cell::Cell;

    thread_local! {
        static SCANS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        SCANS.with(|c| c.set(0));
    }

    pub(crate) fn get() -> u64 {
        SCANS.with(|c| c.get())
    }

    pub(crate) fn inc() {
        SCANS.with(|c| c.set(c.get() + 1));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::camera::asset::DirectorDef;

    #[test]
    fn unresolved_layer_names_hit_nothing_not_everything() {
        let layers = regia_physics::PhysicsLayers::default();
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(layer_mask(&layers, &[]), LayerMask::All);
        assert_eq!(layer_mask(&layers, &names(&["Static"])), LayerMask::Nothing);
        assert_eq!(
            layer_mask(&layers, &names(&["Static", "default"])),
            LayerMask::Some(1)
        );
    }

    const GOOD: &str = r#"(format_version: 1, name: "g",
        rigs: [(name: "a", root: [Fov(degrees: Literal(45.0))])],
        director: Fixed("a"))"#;

    fn hooks() -> CameraHooks {
        CameraHooks::default()
    }

    #[test]
    fn a_good_asset_loads_and_a_bad_edit_goes_stale_then_errors_without_replacing_it() {
        let mut assets = CameraAssets::default();
        let status = assets
            .load_ron(GOOD, Some("g.camera.ron".into()), &hooks())
            .expect("parses");
        assert_eq!(status, CameraBuildStatus::Ok);
        assert_eq!(assets.status("g"), Some(CameraBuildStatus::Ok));
        assert_eq!(assets.revision("g"), 1);
        assert_eq!(assets.path("g"), Some("g.camera.ron"));
        assert!(assets.edit("g", |a| a.director = DirectorDef::Fixed("ghost".into())));
        assert_eq!(
            assets.status("g"),
            Some(CameraBuildStatus::Stale),
            "edited since the last validation"
        );
        assert_eq!(assets.revision("g"), 1, "an edit alone loads nothing");
        assert_eq!(assets.revalidate("g", &hooks()), Some(CameraBuildStatus::Errors));
        assert_eq!(assets.status("g"), Some(CameraBuildStatus::Errors));
        assert_eq!(assets.revision("g"), 1, "errors prevent loading");
        assert!(matches!(
            assets.active("g").expect("previous good version").director,
            DirectorDef::Fixed(ref rig) if rig == "a"
        ));
        assert!(
            assets
                .issues("g")
                .is_some_and(|i| i.iter().any(|x| x.code == "camera_unknown_rig"))
        );
        assert!(assets.edit("g", |a| a.director = DirectorDef::Fixed("a".into())));
        assert_eq!(assets.revalidate("g", &hooks()), Some(CameraBuildStatus::Ok));
        assert_eq!(assets.revision("g"), 2);
        assert!(!assets.edit("nope", |_| {}));
        assert_eq!(assets.revalidate("nope", &hooks()), None);
        assert_eq!(assets.names().collect::<Vec<_>>(), ["g"]);
    }

    #[test]
    fn parse_errors_surface_and_hooks_are_checked_against_the_registry() {
        let mut assets = CameraAssets::default();
        assert!(assets.load_ron("(not ron", None, &hooks()).is_err());
        let hooked = GOOD.replace("Fixed(\"a\")", "Hook(\"h\")");
        assert_eq!(
            assets.load_ron(&hooked, None, &hooks()),
            Ok(CameraBuildStatus::Errors)
        );
        let mut with_hook = CameraHooks::default();
        with_hook.register("h", |_| vec![]);
        assert_eq!(
            assets.load_ron(&hooked, None, &with_hook),
            Ok(CameraBuildStatus::Ok)
        );
    }

    #[test]
    fn a_driver_binds_once_and_rebinds_on_a_new_good_version_keeping_its_values() {
        let text = r#"(format_version: 1, name: "g",
            params: {"len": Float(4.0)},
            variables: [(name: "v", default: Float(0.0))],
            rigs: [(name: "a", root: [Boom(length: Param("len"))])],
            director: Fixed("a"))"#;
        let mut assets = CameraAssets::default();
        assert_eq!(assets.load_ron(text, None, &hooks()), Ok(CameraBuildStatus::Ok));
        let mut driver = CameraDriver::new("g");
        assert!(driver.bound_mut().is_err(), "not bound before the first frame");
        driver.bind(&assets);
        {
            let core = driver.bound_mut().expect("bound");
            core.set_var("v", VarValue::Float(2.0)).expect("var");
            core.set_param("len", crate::camera::asset::ParamValue::Float(9.0))
                .expect("param");
        }
        driver.bind(&assets);
        assert_eq!(
            driver.core().expect("bound").vars.get("v"),
            Some(VarValue::Float(2.0))
        );
        assert_eq!(assets.load_ron(text, None, &hooks()), Ok(CameraBuildStatus::Ok));
        driver.bind(&assets);
        let core = driver.core().expect("still bound");
        assert_eq!(
            core.vars.get("v"),
            Some(VarValue::Float(2.0)),
            "variables carry over by name"
        );
        assert_eq!(
            core.param("len"),
            Some(&crate::camera::asset::ParamValue::Float(9.0))
        );
        driver.unbind();
        assert!(driver.core().is_none());
    }

    /// A `TargetRef::Entity` shared by several entities resolves to the lowest entity id, never
    /// the archetype row order: run 2 despawn-swaps the table so the first row is not the
    /// lowest-id hero, and both runs must still frame the same one (spec 35 § 12.13).
    #[test]
    fn a_duplicate_target_name_resolves_to_the_lowest_entity_id_not_the_row_order() {
        use std::time::Duration;

        use crate::camera::CameraPlugin;

        const ASSET: &str = r#"(format_version: 1, name: "dup",
            rigs: [(name: "a", root: [Pivot(target: Entity("hero"), offset: (0.0, 0.0, 0.0),
                space: World, aim_offset: (0.0, 0.0, 0.0), inherit_rotation: false)])],
            director: Fixed("a"))"#;
        let far = Vec3::new(20.0, 0.0, 0.0);

        let run = |perturb: bool| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, CameraPlugin))
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
            assert_eq!(
                app.world_mut()
                    .resource_mut::<CameraAssets>()
                    .load_ron(ASSET, None, &hooks()),
                Ok(CameraBuildStatus::Ok)
            );
            let near = Vec3::new(1.0, 0.0, 0.0);
            let spawn_hero = |app: &mut App, at: Vec3| {
                app.world_mut()
                    .spawn((Transform::from_translation(at), Name::new("hero")))
                    .id()
            };
            if perturb {
                // Spawn near, far, then a second near; despawning the first swap-removes the last
                // row into the hole, so row order (near, far) is the reverse of id order.
                let h1 = spawn_hero(&mut app, near);
                spawn_hero(&mut app, far);
                spawn_hero(&mut app, near);
                let _ = app.world_mut().despawn(h1);
            } else {
                spawn_hero(&mut app, far);
                spawn_hero(&mut app, near);
            }
            let camera = app
                .world_mut()
                .spawn(Transform::default())
                .insert(CameraDriver::new("dup"))
                .id();
            // The expected framing: the hero with the lowest entity bits (bevy allocates entity
            // indices in its own order, so resolve the expectation from the world, not from the
            // spawn sequence).
            let heroes: Vec<(u64, Vec3)> = {
                let world = app.world_mut();
                let mut q = world.query_filtered::<(Entity, &Transform), With<Name>>();
                q.iter(world).map(|(e, t)| (e.to_bits(), t.translation)).collect()
            };
            let expected = heroes
                .iter()
                .min_by_key(|(bits, _)| *bits)
                .map_or(0.0f32, |(_, p)| p.x);
            app.update();
            let got = app
                .world()
                .get::<Transform>(camera)
                .map_or(0, |t| t.translation.x.to_bits());
            (got, expected.to_bits())
        };

        let (plain, expected_plain) = run(false);
        let (perturbed, expected_perturbed) = run(true);
        assert_eq!(
            plain, perturbed,
            "both runs frame the lowest-id hero regardless of table order"
        );
        assert_eq!(
            plain, expected_plain,
            "the framed hero is the lowest-entity-id one"
        );
        assert_eq!(plain, expected_perturbed);
    }

    /// With no enabled driver the system returns before it walks the named entities (lane
    /// spec-15-35, ADR-5778); one enabled driver brings the single walk back.
    #[test]
    fn no_enabled_driver_builds_no_name_index() {
        use std::time::Duration;

        use crate::camera::CameraPlugin;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
            // The scan counter is a thread-local (see `named_targets_are_resolved_from_one_index_per_frame`).
            .set_schedule_executor(regia_app::ScheduleExecutor::SingleThreaded);
        for i in 0..8 {
            app.world_mut()
                .spawn((Transform::default(), Name::new(format!("named{i}"))));
        }
        name_scans::reset();
        app.update();
        assert_eq!(name_scans::get(), 0, "no driver: no walk");
        let mut disabled = CameraDriver::new("missing");
        disabled.enabled = false;
        let camera = app.world_mut().spawn((Transform::default(), disabled)).id();
        app.update();
        assert_eq!(name_scans::get(), 0, "a disabled driver: no walk");
        app.world_mut()
            .get_mut::<CameraDriver>(camera)
            .expect("driver")
            .enabled = true;
        app.update();
        assert_eq!(name_scans::get(), 1, "an enabled driver: one walk per frame");
    }

    /// Pivot and Frame both name "hero". One frame builds the name index once, then looks up.
    #[test]
    fn named_targets_are_resolved_from_one_index_per_frame() {
        use std::time::Duration;

        use crate::camera::CameraPlugin;
        use regia_core::math::det_quat_from_yaw_pitch_roll;
        use regia_core::math::det_yaw_pitch_roll;

        const ASSET: &str = r#"(format_version: 1, name: "idx",
            rigs: [(name: "a", root: [
                Pivot(target: Entity("hero"), offset: (0.0, 0.0, 5.0), space: World,
                    aim_offset: (0.0, 0.0, 0.0), inherit_rotation: false),
                Frame(target: Entity("hero"), dead_zone_deg: (0.0, 0.0),
                    soft_zone_deg: (0.0, 0.0), speed: 1000000.0)
            ])],
            director: Fixed("a"))"#;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
            // The scan counter is a thread-local. The desktop executor runs systems on the
            // compute pool (ADR-4933), so this test pins the schedule to the calling thread.
            .set_schedule_executor(regia_app::ScheduleExecutor::SingleThreaded);
        assert_eq!(
            app.world_mut()
                .resource_mut::<CameraAssets>()
                .load_ron(ASSET, None, &hooks()),
            Ok(CameraBuildStatus::Ok)
        );
        for i in 0..48 {
            app.world_mut().spawn((
                Transform::from_translation(Vec3::new(i as f32, 3.0, -2.0)),
                Name::new(format!("decoy{i}")),
            ));
        }
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            Name::new("hero"),
        ));
        // Yaw 0 faces −Z. Start at π/2 (facing −X) so Frame must resolve "hero" to snap back.
        let yaw0 = std::f32::consts::FRAC_PI_2;
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_rotation(det_quat_from_yaw_pitch_roll(yaw0, 0.0, 0.0)),
                CameraDriver::new("idx"),
            ))
            .id();
        name_scans::reset();
        app.update();
        let scans = name_scans::get();
        let pose = app.world().get::<Transform>(camera).expect("camera");
        let (yaw, pitch, _) = det_yaw_pitch_roll(pose.rotation);
        assert!(
            (pose.translation.x - 10.0).abs() < 1.0e-3
                && (pose.translation.y).abs() < 1.0e-3
                && (pose.translation.z - 5.0).abs() < 1.0e-3,
            "pivot should sit at hero + (0, 0, 5), got {:?}",
            pose.translation
        );
        assert!(
            yaw.abs() < 1.0e-3 && pitch.abs() < 1.0e-3,
            "frame should snap yaw to 0 (looking −Z at the hero), yaw {yaw} pitch {pitch}"
        );
        assert_eq!(
            scans, 1,
            "pivot and frame share one name-query walk per frame, walked {scans}"
        );
    }

    /// A rail name shared by several splines resolves to the lowest entity id, never the
    /// archetype row order (spec 35 § 12.13). The named-target path already does; the rail
    /// lookup must do the same.
    #[test]
    fn a_duplicate_spline_name_resolves_to_the_lowest_entity_id_not_the_row_order() {
        use std::time::Duration;

        use regia_core::glam::Affine3A;
        use regia_scene::GlobalTransform;
        use regia_scene::spline::Spline;

        const ASSET: &str = r#"(format_version: 2, name: "dup",
            rigs: [(name: "a", root: [Rail(spline: "track", drive: Value(Literal(0.0)))])],
            director: Fixed("a"))"#;

        let spawn_track = |app: &mut App, y: f32| {
            let at = Vec3::new(0.0, y, 0.0);
            app.world_mut()
                .spawn((
                    Name::new("track"),
                    Spline::through([Vec3::ZERO, Vec3::X * 10.0]),
                    Transform::from_translation(at),
                    GlobalTransform(Affine3A::from_translation(at)),
                ))
                .id()
        };
        let run = |perturb: bool| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, crate::camera::CameraPlugin))
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
            assert_eq!(
                app.world_mut()
                    .resource_mut::<CameraAssets>()
                    .load_ron(ASSET, None, &hooks()),
                Ok(CameraBuildStatus::Ok)
            );
            if perturb {
                // Despawn swaps the last row into the hole, so the higher-id track is first.
                let first = spawn_track(&mut app, 5.0);
                spawn_track(&mut app, 0.0);
                spawn_track(&mut app, 5.0);
                let _ = app.world_mut().despawn(first);
            } else {
                spawn_track(&mut app, 0.0);
                spawn_track(&mut app, 5.0);
            }
            let camera = app
                .world_mut()
                .spawn(Transform::default())
                .insert(CameraDriver::new("dup"))
                .id();
            let tracks: Vec<(u64, f32)> = {
                let world = app.world_mut();
                let mut q = world.query::<(Entity, &GlobalTransform, &Name)>();
                q.iter(world)
                    .filter(|(_, _, name)| name.as_str() == "track")
                    .map(|(entity, global, _)| (entity.to_bits(), global.translation().y))
                    .collect()
            };
            let expected = tracks
                .iter()
                .min_by_key(|(bits, _)| *bits)
                .map_or(0.0, |(_, y)| *y);
            app.update();
            let got = app
                .world()
                .get::<Transform>(camera)
                .map_or(f32::NAN, |transform| transform.translation.y);
            (got.to_bits(), expected.to_bits())
        };

        let (plain, expected_plain) = run(false);
        let (perturbed, expected_perturbed) = run(true);
        assert_eq!(
            plain, perturbed,
            "both runs ride the lowest-id spline regardless of table order"
        );
        assert_eq!(plain, expected_plain, "the camera is on the lowest-id spline");
        assert_eq!(plain, expected_perturbed);
    }

    #[test]
    fn player_fov_override_replaces_the_gameplay_camera_fov() {
        use std::time::Duration;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::camera::CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
                1.0 / 60.0,
            )));
        let text = r#"(format_version: 1, name: "g",
            rigs: [(name: "a", root: [Fov(degrees: Literal(45.0))])],
            director: Fixed("a"))"#;
        app.world_mut()
            .resource_mut::<CameraAssets>()
            .load_ron(text, None, &CameraHooks::default())
            .expect("the rig loads");
        let cam = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraDriver::new("g"),
                regia_render::Camera::default(),
            ))
            .id();
        let degrees = |app: &App| {
            app.world()
                .get::<regia_render::Camera>(cam)
                .expect("camera")
                .fov_y
                .to_degrees()
        };
        app.update();
        assert!((degrees(&app) - 45.0).abs() < 1e-3, "{}", degrees(&app));
        app.insert_resource(crate::settings::SceneMotion {
            shake_scale: 1.0,
            bob_scale: 1.0,
            fov_override_deg: Some(80.0),
        });
        app.update();
        assert!((degrees(&app) - 80.0).abs() < 1e-3, "{}", degrees(&app));
        app.insert_resource(crate::settings::SceneMotion {
            shake_scale: 1.0,
            bob_scale: 1.0,
            fov_override_deg: None,
        });
        app.update();
        assert!(
            (degrees(&app) - 45.0).abs() < 1e-3,
            "None keeps the authored fov, got {}",
            degrees(&app)
        );
    }

    /// A rail name shared by several splines resolves to the lowest entity id, never the
    /// archetype row order. Bevy stores the index inverted in `Entity::to_bits`, so the
    /// later spawn is the lowest id; the expectation is read from the world, not from
    /// spawn order. Run 2 despawn-swaps the table. Both runs must still ride that spline
    /// (spec 35 § 12.13).
    #[test]
    fn a_duplicate_rail_name_resolves_to_the_lowest_entity_id_not_the_row_order() {
        use std::time::Duration;

        use regia_core::math::Affine3A;
        use regia_scene::GlobalTransform;
        use regia_scene::spline::Spline;

        use crate::camera::CameraPlugin;

        const ASSET: &str = r#"(format_version: 2, name: "rail",
            rigs: [(name: "a", root: [Rail(spline: "track", drive: Value(Literal(0.0)))])],
            director: Fixed("a"))"#;
        let near_x = 0.0f32;
        let far_x = 40.0f32;

        let run = |perturb: bool| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, CameraPlugin))
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
            assert_eq!(
                app.world_mut()
                    .resource_mut::<CameraAssets>()
                    .load_ron(ASSET, None, &hooks()),
                Ok(CameraBuildStatus::Ok)
            );
            let spline = Spline::through([Vec3::ZERO, Vec3::new(0.0, 0.0, 10.0)]);
            let spawn_track = |app: &mut App, x: f32| {
                let at = Vec3::new(x, 0.0, 0.0);
                app.world_mut()
                    .spawn((
                        Name::new("track"),
                        spline.clone(),
                        Transform::from_translation(at),
                        GlobalTransform(Affine3A::from_translation(at)),
                    ))
                    .id()
            };
            if perturb {
                // Spawn near, far, then a second near; despawning the first swap-removes the last
                // row into the hole, so the first row is not the lowest-id spline.
                let first = spawn_track(&mut app, near_x);
                spawn_track(&mut app, far_x);
                spawn_track(&mut app, near_x);
                let _ = app.world_mut().despawn(first);
            } else {
                spawn_track(&mut app, far_x);
                spawn_track(&mut app, near_x);
            }
            let camera = app
                .world_mut()
                .spawn(Transform::default())
                .insert(CameraDriver::new("rail"))
                .id();
            // The expected rail: the spline named "track" with the lowest entity bits.
            let tracks: Vec<(u64, f32)> = {
                let world = app.world_mut();
                let mut q = world.query::<(Entity, &Name, &GlobalTransform)>();
                q.iter(world)
                    .filter(|(_, name, _)| name.as_str() == "track")
                    .map(|(e, _, g)| (e.to_bits(), g.translation().x))
                    .collect()
            };
            let expected = tracks
                .iter()
                .min_by_key(|(bits, _)| *bits)
                .map_or(f32::NAN, |(_, x)| *x);
            app.update();
            let got = app
                .world()
                .get::<Transform>(camera)
                .map_or(f32::NAN, |t| t.translation.x);
            (got, expected)
        };

        let (plain, expected_plain) = run(false);
        let (perturbed, expected_perturbed) = run(true);
        assert_eq!(
            plain.to_bits(),
            expected_plain.to_bits(),
            "plain run rides the lowest-id rail, got {plain} expected {expected_plain}"
        );
        assert_eq!(
            perturbed.to_bits(),
            expected_perturbed.to_bits(),
            "despawn-swapped row order still rides the lowest-id rail, got {perturbed} expected {expected_perturbed}"
        );
        assert_eq!(
            plain.to_bits(),
            perturbed.to_bits(),
            "both runs ride the same rail regardless of table order, plain {plain} perturbed {perturbed}"
        );
    }

    /// A `std::io::Write` appending to a shared buffer (captures `tracing` output).
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The process-wide sink the capture subscriber writes to (other tests may share it; each
    /// test asserts only the delta it produced).
    static WARN_SINK: std::sync::OnceLock<SharedSink> = std::sync::OnceLock::new();

    fn warn_sink() -> &'static SharedSink {
        WARN_SINK.get_or_init(SharedSink::default)
    }

    /// Installs the WARN-level capture subscriber once per process. A thread-local
    /// `with_default` is not reliable while other tests in the same binary drive apps on shared
    /// threads; the global default only accepts its first installation, so the sink is global
    /// too and tests assert their own deltas.
    fn install_warn_capture() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let sink = warn_sink().clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(move || sink.clone())
                .finish();
            // Another part of the test binary may have won the race: ignore the error.
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
    }

    /// The duplicate-name warning fires once per (driver, name), not per lookup per frame: the
    /// name scan runs for every `Pivot` / `LookAt` reference on every step (Muse read-only review
    /// of fix-framework1-determinism, medium finding 4).
    #[test]
    fn a_duplicate_target_name_warns_once_not_every_frame() {
        use std::time::Duration;

        use crate::camera::CameraPlugin;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        // Systems run on the compute pool by default; run the schedules single-threaded so the
        // warnings this test provokes land on its own thread's capture.
        {
            use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
            let mut schedules = app.world_mut().resource_mut::<Schedules>();
            schedules
                .get_mut(Update)
                .expect("Update schedule")
                .set_executor(SingleThreadedExecutor::new());
            schedules
                .get_mut(PostUpdate)
                .expect("PostUpdate schedule")
                .set_executor(SingleThreadedExecutor::new());
        }
        assert_eq!(
            app.world_mut().resource_mut::<CameraAssets>().load_ron(
                r#"(format_version: 1, name: "dup",
                        rigs: [(name: "a", root: [Pivot(target: Entity("warnonce"), offset: (0.0, 0.0, 0.0),
                            space: World, aim_offset: (0.0, 0.0, 0.0), inherit_rotation: false)])],
                        director: Fixed("a"))"#,
                None,
                &hooks()
            ),
            Ok(CameraBuildStatus::Ok)
        );
        app.world_mut()
            .spawn((Transform::default(), Name::new("warnonce")));
        app.world_mut()
            .spawn((Transform::default(), Name::new("warnonce")));
        let driver = app
            .world_mut()
            .spawn(Transform::default())
            .insert(CameraDriver::new("dup"))
            .id();

        install_warn_capture();
        let sink = warn_sink();
        let before = sink.0.lock().expect("sink lock").len();
        for _ in 0..3 {
            app.update();
        }
        let text = String::from_utf8(sink.0.lock().expect("sink lock").clone()).expect("utf-8");
        // A parallel test (the resolution test) emits the same warning for its own "hero" scene;
        // this test's entities are named "warnonce", so count only those lines.
        let occurrences = text[before..].matches("name=\"warnonce\"").count();
        assert_eq!(
            occurrences, 1,
            "one warning per (driver, name), not per frame; captured: {text:?}"
        );

        // A frame with every driver disabled takes the early return: the latch must survive it,
        // so re-enabling the driver does not warn again.
        app.world_mut()
            .get_mut::<CameraDriver>(driver)
            .expect("driver")
            .enabled = false;
        app.update();
        app.world_mut()
            .get_mut::<CameraDriver>(driver)
            .expect("driver")
            .enabled = true;
        for _ in 0..2 {
            app.update();
        }
        let text = String::from_utf8(sink.0.lock().expect("sink lock").clone()).expect("utf-8");
        let occurrences = text[before..].matches("name=\"warnonce\"").count();
        assert_eq!(
            occurrences, 1,
            "a disabled frame keeps the warning latch; captured: {text:?}"
        );
    }
}
