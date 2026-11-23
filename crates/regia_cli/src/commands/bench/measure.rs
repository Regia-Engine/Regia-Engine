// SPDX-License-Identifier: MPL-2.0
//! The bench's frame loop: the runtime path (an ECS world, `extract_frame`, a persistent
//! `FrameRenderer` and target), Desktop High (or the `--preset` tier), two frames in flight, and
//! percentiles. `--on` turns opt-in renderer features on for their cost ([`BenchFeatures`]).

use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::path::Path;
use std::time::Instant;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{
    Camera, DirectionalLight, ExtractedFrame, FrameRenderer, FrameTarget, MeshRenderer, PointLight,
    RenderDevice, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

use super::BenchError;
use super::scene::BenchScene;

/// How a scene is measured.
#[derive(Debug, Clone, Copy)]
pub struct MeasureOptions {
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// Frames before the measured ones: pipelines, uploads, history.
    pub warmup: u32,
    /// Measured frames.
    pub frames: u32,
    /// Keep the camera still instead of one orbit over the measured frames.
    pub held: bool,
    /// The render tier (Desktop High unless `--preset` names another).
    pub preset: RenderProfilePreset,
    /// Opt-in renderer features `--on` turns on (none by default).
    pub features: BenchFeatures,
}

/// The opt-in renderer features `regia bench --on` takes, each off in every shipped preset; the
/// first six are `do27_chase --on`'s (`xtask gpu-bench --suite features` toggles both). The last
/// three are material probes ([`super::scene::MaterialProbes`]): the loader films or reads the
/// transmission of the scene's materials, so the feature has work to cost.
pub const FEATURE_NAMES: [&str; 9] = [
    "radiance-cache",
    "blue-noise",
    "oit",
    "sss",
    "shadow-occlusion",
    "lens-effects",
    "thin-film",
    "transmission",
    "rough-transmission",
];

/// Which of [`FEATURE_NAMES`] are on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BenchFeatures {
    /// The radiance cache behind screen-space GI (ADR-2085).
    pub radiance_cache: bool,
    /// Spatiotemporal blue noise for SSR, SSGI and SSAO (ADR-2075).
    pub blue_noise: bool,
    /// Weighted blended order-independent transparency (ADR-2065).
    pub oit: bool,
    /// Screen-space subsurface scattering (ADR-2049).
    pub sss: bool,
    /// Two-phase occlusion culling of the shadow casters (ADR-2072).
    pub shadow_occlusion: bool,
    /// The lens-effects pass at the cinematic strengths (ADR-1863).
    pub lens_effects: bool,
    /// The material probes (`thin-film`, `transmission`, `rough-transmission`).
    pub materials: super::scene::MaterialProbes,
}

impl BenchFeatures {
    /// Parses `--on` names.
    ///
    /// # Errors
    ///
    /// The unknown name.
    pub fn parse(names: &[String]) -> Result<Self, String> {
        let mut on = Self::default();
        for name in names {
            match name.trim() {
                "radiance-cache" => on.radiance_cache = true,
                "blue-noise" => on.blue_noise = true,
                "oit" => on.oit = true,
                "sss" => on.sss = true,
                "shadow-occlusion" => on.shadow_occlusion = true,
                "lens-effects" => on.lens_effects = true,
                "thin-film" => on.materials.thin_film = true,
                "transmission" => on.materials.transmission = true,
                "rough-transmission" => on.materials.rough_transmission = true,
                "" => {}
                other => return Err(other.to_owned()),
            }
        }
        Ok(on)
    }

