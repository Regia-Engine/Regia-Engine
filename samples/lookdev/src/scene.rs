// SPDX-License-Identifier: MIT-0
//! Procedural Europa and Mars stand-ins in the DO27 terrain data shape.
//!
//! Each scene is a 16-bit heightfield ([`regia_asset::Heightfield`] decoded through
//! [`regia_terrain::TerrainMap`]), six [`regia_terrain::LayerDesc`] layers plus a
//! height map each, a macro-variation image, a road ribbon cut to a smoothed profile of
//! the terrain, a [`regia_terrain::MeshTunnel`] portal, and seeded scatter.

use regia_asset::Heightfield;
use regia_render::FogSettings;
use regia_terrain::{
    CdlodConfig, CollisionProvenance, HoleEllipse, HolesMask, LayerDesc, MeshTunnel, MinMaxPyramid,
    ScatterInstance, ScatterRule, TerrainLayers, TerrainMap, TunnelDetail, place,
};

use crate::erosion::{hydraulic_erode, thermal_erode};
use crate::{LookdevConfig, LookdevError, Planet};

/// One authored layer height map (0..=1). The terrain GPU on main blends by slope and
/// altitude; the samples stay with the scene until a lane samples them.
#[derive(Clone, Debug)]
pub struct LayerHeightMap {
    /// Layer name, matching [`TerrainLayers`].
    pub name: String,
    /// Edge length in samples.
    pub resolution: u32,
    /// Row-major heights, length `resolution²`, in `0..=1`.
    pub samples: Vec<f32>,
}

/// Low-frequency albedo variation across the whole island. Edge length is configurable;
/// 8192 is the 8K target.
#[derive(Clone, Debug)]
pub struct MacroMap {
    /// Edge length in samples.
    pub resolution: u32,
    /// Row-major scalar variation, length `resolution²`, in `0..=1`.
    pub samples: Vec<f32>,
}

/// The road centerline the cameras and the height carve share.
#[derive(Clone, Debug)]
pub struct RoadPath {
    /// World XZ samples, about one every 12 m.
    pub samples: Vec<[f32; 2]>,
    /// A bend the road-bend camera sits on.
    pub bend: [f32; 2],
    /// Unit XZ tangent at [`Self::bend`], pointing toward the portal.
    pub bend_tangent: [f32; 2],
    /// Portal end of the road.
    pub end: [f32; 2],
    /// Unit XZ tangent at the portal, pointing into the hill.
    pub end_tangent: [f32; 2],
}

/// Fixed XZ landmarks. Cameras add the eye height on top.
#[derive(Clone, Debug)]
pub struct Landmarks {
    /// Close and mid shots, on the road.
    pub close: [f32; 2],
    /// Eye XZ for the 2 km vista (the target is [`Self::vista_target`]).
    pub vista_eye: [f32; 2],
    /// Look target of the vista, 2 km from the eye on a 4096 m world.
    pub vista_target: [f32; 2],
    /// Low-sun camera XZ.
    pub low_sun: [f32; 2],
    /// Sky-heavy camera XZ.
    pub sky: [f32; 2],
}

/// Lighting shared by the shots that do not override the sun.
#[derive(Clone, Copy, Debug)]
pub struct Climate {
    /// Exponential height-fog colour (linear, mixed by the terrain shader).
    pub fog_color: [f32; 3],
    /// Fog density at the base height, 1/metres.
    pub fog_density: f32,
    /// Fog height falloff, 1/metres.
    pub fog_falloff: f32,
    /// Global wetness `0..=1` (ice gloss on Europa, dry on Mars).
    pub wetness: f32,
    /// Sky ambient, linear, pre-exposed the way the terrain shader expects.
    pub ambient: [f32; 3],
    /// Default sun azimuth, radians.
    pub sun_azimuth: f32,
    /// Default sun elevation, radians.
    pub sun_elevation: f32,
    /// Default sun colour, linear.
    pub sun_color: [f32; 3],
    /// Default sun illuminance, lux.
    pub sun_lux: f32,
    /// World Y where [`Self::fog_density`] applies, metres.
    pub base_height: f32,
}

/// One planet, ready to draw.
pub struct Scene {
    /// Europa or Mars.
    pub planet: Planet,
    /// 16-bit heightfield decoded to metres.
    pub map: TerrainMap,
    /// CDLOD min/max pyramid for [`regia_terrain::select_nodes`].
    pub pyramid: MinMaxPyramid,
    /// Quadtree depth used to build [`Self::pyramid`].
    pub root_level: u32,
    /// Grid the GPU was (or will be) built with. Profiles only change `range_factor`.
    pub cdlod: CdlodConfig,
    /// Six splat layers.
    pub layers: TerrainLayers,
    /// One height map per layer, same order as [`Self::layers`].
    pub layer_height_maps: Vec<LayerHeightMap>,
    /// Island-scale variation.
    pub macro_variation: MacroMap,
    /// Tunnel mouth punched out of the heightfield.
    pub holes: HolesMask,
    /// Collision-derived portal mesh. The harness draws [`MeshTunnel::render_mesh`]
    /// into the hole. Scatter is still the terrain hook's pyramid until a rock mesh exists.
    pub tunnel: MeshTunnel,
    /// Rocks, portal ring included, before the per-profile cap.
    pub scatter: Vec<ScatterInstance>,
    /// Road centerline.
    pub road: RoadPath,
    /// Camera anchors.
    pub landmarks: Landmarks,
    /// Planet lighting.
    pub climate: Climate,
}

