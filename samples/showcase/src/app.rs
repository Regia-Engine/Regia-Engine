// SPDX-License-Identifier: MIT-0
//! The showcase app: a [`ShowcasePlugin`] that turns a loaded [`SceneData`] into a running scene
//! on the engine's ECS runtime path, plus the headless and windowed entry points.
//!
//! Frame order (the 60 Hz fixed step): the director samples the track and moves the tracked
//! entity, applies due weather changes, then fires the spawns due on this frame. The clock is the
//! step count, so nothing depends on wall time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use regia::prelude::*;
use regia_app::{FixedTime, MAX_SUBSTEPS_LIMIT, TimeUpdateStrategy, enable_fixed_step};
use regia_core::math::{Mat3, Quat, Vec3};
use regia_render::primitives::{Capsule, Cuboid};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, ExtractedFrame,
    MeshLodChain, MeshRenderer, PostSettings, RenderDevice, RenderPlugin, RenderProfilePreset,
    RenderSettings, RuntimeRenderer, SharedPassDraw, ValleyMist,
};
use regia_scene::level::LevelSettings;
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::{
    CdlodConfig, ContactMap, GrassLayer, PlacedInstance, PlacementInputs, ScatterControl, TerrainGrassHandle,
    TerrainMap, TerrainSharedDraw, WaterPlugin, place_set,
};
use regia_vfx::{
    DecalLibrary, EntryStreak, EntryStreakPlugin, PlaceDecal, SpawnDecal, SpawnEffect, VfxPlugin,
    VfxRuntimePlugin, VfxScene, Weather, place_decal,
};

use crate::scene::{SceneData, TrackMeshes};
use crate::sky::WeatherChange;
use crate::spawns::{self, Event, Ground, Schedule};
use crate::track::{HeightMode, Track};
use crate::{ErrorCode, STEP_HZ, ShowcaseError};

/// How the app renders.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Window size (windowed mode) or the default render-out size hint, pixels.
    pub size: (u32, u32),
    /// Plain look: no temporal anti-aliasing, bloom or auto exposure. Two runs agree on every
    /// pixel on lavapipe. The default is the Desktop High film look.
    pub plain: bool,
    /// Scatter mesh level of detail used for the instance entities (0 is the most detailed).
    pub scatter_lod: usize,
    /// Fixed camera exposure (EV100), overriding the manifest's `camera.exposure_ev`. `None` keeps the
    /// manifest's, and without one the render settings' auto exposure.
    pub exposure_ev: Option<f32>,
    /// Keep the GPU-driven cull's Hi-Z occlusion test. Off (the default) makes two renders of one sequence
    /// identical on the same GPU in every look; on is faster in dense scenes (ADR-1402).
    pub hiz: bool,
    /// The GPU-driven draw path (GPU culling, instanced indirect batches; on by default since ADR-1415: the
    /// streamed forest needs instancing, and the renderer's order-fixed cull keeps it repeatable). Off draws on the
    /// CPU path (one draw per instance, ADR-1402's earlier filming default).
    pub gpu_driven: bool,
    /// Draw the scatter as one entity per instance, thinned to the manifest's `max_scatter_instances` (the path
    /// before ADR-1415). Off (the default) streams the whole set through the engine's scatter field
    /// ([`regia_terrain::RuntimeScatter`]): every instance of the map, tiles placed around the camera, LOD
    /// chains with a dithered cross-fade, distance fade, shadow LODs.
    pub entity_scatter: bool,
    /// The render profile preset (`--render-profile`). `None`: Desktop High. Cinematic is gated by platform like
    /// the engine's own switch ([`regia_render::cinematic::gate_preset`]).
    pub profile: Option<RenderProfilePreset>,
    /// A quality-ladder tier named with `--render-profile` (`VeryLow`, `Medium`; ADR-8844): when set it wins
    /// over [`Self::profile`] (which then holds the tier's preset, for the gate) and the tier is applied as
    /// auto quality applies it ([`regia_render::QualityTier::apply`]).
    pub tier: Option<regia_render::QualityTier>,
    /// Ground the scatter's footprints into the terrain (`--contact-ao`): a [`ContactMap`] fed by the streamed
    /// scatter field every frame. Off by default. It needs the streamed field, so it does nothing with
    /// [`Self::entity_scatter`] (the entity path has no footprint list).
    pub contact_ao: bool,
    /// Sway the scatter's trees in a scene wind (`--foliage-wind`, spec 88 § 2.5): the streamed field's glTF
    /// trees and cone stand-ins take [`regia_render::FoliageSway`] ([`swaying_library`]) and the renderer the
    /// [`SHOWCASE_WIND`] breeze. Off by default (the shipped look is still air). The entity path
    /// ([`Self::entity_scatter`]) does not sway.
    pub foliage_wind: bool,
    /// Play the forest ambience bed (`--ambience`, ADR-2442) with generated placeholder loops: its wind follows
    /// [`SHOWCASE_WIND`] when [`Self::foliage_wind`] sways the trees, else the scene is calm and only the air tone
    /// plays. Off by default.
    pub ambience: bool,
    /// Ambient life (`--ambient-life`, ADR-3024): motes, insects, leaves and birds by the scene's time and
    /// weather, a flock and a soaring bird placed near the camera. Off by default.
    pub ambient_life: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            size: (1920, 1080),
            plain: false,
            scatter_lod: 1,
            exposure_ev: None,
            hiz: false,
            gpu_driven: true,
            entity_scatter: false,
            profile: None,
            tier: None,
            contact_ao: false,
            foliage_wind: false,
            ambience: false,
            ambient_life: false,
        }
    }
}

/// The scene the app was built from.
#[derive(Resource, Clone)]
pub struct ShowcaseScene(pub Arc<SceneData>);

/// The scripted clock: the 60 Hz fixed-step count. Starts at `-lead_in_frames`.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShowcaseClock {
    /// The frame the next fixed step evaluates.
    pub frame: i64,
}

/// Counters the tests and the agent read (`showcase` has no agent method of its own; the existing
/// `world.*` and `play.*` methods cover the entity and the clock).
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct ShowcaseStats {
    /// Scatter instances spawned (entity path), or resident in the streamed field last frame.
    pub scatter_instances: usize,
    /// The scatter is drawn by the streamed field ([`Options::entity_scatter`] off).
    pub scatter_streamed: bool,
    /// Draws the streamed field handed to the last frame (instances × material parts, cross-fade copies and
    /// shadow-only draws included are counted once per instance: the field's `emitted`).
    pub scatter_emitted: usize,
    /// The streamed field failed to build (the message).
    pub scatter_error: Option<String>,
    /// Entities those instances took (a glTF mesh takes one per material part).
    pub scatter_entities: usize,
    /// Scatter instances the placement produced before thinning.
    pub scatter_placed: usize,
    /// The terrain draw is installed in the renderer.
    pub terrain_installed: bool,
    /// The terrain draw failed to build (the message).
    pub terrain_error: Option<String>,
    /// Effects requested so far.
    pub effects_fired: u32,
    /// Decals requested so far.
    pub decals_fired: u32,
    /// Weather changes applied so far.
    pub weather_changes: u32,
    /// The grass layer is bound to the terrain draw.
    pub grass_installed: bool,
    /// The grass layer failed to build (the message).
    pub grass_error: Option<String>,
    /// The contact map is bound to the terrain draw and fed by the scatter field.
    pub contact_installed: bool,
    /// Water bodies spawned from `terrain/stream.json`.
    pub water_bodies: u32,
    /// Entry streaks spawned from `entry.ron`.
    pub entry_streaks: u32,
    /// Scatter materials that sway (`--foliage-wind`, [`swaying_library`]).
    pub swaying_materials: usize,
    /// The valley mist resource is in the world.
    pub mist: bool,
    /// Entities the tracked entity wears now (1 for the capsule, one per material part for a glTF mesh).
    pub track_parts: usize,
    /// The tracked entity wears its `swap_mesh` (the director swapped it at `swap_frame`).
    pub track_swapped: bool,
    /// The scene's tunnels are armed (`tunnels/tunnels.ron`, ADR-8825).
    pub tunnels: bool,
    /// Tube meshes spawned (one per passage of the cooked render mesh).
    pub tunnel_meshes: usize,
    /// Tube lights spawned.
    pub tunnel_lights: usize,
}

/// The terrain draw's counters, available once the draw is installed.
#[derive(Resource, Clone)]
pub struct TerrainStatsHandle(pub Arc<regia_terrain::TerrainDrawStats>);

/// Marks the entity driven by the track.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct Tracked(pub String);

/// Present when the track names a `swap_mesh`: whether the entity wears it yet.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackSwap {
    /// The swapped variant is showing.
    pub swapped: bool,
}

