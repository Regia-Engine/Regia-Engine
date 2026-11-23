// SPDX-License-Identifier: MPL-2.0
//! Scene export: a level, a prefab or a list of entities as one `.glb` for a DCC tool
//! (`docs/specs/22`, T2.14, ADR-2554).
//!
//! The export walks the scene file's entities (no engine world needed): the node hierarchy with
//! its transforms, the meshes behind `MeshSource` (the source glTF/GLB, or a built-in
//! primitive) with normals, tangents, `TEXCOORD_0/1` and vertex colors, metallic-roughness
//! materials with their textures embedded as PNG or JPEG, point/spot/directional lights
//! (`KHR_lights_punctual`) and cameras. Skinned meshes and animation clips come with
//! [`ExportOptions::skins`] and [`ExportOptions::animations`]. A mesh or texture whose source file
//! is gone comes from the project's cook instead (`cooked`, ADR-3016).
//!
//! # Axes
//!
//! Regia Engine looks down −Z; glTF assets face +Z (ADR-009). The importer bakes a 180° turn about
//! +Y above the scene. The exporter is its exact inverse under [`Forward::Convert`] (the
//! default): one root node `Regia_to_glTF` carries that turn, and mesh vertices read from a
//! source glTF are written as the importer would have cooked them (turned about +Y), so a prop
//! placed at the origin comes out as the artist authored it. A re-import is the identity.
//! [`Forward::Keep`] writes the scene in Regia Engine axes with no root node.
//!
//! # Determinism
//!
//! Entities, materials, images and meshes are emitted in scene-file order with ordered maps; PNG
//! encoding uses the `image` crate's fixed settings. The same scene and assets give identical
//! bytes. Original code from the glTF 2.0 specification (Khronos), `KHR_lights_punctual` and
//! `KHR_texture_transform`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use regia_asset::{MaterialAssetRon, MetaSidecar, TextureSlot};
use regia_core::id::Guid;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_scene::format::{SceneEntity, SceneFile};
use serde::Deserialize;
use serde_json::Value;

use crate::Forward;
use crate::gltf_write::{
    GlbCamera, GlbExtras, GlbImage, GlbLight, GlbLightKind, GlbMaterial, GlbMesh, GlbNode, GlbPrimitive,
    GlbProjection, GlbScene, GlbTexSlot, GlbTexTransform, GlbTextureRef,
};

mod cooked;
mod skin;

/// Type paths of the components the export reads (current spelling; older spellings go through
/// `regia_core::brand::current_type_path`).
const TRANSFORM: &str = "regia_scene::transform::Transform";
const MESH_SOURCE: &str = "regia_render::runtime::MeshSource";
const POINT_LIGHT: &str = "regia_render::lights::PointLight";
const SPOT_LIGHT: &str = "regia_render::lights::SpotLight";
const DIRECTIONAL_LIGHT: &str = "regia_render::lights::DirectionalLight";
const CAMERA: &str = "regia_render::camera::Camera";

/// Name of the root node that turns Regia Engine's −Z forward into glTF's +Z (and back).
pub const AXIS_NODE_NAME: &str = "Regia_to_glTF";

/// What to export and how.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// Entities to export, with all their descendants. `None` exports every entity. An
    /// exported entity whose parent is not exported keeps its world transform.
    pub entities: Option<Vec<Guid>>,
    /// The axis convention ([`Forward::Convert`] by default; see the module docs).
    pub forward: Forward,
    /// Export skins (joints, weights, inverse bind matrices) of skinned source meshes.
    pub skins: bool,
    /// Export the animation clips of the source meshes that drive exported nodes (needs
    /// [`Self::skins`] or a rigid animated source).
    pub animations: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            entities: None,
            forward: Forward::Convert,
            skins: false,
            animations: false,
        }
    }
}

/// Counts and notes of one export.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportReport {
    /// Nodes written (the axis node included).
    pub nodes: usize,
    /// Meshes written.
    pub meshes: usize,
    /// Triangles written.
    pub triangles: usize,
    /// Materials written.
    pub materials: usize,
    /// Images embedded.
    pub images: usize,
    /// `KHR_lights_punctual` lights written.
    pub lights: usize,
    /// Cameras written.
    pub cameras: usize,
    /// Skins written.
    pub skins: usize,
    /// Animation clips written.
    pub animations: usize,
    /// Size of the `.glb`.
    pub bytes: usize,
    /// What was left out or approximated, in scene order. Each line starts with a code.
    pub warnings: Vec<String>,
}

/// Why an export failed. [`Self::code`] is the stable agent error code.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// A scene file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
    /// A scene file did not parse or migrate.
    #[error("cannot parse {path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser message.
        message: String,
    },
    /// A selected entity is not in the scene.
    #[error("entity {0} is not in the scene")]
    UnknownEntity(Guid),
    /// The scene has no entities to export.
    #[error("nothing to export: the scene or the selection has no entities")]
    NothingToExport,
    /// The `.glb` could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
    /// A mesh, morph target or animation held a NaN or infinite float, which serde_json 1.0.151
    /// would write as `null` — an invalid glTF (asset_pipeline3 correctness audit, finding 1).
    #[error("the scene has a non-finite float: {0}")]
    NonFinite(String),
}

impl ExportError {
    /// The agent error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } => "glb_read_failed",
            Self::Parse { .. } => "glb_parse_failed",
            Self::UnknownEntity(_) => "glb_unknown_entity",
            Self::NothingToExport => "glb_nothing_to_export",
            Self::Write { .. } => "glb_write_failed",
            Self::NonFinite(_) => "glb_non_finite",
        }
    }
}

