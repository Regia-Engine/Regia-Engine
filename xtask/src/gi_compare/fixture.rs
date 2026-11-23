// SPDX-License-Identifier: MPL-2.0
//! `*.gi_compare.ron`: a small scene for the baked-light review, described in text and built in
//! code (ADR-314).
//!
//! A fixture names materials, an optional procedural terrain (a grid of plane waves), shapes
//! (boxes, ellipsoids, vertical capsules), a sun and a flat sky, and the probe layout to bake.
//! From it this module builds the raster frame (`LabScene`); the reference tracer's scene is the
//! engine's own extraction of that frame (`regia_pt::extract::from_frame`), so both renderers
//! see the same geometry, materials and lights by construction.
//!
//! Shapes with `baked: false` are dynamic units: drawn and traced, but left out of the bake.
//! A dynamic capsule with `occluder: true` is also handed to the renderer as a capsule occluder
//! (ADR-282), which is the case the capsule fixture checks.

use std::collections::HashMap;

use regia_asset::baked_gi::VolumeLayout;
use regia_core::glam::{Mat4, Quat, Vec3};
use regia_pt::PtHeightfield;
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, detail_brick, terrain_layer};
use regia_render::baked_gi::GiCapsule;
use regia_render::{DirectionalLightUniform, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings};
use serde::{Deserialize, Serialize};

use super::views::{View, WorldBox};

/// The only fixture version this build reads.
pub const FIXTURE_FORMAT_VERSION: u32 = 1;

/// A capsule occluder's radius as a fraction of its shape's: fitted inside the unit's hull, as
/// `book/agents/baked-gi.md` asks hosts to fit theirs.
pub const OCCLUDER_FIT: f32 = 0.9;

/// Most terrain samples per side (a fixture is a small scene).
pub const MAX_TERRAIN_SAMPLES: u32 = 1025;

/// The sun: a delta light (the forward pass's sun).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sun {
    /// Direction toward the sun (normalized here).
    pub toward: [f32; 3],
    /// Linear colour.
    #[serde(default = "white")]
    pub color: [f32; 3],
    /// Illuminance at normal incidence, lux.
    pub lux: f32,
}

fn white() -> [f32; 3] {
    [1.0; 3]
}

/// A material.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialDesc {
    /// Name the shapes and the terrain refer to.
    pub name: String,
    /// Linear albedo.
    pub albedo: [f32; 3],
    /// Perceptual roughness.
    #[serde(default = "default_roughness")]
    pub roughness: f32,
    /// Metalness.
    #[serde(default)]
    pub metallic: f32,
    /// Emitted luminance, cd/m² (baked as the emissive component).
    #[serde(default)]
    pub emissive: [f32; 3],
}

fn default_roughness() -> f32 {
    0.9
}

/// One plane wave of the procedural terrain: `amplitude · cos(2π (x / wavelength.0 + z /
/// wavelength.1) + phase)`; a wavelength ≤ 0 leaves that axis out.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wave {
    /// Height amplitude, metres.
    pub amplitude: f32,
    /// Wavelength along X and Z, metres.
    pub wavelength: [f32; 2],
    /// Phase, radians.
    #[serde(default)]
    pub phase: f32,
}

/// A procedural terrain centred on the origin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainDesc {
    /// Extent along X and Z, metres.
    pub size: [f32; 2],
    /// Sample spacing, metres.
    pub spacing: f32,
    /// Base height, metres.
    #[serde(default)]
    pub height: f32,
    /// Plane waves added to the base height.
    #[serde(default)]
    pub waves: Vec<Wave>,
    /// Material name.
    pub material: String,
}

