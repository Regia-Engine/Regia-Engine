// SPDX-License-Identifier: MPL-2.0
//! Render-out AOVs (arbitrary output variables, ADR-1521): per-frame depth, instance and class ids
//! and motion vectors written next to the delivered frames, as control inputs for video tools
//! (enhancement, relighting, segmentation).
//!
//! This is the pure half: the request (`aovs`, `aov_format`), the id tables that keep every id
//! stable across the frames of a job, the planes a readback becomes, the file names, the terrain
//! grid the pass draws, and the encodings the manifest records. The GPU pass is `aov_pass`.
//!
//! Encodings (the manifest's `aov.encodings` repeats them):
//! - `depth`: linear view depth in meters, `f32`, the distance from the camera plane along the
//!   view direction (not the distance to the eye). Background (sky) is `+inf`.
//! - `id`: two `u32` channels. `instance`: one id per entity drawn (the terrain counts as one),
//!   assigned the first time it is seen, in a fixed order, and kept for the rest of the job.
//!   `class`: one id per mesh asset (every instance of the same mesh shares it). 0 = background.
//! - `motion`: two `f32` channels, delivered pixels, `+x` right and `+y` down: where the surface
//!   under this pixel center was in the previous delivered frame, minus where it is now (backward
//!   motion, the engine's TAA convention). 0 on frame 0 and on every cut frame.
//! - the material AOVs (ADR-3256): `normal` and `normal_view` (unit surface normal, world and view
//!   space), `roughness`, `metalness`, `base_color` (linear albedo before lighting, vertex colour
//!   not applied) and `material_id` (`u32`, stable for the job). All 0 on the sky.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_render::runtime::{CpuMaterial, CpuMesh};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// One AOV a job can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Aov {
    /// Linear view depth, meters.
    Depth,
    /// Instance and class ids.
    Id,
    /// Backward motion vectors, pixels.
    Motion,
    /// Unit surface normal, world space (ADR-3256).
    Normal,
    /// Unit surface normal, view space (camera looking down `-z`, `+x` right, `+y` up).
    NormalView,
    /// Perceptual roughness of the material at the pixel.
    Roughness,
    /// Metalness of the material at the pixel.
    Metalness,
    /// Linear base color (albedo) before lighting.
    BaseColor,
    /// Material id, stable for the whole job.
    MaterialId,
}

impl Aov {
    /// Every AOV, in the order the protocol lists them.
    pub const ALL: [Self; 9] = [
        Self::Depth,
        Self::Id,
        Self::Motion,
        Self::Normal,
        Self::NormalView,
        Self::Roughness,
        Self::Metalness,
        Self::BaseColor,
        Self::MaterialId,
    ];

    /// True for the AOVs the material pass draws (ADR-3256).
    pub fn is_material(self) -> bool {
        !matches!(self, Self::Depth | Self::Id | Self::Motion)
    }

    /// True when `png` and `png16` can hold this AOV (a 0..1 or unit-vector quantity).
    pub fn png_capable(self) -> bool {
        matches!(
            self,
            Self::Normal | Self::NormalView | Self::Roughness | Self::Metalness | Self::BaseColor
        )
    }

    /// Parses an `aovs` entry.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "depth" => Some(Self::Depth),
            "id" => Some(Self::Id),
            "motion" => Some(Self::Motion),
            "normal" => Some(Self::Normal),
            "normal_view" => Some(Self::NormalView),
            "roughness" => Some(Self::Roughness),
            "metalness" => Some(Self::Metalness),
            "base_color" => Some(Self::BaseColor),
            "material_id" => Some(Self::MaterialId),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest, file names).
    pub fn name(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::Id => "id",
            Self::Motion => "motion",
            Self::Normal => "normal",
            Self::NormalView => "normal_view",
            Self::Roughness => "roughness",
            Self::Metalness => "metalness",
            Self::BaseColor => "base_color",
            Self::MaterialId => "material_id",
        }
    }

    /// The channel names of this AOV's EXR file (and the channel order of its raw file).
    pub fn channels(self) -> &'static [&'static str] {
        match self {
            Self::Depth => &["Z"],
            Self::Id => &["id.instance", "id.class"],
            Self::Motion => &["motion.u", "motion.v"],
            Self::Normal => &["normal.x", "normal.y", "normal.z"],
            Self::NormalView => &["normal_view.x", "normal_view.y", "normal_view.z"],
            Self::Roughness => &["roughness"],
            Self::Metalness => &["metalness"],
            Self::BaseColor => &["base_color.r", "base_color.g", "base_color.b"],
            Self::MaterialId => &["material_id"],
        }
    }

    /// Bytes per pixel of the raw file.
    pub fn raw_bytes_per_pixel(self) -> u64 {
        4 * self.channels().len() as u64
    }
}

/// How AOV files are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AovFormat {
    /// One OpenEXR file per AOV per frame (ZIP compressed, lossless), the default.
    Exr,
    /// One headerless file per AOV per frame: the channels interleaved, little-endian 32-bit.
    Raw,
    /// One 8-bit PNG per AOV per frame (the material AOVs `normal`, `normal_view`, `roughness`,
    /// `metalness` and `base_color` only, ADR-3256).
    Png,
    /// One 16-bit PNG per AOV per frame (same AOVs as [`Self::Png`]).
    Png16,
}

