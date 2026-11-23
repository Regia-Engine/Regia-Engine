// SPDX-License-Identifier: MIT-0
//! Look director round 2: DO27-like parity shots on the runtime frame path.
//!
//! The UE v13 parity plates (`docs/evidence/parity/2026-10-07/`) are the bar. These
//! shots rebuild the six frames the look-director lane owns — Mars at dusk, the
//! Switchbacks wide / mid / close, a tunnel interior on Mars, and the Europa cockpit
//! view of Jupiter — on the procedural stand-in terrain ([`crate::scene`]) drawn
//! *through* [`regia_render::FrameRenderer`] with [`regia_terrain::TerrainSharedDraw`],
//! the way a game draws them (ADR-041): the Desktop High profile, the map's
//! [`LevelSettings`] (atmosphere preset, sun clock, sky body, look), auto exposure,
//! froxel fog and TAA.
//!
//! The stand-in is not DO27's content: it has its own albedos, no rock scatter and an
//! 4–8 m heightfield. What these frames judge is the engine's part of each plate:
//! exposure, grade, aerial perspective and fog at a low sun, ambient in shadow, and
//! the sky body.

use std::collections::HashMap;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderSettings, SdfScene, SdfShape,
    SharedPassDraw, TimeOfDay, VolumetricFogSettings, box_mesh, sun_position, uv_sphere,
};
use regia_scene::level::{
    AtmospherePreset, LevelGrade, LevelHaze, LevelLook, LevelMeter, LevelSettings, LevelTimeOfDay, SkyBody,
};
use regia_terrain::{
    CdlodConfig, ContactMap, DensityMaps, LayerDesc, LayerStyle, LayerTextures, MacroBake, MaterialParams,
    ScatterField, ScatterSource, ScatterView, TerrainLayers, TerrainMap, TerrainMaterial, TerrainRelief,
    TerrainSharedDraw,
};

use crate::scene::generate;
use crate::{LookdevConfig, LookdevError, Planet};

/// One parity shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum R2Shot {
    /// Mars, sun a few degrees up: elevated view across stratified mesas.
    DuskVista,
    /// Mars afternoon: high wide view down into the canyon.
    SwitchbacksWide,
    /// Mars afternoon: canyon floor with a vehicle, mesas behind.
    SwitchbacksMid,
    /// Mars afternoon: one metre above the dust, a wall a few tens of metres away.
    SwitchbacksClose,
    /// Inside a roofed cut on Mars looking out: trusses, a walkway, sun through the open end.
    TunnelInterior,
    /// Europa: eye height on the ice, Jupiter high in a black sky.
    Cockpit,
    /// Europa: elevated over trenches and a base building.
    BoreTwelveWide,
    /// Europa: a few tens of metres up over chaos ice.
    BoreTwelveMid,
    /// Europa: on a crest looking down a cliff face.
    BoreTwelveClose,
    /// Europa: a hangar set piece beside a trench.
    SetPieceBorehole,
    /// Europa: eye height, a vehicle 20 m out (the UE fireball plate's framing; the effect
    /// itself renders in the VFX harness).
    Fireball20m,
    /// Europa: eye height, a vehicle 80 m out (the UE smoke plate's framing).
    Smoke80m,
}

impl R2Shot {
    /// Every shot, in evidence order.
    pub const ALL: [Self; 6] = [
        Self::DuskVista,
        Self::SwitchbacksWide,
        Self::SwitchbacksMid,
        Self::SwitchbacksClose,
        Self::TunnelInterior,
        Self::Cockpit,
    ];

    /// The twelve UE parity plates, in the parity sheet's order.
    pub const PARITY: [Self; 12] = [
        Self::BoreTwelveWide,
        Self::BoreTwelveMid,
        Self::BoreTwelveClose,
        Self::SwitchbacksWide,
        Self::SwitchbacksMid,
        Self::SwitchbacksClose,
        Self::TunnelInterior,
        Self::Cockpit,
        Self::DuskVista,
        Self::SetPieceBorehole,
        Self::Fireball20m,
        Self::Smoke80m,
    ];

    /// File stem, the same as the parity plate's.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::DuskVista => "dusk_vista",
            Self::SwitchbacksWide => "switchbacks_wide",
            Self::SwitchbacksMid => "switchbacks_mid",
            Self::SwitchbacksClose => "switchbacks_close",
            Self::TunnelInterior => "tunnel_interior",
            Self::Cockpit => "cockpit",
            Self::BoreTwelveWide => "bore_twelve_wide",
            Self::BoreTwelveMid => "bore_twelve_mid",
            Self::BoreTwelveClose => "bore_twelve_close",
            Self::SetPieceBorehole => "set_piece_borehole",
            Self::Fireball20m => "fireball_20m",
            Self::Smoke80m => "smoke_80m",
        }
    }

    /// Stand-in planet.
    #[must_use]
    pub fn planet(self) -> Planet {
        match self {
            Self::Cockpit
            | Self::BoreTwelveWide
            | Self::BoreTwelveMid
            | Self::BoreTwelveClose
            | Self::SetPieceBorehole
            | Self::Fireball20m
            | Self::Smoke80m => Planet::Europa,
            _ => Planet::Mars,
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::PARITY.into_iter().find(|shot| shot.name() == name)
    }
}

/// Frame size, convergence and terrain detail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct R2Options {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Frames rendered before the capture (auto exposure and TAA converge).
    pub frames: u32,
    /// Heightfield edge in samples over the 4096 m stand-in.
    pub terrain_resolution: u32,
    /// Cost run (`look_r2 --perf`): per-pass GPU timings, the eye drifting 5 cm / 3 cm per
    /// frame so cached passes (cascades, static-screen skips) do not hide the cost.
    pub timing: bool,
}

impl R2Options {
    /// Evidence: 960×540, converged, 1025² heightfield (4 m cells).
    #[must_use]
    pub fn evidence() -> Self {
        Self {
            width: 960,
            height: 540,
            frames: 14,
            terrain_resolution: 1025,
            timing: false,
        }
    }

    /// Lavapipe tests: small frames on a coarse field.
    #[must_use]
    pub fn test() -> Self {
        Self {
            width: 320,
            height: 180,
            frames: 10,
            terrain_resolution: 257,
            timing: false,
        }
    }
}

/// Camera (and level) overrides for framing sessions and tests
/// (`look_r2 --eye x,z --above h --yaw d --pitch d`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CameraOverride {
    /// Replaces the map haze density (1/m); tests compare the preset against clear air.
    pub haze_density: Option<f32>,
    /// Eye XZ, metres.
    pub eye: Option<[f32; 2]>,
    /// Eye height above the ground, metres.
    pub above: Option<f32>,
    /// Yaw relative to the sun's azimuth, degrees (0 looks at the sun).
    pub yaw_from_sun: Option<f32>,
    /// Pitch, degrees (negative looks down).
    pub pitch: Option<f32>,
    /// An exact camera: eye xyz, target xyz, vertical field of view in degrees (as
    /// `LOOK_R2_LOOK_AT`, which it overrides).
    pub look_at: Option<[f32; 7]>,
}

/// A camera relative to the sun: where it stands and where it looks.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CameraSpec {
    eye: [f32; 2],
    above: f32,
    yaw_from_sun: f32,
    pitch: f32,
    fov_y_deg: f32,
}

/// One planet's terrain, ready for [`TerrainSharedDraw`].
pub struct R2Terrain {
    /// Heightfield.
    pub map: TerrainMap,
    /// Splat layers.
    pub layers: TerrainLayers,
    /// CDLOD grid.
    pub cdlod: CdlodConfig,
    /// Quadtree depth.
    pub root_level: u32,
    /// Photoreal splat material (ADR-044): strata, relief lighting, detail normals.
    pub material: TerrainMaterial,
    /// Rock scatter (the checked-in `.scatter.ron` set and procedural rocks, ADR scatter-rocks).
    pub scatter: Option<ScatterField>,
    /// The scatter's baked impostor cards ([`impostor_layer`]), kept across shots.
    pub impostor_atlas: Option<regia_terrain::ImpostorAtlas>,
    /// DO27's own content for this map (`REGIA_DO27_CONTENT`), replacing the stand-in's
    /// material, clock, cameras and props.
    pub do27: Option<Box<Do27Content>>,
}

/// A DO27 map loaded from the content drop (see [`crate::do27`]).
pub struct Do27Content {
    /// Level settings, sun, Jupiter, cooked material and props.
    pub map: crate::do27::Do27Map,
    /// The twelve plate cameras by shot name.
    pub cameras: HashMap<String, crate::do27::Do27Camera>,
}

impl R2Terrain {
    /// Ground height at a world XZ position, metres.
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        self.map.sample_bilinear(x, z)
    }
}

/// The stand-in terrains, generated once per planet: Mars is a field of stratified
/// mesas and spires over dune floors (the Switchbacks plates), Europa is the
/// [`crate::scene`] ice stand-in.
#[derive(Default)]
pub struct R2Planets {
    terrains: HashMap<Planet, R2Terrain>,
}

impl R2Planets {
    /// Builds (or returns) the terrain for `planet` at `resolution` samples per edge.
    ///
    /// # Errors
    ///
    /// The generator or the heightfield rejected the configuration.
    pub fn get(&mut self, planet: Planet, resolution: u32) -> Result<&mut R2Terrain, LookdevError> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.terrains.entry(planet) {
            if let Some(dir) = crate::do27::content_dir() {
                slot.insert(load_do27_terrain(&dir, planet)?);
                return self
                    .terrains
                    .get_mut(&planet)
                    .ok_or_else(|| LookdevError::Config("DO27 map missing".into()));
            }
            let terrain = match planet {
                Planet::Mars => mesa_terrain(resolution)?,
                Planet::Europa => {
                    let config = LookdevConfig {
                        height_resolution: resolution,
                        macro_resolution: 64,
                        layer_height_resolution: 32,
                        world_width_m: WORLD_M,
                        frames: 2,
                        width: 64,
                        height: 36,
                        png_width: 64,
                        thermal_iterations: 3,
                        hydraulic_drops: resolution * 6,
                        scatter_cap: 8,
                        planets: vec![planet],
                    };
                    let scene = generate(planet, &config)?;
                    R2Terrain {
                        layers: photoreal_layers(planet, &scene.map),
                        material: photoreal_material(planet, &scene.map),
                        map: scene.map,
                        cdlod: scene.cdlod,
                        root_level: scene.root_level,
                        scatter: None,
                        impostor_atlas: None,
                        do27: None,
                    }
                }
            };
            let mut terrain = terrain;
            terrain.scatter = Some(scatter_for(planet, &terrain)?);
            slot.insert(terrain);
        }
        self.terrains
            .get_mut(&planet)
            .ok_or_else(|| LookdevError::Config("stand-in missing".into()))
    }
}

const WORLD_M: f32 = 4096.0;

/// One DO27 map from the content drop: the visual heightfield, its layer stack, the cooked
/// material, `.scatter.ron` with procedural rocks, `level.ron`, the map's sun and the set
/// pieces, all in the sim frame (the image is mirrored at the end, see [`crate::do27`]).
fn load_do27_terrain(dir: &std::path::Path, planet: Planet) -> Result<R2Terrain, LookdevError> {
    use crate::do27;
    let map_dir = dir.join("maps").join(do27::map_name(planet));
    let map = do27::load_heightfield(&map_dir)?;
    let layers = do27::load_layers(&map_dir)?;
    let level = do27::load_level(&map_dir)?;
    let do27::MapLook {
        sun,
        jupiter,
        fill_lux,
    } = do27::load_look(&map_dir)?;
    let (cooked, params, macro_tiles) = do27::load_cooked(&map_dir)?;
    let (mut props, mut prop_materials) = do27::load_props(&map_dir, materials().len())?;
    // LOOK_R2_BASES=1: stand-in boxes where DO27 places its base buildings and parked
    // vehicles. Off by default: the meshes are not in the drop, and the derived placements
    // (around the team starts) are not where the UE plates show them.
    if std::env::var_os("LOOK_R2_BASES").is_some() {
        props.extend(do27::load_base_standins(&map_dir, M_HULL)?);
    }
    // LOOK_R2_EXTRA_GLTF=<dir>[:<dir>…]: every LOD0 glTF under these content directories as well
    // (e.g. `units`: the match roster a game loads on top of the map), at the map origin —
    // memory measurements of a whole match's textures (lane tex-budget).
    if let Some(dirs) = std::env::var_os("LOOK_R2_EXTRA_GLTF") {
        for extra in std::env::split_paths(&dirs) {
            let extra = if extra.is_absolute() {
                extra
            } else {
                dir.join(extra)
            };
            for path in do27::lod0_gltfs(&extra) {
                do27::load_gltf(&path, true, materials().len(), &mut props, &mut prop_materials)?;
            }
        }
    }
    // LOOK_R2_NO_PROPS=1: drop DO27's set-piece meshes (shadow-caster diagnostics).
    if std::env::var_os("LOOK_R2_NO_PROPS").is_some() {
        props.clear();
    }
    let cameras = do27::load_cameras(dir)?;
    if std::env::var_os("LOOK_R2_DEBUG").is_some() {
        let verts: usize = props.iter().map(|m| m.vertices.len()).sum();
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for m in &props {
            for v in &m.vertices {
                lo = lo.min(Vec3::from_array(v.position));
                hi = hi.max(Vec3::from_array(v.position));
            }
        }
        eprintln!(
            "do27 {planet:?}: {} prop meshes, {verts} vertices, bounds {lo} .. {hi}, {} materials",
            props.len(),
            prop_materials.len()
        );
    }
    let set = do27::load_scatter_set(&map_dir, &layers)?;
    let library = do27::rock_library(planet, &set);
    let holes = do27::tunnel_mouths(&map_dir)?;
    let source = ScatterSource {
        map: std::sync::Arc::new(map.clone()),
        holes: (!holes.is_empty()).then(|| holes.clone()),
        layers: Some(layers.clone()),
        density_maps: DensityMaps::new(),
    };
    let scatter =
        ScatterField::new(&set, source, &library, set.seed).map_err(regia_terrain::TerrainError::from)?;
    Ok(R2Terrain {
        map,
        layers,
        cdlod: CdlodConfig::default(),
        // 1280 m over 2^7 nodes of 32 cells: 0.31 m cells, one per visual texel.
        root_level: 7,
        material: TerrainMaterial {
            layers: Vec::new(),
            macro_set: None,
            params,
        },
        scatter: Some(scatter),
        impostor_atlas: None,
        do27: Some(Box::new(Do27Content {
            map: do27::Do27Map {
                level,
                sun,
                jupiter,
                fill_lux,
                cooked,
                params,
                macro_tiles,
                props,
                prop_materials,
                holes,
            },
            cameras,
        })),
    })
}

/// The shot's level on DO27 content: the map's own `level.ron` (planet, sun strength), the
/// authored light instead of a clock, Jupiter where the map look puts it, and this lane's
/// look and haze presets with the exposure band set for DO27's authored sun and textures.
/// The bands were matched to the UE plates' ground luma on this content (Europa: the
/// bore_twelve_mid and cockpit ground medians within 5 %; Mars: switchbacks_mid's mean);
/// the stand-ins' bands do not carry over, because DO27's cooked albedos and 45 / 18 klx
/// suns differ from the stand-ins'.
pub(crate) fn do27_level(shot: R2Shot, content: &Do27Content) -> LevelSettings {
    do27_level_with(shot, content, None)
}