impl TerrainDesc {
    /// Height at world `(x, z)`.
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        let inv = |l: f32| if l > 0.0 { 1.0 / l } else { 0.0 };
        self.waves.iter().fold(self.height, |h, w| {
            let arg = std::f32::consts::TAU * (x * inv(w.wavelength[0]) + z * inv(w.wavelength[1])) + w.phase;
            h + w.amplitude * arg.cos()
        })
    }

    /// Samples along X and Z.
    #[must_use]
    pub fn samples(&self) -> [u32; 2] {
        [0, 1].map(|a| {
            ((self.size[a] / self.spacing).round() as u32)
                .max(1)
                .saturating_add(1)
        })
    }

    /// World X, Z of sample (0, 0).
    #[must_use]
    pub fn origin(&self) -> [f32; 2] {
        let n = self.samples();
        [0, 1].map(|a| -0.5 * (n[a] - 1) as f32 * self.spacing)
    }
}

/// A shape's geometry, in its local frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ShapeKind {
    /// A box with these half extents.
    Box {
        /// Half extents, metres.
        half: [f32; 3],
    },
    /// An ellipsoid with these radii (a sphere when equal): rocks.
    Ellipsoid {
        /// Radii along the local axes, metres.
        radii: [f32; 3],
    },
    /// A vertical capsule: a segment of `2 · half_height` along local Y swept by `radius`.
    Capsule {
        /// Half the segment length, metres.
        half_height: f32,
        /// Radius, metres.
        radius: f32,
    },
}

/// A placed shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    /// Label (summary and errors).
    pub name: String,
    /// Geometry.
    pub kind: ShapeKind,
    /// Material name.
    pub material: String,
    /// Centre, metres.
    #[serde(default)]
    pub at: [f32; 3],
    /// Rotation about +Y, degrees.
    #[serde(default)]
    pub yaw_deg: f32,
    /// In the bake. `false` = a dynamic unit: drawn and traced, not baked.
    #[serde(default = "yes")]
    pub baked: bool,
    /// A dynamic capsule that is also a capsule occluder over the baked volume (ADR-282).
    #[serde(default)]
    pub occluder: bool,
}

fn yes() -> bool {
    true
}

/// The terrain-following probe layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerDesc {
    /// Column spacing along X and Z, metres.
    pub spacing: [f32; 2],
    /// Probe heights above the ground, metres.
    pub levels: Vec<f32>,
}

/// A detail brick.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrickDesc {
    /// Lower corner.
    pub min: [f32; 3],
    /// Upper corner.
    pub max: [f32; 3],
    /// Probe spacing, metres.
    pub spacing: f32,
}

/// The probe layout to bake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeLayout {
    /// Terrain layer (needs `terrain`).
    #[serde(default)]
    pub terrain: Option<LayerDesc>,
    /// Detail bricks.
    #[serde(default)]
    pub bricks: Vec<BrickDesc>,
    /// Interior boxes: a brick whose centre lies in one is flagged `INTERIOR` (ADR-281).
    #[serde(default)]
    pub interiors: Vec<WorldBox>,
    /// Octahedral moment texels per probe edge.
    #[serde(default = "default_moments")]
    pub moment_resolution: u32,
    /// Moment distance clamp, metres.
    #[serde(default = "default_max_distance")]
    pub max_distance: f32,
}

fn default_moments() -> u32 {
    8
}

fn default_max_distance() -> f32 {
    16.0
}

/// Probe trace settings of the in-process bake.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BakeParams {
    /// Rays per probe per sample.
    pub rays_per_probe: u32,
    /// Samples per probe.
    pub samples: u32,
    /// Path vertices after the probe.
    pub max_bounces: u32,
    /// Seed.
    pub seed: u32,
}

impl Default for BakeParams {
    fn default() -> Self {
        Self {
            rays_per_probe: 256,
            samples: 16,
            max_bounces: 4,
            seed: 7,
        }
    }
}

