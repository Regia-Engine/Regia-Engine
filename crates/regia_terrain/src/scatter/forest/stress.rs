// SPDX-License-Identifier: MPL-2.0
//! A synthetic dense forest and a scripted flythrough (ADR-1503): stand-in trees with LOD chains
//! and crown parts, a forest of them on rolling terrain (optionally with canopy gaps), and a
//! runner that flies a camera path through a [`ScatterField`] frame by frame and records what each
//! frame streamed, emitted and popped.
//!
//! The forest is world data for tests and evidence only: the showcase's forest zone is authored
//! world data (Tincal's kits), and this stands in for it with the same structure (two canopy
//! classes, an understory) and the same field paths.

use std::sync::Arc;

use regia_render::{LabMaterial, LabVertex};

use super::super::ScatterError;
use super::super::field::{
    MAX_SCATTER_LODS, ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterMeshLod, ScatterMeshPart,
    ScatterSource, ScatterView,
};
use super::super::placement::DensityMaps;
use super::super::set::{ScatterClass, ScatterMeshRef, ScatterSet};
use super::corridor::CameraPath;
use super::gaps::{CanopyGaps, canopy_gap_map};
use super::pops::{PopEvent, PopMeter, PopStats, SwitchRecord};
use super::wake::{SwayField, SwaySettings, TrackWake, WakeModel, WakeSample};
use crate::sampler::TerrainMap;

/// Crown form of a stand-in tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeKind {
    /// A rounded crown (an ellipsoid).
    Broadleaf,
    /// A conical crown.
    Conifer,
    /// A low bush: the crown starts near the ground.
    Bush,
}

/// Dimensions of a stand-in tree, metres at scale 1.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StandinTree {
    /// Crown form.
    pub kind: TreeKind,
    /// Total height.
    pub height_m: f32,
    /// Trunk radius at the base.
    pub trunk_radius_m: f32,
    /// Crown radius.
    pub crown_radius_m: f32,
    /// Height of the crown base.
    pub crown_base_m: f32,
    /// LOD switch sizes (projected radius over the screen half-height) of LOD 0, 1 and 2 (the last
    /// is forced to 0).
    pub lod_screen_sizes: [f32; 3],
}

impl StandinTree {
    /// A 22 m broadleaf with a crown from 8 m.
    #[must_use]
    pub fn broadleaf() -> Self {
        Self {
            kind: TreeKind::Broadleaf,
            height_m: 22.0,
            trunk_radius_m: 0.35,
            crown_radius_m: 5.5,
            crown_base_m: 8.0,
            lod_screen_sizes: [0.6, 0.15, 0.0],
        }
    }

    /// A 26 m conifer with a crown from 5 m.
    #[must_use]
    pub fn conifer() -> Self {
        Self {
            kind: TreeKind::Conifer,
            height_m: 26.0,
            trunk_radius_m: 0.3,
            crown_radius_m: 3.5,
            crown_base_m: 5.0,
            lod_screen_sizes: [0.6, 0.15, 0.0],
        }
    }

    /// A 2.5 m bush.
    #[must_use]
    pub fn bush() -> Self {
        Self {
            kind: TreeKind::Bush,
            height_m: 2.5,
            trunk_radius_m: 0.08,
            crown_radius_m: 1.4,
            crown_base_m: 0.4,
            lod_screen_sizes: [0.6, 0.15, 0.0],
        }
    }
}

/// Trunk sides and crown (rings, segments) of LOD 0, 1 and 2.
const TREE_LODS: [(u32, u32, u32); 3] = [(12, 10, 16), (6, 5, 8), (4, 3, 5)];

/// Builds a stand-in tree: a tapered trunk (the asset's own geometry) and a crown (material part
/// 1), three LODs each, pivot at the base centre.
#[must_use]
pub fn standin_tree(tree: &StandinTree) -> ScatterMeshAsset {
    let mut lods = Vec::with_capacity(3);
    let mut crowns = Vec::with_capacity(3);
    for (i, &(sides, rings, segments)) in TREE_LODS.iter().enumerate() {
        let screen_size = if i + 1 == TREE_LODS.len() {
            0.0
        } else {
            tree.lod_screen_sizes[i].max(0.0)
        };
        lods.push(trunk_lod(tree, sides, screen_size));
        crowns.push(crown_lod(tree, rings, segments, screen_size));
    }
    debug_assert!(lods.len() <= MAX_SCATTER_LODS);
    ScatterMeshAsset {
        lods,
        material: LabMaterial {
            base_color: [0.16, 0.12, 0.09, 1.0],
            roughness: 0.9,
            metallic: 0.0,
            ..LabMaterial::default()
        },
        parts: vec![ScatterMeshPart {
            lods: crowns,
            material: LabMaterial {
                base_color: match tree.kind {
                    TreeKind::Conifer => [0.05, 0.11, 0.06, 1.0],
                    TreeKind::Broadleaf => [0.09, 0.16, 0.06, 1.0],
                    TreeKind::Bush => [0.11, 0.15, 0.07, 1.0],
                },
                roughness: 0.8,
                metallic: 0.0,
                ..LabMaterial::default()
            },
        }],
        shadow_lod: 0,
    }
}