/// [`do27_level`] with an explicit Mars grade. `None` reads `LOOK_R2_MARS_GRADE`.
pub(crate) fn do27_level_with(
    shot: R2Shot,
    content: &Do27Content,
    grade: Option<MarsGrade>,
) -> LevelSettings {
    let mut level = content.map.level.clone();
    level.atmosphere.time_of_day = None;
    let (mut look, band) = match shot.planet() {
        Planet::Mars => {
            level.atmosphere.haze = switchbacks_haze();
            // Round 6, after terrain round 5 (ground at its authored reflectance): the lit sand
            // matches the plates' (p80 ≈ 205–220) at EV100 ≈ 11.2. DO27's authored 12.8
            // (12.5 with its trim) prints every plate but dusk 19–64 % darker in Regia Engine.
            // On the real layers a milder grade: 6200 K + tint 0.2 keeps the shadows the
            // plates' warm orange-brown (hue 20–22°, saturation 0.74–0.77; UE 21–23°,
            // 0.73–0.82). The stand-in preset's 5800 K + 0.45 printed them pink-violet
            // (14–19°, the tunnel pad 0.49 saturation) for a 24° overall hue (UE 22–25°;
            // this grade gives 26–28°).
            let mut look = switchbacks_look();
            if let Some(grade) = look.grade.as_mut() {
                grade.white_balance_kelvin = 6200.0;
                // Round 13: tint 0.2 → 0.3 with saturation 1.2 → 1.5 (below): against the UE
                // close-up (the one Mars plate whose framing is current) the sand printed paler
                // and less orange, lit saturation 0.55 against UE's 0.65 and shade 0.63 against
                // 0.71 (median HSV, display), with the shade hue 27° against 25°. Now lit
                // 28.4° / 0.65 and shade 26° / 0.72; luma unchanged (p50 154, UE 155).
                grade.tint = 0.3;
            }
            // Rounds 10–11: the band opens downward so a frame that is mostly ridge shadow
            // adapts like an eye instead of printing a muddy floor: switchbacks_close meters
            // EV100 10.2 with the default terms and 8.9 with GI Depth's probes (darker,
            // closer to the path tracer).
            // Round 11 (after main's held-haze fix took the veil off the canyon): +0.7 EV of
            // key with the ceiling raised by the same 0.7, so the sunlit framings that meter
            // above the ceiling print as before while an in-band, shadow-filled frame (the
            // close-up) lands where the UE plate has it; saturation 1.3 → 1.2 and contrast
            // 1.8 → 1.5, because every Mars plate printed 0.07–0.18 more saturated than UE
            // (median HSV saturation) and the close-up's shadowed sand went red-brown.
            look.exposure_bias_ev = 0.7;
            look.saturation = Some(1.5);
            look.contrast = Some(1.5);
            match grade.unwrap_or_else(MarsGrade::from_env) {
                MarsGrade::Ue => {}
                MarsGrade::Nasa => switchbacks_nasa_grade(&mut look),
                MarsGrade::NasaDark => {
                    switchbacks_nasa_grade(&mut look);
                    look.exposure_bias_ev += NASA_DARK_EV;
                }
            }
            (look, (9.0, 12.0))
        }
        Planet::Europa => {
            // Jupiter and Io where the UE cockpit plate has them (centres at pixels (470, 155)
            // and (268, 150) of 1920×1080), through the plate's own camera. The frame is
            // mirrored after rendering, so a plate column x is column 1920 − x here.
            // (`map_look.json` gives Jupiter at az 110 / el 17 in shell space, which lands
            // off the plate's frame.)
            if let Some(cam) = content.cameras.get("cockpit") {
                let (eye, target) = (Vec3::from_array(cam.eye), Vec3::from_array(cam.target));
                let view = (target - eye).normalize();
                let right = view.cross(Vec3::Y).normalize();
                let up = right.cross(view);
                let t = (cam.fov_h_deg.to_radians() * 0.5).tan();
                let at = |px: f32, py: f32| {
                    let x = (1920.0 - px - 960.0) / 960.0 * t;
                    let y = (540.0 - py) / 960.0 * t;
                    (view + right * x + up * y).normalize().to_array()
                };
                level.atmosphere.sky_body = Some(SkyBody {
                    direction: at(470.0, 155.0),
                    // The map's Jupiter-shine (`fill_lux`); LOOK_R2_FILL=<lux> overrides.
                    fill_lux: std::env::var("LOOK_R2_FILL")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .or(content.map.fill_lux),
                    ..SkyBody::jupiter_from_europa()
                });
                level.atmosphere.moons = vec![SkyBody {
                    direction: at(268.0, 150.0),
                    ..SkyBody::io_from_europa()
                }];
            } else if let Some(dir) = content.map.jupiter {
                level.atmosphere.sky_body = Some(SkyBody {
                    direction: dir,
                    ..SkyBody::jupiter_from_europa()
                });
            }
            // Round 6, after terrain round 5: ground luma against UE at 12.0 — wide +20 %,
            // cockpit +12 %, smoke 0 %, fireball −16 %, close −17 %; mid +41 % is the
            // terrain shading there, not the exposure.
            // Round 11: expose for the ice. The radiance matches the path tracer (sunlit ice
            // 0.9–1.1× with Jupiter traced as an area light), so the grey, dim ice was the
            // meter: a frame half in cliff shadow metered to mid-grey. Snow compensation
            // (+1 EV, the photographic rule for a bright subject; `europa_snow_meter`,
            // weighted by how much of the frame is sunlit ice) with the band opened so the
            // meter, not the clamp, sets each frame; more contrast so the sunlit ice reaches
            // a clean white (p90 ≈ 190–200) without clipping.
            let mut look = bore_twelve_look();
            // Round 13: toward UE's tone. The path tracer's ice is neutral (lit hue 215°,
            // saturation 0.01), so UE's blue-white ice and blue shadows are its grade, not the
            // albedo: lit hue 222–235° / saturation 0.05–0.07 / value 0.78–0.83, shade 227–234° /
            // 0.13–0.25, against ours 214° / 0.02 / 0.69–0.74 and 215° / 0.06–0.09 (median HSV,
            // display). A cool balance with a magenta tint (6200 K, +0.1) brings the hue to
            // 224–228°. Blue's gamma above red's and green's cools the shadows without lifting
            // black (a lift greyed the black sky on cockpit, fireball and smoke). Gain 1.08 lifts
            // the sunlit ice, and contrast 1.3 → 1.05 opens the trench shadows (dim-laptop visible
            // shadow steps on bore_twelve_wide 42 % → ≈ 65 %) while the mid-tones hold
            // (set_piece_borehole p50 ≈ 139, UE 135; bore_twelve_close 109 → ≈ 119, UE 139).
            // Round 14: more shoulder for the sunlit ice. The raster's radiance matches the path
            // tracer's on bore_twelve_close (p50 396 vs 393, p90 1526 vs 1551 cd/m²), so the
            // remaining gap to UE's p90 (205–214) is the display curve, not albedo or specular:
            // gain 1.08 → 1.15 with gamma 0.94 → 0.87 lifts p90 by 8–10 (188 / 208 / 196 on
            // wide / close / borehole) and the mid-tones by 3–5. Going to gain 1.18 reaches UE's
            // p90 but costs the trench shadows on a dim laptop (bore_twelve_wide 65 % → 57 %).
            look.contrast = Some(1.05);
            look.grade = Some(LevelGrade {
                white_balance_kelvin: 6200.0,
                tint: 0.1,
                lift: [0.0; 3],
                gamma: [0.87, 0.88, 0.95],
                gain: [1.15; 3],
            });
            if let Some(meter) = look.meter.as_mut() {
                europa_snow_meter(meter);
            }
            (look, (10.5, 13.6))
        }
    };
    // `LOOK_R2_ATMOSPHERE=earth`: the map under an Earth-like sky (Rayleigh + Mie + ozone,
    // Earth's sun, no sky bodies), for comparisons across planet types at the same camera.
    // The exposure band is set for an Earth noon sky over this ground.
    let band = if std::env::var("LOOK_R2_ATMOSPHERE").as_deref() == Ok("earth") {
        level.atmosphere.preset = AtmospherePreset::Earth;
        level.atmosphere.sun_illuminance_lux = None;
        level.atmosphere.sky_body = None;
        level.atmosphere.moons.clear();
        level.atmosphere.haze = LevelHaze::default();
        (12.0, 13.0)
    } else {
        band
    };
    look.min_ev100 = Some(band.0);
    look.max_ev100 = Some(band.1);
    // Diagnostics for exposure studies: LOOK_R2_BAND="<min>,<max>" (EV100) and
    // LOOK_R2_METER_LOW=<0..1> (fraction of the darkest pixels the meter ignores).
    if let Some((lo, hi)) = std::env::var("LOOK_R2_BAND").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?))
    }) {
        look.min_ev100 = Some(lo);
        look.max_ev100 = Some(hi);
    }
    if let Some(low) = std::env::var("LOOK_R2_METER_LOW")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        && let Some(meter) = look.meter.as_mut()
    {
        meter.low_percentile = low.clamp(0.0, 0.95);
    }
    level.look = look;
    level
}

fn hash01(i: u32, salt: u32) -> f32 {
    regia_terrain::noise::hash2(i as i32, salt as i32, 0x5_A17C)
}

fn fbm(x: f32, z: f32, seed: u32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 1.0, 1.0, 0.0);
    for o in 0..octaves {
        sum += amp * regia_terrain::noise::value_noise2(x * freq, z * freq, seed.wrapping_add(o * 977));
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}

/// One mesa or spire: an elliptical frustum with steep stratified flanks.
struct Mesa {
    centre: [f32; 2],
    /// Top radius along the long axis, metres.
    top: f32,
    /// Flank run (base radius minus top radius), metres.
    run: f32,
    height: f32,
    /// Short-axis scale (1 round, < 1 elongated).
    aspect: f32,
    angle: f32,
}

impl Mesa {
    fn height_at(&self, x: f32, z: f32) -> f32 {
        let (dx, dz) = (x - self.centre[0], z - self.centre[1]);
        let (s, c) = self.angle.sin_cos();
        let u = dx * c + dz * s;
        let v = (-dx * s + dz * c) / self.aspect;
        let wobble = 1.0 + 0.12 * (fbm(x / 70.0, z / 70.0, 31, 3) - 0.5);
        let d = (u * u + v * v).sqrt() * wobble;
        let t = ((self.top + self.run - d) / self.run).clamp(0.0, 1.0);
        if t <= 0.0 {
            return 0.0;
        }
        // Steep flank, a talus apron at the foot and a flat caprock.
        let flank = t * t * (3.0 - 2.0 * t);
        let apron = 0.18 * t.powf(0.5);
        let h = self.height * (0.82 * flank + apron).min(1.0);
        // Sedimentary beds: pull toward 7 m ledges on the flank, not on the cap.
        let bed = 7.0;
        let ledge = (h / bed).round() * bed;
        let flank_weight = if t < 0.98 { 0.55 } else { 0.0 };
        h + (ledge - h) * flank_weight
    }
}

/// Places the planet's checked-in scatter set on `terrain` (procedural stand-in rocks).
fn scatter_for(planet: Planet, terrain: &R2Terrain) -> Result<ScatterField, LookdevError> {
    let set = crate::scatter_shots::scatter_set(planet)?;
    let source = ScatterSource {
        map: std::sync::Arc::new(terrain.map.clone()),
        holes: None,
        layers: Some(terrain.layers.clone()),
        density_maps: DensityMaps::new(),
    };
    Ok(ScatterField::new(
        &set,
        source,
        &crate::scatter_shots::rock_library(planet),
        0x5ca7_0000 + planet as u64,
    )
    .map_err(regia_terrain::TerrainError::from)?)
}

/// The four-layer photoreal stack per planet (the `terrain_lab --do27-look` stand-in's):
/// textured layers whose `albedo` is a tint, height blending, triplanar cliffs.
fn photoreal_layers(planet: Planet, map: &TerrainMap) -> TerrainLayers {
    let (lo, hi) = map.min_max_height();
    let span = hi - lo;
    // Roughness: wind-packed snow and frost crust 0.85–0.9, glazed ice only in its own
    // patches (0.3), Mars dust and rock 0.9.
    let layer =
        |name: &str, tint: [f32; 3], slope: (f32, f32), alt: (f32, f32), tiling: f32, roughness: f32| {
            LayerDesc {
                name: name.into(),
                albedo: tint,
                roughness,
                tiling,
                slope_min: slope.0,
                slope_max: slope.1,
                altitude_min: alt.0,
                altitude_max: alt.1,
                blend_sharpness: 0.15,
                triplanar: name.contains("cliff"),
                height_blend: 0.7,
                detail_normal_strength: 0.8,
                ..LayerDesc::ground()
            }
        };
    let layers = match planet {
        Planet::Europa => vec![
            // The Snow bake averages ≈ 0.9 linear, so a 0.56 tint gives ≈ 0.50 reflectance:
            // the UE plates' mid-grey wind-packed ice (0.45–0.55), not fresh snow (0.85+).
            // This diverges from `terrain_lab --do27-look` on purpose (the look lane owns
            // the harness stand-in's tone; the terrain lane owns relief).
            layer(
                "ice plain",
                [0.54, 0.555, 0.57],
                (0.0, 0.55),
                (lo - 1.0, hi + 1.0),
                0.9,
                0.85,
            ),
            layer(
                "dirty ice",
                [0.80, 0.78, 0.76],
                (0.0, 0.45),
                (lo - 1.0, lo + span * 0.45),
                0.7,
                0.9,
            ),
            layer(
                "ice cliff",
                [0.86, 0.89, 0.93],
                (0.45, 1.6),
                (lo - 1.0, hi + 1.0),
                0.6,
                0.75,
            ),
            layer(
                "frost cap",
                [0.6, 0.61, 0.63],
                (0.0, 0.35),
                (lo + span * 0.6, hi + 1.0),
                1.2,
                0.9,
            ),
            // Glazed ice in the hollows: the height blend breaks it into patches.
            LayerDesc {
                height_blend: 1.0,
                ..layer(
                    "glazed ice",
                    [0.82, 0.88, 0.95],
                    (0.0, 0.25),
                    (lo - 1.0, lo + span * 0.3),
                    0.5,
                    0.3,
                )
            },
        ],
        // Red-brown dust and rock (the plates' Mars), not ochre.
        Planet::Mars => vec![
            layer(
                "dust",
                [0.80, 0.44, 0.26],
                (0.0, 0.5),
                (lo - 1.0, hi + 1.0),
                0.8,
                0.92,
            ),
            layer(
                "talus",
                [0.72, 0.40, 0.26],
                (0.3, 0.85),
                (lo - 1.0, hi + 1.0),
                0.7,
                0.92,
            ),
            layer(
                "sandstone cliff",
                [0.84, 0.48, 0.32],
                (0.65, 1.6),
                (lo - 1.0, hi + 1.0),
                0.35,
                0.9,
            ),
            layer(
                "pale crest",
                [0.92, 0.64, 0.46],
                (0.0, 0.4),
                (lo + span * 0.7, hi + 1.0),
                0.9,
                0.9,
            ),
        ],
    };
    TerrainLayers {
        layers,
        ..Default::default()
    }
}

