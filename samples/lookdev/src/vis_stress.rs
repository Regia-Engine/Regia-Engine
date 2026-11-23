// SPDX-License-Identifier: MIT-0
//! Dense-scatter stress scene for the visibility-buffer path (`docs/specs/27` § 4.2 stage 3).
//!
//! Tens to hundreds of thousands of DO27 stand-in rocks (the scatter library's procedural
//! rocks, LOD 0, shared meshes like the scatter field draws them) on a ground slab around a
//! low chase-height camera, plus DO27's hero buildings at LOD 0 when the content drop is
//! present. The same frames are drawn forward (Desktop High: GPU-driven instanced batches with
//! Hi-Z) and through the visibility buffer, so the two can be timed and compared pixel for
//! pixel. Rocks cast shadows like the scatter field (two local cascades within 190 m, none
//! past it).
//!
//! The geometry is cooked by `regia cook` into a scratch project ([`crate::vis_cooked`]): both
//! paths draw the cooked meshes, and the visibility path reads the cooked `.fmlt` meshlets from
//! the pak (no cooker linked). The forward path gets one [`FrameMesh`] per rock; the visibility
//! path gets the rocks as persistent instance sets ([`FrameRenderer::set_instance_set`]), so its
//! frame list holds only the ground and the heroes.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget, LabMaterial,
    LabVertex, MeshletMesh, PostSettings, RenderDevice, RenderSettings, ShadowCast, VisInstanceSet,
};

use crate::LookdevError;

/// The visibility-buffer path for a run.
#[derive(Clone, Copy, Debug, Default)]
pub struct VisPath {
    /// LOD threshold, cluster capacity, occlusion, cluster shadows.
    pub settings: regia_render::VisBufferSettings,
}

const ROCK_MESH: u64 = 0x0300_0000;
const ROCK_MATERIAL: u64 = 0x0400_0000;
const HERO_MESH: u64 = 0x0100_0000;
const HERO_MATERIAL: u64 = 0x0200_0000;

/// A renderer for one path: the visibility one gets the cooked meshlets and the rock sets.
fn renderer_for(vis: Option<&VisPath>, scene: &StressScene) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    if let Some(vis) = vis {
        renderer.set_visbuffer_settings(vis.settings);
        for (i, hero) in scene.heroes.iter().enumerate() {
            if let Some(m) = &hero.meshlets {
                renderer.insert_meshlets(HERO_MESH + i as u64, Arc::clone(m));
            }
        }
        for (v, rock) in scene.rocks.iter().enumerate() {
            let Some(m) = &rock.meshlets else {
                continue;
            };
            renderer.insert_meshlets(ROCK_MESH + v as u64, Arc::clone(m));
            let (models, shadows): (Vec<_>, Vec<_>) = scene
                .placements
                .iter()
                .filter(|p| p.0 == v)
                .map(|p| (p.1, p.2))
                .unzip();
            renderer.set_instance_set(
                v as u64,
                &VisInstanceSet {
                    mesh_key: ROCK_MESH + v as u64,
                    vertices: &rock.vertices,
                    indices: &rock.indices,
                    material_key: ROCK_MATERIAL + v as u64,
                    material: &rock.material,
                    models: &models,
                    shadow: ShadowCast::All,
                    instance_shadows: &shadows,
                },
            );
        }
    }
    renderer
}

/// Stress-scene shape.
#[derive(Clone, Debug)]
pub struct StressOptions {
    /// Rock instances.
    pub instances: u32,
    /// Rocks are scattered over a disc of this radius around the camera, metres.
    pub radius_m: f32,
    /// Distinct rock meshes (the scatter library's procedural styles).
    pub variants: u32,
    /// Placement seed.
    pub seed: u64,
}

impl Default for StressOptions {
    fn default() -> Self {
        Self {
            instances: 150_000,
            radius_m: 450.0,
            variants: 8,
            seed: 27,
        }
    }
}

/// One cooked mesh of the scene.
struct SceneMesh {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    material: LabMaterial,
    meshlets: Option<Arc<MeshletMesh>>,
}

/// One hero primitive: cooked geometry (node transforms baked in), its placement and material.
struct HeroMesh {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    model: [[f32; 4]; 4],
    material: usize,
    meshlets: Option<Arc<MeshletMesh>>,
}