impl Scene {
    /// Surface height in metres at a world XZ position.
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        self.map.sample_bilinear(x, z)
    }
}

/// Builds one stand-in. `config.world_width_m` scales the road and the vista; the
/// heightfield edge is `config.height_resolution`.
///
/// # Errors
///
/// [`LookdevError`] when the config, the quantised heightfield, the layers, or the
/// tunnel mesh is rejected.
#[tracing::instrument(skip(config), fields(planet = planet.name()))]
pub fn generate(planet: Planet, config: &LookdevConfig) -> Result<Scene, LookdevError> {
    config.validate()?;
    let res = config.height_resolution;
    tracing::info!(
        target: "lookdev",
        resolution = res,
        world_m = config.world_width_m,
        "generating stand-in heightfield"
    );
    let mut heights = base_heights(planet, res, config.world_width_m);
    let spacing = config.world_width_m / (res - 1) as f32;
    let talus = spacing * planet_talus(planet);
    thermal_erode(&mut heights, res, res, talus, 0.45, config.thermal_iterations);
    hydraulic_erode(
        &mut heights,
        res,
        res,
        config.hydraulic_drops,
        28,
        1.2,
        planet.seed(),
    );
    let road = build_road(planet, &heights, res, config.world_width_m);
    // Cut first, then raise the hillside around the mouth so the trench is not filled back in.
    carve_road(
        &mut heights,
        res,
        config.world_width_m,
        &road.path.samples,
        &road.center_heights,
        RoadCut::lookdev(),
    );
    raise_portal_hill(
        &mut heights,
        res,
        config.world_width_m,
        road.path.end,
        road.path.end_tangent,
    );
    let map = quantize(&heights, res, res, config.world_width_m)?;
    let edges = altitude_edges(map.heights());
    let layers = planet_layers(planet, edges)?;
    let layer_height_maps = layer_heights(planet, &layers, config.layer_height_resolution);
    let macro_variation = macro_map(planet, config.macro_resolution);
    let mouth_h = map.sample_bilinear(road.path.end[0], road.path.end[1]);
    let (holes, tunnel) = portal(planet, &road, mouth_h)?;
    let root_level = root_level_for(res);
    let pyramid = MinMaxPyramid::build(&map, root_level).map_err(regia_terrain::TerrainError::from)?;
    let scatter = scatter_rocks(planet, &map, &layers, &holes, &road, config.scatter_cap.max(8));
    let landmarks = landmarks_from(&road, config.world_width_m);
    let climate = climate_for(planet, mean_height(map.heights()));
    Ok(Scene {
        planet,
        map,
        pyramid,
        root_level,
        cdlod: CdlodConfig::default(),
        layers,
        layer_height_maps,
        macro_variation,
        holes,
        tunnel,
        scatter,
        road: road.path,
        landmarks,
        climate,
    })
}

/// Quadtree depth whose finest nodes still cover at least four samples.
#[must_use]
pub fn root_level_for(resolution: u32) -> u32 {
    let mut root = 1_u32;
    while root < 10 && (1_u32 << (root + 1)).saturating_mul(4) <= resolution {
        root += 1;
    }
    root
}

fn planet_talus(planet: Planet) -> f32 {
    // tan of the repose angle: ice slumps sooner than sandstone.
    match planet {
        Planet::Europa => 0.36,
        Planet::Mars => 0.70,
    }
}