/// Reads a `.level.ron`, a `.level.d` directory or a `.prefab.ron` into a migrated
/// [`SceneFile`].
///
/// # Errors
///
/// [`ExportError::Read`] or [`ExportError::Parse`].
pub fn read_scene(path: &Path) -> Result<SceneFile, ExportError> {
    let parse = |message: String| ExportError::Parse {
        path: path.to_path_buf(),
        message,
    };
    let mut file = if path.is_dir() {
        regia_scene::level_dir::read_level_dir(path)
            .map_err(|e| parse(e.to_string()))?
            .0
    } else {
        let text = std::fs::read_to_string(path).map_err(|source| ExportError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        SceneFile::from_ron_str(&text).map_err(|e| parse(e.to_string()))?
    };
    regia_scene::format::migrate_to_latest(&mut file).map_err(|e| parse(e.to_string()))?;
    Ok(file)
}

/// Exports `scene` to GLB bytes. `project_root` resolves the `assets/...` paths the scene names
/// (meshes, materials, textures, prefabs).
///
/// # Errors
///
/// [`ExportError::UnknownEntity`], [`ExportError::NothingToExport`] or
/// [`ExportError::NonFinite`].
pub fn export_scene(
    scene: &SceneFile,
    project_root: &Path,
    options: &ExportOptions,
) -> Result<(Vec<u8>, ExportReport), ExportError> {
    let _span = tracing::info_span!("scene_export", entities = scene.entities.len()).entered();
    let mut exporter = Exporter::new(project_root, options);
    let entities = exporter.expand_prefabs(&scene.entities);
    let (glb, report) = exporter.run(&entities)?;
    Ok((glb, report))
}

/// [`export_scene`] on a file path, written atomically to `out`.
///
/// # Errors
///
/// Any [`ExportError`].
pub fn export_file(
    scene_path: &Path,
    project_root: &Path,
    out: &Path,
    options: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    let scene = read_scene(scene_path)?;
    let (glb, report) = export_scene(&scene, project_root, options)?;
    write_glb(out, &glb)?;
    Ok(report)
}

/// Writes `bytes` to `out` through a temp file and a rename, creating parent directories.
///
/// # Errors
///
/// [`ExportError::Write`].
pub fn write_glb(out: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    let fail = |source| ExportError::Write {
        path: out.to_path_buf(),
        source,
    };
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    crate::cook_ledger::write_atomic(out, bytes).map_err(fail)
}

// ---------------------------------------------------------------------------------------------
// Component data
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(default)]
struct TransformRon {
    translation: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
}