/// One entity of the tracked mesh: a shared mesh and material (and a LOD chain for a glTF part).
struct TrackedPart {
    mesh: Arc<CpuMesh>,
    material: Arc<CpuMaterial>,
    lods: Option<MeshLodChain>,
}

/// The parts the tracked entity wears: the capsule of `track`, or the parts of the glTF in `meshes` (the swap
/// variant when `swapped`).
fn tracked_parts(track: &Track, meshes: &TrackMeshes, swapped: bool) -> Vec<TrackedPart> {
    let model = if swapped { &meshes.swap } else { &meshes.base };
    if let Some(model) = model {
        return model
            .parts
            .iter()
            .map(|part| TrackedPart {
                mesh: Arc::clone(&part.mesh),
                material: Arc::new(part.material.clone()),
                lods: Some(part.lods.clone()),
            })
            .collect();
    }
    let capsule = Capsule {
        radius: track.capsule.radius,
        half_length: track.capsule.half_length,
        ..Capsule::default()
    };
    vec![TrackedPart {
        mesh: Arc::new(capsule.build()),
        material: Arc::new(CpuMaterial {
            base_color: [track.color.0, track.color.1, track.color.2, 1.0],
            roughness: 0.5,
            metallic: 0.2,
            ..CpuMaterial::default()
        }),
        lods: None,
    }]
}

/// Spawns `parts` as tracked entities of `name` at `transform`.
fn spawn_tracked(world: &mut World, name: &str, transform: Transform, parts: Vec<TrackedPart>) {
    for part in parts {
        let mut entity = world.spawn((
            Tracked(name.to_string()),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(part.mesh, part.material),
        ));
        if let Some(lods) = part.lods {
            entity.insert(lods);
        }
    }
}

#[derive(Resource)]
struct Director {
    schedule: Schedule,
    changes: Vec<WeatherChange>,
    next_change: usize,
}

struct MapGround<'a>(&'a TerrainMap);

impl Ground for MapGround<'_> {
    fn height(&self, x: f32, z: f32) -> f32 {
        self.0.sample_bilinear(x, z)
    }

    fn normal(&self, x: f32, z: f32) -> Vec3 {
        Vec3::from(self.0.normal_at(x, z))
    }
}

/// One scatter instance ready to become an entity.
#[derive(Clone, Debug, PartialEq)]
pub struct ScatterEntity {
    /// The mesh path the scatter set names.
    pub mesh: String,
    /// Class index in the set.
    pub class: u16,
    /// World transform.
    pub transform: Transform,
}

/// Places the scene's scatter set and returns its instances, thinned evenly to
/// `manifest.max_scatter_instances`, plus the count before thinning. Pure CPU, deterministic.
///
/// # Errors
/// [`ErrorCode::ScatterMalformed`] when placement fails (a density map is missing).
pub fn scatter_entities(data: &SceneData) -> Result<(Vec<ScatterEntity>, usize), ShowcaseError> {
    let Some(set) = &data.scatter else {
        return Ok((Vec::new(), 0));
    };
    let _span = tracing::info_span!("showcase_scatter_place").entered();
    let placed: Vec<PlacedInstance> = place_set(
        set,
        PlacementInputs {
            map: &data.map,
            // Armed tunnels: no instance stands in a mouth.
            holes: data.features.tunnels.as_ref().map(|t| &t.holes),
            layers: Some(&data.layers),
            density_maps: Some(&data.density_maps),
        },
    )
    .map_err(|e| ShowcaseError::new(ErrorCode::ScatterMalformed, &data.dir, e.to_string()))?;
    let total = placed.len();
    let cap = data.manifest.max_scatter_instances as usize;
    let pick: Vec<usize> = if total <= cap {
        (0..total).collect()
    } else {
        let step = total as f64 / cap as f64;
        (0..cap)
            .map(|k| ((k as f64 * step) as usize).min(total - 1))
            .collect()
    };
    let mut out = Vec::with_capacity(pick.len());
    for index in pick {
        let inst = &placed[index];
        let Some(class) = set.classes.get(usize::from(inst.class)) else {
            continue;
        };
        let Some(mesh_ref) = class.meshes.get(usize::from(inst.mesh)) else {
            continue;
        };
        let up = Vec3::from(inst.up).normalize_or(Vec3::Y);
        let rotation = Quat::from_rotation_arc(Vec3::Y, up) * Quat::from_rotation_y(inst.yaw);
        let translation = Vec3::from(inst.position) - up * (class.sink * inst.scale);
        out.push(ScatterEntity {
            mesh: mesh_ref.mesh.clone(),
            class: inst.class,
            transform: Transform {
                translation,
                rotation,
                scale: Vec3::splat(inst.scale),
            },
        });
    }
    Ok((out, total))
}

/// The rotation that turns local -Z toward `target` from `eye` with +Y up (a camera pose).
#[must_use]
pub fn look_rotation(eye: Vec3, target: Vec3) -> Quat {
    let dir = (target - eye).normalize_or(Vec3::NEG_Z);
    let z = -dir;
    let mut x = Vec3::Y.cross(z);
    if x.length_squared() < 1e-8 {
        x = Vec3::X;
    }
    let x = x.normalize();
    let y = z.cross(x);
    Quat::from_mat3(&Mat3::from_cols(x, y, z))
}

fn tracked_world_position(track: &Track, frame: i64, map: &TerrainMap) -> (Vec3, Quat) {
    let pose = track.sample(frame);
    let mut position = pose.position;
    if track.height_mode == HeightMode::AboveGround {
        position.y += map.sample_bilinear(position.x, position.z);
    }
    (position, pose.rotation)
}

/// Installs a scene into an app: scene plugin, VFX, terrain, scatter, sky, weather, the tracked
/// entity, a camera and a sun, and the director system.
pub struct ShowcasePlugin {
    /// The loaded scene.
    pub scene: Arc<SceneData>,
    /// Rendering options.
    pub options: Options,
}

impl Plugin for ShowcasePlugin {
    fn build(&self, app: &mut App) {
        let data = &self.scene;
        // The effects library goes in before `VfxPlugin` so its startup keeps it.
        let mut library = regia_vfx::scene::VfxLibrary::new();
        for def in &data.vfx_defs {
            if let Err(err) = library.add(def.clone()) {
                tracing::warn!(target: "showcase", "effect {}: {err}", def.name);
            }
        }
        app.insert_resource(VfxScene::new(library));
        app.insert_resource(ShowcaseScene(Arc::clone(&self.scene)))
            .insert_resource(StartOptions(self.options))
            .insert_resource(ShowcaseClock {
                frame: -i64::from(data.manifest.lead_in_frames),
            })
            .init_resource::<ShowcaseStats>()
            .insert_resource(Director {
                schedule: Schedule::new(&data.spawns),
                changes: data.sky.sorted_changes(),
                next_change: 0,
            });

        let level = LevelSettings {
            atmosphere: data.sky.atmosphere.clone(),
            look: data.sky.look.clone(),
            ..LevelSettings::default()
        };
        app.insert_resource(level);
        // `sky.ron` validated the weather; a failure here keeps the engine default.
        let weather = data.sky.initial_weather();
        app.insert_resource(weather);

        if let Some(mist) = &data.features.mist {
            app.insert_resource(mist.clone());
        }
        if data.features.water.is_some() {
            app.add_plugins(WaterPlugin);
        }

        app.insert_resource(Environment {
            ibl: true,
            ambient_lux: 12_000.0,
            double_sided: true,
        });
        app.insert_resource(render_settings(&self.options));
        // Mesh LOD switch sizes are scaled to this height: the filming size, not the 1080p default.
        app.insert_resource(regia_render::LodViewport {
            width: self.options.size.0,
            height: self.options.size.1,
        });

        app.add_plugins(regia_scene::ScenePlugin)
            .add_plugins((VfxPlugin, VfxRuntimePlugin))
            .add_plugins(crate::shake::ShowcaseShakePlugin);
        if data.features.entry.is_some() {
            app.add_plugins(EntryStreakPlugin);
        }
        if let Some(dir) = &data.decal_dir {
            app.insert_resource(DecalDir(dir.clone()));
            app.add_systems(Startup, load_decal_dir);
        }
        if !self.options.entity_scatter {
            app.add_plugins(regia_terrain::ScatterRuntimePlugin);
            // Forest flythrough (ADR-1503): agent terrain.forest_* on the streamed field.
            app.add_plugins(regia_terrain::scatter::forest::ForestFlightPlugin);
            app.add_systems(Last, sync_scatter_stats);
            if self.options.contact_ao {
                app.add_systems(
                    Render,
                    update_contact.after(regia_terrain::extract_runtime_scatter),
                );
            }
        }
        if self.options.ambience {
            app.add_systems(Startup, install_ambience);
        }
        if self.options.ambient_life {
            app.insert_resource(regia_vfx::AmbientLifeSettings {
                enabled: true,
                ..regia_vfx::AmbientLifeSettings::default()
            });
            // A bird spot's calls (`audio.set_ambient_spot`) follow the birds they belong to.
            app.add_plugins(regia_framework::ambient_bird_calls::AmbientBirdCallsPlugin);
        }
        // `tunnels.state`, `tunnels.view` (ADR-8825), registered once every plugin has built.
        #[cfg(feature = "agent")]
        app.add_systems(Startup, crate::tunnels_agent::install);
        app.add_systems(Startup, setup_world)
            .add_systems(Update, (install_terrain, apply_hiz_switch))
            .add_systems(FixedUpdate, director);
        // The camera as a physical actor and the startled bird (ADR-1502).
        app.add_plugins(crate::camera_physics::CameraPhysicsPlugin {
            scene: Arc::clone(&self.scene),
        });
    }
}