fn fbm(x: f32, y: f32, seed: u32, octaves: u32) -> f32 {
    let mut amp = 1.0_f32;
    let mut freq = 1.0_f32;
    let mut sum = 0.0_f32;
    let mut norm = 0.0_f32;
    for octave in 0..octaves {
        sum += amp
            * regia_terrain::noise::value_noise2(
                x * freq,
                y * freq,
                seed.wrapping_add(octave.wrapping_mul(0x9E37)),
            );
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    if norm > 0.0 { sum / norm } else { 0.0 }
}

fn base_heights(planet: Planet, res: u32, _world: f32) -> Vec<f32> {
    let seed = planet.seed();
    let mut heights = Vec::with_capacity((res * res) as usize);
    for row in 0..res {
        let v = row as f32 / (res - 1) as f32;
        for col in 0..res {
            let u = col as f32 / (res - 1) as f32;
            heights.push(planet_height(planet, u, v, seed));
        }
    }
    heights
}

fn planet_height(planet: Planet, u: f32, v: f32, seed: u32) -> f32 {
    match planet {
        Planet::Europa => {
            let plains = fbm(u * 3.2, v * 3.2, seed, 5);
            let ridge_n = fbm(u * 5.5, v * 5.5, seed ^ 0x51C0, 4);
            let ridged = (1.0 - (ridge_n * 2.0 - 1.0).abs()).powi(2);
            let chaos = fbm(u * 2.1, v * 1.4, seed ^ 0x0E11, 3);
            plains * 55.0 + ridged * 150.0 * (0.30 + 0.70 * chaos) + fbm(u * 0.6, v * 0.6, seed ^ 9, 2) * 30.0
        }
        Planet::Mars => {
            let plateau = fbm(u * 2.4, v * 2.4, seed, 5) * 210.0;
            let canyon_v = 0.58 + 0.10 * (u * std::f32::consts::TAU).sin();
            let dist = (v - canyon_v).abs();
            let canyon = (1.0 - (dist / 0.11).clamp(0.0, 1.0)).powi(2) * 130.0;
            let mut h = plateau - canyon;
            // Soft sedimentary staircase: pull toward the nearest bed, keep a lip.
            let step = 18.0;
            let nearest = (h / step).round() * step;
            h += (nearest - h) * 0.62;
            if h < 90.0 {
                h += (u * 46.0).sin() * (v * 22.0).cos() * 2.4;
            }
            h
        }
    }
}

struct BuiltRoad {
    path: RoadPath,
    center_heights: Vec<f32>,
}

fn build_road(planet: Planet, heights: &[f32], res: u32, world: f32) -> BuiltRoad {
    let scale = world / 4096.0;
    let controls: [[f32; 2]; 4] = match planet {
        Planet::Europa => [
            [-1500.0, -280.0],
            [-500.0, -80.0],
            [280.0, 360.0],
            [1280.0, 140.0],
        ],
        Planet::Mars => [
            [-1600.0, 520.0],
            [-420.0, 180.0],
            [360.0, -460.0],
            [1320.0, -180.0],
        ],
    };
    let controls = controls.map(|p| [p[0] * scale, p[1] * scale]);
    let mut samples = Vec::new();
    let step = 12.0_f32;
    for seg in 0..controls.len() - 1 {
        let a = controls[seg];
        let b = controls[seg + 1];
        let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(step);
        let n = (len / step).ceil() as u32;
        for i in 0..n {
            let t = i as f32 / n as f32;
            samples.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
        }
    }
    samples.push(*controls.last().unwrap_or(&controls[0]));
    let raw: Vec<f32> = samples
        .iter()
        .map(|p| sample_height(heights, res, world, p[0], p[1]))
        .collect();
    let center_heights = smooth_heights(&raw, 4);
    let bend_i = samples.len() / 2;
    let end_i = samples.len() - 1;
    let bend_tangent = tangent_at(&samples, bend_i);
    let end_tangent = tangent_at(&samples, end_i);
    BuiltRoad {
        path: RoadPath {
            bend: samples[bend_i],
            bend_tangent,
            end: samples[end_i],
            end_tangent,
            samples,
        },
        center_heights,
    }
}

fn tangent_at(samples: &[[f32; 2]], index: usize) -> [f32; 2] {
    let a = samples[index.saturating_sub(1)];
    let b = samples[(index + 1).min(samples.len() - 1)];
    let dx = b[0] - a[0];
    let dz = b[1] - a[1];
    let len = dx.hypot(dz).max(1.0e-4);
    [dx / len, dz / len]
}

fn smooth_heights(values: &[f32], radius: usize) -> Vec<f32> {
    if values.is_empty() {
        return Vec::new();
    }
    values
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius).min(values.len() - 1);
            let mut sum = 0.0_f32;
            let mut n = 0.0_f32;
            for value in &values[lo..=hi] {
                sum += value;
                n += 1.0;
            }
            sum / n
        })
        .collect()
}

fn sample_height(heights: &[f32], res: u32, world: f32, x: f32, z: f32) -> f32 {
    let half = world * 0.5;
    let spacing = world / (res - 1) as f32;
    let u = ((x + half) / spacing).clamp(0.0, (res - 1) as f32);
    let v = ((z + half) / spacing).clamp(0.0, (res - 1) as f32);
    let x0 = u.floor() as u32;
    let z0 = v.floor() as u32;
    let x1 = (x0 + 1).min(res - 1);
    let z1 = (z0 + 1).min(res - 1);
    let tx = u - x0 as f32;
    let tz = v - z0 as f32;
    let at = |col: u32, row: u32| heights[(row * res + col) as usize];
    let a = at(x0, z0) + (at(x1, z0) - at(x0, z0)) * tx;
    let b = at(x0, z1) + (at(x1, z1) - at(x0, z1)) * tx;
    a + (b - a) * tz
}

/// Carriageway cut. The shoulder is steep on purpose: slope layers (basalt, bedrock)
/// pick up the wall, so the trench reads against the flat floor.
pub(crate) struct RoadCut {
    half_width: f32,
    shoulder: f32,
    depth: f32,
    lip_width: f32,
    lip_height: f32,
}

impl RoadCut {
    /// Look-dev ribbon: 16 m of flat road, a 6 m cut, a short steep shoulder, a lit lip.
    fn lookdev() -> Self {
        Self {
            half_width: 8.0,
            shoulder: 2.8,
            depth: 6.0,
            lip_width: 3.2,
            lip_height: 2.4,
        }
    }
}