impl Default for TransformRon {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

impl TransformRon {
    fn matrix(&self) -> Mat4 {
        let q = Quat::from_xyzw(
            self.rotation[0],
            self.rotation[1],
            self.rotation[2],
            self.rotation[3],
        );
        let q = if q.is_finite() && q.length_squared() > 1e-12 {
            q.normalize()
        } else {
            Quat::IDENTITY
        };
        let finite3 = |v: [f32; 3], fallback: f32| {
            Vec3::from_array(v.map(|c| if c.is_finite() { c } else { fallback }))
        };
        Mat4::from_scale_rotation_translation(finite3(self.scale, 1.0), q, finite3(self.translation, 0.0))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MeshSourceRon {
    mesh: String,
    material: String,
}

#[derive(Deserialize)]
#[serde(default)]
struct PointRon {
    color: [f32; 3],
    intensity: f32,
    radius: f32,
}

impl Default for PointRon {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 800.0,
            radius: 10.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct SpotRon {
    color: [f32; 3],
    intensity: f32,
    outer_angle: f32,
    inner_angle: f32,
    range: f32,
}

impl Default for SpotRon {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 800.0,
            outer_angle: std::f32::consts::FRAC_PI_4,
            inner_angle: std::f32::consts::FRAC_PI_6,
            range: 10.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct DirectionalRon {
    direction: [f32; 3],
    color: [f32; 3],
    illuminance: f32,
}

impl Default for DirectionalRon {
    fn default() -> Self {
        Self {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 100_000.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct CameraRon {
    fov_y: f32,
    ortho_size: f32,
    orthographic: bool,
    near: f32,
    far: f32,
}

impl Default for CameraRon {
    fn default() -> Self {
        Self {
            fov_y: std::f32::consts::FRAC_PI_4,
            ortho_size: 5.0,
            orthographic: false,
            near: 0.1,
            far: 1000.0,
        }
    }
}

/// One entity as the exporter sees it: components as RON text under their current type path.
#[derive(Clone)]
struct Ent {
    guid: Guid,
    name: String,
    parent: Option<Guid>,
    components: BTreeMap<String, String>,
}

impl Ent {
    fn from_scene(entity: &SceneEntity) -> Self {
        let components = entity
            .components
            .iter()
            .map(|(key, raw)| {
                (
                    regia_core::brand::current_type_path(key).into_owned(),
                    raw.get_ron().to_owned(),
                )
            })
            .collect();
        Self {
            guid: entity.guid,
            name: entity.name.clone().unwrap_or_else(|| entity.guid.to_string()),
            parent: entity.parent,
            components,
        }
    }

    fn parse<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Option<Result<T, String>> {
        self.components
            .get(path)
            .map(|text| ron::from_str::<T>(text).map_err(|e| e.to_string()))
    }
}

// ---------------------------------------------------------------------------------------------
// Source meshes
// ---------------------------------------------------------------------------------------------

/// A source glTF/GLB read once: document, buffers, decoded images and the raw JSON (for the
/// extensions the `gltf` crate build here does not model).
pub(crate) struct SourceModel {
    pub key: String,
    pub document: gltf::Document,
    pub buffers: Vec<gltf::buffer::Data>,
    pub images: Vec<gltf::image::Data>,
    pub json: Value,
    /// What opening the file found (`EXT_mesh_gpu_instancing` transforms).
    pub load_data: crate::gltf_normalize::GltfLoadData,
}

fn load_source(path: &Path, key: &str) -> Result<SourceModel, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let crate::gltf_import::GltfOpened {
        document,
        buffers,
        images,
        load_data,
    } = crate::gltf_import::open_gltf(path).map_err(|e| e.to_string())?;
    let json = gltf::Glb::from_slice(&bytes)
        .ok()
        .and_then(|glb| serde_json::from_slice::<Value>(&glb.json).ok())
        .or_else(|| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    Ok(SourceModel {
        key: key.to_owned(),
        document,
        buffers,
        images,
        json,
        load_data,
    })
}

/// A relative path under the project, without `..`, a root or a drive.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if path.is_empty() || p.is_absolute() || path.contains('\\') || path.contains(':') {
        return None;
    }
    p.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then(|| p.to_path_buf())
}

/// PNG bytes of a decoded glTF image (`None` for a float format).
fn png_of(data: &gltf::image::Data) -> Option<Vec<u8>> {
    use gltf::image::Format as F;
    use image::{DynamicImage, ImageBuffer};
    let (w, h) = (data.width, data.height);
    let u16s = |bytes: &[u8]| -> Vec<u16> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect()
    };
    let image = match data.format {
        F::R8 => DynamicImage::ImageLuma8(ImageBuffer::from_raw(w, h, data.pixels.clone())?),
        F::R8G8 => DynamicImage::ImageLumaA8(ImageBuffer::from_raw(w, h, data.pixels.clone())?),
        F::R8G8B8 => DynamicImage::ImageRgb8(ImageBuffer::from_raw(w, h, data.pixels.clone())?),
        F::R8G8B8A8 => DynamicImage::ImageRgba8(ImageBuffer::from_raw(w, h, data.pixels.clone())?),
        F::R16 => DynamicImage::ImageLuma16(ImageBuffer::from_raw(w, h, u16s(&data.pixels))?),
        F::R16G16 => DynamicImage::ImageLumaA16(ImageBuffer::from_raw(w, h, u16s(&data.pixels))?),
        F::R16G16B16 => DynamicImage::ImageRgb16(ImageBuffer::from_raw(w, h, u16s(&data.pixels))?),
        F::R16G16B16A16 => DynamicImage::ImageRgba16(ImageBuffer::from_raw(w, h, u16s(&data.pixels))?),
        _ => return None,
    };
    encode_png(&image)
}

fn encode_png(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// One source primitive in the exporter's axes.
pub(crate) struct SrcPrim {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub tangents: Vec<[f32; 4]>,
    pub uvs: Vec<Vec<[f32; 2]>>,
    pub colors: Vec<[f32; 4]>,
    /// `JOINTS_n` sets, parallel to `weights` (skinned primitives only).
    pub joints: Vec<Vec<[u16; 4]>>,
    /// `WEIGHTS_n` sets.
    pub weights: Vec<Vec<[f32; 4]>>,
    pub indices: Vec<u32>,
    /// Source material index, `None` for the default material.
    pub material: Option<usize>,
}

/// The matrix applied to source geometry above the scene: the importer's turn of 180 degrees about
/// +Y as the exact diagonal matrix (-1, 1, -1) under [`Forward::Convert`].
pub(crate) fn axis_of(forward: Forward) -> Mat4 {
    match forward {
        Forward::Convert => Mat4::from_scale(Vec3::new(-1.0, 1.0, -1.0)),
        Forward::Keep => Mat4::IDENTITY,
    }
}

/// Reads one primitive with `world` applied to positions, normals and tangents (a negative
/// determinant flips the winding and the tangent handedness). `with_skin` keeps the joint
/// influences. `None` (with a warning) for a primitive the export cannot write.
pub(crate) fn read_prim(
    src: &SourceModel,
    mesh_name: &str,
    primitive: &gltf::Primitive,
    world: Mat4,
    with_skin: bool,
    warnings: &mut Vec<String>,
) -> Option<SrcPrim> {
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        warnings.push(format!(
            "primitive_mode_skipped: {} mesh `{mesh_name}` uses {:?}",
            src.key,
            primitive.mode()
        ));
        return None;
    }
    let normal_matrix = world.inverse().transpose();
    let flips = world.determinant() < 0.0;
    let reader = primitive.reader(|b| src.buffers.get(b.index()).map(|d| d.0.as_slice()));
    let positions: Vec<[f32; 3]> = reader
        .read_positions()?
        .map(|p| world.transform_point3(Vec3::from_array(p)).to_array())
        .collect();
    let normals: Vec<[f32; 3]> = reader
        .read_normals()
        .map(|n| {
            n.map(|n| {
                normal_matrix
                    .transform_vector3(Vec3::from_array(n))
                    .normalize_or_zero()
                    .to_array()
            })
            .collect()
        })
        .unwrap_or_default();
    let tangents: Vec<[f32; 4]> = reader
        .read_tangents()
        .map(|t| {
            t.map(|t| {
                let v = world
                    .transform_vector3(Vec3::new(t[0], t[1], t[2]))
                    .normalize_or_zero();
                let w = if flips { -t[3] } else { t[3] };
                [v.x, v.y, v.z, w]
            })
            .collect()
        })
        .unwrap_or_default();
    let mut uvs = Vec::new();
    for set in 0..2 {
        if let Some(t) = reader.read_tex_coords(set) {
            uvs.push(t.into_f32().collect::<Vec<_>>());
        }
    }
    let colors: Vec<[f32; 4]> = reader
        .read_colors(0)
        .map(|c| c.into_rgba_f32().collect())
        .unwrap_or_default();
    let (mut joints, mut weights) = (Vec::new(), Vec::new());
    if with_skin {
        for set in 0..2 {
            if let (Some(j), Some(w)) = (reader.read_joints(set), reader.read_weights(set)) {
                joints.push(j.into_u16().collect::<Vec<_>>());
                weights.push(w.into_f32().collect::<Vec<_>>());
            }
        }
    }
    let mut indices: Vec<u32> = match reader.read_indices() {
        Some(i) => i.into_u32().collect(),
        None => (0..positions.len() as u32).collect(),
    };
    if flips {
        for tri in indices.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
    }
    if indices.iter().any(|i| *i as usize >= positions.len()) {
        warnings.push(format!(
            "primitive_skipped: {} has an index out of range",
            src.key
        ));
        return None;
    }
    Some(SrcPrim {
        positions,
        normals,
        tangents,
        uvs,
        colors,
        joints,
        weights,
        indices,
        material: primitive.material().index(),
    })
}

/// Reads every mesh primitive of the model's default scene with node transforms baked. A skinned
/// node's own transform is ignored (glTF 2.0 section 3.7.3.3: its vertices are in the skeleton's
/// bind space), so it is written in its bind pose.
pub(crate) fn source_primitives(
    src: &SourceModel,
    forward: Forward,
    warnings: &mut Vec<String>,
) -> Vec<SrcPrim> {
    let axis = axis_of(forward);
    let mut out = Vec::new();
    let scene = src
        .document
        .default_scene()
        .or_else(|| src.document.scenes().next());
    let Some(scene) = scene else {
        return out;
    };
    let mut stack: Vec<(gltf::Node, Mat4)> = scene.nodes().map(|n| (n, axis)).collect();
    stack.reverse();
    while let Some((node, parent)) = stack.pop() {
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        let mut kids: Vec<gltf::Node> = node.children().collect();
        kids.reverse();
        for child in kids {
            stack.push((child, world));
        }
        let Some(mesh) = node.mesh() else { continue };
        let bake = if node.skin().is_some() { axis } else { world };
        // `EXT_mesh_gpu_instancing`: the exported mesh holds every copy, as a reader without the
        // extension needs them, up to a limit that keeps a forest from filling the file.
        let (copies, dropped) = src.load_data.baked_worlds(node.index(), bake);
        if dropped > 0 {
            warnings.push(format!(
                "instances_truncated: {} node {} leaves out {dropped} instances",
                src.key,
                node.index()
            ));
        }
        for primitive in mesh.primitives() {
            for copy in &copies {
                out.extend(read_prim(
                    src,
                    mesh.name().unwrap_or(""),
                    &primitive,
                    *copy,
                    false,
                    warnings,
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The exporter
// ---------------------------------------------------------------------------------------------

struct Exporter<'a> {
    root: &'a Path,
    options: &'a ExportOptions,
    scene: GlbScene,
    extras: GlbExtras,
    warnings: Vec<String>,
    /// `(mesh path, material path)` to the mesh index (`None`: could not be built).
    meshes: BTreeMap<(String, String), Option<usize>>,
    /// Image cache key to the image index.
    images: BTreeMap<String, usize>,
    /// Material cache key to the material index.
    materials: BTreeMap<String, usize>,
    sources: BTreeMap<String, Option<SourceModel>>,
    /// `(mesh path, source mesh index, material override, skinned)` to the mesh index of a source tree.
    tree_meshes: BTreeMap<(String, usize, Option<usize>, bool), usize>,
    asset_index: Option<BTreeMap<Guid, PathBuf>>,
    /// The project's cook caches, read on first use ([`cooked`]).
    cooked_dirs: Option<Vec<PathBuf>>,
    triangles: usize,
}

impl<'a> Exporter<'a> {
    fn new(root: &'a Path, options: &'a ExportOptions) -> Self {
        Self {
            root,
            options,
            scene: GlbScene {
                generator: format!("Regia Engine {} scene export", env!("CARGO_PKG_VERSION")),
                ..GlbScene::default()
            },
            extras: GlbExtras::default(),
            warnings: Vec::new(),
            meshes: BTreeMap::new(),
            images: BTreeMap::new(),
            materials: BTreeMap::new(),
            sources: BTreeMap::new(),
            tree_meshes: BTreeMap::new(),
            asset_index: None,
            cooked_dirs: None,
            triangles: 0,
        }
    }

    /// The project file for a path the scene names (`assets/...` or relative to `assets/`).
    fn resolve(&mut self, path: &str) -> Option<PathBuf> {
        let Some(rel) = safe_relative(path) else {
            self.warnings
                .push(format!("path_rejected: `{path}` is not a relative project path"));
            return None;
        };
        let under_assets = self.root.join("assets").join(&rel);
        let found = [self.root.join(&rel), under_assets]
            .into_iter()
            .find(|p| p.is_file())?;
        // A symlink inside the project must not lead the export to a file outside it.
        if let Err(error) = regia_core::project_path::check_under_root(self.root, &found) {
            self.warnings
                .push(format!("path_rejected: `{path}` leaves the project: {error}"));
            return None;
        }
        Some(found)
    }

    /// Asset id to source file, from the `.meta` sidecars under `assets/` (built on first use).
    fn asset_path(&mut self, id: Guid) -> Option<PathBuf> {
        if self.asset_index.is_none() {
            let mut index = BTreeMap::new();
            let assets = self.root.join("assets");
            for entry in walkdir::WalkDir::new(&assets)
                .sort_by_file_name()
                .into_iter()
                .filter_map(Result::ok)
            {
                let file = entry.path();
                let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(stem) = name.strip_suffix(".meta") else {
                    continue;
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(file)
                    && let Ok(meta) = MetaSidecar::from_ron(&bytes)
                {
                    index
                        .entry(meta.id.0)
                        .or_insert_with(|| file.with_file_name(stem));
                }
            }
            self.asset_index = Some(index);
        }
        self.asset_index.as_ref().and_then(|m| m.get(&id).cloned())
    }

    /// `prefab_instance` entities become their prefab's entities under the instance entity.
    fn expand_prefabs(&mut self, entities: &[SceneEntity]) -> Vec<Ent> {
        let mut out = Vec::with_capacity(entities.len());
        for entity in entities {
            let ent = Ent::from_scene(entity);
            out.push(ent.clone());
            if let Some(instance) = &entity.prefab_instance {
                self.expand_instance(&ent, instance.prefab.guid(), instance, 0, &mut out);
            }
        }
        out
    }

    fn expand_instance(
        &mut self,
        host: &Ent,
        prefab: Guid,
        instance: &regia_scene::format::PrefabInstanceData,
        depth: u32,
        out: &mut Vec<Ent>,
    ) {
        let skipped = instance.overrides.len()
            + instance.added_components.len()
            + instance.removed_components.len()
            + instance.added_children.len()
            + instance.removed_children.len();
        if skipped > 0 {
            self.warnings.push(format!(
                "prefab_overrides_ignored: `{}` has {skipped} overrides or edits; the prefab's own values are exported",
                host.name
            ));
        }
        if depth >= 8 {
            self.warnings.push(format!(
                "prefab_depth: `{}` nests prefabs deeper than 8",
                host.name
            ));
            return;
        }
        let Some(path) = self.asset_path(prefab) else {
            self.warnings.push(format!(
                "prefab_not_found: `{}` names prefab {prefab}, which has no .meta under assets/",
                host.name
            ));
            return;
        };
        let template = match read_scene(&path) {
            Ok(file) => file,
            Err(error) => {
                self.warnings.push(format!("prefab_unreadable: {error}"));
                return;
            }
        };
        let mapped = |guid: Guid| Guid::from_u128(host.guid.as_u128().rotate_left(29) ^ guid.as_u128());
        let root_guid = template
            .entities
            .iter()
            .find(|e| e.parent.is_none())
            .map(|e| e.guid);
        for entity in &template.entities {
            let mut ent = Ent::from_scene(entity);
            if Some(entity.guid) == root_guid {
                // The prefab's root is the instance entity: its components fill in what the
                // instance does not set.
                if let Some(host_ent) = out.iter_mut().rev().find(|e| e.guid == host.guid) {
                    for (key, value) in &ent.components {
                        host_ent
                            .components
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                    }
                }
                continue;
            }
            ent.guid = mapped(entity.guid);
            ent.parent = match entity.parent {
                Some(p) if Some(p) == root_guid => Some(host.guid),
                Some(p) => Some(mapped(p)),
                None => Some(host.guid),
            };
            out.push(ent.clone());
            if let Some(nested) = &entity.prefab_instance {
                self.expand_instance(&ent, nested.prefab.guid(), nested, depth + 1, out);
            }
        }
    }

    fn run(&mut self, entities: &[Ent]) -> Result<(Vec<u8>, ExportReport), ExportError> {
        let by_guid: BTreeMap<Guid, usize> = entities.iter().enumerate().map(|(i, e)| (e.guid, i)).collect();
        let included: BTreeSet<Guid> = match &self.options.entities {
            None => entities.iter().map(|e| e.guid).collect(),
            Some(list) => {
                let mut set = BTreeSet::new();
                for guid in list {
                    if !by_guid.contains_key(guid) {
                        return Err(ExportError::UnknownEntity(*guid));
                    }
                    set.insert(*guid);
                }
                // Descendants, in passes until nothing is added (entities may list in any order).
                loop {
                    let before = set.len();
                    for e in entities {
                        if e.parent.is_some_and(|p| set.contains(&p)) {
                            set.insert(e.guid);
                        }
                    }
                    if set.len() == before {
                        break;
                    }
                }
                set
            }
        };
        if included.is_empty() {
            return Err(ExportError::NothingToExport);
        }

        let mut children: BTreeMap<Guid, Vec<usize>> = BTreeMap::new();
        let mut roots = Vec::new();
        for (i, e) in entities.iter().enumerate() {
            if !included.contains(&e.guid) {
                continue;
            }
            match e
                .parent
                .filter(|p| included.contains(p) && by_guid.contains_key(p))
            {
                Some(p) => children.entry(p).or_default().push(i),
                None => roots.push(i),
            }
        }

        // World matrices of every entity (a selected root keeps its world placement).
        let mut world: BTreeMap<Guid, Mat4> = BTreeMap::new();
        for i in 0..entities.len() {
            self.world_of(entities, &by_guid, i, &mut world, 0);
        }

        let wrapper = (self.options.forward == Forward::Convert).then(|| {
            self.scene.nodes.push(GlbNode {
                name: AXIS_NODE_NAME.to_owned(),
                rotation: [0.0, 1.0, 0.0, 0.0],
                ..GlbNode::default()
            });
            self.scene.nodes.len() - 1
        });
        let mut top = Vec::new();
        for r in roots {
            let node = self.emit(entities, r, true, Mat4::IDENTITY, &children, &world);
            top.push(node);
        }
        match wrapper {
            Some(w) => {
                self.scene.nodes[w].children = top;
                self.scene.roots = vec![w];
            }
            None => self.scene.roots = top,
        }

        let glb = self
            .scene
            .to_glb_with(&self.extras)
            .map_err(|e| ExportError::NonFinite(e.to_string()))?;
        let report = ExportReport {
            nodes: self.scene.nodes.len(),
            meshes: self.scene.meshes.len(),
            triangles: self.triangles,
            materials: self.scene.materials.len(),
            images: self.scene.images.len(),
            lights: self.extras.lights.len(),
            cameras: self.extras.cameras.len(),
            skins: self.scene.skins.len(),
            animations: self.scene.animations.len(),
            bytes: glb.len(),
            warnings: std::mem::take(&mut self.warnings),
        };
        Ok((glb, report))
    }

    fn world_of(
        &mut self,
        entities: &[Ent],
        by_guid: &BTreeMap<Guid, usize>,
        i: usize,
        world: &mut BTreeMap<Guid, Mat4>,
        depth: u32,
    ) -> Mat4 {
        let e = &entities[i];
        if let Some(m) = world.get(&e.guid) {
            return *m;
        }
        let local = self.local_matrix(e);
        let parent = match e.parent.and_then(|p| by_guid.get(&p)) {
            Some(&p) if depth < 256 => self.world_of(entities, by_guid, p, world, depth + 1),
            _ => Mat4::IDENTITY,
        };
        let m = parent * local;
        world.insert(e.guid, m);
        m
    }

    fn local_matrix(&mut self, e: &Ent) -> Mat4 {
        match e.parse::<TransformRon>(TRANSFORM) {
            Some(Ok(t)) => t.matrix(),
            Some(Err(error)) => {
                self.warnings
                    .push(format!("transform_unreadable: `{}`: {error}", e.name));
                Mat4::IDENTITY
            }
            None => Mat4::IDENTITY,
        }
    }

    /// Writes entity `i` and its descendants; returns the node index.
    /// `parent_world` is the output parent's world matrix in Regia axes (identity for roots).
    fn emit(
        &mut self,
        entities: &[Ent],
        i: usize,
        is_root: bool,
        parent_world: Mat4,
        children: &BTreeMap<Guid, Vec<usize>>,
        world: &BTreeMap<Guid, Mat4>,
    ) -> usize {
        let e = &entities[i];
        let local = if is_root {
            world.get(&e.guid).copied().unwrap_or(Mat4::IDENTITY)
        } else {
            self.local_matrix(e)
        };
        let (mut scale, mut rotation, translation) = local.to_scale_rotation_translation();
        if !scale.is_finite() || !rotation.is_finite() || !translation.is_finite() {
            scale = Vec3::ONE;
            rotation = Quat::IDENTITY;
        }
        let this_world = parent_world * local;
        let node_index = self.scene.nodes.len();
        self.scene.nodes.push(GlbNode {
            name: e.name.clone(),
            translation: if translation.is_finite() {
                translation.to_array()
            } else {
                [0.0; 3]
            },
            rotation: rotation.to_array(),
            scale: scale.to_array(),
            extras: Some(serde_json::json!({ "regia_guid": e.guid.to_string() })),
            ..GlbNode::default()
        });

        if let Some(source) = e.parse::<MeshSourceRon>(MESH_SOURCE) {
            match source {
                Ok(s) => {
                    if !skin::attach(self, node_index, &s, &e.name) {
                        let mesh = self.mesh_for(&s.mesh, &s.material, &e.name);
                        self.scene.nodes[node_index].mesh = mesh;
                    }
                }
                Err(error) => self
                    .warnings
                    .push(format!("mesh_source_unreadable: `{}`: {error}", e.name)),
            }
        }
        self.lights_and_camera(e, node_index, parent_world);

        if let Some(list) = children.get(&e.guid) {
            let mut kids = Vec::with_capacity(list.len());
            for &c in list {
                kids.push(self.emit(entities, c, false, this_world, children, world));
            }
            self.scene.nodes[node_index].children.extend(kids);
        }
        node_index
    }

    fn lights_and_camera(&mut self, e: &Ent, node: usize, parent_world: Mat4) {
        let add_light = |this: &mut Self, light: GlbLight| {
            this.extras.lights.push(light);
            this.extras.node_lights.insert(node, this.extras.lights.len() - 1);
        };
        match e.parse::<PointRon>(POINT_LIGHT) {
            Some(Ok(p)) => add_light(
                self,
                GlbLight {
                    name: e.name.clone(),
                    kind: GlbLightKind::Point,
                    color: p.color,
                    // Lumens to candela for an isotropic emitter: cd = lm / (4 pi).
                    intensity: p.intensity / (4.0 * std::f32::consts::PI),
                    range: (p.radius > 0.0).then_some(p.radius),
                },
            ),
            Some(Err(error)) => self
                .warnings
                .push(format!("light_unreadable: `{}`: {error}", e.name)),
            None => {}
        }
        match e.parse::<SpotRon>(SPOT_LIGHT) {
            Some(Ok(s)) => add_light(
                self,
                GlbLight {
                    name: e.name.clone(),
                    kind: GlbLightKind::Spot {
                        inner: s.inner_angle,
                        outer: s.outer_angle,
                    },
                    color: s.color,
                    intensity: s.intensity,
                    range: (s.range > 0.0).then_some(s.range),
                },
            ),
            Some(Err(error)) => self
                .warnings
                .push(format!("light_unreadable: `{}`: {error}", e.name)),
            None => {}
        }
        match e.parse::<DirectionalRon>(DIRECTIONAL_LIGHT) {
            Some(Ok(d)) => {
                // The component's `direction` is a world-space vector; glTF aims a light down its
                // node's local -Z, so the node turns -Z onto it.
                let dir = Vec3::from_array(d.direction).normalize_or_zero();
                if dir != Vec3::ZERO {
                    let aim = Quat::from_rotation_arc(Vec3::NEG_Z, dir);
                    let (_, parent_rotation, _) = parent_world.to_scale_rotation_translation();
                    let local = parent_rotation.inverse() * aim;
                    self.scene.nodes[node].rotation = local.to_array();
                    self.scene.nodes[node].scale = [1.0; 3];
                }
                add_light(
                    self,
                    GlbLight {
                        name: e.name.clone(),
                        kind: GlbLightKind::Directional,
                        color: d.color,
                        intensity: d.illuminance,
                        range: None,
                    },
                );
            }
            Some(Err(error)) => self
                .warnings
                .push(format!("light_unreadable: `{}`: {error}", e.name)),
            None => {}
        }
        match e.parse::<CameraRon>(CAMERA) {
            Some(Ok(c)) => {
                let projection = if c.orthographic {
                    GlbProjection::Orthographic {
                        xmag: c.ortho_size,
                        ymag: c.ortho_size,
                        znear: c.near,
                        zfar: if c.far.is_finite() { c.far } else { 100_000.0 },
                    }
                } else {
                    GlbProjection::Perspective {
                        yfov: c.fov_y,
                        znear: c.near,
                        zfar: c.far.is_finite().then_some(c.far),
                    }
                };
                self.extras.cameras.push(GlbCamera {
                    name: e.name.clone(),
                    projection,
                });
                self.extras
                    .node_cameras
                    .insert(node, self.extras.cameras.len() - 1);
            }
            Some(Err(error)) => self
                .warnings
                .push(format!("camera_unreadable: `{}`: {error}", e.name)),
            None => {}
        }
    }

    // ----- meshes -----

    fn mesh_for(&mut self, mesh_path: &str, material_path: &str, entity: &str) -> Option<usize> {
        let key = (mesh_path.to_owned(), material_path.to_owned());
        if let Some(found) = self.meshes.get(&key) {
            return *found;
        }
        let built = self.build_mesh(mesh_path, material_path, entity);
        self.meshes.insert(key, built);
        built
    }

    /// The loaded source model for `mesh_path`, taken out of the cache (put it back with
    /// [`Self::put_source`]); loads and warns on the first request.
    fn take_source(&mut self, mesh_path: &str) -> Option<SourceModel> {
        if !self.sources.contains_key(mesh_path) {
            let file = self.resolve(mesh_path)?;
            let model = match load_source(&file, mesh_path) {
                Ok(model) => Some(model),
                Err(error) => {
                    self.warnings
                        .push(format!("mesh_unreadable: `{mesh_path}`: {error}"));
                    None
                }
            };
            self.sources.insert(mesh_path.to_owned(), model);
        }
        self.sources.remove(mesh_path).flatten()
    }

    fn put_source(&mut self, mesh_path: &str, model: SourceModel) {
        self.sources.insert(mesh_path.to_owned(), Some(model));
    }

    fn build_mesh(&mut self, mesh_path: &str, material_path: &str, entity: &str) -> Option<usize> {
        if mesh_path.is_empty() {
            return None;
        }
        let override_material = if material_path.is_empty() {
            None
        } else {
            self.ron_material(material_path)
        };
        if mesh_path.starts_with("builtin:") {
            let Some((prims, name)) = builtin_primitive(mesh_path) else {
                self.warnings
                    .push(format!("mesh_unknown_builtin: `{entity}` uses `{mesh_path}`"));
                return None;
            };
            return Some(self.push_mesh(name.to_owned(), vec![(prims, override_material)]));
        }
        let Some(file) = self.resolve(mesh_path) else {
            if let Some(mesh) = self.cooked_mesh(mesh_path, override_material, entity) {
                return Some(mesh);
            }
            self.warnings.push(format!(
                "mesh_missing: `{entity}` names `{mesh_path}`, which is not under the project and not in its cook; the node is exported without a mesh"
            ));
            return None;
        };
        if !self.sources.contains_key(mesh_path) {
            let loaded = load_source(&file, mesh_path);
            let model = match loaded {
                Ok(model) => Some(model),
                Err(error) => {
                    self.warnings
                        .push(format!("mesh_unreadable: `{mesh_path}`: {error}"));
                    None
                }
            };
            self.sources.insert(mesh_path.to_owned(), model);
        }
        // The model leaves the map while it is converted (materials and images borrow `self`).
        let Some(src) = self.sources.remove(mesh_path).flatten() else {
            return self.cooked_mesh(mesh_path, override_material, entity);
        };
        let prims = source_primitives(&src, self.options.forward, &mut self.warnings);
        let mut parts = Vec::with_capacity(prims.len());
        for prim in prims {
            let material = match override_material {
                Some(m) => Some(m),
                None => prim.material.and_then(|index| self.gltf_material(&src, index)),
            };
            parts.push((prim, material));
        }
        let name = Path::new(mesh_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("mesh")
            .to_owned();
        let index = self.push_mesh(name, parts);
        self.sources.insert(mesh_path.to_owned(), Some(src));
        Some(index)
    }

    fn push_mesh(&mut self, name: String, parts: Vec<(SrcPrim, Option<usize>)>) -> usize {
        let mesh_index = self.scene.meshes.len();
        let mut primitives = Vec::with_capacity(parts.len());
        for (p, (prim, material)) in parts.into_iter().enumerate() {
            self.triangles += prim.indices.len() / 3;
            if !prim.tangents.is_empty() && prim.tangents.len() == prim.positions.len() {
                self.extras.tangents.insert((mesh_index, p), prim.tangents);
            }
            let n = prim.positions.len();
            primitives.push(GlbPrimitive {
                normals: if prim.normals.len() == n {
                    prim.normals
                } else {
                    Vec::new()
                },
                uvs: prim.uvs.into_iter().filter(|u| u.len() == n).collect(),
                colors: if prim.colors.len() == n {
                    prim.colors
                } else {
                    Vec::new()
                },
                joints: prim.joints,
                weights: prim.weights,
                positions: prim.positions,
                indices: prim.indices,
                material,
                ..GlbPrimitive::default()
            });
        }
        self.scene.meshes.push(GlbMesh {
            name,
            primitives,
            ..GlbMesh::default()
        });
        mesh_index
    }

    // ----- materials and textures -----

    /// Embeds an image once under `key`; `make` builds the bytes on the first use.
    fn image(&mut self, key: &str, make: impl FnOnce() -> Option<(Vec<u8>, &'static str)>) -> Option<usize> {
        if let Some(i) = self.images.get(key) {
            return Some(*i);
        }
        let (bytes, mime) = make()?;
        self.scene.images.push(GlbImage::Embedded { bytes, mime });
        let index = self.scene.images.len() - 1;
        self.images.insert(key.to_owned(), index);
        Some(index)
    }

    fn gltf_image(&mut self, src: &SourceModel, image_index: usize) -> Option<usize> {
        let key = format!("{}#{image_index}", src.key);
        let result = self.image(&key, || {
            png_of(src.images.get(image_index)?).map(|b| (b, "image/png"))
        });
        if result.is_none() {
            self.warnings
                .push(format!("texture_skipped: {key} cannot be written as PNG"));
        }
        result
    }

    fn gltf_material(&mut self, src: &SourceModel, index: usize) -> Option<usize> {
        let key = format!("{}#m{index}", src.key);
        if let Some(m) = self.materials.get(&key) {
            return Some(*m);
        }
        let material = src.document.materials().nth(index)?;
        let pbr = material.pbr_metallic_roughness();
        let strength = material
            .emissive_strength()
            .filter(|s| s.is_finite())
            .unwrap_or(1.0);
        let mut out = GlbMaterial {
            name: material.name().unwrap_or("").to_owned(),
            base_color: pbr.base_color_factor(),
            metallic: pbr.metallic_factor(),
            roughness: pbr.roughness_factor(),
            emissive: material.emissive_factor().map(|v| v * strength),
            alpha_blend: material.alpha_mode() == gltf::material::AlphaMode::Blend,
            double_sided: material.double_sided(),
            ..GlbMaterial::default()
        };
        let m = self.scene.materials.len();
        let slot = |this: &mut Self, info: Option<(usize, u32)>| -> Option<GlbTextureRef> {
            let (image, tex_coord) = info?;
            let image = this.gltf_image(src, image)?;
            Some(GlbTextureRef { image, tex_coord })
        };
        let tex = |t: Option<gltf::texture::Info>| t.map(|t| (t.texture().source().index(), t.tex_coord()));
        out.base_color_texture = slot(self, tex(pbr.base_color_texture()));
        out.metallic_roughness_texture = slot(self, tex(pbr.metallic_roughness_texture()));
        out.emissive_texture = slot(self, tex(material.emissive_texture()));
        out.normal_texture = slot(
            self,
            material
                .normal_texture()
                .map(|t| (t.texture().source().index(), t.tex_coord())),
        );
        if let Some(n) = material.normal_texture()
            && (n.scale() - 1.0).abs() > f32::EPSILON
        {
            self.extras.normal_scale.insert(m, n.scale());
        }
        if let Some(o) = material.occlusion_texture()
            && let Some(r) = slot(self, Some((o.texture().source().index(), o.tex_coord())))
        {
            self.extras.occlusion.insert(m, (r, o.strength()));
        }
        if material.alpha_mode() == gltf::material::AlphaMode::Mask {
            self.extras
                .alpha_mask
                .insert(m, material.alpha_cutoff().unwrap_or(0.5));
        }
        // KHR_texture_transform lives in the raw JSON (the typed API here does not model it).
        // A transform baked into quantized UVs on open is already in the vertices this export writes.
        let baked = src.load_data.baked_uv_materials.contains(&index);
        let raw = &src.json["materials"][index];
        for (which, value) in [
            (
                GlbTexSlot::BaseColor,
                &raw["pbrMetallicRoughness"]["baseColorTexture"],
            ),
            (
                GlbTexSlot::MetallicRoughness,
                &raw["pbrMetallicRoughness"]["metallicRoughnessTexture"],
            ),
            (GlbTexSlot::Normal, &raw["normalTexture"]),
            (GlbTexSlot::Emissive, &raw["emissiveTexture"]),
            (GlbTexSlot::Occlusion, &raw["occlusionTexture"]),
        ] {
            if !baked && let Some(t) = texture_transform(value) {
                self.extras.tex_transforms.insert((m, which), t);
            }
        }
        self.scene.materials.push(out);
        self.materials.insert(key, m);
        Some(m)
    }

    /// A `.material.ron` as a glTF material; other material kinds keep the mesh's own.
    fn ron_material(&mut self, path: &str) -> Option<usize> {
        let key = format!("ron:{path}");
        if let Some(m) = self.materials.get(&key) {
            return Some(*m);
        }
        let Some(file) = self.resolve(path) else {
            return self.cooked_material_at(path);
        };
        if !path.ends_with(".material.ron") {
            self.warnings.push(format!(
                "material_kind_unsupported: `{path}` is not a .material.ron; the mesh keeps its own materials"
            ));
            return None;
        }
        let bytes = std::fs::read(&file).ok()?;
        let ron = match MaterialAssetRon::from_ron(&bytes) {
            Ok(ron) => ron,
            Err(error) => {
                self.warnings
                    .push(format!("material_unreadable: `{path}`: {error}"));
                return None;
            }
        };
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        Some(self.authored_material(key, &ron, &stem))
    }

    /// An authored material (a `.material.ron` or the one inside a cooked material) as a glTF material.
    fn authored_material(&mut self, key: String, ron: &MaterialAssetRon, default_name: &str) -> usize {
        let base = &ron.base;
        let state = &ron.render_state;
        let mut out = GlbMaterial {
            name: ron.name.clone().unwrap_or_else(|| default_name.to_owned()),
            base_color: base.base_color,
            metallic: base.metallic,
            roughness: base.roughness,
            emissive: base.emissive.map(|v| v * base.emissive_strength),
            alpha_blend: state.alpha_mode == regia_asset::AlphaMode::Blend,
            double_sided: state.cull_mode == Some(regia_asset::CullMode::None),
            ..GlbMaterial::default()
        };
        let m = self.scene.materials.len();
        if state.alpha_mode == regia_asset::AlphaMode::Mask {
            self.extras.alpha_mask.insert(m, state.alpha_cutoff);
        }
        out.base_color_texture = self.slot_texture(base.base_color_map.as_ref());
        out.normal_texture = self.slot_texture(base.normal_map.as_ref());
        out.emissive_texture = self.slot_texture(base.emissive_map.as_ref());
        out.metallic_roughness_texture = self.slot_texture(base.orm_map.as_ref());
        if base.orm_packed
            && let Some(orm) = out.metallic_roughness_texture
        {
            // Packed ORM: occlusion in R, roughness in G, metalness in B; the same image.
            self.extras.occlusion.insert(m, (orm, 1.0));
        }
        self.scene.materials.push(out);
        self.materials.insert(key, m);
        m
    }

    fn slot_texture(&mut self, slot: Option<&TextureSlot>) -> Option<GlbTextureRef> {
        let path = match slot? {
            TextureSlot::Path(p) => match self.resolve(p) {
                Some(path) => path,
                None => return self.cooked_texture_at(p),
            },
            TextureSlot::Asset(id) => match self.asset_path(id.0).filter(|p| p.is_file()) {
                Some(p) => p,
                None => {
                    let cooked = self.cooked_texture(*id);
                    if cooked.is_none() {
                        self.warnings.push(format!(
                            "texture_not_found: asset {id} has no source under assets/ and is not in the cook"
                        ));
                    }
                    return cooked;
                }
            },
        };
        let key = format!("file:{}", path.display());
        let image = self.image(&key, || {
            let bytes = std::fs::read(&path).ok()?;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            match ext.as_str() {
                "png" => Some((bytes, "image/png")),
                "jpg" | "jpeg" => Some((bytes, "image/jpeg")),
                _ => {
                    let decoded = image::load_from_memory(&bytes).ok()?;
                    encode_png(&decoded).map(|b| (b, "image/png"))
                }
            }
        });
        match image {
            Some(image) => Some(GlbTextureRef { image, tex_coord: 0 }),
            None => {
                self.warnings.push(format!(
                    "texture_skipped: {} cannot be read as an image",
                    path.display()
                ));
                None
            }
        }
    }
}

/// `KHR_texture_transform` of a texture-info JSON object.
fn texture_transform(info: &Value) -> Option<GlbTexTransform> {
    let t = info.get("extensions")?.get("KHR_texture_transform")?;
    let pair = |key: &str, default: f32| -> [f32; 2] {
        let a = t.get(key).and_then(Value::as_array);
        let get = |i: usize| {
            a.and_then(|a| a.get(i))
                .and_then(Value::as_f64)
                .map_or(default, |v| v as f32)
        };
        [get(0), get(1)]
    };
    Some(GlbTexTransform {
        offset: pair("offset", 0.0),
        rotation: t
            .get("rotation")
            .and_then(Value::as_f64)
            .map_or(0.0, |v| v as f32),
        scale: pair("scale", 1.0),
    })
}

/// A built-in primitive as a source primitive, in Regia axes (no yaw: it is already Regia
/// geometry).
fn builtin_primitive(name: &str) -> Option<(SrcPrim, &'static str)> {
    use regia_render::MeshSource;
    use regia_render::primitives::{box_mesh, uv_sphere};
    let (vertices, indices, label) = match name {
        MeshSource::BUILTIN_CUBE => {
            let (v, i) = box_mesh([0.5; 3]);
            (v, i, "builtin_cube")
        }
        MeshSource::BUILTIN_SPHERE => {
            let (v, i) = uv_sphere(0.5, 16, 8);
            (v, i, "builtin_sphere")
        }
        MeshSource::BUILTIN_PLANE => {
            let quad = |p: [f32; 3], uv: [f32; 2]| regia_render::LabVertex {
                position: p,
                normal: [0.0, 1.0, 0.0],
                uv,
                tangent: [0.0; 4],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            };
            (
                vec![
                    quad([-0.5, 0.0, -0.5], [0.0, 0.0]),
                    quad([-0.5, 0.0, 0.5], [0.0, 1.0]),
                    quad([0.5, 0.0, 0.5], [1.0, 1.0]),
                    quad([0.5, 0.0, -0.5], [1.0, 0.0]),
                ],
                vec![0, 1, 2, 0, 2, 3],
                "builtin_plane",
            )
        }
        _ => return None,
    };
    let has_tangents = vertices.iter().any(|v| v.tangent[3] != 0.0);
    Some((
        SrcPrim {
            positions: vertices.iter().map(|v| v.position).collect(),
            normals: vertices.iter().map(|v| v.normal).collect(),
            tangents: if has_tangents {
                vertices.iter().map(|v| v.tangent).collect()
            } else {
                Vec::new()
            },
            uvs: vec![vertices.iter().map(|v| v.uv).collect()],
            colors: Vec::new(),
            joints: Vec::new(),
            weights: Vec::new(),
            indices,
            material: None,
        },
        label,
    ))
}

#[cfg(test)]
mod tests;