#[derive(Resource)]
struct DecalDir(PathBuf);

fn load_decal_dir(dir: Res<DecalDir>, mut library: ResMut<DecalLibrary>) {
    if let Err(err) = library.load_dir(&dir.0) {
        tracing::warn!(target: "showcase", "decal prefabs in {}: {err}", dir.0.display());
    }
}

/// Fixed exposure of the plain look, EV100.
pub const PLAIN_EXPOSURE_EV: f32 = 14.5;

/// The render settings for `options`.
///
/// A non-plain Desktop High or Desktop Ultra run uses `RenderProfile::dense_scenes` (the forest
/// shot's cluster path). Plain mode and every other preset stay on `RenderSettings::from_profile`.
/// Probe GI and the temporal upscaler stay the preset's. The host does not set a lighting option.
#[must_use]
pub fn render_settings(options: &Options) -> RenderSettings {
    if let Some(tier) = options.tier {
        // A tier by name (ADR-8844): its preset with its columns, as the quality ladder writes them.
        let mut settings = RenderSettings::from_profile(tier.preset());
        tier.apply(&mut settings);
        finish_settings(&mut settings, options);
        return settings;
    }
    let requested = options.profile.unwrap_or(RenderProfilePreset::DesktopHigh);
    let gate = regia_render::cinematic::gate_preset(requested, None, cfg!(target_os = "android"), false);
    // `dense_scenes` is `Some` only for Desktop High and Ultra. Plain mode never takes it.
    // `requested` and `gate.applied` agree there: the gate only replaces Cinematic.
    let mut settings = if !options.plain
        && let Some(profile) = regia_render::RenderProfile::dense_scenes(requested)
    {
        let mut settings = RenderSettings::from_profile(requested);
        settings.profile = profile;
        settings
    } else {
        RenderSettings::from_profile(gate.applied)
    };
    finish_settings(&mut settings, options);
    settings
}

/// The showcase's own changes on top of a tier's settings ([`render_settings`], and the demo's tier picker after
/// it switched the tier live): the draw path, the plain look, and a sunlit fixed exposure for a tier without
/// auto exposure.
pub fn finish_settings(settings: &mut RenderSettings, options: &Options) {
    settings.profile.gpu_driven = options.gpu_driven;
    if options.plain {
        // A fixed exposure for a sunlit scene (EV100 14.5), since auto exposure is off.
        settings.post = PostSettings {
            exposure_ev: PLAIN_EXPOSURE_EV,
            ..PostSettings::golden_deterministic()
        };
        settings.profile.anti_aliasing = AntiAliasing::None;
        settings.profile.auto_exposure = false;
        settings.profile.bloom = false;
    } else if !settings.profile.auto_exposure {
        // A tier without auto exposure (Mobile Low) holds `post.exposure_ev`, whose engine default (EV100 0)
        // prints a sunlit scene pure white. Hold the sunlit fixed exposure instead (ADR-2486); a scene's
        // exposure band (`look.min_ev100`/`max_ev100`, its centre), its camera EV or `--exposure-ev` still win.
        settings.post.exposure_ev = PLAIN_EXPOSURE_EV;
    }
}

fn setup_world(
    mut commands: Commands,
    scene: Res<ShowcaseScene>,
    options: Res<StartOptions>,
    mut stats: ResMut<ShowcaseStats>,
) {
    let data = &scene.0;
    let scatter_lod = options.0.scatter_lod;

    // Camera.
    let cam = data.manifest.camera;
    let eye = Vec3::new(cam.position.0, cam.position.1, cam.position.2);
    let target = Vec3::new(cam.target.0, cam.target.1, cam.target.2);
    let transform = Transform {
        translation: eye,
        rotation: look_rotation(eye, target),
        scale: Vec3::ONE,
    };
    commands.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        Camera {
            fov_y: cam.fov_deg.to_radians(),
            near: cam.near,
            far: cam.far,
            // `--exposure-ev`, then the manifest's camera, then a pinned meter range in sky.ron; a fixed EV turns
            // auto exposure off (the renderer's camera override).
            exposure_ev: options.0.exposure_ev.or(cam.exposure_ev).or(data.sky.pinned_ev()),
            clear: ClearMode::Skybox,
            ..Camera::default()
        },
    ));

    // A sun; the sky's sun clock replaces its direction and strength.
    commands.spawn(DirectionalLight {
        direction: [0.3, -0.8, 0.5],
        color: [1.0, 0.96, 0.9],
        illuminance: 100_000.0,
        cast_shadows: true,
        static_shadow: false,
    });

    // An anchor mesh. The engine renders a frame only when it holds at least one mesh, and the terrain is a
    // shared draw, so a scene without a track or scatter would capture nothing ("nothing to capture
    // offscreen"). A 1 cm box 50 m under the lowest ground keeps every frame renderable and is never seen.
    let (lowest, _) = data.map.min_max_height();
    let anchor = Transform {
        translation: Vec3::new(0.0, lowest - 50.0, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };
    commands.spawn((
        anchor,
        GlobalTransform(anchor.compute_affine()),
        MeshRenderer::new(
            Arc::new(Cuboid::new(Vec3::splat(0.01)).build()),
            Arc::new(CpuMaterial::default()),
        ),
    ));

    // Optional features: the stream's water ribbon and the entry streak (the plugins are added in `build`).
    if let Some(body) = &data.features.water {
        commands.spawn(body.clone());
        stats.water_bodies += 1;
    }
    if let Some(plan) = &data.features.entry {
        commands.spawn(plan.streak());
        stats.entry_streaks += 1;
    }
    stats.mist = data.features.mist.is_some();
    if let Some(tunnels) = &data.features.tunnels {
        spawn_tunnels(&mut commands, tunnels, &mut stats);
    }

    // The tracked entity: the capsule, or the parts of the glTF the track names.
    if let Some(track) = &data.track {
        let (position, rotation) = tracked_world_position(track, 0, &data.map);
        let transform = Transform {
            translation: position,
            rotation,
            scale: Vec3::ONE,
        };
        let parts = tracked_parts(track, &data.track_meshes, false);
        stats.track_parts = parts.len();
        for part in parts {
            let mut entity = commands.spawn((
                Tracked(track.entity.clone()),
                transform,
                GlobalTransform(transform.compute_affine()),
                MeshRenderer::new(part.mesh, part.material),
            ));
            if let Some(lods) = part.lods {
                entity.insert(lods);
            }
        }
        if track.swap_frame.is_some() {
            commands.insert_resource(TrackSwap::default());
        }
    }

    if !options.0.entity_scatter {
        // The streamed field: every instance of the set, placed in tiles around the camera.
        if data.scatter.is_some() {
            let built = if options.0.foliage_wind {
                let (library, adopted) = swaying_library(data);
                stats.swaying_materials = adopted;
                build_scatter_field_with(data, &library)
            } else {
                build_scatter_field(data)
            };
            match built {
                Ok(field) => {
                    commands.insert_resource(regia_terrain::RuntimeScatter::new(field));
                    stats.scatter_streamed = true;
                    insert_forest_flight(&mut commands, data);
                }
                Err(err) => {
                    tracing::error!(target: "showcase", "scatter field: {err}");
                    stats.scatter_error = Some(err.to_string());
                }
            }
        }
        return;
    }

    // Scatter: one entity per instance (per material part for a glTF mesh), sharing one mesh and one material per
    // scatter mesh name, class and part, so the GPU-driven path draws each share as one instanced batch.
    match scatter_entities(data) {
        Ok((instances, placed)) => {
            let mut shared: HashMap<(String, u16), (Arc<CpuMesh>, Arc<CpuMaterial>)> = HashMap::new();
            let mut shared_parts: HashMap<(String, u16, usize), Arc<CpuMaterial>> = HashMap::new();
            let Some(set) = &data.scatter else { return };
            let mut entities = 0usize;
            for instance in &instances {
                let tint = set
                    .classes
                    .get(usize::from(instance.class))
                    .map_or([1.0; 3], |c| c.tint);
                if let Some(model) = data.models.get(&instance.mesh) {
                    for (index, part) in model.parts.iter().enumerate() {
                        let material = shared_parts
                            .entry((instance.mesh.clone(), instance.class, index))
                            .or_insert_with(|| {
                                let mut material = part.material.clone();
                                for (channel, t) in material.base_color.iter_mut().zip(tint) {
                                    *channel *= t;
                                }
                                Arc::new(material)
                            });
                        commands.spawn((
                            instance.transform,
                            GlobalTransform(instance.transform.compute_affine()),
                            MeshRenderer::new(Arc::clone(&part.mesh), Arc::clone(material)),
                            part.lods.clone(),
                        ));
                        entities += 1;
                    }
                    continue;
                }
                let key = (instance.mesh.clone(), instance.class);
                if !shared.contains_key(&key) {
                    let Some(asset) = data.library.get(&instance.mesh) else {
                        continue;
                    };
                    let lod = asset.lods.get(scatter_lod).or_else(|| asset.lods.last());
                    let Some(lod) = lod else { continue };
                    let mut material = asset.material.clone();
                    for (channel, t) in material.base_color.iter_mut().zip(tint) {
                        *channel *= t;
                    }
                    shared.insert(
                        key.clone(),
                        (
                            Arc::new(CpuMesh {
                                vertices: lod.vertices.clone(),
                                indices: lod.indices.clone(),
                            }),
                            Arc::new(material),
                        ),
                    );
                }
                let Some((mesh, material)) = shared.get(&key) else {
                    continue;
                };
                commands.spawn((
                    instance.transform,
                    GlobalTransform(instance.transform.compute_affine()),
                    MeshRenderer::new(Arc::clone(mesh), Arc::clone(material)),
                ));
                entities += 1;
            }
            stats.scatter_instances = instances.len();
            stats.scatter_entities = entities;
            stats.scatter_placed = placed;
        }
        Err(err) => tracing::error!(target: "showcase", "{err}"),
    }
}

