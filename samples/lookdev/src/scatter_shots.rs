// SPDX-License-Identifier: MIT-0
//! Scatter evidence: Bore Twelve- and Switchbacks-like terrain shots with the rock scatter on
//! and off, rendered on the runtime frame path (`FrameRenderer` + the shared terrain draw +
//! scatter instances through the GPU cull), the way the DO27 shell draws a map.
//!
//! The scatter comes from the checked-in `scatter/{mars,europa}.scatter.ron` (format v2). Their
//! mesh paths name DO27's rock set; until those glTFs ship, [`rock_library`] binds seeded
//! procedural rocks to the same paths, so swapping in the real assets is a library change only.

use regia_core::glam::Vec3;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameMesh, FrameRenderer, GpuTimings,
    LabScene, OutdoorParams, RenderDevice, RenderSettings, SharedPassDraw,
};
use std::sync::Arc;

use regia_terrain::{
    CdlodConfig, ContactMap, DensityMaps, RockPalette, ScatterExclusion, ScatterField, ScatterFrameStats,
    ScatterLibrary, ScatterSet, ScatterSource, ScatterView, TerrainSharedDraw, standin_library,
};

use crate::scene::{Scene, fog_for};
use crate::{LookdevConfig, LookdevError, Planet};

/// Mars stand-in scatter (`samples/lookdev/scatter/mars.scatter.ron`).
pub const MARS_SCATTER: &str = include_str!("../scatter/mars.scatter.ron");
/// Europa stand-in scatter (`samples/lookdev/scatter/europa.scatter.ron`).
pub const EUROPA_SCATTER: &str = include_str!("../scatter/europa.scatter.ron");

/// The evidence cameras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScatterShot {
    /// Europa, elevated over the ice plain (UE `bore_twelve_wide`).
    BoreTwelveWide,
    /// Europa, eye height on a crest (UE `bore_twelve_close`).
    BoreTwelveClose,
    /// Mars, elevated over mesas and slopes (UE `switchbacks_wide`).
    SwitchbacksWide,
    /// Mars, eye height on the valley floor (UE `switchbacks_close`).
    SwitchbacksClose,
    /// Europa, looking up the steepest nearby face (strata, talus on the slope).
    BoreTwelveSlope,
    /// Mars, looking up the steepest nearby mesa face (strata, talus on the slope).
    SwitchbacksSlope,
}

impl ScatterShot {
    /// Every shot, Europa first.
    pub const ALL: [Self; 6] = [
        Self::BoreTwelveWide,
        Self::BoreTwelveClose,
        Self::BoreTwelveSlope,
        Self::SwitchbacksWide,
        Self::SwitchbacksClose,
        Self::SwitchbacksSlope,
    ];

    /// File stem.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::BoreTwelveWide => "bore_twelve_wide",
            Self::BoreTwelveClose => "bore_twelve_close",
            Self::SwitchbacksWide => "switchbacks_wide",
            Self::SwitchbacksClose => "switchbacks_close",
            Self::BoreTwelveSlope => "bore_twelve_slope",
            Self::SwitchbacksSlope => "switchbacks_slope",
        }
    }

    /// The planet the shot is on.
    #[must_use]
    pub fn planet(self) -> Planet {
        match self {
            Self::BoreTwelveWide | Self::BoreTwelveClose | Self::BoreTwelveSlope => Planet::Europa,
            Self::SwitchbacksWide | Self::SwitchbacksClose | Self::SwitchbacksSlope => Planet::Mars,
        }
    }
}

/// The planet's `.scatter.ron`.
///
/// # Errors
///
/// [`LookdevError::Terrain`] when the checked-in file does not parse.
pub fn scatter_set(planet: Planet) -> Result<ScatterSet, LookdevError> {
    let text = match planet {
        Planet::Mars => MARS_SCATTER,
        Planet::Europa => EUROPA_SCATTER,
    };
    Ok(ScatterSet::from_ron(text, None)?)
}

/// Procedural stand-ins bound to the mesh paths of the planet's `.scatter.ron`
/// ([`regia_terrain::standin_library`]: dark red-brown sandstone and basalt on Mars, ice
/// and basalt on Europa; 1 m across at scale 1).
#[must_use]
pub fn rock_library(planet: Planet) -> ScatterLibrary {
    let palette = match planet {
        Planet::Mars => RockPalette::Mars,
        Planet::Europa => RockPalette::Europa,
    };
    scatter_set(planet)
        .map(|set| standin_library(&set, palette))
        .unwrap_or_default()
}