/// Baked layer textures and a macro set for [`photoreal_layers`].
fn photoreal_material(planet: Planet, map: &TerrainMap) -> TerrainMaterial {
    let styles: Vec<LayerStyle> = match planet {
        Planet::Europa => vec![
            LayerStyle::Snow,
            LayerStyle::Dirt,
            LayerStyle::Ice,
            LayerStyle::Snow,
            LayerStyle::Ice,
        ],
        // Fine dust grain (Dirt's clumps and pebbles), not wind ripples; ripples only on
        // the pale crests.
        Planet::Mars => vec![
            LayerStyle::Dirt,
            LayerStyle::Dirt,
            LayerStyle::Rock,
            LayerStyle::Sand,
        ],
    };
    let layers: Vec<Option<LayerTextures>> = styles
        .iter()
        .enumerate()
        .map(|(i, style)| Some(style.bake(256, 70 + i as u32)))
        .collect();
    let macro_set = MacroBake::from_heightfield(map, 2, 256, 909).ok();
    TerrainMaterial {
        layers,
        macro_set,
        params: MaterialParams {
            macro_world_size: WORLD_M,
            ..MaterialParams::default()
        },
    }
}

/// Stratified mesas, buttes and spires over a dune floor, 4096 m square.
fn mesa_terrain(resolution: u32) -> Result<R2Terrain, LookdevError> {
    let n = resolution.max(65);
    let mut mesas = Vec::new();
    for i in 0..70u32 {
        let spire = hash01(i, 7) < 0.3;
        let top = if spire {
            6.0 + 18.0 * hash01(i, 2)
        } else {
            40.0 + 170.0 * hash01(i, 2)
        };
        let height = if spire {
            90.0 + 140.0 * hash01(i, 3)
        } else {
            70.0 + 170.0 * hash01(i, 3)
        };
        mesas.push(Mesa {
            centre: [
                (hash01(i, 0) - 0.5) * WORLD_M * 0.92,
                (hash01(i, 1) - 0.5) * WORLD_M * 0.92,
            ],
            top,
            run: height * (0.42 + 0.4 * hash01(i, 4)),
            height,
            aspect: if spire { 1.0 } else { 0.45 + 0.55 * hash01(i, 5) },
            angle: hash01(i, 6) * std::f32::consts::PI,
        });
    }
    let spacing = WORLD_M / (n - 1) as f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        let z = -WORLD_M * 0.5 + row as f32 * spacing;
        for col in 0..n {
            let x = -WORLD_M * 0.5 + col as f32 * spacing;
            let floor = 6.0 * fbm(x / 260.0, z / 260.0, 3, 4) + 1.6 * fbm(x / 38.0, z / 38.0, 9, 2);
            let mut h = floor;
            for mesa in &mesas {
                h = h.max(floor + mesa.height_at(x, z));
            }
            heights.push(h);
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, WORLD_M, 1.0, 0.0)
        .map_err(|e| LookdevError::Config(format!("mesa heightfield: {e}")))?;
    let layers = photoreal_layers(Planet::Mars, &map);
    let material = photoreal_material(Planet::Mars, &map);
    let root_level = crate::scene::root_level_for(n);
    Ok(R2Terrain {
        map,
        layers,
        cdlod: CdlodConfig::default(),
        root_level,
        material,
        scatter: None,
        impostor_atlas: None,
        do27: None,
    })
}

/// The profile preset `LOOK_R2_TIER` names (Desktop High when unset or unknown).
#[must_use]
pub fn tier_preset() -> regia_render::RenderProfilePreset {
    use regia_render::RenderProfilePreset as P;
    match std::env::var("LOOK_R2_TIER").unwrap_or_default().as_str() {
        "mobile-low" => P::MobileLow,
        "mobile-high" => P::MobileHigh,
        "desktop-low" => P::DesktopLow,
        "deck" | "deck-spatial" => P::Deck,
        "desktop-ultra" => P::DesktopUltra,
        _ => P::DesktopHigh,
    }
}

/// `LOOK_R2_STREAM=1`: the memory tier of lane mem-stream — material texture streaming at
/// the tier's policy and the tier's terrain layer resolution ([`regia_terrain::layer_mip_skip_for`]).
/// (`LOOK_R2_STREAM=off` is not the memory tier: see [`streaming_off`].)
#[must_use]
pub fn memory_tier() -> bool {
    std::env::var_os("LOOK_R2_STREAM").is_some_and(|v| v != "off")
}

/// `LOOK_R2_STREAM=off`: no material texture streaming and no texture budget — what every
/// preset did before lane tex-budget (the "before" of `docs/evidence/perf/tex-budget-20261008.md`).
/// Unset: the preset's budgeted streaming ([`regia_render::TextureStreaming::for_preset`]).
#[must_use]
pub fn streaming_off() -> bool {
    std::env::var_os("LOOK_R2_STREAM").is_some_and(|v| v == "off")
}

/// The render tier from `LOOK_R2_TIER`: a profile preset name (`mobile-low`, `mobile-high`,
/// `desktop-low`, `deck`, `desktop-high`, `desktop-ultra`), `desktop-high-mid-range` (Desktop
/// High with `TerrainBudget::MID_RANGE`), `desktop-high-0.67` (Desktop High at the 0.67
/// TAAU render scale) or `deck-spatial` (the Deck's non-temporal fallback: SMAA + SGSR).
/// Unset: Desktop High.
#[must_use]
pub fn tier_settings() -> RenderSettings {
    use regia_render::RenderProfilePreset as P;
    let tier = std::env::var("LOOK_R2_TIER").unwrap_or_default();
    let preset = tier_preset();
    let mut settings = RenderSettings::from_profile(preset);
    match tier.as_str() {
        "desktop-high-mid-range" => settings.profile.terrain_budget = regia_render::TerrainBudget::MID_RANGE,
        "desktop-high-0.67" => settings.profile.render_scale = 0.67,
        "deck-spatial" => settings.profile = settings.profile.non_temporal(),
        _ => {}
    }
    // Diagnostic: LOOK_R2_PROFILE="gpu_driven=1,paged=1,csm=2,csm_res=1024,bloom=0,bloom_mips=2,
    // ssao=0,contact=0,reflocc=0,bent=0,gi_probes=0,scale=0.6,aa=fxaa|smaa|taa|none,
    // upscaler=sgsr|bilinear,budget=mid,aniso=0..16"
    // edits the tier's profile (cost studies, before/after shots).
    if let Ok(spec) = std::env::var("LOOK_R2_PROFILE") {
        let p = &mut settings.profile;
        for kv in spec.split(',') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let on = v == "1" || v == "true";
            match k {
                "gpu_driven" => p.gpu_driven = on,
                "depth_velocity" => p.depth_velocity = on,
                "paged" => p.paged_sun_shadows = on,
                "csm" => p.csm_cascade_count = v.parse().unwrap_or(p.csm_cascade_count),
                "csm_res" => p.csm_resolution = v.parse().unwrap_or(p.csm_resolution),
                "bloom" => p.bloom = on,
                "bloom_mips" => p.bloom_mip_count = v.parse().unwrap_or(p.bloom_mip_count),
                "ssao" if !on => p.ssao = regia_render::SsaoMode::Off,
                "contact" => p.contact_shadows = on,
                "reflocc" => p.reflection_occlusion = on,
                "bent" => p.bent_normals = on,
                "gi_probes" => p.gi_probes = on,
                "scale" => p.render_scale = v.parse().unwrap_or(p.render_scale),
                "aa" => {
                    p.anti_aliasing = match v {
                        "fxaa" => regia_render::AntiAliasing::Fxaa,
                        "smaa" => regia_render::AntiAliasing::Smaa,
                        "none" => regia_render::AntiAliasing::None,
                        _ => regia_render::AntiAliasing::Taa,
                    }
                }
                "upscaler" => {
                    p.spatial_upscaler = match v {
                        "sgsr" => regia_render::SpatialUpscaler::Sgsr,
                        _ => regia_render::SpatialUpscaler::Bilinear,
                    }
                }
                "budget" if v == "mid" => p.terrain_budget = regia_render::TerrainBudget::MID_RANGE,
                "aniso" => p.terrain_budget.anisotropy = v.parse().unwrap_or(p.terrain_budget.anisotropy),
                _ => {}
            }
        }
    }
    if memory_tier() {
        settings.profile.texture_streaming = if preset == P::Deck {
            regia_render::TextureStreaming::DECK
        } else {
            regia_render::TextureStreaming::DESKTOP
        };
    }
    if streaming_off() {
        settings.profile.texture_streaming = regia_render::TextureStreaming::OFF;
    }
    settings
}

/// The map clock of a shot (Mars latitude 20° N, northern spring).
#[must_use]
pub fn shot_clock(shot: R2Shot) -> LevelTimeOfDay {
    // Mars: the UE plates are carved by a low afternoon sun (long floor shadows, dark
    // shadow sides on the walls): ≈ 26° at 16.4 h here; the dusk plate's sun is lower (≈ 9°).
    let hour = match (shot, shot.planet()) {
        (R2Shot::DuskVista, _) => 17.6,
        // The tunnel plate's sun is high (short truss shadows on the bore floor).
        (R2Shot::TunnelInterior, _) => 14.6,
        (_, Planet::Mars) => 16.4,
        // Europa: a low morning sun (raking light over the ice, as on the UE plates).
        (_, Planet::Europa) => 9.5,
    };
    // Diagnostic: LOOK_R2_HOUR=<h> overrides every shot's clock.
    let hour = std::env::var("LOOK_R2_HOUR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(hour);
    LevelTimeOfDay {
        hour,
        day_of_year: 100,
        latitude_deg: 20.0,
    }
}

/// The Switchbacks look (Mars dust), tuned by eye against the UE v13 plates: high-key
/// sunlit sandstone, warm shadows that keep their colour, a light warm haze. Copy the
/// `look` and `atmosphere.haze` blocks into the map's `.level.ron`.
#[must_use]
pub fn switchbacks_look() -> LevelLook {
    LevelLook {
        // A daylight exposure held in a narrow band (EV100 10.25–10.75, the meter only
        // trims): the plates' sunlit sand is near white in every shot, whether the frame
        // holds sky or not (an averaging meter darkened the sky shots by ~2 EV). A firmer
        // curve keeps the shadowed floor about a third of the lit floor's light.
        exposure_bias_ev: 0.0,
        min_ev100: Some(10.25),
        max_ev100: Some(10.75),
        meter: Some(LevelMeter {
            low_percentile: 0.35,
            high_percentile: 0.97,
            center_weight: 0.4,
            ..LevelMeter::default()
        }),
        contrast: Some(1.8),
        saturation: Some(1.3),
        // Neutral-to-cool balance with a magenta tint: a warm balance (6900 K) cut blue,
        // and for an orange (r > g > b) hue = 60·(g − b)/(r − b) rises as blue falls, so
        // the mesas printed ochre/olive (30°) instead of the plates' red-brown (23–25°);
        // the yellow-orange shadow lift added to it. Measured by the hue test. With a neutral
        // grade the mesas still print ~30° (the lit cliff/strata tones; the near ground is
        // right at ~25°), so the tint carries the rest: neutrals lean slightly rose.
        // Round 4: the daylight exposure band lifts the lit mesas into AgX's shoulder,
        // which leans them toward yellow again: 5800 K + 0.45 holds mesa and floor at ≈ 25°.
        grade: Some(LevelGrade {
            white_balance_kelvin: 5800.0,
            tint: 0.45,
            lift: [0.010, 0.003, 0.0],
            gamma: [1.0; 3],
            gain: [1.0; 3],
        }),
        lut: None,
        local_exposure: None,
        baked_gi: None,
        lightmap: None,
        captures: None,
    }
}

/// The Mars grade variant `do27_level` applies on DO27's Switchbacks shots.
/// `LOOK_R2_MARS_GRADE=nasa` selects [`MarsGrade::Nasa`], `nasa_dark` [`MarsGrade::NasaDark`];
/// anything else (or unset) keeps [`MarsGrade::Ue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarsGrade {
    /// Round 13: the UE plates' tone (saturated orange, saturation 1.5).
    Ue,
    /// Round 14: Curiosity Mastcam / Perseverance Mastcam-Z white-balanced photographs
    /// (see [`switchbacks_nasa_grade`]).
    Nasa,
    /// [`MarsGrade::Nasa`] at [`NASA_DARK_EV`]: the photographs' darker mid-tones.
    NasaDark,
}

/// Exposure offset of [`MarsGrade::NasaDark`] against [`MarsGrade::Nasa`]: the darkest step that
/// keeps at least 85 % of shadow-band brightness steps visible on `DisplayModel::DIM_LAPTOP` on
/// every Switchbacks shot (switchbacks_wide 97 %, switchbacks_mid 89 %, switchbacks_close 96 %,
/// tunnel_interior 85 %). −0.5 EV dropped switchbacks_mid to 81 % and tunnel_interior to 83 %.
pub const NASA_DARK_EV: f32 = -0.35;

impl MarsGrade {
    /// The variant `LOOK_R2_MARS_GRADE` asks for.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("LOOK_R2_MARS_GRADE").as_deref() {
            Ok("nasa") => Self::Nasa,
            Ok("nasa_dark") => Self::NasaDark,
            _ => Self::Ue,
        }
    }
}

/// The "nasa" Switchbacks grade (look-r2 round 14), matched to NASA/JPL's white-balanced
/// ("as if under Earth daylight") rover photographs of layered sediment, sand and rock
/// (PIA16768, PIA18881, PIA20765, PIA20843, PIA21851; median sunlit hue 23°, saturation 0.23,
/// shade 24° / 0.46, display HSV), with the natural-colour releases measured alongside
/// (PIA16769, PIA24746, PIA24833, PIA25671, PIA26373: ≈ 26° / 0.5). Against the "ue" grade a
/// warmer-light balance (5200 K) takes the rust cast out of the light, saturation 1.5 → 1.15
/// brings the sunlit landscape to ≈ 0.26 (0.46 on the sand close-up), a smaller magenta tint
/// (0.3) and a green-blue lift keep the shadows from going red, and contrast 1.65 with +0.1 EV keeps
/// the brightness (switchbacks_close p50 158, UE 155). Copy into the map's `.level.ron` look.
pub fn switchbacks_nasa_grade(look: &mut LevelLook) {
    look.exposure_bias_ev = 0.8;
    look.saturation = Some(1.15);
    look.contrast = Some(1.65);
    look.grade = Some(LevelGrade {
        white_balance_kelvin: 5200.0,
        tint: 0.3,
        // Green-blue weighted lift: trims the shade's red cast toward the natural-colour photos
        // (shade hue 19–24° → 22–25°, saturation 0.59–0.71 → 0.57–0.62; NASA natural colour
        // 26° / 0.47) without moving the sunlit match, and opens the dim-laptop shadow steps
        // (switchbacks_mid 89 % → 97 % at nasa_dark).
        lift: [0.0, 0.03, 0.02],
        gamma: [1.0; 3],
        gain: [1.0; 3],
    });
}

