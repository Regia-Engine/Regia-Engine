// SPDX-License-Identifier: MPL-2.0
//! [`VfxScene`]: the live set of effect instances plus the asset library, driven once per frame.
//!
//! The scene owns everything CPU-side: the [`VfxLibrary`] (parsed and curve-baked `.vfx.ron`
//! assets), active [`VfxInstance`]s, and the per-frame hand-off to the GPU
//! ([`VfxScene::upload`]). The host renderer (or the sample's presenter) then calls
//! [`crate::gpu::VfxGpu::simulate`] and the draw functions.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;

use crate::curves::{CurveId, CurveLut};
use crate::format::{BlendMode, RenderMode, VfxFormatError, VfxSystemDef};
use crate::gpu::{EmitterGpu, TrailVertex, VfxGpu};
use crate::mesh_shape::{VfxMesh, VfxMeshError};
use crate::ribbon::{RibbonStats, RibbonWorld};
use crate::sim::{LodInput, SpawnJob, VfxInstance};

/// Static, pre-baked per-emitter data (curve ids and render descriptors).
#[derive(Debug, Clone, Copy)]
struct EmitterBake {
    curve_size: CurveId,
    curve_color: CurveId,
    curve_alpha: CurveId,
    curve_rotation: CurveId,
    sprite_tile: u32,
    mode: u32,
    blend: u32,
    alpha_scale: f32,
    flip_cols: f32,
    flip_rows: f32,
    flip_frames: f32,
    flip_fps: f32,
    stretch_velocity: f32,
    stretch_min: f32,
    /// Drawn in the low-resolution soft layer ([`is_soft_emitter`]).
    soft: bool,
    softness: f32,
    emissive: f32,
    lit: f32,
    self_shadow: f32,
    screen_floor_px: f32,
    random_start: f32,
    blend_frames: f32,
    heat_haze: f32,
    /// Trigger mask of the emitter's event spawns.
    event_triggers: u32,
    /// `SpawnMode::Event`: drawn from the child ring.
    event_child: bool,
    /// `wind_response * wind_drag`: the coupling rate to the scene wind, per second.
    wind_coupling: f32,
}

/// Parsed `.vfx.ron` assets with baked curve LUTs, addressed by name or index.
#[derive(Debug, Default)]
pub struct VfxLibrary {
    defs: Vec<VfxSystemDef>,
    by_name: HashMap<String, u32>,
    bakes: Vec<Vec<EmitterBake>>,
    lut: CurveLut,
    /// Spawn meshes of the mesh shapes, by asset name (v7).
    meshes: HashMap<String, Arc<VfxMesh>>,
}

/// `true` for a `*.vfx.ron` file name.
fn is_vfx_ron(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".vfx.ron"))
}

impl VfxLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads every `*.vfx.ron` under `dir` (non-recursive; other `.ron` files such as
    /// `*.decal.ron` are skipped). An unreadable directory yields an empty library and a
    /// warning (user content must not panic).
    pub fn load_dir(dir: &Path) -> Result<Self, VfxFormatError> {
        let mut lib = Self::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(target: "regia_vfx", "vfx asset dir {dir:?} unreadable: {err}");
                return Ok(lib);
            }
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| is_vfx_ron(p))
            .collect();
        paths.sort();
        for path in paths {
            match std::fs::read_to_string(&path)
                .map_err(|e| VfxFormatError::Invalid(format!("{path:?}: {e}")))
                .and_then(|text| VfxSystemDef::from_ron(&text))
            {
                Ok(def) => {
                    tracing::debug!(target: "regia_vfx", "loaded vfx {}", def.name);
                    lib.add(def)?;
                }
                Err(err) => tracing::error!(target: "regia_vfx", "{path:?}: {err}"),
            }
        }
        Ok(lib)
    }

    /// Registers `mesh` as the spawn mesh named `name` (a [`crate::format::ShapeDef::MeshSurface`]
    /// or [`crate::format::ShapeDef::SkinnedMesh`] shape's `mesh`). Its alias table was built
    /// with it; instances spawned afterwards sample it. Replaces a mesh of the same name.
    pub fn add_mesh(&mut self, name: &str, mesh: VfxMesh) {
        self.meshes.insert(name.to_string(), Arc::new(mesh));
    }

    /// Registers a loaded mesh asset ([`regia_asset::CpuMesh`], with its skin) under `name`. An
    /// empty or zero-area mesh, or one with a bad index, is refused with its error and leaves
    /// nothing registered.
    pub fn add_cpu_mesh(&mut self, name: &str, mesh: &regia_asset::CpuMesh) -> Result<(), VfxMeshError> {
        let mesh = VfxMesh::from_cpu_mesh(mesh)?;
        self.add_mesh(name, mesh);
        Ok(())
    }

    /// The spawn mesh registered as `name`.
    #[must_use]
    pub fn mesh(&self, name: &str) -> Option<&Arc<VfxMesh>> {
        self.meshes.get(name)
    }

    /// The mesh a mesh-shape emitter samples, or why it has none: `Missing` when nothing is
    /// registered under the name, `NoSkin` for a skinned shape over a rigid mesh. `Ok(None)` for
    /// an analytic shape.
    pub fn shape_mesh(&self, shape: &crate::format::ShapeDef) -> Result<Option<&Arc<VfxMesh>>, VfxMeshError> {
        let Some(name) = shape.mesh_name() else {
            return Ok(None);
        };
        let mesh = self
            .meshes
            .get(name)
            .ok_or_else(|| VfxMeshError::Missing(name.to_string()))?;
        if matches!(shape, crate::format::ShapeDef::SkinnedMesh { .. }) && !mesh.is_skinned() {
            return Err(VfxMeshError::NoSkin);
        }
        Ok(Some(mesh))
    }

    /// Gives every mesh-shape emitter of `instance` its mesh from the library; an emitter whose
    /// mesh is missing or unusable gets none (it spawns nothing) and a warning.
    pub fn bind_meshes(&self, instance: &mut VfxInstance) {
        let def = self.def(instance.def_index());
        for (index, emitter) in def.emitters.iter().enumerate() {
            match self.shape_mesh(&emitter.init.shape) {
                Ok(None) => {}
                Ok(Some(mesh)) => instance.bind_mesh(index, Some(Arc::clone(mesh))),
                Err(err) => {
                    tracing::warn!(target: "regia_vfx", "{}/{}: {err}; the emitter spawns nothing", def.name, emitter.name);
                    instance.bind_mesh(index, None);
                }
            }
        }
    }

    /// Adds one definition (validates and bakes its curves). Returns its index.
    pub fn add(&mut self, def: VfxSystemDef) -> Result<u32, VfxFormatError> {
        def.validate()?;
        let index = self.defs.len() as u32;
        let bakes = self.bake_emitters(&def);
        self.by_name.insert(def.name.clone(), index);
        self.defs.push(def);
        self.bakes.push(bakes);
        Ok(index)
    }

    /// Adds `def`, or replaces the definition of the same name in place (live reload): its index
    /// stays, so running instances pick the new parameters up on their next update.
    pub fn upsert(&mut self, def: VfxSystemDef) -> Result<u32, VfxFormatError> {
        let Some(index) = self.index(&def.name) else {
            return self.add(def);
        };
        def.validate()?;
        self.defs[index as usize] = def;
        self.rebake_all();
        Ok(index)
    }

    /// Rebuilds the curve LUT and every emitter bake from the live definitions, so a replaced
    /// definition's old curves are dropped instead of accumulating across live reloads.
    fn rebake_all(&mut self) {
        self.lut = CurveLut::default();
        let defs = std::mem::take(&mut self.defs);
        self.bakes = defs.iter().map(|d| self.bake_emitters(d)).collect();
        self.defs = defs;
    }

    fn bake_emitters(&mut self, def: &VfxSystemDef) -> Vec<EmitterBake> {
        def.emitters
            .iter()
            .map(|e| {
                let u = &e.update;
                let flip = e.render.flipbook;
                EmitterBake {
                    curve_size: self.lut.bake_scalar(&u.size_over_life, 1.0),
                    curve_color: self.lut.bake_color(&u.color_over_life),
                    curve_alpha: self.lut.bake_scalar(&u.alpha_over_life, 1.0),
                    curve_rotation: self.lut.bake_scalar(&u.rotation_over_life, 0.0),
                    sprite_tile: crate::atlas::tile_index(e.render.sprite),
                    mode: match e.render.mode {
                        RenderMode::Billboard => 0,
                        RenderMode::Stretched => 1,
                        RenderMode::Mesh => 2,
                        // Drawn by the CPU strip builder; the GPU pool never holds one.
                        RenderMode::Ribbon => 3,
                    },
                    // Haze cards ride the unsorted list (drawn first by their own pipeline).
                    blend: match e.render.blend {
                        _ if e.render.heat_haze > 0.0 => 0,
                        BlendMode::Additive => 0,
                        BlendMode::Alpha => 1,
                    },
                    alpha_scale: e.render.alpha_scale,
                    flip_cols: flip.map(|f| f.cols as f32).unwrap_or(4.0),
                    flip_rows: flip.map(|f| f.rows as f32).unwrap_or(2.0),
                    flip_frames: flip.map(|f| f.frames as f32).unwrap_or(1.0),
                    flip_fps: flip
                        .map(|f| match f.mode {
                            crate::format::FlipbookMode::Fps(fps) => fps,
                            crate::format::FlipbookMode::OverLife => 0.0,
                        })
                        .unwrap_or(0.0),
                    stretch_velocity: e.render.stretch.velocity_scale,
                    stretch_min: e.render.stretch.min_length,
                    softness: e.render.softness,
                    emissive: e.render.emissive.max(0.0),
                    lit: if e.render.lit { 1.0 } else { 0.0 },
                    self_shadow: e.render.self_shadow.clamp(0.0, 1.0),
                    screen_floor_px: e.render.screen_floor_px.max(0.0),
                    random_start: flip.is_some_and(|f| f.random_start) as u8 as f32,
                    blend_frames: flip.is_some_and(|f| f.blend_frames) as u8 as f32,
                    heat_haze: e.render.heat_haze.max(0.0),
                    soft: is_soft_emitter(e),
                    event_triggers: crate::events::trigger_mask(e),
                    event_child: e.spawn.mode == crate::format::SpawnMode::Event,
                    wind_coupling: e.update.wind_response * e.update.wind_drag,
                }
            })
            .collect()
    }

    /// The longest particle lifetime any loaded definition can spawn, seconds.
    pub fn max_lifetime(&self) -> f32 {
        self.defs
            .iter()
            .flat_map(|d| d.emitters.iter())
            .map(|e| e.spawn.lifetime.max.max(e.spawn.lifetime.min))
            .fold(0.0, f32::max)
    }

    /// The baked emitter records of definition `index`.
    fn bakes_of(&self, index: u32) -> &[EmitterBake] {
        self.bakes.get(index as usize).map_or(&[], Vec::as_slice)
    }

    /// Index of `name`, if loaded.
    pub fn index(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    /// Definition by index.
    pub fn def(&self, index: u32) -> &VfxSystemDef {
        &self.defs[index as usize]
    }

    /// All loaded definitions.
    pub fn defs(&self) -> &[VfxSystemDef] {
        &self.defs
    }

    /// The baked curve LUTs (upload to [`VfxGpu`] once after loading).
    pub fn lut(&self) -> &CurveLut {
        &self.lut
    }

    /// Builds the world-space GPU record for one emitter of one instance.
    fn emitter_record(
        &self,
        def_index: u32,
        emitter_index: usize,
        instance: &VfxInstance,
        wind: Vec3,
    ) -> EmitterGpu {
        let bake = &self.bakes[def_index as usize][emitter_index];
        let emitter = &self.defs[def_index as usize].emitters[emitter_index];
        let u = &emitter.update;
        let (right, up, forward) = instance.axes();
        let to_world = |v: (f32, f32, f32)| right * v.0 + up * v.1 + forward * v.2;
        let vortex = u.vortex.as_ref().map(|v| {
            let axis = crate::format::dir_from_tuple(v.axis);
            [axis.x, axis.y, axis.z, v.strength]
        });
        let attractor = u.attractor.as_ref().map(|a| {
            let p = instance.origin() + to_world(a.position);
            [p.x, p.y, p.z, a.strength]
        });
        let kill = crate::kill_volume::resolve(
            u.kill_volume.as_ref(),
            instance.origin(),
            (right, up, forward),
            instance.scale(),
        );
        let (depth_mode, depth_offset) = u
            .depth_collision
            .as_ref()
            .map(|d| (if d.kill { 2.0 } else { 1.0 }, d.offset))
            .unwrap_or((0.0, 0.05));
        EmitterGpu {
            gravity_drag: [u.gravity.0, u.gravity.1, u.gravity.2, u.drag],
            // `curl.w`: the emitter draws in the low-resolution soft layer.
            curl: {
                let mut c = u
                    .curl
                    .map(|(amount, frequency, speed)| [amount, frequency, speed, 0.0])
                    .unwrap_or([0.0; 4]);
                c[3] = if bake.soft { 1.0 } else { 0.0 };
                c
            },
            vortex: vortex.unwrap_or([0.0; 4]),
            attractor: attractor.unwrap_or([0.0; 4]),
            kill: [
                u.kill_below.unwrap_or(f32::MIN),
                depth_mode,
                depth_offset,
                bake.softness,
            ],
            curve_ids: [
                bake.curve_size.0,
                bake.curve_color.0,
                bake.curve_alpha.0,
                bake.curve_rotation.0,
            ],
            render: [
                bake.mode as f32,
                bake.sprite_tile as f32,
                bake.alpha_scale,
                bake.flip_cols,
            ],
            flip: [bake.flip_rows, bake.flip_frames, bake.flip_fps, bake.random_start],
            stretch: [
                bake.stretch_velocity,
                bake.stretch_min,
                bake.blend as f32,
                bake.blend_frames,
            ],
            origin: [
                instance.origin().x,
                instance.origin().y,
                instance.origin().z,
                bake.heat_haze,
            ],
            quality: [bake.emissive, bake.lit, bake.self_shadow, bake.screen_floor_px],
            wind: if bake.wind_coupling > 0.0 {
                let c = bake.wind_coupling;
                [wind.x * c, wind.y * c, wind.z * c, c]
            } else {
                [0.0; 4]
            },
            // `events.y`: the vortex's extra radial acceleration as f32 bits (non-finite is 0).
            events: [
                bake.event_triggers,
                u.vortex
                    .as_ref()
                    .map_or(0.0, |v| if v.radial.is_finite() { v.radial } else { 0.0 })
                    .to_bits(),
                0,
                0,
            ],
            kill_volume: kill.volume,
            kill_axes: kill.axes,
        }
    }
}

