// SPDX-License-Identifier: MPL-2.0
//! The generated bench scenes that carry counters beside their timings (ADR-2695):
//! `synthetic_grass_1m` (a terrain patch with a deterministic one-million-blade field through the
//! terrain crate's grass path, wind on) and `template_npc_200` (200 navigation agents walking a
//! plaza, animated by the crowd animator and drawn skinned).
//!
//! Both are built from code: no fetched content, fixed seeds, nothing committed but this file.
//! [`Extra`] is the plan a [`super::scene::BenchScene`] carries; [`Live`] is what a measured run
//! installs from it. The bench loop calls [`Live::before_frame`] before each frame and
//! [`Live::counters`] after the last, and the counters join the scene's row beside the timings.
//! `--small` ([`SceneSize::Small`]) shrinks both so a software rasterizer runs them in a test.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia::prelude::*;
use regia_animation::demo_rig::{DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::{CrowdAgentDesc, CrowdAgentId, CrowdAnimator, CrowdArchetype, CrowdSettings};
use regia_core::math::{Mat4, Quat, Vec3};
use regia_nav::{CrowdAgentParams, NavCrowd, NavCrowdConfig, NavMesh, NavMeshBuildParams, NavMeshBuilder};
use regia_render::{
    CpuMaterial, CpuMesh, DirectionalLightUniform, DrawStats, FrameQuality, FrameRenderer, FrameTarget,
    JointMatrix, LabScene, MeshRenderer, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SharedPassDraw, SkinJoints,
};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::{
    CdlodConfig, GrassClass, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet,
    LayerDesc, TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw, TerrainWind,
};
use serde_json::{Value, json};

use super::BenchError;
use super::scene::{BenchScene, SceneMesh, unit_box};

/// Id of the grass scene.
pub const GRASS_ID: &str = "synthetic_grass_1m";
/// Id of the crowd scene.
pub const NPC_ID: &str = "template_npc_200";

/// How big the generated scenes are built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SceneSize {
    /// The published size: one million blades, 200 NPCs.
    #[default]
    Full,
    /// A small size for tests on a software rasterizer: 10,000 blades, 20 NPCs.
    Small,
}

/// The grass field's plan: a square patch grown with a fixed number of candidate clumps in every
/// 8 m cell, so the field's blade count is a product of integers.
#[derive(Debug, Clone, PartialEq)]
pub struct GrassPlan {
    /// Edge of the square terrain patch, metres (a multiple of 8, the grass cell edge).
    pub side_m: f32,
    /// Candidate clumps in each 8 m cell.
    pub clumps_per_cell: u32,
    /// Blades in each clump.
    pub blades_per_clump: u32,
    /// The grass set's seed.
    pub seed: u64,
}

impl GrassPlan {
    /// The plan of a size.
    #[must_use]
    pub fn of(size: SceneSize) -> Self {
        match size {
            // 10 x 10 cells x 625 clumps x 16 blades = 1,000,000.
            SceneSize::Full => Self {
                side_m: 80.0,
                clumps_per_cell: 625,
                blades_per_clump: 16,
                seed: 0x6A55_0001,
            },
            // 2 x 2 cells x 250 clumps x 10 blades = 10,000.
            SceneSize::Small => Self {
                side_m: 16.0,
                clumps_per_cell: 250,
                blades_per_clump: 10,
                seed: 0x6A55_0001,
            },
        }
    }

    /// Grass cells over the patch.
    #[must_use]
    pub fn cells(&self) -> u64 {
        let per_side = (self.side_m / CELL_M).round() as u64;
        per_side * per_side
    }

    /// Blades in the whole field: what the renderer should report when the view takes every cell.
    #[must_use]
    pub fn field_blades(&self) -> u64 {
        self.cells() * u64::from(self.clumps_per_cell) * u64::from(self.blades_per_clump)
    }
}

/// The grass cell edge, metres (the terrain crate's grass patch).
const CELL_M: f32 = 8.0;

