// SPDX-License-Identifier: MPL-2.0
//! A small glTF 2.0 binary (GLB) writer for importers that convert another format into glTF in
//! memory (FBX, ADR-1602), so their output goes through the glTF importer and its cook formats
//! unchanged.
//!
//! The scene is described with owned Rust types ([`GlbScene`]); [`GlbScene::to_glb`] lays every
//! array into one binary chunk (4-byte aligned views) and writes the JSON chunk by the glTF 2.0
//! specification (Khronos, "glTF 2.0 Specification", § 4 and § 5: accessors, buffer views, the
//! GLB container). Original code.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// glTF component type `FLOAT`.
const FLOAT: u32 = 5126;
/// glTF component type `UNSIGNED_SHORT`.
const UNSIGNED_SHORT: u32 = 5123;
/// glTF component type `UNSIGNED_INT`.
const UNSIGNED_INT: u32 = 5125;
/// Buffer view target `ARRAY_BUFFER`.
const ARRAY_BUFFER: u32 = 34962;
/// Buffer view target `ELEMENT_ARRAY_BUFFER`.
const ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// One scene to write as GLB.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbScene {
    /// The `asset.generator` string.
    pub generator: String,
    /// All nodes; children refer to indices here.
    pub nodes: Vec<GlbNode>,
    /// The scene's root nodes.
    pub roots: Vec<usize>,
    /// Meshes nodes refer to.
    pub meshes: Vec<GlbMesh>,
    /// Materials primitives refer to.
    pub materials: Vec<GlbMaterial>,
    /// Images textures refer to (one texture per image).
    pub images: Vec<GlbImage>,
    /// Skins nodes refer to.
    pub skins: Vec<GlbSkin>,
    /// Animation clips.
    pub animations: Vec<GlbAnimation>,
}

/// One node with its local transform.
#[derive(Clone, Debug)]
pub(crate) struct GlbNode {
    pub name: String,
    pub children: Vec<usize>,
    pub translation: [f32; 3],
    /// Unit quaternion `x, y, z, w`.
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub mesh: Option<usize>,
    pub skin: Option<usize>,
    /// Node `extras` (an object), when any.
    pub extras: Option<Value>,
}

impl Default for GlbNode {
    fn default() -> Self {
        Self {
            name: String::new(),
            children: Vec::new(),
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            mesh: None,
            skin: None,
            extras: None,
        }
    }
}

/// One mesh: primitives sharing morph target names and default weights.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbMesh {
    pub name: String,
    pub primitives: Vec<GlbPrimitive>,
    /// `extras.targetNames` (the glTF convention the importer reads).
    pub target_names: Vec<String>,
    /// Default morph weights (one per target, or empty).
    pub weights: Vec<f32>,
    /// Further mesh `extras` keys (merged with `targetNames`).
    pub extras: Map<String, Value>,
}

/// One indexed triangle list.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbPrimitive {
    pub positions: Vec<[f32; 3]>,
    /// Empty when the source had none (the importer generates them).
    pub normals: Vec<[f32; 3]>,
    /// `TEXCOORD_n`, each as long as `positions`.
    pub uvs: Vec<Vec<[f32; 2]>>,
    /// `COLOR_0`, empty when absent.
    pub colors: Vec<[f32; 4]>,
    /// `JOINTS_n` sets (four influences each), parallel to `weights`.
    pub joints: Vec<Vec<[u16; 4]>>,
    /// `WEIGHTS_n` sets.
    pub weights: Vec<Vec<[f32; 4]>>,
    pub indices: Vec<u32>,
    pub material: Option<usize>,
    pub targets: Vec<GlbMorphTarget>,
    /// Application-specific `VEC3` float attributes (names start with `_`, glTF 2.0 § 3.7.2.1).
    pub custom_vec3: Vec<(String, Vec<[f32; 3]>)>,
    /// Application-specific `VEC4` float attributes.
    pub custom_vec4: Vec<(String, Vec<[f32; 4]>)>,
}

/// One morph target of a primitive: per-vertex deltas.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbMorphTarget {
    pub positions: Vec<[f32; 3]>,
    /// Empty when the target moves positions only.
    pub normals: Vec<[f32; 3]>,
}

/// A material texture slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GlbTextureRef {
    /// Index into [`GlbScene::images`].
    pub image: usize,
    /// `TEXCOORD_n` set the slot samples.
    pub tex_coord: u32,
}