/// The render-mesh slot of a `Mesh` emitter's `render.mesh` (`.vfx.ron` v8): the GPU slot of the
/// registered mesh, or [`crate::gpu::mesh_draw::RENDER_MESH_NONE`] (draws nothing) with the
/// reason recorded in `issues`.
fn resolve_render_mesh(
    library: &VfxLibrary,
    gpu: &mut VfxGpu,
    name: &str,
    issues: &mut std::collections::BTreeMap<String, (&'static str, String)>,
) -> u32 {
    let Some(mesh) = library.mesh(name) else {
        let err = VfxMeshError::Missing(name.to_string());
        issues.insert(name.to_string(), (err.code(), err.to_string()));
        return crate::gpu::mesh_draw::RENDER_MESH_NONE;
    };
    match gpu.render_mesh_slot(name, mesh) {
        Ok(slot) => slot,
        Err(refusal) => {
            issues.insert(name.to_string(), ("bad_mesh", refusal.to_string()));
            crate::gpu::mesh_draw::RENDER_MESH_NONE
        }
    }
}

/// Moves `instance`'s new ribbon spawns into its tracks, steps them by `dt` (0: no step) and
/// appends their strips to `out` facing `camera`, up to the trail-vertex cap.
fn ribbons(
    library: &VfxLibrary,
    instance: &mut VfxInstance,
    dt: f32,
    wind: Vec3,
    camera: Vec3,
    out: &mut Vec<TrailVertex>,
) -> RibbonStats {
    let def = library.def(instance.def_index());
    let bakes = library.bakes_of(instance.def_index());
    let mut stats = RibbonStats::default();
    if !bakes.iter().any(|b| b.mode == 3) {
        return stats;
    }
    let _span = tracing::info_span!("vfx_ribbon_build").entered();
    if instance.ribbons.len() < def.emitters.len() {
        instance.ribbons.resize_with(def.emitters.len(), Default::default);
    }
    let (right, up, forward) = instance.axes();
    let cap = crate::gpu::layout::MAX_TRAIL_VERTICES as usize;
    for (index, emitter) in def.emitters.iter().enumerate() {
        if bakes.get(index).is_none_or(|b| b.mode != 3) {
            continue;
        }
        let mut track = std::mem::take(&mut instance.ribbons[index]);
        instance.with_emitter_jobs(index, |jobs| track.push_jobs(jobs, emitter));
        if dt > 0.0 {
            let u = &emitter.update;
            let attractor = u.attractor.as_ref().map_or(Vec3::ZERO, |a| {
                instance.origin() + right * a.position.0 + up * a.position.1 + forward * a.position.2
            });
            let world = RibbonWorld {
                dt,
                wind,
                origin: instance.origin(),
                attractor,
                kill: crate::kill_volume::resolve(
                    u.kill_volume.as_ref(),
                    instance.origin(),
                    (right, up, forward),
                    instance.scale(),
                ),
            };
            track.step(emitter, world);
        }
        let built = track.build(emitter, camera, out, cap);
        instance.ribbons[index] = track;
        stats.strips += built.strips;
        stats.points += built.points;
        stats.vertices += built.vertices;
        stats.dropped_vertices += built.dropped_vertices;
    }
    stats
}

/// Brightness of a burst emitter's particles at `age` seconds after it fired, relative to
/// birth: the alpha curve times the colour curve's Rec. 709 luma (over its luma at birth),
/// sampled at the mean lifetime. `0` once the longest-lived particle is dead. Rate and
/// per-distance emitters keep emitting, so their light stays at 1.
pub fn fire_envelope(emitter: &crate::format::EmitterDef, age: f32) -> f32 {
    if !matches!(emitter.spawn.mode, crate::format::SpawnMode::Burst(_)) {
        return 1.0;
    }
    let life_max = emitter
        .spawn
        .lifetime
        .max
        .max(emitter.spawn.lifetime.min)
        .max(1e-3);
    if age >= life_max {
        return 0.0;
    }
    let life_mean = 0.5 * (emitter.spawn.lifetime.min + emitter.spawn.lifetime.max).max(1e-3);
    let t = (age / life_mean).clamp(0.0, 1.0);
    let u = &emitter.update;
    let alpha = crate::curves::sample_scalar(&u.alpha_over_life, t, 1.0).max(0.0);
    let luma = |c: [f32; 4]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let birth = luma(crate::curves::sample_color(&u.color_over_life, 0.0)).max(1e-4);
    let now = luma(crate::curves::sample_color(&u.color_over_life, t));
    (alpha * now / birth).clamp(0.0, 1.0)
}

/// Whether `emitter` is a soft, low-frequency particle (dust, smoke, billows) that may draw in
/// the low-resolution soft layer ([`crate::gpu::SoftParticleRes`]): alpha-blended, soft
/// (`softness >= 0.8`), not hot (`emissive <= 1`), a puff or soft-disc sprite, not a mesh, a
/// ribbon or a heat-haze card. Sparks, tracers, muzzle cores, fire bodies and small grains stay full
/// resolution (they are sharp, bright or a few pixels wide).
#[must_use]
pub fn is_soft_emitter(emitter: &crate::format::EmitterDef) -> bool {
    use crate::format::SpriteId;
    let r = &emitter.render;
    r.blend == BlendMode::Alpha
        && r.softness >= 0.8
        && r.emissive <= 1.0
        && r.heat_haze <= 0.0
        && r.mode != RenderMode::Mesh
        && r.mode != RenderMode::Ribbon
        && matches!(r.sprite, SpriteId::Puff | SpriteId::PuffAnim | SpriteId::SoftDisc)
}

/// [`VfxScene::container_sizes`]: lengths of the scene's per-frame containers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfxSceneSizes {
    /// Instance slots, live or free.
    pub instance_slots: usize,
    /// Free instance slots.
    pub free_slots: usize,
    /// GPU emitter-slot entries (one per uploaded instance).
    pub gpu_slots: usize,
    /// Death records across every emitter (old-age live count).
    pub death_records: usize,
    /// Most death records on one emitter.
    pub max_death_records: usize,
    /// Spawn jobs scheduled and not yet uploaded.
    pub pending_jobs: usize,
}