/// GPU cache-key namespace of the showcase's scatter field.
const SCATTER_KEY_BASE: u64 = 0x5_C0A5E;

/// Builds the streamed scatter field of a scene ([`Options::entity_scatter`] off): the scene's set over its
/// heightfield, layers and density maps, the glTF assets and stand-ins of [`SceneData::library`], one shared
/// material per mesh and part (the class tint in the vertex colours), so each tree texture is resident once.
///
/// # Errors
/// [`ErrorCode::ScatterMalformed`] when the set names a mesh or density map the scene did not load.
pub fn build_scatter_field(data: &SceneData) -> Result<regia_terrain::ScatterField, ShowcaseError> {
    build_scatter_field_with(data, &data.library)
}

/// `Startup` (`--ambience`, ADR-2442): the forest bed with generated placeholder loops (shaped noise, not
/// recordings). The renderer's foliage wind is set directly here (no `SceneWind`), so the bed hears the same
/// breeze through its wind override: still trees, silent wind.
fn install_ambience(
    mut commands: Commands,
    options: Res<StartOptions>,
    sounds: Option<ResMut<regia::audio::SoundAssets>>,
) {
    let mut bed = regia::audio::AmbienceBed::forest();
    bed.wind_override = options.0.foliage_wind.then_some(SHOWCASE_WIND);
    if let Some(mut sounds) = sounds {
        for (layer, text) in regia::audio::test_tone_loopgens() {
            let name = format!("{}{layer}", regia::audio::TEST_TONE_PREFIX);
            match sounds.add_loopgen(name.clone(), &text) {
                Ok(_) => {
                    bed.set_sound(layer, Some(name));
                }
                Err(err) => tracing::warn!(target: "showcase", "ambience test tone {layer}: {err}"),
            }
        }
    }
    commands.insert_resource(bed);
}

/// The breeze `--foliage-wind` sets (m/s on world X and Z, about 5.4 m/s): the trees' own gust curve
/// ([`regia_render::FoliageSway`]) makes it come and go.
pub const SHOWCASE_WIND: [f32; 2] = [5.0, 2.0];

/// The scene's scatter library with its trees swaying in the scene wind (`--foliage-wind`, spec 88 § 2.5):
/// the materials of every glTF mesh (an imported tree) and every [`crate::scene::StandIn::Cone`] stand-in are
/// adopted with [`regia_render::FoliageSway::adopt`] at the plant's height (the top of its finest level above
/// the origin). Rocks and boulders stay still, as does a material with its own vertex stage. Returns the
/// library and the number of materials adopted.
#[must_use]
pub fn swaying_library(data: &SceneData) -> (regia_terrain::ScatterLibrary, usize) {
    let is_tree = |name: &str| {
        let binding = data.manifest.stand_ins.iter().find(|b| b.mesh == name);
        match binding {
            Some(b) => matches!(b.stand_in, crate::scene::StandIn::Cone { .. }),
            None => data.models.contains_key(name),
        }
    };
    let mut library = data.library.clone();
    let mut adopted = 0;
    for (name, asset) in &mut library {
        if !is_tree(name) {
            continue;
        }
        let top = asset.lods.first().map_or(0.0, |lod| {
            lod.vertices.iter().map(|v| v.position[1]).fold(0.0, f32::max)
        });
        let sway = regia_render::FoliageSway {
            height_m: top.max(0.5),
            ..regia_render::FoliageSway::default()
        };
        let materials =
            std::iter::once(&mut asset.material).chain(asset.parts.iter_mut().map(|p| &mut p.material));
        for material in materials {
            if let Ok(swaying) = sway.adopt(material) {
                *material = swaying;
                adopted += 1;
            }
        }
    }
    (library, adopted)
}

/// [`build_scatter_field`] over `library` instead of the scene's own ([`swaying_library`]).
///
/// # Errors
/// [`ErrorCode::ScatterMalformed`] when the set names a mesh or density map the scene did not load.
pub fn build_scatter_field_with(
    data: &SceneData,
    library: &regia_terrain::ScatterLibrary,
) -> Result<regia_terrain::ScatterField, ShowcaseError> {
    let Some(set) = &data.scatter else {
        return Err(ShowcaseError::new(
            ErrorCode::ScatterMalformed,
            &data.dir,
            "the scene has no scatter set",
        ));
    };
    let _span = tracing::info_span!("showcase_scatter_field").entered();
    let source = regia_terrain::ScatterSource {
        map: Arc::new(data.map.clone()),
        holes: tunnel_holes(data),
        layers: Some(data.layers.clone()),
        density_maps: data.density_maps.clone(),
    };
    regia_terrain::ScatterField::new_with(
        set,
        source,
        library,
        SCATTER_KEY_BASE,
        regia_terrain::ScatterBuildOptions {
            shared_materials: true,
        },
    )
    .map_err(|e| ShowcaseError::new(ErrorCode::ScatterMalformed, &data.dir, e.to_string()))
}

/// The armed tunnels' holes for a scatter source: no instance stands in a mouth.
fn tunnel_holes(data: &SceneData) -> Option<regia_terrain::HolesMask> {
    data.features.tunnels.as_ref().map(|t| t.holes.clone())
}

/// Spawns the armed tunnels (ADR-8825): one mesh per passage of the cooked render mesh, the tube lights, and the
/// cooked indoor cells as the renderer's [`regia_render::viscells::VisCells`] (the portal cull).
fn spawn_tunnels(commands: &mut Commands, tunnels: &crate::tunnels::TunnelsPlan, stats: &mut ShowcaseStats) {
    let _span = tracing::info_span!("showcase_spawn_tunnels").entered();
    let rock = tunnels.file.rock;
    let material = Arc::new(CpuMaterial {
        base_color: [rock.0, rock.1, rock.2, 1.0],
        roughness: 0.92,
        ..CpuMaterial::default()
    });
    let origin = Transform::default();
    for tube in &tunnels.meshes {
        commands.spawn((
            origin,
            GlobalTransform(origin.compute_affine()),
            MeshRenderer::new(Arc::clone(&tube.mesh), Arc::clone(&material)),
        ));
    }
    let rig = &tunnels.file.lights;
    for p in &tunnels.lights {
        let at = Transform {
            translation: Vec3::from_array(*p),
            ..Transform::default()
        };
        commands.spawn((
            at,
            GlobalTransform(at.compute_affine()),
            regia_render::PointLight {
                position: *p,
                color: [rig.color.0, rig.color.1, rig.color.2],
                intensity: rig.intensity_lm,
                radius: rig.radius_m,
                cast_shadows: false,
            },
        ));
    }
    commands.insert_resource(tunnels.viscells.clone());
    stats.tunnels = true;
    stats.tunnel_meshes = tunnels.meshes.len();
    stats.tunnel_lights = tunnels.lights.len();
}