/// A parsed, validated fixture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// [`FIXTURE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Short name (summary and sheet headers).
    pub name: String,
    /// What the fixture checks, one sentence (printed on the sheet's header line).
    #[serde(default)]
    pub purpose: String,
    /// The sun; `None` = no sun.
    #[serde(default)]
    pub sun: Option<Sun>,
    /// Flat sky illuminance, lux: the raster's flat ambient and a uniform sky of radiance
    /// `sky_lux / π` in the tracer and the bake.
    #[serde(default)]
    pub sky_lux: f32,
    /// Materials.
    pub materials: Vec<MaterialDesc>,
    /// Procedural terrain.
    #[serde(default)]
    pub terrain: Option<TerrainDesc>,
    /// Shapes.
    #[serde(default)]
    pub shapes: Vec<Shape>,
    /// Probe layout.
    pub layout: ProbeLayout,
    /// Bake settings.
    #[serde(default)]
    pub bake: BakeParams,
}

/// Why a fixture was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum FixtureError {
    /// Not RON or not the expected shape.
    Parse(String),
    /// `format_version` is not [`FIXTURE_FORMAT_VERSION`].
    Version(u32),
    /// A shape or the terrain names a material that does not exist, or two materials share a name.
    Material(String),
    /// A shape's dimensions are not positive and finite.
    Shape(String),
    /// The terrain's size or spacing is unusable.
    Terrain(String),
    /// The probe layout is unusable (no probes, a terrain layer without terrain, a bad brick).
    Layout(String),
    /// A light value is negative or not finite.
    Light(String),
}

impl FixtureError {
    /// Stable error code, documented in `book/agents/gi-compare.md`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "fixture_parse",
            Self::Version(_) => "fixture_version",
            Self::Material(_) => "fixture_material",
            Self::Shape(_) => "fixture_shape",
            Self::Terrain(_) => "fixture_terrain",
            Self::Layout(_) => "fixture_layout",
            Self::Light(_) => "fixture_light",
        }
    }
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) => write!(f, "fixture does not parse: {m}"),
            Self::Version(v) => write!(
                f,
                "fixture format_version {v} is not supported (this build reads {FIXTURE_FORMAT_VERSION})"
            ),
            Self::Material(m) => write!(f, "fixture material: {m}"),
            Self::Shape(m) => write!(f, "fixture shape: {m}"),
            Self::Terrain(m) => write!(f, "fixture terrain: {m}"),
            Self::Layout(m) => write!(f, "fixture layout: {m}"),
            Self::Light(m) => write!(f, "fixture light: {m}"),
        }
    }
}

impl std::error::Error for FixtureError {}

fn positive(v: f32) -> bool {
    v.is_finite() && v > 0.0
}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

fn non_negative3(v: [f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite() && *x >= 0.0)
}

/// Parses and validates fixture text.
///
/// # Errors
///
/// The first problem found.
pub fn parse_fixture(text: &str) -> Result<Fixture, FixtureError> {
    let fixture: Fixture = ron::from_str(text).map_err(|e| FixtureError::Parse(e.to_string()))?;
    fixture.validate()?;
    Ok(fixture)
}