fn trunk_lod(tree: &StandinTree, sides: u32, screen_size: f32) -> ScatterMeshLod {
    let bottom = -0.3f32;
    let top = (tree.crown_base_m + 0.3 * (tree.height_m - tree.crown_base_m)).min(tree.height_m);
    let (r0, r1) = (tree.trunk_radius_m, tree.trunk_radius_m * 0.6);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for k in 0..=sides {
        let a = k as f32 / sides as f32 * std::f32::consts::TAU;
        let (s, c) = a.sin_cos();
        let u = k as f32 / sides as f32;
        vertices.push(LabVertex::from_pos_normal_uv(
            [r0 * c, bottom, r0 * s],
            [c, 0.0, s],
            [u, 0.0],
        ));
        vertices.push(LabVertex::from_pos_normal_uv(
            [r1 * c, top, r1 * s],
            [c, 0.0, s],
            [u, 1.0],
        ));
    }
    for k in 0..sides {
        let b = 2 * k;
        indices.extend_from_slice(&[b, b + 1, b + 2, b + 1, b + 3, b + 2]);
    }
    ScatterMeshLod {
        vertices,
        indices,
        screen_size,
    }
}

fn crown_lod(tree: &StandinTree, rings: u32, segments: u32, screen_size: f32) -> ScatterMeshLod {
    let base = tree.crown_base_m;
    let h = (tree.height_m - base).max(0.1);
    let r = tree.crown_radius_m;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    // Rings from the bottom pole (ring 0) to the top pole (ring `rings`).
    for ring in 0..=rings {
        let v = ring as f32 / rings as f32;
        let (y, radius) = match tree.kind {
            TreeKind::Conifer => (base + v * h, r * (1.0 - v)),
            TreeKind::Broadleaf | TreeKind::Bush => {
                let phi = v * std::f32::consts::PI;
                (base + 0.5 * h * (1.0 - phi.cos()), r * phi.sin())
            }
        };
        for seg in 0..=segments {
            let a = seg as f32 / segments as f32 * std::f32::consts::TAU;
            let (s, c) = a.sin_cos();
            let p = [radius * c, y, radius * s];
            // Ellipsoid (or cone) normal, roughly: outward and up by the profile slope.
            let ny = match tree.kind {
                TreeKind::Conifer => r / h,
                _ => (y - (base + 0.5 * h)) / (0.5 * h).max(1e-3) * (r / (0.5 * h).max(1e-3)),
            };
            let n = normalize([c, ny, s]);
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [seg as f32 / segments as f32, v],
            ));
        }
    }
    let row = segments + 1;
    for ring in 0..rings {
        for seg in 0..segments {
            let a = ring * row + seg;
            let b = a + row;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    ScatterMeshLod {
        vertices,
        indices,
        screen_size,
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// How [`synthetic_forest`] builds its forest.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ForestSpec {
    /// Placement seed.
    pub seed: u64,
    /// Terrain side, metres (centred on the origin).
    pub world_width_m: f32,
    /// Heightfield samples per side.
    pub samples: u32,
    /// Amplitude of the rolling hills, metres.
    pub hill_m: f32,
    /// Spacing of the broadleaf canopy cells, metres (one candidate per cell).
    pub spacing_m: f32,
    /// Canopy classes' fade start and end, metres.
    pub canopy_fade_m: [f32; 2],
    /// Understory fade start and end, metres.
    pub understory_fade_m: [f32; 2],
    /// Canopy gaps cut into both canopy classes (`None`: closed canopy).
    pub gaps: Option<CanopyGaps>,
    /// Gap map side, pixels.
    pub gap_map_side: u32,
}

impl Default for ForestSpec {
    fn default() -> Self {
        Self {
            seed: 0x0f02_e57f_0000_1503,
            world_width_m: 2048.0,
            samples: 257,
            hill_m: 25.0,
            spacing_m: 6.0,
            canopy_fade_m: [300.0, 380.0],
            understory_fade_m: [100.0, 140.0],
            gaps: None,
            gap_map_side: 1025,
        }
    }
}

/// A forest built by [`synthetic_forest`].
#[derive(Clone)]
pub struct SyntheticForest {
    /// Terrain and density maps.
    pub source: ScatterSource,
    /// The scatter set (classes `canopy_broadleaf`, `canopy_conifer`, `understory`).
    pub set: ScatterSet,
    /// The stand-in meshes the set names.
    pub library: ScatterLibrary,
}

/// Path of the gap density map the canopy classes name when [`ForestSpec::gaps`] is set.
pub const GAP_MAP_PATH: &str = "forest/canopy_gaps.png";

/// Builds a dense synthetic forest (see the module docs).
///
/// # Errors
///
/// [`ScatterError`] for an unusable spec (terrain size, gap map side).
pub fn synthetic_forest(spec: &ForestSpec) -> Result<SyntheticForest, ScatterError> {
    let n = spec.samples.max(2);
    let w = spec.world_width_m;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * w - w * 0.5;
            let z = row as f32 / (n - 1) as f32 * w - w * 0.5;
            heights.push(
                40.0 + spec.hill_m * (x * 0.004).sin() * (z * 0.0035).cos()
                    + 0.25 * spec.hill_m * (x * 0.013 + z * 0.011).sin(),
            );
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, w, 1.0, 0.0).map_err(|e| ScatterError::Asset {
        path: "<synthetic terrain>".into(),
        reason: e.to_string(),
    })?;
    let mut density_maps = DensityMaps::new();
    let gap_map = match &spec.gaps {
        Some(gaps) => {
            let m = canopy_gap_map(gaps, w, spec.gap_map_side)?;
            density_maps.insert(GAP_MAP_PATH.into(), m.density_map()?);
            Some(GAP_MAP_PATH.to_string())
        }
        None => None,
    };
    let tree = |mesh: &str, min: f32, max: f32| ScatterMeshRef {
        mesh: mesh.into(),
        weight: 1.0,
        min_scale: min,
        max_scale: max,
        scale_bias: 1.0,
    };
    let canopy = |name: &str, meshes: Vec<ScatterMeshRef>, cell: f32, density: f32| ScatterClass {
        name: name.into(),
        meshes,
        cell_size: cell,
        density,
        density_map: gap_map.clone(),
        slope_max: 0.7,
        yaw_jitter: std::f32::consts::TAU,
        align_to_normal: 0.0,
        tilt_max: 0.05,
        tilt_jitter: 0.02,
        sink: 0.01,
        tint_jitter: 0.12,
        contact_ao: 0.0,
        fade_start: spec.canopy_fade_m[0],
        fade_end: spec.canopy_fade_m[1],
        cast_shadows: true,
        shadow_max_lod: 2,
        ..ScatterClass::default()
    };
    let set = ScatterSet {
        seed: spec.seed,
        classes: vec![
            canopy(
                "canopy_broadleaf",
                vec![tree("standin/broadleaf.glb", 0.8, 1.25)],
                spec.spacing_m.max(1.0),
                1.0,
            ),
            canopy(
                "canopy_conifer",
                vec![tree("standin/conifer.glb", 0.85, 1.2)],
                spec.spacing_m.max(1.0) * 1.5,
                0.6,
            ),
            ScatterClass {
                name: "understory".into(),
                meshes: vec![tree("standin/bush.glb", 0.6, 1.4)],
                cell_size: (spec.spacing_m * 0.5).max(1.0),
                density: 0.5,
                slope_max: 0.8,
                align_to_normal: 0.3,
                tilt_max: 0.2,
                sink: 0.05,
                contact_ao: 0.0,
                fade_start: spec.understory_fade_m[0],
                fade_end: spec.understory_fade_m[1],
                cast_shadows: false,
                ..ScatterClass::default()
            },
        ],
        ..ScatterSet::default()
    };
    let mut library = ScatterLibrary::new();
    library.insert(
        "standin/broadleaf.glb".into(),
        standin_tree(&StandinTree::broadleaf()),
    );
    library.insert(
        "standin/conifer.glb".into(),
        standin_tree(&StandinTree::conifer()),
    );
    library.insert("standin/bush.glb".into(), standin_tree(&StandinTree::bush()));
    let source = ScatterSource {
        density_maps,
        ..ScatterSource::new(Arc::new(map))
    };
    Ok(SyntheticForest { source, set, library })
}

/// A camera path sampled by time: by the path's own times, or at a constant speed along it.
#[derive(Clone, Debug)]
pub struct PathSampler {
    points: Vec<[f32; 3]>,
    /// Arc length at each point.
    along: Vec<f32>,
    /// Time at each point.
    times: Vec<f32>,
}

impl PathSampler {
    /// Samples `path` by its times when it has valid ones (as many as points, non-decreasing),
    /// otherwise at `speed_m_s` along its length.
    #[must_use]
    pub fn new(path: &CameraPath, speed_m_s: f32) -> Self {
        let points = path.points.clone();
        let mut along = Vec::with_capacity(points.len());
        let mut s = 0.0f32;
        for (i, p) in points.iter().enumerate() {
            if i > 0 {
                let q = points[i - 1];
                s += ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
            }
            along.push(s);
        }
        let valid = path.times.as_ref().filter(|t| {
            t.len() == points.len() && t.windows(2).all(|w| w[1] >= w[0]) && t.iter().all(|v| v.is_finite())
        });
        let times = match valid {
            Some(t) => t.clone(),
            None => {
                let v = speed_m_s.max(1e-3);
                along.iter().map(|a| a / v).collect()
            }
        };
        Self { points, along, times }
    }

    /// Duration of the path, seconds.
    #[must_use]
    pub fn duration(&self) -> f32 {
        match (self.times.first(), self.times.last()) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        }
    }

    /// Length of the path, metres.
    #[must_use]
    pub fn length(&self) -> f32 {
        self.along.last().copied().unwrap_or(0.0)
    }

    /// Arc length at time `t` seconds from the start.
    #[must_use]
    pub fn along_at(&self, t: f32) -> f32 {
        let t = t + self.times.first().copied().unwrap_or(0.0);
        let i = self.times.partition_point(|&x| x <= t);
        if i == 0 {
            return 0.0;
        }
        if i >= self.times.len() {
            return self.length();
        }
        let (t0, t1) = (self.times[i - 1], self.times[i]);
        let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
        self.along[i - 1] + (self.along[i] - self.along[i - 1]) * f
    }

    /// Point at arc length `s`.
    #[must_use]
    pub fn point_at(&self, s: f32) -> [f32; 3] {
        let Some(&first) = self.points.first() else {
            return [0.0; 3];
        };
        let i = self.along.partition_point(|&x| x <= s);
        if i == 0 {
            return first;
        }
        if i >= self.points.len() {
            return self.points[self.points.len() - 1];
        }
        let (s0, s1) = (self.along[i - 1], self.along[i]);
        let f = if s1 > s0 { (s - s0) / (s1 - s0) } else { 0.0 };
        let (a, b) = (self.points[i - 1], self.points[i]);
        [
            a[0] + (b[0] - a[0]) * f,
            a[1] + (b[1] - a[1]) * f,
            a[2] + (b[2] - a[2]) * f,
        ]
    }
}