/// Inserts the forest flythrough (ADR-1503) over the scene's scatter set: placement queries and corridor
/// reports for the agent's `terrain.forest_*`, motion LOD and wake sway. No mesh library is handed over (it would
/// copy every kit's textures), so a carve is reported with its `.scatter.ron` but not installed in the running field.
fn insert_forest_flight(commands: &mut Commands, data: &SceneData) {
    let Some(set) = &data.scatter else { return };
    let source = regia_terrain::ScatterSource {
        map: Arc::new(data.map.clone()),
        holes: tunnel_holes(data),
        layers: Some(data.layers.clone()),
        density_maps: data.density_maps.clone(),
    };
    match regia_terrain::scatter::forest::ScatterQuery::new(set, source, &data.library) {
        Ok(query) => commands.insert_resource(regia_terrain::scatter::forest::ForestFlight::with_query(
            query,
            None,
            SCATTER_KEY_BASE,
        )),
        Err(err) => tracing::warn!(target: "showcase", "forest flythrough query: {err}"),
    }
}

/// `Last`: copies the streamed field's last frame into [`ShowcaseStats`].
fn sync_scatter_stats(scatter: Option<Res<regia_terrain::RuntimeScatter>>, mut stats: ResMut<ShowcaseStats>) {
    if let Some(scatter) = scatter {
        stats.scatter_instances = scatter.last_frame.resident as usize;
        stats.scatter_emitted = scatter.last_frame.emitted as usize;
    }
}

/// The options the app was started with, read by `setup_world`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct StartOptions(pub Options);

/// `Update`: hands [`Options::hiz`], the order-fixed GPU cull and the alpha-tested foliage switches to the
/// renderer once it exists.
///
/// Live play does not wait for exposure readbacks (ADR-4050). The wait drains the whole GPU queue, so on a
/// GPU-bound device it stalled every second frame (the N150 at Desktop Low: 429 blocking polls in 60 s).
/// A render-out job turns the wait on for its own frames and restores it afterwards, so filmed frames stay
/// repeatable.
fn apply_hiz_switch(
    options: Res<StartOptions>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    if let Some(mut renderer) = renderer {
        renderer.0.set_hiz_occlusion(options.0.hiz);
        // ADR-1415: the GPU cull keeps each batch's instances in submission order (repeatable temporal AA on the
        // GPU-driven path); masked twig cards take the hashed alpha test under TAA (fractional coverage that TAA
        // resolves instead of a shimmering hard edge) and cast alpha-tested shadows, not card outlines.
        renderer.0.set_ordered_gpu_cull(true);
        renderer.0.set_taa_hashed_alpha(true);
        renderer.0.set_alpha_tested_shadows(true);
        if options.0.foliage_wind {
            renderer.0.set_foliage_wind(Some(SHOWCASE_WIND));
        }
        *done = true;
    }
}

/// Build the terrain draw on a warm worker (a map-tier [`regia_render::PipelineWarmQueue`] job) instead of on the
/// frame thread (ADR-5131). The windowed demo inserts it: its terrain pipelines then count for the warm queue, and
/// the main loop keeps presenting the loading splash while they compile. Without it (the headless app, filming
/// runs) the draw is built inline on the first update that has a renderer, as before.
///
/// Not `Clone`: a copy would share the job's result but not whether the job was queued, and queue a second one.
#[derive(Resource)]
pub struct WarmTerrainBuild {
    slot: TerrainSlot,
    queued: bool,
    /// What the job runs: [`build_terrain`] (unit tests swap in a stand-in).
    build: BuildTerrainFn,
}

/// Where the warm job leaves the built terrain (or why it has none) for [`install_terrain`].
type TerrainSlot = Arc<std::sync::Mutex<Option<Result<BuiltTerrain, String>>>>;

/// The terrain error when its warm job panicked: the warm queue records the panic as a job error, and
/// `run_terrain_job` leaves this in the slot so the terrain counts as failed (the windowed demo's uncapped loading
/// screen then finishes instead of waiting for a terrain that never comes).
pub const TERRAIN_JOB_PANICKED: &str = "terrain warm job panicked";

/// The warm job's body: runs `build` and leaves its result in `slot`. A `build` that unwinds (a panic, which the warm
/// queue catches, or the queue's cancel at shutdown) leaves [`TERRAIN_JOB_PANICKED`] (or the cancel error) there
/// instead of nothing.
fn run_terrain_job(
    slot: &TerrainSlot,
    build: impl FnOnce() -> Result<BuiltTerrain, String>,
) -> Result<(), String> {
    /// Fills the slot with an error if it drops armed: the build unwound.
    struct OnUnwind<'a>(Option<&'a TerrainSlot>);
    impl Drop for OnUnwind<'_> {
        fn drop(&mut self) {
            if let Some(slot) = self.0.take() {
                let error = if regia_render::pipeline_warm::warm_cancelled() {
                    regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN
                } else {
                    TERRAIN_JOB_PANICKED
                };
                *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Err(error.to_owned()));
            }
        }
    }
    let mut guard = OnUnwind(Some(slot));
    let built = build();
    guard.0 = None;
    let error = built.as_ref().err().cloned();
    *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(built);
    error.map_or(Ok(()), Err)
}

/// The signature of [`build_terrain`].
type BuildTerrainFn =
    fn(&RenderDevice, &SceneData, bool, Option<&regia_render::RenderProfile>) -> Result<BuiltTerrain, String>;

impl Default for WarmTerrainBuild {
    fn default() -> Self {
        Self {
            slot: Arc::default(),
            queued: false,
            build: build_terrain,
        }
    }
}

impl WarmTerrainBuild {
    /// Whether the build job was queued.
    #[must_use]
    pub fn is_queued(&self) -> bool {
        self.queued
    }
}

/// A terrain draw built off the frame thread, with what goes into the world beside it.
struct BuiltTerrain {
    draw: TerrainSharedDraw,
    /// The grass layer's handle, or the error that left the terrain without grass.
    grass: Option<Result<TerrainGrassHandle, String>>,
    contact: Option<ContactMap>,
}

/// Builds the scene's terrain draw with its grass, contact map and every pipeline it draws with in the HDR formats of
/// `profile` (`prewarm`; none without render settings). Runs inline or on a warm worker ([`WarmTerrainBuild`]).
fn build_terrain(
    device: &RenderDevice,
    scene: &SceneData,
    want_contact: bool,
    profile: Option<&regia_render::RenderProfile>,
) -> Result<BuiltTerrain, String> {
    let _span = tracing::info_span!("showcase_build_terrain").entered();
    let mut draw = TerrainSharedDraw::new(
        device,
        scene.map.clone(),
        &scene.layers,
        CdlodConfig::default(),
        0,
    )
    .and_then(|mut draw| {
        if let Some(material) = &scene.material {
            draw.set_material(material, &[])?;
            draw.set_relief(scene.relief);
        }
        if let Some([pack0, pack1]) = &scene.splat {
            draw.set_splat_weights(Some([pack0, pack1]))?;
        }
        // Armed tunnels (ADR-8825): the mouths' named holes, drawn from the cooked lattice cut.
        if let Some(tunnels) = &scene.features.tunnels {
            draw.set_holes(Some(&tunnels.holes));
            draw.set_hole_lattice(tunnels.lattice.clone());
        }
        Ok(draw)
    })
    .map_err(|err| err.to_string())?;
    let grass = scene.features.grass.as_ref().map(|plan| {
        GrassLayer::new(&device.device, &device.queue, plan.set.clone(), plan.mask.clone())
            .map(|layer| {
                let handle = layer.handle();
                draw.set_grass(Some(layer));
                handle
            })
            .map_err(|err| err.to_string())
    });
    let contact = want_contact.then(|| ContactMap::new(&device.device));
    if let Some(contact) = &contact {
        draw.set_contact_map(Some(contact));
    }
    let formats = profile.map(|profile| regia_terrain::terrain_warm_formats(profile, device));
    for format in formats.into_iter().flatten() {
        if regia_render::pipeline_warm::warm_cancelled() {
            return Err(regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN.to_owned());
        }
        draw.prewarm(format);
    }
    Ok(BuiltTerrain { draw, grass, contact })
}