/// Frame options for [`ScatterStage::render`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScatterOptions {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Frames rendered before the capture (TAAU and auto exposure converge).
    pub frames: u32,
    /// Draw the scatter.
    pub scatter: bool,
    /// Draw the contact grounding.
    pub contact: bool,
    /// Record GPU pass timings (timestamp queries).
    pub gpu_timing: bool,
}

impl ScatterOptions {
    /// The evidence frame: 960×540, 12 frames, scatter and contact on.
    #[must_use]
    pub fn evidence() -> Self {
        Self {
            width: 960,
            height: 540,
            frames: 12,
            scatter: true,
            contact: true,
            gpu_timing: false,
        }
    }
}

/// One rendered shot.
pub struct ScatterCapture {
    /// RGBA8, sRGB-encoded, top row first.
    pub pixels: Vec<u8>,
    /// What the field emitted on the captured frame.
    pub stats: ScatterFrameStats,
    /// GPU pass timings of the captured frame (zero without timestamps).
    pub gpu: GpuTimings,
    /// Meshes inside the frame's frustum (scatter instances plus nothing else here).
    pub drawn: usize,
    /// CPU time of the last frame's scatter work (stream update + collect, incl. seating), ms.
    pub scatter_cpu_ms: f32,
}

/// One planet's terrain, scatter field, contact map and frame renderer.
pub struct ScatterStage {
    /// The generated stand-in map.
    pub scene: Scene,
    /// The placed scatter.
    pub field: ScatterField,
    contact: ContactMap,
    renderer: FrameRenderer,
}

impl ScatterStage {
    /// Generates the planet, places its scatter (roads and the tunnel mouth excluded) and
    /// installs terrain + contact map on a fresh frame renderer.
    ///
    /// # Errors
    ///
    /// Generation, placement, mesh resolution or the terrain upload failed.
    pub fn new(device: &RenderDevice, planet: Planet, config: &LookdevConfig) -> Result<Self, LookdevError> {
        Self::with_density(device, planet, config, 1.0)
    }

    /// [`ScatterStage::new`] with every class's density multiplied by `density_scale`
    /// (benchmarks that need a given instance count).
    ///
    /// # Errors
    ///
    /// See [`ScatterStage::new`].
    pub fn with_density(
        device: &RenderDevice,
        planet: Planet,
        config: &LookdevConfig,
        density_scale: f32,
    ) -> Result<Self, LookdevError> {
        let scene = crate::scene::generate(planet, config)?;
        let mut set = scatter_set(planet)?;
        for class in &mut set.classes {
            class.density = (class.density * density_scale).min(64.0);
        }
        let road = ScatterExclusion {
            points: scene.road.samples.clone(),
            radius: 6.0,
        };
        for class in &mut set.classes {
            class.exclusions.push(road.clone());
        }
        let source = ScatterSource {
            map: Arc::new(scene.map.clone()),
            holes: Some(scene.holes.clone()),
            layers: Some(scene.layers.clone()),
            density_maps: DensityMaps::new(),
        };
        let mut field = ScatterField::new(&set, source, &rock_library(planet), 0x5ca7_0000 + planet as u64)
            .map_err(regia_terrain::TerrainError::from)?;
        let contact = ContactMap::new(&device.device);
        let mut draw = TerrainSharedDraw::new(
            device,
            scene.map.clone(),
            &scene.layers,
            CdlodConfig {
                range_factor: 3.5,
                ..CdlodConfig::default()
            },
            scene.root_level,
        )?;
        draw.set_contact_map(Some(&contact));
        // Far rocks sit on the drawn (CDLOD) surface, not the true heightfield (ADR-048).
        field.set_rendered_surface(Some(draw.surface()));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let preset = match planet {
            Planet::Mars => AtmospherePreset::Mars,
            Planet::Europa => AtmospherePreset::Airless,
        };
        // A DO27 level overrides the planet's sun strength (`LevelAtmosphere::sun_illuminance_lux`)
        // so the sky and the ground bounce integrate the same sun the scene is lit by.
        let mut atmosphere = AtmosphereParameters::from_preset(preset);
        atmosphere.sun_illuminance = sun_for(planet, &scene).2;
        renderer.set_outdoor(OutdoorParams {
            planet: atmosphere,
            ..OutdoorParams::default()
        });
        Ok(Self {
            scene,
            field,
            contact,
            renderer,
        })
    }