impl AovFormat {
    /// Parses `aov_format`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "exr" => Some(Self::Exr),
            "raw" => Some(Self::Raw),
            "png" => Some(Self::Png),
            "png16" => Some(Self::Png16),
            _ => None,
        }
    }

    /// Stable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Exr => "exr",
            Self::Raw => "raw",
            Self::Png => "png",
            Self::Png16 => "png16",
        }
    }

    /// The file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png | Self::Png16 => "png",
            other => other.name(),
        }
    }
}

/// The AOV part of a `render.out_start` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AovRequest {
    /// The AOVs to write, sorted, no duplicates. Empty: none (the default).
    pub list: Vec<Aov>,
    /// File format of the AOV files.
    pub format: AovFormat,
}

impl Default for AovRequest {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            format: AovFormat::Exr,
        }
    }
}

impl AovRequest {
    /// Parses `aovs` (an array of AOV names, see [`Aov::ALL`]) and `aov_format` (`"exr"`, `"raw"`,
    /// `"png"` or `"png16"`). An unknown name is `unknown_aov`; `png` or `png16` with an AOV that
    /// is not a 0..1 or unit-vector quantity is `aov_format_unsupported`; the rest is
    /// `INVALID_PARAMS`.
    ///
    /// # Errors
    ///
    /// The refusal described above.
    pub fn parse(params: &Value) -> Result<Self, RpcError> {
        let names = Aov::ALL.map(Aov::name).join(", ");
        let mut list = Vec::new();
        match params.get("aovs") {
            None | Some(Value::Null) => {}
            Some(Value::Array(entries)) => {
                for entry in entries {
                    let aov = entry.as_str().and_then(Aov::parse).ok_or_else(|| {
                        RpcError::new(
                            INVALID_PARAMS,
                            "unknown_aov",
                            format!("unknown aov {entry}; the AOVs are {names}"),
                            "`aovs` is an array of AOV names; agent.schema lists them",
                        )
                    })?;
                    list.push(aov);
                }
            }
            Some(_) => {
                return Err(RpcError::invalid_params(format!(
                    "`aovs` must be an array of AOV names ({names})"
                )));
            }
        }
        list.sort_unstable();
        list.dedup();
        let format = match params.get("aov_format") {
            None | Some(Value::Null) => AovFormat::Exr,
            Some(v) => v.as_str().and_then(AovFormat::parse).ok_or_else(|| {
                RpcError::invalid_params(
                    "`aov_format` must be \"exr\" (default), \"raw\", \"png\" or \"png16\"",
                )
            })?,
        };
        if params.get("aov_format").is_some_and(|v| !v.is_null()) && list.is_empty() {
            return Err(RpcError::invalid_params(
                "`aov_format` needs `aovs` (which AOVs to write)",
            ));
        }
        if matches!(format, AovFormat::Png | AovFormat::Png16)
            && let Some(bad) = list.iter().find(|aov| !aov.png_capable())
        {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "aov_format_unsupported",
                format!(
                    "aov_format \"{}\" cannot hold the {} AOV (exact ids, depth and motion need \"exr\" or \"raw\")",
                    format.name(),
                    bad.name()
                ),
                "png and png16 hold normal, normal_view, roughness, metalness and base_color",
            ));
        }
        Ok(Self { list, format })
    }

    /// True when the job writes a material AOV (the material pass runs).
    pub fn wants_material(&self) -> bool {
        self.list.iter().any(|aov| aov.is_material())
    }

    /// True when the job writes at least one AOV.
    pub fn is_on(&self) -> bool {
        !self.list.is_empty()
    }

    /// The file of `aov` for delivered frame `index`.
    pub fn path(&self, out_dir: &Path, name: &str, aov: Aov, index: u64) -> PathBuf {
        out_dir.join(format!(
            "{name}_{}_{index:06}.{}",
            aov.name(),
            self.format.extension()
        ))
    }

    /// Every AOV file of delivered frame `index`, in request order (depth, id, motion).
    pub fn paths(&self, out_dir: &Path, name: &str, index: u64) -> Vec<PathBuf> {
        self.list
            .iter()
            .map(|aov| self.path(out_dir, name, *aov, index))
            .collect()
    }

    /// The request echo (`request.aovs`, `request.aov_format`).
    pub fn to_json(&self) -> Value {
        json!({
            "aovs": self.list.iter().map(|a| a.name()).collect::<Vec<_>>(),
            "aov_format": self.format.name(),
        })
    }
}

/// The subframe of a delivered frame the AOVs are sampled at: the middle one (`subframes / 2`),
/// the sample nearest the middle of the open shutter. 0 without motion blur.
pub fn sample_subframe(subframes: u32) -> u32 {
    subframes / 2
}

/// What an id is assigned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstanceKey {
    /// The terrain heightfield (sorted before every entity).
    Terrain,
    /// An entity, by `Entity::to_bits`.
    Entity(u64),
}