/// `Update`: installs the terrain draw once the renderer and the device exist: built inline, or taken from the warm
/// job ([`WarmTerrainBuild`]) once it is done (the job is queued on the first such update).
fn install_terrain(world: &mut World) {
    if world.resource::<ShowcaseStats>().terrain_installed
        || world.resource::<ShowcaseStats>().terrain_error.is_some()
    {
        return;
    }
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    if !world.contains_resource::<RuntimeRenderer>() {
        return;
    }
    let scene = world.resource::<ShowcaseScene>().0.clone();
    let contact_ao = world.resource::<StartOptions>().0.contact_ao;
    // The contact map needs the streamed scatter field (it reads its footprints).
    let want_contact = contact_ao && world.contains_resource::<regia_terrain::RuntimeScatter>();
    let profile = world
        .get_resource::<RenderSettings>()
        .map(|settings| settings.profile.clone());
    let queue = world.get_resource::<regia_render::PipelineWarmQueue>().cloned();
    let built = match (world.get_resource_mut::<WarmTerrainBuild>(), queue) {
        (Some(mut warm), Some(queue)) => {
            if !warm.queued {
                warm.queued = true;
                let slot = Arc::clone(&warm.slot);
                let build = warm.build;
                queue.push(
                    regia_render::WarmTier::Map,
                    "showcase terrain",
                    move |device: &RenderDevice| {
                        run_terrain_job(&slot, || build(device, &scene, want_contact, profile.as_ref()))
                    },
                );
                return;
            }
            let taken = warm
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match taken {
                Some(built) => built,
                None => return,
            }
        }
        _ => {
            let _span = tracing::info_span!("showcase_install_terrain").entered();
            build_terrain(&device, &scene, want_contact, profile.as_ref())
        }
    };
    match built {
        Ok(BuiltTerrain {
            mut draw,
            grass,
            contact,
        }) => {
            // Trees and rocks stand on the surface the terrain draw puts on screen, not the true heightfield, so far
            // instances on a coarse level neither float nor sink by its error.
            if let Some(mut scatter) = world.get_resource_mut::<regia_terrain::RuntimeScatter>() {
                scatter.field_mut().set_rendered_surface(Some(draw.surface()));
            }
            if let Some(view) = std::env::var("SHOWCASE_TERRAIN_DEBUG")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
            {
                draw.set_debug_view(view);
            }
            // Agent `terrain.get_debug_view` / `terrain.set_debug_view` switch it on the running draw (ADR-1447).
            world.insert_resource(regia_terrain::TerrainDebugViewControl(draw.debug_view_handle()));
            match grass {
                Some(Ok(handle)) => {
                    world.insert_resource(handle);
                    world.resource_mut::<ShowcaseStats>().grass_installed = true;
                }
                Some(Err(err)) => {
                    tracing::error!(target: "showcase", "grass: {err}");
                    world.resource_mut::<ShowcaseStats>().grass_error = Some(err);
                }
                None => {}
            }
            if let Some(contact) = contact {
                world.insert_resource(ShowcaseContact(contact));
                world.resource_mut::<ShowcaseStats>().contact_installed = true;
            }
            world.insert_resource(TerrainStatsHandle(draw.stats()));
            let boxed: Box<dyn SharedPassDraw + Send + Sync> = Box::new(draw);
            if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
                renderer.0.set_shared_draw(Some(boxed));
            }
            world.resource_mut::<ShowcaseStats>().terrain_installed = true;
        }
        Err(err) => {
            tracing::error!(target: "showcase", "terrain: {err}");
            world.resource_mut::<ShowcaseStats>().terrain_error = Some(err);
        }
    }
}

/// The contact map bound to the terrain draw (`--contact-ao`).
#[derive(Resource)]
pub struct ShowcaseContact(pub ContactMap);

/// `Render`, after the streamed scatter was extracted: re-centres the contact map on the camera and
/// re-rasterizes it from the field's footprints when the window or the field changed. Follows the agent's
/// `terrain.scatter` contact switch.
fn update_contact(
    device: Option<Res<RenderDevice>>,
    contact: Option<ResMut<ShowcaseContact>>,
    scatter: Option<ResMut<regia_terrain::RuntimeScatter>>,
    control: Option<Res<ScatterControl>>,
    extracted: Option<Res<ExtractedFrame>>,
) {
    let (Some(device), Some(mut contact), Some(mut scatter), Some(extracted)) =
        (device, contact, scatter, extracted)
    else {
        return;
    };
    let Some(view) = extracted.primary_view() else {
        return;
    };
    let _span = tracing::info_span!("showcase_update_contact").entered();
    if let Some(control) = control {
        contact.0.set_enabled(control.contact);
    }
    contact
        .0
        .update(&device.queue, scatter.field_mut(), view.camera.eye);
}

/// Present while the interactive demo ([`crate::demo`]) owns the weather: the director still steps through the
/// scene's weather changes but applies none, so a slider is not undone by the next scripted change.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectorWeatherOff;

/// `FixedUpdate`: the director.
fn director(world: &mut World) {
    let _span = tracing::info_span!("showcase_director").entered();
    let frame = world.resource::<ShowcaseClock>().frame;
    let scene = world.resource::<ShowcaseScene>().0.clone();

    // The tracked entity. At its `swap_frame` the entity is replaced by the swap mesh (a pure function of the
    // clock: a frame before it brings the first mesh back).
    if let Some(track) = &scene.track
        && let Some(swap_frame) = track.swap_frame
    {
        let want = frame >= i64::from(swap_frame);
        let swapped = world.get_resource::<TrackSwap>().is_some_and(|s| s.swapped);
        if want != swapped {
            let _span = tracing::info_span!("showcase_track_swap").entered();
            let old: Vec<Entity> = world
                .query_filtered::<Entity, With<Tracked>>()
                .iter(world)
                .collect();
            for entity in old {
                let _ = world.despawn(entity);
            }
            let parts = tracked_parts(track, &scene.track_meshes, want);
            let count = parts.len();
            let (position, rotation) = tracked_world_position(track, frame, &scene.map);
            let transform = Transform {
                translation: position,
                rotation,
                scale: Vec3::ONE,
            };
            spawn_tracked(world, &track.entity, transform, parts);
            world.insert_resource(TrackSwap { swapped: want });
            if let Some(mut stats) = world.get_resource_mut::<ShowcaseStats>() {
                stats.track_parts = count;
                stats.track_swapped = want;
            }
        }
    }
    let entity_pos = scene.track.as_ref().map(|track| {
        let (position, rotation) = tracked_world_position(track, frame, &scene.map);
        let mut query = world.query::<(&Tracked, &mut Transform, &mut GlobalTransform)>();
        for (tracked, mut transform, mut global) in query.iter_mut(world) {
            if tracked.0 == track.entity {
                transform.translation = position;
                transform.rotation = rotation;
                global.0 = transform.compute_affine();
            }
        }
        position
    });

    // The optional features run on the same frame clock: grass sway, mist drift and the entry streak.
    let seconds = frame.max(0) as f32 / STEP_HZ as f32;
    if let Some(handle) = world.get_resource::<TerrainGrassHandle>() {
        handle.set_time(seconds);
    }
    if let Some(mut mist) = world.get_resource_mut::<ValleyMist>() {
        mist.time_s = ValleyMist::time_for_frame(frame.max(0) as u64, STEP_HZ as f32);
    }
    if let Some(plan) = &scene.features.entry {
        let mut query = world.query::<&mut EntryStreak>();
        for mut streak in query.iter_mut(world) {
            streak.frame = plan.streak_frame(frame);
        }
    }

    // Weather changes (none while the interactive demo owns the weather, [`DirectorWeatherOff`]).
    let scripted_weather = !world.contains_resource::<DirectorWeatherOff>();
    let due_changes: Vec<WeatherChange> = {
        let mut director = world.resource_mut::<Director>();
        let mut due = Vec::new();
        while director.next_change < director.changes.len()
            && i64::from(director.changes[director.next_change].frame) <= frame
        {
            due.push(director.changes[director.next_change].clone());
            director.next_change += 1;
        }
        due
    };
    for change in due_changes.into_iter().filter(|_| scripted_weather) {
        if let Ok(state) = change.to.state()
            && let Some(mut weather) = world.get_resource_mut::<Weather>()
        {
            if change.seconds > 0.0 {
                weather.transition_to(state, change.seconds);
            } else {
                weather.set_now(state);
            }
            world.resource_mut::<ShowcaseStats>().weather_changes += 1;
        }
    }

    // Spawns.
    let due = world.resource_mut::<Director>().schedule.take_due(frame);
    let ground = MapGround(&scene.map);
    for event in due {
        match event {
            Event::Effect(effect) => {
                let at = spawns::resolve(&effect.at, entity_pos, &ground);
                world.write_message(SpawnEffect {
                    effect: effect.effect,
                    position: at,
                    forward: Vec3::new(effect.forward.0, effect.forward.1, effect.forward.2)
                        .normalize_or(Vec3::Y),
                    seed: effect.seed,
                });
                world.resource_mut::<ShowcaseStats>().effects_fired += 1;
            }
            Event::Decal(decal) => {
                let position = spawns::resolve(&decal.at, entity_pos, &ground);
                let normal = decal
                    .normal
                    .map_or_else(
                        || spawns::default_normal(&decal.at, entity_pos, &ground),
                        |n| Vec3::new(n.0, n.1, n.2),
                    )
                    .normalize_or(Vec3::Y);
                let yaw = decal.yaw_deg.to_radians();
                match &decal.prefab {
                    Some(prefab) => {
                        place_decal(
                            world,
                            &PlaceDecal {
                                position,
                                normal,
                                yaw,
                                prefab: prefab.clone(),
                                lifetime: decal.lifetime,
                            },
                        );
                    }
                    None => {
                        world.write_message(SpawnDecal {
                            position,
                            rotation: regia_vfx::align_to_normal(normal, yaw),
                            half_size: Vec3::new(decal.half_size.0, decal.half_size.1, decal.half_size.2),
                            color: [decal.color.0, decal.color.1, decal.color.2, decal.color.3],
                            lifetime: decal.lifetime,
                        });
                    }
                }
                world.resource_mut::<ShowcaseStats>().decals_fired += 1;
            }
        }
    }
    world.resource_mut::<ShowcaseClock>().frame = frame + 1;
}