/// The crowd's plan.
#[derive(Debug, Clone, PartialEq)]
pub struct NpcPlan {
    /// Navigation agents, each with an animated skinned body.
    pub agents: usize,
    /// Half the edge of the square plaza they walk, metres.
    pub half_m: f32,
}

impl NpcPlan {
    /// The plan of a size.
    #[must_use]
    pub fn of(size: SceneSize) -> Self {
        match size {
            SceneSize::Full => Self {
                agents: 200,
                half_m: 20.0,
            },
            SceneSize::Small => Self {
                agents: 20,
                half_m: 20.0,
            },
        }
    }
}

/// What a generated scene adds to the plain meshes of a [`BenchScene`].
#[derive(Debug, Clone, PartialEq)]
pub enum Extra {
    /// The grass field.
    Grass(GrassPlan),
    /// The crowd.
    Npcs(NpcPlan),
}

fn failed(what: &str, err: &dyn std::fmt::Display) -> BenchError {
    BenchError::new("bench_load_failed", format!("{what}: {err}"))
}

/// A scene by id at a size, or `None` when the id is not a generated one.
#[must_use]
pub fn scene(id: &str, size: SceneSize) -> Option<Result<BenchScene, BenchError>> {
    match id {
        GRASS_ID => Some(Ok(grass_scene(&GrassPlan::of(size)))),
        NPC_ID => Some(npc_scene(&NpcPlan::of(size))),
        _ => None,
    }
}

/// A scene with no triangles of its own; the terrain or the crowd draws the content.
fn bare_scene(extra: Extra) -> BenchScene {
    BenchScene {
        meshes: Vec::new(),
        points: Vec::new(),
        materials: 0,
        triangles: 0,
        center: Vec3::ZERO,
        radius: 1.0,
        orbit: 2.2,
        exposure_ev: 14.5,
        probed: super::scene::ProbeCounts::default(),
        extra: Some(extra),
    }
}

/// The terrain's heights: gentle deterministic hills, 0.5 m between samples.
fn hills(side_m: f32) -> Result<TerrainMap, BenchError> {
    let samples = (side_m / 0.5).round() as u32 + 1;
    let mut heights = Vec::with_capacity((samples * samples) as usize);
    for row in 0..samples {
        for col in 0..samples {
            let x = col as f32 * 0.5 - side_m * 0.5;
            let z = row as f32 * 0.5 - side_m * 0.5;
            heights.push(0.9 * (x * 0.11).sin() * (z * 0.09).cos() + 0.25 * (x * 0.37 + z * 0.29).sin());
        }
    }
    TerrainMap::from_heights(samples, samples, heights, side_m, 1.0, 0.0)
        .map_err(|e| failed("terrain patch", &e))
}

fn terrain_layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "meadow".into(),
            albedo: [0.2, 0.26, 0.1],
            roughness: 0.9,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -1000.0,
            altitude_max: 5000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..TerrainLayers::default()
    }
}

/// Summer meadow blades swaying: one class whose candidate count per cell is the plan's.
fn grass_set(plan: &GrassPlan) -> GrassSet {
    let mut class = GrassClass::summer();
    class.name = "bench_meadow".into();
    // Candidates per 8 m cell = ceil(density x 64): a power-of-two quotient is exact.
    class.density = plan.clumps_per_cell as f32 / 64.0;
    class.blades = plan.blades_per_clump;
    class.sway = 0.06;
    class.sway_speed = 0.5;
    class.noise_amount = 0.0;
    GrassSet {
        seed: plan.seed,
        classes: vec![class],
        ..GrassSet::default()
    }
}

/// The scene for a grass plan: the camera stands inside the patch at eye height.
fn grass_scene(plan: &GrassPlan) -> BenchScene {
    let mut scene = bare_scene(Extra::Grass(plan.clone()));
    scene.center = Vec3::new(0.0, 1.8, 0.0);
    scene.radius = plan.side_m * 0.5;
    // Inside: the eye keeps the center's height and circles 6 m out, looking at the center.
    scene.orbit = 6.0 / scene.radius;
    scene.exposure_ev = 14.0;
    scene
}