impl Fixture {
    /// Checks names, dimensions, lights and the layout (the layout is also built once).
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.format_version != FIXTURE_FORMAT_VERSION {
            return Err(FixtureError::Version(self.format_version));
        }
        let mut names = HashMap::new();
        for (i, m) in self.materials.iter().enumerate() {
            if names.insert(m.name.as_str(), i).is_some() {
                return Err(FixtureError::Material(format!(
                    "two materials are named {:?}",
                    m.name
                )));
            }
            if !non_negative3(m.albedo) || !non_negative3(m.emissive) {
                return Err(FixtureError::Material(format!(
                    "{:?}: albedo and emissive must be finite and >= 0",
                    m.name
                )));
            }
        }
        if let Some(sun) = &self.sun
            && (!finite3(sun.toward) || !non_negative3(sun.color) || !(sun.lux.is_finite() && sun.lux >= 0.0))
        {
            return Err(FixtureError::Light(
                "the sun needs a finite direction, colour and lux >= 0".into(),
            ));
        }
        if !(self.sky_lux.is_finite() && self.sky_lux >= 0.0) {
            return Err(FixtureError::Light("sky_lux must be finite and >= 0".into()));
        }
        if let Some(t) = &self.terrain {
            if !names.contains_key(t.material.as_str()) {
                return Err(FixtureError::Material(format!(
                    "the terrain names unknown material {:?}",
                    t.material
                )));
            }
            let n = if positive(t.spacing) && positive(t.size[0]) && positive(t.size[1]) {
                t.samples()
            } else {
                [0, 0]
            };
            if n.iter().any(|&s| !(2..=MAX_TERRAIN_SAMPLES).contains(&s)) {
                return Err(FixtureError::Terrain(format!(
                    "size and spacing must be positive and give 2..={MAX_TERRAIN_SAMPLES} samples per side"
                )));
            }
        }
        for s in &self.shapes {
            if !names.contains_key(s.material.as_str()) {
                return Err(FixtureError::Material(format!(
                    "shape {:?} names unknown material {:?}",
                    s.name, s.material
                )));
            }
            let ok = match s.kind {
                ShapeKind::Box { half } => half.iter().all(|v| positive(*v)),
                ShapeKind::Ellipsoid { radii } => radii.iter().all(|v| positive(*v)),
                ShapeKind::Capsule { half_height, radius } => {
                    half_height.is_finite() && half_height >= 0.0 && positive(radius)
                }
            };
            if !ok || !finite3(s.at) || !s.yaw_deg.is_finite() {
                return Err(FixtureError::Shape(format!(
                    "{:?}: dimensions must be positive and the placement finite",
                    s.name
                )));
            }
            if s.occluder && !matches!(s.kind, ShapeKind::Capsule { .. }) {
                return Err(FixtureError::Shape(format!(
                    "{:?}: only a capsule can be an occluder",
                    s.name
                )));
            }
            if s.occluder && s.baked {
                return Err(FixtureError::Shape(format!(
                    "{:?}: an occluder is a dynamic unit; set baked: false",
                    s.name
                )));
            }
        }
        self.volume_layout().map(|_| ())
    }

    /// Whether any material emits light (the reference then traces a second, emitter-free
    /// direct pass to isolate indirect light; ADR-314).
    #[must_use]
    pub fn has_emitters(&self) -> bool {
        self.materials.iter().any(|m| m.emissive.iter().any(|&e| e > 0.0))
    }

    /// The terrain as the tracer's heightfield (for the probe layer's ground heights).
    #[must_use]
    pub fn heightfield(&self) -> Option<PtHeightfield> {
        let t = self.terrain.as_ref()?;
        let n = t.samples();
        let o = t.origin();
        let mut heights = Vec::with_capacity((n[0] * n[1]) as usize);
        for j in 0..n[1] {
            for i in 0..n[0] {
                heights.push(t.height_at(o[0] + i as f32 * t.spacing, o[1] + j as f32 * t.spacing));
            }
        }
        Some(PtHeightfield {
            width: n[0],
            depth: n[1],
            origin: o,
            spacing: [t.spacing; 2],
            heights,
            material: 0,
        })
    }

    /// The probe layout: terrain layer, bricks, interior flags, offsets assigned.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Layout`] when there are no probes, a terrain layer has no terrain, or a
    /// brick is empty or not finite.
    pub fn volume_layout(&self) -> Result<VolumeLayout, FixtureError> {
        let l = &self.layout;
        let terrain = match &l.terrain {
            None => None,
            Some(layer) => {
                let hf = self
                    .heightfield()
                    .ok_or_else(|| FixtureError::Layout("a terrain layer needs a terrain".into()))?;
                Some(
                    terrain_layer(&hf, layer.spacing, &layer.levels)
                        .map_err(|e| FixtureError::Layout(e.to_string()))?,
                )
            }
        };
        let mut bricks = Vec::with_capacity(l.bricks.len());
        for b in &l.bricks {
            let ok = finite3(b.min)
                && finite3(b.max)
                && (0..3).all(|a| b.min[a] < b.max[a])
                && positive(b.spacing);
            if !ok {
                return Err(FixtureError::Layout(format!(
                    "brick {:?}..{:?}: min must be below max and the spacing positive",
                    b.min, b.max
                )));
            }
            bricks.push(detail_brick(b.min, b.max, b.spacing));
        }
        if !(1..=32).contains(&l.moment_resolution) || !positive(l.max_distance) {
            return Err(FixtureError::Layout(
                "moment_resolution must be 1..=32 and max_distance positive".into(),
            ));
        }
        let mut layout = VolumeLayout {
            terrain,
            bricks,
            moment_resolution: l.moment_resolution,
            max_distance: l.max_distance,
        };
        let boxes: Vec<([f32; 3], [f32; 3])> = l.interiors.iter().map(|b| (b.min, b.max)).collect();
        layout.flag_interior_bricks(&boxes);
        layout.assign_brick_offsets();
        if layout.probe_count() == 0 {
            return Err(FixtureError::Layout("the layout has no probes".into()));
        }
        Ok(layout)
    }

    /// The bake settings.
    #[must_use]
    pub fn bake_settings(&self) -> GiBakeSettings {
        GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: self.bake.rays_per_probe.max(16),
                samples: self.bake.samples.max(1),
                max_bounces: self.bake.max_bounces.max(1),
                seed: self.bake.seed,
                samples_per_submit: 8,
                moment_resolution: self.layout.moment_resolution,
                max_distance: self.layout.max_distance,
                ..ProbeTraceSettings::default()
            },
            ..GiBakeSettings::default()
        }
    }

    /// The capsule occluders of the dynamic capsules marked `occluder`, shrunk to
    /// [`OCCLUDER_FIT`] of their radius (fitted inside the hull, ADR-282).
    #[must_use]
    pub fn occluders(&self) -> Vec<GiCapsule> {
        self.shapes
            .iter()
            .filter_map(|s| match s.kind {
                ShapeKind::Capsule { half_height, radius } if s.occluder && !s.baked => {
                    let [x, y, z] = s.at;
                    Some(GiCapsule::new(
                        [x, y - half_height, z],
                        [x, y + half_height, z],
                        radius * OCCLUDER_FIT,
                    ))
                }
                _ => None,
            })
            .collect()
    }

    /// The raster frame of `view`: every shape (`with_dynamic`) or only the baked ones, the flat
    /// sky as ambient (`with_sky`) or none (the direct-only frame).
    #[must_use]
    pub fn lab_scene(&self, frame: &FrameParams<'_>, with_dynamic: bool, with_sky: bool) -> LabScene {
        let index: HashMap<&str, usize> = self
            .materials
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.as_str(), i))
            .collect();
        let materials = self
            .materials
            .iter()
            .map(|m| LabMaterial {
                base_color: [m.albedo[0], m.albedo[1], m.albedo[2], 1.0],
                roughness: m.roughness,
                metallic: m.metallic,
                emissive: m.emissive,
                emissive_intensity: 1.0,
                ..LabMaterial::default()
            })
            .collect();
        let mut meshes = Vec::new();
        if let Some(t) = &self.terrain {
            let (vertices, indices) = terrain_mesh(t);
            meshes.push(LabMesh {
                vertices,
                indices,
                material: index.get(t.material.as_str()).copied().unwrap_or(0),
                ..LabMesh::unskinned()
            });
        }
        for s in self.shapes.iter().filter(|s| s.baked || with_dynamic) {
            let (vertices, indices) = shape_mesh(s.kind);
            let model = Mat4::from_rotation_translation(
                Quat::from_rotation_y(s.yaw_deg.to_radians()),
                Vec3::from_array(s.at),
            );
            meshes.push(LabMesh {
                vertices,
                indices,
                material: index.get(s.material.as_str()).copied().unwrap_or(0),
                model: model.to_cols_array_2d(),
                ..LabMesh::unskinned()
            });
        }
        let directional = match &self.sun {
            Some(sun) => DirectionalLightUniform::new(
                [-sun.toward[0], -sun.toward[1], -sun.toward[2]],
                sun.color,
                sun.lux,
            ),
            None => DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        };
        let mut post = PostSettings::golden_deterministic();
        post.exposure_ev = frame.ev100;
        LabScene {
            width: frame.size.0,
            height: frame.size.1,
            eye: frame.view.eye,
            target: frame.view.target,
            fov_y: frame.view.fov_y_deg.to_radians(),
            near: 0.05,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: frame.ev100,
            ambient_lux: if with_sky { self.sky_lux } else { 0.0 },
            double_sided: false,
            post: Some(post),
            directional,
            local_lights: Vec::new(),
            materials,
            meshes,
        }
    }
}