/// Ids that stay the same for the whole job: an instance (or a mesh asset) keeps the id it got the
/// first frame it was drawn, whatever it does later (moves, swaps LOD, leaves and comes back).
///
/// New keys of one frame are numbered in key order (the terrain, then entities by their bits), so
/// the ids depend on what is drawn, never on the order the world happens to list it.
#[derive(Default)]
pub struct IdTable {
    instances: BTreeMap<InstanceKey, u32>,
    /// Instance ids in assignment order with a label for the manifest.
    instance_rows: Vec<(u32, InstanceKey, String)>,
    /// Mesh asset (by `Arc` address) → class id. The `Arc` is kept so the address is never
    /// reused by another mesh while the job runs.
    classes: HashMap<usize, (u32, Arc<CpuMesh>)>,
    class_rows: Vec<(u32, String)>,
    /// The terrain's class id, once seen.
    terrain_class: Option<u32>,
    /// Material (by `Arc` address) → material id (ADR-3256); the `Arc` keeps the address unique.
    materials: HashMap<usize, (u32, Arc<CpuMaterial>)>,
    material_rows: Vec<(u32, String)>,
    /// The terrain's material id, once seen.
    terrain_material: Option<u32>,
}

impl IdTable {
    /// Assigns ids to this frame's new instances, `keys` in any order with their labels.
    pub fn assign_instances(&mut self, keys: &[(InstanceKey, String)]) {
        let mut fresh: Vec<&(InstanceKey, String)> = keys
            .iter()
            .filter(|(key, _)| !self.instances.contains_key(key))
            .collect();
        fresh.sort_by_key(|(key, _)| *key);
        fresh.dedup_by_key(|(key, _)| *key);
        for (key, label) in fresh {
            let id = (self.instances.len() as u32).saturating_add(1);
            self.instances.insert(*key, id);
            self.instance_rows.push((id, *key, label.clone()));
        }
    }

    /// The id of an instance assigned by [`Self::assign_instances`] (0 if it never was).
    pub fn instance(&self, key: InstanceKey) -> u32 {
        self.instances.get(&key).copied().unwrap_or(0)
    }

    /// The class id of a mesh asset, assigned on first use. Call in a fixed order (the frame's
    /// sorted draw list) so ids repeat across runs.
    pub fn class_of(&mut self, mesh: &Arc<CpuMesh>) -> u32 {
        let key = Arc::as_ptr(mesh) as usize;
        if let Some((id, _)) = self.classes.get(&key) {
            return *id;
        }
        let id = self.next_class();
        self.classes.insert(key, (id, Arc::clone(mesh)));
        self.class_rows.push((
            id,
            format!(
                "mesh {} vertices {} triangles",
                mesh.vertices.len(),
                mesh.indices.len() / 3
            ),
        ));
        id
    }

    /// The terrain's class id, assigned on first use.
    pub fn terrain_class(&mut self) -> u32 {
        if let Some(id) = self.terrain_class {
            return id;
        }
        let id = self.next_class();
        self.terrain_class = Some(id);
        self.class_rows.push((id, "terrain".into()));
        id
    }

    /// The material id of a material asset, assigned on first use (ADR-3256). Call in a fixed
    /// order (the frame's sorted draw list) so ids repeat across runs.
    pub fn material_of(&mut self, material: &Arc<CpuMaterial>) -> u32 {
        let key = Arc::as_ptr(material) as usize;
        if let Some((id, _)) = self.materials.get(&key) {
            return *id;
        }
        let id = self.next_material();
        self.materials.insert(key, (id, Arc::clone(material)));
        self.material_rows.push((
            id,
            format!(
                "base_color [{}, {}, {}] metallic {} roughness {}",
                material.base_color[0],
                material.base_color[1],
                material.base_color[2],
                material.metallic,
                material.roughness
            ),
        ));
        id
    }

    /// The terrain's material id, assigned on first use.
    pub fn terrain_material(&mut self) -> u32 {
        if let Some(id) = self.terrain_material {
            return id;
        }
        let id = self.next_material();
        self.terrain_material = Some(id);
        self.material_rows.push((id, "terrain".into()));
        id
    }

    fn next_material(&self) -> u32 {
        (self.material_rows.len() as u32).saturating_add(1)
    }

    fn next_class(&self) -> u32 {
        (self.class_rows.len() as u32).saturating_add(1)
    }

    /// The tables for the manifest: `{ instances: [{id, kind, entity?, label}], classes: [{id, label}] }`.
    pub fn to_json(&self) -> Value {
        let instances: Vec<Value> = self
            .instance_rows
            .iter()
            .map(|(id, key, label)| match key {
                InstanceKey::Terrain => json!({ "id": id, "kind": "terrain", "label": label }),
                InstanceKey::Entity(bits) => {
                    json!({ "id": id, "kind": "entity", "entity_bits": format!("{bits:016x}"), "label": label })
                }
            })
            .collect();
        let classes: Vec<Value> = self
            .class_rows
            .iter()
            .map(|(id, label)| json!({ "id": id, "label": label }))
            .collect();
        let mut tables = json!({ "instances": instances, "classes": classes });
        if !self.material_rows.is_empty() {
            tables["materials"] = self
                .material_rows
                .iter()
                .map(|(id, label)| json!({ "id": id, "label": label }))
                .collect();
        }
        tables
    }
}