/// The scene's meshes, built once and referenced by every frame.
pub struct StressScene {
    rocks: Vec<SceneMesh>,
    placements: Vec<(usize, [[f32; 4]; 4], ShadowCast)>,
    heroes: Vec<HeroMesh>,
    hero_materials: Vec<LabMaterial>,
    ground: (Vec<LabVertex>, Vec<u32>),
    ground_material: LabMaterial,
    /// Triangles drawn per frame at full detail (every rock's LOD 0 plus the heroes).
    pub full_triangles: u64,
    /// Meshes with cooked meshlets / without.
    pub meshlet_meshes: (usize, usize),
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        // SplitMix64 (Steele, Lea & Flood, OOPSLA 2014).
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// DO27's hero building glTFs (LOD 0 files under `<content>/hero/buildings`), sorted.
fn hero_files(content: Option<&Path>) -> Vec<std::path::PathBuf> {
    let Some(content) = content else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(content.join("hero").join("buildings")) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read_dir(e.path()).ok())
        .flat_map(|d| d.filter_map(Result::ok).map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "gltf"))
        .collect();
    files.sort();
    files
}

/// Builds the scene: generates the rocks, writes and cooks the project under `project`
/// (`regia cook`, [`crate::vis_cooked`]) and loads the cooked meshes and meshlets back.
/// `content` is DO27's drop (for the hero buildings), if any.
///
/// # Errors
///
/// The project could not be written or cooked, or a cooked entry or hero material failed to load.
pub fn build_scene(
    options: &StressOptions,
    content: Option<&Path>,
    project: &Path,
) -> Result<StressScene, LookdevError> {
    let mut sources = Vec::new();
    let mut rock_materials = Vec::new();
    for v in 0..options.variants.max(1) {
        let style = regia_terrain::RockPalette::Mars.style(v as usize);
        let asset =
            regia_terrain::procedural_rock(0x0057_0a4d ^ u64::from(v + 1).wrapping_mul(0x9e37_79b9), style);
        let Some(lod0) = asset.lods.first() else {
            continue;
        };
        sources.push((lod0.vertices.clone(), lod0.indices.clone()));
        rock_materials.push(asset.material.clone());
    }
    if sources.is_empty() {
        return Err(LookdevError::Config("no rock meshes".into()));
    }
    let heroes_src = hero_files(content);
    crate::vis_cooked::prepare_project(project, &sources, &heroes_src)?;
    crate::vis_cooked::run_cook(project)?;
    let mut cooked = crate::vis_cooked::load_cooked(project)?;
    let mut meshlet_meshes = (0, 0);
    let mut count = |m: &Option<Arc<MeshletMesh>>| {
        if m.is_some() {
            meshlet_meshes.0 += 1;
        } else {
            meshlet_meshes.1 += 1;
        }
    };
    let mut rocks = Vec::new();
    for (v, material) in rock_materials.into_iter().enumerate() {
        let key = format!("{}#mesh/rock_{v}/primitive_0", crate::vis_cooked::rock_source(v));
        let prim = cooked
            .remove(&key)
            .ok_or_else(|| LookdevError::Config(format!("cooked rock missing: {key}")))?;
        count(&prim.meshlets);
        rocks.push(SceneMesh {
            vertices: prim.vertices,
            indices: prim.indices,
            material,
            meshlets: prim.meshlets,
        });
    }

    let mut rng = Rng(options.seed);
    let mut placements = Vec::with_capacity(options.instances as usize);
    let mut full_triangles = 0u64;
    while placements.len() < options.instances as usize {
        // Uniform over the disc, leaving the camera's own spot clear.
        let r = options.radius_m * rng.next().sqrt();
        if r < 4.0 {
            continue;
        }
        let a = rng.next() * std::f32::consts::TAU;
        let variant = (rng.next() * rocks.len() as f32) as usize % rocks.len();
        // Many small rocks, a few boulders (DO27's scatter classes span 0.2–4 m).
        let size = 0.25 + 3.5 * rng.next().powi(4);
        let yaw = rng.next() * std::f32::consts::TAU;
        let model = Mat4::from_scale_rotation_translation(
            Vec3::splat(size),
            Quat::from_rotation_y(yaw),
            Vec3::new(r * a.cos(), -0.15 * size, r * a.sin()),
        );
        let shadow = if r < 190.0 {
            ShadowCast::Local {
                reach: 3.0 * size + 1.0,
                max_cascades: 2,
                shadow_scale: 1.0,
                shadow_lift: 0.0,
                anchor: None,
                lod_switch_keeps_pages: false,
            }
        } else {
            ShadowCast::Off
        };
        full_triangles += (rocks[variant].indices.len() / 3) as u64;
        placements.push((variant, model.to_cols_array_2d(), shadow));
    }

    // Heroes: cooked geometry, the drop's materials (textures and all) by glTF material index.
    let mut heroes = Vec::new();
    let mut hero_materials = Vec::new();
    let count_files = heroes_src.len().max(1) as f32;
    for (i, file) in heroes_src.iter().enumerate() {
        let base = hero_materials.len();
        let mut discard = Vec::new();
        crate::do27::load_gltf(file, true, 0, &mut discard, &mut hero_materials)?;
        let by_primitive = crate::vis_cooked::primitive_materials(file)?;
        let angle = i as f32 / count_files * std::f32::consts::TAU + 0.3;
        let distance = 70.0 + 90.0 * (i % 3) as f32 / 2.0;
        let place = Mat4::from_translation(Vec3::new(angle.cos() * distance, 0.0, angle.sin() * distance))
            * Mat4::from_rotation_y(angle);
        let source = crate::vis_cooked::hero_source(i, file);
        let mut keys: Vec<String> = cooked
            .keys()
            .filter(|k| k.starts_with(&format!("{source}#")))
            .cloned()
            .collect();
        keys.sort();
        for key in keys {
            let sub = key.split_once('#').map_or("", |(_, s)| s);
            // LOD 0 nodes only, as the forward stress scene of round 1.
            if !sub.split('/').nth(1).is_some_and(|node| node.ends_with("_LOD0")) {
                continue;
            }
            let Some(prim) = cooked.remove(&key) else {
                continue;
            };
            count(&prim.meshlets);
            full_triangles += (prim.indices.len() / 3) as u64;
            heroes.push(HeroMesh {
                vertices: prim.vertices,
                indices: prim.indices,
                model: place.to_cols_array_2d(),
                material: base + by_primitive.get(sub).copied().unwrap_or(0),
                meshlets: prim.meshlets,
            });
        }
    }
    let half = options.radius_m + 50.0;
    Ok(StressScene {
        rocks,
        placements,
        heroes,
        hero_materials,
        ground: regia_render::box_mesh([half, 0.5, half]),
        ground_material: LabMaterial {
            base_color: [0.42, 0.27, 0.18, 1.0],
            ..LabMaterial::default()
        },
        full_triangles,
        meshlet_meshes,
    })
}