/// `Render` (headless apps only): hands the extracted frame's sky and mesh bookkeeping to the
/// renderer. The windowed runtime does this when it presents; a headless app renders only through
/// the agent's offscreen capture, which does not, so without this the sky would stay at the
/// renderer's default and the level's atmosphere would never apply.
fn sync_headless_renderer(extracted: Res<ExtractedFrame>, mut runtime: ResMut<RuntimeRenderer>) {
    if !extracted.is_renderable() {
        return;
    }
    runtime.0.set_outdoor(extracted.outdoor.clone());
    runtime.0.set_valley_mist(extracted.valley_mist.clone());
    runtime.0.set_mesh_occluders(extracted.mesh_occluders.clone());
    runtime.0.set_temporal_upscaler(extracted.temporal_upscaler);
    // The CPU masked occlusion column (ADR-10072), as the window path hands it over.
    runtime.0.set_masked_occlusion(extracted.masked_occlusion);
    runtime.0.release_meshes(&extracted.released_mesh_keys);
    runtime.0.hold_mesh_identities(extracted.emissive_meshes());
}

/// Most director steps one app frame may run: the cap of the engine's fixed-step accumulator.
///
/// `enable_fixed_step` pins `max_substeps` to 1 (one fixed step per app update, which is right at 60 fps). A
/// render-out at a lower rate gives each frame several 60 Hz steps (`fps 10` is six), and with the cap at 1 the
/// director ran once per frame: the track, the spawns and the weather changes played at a sixth of their speed
/// against the time-driven effects (ADR-1426).
pub const DIRECTOR_MAX_SUBSTEPS: u32 = MAX_SUBSTEPS_LIMIT;

/// Pins the fixed step to [`STEP_HZ`] and lets one frame run as many steps as its time holds
/// ([`DIRECTOR_MAX_SUBSTEPS`]).
pub fn pin_fixed_step(app: &mut App) {
    enable_fixed_step(app, STEP_HZ);
    app.world_mut().resource_mut::<FixedTime>().max_substeps = DIRECTOR_MAX_SUBSTEPS;
}

/// Builds the headless app: no window, a headless renderer, the agent protocol, a paused clock on
/// a fixed 60 Hz step (a `render.out_start` job takes the clock). The caller runs it with
/// [`App::run`] or steps it with [`App::update`].
///
/// The GPU device is created here, so the terrain is installed on the first update and every
/// frame of a job already has it. `project_dir` is where the agent endpoint is advertised
/// (`.regia/agent/`).
///
/// The app has a headless mixer (no output device), so `render.out_start { audio }` writes its
/// WAV mixdown and stems here as in the windowed app (ADR-1561).
///
/// # Errors
/// The device error as text when the machine has no usable Vulkan adapter (not even lavapipe).
pub fn build_headless_app(
    scene: Arc<SceneData>,
    options: Options,
    project_dir: Option<PathBuf>,
) -> Result<App, String> {
    let device = RenderDevice::headless().map_err(|e| format!("no GPU device: {e}"))?;
    #[cfg(not(feature = "agent"))]
    let _ = &project_dir;
    let mut app = App::new();
    app.insert_resource(regia::audio::AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia::audio::AudioPlugin,
    ));
    #[cfg(feature = "agent")]
    app.add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings { project_dir });
    app.insert_resource(device);
    pin_fixed_step(&mut app);
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.add_plugins(ShowcasePlugin { scene, options });
    app.add_systems(
        Render,
        sync_headless_renderer.after(regia_render::runtime::extract_frame),
    );
    Ok(app)
}

/// Runs the windowed app (the engine's default plugins and a window) until it is closed.
pub fn run_windowed(scene: Arc<SceneData>, options: Options, project_dir: Option<PathBuf>) -> AppExit {
    run_windowed_with(scene, options, project_dir, WindowOptions::default())
}

/// How the windowed showcase runs (`--fullscreen`, `--gpu-timing`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowOptions {
    /// Borderless fullscreen on the current monitor: on a Steam Deck the panel's 1280x800, the size the
    /// `Deck` profile is tuned for.
    pub fullscreen: bool,
    /// Per-pass GPU timestamps, so telemetry reports `gpu_ms` and `gpu_pass_ms` (where the adapter has
    /// timestamp queries).
    pub gpu_timing: bool,
}

/// [`run_windowed`] with [`WindowOptions`].
///
/// The window's clock follows the wall clock: the 60 Hz fixed step runs as many steps as a frame's time
/// holds, so the track plays at its real speed under any frame cap, and telemetry's frame times are wall
/// time. (The fixed step's manual 1/60 s clock is kept by the headless app; a `render.out_start` job puts
/// its own clock in and restores this one when it ends.)
pub fn run_windowed_with(
    scene: Arc<SceneData>,
    options: Options,
    project_dir: Option<PathBuf>,
    window: WindowOptions,
) -> AppExit {
    run_windowed_with_replay_roots(scene, options, project_dir, window, Vec::new())
}

/// [`run_windowed_with`] plus directories `replay.load` may read from (`--replay-root DIR`, ADR-1965).
pub fn run_windowed_with_replay_roots(
    scene: Arc<SceneData>,
    options: Options,
    project_dir: Option<PathBuf>,
    window: WindowOptions,
    replay_roots: Vec<PathBuf>,
) -> AppExit {
    run_windowed_with_exit(scene, options, project_dir, window, replay_roots, 0)
}

/// [`run_windowed_with_replay_roots`] plus `--exit-after-frames N` (ADR-5565, found on the Steam Deck pass): with
/// `N > 0` the window quits with [`AppExit::Success`] after `N` frames drawn once any loading splash is gone
/// ([`crate::demo::ExitAfterSceneFrames`], the windowed demo's rule), so a scripted start of the plain showcase
/// ends by itself like the headless one does. `0` runs until the window is closed.
pub fn run_windowed_with_exit(
    scene: Arc<SceneData>,
    options: Options,
    project_dir: Option<PathBuf>,
    window: WindowOptions,
    replay_roots: Vec<PathBuf>,
    exit_after_frames: u64,
) -> AppExit {
    regia::run_with(ShowcasePlugin { scene, options }, move |app| {
        configure_windowed(app, project_dir, window, replay_roots);
        if exit_after_frames > 0 {
            app.add_plugins(crate::demo::ExitAfterSceneFrames(exit_after_frames));
        }
    })
}

/// The windowed app's setup on top of the engine's default plugins (the fixed step, the window, GPU timing,
/// the agent endpoint and the replay roots); [`run_windowed_with_replay_roots`] and the XR walk's windowed
/// runner share it.
pub fn configure_windowed(
    app: &mut App,
    project_dir: Option<PathBuf>,
    window: WindowOptions,
    replay_roots: Vec<PathBuf>,
) {
    {
        #[cfg(feature = "agent")]
        if !replay_roots.is_empty() {
            app.insert_resource(regia_agent::ReplayRoots(replay_roots));
        }
        #[cfg(not(feature = "agent"))]
        let _ = replay_roots;
        pin_fixed_step(app);
        app.insert_resource(TimeUpdateStrategy::Automatic);
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - showcase".into(),
                mode: if window.fullscreen {
                    WindowMode::BorderlessFullscreen
                } else {
                    WindowMode::Windowed
                },
                ..Default::default()
            },
        });
        if window.gpu_timing {
            app.add_systems(Last, enable_gpu_timing);
        }
        if project_dir.is_some() {
            #[cfg(feature = "agent")]
            app.insert_resource(regia_agent::AgentSettings { project_dir });
            #[cfg(not(feature = "agent"))]
            tracing::warn!("showcase: project_dir needs the `agent` feature; ignoring");
        }
    }
}

/// `Last` (with `--gpu-timing`): switches per-pass GPU timestamps on once the renderer exists, on an
/// adapter with timestamp queries.
fn enable_gpu_timing(device: Option<Res<RenderDevice>>, renderer: Option<ResMut<RuntimeRenderer>>) {
    let (Some(device), Some(mut renderer)) = (device, renderer) else {
        return;
    };
    if device.capabilities.timestamp_query && !renderer.0.gpu_timing_enabled() {
        renderer.0.set_gpu_timing(&device, true);
        tracing::info!("showcase: GPU timestamps on");
    }
}