/// A box of `size` metres on the ground, as a plaza floor.
fn floor_mesh(half_m: f32) -> SceneMesh {
    SceneMesh {
        mesh: Arc::new(unit_box()),
        material: Arc::new(CpuMaterial {
            base_color: [0.32, 0.30, 0.27, 1.0],
            roughness: 0.85,
            ..CpuMaterial::default()
        }),
        model: Mat4::from_translation(Vec3::new(0.0, -0.25, 0.0))
            * Mat4::from_scale(Vec3::new(half_m * 2.0 + 8.0, 0.5, half_m * 2.0 + 8.0)),
    }
}

fn npc_scene(plan: &NpcPlan) -> Result<BenchScene, BenchError> {
    let floor = floor_mesh(plan.half_m);
    let triangles = floor.mesh.indices.len() as u64 / 3;
    let mut scene = bare_scene(Extra::Npcs(plan.clone()));
    scene.meshes.push(floor);
    scene.materials = 1;
    scene.triangles = triangles;
    scene.center = Vec3::new(0.0, 0.9, 0.0);
    scene.radius = plan.half_m * 1.4;
    // Far enough out that the whole plaza is in view, high enough to see over the crowd.
    scene.orbit = 1.6;
    Ok(scene)
}

/// The installed extra of one measured run.
pub enum Live {
    /// The grass field.
    Grass(Box<GrassLive>),
    /// The crowd.
    Npcs(Box<NpcLive>),
}

/// The installed grass field.
pub struct GrassLive {
    plan: GrassPlan,
    handle: TerrainGrassHandle,
    census: Value,
    frames: u64,
}

/// The installed crowd.
pub struct NpcLive {
    plan: NpcPlan,
    mesh: NavMesh,
    nav: NavCrowd,
    crowd: CrowdAnimator,
    /// `(nav agent, animator agent, entity, goal)` per NPC.
    walkers: Vec<(regia_nav::CrowdAgentId, CrowdAgentId, Entity, Vec3)>,
    steps: u64,
    evaluated_poses: u64,
    shared_hits: u64,
    updated: u64,
    skipped: u64,
    retargets: u64,
}

impl Extra {
    /// Installs the extra into a measured run: the grass layer on the renderer's shared draw, or
    /// the crowd's entities in the world.
    ///
    /// # Errors
    /// `bench_load_failed` when the terrain, the grass, the navigation mesh or an agent fails.
    pub fn install(
        &self,
        device: &RenderDevice,
        renderer: &mut FrameRenderer,
        world: &mut World,
    ) -> Result<Live, BenchError> {
        match self {
            Self::Grass(plan) => {
                let (draw, handle, _) = grass_draw(device, plan, true)?;
                renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
                let census = grass_census(device, plan)?;
                Ok(Live::Grass(Box::new(GrassLive {
                    plan: plan.clone(),
                    handle,
                    census,
                    frames: 0,
                })))
            }
            Self::Npcs(plan) => Ok(Live::Npcs(Box::new(npc_install(plan, world)?))),
        }
    }
}

/// Seconds of wind clock per bench frame (the clock is the frame index, so a run repeats).
const SWAY_SECONDS_PER_FRAME: f32 = 1.0 / 60.0;

impl Live {
    /// Called before each frame, warm-up included, with the frame's index.
    pub fn before_frame(&mut self, world: &mut World, index: u32) {
        match self {
            Self::Grass(live) => {
                live.handle.set_time(index as f32 * SWAY_SECONDS_PER_FRAME);
                live.frames += 1;
            }
            Self::Npcs(live) => live.advance(world),
        }
    }

    /// The counters of the run, from the last frame and the totals: they join the scene's row.
    #[must_use]
    pub fn counters(&self, draws: &DrawStats) -> Value {
        match self {
            Self::Grass(live) => live.counters(),
            Self::Npcs(live) => live.counters(draws),
        }
    }
}