    /// Eye, target and vertical field of view of `shot`.
    ///
    /// The heading puts the sun behind and to one side (raking light, shadows visible), then
    /// turns in 30° steps until the ground ahead does not rise into the lens.
    #[must_use]
    pub fn camera(&self, shot: ScatterShot) -> ([f32; 3], [f32; 3], f32) {
        let scene = &self.scene;
        let h = |x: f32, z: f32| scene.height_at(x, z);
        let along = scene.road.bend_tangent;
        let side = [-along[1], along[0]];
        let c = scene.landmarks.close;
        let (sun_az, _, _) = sun_for(scene.planet, scene);
        if matches!(shot, ShotKind::BoreTwelveSlope | ShotKind::SwitchbacksSlope) {
            return slope_camera(scene, sun_az);
        }
        // Keep the road cut out of the foreground: the lens and the ground it looks at stay
        // 18 m off the road centre line.
        let road = ScatterExclusion {
            points: scene.road.samples.clone(),
            radius: 18.0,
        };
        let (eye_xz, above, ahead, fov) = match shot {
            ShotKind::BoreTwelveWide
            | ShotKind::SwitchbacksWide
            | ShotKind::BoreTwelveSlope
            | ShotKind::SwitchbacksSlope => (
                [c[0] + side[0] * 45.0, c[1] + side[1] * 45.0],
                35.0,
                130.0,
                55.0_f32.to_radians(),
            ),
            ShotKind::BoreTwelveClose | ShotKind::SwitchbacksClose => (
                [c[0] + side[0] * 30.0, c[1] + side[1] * 30.0],
                1.7,
                28.0,
                60.0_f32.to_radians(),
            ),
        };
        let ground = h(eye_xz[0], eye_xz[1]);
        let base = sun_az + std::f32::consts::PI - 0.95;
        let mut heading = base;
        for k in 0..12 {
            let step = (k / 2 + 1) as f32 * if k % 2 == 0 { 0.52 } else { -0.52 };
            let candidate = if k == 0 { base } else { base + step };
            let dir = [candidate.cos(), candidate.sin()];
            let clear = (1..=8).all(|i| {
                let d = ahead * i as f32 / 8.0;
                let (x, z) = (eye_xz[0] + dir[0] * d, eye_xz[1] + dir[1] * d);
                h(x, z) <= ground + above * 0.6 + d * 0.12 && (above > 5.0 || !road.contains(x, z))
            });
            if clear {
                heading = candidate;
                break;
            }
        }
        let dir = [heading.cos(), heading.sin()];
        let tgt = [eye_xz[0] + dir[0] * ahead, eye_xz[1] + dir[1] * ahead];
        (
            [eye_xz[0], ground + above, eye_xz[1]],
            [
                tgt[0],
                h(tgt[0], tgt[1]) + if above < 5.0 { 0.2 } else { 0.0 },
                tgt[1],
            ],
            fov,
        )
    }

    /// Renders `shot` with the Desktop High runtime defaults.
    ///
    /// # Errors
    ///
    /// The frame submission or readback failed.
    pub fn render(
        &mut self,
        device: &RenderDevice,
        shot: ScatterShot,
        options: ScatterOptions,
    ) -> Result<ScatterCapture, LookdevError> {
        let (eye, target, fov_y) = self.camera(shot);
        self.render_view(device, eye, target, fov_y, options)
    }