/// One instance in [`VfxBudgetStats`].
#[derive(Debug, Clone, PartialEq)]
pub struct VfxInstanceStats {
    /// Instance id ([`VfxScene::spawn`]).
    pub id: u64,
    /// The library name it plays (a medium variant when one was picked).
    pub effect: String,
    /// Live particles, pending jobs included (CPU old-age model).
    pub live: u32,
    /// The system's per-instance budget (`system.budget`).
    pub budget: u32,
    /// Still spawning (`false`: stopped, its particles play out).
    pub active: bool,
    /// Seconds since it was spawned.
    pub age: f32,
}

/// [`VfxScene::budget_stats`]: what the CPU budgets see this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VfxBudgetStats {
    /// Live particles across instances (the global budget's count).
    pub live: u32,
    /// Live alpha-blended particles (the smoke budget's count; the alpha sort covers them).
    pub live_alpha: u32,
    /// Live mesh particles (the mesh sort covers them).
    pub live_mesh: u32,
    /// Spawn jobs scheduled by the last update.
    pub last_scheduled: u32,
    /// Jobs scheduled and not yet uploaded to the GPU.
    pub pending_jobs: u32,
    /// Live instances, in slot order.
    pub instances: Vec<VfxInstanceStats>,
}

/// Whether a live instance still has event spawns for the GPU to run ([`VfxScene::upload`]).
#[derive(Debug, Clone, Copy, Default)]
struct LiveEventChildren {
    /// A live instance has event spawns.
    events: bool,
}

/// The live instances of a [`VfxScene`] at one moment ([`VfxScene::capture_instances`], ADR-2633).
#[derive(Clone)]
pub struct VfxSceneCapture {
    instances: Vec<Option<VfxInstance>>,
    free: Vec<usize>,
    next_id: u64,
    gpu_slots: HashMap<u64, (u32, u32)>,
    last_scheduled: u32,
    library_len: usize,
}

/// Live effect instances over a [`VfxLibrary`], ticked once per frame.
#[derive(Resource)]
pub struct VfxScene {
    library: VfxLibrary,
    instances: Vec<Option<VfxInstance>>,
    free: Vec<usize>,
    next_id: u64,
    /// GPU emitter-slot allocation per instance id: `(base slot, count)`.
    gpu_slots: HashMap<u64, (u32, u32)>,
    /// Emitter runs replaced by a reload that changed an instance's emitter count: their
    /// particles still index the old records, so the run is freed only after the longest
    /// particle lifetime. `(base slot, count, seconds left)`.
    retired_runs: Vec<(u32, u32, f32)>,
    last_scheduled: u32,
    /// The level's air ([`crate::medium`]): picks `name@airless` / `name@mars` variants.
    medium: crate::medium::VfxMedium,
    /// The scene wind, m/s ([`Self::set_wind`]).
    wind: Vec3,
    /// This frame's ribbon strips (v7), rebuilt by every update ([`Self::ribbon_vertices`]).
    ribbon_vertices: Vec<TrailVertex>,
    /// Counts of the last ribbon build.
    ribbon_stats: RibbonStats,
    /// Scene-wide ranking under caps (ADR-3275); off by default.
    significance: crate::significance::VfxSignificance,
    /// Render meshes (`.vfx.ron` v8 `render.mesh`) that drew nothing at the last upload, by
    /// name: the agent code (`missing_mesh` / `bad_mesh`) and the reason.
    render_mesh_issues: std::collections::BTreeMap<String, (&'static str, String)>,
}

impl VfxScene {
    /// Sets the wind particles with a `wind_response` drift in (the plugin copies
    /// [`regia_scene::weather::SceneWind`] here every frame). Non-finite input reads as calm.
    pub fn set_wind(&mut self, wind: Vec3) {
        self.wind = if wind.is_finite() { wind } else { Vec3::ZERO };
    }

    /// The wind particles drift in: [`Self::set_wind`], or calm where the level has no air.
    pub fn wind(&self) -> Vec3 {
        if self.medium.atmosphere == crate::medium::VfxAtmosphere::Airless {
            Vec3::ZERO
        } else {
            self.wind
        }
    }

    /// The live instances as they stand now: their ages, spawn accumulators, random streams,
    /// death records and pending jobs, with the scene's ids and GPU emitter slots. The library is
    /// not part of it.
    #[must_use]
    pub fn capture_instances(&self) -> VfxSceneCapture {
        VfxSceneCapture {
            instances: self.instances.clone(),
            free: self.free.clone(),
            next_id: self.next_id,
            gpu_slots: self.gpu_slots.clone(),
            last_scheduled: self.last_scheduled,
            library_len: self.library.defs.len(),
        }
    }

    /// Puts back what [`Self::capture_instances`] took. Returns `false` and changes nothing when
    /// the library shrank since then (a captured instance could name a definition that is gone).
    pub fn restore_instances(&mut self, capture: VfxSceneCapture) -> bool {
        if self.library.defs.len() < capture.library_len {
            return false;
        }
        self.instances = capture.instances;
        self.free = capture.free;
        // Ids only go up: an owner may still hold an id from the stretch (a hover wash that spawned
        // its instance then), and it must find that instance gone, not meet a new one (ADR-2728).
        self.next_id = self.next_id.max(capture.next_id);
        self.gpu_slots = capture.gpu_slots;
        self.last_scheduled = capture.last_scheduled;
        true
    }

    /// Moves every live instance by `-shift` (spec 78 § 12.2); see [`VfxInstance::shift_origin`].
    pub fn shift_origin(&mut self, shift: Vec3) {
        for instance in self.instances.iter_mut().flatten() {
            instance.shift_origin(shift);
            for track in &mut instance.ribbons {
                track.shift_origin(shift);
            }
        }
    }

    /// This frame's ribbon strips (`.vfx.ron` v7 [`RenderMode::Ribbon`] emitters): a triangle
    /// list for the trail pipelines, rebuilt by every [`Self::update`] facing its LOD camera. The
    /// runtime plugin appends them to [`crate::VfxTrails`]; a host drawing a scene itself passes
    /// them to [`VfxGpu::write_trails`] with its own trails.
    #[must_use]
    pub fn ribbon_vertices(&self) -> &[TrailVertex] {
        &self.ribbon_vertices
    }

    /// Counts of the last ribbon build.
    #[must_use]
    pub fn ribbon_stats(&self) -> RibbonStats {
        self.ribbon_stats
    }

    /// Sets instance `id`'s joint palette for its skinned-mesh shapes
    /// ([`VfxInstance::set_skin_palette`]); `false` when it is not live.
    pub fn set_skin_palette(&mut self, id: u64, palette: &[regia_core::math::Mat4]) -> bool {
        match self.instance_mut(id) {
            Some(instance) => {
                instance.set_skin_palette(palette);
                true
            }
            None => false,
        }
    }

    /// A scene over `library`.
    pub fn new(library: VfxLibrary) -> Self {
        Self {
            library,
            instances: Vec::new(),
            free: Vec::new(),
            next_id: 1,
            gpu_slots: HashMap::new(),
            retired_runs: Vec::new(),
            last_scheduled: 0,
            medium: crate::medium::VfxMedium::earth(),
            wind: Vec3::ZERO,
            ribbon_vertices: Vec::new(),
            ribbon_stats: RibbonStats::default(),
            significance: crate::significance::VfxSignificance::default(),
            render_mesh_issues: std::collections::BTreeMap::new(),
        }
    }

    /// The scene-wide significance ranking: caps, view, priorities and the last ranking's counts
    /// (ADR-3275).
    pub fn significance(&self) -> &crate::significance::VfxSignificance {
        &self.significance
    }

    /// Mutable access to the significance ranking (set caps, the view, priority overrides).
    pub fn significance_mut(&mut self) -> &mut crate::significance::VfxSignificance {
        &mut self.significance
    }

    /// Render meshes (`.vfx.ron` v8 `render.mesh`) that drew nothing at the last
    /// [`Self::upload`], by name: the agent error code (`missing_mesh`: nothing registered under
    /// the name; `bad_mesh`: the GPU refused it) and the reason. Empty when every mesh drew.
    pub fn render_mesh_issues(&self) -> &std::collections::BTreeMap<String, (&'static str, String)> {
        &self.render_mesh_issues
    }

    /// The medium effects spawn in (Earth until the host or the plugin sets the level's).
    pub fn medium(&self) -> crate::medium::VfxMedium {
        self.medium
    }