/// How [`run_flythrough`] flies.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FlythroughSpec {
    /// Frames per second.
    pub fps: f32,
    /// Speed along the path when it has no times, metres per second.
    pub speed_m_s: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Aspect ratio.
    pub aspect: f32,
    /// Output height, pixels (pop sizes are measured at it).
    pub height_px: f32,
    /// Near plane, metres.
    pub near: f32,
    /// Far plane, metres.
    pub far: f32,
    /// Smallest projected radius a pop counts at, pixels.
    pub pop_threshold_px: f32,
    /// The camera looks at the path point this far ahead, metres.
    pub look_ahead_m: f32,
    /// Most frames flown (0 = the whole path).
    pub max_frames: u32,
    /// Sway the trees in the camera's wake.
    pub sway: bool,
    /// The wake model when [`Self::sway`] is on.
    pub wake: WakeModel,
    /// The sway response when [`Self::sway`] is on.
    pub sway_settings: SwaySettings,
    /// Log every on-screen LOD switch ([`FlythroughReport::switches`]).
    pub record_switches: bool,
    /// Measure the CPU time of each frame's update and collect (evidence runs; tests leave it
    /// off and never compare times).
    pub measure_time: bool,
}

impl Default for FlythroughSpec {
    fn default() -> Self {
        Self {
            fps: 60.0,
            speed_m_s: 100.0,
            fov_y: 50f32.to_radians(),
            aspect: 16.0 / 9.0,
            height_px: 2160.0,
            near: 0.1,
            far: 4000.0,
            pop_threshold_px: 2.0,
            look_ahead_m: 20.0,
            max_frames: 0,
            sway: false,
            wake: WakeModel::default(),
            sway_settings: SwaySettings::default(),
            record_switches: false,
            measure_time: false,
        }
    }
}