/// The Switchbacks dust: a thin 600 m layer over the planet's own aerosol (the far mesas
/// keep their strata and edges, as on the UE plates).
#[must_use]
pub fn switchbacks_haze() -> LevelHaze {
    LevelHaze {
        density: 1.2e-5,
        scale_height_m: 600.0,
        base_height_m: 0.0,
    }
}

/// The Bore Twelve look (Europa ice under a 4.4 klx sun and a black sky): a daylight
/// exposure held in a narrow band (EV100 11.5–11.7) so ≈ 0.5-reflectance ice prints the
/// UE plates' mid-grey; an averaging meter pushed any ice to the same bright level
/// whatever its albedo. Neutral grade.
#[must_use]
pub fn bore_twelve_look() -> LevelLook {
    LevelLook {
        exposure_bias_ev: 0.0,
        min_ev100: Some(11.5),
        max_ev100: Some(11.7),
        meter: Some(LevelMeter {
            low_percentile: 0.45,
            high_percentile: 0.98,
            center_weight: 0.3,
            ..LevelMeter::default()
        }),
        contrast: Some(1.15),
        saturation: Some(1.15),
        grade: None,
        lut: None,
        local_exposure: None,
        baked_gi: None,
        lightmap: None,
        captures: None,
    }
}

/// Bore Twelve's snow compensation (look-r2 round 11): +1 EV, the photographer's rule for a
/// bright subject, applied in proportion to how much of the metered frame is sunlit ice —
/// pixels from 250 cd/m² (snow of albedo ≈ 0.68 under Europa's ≈ 4.4 klx sun, lit at
/// cos θ ≥ 0.25) to 16× that. An interior, or a frame filled by fire (above the 16× cap)
/// or smoke (below the threshold), keeps the plain meter. On the twelve plates every Europa
/// frame is 50–93 % sunlit ice at this threshold (the close-up 59 %), so all of them take
/// 0.83–1.0 of the +1 EV; 400 cd/m² dropped the close-up to 44 % (below the vistas' 48–55 %),
/// so no threshold separates the close-up from the horizon vistas by coverage.
/// `LOOK_R2_BRIGHT="<threshold cd/m²>,<EV>"` overrides it.
pub fn europa_snow_meter(meter: &mut LevelMeter) {
    meter.bright_threshold_cd_m2 = 250.0;
    meter.bright_bias_ev = 1.0;
    if let Some((t, ev)) = std::env::var("LOOK_R2_BRIGHT").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?))
    }) {
        meter.bright_threshold_cd_m2 = t;
        meter.bright_bias_ev = ev;
    }
}

/// The level settings a game ships for the shot's map: the planet preset, the clock,
/// (Europa) Jupiter and Io, and the map's look and haze.
#[must_use]
pub fn shot_level(shot: R2Shot) -> LevelSettings {
    let mut level = LevelSettings::default();
    level.atmosphere.time_of_day = Some(shot_clock(shot));
    match shot.planet() {
        Planet::Mars => {
            level.atmosphere.preset = AtmospherePreset::Mars;
            level.atmosphere.haze = switchbacks_haze();
            // Diagnostic: LOOK_R2_HAZE=<density /m> overrides the Switchbacks haze.
            if let Some(d) = std::env::var("LOOK_R2_HAZE").ok().and_then(|v| v.parse().ok()) {
                level.atmosphere.haze.density = d;
            }
            level.look = switchbacks_look();
        }
        Planet::Europa => {
            level.atmosphere.preset = AtmospherePreset::Airless;
            // Where the UE plate has them: Jupiter's centre at pixel (470, 155) and Io at
            // (268, 150) of 1920×1080 through the 90° lens (tangent offsets from the axis).
            let sun = sun_dir(shot);
            let cam = cockpit_camera();
            let flat = yaw_dir(sun_azimuth(sun) + cam.yaw_from_sun);
            let pitch = cam.pitch.to_radians();
            let view = (flat * pitch.cos() + Vec3::Y * pitch.sin()).normalize();
            let right = view.cross(Vec3::Y).normalize();
            let up = right.cross(view);
            let at = |px: f32, py: f32| {
                (view + right * ((px - 960.0) / 960.0) + up * ((540.0 - py) / 960.0)).normalize()
            };
            let dir = at(470.0, 155.0);
            level.atmosphere.sky_body = Some(SkyBody {
                direction: dir.to_array(),
                ..SkyBody::jupiter_from_europa()
            });
            let io = at(268.0, 150.0);
            level.atmosphere.moons = vec![SkyBody {
                direction: io.to_array(),
                ..SkyBody::io_from_europa()
            }];
            level.look = bore_twelve_look();
        }
    }
    level
}

/// The froxel fog resource: none by default (the level haze drives the fog). With
/// `LOOK_R2_AUTHORED_FOG` set, the heavy haze the before frames were authored with
/// (the [`crate::scene`] stand-in's planet haze: 4.2e-4 /m with an ≈80 m scale height
/// above the floor), to show the engine fixes on the same input.
#[must_use]
pub fn shot_fog(shot: R2Shot) -> VolumetricFogSettings {
    let authored = std::env::var_os("LOOK_R2_AUTHORED_FOG").is_some();
    match shot.planet() {
        Planet::Mars if authored => VolumetricFogSettings {
            density: 0.000_42,
            height_falloff: 0.012,
            // The dune floor; a base on a mesa top would pile fog exponentially below it.
            base_height: 0.0,
            ..VolumetricFogSettings::default()
        },
        _ => VolumetricFogSettings::default(),
    }
}

fn time_of(clock: LevelTimeOfDay) -> TimeOfDay {
    TimeOfDay {
        hour: clock.hour,
        day_of_year: clock.day_of_year,
        latitude_deg: clock.latitude_deg,
    }
}

fn sun_dir(shot: R2Shot) -> Vec3 {
    Vec3::from_array(sun_position(&time_of(shot_clock(shot))).direction)
}

/// Azimuth clockwise from north (+Z) through east (+X), degrees.
fn sun_azimuth(toward_sun: Vec3) -> f32 {
    toward_sun.x.atan2(toward_sun.z).to_degrees()
}

fn yaw_dir(azimuth_deg: f32) -> Vec3 {
    let a = azimuth_deg.to_radians();
    Vec3::new(a.sin(), 0.0, a.cos())
}

fn cockpit_camera() -> CameraSpec {
    CameraSpec {
        eye: [-600.0, 500.0],
        above: 2.2,
        yaw_from_sun: 120.0,
        pitch: 2.0,
        // 90° horizontal at 16:9, the UE camera.
        fov_y_deg: 58.72,
    }
}

fn camera_for(shot: R2Shot) -> CameraSpec {
    let spec = |eye: [f32; 2], above: f32, yaw_from_sun: f32, pitch: f32| CameraSpec {
        eye,
        above,
        yaw_from_sun,
        pitch,
        fov_y_deg: 58.72,
    };
    match shot {
        // Mars: the UE plates have the sun ahead and to the right of the lens (shadow sides
        // of the walls face the camera, shadows fall toward it and to the left), so the
        // yaw is negative (sun right of the view axis), not behind the camera.
        R2Shot::DuskVista => spec([-300.0, -400.0], 60.0, -50.0, -12.0),
        R2Shot::SwitchbacksWide => spec([-368.0, -352.0], 90.0, -70.0, -22.0),
        R2Shot::SwitchbacksMid => spec([-500.0, -150.0], 3.0, -75.0, 2.0),
        R2Shot::SwitchbacksClose => spec([-440.0, -300.0], 1.2, -60.0, -16.0),
        R2Shot::TunnelInterior => spec([-620.0, -80.0], 1.7, 100.0, -17.0),
        R2Shot::Cockpit => cockpit_camera(),
        R2Shot::BoreTwelveWide => spec([-300.0, 200.0], 70.0, 120.0, -24.0),
        R2Shot::BoreTwelveMid => spec([-500.0, 350.0], 25.0, 140.0, -18.0),
        R2Shot::BoreTwelveClose => spec([-650.0, 450.0], 3.0, 110.0, -30.0),
        R2Shot::SetPieceBorehole => spec([-300.0, 200.0], 35.0, 100.0, -18.0),
        R2Shot::Fireball20m => spec([-600.0, 500.0], 2.2, 140.0, 0.0),
        R2Shot::Smoke80m => spec([-600.0, 500.0], 2.2, 160.0, 0.0),
    }
}

// ---------------------------------------------------------------------------
// Props
// ---------------------------------------------------------------------------

const M_HULL: usize = 0;
const M_METAL: usize = 1;
const M_WALL: usize = 2;
const M_FLOOR: usize = 3;
const M_PAD: usize = 4;
const M_HANGAR: usize = 5;

/// A dusty, cracked floor: mid-grey-tan with dark cracks (a few percent of the
/// area) and grain. Contrast like a photographed rock texture, not a mask.
fn floor_texture() -> LabTexture {
    let n = 128u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let fx = x as f32 / n as f32;
            let fy = y as f32 / n as f32;
            let grain = regia_terrain::noise::value_noise2(fx * 40.0, fy * 40.0, 11);
            let blotch = regia_terrain::noise::value_noise2(fx * 6.0, fy * 6.0, 5);
            let ridge = (regia_terrain::noise::value_noise2(fx * 9.0, fy * 9.0, 23) - 0.5).abs();
            let crack = (1.0 - (ridge / 0.035).clamp(0.0, 1.0)).powi(2);
            let base = 0.62 + 0.20 * blotch + 0.10 * grain;
            let v = base * (1.0 - 0.55 * crack);
            let to_srgb = |c: f32| (c.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
            rgba.extend_from_slice(&[to_srgb(v * 0.62), to_srgb(v * 0.47), to_srgb(v * 0.36), 255]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

pub(crate) fn materials() -> Vec<LabMaterial> {
    let mat = |c: [f32; 3], metallic: f32, roughness: f32| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        metallic,
        roughness,
        ..LabMaterial::default()
    };
    let mut floor = mat([1.0, 1.0, 1.0], 0.0, 0.92);
    floor.albedo = Some(floor_texture());
    vec![
        mat([0.30, 0.31, 0.33], 0.2, 0.45), // hull
        mat([0.55, 0.55, 0.57], 1.0, 0.40), // steel
        mat([0.34, 0.24, 0.18], 0.0, 0.90), // rock wall
        floor,
        mat([0.36, 0.36, 0.37], 0.0, 0.80), // landing pad
        mat([0.55, 0.50, 0.43], 0.1, 0.70), // hangar panels
    ]
}

fn placed(mesh: (Vec<LabVertex>, Vec<u32>), transform: Mat4, material: usize) -> LabMesh {
    let (mut vertices, indices) = mesh;
    let normal_m = regia_core::glam::Mat3::from_mat4(transform).inverse().transpose();
    for v in &mut vertices {
        v.position = transform
            .transform_point3(Vec3::from_array(v.position))
            .to_array();
        v.normal = (normal_m * Vec3::from_array(v.normal))
            .normalize_or_zero()
            .to_array();
        // World-scale UVs on boxes so textures do not stretch with the slab.
        v.uv = [v.position[0] * 0.25 + v.position[1] * 0.25, v.position[2] * 0.25];
    }
    LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

/// Axis box in a local frame (`origin`, `forward` along local +X), placed by its
/// centre and half extents.
fn local_box(origin: Vec3, forward: Vec3, centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let right = Vec3::new(-forward.z, 0.0, forward.x);
    let basis = Mat4::from_cols(
        forward.extend(0.0),
        Vec3::Y.extend(0.0),
        right.extend(0.0),
        (origin + forward * centre[0] + Vec3::Y * centre[1] + right * centre[2]).extend(1.0),
    );
    placed(box_mesh(half), basis, material)
}

/// Pushes a [`local_box`] mesh and its SDF box (the cut is axis-aligned, so the
/// local box is a world AABB).
fn push_box(
    out: &mut Vec<LabMesh>,
    sdf: &mut SdfScene,
    (origin, forward, centre, half, material): (Vec3, Vec3, [f32; 3], [f32; 3], usize),
) {
    out.push(local_box(origin, forward, centre, half, material));
    let right = Vec3::new(-forward.z, 0.0, forward.x);
    let world = origin + forward * centre[0] + Vec3::Y * centre[1] + right * centre[2];
    let extent = forward.abs() * half[0] + Vec3::Y * half[1] + right.abs() * half[2];
    let albedo = match material {
        M_FLOOR => [0.43, 0.30, 0.22],
        M_METAL => [0.55, 0.55, 0.57],
        _ => [0.34, 0.24, 0.18],
    };
    sdf.shapes.push(SdfShape::Box {
        center: world.to_array(),
        half: extent.to_array(),
        albedo,
        emissive: [0.0; 3],
    });
}

/// A roofed cut 34 m long along `forward`: floor, walls, a roof over the first
/// 18 m, then trusses over an open trench, a raised walkway and posts.
fn tunnel_props(origin: Vec3, forward: Vec3) -> (Vec<LabMesh>, SdfScene) {
    let mut out = Vec::new();
    let mut sdf = SdfScene::default();
    let len = 34.0;
    push_box(
        &mut out,
        &mut sdf,
        (
            origin,
            forward,
            [len * 0.5 - 4.0, -0.15, 0.0],
            [len * 0.5, 0.15, 4.5],
            M_FLOOR,
        ),
    );
    for side in [-1.0f32, 1.0] {
        push_box(
            &mut out,
            &mut sdf,
            (
                origin,
                forward,
                [len * 0.5 - 4.0, 3.2, side * 4.9],
                [len * 0.5, 3.4, 0.4],
                M_WALL,
            ),
        );
    }
    // Roof over the bore, open from x = 14 on.
    push_box(
        &mut out,
        &mut sdf,
        (origin, forward, [5.0, 6.75, 0.0], [9.0, 0.25, 5.3], M_WALL),
    );
    // Back wall behind the camera.
    push_box(
        &mut out,
        &mut sdf,
        (origin, forward, [-4.2, 3.2, 0.0], [0.3, 3.4, 5.3], M_WALL),
    );
    // Trusses across the open trench.
    for i in 0..6 {
        let x = 15.0 + i as f32 * 2.6;
        push_box(
            &mut out,
            &mut sdf,
            (origin, forward, [x, 6.4, 0.0], [0.12, 0.18, 4.6], M_METAL),
        );
        push_box(
            &mut out,
            &mut sdf,
            (origin, forward, [x, 5.6, 0.0], [0.06, 0.06, 4.6], M_METAL),
        );
    }
    for side in [-1.0f32, 1.0] {
        push_box(
            &mut out,
            &mut sdf,
            (
                origin,
                forward,
                [21.5, 6.4, side * 2.3],
                [6.7, 0.1, 0.08],
                M_METAL,
            ),
        );
    }
    // Walkway: deck, rails on posts.
    push_box(
        &mut out,
        &mut sdf,
        (origin, forward, [9.0, 1.2, -2.6], [7.0, 0.06, 0.7], M_METAL),
    );
    for x in [3.0f32, 9.0, 15.0] {
        push_box(
            &mut out,
            &mut sdf,
            (origin, forward, [x, 0.6, -2.0], [0.06, 0.6, 0.06], M_METAL),
        );
        push_box(
            &mut out,
            &mut sdf,
            (origin, forward, [x, 1.8, -1.95], [0.04, 0.6, 0.04], M_METAL),
        );
    }
    push_box(
        &mut out,
        &mut sdf,
        (origin, forward, [9.0, 2.4, -1.95], [6.1, 0.04, 0.04], M_METAL),
    );
    // The ground the cut stands on, for rays that leave it.
    sdf.shapes.push(SdfShape::Box {
        center: (origin - Vec3::Y * 1.0).to_array(),
        half: [120.0, 0.7, 120.0],
        albedo: [0.42, 0.25, 0.13],
        emissive: [0.0; 3],
    });
    (out, sdf)
}

/// A hangar-sized set piece (the stand-in for DO27's base/borehole buildings): a long hall,
/// a roof block, a door recess.
fn hangar_props(ground: Vec3, forward: Vec3) -> Vec<LabMesh> {
    vec![
        local_box(ground, forward, [0.0, 4.0, 0.0], [16.0, 4.5, 9.0], M_HANGAR),
        local_box(ground, forward, [2.0, 9.3, 0.0], [5.0, 1.0, 4.0], M_HANGAR),
        local_box(ground, forward, [16.2, 3.0, 0.0], [0.4, 3.0, 5.0], M_METAL),
    ]
}

fn vehicle_props(ground: Vec3, forward: Vec3) -> Vec<LabMesh> {
    let mut out = Vec::new();
    let pad = Mat4::from_translation(ground + Vec3::Y * 0.05) * Mat4::from_scale(Vec3::new(9.0, 0.08, 9.0));
    out.push(placed(uv_sphere(1.0, 32, 4), pad, M_PAD));
    out.push(local_box(
        ground,
        forward,
        [0.0, 1.5, 0.0],
        [4.2, 1.2, 1.7],
        M_HULL,
    ));
    out.push(local_box(
        ground,
        forward,
        [2.6, 3.0, 0.0],
        [1.4, 0.6, 1.3],
        M_HULL,
    ));
    out.push(local_box(
        ground,
        forward,
        [-3.0, 3.0, 0.0],
        [0.9, 0.5, 1.5],
        M_METAL,
    ));
    out
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// What [`render_shot`] measured besides the pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct R2Frame {
    /// Eye position.
    pub eye: [f32; 3],
    /// Look target.
    pub target: [f32; 3],
    /// Unit vector toward the sun.
    pub toward_sun: [f32; 3],
}

/// What the reference path tracer needs from one parity shot, handed to the hook of
/// [`render_shot_with`] after the last frame (`specs/27` § 2: PT vs real-time).
pub struct ShotCapture<'a> {
    /// The real-time frame exactly as drawn (props, scatter rocks, sun, quality).
    pub frame: &'a regia_render::FrameScene<'a>,
    /// Linear HDR of that frame (before exposure, bloom and tonemap).
    pub hdr_full: regia_render::HdrCapture,
    /// Linear HDR of the same frame with every indirect term off (no IBL, no ambient, no
    /// probe GI, no screen-space GI/reflections, no AO): sun and local lights only.
    pub hdr_direct: regia_render::HdrCapture,
    /// The planet's atmosphere as the frame used it.
    pub planet: regia_render::AtmosphereParameters,
    /// Unit vector toward the sun.
    pub toward_sun: [f32; 3],
    /// The terrain heightfield the shared draw rendered.
    pub map: &'a TerrainMap,
    /// Its splat layers.
    pub layers: &'a TerrainLayers,
    /// The DO27 map directory when the shot renders DO27's content.
    pub map_dir: Option<std::path::PathBuf>,
    /// With GPU probes on: the frame's half-resolution screen lookup (width, height, texels).
    pub probe_screen: Option<(u32, u32, Vec<regia_render::DdgiScreenTexel>)>,
    /// The terrain shader's own albedo over the whole map ([`ShotHook::albedo`]).
    pub terrain_albedo: regia_pt::PtTexture,
    /// The terrain shader's albedo from the shot camera, per pixel.
    pub terrain_view_albedo: Option<TerrainViewAlbedo>,
    /// The outdoor resources the frame used (air, fog, sky bodies).
    pub outdoor: regia_render::OutdoorParams,
}

/// The terrain shader's albedo rendered from a shot's camera ([`ShotCapture::terrain_view_albedo`],
/// `TerrainSharedDraw::render_albedo_view`): what the frame multiplies the light by at every
/// terrain pixel, and the shading normal it lights that pixel with.
#[derive(Clone, Debug)]
pub struct TerrainViewAlbedo {
    /// World → clip of the camera it was rendered with (column-major, 0..1 depth).
    pub view_proj: [[f32; 4]; 4],
    /// That camera's eye (world).
    pub eye: [f32; 3],
    /// sRGB RGBA8 at the frame's size, top row first; alpha 0 where no terrain was drawn.
    pub albedo: regia_pt::PtTexture,
    /// The shading normal at the same pixels (`TerrainSharedDraw::render_normal_view`:
    /// RGB = 0.5 + 0.5·n, sRGB-encoded).
    pub normal: regia_pt::PtTexture,
}

/// The hook of [`render_shot_with`]: the terrain albedo bake it wants (run on the shot's own
/// terrain draw before the first frame) and the callback after the last frame.
pub struct ShotHook<'h> {
    /// Terrain albedo bake settings.
    pub albedo: regia_pt::terrain_bake::AlbedoBake,
    /// Called once with the capture.
    pub run: &'h mut dyn FnMut(ShotCapture<'_>) -> Result<(), LookdevError>,
}