    /// Sets the medium effects spawn in: [`Self::spawn`] then plays the medium's variant of a
    /// name when the library has one.
    pub fn set_medium(&mut self, medium: crate::medium::VfxMedium) {
        self.medium = medium;
    }

    /// The library name `name` plays in the current medium: `name@airless` / `name@mars` when
    /// loaded, else `name` itself.
    pub fn resolve(&self, name: &str) -> String {
        if let Some(variant) = self.medium.variant_name(name)
            && self.library.index(&variant).is_some()
        {
            return variant;
        }
        name.to_string()
    }

    /// The scene's library.
    pub fn library(&self) -> &VfxLibrary {
        &self.library
    }

    /// Mutable library access (loading assets at runtime).
    pub fn library_mut(&mut self) -> &mut VfxLibrary {
        &mut self.library
    }

    /// Spawns an instance of `name` (its medium variant when the library has one, see
    /// [`Self::resolve`]); returns its instance id. `None` when the name is not loaded, or when a
    /// per-system cap ([`crate::significance::SignificanceCaps::per_system`]) refuses it.
    pub fn spawn(&mut self, name: &str, origin: Vec3, forward: Vec3, seed: u64) -> Option<u64> {
        let _span = tracing::info_span!("vfx_spawn", effect = name).entered();
        let def_index = match self.medium.variant_name(name) {
            Some(variant) => self
                .library
                .index(&variant)
                .or_else(|| self.library.index(name))?,
            None => self.library.index(name)?,
        };
        if !self.significance.caps().per_system.is_empty()
            && !self.make_room(name, def_index, origin, forward)
        {
            self.significance.note_rejected();
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut instance = VfxInstance::with_id(id, def_index, seed, origin, forward);
        self.library.bind_meshes(&mut instance);
        if let Some(slot) = self.free.pop() {
            self.instances[slot] = Some(instance);
        } else {
            self.instances.push(Some(instance));
        }
        Some(id)
    }

    /// Applies the per-system cap of `name` (or of the definition played) before a spawn: stops
    /// the instances its steal policy picks, or returns `false` to refuse the spawn (ADR-3275 (4)).
    fn make_room(&mut self, name: &str, def_index: u32, origin: Vec3, forward: Vec3) -> bool {
        use crate::significance::StealPolicy;
        let def = self.library.def(def_index);
        let caps = self.significance.caps();
        let Some(cap) = caps.system_cap(name).or_else(|| caps.system_cap(&def.name)) else {
            return true;
        };
        let (max, steal) = (cap.max_instances, cap.steal);
        let of_system = |i: &&VfxInstance| i.def_index() == def_index && i.is_active();
        let mut active = self.instances.iter().flatten().filter(of_system).count() as u64;
        while active >= u64::from(max) {
            let candidates = self.instances.iter().flatten().filter(of_system);
            let victim = match steal {
                StealPolicy::Reject => None,
                // Oldest first; the lower id among equals.
                StealPolicy::Oldest => candidates
                    .max_by(|a, b| a.age().total_cmp(&b.age()).then(b.id().cmp(&a.id())))
                    .map(VfxInstance::id),
                StealPolicy::LeastSignificant => {
                    let newcomer = VfxInstance::new(def_index, 0, origin, forward);
                    let (new_score, _, _) = self.significance.score_now(&newcomer, def);
                    candidates
                        .map(|i| (self.significance.score_now(i, def).0, i.id()))
                        .min_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)))
                        .filter(|&(score, _)| score < new_score)
                        .map(|(_, id)| id)
                }
            };
            let Some(victim) = victim else {
                return false;
            };
            if let Some(instance) = self.instances.iter_mut().flatten().find(|i| i.id() == victim) {
                instance.stop();
            }
            self.significance.note_stolen();
            active -= 1;
        }
        true
    }

    /// Stops every instance (they finish playing out).
    pub fn stop_all(&mut self) {
        for instance in self.instances.iter_mut().flatten() {
            instance.stop();
        }
    }

    /// Starts the instance slots over when no instance is live. Retired slots come back last in
    /// first out, so effects spawned in the same order took other slots than in the job before:
    /// the slot order sets who gets the particle budget first and the order of the spawn jobs, so
    /// the ring slots of their particles, the blend sort's last tie-break (ADR-2814).
    pub fn restart_idle_slots(&mut self) {
        if self.live_instances() == 0 {
            self.instances.clear();
            self.free.clear();
        }
    }

    /// Number of live (not yet retired) instances.
    pub fn live_instances(&self) -> usize {
        self.instances.iter().flatten().count()
    }

    /// Iterates live instances.
    pub fn instances(&self) -> impl Iterator<Item = &VfxInstance> {
        self.instances.iter().flatten()
    }

    /// The live instance with id `id` (from [`Self::spawn`]), for per-frame transforms, scale
    /// and intensity of attached effects.
    pub fn instance_mut(&mut self, id: u64) -> Option<&mut VfxInstance> {
        self.instances.iter_mut().flatten().find(|i| i.id() == id)
    }

    /// The live instance with id `id`.
    pub fn instance(&self, id: u64) -> Option<&VfxInstance> {
        self.instances.iter().flatten().find(|i| i.id() == id)
    }

    /// The GPU emitter-record run `(base, count)` instance `id` holds after an upload: record
    /// `base + i` is its emitter `i` ([`VfxGpu::read_particle_bounds`] is indexed by record).
    /// `None` before its first upload or once its run is released.
    pub fn gpu_emitter_run(&self, id: u64) -> Option<(u32, u32)> {
        self.gpu_slots.get(&id).copied()
    }

    /// Stops instance `id` (its particles play out); `false` when it is not live.
    pub fn stop(&mut self, id: u64) -> bool {
        match self.instance_mut(id) {
            Some(instance) => {
                instance.stop();
                true
            }
            None => false,
        }
    }

    /// Iterates live instances mutably (per-frame transforms).
    pub fn instances_mut(&mut self) -> impl Iterator<Item = &mut VfxInstance> {
        self.instances.iter_mut().flatten()
    }

    /// Jobs scheduled by the last [`VfxScene::update`] (stat).
    pub fn last_scheduled(&self) -> u32 {
        self.last_scheduled
    }

    /// Lengths of the containers the scene keeps from frame to frame (stat: a long session
    /// must keep every one bounded; `frame_creep.rs` samples them).
    pub fn container_sizes(&self) -> VfxSceneSizes {
        let (death_records, max_death_records) = self
            .instances
            .iter()
            .flatten()
            .map(VfxInstance::death_records)
            .fold((0, 0), |(total, most), (t, m)| (total + t, most.max(m)));
        VfxSceneSizes {
            instance_slots: self.instances.len(),
            free_slots: self.free.len(),
            gpu_slots: self.gpu_slots.len(),
            death_records,
            max_death_records,
            pending_jobs: self
                .instances
                .iter()
                .flatten()
                .map(|i| i.pending_jobs() as usize)
                .sum(),
        }
    }

    /// The CPU budget counts this frame: live, alpha and mesh particles, the last update's
    /// spawns, the upload queue, and every live instance against its system budget.
    pub fn budget_stats(&self) -> VfxBudgetStats {
        let mut stats = VfxBudgetStats {
            last_scheduled: self.last_scheduled,
            ..VfxBudgetStats::default()
        };
        for instance in self.instances.iter().flatten() {
            let def = self.library.def(instance.def_index());
            let bakes = self.library.bakes_of(instance.def_index());
            let live = instance.live_particles();
            let mesh: u32 = (0..def.emitters.len())
                .filter(|&i| bakes.get(i).is_some_and(|b| b.mode == 2))
                .map(|i| instance.emitter_live(i))
                .sum();
            stats.live = stats.live.saturating_add(live);
            stats.live_alpha = stats.live_alpha.saturating_add(instance.live_alpha(def));
            stats.live_mesh = stats.live_mesh.saturating_add(mesh);
            stats.pending_jobs = stats.pending_jobs.saturating_add(instance.pending_jobs());
            stats.instances.push(VfxInstanceStats {
                id: instance.id(),
                effect: def.name.clone(),
                live,
                budget: def.system.budget,
                active: instance.is_active(),
                age: instance.age(),
            });
        }
        stats
    }

    /// Estimated live particles across instances (CPU model; the GPU pool is the hard cap).
    pub fn estimated_live(&self) -> u32 {
        self.instances.iter().flatten().map(|i| i.live_particles()).sum()
    }

    /// Advances every instance's scheduling by `dt`.
    ///
    /// `capacity` is the GPU pool size — the global particle budget. Smoke is not capped here
    /// (see [`VfxScene::update_budgets`]).
    pub fn update(&mut self, dt: f32, lod: &LodInput, capacity: u32) {
        self.update_budgets(dt, lod, capacity, u32::MAX);
    }

    /// Like [`VfxScene::update`], and also caps alpha-blended particles at `smoke_budget`.
    ///
    /// With significance caps set ([`Self::significance_mut`]) the instances are ranked first and
    /// spawn in rank order under the caps (ADR-3275); without, the schedule is the one below.
    pub fn update_budgets(&mut self, dt: f32, lod: &LodInput, capacity: u32, smoke_budget: u32) {
        if self.significance.caps().is_active() {
            self.update_ranked(dt, lod, capacity, smoke_budget);
            return;
        }
        let _span = tracing::info_span!("vfx_schedule").entered();
        let mut scheduled = 0u32;
        // Age every instance and retire this frame's deaths first, so the budgets below count
        // what is alive now (measured before the deaths, they lagged a frame of deaths).
        let mut advanced = false;
        self.age_retired_runs(dt);
        for instance in self.instances.iter_mut().flatten() {
            let def = self.library.def(instance.def_index());
            advanced |= instance.advance(def, dt);
        }
        let live = self.estimated_live();
        regia_core::telemetry::set_gauge(regia_core::telemetry::Gauge::ParticlesLive, u64::from(live));
        let mut headroom = capacity.saturating_sub(live);
        let smoke_live: u32 = self
            .instances
            .iter()
            .flatten()
            .map(|instance| {
                let def = self.library.def(instance.def_index());
                instance.live_alpha(def)
            })
            .sum();
        let mut smoke_headroom = smoke_budget.saturating_sub(smoke_live);
        let mut retired: Vec<usize> = Vec::new();
        for (slot, instance) in self.instances.iter_mut().enumerate() {
            let Some(instance) = instance.as_mut() else {
                continue;
            };
            let def = self.library.def(instance.def_index());
            let (spawned, smoke) = if advanced {
                instance.spawn_counted(def, dt, lod, headroom, smoke_headroom)
            } else {
                (0, 0)
            };
            scheduled += spawned;
            // Both budgets are scene-wide: later instances see what earlier ones used.
            headroom = headroom.saturating_sub(spawned);
            smoke_headroom = smoke_headroom.saturating_sub(smoke);
            if instance.is_dead() {
                retired.push(slot);
            }
        }
        self.build_ribbons(advanced, dt, lod);
        for slot in retired {
            self.instances[slot] = None;
            self.free.push(slot);
        }
        self.last_scheduled = scheduled;
    }

    /// Counts every run retired by a reload down by `dt`; [`Self::upload`] frees the runs that
    /// reach zero. Both schedules call it, so a capped scene frees them too (rc1 review finding 3).
    fn age_retired_runs(&mut self, dt: f32) {
        if dt > 0.0 && dt.is_finite() {
            for run in &mut self.retired_runs {
                run.2 -= dt;
            }
        }
    }

    /// Rebuilds this frame's ribbon strips and their counts in slot order, after the spawns and
    /// before the deaths are retired. Both schedules call it, so a capped scene draws its trails
    /// and drops them with their instance (rc1 review finding 2).
    fn build_ribbons(&mut self, advanced: bool, dt: f32, lod: &LodInput) {
        let wind = self.wind();
        self.ribbon_vertices.clear();
        self.ribbon_stats = RibbonStats::default();
        let step = if advanced { dt } else { 0.0 };
        for instance in self.instances.iter_mut().flatten() {
            let built = ribbons(
                &self.library,
                instance,
                step,
                wind,
                lod.camera_pos,
                &mut self.ribbon_vertices,
            );
            self.ribbon_stats.strips += built.strips;
            self.ribbon_stats.points += built.points;
            self.ribbon_stats.vertices += built.vertices;
            self.ribbon_stats.dropped_vertices += built.dropped_vertices;
        }
    }

    /// [`Self::update_budgets`] under significance caps: ages every instance, ranks them, then
    /// spawns in rank order (ranked instances first, then paused and finishing ones in slot order)
    /// within the pool, smoke and per-frame spawn caps.
    fn update_ranked(&mut self, dt: f32, lod: &LodInput, capacity: u32, smoke_budget: u32) {
        use crate::significance::SpawnPlan;
        let _span = tracing::info_span!("vfx_schedule", ranked = true).entered();
        let caps = self.significance.caps();
        let capacity = capacity.min(caps.max_live.unwrap_or(u32::MAX));
        let mut frame_left = caps.max_spawns_per_frame.unwrap_or(u32::MAX);
        let warmup_max = caps.warmup_max_particles;
        let mut advanced = false;
        self.age_retired_runs(dt);
        for instance in self.instances.iter_mut().flatten() {
            let def = self.library.def(instance.def_index());
            advanced |= instance.advance(def, dt);
        }
        let live = self.estimated_live();
        regia_core::telemetry::set_gauge(regia_core::telemetry::Gauge::ParticlesLive, u64::from(live));
        let mut headroom = capacity.saturating_sub(live);
        let smoke_live: u32 = self
            .instances
            .iter()
            .flatten()
            .map(|instance| instance.live_alpha(self.library.def(instance.def_index())))
            .sum();
        let mut smoke_headroom = smoke_budget.saturating_sub(smoke_live);
        if advanced {
            self.significance
                .rank(&self.instances, &self.library, lod.camera_pos, dt);
        }
        let mut scheduled = 0u32;
        let mut retired: Vec<usize> = Vec::new();
        let ranked = self.significance.order().len();
        for k in 0..ranked + self.instances.len() {
            let slot = match k.checked_sub(ranked) {
                None => self.significance.order()[k] as usize,
                Some(slot) if self.significance.is_ranked(slot) => continue,
                Some(slot) => slot,
            };
            let Some(instance) = self.instances.get_mut(slot).and_then(Option::as_mut) else {
                continue;
            };
            let def = self.library.def(instance.def_index());
            let (spawned, smoke) = if advanced {
                let allowed = headroom.min(frame_left);
                let (spawned, smoke, warm) = match self.significance.plan(slot) {
                    SpawnPlan::Normal => {
                        let (s, m) = instance.spawn_counted(def, dt, lod, allowed, smoke_headroom);
                        (s, m, false)
                    }
                    SpawnPlan::Scaled { rate, warm } => {
                        let allowed = if warm > 0.0 {
                            allowed.min(warmup_max)
                        } else {
                            allowed
                        };
                        let (s, m) =
                            instance.spawn_counted(def, (dt + warm) * rate, lod, allowed, smoke_headroom);
                        (s, m, warm > 0.0)
                    }
                    SpawnPlan::Hold => {
                        // Intensity 0 spends a due burst and spawns nothing; dt 0 adds no rate.
                        let intensity = instance.intensity();
                        instance.set_intensity(0.0);
                        let (s, m) = instance.spawn_counted(def, 0.0, lod, u32::MAX, u32::MAX);
                        instance.set_intensity(intensity);
                        (s, m, false)
                    }
                };
                frame_left = frame_left.saturating_sub(spawned);
                self.significance.note_spawned(spawned, warm, frame_left == 0);
                (spawned, smoke)
            } else {
                (0, 0)
            };
            scheduled += spawned;
            headroom = headroom.saturating_sub(spawned);
            smoke_headroom = smoke_headroom.saturating_sub(smoke);
            if instance.is_dead() {
                retired.push(slot);
            }
        }
        self.build_ribbons(advanced, dt, lod);
        for slot in retired {
            self.instances[slot] = None;
            self.free.push(slot);
        }
        self.last_scheduled = scheduled;
    }

    /// Writes per-frame emitter records and routes pending spawn jobs into the GPU context.
    ///
    /// One call per frame after [`VfxScene::update`] and before [`VfxGpu::simulate`].
    pub fn upload(&mut self, gpu: &mut VfxGpu) {
        let _span = tracing::info_span!("vfx_upload").entered();
        gpu.note_upload();
        gpu.write_curves(self.library.lut());
        // Live alpha and mesh particles bound the GPU sorts; live soft particles gate the soft
        // layer.
        let (alpha, mesh, soft) =
            self.instances
                .iter()
                .flatten()
                .fold((0u32, 0u32, 0u32), |(a, m, s), instance| {
                    let def = self.library.def(instance.def_index());
                    let bakes = self.library.bakes_of(instance.def_index());
                    let live_where = |pick: fn(&EmitterBake) -> bool| -> u32 {
                        (0..def.emitters.len())
                            .filter(|&i| bakes.get(i).is_some_and(pick))
                            .map(|i| instance.emitter_live(i))
                            .sum()
                    };
                    (
                        a.saturating_add(instance.live_alpha(def)),
                        m.saturating_add(live_where(|b| b.mode == 2)),
                        s.saturating_add(live_where(|b| b.soft)),
                    )
                });
        // Event children live on the GPU. The draw bound is the CPU reservation (one firing
        // per scheduled parent, held until the child tail), capped at the ring. An instance
        // whose only emitter is an idle event child reserves nothing, so the sort stays on
        // the CPU alpha count and the soft layer stays off.
        let (est_alpha, est_mesh, est_soft) =
            self.instances
                .iter()
                .flatten()
                .fold((0u32, 0u32, 0u32), |(a, m, s), instance| {
                    let (ea, em, es) = instance.event_child_estimate();
                    (a.saturating_add(ea), m.saturating_add(em), s.saturating_add(es))
                });
        let children = self.live_event_children();
        let ring = gpu.child_capacity();
        gpu.set_events_active(children.events);
        gpu.set_alpha_bound(alpha.saturating_add(est_alpha.min(ring)));
        gpu.set_mesh_bound(mesh.saturating_add(est_mesh.min(ring)));
        gpu.set_soft_live(soft > 0 || est_soft > 0);
        gpu.set_max_lifetime(self.library.max_lifetime());
        gpu.set_scene_live(self.live_instances() > 0);
        let wind = self.wind();
        let mut instances = std::mem::take(&mut self.instances);
        let mut jobs: Vec<SpawnJob> = Vec::new();
        let mut issues = std::mem::take(&mut self.render_mesh_issues);
        let reported: std::collections::BTreeSet<String> = issues.keys().cloned().collect();
        issues.clear();
        self.retired_runs.retain(|&(base, count, left)| {
            if left <= 0.0 {
                gpu.free_emitters(base, count);
                false
            } else {
                true
            }
        });
        let ttl = self.library.max_lifetime() + 0.25;
        for (slot, instance) in instances.iter_mut().enumerate() {
            let Some(instance) = instance.as_mut() else {
                continue;
            };
            let def_index = instance.def_index();
            let def = self.library.def(def_index);
            // A paused or culled instance with nothing left on the GPU gives its emitter run back,
            // so a crowd of culled effects cannot exhaust the records the kept ones need.
            if self.significance.idle(slot, instance.id()) && releasable(instance, def) {
                release_run(
                    &mut self.gpu_slots,
                    instance.id(),
                    &mut *gpu,
                    VfxGpu::free_emitters,
                );
                continue;
            }
            let n = def.emitters.len() as u32;
            // A reload changed the emitter count: the live particles still read the old run, so
            // keep it allocated until they are gone instead of freeing it for reuse now.
            retire_resized_run(&mut self.gpu_slots, &mut self.retired_runs, instance.id(), n, ttl);
            let Some(base) = slot_run(
                &mut self.gpu_slots,
                instance.id(),
                n,
                &mut *gpu,
                VfxGpu::allocate_emitters,
                VfxGpu::free_emitters,
            ) else {
                // Emitter records exhausted: this instance cannot render. Drop its jobs with their
                // live count and death records so they stop counting against the particle budget.
                instance.discard_pending_jobs();
                continue;
            };
            for emitter_index in 0..def.emitters.len() {
                let mut record = self
                    .library
                    .emitter_record(def_index, emitter_index, instance, wind);
                if let Some(name) = def.emitters[emitter_index].render.mesh.as_deref()
                    && def.emitters[emitter_index].render.mode == RenderMode::Mesh
                {
                    record.events[1] = resolve_render_mesh(&self.library, gpu, name, &mut issues);
                }
                gpu.write_emitter(base + emitter_index as u32, record);
                let bake = &self.library.bakes_of(def_index)[emitter_index];
                if bake.event_triggers != 0 || bake.event_child {
                    let emitter = &def.emitters[emitter_index];
                    let lod = instance.lod_scale();
                    gpu.write_emitter_events(
                        base + emitter_index as u32,
                        crate::events::links_for(def, emitter, base, lod, instance.intensity()),
                        crate::events::child_init_for(emitter, instance, lod),
                    );
                }
                jobs.clear();
                instance.drain_emitter_jobs(emitter_index, &mut jobs);
                gpu.push_spawn_jobs(base + emitter_index as u32, &jobs);
            }
        }
        // Free GPU emitter records of retired instances.
        let live: std::collections::HashSet<u64> = instances.iter().flatten().map(|i| i.id()).collect();
        let stale: Vec<u64> = self
            .gpu_slots
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in stale {
            if let Some((base, count)) = self.gpu_slots.remove(&id) {
                gpu.free_emitters(base, count);
            }
        }
        self.instances = instances;
        for (name, (code, reason)) in &issues {
            if !reported.contains(name) {
                tracing::warn!(target: "regia_vfx", "render mesh '{name}' draws nothing ({code}): {reason}");
            }
        }
        self.render_mesh_issues = issues;
    }

    /// Whether a live instance has event spawns the GPU must keep running this frame.
    fn live_event_children(&self) -> LiveEventChildren {
        let mut out = LiveEventChildren::default();
        for instance in self.instances.iter().flatten() {
            for bake in self.library.bakes_of(instance.def_index()) {
                out.events |= bake.event_triggers != 0;
            }
        }
        out
    }

    /// Collects this frame's particle lights (emitters with a light module, positioned at the
    /// instance origin, strongest-first up to `budget`).
    ///
    /// A light is live only while the emitter's delay/duration window is open, so a flash
    /// light does not stay on after the sprite dies. A burst emitter's light also follows
    /// its particles: the intensity is scaled by [`fire_envelope`] at the burst's age, so a
    /// fireball's ground light dims as the body cools and fades, and goes out when it dies.
    pub fn gather_lights(&self, frame: &mut crate::lights::VfxFrameLights, budget: u32) {
        let mut candidates = Vec::new();
        for instance in self.instances.iter().flatten() {
            let def = self.library.def(instance.def_index());
            let age = instance.age();
            for emitter in &def.emitters {
                let Some(light) = &emitter.render.light else {
                    continue;
                };
                if age < emitter.delay {
                    continue;
                }
                if let Some(duration) = emitter.duration
                    && age > emitter.delay + duration
                {
                    continue;
                }
                let envelope = fire_envelope(emitter, age - emitter.delay);
                if envelope <= 0.0 {
                    continue;
                }
                let color = crate::format::parse_color(&light.color).unwrap_or([1.0, 1.0, 1.0, 1.0]);
                // A throttled instance (`set_intensity`, e.g. a plume) dims its light with it.
                let k = light.intensity * envelope * instance.intensity().min(1.0);
                if k <= 0.0 {
                    continue;
                }
                candidates.push(crate::lights::ParticleLight {
                    position: instance.origin(),
                    color: [color[0] * k, color[1] * k, color[2] * k],
                    radius: light.radius,
                });
            }
        }
        frame.gather(candidates, budget);
    }
}