/// One AOV readback at the delivered size: per pixel the ids and `(motion.u, motion.v, depth,
/// coverage)` as the pass wrote them (coverage 0 = background).
pub struct AovPixels {
    /// `[instance, class]` per pixel, row-major, top row first.
    pub ids: Vec<[u32; 2]>,
    /// `[u, v, depth, coverage]` per pixel.
    pub data: Vec<[f32; 4]>,
    /// The material pass's readback, when the request has a material AOV (ADR-3256).
    pub material: Option<MaterialPixels>,
}

/// The readback of the material pass: what the surface under each pixel is made of.
pub struct MaterialPixels {
    /// `[normal.x, normal.y, normal.z, roughness]` per pixel; all 0 on the sky.
    pub normal_roughness: Vec<[f32; 4]>,
    /// `[base_color.r, base_color.g, base_color.b, metalness]` per pixel; all 0 on the sky.
    pub color_metalness: Vec<[f32; 4]>,
    /// Material id per pixel (0 = sky).
    pub material: Vec<u32>,
    /// The view matrix (column-major) the view-space normal rotates by.
    pub view: [[f32; 4]; 4],
}

impl MaterialPixels {
    /// The world normal `n` rotated into view space (`view` is column-major, so the rotation is
    /// the matrix times the column vector).
    fn to_view(&self, n: [f32; 3]) -> [f32; 3] {
        let v = &self.view;
        [
            v[0][0] * n[0] + v[1][0] * n[1] + v[2][0] * n[2],
            v[0][1] * n[0] + v[1][1] * n[1] + v[2][1] * n[2],
            v[0][2] * n[0] + v[1][2] * n[1] + v[2][2] * n[2],
        ]
    }

    fn planes(&self, aov: Aov) -> AovPlanes {
        let pick = |source: &[[f32; 4]], c: usize| -> Vec<f32> { source.iter().map(|p| p[c]).collect() };
        match aov {
            Aov::Normal => AovPlanes::F32((0..3).map(|c| pick(&self.normal_roughness, c)).collect()),
            Aov::NormalView => {
                let views: Vec<[f32; 3]> = self
                    .normal_roughness
                    .iter()
                    .map(|p| self.to_view([p[0], p[1], p[2]]))
                    .collect();
                AovPlanes::F32((0..3).map(|c| views.iter().map(|v| v[c]).collect()).collect())
            }
            Aov::Roughness => AovPlanes::F32(vec![pick(&self.normal_roughness, 3)]),
            Aov::Metalness => AovPlanes::F32(vec![pick(&self.color_metalness, 3)]),
            Aov::BaseColor => AovPlanes::F32((0..3).map(|c| pick(&self.color_metalness, c)).collect()),
            Aov::MaterialId => AovPlanes::U32(vec![self.material.clone()]),
            Aov::Depth | Aov::Id | Aov::Motion => AovPlanes::F32(Vec::new()),
        }
    }
}

/// The channel data of one AOV file, planar (one `Vec` per channel, row-major).
#[derive(Debug, Clone, PartialEq)]
pub enum AovPlanes {
    /// 32-bit float channels.
    F32(Vec<Vec<f32>>),
    /// 32-bit unsigned channels.
    U32(Vec<Vec<u32>>),
}

impl AovPlanes {
    /// The bytes of the raw file: the channels interleaved per pixel, little-endian.
    pub fn interleaved_le(&self) -> Vec<u8> {
        fn weave<T: Copy>(planes: &[Vec<T>], bytes: impl Fn(T) -> [u8; 4]) -> Vec<u8> {
            let pixels = planes.first().map_or(0, Vec::len);
            let mut out = Vec::with_capacity(pixels * planes.len() * 4);
            for i in 0..pixels {
                for plane in planes {
                    out.extend_from_slice(&bytes(plane[i]));
                }
            }
            out
        }
        match self {
            Self::F32(planes) => weave(planes, f32::to_le_bytes),
            Self::U32(planes) => weave(planes, u32::to_le_bytes),
        }
    }
}

impl AovPixels {
    /// The planes of `aov`. Depth is `+inf` on background pixels; motion is zeroed when
    /// `reset` (frame 0, a cut frame).
    pub fn planes(&self, aov: Aov, reset: bool) -> AovPlanes {
        if aov.is_material() {
            return self.material.as_ref().map_or_else(
                || {
                    let zeros = self.data.len();
                    match aov {
                        Aov::MaterialId => AovPlanes::U32(vec![vec![0; zeros]]),
                        other => AovPlanes::F32(vec![vec![0.0; zeros]; other.channels().len()]),
                    }
                },
                |m| m.planes(aov),
            );
        }
        match aov {
            Aov::Depth => AovPlanes::F32(vec![
                self.data
                    .iter()
                    .map(|d| if d[3] > 0.5 { d[2] } else { f32::INFINITY })
                    .collect(),
            ]),
            Aov::Id => AovPlanes::U32(vec![
                self.ids.iter().map(|i| i[0]).collect(),
                self.ids.iter().map(|i| i[1]).collect(),
            ]),
            Aov::Motion => {
                let pick = |c: usize| -> Vec<f32> {
                    self.data
                        .iter()
                        .map(|d| if reset || !d[c].is_finite() { 0.0 } else { d[c] })
                        .collect()
                };
                AovPlanes::F32(vec![pick(0), pick(1)])
            }
            _ => AovPlanes::F32(Vec::new()),
        }
    }
}