/// A metallic-roughness material.
#[derive(Clone, Debug)]
pub(crate) struct GlbMaterial {
    pub name: String,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    /// Linear emissive color; components above 1 are written with `KHR_materials_emissive_strength`.
    pub emissive: [f32; 3],
    pub base_color_texture: Option<GlbTextureRef>,
    /// Roughness in G, metalness in B (glTF `metallicRoughnessTexture`).
    pub metallic_roughness_texture: Option<GlbTextureRef>,
    pub normal_texture: Option<GlbTextureRef>,
    pub emissive_texture: Option<GlbTextureRef>,
    pub alpha_blend: bool,
    pub double_sided: bool,
}

impl Default for GlbMaterial {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_color: [1.0; 4],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            emissive_texture: None,
            alpha_blend: false,
            double_sided: false,
        }
    }
}

/// Where an image's bytes are.
#[derive(Clone, Debug)]
pub(crate) enum GlbImage {
    /// A relative, percent-encoded URI resolved against the source file's directory.
    Uri(String),
    /// Encoded PNG or JPEG bytes stored in the binary chunk.
    Embedded { bytes: Vec<u8>, mime: &'static str },
}

/// A skin: joint nodes and their inverse bind matrices (column-major).
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbSkin {
    pub name: String,
    pub joints: Vec<usize>,
    pub inverse_bind: Vec<[f32; 16]>,
}

/// What an animation channel drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlbPath {
    Translation,
    Rotation,
    Scale,
    Weights,
}

impl GlbPath {
    fn name(self) -> &'static str {
        match self {
            GlbPath::Translation => "translation",
            GlbPath::Rotation => "rotation",
            GlbPath::Scale => "scale",
            GlbPath::Weights => "weights",
        }
    }
}

/// One sampled channel (`LINEAR` interpolation).
#[derive(Clone, Debug)]
pub(crate) struct GlbChannel {
    pub node: usize,
    pub path: GlbPath,
    /// Key times in seconds, increasing.
    pub times: Vec<f32>,
    /// Flattened values: 3 (translation, scale), 4 (rotation) or one per morph target (weights)
    /// per key.
    pub values: Vec<f32>,
}

/// One animation clip.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbAnimation {
    pub name: String,
    pub channels: Vec<GlbChannel>,
}

/// A material texture slot an extra (texture transform) attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GlbTexSlot {
    /// `baseColorTexture`.
    BaseColor,
    /// `metallicRoughnessTexture`.
    MetallicRoughness,
    /// `normalTexture`.
    Normal,
    /// `emissiveTexture`.
    Emissive,
    /// `occlusionTexture`.
    Occlusion,
}

/// A `KHR_texture_transform` (glTF extension): UV offset, rotation (radians) and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GlbTexTransform {
    pub offset: [f32; 2],
    pub rotation: f32,
    pub scale: [f32; 2],
}

/// Which `KHR_lights_punctual` light a [`GlbLight`] is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GlbLightKind {
    /// Lux.
    Directional,
    /// Candela.
    Point,
    /// Candela; cone half-angles in radians.
    Spot { inner: f32, outer: f32 },
}

/// One `KHR_lights_punctual` light.
#[derive(Clone, Debug)]
pub(crate) struct GlbLight {
    pub name: String,
    pub kind: GlbLightKind,
    pub color: [f32; 3],
    pub intensity: f32,
    /// Cut-off distance in metres, when finite.
    pub range: Option<f32>,
}

/// One glTF camera.
#[derive(Clone, Debug)]
pub(crate) struct GlbCamera {
    pub name: String,
    pub projection: GlbProjection,
}

/// A camera projection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GlbProjection {
    /// Vertical field of view in radians; `zfar` `None` is an infinite projection.
    Perspective {
        yfov: f32,
        znear: f32,
        zfar: Option<f32>,
    },
    /// Half extents in metres.
    Orthographic {
        xmag: f32,
        ymag: f32,
        znear: f32,
        zfar: f32,
    },
}