    /// The names that are on, in [`FEATURE_NAMES`] order.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        let flags = [
            self.radiance_cache,
            self.blue_noise,
            self.oit,
            self.sss,
            self.shadow_occlusion,
            self.lens_effects,
            self.materials.thin_film,
            self.materials.transmission,
            self.materials.rough_transmission,
        ];
        FEATURE_NAMES
            .iter()
            .zip(flags)
            .filter_map(|(name, on)| on.then_some(*name))
            .collect()
    }

    /// The settings the bench's world renders with: `preset`'s, with these features on.
    #[must_use]
    pub fn settings(self, preset: RenderProfilePreset) -> RenderSettings {
        let mut settings = RenderSettings::from_profile(preset);
        settings.profile.blue_noise |= self.blue_noise;
        settings.profile.order_independent_transparency |= self.oit;
        settings.profile.subsurface_scattering |= self.sss;
        if self.radiance_cache {
            settings.radiance_cache = Some(true);
        }
        if self.lens_effects {
            settings.post.lens_effects = regia_render::LensEffects::CINEMATIC;
        }
        settings
    }

    /// Turns the renderer-held features on (the radiance cache, the shadow cull, the lens pass).
    ///
    /// # Errors
    ///
    /// The lens pass did not register.
    pub fn apply(self, renderer: &mut FrameRenderer) -> Result<(), String> {
        if self.radiance_cache {
            renderer.set_radiance_cache(true);
        }
        if self.shadow_occlusion {
            renderer.set_shadow_occlusion(true);
        }
        if self.lens_effects && !renderer.enable_lens_effects() {
            return Err("the lens-effects pass did not register".to_owned());
        }
        Ok(())
    }
}

/// Nearest-rank percentiles of a series, in its own units.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Percentiles {
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Worst sample.
    pub max: f64,
    /// Arithmetic mean.
    pub mean: f64,
}

impl Percentiles {
    /// The percentiles of `samples`; all zero when it is empty.
    #[must_use]
    pub fn of(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        // Nearest rank: the smallest value with at least q of the samples at or under it.
        let rank = |q: f64| {
            let n = sorted.len();
            let index = ((q * n as f64).ceil() as usize).clamp(1, n) - 1;
            sorted[index]
        };
        Self {
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: sorted[sorted.len() - 1],
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
        }
    }

    fn to_json(self) -> Value {
        let round = |v: f64| (v * 1000.0).round() / 1000.0;
        json!({
            "p50": round(self.p50),
            "p95": round(self.p95),
            "p99": round(self.p99),
            "max": round(self.max),
            "mean": round(self.mean),
        })
    }
}

/// One scene's result.
#[derive(Debug, Clone)]
pub struct SceneStats {
    /// Frames measured.
    pub frames: u32,
    /// Wall time per frame, ms.
    pub wall: Percentiles,
    /// `render_into` CPU recording, ms.
    pub cpu_encode: Percentiles,
    /// `queue.submit` plus waiting for the encode workers, ms.
    pub cpu_submit: Percentiles,
    /// The whole frame encoder on the GPU, ms; `None` without timestamp queries.
    pub gpu_frame: Option<Percentiles>,
    /// Median per pass on the GPU, ms: name to value; empty without timestamp queries.
    pub gpu_passes: Vec<(&'static str, f64)>,
    /// Meshes drawn on the last frame.
    pub drawn: usize,
    /// Meshes culled on the last frame.
    pub culled: usize,
    /// Forward-pass meshes in GPU-driven batches on the last frame.
    pub instanced: usize,
    /// Forward-pass meshes drawn one by one on the last frame.
    pub per_object: usize,
    /// Mean BT.709 luma of the last frame, 0 to 255: a black frame's numbers measure nothing.
    pub mean_luma: f64,
    /// Percent of the last frame's pixels brighter than near-black (any channel over 5).
    pub lit_pct: f64,
    /// The renderer's GPU memory after the last frame (`seeing.memory_report`'s
    /// `allocated_bytes` and `reserved_bytes`); `None` where the backend reports none.
    pub gpu_memory: Option<(u64, u64)>,
    /// A generated scene's counters (grass blades, NPCs and their animation updates), beside the
    /// timings; `None` for the scenes of plain meshes.
    pub counters: Option<Value>,
}

impl SceneStats {
    /// The result as JSON, the same fields `regia bench --json` documents.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut row = json!({
            "frames": self.frames,
            "wall_ms": self.wall.to_json(),
            "fps_p50": if self.wall.p50 > 0.0 { (10_000.0 / self.wall.p50).round() / 10.0 } else { 0.0 },
            "cpu_encode_ms": self.cpu_encode.to_json(),
            "cpu_submit_ms": self.cpu_submit.to_json(),
            "gpu_frame_ms": self.gpu_frame.map_or(Value::Null, Percentiles::to_json),
            "gpu_passes_p50_ms": self.gpu_passes.iter()
                .map(|(name, value)| ((*name).to_string(), json!((value * 1000.0).round() / 1000.0)))
                .collect::<serde_json::Map<_, _>>(),
            "draws": {
                "drawn": self.drawn,
                "culled": self.culled,
                "instanced": self.instanced,
                "per_object": self.per_object,
            },
            "last_frame": {
                "mean_luma": (self.mean_luma * 100.0).round() / 100.0,
                "lit_pct": (self.lit_pct * 100.0).round() / 100.0,
            },
            "gpu_memory": self.gpu_memory.map_or(Value::Null, |(allocated, reserved)| json!({
                "allocated_bytes": allocated,
                "reserved_bytes": reserved,
            })),
        });
        if let (Some(counters), Value::Object(map)) = (&self.counters, &mut row) {
            map.insert("counters".into(), counters.clone());
        }
        row
    }
}