impl StressScene {
    /// Hero meshes loaded.
    #[must_use]
    pub fn hero_count(&self) -> usize {
        self.heroes.len()
    }

    /// The frame for one camera pose. `rocks_in_list`: one [`FrameMesh`] per rock (the forward
    /// path); otherwise the rocks are the renderer's instance sets and the list holds only the
    /// ground and the heroes.
    #[must_use]
    pub fn frame<'a>(
        &'a self,
        eye: [f32; 3],
        target: [f32; 3],
        size: (u32, u32),
        quality: FrameQuality,
        post: &'a PostSettings,
        rocks_in_list: bool,
    ) -> FrameScene<'a> {
        let rock_count = if rocks_in_list { self.placements.len() } else { 0 };
        let mut meshes = Vec::with_capacity(rock_count + self.heroes.len() + 1);
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 0x00ff_0001,
            material_key: 0x00ff_0002,
            object_key: 0x00ff_0003,
            vertices: &self.ground.0,
            indices: &self.ground.1,
            material: &self.ground_material,
            model: Mat4::from_translation(Vec3::new(0.0, -0.5, 0.0)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
        for (i, mesh) in self.heroes.iter().enumerate() {
            let Some(material) = self.hero_materials.get(mesh.material) else {
                continue;
            };
            meshes.push(FrameMesh {
                is_static: false,
                mesh_key: HERO_MESH + i as u64,
                material_key: HERO_MATERIAL + mesh.material as u64,
                object_key: HERO_MESH + i as u64,
                vertices: &mesh.vertices,
                indices: &mesh.indices,
                material,
                model: mesh.model,
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        if rocks_in_list {
            for (i, (variant, model, shadow)) in self.placements.iter().enumerate() {
                let rock = &self.rocks[*variant];
                meshes.push(FrameMesh {
                    is_static: false,
                    mesh_key: ROCK_MESH + *variant as u64,
                    material_key: ROCK_MATERIAL + *variant as u64,
                    object_key: 0x1_0000_0000 + i as u64,
                    vertices: &rock.vertices,
                    indices: &rock.indices,
                    material: &rock.material,
                    model: *model,
                    joints: &[],
                    receiver_layer: 0,
                    shadow: *shadow,
                    uv1: &[],
                    lod_dither: 0.0,
                });
            }
        }
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        FrameScene {
            width: size.0,
            height: size.1,
            eye,
            target,
            // 90° horizontal, as the DO27 chase camera.
            fov_y: 2.0 * ((90.0f32.to_radians() * 0.5).tan() / aspect).atan(),
            near: 0.1,
            far: 4_000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: regia_render::Environment::default().ambient_lux,
            double_sided: false,
            post: Some(post),
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new([-0.45, -0.6, -0.35], [1.0, 0.95, 0.88], 90_000.0),
            local_lights: &[],
            quality,
            meshes,
        }
    }
}

/// The camera at time `t` seconds: 3.5 m up, driving +X at 15 m/s from x = -20 (the chase).
#[must_use]
pub fn dolly_pose(t: f32) -> ([f32; 3], [f32; 3]) {
    let x = -20.0 + 15.0 * t;
    ([x, 3.5, 0.0], [x + 9.0, 1.2, 0.0])
}

/// Desktop High with post (fixed exposure so two paths meter alike), optionally the
/// visibility buffer; `temporal: false` turns TAA/TAAU off (a frame then depends on its own
/// pose only, so a sequence shows LOD changes and nothing of the history).
#[must_use]
pub fn settings(vis: bool, temporal: bool) -> (FrameQuality, PostSettings) {
    let settings = RenderSettings::default();
    let mut post = settings.resolve_post();
    post.auto_exposure = false;
    // Sunny-day EV100 (the "sunny 16" rule, ~15) for the 90 klx sun.
    post.exposure_ev = 14.5;
    let mut quality = settings.quality();
    quality.visibility_buffer = vis;
    if !temporal {
        quality.taau = false;
        post.taa = false;
    }
    (quality, post)
}

/// Timing of one path over a run.
#[derive(Clone, Debug, Default)]
pub struct StressTiming {
    /// GPU frame time per measured frame, ms.
    pub gpu_ms: Vec<f32>,
    /// GPU depth prepass, ms (median).
    pub prepass_ms: f32,
    /// GPU cull interval (the visibility pass lives here), ms (median).
    pub cull_ms: f32,
    /// GPU forward interval (the material resolve lives here), ms (median).
    pub forward_ms: f32,
    /// GPU shadows, ms (median).
    pub shadows_ms: f32,
    /// Wall time per frame with two frames in flight, ms.
    pub wall_ms: Vec<f32>,
    /// CPU `render_into`, ms (median).
    pub cpu_ms: f32,
    /// Visibility-buffer counts after the run.
    pub vis: regia_render::VisBufferStats,
    /// Clusters drawn in the last frame (phase 1 + phase 2), when the path ran.
    pub clusters: Option<u32>,
    /// Shadow-cluster counters of the last frame (work items, clusters, dropped clusters,
    /// dropped work items), when visibility instances cast cluster shadows.
    pub shadow_clusters: Option<[u32; 4]>,
    /// Every GPU interval of the last measured frame ([`regia_render::GpuTimings::report`]).
    pub last_report: String,
    /// Building the frame's mesh list on the host, ms (median; not part of `cpu_ms`).
    pub build_ms: f32,
}

/// Median of `values` (0 when empty).
#[must_use]
pub fn median(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

struct Lane {
    renderer: FrameRenderer,
    quality: FrameQuality,
    post: PostSettings,
    cpu: Vec<f32>,
    build: Vec<f32>,
    gpu: Vec<regia_render::GpuTimings>,
    timing: StressTiming,
}

/// Renders `warmup + frames` frames held at the start pose (`dolly` drives the camera) on the
/// forward path and on `vis`, **alternating frame by frame** into one offscreen sRGB target with
/// two frames in flight, and returns (forward, visibility) timings. Alternating keeps both paths
/// at the same GPU clocks: a frame loop this CPU-bound lets the GPU drop clocks between frames,
/// which would otherwise favour whichever path ran on a warmer GPU.
///
/// # Errors
///
/// A frame or the final poll failed.
#[allow(clippy::too_many_arguments)]
pub fn time_ab(
    device: &RenderDevice,
    scene: &StressScene,
    vis: &VisPath,
    size: (u32, u32),
    warmup: u32,
    frames: u32,
    dolly: bool,
    pngs: Option<(&Path, &Path)>,
) -> Result<(StressTiming, StressTiming), LookdevError> {
    let mut lanes: Vec<Lane> = [None, Some(vis)]
        .into_iter()
        .map(|v| {
            let (quality, post) = settings(v.is_some(), true);
            let mut renderer = renderer_for(v, scene);
            renderer.set_gpu_timing(device, true);
            Lane {
                renderer,
                quality,
                post,
                cpu: Vec::new(),
                build: Vec::new(),
                gpu: Vec::new(),
                timing: StressTiming::default(),
            }
        })
        .collect();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vis stress target"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
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
    let mut in_flight: VecDeque<wgpu::SubmissionIndex> = VecDeque::new();
    for i in 0..warmup + frames {
        let t = if dolly {
            i.saturating_sub(warmup) as f32 / 60.0
        } else {
            0.0
        };
        let (eye, target) = dolly_pose(t);
        for lane in &mut lanes {
            let started = Instant::now();
            let frame = scene.frame(
                eye,
                target,
                size,
                lane.quality,
                &lane.post,
                !lane.quality.visibility_buffer,
            );
            let built = started.elapsed().as_secs_f32() * 1000.0;
            lane.renderer
                .render_into(device, &frame, FrameTarget::full(&view, format, size.0, size.1))?;
            if let Some(submission) = lane.renderer.take_submission() {
                in_flight.push_back(submission);
            }
            if in_flight.len() >= 3
                && let Some(oldest) = in_flight.pop_front()
            {
                device
                    .device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(oldest),
                        timeout: None,
                    })
                    .map_err(|e| LookdevError::Config(e.to_string()))?;
            }
            if i >= warmup {
                lane.timing.wall_ms.push(started.elapsed().as_secs_f32() * 1000.0);
                lane.cpu.push(lane.renderer.cpu_timings().total_ms);
                lane.build.push(built);
                let g = lane.renderer.gpu_timings();
                lane.timing.gpu_ms.push(g.frame_ms);
                lane.gpu.push(g);
            }
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    let mut out = Vec::with_capacity(2);
    for (lane, png) in lanes.into_iter().zip([pngs.map(|p| p.0), pngs.map(|p| p.1)]) {
        let Lane {
            renderer,
            quality,
            post,
            cpu,
            build,
            gpu,
            mut timing,
        } = lane;
        let pick =
            |f: &dyn Fn(&regia_render::GpuTimings) -> f32| median(&gpu.iter().map(f).collect::<Vec<_>>());
        timing.prepass_ms = pick(&|g| g.prepass_ms);
        timing.cull_ms = pick(&|g| g.cull_ms);
        timing.forward_ms = pick(&|g| g.forward_ms);
        timing.shadows_ms = pick(&|g| g.shadows_ms);
        timing.cpu_ms = median(&cpu);
        timing.build_ms = median(&build);
        timing.last_report = gpu.last().map(|g| g.report()).unwrap_or_default();
        timing.vis = renderer.visbuffer_stats();
        timing.clusters = renderer
            .visbuffer_counters(device)?
            .map(|c| c.phase1_clusters + c.phase2_clusters);
        timing.shadow_clusters = renderer.visbuffer_shadow_counters(device)?;
        let mut renderer = renderer;
        if let Some(png) = png {
            let (eye, target) = dolly_pose(if dolly {
                frames.saturating_sub(1) as f32 / 60.0
            } else {
                0.0
            });
            let pixels = renderer
                .render_offscreen(
                    device,
                    &scene.frame(eye, target, size, quality, &post, !quality.visibility_buffer),
                )?
                .pixels;
            write_png(png, size, &pixels)?;
        }
        out.push(timing);
    }
    let vis_timing = out.pop().unwrap_or_default();
    let forward_timing = out.pop().unwrap_or_default();
    Ok((forward_timing, vis_timing))
}

/// Renders the dolly as read-back frames (for the motion metrics), `count` frames at 60 Hz.
///
/// # Errors
///
/// A frame or a readback failed.
pub fn dolly_frames(
    device: &RenderDevice,
    scene: &StressScene,
    vis: Option<&VisPath>,
    size: (u32, u32),
    count: u32,
    temporal: bool,
) -> Result<Vec<Vec<u8>>, LookdevError> {
    let (quality, post) = settings(vis.is_some(), temporal);
    let mut renderer = renderer_for(vis, scene);
    let mut frames = Vec::with_capacity(count as usize);
    // A few held frames first so TAA and the visibility history settle.
    for i in 0..count + 8 {
        let (eye, target) = dolly_pose(i.saturating_sub(8) as f32 / 60.0);
        let pixels = renderer
            .render_offscreen(
                device,
                &scene.frame(eye, target, size, quality, &post, vis.is_none()),
            )?
            .pixels;
        if i >= 8 {
            frames.push(pixels);
        }
    }
    Ok(frames)
}

/// Writes tightly packed RGBA8 as PNG.
///
/// # Errors
///
/// The file or the encoder failed.
pub fn write_png(path: &Path, size: (u32, u32), rgba: &[u8]) -> Result<(), LookdevError> {
    let io = |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    };
    let file = std::fs::File::create(path).map_err(io)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| LookdevError::Config(e.to_string()))
}

/// Motion metrics of a visibility-buffer sequence against the forward sequence of the same poses.
#[derive(Clone, Copy, Debug, Default)]
pub struct MotionCompare {
    /// Mean isolated-pixel flicker of consecutive forward frames.
    pub forward_flicker: f32,
    /// The same for the visibility-buffer frames.
    pub vis_flicker: f32,
    /// Pop count: frame pairs where more than 0.1 % of the 8×8 blocks change their mean
    /// difference to the forward frame by more than 6 levels at once (a LOD switch that shows;
    /// a silhouette that slides with the camera barely moves a block's mean).
    pub pops: u32,
    /// Largest fraction of blocks in one such jump.
    pub worst_jump: f32,
    /// Lowest per-frame mean absolute luma difference to the forward frame, levels.
    pub min_mad: f32,
    /// Highest per-frame mean absolute luma difference to the forward frame, levels.
    pub max_mad: f32,
}

fn luma(px: &[u8]) -> f32 {
    0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2])
}