/// Additions to a [`GlbScene`] that the older importers never use. Every table is keyed by an
/// index into the scene, and an empty `GlbExtras` writes exactly the bytes [`GlbScene::to_glb`]
/// always did (the FBX importer's output is pinned by a test).
#[derive(Clone, Debug, Default)]
pub(crate) struct GlbExtras {
    /// `TANGENT` (xyz and handedness w) by `(mesh, primitive)`.
    pub tangents: BTreeMap<(usize, usize), Vec<[f32; 4]>>,
    /// `KHR_lights_punctual` lights.
    pub lights: Vec<GlbLight>,
    /// Node index to light index.
    pub node_lights: BTreeMap<usize, usize>,
    /// Cameras.
    pub cameras: Vec<GlbCamera>,
    /// Node index to camera index.
    pub node_cameras: BTreeMap<usize, usize>,
    /// `KHR_texture_transform` by `(material, slot)`.
    pub tex_transforms: BTreeMap<(usize, GlbTexSlot), GlbTexTransform>,
    /// `occlusionTexture` and its strength by material.
    pub occlusion: BTreeMap<usize, (GlbTextureRef, f32)>,
    /// `normalTexture.scale` by material.
    pub normal_scale: BTreeMap<usize, f32>,
    /// `alphaMode: MASK` with this cutoff, by material (wins over `alpha_blend`).
    pub alpha_mask: BTreeMap<usize, f32>,
}

/// The binary chunk being built: views and accessors over it.
#[derive(Default)]
struct BinBuilder {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl BinBuilder {
    /// Appends `bytes` as a 4-byte aligned buffer view and returns its index.
    fn view(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let offset = self.bin.len();
        self.bin.extend_from_slice(bytes);
        let mut view = json!({ "buffer": 0, "byteOffset": offset, "byteLength": bytes.len() });
        if let Some(target) = target {
            view["target"] = json!(target);
        }
        self.views.push(view);
        self.views.len() - 1
    }

    /// A float accessor of `N` components per element (`kind` = `SCALAR`, `VEC2` …).
    ///
    /// # Errors
    ///
    /// [`GlbWriteError`] when `bounds` is set and a component is NaN or infinite: serde_json
    /// would write the accessor `min`/`max` as `null`, an invalid glTF, so the export fails.
    fn floats<const N: usize>(
        &mut self,
        data: &[[f32; N]],
        kind: &str,
        target: Option<u32>,
        bounds: bool,
    ) -> Result<usize, GlbWriteError> {
        if bounds && !data.is_empty() && data.iter().flatten().any(|v| !v.is_finite()) {
            return Err(GlbWriteError(format!(
                "{kind} accessor has a non-finite component"
            )));
        }
        let bytes: Vec<u8> = data.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let view = self.view(&bytes, target);
        let mut accessor = json!({
            "bufferView": view,
            "componentType": FLOAT,
            "count": data.len(),
            "type": kind,
        });
        if bounds && !data.is_empty() {
            let mut min = [f32::INFINITY; N];
            let mut max = [f32::NEG_INFINITY; N];
            for element in data {
                for (c, v) in element.iter().enumerate() {
                    min[c] = min[c].min(*v);
                    max[c] = max[c].max(*v);
                }
            }
            accessor["min"] = json!(min.to_vec());
            accessor["max"] = json!(max.to_vec());
        }
        self.accessors.push(accessor);
        Ok(self.accessors.len() - 1)
    }

    fn joints(&mut self, data: &[[u16; 4]]) -> usize {
        let bytes: Vec<u8> = data.iter().flatten().flat_map(|j| j.to_le_bytes()).collect();
        let view = self.view(&bytes, Some(ARRAY_BUFFER));
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": UNSIGNED_SHORT,
            "count": data.len(),
            "type": "VEC4",
        }));
        self.accessors.len() - 1
    }

    fn indices(&mut self, data: &[u32]) -> usize {
        let bytes: Vec<u8> = data.iter().flat_map(|i| i.to_le_bytes()).collect();
        let view = self.view(&bytes, Some(ELEMENT_ARRAY_BUFFER));
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": UNSIGNED_INT,
            "count": data.len(),
            "type": "SCALAR",
        }));
        self.accessors.len() - 1
    }
}

fn texture_info(slot: &GlbTextureRef) -> Value {
    let mut info = json!({ "index": slot.image });
    if slot.tex_coord != 0 {
        info["texCoord"] = json!(slot.tex_coord);
    }
    info
}

/// [`texture_info`] plus the slot's `KHR_texture_transform`, when `extras` names one.
fn texture_info_with(
    slot: &GlbTextureRef,
    extras: &GlbExtras,
    material: usize,
    which: GlbTexSlot,
    used: &mut Vec<&'static str>,
) -> Value {
    let mut info = texture_info(slot);
    if let Some(t) = extras.tex_transforms.get(&(material, which)) {
        info["extensions"] = json!({
            "KHR_texture_transform": {
                "offset": t.offset.map(|v| finite(v, 0.0)),
                "rotation": finite(t.rotation, 0.0),
                "scale": t.scale.map(|v| finite(v, 1.0)),
            }
        });
        if !used.contains(&"KHR_texture_transform") {
            used.push("KHR_texture_transform");
        }
    }
    info
}