/// The camera and output of one rendered view.
#[derive(Clone, Copy, Debug)]
pub struct FrameParams<'a> {
    /// The camera.
    pub view: &'a View,
    /// Output width and height.
    pub size: (u32, u32),
    /// Exposure the frame's own post stack uses (the tiles are display-encoded from the linear
    /// capture at the view set's exposures, so this only affects the frame's 8-bit output).
    pub ev100: f32,
}

fn vertex(p: Vec3, n: Vec3, uv: [f32; 2]) -> LabVertex {
    LabVertex::from_pos_normal_uv(p.to_array(), n.normalize_or(Vec3::Y).to_array(), uv)
}

/// The terrain as a grid mesh, counter-clockwise seen from above, normals from central
/// differences of the height function.
#[must_use]
pub fn terrain_mesh(t: &TerrainDesc) -> (Vec<LabVertex>, Vec<u32>) {
    let n = t.samples();
    let o = t.origin();
    let e = t.spacing * 0.5;
    let mut vertices = Vec::with_capacity((n[0] * n[1]) as usize);
    for j in 0..n[1] {
        for i in 0..n[0] {
            let (x, z) = (o[0] + i as f32 * t.spacing, o[1] + j as f32 * t.spacing);
            let dx = (t.height_at(x + e, z) - t.height_at(x - e, z)) / (2.0 * e);
            let dz = (t.height_at(x, z + e) - t.height_at(x, z - e)) / (2.0 * e);
            vertices.push(vertex(
                Vec3::new(x, t.height_at(x, z), z),
                Vec3::new(-dx, 1.0, -dz),
                [i as f32 / (n[0] - 1) as f32, j as f32 / (n[1] - 1) as f32],
            ));
        }
    }
    let mut indices = Vec::with_capacity(((n[0] - 1) * (n[1] - 1) * 6) as usize);
    for j in 0..n[1] - 1 {
        for i in 0..n[0] - 1 {
            let p00 = j * n[0] + i;
            let p10 = p00 + 1;
            let p01 = p00 + n[0];
            let p11 = p01 + 1;
            indices.extend_from_slice(&[p00, p01, p10, p10, p01, p11]);
        }
    }
    (vertices, indices)
}