/// Compares two equally long sequences rendered from the same poses.
#[must_use]
pub fn motion_compare(forward: &[Vec<u8>], vis: &[Vec<u8>], size: (u32, u32)) -> MotionCompare {
    let forward_refs: Vec<&[u8]> = forward.iter().map(Vec::as_slice).collect();
    let vis_refs: Vec<&[u8]> = vis.iter().map(Vec::as_slice).collect();
    let mut out = MotionCompare {
        forward_flicker: regia_render::sequence_flicker(&forward_refs, size.0, size.1),
        vis_flicker: regia_render::sequence_flicker(&vis_refs, size.0, size.1),
        min_mad: f32::MAX,
        ..MotionCompare::default()
    };
    let (w, h) = (size.0 as usize, size.1 as usize);
    let (bw, bh) = (w / 8, h / 8);
    let mut blocks_prev: Option<Vec<f32>> = None;
    for (f, v) in forward.iter().zip(vis) {
        let mut sum = 0.0f32;
        let mut blocks = vec![0.0f32; bw * bh];
        for (i, (a, b)) in f.as_chunks::<4>().0.iter().zip(v.as_chunks::<4>().0).enumerate() {
            let d = luma(b) - luma(a);
            sum += d.abs();
            let (x, y) = (i % w.max(1), i / w.max(1));
            if x / 8 < bw && y / 8 < bh {
                blocks[(y / 8) * bw + x / 8] += d / 64.0;
            }
        }
        let mad = sum / (w * h).max(1) as f32;
        out.min_mad = out.min_mad.min(mad);
        out.max_mad = out.max_mad.max(mad);
        if let Some(prev) = blocks_prev.as_ref() {
            let jumped = prev
                .iter()
                .zip(&blocks)
                .filter(|(a, b)| (*b - *a).abs() > 6.0)
                .count();
            let fraction = jumped as f32 / blocks.len().max(1) as f32;
            out.worst_jump = out.worst_jump.max(fraction);
            if fraction > 0.001 {
                out.pops += 1;
            }
        }
        blocks_prev = Some(blocks);
    }
    if blocks_prev.is_none() {
        out.min_mad = 0.0;
    }
    out
}