fn finite(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

/// A scene could not be written as a GLB: a bounded float accessor (`POSITION`, a morph target,
/// animation input times) held a NaN or infinite component, which serde_json 1.0.151 writes as
/// `null` — a silently invalid glTF, not a serialization error. The export fails with this
/// error instead (asset_pipeline3 correctness audit, finding 1).
#[derive(Debug, thiserror::Error)]
#[error("glb: {0}")]
pub struct GlbWriteError(String);

impl GlbScene {
    /// The scene as a GLB file (12-byte header, JSON chunk, BIN chunk).
    ///
    /// # Errors
    ///
    /// [`GlbWriteError`] when a bounded float accessor holds a non-finite component.
    pub(crate) fn to_glb(&self) -> Result<Vec<u8>, GlbWriteError> {
        self.to_glb_with(&GlbExtras::default())
    }

    /// [`Self::to_glb`] with the additions in `extras` (tangents, lights, cameras, texture
    /// transforms, occlusion maps, alpha cutoffs).
    ///
    /// # Errors
    ///
    /// [`GlbWriteError`] when a bounded float accessor holds a non-finite component.
    pub(crate) fn to_glb_with(&self, extras: &GlbExtras) -> Result<Vec<u8>, GlbWriteError> {
        let mut bin = BinBuilder::default();
        let mut extensions_used: Vec<&str> = Vec::new();

        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(node_index, node)| {
                let mut out = Map::new();
                if !node.name.is_empty() {
                    out.insert("name".into(), json!(node.name));
                }
                if !node.children.is_empty() {
                    out.insert("children".into(), json!(node.children));
                }
                if node.translation != [0.0; 3] {
                    out.insert(
                        "translation".into(),
                        json!(node.translation.map(|v| finite(v, 0.0))),
                    );
                }
                if node.rotation != [0.0, 0.0, 0.0, 1.0] {
                    out.insert("rotation".into(), json!(normalized_quat(node.rotation)));
                }
                if node.scale != [1.0; 3] {
                    out.insert("scale".into(), json!(node.scale.map(|v| finite(v, 1.0))));
                }
                if let Some(mesh) = node.mesh {
                    out.insert("mesh".into(), json!(mesh));
                }
                if let Some(skin) = node.skin {
                    out.insert("skin".into(), json!(skin));
                }
                if let Some(extra) = &node.extras {
                    out.insert("extras".into(), extra.clone());
                }
                if let Some(camera) = extras.node_cameras.get(&node_index) {
                    out.insert("camera".into(), json!(camera));
                }
                if let Some(light) = extras.node_lights.get(&node_index) {
                    out.insert(
                        "extensions".into(),
                        json!({ "KHR_lights_punctual": { "light": light } }),
                    );
                }
                Value::Object(out)
            })
            .collect();

        let mut meshes = Vec::with_capacity(self.meshes.len());
        for (mesh_index, mesh) in self.meshes.iter().enumerate() {
            let mut primitives = Vec::with_capacity(mesh.primitives.len());
            for (primitive_index, primitive) in mesh.primitives.iter().enumerate() {
                let mut attributes = Map::new();
                let positions = bin.floats(&primitive.positions, "VEC3", Some(ARRAY_BUFFER), true)?;
                attributes.insert("POSITION".into(), json!(positions));
                if !primitive.normals.is_empty() {
                    let normals = bin.floats(&primitive.normals, "VEC3", Some(ARRAY_BUFFER), false)?;
                    attributes.insert("NORMAL".into(), json!(normals));
                }
                if let Some(tangents) = extras.tangents.get(&(mesh_index, primitive_index)) {
                    let tangents = bin.floats(tangents, "VEC4", Some(ARRAY_BUFFER), false)?;
                    attributes.insert("TANGENT".into(), json!(tangents));
                }
                for (set, uvs) in primitive.uvs.iter().enumerate() {
                    let uvs = bin.floats(uvs, "VEC2", Some(ARRAY_BUFFER), false)?;
                    attributes.insert(format!("TEXCOORD_{set}"), json!(uvs));
                }
                if !primitive.colors.is_empty() {
                    let colors = bin.floats(&primitive.colors, "VEC4", Some(ARRAY_BUFFER), false)?;
                    attributes.insert("COLOR_0".into(), json!(colors));
                }
                for (set, (joints, weights)) in primitive.joints.iter().zip(&primitive.weights).enumerate() {
                    attributes.insert(format!("JOINTS_{set}"), json!(bin.joints(joints)));
                    let weights = bin.floats(weights, "VEC4", Some(ARRAY_BUFFER), false)?;
                    attributes.insert(format!("WEIGHTS_{set}"), json!(weights));
                }
                for (name, values) in &primitive.custom_vec3 {
                    let values = bin.floats(values, "VEC3", Some(ARRAY_BUFFER), false)?;
                    attributes.insert(name.clone(), json!(values));
                }
                for (name, values) in &primitive.custom_vec4 {
                    let values = bin.floats(values, "VEC4", Some(ARRAY_BUFFER), false)?;
                    attributes.insert(name.clone(), json!(values));
                }
                let mut out = json!({
                    "attributes": attributes,
                    "indices": bin.indices(&primitive.indices),
                    "mode": 4,
                });
                if let Some(material) = primitive.material {
                    out["material"] = json!(material);
                }
                if !primitive.targets.is_empty() {
                    let mut targets = Vec::with_capacity(primitive.targets.len());
                    for target in &primitive.targets {
                        let mut t = Map::new();
                        let positions = bin.floats(&target.positions, "VEC3", None, true)?;
                        t.insert("POSITION".into(), json!(positions));
                        if !target.normals.is_empty() {
                            let normals = bin.floats(&target.normals, "VEC3", None, false)?;
                            t.insert("NORMAL".into(), json!(normals));
                        }
                        targets.push(Value::Object(t));
                    }
                    out["targets"] = json!(targets);
                }
                primitives.push(out);
            }
            let mut out = json!({ "primitives": primitives });
            if !mesh.name.is_empty() {
                out["name"] = json!(mesh.name);
            }
            if !mesh.weights.is_empty() {
                out["weights"] = json!(mesh.weights);
            }
            let mut mesh_extras = mesh.extras.clone();
            if !mesh.target_names.is_empty() {
                mesh_extras.insert("targetNames".into(), json!(mesh.target_names));
            }
            if !mesh_extras.is_empty() {
                out["extras"] = Value::Object(mesh_extras);
            }
            meshes.push(out);
        }

        let materials: Vec<Value> = self
            .materials
            .iter()
            .enumerate()
            .map(|(m, material)| {
                let base = material.base_color.map(|v| finite(v, 1.0).clamp(0.0, 1.0));
                let mut pbr = json!({
                    "baseColorFactor": base,
                    "metallicFactor": finite(material.metallic, 0.0).clamp(0.0, 1.0),
                    "roughnessFactor": finite(material.roughness, 1.0).clamp(0.0, 1.0),
                });
                if let Some(slot) = &material.base_color_texture {
                    pbr["baseColorTexture"] =
                        texture_info_with(slot, extras, m, GlbTexSlot::BaseColor, &mut extensions_used);
                }
                if let Some(slot) = &material.metallic_roughness_texture {
                    pbr["metallicRoughnessTexture"] = texture_info_with(
                        slot,
                        extras,
                        m,
                        GlbTexSlot::MetallicRoughness,
                        &mut extensions_used,
                    );
                }
                let mut out = json!({ "pbrMetallicRoughness": pbr });
                if !material.name.is_empty() {
                    out["name"] = json!(material.name);
                }
                if let Some(slot) = &material.normal_texture {
                    out["normalTexture"] =
                        texture_info_with(slot, extras, m, GlbTexSlot::Normal, &mut extensions_used);
                    if let Some(scale) = extras.normal_scale.get(&m) {
                        out["normalTexture"]["scale"] = json!(finite(*scale, 1.0));
                    }
                }
                if let Some(slot) = &material.emissive_texture {
                    out["emissiveTexture"] =
                        texture_info_with(slot, extras, m, GlbTexSlot::Emissive, &mut extensions_used);
                }
                if let Some((slot, strength)) = extras.occlusion.get(&m) {
                    out["occlusionTexture"] =
                        texture_info_with(slot, extras, m, GlbTexSlot::Occlusion, &mut extensions_used);
                    if *strength != 1.0 {
                        out["occlusionTexture"]["strength"] = json!(finite(*strength, 1.0).clamp(0.0, 1.0));
                    }
                }
                let emissive = material.emissive.map(|v| finite(v, 0.0).max(0.0));
                let strength = emissive.iter().copied().fold(0.0f32, f32::max);
                if strength > 1.0 {
                    out["emissiveFactor"] = json!(emissive.map(|v| v / strength));
                    out["extensions"] = json!({
                        "KHR_materials_emissive_strength": { "emissiveStrength": strength }
                    });
                    if !extensions_used.contains(&"KHR_materials_emissive_strength") {
                        extensions_used.push("KHR_materials_emissive_strength");
                    }
                } else if strength > 0.0 || material.emissive_texture.is_some() {
                    let factor = if strength > 0.0 { emissive } else { [1.0; 3] };
                    out["emissiveFactor"] = json!(factor);
                }
                if let Some(cutoff) = extras.alpha_mask.get(&m) {
                    out["alphaMode"] = json!("MASK");
                    out["alphaCutoff"] = json!(finite(*cutoff, 0.5).clamp(0.0, 1.0));
                } else if material.alpha_blend {
                    out["alphaMode"] = json!("BLEND");
                }
                if material.double_sided {
                    out["doubleSided"] = json!(true);
                }
                out
            })
            .collect();

        let mut images = Vec::with_capacity(self.images.len());
        for image in &self.images {
            images.push(match image {
                GlbImage::Uri(uri) => json!({ "uri": uri }),
                GlbImage::Embedded { bytes, mime } => {
                    let view = bin.view(bytes, None);
                    json!({ "bufferView": view, "mimeType": mime })
                }
            });
        }
        let textures: Vec<Value> = (0..self.images.len())
            .map(|i| json!({ "source": i, "sampler": 0 }))
            .collect();

        let mut skins = Vec::with_capacity(self.skins.len());
        for skin in &self.skins {
            let matrices = bin.floats(&skin.inverse_bind, "MAT4", None, false)?;
            let mut out = json!({ "joints": skin.joints, "inverseBindMatrices": matrices });
            if !skin.name.is_empty() {
                out["name"] = json!(skin.name);
            }
            skins.push(out);
        }

        let mut animations = Vec::with_capacity(self.animations.len());
        for animation in &self.animations {
            let mut samplers = Vec::new();
            let mut channels = Vec::new();
            for channel in &animation.channels {
                if channel.times.is_empty() {
                    continue;
                }
                let times: Vec<[f32; 1]> = channel.times.iter().map(|t| [*t]).collect();
                let input = bin.floats(&times, "SCALAR", None, true)?;
                let output = match channel.path {
                    GlbPath::Translation | GlbPath::Scale => {
                        let (values, _) = channel.values.as_chunks::<3>();
                        bin.floats(values, "VEC3", None, false)?
                    }
                    GlbPath::Rotation => {
                        let (values, _) = channel.values.as_chunks::<4>();
                        let values: Vec<[f32; 4]> = values.iter().map(|q| normalized_quat(*q)).collect();
                        bin.floats(&values, "VEC4", None, false)?
                    }
                    GlbPath::Weights => {
                        let values: Vec<[f32; 1]> = channel.values.iter().map(|w| [*w]).collect();
                        bin.floats(&values, "SCALAR", None, false)?
                    }
                };
                samplers.push(json!({ "input": input, "output": output, "interpolation": "LINEAR" }));
                channels.push(json!({
                    "sampler": samplers.len() - 1,
                    "target": { "node": channel.node, "path": channel.path.name() },
                }));
            }
            if channels.is_empty() {
                continue;
            }
            let mut out = json!({ "samplers": samplers, "channels": channels });
            if !animation.name.is_empty() {
                out["name"] = json!(animation.name);
            }
            animations.push(out);
        }

        let mut root = json!({
            "asset": { "version": "2.0", "generator": self.generator },
            "scene": 0,
            "scenes": [{ "nodes": self.roots }],
            "nodes": nodes,
        });
        let set = |root: &mut Value, key: &str, items: Vec<Value>| {
            if !items.is_empty() {
                root[key] = Value::Array(items);
            }
        };
        set(&mut root, "meshes", meshes);
        set(&mut root, "materials", materials);
        if !images.is_empty() {
            root["samplers"] = json!([{ "wrapS": 10497, "wrapT": 10497 }]);
        }
        set(&mut root, "images", images);
        set(&mut root, "textures", textures);
        set(&mut root, "skins", skins);
        set(&mut root, "animations", animations);
        set(
            &mut root,
            "cameras",
            extras.cameras.iter().map(camera_json).collect(),
        );
        if !extras.lights.is_empty() {
            root["extensions"] = json!({
                "KHR_lights_punctual": { "lights": extras.lights.iter().map(light_json).collect::<Vec<_>>() }
            });
            extensions_used.push("KHR_lights_punctual");
        }
        if !extensions_used.is_empty() {
            root["extensionsUsed"] = json!(extensions_used);
        }
        while !bin.bin.len().is_multiple_of(4) {
            bin.bin.push(0);
        }
        if !bin.bin.is_empty() {
            root["buffers"] = json!([{ "byteLength": bin.bin.len() }]);
            root["bufferViews"] = Value::Array(std::mem::take(&mut bin.views));
            root["accessors"] = Value::Array(std::mem::take(&mut bin.accessors));
        }

        let mut json_bytes = serde_json::to_vec(&root).unwrap_or_default();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let bin_len = if bin.bin.is_empty() { 0 } else { 8 + bin.bin.len() };
        let total = 12 + 8 + json_bytes.len() + bin_len;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json_bytes);
        if !bin.bin.is_empty() {
            out.extend_from_slice(&(bin.bin.len() as u32).to_le_bytes());
            out.extend_from_slice(b"BIN\0");
            out.extend_from_slice(&bin.bin);
        }
        Ok(out)
    }
}