type Draw = (TerrainSharedDraw, TerrainGrassHandle, TerrainMap);

/// The terrain draw with the grass layer bound.
fn grass_draw(device: &RenderDevice, plan: &GrassPlan, wind: bool) -> Result<Draw, BenchError> {
    let map = hills(plan.side_m)?;
    let layers = terrain_layers();
    let set = grass_set(plan);
    let mask = GrassMask::build(
        &set,
        &GrassMaskInputs {
            layers: Some(&layers),
            texel_m: 1.0,
            ..GrassMaskInputs::new(&map)
        },
    )
    .map_err(|e| failed("grass mask", &e))?;
    let layer =
        GrassLayer::new(&device.device, &device.queue, set, mask).map_err(|e| failed("grass layer", &e))?;
    let handle = layer.handle();
    // The ultra tier draws every candidate at full density and 16 blades per clump whatever the
    // preset, so the blade count does not move with `--preset`.
    handle.set_request(GrassRequest {
        enabled: true,
        quality: Some(GrassQuality::Ultra),
        ..GrassRequest::default()
    });
    if wind {
        handle.set_wind(TerrainWind::default());
    }
    let mut draw = TerrainSharedDraw::new(device, map.clone(), &layers, CdlodConfig::default(), 5)
        .map_err(|e| failed("terrain", &e))?;
    draw.set_grass(Some(layer));
    Ok((draw, handle, map))
}