/// Renders `shot` and returns sRGB RGBA8 pixels, top row first.
///
/// # Errors
///
/// Terrain generation, the GPU upload or the frame failed.
pub fn render_shot(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    camera: CameraOverride,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    render_shot_with(device, planets, shot, options, camera, None, None)
}

/// Meshes placed into a shot's world with their own camera (the hero-asset harness,
/// [`crate::hero`]): the shot keeps its map, sun, look and the map's set pieces; the
/// insert's meshes index the insert's materials.
#[derive(Clone)]
pub struct HeroInsert {
    /// Meshes in world space (material indices into [`Self::materials`]).
    pub meshes: Vec<LabMesh>,
    /// Their materials.
    pub materials: Vec<LabMaterial>,
    /// Eye, world metres.
    pub eye: [f32; 3],
    /// Look target, world metres.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Per-instance glass of hero meshes: (index in [`Self::meshes`], view mode, damage with
    /// its page already stamped against that mesh) (`regia_render::glass`).
    pub glass: Vec<(
        usize,
        regia_render::glass::GlassView,
        Option<regia_render::glass::GlassDamage>,
    )>,
    /// Large-mesh occlusion and bounce proxies (`FrameRenderer::set_mesh_occluders`); empty
    /// for none. `LOOK_R2_NO_MESH_OCCLUDERS=1` drops them (before / after plates).
    pub occluders: Vec<regia_render::MeshOccluder>,
    /// Point and spot lights placed with the meshes (world metres), e.g. a host's hull lights.
    pub lights: Vec<LabLocalLight>,
}

/// [`render_shot`] with [`HeroInsert`] meshes and camera.
///
/// # Errors
///
/// As [`render_shot`].
pub fn render_hero(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    hero: &HeroInsert,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    render_shot_with(
        device,
        planets,
        shot,
        options,
        CameraOverride::default(),
        Some(hero),
        None,
    )
}

/// [`render_shot`] that also captures the last frame's linear HDR (full and direct-only) and
/// hands it with the frame description to `hook` — the reference path tracer's input.
///
/// # Errors
///
/// Terrain generation, the GPU upload, the frame, the HDR readback or the hook failed.
pub fn render_shot_with(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    camera: CameraOverride,
    hero: Option<&HeroInsert>,
    hook: Option<ShotHook<'_>>,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    render_shot_inner(device, planets, shot, options, camera, hero, hook, None)
}

/// [`render_shot`] with a DO27 effect ([`crate::fx::FxInsert`]) on the ground under the
/// shot's look-at point, simulated over the shot's frames and captured on the last.
///
/// # Errors
///
/// Terrain generation, the GPU upload or the frame failed.
pub fn render_fx_shot(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    fx: &mut crate::fx::FxInsert,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    render_shot_inner(
        device,
        planets,
        shot,
        options,
        CameraOverride::default(),
        None,
        None,
        Some(fx),
    )
}

/// [`render_hero`] with a DO27 effect ([`crate::fx::FxInsert`], usually scripted): DO27 meshes
/// and their camera, and the effect simulated over the shot's frames.
///
/// # Errors
///
/// As [`render_shot`].
pub fn render_hero_fx(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    hero: &HeroInsert,
    fx: &mut crate::fx::FxInsert,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    render_shot_inner(
        device,
        planets,
        shot,
        options,
        CameraOverride::default(),
        Some(hero),
        None,
        Some(fx),
    )
}