/// The encodings block of the manifest.
pub fn encodings_json(request: &AovRequest, width: u32, height: u32) -> Value {
    let mut out = serde_json::Map::new();
    for aov in &request.list {
        let entry = match aov {
            Aov::Depth => json!({
                "channels": aov.channels(),
                "type": "f32",
                "unit": "meter",
                "meaning": "linear view depth: distance from the camera plane along the view direction",
                "background": "+inf",
            }),
            Aov::Id => json!({
                "channels": aov.channels(),
                "type": "u32",
                "meaning": "id.instance: one id per entity drawn (the terrain is one), stable for the whole job; id.class: one id per mesh asset",
                "background": 0,
                "tables": "aov.ids",
            }),
            Aov::Motion => json!({
                "channels": aov.channels(),
                "type": "f32",
                "unit": "delivered pixel",
                "axes": "+u right, +v down",
                "meaning": "backward motion: position of this pixel's surface in the previous delivered frame minus its position now",
                "zero_on": "frame 0 and every cut frame (aov.motion_reset_frames); exactly 0 wherever the surface and the camera are bit-identical to the last delivered frame",
                "interval": "one delivered frame: the render clock's step for render-clock motion (camera paths, shots, VFX), the frame's simulation step (frame_times[i].sim_step_ns) for simulated motion under a time_map",
            }),
            Aov::Normal | Aov::NormalView => json!({
                "channels": aov.channels(),
                "type": "f32",
                "meaning": if *aov == Aov::Normal {
                    "unit surface normal, world space (+y up as in the scene); the vertex normal carried through the entity's normal matrix, interpolated and renormalized; the terrain's normal comes from its heightfield"
                } else {
                    "unit surface normal, view space: +x right, +y up, the camera looks down -z; the world normal rotated by the view matrix"
                },
                "background": 0,
                "png": "(n * 0.5 + 0.5) per channel; background stays 0",
                "limits": "no normal map, no skinned deformation (the rest-pose normal is used), no morph targets",
            }),
            Aov::Roughness | Aov::Metalness => json!({
                "channels": aov.channels(),
                "type": "f32",
                "meaning": if *aov == Aov::Roughness {
                    "perceptual roughness: the material's factor, times the packed ORM map's green channel when the material has one"
                } else {
                    "metalness: the material's factor, times the packed ORM map's blue channel when the material has one"
                },
                "background": 0,
                "png": "the value, 0..1 (8 or 16 bit)",
                "tables": "aov.materials",
            }),
            Aov::BaseColor => json!({
                "channels": aov.channels(),
                "type": "f32",
                "meaning": "linear base color before lighting: the material's factor times its base-color texture (decoded from sRGB); vertex colour is not applied; the terrain is a neutral 0.5 grey",
                "background": 0,
                "png": "sRGB-encoded (the display transfer) so 8 bits hold the dark values",
                "tables": "aov.materials",
            }),
            Aov::MaterialId => json!({
                "channels": aov.channels(),
                "type": "u32",
                "meaning": "one id per material asset drawn (the terrain has its own), assigned the first time it is seen in the frame's sorted draw order and kept for the whole job",
                "background": 0,
                "tables": "aov.materials",
            }),
        };
        out.insert(aov.name().to_string(), entry);
    }
    let raw = (request.format == AovFormat::Raw).then(|| {
        json!({
            "byte_order": "little",
            "layout": "channels interleaved per pixel, rows top to bottom, no header",
            "width": width,
            "height": height,
            "frame_bytes": request.list.iter().map(|aov| {
                (aov.name(), aov.raw_bytes_per_pixel() * u64::from(width) * u64::from(height))
            }).collect::<BTreeMap<_, _>>(),
        })
    });
    json!({
        "format": request.format.name(),
        "size": [width, height],
        "sampling": "one sample at each delivered pixel center, no jitter, no supersampling; the middle subframe of a motion-blurred frame",
        "times": "AOV frame i is delivered frame i: its render and simulation times are frame_times[i]",
        "file_pattern": format!("<name>_<aov>_NNNNNN.{}", request.format.extension()),
        "channels": Value::Object(out),
        "raw": raw,
    })
}

/// Most samples per side of the terrain grid the AOV pass draws. A larger heightfield is drawn
/// every `stride`-th sample (`terrain_stride`).
pub const MAX_TERRAIN_SIDE: u32 = 2048;

/// The sample stride that keeps a `width`×`depth` heightfield within [`MAX_TERRAIN_SIDE`] per side.
pub fn terrain_stride(width: u32, depth: u32) -> u32 {
    width.max(depth).div_ceil(MAX_TERRAIN_SIDE).max(1)
}