/// One frame of a flythrough.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct FlythroughFrame {
    /// Frame index.
    pub frame: u32,
    /// Eye, world metres.
    pub eye: [f32; 3],
    /// Instances resident.
    pub resident: u32,
    /// Instances examined in visible tiles.
    pub considered: u32,
    /// Instances emitted.
    pub emitted: u32,
    /// Draws handed to the renderer (instances × parts, plus cross-fade copies and shadow-only
    /// draws).
    pub draws: u32,
    /// Emitted instances per LOD.
    pub per_lod: [u32; MAX_SCATTER_LODS],
    /// Instances cross-fading LODs.
    pub lod_fading: u32,
    /// Tiles placed this frame.
    pub placed_tiles: u32,
    /// Tiles placed on the calling thread because the camera needed them (a hitch source).
    pub sync_tiles: u32,
    /// Needed tiles whose background placement was not done yet (placed on the calling thread).
    pub replaced_tiles: u32,
    /// This frame's pops.
    pub pops: PopStats,
    /// Trees swaying.
    pub swaying: u32,
    /// CPU microseconds of update + collect (+ sway), when measured.
    pub cpu_us: Option<u32>,
}

/// What [`run_flythrough`] saw.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct FlythroughReport {
    /// One entry per frame.
    pub frames: Vec<FlythroughFrame>,
    /// Pops over the whole flight.
    pub pops: PopStats,
    /// The pop events kept.
    pub events: Vec<PopEvent>,
    /// On-screen LOD switches (when logged).
    pub switches: Vec<SwitchRecord>,
    /// Largest emitted instance count of a frame.
    pub max_emitted: u32,
    /// Mean emitted instances per frame.
    pub mean_emitted: f32,
    /// Largest draw count of a frame.
    pub max_draws: u32,
    /// Largest considered count of a frame.
    pub max_considered: u32,
    /// Frames (after the first) that placed a needed tile on the calling thread.
    pub hitch_frames: u32,
    /// Largest lean of any tree, milliradians.
    pub max_tilt_mrad: u32,
    /// Median, 99th percentile and largest CPU microseconds per frame (when measured).
    pub cpu_us: Option<[u32; 3]>,
    /// Flight length, metres.
    pub length_m: f32,
    /// Flight duration, seconds.
    pub duration_s: f32,
}