/// One camera as glTF JSON.
fn camera_json(camera: &GlbCamera) -> Value {
    let mut out = match camera.projection {
        GlbProjection::Perspective { yfov, znear, zfar } => {
            let mut p = json!({ "yfov": finite(yfov, 0.8), "znear": finite(znear, 0.1).max(1e-6) });
            if let Some(far) = zfar.filter(|f| f.is_finite()) {
                p["zfar"] = json!(far);
            }
            json!({ "type": "perspective", "perspective": p })
        }
        GlbProjection::Orthographic {
            xmag,
            ymag,
            znear,
            zfar,
        } => json!({
            "type": "orthographic",
            "orthographic": {
                "xmag": finite(xmag, 1.0),
                "ymag": finite(ymag, 1.0),
                "znear": finite(znear, 0.1),
                "zfar": finite(zfar, 1000.0),
            },
        }),
    };
    if !camera.name.is_empty() {
        out["name"] = json!(camera.name);
    }
    out
}

/// One `KHR_lights_punctual` light as JSON.
fn light_json(light: &GlbLight) -> Value {
    let (kind, spot) = match light.kind {
        GlbLightKind::Directional => ("directional", None),
        GlbLightKind::Point => ("point", None),
        GlbLightKind::Spot { inner, outer } => ("spot", Some((inner, outer))),
    };
    let mut out = json!({
        "type": kind,
        "color": light.color.map(|v| finite(v, 1.0).clamp(0.0, 1.0)),
        "intensity": finite(light.intensity, 1.0).max(0.0),
    });
    if !light.name.is_empty() {
        out["name"] = json!(light.name);
    }
    if let Some(range) = light.range.filter(|r| r.is_finite() && *r > 0.0)
        && kind != "directional"
    {
        out["range"] = json!(range);
    }
    if let Some((inner, outer)) = spot {
        // Khronos: 0 <= inner < outer <= pi/2.
        let outer = finite(outer, std::f32::consts::FRAC_PI_4).clamp(1e-4, std::f32::consts::FRAC_PI_2);
        let inner = finite(inner, 0.0).clamp(0.0, outer * 0.9999);
        out["spot"] = json!({ "innerConeAngle": inner, "outerConeAngle": outer });
    }
    out
}