#[allow(clippy::too_many_arguments)] // the shot plus its optional inserts
fn render_shot_inner(
    device: &RenderDevice,
    planets: &mut R2Planets,
    shot: R2Shot,
    options: R2Options,
    camera: CameraOverride,
    hero: Option<&HeroInsert>,
    mut hook: Option<ShotHook<'_>>,
    mut fx: Option<&mut crate::fx::FxInsert>,
) -> Result<(Vec<u8>, R2Frame), LookdevError> {
    let scene = planets.get(shot.planet(), options.terrain_resolution)?;
    let do27 = scene.do27.as_deref();
    // LOOK_R2_DO27_RAW=1: DO27's own level exactly as migrated (no look, haze or sky body of
    // this lane), with the map look's authored sun, to check the engine's defaults.
    let raw_do27 = std::env::var_os("LOOK_R2_DO27_RAW").is_some();
    let mut level = match do27 {
        Some(c) if raw_do27 => c.map.level.clone(),
        Some(c) => do27_level(shot, c),
        None => shot_level(shot),
    };
    let do27_camera = do27.and_then(|c| c.cameras.get(shot.name()).copied());
    let do27_sun = do27.map(|c| {
        let mut sun = c.map.sun;
        // The plates' sun by default (a fair side by side); LOOK_R2_AUTHORED_SUN=1 keeps the
        // map look's direction, LOOK_R2_SUN=<elevation°>,<azimuth°> sets one.
        let (el, az) = std::env::var("LOOK_R2_SUN")
            .ok()
            .and_then(|v| {
                v.split_once(',')
                    .and_then(|(a, b)| Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?)))
            })
            .unwrap_or_else(|| crate::do27::plate_sun(shot.planet()));
        // DO27's map look now carries the plate angles (35°/148°, 24°/185°); the override
        // stays for older drops (LOOK_R2_PLATE_SUN=1) and LOOK_R2_SUN.
        if (std::env::var_os("LOOK_R2_PLATE_SUN").is_some() || std::env::var_os("LOOK_R2_SUN").is_some())
            && !raw_do27
        {
            sun.toward = crate::do27::sun_toward(el, az);
        }
        sun
    });
    let do27_props = do27.map(|c| (c.map.props.clone(), c.map.prop_materials.clone()));
    if let Some(density) = camera.haze_density {
        level.atmosphere.haze.density = density;
    }
    let mut spec = camera_for(shot);
    if let Some(eye) = camera.eye {
        spec.eye = eye;
    }
    if let Some(above) = camera.above {
        spec.above = above;
    }
    if let Some(yaw) = camera.yaw_from_sun {
        spec.yaw_from_sun = yaw;
    }
    if let Some(pitch) = camera.pitch {
        spec.pitch = pitch;
    }
    let toward_sun = do27_sun.map_or_else(|| sun_dir(shot), |s| Vec3::from_array(s.toward));
    let mut heading = yaw_dir(sun_azimuth(toward_sun) + spec.yaw_from_sun);
    if shot == R2Shot::TunnelInterior {
        // The cut runs along a world axis so its SDF boxes are exact AABBs.
        heading = if heading.x.abs() > heading.z.abs() {
            Vec3::new(heading.x.signum(), 0.0, 0.0)
        } else {
            Vec3::new(0.0, 0.0, heading.z.signum())
        };
    }
    let ground = scene.height_at(spec.eye[0], spec.eye[1]);
    let mut meshes = Vec::new();
    let mut interior_scene = None;
    let eye = match shot {
        R2Shot::TunnelInterior => {
            // The cut sits on the highest ground under its footprint, so the terrain
            // never pokes through the floor; the eye stands 3 m in from the back wall.
            let right = Vec3::new(-heading.z, 0.0, heading.x);
            let mut floor = f32::MIN;
            for i in -4..=30 {
                for j in -5..=5 {
                    let p = Vec3::new(spec.eye[0], 0.0, spec.eye[1]) + heading * i as f32 + right * j as f32;
                    floor = floor.max(scene.height_at(p.x, p.z));
                }
            }
            let origin = Vec3::new(spec.eye[0], floor + 0.3, spec.eye[1]);
            let (props, sdf) = tunnel_props(origin, heading);
            meshes.extend(props);
            interior_scene = Some(sdf);
            // Under the roof, 5 m before its edge: the bore floor in front, the sunlit
            // trench and its truss shadows beyond.
            origin + Vec3::Y * spec.above + heading * 9.0 - right * 1.2
        }
        R2Shot::BoreTwelveWide | R2Shot::SetPieceBorehole => {
            let reach = if shot == R2Shot::BoreTwelveWide {
                160.0
            } else {
                90.0
            };
            let at = Vec3::new(spec.eye[0], 0.0, spec.eye[1]) + heading * reach;
            let base = Vec3::new(at.x, scene.height_at(at.x, at.z) - 0.5, at.z);
            meshes.extend(hangar_props(base, Vec3::new(heading.z, 0.0, -heading.x)));
            Vec3::new(spec.eye[0], ground + spec.above, spec.eye[1])
        }
        R2Shot::Fireball20m | R2Shot::Smoke80m => {
            let reach = if shot == R2Shot::Fireball20m { 20.0 } else { 80.0 };
            let at = Vec3::new(spec.eye[0], 0.0, spec.eye[1]) + heading * reach;
            let pad = Vec3::new(at.x, scene.height_at(at.x, at.z), at.z);
            meshes.extend(vehicle_props(pad, Vec3::new(heading.z, 0.0, -heading.x)));
            Vec3::new(spec.eye[0], ground + spec.above, spec.eye[1])
        }
        R2Shot::SwitchbacksMid => {
            let at = Vec3::new(spec.eye[0], 0.0, spec.eye[1]) + heading * 40.0;
            let pad = Vec3::new(at.x, scene.height_at(at.x, at.z), at.z);
            meshes.extend(vehicle_props(pad, Vec3::new(heading.z, 0.0, -heading.x)));
            Vec3::new(spec.eye[0], ground + spec.above, spec.eye[1])
        }
        _ => Vec3::new(spec.eye[0], ground + spec.above, spec.eye[1]),
    };
    let pitch = spec.pitch.to_radians();
    let look = (heading * pitch.cos() + Vec3::Y * pitch.sin()).normalize();
    let mut target = eye + look * 100.0;
    let mut eye = eye;
    let mut fov_y = spec.fov_y_deg.to_radians();
    if let Some(cam) = do27_camera {
        // The plate camera and the map's own set pieces replace the stand-in framing.
        eye = Vec3::from_array(cam.eye);
        target = Vec3::from_array(cam.target);
        fov_y = cam.fov_y(options.width as f32 / options.height.max(1) as f32);
        meshes.clear();
        interior_scene = None;
    }
    if let Some((props, _)) = &do27_props {
        meshes.extend(props.iter().cloned());
    }
    // LOOK_R2_LOOK_AT=ex,ey,ez,tx,ty,tz[,fov°]: an exact camera (evidence shots).
    if let Some(v) = std::env::var("LOOK_R2_LOOK_AT").ok().map(|s| {
        s.split(',')
            .filter_map(|x| x.trim().parse::<f32>().ok())
            .collect::<Vec<_>>()
    }) && v.len() >= 6
    {
        eye = Vec3::new(v[0], v[1], v[2]);
        target = Vec3::new(v[3], v[4], v[5]);
        if let Some(f) = v.get(6) {
            fov_y = f.to_radians();
        }
    }
    if let Some(v) = camera.look_at {
        eye = Vec3::new(v[0], v[1], v[2]);
        target = Vec3::new(v[3], v[4], v[5]);
        fov_y = v[6].to_radians();
    }
    if let Some(h) = hero {
        eye = Vec3::from_array(h.eye);
        target = Vec3::from_array(h.target);
        fov_y = h.fov_y;
        interior_scene = None;
        let base = materials().len() + do27_props.as_ref().map_or(0, |(_, m)| m.len());
        meshes.extend(h.meshes.iter().map(|m| LabMesh {
            material: base + m.material,
            ..m.clone()
        }));
    }

    // LOOK_R2_NO_MESHES=1: terrain alone (shadow-caster diagnostics).
    if std::env::var_os("LOOK_R2_NO_MESHES").is_some() {
        meshes.clear();
    }
    let settings = tier_settings();
    let mut post: PostSettings = settings.resolve_post();
    post.taa = false;
    // The tier's own metering (Mobile Low has none: the map's band centre is its fixed EV).
    post.auto_exposure = settings.profile.auto_exposure;
    // The runtime applies the map's look to every view the same way.
    if std::env::var_os("LOOK_R2_NO_LOOK").is_none() {
        regia_render::apply_level_look(&mut post, &level.look);
        if let Some(path) = level.look.lut.as_deref() {
            post.lut = Some(
                regia_render::ColorLut::load(std::path::Path::new(path))
                    .map_err(|e| LookdevError::Config(e.to_string()))?,
            );
        }
    }
    // Diagnostics for grade tuning: LOOK_R2_EV=<compensation, replaces the look bias>, LOOK_R2_WB=<kelvin>, LOOK_R2_TINT=<-1..1>.
    if let Some(ev) = std::env::var("LOOK_R2_EV").ok().and_then(|v| v.parse().ok()) {
        post.exposure_compensation = ev;
    }
    if let Some(ev) = std::env::var("LOOK_R2_EV100").ok().and_then(|v| v.parse().ok()) {
        post.auto_exposure_min_ev100 = ev;
        post.auto_exposure_max_ev100 = ev;
    }
    if std::env::var_os("LOOK_R2_NO_BLOOM").is_some() {
        post.bloom = false;
    }
    // LOOK_R2_LOCAL_EXPOSURE="shadow[,highlight[,detail]]": local exposure contrasts (a level
    // look's `local_exposure`), e.g. DO27's `0.75` shadows.
    if let Ok(v) = std::env::var("LOOK_R2_LOCAL_EXPOSURE") {
        let f: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if let Some(&shadow) = f.first() {
            post.local_exposure = Some(
                regia_render::LocalExposure {
                    shadow_contrast: shadow,
                    highlight_contrast: f.get(1).copied().unwrap_or(1.0),
                    detail_strength: f.get(2).copied().unwrap_or(1.0),
                }
                .sanitized(),
            );
        }
    }
    if let Some(c) = std::env::var("LOOK_R2_CONTRAST")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        post.contrast = c;
    }
    if let Some(s) = std::env::var("LOOK_R2_SATURATION")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        post.saturation = s;
    }
    if let Some(k) = std::env::var("LOOK_R2_WB").ok().and_then(|v| v.parse().ok()) {
        post.grade.white_balance_kelvin = k;
    }
    if let Some(t) = std::env::var("LOOK_R2_TINT").ok().and_then(|v| v.parse().ok()) {
        post.grade.tint = t;
    }
    // LOOK_R2_LIFT / LOOK_R2_GAMMA / LOOK_R2_GAIN="r,g,b": the grade's lift (shadow offset),
    // gamma (mid-tone power) and gain (highlight scale), for split-tone studies.
    let rgb = |name: &str| {
        std::env::var(name).ok().and_then(|v| {
            let c: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (c.len() == 3).then(|| [c[0], c[1], c[2]])
        })
    };
    if let Some(lift) = rgb("LOOK_R2_LIFT") {
        post.grade.lift = lift;
    }
    if let Some(gamma) = rgb("LOOK_R2_GAMMA") {
        post.grade.gamma = gamma;
    }
    if let Some(gain) = rgb("LOOK_R2_GAIN") {
        post.grade.gain = gain;
    }
    match std::env::var("LOOK_R2_TONEMAP").as_deref() {
        Ok("neutral") => post.tonemap = regia_render::TonemapOperator::Neutral,
        Ok("aces") => post.tonemap = regia_render::TonemapOperator::AcesFitted,
        _ => {}
    }
    if std::env::var_os("LOOK_R2_LDR").is_some() {
        post.enabled = false;
    }
    let meter_post = post.clone();
    let mut quality: FrameQuality = settings.quality();
    if std::env::var_os("LOOK_R2_NO_SSAO").is_some() {
        quality.ssao = regia_render::SsaoMode::Off;
    }
    if std::env::var_os("LOOK_R2_NO_SSGI").is_some() {
        quality.screen_space_gi = false;
    }
    // Screen-space refraction behind glass with a volume thickness (on in High/Ultra).
    if std::env::var_os("LOOK_R2_REFRACTION").is_some() {
        quality.screen_space_refraction = true;
    }
    // Desktop High and Ultra refract by default; LOOK_R2_NO_REFRACTION=1 measures without it.
    if std::env::var_os("LOOK_R2_NO_REFRACTION").is_some() {
        quality.screen_space_refraction = false;
    }
    // Diagnostics for host frame paths: LOOK_R2_MSAA=<samples>, LOOK_R2_LDR=1 (no post).
    if let Some(n) = std::env::var("LOOK_R2_MSAA").ok().and_then(|v| v.parse().ok()) {
        quality.msaa_samples = n;
    }
    // LOOK_R2_SHADOW_DEBUG=<n>: the meshes' shadow diagnostic (`RenderQuality::shadow_debug`).
    if let Some(n) = std::env::var("LOOK_R2_SHADOW_DEBUG")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        quality.shadow_debug = n;
    }
    // LOOK_R2_NO_PAGED_SHADOWS=1: whole-cascade sun shadows (no page cache), a diagnostic.
    if std::env::var_os("LOOK_R2_NO_PAGED_SHADOWS").is_some() {
        quality.paged_sun_shadows = false;
    }
    // LOOK_R2_NO_TRANSLUCENT_SHADOWS=1: blended glass casts no shadow (before/after and cost).
    if std::env::var_os("LOOK_R2_NO_TRANSLUCENT_SHADOWS").is_some() {
        quality.translucent_shadow_cascades = 0;
        quality.translucent_local_shadows = false;
    }
    // LOOK_R2_TRANSLUCENT_CASCADES=<n>: sun cascades with a translucency map (cost per cascade).
    if let Some(n) = std::env::var("LOOK_R2_TRANSLUCENT_CASCADES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        quality.translucent_shadow_cascades = n;
    }
    // Diagnostic: LOOK_R2_NO_IBL=1 draws the frame with `ibl: false` (a host that leaves it off).
    let mut ibl = std::env::var_os("LOOK_R2_NO_IBL").is_none();
    let mut ambient_lux = regia_render::Environment::default().ambient_lux;
    let local_lights: Vec<LabLocalLight> = hero.map_or_else(Vec::new, |h| h.lights.clone());
    let mut interiors = Vec::new();
    if shot == R2Shot::TunnelInterior && do27.is_none() {
        // The runtime's interior shade for an eye inside a marked volume (ADR-034):
        // the roofed bore, from the back wall to where the trusses start.
        let right = Vec3::new(-heading.z, 0.0, heading.x);
        let centre = eye - heading * 9.0 + right * 1.2 + heading * 5.0 + Vec3::Y * 1.6;
        let half = (right.abs() * 4.6 + heading.abs() * 9.0 + Vec3::Y * 3.5).to_array();
        let interior = regia_render::InteriorVolume::with_floor(centre.to_array(), half);
        interiors.push(interior);
        let shade = regia_render::shade_for_eye(eye.to_array(), &[interior], ambient_lux, ibl);
        quality.interior_ambient = shade.interior;
        ambient_lux = shade.ambient_lux;
        ibl = shade.ibl;
    }

    let mut renderer = FrameRenderer::new();
    // Diagnostic: LOOK_R2_AERIAL_INTERIORS=0 draws the aerial perspective as before ADR-4757 (the before plates of
    // the tunnel shot: the bore's air then takes the sky's in-scatter).
    if std::env::var_os("LOOK_R2_AERIAL_INTERIORS").is_some_and(|v| v == "0") {
        renderer.set_aerial_interiors(false);
    }
    // The lighting option: the preset's default (Depth, the GPU irradiance probes, on Desktop
    // High and Ultra), `LOOK_R2_PROBES=1` forces Depth, `LOOK_R2_NO_PROBES=1` forces Balanced
    // (the terrain's bounce and canyon-sky terms and the mesh ground bounce).
    if std::env::var_os("LOOK_R2_PROBES").is_some() {
        renderer.set_ddgi_enabled(Some(true));
    } else if std::env::var_os("LOOK_R2_NO_PROBES").is_some() {
        renderer.set_ddgi_enabled(Some(false));
    }
    // `LOOK_R2_DDGI=divisor=12,ray=1500,inst=32,cascades=3,dims=16x8x16,spacing=3`: probe settings for
    // cost studies.
    if let Ok(spec) = std::env::var("LOOK_R2_DDGI") {
        let mut s = renderer.ddgi_settings().clone();
        for kv in spec.split(',') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            // `dims=16x8x16`: probes per axis.
            if k.trim() == "dims" {
                let d: Vec<u32> = v.split('x').filter_map(|n| n.trim().parse().ok()).collect();
                if let [x, y, z] = d[..] {
                    s.dims = [x, y, z];
                }
                continue;
            }
            match (k.trim(), v.trim().parse::<f32>()) {
                ("divisor", Ok(v)) => s.update_divisor = v as u32,
                ("ray", Ok(v)) => s.max_terrain_ray = v,
                ("inst", Ok(v)) => s.max_instances_per_cascade = v as u32,
                ("cascades", Ok(v)) => s.cascades = v as u32,
                ("radius", Ok(v)) => s.min_proxy_radius = v,
                ("skylod", Ok(v)) => s.sky_lod = v,
                ("cone", Ok(v)) => s.hf_cone = v,
                ("mean", Ok(v)) => s.hf_mean_surface = v > 0.5,
                ("shadowcone", Ok(v)) => s.hf_shadow_cone = v,
                ("vbias", Ok(v)) => s.view_bias = v,
                ("nbias", Ok(v)) => s.normal_bias = v,
                ("exact", Ok(v)) => s.screen_exact_debug = v > 0.5,
                ("noshade", Ok(v)) => s.shading_off_debug = v > 0.5,
                ("inactive", Ok(v)) => s.inactive_slowdown = v as u32,
                ("spacing", Ok(v)) => s.spacing = v,
                ("ratio", Ok(v)) => s.spacing_ratio = v,
                ("emitters", Ok(v)) => s.emitters = v > 0.5,
                _ => eprintln!("LOOK_R2_DDGI: ignored {kv}"),
            }
        }
        renderer.set_ddgi_settings(s);
    }
    // `LOOK_R2_GI_LOOKUP=full|half|quarter`: the resolution of the probe GI screen lookup, and
    // `LOOK_R2_SSGI_HALF=1` (or `0`): SSGI gathered at half (or full) resolution whatever the
    // preset says. ADR-1033 measurements; unset keeps the presets' choices.
    if let Ok(v) = std::env::var("LOOK_R2_GI_LOOKUP") {
        match v.trim() {
            "full" => renderer.set_gi_lookup_resolution(regia_render::GiLookupResolution::Full),
            "half" => renderer.set_gi_lookup_resolution(regia_render::GiLookupResolution::Half),
            "quarter" => renderer.set_gi_lookup_resolution(regia_render::GiLookupResolution::Quarter),
            other => eprintln!("LOOK_R2_GI_LOOKUP: ignored {other}"),
        }
    }
    if let Ok(v) = std::env::var("LOOK_R2_SSGI_HALF") {
        renderer.set_ssgi_half_res(Some(v.trim() != "0"));
    }
    if let Some(fx) = fx.as_deref() {
        renderer.set_translucent_layers(vec![fx.layer()]);
    }
    let timing = options.timing || std::env::var_os("LOOK_R2_TIMING").is_some();
    if timing {
        renderer.set_gpu_timing(device, true);
    }
    let mut draw = TerrainSharedDraw::new(
        device,
        scene.map.clone(),
        &scene.layers,
        scene.cdlod.clone(),
        scene.root_level,
    )?;
    match scene.do27.as_deref() {
        // Diagnostic: LOOK_R2_NO_ORM=1 drops the layers' ORM maps (scalar roughness, no
        // texture occlusion) to measure their share of the indirect light.
        Some(content) if std::env::var_os("LOOK_R2_NO_ORM").is_some() => {
            let mut cooked = content.map.cooked.clone();
            cooked.layer_orm.clear();
            draw.set_material_cooked(&cooked, content.map.params, content.map.macro_tiles, &[])?;
        }
        Some(content) => draw.set_material_cooked(
            &content.map.cooked,
            content.map.params,
            content.map.macro_tiles,
            &[],
        )?,
        None => draw.set_material(&scene.material, &[])?,
    }
    // LOOK_R2_RELIEF=off|no-near|no-mid|no-strata|no-cliff|no-bounce|far-self-shadow: terrain relief A/B
    // (diagnostics).
    let relief = match std::env::var("LOOK_R2_RELIEF").as_deref() {
        Ok("off") => TerrainRelief::off(),
        Ok("no-near") => TerrainRelief {
            near_detail: 0.0,
            ..TerrainRelief::default()
        },
        Ok("no-mid") => TerrainRelief {
            mid_relief: 0.0,
            ..TerrainRelief::default()
        },
        Ok("no-strata") => TerrainRelief {
            strata_strength: 0.0,
            ..TerrainRelief::default()
        },
        Ok("no-cavity") => TerrainRelief {
            cavity: 0.0,
            ..TerrainRelief::default()
        },
        Ok("no-cliff") => TerrainRelief {
            cliff_relief: 0.0,
            ..TerrainRelief::default()
        },
        Ok("no-bounce") => TerrainRelief {
            bounce: 0.0,
            ..TerrainRelief::default()
        },
        Ok("far-self-shadow") => TerrainRelief {
            self_shadow_reach_m: 400.0,
            ..TerrainRelief::default()
        },
        _ => TerrainRelief::default(),
    };
    // LOOK_R2_LAYER_AO=<strength>: the layers' ORM occlusion strength (1 = as authored).
    // LOOK_R2_PREVIEW_DO27=1 previews DO27's announced content fixes: layer occlusion at 0.3
    // here and the Bore Twelve hangar at metallic 0 (`do27::load_gltf`).
    let mut relief = relief;
    if let Some(k) = std::env::var("LOOK_R2_LAYER_AO")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        relief.layer_occlusion = k;
    } else if std::env::var_os("LOOK_R2_PREVIEW_DO27").is_some() {
        relief.layer_occlusion = 0.3;
    }
    draw.set_relief(relief);
    // DO27's tunnel mouths are cut through the terrain (LOOK_R2_NO_HOLES=1: uncut, the
    // before of the terrain-holes evidence).
    if let Some(content) = scene.do27.as_deref()
        && std::env::var_os("LOOK_R2_NO_HOLES").is_none()
    {
        draw.set_holes(Some(&content.map.holes));
    }
    // LOOK_R2_FILL_CANYON_VIS=1: the fill on the ground takes canyon-scale and contact
    // occlusion only (`TerrainSharedDraw::set_fill_canyon_visibility`).
    draw.set_fill_canyon_visibility(std::env::var_os("LOOK_R2_FILL_CANYON_VIS").is_some());
    // `LOOK_R2_NO_TERRAIN_LIGHTS=1`: the ground ignores point and spot lights (the v0.1 look
    // and cost: the colour pass is built without the light loop; before / after captures).
    draw.set_local_lights_enabled(std::env::var_os("LOOK_R2_NO_TERRAIN_LIGHTS").is_none());
    // LOOK_R2_TERRAIN_DEBUG=<n>: the terrain's debug view (8 = sun light split).
    if let Some(view) = std::env::var("LOOK_R2_TERRAIN_DEBUG")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        draw.set_debug_view(view);
    }
    // Diagnostic: LOOK_R2_SKY_SCALE=<k> scales the terrain's sky light (ground bounce kept),
    // to measure how much of a shadowed floor is open-sky fill.
    if let Some(k) = std::env::var("LOOK_R2_SKY_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        let mut planet = regia_render::AtmosphereParameters::from_preset(level.atmosphere.preset);
        if let Some(lux) = level.atmosphere.sun_illuminance_lux {
            planet.sun_illuminance = lux;
        }
        let amb = regia_terrain::SkyAmbient::from_atmosphere(&planet, toward_sun.to_array());
        draw.set_sky_ambient(Some(regia_terrain::SkyAmbient {
            sky: amb.sky.map(|v| v * k),
            ground: amb.ground,
        }));
    }
    let mut contact = ContactMap::new(&device.device);
    draw.set_contact_map(Some(&contact));
    // The tracer's terrain albedo, from this draw's own shader, before its first frame; with
    // the probes on it is also the ground albedo their rays see (the albedo the frame shades
    // with, so the bounce off the ground and the tracer's agree).
    let probes_on = renderer.ddgi_enabled().unwrap_or(quality.gi_probes);
    let mut terrain_albedo = if hook.is_some() || probes_on {
        let bake = hook.as_ref().map(|h| h.albedo).unwrap_or_default();
        Some(
            regia_pt::terrain_bake::bake_terrain_albedo(&mut draw, device, &bake)
                .map_err(|e| LookdevError::Config(e.to_string()))?,
        )
    } else {
        None
    };
    let cpu_gi_albedo = std::env::var("LOOK_R2_GI_ALBEDO").as_deref() == Ok("cpu");
    if probes_on
        && !cpu_gi_albedo
        && let Some(t) = terrain_albedo.as_ref()
    {
        draw.set_gi_albedo_map(Some(&regia_terrain::Rgba8Image {
            width: t.width,
            height: t.height,
            data: t.rgba.to_vec(),
        }));
    }
    if hook.is_none() {
        terrain_albedo = None;
    }
    // The tracer's camera-ray terrain albedo: this draw's albedo rendered from the shot camera
    // (`regia_pt::PtViewAlbedo`), so the reference shades each terrain pixel with the albedo
    // the frame shades it with.
    let mut terrain_view_albedo = None;
    if hook.is_some() {
        let aspect = options.width as f32 / options.height.max(1) as f32;
        let far = std::env::var("LOOK_R2_FAR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12_000.0f32);
        let view_proj = (regia_core::glam::camera::rh::proj::directx::perspective(fov_y, aspect, 0.1, far)
            * regia_core::glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y))
        .to_cols_array_2d();
        let texture = |image: regia_terrain::Rgba8Image| regia_pt::PtTexture {
            width: image.width,
            height: image.height,
            rgba: std::sync::Arc::from(image.data),
            srgb: true,
        };
        let albedo = draw
            .render_albedo_view(device, view_proj, eye.to_array(), options.width, options.height)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        let normal = draw
            .render_normal_view(device, view_proj, eye.to_array(), options.width, options.height)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        terrain_view_albedo = Some(TerrainViewAlbedo {
            view_proj,
            eye: eye.to_array(),
            albedo: texture(albedo),
            normal: texture(normal),
        });
    }
    // `LOOK_R2_GI_ALBEDO=cpu`: the probes see the CPU reconstruction of the ground albedo (the
    // layer stack × macro albedo; rounds 1-2) instead of the shader bake.
    if cpu_gi_albedo
        && scene.do27.is_some()
        && let Some(map_dir) =
            crate::do27::content_dir().map(|d| d.join("maps").join(crate::do27::map_name(shot.planet())))
        && let Ok(albedo) = crate::pt_plates::terrain_albedo(&scene.map, &scene.layers, Some(&map_dir), 512)
    {
        draw.set_gi_albedo_map(Some(&regia_terrain::Rgba8Image {
            width: albedo.width,
            height: albedo.height,
            data: albedo.rgba.to_vec(),
        }));
    }
    // REGIA_IMPOSTORS=1: the scatter's octahedral impostor far field (ADR-116).
    let mut impostors = impostor_layer(
        device,
        &mut scene.scatter,
        &mut scene.impostor_atlas,
        options.height,
        fov_y,
    );
    draw.set_impostors(impostors.as_ref());
    // Far rocks sit on the drawn CDLOD surface (ADR-048).
    let surface = draw.surface();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut outdoor = OutdoorParams {
        fog: shot_fog(shot),
        ..OutdoorParams::default()
    };
    outdoor.interiors = interiors;
    outdoor.apply_level(&level.atmosphere, false);
    // Diagnostics (after the level's atmosphere so they are not overwritten).
    if std::env::var("LOOK_R2_NO_FOG").is_ok() {
        outdoor.fog.density = 0.0;
    }
    if std::env::var("LOOK_R2_NO_AERIAL").is_ok() {
        // Diagnostic: the same planet with no aerosol, so the aerial LUT is near clear.
        outdoor.planet.mie_scattering = [0.0; 3];
        outdoor.planet.mie_extinction = [1.0e-9; 3];
    }
    renderer.set_outdoor(outdoor);
    // The runtime passes the `EnclosureScene` shell the same way.
    renderer.set_interior_scene(interior_scene);

    let mut lab = LabScene {
        width: options.width,
        height: options.height,
        eye: eye.to_array(),
        target: target.to_array(),
        fov_y,
        near: 0.1,
        far: std::env::var("LOOK_R2_FAR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12_000.0),
        depth_prepass: settings.profile.depth_prepass,
        directional_shadows: true,
        ibl,
        exposure_ev: 14.0,
        ambient_lux,
        double_sided: false,
        post: Some(post),
        // The level clock overrides the direction and strength; DO27 maps author the light.
        directional: match do27_sun {
            Some(sun) => DirectionalLightUniform::new((-toward_sun).to_array(), sun.color, sun.lux),
            None => DirectionalLightUniform::new((-toward_sun).to_array(), [1.0, 0.95, 0.9], 50_000.0),
        },
        local_lights,
        materials: {
            let mut all = materials();
            if let Some((_, extra)) = &do27_props {
                all.extend(extra.iter().cloned());
            }
            if let Some(h) = hero {
                all.extend(h.materials.iter().cloned());
            }
            all
        },
        meshes,
    };
    if std::env::var_os("LOOK_R2_NO_MESH_OCCLUDERS").is_none() {
        renderer.set_mesh_occluders(hero.map_or_else(Vec::new, |h| h.occluders.clone()));
    }
    // `LOOK_R2_OCCLUDER_AB=<n>` (with timing): the hero's proxies on and off in blocks of `n`
    // frames (on, off, off, on, …) inside one loop, so a busy shared GPU drifts out of the
    // difference; the first 3 frames of a block are not timed (timestamps lag).
    let ab_block = std::env::var("LOOK_R2_OCCLUDER_AB")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|_| options.timing)
        .map(|n| n.max(4));
    let ab_occluders = hero.map_or_else(Vec::new, |h| h.occluders.clone());
    let mut ab_samples: [Vec<[f32; 3]>; 2] = [Vec::new(), Vec::new()];
    if let Some(h) = hero.filter(|h| !h.glass.is_empty()) {
        // Hero meshes are the last `h.meshes.len()` entries; the lab frame keys objects by slot
        // and geometry (`LabScene::object_key`; an address key no longer matches any draw).
        let base = lab.meshes.len() - h.meshes.len();
        let sets = regia_render::glass::GlassCrackSets::default();
        let instances = h
            .glass
            .iter()
            .filter_map(|(index, view, damage)| {
                let mesh = lab.meshes.get(base + index)?;
                regia_render::glass::glass_instance(
                    lab.object_key(base + index)?,
                    Some(view),
                    damage.as_ref(),
                    &sets,
                    regia_core::glam::Mat4::from_cols_array_2d(&mesh.model),
                )
            })
            .collect();
        renderer.set_glass(instances);
    }
    let view = ScatterView::look_at(
        lab.eye,
        lab.target,
        lab.fov_y,
        options.width as f32 / options.height.max(1) as f32,
        lab.near,
        lab.far,
    )
    .with_sun(toward_sun.to_array());
    let scatter_on = std::env::var_os("LOOK_R2_NO_SCATTER").is_none();
    if let Some(field) = scene.scatter.as_mut() {
        field.set_rendered_surface(Some(surface.clone()));
        field.set_enabled(scatter_on);
        if field.impostors().is_some() {
            // A still frame: the impostor far field completely placed.
            field.settle_streaming(lab.eye);
        } else {
            field.update(lab.eye);
        }
        contact.set_enabled(scatter_on);
        contact.update(&device.queue, field, lab.eye);
        if let Some(layer) = impostors.as_mut() {
            layer.update(&device.queue, field);
        }
    }
    let mut pixels = Vec::new();
    let mut samples: Vec<[f32; 16]> = Vec::new();
    let mut tail_samples: Vec<[f32; 2]> = Vec::new();
    let eye0 = lab.eye;
    let heater = if options.timing {
        crate::perf_heat::GpuHeater::from_env(&device.device)
    } else {
        None
    };
    let base_lights = lab.local_lights.clone();
    // `LOOK_R2_TIMING_BURST=N`: after the shot, N more frames of it back to back (see below).
    let burst: usize = if timing {
        std::env::var("LOOK_R2_TIMING_BURST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    for i in 0..options.frames.max(1) {
        if options.timing {
            lab.eye = [eye0[0] + i as f32 * 0.05, eye0[1], eye0[2] + i as f32 * 0.03];
        }
        let ab_on = ab_block.map(|n| matches!((i / n) % 4, 0 | 3));
        if let Some(on) = ab_on {
            renderer.set_mesh_occluders(if on { ab_occluders.clone() } else { Vec::new() });
        }
        if let Some(h) = heater.as_ref() {
            h.heat(&device.device, &device.queue);
        }
        if let Some(fx) = fx.as_deref_mut() {
            // On the ground under the look-at point (the DO27 FX cameras aim 2.5 m above it).
            let origin = Vec3::new(
                lab.target[0],
                scene.height_at(lab.target[0], lab.target[2]),
                lab.target[2],
            );
            let fire = fx.step(i, origin, Vec3::from_array(lab.eye));
            lab.local_lights = base_lights.iter().cloned().chain(fire).collect();
        }
        let mut frame = lab.as_frame_scene_with_quality(quality);
        if let Some(field) = scene.scatter.as_ref() {
            let mut rocks = Vec::new();
            field.collect(&view, &mut rocks);
            // The parity cameras stand where a player would: never inside a rock. Drop the
            // instances whose bounds reach the eye column (the plates have clear foregrounds).
            rocks.retain(|m| {
                let [c0, c1, c2, at] = m.model;
                let scale = Vec3::from_slice(&c0[..3])
                    .length()
                    .max(Vec3::from_slice(&c1[..3]).length())
                    .max(Vec3::from_slice(&c2[..3]).length());
                let d = Vec3::new(at[0] - lab.eye[0], 0.0, at[2] - lab.eye[2]).length();
                d > scale * 1.5 + 1.5
            });
            frame.meshes.extend(rocks);
        }
        let last = i + 1 == options.frames.max(1);
        if last && hook.is_some() {
            renderer.request_hdr_capture();
        }
        pixels = renderer.render_offscreen(device, &frame)?.pixels;
        if burst > 0 && last {
            timing_burst(device, &mut renderer, &frame, burst, shot.name())?;
        }
        if last
            && std::env::var_os("LOOK_R2_PROBE_DEBUG").is_some()
            && let Ok(Some(rb)) = renderer.ddgi_read_back(device)
        {
            let per = (rb.dims[0] * rb.dims[1] * rb.dims[2]) as usize;
            let stamps: Vec<u32> = (0..per * rb.cascades as usize)
                .map(|i| rb.data[i * 12 + 8])
                .collect();
            let newest = stamps.iter().copied().max().unwrap_or(0);
            let young = (0..per * rb.cascades as usize)
                .filter(|&i| (rb.data[i * 12 + 7] as i32) < 8)
                .count();
            eprintln!(
                "probes traced on the last frame: {} of {} (young {young})",
                stamps.iter().filter(|&&s| s == newest).count(),
                stamps.len()
            );
            for c in 0..rb.cascades as usize {
                let (mut active, mut traced, mut misses, mut backs, mut rays) = (0, 0, 0, 0, 0);
                let (mut miss_l, mut hit_l, mut irr_up) = (0.0f64, 0.0f64, 0.0f64);
                for slot in c * per..(c + 1) * per {
                    let (off, tag) = rb.probe(slot as u32);
                    if tag[3] < 1 {
                        continue;
                    }
                    traced += 1;
                    if off[3] > 0.5 {
                        active += 1;
                        irr_up += f64::from(
                            rb.irradiance(slot as u32, 2, 5)[1] + rb.irradiance(slot as u32, 3, 5)[1],
                        ) * 0.5;
                    }
                    for r in &rb.rays[slot * 64..slot * 64 + 64] {
                        rays += 1;
                        if r[3] > 1e8 {
                            misses += 1;
                            miss_l += f64::from(r[1]);
                        } else if r[3] < 0.0 {
                            backs += 1;
                        } else {
                            hit_l += f64::from(r[1]);
                        }
                    }
                }
                eprintln!(
                    "probes c{c}: traced {traced} active {active} irr_up(g) {:.1} rays: miss {:.2} (L {:.1}) back {:.2} hit L {:.1} instances {}",
                    irr_up / f64::from(active.max(1)),
                    misses as f64 / f64::from(rays.max(1)),
                    miss_l / f64::from(misses.max(1)),
                    backs as f64 / f64::from(rays.max(1)),
                    hit_l / f64::from((rays - misses - backs).max(1)),
                    rb.lists[c]
                );
            }
            eprintln!(
                "hemisphere ambient [sky, ground] E/pi: {:?}",
                renderer.debug_hemisphere_ambient()
            );
        }
        if last
            && let Some(hook) = hook.as_mut()
            && let Some(terrain_albedo) = terrain_albedo.take()
        {
            let missing = || LookdevError::Config("the frame ran no post stack: no HDR to capture".into());
            let hdr_full = renderer.take_hdr_capture(device)?.ok_or_else(missing)?;
            let probe_screen = if renderer.ddgi_active() {
                renderer
                    .ddgi_read_back(device)?
                    .and_then(|rb| rb.screen_irradiance())
            } else {
                None
            };
            let mut direct = frame.clone();
            direct.ibl = false;
            direct.ambient_lux = 0.0;
            direct.quality.gi = regia_render::GiMode::Ambient;
            direct.quality.screen_space_gi = false;
            direct.quality.ssr = false;
            direct.quality.reflection_probes = false;
            direct.quality.ssao = regia_render::SsaoMode::Off;
            direct.quality.interior_ambient = false;
            // The probes are indirect light too (their sun bounce survives a zero ambient).
            let probes = renderer.ddgi_enabled();
            // `LOOK_R2_DIRECT_KEEP_PROBES=1` reproduces the old capture (probes left on in the
            // direct-only frame, so full − direct lost their sun bounce).
            if std::env::var_os("LOOK_R2_DIRECT_KEEP_PROBES").is_none() {
                renderer.set_ddgi_enabled(Some(false));
            }
            renderer.request_hdr_capture();
            renderer.render_offscreen(device, &direct)?;
            renderer.set_ddgi_enabled(probes);
            let hdr_direct = renderer.take_hdr_capture(device)?.ok_or_else(missing)?;
            let map_dir = do27
                .is_some()
                .then(|| {
                    crate::do27::content_dir()
                        .map(|d| d.join("maps").join(crate::do27::map_name(shot.planet())))
                })
                .flatten();
            (hook.run)(ShotCapture {
                frame: &frame,
                hdr_full,
                hdr_direct,
                planet: renderer.outdoor_params().planet,
                toward_sun: toward_sun.to_array(),
                map: &scene.map,
                layers: &scene.layers,
                map_dir,
                probe_screen,
                terrain_albedo,
                terrain_view_albedo: terrain_view_albedo.take(),
                outdoor: renderer.outdoor_params().clone(),
            })?;
        }
        if i == 0 && std::env::var_os("LOOK_R2_CHECKLIST").is_some() {
            // The harness's own host checklist (diff a game shell's report against it).
            println!("{}", renderer.host_checklist_report(&frame));
        }
        if timing {
            let t = renderer.gpu_timings();
            samples.push([
                t.shadows_ms,
                t.prepass_ms,
                t.ssao_ms,
                t.forward_ms,
                t.scene_ms,
                t.gi_ms,
                t.ssgi_ms,
                t.gi_stages_ms[0],
                t.gi_stages_ms[1],
                t.gi_stages_ms[2],
                t.gi_stages_ms[3],
                t.gi_stages_ms[4],
                t.ssr_ms,
                t.sssr_trace_ms,
                t.sssr_denoise_ms,
                t.sssr_classify_ms,
            ]);
            tail_samples.push([t.post_ms, t.frame_ms]);
            if let (Some(on), Some(n)) = (ab_on, ab_block)
                && i % n >= 3
            {
                ab_samples[usize::from(on)].push([t.forward_ms, t.shared_forward_ms, t.scene_ms]);
            }
        }
    }
    if timing && let Some(stats) = renderer.sssr_stats() {
        println!(
            "  sssr {}: {} quads traced, {} glossy tiles",
            shot.name(),
            stats.ray_quads,
            stats.tiles
        );
    }
    if ab_samples.iter().all(|s| !s.is_empty()) {
        let median = |s: &[[f32; 3]], k: usize| {
            let mut v: Vec<f32> = s.iter().map(|x| x[k]).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        let [off, on] = &ab_samples;
        println!(
            "  gpu ms ab {} ({} off / {} on frames): forward {:+.3} terrain {:+.3} scene {:+.3} (off forward {:.3})",
            shot.name(),
            off.len(),
            on.len(),
            median(on, 0) - median(off, 0),
            median(on, 1) - median(off, 1),
            median(on, 2) - median(off, 2),
            median(off, 0)
        );
    }
    if timing && tail_samples.len() >= 2 {
        // The last half of the frames: a growing effect (smoke, an explosion) at its full
        // size. `post` holds the translucent layers (VFX) and the post stack.
        let tail = &tail_samples[tail_samples.len() / 2..];
        let median = |i: usize| {
            let mut v: Vec<f32> = tail.iter().map(|s| s[i]).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "  gpu ms tail {} (median of last {}): post {:.3} frame {:.3}",
            shot.name(),
            tail.len(),
            median(0),
            median(1)
        );
    }
    if timing && !samples.is_empty() {
        // Median per pass over the frames after the first few (pipeline and cache warm-up).
        let skip = (samples.len() / 4).min(4);
        let median = |i: usize| {
            let mut v: Vec<f32> = samples[skip..].iter().map(|s| s[i]).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "  gpu ms {} (median of {}): shadows {:.3} prepass {:.3} ssao {:.3} forward {:.3} scene {:.3} gi probes {:.3} ssgi {:.3} | gi trace {:.3} relocate {:.3} blend irr {:.3} vis {:.3} screen {:.3} | ssr {:.3} \
             (sssr trace {:.3} denoise {:.3}, classify {:.3} of the trace) | emitters {}",
            shot.name(),
            samples.len() - skip,
            median(0),
            median(1),
            median(2),
            median(3),
            median(4),
            median(5),
            median(6),
            median(7),
            median(8),
            median(9),
            median(10),
            median(11),
            median(12),
            median(13),
            median(14),
            median(15),
            renderer.ddgi_emitter_count()
        );
        let report = renderer.shadow_cache_report();
        println!(
            "  translucent shadows {}: {} casters, {} passes, {} draws",
            shot.name(),
            report.translucent_casters,
            report.translucent_passes,
            report.translucent_draws
        );
    }
    if std::env::var_os("LOOK_R2_METER").is_some() {
        let n = (pixels.len() / 4).max(1) as f32;
        let (mut clipped, mut sum) = (0usize, 0.0f32);
        for px in pixels.as_chunks::<4>().0 {
            let y = luma(px);
            sum += y;
            if px[0] >= 250 && px[1] >= 250 && px[2] >= 250 {
                clipped += 1;
            }
        }
        match renderer.metered_luminance() {
            Some(lum) => println!(
                "  meter {}: scene luminance {lum:.1} cd/m², EV100 {:.2} (unclamped {:.2}), clipped {:.1} %, mean luma {:.1}, bright coverage {:.2}",
                shot.name(),
                meter_post.auto_exposure_ev100_with_coverage(lum, renderer.metered_bright_coverage()),
                regia_render::ev100_from_luminance(lum),
                100.0 * clipped as f32 / n,
                sum / n,
                renderer.metered_bright_coverage()
            ),
            None => println!("  meter {}: no histogram sample yet", shot.name()),
        }
    }
    if do27_sun.is_some() {
        // The sim frame is left-handed: reflect once, at the image (see `crate::do27`).
        crate::do27::mirror_columns(&mut pixels, options.width);
    }
    Ok((
        pixels,
        R2Frame {
            eye: eye.to_array(),
            target: target.to_array(),
            toward_sun: toward_sun.to_array(),
        },
    ))
}

/// The scatter impostor far field for a harness frame, opt-in with `REGIA_IMPOSTORS=1`
/// (ADR-116). Turns the field's far field on (`REGIA_IMPOSTORS_FAR=<m>`,
/// `REGIA_IMPOSTORS_CELL=<px>`, `REGIA_IMPOSTORS_GRID=<n>` override the defaults; the
/// per-class far range is planned for `height_px` and `fov_y`), bakes the cards once per
/// terrain and returns the layer to bind to the terrain draw
/// ([`TerrainSharedDraw::set_impostors`]) and update after each [`ScatterField::update`].
/// Unset, it turns the far field off and returns `None` (the frame is unchanged).
pub fn impostor_layer(
    device: &RenderDevice,
    scatter: &mut Option<ScatterField>,
    cache: &mut Option<regia_terrain::ImpostorAtlas>,
    height_px: u32,
    fov_y: f32,
) -> Option<regia_terrain::ImpostorLayer> {
    let field = scatter.as_mut()?;
    if std::env::var_os("REGIA_IMPOSTORS").is_none_or(|v| v == "0") {
        field.set_impostors(None);
        return None;
    }
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f32>().ok());
    let defaults = regia_terrain::ImpostorSettings::default();
    let settings = regia_terrain::ImpostorSettings {
        far_end_m: var("REGIA_IMPOSTORS_FAR").unwrap_or(defaults.far_end_m),
        cell: var("REGIA_IMPOSTORS_CELL").map_or(defaults.cell, |v| v as u32),
        grid: var("REGIA_IMPOSTORS_GRID").map_or(defaults.grid, |v| v as u32),
        reference_height_px: height_px as f32,
        reference_fov_y: fov_y,
        ..defaults
    };
    field.set_impostors(Some(settings));
    let rebake = cache
        .as_ref()
        .is_none_or(|a| a.settings.grid != settings.grid || a.settings.cell != settings.cell);
    if rebake {
        let started = std::time::Instant::now();
        let atlas = field.bake_impostors(None);
        eprintln!(
            "impostors: baked {} cards for {} prototypes ({:.1} MB) in {:.2} s",
            atlas.cards.len(),
            atlas.protos.len(),
            atlas.bytes() as f64 / 1.0e6,
            started.elapsed().as_secs_f32()
        );
        *cache = Some(atlas);
    }
    let atlas = cache.as_ref()?;
    Some(regia_terrain::ImpostorLayer::new(
        &device.device,
        &device.queue,
        atlas,
    ))
}