/// Moves instance `id`'s run to `retired` (to be freed after `ttl` seconds) when it was allocated
/// for a different emitter count than `n`, so the next [`slot_run`] allocates a fresh one.
fn retire_resized_run(
    slots: &mut HashMap<u64, (u32, u32)>,
    retired: &mut Vec<(u32, u32, f32)>,
    id: u64,
    n: u32,
    ttl: f32,
) {
    if let Some(&(base, count)) = slots.get(&id)
        && count != n
    {
        slots.remove(&id);
        retired.push((base, count, ttl));
    }
}

/// Whether an idle instance's emitter run can be freed: no live particles (the CPU old-age model,
/// the same test retirement uses), no pending jobs, and no event spawns (children on the GPU read
/// the parent's records and the CPU cannot count them).
fn releasable(instance: &VfxInstance, def: &VfxSystemDef) -> bool {
    instance.live_particles() == 0 && instance.pending_jobs() == 0 && !def.has_events()
}

/// Frees instance `id`'s GPU emitter run, if it has one.
fn release_run<G>(slots: &mut HashMap<u64, (u32, u32)>, id: u64, gpu: &mut G, free: fn(&mut G, u32, u32)) {
    if let Some((base, count)) = slots.remove(&id) {
        free(gpu, base, count);
    }
}