/// Hillside beyond the road end, with the mouth footprint left at the carved grade.
/// The hole mask then removes that footprint so the portal mesh shows through.
fn raise_portal_hill(heights: &mut [f32], res: u32, world: f32, end: [f32; 2], tangent: [f32; 2]) {
    let len = tangent[0].hypot(tangent[1]).max(1.0e-4);
    let tx = tangent[0] / len;
    let tz = tangent[1] / len;
    let px = -tz;
    let pz = tx;
    let half = world * 0.5;
    let spacing = world / (res - 1) as f32;
    let reach = 48.0_f32;
    let c0 = world_to_cell(end[0] - reach, half, spacing, res);
    let c1 = world_to_cell(end[0] + reach, half, spacing, res);
    let r0 = world_to_cell(end[1] - reach, half, spacing, res);
    let r1 = world_to_cell(end[1] + reach, half, spacing, res);
    let (c0, c1) = (c0.min(c1), c0.max(c1));
    let (r0, r1) = (r0.min(r1), r0.max(r1));
    for row in r0..=r1 {
        for col in c0..=c1 {
            let x = -half + col as f32 * spacing;
            let z = -half + row as f32 * spacing;
            let dx = x - end[0];
            let dz = z - end[1];
            let along = dx * tx + dz * tz;
            let lateral = dx * px + dz * pz;
            if !(-8.0..=42.0).contains(&along) || lateral.abs() > 24.0 {
                continue;
            }
            // Notch centred just inside the hill. The hole ellipse is a little larger.
            let oa = (along - 1.0) / 9.2;
            let ol = lateral / 6.6;
            let notch = oa * oa + ol * ol;
            if notch <= 1.0 {
                continue;
            }
            let rise = smoothstep(-2.0, 12.0, along);
            let side = 1.0 - smoothstep(7.0, 24.0, lateral.abs());
            let collar = smoothstep(1.0, 1.45, notch.sqrt());
            heights[(row * res + col) as usize] += 15.0 * rise * side * collar;
        }
    }
}

/// Flattens a ribbon onto `center_heights` and raises a lip outside the shoulder.
/// Samples past the lip are unchanged. Crate-visible so the road tests can drive it
/// without a full planet.
pub(crate) fn carve_road(
    heights: &mut [f32],
    res: u32,
    world: f32,
    samples: &[[f32; 2]],
    center_heights: &[f32],
    cut: RoadCut,
) {
    if samples.len() < 2 || samples.len() != center_heights.len() {
        return;
    }
    let n = (res * res) as usize;
    let mut best_d = vec![f32::MAX; n];
    let mut best_h = vec![0.0_f32; n];
    let half = world * 0.5;
    let spacing = world / (res - 1) as f32;
    let reach = cut.half_width + cut.shoulder;
    let lip_end = reach + cut.lip_width.max(0.0);
    for seg in 0..samples.len() - 1 {
        let a = samples[seg];
        let b = samples[seg + 1];
        let ha = center_heights[seg];
        let hb = center_heights[seg + 1];
        let min_x = a[0].min(b[0]) - lip_end;
        let max_x = a[0].max(b[0]) + lip_end;
        let min_z = a[1].min(b[1]) - lip_end;
        let max_z = a[1].max(b[1]) + lip_end;
        let c0 = world_to_cell(min_x, half, spacing, res);
        let c1 = world_to_cell(max_x, half, spacing, res);
        let r0 = world_to_cell(min_z, half, spacing, res);
        let r1 = world_to_cell(max_z, half, spacing, res);
        let (c0, c1) = (c0.min(c1), c0.max(c1));
        let (r0, r1) = (r0.min(r1), r0.max(r1));
        for row in r0..=r1 {
            for col in c0..=c1 {
                let x = -half + col as f32 * spacing;
                let z = -half + row as f32 * spacing;
                let (dist, t) = segment_distance(a, b, [x, z]);
                let i = (row * res + col) as usize;
                if dist < best_d[i] {
                    best_d[i] = dist;
                    best_h[i] = ha + (hb - ha) * t;
                }
            }
        }
    }
    for i in 0..n {
        let dist = best_d[i];
        if dist > lip_end {
            continue;
        }
        if dist <= reach {
            let weight = 1.0 - smoothstep(cut.half_width, reach, dist);
            let target = best_h[i] - cut.depth;
            heights[i] += (target - heights[i]) * weight;
        } else if cut.lip_width > 0.0 && cut.lip_height != 0.0 {
            let u = (dist - reach) / cut.lip_width;
            let tent = (1.0 - (u * 2.0 - 1.0).abs()).max(0.0);
            heights[i] += tent * cut.lip_height;
        }
    }
}

fn world_to_cell(coord: f32, half: f32, spacing: f32, res: u32) -> u32 {
    ((coord + half) / spacing).round().clamp(0.0, (res - 1) as f32) as u32
}