/// Flies `path` through `field` at `spec.fps`, calling `update` and `collect` once per frame
/// like a host would, and records each frame. Deterministic except for the streaming counters
/// with background streaming on (when a background placement finishes) and the measured times.
pub fn run_flythrough(
    field: &mut ScatterField,
    path: &CameraPath,
    spec: &FlythroughSpec,
) -> FlythroughReport {
    let _span = tracing::info_span!(target: "regia_terrain", "forest_flythrough").entered();
    let sampler = PathSampler::new(path, spec.speed_m_s);
    let fps = spec.fps.max(1.0);
    let duration = sampler.duration();
    let mut frames_total = (duration * fps).floor() as u32 + 1;
    if spec.max_frames > 0 {
        frames_total = frames_total.min(spec.max_frames);
    }
    let mut meter = PopMeter::new(spec.pop_threshold_px, spec.height_px);
    meter.record_switches = spec.record_switches;
    let mut wake = TrackWake::new(spec.wake);
    let mut sway = SwayField::new(spec.sway_settings);
    let mut report = FlythroughReport {
        length_m: sampler.length(),
        duration_s: duration,
        ..FlythroughReport::default()
    };
    let mut last_dir = [0.0f32, 0.0, -1.0];
    let mut times = Vec::new();
    let mut emitted_sum = 0u64;
    for f in 0..frames_total {
        let t = f as f32 / fps;
        let s = sampler.along_at(t);
        let eye = sampler.point_at(s);
        let ahead = sampler.point_at(s + spec.look_ahead_m.max(0.1));
        let d = [ahead[0] - eye[0], ahead[1] - eye[1], ahead[2] - eye[2]];
        if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() > 1e-3 {
            last_dir = normalize(d);
        }
        let target = [eye[0] + last_dir[0], eye[1] + last_dir[1], eye[2] + last_dir[2]];
        let view = ScatterView::look_at(eye, target, spec.fov_y, spec.aspect, spec.near, spec.far);
        let start = spec.measure_time.then(std::time::Instant::now);
        let stream = field.update(eye);
        let mut draws = Vec::new();
        let stats = field.collect(&view, &mut draws);
        let mut swaying = 0;
        if spec.sway {
            wake.push(WakeSample {
                time_s: f64::from(f) / f64::from(fps),
                position: eye,
            });
            let st = sway.step(&wake, &draws, 1.0 / fps);
            sway.apply_frame(&mut draws);
            swaying = st.swaying;
            report.max_tilt_mrad = report.max_tilt_mrad.max(st.max_tilt_mrad);
        }
        let cpu_us = start.map(|s| s.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
        if let Some(us) = cpu_us {
            times.push(us);
        }
        let pops = meter.observe(&view, &draws);
        let frame = FlythroughFrame {
            frame: f,
            eye,
            resident: stats.resident,
            considered: stats.considered,
            emitted: stats.emitted,
            draws: draws.len() as u32,
            per_lod: stats.per_lod,
            lod_fading: stats.lod_fading,
            placed_tiles: stream.placed_tiles,
            sync_tiles: stream.sync_tiles,
            replaced_tiles: stream.replaced_tiles,
            pops,
            swaying,
            cpu_us,
        };
        if f > 0 && (stream.sync_tiles > 0 || stream.replaced_tiles > 0) {
            report.hitch_frames += 1;
        }
        report.max_emitted = report.max_emitted.max(stats.emitted);
        report.max_draws = report.max_draws.max(frame.draws);
        report.max_considered = report.max_considered.max(stats.considered);
        emitted_sum += u64::from(stats.emitted);
        report.frames.push(frame);
    }
    report.mean_emitted = if frames_total > 0 {
        emitted_sum as f32 / frames_total as f32
    } else {
        0.0
    };
    report.pops = meter.total();
    report.events = meter.events().to_vec();
    report.switches = meter.switches().to_vec();
    if !times.is_empty() {
        times.sort_unstable();
        let at = |q: f32| times[((times.len() - 1) as f32 * q).round() as usize];
        report.cpu_us = Some([at(0.5), at(0.99), times[times.len() - 1]]);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::forest::shape::ObstacleShape;

    #[test]
    fn standin_trees_are_valid_assets_with_a_trunk_and_a_crown() {
        for tree in [
            StandinTree::broadleaf(),
            StandinTree::conifer(),
            StandinTree::bush(),
        ] {
            let asset = standin_tree(&tree);
            asset.validate("standin").expect("valid asset");
            let shape = ObstacleShape::of_asset(&asset);
            assert!(shape.has_crown(), "{tree:?}: {shape:?}");
            assert!((shape.top - tree.height_m).abs() < 1e-3);
            assert!((shape.crown_radius - tree.crown_radius_m).abs() < 0.05 * tree.crown_radius_m);
            assert!(shape.trunk_radius <= tree.trunk_radius_m + 1e-4);
            assert!(shape.crown_base <= tree.crown_base_m + 0.5, "{tree:?}: {shape:?}");
        }
    }

    #[test]
    fn the_sampler_walks_the_path_by_time_or_speed() {
        let path = CameraPath {
            points: vec![[0.0, 0.0, 0.0], [100.0, 0.0, 0.0], [100.0, 0.0, 100.0]],
            times: None,
        };
        let s = PathSampler::new(&path, 50.0);
        assert!((s.duration() - 4.0).abs() < 1e-5 && (s.length() - 200.0).abs() < 1e-4);
        assert!((s.point_at(s.along_at(1.0))[0] - 50.0).abs() < 1e-4);
        assert_eq!(s.point_at(s.along_at(10.0)), [100.0, 0.0, 100.0]);
        let timed = CameraPath {
            times: Some(vec![0.0, 1.0, 5.0]),
            ..path
        };
        let s = PathSampler::new(&timed, 50.0);
        let p = s.point_at(s.along_at(3.0));
        assert!((p[0] - 100.0).abs() < 1e-4 && (p[2] - 50.0).abs() < 1e-3, "{p:?}");
    }
}