/// One frame from above that takes every grass cell: the renderer's own count of the field.
fn grass_census(device: &RenderDevice, plan: &GrassPlan) -> Result<Value, BenchError> {
    let (draw, handle, map) = grass_draw(device, plan, true)?;
    let (width, height) = (64u32, 36u32);
    let post = PostSettings {
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let height_up = plan.side_m * 1.5;
    let ground = map.sample_bilinear(0.0, 0.0);
    let scene = LabScene {
        width,
        height,
        eye: [0.0, ground + height_up, plan.side_m * 0.2],
        target: [0.0, ground, 0.0],
        fov_y: 1.3,
        near: 0.5,
        far: height_up * 4.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 9_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.6, -0.5, -0.4], [1.0, 0.95, 0.85], 50_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("grass census target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let census_failed = |err: &dyn std::fmt::Display| BenchError::new("bench_render_failed", err.to_string());
    renderer
        .render_into(device, &frame, FrameTarget::full(&view, format, width, height))
        .map_err(|err| census_failed(&err))?;
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|err| census_failed(&err))?;
    let report = handle.report();
    let blades = report.candidates * u64::from(plan.blades_per_clump);
    Ok(json!({
        "patches": report.patches,
        "patches_per_level": report.patches_per_level,
        "candidates": report.candidates,
        "blades": blades,
        "drawing": report.drawing,
    }))
}

impl GrassLive {
    fn counters(&self) -> Value {
        let report = self.handle.report();
        let wind = self.handle.wind();
        json!({
            "kind": "grass",
            "field_blades": self.plan.field_blades(),
            "clumps_per_cell": self.plan.clumps_per_cell,
            "blades_per_clump": self.plan.blades_per_clump,
            "cells": self.plan.cells(),
            "census": self.census,
            "last_frame": {
                "drawing": report.drawing,
                "quality": report.quality,
                "patches": report.patches,
                "camera_patches": report.camera_patches,
                "candidates": report.candidates,
                "walked_blades": report.candidates * u64::from(self.plan.blades_per_clump),
                "max_instances": report.max_instances,
            },
            "wind": {
                "strength": wind.strength,
                "sway_m": 0.06,
                "clock_s": report.time_s,
                "frames_prepared": report.frames,
            },
            "frames": self.frames,
        })
    }
}

/// A small deterministic hash for per-agent variety (`0..=1`).
fn hash(i: usize, salt: u32) -> f32 {
    let mut x = (i as u32).wrapping_mul(0x9e37_79b9) ^ salt.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    (x & 0xffff) as f32 / 65535.0
}

/// A blocky body for the demo rig: a box along every bone, skinned to the bone's parent joint, and
/// a head box on the head joint. Two or more joints weigh in, so the skin stays a skin.
fn body_mesh(skeleton: &regia_animation::Skeleton) -> CpuMesh {
    let bind = skeleton.bind_pose_pose();
    let globals = skeleton.globals(&bind, None);
    let template = unit_box();
    let mut mesh = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    let mut add = |from: Vec3, to: Vec3, width: f32, joint: u32| {
        let axis = to - from;
        let length = axis.length();
        if length < 1e-4 {
            return;
        }
        let rotation = Quat::from_rotation_arc(Vec3::Y, axis / length);
        let base = mesh.vertices.len() as u32;
        for vertex in &template.vertices {
            let mut v = *vertex;
            let local =
                Vec3::from_array(v.position) * Vec3::new(width, length, width) + Vec3::Y * (length * 0.5);
            v.position = (from + rotation * local).to_array();
            v.normal = (rotation * Vec3::from_array(v.normal)).to_array();
            let tangent = rotation * Vec3::new(v.tangent[0], v.tangent[1], v.tangent[2]);
            v.tangent = [tangent.x, tangent.y, tangent.z, v.tangent[3]];
            v.joints = [joint, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
            mesh.vertices.push(v);
        }
        mesh.indices.extend(template.indices.iter().map(|i| base + i));
    };
    for (joint, parent) in skeleton.parents.iter().enumerate() {
        let Some(parent) = parent else { continue };
        let (Some(a), Some(b)) = (globals.get(*parent as usize), globals.get(joint)) else {
            continue;
        };
        let torso = joint < 6;
        add(
            a.w_axis.truncate(),
            b.w_axis.truncate(),
            if torso { 0.2 } else { 0.09 },
            *parent,
        );
    }
    if let Some(head) = skeleton
        .joint_index("head")
        .or_else(|| skeleton.joint_index("Head"))
        && let Some(g) = globals.get(head as usize)
    {
        let at = g.w_axis.truncate();
        add(at, at + Vec3::Y * 0.22, 0.2, head);
    }
    mesh
}

/// A flat navigation mesh over the plaza.
fn plaza(half: f32) -> Result<NavMesh, BenchError> {
    let c = |x: f32, z: f32| Vec3::new(x, 0.0, z);
    let tris = [
        [c(-half, -half), c(half, -half), c(half, half)],
        [c(-half, -half), c(half, half), c(-half, half)],
    ];
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .map_err(|e| failed("navigation mesh", &e))
}

/// Seconds per fixed step of the crowd (30 Hz, as the crowd lab runs it).
const STEP: f32 = 1.0 / 30.0;

fn npc_install(plan: &NpcPlan, world: &mut World) -> Result<NpcLive, BenchError> {
    let skeleton = demo_humanoid(&DemoRig::default()).ok_or_else(|| failed("rig", &"demo humanoid"))?;
    let clips = demo_locomotion(&skeleton, &DemoGait::standard());
    let body = Arc::new(body_mesh(&skeleton));
    let archetype =
        CrowdArchetype::new("npc", Arc::new(skeleton), clips).map_err(|e| failed("archetype", &e))?;
    let mesh = plaza(plan.half_m)?;
    let mut nav = NavCrowd::new(NavCrowdConfig {
        step_seconds: STEP,
        max_agents: plan.agents.max(1),
        ..NavCrowdConfig::default()
    })
    .map_err(|e| failed("navigation crowd", &e))?;
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        fixed_step: STEP,
        chunk: 0,
        ..CrowdSettings::default()
    });
    let kind = crowd.add_archetype(archetype);
    let materials: Vec<Arc<CpuMaterial>> = (0..6)
        .map(|k| {
            let t = k as f32 / 6.0;
            Arc::new(CpuMaterial {
                base_color: [0.55 - 0.3 * t, 0.25 + 0.4 * t, 0.2 + 0.5 * t, 1.0],
                roughness: 0.6,
                ..CpuMaterial::default()
            })
        })
        .collect();
    let reach = plan.half_m - 3.0;
    let mut walkers = Vec::with_capacity(plan.agents);
    for i in 0..plan.agents {
        // Two opposing flows across the plaza, in rows, at mixed speeds on a fixed path.
        let west = i % 2 == 0;
        let row = (i / 2) as f32;
        let z = -reach + (row * 1.3) % (reach * 2.0);
        let x = if west { -reach } else { reach } + (hash(i, 1) - 0.5) * 3.0;
        let start = Vec3::new(x, 0.0, z);
        let goal = Vec3::new(-x, 0.0, z + (hash(i, 2) - 0.5) * 6.0);
        let speed = 1.0 + 2.0 * hash(i, 3);
        let nav_id = nav
            .add_agent(&mesh, start, CrowdAgentParams::new(0.3, 1.7, speed, 6.0))
            .map_err(|e| failed("navigation agent", &e))?;
        nav.set_target(&mesh, nav_id, goal)
            .map_err(|e| failed("navigation target", &e))?;
        let id = crowd
            .add_agent(CrowdAgentDesc {
                archetype: kind,
                position: start,
                phase: hash(i, 5),
                ..CrowdAgentDesc::default()
            })
            .map_err(|e| failed("crowd agent", &e))?;
        let entity = world
            .spawn((
                regia_scene::components::Name::new("bench_npc"),
                Transform::from_translation(start),
                GlobalTransform(Transform::from_translation(start).compute_affine()),
                MeshRenderer::new(Arc::clone(&body), Arc::clone(&materials[i % materials.len()])),
            ))
            .id();
        walkers.push((nav_id, id, entity, goal));
    }
    crowd.set_viewer(Some(Vec3::new(0.0, 1.7, -plan.half_m * 1.4)));
    Ok(NpcLive {
        plan: plan.clone(),
        mesh,
        nav,
        crowd,
        walkers,
        steps: 0,
        evaluated_poses: 0,
        shared_hits: 0,
        updated: 0,
        skipped: 0,
        retargets: 0,
    })
}