/// The camera at orbit parameter `t` in `[0, 1)`: its eye, looking at the scene's center.
fn orbit_transform(scene: &BenchScene, t: f32) -> Transform {
    let angle = t * TAU;
    let radius = scene.radius * scene.orbit;
    let eye = scene.center
        + Vec3::new(
            angle.cos() * radius,
            scene.radius * scene.orbit * 0.35,
            angle.sin() * radius,
        );
    let eye = if scene.orbit < 1.0 {
        // An interior orbit keeps its height.
        Vec3::new(eye.x, scene.center.y, eye.z)
    } else {
        eye
    };
    let forward = (scene.center - eye).normalize_or_zero();
    Transform {
        translation: eye,
        rotation: Quat::from_rotation_arc(-Vec3::Z, forward),
        scale: Vec3::ONE,
    }
}

/// Spawns the meshes, the camera, the sun and the point lights. Returns the camera.
fn spawn(world: &mut World, scene: &BenchScene) -> Entity {
    for mesh in &scene.meshes {
        let (scale, rotation, translation) = mesh.model.to_scale_rotation_translation();
        let transform = Transform {
            translation,
            rotation,
            scale,
        };
        world.spawn((
            regia_scene::components::Name::new("bench_mesh"),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(mesh.mesh.clone(), mesh.material.clone()),
        ));
    }
    let transform = orbit_transform(scene, 0.0);
    let camera = world
        .spawn((
            regia_scene::components::Name::new("bench_camera"),
            transform,
            GlobalTransform(transform.compute_affine()),
            Camera {
                near: (scene.radius * 0.002).max(0.01),
                far: (scene.radius * 12.0).max(100.0),
                exposure_ev: Some(scene.exposure_ev),
                ..Camera::default()
            },
        ))
        .id();
    let sun = Vec3::new(-0.45, -0.8, -0.4).normalize_or_zero();
    let transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::from_rotation_arc(-Vec3::Z, sun),
        scale: Vec3::ONE,
    };
    world.spawn((
        regia_scene::components::Name::new("bench_sun"),
        transform,
        GlobalTransform(transform.compute_affine()),
        DirectionalLight {
            direction: sun.to_array(),
            color: [1.0, 0.96, 0.9],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    for point in &scene.points {
        let transform = Transform::from_translation(Vec3::from_array(point.position));
        world.spawn((
            regia_scene::components::Name::new("bench_point"),
            transform,
            GlobalTransform(transform.compute_affine()),
            PointLight {
                position: point.position,
                color: point.color,
                intensity: point.intensity,
                radius: point.radius,
                cast_shadows: false,
            },
        ));
    }
    camera
}

/// Mean BT.709 luma and the percent of pixels over near-black, of RGBA8 pixels as stored.
fn frame_brightness(pixels: &[u8]) -> (f64, f64) {
    let rows = pixels.as_chunks::<4>().0;
    if rows.is_empty() {
        return (0.0, 0.0);
    }
    let mut luma = 0.0f64;
    let mut lit = 0usize;
    for [r, g, b, _] in rows {
        luma += 0.2126 * f64::from(*r) + 0.7152 * f64::from(*g) + 0.0722 * f64::from(*b);
        if *r > 5 || *g > 5 || *b > 5 {
            lit += 1;
        }
    }
    (luma / rows.len() as f64, 100.0 * lit as f64 / rows.len() as f64)
}

/// Renders the warm-up and measured frames of one scene, then reads the last frame back (after the
/// timing, so it costs the numbers nothing) and writes it to `png` when asked.
///
/// # Errors
/// `bench_render_failed` when a frame, the readback or the PNG fails.
pub fn measure_scene(
    device: &RenderDevice,
    scene: &BenchScene,
    options: MeasureOptions,
    png: Option<&Path>,
) -> Result<SceneStats, BenchError> {
    let failed = |err: &dyn std::fmt::Display| BenchError::new("bench_render_failed", err.to_string());
    let MeasureOptions {
        width,
        height,
        warmup,
        frames,
        held,
        preset,
        features,
    } = options;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.add_systems(Render, regia_render::extract_frame);
    let camera = spawn(app.world_mut(), scene);
    app.world_mut().insert_resource(features.settings(preset));

    let mut renderer = FrameRenderer::new();
    features.apply(&mut renderer).map_err(|err| failed(&err))?;
    let mut live = match &scene.extra {
        Some(extra) => Some(extra.install(device, &mut renderer, app.world_mut())?),
        None => None,
    };
    renderer.set_gpu_timing(device, true);
    let timestamps = device.capabilities.timestamp_query;

    let mut wall = Vec::with_capacity(frames as usize);
    let mut encode = Vec::with_capacity(frames as usize);
    let mut submit = Vec::with_capacity(frames as usize);
    let mut gpu_frame = Vec::with_capacity(frames as usize);
    let mut passes: Vec<(&'static str, Vec<f64>)> = Vec::new();
    let mut in_flight: VecDeque<wgpu::SubmissionIndex> = VecDeque::new();

    for index in 0..warmup + frames {
        let span = tracing::info_span!("bench_frame", index).entered();
        let start = Instant::now();
        let t = if held || index < warmup {
            0.0
        } else {
            (index - warmup) as f32 / frames.max(1) as f32
        };
        let transform = orbit_transform(scene, t);
        if let Ok(mut entity) = app.world_mut().get_entity_mut(camera) {
            entity.insert((transform, GlobalTransform(transform.compute_affine())));
        }
        if let Some(live) = live.as_mut() {
            live.before_frame(app.world_mut(), index);
        }
        app.update();
        {
            let frame = app
                .world()
                .resource::<ExtractedFrame>()
                .frame_scene(0, width, height);
            renderer
                .render_into(device, &frame, FrameTarget::full(&view, format, width, height))
                .map_err(|err| failed(&err))?;
        }
        if let Some(submission) = renderer.take_submission() {
            in_flight.push_back(submission);
        }
        // Two frames stay queued; the third waits for the oldest, as a game's frame pacing does.
        if in_flight.len() >= 3
            && let Some(oldest) = in_flight.pop_front()
        {
            device
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(oldest),
                    timeout: None,
                })
                .map_err(|err| failed(&err))?;
        }
        drop(span);
        if index < warmup {
            continue;
        }
        wall.push(start.elapsed().as_secs_f64() * 1000.0);
        let cpu = renderer.cpu_timings();
        encode.push(f64::from(cpu.encode_ms));
        submit.push(f64::from(cpu.submit_ms));
        if timestamps {
            let gpu = renderer.gpu_timings();
            gpu_frame.push(f64::from(gpu.frame_ms));
            let named = [
                ("shadows", gpu.shadows_ms),
                ("clusters", gpu.clusters_ms),
                ("gi", gpu.gi_ms),
                ("prepass", gpu.prepass_ms),
                ("cull", gpu.cull_ms),
                ("ssao", gpu.ssao_ms),
                ("forward", gpu.forward_ms),
                ("ssgi", gpu.ssgi_ms),
                ("ssr", gpu.ssr_ms),
                ("post", gpu.post_ms),
            ];
            if passes.is_empty() {
                passes = named.iter().map(|(name, _)| (*name, Vec::new())).collect();
            }
            for (slot, (_, value)) in passes.iter_mut().zip(named) {
                slot.1.push(f64::from(value));
            }
        }
    }
    while let Some(oldest) = in_flight.pop_front() {
        device
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(oldest),
                timeout: None,
            })
            .map_err(|err| failed(&err))?;
    }
    let memory = regia_render::memory_report(device);
    let gpu_memory = memory
        .available
        .then_some((memory.allocated_bytes, memory.reserved_bytes));
    let pixels = regia_render::read_rgba8(&device.device, &device.queue, &target, width, height)
        .map_err(|err| failed(&err))?;
    let (mean_luma, lit_pct) = frame_brightness(&pixels);
    if let Some(path) = png {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| failed(&err))?;
        }
        image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)
            .map_err(|err| failed(&err))?;
    }
    let draws = renderer.draw_stats();
    let counters = live.as_ref().map(|live| live.counters(&draws));
    Ok(SceneStats {
        frames,
        wall: Percentiles::of(&wall),
        cpu_encode: Percentiles::of(&encode),
        cpu_submit: Percentiles::of(&submit),
        gpu_frame: timestamps.then(|| Percentiles::of(&gpu_frame)),
        gpu_passes: passes
            .iter()
            .map(|(name, samples)| (*name, Percentiles::of(samples).p50))
            .collect(),
        drawn: draws.drawn,
        culled: draws.culled,
        instanced: draws.instanced,
        per_object: draws.per_object,
        mean_luma,
        lit_pct,
        gpu_memory,
        counters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_of_known_frames() {
        assert_eq!(frame_brightness(&[]), (0.0, 0.0));
        assert_eq!(frame_brightness(&[0, 0, 0, 255, 0, 0, 0, 255]), (0.0, 0.0));
        let (luma, lit) = frame_brightness(&[255, 255, 255, 255, 0, 0, 0, 255]);
        assert!((luma - 127.5).abs() < 1e-9, "{luma}");
        assert_eq!(lit, 50.0);
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = Percentiles::of(&samples);
        assert_eq!((p.p50, p.p95, p.p99, p.max), (50.0, 95.0, 99.0, 100.0));
        assert!((p.mean - 50.5).abs() < 1e-9);
    }

    #[test]
    fn percentiles_of_short_series() {
        assert_eq!(Percentiles::of(&[]), Percentiles::default());
        let one = Percentiles::of(&[3.0]);
        assert_eq!((one.p50, one.p99, one.max, one.mean), (3.0, 3.0, 3.0, 3.0));
        let unsorted = Percentiles::of(&[5.0, 1.0, 3.0]);
        assert_eq!((unsorted.p50, unsorted.max), (3.0, 5.0));
    }

    fn scene(orbit: f32) -> BenchScene {
        BenchScene {
            meshes: Vec::new(),
            points: Vec::new(),
            materials: 0,
            triangles: 0,
            center: Vec3::new(1.0, 2.0, 3.0),
            radius: 10.0,
            orbit,
            exposure_ev: 14.5,
            probed: super::super::scene::ProbeCounts::default(),
            extra: None,
        }
    }

    #[test]
    fn the_orbit_looks_at_the_center_and_is_deterministic() {
        let scene = scene(2.0);
        for t in [0.0, 0.25, 0.5, 0.9] {
            let transform = orbit_transform(&scene, t);
            let to_center = (scene.center - transform.translation).normalize_or_zero();
            let forward = transform.rotation * -Vec3::Z;
            assert!(
                (forward - to_center).length() < 1e-3,
                "t {t}: {forward} vs {to_center}"
            );
            assert_eq!(transform.translation, orbit_transform(&scene, t).translation);
        }
        assert_ne!(
            orbit_transform(&scene, 0.0).translation,
            orbit_transform(&scene, 0.25).translation
        );
    }

    #[test]
    fn an_interior_orbit_stays_at_the_center_height_and_inside() {
        let scene = scene(0.15);
        for t in [0.0, 0.3, 0.7] {
            let eye = orbit_transform(&scene, t).translation;
            assert_eq!(eye.y, scene.center.y);
            assert!((eye - scene.center).length() < scene.radius);
        }
    }

    #[test]
    fn stats_json_has_the_documented_fields() {
        let stats = SceneStats {
            frames: 2,
            wall: Percentiles::of(&[10.0, 20.0]),
            cpu_encode: Percentiles::default(),
            cpu_submit: Percentiles::default(),
            gpu_frame: None,
            gpu_passes: vec![("forward", 1.23456)],
            drawn: 3,
            culled: 4,
            instanced: 2,
            per_object: 1,
            mean_luma: 12.345,
            lit_pct: 50.0,
            gpu_memory: Some((2048, 4096)),
            counters: None,
        };
        let value = stats.to_json();
        assert_eq!(value["gpu_memory"]["allocated_bytes"], 2048);
        assert_eq!(value["gpu_memory"]["reserved_bytes"], 4096);
        assert_eq!(value["frames"], 2);
        assert_eq!(value["wall_ms"]["p50"], 10.0);
        assert_eq!(value["fps_p50"], 100.0);
        assert!(value["gpu_frame_ms"].is_null());
        assert_eq!(value["gpu_passes_p50_ms"]["forward"], 1.235);
        assert_eq!(value["draws"]["culled"], 4);
        assert_eq!(value["last_frame"]["mean_luma"], 12.35);
    }

    #[test]
    fn feature_names_parse_and_unknown_is_refused() {
        let names: Vec<String> = FEATURE_NAMES.iter().map(|n| (*n).to_owned()).collect();
        let all = BenchFeatures::parse(&names).expect("all");
        assert_eq!(all.names(), FEATURE_NAMES);
        assert_eq!(BenchFeatures::parse(&[]).expect("none"), BenchFeatures::default());
        assert_eq!(
            BenchFeatures::parse(&["oit".to_owned(), "warp-drive".to_owned()]),
            Err("warp-drive".to_owned())
        );
    }

    #[test]
    fn no_features_keep_the_presets_settings() {
        // The default run renders exactly what it did before `--on` existed.
        let plain = BenchFeatures::default().settings(RenderProfilePreset::DesktopHigh);
        let preset = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        assert_eq!(plain.preset(), preset.preset());
        assert_eq!(plain.preset(), Some(RenderProfilePreset::DesktopHigh));
        assert!(!plain.profile.order_independent_transparency);
        assert_eq!(plain.post.lens_effects, regia_render::LensEffects::OFF);
        let on = BenchFeatures {
            oit: true,
            sss: true,
            blue_noise: true,
            lens_effects: true,
            ..BenchFeatures::default()
        }
        .settings(RenderProfilePreset::DesktopUltra);
        assert!(on.profile.order_independent_transparency);
        assert!(on.profile.subsurface_scattering);
        assert!(on.profile.blue_noise);
        assert_eq!(on.post.lens_effects, regia_render::LensEffects::CINEMATIC);
    }
}