/// `q` scaled to unit length; a zero or non-finite quaternion becomes the identity.
fn normalized_quat(q: [f32; 4]) -> [f32; 4] {
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    if len.is_finite() && len > 1e-12 {
        q.map(|c| c / len)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// Percent-encodes a relative path for a glTF URI (RFC 3986 unreserved characters and `/` stay).
pub(crate) fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod pin_scene;

#[cfg(test)]
mod tests {
    use super::pin_scene::{fnv1a, rich_scene};
    use super::*;

    /// FNV-1a of `rich_scene().to_glb()` taken with the writer as it was before `GlbExtras`
    /// (the pin scene compiled against that file), so the FBX importer's output stays byte-identical.
    const RICH_SCENE_PIN: u64 = 0xfbb7_f7ae_1a8f_6f77;

    #[test]
    fn the_older_importers_output_is_byte_identical() {
        let scene = rich_scene();
        let bytes = scene.to_glb().expect("writes");
        assert_eq!(
            bytes,
            scene.to_glb_with(&GlbExtras::default()).expect("writes"),
            "empty extras change nothing"
        );
        assert_eq!(
            fnv1a(&bytes),
            RICH_SCENE_PIN,
            "the writer's bytes moved: {:#x}",
            fnv1a(&bytes)
        );
    }

    fn triangle_scene() -> GlbScene {
        GlbScene {
            generator: "test".into(),
            nodes: vec![GlbNode {
                name: "Tri".into(),
                mesh: Some(0),
                translation: [1.0, 2.0, 3.0],
                ..GlbNode::default()
            }],
            roots: vec![0],
            meshes: vec![GlbMesh {
                name: "Tri".into(),
                primitives: vec![GlbPrimitive {
                    positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                    normals: vec![[0.0, 0.0, 1.0]; 3],
                    uvs: vec![vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]],
                    indices: vec![0, 1, 2],
                    ..GlbPrimitive::default()
                }],
                ..GlbMesh::default()
            }],
            ..GlbScene::default()
        }
    }

    /// Audit finding 1 (asset_pipeline3 correctness): a NaN or ±Inf in a bounded accessor used to
    /// be written as `null` `min`/`max` by serde_json 1.0.151 — a silently invalid glTF, not a
    /// panic. The export now fails with an error, and the GLB never carries the non-finite
    /// component.
    #[test]
    fn a_non_finite_bounded_float_fails_the_export_instead_of_panicking() {
        for bad in [
            [f32::INFINITY, 0.0, 0.0],
            [f32::NAN, 1.0, 0.0],
            [f32::NEG_INFINITY, 0.0, 2.0],
        ] {
            let mut scene = triangle_scene();
            scene.meshes[0].primitives[0].positions[0] = bad;
            let err = scene
                .to_glb()
                .expect_err("a non-finite bounded component must fail the export, not write null");
            assert!(err.to_string().contains("non-finite"), "{err}");
        }
        // A morph target position is bounded too.
        let mut scene = triangle_scene();
        scene.meshes[0].primitives[0].targets = vec![GlbMorphTarget {
            positions: vec![[0.0, f32::NAN, 0.0]],
            normals: Vec::new(),
        }];
        assert!(scene.to_glb().is_err(), "morph target POSITION is bounded");
        // A finite scene still writes the exact same bytes (the pin above guards the format).
        assert!(triangle_scene().to_glb().is_ok());
    }

    #[test]
    fn the_glb_parses_with_the_gltf_crate_and_keeps_the_geometry() {
        let glb = triangle_scene().to_glb().expect("writes");
        assert_eq!(&glb[0..4], b"glTF");
        assert_eq!(
            u32::from_le_bytes([glb[8], glb[9], glb[10], glb[11]]) as usize,
            glb.len()
        );
        assert!(glb.len().is_multiple_of(4));
        let gltf = gltf::Gltf::from_slice(&glb).expect("valid glTF");
        let blob = gltf.blob.clone().expect("bin chunk");
        let mesh = gltf.document.meshes().next().expect("mesh");
        let primitive = mesh.primitives().next().expect("primitive");
        let reader = primitive.reader(|_| Some(blob.as_slice()));
        let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
        assert_eq!(positions, vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        let indices: Vec<u32> = reader.read_indices().expect("indices").into_u32().collect();
        assert_eq!(indices, vec![0, 1, 2]);
        let node = gltf.document.nodes().next().expect("node");
        let (t, _, _) = node.transform().decomposed();
        assert_eq!(t, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn uri_paths_are_percent_encoded() {
        assert_eq!(encode_uri_path("tex/a b.png"), "tex/a%20b.png");
        assert_eq!(encode_uri_path("ü.png"), "%C3%BC.png");
    }

    #[test]
    fn degenerate_quaternions_become_identity() {
        assert_eq!(normalized_quat([0.0; 4]), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(normalized_quat([0.0, 0.0, 0.0, 2.0]), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(normalized_quat([f32::NAN, 0.0, 0.0, 1.0]), [0.0, 0.0, 0.0, 1.0]);
    }
}