impl NpcLive {
    /// One fixed step: navigation, then the animator, then every NPC's transform and palette.
    fn advance(&mut self, world: &mut World) {
        let _span = tracing::info_span!("bench_npc_step", agents = self.walkers.len()).entered();
        self.nav.step(&self.mesh);
        for (nav_id, id, _, goal) in &mut self.walkers {
            if let Some(agent) = self.nav.agent(*nav_id) {
                let position = agent.position();
                self.crowd.set_motion(*id, position, agent.velocity());
                // At the goal an agent turns round, so the crowd keeps walking.
                if (position - *goal).length() < 1.5 {
                    let back = Vec3::new(-goal.x, 0.0, goal.z);
                    if self.nav.set_target(&self.mesh, *nav_id, back).is_ok() {
                        *goal = back;
                        self.retargets += 1;
                    }
                }
            }
        }
        self.crowd.step();
        let stats = self.crowd.stats();
        self.steps += 1;
        self.evaluated_poses += stats.evaluated_poses as u64;
        self.shared_hits += stats.shared_hits as u64;
        self.updated += stats.updated as u64;
        self.skipped += stats.skipped as u64;
        for (_, id, entity, _) in &self.walkers {
            let (Some(model), Some(palette)) = (self.crowd.world_transform(*id), self.crowd.palette(*id))
            else {
                continue;
            };
            let (scale, rotation, translation) = model.to_scale_rotation_translation();
            let transform = Transform {
                translation,
                rotation,
                scale,
            };
            let joints: Vec<JointMatrix> = palette.iter().map(|m| JointMatrix::from_mat4(*m)).collect();
            if let Ok(mut e) = world.get_entity_mut(*entity) {
                e.insert((
                    transform,
                    GlobalTransform(transform.compute_affine()),
                    SkinJoints { palette: joints },
                ));
            }
        }
    }