/// Rec.709 luma (0–255) of one pixel.
#[must_use]
pub fn luma(px: &[u8]) -> f32 {
    0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2])
}

/// Mean luma of a rect given in fractions of the frame (`x0, y0, x1, y1`).
#[must_use]
pub fn region_luma(pixels: &[u8], width: u32, height: u32, rect: [f32; 4]) -> f32 {
    let (x0, x1) = ((rect[0] * width as f32) as u32, (rect[2] * width as f32) as u32);
    let (y0, y1) = ((rect[1] * height as f32) as u32, (rect[3] * height as f32) as u32);
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let i = ((y * width + x) * 4) as usize;
            if let Some(px) = pixels.get(i..i + 4) {
                sum += luma(px);
                n += 1.0;
            }
        }
    }
    if n > 0.0 { sum / n } else { 0.0 }
}

/// Luma percentile (0–100) over a rect.
#[must_use]
pub fn region_percentile(pixels: &[u8], width: u32, height: u32, rect: [f32; 4], pct: f32) -> f32 {
    let (x0, x1) = ((rect[0] * width as f32) as u32, (rect[2] * width as f32) as u32);
    let (y0, y1) = ((rect[1] * height as f32) as u32, (rect[3] * height as f32) as u32);
    let mut values = Vec::new();
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let i = ((y * width + x) * 4) as usize;
            if let Some(px) = pixels.get(i..i + 4) {
                values.push(luma(px));
            }
        }
    }
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let k = ((pct / 100.0) * (values.len() - 1) as f32).round() as usize;
    values[k.min(values.len() - 1)]
}