/// The GPU emitter run for instance `id` with `n` emitters. A run allocated for another emitter
/// count (the definition was reloaded with emitters added or removed) is freed and reallocated:
/// writing `n` records into a shorter run would overwrite the next instance's records.
/// `None` when the allocation fails; the instance then has no run.
fn slot_run<G>(
    slots: &mut HashMap<u64, (u32, u32)>,
    id: u64,
    n: u32,
    gpu: &mut G,
    allocate: fn(&mut G, u32) -> Option<u32>,
    free: fn(&mut G, u32, u32),
) -> Option<u32> {
    if let Some(&(base, count)) = slots.get(&id) {
        if count == n {
            return Some(base);
        }
        slots.remove(&id);
        free(gpu, base, count);
    }
    let base = allocate(gpu, n)?;
    slots.insert(id, (base, n));
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `*.vfx.ron` names are VFX assets; decal prefabs sharing the directory are not.
    #[test]
    fn vfx_ron_filter_skips_other_ron_files() {
        assert!(is_vfx_ron(Path::new("a/muzzle.vfx.ron")));
        assert!(!is_vfx_ron(Path::new("a/scorch.decal.ron")));
        assert!(!is_vfx_ron(Path::new("a/plain.ron")));
        assert!(!is_vfx_ron(Path::new("a/vfx.ron.bak")));
    }
    use crate::format::{BlendMode, EmitterDef, InitModule, RangeDef, RenderModule, SpawnMode, SpawnModule};

    fn rate_def(name: &str, rate: f32, budget: u32) -> VfxSystemDef {
        VfxSystemDef {
            version: 1,
            name: name.into(),
            system: crate::format::SystemModule {
                bounds: None,
                budget,
                lod: None,
            },
            emitters: vec![EmitterDef {
                name: "e".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(rate),
                    lifetime: RangeDef { min: 0.5, max: 0.5 },
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: crate::format::ShapeDef::Point,
                    velocity: Default::default(),
                    size: RangeDef { min: 0.5, max: 0.5 },
                    rotation: RangeDef { min: 0.0, max: 0.0 },
                    spin: RangeDef { min: 0.0, max: 0.0 },
                    color: Vec::new(),
                },
                update: Default::default(),
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: crate::format::SpriteId::SoftDisc,
                    flipbook: None,
                    blend: BlendMode::Alpha,
                    softness: 0.0,
                    alpha_scale: 1.0,
                    stretch: Default::default(),
                    light: None,
                    emissive: 1.0,
                    lit: false,
                    self_shadow: 0.65,
                    screen_floor_px: 0.0,
                    heat_haze: 0.0,
                    ribbon: Default::default(),
                    mesh: None,
                },
            }],
        }
    }

    /// Stand-in for the GPU emitter allocator: a bump allocator that records frees.
    #[derive(Default)]
    struct FakeRuns {
        next: u32,
        fail: bool,
        freed: Vec<(u32, u32)>,
    }

    impl FakeRuns {
        fn allocate(&mut self, n: u32) -> Option<u32> {
            if self.fail {
                return None;
            }
            let base = self.next;
            self.next += n;
            Some(base)
        }

        fn free(&mut self, base: u32, n: u32) {
            self.freed.push((base, n));
        }
    }

    /// A reload that changes the emitter count gets a run of the new size, and the old run is
    /// freed; an unchanged count keeps its run.
    #[test]
    fn slot_run_reallocates_when_the_emitter_count_changes() {
        let mut slots = HashMap::new();
        let mut fake = FakeRuns::default();
        let run = |slots: &mut HashMap<u64, (u32, u32)>, fake: &mut FakeRuns, n| {
            slot_run(slots, 7, n, fake, FakeRuns::allocate, FakeRuns::free)
        };
        assert_eq!(run(&mut slots, &mut fake, 2), Some(0));
        assert_eq!(run(&mut slots, &mut fake, 2), Some(0), "same count keeps the run");
        assert_eq!(
            run(&mut slots, &mut fake, 3),
            Some(2),
            "grown definition gets a run of 3"
        );
        assert_eq!(slots[&7], (2, 3));
        assert_eq!(fake.freed, vec![(0, 2)]);
    }

    /// A resized run is retired with a countdown, not freed, so live particles keep their records.
    #[test]
    fn a_resized_run_is_retired_not_freed() {
        let mut slots = HashMap::from([(7u64, (0u32, 2u32))]);
        let mut retired = Vec::new();
        retire_resized_run(&mut slots, &mut retired, 7, 2, 3.0);
        assert!(retired.is_empty(), "same count keeps the run");
        retire_resized_run(&mut slots, &mut retired, 7, 1, 3.0);
        assert_eq!(retired, vec![(0, 2, 3.0)]);
        assert!(slots.is_empty());
        let mut fake = FakeRuns::default();
        assert_eq!(
            slot_run(&mut slots, 7, 1, &mut fake, FakeRuns::allocate, FakeRuns::free),
            Some(0)
        );
        assert!(
            fake.freed.is_empty(),
            "nothing is freed while particles may be alive"
        );
    }

    /// An idle instance's run goes back to the allocator once, and a later frame that needs it
    /// again gets a fresh run (ADR-3275 (7)).
    #[test]
    fn release_run_frees_once_and_slot_run_reallocates() {
        let mut slots = HashMap::new();
        let mut fake = FakeRuns::default();
        assert_eq!(
            slot_run(&mut slots, 7, 2, &mut fake, FakeRuns::allocate, FakeRuns::free),
            Some(0)
        );
        release_run(&mut slots, 7, &mut fake, FakeRuns::free);
        release_run(&mut slots, 7, &mut fake, FakeRuns::free);
        assert_eq!(fake.freed, vec![(0, 2)], "freed exactly once");
        assert!(slots.is_empty());
        assert_eq!(
            slot_run(&mut slots, 7, 2, &mut fake, FakeRuns::allocate, FakeRuns::free),
            Some(2)
        );
    }

    /// Only an instance with nothing alive, nothing pending and no event spawns may give its run
    /// back.
    #[test]
    fn releasable_needs_no_live_no_pending_and_no_events() {
        let def = rate_def("puff", 640.0, 1000);
        let mut instance = VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z);
        assert!(
            releasable(&instance, &def),
            "a new instance has nothing on the GPU"
        );
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        instance.schedule(&def, 1.0 / 64.0, &lod, 1000);
        assert!(!releasable(&instance, &def), "pending and live particles");
        let events = VfxSystemDef::from_ron(&format!(
            r#"(
    version: {},
    name: "sparks",
    emitters: [
        (
            name: "sparks",
            spawn: (mode: Rate(60.0), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.2)),
            update: (events: [(on: Death, child: "embers", count: 2, probability: 0.5)]),
            render: (mode: Billboard),
        ),
        (
            name: "embers",
            spawn: (mode: Event, lifetime: (min: 0.5, max: 2.0)),
            init: (size: (min: 0.1, max: 0.2)),
            render: (mode: Billboard),
        ),
    ],
)"#,
            crate::format::VFX_FORMAT_VERSION
        ))
        .expect("events fixture");
        assert!(events.has_events());
        assert!(!releasable(&VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z), &events));
    }

    /// A failed allocation leaves no stale run behind.
    #[test]
    fn slot_run_failure_drops_the_old_run() {
        let mut slots = HashMap::from([(7u64, (4u32, 2u32))]);
        let mut fake = FakeRuns {
            fail: true,
            ..FakeRuns::default()
        };
        let got = slot_run(&mut slots, 7, 3, &mut fake, FakeRuns::allocate, FakeRuns::free);
        assert_eq!(got, None);
        assert!(slots.is_empty());
        assert_eq!(fake.freed, vec![(4, 2)]);
    }

    #[test]
    fn upsert_does_not_grow_the_curve_lut_without_bound() {
        let mut lib = VfxLibrary::new();
        lib.add(rate_def("smoke", 10.0, 100)).expect("valid");
        let mut sizes = Vec::new();
        for i in 0..64 {
            let mut def = rate_def("smoke", 10.0, 100);
            def.emitters[0].update.size_over_life = vec![
                crate::format::FloatKey {
                    t: 0.0,
                    v: 1.0 + i as f32,
                },
                crate::format::FloatKey { t: 1.0, v: 2.0 },
            ];
            lib.upsert(def).expect("valid");
            sizes.push(lib.lut().scalars().len());
        }
        assert!(sizes.iter().all(|&n| n == sizes[0]), "LUT grew: {sizes:?}");
        assert_eq!(lib.defs().len(), 1);
    }

    #[test]
    fn library_loads_and_indexes() {
        let mut lib = VfxLibrary::new();
        let idx = lib.add(rate_def("smoke", 10.0, 100)).expect("valid");
        assert_eq!(lib.index("smoke"), Some(idx));
        assert!(lib.index("nope").is_none());
        assert_eq!(lib.def(idx).name, "smoke");
        assert!(!lib.lut().scalars().is_empty(), "curves baked");
    }

    #[test]
    fn library_rejects_invalid() {
        let mut lib = VfxLibrary::new();
        let mut bad = rate_def("bad", 1.0, 10);
        bad.emitters[0].spawn.lifetime = RangeDef { min: 1.0, max: 0.5 };
        assert!(lib.add(bad).is_err());
    }

    #[test]
    fn idle_slots_start_over_so_effects_take_the_same_slots_again() {
        let mut lib = VfxLibrary::new();
        lib.add(rate_def("short", 120.0, 1000)).expect("valid fixture");
        let mut long = rate_def("long", 120.0, 1000);
        long.emitters[0].spawn.lifetime = RangeDef { min: 1.0, max: 1.0 };
        lib.add(long).expect("valid fixture");
        let mut scene = VfxScene::new(lib);
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        let dt = 1.0 / 60.0;
        let spawn_both = |scene: &mut VfxScene| {
            let short = scene.spawn("short", Vec3::ZERO, Vec3::Z, 1).expect("spawned");
            let long = scene.spawn("long", Vec3::ZERO, Vec3::Z, 2).expect("spawned");
            [short, long]
        };
        let slot_order = |scene: &VfxScene| scene.instances().map(VfxInstance::id).collect::<Vec<_>>();

        let first = spawn_both(&mut scene);
        assert_eq!(slot_order(&scene), first);
        // A live instance keeps its slot.
        scene.restart_idle_slots();
        assert_eq!(slot_order(&scene), first);
        // Both have particles in flight; the short one retires first, so its slot comes back last.
        for _ in 0..30 {
            scene.update(dt, &lod, 100_000);
        }
        scene.stop_all();
        for _ in 0..180 {
            scene.update(dt, &lod, 100_000);
        }
        assert_eq!(scene.live_instances(), 0);
        scene.restart_idle_slots();
        let second = spawn_both(&mut scene);
        assert_eq!(slot_order(&scene), second, "the same spawns took the same slots");
    }

    /// A run retired by a reload counts down under significance caps as it does without, so the
    /// next upload frees it (rc1 review finding 3: the ranked schedule never aged it, and every
    /// reload leaked its emitter records until the pool ran out).
    #[test]
    fn retired_runs_age_with_and_without_significance_caps() {
        for capped in [false, true] {
            let mut lib = VfxLibrary::new();
            lib.add(rate_def("puff", 120.0, 1000)).expect("valid fixture");
            let mut scene = VfxScene::new(lib);
            if capped {
                scene
                    .significance_mut()
                    .set_caps(crate::significance::SignificanceCaps {
                        max_instances: Some(4),
                        ..Default::default()
                    });
                assert!(scene.significance().caps().is_active());
            }
            scene.spawn("puff", Vec3::ZERO, Vec3::Z, 42).expect("spawned");
            scene.retired_runs.push((0, 2, 0.5));
            let lod = LodInput {
                camera_pos: Vec3::ZERO,
            };
            for _ in 0..60 {
                scene.update(1.0 / 60.0, &lod, 100_000);
            }
            assert_eq!(scene.retired_runs.len(), 1, "capped {capped}");
            let left = scene.retired_runs[0].2;
            assert!(
                left <= 0.0,
                "capped {capped}: 1 s after a 0.5 s retirement the run is due, left {left}"
            );
        }
    }

    #[test]
    fn scene_spawns_updates_and_retires() {
        let mut lib = VfxLibrary::new();
        lib.add(rate_def("puff", 120.0, 1000)).expect("valid fixture");
        let mut scene = VfxScene::new(lib);
        let id = scene.spawn("puff", Vec3::ZERO, Vec3::Z, 42).expect("spawned");
        assert!(id > 0);
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        let dt = 1.0 / 60.0;
        for _ in 0..30 {
            scene.update(dt, &lod, 100_000);
        }
        assert_eq!(scene.live_instances(), 1);
        assert!(scene.last_scheduled() > 0);
        // Instance dies shortly after its particles' lifetime (0.5 s) once stopped.
        scene.stop_all();
        for _ in 0..60 {
            scene.update(dt, &lod, 100_000);
        }
        assert_eq!(scene.live_instances(), 0, "instance retired");
    }

    #[test]
    fn light_is_off_outside_the_emitter_window() {
        use crate::format::ParticleLightDef;
        use crate::lights::VfxFrameLights;
        let mut def = rate_def("flash", 0.0, 16);
        def.emitters[0].spawn.mode = SpawnMode::Burst(1);
        def.emitters[0].duration = Some(0.08);
        def.emitters[0].render.light = Some(ParticleLightDef {
            color: "#fff1c4".into(),
            intensity: 8.0,
            radius: 6.0,
        });
        let mut lib = VfxLibrary::new();
        lib.add(def).expect("flash");
        let mut scene = VfxScene::new(lib);
        scene
            .spawn("flash", Vec3::new(0.0, 1.0, 0.0), Vec3::Z, 1)
            .expect("spawn");
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        scene.update(0.05, &lod, 1000);
        let mut lights = VfxFrameLights::default();
        scene.gather_lights(&mut lights, 4);
        assert_eq!(lights.lights.len(), 1, "flash light missing at 50 ms");
        scene.update(0.2, &lod, 1000);
        scene.gather_lights(&mut lights, 4);
        assert!(lights.lights.is_empty(), "light outlived the 80 ms window");
    }

    /// A fire light follows the fire: bright while the body is hot and opaque, dimmer as
    /// its colour cools and its alpha fades, off once the particles are dead.
    #[test]
    fn fire_light_follows_the_fire_luminance() {
        use crate::format::{ColorKey, FloatKey, ParticleLightDef, RangeDef};
        use crate::lights::VfxFrameLights;
        let mut def = rate_def("fire", 0.0, 16);
        let e = &mut def.emitters[0];
        e.spawn.mode = SpawnMode::Burst(4);
        e.spawn.lifetime = RangeDef { min: 0.9, max: 1.1 };
        e.update.alpha_over_life = vec![FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 0.0 }];
        e.update.color_over_life = vec![
            ColorKey {
                t: 0.0,
                color: "#ffffff".into(),
            },
            ColorKey {
                t: 1.0,
                color: "#402010".into(),
            },
        ];
        e.render.light = Some(ParticleLightDef {
            color: "#ffb060".into(),
            intensity: 8.0,
            radius: 6.0,
        });
        let mut lib = VfxLibrary::new();
        lib.add(def).expect("fire");
        let mut scene = VfxScene::new(lib);
        scene
            .spawn("fire", Vec3::new(0.0, 1.0, 0.0), Vec3::Z, 1)
            .expect("spawn");
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        let peak = |scene: &VfxScene| {
            let mut lights = VfxFrameLights::default();
            scene.gather_lights(&mut lights, 4);
            lights
                .lights
                .first()
                .map(|l| l.color[0].max(l.color[1]).max(l.color[2]))
        };
        scene.update(0.15, &lod, 1000);
        let early = peak(&scene).expect("fire light at 0.15 s");
        scene.update(0.55, &lod, 1000);
        let late = peak(&scene).expect("fire light at 0.7 s");
        assert!(
            early > 2.0 * late,
            "light did not follow the cooling fire ({early} → {late})"
        );
        assert!(late > 0.0);
        scene.update(0.6, &lod, 1000);
        assert_eq!(peak(&scene), None, "light outlived the fire");
    }

    #[test]
    fn global_capacity_limits_headroom() {
        let mut lib = VfxLibrary::new();
        lib.add(rate_def("many", 1000.0, 1_000_000))
            .expect("valid fixture");
        let mut scene = VfxScene::new(lib);
        scene
            .spawn("many", Vec3::ZERO, Vec3::Z, 1)
            .expect("valid fixture");
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        scene.update(1.0 / 60.0, &lod, 10);
        assert_eq!(scene.last_scheduled(), 10, "global headroom caps the first frame");
    }

    #[test]
    fn smoke_budget_stops_alpha_spawn_and_lets_additive_through() {
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        let mut smoke_lib = VfxLibrary::new();
        let mut smoke = rate_def("smoke", 1000.0, 1_000_000);
        smoke.emitters[0].render.blend = BlendMode::Alpha;
        smoke_lib.add(smoke).expect("smoke");
        let mut smoke_scene = VfxScene::new(smoke_lib);
        smoke_scene.spawn("smoke", Vec3::ZERO, Vec3::Z, 1).expect("smoke");
        smoke_scene.update_budgets(1.0, &lod, 100_000, 4);
        assert!(
            smoke_scene.last_scheduled() <= 4,
            "alpha spawn ignored the smoke budget ({})",
            smoke_scene.last_scheduled()
        );

        let mut spark_lib = VfxLibrary::new();
        let mut sparks = rate_def("sparks", 1000.0, 1_000_000);
        sparks.emitters[0].render.blend = BlendMode::Additive;
        spark_lib.add(sparks).expect("sparks");
        let mut spark_scene = VfxScene::new(spark_lib);
        spark_scene
            .spawn("sparks", Vec3::ZERO, Vec3::Z, 2)
            .expect("sparks");
        spark_scene.update_budgets(1.0, &lod, 100_000, 4);
        assert!(
            spark_scene.last_scheduled() > 4,
            "additive spawn was capped by the smoke budget ({})",
            spark_scene.last_scheduled()
        );
    }

    /// The budget stats agree with the scheduler: per-instance live counts within each system
    /// budget, the scene total within the global capacity, the alpha count within the smoke
    /// budget, and the sum of instances equal to the scene total.
    #[test]
    fn budget_stats_follow_the_budgets() {
        let mut lib = VfxLibrary::new();
        let mut smoke = rate_def("smoke", 640.0, 200);
        smoke.emitters[0].spawn.lifetime = RangeDef { min: 2.0, max: 2.0 };
        lib.add(smoke).expect("smoke");
        let mut debris = rate_def("debris", 960.0, 150);
        debris.emitters[0].render.mode = RenderMode::Mesh;
        debris.emitters[0].render.blend = BlendMode::Additive;
        lib.add(debris).expect("debris");
        let mut scene = VfxScene::new(lib);
        for seed in 0..3 {
            scene.spawn("smoke", Vec3::ZERO, Vec3::Z, seed).expect("smoke");
            scene.spawn("debris", Vec3::X, Vec3::Z, seed).expect("debris");
        }
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        let (capacity, smoke_budget) = (700, 450);
        for frame in 0..256 {
            scene.update_budgets(1.0 / 64.0, &lod, capacity, smoke_budget);
            let stats = scene.budget_stats();
            assert!(stats.live <= capacity, "frame {frame}: {} > capacity", stats.live);
            assert!(
                stats.live_alpha <= smoke_budget,
                "frame {frame}: smoke over budget"
            );
            assert_eq!(stats.live, stats.instances.iter().map(|i| i.live).sum::<u32>());
            for instance in &stats.instances {
                assert!(instance.live <= instance.budget, "{instance:?} over its budget");
            }
            // Without an upload every scheduled job waits; it still counts once.
            assert_eq!(stats.pending_jobs, stats.live);
        }
        let stats = scene.budget_stats();
        // Deaths free their budget in the frame they happen: the pool stays full (the demand,
        // 75 a frame, covers every frame's deaths), and smoke sits within one frame of smoke
        // spawns (30) of its budget.
        assert_eq!(stats.live, capacity, "the global capacity is reachable");
        assert!(
            stats.live_alpha + 30 >= smoke_budget,
            "smoke stuck below its budget: {}",
            stats.live_alpha
        );
        assert!(stats.live_mesh > 0 && stats.live_mesh == stats.live - stats.live_alpha);
        assert_eq!(stats.instances.len(), 6);
        assert!(stats.instances.iter().all(|i| i.active && i.age > 3.9));
        scene.stop_all();
        scene.update_budgets(1.0 / 64.0, &lod, capacity, smoke_budget);
        assert!(scene.budget_stats().instances.iter().all(|i| !i.active));
    }

    /// A particle dying this frame frees the global budget this frame: a pool kept full by a
    /// burst refills on the exact frame its particles die (the headroom used to be measured
    /// before the deaths, a frame late).
    #[test]
    fn deaths_free_the_global_budget_the_same_frame() {
        let mut lib = VfxLibrary::new();
        let mut def = rate_def("steady", 640.0, 1_000);
        def.emitters[0].spawn.lifetime = RangeDef { min: 0.25, max: 0.25 };
        lib.add(def).expect("steady");
        let mut scene = VfxScene::new(lib);
        scene.spawn("steady", Vec3::ZERO, Vec3::Z, 5).expect("spawn");
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        // 10 a frame, a 16-frame lifetime, a 40-particle pool: full after 4 frames, then every
        // frame from 17 on 10 die and 10 spawn. Binary-exact steps: each cohort dies on its
        // own frame.
        let dt = 1.0 / 64.0;
        let mut live = Vec::new();
        for _ in 0..60 {
            scene.update(dt, &lod, 40);
            live.push(scene.estimated_live());
        }
        assert!(
            live[3..].iter().all(|&n| n == 40),
            "pool not refilled each frame: {live:?}"
        );
    }

    #[test]
    fn unknown_effect_spawn_returns_none() {
        let mut scene = VfxScene::new(VfxLibrary::new());
        assert!(scene.spawn("missing", Vec3::ZERO, Vec3::Z, 0).is_none());
    }

    #[test]
    fn global_capacity_is_shared_across_instances() {
        let mut lib = VfxLibrary::new();
        lib.add(rate_def("many", 1000.0, 1_000_000))
            .expect("valid fixture");
        let mut scene = VfxScene::new(lib);
        for seed in 1..=3 {
            scene
                .spawn("many", Vec3::ZERO, Vec3::Z, seed)
                .expect("valid fixture");
        }
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        scene.update(1.0 / 60.0, &lod, 10);
        assert_eq!(
            scene.last_scheduled(),
            10,
            "three instances share one global budget"
        );
    }

    #[test]
    fn smoke_budget_is_shared_across_instances() {
        let mut lib = VfxLibrary::new();
        let mut smoke = rate_def("smoke", 1000.0, 1_000_000);
        smoke.emitters[0].render.blend = BlendMode::Alpha;
        lib.add(smoke).expect("smoke");
        let mut scene = VfxScene::new(lib);
        for seed in 1..=3 {
            scene.spawn("smoke", Vec3::ZERO, Vec3::Z, seed).expect("smoke");
        }
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        scene.update_budgets(1.0, &lod, 100_000, 4);
        assert_eq!(scene.last_scheduled(), 4, "smoke budget is scene-wide");
    }
}