/// Rows and columns of the sphere-like meshes.
const SEGMENTS: u32 = 48;
const RINGS: u32 = 24;

/// A shape's mesh in its local frame (outward normals, the engine's winding).
#[must_use]
pub fn shape_mesh(kind: ShapeKind) -> (Vec<LabVertex>, Vec<u32>) {
    match kind {
        ShapeKind::Box { half } => regia_render::box_mesh(half),
        ShapeKind::Ellipsoid { radii } => {
            let (mut vertices, indices) = regia_render::uv_sphere(1.0, SEGMENTS, RINGS);
            let r = Vec3::from_array(radii);
            for v in &mut vertices {
                let unit = Vec3::from_array(v.position);
                v.position = (unit * r).to_array();
                v.normal = (unit / r).normalize_or(Vec3::Y).to_array();
            }
            (vertices, indices)
        }
        ShapeKind::Capsule { half_height, radius } => capsule_mesh(half_height, radius),
    }
}

/// A vertical capsule: two hemispheres `±half_height` apart joined by the cylinder between
/// their equators. Rows run from the top pole down, as `regia_render::uv_sphere`, with the
/// same index order, so the winding matches the engine's spheres.
#[must_use]
pub fn capsule_mesh(half_height: f32, radius: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let half_rings = RINGS / 2;
    let rows = 2 * half_rings + 2;
    let mut vertices = Vec::with_capacity((rows * (SEGMENTS + 1)) as usize);
    for row in 0..rows {
        let (phi, offset) = if row <= half_rings {
            (
                row as f32 / half_rings as f32 * std::f32::consts::FRAC_PI_2,
                half_height,
            )
        } else {
            let k = row - half_rings - 1;
            (
                std::f32::consts::FRAC_PI_2 + k as f32 / half_rings as f32 * std::f32::consts::FRAC_PI_2,
                -half_height,
            )
        };
        for seg in 0..=SEGMENTS {
            let u = seg as f32 / SEGMENTS as f32;
            let theta = u * std::f32::consts::TAU;
            let n = Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
            vertices.push(vertex(
                n * radius + Vec3::new(0.0, offset, 0.0),
                n,
                [u, row as f32 / (rows - 1) as f32],
            ));
        }
    }
    let stride = SEGMENTS + 1;
    let mut indices = Vec::with_capacity(((rows - 1) * SEGMENTS * 6) as usize);
    for row in 0..rows - 1 {
        for seg in 0..SEGMENTS {
            let i0 = row * stride + seg;
            let i1 = i0 + 1;
            let i2 = i0 + stride;
            let i3 = i2 + 1;
            indices.extend_from_slice(&[i0, i1, i2, i1, i3, i2]);
        }
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"(
        format_version: 1,
        name: "small",
        sun: Some((toward: (0.3, 0.8, 0.2), lux: 50000.0)),
        sky_lux: 10000.0,
        materials: [
            (name: "ground", albedo: (0.3, 0.3, 0.3)),
            (name: "unit", albedo: (0.5, 0.5, 0.5)),
        ],
        terrain: Some((size: (16.0, 16.0), spacing: 1.0, material: "ground",
            waves: [(amplitude: 0.5, wavelength: (8.0, 0.0))])),
        shapes: [
            (name: "rock", kind: Ellipsoid(radii: (1.0, 0.6, 0.8)), material: "ground", at: (2.0, 0.3, 0.0)),
            (name: "unit", kind: Capsule(half_height: 0.5, radius: 0.4), material: "unit",
                at: (-2.0, 1.0, 0.0), baked: false, occluder: true),
        ],
        layout: (
            terrain: Some((spacing: (4.0, 4.0), levels: [0.5, 2.0])),
            bricks: [(min: (1.0, 0.0, -1.0), max: (3.0, 1.5, 1.0), spacing: 0.5)],
        ),
    )"#;

    #[test]
    fn a_fixture_builds_its_frames_layout_and_occluders() {
        let fx = parse_fixture(SMALL).expect("parses");
        let layout = fx.volume_layout().expect("layout");
        let terrain = layout.terrain.as_ref().expect("terrain layer");
        assert_eq!(terrain.levels, vec![0.5, 2.0]);
        assert_eq!(layout.bricks.len(), 1);
        assert!(layout.probe_count() > 0);
        let occ = fx.occluders();
        assert_eq!(occ.len(), 1);
        assert_eq!(occ[0].a, [-2.0, 0.5, 0.0]);
        assert_eq!(occ[0].b, [-2.0, 1.5, 0.0]);
        assert!((occ[0].radius - 0.4 * OCCLUDER_FIT).abs() < 1e-6);
        let view = View {
            name: "v".into(),
            eye: [0.0, 3.0, 10.0],
            target: [0.0, 0.0, 0.0],
            fov_y_deg: 50.0,
            interior: None,
            leak_threshold: None,
            ev100: None,
            indirect_ev100: None,
        };
        let frame = FrameParams {
            view: &view,
            size: (32, 24),
            ev100: 13.0,
        };
        let full = fx.lab_scene(&frame, true, true);
        let baked_only = fx.lab_scene(&frame, false, true);
        let direct = fx.lab_scene(&frame, true, false);
        assert_eq!(full.meshes.len(), 3, "terrain, rock and unit");
        assert_eq!(baked_only.meshes.len(), 2, "the dynamic unit is not baked");
        assert_eq!(full.ambient_lux, 10000.0);
        assert_eq!(direct.ambient_lux, 0.0);
        assert!((full.directional.direction[1] + 0.8 / (0.3f32 * 0.3 + 0.64 + 0.04).sqrt()).abs() < 1e-5);
        assert!(!fx.has_emitters());
    }

    #[test]
    fn terrain_mesh_faces_up_and_follows_the_waves() {
        let t = TerrainDesc {
            size: [4.0, 2.0],
            spacing: 1.0,
            height: 1.0,
            waves: vec![Wave {
                amplitude: 0.25,
                wavelength: [4.0, 0.0],
                phase: 0.0,
            }],
            material: "g".into(),
        };
        assert_eq!(t.samples(), [5, 3]);
        assert_eq!(t.origin(), [-2.0, -1.0]);
        let (v, idx) = terrain_mesh(&t);
        assert_eq!(v.len(), 15);
        assert_eq!(idx.len(), 4 * 2 * 6);
        for tri in idx.as_chunks::<3>().0 {
            let p = |i: u32| Vec3::from_array(v[i as usize].position);
            let n = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
            assert!(n.y > 0.0, "counter-clockwise from above: {n}");
        }
        assert!((t.height_at(0.0, 0.0) - 1.25).abs() < 1e-6);
        assert!((t.height_at(2.0, 0.0) - 0.75).abs() < 1e-5);
    }

    #[test]
    fn capsule_normals_point_out_and_triangles_face_out() {
        let (v, idx) = capsule_mesh(0.5, 0.3);
        for vert in &v {
            let p = Vec3::from_array(vert.position);
            let axis = Vec3::new(0.0, p.y.clamp(-0.5, 0.5), 0.0);
            let out = (p - axis).normalize_or(Vec3::Y);
            assert!(Vec3::from_array(vert.normal).dot(out) > 0.99, "{p}");
            assert!(((p - axis).length() - 0.3).abs() < 1e-4);
        }
        let mut outward = 0usize;
        let mut total = 0usize;
        for tri in idx.as_chunks::<3>().0 {
            let p = |i: u32| Vec3::from_array(v[i as usize].position);
            let n = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
            if n.length_squared() < 1e-12 {
                continue; // a pole's collapsed triangle
            }
            let c = (p(tri[0]) + p(tri[1]) + p(tri[2])) / 3.0;
            let axis = Vec3::new(0.0, c.y.clamp(-0.5, 0.5), 0.0);
            total += 1;
            if n.dot(c - axis) > 0.0 {
                outward += 1;
            }
        }
        assert_eq!(outward, total, "every triangle faces out");
    }

    #[test]
    fn bad_fixtures_are_typed_errors() {
        let cases = [
            (
                SMALL.replace("material: \"unit\",", "material: \"nope\","),
                "fixture_material",
            ),
            (SMALL.replace("radius: 0.4", "radius: 0.0"), "fixture_shape"),
            (
                SMALL.replace("baked: false, occluder: true", "occluder: true"),
                "fixture_shape",
            ),
            (
                SMALL.replace("spacing: 1.0, material", "spacing: 0.0, material"),
                "fixture_terrain",
            ),
            (SMALL.replace("spacing: 0.5)", "spacing: -1.0)"), "fixture_layout"),
            (
                SMALL.replace("format_version: 1", "format_version: 9"),
                "fixture_version",
            ),
            (SMALL.replace("lux: 50000.0", "lux: -1.0"), "fixture_light"),
            (SMALL.replace("sky_lux", "sky_luxx"), "fixture_parse"),
        ];
        for (text, code) in cases {
            let err = parse_fixture(&text).expect_err(code);
            assert_eq!(err.code(), code, "{err}");
        }
        let mut no_terrain = parse_fixture(SMALL).expect("parses");
        no_terrain.terrain = None;
        assert_eq!(
            no_terrain
                .validate()
                .expect_err("a terrain layer needs a terrain")
                .code(),
            "fixture_layout"
        );
    }
}