/// `LOOK_R2_TIMING_BURST`: renders `frame` `burst` more times back to back and prints the GPU
/// medians of the last three quarters.
fn timing_burst(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    frame: &regia_render::FrameScene<'_>,
    burst: usize,
    name: &str,
) -> Result<(), LookdevError> {
    let mut frame = frame.clone();
    // Each shot frame above waits for its pixels and rebuilds the scene on the CPU, so the
    // GPU idles between frames and its clocks drop (an RTX 5090 ran the X12 prepass in 3.1 ms
    // that way and 0.24 ms busy, sssr-budget lane). The burst renders the last frame again
    // with no readback and three frames in flight, the eye creeping so no held-view reuse
    // applies, and reports the medians of its last three quarters.
    let (width, height) = (frame.width.max(1), frame.height.max(1));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("look_r2 burst target"),
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
    let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let eye = frame.eye;
    let mut in_flight = std::collections::VecDeque::new();
    let mut burst_samples: Vec<[f32; 8]> = Vec::new();
    for k in 0..burst {
        frame.eye = [eye[0] + k as f32 * 0.002, eye[1], eye[2] + k as f32 * 0.001];
        renderer
            .render_into(
                device,
                &frame,
                regia_render::FrameTarget::full(&target, format, width, height),
            )
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        if let Some(submission) = renderer.take_submission() {
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
        if k >= burst / 4 {
            let t = renderer.gpu_timings();
            burst_samples.push([
                t.scene_ms,
                t.ssr_ms,
                t.sssr_classify_ms,
                t.sssr_trace_ms,
                t.sssr_denoise_ms,
                t.frame_ms,
                t.shared_forward_ms,
                t.post_ms,
            ]);
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    if !burst_samples.is_empty() {
        let median = |i: usize| {
            let mut v: Vec<f32> = burst_samples.iter().map(|s| s[i]).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "  gpu ms burst {} (median of {}): scene {:.3} ssr {:.3} (sssr classify {:.3} trace {:.3} \
             denoise {:.3}) frame {:.3} terrain-forward {:.3} post {:.3}",
            name,
            burst_samples.len(),
            median(0),
            median(1),
            median(2),
            median(3),
            median(4),
            median(5),
            median(6),
            median(7)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nasa_mars_grade_is_less_saturated_and_balanced_for_a_warmer_light_than_the_preset() {
        let ue = switchbacks_look();
        let mut nasa = switchbacks_look();
        switchbacks_nasa_grade(&mut nasa);
        assert!(nasa.saturation.unwrap_or(1.0) < ue.saturation.unwrap_or(1.0));
        let (ue_k, nasa_k) = (
            ue.grade.map_or(6500.0, |g| g.white_balance_kelvin),
            nasa.grade.map_or(6500.0, |g| g.white_balance_kelvin),
        );
        // Balancing for a warmer (lower-kelvin) light removes the rust cast from the image.
        assert!(nasa_k < ue_k, "{nasa_k} vs {ue_k}");
        // The lift only trims the shade's red cast: small, and no red.
        let lift = nasa.grade.map_or([0.0; 3], |g| g.lift);
        assert!(lift.iter().all(|c| (0.0..0.05).contains(c)) && lift[0] == 0.0);
    }
}