/// A triangle grid of a heightfield (world positions and indices), every `stride`-th sample plus
/// the last row and column so the grid covers the whole field. `None` for an empty field.
pub fn terrain_grid(
    width: u32,
    depth: u32,
    origin: [f32; 2],
    spacing: [f32; 2],
    heights: &[f32],
    stride: u32,
) -> Option<(Vec<[f32; 3]>, Vec<u32>)> {
    if width < 2 || depth < 2 || heights.len() < (width as usize) * (depth as usize) {
        return None;
    }
    let axis = |n: u32| -> Vec<u32> {
        let mut samples: Vec<u32> = (0..n).step_by(stride.max(1) as usize).collect();
        if samples.last() != Some(&(n - 1)) {
            samples.push(n - 1);
        }
        samples
    };
    let (xs, zs) = (axis(width), axis(depth));
    let mut positions = Vec::with_capacity(xs.len() * zs.len());
    for &j in &zs {
        for &i in &xs {
            let h = heights[(j * width + i) as usize];
            positions.push([
                origin[0] + i as f32 * spacing[0],
                if h.is_finite() { h } else { 0.0 },
                origin[1] + j as f32 * spacing[1],
            ]);
        }
    }
    let row = xs.len() as u32;
    let mut indices = Vec::with_capacity((xs.len() - 1) * (zs.len() - 1) * 6);
    for j in 0..zs.len() as u32 - 1 {
        for i in 0..row - 1 {
            let a = j * row + i;
            let (b, c, d) = (a + 1, a + row, a + row + 1);
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    Some((positions, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aov_lists_parse_sort_and_reject_unknown_names() {
        let r = AovRequest::parse(&json!({})).expect("none");
        assert!(!r.is_on());
        assert_eq!(r.format, AovFormat::Exr);
        let r = AovRequest::parse(&json!({ "aovs": ["motion", "depth", "motion", "id"] })).expect("all");
        assert_eq!(r.list, [Aov::Depth, Aov::Id, Aov::Motion]);
        assert!(r.list.contains(&Aov::Id));
        let r = AovRequest::parse(&json!({ "aovs": ["depth"], "aov_format": "raw" })).expect("raw");
        assert_eq!(r.format, AovFormat::Raw);
        for bad in [
            json!({ "aovs": ["normals"] }),
            json!({ "aovs": "depth" }),
            json!({ "aovs": [1] }),
            json!({ "aovs": ["depth"], "aov_format": "tiff" }),
            json!({ "aov_format": "raw" }),
        ] {
            assert!(AovRequest::parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(r.to_json(), json!({ "aovs": ["depth"], "aov_format": "raw" }));
    }

    #[test]
    fn aov_files_are_named_by_aov_and_frame() {
        let r = AovRequest::parse(&json!({ "aovs": ["id", "depth"] })).expect("aovs");
        let dir = Path::new("/o");
        assert_eq!(
            r.path(dir, "shot", Aov::Depth, 7),
            dir.join("shot_depth_000007.exr")
        );
        assert_eq!(
            r.paths(dir, "shot", 12),
            [dir.join("shot_depth_000012.exr"), dir.join("shot_id_000012.exr")]
        );
    }

    #[test]
    fn instance_ids_are_stable_and_independent_of_listing_order() {
        let label = |k: InstanceKey| (k, format!("{k:?}"));
        let mut a = IdTable::default();
        a.assign_instances(&[
            label(InstanceKey::Entity(30)),
            label(InstanceKey::Entity(10)),
            label(InstanceKey::Terrain),
        ]);
        let mut b = IdTable::default();
        b.assign_instances(&[
            label(InstanceKey::Terrain),
            label(InstanceKey::Entity(10)),
            label(InstanceKey::Entity(30)),
            label(InstanceKey::Entity(10)),
        ]);
        for key in [
            InstanceKey::Terrain,
            InstanceKey::Entity(10),
            InstanceKey::Entity(30),
        ] {
            assert_eq!(a.instance(key), b.instance(key), "{key:?}");
        }
        assert_eq!(a.instance(InstanceKey::Terrain), 1);
        assert_eq!(a.instance(InstanceKey::Entity(10)), 2);
        assert_eq!(a.instance(InstanceKey::Entity(30)), 3);
        // A later frame: an entity disappears, a new one with smaller bits appears. Old ids stay.
        a.assign_instances(&[label(InstanceKey::Entity(5)), label(InstanceKey::Entity(30))]);
        assert_eq!(a.instance(InstanceKey::Entity(30)), 3);
        assert_eq!(a.instance(InstanceKey::Entity(10)), 2, "kept while absent");
        assert_eq!(a.instance(InstanceKey::Entity(5)), 4, "new ids continue");
        assert_eq!(a.instance(InstanceKey::Entity(99)), 0, "never drawn");
        let rows = a.to_json();
        assert_eq!(rows["instances"].as_array().map(Vec::len), Some(4));
        assert_eq!(rows["instances"][0]["kind"], "terrain");
        assert_eq!(rows["instances"][3]["entity_bits"], "0000000000000005");
    }

    #[test]
    fn class_ids_follow_the_mesh_asset() {
        let mesh = |n: usize| {
            Arc::new(CpuMesh {
                vertices: vec![
                    regia_render::LabVertex::from_pos_normal_uv(
                        [0.0; 3],
                        [0.0, 1.0, 0.0],
                        [0.0; 2]
                    );
                    n
                ],
                indices: vec![0; 3],
            })
        };
        let (rock, tree) = (mesh(3), mesh(4));
        let mut table = IdTable::default();
        let r1 = table.class_of(&rock);
        let t1 = table.class_of(&tree);
        assert_eq!((r1, t1), (1, 2));
        assert_eq!(
            table.class_of(&Arc::clone(&rock)),
            1,
            "a clone of the Arc is the same asset"
        );
        assert_eq!(table.terrain_class(), 3);
        assert_eq!(table.terrain_class(), 3);
        assert_eq!(table.to_json()["classes"][2]["label"], "terrain");
    }

    fn pixels() -> AovPixels {
        AovPixels {
            ids: vec![[3, 1], [0, 0]],
            data: vec![[1.5, -2.0, 12.25, 1.0], [0.25, 0.5, 0.0, 0.0]],
            material: None,
        }
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn material_names_parse_sort_after_the_old_aovs_and_refuse_with_their_own_codes() {
        let r = AovRequest::parse(&json!({
            "aovs": ["material_id", "base_color", "motion", "normal_view", "normal", "roughness", "metalness", "depth"]
        }))
        .expect("all");
        let names: Vec<&str> = r.list.iter().map(|a| a.name()).collect();
        assert_eq!(
            names,
            [
                "depth",
                "motion",
                "normal",
                "normal_view",
                "roughness",
                "metalness",
                "base_color",
                "material_id"
            ]
        );
        assert!(r.wants_material());
        assert!(
            !AovRequest::parse(&json!({ "aovs": ["id"] }))
                .expect("id")
                .wants_material()
        );
        for aov in Aov::ALL {
            assert_eq!(Aov::parse(aov.name()), Some(aov));
        }
        let unknown = AovRequest::parse(&json!({ "aovs": ["depth", "albedo"] })).expect_err("unknown");
        assert_eq!(code(&unknown), Some("unknown_aov"));
        for aovs in [
            json!(["depth"]),
            json!(["normal", "id"]),
            json!(["material_id"]),
            json!(["motion"]),
        ] {
            for format in ["png", "png16"] {
                let err =
                    AovRequest::parse(&json!({ "aovs": aovs, "aov_format": format })).expect_err("exact");
                assert_eq!(code(&err), Some("aov_format_unsupported"), "{aovs} {format}");
            }
        }
        let png = AovRequest::parse(&json!({ "aovs": ["normal", "base_color"], "aov_format": "png16" }))
            .expect("png");
        assert_eq!(png.format, AovFormat::Png16);
        assert_eq!(
            png.path(Path::new("/o"), "s", Aov::BaseColor, 7),
            Path::new("/o/s_base_color_000007.png")
        );
        assert_eq!(
            AovRequest::parse(&json!({ "aovs": ["roughness"] }))
                .expect("exr")
                .path(Path::new("/o"), "s", Aov::Roughness, 0),
            Path::new("/o/s_roughness_000000.exr")
        );
    }

    #[test]
    fn material_readbacks_become_planes_and_the_view_normal_rotates() {
        // View: a quarter turn about +y (the camera looks down +x): world +x is view -z.
        let view = [
            [0.0, 0.0, -1.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let mut p = pixels();
        p.material = Some(MaterialPixels {
            normal_roughness: vec![[1.0, 0.0, 0.0, 0.25], [0.0; 4]],
            color_metalness: vec![[0.5, 0.25, 0.125, 1.0], [0.0; 4]],
            material: vec![2, 0],
            view,
        });
        assert_eq!(
            p.planes(Aov::Normal, false),
            AovPlanes::F32(vec![vec![1.0, 0.0], vec![0.0, 0.0], vec![0.0, 0.0]])
        );
        assert_eq!(
            p.planes(Aov::NormalView, false),
            AovPlanes::F32(vec![vec![0.0, 0.0], vec![0.0, 0.0], vec![-1.0, 0.0]])
        );
        assert_eq!(
            p.planes(Aov::Roughness, false),
            AovPlanes::F32(vec![vec![0.25, 0.0]])
        );
        assert_eq!(
            p.planes(Aov::Metalness, false),
            AovPlanes::F32(vec![vec![1.0, 0.0]])
        );
        assert_eq!(
            p.planes(Aov::BaseColor, true),
            AovPlanes::F32(vec![vec![0.5, 0.0], vec![0.25, 0.0], vec![0.125, 0.0]]),
            "a cut frame resets motion only"
        );
        assert_eq!(p.planes(Aov::MaterialId, false), AovPlanes::U32(vec![vec![2, 0]]));
        // Without a material readback the planes are zeros of the right shape.
        let none = pixels();
        assert_eq!(
            none.planes(Aov::Normal, false),
            AovPlanes::F32(vec![vec![0.0; 2]; 3])
        );
        assert_eq!(
            none.planes(Aov::MaterialId, false),
            AovPlanes::U32(vec![vec![0; 2]])
        );
    }

    #[test]
    fn material_ids_follow_the_material_asset_in_first_use_order() {
        let a = Arc::new(CpuMaterial::default());
        let b = Arc::new(CpuMaterial {
            roughness: 0.2,
            ..CpuMaterial::default()
        });
        let mut ids = IdTable::default();
        assert_eq!(ids.material_of(&a), 1);
        assert_eq!(ids.material_of(&b), 2);
        assert_eq!(ids.material_of(&a), 1, "the same asset keeps its id");
        assert_eq!(ids.terrain_material(), 3);
        assert_eq!(ids.terrain_material(), 3);
        let json = ids.to_json();
        assert_eq!(json["materials"].as_array().map(Vec::len), Some(3));
        assert_eq!(json["materials"][2]["label"], "terrain");
        assert!(
            IdTable::default().to_json().get("materials").is_none(),
            "no material pass, no table"
        );
    }

    #[test]
    fn readbacks_become_the_documented_planes() {
        let p = pixels();
        assert_eq!(
            p.planes(Aov::Depth, false),
            AovPlanes::F32(vec![vec![12.25, f32::INFINITY]])
        );
        assert_eq!(
            p.planes(Aov::Id, false),
            AovPlanes::U32(vec![vec![3, 0], vec![1, 0]])
        );
        assert_eq!(
            p.planes(Aov::Motion, false),
            AovPlanes::F32(vec![vec![1.5, 0.25], vec![-2.0, 0.5]]),
            "the sky moves with the camera too"
        );
        assert_eq!(
            p.planes(Aov::Motion, true),
            AovPlanes::F32(vec![vec![0.0, 0.0], vec![0.0, 0.0]]),
            "a cut frame has no motion"
        );
    }

    #[test]
    fn raw_aov_bytes_interleave_little_endian_channels() {
        let bytes = pixels().planes(Aov::Id, false).interleaved_le();
        assert_eq!(bytes.len(), 2 * 2 * 4);
        assert_eq!(&bytes[..8], &[3, 0, 0, 0, 1, 0, 0, 0]);
        let depth = pixels().planes(Aov::Depth, false).interleaved_le();
        assert_eq!(&depth[..4], &12.25f32.to_le_bytes());
        assert_eq!(&depth[4..], &f32::INFINITY.to_le_bytes());
    }

    #[test]
    fn the_manifest_states_every_encoding() {
        let r = AovRequest::parse(&json!({ "aovs": ["depth", "id", "motion"], "aov_format": "raw" }))
            .expect("aovs");
        let e = encodings_json(&r, 640, 360);
        assert_eq!(e["channels"]["depth"]["unit"], "meter");
        assert_eq!(e["channels"]["depth"]["background"], "+inf");
        assert_eq!(e["channels"]["id"]["type"], "u32");
        assert_eq!(e["channels"]["motion"]["axes"], "+u right, +v down");
        assert_eq!(e["raw"]["frame_bytes"]["id"], 640 * 360 * 8);
        assert_eq!(e["raw"]["frame_bytes"]["depth"], 640 * 360 * 4);
        let exr = AovRequest::parse(&json!({ "aovs": ["depth"] })).expect("exr");
        assert_eq!(encodings_json(&exr, 4, 4)["raw"], Value::Null);
    }

    #[test]
    fn the_aov_sample_is_the_middle_subframe() {
        assert_eq!(sample_subframe(1), 0);
        assert_eq!(sample_subframe(2), 1);
        assert_eq!(sample_subframe(8), 4);
        assert_eq!(sample_subframe(9), 4);
    }

    #[test]
    fn terrain_grids_cover_the_whole_field_at_any_stride() {
        let heights: Vec<f32> = (0..5 * 3).map(|i| i as f32).collect();
        let (positions, indices) = terrain_grid(5, 3, [10.0, -4.0], [2.0, 0.5], &heights, 1).expect("grid");
        assert_eq!(positions.len(), 15);
        assert_eq!(indices.len(), 4 * 2 * 6);
        assert_eq!(positions[0], [10.0, 0.0, -4.0]);
        assert_eq!(positions[14], [18.0, 14.0, -3.0], "last sample, world position");
        // Stride 3 keeps samples 0, 3 and the last column (4); rows 0 and the last (2).
        let (coarse, coarse_indices) = terrain_grid(5, 3, [0.0, 0.0], [1.0, 1.0], &heights, 3).expect("grid");
        assert_eq!(coarse.len(), 3 * 2);
        assert_eq!(coarse[2], [4.0, 4.0, 0.0]);
        assert_eq!(coarse[5], [4.0, 14.0, 2.0]);
        assert_eq!(coarse_indices.len(), 2 * 6);
        assert!(coarse_indices.iter().all(|i| (*i as usize) < coarse.len()));
        assert!(terrain_grid(1, 3, [0.0; 2], [1.0; 2], &heights, 1).is_none());
        assert!(terrain_grid(5, 3, [0.0; 2], [1.0; 2], &heights[..4], 1).is_none());
        assert_eq!(terrain_stride(2048, 100), 1);
        assert_eq!(terrain_stride(2049, 100), 2);
        assert_eq!(terrain_stride(100, 8192), 4);
    }
}