    /// Renders an arbitrary view (benchmarks, extra angles).
    ///
    /// # Errors
    ///
    /// The frame submission or readback failed.
    pub fn render_view(
        &mut self,
        device: &RenderDevice,
        eye: [f32; 3],
        target: [f32; 3],
        fov_y: f32,
        options: ScatterOptions,
    ) -> Result<ScatterCapture, LookdevError> {
        let _span = tracing::info_span!("scatter_shot").entered();
        self.field.set_enabled(options.scatter);
        let update_started = std::time::Instant::now();
        self.field.update(eye);
        let update_ms = update_started.elapsed().as_secs_f32() * 1000.0;
        self.contact.set_enabled(options.contact);
        self.contact.update(&device.queue, &self.field, eye);
        self.renderer.set_gpu_timing(device, options.gpu_timing);

        let climate = &self.scene.climate;
        let planet = self.scene.planet;
        let (az, el, sun_lux) = sun_for(planet, &self.scene);
        let to_sun = Vec3::new(el.cos() * az.cos(), el.sin(), el.cos() * az.sin());
        let sun_colour = match planet {
            Planet::Europa => [1.0, 0.98, 0.95],
            Planet::Mars => climate.sun_color,
        };
        // The terrain's flat ambient (the shared draw's sky term): a dim bounce on airless
        // Europa, a dusty sky on Mars.
        let ambient_lux = match planet {
            Planet::Europa => 2_500.0,
            Planet::Mars => 9_000.0,
        };
        let settings = RenderSettings::default();
        let mut post = settings.resolve_post();
        post.taa = false;
        let mut quality = settings.quality();
        quality.physical_atmosphere = true;
        let lab = LabScene {
            width: options.width,
            height: options.height,
            eye,
            target,
            fov_y,
            near: 0.1,
            far: 12_000.0,
            depth_prepass: settings.profile.depth_prepass,
            directional_shadows: true,
            ibl: true,
            exposure_ev: post.exposure_ev,
            ambient_lux,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new((-to_sun).to_array(), sun_colour, sun_lux),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let view = ScatterView::look_at(
            eye,
            target,
            fov_y,
            options.width as f32 / options.height.max(1) as f32,
            0.1,
            12_000.0,
        )
        .with_sun(to_sun.to_array());
        let mut pixels = Vec::new();
        let mut stats = ScatterFrameStats::default();
        let mut scatter_cpu_ms = 0.0;
        for _ in 0..options.frames.max(1) {
            let mut frame = lab.as_frame_scene_with_quality(quality);
            frame.post = Some(&post);
            frame.fog = match planet {
                Planet::Mars => fog_for(climate, 1.0),
                Planet::Europa => regia_render::FogSettings::default(),
            };
            let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
            let collect_started = std::time::Instant::now();
            stats = self.field.collect(&view, &mut meshes);
            scatter_cpu_ms = update_ms + collect_started.elapsed().as_secs_f32() * 1000.0;
            frame.meshes.extend(meshes);
            pixels = self.renderer.render_offscreen(device, &frame)?.pixels;
        }
        Ok(ScatterCapture {
            pixels,
            stats,
            gpu: self.renderer.gpu_timings(),
            drawn: self.renderer.draw_stats().drawn,
            scatter_cpu_ms,
        })
    }
}

use ScatterShot as ShotKind;

/// A close line-up of the planet's stand-in rocks on a flat ground plane (every mesh of the
/// library, LOD 0, two yaws), lit by the shot sun: the rock-shape review frame.
///
/// # Errors
///
/// The frame submission failed.
pub fn render_rock_lineup(
    device: &RenderDevice,
    planet: Planet,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, LookdevError> {
    let library = rock_library(planet);
    let mut paths: Vec<&String> = library.keys().collect();
    paths.sort();
    let mut meshes = Vec::new();
    let mut materials = Vec::new();
    let ground_colour = match planet {
        Planet::Mars => [0.46, 0.27, 0.15, 1.0],
        Planet::Europa => [0.75, 0.77, 0.80, 1.0],
    };
    materials.push(regia_render::LabMaterial {
        base_color: ground_colour,
        roughness: 0.95,
        ..regia_render::LabMaterial::default()
    });
    let (plane_v, plane_i) = regia_render::box_mesh([30.0, 0.05, 30.0]);
    meshes.push(regia_render::LabMesh {
        vertices: regia_render::translate_mesh(&plane_v, [0.0, -0.05, 0.0]),
        indices: plane_i,
        material: 0,
        ..regia_render::LabMesh::unskinned()
    });
    for (i, path) in paths.iter().enumerate() {
        let Some(asset) = library.get(*path) else { continue };
        materials.push(asset.material.clone());
        for copy in 0..2 {
            let yaw = copy as f32 * 2.1 + i as f32 * 0.7;
            let (s, c) = yaw.sin_cos();
            let x = -3.0 + i as f32 * 2.2;
            let z = -2.0 - copy as f32 * 2.4;
            let vertices = asset.lods[0]
                .vertices
                .iter()
                .map(|v| {
                    let mut v = *v;
                    let p = v.position;
                    let n = v.normal;
                    v.position = [p[0] * c + p[2] * s + x, p[1] - 0.12, -p[0] * s + p[2] * c + z];
                    v.normal = [n[0] * c + n[2] * s, n[1], -n[0] * s + n[2] * c];
                    v
                })
                .collect();
            meshes.push(regia_render::LabMesh {
                vertices,
                indices: asset.lods[0].indices.clone(),
                material: materials.len() - 1,
                ..regia_render::LabMesh::unskinned()
            });
        }
    }
    let settings = RenderSettings::default();
    let mut post = settings.resolve_post();
    post.taa = false;
    let quality = settings.quality();
    let sun = Vec3::new(0.62, 0.42, 0.66).normalize();
    let mut renderer = FrameRenderer::new();
    let mut atmosphere = AtmosphereParameters::from_preset(match planet {
        Planet::Mars => AtmospherePreset::Mars,
        Planet::Europa => AtmospherePreset::Airless,
    });
    atmosphere.sun_illuminance = 100_000.0;
    renderer.set_outdoor(OutdoorParams {
        planet: atmosphere,
        ..OutdoorParams::default()
    });
    let lab = LabScene {
        width,
        height,
        eye: [0.6, 1.6, 3.2],
        target: [0.6, 0.2, -2.5],
        fov_y: 50.0_f32.to_radians(),
        near: 0.05,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: post.exposure_ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new((-sun).to_array(), [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    };
    let mut pixels = Vec::new();
    for _ in 0..8 {
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        pixels = renderer.render_offscreen(device, &frame)?.pixels;
    }
    Ok(pixels)
}

/// A camera 70 m out from the steepest face within 450 m of the close landmark (off the road),
/// 12 m above the ground, looking at the face: strata on the wall, talus on its foot.
fn slope_camera(scene: &Scene, sun_az: f32) -> ([f32; 3], [f32; 3], f32) {
    let h = |x: f32, z: f32| scene.height_at(x, z);
    let c = scene.landmarks.close;
    let road = ScatterExclusion {
        points: scene.road.samples.clone(),
        radius: 25.0,
    };
    let mut best: Option<(f32, [f32; 2], [f32; 2])> = None;
    let step = 8.0;
    let n = (450.0 / step) as i32;
    for iz in -n..=n {
        for ix in -n..=n {
            let p = [c[0] + ix as f32 * step, c[1] + iz as f32 * step];
            if road.contains(p[0], p[1]) {
                continue;
            }
            // Height drop over 30 m along +/-X and +/-Z: the steepest local face.
            let gx = (h(p[0] + 15.0, p[1]) - h(p[0] - 15.0, p[1])) / 30.0;
            let gz = (h(p[0], p[1] + 15.0) - h(p[0], p[1] - 15.0)) / 30.0;
            let grade = (gx * gx + gz * gz).sqrt();
            if grade > 2.5 {
                continue;
            }
            // Prefer faces lit from the side: the sun across the downhill direction.
            let down = [-gx / grade.max(1e-4), -gz / grade.max(1e-4)];
            let side_light = 1.0 - (down[0] * sun_az.cos() + down[1] * sun_az.sin()).abs() * 0.5;
            let score = grade * side_light;
            if best.is_none_or(|b| score > b.0) {
                best = Some((score, p, down));
            }
        }
    }
    let Some((_, face, down)) = best else {
        return (
            [c[0], h(c[0], c[1]) + 30.0, c[1]],
            [c[0] + 50.0, h(c[0] + 50.0, c[1]), c[1]],
            0.96,
        );
    };
    let eye = [face[0] + down[0] * 70.0, face[1] + down[1] * 70.0];
    (
        [eye[0], h(eye[0], eye[1]) + 12.0, eye[1]],
        [face[0], h(face[0], face[1]), face[1]],
        55.0_f32.to_radians(),
    )
}

/// Sun azimuth (radians, XZ), elevation (radians) and illuminance (lux) of the planet's shots:
/// a low raking sun like the UE plates, long shadows behind every block.
fn sun_for(planet: Planet, scene: &Scene) -> (f32, f32, f32) {
    match planet {
        Planet::Europa => (scene.climate.sun_azimuth, 0.42, 100_000.0),
        Planet::Mars => (scene.climate.sun_azimuth, 0.55, scene.climate.sun_lux),
    }
}