pub use regia_app::AppExit;

/// Runs a headless app to its exit and ends like `regia::run_with` does for the windowed path:
/// [`App::run`] consumes the app, then [`regia::platform::finish_exit`] waits for the GPU workers
/// its drop stopped, so `main` does not return into the exit handlers with a worker inside the
/// driver (ADR-287, ADR-1907).
pub fn run_headless(app: App) -> AppExit {
    let exit = app.run();
    regia::platform::finish_exit(exit_code(&exit), || {});
    exit
}

/// The process exit code of `exit`.
fn exit_code(exit: &AppExit) -> i32 {
    match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    }
}

/// Quits the app after this many `App::update` calls (the clock may be paused, so the count is of
/// updates, not of fixed steps). Used by scripts and tests; agents quit with `quit.request`.
pub struct ExitAfter(pub u64);

impl Plugin for ExitAfter {
    fn build(&self, app: &mut App) {
        let limit = self.0;
        app.add_systems(
            Update,
            move |mut count: Local<u64>, mut exit: MessageWriter<AppExit>| {
                *count += 1;
                if *count >= limit {
                    exit.write(AppExit::Success);
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`build_terrain`] stand-in that panics (an `unwrap` on odd scene data, a device-lost panic).
    fn panicking_build(
        _: &RenderDevice,
        _: &SceneData,
        _: bool,
        _: Option<&regia_render::RenderProfile>,
    ) -> Result<BuiltTerrain, String> {
        panic!("terrain stand-in panics on purpose");
    }

    /// The job body without a device: a build that unwinds leaves [`TERRAIN_JOB_PANICKED`] in the slot (the warm
    /// queue's `run_job` catches the panic the same way); a build error is left as it is.
    #[test]
    fn the_terrain_job_leaves_an_error_in_the_slot_when_its_build_unwinds() {
        let slot = TerrainSlot::default();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_terrain_job(&slot, || panic!("terrain stand-in panics on purpose"))
        }));
        assert!(unwound.is_err(), "the panic still reaches the queue");
        let left = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert!(
            matches!(left, Some(Err(ref e)) if e == TERRAIN_JOB_PANICKED),
            "{:?}",
            left.as_ref().map(|r| r.as_ref().err())
        );

        let failed = run_terrain_job(&slot, || Err("no heightfield".to_owned()));
        assert_eq!(failed, Err("no heightfield".to_owned()));
        let left = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert!(matches!(left, Some(Err(ref e)) if e == "no heightfield"));
    }

    /// fixrev-demo-warm-verify-k: the guard's other branch. A terrain build that unwinds because its queue is
    /// shutting down (the app is quitting) leaves `CANCELLED_AT_SHUTDOWN` in the slot, not [`TERRAIN_JOB_PANICKED`]:
    /// the job runs on a real warm worker, where `warm_cancelled()` turns true at `shutdown`. Needs the headless
    /// device (lavapipe).
    #[test]
    fn a_terrain_build_unwinding_at_shutdown_leaves_the_cancel_error() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("note: no GPU adapter ({err}); skipped");
                return;
            }
        };
        let queue = regia_render::PipelineWarmQueue::new(1);
        queue.start(&device);
        let slot = TerrainSlot::default();
        let (started, on_worker) = std::sync::mpsc::channel();
        let job_slot = Arc::clone(&slot);
        queue.push(
            regia_render::WarmTier::Map,
            "terrain stand-in",
            move |_: &RenderDevice| {
                run_terrain_job(&job_slot, || {
                    let _ = started.send(());
                    while !regia_render::pipeline_warm::warm_cancelled() {
                        std::thread::yield_now();
                    }
                    panic!("terrain stand-in unwinds at shutdown on purpose");
                })
            },
        );
        assert!(
            on_worker
                .recv_timeout(std::time::Duration::from_secs(300))
                .is_ok(),
            "the job runs on the worker"
        );
        queue.shutdown();
        let left = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert!(
            matches!(left, Some(Err(ref e)) if e == regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN),
            "{:?}",
            left.as_ref().map(|r| r.as_ref().err())
        );
    }

    /// fixrev-demo-warm-k (review finding 1): a panic in the terrain's warm job (caught by the warm
    /// queue) left `WarmTerrainBuild`'s slot empty, so `install_terrain` waited forever and the uncapped
    /// loading splash never finished. The job now fills the slot with an error when it unwinds: the
    /// terrain counts as failed and the loading screen may finish. Needs the headless device (lavapipe).
    #[test]
    fn a_panicking_terrain_warm_job_ends_the_loading_screen_with_an_error() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
        let scene = match crate::scene::load_scene(&dir) {
            Ok(scene) => Arc::new(scene),
            Err(err) => panic!("example scene: {err}"),
        };
        let options = Options {
            size: (160, 90),
            plain: true,
            scatter_lod: 2,
            ..Options::default()
        };
        let mut app = match build_headless_app(scene, options, None) {
            Ok(app) => app,
            Err(err) => {
                eprintln!("note: no GPU adapter ({err}); skipped");
                return;
            }
        };
        app.insert_resource(WarmTerrainBuild {
            build: panicking_build,
            ..WarmTerrainBuild::default()
        });
        app.finish_plugins();
        let device = app.world().resource::<RenderDevice>().clone();
        let queue = app.world().resource::<regia_render::PipelineWarmQueue>().clone();
        queue.start(&device);
        for _ in 0..3 {
            app.update();
        }
        assert!(app.world().resource::<WarmTerrainBuild>().is_queued());
        assert!(
            queue.wait_tier(regia_render::WarmTier::Map, std::time::Duration::from_secs(300)),
            "map tier"
        );
        let last_error = queue.progress().last_error;
        assert!(
            last_error.as_deref().is_some_and(|e| e.contains("panicked")),
            "the queue records the panic: {last_error:?}"
        );
        app.update();
        let stats = app.world().resource::<ShowcaseStats>();
        assert!(!stats.terrain_installed);
        assert_eq!(
            stats.terrain_error.as_deref(),
            Some(TERRAIN_JOB_PANICKED),
            "the panic reaches the stats"
        );
        assert!(
            crate::demo::loading_done(stats, Some(&queue)),
            "the loading screen may finish"
        );
        queue.shutdown();
    }

    #[test]
    fn a_tier_without_auto_exposure_holds_a_sunlit_exposure() {
        let low = render_settings(&Options {
            profile: Some(RenderProfilePreset::MobileLow),
            ..Options::default()
        });
        assert!(!low.profile.auto_exposure);
        assert!(
            (low.post.exposure_ev - PLAIN_EXPOSURE_EV).abs() < 1e-6,
            "{}",
            low.post.exposure_ev
        );
        // Tiers with auto exposure keep the engine's settings.
        let high = render_settings(&Options::default());
        assert!(high.post.auto_exposure);
        let engine = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        assert_eq!(high.post.exposure_ev, engine.post.exposure_ev);
    }

    /// ADR-8844: `--render-profile VeryLow` runs the Very Low tier (Desktop Low at a third with SGSR), and
    /// the settings still answer Desktop Low as their preset.
    #[test]
    fn a_tier_by_name_runs_the_tier() {
        let very_low = render_settings(&Options {
            profile: Some(RenderProfilePreset::DesktopLow),
            tier: Some(regia_render::QualityTier::VeryLow),
            ..Options::default()
        });
        assert_eq!(
            regia_render::QualityTier::of_settings(&very_low),
            Some(regia_render::QualityTier::VeryLow)
        );
        assert_eq!(very_low.preset(), Some(RenderProfilePreset::DesktopLow));
        assert!(
            (very_low.profile.render_scale - regia_render::QualityTier::VERY_LOW_RENDER_SCALE).abs() < 1e-6
        );
    }

    /// ADR-4050: live play leaves the exposure readback wait off (it drained the GPU queue every second
    /// frame on a GPU-bound device); render-out jobs turn it on themselves. The other switches still apply.
    #[test]
    fn live_play_does_not_wait_for_exposure_readbacks() {
        use regia::ecs::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.insert_resource(StartOptions(Options::default()));
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        world
            .run_system_once(apply_hiz_switch)
            .expect("the switch system runs");
        let renderer = world.resource::<RuntimeRenderer>();
        assert!(!renderer.0.waits_for_exposure_readback());
        assert!(renderer.0.ordered_gpu_cull());
        assert!(!renderer.0.hiz_occlusion());
    }

    #[test]
    fn the_exit_code_is_the_one_main_reports() {
        assert_eq!(exit_code(&AppExit::Success), 0);
        let error = std::num::NonZeroU8::new(7).expect("not zero");
        assert_eq!(exit_code(&AppExit::Error(error)), 7);
    }
}