    fn counters(&self, draws: &DrawStats) -> Value {
        let stats = self.crowd.stats();
        let tiers: BTreeMap<String, usize> = stats
            .per_tier
            .iter()
            .enumerate()
            .map(|(tier, n)| (format!("tier{tier}"), *n))
            .collect();
        json!({
            "kind": "npc",
            "agents": self.plan.agents,
            "skinned_draws": draws.skinned_draws,
            "skinned_instanced": draws.skinned_instanced,
            "animation": {
                "steps": self.steps,
                "updates": self.updated,
                "skipped": self.skipped,
                "poses_evaluated": self.evaluated_poses,
                "pose_shares": self.shared_hits,
                "last_step": {
                    "updated": stats.updated,
                    "skipped": stats.skipped,
                    "evaluated_poses": stats.evaluated_poses,
                    "per_tier": tiers,
                    "locked_feet": stats.locked_feet,
                },
            },
            "path": {
                "turned_at_goal": self.retargets,
                "nav_digest": format!("{:016x}", self.nav.digest()),
                "crowd_digest": format!("{:016x}", self.crowd.digest()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published size is exactly one million blades, and the small size ten thousand.
    #[test]
    fn the_plans_multiply_out_to_the_published_counts() {
        let full = GrassPlan::of(SceneSize::Full);
        assert_eq!((full.cells(), full.field_blades()), (100, 1_000_000));
        let small = GrassPlan::of(SceneSize::Small);
        assert_eq!((small.cells(), small.field_blades()), (4, 10_000));
        assert_eq!(NpcPlan::of(SceneSize::Full).agents, 200);
        assert_eq!(NpcPlan::of(SceneSize::Small).agents, 20);
    }

    /// The candidates per cell the grass path derives from the density are the plan's number
    /// (a quotient of a power of two, so the float product is exact).
    #[test]
    fn the_density_gives_the_planned_clumps_per_cell() {
        for size in [SceneSize::Full, SceneSize::Small] {
            let plan = GrassPlan::of(size);
            let set = grass_set(&plan);
            let n = (set.classes[0].density * CELL_M * CELL_M).ceil() as u32;
            assert_eq!(n, plan.clumps_per_cell);
            assert!(set.classes[0].blades <= 16 && set.classes[0].sway > 0.0);
        }
    }

    #[test]
    fn the_generated_scenes_build_without_a_device() {
        for id in [GRASS_ID, NPC_ID] {
            let built = scene(id, SceneSize::Small).expect("id").expect("scene");
            assert!(built.extra.is_some(), "{id}");
        }
        assert!(scene("synthetic_cubes", SceneSize::Small).is_none());
        let npc = scene(NPC_ID, SceneSize::Full).expect("id").expect("scene");
        assert_eq!(
            npc.meshes.len(),
            1,
            "the plaza floor; the crowd spawns at install"
        );
        assert!(npc.orbit > 1.0, "the camera stays outside the plaza");
        let grass = scene(GRASS_ID, SceneSize::Full).expect("id").expect("scene");
        assert!(grass.orbit < 1.0, "the grass camera stays inside the patch");
    }

    /// The body is a real skin: more than one joint carries weight and every vertex is weighted.
    #[test]
    fn the_npc_body_is_skinned_to_several_joints() {
        let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
        let body = body_mesh(&skeleton);
        assert!(body.indices.len() >= 12 * 15, "{}", body.indices.len());
        let mut joints: Vec<u32> = body.vertices.iter().map(|v| v.joints[0]).collect();
        joints.sort_unstable();
        joints.dedup();
        assert!(joints.len() >= 8, "{joints:?}");
        assert!(body.vertices.iter().all(|v| v.weights[0] == 1.0));
        assert!(joints.iter().all(|j| (*j as usize) < skeleton.len()));
    }
}