fn segment_distance(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> (f32, f32) {
    let abx = b[0] - a[0];
    let abz = b[1] - a[1];
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1.0e-8 {
        0.0
    } else {
        (((p[0] - a[0]) * abx + (p[1] - a[1]) * abz) / len2).clamp(0.0, 1.0)
    };
    let cx = a[0] + abx * t;
    let cz = a[1] + abz * t;
    ((p[0] - cx).hypot(p[1] - cz), t)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1.0e-4)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn quantize(heights: &[f32], width: u32, height: u32, world: f32) -> Result<TerrainMap, LookdevError> {
    let mut min_h = f32::MAX;
    let mut max_h = f32::MIN;
    for h in heights {
        if !h.is_finite() {
            return Err(LookdevError::Config("heightfield sample is not finite".into()));
        }
        min_h = min_h.min(*h);
        max_h = max_h.max(*h);
    }
    let span = (max_h - min_h).max(1.0);
    let mut samples = Vec::with_capacity(heights.len());
    for h in heights {
        let t = ((*h - min_h) / span).clamp(0.0, 1.0);
        let q = (t * 65535.0).round().clamp(0.0, 65535.0);
        samples.push(q as u16);
    }
    let field = Heightfield {
        width,
        height,
        samples,
    };
    Ok(TerrainMap::new(&field, world, span, min_h).map_err(regia_terrain::TerrainError::from)?)
}

fn altitude_edges(heights: &[f32]) -> [f32; 5] {
    let mut min_h = f32::MAX;
    let mut max_h = f32::MIN;
    for h in heights {
        min_h = min_h.min(*h);
        max_h = max_h.max(*h);
    }
    let span = (max_h - min_h).max(20.0);
    let min_h = if max_h - min_h < 20.0 { max_h - 20.0 } else { min_h };
    let max_h = min_h + span;
    [
        min_h - 2.0,
        min_h + span * 0.22,
        min_h + span * 0.48,
        min_h + span * 0.74,
        max_h + 2.0,
    ]
}

fn layer(
    name: &str,
    albedo: [f32; 3],
    roughness: f32,
    tiling: f32,
    bands: [f32; 4],
    triplanar: bool,
) -> LayerDesc {
    let mut desc = LayerDesc::ground();
    desc.name = name.to_string();
    desc.albedo = albedo;
    desc.roughness = roughness;
    desc.tiling = tiling;
    desc.slope_min = bands[0];
    desc.slope_max = bands[1];
    desc.altitude_min = bands[2];
    desc.altitude_max = bands[3];
    desc.blend_sharpness = 0.12;
    desc.triplanar = triplanar;
    desc
}

fn planet_layers(planet: Planet, e: [f32; 5]) -> Result<TerrainLayers, LookdevError> {
    let layers = match planet {
        Planet::Europa => vec![
            layer(
                "regolith",
                [0.32, 0.30, 0.28],
                0.92,
                6.0,
                [0.0, 0.95, e[0], e[2]],
                false,
            ),
            layer(
                "dirty ice",
                [0.48, 0.56, 0.58],
                0.42,
                9.0,
                [0.0, 0.55, e[1], e[3]],
                false,
            ),
            layer(
                "blue ice",
                [0.16, 0.34, 0.55],
                0.16,
                12.0,
                [0.0, 0.40, e[0], e[1] + 1.0],
                false,
            ),
            layer(
                "snow",
                [0.84, 0.87, 0.91],
                0.72,
                7.0,
                [0.0, 0.50, e[2], e[4]],
                false,
            ),
            layer(
                "wind-packed snow",
                [0.74, 0.78, 0.83],
                0.40,
                4.0,
                [0.0, 0.30, e[3], e[4]],
                false,
            ),
            layer(
                "frosted basalt",
                [0.18, 0.20, 0.24],
                0.86,
                3.0,
                [0.42, 1.35, e[0], e[4]],
                true,
            ),
        ],
        Planet::Mars => vec![
            layer(
                "dust",
                [0.55, 0.32, 0.16],
                0.96,
                11.0,
                [0.0, 0.45, e[0], e[2]],
                false,
            ),
            layer(
                "dune sand",
                [0.74, 0.52, 0.26],
                0.78,
                5.0,
                [0.0, 0.28, e[0], e[1] + 1.0],
                false,
            ),
            layer(
                "gravel",
                [0.42, 0.30, 0.20],
                0.90,
                2.5,
                [0.05, 0.70, e[1], e[3]],
                false,
            ),
            layer(
                "sandstone strata",
                [0.64, 0.38, 0.20],
                0.74,
                6.5,
                [0.15, 0.85, e[2], e[4]],
                false,
            ),
            layer(
                "scree",
                [0.38, 0.32, 0.27],
                0.94,
                1.8,
                [0.40, 1.20, e[0], e[4]],
                true,
            ),
            layer(
                "bedrock",
                [0.30, 0.20, 0.16],
                0.68,
                4.0,
                [0.55, 1.40, e[0], e[4]],
                true,
            ),
        ],
    };
    let stack = TerrainLayers {
        version: regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION,
        wetness_default: match planet {
            Planet::Europa => 0.18,
            Planet::Mars => 0.0,
        },
        layers,
        puddles: true,
    };
    stack.validate().map_err(regia_terrain::TerrainError::from)?;
    Ok(stack)
}

fn layer_heights(planet: Planet, layers: &TerrainLayers, resolution: u32) -> Vec<LayerHeightMap> {
    layers
        .layers
        .iter()
        .enumerate()
        .map(|(i, layer)| LayerHeightMap {
            name: layer.name.clone(),
            resolution,
            samples: noise_image(
                resolution,
                planet.seed().wrapping_add(20 + i as u32),
                3,
                4.0 + i as f32,
            ),
        })
        .collect()
}

fn macro_map(planet: Planet, resolution: u32) -> MacroMap {
    // Two octaves across the whole image: the variation is island-scale, not a tile.
    MacroMap {
        resolution,
        samples: noise_image(resolution, planet.seed() ^ 0x0A11_0000, 4, 1.5),
    }
}

/// Zeroed `resolution²` buffer. The 8192 test calls this directly: filling that many
/// samples with four-octave noise is the evidence-run cost we keep off the GPU lock.
fn allocate_f32_image(resolution: u32) -> Vec<f32> {
    let n = (resolution as usize).saturating_mul(resolution as usize);
    vec![0.0; n]
}

fn noise_image(resolution: u32, seed: u32, octaves: u32, frequency: f32) -> Vec<f32> {
    let mut samples = allocate_f32_image(resolution);
    if resolution < 2 {
        return samples;
    }
    for row in 0..resolution {
        let v = row as f32 / (resolution - 1) as f32;
        for col in 0..resolution {
            let u = col as f32 / (resolution - 1) as f32;
            samples[(row * resolution + col) as usize] = fbm(u * frequency, v * frequency, seed, octaves);
        }
    }
    samples
}

fn portal(planet: Planet, road: &BuiltRoad, floor: f32) -> Result<(HolesMask, MeshTunnel), LookdevError> {
    let tangent = road.path.end_tangent;
    // Slightly larger than the hill notch and the mesh cross-section, so the
    // heightfield drops the triangles that would cover the mouth.
    let mouth = HoleEllipse {
        center: road.path.end,
        radius: [10.5, 7.4],
        rotation_radians: tangent[1].atan2(tangent[0]),
    };
    let (positions, indices) = portal_box(road.path.end, floor, tangent);
    let triangles = (indices.len() / 3) as u32;
    let tunnel = MeshTunnel::from_collision(
        &positions,
        &indices,
        mouth,
        TunnelDetail {
            normal_displace: 0.08,
            noise_amplitude: 0.18,
            noise_scale: 0.7,
            seed: planet.seed(),
        },
        CollisionProvenance {
            source_asset: format!("lookdev/{}/tunnel_portal", planet.name()),
            triangle_count: triangles,
            note: "procedural stand-in portal, not a simulation collision export".into(),
        },
    )?;
    let holes = tunnel.holes.clone();
    Ok((holes, tunnel))
}

fn portal_box(end: [f32; 2], floor: f32, tangent: [f32; 2]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let len = tangent[0].hypot(tangent[1]).max(1.0e-4);
    let tx = tangent[0] / len;
    let tz = tangent[1] / len;
    let px = -tz;
    let pz = tx;
    // Tall enough to fill the hole in the hillside, and starting on the camera
    // side of the hole so the interior blocks the sky.
    let length = 22.0_f32;
    let half_w = 5.6_f32;
    let height = 11.0_f32;
    let mut verts = Vec::with_capacity(8);
    for along in [-3.0_f32, length] {
        for side in [-half_w, half_w] {
            for y in [0.0_f32, height] {
                verts.push([
                    end[0] + tx * along + px * side,
                    floor + y,
                    end[1] + tz * along + pz * side,
                ]);
            }
        }
    }
    // 8 vertices, y changes fastest, then side, then along.
    // 0: along0 side- y0, 1: y1, 2: side+ y0, 3: y1, 4..7: along1.
    // The near end stays open. A cap there is a wall in front of the camera,
    // so the shot reads as a box instead of a mouth.
    let quads = [
        [4, 5, 7, 6], // far cap
        [0, 1, 5, 4], // left
        [2, 6, 7, 3], // right
        [1, 3, 7, 5], // ceiling
        [0, 4, 6, 2], // floor
    ];
    let mut indices = Vec::with_capacity(36);
    for q in quads {
        indices.extend([q[0], q[1], q[2], q[0], q[2], q[3]]);
    }
    (verts, indices)
}

fn scatter_rocks(
    planet: Planet,
    map: &TerrainMap,
    layers: &TerrainLayers,
    holes: &HolesMask,
    road: &BuiltRoad,
    cap: usize,
) -> Vec<ScatterInstance> {
    let rule = ScatterRule {
        seed: u64::from(planet.seed()),
        cell_size: 64.0,
        instances_per_cell: 0.45,
        min_scale: 0.35,
        max_scale: 2.4,
        slope_max: 1.05,
        avoid_holes: true,
        mesh: format!("lookdev/{}/rock", planet.name()),
        fade_start_distance: 180.0,
        fade_end_distance: 420.0,
        ..ScatterRule::default()
    };
    if rule.validate(Some(layers)).is_err() {
        return portal_ring(road, map);
    }
    let mut rocks = place(map, Some(holes), Some(layers), &rule);
    let road_samples = &road.path.samples;
    rocks.retain(|rock| {
        let p = [rock.position[0], rock.position[2]];
        nearest_xz(road_samples, p) > 9.0
    });
    let focus = road.path.bend;
    rocks.sort_by(|a, b| {
        let da = (a.position[0] - focus[0]).hypot(a.position[2] - focus[1]);
        let db = (b.position[0] - focus[0]).hypot(b.position[2] - focus[1]);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    rocks.truncate(cap);
    rocks.extend(portal_ring(road, map));
    rocks
}

fn portal_ring(road: &BuiltRoad, map: &TerrainMap) -> Vec<ScatterInstance> {
    let tangent = road.path.end_tangent;
    let px = -tangent[1];
    let pz = tangent[0];
    let mut ring = Vec::with_capacity(8);
    for i in 0..8 {
        let side = if i % 2 == 0 { -1.0 } else { 1.0 };
        let along = (i as f32 - 3.5) * 3.2;
        let x = road.path.end[0] - tangent[0] * 5.0 + tangent[0] * along * 0.15 + px * side * 9.0;
        let z = road.path.end[1] - tangent[1] * 5.0 + tangent[1] * along * 0.15 + pz * side * 9.0;
        let y = map.sample_bilinear(x, z);
        ring.push(ScatterInstance {
            position: [x, y, z],
            yaw: i as f32 * 0.7,
            scale: 1.6 + (i % 3) as f32 * 0.35,
            normal: map.normal_at(x, z),
            tint: [0.45, 0.42, 0.38],
        });
    }
    ring
}

fn nearest_xz(samples: &[[f32; 2]], p: [f32; 2]) -> f32 {
    samples
        .iter()
        .map(|s| (s[0] - p[0]).hypot(s[1] - p[1]))
        .fold(f32::MAX, f32::min)
}

fn landmarks_from(road: &BuiltRoad, world: f32) -> Landmarks {
    let scale = world / 4096.0;
    let close_i = road.path.samples.len() / 5;
    let close = road.path.samples[close_i];
    Landmarks {
        close,
        vista_eye: [-180.0 * scale, -1900.0 * scale],
        vista_target: [40.0 * scale, 100.0 * scale],
        low_sun: [close[0] - 40.0 * scale, close[1] - 80.0 * scale],
        sky: road.path.samples[road.path.samples.len() * 3 / 5],
    }
}

fn climate_for(planet: Planet, base_height: f32) -> Climate {
    match planet {
        Planet::Europa => Climate {
            fog_color: [0.62, 0.74, 0.86],
            fog_density: 0.00032,
            fog_falloff: 0.018,
            wetness: 0.16,
            ambient: [0.42, 0.50, 0.62],
            sun_azimuth: 0.7,
            sun_elevation: 0.55,
            sun_color: [0.85, 0.90, 1.0],
            sun_lux: 80_000.0,
            base_height,
        },
        Planet::Mars => Climate {
            fog_color: [0.72, 0.48, 0.30],
            fog_density: 0.00042,
            fog_falloff: 0.012,
            wetness: 0.0,
            ambient: [0.55, 0.40, 0.30],
            sun_azimuth: 2.4,
            sun_elevation: 0.62,
            sun_color: [1.0, 0.78, 0.55],
            sun_lux: 90_000.0,
            base_height,
        },
    }
}

/// Fog for one profile scale. `1` is the planet haze; Deck uses less.
#[must_use]
pub fn fog_for(climate: &Climate, scale: f32) -> FogSettings {
    FogSettings {
        enabled: true,
        density: climate.fog_density * scale.max(0.0),
        height_falloff: climate.fog_falloff,
        base_height: climate.base_height,
        color: climate.fog_color,
        sky_inscatter: false,
    }
}

fn mean_height(heights: &[f32]) -> f32 {
    if heights.is_empty() {
        return 0.0;
    }
    let step = (heights.len() / 4096).max(1);
    let mut sum = 0.0_f32;
    let mut n = 0.0_f32;
    for h in heights.iter().step_by(step) {
        sum += h;
        n += 1.0;
    }
    sum / n.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_config() -> LookdevConfig {
        let mut config = LookdevConfig::tiny();
        config.planets = vec![Planet::Europa];
        config
    }

    #[test]
    fn europa_has_six_named_layers_height_maps_and_a_quantised_field() {
        let scene = generate(Planet::Europa, &tiny_config()).expect("europa");
        let names: Vec<_> = scene.layers.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "regolith",
                "dirty ice",
                "blue ice",
                "snow",
                "wind-packed snow",
                "frosted basalt"
            ]
        );
        assert_eq!(scene.layer_height_maps.len(), 6);
        assert_eq!(scene.macro_variation.resolution, tiny_config().macro_resolution);
        assert_eq!(
            scene.macro_variation.samples.len(),
            (scene.macro_variation.resolution * scene.macro_variation.resolution) as usize
        );
        assert!(
            scene
                .macro_variation
                .samples
                .iter()
                .all(|s| (0.0..=1.0).contains(s))
        );
        let scale = scene.map.height_scale();
        let offset = scene.map.height_offset();
        for h in scene.map.heights().iter().step_by(17) {
            let t = (h - offset) / scale * 65535.0;
            assert!(
                (t - t.round()).abs() < 1.0e-2,
                "height {h} is off the 16-bit lattice"
            );
        }
        assert!(scene.holes.contains(scene.road.end[0], scene.road.end[1]));
        assert!(scene.tunnel.provenance.triangle_count >= 1);
        let (ymin, ymax) = scene
            .tunnel
            .render_mesh
            .positions
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        assert!(ymax - ymin > 8.0, "portal mesh height {}", ymax - ymin);
        assert!(!scene.tunnel.render_mesh.indices.is_empty());
        assert!(!scene.scatter.is_empty());
        let span = (scene.landmarks.vista_target[1] - scene.landmarks.vista_eye[1]).abs();
        assert!((span - 2000.0).abs() < 1.0, "vista should be 2 km, got {span}");
    }

    #[test]
    fn mars_layer_names_match_the_switchbacks() {
        let mut config = tiny_config();
        config.hydraulic_drops = 8;
        let scene = generate(Planet::Mars, &config).expect("mars");
        let names: Vec<_> = scene.layers.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "dust",
                "dune sand",
                "gravel",
                "sandstone strata",
                "scree",
                "bedrock"
            ]
        );
    }

    #[test]
    fn road_carve_flattens_a_bumpy_strip() {
        let res = 64_u32;
        let world = 256.0_f32;
        let mut heights = vec![0.0_f32; (res * res) as usize];
        for row in 0..res {
            for col in 0..res {
                heights[(row * res + col) as usize] =
                    ((col as f32 * 0.7).sin() + (row as f32 * 0.5).cos()) * 8.0;
            }
        }
        let samples: Vec<[f32; 2]> = (0..16).map(|i| [-100.0 + i as f32 * 12.0, 0.0]).collect();
        let center = vec![1.0_f32; samples.len()];
        let before = variance_along(&heights, res, world, &samples);
        carve_road(
            &mut heights,
            res,
            world,
            &samples,
            &center,
            RoadCut {
                half_width: 6.0,
                shoulder: 8.0,
                depth: 0.0,
                lip_width: 0.0,
                lip_height: 0.0,
            },
        );
        let after = variance_along(&heights, res, world, &samples);
        assert!(after < before * 0.35, "variance {before} -> {after}");
    }

    fn variance_along(heights: &[f32], res: u32, world: f32, samples: &[[f32; 2]]) -> f32 {
        let values: Vec<f32> = samples
            .iter()
            .map(|p| sample_height(heights, res, world, p[0], p[1]))
            .collect();
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / values.len() as f32
    }

    #[test]
    fn road_carve_cuts_a_trench_and_a_lip() {
        let res = 128_u32;
        let world = 256.0_f32;
        let mut heights = vec![40.0_f32; (res * res) as usize];
        let samples: Vec<[f32; 2]> = (0..21).map(|i| [-80.0 + i as f32 * 8.0, 0.0]).collect();
        let center = vec![40.0_f32; samples.len()];
        carve_road(&mut heights, res, world, &samples, &center, RoadCut::lookdev());
        let floor = sample_height(&heights, res, world, 0.0, 0.0);
        let far = sample_height(&heights, res, world, 0.0, 22.0);
        // The lip is 3.2 m wide. A bilinear sample on this 2 m grid straddles the
        // shoulder, so read the cell nearest the tent peak.
        let spacing = world / (res - 1) as f32;
        let peak = 8.0 + 2.8 + 1.6;
        let row = ((peak + world * 0.5) / spacing).round() as u32;
        let col = ((world * 0.5) / spacing).round() as u32;
        let lip = heights[(row * res + col) as usize];
        assert!(floor < 36.0, "floor {floor} should be about 34");
        assert!(far > 39.0, "untouched ground {far}");
        assert!(lip > 41.0, "lip cell {lip} should stand above the field");
    }

    #[test]
    fn portal_hill_leaves_a_notch_and_raises_the_side() {
        let res = 128_u32;
        let world = 256.0_f32;
        let mut heights = vec![10.0_f32; (res * res) as usize];
        raise_portal_hill(&mut heights, res, world, [0.0, 0.0], [1.0, 0.0]);
        let notch = sample_height(&heights, res, world, 1.0, 0.0);
        let side = sample_height(&heights, res, world, 16.0, 14.0);
        assert!((notch - 10.0).abs() < 0.2, "notch {notch}");
        assert!(side > notch + 4.0, "side {side} notch {notch}");
    }

    #[test]
    fn macro_8k_resolution_allocates() {
        let samples = allocate_f32_image(LookdevConfig::MACRO_8K);
        assert_eq!(samples.len(), 8192 * 8192);
        assert_eq!(samples.len() * core::mem::size_of::<f32>(), 8192 * 8192 * 4);
    }
}
